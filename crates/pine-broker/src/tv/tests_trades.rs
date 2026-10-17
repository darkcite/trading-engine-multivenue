// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Trade-record laws against TradingView's List of Trades, on hand bars (r27b, CI-032 / CI-033):
//! the exit ORDER ids TradingView names (`pin:05-exit-signal-text`) and the per-trade excursion
//! (`pin:05-trade-excursion`). Each test names the E fixture whose captured trades it restates.

#[cfg(test)]
mod tests {
    use crate::signal::close_entry_key;
    use crate::tv::{TvBroker, TvStep};
    use pine_ir::{
        barflag, intent_flag as f, str_key, trade_flag, Bar, ChartInfo, CommissionType, IntentKind,
        OrderIntent, ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
    };

    const E: i64 = 1_000_000;
    const CFG: TvCfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    const L1: StrKey = StrKey(1);

    fn meta(qty_1e6: i64) -> ProgramMeta {
        ProgramMeta {
            chart: ChartInfo {
                symbol_hash: 1,
                timeframe_s: 3600,
                venue: 0,
                _pad: [0; 3],
                mintick_1e6: 10_000,
                pointvalue_1e6: E,
                qty_step_1e6: 0,
                min_notional_1e6: 0,
            },
            decl: StrategyDecl {
                initial_capital_1e6: 1_000_000 * E,
                default_qty_value_1e6: qty_1e6,
                pyramiding: 1,
                ..StrategyDecl::default()
            },
            ..ProgramMeta::default()
        }
    }

    fn broker(m: &ProgramMeta) -> TvBroker {
        TvBroker::new(m, CFG).unwrap_or_else(|_| unreachable!())
    }

    /// A confirmed hourly history bar (prices in cents).
    fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
        Bar {
            open_1e6: o * 10_000,
            high_1e6: h * 10_000,
            low_1e6: l * 10_000,
            close_1e6: c * 10_000,
            volume_1e6: 7 * E,
            open_ms: i64::from(i) * 3_600_000,
            close_ms: i64::from(i + 1) * 3_600_000,
            index: i,
            flags: barflag::CONFIRMED,
        }
    }

    /// One bar; `intents` answer its first evaluation.
    fn run(b: &mut TvBroker, x: &Bar, intents: &[OrderIntent]) {
        let mut step = b.open_bar(x);
        let mut first = true;
        while let TvStep::Evaluate { .. } = step {
            let s = if first { intents } else { &[] };
            first = false;
            step = match b.submit(s) {
                Ok(s) => s,
                Err(e) => panic!("{e:?}"),
            };
        }
    }

    fn entry() -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: L1,
            ..OrderIntent::default()
        }
    }

    fn exit(flags: u16, limit_1e6: i64, stop_1e6: i64) -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(2),
            from_entry: L1,
            flags,
            limit_1e6,
            stop_1e6,
            ..OrderIntent::default()
        }
    }

    /// Flat bar 0 (the entry is placed), bar 1 fills it at 100.00 with high `h1`, low `l1`.
    fn open_long(b: &mut TvBroker, h1: i64, l1: i64, at1: &[OrderIntent]) {
        run(b, &bar(0, 10_000, 10_000, 10_000, 10_000), &[entry()]);
        run(b, &bar(1, 10_000, h1, l1, 10_000), at1);
    }

    fn excursion(t: Option<&pine_ir::TradeRec>) -> Option<(i64, i64)> {
        t.map(|t| (t.max_runup_1e6, t.max_drawdown_1e6))
    }

    #[test]
    fn excursion_includes_the_exit_fill_and_not_the_path_after_it() {
        // CI-033, `pin:05-trade-excursion` — E011 (stop: drawdown −772.5 = entry − stop, not the
        // bar's low) and E009 (profit: run-up 200 = the target, not the bar's high).
        let m = meta(E);
        let mut b = broker(&m);
        open_long(&mut b, 10_050, 9_980, &[exit(f::STOP, 0, 98 * E)]);
        // Bar 2: open → high 100.20 → low 97.00 (the stop at 98.00 fills on the way) → close.
        run(&mut b, &bar(2, 10_000, 10_020, 9_700, 9_750), &[]);
        let t = b.closed_trade(0);
        assert_eq!(t.map(|t| t.exit_px_1e6), Some(98 * E));
        assert_eq!(
            excursion(t),
            Some((500_000, 2 * E)),
            "not 0.2 (pre-fix) nor 3.0"
        );

        let mut b = broker(&m);
        open_long(&mut b, 10_050, 9_980, &[exit(f::LIMIT, 102 * E, 0)]);
        // Bar 2: open → low 99.90 → high 103.00 (the limit at 102.00 fills) → close.
        run(&mut b, &bar(2, 10_000, 10_300, 9_990, 10_250), &[]);
        let t = b.closed_trade(0);
        assert_eq!(t.map(|t| t.exit_px_1e6), Some(102 * E));
        assert_eq!(
            excursion(t),
            Some((2 * E, 200_000)),
            "run-up 2.0, not 0.5 nor 3.0"
        );
    }

    #[test]
    fn excursion_of_a_partial_slice_is_its_quantity_share() {
        // CI-033 — E111: the 1-contract slice of a 20-contract trade shows run-up 4.7, not 94.
        let m = meta(20 * E);
        let mut b = broker(&m);
        let close_one = OrderIntent {
            kind: IntentKind::Close,
            from_entry: L1,
            flags: f::QTY,
            qty_1e6: E,
            ..OrderIntent::default()
        };
        // Bar 1: run-up 0.50 × 20 = 10, drawdown 0.20 × 20 = 4.
        open_long(&mut b, 10_050, 9_980, &[close_one]);
        run(&mut b, &bar(2, 10_010, 10_010, 10_010, 10_010), &[]);
        assert_eq!(excursion(b.closed_trade(0)), Some((500_000, 200_000)));
        assert_eq!(excursion(b.open_trade(0)), Some((9_500_000, 3_800_000)));
    }

    #[test]
    fn excursion_is_net_of_the_entry_fee() {
        // CI-033 — E063 (commission 0.1 %): drawdown −1930.95 = −1853.7 − the 77.25 entry fee,
        // run-up 0 where the move (4.7) is below the fee.
        let mut m = meta(E);
        m.decl.commission_type = CommissionType::Percent;
        m.decl.commission_value_1e6 = 100_000;
        let mut b = broker(&m);
        // Entry fee 0.10 at 100.00; bar 1: high 100.05, low 99.80.
        open_long(&mut b, 10_005, 9_980, &[]);
        assert_eq!(excursion(b.open_trade(0)), Some((0, 300_000)));
    }

    #[test]
    fn exit_ids_name_the_order_tradingview_generates() {
        // CI-032, `pin:05-exit-signal-text` — E008 / E058 "Close position order"; E001
        // "Close entry(s) order L1" (keyed by the emitter in the `Close` intent's `id`); E092
        // "Close Position (Max intraday Loss)".
        let m = meta(E);
        let exit_id = |at1: OrderIntent, m: &ProgramMeta| {
            let mut b = broker(m);
            open_long(&mut b, 10_050, 9_980, &[at1]);
            run(&mut b, &bar(2, 10_000, 10_010, 9_990, 10_000), &[]);
            b.closed_trade(0).map(|t| (t.exit_id, t.flags))
        };
        let close_all = OrderIntent {
            kind: IntentKind::CloseAll,
            ..OrderIntent::default()
        };
        assert_eq!(
            exit_id(close_all, &m).map(|t| t.0),
            Some(str_key(b"Close position order"))
        );
        let close = OrderIntent {
            kind: IntentKind::Close,
            id: close_entry_key(b"L1"),
            from_entry: L1,
            ..OrderIntent::default()
        };
        assert_eq!(
            exit_id(close, &m).map(|t| t.0),
            Some(str_key(b"Close entry(s) order L1"))
        );
        // An emitter that leaves the `Close` id NONE keeps today's record (the entry id).
        let bare = OrderIntent {
            kind: IntentKind::Close,
            from_entry: L1,
            ..OrderIntent::default()
        };
        assert_eq!(exit_id(bare, &m).map(|t| t.0), Some(L1));

        let mut r = meta(E);
        r.decl.risk.max_intraday_loss_1e6 = E;
        let mut b = broker(&r);
        // Bar 1 falls to 98.00: the day's loss 2.00 ≥ 1.00 trips the rule.
        open_long(&mut b, 10_010, 9_800, &[]);
        let t = b.closed_trade(0).map(|t| (t.exit_id, t.flags));
        assert_eq!(
            t.map(|t| t.0),
            Some(str_key(b"Close Position (Max intraday Loss)"))
        );
        assert_eq!(
            t.map(|t| t.1 & trade_flag::RISK_FLATTEN),
            Some(trade_flag::RISK_FLATTEN)
        );
    }

    #[test]
    fn script_prices_floor_to_the_tick() {
        // CI-034, `pin:05-price-rounding` — E113: a buy limit 0.4 tick off the grid (…48.26)
        // fills at …48.2 (floor), not …48.3 (nearest). Here 98.126 on a 0.01 tick → 98.12.
        let m = meta(E);
        let lim = OrderIntent {
            kind: IntentKind::Entry,
            id: L1,
            flags: f::LIMIT,
            limit_1e6: 98_126_000,
            ..OrderIntent::default()
        };
        let fill = |b: &mut TvBroker| {
            run(b, &bar(0, 10_000, 10_000, 10_000, 10_000), &[lim]);
            run(b, &bar(1, 10_000, 10_000, 9_700, 9_800), &[]);
            b.open_trade(0).map(|t| t.entry_px_1e6)
        };
        assert_eq!(fill(&mut broker(&m)), Some(98_120_000));
        let near = crate::switch::Switches {
            price_rounding: crate::switch::PriceRounding::NearestTick,
            ..crate::switch::SWITCHES
        };
        let mut b = TvBroker::with_switches(&m, CFG, near).unwrap_or_else(|_| unreachable!());
        assert_eq!(fill(&mut b), Some(98_130_000));
    }

    #[test]
    fn market_orders_size_at_the_order_bar_close_limits_at_the_fill() {
        // CI-035, `pin:05-qty-reference-price` — E045: 10 % of 1 000 000 at the order bar's close
        // 77249.50 → 1.294506 (TradingView), not at the 77249.60 fill → 1.294505. A limit order
        // sizes at its fill (E046 E049, 39/39); gapped through (fill 99.00 below its 99.50 limit,
        // uncaptured) it keeps §4.7.3's fill-price reading.
        let mut m = meta(E);
        m.decl.default_qty_type = pine_ir::QtyType::PercentOfEquity;
        m.decl.default_qty_value_1e6 = 10 * E;
        let mut b = broker(&m);
        run(
            &mut b,
            &bar(0, 7_724_950, 7_724_950, 7_724_950, 7_724_950),
            &[entry()],
        );
        run(
            &mut b,
            &bar(1, 7_724_960, 7_725_430, 7_690_320, 7_696_430),
            &[],
        );
        assert_eq!(b.open_trade(0).map(|t| t.qty_1e6), Some(1_294_506));

        let mut b = broker(&m);
        let lim = OrderIntent {
            kind: IntentKind::Entry,
            id: L1,
            flags: f::LIMIT,
            limit_1e6: 99_500_000,
            ..OrderIntent::default()
        };
        run(&mut b, &bar(0, 10_000, 10_000, 10_000, 10_000), &[lim]);
        run(&mut b, &bar(1, 9_900, 9_950, 9_850, 9_900), &[]);
        let t = b.open_trade(0).map(|t| (t.entry_px_1e6, t.qty_1e6));
        assert_eq!(t, Some((99 * E, 1_010_101_010)));
    }

    #[test]
    fn a_verified_limit_through_the_market_fills_at_the_open() {
        // CI-034 / CI-036, `pin:05-fill-limits` — E120: `backtest_fill_limits_assumption = 3`, a
        // buy limit 50 ticks above the close fills at the next bar's open (45 / 45), not at its
        // limit. Here: limit 100.50, verification 3 ticks (100.47), bar 1 opens at 100.10.
        let mut m = meta(E);
        m.decl.fill_limits_ticks = 3;
        let lim = OrderIntent {
            kind: IntentKind::Entry,
            id: L1,
            flags: f::LIMIT,
            limit_1e6: 100_500_000,
            ..OrderIntent::default()
        };
        let fill = |b: &mut TvBroker| {
            run(b, &bar(0, 10_000, 10_000, 10_000, 10_000), &[lim]);
            run(b, &bar(1, 10_010, 10_060, 10_000, 10_050), &[]);
            b.open_trade(0).map(|t| t.entry_px_1e6)
        };
        assert_eq!(fill(&mut broker(&m)), Some(100_100_000));
        let at_limit = crate::switch::Switches {
            fill_limits_price: crate::switch::FillLimitsPrice::AtLimit,
            ..crate::switch::SWITCHES
        };
        let mut b = TvBroker::with_switches(&m, CFG, at_limit).unwrap_or_else(|_| unreachable!());
        assert_eq!(fill(&mut b), Some(100_500_000));
    }
}
