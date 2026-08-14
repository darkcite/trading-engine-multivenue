//! Integration test: real TCP loopback against the embedded
//! metrics server. Binds an ephemeral port, registers a counter +
//! gauge, fires a GET /metrics, and asserts the Prometheus body
//! contains the registered metrics.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_metrics::{serve_metrics, MetricsRegistry};

fn boot_server(registry: Arc<MetricsRegistry>) -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // free the port; serve_metrics will rebind.

    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        let _ = serve_metrics(addr, registry, &stop_clone);
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
    let mut sock =
        TcpStream::connect(("127.0.0.1", port)).expect("connect");
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
    let ticks = reg.register_counter("polymarket_ticks_total").unwrap();
    let mid = reg.register_gauge("polymarket_book_mid").unwrap();
    let reg = Arc::new(reg);
    reg.counter(ticks).inc(42);
    reg.gauge(mid).set(500_000);

    let (port, stop, handle) = boot_server(reg.clone());
    let (status, body) = http_get(port, "/metrics");
    stop.store(true, Ordering::Release);
    handle.join().unwrap();

    assert_eq!(status, 200);
    assert!(
        body.contains("polymarket_ticks_total 42"),
        "missing ticks counter; body={body}"
    );
    assert!(
        body.contains("polymarket_book_mid 500000"),
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
