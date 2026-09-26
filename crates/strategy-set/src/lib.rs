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
//! | 0 | `strategy-hyparb` | built (HYPARB H0, 2026-09-23 — **was `strategy-latency-arb`**, unlinked the same day; lands DARK — O-H8) |
//! | 1 | `strategy-vrp` | built (VRP V7, 2026-09-10 — **was `strategy-ev`**) |
//! | 2 | `strategy-xsd` | built (XSD-3, 2026-09-12 — **was `strategy-cross-arb`**, unlinked at XSD-S the same day) — configured only when `~/multivenue/xsd.toml` + `xsd-table.tsv` resolve |
//! | 3 | `strategy-bin15` | built (BIN15 O4b, 2026-09-12 — **was `strategy-rule-tree`**, unlinked the same day) — configured only when `~/multivenue/bin15.toml` + its seeds resolve |
//! | 4 | `strategy-ai-exec` | built (item 8) |
//! | 5 | `strategy-vm` | built (8g item 6) |
//! | 6 | `strategy-xmm` | built (XMM XH1, 2026-09-26 — **was `strategy-icdp`**, unlinked the same day, O-XH1; lands DARK until XH3) — configured only when `~/multivenue/xmm.toml` resolves |
//! | 7 | `strategy-hcv` | built (HC11, 2026-09-26 — Hypercall S1; lands DARK, paper only, in no configured mask, O-HC18) — configured only when `~/multivenue/hcv.toml` resolves |
//!
//! Every slot is built since HC11: an `EnableStrategy` for a slot past 7
//! is refused (counted), and slot 7 was never assigned before, so no
//! capture carries rows of another member under it. Slot 2 changed hands on 2026-09-12:
//! `strategy-cross-arb` was unlinked (the crate stays in the workspace
//! — the `strategy-ev` precedent) and `strategy-xsd` took the number.
//! Slot 6 changed hands on 2026-09-26 the same way: `strategy-icdp`
//! stays in the workspace (and `backtest --member icdp` still drives
//! it); nothing in the set links it.
//!
//! ## Timers are per slot (XMM XH1)
//!
//! The engine calls [`Strategy::on_timer`] at the set's
//! [`Strategy::timer_period_ns`] — the smallest period of any built
//! member. Each member is then called only when ITS OWN period has
//! elapsed since its own last call, so a fast member no longer runs
//! everyone's timer at its rate. Every member that existed before XH1
//! has a 1 s period or an empty `on_timer`, so each fires exactly when
//! it fired before. The regime detector and the long-tenor HAR set are
//! timer clients the same way, each on its own [`REGIME_TIMER_NS`] clock
//! once configured.
//!
//! ## Trades and order events (XMM XH1)
//!
//! [`Strategy::on_trade`] fans out to enabled members like `on_tick`.
//! [`Strategy::on_order_event`] goes to the slot that placed the order
//! ALONE — the X1 fill law — and an event for a disabled, unbuilt or
//! unattributed slot is counted, never delivered.
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
    REGIME_MAX_SYMS,
};
use core_time::{NsTs, WallAnchor};
use core_types::regime::REL_UNKNOWN;
use core_ring::MailboxTx;
use core_vol::{LongForecast, LongSetCounters, LongStateSnap, LongVolSet, DAY_NS};
use core_types::{
    AiCmd, AiCmdKind, ChannelEvent, ChannelId, Fill, Order, OrderEvent, RegimeLabelSet,
    RegimeWord, RuleTableV2, Signal, Tick, TradePrint, REGIME_OFF_HARD, REGIME_PROFILES,
    STRATEGY_SLOT_AI_EXEC, STRATEGY_SLOT_VM,
};
use strategy_ai_exec::AiExec;
use strategy_core::{
    Ctx, HarCounters, HarSeriesView, RegimeCounters, RegimeGate, RegimeRelView, SlotCounters,
    Strategy, StrategyCounters, StrategyError, SubmitErr, VmRowView, HAR_VIEW_NAME_MAX,
    HAR_VIEW_SERIES, HAR_VIEW_TENORS, HAR_VIEW_TENORS_D, HAR_VIEW_WEEKDAYS, REGIME_REL_SYMS,
};

// RG6: `regime_rel_view` copies the detector's view arrays straight
// into the trait's POD — the two capacities must agree.
const _: () = assert!(REGIME_REL_SYMS == REGIME_MAX_SYMS);
// HAR H3.5: `/state.har` carries every series the set can run, whole
// names, the engine's own weekday count, and only tenors it forecasts.
const _: () = assert!(HAR_VIEW_SERIES == core_vol::LONG_SET_MAX);
const _: () = assert!(HAR_VIEW_NAME_MAX == core_vol::LONG_SET_NAME_MAX);
const _: () = assert!(HAR_VIEW_WEEKDAYS == core_vol::WEEKDAYS);
const _: () = assert!(strategy_core::HAR_DAY_MS == core_vol::DAY_MS);
const _: () = assert!(HAR_VIEW_TENORS_D[HAR_VIEW_TENORS - 1] as usize <= core_vol::LONG_TAU_DAYS_MAX);
const _: () = assert!(
    core::mem::size_of::<HarCounters>() == core::mem::size_of::<LongSetCounters>()
);
use strategy_bin15::Bin15Strategy;
use strategy_hyparb::HyparbStrategy;
use strategy_vm::VmStrategy;
use strategy_hcv::HcvStrategy;
use strategy_vrp::VrpStrategy;
use strategy_xmm::XmmStrategy;
use strategy_xsd::XsdStrategy;

// ---------------------------------------------------------------
// Slot / mask constants
// ---------------------------------------------------------------

/// Slot index of the hyparb member (HYPARB O-H2, 2026-09-23 — **was
/// `strategy-latency-arb`**, unlinked by O-H1: the crate stays in the
/// workspace, nothing links it). The number is wire-stable: a capture
/// taken before H0 carries latency-arb rows under slot 0.
pub const SLOT_HYPARB: u8 = 0;
/// Slot index of the VRP member.
///
/// **The swap boundary.** Slot 1 was `strategy-ev` until 2026-09-10
/// (VRP V7). The NUMBER is wire-stable — `Order.strategy_id` 1 and
/// `AiCmd::strategy_id` 1 still mean "slot 1" — so a capture taken
/// before that date carries EV rows under this slot and one taken after
/// carries VRP rows. `docs/migration.md` records the boundary.
pub const SLOT_VRP: u8 = 1;
/// Slot 1 under its pre-2026-09-10 name, for readers of pre-boundary
/// captures and configs. Identical value.
pub const SLOT_EV: u8 = SLOT_VRP;
/// Slot index of the xsd member (cross-sectional dislocation; the
/// statarb lane's coded member).
///
/// **The second swap boundary.** Slot 2 was `strategy-cross-arb` until
/// 2026-09-12 (XSD-S; operator ruling R3 = reuse slot 2). The NUMBER is
/// wire-stable — `Order.strategy_id` 2 and `AiCmd::strategy_id` 2 still
/// mean "slot 2" — so a capture taken before that date carries
/// cross-arb rows under this slot, one taken after XSD-3 carries XSD
/// rows, and between the two the slot emits nothing. `docs/migration.md`
/// records the boundary.
pub const SLOT_XSD: u8 = 2;
/// Slot index of the bin15 member.
///
/// Slot 3 changed hands on 2026-09-12 (BIN15 O4b): it was
/// `strategy-rule-tree`, which is now UNLINKED — the crate remains a
/// workspace member and still builds and tests, but nothing depends on
/// it and no mask name reaches it. The NUMBER is wire-stable
/// — `Order.strategy_id` 3 and `AiCmd::strategy_id` 3 still mean
/// "slot 3" — so a capture taken before that date carries rule-tree
/// rows under this slot and one taken after carries bin15 rows.
/// `docs/migration.md` records the boundary. Pinned in `core-types` as
/// [`core_types::STRATEGY_SLOT_BIN15`], because `SetBinarySpec` shape
/// enforcement depends on it.
pub const SLOT_BIN15: u8 = core_types::STRATEGY_SLOT_BIN15;
/// Slot index of the ai-exec member (wire value pinned in
/// `core-types` — `OrderIntent` shape enforcement depends on it).
pub const SLOT_AI_EXEC: u8 = STRATEGY_SLOT_AI_EXEC;
/// Slot index of the vm member (wire value pinned in `core-types` —
/// `RulesetStage`/`RulesetCommit` shape enforcement depends on it).
pub const SLOT_VM: u8 = STRATEGY_SLOT_VM;
/// Slot index of the xmm member (XMM XH1, ruling O-XH1).
///
/// **The third swap boundary.** Slot 6 was `strategy-icdp` (ICDP I4,
/// 2026-09-03) until 2026-09-26; icdp is UNLINKED — the crate stays in
/// the workspace and `backtest --member icdp` still drives it, but
/// nothing in the set links it and no mask name reaches it. The NUMBER
/// is wire-stable — `Order.strategy_id` 6 and `AiCmd::strategy_id` 6
/// still mean "slot 6" — so a capture taken before XH1 carries icdp
/// rows under this slot and one taken after carries xmm rows.
/// `docs/migration.md` records the boundary.
pub const SLOT_XMM: u8 = 6;
/// Slot index of the hcv member (HC11, ruling O-HC18): the Hypercall S1
/// options member — DARK, paper only (`exec.toml` refuses a live slot 7
/// until its own arming ruling). Slot 7 was reserved until HC11, so the
/// number carries no earlier member's rows.
pub const SLOT_HCV: u8 = 7;

/// Enable-mask bit for the hyparb member (slot 0 — see [`SLOT_HYPARB`]).
pub const BIT_HYPARB: u8 = 1 << SLOT_HYPARB;
/// Enable-mask bit for the VRP member (slot 1 — see [`SLOT_VRP`]).
pub const BIT_VRP: u8 = 1 << SLOT_VRP;
/// Slot 1's bit under its pre-2026-09-10 name. Identical value.
pub const BIT_EV: u8 = BIT_VRP;
/// Enable-mask bit for the xsd member (slot 2 — see [`SLOT_XSD`]).
pub const BIT_XSD: u8 = 1 << SLOT_XSD;
/// Enable-mask bit for the bin15 member (slot 3 — see [`SLOT_BIN15`]).
pub const BIT_BIN15: u8 = 1 << SLOT_BIN15;
/// Enable-mask bit for the ai-exec member (item 8).
pub const BIT_AI_EXEC: u8 = 1 << SLOT_AI_EXEC;
/// Enable-mask bit for the vm member (8g item 6).
pub const BIT_VM: u8 = 1 << SLOT_VM;
/// Enable-mask bit for the xmm member (slot 6 — see [`SLOT_XMM`]).
pub const BIT_XMM: u8 = 1 << SLOT_XMM;
/// Enable-mask bit for the hcv member (slot 7 — see [`SLOT_HCV`]).
pub const BIT_HCV: u8 = 1 << SLOT_HCV;

/// Every built member's bit (slots 0–7: all of them since HC11).
pub const BUILT_MASK: u8 =
    BIT_HYPARB | BIT_VRP | BIT_XSD | BIT_BIN15 | BIT_AI_EXEC | BIT_VM | BIT_XMM | BIT_HCV;

/// Ai-exec capacity inside the set (design §7 sketch `AiExec<64>` —
/// sizes the fair table, book table and cooldown gate alike).
pub const SET_AI_EXEC_SLOTS: usize = 64;
// (VM2 V3: the vm's book generic is gone — mids live in the feature
// engine's fixed sym slots — so the old `SET_VM_SLOTS = 512` law
// retired with it.)

/// Every `--strategy` name the engine resolves, with the enable mask
/// it composes.
///
/// This table is the ONE source of truth for the name set:
/// `mask_for_name` scans it and the cli's boot arm pins itself against
/// it, so a name can never again resolve here while refusing to boot.
/// Boot-only — a linear scan over a handful of entries, never hot.
pub const MASK_TABLE: &[(&str, u8)] = &[
    // HYPARB H0 (2026-09-23): slot 0 is the hyparb member;
    // `latency-arb` is GONE as a name (O-H1) — an operator who types
    // the old one gets a boot refusal, not a different strategy than
    // the one asked for. Lands DARK (O-H8): the live strategy.conf
    // names none of these until HZ and H8 are green.
    ("hyparb", BIT_HYPARB),
    ("ai+hyparb", BIT_AI_EXEC | BIT_VM | BIT_HYPARB),
    (
        "ai+vrp+xsd+bin15+hyparb",
        BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD | BIT_BIN15 | BIT_HYPARB,
    ),
    // XSD-S/XSD-3 (2026-09-12): slot 2 is the xsd member; `cross-arb`
    // is GONE as a name — an operator who types the old one gets a
    // boot refusal, not a different strategy than the one asked for.
    ("xsd", BIT_XSD),
    ("ai+xsd", BIT_AI_EXEC | BIT_VM | BIT_XSD),
    ("ai+vrp+xsd", BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD),
    // BIN15 O4b (2026-09-12): slot 3 is the bin15 member;
    // `rule-tree` is GONE as a name — an operator who types the old
    // one gets a boot refusal, not a different strategy than the
    // one asked for. The same law XSD-S and VRP V7 applied.
    ("bin15", BIT_BIN15),
    ("ai+bin15", BIT_AI_EXEC | BIT_VM | BIT_BIN15),
    ("ai+vrp+bin15", BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_BIN15),
    ("ai+xsd+bin15", BIT_AI_EXEC | BIT_VM | BIT_XSD | BIT_BIN15),
    (
        "ai+vrp+xsd+bin15",
        BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD | BIT_BIN15,
    ),
    ("ai-exec", BIT_AI_EXEC),
    ("vm", BIT_VM),
    // AI-pushed lanes only (operator ruling 2026-09-02: Rust-coded
    // strategies disabled at boot; the engine executes only what
    // the AI command plane pushes — ai-exec intents + VM rulesets).
    ("ai", BIT_AI_EXEC | BIT_VM),
    // XMM XH1 (2026-09-26, O-XH1): slot 6 is the xmm member; `icdp`
    // and `ai+icdp` are GONE as names — an operator who types the old
    // one gets a boot refusal, not a different strategy than the one
    // asked for. The member boots only with its artifact
    // (`~/multivenue/xmm.toml`), paper only; the last name is the live
    // set plus xmm, for the XH3 paper run.
    ("xmm", BIT_XMM),
    ("ai+xmm", BIT_AI_EXEC | BIT_VM | BIT_XMM),
    (
        "ai+vrp+xsd+bin15+hyparb+xmm",
        BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD | BIT_BIN15 | BIT_HYPARB | BIT_XMM,
    ),
    // HC11 (2026-09-26, O-HC18): slot 7 is the hcv member — Hypercall
    // S1, DARK: paper only and in no configured mask. It boots only with
    // its artifact (`~/multivenue/hcv.toml`); the last name is the live
    // set plus xmm and hcv, so switching it on is one `STRATEGY=` line.
    ("hcv", BIT_HCV),
    ("ai+hcv", BIT_AI_EXEC | BIT_VM | BIT_HCV),
    (
        "ai+vrp+xsd+bin15+hyparb+xmm+hcv",
        BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD | BIT_BIN15 | BIT_HYPARB | BIT_XMM | BIT_HCV,
    ),
    // VRP V7: slot 1 is the VRP member. `ev` is GONE as a name —
    // an operator who types it must get a boot refusal, not a
    // different strategy than the one they asked for.
    ("vrp", BIT_VRP),
    ("ai+vrp", BIT_AI_EXEC | BIT_VM | BIT_VRP),
    ("all", BUILT_MASK),
];

/// Map a `--strategy` value to an initial enable mask (design §7:
/// single name = single bit, back-compatible; `all` = all built
/// members). `None` for unknown names — the cli rejects those at
/// boot exactly as before.
///
/// BIN15 O5 (2026-09-12): this body was a `match` whose arms the cli
/// duplicated as boot-arm literals. The two drifted — the five bin15
/// names landed here and in the wrapper allow-list but never in the
/// arm — and `--strategy ai+vrp+xsd+bin15` refused the boot with every
/// member dark behind a capture that still looked healthy.
/// `MASK_TABLE` above is now the only list.
pub fn mask_for_name(name: &str) -> Option<u8> {
    let mut i = 0;
    while i < MASK_TABLE.len() {
        let (candidate, mask) = MASK_TABLE[i];
        if candidate == name {
            return Some(mask);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------
// StrategySet
// ---------------------------------------------------------------

/// Statically-composed strategy set. See the module docs for slot
/// map, routing and boot semantics.
pub struct StrategySet {
    hyparb: HyparbStrategy,
    vrp: VrpStrategy,
    xsd: XsdStrategy,
    bin15: Bin15Strategy,
    ai_exec: AiExec<SET_AI_EXEC_SLOTS>,
    vm: VmStrategy,
    xmm: XmmStrategy,
    hcv: HcvStrategy,
    /// Runtime enable mask (bits per the slot map). Every bit is a
    /// built slot since HC11.
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
    /// X1: fills stamped for a slot that is not enabled, or not built.
    /// Mirrored to `engine_set_fills_unrouted_total`.
    fills_unrouted: u64,
    /// XMM XH1: order events for a slot that is not enabled, not built,
    /// or not attributed — counted, never delivered.
    order_events_unrouted: u64,
    /// XMM XH1: when each slot's `on_timer` last ran (per-slot gating —
    /// module docs).
    timer_last_ns: [NsTs; 8],
    /// XMM XH1: when the regime detector's timer last ran.
    regime_timer_last_ns: NsTs,
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
    /// HAR H3.3: the long-tenor HAR series (boot-boxed; inert until the
    /// cli configures it from `har.toml` — one branch per tick and per
    /// poll, so a boot without the file is the pre-H3 set, bit for bit).
    /// Beside the regime detector, in the same seat and on the same 1 s
    /// period (its own clock); no member reads it (plan law L4).
    har: Box<LongVolSet>,
    /// HAR H3.5: each series' `/state.har` row as of its newest day close
    /// or the restore — rebuilt in `on_timer` only when that series'
    /// epoch moved (the forecasts do not move between day closes), so
    /// the 1 s publish copies rows instead of re-reading 9 tenors × 12
    /// engines. Boot-boxed.
    har_view: Box<[HarSeriesView; HAR_VIEW_SERIES]>,
    /// The epoch each cached row was built at (`u64::MAX`: never).
    har_view_epoch: [u64; HAR_VIEW_SERIES],
    /// When the long-tenor set's timer last ran: its own clock on the
    /// regime's 1 s period (XMM XH1's per-slot timers).
    har_timer_last_ns: NsTs,
    /// HAR H3.7: one mailbox per series to the cli's state-writer thread
    /// (`har.toml` order). Empty = no writer: the cli writes the state on
    /// the engine thread instead (the pre-H3.7 path). Boot-only.
    har_out: Vec<MailboxTx<LongStateSnap>>,
    /// HAR H3.7: the epoch each series' state was last handed to the writer.
    har_offered: [u64; HAR_VIEW_SERIES],
}

/// RG2: the set's timer cadence once a detector is configured — the
/// regime rolls once per wall minute, the timer polls for the boundary.
/// HAR H3.3: a configured long-tenor set arms the same poll (its minute
/// rolls and its staggered day closes ride it).
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
            hyparb: HyparbStrategy::new(),
            vrp: VrpStrategy::new(),
            xsd: XsdStrategy::new(),
            bin15: Bin15Strategy::new(),
            ai_exec: AiExec::new(),
            vm: VmStrategy::new(),
            xmm: XmmStrategy::new(),
            hcv: HcvStrategy::new(),
            enabled: m,
            initial: m,
            halted: false,
            enable_refused: 0,
            fills_unrouted: 0,
            order_events_unrouted: 0,
            timer_last_ns: [0; 8],
            regime_timer_last_ns: 0,
            regime: RegimeState::new_boxed(),
            regime_labels: [RegimeLabelSet::ANY; 8],
            regime_gates: [RegimeGate::OPEN_UNKNOWN; 8],
            regime_declared_total: 0,
            regime_gate_changes: 0,
            har: Box::new(LongVolSet::new()),
            har_view: Box::new([HarSeriesView::default(); HAR_VIEW_SERIES]),
            har_view_epoch: [u64::MAX; HAR_VIEW_SERIES],
            har_timer_last_ns: 0,
            har_out: Vec::new(),
            har_offered: [0; HAR_VIEW_SERIES],
        }
    }

    // ---- HAR H3.3: the long-tenor HAR series --------------------------

    /// The long-tenor HAR series (readers: `/state`, the state writer).
    pub fn har(&self) -> &LongVolSet {
        &self.har
    }

    /// Boot only: configure the series (`core_vol::LongVolSet::configure`)
    /// and restore their state. Nothing on the engine loop calls this.
    pub fn har_mut(&mut self) -> &mut LongVolSet {
        &mut self.har
    }

    /// HAR H3.7, boot only: from now on hand each series' state to the
    /// cli's state-writer thread through `out` (one mailbox per series,
    /// `har.toml` order), at each of its day closes — instead of the cli's
    /// render and fsync on the engine thread. The epochs the restore left
    /// count as handed: a boot that changed nothing writes nothing.
    pub fn install_har_outbox(&mut self, out: Vec<MailboxTx<LongStateSnap>>) {
        debug_assert_eq!(out.len(), self.har.len(), "one mailbox per configured series");
        let mut i = 0usize;
        while i < HAR_VIEW_SERIES {
            self.har_offered[i] = self.har.series_epoch(i);
            i += 1;
        }
        self.har_out = out;
    }

    /// HAR H3.7: hand the state of every series whose epoch moved (its day
    /// close, or the restore) to the writer — one ~201 KiB copy into its
    /// mailbox, the engine thread's whole share of the write. A mailbox the
    /// writer still holds (a write in flight, or failing) is tried again at
    /// the next poll, so the newest state follows. Nothing without a writer.
    #[inline]
    fn offer_har_state(&mut self) {
        let n = self.har.len().min(self.har_out.len()).min(HAR_VIEW_SERIES);
        let mut i = 0usize;
        while i < n {
            let e = self.har.series_epoch(i);
            if e != self.har_offered[i] {
                if let Some(tx) = self.har_out.get_mut(i) {
                    if let Some(mut slot) = tx.try_fill() {
                        if self.har.snapshot_series(i, &mut slot) {
                            self.har_offered[i] = e;
                            slot.commit();
                        }
                    }
                }
            }
            i += 1;
        }
    }

    /// HAR H3.5: rebuild the cached `/state.har` row of every series whose
    /// epoch moved since its row was built — at most one a poll in steady
    /// state (the day closes are staggered), all of them once after the
    /// boot restore. `n` compares when nothing moved; nothing while inert.
    ///
    /// HC11 (O-HC22 — law L4 lifted for slot 7 alone): when a row moved,
    /// the hcv member is handed the rows (its σ̂), enabled or not, so it
    /// is current the moment it is enabled. Once a series per UTC day.
    #[inline]
    fn refresh_har_view(&mut self) {
        let n = self.har.len().min(HAR_VIEW_SERIES);
        let mut moved = false;
        let mut i = 0usize;
        while i < n {
            let e = self.har.series_epoch(i);
            if e != self.har_view_epoch[i] {
                self.har_view[i] = har_row(&self.har, i);
                self.har_view_epoch[i] = e;
                moved = true;
            }
            i += 1;
        }
        if moved {
            self.hcv.set_har_view(&self.har_view[..n]);
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
            SLOT_HYPARB => self.hyparb.set_regime_label(set),
            SLOT_VRP => self.vrp.set_regime_label(set),
            SLOT_XSD => self.xsd.set_regime_label(set),
            SLOT_BIN15 => self.bin15.set_regime_label(set),
            SLOT_AI_EXEC => self.ai_exec.set_regime_label(set),
            SLOT_XMM => self.xmm.set_regime_label(set),
            SLOT_HCV => self.hcv.set_regime_label(set),
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
        self.regime_labels[SLOT_HYPARB as usize] = self.hyparb.regime_label();
        self.regime_labels[SLOT_VRP as usize] = self.vrp.regime_label();
        self.regime_labels[SLOT_XSD as usize] = self.xsd.regime_label();
        self.regime_labels[SLOT_BIN15 as usize] = self.bin15.regime_label();
        self.regime_labels[SLOT_AI_EXEC as usize] = self.ai_exec.regime_label();
        self.regime_labels[SLOT_VM as usize] = RegimeLabelSet::ANY; // rows gate themselves (RG3)
        self.regime_labels[SLOT_XMM as usize] = self.xmm.regime_label();
        self.regime_labels[SLOT_HCV as usize] = self.hcv.regime_label();
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
        self.push_regime_views();
    }

    /// RG3: the set→member seam — hand the view-consuming members the
    /// detector's current view (effective words + per-member REL) so the
    /// vm's rows re-judge and the vrp's band picks up its regime
    /// intercept (P4.1). Called on every minute roll, effective change
    /// and declaration regardless of either slot's own gate; never per
    /// tick.
    fn push_regime_views(&mut self) {
        let view = self.regime.view();
        self.vm.set_regime_view(&view);
        self.vrp.set_regime_view(&view);
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
        self.push_regime_views();
    }

    fn deliver_gate<C: Ctx>(&mut self, slot: u8, gate: RegimeGate, ctx: &mut C) {
        match slot {
            SLOT_HYPARB => self
                .hyparb
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB)),
            SLOT_VRP => self
                .vrp
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_VRP)),
            SLOT_XSD => self
                .xsd
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_XSD)),
            SLOT_BIN15 => self
                .bin15
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_BIN15)),
            SLOT_AI_EXEC => self
                .ai_exec
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC)),
            SLOT_VM => self
                .vm
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_VM)),
            SLOT_XMM => self
                .xmm
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_XMM)),
            SLOT_HCV => self
                .hcv
                .on_regime(gate, &mut StampCtx::new(&mut *ctx, SLOT_HCV)),
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

    /// Configure the hyparb member (boot-only).
    #[inline]
    pub fn hyparb_mut(&mut self) -> &mut HyparbStrategy {
        &mut self.hyparb
    }

    /// X1: deliver an attributed fill to exactly one enabled slot.
    #[inline(always)]
    fn route_fill_to_slot<C: Ctx>(&mut self, slot: u8, fill: &Fill, ctx: &mut C) {
        match slot {
            SLOT_HYPARB => self
                .hyparb
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB)),
            SLOT_VRP => self
                .vrp
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_VRP)),
            SLOT_XSD => self
                .xsd
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_XSD)),
            SLOT_BIN15 => self
                .bin15
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_BIN15)),
            SLOT_AI_EXEC => self
                .ai_exec
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC)),
            SLOT_VM => self.vm.on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_VM)),
            SLOT_XMM => self
                .xmm
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_XMM)),
            SLOT_HCV => self
                .hcv
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_HCV)),
            // No slot past 7 exists. A fill stamped with one is a bug
            // upstream, not a member to deliver to.
            _ => self.fills_unrouted = self.fills_unrouted.wrapping_add(1),
        }
    }

    /// X1: fills that reached the set stamped for a slot that is not
    /// enabled (or not built). Non-zero means an order outlived a
    /// `DisableStrategy`, or a stamp is wrong.
    #[inline]
    #[must_use]
    pub const fn fills_unrouted(&self) -> u64 {
        self.fills_unrouted
    }

    /// XMM XH1: deliver an order event to exactly one enabled slot —
    /// the [`Self::route_fill_to_slot`] law.
    #[inline(always)]
    fn route_order_event_to_slot<C: Ctx>(&mut self, slot: u8, event: &OrderEvent, ctx: &mut C) {
        match slot {
            SLOT_HYPARB => self
                .hyparb
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB)),
            SLOT_VRP => self
                .vrp
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_VRP)),
            SLOT_XSD => self
                .xsd
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_XSD)),
            SLOT_BIN15 => self
                .bin15
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_BIN15)),
            SLOT_AI_EXEC => self
                .ai_exec
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC)),
            SLOT_VM => self
                .vm
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_VM)),
            SLOT_XMM => self
                .xmm
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_XMM)),
            SLOT_HCV => self
                .hcv
                .on_order_event(event, &mut StampCtx::new(&mut *ctx, SLOT_HCV)),
            // No slot past 7 exists — the caller's mask check keeps it
            // out, and this arm counts it if that ever changes.
            _ => self.order_events_unrouted = self.order_events_unrouted.wrapping_add(1),
        }
    }

    /// XMM XH1: order events that reached the set for a slot that is not
    /// enabled, not built, or not attributed. Non-zero means an order
    /// outlived a `DisableStrategy`, or an event is not ours.
    #[inline]
    #[must_use]
    pub const fn order_events_unrouted(&self) -> u64 {
        self.order_events_unrouted
    }

    /// XMM XH1: the per-slot timer gate — true, and the slot's clock
    /// advanced, when `period` has elapsed since the slot last ran.
    /// `u64::MAX` never fires (a member with no timer).
    #[inline(always)]
    fn timer_due(last_ns: &mut NsTs, period: u64, now_ns: NsTs) -> bool {
        if period != u64::MAX && now_ns.saturating_sub(*last_ns) >= period {
            *last_ns = now_ns;
            true
        } else {
            false
        }
    }

    /// Configure the VRP member (boot-only).
    #[inline]
    pub fn vrp_mut(&mut self) -> &mut VrpStrategy {
        &mut self.vrp
    }

    /// The VRP member (cli: the boot tell's artifact hash).
    #[inline]
    pub fn vrp(&self) -> &VrpStrategy {
        &self.vrp
    }

    /// Configure the xsd member (boot-only).
    #[inline]
    pub fn xsd_mut(&mut self) -> &mut XsdStrategy {
        &mut self.xsd
    }

    /// The xsd member (cli: boot tells).
    #[inline]
    pub fn xsd(&self) -> &XsdStrategy {
        &self.xsd
    }

    /// Configure the bin15 member (boot-only).
    #[inline]
    pub fn bin15_mut(&mut self) -> &mut Bin15Strategy {
        &mut self.bin15
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
    /// lends each slot to `vm_mut().receive_table_v2` — the §6
    /// copy-#2 seam; there is no boot config (§7.3: booting inert is
    /// normal).
    #[inline]
    pub fn vm_mut(&mut self) -> &mut VmStrategy {
        &mut self.vm
    }

    /// Configure the xmm member (boot-only: `configure` with the
    /// resolved artifact).
    #[inline]
    pub fn xmm_mut(&mut self) -> &mut XmmStrategy {
        &mut self.xmm
    }

    /// Configure the hcv member (boot-only: `configure` with the resolved
    /// artifact, `install_events` with the calendar's mailbox).
    #[inline]
    pub fn hcv_mut(&mut self) -> &mut HcvStrategy {
        &mut self.hcv
    }

    /// The hcv member (cli: boot tells).
    #[inline]
    pub fn hcv(&self) -> &HcvStrategy {
        &self.hcv
    }

    /// Set-level `EnableStrategy` handling. See module docs.
    #[inline]
    fn enable_slot(&mut self, slot: u8) {
        if self.halted {
            self.enable_refused = self.enable_refused.wrapping_add(1);
            return;
        }
        let bit = match slot {
            SLOT_HYPARB => BIT_HYPARB,
            SLOT_VRP => BIT_VRP,
            SLOT_XSD => BIT_XSD,
            SLOT_BIN15 => BIT_BIN15,
            SLOT_AI_EXEC => BIT_AI_EXEC,
            SLOT_VM => BIT_VM,
            SLOT_XMM => BIT_XMM,
            SLOT_HCV => BIT_HCV,
            // No slot past 7: no member behind it — refuse and count.
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
        self.hyparb.orders_emitted()
            + self.vrp.orders_emitted()
            + self.xsd.orders_emitted()
            + self.bin15.orders_emitted()
            + self.ai_exec.orders_emitted()
            + self.vm.orders_emitted()
            + self.xmm.orders_emitted()
            + self.hcv.orders_emitted()
    }
    #[inline]
    fn orders_dropped(&self) -> u64 {
        self.hyparb.orders_dropped()
            + self.vrp.orders_dropped()
            + self.xsd.orders_dropped()
            + self.bin15.orders_dropped()
            + self.ai_exec.orders_dropped()
            + self.vm.orders_dropped()
            + self.xmm.orders_dropped()
            + self.hcv.orders_dropped()
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
    /// VRP V7: slot 1's observables.
    #[inline]
    fn vrp_counters(&self) -> strategy_core::VrpCounters {
        self.vrp.vrp_counters()
    }
    /// VRP V8a: slot 1's persisted state.
    #[inline]
    fn vrp_state_epoch(&self) -> u64 {
        StrategyCounters::vrp_state_epoch(&self.vrp)
    }
    #[inline]
    fn vrp_last_settle_value_1e6(&self) -> i64 {
        self.vrp.last_settle_value_1e6()
    }
    #[inline]
    fn vrp_regime_offset_1e6(&self) -> i64 {
        StrategyCounters::vrp_regime_offset_1e6(&self.vrp)
    }
    #[inline]
    fn vrp_snapshot_view(&self) -> strategy_core::VrpSnapshotView {
        StrategyCounters::vrp_snapshot_view(&self.vrp)
    }
    #[inline]
    fn fills_unrouted(&self) -> u64 {
        self.fills_unrouted
    }
    #[inline]
    fn order_events_unrouted(&self) -> u64 {
        self.order_events_unrouted
    }
    #[inline]
    fn render_vrp_state(&self, out: &mut String) -> bool {
        StrategyCounters::render_vrp_state(&self.vrp, out)
    }
    /// HAR H3.4: the long-tenor series (the set owns them, no member).
    #[inline]
    fn har_series(&self) -> usize {
        self.har.len()
    }
    #[inline]
    fn har_series_epoch(&self, i: usize) -> u64 {
        self.har.series_epoch(i)
    }
    fn render_har_series(&self, i: usize, out: &mut String) -> bool {
        let (Some(name), Some(e)) = (self.har.name(i), self.har.engine(i)) else {
            return false;
        };
        core_vol::render_state_file(core::str::from_utf8(name).unwrap_or("?"), e, out);
        true
    }
    /// HAR H3.5: the set's counters, field for field.
    #[inline]
    fn har_counters(&self) -> HarCounters {
        let c = self.har.counters();
        HarCounters {
            minutes_rolled: c.minutes_rolled,
            closes: c.closes,
            day_closes: c.day_closes,
            held: c.held,
            forced: c.forced,
            day_close_ns_max: c.day_close_ns_max,
            day_close_ns_last: c.day_close_ns_last,
            epoch: c.epoch,
        }
    }
    /// HAR H3.5: the cached rows, with the three per-minute fields read
    /// live. Never allocates.
    fn har_series_view(&self, out: &mut [HarSeriesView]) -> u32 {
        let n = self.har.len().min(HAR_VIEW_SERIES);
        let m = n.min(out.len());
        let mut i = 0usize;
        while i < m {
            let mut v = self.har_view[i];
            if let Some(e) = self.har.engine(i) {
                v.last_min_ms = e.last_min_ts_ms();
                v.open_minutes = match e.open_day() {
                    Some((_, _, n_min)) => n_min,
                    None => 0,
                };
                v.gaps = e.gaps();
            }
            out[i] = v;
            i += 1;
        }
        n as u32
    }
    /// HYPARB H4: slot 0's observables.
    #[inline]
    fn hyparb_counters(&self) -> strategy_core::HyparbCounters {
        self.hyparb.hyparb_counters()
    }
    #[inline]
    fn hyparb_pools_view(&self, out: &mut [strategy_core::HyparbPoolView]) -> u32 {
        self.hyparb.hyparb_pools_view(out)
    }
    #[inline]
    fn hyparb_coins_view(&self, out: &mut [strategy_core::HyparbCoinView]) -> u32 {
        self.hyparb.hyparb_coins_view(out)
    }
    /// XMM XH3: slot 6's counters and per-perp rows (`/metrics`,
    /// `/state`). An unconfigured member reports zeros and no rows.
    #[inline]
    fn xmm_counters(&self, out: &mut strategy_core::XmmCounters) {
        self.xmm.xmm_counters(out);
    }
    #[inline]
    fn xmm_perps_view(&self, out: &mut [strategy_core::XmmPerpView]) -> u32 {
        self.xmm.xmm_perps_view(out)
    }
    /// HC11: slot 7's counters (`/metrics`, `/state`). An unconfigured
    /// member reports zeros.
    #[inline]
    fn hcv_counters(&self, out: &mut strategy_core::HcvCounters) {
        self.hcv.hcv_counters(out);
    }
    #[inline]
    fn hyparb_decision_log(&self) -> (&[strategy_core::HyparbDecision], u64) {
        self.hyparb.hyparb_decision_log()
    }
    /// BIN15 O4b: slot 3's observables.
    #[inline]
    fn bin15_counters(&self) -> strategy_core::Bin15Counters {
        self.bin15.bin15_counters()
    }
    #[inline]
    fn bin15_families_view(&self, out: &mut [strategy_core::Bin15FamilyView]) -> u32 {
        self.bin15.bin15_families_view(out)
    }
    /// XSD-3: slot 2's observables + persisted state.
    #[inline]
    fn xsd_counters(&self) -> strategy_core::XsdCounters {
        self.xsd.xsd_counters()
    }
    #[inline]
    fn xsd_state_epoch(&self) -> u64 {
        StrategyCounters::xsd_state_epoch(&self.xsd)
    }
    #[inline]
    fn xsd_positions_view(&self, out: &mut [strategy_core::XsdPositionView]) -> u32 {
        StrategyCounters::xsd_positions_view(&self.xsd, out)
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

    // ---- RG6 `/state` family (1 s snapshot, same UFCS route) --------

    #[inline]
    fn is_halted(&self) -> bool {
        self.halted
    }
    /// Per-slot orders + the slot's label shape (the gate itself rides
    /// `regime_counters().gates`).
    fn slot_counters(&self, slot: u8) -> SlotCounters {
        let (emitted, dropped) = match slot {
            SLOT_HYPARB => (self.hyparb.orders_emitted(), self.hyparb.orders_dropped()),
            SLOT_VRP => (self.vrp.orders_emitted(), self.vrp.orders_dropped()),
            SLOT_XSD => (self.xsd.orders_emitted(), self.xsd.orders_dropped()),
            SLOT_BIN15 => (self.bin15.orders_emitted(), self.bin15.orders_dropped()),
            SLOT_AI_EXEC => (self.ai_exec.orders_emitted(), self.ai_exec.orders_dropped()),
            SLOT_VM => (self.vm.orders_emitted(), self.vm.orders_dropped()),
            SLOT_XMM => (self.xmm.orders_emitted(), self.xmm.orders_dropped()),
            SLOT_HCV => (self.hcv.orders_emitted(), self.hcv.orders_dropped()),
            _ => return SlotCounters::default(),
        };
        let label = self.regime_labels[slot as usize];
        SlotCounters::new(emitted, dropped, label.n, label.off)
    }
    #[inline]
    fn vm_active_hash128(&self) -> [u8; 16] {
        self.vm.active_hash128()
    }
    #[inline]
    fn vm_staged_hash128(&self) -> [u8; 16] {
        self.vm.staged_hash128().unwrap_or([0; 16])
    }
    #[inline]
    fn vm_rows_view(&self, out: &mut [VmRowView]) -> u32 {
        self.vm.rows_view(out)
    }
    /// The detector's per-symbol REL state (its view minus the words).
    fn regime_rel_view(&self) -> RegimeRelView {
        let v = self.regime.view();
        RegimeRelView::new(v.syms, v.rel, v.n_syms)
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
    /// E5 — a cancel is stamped exactly as a submit is, and for the
    /// same reason: the router decides paper-vs-live on this byte,
    /// and an unstamped cancel would be routed by slot `0xFF & 7`.
    /// A member that could cancel through the wrong slot's arm is a
    /// member that can pull another member's quote.
    #[inline(always)]
    fn cancel(&mut self, mut req: core_types::CancelReq) -> Result<(), SubmitErr> {
        req.strategy_id = self.slot;
        self.inner.cancel(req)
    }
    /// E5 — likewise for the replacement carried by a modify. Note
    /// this stamps the NEW order; the resting order was stamped with
    /// the same slot when it was submitted, which is what makes the
    /// dispatcher's identity check pass.
    #[inline(always)]
    fn modify(&mut self, prev_client_oid: u64, mut order: Order) -> Result<(), SubmitErr> {
        order.strategy_id = self.slot;
        self.inner.modify(prev_client_oid, order)
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
        if self.initial & BIT_HYPARB != 0 {
            self.hyparb
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_HYPARB))?;
        }
        if self.initial & BIT_VRP != 0 {
            self.vrp.on_start(&mut StampCtx::new(&mut *ctx, SLOT_VRP))?;
        }
        if self.initial & BIT_XSD != 0 {
            self.xsd.on_start(&mut StampCtx::new(&mut *ctx, SLOT_XSD))?;
        }
        if self.initial & BIT_BIN15 != 0 {
            self.bin15
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_BIN15))?;
        }
        if self.initial & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC))?;
        }
        if self.initial & BIT_VM != 0 {
            self.vm.on_start(&mut StampCtx::new(&mut *ctx, SLOT_VM))?;
        }
        if self.initial & BIT_XMM != 0 {
            self.xmm
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_XMM))?;
        }
        if self.initial & BIT_HCV != 0 {
            self.hcv
                .on_start(&mut StampCtx::new(&mut *ctx, SLOT_HCV))?;
        }
        Ok(())
    }

    #[inline(always)]
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C) {
        // RG2: the detector sees every fresh tick first (one probe +
        // one store for members, one probe for everything else).
        self.regime.on_tick(tick);
        // HAR H3.3: so does the long-tenor set (one branch while inert).
        self.har.on_tick(tick);
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_tick(tick, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    #[inline(always)]
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C) {
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_signal(signal, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
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
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_venue_event(event, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// WS10-B: depth snapshots fan out to enabled members exactly
    /// like ticks/events — same mask gate, same slot stamping.
    #[inline(always)]
    fn on_depth<C: Ctx>(&mut self, depth: &core_types::DepthTopK, ctx: &mut C) {
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_depth(depth, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// VM2 V2: options records fan out to enabled members exactly
    /// like depth — same mask gate, same slot stamping.
    #[inline(always)]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &core_types::OptSummary, ctx: &mut C) {
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_opt_summary(opt, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// XMM XH1: trade prints fan out to enabled members exactly like
    /// ticks — same mask gate, same slot stamping. Every member but
    /// xmm inherits the default no-op.
    #[inline(always)]
    fn on_trade<C: Ctx>(&mut self, trade: &TradePrint, ctx: &mut C) {
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_trade(trade, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// XMM XH1: an order event goes to the slot that placed the order
    /// ALONE — the X1 fill law, and stricter: an event is never fanned
    /// out, because an event about slot 3's order handed to slot 6 would
    /// move slot 6's order state machine for an order it never sent. An
    /// event for a disabled, unbuilt or unattributed
    /// (`STRATEGY_ID_NONE`) slot is counted (`order_events_unrouted`),
    /// never delivered.
    #[inline(always)]
    fn on_order_event<C: Ctx>(&mut self, event: &OrderEvent, ctx: &mut C) {
        let slot = event.strategy_id;
        if slot >= 8 || self.enabled & (1u8 << slot) == 0 {
            self.order_events_unrouted = self.order_events_unrouted.wrapping_add(1);
            return;
        }
        self.route_order_event_to_slot(slot, event, ctx);
    }

    /// X1: an ATTRIBUTED fill goes to its slot ALONE.
    ///
    /// `Fill::strategy_id` mirrors `Order::strategy_id`, which the
    /// set's own `StampCtx` already stamps on every submit, so the
    /// paper matcher can hand a fill back to the member that asked for
    /// it. Fanning it out instead would give slot 1's option fill to
    /// slot 5 as well, and a member that infers a position from a
    /// callback would book someone else's trade.
    ///
    /// A fill for a DISABLED slot is counted (`fills_unrouted`) rather
    /// than delivered: the member is not running, and silently dropping
    /// it would hide a live order outliving a `DisableStrategy`.
    ///
    /// `STRATEGY_ID_NONE` still fans out. That is every VENUE fill —
    /// nothing on the wire says who asked — and the pre-X1 behaviour
    /// for every member that has not opted in.
    #[inline(always)]
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        if fill.strategy_id != core_types::STRATEGY_ID_NONE {
            let slot = fill.strategy_id;
            if slot >= 8 || self.enabled & (1u8 << slot) == 0 {
                self.fills_unrouted = self.fills_unrouted.wrapping_add(1);
                return;
            }
            self.route_fill_to_slot(slot, fill, ctx);
            return;
        }
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_fill(fill, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
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
        if self.enabled & BIT_HYPARB != 0 {
            self.hyparb
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0 {
            self.vrp.on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0 {
            self.xsd.on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0 {
            self.bin15
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0 {
            self.ai_exec
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0 {
            self.vm.on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0 {
            self.xmm
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0 {
            self.hcv
                .on_ai(cmd, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// 8g §6 item 7: the engine's pre-AI-drain table pop lands here —
    /// the ring slot itself, lent in place — forwarded to the slot-5
    /// vm member ([`VmStrategy::receive_table_v2`] — documented copy
    /// #2, slot → its staged buffer).
    /// Deliberately NOT mask-gated: staging is control plane and a
    /// staged table is inert until an in-stream `RulesetCommit`,
    /// which IS mask-gated through [`Strategy::on_ai`] — so an
    /// operator may stage while slot 5 is disabled and enable before
    /// committing without losing the table.
    #[inline]
    fn on_ruleset_table(&mut self, table: &RuleTableV2) {
        self.vm.receive_table_v2(table);
    }

    /// XMM XH1: each timer client runs on ITS OWN period (module docs,
    /// "Timers are per slot"): the regime detector and the long-tenor
    /// set every [`REGIME_TIMER_NS`] once configured, and each enabled
    /// member when its own `timer_period_ns` has elapsed since its own
    /// last call.
    ///
    /// Before XH1 every enabled member ran on every call, at the set's
    /// minimum period. The members that existed then all run a 1 s
    /// period or an empty `on_timer` (hyparb, xsd and bin15 when
    /// configured; vrp, ai-exec and vm never), and without xmm the set's
    /// own period is that same 1 s — so a member's clock equals the
    /// engine's and each fires exactly when it did. A `u64::MAX` member
    /// is no longer called at all, which its empty body cannot notice.
    #[inline(always)]
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        // RG2: roll the minute clock (nothing until a boundary) and
        // re-judge the gates only when an effective word changed.
        // RG3: a roll that changed no word may still have moved a
        // member's REL — the vm's `rel:` rows get the view anyway.
        // An unconfigured detector's timer is a no-op, so it arms none.
        let regime_period = if self.regime.is_configured() {
            REGIME_TIMER_NS
        } else {
            u64::MAX
        };
        if Self::timer_due(&mut self.regime_timer_last_ns, regime_period, now_ns) {
            let minutes_before = self.regime.minutes_judged();
            if self.regime.on_timer(now_ns) != 0 {
                self.refresh_gates(ctx);
            } else if self.regime.minutes_judged() != minutes_before {
                self.push_regime_views();
            }
        }
        // HAR H3.3: the long-tenor minute roll and its staggered day
        // closes, every 1 s once configured, on its own clock (XMM XH1's
        // per-slot timers; an inert set arms none). H3.5: then the
        // `/state.har` row of the series whose day just closed.
        let har_period = if self.har.is_configured() {
            REGIME_TIMER_NS
        } else {
            u64::MAX
        };
        if Self::timer_due(&mut self.har_timer_last_ns, har_period, now_ns) {
            self.har.on_timer(now_ns);
            self.refresh_har_view();
            self.offer_har_state();
        }
        if self.enabled & BIT_HYPARB != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_HYPARB as usize],
                self.hyparb.timer_period_ns(),
                now_ns,
            )
        {
            self.hyparb
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        }
        if self.enabled & BIT_VRP != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_VRP as usize],
                self.vrp.timer_period_ns(),
                now_ns,
            )
        {
            self.vrp
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_VRP));
        }
        if self.enabled & BIT_XSD != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_XSD as usize],
                self.xsd.timer_period_ns(),
                now_ns,
            )
        {
            self.xsd
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_XSD));
        }
        if self.enabled & BIT_BIN15 != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_BIN15 as usize],
                self.bin15.timer_period_ns(),
                now_ns,
            )
        {
            self.bin15
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        }
        if self.enabled & BIT_AI_EXEC != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_AI_EXEC as usize],
                self.ai_exec.timer_period_ns(),
                now_ns,
            )
        {
            self.ai_exec
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        }
        if self.enabled & BIT_VM != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_VM as usize],
                self.vm.timer_period_ns(),
                now_ns,
            )
        {
            self.vm
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_VM));
        }
        if self.enabled & BIT_XMM != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_XMM as usize],
                self.xmm.timer_period_ns(),
                now_ns,
            )
        {
            self.xmm
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_XMM));
        }
        if self.enabled & BIT_HCV != 0
            && Self::timer_due(
                &mut self.timer_last_ns[SLOT_HCV as usize],
                self.hcv.timer_period_ns(),
                now_ns,
            )
        {
            self.hcv
                .on_timer(now_ns, &mut StampCtx::new(&mut *ctx, SLOT_HCV));
        }
    }

    /// Minimum over the BUILT members (mask-independent so the
    /// engine's timer arming is stable across runtime Enable/Disable;
    /// `on_timer` gates each member on its own period). A configured
    /// regime detector or long-tenor set arms the 1 s
    /// [`REGIME_TIMER_NS`] poll.
    fn timer_period_ns(&self) -> u64 {
        let mut min = if self.regime.is_configured() || self.har.is_configured() {
            REGIME_TIMER_NS
        } else {
            u64::MAX
        };
        let v = self.hyparb.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.vrp.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.xsd.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.bin15.timer_period_ns();
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
        let v = self.xmm.timer_period_ns();
        if v < min {
            min = v;
        }
        let v = self.hcv.timer_period_ns();
        if v < min {
            min = v;
        }
        min
    }

    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        // HAR H3.3: a minute still held by the stagger goes in before
        // the shutdown state write.
        self.har.drain_held();
        // Stop is unconditional — even disabled members get the
        // teardown callback (they may hold capture-worthy state some
        // day; today all six are no-ops).
        self.hyparb
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_HYPARB));
        self.vrp.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_VRP));
        self.xsd.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_XSD));
        self.bin15
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_BIN15));
        self.ai_exec
            .on_stop(&mut StampCtx::new(&mut *ctx, SLOT_AI_EXEC));
        self.vm.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_VM));
        self.xmm.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_XMM));
        self.hcv.on_stop(&mut StampCtx::new(&mut *ctx, SLOT_HCV));
    }
}

// ---------------------------------------------------------------
// HAR H3.5: one series' `/state.har` row
// ---------------------------------------------------------------

/// An annualised σ ×1e9 as the row's ×1e6 `i32` (`0` = none; a σ past
/// ~2 147 annualised saturates — no real series is near it).
#[inline]
fn har_sigma_1e6(s: Option<i64>) -> i32 {
    match s {
        Some(v) => (v / 1_000).clamp(0, i64::from(i32::MAX)) as i32,
        None => 0,
    }
}

/// Build series `i`'s row from its engine: the census of resident days,
/// both forecasts, the pairs, the fit and QLIKE bits at every panel tenor
/// and the weekday profile. The per-minute fields are left for the reader
/// (`har_series_view` reads them live). Cold: once per series per UTC
/// day, and once after the restore.
fn har_row(set: &LongVolSet, i: usize) -> HarSeriesView {
    let mut v = HarSeriesView::default();
    let (Some(e), Some(name), Some(feed)) = (set.engine(i), set.name(i), set.feed(i)) else {
        return v;
    };
    let nl = name.len().min(HAR_VIEW_NAME_MAX);
    v.name[..nl].copy_from_slice(&name[..nl]);
    v.name_len = nl as u8;
    v.feed = feed;
    v.warm = u8::from(e.is_warm());
    let days = e.n_resident();
    v.days = days.min(usize::from(u8::MAX)) as u8;
    let mut empty = 0u8;
    let mut d = 0usize;
    while d < days {
        if let Some((ts, _, n_min)) = e.day_at(d) {
            empty = empty.saturating_add(u8::from(n_min == 0));
            v.newest_day_ms = ts;
        }
        d += 1;
    }
    v.empty_days = empty;
    let mut k = 0usize;
    while k < HAR_VIEW_TENORS {
        let tau = u64::from(HAR_VIEW_TENORS_D[k]) * DAY_NS;
        v.raw_1e6[k] = har_sigma_1e6(e.sigma_ann_1e9(tau, LongForecast::Raw));
        v.fit_1e6[k] = har_sigma_1e6(e.sigma_ann_1e9(tau, LongForecast::Fit));
        v.pairs[k] = e.n_pairs(tau).min(usize::from(u8::MAX)) as u8;
        v.fitted |= u16::from(e.fit(tau).is_some()) << k;
        v.fit_beats_raw |= u16::from(e.qlike_counters(tau).fit_beats_raw) << k;
        k += 1;
    }
    if let Some((ratio, n_days)) = set.profile(i) {
        let mut w = 0usize;
        while w < HAR_VIEW_WEEKDAYS {
            v.weekday_1e6[w] = ratio[w].clamp(0, i64::from(i32::MAX)) as i32;
            v.weekday_n[w] = n_days[w].min(u32::from(u8::MAX)) as u8;
            w += 1;
        }
    }
    v.epoch = set.series_epoch(i);
    v
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

    /// The set-level PROBE member (HYPARB H0): the latency-arb trigger
    /// pair left with its member (O-H1), so the fan-out / mask / halt
    /// laws are pinned through the ai-exec member instead — a
    /// Heartbeat then one OrderIntent emits exactly one order when
    /// slot 4 is live and nothing when it is not.
    const PROBE_BIT: u8 = BIT_AI_EXEC;
    const PROBE_SLOT: u8 = SLOT_AI_EXEC;

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

    /// Feed the probe: Heartbeat (restores ai-exec liveness, §5.4)
    /// then one OrderIntent for slot 4.
    fn feed_probe<C: Ctx>(s: &mut StrategySet, c: &mut C) {
        s.on_ai(&ai_cmd(AiCmdKind::Heartbeat, STRATEGY_SLOT_NONE), c);
        let pm = make_symbol_id(VenueId::Polymarket, 3);
        s.on_ai(&intent_cmd(2, pm, 430_000, 2_000_000), c);
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

    /// The set's timer cadence with every member unconfigured.
    fn s_timer_without_xsd() -> u64 {
        StrategySet::new(0).timer_period_ns()
    }

    /// A set with slot 0 enabled and its member configured: one
    /// observe-only pool, one perp hedge coin. Tests that need "some
    /// enabled member" use slot 0 — since H4 it validates in `on_start`.
    fn hyparb_set() -> StrategySet {
        let mut s = StrategySet::new(BIT_HYPARB);
        let mut p = strategy_hyparb::HyparbParams::EMPTY;
        p.coins[0] = strategy_hyparb::CoinParams {
            perp_sym: make_symbol_id(VenueId::Hyperliquid, 5),
            spot_sym: core_types::SYMBOL_ID_NONE,
            lot_1e6: 10_000,
            min_notional_usd_1e6: 10_000_000,
        };
        p.n_coins = 1;
        p.pools[0] = strategy_hyparb::PoolParams {
            sym: make_symbol_id(VenueId::HyperEvm, 1),
            coin0: 0,
            coin1: strategy_hyparb::COIN_USD,
            trade: false,
            max_notional_usd_1e6: 1_000_000,
        };
        p.n_pools = 1;
        p.lag_ns = 1;
        p.basis_window_ns = 1;
        p.max_order_usd_1e6 = 1;
        p.cap_day_usd_1e6 = 1;
        p.inventory_cap_usd_1e6 = 1;
        s.hyparb_mut()
            .configure(p, core_time::WallAnchor::new(0, 0))
            .expect("hyparb params");
        s
    }

    #[test]
    fn initial_mask_from_names() {
        // HYPARB H0 (O-H1): slot 0 is the hyparb member and
        // `latency-arb` is gone as a NAME — the old name refuses the
        // boot rather than composing a different member.
        assert_eq!(mask_for_name("latency-arb"), None);
        assert_eq!(mask_for_name("hyparb"), Some(BIT_HYPARB));
        assert_eq!(
            mask_for_name("hyparb"),
            Some(1),
            "slot 0's bit is wire-stable across the swap"
        );
        assert_eq!(mask_for_name("ai+hyparb"), Some(49));
        assert_eq!(mask_for_name("ai+vrp+xsd+bin15+hyparb"), Some(63));
        // VRP V7: slot 1 is the VRP member and `ev` is gone as a NAME.
        // An operator who types the old one gets a boot refusal rather
        // than a different strategy than the one they asked for.
        assert_eq!(mask_for_name("ev"), None);
        assert_eq!(mask_for_name("vrp"), Some(BIT_VRP));
        assert_eq!(mask_for_name("vrp"), Some(2), "the slot NUMBER is wire-stable");
        assert_eq!(
            mask_for_name("ai+vrp"),
            Some(BIT_AI_EXEC | BIT_VM | BIT_VRP)
        );
        assert_eq!(mask_for_name("ai+vrp"), Some(50));
        // XSD-S/XSD-3 (2026-09-12): slot 2 is the xsd member; `cross-arb`
        // is gone as a NAME and refuses the boot. The live masks do not
        // move.
        assert_eq!(mask_for_name("cross-arb"), None);
        assert_eq!(mask_for_name("xsd"), Some(BIT_XSD));
        assert_eq!(mask_for_name("xsd"), Some(4), "slot 2's bit is wire-stable across the swap");
        assert_eq!(mask_for_name("ai+xsd"), Some(BIT_AI_EXEC | BIT_VM | BIT_XSD));
        assert_eq!(mask_for_name("ai+xsd"), Some(52));
        assert_eq!(mask_for_name("ai+vrp+xsd"), Some(54));
        assert_eq!(mask_for_name("ai"), Some(48));
        assert_eq!(mask_for_name("ai+vrp"), Some(50));
        // XMM XH1 (O-XH1): slot 6 is the xmm member; `icdp` and
        // `ai+icdp` are gone as NAMES and refuse the boot.
        assert_eq!(mask_for_name("icdp"), None);
        assert_eq!(mask_for_name("ai+icdp"), None);
        assert_eq!(mask_for_name("xmm"), Some(BIT_XMM));
        assert_eq!(mask_for_name("xmm"), Some(64), "slot 6's bit is wire-stable across the swap");
        assert_eq!(mask_for_name("ai+xmm"), Some(112));
        assert_eq!(mask_for_name("ai+vrp+xsd+bin15+hyparb+xmm"), Some(127));
        // HC11 (O-HC18): slot 7 is the hcv member.
        assert_eq!(mask_for_name("hcv"), Some(BIT_HCV));
        assert_eq!(mask_for_name("hcv"), Some(128), "slot 7's bit");
        assert_eq!(mask_for_name("ai+hcv"), Some(176));
        assert_eq!(mask_for_name("ai+vrp+xsd+bin15+hyparb+xmm+hcv"), Some(255));
        assert_eq!(mask_for_name("bin15"), Some(BIT_BIN15));
        assert_eq!(mask_for_name("rule-tree"), None, "the old name is GONE");
        assert_eq!(
            mask_for_name("ai+vrp+xsd+bin15"),
            Some(BIT_AI_EXEC | BIT_VM | BIT_VRP | BIT_XSD | BIT_BIN15)
        );
        assert_eq!(mask_for_name("ai-exec"), Some(BIT_AI_EXEC));
        assert_eq!(mask_for_name("vm"), Some(BIT_VM));
        assert_eq!(mask_for_name("ai"), Some(BIT_AI_EXEC | BIT_VM));
        assert_eq!(mask_for_name("all"), Some(BUILT_MASK));
        assert_eq!(mask_for_name("all"), Some(255), "every slot 0..=7 is built since HC11");
        // `ai` = AI-pushed lanes only — NO Rust-coded strategy bit
        // (operator ruling 2026-09-02).
        const _: () = assert!(
            (BIT_AI_EXEC | BIT_VM) & BIT_HYPARB == 0,
            "`ai` excludes hyparb"
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
        const _: () = assert!(BUILT_MASK == 0xFF, "no reserved bit is left since HC11");
        let s = StrategySet::new(0b1000_0000);
        assert_eq!(s.enabled_mask(), BIT_HCV, "slot 7 is built (hcv since HC11)");
        let s = StrategySet::new(BIT_XSD);
        assert_eq!(s.enabled_mask(), BIT_XSD, "slot 2 is built now (XSD-3)");
        let s = StrategySet::new(BIT_XMM);
        assert_eq!(s.enabled_mask(), BIT_XMM, "slot 6 is built (xmm since XMM XH1)");
        let s = StrategySet::new(BIT_AI_EXEC);
        assert_eq!(s.enabled_mask(), BIT_AI_EXEC, "slot 4 is built now");
        let s = StrategySet::new(BIT_VM);
        assert_eq!(s.enabled_mask(), BIT_VM, "slot 5 is built now (8g)");
    }

    #[test]
    fn on_start_validates_initially_enabled_members_only() {
        // Probe enabled + valid → ok.
        let mut s = StrategySet::new(PROBE_BIT);
        assert!(s.on_start(&mut ctx()).is_ok());

        // Probe enabled + INVALID → the member's own validation error
        // propagates (fail-fast preserved).
        let mut s = StrategySet::new(PROBE_BIT);
        s.ai_exec_mut().set_edge_1e6(0);
        assert!(matches!(
            s.on_start(&mut ctx()),
            Err(StrategyError::Config(_))
        ));

        // Unconfigured members outside the initial mask are skipped —
        // vrp/bin15 would all fail validation here.
        let mut s = StrategySet::new(PROBE_BIT);
        assert!(s.on_start(&mut ctx()).is_ok());
    }

    /// HYPARB H4: slot 0 is a REAL member — unconfigured it refuses the
    /// boot (fail-fast, like vrp/bin15; the cli never puts it in the
    /// configured mask without its artifact, O-H8), configured it is
    /// inert on anything but its own pools and hedge books, and its
    /// 1 s timer joins the set's minimum.
    #[test]
    fn hyparb_slot_refuses_unconfigured_and_is_inert_configured() {
        let mut s = StrategySet::new(BIT_HYPARB);
        assert!(matches!(
            s.on_start(&mut ctx()),
            Err(StrategyError::Config(_))
        ));
        let mut s = hyparb_set();
        let mut c = ctx();
        assert!(s.on_start(&mut c).is_ok());
        s.on_tick(&tick(VenueId::Binance, BN, 490_000, 510_000), &mut c);
        s.on_tick(&tick(VenueId::Polymarket, PM, 390_000, 410_000), &mut c);
        assert_eq!(c.submitted, 0);
        assert_eq!(s.orders_emitted(), 0);
        assert_eq!(
            s.timer_period_ns(),
            s_timer_without_xsd().min(1_000_000_000)
        );
        assert_eq!(
            s.hyparb_counters(),
            strategy_core::HyparbCounters::default()
        );
        let mut rows = [strategy_core::HyparbPoolView::default(); 2];
        assert_eq!(s.hyparb_pools_view(&mut rows), 1);
        assert_eq!(rows[0].live, 0, "no snapshot yet");
    }

    #[test]
    fn mask_fan_out_gates_member_callbacks() {
        // Enabled: the probe pair emits exactly one order.
        let mut s = StrategySet::new(PROBE_BIT);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_probe(&mut s, &mut c);
        assert_eq!(c.submitted, 1);
        assert_eq!(s.orders_emitted(), 1);

        // Same feed with the bit off: nothing reaches the member.
        let mut s = StrategySet::new(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_probe(&mut s, &mut c);
        assert_eq!(c.submitted, 0);
        assert_eq!(s.orders_emitted(), 0);
    }

    /// X1: an ATTRIBUTED fill reaches its slot and nobody else.
    ///
    /// Without this, the paper matcher's fill for slot 1's option would
    /// also be handed to slot 5, and a member that infers a position
    /// from a callback would book someone else's trade.
    #[test]
    fn an_attributed_fill_reaches_only_its_own_slot() {
        let fill_for = |slot: u8| {
            core_types::Fill::new(
                1,
                7,
                Side::Bid,
                core_types::Price::from_raw(1_000_000),
                core_types::Qty::from_raw(1_000_000),
                99,
            )
            .with_attribution(slot, core_types::FILL_ORIGIN_PAPER)
        };

        // A fill for a DISABLED slot is counted, never delivered.
        let mut s = hyparb_set();
        let mut c = ctx();
        s.on_fill(&fill_for(SLOT_VRP), &mut c);
        assert_eq!(s.fills_unrouted(), 1, "slot 1 is not enabled here");

        // And a slot that is not BUILT at all.
        s.on_fill(&fill_for(7), &mut c);
        assert_eq!(s.fills_unrouted(), 2, "slot 7 is not enabled here");
        s.on_fill(&fill_for(200), &mut c);
        assert_eq!(s.fills_unrouted(), 3, "nor does slot 200");

        // An UNATTRIBUTED fill — every venue fill — still fans out, and
        // is never counted unrouted.
        let venue = core_types::Fill::new(
            1,
            7,
            Side::Bid,
            core_types::Price::from_raw(1_000_000),
            core_types::Qty::from_raw(1_000_000),
            99,
        );
        assert_eq!(venue.strategy_id, core_types::STRATEGY_ID_NONE);
        s.on_fill(&venue, &mut c);
        assert_eq!(s.fills_unrouted(), 3, "a venue fill is not unrouted");
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

        // Through the SET: the probe pair emits ONE order stamped
        // with the probe's slot by the dispatch wrapper (M-c).
        let mut s = StrategySet::new(PROBE_BIT);
        let mut rec = RecordCtx { orders: Vec::new() };
        s.on_start(&mut rec).unwrap();
        feed_probe(&mut s, &mut rec);
        assert_eq!(rec.orders.len(), 1);
        assert_eq!(rec.orders[0].strategy_id, PROBE_SLOT);
    }

    #[test]
    fn enable_via_ai_activates_member() {
        let mut s = StrategySet::new(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, PROBE_SLOT), &mut c);
        assert_eq!(s.enabled_mask(), PROBE_BIT);
        feed_probe(&mut s, &mut c);
        assert_eq!(c.submitted, 1, "enabled-at-runtime member must fire");
    }

    #[test]
    fn disable_always_honored() {
        let mut s = StrategySet::new(PROBE_BIT);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, PROBE_SLOT), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        feed_probe(&mut s, &mut c);
        assert_eq!(c.submitted, 0);

        // Disable also works while halted.
        let mut s = StrategySet::new(PROBE_BIT);
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, PROBE_SLOT), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        assert!(s.is_halted());
    }

    #[test]
    fn halt_clears_mask_and_sticks() {
        let mut s = StrategySet::new(PROBE_BIT);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        assert!(s.is_halted());
        assert_eq!(s.enabled_mask(), 0, "halt is a kill-switch: mask cleared");
        feed_probe(&mut s, &mut c);
        assert_eq!(c.submitted, 0);
    }

    #[test]
    fn enable_while_halted_refused_and_counted() {
        let mut s = hyparb_set();
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::HaltRequest, STRATEGY_SLOT_NONE), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_HYPARB), &mut c);
        assert_eq!(s.enabled_mask(), 0, "enable refused while halted");
        assert_eq!(s.enable_refused_total(), 1);
        assert_eq!(StrategyCounters::ai_enable_refused(&s), 1);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_VRP), &mut c);
        assert_eq!(s.enable_refused_total(), 2);
    }

    /// Migrated from slot 5 in 8g item 6 (§8), from slot 6 in ICDP I4
    /// (slot 6 is xmm since XMM XH1) and from slot 7 in HC11 (hcv): no
    /// slot is reserved any more — ids past 7 are the unknown ones
    /// (probed twice).
    #[test]
    fn enable_reserved_or_unknown_slot_refused() {
        let mut s = StrategySet::new(0);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, 8), &mut c);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, 9), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        assert_eq!(s.enable_refused_total(), 2);
        assert!(!s.is_halted(), "unknown-slot refusal is not a halt");
        // Slot 7 enables: an unconfigured hcv member is inert and arms no
        // timer (HC11).
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_HCV), &mut c);
        assert_eq!(s.enabled_mask(), BIT_HCV);
        assert_eq!(s.timer_period_ns(), s_timer_without_xsd());
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, SLOT_HCV), &mut c);
        assert_eq!(s.enabled_mask(), 0);
        // Slot 6 enables (an unconfigured xmm member is inert: it never
        // fires and arms no timer — XMM XH1); slot 2 likewise (XSD-3: an
        // unconfigured xsd member maps no sym and arms no timer).
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_XMM), &mut c);
        assert_eq!(s.enabled_mask(), BIT_XMM);
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_XSD), &mut c);
        assert_eq!(s.enabled_mask(), BIT_XMM | BIT_XSD);
        assert_eq!(s.enable_refused_total(), 2);
        assert_eq!(s.timer_period_ns(), s_timer_without_xsd(), "an unconfigured xsd arms no timer");
    }

    #[test]
    fn enable_ai_exec_slot_is_honored() {
        let mut s = StrategySet::new(0);
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
        let mut s = StrategySet::new(0);
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
        let mut s = hyparb_set();
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
        assert_eq!(s.enabled_mask(), BIT_HYPARB);
        assert!(!s.is_halted());
        assert_eq!(s.enable_refused_total(), 0);
        assert_eq!(c.submitted, 0);
    }

    #[test]
    fn counters_aggregate_members_and_kind_is_set() {
        let mut s = StrategySet::new(PROBE_BIT);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        feed_probe(&mut s, &mut c);
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
        let mut s = hyparb_set();
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
        let mut s = hyparb_set();
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

    /// One cross_deviation row on the (PM, BN) test pair; horizon 0
    /// keeps every eval armed.
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
        let mut s = hyparb_set();
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
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_HYPARB), &mut c);
        assert_eq!(
            StrategyCounters::enabled_mask(&s),
            u64::from(BIT_VM | BIT_HYPARB)
        );
        s.on_ai(&ai_cmd(AiCmdKind::DisableStrategy, SLOT_HYPARB), &mut c);
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
        let mut s = hyparb_set();
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
            // Labels of unconstrained members are ANY; the vm and hcv
            // (slot 7: it stores no label) cannot be relabelled.
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

    /// BIN15 O5: `MASK_TABLE` is the only name list, so it must not
    /// carry a name twice — a duplicate would silently shadow the
    /// second mask under the linear scan.
    #[test]
    fn mask_table_has_no_duplicate_names() {
        let mut i = 0;
        while i < MASK_TABLE.len() {
            let mut j = i + 1;
            while j < MASK_TABLE.len() {
                assert_ne!(MASK_TABLE[i].0, MASK_TABLE[j].0, "duplicate name in MASK_TABLE");
                j += 1;
            }
            i += 1;
        }
    }

    /// Every table row must resolve through `mask_for_name` to its own
    /// mask — the scan and the table cannot disagree.
    #[test]
    fn mask_table_and_mask_for_name_agree() {
        let mut i = 0;
        while i < MASK_TABLE.len() {
            let (name, mask) = MASK_TABLE[i];
            assert_eq!(mask_for_name(name), Some(mask), "{name}");
            assert!(!name.is_empty(), "an empty name would shadow the None case");
            i += 1;
        }
    }

    /// Every mask in the table must be built — a name that composes a
    /// reserved slot would boot a member that does not exist.
    #[test]
    fn every_table_mask_is_built() {
        let mut i = 0;
        while i < MASK_TABLE.len() {
            let (name, mask) = MASK_TABLE[i];
            assert_eq!(mask & !BUILT_MASK, 0, "{name} composes an unbuilt slot");
            i += 1;
        }
    }

    // ---- HC11 ---------------------------------------------------------

    /// HC11 (O-HC22, law L4 lifted for slot 7 alone): the set hands the
    /// hcv member the HAR rows each time one moves — enabled or not — and
    /// never when nothing moved.
    #[test]
    fn the_har_view_reaches_hcv_when_a_row_moves_enabled_or_not() {
        use core_vol::LongSeries;
        const DAY_MS: u64 = 86_400_000;
        let feed = make_symbol_id(VenueId::Binance, 100);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        c.now = 0;
        s.on_start(&mut c).unwrap();
        let wall0_ms: u64 = 1_767_225_600_000;
        let anchor = WallAnchor::new(0, wall0_ms * 1_000_000);
        s.har_mut()
            .configure(&[LongSeries { name: b"SP500", feed }], anchor, 0)
            .unwrap();
        {
            let e = s.har_mut().engine_mut(0).unwrap();
            let first = wall0_ms - 40 * DAY_MS;
            for d in 0..40u64 {
                assert!(e.seed_day(first + d * DAY_MS, 4_000_000_000_000_000_000_000, 1_440));
            }
            assert!(e.seed_arm(core_vol::DAY_NS, first + 39 * DAY_MS, 25_300_000_000, i64::MIN));
        }
        s.har_mut().restored();
        assert_eq!(s.hcv().counters().har_updates, 0, "nothing handed before the poll");
        s.on_timer(REGIME_TIMER_NS, &mut c);
        assert_eq!(s.hcv().counters().har_updates, 1, "the restore's rows, slot 7 disabled");
        s.on_timer(2 * REGIME_TIMER_NS, &mut c);
        assert_eq!(s.hcv().counters().har_updates, 1, "no row moved: nothing handed");
    }

    /// HC11: slot 7 trades through the set — its inputs fan in, its order
    /// carries slot 7, and its paper fill comes back to it alone.
    #[test]
    fn hcv_trades_through_the_set_on_slot_7() {
        struct Rec(Vec<Order>, NsTs);
        impl Ctx for Rec {
            fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
                self.0.push(order);
                Ok(())
            }
            fn now_ns(&self) -> NsTs {
                self.1
            }
        }
        const DAY_MS: u64 = 86_400_000;
        let t0_ms: u64 = 1_790_424_000_000;
        let hedge = make_symbol_id(VenueId::Hyperliquid, 40);
        let opt = make_symbol_id(VenueId::Hypercall, 513);
        let p = strategy_hcv::HcvParams {
            und: vec![strategy_hcv::HcvUnd::new(b"SP500", hedge, 2)],
            options: vec![strategy_hcv::HcvOpt {
                sym: opt,
                und: 0,
                call: true,
                strike_1e6: 6_600_000_000,
                exp_ms: t0_ms + 7 * DAY_MS,
            }],
            theta_vol_1e6: 50_000,
            atm_band_bps: 500,
            tenor_min_d: 1,
            tenor_max_d: 40,
            clip_usd_1e6: 5_000_000,
            vega_cap_usd_1e6: 20_000_000,
            premium_cap_usd_1e6: 50_000_000,
            day_loss_usd_1e6: 20_000_000,
            tail_loss_usd_1e6: 1_000_000_000,
            opt_size_step_1e6: 1_000,
            hedge_band_1e6: 100_000,
            hedge_min_usd_1e6: 10_000_000,
            hedge_slip_bps: 10,
            quote_stale_ms: 10_000,
            oracle_stale_ms: 10_000,
            unwind_min: 30,
            settle_delay_ms: 60_000,
            settle_order: strategy_hcv::BucketOrder::Sorted,
            event_law: false,
            events_stale_ms: 7_200_000,
            kill: false,
            timer_ms: 1_000,
            anchor: WallAnchor::new(0, t0_ms * 1_000_000),
        };
        let mut s = StrategySet::new(BIT_HCV);
        s.hcv_mut().configure(&p).expect("hcv params");
        let mut row = HarSeriesView::default();
        row.name[..5].copy_from_slice(b"SP500");
        row.name_len = 5;
        row.warm = 1;
        row.raw_1e6 = [150_000; HAR_VIEW_TENORS];
        s.hcv_mut().set_har_view(&[row]);
        let mut c = Rec(Vec::new(), 0);
        s.on_start(&mut c).unwrap();
        assert_eq!(s.timer_period_ns(), 1_000_000_000, "a configured hcv arms its 1 s timer");
        let ev = ChannelEvent::new(10, VenueId::Hyperliquid, ChannelId::Mark, hedge, 0, 0, 6_600_000_000, 6_600_000_000);
        s.on_venue_event(&ev, &mut c);
        // Stamped ticks (a zero stamp is "never seen" to the member).
        let stamped = |venue, sym, bid, ask| {
            let mut t = tick(venue, sym, bid, ask);
            t.ts_ns = 10;
            t
        };
        s.on_tick(&stamped(VenueId::Hyperliquid, hedge, 6_599_500_000, 6_600_500_000), &mut c);
        // The ATM call quoted rich: ~$80 bid is ~22 % against a 15 % σ̂.
        s.on_tick(&stamped(VenueId::Hypercall, opt, 80_000_000, 86_000_000), &mut c);
        s.on_timer(1_000_000_000, &mut c);
        assert_eq!(c.0.len(), 1, "{:?}", s.hcv().counters());
        let o = c.0[0];
        assert_eq!((o.strategy_id, o.venue, o.sym, o.side), (SLOT_HCV, VenueId::Hypercall as u8, opt, Side::Ask));
        assert_eq!(StrategyCounters::slot_counters(&s, SLOT_HCV).orders_emitted, 1);
        // Its paper fill, attributed to slot 7, reaches it alone.
        let f = core_types::Fill::new(2, opt, Side::Ask, o.px, o.qty, o.client_oid)
            .with_attribution(SLOT_HCV, core_types::FILL_ORIGIN_PAPER);
        s.on_fill(&f, &mut c);
        assert_eq!(s.hcv().position_1e6(opt), -o.qty.raw());
        assert_eq!(s.fills_unrouted(), 0);
        let mut k = strategy_core::HcvCounters::default();
        StrategyCounters::hcv_counters(&s, &mut k);
        assert_eq!((k.sells, k.option_fills), (1, 1));
    }

    // ---- XMM XH1 ----------------------------------------------------

    const XMM_HL: SymbolId = make_symbol_id(VenueId::Hyperliquid, 2);

    /// A valid probe-shaped artifact: one perp and its leader.
    fn xmm_params() -> strategy_xmm::XmmParams {
        let mut p = strategy_xmm::XmmParams::EMPTY;
        p.perps[0] = strategy_xmm::XmmPerp {
            hl_sym: XMM_HL,
            lead_sym: make_symbol_id(VenueId::Binance, 5),
            lot_1e6: 10_000,
        };
        p.n_perps = 1;
        p.maker_enabled = 1;
        p.theta_bps_1e6 = 500_000;
        p.gate_window_ms = 500;
        p.lifetime_ms = 30_000;
        p.lead_stale_ms = 300;
        p.follower_stale_ms = 2_000;
        p.rtt_pull_ms = 1_500;
        p.requote_min_ms = 250;
        p.clip_usd_1e6 = 15_000_000;
        p.inv_cap_usd_1e6 = 150_000_000;
        p.gross_inv_cap_usd_1e6 = 400_000_000;
        p.resting_cap_usd_1e6 = 300_000_000;
        p
    }

    fn print(tid: u64) -> TradePrint {
        TradePrint::new(
            1,
            VenueId::Hyperliquid,
            XMM_HL,
            tid,
            0,
            186_000_000,
            2_000_000,
            core_types::TRADE_AGGRESSOR_SELL,
        )
    }

    /// Slot 6 is a REAL member: unconfigured it refuses the boot (the
    /// cli never puts it in the configured mask without its artifact);
    /// configured it starts and, at XH1, stays dark — no order, no timer.
    #[test]
    fn xmm_slot_refuses_unconfigured_and_quotes_once_it_has_both_feeds() {
        let mut s = StrategySet::new(BIT_XMM);
        assert!(matches!(
            s.on_start(&mut ctx()),
            Err(StrategyError::Config(_))
        ));
        let mut s = StrategySet::new(BIT_XMM);
        s.xmm_mut().configure(&xmm_params()).expect("xmm params");
        let mut c = ctx();
        assert!(s.on_start(&mut c).is_ok());
        // XH2: no leader yet — the follower and the tape alone place nothing.
        s.on_tick(&tick(VenueId::Hyperliquid, XMM_HL, 185_990_000, 186_000_000), &mut c);
        s.on_trade(&print(1), &mut c);
        s.on_timer(c.now, &mut c);
        assert_eq!(c.submitted, 0);
        // With its leader fresh, both sides go out at the touch.
        let lead = make_symbol_id(VenueId::Binance, 5);
        s.on_tick(&tick(VenueId::Binance, lead, 185_980_000, 186_010_000), &mut c);
        s.on_tick(&tick(VenueId::Hyperliquid, XMM_HL, 185_990_000, 186_000_000), &mut c);
        assert_eq!(c.submitted, 2);
        assert_eq!(s.orders_emitted(), 2);
        assert_eq!(
            s.timer_period_ns(),
            strategy_xmm::XMM_TIMER_NS,
            "xmm's safety timer is the set's shortest"
        );
        assert!(s.timer_period_ns() < s_timer_without_xsd());
    }

    /// A print reaches every enabled member and changes nothing for the
    /// members that existed before XH1: the probe emits exactly its one
    /// order with or without the tape.
    #[test]
    fn a_trade_print_changes_no_existing_member() {
        let mut quiet = StrategySet::new(PROBE_BIT);
        let mut cq = ctx();
        quiet.on_start(&mut cq).unwrap();
        feed_probe(&mut quiet, &mut cq);

        let mut taped = StrategySet::new(PROBE_BIT);
        let mut ct = ctx();
        taped.on_start(&mut ct).unwrap();
        taped.on_trade(&print(1), &mut ct);
        feed_probe(&mut taped, &mut ct);
        taped.on_trade(&print(2), &mut ct);

        assert_eq!((cq.submitted, ct.submitted), (1, 1));
        assert_eq!(quiet.orders_emitted(), taped.orders_emitted());
        assert_eq!(taped.enabled_mask(), PROBE_BIT);
    }

    /// An order event reaches its own enabled slot alone; an event for
    /// a disabled slot (slot 7 here), or unattributed, is counted and
    /// never delivered — never fanned out.
    #[test]
    fn an_order_event_reaches_only_its_own_slot_or_is_counted() {
        let mut s = StrategySet::new(PROBE_BIT);
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        let ev = |slot: u8| {
            OrderEvent::new(
                1,
                VenueId::Hyperliquid,
                XMM_HL,
                5,
                slot,
                core_types::ORDER_EVENT_CANCELED,
                core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED,
                0,
            )
        };
        s.on_order_event(&ev(PROBE_SLOT), &mut c);
        assert_eq!(s.order_events_unrouted(), 0, "an enabled slot's event is delivered");
        s.on_order_event(&ev(SLOT_VRP), &mut c);
        assert_eq!(s.order_events_unrouted(), 1, "a disabled slot's event is counted");
        s.on_order_event(&ev(7), &mut c);
        assert_eq!(s.order_events_unrouted(), 2, "slot 7 is not enabled here");
        s.on_order_event(&ev(core_types::STRATEGY_ID_NONE), &mut c);
        assert_eq!(s.order_events_unrouted(), 3, "an unattributed event is never fanned out");
        // XMM XH2: the same count through the trait the metrics read.
        assert_eq!(StrategyCounters::order_events_unrouted(&s), 3);
        assert_eq!(c.submitted, 0);
    }

    /// The per-slot gate: fires at its period and not before, advances
    /// only when it fires, never fires for `u64::MAX`, and never for a
    /// clock that went backwards.
    #[test]
    fn timer_due_fires_on_its_own_period_only() {
        let mut last: NsTs = 0;
        assert!(StrategySet::timer_due(&mut last, 1_000, 1_000));
        assert_eq!(last, 1_000);
        assert!(!StrategySet::timer_due(&mut last, 1_000, 1_999));
        assert_eq!(last, 1_000, "a miss leaves the clock");
        assert!(StrategySet::timer_due(&mut last, 1_000, 2_000));
        assert_eq!(last, 2_000);
        let mut never: NsTs = 0;
        assert!(!StrategySet::timer_due(&mut never, u64::MAX, u64::MAX));
        assert_eq!(never, 0);
        let mut ahead: NsTs = 5_000;
        assert!(!StrategySet::timer_due(&mut ahead, 1_000, 4_000));
        assert_eq!(ahead, 5_000);
    }

    /// Through the set, each member runs on its own period whatever the
    /// call cadence: a 1 s member called every 250 ms runs once a second,
    /// and a member with no timer never runs. At the pre-XH1 cadence
    /// (calls ≥ 1 s apart) the 1 s member runs on EVERY call — exactly
    /// what it did before per-slot gating.
    #[test]
    fn per_slot_timers_run_each_member_on_its_own_period() {
        const S: u64 = 1_000_000_000;
        let mut s = hyparb_set();
        let mut c = ctx();
        s.on_start(&mut c).unwrap();
        s.on_ai(&ai_cmd(AiCmdKind::EnableStrategy, SLOT_AI_EXEC), &mut c);
        assert_eq!(s.enabled_mask(), BIT_HYPARB | BIT_AI_EXEC);
        let mut runs = [0u64; 3];
        let mut n = 0usize;
        let mut t = S;
        while t <= 3 * S {
            let before = s.timer_last_ns[SLOT_HYPARB as usize];
            s.on_timer(t, &mut c);
            if s.timer_last_ns[SLOT_HYPARB as usize] != before {
                runs[n] = t;
                n += 1;
            }
            t += S / 4;
        }
        assert_eq!((n, runs), (3, [S, 2 * S, 3 * S]), "once a second, not four times");
        assert_eq!(
            s.timer_last_ns[SLOT_AI_EXEC as usize], 0,
            "a member with no timer is never run"
        );

        let mut s = hyparb_set();
        s.on_start(&mut c).unwrap();
        let mut k = 1u64;
        while k <= 4 {
            s.on_timer(k * (S + 1), &mut c);
            assert_eq!(
                s.timer_last_ns[SLOT_HYPARB as usize],
                k * (S + 1),
                "the pre-XH1 cadence runs the 1 s member on every call"
            );
            k += 1;
        }
    }

    /// HAR H3.3: the long-tenor set rides the set's tick and timer, arms
    /// the 1 s poll only once configured, and drains at stop.
    #[test]
    fn the_long_tenor_set_is_inert_until_configured_then_fed() {
        use core_vol::{LongSeries, LongSetCounters};
        const MIN: NsTs = 60_000_000_000;
        let feed = make_symbol_id(VenueId::Binance, 100);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        c.now = 0;
        s.on_start(&mut c).unwrap();
        assert_eq!(s.timer_period_ns(), u64::MAX, "no har.toml: no poll");
        s.on_tick(&tick(VenueId::Binance, feed, 100_000_000, 100_000_002), &mut c);
        s.on_timer(10 * MIN, &mut c);
        assert_eq!(s.har().counters(), LongSetCounters::default());
        let anchor = WallAnchor::new(0, 1_767_225_600_000_000_000);
        s.har_mut()
            .configure(&[LongSeries { name: b"BTC", feed }], anchor, 0)
            .unwrap();
        assert_eq!(s.timer_period_ns(), REGIME_TIMER_NS);
        // One quote in minute 0 (the helper stamps ts 0), the roll at 1 min.
        s.on_tick(&tick(VenueId::Binance, feed, 100_000_000, 100_000_002), &mut c);
        s.on_timer(MIN, &mut c);
        let e = s.har().engine(0).unwrap();
        assert_eq!(e.last_min_ts_ms(), 1_767_225_600_000);
        assert_eq!(e.prev_px_1e6(), 100_000_001);
        assert_eq!(s.har().counters().closes, 1);
        s.on_stop(&mut c);
        assert_eq!(s.har().held(0), None);
    }

    /// HAR H3.5: a series' `/state.har` row is built by the poll after the
    /// restore (and after each of its day closes), from the engine's own
    /// readers; the per-minute fields are read live at every view.
    #[test]
    fn the_har_row_is_built_on_the_poll_and_its_minute_fields_read_live() {
        use core_vol::LongSeries;
        const MIN: NsTs = 60_000_000_000;
        const DAY_MS: u64 = 86_400_000;
        let feed = make_symbol_id(VenueId::Binance, 100);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        c.now = 0;
        s.on_start(&mut c).unwrap();
        let mut out = [HarSeriesView::default(); HAR_VIEW_SERIES];
        assert_eq!(StrategyCounters::har_series_view(&s, &mut out), 0, "inert: no rows");
        assert_eq!(StrategyCounters::har_counters(&s), HarCounters::default());

        let wall0_ms: u64 = 1_767_225_600_000; // 2026-01-01T00:00Z
        let anchor = WallAnchor::new(0, wall0_ms * 1_000_000);
        s.har_mut()
            .configure(&[LongSeries { name: b"SP500", feed }], anchor, 0)
            .unwrap();
        // Restore 40 closed days ending 2025-12-31 — the third one EMPTY —
        // 60 one-day pairs, and the newest close's one-day arm.
        let first = wall0_ms - 40 * DAY_MS;
        let tau1 = core_vol::DAY_NS;
        {
            let e = s.har_mut().engine_mut(0).unwrap();
            for d in 0..40u64 {
                let (sq, n) = if d == 2 { (0, 0) } else { (4_000_000_000_000_000_000_000, 1_440) };
                assert!(e.seed_day(first + d * DAY_MS, sq, n));
            }
            for i in 0..60i64 {
                let x = 25_000_000_000 + i * 10_000_000;
                let target = first - 20 * DAY_MS + i as u64 * DAY_MS;
                assert!(e.seed_pair(tau1, target, x, x + (i % 7 - 3) * 10_000_000));
            }
            assert!(e.seed_arm(tau1, first + 39 * DAY_MS, 25_300_000_000, i64::MIN));
        }
        s.har_mut().restored();
        assert_eq!(StrategyCounters::har_series_view(&s, &mut out), 1);
        assert_eq!(out[0].days, 0, "the row waits for the poll");

        // The poll builds it: the set's first due poll, 1 s in (no minute
        // has rolled yet).
        s.on_timer(REGIME_TIMER_NS, &mut c);
        assert_eq!(StrategyCounters::har_series_view(&s, &mut out), 1);
        let v = out[0];
        assert_eq!(v.name(), b"SP500");
        assert_eq!(v.feed, feed);
        assert_eq!((v.warm, v.days, v.empty_days), (1, 40, 1));
        assert_eq!(v.newest_day_ms, wall0_ms - DAY_MS);
        assert!(v.raw_1e6[0] > 0, "the one-day arm is the raw forecast");
        assert_eq!(&v.raw_1e6[1..], &[0; HAR_VIEW_TENORS - 1], "no other tenor armed");
        assert_eq!(v.fitted, 1, "60 pairs fit the one-day tenor only");
        assert!(v.fit_1e6[0] > 0);
        assert_eq!(v.fit_1e6[1], 0);
        assert_eq!((v.pairs[0], v.pairs[1]), (60, 0));
        assert_eq!(v.fit_beats_raw, 0, "no QLIKE window yet");
        assert_eq!(v.weekday_1e6, [1_000_000; HAR_VIEW_WEEKDAYS], "every observed day alike");
        assert_eq!(v.weekday_n.iter().map(|&n| u32::from(n)).sum::<u32>(), 39);
        assert_eq!(v.epoch, 1, "the restore's epoch");
        assert_eq!((v.last_min_ms, v.open_minutes), (0, 0));
        assert_eq!(StrategyCounters::har_counters(&s).epoch, 1);

        // Two live minutes: the first opens 2026-01-01 over a restore that
        // ended on a closed day — a crossing by the set's rule (it could
        // have pushed empty days), so the epoch moves and the poll
        // rebuilds the row; the second only folds, and the minute fields
        // move without a rebuild.
        s.on_tick(&tick(VenueId::Binance, feed, 100_000_000, 100_000_002), &mut c);
        s.on_timer(MIN, &mut c);
        assert_eq!(StrategyCounters::har_series_view(&s, &mut out), 1);
        assert_eq!((out[0].epoch, out[0].open_minutes), (2, 0), "a crossing; the first primes");
        s.on_tick(&tick(VenueId::Binance, feed, 100_000_000, 100_000_004), &mut c);
        s.on_timer(2 * MIN, &mut c);
        assert_eq!(StrategyCounters::har_series_view(&s, &mut out), 1);
        assert_eq!(out[0].last_min_ms, wall0_ms + 60_000);
        assert_eq!(out[0].open_minutes, 1);
        assert_eq!(out[0].epoch, 2, "no further crossing");
        assert_eq!((out[0].days, out[0].raw_1e6), (40, v.raw_1e6), "no day closed");
        // A short buffer takes what fits and still reports the count.
        assert_eq!(StrategyCounters::har_series_view(&s, &mut []), 1);
    }

    /// HAR H3.7: with the writer's outbox installed the set hands a series'
    /// state at each of its day closes — the engine copied whole, with its
    /// epoch, while the cli writes nothing on the loop. A mailbox the writer
    /// still holds is refused, and the newest state follows at the first
    /// poll after it is handed back; a boot's epoch counts as handed.
    #[test]
    fn the_set_hands_each_day_close_to_the_state_writer() {
        use core_vol::{LongSeries, LongStateSnap};
        const MIN: NsTs = 60_000_000_000;
        const DAY_MIN: u64 = 1_440;
        let feed = make_symbol_id(VenueId::Binance, 100);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        c.now = 0;
        s.on_start(&mut c).unwrap();
        let anchor = WallAnchor::new(0, 1_767_225_600_000_000_000);
        s.har_mut()
            .configure(&[LongSeries { name: b"BTC", feed }], anchor, 0)
            .unwrap();
        let (tx, mut rx) = core_ring::Mailbox::new(Box::new(LongStateSnap::new())).split();
        s.install_har_outbox(vec![tx]);
        // One quote a minute, the poll at every minute's end, through
        // minute `until` (exclusive).
        let mut m = 0u64;
        let mut run = |s: &mut StrategySet, c: &mut CountCtx, until: u64| {
            while m < until {
                let px = 100_000_000 + ((m * 7_919) % 4_001) as i64;
                s.on_tick(&tick(VenueId::Binance, feed, px, px + 2), c);
                s.on_timer((m + 1) * MIN, c);
                m += 1;
            }
        };
        run(&mut s, &mut c, DAY_MIN);
        assert!(rx.try_take().is_none(), "no day closed: nothing handed");
        // The first minute of day 1 closes day 0: its state is handed.
        run(&mut s, &mut c, DAY_MIN + 1);
        assert_eq!(s.har().series_epoch(0), 1);
        let (mut want, mut got) = (String::new(), String::new());
        {
            let snap = rx.try_take().expect("handed at the close");
            assert_eq!(snap.epoch, 1);
            core_vol::render_state_file("BTC", &snap.engine, &mut got);
        }
        assert!(StrategyCounters::render_har_series(&s, 0, &mut want));
        assert_eq!(got, want, "the engine copied whole");
        // Day 1's close is handed, and the writer keeps it (a write that
        // failed, to be retried) while day 2 closes: that close is refused,
        // and nothing is lost.
        run(&mut s, &mut c, 2 * DAY_MIN + 1);
        assert_eq!(s.har().series_epoch(0), 2);
        rx.try_take().expect("epoch 2 handed").keep();
        run(&mut s, &mut c, 3 * DAY_MIN + 1);
        assert_eq!(s.har().series_epoch(0), 3);
        assert_eq!(rx.try_take().expect("still the held one").epoch, 2, "refused while held");
        // Handed back (the drop above): the next poll hands the newest.
        run(&mut s, &mut c, 3 * DAY_MIN + 2);
        assert_eq!(rx.try_take().expect("the newest follows").epoch, 3);
        run(&mut s, &mut c, 3 * DAY_MIN + 3);
        assert!(rx.try_take().is_none(), "nothing moved since");
    }

    /// HAR H3.7: the restore's epoch counts as handed — a boot that changed
    /// nothing hands (and so writes) nothing — and the first day close after
    /// it is handed with the next epoch.
    #[test]
    fn a_restored_state_is_not_handed_again_at_boot() {
        use core_vol::{LongSeries, LongStateSnap};
        const MIN: NsTs = 60_000_000_000;
        let feed = make_symbol_id(VenueId::Binance, 100);
        let mut s = StrategySet::new(BIT_AI_EXEC);
        let mut c = ctx();
        c.now = 0;
        s.on_start(&mut c).unwrap();
        let anchor = WallAnchor::new(0, 1_767_225_600_000_000_000);
        s.har_mut()
            .configure(&[LongSeries { name: b"BTC", feed }], anchor, 0)
            .unwrap();
        s.har_mut().restored();
        assert_eq!(s.har().series_epoch(0), 1, "the restore's epoch");
        let (tx, mut rx) = core_ring::Mailbox::new(Box::new(LongStateSnap::new())).split();
        s.install_har_outbox(vec![tx]);
        s.on_timer(REGIME_TIMER_NS, &mut c);
        assert!(rx.try_take().is_none(), "the restore is already on disk");
        let mut m = 0u64;
        while m <= 1_440 {
            s.on_tick(&tick(VenueId::Binance, feed, 100_000_000, 100_000_002), &mut c);
            s.on_timer((m + 1) * MIN, &mut c);
            m += 1;
        }
        assert_eq!(rx.try_take().expect("the first close").epoch, 2);
    }
}
