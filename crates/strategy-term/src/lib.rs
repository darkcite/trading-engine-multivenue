// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `strategy-term` — the TERM member (TERM plan v2.1 §4.1; slots 12–15,
//! ruling O-T17; paper only on this branch, O-T18).
//!
//! The member owns the consumer ends of the door's rings. On every timer:
//! 1. **Reducing first** — every command on the reduce ring: cancel,
//!    cancel-all, flatten (cancel leg, then a reduce-only IoC exit at the
//!    touch ± slip, refused on a stale touch or a halted slot), halt, the
//!    heartbeat and the door's `Disconnect`. These skip the pre-gate and are
//!    never dropped (T-L4: a cancel is never blocked).
//! 2. **Growing** — at most `GROW_PER_TIMER` place/modify commands through
//!    the pre-gate: halt latch, age (`recv_ns`), a fresh engine touch, the
//!    band around the engine's mid, the venue minimum, the per-order
//!    notional cap, the per-instrument position cap, one rate bucket, and
//!    the open-order capacity.
//! 3. **Watchdog** — once the terminal has sent a heartbeat, losing it for
//!    `watchdog_ns` cancels every resting order (R-T15).
//!
//! **HALT is slot-scoped.** It latches this member only; it never produces
//! `AiCmdKind::HaltRequest` and touches no other slot (tested). The live
//! `exec.HALT` line of R-T6 is owed with live arming, not on this branch.
//!
//! Hot path (`on_depth`, `on_timer`): fixed arrays, no allocation after
//! `new`, no `format!`.

use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_time::NsTs;
use core_types::{
    AiCmd, CancelReq, DepthTopK, Fill, Order, OrderEvent, Price, Qty, Side, Signal, SymbolId, Tick, VenueId,
    ORDER_EVENT_CANCELED, ORDER_EVENT_FILLED, ORDER_EVENT_REJECTED, ORDER_FLAG_POST_ONLY, ORDER_FLAG_REDUCE_ONLY,
};
use ingress_term::{
    is_term_slot, CmdKind, EvtKind, EvtTx, GrowRx, ReduceRx, Reason, TermCmd, TermEvt, TermIn, OKIND_IOC,
    FLAG_POST_ONLY, FLAG_REDUCE_ONLY,
};
use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError};

pub const MAX_SYMS: usize = 32;
pub const MAX_OPEN: usize = 128;
pub const GROW_PER_TIMER: usize = 32;
pub const TIMER_NS: u64 = 50_000_000;

/// The pre-gate's numbers for one slot (paper: equal to the proposals in
/// TERM plan §4.3; live caps are the operator's call, R-T5).
#[derive(Copy, Clone, Debug)]
pub struct TermParams {
    pub venue: VenueId,
    /// Max distance of a limit from the engine's mid, basis points.
    pub band_bps: i64,
    /// Venue minimum notional, ×1e6.
    pub min_notional_1e6: i64,
    pub max_order_notional_1e6: i64,
    /// Per-instrument |position| cap, ×1e6 contracts.
    pub max_pos_1e6: i64,
    /// Rate bucket: capacity and refill per second (commands + children).
    pub rate_burst: u32,
    pub rate_per_s: u32,
    /// A growing command older than this at the member is refused.
    pub max_age_ns: u64,
    /// A touch older than this is stale (no growing, no flatten exit).
    pub touch_stale_ns: u64,
    pub watchdog_ns: u64,
    /// Flatten's exit crosses the touch by this many bps.
    pub flatten_slip_bps: i64,
    /// TTL of a resting TERM order (`max_rest_s`, T-L7).
    pub max_rest_ns: u64,
}

impl TermParams {
    pub const fn paper_hl() -> TermParams {
        TermParams {
            venue: VenueId::Hyperliquid,
            band_bps: 200,
            min_notional_1e6: 10_000_000,
            max_order_notional_1e6: 12_000_000,
            max_pos_1e6: 1_000_000_000,
            rate_burst: 5,
            rate_per_s: 2,
            max_age_ns: 1_000_000_000,
            touch_stale_ns: 2_000_000_000,
            watchdog_ns: 3_000_000_000,
            flatten_slip_bps: 20,
            max_rest_ns: 300_000_000_000,
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct Touch {
    sym: SymbolId,
    bid: i64,
    ask: i64,
    ts: NsTs,
    used: bool,
}

#[derive(Copy, Clone, Debug, Default)]
struct Pos {
    sym: SymbolId,
    qty: i64,
    used: bool,
}

#[derive(Copy, Clone, Debug)]
struct Open {
    oid: u64,
    sym: SymbolId,
    side: Side,
    qty: i64,
    nonce: u64,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct TermCounters {
    pub reduce: u64,
    pub grow: u64,
    pub sent: u64,
    pub refused: u64,
    pub evt_dropped: u64,
    pub watchdog_fired: u64,
    pub fills: u64,
}

pub struct TermStrategy {
    slot: u8,
    p: TermParams,
    reduce: Option<ReduceRx>,
    grow: Option<GrowRx>,
    evt_tx: Option<EvtTx>,
    halted: bool,
    /// When the door last reported the terminal gone; growing commands
    /// received before it are stale.
    disconnected_at: NsTs,
    touch: [Touch; MAX_SYMS],
    pos: [Pos; MAX_SYMS],
    open: [Option<Open>; MAX_OPEN],
    seq: u64,
    beat_ns: NsTs,
    beat_seen: bool,
    tokens_milli: u64,
    refill_ns: NsTs,
    pub counters: TermCounters,
    emitted: u64,
}

impl TermStrategy {
    /// A member on TERM slot `slot` (12–15). Panics on any other slot: that
    /// is a boot-time wiring error, never a runtime state.
    pub fn new(slot: u8, p: TermParams) -> TermStrategy {
        assert!(is_term_slot(slot), "TERM members live on slots 12–15 (O-T17)");
        TermStrategy {
            slot,
            p,
            reduce: None,
            grow: None,
            evt_tx: None,
            halted: false,
            disconnected_at: 0,
            touch: [Touch::default(); MAX_SYMS],
            pos: [Pos::default(); MAX_SYMS],
            open: [None; MAX_OPEN],
            seq: 0,
            beat_ns: 0,
            beat_seen: false,
            tokens_milli: p.rate_burst as u64 * 1000,
            refill_ns: 0,
            counters: TermCounters::default(),
            emitted: 0,
        }
    }

    /// Hand the member its ends of the door's rings (boot wiring).
    pub fn attach(&mut self, reduce: ReduceRx, grow: GrowRx, evt: EvtTx) {
        self.reduce = Some(reduce);
        self.grow = Some(grow);
        self.evt_tx = Some(evt);
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }
    pub fn is_halted(&self) -> bool {
        self.halted
    }
    pub fn open_count(&self) -> usize {
        self.open.iter().filter(|o| o.is_some()).count()
    }
    pub fn position(&self, sym: SymbolId) -> i64 {
        self.pos.iter().find(|p| p.used && p.sym == sym).map_or(0, |p| p.qty)
    }

    // ---- small fixed-array maps ---------------------------------------------------

    fn touch_of(&self, sym: SymbolId, now: NsTs) -> Option<(i64, i64)> {
        let t = self.touch.iter().find(|t| t.used && t.sym == sym)?;
        (now.saturating_sub(t.ts) <= self.p.touch_stale_ns && t.bid > 0 && t.ask > t.bid).then_some((t.bid, t.ask))
    }

    fn pos_slot(&mut self, sym: SymbolId) -> Option<&mut Pos> {
        let k = self.pos.iter().position(|p| p.used && p.sym == sym).or_else(|| self.pos.iter().position(|p| !p.used))?;
        let p = &mut self.pos[k];
        if !p.used {
            *p = Pos { sym, qty: 0, used: true };
        }
        Some(p)
    }

    fn next_oid(&mut self) -> u64 {
        // E-9: the venue cloid's slot bits are the exec host's (from
        // Order.strategy_id). Here: slot in 56..64, a sequence in 32..56,
        // and instance 0 in the low 32 bits (perps have no instance; the
        // HIP-4 outcome roll is out of TERM v1's scope).
        self.seq = (self.seq + 1) & 0x00FF_FFFF;
        ((self.slot as u64) << 56) | (self.seq << 32)
    }

    // ---- events back to the terminal ------------------------------------------------

    fn evt(&mut self, e: TermEvt) {
        if let Some(tx) = self.evt_tx.as_mut() {
            if !tx.try_push_ref(&e) {
                self.counters.evt_dropped += 1;
            }
        }
    }

    fn refuse(&mut self, c: &TermCmd, r: Reason, now: NsTs) {
        self.counters.refused += 1;
        self.evt(TermEvt::refused(c, r, now));
    }

    // ---- reducing path ------------------------------------------------------------------

    fn cancel_where<C: Ctx>(&mut self, ctx: &mut C, now: NsTs, sym: Option<SymbolId>) -> u32 {
        let mut n = 0;
        for k in 0..MAX_OPEN {
            let Some(o) = self.open[k] else { continue };
            if sym.is_some_and(|s| s != o.sym) {
                continue;
            }
            // T-L4: a cancel is never blocked; a refused request is retried
            // by the next cancel-all or the watchdog, the order stays known.
            if ctx.cancel(CancelReq::new(now, self.p.venue, o.sym, o.oid)).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn on_reduce<C: Ctx>(&mut self, t: TermIn, ctx: &mut C, now: NsTs) {
        self.counters.reduce += 1;
        let c = t.cmd;
        if c.kind != CmdKind::Disconnect as u8 && c.slot != self.slot {
            return; // another TERM slot's command (the door is shared)
        }
        match CmdKind::from_u8(c.kind) {
            Some(CmdKind::Beat) => {
                self.beat_ns = now;
                self.beat_seen = true;
            }
            Some(CmdKind::Cancel) => {
                match self.open.iter().flatten().find(|o| o.oid == c.target_oid).copied() {
                    Some(o) => {
                        if ctx.cancel(CancelReq::new(now, self.p.venue, o.sym, o.oid)).is_ok() {
                            self.evt(TermEvt { kind: EvtKind::Sent as u8, slot: self.slot, sym: o.sym, nonce: c.nonce, client_oid: o.oid, ts_ns: now, extra: 1, ..TermEvt::default() });
                        }
                    }
                    None => self.refuse(&c, Reason::UnknownOrder, now),
                }
            }
            Some(CmdKind::CancelAll) => {
                let n = self.cancel_where(ctx, now, Some(c.sym));
                self.evt(TermEvt { kind: EvtKind::Sent as u8, slot: self.slot, sym: c.sym, nonce: c.nonce, ts_ns: now, extra: n as i64, ..TermEvt::default() });
            }
            Some(CmdKind::Flatten) => {
                self.cancel_where(ctx, now, Some(c.sym));
                let q = self.position(c.sym);
                if q == 0 {
                    return;
                }
                if self.halted {
                    return self.refuse(&c, Reason::Halted, now);
                }
                let Some((bid, ask)) = self.touch_of(c.sym, now) else {
                    return self.refuse(&c, Reason::NoTouch, now);
                };
                let (side, px) = if q > 0 { (Side::Ask, bid - bid * self.p.flatten_slip_bps / 10_000) } else { (Side::Bid, ask + ask * self.p.flatten_slip_bps / 10_000) };
                let oid = self.next_oid();
                let mut o = Order::new(now, self.p.venue, c.sym, side, ORDER_KIND_IOC, Price::from_raw(px), Qty::from_raw(q.abs()), oid);
                o.flags = ORDER_FLAG_REDUCE_ONLY;
                self.send(ctx, o, &c, now);
            }
            Some(CmdKind::Halt) => {
                self.halted = true;
                let n = self.cancel_where(ctx, now, None);
                self.evt(TermEvt { kind: EvtKind::Halted as u8, slot: self.slot, nonce: c.nonce, ts_ns: now, extra: n as i64, ..TermEvt::default() });
            }
            Some(CmdKind::Disconnect) => {
                // R-T15: a lost terminal cancels; growing commands still on
                // the grow ring are discarded at the next drain (their
                // recv_ns predates this).
                let n = self.cancel_where(ctx, now, None);
                self.beat_seen = false;
                self.disconnected_at = now;
                self.evt(TermEvt { kind: EvtKind::Watchdog as u8, slot: self.slot, ts_ns: now, extra: n as i64, ..TermEvt::default() });
            }
            _ => {}
        }
    }

    // ---- growing path -------------------------------------------------------------------

    fn rate_take(&mut self, now: NsTs) -> bool {
        let cap = self.p.rate_burst as u64 * 1000;
        if self.refill_ns == 0 {
            self.refill_ns = now;
        }
        let dt = now.saturating_sub(self.refill_ns);
        self.tokens_milli = (self.tokens_milli + dt * self.p.rate_per_s as u64 / 1_000_000).min(cap);
        self.refill_ns = now;
        if self.tokens_milli >= 1000 {
            self.tokens_milli -= 1000;
            true
        } else {
            false
        }
    }

    /// The pre-gate shared by place and modify. `delta_pos` is the signed
    /// position change if fully filled.
    #[allow(clippy::too_many_arguments)]
    fn pregate(&mut self, c: &TermCmd, t: &TermIn, now: NsTs, sym: SymbolId, px: i64, qty: i64, delta_pos: i64, reduce_only: bool) -> Result<(), Reason> {
        if self.halted {
            return Err(Reason::Halted);
        }
        if now.saturating_sub(t.recv_ns) > self.p.max_age_ns || t.recv_ns < self.disconnected_at {
            return Err(Reason::Stale);
        }
        let (bid, ask) = self.touch_of(sym, now).ok_or(Reason::NoTouch)?;
        let mid = (bid + ask) / 2;
        if (px - mid).abs() as i128 * 10_000 > self.p.band_bps as i128 * mid as i128 {
            return Err(Reason::Band);
        }
        let notional = (px as i128 * qty as i128 / 1_000_000) as i64;
        if notional < self.p.min_notional_1e6 && !reduce_only {
            return Err(Reason::VenueMin);
        }
        if notional > self.p.max_order_notional_1e6 {
            return Err(Reason::Notional);
        }
        let cur = self.position(sym);
        let after = cur + delta_pos;
        if reduce_only && after.abs() > cur.abs() {
            return Err(Reason::Position);
        }
        if after.abs() > self.p.max_pos_1e6 && after.abs() > cur.abs() {
            return Err(Reason::Position);
        }
        if !self.open.iter().any(|o| o.is_none()) {
            return Err(Reason::Submit);
        }
        if !self.rate_take(now) {
            return Err(Reason::Rate);
        }
        let _ = c;
        Ok(())
    }

    fn send<C: Ctx>(&mut self, ctx: &mut C, o: Order, c: &TermCmd, now: NsTs) {
        match ctx.submit(o) {
            Ok(()) => {
                if let Some(k) = self.open.iter().position(|x| x.is_none()) {
                    self.open[k] = Some(Open { oid: o.client_oid, sym: o.sym, side: o.side, qty: o.qty.raw(), nonce: c.nonce });
                }
                self.counters.sent += 1;
                self.emitted += 1;
                self.evt(TermEvt { kind: EvtKind::Sent as u8, slot: self.slot, reason: 0, side: o.side as u8, sym: o.sym, nonce: c.nonce, client_oid: o.client_oid, px: o.px.raw(), qty: o.qty.raw(), ts_ns: now, extra: 0 });
            }
            Err(_) => self.refuse(c, Reason::Submit, now),
        }
    }

    fn on_grow<C: Ctx>(&mut self, t: TermIn, ctx: &mut C, now: NsTs) {
        self.counters.grow += 1;
        let c = t.cmd;
        if c.slot != self.slot {
            return;
        }
        match CmdKind::from_u8(c.kind) {
            Some(CmdKind::Place) => {
                let side = if c.side == 0 { Side::Bid } else { Side::Ask };
                let reduce_only = c.flags & FLAG_REDUCE_ONLY != 0;
                let d = if side == Side::Bid { c.qty } else { -c.qty };
                if let Err(r) = self.pregate(&c, &t, now, c.sym, c.px, c.qty, d, reduce_only) {
                    return self.refuse(&c, r, now);
                }
                let kind = if c.okind == OKIND_IOC { ORDER_KIND_IOC } else { ORDER_KIND_MAKER };
                let oid = self.next_oid();
                let mut o = Order::new(now, self.p.venue, c.sym, side, kind, Price::from_raw(c.px), Qty::from_raw(c.qty), oid);
                o.flags = (if reduce_only { ORDER_FLAG_REDUCE_ONLY } else { 0 }) | (if c.flags & FLAG_POST_ONLY != 0 { ORDER_FLAG_POST_ONLY } else { 0 });
                if kind == ORDER_KIND_MAKER {
                    o = o.with_ttl_ns(self.p.max_rest_ns);
                }
                self.send(ctx, o, &c, now);
            }
            Some(CmdKind::Modify) => {
                let Some(k) = self.open.iter().position(|o| o.is_some_and(|o| o.oid == c.target_oid)) else {
                    return self.refuse(&c, Reason::UnknownOrder, now);
                };
                let Some(prev) = self.open[k] else { return };
                let signed = |q: i64| if prev.side == Side::Bid { q } else { -q };
                // Only the growth beyond the order being replaced counts.
                let d = signed(c.qty) - signed(prev.qty);
                if let Err(r) = self.pregate(&c, &t, now, prev.sym, c.px, c.qty, d, false) {
                    return self.refuse(&c, r, now);
                }
                let oid = self.next_oid();
                let o = Order::new(now, self.p.venue, prev.sym, prev.side, ORDER_KIND_MAKER, Price::from_raw(c.px), Qty::from_raw(c.qty), oid).with_ttl_ns(self.p.max_rest_ns);
                match ctx.modify(prev.oid, o) {
                    Ok(()) => {
                        // E-7: the original stays until the venue confirms;
                        // here the new id replaces it in our book.
                        self.open[k] = Some(Open { oid, sym: prev.sym, side: prev.side, qty: c.qty, nonce: c.nonce });
                        self.counters.sent += 1;
                        self.evt(TermEvt { kind: EvtKind::Sent as u8, slot: self.slot, reason: 0, side: prev.side as u8, sym: prev.sym, nonce: c.nonce, client_oid: oid, px: c.px, qty: c.qty, ts_ns: now, extra: 2 });
                    }
                    Err(_) => self.refuse(&c, Reason::Submit, now),
                }
            }
            _ => {}
        }
    }

    /// Drain the rings, reducing first, then the watchdog. Public so a host
    /// without the set's timer (tests, a bench) can drive it.
    pub fn service<C: Ctx>(&mut self, ctx: &mut C, now: NsTs) {
        // The consumers are moved out while serving so `self` stays
        // borrowable, and always put back.
        if let Some(mut rr) = self.reduce.take() {
            while let Some(t) = rr.try_pop_ref() {
                let t = *t;
                self.on_reduce(t, ctx, now);
            }
            self.reduce = Some(rr);
        }
        if let Some(mut gr) = self.grow.take() {
            let mut n = 0;
            while n < GROW_PER_TIMER {
                let Some(t) = gr.try_pop_ref() else { break };
                let t = *t;
                self.on_grow(t, ctx, now);
                n += 1;
            }
            self.grow = Some(gr);
        }
        if self.beat_seen && now.saturating_sub(self.beat_ns) > self.p.watchdog_ns {
            let k = self.cancel_where(ctx, now, None);
            self.beat_seen = false;
            self.counters.watchdog_fired += 1;
            self.evt(TermEvt { kind: EvtKind::Watchdog as u8, slot: self.slot, ts_ns: now, extra: k as i64, ..TermEvt::default() });
        }
    }
}

impl StrategyCounters for TermStrategy {
    fn orders_emitted(&self) -> u64 {
        self.emitted
    }
}

impl Strategy for TermStrategy {
    fn on_start<C: Ctx>(&mut self, _ctx: &mut C) -> Result<(), StrategyError> {
        Ok(())
    }
    fn on_tick<C: Ctx>(&mut self, _tick: &Tick, _ctx: &mut C) {}
    fn on_signal<C: Ctx>(&mut self, _signal: &Signal, _ctx: &mut C) {}
    /// Positions follow this slot's fills only.
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C) {
        if fill.strategy_id != self.slot {
            return;
        }
        let d = if fill.side == Side::Bid { fill.qty.raw() } else { -fill.qty.raw() };
        if let Some(p) = self.pos_slot(fill.sym) {
            p.qty += d;
        }
        self.counters.fills += 1;
        let now = ctx.now_ns();
        self.evt(TermEvt { kind: EvtKind::Fill as u8, slot: self.slot, reason: 0, side: fill.side as u8, sym: fill.sym, px: fill.px.raw(), qty: fill.qty.raw(), ts_ns: now, extra: self.position(fill.sym), ..TermEvt::default() });
    }
    /// HALT is slot-scoped: an AI command never moves a TERM slot, and a
    /// TERM halt never becomes an AI command.
    fn on_ai<C: Ctx>(&mut self, _cmd: &AiCmd, _ctx: &mut C) {}
    fn on_depth<C: Ctx>(&mut self, d: &DepthTopK, _ctx: &mut C) {
        if d.venue != self.p.venue as u8 || d.k == 0 {
            return;
        }
        let k = self.touch.iter().position(|t| t.used && t.sym == d.sym).or_else(|| self.touch.iter().position(|t| !t.used));
        if let Some(k) = k {
            self.touch[k] = Touch { sym: d.sym, bid: d.bids[0].px_1e6, ask: d.asks[0].px_1e6, ts: d.ts_ns, used: true };
        }
    }
    fn on_order_event<C: Ctx>(&mut self, e: &OrderEvent, _ctx: &mut C) {
        if e.strategy_id != self.slot {
            return;
        }
        if matches!(e.kind, ORDER_EVENT_FILLED | ORDER_EVENT_CANCELED | ORDER_EVENT_REJECTED) {
            if let Some(k) = self.open.iter().position(|o| o.is_some_and(|o| o.oid == e.client_oid)) {
                let o = self.open[k].take();
                if e.kind != ORDER_EVENT_FILLED {
                    let nonce = o.map_or(0, |o| o.nonce);
                    let reason = if e.kind == ORDER_EVENT_REJECTED { Reason::Submit as u8 } else { 0 };
                    self.evt(TermEvt { kind: EvtKind::Cancelled as u8, slot: self.slot, reason, sym: e.sym, nonce, client_oid: e.client_oid, ts_ns: e.ts_ns, ..TermEvt::default() });
                }
            }
        }
    }
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C) {
        self.service(ctx, now_ns);
    }
    /// T-L7: no TERM order outlives its member.
    fn on_stop<C: Ctx>(&mut self, ctx: &mut C) {
        let now = ctx.now_ns();
        self.cancel_where(ctx, now, None);
    }
    fn timer_period_ns(&self) -> u64 {
        TIMER_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{DepthLevel, DEPTH_K};
    use ingress_term::{rings, EvtRx, ReduceTx, GrowTx, OKIND_LIMIT, SLOT_TERM_HL, SLOT_TERM_BN};
    use strategy_core::SubmitErr;

    #[derive(Default)]
    struct MockCtx {
        now: NsTs,
        submitted: Vec<Order>,
        cancelled: Vec<u64>,
        modified: Vec<(u64, Order)>,
    }
    impl Ctx for MockCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            self.submitted.push(o);
            Ok(())
        }
        fn cancel(&mut self, r: CancelReq) -> Result<(), SubmitErr> {
            self.cancelled.push(r.client_oid);
            Ok(())
        }
        fn modify(&mut self, prev: u64, o: Order) -> Result<(), SubmitErr> {
            self.modified.push((prev, o));
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    const SYM: SymbolId = 7;
    const S: u64 = 1_000_000_000;

    struct H {
        m: TermStrategy,
        rt: ReduceTx,
        gt: GrowTx,
        er: EvtRx,
        ctx: MockCtx,
        nonce: u64,
    }

    fn h() -> H {
        let ((rt, gt, er), (rr, gr, et)) = rings();
        let mut m = TermStrategy::new(SLOT_TERM_HL, TermParams::paper_hl());
        m.attach(rr, gr, et);
        let mut x = H { m, rt, gt, er, ctx: MockCtx { now: 100 * S, ..MockCtx::default() }, nonce: 0 };
        x.depth(78_000_000_000, 78_001_000_000);
        x
    }

    impl H {
        fn depth(&mut self, bid: i64, ask: i64) {
            let mut bids = [DepthLevel::EMPTY; DEPTH_K];
            let mut asks = [DepthLevel::EMPTY; DEPTH_K];
            bids[0].px_1e6 = bid;
            bids[0].qty_1e6 = 1_000_000;
            asks[0].px_1e6 = ask;
            asks[0].qty_1e6 = 1_000_000;
            let mut d = DepthTopK::new(self.ctx.now, VenueId::Hyperliquid, SYM, 0, bids, asks);
            d.k = d.k.max(1);
            self.m.on_depth(&d, &mut self.ctx);
        }
        fn cmd(&mut self, kind: CmdKind) -> TermCmd {
            self.nonce += 1;
            TermCmd { kind: kind as u8, slot: SLOT_TERM_HL, sym: SYM, nonce: self.nonce, ..TermCmd::default() }
        }
        fn grow(&mut self, c: TermCmd) {
            assert!(self.gt.try_push_ref(&TermIn { cmd: c, recv_ns: self.ctx.now }));
        }
        fn reduce(&mut self, c: TermCmd) {
            assert!(self.rt.try_push_ref(&TermIn { cmd: c, recv_ns: self.ctx.now }));
        }
        fn tick(&mut self) -> Vec<TermEvt> {
            let now = self.ctx.now;
            self.m.on_timer(now, &mut self.ctx);
            let mut v = Vec::new();
            while let Some(e) = self.er.try_pop_ref() {
                v.push(*e);
            }
            v
        }
        fn place(&mut self, side: u8, px: i64, qty: i64) -> TermCmd {
            let mut c = self.cmd(CmdKind::Place);
            c.side = side;
            c.okind = OKIND_LIMIT;
            c.px = px;
            c.qty = qty;
            c
        }
    }

    #[test]
    #[should_panic(expected = "slots 12–15")]
    fn refuses_pmjs_slot() {
        let _ = TermStrategy::new(11, TermParams::paper_hl());
    }

    #[test]
    fn a_place_inside_the_gate_is_sent() {
        let mut x = h();
        let c = x.place(0, 78_000_000_000, 150); // 0.00015 BTC ≈ $11.7
        x.grow(c);
        let ev = x.tick();
        assert_eq!(x.ctx.submitted.len(), 1);
        let o = x.ctx.submitted[0];
        assert_eq!((o.side, o.px.raw(), o.qty.raw(), o.kind), (Side::Bid, 78_000_000_000, 150, ORDER_KIND_MAKER));
        assert_eq!((o.client_oid >> 56) as u8, SLOT_TERM_HL);
        assert_eq!(ev[0].kind, EvtKind::Sent as u8);
        assert_eq!(x.m.open_count(), 1);
    }

    #[test]
    fn the_pregate_refuses_each_way() {
        let mut x = h();
        let cases = [
            (x.place(0, 70_000_000_000, 150), Reason::Band),        // 10 % away
            (x.place(0, 78_000_000_000, 10), Reason::VenueMin),     // $0.78
            (x.place(0, 78_000_000_000, 1_000), Reason::Notional),  // $78
        ];
        for (c, want) in cases {
            x.grow(c);
            let ev = x.tick();
            assert_eq!(ev[0].reason, want as u8, "{c:?}");
        }
        // Stale touch.
        x.ctx.now += 5 * S;
        let c = x.place(0, 78_000_000_000, 150);
        x.grow(c);
        assert_eq!(x.tick()[0].reason, Reason::NoTouch as u8);
        assert!(x.ctx.submitted.is_empty());
    }

    #[test]
    fn rate_bucket_limits_bursts() {
        let mut x = h();
        for _ in 0..8 {
            let c = x.place(0, 78_000_000_000, 150);
            x.grow(c);
        }
        let ev = x.tick();
        assert_eq!(x.ctx.submitted.len(), 5, "burst of 5");
        assert_eq!(ev.iter().filter(|e| e.reason == Reason::Rate as u8).count(), 3);
    }

    #[test]
    fn halt_is_slot_scoped_and_reducing_goes_first() {
        let mut x = h();
        let c = x.place(1, 78_001_000_000, 150);
        x.grow(c);
        x.tick();
        // Halt and a new place in the same timer: the halt (reduce ring)
        // runs first, so the place is refused.
        let c = x.place(1, 78_001_000_000, 150);
        x.grow(c);
        let hc = x.cmd(CmdKind::Halt);
        x.reduce(TermCmd { sym: 0, ..hc });
        let ev = x.tick();
        assert!(x.m.is_halted());
        assert_eq!(x.ctx.cancelled.len(), 1, "the resting order is cancelled");
        assert_eq!(ev[0].kind, EvtKind::Halted as u8);
        assert!(ev.iter().any(|e| e.reason == Reason::Halted as u8));
        // Another TERM slot's halt does not touch this one.
        let mut y = h();
        let mut other = y.cmd(CmdKind::Halt);
        other.slot = SLOT_TERM_BN;
        other.sym = 0;
        y.reduce(other);
        y.tick();
        assert!(!y.m.is_halted());
    }

    #[test]
    fn flatten_cancels_then_exits_reduce_only_at_the_touch() {
        let mut x = h();
        let fill = Fill::new(x.ctx.now, SYM, Side::Bid, Price::from_raw(78_000_000_000), Qty::from_raw(150), 0).with_attribution(SLOT_TERM_HL, 0);
        x.m.on_fill(&fill, &mut x.ctx);
        assert_eq!(x.m.position(SYM), 150);
        let c = x.place(0, 77_990_000_000, 150);
        x.grow(c);
        x.tick();
        let f = x.cmd(CmdKind::Flatten);
        x.reduce(f);
        x.tick();
        assert_eq!(x.ctx.cancelled.len(), 1);
        let exit = *x.ctx.submitted.last().unwrap();
        assert_eq!((exit.side, exit.kind, exit.flags & ORDER_FLAG_REDUCE_ONLY, exit.qty.raw()), (Side::Ask, ORDER_KIND_IOC, ORDER_FLAG_REDUCE_ONLY, 150));
        assert!(exit.px.raw() < 78_000_000_000, "crosses the bid by the slip");
        // A fill for another slot never moves this position.
        let other = fill.with_attribution(9, 0);
        x.m.on_fill(&other, &mut x.ctx);
        assert_eq!(x.m.position(SYM), 150);
    }

    #[test]
    fn watchdog_and_disconnect_cancel() {
        let mut x = h();
        let b = x.cmd(CmdKind::Beat);
        x.reduce(TermCmd { sym: 0, ..b });
        let c = x.place(0, 78_000_000_000, 150);
        x.grow(c);
        x.tick();
        x.ctx.now += 2 * S;
        x.depth(78_000_000_000, 78_001_000_000);
        assert!(x.tick().is_empty(), "within the watchdog");
        x.ctx.now += 2 * S;
        let ev = x.tick();
        assert_eq!(ev[0].kind, EvtKind::Watchdog as u8);
        assert_eq!(x.ctx.cancelled.len(), 1);
        // Disconnect: cancels, and a grow received before it is stale.
        let mut y = h();
        let c = y.place(0, 78_000_000_000, 150);
        y.grow(c);
        y.ctx.now += 1;
        y.reduce(ingress_term::listener::disconnect(y.ctx.now).cmd);
        let ev = y.tick();
        assert_eq!(ev[0].kind, EvtKind::Watchdog as u8);
        assert!(y.ctx.submitted.is_empty());
        assert!(ev.iter().any(|e| e.reason == Reason::Stale as u8));
    }

    #[test]
    fn modify_and_cancel_by_id() {
        let mut x = h();
        let c = x.place(0, 78_000_000_000, 150);
        x.grow(c);
        let oid = x.tick()[0].client_oid;
        let mut m = x.cmd(CmdKind::Modify);
        m.sym = 0;
        m.target_oid = oid;
        m.px = 77_999_000_000;
        m.qty = 140;
        x.grow(m);
        let ev = x.tick();
        assert_eq!(x.ctx.modified.len(), 1);
        assert_eq!(x.ctx.modified[0].0, oid);
        let new_oid = ev[0].client_oid;
        let mut k = x.cmd(CmdKind::Cancel);
        k.target_oid = new_oid;
        x.reduce(k);
        x.tick();
        assert_eq!(x.ctx.cancelled, vec![new_oid]);
        let mut u = x.cmd(CmdKind::Cancel);
        u.target_oid = 12345;
        x.reduce(u);
        assert_eq!(x.tick()[0].reason, Reason::UnknownOrder as u8);
    }
}
