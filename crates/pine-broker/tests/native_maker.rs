// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.9 (part): the `Maker` column of §4.12.2 — resting post-only makers, `Modify`
//! requotes (E-7), cancel-then-submit when a requote cannot be proven safe (§4.12.4 rule 4),
//! `Cancel` take-backs (E-8) and their re-emission, the venue-slot budget.

use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, EngineVerb, IntentKind, LimitPrimitive, NativeCfg,
    OrderIntent, Primitive, ProgramMeta, Quote, StrKey, StrategyDecl,
};

const E: i64 = 1_000_000;
const NOW: u64 = 10_000_000_000;
const STALE: i64 = 2_000_000_000;

fn broker(max_open: u32) -> NativeBroker {
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
            pyramiding: 5,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    let cfg = NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 100_000 * E,
        cap_day_1e6: 1_000_000 * E,
        min_notional_1e6: 0,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: STALE,
        cross_ticks: 5,
        max_open,
        limit_primitive: LimitPrimitive::Maker,
        live: 0,
        _pad: [0; 6],
    };
    let mut b = NativeBroker::new(&meta, cfg).unwrap_or_else(|_| unreachable!());
    b.open_bar(&Bar {
        close_1e6: 100 * E,
        index: 1,
        ..Bar::default()
    });
    b
}

fn quote(bid: i64, ask: i64, ts: u64) -> Quote {
    Quote {
        bid_1e6: bid,
        ask_1e6: ask,
        bid_qty_1e6: E,
        ask_qty_1e6: E,
        ts_ns: ts,
        ..Quote::default()
    }
}

fn limit_entry(id: u64, px: i64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(id),
        flags: f::LIMIT,
        limit_1e6: px,
        ..OrderIntent::default()
    }
}

#[test]
fn a_limit_below_the_touch_rests_as_a_post_only_maker() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[limit_entry(1, 95 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.verb, e.primitive, e.side, e.flags),
        (EngineVerb::Submit, Primitive::MakerPostOnly, 0, 0)
    );
    // 1 000 USD at the limit 95 → 10.526 contracts on the 0.001 step.
    assert_eq!((e.px_1e6, e.qty_1e6, e.ttl_ns), (95 * E, 10_526_000, 0));
    assert_eq!(b.counters().maker_sent, 1);
    b.on_ack(e.client_oid);
    // Unchanged: nothing more.
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    assert_eq!(b.view().position_size_1e6, 0, "the ack is not the fill");
}

#[test]
fn a_moved_limit_is_requoted_with_a_modify() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[limit_entry(1, 95 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let first = out.as_slice()[0];
    b.on_ack(first.client_oid);
    b.apply_intents(&[limit_entry(1, 96 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let m = out.as_slice()[0];
    assert_eq!(
        (m.verb, m.prev_client_oid, m.px_1e6),
        (EngineVerb::Modify, first.client_oid, 96 * E)
    );
    assert!(m.client_oid > first.client_oid);
    assert_eq!(b.counters().modifies, 1);
    // Host protocol: Ok → ack the replacement, then prev is over.
    b.on_ack(m.client_oid);
    b.on_cancelled(first.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    // A fill of the replacement books the position.
    b.on_fill(m.client_oid, m.qty_1e6, 96 * E, 0, 1, 0).unwrap();
    assert_eq!(b.view().position_size_1e6, m.qty_1e6);
    assert_eq!(b.counters().maker_filled, 1);
}

#[test]
fn a_cancelled_record_takes_its_maker_back_and_re_emits_until_confirmed() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[limit_entry(1, 95 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let mk = out.as_slice()[0];
    b.on_ack(mk.client_oid);
    let cancel = OrderIntent {
        kind: IntentKind::Cancel,
        id: StrKey(1),
        ..OrderIntent::default()
    };
    b.apply_intents(&[cancel], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let c = out.as_slice()[0];
    assert_eq!((c.verb, c.client_oid), (EngineVerb::Cancel, mk.client_oid));
    assert_eq!(
        (c.side, c.px_1e6, c.qty_1e6),
        (mk.side, mk.px_1e6, mk.qty_1e6),
        "repeats the order"
    );
    // Not confirmed: nothing before quote_stale_ns, re-emitted after — even on a stale quote.
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW + 1, &mut out)
        .unwrap();
    assert!(out.is_empty());
    let later = NOW + 3_000_000_000;
    b.reconcile(&quote(100 * E, 100 * E, NOW), later, &mut out)
        .unwrap();
    assert_eq!(
        out.as_slice().first().map(|e| e.verb),
        Some(EngineVerb::Cancel)
    );
    b.on_cancelled(mk.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, later), later, &mut out)
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn a_take_profit_covering_the_position_is_requoted_by_cancel_then_submit() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    // Enter at market (IoC), fill 10.
    let entry = OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(1),
        ..OrderIntent::default()
    };
    b.apply_intents(&[entry], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    b.on_fill(e.client_oid, e.qty_1e6, 100 * E, 0, 1, 0)
        .unwrap();
    let pos = b.view().position_size_1e6;
    let tp = |px: i64| OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT,
        limit_1e6: px,
        ..OrderIntent::default()
    };
    b.apply_intents(&[tp(105 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let m = out.as_slice()[0];
    assert_eq!(
        (m.verb, m.primitive, m.side, m.flags, m.qty_1e6),
        (EngineVerb::Submit, Primitive::MakerPostOnly, 1, 1, pos)
    );
    b.on_ack(m.client_oid);
    // Move the take-profit: a Modify cannot be proven safe (it would double-cover the position),
    // so the maker is cancelled first …
    b.apply_intents(&[tp(106 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out.as_slice()[0].verb, EngineVerb::Cancel);
    assert_eq!(b.counters().modifies, 0);
    // … and re-submitted at the new level after its disposition.
    b.on_cancelled(m.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let r = out.as_slice()[0];
    assert_eq!(
        (r.verb, r.px_1e6, r.qty_1e6),
        (EngineVerb::Submit, 106 * E, pos)
    );
}

#[test]
fn an_increasing_maker_without_a_slot_dies_and_is_counted() {
    // max_open 2: one slot is the IoC's, one maker may rest.
    let mut b = broker(2);
    let mut out = EngineIntentBuf::new();
    let second = OrderIntent {
        seq: 1,
        ..limit_entry(2, 94 * E)
    };
    b.apply_intents(&[limit_entry(1, 95 * E), second], NOW)
        .unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(b.counters().skipped_max_open, 1);
}

#[test]
fn a_marketable_limit_fires_an_ioc_inside_its_limit() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[limit_entry(1, 100 * E + 20_000)], NOW)
        .unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let e = out.as_slice()[0];
    assert_eq!(
        (e.primitive, e.px_1e6),
        (Primitive::TakerIoc, 100 * E + 20_000)
    );
}

#[test]
fn flatten_cancels_every_live_maker() {
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[limit_entry(1, 95 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let mk = out.as_slice()[0];
    b.on_ack(mk.client_oid);
    // A stale quote: the Cancel is still written (I26), no Submit.
    b.flatten(&quote(100 * E, 100 * E, 1), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out.as_slice()[0].verb, out.as_slice()[0].client_oid),
        (EngineVerb::Cancel, mk.client_oid)
    );
}
