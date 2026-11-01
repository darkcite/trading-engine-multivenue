// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Execution frames: the TermEvt mirror (server → UI) and the two frames a
//! browser may send that touch money, `Intent` and `LiveConfirm`.
//!
//! T-L13: the WASM is untrusted. These parsers make a frame well-formed;
//! the server, the member's pre-gate and E6 decide whether it is allowed.

use crate::md::MAX_SLOT;
use crate::{encode, Ascii, Error, Kind, Rd, Result, Side};

/// A client order id: 16 bytes (HL's cloid width); the member encodes the
/// slot in it (E-9). All-zero means "none".
pub type Cloid = [u8; 16];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderKind {
    Limit = 1,
    Ioc = 2,
}

impl OrderKind {
    fn from_u8(v: u8) -> Result<OrderKind> {
        match v {
            1 => Ok(OrderKind::Limit),
            2 => Ok(OrderKind::Ioc),
            _ => Err(Error::BadValue),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderStatus {
    Pending = 1,
    Open = 2,
    Partial = 3,
    Filled = 4,
    Cancelled = 5,
    Rejected = 6,
}

fn slot_ok(s: u8) -> Result<u8> {
    if s <= MAX_SLOT {
        Ok(s)
    } else {
        Err(Error::BadValue)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderView {
    pub slot: u8,
    pub instr: u16,
    pub cloid: Cloid,
    pub side: Side,
    pub kind: OrderKind,
    pub status: OrderStatus,
    pub px: i64,
    pub qty: i64,
    pub filled: i64,
    /// Estimated queue ahead at our price (§4.9); −1 when unknown.
    pub queue_ahead: i64,
}

impl OrderView {
    pub fn parse(body: &[u8]) -> Result<OrderView> {
        let mut r = Rd::new(body);
        let slot = slot_ok(r.u8()?)?;
        let instr = r.u16()?;
        let cloid = r.cloid()?;
        let side = Side::from_u8(r.u8()?)?;
        let kind = OrderKind::from_u8(r.u8()?)?;
        let status = match r.u8()? {
            1 => OrderStatus::Pending,
            2 => OrderStatus::Open,
            3 => OrderStatus::Partial,
            4 => OrderStatus::Filled,
            5 => OrderStatus::Cancelled,
            6 => OrderStatus::Rejected,
            _ => return Err(Error::BadValue),
        };
        let o = OrderView { slot, instr, cloid, side, kind, status, px: r.i64()?, qty: r.i64()?, filled: r.i64()?, queue_ahead: r.i64()? };
        r.end()?;
        if o.px <= 0 || o.qty <= 0 || o.filled < 0 || o.filled > o.qty || o.queue_ahead < -1 || o.cloid == [0; 16] {
            return Err(Error::BadValue);
        }
        Ok(o)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::OrderView, seq, ts, out, |w| {
            w.u8(self.slot)?;
            w.u16(self.instr)?;
            w.bytes(&self.cloid)?;
            w.u8(self.side as u8)?;
            w.u8(self.kind as u8)?;
            w.u8(self.status as u8)?;
            for v in [self.px, self.qty, self.filled, self.queue_ahead] {
                w.i64(v)?;
            }
            Ok(())
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PositionView {
    pub slot: u8,
    pub instr: u16,
    /// Signed: positive long.
    pub qty: i64,
    pub avg_px: i64,
    pub upnl: i64,
    /// 0 when the venue reports none.
    pub liq_px: i64,
}

impl PositionView {
    pub fn parse(body: &[u8]) -> Result<PositionView> {
        let mut r = Rd::new(body);
        let p = PositionView { slot: slot_ok(r.u8()?)?, instr: r.u16()?, qty: r.i64()?, avg_px: r.i64()?, upnl: r.i64()?, liq_px: r.i64()? };
        r.end()?;
        if p.avg_px < 0 || p.liq_px < 0 || (p.qty != 0 && p.avg_px == 0) {
            return Err(Error::BadValue);
        }
        Ok(p)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::PositionView, seq, ts, out, |w| {
            w.u8(self.slot)?;
            w.u16(self.instr)?;
            for v in [self.qty, self.avg_px, self.upnl, self.liq_px] {
                w.i64(v)?;
            }
            Ok(())
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FillView {
    pub slot: u8,
    pub instr: u16,
    pub cloid: Cloid,
    pub side: Side,
    pub px: i64,
    pub qty: i64,
    /// Measured fee (T-L4), signed: a rebate is negative.
    pub fee: i64,
    pub ts_ns: u64,
}

impl FillView {
    pub fn parse(body: &[u8]) -> Result<FillView> {
        let mut r = Rd::new(body);
        let f = FillView {
            slot: slot_ok(r.u8()?)?,
            instr: r.u16()?,
            cloid: r.cloid()?,
            side: Side::from_u8(r.u8()?)?,
            px: r.i64()?,
            qty: r.i64()?,
            fee: r.i64()?,
            ts_ns: r.u64()?,
        };
        r.end()?;
        if f.px <= 0 || f.qty <= 0 {
            return Err(Error::BadValue);
        }
        Ok(f)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::FillView, seq, ts, out, |w| {
            w.u8(self.slot)?;
            w.u16(self.instr)?;
            w.bytes(&self.cloid)?;
            w.u8(self.side as u8)?;
            w.i64(self.px)?;
            w.i64(self.qty)?;
            w.i64(self.fee)?;
            w.u64(self.ts_ns)
        })
    }
}

// ---- EngineStatus ----------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SlotMode {
    #[default]
    Off = 0,
    Paper = 1,
    Live = 2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct SlotState {
    pub slot: u8,
    pub mode: SlotMode,
    pub halted: bool,
}

/// PAPER/LIVE per slot, read from `/state` by the server — never from
/// config (§4.6). The UI shows what this says and nothing else.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EngineStatus {
    pub boot_epoch: u64,
    pub git_sha: Ascii<12>,
    pub engine_up: bool,
    pub n: u8,
    pub slots: [SlotState; 16],
}

impl EngineStatus {
    pub fn parse(body: &[u8]) -> Result<EngineStatus> {
        let mut r = Rd::new(body);
        let boot_epoch = r.u64()?;
        let git_sha = Ascii::<12>::get(&mut r)?;
        if !git_sha.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            return Err(Error::BadValue);
        }
        let engine_up = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Error::BadValue),
        };
        let n = r.u8()?;
        if n as usize > 16 {
            return Err(Error::BadValue);
        }
        let mut slots = [SlotState::default(); 16];
        let mut seen = 0u32;
        for s in slots.iter_mut().take(n as usize) {
            let slot = slot_ok(r.u8()?)?;
            let mode = match r.u8()? {
                0 => SlotMode::Off,
                1 => SlotMode::Paper,
                2 => SlotMode::Live,
                _ => return Err(Error::BadValue),
            };
            let halted = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(Error::BadValue),
            };
            if seen & (1 << slot) != 0 {
                return Err(Error::BadValue); // a slot listed twice
            }
            seen |= 1 << slot;
            *s = SlotState { slot, mode, halted };
        }
        r.end()?;
        Ok(EngineStatus { boot_epoch, git_sha, engine_up, n, slots })
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if self.n as usize > 16 {
            return Err(Error::BadValue);
        }
        encode(Kind::EngineStatus, seq, ts, out, |w| {
            w.u64(self.boot_epoch)?;
            self.git_sha.put(w)?;
            w.u8(self.engine_up as u8)?;
            w.u8(self.n)?;
            for s in &self.slots[..self.n as usize] {
                w.u8(s.slot)?;
                w.u8(s.mode as u8)?;
                w.u8(s.halted as u8)?;
            }
            Ok(())
        })
    }
}

// ---- Intent (UI → server) ----------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    Place = 1,
    Modify = 2,
    Cancel = 3,
    CancelAll = 4,
    Flatten = 5,
    AlgoStart = 6,
    AlgoCancel = 7,
    Halt = 8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Algo {
    None = 0,
    Twap = 1,
    Chase = 2,
    Iceberg = 3,
}

pub const FLAG_REDUCE_ONLY: u8 = 1;
pub const FLAG_POST_ONLY: u8 = 2;
const FLAGS_KNOWN: u8 = FLAG_REDUCE_ONLY | FLAG_POST_ONLY;

/// One operator intent, as ×1e6 integers. Fixed layout, every field always
/// present; fields an op does not use must be zero.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// The client's unique id for this intent (idempotence, confirm nonces).
    pub nonce: u64,
    pub slot: u8,
    pub instr: u16,
    pub op: Op,
    /// 0 when the op has no side.
    pub side: u8,
    /// 0 when the op has no order kind.
    pub kind: u8,
    pub flags: u8,
    pub px: i64,
    pub qty: i64,
    pub target: Cloid,
    pub algo: Algo,
    pub dur_s: u32,
    pub slices: u16,
    pub slip_1e6: i64,
}

impl Intent {
    pub const LEN: usize = 8 + 1 + 2 + 1 + 1 + 1 + 1 + 8 + 8 + 16 + 1 + 4 + 2 + 8;

    pub const fn blank(nonce: u64, slot: u8, instr: u16, op: Op) -> Intent {
        Intent {
            nonce,
            slot,
            instr,
            op,
            side: 0,
            kind: 0,
            flags: 0,
            px: 0,
            qty: 0,
            target: [0; 16],
            algo: Algo::None,
            dur_s: 0,
            slices: 0,
            slip_1e6: 0,
        }
    }

    pub fn side(&self) -> Option<Side> {
        Side::from_u8(self.side).ok()
    }

    pub fn parse(body: &[u8]) -> Result<Intent> {
        let mut r = Rd::new(body);
        let nonce = r.u64()?;
        let slot = slot_ok(r.u8()?)?;
        let instr = r.u16()?;
        let op = match r.u8()? {
            1 => Op::Place,
            2 => Op::Modify,
            3 => Op::Cancel,
            4 => Op::CancelAll,
            5 => Op::Flatten,
            6 => Op::AlgoStart,
            7 => Op::AlgoCancel,
            8 => Op::Halt,
            _ => return Err(Error::BadValue),
        };
        let side = r.u8()?;
        let kind = r.u8()?;
        let flags = r.u8()?;
        let px = r.i64()?;
        let qty = r.i64()?;
        let target = r.cloid()?;
        let algo = match r.u8()? {
            0 => Algo::None,
            1 => Algo::Twap,
            2 => Algo::Chase,
            3 => Algo::Iceberg,
            _ => return Err(Error::BadValue),
        };
        let i = Intent { nonce, slot, instr, op, side, kind, flags, px, qty, target, algo, dur_s: r.u32()?, slices: r.u16()?, slip_1e6: r.i64()? };
        r.end()?;
        i.check()?;
        Ok(i)
    }

    /// The per-op shape: required fields set, unused fields zero.
    pub fn check(&self) -> Result<()> {
        let z_order = self.side == 0 && self.kind == 0 && self.flags == 0 && self.px == 0 && self.qty == 0;
        let z_target = self.target == [0; 16];
        let z_algo = self.algo == Algo::None && self.dur_s == 0 && self.slices == 0 && self.slip_1e6 == 0;
        let order_ok = Side::from_u8(self.side).is_ok()
            && OrderKind::from_u8(self.kind).is_ok()
            && self.flags & !FLAGS_KNOWN == 0
            && !(self.kind == OrderKind::Ioc as u8 && self.flags & FLAG_POST_ONLY != 0)
            && self.px > 0
            && self.qty > 0;
        let ok = self.nonce != 0
            && match self.op {
                Op::Place => order_ok && z_target && z_algo,
                Op::Modify => !z_target && self.px > 0 && self.qty > 0 && self.side == 0 && self.kind == 0 && self.flags == 0 && z_algo,
                Op::Cancel | Op::AlgoCancel => !z_target && z_order && z_algo,
                Op::CancelAll | Op::Flatten | Op::Halt => z_target && z_order && z_algo,
                Op::AlgoStart => {
                    let shape = match self.algo {
                        Algo::Twap => self.dur_s > 0 && self.slices > 0,
                        Algo::Chase => self.slip_1e6 > 0,
                        Algo::Iceberg => self.slices > 1,
                        Algo::None => false,
                    };
                    order_ok && z_target && shape && self.slip_1e6 >= 0
                }
            };
        if ok {
            Ok(())
        } else {
            Err(Error::BadValue)
        }
    }

    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        self.check()?;
        encode(Kind::Intent, seq, ts, out, |w| {
            w.u64(self.nonce)?;
            w.u8(self.slot)?;
            w.u16(self.instr)?;
            w.u8(self.op as u8)?;
            w.u8(self.side)?;
            w.u8(self.kind)?;
            w.u8(self.flags)?;
            w.i64(self.px)?;
            w.i64(self.qty)?;
            w.bytes(&self.target)?;
            w.u8(self.algo as u8)?;
            w.u32(self.dur_s)?;
            w.u16(self.slices)?;
            w.i64(self.slip_1e6)
        })
    }
}

// ---- LiveConfirm (UI → server) ---------------------------------------------

/// The typed live confirmation: a UX step against slips, not authentication
/// (§3). The server compares `phrase` byte for byte and checks `nonce`
/// against the one it issued.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiveConfirm {
    pub slot: u8,
    pub nonce: u64,
    pub phrase: Ascii<32>,
}

impl LiveConfirm {
    pub fn parse(body: &[u8]) -> Result<LiveConfirm> {
        let mut r = Rd::new(body);
        let c = LiveConfirm { slot: slot_ok(r.u8()?)?, nonce: r.u64()?, phrase: Ascii::<32>::get(&mut r)? };
        r.end()?;
        if c.nonce == 0 || c.phrase.as_bytes().is_empty() {
            return Err(Error::BadValue);
        }
        Ok(c)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::LiveConfirm, seq, ts, out, |w| {
            w.u8(self.slot)?;
            w.u64(self.nonce)?;
            self.phrase.put(w)
        })
    }
}
