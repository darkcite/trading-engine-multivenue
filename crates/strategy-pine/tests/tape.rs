// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.7 `tape.rs` (T18): the one clock read is `PopStamp::take` and it is write-only
//! (I5); the sequencing and decision tapes are gapless, or the gap is counted (I44).

mod common;

use common::*;
use core_config::pine::{PineMode, PineScriptCfg};
use core_types::{Fill, FILL_ORIGIN_PAPER};
use strategy_core::{pine_seq, pine_tape, PineSeqRec, PineTapeRec, Strategy, StrategyCounters};
use strategy_pine::{PineStrategy, SEQ_TAPE_CAP};

fn rt(file: &str, coin: usize) -> PineScriptCfg {
    let mut s = script(file, coin);
    s.mode = PineMode::Realtime;
    s.coalesce_ms = 0;
    s
}

fn taped() -> PineStrategy {
    let mut b = boot(vec![rt("rt_entry.pine", 0), script("sma_always.pine", 1)]);
    b.tape = true;
    start(
        b,
        vec![
            slice(0, 60, warm_bars(60, 50, 0)),
            slice(1, 60, warm_bars(60, 50, 0)),
        ],
    )
}

/// I5 (grep): `now_ns()` appears exactly once in `src/`, in `tape.rs`; no other clock exists.
#[test]
fn one_clock_read_and_it_is_write_only() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut reads = Vec::new();
    for e in std::fs::read_dir(&src).expect("src") {
        let path = e.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&path).expect("read");
        for line in text.lines() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("now_ns()") {
                reads.push(name.clone());
            }
            for bad in ["std::time", "Instant", "SystemTime", "WallAnchor::now"] {
                assert!(!code.contains(bad), "{name}: `{line}` reads a clock");
            }
        }
        if name != "tape.rs" {
            assert!(
                !text.contains(".0 as") && !text.contains("PopStamp("),
                "{name} reads or builds a PopStamp"
            );
        }
    }
    assert_eq!(reads, vec!["tape.rs".to_string()], "exactly one clock read");
}

/// Without `--pine-tape` nothing is recorded and no clock is read (RecCtx without a clock
/// panics on `now_ns`).
#[test]
fn no_tape_no_clock() {
    let mut p = member_untaped();
    let mut ctx = RecCtx::new();
    p.on_venue_event(&print(0, T0 + 10, 100_000_000, 1_000_000), &mut ctx);
    p.on_tick(&quote(0, T0 + 20), &mut ctx);
    let k = p.pine_counters();
    assert_eq!((k.seq_records, k.tape_records), (0, 0));
    let mut out = [PineSeqRec::default(); 4];
    assert_eq!(p.pine_seq_view(&mut out, 0), 0);
}

fn member_untaped() -> PineStrategy {
    start(
        boot(vec![rt("rt_entry.pine", 0)]),
        vec![slice(0, 60, warm_bars(60, 50, 0))],
    )
}

/// pin:08-boot-state-cold-final: `BOOT_STATE.a` is each script's boot outcome — 0 for a script
/// that ends the warm-up walk warm (adopted at its last warm bar), 1 for one that boots cold.
#[test]
fn boot_state_cold_is_the_boot_outcome() {
    let states = |p: &PineStrategy| {
        let mut dec = vec![PineTapeRec::default(); 64];
        let (mut from, mut out) = (0u64, Vec::new());
        loop {
            let m = p.pine_tape_view(&mut dec, from) as usize;
            if m == 0 {
                return out;
            }
            for r in &dec[..m] {
                if r.kind == pine_tape::BOOT_STATE {
                    out.push(r.a);
                }
            }
            from = dec[m - 1].seq + 1;
        }
    };
    assert_eq!(states(&taped()), vec![0, 0], "both warm");
    let mut b = boot(vec![rt("rt_entry.pine", 0), script("sma_always.pine", 1)]);
    b.tape = true;
    assert_eq!(states(&start(b, Vec::new())), vec![1, 1], "both cold");
}

/// I44: drained as it goes, both tapes are gapless and match their counters; every decision
/// names a recorded callback (or the boot); `pop_ns` is the ctx's clock, `e_ns` E after the input.
/// Undrained past the ring, the overwritten records are counted and the reader sees the gap.
#[test]
fn tapes_are_gapless_or_counted() {
    let mut p = taped();
    let mut ctx = RecCtx::new();
    ctx.clock = Some(42);
    let mut seq = vec![PineSeqRec::default(); 64];
    let mut dec = vec![PineTapeRec::default(); 64];
    let (mut s_next, mut d_next) = (1u64, 0u64);
    let mut kinds = [0u32; 16];
    let mut t = T0 + 10;
    let mut filled = 0usize;
    while t < T0 + 5 * MIN {
        p.on_venue_event(&print(0, t, 100_000_000, 1_000_000), &mut ctx);
        p.on_tick(&quote(0, t + 5), &mut ctx);
        p.on_venue_event(&print(1, t + 6, 50_000_000, 1_000_000), &mut ctx);
        while filled < ctx.n {
            let r = ctx.recs[filled];
            if r.verb == 0 {
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
                p.on_fill(&f, &mut ctx);
            }
            filled += 1;
        }
        // Drain both, in small batches.
        loop {
            let n = p.pine_seq_view(&mut seq, s_next) as usize;
            for r in &seq[..n] {
                assert_eq!(r.seq, s_next, "gapless sequencing tape");
                assert_eq!(r.pop_ns, 42, "the pop stamp is the ctx's clock");
                assert!(r.kind >= pine_seq::TICK && r.kind <= pine_seq::FILL);
                assert!(r.e_ns > 0, "E after the input");
                s_next += 1;
            }
            let m = p.pine_tape_view(&mut dec, d_next) as usize;
            for r in &dec[..m] {
                assert_eq!(r.seq, d_next, "gapless decision tape");
                assert!(r.seq_ref < s_next, "a decision names a recorded callback");
                kinds[usize::from(r.kind)] += 1;
                d_next += 1;
            }
            if n == 0 && m == 0 {
                break;
            }
        }
        t += 4_000;
    }
    let k = p.pine_counters();
    assert_eq!((k.seq_records, k.seq_dropped), (s_next - 1, 0));
    assert_eq!((k.tape_records, k.tape_dropped), (d_next, 0));
    assert_eq!(kinds[usize::from(pine_tape::BOOT_HDR)], 1);
    assert_eq!(kinds[usize::from(pine_tape::BOOT_SCRIPT)], 2);
    assert_eq!(
        kinds[usize::from(pine_tape::BOOT_STATE)],
        2,
        "one per committing script"
    );
    for kd in [
        pine_tape::EVAL_BAR,
        pine_tape::EVAL_TICK,
        pine_tape::DISPATCH,
        pine_tape::FILL,
    ] {
        assert!(kinds[usize::from(kd)] > 0, "kind {kd} recorded");
    }
    // Overrun: more callbacks than the sequencing ring holds, nothing drained.
    let from = s_next;
    let n = SEQ_TAPE_CAP as u64 + 100;
    let mut i = 0u64;
    while i < n {
        let tt = t + i64::try_from(i).expect("i") * 3;
        p.on_tick(&quote(1, tt), &mut ctx);
        i += 1;
    }
    let k = p.pine_counters();
    assert!(
        k.seq_dropped >= 100,
        "overwritten unread records are counted"
    );
    let got = p.pine_seq_view(&mut seq, from) as usize;
    assert!(got > 0);
    assert_eq!(
        seq[0].seq - from,
        k.seq_dropped,
        "the reader sees exactly the counted gap"
    );
}
