// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The loopback server: one mio poll, a handful of clients, nothing shared.
//! A COLD process (it may allocate; buffers are still sized once per
//! client). T-L1: it holds no venue key and opens no order socket.
//!
//! Routes: `GET /` and the bundle files (no session needed: the bundle holds
//! no secret, T-L13, and its login form is how a session starts);
//! `POST /login` (the one-time token in the body, exact Origin); `GET /ws`
//! (session cookie, exact Origin, RFC 6455) — the only door to data and to
//! order entry. Every answer carries the plan's CSP (§3).

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use term_proto::ctl::{Beat, Control, CtlOp, CH_ALL};
use term_proto::{Frame, Kind};

use crate::auth::{Auth, COOKIE};
use crate::bundle::Bundle;
use crate::http::{has_token, parse, Parse, Req};
use crate::source::{frames, route, Source};
use crate::spool::Recorder;
use crate::ws;

pub const MAX_CLIENTS: usize = 8;

fn wall_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}
const LISTENER: Token = Token(usize::MAX);
const RX_CAP: usize = 192 * 1024;
const TX_CAP: usize = 4 << 20;
/// A client whose unsent bytes pass this is moved to snapshot mode.
const TX_HIGH: usize = 2 << 20;
const MAX_INSTR: usize = 64;
/// Three missed echoes is "UI lost" (§4.6, R-T15).
const MISSED_ECHOES: u64 = 3;

pub struct Config {
    pub port: u16,
    pub token: crate::auth::Secret,
    pub bundle: Bundle,
    pub record: Option<std::path::PathBuf>,
    pub tick: Duration,
    /// Replay and `--no-door` sessions never accept intents.
    pub order_entry: bool,
    /// The engine's TERM door; intents go nowhere without it.
    pub door: Option<crate::door::DoorClient>,
    /// The engine's `/state` (EngineStatus and the door's epoch).
    pub state: Option<crate::state::StatePoller>,
}

#[derive(Default)]
struct WsState {
    subs: Vec<u32>,
    visible: bool,
    needs_snapshot: Vec<bool>,
    beat_n: u64,
    echoed: u64,
    trading: bool,
}

enum State {
    Http,
    Ws(WsState),
}

struct Client {
    sock: TcpStream,
    rx: Vec<u8>,
    tx: Vec<u8>,
    /// Streaming a large bundle file: (file index, offset).
    body: Option<(usize, usize)>,
    state: State,
    close_after_flush: bool,
    dead: bool,
}

/// What happened, for the log and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Listening(u16),
    LoggedIn,
    LoginRefused,
    Upgraded,
    Refused(&'static str),
    Control(CtlOp),
    Intent { nonce: u64, accepted: bool, why: &'static str },
    UiLost,
    Closed,
}

pub struct Server {
    poll: Poll,
    listener: TcpListener,
    clients: Vec<Option<Client>>,
    auth: Auth,
    cfg: Config,
    port: u16,
    csp: String,
    rec: Option<Recorder>,
    start: Instant,
    md: Vec<u8>,
    one: Vec<u8>,
    pub log: Vec<Event>,
}

impl Server {
    pub fn bind(cfg: Config) -> std::io::Result<Server> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.port));
        let mut listener = TcpListener::bind(addr)?;
        let port = listener.local_addr()?.port();
        let poll = Poll::new()?;
        poll.registry().register(&mut listener, LISTENER, Interest::READABLE)?;
        let csp = format!(
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self' ws://127.0.0.1:{port}; img-src 'self'; style-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
        );
        let rec = match &cfg.record {
            Some(p) => Some(Recorder::create(p)?),
            None => None,
        };
        let auth = Auth::new(cfg.token, port);
        let clients = (0..MAX_CLIENTS).map(|_| None).collect();
        Ok(Server { poll, listener, clients, auth, cfg, port, csp, rec, start: Instant::now(), md: Vec::with_capacity(1 << 20), one: Vec::with_capacity(1 << 20), log: vec![Event::Listening(port)] })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn note(&mut self, e: Event) {
        eprintln!("terminal: {e:?}");
        self.log.push(e);
    }

    /// Run until `stop` returns true. The source is ticked every `cfg.tick`.
    pub fn run<S: Source>(&mut self, src: &mut S, mut stop: impl FnMut(&Server) -> bool) -> std::io::Result<()> {
        let mut events = Events::with_capacity(64);
        let mut next_tick = Instant::now();
        let mut next_beat = Instant::now() + Duration::from_secs(1);
        while !stop(self) {
            let now = Instant::now();
            let wait = next_tick.min(next_beat).saturating_duration_since(now).min(Duration::from_millis(50));
            match self.poll.poll(&mut events, Some(wait)) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            for ev in events.iter() {
                if ev.token() == LISTENER {
                    self.accept()?;
                } else {
                    let i = ev.token().0;
                    self.on_readable(i, src);
                    self.flush(i);
                }
            }
            let now = Instant::now();
            if now >= next_tick {
                next_tick += self.cfg.tick;
                if next_tick < now {
                    next_tick = now + self.cfg.tick;
                }
                self.md.clear();
                let t_ns = self.start.elapsed().as_nanos() as u64;
                src.tick(t_ns, &mut self.md);
                self.fan_out(t_ns);
                self.serve_snapshots(src);
            }
            if now >= next_beat {
                next_beat += Duration::from_secs(1);
                self.heartbeat();
                // The member's watchdog hears the UI through these beats;
                // with no UI connected they stop and it cancels (R-T15).
                let ui = self.clients.iter().flatten().any(|c| matches!(c.state, State::Ws(_)));
                if let (true, Some(d)) = (ui, self.cfg.door.as_mut()) {
                    if d.beat(wall_ns()).is_err() {
                        self.cfg.door = None;
                        self.note(Event::Refused("door lost"));
                    }
                }
            }
            self.door_events();
            self.engine_state();
            for i in 0..MAX_CLIENTS {
                self.flush(i);
                if self.clients[i].as_ref().is_some_and(|c| c.dead) {
                    if let Some(mut c) = self.clients[i].take() {
                        let _ = self.poll.registry().deregister(&mut c.sock);
                    }
                    self.note(Event::Closed);
                }
            }
        }
        if let Some(r) = self.rec.as_mut() {
            r.flush()?;
        }
        Ok(())
    }

    fn accept(&mut self) -> std::io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((mut sock, _)) => {
                    let Some(slot) = self.clients.iter().position(Option::is_none) else {
                        drop(sock); // full: refuse
                        continue;
                    };
                    self.poll.registry().register(&mut sock, Token(slot), Interest::READABLE | Interest::WRITABLE)?;
                    self.clients[slot] = Some(Client { sock, rx: Vec::with_capacity(RX_CAP), tx: Vec::with_capacity(1 << 16), body: None, state: State::Http, close_after_flush: false, dead: false });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    fn on_readable<S: Source>(&mut self, i: usize, src: &mut S) {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let Some(c) = self.clients[i].as_mut() else { return };
            match c.sock.read(&mut buf) {
                Ok(0) => {
                    c.dead = true;
                    break;
                }
                Ok(n) => {
                    if c.rx.len() + n > RX_CAP {
                        c.dead = true;
                        break;
                    }
                    c.rx.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    c.dead = true;
                    break;
                }
            }
        }
        self.process(i, src);
    }

    fn process<S: Source>(&mut self, i: usize, src: &mut S) {
        loop {
            let Some(c) = self.clients[i].as_mut() else { return };
            if c.dead || c.close_after_flush {
                return;
            }
            match c.state {
                State::Http => {
                    let mut rx = std::mem::take(&mut c.rx);
                    let used = match parse(&rx) {
                        Parse::Incomplete => {
                            if let Some(c) = self.clients[i].as_mut() {
                                c.rx = rx;
                            }
                            return;
                        }
                        Parse::Bad => {
                            self.reply(i, 400, "text/plain", b"bad request", true);
                            None
                        }
                        Parse::Done(req) => {
                            let body_len = req.content_length.unwrap_or(0);
                            if rx.len() < req.head_len + body_len {
                                if let Some(c) = self.clients[i].as_mut() {
                                c.rx = rx;
                            }
                                return;
                            }
                            let body = rx[req.head_len..req.head_len + body_len].to_vec();
                            self.http(i, &req, &body, src);
                            Some(req.head_len + body_len)
                        }
                    };
                    if let Some(c) = self.clients[i].as_mut() {
                        if let Some(u) = used {
                            rx.drain(..u);
                        }
                        c.rx = rx;
                    }
                }
                State::Ws(_) => {
                    let mut rx = std::mem::take(&mut c.rx);
                    let r = ws::read(&mut rx);
                    let used = match r {
                        ws::In::More => {
                            if let Some(c) = self.clients[i].as_mut() {
                                c.rx = rx;
                            }
                            return;
                        }
                        ws::In::Bad | ws::In::Close => {
                            self.drop_client(i, "ws protocol");
                            return;
                        }
                        ws::In::Skip { used } => used,
                        ws::In::Ping { start, end, used } => {
                            let p = rx[start..end].to_vec();
                            if let Some(c) = self.clients[i].as_mut() {
                                ws::write_frame(0xA, &p, &mut c.tx);
                            }
                            used
                        }
                        ws::In::Binary { start, end, used } => {
                            let msg = rx[start..end].to_vec();
                            self.ui_message(i, &msg, src);
                            used
                        }
                    };
                    if let Some(c) = self.clients[i].as_mut() {
                        rx.drain(..used);
                        c.rx = rx;
                    }
                }
            }
        }
    }

    fn drop_client(&mut self, i: usize, why: &'static str) {
        self.note(Event::Refused(why));
        if let Some(c) = self.clients[i].as_mut() {
            c.dead = true;
        }
    }

    fn reply(&mut self, i: usize, status: u16, ctype: &str, body: &[u8], close: bool) {
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            421 => "Misdirected Request",
            _ => "Service Unavailable",
        };
        let head = self.head(status, reason, ctype, body.len(), None);
        if let Some(c) = self.clients[i].as_mut() {
            c.tx.extend_from_slice(head.as_bytes());
            c.tx.extend_from_slice(body);
            c.close_after_flush |= close;
        }
    }

    fn head(&self, status: u16, reason: &str, ctype: &str, len: usize, cookie: Option<&[u8]>) -> String {
        let mut h = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nContent-Security-Policy: {}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nCache-Control: no-store\r\n",
            self.csp
        );
        if let Some(c) = cookie {
            h.push_str(&format!("Set-Cookie: {}={}; HttpOnly; SameSite=Strict; Path=/\r\n", core::str::from_utf8(COOKIE).unwrap_or("term_s"), core::str::from_utf8(c).unwrap_or("")));
        }
        h.push_str("\r\n");
        h
    }

    fn http<S: Source>(&mut self, i: usize, req: &Req<'_>, body: &[u8], src: &mut S) {
        if !self.auth.host_ok(req.host) {
            self.note(Event::Refused("host"));
            return self.reply(i, 421, "text/plain", b"wrong host", true);
        }
        match (req.method, req.path) {
            (b"POST", b"/login") => {
                if !self.auth.origin_ok(req.origin) {
                    self.note(Event::LoginRefused);
                    return self.reply(i, 403, "text/plain", b"origin", true);
                }
                match self.auth.login(body) {
                    Some(s) => {
                        self.note(Event::LoggedIn);
                        let h = self.head(204, "No Content", "text/plain", 0, Some(&s));
                        if let Some(c) = self.clients[i].as_mut() {
                            c.tx.extend_from_slice(h.as_bytes());
                        }
                    }
                    None => {
                        self.note(Event::LoginRefused);
                        self.reply(i, 401, "text/plain", b"token", true);
                    }
                }
            }
            (b"GET", b"/ws") => {
                let up = req.upgrade.is_some_and(|u| u.eq_ignore_ascii_case(b"websocket")) && req.connection.is_some_and(|c| has_token(c, b"upgrade")) && req.ws_version == Some(&b"13"[..]);
                if !up {
                    return self.reply(i, 400, "text/plain", b"upgrade", true);
                }
                if !self.auth.origin_ok(req.origin) {
                    self.note(Event::Refused("ws origin"));
                    return self.reply(i, 403, "text/plain", b"origin", true);
                }
                if !self.auth.session_ok(req.cookie) {
                    self.note(Event::Refused("ws session"));
                    return self.reply(i, 401, "text/plain", b"login", true);
                }
                let Some(c) = self.clients[i].as_mut() else { return };
                if !ws::accept_response(req.ws_key.unwrap_or_default(), &mut c.tx) {
                    return self.reply(i, 400, "text/plain", b"key", true);
                }
                c.tx.reserve(TX_CAP.saturating_sub(c.tx.capacity()));
                c.state = State::Ws(WsState { subs: vec![0; MAX_INSTR], visible: true, needs_snapshot: vec![false; MAX_INSTR], ..WsState::default() });
                self.note(Event::Upgraded);
                self.one.clear();
                src.hello(&mut self.one);
                // `/state`'s status overrides a source's stub (later wins).
                if let Some(st) = self.cfg.state.as_ref() {
                    let mut b = [0u8; 256];
                    if let Ok(n) = crate::state::engine_status(st.last.as_ref()).encode(0, 0, &mut b) {
                        self.one.extend_from_slice(&b[..n]);
                    }
                }
                self.send_frames(i);
            }
            (b"GET", path) => {
                match self.cfg.bundle.files.iter().position(|f| self.cfg.bundle.get(path).is_some_and(|g| std::ptr::eq(g, f))) {
                    Some(k) => {
                        let f = &self.cfg.bundle.files[k];
                        let h = self.head(200, "OK", f.ctype, f.body.len(), None);
                        if let Some(c) = self.clients[i].as_mut() {
                            c.tx.extend_from_slice(h.as_bytes());
                            c.body = Some((k, 0));
                        }
                    }
                    None if self.cfg.bundle.files.is_empty() => self.reply(i, 503, "text/plain", b"no bundle: run terminal-ui/scripts/terminal-ui-build.sh and pass --bundle terminal-ui/dist", false),
                    None => self.reply(i, 404, "text/plain", b"not found", false),
                }
            }
            _ => self.reply(i, 405, "text/plain", b"method", true),
        }
    }

    /// Send every frame in `self.one` to client `i`, validated.
    fn send_frames(&mut self, i: usize) {
        let Some(c) = self.clients[i].as_mut() else { return };
        for f in frames(&self.one).flatten() {
            ws::write_binary(f, &mut c.tx);
        }
    }

    fn ui_message<S: Source>(&mut self, i: usize, msg: &[u8], src: &mut S) {
        let (f, used) = match term_proto::decode_from_ui(msg) {
            Ok(x) => x,
            Err(_) => return self.drop_client(i, "bad frame from ui"),
        };
        if used != msg.len() {
            return self.drop_client(i, "trailing bytes");
        }
        self.handle(i, f, src);
    }

    fn handle<S: Source>(&mut self, i: usize, f: Frame<'_>, src: &mut S) {
        let order_entry = self.cfg.order_entry;
        let Some(Client { state: State::Ws(w), .. }) = self.clients[i].as_mut() else { return };
        match f.hdr.kind {
            Kind::Echo => {
                if let Ok(b) = Beat::parse(f.body) {
                    if b.n <= w.beat_n {
                        w.echoed = w.echoed.max(b.n);
                    }
                }
            }
            Kind::Control => {
                let Ok(c) = Control::parse(f.body) else { return };
                match c.op {
                    CtlOp::Hidden | CtlOp::Frozen => w.visible = false,
                    CtlOp::Visible | CtlOp::Resumed | CtlOp::Stale => {
                        w.visible = true;
                        for k in 0..MAX_INSTR {
                            w.needs_snapshot[k] = w.subs[k] != 0;
                        }
                    }
                    CtlOp::Subscribe if (c.a as usize) < MAX_INSTR => {
                        let fresh = w.subs[c.a as usize] == 0;
                        w.subs[c.a as usize] |= c.b & CH_ALL;
                        if fresh {
                            self.one.clear();
                            src.snapshot(c.a, &mut self.one);
                            self.send_frames(i);
                        }
                    }
                    CtlOp::Unsubscribe if (c.a as usize) < MAX_INSTR => w.subs[c.a as usize] &= !c.b,
                    CtlOp::TradingEnable => w.trading = order_entry,
                    CtlOp::TradingDisable => w.trading = false,
                    _ => {}
                }
                self.note(Event::Control(c.op));
            }
            Kind::Intent => {
                let Ok(intent) = term_proto::exec::Intent::parse(f.body) else { return };
                let trading = w.trading;
                use term_proto::exec::*;
                let (accepted, why, code) = if !order_entry {
                    (false, "order entry disabled (replay / no door)", ACK_ORDER_ENTRY_OFF)
                } else if !trading {
                    (false, "trading is off for this session", ACK_TRADING_OFF)
                } else {
                    match self.cfg.door.as_mut() {
                        None => (false, "no door configured", ACK_NO_DOOR),
                        Some(d) => match d.intent(&intent, wall_ns()) {
                            Ok(()) => (true, "sent to the door", 0),
                            Err(e) if e.starts_with("no route") => (false, e, ACK_NO_ROUTE),
                            Err(e) => (false, e, ACK_DOOR_LOST),
                        },
                    }
                };
                // The server's own refusals are answered at once; the
                // engine's verdict on a sent intent comes back via the door.
                if !accepted {
                    let mut b = [0u8; 64];
                    let ack = IntentAck { nonce: intent.nonce, slot: intent.slot, accepted: false, source: AckSource::Server, reason: code };
                    if let (Ok(n), Some(c)) = (ack.encode(0, 0, &mut b), self.clients[i].as_mut()) {
                        ws::write_binary(&b[..n], &mut c.tx);
                    }
                }
                self.note(Event::Intent { nonce: intent.nonce, accepted, why });
            }
            _ => {} // Layout / Drawing / KeyMap / LiveConfirm: stored in later milestones.
        }
    }

    /// `/state` changed → a fresh EngineStatus to every UI; a new engine
    /// boot → the door re-keyed to its epoch.
    fn engine_state(&mut self) {
        let Some(st) = self.cfg.state.as_mut() else { return };
        if !st.poll() {
            return;
        }
        let status = crate::state::engine_status(st.last.as_ref());
        if let (Some(d), true) = (self.cfg.door.as_mut(), status.engine_up) {
            if status.boot_epoch != 0 && status.boot_epoch != d.epoch() && d.rekey(status.boot_epoch).is_err() {
                self.cfg.door = None;
                self.note(Event::Refused("door lost on engine restart"));
            }
        }
        let mut b = [0u8; 256];
        let Ok(n) = status.encode(0, 0, &mut b) else { return };
        self.one.clear();
        self.one.extend_from_slice(&b[..n]);
        for i in 0..MAX_CLIENTS {
            if self.clients[i].as_ref().is_some_and(|c| matches!(c.state, State::Ws(_))) {
                self.send_frames(i);
            }
        }
    }

    /// Door events → views for every connected UI.
    fn door_events(&mut self) {
        let Some(d) = self.cfg.door.as_mut() else { return };
        let mut evs = Vec::new();
        if d.poll(|e| evs.push(e)).is_err() {
            self.cfg.door = None;
            self.note(Event::Refused("door lost"));
            return;
        }
        if evs.is_empty() {
            return;
        }
        let t = self.start.elapsed().as_nanos() as u64;
        self.one.clear();
        for e in &evs {
            d.views(e, t, &mut self.one);
        }
        for i in 0..MAX_CLIENTS {
            if self.clients[i].as_ref().is_some_and(|c| matches!(c.state, State::Ws(_))) {
                self.send_frames(i);
            }
        }
    }

    fn fan_out(&mut self, t_ns: u64) {
        let md = std::mem::take(&mut self.md);
        for f in frames(&md) {
            let Ok(f) = f else {
                eprintln!("terminal: source emitted an invalid frame; dropping the batch");
                break;
            };
            if let Some(r) = self.rec.as_mut() {
                let _ = r.record(t_ns, f);
            }
            let Some((ch, instr)) = route(f) else {
                // InstrDef / EngineStatus updates go to every client.
                for c in self.clients.iter_mut().flatten() {
                    if matches!(c.state, State::Ws(_)) {
                        ws::write_binary(f, &mut c.tx);
                    }
                }
                continue;
            };
            for c in self.clients.iter_mut().flatten() {
                let State::Ws(w) = &mut c.state else { continue };
                let k = instr as usize;
                if k >= MAX_INSTR || w.subs[k] & ch == 0 || !w.visible || w.needs_snapshot[k] {
                    continue;
                }
                if c.tx.len() > TX_HIGH {
                    // Backpressure: stop deltas, resend a snapshot later.
                    w.needs_snapshot[k] = true;
                    continue;
                }
                ws::write_binary(f, &mut c.tx);
            }
        }
        self.md = md;
    }

    fn serve_snapshots<S: Source>(&mut self, src: &mut S) {
        for i in 0..MAX_CLIENTS {
            let Some(Client { state: State::Ws(w), tx, .. }) = self.clients[i].as_mut() else { continue };
            if !w.visible || tx.len() > TX_HIGH / 2 {
                continue;
            }
            for k in 0..MAX_INSTR {
                if w.needs_snapshot[k] {
                    w.needs_snapshot[k] = false;
                    self.one.clear();
                    src.snapshot(k as u16, &mut self.one);
                    self.send_frames(i);
                    break; // one per tick
                }
            }
        }
    }

    fn heartbeat(&mut self) {
        let t = self.start.elapsed().as_nanos() as u64;
        let mut lost = Vec::new();
        for (i, c) in self.clients.iter_mut().enumerate() {
            let Some(Client { state: State::Ws(w), tx, .. }) = c else { continue };
            if w.beat_n >= MISSED_ECHOES && w.beat_n - w.echoed >= MISSED_ECHOES {
                lost.push(i);
                continue;
            }
            w.beat_n += 1;
            let mut b = [0u8; 64];
            if let Ok(n) = (Beat { n: w.beat_n, server_ns: t }).encode(false, 0, t, &mut b) {
                ws::write_binary(&b[..n], tx);
            }
        }
        for i in lost {
            // R-T15: a lost UI's algos stop and its resting orders are
            // cancelled; the member does that once the door exists (M5).
            self.note(Event::UiLost);
            if let Some(c) = self.clients[i].as_mut() {
                c.dead = true;
            }
        }
    }

    fn flush(&mut self, i: usize) {
        let bundle = &self.cfg.bundle;
        let Some(c) = self.clients[i].as_mut() else { return };
        loop {
            if c.tx.is_empty() {
                if let Some((k, off)) = c.body {
                    let b = &bundle.files[k].body;
                    let end = (off + (256 << 10)).min(b.len());
                    c.tx.extend_from_slice(&b[off..end]);
                    c.body = if end == b.len() { None } else { Some((k, end)) };
                } else {
                    if c.close_after_flush {
                        c.dead = true;
                    }
                    return;
                }
            }
            match c.sock.write(&c.tx) {
                Ok(0) => {
                    c.dead = true;
                    return;
                }
                Ok(n) => {
                    c.tx.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(_) => {
                    c.dead = true;
                    return;
                }
            }
        }
    }
}
