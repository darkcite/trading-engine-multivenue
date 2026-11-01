// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The terminal's side of the TERM door (TERM plan §4.2): intents become
//! sealed `TermCmd` frames on the engine's UDS; `TermEvt` frames come back as
//! term-proto views for the UI. Every money check stays in the member and
//! E6 (T-L13); this side only translates. T-L2: tests use an in-process door
//! with a per-run key, never the live door.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

use ingress_term::{open_frame, seal_frame, AlgoParams, CmdKind, EvtKind, TermCmd, TermEvt, ALGO_CHASE, ALGO_ICEBERG, ALGO_TWAP, FRAME_LEN, OKIND_IOC, OKIND_LIMIT, REC_LEN};
use term_proto::exec::{Algo, AckSource, FillView, Intent, IntentAck, Op, OrderKind, OrderStatus, OrderView, PositionView};
use term_proto::Side;

/// Where an allow-listed instrument trades (R-T9): its TERM slot and the
/// engine's `SymbolId`. Configured by the operator, never by the UI.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub instr: u16,
    pub slot: u8,
    pub sym: u32,
}

impl Route {
    /// `instr:slot:sym`, e.g. `0:12:3`.
    pub fn parse(s: &str) -> Option<Route> {
        let mut it = s.split(':');
        let r = Route { instr: it.next()?.parse().ok()?, slot: it.next()?.parse().ok()?, sym: it.next()?.parse().ok()? };
        (it.next().is_none() && ingress_term::is_term_slot(r.slot)).then_some(r)
    }
}

/// Read `TERM_DOOR_HMAC_KEY` from `term.env`, refusing a file anyone but
/// the owner can read (R-T13: only the engine wrapper and the terminal).
pub fn read_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must be 0600 (R-T13)", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let hex = text.lines().map(str::trim).find_map(|l| l.strip_prefix("TERM_DOOR_HMAC_KEY=")).ok_or("no TERM_DOOR_HMAC_KEY")?.trim().trim_matches('"');
    if hex.len() != 64 {
        return Err("TERM_DOOR_HMAC_KEY must be 64 hex characters".into());
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| "TERM_DOOR_HMAC_KEY is not hex")?;
    }
    Ok(k)
}

/// A client order id as the UI sees it: the engine's u64 in bytes 0..8.
pub fn cloid_of(oid: u64) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[..8].copy_from_slice(&oid.to_le_bytes());
    c
}
pub fn oid_of(c: &[u8; 16]) -> u64 {
    u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
}

/// Translate an intent; `None` when the op is not supported over the door
/// yet (algos are owed) or the instrument has no route.
pub fn to_cmd(i: &Intent, routes: &[Route], epoch: u64, seq: u32, wall_ns: u64) -> Option<TermCmd> {
    let mut c = TermCmd { slot: i.slot, nonce: i.nonce, boot_epoch: epoch, seq, sent_wall_ns: wall_ns, ..TermCmd::default() };
    let route = routes.iter().find(|r| r.instr == i.instr && r.slot == i.slot);
    match i.op {
        Op::Place => {
            let r = route?;
            c.kind = CmdKind::Place as u8;
            c.sym = r.sym;
            c.side = if i.side()? == Side::Buy { 0 } else { 1 };
            c.okind = if i.kind == OrderKind::Ioc as u8 { OKIND_IOC } else { OKIND_LIMIT };
            c.flags = i.flags; // same bit values on both sides
            c.px = i.px;
            c.qty = i.qty;
        }
        Op::Modify => {
            c.kind = CmdKind::Modify as u8;
            c.target_oid = oid_of(&i.target);
            c.px = i.px;
            c.qty = i.qty;
        }
        Op::Cancel => {
            c.kind = CmdKind::Cancel as u8;
            c.sym = route?.sym;
            c.target_oid = oid_of(&i.target);
        }
        Op::CancelAll | Op::Flatten => {
            c.kind = if i.op == Op::CancelAll { CmdKind::CancelAll as u8 } else { CmdKind::Flatten as u8 };
            c.sym = route?.sym;
        }
        Op::Halt => c.kind = CmdKind::Halt as u8,
        Op::AlgoStart => {
            let r = route?;
            c.kind = CmdKind::AlgoStart as u8;
            c.sym = r.sym;
            c.side = if i.side()? == Side::Buy { 0 } else { 1 };
            c.flags = i.flags & ingress_term::FLAG_REDUCE_ONLY;
            c.px = i.px; // the price cap
            c.qty = i.qty; // the total
            let slip_bps = u16::try_from(i.slip_1e6 / 100).ok()?;
            let (okind, ap) = match i.algo {
                Algo::Twap => (ALGO_TWAP, AlgoParams { dur_s: i.dur_s, slices: i.slices, slip_bps }),
                Algo::Iceberg => (ALGO_ICEBERG, AlgoParams { dur_s: 0, slices: i.slices, slip_bps: 0 }),
                Algo::Chase => (ALGO_CHASE, AlgoParams { dur_s: i.dur_s, slices: 0, slip_bps }),
                Algo::None => return None,
            };
            c.okind = okind;
            c.target_oid = ap.pack();
        }
        Op::AlgoCancel => {
            c.kind = CmdKind::AlgoCancel as u8;
            c.sym = route.map_or(0, |r| r.sym);
            c.target_oid = oid_of(&i.target);
        }
    }
    c.validate_shape().ok().map(|_| c)
}

pub struct DoorClient {
    sock: std::path::PathBuf,
    s: UnixStream,
    key: [u8; 32],
    epoch: u64,
    seq: u32,
    rx: Vec<u8>,
    pub routes: Vec<Route>,
    /// The orders this session sent, for views of later events.
    open: Vec<(u64, u8, u16, Side, i64, i64)>,
}

impl DoorClient {
    pub fn connect(sock: &std::path::Path, key: [u8; 32], epoch: u64, routes: Vec<Route>) -> std::io::Result<DoorClient> {
        let s = UnixStream::connect(sock)?;
        s.set_nonblocking(true)?;
        Ok(DoorClient { sock: sock.to_path_buf(), s, key, epoch, seq: 0, rx: Vec::with_capacity(64 * FRAME_LEN), routes, open: Vec::new() })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The engine restarted (a new `boot.boot_wall_ns` on `/state`): connect
    /// again and quote the new epoch, or every command would be refused.
    pub fn rekey(&mut self, epoch: u64) -> std::io::Result<()> {
        let s = UnixStream::connect(&self.sock)?;
        s.set_nonblocking(true)?;
        self.s = s;
        self.epoch = epoch;
        self.seq = 0;
        self.rx.clear();
        self.open.clear();
        Ok(())
    }

    fn send(&mut self, c: &TermCmd) -> std::io::Result<()> {
        let mut rec = [0u8; REC_LEN];
        c.write_le(&mut rec);
        let mut f = [0u8; FRAME_LEN];
        seal_frame(&self.key, &rec, &mut f);
        // A UDS write of 82 B on a live socket completes or fails whole.
        self.s.write_all(&f)
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    pub fn intent(&mut self, i: &Intent, wall_ns: u64) -> Result<(), &'static str> {
        let seq = self.next_seq();
        let c = to_cmd(i, &self.routes, self.epoch, seq, wall_ns).ok_or("no route for this instrument, or an op the door does not carry yet")?;
        self.send(&c).map_err(|_| "the door socket failed")
    }

    /// Liveness for every routed slot (the member's watchdog input).
    pub fn beat(&mut self, wall_ns: u64) -> std::io::Result<()> {
        let mut slots: Vec<u8> = self.routes.iter().map(|r| r.slot).collect();
        slots.sort_unstable();
        slots.dedup();
        for slot in slots {
            let seq = self.next_seq();
            self.send(&TermCmd { kind: CmdKind::Beat as u8, slot, nonce: u64::from(seq), seq, boot_epoch: self.epoch, sent_wall_ns: wall_ns, ..TermCmd::default() })?;
        }
        Ok(())
    }

    /// Read every event the door has sent; frames with a bad tag are
    /// dropped (they are not from the engine).
    pub fn poll(&mut self, mut f: impl FnMut(TermEvt)) -> std::io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match self.s.read(&mut buf) {
                Ok(0) => return Err(std::io::Error::from(ErrorKind::BrokenPipe)),
                Ok(n) => self.rx.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut used = 0;
        while self.rx.len() - used >= FRAME_LEN {
            let mut fr = [0u8; FRAME_LEN];
            fr.copy_from_slice(&self.rx[used..used + FRAME_LEN]);
            used += FRAME_LEN;
            if let Ok(rec) = open_frame(&self.key, &fr) {
                f(TermEvt::read_le(rec));
            }
        }
        self.rx.drain(..used);
        Ok(())
    }

    fn instr_of(&self, slot: u8, sym: u32) -> u16 {
        self.routes.iter().find(|r| r.slot == slot && r.sym == sym).map_or(u16::MAX, |r| r.instr)
    }

    /// The UI frames for one event, appended to `out`.
    pub fn views(&mut self, e: &TermEvt, ts: u64, out: &mut Vec<u8>) {
        let mut b = [0u8; 256];
        let instr = self.instr_of(e.slot, e.sym);
        let side = if e.side == 0 { Side::Buy } else { Side::Sell };
        // The engine's verdict on the intent this event answers.
        let ack = match EvtKind::from_u8(e.kind) {
            Some(EvtKind::Refused) if e.nonce != 0 && e.reason != 0 => Some(IntentAck { nonce: e.nonce, slot: e.slot, accepted: false, source: AckSource::Engine, reason: e.reason }),
            Some(EvtKind::Sent) if e.nonce != 0 => Some(IntentAck { nonce: e.nonce, slot: e.slot, accepted: true, source: AckSource::Engine, reason: 0 }),
            _ => None,
        };
        if let Some(a) = ack {
            if let Ok(k) = a.encode(0, ts, &mut b) {
                out.extend_from_slice(&b[..k]);
            }
        }
        let n = match EvtKind::from_u8(e.kind) {
            Some(EvtKind::Sent) if e.px > 0 && e.qty > 0 => {
                self.open.push((e.client_oid, e.slot, instr, side, e.px, e.qty));
                OrderView { slot: e.slot, instr, cloid: cloid_of(e.client_oid), side, kind: OrderKind::Limit, status: OrderStatus::Open, px: e.px, qty: e.qty, filled: 0, queue_ahead: -1 }.encode(0, ts, &mut b).ok()
            }
            Some(EvtKind::Cancelled) => self.open.iter().position(|o| o.0 == e.client_oid).and_then(|k| {
                let (oid, slot, instr, side, px, qty) = self.open.remove(k);
                let status = if e.reason == 0 { OrderStatus::Cancelled } else { OrderStatus::Rejected };
                OrderView { slot, instr, cloid: cloid_of(oid), side, kind: OrderKind::Limit, status, px, qty, filled: 0, queue_ahead: -1 }.encode(0, ts, &mut b).ok()
            }),
            Some(EvtKind::Fill) if e.px > 0 && e.qty > 0 => {
                let k = FillView { slot: e.slot, instr, cloid: [0; 16], side, px: e.px, qty: e.qty, fee: 0, ts_ns: e.ts_ns }.encode(0, ts, &mut b).ok();
                if let Some(k) = k {
                    out.extend_from_slice(&b[..k]);
                }
                PositionView { slot: e.slot, instr, qty: e.extra, avg_px: if e.extra != 0 { e.px } else { 0 }, upnl: 0, liq_px: 0 }.encode(0, ts, &mut b).ok()
            }
            _ => None,
        };
        if let Some(n) = n {
            out.extend_from_slice(&b[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ingress_term::listener::{spawn, DoorCfg, Lane};
    use ingress_term::{rings, Door, SLOT_TERM_HL};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn clock() -> (u64, u64) {
        let w = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        (w, w)
    }

    #[test]
    fn algo_intents_map_onto_the_door() {
        let routes = [Route { instr: 0, slot: 12, sym: 3 }];
        let mut i = Intent::blank(5, 12, 0, Op::AlgoStart);
        i.side = Side::Buy as u8;
        i.kind = OrderKind::Limit as u8;
        i.px = 78_000_000_000;
        i.qty = 600;
        i.algo = Algo::Twap;
        i.dur_s = 60;
        i.slices = 6;
        let c = to_cmd(&i, &routes, 1, 1, 1).unwrap();
        assert_eq!((c.kind, c.okind, c.sym, c.qty), (CmdKind::AlgoStart as u8, ALGO_TWAP, 3, 600));
        assert_eq!(AlgoParams::unpack(c.target_oid), AlgoParams { dur_s: 60, slices: 6, slip_bps: 0 });
        i.algo = Algo::Chase;
        i.dur_s = 0;
        i.slices = 0;
        i.slip_1e6 = 2_000; // 20 bps
        let c = to_cmd(&i, &routes, 1, 1, 1).unwrap();
        assert_eq!((c.okind, AlgoParams::unpack(c.target_oid).slip_bps), (ALGO_CHASE, 20));
    }

    #[test]
    fn routes_parse() {
        assert_eq!(Route::parse("0:12:3"), Some(Route { instr: 0, slot: 12, sym: 3 }));
        assert_eq!(Route::parse("0:11:3"), None, "11 is PMJ's");
        assert_eq!(Route::parse("0:12"), None);
    }

    #[test]
    fn intents_cross_the_door_and_events_come_back_as_views() {
        let mut key = [0u8; 32];
        std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut key).unwrap();
        let path = std::env::temp_dir().join(format!("term-door-client-{}.sock", std::process::id()));
        let ((rt, gt, er), (mut rr, mut gr, mut et)) = rings();
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn(DoorCfg { path: path.clone(), door: Door::new(key, 99, 2_000_000_000), clock }, vec![Lane { slot: SLOT_TERM_HL, reduce: rt, grow: gt, evt: er }], stop.clone()).unwrap();
        let mut dc = DoorClient::connect(&path, key, 99, vec![Route { instr: 0, slot: SLOT_TERM_HL, sym: 3 }]).unwrap();
        let mut i = Intent::blank(41, SLOT_TERM_HL, 0, Op::Place);
        i.side = Side::Sell as u8;
        i.kind = OrderKind::Limit as u8;
        i.px = 78_000_000_000;
        i.qty = 150;
        dc.intent(&i, clock().0).unwrap();
        dc.beat(clock().0).unwrap();
        let t0 = Instant::now();
        let (mut g, mut r) = (None, None);
        while (g.is_none() || r.is_none()) && t0.elapsed() < Duration::from_secs(2) {
            g = g.or_else(|| gr.try_pop_ref().map(|x| *x));
            r = r.or_else(|| rr.try_pop_ref().map(|x| *x));
        }
        let c = g.unwrap().cmd;
        assert_eq!((c.kind, c.slot, c.sym, c.side, c.px, c.qty, c.nonce), (CmdKind::Place as u8, 12, 3, 1, 78_000_000_000, 150, 41));
        assert_eq!(r.unwrap().cmd.kind, CmdKind::Beat as u8);
        // An unrouted instrument never reaches the door.
        let mut j = i;
        j.instr = 9;
        assert!(dc.intent(&j, clock().0).is_err());
        // The member's events come back as UI views.
        assert!(et.try_push_ref(&TermEvt { kind: EvtKind::Sent as u8, slot: 12, side: 1, sym: 3, nonce: 41, client_oid: 0x0C00_0001_0000_0000, px: 78_000_000_000, qty: 150, ..TermEvt::default() }));
        let mut evs = Vec::new();
        let t0 = Instant::now();
        while evs.is_empty() && t0.elapsed() < Duration::from_secs(2) {
            dc.poll(|e| evs.push(e)).unwrap();
        }
        let mut out = Vec::new();
        dc.views(&evs[0], 1, &mut out);
        let (a, used) = term_proto::decode(&out).unwrap();
        let a = IntentAck::parse(a.body).unwrap();
        assert_eq!((a.nonce, a.accepted, a.source), (41, true, AckSource::Engine));
        let (f, _) = term_proto::decode(&out[used..]).unwrap();
        let v = OrderView::parse(f.body).unwrap();
        assert_eq!((v.slot, v.instr, v.side, v.status, oid_of(&v.cloid)), (12, 0, Side::Sell, OrderStatus::Open, 0x0C00_0001_0000_0000));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }
}
