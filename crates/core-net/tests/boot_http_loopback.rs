// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: `core_net::boot_http` against a real `127.0.0.1`
//! TLS server with a self-signed cert (same rcgen harness as every
//! ingress loopback). Covers, per the house happy+failure rule:
//! Content-Length GET, chunked GET, POST body observed server-side,
//! non-200 status, truncated body, over-budget body, and deadline
//! timeout.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use core_net::boot_http::{https_get, https_post, BootHttpErr};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

struct LoopbackCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> LoopbackCert {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert");
    let cert_der = cert.cert.der().clone();
    let key_der =
        PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("private key DER");
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

/// Spawn a one-shot TLS server that reads until the request's blank
/// line, replies with `response`, and returns the request bytes it saw.
/// `linger_ms` holds the socket open after writing (0 = close at once).
fn one_shot_server(
    server_cfg: Arc<ServerConfig>,
    response: Vec<u8>,
    linger_ms: u64,
) -> (u16, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut tls = Stream::new(&mut conn, &mut sock);
        let mut seen: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until end-of-headers; requests in this suite either have
        // no body or carry Content-Length whose body follows headers
        // immediately (single write on the client side).
        loop {
            match tls.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    seen.extend_from_slice(&chunk[..n]);
                    if let Some(he) = find_subslice(&seen, b"\r\n\r\n") {
                        let body_len = declared_content_length(&seen[..he]);
                        if seen.len() >= he + body_len {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        tls.write_all(&response).expect("server write");
        // Flush TLS close-notify by ending the session cleanly.
        tls.conn.send_close_notify();
        let _ = tls.write_all(&[]);
        if linger_ms > 0 {
            thread::sleep(Duration::from_millis(linger_ms));
        }
        seen
    });
    (port, handle)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + needle.len())
}

fn declared_content_length(headers: &[u8]) -> usize {
    let lower: Vec<u8> = headers.to_ascii_lowercase();
    match find_subslice(&lower, b"content-length: ") {
        Some(start) => {
            let mut v = 0usize;
            for &b in &lower[start..] {
                if b.is_ascii_digit() {
                    v = v * 10 + (b - b'0') as usize;
                } else {
                    break;
                }
            }
            v
        }
        None => 0,
    }
}

const UA: &[u8] = b"multivenue-boot/1";
const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn get_content_length_body_roundtrips() {
    let cert = make_cert();
    let response =
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello-clen".to_vec();
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let range = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/api/v5/public/instruments?instType=SPOT",
        UA,
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect("fetch ok");
    assert_eq!(&out[range], b"hello-clen");
    let seen = srv.join().expect("server thread");
    assert!(seen.starts_with(b"GET /api/v5/public/instruments?instType=SPOT HTTP/1.1\r\n"));
}

#[test]
fn get_chunked_body_is_dechunked() {
    let cert = make_cert();
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
        .to_vec();
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let range = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/chunky",
        UA,
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect("fetch ok");
    assert_eq!(&out[range], b"hello world");
    srv.join().expect("server thread");
}

#[test]
fn post_body_and_headers_reach_the_server() {
    let cert = make_cert();
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec();
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let range = https_post(
        &build_client_config(&cert),
        "localhost",
        port,
        "/info",
        UA,
        b"application/json",
        b"{\"type\":\"meta\"}",
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect("fetch ok");
    assert_eq!(&out[range], b"ok");
    let seen = srv.join().expect("server thread");
    assert!(seen.starts_with(b"POST /info HTTP/1.1\r\n"));
    assert!(find_subslice(&seen, b"Content-Type: application/json\r\n").is_some());
    assert!(find_subslice(&seen, b"Content-Length: 15\r\n").is_some());
    assert!(seen.ends_with(b"{\"type\":\"meta\"}"));
}

#[test]
fn non_200_status_is_reported() {
    let cert = make_cert();
    let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let err = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/missing",
        UA,
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect_err("must fail");
    assert!(matches!(err, BootHttpErr::Status(404)), "got {err:?}");
    srv.join().expect("server thread");
}

#[test]
fn truncated_content_length_body_is_detected() {
    let cert = make_cert();
    // Declares 100 bytes, sends 5, closes.
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello".to_vec();
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let err = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/trunc",
        UA,
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect_err("must fail");
    assert!(matches!(err, BootHttpErr::Truncated), "got {err:?}");
    srv.join().expect("server thread");
}

#[test]
fn over_budget_body_is_rejected() {
    let cert = make_cert();
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n".to_vec();
    response.extend_from_slice(&[b'x'; 4096]);
    let (port, srv) = one_shot_server(build_server_config(&cert), response, 0);
    let mut out = Vec::new();
    let err = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/big",
        UA,
        &mut out,
        256, // budget far below the body
        TIMEOUT,
    )
    .expect_err("must fail");
    assert!(matches!(err, BootHttpErr::TooLarge), "got {err:?}");
    let _ = srv.join();
}

#[test]
fn silent_server_times_out() {
    let cert = make_cert();
    // Server accepts but never completes a TLS response; client must
    // give up by the deadline.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let srv = thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept");
        thread::sleep(Duration::from_millis(1500));
        drop(sock);
    });
    let mut out = Vec::new();
    let err = https_get(
        &build_client_config(&cert),
        "localhost",
        port,
        "/silent",
        UA,
        &mut out,
        1 << 20,
        Duration::from_millis(300),
    )
    .expect_err("must fail");
    assert!(
        matches!(err, BootHttpErr::Timeout | BootHttpErr::Io(_)),
        "got {err:?}"
    );
    srv.join().expect("server thread");
}

/// Failure-mode for the request writers at the boot_http layer: a path
/// larger than the request buffer is rejected before any I/O.
#[test]
fn oversized_request_is_rejected_before_io() {
    let cert = make_cert();
    let long_path = "/".repeat(8192);
    let mut out = Vec::new();
    let err = https_get(
        &build_client_config(&cert),
        "localhost",
        1, // never reached
        &long_path,
        UA,
        &mut out,
        1 << 20,
        TIMEOUT,
    )
    .expect_err("must fail");
    assert!(matches!(err, BootHttpErr::RequestTooLarge), "got {err:?}");
}
