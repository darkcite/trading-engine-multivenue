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

/// Enter long at market (IoC) for the unit quantity and fill it at 100; returns the position.
fn long_at_100(b: &mut NativeBroker, out: &mut EngineIntentBuf) -> i64 {
    let entry = OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(1),
        ..OrderIntent::default()
    };
    b.apply_intents(&[entry], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, out)
        .unwrap();
    let e = out.as_slice()[0];
    b.on_ack(e.client_oid);
    b.on_fill(e.client_oid, e.qty_1e6, 100 * E, 0, 1, 0)
        .unwrap();
    b.view().position_size_1e6
}

/// A take-profit at 110 from entry 1 resting as a reduce-only maker (`pct` of the trade, 0 =
/// all); returns its sent intent after the ack.
fn rest_tp(
    b: &mut NativeBroker,
    out: &mut EngineIntentBuf,
    extra: OrderIntent,
) -> pine_ir::EngineIntent {
    b.apply_intents(&[extra], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, out)
        .unwrap();
    let m = out.as_slice()[0];
    assert_eq!(
        (m.verb, m.primitive, m.flags),
        (EngineVerb::Submit, Primitive::MakerPostOnly, 1)
    );
    b.on_ack(m.client_oid);
    m
}

fn fill(b: &mut NativeBroker, e: &pine_ir::EngineIntent, px: i64) {
    b.on_ack(e.client_oid);
    let q = if e.side == 0 { e.qty_1e6 } else { -e.qty_1e6 };
    b.on_fill(e.client_oid, q, px, 0, 1, 0).unwrap();
}

#[test]
fn a_trail_arms_and_fires_while_its_own_take_profit_rests() {
    // Review B1 (§4.12.4 rule 2, §4.5.6): the trail steps on every fresh touch even though the
    // set has a resting maker; once through, the take-profit is cancelled and the trail fires.
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    let pos = long_at_100(&mut b, &mut out);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT | f::TRAIL_PRICE | f::TRAIL_OFFSET,
        limit_1e6: 110 * E,
        trail_price_1e6: 102 * E,
        trail_offset_1e6: 100 * E, // 100 ticks = 1.00
        ..OrderIntent::default()
    };
    let tp = rest_tp(&mut b, &mut out, ex);
    assert_eq!((tp.px_1e6, tp.qty_1e6), (110 * E, pos));
    // The touch passes the activation: the trail arms at 103 (stop 102); the TP keeps resting.
    b.reconcile(&quote(103 * E, 103 * E + 10_000, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    // Back through the stop: the TP is taken back first (its covered part waits) …
    b.reconcile(
        &quote(101 * E + 500_000, 101 * E + 510_000, NOW),
        NOW,
        &mut out,
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    let c = out.as_slice()[0];
    assert_eq!((c.verb, c.client_oid), (EngineVerb::Cancel, tp.client_oid));
    // … and after its disposition the trail fires for the whole position.
    b.on_cancelled(tp.client_oid);
    b.reconcile(
        &quote(101 * E + 500_000, 101 * E + 510_000, NOW),
        NOW,
        &mut out,
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    let t = out.as_slice()[0];
    assert_eq!(
        (t.verb, t.primitive, t.side, t.qty_1e6, t.id),
        (
            EngineVerb::Submit,
            Primitive::ReduceOnlyIoc,
            1,
            pos,
            StrKey(9)
        )
    );
}

#[test]
fn a_close_takes_back_a_take_profit_covering_the_position() {
    // Review B2 (§4.12.4 rule 2): room |pos| − R is 0 while the TP rests; the close cancels it
    // and fires after the cancel is confirmed — it never waits forever.
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    let pos = long_at_100(&mut b, &mut out);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT,
        limit_1e6: 110 * E,
        ..OrderIntent::default()
    };
    let tp = rest_tp(&mut b, &mut out, ex);
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(1),
        ..OrderIntent::default()
    };
    b.apply_intents(&[close], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(
        out.len(),
        1,
        "only the take-back: the close is fully covered"
    );
    assert_eq!(
        (out.as_slice()[0].verb, out.as_slice()[0].client_oid),
        (EngineVerb::Cancel, tp.client_oid)
    );
    b.on_cancelled(tp.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(
        out.len(),
        1,
        "the TP is not re-submitted ahead of the close"
    );
    let c = out.as_slice()[0];
    assert_eq!(
        (c.verb, c.primitive, c.side, c.qty_1e6),
        (EngineVerb::Submit, Primitive::ReduceOnlyIoc, 1, pos)
    );
    fill(&mut b, &c, 100 * E);
    assert_eq!(b.view().position_size_1e6, 0);
}

#[test]
fn a_close_all_cut_short_by_a_resting_reducer_lives_until_it_filled_what_it_wanted() {
    // Review B2: the close-all is sent for |pos| − R; that send filling is not its end.
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    let pos = long_at_100(&mut b, &mut out);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT | f::QTY_PCT,
        limit_1e6: 110 * E,
        qty_pct_1e6: 40 * E,
        ..OrderIntent::default()
    };
    let tp = rest_tp(&mut b, &mut out, ex);
    assert!(tp.qty_1e6 > 0 && tp.qty_1e6 < pos);
    let all = OrderIntent {
        kind: IntentKind::CloseAll,
        ..OrderIntent::default()
    };
    b.apply_intents(&[all], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    // Cancel first, then the uncovered part.
    assert_eq!(out.len(), 2);
    assert_eq!(out.as_slice()[0].verb, EngineVerb::Cancel);
    let c1 = out.as_slice()[1];
    assert_eq!(
        (c1.verb, c1.primitive, c1.qty_1e6),
        (
            EngineVerb::Submit,
            Primitive::ReduceOnlyIoc,
            pos - tp.qty_1e6
        )
    );
    fill(&mut b, &c1, 100 * E);
    assert_eq!(b.view().position_size_1e6, tp.qty_1e6);
    b.on_cancelled(tp.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(
        out.len(),
        1,
        "the close-all is still alive for the covered part"
    );
    let c2 = out.as_slice()[0];
    assert_eq!(
        (c2.primitive, c2.side, c2.qty_1e6),
        (Primitive::ReduceOnlyIoc, 1, tp.qty_1e6)
    );
    fill(&mut b, &c2, 100 * E);
    assert_eq!(b.view().position_size_1e6, 0);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty(), "flat: the close-all lapsed");
}

#[test]
fn a_reversal_short_of_its_target_sends_the_remainder_after_the_reducers_dispositions() {
    // Review B2 (§4.12.4 rule 3): q_open + (|pos| − R) first; the TP is taken back and never
    // fills, so the position lands short and the rest follows — never past the target.
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    let pos = long_at_100(&mut b, &mut out);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT | f::QTY_PCT,
        limit_1e6: 110 * E,
        qty_pct_1e6: 40 * E,
        ..OrderIntent::default()
    };
    let tp = rest_tp(&mut b, &mut out, ex);
    let r = tp.qty_1e6;
    let short = OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(2),
        side: pine_ir::IntentSide::Short,
        ..OrderIntent::default()
    };
    b.apply_intents(&[short], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out.as_slice()[0].verb, EngineVerb::Cancel);
    let s1 = out.as_slice()[1];
    // q_open (the unit quantity, = pos here) + |pos| − R.
    assert_eq!((s1.side, s1.flags, s1.qty_1e6), (1, 0, pos + pos - r));
    fill(&mut b, &s1, 100 * E);
    assert_eq!(b.view().position_size_1e6, -(pos - r));
    // While the TP's cancel is unconfirmed, its quantity still covers the rest: nothing sent.
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty());
    b.on_cancelled(tp.client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    let s2 = out.as_slice()[0];
    assert_eq!((s2.side, s2.qty_1e6), (1, r));
    fill(&mut b, &s2, 100 * E);
    assert_eq!(b.view().position_size_1e6, -pos, "on target");
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty(), "done at its target");
}

#[test]
fn a_reversal_whose_reducer_fills_lands_on_target_and_sends_nothing_more() {
    // Review B2 (§4.12.4 rule 3): the TP fills after the reversal (a late reducer fill crossing
    // zero) — the position is on target and the record is done.
    let mut b = broker(4);
    let mut out = EngineIntentBuf::new();
    let pos = long_at_100(&mut b, &mut out);
    let ex = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(9),
        from_entry: StrKey(1),
        flags: f::LIMIT | f::QTY_PCT,
        limit_1e6: 110 * E,
        qty_pct_1e6: 40 * E,
        ..OrderIntent::default()
    };
    let tp = rest_tp(&mut b, &mut out, ex);
    let short = OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(2),
        side: pine_ir::IntentSide::Short,
        ..OrderIntent::default()
    };
    b.apply_intents(&[short], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    let s1 = out.as_slice()[1];
    fill(&mut b, &s1, 100 * E);
    // The TP fills before its cancel lands, then the cancel confirms.
    b.on_fill(tp.client_oid, -tp.qty_1e6, 110 * E, 0, 1, 0)
        .unwrap();
    b.on_cancelled(tp.client_oid);
    assert_eq!(b.view().position_size_1e6, -pos);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(out.is_empty(), "on target: nothing past it");
}

#[test]
fn a_replacement_acked_after_its_owner_moved_links_the_right_record() {
    // Review B3: between a Modify and its ack, an earlier record dies and is swap-removed, so the
    // requoted record moves index; the ack must link the replacement to it, not to whatever sits
    // at its old index.
    let mut b = broker(6);
    let mut out = EngineIntentBuf::new();
    let seq = |mut it: OrderIntent, n: u32| {
        it.seq = n;
        it
    };
    let three = [
        seq(limit_entry(1, 95 * E), 0),
        seq(limit_entry(2, 94 * E), 1),
        seq(limit_entry(3, 93 * E), 2),
    ];
    b.apply_intents(&three, NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 3);
    let m: Vec<_> = out.as_slice().to_vec();
    let mut k = 0;
    while k < 3 {
        b.on_ack(m[k].client_oid);
        k += 1;
    }
    // Requote id 3 (index 2): a Modify, its replacement awaiting the ack.
    b.apply_intents(&[limit_entry(3, 92 * E)], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    let md = out.as_slice()[0];
    assert_eq!(
        (md.verb, md.prev_client_oid),
        (EngineVerb::Modify, m[2].client_oid)
    );
    // Id 1 (index 0) is cancelled by the script: the swap-remove moves id 3 to index 0.
    let cancel1 = OrderIntent {
        kind: IntentKind::Cancel,
        id: StrKey(1),
        ..OrderIntent::default()
    };
    b.apply_intents(&[cancel1], NOW).unwrap();
    // The replacement's ack arrives now.
    b.on_ack(md.client_oid);
    b.on_cancelled(m[2].client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    // Id 1's maker is taken back; ids 2 and 3 keep theirs untouched (no Cancel, no re-Submit).
    assert_eq!(out.len(), 1);
    let c = out.as_slice()[0];
    assert_eq!(
        (c.verb, c.client_oid),
        (EngineVerb::Cancel, m[0].client_oid)
    );
    b.on_cancelled(m[0].client_oid);
    b.reconcile(&quote(100 * E, 100 * E, NOW), NOW, &mut out)
        .unwrap();
    assert!(
        out.is_empty(),
        "every surviving record is linked to its own maker"
    );
    // A fill of the replacement books id 3's entry.
    b.on_fill(md.client_oid, md.qty_1e6, 92 * E, 0, 1, 0)
        .unwrap();
    assert_eq!(b.open_trade(0).map(|t| t.entry_id), Some(StrKey(3)));
}
