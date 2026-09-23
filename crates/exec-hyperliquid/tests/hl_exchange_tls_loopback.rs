// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `HlHttp` against a real rustls server on `127.0.0.1` — the house
//! standard for every network path (precedent:
//! `clob-dispatcher/tests/live_dispatcher_loopback.rs`,
//! `core-net/tests/tls_burst_loopback.rs`).
//!
//! Five scripted behaviours, per plan §5. Four of them are failures,
//! because the failures are what matter: an order path that mistakes a
//! truncated response, a dead peer or a stalled server for an
//! acceptance will book a position that does not exist.
//!
//! | script | asserts |
//! |---|---|
//! | ack | a real signed POST round-trips and the envelope scans as accepted |
//! | reject | `{"status":"err"}` at **HTTP 200** is a REFUSAL |
//! | malformed body | a non-JSON 200 body refuses, never accepts |
//! | mid-body disconnect | a truncated body is `Disconnected`, not a short read treated as complete |
//! | slow loris | a server that stalls is bounded by `REQ_DEADLINE`, not hung forever |
//! | idle close | the venue closed the kept-alive connection while idle: the next order dials fresh and SUCCEEDS — it is never written into the dead socket and reported "left the host" |
//! | `Connection: close` | an announced close retires the connection with the answer; the next order does not reuse it |

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use exec_hyperliquid::action::{encode_order, OrderWire, Tif, MAX_ACTION};
use exec_hyperliquid::http::{HlHttp, HttpErr, REQ_DEADLINE};
use exec_hyperliquid::response::{scan, HlResponse};
use exec_hyperliquid::sign::{sign_action, Network, Vault};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// What the scripted server does after reading the request.
#[derive(Copy, Clone)]
enum Script {
    /// Full, well-framed reply.
    Reply(&'static [u8]),
    /// Send headers claiming N bytes, send fewer, then hang up.
    TruncateThenClose(&'static [u8]),
    /// Send nothing at all and hold the socket open.
    Stall,
}

struct Cert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> Cert {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der = c.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");
    Cert { cert_der, key_der }
}

fn boot(script: Script) -> (u16, Arc<ClientConfig>, Arc<AtomicBool>) {
    let cert = make_cert();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("trust anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let done = Arc::new(AtomicBool::new(false));
    let done_srv = done.clone();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut conn = ServerConnection::new(server_cfg).expect("conn");
        let mut stream = Stream::new(&mut conn, &mut sock);
        if !read_one_request(&mut stream) {
            return;
        }

        match script {
            Script::Reply(body) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
            Script::TruncateThenClose(body) => {
                // Claim the full length, send half, then hang up. A
                // reader that trusted what arrived would hand a
                // half-JSON envelope to the scanner.
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body[..body.len() / 2]);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(30));
                let _ = sock.shutdown(Shutdown::Both);
                return;
            }
            Script::Stall => {
                // Never answer. Hold the socket past the deadline.
                let deadline = Instant::now() + REQ_DEADLINE + Duration::from_secs(3);
                while !done_srv.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                let _ = sock.shutdown(Shutdown::Both);
                return;
            }
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        while !done_srv.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        let _ = sock.shutdown(Shutdown::Both);
    });

    (port, client_cfg, done)
}

/// Read one request — the head and its declared body — off `stream`.
/// `false` at EOF or on a read error.
fn read_one_request<S: Read>(stream: &mut S) -> bool {
    let mut buf = [0u8; 32 * 1024];
    let mut total = 0usize;
    let header_end = loop {
        let Ok(n) = stream.read(&mut buf[total..]) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        total += n;
        if let Some(i) = (0..total.saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n") {
            break i + 4;
        }
        if total == buf.len() {
            return false;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let clen: usize = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let mut remaining = clen.saturating_sub(total - header_end);
    while remaining > 0 {
        let need = remaining.min(buf.len());
        let Ok(n) = stream.read(&mut buf[..need]) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        remaining -= n;
    }
    true
}

/// A venue that answers ONE request per connection and then closes it:
/// after `close_delay` (the idle close — the client has already read the
/// answer when the FIN arrives), announcing it with `Connection: close`
/// when `announce`. Serves any number of connections; counts them.
fn boot_closing(
    announce: bool,
    close_delay: Duration,
) -> (u16, Arc<ClientConfig>, Arc<std::sync::atomic::AtomicUsize>) {
    let cert = make_cert();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepts_srv = accepts.clone();
    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut sock) = incoming else { return };
            accepts_srv.fetch_add(1, Ordering::AcqRel);
            let mut conn = ServerConnection::new(server_cfg.clone()).expect("conn");
            {
                let mut stream = Stream::new(&mut conn, &mut sock);
                if !read_one_request(&mut stream) {
                    continue;
                }
                let h = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                    OK_RESTING.len(),
                    if announce { "close" } else { "keep-alive" }
                );
                let _ = stream.write_all(h.as_bytes());
                let _ = stream.write_all(OK_RESTING);
                let _ = stream.flush();
            }
            thread::sleep(close_delay);
            conn.send_close_notify();
            let _ = conn.write_tls(&mut sock);
            let _ = sock.shutdown(Shutdown::Both);
        }
    });
    (port, client_cfg, accepts)
}

const TEST_KEY: [u8; 32] = [0x2b; 32];

/// Build a real, really-signed request body — the same path a live
/// order takes. The loopback server never verifies it; the point is
/// that the bytes on the wire are the production bytes.
fn signed_body(out: &mut Vec<u8>) {
    let mut action = [0u8; MAX_ACTION];
    let n = encode_order(
        &mut action,
        &[OrderWire::new(100_032_530, true, 45_670_000, 2_500_000_000, Tif::Ioc)],
        b"na",
    )
    .expect("encode");
    let sk = signer_eip712::parse_secret_key(&TEST_KEY).expect("key");
    let sig = sign_action(
        &sk,
        &action[..n],
        1_789_000_000_000,
        Vault::None,
        None,
        Network::Testnet,
    )
    .expect("sign");

    // The venue's request envelope: the action JSON, the nonce and the
    // r/s/v. (E4 gives this its own encoder; the loopback only needs
    // well-formed bytes of the right size.)
    out.clear();
    out.extend_from_slice(br#"{"action":{"type":"order"},"nonce":1789000000000,"signature":{"r":"0x"#);
    for b in &sig[..32] {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    out.extend_from_slice(br#"","s":"0x"#);
    for b in &sig[32..64] {
        out.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    out.extend_from_slice(format!(r#"","v":{}}}}}"#, sig[64]).as_bytes());
}

fn client(port: u16, cfg: Arc<ClientConfig>) -> HlHttp {
    HlHttp::new("localhost", port, cfg).expect("construct")
}

const OK_RESTING: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424242}}]}}}"#;
const ERR_ENVELOPE: &[u8] = br#"{"status":"err","response":"Unable to recover signer."}"#;
const NOT_JSON: &[u8] = b"Failed to deserialize the JSON body into the target type";

#[test]
fn ack_round_trips_and_scans_as_accepted() {
    let (port, cfg, done) = boot(Script::Reply(OK_RESTING));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);

    let (status, range) = c.post(&body).expect("round trip");
    assert_eq!(status, 200);
    let scanned = scan(&c.resp()[range]).expect("scan");
    let HlResponse::Ok(o) = scanned else {
        panic!("expected Ok, got {scanned:?}")
    };
    assert!(o.accepted());
    assert_eq!(o.oid, 424_242);
    assert!(o.any_resting);
    assert!(c.is_connected(), "keep-alive must survive a good reply");
    done.store(true, Ordering::Release);
}

/// The trap, end to end: HTTP 200 carrying a refusal.
#[test]
fn a_rejection_at_http_200_is_a_refusal_not_an_acceptance() {
    let (port, cfg, done) = boot(Script::Reply(ERR_ENVELOPE));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);

    let (status, range) = c.post(&body).expect("round trip");
    assert_eq!(status, 200, "the venue really does answer 200 here");
    // A `Span` indexes the slice that was SCANNED, not the whole
    // response buffer — so resolve it against the same body slice.
    let resp = c.resp();
    let body_bytes = &resp[range];
    match scan(body_bytes).expect("scan") {
        HlResponse::Err { msg } => {
            assert_eq!(msg.of(body_bytes), b"Unable to recover signer.");
        }
        other => panic!("HTTP 200 + status:err must be a refusal, got {other:?}"),
    }
    done.store(true, Ordering::Release);
}

#[test]
fn a_malformed_body_refuses_rather_than_accepting() {
    let (port, cfg, done) = boot(Script::Reply(NOT_JSON));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);

    let (status, range) = c.post(&body).expect("round trip");
    assert_eq!(status, 200);
    assert!(
        scan(&c.resp()[range]).is_err(),
        "an unparseable body must never read as an acceptance"
    );
    done.store(true, Ordering::Release);
}

/// A body shorter than its declared Content-Length must be an error.
/// Handing the scanner half an envelope is how a refusal gets read as
/// an acceptance — or an oid gets invented.
#[test]
fn a_mid_body_disconnect_is_an_error_not_a_short_read() {
    let (port, cfg, done) = boot(Script::TruncateThenClose(OK_RESTING));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);

    let e = match c.post(&body) {
        Err(e) => e,
        Ok((s, r)) => panic!("a truncated body must not succeed: {s} {r:?}"),
    };
    assert_eq!(e.err, HttpErr::Disconnected);
    // **The load-bearing half.** The server read our request and then
    // died on its answer, so the venue HAS the action — it may have
    // placed an order. The address-rate governor must count it, and
    // it can only know to from this flag: `Disconnected` alone is
    // also what a failed connect returns, where nothing left at all.
    assert!(
        e.left_host,
        "a request the server already read must report that it left"
    );
    assert!(
        !c.is_connected(),
        "a failed cycle must drop the connection, or the next order reads this one's leftovers"
    );
    done.store(true, Ordering::Release);
}

/// A server that accepts, reads, and then says nothing must not hold
/// the worker thread forever.
#[test]
fn a_stalled_server_is_bounded_by_the_deadline() {
    let (port, cfg, done) = boot(Script::Stall);
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);

    let t0 = Instant::now();
    let e = match c.post(&body) {
        Err(e) => e,
        Ok(_) => panic!("a silent server must not look like a reply"),
    };
    let waited = t0.elapsed();
    assert_eq!(e.err, HttpErr::Timeout);
    assert!(
        e.left_host,
        "a stalled server is one that already has our request"
    );
    assert!(
        waited < REQ_DEADLINE + Duration::from_secs(3),
        "gave up after {waited:?}, which is not bounded by REQ_DEADLINE {REQ_DEADLINE:?}"
    );
    assert!(!c.is_connected());
    done.store(true, Ordering::Release);
}

/// Keep-alive: two orders over one TLS session. The handshake is
/// 50–150 ms on a WAN and paying it per order would dominate the
/// latency budget.
#[test]
fn the_connection_is_reused_across_requests() {
    let cert = make_cert();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepts_srv = accepts.clone();
    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        accepts_srv.fetch_add(1, Ordering::Release);
        let mut conn = ServerConnection::new(server_cfg).expect("conn");
        let mut stream = Stream::new(&mut conn, &mut sock);
        // Answer two requests on the same connection.
        for _ in 0..2 {
            if !read_one_request(&mut stream) {
                return;
            }
            let h = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                OK_RESTING.len()
            );
            let _ = stream.write_all(h.as_bytes());
            let _ = stream.write_all(OK_RESTING);
            let _ = stream.flush();
        }
        thread::sleep(Duration::from_millis(200));
        let _ = sock.shutdown(Shutdown::Both);
    });

    let mut c = HlHttp::new("localhost", port, client_cfg).expect("construct");
    let mut body = Vec::new();
    signed_body(&mut body);

    for i in 0..2 {
        let (status, range) = c.post(&body).unwrap_or_else(|e| panic!("request {i}: {e}"));
        assert_eq!(status, 200);
        let HlResponse::Ok(o) = scan(&c.resp()[range]).expect("scan") else {
            panic!("request {i}")
        };
        assert_eq!(o.oid, 424_242);
    }
    assert_eq!(
        accepts.load(Ordering::Acquire),
        1,
        "two orders must share ONE TLS session"
    );
    assert_eq!(c.dials(), 1);
}

/// The venue closed the kept-alive connection while we were idle. The
/// next order must dial fresh and SUCCEED — before the fix it was
/// written into the dead socket, read EOF, and came back `Disconnected`
/// with `left_host == true`: an order lost to reconciliation that never
/// reached the venue (operator ask, 2026-09-24).
#[test]
fn an_idle_close_is_noticed_before_the_next_order_is_written() {
    let (port, cfg, accepts) = boot_closing(false, Duration::from_millis(50));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);
    let mut i = 0;
    while i < 3 {
        // Idle long enough for the venue's close to land.
        thread::sleep(Duration::from_millis(250));
        let (status, range) = c
            .post(&body)
            .unwrap_or_else(|e| panic!("order {i}: {e} — a closed keep-alive was reused"));
        assert_eq!(status, 200);
        let HlResponse::Ok(o) = scan(&c.resp()[range]).expect("scan") else {
            panic!("order {i}")
        };
        assert_eq!(o.oid, 424_242);
        i += 1;
    }
    assert_eq!(
        accepts.load(Ordering::Acquire),
        3,
        "one connection per answer"
    );
    assert_eq!(c.dials(), 3, "the dial counter shows the venue's closes");
}

/// `Connection: close` on an answer retires the connection WITH the
/// answer (kept intact) — the next order, sent at once, before the venue
/// has actually closed, must not reuse it.
#[test]
fn an_announced_close_retires_the_connection_with_the_answer() {
    let (port, cfg, accepts) = boot_closing(true, Duration::from_millis(300));
    let mut c = client(port, cfg);
    let mut body = Vec::new();
    signed_body(&mut body);
    let (status, range) = c.post(&body).expect("first");
    assert_eq!(status, 200);
    assert!(!c.is_connected(), "`Connection: close` retires it at once");
    let HlResponse::Ok(o) = scan(&c.resp()[range]).expect("the answer survives") else {
        panic!("first")
    };
    assert_eq!(o.oid, 424_242);
    let (status, _) = c.post(&body).expect("second, immediately");
    assert_eq!(status, 200);
    assert_eq!(accepts.load(Ordering::Acquire), 2);
}
