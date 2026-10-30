// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The broker surface every host touches (contract §3.12).
//!
//! `pine-broker` implements the behaviour (`TvBroker`, `NativeBroker`);
//! these are the PODs that cross between it and its hosts
//! (`strategy-pine`, `pine-screen`, `pine-conformance`,
//! `cli backtest --strategy pine`). `pine-broker` depends on `pine-ir`
//! only, so it never names `core-types`: an [`EngineIntent`] is
//! venue-neutral and the host maps it onto `core_types::Order` with no
//! rescale (D18 — the units already match).

use crate::ids::StrKey;

/// A live touch, from the host's book. Never from a `Bar`: a bar's close
/// is history by the time an order is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Quote {
    /// Best bid ×1e6.
    pub bid_1e6: i64,
    /// Best ask ×1e6.
    pub ask_1e6: i64,
    /// Displayed bid size ×1e6.
    pub bid_qty_1e6: i64,
    /// Displayed ask size ×1e6.
    pub ask_qty_1e6: i64,
    /// The quote's event time in the host's event clock (ns; monotonic live,
    /// virtual in replay). Compared only with the host's `now_ns` argument
    /// in the same clock — never with a system clock (P5).
    pub ts_ns: u64,
    /// Bit 0: the tick was stale (`core_types::TICK_FLAG_STALE`).
    pub flags: u32,
    /// Always zero.
    pub _pad: u32,
}

/// The execution primitive of one engine instruction. Maps 1:1 onto the
/// engine's two order kinds (`core_fill::ORDER_KIND_IOC` /
/// `ORDER_KIND_MAKER`) — there is no third fill law (D17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Primitive {
    /// IoC under the I1 law: judged once at the first fresh two-sided tick
    /// at or after emit + Δ, filled at the touch capped by displayed size,
    /// the remainder cancelled. Market orders, triggered stops, and limit
    /// orders on an `ioc` script (D21).
    #[default]
    TakerIoc = 0,
    /// Post-only maker under the strict-cross law. Limit entries and
    /// take-profit legs on a `maker` script (D21) only.
    MakerPostOnly = 1,
    /// IoC that may only reduce the position: exits, closes, liquidations,
    /// the D13 flatten.
    ReduceOnlyIoc = 2,
}

/// What an engine instruction does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum EngineVerb {
    /// A new order.
    #[default]
    Submit = 0,
    /// Replace a resting maker in place (LAW E-7: a requote is a MODIFY).
    Modify = 1,
    /// Cancel a resting maker. Never blocked by a cap.
    Cancel = 2,
}

/// The per-script limit-order primitive (ruling D21, manifest
/// `limit_primitive`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum LimitPrimitive {
    /// `"ioc"`: pending limits and stops are held in the member; when the
    /// touch reaches the level the member fires an IoC. Nothing rests at
    /// the venue.
    #[default]
    Ioc = 0,
    /// `"maker"`: limit entries and take-profit legs rest as post-only
    /// makers at their limit; market orders and stops stay IoC. Resting
    /// orders are reconciled at boot and swept on halt (E-8).
    Maker = 1,
}

/// One instruction for the host's dispatcher bridge. 64 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C, align(64))]
pub struct EngineIntent {
    /// Submit, modify or cancel.
    pub verb: EngineVerb,
    /// IoC, maker or reduce-only IoC.
    pub primitive: Primitive,
    /// Order side, `core_types::Side` numbering: 0 = buy (bid), 1 = sell (ask).
    pub side: u8,
    /// Bit 0: reduce-only.
    pub flags: u8,
    /// Broker-assigned client order id (monotone per script); the host maps
    /// it into the venue cloid with the slot encoding (LAW E-9).
    pub client_oid: u32,
    /// `Modify`: the order being replaced. 0 otherwise.
    pub prev_client_oid: u32,
    /// Always zero.
    pub _pad0: u32,
    /// Limit price ×1e6 — for an IoC, the protective limit (the touch
    /// ± `cross_ticks`, or the script's own limit), already on the venue tick.
    pub px_1e6: i64,
    /// Unsigned quantity ×1e6, already floored to the venue step.
    pub qty_1e6: i64,
    /// `Order.ttl_ns`: 0 = never expires (the I1 TTL law).
    pub ttl_ns: u64,
    /// The Pine order id this instruction serves (tape and logs).
    pub id: StrKey,
    /// Always zero.
    pub _pad1: [u64; 2],
}

/// How quantities are decided. Mandatory on every run record: comparing
/// scripts at their published `default_qty_type` is a leverage contest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub enum SizingMode {
    /// The script's own `default_qty_*` / `qty`. Conformance only — not
    /// comparable across scripts.
    #[default]
    AsDeclared,
    /// Every entry's unit is `notional_1e6` USD; explicit quantities and
    /// `qty_percent` scale by the same ratio (the N-rule, D22).
    FixedNotional {
        /// Unit notional, USD ×1e6.
        notional_1e6: i64,
    },
}

/// Native-mode configuration, built by the host from the manifest and the
/// venue's measured instrument facts. 72 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct NativeCfg {
    /// The unit order notional, USD ×1e6 (manifest `qty_notional_1e6`, D22).
    pub unit_notional_1e6: i64,
    /// Per-script open-exposure cap, USD ×1e6.
    pub cap_instance_1e6: i64,
    /// Per-script day-notional cap, USD ×1e6 (UTC day on event time).
    pub cap_day_1e6: i64,
    /// Venue minimum order notional, USD ×1e6; smaller orders are dropped
    /// and counted, never rounded up.
    pub min_notional_1e6: i64,
    /// Venue quantity step ×1e6.
    pub qty_step_1e6: i64,
    /// Venue price tick ×1e6.
    pub tick_1e6: i64,
    /// A quote older than this (event clock) blocks every emission.
    pub quote_stale_ns: i64,
    /// Ticks beyond the touch for a market or triggered-stop IoC's
    /// protective limit.
    pub cross_ticks: u32,
    /// Pending plus resting orders this script may hold.
    pub max_open: u32,
    /// D21.
    pub limit_primitive: LimitPrimitive,
    /// 1 on a Live route: `percent_of_equity` sizing is refused.
    pub live: u8,
    /// Always zero.
    pub _pad: [u8; 6],
}

/// TV-equivalent configuration (conformance and the tier-1 screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TvCfg {
    /// `AsDeclared` for a conformance fixture; `FixedNotional` for the
    /// screen.
    pub sizing: SizingMode,
    /// Seconds added to a bar's open before taking the day for the
    /// intraday risk rules (the exchange day). No clock is read.
    pub day_offset_s: i32,
    /// Always zero.
    pub _pad: u32,
}

/// Every reason the broker declined or adjusted something. A refusal is
/// never silent and never only a total (the BIN15 doctrine). The first
/// three fields plus `ioc_sent` feed the UNJUDGED gate of lane B'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct BrokerCounters {
    /// Native: emissions blocked by `cap_instance` / `cap_day` / the slot caps.
    pub skipped_cap: u64,
    /// Native: submits refused because the script held `max_open` orders.
    pub skipped_max_open: u64,
    /// Native: IoC remainders cancelled by the I1 law (partial or no fill).
    pub ioc_remainder_cancelled: u64,
    /// Native: IoCs sent.
    pub ioc_sent: u64,
    /// Native: IoCs that filled at least partly.
    pub ioc_filled: u64,
    /// Native: makers sent.
    pub maker_sent: u64,
    /// Native: makers that filled at least partly.
    pub maker_filled: u64,
    /// Native: modifications emitted in place of cancel + submit (E-7).
    pub modifies: u64,
    /// Native: emissions blocked by a stale quote.
    pub skipped_stale: u64,
    /// Native: orders below `min_notional_1e6`, dropped.
    pub skipped_min_notional: u64,
    /// Native: `percent_of_equity` sizing refused on a Live route.
    pub refused_pct_equity_live: u64,
    /// Native: fills that matched no issued `client_oid` (a defect).
    pub unknown_fills: u64,
    /// Quantity resolved to zero or below a step.
    pub sized_to_zero: u64,
    /// Intents committed (W21).
    pub intents_committed: u64,
    /// Intents that modified a pending order with the same `(kind, id)`.
    pub intents_modified: u64,
    /// `pyramiding` refusals.
    pub entries_refused_pyramiding: u64,
    /// `strategy.risk.allow_entry_in` refusals.
    pub entries_refused_allow_entry_in: u64,
    /// `strategy.risk.max_position_size` clamps.
    pub entries_clamped_max_position: u64,
    /// OCA cancellations.
    pub orders_cancelled_oca: u64,
    /// OCA reductions.
    pub orders_reduced_oca: u64,
    /// Margin calls (TV-equivalent mode).
    pub margin_calls: u64,
    /// `strategy.risk.max_drawdown` trips.
    pub risk_trips_drawdown: u64,
    /// `strategy.risk.max_intraday_loss` trips.
    pub risk_trips_intraday_loss: u64,
    /// `strategy.risk.max_intraday_filled_orders` trips.
    pub risk_trips_intraday_orders: u64,
    /// `strategy.risk.max_cons_loss_days` trips.
    pub risk_trips_cons_loss_days: u64,
    /// `calc_on_order_fills` re-evaluations.
    pub reevals_on_order_fill: u64,
}

/// The header of one run, written once at the end by the harness. Money
/// is ×1e12 (computation domain); the harness renders to USD ×1e6 with
/// `scale::e12_to_1e6_floor` (P&L) / `_ceil` (risk) when it builds a
/// `TrialSummary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RunRecord {
    /// FNV-1a 64 over the `StrategyDecl`'s bytes (field by field, little
    /// endian) — proves two runs were sized the same way.
    pub decl_hash64: u64,
    /// How quantities were decided.
    pub sizing: SizingMode,
    /// `strategy.initial_capital` ×1e12.
    pub initial_capital_1e12: i128,
    /// Final equity ×1e12.
    pub final_equity_1e12: i128,
    /// Net profit ×1e12.
    pub net_profit_1e12: i128,
    /// Gross profit ×1e12.
    pub gross_profit_1e12: i128,
    /// Gross loss ×1e12 (≥ 0).
    pub gross_loss_1e12: i128,
    /// Commission paid ×1e12.
    pub commission_paid_1e12: i128,
    /// Max drawdown ×1e12 (≥ 0).
    pub max_drawdown_1e12: i128,
    /// Bars stepped.
    pub bars: u32,
    /// Fills booked.
    pub fills: u32,
    /// Winning closed trades.
    pub wins: u32,
    /// Losing closed trades.
    pub losses: u32,
    /// Refusal and adjustment counters.
    pub counters: BrokerCounters,
}

const _: () = assert!(core::mem::size_of::<Quote>() == 48);
const _: () = assert!(core::mem::size_of::<Primitive>() == 1);
const _: () = assert!(core::mem::size_of::<EngineVerb>() == 1);
const _: () = assert!(core::mem::size_of::<LimitPrimitive>() == 1);
const _: () = assert!(core::mem::size_of::<EngineIntent>() == 64);
const _: () = assert!(core::mem::align_of::<EngineIntent>() == 64);
const _: () = assert!(core::mem::size_of::<SizingMode>() == 16);
const _: () = assert!(core::mem::size_of::<NativeCfg>() == 72);
const _: () = assert!(core::mem::size_of::<TvCfg>() == 24);
const _: () = assert!(core::mem::size_of::<BrokerCounters>() == 26 * 8);
// 8 + 16 (SizingMode) + 8 pad + 7 × 16 (i128, align 16) + 4 × 4 + 26 × 8 (BrokerCounters).
const _: () = assert!(core::mem::size_of::<RunRecord>() == 368);
const _: () = assert!(core::mem::align_of::<RunRecord>() == 16);
