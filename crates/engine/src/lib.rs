//! # engine
//!
//! `Engine<S: Strategy, D: OrderDispatch>`. The single-writer,
//! compile-time-monomorphized main loop. Drains the tick / signal /
//! fill rings in priority order and dispatches to the strategy's
//! callbacks.
//!
//! Phase 2 expands the tick path: **two** tick consumers
//! (Polymarket + Binance) instead of one. The strategy sees a
//! merged stream and routes internally by `tick.sym`. Sizes match
//! the per-ingress crate's `DEFAULT_TICK_RING_CAP` so the engine
//! never reallocates and the consumer-end type is fixed.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use clob_dispatcher::OrderDispatch;
use core_latency::LatencyTracker;
use core_ring::Consumer;
use core_time::{now_ns, NsTs};
use core_types::{Fill, Order, Signal, Tick};
use strategy_core::{Ctx, Strategy, StrategyError, SubmitErr};

// ---------------------------------------------------------------
// Compile-time ring sizes
// ---------------------------------------------------------------

/// Polymarket tick ring capacity. Matches
/// `ingress_polymarket::run_loop::DEFAULT_TICK_RING_CAP` so the
/// consumer type lines up without a re-allocate.
pub const PM_TICK_RING_SIZE: usize = 16_384;
/// Binance tick ring capacity. Matches
/// `ingress_binance::run_loop::DEFAULT_TICK_RING_CAP`.
pub const BN_TICK_RING_SIZE: usize = 8_192;
/// Signal ring capacity. Matches
/// `ingress_rpc::run_loop::DEFAULT_SIGNAL_RING_CAP` so the
/// consumer type lines up without a re-allocate.
pub const SIGNAL_RING_SIZE: usize = 1_024;
/// Fill ring capacity.
pub const FILL_RING_SIZE: usize = 1_024;

/// Legacy alias kept for callers that still want a single
/// `TICK_RING_SIZE`. New code should pick `PM_TICK_RING_SIZE`
/// or `BN_TICK_RING_SIZE` explicitly.
pub const TICK_RING_SIZE: usize = PM_TICK_RING_SIZE;

// ---------------------------------------------------------------
// Engine
// ---------------------------------------------------------------

/// The engine. Generic over the strategy `S` and the dispatcher `D`
/// so both are monomorphized at compile time.
pub struct Engine<S: Strategy, D: OrderDispatch> {
    strat: S,
    disp: D,
    pm_tick_cons: Consumer<Tick, PM_TICK_RING_SIZE>,
    bn_tick_cons: Consumer<Tick, BN_TICK_RING_SIZE>,
    sig_cons: Consumer<Signal, SIGNAL_RING_SIZE>,
    fill_cons: Consumer<Fill, FILL_RING_SIZE>,
    last_timer_ns: NsTs,
    /// Number of iterations completed (wraps on u64; for paper-mode stats).
    pub iterations: u64,
    /// Cumulative ticks dispatched (Polymarket + Binance combined).
    pub ticks_dispatched: u64,
    /// Cumulative signals dispatched.
    pub signals_dispatched: u64,
    /// Cumulative fills dispatched.
    pub fills_dispatched: u64,

    // ---- Per-stage latency trackers (lock-free) ----
    //
    // `ingest_to_strategy` is recorded on every tick: the gap
    // between the producer's timestamp inside `Tick.ts_ns` and the
    // engine's `now_ns()` when the strategy callback is about to
    // fire. Captures ring + drain latency.
    //
    // `strategy_to_submit` is recorded inside `EngineCtx::submit`
    // — gap between the order's `ts_ns` (strategy stamped it when
    // it decided) and `now_ns()` at the dispatcher boundary.
    //
    // `submit_to_ack` is reserved for Phase 7 when fills land via
    // the Polymarket WS order channel.
    ingest_lat: LatencyTracker<24>,
    decide_lat: LatencyTracker<24>,
    ack_lat: LatencyTracker<24>,

    /// Per-bucket last-tick timestamps. Each bucket holds the
    /// `now_ns()` of the most recent tick whose `tick.sym % SYMS`
    /// hashed to it. Stale buckets surface as a high
    /// `max_tick_age_ns()` — useful for detecting a silenced
    /// market mid-soak.
    ///
    /// 64 buckets covers any realistic symbol set with low
    /// collision; collisions just mean two sym hash to the same
    /// bucket and refresh each other, which is conservative (no
    /// false-positive staleness).
    last_tick_ns_per_sym: [u64; SYM_BUCKETS],
    /// How many distinct buckets have ever observed a tick.
    /// Lets `max_tick_age_ns` skip uninitialized slots.
    sym_populated: u64,
}

/// Number of per-symbol last-tick buckets. Power-of-two so the
/// modulo collapses to a bitmask.
pub const SYM_BUCKETS: usize = 64;

impl<S: Strategy, D: OrderDispatch> Engine<S, D> {
    /// Construct an engine that owns the strategy, dispatcher, and
    /// four consumer handles for the shared rings.
    pub fn new(
        strat: S,
        disp: D,
        pm_tick_cons: Consumer<Tick, PM_TICK_RING_SIZE>,
        bn_tick_cons: Consumer<Tick, BN_TICK_RING_SIZE>,
        sig_cons: Consumer<Signal, SIGNAL_RING_SIZE>,
        fill_cons: Consumer<Fill, FILL_RING_SIZE>,
    ) -> Self {
        Self {
            strat,
            disp,
            pm_tick_cons,
            bn_tick_cons,
            sig_cons,
            fill_cons,
            last_timer_ns: 0,
            iterations: 0,
            ticks_dispatched: 0,
            signals_dispatched: 0,
            fills_dispatched: 0,
            ingest_lat: LatencyTracker::new(),
            decide_lat: LatencyTracker::new(),
            ack_lat: LatencyTracker::new(),
            last_tick_ns_per_sym: [0u64; SYM_BUCKETS],
            sym_populated: 0,
        }
    }

    /// Call `on_start` on the owned strategy.
    pub fn start(&mut self) -> Result<(), StrategyError> {
        let mut ctx = EngineCtx {
            disp: &mut self.disp,
            decide_lat: &self.decide_lat,
            now: now_ns(),
        };
        self.strat.on_start(&mut ctx)
    }

    /// Drain each ring once (up to `max_per_ring` items per ring).
    /// This is the single-writer hot path body — real run loop
    /// calls this in a tight `loop { ... }`.
    ///
    /// Tick rings are drained in **Polymarket then Binance** order
    /// so the strategy sees the book update before the cross-venue
    /// trigger for any given iteration. Within one Phase 2 tick
    /// either ring can dominate; size caps prevent starvation.
    ///
    /// **Latency recording.** Every drained tick samples the
    /// `now - tick.ts_ns` gap into the `ingest_lat` tracker. Every
    /// `ctx.submit` samples the `now - order.ts_ns` gap into the
    /// `decide_lat` tracker via [`EngineCtx::submit`]. Both record
    /// paths are zero-alloc.
    #[inline]
    pub fn tick(&mut self, max_per_ring: usize) {
        self.iterations = self.iterations.wrapping_add(1);

        // --- Polymarket ticks ---
        // E-2: per-item clock sample. The old code captured `now`
        // once at the top of `tick()` so a burst of 64 ticks
        // measured ingest-latency against the *first* tick's
        // observed-at time, biasing every record after the first
        // toward zero. Now each pop re-samples — costs ~14 ns/tick
        // but the latency histogram tells the truth.
        let mut i = 0;
        while i < max_per_ring {
            match self.pm_tick_cons.try_pop() {
                Some(t) => {
                    let now = now_ns();
                    self.ingest_lat.record(now.saturating_sub(t.ts_ns));
                    self.touch_sym_bucket(t.sym, now);
                    let mut ctx = EngineCtx {
                        disp: &mut self.disp,
                        decide_lat: &self.decide_lat,
                        now,
                    };
                    self.strat.on_tick(&t, &mut ctx);
                    self.ticks_dispatched = self.ticks_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- Binance ticks ---
        let mut i = 0;
        while i < max_per_ring {
            match self.bn_tick_cons.try_pop() {
                Some(t) => {
                    let now = now_ns();
                    self.ingest_lat.record(now.saturating_sub(t.ts_ns));
                    self.touch_sym_bucket(t.sym, now);
                    let mut ctx = EngineCtx {
                        disp: &mut self.disp,
                        decide_lat: &self.decide_lat,
                        now,
                    };
                    self.strat.on_tick(&t, &mut ctx);
                    self.ticks_dispatched = self.ticks_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- signals ---
        let mut i = 0;
        while i < max_per_ring {
            match self.sig_cons.try_pop() {
                Some(s) => {
                    let now = now_ns();
                    self.ingest_lat.record(now.saturating_sub(s.ts_ns));
                    let mut ctx = EngineCtx {
                        disp: &mut self.disp,
                        decide_lat: &self.decide_lat,
                        now,
                    };
                    self.strat.on_signal(&s, &mut ctx);
                    self.signals_dispatched = self.signals_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- fills ---
        let mut i = 0;
        while i < max_per_ring {
            match self.fill_cons.try_pop() {
                Some(f) => {
                    let now = now_ns();
                    self.ack_lat.record(now.saturating_sub(f.ts_ns));
                    let mut ctx = EngineCtx {
                        disp: &mut self.disp,
                        decide_lat: &self.decide_lat,
                        now,
                    };
                    self.strat.on_fill(&f, &mut ctx);
                    self.fills_dispatched = self.fills_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- timer ---
        let period = self.strat.timer_period_ns();
        if period != u64::MAX {
            let now = now_ns();
            if now.saturating_sub(self.last_timer_ns) >= period {
                self.last_timer_ns = now;
                let mut ctx = EngineCtx {
                    disp: &mut self.disp,
                    decide_lat: &self.decide_lat,
                    now,
                };
                self.strat.on_timer(now, &mut ctx);
            }
        }
    }

    /// p50 ingest→strategy latency (ns). 0 if no samples.
    #[inline]
    pub fn ingest_p50_ns(&self) -> u64 {
        self.ingest_lat.percentile(0.50)
    }
    /// p99 ingest→strategy latency.
    #[inline]
    pub fn ingest_p99_ns(&self) -> u64 {
        self.ingest_lat.percentile(0.99)
    }
    /// p50 strategy→submit latency.
    #[inline]
    pub fn decide_p50_ns(&self) -> u64 {
        self.decide_lat.percentile(0.50)
    }
    /// p99 strategy→submit latency.
    #[inline]
    pub fn decide_p99_ns(&self) -> u64 {
        self.decide_lat.percentile(0.99)
    }
    /// p50 submit→ack latency (populated only when fill ring sees
    /// data — Phase 7).
    #[inline]
    pub fn ack_p50_ns(&self) -> u64 {
        self.ack_lat.percentile(0.50)
    }
    /// p99 submit→ack latency.
    #[inline]
    pub fn ack_p99_ns(&self) -> u64 {
        self.ack_lat.percentile(0.99)
    }

    /// Touch the per-symbol last-tick bucket. Zero-alloc.
    #[inline(always)]
    fn touch_sym_bucket(&mut self, sym: core_types::SymbolId, now: NsTs) {
        let bucket = (sym as usize) & (SYM_BUCKETS - 1);
        // Bit `bucket` flagged in `sym_populated` so
        // `max_tick_age_ns` only inspects buckets we've actually
        // touched.
        self.sym_populated |= 1u64 << bucket;
        self.last_tick_ns_per_sym[bucket] = now;
    }

    /// Maximum tick age across every populated symbol bucket, in
    /// nanoseconds. Returns 0 if no symbols have been touched.
    ///
    /// Useful for staleness detection — pair with a `tracing::warn!`
    /// once it crosses a configured threshold (e.g. 30 s).
    pub fn max_tick_age_ns(&self, now: NsTs) -> u64 {
        if self.sym_populated == 0 {
            return 0;
        }
        let mut max_age: u64 = 0;
        let mut mask = self.sym_populated;
        while mask != 0 {
            // Index of the lowest set bit.
            let bucket = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let last = self.last_tick_ns_per_sym[bucket];
            let age = now.saturating_sub(last);
            if age > max_age {
                max_age = age;
            }
        }
        max_age
    }

    /// Number of distinct populated symbol buckets. Useful for
    /// asserting that the engine actually saw multiple symbols.
    #[inline]
    pub fn populated_sym_count(&self) -> u32 {
        self.sym_populated.count_ones()
    }

    /// Tick age for a specific bucket, in nanoseconds. Returns 0
    /// when the bucket has never been touched. Used by the cli to
    /// publish one Prometheus gauge per bucket — operators can pick
    /// out which specific symbol slot went silent instead of only
    /// seeing the across-buckets max.
    #[inline]
    pub fn tick_age_ns_bucket(&self, bucket: usize, now: NsTs) -> u64 {
        if bucket >= SYM_BUCKETS {
            return 0;
        }
        if self.sym_populated & (1u64 << bucket) == 0 {
            return 0;
        }
        now.saturating_sub(self.last_tick_ns_per_sym[bucket])
    }

    /// Bitmask of which buckets have ever seen a tick. The cli
    /// reads this to skip empty buckets when publishing per-bucket
    /// gauges.
    #[inline]
    pub fn populated_sym_mask(&self) -> u64 {
        self.sym_populated
    }

    /// Write all three LatencyTracker histograms to `out` as
    /// human-readable text. Used by the cli's periodic dump.
    /// Allocation is OK here — this is not on the hot path.
    pub fn write_latency_hgrm(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        self.ingest_lat
            .write_hgrm(out, "engine.ingest_to_strategy")?;
        self.decide_lat
            .write_hgrm(out, "engine.strategy_to_submit")?;
        self.ack_lat.write_hgrm(out, "engine.submit_to_ack")?;
        Ok(())
    }

    /// Call `on_stop` on the owned strategy.
    pub fn stop(&mut self) {
        let mut ctx = EngineCtx {
            disp: &mut self.disp,
            decide_lat: &self.decide_lat,
            now: now_ns(),
        };
        self.strat.on_stop(&mut ctx);
    }

    /// Borrow the dispatcher (for paper-mode stats reads).
    #[inline]
    pub fn dispatcher(&self) -> &D {
        &self.disp
    }

    /// Borrow the strategy (for paper-mode stats reads).
    #[inline]
    pub fn strategy(&self) -> &S {
        &self.strat
    }
}

/// Concrete `Ctx` passed into strategy callbacks.
struct EngineCtx<'a, D: OrderDispatch> {
    disp: &'a mut D,
    decide_lat: &'a LatencyTracker<24>,
    now: NsTs,
}

impl<'a, D: OrderDispatch> Ctx for EngineCtx<'a, D> {
    #[inline(always)]
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        // Strategy stamps `order.ts_ns` from `ctx.now_ns()` so the
        // gap here equals the time spent inside the strategy
        // callback. Atomic record — never blocks.
        self.decide_lat
            .record(now_ns().saturating_sub(order.ts_ns));
        self.disp.submit(&order).map_err(|_| SubmitErr::RingFull)
    }
    #[inline(always)]
    fn now_ns(&self) -> NsTs {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clob_dispatcher::PaperDispatcher;
    use core_ring::Ring;
    use core_types::{Price, Qty, Side};

    struct Counter {
        ticks: u32,
        signals: u32,
        fills: u32,
    }

    impl strategy_core::StrategyCounters for Counter {}

    impl Strategy for Counter {
        fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
            Ok(())
        }
        fn on_tick<C: Ctx>(&mut self, _t: &Tick, _ctx: &mut C) {
            self.ticks += 1;
        }
        fn on_signal<C: Ctx>(&mut self, _s: &Signal, _ctx: &mut C) {
            self.signals += 1;
        }
        fn on_fill<C: Ctx>(&mut self, _f: &Fill, _ctx: &mut C) {
            self.fills += 1;
        }
        fn on_timer<C: Ctx>(&mut self, _n: NsTs, _ctx: &mut C) {}
        fn timer_period_ns(&self) -> u64 {
            u64::MAX
        }
        fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
    }

    fn build_engine() -> (
        Engine<Counter, PaperDispatcher>,
        core_ring::Producer<Tick, PM_TICK_RING_SIZE>,
        core_ring::Producer<Tick, BN_TICK_RING_SIZE>,
        core_ring::Producer<Signal, SIGNAL_RING_SIZE>,
        core_ring::Producer<Fill, FILL_RING_SIZE>,
    ) {
        let pm_ring: std::sync::Arc<Ring<Tick, PM_TICK_RING_SIZE>> = Ring::new();
        let bn_ring: std::sync::Arc<Ring<Tick, BN_TICK_RING_SIZE>> = Ring::new();
        let sig_ring: std::sync::Arc<Ring<Signal, SIGNAL_RING_SIZE>> = Ring::new();
        let fill_ring: std::sync::Arc<Ring<Fill, FILL_RING_SIZE>> = Ring::new();

        let (pm_p, pm_c) = pm_ring.split();
        let (bn_p, bn_c) = bn_ring.split();
        let (sp, sc) = sig_ring.split();
        let (fp, fc) = fill_ring.split();

        let strat = Counter {
            ticks: 0,
            signals: 0,
            fills: 0,
        };
        let disp = PaperDispatcher::new();
        let eng = Engine::new(strat, disp, pm_c, bn_c, sc, fc);
        (eng, pm_p, bn_p, sp, fp)
    }

    #[test]
    fn engine_drains_polymarket_tick_ring() {
        let (mut eng, mut pm_p, _bn_p, _sp, _fp) = build_engine();
        eng.start().unwrap();
        for i in 0..3u32 {
            pm_p.try_push(Tick::new(
                0,
                1,
                i + 1,
                Price::from_raw(0),
                Qty::from_raw(0),
                Price::from_raw(0),
                Qty::from_raw(0),
            ))
            .unwrap();
        }
        eng.tick(16);
        assert_eq!(eng.iterations, 1);
        assert_eq!(eng.ticks_dispatched, 3);
    }

    #[test]
    fn engine_drains_both_tick_rings_per_iteration() {
        let (mut eng, mut pm_p, mut bn_p, _sp, _fp) = build_engine();
        eng.start().unwrap();
        for i in 0..2u32 {
            pm_p.try_push(Tick::new(
                0,
                1,
                i + 1,
                Price::from_raw(0),
                Qty::from_raw(0),
                Price::from_raw(0),
                Qty::from_raw(0),
            ))
            .unwrap();
            bn_p.try_push(Tick::new(
                0,
                2,
                i + 1,
                Price::from_raw(0),
                Qty::from_raw(0),
                Price::from_raw(0),
                Qty::from_raw(0),
            ))
            .unwrap();
        }
        eng.tick(16);
        assert_eq!(eng.ticks_dispatched, 4);
    }

    #[test]
    fn engine_respects_max_per_ring_cap() {
        let (mut eng, mut pm_p, _bn_p, _sp, _fp) = build_engine();
        eng.start().unwrap();
        for i in 0..10u32 {
            pm_p.try_push(Tick::new(
                0,
                1,
                i + 1,
                Price::from_raw(0),
                Qty::from_raw(0),
                Price::from_raw(0),
                Qty::from_raw(0),
            ))
            .unwrap();
        }
        eng.tick(3);
        // Only 3 drained on this iteration; the rest stay in the ring.
        assert_eq!(eng.ticks_dispatched, 3);
        eng.tick(16);
        assert_eq!(eng.ticks_dispatched, 10);
    }

    #[test]
    fn engine_dispatcher_starts_with_zero_accepted() {
        let (eng, _pm_p, _bn_p, _sp, _fp) = build_engine();
        assert_eq!(eng.dispatcher().stats().accepted, 0);
    }

    #[test]
    fn max_tick_age_zero_before_any_tick() {
        let (eng, _pm_p, _bn_p, _sp, _fp) = build_engine();
        assert_eq!(eng.populated_sym_count(), 0);
        assert_eq!(eng.max_tick_age_ns(1_000_000), 0);
    }

    #[test]
    fn max_tick_age_tracks_freshest_per_bucket() {
        let (mut eng, mut pm_p, _bn_p, _sp, _fp) = build_engine();
        eng.start().unwrap();
        pm_p.try_push(Tick::new(
            1_000,
            7,
            1,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        ))
        .unwrap();
        pm_p.try_push(Tick::new(
            2_000,
            11,
            1,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        ))
        .unwrap();
        eng.tick(16);
        // Two distinct symbols → two populated buckets (no hash
        // collision at 7 % 64 vs 11 % 64).
        assert_eq!(eng.populated_sym_count(), 2);
        // Probe age with `now` set well past any plausible
        // wallclock — the result must be strictly positive when at
        // least one symbol has been recorded.
        let far_future = core_time::now_ns().saturating_add(1_000_000_000);
        let age = eng.max_tick_age_ns(far_future);
        assert!(age > 0, "expected non-zero age, got {age}");
        // And `populated_sym_count` should fit inside the bucket
        // bitmask.
        assert!(eng.populated_sym_count() <= SYM_BUCKETS as u32);
    }

    #[test]
    fn max_tick_age_handles_now_before_recorded() {
        let (mut eng, mut pm_p, _bn_p, _sp, _fp) = build_engine();
        eng.start().unwrap();
        pm_p.try_push(Tick::new(
            1_000,
            7,
            1,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        ))
        .unwrap();
        eng.tick(16);
        // `now` < last_touched → saturating_sub returns 0, not
        // wrapping nonsense.
        assert_eq!(eng.max_tick_age_ns(0), 0);
    }

    // Silence unused_imports warnings in the test module.
    #[allow(dead_code)]
    fn _touch_side_order() {
        let _ = Order::new(
            0,
            0,
            Side::Bid,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            0,
        );
    }
}
