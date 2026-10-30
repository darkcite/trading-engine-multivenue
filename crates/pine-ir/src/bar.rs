// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The bar and the one permitted `dyn` (contract §3.9).

use crate::intent::{BrokerView, TradeRec};

/// One bar, one cache line. Also the row format of the P3 bar files: a bar
/// file is a 64-byte header followed by `Bar`s, mapped read-only and handed
/// to the VM as `&Bar` with no copy.
///
/// Units match the engine exactly (D18): prices and volume are `i64 ×1e6`;
/// times are UTC **milliseconds** (Pine's unit). The bar-boundary convention
/// is `core_time::barclock` (UTC, left-closed/right-open, weeks
/// Monday-anchored); `close_ms` is the **exclusive** end, which is also what
/// `time_close` means in a script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct Bar {
    /// Open price ×1e6.
    pub open_1e6: i64,
    /// High ×1e6.
    pub high_1e6: i64,
    /// Low ×1e6.
    pub low_1e6: i64,
    /// Close ×1e6.
    pub close_1e6: i64,
    /// Volume ×1e6, in the venue's native volume unit; saturates at
    /// `i64::MAX` and sets [`barflag::VOL_SAT`].
    pub volume_1e6: i64,
    /// Bar open, UTC ms.
    pub open_ms: i64,
    /// Bar close (exclusive), UTC ms.
    pub close_ms: i64,
    /// Bar index since the series start; advances by exactly 1 per present
    /// bar (a hole sets [`barflag::GAP_BEFORE`] on the next bar instead).
    pub index: u32,
    /// [`barflag`] bits.
    pub flags: u32,
}

/// [`Bar::flags`] bits, frozen. Bits 6..31 are reserved and must be zero.
pub mod barflag {
    /// The bar is closed (a history bar, or a live bar at its confirm). Clear on a live forming bar
    /// and on a **partial history bar** — the developing bar of a TV-mode history-tick run
    /// (`calc_on_every_history_tick`, R-P6; impl-05 §4.8.9), which also has `REALTIME` clear.
    pub const CONFIRMED: u32 = 1 << 0;
    /// At least one bucket is missing between this bar and the previous one.
    pub const GAP_BEFORE: u32 = 1 << 1;
    /// The bar comes from the live tape (a forming bar, or a live bar at its
    /// confirm) — `barstate.isrealtime`.
    pub const REALTIME: u32 = 1 << 2;
    /// The last bar of the loaded history — `barstate.islast` on history.
    pub const LAST: u32 = 1 << 3;
    /// The last confirmed history bar — `barstate.islastconfirmedhistory`.
    pub const LAST_CONFIRMED_HISTORY: u32 = 1 << 4;
    /// `volume_1e6` saturated at `i64::MAX`.
    pub const VOL_SAT: u32 = 1 << 5;
    /// Every defined bit.
    pub const DEFINED: u32 = (1 << 6) - 1;
}

/// A read-only view of one shared higher-timeframe series (law P4): the
/// outputs of one `SubProgram`, one element per HTF bar, oldest first.
///
/// The host owns the storage (a mirror ring, so the view is always
/// contiguous) and steps the sub-program; the parent reads through the
/// `request.security` builtin, which aligns the view to its own bar
/// (lookahead and gaps are Pine semantics and live in `pine-std`, once).
#[derive(Debug, Clone, Copy)]
pub struct SharedSeries<'a> {
    /// Output values, oldest first. Ints, bools and colours are exact in
    /// f64; `na` is NaN.
    pub values: &'a [f64],
    /// Open time (UTC ms) of each element's HTF bar; same length.
    pub open_ms: &'a [i64],
    /// Close time (UTC ms, exclusive) of each element's HTF bar; same length.
    pub close_ms: &'a [i64],
    /// Whether the last element is a confirmed HTF bar (`false`: it is the
    /// developing HTF bar, updated per coalesced tick in realtime).
    pub last_confirmed: bool,
}

/// What the host lends the VM for one evaluation. The **one** permitted
/// `dyn` in the lane (`&mut dyn BarCtx`), reached a handful of times per
/// evaluation from builtins and never from the dispatch loop.
///
/// There is no system clock anywhere behind this trait (law P5, extended by
/// B10 to the member's whole decision path).
pub trait BarCtx {
    /// The shared series `out_idx` of the sub-program whose content key is
    /// `key` (`SubProgram::key`). `None` when the host has none yet — the
    /// builtin then yields `na`.
    fn shared_series(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>>;

    /// The broker's read-back for `strategy.*` variables. `None` for an
    /// indicator, or a host with no broker attached.
    fn broker(&self) -> Option<&BrokerView>;

    /// Open trade `i` (`0` = oldest open), for `strategy.opentrades.*`.
    fn open_trade(&self, i: u32) -> Option<&TradeRec>;

    /// Closed trade `i` (`0` = oldest retained), for
    /// `strategy.closedtrades.*`. The broker retains the most recent
    /// `BrokerView::closed_retained` closed trades; an older index yields
    /// `None` → `na` (a documented, fixture-pinned limit).
    fn closed_trade(&self, i: u32) -> Option<&TradeRec>;

    /// Event-stream time of the current evaluation, UTC ms — `timenow`.
    /// Never a clock read: the host passes the time of the event being
    /// processed.
    fn now_ms(&self) -> i64;
}

const _: () = assert!(core::mem::size_of::<Bar>() == 64);
const _: () = assert!(core::mem::align_of::<Bar>() == 64);
