// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # strategy-set
//!
//! Runtime composition of the in-tree strategies (Phase 8f, design
//! §7; §13 decision 3 made this its own crate so `strategy-core`
//! stays dependency-clean).
//!
//! [`StrategySet`] owns one statically-composed member per built
//! slot and fans every engine callback out to the members whose
//! enable bit is set — one predictable branch per member per event,
//! fully monomorphized, no `dyn`, no allocation after boot.
//!
//! ## Slot map (wire-stable — `AiCmd::strategy_id`)
//!
//! | slot | member | status |
//! |---|---|---|
//! | 0 | `strategy-latency-arb` | built |
//! | 1 | `strategy-ev` | built |
//! | 2 | `strategy-cross-arb` | built |
//! | 3 | `strategy-rule-tree` | built |
//! | 4 | `strategy-ai-exec` | built (item 8) |
//! | 5 | `strategy-vm` | built (8g item 6) |
//! | 6 | `strategy-icdp` | built (ICDP I4, 2026-09-03) — configured only when `~/multivenue/icdp.toml` resolves |
//!
//! Slot 7 is the only reserved value: no member exists behind it, no
//! bit constant is defined (the cli cannot express it via
//! [`mask_for_name`]), and an `EnableStrategy` targeting it is
//! refused (counted).
//!
//! ## AI command routing (`on_ai`, §7)
//!
//! * `EnableStrategy` — **refused while halted** (sticky), refused
//!   for reserved/unknown slots; otherwise sets the bit. Every
//!   refusal increments the counter behind
//!   `engine_ai_enable_refused_total` (both refusal causes share it —
//!   the capture stream disambiguates offline).
//! * `DisableStrategy` — **always honored** (halted or not).
//! * `HaltRequest` — sticky: clears the entire enable mask and
//!   refuses all future enables. There is deliberately no Resume
//!   command on the wire — recovery is a manual engine restart
//!   (docs/risk-policy.md). 8i replaces this set-local flag with the
//!   real risk state machine.
//! * Everything else (`SetFairValue`/`SetBias`/`SetParam`/
//!   `OrderIntent`/`Heartbeat`/`RulesetStage`/`RulesetCommit`) fans
//!   out to enabled members. `strategy-ai-exec` (slot 4) consumes
//!   fair-table upserts, paper intents, and frame-derived liveness;
//!   `strategy-vm` (slot 5) consumes `RulesetCommit` — the generic
//!   fan-out delivers `RulesetStage` to it too and vm ignores it by
//!   design (staging is the ingress side path's state machine, 8g
//!   §6/§8); the other members inherit the default no-op `on_ai`.
//!   No set-level `SetParam` ids are defined (none for vm in v1
//!   either) — §7's "SetParam(set-level)" clause activates when one
//!   exists.
//!
//! ## Boot semantics
//!
//! [`StrategySet::new`] takes the initial enable mask (`--strategy`,
//! see [`mask_for_name`]). The cli configures members through the
//! `*_mut` accessors, then the engine calls `on_start`, which is
//! forwarded ONLY to initially-enabled members — their config
//! validation stays as fail-fast as the single-strategy paths. A
//! member left out of the initial mask boots unvalidated and inert;
//! enabling it later via AI is safe for every in-tree member because
//! their `on_start` is pure validation (no state init) and an
//! unconfigured member simply never fires. Revisit this invariant if
//! a member ever gains a stateful `on_start`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

use core_regime::{
    RegimeErr, RegimeParams, RegimeState, SeedRow, RAW_ER, RAW_RET, RAW_RV, RAW_STRETCH,
};
use core_time::{NsTs, WallAnchor};
use core_types::regime::REL_UNKNOWN;
use core_types::{
    AiCmd, AiCmdKind, ChannelEvent, ChannelId, Fill, Order, RegimeLabelSet, RegimeWord,
    RuleTableV2, Signal, Tick, REGIME_OFF_HARD, REGIME_PROFILES, STRATEGY_SLOT_AI_EXEC,
    STRATEGY_SLOT_VM,
};
use strategy_ai_exec::AiExec;
use strategy_core::{
    Ctx, RegimeCounters, RegimeGate, Strategy, StrategyCounters, StrategyError, SubmitErr,
};
use strategy_cross_arb::CrossArb;
use strategy_ev::EvStrategy;
use strategy_icdp::IcdpStrategy;
use strategy_latency_arb::LatencyArb;
use strategy_rule_tree::RuleTree;
use strategy_vm::VmStrategy;

// ---------------------------------------------------------------
// Slot / mask constants
// ---------------------------------------------------------------

/// Slot index of the latency-arb member.
pub const SLOT_LATENCY_ARB: u8 = 0;
/// Slot index of the ev member.
pub const SLOT_EV: u8 = 1;
/// Slot index of the cross-arb member.
pub const SLOT_CROSS_ARB: u8 = 2;
/// Slot index of the rule-tree member.
pub const SLOT_RULE_TREE: u8 = 3;
/// Slot index of the ai-exec member (wire value pinned in
/// `core-types` — `OrderIntent` shape enforcement depends on it).
pub const SLOT_AI_EXEC: u8 = STRATEGY_SLOT_AI_EXEC;
/// Slot index of the vm member (wire value pinned in `core-types` —
/// `RulesetStage`/`RulesetCommit` shape enforcement depends on it).
pub const SLOT_VM: u8 = STRATEGY_SLOT_VM;
/// Slot index of the icdp member (ICDP I4; `docs/wire-format.md`
/// `Order.strategy_id` 6 = icdp).
pub const SLOT_ICDP: u8 = 6;

/// Enable-mask bit for the latency-arb member.
pub const BIT_LATENCY_ARB: u8 = 1 << SLOT_LATENCY_ARB;
/// Enable-mask bit for the ev member.
pub const BIT_EV: u8 = 1 << SLOT_EV;
/// Enable-mask bit for the cross-arb member.
pub const BIT_CROSS_ARB: u8 = 1 << SLOT_CROSS_ARB;
/// Enable-mask bit for the rule-tree member.
pub const BIT_RULE_TREE: u8 = 1 << SLOT_RULE_TREE;
/// Enable-mask bit for the ai-exec member (item 8).
pub const BIT_AI_EXEC: u8 = 1 << SLOT_AI_EXEC;
/// Enable-mask bit for the vm member (8g item 6).
pub const BIT_VM: u8 = 1 << SLOT_VM;
/// Enable-mask bit for the icdp member (ICDP I4).
pub const BIT_ICDP: u8 = 1 << SLOT_ICDP;

/// Every built member's bit (slots 0–6).
pub const BUILT_MASK: u8 =
    BIT_LATENCY_ARB | BIT_EV | BIT_CROSS_ARB | BIT_RULE_TREE | BIT_AI_EXEC | BIT_VM | BIT_ICDP;

/// Latency-arb slot capacity inside the set (design §7 sketch).
pub const SET_LATENCY_ARB_SLOTS: usize = 64;
/// Ev slot capacity inside the set (design §7 sketch).
pub const SET_EV_SLOTS: usize = 8;
/// Cross-arb group capacity inside the set.
pub const SET_CROSS_ARB_GROUPS: usize = 8;
/// Cross-arb per-group member capacity inside the set.
pub const SET_CROSS_ARB_MEMBERS: usize = 8;
/// Rule-tree slot capacity inside the set (design §7 sketch).
pub const SET_RULE_TREE_SLOTS: usize = 8;
/// Ai-exec capacity inside the set (design §7 sketch `AiExec<64>` —
/// sizes the fair table, book table and cooldown gate alike).
pub const SET_AI_EXEC_SLOTS: usize = 64;
// (VM2 V3: the vm's book generic is gone — mids live in the feature
// engine's fixed sym slots — so the old `SET_VM_SLOTS = 512` law
// retired with it.)

/// Map a `--strategy` value to an initial enable mask (design §7:
/// single name = single bit, back-compatible; `all` = all built
/// members). `None` for unknown names — the cli rejects those at
/// boot exactly as before.
pub fn mask_for_name(name: &str) -> Option<u8> {
    match name {
        "latency-arb" => Some(BIT_LATENCY_ARB),
        "ev" => Some(BIT_EV),
        "cross-arb" => Some(BIT_CROSS_ARB),
        "rule-tree" => Some(BIT_RULE_TREE),
        "ai-exec" => Some(BIT_AI_EXEC),
        "vm" => Some(BIT_VM),
        // AI-pushed lanes only (operator ruling 2026-09-02: Rust-coded
        // strategies disabled at boot; the engine executes only what
        // the AI command plane pushes — ai-exec intents + VM rulesets).
        "ai" => Some(BIT_AI_EXEC | BIT_VM),
        // ICDP I4: the operator opts the intrabar member in beside
        // the AI lanes (paper only — the wrapper refuses it without
        // `--paper`); `icdp` alone boots it bare.
        "icdp" => Some(BIT_ICDP),
        "ai+icdp" => Some(BIT_AI_EXEC | BIT_VM | BIT_ICDP),
        "all" => Some(BUILT_MASK),
        _ => None,
    }
}

// ---------------------------------------------------------------
// StrategySet
// ---------------------------------------------------------------

/// Statically-composed strategy set. See the module docs for slot
/// map, routing and boot semantics.
pub struct StrategySet {
    latency_arb: LatencyArb<SET_LATENCY_ARB_SLOTS>,
    ev: EvStrategy<SET_EV_SLOTS>,
    cross_arb: CrossArb<SET_CROSS_ARB_GROUPS, SET_CROSS_ARB_MEMBERS>,
    rule_tree: RuleTree<SET_RULE_TREE_SLOTS>,
    ai_exec: AiExec<SET_AI_EXEC_SLOTS>,
    vm: VmStrategy,
    icdp: IcdpStrategy,
    /// Runtime enable mask (bits per the slot map). Only built bits
    /// are ever set — Enable of a reserved slot is refused.
    enabled: u8,
    /// Initial mask as passed to [`Self::new`] — `on_start` validates
    /// exactly these members.
    initial: u8,
    /// Sticky halt flag (set-local until 8i). Once set, enables are
    /// refused until the process restarts.
    halted: bool,
    /// Refused `EnableStrategy` commands (halted or reserved/unknown
    /// slot). Mirrored to `engine_ai_enable_refused_total`.
    enable_refused: u64,
    /// RG2: the regime detector (boot-boxed; inert until
    /// [`Self::configure_regime`] — every word UNKNOWN, every gate open
    /// for the unconstrained members that exist today).
    regime: Box<RegimeState>,
    /// RG2: per-slot label sets, pulled from the members at configure
    /// time (or overridden from `regime.toml [labels.*]`).
    regime_labels: [RegimeLabelSet; 8],
    /// RG2: per-slot current gate.
    regime_gates: [RegimeGate; 8],
    /// RG2: `SetRegime` commands applied.
    regime_declared_total: u64,
    /// RG2: gate edges fanned out (`on_regime` calls).
    regime_gate_changes: u64,
}

/// RG2: the set's timer cadence once a detector is configured — the
/// regime rolls once per wall minute, the timer polls for the boundary.
pub const REGIME_TIMER_NS: u64 = 1_000_000_000;

impl StrategySet {
    /// Build the set with all members default-constructed and the
    /// given initial enable mask. Reserved/unknown bits in
    /// `initial_mask` are silently cleared to `BUILT_MASK` — the wire
    /// cannot express them and the cli builds masks via
    /// [`mask_for_name`], so anything else is caller error contained
    /// at boot. Boot-only.
    pub fn new(initial_mask: u8) -> Self {
        let m = initial_mask & BUILT_MASK;
        Self {
            latency_arb: LatencyArb::new(),
            ev: EvStrategy::new(),
            cross_arb: CrossArb::new(),
            rule_tree: RuleTree::new(),
            ai_exec: AiExec::new(),
            vm: VmStrategy::new(),
            icdp: IcdpStrategy::new(),
            enabled: m,
            initial: m,
            halted: false,
            enable_refused: 0,
            regime: RegimeState::new_boxed(),
            regime_labels: [RegimeLabelSet::ANY; 8],
            regime_gates: [RegimeGate::OPEN_UNKNOWN; 8],
            regime_declared_total: 0,
            regime_gate_changes: 0,
        }
    }

    // ---- RG2: regime detector (plan §4.2) ----------------------------

    /// Boot: install the detector's parameters (descriptors already
    /// resolved by the cli), anchor its minute clock at `now`, and pull
    /// every member's label. Refuses (detector untouched) on invalid
    /// params. Gates are re-judged on the next timer tick / seed.
    pub fn configure_regime(
        &mut self,
        params: &RegimeParams,
        anchor: WallAnchor,
        now: NsTs,
    ) -> Result<(), RegimeErr> {
        self.regime.configure(params, anchor, now)?;
        self.pull_regime_labels();
        // Fail-closed from the first instant: a labelled member stays
        // shut until the regime is known (seed or live warm-up).
        self.judge_gates_silently();
        Ok(())
    }

    /// Boot: seed the detector's rings (plan §4.3) and re-judge the
    /// gates at once — silently (no member callbacks: nothing has run
    /// yet; `on_start` members see the current gate through
    /// [`Self::regime_gate`]). Returns rows applied.
    pub fn seed_regime(&mut self, rows: &[SeedRow], now: NsTs) -> u32 {
        let applied = self.regime.seed(rows);
        let _ = self.regime.refresh_effective(now);
        self.judge_gates_silently();
        applied
    }

    /// Boot: override one coded member's label from
    /// `regime.toml [labels.<member>]`. `false` when the slot is not a
    /// coded member or the member cannot be relabelled.
    pub fn set_regime_label(&mut self, slot: u8, set: RegimeLabelSet) -> bool {
        let ok = match slot {
            SLOT_LATENCY_ARB => self.latency_arb.set_regime_label(set),
            SLOT_EV => self.ev.set_regime_label(set),
            SLOT_CROSS_ARB => self.cross_arb.set_regime_label(set),
            SLOT_RULE_TREE => self.rule_tree.set_regime_label(set),
            SLOT_AI_EXEC => self.ai_exec.set_regime_label(set),
            SLOT_ICDP => self.icdp.set_regime_label(set),
            _ => false,
        };
        if ok {
            self.regime_labels[slot as usize] = set;
        }
        ok
    }

    /// The detector (cli: boot tells, `/state`; tests).
    #[inline]
    pub fn regime(&self) -> &RegimeState {
        &self.regime
    }

    /// The current gate of `slot` (open for unconstrained members).
    #[inline]
    pub fn regime_gate(&self, slot: u8) -> RegimeGate {
        if slot < 8 {
            self.regime_gates[slot as usize]
        } else {
            RegimeGate::OPEN_UNKNOWN
        }
    }

    /// The label set of `slot`.
    #[inline]
    pub fn regime_label_of(&self, slot: u8) -> RegimeLabelSet {
        if slot < 8 {
            self.regime_labels[slot as usize]
        } else {
            RegimeLabelSet::ANY
        }
    }

    fn pull_regime_labels(&mut self) {
        self.regime_labels[SLOT_LATENCY_ARB as usize] = self.latency_arb.regime_label();
        self.regime_labels[SLOT_EV as usize] = self.ev.regime_label();
        self.regime_labels[SLOT_CROSS_ARB as usize] = self.cross_arb.regime_label();
        self.regime_labels[SLOT_RULE_TREE as usize] = self.rule_tree.regime_label();
        self.regime_labels[SLOT_AI_EXEC as usize] = self.ai_exec.regime_label();
        self.regime_labels[SLOT_VM as usize] = RegimeLabelSet::ANY; // rows gate themselves (RG3)
        self.regime_labels[SLOT_ICDP as usize] = self.icdp.regime_label();
        self.regime_labels[7] = RegimeLabelSet::ANY;
    }

    /// The gate verdict for `slot` on the current effective words.
    /// Coded members are not per-symbol: REL is judged as unknown,
    /// which the `regime.toml` grammar keeps unconstrained for them.
    #[inline]
    fn judge_slot(&self, slot: usize) -> RegimeGate {
        let mut eff = [RegimeWord::UNKNOWN; 4];
        let mut p = 0u8;
        while (p as usize) < REGIME_PROFILES {
            eff[p as usize] = self.regime.effective(p);
            p += 1;
        }
        let set = self.regime_labels[slot];
        let open = set.allows(eff[0], eff[1], REL_UNKNOWN, REL_UNKNOWN);
        RegimeGate::new(eff, open, set.off)
    }

    /// Re-judge every slot without member callbacks (boot / seed).
    fn judge_gates_silently(&mut self) {
        let mut slot = 0usize;
        while slot < 8 {
            self.regime_gates[slot] = self.judge_slot(slot);
            slot += 1;
        }
        self.push_vm_regime_view();
    }

    /// RG3: the set→vm seam — hand the vm the detector's current view
    /// (effective words + per-member REL) so its rows re-judge. Called
    /// on every minute roll, effective change and declaration
    /// regardless of slot 5's own (always-ANY) gate; never per tick.
    fn push_vm_regime_view(&mut self) {
        let view = self.regime.view();
        self.vm.set_regime_view(&view);
    }

    /// Re-judge every slot and fan `on_regime` out to the ENABLED
    /// members whose verdict flipped (edge-triggered; a disabled
    /// member's stored gate still updates and is delivered by
    /// [`Self::enable_slot`] when it comes back). The vm's rows judge
    /// themselves from the pushed view (RG3).
    fn refresh_gates<C: Ctx>(&mut self, ctx: &mut C) {
        let mut slot = 0usize;
        while slot < 8 {
            let next = self.judge_slot(slot);
            let prev = self.regime_gates[slot];
            self.regime_gates[slot] = next;
            if next.open != prev.open && self.enabled & (1u8 << slot) != 0 {
                self.regime_gate_changes = self.regime_gate_changes.wrapping_add(1);
                self.deliver_gate(slot as u8, next, ctx);
            }
            slot += 1;
        }
        self.push_vm_regime_view();
    }

    fn deliver_gate<C: Ctx>(&mut self, slot: u8, gate: RegimeGate, ctx: &mut C) {
        match slot {
            SLOT_LATENCY_ARB => self
                .latency_arb
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB)),
            SLOT_EV => self
                .ev
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_EV)),
            SLOT_CROSS_ARB => self
                .cross_arb
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB)),
            SLOT_RULE_TREE => self
                .rule_tree
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE)),
            SLOT_AI_EXEC => self
                .ai_exec
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC)),
            SLOT_VM => self
                .vm
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_VM)),
            SLOT_ICDP => self
                .icdp
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_ICDP)),
            _ => {}
        }
    }

    /// Current enable mask.
    #[inline]
    pub fn enabled_mask(&self) -> u8 {
        self.enabled
    }

    /// Sticky halt state.
    #[inline]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Refused enable count (mirrored to
    /// `engine_ai_enable_refused_total`).
    #[inline]
    pub fn enable_refused_total(&self) -> u64 {
        self.enable_refused
    }

    /// Configure the latency-arb member (boot-only).
    #[inline]
    pub fn latency_arb_mut(&mut self) -> &mut LatencyArb<SET_LATENCY_ARB_SLOTS> {
        &mut self.latency_arb
    }

    /// Configure the ev member (boot-only).
    #[inline]
    pub fn ev_mut(&mut self) -> &mut EvStrategy<SET_EV_SLOTS> {
        &mut self.ev
    }

    /// Configure the cross-arb member (boot-only).
    #[inline]
    pub fn cross_arb_mut(&mut self) -> &mut CrossArb<SET_CROSS_ARB_GROUPS, SET_CROSS_ARB_MEMBERS> {
        &mut self.cross_arb
    }

    /// Configure the rule-tree member (boot-only).
    #[inline]
    pub fn rule_tree_mut(&mut self) -> &mut RuleTree<SET_RULE_TREE_SLOTS> {
        &mut self.rule_tree
    }

    /// Configure the ai-exec member (boot-only).
    #[inline]
    pub fn ai_exec_mut(&mut self) -> &mut AiExec<SET_AI_EXEC_SLOTS> {
        &mut self.ai_exec
    }

    /// Read the vm member (§9 gauges read rows_active/epoch/hash
    /// through this in item 8; tests observe counters).
    #[inline]
    pub fn vm(&self) -> &VmStrategy {
        &self.vm
    }

    /// Mutate the vm member. The engine's table-ring pop (item 7)
    /// hands popped slots to `vm_mut().receive_table` — the §6
    /// copy-#2 seam; there is no boot config (§7.3: booting inert is
    /// normal).
    #[inline]
    pub fn vm_mut(&mut self) -> &mut VmStrategy {
        &mut self.vm
    }

    /// Read the icdp member (counters, params hash).
    #[inline]
    pub fn icdp(&self) -> &IcdpStrategy {
        &self.icdp
    }

    /// Configure the icdp member (boot-only: `configure` with the wall
    /// anchor + the resolved artifact).
    #[inline]
    pub fn icdp_mut(&mut self) -> &mut IcdpStrategy {
        &mut self.icdp
    }

    /// Set-level `EnableStrategy` handling. See module docs.
    #[inline]
    fn enable_slot(&mut self, slot: u8) {
        if self.halted {
            self.enable_refused = self.enable_refused.wrapping_add(1);
            return;
        }
        let bit = match slot {
            SLOT_LATENCY_ARB => BIT_LATENCY_ARB,
            SLOT_EV => BIT_EV,
            SLOT_CROSS_ARB => BIT_CROSS_ARB,
            SLOT_RULE_TREE => BIT_RULE_TREE,
            SLOT_AI_EXEC => BIT_AI_EXEC,
            SLOT_VM => BIT_VM,
            SLOT_ICDP => BIT_ICDP,
            // Reserved slot (7): no member behind it — refuse and
            // count.
            _ => {
                self.enable_refused = self.enable_refused.wrapping_add(1);
                return;
            }
        };
        self.enabled |= bit;
    }

    /// RG2: a member coming back through Enable receives its CURRENT
    /// gate (its own state may be stale from before the disable).
    #[inline]
    fn sync_gate_on_enable<C: Ctx>(&mut self, slot: u8, ctx: &mut C) {
        if slot < 8 && self.enabled & (1u8 << slot) != 0 {
            let gate = self.regime_gates[slot as usize];
            self.deliver_gate(slot, gate, ctx);
        }
    }

    /// Set-level `DisableStrategy` handling — always honored. Bits
    /// outside `BUILT_MASK` are never set, so clearing them is a
    /// no-op by construction.
    #[inline]
    fn disable_slot(&mut self, slot: u8) {
        if slot < 8 {
            self.enabled &= !(1u8 << slot);
        }
    }
}

impl StrategyCounters for StrategySet {
    #[inline]
    fn orders_emitted(&self) -> u64 {
        self.latency_arb.orders_emitted()
            + self.ev.orders_emitted()
            + self.cross_arb.orders_emitted()
            + self.rule_tree.orders_emitted()
            + self.ai_exec.orders_emitted()
            + self.vm.orders_emitted()
            + self.icdp.orders_emitted()
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.latency_arb.orders_dropped()
            + self.ev.orders_dropped()
            + self.cross_arb.orders_dropped()
            + self.rule_tree.orders_dropped()
            + self.ai_exec.orders_dropped()
            + self.vm.orders_dropped()
            + self.icdp.orders_dropped()
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "set"
    }
    #[inline]
    fn ai_enable_refused(&self) -> u64 {
        self.enable_refused
    }

    // ---- Phase 8g §9 family (cli 5 s mirror reads via UFCS) ------
    //
    // Same route as `ai_enable_refused`: set-level values cross the
    // generic engine boundary through these overrides — the loop
    // never names `StrategySet`. The vm rows isolate the vm member
    // (kind="vm" counters), NOT the set aggregates above.

    /// Live enable mask (`engine_strategy_enabled_mask`). Shadowed by
    /// the inherent u8 accessor on method-call syntax — see the trait
    /// docs; readers use UFCS.
    #[inline]
    fn enabled_mask(&self) -> u64 {
        u64::from(self.enabled)
    }
    #[inline]
    fn vm_rows_active(&self) -> u64 {
        u64::from(self.vm.rows_active())
    }
    #[inline]
    fn vm_table_epoch(&self) -> u64 {
        u64::from(self.vm.active_epoch())
    }
    #[inline]
    fn vm_fires(&self) -> u64 {
        self.vm.fires
    }
    #[inline]
    fn vm_orders_emitted(&self) -> u64 {
        self.vm.orders_emitted()
    }
    #[inline]
    fn vm_orders_dropped(&self) -> u64 {
        self.vm.orders_dropped()
    }
    #[inline]
    fn vm_commit_dropped(&self) -> u64 {
        self.vm.commits_dropped
    }
    #[inline]
    fn vm_regime_blocked(&self) -> u64 {
        self.vm.regime_blocked
    }
    #[inline]
    fn vm_regime_hard_exits(&self) -> u64 {
        self.vm.regime_hard_exits
    }
    #[inline]
    fn icdp_counters(&self) -> strategy_core::IcdpCounters {
        self.icdp.icdp_counters()
    }
    /// RG2: the detector's observables + per-slot gates.
    fn regime_counters(&self) -> RegimeCounters {
        let mut c = RegimeCounters::default();
        c.configured = u8::from(self.regime.is_configured());
        let mut p = 0u8;
        while (p as usize) < REGIME_PROFILES {
            let i = p as usize;
            c.measured[i] = self.regime.measured(p);
            c.declared[i] = self.regime.declared(p);
            c.effective[i] = self.regime.effective(p);
            c.declared_ts_ns[i] = self.regime.declared_ts(p);
            c.declared_ttl_ns[i] = self.regime.declared_ttl(p);
            let mut d = 0u8;
            while d < 8 {
                c.flips[i][d as usize] = self.regime.flips(p, d);
                d += 1;
            }
            c.disagree[i] = self.regime.disagree(p);
            let raw = self.regime.raw(p);
            c.raw[i] = [raw.ret_bps_1e9, raw.er_1e9, raw.rv_bps_1e9, raw.stretch_1e9];
            c.raw_present[i] = raw.present & (RAW_RET | RAW_ER | RAW_RV | RAW_STRETCH);
            p += 1;
        }
        let mut slot = 0usize;
        while slot < 8 {
            let g = self.regime_gates[slot];
            c.gates[slot] = if g.open {
                0
            } else if g.off == REGIME_OFF_HARD {
                2
            } else {
                1
            };
            slot += 1;
        }
        c.minutes_judged = self.regime.minutes_judged();
        c.seed_rows = u64::from(self.regime.seed_rows());
        c.declared_total = self.regime_declared_total;
        c.gate_changes = self.regime_gate_changes;
        c
    }
}

/// M4.1 M-c: per-member attribution adapter. Wraps the engine ctx for
/// exactly ONE member callback and stamps [`Order::strategy_id`] with
/// that member's slot before forwarding — members stay byte-untouched
/// and unaware; the engine stays set-agnostic. Monomorphized (`C:
/// Ctx`, no `dyn` — house rule); cost is one register write per
/// submit. Bare single-strategy boots bypass the set and therefore
/// submit unstamped (`STRATEGY_ID_NONE`) — recorded semantics
/// (docs/m4-progress.md M4.1).
pub struct StampCtx<'a, C: Ctx> {
    inner: &'a mut C,
    slot: u8,
}

impl<'a, C: Ctx> StampCtx<'a, C> {
    /// Wrap `inner` for the member occupying `slot`.
    #[inline(always)]
    pub fn new(inner: &'a mut C, slot: u8) -> Self {
        Self { inner, slot }
    }
}

impl<'a, C: Ctx> Ctx for StampCtx<'a, C> {
    #[inline(always)]
    fn submit(&mut self, mut order: Order) -> Result<(), SubmitErr> {
        order.strategy_id = self.slot;
        self.inner.submit(order)
    }
    #[inline(always)]
    fn now_ns(&self) -> NsTs {
        self.inner.now_ns()
    }
}

impl Strategy for StrategySet {
    /// Forward `on_start` to the initially-enabled members only —
    /// their validation is exactly as fail-fast as the standalone
    /// paths. See the module docs for why skipped members are safe.
    /// Every member callback in this impl goes through [`StampCtx`]
    /// (M4.1 M-c) so any submit carries its member's slot.
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        if self.initial & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB))?;
        }
        if self.initial & BIT_EV != 0 {
            self.ev.on_start(&mut StampCtx::new(&mut *ctx, SLOT_EV))?;
        }
        if self.initial & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB))?;
        }
        if self.initial & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE))?;
        }
        if self.initial & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC))?;
        }
        if self.initial & BIT_VM != 0 {
            self.vm.on_start(&mut StampCtx::new(&mut *ctx, SLOT_VM))?;
        }
        if self.initial & BIT_ICDP != 0 {
            self.icdp
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_ICDP))?;
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // RG2: the detector sees every fresh tick first (one probe +
        // one store for members, one probe for everything else).
        self.regime.on_tick(tick);
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// WS10-A: venue events fan out to enabled members exactly like
    /// ticks/signals — the member sees the same defaulted no-op until
    /// it opts in, and submits (if it ever does) are slot-stamped.
    #[inline(always)]
    fn on_venue_event<C: Ctx>(&mut self, event: &ChannelEvent, ctx: &mut C) {
        // RG2: the funding reference's prints feed the detector
        // (Funding on every venue; Hyperliquid rides AssetCtx, whose
        // `v0` is the rate — the vm feature engine's law).
        if event.sym == self.regime.params().fund_ref
            && self.regime.is_configured()
            && (event.channel == ChannelId::Funding as u8
                || event.channel == ChannelId::AssetCtx as u8)
        {
            self.regime.on_funding(event.v0, event.venue_time_ms);
        }
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// WS10-B: depth snapshots fan out to enabled members exactly
    /// like ticks/events — same mask gate, same slot stamping.
    #[inline(always)]
    fn on_depth<C: Ctx>(&mut self, depth: &core_types::DepthTopK, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// VM2 V2: options records fan out to enabled members exactly
    /// like depth — same mask gate, same slot stamping.
    #[inline(always)]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &core_types::OptSummary, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// Set-level routing per §7 (module docs), then fan-out.
    #[inline]
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        match cmd.kind() {
            Some(AiCmdKind::EnableStrategy) => {
                let before = self.enabled;
                self.enable_slot(cmd.strategy_id);
                if self.enabled != before {
                    self.sync_gate_on_enable(cmd.strategy_id, ctx);
                }
                return;
            }
            Some(AiCmdKind::DisableStrategy) => {
                self.disable_slot(cmd.strategy_id);
                return;
            }
            Some(AiCmdKind::HaltRequest) => {
                // Sticky kill-switch: nothing trades until a manual
                // restart. No Resume exists on the wire.
                self.halted = true;
                self.enabled = 0;
                return;
            }
            Some(AiCmdKind::SetRegime) => {
                // RG2 §4.4: set-level — declare one profile's word
                // (shape-checked upstream: SOURCE empty, ttl > 0,
                // profile < REGIME_PROFILES). Effective words and gates
                // re-judge at once; never fanned out. The declaration
                // is bounded by its TTL only (the expire-on-silence
                // flag is accepted on the wire and honoured by the
                // TTL law in RG2 — heartbeat-bound expiry is RG3+).
                let now = ctx.now_ns();
                self.regime.set_declared(
                    cmd.param_id as u8,
                    RegimeWord(cmd.px as u64),
                    now,
                    cmd.ttl_ns,
                );
                self.regime_declared_total = self.regime_declared_total.wrapping_add(1);
                if self.regime.refresh_effective(now) != 0 {
                    self.refresh_gates(ctx);
                }
                return;
            }
            // Unknown kinds cannot reach here (ingress + drain-site
            // shape checks), and every remaining kind fans out.
            _ => {}
        }
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm.on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// 8g §6 item 7: the engine's pre-AI-drain table pop lands here,
    /// forwarded to the slot-5 vm member
    /// ([`VmStrategy::receive_table`] — documented copy #2).
    /// Deliberately NOT mask-gated: staging is control plane and a
    /// staged table is inert until an in-stream `RulesetCommit`,
    /// which IS mask-gated through [`Strategy::on_ai`] — so an
    /// operator may stage while slot 5 is disabled and enable before
    /// committing without losing the table.
    #[inline]
    fn on_ruleset_table(&mut self, table: &RuleTableV2) {
        self.vm.receive_table_v2(table);
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        // RG2: roll the minute clock (nothing until a boundary) and
        // re-judge the gates only when an effective word changed.
        // RG3: a roll that changed no word may still have moved a
        // member's REL — the vm's `rel:` rows get the view anyway.
        let minutes_before = self.regime.minutes_judged();
        if self.regime.on_timer(now_ns) != 0 {
            self.refresh_gates(ctx);
        } else if self.regime.minutes_judged() != minutes_before {
            self.push_vm_regime_view();
        }
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        }
        if self.enabled & BIT_EV != 0 {
            self.ev
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_EV));
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_ICDP != 0 {
            self.icdp
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_ICDP));
        }
    }

    /// Minimum over the BUILT members (mask-independent so the
    /// engine's timer arming is stable across runtime Enable/Disable;
    /// `on_timer` itself fans out to enabled members only). All seven
    /// members currently return `u64::MAX` (disabled); a configured
    /// regime detector arms the 1 s [`REGIME_TIMER_NS`] poll.
    fn timer_period_ns(&self) -> u64 {
        let mut min = if self.regime.is_configured() {
            REGIME_TIMER_NS
        } else {
            u64::MAX
        };
        let v = self.latency_arb.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.ev.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.cross_arb.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.rule_tree.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.ai_exec.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.vm.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.icdp.timer_period_ns();
        if v < min {
            min = v;
        }
        min
    }

    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        // Stop is unconditional — even disabled members get the
        // teardown callback (they may hold capture-worthy state some
        // day; today all six are no-ops).
        self.latency_arb
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_LATENCY_ARB));
        self.ev.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_EV));
        self.cross_arb
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_CROSS_ARB));
        self.rule_tree
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_RULE_TREE));
        self.ai_exec
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        self.vm.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_VM));
        self.icdp.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_ICDP));
    }
}

// ---------------------------------------------------------------
// Tests (§11 rows: mask fan-out, enable-while-halted refused,
// disable always, initial mask)
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{
        make_symbol_id, Order, Price, Qty, SymbolId, VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE,
        SYMBOL_ID_NONE,
    };
    use strategy_core::SubmitErr;

    struct CountCtx {
        submitted: u32,
        now: NsTs,
    }

    impl Ctx for CountCtx {
        fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    fn ctx() -> CountCtx {
        CountCtx {
            submitted: 0,
            // Far past every cooldown window.
            now: 1_000_000_000_000,
        }
    }

    const PM: SymbolId = 11;
    const BN: SymbolId = 22;

    /// Set with the latency-arb member configured on (PM, BN) and
    /// cooldown 0 so every trigger emits.
    fn set_with_latency_arb(initial_mask: u8) -> StrategySet {
        let mut s = StrategySet::new(initial_mask);
        s.latency_arb_mut().add_pair(PM, BN).unwrap();
        s.latency_arb_mut().set_cooldown_ns(0);
        s
    }

    fn tick(venue: VenueId, sym: SymbolId, bid_1e6: i64, ask_1e6: i64) -> Tick {
        Tick::new(
            0,
            venue,
            sym,
            1,
            Price::from_raw(bid_1e6),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask_1e6),
            Qty::from_raw(1_000_000),
        )
    }

    /// Feed a Binance reference then a diverged Polymarket book —
    /// emits exactly one latency-arb order when the member is live.
    fn feed_trigger(s: &mut StrategySet, c: &mut CountCtx) {
        // BN mid = 500_000.
        s.on_tick(&tick(VenueId::Binance, BN, 490_000, 510_000), c);
        // PM mid = 400_000 → |delta| = 100_000 ≥ threshold 20_000.
        s.on_tick(&tick(VenueId::Polymarket, PM, 390_000, 410_000), c);
    }

    fn ai_cmd(kind: AiCmdKind, slot: u8) -> AiCmd {
        AiCmd::new(
            1,
            1,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            kind,
            VenueId::Ai,
            slot,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn initial_mask_from_names() {
        assert_eq!(mask_for_name("latency-arb"), Some(BIT_LATENCY_ARB));
        assert_eq!(mask_for_name("ev"), Some(BIT_EV));
        assert_eq!(mask_for_name("cross-arb"), Some(BIT_CROSS_ARB));
        assert_eq!(mask_for_name("rule-tree"), Some(BIT_RULE_TREE));
        assert_eq!(mask_for_name("ai-exec"), Some(BIT_AI_EXEC));
        assert_eq!(mask_for_name("vm"), Some(BIT_VM));
        assert_eq!(mask_for_name("ai"), Some(BIT_AI_EXEC | BIT_VM));
        assert_eq!(mask_for_name("all"), Some(BUILT_MASK));
        // `ai` = AI-pushed lanes only — NO Rust-coded strategy bit
        // (operator ruling 2026-09-02).
        const _: () = assert!(
            (BIT_AI_EXEC | BIT_VM) & BIT_LATENCY_ARB == 0,
            "`ai` excludes latency-arb"
        );
        // Const pins — checked at compile time (clippy: a runtime
        // `assert!` on consts folds away; this makes the pin official).
        const _: () = assert!(BUILT_MASK & BIT_AI_EXEC != 0, "`all` includes ai-exec");
        const _: () = assert!(BUILT_MASK & BIT_VM != 0, "`all` composes vm (8g item 6)");
        assert_eq!(BIT_VM, 1 << STRATEGY_SLOT_VM, "wire slot pinned");
        assert_eq!(mask_for_name("nope"), None);
        assert_eq!(mask_for_name(""), None);
    }

    #[test]
    fn new_clamps_reserved_bits_to_built_mask() {
        let s = StrategySet::new(0xFF);
        assert_eq!(s.enabled_mask(), BUILT_MASK);
        let s = StrategySet::new(0b1000_0000);
        assert_eq!(s.enabled_mask(), 0, "reserved bit 7 cleared");
        let s = StrategySet::new(BIT_ICDP);
        assert_eq!(s.enabled_mask(), BIT_ICDP, "slot 6 is built now (ICDP I4)");
        let s = StrategySet::new(BIT_AI_EXEC);
        assert_eq!(s.enabled_mask(), BIT_AI_EXEC, "slot 4 is built now");
        let s = StrategySet::new(BIT_VM);
        assert_eq!(s.enabled_mask(), BIT_VM, "slot 5 is built now (8g)");
    }

    #[test]
    fn on_start_validates_initially_enabled_members_only() {
        // bit0 enabled + configured → ok.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        assert!(s.on_start(&mut ctx()).is_ok());

        // bit0 enabled + NOT configured → the member's own
        // validation error propagates (fail-fast preserved).
        let mut s = StrategySet::new(BIT_LATENCY_ARB);
        assert!(matches!(
            s.on_start(&mut ctx()),
            Err(StrategyError::Config(_))
        ));

        // Unconfigured members outside the initial mask are skipped —
        // ev/cross/rule-tree would all fail validation here.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        assert!(s.on_start(&mut ctx()).is_ok());
    }

    #[test]
    fn mask_fan_out_gates_member_callbacks() {
        // Enabled: the trigger pair emits exactly one order.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 1);
        assert_eq!(s.orders_emitted(), 1);

        // Same feed with the bit off: nothing reaches the member.
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 0);
        assert_eq!(s.orders_emitted(), 0);
    }

    /// M4.1: ctx double that RECORDS submitted orders (attribution pin).
    struct RecordCtx {
        orders: Vec<Order>,
    }
    impl Ctx for RecordCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            self.orders.push(order);
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            0
        }
    }

    #[test]
    fn stamp_ctx_attributes_member_orders() {
        // Direct adapter law: the wrapped slot lands on the order.
        let mut rec = RecordCtx { orders: Vec::new() };
        let mut sc = StampCtx::new(&mut rec, SLOT_VM);
        let o = Order::new(
            0,
            VenueId::Polymarket,
            42,
            Side::Bid,
            0,
            Price::from_raw(1),
            Qty::from_raw(1),
            9,
        );
        assert_eq!(o.strategy_id, core_types::STRATEGY_ID_NONE);
        Ctx::submit(&mut sc, o).unwrap();
        assert_eq!(rec.orders.len(), 1);
        assert_eq!(rec.orders[0].strategy_id, SLOT_VM);
        assert_eq!(rec.orders[0].client_oid, 9, "everything else untouched");

        // Through the SET: the latency-arb trigger pair emits ONE
        // order stamped slot 0 by the dispatch wrapper (M-c) — the
        // member itself never saw the field.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut rec = RecordCtx { orders: Vec::new() };
        s.on_start(&mut rec).unwrap();
        s.on_tick(&tick(VenueId::Binance, BN, 490_000, 510_000), &mut rec);
        s.on_tick(&tick(VenueId::Polymarket, PM, 390_000, 410_000), &mut rec);
        assert_eq!(rec.orders.len(), 1);
        assert_eq!(rec.orders[0].strategy_id, SLOT_LATENCY_ARB);
    }

    #[test]
    fn enable_via_ai_activates_member() {
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_LATENCY_ARB), &mut c);
        assert_eq!(s.enabled_mask(), BIT_LATENCY_ARB);
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 1, "enabled-at-runtime member must fire");
    }

    #[test]
    fn disable_always_honored() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(
            &ai_cmd(AiCmdKind::DisableStrategy, SLOT_LATENCY_ARB),
            &mut c,
        );
        assert_eq!(s.enabled_mask(), 0);
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 0);

        // Disable also works while halted.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        s.on_ai(
            &ai_cmd(AiCmdKind::DisableStrategy, SLOT_LATENCY_ARB),
            &mut c,
        );
        assert_eq!(s.enabled_mask(), 0);
        assert!(s.is_halted());
    }

    #[test]
    fn halt_clears_mask_and_sticks() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        assert!(s.is_halted());
        assert_eq!(s.enabled_mask(), 0, "halt is a kill-switch: mask cleared");
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 0);
    }

    #[test]
    fn enable_while_halted_refused_and_counted() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_LATENCY_ARB), &mut c);
        assert_eq!(s.enabled_mask(), 0, "enable refused while halted");
        assert_eq!(s.enable_refused_total(), 1);
        assert_eq!(StrategyCounters::ai_enable_refused(&s), 1);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_EV), &mut c);
        assert_eq!(s.enable_refused_total(), 2);
    }

    /// Migrated from slot 5 in 8g item 6 (§8) and from slot 6 in ICDP
    /// I4: the only reserved slot is 7 (probed twice: reserved + an
    /// out-of-range id).
    #[test]
    fn enable_reserved_or_unknown_slot_refused() {
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, 7), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, 9), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        assert_eq!(s.enable_refused_total(), 2);
        assert!(!s.is_halted(), "reserved-slot refusal is not a halt");
        // Slot 6 enables (an unconfigured icdp member is inert: it
        // registers nothing and never fires).
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_ICDP), &mut c);
        assert_eq!(s.enabled_mask(), BIT_ICDP);
        assert_eq!(s.enable_refused_total(), 2);
    }

    #[test]
    fn enable_ai_exec_slot_is_honored() {
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(
            &ai_cmd(AiCmdKind::EnableStrategy, STRATEGY_SLOT_AI_EXEC),
            &mut c,
        );
        assert_eq!(s.enabled_mask(), BIT_AI_EXEC, "slot 4 is built in item 8");
        assert_eq!(s.enable_refused_total(), 0);
    }

    /// 8g item 6: the G0 demo probe `enable --strategy 5` now
    /// SUCCEEDS (§8 semantics change) — and Disable round-trips it.
    #[test]
    fn enable_vm_slot_round_trips() {
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, STRATEGY_SLOT_VM), &mut c);
        assert_eq!(s.enabled_mask(), BIT_VM, "slot 5 is built in 8g item 6");
        assert_eq!(s.enable_refused_total(), 0);
        s.on_ai(
            &ai_cmd(AiCmdKind::DisableStrategy, STRATEGY_SLOT_VM),
            &mut c,
        );
        assert_eq!(s.enabled_mask(), 0);
        assert!(!s.is_halted());
    }

    #[test]
    fn non_set_kinds_fan_out_without_side_effects() {
        // Heartbeat / SetFairValue reach members' default no-op
        // on_ai; the set itself must not change state.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        let hb = ai_cmd(AiCmdKind::Heartbeat, STRATEGY_SLOT_NONE);
        s.on_ai(&hb, &mut c);
        let fv = AiCmd::new(
            1,
            2,
            make_symbol_id(VenueId::Polymarket, 1),
            500_000,
            0,
            1_000,
            AiCmdKind::SetFairValue,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        );
        s.on_ai(&fv, &mut c);
        assert_eq!(s.enabled_mask(), BIT_LATENCY_ARB);
        assert!(!s.is_halted());
        assert_eq!(s.enable_refused_total(), 0);
        assert_eq!(c.submitted, 0);
    }

    #[test]
    fn counters_aggregate_members_and_kind_is_set() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_trigger(&mut s, &mut c);
        assert_eq!(s.orders_emitted(), 1);
        assert_eq!(s.orders_dropped(), 0);
        assert_eq!(s.strategy_kind(), "set");
    }

    #[test]
    fn timer_period_is_min_over_built_members() {
        let s = StrategySet::new(BUILT_MASK);
        // All six members currently disable their timers.
        assert_eq!(s.timer_period_ns(), u64::MAX);
    }

    // ------------- ai-exec member integration (item 8b) -------------

    use core_types::Side;

    fn fair_cmd(ts: u64, sym: SymbolId, px: i64) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            px,
            0,
            60_000_000_000,
            AiCmdKind::SetFairValue,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn intent_cmd(ts: u64, sym: SymbolId, px: i64, qty: i64) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            px,
            qty,
            1_000_000_000,
            AiCmdKind::OrderIntent,
            VenueId::Polymarket,
            STRATEGY_SLOT_AI_EXEC,
            Side::Bid as u8,
            0,
            0,
        )
    }

    #[test]
    fn set_fair_value_reaches_enabled_ai_exec() {
        let pm = make_symbol_id(VenueId::Polymarket, 3);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&fair_cmd(c.now - 100, pm, 500_000), &mut c);
        let snap = s.ai_exec_mut().fair_snapshot(pm).expect("entry upserted");
        assert_eq!(snap.px_1e6, 500_000);
        assert!(snap.live);
    }

    #[test]
    fn disabled_ai_exec_receives_nothing() {
        let pm = make_symbol_id(VenueId::Polymarket, 3);
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&fair_cmd(c.now - 100, pm, 500_000), &mut c);
        assert!(
            s.ai_exec_mut().fair_snapshot(pm).is_none(),
            "bit off → member never sees the frame"
        );
    }

    #[test]
    fn order_intent_paper_flow_through_set() {
        let pm = make_symbol_id(VenueId::Polymarket, 3);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        // Heartbeat precedes payload (§5.4) — restores liveness, so
        // the intent that follows is honored.
        let hb = ai_cmd(AiCmdKind::Heartbeat, STRATEGY_SLOT_NONE);
        s.on_ai(&hb, &mut c);
        s.on_ai(&intent_cmd(2, pm, 430_000, 2_000_000), &mut c);
        assert_eq!(c.submitted, 1, "intent submitted via ctx");
        assert_eq!(s.orders_emitted(), 1, "set aggregates ai-exec orders");
    }

    #[test]
    fn on_start_validates_ai_exec_when_initially_enabled() {
        let mut s = StrategySet::new(BIT_AI_EXEC);
        s.ai_exec_mut().set_edge_1e6(0);
        assert!(matches!(
            s.on_start(&mut ctx()),
            Err(StrategyError::Config(_))
        ));
        // Outside the initial mask the invalid member is skipped.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        s.ai_exec_mut().set_edge_1e6(0);
        assert!(s.on_start(&mut ctx()).is_ok());
    }

    // ------------- vm member integration (8g item 6) -------------

    use core_types::{fnv1a_64, RuleRow, RuleRowV2, RuleTableV2};

    /// Production-like clock for any test driving `VmStrategy` (G3
    /// lesson: fresh cooldown stamps (0) arm only once
    /// `now ≥ horizon_ns` — small synthetic clocks never clear the
    /// first window).
    const VM_T0: NsTs = 100_000_000_000_000_000;
    const VM_HASH_A: [u8; 16] = [0xAB; 16];
    const VM_HASH_B: [u8; 16] = [0xCD; 16];

    fn vm_ctx() -> CountCtx {
        CountCtx {
            submitted: 0,
            now: VM_T0,
        }
    }

    /// One cross_deviation row on the (PM, BN) pair the latency-arb
    /// fixtures already use; horizon 0 keeps every eval armed.
    fn vm_table(hash128: [u8; 16]) -> Box<RuleTableV2> {
        let mut t = Box::new(RuleTableV2::EMPTY);
        t.rows[0] = RuleRowV2::from_v1(&RuleRow::new(
            PM,
            BN,
            20,
            0,
            0,
            1_000_000,
            fnv1a_64(b"g4-set"),
            RuleRow::TRIGGER_CROSS_DEVIATION,
            RuleRow::SIDE_BOTH,
            0,
        ));
        t.len = 1;
        t.epoch = 1;
        t.hash128 = hash128;
        t
    }

    /// Ruleset Stage/Commit frame targeting slot 5 — hash128 rides
    /// the px/qty pair (`AiCmd::ruleset_hash128`).
    fn ruleset_cmd(kind: AiCmdKind, hash128: [u8; 16]) -> AiCmd {
        let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            1,
            1,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            kind,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    /// §8 happy path: `receive_table` through the seam, Commit
    /// through the set's generic `on_ai` fan-out ⇒ the flip lands,
    /// and the committed table fires through the set's `on_tick`
    /// fan-out (counters aggregate the vm member).
    #[test]
    fn ruleset_commit_fanout_reaches_vm() {
        let mut s = StrategySet::new(BIT_VM);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();

        s.vm_mut().receive_table_v2(&vm_table(VM_HASH_A));
        assert_eq!(s.vm().staged_hash128(), Some(VM_HASH_A));
        assert_eq!(s.vm().rows_active(), 0, "staged ≠ active");

        // Stage frames reach vm through the same fan-out and are
        // ignored by design (§8 — staging is the side path's state
        // machine).
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetStage, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_applied, 0);
        assert_eq!(s.vm().rows_active(), 0);

        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_applied, 1, "flip applied via set fan-out");
        assert_eq!(s.vm().rows_active(), 1);
        assert_eq!(s.vm().active_epoch(), 1);

        // The committed row fires through the set's on_tick fan-out:
        // BN ref then diverged PM book (2000 bps ≥ 20 bps edge).
        s.on_tick(&tick(VenueId::Binance, BN, 490_000, 510_000), &mut c);
        s.on_tick(&tick(VenueId::Polymarket, PM, 390_000, 410_000), &mut c);
        assert_eq!(c.submitted, 1, "vm order submitted via ctx");
        assert_eq!(s.orders_emitted(), 1, "set aggregates vm orders");
    }

    /// Failure legs: a Commit whose hash matches nothing staged is
    /// dropped (staged table survives for a later correct Commit),
    /// and a Commit with nothing staged at all is dropped too.
    #[test]
    fn ruleset_commit_mismatch_dropped_staged_survives() {
        let mut s = StrategySet::new(BIT_VM);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();

        // Nothing staged: dropped.
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_dropped, 1);

        // Staged HASH_A, committed HASH_B: dropped, staged survives.
        s.vm_mut().receive_table_v2(&vm_table(VM_HASH_A));
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_B), &mut c);
        assert_eq!(s.vm().commits_dropped, 2);
        assert_eq!(s.vm().commits_applied, 0);
        assert_eq!(s.vm().staged_hash128(), Some(VM_HASH_A), "staged survives");
        assert_eq!(s.vm().rows_active(), 0, "no flip");
    }

    /// Mask gating (§8): with bit 5 off the member never sees the
    /// frame — the Commit neither applies nor counts as dropped.
    #[test]
    fn disabled_vm_never_sees_commit() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();
        s.vm_mut().receive_table_v2(&vm_table(VM_HASH_A));
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_applied, 0, "bit off → frame never arrives");
        assert_eq!(s.vm().commits_dropped, 0);
        assert_eq!(s.vm().staged_hash128(), Some(VM_HASH_A));
    }

    /// 8g §9: the observability overrides surface live set/vm state
    /// through the `StrategyCounters` trait (UFCS — the cli's generic
    /// mirror route). Happy path: mask + the whole vm family after a
    /// stage → commit → fire cycle; the vm rows isolate the member
    /// from the set aggregate.
    #[test]
    fn observability_overrides_surface_vm_state() {
        let mut s = StrategySet::new(BIT_VM);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();
        assert_eq!(StrategyCounters::enabled_mask(&s), u64::from(BIT_VM));
        assert_eq!(StrategyCounters::vm_rows_active(&s), 0, "inert boot");
        assert_eq!(StrategyCounters::vm_table_epoch(&s), 0);

        // Mask reads move with enable/disable (the G0 demo gap: the
        // flip becomes directly observable, not order-flow-inferred).
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_LATENCY_ARB), &mut c);
        assert_eq!(
            StrategyCounters::enabled_mask(&s),
            u64::from(BIT_VM | BIT_LATENCY_ARB)
        );
        s.on_ai(
            &ai_cmd(AiCmdKind::DisableStrategy, SLOT_LATENCY_ARB),
            &mut c,
        );
        assert_eq!(StrategyCounters::enabled_mask(&s), u64::from(BIT_VM));

        // Mismatched Commit → vm_commit_dropped through the trait.
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_B), &mut c);
        assert_eq!(StrategyCounters::vm_commit_dropped(&s), 1);

        // Stage → Commit → tick-fire; every §9 row goes live.
        s.vm_mut().receive_table_v2(&vm_table(VM_HASH_A));
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        s.on_tick(&tick(VenueId::Binance, BN, 490_000, 510_000), &mut c);
        s.on_tick(&tick(VenueId::Polymarket, PM, 390_000, 410_000), &mut c);
        assert_eq!(StrategyCounters::vm_rows_active(&s), 1);
        assert_eq!(StrategyCounters::vm_table_epoch(&s), 1);
        assert_eq!(StrategyCounters::vm_fires(&s), 1);
        assert_eq!(StrategyCounters::vm_orders_emitted(&s), 1);
        assert_eq!(StrategyCounters::vm_orders_dropped(&s), 0);
        assert_eq!(
            s.vm().fires,
            StrategyCounters::vm_fires(&s),
            "trait == member"
        );
    }

    // ------------- engine table-pop seam (8g item 7) -------------

    /// Item-7 happy path: the engine's pop arrives via the
    /// `Strategy::on_ruleset_table` hook and lands in the vm member's
    /// staged buffer (§6 copy #2); a second delivery supersedes the
    /// first (engine-side restage mirror), and the in-stream Commit
    /// of the LAST delivery flips.
    #[test]
    fn on_ruleset_table_forwards_to_vm_seam() {
        let mut s = StrategySet::new(BIT_VM);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();

        Strategy::on_ruleset_table(&mut s, &vm_table(VM_HASH_A));
        assert_eq!(
            s.vm().staged_hash128(),
            Some(VM_HASH_A),
            "hook stages via the seam"
        );

        Strategy::on_ruleset_table(&mut s, &vm_table(VM_HASH_B));
        assert_eq!(
            s.vm().staged_hash128(),
            Some(VM_HASH_B),
            "later delivery supersedes"
        );

        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_dropped, 1, "superseded hash must not commit");
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_B), &mut c);
        assert_eq!(s.vm().commits_applied, 1);
        assert_eq!(s.vm().rows_active(), 1);
    }

    /// Item-7 gating pin: the table hook is deliberately NOT
    /// mask-gated (staging is control plane, inert until the
    /// mask-gated Commit) — stage-while-disabled → enable → commit
    /// must work without restaging.
    #[test]
    fn on_ruleset_table_stages_even_when_vm_disabled() {
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        let mut c = vm_ctx();
        s.on_start(&mut c).unwrap();

        Strategy::on_ruleset_table(&mut s, &vm_table(VM_HASH_A));
        assert_eq!(
            s.vm().staged_hash128(),
            Some(VM_HASH_A),
            "hook stages even with bit 5 off (not mask-gated by design)"
        );
        // Commit while disabled: frame never arrives (mask-gated).
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(s.vm().commits_applied, 0);
        assert_eq!(s.vm().commits_dropped, 0);

        // Enable slot 5, re-commit: the staged table was not lost.
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, STRATEGY_SLOT_VM), &mut c);
        s.on_ai(&ruleset_cmd(AiCmdKind::RulesetCommit, VM_HASH_A), &mut c);
        assert_eq!(
            s.vm().commits_applied,
            1,
            "staged survived the disabled window"
        );
        assert_eq!(s.vm().rows_active(), 1);
    }

    // ---------------- RG2: regime detector + gates ----------------

    mod regime_gates {
        use super::*;
        use core_regime::{ProfileParams, RegimeParams, SeedRow, MINUTE_NS, REGIME_MAX_MEMBERS};
        use core_types::regime::{
            RegimeLabelBuilder, DIM_TREND, SOURCE_DECLARED, SOURCE_MEASURED, TREND_BEAR, TREND_BULL,
        };
        use core_types::{RegimeTerm, REGIME_OFF_SOFT};
        use strategy_core::RegimeCounters;

        const BTC: SymbolId = make_symbol_id(VenueId::Binance, 100);
        const ETH: SymbolId = make_symbol_id(VenueId::Binance, 101);
        const T0: NsTs = 1_000_000_000_000;
        const WALL0: u64 = 1_800_000_000 * 1_000_000_000;

        fn params() -> RegimeParams {
            let mut members = [SYMBOL_ID_NONE; REGIME_MAX_MEMBERS];
            members[0] = ETH;
            let mut fast = ProfileParams::FAST_DEFAULT;
            fast.trend_w_min = 10;
            fast.shape_w_min = 10;
            fast.vol_w_min = 10;
            fast.stretch_w_min = 10;
            fast.rel_w_min = 10;
            let mut slow = fast;
            slow.trend_w_min = 20;
            slow.shape_w_min = 20;
            slow.vol_w_min = 20;
            slow.stretch_w_min = 20;
            slow.rel_w_min = 20;
            RegimeParams::new(BTC, BTC, members, 1, 1, [fast, slow])
        }

        fn bull_label(off: u8) -> RegimeLabelSet {
            let mut b = RegimeLabelBuilder::new();
            b.add(b"fast:trend:bull").unwrap();
            RegimeLabelSet::from_terms(&[b.finish()], off).unwrap()
        }

        fn uptrend_seed(minute0: i64) -> Vec<SeedRow> {
            let mut rows = Vec::new();
            let mut k = 0i64;
            while k < 40 {
                let m = minute0 - 40 + k;
                rows.push(SeedRow::new(BTC, m, 100_000_000 + k * 200_000));
                rows.push(SeedRow::new(ETH, m, 3_000_000_000 + k * 6_000_000));
                k += 1;
            }
            rows
        }

        fn hb_at(ts: u64) -> AiCmd {
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

        fn set_regime_cmd(ts: u64, profile: u16, word: RegimeWord, ttl: u64) -> AiCmd {
            AiCmd::new(
                ts,
                1,
                SYMBOL_ID_NONE,
                word.0 as i64,
                0,
                ttl,
                AiCmdKind::SetRegime,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                profile,
                0,
            )
        }

        #[test]
        fn unconfigured_detector_is_inert_and_open() {
            let mut s = StrategySet::new(BIT_AI_EXEC);
            let mut c = ctx();
            s.on_start(&mut c).unwrap();
            assert_eq!(s.timer_period_ns(), u64::MAX);
            assert!(s.regime_gate(SLOT_AI_EXEC).open);
            assert!(s.regime_gate(9).open);
            let k = s.regime_counters();
            assert_eq!(k, RegimeCounters::default());
            // Ticks and timers are harmless.
            s.on_tick(
                &tick(VenueId::Binance, BTC, 100_000_000, 100_001_000),
                &mut c,
            );
            s.on_timer(c.now, &mut c);
            assert_eq!(s.regime_counters().configured, 0);
            // Labels of unconstrained members are ANY; the vm and the
            // reserved slot cannot be relabelled.
            assert_eq!(s.regime_label_of(SLOT_AI_EXEC), RegimeLabelSet::ANY);
            assert!(!s.set_regime_label(SLOT_VM, bull_label(REGIME_OFF_SOFT)));
            assert!(!s.set_regime_label(7, bull_label(REGIME_OFF_SOFT)));
            assert!(s.set_regime_label(SLOT_AI_EXEC, bull_label(REGIME_OFF_SOFT)));
            assert_eq!(s.regime_label_of(SLOT_AI_EXEC), bull_label(REGIME_OFF_SOFT));
        }

        #[test]
        fn labelled_member_is_closed_until_the_regime_is_known_then_gated_edge_triggered() {
            let mut s = StrategySet::new(BIT_AI_EXEC);
            let mut c = ctx();
            c.now = T0;
            assert!(s.set_regime_label(SLOT_AI_EXEC, bull_label(REGIME_OFF_SOFT)));
            let anchor = WallAnchor::new(T0, WALL0);
            s.configure_regime(&params(), anchor, T0).unwrap();
            assert_eq!(s.timer_period_ns(), REGIME_TIMER_NS);
            assert_eq!(s.regime_counters().configured, 1);
            // The label survives configure (pulled from the member).
            assert_eq!(s.regime_label_of(SLOT_AI_EXEC), bull_label(REGIME_OFF_SOFT));
            // Nothing known yet ⇒ a labelled member is closed (fail-closed).
            s.on_timer(T0, &mut c);
            assert!(!s.regime_gate(SLOT_AI_EXEC).open);
            assert_eq!(s.regime_counters().gates[SLOT_AI_EXEC as usize], 1);
            s.on_start(&mut c).unwrap();
            // Seed an uptrend ⇒ BULL measured ⇒ gate opens silently at boot.
            let minute0 = s.regime().minute();
            let applied = s.seed_regime(&uptrend_seed(minute0), T0);
            assert_eq!(applied, 80);
            assert!(s.regime_gate(SLOT_AI_EXEC).open);
            assert_eq!(
                s.regime_counters().gate_changes,
                0,
                "boot judgement is silent"
            );
            assert_eq!(
                s.regime_counters().effective[0].value_of(DIM_TREND),
                Some(TREND_BULL)
            );
            assert_eq!(
                s.regime_counters().effective[0].source(),
                1 << SOURCE_MEASURED
            );
            // The ai-exec honours an intent while open (heartbeat first —
            // the §5.4 liveness law).
            s.on_ai(&hb_at(T0 + 1), &mut c);
            s.on_ai(&intent_cmd(T0 + 2, PM, 430_000, 2_000_000), &mut c);
            assert_eq!(s.ai_exec_mut().intents_honored, 1);
            // A declaration of TREND=bear closes it at once (edge → on_regime).
            let bear = RegimeWord::EMPTY.with_dim(DIM_TREND, TREND_BEAR);
            s.on_ai(&set_regime_cmd(T0 + 3, 0, bear, 5 * MINUTE_NS), &mut c);
            assert!(!s.regime_gate(SLOT_AI_EXEC).open);
            assert_eq!(s.regime_counters().gate_changes, 1);
            assert_eq!(s.regime_counters().declared_total, 1);
            assert_eq!(
                s.regime_counters().effective[0].source(),
                1 << SOURCE_DECLARED
            );
            assert_eq!(s.regime_counters().declared[0], bear);
            assert_eq!(
                s.regime_counters().declared_ts_ns[0],
                T0,
                "stamped with the ctx clock"
            );
            assert_eq!(s.regime_counters().declared_ttl_ns[0], 5 * MINUTE_NS);
            s.on_ai(&intent_cmd(T0 + 4, PM, 430_000, 2_000_000), &mut c);
            assert_eq!(
                s.ai_exec_mut().intents_honored,
                1,
                "closed gate refuses the entry"
            );
            assert_eq!(s.ai_exec_mut().intents_refused_regime, 1);
            // A second identical declaration is not an edge.
            s.on_ai(&set_regime_cmd(T0 + 5, 0, bear, 5 * MINUTE_NS), &mut c);
            assert_eq!(s.regime_counters().gate_changes, 1);
            // TTL expiry reopens (edge again) — the market keeps
            // trending up meanwhile (a silent feed would leave TREND
            // unknown-marked and the gate closed: fail-closed).
            let mut k = 0i64;
            while k < 6 {
                c.now = T0 + (k as u64 + 1) * MINUTE_NS;
                let btc = 108_000_000 + k * 200_000;
                s.on_tick(&tick(VenueId::Binance, BTC, btc - 500, btc + 500), &mut c);
                let eth = 3_240_000_000 + k * 6_000_000;
                s.on_tick(&tick(VenueId::Binance, ETH, eth - 500, eth + 500), &mut c);
                s.on_timer(c.now + 1_000_000, &mut c);
                k += 1;
            }
            assert!(s.regime_gate(SLOT_AI_EXEC).open);
            assert_eq!(s.regime_counters().gate_changes, 2);
            s.on_ai(&hb_at(c.now), &mut c);
            s.on_ai(&intent_cmd(c.now + 1, PM, 430_000, 2_000_000), &mut c);
            assert_eq!(s.ai_exec_mut().intents_honored, 2);
            assert!(
                s.regime_counters().minutes_judged >= 6,
                "the timer rolled the minutes"
            );
        }

        #[test]
        fn disabled_member_gets_its_gate_when_enabled() {
            // ai-exec starts disabled; the regime turns bearish while
            // it is off; Enable must hand it the CURRENT (closed) gate.
            let mut s = StrategySet::new(BIT_VM);
            let mut c = ctx();
            c.now = T0;
            assert!(s.set_regime_label(SLOT_AI_EXEC, bull_label(REGIME_OFF_SOFT)));
            s.configure_regime(&params(), WallAnchor::new(T0, WALL0), T0)
                .unwrap();
            s.on_start(&mut c).unwrap();
            let minute0 = s.regime().minute();
            s.seed_regime(&uptrend_seed(minute0), T0);
            assert!(s.regime_gate(SLOT_AI_EXEC).open);
            let bear = RegimeWord::EMPTY.with_dim(DIM_TREND, TREND_BEAR);
            s.on_ai(&set_regime_cmd(T0 + 3, 0, bear, 5 * MINUTE_NS), &mut c);
            assert!(!s.regime_gate(SLOT_AI_EXEC).open);
            assert_eq!(s.regime_counters().gate_changes, 0, "disabled: no callback");
            s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_AI_EXEC), &mut c);
            s.on_ai(&hb_at(T0 + 3), &mut c);
            s.on_ai(&intent_cmd(T0 + 4, PM, 430_000, 2_000_000), &mut c);
            assert_eq!(
                s.ai_exec_mut().intents_refused_regime,
                1,
                "enabled into a closed gate"
            );
        }

        #[test]
        fn funding_reference_events_reach_the_detector() {
            let mut s = StrategySet::new(BIT_AI_EXEC);
            let mut c = ctx();
            c.now = T0;
            s.configure_regime(&params(), WallAnchor::new(T0, WALL0), T0)
                .unwrap();
            s.on_start(&mut c).unwrap();
            let ev = ChannelEvent::new(
                T0 + 1,
                VenueId::Binance,
                ChannelId::Funding,
                BTC,
                7,
                1_700_000_000_000,
                -25_000,
                0,
            );
            s.on_venue_event(&ev, &mut c);
            assert_eq!(s.regime().funding(), (-25_000, 1_700_000_000_000));
            // Another symbol's funding is ignored.
            let other = ChannelEvent::new(
                T0 + 2,
                VenueId::Binance,
                ChannelId::Funding,
                ETH,
                8,
                1_700_000_001_000,
                99,
                0,
            );
            s.on_venue_event(&other, &mut c);
            assert_eq!(s.regime().funding(), (-25_000, 1_700_000_000_000));
        }

        #[test]
        fn vm_receives_the_regime_view_on_seed_declaration_and_every_minute() {
            // RG3 seam: the vm's rows judge against the view the set
            // pushes — at seed (silent), on a declaration (edge), and
            // on every minute roll even when no word changed (REL).
            let mut s = StrategySet::new(BIT_VM);
            let mut c = ctx();
            c.now = T0;
            assert_eq!(s.vm().regime_view().configured, 0);
            s.configure_regime(&params(), WallAnchor::new(T0, WALL0), T0)
                .unwrap();
            assert_eq!(s.vm().regime_view().configured, 1, "configure pushes");
            assert_eq!(s.vm().regime_view().n_syms, 2);
            assert_eq!(s.vm().regime_view().syms[1], ETH);
            s.on_start(&mut c).unwrap();
            let minute0 = s.regime().minute();
            s.seed_regime(&uptrend_seed(minute0), T0);
            let v = *s.vm().regime_view();
            assert_eq!(v.effective[0], s.regime().effective(0), "seed pushes");
            assert_eq!(v.effective[0].value_of(DIM_TREND), Some(TREND_BULL));
            assert_eq!(v.rel_of(0, ETH), s.regime().rel_of(0, ETH));
            // A declaration that changes the effective word pushes at
            // once — slot 5's own gate is ANY and never flips, the
            // push is unconditional.
            let bear = RegimeWord::EMPTY.with_dim(DIM_TREND, TREND_BEAR);
            s.on_ai(&set_regime_cmd(T0 + 3, 0, bear, 5 * MINUTE_NS), &mut c);
            assert_eq!(
                s.vm().regime_view().effective[0].value_of(DIM_TREND),
                Some(TREND_BEAR)
            );
            assert_eq!(s.regime_counters().gate_changes, 0, "vm gate is ANY");
            // A minute roll with an unchanged word still pushes (the
            // ETH REL moves from INLINE to LAGGING as ETH stalls).
            let before = *s.vm().regime_view();
            let mut k = 0i64;
            while k < 12 {
                c.now = T0 + (k as u64 + 1) * MINUTE_NS;
                let btc = 108_000_000 + k * 200_000;
                s.on_tick(&tick(VenueId::Binance, BTC, btc - 500, btc + 500), &mut c);
                s.on_tick(
                    &tick(
                        VenueId::Binance,
                        ETH,
                        3_240_000_000 - 500,
                        3_240_000_000 + 500,
                    ),
                    &mut c,
                );
                s.on_timer(c.now + 1_000_000, &mut c);
                k += 1;
            }
            let after = *s.vm().regime_view();
            assert_ne!(before, after, "minute rolls re-push the view");
            assert_eq!(after.rel_of(0, ETH), s.regime().rel_of(0, ETH));
            assert_eq!(after.rel_of(0, ETH), core_types::regime::REL_LAGGING);
            assert_eq!(StrategyCounters::vm_regime_blocked(&s), 0);
            assert_eq!(StrategyCounters::vm_regime_hard_exits(&s), 0);
        }

        #[test]
        fn term_and_word_helpers_compile_in_the_set() {
            let t = RegimeTerm::ANY;
            assert!(t.allows(
                RegimeWord::UNKNOWN,
                RegimeWord::UNKNOWN,
                REL_UNKNOWN,
                REL_UNKNOWN
            ));
        }
    }
}
