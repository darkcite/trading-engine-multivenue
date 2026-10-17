// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The script-visible f64 the book keeps beside its integers (CAR-R33-1, r33h).
//!
//! D18 keeps every broker law in integers; this module is the one exception, and it decides
//! nothing: it forms `strategy.netprofit` the way TradingView does — a running f64 sum — so the
//! view can lend it to `pine-std` (`BrokerView::netprofit_f64_bits`). No fill, sizing or risk
//! rule reads it.
//!
//! CAR-R33K-1 (r33m): `strategy.max_drawdown_percent` likewise — a running maximum of an f64 ratio
//! over the closed-equity peak, folded at every marked path point ([`raise_peak`],
//! [`fold_dd_pct`]) and lent as `BrokerView::max_drawdown_pct_f64_bits`. Shared with native (the
//! book is shared); no fill, sizing or risk rule reads it (`risk.rs` keeps its integer
//! closed-equity peak).
//!
//! `px_f64` / `qty_f64` are twins of `pine-std`'s `strategy::px_f64` / `qty_f64`: `pine-broker`
//! depends on `pine-ir` only (impl-05 I2) and `pine-std` may not depend on `pine-broker`
//! (contract §2), so the shared home would be `pine_ir::scale` — a pine-ir change outside
//! CAR-R33-1's grant, requested in the CAR doc. Both copies are pinned to the same captured rows
//! (`tests/r33h.rs` here, `pine-std/tests/r33f.rs` there).

#![allow(clippy::float_arithmetic)]

use pine_ir::scale;

/// `pin:04-px-float-ticks` (twin of `pine-std`'s) — a fill price as the script reads it: its tick
/// count times the f64 mintick, `(px / mintick) as f64 · mintick`; off the tick grid (or with no
/// mintick) `px / 1e6`.
#[inline(always)]
pub(crate) fn px_f64(px_1e6: i64, mt_1e6: i64) -> f64 {
    if mt_1e6 > 0 && px_1e6 % mt_1e6 == 0 {
        (px_1e6 / mt_1e6) as f64 * scale::from_1e6(mt_1e6)
    } else {
        scale::from_1e6(px_1e6)
    }
}

/// `pin:04-qty-float-1e8` (CI-058, twin of `pine-std`'s) — a quantity as the script reads it:
/// `((q_1e6 · 100) as f64) · 1e-8`.
#[inline(always)]
pub(crate) fn qty_f64(q_1e6: i64) -> f64 {
    (i128::from(q_1e6) * 100) as f64 * 1e-8
}

/// One closed slice's script-visible profit (`pin:04-trade-profit-f64`, r33f):
/// `(px(exit) − px(entry)) · qty(q) · pointvalue` over the formed prices and the signed slice
/// quantity, less `fee(entry share) + fee(exit share)`, each share `fee_1e12 / 1e12`.
///
/// `pin:05-net-f64-fees-provisional` (r33h) — E114 (0.1 % commission, one slice per trade) fits
/// `gross − (entry_fee + exit_fee)` on 4320/4320 `strategy.equity` rows; the slice's integer fee
/// converted once (`gross − (entry + exit)_1e12 / 1e12`) fits 4196, and `gross − entry − exit`
/// 4196. Whether TradingView's fee is `fee_1e12 / 1e12` or `px · qty · rate` in f64 is not decided
/// (both fit E114); a pro-rata share of a partly closed trade's entry fee is uncaptured. With no
/// fee the subtraction is `− 0.0`, exact: the commission-free law is r33f's, unchanged.
#[inline(always)]
fn slice_profit_f64(
    entry_px_1e6: i64,
    exit_px_1e6: i64,
    qty_1e6: i64,
    entry_fee_1e12: i128,
    exit_fee_1e12: i128,
    mt_1e6: i64,
    pv_1e6: i64,
) -> f64 {
    let gross = (px_f64(exit_px_1e6, mt_1e6) - px_f64(entry_px_1e6, mt_1e6))
        * qty_f64(qty_1e6)
        * scale::from_1e6(pv_1e6);
    gross - (scale::from_1e12(entry_fee_1e12) + scale::from_1e12(exit_fee_1e12))
}

/// The running sum after one more closed slice, `net + slice_profit_f64(…)`, in close order (E086
/// 4320/4320 rows; the integer sum fits 72). The sum travels as `f64::to_bits` so the book itself
/// holds no float (D18's grep gate covers every file but this one).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
pub(crate) fn add_slice(
    net_bits: u64,
    entry_px_1e6: i64,
    exit_px_1e6: i64,
    qty_1e6: i64,
    entry_fee_1e12: i128,
    exit_fee_1e12: i128,
    mt_1e6: i64,
    pv_1e6: i64,
) -> u64 {
    let p = slice_profit_f64(
        entry_px_1e6,
        exit_px_1e6,
        qty_1e6,
        entry_fee_1e12,
        exit_fee_1e12,
        mt_1e6,
        pv_1e6,
    );
    (f64::from_bits(net_bits) + p).to_bits()
}

/// The view's `netprofit_f64_bits`: the running sum less the open trades' paid entry fees, the
/// integer Σ converted once — the integer view's `pin:05-openprofit-fees` rule (E114: one open
/// trade per row, 4320/4320 equity rows).
#[inline(always)]
pub(crate) fn view_bits(net_bits: u64, open_fees_1e12: i128) -> u64 {
    (f64::from_bits(net_bits) - scale::from_1e12(open_fees_1e12)).to_bits()
}

/// `initial_capital` as the script reads it, `ic_1e12 / 1e12` (`pine-std`'s
/// `strategy.initial_capital` read) — the closed-equity peak's start (CAR-R33K-1).
#[inline(always)]
pub(crate) fn initial_bits(ic_1e12: i128) -> u64 {
    scale::from_1e12(ic_1e12).to_bits()
}

/// The open profit's f64 running sum after one more open trade marked at `px`:
/// `op + (px(p) − px(entry)) · qty(q) · pointvalue`, oldest first from `+0.0` — `pine-std`'s
/// `open_profit_f64` term (`pin:04-openprofit-f64`, r33f; E114 4320/4320), gross of the entry fee,
/// the signed quantity carrying a short's sign. `pin:04-max-dd-pct-uncaptured-provisional`: the
/// order over several open trades is that read's (oldest first), uncaptured here.
#[inline(always)]
pub(crate) fn add_open(
    op_bits: u64,
    px_1e6: i64,
    entry_px_1e6: i64,
    qty_1e6: i64,
    mt_1e6: i64,
    pv_1e6: i64,
) -> u64 {
    let t = (px_f64(px_1e6, mt_1e6) - px_f64(entry_px_1e6, mt_1e6))
        * qty_f64(qty_1e6)
        * scale::from_1e6(pv_1e6);
    (f64::from_bits(op_bits) + t).to_bits()
}

/// `pin:04-max-dd-pct-equity-f64` (CAR-R33K-1, r33m) — the closed-equity peak after a close:
/// `peak = max(peak, ic + NP)`, `NP` the f64 net profit as `strategy.netprofit` reads it
/// ([`view_bits`]); open profit never raises it (E118: a variant whose peak is every marked
/// equity, the highs included, fits 51/4320 rows; the bar-close equity peak 302).
///
/// `pin:04-max-dd-pct-uncaptured-provisional`: taken once per closing fill, after its closed
/// slices and before a reversal's opening part, `NP` net of the still-open trades' paid entry fees
/// (fees in the peak, several trades closed by one fill — uncaptured; E118 closes one trade per
/// fill, commission-free).
#[inline(always)]
pub(crate) fn raise_peak(peak_bits: u64, ic_1e12: i128, np_bits: u64) -> u64 {
    let c = scale::from_1e12(ic_1e12) + f64::from_bits(np_bits);
    if c > f64::from_bits(peak_bits) {
        c.to_bits()
    } else {
        peak_bits
    }
}

/// `pin:04-max-dd-pct-equity-f64` (CAR-R33K-1, r33m) — `strategy.max_drawdown_percent` after
/// one more marked path point: `mddp = max(mddp, (peak − eq) / peak * 100)` with
/// `eq = (ic + NP) + op` (`pine-std`'s `strategy.equity` formation, `pin:04-equity-f64`), in
/// f64 and in exactly that operation order — TV E118 4320/4320 rows bit for bit; `eq = ic + (NP +
/// op)` fits 3230, `(peak − eq) * 100 / peak` 254, `(1 − eq / peak) * 100` 51. `+0.0` before any
/// trade (`eq = peak`). The maximum keeps `mddp` unless the new value is strictly larger.
/// `pin:04-max-dd-pct-uncaptured-provisional`: shorts and several open trades follow the literal
/// law (`op` from [`add_open`]), uncaptured.
#[inline(always)]
pub(crate) fn fold_dd_pct(
    mddp_bits: u64,
    peak_bits: u64,
    ic_1e12: i128,
    np_bits: u64,
    op_bits: u64,
) -> u64 {
    let peak = f64::from_bits(peak_bits);
    let eq = (scale::from_1e12(ic_1e12) + f64::from_bits(np_bits)) + f64::from_bits(op_bits);
    let v = (peak - eq) / peak * 100.0;
    if v > f64::from_bits(mddp_bits) {
        v.to_bits()
    } else {
        mddp_bits
    }
}

#[cfg(test)]
mod tests {
    use super::{
        add_open, add_slice, fold_dd_pct, initial_bits, px_f64, qty_f64, raise_peak,
        slice_profit_f64, view_bits,
    };

    const MT: i64 = 100_000;
    const PV: i64 = 1_000_000;

    #[test]
    fn formations_match_the_captured_texts() {
        // E082 75577.4 → 75577.40000000001 (ticks × mintick); off the grid: px / 1e6.
        assert_eq!(
            px_f64(75_577_400_000, MT).to_bits(),
            75577.40000000001f64.to_bits()
        );
        assert_eq!(px_f64(75_577_450_000, MT).to_bits(), 75577.45f64.to_bits());
        assert_eq!(px_f64(1_500_000, 0).to_bits(), 1.5f64.to_bits());
        // CI-058: 1543003 → 1.5430030000000001; 13 → 0.000013000000000000001.
        assert_eq!(
            qty_f64(1_543_003).to_bits(),
            1.5430030000000001f64.to_bits()
        );
        assert_eq!(qty_f64(13).to_bits(), 0.000013000000000000001f64.to_bits());
        assert_eq!(
            qty_f64(-13).to_bits(),
            (-0.000013000000000000001f64).to_bits()
        );
    }

    #[test]
    fn slice_profit_is_r33f_law_and_fees_subtract_as_two_shares() {
        let (e, x) = (77_249_600_000, 75_653_800_000);
        // E108's first trade: long 1 @ 77249.6, out @ 75653.8 → −1595.800000000003.
        let p = slice_profit_f64(e, x, 1_000_000, 0, 0, MT, PV);
        assert_eq!(p.to_bits(), (-1595.800000000003f64).to_bits());
        // A fee-free slice is the gross exactly (`− 0.0`), a short's sign included.
        let s = slice_profit_f64(e, x, -1_000_000, 0, 0, MT, PV);
        assert_eq!(s.to_bits(), 1595.800000000003f64.to_bits());
        // E114 trade 1 (0.1 %): the two fees converted, summed, then subtracted.
        let (fe, fx) = (77_249_600_000_000, 75_653_800_000_000);
        let f = slice_profit_f64(e, x, 1_000_000, fe, fx, MT, PV);
        assert_eq!(
            f.to_bits(),
            (-1595.800000000003f64 - (77.2496 + 75.6538)).to_bits()
        );
        // The running sum and the view, through the bits.
        let n = add_slice(0, e, x, 1_000_000, 0, 0, MT, PV);
        assert_eq!(n, p.to_bits());
        assert_eq!(add_slice(n, e, x, -1_000_000, 0, 0, MT, PV), 0f64.to_bits());
        assert_eq!(view_bits(n, 0), p.to_bits());
        assert_eq!(view_bits(0, 0), 0f64.to_bits());
        assert_eq!(view_bits(0, fe), (-77.2496f64).to_bits());
    }

    /// CAR-R33K-1's worked rows (TV E118, `pin:04-max-dd-pct-equity-f64`), one fold each.
    #[test]
    fn dd_pct_fold_matches_the_car_worked_rows() {
        let ic: i128 = 1_000_000 * 1_000_000_000_000;
        let peak0 = initial_bits(ic);
        assert_eq!(peak0, 1e6f64.to_bits());
        // Trade 1 open, entry 77249.6, the entry bar's low 76903.2: (1e6 − 999653.6) / 1e6 * 100.
        let op = add_open(0, 76_903_200_000, 77_249_600_000, 1_000_000, MT, PV);
        let m1 = fold_dd_pct(0, peak0, ic, 0, op);
        assert_eq!(m1, 0.03464000000000233f64.to_bits());
        // Before any trade (eq = peak): +0.0, and a smaller value keeps the maximum.
        assert_eq!(fold_dd_pct(0, peak0, ic, 0, 0), 0f64.to_bits());
        let up = add_open(0, 77_254_300_000, 77_249_600_000, 1_000_000, MT, PV);
        assert_eq!(fold_dd_pct(m1, peak0, ic, 0, up), m1);
        // Trade 2 after trade 1's −1595.800000000003: the peak stays 1e6 (1859.2 below the
        // initial capital at its low 75314.0, entry 75577.4).
        let np = add_slice(0, 77_249_600_000, 75_653_800_000, 1_000_000, 0, 0, MT, PV);
        assert_eq!(
            raise_peak(peak0, ic, np),
            peak0,
            "a loss never raises the peak"
        );
        let op = add_open(0, 75_314_000_000, 75_577_400_000, 1_000_000, MT, PV);
        let m2 = fold_dd_pct(m1, peak0, ic, np, op);
        assert_eq!(m2, 0.185920000000007f64.to_bits());
        // `eq = ic + (NP + op)` (3230/4320 rows) reads 0.18591999999999534 here.
        let alt = (1e6 - (1e6 + (f64::from_bits(np) + f64::from_bits(op)))) / 1e6 * 100.0;
        assert_eq!(alt.to_bits(), 0.18591999999999534f64.to_bits());
        // Trade 10 after the closed-equity peak 1003158.2 (trades 1–8), NP −11.000000000014552,
        // entry 67260.0, low 66760.0.
        let peak = 1003158.2f64.to_bits();
        let np = (-11.000000000014552f64).to_bits();
        assert_eq!(raise_peak(peak, ic, np), peak);
        let op = add_open(0, 66_760_000_000, 67_260_000_000, 1_000_000, MT, PV);
        let m10 = fold_dd_pct(m2, peak, ic, np, op);
        assert_eq!(m10, 0.36576484147764066f64.to_bits());
        // A profit raises the peak to `ic + NP` exactly.
        let win = 3158.2f64.to_bits();
        assert_eq!(raise_peak(peak0, ic, win), (1e6f64 + 3158.2f64).to_bits());
        // A short's open profit carries its sign (`pin:04-max-dd-pct-uncaptured-provisional`).
        let s = add_open(0, 77_259_600_000, 77_249_600_000, -1_000_000, MT, PV);
        assert!(f64::from_bits(s) < 0.0);
        assert_eq!(
            f64::from_bits(add_open(
                s,
                77_249_600_000,
                77_249_600_000,
                1_000_000,
                MT,
                PV
            )),
            f64::from_bits(s)
        );
    }
}
