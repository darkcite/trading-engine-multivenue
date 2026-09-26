// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX5: `core_net::WsConn` — the non-blocking client WebSocket an exec
//! gateway drives from its own poll — against a scripted rustls server
//! on `127.0.0.1` (rcgen certificate).
//!
//! | test | asserts |
//! |---|---|
//! | open + echo | the upgrade opens (a frame packed with the 101 is kept); three frames flushed in ONE write arrive intact; a ping is echoed byte for byte |
//! | wrong accept | an upgrade answered with someone else's accept is refused |
//! | fragmented | a fragmented message ends the session |
//! | too large | a frame the window can never hold ends the session |
//! | burst | 11 KiB of frames through a 1 KiB window: every frame, in order (the edge-triggered starvation path) |
//! | idle | a silent session dies at the idle limit |
//! | close | a Close frame ends the session |
//! | bare FIN | frames read before a FIN without `close_notify` still drain, then the session ends |
//! | no TLS | a peer that never speaks TLS times out the establishment |
//!
//! Offline-path doctrine: this test allocates freely.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_net::{expected_accept, WsCfg, WsConn, WsErr, WsNext, WsProgress};
use mio::{Events, Poll, Token};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, StreamOwned};

const TOKEN: Token = Token(9);

type Tls = StreamOwned<ServerConnection, TcpStream>;

/// A server frame (unmasked).
fn frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![(if fin { 0x80 } else { 0 }) | op];
    if payload.len() < 126 {
        v.push(payload.len() as u8);
    } else {
        v.push(126);
        v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

/// Read one CLIENT frame: (opcode, unmasked payload). Asserts the
/// client's side of RFC 6455: FIN, masked.
fn read_client_frame(tls: &mut Tls, buf: &mut Vec<u8>) -> (u8, Vec<u8>) {
    let mut b = [0u8; 4096];
    loop {
        if buf.len() >= 2 {
            assert!(buf[0] & 0x80 != 0, "the client never fragments");
            assert!(buf[1] & 0x80 != 0, "a client frame is masked");
            let (len, at) = match buf[1] & 0x7F {
                126 if buf.len() >= 4 => (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4),
                126 => (usize::MAX, 0),
                127 => panic!("no 64-bit frames in these tests"),
                n => (n as usize, 2),
            };
            if len != usize::MAX && buf.len() >= at + 4 + len {
                let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
                let p = (0..len).map(|i| buf[at + 4 + i] ^ mask[i & 3]).collect();
                let op = buf[0] & 0x0F;
                buf.drain(..at + 4 + len);
                return (op, p);
            }
        }
        let n = tls.read(&mut b).expect("server read");
        assert!(n > 0, "the client hung up mid-frame");
        buf.extend_from_slice(&b[..n]);
    }
}

fn certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key");
    (c.cert.der().clone(), key)
}

/// Accept one TLS client, read its upgrade request, answer `101` with
/// the right accept (or `bad_accept`) and `packed` frames in the same
/// write, then hand the stream to `script`.
fn serve<F>(bad_accept: bool, packed: Vec<u8>, script: F) -> (u16, Arc<ClientConfig>, thread::JoinHandle<()>)
where
    F: FnOnce(&mut Tls, &mut Vec<u8>) + Send + 'static,
{
    let (cert, key) = certs();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().expect("accept");
        sock.set_nodelay(true).ok();
        let mut tls = StreamOwned::new(ServerConnection::new(server_cfg).expect("conn"), sock);
        let mut buf = Vec::new();
        let mut b = [0u8; 4096];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = tls.read(&mut b).expect("read upgrade");
            assert!(n > 0, "no upgrade request");
            buf.extend_from_slice(&b[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        buf.drain(..head_end);
        assert!(head.starts_with("GET /ws-api/v3 HTTP/1.1\r\nHost: localhost\r\n"), "{head}");
        let key = head
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .expect("key header");
        let key: [u8; 24] = key.as_bytes().try_into().expect("24-byte key");
        let accept = if bad_accept {
            *b"AAAAAAAAAAAAAAAAAAAAAAAAAAA="
        } else {
            expected_accept(&key)
        };
        let mut out = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            String::from_utf8_lossy(&accept)
        )
        .into_bytes();
        out.extend_from_slice(&packed);
        tls.write_all(&out).expect("101");
        tls.flush().expect("flush");
        script(&mut tls, &mut buf);
    });
    (port, client_cfg, h)
}

struct Owner {
    poll: Poll,
    events: Events,
    conn: WsConn,
    epoch: Instant,
    texts: Vec<Vec<u8>>,
    /// The session reported `Opened` at least once.
    opened: bool,
}

impl Owner {
    fn new(port: u16, cfg: Arc<ClientConfig>, rx_cap: usize, establish_ms: u64, idle_ms: u64) -> Self {
        let c = WsCfg {
            rx_cap,
            tx_cap: 16 * 1024,
            establish_ns: establish_ms * 1_000_000,
            idle_ns: idle_ms * 1_000_000,
        };
        Self {
            poll: Poll::new().expect("poll"),
            events: Events::with_capacity(16),
            conn: WsConn::new("localhost", port, "/ws-api/v3", cfg, c, TOKEN, 0x5EED).expect("ws"),
            epoch: Instant::now(),
            texts: Vec::new(),
            opened: false,
        }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Drain every complete frame; the first failure ends the drain.
    fn drain(&mut self) -> Result<(), WsErr> {
        loop {
            match self.conn.next_frame() {
                WsNext::Text(s) => self.texts.push(self.conn.payload(s).to_vec()),
                WsNext::Binary(_) => panic!("no binary frames in these tests"),
                WsNext::Idle => return Ok(()),
                WsNext::Failed(e) => return Err(e),
            }
        }
    }

    /// One loop pass: poll, dispatch (draining on news), tick, flush.
    fn pass(&mut self) -> Result<WsProgress, WsErr> {
        self.poll
            .poll(&mut self.events, Some(Duration::from_millis(5)))
            .expect("poll");
        let mut seen = WsProgress::Waiting;
        for ev in self.events.iter() {
            assert_eq!(ev.token(), TOKEN);
            let now = self.epoch.elapsed().as_nanos() as u64;
            match self.conn.on_event(ev, self.poll.registry(), now) {
                WsProgress::Failed(e) => return Err(e),
                WsProgress::Waiting => {}
                p => {
                    self.opened |= p == WsProgress::Opened;
                    seen = p;
                }
            }
        }
        if matches!(seen, WsProgress::Opened | WsProgress::Readable) {
            self.drain()?;
        }
        if let WsProgress::Failed(e) = self.conn.on_tick(self.now()) {
            return Err(e);
        }
        if self.conn.wants_flush() {
            self.conn.flush(self.poll.registry())?;
        }
        Ok(seen)
    }

    fn connect(&mut self) -> Result<(), WsErr> {
        let now = self.now();
        self.conn.connect(self.poll.registry(), now)
    }

    fn open(&mut self) -> Result<(), WsErr> {
        self.connect()?;
        let give_up = Instant::now() + Duration::from_secs(10);
        while !self.conn.is_open() {
            assert!(Instant::now() < give_up, "never opened");
            self.pass()?;
        }
        Ok(())
    }

    /// Turn the loop until `n` texts are in, or the session fails.
    fn until_texts(&mut self, n: usize) -> Result<(), WsErr> {
        let give_up = Instant::now() + Duration::from_secs(10);
        while self.texts.len() < n {
            assert!(Instant::now() < give_up, "only {} of {n} texts", self.texts.len());
            self.pass()?;
        }
        Ok(())
    }

    /// Turn the loop until the session fails.
    fn until_failed(&mut self) -> WsErr {
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < give_up, "the session never failed");
            if let Err(e) = self.pass() {
                return e;
            }
        }
    }
}

#[test]
fn a_session_opens_echoes_a_burst_and_answers_pings() {
    let (port, cfg, h) = serve(false, frame(true, 0x1, b"hello"), |tls, buf| {
        let mut got = Vec::new();
        while got.len() < 3 {
            let (op, p) = read_client_frame(tls, buf);
            assert_eq!(op, 0x1);
            got.push(p);
        }
        assert_eq!(got[0], br#"{"id":"1","method":"session.logon"}"#);
        // Echo all three and a ping in ONE write.
        let mut out = Vec::new();
        for p in &got {
            out.extend(frame(true, 0x1, p));
        }
        out.extend(frame(true, 0x9, b"ping-7f"));
        tls.write_all(&out).expect("echo");
        tls.flush().expect("flush");
        let (op, p) = read_client_frame(tls, buf);
        assert_eq!((op, p.as_slice()), (0xA, &b"ping-7f"[..]), "the pong echoes the ping");
        thread::sleep(Duration::from_millis(100));
    });
    let mut o = Owner::new(port, cfg, 4096, 5_000, 5_000);
    o.open().expect("open");
    o.until_texts(1).expect("the packed frame");
    assert_eq!(o.texts[0], b"hello", "a frame packed with the 101 is kept");
    o.conn
        .queue_text(&[br#"{"id":"1","method":"#, br#""session.logon"}"#])
        .expect("queue");
    o.conn.queue_text(&[b"{\"id\":\"2\"}"]).expect("queue");
    o.conn.queue_text(&[&[b'x'; 300]]).expect("queue");
    o.conn.flush(o.poll.registry()).expect("one write");
    o.until_texts(4).expect("the echoes");
    assert_eq!(o.texts[2], b"{\"id\":\"2\"}");
    assert_eq!(o.texts[3], vec![b'x'; 300]);
    // The pong leaves with the next pass's flush.
    let until = Instant::now() + Duration::from_millis(50);
    while Instant::now() < until {
        o.pass().expect("steady");
    }
    h.join().expect("server assertions");
    assert_eq!((o.conn.dials(), o.conn.fail_streak()), (1, 0));
}

#[test]
fn an_upgrade_with_the_wrong_accept_is_refused() {
    let (port, cfg, h) = serve(true, Vec::new(), |_, _| thread::sleep(Duration::from_millis(200)));
    let mut o = Owner::new(port, cfg, 4096, 5_000, 5_000);
    assert_eq!(o.open(), Err(WsErr::Upgrade));
    assert!(o.conn.is_down());
    assert_eq!(o.conn.fail_streak(), 1);
    h.join().expect("server");
}

#[test]
fn a_fragmented_message_ends_the_session() {
    let (port, cfg, h) = serve(false, Vec::new(), |tls, _| {
        tls.write_all(&frame(false, 0x1, b"first half")).expect("frag");
        tls.flush().expect("flush");
        thread::sleep(Duration::from_millis(200));
    });
    let mut o = Owner::new(port, cfg, 4096, 5_000, 5_000);
    o.connect().expect("connect");
    
    assert_eq!(o.until_failed(), WsErr::Fragmented);
    assert!(o.opened, "the upgrade completed first");
    assert!(o.conn.is_down());
    h.join().expect("server");
}

#[test]
fn a_frame_larger_than_the_window_ends_the_session() {
    let (port, cfg, h) = serve(false, Vec::new(), |tls, _| {
        tls.write_all(&frame(true, 0x1, &[b'L'; 3000])).expect("big");
        tls.flush().expect("flush");
        thread::sleep(Duration::from_millis(200));
    });
    let mut o = Owner::new(port, cfg, 1024, 5_000, 5_000);
    o.connect().expect("connect");
    
    assert_eq!(o.until_failed(), WsErr::TooLarge);
    assert!(o.opened, "the upgrade completed first");
    h.join().expect("server");
}

#[test]
fn a_burst_larger_than_the_window_flows_through_in_order() {
    const N: usize = 60;
    let (port, cfg, h) = serve(false, Vec::new(), |tls, _| {
        let mut out = Vec::new();
        let mut i = 0;
        while i < N {
            let mut p = format!("{{\"seq\":{i:03}}}").into_bytes();
            p.resize(180, b' ');
            out.extend(frame(true, 0x1, &p));
            i += 1;
        }
        tls.write_all(&out).expect("burst");
        tls.flush().expect("flush");
        thread::sleep(Duration::from_millis(300));
    });
    let mut o = Owner::new(port, cfg, 1024, 5_000, 5_000);
    o.open().expect("open");
    o.until_texts(N).expect("every frame");
    let mut i = 0;
    while i < N {
        assert!(
            o.texts[i].starts_with(format!("{{\"seq\":{i:03}}}").as_bytes()),
            "frame {i} out of order"
        );
        i += 1;
    }
    h.join().expect("server");
}

#[test]
fn the_idle_law_ends_a_silent_session() {
    let (port, cfg, h) = serve(false, Vec::new(), |_, _| thread::sleep(Duration::from_millis(600)));
    let mut o = Owner::new(port, cfg, 4096, 5_000, 150);
    o.open().expect("open");
    let t0 = Instant::now();
    assert_eq!(o.until_failed(), WsErr::Idle);
    assert!(t0.elapsed() >= Duration::from_millis(140), "{:?}", t0.elapsed());
    h.join().expect("server");
}

#[test]
fn a_close_frame_ends_the_session() {
    let (port, cfg, h) = serve(false, Vec::new(), |tls, _| {
        tls.write_all(&frame(true, 0x8, &[0x03, 0xE8])).expect("close");
        tls.flush().expect("flush");
        thread::sleep(Duration::from_millis(200));
    });
    let mut o = Owner::new(port, cfg, 4096, 5_000, 5_000);
    o.connect().expect("connect");
    
    assert_eq!(o.until_failed(), WsErr::Closed);
    assert!(o.opened, "the upgrade completed first");
    h.join().expect("server");
}

#[test]
fn a_peer_that_never_speaks_tls_times_out_the_establishment() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let h = thread::spawn(move || {
        let (_sock, _) = listener.accept().expect("accept");
        thread::sleep(Duration::from_millis(500));
    });
    let (cert, _) = certs();
    let mut roots = RootCertStore::empty();
    roots.add(cert).expect("anchor");
    let cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut o = Owner::new(port, cfg, 4096, 200, 5_000);
    let t0 = Instant::now();
    assert_eq!(o.open(), Err(WsErr::Timeout));
    assert!(t0.elapsed() >= Duration::from_millis(190));
    assert_eq!(o.conn.fail_streak(), 1);
    assert!(o.conn.is_down());
    h.join().expect("peer");
}

#[test]
fn frames_before_a_bare_fin_still_drain() {
    let (port, cfg, h) = serve(false, Vec::new(), |tls, _| {
        let mut out = Vec::new();
        let mut i = 0;
        while i < 3 {
            out.extend(frame(true, 0x1, format!("last-{i}").as_bytes()));
            i += 1;
        }
        tls.write_all(&out).expect("frames");
        tls.flush().expect("flush");
        // A FIN with no close_notify.
        tls.sock.shutdown(std::net::Shutdown::Write).expect("fin");
        thread::sleep(Duration::from_millis(200));
    });
    let mut o = Owner::new(port, cfg, 4096, 5_000, 5_000);
    o.connect().expect("connect");
    assert_eq!(o.until_failed(), WsErr::Disconnected);
    assert_eq!(o.texts, vec![b"last-0".to_vec(), b"last-1".to_vec(), b"last-2".to_vec()]);
    h.join().expect("server");
}
