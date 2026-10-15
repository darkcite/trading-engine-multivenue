// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The quote gate and the protective band (impl-05 §4.12.2, §4.12.3 gate 1). Pure functions.

use crate::money::{ceil_tick, floor_tick, ticks_px};
use pine_ir::Quote;

/// Gate 1: a fresh two-sided quote on event time.
pub(crate) fn fresh(q: &Quote, now_ns: u64, stale_ns: i64) -> bool {
    let stale = u64::try_from(stale_ns).unwrap_or(0);
    q.flags & 1 == 0
        && q.bid_1e6 > 0
        && q.ask_1e6 > 0
        && q.bid_1e6 <= q.ask_1e6
        && q.ts_ns <= now_ns
        && now_ns - q.ts_ns <= stale
}

/// The protective band: `ask + cross·tick` to buy, `bid − cross·tick` to sell, rounded to the
/// tick toward the protective side (buy down, sell up) and never below one tick.
pub(crate) fn band(buy: bool, q: &Quote, cross_ticks: u32, tick_1e6: i64) -> i64 {
    let d = ticks_px(i64::from(cross_ticks).saturating_mul(1_000_000), tick_1e6);
    let px = if buy {
        floor_tick(q.ask_1e6.saturating_add(d), tick_1e6)
    } else {
        ceil_tick(q.bid_1e6.saturating_sub(d), tick_1e6)
    };
    px.max(tick_1e6)
}

/// An IoC's price: the band, tightened (never widened) by the script's own limit (§4.12.2).
pub(crate) fn ioc_px(buy: bool, band_1e6: i64, limit_1e6: Option<i64>, tick_1e6: i64) -> i64 {
    let px = match limit_1e6 {
        Some(l) if buy => floor_tick(l, tick_1e6).min(band_1e6),
        Some(l) => ceil_tick(l, tick_1e6).max(band_1e6),
        None => band_1e6,
    };
    px.max(tick_1e6)
}

#[cfg(test)]
mod tests {
    use super::{band, fresh, ioc_px};
    use pine_ir::Quote;

    const E: i64 = 1_000_000;

    fn q() -> Quote {
        Quote {
            bid_1e6: 100 * E,
            ask_1e6: 100 * E + 10_000,
            ts_ns: 1_000,
            ..Quote::default()
        }
    }

    #[test]
    fn freshness_rules() {
        assert!(fresh(&q(), 1_000, 5));
        assert!(fresh(&q(), 1_005, 5));
        assert!(!fresh(&q(), 1_006, 5), "stale");
        assert!(!fresh(&q(), 999, 5), "from the future");
        assert!(!fresh(&Quote { flags: 1, ..q() }, 1_000, 5), "stale flag");
        assert!(
            !fresh(
                &Quote {
                    bid_1e6: 101 * E,
                    ..q()
                },
                1_000,
                5
            ),
            "crossed"
        );
        assert!(!fresh(&Quote { bid_1e6: 0, ..q() }, 1_000, 5), "one-sided");
    }

    #[test]
    fn band_is_protective_and_a_limit_only_tightens() {
        let t = 10_000; // 0.01
        assert_eq!(band(true, &q(), 5, t), 100 * E + 60_000);
        assert_eq!(band(false, &q(), 5, t), 100 * E - 50_000);
        // A buy limit above the band cannot widen it; below, it tightens.
        assert_eq!(
            ioc_px(true, 100 * E + 60_000, Some(200 * E), t),
            100 * E + 60_000
        );
        assert_eq!(
            ioc_px(true, 100 * E + 60_000, Some(100 * E + 25_555), t),
            100 * E + 20_000
        );
        assert_eq!(
            ioc_px(false, 100 * E - 50_000, Some(-5), t),
            100 * E - 50_000
        );
        // Never below one tick.
        assert_eq!(
            band(
                false,
                &Quote {
                    bid_1e6: 10_000,
                    ..q()
                },
                5,
                t
            ),
            t
        );
    }
}
