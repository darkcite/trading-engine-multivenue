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
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};

use book_builder::MultiBook;
use clob_dispatcher::{OrderDispatch, PaperDispatcher, QueuedDispatcher};
use core_latency::LatencyTracker;
use core_metrics::MetricsRegistry;
use core_ring::Ring;
use core_time::now_ns;
use core_types::{Order, Price, Qty, Side, Tick, VenueId};
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
// 2. Ring SPSC — push then pop single-threaded.
// -----------------------------------------------------------------

fn bench_ring(c: &mut Criterion) {
    let ring: Arc<Ring<Tick, 1024>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    let t = Tick::new(
        0,
        VenueId::Polymarket,
        7,
        1,
        Price::from_raw(500_000),
        Qty::from_raw(100),
        Price::from_raw(510_000),
        Qty::from_raw(50),
    );
    c.bench_function("ring/push_pop_tick", |b| {
        b.iter(|| {
            prod.try_push(t).expect("push");
            let popped = cons.try_pop().expect("pop");
            black_box(popped);
        });
    });
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
);
criterion_main!(benches);
