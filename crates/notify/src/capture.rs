// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Tailing the run's PMLR captures: a growing file of 64 B slots behind a
//! 64 B header (`core_io::pmlr`). The engine's capture thread flushes at
//! least every second; a slot only partly on disk is left for the next
//! poll (the torn tail). Records are decoded field by field at the offsets
//! `core_types` itself defines (`offset_of!`), so no byte pattern is ever
//! reinterpreted as a Rust enum.

use std::fs::File;
use std::io;
use std::mem::offset_of;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use core_io::pmlr::{SlotKind, HEADER_SIZE, MAGIC, SLOT_SIZE, VERSION};
use core_types::{Fill, Order, OrderEvent};

/// One raw slot.
pub type Slot = [u8; SLOT_SIZE];

/// Why a capture is not being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailErr {
    /// Not a PMLR file, a newer version than this binary knows, or the
    /// wrong slot kind. Sticky until the path changes.
    BadHeader(String),
    /// An I/O failure (retried next poll).
    Io(String),
}

impl std::fmt::Display for TailErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailErr::BadHeader(s) => write!(f, "capture header refused: {s}"),
            TailErr::Io(s) => write!(f, "capture i/o: {s}"),
        }
    }
}

/// A reader over one growing capture file.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    kind: SlotKind,
    consumed: u64,
    file: Option<File>,
    bad: Option<String>,
}

impl Tail {
    /// A tail of `path` that has already consumed `consumed` records.
    #[must_use]
    pub fn new(path: PathBuf, kind: SlotKind, consumed: u64) -> Tail {
        Tail {
            path,
            kind,
            consumed,
            file: None,
            bad: None,
        }
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records consumed so far.
    #[must_use]
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Skip to the current end of the file (first start: history is not
    /// replayed as messages).
    ///
    /// # Errors
    /// As [`Tail::poll`].
    pub fn skip_to_end(&mut self) -> Result<(), TailErr> {
        let avail = self.available()?;
        self.consumed = avail;
        Ok(())
    }

    fn open(&mut self) -> Result<Option<&File>, TailErr> {
        if let Some(b) = &self.bad {
            return Err(TailErr::BadHeader(b.clone()));
        }
        if self.file.is_none() {
            let f = match File::open(&self.path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(TailErr::Io(e.to_string())),
            };
            let mut h = [0u8; HEADER_SIZE];
            match f.read_exact_at(&mut h, 0) {
                Ok(()) => {}
                // The header is not on disk yet: try again next poll.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(TailErr::Io(e.to_string())),
            }
            if h[0..4] != MAGIC {
                return Err(self.refuse("bad magic".to_string()));
            }
            let ver = u16::from_le_bytes([h[4], h[5]]);
            if ver > VERSION {
                return Err(self.refuse(format!(
                    "version {ver} is newer than this binary's {VERSION}"
                )));
            }
            if h[6] != self.kind.to_u8() {
                return Err(self.refuse(format!(
                    "slot kind {} where {} was expected",
                    h[6],
                    self.kind.to_u8()
                )));
            }
            if self.kind.slot_size() != SLOT_SIZE {
                return Err(self.refuse("not a 64 B slot kind".to_string()));
            }
            self.file = Some(f);
        }
        Ok(self.file.as_ref())
    }

    fn refuse(&mut self, why: String) -> TailErr {
        self.bad = Some(why.clone());
        TailErr::BadHeader(why)
    }

    fn available(&mut self) -> Result<u64, TailErr> {
        let Some(f) = self.open()? else {
            return Ok(0);
        };
        let len = f.metadata().map_err(|e| TailErr::Io(e.to_string()))?.len();
        Ok(len.saturating_sub(HEADER_SIZE as u64) / SLOT_SIZE as u64)
    }

    /// Append up to `max` new complete records to `out`; returns how many.
    ///
    /// # Errors
    /// [`TailErr`]; a missing file is `Ok(0)`.
    pub fn poll(&mut self, max: usize, out: &mut Vec<Slot>) -> Result<usize, TailErr> {
        let avail = self.available()?;
        if avail < self.consumed {
            // The file was replaced by a shorter one: start it over.
            self.consumed = 0;
            self.file = None;
            return Ok(0);
        }
        let n = usize::try_from((avail - self.consumed).min(max as u64)).unwrap_or(max);
        if n == 0 {
            return Ok(0);
        }
        let Some(f) = self.file.as_ref() else {
            return Ok(0);
        };
        let mut buf = vec![0u8; n * SLOT_SIZE];
        let off = HEADER_SIZE as u64 + self.consumed * SLOT_SIZE as u64;
        f.read_exact_at(&mut buf, off)
            .map_err(|e| TailErr::Io(e.to_string()))?;
        for chunk in buf.chunks_exact(SLOT_SIZE) {
            let mut s = [0u8; SLOT_SIZE];
            s.copy_from_slice(chunk);
            out.push(s);
        }
        self.consumed += n as u64;
        Ok(n)
    }
}

fn u64_at(b: &Slot, off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

fn i64_at(b: &Slot, off: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    i64::from_le_bytes(a)
}

fn u32_at(b: &Slot, off: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    u32::from_le_bytes(a)
}

/// `Fill.flags`: the venue settled the instance (`core_types`, pinned).
pub const FILL_FLAG_SETTLEMENT: u8 = 1 << 0;
/// `Fill.flags`: adopted inventory at boot.
pub const FILL_FLAG_ADOPTED: u8 = 1 << 1;
/// `Fill.origin`: modelled by the paper dispatcher.
pub const FILL_ORIGIN_PAPER: u8 = 1;
/// `Order.verb`: a place.
pub const ORDER_VERB_PLACE: u8 = 0;
/// `OrderEvent.kind`: the order rests.
pub const OEV_RESTING: u8 = 1;
/// `OrderEvent.kind`: refused (or an IoC that did not match).
pub const OEV_REJECTED: u8 = 2;
/// `OrderEvent.kind`: cancelled.
pub const OEV_CANCELED: u8 = 3;
/// `OrderEvent.kind`: filled.
pub const OEV_FILLED: u8 = 4;
/// `OrderEvent.reason`: the venue's IoC "could not immediately match"
/// (`exec_hyperliquid::event::REASON_IOC_MISS`, pinned).
pub const REASON_IOC_MISS: u8 = 12;

/// A decoded `Fill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillRec {
    /// Venue wall ns for a live fill; the engine clock for a paper one.
    pub ts_ns: u64,
    /// Symbol.
    pub sym: u32,
    /// 0 bid (buy), 1 ask (sell).
    pub side: u8,
    /// Slot, 0xFF = unattributed.
    pub strategy_id: u8,
    /// 0 venue, 1 paper.
    pub origin: u8,
    /// `FILL_FLAG_*`.
    pub flags: u8,
    /// Price ×1e6.
    pub px: i64,
    /// Quantity ×1e6 (contracts).
    pub qty: i64,
    /// Our client oid for a trade; the venue's oid for a settlement.
    pub order_id: u64,
}

impl FillRec {
    /// Decode a slot.
    #[must_use]
    pub fn decode(b: &Slot) -> FillRec {
        FillRec {
            ts_ns: u64_at(b, offset_of!(Fill, ts_ns)),
            sym: u32_at(b, offset_of!(Fill, sym)),
            side: b[offset_of!(Fill, side)],
            strategy_id: b[offset_of!(Fill, strategy_id)],
            origin: b[offset_of!(Fill, origin)],
            flags: b[offset_of!(Fill, flags)],
            px: i64_at(b, offset_of!(Fill, px)),
            qty: i64_at(b, offset_of!(Fill, qty)),
            order_id: u64_at(b, offset_of!(Fill, order_id)),
        }
    }

    /// A venue settlement.
    #[must_use]
    pub fn is_settlement(&self) -> bool {
        self.flags & FILL_FLAG_SETTLEMENT != 0
    }

    /// Adopted inventory.
    #[must_use]
    pub fn is_adopted(&self) -> bool {
        self.flags & FILL_FLAG_ADOPTED != 0
    }

    /// Modelled, not reported by a venue.
    #[must_use]
    pub fn is_paper(&self) -> bool {
        self.origin == FILL_ORIGIN_PAPER
    }
}

/// A decoded `Order`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderRec {
    /// Engine clock.
    pub ts_ns: u64,
    /// Symbol.
    pub sym: u32,
    /// 0 bid, 1 ask.
    pub side: u8,
    /// Price ×1e6 (the limit).
    pub px: i64,
    /// Quantity ×1e6.
    pub qty: i64,
    /// LAW E-9's durable name.
    pub client_oid: u64,
    /// Slot.
    pub strategy_id: u8,
    /// Place / cancel / modify.
    pub verb: u8,
}

impl OrderRec {
    /// Decode a slot.
    #[must_use]
    pub fn decode(b: &Slot) -> OrderRec {
        OrderRec {
            ts_ns: u64_at(b, offset_of!(Order, ts_ns)),
            sym: u32_at(b, offset_of!(Order, sym)),
            side: b[offset_of!(Order, side)],
            px: i64_at(b, offset_of!(Order, px)),
            qty: i64_at(b, offset_of!(Order, qty)),
            client_oid: u64_at(b, offset_of!(Order, client_oid)),
            strategy_id: b[offset_of!(Order, strategy_id)],
            verb: b[offset_of!(Order, verb)],
        }
    }
}

/// A decoded `OrderEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRec {
    /// Engine clock.
    pub ts_ns: u64,
    /// The order's client oid.
    pub client_oid: u64,
    /// Symbol.
    pub sym: u32,
    /// Slot.
    pub strategy_id: u8,
    /// `OEV_*`.
    pub kind: u8,
    /// `ORDER_EVENT_REASON_*` / the arm's reasons.
    pub reason: u8,
}

impl EventRec {
    /// Decode a slot.
    #[must_use]
    pub fn decode(b: &Slot) -> EventRec {
        EventRec {
            ts_ns: u64_at(b, offset_of!(OrderEvent, ts_ns)),
            client_oid: u64_at(b, offset_of!(OrderEvent, client_oid)),
            sym: u32_at(b, offset_of!(OrderEvent, sym)),
            strategy_id: b[offset_of!(OrderEvent, strategy_id)],
            kind: b[offset_of!(OrderEvent, kind)],
            reason: b[offset_of!(OrderEvent, reason)],
        }
    }
}

/// The word for an order-event reason byte.
#[must_use]
pub fn reason_word(r: u8) -> &'static str {
    match r {
        0 => "none",
        1 => "cancel-requested",
        2 => "replaced",
        3 => "expired",
        4 => "bad-alo-px",
        5 => "self-trade",
        6 => "margin",
        7 => "open-interest-cap",
        8 => "scheduled-cancel",
        9 => "reduce-only",
        10 => "tick-or-lot",
        11 => "min-notional",
        12 => "ioc-miss",
        13 => "not-found",
        14 => "rate-limit",
        15 => "transport",
        16 => "malformed",
        17 => "halted",
        18 => "local",
        19 => "settlement",
        _ => "other",
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    //! COPY-DOCTRINE: test-only (declared under `#[cfg(test)]`).
    use super::Slot;
    use core_io::pmlr::{SlotKind, HEADER_SIZE, MAGIC, VERSION};
    use core_types::AsBytes;
    use std::io::Write;
    use std::path::Path;

    /// The bytes of a `core_types` POD.
    pub fn bytes_of<T: AsBytes>(v: &T) -> Slot {
        assert_eq!(std::mem::size_of::<T>(), 64);
        let mut out = [0u8; 64];
        // SAFETY: `AsBytes` guarantees every one of the 64 bytes of `T` is
        // initialized (explicit padding, `#[repr(C, align(64))]`); we read
        // exactly `size_of::<T>()` = 64 of them into a plain array.
        let src = unsafe { std::slice::from_raw_parts(std::ptr::addr_of!(*v).cast::<u8>(), 64) };
        out.copy_from_slice(src);
        out
    }

    /// Write a PMLR header.
    pub fn write_header(path: &Path, kind: SlotKind) {
        let mut h = [0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(&MAGIC);
        h[4..6].copy_from_slice(&VERSION.to_le_bytes());
        h[6] = kind.to_u8();
        std::fs::write(path, h).unwrap();
    }

    /// Append raw bytes.
    pub fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(bytes).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{append, bytes_of, write_header};
    use super::*;
    use core_types::{Price, Qty, Side, VenueId};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notify-cap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn decodes_core_types_records_at_their_own_offsets() {
        let f = Fill::new(
            1_792_846_648_013_000_000,
            0x0400_1002,
            Side::Ask,
            Price(1_000_000),
            Qty(42_000_000),
            77,
        )
        .with_attribution(3, 0)
        .with_flags(core_types::FILL_FLAG_SETTLEMENT);
        let r = FillRec::decode(&bytes_of(&f));
        assert_eq!(r.ts_ns, 1_792_846_648_013_000_000);
        assert_eq!(
            (r.sym, r.side, r.strategy_id, r.origin),
            (0x0400_1002, 1, 3, 0)
        );
        assert!(r.is_settlement() && !r.is_adopted() && !r.is_paper());
        assert_eq!((r.px, r.qty, r.order_id), (1_000_000, 42_000_000, 77));

        let mut o = Order::new(
            5,
            VenueId::Hyperliquid,
            0x0400_1000,
            Side::Bid,
            0,
            Price(940_000),
            Qty(42_000_000),
            9001,
        );
        o.strategy_id = 3;
        let r = OrderRec::decode(&bytes_of(&o));
        assert_eq!(
            (
                r.ts_ns,
                r.sym,
                r.side,
                r.px,
                r.qty,
                r.client_oid,
                r.strategy_id,
                r.verb
            ),
            (5, 0x0400_1000, 0, 940_000, 42_000_000, 9001, 3, 0)
        );

        let e = OrderEvent::new(
            6,
            VenueId::Hyperliquid,
            0x0400_1000,
            9001,
            3,
            core_types::ORDER_EVENT_REJECTED,
            12,
            0,
        );
        let r = EventRec::decode(&bytes_of(&e));
        assert_eq!(
            (
                r.ts_ns,
                r.client_oid,
                r.sym,
                r.strategy_id,
                r.kind,
                r.reason
            ),
            (6, 9001, 0x0400_1000, 3, OEV_REJECTED, REASON_IOC_MISS)
        );
    }

    #[test]
    fn constants_match_core_types_and_the_hl_arm() {
        assert_eq!(FILL_FLAG_SETTLEMENT, core_types::FILL_FLAG_SETTLEMENT);
        assert_eq!(FILL_FLAG_ADOPTED, core_types::FILL_FLAG_ADOPTED);
        assert_eq!(FILL_ORIGIN_PAPER, core_types::FILL_ORIGIN_PAPER);
        assert_eq!(ORDER_VERB_PLACE, core_types::ORDER_VERB_PLACE);
        assert_eq!(OEV_RESTING, core_types::ORDER_EVENT_RESTING);
        assert_eq!(OEV_REJECTED, core_types::ORDER_EVENT_REJECTED);
        assert_eq!(OEV_CANCELED, core_types::ORDER_EVENT_CANCELED);
        assert_eq!(OEV_FILLED, core_types::ORDER_EVENT_FILLED);
        use exec_hyperliquid::event as ev;
        assert_eq!(REASON_IOC_MISS, ev::REASON_IOC_MISS);
        assert_eq!(reason_word(ev::REASON_NOT_FOUND), "not-found");
        assert_eq!(reason_word(ev::REASON_RATE_LIMIT), "rate-limit");
        assert_eq!(reason_word(ev::REASON_TRANSPORT), "transport");
        assert_eq!(reason_word(ev::REASON_MALFORMED), "malformed");
        assert_eq!(reason_word(ev::REASON_HALTED), "halted");
        assert_eq!(reason_word(ev::REASON_LOCAL), "local");
        assert_eq!(reason_word(ev::REASON_SETTLEMENT), "settlement");
        assert_eq!(
            reason_word(core_types::ORDER_EVENT_REASON_MIN_NOTIONAL),
            "min-notional"
        );
        assert_eq!(reason_word(core_types::ORDER_EVENT_REASON_OTHER), "other");
    }

    #[test]
    fn tails_a_growing_file_and_leaves_the_torn_tail() {
        let d = tmpdir("grow");
        let p = d.join("engine-fills.pmlr");
        let mut t = Tail::new(p.clone(), SlotKind::Fill, 0);
        let mut out = Vec::new();
        assert_eq!(t.poll(100, &mut out).unwrap(), 0, "missing file is empty");
        write_header(&p, SlotKind::Fill);
        let f = Fill::new(1, 2, Side::Bid, Price(3), Qty(4), 5);
        let s = bytes_of(&f);
        append(&p, &s);
        append(&p, &s[..30]); // a torn slot
        assert_eq!(t.poll(100, &mut out).unwrap(), 1);
        assert_eq!(t.poll(100, &mut out).unwrap(), 0);
        append(&p, &s[30..]);
        append(&p, &s);
        assert_eq!(t.poll(1, &mut out).unwrap(), 1, "max bounds a poll");
        assert_eq!(t.poll(100, &mut out).unwrap(), 1);
        assert_eq!(out.len(), 3);
        assert_eq!(t.consumed(), 3);
        // A resumed tail starts where it left off.
        let mut t2 = Tail::new(p.clone(), SlotKind::Fill, 2);
        let mut o2 = Vec::new();
        assert_eq!(t2.poll(100, &mut o2).unwrap(), 1);
        let mut t3 = Tail::new(p, SlotKind::Fill, 0);
        t3.skip_to_end().unwrap();
        assert_eq!(t3.consumed(), 3);
    }

    #[test]
    fn refuses_a_wrong_kind_or_a_newer_version() {
        let d = tmpdir("bad");
        let p = d.join("x.pmlr");
        write_header(&p, SlotKind::Order);
        let mut t = Tail::new(p.clone(), SlotKind::Fill, 0);
        let mut out = Vec::new();
        assert!(matches!(t.poll(10, &mut out), Err(TailErr::BadHeader(_))));
        assert!(
            matches!(t.poll(10, &mut out), Err(TailErr::BadHeader(_))),
            "sticky"
        );
        let mut h = std::fs::read(&p).unwrap();
        h[4..6].copy_from_slice(&(VERSION + 1).to_le_bytes());
        h[6] = SlotKind::Fill.to_u8();
        std::fs::write(&p, &h).unwrap();
        let mut t = Tail::new(p, SlotKind::Fill, 0);
        assert!(matches!(t.poll(10, &mut out), Err(TailErr::BadHeader(_))));
    }
}
