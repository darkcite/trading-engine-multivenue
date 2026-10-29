// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # arm — the engine thread's half of slot 11's arm (PMJ J3)
//!
//! [`JupArm`] is an `OrderDispatch`. **Admission** (before the ring): slot
//! 11 only; `Order.venue` = `VenueId::Jupiter` (R-PMJ-S2: the order keeps
//! the Polymarket leg's sym, the book it is judged against); a LEG of a 5 m
//! family (R-PMJ-3: the family split — slot 11 trades the 5 m pool alone)
//! that is allowed live; the window's instance the leg's order law is in
//! force for; an IoC BUY at `0 < px < 1` — the binary-contract law, the
//! deposit `px·qty/1e6`, ≥ the venue's $5 minimum and ≤ the per-order
//! maximum (`pmj.toml`); room in the table; not halted. Everything else is
//! refused unsent. No cancel and no modify: a landed keeper order cannot
//! be pulled (`Unsupported`); the router's halt (`exec.HALT`) reaches
//! [`OrderDispatch::cancel_slot`], which halts the gateway's builds.
//!
//! **Fills** (LAW E-5 for this venue): jup-gw reports an order's fill
//! numbers once recon reads them, cumulatively; the arm books the
//! difference, at the price that difference cost. **Retirements** follow
//! their order's fills by one poll (the router sees the fill first).
//!
//! **The venue relationship**: a ledger of the legs bought (contracts and
//! cost); every balance read (jup-gw's, 30 s) values the wallet as USDC
//! plus each unclaimed leg at cost, a leg its roll settled as lost at 0
//! (it leaves the ledger). The first read seeds `reconciled` and the
//! session anchor (unless the boot restored one). A won (or unknown) leg
//! asks jup-gw to read the positions and claim ([`INTENT_CLAIM`]); it stays
//! in the worth at cost — the claim's submission route is J0's, so no
//! claim lands in this pass.
//!
//! Zero allocation after [`JupArm::new`]; one 64 B ring write per order.

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, OrderDispatch, Retired,
    RETIRED_CANCELED_VENUE, RETIRED_FILLED, RETIRED_REJECTED,
};
use core_fill::ORDER_KIND_IOC;
use core_types::pm_roll::{
    pm5_instance_parts, pm5_rolling_of, pm5_window_end_ns, PM5_OFF_MARK, PM5_ROLLING_MAX,
    PM_SETTLED_DOWN, PM_SETTLED_UP,
};
use core_types::{
    CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId,
    VenueId, FILL_ORIGIN_VENUE, OID_INSTANCE_MASK,
};
use exec_gateway::{EngineEnd, HALT_NONE};

use crate::cmd::{
    reason, JupEvent, JupIntent, EVT_BALANCE, EVT_CLAIMED, EVT_CLAIM_READY, EVT_DONE, EVT_FILL,
    EVT_IN_DOUBT, EVT_LANDED, EVT_LOST, EVT_REFUSED, INTENT_BUY, INTENT_CLAIM, JUP_CMD_RING,
    JUP_EVT_RING,
};
use crate::gw::{HALT_ARM, HALT_OPERATOR, MIN_DEPOSIT_MICRO};
use crate::held::{HeldLeg, HELD_MAX};
use crate::stats::{word, JupExecStats, ARM_WORDS, PUBLISH_EVERY_NS};

/// The engine's end of the jup-gw duplex.
pub type JupArmEnd = EngineEnd<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>;

/// In-flight rows (open orders and the recently done).
pub const ARM_ROWS: usize = 16;
/// Fills and retirements queued for the engine.
pub const ARM_OUT: usize = 64;
/// Legs the ledger holds (the held file's size).
pub const LEDGER_LEGS: usize = HELD_MAX;
/// A done row is kept this long for a late event, ns.
pub const DONE_KEEP_NS: u64 = 120_000_000_000;

const R_FREE: u8 = 0;
const R_QUEUED: u8 = 1;
const R_LANDED: u8 = 2;
const R_DOUBT: u8 = 3;
const R_DONE: u8 = 4;

const L_FREE: u8 = 0;
const L_OPEN: u8 = 1;
/// Settled won (or unknown): claim asked, worth at cost.
const L_SETTLED: u8 = 2;

/// The arm's configuration (boot, from `exec.toml` and `pmj.toml`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct JupArmCfg {
    /// The slot it trades (11).
    pub slot: u8,
    /// 5 m families (the period's rolling order) allowed live: bit `g`.
    pub live_families: u16,
    /// The per-order maximum deposit, USDC micro units.
    pub max_order_micro: u64,
}

/// Why [`JupArm::new`] refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmBootErr {
    /// The slot is not `pmj`'s (11).
    Slot,
    /// The per-order maximum is under the venue's $5 minimum.
    MaxOrder,
}

/// The arm's tallies.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ArmCounters {
    /// Orders pushed to jup-gw.
    pub submitted: u64,
    /// Refused before the ring: not ours, not a 5 m leg, not an IoC BUY,
    /// no law in force, a duplicate id.
    pub refused_shape: u64,
    /// Refused before the ring: a family not allowed live.
    pub refused_family: u64,
    /// Refused before the ring: under $5 or over the per-order maximum.
    pub refused_size: u64,
    /// Refused before the ring: halted.
    pub refused_halted: u64,
    /// Refused before the ring: the table or the ring full.
    pub refused_full: u64,
    /// Orders landed.
    pub landed: u64,
    /// Refusals (jup-gw's and the venue's).
    pub refused: u64,
    /// Orders left in doubt.
    pub in_doubt: u64,
    /// Fills booked.
    pub fills: u64,
    /// Orders done.
    pub done: u64,
    /// Orders lost (recon gave up: the arm halts).
    pub lost: u64,
    /// Legs a settlement found lost (worth 0, out of the ledger).
    pub legs_lost: u64,
    /// Claims jup-gw built and holds.
    pub claims_ready: u64,
    /// Legs the positions read showed claimed (they left the ledger).
    pub legs_claimed: u64,
    /// A fill or retirement the queue could not hold (the arm halts).
    pub out_full: u64,
    /// Events of an order the table does not know.
    pub unknown: u64,
    /// Legs the ledger could not hold (the arm halts).
    pub ledger_full: u64,
    /// (Risk review W5-1) fills whose average price is above the order's
    /// own limit: the Limit evidence is false — booked (the contracts are
    /// held), and the arm halts.
    pub over_limit: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Row {
    client_oid: u64,
    asked_micro: i64,
    filled_micro: i64,
    cost_micro: i64,
    done_ns: u64,
    sym: SymbolId,
    instance: u32,
    outcome: u8,
    state: u8,
    /// The order's own limit, ×1e6 (the intent's `limit_px_1e6`).
    limit_px: i64,
}

const EMPTY_ROW: Row = Row {
    client_oid: 0,
    asked_micro: 0,
    filled_micro: 0,
    cost_micro: 0,
    done_ns: 0,
    limit_px: 0,
    sym: 0,
    instance: 0,
    outcome: 0,
    state: R_FREE,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Leg {
    contracts_micro: i64,
    cost_micro: i64,
    instance: u32,
    outcome: u8,
    state: u8,
}

const EMPTY_LEG: Leg = Leg {
    contracts_micro: 0,
    cost_micro: 0,
    instance: 0,
    outcome: 0,
    state: L_FREE,
};

/// A fixed queue whose entries become visible one pump after they were
/// queued when `gated` (the retirements: the router sees a fill first).
struct Out<T: Copy, const N: usize> {
    buf: [T; N],
    head: usize,
    len: usize,
    visible: usize,
}

impl<T: Copy, const N: usize> Out<T, N> {
    fn new(blank: T) -> Self {
        Self {
            buf: [blank; N],
            head: 0,
            len: 0,
            visible: 0,
        }
    }

    fn push(&mut self, v: T) -> bool {
        if self.len == N {
            return false;
        }
        self.buf[(self.head + self.len) % N] = v;
        self.len += 1;
        true
    }

    /// Everything queued so far becomes visible.
    fn release(&mut self) {
        self.visible = self.len;
    }

    fn pop_visible(&mut self) -> Option<T> {
        if self.visible == 0 {
            return None;
        }
        let v = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        self.visible -= 1;
        Some(v)
    }

    fn pop(&mut self) -> Option<T> {
        self.visible = self.len;
        self.pop_visible()
    }
}

/// Slot 11's arm (module docs).
pub struct JupArm {
    cfg: JupArmCfg,
    end: JupArmEnd,
    /// The instance whose order law is in force per (5 m family, offset).
    law: [[u32; PM5_OFF_MARK as usize]; PM5_ROLLING_MAX],
    rows: [Row; ARM_ROWS],
    legs: [Leg; LEDGER_LEGS],
    fills: Out<Fill, ARM_OUT>,
    retired: Out<Retired, ARM_OUT>,
    counters: ArmCounters,
    halted: bool,
    reconciled: bool,
    agreed_ns: u64,
    usdc_micro: i64,
    worth_micro: i64,
    anchor_micro: Option<i64>,
    reject_streak: u32,
    stats: Option<std::sync::Arc<JupExecStats>>,
    publish_ns: u64,
}

impl JupArm {
    /// Boot the arm. **Boot-only.** Its `max_order_micro` must be jup-gw's
    /// (the boot builds both from one `pmj.toml` key).
    ///
    /// # Errors
    /// [`ArmBootErr`].
    pub fn new(cfg: JupArmCfg, end: JupArmEnd) -> Result<Self, ArmBootErr> {
        if cfg.slot != core_types::STRATEGY_SLOT_PMJ {
            return Err(ArmBootErr::Slot);
        }
        if cfg.max_order_micro < MIN_DEPOSIT_MICRO {
            return Err(ArmBootErr::MaxOrder);
        }
        let blank_fill = Fill::new(0, 0, Side::Bid, Price(0), Qty(0), 0);
        Ok(Self {
            cfg,
            end,
            law: [[0; PM5_OFF_MARK as usize]; PM5_ROLLING_MAX],
            rows: [EMPTY_ROW; ARM_ROWS],
            legs: [EMPTY_LEG; LEDGER_LEGS],
            fills: Out::new(blank_fill),
            retired: Out::new(Retired::new(0, 0, 0)),
            counters: ArmCounters::default(),
            halted: false,
            reconciled: false,
            agreed_ns: 0,
            usdc_micro: -1,
            worth_micro: 0,
            anchor_micro: None,
            reject_streak: 0,
            stats: None,
            publish_ns: 0,
        })
    }

    /// Publish tallies into `stats`.
    pub fn set_stats(&mut self, stats: std::sync::Arc<JupExecStats>) {
        self.stats = Some(stats);
    }

    /// The session anchor the boot restored.
    pub fn restore_anchor(&mut self, anchor_micro: i64) {
        self.anchor_micro = Some(anchor_micro);
    }

    /// The legs the boot restored (`pmj-held.state`).
    pub fn restore_legs(&mut self, held: &[HeldLeg]) {
        let mut i = 0usize;
        let mut k = 0usize;
        while i < held.len() && k < LEDGER_LEGS {
            let h = held[i];
            i += 1;
            if h.state == 0 {
                continue;
            }
            self.legs[k] = Leg {
                contracts_micro: word(h.contracts_micro),
                cost_micro: word(h.cost_micro),
                instance: h.instance,
                outcome: h.outcome & 1,
                state: if h.state == crate::held::HELD_OPEN { L_OPEN } else { L_SETTLED },
            };
            k += 1;
        }
    }

    /// The tallies.
    #[must_use]
    pub const fn counters(&self) -> ArmCounters {
        self.counters
    }

    /// The worth, the anchor (micro USD).
    #[must_use]
    pub const fn worth(&self) -> (i64, Option<i64>) {
        (self.worth_micro, self.anchor_micro)
    }

    /// Whether the arm refuses every order: its own halt, or the kit's.
    #[inline]
    #[must_use]
    pub fn halted(&self) -> bool {
        self.halted || self.end.stats.halt_reason() != HALT_NONE
    }

    /// Orders not done.
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

    fn free_row(&mut self, now: u64) -> Option<usize> {
        let mut i = 0usize;
        while i < ARM_ROWS {
            let r = &self.rows[i];
            if r.state == R_FREE
                || (r.state == R_DONE && now.saturating_sub(r.done_ns) >= DONE_KEEP_NS)
            {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// The arm's own halt — and jup-gw's: an intent already queued is
    /// refused unbuilt.
    fn halt(&mut self) {
        self.halted = true;
        self.end.stats.halt(HALT_ARM);
    }

    /// One pass over the event ring.
    fn pump(&mut self, now: u64) -> bool {
        self.retired.release();
        let mut any = false;
        loop {
            let ev = match self.end.evt.pop() {
                Some(e) => *e,
                None => break,
            };
            any = true;
            self.on_gw(&ev, now);
        }
        if now >= self.publish_ns {
            self.publish(now);
        }
        any
    }

    fn retire(&mut self, i: usize, now: u64, why: u8) {
        let r = &mut self.rows[i];
        if r.state == R_DONE {
            return;
        }
        r.state = R_DONE;
        r.done_ns = now;
        self.counters.done += 1;
        let cid = r.client_oid;
        if !self.retired.push(Retired::new(cid, self.cfg.slot, why)) {
            self.counters.out_full += 1;
            self.halt();
        }
    }

    fn on_gw(&mut self, ev: &JupEvent, now: u64) {
        match ev.kind {
            EVT_BALANCE => return self.on_balance(ev, now),
            EVT_CLAIM_READY => {
                self.counters.claims_ready += 1;
                return;
            }
            EVT_CLAIMED => return self.on_claimed(ev),
            _ => {}
        }
        let Some(i) = self.find_cid(ev.client_oid) else {
            self.counters.unknown += 1;
            return self.halt();
        };
        match ev.kind {
            EVT_LANDED => {
                self.counters.landed += 1;
                self.reject_streak = 0;
                if self.rows[i].state != R_DONE {
                    self.rows[i].state = R_LANDED;
                }
            }
            EVT_IN_DOUBT => {
                self.counters.in_doubt += 1;
                // A venue that said no at the execute (`Failed`, a 4xx)
                // counts toward the reject-streak halt — the row stays in
                // doubt (the transaction may still land); a later LANDED
                // resets the streak.
                if ev.reason != 0 {
                    self.reject_streak = self.reject_streak.saturating_add(1);
                }
                if self.rows[i].state == R_QUEUED {
                    self.rows[i].state = R_DOUBT;
                }
            }
            EVT_REFUSED => {
                self.counters.refused += 1;
                if ev.reason != reason::HALTED {
                    self.reject_streak = self.reject_streak.saturating_add(1);
                }
                self.retire(i, now, RETIRED_REJECTED);
            }
            EVT_FILL => self.on_fill(i, ev, now),
            EVT_DONE => {
                let r = self.rows[i];
                let why = if r.filled_micro > 0 && r.filled_micro >= r.asked_micro {
                    RETIRED_FILLED
                } else {
                    RETIRED_CANCELED_VENUE
                };
                self.retire(i, now, why);
            }
            EVT_LOST => {
                self.counters.lost += 1;
                self.retire(i, now, RETIRED_CANCELED_VENUE);
                self.halt();
            }
            _ => {
                self.counters.unknown += 1;
                self.halt();
            }
        }
    }

    fn on_fill(&mut self, i: usize, ev: &JupEvent, now: u64) {
        let r = self.rows[i];
        let filled = word(ev.filled_micro);
        let cost = word(ev.cost_micro);
        let delta = filled - r.filled_micro;
        if delta <= 0 {
            return;
        }
        let cost_delta = (cost - r.cost_micro).max(0);
        let px = i64::try_from(i128::from(cost_delta) * 1_000_000 / i128::from(delta)).unwrap_or(0);
        if px <= 0 || px >= 1_000_000 {
            self.counters.unknown += 1;
            return self.halt();
        }
        self.rows[i].filled_micro = filled;
        self.rows[i].cost_micro = cost;
        let fill = Fill::new(now, r.sym, Side::Bid, Price(px), Qty(delta), r.client_oid)
            .with_attribution(self.cfg.slot, FILL_ORIGIN_VENUE);
        if !self.fills.push(fill) {
            self.counters.out_full += 1;
            return self.halt();
        }
        self.counters.fills += 1;
        self.ledger_add(r.instance, r.outcome, delta, cost_delta);
        // Only a Limit order is signed (`tx::guard_order`), and the program
        // fills a Limit order at or under its cap: a fill whose average is
        // above the order's own limit falsifies that evidence. The fill is
        // booked (the contracts are held); the arm halts.
        if word(ev.avg_px_micro) > r.limit_px {
            self.counters.over_limit += 1;
            self.halt();
        }
    }

    fn ledger_add(&mut self, instance: u32, outcome: u8, contracts: i64, cost: i64) {
        let mut free = LEDGER_LEGS;
        let mut k = 0usize;
        while k < LEDGER_LEGS {
            let l = &mut self.legs[k];
            if l.state == L_OPEN && l.instance == instance && l.outcome == outcome {
                l.contracts_micro += contracts;
                l.cost_micro += cost;
                return;
            }
            if free == LEDGER_LEGS && l.state == L_FREE {
                free = k;
            }
            k += 1;
        }
        if free == LEDGER_LEGS {
            self.counters.ledger_full += 1;
            return self.halt();
        }
        self.legs[free] = Leg {
            contracts_micro: contracts,
            cost_micro: cost,
            instance,
            outcome,
            state: L_OPEN,
        };
    }

    fn legs_cost(&self) -> i64 {
        let mut c = 0i64;
        let mut k = 0usize;
        while k < LEDGER_LEGS {
            if self.legs[k].state != L_FREE {
                c += self.legs[k].cost_micro;
            }
            k += 1;
        }
        c
    }

    fn on_balance(&mut self, ev: &JupEvent, now: u64) {
        if ev.reason != 0 {
            return;
        }
        self.usdc_micro = word(ev.aux);
        self.worth_micro = self.usdc_micro + self.legs_cost();
        self.reconciled = true;
        self.agreed_ns = now;
        if self.anchor_micro.is_none() {
            self.anchor_micro = Some(self.worth_micro);
        }
    }

    /// The positions read shows a held leg claimed (or settled by the
    /// venue): it leaves the ledger — its payout is in the USDC — and the
    /// next balance read values the wallet without it.
    fn on_claimed(&mut self, ev: &JupEvent) {
        let (inst, outcome) = (ev.aux as u32, ev.reason & 1);
        let mut k = 0usize;
        while k < LEDGER_LEGS {
            let l = self.legs[k];
            if l.state != L_FREE && l.instance == inst && l.outcome == outcome {
                self.legs[k] = EMPTY_LEG;
                self.counters.legs_claimed += 1;
            }
            k += 1;
        }
        if self.reconciled {
            self.worth_micro = self.usdc_micro + self.legs_cost();
        }
    }

    fn on_settled(&mut self, ev: &ChannelEvent) {
        let (inst, _, _, settled) = core_types::unpack_roll_seq(ev.venue_seq);
        if !settled || pm5_rolling_of(ev.sym).is_none() {
            return;
        }
        let known = ev.v0 == PM_SETTLED_UP || ev.v0 == PM_SETTLED_DOWN;
        let mut claim = false;
        let mut k = 0usize;
        while k < LEDGER_LEGS {
            let l = self.legs[k];
            k += 1;
            if l.state != L_OPEN || l.instance != inst {
                continue;
            }
            let won = (l.outcome == 0 && ev.v0 == PM_SETTLED_UP)
                || (l.outcome == 1 && ev.v0 == PM_SETTLED_DOWN);
            if known && !won {
                self.counters.legs_lost += 1;
                self.legs[k - 1] = EMPTY_LEG;
            } else {
                self.legs[k - 1].state = L_SETTLED;
                claim = true;
            }
        }
        if claim {
            let it = JupIntent {
                kind: INTENT_CLAIM,
                instance: inst,
                ..JupIntent::default()
            };
            // A lost ask is harmless: jup-gw reads the positions every
            // minute anyway.
            let _ = self.end.cmd.push(&it);
        }
        if self.reconciled {
            self.worth_micro = self.usdc_micro + self.legs_cost();
        }
    }

    fn on_law(&mut self, ev: &ChannelEvent) {
        let Some((g, off)) = pm5_rolling_of(ev.sym) else {
            return;
        };
        if g >= PM5_ROLLING_MAX || off >= PM5_OFF_MARK {
            return;
        }
        self.law[g][off as usize] = ev.venue_seq as u32;
    }

    fn publish(&mut self, now: u64) {
        self.publish_ns = now.saturating_add(PUBLISH_EVERY_NS);
        let Some(stats) = self.stats.as_ref() else {
            return;
        };
        let c = &self.counters;
        let v: [i64; ARM_WORDS] = [
            word(c.submitted),
            word(c.refused_shape),
            word(c.refused_family),
            word(c.refused_size),
            word(c.refused_halted),
            word(c.refused_full),
            word(c.landed),
            word(c.refused),
            word(c.in_doubt),
            word(c.fills),
            word(c.done),
            word(c.lost),
            word(c.legs_lost),
            word(c.claims_ready),
            word(c.out_full),
            word(c.over_limit),
            i64::from(self.halted()),
            i64::from(self.reconciled),
            self.worth_micro,
            self.anchor_micro.unwrap_or(0),
            self.usdc_micro,
        ];
        stats.set_arm(&v);
    }
}

impl OrderDispatch for JupArm {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        if order.strategy_id != self.cfg.slot {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        if self.halted() {
            self.counters.refused_halted += 1;
            return Err(DispatchError::SlotDisabled);
        }
        if order.venue != VenueId::Jupiter as u8 || order.kind != ORDER_KIND_IOC || order.side != Side::Bid {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        let Some((g, off)) = pm5_rolling_of(order.sym) else {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        };
        if off >= PM5_OFF_MARK {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        if g >= 16 || self.cfg.live_families & (1u16 << g) == 0 {
            self.counters.refused_family += 1;
            return Err(DispatchError::Unroutable);
        }
        let px = order.px.raw();
        let qty = order.qty.raw();
        let instance = (order.client_oid & OID_INSTANCE_MASK) as u32;
        let parts = pm5_instance_parts(instance);
        if px <= 0
            || px >= 1_000_000
            || qty <= 0
            || !matches!(parts, Some((_, ig)) if ig == g)
            || self.law[g][off as usize] != instance
        {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        let Some((window, _)) = parts else {
            return Err(DispatchError::Unroutable);
        };
        // The binary-contract law: the deposit is px·qty/1e6.
        let deposit = u64::try_from(i128::from(px) * i128::from(qty) / 1_000_000).unwrap_or(0);
        if deposit < MIN_DEPOSIT_MICRO || deposit > self.cfg.max_order_micro {
            self.counters.refused_size += 1;
            return Err(DispatchError::Unroutable);
        }
        if self.find_cid(order.client_oid).is_some() {
            self.counters.refused_shape += 1;
            return Err(DispatchError::Unroutable);
        }
        let now = core_time::now_ns();
        let Some(i) = self.free_row(now) else {
            self.counters.refused_full += 1;
            return Err(DispatchError::QueueFull);
        };
        let it = JupIntent {
            client_oid: order.client_oid,
            deposit_micro: deposit,
            limit_px_1e6: px,
            decision_ns: now,
            window_end_ns: pm5_window_end_ns(window),
            instance,
            sym: order.sym,
            outcome: (off % 2) as u8,
            kind: INTENT_BUY,
            ..JupIntent::default()
        };
        if !self.end.cmd.push(&it) {
            self.counters.refused_full += 1;
            return Err(DispatchError::QueueFull);
        }
        self.counters.submitted += 1;
        self.rows[i] = Row {
            client_oid: order.client_oid,
            asked_micro: qty,
            filled_micro: 0,
            cost_micro: 0,
            done_ns: 0,
            sym: order.sym,
            instance,
            outcome: (off % 2) as u8,
            state: R_QUEUED,
            limit_px: px,
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
        self.retired.pop_visible()
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats {
            accepted: self.counters.landed,
            rejected: self.counters.refused,
            fills_seen: self.counters.fills,
            ..DispatchStats::default()
        }
    }

    fn on_idle(&mut self) -> bool {
        self.pump(core_time::now_ns())
    }

    fn on_venue_event(&mut self, event: &ChannelEvent) {
        if event.venue != VenueId::Polymarket as u8 {
            return;
        }
        match ChannelId::from_u8(event.channel) {
            Some(ChannelId::OutcomeMeta) => self.on_law(event),
            Some(ChannelId::InstrumentRoll) => self.on_settled(event),
            _ => {}
        }
    }

    /// The router's halt (`exec.HALT` on slot 11): no new builds. A landed
    /// order cannot be pulled; it is polled and reconciled to its end.
    fn cancel_slot(&mut self, slot: u8) -> Result<(), DispatchError> {
        if slot == self.cfg.slot {
            // The operator's reason first: the first reason wins.
            self.end.stats.halt(HALT_OPERATOR);
            self.halted = true;
        }
        Ok(())
    }

    fn cancel_slot_state(&self, slot: u8) -> CancelAllState {
        if slot == self.cfg.slot && self.open_orders() > 0 {
            CancelAllState::Working
        } else {
            CancelAllState::Clear
        }
    }

    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        self.cancel_slot(self.cfg.slot)
    }

    fn cancel_all_state(&self) -> CancelAllState {
        self.cancel_slot_state(self.cfg.slot)
    }

    fn halt_signal(&self) -> HaltSignal {
        let now = core_time::now_ns();
        let age = if self.agreed_ns == 0 {
            0
        } else {
            now.saturating_sub(self.agreed_ns)
        };
        let (judged, delta) = match self.anchor_micro {
            Some(a) if self.reconciled => (true, self.worth_micro - a),
            _ => (false, 0),
        };
        HaltSignal::new(0, 0, self.reject_streak, 0, false, self.reconciled, age)
            .with_pnl(judged, delta)
            .with_gateway_halted(self.halted())
    }
}

#[cfg(test)]
mod tests;
