// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.10 R6 (lead ruling 2026-10-17): the delivery journal — one `PineDeliveryRec` at
//! every sequencing-tape point (tick 0, event 1, fill 2), kind 3 for every cancel or withhold,
//! only with `--pine-tape`, no timer records; gapless or counted.

mod common;

use common::*;
use core_config::pine::{PineMode, PineScriptCfg};
use core_types::{Fill, FILL_ORIGIN_PAPER};
use strategy_core::{pine_seq, PineSeqRec, Strategy, StrategyCounters};
use strategy_pine::delivery::{kind, PineDeliveryRec};
use strategy_pine::{PineStrategy, DELIVERY_CAP};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn member(tape: bool) -> PineStrategy {
    let mut b = boot(vec![rt("rt_entry.pine", 0), script("sma_always.pine", 1)]);
    b.tape = tape;
    start(
        b,
        vec![
            slice(0, 60, warm_bars(60, 50, 0)),
            slice(1, 60, warm_bars(60, 50, 0)),
        ],
    )
}

/// The journal's non-done records line up 1:1, in order, with the sequencing tape (same
/// kinds, same payload stamps); kind 2 carries the client oid and the noted fee; kind 3 counts
/// exactly the cancels the member reported; no timer record; nothing dropped.
#[test]
fn journal_follows_the_sequencing_tape() {
    let mut p = member(true);
    let mut ctx = RecCtx::new();
    ctx.clock = Some(7);
    let mut seq = vec![PineSeqRec::default(); 64];
    let mut del = vec![PineDeliveryRec::default(); 64];
    let mut seqs: Vec<PineSeqRec> = Vec::new();
    let mut dels: Vec<PineDeliveryRec> = Vec::new();
    let (mut s_next, mut d_next) = (1u64, 0u64);
    let mut fees: Vec<(u64, i64)> = Vec::new();
    let mut t = T0 + 10;
    let mut seen = 0usize;
    let mut submits = 0usize;
    while t < T0 + 6 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, t + 5), &mut ctx);
        p.on_venue_event(&print(1, t + 6, 50_000_000, 1_000_000), &mut ctx);
        while seen < ctx.n {
            let r = ctx.recs[seen];
            if r.verb == 0 {
                submits += 1;
                // Every other submit fills (with a noted fee); the rest are judged gone and
                // reported cancelled at a later settle point.
                if submits % 2 == 0 {
                    let mut f = Fill::new(
                        r.order.ts_ns + 1_000_000,
                        r.order.sym,
                        r.order.side,
                        r.order.px,
                        r.order.qty,
                        r.order.client_oid,
                    );
                    f.strategy_id = 8;
                    f.origin = FILL_ORIGIN_PAPER;
                    let fee = 1_000_000_000 + i64::try_from(submits).expect("n");
                    p.note_fill_fee(f.order_id, i128::from(fee));
                    fees.push((f.order_id, fee));
                    p.on_fill(&f, &mut ctx);
                }
            }
            seen += 1;
        }
        loop {
            let n = p.pine_seq_view(&mut seq, s_next) as usize;
            for r in &seq[..n] {
                s_next = r.seq + 1;
                seqs.push(*r);
            }
            let (first, m) = p.delivery_view(&mut del, d_next);
            assert_eq!(first, d_next, "gapless journal");
            for r in &del[..m as usize] {
                dels.push(*r);
                d_next += 1;
            }
            if n == 0 && m == 0 {
                break;
            }
        }
        t += 4_000;
    }
    assert_eq!(p.delivery_dropped(), 0);
    let inputs: Vec<&PineDeliveryRec> = dels.iter().filter(|r| r.kind != kind::DONE).collect();
    assert_eq!(
        inputs.len(),
        seqs.len(),
        "one input record per sequencing record"
    );
    let mut fills_seen = 0usize;
    for (d, s) in inputs.iter().zip(seqs.iter()) {
        let want = match s.kind {
            pine_seq::TICK => kind::TICK,
            pine_seq::EVENT => kind::EVENT,
            pine_seq::FILL => kind::FILL,
            other => panic!("sequencing kind {other}"),
        };
        assert_eq!(d.kind, want);
        assert_eq!(d.ts_ns, s.payload[0], "the payload's own ts_ns");
        if d.kind == kind::FILL {
            let (oid, fee) = fees[fills_seen];
            assert_eq!(d.seq, oid, "a fill's client oid");
            assert_eq!(
                d.aux,
                u64::from_ne_bytes(fee.to_ne_bytes()),
                "the noted fee"
            );
            fills_seen += 1;
        }
        if d.kind == kind::EVENT {
            assert_eq!(d.aux, u64::from(core_types::ChannelId::Trade as u8));
        }
    }
    assert!(!fees.is_empty(), "fills were delivered");
    assert_eq!(fills_seen, fees.len());
    let dones: Vec<&PineDeliveryRec> = dels.iter().filter(|r| r.kind == kind::DONE).collect();
    assert!(!dones.is_empty(), "unfilled IoCs are reported cancelled");
    assert_eq!(
        dones.len() as u64,
        p.pine_counters().cancelled_reported,
        "one kind-3 record per on_cancelled"
    );
    for d in &dones {
        // The engine client oid of script 0 (the realtime entry script) or 1.
        assert!(d.seq >> 32 != 0 && d.seq & ((1 << 29) - 1) != 0);
        assert!(d.ts_ns > 0, "E at the report");
    }
    assert!(
        dels.iter().all(|r| r.kind != kind::TIMER),
        "no timer records"
    );
}

/// Without `--pine-tape` the journal is empty (and allocated empty).
#[test]
fn no_tape_no_journal() {
    let mut p = member(false);
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let mut out = [PineDeliveryRec::default(); 4];
    assert_eq!(p.delivery_view(&mut out, 0), (0, 0));
    assert_eq!(p.delivery_dropped(), 0);
    // Before `on_start`, too.
    let shell = PineStrategy::new();
    assert_eq!(shell.delivery_view(&mut out, 5), (5, 0));
}

/// Undrained past the ring, overwritten records are counted and the reader sees exactly that
/// gap in the returned first ordinal.
#[test]
fn journal_overrun_is_counted_and_visible() {
    let mut p = member(true);
    let mut ctx = RecCtx::new();
    ctx.clock = Some(1);
    let n = DELIVERY_CAP as u64 + 100;
    let mut i = 0u64;
    while i < n {
        let tt = T0 + 10 + i64::try_from(i).expect("i") * 3;
        p.on_tick(&quote(1, tt), &mut ctx);
        i += 1;
    }
    let dropped = p.delivery_dropped();
    assert!(dropped >= 100, "overwritten unread records are counted");
    let mut out = vec![PineDeliveryRec::default(); 8];
    let (first, got) = p.delivery_view(&mut out, 0);
    assert!(got > 0);
    assert_eq!(first, dropped, "the reader sees exactly the counted gap");
    let _ = p.pine_counters();
}
