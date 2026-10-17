// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The engine thread's half of the slot-9 arm (PM15-6b, plan §5–§6):
//! [`PmArm`] is an `OrderDispatch`.
//!
//! **Admission** (before the ring): slot 9 only; a Polymarket rolling LEG
//! of a family in `live_families` (D-9: the member's `pm15.toml
//! live_assets`, enforced here a second time); an IoC BUY at `0 < px < 1`
//! on the leg's order law in force (the `OutcomeMeta` the roll carried);
//! room in the in-flight table; not halted. Everything else is refused
//! unsent. No cancel and no modify: a FAK cannot be cancelled during the
//! taker hold, and nothing rests (`Unsupported`).
//!
//! **The in-flight table** (≤ [`ARM_ROWS`]): an order is
//! `QUEUED → SENT → ANSWERED → DONE`, or `→ REFUSED → DONE`, or
//! `→ DOUBT → RECON → DONE`. SENT names its `orderID`, which is how a
//! channel trade finds it. DONE is reached when the answer's `tradeIDs`
//! count of trades has been booked, at a refusal, or when recon says so;
//! an order past its deadline ([`DEADLINE_NS`]: the taker hold, the round
//! trip and the channel's lag, with room) is sent to recon, and so is
//! every open order at a channel GAP. A DONE row is remembered — for
//! [`DONE_KEEP_NS`] unless a new order needs its row first (counted) — so
//! a late trade still finds its order.
//!
//! **Fills** (LAW E-5): a trade id is booked once — at its first
//! `MATCHED` / `MATCHED_NOT_BROADCASTED`, or at its first sighting in any
//! later status (the channel missed the match); `CONFIRMED` closes it; a
//! `FAILED` after a booking is a reversal: the arm halts (the operator
//! reconciles). A trade of no order in the table — after one more read of
//! the gw ring, where its SENT may still wait — halts the arm too.
//!
//! **Retirements** follow their order's fills by one poll (the HL arm's
//! XH4f law: a router sees a fill before the retirement of its order).
//!
//! Zero allocation after [`PmArm::new`]; one 64 B ring write per order.

use core_types::pm_roll::{pm_instance_parts, pm_rolling_of, PM_OFF_MARK, PM_ROLLING_MAX};
use core_types::{
    CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId,
    VenueId, FILL_ORIGIN_VENUE, OID_INSTANCE_MASK, SYMBOL_ID_NONE,
};

use clob_dispatcher::{
    DispatchError, DispatchStats, OrderDispatch, Retired, RETIRED_CANCELED_VENUE, RETIRED_FILLED,
    RETIRED_REJECTED,
};
use core_fill::ORDER_KIND_IOC;
use core_ring::Consumer;
use exec_gateway::{EngineEnd, HALT_NONE};

use crate::amounts::usd_of;
use crate::cmd::{
    PmCmd, PmEvt, CMD_RECON, CMD_SUBMIT, EVT_ACK, EVT_DONE, EVT_FILL, EVT_GAP, EVT_IN_DOUBT,
    EVT_REFUSED, EVT_SENT, PM_CMD_RING, PM_EVT_RING, PM_USER_RING,
};
use crate::userws::{TS_CONFIRMED, TS_FAILED};

/// The engine's end of the pm-gw duplex.
pub type PmArmEnd = EngineEnd<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>;

/// In-flight rows (open orders and the recently done).
pub const ARM_ROWS: usize = 16;
/// Trade ids remembered (booked) — far more than can be open at once.
pub const ARM_KEYS: usize = 256;
/// Fills and retirements queued for the engine.
pub const ARM_OUT: usize = 64;
/// An order not DONE this long after its submit goes to recon, ns.
pub const DEADLINE_NS: u64 = 10_000_000_000;
/// An order IN_DOUBT goes to recon this long after the answer, ns.
pub const DOUBT_RECON_NS: u64 = 2_000_000_000;
/// A DONE row is kept this long for a late trade, ns.
pub const DONE_KEEP_NS: u64 = 120_000_000_000;
/// The deadline scan's cadence, ns.
const SCAN_EVERY_NS: u64 = 100_000_000;

const R_FREE: u8 = 0;
const R_QUEUED: u8 = 1;
const R_SENT: u8 = 2;
const R_ANSWERED: u8 = 3;
const R_DOUBT: u8 = 4;
const R_RECON: u8 = 5;
const R_DONE: u8 = 6;

/// `expected` before an answer told it.
const TRADES_UNKNOWN: u16 = u16::MAX;

/// The arm's configuration (boot).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmArmCfg {
    /// The slot it trades (9).
    pub slot: u8,
    /// Families (rolling order) allowed live: bit `f` (D-9).
    pub live_families: u16,
}

/// The arm's tallies.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ArmCounters {
    /// Orders pushed to pm-gw.
    pub submitted: u64,
    /// Refused before the ring: not ours, not a leg, not IoC BUY, no law.
    pub refused_shape: u64,
    /// Refused before the ring: a family not allowed live (D-9).
    pub refused_family: u64,
    /// Refused before the ring: halted.
    pub refused_halted: u64,
    /// Refused before the ring: the table or the ring full.
    pub refused_full: u64,
    /// Answers accepted.
    pub acks: u64,
    /// Refusals (the venue's and pm-gw's own).
    pub refused: u64,
    /// The venue's zero-fill FAK refusals among them.
    pub no_match: u64,
    /// Orders left in doubt.
    pub in_doubt: u64,
    /// Recons asked.
    pub recons: u64,
    /// Channel gaps.
    pub gaps: u64,
    /// Trades booked as fills.
    pub fills: u64,
    /// Trade events of a trade already booked (later statuses).
    pub repeats: u64,
    /// Trades confirmed final.
    pub confirmed: u64,
    /// Trades that failed after their booking (halts).
    pub reversed: u64,
    /// Trades of no order in the table (halts).
    pub unknown: u64,
    /// Orders done.
    pub done: u64,
    /// Fills or retirements the out-queues could not take (halts).
    pub out_full: u64,
    /// DONE rows reused before their keep ran out.
    pub evicted_young: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Row {
    order_id: [u8; 32],
    client_oid: u64,
    asked_1e6: i64,
    filled_1e6: i64,
    deadline_ns: u64,
    done_ns: u64,
    sym: SymbolId,
    instance: u32,
    expected: u16,
    booked: u16,
    state: u8,
}

const EMPTY_ROW: Row = Row {
    order_id: [0; 32],
    client_oid: 0,
    asked_1e6: 0,
    filled_1e6: 0,
    deadline_ns: 0,
    done_ns: 0,
    sym: SYMBOL_ID_NONE,
    instance: 0,
    expected: 0,
    booked: 0,
    state: R_FREE,
};

/// A fixed out-queue; [`Out::release`] makes what was pushed so far
/// poppable by [`Out::pop_released`].
struct Out<T: Copy> {
    buf: Box<[T; ARM_OUT]>,
    head: usize,
    len: usize,
    ready: usize,
}

impl<T: Copy> Out<T> {
    fn new(blank: T) -> Self {
        Self {
            buf: exec_gateway::boxed_array(blank),
            head: 0,
            len: 0,
            ready: 0,
        }
    }

    #[inline(always)]
    fn push(&mut self, v: T) -> bool {
        if self.len >= ARM_OUT {
            return false;
        }
        self.buf[(self.head + self.len) % ARM_OUT] = v;
        self.len += 1;
        true
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head = (self.head + 1) % ARM_OUT;
        self.len -= 1;
        self.ready = self.ready.min(self.len);
        Some(v)
    }

    #[inline(always)]
    fn release(&mut self) {
        self.ready = self.len;
    }

    #[inline(always)]
    fn pop_released(&mut self) -> Option<T> {
        if self.ready == 0 {
            return None;
        }
        self.ready -= 1;
        self.pop()
    }
}

/// The arm (module docs).
pub struct PmArm {
    cfg: PmArmCfg,
    end: PmArmEnd,
    user: Consumer<PmEvt, PM_USER_RING>,
    rows: [Row; ARM_ROWS],
    keys: Box<[u64; ARM_KEYS]>,
    key_at: usize,
    /// The order law in force per family per leg offset: `(instance,
    /// tick ×1e6)` from the roll's `OutcomeMeta`.
    law: [[(u32, u32); PM_OFF_MARK as usize]; PM_ROLLING_MAX],
    fills: Out<Fill>,
    retired: Out<Retired>,
    halted: bool,
    next_scan_ns: u64,
    counters: ArmCounters,
}

impl PmArm {
    /// **Boot-only.** `end` is the engine's end of the pm-gw duplex, `user`
    /// the user ring's consumer.
    #[must_use]
    pub fn new(cfg: PmArmCfg, end: PmArmEnd, user: Consumer<PmEvt, PM_USER_RING>) -> Self {
        Self {
            cfg,
            end,
            user,
            rows: [EMPTY_ROW; ARM_ROWS],
            keys: exec_gateway::boxed_array(0),
            key_at: 0,
            law: [[(0, 0); PM_OFF_MARK as usize]; PM_ROLLING_MAX],
            fills: Out::new(Fill::new(0, SYMBOL_ID_NONE, Side::Bid, Price(0), Qty(0), 0)),
            retired: Out::new(Retired::new(0, 0, 0)),
            halted: false,
            next_scan_ns: 0,
            counters: ArmCounters::default(),
        }
    }

    /// The tallies.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> ArmCounters {
        self.counters
    }

    /// Whether the arm refuses every order: its own halt, or the kit's
    /// halt word (pm-gw's or pm-user's).
    #[inline]
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted || self.end.stats.halt_reason() != HALT_NONE
    }

    /// Orders not DONE.
    #[must_use]
    pub fn open_orders(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < ARM_ROWS {
            n += usize::from(self.rows[i].state != R_FREE && self.rows[i].state != R_DONE);
            i += 1;
        }
        n
    }

    #[inline]
    fn find_cid(&self, cid: u64) -> Option<usize> {
        let mut i = 0usize;
        while i < ARM_ROWS {
            if self.rows[i].state != R_FREE && self.rows[i].client_oid == cid {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn find_order(&self, order_id: &[u8; 32]) -> Option<usize> {
        let mut i = 0usize;
        while i < ARM_ROWS {
            let r = &self.rows[i];
            if r.state >= R_SENT && r.order_id == *order_id {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// A row for a new order: a free one, else the oldest DONE row (its
    /// late-trade memory goes first; open orders are never displaced).
    /// Counted when that row was younger than [`DONE_KEEP_NS`].
    fn free_row(&mut self, now: u64) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut i = 0usize;
        while i < ARM_ROWS {
            let r = &self.rows[i];
            if r.state == R_FREE {
                return Some(i);
            }
            if r.state == R_DONE && best.is_none_or(|b| r.done_ns < self.rows[b].done_ns) {
                best = Some(i);
            }
            i += 1;
        }
        if let Some(b) = best {
            if now.saturating_sub(self.rows[b].done_ns) < DONE_KEEP_NS {
                self.counters.evicted_young += 1;
            }
        }
        best
    }

    fn halt(&mut self) {
        self.halted = true;
    }

    /// One pass over the rings and the clock (module docs).
    fn pump(&mut self, now: u64) -> bool {
        // What was retired last poll may go now: its fills went first.
        self.retired.release();
        let mut did = self.drain_gw(now);
        loop {
            // COPY: the 64 B event off the user ring — handling it may read
            // the gw ring again (a SENT still waiting) — per event.
            let Some(ev) = self.user.try_pop_ref().map(|e| *e) else {
                break;
            };
            did = true;
            match ev.kind {
                EVT_FILL => self.on_fill(&ev, now),
                EVT_GAP => self.on_gap(),
                _ => {}
            }
        }
        if now >= self.next_scan_ns {
            self.next_scan_ns = now.saturating_add(SCAN_EVERY_NS);
            did |= self.scan_deadlines(now);
        }
        did
    }

    fn drain_gw(&mut self, now: u64) -> bool {
        let mut did = false;
        loop {
            // COPY: the 64 B event off the gw ring (the guard would hold
            // the ring's borrow across the row updates) — per event.
            let Some(ev) = self.end.evt.pop().map(|e| *e) else {
                break;
            };
            did = true;
            self.on_gw(&ev, now);
        }
        did
    }

    fn on_gw(&mut self, ev: &PmEvt, now: u64) {
        if ev.kind == EVT_FILL {
            return self.on_fill(ev, now);
        }
        let Some(i) = self.find_cid(ev.a) else {
            // An event about an order the arm does not hold: pm-gw and the
            // arm disagree — fail fast.
            self.counters.unknown += 1;
            return self.halt();
        };
        match ev.kind {
            EVT_SENT => {
                let r = &mut self.rows[i];
                r.order_id = ev.order_id;
                if r.state == R_QUEUED {
                    r.state = R_SENT;
                }
            }
            EVT_ACK => {
                self.counters.acks += 1;
                let r = &mut self.rows[i];
                r.expected = ev.b.clamp(0, i64::from(TRADES_UNKNOWN - 1)) as u16;
                if r.state == R_SENT || r.state == R_QUEUED {
                    r.state = R_ANSWERED;
                }
                self.try_done(i, now);
            }
            EVT_REFUSED => {
                self.counters.refused += 1;
                if ev.why == crate::response::Refusal::NoMatch as u8 {
                    self.counters.no_match += 1;
                }
                self.done(i, now, RETIRED_REJECTED);
            }
            EVT_IN_DOUBT => {
                self.counters.in_doubt += 1;
                let r = &mut self.rows[i];
                r.order_id = ev.order_id;
                r.state = R_DOUBT;
                r.deadline_ns = now.saturating_add(DOUBT_RECON_NS);
            }
            EVT_DONE => {
                if ev.b < 0 {
                    // Recon could not read the order's trades: its position
                    // is unknown — the slot stops.
                    self.halt();
                }
                let why = if self.rows[i].filled_1e6 >= self.rows[i].asked_1e6 {
                    RETIRED_FILLED
                } else {
                    RETIRED_CANCELED_VENUE
                };
                if self.rows[i].state != R_DONE {
                    self.done(i, now, why);
                }
            }
            _ => {}
        }
    }

    fn on_fill(&mut self, ev: &PmEvt, now: u64) {
        let i = match self.find_order(&ev.order_id) {
            Some(i) => i,
            None => {
                // Its SENT may still wait in the gw ring.
                self.drain_gw(now);
                match self.find_order(&ev.order_id) {
                    Some(i) => i,
                    None => {
                        self.counters.unknown += 1;
                        return self.halt();
                    }
                }
            }
        };
        let key = ev.a;
        let booked = self.key_booked(key);
        let status = ev.why;
        if status == TS_FAILED {
            if booked {
                self.counters.reversed += 1;
                self.halt();
            }
            return;
        }
        if booked {
            self.counters.repeats += 1;
            if status == TS_CONFIRMED {
                self.counters.confirmed += 1;
            }
            return;
        }
        // The first sighting of a trade id is its fill: `MATCHED` /
        // `MATCHED_NOT_BROADCASTED`, or a later status when the channel
        // missed the match.
        let (px, qty) = (ev.b, ev.c);
        if qty <= 0 || px <= 0 {
            self.counters.unknown += 1;
            return self.halt();
        }
        self.remember(key);
        let r = &mut self.rows[i];
        r.filled_1e6 += qty;
        r.booked = r.booked.saturating_add(1);
        let fill = Fill::new(now, r.sym, Side::Bid, Price(px), Qty(qty), r.client_oid)
            .with_attribution(self.cfg.slot, FILL_ORIGIN_VENUE);
        if !self.fills.push(fill) {
            self.counters.out_full += 1;
            self.halt();
            return;
        }
        self.counters.fills += 1;
        if status == TS_CONFIRMED {
            self.counters.confirmed += 1;
        }
        self.try_done(i, now);
    }

    fn on_gap(&mut self) {
        self.counters.gaps += 1;
        let mut i = 0usize;
        while i < ARM_ROWS {
            let s = self.rows[i].state;
            if s == R_SENT || s == R_ANSWERED || s == R_DOUBT {
                self.ask_recon(i);
            }
            i += 1;
        }
    }

    fn scan_deadlines(&mut self, now: u64) -> bool {
        let mut did = false;
        let mut i = 0usize;
        while i < ARM_ROWS {
            let r = &self.rows[i];
            let open = r.state == R_SENT || r.state == R_ANSWERED || r.state == R_DOUBT;
            if open && now >= r.deadline_ns {
                self.ask_recon(i);
                did = true;
            }
            i += 1;
        }
        did
    }

    fn ask_recon(&mut self, i: usize) {
        let r = self.rows[i];
        let mut c = PmCmd::new(CMD_RECON, r.client_oid);
        c.order_id = r.order_id;
        c.instance = r.instance;
        if self.end.cmd.push(&c) {
            self.counters.recons += 1;
            self.rows[i].state = R_RECON;
        }
        // A full command ring: the next scan asks again.
    }

    #[inline]
    fn key_booked(&self, key: u64) -> bool {
        let mut i = 0usize;
        while i < ARM_KEYS {
            if self.keys[i] == key && key != 0 {
                return true;
            }
            i += 1;
        }
        false
    }

    #[inline]
    fn remember(&mut self, key: u64) {
        self.keys[self.key_at % ARM_KEYS] = key;
        self.key_at = self.key_at.wrapping_add(1);
    }

    /// DONE once the answer's trades are all booked.
    fn try_done(&mut self, i: usize, now: u64) {
        let r = &self.rows[i];
        if r.state == R_ANSWERED && r.expected != TRADES_UNKNOWN && r.booked >= r.expected {
            let why = if r.filled_1e6 >= r.asked_1e6 {
                RETIRED_FILLED
            } else {
                RETIRED_CANCELED_VENUE
            };
            self.done(i, now, why);
        }
    }

    fn done(&mut self, i: usize, now: u64, why: u8) {
        self.counters.done += 1;
        let r = &mut self.rows[i];
        r.state = R_DONE;
        r.done_ns = now;
        let cid = r.client_oid;
        if !self.retired.push(Retired::new(cid, self.cfg.slot, why)) {
            self.counters.out_full += 1;
            self.halt();
        }
    }

    /// Show the arm the roll's order law (`OutcomeMeta`): the tick its
    /// orders are priced on.
    fn on_law(&mut self, ev: &ChannelEvent) {
        let Some((f, off)) = pm_rolling_of(ev.sym) else {
            return;
        };
        if f >= PM_ROLLING_MAX || off >= PM_OFF_MARK || ev.v0 <= 0 || ev.v0 > i64::from(u32::MAX) {
            return;
        }
        self.law[f][off as usize] = (ev.venue_seq as u32, ev.v0 as u32);
    }
}

impl OrderDispatch for PmArm {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if order.strategy_id != self.cfg.slot {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        if self.halted() {
            self.counters.refused_halted += 1;
            return Err(DispatchError::SlotDisabled);
        }
        let Some((f, off)) = pm_rolling_of(order.sym) else {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        };
        if off >= PM_OFF_MARK
            || order.venue != VenueId::Polymarket as u8
            || order.kind != ORDER_KIND_IOC
            || order.side != Side::Bid
        {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        if f >= 16 || self.cfg.live_families & (1u16 << f) == 0 {
            self.counters.refused_family += 1;
            return Err(DispatchError::Unroutable);
        }
        let px = order.px.raw();
        let qty = order.qty.raw();
        let instance = (order.client_oid & OID_INSTANCE_MASK) as u32;
        let (law_inst, tick) = self.law[f][off as usize];
        let (_, inst_f) = pm_instance_parts(instance);
        let usd = usd_of(qty.max(0) as u64, px.max(0) as u64);
        if px <= 0
            || px >= 1_000_000
            || qty <= 0
            || inst_f != f
            || law_inst != instance
            || tick == 0
            || usd == 0
            || usd > u64::from(u32::MAX)
        {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        let now = core_time::now_ns();
        if self.find_cid(order.client_oid).is_some() {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        let Some(i) = self.free_row(now) else {
            self.counters.refused_full += 1;
            return Err(DispatchError::QueueFull);
        };
        let mut c = PmCmd::new(CMD_SUBMIT, order.client_oid);
        c.px_1e6 = px as u32;
        c.usd_1e6 = usd as u32;
        c.instance = instance;
        c.tick_1e6 = tick;
        c.leg = (off % 2) as u8;
        if !self.end.cmd.push(&c) {
            self.counters.refused_full += 1;
            return Err(DispatchError::QueueFull);
        }
        self.counters.submitted += 1;
        self.rows[i] = Row {
            order_id: [0; 32],
            client_oid: order.client_oid,
            asked_1e6: qty,
            filled_1e6: 0,
            deadline_ns: now.saturating_add(DEADLINE_NS),
            done_ns: 0,
            sym: order.sym,
            instance,
            expected: TRADES_UNKNOWN,
            booked: 0,
            state: R_QUEUED,
        };
        Ok(())
    }

    fn cancel(&mut self, _req: &CancelReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    fn modify(&mut self, _req: &ModifyReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        let _ = self.pump(core_time::now_ns());
        self.fills.pop()
    }

    fn try_next_retired(&mut self) -> Option<Retired> {
        self.retired.pop_released()
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats {
            accepted: self.counters.acks,
            rejected: self.counters.refused,
            fills_seen: self.counters.fills,
            ..DispatchStats::default()
        }
    }

    fn on_idle(&mut self) -> bool {
        self.pump(core_time::now_ns())
    }

    fn on_venue_event(&mut self, event: &ChannelEvent) {
        if event.venue == VenueId::Polymarket as u8
            && ChannelId::from_u8(event.channel) == Some(ChannelId::OutcomeMeta)
        {
            self.on_law(event);
        }
    }
}

#[cfg(test)]
mod tests;
