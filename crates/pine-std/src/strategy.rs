// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `strategy.*` (impl-04 §4.15; contract §3.12, §3.15.3; W21, W23): the order functions, each
//! one `OrderIntent` (T28, I21, I35), and the variables and trade accessors that read the broker
//! back (T29, I22) — the one place ×1e6 / ×1e12 become the f64 a script sees.
//!
//! Window positions are found by parameter **name** in pine-spec at compile time
//! ([`Pos::of`]), per dialect: `when` is a `versions: [5]` parameter (MG-10), so a v6 window does
//! not carry it. A spec edit that renames or drops a parameter fails the build.

use crate::sw;
use pine_ir::{
    intent_flag as fl, scale, str_key, str_region, trade_flag, Dialect, FaultKind, IntentKind,
    IntentSide, OcaType, OrderIntent, OverloadId, SiteId, StrKey, StrRef, Sym, TradeRec,
};
use pine_spec::{ov, pf, swr, Sw, OVERLOADS, PARAMS};
use pine_vm::Frame;

// ---- window positions (compile time) ------------------------------------------------------

const fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Window position of parameter `name` of `id` in dialect `d` (synthetic parameters count;
/// parameters absent from `d` do not), or `NONE`.
const fn pos_in(id: OverloadId, name: &str, d: Dialect) -> u8 {
    let o = &OVERLOADS[id.0 as usize];
    let mut p = o.p_lo as usize;
    let mut at = 0u8;
    while p < o.p_hi as usize {
        let r = &PARAMS[p];
        if r.versions.has(d) {
            if eq(r.name.as_bytes(), name.as_bytes()) {
                return at;
            }
            at += 1;
        }
        p += 1;
    }
    Pos::NONE
}

/// Number of window inputs of `id` in `d` (the strategy functions have `n_out == 0`).
const fn n_in(id: OverloadId, d: Dialect) -> u8 {
    let o = &OVERLOADS[id.0 as usize];
    assert!(o.n_out == 0, "a strategy function has no output");
    let mut p = o.p_lo as usize;
    let mut n = 0u8;
    while p < o.p_hi as usize {
        let r = &PARAMS[p];
        assert!(
            r.flags & pf::VARIADIC == 0,
            "no variadic strategy parameter"
        );
        if r.versions.has(d) {
            n += 1;
        }
        p += 1;
    }
    n
}

/// A parameter's position in both dialects' windows, `[v5, v6]`.
#[derive(Clone, Copy)]
struct Pos([u8; 2]);

impl Pos {
    const NONE: u8 = u8::MAX;

    /// A parameter both dialects carry.
    const fn of(id: OverloadId, name: &str) -> Pos {
        let p = Pos([pos_in(id, name, Dialect::V5), pos_in(id, name, Dialect::V6)]);
        assert!(
            p.0[0] != Pos::NONE && p.0[1] != Pos::NONE,
            "parameter missing"
        );
        p
    }

    /// `when`: v5 only (MG-10).
    const fn when(id: OverloadId) -> Pos {
        let p = Pos([
            pos_in(id, "when", Dialect::V5),
            pos_in(id, "when", Dialect::V6),
        ]);
        assert!(
            p.0[0] != Pos::NONE && p.0[1] == Pos::NONE,
            "`when` is v5-only"
        );
        p
    }

    #[inline(always)]
    fn at(self, d: Dialect) -> usize {
        sw::pick(self.0, d) as usize
    }
}

/// The window length of `id` in both dialects.
const fn win_len(id: OverloadId) -> [u8; 2] {
    [n_in(id, Dialect::V5), n_in(id, Dialect::V6)]
}

/// Check the window length, then evaluate v5's `when`: `Ok(false)` = no order this evaluation.
#[inline(always)]
fn open(f: &Frame<'_>, args: &[u16], len: [u8; 2], when: Pos) -> Result<bool, FaultKind> {
    let d = f.dialect();
    if args.len() != sw::pick(len, d) as usize {
        return Err(FaultKind::Internal);
    }
    if d == Dialect::V5 && f.bool_arg(args[when.at(d)]) != Some(true) {
        return Ok(false); // `when` false or na (§4.15.1)
    }
    Ok(true)
}

// ---- shared pieces (§3.10) ----------------------------------------------------------------

/// An id / from_entry / OCA name as a key; na → NONE, and `""` → NONE where "none" is meant.
#[inline(always)]
fn key_of(f: &Frame<'_>, e: u16, empty_is_none: bool) -> StrKey {
    match f.str_arg(e) {
        None => StrKey::NONE,
        Some(b) if b.is_empty() && empty_is_none => StrKey::NONE,
        Some(b) => str_key(b),
    }
}

/// The `Sym` of a `CONST`-region string that is exactly an interned symbol (display only).
fn const_sym(f: &Frame<'_>, e: u16) -> Option<Sym> {
    let r = StrRef(f.i64_arg(e)? as u64);
    if r.region() != str_region::CONST {
        return None;
    }
    let syms = f.syms();
    let s = core::str::from_utf8(syms.str_at(r.off(), r.len())).ok()?;
    let sym = syms.get(s)?;
    if syms.span_of(sym) == (r.off(), r.len()) {
        Some(sym)
    } else {
        None
    }
}

/// The required order id: na → `Err(ScriptRefused)`; sets `id`, and `id_sym` or `ID_DYNAMIC`.
#[inline(always)]
fn set_id(f: &Frame<'_>, e: u16, it: &mut OrderIntent) -> Result<(), FaultKind> {
    if f.str_arg(e).is_none() {
        return Err(FaultKind::ScriptRefused); // an order needs an id
    }
    it.id = key_of(f, e, false);
    match const_sym(f, e) {
        Some(s) if s != Sym::NONE => it.id_sym = s,
        _ => it.flags |= fl::ID_DYNAMIC,
    }
    Ok(())
}

/// A float argument through the one f64 → ×1e6 crossing; NaN → absent (flag clear, field 0).
#[inline(always)]
fn put(f: &Frame<'_>, e: u16, field: &mut i64, flags: &mut u16, bit: u16) {
    if let Some(v) = scale::to_1e6(f.f64_arg(e)) {
        *field = v;
        *flags |= bit;
    }
}

/// A constant comment's `Sym` (display only); anything else leaves `comment_sym` NONE.
#[inline(always)]
fn set_comment(f: &Frame<'_>, e: u16, it: &mut OrderIntent) {
    if let Some(s) = const_sym(f, e) {
        it.comment_sym = s;
    }
}

/// `immediately` (a bool) → `IMMEDIATELY`.
#[inline(always)]
fn set_immediately(f: &Frame<'_>, e: u16, it: &mut OrderIntent) {
    if f.bool_arg(e) == Some(true) {
        it.flags |= fl::IMMEDIATELY;
    }
}

/// The OCA fields travel together (impl-05 §4.4.1): either both set or both cleared.
#[inline(always)]
fn set_oca(it: &mut OrderIntent, name: StrKey, ty: OcaType) {
    if name == StrKey::NONE || ty == OcaType::None {
        it.oca_name = StrKey::NONE;
        it.oca_type = OcaType::None;
    } else {
        it.oca_name = name;
        it.oca_type = ty;
    }
}

// ---- entry / order ------------------------------------------------------------------------

macro_rules! entry_like {
    ($name:ident, $id:expr, $kind:expr, $doc:literal) => {
        #[doc = $doc]
        pub fn $name(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
            const LEN: [u8; 2] = win_len($id);
            const WHEN: Pos = Pos::when($id);
            const ID: Pos = Pos::of($id, "id");
            const DIR: Pos = Pos::of($id, "direction");
            const QTY: Pos = Pos::of($id, "qty");
            const LIMIT: Pos = Pos::of($id, "limit");
            const STOP: Pos = Pos::of($id, "stop");
            const OCA_NAME: Pos = Pos::of($id, "oca_name");
            const OCA_TYPE: Pos = Pos::of($id, "oca_type");
            const COMMENT: Pos = Pos::of($id, "comment");
            if !open(f, args, LEN, WHEN)? {
                return Ok(());
            }
            let d = f.dialect();
            let mut it = OrderIntent {
                kind: $kind,
                ..OrderIntent::default()
            };
            set_id(f, args[ID.at(d)], &mut it)?;
            // `strategy.long` / `strategy.short` are int 0 / 1 (§3.4).
            if f.i64_arg(args[DIR.at(d)]) == Some(1) {
                it.side = IntentSide::Short;
            }
            put(f, args[QTY.at(d)], &mut it.qty_1e6, &mut it.flags, fl::QTY);
            put(
                f,
                args[LIMIT.at(d)],
                &mut it.limit_1e6,
                &mut it.flags,
                fl::LIMIT,
            );
            put(
                f,
                args[STOP.at(d)],
                &mut it.stop_1e6,
                &mut it.flags,
                fl::STOP,
            );
            let name = key_of(f, args[OCA_NAME.at(d)], true);
            // `strategy.oca.none` / `cancel` / `reduce` are int 0 / 1 / 2 (§3.4).
            let ty = match f.i64_arg(args[OCA_TYPE.at(d)]) {
                Some(1) => OcaType::Cancel,
                Some(2) => OcaType::Reduce,
                _ => OcaType::None,
            };
            set_oca(&mut it, name, ty);
            set_comment(f, args[COMMENT.at(d)], &mut it);
            f.push_intent(&it) // PoolFull at intent_cap; the VM stamps seq
        }
    };
}

entry_like!(
    bi_entry,
    ov::STRATEGY_ENTRY,
    IntentKind::Entry,
    "`strategy.entry` → one `Entry` intent (§3.10)."
);
entry_like!(
    bi_order,
    ov::STRATEGY_ORDER,
    IntentKind::Order,
    "`strategy.order` → one `Order` intent."
);

// ---- exit ---------------------------------------------------------------------------------

const EXIT_PAIRS: [u8; 2] = sw::both(Sw::ExitPairs);

/// `strategy.exit` → one `Exit` intent. Distances (`profit`, `loss`, `trail_points`,
/// `trail_offset`) are ticks ×1e6, never converted here (W23). Under `exit_pairs =
/// absolute_wins` (v5) the relative leg of a pair given with its absolute partner is dropped
/// (`pin:W23-exit-legs`); under `both` (v6) both go. The OCA name travels with `oca_type =
/// Reduce` (exit has no type parameter; `pin:04-strategy-edges`).
pub fn bi_exit(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const ID_: OverloadId = ov::STRATEGY_EXIT;
    const LEN: [u8; 2] = win_len(ID_);
    const WHEN: Pos = Pos::when(ID_);
    const ID: Pos = Pos::of(ID_, "id");
    const FROM: Pos = Pos::of(ID_, "from_entry");
    const QTY: Pos = Pos::of(ID_, "qty");
    const QTY_PCT: Pos = Pos::of(ID_, "qty_percent");
    const PROFIT: Pos = Pos::of(ID_, "profit");
    const LIMIT: Pos = Pos::of(ID_, "limit");
    const LOSS: Pos = Pos::of(ID_, "loss");
    const STOP: Pos = Pos::of(ID_, "stop");
    const TRAIL_PRICE: Pos = Pos::of(ID_, "trail_price");
    const TRAIL_POINTS: Pos = Pos::of(ID_, "trail_points");
    const TRAIL_OFFSET: Pos = Pos::of(ID_, "trail_offset");
    const OCA_NAME: Pos = Pos::of(ID_, "oca_name");
    const COMMENT: Pos = Pos::of(ID_, "comment");
    if !open(f, args, LEN, WHEN)? {
        return Ok(());
    }
    let d = f.dialect();
    let mut it = OrderIntent {
        kind: IntentKind::Exit,
        ..OrderIntent::default()
    };
    set_id(f, args[ID.at(d)], &mut it)?;
    it.from_entry = key_of(f, args[FROM.at(d)], true); // ""/na → every entry
    let fl_ = &mut it.flags;
    put(f, args[QTY.at(d)], &mut it.qty_1e6, fl_, fl::QTY);
    put(
        f,
        args[QTY_PCT.at(d)],
        &mut it.qty_pct_1e6,
        fl_,
        fl::QTY_PCT,
    );
    put(
        f,
        args[PROFIT.at(d)],
        &mut it.profit_ticks_1e6,
        fl_,
        fl::PROFIT,
    );
    put(f, args[LIMIT.at(d)], &mut it.limit_1e6, fl_, fl::LIMIT);
    put(f, args[LOSS.at(d)], &mut it.loss_ticks_1e6, fl_, fl::LOSS);
    put(f, args[STOP.at(d)], &mut it.stop_1e6, fl_, fl::STOP);
    put(
        f,
        args[TRAIL_PRICE.at(d)],
        &mut it.trail_price_1e6,
        fl_,
        fl::TRAIL_PRICE,
    );
    put(
        f,
        args[TRAIL_POINTS.at(d)],
        &mut it.trail_points_1e6,
        fl_,
        fl::TRAIL_POINTS,
    );
    put(
        f,
        args[TRAIL_OFFSET.at(d)],
        &mut it.trail_offset_1e6,
        fl_,
        fl::TRAIL_OFFSET,
    );
    if sw::pick(EXIT_PAIRS, d) == swr::EXIT_PAIRS_ABSOLUTE_WINS {
        drop_leg(
            &mut it.flags,
            &mut it.profit_ticks_1e6,
            fl::PROFIT,
            fl::LIMIT,
        );
        drop_leg(&mut it.flags, &mut it.loss_ticks_1e6, fl::LOSS, fl::STOP);
        drop_leg(
            &mut it.flags,
            &mut it.trail_points_1e6,
            fl::TRAIL_POINTS,
            fl::TRAIL_PRICE,
        );
    }
    let name = key_of(f, args[OCA_NAME.at(d)], true);
    set_oca(&mut it, name, OcaType::Reduce);
    set_comment(f, args[COMMENT.at(d)], &mut it);
    f.push_intent(&it)
}

/// W23 `absolute_wins`: the relative leg `rel` given together with its absolute partner `abs`
/// is dropped — flag cleared, field zero.
#[inline(always)]
fn drop_leg(flags: &mut u16, field: &mut i64, rel: u16, abs: u16) {
    if *flags & rel != 0 && *flags & abs != 0 {
        *flags &= !rel;
        *field = 0;
    }
}

// ---- close / close_all / cancel / cancel_all ----------------------------------------------

/// `strategy.close(id, …)` → one `Close` intent: `id` and `from_entry` are both the entry's key.
pub fn bi_close(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const ID_: OverloadId = ov::STRATEGY_CLOSE;
    const LEN: [u8; 2] = win_len(ID_);
    const WHEN: Pos = Pos::when(ID_);
    const ID: Pos = Pos::of(ID_, "id");
    const COMMENT: Pos = Pos::of(ID_, "comment");
    const QTY: Pos = Pos::of(ID_, "qty");
    const QTY_PCT: Pos = Pos::of(ID_, "qty_percent");
    const IMM: Pos = Pos::of(ID_, "immediately");
    if !open(f, args, LEN, WHEN)? {
        return Ok(());
    }
    let d = f.dialect();
    let mut it = OrderIntent {
        kind: IntentKind::Close,
        ..OrderIntent::default()
    };
    set_id(f, args[ID.at(d)], &mut it)?;
    it.from_entry = it.id;
    put(f, args[QTY.at(d)], &mut it.qty_1e6, &mut it.flags, fl::QTY);
    put(
        f,
        args[QTY_PCT.at(d)],
        &mut it.qty_pct_1e6,
        &mut it.flags,
        fl::QTY_PCT,
    );
    set_immediately(f, args[IMM.at(d)], &mut it);
    set_comment(f, args[COMMENT.at(d)], &mut it);
    f.push_intent(&it)
}

/// `strategy.close_all(…)` → one `CloseAll` intent.
pub fn bi_close_all(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const ID_: OverloadId = ov::STRATEGY_CLOSE_ALL;
    const LEN: [u8; 2] = win_len(ID_);
    const WHEN: Pos = Pos::when(ID_);
    const COMMENT: Pos = Pos::of(ID_, "comment");
    const IMM: Pos = Pos::of(ID_, "immediately");
    if !open(f, args, LEN, WHEN)? {
        return Ok(());
    }
    let d = f.dialect();
    let mut it = OrderIntent {
        kind: IntentKind::CloseAll,
        ..OrderIntent::default()
    };
    set_immediately(f, args[IMM.at(d)], &mut it);
    set_comment(f, args[COMMENT.at(d)], &mut it);
    f.push_intent(&it)
}

/// `strategy.cancel(id)` → one `Cancel` intent.
pub fn bi_cancel(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const ID_: OverloadId = ov::STRATEGY_CANCEL;
    const LEN: [u8; 2] = win_len(ID_);
    const WHEN: Pos = Pos::when(ID_);
    const ID: Pos = Pos::of(ID_, "id");
    if !open(f, args, LEN, WHEN)? {
        return Ok(());
    }
    let d = f.dialect();
    let mut it = OrderIntent {
        kind: IntentKind::Cancel,
        ..OrderIntent::default()
    };
    set_id(f, args[ID.at(d)], &mut it)?;
    f.push_intent(&it)
}

/// `strategy.cancel_all()` → one `CancelAll` intent.
pub fn bi_cancel_all(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const ID_: OverloadId = ov::STRATEGY_CANCEL_ALL;
    const LEN: [u8; 2] = win_len(ID_);
    const WHEN: Pos = Pos::when(ID_);
    if !open(f, args, LEN, WHEN)? {
        return Ok(());
    }
    let it = OrderIntent {
        kind: IntentKind::CancelAll,
        ..OrderIntent::default()
    };
    f.push_intent(&it)
}

// ---- variables (§4.15.2) ------------------------------------------------------------------

/// Which `BrokerView` field a variable reads, and how it converts.
#[derive(Clone, Copy)]
pub enum Var {
    /// `position_size` (signed, ×1e6).
    PositionSize,
    /// `position_avg_price` (×1e6; 0 → na).
    PositionAvgPrice,
    /// `equity` (×1e12).
    Equity,
    /// `netprofit` (×1e12).
    NetProfit,
    /// `openprofit` (×1e12).
    OpenProfit,
    /// `grossprofit` (×1e12).
    GrossProfit,
    /// `grossloss` (×1e12).
    GrossLoss,
    /// `initial_capital` (×1e12).
    InitialCapital,
    /// `max_drawdown` (×1e12).
    MaxDrawdown,
    /// `max_runup` (×1e12).
    MaxRunup,
    /// `max_contracts_held_all` (×1e6).
    HeldAll,
    /// `max_contracts_held_long` (×1e6).
    HeldLong,
    /// `max_contracts_held_short` (×1e6).
    HeldShort,
    /// `margin_liquidation_price` (×1e6; 0 → na).
    LiqPrice,
    /// `opentrades` (int).
    OpenTrades,
    /// `closedtrades` (int).
    ClosedTrades,
    /// `wintrades` (int).
    WinTrades,
    /// `losstrades` (int).
    LossTrades,
    /// `eventrades` (int).
    EvenTrades,
    /// `closedtrades.first_index` = `closedtrades − closed_retained` (int).
    FirstIndex,
    /// `netprofit_percent` = `netprofit / initial_capital × 100` ("the total value of all
    /// completed trades, expressed as a percentage of the initial capital").
    NetProfitPct,
    /// `grossprofit_percent` = `grossprofit / initial_capital × 100`.
    GrossProfitPct,
    /// `grossloss_percent` = `grossloss / initial_capital × 100` (`grossloss ≥ 0`).
    GrossLossPct,
    /// `avg_trade` = `netprofit / closedtrades` ("the sum of all profits and losses divided by
    /// the number of closed trades").
    AvgTrade,
    /// `avg_winning_trade` = `grossprofit / wintrades`.
    AvgWinTrade,
    /// `avg_losing_trade` = `grossloss / losstrades` (a positive amount, as `grossloss`).
    AvgLossTrade,
    /// `openprofit_percent` = `openprofit / (initial_capital + netprofit) × 100` — the
    /// reference's "`openPL / realizedEquity * 100`", realized equity = initial capital plus the
    /// closed trades' net profit (`pin:04-openprofit-pct-provisional`, operator ruling 2026-10-16).
    OpenProfitPct,
    /// A9 `max_drawdown_percent` = `BrokerView::max_drawdown_pct_1e6` — the reference's
    /// per-trade `Lowest Value During Trade / (Entry Price x Quantity) * 100`, the largest over
    /// every trade, open or closed (`pin:04-max-dd-pct-provisional`); na before the first trade.
    MaxDrawdownPct,
    /// A9 `max_runup_percent` = `BrokerView::max_runup_pct_1e6` — the same over
    /// `Highest Value During Trade` (`pin:04-max-runup-pct-provisional`); na before the first trade.
    MaxRunupPct,
    /// A9 `avg_trade_percent` = Σ closed `profit_percent` / `closedtrades` ("the sum of all profit
    /// and loss percentages divided by the number of closed trades"; `÷0 → na`;
    /// `pin:04-avg-trade-pct-provisional`).
    AvgTradePct,
    /// A9 `avg_winning_trade_percent` = Σ winning `profit_percent` / `wintrades` (`÷0 → na`).
    AvgWinTradePct,
    /// A9 `avg_losing_trade_percent` = −Σ losing `profit_percent` / `losstrades` — a positive
    /// amount, as `avg_losing_trade` (`÷0 → na`; `pin:04-avg-trade-pct-provisional`).
    AvgLossTradePct,
}

/// What a variable read yields before it is written.
enum Val {
    F(f64),
    I(Option<i64>),
}

/// The single output of an `n_out == 1` read row.
#[inline(always)]
fn out_of(args: &[u16], n_in: usize) -> Result<u16, FaultKind> {
    if args.len() != n_in + 1 {
        return Err(FaultKind::Internal);
    }
    Ok(args[n_in])
}

#[inline(always)]
fn write(f: &mut Frame<'_>, e: u16, v: Val) {
    match v {
        Val::F(x) => f.set_f64_arg(e, x),
        Val::I(x) => f.set_i64_arg(e, x),
    }
}

#[inline(always)]
fn nz_px(v: i64) -> f64 {
    if v == 0 {
        f64::NAN
    } else {
        scale::from_1e6(v)
    }
}

/// A `strategy.*` variable: `None` from `broker()` (an indicator) → na.
pub fn bi_var<const V: u8>(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let e = out_of(args, 0)?;
    let int_out = pine_ir::argwin::is_i64(e);
    let v = match f.ctx().broker() {
        None if int_out => Val::I(None),
        None => Val::F(f64::NAN),
        Some(b) => read_var(b, V),
    };
    write(f, e, v);
    Ok(())
}

fn read_var(b: &pine_ir::BrokerView, v: u8) -> Val {
    const PS: u8 = Var::PositionSize as u8;
    const PAP: u8 = Var::PositionAvgPrice as u8;
    const EQ: u8 = Var::Equity as u8;
    const NP: u8 = Var::NetProfit as u8;
    const OP: u8 = Var::OpenProfit as u8;
    const GP: u8 = Var::GrossProfit as u8;
    const GL: u8 = Var::GrossLoss as u8;
    const IC: u8 = Var::InitialCapital as u8;
    const MDD: u8 = Var::MaxDrawdown as u8;
    const MRU: u8 = Var::MaxRunup as u8;
    const HA: u8 = Var::HeldAll as u8;
    const HL: u8 = Var::HeldLong as u8;
    const HS: u8 = Var::HeldShort as u8;
    const LP: u8 = Var::LiqPrice as u8;
    const OT: u8 = Var::OpenTrades as u8;
    const CT: u8 = Var::ClosedTrades as u8;
    const WT: u8 = Var::WinTrades as u8;
    const LT: u8 = Var::LossTrades as u8;
    const ET: u8 = Var::EvenTrades as u8;
    const FI: u8 = Var::FirstIndex as u8;
    const NPP: u8 = Var::NetProfitPct as u8;
    const GPP: u8 = Var::GrossProfitPct as u8;
    const GLP: u8 = Var::GrossLossPct as u8;
    const AT: u8 = Var::AvgTrade as u8;
    const AW: u8 = Var::AvgWinTrade as u8;
    const AL: u8 = Var::AvgLossTrade as u8;
    const OPP: u8 = Var::OpenProfitPct as u8;
    const MDP: u8 = Var::MaxDrawdownPct as u8;
    const MRP: u8 = Var::MaxRunupPct as u8;
    const ATP: u8 = Var::AvgTradePct as u8;
    const AWP: u8 = Var::AvgWinTradePct as u8;
    const ALP: u8 = Var::AvgLossTradePct as u8;
    // A9: no trade yet (none open, none ever closed) → the per-trade maxima read na.
    let traded = b.opentrades != 0 || b.closedtrades != 0;
    let ic = scale::from_1e12(b.initial_capital_1e12);
    match v {
        PS => Val::F(scale::from_1e6(b.position_size_1e6)),
        PAP => Val::F(nz_px(b.position_avg_price_1e6)),
        EQ => Val::F(scale::from_1e12(b.equity_1e12)),
        NP => Val::F(scale::from_1e12(b.netprofit_1e12)),
        OP => Val::F(scale::from_1e12(b.openprofit_1e12)),
        GP => Val::F(scale::from_1e12(b.grossprofit_1e12)),
        GL => Val::F(scale::from_1e12(b.grossloss_1e12)),
        IC => Val::F(scale::from_1e12(b.initial_capital_1e12)),
        MDD => Val::F(scale::from_1e12(b.max_drawdown_1e12)),
        MRU => Val::F(scale::from_1e12(b.max_runup_1e12)),
        HA => Val::F(scale::from_1e6(b.max_contracts_held_all_1e6)),
        HL => Val::F(scale::from_1e6(b.max_contracts_held_long_1e6)),
        HS => Val::F(scale::from_1e6(b.max_contracts_held_short_1e6)),
        LP => Val::F(nz_px(b.margin_liquidation_price_1e6)),
        OT => Val::I(Some(b.opentrades as i64)),
        CT => Val::I(Some(b.closedtrades as i64)),
        WT => Val::I(Some(b.wintrades as i64)),
        LT => Val::I(Some(b.losstrades as i64)),
        ET => Val::I(Some(b.eventrades as i64)),
        FI => Val::I(Some(first_index(b) as i64)),
        NPP => Val::F(pct(scale::from_1e12(b.netprofit_1e12), ic)),
        GPP => Val::F(pct(scale::from_1e12(b.grossprofit_1e12), ic)),
        GLP => Val::F(pct(scale::from_1e12(b.grossloss_1e12), ic)),
        AT => Val::F(div_na(
            scale::from_1e12(b.netprofit_1e12),
            b.closedtrades as f64,
        )),
        AW => Val::F(div_na(
            scale::from_1e12(b.grossprofit_1e12),
            b.wintrades as f64,
        )),
        AL => Val::F(div_na(
            scale::from_1e12(b.grossloss_1e12),
            b.losstrades as f64,
        )),
        OPP => Val::F(pct(
            scale::from_1e12(b.openprofit_1e12),
            scale::from_1e12(b.initial_capital_1e12 + b.netprofit_1e12),
        )),
        MDP if traded => Val::F(scale::from_1e6(b.max_drawdown_pct_1e6)),
        MRP if traded => Val::F(scale::from_1e6(b.max_runup_pct_1e6)),
        ATP => Val::F(div_na(
            scale::from_1e6(b.sum_trade_pct_1e6),
            b.closedtrades as f64,
        )),
        AWP => Val::F(div_na(
            scale::from_1e6(b.sum_win_pct_1e6),
            b.wintrades as f64,
        )),
        ALP => Val::F(div_na(
            scale::from_1e6(b.sum_loss_pct_1e6.saturating_neg()),
            b.losstrades as f64,
        )),
        _ => Val::F(f64::NAN),
    }
}

/// A derived variable's quotient in f64 (§4.15.2): `÷0 → na` (`pin:04-strategy-edges`).
#[inline(always)]
fn div_na(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        f64::NAN
    } else {
        a / b
    }
}

/// `a` as a percentage of `base`: `a / base × 100`, `÷0 → na`.
#[inline(always)]
fn pct(a: f64, base: f64) -> f64 {
    div_na(a, base) * 100.0
}

/// `strategy.closedtrades.first_index` (§4.15.2). A host with `closed_retained > closedtrades`
/// is out of contract; saturating keeps the read defined (`pin:04-strategy-edges`).
#[inline(always)]
fn first_index(b: &pine_ir::BrokerView) -> u32 {
    b.closedtrades.saturating_sub(b.closed_retained)
}

/// The `CONST` `StrRef` of a constant id's `Sym`; `Sym::NONE` (a dynamic id) → na. A `Sym`
/// outside this program's table is a host defect (the broker echoes the intent's own `id_sym`):
/// `Err(Internal)`.
#[inline(always)]
fn sym_ref(f: &Frame<'_>, s: Sym) -> Result<Option<i64>, FaultKind> {
    if s == Sym::NONE {
        return Ok(None);
    }
    if (s.0 as usize) >= f.syms().len() {
        return Err(FaultKind::Internal);
    }
    let (off, len) = f.syms().span_of(s);
    Ok(Some(StrRef::konst(off, len).0 as i64))
}

/// `strategy.position_entry_name`: `open_trade(0).entry_sym` when its `entry_id` is the
/// position's entry name, else na (§4.15.2).
pub fn bi_position_entry_name(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    let e = out_of(args, 0)?;
    let ctx = f.ctx();
    let name = ctx.broker().map_or(StrKey::NONE, |b| b.position_entry_name);
    let sym = match ctx.open_trade(0) {
        Some(t) if name != StrKey::NONE && t.entry_id == name => t.entry_sym,
        _ => Sym::NONE,
    };
    let r = sym_ref(f, sym)?;
    f.set_i64_arg(e, r);
    Ok(())
}

// ---- trade accessors (§4.15.2) ------------------------------------------------------------

/// Which `TradeRec` field a trade accessor reads.
#[derive(Clone, Copy)]
pub enum Fld {
    /// `entry_price` (0 → na).
    EntryPrice,
    /// `exit_price` (0 → na).
    ExitPrice,
    /// `entry_bar_index` (`u32::MAX` → na).
    EntryBar,
    /// `exit_bar_index` (`u32::MAX` → na).
    ExitBar,
    /// `entry_time` (ms; 0 → na).
    EntryTime,
    /// `exit_time` (ms; 0 → na).
    ExitTime,
    /// `size` (signed ×1e6).
    Size,
    /// `profit` (`pnl_1e12`, net of fees).
    Profit,
    /// `commission` (`fee_1e12`).
    Commission,
    /// `max_runup` (USD ×1e6).
    MaxRunup,
    /// `max_drawdown` (USD ×1e6).
    MaxDrawdown,
    /// `entry_id` (the `CONST` string of `entry_sym`; dynamic → na).
    EntryId,
    /// `exit_id` (the `CONST` string of `exit_sym`; dynamic → na).
    ExitId,
    /// `entry_comment` / `exit_comment`: not carried → na (`pin:04-strategy-edges`).
    Comment,
    /// `profit_percent`: `profit / (entry_price × |size|) × 100`, losses negative. The one
    /// definition: `pine-broker` sums the same quotient per closed record (×1e6, truncated) into
    /// `BrokerView::sum_*_pct_1e6` for the `avg_*_percent` variables (A9).
    ProfitPct,
    /// `max_runup_percent`: the reference's `Highest Value During Trade / (Entry Price x
    /// Quantity) * 100` — `max_runup / (entry_price × |size|) × 100`.
    MaxRunupPct,
    /// `max_drawdown_percent`: `Lowest Value During Trade / (Entry Price x Quantity) * 100` —
    /// `max_drawdown / (entry_price × |size|) × 100` (a positive amount, as `max_drawdown`).
    MaxDrawdownPct,
}

/// The trade a call names, read in place: `OPEN` reads `open_trade(i)`; closed maps `k` to the
/// retained window (`closed_trade(k − first_index)`, na when `k < first_index` or
/// `k ≥ closedtrades`). A `TradeRec` without `PNL_NET` is a host defect: `Err(Internal)`. Only
/// the one field crosses out of the borrow (no record copy); a `Sym` is resolved after it ends.
fn trade_read<const OPEN: bool>(
    f: &mut Frame<'_>,
    k: Option<i64>,
    fld: u8,
) -> Result<Raw, FaultKind> {
    let Some(k) = k else { return Ok(Raw::Na) };
    if !(0..=u32::MAX as i64).contains(&k) {
        return Ok(Raw::Na);
    }
    let k = k as u32;
    let ctx = f.ctx();
    let t = if OPEN {
        ctx.open_trade(k)
    } else {
        let Some(b) = ctx.broker() else {
            return Ok(Raw::Na);
        };
        let first = first_index(b);
        if k < first || k >= b.closedtrades {
            return Ok(Raw::Na);
        }
        ctx.closed_trade(k - first)
    };
    match t {
        None => Ok(Raw::Na),
        Some(t) if t.flags & trade_flag::PNL_NET == 0 => Err(FaultKind::Internal),
        Some(t) => Ok(read_fld(t, fld)),
    }
}

/// One field of a trade, before a `Sym` is turned into a string.
enum Raw {
    Na,
    V(Val),
    S(Sym),
}

/// A `strategy.opentrades.*(i)` (`OPEN`) or `strategy.closedtrades.*(k)` accessor.
pub fn bi_trade<const OPEN: bool, const F: u8>(
    f: &mut Frame<'_>,
    _: SiteId,
    args: &[u16],
) -> Result<(), FaultKind> {
    let e = out_of(args, 1)?;
    let k = f.i64_arg(args[0]);
    let v = match trade_read::<OPEN>(f, k, F)? {
        Raw::Na if pine_ir::argwin::is_i64(e) => Val::I(None),
        Raw::Na => Val::F(f64::NAN),
        Raw::V(v) => v,
        Raw::S(s) => Val::I(sym_ref(f, s)?),
    };
    write(f, e, v);
    Ok(())
}

fn read_fld(t: &TradeRec, fld: u8) -> Raw {
    const EP: u8 = Fld::EntryPrice as u8;
    const XP: u8 = Fld::ExitPrice as u8;
    const EB: u8 = Fld::EntryBar as u8;
    const XB: u8 = Fld::ExitBar as u8;
    const ET: u8 = Fld::EntryTime as u8;
    const XT: u8 = Fld::ExitTime as u8;
    const SZ: u8 = Fld::Size as u8;
    const PR: u8 = Fld::Profit as u8;
    const CM: u8 = Fld::Commission as u8;
    const RU: u8 = Fld::MaxRunup as u8;
    const DD: u8 = Fld::MaxDrawdown as u8;
    const EI: u8 = Fld::EntryId as u8;
    const XI: u8 = Fld::ExitId as u8;
    const PP: u8 = Fld::ProfitPct as u8;
    const RP: u8 = Fld::MaxRunupPct as u8;
    const DP: u8 = Fld::MaxDrawdownPct as u8;
    // The percent accessors' base: entry price × |quantity| — the reference's formula taken
    // literally, no point value (`pin:04-trade-pct-provisional`, operator ruling 2026-10-16);
    // a zero base → na.
    let base = scale::from_1e6(t.entry_px_1e6) * scale::from_1e6(t.qty_1e6).abs();
    let bar = |b: u32| Raw::V(Val::I(if b == u32::MAX { None } else { Some(b as i64) }));
    let ms = |m: i64| Raw::V(Val::I(if m == 0 { None } else { Some(m) }));
    match fld {
        EP => Raw::V(Val::F(nz_px(t.entry_px_1e6))),
        XP => Raw::V(Val::F(nz_px(t.exit_px_1e6))),
        EB => bar(t.entry_bar),
        XB => bar(t.exit_bar),
        ET => ms(t.entry_ms),
        XT => ms(t.exit_ms),
        SZ => Raw::V(Val::F(scale::from_1e6(t.qty_1e6))),
        PR => Raw::V(Val::F(scale::from_1e12(t.pnl_1e12))),
        CM => Raw::V(Val::F(scale::from_1e12(t.fee_1e12))),
        RU => Raw::V(Val::F(scale::from_1e6(t.max_runup_1e6))),
        DD => Raw::V(Val::F(scale::from_1e6(t.max_drawdown_1e6))),
        EI => Raw::S(t.entry_sym),
        XI => Raw::S(t.exit_sym),
        PP => Raw::V(Val::F(pct(scale::from_1e12(t.pnl_1e12), base))),
        RP => Raw::V(Val::F(pct(scale::from_1e6(t.max_runup_1e6), base))),
        DP => Raw::V(Val::F(pct(scale::from_1e6(t.max_drawdown_1e6), base))),
        _ => Raw::Na, // comments are not carried
    }
}
