// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: `LiveDispatcher` against a real
//! `127.0.0.1` rustls server with a self-signed cert.
//!
//! Scripts a Polymarket-shaped POST → success-envelope reply,
//! drives `LiveDispatcher::submit_inline` against the real socket,
//! and asserts the dispatcher counters increment correctly. HD3 adds a
//! keep-alive server: an order after an idle close goes out on a fresh
//! connection, and a chunked answer is decoded.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use clob_dispatcher::{DispatchError, DispatchStats, LiveDispatcher, OrderDispatch};
use core_types::{Order, Price, Qty, Side, VenueId};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

struct LoopbackCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> LoopbackCert {
    let cert =
        generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-signed cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("private key DER");
    LoopbackCert { cert_der, key_der }
}

fn build_server_config(cert: &LoopbackCert) -> Arc<ServerConfig> {
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
        .expect("server config");
    Arc::new(cfg)
}

fn build_client_config(cert: &LoopbackCert) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("add trust anchor");
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

fn boot_server(
    reply_body: &'static [u8],
    status_line: &'static [u8],
) -> (u16, Arc<ClientConfig>, Arc<AtomicBool>) {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        // Read the client's POST until we see \r\n\r\n.
        let mut buf = [0u8; 16 * 1024];
        let mut total = 0;
        let header_end = loop {
            let n = stream.read(&mut buf[total..]).expect("server read");
            if n == 0 {
                panic!("client closed before sending complete headers");
            }
            total += n;
            if let Some(i) = (0..total.saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n") {
                break i + 4;
            }
            if total == buf.len() {
                panic!("oversized request");
            }
        };

        // Parse Content-Length so we can drain the body (we don't
        // actually validate it; this test just confirms the
        // dispatcher round-trips a real request).
        let header_str = std::str::from_utf8(&buf[..header_end]).expect("ascii headers");
        let content_length: usize = header_str
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // Drain the rest of the body.
        let body_already = total - header_end;
        let mut body_remaining = content_length.saturating_sub(body_already);
        while body_remaining > 0 {
            let need = body_remaining.min(buf.len());
            let n = stream.read(&mut buf[..need]).expect("body read");
            if n == 0 {
                break;
            }
            body_remaining = body_remaining.saturating_sub(n);
        }

        // Write the canned response: status line + Content-Length
        // framing + body.
        let body_len_str = format!("{}", reply_body.len());
        let resp_head = [
            status_line,
            b"\r\nContent-Type: application/json\r\nContent-Length: ",
            body_len_str.as_bytes(),
            b"\r\nConnection: close\r\n\r\n",
        ]
        .concat();
        stream.write_all(&resp_head).expect("write head");
        stream.write_all(reply_body).expect("write body");
        stream.flush().expect("flush stream");
        // Wait on signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
    });

    (port, client_cfg, client_done)
}

fn canned_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    k[31] = 1;
    k
}

fn canned_order() -> Order {
    Order::new(
        0,
        VenueId::Polymarket,
        42,
        Side::Bid,
        0,
        Price::from_raw(500_000),
        Qty::from_raw(1_000_000),
        0,
    )
}

fn submit_and_collect(
    reply_body: &'static [u8],
    status_line: &'static [u8],
) -> Result<DispatchStats, DispatchError> {
    let (port, client_cfg, client_done) = boot_server(reply_body, status_line);
    let mut disp = LiveDispatcher::connect("localhost", "/order", port, canned_key(), client_cfg)
        .expect("LiveDispatcher::connect");
    let result = disp.submit_inline(&canned_order());
    // Signal the server we have what we need so it can shut down
    // immediately instead of waiting on the deadline.
    client_done.store(true, Ordering::Release);
    result?;
    Ok(disp.stats())
}

#[test]
fn live_dispatcher_accepts_success_envelope() {
    let stats = submit_and_collect(
        br#"{"orderID":"0xabc123","success":true}"#,
        b"HTTP/1.1 200 OK",
    )
    .expect("dispatch should succeed");
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.rejected, 0);
}

#[test]
fn live_dispatcher_surfaces_error_envelope() {
    let res = submit_and_collect(br#"{"error":"insufficient balance"}"#, b"HTTP/1.1 200 OK");
    // The dispatcher treats the error envelope as Http(200) so
    // the caller sees a structured failure even though the
    // transport succeeded.
    match res {
        Err(DispatchError::Http(200)) => {}
        other => panic!("expected Http(200), got {other:?}"),
    }
}

#[test]
fn live_dispatcher_returns_http_status_for_non_2xx() {
    let res = submit_and_collect(br#"{"error":"unauthorized"}"#, b"HTTP/1.1 401 Unauthorized");
    match res {
        Err(DispatchError::Http(401)) => {}
        other => panic!("expected Http(401), got {other:?}"),
    }
}

/// How the keep-alive server of the HD3 tests answers.
#[derive(Clone, Copy, Default)]
struct Script {
    /// Answer `Transfer-Encoding: chunked`, the body cut inside its
    /// `"orderID"` field.
    chunked: bool,
    /// After each answer, hold the connection this long, then close it
    /// (`close_notify` and a FIN) while the client is idle (0 = never).
    idle_close_ms: u64,
}

const ENVELOPE: &[u8] = br#"{"orderID":"0xabc123","success":true}"#;

/// A keep-alive server on `127.0.0.1` that answers every request with
/// the success envelope: its port, a client config that trusts it, the
/// connections it accepted and the idle closes it made.
fn serve(script: Script) -> (u16, Arc<ClientConfig>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let conns = Arc::new(AtomicUsize::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let (counted, closed) = (conns.clone(), closes.clone());
    thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            counted.fetch_add(1, Ordering::SeqCst);
            let (cfg, closed) = (server_cfg.clone(), closed.clone());
            thread::spawn(move || answer_each(sock, cfg, script, &closed));
        }
    });
    (port, client_cfg, conns, closes)
}

/// Serve one connection: every request gets the envelope, kept alive —
/// or the connection is closed `idle_close_ms` after each answer, and
/// the close counted in `closed`.
fn answer_each(mut sock: TcpStream, cfg: Arc<ServerConfig>, script: Script, closed: &AtomicUsize) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut conn = ServerConnection::new(cfg).expect("server conn");
    let mut tls = Stream::new(&mut conn, &mut sock);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        // One whole request: the head, then its Content-Length body.
        let whole = loop {
            if let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= he + 4 + len {
                    break he + 4 + len;
                }
            }
            match tls.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        buf.drain(..whole);
        let mut wire = Vec::new();
        if script.chunked {
            wire.extend_from_slice(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                  Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n",
            );
            // `{"ord` | `erID":"0xabc` | `123","success":true}`: read as
            // raw bytes, a chunk line sits inside the field the scanner
            // looks for.
            let (a, rest) = ENVELOPE.split_at(5);
            let (b, c) = rest.split_at(12);
            for part in [a, b, c] {
                wire.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
                wire.extend_from_slice(part);
                wire.extend_from_slice(b"\r\n");
            }
            wire.extend_from_slice(b"0\r\n\r\n");
        } else {
            wire.extend_from_slice(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    ENVELOPE.len()
                )
                .as_bytes(),
            );
            wire.extend_from_slice(ENVELOPE);
        }
        if tls.write_all(&wire).is_err() || tls.flush().is_err() {
            return;
        }
        if script.idle_close_ms != 0 {
            thread::sleep(Duration::from_millis(script.idle_close_ms));
            tls.conn.send_close_notify();
            let _ = tls.conn.write_tls(tls.sock);
            let _ = tls.sock.shutdown(Shutdown::Both);
            closed.fetch_add(1, Ordering::SeqCst);
            return;
        }
    }
}

#[test]
fn an_order_after_an_idle_close_goes_out_on_a_fresh_connection() {
    // HD3: the server closes the kept-alive connection while the
    // dispatcher is idle. The next order must dial fresh; before HD3 it
    // was written into the dead connection (no reuse probe) and failed.
    let (port, client_cfg, conns, closes) = serve(Script {
        idle_close_ms: 50,
        ..Script::default()
    });
    let mut disp = LiveDispatcher::connect("localhost", "/order", port, canned_key(), client_cfg)
        .expect("LiveDispatcher::connect");
    disp.submit_inline(&canned_order())
        .expect("the first order");
    // Wait for the server's idle close (bounded), then give its
    // `close_notify` and FIN a moment to land.
    let deadline = Instant::now() + Duration::from_secs(5);
    while closes.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the server never closed");
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(50));
    disp.submit_inline(&canned_order())
        .expect("the second order, on a fresh connection");
    assert_eq!(disp.stats().accepted, 2);
    assert_eq!(conns.load(Ordering::SeqCst), 2, "one connection per order");
}

#[test]
fn a_chunked_answer_is_decoded_and_the_connection_kept() {
    // HD3: a `Transfer-Encoding: chunked` success envelope. Before HD3
    // the raw chunk lines reached the scanner (`JsonMalformed`); now the
    // body is decoded in place and the connection serves the next order.
    let (port, client_cfg, conns, _) = serve(Script {
        chunked: true,
        ..Script::default()
    });
    let mut disp = LiveDispatcher::connect("localhost", "/order", port, canned_key(), client_cfg)
        .expect("LiveDispatcher::connect");
    disp.submit_inline(&canned_order())
        .expect("a chunked success envelope is an accepted order");
    disp.submit_inline(&canned_order())
        .expect("the next order on the same connection");
    assert_eq!(disp.stats().accepted, 2);
    assert_eq!(conns.load(Ordering::SeqCst), 1, "chunked framing keeps it");
}
