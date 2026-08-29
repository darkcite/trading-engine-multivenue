// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # engine
//!
//! `Engine<S: Strategy, D: OrderDispatch>`. The single-writer,
//! compile-time-monomorphized main loop. Drains the tick / signal /
//! fill rings in priority order and dispatches to the strategy's
//! callbacks.
//!
//! Phase 8a generalizes the fan-in: instead of one hardwired
//! consumer field + drain arm per venue, the engine owns **lane
//! arrays** indexed by `VenueId` — five tick lanes and four fill
//! lanes. Unspawned venues hand the engine a permanently-empty ring
//! (a `try_pop() → None` is two atomic loads — negligible), which
//! makes venues 4..N mechanical instead of structural.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use std::sync::Arc;

use clob_dispatcher::OrderDispatch;
use core_io::SlotCapture;
use core_latency::LatencyTracker;
use core_ring::Consumer;
use core_time::{now_ns, NsTs};
use core_types::{
    AiCmd, ChannelEvent, Fill, Order, RuleTableSlot, Signal, Tick, VenueId, AI_RING_SIZE,
    EVENT_RING_SIZE, RULE_TABLE_RING_SLOTS,
};
use ingress_ai::AiIngressStatus;
use strategy_core::{Ctx, Strategy, StrategyError, SubmitErr};

/// File name of the engine-thread fills capture inside the per-run
/// capture directory (Phase 8f item 6). `SlotKind::Fill` slots: paper
/// fills now, venue fills ride the same path post-8j. This file is
/// the positions/P&L feed for the offline research loop — the AI sees
/// open positions via replay, never via a new IPC channel (design §2).
pub const ENGINE_FILLS_FILE: &str = "engine-fills.pmlr";

/// Engine-thread order-intent capture (M4.1 M-a): every order the
/// dispatcher ACCEPTED via `ctx.submit`, staged next to the fills
/// file. Refusals (ring-full etc.) remain counters only. This file is
/// the `audit-pnl` "logged intents" input (mvp-plan §9.9) — before it
/// existed, paper mode captured neither intents nor fills.
pub const ENGINE_ORDERS_FILE: &str = "engine-orders.pmlr";

// ---------------------------------------------------------------
// Compile-time ring sizes + lane geometry
// ---------------------------------------------------------------

/// Capacity of **every** venue tick ring (Phase 8a §3.3: one
/// standardized size so the lane array has a single consumer type).
/// 64 B × 16 384 = 1 MiB per ring; five rings = 5 MiB.
/// Ingress crates' `DEFAULT_TICK_RING_CAP` must equal this — the cli
/// const-asserts it.
pub const TICK_RING_SIZE: usize = 16_384;
/// Signal ring capacity. Matches
/// `ingress_rpc::run_loop::DEFAULT_SIGNAL_RING_CAP` so the
/// consumer type lines up without a re-allocate.
pub const SIGNAL_RING_SIZE: usize = 1_024;
/// Fill ring capacity (per lane).
pub const FILL_RING_SIZE: usize = 1_024;

/// Number of tick lanes: 0 = Polymarket, 1 = Binance, 2 = OKX,
/// 3 = Deribit, 4 = Hyperliquid, 5 = Bybit (WS9). Lane indices are
/// BOOT-WIRED and NO LONGER equal `VenueId as usize` past lane 4:
/// `VenueId::Ai = 5` has no tick lane (AI commands ride their own
/// ring, 8f), so `VenueId::Bybit = 6` occupies lane 5 — see
/// [`tick_lane_of`].
pub const NUM_TICK_LANES: usize = 6;

/// Tick-lane index for a market-data venue (WS9 — the lane↔venue
/// identity broke when Bybit's discriminant landed past `Ai`).
/// `None` for `Ai` (no market data). Cold-path helper for boot
/// wiring — the drain loop walks all lanes unconditionally.
#[inline]
pub const fn tick_lane_of(venue: VenueId) -> Option<usize> {
    match venue {
        VenueId::Polymarket => Some(0),
        VenueId::Binance => Some(1),
        VenueId::Okx => Some(2),
        VenueId::Deribit => Some(3),
        VenueId::Hyperliquid => Some(4),
        VenueId::Bybit => Some(5),
        VenueId::Ai => None,
    }
}

/// Number of venue-event lanes (WS10-A): mirrors the tick-lane
/// geometry — same indices, same [`tick_lane_of`] map. Every lane
/// carries [`ChannelEvent`] slots; venues without an event source
/// (PM, RPC in v1) hand the engine a permanently-empty ring (§3.3
/// unspawned pattern). Ring capacity is
/// [`core_types::EVENT_RING_SIZE`].
pub const NUM_EVENT_LANES: usize = NUM_TICK_LANES;

/// Number of fill lanes: Polymarket, OKX, Deribit, Hyperliquid.
/// Binance is market-data-only, so it has no fill lane.
pub const NUM_FILL_LANES: usize = 4;

/// Per-iteration drain budget for the AI command lane (Phase 8f §4.3).
///
/// Deliberately its own constant rather than reusing `max_per_ring`:
/// AI commands are control-plane traffic at ~1 cmd/s steady state, so
/// the lane is almost always empty; a small fixed budget bounds the
/// worst-case tick-time contribution while still draining a fully
/// backed-up `AI_RING_SIZE` (1024) ring within 128 iterations of an
/// engine loop that spins far faster than the producer can refill.
pub const AI_DRAIN_BUDGET: usize = 8;

/// Fill-lane index for an execution venue. `None` for venues that
/// cannot produce fills (Binance = data-only, Ai = command feed).
/// Cold-path helper for dispatcher wiring — not used in the drain
/// loop, which walks all lanes unconditionally.
#[inline]
pub const fn fill_lane_of(venue: VenueId) -> Option<usize> {
    match venue {
        VenueId::Polymarket => Some(0),
        VenueId::Okx => Some(1),
        VenueId::Deribit => Some(2),
        VenueId::Hyperliquid => Some(3),
        // WS9: Bybit is market-data-only in Stage 2 (order
        // submission is Stage-3, gaps-doc §7) — no fill lane yet.
        VenueId::Binance | VenueId::Ai | VenueId::Bybit => None,
    }
}

// ---------------------------------------------------------------
// Engine
// ---------------------------------------------------------------

/// The engine. Generic over the strategy `S` and the dispatcher `D`
/// so both are monomorphized at compile time.
pub struct Engine<S: Strategy, D: OrderDispatch> {
    strat: S,
    disp: D,
    /// Tick lanes indexed by `VenueId as usize` (§3.3).
    tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
    /// Venue-event lanes (WS10-A), tick-lane indexing. v1 carries
    /// funding only (the spawn-time `event_mask` gates producers).
    event_lanes: [Consumer<ChannelEvent, EVENT_RING_SIZE>; NUM_EVENT_LANES],
    sig_cons: Consumer<Signal, SIGNAL_RING_SIZE>,
    /// Fill lanes; see [`fill_lane_of`] for the venue → index map.
    fill_lanes: [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
    /// AI command lane (Phase 8f §4.3). Sole consumer of the
    /// `Ring<AiCmd, AI_RING_SIZE>` produced by the `ingress-ai`
    /// thread. When `ingress-ai` is not spawned the producer half is
    /// dropped and this lane reads empty forever (§3.3 pattern).
    ai_cons: Consumer<AiCmd, AI_RING_SIZE>,
    /// Shared AI-ingress status slot. The engine writes exactly one
    /// field — `expired_total` via `inc_expired()` (TTL-expiry is
    /// observable at pop, not at accept; per-field single-writer
    /// discipline documented in `ingress-ai::status`).
    ai_status: Arc<AiIngressStatus>,
    /// Ruleset table-handoff lane (Phase 8g §6, item 7). Sole
    /// consumer of the `Ring<RuleTableSlot, 2>` the ingress-ai side
    /// path pushes validated tables into at Stage time. Popped
    /// IMMEDIATELY before the AI-cmd drain each iteration and handed
    /// to `Strategy::on_ruleset_table` (→ the set's vm member —
    /// documented copy #2). When `ingress-ai` is not spawned the
    /// producer half is dropped and this lane reads empty forever
    /// (§3.3 pattern — one acquire load per iteration).
    table_cons: Consumer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    /// AI commands dispatched to `Strategy::on_ai` (post TTL + shape
    /// checks). Read by paper-mode stats and tests.
    pub ai_dispatched: u64,
    /// AI commands dropped by the drain-site shape re-check. The
    /// ingress already rejects malformed frames (§4.4 step 4), so a
    /// nonzero value means a producer bug or ring corruption —
    /// defense in depth, mirrored to
    /// `engine_ai_drain_malformed_total` by the cli.
    pub ai_drain_malformed: u64,
    /// Engine-thread fills capture → [`ENGINE_FILLS_FILE`] (Phase 8f
    /// item 6). `None` when the cli did not wire one (tests, replay
    /// tooling). Every fill dispatched to `Strategy::on_fill` — fill
    /// lanes and dispatcher pump alike — is staged BEFORE the
    /// strategy callback runs, so a strategy panic cannot lose the
    /// record. Flush cadence is caller-driven:
    /// [`Self::maybe_flush_fill_capture`] from the cli report tick +
    /// an unconditional drain in [`Self::stop`].
    fill_capture: Option<SlotCapture<Fill>>,
    /// Engine-thread order-intent capture → [`ENGINE_ORDERS_FILE`]
    /// (M4.1 M-a). `None` when the cli did not wire one. Appended in
    /// `EngineCtx::submit` AFTER the dispatcher accepts — the intent
    /// log records what was TAKEN; refusals remain counters. Flush
    /// rides [`Self::maybe_flush_fill_capture`]'s caller cadence via
    /// [`Self::maybe_flush_order_capture`] + the [`Self::stop`] drain.
    order_capture: Option<SlotCapture<Order>>,
    last_timer_ns: NsTs,
    /// Number of iterations completed (wraps on u64; for paper-mode stats).
    pub iterations: u64,
    /// Cumulative ticks dispatched (all lanes combined).
    pub ticks_dispatched: u64,
    /// Cumulative signals dispatched.
    pub signals_dispatched: u64,
    /// Cumulative fills dispatched (fill lanes + dispatcher pump).
    pub fills_dispatched: u64,
    /// Cumulative venue events dispatched to `on_venue_event`
    /// (WS10-A, all event lanes combined).
    pub events_dispatched: u64,

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
    // `submit_to_ack` is recorded on every consumed fill.
    ingest_lat: LatencyTracker<24>,
    decide_lat: LatencyTracker<24>,
    ack_lat: LatencyTracker<24>,

    /// Per-bucket last-tick timestamps. Each bucket holds the
    /// `now_ns()` of the most recent tick whose mixed symbol hash
    /// (see `core_types::symbol_bucket_mix` — venue byte folded into
    /// the low bits) landed in it. Stale buckets surface as a high
    /// `max_tick_age_ns()` — useful for detecting a silenced
    /// market mid-soak.
    ///
    /// 64 buckets covers any realistic symbol set with low
    /// collision; collisions just mean two syms hash to the same
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
    /// the consumer ends of every lane. Unspawned venues pass the
    /// consumer half of a ring whose producer was dropped — it reads
    /// empty forever at the cost of two atomic loads per iteration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        strat: S,
        disp: D,
        tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
        event_lanes: [Consumer<ChannelEvent, EVENT_RING_SIZE>; NUM_EVENT_LANES],
        sig_cons: Consumer<Signal, SIGNAL_RING_SIZE>,
        fill_lanes: [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
        ai_cons: Consumer<AiCmd, AI_RING_SIZE>,
        ai_status: Arc<AiIngressStatus>,
        table_cons: Consumer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    ) -> Self {
        Self {
            strat,
            disp,
            tick_lanes,
            event_lanes,
            sig_cons,
            fill_lanes,
            ai_cons,
            ai_status,
            table_cons,
            ai_dispatched: 0,
            ai_drain_malformed: 0,
            fill_capture: None,
            order_capture: None,
            last_timer_ns: 0,
            iterations: 0,
            ticks_dispatched: 0,
            signals_dispatched: 0,
            fills_dispatched: 0,
            events_dispatched: 0,
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
            order_capture: self.order_capture.as_mut(),
            now: now_ns(),
        };
        self.strat.on_start(&mut ctx)
    }

    /// Drain each lane once (up to `max_per_ring` items per lane).
    /// This is the single-writer hot path body — the real run loop
    /// calls this in a tight `loop { ... }`.
    ///
    /// Tick lanes drain in **fixed `VenueId` order** (Polymarket,
    /// Binance, OKX, Deribit, Hyperliquid) so the venue book update
    /// precedes any cross-venue trigger within one iteration; the
    /// per-lane budget prevents starvation. Then signals, then fill
    /// lanes, then the dispatcher's own fill queue (D3 fix — paper
    /// and queued dispatchers surface fills via `try_next_fill`,
    /// live venue dispatchers via their fill lane), then the ruleset
    /// table pop (8g §6 — always immediately before the AI-cmd
    /// drain), then the AI command lane.
    ///
    /// **Latency recording.** Every drained tick samples the
    /// `now - tick.ts_ns` gap into the `ingest_lat` tracker. Every
    /// `ctx.submit` samples the `now - order.ts_ns` gap into the
    /// `decide_lat` tracker via [`EngineCtx::submit`]. Both record
    /// paths are zero-alloc.
    #[inline]
    pub fn tick(&mut self, max_per_ring: usize) {
        self.iterations = self.iterations.wrapping_add(1);

        // --- tick lanes, fixed VenueId order ---
        // E-2: per-item clock sample. Capturing `now` once per batch
        // would bias every record after the first toward zero; each
        // pop re-samples — ~14 ns/tick for a truthful histogram.
        let mut lane = 0;
        while lane < NUM_TICK_LANES {
            let mut i = 0;
            while i < max_per_ring {
                match self.tick_lanes[lane].try_pop() {
                    Some(t) => {
                        let now = now_ns();
                        self.ingest_lat.record(now.saturating_sub(t.ts_ns));
                        self.touch_sym_bucket(t.sym, now);
                        let mut ctx = EngineCtx {
                            disp: &mut self.disp,
                            decide_lat: &self.decide_lat,
                            order_capture: self.order_capture.as_mut(),
                            now,
                        };
                        self.strat.on_tick(&t, &mut ctx);
                        self.ticks_dispatched = self.ticks_dispatched.wrapping_add(1);
                    }
                    None => break,
                }
                i += 1;
            }
            lane += 1;
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
                        order_capture: self.order_capture.as_mut(),
                        now,
                    };
                    self.strat.on_signal(&s, &mut ctx);
                    self.signals_dispatched = self.signals_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- fill lanes ---
        let mut lane = 0;
        while lane < NUM_FILL_LANES {
            let mut i = 0;
            while i < max_per_ring {
                match self.fill_lanes[lane].try_pop() {
                    Some(f) => {
                        let now = now_ns();
                        self.ack_lat.record(now.saturating_sub(f.ts_ns));
                        // Phase 8f: stage the fill to
                        // engine-fills.pmlr before the strategy sees
                        // it (audit completeness over callback order).
                        if let Some(cap) = self.fill_capture.as_mut() {
                            cap.append(&f);
                        }
                        let mut ctx = EngineCtx {
                            disp: &mut self.disp,
                            decide_lat: &self.decide_lat,
                            order_capture: self.order_capture.as_mut(),
                            now,
                        };
                        self.strat.on_fill(&f, &mut ctx);
                        self.fills_dispatched = self.fills_dispatched.wrapping_add(1);
                    }
                    None => break,
                }
                i += 1;
            }
            lane += 1;
        }

        // --- dispatcher fill pump (D3) ---
        // `try_next_fill` was declared in Phase 1 and never called:
        // fills from the paper/queued dispatchers were unreachable
        // and `Strategy::on_fill` was dead code. Budgeted like every
        // other source.
        let mut i = 0;
        while i < max_per_ring {
            match self.disp.try_next_fill() {
                Some(f) => {
                    let now = now_ns();
                    self.ack_lat.record(now.saturating_sub(f.ts_ns));
                    // Phase 8f: paper/queued fills are captured on
                    // the same path as venue fills (design §2 row —
                    // one file, both sources).
                    if let Some(cap) = self.fill_capture.as_mut() {
                        cap.append(&f);
                    }
                    let mut ctx = EngineCtx {
                        disp: &mut self.disp,
                        decide_lat: &self.decide_lat,
                        order_capture: self.order_capture.as_mut(),
                        now,
                    };
                    self.strat.on_fill(&f, &mut ctx);
                    self.fills_dispatched = self.fills_dispatched.wrapping_add(1);
                }
                None => break,
            }
            i += 1;
        }

        // --- venue-event lanes (WS10-A) ---
        // Drained after every market-data/fill source (funding is
        // analytics-cadence, 1–10 Hz/venue — ticks keep priority)
        // and before the control plane. Same fixed lane order and
        // per-lane budget as ticks. No latency sample and no sym-
        // bucket touch: events are not tick freshness.
        let mut lane = 0;
        while lane < NUM_EVENT_LANES {
            let mut i = 0;
            while i < max_per_ring {
                match self.event_lanes[lane].try_pop() {
                    Some(e) => {
                        let now = now_ns();
                        let mut ctx = EngineCtx {
                            disp: &mut self.disp,
                            decide_lat: &self.decide_lat,
                            order_capture: self.order_capture.as_mut(),
                            now,
                        };
                        self.strat.on_venue_event(&e, &mut ctx);
                        self.events_dispatched = self.events_dispatched.wrapping_add(1);
                    }
                    None => break,
                }
                i += 1;
            }
            lane += 1;
        }

        // --- ruleset table lane (Phase 8g §6, item 7) ---
        // Popped IMMEDIATELY before the AI-cmd drain, every
        // iteration: a table Staged and Commit'd in the same batch
        // must land in the vm member's staged buffer BEFORE the
        // Commit AiCmd dispatches through `on_ai` (§6 in-stream flip
        // ordering). Drain-to-newest with gate-35 semantics: every
        // popped slot is delivered in ring order and the receiver's
        // staged buffer keeps the last one (restage supersedes —
        // `VmStrategy::receive_table` overwrites). The while-let is
        // bounded by construction (RULE_TABLE_RING_SLOTS = 2); an
        // empty lane costs one acquire load (§3.3 unspawned shape).
        // The pop moves the 16 KiB slot by value into the callee —
        // that is documented copy #2 (§6), operator cadence, bytes
        // not heap.
        while let Some(t) = self.table_cons.try_pop() {
            self.strat.on_ruleset_table(&t);
        }

        // --- AI command lane (Phase 8f §4.3) ---
        // Drained LAST among event sources: AI commands are control
        // plane; market data and fills keep priority within an
        // iteration. Budget is [`AI_DRAIN_BUDGET`], not
        // `max_per_ring` — see the constant's docs.
        let mut i = 0;
        while i < AI_DRAIN_BUDGET {
            match self.ai_cons.try_pop() {
                Some(cmd) => {
                    // Per-item clock sample (E-2 pattern): TTL
                    // arithmetic needs the pop-time clock, and a
                    // batch-captured `now` would misclassify
                    // commands expiring mid-batch.
                    let now = now_ns();
                    // TTL-on-pop (§3, §13 decision 1): `ts_ns` is
                    // engine-monotonic since the accept-time rewrite,
                    // so residency = now - ts_ns with no cross-clock
                    // term. `ttl_ns == 0` means no expiry.
                    if cmd.ttl_ns != 0 && now.saturating_sub(cmd.ts_ns) > cmd.ttl_ns {
                        // Designated drain-site write into the shared
                        // status slot (its only engine-written field).
                        self.ai_status.inc_expired();
                    } else if cmd.validate_shape().is_err() {
                        // Defense in depth: the ingress already
                        // shape-checked at accept (§4.4 step 4), so
                        // this fires only on a producer bug or ring
                        // corruption. Counted, not asserted — the §11
                        // "malformed rejected" drain test exercises
                        // this path, and the asserting boundary is
                        // the ingress.
                        self.ai_drain_malformed = self.ai_drain_malformed.wrapping_add(1);
                    } else {
                        let mut ctx = EngineCtx {
                            disp: &mut self.disp,
                            decide_lat: &self.decide_lat,
                            order_capture: self.order_capture.as_mut(),
                            now,
                        };
                        self.strat.on_ai(&cmd, &mut ctx);
                        self.ai_dispatched = self.ai_dispatched.wrapping_add(1);
                    }
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
                    order_capture: self.order_capture.as_mut(),
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
    /// p50 submit→ack latency (populated whenever any fill source
    /// delivers).
    #[inline]
    pub fn ack_p50_ns(&self) -> u64 {
        self.ack_lat.percentile(0.50)
    }
    /// p99 submit→ack latency.
    #[inline]
    pub fn ack_p99_ns(&self) -> u64 {
        self.ack_lat.percentile(0.99)
    }

    /// Touch the per-symbol last-tick bucket. Zero-alloc. The venue
    /// byte is mixed into the bucket index (Phase 8a §3.1) so two
    /// venues' ordinal-0 symbols do not collide on low bits.
    #[inline(always)]
    fn touch_sym_bucket(&mut self, sym: core_types::SymbolId, now: NsTs) {
        let bucket = (core_types::symbol_bucket_mix(sym) as usize) & (SYM_BUCKETS - 1);
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

    /// Attach the engine-thread fills capture (boot-only, before
    /// [`Self::start`]). The cli opens [`ENGINE_FILLS_FILE`] inside
    /// the per-run capture directory and hands it over here.
    pub fn set_fill_capture(&mut self, cap: SlotCapture<Fill>) {
        self.fill_capture = Some(cap);
    }

    /// Attach the engine-thread order-intent capture (M4.1; boot-only,
    /// before [`Self::start`]). The cli opens [`ENGINE_ORDERS_FILE`]
    /// inside the per-run capture directory and hands it over here.
    pub fn set_order_capture(&mut self, cap: SlotCapture<Order>) {
        self.order_capture = Some(cap);
    }

    /// Drain staged fills to disk if the capture flush interval has
    /// elapsed. Called from the cli's 5 s report tick — off the hot
    /// path — so staged fills reach disk within one report period
    /// even when no further fills arrive. No-op without a capture.
    #[inline]
    pub fn maybe_flush_fill_capture(&mut self, now_ns: u64) {
        if let Some(cap) = self.fill_capture.as_mut() {
            cap.maybe_flush(now_ns);
        }
    }

    /// Drain staged order intents to disk on the same caller cadence
    /// as the fills capture (cli 5 s report tick). No-op without a
    /// capture.
    #[inline]
    pub fn maybe_flush_order_capture(&mut self, now_ns: u64) {
        if let Some(cap) = self.order_capture.as_mut() {
            cap.maybe_flush(now_ns);
        }
    }

    /// Orders staged to the intent capture since boot (0 without a
    /// capture). Mirrored into the `engine_orders_capture_records`
    /// gauge.
    #[inline]
    pub fn order_capture_records(&self) -> u64 {
        match self.order_capture.as_ref() {
            Some(cap) => cap.records(),
            None => 0,
        }
    }

    /// Intent-capture I/O errors (first one sticky-disables; 0
    /// without a capture). Mirrored into the
    /// `engine_orders_capture_io_errors` gauge.
    #[inline]
    pub fn order_capture_io_errors(&self) -> u64 {
        match self.order_capture.as_ref() {
            Some(cap) => cap.io_errors(),
            None => 0,
        }
    }

    /// Fills staged to the capture since boot (0 without a capture).
    /// Mirrored into the `engine_fills_capture_records` gauge.
    #[inline]
    pub fn fill_capture_records(&self) -> u64 {
        match self.fill_capture.as_ref() {
            Some(cap) => cap.records(),
            None => 0,
        }
    }

    /// Capture I/O errors (first one sticky-disables the sink; 0
    /// without a capture). Mirrored into the
    /// `engine_fills_capture_io_errors` gauge — nonzero is a soak
    /// red flag even though trading continues.
    #[inline]
    pub fn fill_capture_io_errors(&self) -> u64 {
        match self.fill_capture.as_ref() {
            Some(cap) => cap.io_errors(),
            None => 0,
        }
    }

    /// Call `on_stop` on the owned strategy, then drain the fills
    /// capture (orderly-shutdown path; drop would also drain, but an
    /// explicit flush surfaces the I/O error counter first).
    pub fn stop(&mut self) {
        let mut ctx = EngineCtx {
            disp: &mut self.disp,
            decide_lat: &self.decide_lat,
            order_capture: self.order_capture.as_mut(),
            now: now_ns(),
        };
        self.strat.on_stop(&mut ctx);
        if let Some(cap) = self.fill_capture.as_mut() {
            // Sticky-disable policy: errors here are counted by the
            // sink itself; nothing to propagate at teardown.
            let _ = cap.flush_all();
        }
        if let Some(cap) = self.order_capture.as_mut() {
            // Same policy for the M4.1 intent log.
            let _ = cap.flush_all();
        }
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

    /// Borrow the shared AI-ingress status slot (cli metrics mirror +
    /// tests). The engine's own write into it is `inc_expired` at the
    /// drain site; everything else is read-only from here.
    #[inline]
    pub fn ai_status(&self) -> &AiIngressStatus {
        &self.ai_status
    }
}

/// Concrete `Ctx` passed into strategy callbacks.
struct EngineCtx<'a, D: OrderDispatch> {
    disp: &'a mut D,
    decide_lat: &'a LatencyTracker<24>,
    /// M4.1 intent log — reborrowed from the engine per callback;
    /// `None` when the cli wired no capture (tests, replay tooling).
    order_capture: Option<&'a mut SlotCapture<Order>>,
    now: NsTs,
}

impl<'a, D: OrderDispatch> Ctx for EngineCtx<'a, D> {
    #[inline(always)]
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        // Strategy stamps `order.ts_ns` from `ctx.now_ns()` so the
        // gap here equals the time spent inside the strategy
        // callback. Atomic record — never blocks.
        self.decide_lat.record(now_ns().saturating_sub(order.ts_ns));
        match self.disp.submit(&order) {
            Ok(()) => {
                // M4.1: capture-what-was-accepted — stage the intent
                // to engine-orders.pmlr AFTER the dispatcher takes
                // it; refusals stay counters. Staged copy of one
                // 64 B slot; zero-alloc (SlotCapture policy).
                if let Some(cap) = self.order_capture.as_deref_mut() {
                    cap.append(&order);
                }
                Ok(())
            }
            Err(_) => Err(SubmitErr::RingFull),
        }
    }
    #[inline(always)]
    fn now_ns(&self) -> NsTs {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clob_dispatcher::{DispatchError, DispatchStats, PaperDispatcher};
    use core_ring::{Producer, Ring};
    use core_types::{Price, Qty, Side};

    #[derive(Default)]
    struct Counter {
        ticks: u32,
        signals: u32,
        fills: u32,
        ai_cmds: u32,
        /// Venue events delivered via `on_venue_event` (WS10-A).
        events: u32,
        /// v0 of the most recent delivered event — pins payload
        /// pass-through.
        last_event_v0: i64,
        /// Ruleset tables delivered via `on_ruleset_table` (8g §6).
        tables: u32,
        /// Epoch of the most recent delivered table — pins in-ring
        /// delivery order (the receiver keeps the last = supersede).
        last_table_epoch: u32,
        /// `tables` observed when the FIRST AiCmd dispatched — pins
        /// the §6 pop-precedes-AI-drain same-iteration ordering.
        tables_at_first_ai: u32,
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
        fn on_ai<C: Ctx>(&mut self, _cmd: &AiCmd, _ctx: &mut C) {
            if self.ai_cmds == 0 {
                self.tables_at_first_ai = self.tables;
            }
            self.ai_cmds += 1;
        }
        fn on_venue_event<C: Ctx>(&mut self, e: &ChannelEvent, _ctx: &mut C) {
            self.events += 1;
            self.last_event_v0 = e.v0;
        }
        fn on_ruleset_table(&mut self, table: &core_types::RuleTable) {
            self.tables += 1;
            self.last_table_epoch = table.epoch;
        }
        fn on_timer<C: Ctx>(&mut self, _n: NsTs, _ctx: &mut C) {}
        fn timer_period_ns(&self) -> u64 {
            u64::MAX
        }
        fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
    }

    fn mk_tick(venue: VenueId, sym: u32, seq: u32) -> Tick {
        Tick::new(
            0,
            venue,
            sym,
            seq,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        )
    }

    fn split_tick_lanes() -> (
        [Producer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
        [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
    ) {
        let (p0, c0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        let (p1, c1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        let (p2, c2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        let (p3, c3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        let (p4, c4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        let (p5, c5) = Ring::<Tick, TICK_RING_SIZE>::new().split();
        ([p0, p1, p2, p3, p4, p5], [c0, c1, c2, c3, c4, c5])
    }

    fn split_event_lanes() -> (
        [Producer<ChannelEvent, EVENT_RING_SIZE>; NUM_EVENT_LANES],
        [Consumer<ChannelEvent, EVENT_RING_SIZE>; NUM_EVENT_LANES],
    ) {
        let (p0, c0) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        let (p1, c1) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        let (p2, c2) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        let (p3, c3) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        let (p4, c4) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        let (p5, c5) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
        ([p0, p1, p2, p3, p4, p5], [c0, c1, c2, c3, c4, c5])
    }

    fn split_fill_lanes() -> (
        [Producer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
        [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
    ) {
        let (p0, c0) = Ring::<Fill, FILL_RING_SIZE>::new().split();
        let (p1, c1) = Ring::<Fill, FILL_RING_SIZE>::new().split();
        let (p2, c2) = Ring::<Fill, FILL_RING_SIZE>::new().split();
        let (p3, c3) = Ring::<Fill, FILL_RING_SIZE>::new().split();
        ([p0, p1, p2, p3], [c0, c1, c2, c3])
    }

    /// Build an engine plus producer halves for every lane. Tick
    /// producers are indexed by `VenueId as usize`, fill producers
    /// by [`fill_lane_of`]; the last producer feeds the 8g ruleset
    /// table lane.
    #[allow(clippy::type_complexity)]
    fn build_engine() -> (
        Engine<Counter, PaperDispatcher>,
        [Producer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
        [Producer<ChannelEvent, EVENT_RING_SIZE>; NUM_EVENT_LANES],
        Producer<Signal, SIGNAL_RING_SIZE>,
        [Producer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
        Producer<AiCmd, AI_RING_SIZE>,
        Producer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    ) {
        let (tp, tc) = split_tick_lanes();
        let (ep, ec) = split_event_lanes();
        let sig_ring: std::sync::Arc<Ring<Signal, SIGNAL_RING_SIZE>> = Ring::new();
        let (sp, sc) = sig_ring.split();
        let (fp, fc) = split_fill_lanes();
        let (ap, ac) = Ring::<AiCmd, AI_RING_SIZE>::new().split();
        let (tblp, tblc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();

        let strat = Counter::default();
        let disp = PaperDispatcher::new();
        let eng = Engine::new(
            strat,
            disp,
            tc,
            ec,
            sc,
            fc,
            ac,
            Arc::new(AiIngressStatus::new()),
            tblc,
        );
        (eng, tp, ep, sp, fp, ap, tblp)
    }

    #[test]
    fn engine_drains_polymarket_tick_lane() {
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        for i in 0..3u32 {
            tp[VenueId::Polymarket as usize]
                .try_push(mk_tick(VenueId::Polymarket, 1, i + 1))
                .unwrap();
        }
        eng.tick(16);
        assert_eq!(eng.iterations, 1);
        assert_eq!(eng.ticks_dispatched, 3);
    }

    #[test]
    fn engine_drains_every_lane_per_iteration() {
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        let venues = [
            VenueId::Polymarket,
            VenueId::Binance,
            VenueId::Okx,
            VenueId::Deribit,
            VenueId::Hyperliquid,
        ];
        let mut i = 0;
        while i < venues.len() {
            let v = venues[i];
            tp[v as usize]
                .try_push(mk_tick(v, core_types::make_symbol_id(v, 1), 1))
                .unwrap();
            i += 1;
        }
        eng.tick(16);
        assert_eq!(eng.ticks_dispatched, 5, "one tick per venue lane");
    }

    #[test]
    fn engine_respects_max_per_ring_cap_per_lane() {
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        for i in 0..10u32 {
            tp[0]
                .try_push(mk_tick(VenueId::Polymarket, 1, i + 1))
                .unwrap();
            tp[2].try_push(mk_tick(VenueId::Okx, 2, i + 1)).unwrap();
        }
        eng.tick(3);
        // 3 drained per lane on this iteration; the rest stay queued.
        assert_eq!(eng.ticks_dispatched, 6);
        eng.tick(16);
        assert_eq!(eng.ticks_dispatched, 20);
    }

    #[test]
    fn engine_drains_fill_lanes() {
        let (mut eng, _tp, _ep, _sp, mut fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        let f = Fill::new(10, 7, Side::Bid, Price::from_raw(1), Qty::from_raw(1), 99);
        fp[0].try_push(f).unwrap(); // Polymarket fill lane
        fp[3].try_push(f).unwrap(); // Hyperliquid fill lane
        eng.tick(16);
        assert_eq!(eng.fills_dispatched, 2);
        assert_eq!(eng.strategy().fills, 2);
    }

    /// WS10-A fixture: a funding event with a recognizable payload.
    fn mk_funding_event(venue: VenueId, sym: u32, rate_1e9: i64) -> ChannelEvent {
        ChannelEvent::new(
            1,
            venue,
            core_types::ChannelId::Funding,
            sym,
            0,
            1_700_000_000_000,
            rate_1e9,
            1_700_000_400_000,
        )
    }

    #[test]
    fn engine_drains_event_lanes_and_dispatches() {
        let (mut eng, _tp, mut ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        // One funding event per producing venue (lane indices per
        // tick_lane_of: okx 2, deribit 3, bn 1, bybit 5).
        ep[2].try_push(mk_funding_event(VenueId::Okx, 7, 125)).unwrap();
        ep[3]
            .try_push(mk_funding_event(VenueId::Deribit, 8, -50))
            .unwrap();
        ep[1]
            .try_push(mk_funding_event(VenueId::Binance, 9, 10))
            .unwrap();
        ep[5]
            .try_push(mk_funding_event(VenueId::Bybit, 10, 99))
            .unwrap();
        eng.tick(16);
        assert_eq!(eng.events_dispatched, 4, "one event per producing lane");
        assert_eq!(eng.strategy().events, 4);
        // Lanes drain in fixed index order → the last delivered is
        // lane 5 (bybit); payload passes through untouched.
        assert_eq!(eng.strategy().last_event_v0, 99);
        // Ticks/fills untouched by the event path.
        assert_eq!(eng.ticks_dispatched, 0);
        assert_eq!(eng.fills_dispatched, 0);
    }

    #[test]
    fn engine_event_lanes_respect_budget_and_empty_is_noop() {
        let (mut eng, _tp, mut ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        // Empty lanes: a tick() is a per-lane no-op.
        eng.tick(16);
        assert_eq!(eng.events_dispatched, 0);
        // Budget: 10 queued on one lane, budget 3 → 3 per iteration.
        for i in 0..10 {
            ep[2]
                .try_push(mk_funding_event(VenueId::Okx, 7, i as i64))
                .unwrap();
        }
        eng.tick(3);
        assert_eq!(eng.events_dispatched, 3, "budget caps one iteration");
        eng.tick(16);
        assert_eq!(eng.events_dispatched, 10, "backlog drains across iterations");
    }

    /// Dispatcher that emits one queued fill — proves the D3 pump.
    struct OneFillDispatcher {
        fill: Option<Fill>,
        stats: DispatchStats,
    }

    impl OrderDispatch for OneFillDispatcher {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            self.fill.take()
        }
        fn stats(&self) -> DispatchStats {
            self.stats
        }
    }

    #[test]
    fn engine_pumps_dispatcher_fills_d3() {
        let (_tp, tc) = split_tick_lanes();
        let (_sig_p, sc) = Ring::<Signal, SIGNAL_RING_SIZE>::new().split();
        let (_ep, ec) = split_event_lanes();
        let (_fp, fc) = split_fill_lanes();
        let disp = OneFillDispatcher {
            fill: Some(Fill::new(
                5,
                3,
                Side::Ask,
                Price::from_raw(2),
                Qty::from_raw(1),
                42,
            )),
            stats: DispatchStats::default(),
        };
        let strat = Counter::default();
        let (_ap, ac) = Ring::<AiCmd, AI_RING_SIZE>::new().split();
        let (_tblp, tblc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        let mut eng = Engine::new(
            strat,
            disp,
            tc,
            ec,
            sc,
            fc,
            ac,
            Arc::new(AiIngressStatus::new()),
            tblc,
        );
        eng.start().unwrap();
        eng.tick(16);
        assert_eq!(
            eng.fills_dispatched, 1,
            "dispatcher fill must reach on_fill"
        );
        assert_eq!(eng.strategy().fills, 1);
    }

    #[test]
    fn fill_lane_map_matches_layout() {
        assert_eq!(fill_lane_of(VenueId::Polymarket), Some(0));
        assert_eq!(fill_lane_of(VenueId::Okx), Some(1));
        assert_eq!(fill_lane_of(VenueId::Deribit), Some(2));
        assert_eq!(fill_lane_of(VenueId::Hyperliquid), Some(3));
        assert_eq!(fill_lane_of(VenueId::Binance), None);
        assert_eq!(fill_lane_of(VenueId::Ai), None);
    }

    #[test]
    fn engine_dispatcher_starts_with_zero_accepted() {
        let (eng, _tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        assert_eq!(eng.dispatcher().stats().accepted, 0);
    }

    #[test]
    fn max_tick_age_zero_before_any_tick() {
        let (eng, _tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        assert_eq!(eng.populated_sym_count(), 0);
        assert_eq!(eng.max_tick_age_ns(1_000_000), 0);
    }

    #[test]
    fn max_tick_age_tracks_freshest_per_bucket() {
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        tp[0].try_push(mk_tick(VenueId::Polymarket, 7, 1)).unwrap();
        tp[0].try_push(mk_tick(VenueId::Polymarket, 11, 1)).unwrap();
        eng.tick(16);
        // Two distinct symbols → two populated buckets (no mix
        // collision for venue-0 syms 7 and 11).
        assert_eq!(eng.populated_sym_count(), 2);
        let far_future = core_time::now_ns().saturating_add(1_000_000_000);
        let age = eng.max_tick_age_ns(far_future);
        assert!(age > 0, "expected non-zero age, got {age}");
        assert!(eng.populated_sym_count() <= SYM_BUCKETS as u32);
    }

    #[test]
    fn same_ordinal_on_two_venues_lands_in_two_buckets() {
        // The §3.1 regression the mixed bucket exists to prevent:
        // ordinal 0 on every venue used to collapse into bucket 0.
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        let pm = core_types::make_symbol_id(VenueId::Polymarket, 0);
        let okx = core_types::make_symbol_id(VenueId::Okx, 0);
        tp[0].try_push(mk_tick(VenueId::Polymarket, pm, 1)).unwrap();
        tp[2].try_push(mk_tick(VenueId::Okx, okx, 1)).unwrap();
        eng.tick(16);
        assert_eq!(eng.populated_sym_count(), 2);
    }

    // ---------------- fills capture (Phase 8f item 6) ----------------

    #[test]
    fn fills_capture_stages_lane_and_dispatcher_fills() {
        let dir = std::env::temp_dir().join(format!("stage2_engine_fills_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ENGINE_FILLS_FILE);

        // Dispatcher-pump source: one queued fill (D3 shape).
        let (_tp, tc) = split_tick_lanes();
        let (_sig_p, sc) = Ring::<Signal, SIGNAL_RING_SIZE>::new().split();
        let (_ep, ec) = split_event_lanes();
        let (mut fp, fc) = split_fill_lanes();
        let (_ap, ac) = Ring::<AiCmd, AI_RING_SIZE>::new().split();
        let disp = OneFillDispatcher {
            fill: Some(Fill::new(
                5,
                3,
                Side::Ask,
                Price::from_raw(2),
                Qty::from_raw(1),
                42,
            )),
            stats: DispatchStats::default(),
        };
        let strat = Counter::default();
        let (_tblp, tblc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        let mut eng = Engine::new(
            strat,
            disp,
            tc,
            ec,
            sc,
            fc,
            ac,
            Arc::new(AiIngressStatus::new()),
            tblc,
        );
        eng.set_fill_capture(
            core_io::SlotCapture::open(&path, core_io::SlotKind::Fill, 7).unwrap(),
        );
        eng.start().unwrap();

        // Fill-lane source: one Polymarket fill.
        fp[0]
            .try_push(Fill::new(
                10,
                7,
                Side::Bid,
                Price::from_raw(1),
                Qty::from_raw(1),
                99,
            ))
            .unwrap();
        eng.tick(16);
        assert_eq!(eng.fills_dispatched, 2);
        assert_eq!(eng.fill_capture_records(), 2, "both fill sources captured");
        assert_eq!(eng.fill_capture_io_errors(), 0);
        eng.stop(); // drains staging

        let r = core_io::PmlrReader::<Fill>::open(&path).unwrap();
        assert_eq!(r.slot_kind(), core_io::SlotKind::Fill);
        assert_eq!(r.len(), 2);
        // Lane fills drain before the dispatcher pump within tick().
        assert_eq!(r.records()[0].ts_ns, 10);
        assert_eq!(r.records()[1].ts_ns, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fills_capture_getters_zero_without_capture() {
        let (eng, _tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        assert_eq!(eng.fill_capture_records(), 0);
        assert_eq!(eng.fill_capture_io_errors(), 0);
    }

    // ---------------- M4.1: order-intent capture ----------------

    /// A strategy that submits one PM order per tick — the
    /// intent-capture coverage double.
    struct SubmitOnTick;
    impl strategy_core::StrategyCounters for SubmitOnTick {}
    impl Strategy for SubmitOnTick {
        fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
            Ok(())
        }
        fn on_tick<C: Ctx>(&mut self, t: &Tick, ctx: &mut C) {
            let _ = ctx.submit(Order::new(
                t.ts_ns,
                VenueId::Polymarket,
                t.sym,
                Side::Bid,
                0,
                Price::from_raw(410_000),
                Qty::from_raw(1_000_000),
                77,
            ));
        }
        fn on_signal<C: Ctx>(&mut self, _s: &Signal, _ctx: &mut C) {}
        fn on_fill<C: Ctx>(&mut self, _f: &Fill, _ctx: &mut C) {}
        fn on_ai<C: Ctx>(&mut self, _c: &AiCmd, _ctx: &mut C) {}
        fn on_ruleset_table(&mut self, _t: &core_types::RuleTable) {}
        fn on_timer<C: Ctx>(&mut self, _now: u64, _ctx: &mut C) {}
        fn timer_period_ns(&self) -> u64 {
            u64::MAX
        }
        fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
    }

    #[test]
    fn orders_capture_logs_accepted_intents() {
        let dir =
            std::env::temp_dir().join(format!("engine_orders_capture_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ENGINE_ORDERS_FILE);

        let (mut tp, tc) = split_tick_lanes();
        let (_sig_p, sc) = Ring::<Signal, SIGNAL_RING_SIZE>::new().split();
        let (_ep, ec) = split_event_lanes();
        let (_fp, fc) = split_fill_lanes();
        let (_ap, ac) = Ring::<AiCmd, AI_RING_SIZE>::new().split();
        let (_tblp, tblc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        let mut eng = Engine::new(
            SubmitOnTick,
            PaperDispatcher::new(),
            tc,
            ec,
            sc,
            fc,
            ac,
            Arc::new(AiIngressStatus::new()),
            tblc,
        );
        eng.set_order_capture(
            core_io::SlotCapture::open(&path, core_io::SlotKind::Order, 7).unwrap(),
        );
        eng.start().unwrap();
        tp[VenueId::Polymarket as usize]
            .try_push(mk_tick(VenueId::Polymarket, 1, 1))
            .unwrap();
        eng.tick(16);
        assert_eq!(eng.order_capture_records(), 1, "accepted intent staged");
        assert_eq!(eng.order_capture_io_errors(), 0);
        eng.stop(); // drains staging

        let r = core_io::PmlrReader::<Order>::open(&path).unwrap();
        assert_eq!(r.slot_kind(), core_io::SlotKind::Order);
        assert_eq!(r.epoch_ns(), 7);
        assert_eq!(r.len(), 1);
        let o = r.records()[0];
        assert_eq!(o.client_oid, 77);
        assert_eq!(o.sym, 1);
        // The ENGINE does not stamp attribution — bare strategies
        // stay STRATEGY_ID_NONE (the set's StampCtx stamps; pinned
        // in strategy-set tests).
        assert_eq!(o.strategy_id, core_types::STRATEGY_ID_NONE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dispatcher that refuses everything — capture-what-was-accepted
    /// means a refused submit is NEVER staged.
    struct RefuseAllDispatcher;
    impl OrderDispatch for RefuseAllDispatcher {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Err(DispatchError::QueueFull)
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats::default()
        }
    }

    #[test]
    fn orders_capture_skips_refused_submits() {
        let dir =
            std::env::temp_dir().join(format!("engine_orders_refused_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ENGINE_ORDERS_FILE);

        let (mut tp, tc) = split_tick_lanes();
        let (_sig_p, sc) = Ring::<Signal, SIGNAL_RING_SIZE>::new().split();
        let (_ep, ec) = split_event_lanes();
        let (_fp, fc) = split_fill_lanes();
        let (_ap, ac) = Ring::<AiCmd, AI_RING_SIZE>::new().split();
        let (_tblp, tblc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        let mut eng = Engine::new(
            SubmitOnTick,
            RefuseAllDispatcher,
            tc,
            ec,
            sc,
            fc,
            ac,
            Arc::new(AiIngressStatus::new()),
            tblc,
        );
        eng.set_order_capture(
            core_io::SlotCapture::open(&path, core_io::SlotKind::Order, 0).unwrap(),
        );
        eng.start().unwrap();
        tp[VenueId::Polymarket as usize]
            .try_push(mk_tick(VenueId::Polymarket, 1, 1))
            .unwrap();
        eng.tick(16);
        assert_eq!(
            eng.order_capture_records(),
            0,
            "refusal is a counter, not a record"
        );
        eng.stop();
        let r = core_io::PmlrReader::<Order>::open(&path).unwrap();
        assert_eq!(r.len(), 0, "header-only file after refused submits");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orders_capture_getters_zero_without_capture() {
        let (eng, _tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        assert_eq!(eng.order_capture_records(), 0);
        assert_eq!(eng.order_capture_io_errors(), 0);
    }

    // ---------------- AI command lane (Phase 8f §11) ----------------

    /// Well-formed Heartbeat with a caller-chosen accept time + TTL.
    /// Shape rules (§3 table): Heartbeat carries no sym/px/qty/ttl —
    /// so for TTL tests we use SetFairValue, which REQUIRES ttl > 0.
    fn mk_heartbeat(ts_ns: u64, seq: u32) -> AiCmd {
        AiCmd::new(
            ts_ns,
            seq,
            core_types::SYMBOL_ID_NONE,
            0,
            0,
            0,
            core_types::AiCmdKind::Heartbeat,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_NONE,
            core_types::AI_SIDE_NONE,
            0,
            0,
        )
    }

    /// Well-formed SetFairValue whose expiry is fully caller-driven:
    /// `ts_ns` is the (simulated) engine accept time, `ttl_ns` the
    /// allowed ring residency.
    fn mk_fair_value(ts_ns: u64, seq: u32, ttl_ns: u64) -> AiCmd {
        AiCmd::new(
            ts_ns,
            seq,
            core_types::make_symbol_id(VenueId::Polymarket, 1),
            500_000,
            0,
            ttl_ns,
            core_types::AiCmdKind::SetFairValue,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_NONE,
            core_types::AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn ai_lane_dispatches_to_on_ai() {
        let (mut eng, _tp, _ep, _sp, _fp, mut ap, _tblp) = build_engine();
        eng.start().unwrap();
        // ts = now, no TTL → never expires; heartbeat has no shape
        // surprises.
        ap.try_push(mk_heartbeat(now_ns(), 1)).unwrap();
        eng.tick(16);
        assert_eq!(eng.ai_dispatched, 1);
        assert_eq!(eng.strategy().ai_cmds, 1);
        assert_eq!(eng.ai_status().expired(), 0);
        assert_eq!(eng.ai_drain_malformed, 0);
    }

    #[test]
    fn ai_lane_ttl_expires_on_pop() {
        let (mut eng, _tp, _ep, _sp, _fp, mut ap, _tblp) = build_engine();
        eng.start().unwrap();
        let now = now_ns();
        // Accepted 10 ms ago with a 1 ms TTL → expired at pop.
        ap.try_push(mk_fair_value(now.saturating_sub(10_000_000), 1, 1_000_000))
            .unwrap();
        // Accepted now with a generous TTL → dispatched.
        ap.try_push(mk_fair_value(now, 2, 60_000_000_000)).unwrap();
        eng.tick(16);
        assert_eq!(eng.ai_status().expired(), 1, "stale command dropped at pop");
        assert_eq!(eng.ai_dispatched, 1, "fresh command still dispatched");
        assert_eq!(eng.strategy().ai_cmds, 1);
    }

    #[test]
    fn ai_lane_respects_drain_budget() {
        let (mut eng, _tp, _ep, _sp, _fp, mut ap, _tblp) = build_engine();
        eng.start().unwrap();
        let n = (AI_DRAIN_BUDGET * 2 + 3) as u32;
        for i in 0..n {
            ap.try_push(mk_heartbeat(now_ns(), i + 1)).unwrap();
        }
        eng.tick(16);
        assert_eq!(
            eng.ai_dispatched, AI_DRAIN_BUDGET as u64,
            "one iteration must drain at most AI_DRAIN_BUDGET commands"
        );
        eng.tick(16);
        eng.tick(16);
        assert_eq!(
            eng.ai_dispatched, n as u64,
            "backlog drains across iterations"
        );
    }

    #[test]
    fn ai_lane_recheck_rejects_malformed_at_drain() {
        let (mut eng, _tp, _ep, _sp, _fp, mut ap, _tblp) = build_engine();
        eng.start().unwrap();
        // Bypass the ingress (the ring producer is ours) and push a
        // shape violation: Heartbeat must not carry px.
        let mut bad = mk_heartbeat(now_ns(), 1);
        bad.px = 1;
        assert!(bad.validate_shape().is_err(), "fixture must be malformed");
        ap.try_push(bad).unwrap();
        ap.try_push(mk_heartbeat(now_ns(), 2)).unwrap();
        eng.tick(16);
        assert_eq!(
            eng.ai_drain_malformed, 1,
            "malformed slot counted, not dispatched"
        );
        assert_eq!(eng.ai_dispatched, 1);
        assert_eq!(eng.strategy().ai_cmds, 1);
        assert_eq!(
            eng.ai_status().malformed(),
            0,
            "ingress-side malformed counter must NOT move from the drain site"
        );
    }

    // ------------- ruleset table lane (Phase 8g §6, item 7) -------------

    /// Table slot with a recognizable epoch (contents immaterial to
    /// the engine — it moves slots, never reads rows).
    fn mk_table(epoch: u32) -> RuleTableSlot {
        let mut t = core_types::RuleTable::EMPTY;
        t.epoch = epoch;
        t
    }

    /// §6 ordering invariant, engine-level: a table pushed and an
    /// AiCmd (the in-stream Commit's lane) arriving in the SAME batch
    /// are delivered table-first within ONE `tick()` — the pop runs
    /// immediately before the AI-cmd drain.
    #[test]
    fn ruleset_table_pop_precedes_ai_drain_same_iteration() {
        let (mut eng, _tp, _ep, _sp, _fp, mut ap, mut tblp) = build_engine();
        eng.start().unwrap();
        assert!(tblp.try_push(mk_table(7)).is_ok());
        ap.try_push(mk_heartbeat(now_ns(), 1)).unwrap();
        eng.tick(16);
        assert_eq!(
            eng.strategy().tables,
            1,
            "table delivered in the same iteration"
        );
        assert_eq!(eng.strategy().ai_cmds, 1);
        assert_eq!(
            eng.strategy().tables_at_first_ai,
            1,
            "pop must precede the AI-cmd drain within one iteration (§6)"
        );
        assert_eq!(eng.strategy().last_table_epoch, 7);
    }

    /// Drain-to-newest (gate-35 semantics): both queued slots are
    /// delivered in ring order in one iteration; the receiver keeps
    /// the last (restage supersedes — pinned here via epoch order).
    #[test]
    fn ruleset_table_lane_drains_all_slots_in_order() {
        let (mut eng, _tp, _ep, _sp, _fp, _ap, mut tblp) = build_engine();
        eng.start().unwrap();
        assert!(tblp.try_push(mk_table(1)).is_ok());
        assert!(tblp.try_push(mk_table(2)).is_ok());
        assert!(
            tblp.try_push(mk_table(3)).is_err(),
            "cap-2 ring must reject the third undrained stage (§5)"
        );
        eng.tick(16);
        assert_eq!(
            eng.strategy().tables,
            2,
            "both slots delivered in one iteration"
        );
        assert_eq!(
            eng.strategy().last_table_epoch,
            2,
            "in-ring order: newest last"
        );
        // Ring drained ⇒ the lane accepts stages again.
        assert!(tblp.try_push(mk_table(4)).is_ok());
        eng.tick(16);
        assert_eq!(eng.strategy().tables, 3);
        assert_eq!(eng.strategy().last_table_epoch, 4);
    }

    /// Failure-mode shape: an unwired/empty lane is a per-iteration
    /// no-op (the §3.3 unspawned pattern — the hook never fires).
    #[test]
    fn ruleset_table_lane_empty_is_noop() {
        let (mut eng, _tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        eng.tick(16);
        eng.tick(16);
        assert_eq!(eng.strategy().tables, 0);
        assert_eq!(eng.strategy().tables_at_first_ai, 0);
    }

    #[test]
    fn max_tick_age_handles_now_before_recorded() {
        let (mut eng, mut tp, _ep, _sp, _fp, _ap, _tblp) = build_engine();
        eng.start().unwrap();
        tp[0].try_push(mk_tick(VenueId::Polymarket, 7, 1)).unwrap();
        eng.tick(16);
        // `now` < last_touched → saturating_sub returns 0, not
        // wrapping nonsense.
        assert_eq!(eng.max_tick_age_ns(0), 0);
    }
}
