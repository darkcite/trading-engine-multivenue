// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r27b unit tests on hand bars: the v5 order cap (CI-005 / E110, `pin:05-v5-order-limit`) and
//! the history-tick walk behind `history_tick = PathPoints` (R-P6, §4.8.9, `pin:05-hist-tick-*`;
//! the default since R-P18 — the `Refuse` reading is reached only through `with_switches`, I38).

#[cfg(test)]
mod tests {
    use crate::fault::{code, BrokerFault, ScriptRefusal};
    use crate::switch::{HistoryTick, Switches, SWITCHES};
    use crate::tv::{TvBroker, TvStep};
    use pine_ir::{
        barflag, decl_flag, intent_flag as f, Bar, ChartInfo, Dialect, IntentKind, OrderIntent,
        ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
    };

    const E: i64 = 1_000_000;

    const CFG: TvCfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };

    const PATH_POINTS: Switches = Switches {
        history_tick: HistoryTick::PathPoints,
        ..SWITCHES
    };

    fn meta(dialect: Dialect) -> ProgramMeta {
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
                initial_capital_1e6: 1_000_000 * E,
                default_qty_value_1e6: E,
                pyramiding: 1,
                ..StrategyDecl::default()
            },
            dialect,
            ..ProgramMeta::default()
        }
    }

    fn hist_meta() -> ProgramMeta {
        let mut m = meta(Dialect::V6);
        m.decl.flags |= decl_flag::CALC_ON_EVERY_HISTORY_TICK;
        m.uses_intrabar = true;
        m
    }

    /// A confirmed history bar.
    fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            open_1e6: o,
            high_1e6: h,
            low_1e6: l,
            close_1e6: c,
            volume_1e6: 7 * E,
            open_ms: i64::from(i) * 60_000,
            close_ms: i64::from(i + 1) * 60_000,
            index: i,
            flags: barflag::CONFIRMED,
        }
    }

    fn entry() -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(1),
            ..OrderIntent::default()
        }
    }

    fn close() -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(1),
            ..OrderIntent::default()
        }
    }

    /// Drive one bar; `f(k, bar, intrabar)` answers the `k`-th evaluation. Returns every evaluation's
    /// `(bar, intrabar)` and the first `submit` error.
    fn drive(
        b: &mut TvBroker,
        x: &Bar,
        mut f: impl FnMut(usize, &Bar, bool) -> Vec<OrderIntent>,
    ) -> (Vec<(Bar, bool)>, Option<BrokerFault>) {
        let mut seen = Vec::new();
        let mut step = b.open_bar(x);
        while let TvStep::Evaluate { bar, intrabar } = step {
            let it = f(seen.len(), &bar, intrabar);
            seen.push((bar, intrabar));
            step = match b.submit(&it) {
                Ok(s) => s,
                Err(e) => return (seen, Some(e)),
            };
        }
        (seen, None)
    }

    /// E110's script: `strategy.entry` on even steps, `strategy.close` on odd ones (one order a step).
    fn e110_step(b: &mut TvBroker, i: u32) -> Option<BrokerFault> {
        let x = bar(i, 100 * E, 101 * E, 99 * E, 100 * E);
        let it = if i % 2 == 0 { entry() } else { close() };
        drive(b, &x, |_, _, _| vec![it]).1
    }

    #[test]
    fn v5_order_limit_faults_the_run_creating_order_9001() {
        // CI-005 / E110 (`pin:05-v5-order-limit`): order n is created at step n − 1, so the 9001st
        // at step 9000 — that run faults ScriptRefused (`OrderTableFull`, no new kind), latched.
        let mut b = TvBroker::new(&meta(Dialect::V5), CFG).unwrap_or_else(|_| unreachable!());
        let mut i = 0;
        while i < 9000 {
            assert_eq!(e110_step(&mut b, i), None, "step {i}");
            i += 1;
        }
        // Order 9000 (a close) filled at bar 9000's open: 4500 round trips.
        let fault = BrokerFault::Script(ScriptRefusal::OrderTableFull);
        assert_eq!(e110_step(&mut b, 9000), Some(fault));
        assert_eq!(b.view().closedtrades, 4500);
        assert_eq!(b.submit(&[]), Err(fault));
    }

    #[test]
    fn v6_runs_past_9000_orders() {
        // CI-005: v6 unchanged — no cap (E109 ran ~9000 trades without a fault).
        let mut b = TvBroker::new(&meta(Dialect::V6), CFG).unwrap_or_else(|_| unreachable!());
        let mut i = 0;
        while i < 9010 {
            assert_eq!(e110_step(&mut b, i), None, "step {i}");
            i += 1;
        }
        assert_eq!(b.view().closedtrades, 4504);
    }

    #[test]
    fn v5_resends_and_unfilled_orders_do_not_count() {
        // Lead answer 3: a re-send of the same live order is a modification; a close with nothing
        // open dies unfilled at its fill attempt (zero size) — neither counts.
        let mut b = TvBroker::new(&meta(Dialect::V5), CFG).unwrap_or_else(|_| unreachable!());
        let rest = OrderIntent {
            flags: f::LIMIT,
            limit_1e6: 50 * E,
            ..entry()
        };
        let lapse = OrderIntent {
            from_entry: StrKey(2),
            seq: 1,
            ..close()
        };
        let mut i = 0;
        while i < 9100 {
            let x = bar(i, 100 * E, 101 * E, 99 * E, 100 * E);
            assert_eq!(
                drive(&mut b, &x, |_, _, _| vec![rest, lapse]).1,
                None,
                "step {i}"
            );
            i += 1;
        }
        assert_eq!(b.view().opentrades, 0);
        assert_eq!(b.orders.created(), 9101); // 1 entry + 9100 closes created …
        assert_eq!(b.v5_dropped, 9099); // … 9099 died unfilled (the last one is still pending)
    }

    #[test]
    fn exit_legs_are_orders_once_per_set() {
        // Lead answer 3: exit legs count — a take-profit + stop-loss set is two orders, counted when
        // the set is built; re-committing the same exit every bar is a modification.
        let mut b = TvBroker::new(&meta(Dialect::V5), CFG).unwrap_or_else(|_| unreachable!());
        let exit = OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(9),
            flags: f::LIMIT | f::STOP,
            limit_1e6: 120 * E,
            stop_1e6: 80 * E,
            seq: 1,
            ..OrderIntent::default()
        };
        let mut i = 0;
        while i < 4 {
            let x = bar(i, 100 * E, 101 * E, 99 * E, 100 * E);
            let it = if i == 0 {
                vec![entry(), exit]
            } else {
                vec![exit]
            };
            assert_eq!(drive(&mut b, &x, |_, _, _| it.clone()).1, None);
            i += 1;
        }
        assert_eq!(b.view().opentrades, 1);
        assert_eq!(b.exits.legs_created(), 2);
    }

    #[test]
    fn history_tick_admitted_by_default_and_refused_under_refuse() {
        // R-P18 (2026-10-17): `PathPoints` is the default; `Refuse` (R-P6 / lead answer 6,
        // `pin:05-hist-tick-refuse`, the default until R-P18) still refuses 5109 when selected.
        let m = hist_meta();
        assert!(TvBroker::new(&m, CFG).is_ok());
        assert_eq!(SWITCHES.history_tick, PATH_POINTS.history_tick);
        let refuse = Switches {
            history_tick: HistoryTick::Refuse,
            ..SWITCHES
        };
        assert_eq!(
            TvBroker::with_switches(&m, CFG, refuse).err(),
            Some(code::HISTORY_TICK)
        );
        let mut bad = m;
        bad.uses_intrabar = false; // lead answer 13: a mis-paired program refuses 5107
        assert_eq!(
            TvBroker::new(&bad, CFG).err(),
            Some(code::INTRABAR_ON_BAR_CLOSE)
        );
    }

    #[test]
    fn history_tick_three_developing_runs_then_the_confirmed_close() {
        // Lead answers 7, 10 (`pin:05-hist-tick-runs`, `pin:05-hist-tick-bar`): path 100→99→104→103
        // (low first). Runs at the open and the two extremes on the developing bar — full volume,
        // CONFIRMED dropped, times unchanged — then the bar-close run on the full bar. CONFIRMED and
        // REALTIME both clear is how the VM detects a history tick; it reads `isconfirmed` true on
        // it itself (CAR-R32-1, TV E122) — nothing here reads the flag.
        let mut b = TvBroker::with_switches(&hist_meta(), CFG, PATH_POINTS)
            .unwrap_or_else(|_| unreachable!());
        let x = bar(0, 100 * E, 104 * E, 99 * E, 103 * E);
        let (seen, err) = drive(&mut b, &x, |_, _, _| Vec::new());
        assert_eq!(err, None);
        let ohlc = |y: &Bar| {
            (
                y.open_1e6 / E,
                y.high_1e6 / E,
                y.low_1e6 / E,
                y.close_1e6 / E,
            )
        };
        assert_eq!(seen.len(), 4);
        assert_eq!(ohlc(&seen[0].0), (100, 100, 100, 100));
        assert_eq!(ohlc(&seen[1].0), (100, 100, 99, 99));
        assert_eq!(ohlc(&seen[2].0), (100, 104, 99, 104));
        let mut k = 0;
        while k < 3 {
            let (y, intrabar) = seen[k];
            assert!(intrabar);
            assert_eq!(y.flags & barflag::CONFIRMED, 0);
            assert_eq!(
                (y.volume_1e6, y.open_ms, y.close_ms, y.index),
                (7 * E, 0, 60_000, 0)
            );
            k += 1;
        }
        assert_eq!(seen[3], (x, false));
        // A realtime bar takes no history-tick run (§4.8.9).
        let rt = Bar {
            index: 1,
            flags: barflag::REALTIME,
            ..x
        };
        let (seen, _) = drive(&mut b, &rt, |_, _, _| Vec::new());
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn history_tick_orders_fill_at_their_own_point() {
        // `pin:05-hist-tick-same-point` (R-P18 lead ruling, TV E124–E127 45/45; retires lead
        // answer 9's next point): a market entry created by the run at point k fills at point k,
        // right after that run — 100 (k = 0), 99 (k = 1), 104 (k = 2); the later runs see it.
        let x0 = bar(0, 100 * E, 104 * E, 99 * E, 103 * E);
        let mut k = 0;
        while k < 3 {
            let mut b = TvBroker::with_switches(&hist_meta(), CFG, PATH_POINTS)
                .unwrap_or_else(|_| unreachable!());
            let (_, err) = drive(
                &mut b,
                &x0,
                |n, _, _| if n == k { vec![entry()] } else { Vec::new() },
            );
            assert_eq!(err, None);
            let px = b.open_trade(0).map(|t| t.entry_px_1e6 / E);
            assert_eq!(px, Some([100, 99, 104][k]), "point {k}");
            assert_eq!(b.view().position_size_1e6, E, "point {k}");
            k += 1;
        }
        // The next run sees the fill: an entry at the run at point 0, the run at point 1 reads
        // position 1 (TV E127's `p1` reading, here one point later).
        let mut b = TvBroker::with_switches(&hist_meta(), CFG, PATH_POINTS)
            .unwrap_or_else(|_| unreachable!());
        let mut seen_pos = [0_i64; 4];
        let mut step = b.open_bar(&x0);
        let mut n = 0;
        while let TvStep::Evaluate { .. } = step {
            seen_pos[n] = b.view().position_size_1e6;
            let it = if n == 0 { vec![entry()] } else { Vec::new() };
            step = b.submit(&it).unwrap_or_else(|e| panic!("{e:?}"));
            n += 1;
        }
        assert_eq!((n, seen_pos), (4, [0, E, E, E]));
    }

    #[test]
    fn history_tick_fill_at_its_run_point_takes_no_cof_execution_e126() {
        // `pin:05-hist-tick-cof` (R-P18 lead ruling) — TV E126: with `calc_on_order_fills`, a
        // market entry created by the open's history-tick run fills at the open right after it
        // and requests no execution: 4 runs on the fill bar (all 4 320 bars run 4 times).
        let mut m = hist_meta();
        m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
        let mut b =
            TvBroker::with_switches(&m, CFG, PATH_POINTS).unwrap_or_else(|_| unreachable!());
        let x0 = bar(0, 100 * E, 104 * E, 99 * E, 103 * E);
        let (seen, err) = drive(
            &mut b,
            &x0,
            |n, _, _| if n == 0 { vec![entry()] } else { Vec::new() },
        );
        assert_eq!(err, None);
        assert_eq!(seen.len(), 4);
        assert_eq!(b.counters().reevals_on_order_fill, 0);
        assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(100 * E));
    }

    #[test]
    fn history_tick_fills_at_the_point_before_the_run() {
        // Lead answer 8 (`pin:05-hist-tick-fill-first`): the market entry from bar 0's close fills
        // at bar 1's open before the run at that point sees the position.
        let mut b = TvBroker::with_switches(&hist_meta(), CFG, PATH_POINTS)
            .unwrap_or_else(|_| unreachable!());
        let x0 = bar(0, 100 * E, 101 * E, 99 * E, 100 * E);
        let (_, err) = drive(
            &mut b,
            &x0,
            |n, _, _| if n == 3 { vec![entry()] } else { Vec::new() },
        );
        assert_eq!(err, None);
        let x1 = bar(1, 102 * E, 104 * E, 101 * E, 103 * E);
        let mut step = b.open_bar(&x1);
        assert!(matches!(step, TvStep::Evaluate { intrabar: true, .. }));
        assert_eq!(b.view().position_size_1e6, E);
        assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(102 * E));
        while let TvStep::Evaluate { .. } = step {
            step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
        }
    }

    #[test]
    fn history_tick_combines_with_calc_on_order_fills() {
        // Lead answer 11 (`pin:05-hist-tick-cof`): a buy limit at 99 fills at segment 0's end; the
        // fill execution (full bar) comes before the history-tick run at point 1 (developing bar).
        let mut m = hist_meta();
        m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
        let mut b =
            TvBroker::with_switches(&m, CFG, PATH_POINTS).unwrap_or_else(|_| unreachable!());
        let lim = OrderIntent {
            flags: f::LIMIT,
            limit_1e6: 99 * E,
            ..entry()
        };
        let x0 = bar(0, 100 * E, 101 * E, 99_500_000, 100 * E);
        let (_, err) = drive(
            &mut b,
            &x0,
            |n, _, _| if n == 3 { vec![lim] } else { Vec::new() },
        );
        assert_eq!(err, None);
        let x1 = bar(1, 100 * E, 104 * E, 99 * E, 103 * E);
        let (seen, err) = drive(&mut b, &x1, |_, _, _| Vec::new());
        assert_eq!(err, None);
        let kinds: Vec<(i64, bool, bool)> = seen
            .iter()
            .map(|(y, ib)| (y.close_1e6 / E, *ib, y.flags & barflag::CONFIRMED != 0))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (100, true, false), // history tick at the open
                (103, true, true),  // the fill execution at 99: the full bar (§4.3.2)
                (99, true, false),  // history tick at the low
                (104, true, false), // history tick at the high
                (103, false, true), // the bar-close run
            ]
        );
        assert_eq!(b.counters().reevals_on_order_fill, 1);
        assert_eq!(b.open_trade(0).map(|t| t.entry_px_1e6), Some(99 * E));
    }
}
