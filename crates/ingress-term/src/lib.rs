// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `ingress-term` — the TERM door (TERM plan v2.1 §4.2, slots per O-T17).
//!
//! The terminal (a cold process) reaches the engine only here: one UDS,
//! frames of `len u16 = 80 · 64-B record · HMAC-SHA256 tag[0..16]` under
//! TERM's own key (`~/multivenue/term.env`, R-T13) — the `ingress-ai` frame
//! law with a different key and record. Both directions are tagged.
//!
//! **Accept order** ([`Door::accept`]): length → HMAC → shape (the slot is a
//! TERM slot, 12–15) → seq → boot epoch → `sent_wall_ns` age → receive-time
//! stamp → capture (`term-cmds.pmlr`, `SlotKind::TermCmd`, by the door's own
//! thread) → hand-off.
//!
//! **Hand-off**: reducing commands (cancel, cancel-all, flatten, halt, the
//! heartbeat and the door's own `Disconnect`) ride their own ring and are
//! never dropped — a full reduce ring is backpressure. Growing commands
//! (place, modify) ride a second ring; a full grow ring refuses.
//!
//! T-L2: Claude never sends a frame to the live door. Tests use an
//! in-process door with a key generated per run.

pub mod listener;

use core_crypto::{ct_eq, hmac_sha256_tag16};

pub const FRAME_LEN: usize = 82;
pub const LEN_FIELD_VALUE: u16 = 80;
pub const REC_LEN: usize = 64;
const TAG_OFFSET: usize = 2 + REC_LEN;

/// Ruling O-T17 (2026-11-01): term-hl 12, term-bn 13, term-pm 14,
/// term-hc 15. Slot 10 stays reserved; 11 is PMJ's.
pub const SLOT_TERM_HL: u8 = 12;
pub const SLOT_TERM_BN: u8 = 13;
pub const SLOT_TERM_PM: u8 = 14;
pub const SLOT_TERM_HC: u8 = 15;
pub const TERM_SLOT_MASK: u16 = 0xF000;

#[inline]
pub const fn is_term_slot(slot: u8) -> bool {
    slot < 16 && TERM_SLOT_MASK & (1u16 << slot) != 0
}

// ---- TermCmd ------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CmdKind {
    /// Liveness from the terminal, once a second (the watchdog's input).
    Beat = 1,
    Place = 2,
    Modify = 3,
    Cancel = 4,
    CancelAll = 5,
    /// Cancel everything on `sym`, then exit reduce-only IoC at the touch.
    Flatten = 6,
    /// Slot-scoped halt (never the engine-wide `AiCmdKind::HaltRequest`).
    Halt = 7,
    /// Door-only: the terminal's socket closed (R-T15: cancel). Refused
    /// from the wire.
    Disconnect = 8,
    /// Start an algo (growing): `okind` is the algo, `px` its price cap,
    /// `qty` its total, `target_oid` its packed parameters ([`AlgoParams`]).
    AlgoStart = 9,
    /// Stop an algo (reducing): `target_oid` is the algo's id (its start
    /// command's nonce).
    AlgoCancel = 10,
}

pub const ALGO_TWAP: u8 = 1;
pub const ALGO_CHASE: u8 = 2;
pub const ALGO_ICEBERG: u8 = 3;
pub const ALGO_MAX_SLICES: u16 = 100;

/// An algo's parameters, packed into `target_oid`: `dur_s` in bits 0..32,
/// `slices` in 32..48, `slip_bps` in 48..64.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct AlgoParams {
    pub dur_s: u32,
    pub slices: u16,
    pub slip_bps: u16,
}

impl AlgoParams {
    pub const fn pack(self) -> u64 {
        self.dur_s as u64 | (self.slices as u64) << 32 | (self.slip_bps as u64) << 48
    }
    pub const fn unpack(v: u64) -> AlgoParams {
        AlgoParams { dur_s: v as u32, slices: (v >> 32) as u16, slip_bps: (v >> 48) as u16 }
    }
    /// The per-algo shape.
    pub const fn valid_for(self, algo: u8) -> bool {
        match algo {
            ALGO_TWAP => self.dur_s >= 1 && self.dur_s <= 86_400 && self.slices >= 1 && self.slices <= ALGO_MAX_SLICES && self.slip_bps <= 500,
            ALGO_ICEBERG => self.slices >= 2 && self.slices <= ALGO_MAX_SLICES && self.dur_s == 0 && self.slip_bps == 0,
            ALGO_CHASE => self.slip_bps >= 1 && self.slip_bps <= 500 && self.dur_s <= 86_400 && self.slices == 0,
            _ => false,
        }
    }
}

impl CmdKind {
    pub const fn from_u8(v: u8) -> Option<CmdKind> {
        Some(match v {
            1 => CmdKind::Beat,
            2 => CmdKind::Place,
            3 => CmdKind::Modify,
            4 => CmdKind::Cancel,
            5 => CmdKind::CancelAll,
            6 => CmdKind::Flatten,
            7 => CmdKind::Halt,
            8 => CmdKind::Disconnect,
            9 => CmdKind::AlgoStart,
            10 => CmdKind::AlgoCancel,
            _ => return None,
        })
    }
    /// Reducing commands skip the pre-gate and are never dropped.
    pub const fn is_reducing(self) -> bool {
        !matches!(self, CmdKind::Place | CmdKind::Modify | CmdKind::AlgoStart)
    }
}

pub const OKIND_LIMIT: u8 = 1;
pub const OKIND_IOC: u8 = 2;
pub const FLAG_REDUCE_ONLY: u8 = 1;
pub const FLAG_POST_ONLY: u8 = 2;

/// One terminal command, 64 B little-endian:
/// `kind u8 · slot u8 · side u8 · okind u8 · flags u8 · 0×3 · seq u32 ·
/// sym u32 · px i64 · qty i64 · target_oid u64 · nonce u64 · boot_epoch u64
/// · sent_wall_ns u64`. Prices and quantities ×1e6 (T-L8); `side` uses
/// `core_types::Side` numbering (0 buy, 1 sell); `sym` is the engine's
/// `SymbolId`, resolved by the terminal from the descriptor allow-list.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TermCmd {
    pub kind: u8,
    pub slot: u8,
    pub side: u8,
    pub okind: u8,
    pub flags: u8,
    pub seq: u32,
    pub sym: u32,
    pub px: i64,
    pub qty: i64,
    pub target_oid: u64,
    pub nonce: u64,
    pub boot_epoch: u64,
    pub sent_wall_ns: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShapeError {
    Kind,
    Slot,
    Padding,
    Nonce,
    Fields,
}

fn rd8(b: &[u8; REC_LEN], o: usize) -> u64 {
    // COPY: 8 B field → stack array — the record is unaligned bytes — read_unaligned
    // via unsafe rejected (the crate has no unsafe; 8 B costs nothing).
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
fn rd4(b: &[u8; REC_LEN], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl TermCmd {
    pub fn read_le(b: &[u8; REC_LEN]) -> (TermCmd, [u8; 3]) {
        let c = TermCmd {
            kind: b[0],
            slot: b[1],
            side: b[2],
            okind: b[3],
            flags: b[4],
            seq: rd4(b, 8),
            sym: rd4(b, 12),
            px: rd8(b, 16) as i64,
            qty: rd8(b, 24) as i64,
            target_oid: rd8(b, 32),
            nonce: rd8(b, 40),
            boot_epoch: rd8(b, 48),
            sent_wall_ns: rd8(b, 56),
        };
        (c, [b[5], b[6], b[7]])
    }

    pub fn write_le(&self, b: &mut [u8; REC_LEN]) {
        b[0] = self.kind;
        b[1] = self.slot;
        b[2] = self.side;
        b[3] = self.okind;
        b[4] = self.flags;
        b[5..8].fill(0);
        // COPY: scalar → LE bytes, ≤ 8 B per field, 64 B per record — the wire is a
        // fixed little-endian layout — a #[repr(C)] transmute rejected (endianness, padding).
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12..16].copy_from_slice(&self.sym.to_le_bytes());
        b[16..24].copy_from_slice(&self.px.to_le_bytes());
        b[24..32].copy_from_slice(&self.qty.to_le_bytes());
        // COPY: scalar → LE bytes, ≤ 8 B per field, 64 B per record — the wire is a
        // fixed little-endian layout — a #[repr(C)] transmute rejected (endianness, padding).
        b[32..40].copy_from_slice(&self.target_oid.to_le_bytes());
        b[40..48].copy_from_slice(&self.nonce.to_le_bytes());
        b[48..56].copy_from_slice(&self.boot_epoch.to_le_bytes());
        b[56..64].copy_from_slice(&self.sent_wall_ns.to_le_bytes());
    }

    /// The per-kind shape. Fields a kind does not use must be zero.
    pub fn validate_shape(&self) -> Result<CmdKind, ShapeError> {
        let k = CmdKind::from_u8(self.kind).ok_or(ShapeError::Kind)?;
        if k == CmdKind::Disconnect {
            return Err(ShapeError::Kind); // door-only
        }
        if !is_term_slot(self.slot) {
            return Err(ShapeError::Slot);
        }
        if self.nonce == 0 {
            return Err(ShapeError::Nonce);
        }
        let no_order = self.side == 0 && self.okind == 0 && self.flags == 0 && self.px == 0 && self.qty == 0;
        let ok = match k {
            CmdKind::Place => {
                self.side <= 1
                    && (self.okind == OKIND_LIMIT || self.okind == OKIND_IOC)
                    && self.flags & !(FLAG_REDUCE_ONLY | FLAG_POST_ONLY) == 0
                    && !(self.okind == OKIND_IOC && self.flags & FLAG_POST_ONLY != 0)
                    && self.px > 0
                    && self.qty > 0
                    && self.target_oid == 0
            }
            CmdKind::Modify => self.target_oid != 0 && self.px > 0 && self.qty > 0 && self.side == 0 && self.okind == 0 && self.flags == 0,
            CmdKind::Cancel => self.target_oid != 0 && no_order,
            CmdKind::CancelAll | CmdKind::Flatten => self.target_oid == 0 && no_order,
            CmdKind::Halt | CmdKind::Beat => self.target_oid == 0 && no_order && self.sym == 0,
            CmdKind::AlgoStart => {
                self.side <= 1
                    && self.flags & !FLAG_REDUCE_ONLY == 0
                    && self.px > 0
                    && self.qty > 0
                    && AlgoParams::unpack(self.target_oid).valid_for(self.okind)
            }
            CmdKind::AlgoCancel => self.target_oid != 0 && no_order,
            CmdKind::Disconnect => false,
        };
        if ok {
            Ok(k)
        } else {
            Err(ShapeError::Fields)
        }
    }
}

// ---- TermEvt ------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EvtKind {
    /// The door or the member refused a command; `reason` says why.
    Refused = 1,
    /// An order left the member (submit / modify / cancel issued).
    Sent = 2,
    Fill = 3,
    Cancelled = 4,
    Halted = 5,
    /// The watchdog cancelled everything (heartbeat or UI lost).
    Watchdog = 6,
}

impl EvtKind {
    pub const fn from_u8(v: u8) -> Option<EvtKind> {
        Some(match v {
            1 => EvtKind::Refused,
            2 => EvtKind::Sent,
            3 => EvtKind::Fill,
            4 => EvtKind::Cancelled,
            5 => EvtKind::Halted,
            6 => EvtKind::Watchdog,
            _ => return None,
        })
    }
}

/// Why a command was refused (door and pre-gate, one byte).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    None = 0,
    Shape = 1,
    Seq = 2,
    Epoch = 3,
    Stale = 4,
    RingFull = 5,
    Halted = 6,
    Band = 7,
    VenueMin = 8,
    Notional = 9,
    Position = 10,
    Rate = 11,
    NoTouch = 12,
    UnknownOrder = 13,
    Submit = 14,
    Instrument = 15,
}

/// One door event, 64 B: `kind u8 · slot u8 · reason u8 · side u8 · 0×4 ·
/// sym u32 · 0×4 · nonce u64 · client_oid u64 · px i64 · qty i64 · ts_ns
/// u64 · extra i64`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TermEvt {
    pub kind: u8,
    pub slot: u8,
    pub reason: u8,
    pub side: u8,
    pub sym: u32,
    pub nonce: u64,
    pub client_oid: u64,
    pub px: i64,
    pub qty: i64,
    pub ts_ns: u64,
    pub extra: i64,
}

impl TermEvt {
    pub fn write_le(&self, b: &mut [u8; REC_LEN]) {
        b.fill(0);
        b[0] = self.kind;
        b[1] = self.slot;
        b[2] = self.reason;
        b[3] = self.side;
        // COPY: scalar → LE bytes, ≤ 8 B per field, 64 B per record — the wire is a
        // fixed little-endian layout — a #[repr(C)] transmute rejected (endianness, padding).
        b[8..12].copy_from_slice(&self.sym.to_le_bytes());
        b[16..24].copy_from_slice(&self.nonce.to_le_bytes());
        b[24..32].copy_from_slice(&self.client_oid.to_le_bytes());
        b[32..40].copy_from_slice(&self.px.to_le_bytes());
        // COPY: scalar → LE bytes, ≤ 8 B per field, 64 B per record — the wire is a
        // fixed little-endian layout — a #[repr(C)] transmute rejected (endianness, padding).
        b[40..48].copy_from_slice(&self.qty.to_le_bytes());
        b[48..56].copy_from_slice(&self.ts_ns.to_le_bytes());
        b[56..64].copy_from_slice(&self.extra.to_le_bytes());
    }
    pub fn read_le(b: &[u8; REC_LEN]) -> TermEvt {
        TermEvt {
            kind: b[0],
            slot: b[1],
            reason: b[2],
            side: b[3],
            sym: rd4(b, 8),
            nonce: rd8(b, 16),
            client_oid: rd8(b, 24),
            px: rd8(b, 32) as i64,
            qty: rd8(b, 40) as i64,
            ts_ns: rd8(b, 48),
            extra: rd8(b, 56) as i64,
        }
    }
    pub fn refused(cmd: &TermCmd, reason: Reason, ts_ns: u64) -> TermEvt {
        TermEvt { kind: EvtKind::Refused as u8, slot: cmd.slot, reason: reason as u8, side: cmd.side, sym: cmd.sym, nonce: cmd.nonce, ts_ns, ..TermEvt::default() }
    }
}

// ---- framing ----------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    BadLen(u16),
    BadTag,
}

/// Verify a frame's length and tag; return its record bytes in place.
pub fn open_frame<'a>(key: &[u8; 32], f: &'a [u8; FRAME_LEN]) -> Result<&'a [u8; REC_LEN], FrameError> {
    let lf = u16::from_le_bytes([f[0], f[1]]);
    if lf != LEN_FIELD_VALUE {
        return Err(FrameError::BadLen(lf));
    }
    let tag = hmac_sha256_tag16(key, &f[2..TAG_OFFSET]);
    if !ct_eq(&tag, &f[TAG_OFFSET..]) {
        return Err(FrameError::BadTag);
    }
    let rec: &[u8; REC_LEN] = f[2..TAG_OFFSET].try_into().map_err(|_| FrameError::BadLen(lf))?;
    Ok(rec)
}

/// Seal a 64-B record into a frame under `key`.
pub fn seal_frame(key: &[u8; 32], rec: &[u8; REC_LEN], out: &mut [u8; FRAME_LEN]) {
    // COPY: 64 B record + 18 B header/tag into the 82 B frame — the frame is the
    // one buffer the socket writes — writev of three slices rejected (one syscall per frame).
    out[..2].copy_from_slice(&LEN_FIELD_VALUE.to_le_bytes());
    out[2..TAG_OFFSET].copy_from_slice(rec);
    let tag = hmac_sha256_tag16(key, rec);
    out[TAG_OFFSET..].copy_from_slice(&tag);
}

// ---- the door's accept order --------------------------------------------------------

/// A command accepted by the door, stamped with the engine's receive time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TermIn {
    pub cmd: TermCmd,
    /// Engine monotonic ns at acceptance (the `ts_ns` rewrite).
    pub recv_ns: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Frame(FrameError),
    Shape(ShapeError),
    SeqRegress,
    Epoch,
    Stale,
}

impl Refusal {
    pub fn reason(self) -> Reason {
        match self {
            Refusal::Frame(_) | Refusal::Shape(_) => Reason::Shape,
            Refusal::SeqRegress => Reason::Seq,
            Refusal::Epoch => Reason::Epoch,
            Refusal::Stale => Reason::Stale,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DoorStats {
    pub accepted: u64,
    pub refused: u64,
    pub seq_gaps: u64,
}

pub struct Door {
    key: [u8; 32],
    boot_epoch: u64,
    max_age_ns: u64,
    last_seq: Option<u32>,
    pub stats: DoorStats,
}

impl Door {
    pub fn new(key: [u8; 32], boot_epoch: u64, max_age_ns: u64) -> Door {
        Door { key, boot_epoch, max_age_ns, last_seq: None, stats: DoorStats::default() }
    }

    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }

    pub fn boot_epoch(&self) -> u64 {
        self.boot_epoch
    }

    /// A new connection starts a new sequence.
    pub fn reset_seq(&mut self) {
        self.last_seq = None;
    }

    /// The accept order of TERM plan §4.2, steps 1–7.
    pub fn accept(&mut self, f: &[u8; FRAME_LEN], wall_ns: u64, mono_ns: u64) -> Result<(TermIn, CmdKind), (Refusal, TermCmd)> {
        let r = self.accept_inner(f, wall_ns, mono_ns);
        match &r {
            Ok(_) => self.stats.accepted += 1,
            Err(_) => self.stats.refused += 1,
        }
        r
    }

    fn accept_inner(&mut self, f: &[u8; FRAME_LEN], wall_ns: u64, mono_ns: u64) -> Result<(TermIn, CmdKind), (Refusal, TermCmd)> {
        let rec = open_frame(&self.key, f).map_err(|e| (Refusal::Frame(e), TermCmd::default()))?;
        let (cmd, pad) = TermCmd::read_le(rec);
        if pad != [0; 3] {
            return Err((Refusal::Shape(ShapeError::Padding), cmd));
        }
        let kind = cmd.validate_shape().map_err(|e| (Refusal::Shape(e), cmd))?;
        match self.last_seq {
            Some(last) if cmd.seq <= last => return Err((Refusal::SeqRegress, cmd)),
            Some(last) if cmd.seq > last + 1 => self.stats.seq_gaps += 1,
            _ => {}
        }
        self.last_seq = Some(cmd.seq);
        if cmd.boot_epoch != self.boot_epoch {
            return Err((Refusal::Epoch, cmd));
        }
        // Reducing commands are never refused for age: a late cancel is
        // still a cancel (T-L4).
        if !kind.is_reducing() && (wall_ns < cmd.sent_wall_ns.saturating_sub(self.max_age_ns) || wall_ns.saturating_sub(cmd.sent_wall_ns) > self.max_age_ns) {
            return Err((Refusal::Stale, cmd));
        }
        Ok((TermIn { cmd, recv_ns: mono_ns }, kind))
    }
}

/// The reduce ring: deep, never dropped (backpressure).
pub const REDUCE_RING: usize = 1024;
/// The grow ring: shallow; full ⇒ refuse.
pub const GROW_RING: usize = 256;
pub const EVT_RING: usize = 1024;

pub type ReduceTx = core_ring::Producer<TermIn, REDUCE_RING>;
pub type ReduceRx = core_ring::Consumer<TermIn, REDUCE_RING>;
pub type GrowTx = core_ring::Producer<TermIn, GROW_RING>;
pub type GrowRx = core_ring::Consumer<TermIn, GROW_RING>;
pub type EvtTx = core_ring::Producer<TermEvt, EVT_RING>;
pub type EvtRx = core_ring::Consumer<TermEvt, EVT_RING>;

/// Build the three rings: (door ends, member ends).
pub fn rings() -> ((ReduceTx, GrowTx, EvtRx), (ReduceRx, GrowRx, EvtTx)) {
    let (rt, rr) = core_ring::Ring::<TermIn, REDUCE_RING>::new().split();
    let (gt, gr) = core_ring::Ring::<TermIn, GROW_RING>::new().split();
    let (et, er) = core_ring::Ring::<TermEvt, EVT_RING>::new().split();
    ((rt, gt, er), (rr, gr, et))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const KEY: [u8; 32] = [7; 32];

    fn place(seq: u32, epoch: u64, sent: u64) -> TermCmd {
        TermCmd { kind: CmdKind::Place as u8, slot: SLOT_TERM_HL, side: 0, okind: OKIND_LIMIT, flags: 0, seq, sym: 3, px: 78_000_000_000, qty: 100, target_oid: 0, nonce: 9, boot_epoch: epoch, sent_wall_ns: sent }
    }

    fn frame(c: &TermCmd, key: &[u8; 32]) -> [u8; FRAME_LEN] {
        let mut rec = [0u8; REC_LEN];
        c.write_le(&mut rec);
        let mut f = [0u8; FRAME_LEN];
        seal_frame(key, &rec, &mut f);
        f
    }

    #[test]
    fn the_accept_order() {
        let mut d = Door::new(KEY, 77, 2_000_000_000);
        let now = 1_000_000_000_000;
        assert!(d.accept(&frame(&place(1, 77, now), &KEY), now, 5).is_ok());
        // Wrong key → tag.
        assert_eq!(d.accept(&frame(&place(2, 77, now), &[8; 32]), now, 5).unwrap_err().0, Refusal::Frame(FrameError::BadTag));
        // Slot 11 is PMJ's: shape.
        let mut c = place(3, 77, now);
        c.slot = 11;
        assert_eq!(d.accept(&frame(&c, &KEY), now, 5).unwrap_err().0, Refusal::Shape(ShapeError::Slot));
        // Seq regress.
        assert_eq!(d.accept(&frame(&place(1, 77, now), &KEY), now, 5).unwrap_err().0, Refusal::SeqRegress);
        // Another boot's epoch.
        assert_eq!(d.accept(&frame(&place(4, 76, now), &KEY), now, 5).unwrap_err().0, Refusal::Epoch);
        // Stale growing command…
        assert_eq!(d.accept(&frame(&place(5, 77, now - 3_000_000_000), &KEY), now, 5).unwrap_err().0, Refusal::Stale);
        // …but a stale cancel is still a cancel.
        let cancel = TermCmd { kind: CmdKind::Cancel as u8, slot: SLOT_TERM_HL, target_oid: 42, nonce: 1, seq: 6, boot_epoch: 77, sent_wall_ns: now - 9_000_000_000, sym: 3, ..TermCmd::default() };
        let (ok, k) = d.accept(&frame(&cancel, &KEY), now, 6).unwrap();
        assert!(k.is_reducing());
        assert_eq!(ok.recv_ns, 6);
    }

    #[test]
    fn disconnect_is_door_only_and_halt_is_bare() {
        let mut c = TermCmd { kind: CmdKind::Disconnect as u8, slot: 12, nonce: 1, ..TermCmd::default() };
        assert_eq!(c.validate_shape(), Err(ShapeError::Kind));
        c.kind = CmdKind::Halt as u8;
        assert_eq!(c.validate_shape(), Ok(CmdKind::Halt));
        c.sym = 4;
        assert_eq!(c.validate_shape(), Err(ShapeError::Fields), "halt is slot-scoped, never per symbol");
    }

    #[test]
    fn algo_shapes() {
        let mut c = TermCmd { kind: CmdKind::AlgoStart as u8, slot: 12, side: 1, okind: ALGO_TWAP, px: 5, qty: 60, nonce: 3, sym: 1, ..TermCmd::default() };
        c.target_oid = AlgoParams { dur_s: 60, slices: 6, slip_bps: 0 }.pack();
        assert_eq!(c.validate_shape(), Ok(CmdKind::AlgoStart));
        assert!(!CmdKind::AlgoStart.is_reducing() && CmdKind::AlgoCancel.is_reducing());
        c.target_oid = AlgoParams { dur_s: 60, slices: 0, slip_bps: 0 }.pack();
        assert!(c.validate_shape().is_err(), "a TWAP needs slices");
        c.okind = ALGO_ICEBERG;
        c.target_oid = AlgoParams { dur_s: 0, slices: 5, slip_bps: 0 }.pack();
        assert!(c.validate_shape().is_ok());
        c.okind = ALGO_CHASE;
        c.target_oid = AlgoParams { dur_s: 0, slices: 0, slip_bps: 600 }.pack();
        assert!(c.validate_shape().is_err(), "chase slip ≤ 500 bps");
        assert_eq!(AlgoParams::unpack(AlgoParams { dur_s: 7, slices: 8, slip_bps: 9 }.pack()), AlgoParams { dur_s: 7, slices: 8, slip_bps: 9 });
    }

    #[test]
    fn evt_round_trip() {
        let e = TermEvt { kind: 3, slot: 13, reason: 0, side: 1, sym: 9, nonce: 5, client_oid: u64::MAX, px: -1, qty: 7, ts_ns: 8, extra: -9 };
        let mut b = [0u8; REC_LEN];
        e.write_le(&mut b);
        assert_eq!(TermEvt::read_le(&b), e);
    }

    proptest! {
        #[test]
        fn any_bytes_never_panic_and_accepted_means_shaped(bytes in prop::collection::vec(any::<u8>(), FRAME_LEN..=FRAME_LEN), seal in any::<bool>()) {
            let mut f = [0u8; FRAME_LEN];
            f.copy_from_slice(&bytes);
            if seal {
                let rec: [u8; REC_LEN] = f[2..66].try_into().unwrap();
                seal_frame(&KEY, &rec, &mut f);
            }
            let mut d = Door::new(KEY, u64::from_le_bytes(f[50..58].try_into().unwrap()), u64::MAX);
            if let Ok((t, _)) = d.accept(&f, 0, 1) {
                prop_assert!(is_term_slot(t.cmd.slot));
                prop_assert!(t.cmd.validate_shape().is_ok());
            }
        }

        #[test]
        fn cmd_round_trip(kind in 1u8..=7, slot in 12u8..=15, side in 0u8..2, seq in any::<u32>(), sym in any::<u32>(), px in 1i64.., qty in 1i64.., t in any::<u64>(), n in 1u64.., e in any::<u64>(), w in any::<u64>()) {
            let c = TermCmd { kind, slot, side, okind: 1, flags: 0, seq, sym, px, qty, target_oid: t, nonce: n, boot_epoch: e, sent_wall_ns: w };
            let mut b = [0u8; REC_LEN];
            c.write_le(&mut b);
            prop_assert_eq!(TermCmd::read_le(&b), (c, [0; 3]));
        }
    }
}
