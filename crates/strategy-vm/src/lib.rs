// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-vm
//!
//! The ruleset-VM strategy (Phase 8g, design §7): slot 5 of the
//! `StrategySet`. Evaluates the operator-committed [`RuleTable`] in
//! the engine hot path — zero alloc, zero `dyn`, compile-time
//! monomorphized like every other member.
//!
//! State is fully inline:
//!
//! * `[RuleTable; 2]` — active/staged **ping-pong** (§6). The engine's
//!   table-ring pop lands in the staged buffer via
//!   [`VmStrategy::receive_table`] (**documented copy #2**, 16 KiB+64
//!   by value, operator cadence; a later pop overwrites staged =
//!   engine-side restage-supersedes). The in-stream `RulesetCommit`
//!   flips the index — **no copy at flip**.
//! * `MultiBook<N>` for top-of-book. Symbols are tracked **lazily**:
//!   the first tick of a symbol the active table references (action
//!   OR reference leg) claims a book slot.
//! * Per-row cooldown stamps (`[u64; 256]`) — `CooldownGate`-style
//!   lazy `now_ns` compares, but per-row `horizon_ms` (the shared
//!   gate type carries one global cooldown, rows each carry their
//!   own), so the stamps live inline. No `on_timer` sweep (§7.1
//!   timer disabled): stamps are compared lazily at eval time.
//!
//! ## Trigger semantics (§7.1, D2 as amended)
//!
//! Both legs are venue-explicit via namespaced SymbolIds; `ctx.submit`
//! is venue-agnostic (the order's venue byte decodes from `row.sym`).
//! Every trigger requires a **two-sided book** (`bid_px > 0 &&
//! ask_px > 0`) on the action sym — and on the reference leg for
//! `cross_deviation` — so one-sided/preopen books (the 8e OKX lesson)
//! can never fire; the emit price is the action-sym mid (house
//! pattern: post-only at mid, like rule-tree and ai-exec).
//!
//! * `cross_deviation`: fire when `|mid(sym) − mid(ref)|` in basis
//!   points of `mid(ref)` reaches `edge_bps`
//!   (`|dev| × 10_000 ≥ edge_bps × mid(ref)`, i128 — overflow-free).
//!   Direction is mean-reverting, the ai-exec convention: sym rich
//!   (`dev > 0`) ⇒ `Ask`, sym cheap ⇒ `Bid`. `row.side` acts as a
//!   **filter**: a `bid`/`ask` row fires only when the computed
//!   direction matches; `both` takes either.
//! * `level_breach`: the row's side is the **emitted** side and the
//!   trigger watches the price you would transact at — `bid` rows
//!   fire when best ask ≤ `level_1e6` (buy at/below the level),
//!   `ask` rows fire when best bid ≥ `level_1e6` (sell at/above).
//!   "Crosses" is realized as level-attained + horizon re-arm: a
//!   fired row sleeps `horizon_ms`, so a level that HOLDS refires at
//!   most once per horizon. `both` checks the bid leg first, then the
//!   ask leg — at most one emission per row per tick, deterministic.
//!
//! A fired row re-arms after `horizon_ms`; the stamp is recorded
//! **only on an accepted submit** (`CooldownGate::record_emit`
//! doctrine — a ring-full reject leaves the row armed to retry).
//! Commit flips reset every stamp: a fresh table boots fully armed.
//!
//! ## Emit-time re-clamp (defense in depth; 8i replaces with RiskGate)
//!
//! Per order: `qty` is sized so `notional = px×qty/1e6 ≤
//! min(row.max_risk_1e6, $100)` — the row's own cap re-clamped
//! against the `docs/risk-policy.md` single-order cap, independently
//! of the §4.2 rule-7 validation (two enforcement layers by design; a
//! hand-built table that never saw the validator is still policy-
//! clamped). Because each row emits at most once per tick and each
//! order respects its row's cap, one evaluation pass emits at most
//! `Σ max_risk_1e6` per sym / per table — the rule-7-validated
//! budgets (≤ $250 / ≤ $1 000); the caps proptest pins exactly this
//! composition. The risk-policy per-symbol/total NET caps are
//! position caps: they need fill feedback and belong to 8i's
//! RiskGate (§15) — the engine's open-order caps bound the in-flight
//! count meanwhile.
//!
//! ## Inert states (§7.3)
//!
//! No table committed / `len == 0` / slot disabled at the set level ⇒
//! `on_tick` falls through on one predictable branch. Booting inert
//! under `--strategy all` is normal, not an error.
//!
//! ## `on_ai` (§6)
//!
//! Consumes `RulesetCommit` ONLY: reassembles the identity via
//! [`AiCmd::ruleset_hash128`] (THE shared helper — same code path as
//! the ingress-ai side path), compares against the staged buffer's
//! `hash128`; match ⇒ index flip, mismatch/no-staged ⇒ drop +
//! `commits_dropped`. `RulesetStage` is deliberately ignored (a
//! side-path concern), as is every other kind — fair values and
//! biases stay ai-exec's domain, and per D3 (a) the committed table
//! persists through worker silence: the vm tracks no liveness.
//!
//! Hot path: one `len == 0` branch when inert; else one `MultiBook`
//! apply + a linear row scan (≤ 256 contiguous 64 B rows,
//! `get_unchecked` inside safe wrappers). Zero alloc after boot —
//! gate 36 in `bench/tests/alloc_assertions.rs`.

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
    symbol_venue_byte, AiCmd, AiCmdKind, Fill, Order, Price, Qty, RuleRow, RuleTable, Side, Signal,
    SymbolId, Tick, VenueId, RULE_TABLE_ROWS,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

/// `docs/risk-policy.md` "max single-order notional" (×1e6). The
/// §4.2 validator holds the mirror constant on the ingress side
/// (`ingress-ai::RULE_ROW_MAX_RISK_1E6`) — two INDEPENDENT
/// enforcement layers by design (defense in depth; the risk-reviewer
/// subagent keeps the doc and both code sites in sync).
// Operator ruling 2026-08-29: $50k-book research tier (per-order $10k).
pub const POLICY_SINGLE_ORDER_CAP_1E6: i64 = 10_000_000_000;

const ORDER_KIND_POST_ONLY: u8 = 0;

/// The ruleset-VM strategy. `N` sizes the book table (it must cover
/// every distinct action + reference leg the active table can name —
/// the set instantiates `VmStrategy<512>`: 256 rows × 2 legs).
pub struct VmStrategy<const N: usize> {
    /// Active/staged ping-pong (§6). `active & 1` indexes the live
    /// table; the other slot is the staging target.
    tables: [RuleTable; 2],
    active: u8,
    /// The staging buffer holds a table received since the last flip.
    staged_valid: bool,

    book: MultiBook<N>,

    /// Per-row cooldown stamps (ns of the last ACCEPTED emit; 0 =
    /// armed). Row-indexed into the ACTIVE table; reset on flip.
    last_fire_ns: [u64; RULE_TABLE_ROWS],

    next_oid: u64,

    /// Rows evaluated (passed the `sym` filter) across all ticks.
    pub evals: u64,
    /// Rows whose trigger (incl. the side constraint) fired — the §9
    /// pre-clamp counter.
    pub fires: u64,
    /// Orders accepted by the dispatcher.
    pub orders_emitted: u64,
    /// Orders rejected by the dispatcher (ring full).
    pub orders_dropped: u64,
    /// In-stream Commits that matched the staged hash and flipped.
    pub commits_applied: u64,
    /// In-stream Commits dropped: no staged table or hash mismatch.
    pub commits_dropped: u64,
    /// Referenced symbols that could not claim a book slot (`N`
    /// exhausted) — evaluation for them stays off, fail closed.
    pub book_track_failed: u64,
}

impl<const N: usize> VmStrategy<N> {
    /// Construct the inert strategy (no table, empty book). Boot-only.
    pub fn new() -> Self {
        Self {
            tables: [RuleTable::EMPTY; 2],
            active: 0,
            staged_valid: false,
            book: MultiBook::empty(),
            last_fire_ns: [0; RULE_TABLE_ROWS],
            next_oid: 1,
            evals: 0,
            fires: 0,
            orders_emitted: 0,
            orders_dropped: 0,
            commits_applied: 0,
            commits_dropped: 0,
            book_track_failed: 0,
        }
    }

    /// Active-table row count (0 = inert). §9 `engine_vm_rows_active`.
    #[inline]
    pub fn rows_active(&self) -> u32 {
        self.tables[(self.active & 1) as usize].len
    }

    /// Active-table epoch (0 = none ever). §9 `engine_vm_table_epoch`.
    #[inline]
    pub fn active_epoch(&self) -> u32 {
        self.tables[(self.active & 1) as usize].epoch
    }

    /// Active-table identity (all-zero = none ever committed).
    #[inline]
    pub fn active_hash128(&self) -> [u8; 16] {
        self.tables[(self.active & 1) as usize].hash128
    }

    /// Identity of the staged (received, not yet committed) table.
    #[inline]
    pub fn staged_hash128(&self) -> Option<[u8; 16]> {
        if self.staged_valid {
            Some(self.tables[((self.active & 1) ^ 1) as usize].hash128)
        } else {
            None
        }
    }

    /// §6 copy #2 target: the engine's table-ring pop hands the
    /// popped slot here (item 7 wires the call; gate 35 exercises it
    /// directly). Copies the table into the staging buffer —
    /// **documented copy #2** (16 KiB + 64 by value, operator
    /// cadence, moves bytes never the heap). A later call overwrites
    /// the staged table: the engine-side restage-supersedes mirror.
    pub fn receive_table(&mut self, table: &RuleTable) {
        let sidx = ((self.active & 1) ^ 1) as usize;
        self.tables[sidx] = *table;
        if self.tables[sidx].len as usize > RULE_TABLE_ROWS {
            // Unreachable through the §4.2 validator; clamping here
            // is what upholds the hot loop's `get_unchecked` bound at
            // the single mutation entry point (safe-wrapper doctrine).
            debug_assert!(false, "received table len exceeds RULE_TABLE_ROWS");
            self.tables[sidx].len = RULE_TABLE_ROWS as u32;
        }
        self.staged_valid = true;
    }

    /// Does the active table reference `sym` on either leg? Decides
    /// lazy book tracking for first-sighted symbols.
    #[inline(always)]
    fn table_references(&self, sym: SymbolId, alen: usize) -> bool {
        let ai = (self.active & 1) as usize;
        let mut i = 0usize;
        while i < alen {
            // SAFETY: `i < alen ≤ len`, and `receive_table` clamps
            // every stored table's `len` to `RULE_TABLE_ROWS`.
            let r = unsafe { self.tables.get_unchecked(ai).rows.get_unchecked(i) };
            if r.sym == sym || r.ref_sym == sym {
                return true;
            }
            i += 1;
        }
        false
    }
}

impl<const N: usize> Default for VmStrategy<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StrategyCounters for VmStrategy<N> {
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
        "vm"
    }
}

impl<const N: usize> Strategy for VmStrategy<N> {
    /// Nothing to allocate, nothing to validate (§7.1): tables are
    /// inline fields that arrive later via the ring; per-row
    /// parameters were validated by §4.2. Always `Ok` — booting inert
    /// is normal (§7.3), so there is no failure mode by design.
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        let ai = (self.active & 1) as usize;
        let alen = self.tables[ai].len as usize;
        if alen == 0 {
            // §7.3: inert — one predictable branch.
            return;
        }

        // Book refresh; lazily track symbols the table references.
        let bidx = match self.book.index_of(tick.sym) {
            Some(b) => {
                let _ = self.book.apply_at(b, tick);
                b as usize
            }
            None => {
                if !self.table_references(tick.sym, alen) {
                    return;
                }
                match self.book.track(tick.sym) {
                    Ok(b) => {
                        let _ = self.book.apply_at(b, tick);
                        b as usize
                    }
                    Err(_) => {
                        self.book_track_failed = self.book_track_failed.wrapping_add(1);
                        return;
                    }
                }
            }
        };

        // Action-sym top: every trigger requires a two-sided book
        // (module docs — the 8e one-sided/preopen lesson).
        let top = self.book.slots()[bidx];
        if !top.has_quotes() || top.bid_px.raw() <= 0 || top.ask_px.raw() <= 0 {
            return;
        }
        let mid_s = top.mid().raw();
        let now = ctx.now_ns();

        // Row scan: linear over `len` with a `sym` filter (§7.1).
        let mut i = 0usize;
        while i < alen {
            // SAFETY: `i < alen ≤ len`, and `receive_table` clamps
            // every stored table's `len` to `RULE_TABLE_ROWS`.
            let row: RuleRow = unsafe { *self.tables.get_unchecked(ai).rows.get_unchecked(i) };
            if row.sym != tick.sym {
                i += 1;
                continue;
            }
            self.evals = self.evals.wrapping_add(1);

            // Cooldown: lazy stamp compare (no on_timer sweep).
            let horizon_ns = (row.horizon_ms as u64).wrapping_mul(1_000_000);
            // SAFETY: `i < alen ≤ RULE_TABLE_ROWS` — the stamp array
            // is RULE_TABLE_ROWS long.
            let last = unsafe { *self.last_fire_ns.get_unchecked(i) };
            if now < last.saturating_add(horizon_ns) {
                i += 1;
                continue;
            }

            // Trigger math (module docs).
            let fire_side: Option<Side> = if row.trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
                match self.book.snapshot(row.ref_sym) {
                    Some(rt) if rt.has_quotes() && rt.bid_px.raw() > 0 && rt.ask_px.raw() > 0 => {
                        let mid_r = rt.mid().raw();
                        let dev = mid_s as i128 - mid_r as i128;
                        let abs_dev = if dev >= 0 { dev } else { -dev };
                        if abs_dev * 10_000 >= (row.edge_bps as i128) * (mid_r as i128) {
                            let s = if dev > 0 { Side::Ask } else { Side::Bid };
                            if row.side == RuleRow::SIDE_BOTH || row.side == s as u8 {
                                Some(s)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            } else if row.trigger == RuleRow::TRIGGER_LEVEL_BREACH {
                let bid_leg = top.ask_px.raw() <= row.level_1e6; // buy at/below
                let ask_leg = top.bid_px.raw() >= row.level_1e6; // sell at/above
                if row.side == Side::Bid as u8 {
                    if bid_leg {
                        Some(Side::Bid)
                    } else {
                        None
                    }
                } else if row.side == Side::Ask as u8 {
                    if ask_leg {
                        Some(Side::Ask)
                    } else {
                        None
                    }
                } else if row.side == RuleRow::SIDE_BOTH {
                    // Bid leg first — deterministic, at most one per tick.
                    if bid_leg {
                        Some(Side::Bid)
                    } else if ask_leg {
                        Some(Side::Ask)
                    } else {
                        None
                    }
                } else {
                    // Unreachable through the §4.2 validator.
                    debug_assert!(false, "invalid row side byte");
                    None
                }
            } else {
                // Unreachable through the §4.2 validator.
                debug_assert!(false, "invalid row trigger byte");
                None
            };
            let side = match fire_side {
                Some(s) => s,
                None => {
                    i += 1;
                    continue;
                }
            };
            self.fires = self.fires.wrapping_add(1);

            // Emit-time re-clamp (module docs): row cap ∧ policy
            // single-order cap. `mid_s > 0` by the two-sided guard;
            // i128 keeps the products exact; floor division
            // guarantees notional ≤ allowed.
            let mut allowed = row.max_risk_1e6;
            if allowed > POLICY_SINGLE_ORDER_CAP_1E6 {
                allowed = POLICY_SINGLE_ORDER_CAP_1E6;
            }
            if allowed <= 0 {
                // A non-positive row cap cannot pass the §4.2
                // validator; hand-built tables fail closed here.
                i += 1;
                continue;
            }
            let qty_1e6 = ((allowed as i128 * 1_000_000) / mid_s as i128) as i64;
            if qty_1e6 <= 0 {
                // Price too high for the cap to buy any quantity —
                // fired but nothing to emit (fires − emitted −
                // dropped surfaces this).
                i += 1;
                continue;
            }

            let venue = match VenueId::from_u8(symbol_venue_byte(row.sym)) {
                Some(v) => v,
                None => {
                    // Rows are validated against the boot universe —
                    // an undecodable venue byte cannot happen.
                    debug_assert!(false, "table row with undecodable venue");
                    i += 1;
                    continue;
                }
            };
            let order = Order::new(
                now,
                venue,
                row.sym,
                side,
                ORDER_KIND_POST_ONLY,
                Price::from_raw(mid_s),
                Qty::from_raw(qty_1e6),
                self.next_oid,
            );
            self.next_oid = self.next_oid.wrapping_add(1);
            match ctx.submit(order) {
                Ok(()) => {
                    self.orders_emitted = self.orders_emitted.wrapping_add(1);
                    // SAFETY: `i < alen ≤ RULE_TABLE_ROWS` (stamp array).
                    unsafe {
                        *self.last_fire_ns.get_unchecked_mut(i) = now;
                    }
                }
                Err(SubmitErr::RingFull) => {
                    // Cooldown stays open — the row retries on the
                    // next tick (CooldownGate doctrine).
                    self.orders_dropped = self.orders_dropped.wrapping_add(1);
                }
            }
            i += 1;
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, _fill: &Fill, _ctx: &mut C) {}

    /// §6 flip consumer — see the module docs. Everything except
    /// `RulesetCommit` (Stage included) is deliberately ignored.
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, _ctx: &mut C) {
        if !matches!(cmd.kind(), Some(AiCmdKind::RulesetCommit)) {
            return;
        }
        let sidx = ((self.active & 1) ^ 1) as usize;
        if self.staged_valid && self.tables[sidx].hash128 == cmd.ruleset_hash128() {
            // Flip: index swap, no copy (§6).
            self.active ^= 1;
            self.staged_valid = false;
            self.commits_applied = self.commits_applied.wrapping_add(1);
            // Fresh table ⇒ fully armed (module docs). Operator
            // cadence — a plain indexed loop.
            let mut i = 0usize;
            while i < RULE_TABLE_ROWS {
                self.last_fire_ns[i] = 0;
                i += 1;
            }
        } else {
            self.commits_dropped = self.commits_dropped.wrapping_add(1);
        }
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, _now_ns: NsTs, _ctx: &mut C) {}

    /// Disabled (§7.1): cooldown stamps are compared lazily.
    #[inline(always)]
    fn timer_period_ns(&self) -> u64 {
        u64::MAX
    }

    fn on_stop<C: Ctx>(&mut self, _ctx: &mut C) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{fnv1a_64, AI_SIDE_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE};

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

    /// Action leg (raw Polymarket-style id — venue byte 0).
    const SYM: SymbolId = 42;
    /// Reference leg.
    const REF: SymbolId = 7;
    const HASH_A: [u8; 16] = [0xAB; 16];
    const HASH_B: [u8; 16] = [0xCD; 16];
    /// Production-like base clock: fresh stamps (0) arm only once
    /// `now ≥ horizon_ns` (the CooldownGate first-window semantic —
    /// wallclock ns exceeds every horizon at boot; synthetic test
    /// clocks must too, so T0 > the 86_400_000 ms max horizon).
    const T0: u64 = 100_000_000_000_000_000;

    fn cd_row(side: u8, edge_bps: u32, horizon_ms: u32, max_risk_1e6: i64) -> RuleRow {
        RuleRow::new(
            SYM,
            REF,
            edge_bps,
            horizon_ms,
            0,
            max_risk_1e6,
            fnv1a_64(b"cd"),
            RuleRow::TRIGGER_CROSS_DEVIATION,
            side,
            0,
        )
    }

    fn lb_row(side: u8, level_1e6: i64, horizon_ms: u32, max_risk_1e6: i64) -> RuleRow {
        RuleRow::new(
            SYM,
            SYMBOL_ID_NONE,
            0,
            horizon_ms,
            level_1e6,
            max_risk_1e6,
            fnv1a_64(b"lb"),
            RuleRow::TRIGGER_LEVEL_BREACH,
            side,
            1,
        )
    }

    fn table_with(rows: &[RuleRow], epoch: u32, hash128: [u8; 16]) -> RuleTable {
        let mut t = RuleTable::EMPTY;
        let mut i = 0;
        while i < rows.len() {
            t.rows[i] = rows[i];
            i += 1;
        }
        t.len = rows.len() as u32;
        t.epoch = epoch;
        t.hash128 = hash128;
        t
    }

    fn commit_cmd(hash128: [u8; 16]) -> AiCmd {
        let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            1,
            1,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            AiCmdKind::RulesetCommit,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn stage_cmd(hash128: [u8; 16]) -> AiCmd {
        let mut c = commit_cmd(hash128);
        c.kind = AiCmdKind::RulesetStage as u8;
        c
    }

    /// Receive + commit `rows` as epoch-1 table `HASH_A`.
    fn install(vm: &mut VmStrategy<8>, ctx: &mut TestCtx, rows: &[RuleRow]) {
        vm.receive_table(&table_with(rows, 1, HASH_A));
        vm.on_ai(&commit_cmd(HASH_A), ctx);
        assert_eq!(vm.commits_applied, 1);
    }

    fn tick(sym: SymbolId, seq: u32, bid: i64, ask: i64) -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            seq,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    fn notional_1e6(o: &Order) -> i64 {
        ((o.px.raw() as i128 * o.qty.raw() as i128) / 1_000_000) as i64
    }

    // ---------------- lifecycle / inert (§7.3) ----------------

    #[test]
    fn on_start_ok_and_inert_without_table() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        assert!(vm.on_start(&mut ctx).is_ok());
        assert_eq!(vm.rows_active(), 0);
        vm.on_tick(&tick(SYM, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0, "inert vm must not evaluate");
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.timer_period_ns(), u64::MAX);
    }

    #[test]
    fn zero_len_committed_table_is_inert() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[]);
        assert_eq!(vm.rows_active(), 0);
        vm.on_tick(&tick(SYM, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- commit flip (§6) ----------------

    #[test]
    fn receive_then_commit_flips() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        assert_eq!(vm.staged_hash128(), Some(HASH_A));
        assert_eq!(vm.rows_active(), 0, "staged table must not evaluate yet");
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.rows_active(), 1);
        assert_eq!(vm.active_epoch(), 1);
        assert_eq!(vm.active_hash128(), HASH_A);
        assert_eq!(vm.staged_hash128(), None, "flip consumes the staged buffer");
    }

    #[test]
    fn commit_without_staged_drops() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_dropped, 1);
        assert_eq!(vm.commits_applied, 0);
        assert_eq!(vm.rows_active(), 0);
    }

    #[test]
    fn commit_hash_mismatch_drops_and_keeps_staged() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_dropped, 1);
        assert_eq!(
            vm.staged_hash128(),
            Some(HASH_A),
            "mismatch drops the COMMIT, not the staged table"
        );
        // The correct commit still lands.
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.active_hash128(), HASH_A);
    }

    #[test]
    fn stage_and_other_kinds_are_ignored() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.on_ai(&stage_cmd(HASH_A), &mut ctx);
        let mut hb = commit_cmd(HASH_A);
        hb.kind = AiCmdKind::Heartbeat as u8;
        vm.on_ai(&hb, &mut ctx);
        assert_eq!(vm.commits_applied, 0);
        assert_eq!(vm.commits_dropped, 0);
        assert_eq!(vm.staged_hash128(), Some(HASH_A));
    }

    #[test]
    fn restage_supersedes_staged_buffer() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table(&table_with(
            &[lb_row(0, 12_000, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.receive_table(&table_with(
            &[lb_row(1, 900_000, 1_000, 3_000_000)],
            2,
            HASH_B,
        ));
        assert_eq!(vm.staged_hash128(), Some(HASH_B));
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_dropped, 1, "superseded hash must not commit");
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_applied, 1);
        assert_eq!(vm.active_epoch(), 2);
    }

    #[test]
    fn ping_pong_reuses_both_buffers_across_two_flips() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(&mut vm, &mut ctx, &[lb_row(0, 12_000, 1_000, 3_000_000)]);
        vm.receive_table(&table_with(
            &[lb_row(1, 900_000, 1_000, 4_000_000)],
            2,
            HASH_B,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        assert_eq!(vm.commits_applied, 2);
        assert_eq!(vm.active_epoch(), 2);
        assert_eq!(vm.active_hash128(), HASH_B);
        // Third receive lands in the buffer the FIRST table used.
        vm.receive_table(&table_with(
            &[lb_row(0, 12_000, 1_000, 5_000_000)],
            3,
            HASH_A,
        ));
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        assert_eq!(vm.commits_applied, 3);
        assert_eq!(vm.active_epoch(), 3);
    }

    // ---------------- cross_deviation triggers ----------------

    /// Ref mid 500_000; a later SYM tick supplies the action mid.
    fn prime_cd(vm: &mut VmStrategy<8>, ctx: &mut TestCtx, side: u8, edge_bps: u32) {
        install(vm, ctx, &[cd_row(side, edge_bps, 1_000, 3_000_000)]);
        ctx.now = T0;
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), ctx);
        assert!(
            ctx.submitted.is_empty(),
            "ref tick evaluates no action rows"
        );
        assert_eq!(vm.evals, 0, "ref leg has no action rows");
    }

    #[test]
    fn cross_deviation_fires_ask_when_sym_rich() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 80);
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
        let o = &ctx.submitted[0];
        assert_eq!(o.sym, SYM);
        assert_eq!(o.side, Side::Ask);
        assert_eq!(o.px.raw(), 700_000);
        assert_eq!(o.venue, VenueId::Polymarket as u8);
    }

    #[test]
    fn cross_deviation_fires_bid_when_sym_cheap() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 80);
        vm.on_tick(&tick(SYM, 1, 290_000, 310_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn cross_deviation_edge_boundary_is_inclusive() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // edge 400 bps of ref mid 500_000 = 20_000 raw.
        prime_cd(&mut vm, &mut ctx, RuleRow::SIDE_BOTH, 400);
        // dev 19_999 < 20_000 ⇒ silent.
        vm.on_tick(&tick(SYM, 1, 509_999, 529_999), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.fires, 0);
        // dev exactly 20_000 ⇒ fire (≥, design-literal).
        vm.on_tick(&tick(SYM, 2, 510_000, 530_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
    }

    #[test]
    fn cross_deviation_side_filter_blocks_mismatched_direction() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // Bid-only row: a rich (Ask-direction) deviation must not fire.
        prime_cd(&mut vm, &mut ctx, Side::Bid as u8, 80);
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.fires, 0, "side filter is part of the trigger");
        assert!(ctx.submitted.is_empty());
        // The matching (cheap ⇒ Bid) direction fires.
        vm.on_tick(&tick(SYM, 2, 290_000, 310_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn cross_deviation_without_ref_book_never_fires() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[cd_row(RuleRow::SIDE_BOTH, 80, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // No REF tick ever: action ticks evaluate but cannot fire.
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.evals, 1);
        assert_eq!(vm.fires, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- level_breach triggers ----------------

    #[test]
    fn level_breach_bid_fires_at_or_below_level() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // Ask above the level ⇒ silent.
        vm.on_tick(&tick(SYM, 1, 11_000, 13_000), &mut ctx);
        assert_eq!(vm.fires, 0);
        // Ask at the level ⇒ buy.
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(ctx.submitted[0].side, Side::Bid);
        assert_eq!(ctx.submitted[0].px.raw(), 11_000, "emits at mid");
    }

    #[test]
    fn level_breach_ask_fires_at_or_above_level() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Ask as u8, 900_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 880_000, 895_000), &mut ctx);
        assert_eq!(vm.fires, 0);
        vm.on_tick(&tick(SYM, 2, 900_000, 910_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(ctx.submitted[0].side, Side::Ask);
    }

    #[test]
    fn level_breach_both_prefers_the_bid_leg() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(RuleRow::SIDE_BOTH, 450_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // Crossed fixture: ask 400k ≤ level ≤ bid 500k — both legs
        // satisfied; the bid leg wins deterministically.
        vm.on_tick(&tick(SYM, 1, 500_000, 400_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert_eq!(
            ctx.submitted.len(),
            1,
            "at most one emission per row per tick"
        );
        assert_eq!(ctx.submitted[0].side, Side::Bid);
    }

    #[test]
    fn one_sided_book_never_fires() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 500_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        // Ask side empty (0): a naive `ask ≤ level` would fire — the
        // two-sided guard must hold it (8e preopen lesson).
        vm.on_tick(&tick(SYM, 1, 400_000, 0), &mut ctx);
        assert_eq!(vm.fires, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- cooldown / re-arm ----------------

    #[test]
    fn cooldown_rearm_after_horizon() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 100, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        // Within the horizon: the armed check blocks before trigger
        // math — the sleeping row does not even fire.
        ctx.now += 50_000_000;
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(vm.fires, 1, "a sleeping row does not fire");
        // At the horizon boundary: re-armed.
        ctx.now += 50_000_000;
        vm.on_tick(&tick(SYM, 3, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
        assert_eq!(vm.fires, 2);
    }

    #[test]
    fn flip_resets_cooldown_stamps() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // Day-long horizon: without a flip the row stays asleep.
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        ctx.now += 1_000_000;
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1, "asleep for a day");
        // Recommit (same rows, new epoch/hash) ⇒ fresh arms.
        vm.receive_table(&table_with(
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
            2,
            HASH_B,
        ));
        vm.on_ai(&commit_cmd(HASH_B), &mut ctx);
        ctx.now += 1_000_000;
        vm.on_tick(&tick(SYM, 3, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2, "flip re-arms every row");
    }

    // ---------------- emit-time clamp ----------------

    #[test]
    fn per_order_notional_clamped_to_row_cap() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 600_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        let o = &ctx.submitted[0];
        // mid 500_000: qty = 3e6 × 1e6 / 5e5 = 6e6; notional = 3e6.
        assert_eq!(o.qty.raw(), 6_000_000);
        assert_eq!(notional_1e6(o), 3_000_000);
    }

    #[test]
    fn handbuilt_table_exceeding_policy_cap_is_still_policy_clamped() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // $50,000 row cap — the §4.2 validator would reject this
        // table; built by hand it proves the emit-time layer stands
        // alone (defense in depth): every order ≤ the $10k policy cap
        // (operator ruling 2026-08-29, $50k research tier).
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 600_000, 10, 50_000_000_000)],
        );
        ctx.now = T0;
        let mut seq = 0u32;
        for _ in 0..3 {
            ctx.now += 100_000_000;
            seq += 1;
            vm.on_tick(&tick(SYM, seq, 480_000, 520_000), &mut ctx);
        }
        assert_eq!(ctx.submitted.len(), 3);
        for o in &ctx.submitted {
            assert_eq!(notional_1e6(o), POLICY_SINGLE_ORDER_CAP_1E6);
        }
    }

    #[test]
    fn nonpositive_row_cap_fires_but_emits_nothing() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // A zero cap cannot pass the validator; hand-built it must
        // fail closed at the clamp (fires visible, no order).
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 600_000, 10, 0)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(vm.fires, 1);
        assert!(ctx.submitted.is_empty());
        assert_eq!(
            vm.orders_dropped, 0,
            "clamp-to-zero is not a dispatcher drop"
        );
    }

    #[test]
    fn one_pass_sym_emission_is_bounded_by_the_table_sym_budget() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        // Two $2 rows on the same sym: one tick fires both; the
        // pass's Σ notional ≤ Σ row caps (the §4.2 rule-7 budget).
        let mut a = lb_row(Side::Bid as u8, 600_000, 86_400_000, 2_000_000);
        a.name_h = fnv1a_64(b"a");
        let mut b = lb_row(Side::Bid as u8, 550_000, 86_400_000, 2_000_000);
        b.name_h = fnv1a_64(b"b");
        install(&mut vm, &mut ctx, &[a, b]);
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 480_000, 520_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 2);
        let pass_total: i64 = ctx.submitted.iter().map(notional_1e6).sum();
        assert!(pass_total <= 4_000_000, "Σ per pass ≤ table sym budget");
        assert_eq!(notional_1e6(&ctx.submitted[0]), 2_000_000);
        assert_eq!(notional_1e6(&ctx.submitted[1]), 2_000_000);
    }

    #[test]
    fn ring_full_drops_and_leaves_cooldown_open() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 86_400_000, 3_000_000)],
        );
        ctx.now = T0;
        ctx.next_err = Some(SubmitErr::RingFull);
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert!(ctx.submitted.is_empty());
        assert_eq!(vm.orders_dropped, 1);
        // Cooldown untouched ⇒ the very next tick emits.
        vm.on_tick(&tick(SYM, 2, 10_000, 12_000), &mut ctx);
        assert_eq!(ctx.submitted.len(), 1);
        assert_eq!(vm.orders_emitted, 1);
    }

    // ---------------- book handling ----------------

    #[test]
    fn irrelevant_sym_claims_no_book_slot_and_evaluates_nothing() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[cd_row(RuleRow::SIDE_BOTH, 80, 1_000, 3_000_000)],
        );
        vm.on_tick(&tick(999, 1, 400_000, 420_000), &mut ctx);
        assert_eq!(vm.evals, 0);
        assert_eq!(vm.book_track_failed, 0);
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn book_capacity_exhaustion_fails_closed_and_counts() {
        // N = 1: the ref leg claims the only slot; the action sym
        // cannot track ⇒ counted, no eval, no panic.
        let mut vm: VmStrategy<1> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        vm.receive_table(&table_with(
            &[cd_row(RuleRow::SIDE_BOTH, 80, 1_000, 3_000_000)],
            1,
            HASH_A,
        ));
        vm.on_ai(&commit_cmd(HASH_A), &mut ctx);
        vm.on_tick(&tick(REF, 1, 490_000, 510_000), &mut ctx);
        vm.on_tick(&tick(SYM, 1, 690_000, 710_000), &mut ctx);
        assert_eq!(vm.book_track_failed, 1);
        assert_eq!(vm.evals, 0);
        assert!(ctx.submitted.is_empty());
    }

    // ---------------- counters surface ----------------

    #[test]
    fn strategy_counters_trait_surface() {
        let mut vm: VmStrategy<8> = VmStrategy::new();
        let mut ctx = TestCtx::new();
        install(
            &mut vm,
            &mut ctx,
            &[lb_row(Side::Bid as u8, 12_000, 1_000, 3_000_000)],
        );
        ctx.now = T0;
        vm.on_tick(&tick(SYM, 1, 10_000, 12_000), &mut ctx);
        assert_eq!(StrategyCounters::orders_emitted(&vm), 1);
        assert_eq!(StrategyCounters::orders_dropped(&vm), 0);
        assert_eq!(vm.strategy_kind(), "vm");
        assert_eq!(StrategyCounters::ai_enable_refused(&vm), 0);
    }
}
