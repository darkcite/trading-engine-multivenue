// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.9 (part): `NativeBroker::reconcile` / `flatten` over the `Ioc` column, driven by
//! hand through the §3.6.2 host protocol.

use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, EngineVerb, IntentKind, LimitPrimitive, NativeCfg,
    OrderIntent, Primitive, ProgramMeta, Quote, StrKey, StrategyDecl,
};

const E: i64 = 1_000_000;
const NOW: u64 = 10_000_000_000;

fn cfg() -> NativeCfg {
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
        limit_primitive: LimitPrimitive::Ioc,
        live: 0,
        _pad: [0; 6],
    }
}

fn broker(c: NativeCfg) -> NativeBroker {
    let meta = ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1_000,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 100_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 1,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    let mut b = NativeBroker::new(&meta, c).unwrap_or_else(|_| unreachable!());
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
        bid_qty_1e6: E,
        ask_qty_1e6: E,
        ts_ns: NOW,
        ..Quote::default()
    }
}

fn entry() -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(1),
        ..OrderIntent::default()
    }
}

/// Fill the single IoC in `out` completely at `px` and report it as the host would.
fn fill_all(b: &mut NativeBroker, out: &EngineIntentBuf, px: i64) {
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    let q = if e.side == 0 { e.qty_1e6 } else { -e.qty_1e6 };
    b.on_fill(e.client_oid, q, px, 0, 1, 0)
        .unwrap_or_else(|x| panic!("{x:?}"));
}

#[test]
fn a_market_entry_fires_one_ioc_at_the_band() {
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    let e = out.as_slice()[0];
    assert_eq!(
        (e.verb, e.primitive, e.side, e.flags),
        (EngineVerb::Submit, Primitive::TakerIoc, 0, 0)
    );
    // The band: ask + 5 ticks; the unit: 1 000 USD at the ask 100 → 10 contracts.
    assert_eq!(
        (e.px_1e6, e.qty_1e6, e.ttl_ns),
        (100 * E + 50_000, 10 * E, 2_000_000_000)
    );
    assert_eq!(e.client_oid, 1);
    // One IoC in flight: nothing more until its disposition.
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    // The ack is not the fill (E-5): the view is untouched until on_fill.
    b.on_ack(1);
    assert_eq!(b.view().position_size_1e6, 0);
    b.on_fill(1, 10 * E, 100 * E, 0, 1, 0).unwrap();
    assert_eq!(b.view().position_size_1e6, 10 * E);
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty(), "the entry is done");
    assert_eq!(b.counters().ioc_sent, 1);
}

#[test]
fn a_stale_quote_blocks_every_emission() {
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    let stale = Quote {
        ts_ns: NOW - 3_000_000_000,
        ..quote(100 * E, 100 * E)
    };
    b.reconcile(&stale, NOW, &mut out).unwrap();
    assert!(out.is_empty());
    assert_eq!(b.counters().skipped_stale, 1);
}

#[test]
fn a_synthetic_stop_loss_fires_a_reduce_only_ioc() {
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LOSS,
        loss_ticks_1e6: 100 * E, // 1.00 below the entry
        seq: 1,
        ..OrderIntent::default()
    };
    b.apply_intents(&[entry(), ex], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut b, &out, 100 * E);
    // Above the stop: nothing; at 99: the stop fires for the whole position, reduce-only.
    b.reconcile(&quote(99_500_000, 99_510_000), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    b.reconcile(&quote(99 * E, 99 * E + 10_000), NOW, &mut out)
        .unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.side, e.flags, e.qty_1e6),
        (Primitive::ReduceOnlyIoc, 1, 1, 10 * E)
    );
    assert_eq!(e.id, StrKey(9));
    fill_all(&mut b, &out, 99 * E);
    assert_eq!(b.view().position_size_1e6, 0);
    assert_eq!(b.closed_trade(0).map(|t| t.exit_id), Some(StrKey(9)));
}

#[test]
fn reducers_never_exceed_the_position() {
    // I28: a close and an exit both want to reduce; the second waits for the first's disposition
    // and then sizes against |pos| − R.
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut b, &out, 100 * E);
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(1),
        seq: 0,
        ..OrderIntent::default()
    };
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LOSS,
        loss_ticks_1e6: 10 * E,
        seq: 1,
        ..OrderIntent::default()
    };
    b.apply_intents(&[close, ex], NOW).unwrap();
    b.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out.as_slice()[0].qty_1e6, 10 * E);
    // In flight: nothing else, however many reducers want to fire.
    let mut again = EngineIntentBuf::new();
    b.reconcile(&quote(99 * E, 99 * E), NOW, &mut again)
        .unwrap();
    assert!(again.is_empty());
    fill_all(&mut b, &out, 99 * E);
    assert_eq!(b.view().position_size_1e6, 0);
    b.reconcile(&quote(98 * E, 98 * E), NOW, &mut out).unwrap();
    assert!(out.is_empty(), "flat: no reducer is sent");
}

#[test]
fn the_cap_refuses_an_entry_and_counts_it() {
    let c = NativeCfg {
        cap_instance_1e6: 500 * E,
        ..cfg()
    };
    let mut b = broker(c);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    assert_eq!(b.counters().skipped_cap, 1);
}

#[test]
fn flatten_closes_the_position_and_stops_committing() {
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut b, &out, 100 * E);
    b.flatten(&quote(100 * E, 100 * E), NOW, &mut out).unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.side, e.flags, e.qty_1e6),
        (Primitive::ReduceOnlyIoc, 1, 1, 10 * E)
    );
    // Idempotent while the reducer is in flight.
    let oid = e.client_oid;
    b.flatten(&quote(100 * E, 100 * E), NOW, &mut out).unwrap();
    assert!(out.is_empty());
    b.on_ack(oid);
    b.on_fill(oid, -10 * E, 100 * E, 0, 1, 0).unwrap();
    assert_eq!(b.view().position_size_1e6, 0);
    // Halted for good: a new entry is not committed.
    b.apply_intents(&[OrderIntent { seq: 0, ..entry() }], NOW)
        .unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn a_native_risk_trip_flattens_at_the_next_reconcile() {
    let mut out = EngineIntentBuf::new();
    let mut meta = ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1_000,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 100_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 1,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    meta.decl.risk.max_drawdown_1e6 = 5 * E;
    let mut r = NativeBroker::new(&meta, cfg()).unwrap_or_else(|_| unreachable!());
    r.open_bar(&Bar {
        close_1e6: 100 * E,
        index: 1,
        ..Bar::default()
    });
    r.apply_intents(&[entry()], NOW).unwrap();
    r.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut r, &out, 100 * E);
    // 10 contracts lose 1.00 each at the next mark: drawdown 10 ≥ 5 → the flatten is pending.
    r.open_bar(&Bar {
        close_1e6: 99 * E,
        index: 2,
        ..Bar::default()
    });
    assert_eq!(r.counters().risk_trips_drawdown, 1);
    r.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.qty_1e6, e.flags),
        (Primitive::ReduceOnlyIoc, 10 * E, 1)
    );
    fill_all(&mut r, &out, 99 * E);
    let c = r.closed_trade(0).copied().unwrap_or_default();
    assert_ne!(c.flags & pine_ir::trade_flag::RISK_FLATTEN, 0);
    // Halted: a new entry is not committed.
    r.apply_intents(&[entry()], NOW).unwrap();
    r.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn boot_reconciliation_is_pristine_across_open_bar_and_covered_by_any_exit() {
    // I34.
    let mut b = broker(cfg());
    b.open_bar(&Bar {
        close_1e6: 101 * E,
        index: 2,
        ..Bar::default()
    });
    b.open_bar(&Bar {
        close_1e6: 102 * E,
        index: 3,
        ..Bar::default()
    });
    b.reconcile_boot(5 * E, 100 * E, 0);
    assert_eq!(b.view().position_size_1e6, 5 * E);
    assert_eq!(b.view().position_avg_price_1e6, 100 * E);
    // Non-pristine now: a second boot changes nothing.
    b.reconcile_boot(9 * E, 50 * E, 0);
    assert_eq!(b.view().position_size_1e6, 5 * E);
    // An exit naming some entry id covers the boot trade: its stop fires for the position.
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(77),
        flags: f::LOSS,
        loss_ticks_1e6: 100 * E,
        ..OrderIntent::default()
    };
    b.apply_intents(&[ex], NOW).unwrap();
    let mut out = EngineIntentBuf::new();
    b.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.qty_1e6, e.id),
        (Primitive::ReduceOnlyIoc, 5 * E, StrKey(9))
    );
}

#[test]
fn repeated_flattens_against_a_filling_venue_never_cross_zero() {
    // I33: kills every record and rule; one reducer at a time, sized |pos| − R.
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut b, &out, 100 * E);
    let lim = OrderIntent {
        flags: f::LIMIT,
        limit_1e6: 90 * E,
        id: StrKey(2),
        ..entry()
    };
    b.apply_intents(&[lim], NOW).unwrap();
    let mut k = 0;
    while k < 6 {
        b.flatten(&quote(100 * E, 100 * E), NOW, &mut out).unwrap();
        if let Some(e) = out.as_slice().first().copied() {
            assert_eq!(e.flags, 1);
            assert!(e.qty_1e6 <= b.view().position_size_1e6.abs());
            b.on_ack(e.client_oid);
            // The venue fills half, then reports the remainder cancelled.
            let half = (e.qty_1e6 / 2).max(1_000);
            b.on_fill(e.client_oid, -half, 100 * E, 0, 1, 0).unwrap();
            b.on_cancelled(e.client_oid);
        }
        assert!(b.view().position_size_1e6 >= 0, "never past zero");
        k += 1;
    }
    // The pending limit entry was killed: nothing but reducers was ever sent.
    assert!(b.view().position_size_1e6 < 10 * E);
}

#[test]
fn a_close_naming_any_id_closes_the_boot_trade() {
    // `pin:05-boot-close` (lead ruling).
    let mut b = broker(cfg());
    b.reconcile_boot(5 * E, 100 * E, 0);
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(42),
        ..OrderIntent::default()
    };
    b.apply_intents(&[close], NOW).unwrap();
    let mut out = EngineIntentBuf::new();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.side, e.qty_1e6, e.flags),
        (Primitive::ReduceOnlyIoc, 1, 5 * E, 1)
    );
}
