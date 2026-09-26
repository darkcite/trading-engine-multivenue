// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: `core_net::HttpsReq` (HC2) against a real
//! `127.0.0.1` keep-alive TLS server with a self-signed cert (the rcgen
//! harness every loopback here uses). The server records every request
//! it parses and echoes its method, target, headers and body length.
//!
//! | test | asserts |
//! |---|---|
//! | methods | GET / PUT / DELETE-with-body / POST on ONE connection, each seen as sent |
//! | headers | extra headers arrive verbatim, after the fixed set |
//! | bodiless GET | no `Content-Length`, no `Content-Type` |
//! | idle close | the server hangs up after answering: the next request dials fresh and SUCCEEDS |
//! | announced close | `Connection: close` retires the connection with the answer |
//! | chunked | a many-chunk answer is located (compacted) in place |
//! | large | a 600 KB answer (a `/options-summary` body) spans many TLS records |
//! | refusals | overflow / header injection never dial |
//!
//! Offline-path doctrine: this test allocates freely.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use core_net::{HttpsReq, Method, PostErr, PostErrKind};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// How the server answers.
#[derive(Clone, Copy, Default)]
struct Script {
    /// Close the connection after this many answers on it (0 = never).
    close_after: usize,
    /// Say `Connection: close` on the last answer before closing.
    announce_close: bool,
    /// Wait this long before closing (so the close lands while idle).
    close_delay_ms: u64,
    /// Answer `Transfer-Encoding: chunked` (three chunks).
    chunked: bool,
    /// Pad every answer's body to at least this many bytes.
    pad_to: usize,
}

/// One request as the server parsed it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Default)]
struct Log {
    conns: usize,
    seen: Vec<Seen>,
}

struct Server {
    port: u16,
    client_cfg: Arc<ClientConfig>,
    log: Arc<Mutex<Log>>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_head(head: &[u8]) -> (String, String, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head).to_string();
    let mut lines = text.split("\r\n");
    let mut rl = lines.next().unwrap_or("").split(' ');
    let method = rl.next().unwrap_or("").to_string();
    let target = rl.next().unwrap_or("").to_string();
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            l.split_once(": ")
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    (method, target, headers)
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn answer(seen: &Seen, script: Script, last: bool) -> Vec<u8> {
    let sig = header(&seen.headers, "X-Hypercall-Signature").unwrap_or("");
    let mut body = format!(
        "{{\"method\":\"{}\",\"target\":\"{}\",\"body_len\":{},\"sig\":\"{}\"}}",
        seen.method,
        seen.target,
        seen.body.len(),
        sig
    )
    .into_bytes();
    while body.len() < script.pad_to {
        body.push(b' ');
    }
    let conn = if last && script.announce_close {
        "close"
    } else {
        "keep-alive"
    };
    if script.chunked {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: {conn}\r\n\r\n"
        )
        .into_bytes();
        let third = body.len().div_ceil(3).max(1);
        for part in body.chunks(third) {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    } else {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(&body);
        out
    }
}

fn serve(script: Script) -> Server {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der: CertificateDer<'static> = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("key");
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let log = Arc::new(Mutex::new(Log::default()));
    let slog = Arc::clone(&log);
    thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            slog.lock().unwrap().conns += 1;
            // One thread per connection: a client that keeps its
            // connection alive must not starve the next one's accept.
            let (server_cfg, slog) = (Arc::clone(&server_cfg), Arc::clone(&slog));
            thread::spawn(move || handle(sock, &server_cfg, script, &slog));
        }
    });
    Server {
        port,
        client_cfg,
        log,
    }
}

/// Serve one connection until the client leaves or the script closes it.
fn handle(
    mut sock: std::net::TcpStream,
    server_cfg: &Arc<ServerConfig>,
    script: Script,
    slog: &Arc<Mutex<Log>>,
) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut conn = ServerConnection::new(Arc::clone(server_cfg)).expect("conn");
    let mut tls = Stream::new(&mut conn, &mut sock);
    let mut buf: Vec<u8> = Vec::new();
    let mut answered = 0usize;
    let mut chunk = vec![0u8; 64 * 1024];
    'conn: loop {
        // One whole request: head, then Content-Length body.
        let (he, body_len) = loop {
            if let Some(he) = find(&buf, b"\r\n\r\n") {
                let (_, _, h) = parse_head(&buf[..he]);
                let n = header(&h, "Content-Length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= he + 4 + n {
                    break (he, n);
                }
            }
            match tls.read(&mut chunk) {
                Ok(0) | Err(_) => break 'conn,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let (method, target, headers) = parse_head(&buf[..he]);
        let seen = Seen {
            method,
            target,
            headers,
            body: buf[he + 4..he + 4 + body_len].to_vec(),
        };
        buf.drain(..he + 4 + body_len);
        answered += 1;
        let last = script.close_after != 0 && answered >= script.close_after;
        let out = answer(&seen, script, last);
        slog.lock().unwrap().seen.push(seen);
        if tls.write_all(&out).is_err() || tls.flush().is_err() {
            break;
        }
        if last {
            thread::sleep(Duration::from_millis(script.close_delay_ms));
            break;
        }
    }
    conn.send_close_notify();
    let _ = conn.write_tls(&mut sock);
}

fn client(s: &Server, resp_cap: usize) -> HttpsReq {
    HttpsReq::new(
        "localhost",
        s.port,
        Arc::clone(&s.client_cfg),
        1024,
        4096,
        resp_cap,
    )
    .expect("client")
}

fn echo(h: &HttpsReq, r: &std::ops::Range<usize>) -> String {
    String::from_utf8_lossy(&h.resp()[r.clone()]).to_string()
}

#[test]
fn every_method_shares_one_keep_alive_connection() {
    let s = serve(Script::default());
    let mut h = client(&s, 16 * 1024);

    let (st, r) = h
        .request(Method::Get, b"/options-summary?currency=BTC", &[], 0)
        .expect("get");
    assert_eq!(st, 200);
    assert!(echo(&h, &r)
        .contains(r#""method":"GET","target":"/options-summary?currency=BTC","body_len":0"#));

    let body = br#"{"symbol":"BTC-20261002-100000-C","price":"0.05"}"#;
    h.body_mut()[..body.len()].copy_from_slice(body);
    let (_, r) = h
        .request(Method::Put, b"/order", &[], body.len())
        .expect("put");
    assert!(echo(&h, &r).contains(&format!(
        r#""method":"PUT","target":"/order","body_len":{}"#,
        body.len()
    )));

    let cancel = br#"{"order_id":"42"}"#;
    h.body_mut()[..cancel.len()].copy_from_slice(cancel);
    let (_, r) = h
        .request(Method::Delete, b"/order", &[], cancel.len())
        .expect("delete");
    assert!(echo(&h, &r).contains(r#""method":"DELETE","target":"/order","body_len":17"#));

    let (_, r) = h
        .request(Method::Post, b"/risk/simulate/orders", &[], 2)
        .expect("post");
    assert!(echo(&h, &r).contains(r#""method":"POST""#));

    let log = s.log.lock().unwrap();
    assert_eq!(log.conns, 1, "one connection throughout");
    assert_eq!(h.dials(), 1);
    assert_eq!(log.seen.len(), 4);
    assert_eq!(
        log.seen[1].body,
        body.to_vec(),
        "the PUT body arrived intact"
    );
    assert_eq!(
        log.seen[2].body,
        cancel.to_vec(),
        "the DELETE carried its body"
    );
    assert_eq!(
        header(&log.seen[2].headers, "Content-Type"),
        Some("application/json")
    );
    assert_eq!(
        header(&log.seen[2].headers, "Connection"),
        Some("keep-alive")
    );
}

#[test]
fn a_bodiless_get_sends_no_length_and_extra_headers_arrive_verbatim() {
    let s = serve(Script::default());
    let mut h = client(&s, 16 * 1024);
    let extra: [core_net::Header<'_>; 2] = [
        (b"X-Hypercall-Expires-At-Ms", b"1790371200000"),
        (b"X-Hypercall-Signature", b"0x1b2c3d"),
    ];
    let (st, r) = h
        .request(Method::Get, b"/mmp-config?wallet=0xabc", &extra, 0)
        .expect("signed get");
    assert_eq!(st, 200);
    assert!(echo(&h, &r).contains(r#""sig":"0x1b2c3d""#));
    let log = s.log.lock().unwrap();
    let hs = &log.seen[0].headers;
    assert_eq!(
        header(hs, "Content-Length"),
        None,
        "a bodiless GET has no length"
    );
    assert_eq!(header(hs, "Content-Type"), None);
    assert_eq!(header(hs, "Accept-Encoding"), Some("identity"));
    assert_eq!(header(hs, "User-Agent"), Some("multivenue-engine/1"));
    assert_eq!(
        header(hs, "X-Hypercall-Expires-At-Ms"),
        Some("1790371200000")
    );
    assert_eq!(header(hs, "Host"), Some("localhost"));
    // The extras come after the fixed set, in order, before Connection.
    let names: Vec<&str> = hs.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "Host",
            "User-Agent",
            "Accept",
            "Accept-Encoding",
            "X-Hypercall-Expires-At-Ms",
            "X-Hypercall-Signature",
            "Connection"
        ]
    );
}

#[test]
fn an_idle_close_is_noticed_before_the_next_request_is_written() {
    let s = serve(Script {
        close_after: 1,
        close_delay_ms: 50,
        ..Script::default()
    });
    let mut h = client(&s, 16 * 1024);
    let mut i = 0;
    while i < 4 {
        // The FIN arrives while the client is idle; the reuse probe must
        // retire the connection so this request dials fresh and never
        // reports `left_host` for bytes a dead socket swallowed.
        thread::sleep(Duration::from_millis(250));
        match h.request(Method::Get, b"/health", &[], 0) {
            Ok((st, _)) => assert_eq!(st, 200),
            Err(e) => panic!("request {i}: {e} — a stale keep-alive was reused"),
        }
        i += 1;
    }
    assert_eq!(s.log.lock().unwrap().conns, 4, "one connection per answer");
    assert_eq!(h.dials(), 4, "the dial counter shows the endpoint's closes");
}

#[test]
fn an_announced_close_retires_the_connection_with_the_answer() {
    let s = serve(Script {
        close_after: 1,
        announce_close: true,
        close_delay_ms: 300,
        ..Script::default()
    });
    let mut h = client(&s, 16 * 1024);
    let (st, r) = h.request(Method::Get, b"/health", &[], 0).expect("first");
    assert_eq!(st, 200);
    assert!(echo(&h, &r).contains("/health"), "the answer is kept");
    assert!(
        !h.is_connected(),
        "`Connection: close` retired it with the answer"
    );
    let (st, _) = h
        .request(Method::Get, b"/ready", &[], 0)
        .expect("second dials fresh");
    assert_eq!(st, 200);
    assert_eq!(h.dials(), 2);
}

#[test]
fn a_chunked_answer_is_located_in_place() {
    let s = serve(Script {
        chunked: true,
        pad_to: 3000,
        ..Script::default()
    });
    let mut h = client(&s, 16 * 1024);
    let mut i = 0;
    while i < 3 {
        let (st, r) = h
            .request(Method::Get, b"/markets", &[], 0)
            .expect("chunked");
        assert_eq!(st, 200);
        assert_eq!(
            r.end - r.start,
            3000,
            "three chunks compacted into one span"
        );
        assert!(echo(&h, &r).starts_with(r#"{"method":"GET","target":"/markets""#));
        i += 1;
    }
    assert_eq!(h.dials(), 1, "chunked framing keeps the connection");
}

#[test]
fn a_large_answer_spans_many_records_and_the_connection_survives() {
    // ~600 KB: a real `/options-summary?currency=BTC` was 541 121 B
    // (docs/venue-latency.md §3, 2026-09-25).
    let s = serve(Script {
        pad_to: 600 * 1024,
        ..Script::default()
    });
    let mut h = client(&s, 1024 * 1024);
    let mut i = 0;
    while i < 3 {
        let (st, r) = h
            .request(Method::Get, b"/options-summary?currency=BTC", &[], 0)
            .expect("large");
        assert_eq!(st, 200);
        assert_eq!(r.end - r.start, 600 * 1024);
        i += 1;
    }
    assert_eq!(h.dials(), 1);
    // The same answer into a buffer too small for it is an Overflow,
    // and the connection is dropped (a half-read answer never leaks
    // into the next request).
    let mut small = client(&s, 64 * 1024);
    assert_eq!(
        small
            .request(Method::Get, b"/options-summary?currency=BTC", &[], 0)
            .err(),
        Some(PostErr {
            err: PostErrKind::Overflow,
            left_host: true
        })
    );
    assert!(!small.is_connected());
}

#[test]
fn refusals_never_dial() {
    let s = serve(Script::default());
    let mut h = client(&s, 16 * 1024);
    let bad = h
        .request(Method::Get, b"/a\r\nX-Evil: 1", &[], 0)
        .expect_err("injection");
    assert_eq!(
        bad,
        PostErr {
            err: PostErrKind::BadRequest,
            left_host: false
        }
    );
    let over = h
        .request(Method::Post, b"/", &[], 4097)
        .expect_err("body over");
    assert_eq!(over.err, PostErrKind::Overflow);
    assert!(!over.left_host);
    assert_eq!(h.dials(), 0);
    assert_eq!(s.log.lock().unwrap().conns, 0, "nothing reached the server");
}
