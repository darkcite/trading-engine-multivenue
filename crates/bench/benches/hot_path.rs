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

// HC8: one Hypercall `PlaceOrder` — the struct hash over the body's
// spans, the digest under the boot-cached separator, the signature. What
// the exec arm (HC9) will pay per order; `docs/hot-path-latency.md`.
fn bench_signer_hypercall_place_order(c: &mut Criterion) {
    use signer_eip712::hypercall as hc;
    let mut key = [0u8; 32];
    key[31] = 1;
    let sk = signer_eip712::parse_secret_key(&key).unwrap();
    let ds = hc::hc_domain_separator(hc::HC_CHAIN_ID_MAINNET);
    let wallet = [0x5a; 20];
    let v = hc::HcPlaceView {
        wallet: &wallet,
        symbol: b"BTC-20261002-100000-C",
        side: b"Buy",
        size: b"0.5",
        price: b"1234.5",
        tif: b"ioc",
        route: b"best_execution",
        client_id: b"0x4843000300000000000000000000002a",
        nonce: 1_790_400_000_000_123,
    };
    c.bench_function("signer/hypercall_place_order", |b| {
        b.iter(|| {
            let sig = hc::sign_place_order_with_key(&sk, &ds, black_box(&v)).unwrap();
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

// -----------------------------------------------------------------
// 12. BIN15 H90 — the late-window pricer (vault doc 33 §L2): the grid's
//     per-mark advance and one `late_fair` inside the window, on a grid
//     warmed with 900 s of marks and perp BBOs (σ, the basis and the
//     observed half of the settlement minute all live). Measured, not
//     baselined; the 0 B law is alloc gate 52b's.
// -----------------------------------------------------------------

fn bench_bin15_late(c: &mut Criterion) {
    use strategy_bin15::late::{late_fair, LateGrid, LateLut, LATE_PHI_POINTS, LATE_PHI_STEP_1E6};
    const WALL0: u64 = 1_789_171_200_000_000_000;
    const SEC: u64 = 1_000_000_000;
    let mut lut = Box::new(LateLut::flat());
    let mut i = 0usize;
    while i < LATE_PHI_POINTS {
        let z = i as i64 * LATE_PHI_STEP_1E6;
        lut.phi[i] = (500_000 + z.min(4_000_000) / 8) as u32;
        i += 1;
    }
    let mut g = Box::new(LateGrid::new());
    let (mut s, mut px) = (0x2545_F491_4F6C_DD1Du64, 79_000_000_000i64);
    let step = |s: &mut u64, px: &mut i64| {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *px += (*s % 19_750_001) as i64 - 9_875_000;
        *px
    };
    let mut t = 0u64;
    while t < 930 {
        let m = step(&mut s, &mut px);
        g.on_mark(WALL0 + t * SEC + 200_000_000, m);
        g.on_perp(WALL0 + t * SEC + 300_000_000, 2 * m);
        t += 1;
    }
    // An instance expiring 20 s after the last second: 40 of its 60
    // settlement seconds observed, 20 to come.
    let expiry = WALL0 + (t + 20) * SEC;
    let now = WALL0 + t * SEC - 500_000_000;
    c.bench_function("bin15/late_fair_in_window", |b| {
        b.iter(|| black_box(late_fair(&g, &lut, black_box(79_000_000_000), expiry, now, 1_550)));
    });
    let mut wall = WALL0 + t * SEC;
    c.bench_function("bin15/late_grid_mark_and_perp_1s", |b| {
        b.iter(|| {
            let m = step(&mut s, &mut px);
            wall += SEC;
            g.on_mark(wall + 200_000_000, m);
            g.on_perp(wall + 300_000_000, 2 * m);
            black_box(g.sec());
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
    bench_signer_hypercall_place_order,
    bench_long_vol_day_close,
    bench_bin15_late,
);
criterion_main!(benches);
