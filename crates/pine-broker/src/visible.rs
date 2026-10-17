// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The script-visible f64 the book keeps beside its integers (CAR-R33-1, r33h).
//!
//! D18 keeps every broker law in integers; this module is the one exception, and it decides
//! nothing: it forms `strategy.netprofit` the way TradingView does — a running f64 sum — so the
//! view can lend it to `pine-std` (`BrokerView::netprofit_f64_bits`). No fill, sizing or risk
//! rule reads it.
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

#[cfg(test)]
mod tests {
    use super::{add_slice, px_f64, qty_f64, slice_profit_f64, view_bits};

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
}
