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
use core_types::regime::REL_UNKNOWN;
use core_types::{
    AiCmd, CancelReq, ChannelEvent, DepthTopK, Fill, OptSummary, Order, OrderEvent,
    RegimeLabelSet, RegimeWord, RuleTableV2, Signal, SymbolId, Tick, TradePrint, VenueId,
    REGIME_OFF_HARD, REGIME_OFF_SOFT, REGIME_PROFILES, SYMBOL_ID_NONE,
};

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

/// Reason a `ctx.submit`, `ctx.cancel` or `ctx.modify` call was
/// rejected.
///
/// **Four names for four different facts.** Until E5 there was one
/// variant and the engine mapped every dispatcher error onto it, so a
/// disabled slot and a full ring were the same word to a strategy.
/// That is survivable for a submit (the strategy drops the order
/// either way) and NOT survivable for a cancel: a strategy that reads
/// `RingFull` retries later, and a strategy that reads `NoSuchOrder`
/// knows the order is already gone. The two call for opposite
/// behaviour, so they get opposite names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubmitErr {
    /// The order ring is full — caller must drop the order rather
    /// than block. **Retryable**: the same call may succeed later.
    RingFull,
    /// The dispatcher in force does not implement this operation at
    /// all. The default `cancel`/`modify` bodies return this, so a
    /// dispatcher that has not been taught the verb refuses loudly
    /// instead of silently doing nothing. **Not retryable.**
    Unsupported,
    /// No order with that client id is resting. Either it already
    /// filled, already expired, or was already taken back — a RACE,
    /// not a failure. The order is gone, which is what the caller
    /// wanted; what it must not do is assume its own cancel is what
    /// removed it. **Not retryable.**
    NoSuchOrder,
    /// The dispatcher refused, and repeating the same call will not
    /// change that. Two kinds of cause land here and the strategy
    /// cannot tell them apart:
    ///
    /// * **operator conditions** — the slot is off, the slot is live
    ///   with no route to that venue, the venue answered non-2xx, the
    ///   signer said no;
    /// * **the caller built a bad request** — the replacement changed
    ///   the order's identity, its price or size was unmodellable, or
    ///   the client id named more than one of that slot's resting
    ///   orders.
    ///
    /// The second kind is a bug in the member, and the dispatcher's
    /// own counters (`identity_mismatch`, `unroutable`,
    /// `ambiguous_order`) are where it is visible; this variant is
    /// deliberately NOT the place to branch on it, because a strategy
    /// that could distinguish its own malformed request would be
    /// tempted to retry a variation of it. **Not retryable.**
    Refused,
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

    /// **E5 — take one resting order back.**
    ///
    /// `Ok(())` means *this call* removed the order. It does NOT mean
    /// "the order is not resting": that weaker fact is
    /// [`SubmitErr::NoSuchOrder`], and conflating the two is how a
    /// strategy comes to believe it cancelled a quote that a fill had
    /// already taken.
    ///
    /// Defaulted to [`SubmitErr::Unsupported`] so every existing `Ctx`
    /// — forty of them, mostly test doubles — keeps compiling, and so
    /// a ctx that has not been taught the verb says so rather than
    /// swallowing the request.
    #[inline]
    fn cancel(&mut self, _req: CancelReq) -> Result<(), SubmitErr> {
        Err(SubmitErr::Unsupported)
    }

    /// **E5, LAW E-7 — replace a resting order in place.**
    ///
    /// Two explicit arguments rather than a `ModifyReq`, because at
    /// the strategy's call site the previous id and the replacement
    /// are two separate thoughts and the `u64` next to an `Order` is
    /// unmistakable. The dispatcher boundary pairs them into
    /// [`core_types::ModifyReq`].
    ///
    /// `order.ttl_ns` is IGNORED — a modify inherits the original
    /// order's expiry, so no amount of repricing extends a quote's
    /// life past the TTL its ruleset gave it. `order.ts_ns` is NOT
    /// ignored: it is the decision clock the dispatcher re-arms the
    /// venue activation delta from, so stamp it from `ctx.now_ns()`
    /// exactly as on a submit. Everything else about the order's
    /// identity
    /// ([`core_types::OrderIdentity`]) must match the resting order;
    /// only price, size and client id may change.
    #[inline]
    fn modify(&mut self, _prev_client_oid: u64, _order: Order) -> Result<(), SubmitErr> {
        Err(SubmitErr::Unsupported)
    }

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
    /// vm member (RG3): entry/refire evaluations refused by a closed
    /// row regime gate (`engine_vm_regime_blocked_total`).
    #[inline]
    fn vm_regime_blocked(&self) -> u64 {
        0
    }
    /// vm member (RG3): positions flattened by a HARD-closed row gate
    /// (`engine_vm_regime_hard_exits_total`).
    #[inline]
    fn vm_regime_hard_exits(&self) -> u64 {
        0
    }
    /// icdp member (slot 6, ICDP I3/I4): the intrabar strategy's
    /// diagnostic counters (`engine_icdp_*_total`). Zeroes for every
    /// strategy but the set carrying a configured icdp member.
    #[inline]
    fn icdp_counters(&self) -> IcdpCounters {
        IcdpCounters::default()
    }

    /// VRP V6: the VRP member's observables (`engine_vrp_*`), mirrored
    /// by the cli's generic 5 s block exactly like [`Self::icdp_counters`].
    #[inline]
    fn vrp_counters(&self) -> VrpCounters {
        VrpCounters::default()
    }

    /// VRP V8a: the VRP member's persisted-state epoch, bumped whenever
    /// something that outlives the process changes. The cli watches it
    /// on the same 5 s cadence and rewrites `vrp-state.tsv` only when it
    /// moved — so a quiet engine writes nothing.
    #[inline]
    fn vrp_state_epoch(&self) -> u64 {
        0
    }

    /// X1: the cash the VRP member's last settlement booked ×1e6. No
    /// order is emitted for it (a European settlement is not a trade),
    /// so this gauge is the only place it shows.
    #[inline]
    fn vrp_last_settle_value_1e6(&self) -> i64 {
        0
    }

    /// R3 (P4.1): the regime's log-vol intercept in force at the VRP
    /// member's LAST decision, ×1e6. Zero when no offset is configured,
    /// when the detector has not spoken, or when the effective word is
    /// `vol:normal` — all three are the same number by construction, so
    /// the gauge alone does not say which; the `vrp: regime intercepts`
    /// boot tell says whether a table is loaded at all.
    #[inline]
    fn vrp_regime_offset_1e6(&self) -> i64 {
        0
    }

    /// P6: the VRP member's non-counter observables, all from ONE
    /// instant. `Default` (every field zero) for a strategy that is not
    /// the set, or a set with no VRP member.
    #[inline]
    fn vrp_snapshot_view(&self) -> VrpSnapshotView {
        VrpSnapshotView::default()
    }

    /// X1: fills that reached the strategy SET stamped for a slot that
    /// is not enabled, or not built. Non-zero means an order outlived a
    /// `DisableStrategy`, or a stamp is wrong.
    #[inline]
    fn fills_unrouted(&self) -> u64 {
        0
    }

    /// XMM XH2: order events that reached the strategy SET for a slot
    /// that is not enabled or not built, or attributed to no slot —
    /// counted, never fanned out (the X1 law).
    #[inline]
    fn order_events_unrouted(&self) -> u64 {
        0
    }

    /// VRP V8a: render that state. `false` = there is nothing to
    /// persist (no VRP member, or it is unconfigured), and the cli
    /// leaves the file alone.
    ///
    /// The member owns its own format: the cli owns only the file. Cold
    /// path — at most once per state change, off the tick loop.
    #[inline]
    fn render_vrp_state(&self, out: &mut String) -> bool {
        let _ = out;
        false
    }

    /// HAR H3.4: the long-tenor HAR series the strategy runs (`0`: none —
    /// no `har.toml`, and nothing is ever written).
    #[inline]
    fn har_series(&self) -> usize {
        0
    }

    /// HAR H3.4: series `i`'s state epoch — bumped at each of its UTC day
    /// closes and at the boot restore. The cli rewrites that series'
    /// `state-<NAME>.tsv` only when it moved (the
    /// [`Self::vrp_state_epoch`] law, one file per series so a day close
    /// writes one engine's rows, not twelve).
    #[inline]
    fn har_series_epoch(&self, i: usize) -> u64 {
        let _ = i;
        0
    }

    /// HAR H3.4: render series `i`'s state file into `out` (cleared
    /// first): a comment header, then the rows `core_vol::parse_rows`
    /// reads. `false` = no such series. Cold path.
    #[inline]
    fn render_har_series(&self, i: usize, out: &mut String) -> bool {
        let _ = (i, out);
        false
    }

    /// HAR H3.5: the long-tenor set's counters (`/state.har`, the
    /// `engine_har_*` gauges); all zero with no `har.toml`.
    #[inline]
    fn har_counters(&self) -> HarCounters {
        HarCounters::default()
    }

    /// HAR H3.5: copy the configured series' rows into `out` (`har.toml`
    /// order, `min(series, out.len())` rows); returns the series
    /// configured — `0` for a plain strategy. Never allocates.
    #[inline]
    fn har_series_view(&self, out: &mut [HarSeriesView]) -> u32 {
        let _ = out;
        0
    }

    /// XSD (slot 2, statarb doc 08): the cross-sectional member's
    /// observables (`engine_xsd_*`), mirrored like [`Self::icdp_counters`].
    #[inline]
    fn xsd_counters(&self) -> XsdCounters {
        XsdCounters::default()
    }

    /// XSD: the member's persisted-state epoch (the
    /// [`Self::vrp_state_epoch`] law — bumped on every position change;
    /// the cli rewrites `xsd-state.tsv` only when it moved).
    #[inline]
    fn xsd_state_epoch(&self) -> u64 {
        0
    }

    /// XSD: copy the entered positions into `out`, returning how many
    /// exist. The cli renders `xsd-state.tsv` from this view with the
    /// DESCRIPTORS it resolved at boot (a persisted `SymbolId` would name
    /// a different instrument after a universe reorder). Cold path.
    #[inline]
    fn xsd_positions_view(&self, out: &mut [XsdPositionView]) -> u32 {
        let _ = out;
        0
    }

    /// BIN15 O4b: the member's observables (`engine_bin15_*`), mirrored
    /// by the cli's generic 5 s block exactly like [`Self::xsd_counters`].
    #[inline]
    fn bin15_counters(&self) -> Bin15Counters {
        Bin15Counters::default()
    }

    /// BIN15 O4b: copy the per-family view into `out`, returning how
    /// many families are CONFIGURED (not how many are live — a dormant
    /// slot still has a row, because "no instance" is the observation).
    /// Cold path.
    #[inline]
    fn bin15_families_view(&self, out: &mut [Bin15FamilyView]) -> u32 {
        let _ = out;
        0
    }

    /// HYPARB H4: the slot-0 member's observables (`engine_hyparb_*`),
    /// mirrored by the cli's generic 5 s block like [`Self::bin15_counters`].
    #[inline]
    fn hyparb_counters(&self) -> HyparbCounters {
        HyparbCounters::default()
    }

    /// HYPARB H4: copy the per-pool view into `out` (a caller-owned
    /// slice, `min(out.len())` rows), returning how many pools are
    /// CONFIGURED. Cold path; never allocates.
    #[inline]
    fn hyparb_pools_view(&self, out: &mut [HyparbPoolView]) -> u32 {
        let _ = out;
        0
    }

    /// HYPARB H6: copy the per-coin view into `out` (`min(out.len())`
    /// rows), returning how many coins are CONFIGURED. Cold path.
    #[inline]
    fn hyparb_coins_view(&self, out: &mut [HyparbCoinView]) -> u32 {
        let _ = out;
        0
    }

    /// XMM XH3: the slot-6 member's counters (`engine_xmm_*_total`,
    /// `/state` `xmm`), written into `out` — the snapshot's own field on
    /// the 1 s publish, so nothing is returned by value. Zeroes for every
    /// strategy but the set carrying a configured xmm member.
    #[inline]
    fn xmm_counters(&self, out: &mut XmmCounters) {
        *out = XmmCounters::default();
    }

    /// XMM XH3: copy the per-perp view into `out` (`min(out.len())`
    /// rows), returning how many perps are CONFIGURED. Cold path (the
    /// 1 s publish); never allocates.
    #[inline]
    fn xmm_perps_view(&self, out: &mut [XmmPerpView]) -> u32 {
        let _ = out;
        0
    }

    /// HYPARB H8/H9: the member's AMM decision log, BORROWED — a ring of
    /// [`HYPARB_DECISION_LOG`] entries indexed by `seq %
    /// HYPARB_DECISION_LOG` (an entry whose `seq` is not the one its slot
    /// is read for was never written, or has been overwritten) — and the
    /// newest `seq` (0 = none yet). The reader (the EVM shadow's tap, on
    /// the engine thread) walks it in place and moves each new decision
    /// straight into its own ring: no staging copy (H9). What the write
    /// path shadows (O-H12). Never allocates.
    #[inline]
    fn hyparb_decision_log(&self) -> (&[HyparbDecision], u64) {
        (&[], 0)
    }

    /// RG2: the regime detector's observables (`engine_regime_*`),
    /// mirrored by the cli's generic 5 s block. The default (no
    /// detector) reports UNKNOWN words, open gates and zero counters —
    /// bare strategies never carry a detector.
    #[inline]
    fn regime_counters(&self) -> RegimeCounters {
        RegimeCounters::default()
    }

    // ---- RG6 `/state` snapshot family (plan §6.1) ----------------
    //
    // Read once per second by the cli's snapshot publish through the
    // same generic route as every row above. Defaults are the empty
    // value, so every strategy but the set is untouched.

    /// Sticky halt state (`StrategySet::is_halted`); `false` for
    /// every plain strategy (they have no halt lever).
    #[inline]
    fn is_halted(&self) -> bool {
        false
    }
    /// Per-slot counters of a composed set (slot map in
    /// `strategy-set`); zeros for a plain strategy or an unbuilt slot.
    #[inline]
    fn slot_counters(&self, _slot: u8) -> SlotCounters {
        SlotCounters::default()
    }
    /// vm member: identity of the active table (all-zero = none ever
    /// committed).
    #[inline]
    fn vm_active_hash128(&self) -> [u8; 16] {
        [0; 16]
    }
    /// vm member: identity of the staged (received, not yet committed)
    /// table; all-zero = nothing staged.
    #[inline]
    fn vm_staged_hash128(&self) -> [u8; 16] {
        [0; 16]
    }
    /// vm member: copy the active table's rows into `out`
    /// (`min(rows_active, out.len())` entries — row, position and gate
    /// per entry); returns the count written. 0 for a plain strategy.
    #[inline]
    fn vm_rows_view(&self, _out: &mut [VmRowView]) -> u32 {
        0
    }
    /// icdp member: SHA-256 of the configured artifact (all-zero =
    /// unconfigured).
    #[inline]
    fn icdp_params_hash(&self) -> [u8; 32] {
        [0; 32]
    }
    /// icdp member: instruments configured (0 = unconfigured).
    #[inline]
    fn icdp_instruments(&self) -> u32 {
        0
    }
    /// RG6: the detector's per-symbol RELATIVE state per profile
    /// (`RegimeView` minus the words, which `regime_counters` carries).
    /// Empty for a plain strategy.
    #[inline]
    fn regime_rel_view(&self) -> RegimeRelView {
        RegimeRelView::EMPTY
    }
}

/// RG6: one strategy-set slot's dashboard counters — POD, embedded in
/// the `/state` snapshot (`engine-snapshot`) eight times.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct SlotCounters {
    /// Orders the member emitted via `ctx.submit` (dispatcher accepted).
    pub orders_emitted: u64,
    /// Orders the dispatcher refused (ring full).
    pub orders_dropped: u64,
    /// Live terms of the member's regime label set (0 = `ANY`).
    pub label_terms: u8,
    /// The label's off-mode (`REGIME_OFF_SOFT` / `REGIME_OFF_HARD`).
    pub label_off: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 6],
}

impl SlotCounters {
    /// Construct without naming the padding.
    #[inline(always)]
    pub const fn new(
        orders_emitted: u64,
        orders_dropped: u64,
        label_terms: u8,
        label_off: u8,
    ) -> Self {
        Self {
            orders_emitted,
            orders_dropped,
            label_terms,
            label_off,
            _pad: [0; 6],
        }
    }
}

/// RG6: one vm row as the dashboard sees it — the `RuleRowV2` identity
/// fields + the row's position + its regime gate byte, flattened into
/// one 48 B POD (256 of them ride in the `/state` snapshot).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct VmRowView {
    /// FNV-1a 64 of the row's `name` (`RuleRowV2::name_h`).
    pub name_h: u64,
    /// Action-leg entry px ×1e6 (0 when flat).
    pub entry_px_1e6: i64,
    /// Engine-monotonic entry stamp (0 when flat).
    pub entry_ts_ns: u64,
    /// Entry qty on the action leg ×1e6 (0 when flat).
    pub qty_sym_1e6: i64,
    /// Action-leg symbol.
    pub sym: SymbolId,
    /// Reference-leg symbol (`SYMBOL_ID_NONE` for single-leg rows).
    pub ref_sym: SymbolId,
    /// Position state byte (0 flat, 1 entered — the vm's law).
    pub state: u8,
    /// Entered side (`Side` byte; meaningful when `state == 1`).
    pub side: u8,
    /// RG3 row gate byte: bit 0 open, bit 1 hard-closed.
    pub gate: u8,
    /// `RuleRowV2::flags`.
    pub flags: u8,
    /// `RuleRowV2::family` (`MarketFamily` byte).
    pub family: u8,
    /// `RuleRowV2::regime_off`.
    pub regime_off: u8,
    /// Sign of the entry signal (+1 / −1; 0 when flat).
    pub entry_sign: i8,
    /// Explicit padding — always zero.
    _pad: u8,
}

impl VmRowView {
    /// Construct without naming the padding.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        name_h: u64,
        entry_px_1e6: i64,
        entry_ts_ns: u64,
        qty_sym_1e6: i64,
        sym: SymbolId,
        ref_sym: SymbolId,
        state: u8,
        side: u8,
        gate: u8,
        flags: u8,
        family: u8,
        regime_off: u8,
        entry_sign: i8,
    ) -> Self {
        Self {
            name_h,
            entry_px_1e6,
            entry_ts_ns,
            qty_sym_1e6,
            sym,
            ref_sym,
            state,
            side,
            gate,
            flags,
            family,
            regime_off,
            entry_sign,
            _pad: 0,
        }
    }
}

/// Slots of [`RegimeRelView`] — equals `core_regime::REGIME_MAX_SYMS`
/// (pinned by a const assert in `strategy-set`, which copies the
/// detector's view straight in).
pub const REGIME_REL_SYMS: usize = 32;

/// RG6: the detector's per-symbol RELATIVE state (`RegimeView` minus
/// the words) — slot 0 is the BTC ref, `syms[..n]` live. POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RegimeRelView {
    /// Slot → symbol (`SYMBOL_ID_NONE` beyond `n`).
    pub syms: [SymbolId; REGIME_REL_SYMS],
    /// REL byte per profile per slot (`REL_UNKNOWN` for the ref, empty
    /// slots and warm-up).
    pub rel: [[u8; REGIME_REL_SYMS]; REGIME_PROFILES],
    /// Live slots.
    pub n: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 7],
}

impl RegimeRelView {
    /// No detector: no members, every REL unknown.
    pub const EMPTY: Self = Self {
        syms: [SYMBOL_ID_NONE; REGIME_REL_SYMS],
        rel: [[REL_UNKNOWN; REGIME_REL_SYMS]; REGIME_PROFILES],
        n: 0,
        _pad: [0; 7],
    };

    /// Construct without naming the padding.
    #[inline(always)]
    pub const fn new(
        syms: [SymbolId; REGIME_REL_SYMS],
        rel: [[u8; REGIME_REL_SYMS]; REGIME_PROFILES],
        n: u8,
    ) -> Self {
        Self {
            syms,
            rel,
            n,
            _pad: [0; 7],
        }
    }
}

/// The regime detector's observables (RG2, plan §4.9) — defined here
/// so the cli mirrors them through [`StrategyCounters`] without naming
/// `core-regime` or the set (the icdp-counters precedent). POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RegimeCounters {
    /// Measured word per profile (index = profile).
    pub measured: [RegimeWord; 4],
    /// Declared word per profile (EMPTY when none).
    pub declared: [RegimeWord; 4],
    /// Effective word per profile.
    pub effective: [RegimeWord; 4],
    /// Engine-monotonic stamp of the declaration per profile (0 = none);
    /// the cli derives the age with its own clock at mirror time.
    pub declared_ts_ns: [u64; 4],
    /// TTL of the declaration per profile (0 = none).
    pub declared_ttl_ns: [u64; 4],
    /// Flip counters per profile × dimension (0..6 live).
    pub flips: [[u64; 8]; 4],
    /// Disagree counter per profile.
    pub disagree: [u64; 4],
    /// Raw judgement inputs per profile: ret bps ×1e9, ER ×1e9, RV bps
    /// ×1e9, stretch ×1e9 (present bits in `raw_present`).
    pub raw: [[i64; 4]; 4],
    /// Presence bits per profile (`core_regime::RAW_*`).
    pub raw_present: [u8; 4],
    /// Per-slot gate: 0 open, 1 soft-closed, 2 hard-closed.
    pub gates: [u8; 8],
    /// Minutes judged since boot.
    pub minutes_judged: u64,
    /// Seed rows applied at boot.
    pub seed_rows: u64,
    /// `SetRegime` commands applied.
    pub declared_total: u64,
    /// Gate changes fanned out (`on_regime` calls).
    pub gate_changes: u64,
    /// 1 when a detector is configured (else every word is UNKNOWN).
    pub configured: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 7],
}

impl Default for RegimeCounters {
    fn default() -> Self {
        Self {
            measured: [RegimeWord::UNKNOWN; 4],
            declared: [RegimeWord::EMPTY; 4],
            effective: [RegimeWord::UNKNOWN; 4],
            declared_ts_ns: [0; 4],
            declared_ttl_ns: [0; 4],
            flips: [[0; 8]; 4],
            disagree: [0; 4],
            raw: [[0; 4]; 4],
            raw_present: [0; 4],
            gates: [0; 8],
            minutes_judged: 0,
            seed_rows: 0,
            declared_total: 0,
            gate_changes: 0,
            configured: 0,
            _pad: [0; 7],
        }
    }
}

/// Diagnostic counters of the intrabar (ICDP) strategy — defined here
/// so the engine/cli mirror them through [`StrategyCounters`] without
/// naming the strategy crate (the vm-gauge precedent). POD.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct IcdpCounters {
    /// Decisions evaluated (a fresh tick at/after `open + δ`).
    pub decisions: u64,
    /// Composites past the threshold.
    pub signals: u64,
    /// Entry IoCs emitted.
    pub intents: u64,
    /// Exit IoCs emitted.
    pub exits: u64,
    /// Exits emitted while the last quote was stale (fill deferred).
    pub exit_on_stale: u64,
    /// Bars skipped: spread over the cap at decision.
    pub skipped_spread: u64,
    /// Bars skipped: the open quote was stale (or absent / too old).
    pub skipped_stale_open: u64,
    /// Bars skipped: a stale tick inside the bar before the decision.
    pub skipped_stale_dec: u64,
    /// Bars skipped: the previous bar was not valid (gap / stale).
    pub skipped_prev: u64,
    /// Bars skipped: the first fresh tick after δ came in the last
    /// fifth of the bar.
    pub late_bars: u64,
    /// Entries refused by the table cap.
    pub caps_rejected: u64,
    /// Bar rolls processed.
    pub rolls: u64,
    /// RG2: decisions refused because the member's regime gate was
    /// closed (`engine_icdp_regime_blocked_total`).
    pub regime_blocked: u64,
    /// RG2: positions exited early by a hard-closed gate
    /// (`engine_icdp_regime_exits_total`).
    pub regime_exits: u64,
}

/// VRP V6 counters (`engine_vrp_*`), mirrored by the cli's generic 5 s
/// block. Defined HERE rather than in `strategy-vrp` for the same reason
/// [`IcdpCounters`] is: the cli must never name a member crate.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VrpCounters {
    /// Entry instants evaluated (the two-compare decision ran).
    pub decisions: u64,
    /// Option entries submitted.
    pub entries: u64,
    /// Hedge orders submitted (entry hedge + rebalances + unwind).
    pub hedges: u64,
    /// Exits submitted by the RISK unwind only — a hard-closed regime
    /// gate. F32: this is NOT the `E−ε` exit; Y1 retired that rung and
    /// a campaign now ends at settlement. A non-zero value here means a
    /// gate slammed shut mid-hold, not that a campaign closed normally.
    pub exits: u64,
    /// Decisions that HELD: the quoted implied vol was inside the band.
    pub holds: u64,
    /// Decisions refused because the forecast had no bounds (a cold
    /// ring or fewer than 60 fitted pairs — ABSENT DATA HOLDS).
    pub no_bounds: u64,
    /// F30: DECISIONS, rebalances and settlements skipped because the
    /// member's own cached mark was stale or absent — an action it
    /// wanted to take and could not. Non-zero at a decision instant is
    /// a campaign lost.
    ///
    /// Distinct from [`Self::records_ignored`], which counts option
    /// RECORDS that carried nothing usable. Reading the two as one
    /// number hid the difference between "the venue sent noise" and
    /// "we could not decide", and the venue sends noise all day.
    pub stale_skips: u64,
    /// F30: option records DROPPED on arrival — no mark flag, a
    /// non-positive mark, IV or underlying, or a coin price that does
    /// not convert. Routine: Deribit publishes summaries for
    /// instruments with no book. It costs nothing and decides nothing.
    pub records_ignored: u64,
    /// An expiry was inside the selection window and NOTHING in the
    /// chain was tradeable for this member — it rolled without our
    /// currency, or carries no calls at that expiry.
    ///
    /// Not "no expiry is due", which is the normal state for ~23 h 50 m
    /// of every day and is not counted at all. A rising value here is a
    /// chain problem, not a quiet market.
    pub no_selection: u64,
    /// W6: campaigns whose decision instant was reached too LATE to
    /// act on — the engine was down across `E−τ` and came back with
    /// less of the hold left than the forecast was made over.
    ///
    /// `τ` is not a start line, it is the horizon: the bounds are a
    /// variance forecast for a τ-long hold, and the edge was measured
    /// on one. Entering an hour before expiry on an eight-hour forecast
    /// is a different trade wearing the same gate, so the campaign is
    /// spent unused and counted here instead. Non-zero means a restart
    /// straddled a decision instant.
    pub decisions_late: u64,
    /// Decisions refused because the member's regime gate was closed.
    pub regime_blocked: u64,
    /// Campaigns flattened early by a hard-closed gate.
    pub regime_exits: u64,
    /// Settled expiries folded back into the forecast.
    pub settlements: u64,
    /// Entries refused by a `docs/risk-policy.md` notional cap.
    pub caps_rejected: u64,
    /// VX: campaigns that reached expiry still holding and cash-settled
    /// IN the money (a closing order at intrinsic value).
    pub settled_itm: u64,
    /// VX: campaigns that reached expiry still holding and expired OUT
    /// of the money — the option is worth nothing, so there is no order
    /// and, per the venue's schedule, no fee.
    pub settled_otm: u64,
    /// V8a: an IN-the-money expiry the member could not PRICE, because
    /// the campaign was restored across a restart and its contract has
    /// already rolled off the boot chain. The position is closed out of
    /// the member's book and the value is NOT recorded — an operator has
    /// to reconcile that one expiry by hand. Any non-zero value here is
    /// a reconciliation item, not a routine counter.
    pub settled_unpriced: u64,
    /// `1` once kill criterion 3 has HALTED the member: a full
    /// trailing-60 window in which the forecast no longer beat implied
    /// vol. Sticky — it takes a restart to clear, exactly like the
    /// engine's own halt.
    pub killed: u64,
    /// Kill criterion 3 (edge spec §5.3): trailing-60 mean QLIKE of the
    /// venue's implied vol, ×1e6. Lower is better.
    pub qlike_iv_1e6: i64,
    /// Trailing-60 mean QLIKE of this member's own forecast, ×1e6.
    pub qlike_har_1e6: i64,
    /// `1` once a FULL trailing-60 window shows the forecast still
    /// beating implied vol; `0` while the window fills OR once the
    /// mechanism has stopped holding. **A `0` on a full window is the
    /// halt tell** — E1 is gone and the member has no edge to harvest.
    pub qlike_har_beats_iv: u64,
    /// Q4: decisions where the band opened an arm and `sides` policy
    /// refused it. Distinct from [`Self::holds`], which is the band
    /// itself saying hold — one is a strategy fact, the other a config
    /// one, and reading them as one number hides which.
    pub holds_side: u64,
    /// F31: chain scans run by the selection law. Bounded by ONE per
    /// campaign plus the records inside a 10-minute selection window;
    /// it used to run on every option record for ~23 h 50 m a day.
    pub select_scans: u64,
    /// X1: option entries SUBMITTED. `entries` counts the ones that
    /// FILLED, so `entries_submitted − entries` is the F7 gap — the
    /// number the member used to report as `entries` outright.
    pub entries_submitted: u64,
    /// X1: entries that met no fill by their deadline. The campaign is
    /// a HOLD; the forecast is disarmed; there is no retry in v1.
    pub entries_unfilled: u64,
    /// X1: hedge orders that met no fill by their deadline and were
    /// retried at the then-current touch.
    pub hedge_unfilled: u64,
    /// X1: a hedge target GIVEN UP ON after `HEDGE_RETRIES_MAX`. The
    /// book is not at its delta target and no order is chasing it.
    /// **Any non-zero value is an operator alert** — see
    /// `docs/risk-policy.md`.
    pub hedge_abandoned: u64,
    /// X1: modelled fills this member consumed (both legs).
    pub fills: u64,
    /// X1: fills that reached this member and matched NO leg in
    /// flight — a late partial after a deadline, or a mis-stamped
    /// fill. Counted, never booked.
    pub fills_ignored: u64,
    /// R1: entries submitted as a RESTING maker order rather than an
    /// IoC. `entries_submitted − entry_maker_submitted` is the number
    /// that crossed at the mark.
    pub entry_maker_submitted: u64,
    /// R1: unfilled maker entries the fallback CROSSED at the touch.
    pub entry_crossed: u64,
    /// R1: unfilled maker entries the fallback REFUSED to cross,
    /// because the signal no longer cleared the cost gate at the touch
    /// price. Counted apart from `entries_unfilled`: "nobody came to my
    /// price" and "crossing would not have paid" are different facts.
    pub entry_cost_refused: u64,
    /// R2: hedges that crossed after a maker rest expired. A hedge must
    /// complete, so this is the unconditional fallback firing — not an
    /// error, but the number that says how much of the maker saving is
    /// real.
    pub hedge_crossed: u64,
    /// R5: settlements priced off the LAST print because the 30-minute
    /// delivery TWAP had less than 10 minutes of samples behind it — a
    /// quiet option lane, or a restart inside the window. Not an error;
    /// the number that says how much of the settled P&L is measured
    /// against the venue's own delivery law and how much is a proxy.
    pub settle_index_fallback: u64,
    /// R6: decisions taken on the LAST quoted implied vol because the
    /// selection window held fewer than
    /// `strategy_vrp::IV_MEDIAN_MIN_SAMPLES` samples of it. The median
    /// exists so one wide print at the decision instant cannot flip a
    /// campaign; below the floor there is no median to take.
    pub iv_median_fallback: u64,
    /// R7: HOLDs that θ ALONE would have traded — the band opened, and
    /// the round-trip cost of the option leg closed it again. The
    /// number that says how much of the strategy the fee load eats;
    /// counted on top of [`Self::holds`], never instead of it.
    pub holds_cost: u64,
}

/// P6: everything `/state` shows about the VRP member (slot 1) that is
/// NOT a counter — the artifact it booted with, the campaign in force,
/// the orders in flight, and the two gauges.
///
/// One POD rather than a dozen trait accessors: the snapshot is filled
/// once per publish and every field has to come from the same instant,
/// or an operator reads a strike from one campaign against a position
/// from the next.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VrpSnapshotView {
    /// SHA-256 of the `vrp.toml` bytes (all-zero = unconfigured).
    pub hash: [u8; 32],
    /// Campaign expiry, WALL ns (0 = no campaign).
    pub expiry_ns: u64,
    /// V8a state epoch — moves whenever the persisted state changes.
    pub state_epoch: u64,
    /// `client_oid` of the option leg in flight (0 = none).
    pub opt_oid: u64,
    /// `client_oid` of the hedge leg in flight (0 = none).
    pub hedge_oid: u64,
    /// Selected contract's strike ×1e6.
    pub strike_1e6: i64,
    /// SIGNED option position ×1e6.
    pub opt_qty_1e6: i64,
    /// SIGNED perp hedge position ×1e6.
    pub perp_qty_1e6: i64,
    /// R3: the regime intercept in force at the last decision ×1e9.
    pub regime_offset_1e9: i64,
    /// X1: cash the last settlement booked ×1e6.
    pub last_settle_value_1e6: i64,
    /// Selected option sym (`SYMBOL_ID_NONE` = nothing selected).
    pub selected_sym: SymbolId,
    /// `opt_registry::RIGHT_CALL` / `RIGHT_PUT`.
    pub right: u8,
    /// `SIDE_SHORT_VOL` / `SIDE_LONG_VOL` / `SIDE_FLAT`.
    pub side: i8,
    /// 1 once this campaign's ONE decision has been taken.
    pub entry_done: u8,
    /// 1 once `configure` succeeded.
    pub configured: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 8],
}

/// XSD counters (`engine_xsd_*`), mirrored by the cli's generic 5 s
/// block. Defined HERE for the reason [`IcdpCounters`] is.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XsdCounters {
    /// Hour boundaries rolled.
    pub rolls: u64,
    /// Pairs whose z was finite at the last roll (a level, not a total).
    pub pairs_warm: u64,
    /// Target decisions evaluated (one per target per roll).
    pub decisions: u64,
    /// ENTER decisions.
    pub entries_decided: u64,
    /// ADD decisions.
    pub adds_decided: u64,
    /// Entry IoCs emitted (the logical position opened).
    pub entries: u64,
    /// Grid-unit IoCs emitted.
    pub adds: u64,
    /// Exits emitted: `z̄ · d ≤ z_out`.
    pub exits_revert: u64,
    /// Exits emitted: `|z|̄ ≥ z_stop`.
    pub exits_stop: u64,
    /// Exits emitted: held `≥ max_hold_h`.
    pub exits_maxhold: u64,
    /// Exits emitted: restored under a changed table hash.
    pub exits_rotation: u64,
    /// Exits emitted: the regime gate hard-closed.
    pub exits_regime: u64,
    /// Pending intents that crossed a roll unpriced (no fresh tick in
    /// the hour) and were carried into the next one.
    pub intents_carried: u64,
    /// Unfilled entries superseded by their own exit signal.
    pub entries_cancelled: u64,
    /// Intents refused by a notional / position cap.
    pub caps_rejected: u64,
    /// Decisions that HELD because no partner z was finite.
    pub holds_absent: u64,
    /// Entries refused by a closed regime gate.
    pub regime_blocked: u64,
    /// Seed rows that landed in a bucket.
    pub seed_rows: u64,
    /// Seed rows dropped (unknown sym, duplicate hour, older than the ring).
    pub seed_dropped: u64,
}

/// HYPARB H4 counters (`engine_hyparb_*`), mirrored by the cli's generic
/// 5 s block. Defined HERE for the reason [`IcdpCounters`] is. Every
/// `skipped_*` is a REASON (the bin15 law): which gate held a pool is the
/// observation, and "no hedge book" and "the day cap is full" call for
/// opposite actions.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HyparbCounters {
    /// Pool-event signals applied.
    pub pool_events: u64,
    /// Pool-event signals refused (undecodable, not a configured pool).
    pub pool_refused: u64,
    /// Tick maps loaded from a completed snapshot.
    pub maps_loaded: u64,
    /// Tick maps refused (a snapshot that does not load, a position the
    /// map refuses) — the pool is stale until its next snapshot.
    pub maps_refused: u64,
    /// Pool evaluations run (the solve reached).
    pub evaluations: u64,
    /// Arb decisions: an AMM swap submitted.
    pub arbs_submitted: u64,
    /// Solves whose net edge was under `min_net_bps` (or not positive).
    pub skipped_below_min: u64,
    /// Held: the pool is not judgeable (never snapshotted, stale, edge).
    pub skipped_not_live: u64,
    /// Held: a hedge book is absent, stale or one-sided.
    pub skipped_no_hedge: u64,
    /// Held: the pool's previous swap is still in flight.
    pub skipped_inflight: u64,
    /// Held: the pool's cooldown.
    pub skipped_cooldown: u64,
    /// Held: the member is halted (inventory cap) or the day cap is full.
    pub skipped_halted: u64,
    /// Decisions whose size a cap cut (depth, order, pool, day).
    pub size_capped: u64,
    /// AMM-leg fills.
    pub amm_fills: u64,
    /// Hedge IoCs submitted after an AMM fill.
    pub hedges_submitted: u64,
    /// Hedge IoCs routed to the perp book.
    pub hedges_perp: u64,
    /// Hedge IoCs routed to the spot book.
    pub hedges_spot: u64,
    /// Hedge fills (any quantity).
    pub hedge_fills: u64,
    /// Hedge IoCs that reached their deadline unfilled — the latency
    /// correction made visible (the book moved inside Δ).
    pub hedges_missed: u64,
    /// Inventory-flattening IoCs (the timer's TWAP of unhedged residue).
    pub flattens_submitted: u64,
    /// Times the unhedged-inventory cap was breached (the member halts).
    pub inventory_breaches: u64,
    /// Orders the context refused (ring full, unsupported, refused).
    pub orders_dropped: u64,
    /// Gas charged per ATTEMPT (every AMM swap submitted), USD × 1e6.
    pub gas_charged_usd_1e6: i64,
    /// Sum of the solver's predicted net P&L over submitted arbs, USD ×
    /// 1e6 (after pool fee, hedge fees and gas) — the model's claim, for
    /// the harness to hold it to.
    pub pnl_predicted_usd_1e6: i64,
    /// Sum of AMM-leg fill notional, USD × 1e6.
    pub amm_notional_usd_1e6: i64,
    /// HYPARB H6: arbs that BOUGHT token0 from the pool. With
    /// [`Self::arbs_sell`] the side balance — a persistent skew in live
    /// paper means basis control is off or wrong (plan §10 #2).
    pub arbs_buy: u64,
    /// HYPARB H6: arbs that SOLD token0 into the pool.
    pub arbs_sell: u64,
    /// HYPARB H6 LEVEL (not cumulative-monotonic): funding the perp
    /// hedges have earned so far, USD × 1e6, signed (a short earns a
    /// positive rate) — plan §10 #6.
    pub funding_earned_usd_1e6: i64,
    /// HYPARB H6 LEVEL: 1 while the inventory cap halts new arbs.
    pub halted: u64,
    /// HYPARB go-live LEVEL (2026-09-24): the member's own marked P&L
    /// this session, USD × 1e6, signed — cash from every fill less the
    /// hedge taker fees and the gas charged per attempt, plus funding,
    /// plus the coins held valued at their mids. Held at its last value
    /// while some held coin has no mark. A level, never a stop: the P&L
    /// stop is LIVE-only (ruling O-HL5) — paper's evidence for gate G1.
    pub pnl_session_usd_1e6: i64,
}

/// HYPARB H4: one pool's row (`/state`, the dashboard).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HyparbPoolView {
    /// The pool's symbol.
    pub sym: u32,
    /// 1 when judgeable (snapshotted, not stale, not edge-bound).
    pub live: u8,
    /// 1 when the tick map is loaded and consistent.
    pub map_ok: u8,
    /// Last hedge venue chosen for token0 (0 perp, 1 spot, 255 none).
    pub hedge_venue: u8,
    _pad: u8,
    /// Fee the member prices with, pips.
    pub fee_pips: u32,
    _pad2: u32,
    /// Pool mid, token1 per token0 × 1e6 (0 = unknown).
    pub mid_1e6: i64,
    /// Basis EMA (pool vs hedge), bps × 1e6.
    pub basis_bps_1e6: i64,
    /// Arbs submitted on this pool.
    pub arbs: u64,
    /// HYPARB H6: the solver's predicted net P&L summed over this pool's
    /// arbs, USD × 1e6 — where the edge concentrates (plan §10 #4).
    pub pnl_predicted_usd_1e6: i64,
}

impl HyparbPoolView {
    /// A row (the padding stays private and zero).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sym: u32,
        live: u8,
        map_ok: u8,
        hedge_venue: u8,
        fee_pips: u32,
        mid_1e6: i64,
        basis_bps_1e6: i64,
        arbs: u64,
        pnl_predicted_usd_1e6: i64,
    ) -> Self {
        Self {
            sym,
            live,
            map_ok,
            hedge_venue,
            _pad: 0,
            fee_pips,
            _pad2: 0,
            mid_1e6,
            basis_bps_1e6,
            arbs,
            pnl_predicted_usd_1e6,
        }
    }
}

/// HYPARB H6: one hedge coin's row — the books the selector reads and
/// what the member holds (plan §10 #1 and #6).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HyparbCoinView {
    /// The perp's symbol (0 = none).
    pub perp_sym: u32,
    /// The spot pair's symbol (0 = none).
    pub spot_sym: u32,
    /// The perp's displayed touch, the thinner side, USD × 1e6.
    pub perp_depth_usd_1e6: i64,
    /// The spot pair's displayed touch, the thinner side, USD × 1e6.
    pub spot_depth_usd_1e6: i64,
    /// The selector's last total cost on the perp, bps × 1e6 (signed).
    pub perp_cost_bps_1e6: i64,
    /// The selector's last total cost on spot, bps × 1e6.
    pub spot_cost_bps_1e6: i64,
    /// Unhedged inventory, coin × 1e6, signed.
    pub inventory_1e6: i64,
    /// Net perp position the hedges built, coin × 1e6, signed.
    pub perp_pos_1e6: i64,
    /// The perp's hourly funding rate × 1e9.
    pub funding_1e9: i64,
}

/// Decisions the HYPARB member remembers for
/// [`StrategyCounters::hyparb_decision_log`].
pub const HYPARB_DECISION_LOG: usize = 64;

/// HYPARB H8: one AMM decision exactly as the member made it — the input
/// the EVM write path shadows (O-H12: one testnet swap per paper
/// decision, its G2 bid a fraction of THIS edge). Never persisted.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HyparbDecision {
    /// Per-member sequence, from 1.
    pub seq: u64,
    /// Decision time, ns.
    pub ts_ns: u64,
    /// The solver's net P&L (after the pool fee, both hedge fees and
    /// p50 gas), USD × 1e6 — the edge a gas bid is a fraction of.
    pub edge_usd_1e6: i64,
    /// The AMM leg's token0 notional, USD × 1e6.
    pub notional_usd_1e6: i64,
    /// The gas coin's USD mid at the decision (0 = no gas coin priced).
    pub gas_px_usd_1e6: i64,
    /// The pool's symbol.
    pub pool_sym: u32,
    /// 1 = bought token0 from the pool, 0 = sold it.
    pub buy: u8,
    /// Padding.
    pub _pad: [u8; 3],
}
const _: () = assert!(core::mem::size_of::<HyparbDecision>() == 48);

/// XMM (slot 6) counters — what the member did, cumulatively
/// (`engine_xmm_*_total`, `/state` `xmm`, the harness lines). Defined
/// HERE for the reason [`IcdpCounters`] is: the cli never names a
/// member crate. POD.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XmmCounters {
    /// Post-only orders placed (fresh placements, not modifies).
    pub placed: u64,
    /// Requotes sent as a modify (E-7).
    pub modifies: u64,
    /// Cancels sent by the LEAD rule.
    pub lead_cancels: u64,
    /// Cancels sent to requote (the parity switch, a requote the gate
    /// or the caps would not re-place, a modify the arm refused, or a
    /// refused replacement's predecessor).
    pub requote_cancels: u64,
    /// Cancels sent by a safety pull (a stale leader or follower).
    pub pull_cancels: u64,
    /// Cancels the member sent itself at its order's TTL (LAW E-8's
    /// member half; the venue's or the paper model's expiry normally
    /// lands first).
    pub expiry_cancels: u64,
    /// Placements the gate held back.
    pub gated: u64,
    /// Placements held back because the leader's history overflowed the
    /// gate window (fail-closed; must stay 0 at the ring's size).
    pub gate_overflow: u64,
    /// Placements a cap held back.
    pub capped: u64,
    /// Orders the venue rejected for crossing (`BAD_ALO_PX`) —
    /// information, not a fault (XH-7).
    pub rejected_alo: u64,
    /// Orders rejected for any other reason.
    pub rejected_other: u64,
    /// Orders that ended cancelled (requested, replaced or expired).
    pub canceled: u64,
    /// Orders that ended filled.
    pub filled: u64,
    /// Fills received.
    pub fills: u64,
    /// Fills or events naming no order of ours (a race, or a bug when
    /// large).
    pub unmatched: u64,
    /// Submits, cancels or modifies the ctx refused.
    pub ctx_refused: u64,
    /// Sides released after waiting 10 s on a final event (a
    /// best-effort cancel goes out for what they held). Must stay 0.
    pub stuck: u64,
}
const _: () = assert!(core::mem::size_of::<XmmCounters>() == 17 * 8);

/// XMM XH3: one quoted perp's row, read at one instant — the follower's
/// touch, our two quotes on it, what the member holds and how old each
/// feed is (the `/state` `xmm.perps` array).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XmmPerpView {
    /// Signed position from the member's own fills, base × 1e6.
    pub pos_1e6: i64,
    /// The follower's (Hyperliquid) bid, × 1e6 (0 = no fresh book yet).
    pub touch_bid_1e6: i64,
    /// The follower's ask, × 1e6 (0 = no fresh book yet).
    pub touch_ask_1e6: i64,
    /// Our bid's price, × 1e6 (0 = no bid).
    pub bid_px_1e6: i64,
    /// Our ask's price, × 1e6 (0 = no ask).
    pub ask_px_1e6: i64,
    /// Engine-monotonic ns of the leader's last fresh update (0 = none).
    pub lead_rx_ns: u64,
    /// Engine-monotonic ns of the follower's last update (0 = none).
    pub fol_rx_ns: u64,
    /// The Hyperliquid perp.
    pub hl_sym: u32,
    /// Its Binance USDⓈ-M leader.
    pub lead_sym: u32,
    /// Our bid: 0 none, 1 sent, 2 resting, 3 cancelling.
    pub bid_state: u8,
    /// Our ask, as `bid_state`.
    pub ask_state: u8,
    /// Bit 0: the follower's last update was flagged stale; bit 1: the
    /// leader's.
    pub stale_flags: u8,
    /// Padding.
    pub _pad: [u8; 5],
}
const _: () = assert!(core::mem::size_of::<XmmPerpView>() == 72);

/// BIN15 counters (`engine_bin15_*`), mirrored by the cli's generic 5 s
/// block. Defined HERE for the reason [`IcdpCounters`] is.
///
/// Every `skipped_*` is a REASON, not a total: the sum of them is the
/// number of re-price attempts that produced no intent, and an operator
/// looking at a silent member needs to know WHICH gate held. A single
/// `skipped_total` would make "the book is one-sided" and "the caps are
/// full" the same observable, and they call for opposite actions.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Bin15Counters {
    /// Re-prices that produced a `p_hat` (the pricer ran to the end).
    pub reprices: u64,
    /// `InstrumentRoll` created events bound to a family.
    pub rolls: u64,
    /// `InstrumentRoll` settled events that closed a live instance.
    pub rolls_settled: u64,
    /// `SetBinarySpec` frames accepted.
    pub spec_overrides: u64,
    /// `SetBinarySpec` frames refused on shape.
    pub spec_refused: u64,
    /// Arm A IoCs emitted.
    pub takes_submitted: u64,
    /// Arm A IoCs that filled (any quantity).
    pub takes_filled: u64,
    /// Arm A IoCs that reached their deadline unfilled.
    pub takes_unfilled: u64,
    /// Arm B resting quotes emitted.
    pub quotes_submitted: u64,
    /// Arm B quotes that filled (any quantity), counted when the
    /// fill arrives under the id the member is currently tracking.
    ///
    /// E5: a fill that raced a reprice and came back under the id the
    /// order carried BEFORE it moved is [`Self::quotes_raced`]
    /// instead — it belongs to an order the member has already
    /// replaced, and counting it here would say a resting quote
    /// filled when a retired one did.
    pub quotes_filled: u64,
    /// Arm B quotes that reached their TTL unfilled.
    pub quotes_expired: u64,
    /// Closing takes emitted (the only sell the member ever sends).
    pub closes_submitted: u64,
    /// Re-prices held: `τ` under the arm's minimum.
    pub skipped_tau: u64,
    /// Re-prices held: inside `tail_refuse_ns`.
    pub skipped_tail: u64,
    /// Re-prices held: no underlying mark, a cold forecast or a price
    /// the pricer refused — and (BIN15 S3) inside a settlement window
    /// whose running average has a hole (`twap_gap`: an open the
    /// instance never saw a mark in force at, or a mark piece longer
    /// than `core_types::BINARY_SETTLE_MARK_GAP_MAX_NS`).
    pub skipped_stale: u64,
    /// BIN15 P0 (F4): re-prices held because the underlying mark is
    /// OLDER than `mark_stale_ns`. Distinct from `skipped_stale`: that
    /// one means an input that does not exist (no mark, no forecast, no
    /// whole average), this one means a mark the tape has left behind,
    /// which is the failure that prices four families off a frozen
    /// number while their books track reality.
    pub skipped_mark_stale: u64,
    /// Re-prices held: the binary book is one-sided or empty.
    pub skipped_book: u64,
    /// Re-prices held: a closing take with nothing to close.
    pub skipped_inventory: u64,
    /// Re-prices held: a notional cap left no room.
    pub skipped_cap: u64,
    /// Re-prices held: the price or size fell off the HIP-4 grid.
    pub skipped_grid: u64,
    /// **E5: re-prices held because the resting quote is PARTIALLY
    /// FILLED.**
    ///
    /// A MODIFY replaces the venue's remaining size wholesale, so
    /// moving a partially-filled quote means the member's
    /// `filled_1e6`/`qty_1e6` pair and the venue's remainder have to
    /// agree across a race — which is where double-count bugs live.
    /// A quote that is already working does not need the help; its
    /// TTL will end it.
    pub skipped_partial: u64,
    /// BIN15 P3 (F6): coverage entries NOT taken because the ask did
    /// not clear `p̂ − e_entry`.
    ///
    /// Not a defect — it is the arm working. The bar is `p > a` plus a
    /// margin, so an instance whose favourite is priced at or above the
    /// model's own belief is one the member declines to pay for. A
    /// counter that stays at zero while entries fire on every instance
    /// means the bound is not binding and the arm is a market order
    /// wearing a price limit.
    pub skipped_entry_price: u64,
    /// BIN15 S5: coverage entries NOT taken YET because the price test
    /// has not held on `entry_persist_polls` consecutive distinct book
    /// snapshots. Counted per reprice, like `skipped_entry_price`; zero
    /// for ever under `entry_persist_polls = 1` (the pre-S5 law).
    pub skipped_entry_persist: u64,
    /// BIN15 S5: instances whose coverage entry the elapsed ceiling
    /// CLOSED — past `entry_elapsed_max_ns` with no run of passing
    /// snapshots under way (none began by then, or the one that did was
    /// broken after it). Counted ONCE per instance, not per reprice, so it
    /// reads as instances; zero for ever with no ceiling. A closed
    /// instance is silent after, so `skipped_entry_price` stops counting
    /// its refusals too.
    pub skipped_entry_elapsed: u64,
    /// Configured families with no live instance (a level, not a total).
    pub families_dormant: u64,
    /// Fills matched to one of this member's pendings.
    pub fills: u64,
    /// Fills whose `order_id` matched no pending. Non-zero means the
    /// member's own book of intents disagrees with the dispatcher's.
    ///
    /// **A SETTLEMENT is not one of these.** The venue places the
    /// settling order itself, so it matches no pending by
    /// construction; counting it here would make this number mean two
    /// things at once and mask the disagreement it exists to report.
    /// See [`Self::settlement_fills`].
    pub unknown_fills: u64,
    /// **E5 (LAW E-7): Arm B quotes repriced in place with a MODIFY**
    /// rather than left to expire.
    ///
    /// Before E5 this member had no cancel path, so a live quote
    /// could only be replaced by letting its TTL run out —
    /// `requote_ttl_ns` was the replace cadence, not a nicety. These
    /// are the reprices that no longer wait.
    pub quotes_modified: u64,
    /// E5: MODIFY attempts the dispatcher refused. The resting quote
    /// is still at its old price, still reserving its cap room, and
    /// the member changed nothing.
    ///
    /// **`NoSuchOrder` is the common one and is not a defect** — a
    /// fill or a TTL beat the reprice, which is a race the venue
    /// wins fair and square.
    pub quotes_modify_refused: u64,
    /// **E5 (LAW E-8): Arm B quotes the member CANCELLED** because
    /// their `ttl_ns` lapsed. The venue has no server-side TTL on
    /// Gtc/Alo orders, so a quote nobody cancels rests forever —
    /// before E5 the member simply forgot it, which was correct in
    /// paper and would have stranded a live quote.
    pub quotes_cancelled: u64,
    /// E5: CANCEL attempts the dispatcher did not perform, for any
    /// reason other than the order already being gone — including a
    /// dispatcher that does not implement the verb at all
    /// (`SubmitErr::Unsupported`, which is what every `Ctx` taking
    /// the trait default answers).
    ///
    /// The member clears its own book at its own deadline either way,
    /// so against a LIVE arm a non-zero value is a quote the member
    /// has stopped tracking and the venue may still hold — which is
    /// what E6's reconciliation exists to find. Against a paper arm
    /// it means the dispatcher never learned to cancel, and its own
    /// TTL law is what retires the order; read the two apart by which
    /// arm the slot is routed to, not by this number alone.
    ///
    /// `NoSuchOrder` is excluded on purpose — there the order really
    /// is gone, which is what the member wanted.
    pub quotes_cancel_refused: u64,
    /// E5: fills that booked against a quote's PREVIOUS client id —
    /// a fill that raced a reprice and was answered under the id the
    /// order had before it moved.
    ///
    /// Expected to be small and non-zero. Before the one-generation
    /// memory these landed in `unknown_fills` and moved no position,
    /// so the member believed it held less than it did.
    pub quotes_raced: u64,
    /// Fills the VENUE generated to settle an instance
    /// (`FILL_FLAG_SETTLEMENT`) rather than a trade the member asked
    /// for.
    ///
    /// They reach the tape — which is what retires a position that
    /// would otherwise be marked at a stale price on a market that no
    /// longer exists — and they move no member position, because the
    /// roll's own `clear_instance` is what flattens the family.
    pub settlement_fills: u64,
}

/// Families one BIN15 view carries. Mirrors
/// `strategy_bin15::BIN15_MAX_FAMILIES`; a mismatch is a compile error
/// at the member's own `debug_assert`, not a silent truncation.
pub const BIN15_VIEW_FAMILIES: usize = 8;

/// One BIN15 family as `/metrics` reads it (O4b). POD.
///
/// What an operator watching a live member needs: WHICH instance a
/// slot holds, what the member thinks it is worth, what it holds of
/// each leg, and — since BIN15 O6 — every input the fair value was
/// built from (`mark`, `strike`, `tau_s`, `den_1e9`) plus the `d_1e6`
/// and `p_raw_1e6` between them. Those six exist so that a `p̂`
/// sitting at 0 or 1e6 can be explained from `/metrics` alone;
/// before them it took an offline replay. `live_outcome == 0` is a dormant slot, which
/// is a level and not an error — a family with no instance on the
/// venue is the normal state of six of the eight.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Bin15FamilyView {
    /// The venue's own id for the live instance; `0` = dormant.
    pub live_outcome: u32,
    /// Whole seconds of pricing horizon at the last reprice, rounded UP
    /// — the variance-time left in the venue's settlement TWAP
    /// (`strategy_bin15::price::pricing_horizon_ns`, BIN15 S3: `τ − 2W/3`
    /// with the window ahead, `τ³/(3W²)` inside it, sub-second for the
    /// last ~22 s). `0` when the family has never priced.
    pub tau_s: u32,
    /// Last fair value ×1e6.
    pub p_hat_1e6: i64,
    /// Yes contracts held ×1e6, from fills.
    pub pos_yes_1e6: i64,
    /// No contracts held ×1e6, from fills.
    pub pos_no_1e6: i64,
    /// BIN15 O6: last RAW (pre-recalibration) fair value ×1e6.
    /// Against `p_hat_1e6` it says what the recal table did.
    pub p_raw_1e6: i64,
    /// BIN15 O6: the live instance's threshold ×1e6; `0` when dormant.
    pub strike_1e6: i64,
    /// BIN15 O6: the underlying mark the last reprice used ×1e6.
    pub mark_1e6: i64,
    /// BIN15 O6: the standardised distance to the strike ×1e6 — the
    /// `z` the Φ lookup was taken at, clamped as the pricer clamps it.
    pub d_1e6: i64,
    /// BIN15 O6: σ√τ ×1e9, the denominator behind `d_1e6`.
    pub den_1e9: i64,
}

/// One entered XSD position as the cli persists it (`xsd-state.tsv`)
/// and `/state` shows it. POD.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XsdPositionView {
    /// The target.
    pub sym: SymbolId,
    /// `1` long, `2` short.
    pub side: u8,
    /// Entry z-sign (`+1` / `−1`).
    pub d: i8,
    /// Grid units filled.
    pub grid_units: u8,
    /// Coins ×1e6 held.
    pub qty_1e6: i64,
    /// Notional ×1e6 booked against the caps.
    pub notional_1e6: i64,
    /// Boundary hour of the entry decision.
    pub entry_hour: i64,
    /// Boundary hour of the last add (`entry_hour` when none).
    pub last_add_hour: i64,
}

// ---------------------------------------------------------------
// HAR H3.5 — the long-tenor set's `/state.har` rows and counters
// ---------------------------------------------------------------

/// Series `/state.har` carries — `core_vol::LONG_SET_MAX` (a const assert
/// in `strategy-set` pins the two together).
pub const HAR_VIEW_SERIES: usize = 12;
/// A series name's bytes — `core_vol::LONG_SET_NAME_MAX`.
pub const HAR_VIEW_NAME_MAX: usize = 12;
/// The dashboard's tenors (the H3 plan's §7 panel), whole days, in
/// `/state` order.
pub const HAR_VIEW_TENORS_D: [u32; 9] = [1, 2, 3, 5, 7, 14, 21, 30, 40];
/// Tenors per series row.
pub const HAR_VIEW_TENORS: usize = HAR_VIEW_TENORS_D.len();
/// Weekdays of the profile, Monday first — `core_vol::WEEKDAYS`.
pub const HAR_VIEW_WEEKDAYS: usize = 7;

/// HAR H3.5: the long-tenor set's own counters (`core_vol::LongSetCounters`,
/// field for field) — defined here so the cli mirrors them without naming
/// `core-vol` (the regime-counters precedent). POD.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HarCounters {
    /// Minute boundaries the clock rolled.
    pub minutes_rolled: u64,
    /// Minute closes delivered to the engines.
    pub closes: u64,
    /// Closes that crossed a UTC day (the close law ran).
    pub day_closes: u64,
    /// First new-day minutes held for a later poll (the stagger).
    pub held: u64,
    /// Held minutes forced out by a newer minute of their series.
    pub forced: u64,
    /// The costliest day close, ns (the engine's close law alone).
    pub day_close_ns_max: u64,
    /// The newest day close's cost, ns.
    pub day_close_ns_last: u64,
    /// Bumped at every day close and at the boot restore.
    pub epoch: u64,
}

/// HAR H3.5: one long-tenor series as `/state.har` and the dashboard read
/// it. POD, 176 B.
///
/// The forecasts, the pairs, the profile and the day census move only at
/// the series' own UTC day close (or the boot restore), so the set builds
/// this row there and the 1 s publish copies it; `last_min_ms`,
/// `open_minutes` and `gaps` are read live at the publish.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HarSeriesView {
    /// The newest folded minute's open, ms since the epoch (0 = none).
    pub last_min_ms: u64,
    /// The newest CLOSED day's UTC midnight, ms since the epoch (0 = none).
    pub newest_day_ms: u64,
    /// Non-contiguous minutes folded (one splice per restart is normal).
    pub gaps: u64,
    /// The series' state epoch when this row was built.
    pub epoch: u64,
    /// The feed's symbol.
    pub feed: SymbolId,
    /// Minutes folded into the open day.
    pub open_minutes: u32,
    /// σ annualised ×1e6 at each [`HAR_VIEW_TENORS_D`] tenor: the raw
    /// fold (`0` = none — cold, or before the first close).
    pub raw_1e6: [i32; HAR_VIEW_TENORS],
    /// σ annualised ×1e6: the rolling fit applied to the fold (`0` =
    /// unfitted — fewer than 60 pairs).
    pub fit_1e6: [i32; HAR_VIEW_TENORS],
    /// The weekday profile ×1e6, Monday first: the mean `Σ r²` of that
    /// weekday's observed days over the mean observed day (`1e6` = an
    /// average day; `0` = no observed day of it).
    pub weekday_1e6: [i32; HAR_VIEW_WEEKDAYS],
    /// Observed days behind each weekday's mean (saturating at 255).
    pub weekday_n: [u8; HAR_VIEW_WEEKDAYS],
    /// Pairs held at each tenor (the ring holds 128).
    pub pairs: [u8; HAR_VIEW_TENORS],
    /// The `har.toml` name, `name_len` bytes live.
    pub name: [u8; HAR_VIEW_NAME_MAX],
    /// Live bytes of `name`.
    pub name_len: u8,
    /// 1 when the fold forecasts: the newest 30 closed days are resident
    /// and every one was observed.
    pub warm: u8,
    /// Closed days resident (≤ 64).
    pub days: u8,
    /// Resident days with no minute at all (holes — the empty-day law).
    pub empty_days: u8,
    /// Bit `k`: tenor `k` has a fit.
    pub fitted: u16,
    /// Bit `k`: tenor `k`'s QLIKE over a FULL window says the fit beats
    /// the raw fold.
    pub fit_beats_raw: u16,
}

impl HarSeriesView {
    /// The live name bytes.
    #[inline]
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(HAR_VIEW_NAME_MAX)]
    }
}

const _: () = assert!(core::mem::size_of::<HarSeriesView>() == 176);
const _: () = assert!(core::mem::size_of::<HarCounters>() == 64);
const _: () = assert!(HAR_VIEW_TENORS <= 16, "fitted / fit_beats_raw are u16 masks");

/// A UTC day, ms — `core_vol::DAY_MS` (a const assert in `strategy-set`
/// pins the two together).
pub const HAR_DAY_MS: u64 = 86_400_000;

/// HAR H3.7: the `engine_har_*` gauges, as one law the cli mirrors and the
/// alloc gate measures. POD.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HarGauges {
    /// `engine_har_series_configured`.
    pub configured: i64,
    /// `engine_har_series_warm`: series whose fold forecasts.
    pub warm: i64,
    /// `engine_har_day_age_max_s`: seconds since the stalest series'
    /// newest closed day ENDED (`-1` = no series has closed a day).
    pub day_age_max_s: i64,
    /// `engine_har_day_close_ns_max`: the costliest day close since boot.
    pub day_close_ns_max: i64,
}

/// HAR H3.7: the gauges from the set's rows — `configured` series, the
/// first `min(configured, rows.len())` rows read — and its counters, at
/// `wall_ms`. Never allocates.
#[must_use]
pub fn har_gauges(
    rows: &[HarSeriesView],
    configured: u32,
    counters: &HarCounters,
    wall_ms: u64,
) -> HarGauges {
    let m = (configured as usize).min(rows.len());
    let mut warm = 0i64;
    let mut age_max: i64 = -1;
    let mut i = 0usize;
    while i < m {
        let r = &rows[i];
        warm += i64::from(r.warm);
        if r.newest_day_ms != 0 {
            let closed_ms = r.newest_day_ms.saturating_add(HAR_DAY_MS);
            let age = (wall_ms.saturating_sub(closed_ms) / 1_000).min(i64::MAX as u64) as i64;
            if age > age_max {
                age_max = age;
            }
        }
        i += 1;
    }
    HarGauges {
        configured: i64::from(configured),
        warm,
        day_age_max_s: age_max,
        day_close_ns_max: counters.day_close_ns_max.min(i64::MAX as u64) as i64,
    }
}

// ---------------------------------------------------------------
// risk — the venue-aware notional caps (`docs/risk-policy.md`)
// ---------------------------------------------------------------

/// One venue's position limits, as the coded members enforce them.
///
/// Two units, because two kinds of instrument. A Polymarket token or a
/// USDT-margined perp is naturally sized in DOLLARS, so its cap is a
/// notional. A Deribit inverse contract is one whole coin by
/// construction — its "size" is a coin count and its dollar value moves
/// with the index — so capping it in dollars means the effective size
/// shrinks as the coin rises and the member starts refusing at a price
/// nobody chose. Deribit is therefore SIZE-capped and everything else
/// NOTIONAL-capped.
///
/// `0` means "this unit does not apply here", and a member that can only
/// test the absent unit must REFUSE rather than read the zero as
/// unlimited. That is the whole reason the field is zero and not
/// `i64::MAX`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VenueCaps {
    /// Max single-order notional USD ×1e6; `0` = size-capped venue.
    pub leg_usd_1e6: i64,
    /// Max single-order size ×1e6 in the instrument's own unit;
    /// `0` = notional-capped venue.
    pub leg_qty_1e6: i64,
    /// Max net per-symbol notional USD ×1e6; `0` = size-capped venue.
    pub sym_usd_1e6: i64,
    /// Max net per-symbol size ×1e6; `0` = notional-capped venue.
    pub sym_qty_1e6: i64,
    /// Max total book notional USD ×1e6. Always dollars — a book total
    /// has to be in one currency.
    pub table_usd_1e6: i64,
}

/// The base tier (operator ruling 2026-08-29, $50k research book):
/// $10 000 per order, $20 000 per symbol, $100 000 total.
pub const CAPS_BASE: VenueCaps = VenueCaps {
    leg_usd_1e6: 10_000_000_000,
    leg_qty_1e6: 0,
    sym_usd_1e6: 20_000_000_000,
    sym_qty_1e6: 0,
    table_usd_1e6: 100_000_000_000,
};

/// Deribit (operator amendment 2026-09-10): **one whole coin** per order
/// and per symbol, with a $250 000 book total.
///
/// A Deribit inverse contract IS one coin, so this is the venue's own
/// natural unit and it does not move with the index. The book total
/// stays in dollars and is sized so one full delta-hedged campaign — a
/// one-coin hedge plus its premium — fits with room for the hedge to
/// walk as delta does.
pub const CAPS_DERIBIT: VenueCaps = VenueCaps {
    leg_usd_1e6: 0,
    leg_qty_1e6: 1_000_000,
    sym_usd_1e6: 0,
    sym_qty_1e6: 1_000_000,
    table_usd_1e6: 250_000_000_000,
};

/// The caps that apply to an order on `venue_byte`
/// ([`core_types::VenueId`] as a raw byte).
#[inline]
#[must_use]
pub const fn caps_for_venue(venue_byte: u8) -> VenueCaps {
    if venue_byte == VenueId::Deribit as u8 {
        CAPS_DERIBIT
    } else {
        CAPS_BASE
    }
}

/// The caps for the venue `sym` belongs to.
#[inline]
#[must_use]
pub fn caps_for_sym(sym: SymbolId) -> VenueCaps {
    caps_for_venue(core_types::symbol_venue_byte(sym))
}

/// P5: the ONE order-size law, beside the caps table it reads.
///
/// Every member that sizes an order asks the same two questions — what
/// is this worth, and does it fit — and the answers must not differ by
/// member. They lived on `VrpStrategy` until P5; icdp asked the second
/// one by hand.
pub mod risk {
    /// USD ×1e6 notional of `qty_1e6` units at `px_1e6`. `i128`
    /// intermediate, saturating — a notional that cannot be represented
    /// is treated as INFINITE, which refuses rather than admits.
    #[inline]
    #[must_use]
    pub fn notional_1e6(px_1e6: i64, qty_1e6: i64) -> i64 {
        let n = (px_1e6 as i128 * qty_1e6.unsigned_abs() as i128) / 1_000_000;
        i64::try_from(n).unwrap_or(i64::MAX)
    }

    /// Whether one order of `qty_1e6` units and `notional_1e6` dollars
    /// is inside `caps`, in whichever unit that venue is capped in.
    ///
    /// A venue capped in the OTHER unit refuses — a `0` there means
    /// "this unit does not apply here", never "unlimited", and reading
    /// it as unlimited is the one mistake this shape exists to prevent.
    #[inline]
    #[must_use]
    pub fn size_ok(caps: super::VenueCaps, qty_1e6: i64, notional_1e6: i64) -> bool {
        let q = qty_1e6.unsigned_abs();
        if caps.leg_qty_1e6 > 0 {
            return q <= caps.leg_qty_1e6.unsigned_abs()
                && q <= caps.sym_qty_1e6.unsigned_abs();
        }
        if caps.leg_usd_1e6 > 0 {
            return notional_1e6 <= caps.leg_usd_1e6 && notional_1e6 <= caps.sym_usd_1e6;
        }
        false
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

    /// Called once per [`RuleTableV2`] slot popped from the ruleset
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
    fn on_ruleset_table(&mut self, table: &RuleTableV2) {
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

    /// Called once per [`DepthTopK`] popped from a depth lane
    /// (WS10-B). Snapshots are change-gated at the ingress (a delivery
    /// means the top-K actually moved); `flags` carrying
    /// `DEPTH_FLAG_STALE` marks a book mid-resync — never trade on a
    /// stale snapshot.
    ///
    /// Defaulted to a no-op so every existing strategy compiles and
    /// behaves unchanged; `strategy-set` forwards it to enabled
    /// members like `on_tick`. Monomorphized — no `dyn`. Lands dark
    /// in Stage 2: no in-tree strategy consumes it yet.
    #[inline]
    fn on_depth<C: Ctx>(&mut self, depth: &DepthTopK, ctx: &mut C) {
        let _ = (depth, ctx);
    }

    /// Called once per [`OptSummary`] popped from an options-summary
    /// lane (VM2 V2 — the kind-6 channel's first engine-side
    /// consumer; before v2 it was capture-only). Records carry raw
    /// venue units (`mark_px_1e9` coin-denominated on Deribit) and
    /// venue-optional fields flagged in `flags` — consumers must
    /// honor the flag bits.
    ///
    /// Defaulted to a no-op so every existing strategy compiles and
    /// behaves unchanged; `strategy-set` forwards it to enabled
    /// members like `on_depth`. Monomorphized — no `dyn`.
    #[inline]
    fn on_opt_summary<C: Ctx>(&mut self, opt: &OptSummary, ctx: &mut C) {
        let _ = (opt, ctx);
    }

    /// Called once per [`TradePrint`] popped from the trade lane (XMM
    /// XH1): the venue's public tape — fed today by the Hyperliquid
    /// ingress. A maker reads its queue being consumed from it.
    ///
    /// Defaulted to a no-op so every existing strategy compiles and
    /// behaves unchanged; `strategy-set` forwards it to enabled members
    /// like `on_tick`. Monomorphized — no `dyn`.
    #[inline]
    fn on_trade<C: Ctx>(&mut self, trade: &TradePrint, ctx: &mut C) {
        let _ = (trade, ctx);
    }

    /// Called once per [`OrderEvent`] about an order this member placed
    /// (XMM XH1): it rests, it was refused, it left the book, it is
    /// done. The paper model and the live gateway emit the same events,
    /// so a member's order state machine runs identically in both.
    ///
    /// Defaulted to a no-op so every existing strategy compiles and
    /// behaves unchanged. `strategy-set` routes each event to the slot
    /// named by its `strategy_id` ALONE — the fill law, never a fan-out.
    #[inline]
    fn on_order_event<C: Ctx>(&mut self, event: &OrderEvent, ctx: &mut C) {
        let _ = (event, ctx);
    }

    /// Periodic timer. `now_ns` is the current timestamp; the engine
    /// calls this at roughly the interval returned by `timer_period_ns`.
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C);

    /// How often `on_timer` should fire (ns). `u64::MAX` disables.
    fn timer_period_ns(&self) -> u64;

    /// The member's static regime label set (RG2,
    /// `docs/regime-and-dashboard-plan.md` §4.2): the regimes it may
    /// ENTER in, ∃ over up to four product terms, plus its off-mode.
    /// The strategy set evaluates it on regime change only — never per
    /// tick — and fans [`Self::on_regime`] out when the verdict flips.
    /// Default = unconstrained / soft: every strategy that exists today
    /// keeps its exact behaviour. A coded member's compiled constant
    /// can be overridden at boot from `regime.toml [labels.<member>]`
    /// through [`Self::set_regime_label`].
    #[inline]
    fn regime_label(&self) -> RegimeLabelSet {
        RegimeLabelSet::ANY
    }

    /// Boot-time override of [`Self::regime_label`] (the `regime.toml`
    /// `[labels.<member>]` seam). Default: ignored — a member that does
    /// not store a label cannot be relabelled, and the set logs the
    /// refusal at boot.
    #[inline]
    fn set_regime_label(&mut self, set: RegimeLabelSet) -> bool {
        let _ = set;
        false
    }

    /// Edge-triggered regime gate (RG2): called by the strategy set
    /// only when this member's gate CHANGED — opened, or closed
    /// (soft: block entries and let the member's own exit law drain;
    /// hard: block entries and run the member's flatten path). Never
    /// called per tick. The default ignores it, which is exactly right
    /// for an unconstrained member (its gate never closes).
    #[inline]
    fn on_regime<C: Ctx>(&mut self, gate: RegimeGate, ctx: &mut C) {
        let _ = (gate, ctx);
    }

    /// Called once on graceful shutdown.
    fn on_stop<C: Ctx>(&mut self, ctx: &mut C);
}

/// The verdict handed to [`Strategy::on_regime`] (RG2). POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RegimeGate {
    /// The effective words the verdict was taken on (index = profile;
    /// `REGIME_PROFILES` live, the rest UNKNOWN).
    pub effective: [RegimeWord; 4],
    /// `true` = the member may enter; `false` = closed.
    pub open: bool,
    /// The member's off-mode when closed: [`REGIME_OFF_SOFT`] or
    /// [`REGIME_OFF_HARD`] (meaningful only while `!open`).
    pub off: u8,
    /// Explicit padding — always zero.
    _pad: [u8; 6],
}

impl RegimeGate {
    /// Construct without naming the padding.
    #[inline(always)]
    pub const fn new(effective: [RegimeWord; 4], open: bool, off: u8) -> Self {
        Self {
            effective,
            open,
            off,
            _pad: [0; 6],
        }
    }

    /// The boot value: open, every word UNKNOWN (no detector yet).
    pub const OPEN_UNKNOWN: Self = Self::new([RegimeWord::UNKNOWN; 4], true, REGIME_OFF_SOFT);

    /// `true` when closed with the hard off-mode (flatten now).
    #[inline(always)]
    pub const fn hard_closed(&self) -> bool {
        !self.open && self.off == REGIME_OFF_HARD
    }
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
        let mut table = RuleTableV2::EMPTY;
        table.len = 1;
        s.on_ruleset_table(&table);
        assert!(s.started);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook cannot submit (no Ctx)");
    }

    #[test]
    fn regime_hooks_default_to_unconstrained_and_noop() {
        // RG2 defaults: an unconstrained label, no relabel, a no-op
        // gate callback, and the counters' "no detector" shape.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        assert_eq!(s.regime_label(), RegimeLabelSet::ANY);
        assert!(!s.set_regime_label(RegimeLabelSet::ANY));
        let closed = RegimeGate::new([RegimeWord::UNKNOWN; 4], false, REGIME_OFF_HARD);
        assert!(closed.hard_closed());
        let boot = RegimeGate::OPEN_UNKNOWN;
        assert!(!boot.hard_closed());
        assert!(boot.open && boot.off == REGIME_OFF_SOFT);
        s.on_regime(closed, &mut ctx);
        assert_eq!(s.ticks, 0);
        assert_eq!(ctx.submitted, 0);
        let c = s.regime_counters();
        assert_eq!(c, RegimeCounters::default());
        assert_eq!(c.effective[0], RegimeWord::UNKNOWN);
        assert_eq!(c.declared[1], RegimeWord::EMPTY);
        assert_eq!(c.declared_ts_ns[0], 0);
        assert_eq!(c.declared_ttl_ns[0], 0);
        assert_eq!(c.gates, [0; 8]);
        assert_eq!(c.configured, 0);
        assert_eq!(core::mem::size_of::<RegimeGate>(), 40);
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
    fn on_depth_defaults_to_noop() {
        // WS10-B default: a delivered depth snapshot touches neither
        // strategy state nor the Ctx.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let d = DepthTopK::EMPTY;
        s.on_depth(&d, &mut ctx);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook must not submit");
    }

    #[test]
    fn on_trade_defaults_to_noop() {
        // XMM XH1 default: a delivered print touches neither strategy
        // state nor the Ctx — every member that exists today is
        // unchanged by the new lane.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let p = TradePrint::new(
            1,
            core_types::VenueId::Hyperliquid,
            7,
            9,
            1_790_000_000_000,
            187_000_000,
            1_000_000,
            core_types::TRADE_AGGRESSOR_BUY,
        );
        s.on_trade(&p, &mut ctx);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook must not submit");
    }

    #[test]
    fn on_order_event_defaults_to_noop() {
        // XMM XH1 default: even a terminal event about an order the
        // strategy never placed is inert through the default hook.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let e = OrderEvent::new(
            1,
            core_types::VenueId::Hyperliquid,
            7,
            42,
            6,
            core_types::ORDER_EVENT_REJECTED,
            core_types::ORDER_EVENT_REASON_BAD_ALO_PX,
            0,
        );
        s.on_order_event(&e, &mut ctx);
        assert_eq!(s.ticks, 0, "default hook must not touch strategy state");
        assert_eq!(ctx.submitted, 0, "default hook must not submit");
    }

    #[test]
    fn on_opt_summary_defaults_to_noop() {
        // VM2 V2 default: a delivered options record touches neither
        // strategy state nor the Ctx.
        let mut ctx = NoopCtx {
            submitted: 0,
            now: 0,
        };
        let mut s = NoopStrat {
            started: false,
            ticks: 0,
        };
        s.on_start(&mut ctx).unwrap();
        let o = OptSummary::new(
            1,
            core_types::VenueId::Deribit,
            7,
            0,
            0,
            650_000_000,
            0,
            0,
            500_000_000,
            1,
            1,
            -1,
        );
        s.on_opt_summary(&o, &mut ctx);
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
        let mut table = RuleTableV2::EMPTY;
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
        // XSD (slot 2): the same posture — zero counters, no state, no
        // positions, and the view buffer is left untouched.
        assert_eq!(s.xsd_counters(), XsdCounters::default());
        assert_eq!(s.xsd_state_epoch(), 0);
        let mut out = [XsdPositionView::default(); 2];
        assert_eq!(s.xsd_positions_view(&mut out), 0);
        assert_eq!(out, [XsdPositionView::default(); 2]);
        // XMM (slot 6): zero counters, and the view buffer untouched.
        let mut c = XmmCounters {
            placed: 9,
            ..XmmCounters::default()
        };
        s.xmm_counters(&mut c);
        assert_eq!(c, XmmCounters::default());
        let mut rows = [XmmPerpView::default(); 2];
        assert_eq!(s.xmm_perps_view(&mut rows), 0);
        assert_eq!(rows, [XmmPerpView::default(); 2]);
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

    /// HAR H3.7: the gauge law — warm series counted, the STALEST newest
    /// closed day's age (a day ends at the next midnight), `-1` before any
    /// close, rows past `configured` ignored, the counter saturated.
    #[test]
    fn har_gauges_read_the_stalest_day_and_only_the_configured_rows() {
        let day = HAR_DAY_MS;
        let mut rows = [HarSeriesView::default(); 3];
        rows[0].warm = 1;
        rows[0].newest_day_ms = 10 * day;
        rows[1].newest_day_ms = 8 * day;
        rows[2].warm = 1;
        rows[2].newest_day_ms = day; // past `configured`: never read
        let c = HarCounters {
            day_close_ns_max: u64::MAX,
            ..HarCounters::default()
        };
        let now = 11 * day + 5_000;
        let g = har_gauges(&rows, 2, &c, now);
        assert_eq!((g.configured, g.warm), (2, 1));
        assert_eq!(g.day_age_max_s, (2 * day + 5_000) as i64 / 1_000, "series 1: day 8 ended at day 9");
        assert_eq!(g.day_close_ns_max, i64::MAX, "saturated, never negative");

        // Nothing closed yet: the age is -1; a clock behind the close is 0.
        let fresh = [HarSeriesView::default(); 2];
        assert_eq!(har_gauges(&fresh, 2, &HarCounters::default(), now).day_age_max_s, -1);
        assert_eq!(har_gauges(&rows, 1, &c, 0).day_age_max_s, 0);
        // More series configured than rows handed: only the rows count.
        assert_eq!(har_gauges(&rows[..1], 12, &c, now).configured, 12);
        assert_eq!(har_gauges(&rows[..1], 12, &c, now).warm, 1);
    }
}
