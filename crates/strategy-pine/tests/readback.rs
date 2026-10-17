// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.9's read-backs, adopted by impl-08 §3.7, for the B'3 replay host: R2
//! `broker_counters`; R4 `native_cfg`, `chart`, `closed_trade` / `closed_count`,
//! `broker_record`, `max_insns`, `fault`; R7 `take_bar_marks`; R8 `note_fill_fee`.

mod common;

use common::*;
use core_config::pine::{PineMode, PineScriptCfg};
use core_types::{Fill, Order, FILL_ORIGIN_PAPER};
use pine_ir::{Bar, FaultKind};
use strategy_core::{PineIncident, Strategy, StrategyCounters};
use strategy_pine::{HostKind, PineBootCfg, PineStrategy, BAR_MARKS_CAP};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn warm() -> Vec<strategy_pine::WarmSlice> {
    vec![
        slice(0, 60, warm_bars(60, 50, 0)),
        slice(1, 60, warm_bars(60, 50, 0)),
    ]
}

fn member_with(b: PineBootCfg) -> PineStrategy {
    start(b, warm())
}

fn fill_of(o: &Order, qty_1e6: i64, dt_ms: u64) -> Fill {
    let mut f = Fill::new(
        o.ts_ns + dt_ms * 1_000_000,
        o.sym,
        o.side,
        o.px,
        core_types::Qty::from_raw(qty_1e6),
        o.client_oid,
    );
    f.strategy_id = 8;
    f.origin = FILL_ORIGIN_PAPER;
    f
}

/// The first coin-0 order `ctx` saw at or after record `from`.
fn order_from(ctx: &RecCtx, from: usize) -> Order {
    ctx.seen()[from..]
        .iter()
        .find(|r| r.order.sym == sym(0) && r.verb == 0)
        .expect("an order")
        .order
}

/// Nothing to read before `on_start` (pin:08-readback-after-start), nor past the loaded scripts.
#[test]
fn readback_is_empty_before_start_and_past_the_scripts() {
    let mut p = PineStrategy::new();
    assert!(p.broker_counters(0).is_none());
    assert!(p.native_cfg(0).is_none());
    assert!(p.chart(0).is_none());
    assert!(p.broker_record(0).is_none());
    assert!(p.closed_trade(0, 0).is_none());
    assert!(p.fault(0).is_none());
    assert_eq!((p.closed_count(0), p.max_insns(0)), (0, 0));
    let mut out = [(Bar::default(), 0i128); 4];
    assert_eq!(p.take_bar_marks(0, &mut out), 0);
    p.note_fill_fee(1, 1); // dropped: nothing to book
    p.configure(boot(vec![script("sma_always.pine", 0)]))
        .expect("configure");
    assert!(p.native_cfg(0).is_none(), "configured is not started");
    let q = member_with(boot(vec![script("sma_always.pine", 0)]));
    assert!(q.native_cfg(0).is_some());
    assert!(q.native_cfg(1).is_none());
    assert!(q.broker_counters(1).is_none());
    assert_eq!(q.closed_count(1), 0);
}

/// R2 / R4: the static read-backs are the boot's — `NativeCfg` per §4.1.5 from the manifest and
/// the facts, the chart from the facts; the counters, record and `max_insns` are the broker's
/// and the VM's own.
#[test]
fn static_readback_matches_the_boot() {
    let mut p = member_with(boot(vec![
        rt("rt_entry.pine", 0),
        script("sma_always.pine", 1),
    ]));
    let n = p.native_cfg(0).expect("cfg");
    let f = facts(0);
    assert_eq!(
        (
            n.unit_notional_1e6,
            n.cap_instance_1e6,
            n.cap_day_1e6,
            n.max_open,
            n.cross_ticks
        ),
        (12_000_000, 12_000_000, 48_000_000, 4, 2),
        "the manifest's"
    );
    assert_eq!(
        (n.tick_1e6, n.qty_step_1e6, n.min_notional_1e6),
        (f.mintick_1e6, f.qty_step_1e6, f.min_notional_1e6),
        "the facts'"
    );
    assert_eq!(n.quote_stale_ns, 700_000_000, "HL's default stale window");
    assert_eq!(n.live, 0);
    let c = p.chart(1).expect("chart");
    assert_eq!(c.symbol_hash, pine_ir::symbol_hash(facts(1).key.as_bytes()));
    assert_eq!(
        (c.timeframe_s, c.mintick_1e6, c.pointvalue_1e6),
        (60, 10_000, 1_000_000)
    );
    assert_eq!(p.broker_counters(0).copied(), Some(Default::default()));
    let r0 = p.broker_record(0).expect("record");
    assert_eq!(r0.commission_paid_1e12, 0);
    let warm_insns = p.max_insns(1);
    assert!(warm_insns > 0, "the warm-up evaluated script 1");
    // One live evaluation of script 0: its own VM's high-water mark moves.
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    assert!(p.max_insns(0) > 0);
    assert_eq!(
        p.broker_counters(0).expect("counters").ioc_sent,
        1,
        "the entry IoC, counted by the broker"
    );
}

/// R4 `closed_trade` / `closed_count`: retained indexing, `closedtrades` from the view.
#[test]
fn closed_trades_read_back_in_retained_order() {
    let mut p = member_with(boot(vec![rt("rt_close.pine", 0)]));
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let entry = order_from(&ctx, 0);
    p.on_fill(&fill_of(&entry, entry.qty.raw(), 1), &mut ctx);
    assert_eq!(p.closed_count(0), 0);
    assert!(p.closed_trade(0, 0).is_none());
    // Long now: the next evaluation closes.
    let mark = ctx.n;
    p.on_venue_event(&print(0, T0 + 30, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 40), &mut ctx);
    let exit = order_from(&ctx, mark);
    assert_eq!(exit.side, core_types::Side::Ask);
    p.on_fill(&fill_of(&exit, exit.qty.raw(), 1), &mut ctx);
    assert_eq!(p.closed_count(0), 1);
    let t = p.closed_trade(0, 0).expect("closed trade 0");
    assert_eq!(t.qty_1e6, entry.qty.raw(), "the long that closed");
    assert!(p.closed_trade(0, 1).is_none());
}

/// R4 `fault`: the kind, pc and bar of the incident; `None` for a running script.
#[test]
fn fault_reports_the_disabling_fault() {
    let mut p = member_with(boot(vec![rt("rt_fault.pine", 0), rt("rt_entry.pine", 1)]));
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let o = order_from(&ctx, 0);
    p.on_fill(&fill_of(&o, o.qty.raw(), 1), &mut ctx);
    assert!(p.fault(0).is_none(), "running");
    p.on_venue_event(&print(0, T0 + 30, 100_000_000, 1_000_000), &mut ctx);
    let (k, pc, bar) = p.fault(0).expect("faulted");
    let mut inc = vec![PineIncident::default(); 8];
    let n = p.pine_incidents_view(&mut inc, 0) as usize;
    assert_eq!(n, 1);
    assert_eq!(k, FaultKind::InsnBudget);
    assert_eq!(
        (k as u8, pc, bar),
        (inc[0].fault_kind, inc[0].pc, inc[0].bar_index)
    );
    assert!(p.fault(1).is_none(), "the bystander runs");
}

/// R8: the noted fee books through `NativeBroker::on_fill` (the commission); without it, the
/// model (0 ppm here) books.
#[test]
fn noted_fee_reaches_the_broker() {
    let run = |note: Option<i128>| {
        let mut p = member_with(boot(vec![rt("rt_entry.pine", 0)]));
        let mut ctx = RecCtx::new();
        p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, T0 + 20), &mut ctx);
        let o = order_from(&ctx, 0);
        if let Some(fee) = note {
            p.note_fill_fee(o.client_oid, fee);
        }
        p.on_fill(&fill_of(&o, o.qty.raw(), 1), &mut ctx);
        assert_eq!(p.pine_counters().fills, 1);
        p.broker_record(0).expect("record").commission_paid_1e12
    };
    assert_eq!(run(None), 0, "the model at 0 ppm");
    assert_eq!(run(Some(5_364_162_000)), 5_364_162_000, "the host's fee");
}

/// pin:08-noted-fee-non-negative: a negative noted fee fails fast in every mode (`assert!`),
/// never clamped to the model.
#[test]
#[should_panic(expected = "pin:08-noted-fee-non-negative")]
fn noted_fee_negative_fails_fast() {
    let mut p = member_with(boot(vec![rt("rt_entry.pine", 0)]));
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let o = order_from(&ctx, 0);
    p.note_fill_fee(o.client_oid, -1);
    p.on_fill(&fill_of(&o, o.qty.raw(), 1), &mut ctx);
}

/// pin:08-noted-fee-one-fill: a note naming another oid is a host defect — `debug_assert!` in
/// debug, counted on the cold `noted_fee_mismatched` in release (the model books).
#[test]
fn noted_fee_mismatch_is_counted() {
    let mut p = member_with(boot(vec![rt("rt_entry.pine", 0)]));
    let mut ctx = RecCtx::new();
    assert_eq!(p.noted_fee_mismatched(), 0);
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let o = order_from(&ctx, 0);
    p.note_fill_fee(o.client_oid ^ 1, 7);
    let f = fill_of(&o, o.qty.raw(), 1);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.on_fill(&f, &mut ctx)));
    assert_eq!(r.is_err(), cfg!(debug_assertions), "a defect in debug only");
    assert_eq!(p.noted_fee_mismatched(), 1);
    if !cfg!(debug_assertions) {
        assert_eq!(p.pine_counters().fills, 1);
        let rec = p.broker_record(0).expect("record");
        assert_eq!(
            rec.commission_paid_1e12, 0,
            "the model at 0 ppm, not the note"
        );
    }
}

fn replay_member() -> PineStrategy {
    let mut b = boot(vec![script("sma_always.pine", 0)]);
    b.host = HostKind::Replay;
    member_with(b)
}

/// R7: one mark per confirmed chart bar, oldest first, equity as the view holds it; drained by
/// the call; a short buffer leaves the rest in order. The engine host records none.
#[test]
fn bar_marks_drain_in_order_on_the_replay_host() {
    let mut p = replay_member();
    let mut out = [(Bar::default(), 0i128); 4];
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut NoCtx);
    assert_eq!(p.take_bar_marks(0, &mut out), 0, "the boot bar still forms");
    p.on_venue_event(&print(0, T0 + 70_000, 101_000_000, 1_000_000), &mut NoCtx);
    assert_eq!(p.take_bar_marks(0, &mut out), 1);
    assert_eq!((out[0].0.open_ms, out[0].0.close_1e6), (T0, 100_000_000));
    let eq = p.broker_record(0).expect("record").final_equity_1e12;
    assert_eq!(out[0].1, eq, "the view's equity after the evaluation");
    assert_eq!(p.take_bar_marks(0, &mut out), 0, "drained");
    // Two confirms, a one-slot buffer: oldest first, the rest kept.
    p.on_venue_event(&print(0, T0 + 130_000, 102_000_000, 1_000_000), &mut NoCtx);
    p.on_venue_event(&print(0, T0 + 190_000, 103_000_000, 1_000_000), &mut NoCtx);
    let mut one = [(Bar::default(), 0i128); 1];
    assert_eq!(p.take_bar_marks(0, &mut one), 1);
    assert_eq!(one[0].0.open_ms, T0 + 60_000);
    assert_eq!(p.take_bar_marks(0, &mut one), 1);
    assert_eq!(one[0].0.open_ms, T0 + 120_000);
    assert_eq!(p.take_bar_marks(0, &mut one), 0);
    assert_eq!(p.take_bar_marks(1, &mut one), 0, "past the scripts");
    // The engine host: no ring, no marks (pin:08-bar-marks-replay-only).
    let mut q = member_with(boot(vec![script("sma_always.pine", 0)]));
    q.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut NoCtx);
    q.on_venue_event(&print(0, T0 + 70_000, 101_000_000, 1_000_000), &mut NoCtx);
    assert_eq!(q.take_bar_marks(0, &mut out), 0);
}

/// pin:08-bar-marks-undrained-panics: a host that never drains fails fast.
#[test]
#[should_panic(expected = "bar marks not drained")]
fn undrained_bar_marks_fail_fast() {
    let mut p = replay_member();
    let mut t = T0 + 10;
    for _ in 0..=BAR_MARKS_CAP + 1 {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut NoCtx);
        t += 60_000;
    }
}
