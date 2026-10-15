// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! I3 — `new` allocates exactly once (the closed ring); no other broker call allocates.
//! impl-05 §7.2: this binary installs its own counting `#[global_allocator]` (std only — I2 keeps
//! `pine-broker` free of workspace dev-dependencies) and holds a single test. Counting is
//! per-thread (a const-initialised thread-local, which itself never allocates), armed only around
//! the measured calls on the test thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pine_broker::{EngineIntentBuf, NativeBroker, TvBroker, TvStep};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, IntentKind, LimitPrimitive, NativeCfg, OrderIntent,
    ProgramMeta, Quote, SizingMode, StrKey, StrategyDecl, TvCfg,
};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: every call forwards to `System` unchanged; the counter only reads/writes a
// const-initialised thread-local `Cell`, which neither allocates nor unwinds.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ARMED.try_with(|a| {
            if a.get() {
                let _ = COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract; forwarded as is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout` (forwarded above).
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` with counting armed on this thread; return the allocations it made.
fn count<R>(f: impl FnOnce() -> R) -> (R, u64) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    (r, COUNT.with(Cell::get))
}

const E: i64 = 1_000_000;

fn bar(i: u32, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        open_1e6: o * E,
        high_1e6: h * E,
        low_1e6: l * E,
        close_1e6: c * E,
        open_ms: i64::from(i) * 60_000,
        index: i,
        ..Bar::default()
    }
}

#[test]
fn new_allocates_exactly_once() {
    let meta = ProgramMeta {
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
            commission_value_1e6: 100_000,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    };
    let cfg = TvCfg {
        sizing: SizingMode::AsDeclared,
        day_offset_s: 0,
        _pad: 0,
    };
    let (b, n) = count(|| TvBroker::new(&meta, cfg));
    assert_eq!(n, 1, "new must allocate exactly once (the closed ring)");
    let Ok(mut b) = b else { unreachable!() };
    let entry = OrderIntent {
        kind: IntentKind::Entry,
        id: StrKey(1),
        ..OrderIntent::default()
    };
    let exit = OrderIntent {
        kind: IntentKind::Exit,
        id: StrKey(2),
        from_entry: StrKey(1),
        flags: f::PROFIT | f::LOSS,
        profit_ticks_1e6: 100 * E,
        loss_ticks_1e6: 100 * E,
        seq: 1,
        ..OrderIntent::default()
    };
    let intents = [entry, exit];
    let ((), n) = count(|| {
        let mut i = 0_u32;
        while i < 50 {
            let x = bar(i, 100, 102, 98, 101);
            let mut step = b.open_bar(&x);
            while let TvStep::Evaluate { .. } = step {
                step = match b.submit(&intents) {
                    Ok(s) => s,
                    Err(_) => TvStep::Done,
                };
            }
            let _ = b.view();
            let _ = b.record();
            i += 1;
        }
    });
    assert_eq!(n, 0, "no broker call after new may allocate");
    assert!(b.view().closedtrades > 0);

    // Native: `new` allocates once; marks, commits, reconciles, acks, fills and cancels never.
    let ncfg = NativeCfg {
        unit_notional_1e6: 1_000 * E,
        cap_instance_1e6: 1_000_000 * E,
        cap_day_1e6: 1_000_000_000 * E,
        min_notional_1e6: 0,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: LimitPrimitive::Ioc,
        live: 0,
        _pad: [0; 6],
    };
    let (nb, n) = count(|| NativeBroker::new(&meta, ncfg));
    assert_eq!(n, 1, "NativeBroker::new must allocate exactly once");
    let Ok(mut nb) = nb else { unreachable!() };
    let mut out = EngineIntentBuf::new();
    let ((), n) = count(|| {
        let mut i = 0_u32;
        while i < 50 {
            let now = 10_000_000_000 + u64::from(i) * 1_000_000;
            nb.open_bar(&bar(i, 100, 102, 98, 100));
            let _ = nb.apply_intents(&intents, now);
            let q = Quote {
                bid_1e6: 100 * E,
                ask_1e6: 100 * E,
                bid_qty_1e6: E,
                ask_qty_1e6: E,
                ts_ns: now,
                ..Quote::default()
            };
            let _ = nb.reconcile(&q, now, &mut out);
            let mut k = 0;
            while k < out.len() {
                let e = out.as_slice()[k];
                nb.on_ack(e.client_oid);
                let sq = if e.side == 0 { e.qty_1e6 } else { -e.qty_1e6 };
                let _ = nb.on_fill(e.client_oid, sq / 2, 100 * E, 0, i, 0);
                nb.on_cancelled(e.client_oid);
                k += 1;
            }
            let _ = nb.flatten(&q, now, &mut out);
            let _ = nb.record();
            i += 1;
        }
    });
    assert_eq!(n, 0, "no native broker call after new may allocate");
}
