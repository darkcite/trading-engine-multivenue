// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The fee gate (§4.6.2), D18 units: computed before any bar is stepped; a dropped combination is
//! never stepped (I22). Pure, integer, `i128` intermediates.

use pine_ir::Bar;

/// Basis points ×1e4 — `TrialSummary`'s unit: 1 bp = 10 000.
pub type Bps1e4 = i64;

/// A ratio of exactly 1 in [`Bps1e4`]: 10 000 bps × the 10 000 scale.
pub const BPS_1E4_PER_UNIT: i128 = 100_000_000;

/// The policy constants. They decide what gets screened: printed in every report header, ratified by
/// the operator (§10 Q2), never tuned per run.
pub mod policy {
    /// Fraction of a bar's mean high−low range a bar strategy can capture per bar held (0.25).
    pub const CAPTURE_FRACTION_1E4: i64 = 2_500;
    /// Required gross / cost ratio (1.00).
    pub const HURDLE_MARGIN_1E4: i64 = 10_000;
}

/// Everything the gate computed for one (script, symbol, timeframe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Hurdle {
    /// `2 × taker_bps × 10 000`.
    pub rt_fee_bps: Bps1e4,
    /// `2 × tick`, `tick = mintick_1e6 × BPS_1E4_PER_UNIT / median_close_1e6` (§4.6.2).
    pub rt_tick_bps: Bps1e4,
    /// `rt_fee_bps + rt_tick_bps`.
    pub cost_per_trade_bps: Bps1e4,
    /// Mean over the window of `(high − low) × BPS_1E4_PER_UNIT / close`.
    pub mean_range_bps: Bps1e4,
    /// `CAPTURE_FRACTION_1E4 × mean_range_bps × hold_bars / 10 000`.
    pub gross_per_trade_bps: Bps1e4,
    /// `gross × 10 000 / cost`.
    pub ratio_1e4: i64,
    /// The script's hold prior, bars (§4.7).
    pub hold_bars: u32,
    /// The taker rate used, bps.
    pub taker_bps: u32,
}
const _: () = assert!(core::mem::size_of::<Hurdle>() == 56);
const _: () = assert!(core::mem::align_of::<Hurdle>() == 8);

/// The gate's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HurdleVerdict {
    /// `ratio_1e4 > HURDLE_MARGIN_1E4`: screen it.
    Clear = 0,
    /// Otherwise: step no bar (I22).
    Drop = 1,
    /// `--hurdle-mode annotate`: run it anyway; excluded from every aggregate.
    Annotated = 2,
}

fn sat(v: i128) -> i64 {
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Pure, integer, `i128` intermediates.
// pin:07-rt-tick-is-twice-tick: `rt_tick = 2 × tick` with `tick` floored first — §4.6.2's text and
// worked example (332, not the 333 a single floor of `2 × mintick × …` would give).
// pin:07-hurdle-no-price: `median_close_1e6 ≤ 0` (an empty window) leaves the tick unbounded, so
// the cost saturates and the ratio is 0 (Drop) — the gate never clears what it cannot price.
// pin:07-hurdle-zero-cost: a cost of 0 (taker 0, mintick 0) gives a ratio of `i64::MAX` when the
// gross is positive and 0 otherwise.
pub fn hurdle_of(
    taker_bps: u32,
    mintick_1e6: i64,
    median_close_1e6: i64,
    mean_range_bps: Bps1e4,
    hold_bars: u32,
) -> Hurdle {
    let rt_fee = 2 * i128::from(taker_bps) * 10_000;
    let tick: i128 = if median_close_1e6 > 0 {
        i128::from(mintick_1e6).saturating_mul(BPS_1E4_PER_UNIT) / i128::from(median_close_1e6)
    } else {
        i128::from(i64::MAX)
    };
    let rt_tick = tick.saturating_mul(2);
    let cost = rt_fee.saturating_add(rt_tick);
    let gross = i128::from(policy::CAPTURE_FRACTION_1E4)
        .saturating_mul(i128::from(mean_range_bps))
        .saturating_mul(i128::from(hold_bars))
        / 10_000;
    let ratio = if cost > 0 {
        gross.saturating_mul(10_000) / cost
    } else if gross > 0 {
        i128::from(i64::MAX)
    } else {
        0
    };
    Hurdle {
        rt_fee_bps: sat(rt_fee),
        rt_tick_bps: sat(rt_tick),
        cost_per_trade_bps: sat(cost),
        mean_range_bps,
        gross_per_trade_bps: sat(gross),
        ratio_1e4: sat(ratio),
        hold_bars,
        taker_bps,
    }
}

/// Clear / Drop / Annotated.
pub fn hurdle_decide(h: &Hurdle, annotate: bool) -> HurdleVerdict {
    if h.ratio_1e4 > policy::HURDLE_MARGIN_1E4 {
        HurdleVerdict::Clear
    } else if annotate {
        HurdleVerdict::Annotated
    } else {
        HurdleVerdict::Drop
    }
}

/// `(median close ×1e6, mean range bps ×1e4)` over `rows` (the scored window); `scratch` is reused
/// (`select_nth_unstable`, no sort). One pass per (symbol, timeframe), reused for every script.
// pin:07-median-nearest-rank: the median is the nearest-rank 50 % (`ceil(n / 2)`-th smallest, the
// lower middle for an even `n`); the mean is floored; an empty window is `(0, 0)`.
pub fn window_stats(rows: &[Bar], scratch: &mut Vec<i64>) -> (i64, Bps1e4) {
    let n = rows.len();
    if n == 0 {
        return (0, 0);
    }
    scratch.clear();
    let mut sum: i128 = 0;
    let mut i = 0;
    while i < n {
        let b = &rows[i];
        scratch.push(b.close_1e6);
        if b.close_1e6 > 0 {
            sum = sum.saturating_add(
                i128::from(b.high_1e6 - b.low_1e6).saturating_mul(BPS_1E4_PER_UNIT)
                    / i128::from(b.close_1e6),
            );
        }
        i += 1;
    }
    let k = (n - 1) / 2;
    let (_, med, _) = scratch.select_nth_unstable(k);
    (*med, sat(sum / n as i128))
}
