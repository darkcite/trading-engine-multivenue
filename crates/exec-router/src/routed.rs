// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The compositing dispatcher: one `OrderDispatch` over two arms,
//! choosing between them on `Order.strategy_id`.
//!
//! ## The hot path
//!
//! One masked byte load from the route table, one three-way branch,
//! one delegated call. No `dyn`, no allocation, no lock, no syscall.
//! Both arms are concrete type parameters, so the whole thing
//! monomorphises and the branch predicts perfectly in the steady state
//! (a given slot's mode never changes within a boot).
//!
//! ## The two laws this type exists to enforce
//!
//! **LAW E-1 — a live slot never falls back to paper.** A `Live` slot
//! whose order names a venue it has no route to is *refused and
//! counted*, never quietly matched on paper. A modelled fill carries
//! `FILL_ORIGIN_PAPER`, but every downstream reader of a live slot's
//! tape is entitled to assume its fills happened; silently satisfying
//! a live order on paper would put a trade that never occurred into
//! the P&L with live semantics.
//!
//! **LAW E-2 — the matcher's counters describe the PAPER arm only.**
//! `matcher_counters()` and `open_paper_orders()` forward to the paper
//! arm and nothing else, so `/metrics` can never suggest the matcher
//! is modelling something the venue is really doing.
//!
//! ## Where live fills come from (plan §0.1-1)
//!
//! **Not from here.** `QueuedDispatcher::try_next_fill` returns `None`
//! by contract ("fills flow through a separate path (engine fill
//! ring)"), and the engine already owns per-venue fill lanes —
//! `engine::fill_lane_of(VenueId::Hyperliquid) == Some(3)`, whose
//! producer the live worker thread takes over in E4. The engine drains
//! fill lanes *before* the dispatcher fill pump, so "a real fill never
//! queues behind a modelled one" is already a structural property of
//! the loop and costs no code here. `try_next_fill` therefore forwards
//! the **paper arm only**.

use crate::counters::{RiskRefusal, RouteCounters};
use crate::ledger::Ledger;
use crate::mode::ExecMode;
use crate::route::ExecRoute;
use clob_dispatcher::{
    DispatchError, DispatchStats, ExecCounters, MatcherCounters, OrderDispatch,
};
use core_types::{CancelReq, Fill, ModifyReq, NsTs, Order, Side, Tick};

/// Routes each order to the paper matcher or the live arm according to
/// the boot-fixed [`ExecRoute`].
///
/// With an all-paper table this is observationally identical to the
/// bare paper arm — that equivalence is the E1 acceptance property and
/// is asserted in `tests::an_all_paper_table_is_indistinguishable_…`.
#[derive(Debug)]
pub struct RoutedDispatcher<P: OrderDispatch, L: OrderDispatch> {
    route: ExecRoute,
    paper: P,
    live: L,
    counters: RouteCounters,
    /// **E6** — what the venue has actually done, as the router sees
    /// it. Boxed because it is ~14 KiB of tables and `RoutedDispatcher`
    /// is moved by value into the engine at boot; the engine's own
    /// struct is not a place to grow by cache lines nobody on the tick
    /// path reads. One pointer hop, on the refusal path only, past an
    /// HTTP round trip.
    ledger: Box<Ledger>,
}

impl<P: OrderDispatch, L: OrderDispatch> RoutedDispatcher<P, L> {
    /// Compose the two arms under `route`. Boot-only.
    ///
    /// `anchor` is the monotonic→wall conversion the day cap's 00:00Z
    /// epoch needs; take it with `core_time::WallAnchor::now()` at
    /// boot. See `ledger::Ledger::anchor` for why an engine timestamp
    /// cannot be fed to a wall-clock epoch directly.
    #[inline]
    pub fn new(route: ExecRoute, paper: P, live: L, anchor: core_time::WallAnchor) -> Self {
        Self {
            route,
            paper,
            live,
            counters: RouteCounters::new(),
            ledger: Box::new(Ledger::new(anchor)),
        }
    }

    /// **E6** — the venue-fill ledger. Cold; `/state`, `/metrics`
    /// and tests.
    #[inline]
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// **E6 — the ledger has been reconciled against the venue.**
    ///
    /// Until this is called the risk gate refuses every live PLACE,
    /// because an unreconciled ledger reads zero exposure, zero
    /// turnover and zero resting orders — which after a restart is not
    /// the truth, and would fail all three clamps OPEN against
    /// whatever the previous boot left working.
    ///
    /// **Nothing in production calls this yet.** The reconciler
    /// wiring is E6 commit 3, so a live slot armed on this commit
    /// alone refuses every order. That is the intended reading: a risk
    /// gate with no memory of the venue must not pass orders, and a
    /// slot that refuses loudly in its first second is a better
    /// failure than a cap that was never really there.
    #[inline]
    pub fn mark_ledger_seeded(&mut self) {
        self.ledger.mark_seeded();
    }

    /// The table in force. Cold; boot tell, `/state`.
    #[inline]
    #[must_use]
    pub fn route(&self) -> &ExecRoute {
        &self.route
    }

    /// What the router did. Cold; the 5 s metrics tick.
    #[inline]
    #[must_use]
    pub fn counters(&self) -> RouteCounters {
        self.counters
    }

    /// The paper arm, for tests and for the cli's `/state` assembly.
    #[inline]
    pub fn paper(&self) -> &P {
        &self.paper
    }

    /// The live arm, same.
    #[inline]
    pub fn live(&self) -> &L {
        &self.live
    }
}

impl<P: OrderDispatch, L: OrderDispatch> RoutedDispatcher<P, L> {
    /// **E6 — the risk gate.** Four clamps, live arm only.
    ///
    /// | clamp | asks | from |
    /// |---|---|---|
    /// | `max_order_usd` | is this ONE order too big | the request |
    /// | `cap_instance_usd` | would it RAISE net exposure past the cap | the ledger |
    /// | `cap_day_usd` | would it take today's BUY TURNOVER too far | the ledger |
    /// | `max_open_orders` | are too many already working | the ledger |
    ///
    /// Ahead of all four: **has this ledger ever been reconciled?**
    /// If not, its three numbers describe an empty world rather than
    /// the venue's, and every verb that can reach the venue is
    /// refused.
    ///
    /// ## Why here and not in the member
    ///
    /// bin15 has its own `cap_instance`/`cap_day` ledger and sizes
    /// every order against it. This is a SECOND OPINION, not a copy:
    /// it is the operator's number, it is checked on the dispatch path
    /// rather than the sizing path, and — for the two money caps — it
    /// is computed from **what the venue actually filled**, while the
    /// member's is computed from what it INTENDED to spend (notional
    /// reserved at submit, credited back on the unfilled part).
    ///
    /// **Those are different quantities, deliberately.** The member's
    /// `cap_instance` counts cumulative entry notional; this one
    /// counts `|yes − no|`, money actually at stake. So a gap between
    /// the two is NOT automatically an alarm the way a
    /// `max_order_usd` refusal is — a member can sit well inside its
    /// own turnover cap while holding a one-sided position this clamp
    /// refuses to add to, and that is the clamp working, not a
    /// disagreement. `refused_max_order` is the field that means "the
    /// two ledgers disagreed"; the other three mean "the operator's
    /// ceiling was reached".
    ///
    /// ## Order of the tests
    ///
    /// Cheapest and most local first: the request's own notional, then
    /// the resting count (one array load), then the two ledger walks.
    /// Only the FIRST clamp to fire is counted — the order stops at
    /// it, so no second clamp is ever breached, and counting a
    /// "would also have failed" would inflate the number an operator
    /// reads as a rate.
    ///
    /// ## Notional
    ///
    /// `px` and `qty` are both ×1e6, so their product over 1e6 is USD
    /// ×1e6, the same scale the caps are stated in. Done in `i128`
    /// because the product overflows `i64` at ~9.2e12 — a $92 000
    /// order of a $1 contract reaches it, which is inside the range a
    /// fat-fingered `exec.toml` could ask for, and a saturating
    /// product would clamp to a POSITIVE `i64::MAX` and sail past a
    /// cap rather than into it.
    ///
    /// ## A cap of 0
    ///
    /// `0` means UNSET, never "unlimited" — `core_config::exec` says
    /// so in those words and REFUSES A LIVE SLOT that leaves any of
    /// the four at zero, so a boot can never reach this with an unset
    /// clamp. A default `ExecRoute` leaves all four zero, but a
    /// default route is all-paper and never gets here.
    ///
    /// Three of the four then refuse everything. **`cap_instance` does
    /// not**, and saying it did would be a checkably wrong claim: its
    /// second test lets any non-increasing order through at any cap,
    /// zero included. `max_open_orders = 0` refuses every PLACE first,
    /// so nothing reaches the venue either way — but the reason is the
    /// open-order clamp, not this one.
    ///
    /// ## What these clamps do NOT bound
    ///
    /// `cap_instance` is computed from FILLS. A slot's own resting
    /// orders are not in it, so N orders in flight are each judged
    /// against the same unchanged position and all N can pass. The
    /// worst case with every quote working is
    /// `cap_instance_usd + max_open_orders × max_order_usd`, not
    /// `cap_instance_usd`. The three numbers MULTIPLY, and an
    /// operator setting them needs to know that. Projecting resting
    /// notional too would mean assuming both sides of a two-sided
    /// quote fill, which cannot happen and would strangle the maker —
    /// so this is a stated bound, not an oversight.
    fn risk_check(&mut self, order: &Order, verb: RiskVerb) -> Result<(), DispatchError> {
        let slot = order.strategy_id as usize;
        let Some(caps) = self.route.caps_at(slot) else {
            // No such slot. The caller's own `mode()` lookup masks the
            // id into range, so this is unreachable — and it refuses
            // rather than passing, because a clamp that cannot find
            // its number must not wave the order through.
            self.counters
                .on_refused_risk(order.strategy_id, RiskRefusal::MaxOrder);
            return Err(DispatchError::RiskRefused);
        };

        // ---- 0. does this ledger know what the venue holds? --------
        //
        // A ledger that has not been reconciled reads zero exposure,
        // zero turnover and zero resting orders — which after a
        // RESTART is not the truth, and all three clamps below would
        // fail OPEN against whatever the previous boot left at the
        // venue.
        //
        // **BOTH VERBS.** An earlier cut exempted a modify, on the
        // reasoning that refusing one would strand a quote at the
        // venue with no way to move or shrink it. That reasoning was
        // simply wrong: `cancel` is never risk-checked, so a member
        // always has a way to take a quote back, and waiting for
        // seeding is not being stranded. What the exemption actually
        // bought was the one thing this interlock exists to stop — a
        // modify RAISES price and size, the venue holds pre-boot
        // orders across our restarts, and the exempted verb would
        // have been judged against a ledger reading zero. Commit 1's
        // own note names that hole: "a clamp on `submit` alone leaves
        // the cap reachable by repricing upward."
        if !self.ledger.is_seeded() {
            return self.refuse(order.strategy_id, RiskRefusal::Unseeded);
        }

        let px = order.px.raw();
        let qty = order.qty.raw();
        let buy = order.side == Side::Bid;
        let notional_1e6 = ((px as i128).saturating_mul(qty as i128) / 1_000_000) as i64;

        // ---- 1. this one order ---------------------------------------
        if notional_1e6 > caps.max_order_usd_1e6 {
            return self.refuse(order.strategy_id, RiskRefusal::MaxOrder);
        }

        // ---- 2. how many are already working -------------------------
        // A REPLACE is exempt by LAW E-7: a modify swaps one resting
        // order for another in place, so the count it is judged
        // against is the count it will leave behind. Testing it would
        // refuse the requote of a slot sitting exactly at its cap —
        // which is the slot that most needs to be able to move its
        // quotes.
        if matches!(verb, RiskVerb::Place)
            && self.ledger.slot_resting(slot) >= caps.max_open_orders
        {
            return self.refuse(order.strategy_id, RiskRefusal::OpenOrders);
        }

        // ---- 3. what it would leave at stake -------------------------
        //
        // **Two conditions, and the second is not redundant.** Over
        // the cap is not enough: the order must also INCREASE
        // exposure. A slot can be over its cap without having asked
        // to be — the operator lowered the number, or fills landed
        // past what any projection could have known — and the only
        // way out of a position is to send an order. A clamp that
        // tested `projected > cap` alone would refuse exactly that
        // order and TRAP the member inside the exposure the cap
        // exists to bound, with no path back except an operator
        // cancelling by hand.
        //
        // So: an order that does not raise exposure is never refused
        // here. Below the cap, increases up to it pass. At or over
        // it, every increase is refused and every reduction passes.
        // There is no way to nibble upward — any increase at all
        // fails the second test once the first is failing.
        // MONOTONIC — `Order::ts_ns` comes from the engine's
        // `core_time::now_ns`, which is `CLOCK_MONOTONIC_RAW`. The
        // ledger converts through its boot anchor; handing it straight
        // to a `wall_ns / DAY_NS` epoch would put two clocks in one
        // field and wipe the day's turnover on every alternation with
        // a (wall-stamped) venue fill.
        self.ledger.observe_mono_clock(order.ts_ns);
        let current = self.ledger.slot_exposure_1e6(slot);
        let projected = self
            .ledger
            .projected_exposure_1e6(slot, order.sym, qty, buy);
        if projected > caps.cap_instance_usd_1e6 && projected > current {
            return self.refuse(order.strategy_id, RiskRefusal::CapInstance);
        }

        // ---- 4. what it would have bought today ----------------------
        // BUYS ONLY. A sell adds no turnover, and testing one would
        // refuse the order that closes a position on a day whose cap
        // is already spent — trapping a member inside exactly the
        // exposure the caps exist to bound.
        if buy {
            let day = self
                .ledger
                .slot_day_turnover_1e6(slot)
                .saturating_add(notional_1e6);
            if day > caps.cap_day_usd_1e6 {
                return self.refuse(order.strategy_id, RiskRefusal::CapDay);
            }
        }

        Ok(())
    }

    /// Count one refusal and name it. Never inlined into four copies
    /// of the same two lines.
    #[inline]
    fn refuse(&mut self, strategy_id: u8, why: RiskRefusal) -> Result<(), DispatchError> {
        self.counters.on_refused_risk(strategy_id, why);
        Err(DispatchError::RiskRefused)
    }
}

/// Which lifecycle verb the risk gate is judging.
///
/// Only [`RiskVerb::Place`] adds to the slot's resting count, so only
/// it can be the order that takes the count over `max_open_orders`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RiskVerb {
    /// A fresh order.
    Place,
    /// A modify — LAW E-7, one resting order swapped for another.
    Replace,
}

impl<P: OrderDispatch, L: OrderDispatch> OrderDispatch for RoutedDispatcher<P, L> {
    /// Route one order. **Hot path.**
    ///
    /// E6: the risk clamp runs on the LIVE arm only. A paper slot is
    /// modelling, and refusing its orders would make the model
    /// disagree with the harness — which replays the same intents
    /// through no such gate — for a reason that has nothing to do
    /// with the strategy. The clamp exists to stop real money
    /// leaving.
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.route.mode(order.strategy_id) {
            ExecMode::Paper => {
                self.counters.on_paper_submit();
                self.paper.submit(order)
            }
            ExecMode::Off => {
                self.counters.on_refused_off(order.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(order.strategy_id, order.venue) {
                    // LAW E-1. NOT `self.paper.submit(order)`.
                    self.counters.on_refused_no_route(order.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                self.risk_check(order, RiskVerb::Place)?;
                self.counters.on_live_submit(order.strategy_id);
                let r = self.live.submit(order);
                if r.is_ok() {
                    // **Only on acceptance.** An order the arm refused
                    // is not working at the venue, and counting it
                    // would leak the resting count upward until
                    // `max_open_orders` refused a slot that had
                    // nothing resting at all.
                    self.ledger.on_submit(
                        order.client_oid,
                        order.strategy_id as usize,
                        order.sym,
                        order.qty.raw(),
                    );
                }
                r
            }
        }
    }

    /// Route one cancel. **LAW E-1 applies to a cancel too, and
    /// harder.**
    ///
    /// A live slot's cancel satisfied by the paper matcher would take
    /// a modelled order out of a modelled book and report success,
    /// while the real quote stays resting at the venue — the strategy
    /// then believes it has no exposure and the venue disagrees. A
    /// mis-routed submit invents a fill; a mis-routed cancel invents
    /// the ABSENCE of one, which nothing downstream can detect.
    ///
    /// Same three-way branch as `submit`, on the cancel's own
    /// `strategy_id`/`venue` — which `StampCtx` stamps exactly as it
    /// stamps an order's.
    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        match self.route.mode(req.strategy_id) {
            ExecMode::Paper => self.paper.cancel(req),
            ExecMode::Off => {
                self.counters.on_refused_off(req.strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.strategy_id, req.venue) {
                    self.counters.on_refused_no_route(req.strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                let r = self.live.cancel(req);
                if r.is_ok() {
                    self.ledger
                        .on_cancel(req.client_oid, req.strategy_id as usize);
                }
                r
            }
        }
    }

    /// Route one modify — LAW E-1 and LAW E-7 together. Identical
    /// branch to `cancel`, on the REPLACEMENT's routing fields: a
    /// modify carries a whole `Order`, and the slot/venue that own
    /// the resting order are the slot/venue that own its replacement
    /// (a modify may not change either — [`core_types::OrderIdentity`]).
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        match self.route.mode(req.order().strategy_id) {
            ExecMode::Paper => self.paper.modify(req),
            ExecMode::Off => {
                self.counters.on_refused_off(req.order().strategy_id);
                Err(DispatchError::SlotDisabled)
            }
            ExecMode::Live => {
                if !self.route.venue_allowed(req.order().strategy_id, req.order().venue) {
                    self.counters.on_refused_no_route(req.order().strategy_id);
                    return Err(DispatchError::NoLiveRoute);
                }
                // **A modify can RAISE size**, so a clamp on `submit`
                // alone leaves the cap reachable by repricing upward
                // — the hole the E5 commit-4b review named. The
                // replacement is measured exactly as a fresh order is.
                self.risk_check(req.order(), RiskVerb::Replace)?;
                let r = self.live.modify(req);
                if r.is_ok() {
                    self.ledger.on_modify(
                        req.prev_client_oid(),
                        req.order().client_oid,
                        req.order().strategy_id as usize,
                        req.order().sym,
                        req.order().qty.raw(),
                    );
                }
                r
            }
        }
    }

    /// Paper fills only — see the module note on §0.1-1. Venue fills
    /// ride the engine's own fill lane 3, which the engine drains
    /// before it ever pumps this.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        self.paper.try_next_fill()
    }

    /// Both arms, summed (plan §0.1-3). Cold: the 5 s tick only.
    fn stats(&self) -> DispatchStats {
        self.paper.stats().merged(self.live.stats())
    }

    /// Every tick reaches the paper matcher, **always and ungated**.
    ///
    /// Even with slot 3 live, the other seven slots' paper orders
    /// still need judging against the book. Gating this on "is any
    /// slot live" would silently freeze the paper matcher for every
    /// member that is still modelling.
    #[inline]
    fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        self.paper.observe_tick(tick, now_ns);
    }

    /// LAW E-2 — the paper arm's numbers, never the live arm's.
    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        self.paper.matcher_counters()
    }

    /// LAW E-2 — likewise.
    #[inline]
    fn open_paper_orders(&self) -> usize {
        self.paper.open_paper_orders()
    }

    /// E1: hand the router's counters and the live route map across the
    /// trait boundary, so the engine loop can mirror them to `/metrics`
    /// without knowing this type. **Cold** — the 5 s tick only.
    /// E4: both arms get the idle moment.
    ///
    /// `|` and NOT `||`: short-circuiting would starve the second arm
    /// every time the first reported work, and the second arm is the
    /// one that owns a venue socket. A hook that runs only when the
    /// other arm is quiet is a hook that stops running exactly when
    /// the engine is busiest.
    ///
    /// This forwarding is why the hook exists at all — `RoutedDispatcher`
    /// is what the `--exec` path wires, so a default `false` here would
    /// leave a live arm's user-event socket unpumped and its budget
    /// state file unwritten, which is the "valid, empty and silent"
    /// failure the user-event module names as the worst one.
    #[inline]
    fn on_idle(&mut self) -> bool {
        let a = self.paper.on_idle();
        let b = self.live.on_idle();
        a | b
    }

    /// Both arms, unconditionally. No short-circuit subtlety here —
    /// this returns nothing, so there is no `|` vs `||` trap the way
    /// there is in `on_idle`; the only requirement is that the LIVE
    /// arm is never skipped, because it is the one that binds.
    #[inline]
    /// **E6** — the arms first, then the ledger.
    ///
    /// The arms keep the ordering they have always had (a roll must
    /// reach the live arm before the member that will quote into the
    /// new instance). The ledger goes last because nothing it does is
    /// visible to either arm, and putting it first would make a
    /// refusal in the binding table look like it came from the venue
    /// path.
    fn on_venue_event(&mut self, event: &core_types::ChannelEvent) {
        self.paper.on_venue_event(event);
        self.live.on_venue_event(event);

        if event.channel != core_types::ChannelId::InstrumentRoll as u8 {
            return;
        }
        // LAW E-4: bound by the roll, never derived. `core_types` owns
        // the one copy of this layout — the codec the ingress writes
        // with, the live arm reads with and the harness replays with.
        //
        // **The STRICT reading of the kind byte**, not
        // `unpack_roll_seq`'s low-bit mask. This is new code with no
        // prior behaviour to preserve, so it has no reason to inherit
        // the permissive reading the older call sites are stuck with.
        // On a byte no packer of ours writes, the masking reading
        // calls `0x03` "settled" — the ledger would zero the row and
        // drop its resting orders — while `strategy_bin15` reads the
        // whole byte and calls the same frame "created" and goes on
        // quoting. Split brain with the risk ledger on the blind side,
        // and every subsequent fill landing as `fills_unbound`.
        // Refused instead.
        let (outcome, _twap_s, family, _settled) =
            core_types::unpack_roll_seq(event.venue_seq);
        match core_types::roll_kind(event.venue_seq) {
            core_types::ROLL_KIND_CREATED => {
                self.ledger.bind(event.venue, family, outcome, event.sym)
            }
            core_types::ROLL_KIND_SETTLED => self.ledger.settle(event.venue, family, outcome),
            _ => self.ledger.refuse_roll(),
        }
    }

    /// **E6** — book the fill into the ledger the clamps read.
    ///
    /// Neither arm is forwarded to, and that is deliberate rather than
    /// an omission. The fill came OUT of one of the two arms, so
    /// handing it back is an echo; handing it ACROSS is LAW E-2's
    /// exact prohibition — the paper matcher must never be shown a
    /// venue fill, or its counters start describing something the
    /// venue did.
    #[inline]
    fn on_fill_booked(&mut self, fill: &Fill) {
        self.ledger.book_fill(fill);
    }

    fn exec_counters(&self) -> ExecCounters {
        let c = self.counters;
        let mut modes = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        for (slot, m) in modes.iter_mut().enumerate() {
            *m = self
                .route
                .mode_at(slot)
                .unwrap_or(ExecMode::Paper)
                .as_u8();
        }
        ExecCounters {
            configured: 1,
            modes,
            live_submits: c.live_submits,
            paper_submits: c.paper_submits,
            refused_off: c.refused_off,
            refused_no_route: c.refused_no_route,
            refused_risk: c.refused_risk,
            live_submits_by_slot: c.live_submits_by_slot,
            refused_by_slot: c.refused_by_slot,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::route::SlotCaps;
    use super::*;

    /// The two crates each name their own slot count (the dependency
    /// runs one way, so neither can import the other's). If they ever
    /// disagree, the per-slot arrays crossing the trait boundary would
    /// silently truncate.
    #[test]
    fn the_slot_counts_of_the_two_crates_agree() {
        assert_eq!(crate::route::EXEC_SLOTS, clob_dispatcher::EXEC_COUNTER_SLOTS);
    }
    use crate::null::NullLiveDispatcher;
    use clob_dispatcher::PaperDispatcher;
    use core_types::{Price, Qty, Side, VenueId, STRATEGY_ID_NONE, STRATEGY_SLOT_BIN15};

    /// A live arm that ACCEPTS, so the tests can tell "routed to live"
    /// apart from "refused". Records what it saw.
    #[derive(Debug, Default)]
    struct SpyLive {
        seen: Vec<u64>,
        cancelled: Vec<u64>,
        modified: Vec<(u64, u64)>,
    }

    impl OrderDispatch for SpyLive {
        fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
            self.seen.push(order.client_oid);
            Ok(())
        }
        fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
            self.cancelled.push(req.client_oid);
            Ok(())
        }
        fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
            self.modified.push((req.prev_client_oid(), req.order().client_oid));
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> DispatchStats {
            DispatchStats {
                accepted: self.seen.len() as u64,
                ..DispatchStats::default()
            }
        }
    }

    /// The anchor the tests build a router with: the identity, so a
    /// fixture's monotonic order stamp and its wall fill stamp are the
    /// same number and land in one day epoch.
    ///
    /// **Production cannot take that shortcut** — the two clocks are
    /// decades apart — which is exactly why an identity anchor must
    /// not be the only one any test uses.
    /// `ledger::tests::the_two_clocks_do_not_thrash_the_day_epoch`
    /// builds a realistic one and is what actually holds the
    /// conversion.
    fn test_anchor() -> core_time::WallAnchor {
        core_time::WallAnchor::new(T0, T0)
    }

    fn order(slot: u8, venue: VenueId, oid: u64) -> Order {
        let mut o = Order::new(
            T0,
            venue,
            42,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    /// The same, at an explicit price and size — the risk gate's
    /// whole input.
    fn order_px_qty(slot: u8, oid: u64, px: i64, qty: i64) -> Order {
        let mut o = Order::new(
            T0,
            VenueId::Hyperliquid,
            42,
            Side::Bid,
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.strategy_id = slot;
        o
    }

    fn bin15_live_table() -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            // The shipped `exec.toml` template's own numbers. A
            // fixture with zero caps would be refused by E6's clamps
            // before it reached whatever the test is about — and `0`
            // means UNSET, which `core_config::exec` refuses at boot.
            SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
        )
        .unwrap();
        r
    }

    #[test]
    fn a_live_slot_reaches_the_live_arm_and_no_other_slot_does() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        for slot in [0u8, 1, 2, 4, 5, 6, 7] {
            assert!(d.submit(&order(slot, VenueId::Hyperliquid, 100 + slot as u64)).is_ok());
        }
        assert_eq!(d.live().seen, vec![1], "only slot 3 went live");
        assert_eq!(d.counters().live_submits, 1);
        assert_eq!(d.counters().paper_submits, 7);
        assert_eq!(d.counters().live_submits_at(3), Some(1));
    }

    /// LAW E-1, the load-bearing test.
    #[test]
    fn a_live_slot_on_a_wrong_venue_is_refused_and_never_falls_back_to_paper() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let before = d.paper().open_paper_orders();
        // Slot 3 is live for Hyperliquid only; Binance has no route.
        let r = d.submit(&order(3, VenueId::Binance, 9));
        assert_eq!(r, Err(DispatchError::NoLiveRoute));
        assert!(d.live().seen.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            before,
            "LAW E-1: the paper matcher must not have taken it either"
        );
        assert_eq!(d.counters().refused_no_route, 1);
        assert_eq!(d.counters().paper_submits, 0);
    }

    #[test]
    fn an_off_slot_is_refused_by_both_arms() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], SlotCaps::none()).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        assert_eq!(
            d.submit(&order(6, VenueId::Hyperliquid, 5)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().seen.is_empty());
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.counters().refused_off, 1);
        assert_eq!(d.counters().refused_at(6), Some(1));
    }

    #[test]
    fn an_unstamped_order_goes_to_paper_even_under_a_live_table() {
        // STRATEGY_ID_NONE == 0xFF; 0xFF & 7 == 7. Arm slot 7 live to
        // make the aliasing maximally tempting.
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            7,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            SlotCaps::none(),
        )
            .unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        assert!(d
            .submit(&order(STRATEGY_ID_NONE, VenueId::Hyperliquid, 11))
            .is_ok());
        assert!(d.live().seen.is_empty(), "un-stamped must never go live");
        assert_eq!(d.counters().paper_submits, 1);
    }

    /// The E1 acceptance property: with an all-paper table the router
    /// is indistinguishable from the bare paper dispatcher on every
    /// observable the trait exposes.
    #[test]
    fn an_all_paper_table_is_indistinguishable_from_a_bare_paper_dispatcher() {
        let mut bare = PaperDispatcher::new();
        let mut routed = RoutedDispatcher::new(
            ExecRoute::all_paper(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        routed.mark_ledger_seeded();

        // A scripted stream over every slot, both venues, both sides.
        let mut oid = 0u64;
        for round in 0..64u64 {
            for slot in [0u8, 1, 2, 3, 4, 5, 6, 7, STRATEGY_ID_NONE] {
                oid += 1;
                let venue = if round % 2 == 0 {
                    VenueId::Hyperliquid
                } else {
                    VenueId::Polymarket
                };
                let o = order(slot, venue, oid);
                assert_eq!(
                    bare.submit(&o),
                    routed.submit(&o),
                    "submit disagreed at oid {oid}"
                );
            }
            let t = Tick::new(
                1_000 + round,
                VenueId::Hyperliquid,
                42,
                round as u32,
                Price::from_raw(499_000),
                Qty::from_raw(1_000_000),
                Price::from_raw(501_000),
                Qty::from_raw(1_000_000),
            );
            bare.observe_tick(&t, 1_000 + round);
            routed.observe_tick(&t, 1_000 + round);

            assert_eq!(
                bare.try_next_fill().map(|f| (f.order_id, f.origin)),
                routed.try_next_fill().map(|f| (f.order_id, f.origin)),
                "fill stream diverged at round {round}"
            );
            assert_eq!(
                bare.matcher_counters(),
                routed.matcher_counters(),
                "matcher counters diverged at round {round}"
            );
            assert_eq!(
                bare.open_paper_orders(),
                routed.open_paper_orders(),
                "open orders diverged at round {round}"
            );
        }

        // Stats: the null live arm contributes nothing at all, so the
        // merged total is the bare total field for field.
        let b = bare.stats();
        let r = routed.stats();
        assert_eq!(b.accepted, r.accepted);
        assert_eq!(b.rejected, r.rejected);
        assert_eq!(b.rejected_queue_full, r.rejected_queue_full);
        assert_eq!(b.rejected_routing, r.rejected_routing);
        assert_eq!(b.fills_seen, r.fills_seen);
        assert_eq!(routed.counters().live_submits, 0);
        assert_eq!(routed.counters().refused_off, 0);
        assert_eq!(routed.counters().refused_no_route, 0);
    }

    // ---------------- E5: routing the lifecycle verbs ----------------

    fn cancel_of(o: &Order) -> CancelReq {
        CancelReq::of(o, 2_000)
    }

    /// **LAW E-1 for a cancel, the load-bearing test.**
    ///
    /// A mis-routed submit invents a fill. A mis-routed cancel
    /// invents the ABSENCE of one: the paper matcher would remove a
    /// modelled order and report success while the real quote stays
    /// resting at the venue, and nothing downstream can detect the
    /// difference.
    #[test]
    fn a_live_slots_cancel_on_a_wrong_venue_never_reaches_the_paper_matcher() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Slot 3 is live for Hyperliquid only. Park a paper order on
        // the matcher so there is a book for a fall-through to reach.
        let decoy = order(0, VenueId::Polymarket, 9);
        assert!(d.submit(&decoy).is_ok());
        assert_eq!(d.paper().open_paper_orders(), 1);

        let mut c = cancel_of(&order(3, VenueId::Binance, 9));
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert!(d.live().cancelled.is_empty(), "never reached the live arm");
        assert_eq!(
            d.paper().open_paper_orders(),
            1,
            "LAW E-1: the paper matcher must not have taken the cancel either"
        );
        // The open count alone would NOT prove that: a fall-through
        // would have been refused by the matcher's own lookup and
        // left the count at 1 anyway. These are what prove the
        // matcher was never asked — every path through
        // `PaperMatcher::cancel` moves exactly one of them.
        let mc = d.paper().matcher_counters();
        assert_eq!(mc.cancels, 0, "the matcher performed no cancel");
        assert_eq!(mc.no_such_order, 0, "the matcher was never even asked");
        assert_eq!(mc.identity_mismatch, 0);
        assert_eq!(mc.ambiguous_order, 0);
        assert_eq!(d.counters().refused_no_route, 1);
    }

    #[test]
    fn a_live_slots_cancel_and_modify_reach_the_live_arm_and_no_other_slots_do() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let live = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&live);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Ok(()));
        assert_eq!(d.live().cancelled, vec![1]);

        let mut repl = order(3, VenueId::Hyperliquid, 2);
        repl.strategy_id = 3;
        assert_eq!(d.modify(&ModifyReq::new(1, repl)), Ok(()));
        assert_eq!(d.live().modified, vec![(1, 2)]);

        // A paper slot's verbs must not touch the live arm.
        let paper = order(0, VenueId::Polymarket, 5);
        assert!(d.submit(&paper).is_ok());
        assert_eq!(d.cancel(&cancel_of(&paper)), Ok(()));
        assert_eq!(d.live().cancelled, vec![1], "still only the live one");
        assert_eq!(d.paper().matcher_counters().cancels, 1);
    }

    #[test]
    fn an_off_slots_cancel_and_modify_are_refused_by_both_arms() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], SlotCaps::none()).unwrap();
        let mut d = RoutedDispatcher::new(r, PaperDispatcher::new(), SpyLive::default(), test_anchor());
        d.mark_ledger_seeded();
        let o = order(6, VenueId::Hyperliquid, 5);
        assert_eq!(d.cancel(&cancel_of(&o)), Err(DispatchError::SlotDisabled));
        assert_eq!(
            d.modify(&ModifyReq::new(4, o)),
            Err(DispatchError::SlotDisabled)
        );
        assert!(d.live().cancelled.is_empty());
        assert!(d.live().modified.is_empty());
        assert_eq!(d.paper().matcher_counters().cancels, 0);
        assert_eq!(d.paper().matcher_counters().no_such_order, 0);
        assert_eq!(d.paper().matcher_counters().identity_mismatch, 0);
        assert_eq!(d.counters().refused_off, 2);
    }

    /// The stub live arm must refuse a lifecycle verb exactly as it
    /// refuses a submit. `Ok` here would be a quote the strategy
    /// stops tracking and the venue never had.
    #[test]
    fn the_null_live_arm_refuses_a_lifecycle_verb_rather_than_swallowing_it() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        let o = order(3, VenueId::Hyperliquid, 1);
        let mut c = cancel_of(&o);
        c.strategy_id = 3;
        assert_eq!(d.cancel(&c), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.modify(&ModifyReq::new(1, o)), Err(DispatchError::NoLiveRoute));
        assert_eq!(d.live().refused(), 2);
        assert_eq!(d.paper().open_paper_orders(), 0);
    }

    // ---------------- E6: the risk gate's per-order clamp ----------

    /// **The clamp, and the thing it is for.** `bin15_live_table` sets
    /// `max_order_usd = $100`; an order for more is refused BEFORE the
    /// live arm sees it, so nothing reaches the venue.
    ///
    /// bin15 sizes against its own caps, so this should never fire —
    /// which is exactly why it is counted. A non-zero `refused_risk`
    /// means the member's ledger and the operator's number disagreed.
    #[test]
    fn an_order_over_the_slots_cap_never_reaches_the_live_arm() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // $100.50: 201 contracts at 0.50.
        let over = order_px_qty(3, 1, 500_000, 201_000_000);
        assert_eq!(d.submit(&over), Err(DispatchError::RiskRefused));
        assert!(d.live().seen.is_empty(), "the venue must never see it");
        assert_eq!(
            d.paper().open_paper_orders(),
            0,
            "and LAW E-1 still holds — a refused live order is not modelled"
        );
        assert_eq!(d.counters().refused_risk, 1);
        assert_eq!(d.counters().refused_at(3), Some(1));
        assert_eq!(d.counters().live_submits, 0);
    }

    /// The boundary is `>`, not `>=`: an order exactly AT the cap is
    /// what an operator who wrote that number asked for.
    #[test]
    fn an_order_exactly_at_the_cap_is_allowed() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Exactly $100.00.
        assert_eq!(d.submit(&order_px_qty(3, 7, 500_000, 200_000_000)), Ok(()));
        assert_eq!(d.live().seen, vec![7]);
        assert_eq!(d.counters().refused_risk, 0);
    }

    /// **A modify can RAISE size**, so a clamp on `submit` alone
    /// leaves the cap reachable by repricing upward — the hole the E5
    /// commit-4b review named. The replacement is measured exactly as
    /// a fresh order is.
    #[test]
    fn a_modify_that_raises_the_order_past_the_cap_is_refused() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // A legal order first, so the modify is the only thing on
        // trial.
        assert_eq!(d.submit(&order_px_qty(3, 1, 500_000, 100_000_000)), Ok(()));
        let bigger = order_px_qty(3, 2, 500_000, 400_000_000); // $200
        assert_eq!(
            d.modify(&ModifyReq::new(1, bigger)),
            Err(DispatchError::RiskRefused)
        );
        assert!(
            d.live().modified.is_empty(),
            "the venue must never be asked to grow it past the cap"
        );
        assert_eq!(d.counters().refused_risk, 1);
    }

    /// A PAPER slot is modelling, and the offline harness replays the
    /// same intents through no such gate. Refusing them here would
    /// make the two disagree for a reason that has nothing to do with
    /// the strategy.
    #[test]
    fn the_clamp_does_not_touch_a_paper_slot() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Slot 0 is paper; the same size that slot 3 was refused for.
        assert_eq!(d.submit(&order_px_qty(0, 1, 500_000, 201_000_000)), Ok(()));
        assert_eq!(d.counters().refused_risk, 0);
        assert_eq!(d.counters().paper_submits, 1);
    }

    /// **The overflow the `i128` is for.** `px × qty` leaves `i64`
    /// at about 9.2e18 — a $4 m price and three contracts reaches it
    /// — and a wrapped product is NEGATIVE, which sails straight past
    /// a `>` test. Saturating to `i64::MAX` would be no better: it is
    /// positive, but it is a number nobody computed.
    #[test]
    fn a_notional_that_would_overflow_i64_is_refused_not_wrapped() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // 4e12 x 3e6: the i64 product wraps negative.
        let absurd = order_px_qty(3, 1, 4_000_000_000_000, 3_000_000);
        assert!(
            (4_000_000_000_000i64).checked_mul(3_000_000).is_none(),
            "the premise: this product does not fit i64"
        );
        assert_eq!(d.submit(&absurd), Err(DispatchError::RiskRefused));
        assert!(d.live().seen.is_empty());
    }

    #[test]
    fn law_e2_matcher_numbers_are_the_paper_arms_even_with_a_live_slot() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Three live orders; the matcher must stay at zero intake.
        for i in 0..3 {
            assert!(d.submit(&order(3, VenueId::Hyperliquid, i)).is_ok());
        }
        assert_eq!(d.matcher_counters().intake, 0, "LAW E-2");
        assert_eq!(d.open_paper_orders(), 0, "LAW E-2");
        assert_eq!(d.live().seen.len(), 3);
    }

    #[test]
    fn exec_counters_cross_the_trait_boundary_intact() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok());
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok());
        assert_eq!(
            d.submit(&order(3, VenueId::Binance, 3)),
            Err(DispatchError::NoLiveRoute)
        );
        let e = d.exec_counters();
        assert_eq!(e.configured, 1);
        assert_eq!(e.live_submits, 1);
        assert_eq!(e.paper_submits, 1);
        assert_eq!(e.refused_no_route, 1);
        assert_eq!(e.refused_off, 0);
        assert_eq!(e.live_submits_by_slot[3], 1);
        assert_eq!(e.refused_by_slot[3], 1);
        assert_eq!(e.modes[3], ExecMode::Live.as_u8());
        assert_eq!(e.modes[0], ExecMode::Paper.as_u8());
    }

    /// A plain paper dispatcher reports NO router — this is what the
    /// cli reads at boot to decide whether `/metrics` grows the
    /// `engine_exec_*` family at all.
    #[test]
    fn a_bare_paper_dispatcher_reports_no_router() {
        let d = PaperDispatcher::new();
        assert_eq!(d.exec_counters().configured, 0);
        assert_eq!(d.exec_counters(), Default::default());
    }

    #[test]
    fn stats_sum_both_arms() {
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert!(d.submit(&order(3, VenueId::Hyperliquid, 1)).is_ok()); // live
        assert!(d.submit(&order(0, VenueId::Polymarket, 2)).is_ok()); // paper
        let s = d.stats();
        assert_eq!(s.accepted, d.paper().stats().accepted + 1);
    }

    #[test]
    fn the_null_live_arm_makes_a_stray_live_order_a_refusal_not_a_paper_fill() {
        // Belt and braces: even if config validation were bypassed and
        // a Live slot pointed at the stub, nothing gets modelled.
        let mut d = RoutedDispatcher::new(
            bin15_live_table(),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        assert_eq!(
            d.submit(&order(3, VenueId::Hyperliquid, 1)),
            Err(DispatchError::NoLiveRoute)
        );
        assert_eq!(d.paper().open_paper_orders(), 0);
        assert_eq!(d.live().refused(), 1);
    }

    // -----------------------------------------------------------------
    // E6 commit 2 — the three ledger-fed clamps
    // -----------------------------------------------------------------

    const OUTCOME: u32 = 20_182;
    /// Hyperliquid namespace, ordinal 900 — the Yes leg; 901 is No.
    const SYM_YES: core_types::SymbolId = (4u32 << 24) | 900;
    const SYM_NO: core_types::SymbolId = (4u32 << 24) | 901;
    /// 2026-09-19T00:00:01Z.
    const T0: u64 = 1_789_776_001_000_000_000;

    /// A live slot 3 with the caps the test names, so each clamp can
    /// be driven to its edge without the other three firing first.
    fn table_with(caps: SlotCaps) -> ExecRoute {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            STRATEGY_SLOT_BIN15 as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            caps,
        )
        .unwrap();
        r
    }

    fn leg_order(oid: u64, sym: core_types::SymbolId, buy: bool, px: i64, qty: i64) -> Order {
        let mut o = Order::new(
            T0,
            VenueId::Hyperliquid,
            42,
            if buy { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.strategy_id = STRATEGY_SLOT_BIN15;
        o.sym = sym;
        o
    }

    fn roll(outcome: u32, sym_yes: core_types::SymbolId, settled: bool) -> core_types::ChannelEvent {
        core_types::ChannelEvent::new(
            T0,
            VenueId::Hyperliquid,
            core_types::ChannelId::InstrumentRoll,
            sym_yes,
            core_types::pack_roll_seq(outcome, 60, 0, settled),
            0,
            0,
            0,
        )
    }

    fn venue_fill(sym: core_types::SymbolId, buy: bool, px: i64, qty: i64, oid: u64) -> Fill {
        Fill::new(
            T0,
            sym,
            if buy { Side::Bid } else { Side::Ask },
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        )
        .with_attribution(STRATEGY_SLOT_BIN15, core_types::FILL_ORIGIN_VENUE)
    }

    fn armed(caps: SlotCaps) -> RoutedDispatcher<PaperDispatcher, SpyLive> {
        let mut d = RoutedDispatcher::new(
            table_with(caps),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        // What E6 commit 3's reconciler will do at boot. Without it
        // every live PLACE is refused — see `mark_ledger_seeded`.
        d.mark_ledger_seeded();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        d
    }

    /// **An unreconciled ledger refuses, it does not guess.**
    ///
    /// A fresh `Ledger` reads zero exposure, zero turnover and zero
    /// resting orders. After a RESTART that is not the truth — the
    /// venue still holds whatever the last boot left — and all three
    /// ledger-fed clamps would fail OPEN: a whole `cap_instance`
    /// addable on top of an existing position, a fresh `cap_day` on
    /// top of the day's real spend, and `max_open_orders` more orders
    /// on top of the ones already working.
    #[test]
    fn a_live_place_is_refused_until_the_ledger_has_been_reconciled() {
        let mut d = RoutedDispatcher::new(
            table_with(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64)),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        assert!(!d.ledger().is_seeded());
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert!(d.live().seen.is_empty(), "it never reached the venue");
        assert_eq!(d.counters().refused_unseeded, 1);
        assert_eq!(
            d.counters().refused_cap_instance,
            0,
            "an unseeded refusal must not read as a cap_instance breach"
        );

        // **A MODIFY IS REFUSED TOO.** An earlier cut exempted it,
        // reasoning that a quote would otherwise be stranded. That
        // was false — a modify RAISES price and size, which is the
        // one thing this interlock exists to stop, and it would have
        // been judged against a ledger reading zero.
        let m = core_types::ModifyReq::new(1, leg_order(2, SYM_YES, true, 510_000, 1_000_000));
        assert_eq!(d.modify(&m), Err(DispatchError::RiskRefused));

        // A CANCEL is the escape hatch, and it always was: it is
        // never risk-checked at all, so nothing is ever stranded.
        let c = core_types::CancelReq::of(&leg_order(1, SYM_YES, true, 500_000, 1_000_000), T0);
        assert!(d.cancel(&c).is_ok());

        d.mark_ledger_seeded();
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_malformed_roll_kind_byte_is_refused_rather_than_read_as_settled() {
        // New code, so it takes the STRICT reading. Under
        // `unpack_roll_seq`'s low-bit mask a `0x03` byte reads as
        // "settled" — the ledger would zero the row and drop its
        // resting orders — while `strategy_bin15` reads the whole byte
        // and calls the same frame "created" and goes on quoting.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        let mut ev = roll(OUTCOME, SYM_YES, false);
        ev.venue_seq |= 0x03u64 << 56;
        let refused_before = d.ledger().counters().binds_refused;
        d.on_venue_event(&ev);
        assert_eq!(d.ledger().counters().binds_refused, refused_before + 1);
        assert_eq!(
            d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize),
            4_000_000,
            "a malformed frame must not clear the position"
        );
    }

    #[test]
    fn a_roll_reaches_the_ledger_through_the_event_the_arms_already_get() {
        // LAW E-4 — the binding comes from the roll, and the router
        // gets the same event the live arm binds from. No cross-arm
        // reach, and one codec (`core_types`) for both readings.
        let d = armed(SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64));
        assert_eq!(d.ledger().counters().binds, 1);
        assert_eq!(d.ledger().position_1e6(STRATEGY_SLOT_BIN15 as usize, OUTCOME), Some((0, 0)));
    }

    #[test]
    fn a_settle_and_then_its_successor_clear_the_instance_through_the_same_path() {
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        // The settle keeps the position — the contracts are held
        // until the settlement pays out — and drops what was resting.
        d.on_venue_event(&roll(OUTCOME, SYM_YES, true));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 4_000_000);

        // The successor's CREATED roll is what retires the instance,
        // and it carries a new outcome id on the same family.
        d.on_venue_event(&roll(OUTCOME + 1, SYM_YES + 2, false));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
    }

    #[test]
    fn the_instance_cap_refuses_the_order_that_would_breach_it() {
        // cap_instance $6. Four contracts of Yes are at stake ($4);
        // three more would be $7.
        let mut d = armed(SlotCaps::new(100_000_000, 6_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 3_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
        assert_eq!(d.counters().refused_risk, 1);
        assert!(d.live().seen.is_empty(), "it never reached the venue");
        // Two more WOULD fit, exactly at the cap: the boundary is `>`.
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 2_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_instance_cap_never_refuses_an_order_that_does_not_raise_exposure() {
        // The property that keeps a cap from trapping a member inside
        // the exposure it is trying to leave, asserted from the WORST
        // position: already over the cap, where the only way out is an
        // order. A clamp testing `projected > cap` alone refuses both
        // of these and leaves no path back but an operator cancelling
        // by hand. (It did, until this test said so.)
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000, 30_000_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 9_000_000, 1));
        assert!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize) > 1_000_000);
        // Selling the long leg.
        assert!(d.submit(&leg_order(2, SYM_YES, false, 500_000, 5_000_000)).is_ok());
        // Buying the SHORT leg — also risk-reducing.
        assert!(d.submit(&leg_order(3, SYM_NO, true, 500_000, 5_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_instance, 0);
        // And the trap door stays shut: from over the cap, an order
        // that RAISES exposure is still refused.
        assert_eq!(
            d.submit(&leg_order(4, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_instance_cap_sees_a_slots_first_order() {
        // Without the projection a flat slot passes any exposure test,
        // so the cap would start biting only on the SECOND order —
        // one order too late, and `max_order_usd` is the only thing
        // that would have stopped the first.
        let mut d = armed(SlotCaps::new(i64::MAX, 1_000_000, 30_000_000_000, 64));
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 500_000, 9_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_day_cap_counts_what_was_bought_and_refuses_the_next_buy() {
        // cap_day $3. One fill of 4 contracts at $0.50 is $2.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 3_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        assert_eq!(d.ledger().slot_day_turnover_1e6(STRATEGY_SLOT_BIN15 as usize), 2_000_000);
        // $1.50 more would be $3.50.
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 3_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_day, 1);
        // Exactly $1 more lands on the cap, and `>` lets it through.
        assert!(d.submit(&leg_order(3, SYM_YES, true, 500_000, 2_000_000)).is_ok());
    }

    #[test]
    fn the_day_cap_never_refuses_a_sell() {
        // A sell adds no turnover. Testing one would refuse the order
        // that closes a position on a day whose cap is already spent.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 1_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 900_000, 9_000_000, 1));
        assert!(d.ledger().slot_day_turnover_1e6(STRATEGY_SLOT_BIN15 as usize) > 1_000_000);
        assert!(d.submit(&leg_order(2, SYM_YES, false, 900_000, 9_000_000)).is_ok());
        assert_eq!(d.counters().refused_cap_day, 0);
    }

    #[test]
    fn the_open_order_cap_counts_what_the_arm_accepted_and_nothing_else() {
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 2));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 2);
        assert_eq!(
            d.submit(&leg_order(3, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_open_orders, 1);
        // A cancel frees a place.
        // `CancelReq::of` copies the identity fields off the order,
        // so the test cannot get them wrong in a way the production
        // path could not.
        let c = core_types::CancelReq::of(&leg_order(1, SYM_YES, true, 500_000, 1_000_000), T0);
        assert!(d.cancel(&c).is_ok());
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 1);
        assert!(d.submit(&leg_order(4, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_modify_is_exempt_from_the_open_order_cap() {
        // LAW E-7: a requote replaces in place, so a modify cannot be
        // the order that takes the count over. Testing it would refuse
        // the requote of a slot sitting exactly at its cap — the slot
        // that most needs to move its quotes.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 1));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 1_000_000)).is_ok());
        let m = core_types::ModifyReq::new(1, leg_order(2, SYM_YES, true, 510_000, 1_000_000));
        assert!(d.modify(&m).is_ok());
        assert_eq!(d.counters().refused_open_orders, 0);
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 1);
        // And the replacement's id is what a fill now matches.
        d.on_fill_booked(&venue_fill(SYM_YES, true, 510_000, 1_000_000, 2));
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 0);
    }

    #[test]
    fn a_refused_submit_does_not_count_against_the_open_order_cap() {
        // Only acceptance counts. An order the arm refused is not
        // working at the venue, and counting it would leak the count
        // upward until the clamp refused a slot holding nothing.
        let mut d = RoutedDispatcher::new(
            table_with(SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 4)),
            PaperDispatcher::new(),
            NullLiveDispatcher::new(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        d.on_venue_event(&roll(OUTCOME, SYM_YES, false));
        for oid in 1..=6u64 {
            assert!(d.submit(&leg_order(oid, SYM_YES, true, 500_000, 1_000_000)).is_err());
        }
        assert_eq!(d.ledger().slot_resting(STRATEGY_SLOT_BIN15 as usize), 0);
        assert_eq!(d.counters().refused_open_orders, 0, "no clamp ever fired");
    }

    #[test]
    fn only_the_first_clamp_to_fire_is_counted() {
        // All four would refuse this order. The order stops at the
        // first, so no second clamp is ever breached, and counting a
        // "would also have failed" would inflate the rate an operator
        // reads.
        let mut d = armed(SlotCaps::new(1, 1, 1, 0));
        assert_eq!(
            d.submit(&leg_order(1, SYM_YES, true, 900_000, 9_000_000)),
            Err(DispatchError::RiskRefused)
        );
        let c = d.counters();
        assert_eq!(c.refused_risk, 1);
        assert_eq!(c.refused_max_order, 1);
        assert_eq!(c.refused_cap_instance, 0);
        assert_eq!(c.refused_cap_day, 0);
        assert_eq!(c.refused_open_orders, 0);
        assert_eq!(
            c.refused_max_order + c.refused_cap_instance + c.refused_cap_day + c.refused_open_orders,
            c.refused_risk,
            "the breakdown sums to the aggregate E6 commit 1 shipped"
        );
    }

    #[test]
    fn the_clamps_are_live_arm_only() {
        // A paper slot is modelling. Refusing its orders would make
        // the model disagree with the harness — which replays the same
        // intents through no such gate — for a reason that has nothing
        // to do with the strategy.
        let mut d = RoutedDispatcher::new(
            ExecRoute::all_paper(),
            PaperDispatcher::new(),
            SpyLive::default(),
            test_anchor(),
        );
        d.mark_ledger_seeded();
        // Every cap is 0 on an all-paper table, which on the live arm
        // refuses everything.
        let mut o = leg_order(1, SYM_YES, true, 900_000, 9_000_000);
        o.strategy_id = 0;
        assert!(d.submit(&o).is_ok());
        assert_eq!(d.counters().refused_risk, 0);
    }

    #[test]
    fn a_paper_fill_does_not_move_a_live_slots_ledger() {
        // LAW E-2's shape, applied to the ledger: the paper matcher's
        // modelled trades put no money at risk and must not consume a
        // live slot's caps.
        let mut d = armed(SlotCaps::new(100_000_000, 1_000_000, 30_000_000_000, 64));
        let f = venue_fill(SYM_YES, true, 900_000, 9_000_000, 1)
            .with_attribution(STRATEGY_SLOT_BIN15, core_types::FILL_ORIGIN_PAPER);
        d.on_fill_booked(&f);
        assert_eq!(d.ledger().slot_exposure_1e6(STRATEGY_SLOT_BIN15 as usize), 0);
        assert!(d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)).is_ok());
    }

    #[test]
    fn a_venue_fill_reaching_the_router_is_what_makes_the_next_order_refusable() {
        // The whole commit in one test. Before the hook existed the
        // router had no way to learn a live fill had happened —
        // `try_next_fill` carries the paper arm only and
        // `on_venue_event` carries market data — so this second order
        // would have passed.
        let mut d = armed(SlotCaps::new(100_000_000, 5_000_000, 30_000_000_000, 64));
        assert!(d.submit(&leg_order(1, SYM_YES, true, 500_000, 5_000_000)).is_ok());
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 5_000_000, 1));
        assert_eq!(
            d.submit(&leg_order(2, SYM_YES, true, 500_000, 1_000_000)),
            Err(DispatchError::RiskRefused)
        );
        assert_eq!(d.counters().refused_cap_instance, 1);
    }

    #[test]
    fn the_day_cap_rolls_on_the_orders_own_clock() {
        // A boot that fills nothing after midnight must not judge the
        // new day's first order against yesterday's turnover.
        let mut d = armed(SlotCaps::new(100_000_000, i64::MAX, 2_000_000, 64));
        d.on_fill_booked(&venue_fill(SYM_YES, true, 500_000, 4_000_000, 1));
        let mut over = leg_order(2, SYM_YES, true, 500_000, 1_000_000);
        assert_eq!(d.submit(&over), Err(DispatchError::RiskRefused));
        over.ts_ns = T0 + 24 * 3_600_000_000_000;
        over.client_oid = 3;
        assert!(d.submit(&over).is_ok(), "a new day, and no fill rolled it");
        assert_eq!(d.ledger().counters().day_rollovers, 1);
    }
}
