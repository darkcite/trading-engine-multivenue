// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Order intents, trades and the broker read-back (contract §3.12).

use crate::ids::{StrKey, Sym};

/// What a `strategy.*` call asks for. Numbering frozen since v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum IntentKind {
    /// `strategy.entry`.
    #[default]
    Entry = 0,
    /// `strategy.exit`.
    Exit = 1,
    /// `strategy.close`.
    Close = 2,
    /// `strategy.close_all`.
    CloseAll = 3,
    /// `strategy.order`.
    Order = 4,
    /// `strategy.cancel`.
    Cancel = 5,
    /// `strategy.cancel_all`.
    CancelAll = 6,
}

/// Position direction of an `Entry` / `Order` (`strategy.long` /
/// `strategy.short`). Unused (0) for the other kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum IntentSide {
    /// `strategy.long`.
    #[default]
    Long = 0,
    /// `strategy.short`.
    Short = 1,
}

/// OCA behaviour of an order's group (`strategy.oca.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum OcaType {
    /// No group.
    #[default]
    None = 0,
    /// `strategy.oca.cancel`.
    Cancel = 1,
    /// `strategy.oca.reduce`.
    Reduce = 2,
}

/// [`OrderIntent::flags`] bits: which optional arguments the call carried.
/// A field whose bit is clear is meaningless and must be 0.
pub mod intent_flag {
    /// `qty` given (contracts). Clear ⇒ the broker applies the default size.
    pub const QTY: u16 = 1 << 0;
    /// `qty_percent` given (exit/close).
    pub const QTY_PCT: u16 = 1 << 1;
    /// Absolute `limit` price given.
    pub const LIMIT: u16 = 1 << 2;
    /// Absolute `stop` price given.
    pub const STOP: u16 = 1 << 3;
    /// `profit` given — a **tick distance** (W23).
    pub const PROFIT: u16 = 1 << 4;
    /// `loss` given — a tick distance (W23).
    pub const LOSS: u16 = 1 << 5;
    /// `trail_price` given (absolute activation price).
    pub const TRAIL_PRICE: u16 = 1 << 6;
    /// `trail_points` given (activation distance, ticks).
    pub const TRAIL_POINTS: u16 = 1 << 7;
    /// `trail_offset` given (ticks).
    pub const TRAIL_OFFSET: u16 = 1 << 8;
    /// `immediately = true` (`strategy.close` / `close_all`).
    pub const IMMEDIATELY: u16 = 1 << 9;
    /// The id was built at runtime (a series string); `id_sym` is `NONE`.
    pub const ID_DYNAMIC: u16 = 1 << 10;
    /// Every defined bit.
    pub const DEFINED: u16 = (1 << 11) - 1;
}

/// One `strategy.*` call's request, as the VM emits it. 128 bytes, two
/// cache lines.
///
/// **No f64 reaches this struct** (D18): the `strategy.*` builtin converts
/// every price with `scale::to_1e6` (round half away from zero, saturating;
/// NaN ⇒ the flag stays clear). Prices are absolute ×1e6; tick distances
/// (`profit`, `loss`, `trail_points`, `trail_offset`) are tick counts ×1e6 —
/// the broker multiplies by `syminfo.mintick`.
///
/// Ids are [`StrKey`]s (FNV-1a of the id's bytes), so a constant id and the
/// same id built at runtime match, and `from_entry` / OCA names compare by
/// key. Intents are **committed** by the host after every evaluation,
/// realtime ticks included (W21: orders placed or filled within a bar are not
/// rolled back); a later intent with the same `(kind, id)` is a
/// modification of the pending order, not a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C, align(64))]
pub struct OrderIntent {
    /// What was asked for.
    pub kind: IntentKind,
    /// Direction (`Entry`, `Order`); `Long` (0) for the other kinds.
    pub side: IntentSide,
    /// [`intent_flag`] bits.
    pub flags: u16,
    /// The OCA behaviour of `oca_name`'s group.
    pub oca_type: OcaType,
    /// Always zero.
    pub _pad0: [u8; 3],
    /// The order id (`Cancel`: the id to cancel; `CloseAll`/`CancelAll`: NONE).
    pub id: StrKey,
    /// `Exit`: the entry it closes (NONE = every entry). `Close`: the entry id
    /// to close. NONE otherwise.
    pub from_entry: StrKey,
    /// The OCA group name, or NONE.
    pub oca_name: StrKey,
    /// Contracts ×1e6 ([`intent_flag::QTY`]).
    pub qty_1e6: i64,
    /// Percent ×1e6, `0..=100` ([`intent_flag::QTY_PCT`]).
    pub qty_pct_1e6: i64,
    /// Absolute limit price ×1e6.
    pub limit_1e6: i64,
    /// Absolute stop price ×1e6.
    pub stop_1e6: i64,
    /// Take-profit distance, ticks ×1e6.
    pub profit_ticks_1e6: i64,
    /// Stop-loss distance, ticks ×1e6.
    pub loss_ticks_1e6: i64,
    /// Trailing activation price ×1e6.
    pub trail_price_1e6: i64,
    /// Trailing activation distance, ticks ×1e6.
    pub trail_points_1e6: i64,
    /// Trailing offset, ticks ×1e6.
    pub trail_offset_1e6: i64,
    /// The id's `Sym` when it was a constant; NONE when dynamic. Display only.
    pub id_sym: Sym,
    /// The comment's `Sym` when constant; NONE otherwise. Display only.
    pub comment_sym: Sym,
    /// Emission order within the evaluation — the deterministic tie-break.
    pub seq: u32,
    /// Always zero.
    pub _pad1: [u8; 12],
}

/// [`TradeRec::flags`] bits.
pub mod trade_flag {
    /// The trade is open: `exit_*` are 0 / `u32::MAX`, and `pnl_1e12` is the
    /// open profit at the last mark.
    pub const OPEN: u32 = 1 << 0;
    /// `pnl_1e12` is net of `fee_1e12`. Every v4 producer sets it; a consumer
    /// refuses a record without it.
    pub const PNL_NET: u32 = 1 << 1;
    /// Closed by the broker's margin call.
    pub const LIQUIDATION: u32 = 1 << 2;
    /// Closed by a `strategy.risk.*` trip.
    pub const RISK_FLATTEN: u32 = 1 << 3;
}

/// One trade — TradingView's "List of Trades" row. 128 bytes. What the
/// broker emits, what the conformance fixtures and the PVM-8g determinism
/// gate compare, and what `strategy.opentrades.*` / `closedtrades.*` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C, align(64))]
pub struct TradeRec {
    /// The entry order's id key.
    pub entry_id: StrKey,
    /// The exit order's id key; NONE while open.
    pub exit_id: StrKey,
    /// Entry time, UTC ms (the bar's time in TV-equivalent mode, the fill's
    /// event time in native mode).
    pub entry_ms: i64,
    /// Exit time, UTC ms; 0 while open.
    pub exit_ms: i64,
    /// Entry price ×1e6 (fill price, slippage included).
    pub entry_px_1e6: i64,
    /// Exit price ×1e6; 0 while open.
    pub exit_px_1e6: i64,
    /// Signed contracts ×1e6: positive long, negative short.
    pub qty_1e6: i64,
    /// Entry bar index.
    pub entry_bar: u32,
    /// Exit bar index; `u32::MAX` while open.
    pub exit_bar: u32,
    /// Profit ×1e12, **net of `fee_1e12`** ([`trade_flag::PNL_NET`]). Open
    /// trades: open profit at the last mark, net of the entry fee.
    pub pnl_1e12: i128,
    /// Commission charged on entry and exit ×1e12, ≥ 0.
    pub fee_1e12: i128,
    /// Max run-up while open, USD ×1e6 (report domain).
    pub max_runup_1e6: i64,
    /// Max drawdown while open, USD ×1e6 (report domain), ≥ 0.
    pub max_drawdown_1e6: i64,
    /// [`trade_flag`] bits.
    pub flags: u32,
    /// Entry id `Sym` when constant (display).
    pub entry_sym: Sym,
    /// Exit id `Sym` when constant (display).
    pub exit_sym: Sym,
    /// Always zero.
    pub _pad: u32,
}

/// The broker's scalar read-back: everything a `strategy.*` variable reads.
/// 256 bytes. Updated by the broker from **fills only** (LAW E-5 — the ack
/// is not the fill); lent to builtins through `BarCtx::broker`.
///
/// Money is ×1e12 (D18); the builtin converts to the f64 the script sees.
/// Averages and percentages (`avg_trade`, `netprofit_percent`, …) are
/// derived by the builtins from these fields and the trade lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C, align(64))]
pub struct BrokerView {
    /// `strategy.position_size` — signed contracts ×1e6.
    pub position_size_1e6: i64,
    /// `strategy.position_avg_price` ×1e6; 0 when flat (the builtin yields na).
    pub position_avg_price_1e6: i64,
    /// `strategy.position_entry_name` — key of the first open entry; NONE flat.
    pub position_entry_name: StrKey,
    /// `strategy.margin_liquidation_price` ×1e6; 0 = none (na).
    pub margin_liquidation_price_1e6: i64,
    /// `strategy.max_contracts_held_all` ×1e6.
    pub max_contracts_held_all_1e6: i64,
    /// `strategy.max_contracts_held_long` ×1e6.
    pub max_contracts_held_long_1e6: i64,
    /// `strategy.max_contracts_held_short` ×1e6.
    pub max_contracts_held_short_1e6: i64,
    /// `strategy.opentrades`.
    pub opentrades: u32,
    /// `strategy.closedtrades` (all ever closed, not just retained).
    pub closedtrades: u32,
    /// `strategy.wintrades`.
    pub wintrades: u32,
    /// `strategy.losstrades`.
    pub losstrades: u32,
    /// `strategy.eventrades`.
    pub eventrades: u32,
    /// Closed trades still retained for `closedtrades.*(i)`;
    /// `strategy.closedtrades.first_index == closedtrades - closed_retained`.
    pub closed_retained: u32,
    /// `strategy.equity` ×1e12.
    pub equity_1e12: i128,
    /// `strategy.netprofit` ×1e12.
    pub netprofit_1e12: i128,
    /// `strategy.openprofit` ×1e12.
    pub openprofit_1e12: i128,
    /// `strategy.grossprofit` ×1e12.
    pub grossprofit_1e12: i128,
    /// `strategy.grossloss` ×1e12 (≥ 0).
    pub grossloss_1e12: i128,
    /// `strategy.initial_capital` ×1e12.
    pub initial_capital_1e12: i128,
    /// `strategy.max_drawdown` ×1e12 (≥ 0).
    pub max_drawdown_1e12: i128,
    /// `strategy.max_runup` ×1e12 (≥ 0).
    pub max_runup_1e12: i128,
    /// A9: `strategy.max_drawdown_percent` ×1e6 (≥ 0) — the largest per-trade
    /// `max_drawdown / (entry_price × |size|) × 100` over every trade, open or
    /// closed, as `closedtrades.max_drawdown_percent` reads each record
    /// (`pin:04-max-dd-pct-provisional`); a zero-base trade adds nothing.
    pub max_drawdown_pct_1e6: i64,
    /// A9: `strategy.max_runup_percent` ×1e6 (≥ 0) — the same over the
    /// per-trade `max_runup` (`pin:04-max-runup-pct-provisional`).
    pub max_runup_pct_1e6: i64,
    /// A9: Σ closed-trade `profit_percent` ×1e6 — `pnl / (entry_price ×
    /// |size|) × 100` per closed record, truncated toward zero, saturating
    /// (`pin:04-avg-trade-pct-provisional`).
    pub sum_trade_pct_1e6: i64,
    /// A9: the same Σ over winning closed trades (`pnl > 0`; ≥ 0).
    pub sum_win_pct_1e6: i64,
    /// A9: the same Σ over losing closed trades (`pnl < 0`; ≤ 0).
    pub sum_loss_pct_1e6: i64,
    /// CAR-R33-1 (r33h): `strategy.netprofit` as TradingView forms it, as `f64::to_bits` (the
    /// view derives `Eq`) — the f64 running sum, in close order, of every closed slice's
    /// script-visible profit, less the open trades' paid entry fees. Script-visible only: the
    /// broker's money stays `netprofit_1e12`. `0` (the default) reads `0.0`.
    pub netprofit_f64_bits: u64,
}

const _: () = assert!(core::mem::size_of::<IntentKind>() == 1);
const _: () = assert!(core::mem::size_of::<IntentSide>() == 1);
const _: () = assert!(core::mem::size_of::<OcaType>() == 1);
const _: () = assert!(core::mem::size_of::<OrderIntent>() == 128);
const _: () = assert!(core::mem::align_of::<OrderIntent>() == 64);
const _: () = assert!(core::mem::offset_of!(OrderIntent, id) == 8);
const _: () = assert!(core::mem::offset_of!(OrderIntent, qty_1e6) == 32);
const _: () = assert!(core::mem::offset_of!(OrderIntent, id_sym) == 104);
const _: () = assert!(core::mem::size_of::<TradeRec>() == 128);
const _: () = assert!(core::mem::align_of::<TradeRec>() == 64);
const _: () = assert!(core::mem::offset_of!(TradeRec, pnl_1e12) == 64);
const _: () = assert!(core::mem::size_of::<BrokerView>() == 256);
const _: () = assert!(core::mem::align_of::<BrokerView>() == 64);
const _: () = assert!(core::mem::offset_of!(BrokerView, equity_1e12) == 80);
// CAR-R33-1: the former `_pad: [u64; 1]` slot; nothing else moves.
const _: () = assert!(core::mem::offset_of!(BrokerView, sum_loss_pct_1e6) == 240);
const _: () = assert!(core::mem::offset_of!(BrokerView, netprofit_f64_bits) == 248);
