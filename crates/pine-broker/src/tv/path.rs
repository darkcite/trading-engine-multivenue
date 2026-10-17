// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The intrabar path, `reach` / `TRIGGER`, the segment sort and the §4.8.6 verification level
//! (impl-05 §4.8.2, §4.8.4, §4.8.6). Pure functions over `i64 ×1e6` prices; no allocation; index
//! loops.

use crate::switch::{FillLimitsPrice, PathTie};
use pine_ir::Bar;

/// The four path points: `[o, h, l, c]` or `[o, l, h, c]` (§4.8.2).
pub(crate) fn path(bar: &Bar, tie: PathTie) -> [i64; 4] {
    let (o, h, l, c) = (bar.open_1e6, bar.high_1e6, bar.low_1e6, bar.close_1e6);
    let up = i128::from(h) - i128::from(o);
    let down = i128::from(o) - i128::from(l);
    let high_first = if up < down {
        true
    } else if up > down {
        false
    } else {
        tie == PathTie::HighFirst
    };
    if high_first {
        [o, h, l, c]
    } else {
        [o, l, h, c]
    }
}

/// Segment `k` of a path: `(path[k], path[k+1])` for k = 0, 1, 2; segment 3 is the close tick
/// `(c, c)`.
pub(crate) const fn segment(p: &[i64; 4], k: usize) -> (i64, i64) {
    if k < 3 {
        (p[k], p[k + 1])
    } else {
        (p[3], p[3])
    }
}

/// A candidate's trigger rule (§4.8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// Market: always, at the segment start.
    Market,
    /// Buy limit at `L`.
    BuyLimit(i64),
    /// Sell limit at `L`.
    SellLimit(i64),
    /// Buy stop at `S` (also a short's stop-loss / armed trail).
    BuyStop(i64),
    /// Sell stop at `S`.
    SellStop(i64),
}

/// Where a candidate triggers on segment `(a, b)` and the price it fills at before slippage, or
/// `None` when the segment does not reach it (§4.8.4). `verify_1e6 > 0` is the §4.8.6 distance
/// `d` for limit rows (`fill_limits_ticks · mintick`); `fill_at` its `fill_limits_price` reading.
pub(crate) fn reach(
    lv: Level,
    a: i64,
    b: i64,
    verify_1e6: i64,
    fill_at: FillLimitsPrice,
) -> Option<(i64, i64)> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    match lv {
        Level::Market => Some((a, a)),
        Level::BuyLimit(l) => {
            if verify_1e6 > 0 {
                // V = L − d, saturating; a saturated V never verifies.
                let v = i64::try_from(i128::from(l) - i128::from(verify_1e6)).ok()?;
                if lo > v {
                    return None;
                }
                let px = if fill_at == FillLimitsPrice::AtStart && a <= v {
                    a
                } else {
                    l
                };
                Some((a.min(v), px))
            } else if lo <= l {
                let r = a.min(l);
                Some((r, r))
            } else {
                None
            }
        }
        Level::SellLimit(l) => {
            if verify_1e6 > 0 {
                let v = i64::try_from(i128::from(l) + i128::from(verify_1e6)).ok()?;
                if hi < v {
                    return None;
                }
                let px = if fill_at == FillLimitsPrice::AtStart && a >= v {
                    a
                } else {
                    l
                };
                Some((a.max(v), px))
            } else if hi >= l {
                let r = a.max(l);
                Some((r, r))
            } else {
                None
            }
        }
        Level::BuyStop(s) => {
            if hi >= s {
                let r = a.max(s);
                Some((r, r))
            } else {
                None
            }
        }
        Level::SellStop(s) => {
            if lo <= s {
                let r = a.min(s);
                Some((r, r))
            } else {
                None
            }
        }
    }
}

/// One walk candidate: its reach along the segment, its commit order, and what it is. 16 B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Cand {
    /// Reach along the segment.
    pub(crate) key: i64,
    /// Commit order — the tie-break.
    pub(crate) seq: u32,
    /// Order index, or leg-set index | leg bits.
    pub(crate) what: u16,
    /// Walk bits (`tv::CAND_REKEYED`: a stop-limit's limit stage re-keyed with `a := S`;
    /// [`CAND_OPEN_MARKET`]: a market order at the open).
    pub(crate) flags: u16,
}

const _: () = assert!(core::mem::size_of::<Cand>() == 16);
const _: () = assert!(core::mem::align_of::<Cand>() == 8);

/// `Cand::flags` bit: a pending market order at the open path point (segment 0, reached at the
/// open). `pin:05-open-market-first` (provisional, R-P19, one TV case — E130 trade 26): at the
/// open it fills before the stop / limit candidates the open triggers (a gap through them),
/// whatever their creation order.
pub(crate) const CAND_OPEN_MARKET: u16 = 8;

/// `x` precedes `y` in path order: ascending reach on an up segment, descending on a down one,
/// ties by `seq` (I19) — except at the open, where a [`CAND_OPEN_MARKET`] candidate precedes the
/// stop / limit candidates the open triggers (`pin:05-open-market-first`, provisional, E130 trade
/// 26); market against market and every other tie keep creation order.
pub(crate) const fn before(x: &Cand, y: &Cand, up: bool) -> bool {
    if x.key == y.key {
        let (xm, ym) = (
            x.flags & CAND_OPEN_MARKET != 0,
            y.flags & CAND_OPEN_MARKET != 0,
        );
        if xm != ym {
            xm
        } else {
            x.seq < y.seq
        }
    } else if up {
        x.key < y.key
    } else {
        x.key > y.key
    }
}

/// Insertion sort of the candidates into path order (§4.8.4); no allocation.
pub(crate) fn sort_cands(c: &mut [Cand], up: bool) {
    let n = c.len();
    let mut i = 1;
    while i < n {
        let x = c[i];
        let mut j = i;
        while j > 0 && before(&x, &c[j - 1], up) {
            // COPY: a 16-B candidate record shifts one slot (§4.17: candidates are index records).
            c[j] = c[j - 1];
            j -= 1;
        }
        c[j] = x;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{path, reach, segment, sort_cands, Cand, Level, CAND_OPEN_MARKET};
    use crate::switch::{FillLimitsPrice, PathTie};
    use pine_ir::Bar;
    use proptest::prelude::{prop_assert, proptest, ProptestConfig};

    const E: i64 = 1_000_000;

    fn bar(o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            open_1e6: o * E,
            high_1e6: h * E,
            low_1e6: l * E,
            close_1e6: c * E,
            ..Bar::default()
        }
    }

    #[test]
    fn cand_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Cand>(), 16);
        assert_eq!(core::mem::align_of::<Cand>(), 8);
    }

    #[test]
    fn path_goes_to_the_nearer_extreme_first() {
        assert_eq!(
            path(&bar(10, 11, 5, 8), PathTie::LowFirst),
            [10 * E, 11 * E, 5 * E, 8 * E]
        );
        assert_eq!(
            path(&bar(10, 15, 9, 12), PathTie::HighFirst),
            [10 * E, 9 * E, 15 * E, 12 * E]
        );
        // The tie follows the switch (`pin:05-path-tie`).
        assert_eq!(path(&bar(10, 12, 8, 9), PathTie::LowFirst)[1], 8 * E);
        assert_eq!(path(&bar(10, 12, 8, 9), PathTie::HighFirst)[1], 12 * E);
        let p = path(&bar(10, 12, 8, 9), PathTie::LowFirst);
        assert_eq!(segment(&p, 3), (9 * E, 9 * E));
        assert_eq!(segment(&p, 1), (8 * E, 12 * E));
    }

    #[test]
    fn reach_and_fill_price_per_row() {
        let at = FillLimitsPrice::AtLimit;
        // Down segment 10 → 5.
        let (a, b) = (10 * E, 5 * E);
        assert_eq!(reach(Level::Market, a, b, 0, at), Some((a, a)));
        assert_eq!(
            reach(Level::BuyLimit(7 * E), a, b, 0, at),
            Some((7 * E, 7 * E))
        );
        // A buy limit the segment starts through fills at the start.
        assert_eq!(reach(Level::BuyLimit(12 * E), a, b, 0, at), Some((a, a)));
        assert_eq!(reach(Level::BuyLimit(4 * E), a, b, 0, at), None);
        assert_eq!(
            reach(Level::SellStop(6 * E), a, b, 0, at),
            Some((6 * E, 6 * E))
        );
        assert_eq!(reach(Level::BuyStop(11 * E), a, b, 0, at), None);
        // A gapped-through buy stop fills at the start, never better than its stop (I20).
        assert_eq!(reach(Level::BuyStop(9 * E), a, b, 0, at), Some((a, a)));
        assert_eq!(reach(Level::SellLimit(9 * E), a, b, 0, at), Some((a, a)));
    }

    #[test]
    fn fill_limits_verification_is_inclusive_and_fills_at_the_limit() {
        // I40: buy limit 7, d = 1 → V = 6. Segment 10 → 6 reaches V (inclusive): fills at 7.
        let at = FillLimitsPrice::AtLimit;
        assert_eq!(
            reach(Level::BuyLimit(7 * E), 10 * E, 6 * E, E, at),
            Some((6 * E, 7 * E))
        );
        // 10 → 6.5 touches L but not V: no fill.
        assert_eq!(
            reach(Level::BuyLimit(7 * E), 10 * E, 6_500_000, E, at),
            None
        );
        // A segment starting beyond V (gap): AtLimit fills at L, AtStart at the start.
        assert_eq!(
            reach(Level::BuyLimit(7 * E), 5 * E, 4 * E, E, at),
            Some((5 * E, 7 * E))
        );
        let st = FillLimitsPrice::AtStart;
        assert_eq!(
            reach(Level::BuyLimit(7 * E), 5 * E, 4 * E, E, st),
            Some((5 * E, 5 * E))
        );
        // Sell side mirrors it.
        assert_eq!(
            reach(Level::SellLimit(7 * E), 5 * E, 8 * E, E, at),
            Some((8 * E, 7 * E))
        );
        // A saturated V never verifies.
        assert_eq!(
            reach(Level::BuyLimit(i64::MIN + 1), 0, i64::MIN, 10, at),
            None
        );
    }

    #[test]
    fn candidates_sort_in_path_order_ties_by_seq() {
        let c = |key: i64, seq: u32| Cand {
            key,
            seq,
            what: 0,
            flags: 0,
        };
        let mut v = [c(7, 3), c(5, 2), c(7, 1), c(9, 0)];
        sort_cands(&mut v, true);
        assert_eq!(v, [c(5, 2), c(7, 1), c(7, 3), c(9, 0)]);
        sort_cands(&mut v, false);
        assert_eq!(v, [c(9, 0), c(7, 1), c(7, 3), c(5, 2)]);
        // `pin:05-open-market-first`: an open market candidate wins its tie whatever its seq;
        // market against market keeps seq order; it never jumps a different reach.
        let m = |key: i64, seq: u32| Cand {
            flags: CAND_OPEN_MARKET,
            ..c(key, seq)
        };
        let mut v = [c(9, 0), m(9, 5), c(9, 1), m(9, 4), c(8, 6)];
        sort_cands(&mut v, false);
        assert_eq!(v, [m(9, 4), m(9, 5), c(9, 0), c(9, 1), c(8, 6)]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        /// I20 and I21: on every segment of a bar, a fill without verification lies inside the bar;
        /// a limit is never worse than its limit, a stop never better than its stop.
        #[test]
        fn prop_fills_stay_in_the_bar_and_honour_their_level(
            o in 1_i64..1_000, up in 0_i64..500, down in 0_i64..500, cfrac in 0_i64..=100,
            lvl in -100_i64..1_600, kind in 0_u8..5, tie in proptest::bool::ANY,
        ) {
            let (h, l) = (o + up, (o - down).max(0));
            let c = l + (h - l) * cfrac / 100;
            let b = Bar { open_1e6: o, high_1e6: h, low_1e6: l, close_1e6: c, ..Bar::default() };
            let p = path(&b, if tie { PathTie::HighFirst } else { PathTie::LowFirst });
            let lv = match kind {
                0 => Level::Market,
                1 => Level::BuyLimit(lvl),
                2 => Level::SellLimit(lvl),
                3 => Level::BuyStop(lvl),
                _ => Level::SellStop(lvl),
            };
            let mut k = 0;
            while k < 4 {
                let (a, z) = segment(&p, k);
                if let Some((_, px)) = reach(lv, a, z, 0, FillLimitsPrice::AtLimit) {
                    prop_assert!(px >= l && px <= h, "I21");
                    match lv {
                        Level::BuyLimit(x) | Level::SellStop(x) => prop_assert!(px <= x, "I20"),
                        Level::SellLimit(x) | Level::BuyStop(x) => prop_assert!(px >= x, "I20"),
                        Level::Market => {}
                    }
                }
                k += 1;
            }
        }
    }
}
