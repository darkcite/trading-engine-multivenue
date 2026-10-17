// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.7 `fault.rs` (T15, 8e): D13 — a fault disables exactly one script and flattens it
//! (I32), the retries / `flatten_stuck` / slow phase (I33), `BrokerFault::Internal` panics (I34),
//! and the order-flag ruling on the flatten (pin:08-order-flags-set).

mod common;

use common::*;
use core_config::pine::{PineMode, PineScriptCfg};
use core_types::{Fill, Order, FILL_ORIGIN_PAPER, ORDER_FLAG_POST_ONLY, ORDER_FLAG_REDUCE_ONLY};
use strategy_core::{pine_incident, PineIncident, Strategy, StrategyCounters};
use strategy_pine::oid::encode_client_oid;
use strategy_pine::{PineStrategy, FLATTEN_RETRIES};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn member(scripts: Vec<PineScriptCfg>) -> PineStrategy {
    start(
        boot(scripts),
        vec![
            slice(0, 60, warm_bars(60, 50, 0)),
            slice(1, 60, warm_bars(60, 50, 0)),
        ],
    )
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

fn incidents(p: &PineStrategy) -> Vec<PineIncident> {
    let mut out = vec![PineIncident::default(); 64];
    let n = p.pine_incidents_view(&mut out, 0) as usize;
    out.truncate(n);
    out
}

/// Drive coin 1 identically in every run (the bystander script's whole input).
fn coin1(p: &mut PineStrategy, ctx: &mut RecCtx, t: i64) {
    p.on_venue_event(&print(1, t + 1, 100_000_000, 1_000_000), ctx);
    p.on_tick(&quote(1, t + 2), ctx);
}

/// The scenario: script 0 enters on coin 0, is filled, then faults holding the position; coin 1's
/// script trades through it. Returns the member, its ctx, and script 0's entry order.
fn faulted(first: &str) -> (PineStrategy, RecCtx, Order) {
    let mut p = member(vec![rt(first, 0), rt("rt_entry.pine", 1)]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    coin1(&mut p, &mut ctx, T0 + 20);
    let o = ctx
        .seen()
        .iter()
        .find(|r| r.order.sym == sym(0))
        .expect("entry")
        .order;
    p.on_fill(&fill_of(&o, o.qty.raw(), 1), &mut ctx);
    // The next evaluation (the print's forming-bar update) holds the position: `rt_fault` faults
    // here, and its flatten goes out at the end of the same callback.
    p.on_venue_event(&print(0, T0 + 30, 100_000_000, 1_000_000), &mut ctx);
    coin1(&mut p, &mut ctx, T0 + 40);
    (p, ctx, o)
}

/// I32: the fault disables script 0 only (sticky), flattens it with one reduce-only IoC, pushes
/// one `FAULT` incident; script 1 is bit-identical to a run whose script 0 never faulted.
#[test]
fn fault_disables_one_script_only() {
    let (p, ctx, o) = faulted("rt_fault.pine");
    let pr = p.rt_probe().expect("core");
    let k = p.pine_counters();
    assert_eq!(k.scripts_faulted, 1);
    assert_eq!(k.flattens_started, 1);
    assert!(pr.script_fault[0].0, "script 0 disabled");
    assert!(!pr.script_fault[1].0, "script 1 untouched");
    // The flatten: a reduce-only IoC selling the whole position (pin:08-order-flags-set).
    let flat: Vec<_> = ctx
        .seen()
        .iter()
        .filter(|r| r.order.sym == sym(0) && r.order.is_reduce_only())
        .collect();
    assert_eq!(flat.len(), 1, "one flatten IoC");
    let f = flat[0].order;
    assert_eq!(
        (flat[0].verb, f.side),
        (0, core_types::Side::Ask),
        "a Submit, selling"
    );
    assert_eq!(f.qty.raw(), o.qty.raw());
    assert_eq!(
        f.flags, ORDER_FLAG_REDUCE_ONLY,
        "reduce-only, never post-only"
    );
    assert_eq!(f.kind, core_fill::ORDER_KIND_IOC);
    // Script 1: the same as beside a script 0 that never faults (`rt_entry` on coin 0).
    let (q, qctx, _) = faulted("rt_entry.pine");
    let qr = q.rt_probe().expect("core");
    assert_eq!(pr.script_state[1], qr.script_state[1]);
    assert_eq!(pr.script_run[1], qr.script_run[1]);
    assert_eq!(pr.script_pos[1], qr.script_pos[1]);
    assert_eq!(pr.script_settle[1], qr.script_settle[1]);
    let mine: Vec<_> = ctx
        .seen()
        .iter()
        .filter(|r| r.order.sym == sym(1))
        .collect();
    let theirs: Vec<_> = qctx
        .seen()
        .iter()
        .filter(|r| r.order.sym == sym(1))
        .collect();
    assert_eq!(mine.len(), theirs.len());
    for (a, b) in mine.iter().zip(&theirs) {
        assert_eq!(
            (a.verb, a.order.client_oid, a.order.px, a.order.qty),
            (b.verb, b.order.client_oid, b.order.px, b.order.qty)
        );
    }
}

/// I32: the incident carries the full SHA-256, the bar index and the pc; it is the only one.
#[test]
fn incident_carries_full_sha_bar_and_pc() {
    let (p, _ctx, o) = faulted("rt_fault.pine");
    let inc = incidents(&p);
    assert_eq!(inc.len(), 1);
    assert_eq!(p.pine_counters().incidents, 1);
    let i = inc[0];
    assert_eq!(i.seq, 0);
    assert_eq!(i.event, pine_incident::FAULT);
    assert_eq!(i.source, 0, "the script's own VM");
    assert_eq!(i.script, 0);
    assert_eq!(
        i.sha256,
        script("rt_fault.pine", 0).sha256,
        "the full 32 bytes"
    );
    assert_eq!(i.fault_kind, pine_ir::FaultKind::InsnBudget as u8);
    assert_ne!(i.pc, u32::MAX, "faulted inside the loop: a real pc");
    assert_eq!(i.position_1e6, o.qty.raw());
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.script_fault[0].4, (i.fault_kind, i.pc, i.bar_index));
    assert!(i.bar_index >= 50, "a live bar after the 50 warm bars");
    assert_eq!(i.utc_ms, T0 + 30, "E at the faulting print");
    // `from_seq` past the last: nothing.
    let mut out = [PineIncident::default(); 4];
    assert_eq!(p.pine_incidents_view(&mut out, 1), 0);
}

/// I33: at most `FLATTEN_RETRIES` failed attempts (each IoC judged without a fill and reported),
/// then `flatten_stuck` (a level, an incident), then one attempt per confirmed chart bar; the
/// level drops when the position is flat.
#[test]
fn flatten_retries_then_stuck_then_slow() {
    let (mut p, mut ctx, _o) = faulted("rt_fault.pine");
    let attempts = |ctx: &RecCtx| {
        ctx.seen()
            .iter()
            .filter(|r| r.order.sym == sym(0) && r.order.is_reduce_only())
            .count()
    };
    assert_eq!(attempts(&ctx), 1);
    // Fast phase, inside the first live minute: evidence (a fresh tick), then a settle point
    // (a print) — the IoC is reported without a fill: a failed attempt — and the next is sent.
    let mut t = T0 + 100;
    let mut fails = 1u8;
    while fails < FLATTEN_RETRIES {
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 5, 100_000_000, 1_000_000), &mut ctx);
        t += 1_000;
        fails += 1;
        assert_eq!(
            attempts(&ctx),
            usize::from(fails),
            "one new attempt per failure"
        );
    }
    // The eighth failure: stuck, no new attempt in this bar.
    p.on_tick(&quote(0, t), &mut ctx);
    p.on_venue_event(&print(0, t + 5, 100_000_000, 1_000_000), &mut ctx);
    let k = p.pine_counters();
    assert_eq!(k.flatten_stuck, 1, "the level is up");
    assert_eq!(k.flatten_attempts, u64::from(FLATTEN_RETRIES));
    let pr = p.rt_probe().expect("core");
    assert!(pr.script_fault[0].2, "stuck");
    assert_eq!(pr.script_fault[0].3, FLATTEN_RETRIES);
    let inc = incidents(&p);
    assert_eq!(inc.len(), 2);
    assert_eq!(inc[1].event, pine_incident::FLATTEN_STUCK);
    assert_eq!(inc[1].detail, u32::from(FLATTEN_RETRIES));
    let n = attempts(&ctx);
    assert_eq!(n, usize::from(FLATTEN_RETRIES));
    // Slow phase: more ticks and prints in the same bar send nothing …
    t += 1_000;
    while t < T0 + MIN - 2_000 {
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 5, 100_000_000, 1_000_000), &mut ctx);
        t += 3_000;
    }
    assert_eq!(
        attempts(&ctx),
        n,
        "no attempt before the next confirmed bar"
    );
    // … the next confirmed chart bar owes exactly one.
    p.on_venue_event(&print(0, T0 + MIN + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + MIN + 20), &mut ctx);
    p.on_tick(&quote(0, T0 + MIN + 30), &mut ctx);
    assert_eq!(attempts(&ctx), n + 1, "one attempt per confirmed bar");
    // It fills: flat, the chase ends, the level drops; the script stays disabled.
    let last = ctx
        .seen()
        .iter()
        .rev()
        .find(|r| r.order.is_reduce_only())
        .expect("attempt")
        .order;
    p.on_fill(&fill_of(&last, last.qty.raw(), 1), &mut ctx);
    p.on_tick(&quote(0, T0 + MIN + 40), &mut ctx);
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.script_pos[0], 0);
    assert!(pr.script_fault[0].0, "disabled stays");
    assert!(!pr.script_fault[0].1, "the chase is over");
    assert_eq!(p.pine_counters().flatten_stuck, 0, "the level dropped");
    let before = ctx.n;
    p.on_venue_event(
        &print(0, T0 + 2 * MIN + 10, 100_000_000, 1_000_000),
        &mut ctx,
    );
    p.on_tick(&quote(0, T0 + 2 * MIN + 20), &mut ctx);
    assert_eq!(ctx.n, before, "a disabled, flat script sends nothing");
}

/// I34: `BrokerFault::Internal` fails fast — a fill of this boot's epoch naming an oid the broker
/// never issued (`UnknownFill`).
#[test]
#[should_panic(expected = "broker defect in on_fill")]
fn internal_broker_defect_panics() {
    let mut p = member(vec![rt("rt_entry.pine", 0)]);
    let mut ctx = RecCtx::new();
    let o = Order::new(
        1,
        core_types::VenueId::Hyperliquid,
        sym(0),
        core_types::Side::Bid,
        core_fill::ORDER_KIND_IOC,
        core_types::Price::from_raw(100_000_000),
        core_types::Qty::from_raw(1_000),
        encode_client_oid(1, 0, 999),
    );
    p.on_fill(&fill_of(&o, 1_000, 1), &mut ctx);
}

/// pin:08-no-post-only-paper (risk review): no slot-8 order carries `ORDER_FLAG_POST_ONLY` —
/// makers and their requote Modifies stay on the strict-cross law; an entry IoC is unflagged.
#[test]
fn no_slot8_order_is_post_only() {
    let mut s = rt("rt_maker.pine", 0);
    s.limit_primitive = core_config::pine::PineLimitPrimitive::Maker;
    s.cap_instance_1e6 = 48_000_000;
    let mut p = member(vec![s, rt("rt_entry.pine", 1)]);
    let mut ctx = RecCtx::new();
    let mut t = T0 + 10;
    while t < T0 + 6 * MIN {
        p.on_venue_event(
            &print(0, t, 100_000_000 + (t % 9) * 30_000, 1_000_000),
            &mut ctx,
        );
        p.on_tick(&quote(0, t + 5), &mut ctx);
        coin1(&mut p, &mut ctx, t);
        t += 7_000;
    }
    let (mut makers, mut iocs) = (0, 0);
    for r in ctx.seen() {
        assert_eq!(r.order.flags & ORDER_FLAG_POST_ONLY, 0, "never post-only");
        if r.verb == 2 {
            continue;
        }
        if r.order.sym == sym(0) {
            assert_eq!(r.order.kind, core_fill::ORDER_KIND_MAKER);
            assert_eq!(r.order.flags, 0, "a maker entry / requote is unflagged");
            makers += 1;
        } else {
            assert_eq!(r.order.flags, 0, "an entry IoC is unflagged");
            iocs += 1;
        }
    }
    assert!(makers > 1 && iocs > 0, "makers {makers} iocs {iocs}");
}

/// pin:08-slow-phase-throttles-submits-only (risk review fix (c)): stuck, a resting maker whose
/// cancel the router keeps refusing is re-cancelled as soon as the broker re-emits it
/// (`quote_stale_ns`), without waiting for a bar; the reduce-only Submit still waits for the bar.
#[test]
fn stuck_flatten_recancels_makers_without_waiting_for_a_bar() {
    let mut s = rt("maker_fault.pine", 0);
    s.limit_primitive = core_config::pine::PineLimitPrimitive::Maker;
    s.cap_instance_1e6 = 48_000_000;
    let mut p = member(vec![s]);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let a = ctx
        .seen()
        .iter()
        .find(|r| r.verb == 0 && r.order.kind == core_fill::ORDER_KIND_IOC)
        .expect("market entry")
        .order;
    assert!(
        ctx.seen()
            .iter()
            .any(|r| r.verb == 0 && r.order.kind == core_fill::ORDER_KIND_MAKER),
        "the far limit entry rests"
    );
    p.on_fill(&fill_of(&a, a.qty.raw(), 1), &mut ctx);
    // Every cancel is refused from here on: the maker keeps resting.
    ctx.cancel_res = Err(core_types_submit_refused());
    p.on_venue_event(&print(0, T0 + 30, 100_000_000, 1_000_000), &mut ctx);
    assert!(
        p.rt_probe().expect("core").script_fault[0].0,
        "faulted, flattening"
    );
    let mut t = T0 + 1_000;
    while !p.rt_probe().expect("core").script_fault[0].2 {
        assert!(t < T0 + 40_000, "stuck within the bar");
        p.on_tick(&quote(0, t), &mut ctx);
        p.on_venue_event(&print(0, t + 5, 100_000_000, 1_000_000), &mut ctx);
        t += 1_000;
    }
    let cancels = |c: &RecCtx| c.seen().iter().filter(|r| r.verb == 2).count();
    let iocs = |c: &RecCtx| {
        c.seen()
            .iter()
            .filter(|r| r.verb == 0 && r.order.is_reduce_only())
            .count()
    };
    let (c0, i0) = (cancels(&ctx), iocs(&ctx));
    // Still inside the same chart bar, past the broker's 700 ms re-emit window.
    p.on_tick(&quote(0, t + 900), &mut ctx);
    p.on_tick(&quote(0, t + 1_900), &mut ctx);
    assert!(
        cancels(&ctx) > c0,
        "the maker's cancel is re-emitted without a bar"
    );
    assert_eq!(
        iocs(&ctx),
        i0,
        "no reduce-only Submit before the next confirmed bar"
    );
}

fn core_types_submit_refused() -> strategy_core::SubmitErr {
    strategy_core::SubmitErr::Refused
}

/// I33 (grep): `disabled` is never assigned `false` — the one `disabled: false` is the
/// constructor in `rt.rs`, and no `disabled = false` exists anywhere in the crate.
#[test]
fn disabled_is_never_cleared() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut inits = Vec::new();
    for e in std::fs::read_dir(&src).expect("src") {
        let path = e.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("read");
        for line in text.lines() {
            let l: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                !l.contains("disabled=false") && !l.contains("disabled=!"),
                "{}: `{line}` clears `disabled`",
                path.display()
            );
            if l.starts_with("disabled:false") {
                inits.push(path.file_name().map(|f| f.to_string_lossy().into_owned()));
            }
        }
    }
    assert_eq!(
        inits,
        vec![Some("rt.rs".to_string())],
        "only the constructor"
    );
}

/// I35: a sub-program fault disables EVERY parent that reads the (deduped) instance — one
/// `SUB_FAULT` incident each, `detail` = the instance — and nobody else.
#[test]
fn sub_fault_disables_every_parent() {
    let mut a = script("htf_fault.pine", 0);
    let mut b = script("htf_fault.pine", 0);
    let mut c = rt("rt_entry.pine", 1);
    for s in [&mut a, &mut b, &mut c] {
        s.max_open = 2;
    }
    let mut p = start(
        boot(vec![a, b, c]),
        vec![
            slice(0, 60, warm_bars(60, 50, 0)),
            slice(0, 300, warm_bars(300, 20, 0)),
            slice(1, 60, warm_bars(60, 50, 0)),
        ],
    );
    let pr = p.rt_probe().expect("core");
    assert_eq!(pr.n_subs, 1, "P4: one shared instance");
    let mut ctx = RecCtx::new();
    let mut t = T0 + 10;
    while t < T0 + 7 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, t + 2), &mut ctx);
        coin1(&mut p, &mut ctx, t);
        t += 5_000;
    }
    let pr = p.rt_probe().expect("core");
    assert!(pr.sub_run[0].4, "the instance latched");
    assert!(
        pr.script_fault[0].0 && pr.script_fault[1].0,
        "both parents disabled"
    );
    assert!(!pr.script_fault[2].0, "the bystander is untouched");
    let inc = incidents(&p);
    let subs: Vec<_> = inc
        .iter()
        .filter(|x| x.event == pine_incident::SUB_FAULT)
        .collect();
    assert_eq!(subs.len(), 2, "one per parent");
    assert_eq!((subs[0].script, subs[1].script), (0, 1));
    assert!(subs.iter().all(|x| x.source == 1
        && x.detail == 0
        && x.fault_kind == pine_ir::FaultKind::InsnBudget as u8));
    assert_eq!(p.pine_counters().scripts_faulted, 2);
}
