// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.7 — the TV walk through the public surface (hand-built intents and bars).

use pine_broker::{BrokerFault, InternalDefect, TvBroker, TvStep};
use pine_ir::{
    decl_flag, intent_flag as f, Bar, ChartInfo, IntentKind, IntentSide, OrderIntent, ProgramMeta,
    SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;

fn meta(flags: u16) -> ProgramMeta {
    ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 0,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 100_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 1,
            flags,
            ..StrategyDecl::default()
        },
        uses_intrabar: flags & decl_flag::CALC_ON_ORDER_FILLS != 0,
        ..ProgramMeta::default()
    }
}

fn broker(flags: u16) -> TvBroker {
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    TvBroker::new(&meta(flags), cfg).unwrap_or_else(|_| unreachable!())
}

fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        open_1e6: o * E,
        high_1e6: h * E,
        low_1e6: l * E,
        close_1e6: c * E,
        open_ms: i64::from(i) * 60_000,
        close_ms: i64::from(i) * 60_000 + 60_000,
        index: i,
        ..Bar::default()
    }
}

fn entry(id: u64, side: IntentSide) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(id),
        side,
        ..OrderIntent::default()
    }
}

/// Drive one bar: open it, answer every `Evaluate` with `intents` on the bar close, nothing else.
fn run_bar(b: &mut TvBroker, x: &Bar, intents: &[OrderIntent]) {
    let mut step = b.open_bar(x);
    loop {
        match step {
            TvStep::Evaluate {
                intrabar: false, ..
            } => {
                step = b.submit(intents).unwrap_or_else(|e| panic!("{e:?}"));
                // Later evaluations of the same bar submit nothing.
                while let TvStep::Evaluate { .. } = step {
                    step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
                }
            }
            TvStep::Evaluate { intrabar: true, .. } => {
                step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
            }
            TvStep::Done => return,
        }
    }
}

#[test]
fn market_order_fills_at_the_next_bar_open() {
    let mut b = broker(0);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    assert_eq!(b.view().position_size_1e6, 0);
    run_bar(&mut b, &bar(1, 103, 104, 102, 103), &[]);
    let t = b.open_trade(0).copied().unwrap_or_default();
    assert_eq!((t.entry_px_1e6, t.qty_1e6, t.entry_bar), (103 * E, E, 1));
    assert_eq!(b.view().position_size_1e6, E);
}

#[test]
fn limit_entry_fills_at_its_limit_on_the_path() {
    let mut b = broker(0);
    let lim = OrderIntent {
        flags: f::LIMIT,
        limit_1e6: 98 * E,
        seq: 0,
        ..entry(1, IntentSide::Long)
    };
    run_bar(&mut b, &bar(0, 100, 101, 99, 100), &[lim]);
    // Next bar dips to 97: the buy limit fills at 98 (not worse than its limit).
    run_bar(&mut b, &bar(1, 100, 101, 97, 99), &[]);
    assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(98 * E));
}

#[test]
fn exit_take_profit_and_stop_loss_on_the_path() {
    let mut b = broker(0);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::PROFIT | f::LOSS,
        profit_ticks_1e6: 200 * E, // 2.00
        loss_ticks_1e6: 150 * E,   // 1.50
        seq: 1,
        ..OrderIntent::default()
    };
    let en = OrderIntent {
        seq: 0,
        ..entry(1, IntentSide::Long)
    };
    run_bar(&mut b, &bar(0, 100, 101, 99, 100), &[en, ex]);
    // Bar 1: opens 100 (entry fills); the low is nearer, so the path is 100 → 99 → 102 → 101.
    run_bar(&mut b, &bar(1, 100, 102, 99, 101), &[]);
    // The stop at 98.50 is not touched; the take-profit at 102 is reached on the way up.
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!((c.exit_px_1e6, c.exit_id), (102 * E, StrKey(9)));
    assert_eq!(b.view().position_size_1e6, 0);
    assert_eq!(b.view().netprofit_1e12, 2 * i128::from(E) * i128::from(E));
}

#[test]
fn reversal_entry_fills_qty_plus_position() {
    let mut b = broker(0);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    run_bar(
        &mut b,
        &bar(1, 100, 101, 99, 100),
        &[OrderIntent {
            seq: 0,
            ..entry(2, IntentSide::Short)
        }],
    );
    run_bar(&mut b, &bar(2, 100, 101, 99, 100), &[]);
    assert_eq!(b.view().position_size_1e6, -E);
    assert_eq!(b.view().closedtrades, 1);
}

#[test]
fn calc_on_order_fills_evaluates_once_per_fill_tick() {
    let mut b = broker(decl_flag::CALC_ON_ORDER_FILLS);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    let mut step = b.open_bar(&bar(1, 100, 102, 99, 101));
    let mut intrabar = 0;
    loop {
        match step {
            TvStep::Evaluate { intrabar: true, .. } => {
                intrabar += 1;
                step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
            }
            TvStep::Evaluate {
                intrabar: false, ..
            } => {
                step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
            }
            TvStep::Done => break,
        }
    }
    assert_eq!((intrabar, b.counters().reevals_on_order_fill), (1, 1));
}

#[test]
fn process_orders_on_close_fills_at_the_close() {
    let mut b = broker(decl_flag::ORDERS_ON_CLOSE);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    assert_eq!(
        b.open_trade(0).map(|t| (t.entry_px_1e6, t.entry_bar)),
        Some((100 * E, 0))
    );
}

#[test]
fn submit_out_of_order_is_a_path_cursor_defect() {
    let mut b = broker(0);
    let r = b.submit(&[]);
    assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::PathCursor)));
}

#[test]
fn a_latched_fault_is_returned_by_every_later_submit() {
    let mut b = broker(0);
    // open_bar while a bar is still awaiting its submit latches PathCursor.
    let _ = b.open_bar(&bar(0, 100, 101, 99, 100));
    let step = b.open_bar(&bar(1, 100, 101, 99, 100));
    assert!(matches!(
        step,
        TvStep::Evaluate {
            intrabar: false,
            ..
        }
    ));
    let latched = Err(BrokerFault::Internal(InternalDefect::PathCursor));
    assert_eq!(b.submit(&[]), latched);
    assert_eq!(b.submit(&[]), latched);
}

#[test]
fn every_fill_is_inside_the_bar_without_slippage() {
    let mut b = broker(0);
    let stop = OrderIntent {
        flags: f::STOP,
        stop_1e6: 101 * E,
        seq: 0,
        ..entry(1, IntentSide::Long)
    };
    run_bar(&mut b, &bar(0, 100, 100, 99, 100), &[stop]);
    // Gaps up through the stop: fills at the open (106), inside the bar.
    run_bar(&mut b, &bar(1, 106, 107, 105, 106), &[]);
    let t = b.open_trade(0).copied().unwrap_or_default();
    assert_eq!(t.entry_px_1e6, 106 * E);
    assert!(t.entry_px_1e6 >= 105 * E && t.entry_px_1e6 <= 107 * E);
}

#[test]
fn a_stop_loss_at_the_bar_low_fills_there() {
    // Same entry as above with the stop at exactly the low (99): it fills at 99 on the segment
    // that starts there, before the take-profit.
    let mut b = broker(0);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::PROFIT | f::LOSS,
        profit_ticks_1e6: 200 * E,
        loss_ticks_1e6: 100 * E,
        seq: 1,
        ..OrderIntent::default()
    };
    let en = OrderIntent {
        seq: 0,
        ..entry(1, IntentSide::Long)
    };
    run_bar(&mut b, &bar(0, 100, 101, 99, 100), &[en, ex]);
    run_bar(&mut b, &bar(1, 100, 102, 99, 101), &[]);
    assert_eq!(b.closed_trade(0).map(|t| t.exit_px_1e6), Some(99 * E));
}

fn close_now(from: u64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(from),
        flags: f::IMMEDIATELY,
        ..OrderIntent::default()
    }
}

#[test]
fn immediately_close_fills_at_the_close_of_its_bar() {
    let mut b = broker(0);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    run_bar(&mut b, &bar(1, 100, 101, 99, 100), &[]);
    // The bar-close evaluation of bar 2 closes with `immediately`: the on-close pass fills at c.
    run_bar(&mut b, &bar(2, 100, 104, 99, 103), &[close_now(1)]);
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!((c.exit_px_1e6, c.exit_bar), (103 * E, 2));
}

#[test]
fn immediately_close_at_an_intrabar_execution_fills_at_that_tick() {
    let mut b = broker(decl_flag::CALC_ON_ORDER_FILLS);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    let mut step = b.open_bar(&bar(1, 100, 102, 99, 101));
    // The entry fills at the open (100): one intrabar execution, which closes immediately.
    assert!(matches!(step, TvStep::Evaluate { intrabar: true, .. }));
    step = b
        .submit(&[close_now(1)])
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(b.closed_trade(0).map(|t| t.exit_px_1e6), Some(100 * E));
    while let TvStep::Evaluate { .. } = step {
        step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
    }
    assert_eq!(b.view().position_size_1e6, 0);
}

#[test]
fn an_order_born_at_a_point_execution_fills_at_that_point() {
    let mut b = broker(decl_flag::CALC_ON_ORDER_FILLS);
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    let mut step = b.open_bar(&bar(1, 100, 102, 99, 101));
    assert!(matches!(step, TvStep::Evaluate { intrabar: true, .. }));
    // `pin:05-cof-next-tick` (r33d, `SamePoint`; TV E016 / E031 / E105): the execution is at a
    // path point (the open's fill), so a market close born there fills at that point (100), not
    // at the next one (the low, 99 — the reading before r33d).
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(1),
        ..OrderIntent::default()
    };
    step = b.submit(&[close]).unwrap_or_else(|e| panic!("{e:?}"));
    while let TvStep::Evaluate { .. } = step {
        step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
    }
    assert_eq!(b.closed_trade(0).map(|t| t.exit_px_1e6), Some(100 * E));
}

#[test]
fn a_drawdown_trip_on_the_closed_equity_halts_for_good() {
    // `pin:U10-risk-rules` (r33d, TV E091): `max_drawdown` measures the closed equity.
    let mut m = meta(0);
    m.decl.risk.max_drawdown_1e6 = 2 * E;
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let mut b = TvBroker::new(&m, cfg).unwrap_or_else(|_| unreachable!());
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    // Path 100 → 101 → 97 → 98: an open drawdown of 4 from the 101 mark trips nothing.
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(1),
        ..OrderIntent::default()
    };
    run_bar(&mut b, &bar(1, 100, 101, 97, 98), &[close]);
    assert_eq!(b.counters().risk_trips_drawdown, 0);
    assert_eq!(b.view().position_size_1e6, E);
    // The close fills at bar 2's open, 97: the closed equity is 3 below its peak — the trip.
    run_bar(&mut b, &bar(2, 97, 98, 96, 97), &[]);
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_eq!(c.exit_px_1e6, 97 * E);
    assert_eq!(c.flags & pine_ir::trade_flag::RISK_FLATTEN, 0);
    assert_eq!(b.counters().risk_trips_drawdown, 1);
    // Halted for good: the re-emitted entry is not committed.
    run_bar(
        &mut b,
        &bar(3, 100, 101, 99, 100),
        &[entry(1, IntentSide::Long)],
    );
    run_bar(&mut b, &bar(4, 100, 101, 99, 100), &[]);
    assert_eq!(b.view().position_size_1e6, 0);
}

#[test]
fn a_v6_margin_rejects_an_entry_the_funds_cannot_cover() {
    // 100 000 USD of capital, margin 100 %: 2 000 contracts at 100 need 200 000.
    let mut m = meta(0);
    m.decl.margin_long_pct_1e6 = 100 * E;
    m.decl.margin_short_pct_1e6 = 100 * E;
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let mut b = TvBroker::new(&m, cfg).unwrap_or_else(|_| unreachable!());
    let big = OrderIntent {
        flags: f::QTY,
        qty_1e6: 2_000 * E,
        ..entry(1, IntentSide::Long)
    };
    run_bar(&mut b, &bar(0, 100, 101, 99, 100), &[big]);
    run_bar(&mut b, &bar(1, 100, 101, 99, 100), &[]);
    assert_eq!(b.view().position_size_1e6, 0);
    assert_eq!(b.counters().sized_to_zero, 1);
}

#[test]
fn a_short_is_margin_called_four_times_the_cover() {
    // Short 1 000 at 100 with 100 000 capital and margin 100 %: the price runs to 150.
    let mut m = meta(0);
    m.decl.margin_short_pct_1e6 = 100 * E;
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let mut b = TvBroker::new(&m, cfg).unwrap_or_else(|_| unreachable!());
    let short = OrderIntent {
        flags: f::QTY,
        qty_1e6: 1_000 * E,
        ..entry(1, IntentSide::Short)
    };
    run_bar(&mut b, &bar(0, 100, 101, 99, 100), &[short]);
    run_bar(&mut b, &bar(1, 100, 100, 100, 100), &[]);
    assert_eq!(b.view().position_size_1e6, -1_000 * E);
    run_bar(&mut b, &bar(2, 150, 150, 150, 150), &[]);
    assert!(b.counters().margin_calls >= 1);
    let c = b.closed_trade(0).copied().unwrap_or_default();
    assert_ne!(c.flags & pine_ir::trade_flag::LIQUIDATION, 0);
    assert!(b.view().position_size_1e6 > -1_000 * E);
}

fn w23_exit_price(dialect: pine_ir::Dialect) -> i64 {
    let mut m = meta(0);
    m.dialect = dialect;
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let mut b = TvBroker::new(&m, cfg).unwrap_or_else(|_| unreachable!());
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT | f::PROFIT,
        limit_1e6: 103 * E,
        profit_ticks_1e6: 200 * E, // 2.00 above the entry: 102
        seq: 1,
        ..OrderIntent::default()
    };
    run_bar(
        &mut b,
        &bar(0, 100, 101, 99, 100),
        &[
            OrderIntent {
                seq: 0,
                ..entry(1, IntentSide::Long)
            },
            ex,
        ],
    );
    run_bar(&mut b, &bar(1, 100, 104, 99, 101), &[]);
    b.closed_trade(0).map(|t| t.exit_px_1e6).unwrap_or(0)
}

#[test]
fn w23_v5_takes_the_absolute_level_and_v6_the_one_reached_first() {
    assert_eq!(w23_exit_price(pine_ir::Dialect::V5), 103 * E);
    assert_eq!(w23_exit_price(pine_ir::Dialect::V6), 102 * E);
}

/// One evaluation of a five-level grid (pvm7_grid's shape): optional `cancel_all`, five
/// `strategy.order` limit buys `Bi` at `100 − i`, each its own OCA-cancel group, and five
/// `strategy.exit("Ti", "Bi", limit = level + 6)`.
fn grid_slice(cancel_all: bool) -> ([OrderIntent; 11], usize) {
    let mut s = [OrderIntent::default(); 11];
    let mut n = 0;
    if cancel_all {
        s[0] = OrderIntent {
            kind: IntentKind::CancelAll,
            ..OrderIntent::default()
        };
        n = 1;
    }
    let mut i = 1;
    while i <= 5 {
        let level = (100 - i) * E;
        s[n] = OrderIntent {
            kind: IntentKind::Order,
            id: StrKey(i as u64),
            side: IntentSide::Long,
            flags: f::LIMIT,
            limit_1e6: level,
            oca_name: StrKey(100 + i as u64),
            oca_type: pine_ir::OcaType::Cancel,
            ..OrderIntent::default()
        };
        s[n + 5] = OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(10 + i as u64),
            from_entry: StrKey(i as u64),
            flags: f::LIMIT,
            limit_1e6: level + 6 * E,
            ..OrderIntent::default()
        };
        n += 1;
        i += 1;
    }
    n += 5;
    let mut k = 0;
    while k < n {
        s[k].seq = u32::try_from(k).unwrap_or(0);
        k += 1;
    }
    (s, n)
}

/// pvm7_grid (pine-screen, `Broker(OrderTableFull)` at bar 201): a grid re-armed every evaluation,
/// its trades piling up over five falling bars and released by a sixth that reaches every take-
/// profit, 300 bars (past `cap::ORDERS` 64, `cap::EXIT_RULES` 32 and `cap::LEG_SETS` 128). Each
/// `Ti` covers only `Bi`'s trades, so the leg sets never exceed the open trades (≤ 25); the v1
/// capacity bound (every rule × every trade, the slice's cancelled rules counted as live) refused
/// the slice at 10 open trades with the `cancel_all` and at 25 without (`pin:05-leg-bound-coverage`).
#[test]
fn a_re_armed_grid_with_exits_and_cancel_all_runs_past_every_table_capacity() {
    for cancel_all in [true, false] {
        let mut b = broker(0);
        let (slice, n) = grid_slice(cancel_all);
        let mut peak = 0;
        let mut k = 0_u32;
        while k < 300 {
            let x = if k % 6 < 5 {
                bar(k, 100, 100, 94, 94) // every level fills on the way down
            } else {
                bar(k, 100, 110, 100, 108) // every take-profit (101..105) is reached
            };
            run_bar(&mut b, &x, &slice[..n]);
            peak = peak.max(b.view().opentrades);
            if k % 6 == 5 {
                assert_eq!(b.view().opentrades, 0, "bar {k}: the release closes all");
            }
            k += 1;
        }
        assert_eq!(peak, 25, "cancel_all = {cancel_all}");
        // Bar 0's orders commit at its close, so the first cycle fills on four bars (20 trades).
        assert_eq!(
            b.view().closedtrades,
            20 + 49 * 25,
            "cancel_all = {cancel_all}"
        );
    }
}

/// Amendment A9 through the TV walk: long 1 filled @ 100 on bar 1 whose path reaches 90 and 110
/// ($10 on a $100 base: 10 % drawdown and run-up), closed `immediately` at bar 2's close 104
/// (+$4: +4 %). `pin:04-max-dd-pct-provisional`, `pin:04-avg-trade-pct-provisional`.
#[test]
fn a9_percent_aggregates_ride_the_walk() {
    let mut b = broker(0);
    run_bar(
        &mut b,
        &bar(0, 100, 100, 100, 100),
        &[entry(1, IntentSide::Long)],
    );
    run_bar(&mut b, &bar(1, 100, 110, 90, 105), &[]);
    let v = *b.view();
    assert_eq!(v.opentrades, 1);
    assert_eq!(
        (v.max_drawdown_pct_1e6, v.max_runup_pct_1e6),
        (10 * E, 10 * E)
    );
    assert_eq!(v.sum_trade_pct_1e6, 0, "nothing closed yet");
    run_bar(&mut b, &bar(2, 104, 104, 104, 104), &[close_now(1)]);
    let v = *b.view();
    assert_eq!((v.closedtrades, v.wintrades), (1, 1));
    assert_eq!(
        (v.sum_trade_pct_1e6, v.sum_win_pct_1e6, v.sum_loss_pct_1e6),
        (4 * E, 4 * E, 0)
    );
    assert_eq!(
        (v.max_drawdown_pct_1e6, v.max_runup_pct_1e6),
        (10 * E, 10 * E)
    );
}
