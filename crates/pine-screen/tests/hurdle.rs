// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.7 — the fee hurdle (§4.6.2).

use pine_ir::{barflag, Bar};
use pine_screen::hurdle::{
    hurdle_decide, hurdle_of, policy, window_stats, Hurdle, HurdleVerdict, BPS_1E4_PER_UNIT,
};

#[test]
fn hurdle_layout_is_frozen() {
    assert_eq!(core::mem::size_of::<Hurdle>(), 56);
    assert_eq!(core::mem::align_of::<Hurdle>(), 8);
    assert_eq!(BPS_1E4_PER_UNIT, 100_000_000);
    assert_eq!(
        (policy::CAPTURE_FRACTION_1E4, policy::HURDLE_MARGIN_1E4),
        (2_500, 10_000)
    );
}

#[test]
fn hurdle_arithmetic_matches_the_worked_example() {
    // Binance USDⓈ-M BTCUSDT: taker 5 bps, mintick $0.10, median close $60 000.
    let h = hurdle_of(5, 100_000, 60_000_000_000, 100_000, 2);
    assert_eq!(h.rt_fee_bps, 100_000);
    assert_eq!(h.rt_tick_bps, 332);
    assert_eq!(h.cost_per_trade_bps, 100_332);
    assert_eq!((h.gross_per_trade_bps, h.ratio_1e4), (50_000, 4_983));
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Drop);
    let h = hurdle_of(5, 100_000, 60_000_000_000, 100_000, 6);
    assert_eq!((h.gross_per_trade_bps, h.ratio_1e4), (150_000, 14_950));
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Clear);
    let h = hurdle_of(5, 100_000, 60_000_000_000, 750_000, 6);
    assert_eq!((h.gross_per_trade_bps, h.ratio_1e4), (1_125_000, 112_127));
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Clear);
    assert_eq!(
        (h.hold_bars, h.taker_bps, h.mean_range_bps),
        (6, 5, 750_000)
    );
}

#[test]
fn hurdle_round_trip_is_two_taker_sides_plus_two_ticks() {
    for taker in [0u32, 1, 2, 5, 10] {
        for (tick_1e6, close) in [
            (10_000i64, 100_000_000i64),
            (100_000, 60_000_000_000),
            (1, 3),
        ] {
            let h = hurdle_of(taker, tick_1e6, close, 1, 1);
            let tick = i128::from(tick_1e6) * BPS_1E4_PER_UNIT / i128::from(close);
            assert_eq!(h.rt_fee_bps, 2 * i64::from(taker) * 10_000);
            assert_eq!(i128::from(h.rt_tick_bps), 2 * tick);
            assert_eq!(h.cost_per_trade_bps, h.rt_fee_bps + h.rt_tick_bps);
        }
    }
    // The margin itself does not clear: DROP iff ratio ≤ 1.00×.
    let mut h = hurdle_of(5, 100_000, 60_000_000_000, 100_000, 2);
    h.ratio_1e4 = policy::HURDLE_MARGIN_1E4;
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Drop);
    h.ratio_1e4 += 1;
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Clear);
    // Edges: no price drops, no cost clears a positive gross.
    let h = hurdle_of(5, 100_000, 0, 750_000, 6);
    assert_eq!(hurdle_decide(&h, false), HurdleVerdict::Drop);
    let h = hurdle_of(0, 0, 60_000_000_000, 750_000, 6);
    assert_eq!(h.ratio_1e4, i64::MAX);
}

fn bar(i: u32, close: i64, range: i64) -> Bar {
    Bar {
        open_1e6: close,
        high_1e6: close + range / 2,
        low_1e6: close - range / 2,
        close_1e6: close,
        volume_1e6: 0,
        open_ms: i64::from(i) * 60_000,
        close_ms: i64::from(i) * 60_000 + 60_000,
        index: i,
        flags: barflag::CONFIRMED,
    }
}

#[test]
fn hurdle_window_stats_median_is_a_median() {
    // Fat-tailed closes: nine at 100, one at 10 000 → median 100, mean ≈ 1 090.
    let mut rows = Vec::new();
    for i in 0..9 {
        rows.push(bar(i, 100_000_000, 1_000_000)); // range 1 % of close → 1 000 000 bps×1e4
    }
    rows.push(bar(9, 10_000_000_000, 100_000_000)); // range 1 %
    let mut scratch = Vec::new();
    let (med, range) = window_stats(&rows, &mut scratch);
    assert_eq!(med, 100_000_000);
    assert_eq!(range, 1_000_000);
    // An even count takes the lower middle (nearest rank); the input is not reordered.
    let even: Vec<Bar> = [4, 1, 3, 2]
        .iter()
        .enumerate()
        .map(|(i, &c)| bar(i as u32, c * 1_000_000, 0))
        .collect();
    assert_eq!(window_stats(&even, &mut scratch), (2_000_000, 0));
    assert_eq!(even[0].close_1e6, 4_000_000);
    assert_eq!(window_stats(&[], &mut scratch), (0, 0));
    // The scratch is reused without growing past the largest window.
    let cap = scratch.capacity();
    let _ = window_stats(&even, &mut scratch);
    assert_eq!(scratch.capacity(), cap);
}

#[test]
fn hurdle_annotate_marks_instead_of_dropping() {
    let h = hurdle_of(5, 100_000, 60_000_000_000, 100_000, 2);
    assert_eq!(hurdle_decide(&h, true), HurdleVerdict::Annotated);
    let c = hurdle_of(5, 100_000, 60_000_000_000, 750_000, 6);
    assert_eq!(hurdle_decide(&c, true), HurdleVerdict::Clear);
    assert_eq!(HurdleVerdict::Annotated as u8, 2);
}
