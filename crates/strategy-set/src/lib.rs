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
//! | 4 | `strategy-ai-exec` | **reserved** — member lands in item 8 |
//! | 5 | `strategy-vm` | **reserved** — member lands in 8g |
//!
//! Slots 4/5 exist ONLY as reserved mask bits
//! ([`core_types::STRATEGY_SLOT_AI_EXEC`] /
//! [`core_types::STRATEGY_SLOT_VM`]): per the unused-code rule there
//! is no dead member field behind them, and an `EnableStrategy`
//! targeting them is refused (counted) until the member exists.
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
//!   out to enabled members. Today every built member inherits the
//!   default no-op `on_ai`; `strategy-ai-exec` (item 8) is the first
//!   real consumer. No set-level `SetParam` ids are defined in 8f —
//!   §7's "SetParam(set-level)" clause activates when one exists.
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

use core_time::NsTs;
use core_types::{
    AiCmd, AiCmdKind, Fill, Signal, Tick, STRATEGY_SLOT_AI_EXEC, STRATEGY_SLOT_VM,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError};
use strategy_cross_arb::CrossArb;
use strategy_ev::EvStrategy;
use strategy_latency_arb::LatencyArb;
use strategy_rule_tree::RuleTree;

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

/// Enable-mask bit for the latency-arb member.
pub const BIT_LATENCY_ARB: u8 = 1 << SLOT_LATENCY_ARB;
/// Enable-mask bit for the ev member.
pub const BIT_EV: u8 = 1 << SLOT_EV;
/// Enable-mask bit for the cross-arb member.
pub const BIT_CROSS_ARB: u8 = 1 << SLOT_CROSS_ARB;
/// Enable-mask bit for the rule-tree member.
pub const BIT_RULE_TREE: u8 = 1 << SLOT_RULE_TREE;

/// Mask bit reserved for `strategy-ai-exec` (slot 4, item 8). No
/// member exists behind it in 8f — Enable is refused.
pub const BIT_AI_EXEC_RESERVED: u8 = 1 << STRATEGY_SLOT_AI_EXEC;
/// Mask bit reserved for `strategy-vm` (slot 5, 8g). No member exists
/// behind it in 8f — Enable is refused.
pub const BIT_VM_RESERVED: u8 = 1 << STRATEGY_SLOT_VM;

/// Every built member's bit (8f: slots 0–3).
pub const BUILT_MASK: u8 = BIT_LATENCY_ARB | BIT_EV | BIT_CROSS_ARB | BIT_RULE_TREE;

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
}

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
            enabled: m,
            initial: m,
            halted: false,
            enable_refused: 0,
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
            // Reserved (4/5) and unknown (6/7) slots: no member in
            // 8f — refuse and count.
            _ => {
                self.enable_refused = self.enable_refused.wrapping_add(1);
                return;
            }
        };
        self.enabled |= bit;
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
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.latency_arb.orders_dropped()
            + self.ev.orders_dropped()
            + self.cross_arb.orders_dropped()
            + self.rule_tree.orders_dropped()
    }
    #[inline]
    fn strategy_kind(&self) -> &'static str {
        "set"
    }
    #[inline]
    fn ai_enable_refused(&self) -> u64 {
        self.enable_refused
    }
}

impl Strategy for StrategySet {
    /// Forward `on_start` to the initially-enabled members only —
    /// their validation is exactly as fail-fast as the standalone
    /// paths. See the module docs for why skipped members are safe.
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError> {
        if self.initial & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_start(ctx)?;
        }
        if self.initial & BIT_EV != 0 {
            self.ev.on_start(ctx)?;
        }
        if self.initial & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_start(ctx)?;
        }
        if self.initial & BIT_RULE_TREE != 0 {
            self.rule_tree.on_start(ctx)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_tick(tick, ctx);
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_tick(tick, ctx);
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_tick(tick, ctx);
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree.on_tick(tick, ctx);
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_signal(signal, ctx);
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_signal(signal, ctx);
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_signal(signal, ctx);
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree.on_signal(signal, ctx);
        }
    }

    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_fill(fill, ctx);
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_fill(fill, ctx);
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_fill(fill, ctx);
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree.on_fill(fill, ctx);
        }
    }

    /// Set-level routing per §7 (module docs), then fan-out.
    #[inline]
    fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {
        match cmd.kind() {
            Some(AiCmdKind::EnableStrategy) => {
                self.enable_slot(cmd.strategy_id);
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
            // Unknown kinds cannot reach here (ingress + drain-site
            // shape checks), and every remaining kind fans out.
            _ => {}
        }
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_ai(cmd, ctx);
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_ai(cmd, ctx);
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_ai(cmd, ctx);
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree.on_ai(cmd, ctx);
        }
    }

    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        if self.enabled & BIT_LATENCY_ARB != 0 {
            self.latency_arb.on_timer(now_ns, ctx);
        }
        if self.enabled & BIT_EV != 0 {
            self.ev.on_timer(now_ns, ctx);
        }
        if self.enabled & BIT_CROSS_ARB != 0 {
            self.cross_arb.on_timer(now_ns, ctx);
        }
        if self.enabled & BIT_RULE_TREE != 0 {
            self.rule_tree.on_timer(now_ns, ctx);
        }
    }

    /// Minimum over the BUILT members (mask-independent so the
    /// engine's timer arming is stable across runtime Enable/Disable;
    /// `on_timer` itself fans out to enabled members only). All four
    /// members currently return `u64::MAX` (disabled).
    fn timer_period_ns(&self) -> u64 {
        let mut min = self.latency_arb.timer_period_ns();
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
        min
    }

    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        // Stop is unconditional — even disabled members get the
        // teardown callback (they may hold capture-worthy state some
        // day; today all four are no-ops).
        self.latency_arb.on_stop(ctx);
        self.ev.on_stop(ctx);
        self.cross_arb.on_stop(ctx);
        self.rule_tree.on_stop(ctx);
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
        assert_eq!(mask_for_name("all"), Some(BUILT_MASK));
        assert_eq!(mask_for_name("nope"), None);
        assert_eq!(mask_for_name(""), None);
    }

    #[test]
    fn new_clamps_reserved_bits_to_built_mask() {
        let s = StrategySet::new(0xFF);
        assert_eq!(s.enabled_mask(), BUILT_MASK);
        let s = StrategySet::new(BIT_AI_EXEC_RESERVED | BIT_VM_RESERVED);
        assert_eq!(s.enabled_mask(), 0);
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
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, SLOT_LATENCY_ARB), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        feed_trigger(&mut s, &mut c);
        assert_eq!(c.submitted, 0);

        // Disable also works while halted.
        let mut s = set_with_latency_arb(BIT_LATENCY_ARB);
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, SLOT_LATENCY_ARB), &mut c);
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

    #[test]
    fn enable_reserved_or_unknown_slot_refused() {
        let mut s = set_with_latency_arb(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(
            &ai_cmd(AiCmdKind::EnableStrategy, STRATEGY_SLOT_AI_EXEC),
            &mut c,
        );
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, STRATEGY_SLOT_VM), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, 7), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        assert_eq!(s.enable_refused_total(), 3);
        assert!(!s.is_halted(), "reserved-slot refusal is not a halt");
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
        // All four members currently disable their timers.
        assert_eq!(s.timer_period_ns(), u64::MAX);
    }
}
