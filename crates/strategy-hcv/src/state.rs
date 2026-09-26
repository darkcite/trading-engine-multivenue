// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # HC11b — the slot-7 book, persisted across restarts (`hcv-state.tsv`)
//!
//! The engine restarts three times a UTC day (00:10, 08:33, 16:05Z) and an
//! option position lives for days: a book kept only in memory is options
//! nobody hedges or settles after the next restart, and a P&L that starts
//! again from zero. What outlives the process:
//!
//! * every held option — by CONTRACT (underlying, expiry, strike, right),
//!   never by symbol: a Hypercall ordinal is this boot's, and the next
//!   boot's chain renumbers — with its entry basis and last traded price;
//! * each underlying's hedge perp position;
//! * the cash, and the UTC day with its opening mark (the day's stop);
//! * every live settlement window — its grid, run-length encoded, or its
//!   price once fixed — so a restart inside the final 30 minutes does not
//!   settle on half a window, and one between `T` and the booking does not
//!   lose the fixed price.
//!
//! What does not: the IoCs in flight (the X1 law — the paper matcher's
//! table dies with the process, so an order in flight is dead by
//! construction), quotes, touches, the oracle, σ̂ and the calendar (all
//! re-read live), and the counters.
//!
//! ## Writing — off the engine thread (the H3.7 law)
//!
//! Every change that outlives the process moves the member's state epoch:
//! a fill, a booking, a window opened, fixed or freed, a window's sample in
//! a new minute, the UTC day. At its timer, while the epoch moved, the
//! member copies its book into the [`core_ring::Mailbox`] the cli's writer
//! thread drains ([`HcvStrategy::install_state_outbox`]) — the engine
//! thread's whole share of a write; the writer renders ([`render_state`])
//! and fsyncs. A slot the writer still holds is offered again at the next
//! timer, so the newest book follows. The shutdown writes once more,
//! synchronously, after the writer is joined.
//!
//! ## Restoring — fail closed
//!
//! [`HcvStrategy::restore_state`] reads the file back after `configure`.
//! A row the member cannot place — an underlying `hcv.toml` does not trade,
//! a malformed field or an implausible value, an unknown tag, a duplicate —
//! refuses the boot: a position nobody hedges or settles is worse than a
//! boot that asks for a hand (the VRP and XSD law). One exception: a hedge
//! residual under the venue's minimum on an underlying no longer traded
//! (one the member could never close) is dropped and counted. A held
//! contract outside this boot's chain (a strike the capped chain no longer
//! reaches) is an ORPHAN: carried by its terms — hedged and marked at σ̂
//! (or, without one, at the implied vol of its expiry's nearest quoted
//! strike), sampled and settled at expiry — and never traded, until a
//! later boot's chain lists it again.
//!
//! The marks come back with the feeds, not the file: until every held
//! underlying's oracle has arrived the book's mark is unknown, and new
//! risk waits for it (`HcvStrategy::marked_pnl`).

use core::fmt::Write as _;

use core_settle::{GRID_POINTS, SETTLE_WINDOW_MS};
use core_types::{NsTs, SYMBOL_ID_NONE};

use crate::{HcvOpt, HcvStrategy, OptState, HCV_MAX_OPTIONS, HCV_MAX_UND, HCV_SETTLE_SLOTS, NAME_MAX};

/// `hcv-state.tsv` grammar version.
pub const HCV_STATE_VERSION: u32 = 1;
/// Held contracts outside the booted chain one member carries (module doc).
pub const HCV_MAX_ORPHANS: usize = 64;
/// Option rows one snapshot holds: every row the member's table can have.
pub const HCV_SNAP_POSITIONS: usize = HCV_MAX_OPTIONS + HCV_MAX_ORPHANS;
/// The largest magnitude a ×1e6 field of the file may carry: 1e15, a
/// billion dollars or units — far past any book the member can hold, and
/// far enough inside `i64` that no product or `abs` in its arithmetic
/// overflows.
const MAX_ABS_1E6: i64 = 1_000_000_000_000_000;
/// The last expiry a row may name: 2100-01-01T00:00Z, ms.
const MAX_EXP_MS: i64 = 4_102_444_800_000;
/// A UTC day, ms (the `D` row's unit).
const DAY_MS_I: i64 = 86_400_000;

/// `|x|` past [`MAX_ABS_1E6`] — `i64::MIN` included, which `abs` cannot
/// carry.
#[inline]
const fn too_big(x: i64) -> bool {
    x.unsigned_abs() > MAX_ABS_1E6 as u64
}

/// A moved book the writer has not taken for this long stops new risk: the
/// writer retries a failed write every 5 s, so this is six failures in a
/// row — a book that cannot persist must not grow (HC11b review).
pub const HCV_BOOK_STALE_NS: u64 = 30_000_000_000;

/// One held option, by contract (module doc). POD.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HcvPosRow {
    /// Expiry, wall ms.
    pub exp_ms: u64,
    /// Strike, USD ×1e6.
    pub strike_1e6: i64,
    /// Contracts ×1e6, signed (+ long).
    pub pos_1e6: i64,
    /// The entry basis, USD ×1e6.
    pub avg_px_1e6: i64,
    /// The last traded price, USD ×1e6.
    pub last_px_1e6: i64,
    /// The underlying's index in the member's table.
    pub und: u8,
    /// A call (else a put).
    pub call: bool,
}

impl HcvPosRow {
    const EMPTY: Self = Self {
        exp_ms: 0,
        strike_1e6: 0,
        pos_1e6: 0,
        avg_px_1e6: 0,
        last_px_1e6: 0,
        und: 0,
        call: false,
    };
}

/// One live settlement window (14.4 KiB).
#[repr(C)]
#[derive(Clone)]
pub struct HcvWindowRow {
    /// Expiry, wall ms.
    pub exp_ms: u64,
    /// The settlement once fixed: `> 0` the window's, `-1` none reached
    /// it, `0` still open.
    pub px_1e6: i64,
    /// The last accepted print's stamp, wall ms.
    pub last_ts_ms: u64,
    /// The next grid index to fill.
    pub next: u32,
    /// Grid points held — `samples[..n]`; 0 once the price is fixed (the
    /// price is all a fixed window still needs).
    pub n: u32,
    /// The underlying's index.
    pub und: u8,
    /// The grid, time order.
    pub samples: [i64; GRID_POINTS],
}

impl HcvWindowRow {
    const EMPTY: Self = Self {
        exp_ms: 0,
        px_1e6: 0,
        last_ts_ms: 0,
        next: 0,
        n: 0,
        und: 0,
        samples: [0; GRID_POINTS],
    };
}

/// The member's whole persisted book at one instant: the writer's mailbox
/// slot, boxed once at boot (~170 KiB).
pub struct HcvStateSnap {
    /// The state epoch it was taken at.
    pub epoch: u64,
    /// Cash, USD ×1e6.
    pub cash_usd_1e6: i64,
    /// The UTC day of the opening mark below (days since the epoch).
    pub day: u64,
    /// The day's opening marked P&L, USD ×1e6.
    pub day_start_pnl_usd_1e6: i64,
    /// Underlyings in the table.
    pub n_und: u8,
    /// Their names (`hcv.toml`'s Hypercall names — the file's keys).
    pub und_names: [[u8; NAME_MAX]; HCV_MAX_UND],
    /// …their live lengths.
    pub und_name_len: [u8; HCV_MAX_UND],
    /// Each underlying's hedge perp position, units ×1e6.
    pub hedge_pos_1e6: [i64; HCV_MAX_UND],
    /// …and its oracle when the book was taken, USD ×1e6 — the last one
    /// known, carried from the restored book until this process's first
    /// Mark (0 = none ever): the next boot's measure of a residual it may
    /// drop (module doc).
    pub oracle_1e6: [i64; HCV_MAX_UND],
    /// Held options in `pos`.
    pub n_pos: u32,
    /// The held options.
    pub pos: [HcvPosRow; HCV_SNAP_POSITIONS],
    /// Live windows in `win`.
    pub n_win: u32,
    /// The live settlement windows.
    pub win: [HcvWindowRow; HCV_SETTLE_SLOTS],
}

impl HcvStateSnap {
    /// An empty book, boxed (boot only).
    #[must_use]
    pub fn new_boxed() -> Box<Self> {
        Box::new(Self {
            epoch: 0,
            cash_usd_1e6: 0,
            day: 0,
            day_start_pnl_usd_1e6: 0,
            n_und: 0,
            und_names: [[0; NAME_MAX]; HCV_MAX_UND],
            und_name_len: [0; HCV_MAX_UND],
            hedge_pos_1e6: [0; HCV_MAX_UND],
            oracle_1e6: [0; HCV_MAX_UND],
            n_pos: 0,
            pos: [HcvPosRow::EMPTY; HCV_SNAP_POSITIONS],
            n_win: 0,
            win: [HcvWindowRow::EMPTY; HCV_SETTLE_SLOTS],
        })
    }

    /// Underlying `u`'s name as the file writes it.
    fn und_name(&self, u: u8) -> &str {
        let u = u as usize;
        if u >= (self.n_und as usize).min(HCV_MAX_UND) {
            debug_assert!(false, "a book row names no underlying");
            return "";
        }
        let len = (self.und_name_len[u] as usize).min(NAME_MAX);
        core::str::from_utf8(&self.und_names[u][..len]).unwrap_or("")
    }
}

/// What a boot restored (the tell).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HcvRestored {
    /// Held options restored.
    pub positions: u32,
    /// …of which outside this boot's chain (carried by their terms).
    pub orphans: u32,
    /// …of which already past their expiry (booked at the first timer:
    /// on their window if one was kept, else counted as a fallback).
    pub expired: u32,
    /// Hedge positions restored (non-zero).
    pub hedges: u32,
    /// Hedge residuals under the venue's minimum on an underlying no longer
    /// traded, dropped (module doc).
    pub hedges_dropped: u32,
    /// Settlement windows restored.
    pub windows: u32,
    /// The cash restored, USD ×1e6.
    pub cash_usd_1e6: i64,
}

/// Why a state file was refused: its line (0 = the file as a whole) and
/// what is wrong with it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HcvStateError {
    /// 1-based line, or 0.
    pub line: usize,
    /// What is wrong.
    pub what: &'static str,
}

impl core::fmt::Display for HcvStateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line == 0 {
            write!(f, "hcv-state.tsv: {}", self.what)
        } else {
            write!(f, "hcv-state.tsv line {}: {}", self.line, self.what)
        }
    }
}

const fn refused(line: usize, what: &'static str) -> HcvStateError {
    HcvStateError { line, what }
}

/// Render `snap` as `hcv-state.tsv` into `out` (cleared first). Cold: the
/// writer thread, and the shutdown's forced write.
pub fn render_state(snap: &HcvStateSnap, out: &mut String) {
    out.clear();
    out.push_str(
        "# hcv-state.tsv — slot 7's book, written by the engine, read at boot. Not for hand editing.\n\
         # V version | C cash_usd_1e6 | D utc_day day_start_pnl_usd_1e6\n\
         # P underlying expiry_ms strike_1e6 right(C/P) pos_1e6 avg_px_1e6 last_px_1e6\n\
         # H underlying hedge_pos_1e6 oracle_1e6(when written; 0 = none)\n\
         # W underlying expiry_ms px_1e6(0 open / -1 none / >0 fixed) next n last_ts_ms,\n\
         #   then S px_1e6 run: the window's grid, run-length (the runs sum to n)\n",
    );
    let _ = writeln!(out, "V\t{HCV_STATE_VERSION}");
    let _ = writeln!(out, "C\t{}", snap.cash_usd_1e6);
    let _ = writeln!(out, "D\t{}\t{}", snap.day, snap.day_start_pnl_usd_1e6);
    let n_pos = (snap.n_pos as usize).min(HCV_SNAP_POSITIONS);
    let mut i = 0usize;
    while i < n_pos {
        let r = &snap.pos[i];
        let _ = writeln!(
            out,
            "P\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            snap.und_name(r.und),
            r.exp_ms,
            r.strike_1e6,
            if r.call { 'C' } else { 'P' },
            r.pos_1e6,
            r.avg_px_1e6,
            r.last_px_1e6
        );
        i += 1;
    }
    let n_und = (snap.n_und as usize).min(HCV_MAX_UND);
    let mut u = 0usize;
    while u < n_und {
        if snap.hedge_pos_1e6[u] != 0 {
            let _ = writeln!(
                out,
                "H\t{}\t{}\t{}",
                snap.und_name(u as u8),
                snap.hedge_pos_1e6[u],
                snap.oracle_1e6[u]
            );
        }
        u += 1;
    }
    let n_win = (snap.n_win as usize).min(HCV_SETTLE_SLOTS);
    let mut w = 0usize;
    while w < n_win {
        let r = &snap.win[w];
        let n = (r.n as usize).min(GRID_POINTS);
        let _ = writeln!(
            out,
            "W\t{}\t{}\t{}\t{}\t{n}\t{}",
            snap.und_name(r.und),
            r.exp_ms,
            r.px_1e6,
            r.next,
            r.last_ts_ms
        );
        // The oracle holds between prints: a 1 s grid is mostly runs.
        let mut i = 0usize;
        while i < n {
            let px = r.samples[i];
            let mut j = i + 1;
            while j < n && r.samples[j] == px {
                j += 1;
            }
            let _ = writeln!(out, "S\t{px}\t{}", j - i);
            i = j;
        }
        w += 1;
    }
}

/// One `W` row and its grid, parsed.
struct ParsedWindow {
    line: usize,
    und: u8,
    exp_ms: u64,
    px_1e6: i64,
    next: u32,
    last_ts_ms: u64,
    samples: Vec<i64>,
}

/// A whole file, parsed and checked — nothing of the member touched yet.
struct ParsedBook {
    cash_usd_1e6: i64,
    day: u64,
    day_start_pnl_usd_1e6: i64,
    pos: Vec<(usize, HcvPosRow)>,
    /// (underlying, position, the mark it was written with).
    hedges: Vec<(u8, i64, i64)>,
    hedges_dropped: u32,
    windows: Vec<ParsedWindow>,
}

/// The fields of one row, strictly: each asked for must be there, and
/// none may be left over.
struct Fields<'a> {
    it: core::str::Split<'a, char>,
    line: usize,
}

impl<'a> Fields<'a> {
    fn text(&mut self) -> Result<&'a str, HcvStateError> {
        self.it.next().ok_or(refused(self.line, "a short row"))
    }

    fn int(&mut self) -> Result<i64, HcvStateError> {
        self.text()?
            .parse::<i64>()
            .map_err(|_| refused(self.line, "a field is not an integer"))
    }

    fn end(mut self) -> Result<(), HcvStateError> {
        match self.it.next() {
            None => Ok(()),
            Some(_) => Err(refused(self.line, "a row with a field too many")),
        }
    }
}

impl HcvStrategy {
    /// HC11b: move the state epoch — something that outlives the process
    /// changed ([`crate::state`]).
    #[inline]
    pub(crate) fn bump_state(&mut self) {
        self.state_epoch = self.state_epoch.wrapping_add(1);
    }

    /// Boot: from now on hand the book to the cli's writer thread through
    /// `tx` whenever it changes. The book the restore left counts as
    /// handed: a boot that changed nothing writes nothing.
    pub fn install_state_outbox(&mut self, tx: core_ring::MailboxTx<HcvStateSnap>) {
        self.offered_epoch = self.state_epoch;
        self.state_tx = Some(tx);
    }

    /// At the timer: while the epoch moved, copy the book into the writer's
    /// slot — the engine thread's whole share of a write. A slot the writer
    /// still holds (a write in flight, or failing) is tried again at the
    /// next timer, so the newest book follows; one held since
    /// [`HCV_BOOK_STALE_NS`] stops new risk ([`Self::book_stale`]). Never
    /// waits, never allocates.
    pub(crate) fn offer_state(&mut self, now_ns: NsTs) {
        if self.state_epoch == self.offered_epoch {
            return;
        }
        let Some(mut tx) = self.state_tx.take() else {
            return;
        };
        if let Some(mut slot) = tx.try_fill() {
            self.fill_state(&mut slot);
            slot.commit();
            self.offered_epoch = self.state_epoch;
            self.offer_blocked_ns = 0;
        } else if self.offer_blocked_ns == 0 {
            self.offer_blocked_ns = now_ns.max(1);
        }
        self.state_tx = Some(tx);
    }

    /// A moved book has found the writer's slot held for
    /// [`HCV_BOOK_STALE_NS`]: the file is behind the book, and a restart
    /// would lose the difference.
    #[inline]
    pub(crate) fn book_stale(&self, now_ns: NsTs) -> bool {
        self.offer_blocked_ns != 0 && now_ns.saturating_sub(self.offer_blocked_ns) >= HCV_BOOK_STALE_NS
    }

    /// Copy the book into `out` (the rows in use, not the arrays).
    pub(crate) fn fill_state(&self, out: &mut HcvStateSnap) {
        out.epoch = self.state_epoch;
        out.cash_usd_1e6 = self.cash_usd_1e6;
        out.day = self.day;
        out.day_start_pnl_usd_1e6 = self.day_start_pnl_usd_1e6;
        out.n_und = self.n_und as u8;
        // COPY: the underlying names (10 × 16 B), once per hand-off — the
        // snapshot is read on another thread, so it carries its own keys —
        // rejected: a shared name table (the writer would read the member's
        // memory while the engine runs).
        out.und_names = self.und_names;
        out.und_name_len = self.und_name_len;
        let mut u = 0usize;
        while u < HCV_MAX_UND {
            let st = &self.und[u];
            out.hedge_pos_1e6[u] = st.hedge_pos_1e6;
            out.oracle_1e6[u] = if st.oracle_1e6 > 0 { st.oracle_1e6 } else { st.kept_mark_1e6 };
            u += 1;
        }
        debug_assert!(self.n_opts <= HCV_SNAP_POSITIONS, "the table outgrew the snapshot");
        let mut n = 0usize;
        let mut i = 0usize;
        while i < self.n_opts && n < HCV_SNAP_POSITIONS {
            let o = &self.opts[i];
            if o.pos_1e6 != 0 {
                out.pos[n] = HcvPosRow {
                    exp_ms: o.cfg.exp_ms,
                    strike_1e6: o.cfg.strike_1e6,
                    pos_1e6: o.pos_1e6,
                    avg_px_1e6: o.avg_px_1e6,
                    last_px_1e6: o.last_px_1e6,
                    und: o.cfg.und,
                    call: o.cfg.call,
                };
                n += 1;
            }
            i += 1;
        }
        out.n_pos = n as u32;
        let mut w = 0usize;
        let mut k = 0usize;
        while k < self.settle.len() && w < HCV_SETTLE_SLOTS {
            let s = &self.settle[k];
            if s.live {
                let row = &mut out.win[w];
                row.und = s.und;
                row.exp_ms = s.exp_ms;
                row.px_1e6 = s.px_1e6;
                row.last_ts_ms = s.window.last_ts_ms();
                row.next = s.window.next_index();
                // A fixed window needs only its price.
                let pts = if s.px_1e6 == 0 { s.window.samples() } else { &[] };
                row.n = pts.len() as u32;
                // COPY: an open window's grid (≤ 14.4 KiB; ≤ 8 windows) into
                // the writer's slot, only when the epoch moved — at most once
                // a timer (a fill between timers moves it too) — the member
                // keeps sampling into its own — rejected: lending the live
                // window to another thread.
                row.samples[..pts.len()].copy_from_slice(pts);
                w += 1;
            }
            k += 1;
        }
        out.n_win = w as u32;
    }

    /// Boot, after [`Self::configure`] and before the first event: read
    /// back the book a previous process wrote ([`crate::state`]: what is
    /// kept, the orphan law). `now_ms` (wall) only sorts out what expired
    /// while the engine was down, for the tell.
    ///
    /// # Errors
    ///
    /// Fail closed — the line and what is wrong; the member is unchanged.
    pub fn restore_state(&mut self, text: &str, now_ms: u64) -> Result<HcvRestored, HcvStateError> {
        if !self.configured {
            return Err(refused(0, "the book is restored after configure"));
        }
        if self.state_restored {
            return Err(refused(0, "the book was restored once already"));
        }
        let book = self.parse_book(text)?;
        // Place each held contract: the chain's own row, else an orphan.
        let chain = self.n_opts;
        let mut at: Vec<usize> = Vec::with_capacity(book.pos.len());
        let mut orphans = 0usize;
        let mut i = 0usize;
        while i < book.pos.len() {
            let (line, r) = book.pos[i];
            match self.chain_row(&r) {
                Some(k) => at.push(k),
                None => {
                    orphans += 1;
                    if orphans > HCV_MAX_ORPHANS {
                        return Err(refused(line, "more held contracts outside the chain than the member carries"));
                    }
                    at.push(usize::MAX);
                }
            }
            i += 1;
        }
        // Nothing above touched the member, and nothing below fails.
        if orphans > 0 {
            let mut table: Vec<OptState> = Vec::with_capacity(chain + orphans);
            // COPY: the chain's option rows (≤ 1 024 × 144 B) into the
            // widened table, once at boot — every loop of the member walks
            // one table — rejected: a second table for orphans (each loop
            // would walk two).
            table.extend_from_slice(&self.opts[..chain]);
            let mut k = 0usize;
            while k < book.pos.len() {
                if at[k] == usize::MAX {
                    let r = book.pos[k].1;
                    table.push(OptState {
                        cfg: HcvOpt {
                            sym: SYMBOL_ID_NONE,
                            und: r.und,
                            call: r.call,
                            strike_1e6: r.strike_1e6,
                            exp_ms: r.exp_ms,
                        },
                        ..OptState::default()
                    });
                    at[k] = table.len() - 1;
                }
                k += 1;
            }
            self.opts = table.into_boxed_slice();
            self.n_opts = self.opts.len();
        }
        let mut done = HcvRestored {
            positions: book.pos.len() as u32,
            orphans: orphans as u32,
            hedges_dropped: book.hedges_dropped,
            cash_usd_1e6: book.cash_usd_1e6,
            ..HcvRestored::default()
        };
        let mut k = 0usize;
        while k < book.pos.len() {
            let r = book.pos[k].1;
            let o = &mut self.opts[at[k]];
            o.pos_1e6 = r.pos_1e6;
            o.avg_px_1e6 = r.avg_px_1e6;
            o.last_px_1e6 = r.last_px_1e6;
            done.expired += u32::from(r.exp_ms <= now_ms);
            k += 1;
        }
        let mut h = 0usize;
        while h < book.hedges.len() {
            let (u, q, mark) = book.hedges[h];
            self.und[u as usize].hedge_pos_1e6 = q;
            self.und[u as usize].kept_mark_1e6 = mark;
            done.hedges += u32::from(q != 0);
            h += 1;
        }
        let mut w = 0usize;
        while w < book.windows.len() {
            let pw = &book.windows[w];
            let s = &mut self.settle[w];
            let ok = s.window.restore(pw.exp_ms, pw.next, pw.last_ts_ms, &pw.samples);
            debug_assert!(ok.is_ok(), "the parse checked every window");
            s.live = ok.is_ok();
            s.und = pw.und;
            s.exp_ms = pw.exp_ms;
            s.px_1e6 = pw.px_1e6;
            s.last_sample_ms = pw.last_ts_ms;
            done.windows += u32::from(s.live);
            w += 1;
        }
        self.cash_usd_1e6 = book.cash_usd_1e6;
        self.day = book.day;
        self.day_start_pnl_usd_1e6 = book.day_start_pnl_usd_1e6;
        self.counters.restored = u64::from(done.positions);
        self.state_restored = true;
        Ok(done)
    }

    /// The chain's row of contract `r` (never an orphan's).
    fn chain_row(&self, r: &HcvPosRow) -> Option<usize> {
        let mut i = 0usize;
        while i < self.n_opts {
            let c = &self.opts[i].cfg;
            if c.sym != SYMBOL_ID_NONE
                && c.und == r.und
                && c.exp_ms == r.exp_ms
                && c.strike_1e6 == r.strike_1e6
                && c.call == r.call
            {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Underlying `name`'s index in the table (`None`: `hcv.toml` does not
    /// trade it).
    fn und_index(&self, name: &str) -> Option<u8> {
        let mut u = 0usize;
        while u < self.n_und {
            if &self.und_names[u][..self.und_name_len[u] as usize] == name.as_bytes() {
                return Some(u as u8);
            }
            u += 1;
        }
        None
    }

    /// Parse and check the whole file (boot path: allocation is fine).
    fn parse_book(&self, text: &str) -> Result<ParsedBook, HcvStateError> {
        let mut version = false;
        let mut cash: Option<i64> = None;
        let mut day: Option<(u64, i64)> = None;
        let mut book = ParsedBook {
            cash_usd_1e6: 0,
            day: 0,
            day_start_pnl_usd_1e6: 0,
            pos: Vec::new(),
            hedges: Vec::new(),
            hedges_dropped: 0,
            windows: Vec::new(),
        };
        // The open window's grid points still owed by `S` rows.
        let mut owed = 0u32;
        for (idx, raw) in text.lines().enumerate() {
            let line = idx + 1;
            let row = raw.trim();
            if row.is_empty() || row.starts_with('#') {
                continue;
            }
            let mut f = Fields {
                it: row.split('\t'),
                line,
            };
            let tag = f.text()?;
            if tag != "S" && owed != 0 {
                return Err(refused(line, "a window's grid is short of its points"));
            }
            if tag != "V" && !version {
                return Err(refused(line, "the V row comes first"));
            }
            match tag {
                "V" => {
                    if version {
                        return Err(refused(line, "a second V row"));
                    }
                    if f.int()? != i64::from(HCV_STATE_VERSION) {
                        return Err(refused(line, "a version this binary does not read"));
                    }
                    f.end()?;
                    version = true;
                }
                "C" => {
                    if cash.is_some() {
                        return Err(refused(line, "a second C row"));
                    }
                    let c = f.int()?;
                    f.end()?;
                    if too_big(c) {
                        return Err(refused(line, "implausible cash"));
                    }
                    cash = Some(c);
                }
                "D" => {
                    if day.is_some() {
                        return Err(refused(line, "a second D row"));
                    }
                    let d = f.int()?;
                    let base = f.int()?;
                    f.end()?;
                    if !(0..=MAX_EXP_MS / DAY_MS_I).contains(&d) || too_big(base) {
                        return Err(refused(line, "an implausible day or opening mark"));
                    }
                    day = Some((d as u64, base));
                }
                "P" => {
                    let Some(und) = self.und_index(f.text()?) else {
                        return Err(refused(
                            line,
                            "a position on an underlying hcv.toml does not trade — nobody could hedge or \
                             settle it: trade it again, or move the file aside",
                        ));
                    };
                    let exp = f.int()?;
                    let strike = f.int()?;
                    let call = match f.text()? {
                        "C" => true,
                        "P" => false,
                        _ => return Err(refused(line, "a right that is neither C nor P")),
                    };
                    let (pos, avg, last) = (f.int()?, f.int()?, f.int()?);
                    f.end()?;
                    if !(1..=MAX_EXP_MS).contains(&exp)
                        || !(1..=MAX_ABS_1E6).contains(&strike)
                        || pos == 0
                        || too_big(pos)
                        || !(0..=MAX_ABS_1E6).contains(&avg)
                        || !(0..=MAX_ABS_1E6).contains(&last)
                    {
                        return Err(refused(line, "a position without a plausible expiry, strike, size or price"));
                    }
                    let r = HcvPosRow {
                        exp_ms: exp as u64,
                        strike_1e6: strike,
                        pos_1e6: pos,
                        avg_px_1e6: avg,
                        last_px_1e6: last,
                        und,
                        call,
                    };
                    let mut j = 0usize;
                    while j < book.pos.len() {
                        let q = &book.pos[j].1;
                        if q.und == und && q.exp_ms == r.exp_ms && q.strike_1e6 == strike && q.call == call {
                            return Err(refused(line, "a contract held twice"));
                        }
                        j += 1;
                    }
                    book.pos.push((line, r));
                }
                "H" => {
                    let name = f.text()?;
                    let (q, mark) = (f.int()?, f.int()?);
                    f.end()?;
                    if too_big(q) || !(0..=MAX_ABS_1E6).contains(&mark) {
                        return Err(refused(line, "an implausible hedge or mark"));
                    }
                    let Some(und) = self.und_index(name) else {
                        // A residual under the venue's minimum the member could
                        // never close (risk-policy gap), on an underlying no
                        // longer traded: dropped, counted — never the book.
                        let dust = self.p.as_deref().map_or(0, |p| p.hedge_min_usd_1e6);
                        if mark > 0 && crate::mul_1e6(q.abs(), mark) < dust {
                            book.hedges_dropped += 1;
                            continue;
                        }
                        return Err(refused(
                            line,
                            if mark == 0 {
                                "a hedge on an underlying hcv.toml does not trade, with no mark to judge it \
                                 by — nobody could close or mark it: trade it again, or move the file aside"
                            } else {
                                "a hedge on an underlying hcv.toml does not trade, above the venue's \
                                 minimum — nobody could close or mark it: trade it again, or move the file aside"
                            },
                        ));
                    };
                    if book.hedges.iter().any(|&(u, _, _)| u == und) {
                        return Err(refused(line, "a hedge held twice"));
                    }
                    book.hedges.push((und, q, mark));
                }
                "W" => {
                    let Some(und) = self.und_index(f.text()?) else {
                        return Err(refused(line, "a settlement window on an underlying hcv.toml does not trade"));
                    };
                    let (exp, px, next, n, last) = (f.int()?, f.int()?, f.int()?, f.int()?, f.int()?);
                    f.end()?;
                    let grid = GRID_POINTS as i64;
                    let fixed_ok = px == 0 || n == 0;
                    if exp < SETTLE_WINDOW_MS as i64
                        || exp > MAX_EXP_MS
                        || !(-1..=MAX_ABS_1E6).contains(&px)
                        || !(0..=grid).contains(&next)
                        || !(0..=next).contains(&n)
                        || !(0..=exp).contains(&last)
                        || !fixed_ok
                    {
                        return Err(refused(line, "a window no expiry could hold"));
                    }
                    if book.windows.len() >= HCV_SETTLE_SLOTS {
                        return Err(refused(line, "more windows than the member samples at once"));
                    }
                    if book.windows.iter().any(|w| w.und == und && w.exp_ms == exp as u64) {
                        return Err(refused(line, "a window kept twice"));
                    }
                    book.windows.push(ParsedWindow {
                        line,
                        und,
                        exp_ms: exp as u64,
                        px_1e6: px,
                        next: next as u32,
                        last_ts_ms: last as u64,
                        samples: Vec::with_capacity(n as usize),
                    });
                    owed = n as u32;
                }
                "S" => {
                    let (px, run) = (f.int()?, f.int()?);
                    f.end()?;
                    if !(1..=MAX_ABS_1E6).contains(&px) || run <= 0 || run > i64::from(owed) {
                        return Err(refused(line, "a grid run that is empty, unpriced or past its window's points"));
                    }
                    let Some(w) = book.windows.last_mut() else {
                        return Err(refused(line, "a grid run outside a window"));
                    };
                    let mut k = 0i64;
                    while k < run {
                        w.samples.push(px);
                        k += 1;
                    }
                    owed -= run as u32;
                }
                _ => return Err(refused(line, "an unknown row tag")),
            }
        }
        if owed != 0 {
            let line = book.windows.last().map_or(0, |w| w.line);
            return Err(refused(line, "a window's grid is short of its points"));
        }
        if !version {
            return Err(refused(0, "no V row"));
        }
        let (Some(cash), Some((d, base))) = (cash, day) else {
            return Err(refused(0, "no C row or no D row"));
        };
        book.cash_usd_1e6 = cash;
        book.day = d;
        book.day_start_pnl_usd_1e6 = base;
        Ok(book)
    }
}
