// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.4 — the one tolerance. This edition carries the `dec` rows (T2); `cmp_*` and
//! `trades_*` land with T16.

use pine_conformance::dec::{money_ok, parse_dec, scaled_half_away, Dec, DecErr};

fn d(s: &str) -> Dec {
    parse_dec(s.as_bytes()).unwrap()
}

#[test]
fn dec_scaled_half_away_rounds_away_from_zero() {
    assert_eq!(scaled_half_away(d("60123.4567895"), 6), 60_123_456_790);
    assert_eq!(scaled_half_away(d("-12.3456785"), 6), -12_345_679);
    assert_eq!(scaled_half_away(d("0.0000005"), 6), 1);
    assert_eq!(scaled_half_away(d("-0.0000005"), 6), -1);
    assert_eq!(scaled_half_away(d("0.0000004999"), 6), 0);
    assert_eq!(scaled_half_away(d("60123.5"), 6), 60_123_500_000);
    assert_eq!(scaled_half_away(d("1"), 6), 1_000_000);
    assert_eq!(scaled_half_away(d("-0.00"), 6), 0);
}

#[test]
fn dec_parse_refuses_separators_and_exponents() {
    assert_eq!(parse_dec(b"1,000.5"), Err(DecErr::BadChar(b',')));
    assert_eq!(parse_dec(b"1e5"), Err(DecErr::BadChar(b'e')));
    assert_eq!(parse_dec(b"1 000"), Err(DecErr::BadChar(b' ')));
    assert_eq!(parse_dec(b"1.2.3"), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b".5"), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b"5."), Err(DecErr::BadChar(b'.')));
    assert_eq!(parse_dec(b""), Err(DecErr::Empty));
    assert_eq!(parse_dec(b"-"), Err(DecErr::Empty));
    assert_eq!(parse_dec(&[b'9'; 37]), Err(DecErr::TooLong));
    assert!(parse_dec(&[b'9'; 36]).is_ok());
    assert_eq!(
        d("-376.50"),
        Dec {
            neg: true,
            m: 37650,
            d: 2
        }
    );
    assert_eq!(
        d("+1.5"),
        Dec {
            neg: false,
            m: 15,
            d: 1
        }
    );
    for t in ["376.50", "-0.00", "0.0000005", "12", "-12.3456785"] {
        assert_eq!(d(t).render(), t);
    }
}

#[test]
fn dec_money_is_within_half_a_printed_unit() {
    let p = d("376.50");
    let lo: i128 = 376_495_000_000_000;
    let hi: i128 = 376_505_000_000_000;
    assert!(money_ok(lo, p));
    assert!(money_ok(hi, p));
    assert!(money_ok(376_500_000_000_000, p));
    assert!(!money_ok(lo - 1, p));
    assert!(!money_ok(hi + 1, p));
    // Negative values and a printed precision finer than ×1e12.
    assert!(money_ok(-376_505_000_000_000, d("-376.50")));
    assert!(!money_ok(-376_505_000_000_001, d("-376.50")));
    // 13 decimals: half a printed unit is 0.05 ×1e-12, so only the exact unit matches.
    let fine = d("0.0000000000010");
    assert!(money_ok(1, fine) && !money_ok(0, fine) && !money_ok(2, fine));
    assert!(!money_ok(1, d("0.0000000000005")) && !money_ok(0, d("0.0000000000005")));
    // Overflow fails closed.
    assert!(!money_ok(i128::MAX, d("1")));
}

use pine_conformance::cmp::{compare_cell, compare_cell_fmt, format_sig, ulps_between, Cmp};
use pine_conformance::fixture::{Cell, Tol};

fn val(v: f64, text: &str) -> Cell {
    Cell::Val {
        v,
        text: text.to_string(),
    }
}

#[test]
fn cmp_ulps_across_zero_and_sign() {
    assert_eq!(ulps_between(-0.0, 0.0), 1);
    assert_eq!(ulps_between(f64::INFINITY, f64::MAX), 1);
    assert_eq!(ulps_between(1.0, 1.0), 0);
    let tiny = f64::from_bits(1);
    assert_eq!(ulps_between(-tiny, tiny), 3);
}

#[test]
fn cmp_exact_requires_bit_identity() {
    let c = compare_cell(&val(0.3, "0.3"), 0.1 + 0.2, Tol::Exact);
    assert!(matches!(c, Cmp::Divergent { within: false, .. }));
    assert_eq!(compare_cell(&val(0.3, "0.3"), 0.3, Tol::Exact), Cmp::Equal);
    assert!(compare_cell(&val(0.3, "0.3"), 0.1 + 0.2, Tol::Ulp(1)).ok());
}

#[test]
fn cmp_printed_zero_matches_either_sign() {
    assert_eq!(compare_cell(&val(0.0, "0"), -0.0, Tol::Exact), Cmp::Equal);
    assert_eq!(compare_cell(&val(0.0, "0"), 0.0, Tol::Exact), Cmp::Equal);
    assert!(!compare_cell_fmt(&val(0.0, "0"), -0.0, Tol::Exact, true).ok());
    assert!(!compare_cell(&val(-0.0, "-0.0"), 0.0, Tol::Exact).ok());
}

#[test]
fn cmp_na_matches_only_na() {
    assert_eq!(compare_cell(&Cell::Na, f64::NAN, Tol::Exact), Cmp::Equal);
    assert!(!compare_cell(&Cell::Na, 0.0, Tol::Ulp(64)).ok());
    assert!(!compare_cell(&Cell::Na, f64::INFINITY, Tol::Dec(3)).ok());
    assert!(!compare_cell(&val(0.0, "0"), f64::NAN, Tol::Dec(3)).ok());
}

#[test]
fn cmp_inf_matches_only_itself() {
    assert_eq!(
        compare_cell(&val(f64::INFINITY, "inf"), f64::INFINITY, Tol::Exact),
        Cmp::Equal
    );
    assert!(!compare_cell(&val(f64::INFINITY, "inf"), 1e308, Tol::Ulp(u32::MAX)).ok());
    assert!(!compare_cell(&val(f64::MAX, "x"), f64::INFINITY, Tol::Ulp(64)).ok());
}

#[test]
fn cmp_dec_absorbs_half_up_rendering() {
    // Text arm: round-half-even shortest form.
    assert_eq!(format_sig(1234.5678, 6), "1234.57");
    assert_eq!(format_sig(0.000123456, 3), "0.000123");
    assert_eq!(format_sig(123456789.0, 3), "123000000");
    assert!(compare_cell(&val(1234.57, "1234.57"), 1234.5678, Tol::Dec(6)).ok());
    // Bound arm: a half-up rendering of an exact tie (12.5 → "13" at 2 sig; half-even says "12").
    assert_eq!(format_sig(12.5, 2), "12");
    assert!(compare_cell(&val(13.0, "13"), 12.5, Tol::Dec(2)).ok());
    assert!(!compare_cell(&val(13.0, "13"), 12.25, Tol::Dec(2)).ok());
    // pin:06-dec-bound-exact — the arm is exact decimal: a fractional half-up tie (0.125 → "0.13";
    // in f64, 0.13 − 0.125 = 0.005000000000000004 > 0.005) is inside; one ULP past it is not.
    assert_eq!(format_sig(0.125, 2), "0.12");
    assert!(compare_cell(&val(0.13, "0.13"), 0.125, Tol::Dec(2)).ok());
    let below = f64::from_bits(0.125f64.to_bits() - 1);
    assert!(!compare_cell(&val(0.13, "0.13"), below, Tol::Dec(2)).ok());
    assert!(compare_cell(&val(-0.13, "-0.13"), -0.125, Tol::Dec(2)).ok());
    // 0.135 is not exactly representable; its f64 lies just above 0.135 — outside "0.13" ± 0.005.
    assert!(!compare_cell(&val(0.13, "0.13"), 0.135, Tol::Dec(2)).ok());
    // Large magnitude: "60000" at 2 sig absorbs ±500 exactly.
    assert!(compare_cell(&val(60000.0, "60000"), 60500.0, Tol::Dec(2)).ok());
    let past = f64::from_bits(60500f64.to_bits() + 1);
    assert!(!compare_cell(&val(60000.0, "60000"), past, Tol::Dec(2)).ok());
}

#[test]
fn cmp_dec_rejects_a_digit_difference() {
    assert!(!compare_cell(&val(1234.58, "1234.58"), 1234.5678, Tol::Dec(6)).ok());
}

#[test]
fn cmp_ulp_bound_table() {
    // §4.4.2: single-pass window min(4·len, 64); ema ⌈(len+1)/2⌉ + 2; rma len + 2.
    let window = |len: u32| (4 * len).min(64);
    assert_eq!([window(2), window(14), window(200)], [8, 56, 64]);
    let ema = |len: u32| len.div_ceil(2) + 2;
    let ema14 = (14u32 + 1).div_ceil(2) + 2;
    assert_eq!(ema14, 10);
    assert_eq!(ema(2), 3);
    let x = 1500.0f64;
    let y = f64::from_bits(x.to_bits() + 64);
    assert!(compare_cell(&val(x, "1500"), y, Tol::Ulp(window(200))).ok());
    let z = f64::from_bits(x.to_bits() + 65);
    assert!(!compare_cell(&val(x, "1500"), z, Tol::Ulp(window(200))).ok());
}

use pine_conformance::fixture::{ExitCols, Summary, TradeRow};
use pine_conformance::trades::{compare_summary, compare_trades, PnlBasis, SignalTable, TradeDiv};
use pine_ir::{trade_flag, RunRecord, SymTable, TradeRec};

fn rec(entry_ms: i64, px: i64, qty: i64, pnl: i128, fee: i128) -> TradeRec {
    TradeRec {
        entry_id: pine_ir::str_key(b"L"),
        entry_ms,
        exit_ms: entry_ms + 3_600_000,
        entry_px_1e6: px,
        exit_px_1e6: px + 1_000_000,
        qty_1e6: qty,
        entry_bar: 1,
        exit_bar: 2,
        pnl_1e12: pnl,
        fee_1e12: fee,
        flags: trade_flag::PNL_NET,
        exit_id: pine_ir::str_key(b"X"),
        ..TradeRec::default()
    }
}

fn row(n: u32, entry_ms: i64, px: &str, pnl: &str) -> TradeRow {
    TradeRow {
        n,
        long: true,
        entry_ms,
        entry_px: d(px),
        entry_sig: "L".into(),
        exit: Some(ExitCols {
            ms: entry_ms + 3_600_000,
            px: d("101"),
            sig: "X".into(),
        }),
        qty: d("1"),
        pnl: Some(d(pnl)),
        cum_pnl: None,
        runup: None,
        drawdown: None,
        bars: Some(1),
    }
}

#[test]
fn trades_count_mismatch_reported_first() {
    let syms = SymTable::new();
    let e = [row(1, 0, "100", "1.00"), row(2, 7_200_000, "999", "1.00")];
    let a = [rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0)];
    let got = compare_trades(&e, &a, &syms, &SignalTable::default(), PnlBasis::Net);
    assert_eq!(
        got,
        Some(TradeDiv::Count {
            expected: 2,
            actual: 1,
            first_mismatch: 1
        })
    );
}

#[test]
fn trades_price_is_exact_at_1e6() {
    let syms = SymTable::new();
    let e = [row(1, 0, "100.000001", "1.00")];
    let a = [rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0)];
    let got = compare_trades(&e, &a, &syms, &SignalTable::default(), PnlBasis::Net);
    assert!(matches!(
        got,
        Some(TradeDiv::Field {
            field: "entry_px",
            ..
        })
    ));
    let ok = [row(1, 0, "100", "1.00")];
    assert_eq!(
        compare_trades(&ok, &a, &syms, &SignalTable::default(), PnlBasis::Net),
        None
    );
}

#[test]
fn trades_pnl_net_and_gross_bases() {
    let syms = SymTable::new();
    // pnl net 1.00, fee 0.10 → gross 1.10.
    let a = [rec(
        0,
        100_000_000,
        1_000_000,
        1_000_000_000_000,
        100_000_000_000,
    )];
    let s = SignalTable::default();
    assert_eq!(
        compare_trades(&[row(1, 0, "100", "1.00")], &a, &syms, &s, PnlBasis::Net),
        None
    );
    assert_eq!(
        compare_trades(&[row(1, 0, "100", "1.10")], &a, &syms, &s, PnlBasis::Gross),
        None
    );
    assert!(compare_trades(&[row(1, 0, "100", "1.10")], &a, &syms, &s, PnlBasis::Net).is_some());
}

#[test]
fn trades_size_is_exact() {
    let syms = SymTable::new();
    let s = SignalTable::default();
    let a = [rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0)];
    let mut e = row(1, 0, "100", "1.00");
    e.qty = d("1.000001");
    assert!(matches!(
        compare_trades(&[e.clone()], &a, &syms, &s, PnlBasis::Net),
        Some(TradeDiv::Field { field: "qty", .. })
    ));
    e.qty = d("1.0000005");
    assert!(matches!(
        compare_trades(&[e], &a, &syms, &s, PnlBasis::Net),
        Some(TradeDiv::Field { field: "qty", .. })
    ));
    // A short trade's size is the magnitude; the side is its own field.
    let short = [rec(0, 100_000_000, -1_000_000, 1_000_000_000_000, 0)];
    assert!(matches!(
        compare_trades(
            &[row(1, 0, "100", "1.00")],
            &short,
            &syms,
            &s,
            PnlBasis::Net
        ),
        Some(TradeDiv::Field { field: "side", .. })
    ));
    let mut es = row(1, 0, "100", "1.00");
    es.long = false;
    assert_eq!(
        compare_trades(&[es], &short, &syms, &s, PnlBasis::Net),
        None
    );
}

#[test]
fn trades_open_trade_compares_entry_fields_only() {
    let syms = SymTable::new();
    let s = SignalTable::default();
    let mut a = rec(0, 100_000_000, 1_000_000, 7_000_000_000_000, 0);
    a.flags |= trade_flag::OPEN;
    let mut e = row(1, 0, "100", "999.99");
    e.exit = None;
    e.cum_pnl = Some(d("123.45"));
    // The marked P&L (and the running total) of an open trade is ignored.
    assert_eq!(
        compare_trades(&[e.clone()], &[a], &syms, &s, PnlBasis::Net),
        None
    );
    // Its entry fields are not.
    let mut px = e.clone();
    px.entry_px = d("100.5");
    assert!(matches!(
        compare_trades(&[px], &[a], &syms, &s, PnlBasis::Net),
        Some(TradeDiv::Field {
            field: "entry_px",
            ..
        })
    ));
    // Its OPEN flag is not: an exported exit against our open trade diverges, and vice versa.
    assert!(matches!(
        compare_trades(&[row(1, 0, "100", "7.00")], &[a], &syms, &s, PnlBasis::Net),
        Some(TradeDiv::Field { field: "open", .. })
    ));
    let mut closed = a;
    closed.flags &= !trade_flag::OPEN;
    assert!(matches!(
        compare_trades(&[e], &[closed], &syms, &s, PnlBasis::Net),
        Some(TradeDiv::Field { field: "open", .. })
    ));
}

#[test]
fn trades_exit_signal_from_signals_table() {
    let syms = SymTable::new();
    let s = SignalTable::parse(
        "liquidation\tMargin call\nrisk_flatten\tRisk flatten\nclose\tClose entry\nclose_all\tClose all\n",
    )
    .unwrap();
    let idless = |flags: u32| {
        let mut a = rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0);
        a.exit_id = pine_ir::StrKey::NONE;
        a.flags = trade_flag::PNL_NET | flags;
        a
    };
    let with_sig = |sig: &str| {
        let mut e = row(1, 0, "100", "1.00");
        e.exit.as_mut().unwrap().sig = sig.into();
        e
    };
    let cmp = |e: TradeRow, a: TradeRec| compare_trades(&[e], &[a], &syms, &s, PnlBasis::Net);
    let liq = idless(trade_flag::LIQUIDATION);
    assert_eq!(cmp(with_sig("Margin call"), liq), None);
    assert!(matches!(
        cmp(with_sig("Close entry"), liq),
        Some(TradeDiv::Field {
            field: "exit_sig",
            ..
        })
    ));
    let flat = idless(trade_flag::RISK_FLATTEN);
    assert_eq!(cmp(with_sig("Risk flatten"), flat), None);
    assert!(cmp(with_sig("Margin call"), flat).is_some());
    // pin:06-exit-cause-close — an id-less plain exit matches either close text.
    let plain = idless(0);
    assert_eq!(cmp(with_sig("Close entry"), plain), None);
    assert_eq!(cmp(with_sig("Close all"), plain), None);
    assert!(cmp(with_sig("Margin call"), plain).is_some());
    // A user exit id wins over the table.
    assert_eq!(
        cmp(
            with_sig("X"),
            rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0)
        ),
        None
    );
}

#[test]
fn trades_require_pnl_net_flag() {
    let syms = SymTable::new();
    let mut a = rec(0, 100_000_000, 1_000_000, 1_000_000_000_000, 0);
    a.flags = 0;
    let got = compare_trades(
        &[row(1, 0, "100", "1.00")],
        &[a],
        &syms,
        &SignalTable::default(),
        PnlBasis::Net,
    );
    assert!(matches!(
        got,
        Some(TradeDiv::Field {
            field: "pnl_net_flag",
            ..
        })
    ));
}

#[test]
fn summary_against_run_record() {
    let s = Summary {
        net: d("376.50"),
        gross_profit: d("500.00"),
        gross_loss: d("123.50"),
        commission: d("0"),
        max_drawdown: d("10.00"),
        closed: 3,
        wins: 2,
        losses: 1,
    };
    let r = RunRecord {
        net_profit_1e12: 376_504_000_000_000,
        gross_profit_1e12: 500_000_000_000_000,
        gross_loss_1e12: 123_500_000_000_000,
        commission_paid_1e12: 0,
        max_drawdown_1e12: 10_000_000_000_000,
        wins: 2,
        losses: 1,
        ..RunRecord::default()
    };
    assert_eq!(compare_summary(&s, &r, 3), None);
    assert!(matches!(
        compare_summary(&s, &r, 4),
        Some(TradeDiv::Field {
            field: "closed",
            ..
        })
    ));
    let off = RunRecord {
        net_profit_1e12: 376_506_000_000_000,
        ..r
    };
    assert!(matches!(
        compare_summary(&s, &off, 3),
        Some(TradeDiv::Field { field: "net", .. })
    ));
}
