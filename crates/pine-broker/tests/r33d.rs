// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33d acceptance — the TV broker laws fitted to the captured E fixtures, replayed through the
//! public surface on the captured bars (prices bit-exact, ×1e6). Each test names the fixture and
//! trade whose List-of-Trades row it reproduces; the synthetic ones say so.

use pine_broker::{TvBroker, TvStep};
use pine_ir::{
    decl_flag, intent_flag as f, trade_flag, Bar, ChartInfo, CommissionType, IntentKind,
    IntentSide, OcaType, OrderIntent, ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;
const E12: i128 = 1_000_000_000_000;

/// A decimal text as ×1e6 (test-only parsing).
fn d(s: &str) -> i64 {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (i, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut v = i.parse::<i64>().unwrap_or_else(|e| panic!("{e:?}")) * E;
    let mut scale = E / 10;
    for c in frac.chars() {
        v += i64::from(c.to_digit(10).unwrap_or_else(|| panic!("digit"))) * scale;
        scale /= 10;
    }
    if neg {
        -v
    } else {
        v
    }
}

/// BTCUSDT.P-like chart: mintick 0.1, point value 1, quantity step 1e-6; 1 000 000 USD, one
/// contract per entry, pyramiding 1.
fn meta(flags: u16) -> ProgramMeta {
    ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 3_600,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 100_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 1_000_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 1,
            flags,
            ..StrategyDecl::default()
        },
        uses_intrabar: flags & decl_flag::CALC_ON_ORDER_FILLS != 0,
        ..ProgramMeta::default()
    }
}

const CFG: TvCfg = TvCfg {
    sizing: SizingMode::AsDeclared,
    day_offset_s: 0,
    _pad: 0,
};

fn tv(m: &ProgramMeta) -> TvBroker {
    TvBroker::new(m, CFG).unwrap_or_else(|c| panic!("{c:?}"))
}

/// Hourly bar `i` (open at `i` hours) from decimal texts.
fn bar(i: u32, o: &str, h: &str, l: &str, c: &str) -> Bar {
    Bar {
        open_1e6: d(o),
        high_1e6: d(h),
        low_1e6: d(l),
        close_1e6: d(c),
        index: i,
        open_ms: i64::from(i) * 3_600_000,
        close_ms: i64::from(i + 1) * 3_600_000,
        ..Bar::default()
    }
}

/// One bar: `fill` goes to its first `calc_on_order_fills` execution, `close` to its bar-close
/// evaluation.
fn run(b: &mut TvBroker, x: &Bar, fill: &[OrderIntent], close: &[OrderIntent]) {
    let mut step = b.open_bar(x);
    let mut sent = false;
    while let TvStep::Evaluate { intrabar, .. } = step {
        let s: &[OrderIntent] = if !intrabar {
            close
        } else if sent {
            &[]
        } else {
            sent = true;
            fill
        };
        step = b.submit(s).unwrap_or_else(|e| panic!("{e:?}"));
    }
}

fn entry(id: u64, side: IntentSide) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Entry,
        side,
        id: StrKey(id),
        seq: u32::try_from(id).unwrap_or(0),
        ..OrderIntent::default()
    }
}

fn long(id: u64) -> OrderIntent {
    entry(id, IntentSide::Long)
}

fn close(id: u64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(id),
        seq: 50,
        ..OrderIntent::default()
    }
}

fn close_all() -> OrderIntent {
    OrderIntent {
        kind: IntentKind::CloseAll,
        seq: 51,
        ..OrderIntent::default()
    }
}

/// `strategy.exit(id, from, profit = ticks)`.
fn exit_profit(id: u64, from: u64, ticks: i64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(id),
        from_entry: StrKey(from),
        flags: f::PROFIT,
        profit_ticks_1e6: ticks * E,
        seq: 40,
        ..OrderIntent::default()
    }
}

fn exit_px(b: &TvBroker, i: u32) -> Option<i64> {
    b.closed_trade(i).map(|t| t.exit_px_1e6)
}

// --- `pin:05-cof-next-tick` (SamePoint) ------------------------------------------------------

#[test]
fn cof_a_point_execution_admits_its_orders_at_that_point_e016_trade_43() {
    // E016: trade 42 closes at 1791140400000's open (85296.8); the execution there re-enters —
    // TV trade 43 enters at 85296.8, the same point (the reading before r33d: the low, 85296.6).
    let mut b = tv(&meta(decl_flag::CALC_ON_ORDER_FILLS));
    run(
        &mut b,
        &bar(0, "85328.1", "85428.3", "85268.5", "85277.9"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(1, "85277.8", "85322.0", "85270.0", "85296.8"),
        &[],
        &[close(1)],
    );
    run(
        &mut b,
        &bar(2, "85296.8", "85553.6", "85296.6", "85387.4"),
        &[long(1)],
        &[],
    );
    assert_eq!(exit_px(&b, 0), Some(d("85296.8")));
    assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(d("85296.8")));
}

#[test]
fn cof_an_execution_inside_a_segment_waits_for_the_next_point_e025_trade_37() {
    // E025: trade 36's take-profit (+2000 ticks) fills at 84371.5 inside the L→H segment of
    // 1790323200000; the entry its execution places fills at the high, 84470.5 (trade 37) — the
    // guard on SamePoint: `RestOfSegment` would fill it at 84371.5.
    let mut b = tv(&meta(decl_flag::CALC_ON_ORDER_FILLS));
    run(
        &mut b,
        &bar(0, "84152.0", "84246.6", "83885.7", "84171.5"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(1, "84171.5", "84200.0", "84100.0", "84150.0"),
        &[],
        &[exit_profit(9, 1, 2000)],
    );
    run(
        &mut b,
        &bar(2, "83994.7", "84470.5", "83973.5", "84376.1"),
        &[long(1)],
        &[],
    );
    assert_eq!(exit_px(&b, 0), Some(d("84371.5")));
    assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(d("84470.5")));
}

#[test]
fn cof_a_point_execution_admits_its_exit_legs_e025_trade_39() {
    // E025 trade 39: the entry fills at 1790600400000's open (83546.4); the take-profit its
    // execution sets (+2000 ticks, 83746.4) fills at its limit on O→H, not at the high 83794.9.
    let mut b = tv(&meta(decl_flag::CALC_ON_ORDER_FILLS));
    run(
        &mut b,
        &bar(0, "83054.5", "83609.4", "82979.9", "83546.5"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(1, "83546.4", "83794.9", "83270.0", "83609.1"),
        &[exit_profit(9, 1, 2000)],
        &[],
    );
    assert_eq!(
        b.closed_trade(0).map(|t| t.entry_px_1e6),
        Some(d("83546.4"))
    );
    assert_eq!(exit_px(&b, 0), Some(d("83746.4")));
}

// --- `pin:05-trail-arm-at-point` -------------------------------------------------------------

/// E012's `strategy.exit("X1", "L1", trail_points = 2000, trail_offset = 500)`: entry at
/// `entry_bar`'s open, the exit placed at its close, the exit price on `next`.
fn e012_exit(entry_bar: [&str; 4], next: [&str; 4]) -> Option<i64> {
    let mut b = tv(&meta(0));
    let x = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::TRAIL_POINTS | f::TRAIL_OFFSET,
        trail_points_1e6: 2_000 * E,
        trail_offset_1e6: 500 * E,
        seq: 40,
        ..OrderIntent::default()
    };
    let o = entry_bar[0];
    run(&mut b, &bar(0, o, o, o, o), &[], &[long(1)]);
    let [o1, h1, l1, c1] = entry_bar;
    run(&mut b, &bar(1, o1, h1, l1, c1), &[], &[x]);
    let [o2, h2, l2, c2] = next;
    run(&mut b, &bar(2, o2, h2, l2, c2), &[], &[x]);
    exit_px(&b, 0)
}

#[test]
fn trail_gapped_past_its_activation_arms_at_the_open_e012_trade_15() {
    // Activation 80873.4; the next bar opens 80960.1 and falls to 80882.7: 80960.1 − 50.
    let x = e012_exit(
        ["80673.4", "81168.4", "80635.1", "80960.1"],
        ["80960.1", "81239.1", "80882.7", "81018.1"],
    );
    assert_eq!(x, Some(d("80910.1")));
}

#[test]
fn trail_placed_at_the_close_anchors_at_the_close_e012_trade_13() {
    // Activation 79835.0, placed at the close 79849.3; the next bar opens 79849.2: 79849.3 − 50.
    let x = e012_exit(
        ["79635.0", "79951.5", "79626.0", "79849.3"],
        ["79849.2", "80044.6", "79743.3", "79826.3"],
    );
    assert_eq!(x, Some(d("79799.3")));
}

#[test]
fn trail_armed_at_the_close_moves_to_a_better_open_e012_trade_6() {
    // Armed at the close 78613.5; the next bar opens 78614.4: 78614.4 − 50.
    let x = e012_exit(
        ["78274.7", "78870.3", "77988.1", "78613.5"],
        ["78614.4", "79338.0", "78319.1", "79263.7"],
    );
    assert_eq!(x, Some(d("78564.4")));
}

// --- `pin:05-pyramiding-check` (AtGeneration) ------------------------------------------------

fn limit(id: u64, px: &str) -> OrderIntent {
    OrderIntent {
        flags: f::LIMIT,
        limit_1e6: d(px),
        ..long(id)
    }
}

fn stop(id: u64, side: IntentSide, px: &str) -> OrderIntent {
    OrderIntent {
        flags: f::STOP,
        stop_1e6: d(px),
        ..entry(id, side)
    }
}

#[test]
fn pyramiding_an_entry_generated_full_never_fills_e035() {
    // Synthetic, E035's shape: pyramiding 2, a limit placed with 2 of 2 trades open; the position
    // closes and the price reaches the limit — it never fills (the fill-time reading filled it).
    let mut m = meta(0);
    m.decl.pyramiding = 2;
    let mut b = tv(&m);
    run(&mut b, &bar(0, "100", "100", "100", "100"), &[], &[long(1)]);
    run(&mut b, &bar(1, "100", "101", "99", "100"), &[], &[long(2)]);
    run(
        &mut b,
        &bar(2, "100", "101", "99", "100"),
        &[],
        &[limit(3, "90"), close_all()],
    );
    assert_eq!(b.view().opentrades, 2);
    run(&mut b, &bar(3, "100", "101", "85", "95"), &[], &[]);
    run(&mut b, &bar(4, "95", "96", "85", "90"), &[], &[]);
    assert_eq!(b.view().position_size_1e6, 0);
    assert_eq!(b.view().closedtrades, 2);
    assert_eq!(b.counters().entries_refused_pyramiding, 1);
}

#[test]
fn pyramiding_is_not_counted_at_the_fill_e121_trade_11() {
    // Synthetic, E121's shape (pyramiding 1): a limit (95) placed flat on bar 0, a stop (105)
    // placed flat on bar 1; the stop fills on bar 2's O→H, the limit on H→L while that trade is
    // open — TV trade 11 fills `L1` that way (the fill-time count refused it: the two were born at
    // different evaluations, so the same-tick exception does not apply).
    let mut b = tv(&meta(0));
    run(
        &mut b,
        &bar(0, "100", "100", "100", "100"),
        &[],
        &[limit(1, "95")],
    );
    run(
        &mut b,
        &bar(1, "100", "101", "99", "100"),
        &[],
        &[stop(2, IntentSide::Long, "105")],
    );
    run(&mut b, &bar(2, "100", "106", "93", "100"), &[], &[]);
    assert_eq!(b.view().opentrades, 2);
    assert_eq!(b.view().position_size_1e6, 2 * E);
    assert_eq!(b.counters().entries_refused_pyramiding, 0);
}

// --- `pin:05-reversal-at-generation` ---------------------------------------------------------

#[test]
fn an_entry_born_flat_closes_at_most_its_quantity_e057_trade_2() {
    // Synthetic, E057's shape (`oca.none`): LA (buy stop 105) and SB (sell stop 95) are placed
    // flat; SB opens a 1-lot short; LA fills later and only closes it — position 0, the exit `LA`.
    let mut b = tv(&meta(0));
    run(
        &mut b,
        &bar(0, "100", "100", "100", "100"),
        &[],
        &[
            stop(1, IntentSide::Long, "105"),
            stop(2, IntentSide::Short, "95"),
        ],
    );
    run(&mut b, &bar(1, "100", "101", "94", "96"), &[], &[]);
    assert_eq!(b.view().position_size_1e6, -E);
    run(&mut b, &bar(2, "96", "106", "95.5", "104"), &[], &[]);
    assert_eq!(b.view().position_size_1e6, 0);
    assert_eq!(b.view().opentrades, 0);
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!((c.exit_px_1e6, c.exit_id), (d("105"), StrKey(1)));
}

// --- `pin:05-oca-reduce-default` -------------------------------------------------------------

fn oca_reduce(mut it: OrderIntent) -> OrderIntent {
    it.oca_name = StrKey(77);
    it.oca_type = OcaType::Reduce;
    it
}

#[test]
fn a_used_up_default_sized_reduce_member_dies_e061_trade_4() {
    // Synthetic, E061's shape (fixed 1 contract, `oca.reduce`): LA fills, so SB (default 1) is
    // used up and dies; SB placed again later is a new order and opens a 1-lot short (TV trade 4,
    // `SB` short) — before r33d the re-placed SB kept the reduction and sized to zero.
    let mut b = tv(&meta(0));
    let la = oca_reduce(stop(1, IntentSide::Long, "105"));
    let sb = oca_reduce(stop(2, IntentSide::Short, "95"));
    run(&mut b, &bar(0, "100", "100", "100", "100"), &[], &[la, sb]);
    run(
        &mut b,
        &bar(1, "100", "106", "99", "104"),
        &[],
        &[close_all()],
    );
    assert_eq!(b.view().position_size_1e6, E);
    run(&mut b, &bar(2, "104", "104", "100", "100"), &[], &[la, sb]);
    assert_eq!(b.view().position_size_1e6, 0);
    run(&mut b, &bar(3, "100", "101", "94", "96"), &[], &[]);
    assert_eq!(b.view().position_size_1e6, -E);
}

// --- `pin:05-trade-excursion` (shared with native) -------------------------------------------

#[test]
fn the_entry_fill_takes_no_excursion_sample_e061_trade_21() {
    // E061 trade 21 (cash per order 1): LA's stop 64636.6 fills on 1784559600000's L→H segment
    // and the price never comes back — drawdown 0, not the 1 USD fee; run-up at the high 65688.0
    // net of the fee: 1050.4.
    let mut m = meta(0);
    m.decl.commission_type = CommissionType::CashPerOrder;
    m.decl.commission_value_1e6 = E;
    let mut b = tv(&m);
    run(
        &mut b,
        &bar(0, "64315.0", "64550.0", "64034.3", "64393.0"),
        &[],
        &[stop(1, IntentSide::Long, "64636.6")],
    );
    run(
        &mut b,
        &bar(1, "64393.0", "65688.0", "64386.8", "65570.1"),
        &[],
        &[],
    );
    run(
        &mut b,
        &bar(2, "65570.1", "65615.5", "65301.8", "65478.6"),
        &[],
        &[close_all()],
    );
    run(
        &mut b,
        &bar(3, "65478.6", "65642.0", "65313.9", "65560.0"),
        &[],
        &[],
    );
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!(c.entry_px_1e6, d("64636.6"));
    assert_eq!((c.max_runup_1e6, c.max_drawdown_1e6), (d("1050.4"), 0));
}

// --- `pin:05-openprofit-fees` (shared with native) -------------------------------------------

#[test]
fn openprofit_is_gross_and_netprofit_carries_the_entry_fee_e114() {
    // E114 row 1776481200000: a long at 77249.6, 0.1 % commission, the bar closes 76964.3 —
    // openprofit −285.3, equity 999 637.4504 (TV's printed f64 noise aside).
    let mut m = meta(0);
    m.decl.commission_type = CommissionType::Percent;
    m.decl.commission_value_1e6 = 100_000;
    let mut b = tv(&m);
    run(
        &mut b,
        &bar(0, "77335.6", "77400.0", "77200.0", "77249.5"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(1, "77249.6", "77254.3", "76903.2", "76964.3"),
        &[],
        &[],
    );
    let v = *b.view();
    assert_eq!(v.openprofit_1e12, -2_853 * E12 / 10);
    assert_eq!(v.netprofit_1e12, -772_496 * E12 / 10_000);
    assert_eq!(v.equity_1e12, 9_996_374_504 * E12 / 10_000);
}

// --- `pin:U10-risk-rules` (shared with native) -----------------------------------------------

#[test]
fn intraday_filled_orders_trip_on_the_fill_that_reaches_the_bound_e093_trade_2() {
    // E093 (`max_intraday_filled_orders(2)`): trade 1's exit at the day's first bar and trade 2's
    // entry at 75577.4 the same day are 2 fills — trade 2 is flattened at its own entry tick.
    let mut m = meta(0);
    m.decl.risk.max_intraday_filled_orders = 2;
    let mut b = tv(&m);
    run(
        &mut b,
        &bar(20, "77249.6", "77300.0", "77200.0", "77250.0"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(21, "77249.6", "77300.0", "77200.0", "77250.0"),
        &[],
        &[close(1)],
    );
    // Day 1 (bar 24 opens at 24 h): the close fills at the open, then the new entry.
    run(
        &mut b,
        &bar(24, "75653.8", "75700.0", "75600.0", "75650.0"),
        &[],
        &[long(1)],
    );
    run(
        &mut b,
        &bar(25, "75577.4", "75600.0", "75500.0", "75550.0"),
        &[],
        &[],
    );
    let c = b.closed_trade(1).copied().unwrap_or_default();
    assert_eq!(
        (c.entry_px_1e6, c.exit_px_1e6),
        (d("75577.4"), d("75577.4"))
    );
    assert_ne!(c.flags & trade_flag::RISK_FLATTEN, 0);
    assert_eq!(b.counters().risk_trips_intraday_orders, 1);
}

#[test]
fn a_losing_day_is_an_equity_fall_e094() {
    // Synthetic, E094's shape (`max_cons_loss_days(2)`): day 0 holds a long open and falling (no
    // close), day 1 falls further — the trip flattens it at day 2's open.
    let mut m = meta(0);
    m.decl.risk.max_cons_loss_days = 2;
    let mut b = tv(&m);
    run(&mut b, &bar(0, "100", "100", "100", "100"), &[], &[long(1)]);
    run(&mut b, &bar(1, "100", "100", "95", "96"), &[], &[]);
    run(&mut b, &bar(24, "96", "96", "90", "91"), &[], &[]);
    assert_eq!(b.view().position_size_1e6, E);
    run(&mut b, &bar(48, "92", "93", "91", "92"), &[], &[]);
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!(c.exit_px_1e6, d("92"));
    assert_ne!(c.flags & trade_flag::RISK_FLATTEN, 0);
    assert_eq!(b.counters().risk_trips_cons_loss_days, 1);
}

// --- `pin:05-exit-lapse-at-fill` -------------------------------------------------------------

#[test]
fn an_id_less_exit_lapses_at_the_fill_that_flattens_e121_trade_8() {
    // Synthetic, E121 trade 8's shape: `strategy.exit("TP", limit = 101)` (no `from_entry`) takes
    // trade 1 at 101 on L→H; a buy stop placed flat opens trade 2 at 102 further up — the old
    // level does not reach it on H→C (it would have filled at the high, 103).
    let mut b = tv(&meta(0));
    let tp = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        flags: f::LIMIT,
        limit_1e6: d("101"),
        seq: 40,
        ..OrderIntent::default()
    };
    run(
        &mut b,
        &bar(0, "100", "100", "100", "100"),
        &[],
        &[long(1), stop(2, IntentSide::Long, "102")],
    );
    // Bar 1 stays under 102 (the stop, born flat, survives at generation: pyramiding counts at
    // birth only).
    run(&mut b, &bar(1, "100", "100.5", "99.5", "100"), &[], &[tp]);
    run(&mut b, &bar(2, "100", "103", "99.8", "102.5"), &[], &[]);
    assert_eq!(exit_px(&b, 0), Some(d("101")));
    assert_eq!(b.view().position_size_1e6, E);
    assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(d("102")));
}
