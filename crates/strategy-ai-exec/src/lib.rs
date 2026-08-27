// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-ai-exec
//!
//! The AI-driven execution strategy (Phase 8f, design §7): slot 4 of
//! the `StrategySet`. Consumes `SetFairValue` / `SetBias` frames into
//! a fixed fair-value table, quotes when a venue book deviates beyond
//! the edge parameter from `fair + bias`, and honors paper-only
//! `OrderIntent` frames — all under the §5.4 staleness fail-safe.
//!
//! Compile-time monomorphized. State is fully inline:
//!
//! * `[FairEntry; N]` — open-addressed fair table keyed by
//!   venue-namespaced `SymbolId`. **Linear probe, no hashing in the
//!   hot path**: home slot is `sym % N`, then a forward scan. `N` is
//!   64 in the set (design §7 sketch `AiExec<64>`).
//! * `MultiBook<N>` for venue top-of-book. Symbols are tracked
//!   **lazily**: the first tick of a symbol that has a live fair
//!   entry claims a book slot — no boot symbol config exists, the AI
//!   decides the universe at runtime.
//! * `CooldownGate<N>` per book slot (house pattern).
//!
//! ## Fair table semantics
//!
//! One entry per symbol carrying `{px_1e6, bias_1e6, set_ns, ttl_ns}`
//! (§7). `SetFairValue` upserts the fair price; `SetBias` upserts the
//! signed bias. **Either kind refreshes the entry's single
//! `set_ns`/`ttl_ns` AND its `expire_on_silence` flag** — the entry
//! has one TTL and one policy, not one per field (§7 sketch);
//! last-writer-wins, so a follow-up upsert without the flag clears
//! it. A bias-only entry (`SetBias` before any `SetFairValue`) is
//! held but never quotes until a fair arrives.
//! Entry liveness is evaluated at read time (`now − set_ns >
//! ttl_ns` ⇒ dead) — expiry never writes.
//!
//! Probe invariants (rustdoc'd here because open addressing plus
//! reuse is where these tables rot):
//!
//! * a lookup probes from the home slot until the symbol or a
//!   never-used (`SYMBOL_ID_NONE`) slot;
//! * an upsert scans the full chain for the symbol **before**
//!   claiming the first reusable (never-used, dead, or
//!   silence-expired) slot, so a symbol can never occupy two slots;
//! * reuse rewrites a dead slot's symbol but never empties it, so no
//!   probe chain ever shortens under a live lookup.
//!
//! ## Staleness (§5.4, decision 6)
//!
//! `now − last_frame_ns > `[`AI_STALENESS_NS`]` ⇒ pull AI quotes +
//! refuse intents; recover on the next valid frame.` Liveness is
//! derived from popped frames only (§4.3 — every `on_ai` call updates
//! `last_frame_ns` from the frame's engine-monotonic accept stamp,
//! decision 1); no extra atomics exist. Before the first frame ever,
//! the strategy is stale by definition (fail-safe; the table is
//! necessarily empty anyway). The constant is compile-time and not
//! loosenable anywhere — tests drive synthetic clocks instead.
//!
//! An `OrderIntent` that itself ends a silence window is **refused**
//! (counted in `intents_refused_stale`) but still restores liveness —
//! this is exactly why the worker's heartbeat-precedes-payload rule
//! exists (§5.4): the heartbeat lands first and the intent that
//! follows it is honored.
//!
//! `expire_on_silence` (`AiCmd::flags` bit 0, valid on
//! `SetFairValue`/`SetBias`): when a silence window closes, entries
//! carrying the flag are **permanently** expired (swept once, on the
//! recovery frame — cold path, bounded at `N`). Unflagged entries
//! resume quoting after recovery if their own TTL is still alive.
//! While the strategy is stale, *no* entry quotes regardless of flags
//! — the sweep only decides who comes back.
//!
//! ## Deviation quoting
//!
//! ```text
//! target = fair_1e6 + bias_1e6          // skip if target <= 0
//! dev    = book.mid() - target
//! |dev| < edge          → skip
//! dev > 0               → Ask (market rich)   else Bid (market cheap)
//! ctx.submit(post-only @ mid, self.qty)      // ev-style paper quote
//! ```
//!
//! ## OrderIntent (paper; 8i clamps)
//!
//! Shape is already enforced twice upstream (ingress §4.4, engine
//! drain re-check): `strategy_id == 4`, real market venue, `px > 0`,
//! `qty > 0`, side ∈ {Bid, Ask}. When not stale, the intent is
//! submitted verbatim: `Order { px, qty, side, venue, sym }` at the
//! intent's coordinates.
//!
//! Hot path: one probe (≤ N slot loads, home-hit = 1) + one
//! `MultiBook` apply + integer compares. Zero alloc after boot —
//! gated in `bench/tests/alloc_assertions.rs` for both `on_tick` and
//! `on_ai`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use book_builder::MultiBook;
use core_time::NsTs;
use core_types::{
    symbol_venue_byte, AiCmd, AiCmdKind, Fill, Order, Price, Qty, Side, Signal, SymbolId, Tick,
    VenueId, AI_CMD_FLAG_EXPIRE_ON_SILENCE, SYMBOL_ID_NONE,
};
use strategy_core::{CooldownGate, Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// §5.4 staleness threshold: worker silence beyond this pulls AI
/// quotes and refuses intents. Compile-time per §13 decision 6 —
/// deliberately not settable at runtime; tests use synthetic clocks.
pub const AI_STALENESS_NS: u64 = 15_000_000_000;

/// Default deviation edge: 0.02 (2 cents on a 0..1 binary), matching
/// the ev strategy's default threshold magnitude.
pub const DEFAULT_EDGE_1E6: i64 = 20_000;

/// Default quote quantity (1e6 fixed-point).
pub const DEFAULT_QTY: Qty = Qty::from_raw(10_000_000);

/// Default per-symbol emit cooldown (250 ms).
pub const DEFAULT_COOLDOWN_NS: u64 = 250_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

/// One fair-table slot. `sym == SYMBOL_ID_NONE` means never used
/// (probe chains end here). A dead entry (TTL lapsed or
/// silence-expired) keeps its symbol so probe chains stay intact —
/// see the module docs.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct FairEntry {
    sym: SymbolId,
    /// Cleared by the silence sweep; set by any upsert.
    live: bool,
    /// A `SetFairValue` has populated `px_1e6` (bias-only entries
    /// never quote).
    has_fair: bool,
    /// `AI_CMD_FLAG_EXPIRE_ON_SILENCE` was set on the last upsert.
    expire_on_silence: bool,
    _pad: u8,
    px_1e6: i64,
    bias_1e6: i64,
    set_ns: u64,
    ttl_ns: u64,
}

impl FairEntry {
    const EMPTY: Self = Self {
        sym: SYMBOL_ID_NONE,
        live: false,
        has_fair: false,
        expire_on_silence: false,
        _pad: 0,
        px_1e6: 0,
        bias_1e6: 0,
        set_ns: 0,
        ttl_ns: 0,
    };

    /// TTL check at read time. `ttl_ns` is required `> 0` by the
    /// shape table, so `0` here only occurs on never-upserted slots —
    /// which are unreachable through lookups.
    #[inline(always)]
    fn ttl_alive(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.set_ns) <= self.ttl_ns
    }

    /// Quotable: upsert-live, fair present, TTL alive.
    #[inline(always)]
    fn quotable(&self, now_ns: u64) -> bool {
        self.live && self.has_fair && self.ttl_alive(now_ns)
    }
}

/// Read-only copy of a fair-table entry for tests and dashboards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FairSnapshot {
    /// Fair value ×1e6 (meaningful only when `has_fair`).
    pub px_1e6: i64,
    /// Signed bias ×1e6.
    pub bias_1e6: i64,
    /// Engine-monotonic stamp of the last upsert (decision 1 base).
    pub set_ns: u64,
    /// Entry TTL relative to `set_ns`.
    pub ttl_ns: u64,
    /// Entry dies permanently on the next silence-window close.
    pub expire_on_silence: bool,
    /// A fair value has been published for this symbol.
    pub has_fair: bool,
    /// Not silence-expired.
    pub live: bool,
}

/// The AI-driven execution strategy. `N` sizes the fair table, the
/// book table and the cooldown gate alike (design §7: `AiExec<64>`
/// inside the set).
pub struct AiExec<const N: usize> {
    fair: [FairEntry; N],
    book: MultiBook<N>,
    gate: CooldownGate<N>,

    /// Engine-monotonic accept stamp of the last popped frame
    /// (any kind). 0 = no frame ever ⇒ stale by definition.
    last_frame_ns: u64,

    edge_1e6: i64,
    qty: Qty,
    next_oid: u64,

    /// Frames seen via `on_ai` (any kind — liveness beacon count).
    pub ai_frames_seen: u64,
    /// Accepted `SetFairValue` upserts.
    pub fair_sets: u64,
    /// Accepted `SetBias` upserts.
    pub bias_sets: u64,
    /// Upserts dropped because the probe found no reusable slot.
    pub fair_table_full: u64,
    /// Entries permanently expired by silence-window sweeps.
    pub silence_expired: u64,
    /// `OrderIntent` frames submitted to the dispatcher.
    pub intents_honored: u64,
    /// `OrderIntent` frames refused because the strategy was stale
    /// at the frame's arrival (§5.4).
    pub intents_refused_stale: u64,
    /// Fair-entry symbols that could not claim a book slot (book
    /// table full) — quoting for them is off until a slot frees
    /// (it never does in 8f; sized N=64 alongside the fair table).
    pub book_track_failed: u64,
    /// Orders accepted by the dispatcher (quotes + intents).
    pub orders_emitted: u64,
    /// Orders rejected by the dispatcher (ring full).
    pub orders_dropped: u64,
}

impl<const N: usize> AiExec<N> {
    /// Construct with defaults. Boot-only.
    pub fn new() -> Self {
        Self {
            fair: [FairEntry::EMPTY; N],
            book: MultiBook::empty(),
            gate: CooldownGate::new(DEFAULT_COOLDOWN_NS),
            last_frame_ns: 0,
            edge_1e6: DEFAULT_EDGE_1E6,
            qty: DEFAULT_QTY,
            next_oid: 1,
            ai_frames_seen: 0,
            fair_sets: 0,
            bias_sets: 0,
            fair_table_full: 0,
            silence_expired: 0,
            intents_honored: 0,
            intents_refused_stale: 0,
            book_track_failed: 0,
            orders_emitted: 0,
            orders_dropped: 0,
        }
    }

    /// Replace the deviation edge (1e6 units). Boot-only.
    #[inline]
    pub fn set_edge_1e6(&mut self, edge_1e6: i64) {
        self.edge_1e6 = edge_1e6;
    }

    /// Replace the quote quantity. Boot-only.
    #[inline]
    pub fn set_qty(&mut self, qty: Qty) {
        self.qty = qty;
    }

    /// Replace the per-symbol emit cooldown (ns). Boot-only.
    #[inline]
    pub fn set_cooldown_ns(&mut self, cooldown_ns: u64) {
        self.gate.set_cooldown_ns(cooldown_ns);
    }

    /// Current edge (1e6 units).
    #[inline]
    pub const fn edge_1e6(&self) -> i64 {
        self.edge_1e6
    }

    /// Accept stamp of the last popped frame (0 = never).
    #[inline]
    pub const fn last_frame_ns(&self) -> u64 {
        self.last_frame_ns
    }

    /// §5.4 staleness at `now_ns`. True before the first frame ever.
    #[inline]
    pub fn is_stale(&self, now_ns: u64) -> bool {
        self.last_frame_ns == 0
            || now_ns.saturating_sub(self.last_frame_ns) > AI_STALENESS_NS
    }

    /// Count of upsert-live entries (silence-expired excluded, TTL
    /// not evaluated — pass a clock to reason about TTL). Test +
    /// dashboard surface; O(N).
    pub fn fair_len(&self) -> usize {
        let mut n = 0usize;
        for i in 0..N {
            if self.fair[i].sym != SYMBOL_ID_NONE && self.fair[i].live {
                n += 1;
            }
        }
        n
    }

    /// Read-only copy of `sym`'s fair entry, live or dead. `None`
    /// when the symbol has no slot. Test + dashboard surface.
    pub fn fair_snapshot(&self, sym: SymbolId) -> Option<FairSnapshot> {
        let e = self.fair_get(sym)?;
        Some(FairSnapshot {
            px_1e6: e.px_1e6,
            bias_1e6: e.bias_1e6,
            set_ns: e.set_ns,
            ttl_ns: e.ttl_ns,
            expire_on_silence: e.expire_on_silence,
            has_fair: e.has_fair,
            live: e.live,
        })
    }

    // ---- fair table (open-addressed, linear probe) ----

    /// Lookup: probe from the home slot until `sym` or a never-used
    /// slot. Returns dead entries too — callers gate on liveness.
    #[inline(always)]
    fn fair_get(&self, sym: SymbolId) -> Option<&FairEntry> {
        let home = (sym as usize) % N;
        for k in 0..N {
            let i = (home + k) % N;
            let e = &self.fair[i];
            if e.sym == sym {
                return Some(e);
            }
            if e.sym == SYMBOL_ID_NONE {
                return None;
            }
        }
        None
    }

    /// Upsert probe: index of `sym`'s slot (existing, or a claimed
    /// reusable slot with `sym` written). `None` + counter when the
    /// table has no room. See the module docs for the invariants.
    fn fair_slot_for(&mut self, sym: SymbolId, now_ns: u64) -> Option<usize> {
        let home = (sym as usize) % N;
        let mut reuse: Option<usize> = None;
        for k in 0..N {
            let i = (home + k) % N;
            let e = &self.fair[i];
            if e.sym == sym {
                return Some(i);
            }
            if e.sym == SYMBOL_ID_NONE {
                // Chain ends: sym is absent. Claim the earliest
                // reusable slot seen, else this never-used one.
                let slot = reuse.unwrap_or(i);
                self.fair[slot] = FairEntry::EMPTY;
                self.fair[slot].sym = sym;
                return Some(slot);
            }
            if reuse.is_none() && (!e.live || !e.ttl_alive(now_ns)) {
                reuse = Some(i);
            }
        }
        // Full chain, no sym, no never-used slot.
        match reuse {
            Some(slot) => {
                self.fair[slot] = FairEntry::EMPTY;
                self.fair[slot].sym = sym;
                Some(slot)
            }
            None => {
                self.fair_table_full = self.fair_table_full.wrapping_add(1);
                None
            }
        }
    }

    /// Permanently expire every live `expire_on_silence` entry. Runs
    /// once per silence-window close (cold path, bounded at `N`).
    fn expire_silent_entries(&mut self) {
        for i in 0..N {
            let e = &mut self.fair[i];
            if e.sym != SYMBOL_ID_NONE && e.live && e.expire_on_silence {
                e.live = false;
                self.silence_expired = self.silence_expired.wrapping_add(1);
            }
        }
    }

    // ---- hot path ----

    /// Quote when the book at `bidx` deviates beyond the edge from
    /// `fair + bias`. Caller has already applied the tick and checked
    /// global staleness.
    #[inline(always)]
    fn maybe_quote<C: Ctx>(&mut self, bidx: usize, ctx: &mut C) {
        let top = self.book.slots()[bidx];
        if !top.has_quotes() {
            return;
        }
        let now = ctx.now_ns();
        let (target, entry_ok) = match self.fair_get(top.sym) {
            Some(e) if e.quotable(now) => (e.px_1e6.saturating_add(e.bias_1e6), true),
            _ => (0, false),
        };
        if !entry_ok || target <= 0 {
            // Bias pushed the target out of the price domain, or the
            // entry is bias-only / dead — nothing to quote against.
            return;
        }

        let mid_1e6 = top.mid().raw();
        let dev = mid_1e6 - target;
        let abs_dev = if dev >= 0 { dev } else { -dev };
        if abs_dev < self.edge_1e6 {
            return;
        }
        if !self.gate.allow(bidx, now) {
            return;
        }

        let venue = match VenueId::from_u8(symbol_venue_byte(top.sym)) {
            Some(v) => v,
            None => {
                // Tracked symbols come off venue rings — an
                // undecodable venue byte cannot happen.
                debug_assert!(false, "tracked sym with undecodable venue");
                return;
            }
        };
        let side = if dev > 0 { Side::Ask } else { Side::Bid };
        let order = Order::new(
            now,
            venue,
            top.sym,
            side,
            ORDER_KIND_POST_ONLY,
            Price::from_raw(mid_1e6),
            self.qty,
            self.next_oid,
        );
        self.next_oid = self.next_oid.wrapping_add(1);
        match ctx.submit(order) {
            Ok(()) => {
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
                self.gate.record_emit(bidx, now);
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
            }
        }
    }

    /// Submit an `OrderIntent` verbatim (paper; 8i clamps). Shape is
    /// enforced upstream — see the module docs.
    #[inline(always)]
    fn honor_intent<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        debug_assert_eq!(cmd.strategy_id, core_types::STRATEGY_SLOT_AI_EXEC);
        let venue = match VenueId::from_u8(cmd.venue) {
            Some(v) => v,
            None => {
                // Unreachable past ingress + drain shape checks.
                debug_assert!(false, "OrderIntent with undecodable venue");
                return;
            }
        };
        let side = if cmd.side == Side::Bid as u8 {
            Side::Bid
        } else {
            Side::Ask
        };
        let order = Order::new(
            ctx.now_ns(),
            venue,
            cmd.sym,
            side,
            ORDER_KIND_POST_ONLY,
            Price::from_raw(cmd.px),
            Qty::from_raw(cmd.qty),
            self.next_oid,
        );
        self.next_oid = self.next_oid.wrapping_add(1);
        match ctx.submit(order) {
            Ok(()) => {
                self.intents_honored = self.intents_honored.wrapping_add(1);
                self.orders_emitted = self.orders_emitted.wrapping_add(1);
            }
            Err(SubmitErr::RingFull) => {
                self.orders_dropped = self.orders_dropped.wrapping_add(1);
            }
        }
    }
}

impl<const N: usize> Default for AiExec<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StrategyCounters for AiExec<N> {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.orders_emitted
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.orders_dropped
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "ai-exec"
    }
}

impl<const N: usize> Strategy for AiExec<N> {
    /// Pure parameter validation — the member is inert until frames
    /// arrive, so there is no mandatory boot config (the set's
    /// late-enable invariant holds: validation-only `on_start`).
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        if self.edge_1e6 <= 0 {
            return Err(StrategyError::Config("strategy-ai-exec: edge must be > 0"));
        }
        if self.qty.raw() <= 0 {
            return Err(StrategyError::Config("strategy-ai-exec: qty must be > 0"));
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // §5.4: stale ⇒ every AI quote is pulled.
        if self.is_stale(ctx.now_ns()) {
            return;
        }
        if let Some(bidx) = self.book.index_of(tick.sym) {
            let _ = self.book.apply_at(bidx, tick);
            self.maybe_quote(bidx as usize, ctx);
            return;
        }
        // Untracked symbol: claim a book slot only when the AI has
        // published a (upsert-live) fair entry for it. TTL is not
        // consulted here — a dead-but-refreshable entry still earns
        // its slot; `quotable()` gates the actual quoting.
        if !matches!(self.fair_get(tick.sym), Some(e) if e.live) {
            return;
        }
        match self.book.track(tick.sym) {
            Ok(bidx) => {
                let _ = self.book.apply_at(bidx, tick);
                self.maybe_quote(bidx as usize, ctx);
            }
            Err(_) => {
                self.book_track_failed = self.book_track_failed.wrapping_add(1);
            }
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    /// §7 consumption: `SetFairValue`/`SetBias` upsert the fair
    /// table; `OrderIntent` is honored when not stale; every frame
    /// (any kind) restores liveness. See the module docs for the
    /// silence-window ordering.
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        self.ai_frames_seen = self.ai_frames_seen.wrapping_add(1);
        // Decision 1: the frame's ts_ns is the engine-monotonic
        // accept stamp — the TTL and staleness base.
        let frame_ns = cmd.ts_ns;
        let stale_before = self.last_frame_ns == 0
            || frame_ns.saturating_sub(self.last_frame_ns) > AI_STALENESS_NS;
        if stale_before {
            // A silence window closes with this frame: flagged
            // entries die permanently (no-op before the first frame
            // ever — the table is empty).
            self.expire_silent_entries();
        }
        match cmd.kind() {
            Some(AiCmdKind::SetFairValue) => {
                if let Some(i) = self.fair_slot_for(cmd.sym, frame_ns) {
                    let e = &mut self.fair[i];
                    e.px_1e6 = cmd.px;
                    e.has_fair = true;
                    // bias_1e6 deliberately untouched — preserved.
                    e.set_ns = frame_ns;
                    e.ttl_ns = cmd.ttl_ns;
                    e.expire_on_silence = cmd.flags & AI_CMD_FLAG_EXPIRE_ON_SILENCE != 0;
                    e.live = true;
                    self.fair_sets = self.fair_sets.wrapping_add(1);
                }
            }
            Some(AiCmdKind::SetBias) => {
                if let Some(i) = self.fair_slot_for(cmd.sym, frame_ns) {
                    let e = &mut self.fair[i];
                    e.bias_1e6 = cmd.px;
                    e.set_ns = frame_ns;
                    e.ttl_ns = cmd.ttl_ns;
                    e.expire_on_silence = cmd.flags & AI_CMD_FLAG_EXPIRE_ON_SILENCE != 0;
                    e.live = true;
                    self.bias_sets = self.bias_sets.wrapping_add(1);
                }
            }
            Some(AiCmdKind::OrderIntent) => {
                if stale_before {
                    // §5.4: refuse intents while stale. This frame
                    // still restores liveness below — the worker's
                    // heartbeat-precedes-payload rule exists so this
                    // branch never fires in a well-behaved session.
                    self.intents_refused_stale = self.intents_refused_stale.wrapping_add(1);
                } else {
                    self.honor_intent(cmd, ctx);
                }
            }
            // Heartbeat / SetParam / RulesetStage / RulesetCommit /
            // Enable / Disable / Halt: liveness only. (Set-consumed
            // kinds never reach a member inside the set; SetParam
            // ids for ai-exec do not exist in 8f.)
            _ => {}
        }
        self.last_frame_ns = frame_ns;
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, AI_SIDE_NONE, STRATEGY_SLOT_AI_EXEC, STRATEGY_SLOT_NONE};

    struct TestCtx {
        now: NsTs,
        submitted: Vec<Order>,
        next_err: Option<SubmitErr>,
    }

    impl TestCtx {
        fn new() -> Self {
            Self {
                now: 0,
                submitted: Vec::new(),
                next_err: None,
            }
        }
    }

    impl Ctx for TestCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            if let Some(e) = self.next_err.take() {
                return Err(e);
            }
            self.submitted.push(o);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    const PM: SymbolId = make_symbol_id(VenueId::Polymarket, 7);

    /// 1 s base "now" for tests; every synthetic stamp builds on it.
    const T0: u64 = 1_000_000_000;
    /// Comfortably past the 15 s staleness window.
    const SILENCE: u64 = AI_STALENESS_NS + 1_000;

    fn heartbeat(ts: u64) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn set_fair(ts: u64, sym: SymbolId, px: i64, ttl: u64, eos: bool) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            px,
            0,
            ttl,
            AiCmdKind::SetFairValue,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            if eos { AI_CMD_FLAG_EXPIRE_ON_SILENCE } else { 0 },
        )
    }

    fn set_bias(ts: u64, sym: SymbolId, bias: i64, ttl: u64) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            bias,
            0,
            ttl,
            AiCmdKind::SetBias,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn intent(ts: u64, sym: SymbolId, px: i64, qty: i64, side: Side, venue: VenueId) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            px,
            qty,
            1_000_000_000,
            AiCmdKind::OrderIntent,
            venue,
            STRATEGY_SLOT_AI_EXEC,
            side as u8,
            0,
            0,
        )
    }

    fn tick(sym: SymbolId, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            1,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    /// Strategy with a live fair(500_000, ttl 60 s) for PM, cooldown
    /// off, ctx clock just past the fair's stamp.
    fn primed() -> (AiExec<8>, TestCtx) {
        let mut s: AiExec<8> = AiExec::new();
        s.set_cooldown_ns(0);
        let mut c = TestCtx::new();
        c.now = T0 + 10;
        s.on_start(&mut c).unwrap();
        s.on_ai(&set_fair(T0, PM, 500_000, 60_000_000_000, false), &mut c);
        (s, c)
    }

    // ---------------- on_start ----------------

    #[test]
    fn on_start_ok_with_defaults() {
        let mut s: AiExec<8> = AiExec::new();
        assert!(s.on_start(&mut TestCtx::new()).is_ok());
    }

    #[test]
    fn on_start_rejects_bad_params() {
        let mut s: AiExec<8> = AiExec::new();
        s.set_edge_1e6(0);
        assert!(matches!(
            s.on_start(&mut TestCtx::new()),
            Err(StrategyError::Config(_))
        ));
        let mut s: AiExec<8> = AiExec::new();
        s.set_qty(Qty::from_raw(0));
        assert!(matches!(
            s.on_start(&mut TestCtx::new()),
            Err(StrategyError::Config(_))
        ));
    }

    // ---------------- deviation quoting ----------------

    #[test]
    fn quotes_ask_when_market_rich() {
        let (mut s, mut c) = primed();
        // mid 700_000 vs fair 500_000 → dev +200_000 ≥ edge 20_000.
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        let o = c.submitted[0];
        assert_eq!(o.sym, PM);
        assert_eq!(o.side, Side::Ask);
        assert_eq!(o.px.raw(), 700_000);
        assert_eq!(o.venue, VenueId::Polymarket as u8);
        assert_eq!(s.orders_emitted, 1);
    }

    #[test]
    fn quotes_bid_when_market_cheap() {
        let (mut s, mut c) = primed();
        s.on_tick(&tick(PM, 290_000, 310_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        assert_eq!(c.submitted[0].side, Side::Bid);
    }

    #[test]
    fn no_quote_under_edge() {
        let (mut s, mut c) = primed();
        // mid 505_000 → dev 5_000 < edge 20_000.
        s.on_tick(&tick(PM, 500_000, 510_000), &mut c);
        assert!(c.submitted.is_empty());
    }

    #[test]
    fn bias_shifts_the_target() {
        let (mut s, mut c) = primed();
        // bias +150_000 → target 650_000; mid 700_000 → dev 50_000.
        s.on_ai(&set_bias(T0 + 1, PM, 150_000, 60_000_000_000), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        assert_eq!(c.submitted[0].side, Side::Ask);

        // bias +190_000 → target 690_000; dev 10_000 < edge → silent.
        let (mut s, mut c) = primed();
        s.on_ai(&set_bias(T0 + 1, PM, 190_000, 60_000_000_000), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert!(c.submitted.is_empty());
    }

    #[test]
    fn nonpositive_target_never_quotes() {
        let (mut s, mut c) = primed();
        // fair 500_000 + bias −600_000 → target −100_000 ⇒ skip.
        s.on_ai(&set_bias(T0 + 1, PM, -600_000, 60_000_000_000), &mut c);
        s.on_tick(&tick(PM, 290_000, 310_000), &mut c);
        assert!(c.submitted.is_empty());
    }

    #[test]
    fn bias_only_entry_never_quotes() {
        let mut s: AiExec<8> = AiExec::new();
        s.set_cooldown_ns(0);
        let mut c = TestCtx::new();
        c.now = T0 + 10;
        s.on_ai(&set_bias(T0, PM, 100_000, 60_000_000_000), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert!(c.submitted.is_empty(), "no fair value → no quoting");
        // Fair arrives → quoting starts, bias applied (target 600_000,
        // mid 700_000 → dev 100_000).
        s.on_ai(&set_fair(T0 + 1, PM, 500_000, 60_000_000_000, false), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        assert_eq!(c.submitted[0].side, Side::Ask);
    }

    #[test]
    fn cooldown_suppresses_duplicate_quotes() {
        let (mut s, mut c) = primed();
        s.set_cooldown_ns(1_000);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        c.now += 100; // within cooldown
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
        c.now += 1_000; // past cooldown
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 2);
    }

    #[test]
    fn ring_full_counts_dropped() {
        let (mut s, mut c) = primed();
        c.next_err = Some(SubmitErr::RingFull);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert!(c.submitted.is_empty());
        assert_eq!(s.orders_dropped, 1);
        assert_eq!(s.orders_emitted, 0);
        // Cooldown untouched → next tick emits.
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);
    }

    #[test]
    fn tick_without_fair_entry_is_ignored() {
        let mut s: AiExec<8> = AiExec::new();
        let mut c = TestCtx::new();
        c.now = T0;
        // Liveness present, but no entry for this sym.
        s.on_ai(&heartbeat(T0 - 10), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert!(c.submitted.is_empty());
        assert_eq!(s.book_track_failed, 0);
        assert!(s.fair_snapshot(PM).is_none());
    }

    // ---------------- fair-table TTL (§11 row) ----------------

    #[test]
    fn fair_entry_ttl_expires_quoting() {
        let (mut s, mut c) = primed(); // fair set at T0, ttl 60 s
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1, "TTL alive → quote");

        // Keep liveness fresh but let the ENTRY TTL lapse.
        c.now = T0 + 61_000_000_000;
        s.on_ai(&heartbeat(c.now - 5), &mut c);
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1, "TTL dead → quote pulled");

        // A fresh SetFairValue revives the same slot.
        s.on_ai(&set_fair(c.now, PM, 500_000, 60_000_000_000, false), &mut c);
        c.now += 10;
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 2);
    }

    // ---------------- staleness (§11 row: 15 s pull) ----------------

    #[test]
    fn staleness_pulls_quotes_and_recovers() {
        let (mut s, mut c) = primed();
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1);

        // Worker silent past 15 s: quotes pulled even though the
        // entry TTL (60 s) is alive.
        c.now = T0 + SILENCE;
        assert!(s.is_stale(c.now));
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1, "stale → no quoting");

        // Any valid frame recovers; the unflagged entry resumes.
        s.on_ai(&heartbeat(c.now), &mut c);
        c.now += 10;
        assert!(!s.is_stale(c.now));
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 2, "recovered → quoting resumes");
    }

    #[test]
    fn stale_before_any_frame_ever() {
        let s: AiExec<8> = AiExec::new();
        assert!(s.is_stale(T0));
    }

    // ---------------- expire_on_silence (§11 row) ----------------

    #[test]
    fn expire_on_silence_kills_flagged_entries_permanently() {
        let mut s: AiExec<8> = AiExec::new();
        s.set_cooldown_ns(0);
        let mut c = TestCtx::new();
        c.now = T0 + 10;
        let pm2: SymbolId = make_symbol_id(VenueId::Polymarket, 8);
        // PM flagged, pm2 unflagged; both TTL 10 min.
        s.on_ai(&set_fair(T0, PM, 500_000, 600_000_000_000, true), &mut c);
        s.on_ai(&set_fair(T0, pm2, 500_000, 600_000_000_000, false), &mut c);
        assert_eq!(s.fair_len(), 2);

        // Silence window, then recovery heartbeat.
        let t_rec = T0 + SILENCE;
        s.on_ai(&heartbeat(t_rec), &mut c);
        assert_eq!(s.silence_expired, 1, "only the flagged entry dies");
        assert_eq!(s.fair_len(), 1);
        assert!(!s.fair_snapshot(PM).unwrap().live);
        assert!(s.fair_snapshot(pm2).unwrap().live);

        // Flagged entry stays dead after recovery; unflagged quotes.
        c.now = t_rec + 10;
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert!(c.submitted.is_empty(), "silence-expired entry is gone");
        s.on_tick(&tick(pm2, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 1, "unflagged entry survived");

        // A fresh SetFairValue revives the flagged symbol.
        s.on_ai(&set_fair(t_rec + 20, PM, 500_000, 600_000_000_000, true), &mut c);
        c.now = t_rec + 30;
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(c.submitted.len(), 2);
    }

    // ---------------- OrderIntent paper flow (§11 row) ----------------

    #[test]
    fn intent_honored_when_live() {
        let (mut s, mut c) = primed();
        s.on_ai(
            &intent(T0 + 5, PM, 430_000, 2_000_000, Side::Bid, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_honored, 1);
        assert_eq!(c.submitted.len(), 1);
        let o = c.submitted[0];
        assert_eq!(o.sym, PM);
        assert_eq!(o.side, Side::Bid);
        assert_eq!(o.px.raw(), 430_000);
        assert_eq!(o.qty.raw(), 2_000_000);
        assert_eq!(o.venue, VenueId::Polymarket as u8);
    }

    #[test]
    fn intent_refused_while_stale_but_restores_liveness() {
        let mut s: AiExec<8> = AiExec::new();
        let mut c = TestCtx::new();
        c.now = T0;
        // First frame ever is an intent → refused (fail-safe; the
        // worker's heartbeat would normally have landed first).
        s.on_ai(
            &intent(T0, PM, 430_000, 2_000_000, Side::Ask, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_refused_stale, 1);
        assert!(c.submitted.is_empty());
        // …but the frame restored liveness: the next intent lands.
        s.on_ai(
            &intent(T0 + 5, PM, 430_000, 2_000_000, Side::Ask, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_honored, 1);
        assert_eq!(c.submitted.len(), 1);
        assert_eq!(c.submitted[0].side, Side::Ask);
    }

    #[test]
    fn intent_after_silence_refused_then_honored_after_heartbeat() {
        let (mut s, mut c) = primed();
        let t1 = T0 + SILENCE;
        s.on_ai(
            &intent(t1, PM, 430_000, 2_000_000, Side::Bid, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_refused_stale, 1, "silence-ending intent refused");
        // Well-behaved worker: heartbeat precedes the retry.
        s.on_ai(&heartbeat(t1 + 1), &mut c);
        s.on_ai(
            &intent(t1 + 2, PM, 430_000, 2_000_000, Side::Bid, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_honored, 1);
        assert_eq!(c.submitted.len(), 1);
    }

    #[test]
    fn intent_ring_full_counts_dropped_not_honored() {
        let (mut s, mut c) = primed();
        c.next_err = Some(SubmitErr::RingFull);
        s.on_ai(
            &intent(T0 + 5, PM, 430_000, 2_000_000, Side::Bid, VenueId::Polymarket),
            &mut c,
        );
        assert_eq!(s.intents_honored, 0);
        assert_eq!(s.orders_dropped, 1);
    }

    // ---------------- fair table probe mechanics ----------------

    #[test]
    fn upsert_flag_is_last_writer_wins() {
        // Every upsert rewrites the entry policy (set_ns/ttl_ns AND
        // expire_on_silence) — a bias without the flag clears it.
        let (mut s, mut c) = primed();
        s.on_ai(&set_fair(T0 + 1, PM, 500_000, 60_000_000_000, true), &mut c);
        assert!(s.fair_snapshot(PM).unwrap().expire_on_silence);
        s.on_ai(&set_bias(T0 + 2, PM, 10_000, 60_000_000_000), &mut c);
        assert!(
            !s.fair_snapshot(PM).unwrap().expire_on_silence,
            "unflagged upsert clears the flag (last writer wins)"
        );
    }

    #[test]
    fn upsert_updates_in_place_and_preserves_bias() {
        let (mut s, mut c) = primed();
        s.on_ai(&set_bias(T0 + 1, PM, 50_000, 60_000_000_000), &mut c);
        s.on_ai(&set_fair(T0 + 2, PM, 400_000, 60_000_000_000, false), &mut c);
        let snap = s.fair_snapshot(PM).unwrap();
        assert_eq!(snap.px_1e6, 400_000);
        assert_eq!(snap.bias_1e6, 50_000, "SetFairValue preserves bias");
        assert_eq!(snap.set_ns, T0 + 2, "upsert refreshed the stamp");
        assert_eq!(s.fair_len(), 1, "same sym, same slot");
    }

    #[test]
    fn colliding_symbols_chain_and_stay_distinct() {
        // N=8 → syms 1, 9, 17 share home slot 1.
        let mut s: AiExec<8> = AiExec::new();
        let mut c = TestCtx::new();
        c.now = T0;
        for (i, sym) in [1u32, 9, 17].into_iter().enumerate() {
            s.on_ai(
                &set_fair(T0 + i as u64, sym, 100_000 + i as i64, 60_000_000_000, false),
                &mut c,
            );
        }
        assert_eq!(s.fair_len(), 3);
        assert_eq!(s.fair_snapshot(1).unwrap().px_1e6, 100_000);
        assert_eq!(s.fair_snapshot(9).unwrap().px_1e6, 100_001);
        assert_eq!(s.fair_snapshot(17).unwrap().px_1e6, 100_002);
    }

    #[test]
    fn dead_slot_reuse_keeps_chains_reachable() {
        let mut s: AiExec<8> = AiExec::new();
        let mut c = TestCtx::new();
        c.now = T0;
        // Chain on home slot 1: sym 1 (short TTL), then sym 9.
        s.on_ai(&set_fair(T0, 1, 100_000, 1_000, false), &mut c);
        s.on_ai(&set_fair(T0 + 1, 9, 200_000, 60_000_000_000, false), &mut c);
        // sym 1's TTL lapses; sym 17 reuses its slot.
        let t1 = T0 + 2_000;
        s.on_ai(&set_fair(t1, 17, 300_000, 60_000_000_000, false), &mut c);
        // sym 9 must still be reachable THROUGH the reused slot.
        assert_eq!(s.fair_snapshot(9).unwrap().px_1e6, 200_000);
        assert_eq!(s.fair_snapshot(17).unwrap().px_1e6, 300_000);
        // sym 1 was evicted (its slot re-keyed) — gone, not stale.
        assert!(s.fair_snapshot(1).is_none());
    }

    #[test]
    fn table_full_drops_and_counts() {
        let mut s: AiExec<4> = AiExec::new();
        let mut c = TestCtx::new();
        c.now = T0;
        for sym in 0u32..4 {
            s.on_ai(&set_fair(T0, sym, 100_000, 60_000_000_000, false), &mut c);
        }
        assert_eq!(s.fair_len(), 4);
        s.on_ai(&set_fair(T0 + 1, 99, 100_000, 60_000_000_000, false), &mut c);
        assert_eq!(s.fair_table_full, 1);
        assert!(s.fair_snapshot(99).is_none());
        // Existing entries untouched.
        assert_eq!(s.fair_len(), 4);
    }

    // ---------------- counters ----------------

    #[test]
    fn strategy_kind_and_counter_traits() {
        let (mut s, mut c) = primed();
        s.on_tick(&tick(PM, 690_000, 710_000), &mut c);
        assert_eq!(StrategyCounters::orders_emitted(&s), 1);
        assert_eq!(StrategyCounters::orders_dropped(&s), 0);
        assert_eq!(s.strategy_kind(), "ai-exec");
        assert_eq!(StrategyCounters::ai_enable_refused(&s), 0);
        assert_eq!(s.timer_period_ns(), u64::MAX);
    }
}
