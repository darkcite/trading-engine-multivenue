// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: real TCP loopback against the embedded
//! metrics server. Binds an ephemeral port, registers a counter +
//! gauge, fires a GET /metrics, and asserts the Prometheus body
//! contains the registered metrics; RG6 adds the `/state` route
//! (writer-supplied body, 404 without a writer, 500 on overflow).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_metrics::{serve_metrics, EncodeErr, MetricsRegistry, StateFn};

fn boot_server(registry: Arc<MetricsRegistry>) -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
    boot_server_with(registry, None::<StateFn>)
}

fn boot_server_with<F>(
    registry: Arc<MetricsRegistry>,
    state: Option<F>,
) -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>)
where
    F: FnMut(&mut [u8]) -> Result<usize, EncodeErr> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // free the port; serve_metrics will rebind.

    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        let _ = serve_metrics(addr, registry, state, &stop_clone, |_ev| {});
    });

    // Wait until the server is ready to accept (poll the port).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            break;
        }
        if Instant::now() > deadline {
            panic!("metrics server did not come up");
        }
        thread::sleep(Duration::from_millis(20));
    }
    (addr.port(), stop, handle)
}

fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).expect("write");
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 1024];
    while let Ok(n) = sock.read(&mut tmp) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    let status = s
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, s)
}

#[test]
fn metrics_endpoint_returns_registered_counters() {
    let mut reg = MetricsRegistry::new();
    let ticks = reg.register_counter("engine_ticks_total").unwrap();
    let mid = reg.register_gauge("engine_book_mid").unwrap();
    let reg = Arc::new(reg);
    reg.counter(ticks).inc(42);
    reg.gauge(mid).set(500_000);

    let (port, stop, handle) = boot_server(reg.clone());
    let (status, body) = http_get(port, "/metrics");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();

    assert_eq!(status, 200);
    assert!(
        body.contains("engine_ticks_total 42"),
        "missing ticks counter; body={body}"
    );
    assert!(
        body.contains("engine_book_mid 500000"),
        "missing mid gauge; body={body}"
    );
}

#[test]
fn healthz_returns_ok() {
    let reg = Arc::new(MetricsRegistry::new());
    let (port, stop, handle) = boot_server(reg);
    let (status, body) = http_get(port, "/healthz");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("ok"), "body={body}");
}

#[test]
fn unknown_path_returns_404() {
    let reg = Arc::new(MetricsRegistry::new());
    let (port, stop, handle) = boot_server(reg);
    let (status, _body) = http_get(port, "/nope");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(status, 404);
}

/// G1 remediation item 2 regression (macOS EAGAIN, "os error 35"):
/// sockets accepted from a nonblocking listener inherit nonblocking
/// mode on BSD/macOS, so a scrape whose request bytes arrive after
/// `accept()` returned made `read()` fail EAGAIN immediately — the
/// two single-scrape failures of the first 6 h soak. With the socket
/// restored to blocking, a slow-arriving request must be served.
#[test]
fn slow_request_after_connect_is_served_not_eagain() {
    let mut reg = MetricsRegistry::new();
    let ticks = reg.register_counter("engine_slow_total").unwrap();
    let reg = Arc::new(reg);
    reg.counter(ticks).inc(7);

    let (port, stop, handle) = boot_server(reg);
    // Connect first, let the server accept an idle socket, THEN send.
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    thread::sleep(Duration::from_millis(300));
    sock.write_all(b"GET /metrics HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write");
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 1024];
    while let Ok(n) = sock.read(&mut tmp) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    stop.store(true, Ordering::Release);
    handle.join().unwrap();

    let s = String::from_utf8_lossy(&buf);
    assert!(
        s.starts_with("HTTP/1.1 200"),
        "slow request must be served; got: {s}"
    );
    assert!(s.contains("engine_slow_total 7"), "{s}");
}

/// Scrape hammer (acceptance for item 2): rapid sequential scrapes
/// must all succeed and the error sink must stay silent.
#[test]
fn scrape_hammer_all_succeed_without_conn_errors() {
    use std::sync::atomic::AtomicU32;

    let mut reg = MetricsRegistry::new();
    let ticks = reg.register_counter("engine_hammer_total").unwrap();
    let reg = Arc::new(reg);
    reg.counter(ticks).inc(1);

    // Local boot with a counting sink (boot_server discards events).
    let errors = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    let (stop_clone, errs_clone, reg_clone) = (stop.clone(), errors.clone(), reg.clone());
    let handle = thread::spawn(move || {
        let _ = serve_metrics(addr, reg_clone, None::<StateFn>, &stop_clone, |_ev| {
            errs_clone.fetch_add(1, Ordering::Relaxed);
        });
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            break;
        }
        assert!(Instant::now() <= deadline, "metrics server did not come up");
        thread::sleep(Duration::from_millis(20));
    }

    for i in 0..50 {
        let (status, body) = http_get(addr.port(), "/metrics");
        assert_eq!(status, 200, "scrape {i} failed; body={body}");
        assert!(body.contains("engine_hammer_total 1"), "scrape {i}: {body}");
    }
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(
        errors.load(Ordering::Relaxed),
        0,
        "no connection errors under hammer"
    );
}

/// RG6: `/state` serves whatever the wired writer encodes, as JSON.
#[test]
fn state_endpoint_serves_the_writer_body_as_json() {
    let reg = Arc::new(MetricsRegistry::new());
    let mut calls = 0u32;
    let writer = move |dst: &mut [u8]| -> Result<usize, EncodeErr> {
        calls += 1;
        let body = b"{\"v\":1,\"calls\":";
        let mut n = 0usize;
        dst[..body.len()].copy_from_slice(body);
        n += body.len();
        dst[n] = b'0' + (calls as u8);
        n += 1;
        dst[n] = b'}';
        n += 1;
        Ok(n)
    };
    let (port, stop, handle) = boot_server_with(reg, Some(writer));
    let (status, first) = http_get(port, "/state");
    let (status2, second) = http_get(port, "/state");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(status, 200);
    assert_eq!(status2, 200);
    assert!(first.contains("Content-Type: application/json"), "{first}");
    assert!(first.ends_with("{\"v\":1,\"calls\":1}"), "{first}");
    assert!(second.ends_with("{\"v\":1,\"calls\":2}"), "writer is FnMut: {second}");
}

/// RG6: without a writer the path is simply absent.
#[test]
fn state_endpoint_is_404_without_a_writer() {
    let reg = Arc::new(MetricsRegistry::new());
    let (port, stop, handle) = boot_server(reg);
    let (status, _body) = http_get(port, "/state");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(status, 404);
}

/// RG6: an overflowing writer is a 500 — the body is never truncated
/// — and the server keeps serving afterwards.
#[test]
fn state_endpoint_overflow_is_500_and_server_survives() {
    let reg = Arc::new(MetricsRegistry::new());
    let writer = |_dst: &mut [u8]| -> Result<usize, EncodeErr> { Err(EncodeErr::BufferTooSmall) };
    let (port, stop, handle) = boot_server_with(reg, Some(writer));
    let (status, _body) = http_get(port, "/state");
    let (health, _ok) = http_get(port, "/healthz");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(status, 500);
    assert_eq!(health, 200);
}
