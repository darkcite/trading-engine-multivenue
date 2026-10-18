// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # book15 — the full price ladder of one rolling token (PM15-2)
//!
//! A `price_change` row carries the venue's touch PRICES (`best_bid`,
//! `best_ask`) but the size of the changed level only; the static-token
//! parser ([`crate::parse_price_change_row`]) therefore leaves a touch size
//! 0 whenever the row moved a level behind the touch. The late law sizes a
//! take against the ask it crosses, so a rolling token keeps its WHOLE
//! ladder: one `i64` size per 0.001 price level, both sides, loaded from
//! each `book` snapshot and moved by each `price_change` row.
//!
//! ## The touch law (measured on the 2026-10-17 tap, 334 896 events)
//!
//! - **Prices are the venue's.** Each row's `best_bid` / `best_ask` is the
//!   book AFTER the whole event (rows of one event for one asset share the
//!   final touch), so the touch is applied once per asset per event, from
//!   its last row.
//! - **Sizes are the ladder's.** At the venue's touch the ladder knows the
//!   size on 334 881 of 334 896 events (15 unknown, reported as 0).
//! - **Levels the venue says are gone are cleared.** 754 times in the tap
//!   a level above the venue's best bid (or below its best ask) still
//!   held size — the venue moved the touch past it without a size-0 row.
//!   [`Ladder15::apply_touch`] zeroes every such level, so the ladder never
//!   disagrees with the venue's own touch; 610 of 612 post-trade `book`
//!   re-snapshots then matched the ladder level for level.
//!
//! ## The grid
//!
//! 15-minute markets quote on 0.01 or 0.001 (`tick_size_change` at the
//! 0.96 / 0.04 crossings), so every price is a multiple of 0.001: level
//! `i` is the price `i / 1000`, `i ∈ 1..=999`. A price off that grid is
//! refused ([`LadderErr::OffGrid`]) rather than rounded: it would mean the
//! venue moved to a tick this ladder cannot hold, and the binding is then
//! retired by its owner. `best_bid = "0"` and `best_ask = "1"` mean an
//! empty side.
//!
//! Zero allocation: the ladder is a fixed POD, allocated at boot by its
//! owner. No iterator on any path; while-index loops.

use core_parse::{find_field, scan_price_1e6, skip_byte, skip_ws};

/// Price levels per side (power of two; `1..=999` are used).
pub const PM_LADDER_LEVELS: usize = 1024;

/// The ladder's price step ×1e6 (0.001).
pub const PM_LADDER_STEP_1E6: i64 = 1_000;

/// The highest price level index (0.999).
const IDX_MAX: usize = 999;

/// Why a ladder update was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LadderErr {
    /// A level or touch price that is not a multiple of 0.001 in (0, 1).
    OffGrid,
    /// The frame did not have the shape this parser reads.
    Malformed,
}

/// The four numbers of a touch ×1e6; a price 0 is an empty side (its
/// size is then 0 too).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Touch15 {
    /// Best bid price ×1e6.
    pub bid_px: i64,
    /// Size at the best bid, shares ×1e6.
    pub bid_sz: i64,
    /// Best ask price ×1e6.
    pub ask_px: i64,
    /// Size at the best ask, shares ×1e6.
    pub ask_sz: i64,
}

/// One `price_change` row, parsed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    /// The changed level's price ×1e6.
    pub px: i64,
    /// The level's NEW aggregate size, shares ×1e6 (0 = removed).
    pub size: i64,
    /// `side` was `BUY` (a bid level).
    pub is_bid: bool,
    /// The venue's best bid after the event ×1e6 (0 = none).
    pub best_bid: i64,
    /// The venue's best ask after the event ×1e6 (≥ 1e6 = none).
    pub best_ask: i64,
}

/// One token's ladder (module docs).
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct Ladder15 {
    bid: [i64; PM_LADDER_LEVELS],
    ask: [i64; PM_LADDER_LEVELS],
    /// The venue's best bid level (0 = none).
    best_bid: u16,
    /// The venue's best ask level (0 = none).
    best_ask: u16,
    /// The highest bid level that may hold size (0 = none).
    top_bid: u16,
    /// The lowest ask level that may hold size (0 = none).
    low_ask: u16,
    /// A `book` snapshot has been loaded since the last [`Self::clear`].
    ready: u8,
    _pad: [u8; 55],
}

impl Ladder15 {
    /// The empty, not-ready ladder.
    pub const EMPTY: Self = Self {
        bid: [0; PM_LADDER_LEVELS],
        ask: [0; PM_LADDER_LEVELS],
        best_bid: 0,
        best_ask: 0,
        top_bid: 0,
        low_ask: 0,
        ready: 0,
        _pad: [0; 55],
    };

    /// The level index of a price ×1e6 on the 0.001 grid, `1..=999`.
    #[inline(always)]
    #[must_use]
    pub const fn idx_of(px_1e6: i64) -> Option<usize> {
        if px_1e6 <= 0 || px_1e6 % PM_LADDER_STEP_1E6 != 0 {
            return None;
        }
        let i = (px_1e6 / PM_LADDER_STEP_1E6) as usize;
        if i > IDX_MAX {
            return None;
        }
        Some(i)
    }

    /// A touch price as a level: `0` / `≥ 1` are an empty side (level 0).
    #[inline(always)]
    fn touch_idx(px_1e6: i64) -> Result<usize, LadderErr> {
        if px_1e6 <= 0 || px_1e6 >= 1_000_000 {
            return Ok(0);
        }
        Self::idx_of(px_1e6).ok_or(LadderErr::OffGrid)
    }

    /// Whether a snapshot has been loaded since the last clear.
    #[inline(always)]
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.ready != 0
    }

    /// Forget everything (a new binding, a reconnect).
    pub fn clear(&mut self) {
        let mut i = 0usize;
        while i < PM_LADDER_LEVELS {
            self.bid[i] = 0;
            self.ask[i] = 0;
            i += 1;
        }
        self.best_bid = 0;
        self.best_ask = 0;
        self.top_bid = 0;
        self.low_ask = 0;
        self.ready = 0;
    }

    /// The touch: the venue's prices, the ladder's sizes.
    #[inline(always)]
    #[must_use]
    pub fn touch(&self) -> Touch15 {
        let b = self.best_bid as usize % PM_LADDER_LEVELS;
        let a = self.best_ask as usize % PM_LADDER_LEVELS;
        Touch15 {
            bid_px: b as i64 * PM_LADDER_STEP_1E6,
            bid_sz: if b == 0 { 0 } else { self.bid[b] },
            ask_px: a as i64 * PM_LADDER_STEP_1E6,
            ask_sz: if a == 0 { 0 } else { self.ask[a] },
        }
    }

    /// The resting size at `px_1e6` on one side (0 off the grid).
    #[inline(always)]
    #[must_use]
    pub fn size_at(&self, is_bid: bool, px_1e6: i64) -> i64 {
        match Self::idx_of(px_1e6) {
            Some(i) if is_bid => self.bid[i],
            Some(i) => self.ask[i],
            None => 0,
        }
    }

    /// Set one level's aggregate size (a `price_change` row's level).
    ///
    /// # Errors
    /// [`LadderErr::OffGrid`] — nothing changes.
    #[inline]
    pub fn set_level(&mut self, is_bid: bool, px_1e6: i64, size_1e6: i64) -> Result<(), LadderErr> {
        let i = Self::idx_of(px_1e6).ok_or(LadderErr::OffGrid)?;
        let sz = if size_1e6 > 0 { size_1e6 } else { 0 };
        if is_bid {
            self.bid[i] = sz;
            if sz > 0 && i > self.top_bid as usize {
                self.top_bid = i as u16;
            }
        } else {
            self.ask[i] = sz;
            if sz > 0 && (self.low_ask == 0 || i < self.low_ask as usize) {
                self.low_ask = i as u16;
            }
        }
        Ok(())
    }

    /// Adopt the venue's touch after an event and clear every level the
    /// venue says is gone (module docs). Nothing changes on an error.
    ///
    /// # Errors
    /// [`LadderErr::OffGrid`] for a touch price off the grid.
    pub fn apply_touch(&mut self, best_bid_1e6: i64, best_ask_1e6: i64) -> Result<(), LadderErr> {
        let b = Self::touch_idx(best_bid_1e6)?;
        let a = Self::touch_idx(best_ask_1e6)?;
        // Bids above the venue's best bid are stale.
        let mut i = self.top_bid as usize;
        while i > b {
            self.bid[i % PM_LADDER_LEVELS] = 0;
            i -= 1;
        }
        self.top_bid = b as u16;
        // Asks below the venue's best ask are stale (all of them when the
        // ask side is empty).
        if self.low_ask != 0 {
            let stop = if a == 0 { IDX_MAX + 1 } else { a };
            let mut i = self.low_ask as usize;
            while i < stop {
                self.ask[i % PM_LADDER_LEVELS] = 0;
                i += 1;
            }
        }
        self.low_ask = a as u16;
        self.best_bid = b as u16;
        self.best_ask = a as u16;
        Ok(())
    }

    /// Load a `book` snapshot (one event, already sliced out of its
    /// frame): clear, then every level of both sides; the touch is the
    /// best of each side, whatever order the levels arrive in.
    ///
    /// # Errors
    /// [`LadderErr`] — the ladder is then cleared and NOT ready.
    pub fn load_book(&mut self, ev: &[u8]) -> Result<(), LadderErr> {
        self.clear();
        let r = match self.load_side(ev, b"\"bids\":[", true) {
            Ok(()) => self.load_side(ev, b"\"asks\":[", false),
            e => e,
        };
        if r.is_err() {
            self.clear();
            return r;
        }
        // The touch from the levels: the highest bid, the lowest ask.
        let mut b = IDX_MAX;
        while b > 0 && self.bid[b] == 0 {
            b -= 1;
        }
        let mut a = 1usize;
        while a <= IDX_MAX && self.ask[a] == 0 {
            a += 1;
        }
        let a = if a > IDX_MAX { 0 } else { a };
        self.best_bid = b as u16;
        self.top_bid = b as u16;
        self.best_ask = a as u16;
        self.low_ask = a as u16;
        self.ready = 1;
        Ok(())
    }

    /// One side's `[{"price":"X","size":"Y"}, …]` array after `marker`.
    fn load_side(&mut self, ev: &[u8], marker: &[u8], is_bid: bool) -> Result<(), LadderErr> {
        let start = memchr::memmem::find(ev, marker).ok_or(LadderErr::Malformed)? + marker.len();
        let mut at = skip_ws(ev, start);
        if ev.get(at) == Some(&b']') {
            return Ok(());
        }
        loop {
            if ev.get(at..at + 10) != Some(b"{\"price\":\"") {
                return Err(LadderErr::Malformed);
            }
            let (px, px_end) = scan_price_1e6(ev, at + 10).ok_or(LadderErr::Malformed)?;
            if ev.get(px_end..px_end + 2) != Some(b"\",") {
                return Err(LadderErr::Malformed);
            }
            let sz_at = skip_ws(ev, px_end + 2);
            if ev.get(sz_at..sz_at + 8) != Some(b"\"size\":\"") {
                return Err(LadderErr::Malformed);
            }
            let (sz, sz_end) = scan_price_1e6(ev, sz_at + 8).ok_or(LadderErr::Malformed)?;
            self.set_level(is_bid, px, sz)?;
            let rel = memchr::memchr(b'}', ev.get(sz_end..).ok_or(LadderErr::Malformed)?)
                .ok_or(LadderErr::Malformed)?;
            at = skip_ws(ev, sz_end + rel + 1);
            match ev.get(at) {
                Some(b',') => at = skip_ws(ev, at + 1),
                Some(b']') => return Ok(()),
                _ => return Err(LadderErr::Malformed),
            }
        }
    }
}

/// Parse one `price_change` row (the span from its `"asset_id":"` to the
/// next row's): field order is not assumed.
#[must_use]
pub fn parse_change_row(row: &[u8]) -> Option<ChangeRow> {
    let px = quoted_price(row, b"\"price\":")?;
    let size = quoted_price(row, b"\"size\":")?;
    let best_bid = quoted_price(row, b"\"best_bid\":")?;
    let best_ask = quoted_price(row, b"\"best_ask\":")?;
    let is_bid = if memchr::memmem::find(row, b"\"side\":\"BUY\"").is_some() {
        true
    } else if memchr::memmem::find(row, b"\"side\":\"SELL\"").is_some() {
        false
    } else {
        return None;
    };
    Some(ChangeRow {
        px,
        size,
        is_bid,
        best_bid,
        best_ask,
    })
}

/// A quoted decimal field ×1e6 (`"key":"0.53"`, a space after the colon
/// tolerated).
#[inline]
pub(crate) fn quoted_price(buf: &[u8], key: &[u8]) -> Option<i64> {
    let p = skip_ws(buf, find_field(buf, key)?);
    let p = skip_byte(buf, p, b'"');
    Some(scan_price_1e6(buf, p)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live 2026-10-17 (ETH 15m, trimmed): the subscribe-time snapshot,
    /// levels worst→best.
    const BOOK: &[u8] = br#"{"market":"0x2b38","asset_id":"3257","timestamp":"1792217672506","hash":"18e6","bids":[{"price":"0.01","size":"1130"},{"price":"0.42","size":"10"},{"price":"0.43","size":"54.4"}],"asks":[{"price":"0.99","size":"100"},{"price":"0.46","size":"18"},{"price":"0.45","size":"20"}],"event_type":"book"}"#;

    fn loaded() -> Box<Ladder15> {
        let mut l = Box::new(Ladder15::EMPTY);
        l.load_book(BOOK).expect("live shape");
        l
    }

    #[test]
    fn a_snapshot_loads_every_level_and_the_touch_is_order_independent() {
        let l = loaded();
        assert!(l.is_ready());
        assert_eq!(
            l.touch(),
            Touch15 {
                bid_px: 430_000,
                bid_sz: 54_400_000,
                ask_px: 450_000,
                ask_sz: 20_000_000
            }
        );
        assert_eq!(l.size_at(true, 420_000), 10_000_000);
        assert_eq!(l.size_at(false, 990_000), 100_000_000);
        // The best-first order loads the same ladder.
        let mut r = Box::new(Ladder15::EMPTY);
        r.load_book(br#"{"bids":[{"price":"0.43", "size":"54.4"}, {"price":"0.01", "size":"1130"}], "asks":[], "event_type":"book"}"#)
            .expect("spaced shape");
        assert_eq!(r.touch().bid_px, 430_000);
        assert_eq!(r.touch().ask_px, 0, "an empty side");
    }

    #[test]
    fn rows_move_levels_and_the_venue_touch_clears_what_it_passed() {
        let mut l = loaded();
        // A level behind the touch: the touch size is unchanged.
        let row = parse_change_row(br#""asset_id":"3257", "price":"0.42", "size":"12", "side":"BUY", "hash":"aa", "best_bid":"0.43", "best_ask":"0.45"}"#)
            .expect("live row");
        assert_eq!(
            row,
            ChangeRow {
                px: 420_000,
                size: 12_000_000,
                is_bid: true,
                best_bid: 430_000,
                best_ask: 450_000
            }
        );
        l.set_level(row.is_bid, row.px, row.size).unwrap();
        l.apply_touch(row.best_bid, row.best_ask).unwrap();
        assert_eq!(l.touch().bid_sz, 54_400_000);
        // A trade took 0.43 and the venue says the best bid is now 0.42 —
        // with no size-0 row for 0.43: the ladder clears it.
        l.apply_touch(420_000, 450_000).unwrap();
        assert_eq!(
            l.touch(),
            Touch15 {
                bid_px: 420_000,
                bid_sz: 12_000_000,
                ask_px: 450_000,
                ask_sz: 20_000_000
            }
        );
        assert_eq!(l.size_at(true, 430_000), 0);
        // The ask side lifts to 0.46: 0.45 is cleared, 0.46 keeps 18.
        l.apply_touch(420_000, 460_000).unwrap();
        assert_eq!(l.size_at(false, 450_000), 0);
        assert_eq!(l.touch().ask_sz, 18_000_000);
        // `best_ask = 1` empties the side and clears every ask.
        l.apply_touch(420_000, 1_000_000).unwrap();
        assert_eq!(l.touch().ask_px, 0);
        assert_eq!(l.size_at(false, 990_000), 0);
        // A new ask after that sets the low mark again.
        l.set_level(false, 980_000, 5_000_000).unwrap();
        l.apply_touch(420_000, 980_000).unwrap();
        assert_eq!(l.touch().ask_sz, 5_000_000);
    }

    #[test]
    fn off_grid_prices_and_bad_shapes_are_refused() {
        let mut l = loaded();
        assert_eq!(l.set_level(true, 425_500, 1), Err(LadderErr::OffGrid));
        assert_eq!(l.set_level(true, 1_000_000, 1), Err(LadderErr::OffGrid));
        assert_eq!(l.set_level(true, 0, 1), Err(LadderErr::OffGrid));
        assert_eq!(l.apply_touch(425_500, 450_000), Err(LadderErr::OffGrid));
        assert_eq!(l.touch().bid_px, 430_000, "a refused touch changes nothing");
        // The 0.001 grid itself is fine: 0.999 and 0.001.
        l.set_level(true, 999_000, 3_000_000).unwrap();
        l.set_level(false, 1_000, 3_000_000).unwrap();
        let mut b = Box::new(Ladder15::EMPTY);
        assert_eq!(
            b.load_book(br#"{"bids":[{"price":"0.4255","size":"1"}],"asks":[]}"#),
            Err(LadderErr::OffGrid)
        );
        assert!(!b.is_ready());
        assert_eq!(
            b.load_book(br#"{"bids":[{"px":"0.42","size":"1"}],"asks":[]}"#),
            Err(LadderErr::Malformed)
        );
        assert_eq!(b.load_book(br#"{"asks":[]}"#), Err(LadderErr::Malformed));
        assert!(parse_change_row(br#""asset_id":"1","price":"0.4","size":"1","side":"HOLD","best_bid":"0.4","best_ask":"0.5""#).is_none());
        assert!(parse_change_row(
            br#""asset_id":"1","price":"0.4","size":"1","side":"BUY","best_bid":"0.4""#
        )
        .is_none());
    }
}
