// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.9 (part): `NativeBroker::reconcile` / `flatten` over the `Ioc` column, driven by
//! hand through the §3.6.2 host protocol.

use pine_broker::{EngineIntentBuf, NativeBroker};
use pine_ir::{
    entry_dir, intent_flag as f, Bar, ChartInfo, EngineVerb, EntryDir, IntentKind, LimitPrimitive,
    NativeCfg, OrderIntent, Primitive, ProgramMeta, Quote, StrKey, StrategyDecl,
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
    broker_dirs(c, &[])
}

/// A broker whose script opens each `(id, dirs)` (amendment A6: `ProgramMeta::entry_dirs`).
fn broker_dirs(c: NativeCfg, dirs: &[(u64, u8)]) -> NativeBroker {
    let mut rows: Vec<EntryDir> = dirs
        .iter()
        .map(|&(id, d)| EntryDir {
            id: StrKey(id),
            dirs: d,
            _pad: [0; 7],
        })
        .collect();
    rows.sort_by_key(|r| r.id.0);
    broker_rows(c, rows)
}

/// A broker given `entry_dirs` exactly as listed (any order, any duplicates).
fn broker_rows(c: NativeCfg, rows: Vec<EntryDir>) -> NativeBroker {
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
        entry_dirs: rows,
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
fn boot_reconciliation_is_pristine_across_open_bar_and_covered_by_a_same_direction_exit() {
    // I34.
    // A6.3: the script opens id 77 long, so its exit reaches a long boot trade.
    let mut b = broker_dirs(cfg(), &[(77, entry_dir::LONG)]);
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
    // An exit naming an id the script opens long covers the long boot trade: its stop fires.
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

/// A close of `id` against a 5-contract long boot trade: what `reconcile` sends.
fn boot_close(dirs: &[(u64, u8)], id: u64) -> Vec<pine_ir::EngineIntent> {
    close_boot(broker_dirs(cfg(), dirs), 5 * E, id)
}

/// A close of `id` against a boot trade of `pos` on broker `b`: what `reconcile` sends.
fn close_boot(mut b: NativeBroker, pos: i64, id: u64) -> Vec<pine_ir::EngineIntent> {
    b.reconcile_boot(pos, 100 * E, 0);
    let close = OrderIntent {
        kind: IntentKind::Close,
        from_entry: StrKey(id),
        ..OrderIntent::default()
    };
    b.apply_intents(&[close], NOW).unwrap();
    let mut out = EngineIntentBuf::new();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    out.as_slice().to_vec()
}

#[test]
fn a_close_of_an_id_opened_in_the_boot_direction_closes_the_boot_trade() {
    // A6.3 `pin:05-boot-close-dir` (operator ruling 2026-10-16).
    let sent = boot_close(&[(42, entry_dir::LONG)], 42);
    assert_eq!(sent.len(), 1);
    let e = sent[0];
    assert_eq!(
        (e.primitive, e.side, e.qty_1e6, e.flags),
        (Primitive::ReduceOnlyIoc, 1, 5 * E, 1)
    );
}

#[test]
fn a_close_of_an_opposite_both_or_absent_id_leaves_the_boot_trade() {
    // A6.3: `strategy.close("Short")` must not close a boot LONG.
    assert!(
        boot_close(&[(42, entry_dir::SHORT)], 42).is_empty(),
        "opposite"
    );
    assert!(
        boot_close(&[(42, entry_dir::LONG | entry_dir::SHORT)], 42).is_empty(),
        "both directions"
    );
    assert!(
        boot_close(&[(41, entry_dir::LONG)], 42).is_empty(),
        "absent id"
    );
    assert!(boot_close(&[], 42).is_empty(), "no rows");
}

#[test]
fn an_exit_from_an_opposite_id_does_not_cover_the_boot_trade_but_close_all_does() {
    // A6.3: `exit(from_entry = id)` follows the same rule; `close_all` and an exit without
    // `from_entry` reach the boot trade as before.
    let mut b = broker_dirs(cfg(), &[(77, entry_dir::SHORT), (78, entry_dir::LONG)]);
    b.reconcile_boot(5 * E, 100 * E, 0);
    let ex = |id: u64, from: u64| OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(id),
        from_entry: StrKey(from),
        flags: f::LOSS,
        loss_ticks_1e6: 100 * E,
        ..OrderIntent::default()
    };
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[ex(9, 77)], NOW).unwrap();
    b.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    assert!(
        out.is_empty(),
        "an exit from a short id never stops a boot long"
    );
    // Without `from_entry` the same stop covers it.
    b.apply_intents(&[ex(10, 0)], NOW).unwrap();
    b.reconcile(&quote(99 * E, 99 * E), NOW, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out.as_slice()[0].id, out.as_slice()[0].qty_1e6),
        (StrKey(10), 5 * E)
    );

    let mut c = broker_dirs(cfg(), &[(77, entry_dir::SHORT)]);
    c.reconcile_boot(5 * E, 100 * E, 0);
    let all = OrderIntent {
        kind: IntentKind::CloseAll,
        ..OrderIntent::default()
    };
    c.apply_intents(&[all], NOW).unwrap();
    c.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        out.as_slice()[0].qty_1e6,
        5 * E,
        "close_all reaches the boot trade"
    );
}

fn row(id: u64, dirs: u8) -> EntryDir {
    EntryDir {
        id: StrKey(id),
        dirs,
        _pad: [0; 7],
    }
}

#[test]
fn entry_dirs_rows_for_one_id_merge_before_the_boot_check() {
    // Lead ruling N2 (`pin:05-entry-dirs-merge`, fail closed): every row of an id is OR-merged
    // before deciding; an id whose merged directions are not exactly one never reaches the boot
    // trade, whatever the rows' order.
    let (l, s) = (entry_dir::LONG, entry_dir::SHORT);
    let mixed = [
        vec![row(42, l), row(42, s)],
        vec![row(42, s), row(42, l)],
        vec![row(42, l), row(7, s), row(42, s)],
        vec![row(42, l), row(42, l | s)],
    ];
    for rows in mixed {
        assert!(
            close_boot(broker_rows(cfg(), rows.clone()), 5 * E, 42).is_empty(),
            "merged both directions must not reach: {rows:?}"
        );
    }
    // A duplicated single direction, unsorted, still reaches it.
    let sent = close_boot(
        broker_rows(cfg(), vec![row(42, l), row(7, s), row(42, l)]),
        5 * E,
        42,
    );
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].side, sent[0].qty_1e6), (1, 5 * E));
}

#[test]
fn the_257th_single_direction_id_never_reaches_the_boot_trade() {
    // `pin:05-entry-dirs-cap`: 256 single-direction ids are retained — the 256 smallest, so the
    // input order does not matter — and any other id never reaches the boot trade (fail closed).
    let sorted: Vec<EntryDir> = (1..=257).map(|id| row(id, entry_dir::LONG)).collect();
    let mut reversed = sorted.clone();
    reversed.reverse();
    for rows in [sorted, reversed] {
        assert!(
            close_boot(broker_rows(cfg(), rows.clone()), 5 * E, 257).is_empty(),
            "the 257th id reaches nothing"
        );
        assert_eq!(
            close_boot(broker_rows(cfg(), rows.clone()), 5 * E, 256).len(),
            1
        );
        assert_eq!(close_boot(broker_rows(cfg(), rows), 5 * E, 1).len(), 1);
    }
    // Ids whose rows merge to both directions take no slot: 256 more single ones all fit.
    let mut rows: Vec<EntryDir> = (1..=300)
        .map(|id| row(id, entry_dir::LONG | entry_dir::SHORT))
        .collect();
    rows.extend((1_000..1_256).map(|id| row(id, entry_dir::LONG)));
    assert_eq!(close_boot(broker_rows(cfg(), rows), 5 * E, 1_255).len(), 1);
}

#[test]
fn a_boot_short_is_closed_and_stopped_only_through_short_opened_ids() {
    // A6.3 mirrored on a short boot position (`pin:05-boot-close-dir`).
    let dirs = [(42, entry_dir::SHORT), (43, entry_dir::LONG)];
    let sent = close_boot(broker_dirs(cfg(), &dirs), -5 * E, 42);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (
            sent[0].primitive,
            sent[0].side,
            sent[0].qty_1e6,
            sent[0].flags
        ),
        (Primitive::ReduceOnlyIoc, 0, 5 * E, 1)
    );
    assert!(
        close_boot(broker_dirs(cfg(), &dirs), -5 * E, 43).is_empty(),
        "a long-opened id never closes a boot short"
    );
    let stop = |from: u64| {
        let mut b = broker_dirs(cfg(), &dirs);
        b.reconcile_boot(-5 * E, 100 * E, 0);
        let ex = OrderIntent {
            kind: IntentKind::Exit,
            id: StrKey(9),
            from_entry: StrKey(from),
            flags: f::LOSS,
            loss_ticks_1e6: 100 * E,
            ..OrderIntent::default()
        };
        b.apply_intents(&[ex], NOW).unwrap();
        let mut out = EngineIntentBuf::new();
        b.reconcile(&quote(101 * E, 101 * E), NOW, &mut out)
            .unwrap();
        out.as_slice().to_vec()
    };
    let s = stop(42);
    assert_eq!(s.len(), 1);
    assert_eq!(
        (s[0].primitive, s[0].side, s[0].qty_1e6, s[0].id),
        (Primitive::ReduceOnlyIoc, 0, 5 * E, StrKey(9))
    );
    assert!(
        stop(43).is_empty(),
        "an exit from a long id never stops a boot short"
    );
}

/// Amendment A9 in native mode: 10 @ 100 ($1 000 base) marked at bar closes 90 and 110 (10 %
/// drawdown and run-up), flattened @ 95 (−$50: −5 %). The aggregates ride `on_fill` and
/// `open_bar`, the book's one transition and its marks.
#[test]
fn a9_percent_aggregates_ride_on_fill_and_open_bar() {
    let mut b = broker(cfg());
    let mut out = EngineIntentBuf::new();
    b.apply_intents(&[entry()], NOW).unwrap();
    b.reconcile(&quote(100 * E, 100 * E), NOW, &mut out)
        .unwrap();
    fill_all(&mut b, &out, 100 * E);
    for (i, c) in [(1, 90), (2, 110)] {
        b.open_bar(&Bar {
            open_1e6: c * E,
            high_1e6: c * E,
            low_1e6: c * E,
            close_1e6: c * E,
            index: i,
            ..Bar::default()
        });
    }
    let v = *b.view();
    assert_eq!(
        (v.max_drawdown_pct_1e6, v.max_runup_pct_1e6),
        (10 * E, 10 * E)
    );
    b.flatten(&quote(95 * E, 95 * E), NOW, &mut out).unwrap();
    let oid = out.as_slice()[0].client_oid;
    b.on_ack(oid);
    b.on_fill(oid, -10 * E, 95 * E, 0, 1, 0).unwrap();
    let v = *b.view();
    assert_eq!((v.closedtrades, v.losstrades), (1, 1));
    assert_eq!(
        (v.sum_trade_pct_1e6, v.sum_win_pct_1e6, v.sum_loss_pct_1e6),
        (-5 * E, 0, -5 * E)
    );
    assert_eq!(v._pad, [0; 1]);
}
