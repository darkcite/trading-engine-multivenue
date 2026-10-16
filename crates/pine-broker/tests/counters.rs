// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.10 `counters.rs` — I37: each `BrokerCounters` field moves at its §4.15 site and
//! nowhere else. Every test snapshots the counters right before the one call that is the site and
//! asserts the exact set of fields that call moved (companions named: a modification is also a
//! committed intent; a maker that dies at gate 4 rides beside the one that was sent).

use pine_broker::{code, EngineIntentBuf, NativeBroker, TvBroker, TvStep};
use pine_ir::{
    decl_flag, intent_flag as f, AllowEntry, Bar, BrokerCounters, ChartInfo, IntentKind,
    IntentSide, LimitPrimitive, NativeCfg, OcaType, OrderIntent, ProgramMeta, QtyType, Quote,
    SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;
const NOW: u64 = 10_000_000_000;

fn fields(c: &BrokerCounters) -> [(&'static str, u64); 26] {
    [
        ("skipped_cap", c.skipped_cap),
        ("skipped_max_open", c.skipped_max_open),
        ("ioc_remainder_cancelled", c.ioc_remainder_cancelled),
        ("ioc_sent", c.ioc_sent),
        ("ioc_filled", c.ioc_filled),
        ("maker_sent", c.maker_sent),
        ("maker_filled", c.maker_filled),
        ("modifies", c.modifies),
        ("skipped_stale", c.skipped_stale),
        ("skipped_min_notional", c.skipped_min_notional),
        ("refused_pct_equity_live", c.refused_pct_equity_live),
        ("unknown_fills", c.unknown_fills),
        ("sized_to_zero", c.sized_to_zero),
        ("intents_committed", c.intents_committed),
        ("intents_modified", c.intents_modified),
        ("entries_refused_pyramiding", c.entries_refused_pyramiding),
        (
            "entries_refused_allow_entry_in",
            c.entries_refused_allow_entry_in,
        ),
        (
            "entries_clamped_max_position",
            c.entries_clamped_max_position,
        ),
        ("orders_cancelled_oca", c.orders_cancelled_oca),
        ("orders_reduced_oca", c.orders_reduced_oca),
        ("margin_calls", c.margin_calls),
        ("risk_trips_drawdown", c.risk_trips_drawdown),
        ("risk_trips_intraday_loss", c.risk_trips_intraday_loss),
        ("risk_trips_intraday_orders", c.risk_trips_intraday_orders),
        ("risk_trips_cons_loss_days", c.risk_trips_cons_loss_days),
        ("reevals_on_order_fill", c.reevals_on_order_fill),
    ]
}

/// The fields that differ between `a` and `b`, each by exactly one.
fn moved(a: &BrokerCounters, b: &BrokerCounters) -> Vec<&'static str> {
    let (x, y) = (fields(a), fields(b));
    let mut out = Vec::new();
    for i in 0..x.len() {
        if x[i].1 != y[i].1 {
            assert_eq!(y[i].1, x[i].1 + 1, "{} moved by more than one", x[i].0);
            out.push(x[i].0);
        }
    }
    out
}

#[test]
fn the_table_names_every_field_once() {
    // 26 u64 fields: the row schema of 00-INDEX §3.12, all covered by `fields`.
    assert_eq!(core::mem::size_of::<BrokerCounters>(), 26 * 8);
    let n = fields(&BrokerCounters::default());
    for i in 0..n.len() {
        for j in i + 1..n.len() {
            assert_ne!(n[i].0, n[j].0);
        }
    }
}

// ---------------------------------------------------------------- TV ----

fn meta() -> ProgramMeta {
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

fn tv(m: &ProgramMeta) -> TvBroker {
    TvBroker::new(m, CFG).unwrap_or_else(|c| panic!("{c:?}"))
}

fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        open_1e6: o,
        high_1e6: h,
        low_1e6: l,
        close_1e6: c,
        index: i,
        open_ms: i64::from(i) * 60_000,
        ..Bar::default()
    }
}

fn flat(i: u32) -> Bar {
    bar(i, 100 * E, 101 * E, 99 * E, 100 * E)
}

/// One bar; `intents` go to its first evaluation.
fn run(b: &mut TvBroker, x: &Bar, intents: &[OrderIntent]) {
    let mut step = b.open_bar(x);
    let mut first = true;
    while let TvStep::Evaluate { .. } = step {
        let s = if first { intents } else { &[] };
        first = false;
        step = b.submit(s).unwrap_or_else(|e| panic!("{e:?}"));
    }
}

/// `run`, returning the fields the bar moved.
fn run_moved(b: &mut TvBroker, x: &Bar, intents: &[OrderIntent]) -> Vec<&'static str> {
    let before = *b.counters();
    run(b, x, intents);
    moved(&before, b.counters())
}

fn entry(id: u64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(id),
        seq: u32::try_from(id).unwrap_or(0),
        ..OrderIntent::default()
    }
}

fn limit(id: u64, px: i64) -> OrderIntent {
    OrderIntent {
        flags: f::LIMIT,
        limit_1e6: px,
        ..entry(id)
    }
}

fn close(id: u64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(id),
        seq: 9,
        ..OrderIntent::default()
    }
}

#[test]
fn intents_committed_per_committed_intent() {
    let mut b = tv(&meta());
    assert_eq!(
        b.open_bar(&flat(0)),
        TvStep::Evaluate {
            bar: flat(0),
            intrabar: false
        }
    );
    let before = *b.counters();
    b.submit(&[entry(1)]).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(moved(&before, b.counters()), ["intents_committed"]);
    // Per intent: a two-intent slice counts two.
    run(&mut b, &flat(1), &[]);
    b.open_bar(&flat(2));
    let n = b.counters().intents_committed;
    b.submit(&[limit(2, 90 * E), limit(3, 91 * E)])
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(b.counters().intents_committed, n + 2);
}

#[test]
fn intents_modified_per_same_key_modification() {
    let mut b = tv(&meta());
    run(&mut b, &flat(0), &[limit(1, 90 * E)]);
    assert_eq!(
        run_moved(&mut b, &flat(1), &[limit(1, 91 * E)]),
        ["intents_committed", "intents_modified"]
    );
}

#[test]
fn entries_refused_pyramiding_at_the_fill() {
    let mut b = tv(&meta());
    run(&mut b, &flat(0), &[entry(1)]);
    run(&mut b, &flat(1), &[entry(2)]);
    assert_eq!(b.view().opentrades, 1);
    assert_eq!(
        run_moved(&mut b, &flat(2), &[]),
        ["entries_refused_pyramiding"]
    );
    assert_eq!(b.view().opentrades, 1);
}

#[test]
fn entries_refused_allow_entry_in_at_the_commit() {
    let mut m = meta();
    m.decl.risk.allow_entry_in = AllowEntry::Long;
    let mut b = tv(&m);
    let short = OrderIntent {
        side: IntentSide::Short,
        ..entry(1)
    };
    assert_eq!(
        run_moved(&mut b, &flat(0), &[short]),
        ["intents_committed", "entries_refused_allow_entry_in"]
    );
    assert_eq!(run_moved(&mut b, &flat(1), &[]), Vec::<&str>::new());
}

#[test]
fn entries_clamped_max_position_at_the_fill() {
    let mut m = meta();
    m.decl.risk.max_position_size_1e6 = E / 2;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    assert_eq!(
        run_moved(&mut b, &flat(1), &[]),
        ["entries_clamped_max_position"]
    );
    assert_eq!(b.view().position_size_1e6, E / 2);
}

#[test]
fn sized_to_zero_when_the_size_floors_to_nothing() {
    let mut m = meta();
    m.chart.qty_step_1e6 = 10 * E;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    assert_eq!(run_moved(&mut b, &flat(1), &[]), ["sized_to_zero"]);
    assert_eq!(b.view().position_size_1e6, 0);
}

fn oca(it: OrderIntent, ty: OcaType) -> OrderIntent {
    OrderIntent {
        oca_name: StrKey(7),
        oca_type: ty,
        flags: it.flags | f::QTY,
        qty_1e6: 2 * E,
        ..it
    }
}

#[test]
fn orders_cancelled_oca_per_member() {
    let mut m = meta();
    m.decl.pyramiding = 2;
    let mut b = tv(&m);
    let (a, c) = (
        oca(limit(1, 99 * E), OcaType::Cancel),
        oca(limit(2, 98 * E), OcaType::Cancel),
    );
    run(
        &mut b,
        &bar(0, 100 * E, 101 * E, 99_500_000, 100 * E),
        &[a, c],
    );
    // 99 is reached, 98 is not: A fills, B is cancelled by its group.
    assert_eq!(
        run_moved(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[]),
        ["orders_cancelled_oca"]
    );
    assert_eq!(
        run_moved(&mut b, &bar(2, 98 * E, 99 * E, 97 * E, 98 * E), &[]),
        Vec::<&str>::new()
    );
    assert_eq!(b.view().opentrades, 1);
}

#[test]
fn orders_reduced_oca_per_member() {
    let mut m = meta();
    m.decl.pyramiding = 2;
    let mut b = tv(&m);
    let (a, c) = (
        oca(limit(1, 99 * E), OcaType::Reduce),
        oca(limit(2, 98 * E), OcaType::Reduce),
    );
    run(
        &mut b,
        &bar(0, 100 * E, 101 * E, 99_500_000, 100 * E),
        &[a, c],
    );
    assert_eq!(
        run_moved(&mut b, &bar(1, 100 * E, 101 * E, 99 * E, 100 * E), &[]),
        ["orders_reduced_oca"]
    );
    // B (2) reduced by A's 2 dies: nothing fills at 98.
    assert_eq!(
        run_moved(&mut b, &bar(2, 98 * E, 99 * E, 97 * E, 98 * E), &[]),
        Vec::<&str>::new()
    );
    assert_eq!(b.view().position_size_1e6, 2 * E);
}

#[test]
fn margin_calls_at_the_tick() {
    let mut m = meta();
    m.decl.initial_capital_1e6 = 1_000 * E;
    m.decl.margin_long_pct_1e6 = 50 * E;
    let mut b = tv(&m);
    let big = OrderIntent {
        flags: f::QTY,
        qty_1e6: 15 * E,
        ..entry(1)
    };
    run(&mut b, &flat(0), &[big]);
    run(&mut b, &flat(1), &[]);
    assert_eq!(b.view().position_size_1e6, 15 * E);
    // At 60: equity 400 < margin 450 → one call.
    assert_eq!(
        run_moved(&mut b, &bar(2, 100 * E, 100 * E, 60 * E, 60 * E), &[]),
        ["margin_calls"]
    );
    assert!(b.view().position_size_1e6 < 15 * E);
}

#[test]
fn risk_trips_drawdown_once_per_trip() {
    let mut m = meta();
    m.decl.risk.max_drawdown_1e6 = 2 * E;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    run(&mut b, &flat(1), &[]);
    assert_eq!(
        run_moved(&mut b, &bar(2, 100 * E, 100 * E, 97 * E, 98 * E), &[]),
        ["risk_trips_drawdown"]
    );
    assert_eq!(b.view().position_size_1e6, 0);
    // Halted for good: a further drop trips nothing.
    assert_eq!(
        run_moved(&mut b, &bar(3, 98 * E, 98 * E, 90 * E, 91 * E), &[entry(1)]),
        Vec::<&str>::new()
    );
}

#[test]
fn risk_trips_intraday_loss_once_per_trip() {
    let mut m = meta();
    m.decl.risk.max_intraday_loss_1e6 = 2 * E;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    run(&mut b, &flat(1), &[]);
    assert_eq!(
        run_moved(&mut b, &bar(2, 100 * E, 100 * E, 97 * E, 98 * E), &[]),
        ["risk_trips_intraday_loss"]
    );
    assert_eq!(b.view().position_size_1e6, 0);
}

#[test]
fn risk_trips_intraday_orders_once_per_trip() {
    let mut m = meta();
    m.decl.risk.max_intraday_filled_orders = 1;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    run(&mut b, &flat(1), &[close(1)]);
    // The second fill of the day exceeds the bound.
    assert_eq!(
        run_moved(&mut b, &flat(2), &[]),
        ["risk_trips_intraday_orders"]
    );
}

#[test]
fn risk_trips_cons_loss_days_at_the_day_roll() {
    let mut m = meta();
    m.decl.risk.max_cons_loss_days = 1;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    run(&mut b, &flat(1), &[close(1)]);
    run(&mut b, &bar(2, 95 * E, 96 * E, 94 * E, 95 * E), &[]);
    assert!(b.view().netprofit_1e12 < 0, "day 0 lost");
    let day1 = Bar {
        open_ms: 86_400_000,
        ..flat(3)
    };
    assert_eq!(run_moved(&mut b, &day1, &[]), ["risk_trips_cons_loss_days"]);
}

#[test]
fn reevals_on_order_fill_per_intrabar_evaluate() {
    let mut m = meta();
    m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
    m.uses_intrabar = true;
    let mut b = tv(&m);
    run(&mut b, &flat(0), &[entry(1)]);
    let before = *b.counters();
    let mut step = b.open_bar(&flat(1));
    let mut intrabar = 0;
    while let TvStep::Evaluate { intrabar: ib, .. } = step {
        intrabar += u64::from(ib);
        step = b.submit(&[]).unwrap_or_else(|e| panic!("{e:?}"));
    }
    assert_eq!(intrabar, 1);
    assert_eq!(moved(&before, b.counters()), ["reevals_on_order_fill"]);
}

// ------------------------------------------------------------ native ----

fn ncfg(prim: LimitPrimitive) -> NativeCfg {
    NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 10_000 * E,
        cap_day_1e6: 100_000 * E,
        min_notional_1e6: 10 * E,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: prim,
        live: 0,
        _pad: [0; 6],
    }
}

fn native_meta() -> ProgramMeta {
    ProgramMeta {
        decl: StrategyDecl {
            pyramiding: 5,
            ..meta().decl
        },
        ..meta()
    }
}

fn native(m: &ProgramMeta, c: NativeCfg) -> NativeBroker {
    let mut b = NativeBroker::new(m, c).unwrap_or_else(|x| panic!("{x:?}"));
    b.open_bar(&Bar {
        close_1e6: 100 * E,
        index: 1,
        ..Bar::default()
    });
    b
}

fn quote(bid: i64, ask: i64) -> Quote {
    Quote {
        bid_1e6: bid,
        ask_1e6: ask,
        bid_qty_1e6: 100 * E,
        ask_qty_1e6: 100 * E,
        ts_ns: NOW,
        ..Quote::default()
    }
}

/// `reconcile` at the 100 touch; the fields it moved.
fn rec_moved(b: &mut NativeBroker, out: &mut EngineIntentBuf) -> Vec<&'static str> {
    let before = *b.counters();
    b.reconcile(&quote(100 * E, 100 * E), NOW, out)
        .unwrap_or_else(|x| panic!("{x:?}"));
    moved(&before, b.counters())
}

fn commit(b: &mut NativeBroker, it: &[OrderIntent]) {
    b.apply_intents(it, NOW).unwrap_or_else(|x| panic!("{x:?}"));
}

#[test]
fn ioc_sent_when_an_ioc_submit_is_written() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["ioc_sent"]);
    assert_eq!(
        rec_moved(&mut b, &mut out),
        Vec::<&str>::new(),
        "one IoC in flight"
    );
}

#[test]
fn ioc_filled_at_the_first_fill_only() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    rec_moved(&mut b, &mut out);
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 2, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), ["ioc_filled"]);
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 - e.qty_1e6 / 2, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), Vec::<&str>::new());
}

#[test]
fn ioc_remainder_cancelled_on_an_acked_ioc_only() {
    let mut out = EngineIntentBuf::new();
    // Refused before judgment: no ack, no count.
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    commit(&mut b, &[entry(1)]);
    rec_moved(&mut b, &mut out);
    let before = *b.counters();
    b.on_cancelled(out.as_slice()[0].client_oid);
    assert_eq!(moved(&before, b.counters()), Vec::<&str>::new());
    // Acked, judged with a remainder: counted.
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    commit(&mut b, &[entry(1)]);
    rec_moved(&mut b, &mut out);
    let oid = out.as_slice()[0].client_oid;
    b.on_ack(oid);
    let before = *b.counters();
    b.on_cancelled(oid);
    assert_eq!(moved(&before, b.counters()), ["ioc_remainder_cancelled"]);
}

#[test]
fn maker_sent_when_a_maker_submit_is_written() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Maker));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[limit(1, 95 * E)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["maker_sent"]);
}

#[test]
fn maker_filled_at_the_first_fill_only() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Maker));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[limit(1, 95 * E)]);
    rec_moved(&mut b, &mut out);
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 2, 95 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), ["maker_filled"]);
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 4, 95 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), Vec::<&str>::new());
}

#[test]
fn modifies_when_a_modify_is_written() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Maker));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[limit(1, 95 * E)]);
    rec_moved(&mut b, &mut out);
    b.on_ack(out.as_slice()[0].client_oid);
    commit(&mut b, &[limit(1, 96 * E)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["modifies"]);
}

#[test]
fn skipped_stale_when_a_wanted_submit_meets_a_stale_quote() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    let before = *b.counters();
    let stale = Quote {
        ts_ns: NOW - 3_000_000_000,
        ..quote(100 * E, 100 * E)
    };
    b.reconcile(&stale, NOW, &mut out)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), ["skipped_stale"]);
    assert!(out.is_empty());
}

#[test]
fn skipped_min_notional_at_gate_3() {
    let mut c = ncfg(LimitPrimitive::Ioc);
    c.min_notional_1e6 = 2_000 * E;
    let mut b = native(&native_meta(), c);
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["skipped_min_notional"]);
}

#[test]
fn skipped_max_open_at_gate_4() {
    let mut c = ncfg(LimitPrimitive::Maker);
    c.max_open = 2;
    let mut b = native(&native_meta(), c);
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[limit(1, 95 * E), limit(2, 94 * E)]);
    assert_eq!(
        rec_moved(&mut b, &mut out),
        ["skipped_max_open", "maker_sent"]
    );
}

#[test]
fn skipped_cap_at_gate_5_or_6() {
    let mut c = ncfg(LimitPrimitive::Ioc);
    c.cap_instance_1e6 = 500 * E;
    let mut b = native(&native_meta(), c);
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["skipped_cap"]);
    let mut c = ncfg(LimitPrimitive::Ioc);
    c.cap_day_1e6 = 500 * E;
    let mut b = native(&native_meta(), c);
    commit(&mut b, &[entry(1)]);
    assert_eq!(rec_moved(&mut b, &mut out), ["skipped_cap"]);
}

#[test]
fn unknown_fills_for_an_oid_never_issued() {
    let mut b = native(&native_meta(), ncfg(LimitPrimitive::Ioc));
    for oid in [0_u32, 99] {
        let before = *b.counters();
        assert!(b.on_fill(oid, E, 100 * E, 0, 1, 0).is_err());
        assert_eq!(moved(&before, b.counters()), ["unknown_fills"]);
        assert_eq!(b.view().position_size_1e6, 0);
    }
}

#[test]
fn refused_pct_equity_live_never_moves_on_a_constructed_broker() {
    let mut m = native_meta();
    m.decl.default_qty_type = QtyType::PercentOfEquity;
    m.decl.default_qty_value_1e6 = 10 * E;
    let mut live = ncfg(LimitPrimitive::Ioc);
    live.live = 1;
    assert_eq!(
        NativeBroker::new(&m, live).err(),
        Some(code::PCT_EQUITY_LIVE)
    );
    // Paper admits it; a whole round trip never touches the field.
    let mut b = native(&m, ncfg(LimitPrimitive::Ioc));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    rec_moved(&mut b, &mut out);
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    b.on_fill(e.client_oid, e.qty_1e6, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(b.counters().refused_pct_equity_live, 0);
}

#[test]
fn reevals_on_order_fill_arms_once_per_evaluation() {
    let mut m = native_meta();
    m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
    m.uses_intrabar = true;
    let mut b = native(&m, ncfg(LimitPrimitive::Ioc));
    let mut out = EngineIntentBuf::new();
    commit(&mut b, &[entry(1)]);
    rec_moved(&mut b, &mut out);
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 2, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(
        moved(&before, b.counters()),
        ["ioc_filled", "reevals_on_order_fill"]
    );
    // A second fill before the next evaluation does not re-arm the latch.
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 4, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), Vec::<&str>::new());
    // `open_bar` (before the next evaluation) clears it.
    b.open_bar(&Bar {
        close_1e6: 100 * E,
        index: 1,
        ..Bar::default()
    });
    let before = *b.counters();
    b.on_fill(e.client_oid, e.qty_1e6 / 4, 100 * E, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
    assert_eq!(moved(&before, b.counters()), ["reevals_on_order_fill"]);
}
