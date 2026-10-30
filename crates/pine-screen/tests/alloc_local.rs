// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.10 — the bar loop allocates nothing (I16), counted by this binary's own global
//! allocator. ONE `#[test]`: the counter is process-global and a file's tests run in parallel.

mod common;

use common::*;
use pine_ir::{Bar, SizingMode, TradeRec};
use pine_screen::{
    run_group, AccumCfg, Loaded, LoopProbe, TrialAccum, TrialEnd, TrialOutcome, Workspace,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// `System`, counting every allocation and reallocation.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards to `System` with the caller's arguments unchanged; the counters
// are relaxed atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged; the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(n as u64, Ordering::Relaxed);
        // SAFETY: as `alloc`; `p` was returned by this allocator with layout `l`.
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: as `alloc`; `p` was returned by this allocator with layout `l`.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn snap() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}

/// Snapshots the counters at `before_loop` and the delta at `after_loop`; records nothing else.
#[derive(Default)]
struct Guard {
    at: (u64, u64),
    delta: Option<(u64, u64)>,
    evals: u64,
    pubs: u64,
}

impl LoopProbe for Guard {
    fn before_loop(&mut self) {
        self.at = snap();
    }
    fn after_loop(&mut self) {
        let now = snap();
        self.delta = Some((now.0 - self.at.0, now.1 - self.at.1));
    }
    fn on_publish(&mut self, _slot: u32, _htf: &Bar, _chart_row: u32) {
        self.pubs += 1;
    }
    fn on_eval(&mut self, _k: u32, _bar: &Bar, _intrabar: bool, _plots: &[f64]) {
        self.evals += 1;
    }
}

const SMA_CROSS: &str = "//@version=6
strategy(\"sma cross\", overlay=true)
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
";

fn group(trials: &[&Loaded], b: &pine_screen::TrialBars) -> Guard {
    let mut out = vec![TrialOutcome::default(); trials.len()];
    let mut g = Guard::default();
    // 2 000 hourly rows ≈ 83 days: warm-up days 0–1, window [2, 80).
    run_group(
        trials,
        b,
        &cfg(2, 80),
        &mut Workspace::new(),
        &mut out,
        &mut g,
    )
    .expect("runs");
    g
}

#[test]
fn alloc_bar_loop_is_zero() {
    // (a) the accumulator after `reserve`.
    let rows = hourly_rows(&sine_closes(2_000));
    let mut acc = TrialAccum::new();
    acc.reserve(2_000, 90);
    let before = snap();
    acc.begin(&AccumCfg {
        sizing: SizingMode::FixedNotional {
            notional_1e6: 1_000_000_000,
        },
        pointvalue_1e6: 1_000_000,
        from_ms: T0_MS + DAY_MS,
        to_ms: T0_MS + 80 * DAY_MS,
        intrabar_approx: false,
    });
    let mut i = 0;
    while i < rows.len() {
        acc.on_bar(&rows[i], 4_000_000_000_000_000_000_000);
        if i + 1 == 24 {
            acc.baseline(4_000_000_000_000_000_000_000, 0);
        }
        if i % 50 == 49 {
            let t = TradeRec {
                entry_ms: rows[i - 10].close_ms,
                exit_ms: rows[i].close_ms,
                entry_px_1e6: rows[i - 10].close_1e6,
                exit_px_1e6: rows[i].close_1e6,
                qty_1e6: 10_000_000,
                entry_bar: (i - 10) as u32,
                exit_bar: i as u32,
                ..TradeRec::default()
            };
            acc.on_closed_trade(&t);
        }
        i += 1;
    }
    let _ = acc.finish(0, 100, TrialEnd::Completed, 0);
    let after = snap();
    assert_eq!(
        (after.0 - before.0, after.1 - before.1),
        (0, 0),
        "TrialAccum allocated after reserve"
    );

    // (b) run_group over 2 000 hourly bars, K ∈ {1, 4}, and a group with a sub-program.
    let closes = sine_closes(2_000);
    let a = compile(SMA_CROSS).expect("sma");
    let h = compile(&probe_src("htf_close.pine")).expect("htf_close");
    let b = bars_with_4h(&closes, Some((0, 500)));
    let g1 = group(&[&a], &b);
    assert_eq!(g1.delta, Some((0, 0)), "K = 1 bar loop allocated");
    assert!(g1.evals >= 1_900, "K = 1 stepped {} evaluations", g1.evals);
    let g4 = group(&[&a, &a, &a, &a], &b);
    assert_eq!(g4.delta, Some((0, 0)), "K = 4 bar loop allocated");
    let gh = group(&[&a, &h, &a, &h], &b);
    assert_eq!(
        gh.delta,
        Some((0, 0)),
        "the sub-program group's bar loop allocated"
    );
    // One deduplicated instance: the 1 920 stepped rows reach 480 4-h closes.
    assert_eq!(
        gh.pubs, 480,
        "the sub-program was stepped once per 4-h bar ({} publications)",
        gh.pubs
    );
}
