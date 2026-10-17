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
    fnv1a64, fnv1a64_continue, intent_flag as fl, scale, str_key, str_region, trade_flag, BarCtx,
    BrokerView, Dialect, FaultKind, IntentKind, IntentSide, OcaType, OrderIntent, OverloadId,
    SiteId, StrKey, StrRef, Sym, TradeRec,
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

/// `pin:05-exit-signal-text` (CI-032) — the order `strategy.close(id)` places is named
/// `"Close entry(s) order " ++ id` by TradingView: its List of Trades prints it as the trade's exit
/// Signal (E001–E131, every captured `strategy.close` trade).
const CLOSE_ORDER_PREFIX: &[u8] = b"Close entry(s) order ";
/// FNV-1a state after [`CLOSE_ORDER_PREFIX`] (compile time).
const CLOSE_ORDER_SEED: u64 = fnv1a64(CLOSE_ORDER_PREFIX);

/// The key of `CLOSE_ORDER_PREFIX ++ id` without joining the bytes: FNV-1a continues over `id`
/// from the prefix's state (`str_key`'s zero → 1 rule kept) — no allocation on the bar path.
#[inline(always)]
const fn close_order_key(id: &[u8]) -> StrKey {
    let h = fnv1a64_continue(CLOSE_ORDER_SEED, id);
    StrKey(if h == 0 { 1 } else { h })
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

/// `strategy.close(id, …)` → one `Close` intent: `from_entry` is the entry's key, `id` the key of
/// the exit order TradingView places, `Close entry(s) order <id>` (`pin:05-exit-signal-text`).
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
    // `pin:05-exit-signal-text` (CI-032): the broker records this `id` as the closed trade's
    // `exit_id` (impl-05 §4.6.2), so it keys TradingView's order name; the record stays keyed by
    // `from_entry`. `id_sym` (and `ID_DYNAMIC`) stay the script's id: `closedtrades.exit_id` keeps
    // reading it (`pin:04-close-exit-id-read`).
    if let Some(b) = f.str_arg(args[ID.at(d)]) {
        it.id = close_order_key(b);
    }
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
    /// `position_size` (signed, ×1e6; read as [`qty_f64`] forms it).
    PositionSize,
    /// `position_avg_price` (×1e6; 0 → na; read as [`px_f64`] forms it).
    PositionAvgPrice,
    /// `equity` = `(initial_capital + netprofit) + openprofit` in f64 over the reads of
    /// [`Var::NetProfit`] and [`open_profit_f64`] (CAR-R33-1); where the open profit falls back,
    /// `equity_1e12`.
    Equity,
    /// `netprofit` (`BrokerView::netprofit_f64_bits`, CAR-R33-1).
    NetProfit,
    /// `openprofit` (the f64 sum of [`open_profit_f64`], else ×1e12).
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
    /// `max_contracts_held_all` (×1e6; [`qty_f64`]).
    HeldAll,
    /// `max_contracts_held_long` (×1e6; [`qty_f64`]).
    HeldLong,
    /// `max_contracts_held_short` (×1e6; [`qty_f64`]).
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
    /// completed trades, expressed as a percentage of the initial capital"), over the f64
    /// `netprofit` (CAR-R33-1).
    NetProfitPct,
    /// `grossprofit_percent` = `grossprofit / initial_capital × 100`.
    GrossProfitPct,
    /// `grossloss_percent` = `grossloss / initial_capital × 100` (`grossloss ≥ 0`).
    GrossLossPct,
    /// `avg_trade` = `netprofit / closedtrades` ("the sum of all profits and losses divided by
    /// the number of closed trades"), to 10 significant digits ([`round_sig10`]).
    AvgTrade,
    /// `avg_winning_trade` = `grossprofit / wintrades`, to 10 significant digits.
    AvgWinTrade,
    /// `avg_losing_trade` = `grossloss / losstrades` (a positive amount, as `grossloss`), to 10
    /// significant digits.
    AvgLossTrade,
    /// `openprofit_percent` = `openprofit / (initial_capital + netprofit) × 100` — the
    /// reference's "`openPL / realizedEquity * 100`", realized equity = initial capital plus the
    /// closed trades' net profit (`pin:04-openprofit-pct-provisional`, operator ruling 2026-10-16).
    OpenProfitPct,
    /// `max_drawdown_percent` = `BrokerView::max_drawdown_pct_f64_bits` (CAR-R33K-1, r33m) — the
    /// broker's running maximum over its marked path points of `(peak − eq) / peak * 100` in f64,
    /// `peak` the closed-equity peak (`pin:04-max-dd-pct-equity-f64`, E118 4320/4320 rows; A9's
    /// per-trade formula fit 51); 0 before the first trade (`pin:04-max-pct-zero-before-trade`).
    MaxDrawdownPct,
    /// A9 `max_runup_percent` = `BrokerView::max_runup_pct_1e6` — the same over
    /// `Highest Value During Trade` (`pin:04-max-runup-pct-provisional`); 0 before the first
    /// trade.
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

// ---- script-visible float formation (r33f) ------------------------------------------------

/// The chart constants the float formations read (`meta.chart`), copied out before the
/// `BarCtx` borrow.
#[derive(Clone, Copy)]
struct Fmt {
    /// `syminfo.mintick` ×1e6.
    mt_1e6: i64,
    /// `syminfo.pointvalue` ×1e6.
    pv_1e6: i64,
}

impl Fmt {
    #[inline(always)]
    fn of(f: &Frame<'_>) -> Fmt {
        let c = &f.meta().chart;
        Fmt {
            mt_1e6: c.mintick_1e6,
            pv_1e6: c.pointvalue_1e6,
        }
    }
}

/// `pin:04-qty-float-1e8` (CI-058, r33f) — a quantity reads as TradingView forms it: its
/// quantity integer at ×1e8 times `1e-8` in f64, `((q_1e6 · 100) as f64) · 1e-8`, not
/// `q_1e6 / 1e6`. 567/567 distinct captured `position_size` / `opentrades.size` texts of the E
/// fixtures (E045 1543003 → 1.5430030000000001, E054 13 → 0.000013000000000000001, E132 13258776
/// → 13.258776000000001); `q / 1e6` fits 495. `max_contracts_held_*` read the same (uncaptured).
#[inline(always)]
fn qty_f64(q_1e6: i64) -> f64 {
    (q_1e6 as i128 * 100) as f64 * 1e-8
}

/// `pin:04-px-float-ticks` (r33f) — a fill price reads as TradingView forms it: its tick count
/// times the f64 mintick, `(px / mintick) as f64 · mintick`. E082 `position_avg_price`, E107
/// `opentrades.entry_price`, E108 `closedtrades.entry_price` 45/45 distinct values each (75577.4
/// → 75577.40000000001; `px / 1e6` fits 24/45); E108's `closedtrades.profit` fits only with the
/// exit price so formed (45/45 vs 22/45). A price off the tick grid (an average over several
/// fills — uncaptured) keeps `px / 1e6`.
#[inline(always)]
fn px_f64(px_1e6: i64, mt_1e6: i64) -> f64 {
    if mt_1e6 > 0 && px_1e6 % mt_1e6 == 0 {
        (px_1e6 / mt_1e6) as f64 * scale::from_1e6(mt_1e6)
    } else {
        scale::from_1e6(px_1e6)
    }
}

/// A fill price, 0 → na.
#[inline(always)]
fn nz_px(v: i64, mt_1e6: i64) -> f64 {
    if v == 0 {
        f64::NAN
    } else {
        px_f64(v, mt_1e6)
    }
}

/// `pin:04-trade-profit-f64` (r33f) — a closed trade's `profit` is TradingView's f64
/// `(exit_price − entry_price) · size · pointvalue` over the script-visible prices and size
/// (E108 45/45 trades, 4248/4248 rows: −1595.800000000003; `pnl / 1e12` 22/45). Measured on
/// commission-free trades only: a trade that paid a fee keeps `pnl_1e12 / 1e12` (the f64 fee law
/// is not captured — E114's equity fits no tried fee formation on every row). The `pointvalue`
/// factor is last (1 on every capture: its place is unmeasured).
#[inline(always)]
fn profit_f64(t: &TradeRec, x: Fmt) -> f64 {
    if t.flags & trade_flag::OPEN != 0 || t.fee_1e12 != 0 || t.exit_px_1e6 == 0 {
        return scale::from_1e12(t.pnl_1e12);
    }
    (px_f64(t.exit_px_1e6, x.mt_1e6) - px_f64(t.entry_px_1e6, x.mt_1e6))
        * qty_f64(t.qty_1e6)
        * scale::from_1e6(x.pv_1e6)
}

/// `pin:04-openprofit-f64` (r33f) — `strategy.openprofit` is TradingView's f64 sum over the open
/// trades, oldest first, of `(close − entry_price) · size · pointvalue`, gross of the entry fee,
/// prices and sizes as the script reads them, the close formed as ticks × mintick too (E114
/// 4320/4320 rows: −285.3000000000029 at a 0.1 % commission; a `close / 1e6` close 3551/4320,
/// `openprofit_1e12 / 1e12` 1927/4320). The close is the evaluation bar's,
/// which is the broker's mark only at a bar-close evaluation — so the f64 sum is taken only when
/// the same sum in the broker's integers (`gross_1e12`: `sign · trunc(Δpx · |q| · pv / 1e6)`)
/// equals `BrokerView::openprofit_1e12`; any other mark (a `calc_on_order_fills` execution at a
/// fill price) or an overflow keeps `openprofit_1e12 / 1e12` (`None`). Summation order for
/// several open trades is unmeasured (one open trade per captured row).
fn open_profit_f64(ctx: &dyn BarCtx, b: &BrokerView, close_1e6: i64, x: Fmt) -> Option<f64> {
    let close = px_f64(close_1e6, x.mt_1e6);
    let pv = scale::from_1e6(x.pv_1e6);
    let mut sum = 0.0f64;
    let mut chk = 0i128;
    let mut i = 0u32;
    while i < b.opentrades {
        let t = ctx.open_trade(i)?;
        let d = (close_1e6 as i128 - t.entry_px_1e6 as i128)
            .checked_mul((t.qty_1e6 as i128).abs())?
            .checked_mul(x.pv_1e6 as i128)?
            / 1_000_000;
        chk = chk.checked_add(if t.qty_1e6 < 0 { -d } else { d })?;
        sum += (close - px_f64(t.entry_px_1e6, x.mt_1e6)) * qty_f64(t.qty_1e6) * pv;
        i += 1;
    }
    if chk == b.openprofit_1e12 {
        Some(sum)
    } else {
        None
    }
}

/// Powers of ten exact in binary64 (`10^0 ..= 10^22`).
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

#[inline(always)]
fn pow10(k: i32) -> f64 {
    if (0..=22).contains(&k) {
        POW10[k as usize]
    } else {
        10f64.powi(k)
    }
}

/// `pin:04-avg-trade-sig10` (r33f) — TradingView's `avg_trade`, `avg_winning_trade` and
/// `avg_losing_trade` are the quotient rounded to 10 significant digits (E118 4320/4320 rows
/// each, 46 distinct triples: 420.06666666666666 reads 420.0666667, 1155.3333333333333 reads
/// 1155.333333; 12 or 15 digits fit 1427 / 648 rows of `avg_trade`). Rounded as
/// `round(v · 10^p) / 10^p` with `p = 9 − ⌊log₁₀|v|⌋`, half away from zero (the tie rule and a
/// decimal-exact rounding are indistinguishable on the capture). na and ±0 pass through.
#[inline(always)]
fn round_sig10(v: f64) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return v;
    }
    let a = v.abs();
    let mut e = a.log10().floor() as i32;
    // `log10` may land one off next to a power of ten: settle `e` against the exact powers.
    if a >= pow10(e + 1) {
        e += 1;
    } else if a < pow10(e) {
        e -= 1;
    }
    let p = 9 - e;
    if p > 300 {
        return v; // |v| < 1e-291: 10^p overflows — no 10-digit scaling exists (uncaptured)
    }
    if p >= 0 {
        let s = pow10(p);
        (v * s).round() / s
    } else {
        let s = pow10(-p);
        (v / s).round() * s
    }
}

/// A `strategy.*` variable: `None` from `broker()` (an indicator) → na.
pub fn bi_var<const V: u8>(f: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    const OP: u8 = Var::OpenProfit as u8;
    const OPP: u8 = Var::OpenProfitPct as u8;
    const EQ: u8 = Var::Equity as u8;
    let e = out_of(args, 0)?;
    let int_out = pine_ir::argwin::is_i64(e);
    let x = Fmt::of(f);
    let close_1e6 = f.bar().close_1e6;
    let ctx: &dyn BarCtx = f.ctx();
    let v = match ctx.broker() {
        None if int_out => Val::I(None),
        None => Val::F(f64::NAN),
        Some(b) => {
            let open = if V == OP || V == OPP || V == EQ {
                open_profit_f64(ctx, b, close_1e6, x)
            } else {
                None
            };
            read_var(b, V, x, open)
        }
    };
    write(f, e, v);
    Ok(())
}

/// One variable of `b`; `open` is [`open_profit_f64`]'s sum when the read needs it.
fn read_var(b: &BrokerView, v: u8, x: Fmt, open: Option<f64>) -> Val {
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
    let ic = scale::from_1e12(b.initial_capital_1e12);
    let op = open.unwrap_or_else(|| scale::from_1e12(b.openprofit_1e12));
    // CAR-R33-1 (`pin:04-netprofit-f64`, r33h): `strategy.netprofit` is the broker's f64 running
    // sum of the closed slices' script-visible profits, less the open trades' paid entry fees —
    // E086 4320/4320 rows (`netprofit_1e12 / 1e12` 72), E118 `netprofit_percent` 4320/4320 (72).
    let np = f64::from_bits(b.netprofit_f64_bits);
    match v {
        PS => Val::F(qty_f64(b.position_size_1e6)),
        PAP => Val::F(nz_px(b.position_avg_price_1e6, x.mt_1e6)),
        // `pin:04-equity-f64` (r33h): `(initial_capital + netprofit) + openprofit`, in that order,
        // over the f64 reads — E085 4320/4320 rows (`ic + (np + op)` 3697, `equity_1e12` 3396),
        // E114 4320/4320 at a 0.1 % commission (`equity_1e12` 3732). Where the open profit is not
        // the bar close's (`open_profit_f64` → `None`), `equity_1e12 / 1e12` as before.
        EQ => Val::F(match open {
            Some(o) => (ic + np) + o,
            None => scale::from_1e12(b.equity_1e12),
        }),
        NP => Val::F(np),
        OP => Val::F(op),
        GP => Val::F(scale::from_1e12(b.grossprofit_1e12)),
        GL => Val::F(scale::from_1e12(b.grossloss_1e12)),
        IC => Val::F(scale::from_1e12(b.initial_capital_1e12)),
        MDD => Val::F(scale::from_1e12(b.max_drawdown_1e12)),
        MRU => Val::F(scale::from_1e12(b.max_runup_1e12)),
        HA => Val::F(qty_f64(b.max_contracts_held_all_1e6)),
        HL => Val::F(qty_f64(b.max_contracts_held_long_1e6)),
        HS => Val::F(qty_f64(b.max_contracts_held_short_1e6)),
        // `pin:04-liq-px-1e6` (r33f): the liquidation price is the margin module's computed level,
        // not a fill price — it keeps `px / 1e6` (uncaptured; mintick 0 selects that arm).
        LP => Val::F(nz_px(b.margin_liquidation_price_1e6, 0)),
        OT => Val::I(Some(b.opentrades as i64)),
        CT => Val::I(Some(b.closedtrades as i64)),
        WT => Val::I(Some(b.wintrades as i64)),
        LT => Val::I(Some(b.losstrades as i64)),
        ET => Val::I(Some(b.eventrades as i64)),
        FI => Val::I(Some(first_index(b) as i64)),
        // `np / ic × 100` in that order (E118 4320/4320; `np × 100 / ic` 2890, `np / (ic / 100)`
        // 3311).
        NPP => Val::F(pct(np, ic)),
        GPP => Val::F(pct(scale::from_1e12(b.grossprofit_1e12), ic)),
        GLP => Val::F(pct(scale::from_1e12(b.grossloss_1e12), ic)),
        AT => Val::F(round_sig10(div_na(
            scale::from_1e12(b.netprofit_1e12),
            b.closedtrades as f64,
        ))),
        AW => Val::F(round_sig10(div_na(
            scale::from_1e12(b.grossprofit_1e12),
            b.wintrades as f64,
        ))),
        AL => Val::F(round_sig10(div_na(
            scale::from_1e12(b.grossloss_1e12),
            b.losstrades as f64,
        ))),
        // `pin:04-openprofit-pct-base-int` (r33h): the realized-equity base stays the integer
        // `initial_capital + netprofit` — no capture reads `openprofit_percent`, so CAR-R33-1's f64
        // `ic + np` (equity's own first term) is the candidate, not the law.
        OPP => Val::F(pct(
            op,
            scale::from_1e12(b.initial_capital_1e12 + b.netprofit_1e12),
        )),
        // `pin:04-max-pct-zero-before-trade` (r33f, retires A9's "na before the first trade"): E118
        // reads `max_drawdown_percent` 0 on all 51 rows before its first trade; the run-up twin
        // follows (the maximum over no trade is 0, as `strategy.max_runup` reads).
        // `pin:04-max-dd-pct-equity-f64` (CAR-R33K-1, r33m; replaces
        // `pin:04-max-dd-pct-provisional` for this read — the per-trade
        // `closedtrades.max_drawdown_percent` keeps its own formula): the broker's f64, by its
        // bits (the default `0` reads `+0.0`).
        MDP => Val::F(f64::from_bits(b.max_drawdown_pct_f64_bits)),
        MRP => Val::F(scale::from_1e6(b.max_runup_pct_1e6)),
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
    /// `entry_price` (0 → na; [`px_f64`]).
    EntryPrice,
    /// `exit_price` (0 → na; [`px_f64`]).
    ExitPrice,
    /// `entry_bar_index` (`u32::MAX` → na).
    EntryBar,
    /// `exit_bar_index` (`u32::MAX` → na).
    ExitBar,
    /// `entry_time` (ms; 0 → na).
    EntryTime,
    /// `exit_time` (ms; 0 → na).
    ExitTime,
    /// `size` (signed ×1e6; [`qty_f64`]).
    Size,
    /// `profit` (`pnl_1e12`, net of fees; a closed commission-free trade per [`profit_f64`]).
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
    ///
    /// `pin:04-close-exit-id-read` — a `strategy.close(id)` trade's `exit_sym` is the script's
    /// `id` while its `exit_id` keys `Close entry(s) order <id>` (`bi_close`): no capture reads
    /// `closedtrades.exit_id` of such a trade, so it keeps reading `<id>` (the r27b behaviour);
    /// the List of Trades' text is the candidate a capture would decide.
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
    let x = Fmt::of(f);
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
        Some(t) => Ok(read_fld(t, fld, x)),
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

fn read_fld(t: &TradeRec, fld: u8, x: Fmt) -> Raw {
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
    // literally, no point value (`pin:04-trade-pct-provisional`, operator ruling 2026-10-16),
    // over the values `entry_price` / `size` read (r33f's formations); a zero base → na.
    let base = px_f64(t.entry_px_1e6, x.mt_1e6) * qty_f64(t.qty_1e6).abs();
    let bar = |b: u32| Raw::V(Val::I(if b == u32::MAX { None } else { Some(b as i64) }));
    let ms = |m: i64| Raw::V(Val::I(if m == 0 { None } else { Some(m) }));
    match fld {
        EP => Raw::V(Val::F(nz_px(t.entry_px_1e6, x.mt_1e6))),
        XP => Raw::V(Val::F(nz_px(t.exit_px_1e6, x.mt_1e6))),
        EB => bar(t.entry_bar),
        XB => bar(t.exit_bar),
        ET => ms(t.entry_ms),
        XT => ms(t.exit_ms),
        SZ => Raw::V(Val::F(qty_f64(t.qty_1e6))),
        PR => Raw::V(Val::F(profit_f64(t, x))),
        CM => Raw::V(Val::F(scale::from_1e12(t.fee_1e12))),
        RU => Raw::V(Val::F(scale::from_1e6(t.max_runup_1e6))),
        DD => Raw::V(Val::F(scale::from_1e6(t.max_drawdown_1e6))),
        EI => Raw::S(t.entry_sym),
        XI => Raw::S(t.exit_sym),
        PP => Raw::V(Val::F(pct(profit_f64(t, x), base))),
        RP => Raw::V(Val::F(pct(scale::from_1e6(t.max_runup_1e6), base))),
        DP => Raw::V(Val::F(pct(scale::from_1e6(t.max_drawdown_1e6), base))),
        _ => Raw::Na, // comments are not carried
    }
}
