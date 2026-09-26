// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX5: `core_net::HttpsConn` — the non-blocking keep-alive client an
//! exec gateway drives from its own poll — against a scripted rustls
//! server on `127.0.0.1` (rcgen certificate).
//!
//! | test | asserts |
//! |---|---|
//! | templates | GET-query, POST/DELETE-body, PUT-empty and bare-GET requests leave exactly as rendered, all on ONE connection |
//! | non-blocking | `start` returns at once; the owner's loop keeps turning while the answer is 300 ms away |
//! | silence | a server that never answers times out with `left_host`; the next request dials fresh and succeeds |
//! | idle close | a close while idle is noticed (event or reuse probe): the next request dials fresh, never a false `left_host` |
//! | announced close | `Connection: close` retires the connection with the answer |
//! | refused dial | nothing listening: `Disconnected`, `left_host == false`, the fail streak counts |
//! | framing | a chunked answer arrives whole; an answer over the buffer is `Overflow` |
//! | incomplete close | a FIN without `close_notify` after a Content-Length or chunked answer read whole still delivers it; a close-delimited answer counts only after a close a read saw clean — never after a bare FIN (small or past rustls' 16 KiB wave) nor cut by our own full buffer (RFC 9112 §9.8) |
//! | overflow | a request over the window is refused before any connection opens |
//!
//! Offline-path doctrine: this test allocates freely.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_net::{ConnCfg, HttpsConn, Method, Params, PostErr, PostErrKind, Progress, ReqSpec};
use mio::{Events, Poll, Token};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

const TOKEN: Token = Token(42);
const KEY: &str = "k3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK3yk3yK";

/// What the scripted server does.
#[derive(Clone, Default)]
struct Behave {
    /// Sleep before each answer.
    delay_ms: u64,
    /// Hang up after this many answers on a connection (0 = never)…
    close_after: u32,
    /// …after this long (the client has read the answer by then)…
    close_delay_ms: u64,
    /// …and say so on the last answer.
    announce_close: bool,
    /// Answer `Transfer-Encoding: chunked`, in three chunks.
    chunked: bool,
    /// Read requests, never answer.
    silent: bool,
    /// Pad each answer's body to at least this many bytes.
    body_len: usize,
    /// From this answer on a connection (0 = never): no
    /// `Content-Length`, no chunking — the body ends at the close, which
    /// follows the answer at once.
    close_delimited_from: u32,
    /// With `close_delimited_from`: close with a bare TCP FIN — no
    /// `close_notify`.
    bare_fin: bool,
    /// After this many answers on a connection (0 = never), a bare FIN
    /// right behind the answer, in the same breath.
    bare_fin_after: u32,
}

struct Server {
    port: u16,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    conns: Arc<AtomicU32>,
    client_cfg: Arc<ClientConfig>,
}

fn certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key");
    (c.cert.der().clone(), key)
}

fn client_cfg(cert: &CertificateDer<'static>) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).expect("anchor");
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// One request off the stream: head and `Content-Length` body. `None`
/// at EOF.
fn read_request<S: Read>(s: &mut S, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let mut b = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let clen: usize = head
                .lines()
                .find(|l| l.starts_with("content-length:"))
                .and_then(|l| l[15..].trim().parse().ok())
                .unwrap_or(0);
            let need = i + 4 + clen;
            while buf.len() < need {
                let n = s.read(&mut b).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&b[..n]);
            }
            let req = buf[..need].to_vec();
            buf.drain(..need);
            return Some(req);
        }
        let n = s.read(&mut b).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&b[..n]);
    }
}

fn serve(b: Behave) -> Server {
    let (cert, key) = certs();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server cfg"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let conns = Arc::new(AtomicU32::new(0));
    let (s2, c2) = (seen.clone(), conns.clone());
    thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            c2.fetch_add(1, Ordering::SeqCst);
            let mut tls = ServerConnection::new(server_cfg.clone()).expect("conn");
            let mut pending = Vec::new();
            let mut answered = 0u32;
            loop {
                let Some(req) = read_request(&mut Stream::new(&mut tls, &mut sock), &mut pending)
                else {
                    break;
                };
                let line = String::from_utf8_lossy(&req[..req.iter().position(|&c| c == b'\r').unwrap_or(0)])
                    .into_owned();
                s2.lock().unwrap().push(req);
                if b.silent {
                    continue;
                }
                thread::sleep(Duration::from_millis(b.delay_ms));
                answered += 1;
                let last = b.close_after != 0 && answered >= b.close_after;
                let conn_hdr = if last && b.announce_close { "close" } else { "keep-alive" };
                let mut body = format!("{{\"n\":{answered},\"line\":\"{line}\"}}");
                while body.len() < b.body_len {
                    body.push(' ');
                }
                let delimited = b.close_delimited_from != 0 && answered >= b.close_delimited_from;
                let wire = if delimited {
                    format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}")
                } else if b.chunked {
                    let third = body.len() / 3;
                    let (a, rest) = body.split_at(third);
                    let (m, z) = rest.split_at(third);
                    format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: {conn_hdr}\r\n\r\n\
                         {:x}\r\n{a}\r\n{:x}\r\n{m}\r\n{:x}\r\n{z}\r\n0\r\n\r\n",
                        a.len(),
                        m.len(),
                        z.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                         Connection: {conn_hdr}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let mut st = Stream::new(&mut tls, &mut sock);
                if st.write_all(wire.as_bytes()).is_err() {
                    break;
                }
                let _ = st.flush();
                if delimited {
                    if !b.bare_fin {
                        tls.send_close_notify();
                        let _ = tls.write_tls(&mut sock);
                    }
                    let _ = sock.shutdown(std::net::Shutdown::Write);
                    thread::sleep(Duration::from_millis(300));
                    break;
                }
                if b.bare_fin_after != 0 && answered >= b.bare_fin_after {
                    let _ = sock.shutdown(std::net::Shutdown::Write);
                    thread::sleep(Duration::from_millis(300));
                    break;
                }
                if last {
                    thread::sleep(Duration::from_millis(b.close_delay_ms));
                    tls.send_close_notify();
                    let _ = tls.write_tls(&mut sock);
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
        }
    });
    Server {
        port,
        seen,
        conns,
        client_cfg: client_cfg(&cert),
    }
}

const H: &[(&str, &str)] = &[("X-MBX-APIKEY", KEY)];
const FORM: Params<'static> = Params::Body("application/x-www-form-urlencoded");

fn specs() -> [ReqSpec<'static>; 5] {
    [
        ReqSpec { method: Method::Post, path: "/fapi/v1/order", params: FORM, headers: H },
        ReqSpec { method: Method::Delete, path: "/fapi/v1/order", params: FORM, headers: H },
        ReqSpec { method: Method::Get, path: "/fapi/v1/openOrders", params: Params::Query, headers: H },
        ReqSpec { method: Method::Put, path: "/fapi/v1/listenKey", params: Params::Query, headers: H },
        ReqSpec { method: Method::Get, path: "/fapi/v1/time", params: Params::Query, headers: &[] },
    ]
}

/// A gateway-shaped owner: its own poll, the connection registered
/// under `TOKEN`, a 5 ms poll slice, a tick every pass.
struct Owner {
    poll: Poll,
    events: Events,
    conn: HttpsConn,
    epoch: Instant,
    passes: u64,
}

impl Owner {
    fn new(port: u16, cfg: Arc<ClientConfig>, win_cap: usize, resp_cap: usize, timeout_ms: u64) -> Self {
        let c = ConnCfg {
            win_cap,
            resp_cap,
            req_timeout_ns: timeout_ms * 1_000_000,
        };
        Self {
            poll: Poll::new().expect("poll"),
            events: Events::with_capacity(16),
            conn: HttpsConn::new("localhost", port, cfg, &specs(), c, TOKEN).expect("conn"),
            epoch: Instant::now(),
            passes: 0,
        }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn start(&mut self, t: usize, content: &[u8]) -> Result<(), PostErr> {
        self.conn.window_mut(t)[..content.len()].copy_from_slice(content);
        let now = self.now();
        self.conn.start(t, content.len(), self.poll.registry(), now)
    }

    /// One loop pass: poll, dispatch, tick.
    fn pass(&mut self) -> Progress {
        self.poll
            .poll(&mut self.events, Some(Duration::from_millis(5)))
            .expect("poll");
        self.passes += 1;
        for ev in self.events.iter() {
            assert_eq!(ev.token(), TOKEN, "only the connection's token is registered");
            match self.conn.on_event(ev, self.poll.registry()) {
                Progress::Waiting => {}
                p => return p,
            }
        }
        let now = self.now();
        self.conn.on_tick(now)
    }

    fn wait(&mut self) -> Progress {
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < give_up, "the request never finished");
            match self.pass() {
                Progress::Waiting => {}
                p => return p,
            }
        }
    }

    /// Keep the loop turning with no request in flight (idle events).
    fn idle(&mut self, d: Duration) {
        let until = Instant::now() + d;
        while Instant::now() < until {
            assert_eq!(self.pass(), Progress::Waiting, "idle passes report nothing");
        }
    }

    fn request(&mut self, t: usize, content: &[u8]) -> Result<(u16, String), PostErr> {
        self.start(t, content)?;
        match self.wait() {
            Progress::Done { status, body } => Ok((
                status,
                String::from_utf8_lossy(&self.conn.resp()[body.start..body.end]).into_owned(),
            )),
            Progress::Failed(e) => Err(e),
            Progress::Waiting => unreachable!("wait returns only an outcome"),
        }
    }
}

#[test]
fn every_template_leaves_exactly_as_rendered_on_one_connection() {
    let s = serve(Behave::default());
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    let order = b"symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=IOC&quantity=0.001&price=60000&timestamp=1&signature=sig%2Bsig%3D";
    let mut k = 0u32;
    while k < 20 {
        let (t, content): (usize, &[u8]) = match k % 5 {
            0 => (0, order),
            1 => (1, b"symbol=BTCUSDT&origClientOrderId=mv0&timestamp=2&signature=s"),
            2 => (2, b"symbol=BTCUSDT&timestamp=3&signature=s"),
            3 => (3, b""),
            _ => (4, b""),
        };
        let (status, body) = o.request(t, content).expect("round trip");
        assert_eq!(status, 200);
        assert!(body.contains(&format!("\"n\":{}", k + 1)), "answer {k}: {body}");
        k += 1;
    }
    assert_eq!(s.conns.load(Ordering::SeqCst), 1, "one keep-alive connection");
    assert_eq!(o.conn.dials(), 1);
    assert!(o.conn.is_connected() && o.conn.is_idle());

    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 20);
    let tail = format!("Host: localhost\r\nX-MBX-APIKEY: {KEY}\r\n");
    assert_eq!(
        String::from_utf8_lossy(&seen[0]),
        format!(
            "POST /fapi/v1/order HTTP/1.1\r\n{tail}Content-Type: application/x-www-form-urlencoded\r\n\
             Connection: keep-alive\r\nContent-Length: {}\r\n\r\n{}",
            order.len(),
            String::from_utf8_lossy(order)
        )
    );
    assert!(String::from_utf8_lossy(&seen[1]).starts_with("DELETE /fapi/v1/order HTTP/1.1\r\n"));
    assert_eq!(
        String::from_utf8_lossy(&seen[2]),
        format!(
            "GET /fapi/v1/openOrders?symbol=BTCUSDT&timestamp=3&signature=s HTTP/1.1\r\n{tail}\
             Connection: keep-alive\r\n\r\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&seen[3]),
        format!("PUT /fapi/v1/listenKey HTTP/1.1\r\n{tail}Content-Length: 0\r\nConnection: keep-alive\r\n\r\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&seen[4]),
        "GET /fapi/v1/time HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n"
    );
    // The same requests again, byte for byte, four more times.
    let mut i = 5;
    while i < 20 {
        assert_eq!(seen[i], seen[i % 5], "request {i}");
        i += 1;
    }
}

#[test]
fn start_returns_at_once_and_the_owner_keeps_turning() {
    let s = serve(Behave {
        delay_ms: 300,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    // First request dials: start must not wait for TCP, TLS or the answer.
    let t0 = Instant::now();
    o.start(2, b"a=1").expect("start");
    assert!(t0.elapsed() < Duration::from_millis(50), "start blocked {:?}", t0.elapsed());
    assert!(!o.conn.is_idle());
    let p0 = o.passes;
    let Progress::Done { status, .. } = o.wait() else { panic!("done") };
    assert_eq!(status, 200);
    assert!(o.passes - p0 >= 20, "the loop turned only {} times in 300 ms", o.passes - p0);
    // Kept alive: the second start writes at once.
    let t1 = Instant::now();
    o.start(0, b"b=2").expect("start");
    assert!(t1.elapsed() < Duration::from_millis(50));
    assert!(matches!(o.wait(), Progress::Done { status: 200, .. }));
    assert_eq!(o.conn.dials(), 1);
}

#[test]
fn a_silent_server_times_out_with_left_host_and_the_next_request_redials() {
    let s = serve(Behave {
        silent: true,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 200);
    let t0 = Instant::now();
    assert_eq!(
        o.request(0, b"x=1"),
        Err(PostErr {
            err: PostErrKind::Timeout,
            left_host: true
        }),
        "the request was written: it may have been taken"
    );
    assert!(t0.elapsed() >= Duration::from_millis(200));
    assert!(!o.conn.is_connected() && o.conn.is_idle(), "a timeout closes the connection");
    assert_eq!(o.conn.fail_streak(), 0, "the dial itself succeeded");
    // A second silent round dials a second connection.
    assert!(matches!(o.request(2, b"y=2"), Err(PostErr { err: PostErrKind::Timeout, .. })));
    assert_eq!((o.conn.dials(), s.conns.load(Ordering::SeqCst)), (2, 2));
}

#[test]
fn an_idle_close_is_noticed_and_the_next_request_dials_fresh() {
    let s = serve(Behave {
        close_after: 1,
        close_delay_ms: 50,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    let mut k = 0;
    while k < 4 {
        match o.request(k % 3, b"z=1") {
            Ok((status, _)) => assert_eq!(status, 200),
            Err(e) => panic!("request {k}: {e} — a stale keep-alive was reused"),
        }
        if k % 2 == 0 {
            // The FIN arrives while the loop turns: the idle event drops
            // the connection…
            o.idle(Duration::from_millis(250));
            assert!(!o.conn.is_connected(), "the idle close was seen");
        } else {
            // …or, with no loop turning, the reuse probe does.
            thread::sleep(Duration::from_millis(250));
        }
        k += 1;
    }
    assert_eq!(s.conns.load(Ordering::SeqCst), 4, "one connection per answer");
    assert_eq!(o.conn.dials(), 4);
}

#[test]
fn an_announced_close_retires_the_connection_with_the_answer() {
    let s = serve(Behave {
        close_after: 1,
        close_delay_ms: 300,
        announce_close: true,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    let (status, body) = o.request(0, b"a=1").expect("first");
    assert_eq!(status, 200);
    assert!(body.contains("\"n\":1"));
    assert!(!o.conn.is_connected(), "`Connection: close` retires it at once");
    assert!(!o.conn.resp().is_empty(), "the answer survives the retirement");
    // At once — the server has not closed yet: no reuse of the doomed one.
    o.request(0, b"a=2").expect("second");
    assert_eq!(s.conns.load(Ordering::SeqCst), 2);
}

#[test]
fn a_refused_dial_fails_without_left_host_and_counts() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }; // dropped: nothing listens there now
    let (cert, _) = certs();
    let mut o = Owner::new(port, client_cfg(&cert), 1024, 8192, 2_000);
    let mut k = 1;
    while k <= 3 {
        match o.request(0, b"a=1") {
            Err(PostErr {
                err: PostErrKind::Disconnected,
                left_host: false,
            }) => {}
            other => panic!("attempt {k}: {other:?}"),
        }
        assert_eq!(o.conn.fail_streak(), k);
        k += 1;
    }
    assert_eq!(o.conn.dials(), 0);
}

#[test]
fn a_chunked_answer_arrives_whole_and_an_oversized_one_overflows() {
    let s = serve(Behave {
        chunked: true,
        body_len: 900,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    let (status, body) = o.request(2, b"q=1").expect("chunked");
    assert_eq!(status, 200);
    assert_eq!(body.len(), 900);
    assert!(body.starts_with("{\"n\":1,\"line\":\"GET /fapi/v1/openOrders?q=1 HTTP/1.1\"}"));

    let big = serve(Behave {
        body_len: 4000,
        ..Behave::default()
    });
    let mut o = Owner::new(big.port, big.client_cfg.clone(), 1024, 1024, 5_000);
    assert_eq!(
        o.request(0, b"a=1"),
        Err(PostErr {
            err: PostErrKind::Overflow,
            left_host: true
        })
    );
    assert!(!o.conn.is_connected());
}

#[test]
fn a_request_over_the_window_is_refused_before_any_connection() {
    let s = serve(Behave::default());
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 64, 1024, 5_000);
    let now = o.now();
    assert_eq!(
        o.conn.start(0, 65, o.poll.registry(), now),
        Err(PostErr {
            err: PostErrKind::Overflow,
            left_host: false
        })
    );
    assert!(o.conn.is_idle());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(s.conns.load(Ordering::SeqCst), 0, "no connection was even opened");
}

#[test]
fn an_answer_proven_whole_survives_a_close_without_close_notify() {
    // Content-Length, then a bare FIN in the same breath: the answer is
    // authentic and whole by its own framing — it must be delivered, not
    // turned into a false in-doubt (the pre-BX5 client lost it: rustls
    // reports the FIN as `UnexpectedEof` on the read after the answer).
    let s = serve(Behave {
        bare_fin_after: 2,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    o.request(0, b"a=1").expect("the first answer, kept alive");
    assert!(o.conn.is_connected());
    // Written at once on the kept-alive connection; the loop does not
    // turn until the answer AND the FIN are both in the socket, so one
    // wake reads both.
    o.start(0, b"a=2").expect("start");
    thread::sleep(Duration::from_millis(200));
    let Progress::Done { status, body } = o.wait() else {
        panic!("the answer was lost to the bare FIN")
    };
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&o.conn.resp()[body.start..body.end]).into_owned();
    assert!(text.contains("\"n\":2"), "{text}");
    assert!(!o.conn.is_connected(), "the closed connection is retired");
    o.request(0, b"a=3").expect("the next request dials fresh");
    assert_eq!(o.conn.dials(), 2);
}

#[test]
fn a_close_delimited_answer_needs_a_clean_close() {
    let clean = serve(Behave {
        close_delimited_from: 1,
        ..Behave::default()
    });
    let mut o = Owner::new(clean.port, clean.client_cfg.clone(), 1024, 8192, 5_000);
    let (status, body) = o.request(2, b"q=1").expect("clean close: whole");
    assert_eq!(status, 200);
    assert!(body.starts_with("{\"n\":1,"), "{body}");

    let cut = serve(Behave {
        close_delimited_from: 1,
        bare_fin: true,
        ..Behave::default()
    });
    let mut o = Owner::new(cut.port, cut.client_cfg.clone(), 1024, 8192, 5_000);
    assert_eq!(
        o.request(2, b"q=1"),
        Err(PostErr {
            err: PostErrKind::Disconnected,
            left_host: true
        }),
        "a bare FIN cannot be told from a truncation"
    );
}

#[test]
fn a_chunked_answer_proven_whole_survives_a_close_without_close_notify() {
    let s = serve(Behave {
        chunked: true,
        bare_fin_after: 2,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8192, 5_000);
    o.request(2, b"q=1").expect("the first answer, kept alive");
    o.start(2, b"q=2").expect("start");
    thread::sleep(Duration::from_millis(200));
    let Progress::Done { status, body } = o.wait() else {
        panic!("the chunked answer was lost to the bare FIN")
    };
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&o.conn.resp()[body.start..body.end]).into_owned();
    assert!(text.starts_with("{\"n\":2,"), "{text}");
    assert!(!o.conn.is_connected());
}

#[test]
fn a_large_close_delimited_answer_needs_a_clean_close_too() {
    // Past rustls' 16 KiB received-plaintext wave the transport's
    // pull-through, not the pump, meets the FIN — and reports it as a
    // plain end of stream. The confirming read must still tell a bare
    // FIN from `close_notify`. The second answer (40 KB) and its close
    // are both in the socket before the loop turns.
    let mut k = 0;
    while k < 2 {
        let bare = k == 1;
        let s = serve(Behave {
            close_delimited_from: 2,
            bare_fin: bare,
            body_len: 40_000,
            ..Behave::default()
        });
        let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 64 * 1024, 5_000);
        o.request(2, b"q=1").expect("the first answer, kept alive");
        o.start(2, b"q=2").expect("start");
        thread::sleep(Duration::from_millis(200));
        match (bare, o.wait()) {
            (false, Progress::Done { status, body }) => {
                assert_eq!((status, body.end - body.start), (200, 40_000), "clean close: whole");
            }
            (
                true,
                Progress::Failed(PostErr {
                    err: PostErrKind::Disconnected,
                    left_host: true,
                }),
            ) => {}
            (bare, other) => panic!("bare FIN {bare}: {other:?}"),
        }
        k += 1;
    }
}

#[test]
fn a_close_delimited_answer_cut_by_our_own_buffer_overflows() {
    let s = serve(Behave {
        close_delimited_from: 1,
        body_len: 40_000,
        ..Behave::default()
    });
    let mut o = Owner::new(s.port, s.client_cfg.clone(), 1024, 8 * 1024, 5_000);
    assert_eq!(
        o.request(2, b"q=1"),
        Err(PostErr {
            err: PostErrKind::Overflow,
            left_host: true
        }),
        "a buffer-full close-delimited body is never whole"
    );
}
