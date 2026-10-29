// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `Book` — open trades, the one transition, equity, trade records (impl-05 §4.6).
//!
//! Written by [`Book::apply_fill`] and [`Book::mark`] only. Every check that can refuse a fill runs
//! before the first mutation, so a refused fill leaves the book untouched. Zero allocation after
//! [`Book::new`] (whose `ClosedRing` is the crate's one allocation); index loops.
//!
//! CAR-R33-1 (r33h): beside `netprofit_1e12` the book keeps `strategy.netprofit` as TradingView
//! forms it — since r39 the f64 running gross profit less the gross loss ([`crate::visible`]);
//! it is lent to the script through `BrokerView::netprofit_f64_bits` and read by no broker law. CAR-R33K-1 (r33m): likewise
//! `strategy.max_drawdown_percent` — the closed-equity peak and the running f64 maximum folded at
//! every mark ([`visible::raise_peak`], [`visible::fold_dd_pct`]), lent through
//! `BrokerView::max_drawdown_pct_f64_bits`; the risk rules keep their integer source.

use crate::cap;
use crate::closed::ClosedRing;
use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
use crate::money::{
    e12_to_1e6_ceil, e12_to_1e6_floor, mul_div, trade_pct_1e6, usd_1e6_to_1e12, E6,
};
use crate::visible;
use pine_ir::{trade_flag, BrokerView, StrKey, Sym, TradeRec};

/// `cap::LOTS − 1`: the ring mask (`LOTS` is a power of two).
const MASK: usize = cap::LOTS - 1;
const _: () = assert!(cap::LOTS.is_power_of_two() && cap::LOTS <= u16::MAX as usize);

/// The order a fill serves (§4.6.2 `src`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FillSrc {
    /// The filling order's id: a new trade's `entry_id`, a closed slice's `exit_id`.
    pub(crate) id: StrKey,
    /// Its `Sym` (display).
    pub(crate) sym: Sym,
    /// `close_entries_rule = "ANY"`: the entry id closed first (`NONE` = FIFO only).
    pub(crate) named_entry: StrKey,
    /// Bar at which the order was born (§4.5.1).
    pub(crate) born_bar: u32,
    /// Evaluation that generated it (§4.7.4).
    pub(crate) born_eval: u32,
    /// `Entry` / `Order`: may open a trade. A close or exit that would open one is a desync.
    pub(crate) is_entry: bool,
    /// [`lot_flag`] bits for a trade this fill opens.
    pub(crate) lot_flags: u8,
}

/// Per-open-trade facts the sizing rules read (§4.7.4).
pub(crate) mod lot_flag {
    /// Opened by a `strategy.entry` order (counts toward `pyramiding`; `strategy.order` never does).
    pub(crate) const PYRAMID: u8 = 1 << 0;
    /// Opened by a price-based (limit / stop / stop-limit) entry — the same-tick exception.
    pub(crate) const PRICE_BASED: u8 = 1 << 1;
    /// The D19 boot trade: covered by an exit with any `from_entry` (I34).
    pub(crate) const BOOT: u8 = 1 << 2;
}

/// `±1` as the sign of `x` (`0` stays `0`).
const fn sgn(x: i64) -> i128 {
    if x > 0 {
        1
    } else if x < 0 {
        -1
    } else {
        0
    }
}

/// `sign·value(px − entry, n, pv)` ×1e12 in two saturating steps (§4.6.2 "P&L").
fn gross_1e12(sign: i128, px_1e6: i64, entry_px_1e6: i64, n_1e6: i128, pv_1e6: i64) -> i128 {
    let diff = i128::from(px_1e6) - i128::from(entry_px_1e6);
    // `mul_div` stays within ±i128::MAX, so the negation never overflows.
    sign * mul_div(mul_div(diff, n_1e6, 1), i128::from(pv_1e6), E6)
}

/// Open trade `l`'s `(max_runup_1e6, max_drawdown_1e6)` after one more sample at `px` (§4.6.3).
///
/// `pin:05-trade-excursion` (CI-033 / CI-034) — TradingView's per-trade favorable / adverse
/// excursion ("Favorable / Adverse excursion" in the List of Trades, `max_runup` /
/// `max_drawdown`) is the extreme of the trade's open profit — `sign·value(p − entry, |qty|, pv)`
/// net of the entry fee the trade carries (`pin:05-openprofit-fees`) — over the path points from
/// its entry fill to its exit fill, the exit fill price included and the exit bar's path after it
/// excluded; run-up floored at 0, drawdown at 0 (printed ≤ 0 by TradingView). A partially closed
/// trade gives each closed slice its quantity's share. Derived from 6 067 captured trades of the
/// E fixtures on the chart's own OHLC (5 878 match to the cent; the rest are other broker laws:
/// trails, exit legs, CI-036): E009 run-up 200 = the profit target, E011 drawdown −772.5 = the stop,
/// E116 a same-bar stop (−200), E111 a 1-of-20 slice (4.7, not 94), E063–E068 the entry fee.
fn excursion_at(l: &TradeRec, px_1e6: i64, pv_1e6: i64) -> (i64, i64) {
    let a = i128::from(l.qty_1e6).abs();
    let open =
        gross_1e12(sgn(l.qty_1e6), px_1e6, l.entry_px_1e6, a, pv_1e6).saturating_sub(l.fee_1e12);
    (
        l.max_runup_1e6.max(e12_to_1e6_floor(open.max(0))),
        l.max_drawdown_1e6.max(e12_to_1e6_ceil((-open).max(0))),
    )
}

/// `v·n/lq` (truncated) — a closed slice's share of its trade's excursion; `0 < n < lq`.
fn share_of(v: i64, n: i128, lq: i128) -> i64 {
    crate::money::sat_i64(mul_div(i128::from(v), n, lq))
}

/// The book both brokers drive (§4.6.1).
pub(crate) struct Book {
    /// Open trades, a ring: index `(head + i) & MASK`, oldest first.
    lots: [TradeRec; cap::LOTS],
    /// Bar at which each trade's entry order was born (§4.5.1).
    lot_born_bar: [u32; cap::LOTS],
    /// Evaluation that generated each trade (same-tick pyramiding, §4.7.4).
    lot_born_eval: [u32; cap::LOTS],
    /// Open order, for the exit-set sort.
    lot_open_seq: [u32; cap::LOTS],
    /// [`lot_flag`] bits.
    lot_flags: [u8; cap::LOTS],
    head: u16,
    n: u16,
    next_open_seq: u32,
    pv_1e6: i64,
    /// `chart.mintick_1e6` — the tick of the script-visible price formation
    /// ([`visible::px_f64`]); 0 until [`Book::set_mintick`] (prices then read `px / 1e6`).
    mt_1e6: i64,
    close_any: bool,
    position_1e6: i64,
    avg_px_1e6: i64,
    mark_1e6: i64,
    initial_1e12: i128,
    netprofit_1e12: i128,
    /// CAR-R33-1 (r39 form): the f64 running gross profit and gross loss of the closed slices'
    /// script-visible profits, in close order ([`visible::add_slice`]); `strategy.netprofit` is
    /// their difference ([`visible::view_bits`]). Held as `f64::to_bits` (D18: the book names no
    /// float).
    gp_f64_bits: u64,
    gl_f64_bits: u64,
    /// CAR-R33K-1 (`pin:04-max-dd-pct-equity-f64`): the closed-equity peak `ic + NP` in f64
    /// ([`visible::raise_peak`]), starting at `ic`, as `f64::to_bits`.
    dd_peak_bits: u64,
    /// CAR-R33K-1: `strategy.max_drawdown_percent`, the running maximum of
    /// [`visible::fold_dd_pct`] over every mark, as `f64::to_bits` (`0` reads `+0.0`).
    dd_pct_bits: u64,
    grossprofit_1e12: i128,
    grossloss_1e12: i128,
    commission_1e12: i128,
    openprofit_1e12: i128,
    peak_equity_1e12: i128,
    trough_equity_1e12: i128,
    max_drawdown_1e12: i128,
    max_runup_1e12: i128,
    /// A9 — the largest per-trade `max_runup` percent seen, ×1e6 (the drawdown twin is
    /// `dd_pct_bits`, CAR-R33K-1).
    max_ru_pct_1e6: i64,
    /// A9 — Σ closed-trade `profit_percent` ×1e6: all, winning, losing.
    sum_trade_pct_1e6: i64,
    sum_win_pct_1e6: i64,
    sum_loss_pct_1e6: i64,
    max_held_all_1e6: i64,
    max_held_long_1e6: i64,
    max_held_short_1e6: i64,
    closedtrades: u32,
    wintrades: u32,
    losstrades: u32,
    eventrades: u32,
    fills: u32,
    /// §4.6.6 — the one allocation.
    closed: ClosedRing,
    /// Refreshed at the end of every mutating call.
    view: BrokerView,
}

impl Book {
    /// An empty book. Allocates the closed ring — the only allocation (I3).
    pub(crate) fn new(initial_capital_1e6: i64, pointvalue_1e6: i64, close_any: bool) -> Self {
        let mut b = Self {
            lots: [TradeRec::default(); cap::LOTS],
            lot_born_bar: [0; cap::LOTS],
            lot_born_eval: [0; cap::LOTS],
            lot_open_seq: [0; cap::LOTS],
            lot_flags: [0; cap::LOTS],
            head: 0,
            n: 0,
            next_open_seq: 0,
            pv_1e6: 0,
            mt_1e6: 0,
            close_any: false,
            position_1e6: 0,
            avg_px_1e6: 0,
            mark_1e6: 0,
            initial_1e12: 0,
            netprofit_1e12: 0,
            gp_f64_bits: 0,
            gl_f64_bits: 0,
            dd_peak_bits: 0,
            dd_pct_bits: 0,
            grossprofit_1e12: 0,
            grossloss_1e12: 0,
            commission_1e12: 0,
            openprofit_1e12: 0,
            peak_equity_1e12: 0,
            trough_equity_1e12: 0,
            max_drawdown_1e12: 0,
            max_runup_1e12: 0,
            max_ru_pct_1e6: 0,
            sum_trade_pct_1e6: 0,
            sum_win_pct_1e6: 0,
            sum_loss_pct_1e6: 0,
            max_held_all_1e6: 0,
            max_held_long_1e6: 0,
            max_held_short_1e6: 0,
            closedtrades: 0,
            wintrades: 0,
            losstrades: 0,
            eventrades: 0,
            fills: 0,
            closed: ClosedRing::new(),
            view: BrokerView::default(),
        };
        b.reset(initial_capital_1e6, pointvalue_1e6, close_any);
        b
    }

    /// Back to the empty book of [`Book::new`], IN PLACE: the closed ring is cleared, never
    /// reallocated (I3/I4 — the D19 boot reconciliation runs after `new`). The mintick of
    /// [`Book::set_mintick`] is a chart constant and stays.
    pub(crate) fn reset(&mut self, initial_capital_1e6: i64, pointvalue_1e6: i64, close_any: bool) {
        let initial = usd_1e6_to_1e12(initial_capital_1e6);
        let mut i = 0;
        while i < cap::LOTS {
            self.lots[i] = TradeRec::default();
            self.lot_born_bar[i] = 0;
            self.lot_born_eval[i] = 0;
            self.lot_open_seq[i] = 0;
            self.lot_flags[i] = 0;
            i += 1;
        }
        self.head = 0;
        self.n = 0;
        self.next_open_seq = 0;
        self.pv_1e6 = pointvalue_1e6;
        self.close_any = close_any;
        self.position_1e6 = 0;
        self.avg_px_1e6 = 0;
        self.mark_1e6 = 0;
        self.initial_1e12 = initial;
        self.netprofit_1e12 = 0;
        self.gp_f64_bits = 0;
        self.gl_f64_bits = 0;
        self.dd_peak_bits = visible::initial_bits(initial);
        self.dd_pct_bits = 0;
        self.grossprofit_1e12 = 0;
        self.grossloss_1e12 = 0;
        self.commission_1e12 = 0;
        self.openprofit_1e12 = 0;
        self.peak_equity_1e12 = initial;
        self.trough_equity_1e12 = initial;
        self.max_drawdown_1e12 = 0;
        self.max_runup_1e12 = 0;
        self.max_ru_pct_1e6 = 0;
        self.sum_trade_pct_1e6 = 0;
        self.sum_win_pct_1e6 = 0;
        self.sum_loss_pct_1e6 = 0;
        self.max_held_all_1e6 = 0;
        self.max_held_long_1e6 = 0;
        self.max_held_short_1e6 = 0;
        self.closedtrades = 0;
        self.wintrades = 0;
        self.losstrades = 0;
        self.eventrades = 0;
        self.fills = 0;
        self.closed.clear();
        self.view = BrokerView::default();
        self.refresh_view();
    }

    /// The chart's mintick ×1e6 for the script-visible price formation (CAR-R33-1); each broker
    /// sets it once after [`Book::new`]. Changes no integer.
    pub(crate) fn set_mintick(&mut self, mintick_1e6: i64) {
        self.mt_1e6 = mintick_1e6;
    }

    /// Ring slot of open trade `i`.
    const fn slot(&self, i: usize) -> usize {
        (self.head as usize + i) & MASK
    }

    /// The view (§4.6.4) — a borrow.
    pub(crate) fn view(&self) -> &BrokerView {
        &self.view
    }

    /// Open trade `i` (0 = oldest) — a borrow, never a copy.
    pub(crate) fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        if i >= u32::from(self.n) {
            return None;
        }
        Some(&self.lots[self.slot(i as usize)])
    }

    /// Closed trade `i` (0 = oldest retained).
    pub(crate) fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.closed.get(i)
    }

    /// REQ-r33g-1: the net profit ×1e12 the view read just after closed trade `i` (0 = oldest
    /// retained) was booked — the closed trades' P&L so far less the still-open trades' paid entry
    /// fees (`pin:05-openprofit-fees`), TradingView's List of Trades `cum_pnl`.
    pub(crate) fn closed_cum_net(&self, i: u32) -> Option<i128> {
        self.closed.cum_net(i)
    }

    /// Fills booked.
    pub(crate) fn fills(&self) -> u32 {
        self.fills
    }

    /// Equity ×1e12 were the open trades marked at `px` (no mutation) — the funds check's equity
    /// after a fill's reducing part (§4.9.1).
    pub(crate) fn equity_at(&self, px_1e6: i64) -> i128 {
        let n = usize::from(self.n);
        let mut open = 0_i128;
        let mut i = 0;
        while i < n {
            let l = &self.lots[self.slot(i)];
            let a = i128::from(l.qty_1e6).abs();
            let g = gross_1e12(sgn(l.qty_1e6), px_1e6, l.entry_px_1e6, a, self.pv_1e6);
            open = open.saturating_add(g.saturating_sub(l.fee_1e12));
            i += 1;
        }
        self.initial_1e12
            .saturating_add(self.netprofit_1e12)
            .saturating_add(open)
    }

    /// Set `strategy.margin_liquidation_price` (TV margin, §4.9.3).
    pub(crate) fn set_liquidation_price(&mut self, px_1e6: i64) {
        self.view.margin_liquidation_price_1e6 = px_1e6;
    }

    /// Commission paid ×1e12.
    pub(crate) fn commission_1e12(&self) -> i128 {
        self.commission_1e12
    }

    /// Bar and evaluation at which open trade `i`'s entry order was born.
    pub(crate) fn lot_born(&self, i: u32) -> Option<(u32, u32)> {
        if i >= u32::from(self.n) {
            return None;
        }
        let s = self.slot(i as usize);
        Some((self.lot_born_bar[s], self.lot_born_eval[s]))
    }

    /// Open order of open trade `i`.
    pub(crate) fn lot_open_seq(&self, i: u32) -> Option<u32> {
        if i >= u32::from(self.n) {
            return None;
        }
        Some(self.lot_open_seq[self.slot(i as usize)])
    }

    /// [`lot_flag`] bits of open trade `i`.
    pub(crate) fn lot_flags(&self, i: u32) -> Option<u8> {
        if i >= u32::from(self.n) {
            return None;
        }
        Some(self.lot_flags[self.slot(i as usize)])
    }

    /// Σ fee over the open trades: their paid entry fees (`pin:05-openprofit-fees`).
    fn open_fees_1e12(&self) -> i128 {
        let n = usize::from(self.n);
        let mut s = 0_i128;
        let mut i = 0;
        while i < n {
            s = s.saturating_add(self.lots[self.slot(i)].fee_1e12);
            i += 1;
        }
        s
    }

    /// Σ |qty| over the open trades.
    fn open_abs_sum(&self) -> i128 {
        let n = usize::from(self.n);
        let mut s = 0_i128;
        let mut i = 0;
        while i < n {
            s += i128::from(self.lots[self.slot(i)].qty_1e6).abs();
            i += 1;
        }
        s
    }

    /// The open trade consumed next: with `"ANY"`, the oldest whose `entry_id == named`; else the
    /// oldest (FIFO, §4.6.2 line 5).
    fn next_to_close(&self, named: StrKey) -> usize {
        if self.close_any && named != StrKey::NONE {
            let n = usize::from(self.n);
            let mut i = 0;
            while i < n {
                if self.lots[self.slot(i)].entry_id == named {
                    return i;
                }
                i += 1;
            }
        }
        0
    }

    /// Remove open trade `i` (it is flat).
    fn remove_lot(&mut self, i: usize) {
        let n = usize::from(self.n);
        if i == 0 {
            self.head = u16::try_from((usize::from(self.head) + 1) & MASK).unwrap_or(0);
        } else {
            let mut k = i;
            while k + 1 < n {
                let (dst, src) = (self.slot(k), self.slot(k + 1));
                // COPY: §4.17 #2 — mid-ring removal under "ANY": `open_trade(i)` must stay O(1).
                self.lots[dst] = self.lots[src];
                self.lot_born_bar[dst] = self.lot_born_bar[src];
                self.lot_born_eval[dst] = self.lot_born_eval[src];
                self.lot_open_seq[dst] = self.lot_open_seq[src];
                self.lot_flags[dst] = self.lot_flags[src];
                k += 1;
            }
        }
        self.n -= 1;
    }

    /// The one transition (§4.6.2). `q`: signed contracts ×1e6 (> 0 bought); `fee_1e12 ≥ 0`;
    /// `cause`: `0`, `trade_flag::LIQUIDATION` or `trade_flag::RISK_FLATTEN`.
    pub(crate) fn apply_fill(
        &mut self,
        q: i64,
        px_1e6: i64,
        fee_1e12: i128,
        src: &FillSrc,
        bar_index: u32,
        time_ms: i64,
        cause: u32,
    ) -> Result<(), BrokerFault> {
        if q == 0 {
            return Ok(()); // I11
        }
        let aq = i128::from(q).abs();
        let pos = self.position_1e6;
        let red = if pos != 0 && (q > 0) != (pos > 0) {
            aq.min(i128::from(pos).abs())
        } else {
            0
        };
        let open = aq - red;
        // Every refusal before the first mutation.
        if red > self.open_abs_sum() {
            return Err(BrokerFault::Internal(InternalDefect::LotUnderflow));
        }
        if open > 0 && !src.is_entry {
            return Err(BrokerFault::Internal(InternalDefect::BookDesync));
        }
        if open > 0 && red == 0 && usize::from(self.n) == cap::LOTS {
            return Err(BrokerFault::Script(ScriptRefusal::LotTableFull));
        }
        let new_pos = match pos.checked_add(q) {
            Some(p) => p,
            None => return Err(BrokerFault::Internal(InternalDefect::BookDesync)),
        };
        let open_signed = match i64::try_from(sgn(q) * open) {
            Ok(v) => v,
            Err(_) => return Err(BrokerFault::Internal(InternalDefect::BookDesync)),
        };

        // Fee slices: each consumed slice, then the opening part, pro rata; the last slice takes
        // the remainder so Σ slices == fee exactly (I13).
        let mut fee_left = fee_1e12;
        let mut qty_left = aq;
        let mut red_left = red;
        // REQ-r33g-1: the open trades' paid entry fees, kept current slice by slice.
        let mut open_fees = if red > 0 { self.open_fees_1e12() } else { 0 };
        while red_left > 0 {
            let i = self.next_to_close(src.named_entry);
            let s = self.slot(i);
            let l = &self.lots[s];
            let (l_qty, l_fee, l_entry) = (l.qty_1e6, l.fee_1e12, l.entry_px_1e6);
            let lq = i128::from(l_qty).abs();
            let n = red_left.min(lq);
            // `pin:05-trade-excursion` (CI-033; E009 E011 E063 E111 E116): the exit fill is the
            // closing trade's last excursion sample, on the open-profit basis (§4.6.3); the slice
            // takes its quantity's share of the trade's excursion, the rest stays with the lot.
            let (ru, dd) = excursion_at(&self.lots[s], px_1e6, self.pv_1e6);
            let (c_ru, c_dd) = if n == lq {
                (ru, dd)
            } else {
                (share_of(ru, n, lq), share_of(dd, n, lq))
            };
            let exit_share = if n == qty_left {
                fee_left
            } else {
                mul_div(fee_1e12, n, aq)
            };
            fee_left -= exit_share;
            qty_left -= n;
            let entry_share = if n == lq {
                l_fee
            } else {
                mul_div(l_fee, n, lq)
            };
            let sign = sgn(l_qty);
            // `n ≤ |L.qty|`, so the narrowing is exact.
            let c_qty = i64::try_from(sign * n).unwrap_or(0);
            let c_fee = entry_share.saturating_add(exit_share);
            let c_pnl = gross_1e12(sign, px_1e6, l_entry, n, self.pv_1e6).saturating_sub(c_fee);
            // CAR-R33-1 (`pin:04-trade-profit-f64`, `pin:05-net-f64-fees-provisional`): the
            // slice's script-visible profit joins the f64 gross profit or gross loss (r39).
            (self.gp_f64_bits, self.gl_f64_bits) = visible::add_slice(
                self.gp_f64_bits,
                self.gl_f64_bits,
                l_entry,
                px_1e6,
                c_qty,
                entry_share,
                exit_share,
                self.mt_1e6,
                self.pv_1e6,
            );
            // The lot gives up `entry_share` below (removed when it reaches zero).
            open_fees = open_fees.saturating_sub(entry_share);
            let (c, cum) = self.closed.push_slot();
            // COPY: §4.17 #1 lot → closed-ring slot, 128 B, once per closed slice — the ring owns
            // an immutable record while the lot slot is reused — a local staging copy (2×) rejected.
            *c = self.lots[s];
            c.qty_1e6 = c_qty;
            c.fee_1e12 = c_fee;
            c.pnl_1e12 = c_pnl;
            c.max_runup_1e6 = c_ru;
            c.max_drawdown_1e6 = c_dd;
            c.flags = trade_flag::PNL_NET | cause;
            c.exit_id = src.id;
            c.exit_sym = src.sym;
            c.exit_px_1e6 = px_1e6;
            c.exit_ms = time_ms;
            c.exit_bar = bar_index;
            self.netprofit_1e12 = self.netprofit_1e12.saturating_add(c_pnl);
            // REQ-r33g-1 (r33g, TV E062): TradingView's `cum_pnl` after a close is the net profit
            // at that moment — the still-open trades' paid entry fees included (−1602.94 =
            // −1596.94 − six 1 USD entry fees); recorded beside the record, same ring.
            *cum = self.netprofit_1e12.saturating_sub(open_fees);
            // A9: the closed record's run-up percent, read as `closedtrades.*_percent` reads it.
            let c_ru = c.max_runup_1e6;
            self.roll_runup_pct(c_ru, l_entry, c_qty);
            let c_pct = trade_pct_1e6(c_pnl, l_entry, c_qty).unwrap_or(0);
            self.sum_trade_pct_1e6 = self.sum_trade_pct_1e6.saturating_add(c_pct);
            if c_pnl > 0 {
                self.sum_win_pct_1e6 = self.sum_win_pct_1e6.saturating_add(c_pct);
                self.grossprofit_1e12 = self.grossprofit_1e12.saturating_add(c_pnl);
                self.wintrades += 1;
            } else if c_pnl < 0 {
                self.sum_loss_pct_1e6 = self.sum_loss_pct_1e6.saturating_add(c_pct);
                self.grossloss_1e12 = self.grossloss_1e12.saturating_sub(c_pnl);
                self.losstrades += 1;
            } else {
                self.eventrades += 1;
            }
            self.closedtrades += 1;
            let lot = &mut self.lots[s];
            lot.fee_1e12 -= entry_share;
            lot.qty_1e6 -= c_qty;
            lot.max_runup_1e6 = ru - c_ru;
            lot.max_drawdown_1e6 = dd - c_dd;
            if lot.qty_1e6 == 0 {
                self.remove_lot(i);
            }
            red_left -= n;
        }
        if red > 0 {
            // CAR-R33K-1 (`pin:04-max-dd-pct-equity-f64`): the closed-equity peak after the close,
            // before the mark below folds this fill's point.
            // `pin:04-max-dd-pct-uncaptured-provisional`: once per closing fill, before a
            // reversal's opening part, `NP` net of the still-open trades' paid entry fees
            // (`open_fees`, kept current slice by slice above).
            let np = visible::view_bits(self.gp_f64_bits, self.gl_f64_bits, open_fees);
            self.dd_peak_bits = visible::raise_peak(self.dd_peak_bits, self.initial_1e12, np);
        }
        if open > 0 {
            let s = self.slot(usize::from(self.n));
            self.lots[s] = TradeRec {
                entry_id: src.id,
                exit_id: StrKey::NONE,
                entry_ms: time_ms,
                exit_ms: 0,
                entry_px_1e6: px_1e6,
                exit_px_1e6: 0,
                qty_1e6: open_signed,
                entry_bar: bar_index,
                exit_bar: u32::MAX,
                pnl_1e12: -fee_left,
                fee_1e12: fee_left,
                max_runup_1e6: 0,
                max_drawdown_1e6: 0,
                flags: trade_flag::OPEN | trade_flag::PNL_NET,
                entry_sym: src.sym,
                exit_sym: Sym::NONE,
                ..TradeRec::default()
            };
            self.lot_born_bar[s] = src.born_bar;
            self.lot_born_eval[s] = src.born_eval;
            self.lot_open_seq[s] = self.next_open_seq;
            self.lot_flags[s] = src.lot_flags;
            self.next_open_seq = self.next_open_seq.wrapping_add(1);
            self.n += 1;
        }
        self.position_1e6 = new_pos;
        self.avg_px_1e6 = self.weighted_entry();
        self.commission_1e12 = self.commission_1e12.saturating_add(fee_1e12);
        self.fills += 1;
        let ap = new_pos.saturating_abs();
        self.max_held_all_1e6 = self.max_held_all_1e6.max(ap);
        if new_pos > 0 {
            self.max_held_long_1e6 = self.max_held_long_1e6.max(ap);
        } else if new_pos < 0 {
            self.max_held_short_1e6 = self.max_held_short_1e6.max(ap);
        }
        // INCREMENT: §4.6.2 line 9 (exit sets) lands with `exit.rs` (T8).
        // `pin:05-trade-excursion` (r33d): the trade this fill opens takes no excursion sample at
        // its own entry fill — its first is the next tick (TV E061 trades 21 / 31 / 43: stop entries
        // filled inside a segment, the price never back through them — drawdown 0, not the 1 USD
        // entry fee; E062 trade 156: a market entry at an open that is also the low — the low's
        // tick gives −1, the fee).
        let fresh = if open > 0 {
            self.slot(usize::from(self.n) - 1)
        } else {
            usize::MAX
        };
        self.mark_skip(px_1e6, fresh);
        Ok(())
    }

    /// Σ|qty|·entry_px / Σ|qty| over the open trades, truncated; 0 when flat (fees excluded).
    fn weighted_entry(&self) -> i64 {
        let n = usize::from(self.n);
        let mut num = 0_i128;
        let mut den = 0_i128;
        let mut i = 0;
        while i < n {
            let l = &self.lots[self.slot(i)];
            let a = i128::from(l.qty_1e6).abs();
            num = num.saturating_add(mul_div(a, i128::from(l.entry_px_1e6), 1));
            den += a;
            i += 1;
        }
        if den == 0 {
            0
        } else {
            crate::money::sat_i64(num / den)
        }
    }

    /// Re-price every open trade at `px` and roll equity, drawdown and run-up (§4.6.3).
    pub(crate) fn mark(&mut self, px_1e6: i64) {
        self.mark_skip(px_1e6, usize::MAX);
    }

    /// [`Self::mark`], the lot in slot `skip` re-priced but not sampled for its excursion (the
    /// trade a fill just opened; `usize::MAX`: none).
    fn mark_skip(&mut self, px_1e6: i64, skip: usize) {
        let n = usize::from(self.n);
        let pv = self.pv_1e6;
        let mt = self.mt_1e6;
        let mut open = 0_i128;
        // CAR-R33K-1: the open trades' f64 open profit (`+0.0` start, oldest first) and paid
        // entry fees, for the drawdown-percent fold below.
        let mut op_bits = 0_u64;
        let mut open_fees = 0_i128;
        let mut i = 0;
        while i < n {
            let s = self.slot(i);
            let l = &mut self.lots[s];
            let a = i128::from(l.qty_1e6).abs();
            let gross = gross_1e12(sgn(l.qty_1e6), px_1e6, l.entry_px_1e6, a, pv);
            l.pnl_1e12 = gross.saturating_sub(l.fee_1e12);
            open = open.saturating_add(l.pnl_1e12);
            op_bits = visible::add_open(op_bits, px_1e6, l.entry_px_1e6, l.qty_1e6, mt, pv);
            open_fees = open_fees.saturating_add(l.fee_1e12);
            if s != skip {
                // `pin:05-trade-excursion`: sampled on the open-profit basis (net of the entry fee).
                let o = l.pnl_1e12;
                l.max_runup_1e6 = l.max_runup_1e6.max(e12_to_1e6_floor(o.max(0)));
                l.max_drawdown_1e6 = l.max_drawdown_1e6.max(e12_to_1e6_ceil((-o).max(0)));
                let (ru, e, q) = (l.max_runup_1e6, l.entry_px_1e6, l.qty_1e6);
                self.roll_runup_pct(ru, e, q);
            }
            i += 1;
        }
        self.mark_1e6 = px_1e6;
        self.openprofit_1e12 = open;
        // CAR-R33K-1 (`pin:04-max-dd-pct-equity-f64`, shared with native): every marked path point
        // — a fill's included, the trade it just opened included — folds
        // `(peak − eq) / peak * 100`, `eq = (ic + NP) + op` with `NP` the view's f64 net profit.
        let np = visible::view_bits(self.gp_f64_bits, self.gl_f64_bits, open_fees);
        self.dd_pct_bits = visible::fold_dd_pct(
            self.dd_pct_bits,
            self.dd_peak_bits,
            self.initial_1e12,
            np,
            op_bits,
        );
        let eq = self.equity_1e12();
        self.peak_equity_1e12 = self.peak_equity_1e12.max(eq);
        self.trough_equity_1e12 = self.trough_equity_1e12.min(eq);
        self.max_drawdown_1e12 = self
            .max_drawdown_1e12
            .max(self.peak_equity_1e12.saturating_sub(eq));
        self.max_runup_1e12 = self
            .max_runup_1e12
            .max(eq.saturating_sub(self.trough_equity_1e12));
        self.refresh_view();
    }

    /// A9 — roll `strategy.max_runup_percent`: the reference's per-trade formula (`Highest Value
    /// During Trade / (Entry Price x Quantity) * 100`) taken literally, the largest over every
    /// trade's record as `opentrades.*` / `closedtrades.*` read it
    /// (`pin:04-max-runup-pct-provisional`; no capture). Called for each open trade at every mark
    /// and for each closed slice; a zero base adds nothing. `max_drawdown_percent` left this rule
    /// for the captured equity-curve law (CAR-R33K-1, [`visible::fold_dd_pct`]).
    fn roll_runup_pct(&mut self, ru_1e6: i64, entry_px_1e6: i64, qty_1e6: i64) {
        if ru_1e6 != 0 {
            if let Some(p) = trade_pct_1e6(usd_1e6_to_1e12(ru_1e6), entry_px_1e6, qty_1e6) {
                self.max_ru_pct_1e6 = self.max_ru_pct_1e6.max(p);
            }
        }
    }

    /// `initial_capital + netprofit + openprofit` (I9).
    fn equity_1e12(&self) -> i128 {
        self.initial_1e12
            .saturating_add(self.netprofit_1e12)
            .saturating_add(self.openprofit_1e12)
    }

    /// Refresh from the running aggregates and the open trades' fees (§4.6.4).
    /// `margin_liquidation_price_1e6` is the TV margin module's (§4.9.3) and is left as it is.
    fn refresh_view(&mut self) {
        let eq = self.equity_1e12();
        let open_fees = self.open_fees_1e12();
        let entry_name = if self.n == 0 {
            StrKey::NONE
        } else {
            self.lots[usize::from(self.head)].entry_id
        };
        let v = &mut self.view;
        v.position_size_1e6 = self.position_1e6;
        v.position_avg_price_1e6 = self.avg_px_1e6;
        v.position_entry_name = entry_name;
        v.max_contracts_held_all_1e6 = self.max_held_all_1e6;
        v.max_contracts_held_long_1e6 = self.max_held_long_1e6;
        v.max_contracts_held_short_1e6 = self.max_held_short_1e6;
        v.opentrades = u32::from(self.n);
        v.closedtrades = self.closedtrades;
        v.wintrades = self.wintrades;
        v.losstrades = self.losstrades;
        v.eventrades = self.eventrades;
        v.closed_retained = self.closed.len();
        v.equity_1e12 = eq;
        // `pin:05-openprofit-fees` (r33d, shared with native): an open trade's entry fee is paid —
        // `strategy.netprofit` carries it from the entry fill and `strategy.openprofit` is the open
        // trades' gross profit; equity is unchanged. TV E114: one long at 77249.6, 0.1 % commission,
        // close 76964.3 → openprofit −285.3 (gross), equity 999 637.4504 (fee 77.2496 paid); E062's
        // cumulative P&L after trade 1 of 7 closed at once is −1602.94: its −1596.94 and the six
        // still-open trades' 1 USD entry fees.
        v.netprofit_1e12 = self.netprofit_1e12.saturating_sub(open_fees);
        v.openprofit_1e12 = self.openprofit_1e12.saturating_add(open_fees);
        v.grossprofit_1e12 = self.grossprofit_1e12;
        v.grossloss_1e12 = self.grossloss_1e12;
        v.initial_capital_1e12 = self.initial_1e12;
        v.max_drawdown_1e12 = self.max_drawdown_1e12;
        v.max_runup_1e12 = self.max_runup_1e12;
        // CAR-R33K-1: the f64 drawdown percent, as its bits.
        v.max_drawdown_pct_f64_bits = self.dd_pct_bits;
        v.max_runup_pct_1e6 = self.max_ru_pct_1e6;
        v.sum_trade_pct_1e6 = self.sum_trade_pct_1e6;
        v.sum_win_pct_1e6 = self.sum_win_pct_1e6;
        v.sum_loss_pct_1e6 = self.sum_loss_pct_1e6;
        // CAR-R33-1: the f64 net profit, less the open trades' paid entry fees as the integer
        // `netprofit_1e12` above (`pin:05-openprofit-fees`).
        v.netprofit_f64_bits = visible::view_bits(self.gp_f64_bits, self.gl_f64_bits, open_fees);
    }
}

#[cfg(test)]
mod tests {
    //! §7.4's cases on the book itself (`tests/book.rs` repeats them through the brokers once they
    //! land, T11 / T14).
    // CAR-R33K-1: the drawdown-percent cases write the f64 law out (test code; the D18 gate and the
    // crate's float lint cover the book's own code).
    #![allow(clippy::float_arithmetic)]

    use super::{Book, FillSrc};
    use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
    use crate::money::{trade_pct_1e6, usd_1e6_to_1e12, value_1e12};
    use pine_ir::{trade_flag, StrKey, Sym, TradeRec};
    use proptest::prelude::{prop_assert, prop_assert_eq, proptest, ProptestConfig};

    const PV: i64 = 1_000_000;
    const CAP: i64 = 100_000 * 1_000_000;

    fn src(id: u64) -> FillSrc {
        FillSrc {
            id: StrKey(id),
            sym: Sym(u32::try_from(id).unwrap_or(0)),
            named_entry: StrKey::NONE,
            born_bar: 0,
            born_eval: 0,
            is_entry: true,
            lot_flags: 0,
        }
    }

    fn close(id: u64, named: u64) -> FillSrc {
        FillSrc {
            named_entry: StrKey(named),
            is_entry: false,
            ..src(id)
        }
    }

    fn px(x: i64) -> i64 {
        x * 1_000_000
    }

    fn qty(x: i64) -> i64 {
        x * 1_000_000
    }

    /// I7, I8, I9 and I13 over the whole book.
    fn check(b: &Book) {
        let v = b.view();
        let mut sum = 0_i64;
        let mut fees = 0_i128;
        let mut open_fees = 0_i128;
        let mut i = 0;
        while let Some(t) = b.open_trade(i) {
            sum += t.qty_1e6;
            fees += t.fee_1e12;
            open_fees += t.fee_1e12;
            assert_eq!(t.flags, trade_flag::OPEN | trade_flag::PNL_NET);
            i += 1;
        }
        let mut i = 0;
        while let Some(t) = b.closed_trade(i) {
            fees += t.fee_1e12;
            assert_eq!(
                t.flags & (trade_flag::OPEN | trade_flag::PNL_NET),
                trade_flag::PNL_NET
            );
            i += 1;
        }
        assert_eq!(v.position_size_1e6, sum, "I7");
        assert_eq!(v.position_avg_price_1e6 == 0, sum == 0, "I8");
        assert_eq!(
            v.equity_1e12,
            v.initial_capital_1e12 + v.netprofit_1e12 + v.openprofit_1e12
        );
        // I9 on the closed trades: the view's `netprofit` also carries the open trades' paid entry
        // fees (`pin:05-openprofit-fees`, r33d).
        assert_eq!(
            v.netprofit_1e12 + open_fees,
            v.grossprofit_1e12 - v.grossloss_1e12,
            "I9"
        );
        if v.closedtrades <= 9000 {
            assert_eq!(fees, b.commission_1e12(), "I13");
        }
        assert_eq!(v.closed_retained, v.closedtrades.min(9000), "I12");
        if v.closedtrades <= 9000 {
            check_a9(b);
        }
    }

    /// A9: the view's four integer percent aggregates are the records' own, recomputed from
    /// scratch — the per-trade run-up maximum over every open and closed record, the sums over the
    /// closed ones. The drawdown percent (CAR-R33K-1) is an equity-curve maximum no record holds:
    /// it is never negative and never NaN at a positive initial capital.
    fn check_a9(b: &Book) {
        let pct = |v: i128, t: &TradeRec| trade_pct_1e6(v, t.entry_px_1e6, t.qty_1e6);
        let (mut ru, mut all, mut win, mut loss) = (0_i64, 0_i64, 0_i64, 0_i64);
        let mut roll = |t: &TradeRec| {
            ru = ru.max(pct(usd_1e6_to_1e12(t.max_runup_1e6), t).unwrap_or(0));
        };
        let mut i = 0;
        while let Some(t) = b.open_trade(i) {
            roll(t);
            i += 1;
        }
        let mut i = 0;
        while let Some(t) = b.closed_trade(i) {
            roll(t);
            let p = pct(t.pnl_1e12, t).unwrap_or(0);
            all = all.saturating_add(p);
            if t.pnl_1e12 > 0 {
                win = win.saturating_add(p);
            } else if t.pnl_1e12 < 0 {
                loss = loss.saturating_add(p);
            }
            i += 1;
        }
        let v = b.view();
        assert_eq!(v.max_runup_pct_1e6, ru, "A9 maximum");
        let dd = f64::from_bits(v.max_drawdown_pct_f64_bits);
        assert!(dd >= 0.0 && dd.to_bits() >> 63 == 0, "CAR-R33K-1: {dd}");
        assert_eq!(
            (v.sum_trade_pct_1e6, v.sum_win_pct_1e6, v.sum_loss_pct_1e6),
            (all, win, loss),
            "A9 sums"
        );
    }

    /// `strategy.max_drawdown_percent` as the view lends it (CAR-R33K-1).
    fn mddp(b: &Book) -> f64 {
        f64::from_bits(b.view().max_drawdown_pct_f64_bits)
    }

    /// `pin:04-max-dd-pct-equity-f64` written out: `(peak − ((ic + np) + op)) / peak * 100`.
    fn law(peak: f64, np: f64, op: f64) -> f64 {
        let eq = (100_000.0 + np) + op;
        (peak - eq) / peak * 100.0
    }

    /// A9 by hand: long 2 @ 100 marked 90 then 110 (run-up $20 on a $200 base: 10 %), closed @ 105
    /// (+$10: +5 %); short 1 @ 200 marked 210, closed @ 204 (−$4: −2 %). Run-up maximum 10 %;
    /// Σ all +3 %, winners +5 %, losers −2 %. The drawdown percent is CAR-R33K-1's equity-curve
    /// law on $100 000: $20 below the $100 000 peak at 90 (0.02 %) — the short's $10 below the
    /// $100 010 closed-equity peak is less. `reset` zeroes.
    #[test]
    fn a9_percent_aggregates_follow_the_records() {
        let mut b = Book::new(CAP, PV, false);
        let v = b.view();
        assert_eq!(
            (
                v.max_drawdown_pct_f64_bits,
                v.max_runup_pct_1e6,
                v.sum_trade_pct_1e6
            ),
            (0, 0, 0)
        );
        b.apply_fill(qty(2), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.mark(px(90));
        let dd = law(100_000.0, 0.0, (90.0 - 100.0) * 2.0);
        assert_eq!(mddp(&b).to_bits(), dd.to_bits(), "an open trade counts");
        b.mark(px(110));
        b.apply_fill(-qty(2), px(105), 0, &close(2, 0), 1, 0, 0)
            .unwrap();
        b.apply_fill(-qty(1), px(200), 0, &src(3), 2, 0, 0).unwrap();
        b.mark(px(210));
        assert!(law(100_010.0, 10.0, -10.0) < dd);
        b.apply_fill(qty(1), px(204), 0, &close(4, 0), 3, 0, 0)
            .unwrap();
        let v = *b.view();
        assert_eq!(
            (v.max_drawdown_pct_f64_bits, v.max_runup_pct_1e6),
            (dd.to_bits(), 10_000_000)
        );
        assert_eq!(
            (v.sum_trade_pct_1e6, v.sum_win_pct_1e6, v.sum_loss_pct_1e6),
            (3_000_000, 5_000_000, -2_000_000)
        );
        check(&b);
        b.reset(CAP, PV, false);
        assert_eq!(*b.view(), *Book::new(CAP, PV, false).view());
        // `reset` restores the peak to the initial capital too.
        b.apply_fill(qty(1), px(100), 0, &src(5), 4, 0, 0).unwrap();
        b.mark(px(97));
        assert_eq!(mddp(&b).to_bits(), law(100_000.0, 0.0, -3.0).to_bits());
    }

    /// CAR-R33K-1 (`pin:04-max-dd-pct-equity-f64`): the peak is the closed equity `ic + NP` —
    /// open profit never raises it (a long marked +50 then +20 is no drawdown) — taken after a
    /// close (+30: peak 100 030), and the fold runs at every mark, `(peak − eq) / peak * 100`.
    #[test]
    fn max_dd_pct_peak_is_closed_equity_open_profit_never_raises_it() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.mark(px(150));
        b.mark(px(120));
        assert_eq!(mddp(&b).to_bits(), 0f64.to_bits(), "no drawdown below ic");
        b.apply_fill(-qty(1), px(130), 0, &close(2, 0), 1, 0, 0)
            .unwrap();
        assert_eq!(mddp(&b).to_bits(), 0f64.to_bits());
        b.apply_fill(qty(1), px(130), 0, &src(3), 2, 0, 0).unwrap();
        b.mark(px(100));
        let dd = law(100_030.0, 30.0, -30.0);
        assert_eq!(mddp(&b).to_bits(), dd.to_bits());
        // A marked-equity peak (100 050 at 150) would give 0.04998…, not this.
        assert!(dd < 50.0 / 100_050.0 * 100.0);
        // A smaller drawdown keeps the maximum.
        b.mark(px(110));
        assert_eq!(mddp(&b).to_bits(), dd.to_bits());
        check(&b);
    }

    /// CAR-R33K-1: a loss leaves the peak where it was (E118 trade 2: 1 859.2 below the initial
    /// capital, not below `ic + NP`); the closing fill's own point folds (`eq = ic + NP`, flat);
    /// the next trade's drawdown adds to the closed loss.
    #[test]
    fn max_dd_pct_after_a_loss_measures_from_the_old_peak() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(-qty(1), px(90), 0, &close(2, 0), 1, 0, 0)
            .unwrap();
        assert_eq!(mddp(&b).to_bits(), law(100_000.0, -10.0, 0.0).to_bits());
        b.apply_fill(qty(1), px(90), 0, &src(3), 2, 0, 0).unwrap();
        b.mark(px(85));
        assert_eq!(mddp(&b).to_bits(), law(100_000.0, -10.0, -5.0).to_bits());
        check(&b);
    }

    /// `pin:04-max-dd-pct-uncaptured-provisional` — a short's open profit carries its sign (the
    /// literal law; no short is captured): short 1 @ 200 marked 210 is $10 below the peak.
    #[test]
    fn max_dd_pct_short_follows_the_literal_law() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(-qty(1), px(200), 0, &src(1), 0, 0, 0).unwrap();
        b.mark(px(190));
        assert_eq!(mddp(&b).to_bits(), 0f64.to_bits());
        b.mark(px(210));
        assert_eq!(mddp(&b).to_bits(), law(100_000.0, 0.0, -10.0).to_bits());
        check(&b);
    }

    /// `pin:04-max-dd-pct-uncaptured-provisional` — fees and a reversal (uncaptured): the peak is
    /// taken once per closing fill, after its closed slices and before the opening part. Long 1 @
    /// 100, reversed by −2 @ 110 (fee $2, $1 per half): the close nets 10 − 1 = 9 (peak 100 009);
    /// the new short's $1 entry fee is paid after it, so the fill's own point is
    /// `eq = 100 000 + 8`, 1 below the peak (a peak taken after the whole fill would read 0). Two
    /// open trades (oldest first) sum their open profits.
    #[test]
    fn max_dd_pct_reversal_with_fees_and_several_trades() {
        let usd = 1_000_000_000_000_i128;
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(-qty(2), px(110), 2 * usd, &src(2), 1, 0, 0)
            .unwrap();
        assert_eq!(b.view().position_size_1e6, -qty(1));
        assert_eq!(b.open_trade(0).map(|t| t.fee_1e12), Some(usd));
        let dd = law(100_009.0, 8.0, 0.0);
        assert!(dd > 0.0);
        assert_eq!(mddp(&b).to_bits(), dd.to_bits());
        // Several open trades: two longs, the open profit summed oldest first from +0.0.
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(qty(3), px(102), 0, &src(2), 0, 0, 0).unwrap();
        b.mark(px(95));
        let op = (0.0 + (95.0 - 100.0) * 1.0) + (95.0 - 102.0) * 3.0;
        assert_eq!(mddp(&b).to_bits(), law(100_000.0, 0.0, op).to_bits());
        check(&b);
    }

    #[test]
    fn open_then_close_books_exact_pnl() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(3), px(100), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(-qty(3), 101_234_567, 0, &close(2, 0), 1, 0, 0)
            .unwrap();
        assert_eq!(b.view().netprofit_1e12, value_1e12(1_234_567, qty(3), PV));
        assert_eq!(b.view().position_size_1e6, 0);
        check(&b);
        // A short round trip with point value 50.
        let mut b = Book::new(CAP, 50 * PV, false);
        b.apply_fill(-qty(2), px(5000), 0, &src(1), 0, 0, 0)
            .unwrap();
        b.apply_fill(qty(2), px(4990), 0, &close(2, 0), 1, 0, 0)
            .unwrap();
        assert_eq!(b.view().netprofit_1e12, value_1e12(px(10), qty(2), 50 * PV));
        check(&b);
    }

    #[test]
    fn position_equals_sum_of_open_trades() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(5), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(qty(10), px(11), 0, &src(2), 0, 0, 0).unwrap();
        b.apply_fill(-qty(7), px(12), 0, &close(3, 0), 0, 0, 0)
            .unwrap();
        assert_eq!(b.view().position_size_1e6, qty(8));
        assert_eq!(b.view().opentrades, 1);
        check(&b);
    }

    #[test]
    fn avg_px_is_zero_exactly_when_flat() {
        let mut b = Book::new(CAP, PV, false);
        assert_eq!(b.view().position_avg_price_1e6, 0);
        b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
        assert_eq!(b.view().position_avg_price_1e6, px(10));
        b.apply_fill(-qty(1), px(12), 0, &close(2, 0), 0, 0, 0)
            .unwrap();
        assert_eq!(b.view().position_avg_price_1e6, 0);
        check(&b);
    }

    #[test]
    fn avg_px_excludes_commission() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(10), 7_000_000_000_000, &src(1), 0, 0, 0)
            .unwrap();
        b.apply_fill(qty(3), px(14), 9_000_000_000_000, &src(2), 0, 0, 0)
            .unwrap();
        assert_eq!(b.view().position_avg_price_1e6, px(13));
        check(&b);
    }

    #[test]
    fn equity_is_capital_plus_net_plus_open() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(2), px(100), 1_000_000_000_000, &src(1), 0, 0, 0)
            .unwrap();
        b.mark(px(90));
        let v = *b.view();
        // `pin:05-openprofit-fees` (r33d, TV E114): open profit is gross; the paid entry fee is in
        // `netprofit`; equity carries it once.
        assert_eq!(v.openprofit_1e12, value_1e12(-px(10), qty(2), PV));
        assert_eq!(v.netprofit_1e12, -1_000_000_000_000);
        assert_eq!(
            v.equity_1e12,
            v.initial_capital_1e12 + value_1e12(-px(10), qty(2), PV) - 1_000_000_000_000
        );
        assert_eq!(v.max_drawdown_1e12, 21 * 1_000_000_000_000);
        check(&b);
    }

    #[test]
    fn netprofit_is_gross_profit_minus_gross_loss() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(-qty(1), px(15), 0, &close(2, 0), 0, 0, 0)
            .unwrap();
        b.apply_fill(qty(1), px(10), 0, &src(3), 0, 0, 0).unwrap();
        b.apply_fill(-qty(1), px(8), 0, &close(4, 0), 0, 0, 0)
            .unwrap();
        b.apply_fill(qty(1), px(10), 0, &src(5), 0, 0, 0).unwrap();
        b.apply_fill(-qty(1), px(10), 0, &close(6, 0), 0, 0, 0)
            .unwrap();
        let v = b.view();
        assert_eq!(
            (v.wintrades, v.losstrades, v.eventrades, v.closedtrades),
            (1, 1, 1, 3)
        );
        assert_eq!(v.grossprofit_1e12, 5 * 1_000_000_000_000);
        assert_eq!(v.grossloss_1e12, 2 * 1_000_000_000_000);
        check(&b);
    }

    #[test]
    fn zero_qty_fill_is_a_no_op() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
        let before = *b.view();
        b.apply_fill(0, px(99), 5, &src(2), 9, 9, 0).unwrap();
        assert_eq!(*b.view(), before);
        assert_eq!(b.fills(), 1);
    }

    #[test]
    fn fifo_closes_the_oldest_even_when_another_id_is_named() {
        // Buy1 5, Buy2 10, close("Buy2") under FIFO: the 10-contract close takes Buy1's 5 first.
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(5), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(qty(10), px(11), 0, &src(2), 0, 0, 0).unwrap();
        b.apply_fill(-qty(10), px(12), 0, &close(3, 2), 1, 0, 0)
            .unwrap();
        assert_eq!(
            b.closed_trade(0).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(1), qty(5)))
        );
        assert_eq!(
            b.closed_trade(1).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(2), qty(5)))
        );
        assert_eq!(
            b.open_trade(0).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(2), qty(5)))
        );
        check(&b);
    }

    #[test]
    fn close_any_closes_the_named_trade_first() {
        let mut b = Book::new(CAP, PV, true);
        b.apply_fill(qty(5), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(qty(10), px(11), 0, &src(2), 0, 0, 0).unwrap();
        b.apply_fill(qty(2), px(9), 0, &src(4), 0, 0, 0).unwrap();
        b.apply_fill(-qty(12), px(12), 0, &close(3, 2), 1, 0, 0)
            .unwrap();
        // Buy2's 10 first, then FIFO: Buy1's 2 of 5.
        assert_eq!(
            b.closed_trade(0).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(2), qty(10)))
        );
        assert_eq!(
            b.closed_trade(1).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(1), qty(2)))
        );
        assert_eq!(
            b.open_trade(0).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(1), qty(3)))
        );
        assert_eq!(
            b.open_trade(1).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(4), qty(2)))
        );
        assert_eq!(b.view().position_entry_name, StrKey(1));
        check(&b);
    }

    #[test]
    fn partial_close_splits_the_trade_and_its_entry_fee() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(4), px(10), 8_000_000_000_000, &src(1), 0, 0, 0)
            .unwrap();
        b.apply_fill(-qty(1), px(10), 2_000_000_000_000, &close(2, 0), 1, 0, 0)
            .unwrap();
        let c = *b.closed_trade(0).unwrap();
        assert_eq!(c.qty_1e6, qty(1));
        // A quarter of the entry fee plus the whole exit fee.
        assert_eq!(c.fee_1e12, 4_000_000_000_000);
        assert_eq!(c.pnl_1e12, -4_000_000_000_000);
        assert_eq!(b.open_trade(0).map(|t| t.fee_1e12), Some(6_000_000_000_000));
        check(&b);
    }

    #[test]
    fn fee_split_sums_exactly() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(3), px(10), 1_000_000_000_001, &src(1), 0, 0, 0)
            .unwrap();
        b.apply_fill(qty(7), px(10), 3, &src(2), 0, 0, 0).unwrap();
        // One fill closes 3 + 7 and opens 1: three slices of an odd fee.
        b.apply_fill(-qty(11), px(10), 1_000_000_000_007, &src(3), 0, 0, 0)
            .unwrap();
        check(&b);
    }

    #[test]
    fn reversal_is_one_fill_of_qty_plus_position() {
        // 15 long → entry short 5: one transaction of 20.
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(15), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(-qty(20), px(11), 0, &src(2), 1, 0, 0).unwrap();
        assert_eq!(b.view().position_size_1e6, -qty(5));
        assert_eq!(b.view().closedtrades, 1);
        assert_eq!(
            b.open_trade(0).map(|t| (t.entry_id, t.qty_1e6)),
            Some((StrKey(2), -qty(5)))
        );
        assert_eq!(b.fills(), 2);
        check(&b);
    }

    #[test]
    fn closed_and_open_trades_carry_their_flags() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(2), px(10), 0, &src(1), 0, 0, 0).unwrap();
        b.apply_fill(
            -qty(1),
            px(10),
            0,
            &close(2, 0),
            0,
            0,
            trade_flag::LIQUIDATION,
        )
        .unwrap();
        b.apply_fill(
            -qty(1),
            px(10),
            0,
            &close(3, 0),
            0,
            0,
            trade_flag::RISK_FLATTEN,
        )
        .unwrap();
        assert_eq!(
            b.closed_trade(0).map(|t| t.flags),
            Some(trade_flag::PNL_NET | trade_flag::LIQUIDATION)
        );
        assert_eq!(
            b.closed_trade(1).map(|t| (t.flags, t.exit_id, t.exit_bar)),
            Some((trade_flag::PNL_NET | trade_flag::RISK_FLATTEN, StrKey(3), 0))
        );
        check(&b);
    }

    #[test]
    fn the_9001st_closed_trade_trims_the_oldest() {
        let mut b = Book::new(CAP, PV, false);
        let mut k = 0_u32;
        while k < 9001 {
            b.apply_fill(qty(1), px(10), 0, &src(1), k, 0, 0).unwrap();
            b.apply_fill(-qty(1), px(10), 0, &close(2, 0), k, 0, 0)
                .unwrap();
            k += 1;
        }
        let v = b.view();
        assert_eq!((v.closedtrades, v.closed_retained), (9001, 9000));
        // first_index = closedtrades − closed_retained = 1: the oldest retained exited on bar 1.
        assert_eq!(v.closedtrades - v.closed_retained, 1);
        assert_eq!(b.closed_trade(0).map(|t| t.exit_bar), Some(1));
        assert!(b.closed_trade(9000).is_none());
    }

    #[test]
    fn open_trade_is_a_borrow_not_a_copy() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
        let a = b.open_trade(0).map(core::ptr::from_ref);
        let c = b.open_trade(0).map(core::ptr::from_ref);
        assert!(a.is_some() && a == c);
    }

    #[test]
    fn lot_underflow_is_an_internal_defect() {
        let mut b = Book::new(CAP, PV, false);
        b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
        // Desync the aggregate from the lots (unreachable while I7 holds).
        b.position_1e6 = qty(5);
        let r = b.apply_fill(-qty(3), px(10), 0, &close(2, 0), 0, 0, 0);
        assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::LotUnderflow)));
        // A close that would open a trade contradicts its order.
        let mut b = Book::new(CAP, PV, false);
        let r = b.apply_fill(-qty(1), px(10), 0, &close(2, 0), 0, 0, 0);
        assert_eq!(r, Err(BrokerFault::Internal(InternalDefect::BookDesync)));
        assert_eq!(b.view().opentrades, 0);
    }

    #[test]
    fn lot_table_full_is_a_script_refusal() {
        let mut b = Book::new(CAP, PV, false);
        let mut k = 0;
        while k < 128 {
            b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0).unwrap();
            k += 1;
        }
        let before = *b.view();
        let r = b.apply_fill(qty(1), px(10), 0, &src(1), 0, 0, 0);
        assert_eq!(r, Err(BrokerFault::Script(ScriptRefusal::LotTableFull)));
        assert_eq!(*b.view(), before);
        // A reversal still fits: it empties the table first.
        b.apply_fill(-qty(129), px(10), 0, &src(2), 0, 0, 0)
            .unwrap();
        assert_eq!(b.view().position_size_1e6, -qty(1));
        check(&b);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        #[test]
        fn prop_position_equals_open_sum(
            steps in proptest::collection::vec((-50_i64..=50, 1_i64..=200, 0_i64..=1_000), 1..60),
            any in proptest::bool::ANY,
        ) {
            let mut b = Book::new(CAP, PV, any);
            let mut i = 0;
            while i < steps.len() {
                let (q, p, f) = steps[i];
                let s = FillSrc { named_entry: StrKey(u64::try_from(i % 3).unwrap_or(0)), ..src(
                    u64::try_from(i % 3).unwrap_or(0) + 1,
                ) };
                let r = b.apply_fill(qty(q), px(p), i128::from(f) * 1_000_000, &s, 0, 0, 0);
                prop_assert!(r.is_ok() || r == Err(BrokerFault::Script(ScriptRefusal::LotTableFull)));
                check(&b);
                i += 1;
            }
            let mut sum = 0;
            let mut k = 0;
            while let Some(t) = b.open_trade(k) {
                sum += t.qty_1e6;
                k += 1;
            }
            prop_assert_eq!(b.view().position_size_1e6, sum);
        }
    }
}
