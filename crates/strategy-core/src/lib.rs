// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-core
//!
//! The `Strategy` trait every strategy crate implements. Dispatch is
//! **compile-time monomorphised** — `Engine<S: Strategy>` inlines every
//! callback into the main loop, so there is zero dyn-dispatch overhead.
//!
//! ## Contract
//!
//! A strategy is an owned value type that:
//! * lives for the entire lifetime of the process,
//! * owns all its state inline (no heap after `on_start`),
//! * receives callbacks on Ticks, Signals, Fills, and periodic Timers,
//! * submits orders by calling `ctx.submit(order)`.
//!
//! The `Ctx` handle is passed by `&mut` on every callback, so the
//! strategy can mutate its internal counters and submit orders without
//! owning the dispatcher directly.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_time::NsTs;
use core_types::{AiCmd, ChannelEvent, Fill, Order, RuleTable, Signal, Tick};

/// Error type returned from `Strategy::on_start`. Startup errors are
/// fatal; the process exits rather than continuing with half-init.
#[derive(Debug)]
pub enum StrategyError {
    /// The strategy detected a misconfiguration (missing symbol map,
    /// nonsensical size caps, etc.).
    Config(&'static str),
}

impl ::core::fmt::Display for StrategyError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            Self::Config(s) => write!(f, "strategy config error: {s}"),
        }
    }
}

impl std::error::Error for StrategyError {}

/// Reason a `ctx.submit` call was rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubmitErr {
    /// The order ring is full — caller must drop the order rather
    /// than block.
    RingFull,
}

/// Dispatcher handle passed to every callback. The real implementation
/// lives in `engine` and pushes orders onto the order ring. Here we
/// define the trait alone so the strategy crates don't pull in
/// `clob-dispatcher`.
pub trait Ctx {
    /// Submit an `Order` to the CLOB. Returns `Err(SubmitErr::RingFull)`
    /// when the order ring is full — the strategy is expected to drop
    /// the order rather than block.
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr>;

    /// Current wall-clock nanoseconds — cheaper than hitting the clock
    /// again from inside a strategy callback.
    fn now_ns(&self) -> NsTs;
}

/// Optional dashboard-facing counters every strategy can expose.
/// Default implementations return 0 so legacy strategies (e.g. the
/// placeholder ones in test fixtures) compile without changes.
pub trait StrategyCounters {
    /// Cumulative orders emitted via `ctx.submit` since `on_start`.
    #[inline]
    fn orders_emitted(&self) -> u64 {
        0
    }
    /// Cumulative orders rejected by the dispatcher (ring-full,
    /// network errors, etc.).
    #[inline]
    fn orders_dropped(&self) -> u64 {
        0
    }
    /// Short ASCII tag identifying the strategy implementation.
    /// Used to register per-strategy Prometheus counters at boot
    /// so the cli can break down `orders_emitted_total` by which
    /// strategy fired. Default is `"unknown"`.
    ///
    /// Implementors should return a stable static string. Each
    /// in-tree strategy overrides this — `"latency-arb"`, `"ev"`,
    /// `"cross-arb"`, `"rule-tree"`, `"set"`.
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "unknown"
    }

    /// Cumulative refused AI `EnableStrategy` commands (Phase 8f §7).
    /// Only `strategy-set` refuses enables, so the default is 0 for
    /// every plain strategy; the cli mirrors this into
    /// `engine_ai_enable_refused_total` generically (monomorphized —
    /// no set-specific plumbing in the engine loop).
    #[inline]
    fn ai_enable_refused(&self) -> u64 {
        0
    }

    // ---- Phase 8g §9 observability family ------------------------
    //
    // Set-level values reach the cli's generic 5 s mirror the same
    // way `ai_enable_refused` does: a default-0 accessor here,
    // overridden by `strategy-set` — never set-specific engine
    // plumbing. Bare strategies report 0 on every row, mirroring how
    // they swallow AI cmds and ruleset tables via the trait defaults.
    //
    // NOTE on `enabled_mask`: `StrategySet` also has an *inherent*
    // `enabled_mask(&self) -> u8`; method-call syntax on the concrete
    // type resolves to the inherent one, so the cli reads this via
    // UFCS (`StrategyCounters::enabled_mask(...)`) exactly like the
    // other rows.

    /// Live strategy-set enable mask (`engine_strategy_enabled_mask`
    /// gauge — the G0 demo finding: this observable did not exist).
    /// 0 for plain strategies, which have no mask.
    #[inline]
    fn enabled_mask(&self) -> u64 {
        0
    }
    /// vm member: active-table row count (`engine_vm_rows_active`
    /// gauge; 0 = inert).
    #[inline]
    fn vm_rows_active(&self) -> u64 {
        0
    }
    /// vm member: active-table epoch (`engine_vm_table_epoch` gauge;
    /// 0 = none ever committed).
    #[inline]
    fn vm_table_epoch(&self) -> u64 {
        0
    }
    /// vm member: rows whose trigger fired, pre-clamp
    /// (`engine_vm_fires_total`).
    #[inline]
    fn vm_fires(&self) -> u64 {
        0
    }
    /// vm member: orders accepted by the dispatcher
    /// (`engine_vm_orders_emitted_total` — the kind="vm"
    /// `StrategyCounters` value, isolated from the set aggregate).
    #[inline]
    fn vm_orders_emitted(&self) -> u64 {
        0
    }
    /// vm member: orders rejected by the dispatcher
    /// (`engine_vm_orders_dropped_total` — kind="vm" value).
    #[inline]
    fn vm_orders_dropped(&self) -> u64 {
        0
    }
    /// vm member: in-stream Commits dropped — nothing staged or hash
    /// mismatch (`engine_vm_commit_dropped_total`, §6).
    #[inline]
    fn vm_commit_dropped(&self) -> u64 {
        0
    }
}

// ---------------------------------------------------------------
// CooldownGate — shared helper for per-slot emit cooldowns
// ---------------------------------------------------------------

/// Fixed-capacity per-slot cooldown gate. Used by every in-tree
/// strategy to enforce a minimum interval between successive emits
/// for the same slot.
///
/// `N` is the slot count (per-symbol for latency-arb / ev /
/// rule-tree; per-group for cross-arb). Zero-alloc; cache-warm.
///
/// ## Usage
///
/// ```ignore
/// let mut gate: CooldownGate<8> = CooldownGate::new(250_000_000); // 250 ms
/// if !gate.allow(idx, now_ns) {
///     return;
/// }
/// // ... build + ctx.submit the order ...
/// if accepted {
///     gate.record_emit(idx, now_ns);
/// }
/// ```
///
/// **Important** — call `record_emit` ONLY when the dispatcher
/// accepted the order. On `RingFull` rejection the cooldown stays
/// open so the strategy retries on the next tick.
#[derive(Debug)]
#[repr(C, align(64))]
pub struct CooldownGate<const N: usize> {
    last_emit_ns: [u64; N],
    cooldown_ns: u64,
}

impl<const N: usize> CooldownGate<N> {
    /// Build a gate with `cooldown_ns` between emits per slot.
    /// All slots start "ready" (`last_emit_ns = 0`).
    #[inline]
    pub const fn new(cooldown_ns: u64) -> Self {
        Self {
            last_emit_ns: [0u64; N],
            cooldown_ns,
        }
    }

    /// Replace the cooldown duration. Boot-only by convention; the
    /// hot path reads it via `allow` without rechecking.
    #[inline]
    pub fn set_cooldown_ns(&mut self, cooldown_ns: u64) {
        self.cooldown_ns = cooldown_ns;
    }

    /// Current cooldown setting (ns).
    #[inline]
    pub const fn cooldown_ns(&self) -> u64 {
        self.cooldown_ns
    }

    /// Check whether `idx`'s cooldown has elapsed. Zero-alloc;
    /// branchless on the common path.
    ///
    /// Returns `false` (gate closed) if `idx >= N` so out-of-range
    /// slots quietly fail closed rather than panic in release.
    #[inline]
    pub fn allow(&self, idx: usize, now_ns: u64) -> bool {
        if idx >= N {
            return false;
        }
        now_ns >= self.last_emit_ns[idx].saturating_add(self.cooldown_ns)
    }

    /// Mark `idx` as having just emitted at `now_ns`. Out-of-range
    /// indices are silently ignored (matches `allow`'s fail-closed
    /// semantics).
    #[inline]
    pub fn record_emit(&mut self, idx: usize, now_ns: u64) {
        if idx < N {
            self.last_emit_ns[idx] = now_ns;
        }
    }

    /// Read the last emit timestamp for `idx` — useful for tests
    /// and dashboards.
    #[inline]
    pub fn last_emit_ns(&self, idx: usize) -> u64 {
        if idx < N {
            self.last_emit_ns[idx]
        } else {
            0
        }
    }
}

impl<const N: usize> Default for CooldownGate<N> {
    fn default() -> Self {
        Self::new(0)
    }
}

/// The strategy trait.
pub trait Strategy: StrategyCounters {
    /// Called exactly once at engine start. Strategies allocate here
    /// (and only here).
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError>;

    /// Called once per Tick popped from the Polymarket tick ring.
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C);

    /// Called once per Signal popped from the signal ring.
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C);

    /// Called once per Fill popped from the fill ring.
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C);

    /// Called once per accepted [`AiCmd`] popped from the AI command
    /// ring (Phase 8f §4.3). The engine has already dropped
    /// TTL-expired commands and re-validated the shape at the drain
    /// site, so implementations may trust `cmd` structurally.
    ///
    /// Defaulted to a no-op so existing strategies compile and behave
    /// unchanged; `strategy-set` (item 7) consumes Enable/Disable/
    /// Halt at the set level and `strategy-ai-exec` (item 8) consumes
    /// the rest. Monomorphized like every other callback — no `dyn`.
    #[inline]
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        let _ = (cmd, ctx);
    }

    /// Called once per [`RuleTable`] slot popped from the ruleset
    /// table-handoff ring (Phase 8g §6), IMMEDIATELY before the AI-cmd
    /// drain of the same engine iteration — so a table Staged and
    /// Commit'd in one batch is received before the Commit dispatches
    /// through [`Self::on_ai`]. Control-plane, operator cadence.
    ///
    /// Defaulted to a no-op: only `strategy-set` forwards the table to
    /// its slot-5 vm member (`vm_mut().receive_table` — the §6 copy-#2
    /// seam); bare strategies ignore tables by design, mirroring how
    /// the `on_ai` default swallows commands on non-set boots.
    /// Monomorphized like every other callback — no `dyn`. No `Ctx`:
    /// receiving a table stages state and never submits.
    #[inline]
    fn on_ruleset_table(&mut self, table: &RuleTable) {
        let _ = table;
    }

    /// Called once per [`ChannelEvent`] popped from a venue-event
    /// lane (WS10-A). v1 carries ONLY funding updates (the spawn-time
    /// `event_mask` gates what an ingress pushes); the cross-venue
    /// field semantics are pinned in docs/wire-format.md — funding:
    /// `channel = Funding`, `v0` = rate ×1e9, `v1` = next-funding-time
    /// ms (0 where the venue has none).
    ///
    /// Defaulted to a no-op so every existing strategy compiles and
    /// behaves unchanged; `strategy-set` forwards it to enabled
    /// members like `on_tick`. Monomorphized — no `dyn`. Lands dark
    /// in Stage 2: no in-tree strategy consumes it yet (the first
    /// consumer is M5/Stage-3 research work).
    #[inline]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        let _ = (event, ctx);
    }

    /// Periodic timer. `now_ns` is the current timestamp; the engine
    /// calls this at roughly the interval returned by `timer_period_ns`.
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C);

    /// How often `on_timer` should fire (ns). `u64::MAX` disables.
    fn timer_period_ns(&self) -> u64;

    /// Called once on graceful shutdown.
    fn on_stop<C: Ctx>(&mut self, ctx: &mut C);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopCtx {
        submitted: u32,
        now: NsTs,
    }

    impl Ctx for NoopCtx {
        fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    struct NoopStrat {
        started: bool,
        ticks: u32,
    }

    impl StrategyCounters for NoopStrat {}

    impl Strategy for NoopStrat {
        fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
            self.started = true;
            Ok(())
        }
        fn on_tick<C: Ctx>(&mut self, _tick: &Tick, _ctx: &mut C) {
            self.ticks += 1;
        }
        fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}
        fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}
        fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}
        fn timer_period_ns(&self) -> u64 {
            u64::MAX
        }
        fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
    }

    #[test]
    fn trait_is_object_usable_through_monomorphised_engine() {
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        assert!(s.started);

        let t = Tick::new(
            0,
            core_types::VenueId::Polymarket,
            1,
            1,
            core_types::Price::from_raw(0),
            core_types::Qty::from_raw(0),
            core_types::Price::from_raw(0),
            core_types::Qty::from_raw(0),
        );
        s.on_tick(&t, &mut ctx);
        assert_eq!(s.ticks, 1);
    }

    #[test]
    fn on_ruleset_table_defaults_to_noop() {
        // Happy path for the 8g §6 default: a strategy that does not
        // override the hook compiles and its state is untouched by a
        // delivered table.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let mut table = RuleTable::EMPTY;
        table.len = 1;
        s.on_ruleset_table(&table);
        assert!(s.started);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook cannot submit (no Ctx)");
    }

    #[test]
    fn on_venue_event_defaults_to_noop() {
        // WS10-A default: a strategy that does not override the hook
        // compiles and neither its state nor the Ctx is touched by a
        // delivered event.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let ev = ChannelEvent::new(
            1,
            core_types::VenueId::Okx,
            core_types::ChannelId::Funding,
            7,
            0,
            0,
            125_000_000,
            1_700_000_000_000,
        );
        s.on_venue_event(&ev, &mut ctx);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook must not submit");
    }

    #[test]
    fn on_ruleset_table_default_ignores_oversized_len() {
        // Failure-mode shape: even a table whose `len` exceeds
        // RULE_TABLE_ROWS (impossible through the §4.2 validator) is
        // inert through the default hook — clamping is the concrete
        // receiver's job (`VmStrategy::receive_table`), not the
        // trait's.
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        let mut table = RuleTable::EMPTY;
        table.len = u32::MAX;
        s.on_ruleset_table(&table);
        assert_eq!(s.ticks, 0);
    }

    #[test]
    fn observability_defaults_are_all_zero() {
        // 8g §9 bare-strategy posture: a strategy that overrides
        // nothing reports 0 on every observability row — the cli's
        // generic mirror renders an inert vm family on non-set boots.
        let s = NoopStrat {
            started: false,
            ticks: 0,
        };
        assert_eq!(StrategyCounters::enabled_mask(&s), 0);
        assert_eq!(s.vm_rows_active(), 0);
        assert_eq!(s.vm_table_epoch(), 0);
        assert_eq!(s.vm_fires(), 0);
        assert_eq!(s.vm_orders_emitted(), 0);
        assert_eq!(s.vm_orders_dropped(), 0);
        assert_eq!(s.vm_commit_dropped(), 0);
    }

    #[test]
    fn observability_overrides_flow_through_the_trait() {
        // The `ai_enable_refused` route generalized: an overriding
        // implementor's values reach a generic reader through the
        // trait (UFCS — the cli never names the concrete type).
        struct Rich;
        impl StrategyCounters for Rich {
            fn enabled_mask(&self) -> u64 {
                0b10_0011
            }
            fn vm_rows_active(&self) -> u64 {
                7
            }
            fn vm_table_epoch(&self) -> u64 {
                3
            }
            fn vm_fires(&self) -> u64 {
                41
            }
            fn vm_orders_emitted(&self) -> u64 {
                11
            }
            fn vm_orders_dropped(&self) -> u64 {
                2
            }
            fn vm_commit_dropped(&self) -> u64 {
                5
            }
        }
        fn read<S: StrategyCounters>(s: &S) -> [u64; 7] {
            [
                StrategyCounters::enabled_mask(s),
                s.vm_rows_active(),
                s.vm_table_epoch(),
                s.vm_fires(),
                s.vm_orders_emitted(),
                s.vm_orders_dropped(),
                s.vm_commit_dropped(),
            ]
        }
        assert_eq!(read(&Rich), [0b10_0011, 7, 3, 41, 11, 2, 5]);
    }

    // ---------------- CooldownGate ----------------
    //
    // The gate mirrors the existing in-tree strategies'
    // `now >= last_emit + cooldown` semantic: the very first call
    // requires `now >= cooldown_ns`. In production `now_ns()` is
    // wallclock ns (~10^18), so cooldown is always trivially
    // exceeded at boot. Tests use synthetic `now` values that
    // explicitly clear the cooldown window.

    #[test]
    fn cooldown_gate_allow_after_first_window() {
        // Cooldown=1000; now=2000 ≥ 0+1000 → allowed.
        let gate: CooldownGate<4> = CooldownGate::new(1_000);
        assert!(gate.allow(0, 2_000));
        assert!(gate.allow(0, 9_999));
    }

    #[test]
    fn cooldown_gate_blocks_within_window_after_record() {
        let mut gate: CooldownGate<4> = CooldownGate::new(1_000);
        gate.record_emit(0, 5_000);
        assert!(!gate.allow(0, 5_500), "within cooldown should block");
        assert!(gate.allow(0, 6_000), "at boundary should allow");
        assert!(gate.allow(0, 7_000));
    }

    #[test]
    fn cooldown_gate_is_per_slot() {
        let mut gate: CooldownGate<4> = CooldownGate::new(1_000);
        gate.record_emit(0, 5_000);
        // Slot 1 untouched — last_emit=0, so allowed once
        // `now >= cooldown_ns = 1_000`.
        assert!(gate.allow(1, 5_000));
        assert!(!gate.allow(0, 5_500));
    }

    #[test]
    fn cooldown_gate_out_of_range_fails_closed() {
        let mut gate: CooldownGate<4> = CooldownGate::new(1_000);
        assert!(!gate.allow(4, 9_999), "idx >= N must fail closed");
        // record_emit silently no-ops; nothing panics.
        gate.record_emit(99, 5_000);
        // Slot 0 untouched; now=2_000 >= cooldown=1_000.
        assert!(gate.allow(0, 2_000));
    }

    #[test]
    fn cooldown_gate_set_cooldown_updates() {
        let mut gate: CooldownGate<4> = CooldownGate::new(1_000);
        gate.record_emit(0, 5_000);
        gate.set_cooldown_ns(500);
        assert!(gate.allow(0, 5_500));
    }

    #[test]
    fn cooldown_gate_last_emit_ns_accessor() {
        let mut gate: CooldownGate<4> = CooldownGate::new(1_000);
        assert_eq!(gate.last_emit_ns(0), 0);
        gate.record_emit(0, 42);
        assert_eq!(gate.last_emit_ns(0), 42);
        assert_eq!(gate.last_emit_ns(99), 0, "OOB returns 0");
    }
}
