// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33k acceptance — `pin:06-open-trade-export-mark` (the runner's side is
//! `pine-conformance::run`): TradingView prints a still-open trade's run-up / drawdown as of the
//! export, its chart having run on past the compared window to the capture minute. The runner
//! walks the bar root's rows after `TO_MS` through the `TvBroker` with no intents; this pins the
//! broker contract it relies on — a bar walked with no intents marks the open trades over its
//! path (no fill, no new trade) — on E107's trade 45 (TV: run-up 1563, drawdown −3044.9; the
//! chart's own bars give −1077.1). Bars: the captured BTCUSDT.P 1 h OHLC (`fullbars-tvohlc-r33j`)
//! of the trade's entry bar, its run-up bar, the chart's last bar and two rows after `TO_MS` (the
//! 2026-10-15 13:00 low 79732.3).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_broker::{TvBroker, TvStep};
use pine_ir::{
    intent_flag as f, trade_flag, Bar, ChartInfo, CommissionType, IntentKind, IntentSide,
    OrderIntent, ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;
const HOUR_MS: i64 = 3_600_000;

/// BTCUSDT.P on the captures: mintick 0.1, point value 1; 1 000 000 USD, fixed 1, no commission.
fn meta() -> ProgramMeta {
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
            commission_type: CommissionType::Percent,
            commission_value_1e6: 0,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    }
}

const CFG: TvCfg = TvCfg {
    sizing: SizingMode::AsDeclared,
    day_offset_s: 0,
    _pad: 0,
};

/// A bar from the captured OHLC (×1e6 prices).
fn bar(index: u32, open_ms: i64, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        open_1e6: o,
        high_1e6: h,
        low_1e6: l,
        close_1e6: c,
        index,
        open_ms,
        close_ms: open_ms + HOUR_MS,
        ..Bar::default()
    }
}

/// Walks `b` over `bar`, the close evaluation sending `send` (intrabar evaluations send nothing).
fn walk(b: &mut TvBroker, bar: &Bar, send: &[OrderIntent]) {
    let mut step = b.open_bar(bar);
    while let TvStep::Evaluate { intrabar, .. } = step {
        let s: &[OrderIntent] = if intrabar { &[] } else { send };
        step = b
            .submit(s)
            .unwrap_or_else(|e| panic!("bar {}: {e:?}", bar.open_ms));
    }
}

/// The one open trade's `(max_runup_1e6, max_drawdown_1e6)`.
fn excursion(b: &TvBroker) -> (i64, i64) {
    assert_eq!(b.view().opentrades, 1);
    let t = b.open_trade(0).expect("one open trade");
    assert!(t.flags & trade_flag::OPEN != 0);
    (t.max_runup_1e6, t.max_drawdown_1e6)
}

/// E107 trade 45: long 1 at 82777.2 (2026-10-11 04:00), open at the export. On the chart's bars
/// (to `TO_MS`) its run-up is 1563 and its drawdown 1077.1 (the last bar's low 81700.1); the two
/// rows after `TO_MS`, walked with no intents as the runner walks the tail, take the drawdown to
/// TradingView's −3044.9 (the 2026-10-15 13:00 low 79732.3) and leave the run-up (no later high
/// above 84340.2) — no fill, no new trade.
#[test]
fn an_open_trade_is_marked_over_bars_walked_without_intents_e107() {
    let mut b = TvBroker::new(&meta(), CFG).unwrap_or_else(|c| panic!("{c:?}"));
    let entry = OrderIntent {
        kind: IntentKind::Entry,
        side: IntentSide::Long,
        id: StrKey(45),
        flags: f::QTY,
        qty_1e6: E,
        seq: 1,
        ..OrderIntent::default()
    };
    // The bar before the entry sends it; it fills at the next open.
    let t0 = 1_791_691_200_000;
    let pre = bar(
        0,
        t0 - HOUR_MS,
        82_700_000_000,
        82_800_000_000,
        82_650_000_000,
        82_777_200_000,
    );
    walk(&mut b, &pre, &[entry]);
    assert_eq!(b.view().opentrades, 0);
    let chart = [
        bar(
            1,
            t0,
            82_777_200_000,
            82_985_000_000,
            82_751_100_000,
            82_981_100_000,
        ),
        bar(
            2,
            1_791_784_800_000,
            83_969_100_000,
            84_340_200_000,
            83_712_600_000,
            84_178_600_000,
        ),
        bar(
            3,
            1_791_846_000_000,
            82_126_600_000,
            82_232_900_000,
            81_700_100_000,
            81_813_400_000,
        ),
    ];
    for x in &chart {
        walk(&mut b, x, &[]);
    }
    let t = b.open_trade(0).expect("filled");
    assert_eq!(
        (t.entry_ms, t.entry_px_1e6, t.qty_1e6),
        (t0, 82_777_200_000, E)
    );
    // The chart's own bars: run-up 1563, drawdown 1077.1 (the runner's r33p4 value).
    assert_eq!(excursion(&b), (1_563 * E, 1_077_100_000));
    // The tail after `TO_MS`, no intents: 1791849600000 (low 80985.0), 1792069200000 (low 79732.3).
    let tail = [
        bar(
            4,
            1_791_849_600_000,
            81_813_500_000,
            81_913_800_000,
            80_985_000_000,
            81_394_400_000,
        ),
        bar(
            5,
            1_792_069_200_000,
            80_515_300_000,
            80_743_500_000,
            79_732_300_000,
            80_164_500_000,
        ),
    ];
    walk(&mut b, &tail[0], &[]);
    assert_eq!(excursion(&b), (1_563 * E, 1_792_200_000));
    walk(&mut b, &tail[1], &[]);
    // TradingView's List of Trades: run-up 1563, drawdown −3044.9.
    assert_eq!(excursion(&b), (1_563 * E, 3_044_900_000));
    let v = b.view();
    assert_eq!(
        (v.closedtrades, v.opentrades, v.position_size_1e6),
        (0, 1, E)
    );
}
