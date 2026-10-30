// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.1–§4.3.5 (T8): rows and the bar builder. Bars come from Trade prints only (I12),
//! bucketed by the print's venue time through `core_time::barclock` (I13); a quote tick only
//! confirms, by time. `index` advances by exactly 1 per present bar, a hole sets `GAP_BEFORE` on
//! the next bar, no synthetic bar is ever made, and a late print is dropped and counted (I14). The
//! flags follow §4.3.5 (I15).
//!
//! Hot module (I4): no allocation, no clock, no iterator loop. Every boundary is a
//! `core_time::barclock` call — this file holds no boundary arithmetic (I13).
//!
//! Literal readings:
//! - pin:08-partial-is-boot-bucket: the partial boot bar (§4.3.4) is the live bar whose bucket is
//!   `barclock::open_ms(boot_utc_ms, tf)`; `Confirmed::partial` carries it to the host, which
//!   counts `bars_partial_boot` (no `Bar` flag exists for it, none is invented).
//! - pin:08-qty-abs-saturates: `qty_1e6 = |v1|`, with `|i64::MIN|` saturated to `i64::MAX` (and
//!   `VOL_SAT` set), never a wrap.
//! - pin:08-gap-len: a bar's gap length is `bars_between(last_open, open) − 1` (missing buckets);
//!   the host compares it with `max_bars_back` for `bars_gapped` (§4.3.3).

use core_time::barclock;
use pine_ir::{barflag, Bar};

/// "No bar yet" for `Row::last_open_ms`.
pub(crate) const NONE_MS: i64 = i64::MIN;

/// One `(symbol, tf_s)` row (§4.3.1).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row {
    /// The engine symbol that feeds the row.
    pub(crate) sym: core_types::SymbolId,
    /// Row timeframe (an `ALLOWED_TF_S` value, checked at load).
    pub(crate) tf_s: u32,
    /// A bucket is forming.
    pub(crate) forming: bool,
    /// The forming bucket's open, UTC ms.
    pub(crate) open_ms: i64,
    o: i64,
    h: i64,
    l: i64,
    c: i64,
    v: i64,
    vol_sat: bool,
    gap_next: bool,
    gap_len: i64,
    partial: bool,
    /// `Bar::index` of the next confirmed bar (= confirmed bars so far, warm bars included).
    pub(crate) next_index: u32,
    /// The last confirmed (or warm) bar's open; `NONE_MS` = none.
    pub(crate) last_open_ms: i64,
    /// The boot bucket's open (`barclock::open_ms(boot_utc_ms, tf)`).
    pub(crate) boot_open_ms: i64,
    /// The last confirmed bar (warm or live); valid once `next_index > 0`.
    pub(crate) last: Bar,
    /// The row is warm (§4.3.6); a cold row warms on the live tape (§4.3.7).
    pub(crate) warm: bool,
}

/// A confirmed bar and what the host counts about it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Confirmed {
    pub(crate) bar: Bar,
    /// The boot bucket's bar (pin:08-partial-is-boot-bucket).
    pub(crate) partial: bool,
    /// Missing buckets before it (pin:08-gap-len); 0 without a hole.
    pub(crate) gap_len: i64,
}

/// What one print did to a row (§4.3.3).
#[derive(Debug, Clone, Copy)]
pub(crate) enum TradeOut {
    /// Late: dropped, counted.
    Late,
    /// The forming bucket took it.
    Updated,
    /// It opened the first bucket after none was forming.
    Opened,
    /// It closed the forming bucket and opened the next.
    ConfirmedAndOpened(Confirmed),
}

const ZERO_BAR: Bar = Bar {
    open_1e6: 0,
    high_1e6: 0,
    low_1e6: 0,
    close_1e6: 0,
    volume_1e6: 0,
    open_ms: 0,
    close_ms: 0,
    index: 0,
    flags: 0,
};

impl Row {
    /// A fresh, cold row (boot only).
    pub(crate) fn new(sym: core_types::SymbolId, tf_s: u32, boot_utc_ms: i64) -> Self {
        Self {
            sym,
            tf_s,
            forming: false,
            open_ms: 0,
            o: 0,
            h: 0,
            l: 0,
            c: 0,
            v: 0,
            vol_sat: false,
            gap_next: false,
            gap_len: 0,
            partial: false,
            next_index: 0,
            last_open_ms: NONE_MS,
            boot_open_ms: barclock::open_ms(boot_utc_ms, tf_s),
            last: ZERO_BAR,
            warm: false,
        }
    }

    /// §4.3.3 `on_trade`: `px` ×1e6, `q` = |qty| ×1e6, `t_ms` the print's venue time.
    #[inline]
    pub(crate) fn on_trade(&mut self, px: i64, q: i64, t_ms: i64) -> TradeOut {
        let b = barclock::open_ms(t_ms, self.tf_s);
        if self.forming {
            if b < self.open_ms {
                return TradeOut::Late;
            }
            if b == self.open_ms {
                self.h = self.h.max(px);
                self.l = self.l.min(px);
                self.c = px;
                self.add_vol(q);
                return TradeOut::Updated;
            }
            let c = self.confirm();
            self.start(b, px, q);
            return TradeOut::ConfirmedAndOpened(c);
        }
        if self.last_open_ms != NONE_MS && b <= self.last_open_ms {
            return TradeOut::Late;
        }
        self.start(b, px, q);
        TradeOut::Opened
    }

    /// §4.3.3 `time_confirm`: a quote tick or print of the symbol, or a chart close, at `t_ms`.
    #[inline]
    pub(crate) fn time_confirm(&mut self, t_ms: i64) -> Option<Confirmed> {
        if self.forming && t_ms >= barclock::close_ms(self.open_ms, self.tf_s) {
            return Some(self.confirm());
        }
        None
    }

    /// The forming bar as `step_tick` / `open_bar` see it (§4.3.3): `REALTIME (+ GAP_BEFORE /
    /// VOL_SAT)`, `index = next_index`. `None` when no bucket is forming.
    #[inline]
    pub(crate) fn forming_bar(&self) -> Option<Bar> {
        if !self.forming {
            return None;
        }
        Some(Bar {
            open_1e6: self.o,
            high_1e6: self.h,
            low_1e6: self.l,
            close_1e6: self.c,
            volume_1e6: self.v,
            open_ms: self.open_ms,
            close_ms: barclock::close_ms(self.open_ms, self.tf_s),
            index: self.next_index,
            flags: barflag::REALTIME | self.carried_flags(),
        })
    }

    /// The live start after warm-up (§4.3.6): live bars continue at `n`, after `last_open_ms`.
    pub(crate) fn resume_after(&mut self, last: &Bar) {
        self.next_index = last.index.saturating_add(1);
        self.last_open_ms = last.open_ms;
        // COPY: one 64 B Bar per warm row, once at boot — the row outlives the freed warm slice —
        // a borrow of the slice rejected: §4.2 step 9 frees it inside `on_start`.
        self.last = *last;
    }

    #[inline]
    fn carried_flags(&self) -> u32 {
        let mut f = 0u32;
        if self.gap_next {
            f |= barflag::GAP_BEFORE;
        }
        if self.vol_sat {
            f |= barflag::VOL_SAT;
        }
        f
    }

    #[inline]
    fn add_vol(&mut self, q: i64) {
        match self.v.checked_add(q) {
            Some(v) => self.v = v,
            None => {
                self.v = i64::MAX;
                self.vol_sat = true;
            }
        }
    }

    #[inline]
    fn start(&mut self, b: i64, px: i64, q: i64) {
        self.forming = true;
        self.open_ms = b;
        self.o = px;
        self.h = px;
        self.l = px;
        self.c = px;
        self.v = q;
        self.vol_sat = q == i64::MAX;
        let between = if self.last_open_ms == NONE_MS {
            1
        } else {
            barclock::bars_between(self.last_open_ms, b, self.tf_s)
        };
        self.gap_next = between > 1;
        self.gap_len = between.saturating_sub(1).max(0);
        self.partial = b == self.boot_open_ms;
    }

    #[inline]
    fn confirm(&mut self) -> Confirmed {
        let bar = Bar {
            open_1e6: self.o,
            high_1e6: self.h,
            low_1e6: self.l,
            close_1e6: self.c,
            volume_1e6: self.v,
            open_ms: self.open_ms,
            close_ms: barclock::close_ms(self.open_ms, self.tf_s),
            index: self.next_index,
            flags: barflag::CONFIRMED | barflag::REALTIME | self.carried_flags(),
        };
        let c = Confirmed {
            bar,
            partial: self.partial,
            gap_len: self.gap_len,
        };
        self.next_index = self.next_index.saturating_add(1);
        self.last_open_ms = self.open_ms;
        // COPY: the confirmed 64 B Bar into the row (a POD by value), once per confirm — the
        // forming fields are reset below and the probe / warm resume read it — a borrow rejected:
        // the bucket state it came from is overwritten by the next `start`.
        self.last = bar;
        self.forming = false;
        self.gap_next = false;
        self.gap_len = 0;
        self.vol_sat = false;
        self.partial = false;
        c
    }
}

/// `|v1|` ×1e6 (pin:08-qty-abs-saturates).
#[inline]
pub(crate) fn qty_abs(v1: i64) -> i64 {
    v1.checked_abs().unwrap_or(i64::MAX)
}

/// §4.3.2: a print's (or tick's) UTC ms — the venue time when non-zero, else the anchored ingress
/// time (`ms_from_ns(sat_i64(anchor.wall_of(ts_ns)))`).
#[inline]
pub(crate) fn t_ms(
    venue_time_ms: u64,
    ts_ns: core_time::NsTs,
    anchor: &core_time::WallAnchor,
) -> i64 {
    if venue_time_ms != 0 {
        return i64::try_from(venue_time_ms).unwrap_or(i64::MAX);
    }
    pine_ir::scale::ms_from_ns(i64::try_from(anchor.wall_of(ts_ns)).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00Z, a Thursday
    const BOOT: i64 = T0 + 30 * 60_000 + 7; // inside the 00:30 1 m bucket

    fn bars(out: TradeOut) -> Option<Bar> {
        match out {
            TradeOut::ConfirmedAndOpened(c) => Some(c.bar),
            _ => None,
        }
    }

    /// I12: OHLCV are the prints'; a quote tick confirms by time and never touches them.
    #[test]
    fn bars_come_from_prints_only() {
        let mut r = Row::new(1, 60, BOOT);
        let base = T0 + 31 * 60_000;
        assert!(matches!(r.on_trade(100, 2, base + 1), TradeOut::Opened));
        assert!(matches!(r.on_trade(105, 3, base + 2), TradeOut::Updated));
        assert!(matches!(r.on_trade(95, 4, base + 3), TradeOut::Updated));
        assert!(matches!(r.on_trade(101, 5, base + 4), TradeOut::Updated));
        let b = bars(r.on_trade(200, 1, base + 60_000)).expect("confirmed");
        assert_eq!(
            (b.open_1e6, b.high_1e6, b.low_1e6, b.close_1e6, b.volume_1e6),
            (100, 105, 95, 101, 14)
        );
        assert_eq!((b.open_ms, b.close_ms, b.index), (base, base + 60_000, 0));
    }

    #[test]
    fn quote_ticks_confirm_but_never_touch_ohlcv() {
        let mut r = Row::new(1, 60, BOOT);
        let base = T0 + 31 * 60_000;
        r.on_trade(100, 2, base + 5);
        // A quote inside the bucket: nothing.
        assert!(r.time_confirm(base + 59_999).is_none());
        let fb = r.forming_bar().expect("forming");
        assert_eq!((fb.open_1e6, fb.close_1e6, fb.volume_1e6), (100, 100, 2));
        // A quote at the close: the bar confirms, with the prints' OHLCV only.
        let c = r.time_confirm(base + 60_000).expect("confirmed");
        assert_eq!(
            (c.bar.open_1e6, c.bar.close_1e6, c.bar.volume_1e6),
            (100, 100, 2)
        );
        assert!(!r.forming && r.time_confirm(base + 120_000).is_none());
    }

    /// I13 (property): every print lands in `barclock::open_ms(t, tf)`, for every allowed tf.
    #[test]
    fn bucket_is_barclock_of_venue_time() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut k = 0;
        while k < barclock::ALLOWED_TF_S.len() {
            let tf = barclock::ALLOWED_TF_S[k];
            let mut r = Row::new(1, tf, T0);
            let mut t = T0;
            let mut n = 0;
            while n < 8_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                t += i64::try_from(x % 600_000).expect("small");
                match r.on_trade(1, 1, t) {
                    TradeOut::Late => panic!("ascending prints are never late"),
                    TradeOut::Opened | TradeOut::Updated => {}
                    TradeOut::ConfirmedAndOpened(c) => {
                        assert_eq!(barclock::open_ms(c.bar.open_ms, tf), c.bar.open_ms);
                        assert_eq!(c.bar.close_ms, barclock::close_ms(c.bar.open_ms, tf));
                    }
                }
                assert_eq!(r.open_ms, barclock::open_ms(t, tf));
                n += 1;
            }
            k += 1;
        }
    }

    #[test]
    fn anchor_fallback_when_venue_time_is_zero() {
        let a = core_time::WallAnchor::new(1_000, 1_767_225_600_000_000_000);
        assert_eq!(t_ms(1_767_225_600_123, 5, &a), 1_767_225_600_123);
        // venue time 0: ingress ns 1_000 + 2_500_000 → wall + 2.5 ms → floor 2 ms.
        assert_eq!(t_ms(0, 2_501_000, &a), 1_767_225_600_002);
    }

    /// I14: a hole sets GAP_BEFORE and the index still advances by exactly 1; no synthetic bar.
    #[test]
    fn hole_sets_gap_before_and_index_plus_one() {
        let mut r = Row::new(1, 60, BOOT);
        let m = |k: i64| T0 + k * 60_000;
        r.on_trade(1, 1, m(40));
        let b0 = bars(r.on_trade(1, 1, m(41))).expect("b0");
        let b1 = bars(r.on_trade(1, 1, m(45))).expect("b1"); // 42, 43, 44 missing
        let c = r.time_confirm(m(46)).expect("b2");
        assert_eq!((b0.index, b1.index, c.bar.index), (0, 1, 2));
        assert_eq!(b1.flags & barflag::GAP_BEFORE, 0);
        assert_ne!(c.bar.flags & barflag::GAP_BEFORE, 0);
        assert_eq!(c.gap_len, 3);
    }

    #[test]
    fn late_print_is_dropped_and_counted() {
        let mut r = Row::new(1, 60, BOOT);
        let m = |k: i64| T0 + k * 60_000;
        r.on_trade(10, 1, m(40) + 5);
        assert!(matches!(r.on_trade(99, 1, m(39) + 5), TradeOut::Late));
        let c = r.time_confirm(m(41)).expect("confirmed");
        assert_eq!(c.bar.high_1e6, 10);
        // After the confirm nothing is forming: a print in the confirmed bucket is late too.
        assert!(matches!(r.on_trade(99, 1, m(40) + 9), TradeOut::Late));
    }

    /// I15 (live states): forming `REALTIME`, confirmed `CONFIRMED | REALTIME`, never `LAST`.
    #[test]
    fn flags_are_truthful_in_every_state() {
        let mut r = Row::new(1, 60, BOOT);
        let m = |k: i64| T0 + k * 60_000;
        r.on_trade(1, i64::MAX, m(40));
        let f = r.forming_bar().expect("forming");
        assert_eq!(f.flags, barflag::REALTIME | barflag::VOL_SAT);
        r.on_trade(1, 5, m(40) + 1);
        let c = r.time_confirm(m(41)).expect("c");
        assert_eq!(
            c.bar.flags,
            barflag::CONFIRMED | barflag::REALTIME | barflag::VOL_SAT
        );
        assert_eq!(c.bar.volume_1e6, i64::MAX);
        r.on_trade(1, 1, m(43));
        let f = r.forming_bar().expect("forming");
        assert_eq!(f.flags, barflag::REALTIME | barflag::GAP_BEFORE);
        assert_eq!(f.index, 1);
        let c = r.time_confirm(m(44)).expect("c");
        assert_eq!(
            c.bar.flags,
            barflag::CONFIRMED | barflag::REALTIME | barflag::GAP_BEFORE
        );
        assert_eq!(
            c.bar.flags & (barflag::LAST | barflag::LAST_CONFIRMED_HISTORY),
            0
        );
    }

    /// §4.3.4: the boot bucket's bar is confirmed and evaluated like any other, and marked.
    #[test]
    fn partial_boot_bar_is_counted_and_marked() {
        let mut r = Row::new(1, 60, BOOT);
        let boot_bucket = barclock::open_ms(BOOT, 60);
        r.on_trade(1, 1, BOOT + 10);
        let c = r.time_confirm(boot_bucket + 60_000).expect("c");
        assert!(c.partial);
        assert_eq!(c.bar.open_ms, boot_bucket);
        r.on_trade(1, 1, boot_bucket + 60_000 + 1);
        let c = r.time_confirm(boot_bucket + 120_000).expect("c");
        assert!(!c.partial);
    }
}
