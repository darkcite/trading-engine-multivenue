// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! End to end over real loopback sockets: login, Origin and session checks,
//! the upgrade, hello frames, subscribe → snapshot → deltas, intents refused
//! without a door, and UI-lost after three missed echoes.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term_proto::ctl::{Beat, Control, CtlOp, CH_ALL};
use term_proto::exec::{Intent, Op};
use term_proto::Kind;
use terminal::bundle::Bundle;
use terminal::server::{Config, Event, Server};

struct Harness {
    port: u16,
    token: String,
    log: Arc<Mutex<Vec<Event>>>,
    stop: Arc<Mutex<bool>>,
}

fn boot(order_entry: bool) -> Harness {
    let token = terminal::auth::random_secret().unwrap();
    let cfg = Config { port: 0, token, bundle: Bundle::none(), record: None, tick: Duration::from_millis(20), order_entry, door: None, state: None };
    let mut srv = Server::bind(cfg).unwrap();
    let port = srv.port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(Mutex::new(false));
    let (l2, s2) = (log.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut src = terminal::synth::Synthetic::new(1);
        srv.run(&mut src, |s| {
            *l2.lock().unwrap() = s.log.clone();
            *s2.lock().unwrap()
        })
        .unwrap();
    });
    Harness { port, token: String::from_utf8(token.to_vec()).unwrap(), log, stop }
}

impl Drop for Harness {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
    }
}

fn request(port: u16, raw: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn login(h: &Harness) -> String {
    let r = request(h.port, &format!("POST /login HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nContent-Length: 32\r\n\r\n{}", h.token, p = h.port));
    assert!(r.starts_with("HTTP/1.1 204"), "{r}");
    assert!(r.contains("HttpOnly; SameSite=Strict"), "{r}");
    let c = r.split("Set-Cookie: ").nth(1).unwrap();
    c.split(';').next().unwrap().to_owned()
}

fn masked(payload: &[u8]) -> Vec<u8> {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut f = vec![0x82];
    if payload.len() < 126 {
        f.push(0x80 | payload.len() as u8);
    } else {
        f.push(0x80 | 126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

struct Ws {
    s: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    fn open(h: &Harness, cookie: &str) -> Ws {
        let mut s = TcpStream::connect(("127.0.0.1", h.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let req = format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nCookie: {cookie}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n", p = h.port);
        s.write_all(req.as_bytes()).unwrap();
        let mut w = Ws { s, buf: Vec::new() };
        let t0 = Instant::now();
        while !w.buf.windows(4).any(|x| x == b"\r\n\r\n") && t0.elapsed() < Duration::from_secs(3) {
            w.fill();
        }
        let end = w.buf.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&w.buf[..end]).into_owned();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        w.buf.drain(..end);
        w
    }
    fn fill(&mut self) {
        let mut b = [0u8; 65536];
        if let Ok(n) = self.s.read(&mut b) {
            self.buf.extend_from_slice(&b[..n]);
        }
    }
    /// Next server frame's payload (server frames are never masked).
    fn next(&mut self, within: Duration) -> Option<Vec<u8>> {
        let t0 = Instant::now();
        loop {
            if self.buf.len() >= 2 {
                assert_eq!(self.buf[1] & 0x80, 0, "server frames must not be masked");
                let (len, hl) = match self.buf[1] & 0x7f {
                    126 if self.buf.len() >= 4 => (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4),
                    127 if self.buf.len() >= 10 => (u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize, 10),
                    n if n < 126 => (n as usize, 2),
                    _ => (usize::MAX, 0),
                };
                if len != usize::MAX && self.buf.len() >= hl + len {
                    let p = self.buf[hl..hl + len].to_vec();
                    self.buf.drain(..hl + len);
                    return Some(p);
                }
            }
            if t0.elapsed() > within {
                return None;
            }
            self.fill();
        }
    }
    fn send(&mut self, frame: &[u8]) {
        self.s.write_all(&masked(frame)).unwrap();
    }
    fn kinds(&mut self, within: Duration) -> Vec<(Kind, Vec<u8>)> {
        let mut v = Vec::new();
        let t0 = Instant::now();
        while t0.elapsed() < within {
            if let Some(p) = self.next(Duration::from_millis(50)) {
                let (f, _) = term_proto::decode(&p).expect("every server frame decodes");
                v.push((f.hdr.kind, p));
            }
        }
        v
    }
}

#[test]
fn refusals_before_a_session() {
    let h = boot(true);
    let p = h.port;
    assert!(request(p, &format!("GET /ws HTTP/1.1\r\nHost: localhost:{p}\r\n\r\n")).starts_with("HTTP/1.1 421"));
    let r = request(p, &format!("POST /login HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: null\r\nContent-Length: 32\r\n\r\n{}", h.token));
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    let r = request(p, &format!("POST /login HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nContent-Length: 32\r\n\r\n{}", "0".repeat(32)));
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    let r = request(p, &format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"));
    assert!(r.starts_with("HTTP/1.1 401"), "no cookie: {r}");
    let r = request(p, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n"));
    assert!(r.starts_with("HTTP/1.1 503") && r.contains("Content-Security-Policy: default-src 'none'"), "{r}");
    let cookie = login(&h);
    let r = request(p, &format!("POST /login HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nContent-Length: 32\r\n\r\n{}", h.token));
    assert!(r.starts_with("HTTP/1.1 401"), "the token is one-time: {r}");
    let r = request(p, &format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://evil.example\r\nCookie: {cookie}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"));
    assert!(r.starts_with("HTTP/1.1 403"), "foreign origin: {r}");
}

#[test]
fn session_streams_and_refuses_intents_without_a_door() {
    let h = boot(true);
    let cookie = login(&h);
    let mut ws = Ws::open(&h, &cookie);
    let hello = ws.kinds(Duration::from_millis(300));
    assert_eq!(hello.iter().filter(|(k, _)| *k == Kind::InstrDef).count(), 2);
    assert!(hello.iter().any(|(k, _)| *k == Kind::EngineStatus));
    let mut b = [0u8; 256];
    let n = Control { op: CtlOp::Subscribe, a: 0, b: CH_ALL, c: 0 }.encode(1, 0, &mut b).unwrap();
    ws.send(&b[..n]);
    let got = ws.kinds(Duration::from_millis(1500));
    let count = |k: Kind| got.iter().filter(|(x, _)| *x == k).count();
    assert_eq!(count(Kind::BookSnap), 2, "a snapshot per side first");
    assert!(count(Kind::Bar) >= 300, "bar history");
    assert!(count(Kind::BookDelta) > 5 && count(Kind::HeatCol) > 5 && count(Kind::FlowPoint) > 5);
    assert!(got.iter().all(|(_, p)| term_proto::md::BookDelta::parse(&p[term_proto::HDR_LEN..]).map(|d| d.instr == 0).unwrap_or(true)), "only instrument 0");
    // An intent is refused: trading is off at login.
    let mut i = Intent::blank(42, 10, 0, Op::CancelAll);
    i.nonce = 42;
    let n = i.encode(2, 0, &mut b).unwrap();
    ws.send(&b[..n]);
    // The refusal comes back as an IntentAck from the server.
    let acks = ws.kinds(Duration::from_millis(300));
    let ack = acks.iter().find(|(k, _)| *k == Kind::IntentAck).expect("an ack");
    let a = term_proto::exec::IntentAck::parse(&ack.1[term_proto::HDR_LEN..]).unwrap();
    assert_eq!((a.nonce, a.accepted, a.reason), (42, false, term_proto::exec::ACK_TRADING_OFF));
    // Hidden: deltas stop.
    let n = Control::op(CtlOp::Hidden).encode(3, 0, &mut b).unwrap();
    ws.send(&b[..n]);
    std::thread::sleep(Duration::from_millis(150));
    let _ = ws.kinds(Duration::from_millis(100));
    let quiet = ws.kinds(Duration::from_millis(400));
    assert!(quiet.iter().all(|(k, _)| *k == Kind::Heartbeat), "hidden ⇒ snapshot mode: {:?}", quiet.iter().map(|x| x.0).collect::<Vec<_>>());
    std::thread::sleep(Duration::from_millis(200));
    let log = h.log.lock().unwrap().clone();
    assert!(log.contains(&Event::Intent { nonce: 42, accepted: false, why: "trading is off for this session" }), "{log:?}");
    assert!(log.contains(&Event::Control(CtlOp::Hidden)));
}

#[test]
fn ui_lost_after_three_missed_echoes_but_not_while_echoing() {
    let h = boot(true);
    let cookie = login(&h);
    let mut ws = Ws::open(&h, &cookie);
    // Echo every heartbeat for ~3.5 s: no loss.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(3500) {
        if let Some(p) = ws.next(Duration::from_millis(100)) {
            let (f, _) = term_proto::decode(&p).unwrap();
            if f.hdr.kind == Kind::Heartbeat {
                let b = Beat::parse(f.body).unwrap();
                let mut out = [0u8; 64];
                let n = b.encode(true, 0, 0, &mut out).unwrap();
                ws.send(&out[..n]);
            }
        }
    }
    assert!(!h.log.lock().unwrap().contains(&Event::UiLost));
    // Stop echoing: lost within ~4 s.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(5000) && !h.log.lock().unwrap().contains(&Event::UiLost) {
        let _ = ws.next(Duration::from_millis(100));
    }
    assert!(h.log.lock().unwrap().contains(&Event::UiLost));
}

#[test]
fn record_then_replay_round_trips() {
    let dir = std::env::temp_dir().join(format!("term-spool-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("md.spool");
    {
        let mut rec = terminal::spool::Recorder::create(&path).unwrap();
        let mut s = terminal::synth::Synthetic::new(5);
        let mut out = Vec::new();
        s.hello(&mut out);
        for t in 0..50u64 {
            s.tick(t * 1_000_000, &mut out);
        }
        for (k, f) in terminal::source::frames(&out).enumerate() {
            rec.record(k as u64, f.unwrap()).unwrap();
        }
        rec.flush().unwrap();
    }
    use terminal::source::Source;
    let mut r = terminal::replay::Replay::open(&path).unwrap();
    let mut out = Vec::new();
    r.tick(u64::MAX, &mut out);
    assert!(r.done);
    let n = terminal::source::frames(&out).filter(|f| f.is_ok()).count();
    assert!(n > 100);
    let mut hello = Vec::new();
    r.hello(&mut hello);
    assert_eq!(terminal::source::frames(&hello).count(), 3, "two InstrDefs and the status kept for late joiners");
    // Another wire version is refused.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[9] = term_proto::VER.wrapping_add(1);
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(terminal::replay::Replay::open(&path), Err(terminal::spool::SpoolErr::Version(_))));
    let _ = std::fs::remove_dir_all(&dir);
}
