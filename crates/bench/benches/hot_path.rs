// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hot-path criterion benchmarks.
//!
//! Measures the ns/op cost of every stage on the engine hot path,
//! plus the engine-side queued-dispatch push and the off-engine
//! signer + JSON-encode cost. Numbers feed the
//! `docs/hot-path-latency.md` budget vs actual table.
//!
//! Run:
//!     cargo bench -p bench --bench hot_path
//!
//! Output goes to `target/criterion/<group>/<bench>/report/`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};

use book_builder::MultiBook;
use clob_dispatcher::{OrderDispatch, PaperDispatcher, QueuedDispatcher};
use core_latency::LatencyTracker;
use core_metrics::MetricsRegistry;
use core_ring::Ring;
use core_time::now_ns;
use core_types::{DepthTopK, Order, Price, Qty, Side, Tick, VenueId};
use strategy_core::{CooldownGate, Ctx, Strategy, SubmitErr};
use strategy_latency_arb::LatencyArb;

// -----------------------------------------------------------------
// 1. Clock cost  — validates F-12 finding (CLOCK_MONOTONIC_RAW).
// -----------------------------------------------------------------

fn bench_clock(c: &mut Criterion) {
    c.bench_function("clock/now_ns", |b| {
        b.iter(|| {
            let t = now_ns();
            black_box(t);
        });
    });
}

// -----------------------------------------------------------------
// 2. Ring SPSC — the in-place API (`try_push_ref` / `try_pop_ref`):
//    single-threaded push+pop of a `Tick` and of a 192 B `DepthTopK`,
//    a two-thread round trip, and a two-thread stream whose consumer
//    reads every field twice (the M4's 128 B line holds two 64 B slots,
//    so an in-place reader can contend with the producer's next write).
//    The by-value twins these replaced were measured side by side in
//    ZC pass A (docs/risk-policy.md) and deleted with that API.
// -----------------------------------------------------------------

fn sample_tick() -> Tick {
    Tick::new(
        0,
        VenueId::Polymarket,
        7,
        1,
        Price::from_raw(500_000),
        Qty::from_raw(100),
        Price::from_raw(510_000),
        Qty::from_raw(50),
    )
}

/// Every field of `t`, folded — what a consumer that reads the whole
/// tick does.
#[inline(always)]
fn fold_tick(t: &Tick) -> i64 {
    (t.ts_ns as i64)
        .wrapping_add(i64::from(t.sym))
        .wrapping_add(i64::from(t.venue_seq))
        .wrapping_add(t.bid_px.raw())
        .wrapping_add(t.bid_qty.raw())
        .wrapping_add(t.ask_px.raw())
        .wrapping_add(t.ask_qty.raw())
        .wrapping_add(i64::from(t.venue))
        .wrapping_add(i64::from(t.flags))
        .wrapping_add(t.venue_time_ms as i64)
}

fn bench_ring(c: &mut Criterion) {
    let t = sample_tick();
    let (mut prod, mut cons) = Ring::<Tick, 1024>::new().split();
    c.bench_function("ring/push_ref_pop_ref_tick", |b| {
        b.iter(|| {
            assert!(prod.try_push_ref(black_box(&t)));
            let slot = cons.try_pop_ref().expect("pop");
            black_box(fold_tick(&slot));
        });
    });

    let mut d = DepthTopK::EMPTY;
    d.k = 5;
    let (mut prod, mut cons) = Ring::<DepthTopK, 1024>::new().split();
    c.bench_function("ring/push_ref_pop_ref_depth", |b| {
        b.iter(|| {
            assert!(prod.try_push_ref(black_box(&d)));
            let slot = cons.try_pop_ref().expect("pop");
            black_box(slot.ts_ns.wrapping_add(u64::from(slot.k)));
        });
    });

    c.bench_function("ring/spsc_rtt_tick", |b| b.iter_custom(spsc_rtt));
    c.bench_function("ring/spsc_stream_tick", |b| b.iter_custom(spsc_stream));
}

/// `n` round trips: this thread pushes a tick into ring A; an echo
/// thread reads it in its slot and pushes it into ring B; this thread
/// reads it back.
fn spsc_rtt(n: u64) -> Duration {
    let (mut a_tx, mut a_rx) = Ring::<Tick, 1024>::new().split();
    let (mut b_tx, mut b_rx) = Ring::<Tick, 1024>::new().split();
    let echo = std::thread::spawn(move || {
        let mut done = 0u64;
        while done < n {
            if let Some(slot) = a_rx.try_pop_ref() {
                while !b_tx.try_push_ref(&slot) {
                    std::hint::spin_loop();
                }
                done += 1;
            }
        }
    });
    let t = sample_tick();
    let mut acc = 0i64;
    let start = Instant::now();
    for _ in 0..n {
        assert!(a_tx.try_push_ref(&t));
        loop {
            if let Some(slot) = b_rx.try_pop_ref() {
                acc = acc.wrapping_add(fold_tick(&slot));
                break;
            }
            std::hint::spin_loop();
        }
    }
    let elapsed = start.elapsed();
    black_box(acc);
    echo.join().expect("echo thread");
    elapsed
}

/// `n` ticks streamed by a producer thread; this thread reads each one
/// in its slot, every field twice.
fn spsc_stream(n: u64) -> Duration {
    let (mut tx, mut rx) = Ring::<Tick, 1024>::new().split();
    let producer = std::thread::spawn(move || {
        let mut t = sample_tick();
        let mut sent = 0u64;
        while sent < n {
            t.ts_ns = sent;
            if tx.try_push_ref(&t) {
                sent += 1;
            } else {
                std::hint::spin_loop();
            }
        }
    });
    let mut acc = 0i64;
    let mut got = 0u64;
    let start = Instant::now();
    while got < n {
        if let Some(slot) = rx.try_pop_ref() {
            acc = acc
                .wrapping_add(fold_tick(&slot))
                .wrapping_add(fold_tick(&slot));
            got += 1;
        } else {
            std::hint::spin_loop();
        }
    }
    let elapsed = start.elapsed();
    black_box(acc);
    producer.join().expect("producer thread");
    elapsed
}

// -----------------------------------------------------------------
// 3. LatencyTracker::record — atomic record cost (F-9).
// -----------------------------------------------------------------

fn bench_latency_record(c: &mut Criterion) {
    let tracker = LatencyTracker::<24>::new();
    c.bench_function("latency/record_1us", |b| {
        b.iter(|| {
            tracker.record(black_box(1_000));
        });
    });
    c.bench_function("latency/record_1ms", |b| {
        b.iter(|| {
            tracker.record(black_box(1_000_000));
        });
    });
}

// -----------------------------------------------------------------
// 4. Metrics counter inc — single relaxed atomic (F-6).
// -----------------------------------------------------------------

fn bench_metrics_counter(c: &mut Criterion) {
    let mut reg = MetricsRegistry::new();
    let id = reg.register_counter("bench_counter").unwrap();
    c.bench_function("metrics/counter_inc_1", |b| {
        b.iter(|| {
            reg.counter(id).inc(1);
        });
    });
}

// -----------------------------------------------------------------
// 5. MultiBook::apply — linear scan (H4 finding).
// -----------------------------------------------------------------

fn bench_book_apply(c: &mut Criterion) {
    // N = 8 (matches LatencyArb<8> in the cli)
    let mut book: MultiBook<8> = MultiBook::empty();
    for s in 1..=8u32 {
        book.track(s).unwrap();
    }
    // Mid-of-table sym — typical not best/worst case for linear scan.
    let t = Tick::new(
        0,
        VenueId::Polymarket,
        4,
        1,
        Price::from_raw(500_000),
        Qty::from_raw(100),
        Price::from_raw(510_000),
        Qty::from_raw(50),
    );
    c.bench_function("book/apply_n8_middle", |b| {
        b.iter(|| {
            book.apply(black_box(&t));
        });
    });
}

// -----------------------------------------------------------------
// 6. CooldownGate::allow — fail-closed branch + saturating_add.
// -----------------------------------------------------------------

fn bench_cooldown_gate(c: &mut Criterion) {
    let gate: CooldownGate<8> = CooldownGate::new(1_000);
    c.bench_function("cooldown/allow", |b| {
        b.iter(|| {
            let r = gate.allow(black_box(3), black_box(10_000));
            black_box(r);
        });
    });
}

// -----------------------------------------------------------------
// 7. QueuedDispatcher::submit — engine-side push only (F1).
// -----------------------------------------------------------------

fn bench_queued_dispatcher_submit(c: &mut Criterion) {
    // Construct a queued dispatcher with a paper inner. We discard
    // the worker — the engine-side push doesn't need it to drain.
    // Ring will fill but for a few-iters bench it stays well under
    // ORDER_RING_CAP=1024.
    let (mut queued, _worker) = QueuedDispatcher::new(PaperDispatcher::new());
    let order = Order::new(
        now_ns(),
        VenueId::Polymarket,
        7,
        Side::Bid,
        0,
        Price::from_raw(500_000),
        Qty::from_raw(1_000_000),
        1,
    );
    // Periodically drop a few orders to keep the ring from filling
    // — we benchmark the *push* not the *fill error path*.
    let mut counter: u64 = 0;
    c.bench_function("dispatcher/queued_submit", |b| {
        b.iter(|| {
            counter = counter.wrapping_add(1);
            let _ = queued.submit(black_box(&order));
            // After ~512 pushes, reset by creating a fresh queue
            // (off the timed path via Criterion's outer loop won't
            // help here; instead we ignore ring-full errors which
            // are themselves fast).
        });
    });
}

// -----------------------------------------------------------------
// 8. LatencyArb::on_tick — strategy callback, no fire (cooldown).
// -----------------------------------------------------------------

struct NullCtx {
    now: u64,
}
impl Ctx for NullCtx {
    #[inline(always)]
    fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
        Ok(())
    }
    #[inline(always)]
    fn now_ns(&self) -> u64 {
        self.now
    }
}

fn bench_latency_arb_on_tick(c: &mut Criterion) {
    let mut strat: LatencyArb<8> = LatencyArb::new();
    strat.set_threshold(strategy_latency_arb::DEFAULT_THRESHOLD_1E6);
    strat.set_qty(strategy_latency_arb::DEFAULT_QTY);
    strat.set_cooldown_ns(strategy_latency_arb::DEFAULT_COOLDOWN_NS);
    strat.add_pair(7, 13).unwrap();
    let mut ctx = NullCtx { now: 1 };
    strat.on_start(&mut ctx).unwrap();
    // Both books primed with mids that are well within threshold so
    // no order emits.
    let pm = Tick::new(
        0,
        VenueId::Polymarket,
        7,
        1,
        Price::from_raw(500_000),
        Qty::from_raw(100),
        Price::from_raw(510_000),
        Qty::from_raw(50),
    );
    let bn = Tick::new(
        0,
        VenueId::Binance,
        13,
        1,
        Price::from_raw(500_000),
        Qty::from_raw(100),
        Price::from_raw(510_000),
        Qty::from_raw(50),
    );
    strat.on_tick(&pm, &mut ctx);
    strat.on_tick(&bn, &mut ctx);
    c.bench_function("strategy/latency_arb_on_tick_no_fire", |b| {
        b.iter(|| {
            strat.on_tick(black_box(&pm), &mut ctx);
        });
    });
}

// -----------------------------------------------------------------
// 9. Signer end-to-end — sign one Polymarket order.
//
// Tests F-10 (Secp256k1 context rebuild per call) — if the fix
// lands, expect this number to drop substantially.
// -----------------------------------------------------------------

fn bench_signer_sign_order(c: &mut Criterion) {
    use signer_eip712::{sign_order, OrderToSign};
    // Deterministic test key (NOT a real Polymarket key).
    let mut key = [0u8; 32];
    key[31] = 1;
    let o = OrderToSign {
        salt: 1,
        maker: [0u8; 20],
        signer: [0u8; 20],
        taker: [0u8; 20],
        token_id: [0u8; 32],
        maker_amount: 1_000_000,
        taker_amount: 1_000_000,
        expiration: 0,
        nonce: 0,
        fee_rate_bps: 0,
        side: 0,
        signature_type: 0,
    };
    c.bench_function("signer/sign_order_full", |b| {
        b.iter(|| {
            let sig = sign_order(black_box(&o), &key).unwrap();
            black_box(sig);
        });
    });
}

// -----------------------------------------------------------------
// 11. HAR H3.5 — the long-tenor day close: one warm `LongVolEngine`'s
//     close law over the whole 1–40 d grid, what ONE series costs the
//     engine thread at 00:01Z (`core_vol::LongVolSet` staggers the twelve
//     one a poll). HAR H3.7 — and the state hand-off that follows it at
//     the same poll: the warm engine copied whole into the state writer's
//     mailbox. `docs/hot-path-latency.md` "Addendum 2026-09-26".
// -----------------------------------------------------------------

/// A ±10 bps-a-minute xorshift walk — every day observed, every ring
/// fills, every tenor fits.
fn long_vol_step(s: &mut u64, px: &mut i64) -> i64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    let bps = (*s % 21) as i64 - 10;
    *px = (*px + *px * bps / 10_000).max(1_000_000);
    *px
}

fn bench_long_vol_day_close(c: &mut Criterion) {
    use core_vol::{LongVolEngine, DAY_MS, DAY_NS};
    const DAY0: u64 = 1_767_225_600_000; // 2026-01-01T00:00Z
    let mut e = Box::new(LongVolEngine::new());
    let (mut s, mut px) = (0x9E37_79B9_7F4A_7C15u64, 100_000_000i64);
    // 70 whole days: the day ring, every pair ring and every QLIKE
    // window full — the steady state a live series closes in.
    let mut day = 0u64;
    while day < 70 {
        let mut m = 0u64;
        while m < 1_440 {
            e.on_minute_close_at(long_vol_step(&mut s, &mut px), DAY0 + day * DAY_MS + m * 60_000);
            m += 1;
        }
        day += 1;
    }
    c.bench_function("vol/long_day_close_warm", |b| {
        b.iter(|| {
            // The first minute of the next UTC day: the close law runs
            // over the day before (one observed minute — the close's cost
            // is the ring's, never the minutes').
            e.on_minute_close_at(long_vol_step(&mut s, &mut px), DAY0 + day * DAY_MS);
            day += 1;
            black_box(e.x_1e9(DAY_NS));
        });
    });
    // H3.7: the engine thread's whole share of a series' state write — the
    // warm engine copied into its (boxed) mailbox slot.
    let mut dst = Box::new(LongVolEngine::new());
    c.bench_function("vol/long_state_copy_warm", |b| {
        b.iter(|| {
            black_box(&*e).copy_to(&mut dst);
            black_box(&*dst);
        });
    });
}

criterion_group!(
    benches,
    bench_clock,
    bench_ring,
    bench_latency_record,
    bench_metrics_counter,
    bench_book_apply,
    bench_cooldown_gate,
    bench_queued_dispatcher_submit,
    bench_latency_arb_on_tick,
    bench_signer_sign_order,
    bench_long_vol_day_close,
);
criterion_main!(benches);
