// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `HlExchange` — the engine-facing live dispatcher (plan §6).
//!
//! This is the piece the engine can actually reach: an
//! [`OrderDispatch`] that signs and sends, owns the venue
//! relationship, and writes venue fills into engine fill lane 3.
//!
//! It runs on the dispatcher worker thread and nowhere else. That is
//! not a style choice — the fill lane needs exactly ONE writer, and
//! the worker already owns the HTTP socket, the nonce and the budget.
//! A second thread for the user-event stream would need a lock around
//! all of it, on the path that books fills.
//!
//! ## The laws this file is made of
//!
//! * **LAW E-1** — a live slot never falls back to paper. Every
//!   failure here is a refusal, counted; none is a modelled fill.
//! * **LAW E-4** — the asset id is BOUND by a roll event, never
//!   derived. [`submit`](HlExchange::submit) looks it up and refuses
//!   when it is absent, rather than computing one.
//! * **LAW E-5** — the HTTP ack binds `cloid → oid` and surfaces
//!   reject reasons. **It never books a fill.** Fills come from the
//!   user-event stream alone, so [`try_next_fill`] here always
//!   returns `None`: booking through both paths is double-counting,
//!   and this type must not be the one that does it.
//! * **LAW E-9** — the cloid carries the slot, so the fill that comes
//!   back an unknown time later can be routed without a table.
//!
//! ## Order of operations in a submit, and why
//!
//! 1. asset lookup — refuse before anything else is spent;
//! 2. budget check — **before signing**, because a signed action that
//!    is then discarded has still burned a nonce;
//! 3. encode, sign, post;
//! 4. read the ack fail-closed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clob_dispatcher::{DispatchError, DispatchStats, OrderDispatch};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_ring::Producer;
use core_types::{
    CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, NsTs, Order, Side, Tick, VenueId,
};

use crate::action::{encode_order, OrderWire, Tif, MAX_ACTION};
use crate::asset::AssetTable;
use crate::budget::{self, AddressBudget};
use crate::cloid::encode as encode_cloid;
use crate::config::HlConfig;
use crate::http::{HlHttp, MAX_REQ_BODY};
use crate::nonce::Nonce;
use crate::request::{envelope_close, envelope_open, order_json};
use crate::response::{scan, HlOk, HlResponse};
use crate::sign::{sign_action, Network, Vault};
use crate::userws::{scan_user_fills, to_fill, to_fill_as, Routed, TidRing, UserFill, SNAPSHOT_RING};
use crate::userws_conn::UserWs;

/// Engine 1e6 → venue 1e8.
const ENGINE_TO_WIRE: i64 = 100;

/// A booked fill's SIGNED contribution to a position, 1e6.
///
/// Side comes from the venue row rather than the converted `Fill`, so
/// the ledger and the lane cannot disagree about direction through two
/// readings of the same byte.
#[inline]
fn signed_qty_1e6(f: &UserFill, fill: &Fill) -> i64 {
    let q = fill.qty.raw();
    if f.is_buy {
        q
    } else {
        -q
    }
}

/// How often reconciliation asks the venue what it holds.
///
/// Once a minute, weight 2 — negligible against the address budget,
/// and the cadence §6.2 specifies. It is a CEILING on frequency, not a
/// guarantee of it: the check rides the idle path, so a saturated
/// worker reconciles later rather than not at all.
const RECON_EVERY: Duration = Duration::from_secs(60);

/// How often the budget's state file is rewritten.
const PERSIST_EVERY: Duration = Duration::from_secs(5);

/// How long a single idle pump may spend reading the socket.
///
/// A CEILING BETWEEN OPERATIONS, not a hard bound: the underlying
/// poll blocks for its own slice before this is re-checked, so a quiet
/// socket costs that slice. It is on the worker's idle path, which is
/// the right place for a blocking wait — but it is NOT a latency
/// guarantee and must not be read as one.
const PUMP_BUDGET: Duration = Duration::from_millis(20);

/// Reconnect backoff for the user-event socket.
///
/// Mandatory, not politeness: without it a black-holing socket is
/// retried every `WORKER_IDLE_BACKOFF` (tens of microseconds) on the
/// only thread that also submits orders.
const WS_BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

/// **S7-L1** — passes the account-wide sweep makes: ask the venue,
/// cancel what is ours, ask again. Three is two retries for an order
/// that survived its cancel (a fill racing it, a venue blip).
const SWEEP_ALL_ROUNDS: u32 = 3;

/// **S7-L1** — the account-wide sweep starts no request after this
/// long. The drain it runs in has its own, longer deadline
/// (`cli::sigint::DRAIN_DEADLINE_S`); this keeps the sweep inside it
/// with room for the rest of the drain.
const SWEEP_ALL_DEADLINE: Duration = Duration::from_secs(12);

/// **S7-L1** — the request-weight top-up is checked at most this
/// often, whatever the check found or did.
const TOPUP_EVERY: Duration = Duration::from_secs(60);

/// **S7-L1** — the venue's price of one reserved request, USD ×1e6
/// (0.0005 USDC, "paid from the Perps balance").
const TOPUP_PRICE_1E6: i64 = 500;

/// **S7-L1** — the first wait after a failed day-spend read; each
/// further failure doubles it, up to [`DAY_RETRY_MAX`].
const DAY_RETRY_MIN: Duration = Duration::from_secs(60);

/// **S7-L1** — the longest wait between failed day-spend reads.
const DAY_RETRY_MAX: Duration = Duration::from_secs(900);

/// Counters an operator reads on `/metrics`.
#[repr(C, align(64))]
#[derive(Debug, Default, Copy, Clone)]
pub struct HlExecCounters {
    /// Orders accepted by the venue.
    pub submitted: u64,
    /// Orders the venue refused.
    pub rejected: u64,
    /// IoCs the venue UNDERSTOOD and could not match (E7-F2). Not in
    /// [`Self::rejected`] and not in the reject streak: the order was
    /// priced against the book and found no counterparty, which is
    /// the coverage entry's ordinary outcome, not the venue saying no.
    pub ioc_missed: u64,
    /// Submits refused locally, before any packet left.
    pub refused_local: u64,
    /// **Actions that reached the wire and whose answer we never
    /// read.** The budget counted them — it must, or the governor
    /// drifts optimistic — but nothing in this process knows what the
    /// venue did with them.
    ///
    /// A non-zero value means there may be an order resting at the
    /// venue that no local book has an id for. It is the single most
    /// direct reason to run E6's reconciliation, and before E5 it was
    /// invisible: the same failure silently under-counted the budget
    /// instead.
    pub sent_unanswered: u64,
    /// The subset of [`Self::refused_local`] that were LAW E-4
    /// staleness refusals — the order named an instance the table has
    /// rolled past. **The most important refusal this module makes**,
    /// and the reason it is not merely folded into the total: an order
    /// that would have gone to someone else's market must not read
    /// like a full ring.
    pub refused_stale: u64,
    /// Venue fills routed into fill lane 3.
    pub fills_booked: u64,
    /// Venue fills that were NOT ours — counted, never routed.
    pub fills_foreign: u64,
    /// Fills the lane could not accept (ring full). **A dropped fill
    /// is a position the engine does not know it has.**
    pub fills_dropped: u64,
    /// Ours, but the venue's coin name could not be resolved to an
    /// engine symbol. **Counted and NOT booked.**
    pub fills_unresolved: u64,
    /// Fills REFUSED by the converter — zero or negative quantity.
    /// A refusal, not a loss: distinct from [`Self::fills_dropped`],
    /// whose whole meaning is "a position the engine does not know it
    /// has". One alarm that is permanently noisy is no alarm.
    pub fills_refused: u64,
    /// A frame that WAS `userFills` and could not be scanned —
    /// overflow, or a shape we refuse. **Never conflated with a frame
    /// from another channel**, because discarding a whole reconnect
    /// snapshot in silence is precisely the failure this counter
    /// exists to make loud.
    pub fills_scan_failed: u64,
    /// Fills that were the venue SETTLING an instance rather than a
    /// trade. **Booked like any other fill** (operator ruling,
    /// 2026-09-15: the venue is the truth, and a payout is exactly a
    /// sale at 1.0 or 0.0) — counted separately only so an operator
    /// watching a position go flat can tell settlement from a trade.
    pub fills_settlement: u64,
    /// Reconciliation cycles that COMPLETED — the venue answered and
    /// the answer parsed.
    pub recon_ok: u64,
    /// Reconciliation cycles that did not complete: the venue was
    /// unreachable, answered with something unparseable, or the reply
    /// overflowed the scratch. **Counted, never retried in a tight
    /// loop** — the next idle tries again a minute later.
    pub recon_failed: u64,
    /// Legs where the venue's balance and this arm's own booked
    /// quantity DISAGREED at the last reconciliation.
    ///
    /// **The single most valuable number this arm produces.** It
    /// catches a lost fill, a double-counted fill, a wrong asset id and
    /// a stale position view with one comparison, and it is the only
    /// check independent of every belief the engine holds — the
    /// comparison is against what we BOOKED, not what a member thinks.
    pub recon_drift_legs: u64,
    /// Outcome legs the VENUE holds with a non-zero balance that no
    /// bound leg matched at the last reconciliation — the comparison's
    /// blind spot, measured from the side that can see it
    /// (`recon::unreconciled_venue_legs`). Non-zero after a restart
    /// until the retired instance settles; non-zero at any other time
    /// is a position this arm is not tracking, and `reconciled` stays
    /// false while it is.
    pub recon_unseen_legs: u32,
    /// Explicit padding after the one `u32` in this struct.
    pub _pad_recon: u32,
    /// The largest single-leg disagreement seen, as a **CONTRACT
    /// QUANTITY**, 1e6. Not a running total: a drift that appears and
    /// is corrected still leaves its mark here.
    ///
    /// The name carries the unit because the halt rule E6 will write
    /// is `halt_on_recon_drift_usd_1e6` — **dollars**, and the two
    /// share a scale suffix while meaning different things. Put this
    /// through [`crate::recon::drift_qty_to_usd_1e6`] before comparing
    /// it to anything denominated in money.
    pub recon_drift_max_qty_1e6: i64,
    /// Symbols two different strategy slots have both traded. The
    /// binding stops naming an owner, so their settlements are counted
    /// and never booked — guessing between two claimants is the
    /// misattribution this module exists to prevent. **A
    /// configuration error, not a market event**: one symbol belongs
    /// to one member.
    pub owner_contested: u64,
    /// Settlements for a leg NO member has ever traded, so
    /// the binding records no owner. Counted, never booked: a position
    /// cannot exist in a leg nothing traded, and guessing a slot is the
    /// misattribution this module exists to prevent.
    pub fills_unowned: u64,
    /// Fills whose VENUE TIMESTAMP did not convert to nanoseconds.
    /// The fill is still booked, stamped with the local receive clock:
    /// a position is real whatever the venue says the time was, and a
    /// clamped `u64::MAX` would place it around the year 2554 in a
    /// tape that is read in time order. The counter is the only record
    /// that the stamp is ours and not the venue's — and it reaches no
    /// gauge until `stats()` is wired (risk-policy §LAW E-9, item 5),
    /// so today it is visible to a test and not to an operator.
    pub fills_bad_ts: u64,
    /// User-event socket reconnects that SUCCEEDED.
    pub ws_reconnects: u64,
    /// Connect attempts that failed. Counted separately, because a
    /// permanent reconnect loop otherwise shows as zero reconnects and
    /// looks like a healthy quiet socket.
    pub ws_connect_failures: u64,
    /// Encode or sign refusals — local, before anything was sent.
    pub encode_failures: u64,
    /// Cancels the venue ACCEPTED.
    pub cancels_sent: u64,
    /// Cancels refused, locally or by the venue.
    pub cancels_refused: u64,
    /// Modifies the venue ACCEPTED (LAW E-7 — a requote is one of
    /// these, never a cancel plus a place).
    pub modifies_sent: u64,
    /// Modifies refused, locally or by the venue.
    pub modifies_refused: u64,
    /// Roll sweeps ATTEMPTED (LAW E-8).
    pub sweeps_run: u64,
    /// Orders a sweep took off the book.
    pub sweep_cancelled: u64,
    /// **Orders a sweep could NOT take off the book**, after its
    /// retries were spent. Quotes resting on a dead instance, which is
    /// the number E6's kill switch will read.
    ///
    /// **It counts in two units and a threshold must know which.** Two
    /// paths increment it per ENTRY rather than per order: a queue
    /// overflow, and a sweep whose retries ran out — and in the
    /// truncated case one increment can stand for many orders. So it is
    /// a LOWER BOUND on orders left resting, never an exact count. Same
    /// trap as `recon_drift_max_qty_1e6` against
    /// `halt_on_recon_drift_usd_1e6`: two numbers sharing a name and
    /// meaning different things is how a threshold gets compared to the
    /// wrong quantity — and the reason the
    /// sweep counts its failures rather than halting on them: a halt
    /// invented here would be a policy this file made up, the same
    /// reasoning that keeps `reconcile` from halting.
    pub sweep_left: u64,
    /// Sweeps that could not even enumerate — the venue was unreachable
    /// or its answer unreadable. Distinct from `sweep_left`: one is
    /// "we know what is resting and could not cancel it", the other is
    /// "we do not know what is resting". Retried on the next idle.
    pub sweep_failed: u64,
    /// `outcomeCreated` rolls that bound BOTH legs.
    pub rolls_bound: u64,
    /// `outcomeSettled` rolls. The binding is deliberately KEPT — see
    /// [`HlExchange::on_venue_event`].
    pub rolls_settled: u64,
    /// Rolls that bound nothing: no outcome id, an id past the u32
    /// bound, or a table with no room for BOTH legs. The name is
    /// exact: everything that can fail is checked before the first
    /// mutation, so a refused roll really did bind nothing.
    ///
    /// An unbound leg silently books no fills, so this wants to be
    /// loud — and today it is not. Like every counter here it reaches
    /// no gauge until `stats()` is wired (risk-policy pre-arming item
    /// 5), so it is visible to a test and not to an operator.
    pub rolls_refused: u64,
    /// **E6 commit 3a** — sweep calls that stopped early because they
    /// had spent [`SWEEP_CANCELS_PER_IDLE`], or could not see the end
    /// of the selection, and will continue on the next idle moment.
    /// NOT a failure and NOT a spent retry; a steady stream of these
    /// is one big sweep in progress, which is what keeping the engine
    /// thread responsive looks like.
    ///
    /// **Appended, not inserted.** Placing it beside `sweeps_run`
    /// would have shifted the offset of every field after it, and a
    /// `#[repr(C)]` counter block is the kind of thing something
    /// reads positionally one day.
    pub sweep_deferred: u64,
    /// **E6 commit 3a** — sweep entries that hit
    /// [`SWEEP_MAX_DEFERS`]: the selection stopped shrinking between
    /// calls, so the entry fell back to spending retries. Unreachable
    /// while the venue's `frontendOpenOrders` view reflects our own
    /// cancels; non-zero means it does not, and that the sweep is
    /// making no progress.
    pub sweep_stalled: u64,
    /// **E6 commit 3** — `cancel_all` calls. One per halt edge, plus
    /// one per retry while the venue is not yet clear.
    pub cancel_all_runs: u64,
    /// **E6 commit 3** — live legs `cancel_all` could not queue a
    /// sweep for, because the pending-sweep table was full. Non-zero
    /// means a halted arm still has orders the engine has not asked
    /// the venue to remove.
    pub cancel_all_unqueued: u64,
    /// **E7 session bound** — the anchor was set in memory and the
    /// durable write of `exec-pnl-anchor.state` FAILED. This process
    /// still judges the bound; the next boot re-anchors from its own
    /// first flat balance, which is the thing the file exists to
    /// prevent. Appended, like `sweep_deferred`, for the same reason.
    pub anchor_persist_failed: u64,
    /// **S7-L1** — cancels the account-wide sweep (boot and shutdown,
    /// [`HlExchange::cancel_ours_everywhere`]) had the venue confirm.
    /// A total across both sweeps.
    pub sweep_all_cancelled: u64,
    /// **S7-L1** — orders of ours the LAST account-wide sweep could
    /// not confirm gone. A level, not a total; `u64::MAX` = the venue's
    /// open orders could not be read at all, which is NOT clear.
    pub sweep_all_left: u64,
    /// **S7-L1** — `userFillsByTime` day-spend reads that parsed.
    pub day_sync_ok: u64,
    /// **S7-L1** — day-spend reads that did not (transport, parse, or
    /// a page the venue may have cut short).
    pub day_sync_failed: u64,
    /// **S7-L1** — `reserveRequestWeight` top-ups the venue accepted.
    pub topup_ok: u64,
    /// **S7-L1** — top-up checks that failed: the venue's budget could
    /// not be read, or the purchase was not accepted.
    pub topup_failed: u64,
}

/// Which budget rule an action answers to.
///
/// **A cap never blocks an EXIT.** `AddressBudget` has said so since it
/// was written — `may_cancel` returns `true` unconditionally, with the
/// note that "a halted engine must be able to flatten" — and until E5
/// that method had NO CALLERS, because `submit` was the only verb and
/// it is a submit.
///
/// Folding cancels into `may_submit` would invert the rule at exactly
/// the wrong moment: the engine's cancel verb would stop working at the
/// budget floor, which is a bad day, and the LAW E-8 sweep would
/// convert a transient squeeze into `sweep_left` — orders "left resting
/// on a dead instance", the number E6's kill switch reads — while the
/// real cause was our own governor, and drop the entry for good.
///
/// Cancels still COUNT against the address (`on_action_sent` fires for
/// every action, cancels included). They are never REFUSED by it.
/// Where the budget the arm booted with came from (E7-F1).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// `/info userRateLimit` answered at boot — the venue's own figures.
    Venue,
    /// The venue did not answer; the state file did.
    File,
    /// Neither — the cold assumption (remaining 0, under any floor).
    Cold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spend {
    /// Anything that can open or move exposure. Refused at the floor.
    Submit,
    /// An exit. Counted, never refused.
    Cancel,
}

/// A leg whose instance has ENDED and whose resting orders have not
/// been taken off the book yet (LAW E-8).
///
/// The coin bytes are CARRIED rather than derived from the asset id:
/// a name derived from an id is the same class of guess LAW E-4
/// refuses in the other direction, and this one is used to decide
/// which of the venue's open orders to cancel.
#[repr(C)]
#[derive(Clone, Copy)]
struct PendingSweep {
    /// The venue asset id, for the cancel wire.
    asset: u32,
    /// How many attempts are left before the entry is dropped and
    /// counted as `sweep_left`.
    tries: u8,
    coin_len: u8,
    /// **E6 commit 3a — how many times this entry has deferred.**
    ///
    /// The runaway guard. See [`SWEEP_MAX_DEFERS`].
    defers: u16,
    coin: [u8; crate::asset::COIN_MAX],
    _pad2: [u8; 8],
}

impl PendingSweep {
    const EMPTY: Self = Self {
        asset: 0,
        tries: 0,
        coin_len: 0,
        defers: 0,
        coin: [0; crate::asset::COIN_MAX],
        _pad2: [0; 8],
    };
}

/// How many ended legs can await a sweep at once.
///
/// A roll retires two legs, eight families roll on a quarter-hour, and
/// the idle path drains one per call — so this is far above anything
/// the member explains. Overflow is counted as `sweep_left` rather
/// than silently dropped: a leg nobody swept and nobody counted is the
/// stranded quote LAW E-8 exists to prevent.
const MAX_PENDING_SWEEPS: usize = 16;

/// Attempts a pending sweep gets before it is given up on and counted.
///
/// Bounded because "retry on the next idle" without a bound is a leg
/// that burns the address budget forever. When they are spent the
/// entry becomes `sweep_left`, which is precisely the number E6 arms
/// on.
const SWEEP_TRIES: u8 = 8;

/// **Cancels one [`HlExchange::sweep_one_pending`] may send per idle
/// moment.**
///
/// `crate::recon::ours_on_leg` can select up to
/// [`crate::recon::MAX_OPEN_ORDERS`] (256) oids and each cancel is its
/// own HTTPS round trip bounded by [`crate::http::REQ_DEADLINE`]
/// (5 s). E6 commit 3a put `on_idle` on the ENGINE THREAD for the
/// `--exec` path, so cancelling a whole selection in one call is up
/// to ~21 minutes of engine stall in the pathological case and ~13 s
/// at a realistic 50 ms a round trip — during which no ring drains,
/// `shutdown_requested()` is never reached, and E6's halt machine
/// cannot run on the very thread a dead venue is blocking.
///
/// 8 holds the worst case per call to ~40 s of deadline and ~0.4 s in
/// practice. The remainder is not dropped: the sweep entry stays
/// pending and the next idle moment continues it, through the retry
/// machinery this function already had — and idle moments come round
/// every 2 ms.
const SWEEP_CANCELS_PER_IDLE: usize = 8;

/// How many times one sweep entry may defer before it stops deferring
/// and starts spending retries.
///
/// **The invariant deferral rests on, and the guard for when it does
/// not hold.** `sweep_one_pending` re-asks the VENUE on every call —
/// it POSTs `frontendOpenOrders` and re-runs
/// `crate::recon::ours_on_leg` over the fresh answer — so an order
/// cancelled last call is no longer listed and the selection shrinks
/// by what was cancelled. That is what makes 8-at-a-time terminate,
/// and it is a property of re-fetching rather than of this file.
///
/// If it ever stops holding — a venue that keeps listing a cancelled
/// order AND accepts the re-cancel, so nothing fails and nothing
/// completes — deferral would never spend a retry and the entry would
/// sit there issuing 8 HTTPS round trips every 2 ms for the life of
/// the boot, on the engine thread. Past this many deferrals the entry
/// falls back to spending retries, so it terminates either way.
///
/// **A TOTAL count, and the size is the whole argument.** The
/// obvious alternative — count only CONSECUTIVE calls where the
/// selection failed to shrink — does not work, because the selection
/// cannot reveal progress while the leg is larger than the buffer: a
/// 312-order leg reports 256 selected on every call for the first
/// seven, even though eight orders really are being cancelled each
/// time. That reading fires the guard on a sweep that is working.
///
/// So the bound is sized against the largest leg a VALID
/// configuration can produce. `core_config::exec` refuses a live slot
/// above `MAX_OPEN_ORDERS_PER_SLOT` (64) and there are 8 slots, so no
/// leg can legitimately carry more than 512 of our orders — 64
/// deferrals at [`SWEEP_CANCELS_PER_IDLE`] a call. 256 is four times
/// that: it cannot fire on a sweep that is merely long, and it still
/// terminates an entry that is making no progress at all within
/// about half a second of idle moments.
const SWEEP_MAX_DEFERS: u16 = 256;

/// What one [`HlExchange::sweep_one_pending`] call should do with a
/// selection of `selected` oids: `(send now, defer to the next idle)`.
///
/// A free function so the budget is TESTABLE. Inline, it sits inside a
/// method that needs a scripted TLS endpoint and a live `HlExchange`
/// to reach, and break-and-watch confirmed the obvious: removing the
/// cap failed nothing.
#[inline]
#[must_use]
const fn sweep_plan(selected: usize) -> (usize, u32) {
    let now = if selected < SWEEP_CANCELS_PER_IDLE {
        selected
    } else {
        SWEEP_CANCELS_PER_IDLE
    };
    (now, (selected - now) as u32)
}

/// What a sweep call learned about the entry it was working on.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SweepOutcome {
    /// Every selected order is cancelled and the venue's answer was
    /// complete. Drop the entry.
    Done,
    /// Nothing failed; the call simply spent its budget. Keep the
    /// entry and continue on the next idle moment — **without
    /// spending a retry**, because stopping early was the plan.
    Deferred,
    /// Something failed, or the selection was truncated. Keep the
    /// entry and burn one of [`SWEEP_TRIES`].
    Retry,
}

/// Classify a sweep call. `deferred` is budget, `failed` is failure,
/// and conflating them is what would abandon a 256-order sweep after
/// 64 with the rest reported as `sweep_left` — quotes left resting on
/// a retired instance, which is the one thing LAW E-8 exists to
/// prevent.
///
/// **`truncated` is MORE WORK, not failure**, and the first cut of
/// this function got that wrong in a way the budget itself made
/// dangerous. `truncated` means the selection filled the buffer —
/// `k == MAX_OPEN_ORDERS`. Before the budget, one call cancelled all
/// 256, so a leg of N orders was truncated for about `N / 256` calls.
/// After it, progress is 8 a call, so the selection sits AT the
/// ceiling for roughly `(N − 256) / 8` calls — and returning `Retry`
/// for each of them spends one of [`SWEEP_TRIES`] (8) every time.
/// Past **N > 312** the retries run out and the entry is dropped with
/// the remainder reported as `sweep_left`. Commit 2's boot rule
/// allows 64 open orders on each of 8 slots, so a leg of 512 is a
/// valid configuration and that abandonment is reachable.
///
/// So truncation with a clean budget defers like any other
/// incomplete call. It is still never `Done` — we could not see the
/// end of the selection — and a real `failed` still wins, because
/// then there IS something to retry rather than merely continue.
#[inline]
#[must_use]
const fn classify_sweep(failed: u32, deferred: u32, truncated: bool) -> SweepOutcome {
    if failed > 0 {
        return SweepOutcome::Retry;
    }
    if deferred > 0 || truncated {
        return SweepOutcome::Deferred;
    }
    SweepOutcome::Done
}

/// The live Hyperliquid dispatcher.
pub struct HlExchange<const FILL_N: usize> {
    http: HlHttp,
    ws: UserWs,
    sk: secp256k1::SecretKey,
    network: Network,
    nonce: Nonce,
    budget: AddressBudget,
    /// Where `budget` came from at boot — for the boot tell.
    budget_source: BudgetSource,
    seen: TidRing<SNAPSHOT_RING>,
    assets: AssetTable,
    fills: Producer<Fill, FILL_N>,
    counters: HlExecCounters,
    budget_path: PathBuf,
    last_persist: Instant,
    /// **E7 session bound** — the account's equity AT COST (spot USDC
    /// plus the held legs' `entryNtl`, ×1e6) at the first
    /// reconciliation of the session, restored from `pnl_path` at boot
    /// or set by [`Self::note_account`]. `0` = not anchored yet. Never
    /// moved once set: a session's bound is judged from where it
    /// started (see [`crate::anchor`]). On a flat account it is the
    /// spot USDC — what the first cut anchored on, so a file it wrote
    /// reads the same.
    pnl_anchor_1e6: i64,
    /// Spot USDC (×1e6) at the last reconciliation that parsed.
    usdc_1e6: i64,
    /// What the legs held at that reconciliation cost (Σ `entryNtl`,
    /// ×1e6) — the other half of the equity the bound is judged on.
    held_cost_1e6: i64,
    /// Outcome legs the venue held at that reconciliation. For the
    /// operator; the bound is judged whether or not one is held.
    legs_held: u32,
    /// Explicit padding after the one `u32` in this run of fields.
    _pad_legs: u32,
    /// Where the anchor lives — beside the budget file
    /// (`exec-pnl-anchor.state`).
    pnl_path: PathBuf,
    /// Consecutive connect failures, indexing [`WS_BACKOFF`].
    ws_fail_streak: u32,
    /// **E6 commit 3** — when the user-event stream was last known
    /// ALIVE: a pump that returned without error, whether or not it
    /// carried messages. A quiet socket is alive; a socket that
    /// cannot be read is not.
    ///
    /// `None` = never up, which reports as "no observation" rather
    /// than as an infinite gap — an arm that has not connected yet is
    /// not an arm that has gone quiet, and halting a boot before its
    /// first connect would make the engine unstartable.
    ///
    /// `Instant`, not a `core_time` stamp, because every other timer
    /// in this file is one and a second clock here would be a second
    /// thing to get wrong.
    last_ws_ok: Option<Instant>,
    /// **E6 commit 3** — CONSECUTIVE orders the venue understood and
    /// refused. An acceptance resets it.
    reject_streak: u32,
    /// **E6 commit 3** — CONSECUTIVE submits refused locally for
    /// naming a rolled instance (LAW E-4). An acceptance resets it.
    asset_refusal_streak: u32,
    /// **E6 commit 3** — `true` once `reconcile` has completed a
    /// comparison against the venue since boot. What lets the router
    /// stop refusing every live place.
    reconciled: bool,
    /// Earliest instant a reconnect may be attempted.
    ws_retry_at: Instant,
    /// When reconciliation last ran.
    last_recon: Instant,
    /// **E7-F3** — the worst single-leg disagreement of the PREVIOUS
    /// reconciliation, contracts ×1e6. A disagreement reaches the
    /// high-water mark the halt reads only when the next cycle sees
    /// one too: the venue's sheet is updated before its `userFills`
    /// push reaches this arm, and a reconciliation that lands in that
    /// gap sees a leg we have not booked yet. Mainnet 2026-09-19
    /// 15:32:22Z: the reconcile ran in the same second as an entry
    /// fill, read 2 contracts against 0 booked, and the $2 threshold
    /// latched `recon-drift` for good over a fill that was booked
    /// milliseconds later. A lost fill survives 60 s; a race does not.
    recon_prev_worst: i64,
    /// The MASTER account reconciliation asks about — the agent signs
    /// on its behalf and the venue reports balances under it.
    master_addr: [u8; 20],
    /// Legs whose instance has ENDED and whose resting orders have not
    /// been swept yet (LAW E-8).
    sweeps: [PendingSweep; MAX_PENDING_SWEEPS],
    /// How many of `sweeps` are live.
    sweeps_n: usize,
    /// `sweep_left` as it stood when the last cancel-all was
    /// REQUESTED. A sweep that terminates by abandonment bumps that
    /// counter, so `sweep_left != cancel_all_mark` is exactly "a leg
    /// was given up on since we were asked" — which is what makes
    /// `cancel_all_state` able to say `Stranded` instead of quietly
    /// reporting an empty table as a clear venue.
    cancel_all_mark: u64,
    /// `cancel_all_unqueued` as it stood when the last cancel-all was
    /// REQUESTED. A leg the request could not queue is a leg nobody
    /// is sweeping; the first cut's `cancel_all_state` was blind to
    /// it and reported `Clear` once the legs it DID queue drained,
    /// so the router zeroed every slot's resting count over quotes
    /// still live at the venue (E7 review, 2026-09-19).
    cancel_all_unqueued_mark: u64,
    /// When a reconciliation last COMPLETED a comparison the arm was
    /// willing to stand behind. `None` = never. `last_recon` stamps
    /// the attempt; this stamps the success, and the gap between them
    /// is the `recon_age_ns` halt trigger — without it a reconciler
    /// failing every cycle was indistinguishable from a healthy one.
    last_recon_ok: Option<Instant>,
    /// Scratch for one sweep's `frontendOpenOrders` answer.
    open: Box<[crate::recon::OpenOrder]>,
    /// The oids `ours_on_leg` selects from `open`, once per sweep call.
    oids: Box<[u64]>,
    /// Scratch for one `spotClearinghouseState` answer. Boxed and
    /// sized for the venue's full reply, which is NOT the number of
    /// coins we hold — a one-coin account came back with fourteen
    /// rows.
    bal: Box<[crate::recon::SpotBalance]>,
    /// The msgpack of the action being sent — the SIGNER's input.
    /// Preallocated at boot (plan §4.2), never on the stack: the first
    /// cut zeroed 4 KiB of it per action, and re-declared it inside
    /// the sweep loop.
    mp: Box<[u8]>,
    /// The request body, built IN PLACE: envelope head, then the
    /// action JSON rendered straight into it, then the signature
    /// tail. One buffer, one write per byte, no staging copy.
    req: Box<[u8]>,
    /// Scratch for one frame's fills. **Boxed and sized for the
    /// venue's SNAPSHOT**, not for a steady-state frame — see
    /// [`HlExchange::pump_user_events`].
    scratch: Box<[UserFill]>,
    /// **S7-L1** — scratch for the account-wide sweep's selection:
    /// `(asset, oid)` of every order of ours the venue reports resting.
    /// A boot buffer, sized like `open`.
    wires: Box<[crate::action::CancelWire]>,
    /// **S7-L1** — the operator arm's restart safety is on
    /// ([`HlExchange::enable_restart_safety`]): the day-spend read (gap
    /// A) and a clean read of the account's resting orders (gap C) both
    /// gate the seeding verdict. Off for an arm that never turns it on,
    /// which keeps exactly its old behaviour.
    restart_safety: bool,
    /// **S7-L1 (gap C)** — a read of the whole account's resting orders
    /// has found nothing of ours since boot. Until then, with restart
    /// safety on, the seeding verdict waits and each reconciliation
    /// retries one sweep round.
    orphans_clear: bool,
    /// **S7-L1 (gap A)** — what each slot BOUGHT on the venue since
    /// 00:00Z of `day_epoch`, USD ×1e6 ([`crate::dayspend`]).
    day_bought_1e6: [i64; core_config::exec::EXEC_SLOTS],
    /// `wall_ms / DAY_MS` of the day `day_bought_1e6` is for; `0` =
    /// not read since boot. The seeding verdict waits for it.
    day_epoch: u64,
    /// Earliest instant a failed day-spend read may be tried again.
    day_retry_at: Instant,
    /// The wait after the next failed read: [`DAY_RETRY_MIN`] doubling
    /// to [`DAY_RETRY_MAX`], back to the minimum on a success.
    day_backoff: Duration,
    /// **S7-L1 (gap E)** — request weight bought per top-up; `0` = off.
    topup_weight: u64,
    /// The most weight the top-up may buy in one UTC day.
    topup_day_max: u64,
    /// The UTC day `topup_used` counts.
    topup_day: u64,
    /// Weight SENT on `topup_day`. Persisted with the day in
    /// `exec-topup.state`, so a restart does not hand the day its
    /// ceiling again.
    topup_used: u64,
    /// What this SESSION's top-ups cost, USD ×1e6 — subtracted from the
    /// session P&L, because the perps balance it is paid from is not in
    /// the equity the reconciler reads. Persisted with the anchor it
    /// belongs to.
    topup_cost_1e6: i64,
    /// When the current anchor was set (unix s), `0` = none: what ties
    /// `topup_cost_1e6` in the state file to THIS session.
    anchor_set_s: u64,
    /// Earliest instant the next top-up check may run.
    topup_next_at: Instant,
    /// Where `exec-topup.state` lives — beside the budget file.
    topup_path: PathBuf,
}

impl<const FILL_N: usize> HlExchange<FILL_N> {
    /// Build the arm. Opens nothing; the first submit dials.
    ///
    /// # Errors
    /// The configuration's key is unusable, or a socket could not be
    /// constructed (bad host).
    pub fn new(
        cfg: &HlConfig,
        tls: Arc<rustls::ClientConfig>,
        fills: Producer<Fill, FILL_N>,
        budget_path: PathBuf,
        floor: u64,
    ) -> Result<Self, crate::config::ConfigErr> {
        let sk = cfg.secret_key()?;
        let http = HlHttp::new(&cfg.host, 443, tls.clone())
            .map_err(|_| crate::config::ConfigErr::BadHex("HYPERLIQUID host"))?;
        let ws = UserWs::new(&cfg.host, 443, tls, &cfg.master_addr)
            .map_err(|_| crate::config::ConfigErr::BadHex("HYPERLIQUID host"))?;
        let budget = budget::load(&budget_path, cfg.master_addr, floor);
        let budget_source = if budget == AddressBudget::cold(cfg.master_addr, floor) {
            BudgetSource::Cold
        } else {
            BudgetSource::File
        };
        // E7 session bound: the anchor lives beside the budget file
        // and, like it, is restored only for THIS master address.
        let pnl_path = budget_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default()
            .join(crate::anchor::DEFAULT_STATE_PATH);
        let anchor = crate::anchor::load(&pnl_path, cfg.master_addr);
        let pnl_anchor_1e6 = anchor.map_or(0, |a| a.usdc_1e6);
        let anchor_set_s = anchor.map_or(0, |a| a.set_unix_s);
        // S7-L1 (gap E): the top-up's day ceiling and the session's
        // top-up spend survive a restart, beside the anchor.
        let topup_path = pnl_path.with_file_name(budget::TOPUP_STATE_PATH);
        let (topup_day, topup_used, topup_cost_1e6) = budget::restore_topup(
            &topup_path,
            cfg.master_addr,
            crate::nonce::now_ms() / crate::dayspend::DAY_MS,
            anchor_set_s,
        );
        Ok(Self {
            http,
            ws,
            sk,
            network: cfg.network,
            nonce: Nonce::new(),
            budget,
            budget_source,
            seen: TidRing::new(),
            assets: AssetTable::default(),
            fills,
            counters: HlExecCounters::default(),
            budget_path,
            last_persist: Instant::now(),
            pnl_anchor_1e6,
            usdc_1e6: 0,
            held_cost_1e6: 0,
            legs_held: 0,
            _pad_legs: 0,
            pnl_path,
            ws_fail_streak: 0,
            last_ws_ok: None,
            reject_streak: 0,
            asset_refusal_streak: 0,
            reconciled: false,
            ws_retry_at: Instant::now(),
            scratch: vec![UserFill::default(); SNAPSHOT_RING].into_boxed_slice(),
            last_recon: Instant::now(),
            recon_prev_worst: 0,
            master_addr: cfg.master_addr,
            sweeps: [PendingSweep::EMPTY; MAX_PENDING_SWEEPS],
            sweeps_n: 0,
            cancel_all_mark: 0,
            cancel_all_unqueued_mark: 0,
            last_recon_ok: None,
            open: vec![crate::recon::OpenOrder::default(); crate::recon::MAX_OPEN_ORDERS]
                .into_boxed_slice(),
            oids: vec![0u64; crate::recon::MAX_OPEN_ORDERS].into_boxed_slice(),
            bal: vec![crate::recon::SpotBalance::default(); crate::recon::MAX_SPOT_BALANCES]
                .into_boxed_slice(),
            mp: vec![0u8; MAX_ACTION].into_boxed_slice(),
            req: vec![0u8; MAX_REQ_BODY].into_boxed_slice(),
            wires: vec![
                crate::action::CancelWire { asset: 0, oid: 0 };
                crate::recon::MAX_OPEN_ORDERS
            ]
            .into_boxed_slice(),
            restart_safety: false,
            orphans_clear: false,
            day_bought_1e6: [0; core_config::exec::EXEC_SLOTS],
            day_epoch: 0,
            day_retry_at: Instant::now(),
            day_backoff: DAY_RETRY_MIN,
            topup_weight: 0,
            topup_day_max: 0,
            topup_day,
            topup_used,
            topup_cost_1e6,
            anchor_set_s,
            topup_next_at: Instant::now(),
            topup_path,
        })
    }

    /// The asset table. **Bound by a roll event, never derived**
    /// (LAW E-4) — the boot and the roll are what call `bind`.
    #[inline]
    pub fn assets_mut(&mut self) -> &mut AssetTable {
        &mut self.assets
    }

    /// The asset table, read-only — what the fill path resolves coin
    /// names against.
    #[inline]
    #[must_use]
    pub const fn assets(&self) -> &AssetTable {
        &self.assets
    }

    /// Operator counters. By reference: the struct is five cache lines
    /// and the `/state` mirror reads a handful of fields.
    #[inline]
    #[must_use]
    pub fn counters(&self) -> &HlExecCounters {
        &self.counters
    }

    /// Requests believed to remain before the venue's cliff.
    #[inline]
    #[must_use]
    pub fn budget_remaining(&self) -> i64 {
        self.budget.remaining()
    }

    /// **E7 session bound** — the anchor the bound is judged from,
    /// USD ×1e6; `0` until the first reconciliation sets it (or the
    /// file restored it). For the boot tell and `/state`.
    #[inline]
    #[must_use]
    pub const fn pnl_anchor_usd_1e6(&self) -> i64 {
        self.pnl_anchor_1e6
    }

    /// **E7 session bound** — the equity at cost (spot USDC plus the
    /// held legs' `entryNtl`) minus the anchor, minus what this
    /// session's request-weight top-ups cost (S7-L1: paid from the
    /// perps balance, which that equity does not read), as of the last
    /// reconciliation, USD ×1e6; `0` while not anchored or while
    /// nothing has been reconciled since boot. A LEVEL, signed: what
    /// `/state` shows and what the router halts on.
    #[inline]
    #[must_use]
    pub const fn session_pnl_usd_1e6(&self) -> i64 {
        if self.pnl_anchor_1e6 == 0 || self.usdc_1e6 == 0 {
            0
        } else {
            self.usdc_1e6
                .saturating_add(self.held_cost_1e6)
                .saturating_sub(self.pnl_anchor_1e6)
                .saturating_sub(self.topup_cost_1e6)
        }
    }

    /// Where the session anchor is persisted — for the boot tell.
    #[inline]
    #[must_use]
    pub fn pnl_state_path(&self) -> &std::path::Path {
        &self.pnl_path
    }

    /// **E7 session bound** — record one reconciliation's account
    /// reading and, on the first one, set the session anchor to the
    /// equity at cost. Split from [`Self::reconcile`] so it can be
    /// tested without a venue.
    ///
    /// Returns `true` when this call SET the anchor — the caller
    /// persists it. The anchor is never moved afterwards, and never
    /// set at zero USDC (nothing to bound). A held leg no longer
    /// defers it: at cost, the sheet is the account's worth whatever
    /// it holds (see [`crate::recon::account_view`]).
    fn note_account(&mut self, v: crate::recon::AccountView) -> bool {
        self.usdc_1e6 = v.usdc_1e6;
        self.held_cost_1e6 = v.held_cost_1e6;
        self.legs_held = v.legs;
        if self.pnl_anchor_1e6 != 0 || v.usdc_1e6 <= 0 {
            return false;
        }
        self.pnl_anchor_1e6 = v.equity_at_cost_1e6();
        true
    }

    /// **E7-F3** — admit one reconciliation's worst disagreement to
    /// the high-water mark the halt reads, but only the part of it
    /// the PREVIOUS cycle also saw. A disagreement that survives a
    /// reconciliation interval is drift (a lost or double-counted
    /// fill stays wrong); one that the next cycle no longer sees was
    /// the venue's sheet running ahead of its `userFills` push — see
    /// [`Self::recon_prev_worst`]. `recon_drift_legs` stays the
    /// instantaneous level, so an operator still sees the race.
    ///
    /// The minimum of two consecutive worsts, not a per-leg memory:
    /// two different legs racing 60 s apart would confirm as the
    /// smaller of the two, which errs toward halting, and is two
    /// races in a row against a window of milliseconds.
    fn note_drift(&mut self, worst: i64) {
        let confirmed = worst.min(self.recon_prev_worst);
        self.recon_prev_worst = worst;
        if confirmed > self.counters.recon_drift_max_qty_1e6 {
            self.counters.recon_drift_max_qty_1e6 = confirmed;
        }
    }

    /// Where the boot budget came from (E7-F1) — for the boot tell.
    #[inline]
    #[must_use]
    pub const fn budget_source(&self) -> BudgetSource {
        self.budget_source
    }

    /// E7-F1: replace the boot budget with the venue's own figures.
    ///
    /// One blocking `/info userRateLimit` round trip, on the same
    /// connection the first order will use. Called by the BOOT, once,
    /// after `new` and before the arm is announced — never from `new`
    /// itself, so building an arm in a test reaches no socket. When
    /// the venue does not answer, or answers half (`scan_rate_limit`),
    /// the budget `new` loaded stands — the file, else cold — and the
    /// returned source says which.
    pub fn seed_budget_from_venue(&mut self) -> BudgetSource {
        if self.read_venue_budget() {
            self.budget_source = BudgetSource::Venue;
        }
        self.budget_source
    }

    /// One `/info userRateLimit` read into `self.budget`; `false` — the
    /// budget untouched — when the venue did not answer or answered
    /// half. The boot seed and the top-up's re-read share it.
    fn read_venue_budget(&mut self) -> bool {
        let mut req = [0u8; budget::MAX_RATE_REQ];
        let Ok(n) = budget::rate_limit_request(&mut req, &self.master_addr) else {
            return false;
        };
        let Ok((_status, range)) = self.http.post_to(crate::http::INFO_PATH, &req[..n]) else {
            return false;
        };
        let Some((used, vlm_1e6, surplus)) = budget::scan_rate_limit(&self.http.resp()[range])
        else {
            return false;
        };
        self.budget =
            AddressBudget::from_venue(self.master_addr, self.budget.floor(), used, vlm_1e6, surplus);
        true
    }

    /// **S7-L1** — turn on the operator arm's restart safety: the
    /// seeding verdict waits for the day-spend read (gap A) and for a
    /// read of the whole account that finds no order of ours resting
    /// (gap C). The boot calls it for the arm that trades the operator's
    /// slots, before [`Self::cancel_ours_everywhere`]; an arm that never
    /// calls it keeps exactly its old behaviour.
    pub fn enable_restart_safety(&mut self) {
        self.restart_safety = true;
    }

    /// **S7-L1 (gap E)** — arm the request-weight top-up: when the
    /// headroom comes within `weight` of the floor, buy `weight`
    /// requests, at most `day_max` per UTC day. `weight == 0` leaves it
    /// off. Boot only, from `exec.toml` (`request_topup_weight`,
    /// `request_topup_day_max`); the day's usage and the session's
    /// spend were restored by [`Self::new`].
    pub fn set_topup(&mut self, weight: u64, day_max: u64) {
        self.topup_weight = weight;
        self.topup_day_max = day_max;
    }

    /// **S7-L1 (gap C) — take every resting order of OURS off the
    /// venue, on every leg, now.** Blocking and bounded. The boot calls
    /// it before the arm is announced, and `on_shutdown` as the engine
    /// stops; with restart safety on, each reconciliation retries one
    /// round until a read finds nothing of ours.
    ///
    /// Why both ends: a restart boots every member FLAT — the paper
    /// arm's resting orders die with its process, and the venue's must
    /// too, or a quote the old process left fills into a position no
    /// member of the new one knows it has. The shutdown sweep is the
    /// normal path; the boot sweep is the one that still holds after a
    /// crash, a `kill -9` or a drain that ran out of time.
    ///
    /// Unlike the LAW E-8 sweep ([`Self::cancel_all`]) it asks about
    /// the WHOLE account, not per bound leg — the orders it exists for
    /// sit on instances this process never bound — and it queues no
    /// work for idle moments that, at shutdown, will not come. At most
    /// [`SWEEP_ALL_ROUNDS`] passes of ask → cancel each (single-item
    /// actions, the sweep's own shape), stopping at the first read that
    /// finds nothing of ours, and no request starts after
    /// [`SWEEP_ALL_DEADLINE`].
    ///
    /// Returns `(cancelled, left)`. `left == u32::MAX` means the
    /// venue's open orders could not be read: NOT confirmed clear.
    pub fn cancel_ours_everywhere(&mut self) -> (u32, u32) {
        self.sweep_ours_everywhere(SWEEP_ALL_ROUNDS, usize::MAX)
    }

    /// The account-wide sweep: at most `rounds` cancel passes of at
    /// most `per_round` cancels each; a read that finds nothing of ours
    /// sets `orphans_clear`. The boot and shutdown sweeps are unbounded
    /// per round (the deadline bounds them); the reconciliation's retry
    /// takes [`SWEEP_CANCELS_PER_IDLE`], as the LAW E-8 sweep does, and
    /// continues at the next reconciliation.
    ///
    /// **Neutral to the streaks.** A cancel of an order that filled or
    /// expired between the read and the cancel comes back "never
    /// placed, already canceled, or filled", which `judge` counts into
    /// the reject streak — five of them in a boot sweep racing an
    /// instance's expiry would latch a sticky `reject-streak` halt over
    /// orders that were simply gone. The streaks are the venue refusing
    /// to TRADE; the sweep puts them back as it found them.
    fn sweep_ours_everywhere(&mut self, rounds: u32, per_round: usize) -> (u32, u32) {
        let deadline = Instant::now() + SWEEP_ALL_DEADLINE;
        let streaks = (self.reject_streak, self.asset_refusal_streak);
        let mut cancelled = 0u32;
        let mut round = 0u32;
        let mut left_known = u32::MAX;
        let left = loop {
            // No request starts after the deadline — not even the
            // re-read; the last read's count stands, unconfirmed.
            if round > 0 && Instant::now() >= deadline {
                break left_known;
            }
            let Some((k, unmapped)) = self.read_ours_everywhere() else {
                break u32::MAX;
            };
            if k == 0 {
                // Nothing of ours this crate can cancel: clear, but for
                // any order of ours on a coin that is not an outcome
                // leg, which is reported rather than forgotten.
                self.orphans_clear = true;
                break unmapped;
            }
            left_known = u32::try_from(k).unwrap_or(u32::MAX).saturating_add(unmapped);
            if round >= rounds {
                break left_known;
            }
            round += 1;
            let mut j = 0usize;
            while j < k && j < per_round && Instant::now() < deadline {
                let c = self.wires[j];
                j += 1;
                if self.cancel_by_oid(c.asset, c.oid) {
                    cancelled = cancelled.saturating_add(1);
                }
            }
        };
        self.reject_streak = streaks.0;
        self.asset_refusal_streak = streaks.1;
        self.counters.sweep_all_cancelled =
            self.counters.sweep_all_cancelled.wrapping_add(u64::from(cancelled));
        self.counters.sweep_all_left = if left == u32::MAX {
            u64::MAX
        } else {
            u64::from(left)
        };
        (cancelled, left)
    }

    /// One `frontendOpenOrders` read, selected to the orders of ours on
    /// any outcome leg ([`crate::recon::ours_everywhere`]) in
    /// `self.wires`: `(selected, unmapped)`. `None` when the answer
    /// could not be had or read — never "nothing resting".
    fn read_ours_everywhere(&mut self) -> Option<(usize, u32)> {
        let mut req = [0u8; crate::recon::MAX_OPEN_ORDERS_REQ];
        let n = crate::recon::open_orders_request(&mut req, &self.master_addr).ok()?;
        let (_status, range) = self.http.post_to(crate::http::INFO_PATH, &req[..n]).ok()?;
        let body = &self.http.resp()[range];
        let rows = crate::recon::scan_open_orders(body, &mut self.open).ok()?;
        Some(crate::recon::ours_everywhere(
            &self.open[..rows],
            body,
            &mut self.wires,
        ))
    }

    /// One single-item cancel by venue oid — the LAW E-8 sweep's shape,
    /// shared with the account-wide sweep. `true` only when the venue
    /// confirmed it (`judge` under [`Spend::Cancel`]); an encode failure
    /// is counted.
    fn cancel_by_oid(&mut self, asset: u32, oid: u64) -> bool {
        let c = [crate::action::CancelWire { asset, oid }];
        let (Ok(mp_n), Ok(head)) = (
            crate::action::encode_cancel(&mut self.mp, &c),
            envelope_open(&mut self.req),
        ) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            return false;
        };
        let Ok(aj_n) = crate::request::cancel_json(&mut self.req[head..], &c) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            return false;
        };
        self.send_action(mp_n, head + aj_n, 1, Spend::Cancel).is_ok()
    }

    /// **S7-L1 (gap A)** — read what each slot BOUGHT on the venue since
    /// today's 00:00Z ([`crate::dayspend`]), once per UTC day: at the
    /// first reconciliation of a boot and at the first after each
    /// midnight. Restart safety only. A failed read is counted and tried
    /// again after a backoff ([`DAY_RETRY_MIN`] doubling to
    /// [`DAY_RETRY_MAX`]); the figure it would have replaced stands.
    fn sync_day_spend(&mut self) {
        if !self.restart_safety {
            return;
        }
        let today = crate::nonce::now_ms() / crate::dayspend::DAY_MS;
        if today == self.day_epoch {
            return;
        }
        let now = Instant::now();
        if now < self.day_retry_at {
            return;
        }
        let start_ms = today * crate::dayspend::DAY_MS;
        let mut req = [0u8; crate::dayspend::MAX_DAY_REQ];
        let mut bought = [0i64; core_config::exec::EXEC_SLOTS];
        let read = match crate::dayspend::fills_since_request(&mut req, &self.master_addr, start_ms)
        {
            Ok(n) => match self.http.post_to(crate::http::INFO_PATH, &req[..n]) {
                Ok((_status, range)) => {
                    crate::dayspend::scan_day_bought(&self.http.resp()[range], start_ms, &mut bought)
                        .is_ok()
                }
                Err(_) => false,
            },
            Err(_) => false,
        };
        if read {
            // COPY: [i64; EXEC_SLOTS] (64 B) of day spend, stack → arm,
            // once per UTC day — `scan_day_bought` zeroes its target on
            // every failure and a failed read must leave the standing
            // figure — rejected: scanning straight into `day_bought_1e6`.
            self.day_bought_1e6 = bought;
            self.day_epoch = today;
            self.day_backoff = DAY_RETRY_MIN;
            self.counters.day_sync_ok = self.counters.day_sync_ok.wrapping_add(1);
        } else {
            self.counters.day_sync_failed = self.counters.day_sync_failed.wrapping_add(1);
            self.day_retry_at = now + self.day_backoff;
            self.day_backoff = self.day_backoff.saturating_mul(2).min(DAY_RETRY_MAX);
        }
    }

    /// **S7-L1 (gap E) — buy request weight before the floor, not at
    /// it.**
    ///
    /// The address allowance is 10 000 requests plus one per USDC of
    /// lifetime volume, and a member requoting every instance spends it
    /// in about half a day, after which the floor halts the slot. The
    /// venue sells more (`reserveRequestWeight`, paid from the PERPS
    /// balance): when the headroom is within one top-up of the floor,
    /// buy one, at most `topup_day_max` weight per UTC day. Weight `0`
    /// is off, and the floor halts as it always did.
    ///
    /// Guarded six ways, each against a way a spender runs away:
    /// - **the venue's headroom, not the model's**: `userRateLimit` is
    ///   re-read before every purchase, so a top-up that went through
    ///   with an answer this arm could not read shows up as headroom
    ///   and is not bought twice;
    /// - **seen on the venue, or no more**: right after a purchase the
    ///   venue's figures must show it; if they do not, top-ups stop for
    ///   the process — the guard that holds whatever the venue's
    ///   netting turns out to be;
    /// - **one check per [`TOPUP_EVERY`]**, whatever it found or did;
    /// - **charged when bought or possibly bought** (accepted, or sent
    ///   with no readable answer — a clear refusal buys nothing): the
    ///   day's usage and the session's cost are on disk before the
    ///   next step, and a failed write stops top-ups for the process;
    /// - **only on a budget the venue stated** at boot, never the cold
    ///   or file fallback;
    /// - **not at or under the floor**: the router has halted the slot
    ///   there (sticky), and headroom bought would un-halt nothing.
    ///
    /// A refused or stopped top-up is counted (`topup_failed`), never
    /// put in the reject streak — it is not the venue refusing to TRADE.
    /// If top-ups keep failing, the floor halts the slot, which is what
    /// the floor is for.
    fn top_up_budget(&mut self) {
        if self.topup_weight == 0 || self.budget_source != BudgetSource::Venue {
            return;
        }
        let now = Instant::now();
        if now < self.topup_next_at {
            return;
        }
        let floor = i64::try_from(self.budget.floor()).unwrap_or(i64::MAX);
        let band = floor.saturating_add(i64::try_from(self.topup_weight).unwrap_or(i64::MAX));
        if self.budget.remaining() > band {
            return;
        }
        self.topup_next_at = now + TOPUP_EVERY;
        if !self.read_venue_budget() {
            self.counters.topup_failed = self.counters.topup_failed.wrapping_add(1);
            return;
        }
        let today = crate::nonce::now_ms() / crate::dayspend::DAY_MS;
        if today != self.topup_day {
            self.topup_day = today;
            self.topup_used = 0;
        }
        if !topup_fits(
            self.budget.remaining(),
            floor,
            self.topup_weight,
            self.topup_used,
            self.topup_day_max,
        ) {
            return;
        }
        let weight = i64::try_from(self.topup_weight).unwrap_or(i64::MAX);
        let before = self.budget.remaining();
        let outcome = self.reserve_weight(self.topup_weight);
        if !matches!(outcome, Reserve::Accepted | Reserve::Unknown) {
            // Never sent, or the venue said no: nothing was bought.
            self.counters.topup_failed = self.counters.topup_failed.wrapping_add(1);
            return;
        }
        // Bought, or it may have been: the day and the session are
        // charged now, and the charge is on disk before anything else.
        self.topup_used = self.topup_used.saturating_add(self.topup_weight);
        if self.pnl_anchor_1e6 != 0 {
            self.topup_cost_1e6 = self
                .topup_cost_1e6
                .saturating_add(weight.saturating_mul(TOPUP_PRICE_1E6));
        }
        let st = budget::TopupState {
            day: self.topup_day,
            used: self.topup_used,
            anchor_set_s: self.anchor_set_s,
            cost_1e6: self.topup_cost_1e6,
        };
        if budget::store_topup(&self.topup_path, &self.master_addr, st).is_err() {
            // The next boot would restore an older day: no more
            // purchases from this process.
            self.stop_topups();
            return;
        }
        // **Seen on the venue, or no more.** The purchase must show as
        // headroom in the venue's own figures (less the one request it
        // cost). If it does not — the venue did not credit it, or
        // credits it where this arm cannot read it — the next purchase
        // could be a second one for the same need: stop for this process
        // and let the floor decide.
        if !self.read_venue_budget() {
            if outcome == Reserve::Accepted {
                // The venue said yes in so many words; the model carries
                // the credit it cannot re-read.
                self.budget.on_reserved(self.topup_weight);
            }
            self.stop_topups();
            return;
        }
        if self.budget.remaining() >= before.saturating_add(weight / 2) {
            self.counters.topup_ok = self.counters.topup_ok.wrapping_add(1);
        } else {
            self.stop_topups();
        }
    }

    /// No more request-weight top-ups from this process — nor, for the
    /// rest of the UTC day, from the next: the day is written as spent,
    /// so a restart does not re-arm what this one stopped. Counted as a
    /// failure; the floor decides from here.
    fn stop_topups(&mut self) {
        self.topup_weight = 0;
        self.topup_used = self.topup_used.max(self.topup_day_max);
        let st = budget::TopupState {
            day: self.topup_day,
            used: self.topup_used,
            anchor_set_s: self.anchor_set_s,
            cost_1e6: self.topup_cost_1e6,
        };
        let _ = budget::store_topup(&self.topup_path, &self.master_addr, st);
        self.counters.topup_failed = self.counters.topup_failed.wrapping_add(1);
    }

    /// One signed `reserveRequestWeight`, and what it came to.
    fn reserve_weight(&mut self, weight: u64) -> Reserve {
        let Ok(mp_n) = crate::action::encode_reserve_weight(&mut self.mp, weight) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            return Reserve::NotSent;
        };
        let Ok(head) = self.open_action() else {
            return Reserve::NotSent;
        };
        let Ok(aj_n) = crate::request::reserve_weight_json(&mut self.req[head..], weight) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            return Reserve::NotSent;
        };
        let Ok(n) = self.seal(mp_n, head + aj_n) else {
            return Reserve::NotSent;
        };
        let posted = self.post_counted(n, 1);
        let left_host = Self::counts_against_address(&posted);
        let Ok((_status, range)) = posted else {
            return if left_host {
                Reserve::Unknown
            } else {
                Reserve::NotSent
            };
        };
        match scan(&self.http.resp()[range]) {
            Ok(HlResponse::Ok(ok))
                if ok.accepted() && !ok.any_resting && !ok.any_filled && !ok.any_success =>
            {
                Reserve::Accepted
            }
            // Only an explicit `status: err` is a refusal. An ok envelope
            // of another shape may be this action's success in a form not
            // yet measured: charged, and checked on the venue like one.
            Ok(HlResponse::Err { .. }) => Reserve::Refused,
            Ok(HlResponse::Ok(_)) | Err(_) => Reserve::Unknown,
        }
    }

    /// Map the engine's order kind onto the venue's TIF.
    ///
    /// `ORDER_KIND_MAKER` is POST-ONLY, and `Alo` is the only tif that
    /// guarantees it never takes — a `Gtc` that crossed would pay the
    /// spread the member's whole edge is made of.
    ///
    /// An unknown kind is **refused**, not mapped. The paper arm
    /// refuses it (`PaperDispatcher::submit` counts it `unroutable`),
    /// and the two arms disagreeing about the same order is the shape
    /// LAW E-1 exists to forbid: the order would be dropped in the
    /// model and RESTING on the venue, so the paper P&L and the real
    /// book would describe different worlds.
    #[inline]
    fn tif_of(kind: u8) -> Option<Tif> {
        match kind {
            ORDER_KIND_IOC => Some(Tif::Ioc),
            ORDER_KIND_MAKER => Some(Tif::Alo),
            _ => None,
        }
    }

    /// Test seam over [`Self::route_frame`]: one frame, one `&mut
    /// self` call. The production path routes inside the pump's
    /// closure and never goes through here, which is why this is
    /// `cfg(test)` rather than dead code left lying around.
    #[cfg(test)]
    fn route_fills(&mut self, payload: &[u8]) -> usize {
        let recv_ns = now_ns();
        Self::route_frame(
            payload,
            recv_ns,
            &mut self.assets,
            &mut self.scratch,
            &mut self.seen,
            &mut self.budget,
            &mut self.fills,
            &mut self.counters,
        )
    }

    /// Route one `userFills` frame into fill lane 3.
    ///
    /// **`is_snapshot` matters and is not decoration: a snapshot is
    /// HISTORY.** Its notional must not be re-added to the budget's
    /// `traded` on every boot, or five restarts a day would inflate
    /// the allowance in exactly the permissive direction the budget
    /// exists to prevent. It is derived INSIDE `scan_user_fills` from
    /// the payload and is deliberately not a parameter here, so no
    /// caller can present a snapshot as live trading.
    ///
    /// Takes DISJOINT borrows rather than `&mut self`, which is not a
    /// style choice. The socket's payload borrows the socket, so a
    /// `&mut self` router cannot be called from inside
    /// [`UserWs::pump`]'s closure — and the copy that used to buy its
    /// way around that was a heap `Vec` grown per frame, on the one
    /// thread that also signs and submits. Taking the six fields this
    /// needs (none of which is `ws`) lets the scan read the socket's
    /// own receive buffer in place: zero copy, zero allocation.
    ///
    /// `assets` is `&mut` because a BOOKED fill feeds the reconciler's
    /// ledger — the table is both what resolves a coin and what
    /// records what we hold in it.
    ///
    /// Public so the allocation gate can measure THIS function rather
    /// than a lookalike. The audit that produced this shape also found
    /// that no gate touched the fill path at all, which made "0 B/op"
    /// a statement about other code. Publishing it adds no capability:
    /// every ingredient (`scan_user_fills`, `to_fill`, `TidRing`,
    /// `AddressBudget::on_venue_fill`, `Producer::try_push`) was
    /// already public, and every `HlExchange` field is private, so
    /// this is a NARROWING wrapper over `try_push` — it adds the
    /// channel test, the strict parse, the tid dedupe, the cloid
    /// containment and the zero-quantity refusal.
    #[allow(clippy::too_many_arguments)]
    pub fn route_frame(
        payload: &[u8],
        recv_ns: NsTs,
        assets: &mut AssetTable,
        scratch: &mut [UserFill],
        seen: &mut TidRing<SNAPSHOT_RING>,
        budget: &mut AddressBudget,
        fills: &mut Producer<Fill, FILL_N>,
        counters: &mut HlExecCounters,
    ) -> usize {
        // `seen` is pinned to SNAPSHOT_RING by its type; `scratch` is
        // an unsized slice, so the same bound is asserted rather than
        // constructed. A short scratch still FAILS CLOSED —
        // `scan_user_fills` refuses the frame at its own bound check
        // and it is counted `fills_scan_failed` — but that drops a
        // whole snapshot, which is the failure this file exists to
        // make loud, so it is worth catching in debug.
        debug_assert!(
            scratch.len() >= SNAPSHOT_RING,
            "scratch must hold a venue SNAPSHOT, not a steady-state frame"
        );
        if !crate::userws::is_user_fills(payload) {
            // An ack, an orderUpdates, a pong. Books nothing, and is
            // NOT a failure.
            return 0;
        }
        let (n, is_snapshot) = match scan_user_fills(payload, scratch) {
            Ok(v) => v,
            Err(_) => {
                // It WAS a userFills frame and it did not scan. Loud,
                // and never confused with a frame from another
                // channel — silently discarding a reconnect snapshot
                // is the failure this counter exists for.
                counters.fills_scan_failed =
                    counters.fills_scan_failed.wrapping_add(1);
                return 0;
            }
        };
        let mut booked = 0usize;
        // An INDEX loop, deliberately. clippy wants
        // `scratch.iter().take(n)`; CLAUDE.md forbids iterator chains
        // in hot loops, and `scratch` is `SNAPSHOT_RING` long while
        // `n` is the row count, so the index is also what keeps the
        // read inside the frame that actually scanned.
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            // A reference, not a copy: `UserFill` is 88 bytes and a
            // reconnect snapshot is ~2,000 rows on the engine thread.
            let f = &scratch[i];
            // LAW E-5: the venue's tid is the dedupe key, and the ring
            // outlives the socket precisely so a reconnect snapshot
            // cannot re-book.
            if !seen.admit(f.tid) {
                continue;
            }
            // The address budget is credited ONLY from a row the arm
            // would book: a row whose sign or scale fails `to_fill`'s
            // refusal below must not manufacture request headroom
            // (the first cut credited every admitted row before
            // validating it — one malformed `px` defeated the floor
            // until the next cold boot; E7 review, 2026-09-19).
            if !is_snapshot && f.px_1e8 >= 0 && f.sz_1e8 > 0 {
                budget.on_venue_fill(f.notional_usdc_1e6());
            }
            if f.is_settlement {
                counters.fills_settlement = counters.fills_settlement.wrapping_add(1);
            }
            // `checked_`, not `saturating_`, for the same reason
            // `submit` uses it on price and qty: a clamp is a wrong
            // answer that looks like an answer. A venue stamp that
            // does not fit is garbage, and the fill is booked with the
            // local receive time rather than dropped — the POSITION is
            // real either way, and a fill the engine never hears about
            // is the worse of the two failures.
            let ts = match f.time_ms.checked_mul(1_000_000) {
                Some(v) => v,
                None => {
                    counters.fills_bad_ts = counters.fills_bad_ts.wrapping_add(1);
                    recv_ns
                }
            };

            // THE SYMBOL. The venue echoes a COIN NAME; the engine
            // routes on its own `SymbolId`. The table answers by
            // COMPARING BYTES against what a roll bound — never by
            // parsing `+<enc>` back into an asset id. A fill booked
            // against the wrong symbol moves a position the member
            // never took, silently, in the tape, forever; a missing
            // fill is caught by reconciliation inside a minute, a
            // misattributed one by nobody.
            // `current` says whether the coin matched the leg live
            // NOW or the one that just rolled off. A late fill from the
            // ended instance still books into the lane and the tape —
            // that is what the one-generation memory is FOR — but it
            // must not credit the successor's ledger, which `bind` just
            // zeroed and whose venue balance will never contain it.
            let Some((sym, current)) = assets.sym_of_coin_gen(f.coin.of(payload)) else {
                counters.fills_unresolved =
                    counters.fills_unresolved.wrapping_add(1);
                continue;
            };
            // A SETTLEMENT has no cloid — the venue placed the order,
            // not us — so `to_fill` routes it to the tape-only arm and
            // it never reaches lane 3 at all. Operator ruling
            // 2026-09-15: book it, the venue is the truth. The slot
            // comes from the BINDING, which learned it from an order
            // the VENUE ACCEPTED — never from the fill, and never from
            // a submit that was merely attempted. A leg nothing has
            // traded still books nothing.
            //
            // COPY (both `try_push` below): one `Fill` POD (≤ 128 B)
            // by value into lane 3's ring slot — the ring publish IS
            // the ownership transfer to the engine thread (§7); the
            // frame's bytes in `rx` are compacted away after the pump.
            if f.is_settlement && f.cloid.is_none() {
                match assets.owner_of_sym(sym) {
                    Some(slot) => match to_fill_as(f, sym, ts, slot) {
                        Ok(fill) => {
                            if fills.try_push(fill).is_err() {
                                counters.fills_dropped = counters.fills_dropped.wrapping_add(1);
                            } else {
                                counters.fills_booked = counters.fills_booked.wrapping_add(1);
                                if current {
                                    assets.book_qty(sym, signed_qty_1e6(f, &fill));
                                }
                                booked += 1;
                            }
                        }
                        Err(_) => {
                            counters.fills_refused = counters.fills_refused.wrapping_add(1);
                        }
                    },
                    // Never traded by any member we know of. Counted,
                    // not guessed — STRATEGY_ID_NONE in the lane would
                    // fan it out to everybody.
                    None => {
                        counters.fills_unowned = counters.fills_unowned.wrapping_add(1);
                    }
                }
                continue;
            }
            match to_fill(f, sym, ts) {
                Ok(Routed::Slot(fill)) => {
                    if fills.try_push(fill).is_err() {
                        // A dropped fill is a position the engine does
                        // not know it has. Reconciliation catches it;
                        // this counter explains it afterwards.
                        counters.fills_dropped =
                            counters.fills_dropped.wrapping_add(1);
                    } else {
                        counters.fills_booked =
                            counters.fills_booked.wrapping_add(1);
                        // The RECONCILER's own side of the comparison,
                        // fed only from fills that actually entered the
                        // lane — a dropped or refused fill is not a
                        // position.
                        if current {
                            assets.book_qty(sym, signed_qty_1e6(f, &fill));
                        }
                        booked += 1;
                    }
                }
                Ok(Routed::TapeOnly(_)) => {
                    // Not ours. Never enters the lane — there,
                    // STRATEGY_ID_NONE would fan it out to EVERY
                    // member. (Plan §6.1 also wants it written to the
                    // tape; that writer does not exist here and is
                    // recorded as open in docs/risk-policy.md.)
                    counters.fills_foreign =
                        counters.fills_foreign.wrapping_add(1);
                }
                Err(_) => {
                    // A REFUSAL (zero or negative quantity), not a
                    // loss. Kept off `fills_dropped` so that alarm
                    // keeps meaning "a position we do not know about".
                    counters.fills_refused =
                        counters.fills_refused.wrapping_add(1);
                }
            }
        }
        booked
    }

    /// Drain the user-event socket into fill lane 3.
    ///
    /// Returns whether anything was read.
    fn pump_user_events(&mut self) -> bool {
        if !self.ws.is_connected() {
            if Instant::now() < self.ws_retry_at {
                return false;
            }
            if self.ws.connect().is_err() {
                self.counters.ws_connect_failures =
                    self.counters.ws_connect_failures.wrapping_add(1);
                let i = (self.ws_fail_streak as usize).min(WS_BACKOFF.len() - 1);
                self.ws_retry_at = Instant::now() + WS_BACKOFF[i];
                self.ws_fail_streak = self.ws_fail_streak.saturating_add(1);
                return false;
            }
            self.ws_fail_streak = 0;
            self.counters.ws_reconnects = self.counters.ws_reconnects.wrapping_add(1);
        }

        // ZERO COPY. Each payload is routed from the socket's own
        // receive buffer, inside the pump's closure. `ws` is borrowed
        // by `pump`; the six fields the router needs are borrowed
        // beside it, which is what the destructuring below is for.
        //
        // An earlier revision staged the frames into a `Vec` first,
        // because `route_fills` took `&mut self`. That allocated on
        // every frame -- pings and `orderUpdates` included, since the
        // channel test happens inside the router -- on the thread that
        // also signs and submits. The borrow, not the copy, was the
        // actual problem.
        let recv_ns = now_ns();
        let Self {
            ws,
            assets,
            scratch,
            seen,
            budget,
            fills,
            counters,
            ..
        } = self;
        let r = ws.pump(PUMP_BUDGET, |payload| {
            Self::route_frame(
                payload, recv_ns, assets, scratch, seen, budget, fills, counters,
            );
        });
        match r {
            Ok(n) => {
                // The socket answered, so the stream is alive — even
                // with nothing on it. A quiet market is not a gap.
                self.last_ws_ok = Some(Instant::now());
                n > 0
            }
            Err(_) => {
                // Any socket failure drops the connection; the next
                // idle redials, after the backoff. The tid ring
                // SURVIVES, which is what makes the snapshot safe.
                self.ws.disconnect();
                let i = (self.ws_fail_streak as usize).min(WS_BACKOFF.len() - 1);
                self.ws_retry_at = Instant::now() + WS_BACKOFF[i];
                self.ws_fail_streak = self.ws_fail_streak.saturating_add(1);
                false
            }
        }
    }

    /// Ask the venue what it actually holds, and compare.
    ///
    /// §6.2, and the plan calls it "the single most valuable safety net
    /// in the plan" for a reason: one comparison catches a lost fill, a
    /// double-counted fill, a wrong asset id and a stale position view,
    /// and it is the only check independent of every belief the engine
    /// holds.
    ///
    /// **Independent means what it says.** The comparison is between
    /// the venue's balance and what THIS ARM BOOKED into the lane —
    /// not the member's position. A reconciler that asked the member
    /// would be agreeing with itself.
    ///
    /// It does NOT halt. `halt_on_recon_drift_usd_1e6` is an
    /// arming-path decision and nothing is armed; a halt inferred here
    /// would be a policy this file invented. Drift is counted and the
    /// worst magnitude kept.
    ///
    /// On the IDLE path, which is where a blocking HTTPS round trip
    /// belongs. `HlHttp`'s response buffer holds only the last answer,
    /// so this must consume it before the next submit — both live on
    /// one thread and this one runs between order batches, so they are
    /// serialised by construction.
    ///
    /// Returns whether it RAN this call (past its cadence gate), for
    /// `on_idle`'s one-blocking-step rule.
    fn reconcile(&mut self) -> bool {
        if self.last_recon.elapsed() < RECON_EVERY {
            return false;
        }
        self.last_recon = Instant::now();

        let mut req = [0u8; crate::recon::MAX_STATE_REQ];
        let Ok(n) = crate::recon::spot_state_request(&mut req, &self.master_addr) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return true;
        };
        let Ok((_status, range)) = self.http.post_to(crate::http::INFO_PATH, &req[..n]) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return true;
        };
        let body = &self.http.resp()[range];
        let Ok(rows) = crate::recon::scan_spot_state(body, &mut self.bal) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return true;
        };
        self.counters.recon_ok = self.counters.recon_ok.wrapping_add(1);
        // E7 session bound: the same sheet, read for the account's
        // worth (recorded below, once `body`'s borrow of the response
        // buffer has ended).
        let view = crate::recon::account_view(&self.bal[..rows], body);
        let (legs, worst) = Self::compare(&self.assets, &self.bal[..rows], body);
        let unseen = crate::recon::unreconciled_venue_legs(&self.assets, &self.bal[..rows], body);
        self.counters.recon_drift_legs = legs;
        self.counters.recon_unseen_legs = unseen;
        self.note_drift(worst);
        // E7 session bound: the anchor is written ONCE per session,
        // from here — the idle path, on the 60 s cadence — never from
        // a tick.
        if self.note_account(view) {
            let a = crate::anchor::PnlAnchor {
                address: self.master_addr,
                usdc_1e6: self.pnl_anchor_1e6,
                set_unix_s: crate::nonce::now_ms() / 1000,
            };
            // S7-L1: a new session's top-up spend starts at zero, tied
            // to THIS anchor in `exec-topup.state`.
            self.anchor_set_s = a.set_unix_s;
            self.topup_cost_1e6 = 0;
            if crate::anchor::store(&self.pnl_path, a).is_err() {
                // The in-memory anchor stands for this process; the
                // next boot re-anchors. Counted so it is not silent.
                self.counters.anchor_persist_failed =
                    self.counters.anchor_persist_failed.wrapping_add(1);
            }
        }
        // E6 commit 3: the arm has compared itself against the venue.
        // The router reads this to stop refusing every live place —
        // see the seeding interlock, which exists because a ledger
        // that has never been reconciled reads zero exposure after a
        // restart and would fail every clamp OPEN.
        //
        // **Only when the comparison AGREED and covered everything the
        // venue holds** — the same three-clause verdict the phase E
        // probe's `agreed()` requires. The first cut set this on the
        // parse succeeding: after a restart the asset table is empty
        // until the next roll, `compare` walks zero legs, and "zero
        // legs disagreed" unlocked the interlock over a venue holding
        // the previous boot's real position — the exact hazard the
        // interlock exists for, reached through it. `unseen` is the
        // venue-side count of outcome legs with a non-zero holding
        // that no bound leg matched; a flat account seeds at once, an
        // account still holding a retired instance waits for its
        // settlement. (E7 review, 2026-09-19.)
        //
        // **S7-L1, with restart safety on: and only once the day's
        // spend has been read from the venue (gap A) and a read of the
        // whole account has found no order of ours resting (gap C).**
        // The router adopts the day's spend into its day cap on the
        // same poll that seeds it, so a restart never starts that cap
        // at zero under a day the venue says was already spent; and an
        // orphan a previous process left — the boot sweep could not
        // confirm it gone — is retried here, one round a
        // reconciliation, before anything trades beside it.
        self.sync_day_spend();
        if self.restart_safety && !self.orphans_clear {
            let _ = self.sweep_ours_everywhere(1, SWEEP_CANCELS_PER_IDLE);
        }
        let safe = !self.restart_safety || (self.day_epoch != 0 && self.orphans_clear);
        if legs == 0 && unseen == 0 && safe {
            self.reconciled = true;
            self.last_recon_ok = Some(Instant::now());
        }
        true
    }

    /// The comparison itself, split out of the I/O.
    ///
    /// Public so it can be TESTED and MEASURED without a venue —
    /// `reconcile` needs a socket, and a comparison that has only ever
    /// run behind one is a claim about source code. Returns
    /// `(legs that disagreed, worst magnitude 1e6)`.
    ///
    /// A leg the venue does not mention reads as ZERO, which is the
    /// right reading and is itself a drift if we booked something.
    #[must_use]
    pub fn compare(
        assets: &AssetTable,
        bal: &[crate::recon::SpotBalance],
        body: &[u8],
    ) -> (u64, i64) {
        crate::recon::compare_booked(assets, bal, body)
    }

    /// Sign, send and read ONE action.
    ///
    /// The shared tail of every verb this arm has. It was inline in
    /// `submit` while `submit` was the only one; E5 adds a cancel and a
    /// modify, and three copies of the signer-envelope-post-scan
    /// sequence is three places for a signing detail to drift.
    ///
    /// **The budget is checked here, before the signature** — and it is
    /// the ONLY place that checks it, so the policy lives in one spot.
    /// A signed action that is then discarded has still burned a nonce.
    /// Everything a caller can refuse locally — an unbound symbol, a
    /// price that will not scale — belongs above this call, so a local
    /// refusal never reaches the budget at all.
    ///
    /// Everything [`Self::send_action`] does BEFORE the socket:
    /// nonce, EIP-712 signature, HTTP envelope.
    ///
    /// Split out so it can be MEASURED. Gate 59 split `compare` for
    /// the same reason and stated it plainly: a path that has only
    /// ever run behind an HTTPS round trip is a claim about source
    /// code, and this lane has already made that claim wrongly three
    /// times. This is the half of every submit, cancel and requote
    /// that runs on the engine thread and must not allocate.
    ///
    /// The msgpack is `self.mp[..mp_n]`; the action JSON already sits
    /// in `self.req[..action_end]` behind the envelope head
    /// ([`Self::open_action`]). Returns the finished body length in
    /// `self.req`.
    ///
    /// # Errors
    /// `SignerRejected` if the key refuses — or if the wall clock reads
    /// zero, because a nonce of 1 is a signed order stamped 1970 and a
    /// clock that far wrong is not one to sign with; `EncodeOverflow`
    /// if the envelope does not fit.
    pub fn seal(&mut self, mp_n: usize, action_end: usize) -> Result<usize, DispatchError> {
        let ms = crate::nonce::now_ms();
        if ms == 0 {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            return Err(DispatchError::SignerRejected);
        }
        let nonce = self.nonce.next(ms);
        let sig = sign_action(&self.sk, &self.mp[..mp_n], nonce, Vault::None, None, self.network)
            .map_err(|_| {
                self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
                DispatchError::SignerRejected
            })?;
        envelope_close(&mut self.req, action_end, nonce, &sig, None, None).map_err(|_| {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            DispatchError::EncodeOverflow
        })
    }

    /// Write the envelope head into `self.req` and return the offset
    /// the caller renders the action JSON at. See
    /// [`crate::request::envelope_open`].
    #[inline(always)]
    fn open_action(&mut self) -> Result<usize, DispatchError> {
        envelope_open(&mut self.req).map_err(|_| {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            DispatchError::EncodeOverflow
        })
    }

    /// POST one signed action **and count it against the address.**
    ///
    /// The counting lives here, on the only path that reaches the
    /// socket, because it used to live in `send_action` after the
    /// `?` — so a request the venue received and answered unreadably
    /// was never counted at all, and the governor drifted OPTIMISTIC.
    /// `AddressBudget::on_action_sent` names that the wrong
    /// direction: under-counting means exceeding the venue's real
    /// limit and then reading the rate-limit answer as a transport
    /// problem.
    ///
    /// [`crate::http::PostErr::left_host`] is the fact, recorded
    /// inside the HTTP cycle where it is known. It cannot be derived
    /// from the error variant — `Disconnected` is both "the connect
    /// failed" and "the peer went away mid-response" — and it is
    /// deliberately conservative about a torn write.
    fn post_counted(
        &mut self,
        n: usize,
        items: u32,
    ) -> Result<(u16, core::ops::Range<usize>), crate::http::PostErr> {
        let posted = self.http.post(&self.req[..n]);
        self.count_post(&posted, items);
        posted
    }

    /// Charge `posted` to the address if it may have left the host —
    /// `items` address requests for a batch of `items` (§2.2).
    ///
    /// Separated from the post itself so the wiring — predicate to
    /// governor — can be asserted without a socket. The only link
    /// this leaves untested is the call one line above, which is why
    /// it is one line above.
    #[inline]
    fn count_post(
        &mut self,
        posted: &Result<(u16, core::ops::Range<usize>), crate::http::PostErr>,
        items: u32,
    ) {
        if Self::counts_against_address(posted) {
            self.budget.on_action_sent(items);
        }
    }

    /// Does this outcome count against the address-rate governor?
    ///
    /// A success obviously did leave. A failure did iff any byte
    /// reached the socket. Named and separated so the rule can be
    /// asserted without a network.
    #[inline]
    fn counts_against_address(
        posted: &Result<(u16, core::ops::Range<usize>), crate::http::PostErr>,
    ) -> bool {
        match posted {
            Ok(_) => true,
            Err(e) => e.left_host,
        }
    }

    /// Returns the venue's ACK (LAW E-5: an ACK, never a fill).
    ///
    /// `mp_n` / `action_end` locate the msgpack and the in-place
    /// action JSON (see [`Self::seal`]); `items` is the batch size the
    /// address is charged for.
    ///
    /// **The acceptance predicate is `Spend`-aware.** `HlOk::accepted`
    /// only says "no item errored"; a place or a requote is accepted
    /// when the venue reports it RESTING or FILLED, a cancel when it
    /// reports `success`. The first cut accepted on `accepted()`
    /// alone, so an ok envelope with no outcome at all — the shape a
    /// truncated answer takes — counted as a placed order, bound the
    /// slot to the leg, and left an `oid == 0` order the sweep could
    /// not see (E7 review, 2026-09-19). The stricter check existed in
    /// the operator probe and not on the path that trades.
    fn send_action(
        &mut self,
        mp_n: usize,
        action_end: usize,
        items: u32,
        spend: Spend,
    ) -> Result<HlOk, DispatchError> {
        // `Spend`, not the verb's name: see its docs. A cancel is an
        // EXIT and a cap that can stop a position being closed is not a
        // risk control.
        let barred = match spend {
            Spend::Submit => self.budget.may_submit().is_err(),
            Spend::Cancel => !self.budget.may_cancel(),
        };
        if barred {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            return Err(DispatchError::SlotDisabled);
        }
        let n = self.seal(mp_n, action_end)?;
        let posted = self.post_counted(n, items);
        let (_status, range) = posted.map_err(|e| {
            self.counters.rejected = self.counters.rejected.wrapping_add(1);
            if e.left_host {
                // The venue has it and we do not know what it did.
                // Distinct from a request that never left: this one
                // may have placed an order nothing in this process
                // knows the id of, which is what E6's reconciliation
                // is for.
                self.counters.sent_unanswered =
                    self.counters.sent_unanswered.wrapping_add(1);
            }
            DispatchError::Disconnected
        })?;

        let scanned = scan(&self.http.resp()[range]);
        self.judge(spend, scanned)
    }

    /// The verdict on a venue answer, and what it does to the streaks.
    /// Split from `send_action` so the four outcomes can be pinned
    /// without a socket (E7-F2).
    fn judge(
        &mut self,
        spend: Spend,
        scanned: Result<HlResponse, crate::response::ScanErr>,
    ) -> Result<HlOk, DispatchError> {
        let outcome_seen = |ok: &HlOk| match spend {
            Spend::Submit => ok.any_resting || ok.any_filled,
            Spend::Cancel => ok.any_success,
        };
        match scanned {
            Ok(HlResponse::Ok(ok)) if ok.accepted() && outcome_seen(&ok) => {
                // An acceptance ends both streaks. They are
                // CONSECUTIVE counts: a venue refusing every order is
                // a different fact from one that has refused a few
                // over a long boot, and only the first is a halt.
                self.reject_streak = 0;
                self.asset_refusal_streak = 0;
                Ok(ok)
            }
            // E7-F2: an IoC that found nothing to match. The venue
            // understood the order and it did not trade — the caller
            // still gets an error (nothing rests, nothing filled), but
            // NEITHER streak moves and `rejected` does not count it.
            // The first mainnet hour (2026-09-19) counted two of these
            // as venue rejections; five in a row — routine for a 1 s
            // IoC against a 6 s book — would have halted the arm.
            Ok(HlResponse::Ok(ok)) if spend == Spend::Submit && ok.missed() => {
                self.counters.ioc_missed = self.counters.ioc_missed.wrapping_add(1);
                Err(DispatchError::Http(200))
            }
            // The venue understood us and said NO. Distinct from an
            // answer we could not read: E6's `halt_on_reject_streak`
            // counts this one, and conflating the two would have it
            // halt on a parser bug or miss a venue refusing every
            // order.
            Ok(_) => {
                self.counters.rejected = self.counters.rejected.wrapping_add(1);
                self.reject_streak = self.reject_streak.saturating_add(1);
                Err(DispatchError::Http(200))
            }
            Err(_) => {
                self.counters.rejected = self.counters.rejected.wrapping_add(1);
                self.reject_streak = self.reject_streak.saturating_add(1);
                Err(DispatchError::JsonMalformed)
            }
        }
    }

    /// Queue a leg whose instance has ended (LAW E-8). Idempotent per
    /// asset: a second roll before the first was swept must not add a
    /// second entry racing the first.
    fn queue_sweep(&mut self, asset: u32, coin: [u8; crate::asset::COIN_MAX], coin_len: u8) {
        let mut i = 0usize;
        while i < self.sweeps_n {
            if self.sweeps[i].asset == asset {
                return;
            }
            i += 1;
        }
        if self.sweeps_n >= MAX_PENDING_SWEEPS {
            // COUNTED, never silently dropped. A leg nobody swept and
            // nobody counted is the stranded quote this law exists to
            // prevent.
            self.counters.sweep_left = self.counters.sweep_left.wrapping_add(1);
            return;
        }
        self.sweeps[self.sweeps_n] = PendingSweep {
            asset,
            tries: SWEEP_TRIES,
            coin_len,
            defers: 0,
            coin,
            _pad2: [0; 8],
        };
        self.sweeps_n += 1;
    }

    /// Drop entry `i`, keeping the queue contiguous.
    fn drop_sweep(&mut self, i: usize) {
        debug_assert!(i < self.sweeps_n);
        self.sweeps_n -= 1;
        self.sweeps[i] = self.sweeps[self.sweeps_n];
        self.sweeps[self.sweeps_n] = PendingSweep::EMPTY;
    }

    /// **LAW E-8 — take back every order THIS ENGINE placed on a leg
    /// the venue has retired.** One entry per idle call.
    ///
    /// The list of what to cancel comes from the VENUE, not from a
    /// table this arm keeps. A local record of open orders can
    /// disagree with the venue — and it disagrees invisibly, which is
    /// exactly when a restart or a missed ACK makes it matter. Same
    /// principle as reconciliation: believe the venue.
    ///
    /// **Only orders whose cloid decodes as OURS are cancelled.** The
    /// answer covers the whole account, and cancelling a stranger's
    /// order would be the mirror image of booking a stranger's fill.
    ///
    /// It does NOT halt. `sweep_left` counts what could not be taken
    /// off the book after the retries are spent, and E6 decides what
    /// that is worth — a halt inferred here would be a policy this
    /// file invented, the same reasoning `reconcile` carries.
    fn sweep_one_pending(&mut self) {
        if self.sweeps_n == 0 {
            return;
        }
        let e = self.sweeps[0];
        self.counters.sweeps_run = self.counters.sweeps_run.wrapping_add(1);

        // ---- ask the venue what is resting --------------------------
        let mut req = [0u8; crate::recon::MAX_OPEN_ORDERS_REQ];
        let Ok(n) = crate::recon::open_orders_request(&mut req, &self.master_addr) else {
            self.counters.sweep_failed = self.counters.sweep_failed.wrapping_add(1);
            self.spend_try(0);
            return;
        };
        let Ok((_status, range)) = self.http.post_to(crate::http::INFO_PATH, &req[..n]) else {
            self.counters.sweep_failed = self.counters.sweep_failed.wrapping_add(1);
            self.spend_try(0);
            return;
        };
        let body = &self.http.resp()[range];
        let Ok(rows) = crate::recon::scan_open_orders(body, &mut self.open) else {
            // Fail-closed: an unreadable answer is NOT "nothing is
            // resting". That reading is what would let a sweep report
            // success over orders it never saw.
            self.counters.sweep_failed = self.counters.sweep_failed.wrapping_add(1);
            self.spend_try(0);
            return;
        };

        // ---- which of them are ours, on THIS leg --------------------
        // Collected first: the cancel below borrows `self` mutably,
        // and the rows borrow the response buffer. `oids` is a boot
        // buffer, not 2 KiB of stack zeroed every 2 ms while a sweep
        // is pending.
        let k = crate::recon::ours_on_leg(
            &self.open[..rows],
            body,
            &e.coin[..e.coin_len as usize],
            &mut self.oids,
        );
        if k == 0 {
            // Nothing of ours resting on a retired leg is the normal
            // answer and the one this law wants.
            self.drop_sweep(0);
            return;
        }
        // A FULL selection buffer means there may be more than we were
        // told about. Unreachable while `oids` is sized from
        // MAX_OPEN_ORDERS like the row buffer — and the entry is kept
        // pending anyway, because the alternative is cancelling a
        // prefix and reporting the leg clean.
        let truncated = k == self.oids.len();

        // ---- cancel them, by oid ------------------------------------
        //
        // **BUDGETED PER CALL** — see [`SWEEP_CANCELS_PER_IDLE`]. The
        // leftovers are counted into `left`, which keeps the entry
        // pending, so the next idle moment continues the sweep
        // instead of this one stalling the engine thread through 256
        // round trips.
        // `failed` and `deferred` are counted SEPARATELY, and the
        // difference decides whether a retry is burned. Running out of
        // budget is planned continuation, not a failure — folding it
        // into `left` would spend one of [`SWEEP_TRIES`] (8) per idle
        // moment, so a sweep of 256 orders at 8 a call would be
        // abandoned after 64 with the rest reported as `sweep_left`:
        // quotes left resting on a retired instance, which is the one
        // thing LAW E-8 exists to prevent.
        let mut failed = 0u32;
        let (budget, deferred) = sweep_plan(k);
        let mut j = 0usize;
        while j < budget {
            let oid = self.oids[j];
            j += 1;
            if self.cancel_by_oid(e.asset, oid) {
                self.counters.sweep_cancelled = self.counters.sweep_cancelled.wrapping_add(1);
            } else {
                failed += 1;
            }
        }
        match classify_sweep(failed, deferred, truncated) {
            SweepOutcome::Done => self.drop_sweep(0),
            // The entry stays pending and the NEXT idle moment
            // continues it, with its retries untouched — this call did
            // exactly what it set out to do.
            SweepOutcome::Deferred => {
                self.counters.sweep_deferred = self.counters.sweep_deferred.wrapping_add(1);
                if self.sweeps[0].defers >= SWEEP_MAX_DEFERS {
                    // Far past any leg a valid configuration can
                    // produce, so the selection has stopped
                    // shrinking. Stop deferring and start spending
                    // retries, so the entry terminates whether or not
                    // the venue's view reflects our own cancels.
                    self.counters.sweep_stalled =
                        self.counters.sweep_stalled.wrapping_add(1);
                    self.sweeps[0].defers = 0;
                    self.spend_try(0);
                } else {
                    self.sweeps[0].defers = self.sweeps[0].defers.saturating_add(1);
                }
            }
            // Retried on the next idle, bounded. `spend_try` counts
            // them as `sweep_left` when the retries run out.
            SweepOutcome::Retry => self.spend_try(0),
        }
    }

    /// Is a sweep already pending for this asset? `queue_sweep` is
    /// idempotent per asset, so a leg already queued is already
    /// covered and must not read as a failure to queue.
    fn sweep_pending_for(&self, asset: u32) -> bool {
        let mut i = 0usize;
        while i < self.sweeps_n {
            if self.sweeps[i].asset == asset {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Burn one retry on a pending sweep, counting it as `sweep_left`
    /// and dropping it when they are spent.
    fn spend_try(&mut self, i: usize) {
        if self.sweeps[i].tries > 1 {
            self.sweeps[i].tries -= 1;
            return;
        }
        self.counters.sweep_left = self.counters.sweep_left.wrapping_add(1);
        self.drop_sweep(i);
    }

    /// **Cancel one order this arm placed, BY CLOID.**
    ///
    /// By cloid rather than by oid because the oid is not durable: a
    /// modify issues a NEW one (measured 2026-09-16, phase F), so a
    /// caller that kept only the oid could not cancel what it had just
    /// requoted. LAW E-9 puts the slot in the cloid precisely so the
    /// client id is the handle that survives.
    ///
    /// # Errors
    /// The symbol is unbound or its instance has rolled (LAW E-4), the
    /// budget is spent, or the venue refused.
    pub fn cancel_by_cloid(
        &mut self,
        sym: u32,
        strategy_id: u8,
        client_oid: u64,
    ) -> Result<(), DispatchError> {
        let asset = self
            .assets
            .lookup(sym, core_types::instance_of(client_oid))
            .map_err(|e| {
                if matches!(e, crate::asset::AssetError::StaleInstance { .. }) {
                    self.counters.refused_stale = self.counters.refused_stale.wrapping_add(1);
                    // E6 commit 3: CONSECUTIVE. One is a race with a
                    // roll; a streak is a member quoting an instance
                    // that no longer exists.
                    self.asset_refusal_streak =
                        self.asset_refusal_streak.saturating_add(1);
                }
                self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
                self.counters.cancels_refused = self.counters.cancels_refused.wrapping_add(1);
                DispatchError::NoLiveRoute
            })?;
        let c = [crate::action::CancelByCloidWire {
            asset,
            cloid: encode_cloid(strategy_id, client_oid),
        }];
        let (Ok(mp_n), Ok(head)) = (
            crate::action::encode_cancel_by_cloid(&mut self.mp, &c),
            envelope_open(&mut self.req),
        ) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.cancels_refused = self.counters.cancels_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        let Ok(aj_n) = crate::request::cancel_by_cloid_json(&mut self.req[head..], &c) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.cancels_refused = self.counters.cancels_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        match self.send_action(mp_n, head + aj_n, 1, Spend::Cancel) {
            Ok(_) => {
                self.counters.cancels_sent = self.counters.cancels_sent.wrapping_add(1);
                Ok(())
            }
            Err(e) => {
                self.counters.cancels_refused = self.counters.cancels_refused.wrapping_add(1);
                Err(e)
            }
        }
    }

    /// The ENCODE half of [`Self::modify_by_cloid`]: asset lookup, the scale
    /// guards, the cloid builder, the wire structs, msgpack into
    /// `self.mp` and the action JSON in place behind the envelope head
    /// in `self.req`. Returns `(mp_n, action_end)` for
    /// [`Self::seal`] / `send_action`. Touches no socket and spends no
    /// budget.
    ///
    /// Public for the same reason `seal` and `compare` are: so bench
    /// gate 60 can drive the exact bytes a requote signs, in the exact
    /// buffers, without a venue — and without a staging copy of them.
    ///
    /// # Errors
    /// As [`Self::modify_by_cloid`]'s local refusals. Every one is counted here
    /// (`refused_local` / `modifies_refused` / `encode_failures`), so
    /// the caller adds nothing on `Err`.
    pub fn stage_modify(
        &mut self,
        prev_client_oid: u64,
        order: &Order,
    ) -> Result<(usize, usize), DispatchError> {
        let asset = self
            .assets
            .lookup(order.sym, core_types::instance_of(order.client_oid))
            .map_err(|e| {
                if matches!(e, crate::asset::AssetError::StaleInstance { .. }) {
                    self.counters.refused_stale = self.counters.refused_stale.wrapping_add(1);
                    // E6 commit 3: CONSECUTIVE. One is a race with a
                    // roll; a streak is a member quoting an instance
                    // that no longer exists.
                    self.asset_refusal_streak =
                        self.asset_refusal_streak.saturating_add(1);
                }
                self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
                self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
                DispatchError::NoLiveRoute
            })?;
        // The same scale guards `submit` has, for the same reason:
        // saturation clamps to a POSITIVE i64::MAX and would sail past
        // the `<= 0` check below.
        let (Some(px), Some(sz)) = (
            order.px.raw().checked_mul(ENGINE_TO_WIRE),
            order.qty.raw().checked_mul(ENGINE_TO_WIRE),
        ) else {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        if px <= 0 || sz <= 0 {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        }
        let Some(tif) = Self::tif_of(order.kind) else {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::NoLiveRoute);
        };
        let wire = OrderWire::new(asset, order.side == Side::Bid, px, sz, tif)
            .with_cloid(encode_cloid(order.strategy_id, order.client_oid));
        let m = [crate::action::ModifyWire {
            order: wire,
            oid: 0,
            oid_cloid: encode_cloid(order.strategy_id, prev_client_oid),
            oid_is_cloid: true,
        }];
        let (Ok(mp_n), Ok(head)) = (
            crate::action::encode_batch_modify(&mut self.mp, &m),
            envelope_open(&mut self.req),
        ) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        let Ok(aj_n) = crate::request::batch_modify_json(&mut self.req[head..], &m) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        Ok((mp_n, head + aj_n))
    }

    /// **LAW E-7 — replace a resting order with a new one.**
    ///
    /// One request, not a cancel plus a place. At Arm B's ~333
    /// reprices per instance that is the difference between fitting
    /// inside the address budget and not.
    ///
    /// The resting order is addressed BY CLOID and the replacement
    /// carries a DIFFERENT one, so every `userFills` row maps to
    /// exactly one quote. Both halves were measured against testnet
    /// before this existed (phase F, `exec-smoke --requote`) — the
    /// venue does not have to accept a modify that changes the id, and
    /// nothing in this repo could say that it did until it was asked.
    ///
    /// Named `_by_cloid`, like its cancel sibling, so that no inherent
    /// method shares a name with an [`OrderDispatch`] one: with both
    /// called `modify`, the inherent method shadowed the trait's at
    /// every call site that could see it, which is how the trait's own
    /// `modify` went unimplemented without anything noticing (BX0-F3).
    ///
    /// # Errors
    /// As [`Self::cancel_by_cloid`], plus a price or size that will not
    /// scale and an order kind with no TIF.
    pub fn modify_by_cloid(
        &mut self,
        prev_client_oid: u64,
        order: &Order,
    ) -> Result<(), DispatchError> {
        let (mp_n, action_end) = self.stage_modify(prev_client_oid, order)?;
        // A modify can MOVE exposure, so it answers to the submit rule
        // rather than the exit one — LAW E-7 makes it the requote path,
        // not a way around the governor.
        match self.send_action(mp_n, action_end, 1, Spend::Submit) {
            Ok(_) => {
                self.counters.modifies_sent = self.counters.modifies_sent.wrapping_add(1);
                // The member has traded this leg — recorded on the same
                // acceptance rule `submit` uses, because a requote is a
                // submit that kept its place in the queue.
                if self.assets.note_owner(order.sym, order.strategy_id) {
                    self.counters.owner_contested =
                        self.counters.owner_contested.wrapping_add(1);
                }
                Ok(())
            }
            Err(e) => {
                self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
                Err(e)
            }
        }
    }

    fn persist_budget(&mut self) {
        if self.last_persist.elapsed() < PERSIST_EVERY {
            return;
        }
        self.last_persist = Instant::now();
        let _ = budget::store(&self.budget_path, &self.budget);
    }
}

impl<const FILL_N: usize> OrderDispatch for HlExchange<FILL_N> {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        // 1. LAW E-4 — bound, never derived. An unbound symbol is a
        //    refusal, not an arithmetic problem.
        //
        //    THE INSTANCE COMES FROM THE ORDER. This read `0` until the
        //    roll handler existed, which was harmless only while the
        //    table was empty: a hardcoded 0 compares 0 to 0 forever, so
        //    the staleness check LAW E-4 exists for could never fire
        //    and a stale asset id would have sailed through. The member
        //    already states which instance it believes it is trading,
        //    in `client_oid`'s low bits (`core_types::instance_of`), so
        //    the table is asked about THAT one — and a table that has
        //    rolled refuses instead of sending an order to someone
        //    else's market.
        let asset = self
            .assets
            .lookup(order.sym, core_types::instance_of(order.client_oid))
            .map_err(|e| {
                // A STALE instance gets its own counter. It is the one
                // thing this module exists to catch — an order naming
                // an instance that has rolled is an order bound for
                // someone else's market — and folded into
                // `refused_local` it is indistinguishable from a spent
                // budget or an unbound symbol. This diff is what makes
                // it reachable for the first time.
                if matches!(e, crate::asset::AssetError::StaleInstance { .. }) {
                    self.counters.refused_stale = self.counters.refused_stale.wrapping_add(1);
                    // E6 commit 3: CONSECUTIVE. One is a race with a
                    // roll; a streak is a member quoting an instance
                    // that no longer exists.
                    self.asset_refusal_streak =
                        self.asset_refusal_streak.saturating_add(1);
                }
                self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
                DispatchError::NoLiveRoute
            })?;

        // 2. The budget is checked ONCE, inside `send_action` — still
        //    before the signature, and now in the one place that knows
        //    whether this action is a submit or an exit. Two checks
        //    would be two places that have to agree about one policy.

        // `checked_mul`, not `saturating_mul`: saturation clamps to
        // i64::MAX, which is POSITIVE and would sail straight past the
        // `<= 0` guard below — an overflow guard that cannot catch an
        // overflow.
        let (Some(px), Some(sz)) = (
            order.px.raw().checked_mul(ENGINE_TO_WIRE),
            order.qty.raw().checked_mul(ENGINE_TO_WIRE),
        ) else {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        if px <= 0 || sz <= 0 {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        }
        let Some(tif) = Self::tif_of(order.kind) else {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            return Err(DispatchError::NoLiveRoute);
        };
        let wire = OrderWire::new(asset, order.side == Side::Bid, px, sz, tif)
        // 3. LAW E-9 — the slot travels in the cloid, so the fill can
        //    be routed without a table when it comes back.
        .with_cloid(encode_cloid(order.strategy_id, order.client_oid));

        // Every local failure from here on bumps a counter. Without
        // that they are invisible: `stats()` reports nothing for this
        // arm, so an encode that silently refused every order would
        // look exactly like an engine that emitted none.
        let enc = |r: Result<usize, crate::msgpack::MsgPackErr>,
                       c: &mut HlExecCounters|
         -> Result<usize, DispatchError> {
            r.map_err(|_| {
                c.encode_failures = c.encode_failures.wrapping_add(1);
                DispatchError::EncodeOverflow
            })
        };
        let mp_n = enc(encode_order(&mut self.mp, &[wire], b"na"), &mut self.counters)?;
        let head = self.open_action()?;
        let aj_n = enc(order_json(&mut self.req[head..], &[wire], b"na"), &mut self.counters)?;

        // The nonce is taken LAST among the things that can fail, so a
        // local refusal cannot burn one. (HL only requires strictly
        // increasing nonces, so a gap is harmless — but not burning
        // one at all is simpler to reason about.)
        self.send_action(mp_n, head + aj_n, 1, Spend::Submit)?;
        self.counters.submitted = self.counters.submitted.wrapping_add(1);
        // WHO trades this leg — recorded on ACCEPTANCE, not on intent.
        // The venue settles a binary with a cloid-less fill, so
        // attribution has to come from somewhere that is not the fill,
        // and the only authority that cannot be wrong is a member whose
        // order the venue took. A submit refused by the budget, the
        // scale guards, the signer or the venue never traded, and a leg
        // we have not traded must not absorb a settlement.
        if self.assets.note_owner(order.sym, order.strategy_id) {
            self.counters.owner_contested = self.counters.owner_contested.wrapping_add(1);
        }
        Ok(())
    }

    /// **E5 — the router's cancel reaches the venue (BX0-F3).**
    ///
    /// `RoutedDispatcher` drives every live verb through this trait,
    /// and until BX0 this impl overrode `submit` alone: a live cancel
    /// fell through to the trait default, `Unsupported`, and never
    /// left the host — invisible while bin15 ran with its maker off.
    /// The three fields that name the order are forwarded unchanged:
    /// its market, its slot (LAW E-9 carries it in the cloid) and its
    /// client id. Spent as an exit, so the budget floor never blocks
    /// it.
    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        self.cancel_by_cloid(req.sym, req.strategy_id, req.client_oid)
    }

    /// **E5, LAW E-7 — the router's requote reaches the venue
    /// (BX0-F3).** Same defect as `cancel`, same repair: the resting
    /// order is named by the client id it was sent with, and the
    /// replacement goes as the router stamped it. Spent as a submit —
    /// a requote can move exposure, and the router has already put it
    /// through the risk gate as a `Replace`.
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        self.modify_by_cloid(req.prev_client_oid(), req.order())
    }

    /// **Always `None`, deliberately.**
    ///
    /// Fills reach the engine through fill lane 3, written by
    /// [`HlExchange::pump_user_events`]. Returning them here as well
    /// would book every venue fill twice — once through the lane and
    /// once through this call — which is precisely the double-counting
    /// LAW E-5 exists to prevent.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats::default()
    }

    /// **E7 — the arm's numbers, finally on a surface.** Every field
    /// here used to be a unit-test fact; four of the five blocking
    /// findings of the E7 review were invisible for exactly that
    /// reason.
    fn arm_counters(&self) -> clob_dispatcher::LiveArmCounters {
        let c = &self.counters;
        // COPY: 272 B POD (31 × u64 + 3 × i64) composed here and
        // returned by value — cold (1 Hz /state, 0.2 Hz /metrics); it is
        // BUILT from two sources (`counters`, `budget`) so there is
        // nothing to borrow — rejected: an out-param, for one struct read
        // twice a second.
        clob_dispatcher::LiveArmCounters {
            submitted: c.submitted,
            rejected: c.rejected,
            ioc_missed: c.ioc_missed,
            refused_local: c.refused_local,
            refused_stale: c.refused_stale,
            sent_unanswered: c.sent_unanswered,
            fills_booked: c.fills_booked,
            fills_unresolved: c.fills_unresolved,
            fills_foreign: c.fills_foreign,
            fills_dropped: c.fills_dropped,
            fills_refused: c.fills_refused,
            fills_scan_failed: c.fills_scan_failed,
            fills_unowned: c.fills_unowned,
            recon_ok: c.recon_ok,
            recon_failed: c.recon_failed,
            recon_drift_legs: c.recon_drift_legs,
            recon_unseen_legs: u64::from(c.recon_unseen_legs),
            sweep_left: c.sweep_left,
            sweep_stalled: c.sweep_stalled,
            cancel_all_unqueued: c.cancel_all_unqueued,
            ws_reconnects: c.ws_reconnects,
            ws_connect_failures: c.ws_connect_failures,
            rolls_bound: c.rolls_bound,
            rolls_refused: c.rolls_refused,
            owner_contested: c.owner_contested,
            budget_remaining: self.budget.remaining(),
            pnl_anchor_usd_1e6: self.pnl_anchor_1e6,
            session_pnl_usd_1e6: self.session_pnl_usd_1e6(),
            sweep_all_cancelled: c.sweep_all_cancelled,
            sweep_all_left: c.sweep_all_left,
            topup_ok: c.topup_ok,
            topup_failed: c.topup_failed,
            day_sync_ok: c.day_sync_ok,
            day_sync_failed: c.day_sync_failed,
        }
    }

    /// A live dispatcher invents nothing from a book.
    #[inline]
    fn observe_tick(&mut self, _tick: &Tick, _now_ns: NsTs) {}

    /// The worker's idle moment is this arm's only thread.
    /// LAW E-4's writer. A roll is the ONLY thing that may bind an
    /// asset id, and this is where it lands.
    ///
    /// One event binds BOTH legs: the wire carries the family's Yes
    /// symbol and the No leg is the next ordinal, which is the boot
    /// ordinal law (`4096 + 2*family + side`) and not an inference of
    /// this crate's — the ingress assigns them adjacently and
    /// `bin15_boot` copies them out that way.
    ///
    /// A SETTLED roll binds nothing and unbinds nothing, deliberately.
    /// `unbind` clears `prev_coin`, which is exactly the one-generation
    /// memory that lets a fill still in flight across the roll resolve
    /// (see [`AssetTable::sym_of_coin`]); the venue itself keeps a
    /// settled instance's coins subscribed, and the successor's `bind`
    /// overwrites in place and carries the old name forward. Dropping
    /// the binding here would throw away a real fill to tidy a table.
    fn on_venue_event(&mut self, event: &ChannelEvent) {
        if event.channel != ChannelId::InstrumentRoll as u8 {
            return;
        }
        if event.venue != VenueId::Hyperliquid as u8 {
            return;
        }
        let Some((outcome, settled)) = unpack_roll(event.venue_seq) else {
            // A kind byte no packer of ours writes. Neither created nor
            // settled — refused, never guessed (core_types::roll_kind_strict).
            self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
            return;
        };
        if settled {
            self.counters.rolls_settled = self.counters.rolls_settled.wrapping_add(1);
            return;
        }
        // A created roll with no outcome names nothing. Refuse rather
        // than bind slot 0 of somebody's market.
        if outcome == 0 {
            self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
            return;
        }
        // BOTH LEGS OR NEITHER. Everything that can fail is computed
        // and checked before the first mutation, because a table left
        // holding the Yes leg and not the No would book one side of a
        // position and count the other as a stranger's fill — while
        // `rolls_refused` said nothing had been bound.
        //
        // The YES leg is what the wire carries; NO is the next ordinal
        // (the boot ordinal law, not an inference of this crate's).
        // `sym + 1` is the NO leg — but `sym` comes off the wire, and
        // this crate has twice refused an unchecked add on exactly
        // this kind of value. Release sets `overflow-checks = false`,
        // the ordinal field is 24 bits, and `SYMBOL_ID_NONE` is
        // `u32::MAX`, so an unchecked `+ 1` could carry out of the
        // ordinal into the VENUE byte and bind a leg in another
        // venue's namespace. Refused at runtime in every profile, the
        // same ruling `AssetTable::asset_id` got.
        let ord = core_types::symbol_ordinal(event.sym);
        if event.sym == core_types::SYMBOL_ID_NONE || ord >= core_types::SYMBOL_ORDINAL_MASK {
            self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
            return;
        }
        let syms = [event.sym, event.sym + 1];
        let mut coins = [[0u8; crate::asset::COIN_MAX]; 2];
        let mut lens = [0usize; 2];
        let mut assets = [0u32; 2];
        for side in 0usize..2 {
            let Some(a) = AssetTable::asset_id(outcome, side as u8) else {
                self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
                return;
            };
            let Some(n) = AssetTable::outcome_coin(outcome, side as u8, &mut coins[side]) else {
                self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
                return;
            };
            assets[side] = a;
            lens[side] = n;
        }
        if !self.assets.would_fit(&syms) {
            self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
            return;
        }

        // LAW E-8 — what this roll RETIRES, recorded before the bind
        // overwrites the slot. Queued rather than swept here: a sweep
        // is one info round trip plus a cancel per resting order, and
        // `on_venue_event` runs INLINE ON THE ENGINE THREAD. Blocking
        // it at roll time is blocking it at the exact moment the
        // member wants to quote the successor. The idle path is where
        // a blocking HTTPS round trip belongs — the same reasoning
        // `reconcile` already carries — and it gives the operator's
        // "retry on the next idle" ruling for free.
        // A range loop: it indexes BOTH `syms` and `assets`, so there
        // is no `- 1` for an off-by-one to hide in. (`syms.iter()`
        // would borrow `syms` across the `&mut self` call — the same
        // reason the rollback loop below carries its allow.)
        for side in 0..syms.len() {
            if let Some((asset, coin, coin_len)) = self.assets.bound(syms[side]) {
                // A REPEAT of the roll that is already live retires
                // nothing. The venue re-sends `outcomeCreated` on a
                // reconnect snapshot and a replayed ring entry carries
                // it too — and without this the sweep would enumerate
                // the account and cancel every one of our quotes on a
                // LIVE leg, at the moment the member is quoting it.
                if asset != assets[side] {
                    self.queue_sweep(asset, coin, coin_len);
                }
            }
        }

        // From here nothing can fail: both names are non-empty and
        // within COIN_MAX by construction, and the slots are known to
        // be available. A failure here would be a bug in `bind`, and
        // `debug_assert!` is how this file says so.
        for side in 0usize..2 {
            let r = self.assets.bind(
                syms[side],
                assets[side],
                u64::from(outcome),
                &coins[side][..lens[side]],
            );
            debug_assert!(r.is_ok(), "a prechecked bind failed: {r:?}");
            if r.is_err() {
                // Unreachable by construction, and `debug_assert!` is a
                // no-op in the profile that trades — so this is the
                // RELEASE behaviour on a `bind` bug. Roll the first leg
                // back rather than return a half-bound family while
                // the counter says nothing was bound.
                // Index loop, per the doctrine — and clippy's
                // `iter().take(side)` would borrow `syms` across the
                // `&mut self.assets` call anyway.
                #[allow(clippy::needless_range_loop)]
                for done in 0usize..side {
                    self.assets.unbind(syms[done]);
                }
                self.counters.rolls_refused = self.counters.rolls_refused.wrapping_add(1);
                return;
            }
        }
        self.counters.rolls_bound = self.counters.rolls_bound.wrapping_add(1);
    }

    /// **E6 commit 3 — what this arm can see of the venue.**
    ///
    /// Raw observations only. The router owns every threshold, so two
    /// slots with different `halt_on_*` numbers reach different
    /// conclusions from one signal, and this file states no policy.
    fn halt_signal(&self) -> clob_dispatcher::HaltSignal {
        clob_dispatcher::HaltSignal::new(
            // `None` reports 0 — "no observation" — rather than an
            // infinite gap. An arm that has never connected is not an
            // arm that has gone quiet, and reporting u64::MAX here
            // would halt every boot before its first connect.
            self.last_ws_ok
                .map_or(0, |t| t.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64),
            // The reconciler measures a CONTRACT QUANTITY; the
            // threshold is money. Converted here, once, through the
            // function `recon` owns — the two numbers share a name
            // and not a unit, which `recon.rs` warns about in those
            // words.
            crate::recon::drift_qty_to_usd_1e6(self.counters.recon_drift_max_qty_1e6),
            self.reject_streak,
            self.asset_refusal_streak,
            self.budget.may_submit().is_err(),
            self.reconciled,
            // Age of the last reconciliation that AGREED. 0 = never,
            // which the interlock already covers; once it has agreed
            // once, a reconciler that stops agreeing (or stops
            // answering) is measured here and halts at
            // `halt_on_recon_stale_ms`.
            self.last_recon_ok
                .map_or(0, |t| t.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64),
        )
        // E7 session bound: judged whenever anchored, legs held or
        // not (they are valued at cost — `recon::account_view`), and
        // only from a balance this process has actually read — a
        // restored anchor with no reconciliation yet reports "not
        // judged" rather than a $-anchor delta.
        .with_pnl(
            self.pnl_anchor_1e6 != 0 && self.usdc_1e6 != 0,
            self.session_pnl_usd_1e6(),
        )
    }

    /// **E6 commit 3 — take every order of ours off the venue.**
    ///
    /// Queues a LAW E-8 sweep for every leg the asset table holds.
    /// The sweep machinery then does the work from the idle path:
    /// ask the venue what is resting, select the rows carrying our
    /// cloid (LAW E-9), cancel them 8 at a time, and keep the entry
    /// pending until the leg comes back clean.
    ///
    /// **Queued rather than cancelled inline, and the reasoning
    /// changed with commit 3a.** The operator's ruling was that a
    /// kill switch which queues work for an idle loop is not a kill
    /// switch — which was correct when nothing on the `--exec` path
    /// called `on_idle` at all. It does now, every 2 ms at worst, so
    /// the queue IS the synchronous path with a bound on it: this
    /// call returns immediately, the refusal latches immediately, and
    /// the venue is cleared by machinery that is already budgeted,
    /// already retried and already tested. Cancelling inline would
    /// put up to `MAX_OPEN_ORDERS` round trips on the engine thread
    /// at the exact moment something has gone wrong — see
    /// [`SWEEP_CANCELS_PER_IDLE`].
    ///
    /// # Errors
    /// [`DispatchError::QueueFull`] when the pending-sweep table
    /// could not hold every live leg. The caller halts anyway and
    /// retries, which is what drains the table.
    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        self.counters.cancel_all_runs = self.counters.cancel_all_runs.wrapping_add(1);
        // Everything `cancel_all_state` reports is relative to THIS
        // request, so the mark moves with it. A leg abandoned before
        // we were asked is not this cancel-all's business; one
        // abandoned after it is the whole point.
        self.cancel_all_mark = self.counters.sweep_left;
        self.cancel_all_unqueued_mark = self.counters.cancel_all_unqueued;

        // Collected first: `queue_sweep` borrows `self` mutably and
        // `for_each_live` borrows it immutably.
        let mut syms = [0u32; crate::asset::ASSET_SLOTS];
        let mut n = 0usize;
        self.assets.for_each_live(|sym, _coin, _booked| {
            if n < syms.len() {
                syms[n] = sym;
                n += 1;
            }
        });

        let mut queued = 0usize;
        let mut i = 0usize;
        while i < n {
            let sym = syms[i];
            i += 1;
            let Some((asset, coin, coin_len)) = self.assets.bound(sym) else {
                continue;
            };
            // A leg already pending is already covered.
            if self.sweep_pending_for(asset) {
                queued += 1;
                continue;
            }
            // **Checked BEFORE queueing, not after.** `queue_sweep`
            // answers a full table by bumping `sweep_left`, which is
            // the roll path's honest accounting for a stranded leg —
            // but this path is retried from every idle moment, so
            // going through it would add ~500 phantom strandings a
            // second to the one counter LAW E-8 arms an operator on.
            // Unqueued legs are counted once per run below instead.
            if self.sweeps_n >= MAX_PENDING_SWEEPS {
                continue;
            }
            self.queue_sweep(asset, coin, coin_len);
            queued += 1;
        }

        if queued == n {
            Ok(())
        } else {
            self.counters.cancel_all_unqueued = self
                .counters
                .cancel_all_unqueued
                .wrapping_add((n - queued) as u64);
            Err(DispatchError::QueueFull)
        }
    }

    /// **LAW E-8's confirmation.** See
    /// [`clob_dispatcher::CancelAllState`].
    ///
    /// A sweep terminates two ways, and only one of them is a
    /// confirmation: `sweep_one_pending` drops the entry when the
    /// venue reports nothing of ours resting on that leg (`k == 0`),
    /// and `spend_try` drops it — bumping `sweep_left` — when the
    /// retries run out. An empty table therefore means "clear" only
    /// when `sweep_left` has not moved since we were asked.
    fn cancel_all_state(&self) -> clob_dispatcher::CancelAllState {
        if self.sweeps_n > 0 {
            return clob_dispatcher::CancelAllState::Working;
        }
        // A leg the request could NOT queue is not clear either: nobody
        // is sweeping it. Reported as `Stranded` so the router asks
        // again, and a fresh request queues it into the space the
        // drained sweeps freed.
        if self.counters.sweep_left != self.cancel_all_mark
            || self.counters.cancel_all_unqueued != self.cancel_all_unqueued_mark
        {
            return clob_dispatcher::CancelAllState::Stranded;
        }
        clob_dispatcher::CancelAllState::Clear
    }

    fn on_idle(&mut self) -> bool {
        let worked = self.pump_user_events();
        self.persist_budget();
        // Before `reconcile`, deliberately: a sweep that leaves quotes
        // resting is drift the reconciler would then report, and the
        // useful ordering is to try the fix before measuring the
        // damage.
        self.sweep_one_pending();
        // S7-L1: the reconciliation and the top-up are both blocking
        // round trips; at most one of them per idle call.
        if !self.reconcile() {
            self.top_up_budget();
        }
        worked
    }

    /// **S7-L1** — the engine is stopping: every resting order of ours
    /// comes off the venue ([`HlExchange::cancel_ours_everywhere`];
    /// its counters carry the result to the drain's log line), and the
    /// budget is written one last time — the sweep's cancels count
    /// against the address like any other action.
    fn on_shutdown(&mut self) {
        let _ = self.cancel_ours_everywhere();
        let _ = budget::store(&self.budget_path, &self.budget);
    }

    /// **S7-L1 (gap A)** — see [`crate::dayspend`].
    fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
        if self.day_epoch == 0 {
            return None;
        }
        self.day_bought_1e6.get(slot).map(|&v| (self.day_epoch, v))
    }
}

/// **S7-L1 (gap E)** — what one signed `reserveRequestWeight` came to.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Reserve {
    /// Never left the host (encode, sign or connect failed): nothing
    /// was spent.
    NotSent,
    /// The venue's `{"status":"ok","response":{"type":"default"}}`.
    Accepted,
    /// A readable refusal: the venue said no, and nothing was bought.
    Refused,
    /// It left the host and no readable answer came back: it may have
    /// gone through, so it is charged as if it had.
    Unknown,
}

/// **S7-L1 (gap E)** — with the venue's own headroom in hand: buy one
/// top-up of `weight`? Only inside the band `(floor, floor + weight]`
/// — above it there IS headroom (a purchase this arm could not confirm
/// may already have gone through), and at or under the floor the router
/// has halted the slot, sticky — and only while the day's ceiling holds
/// one more. Pure, so the arithmetic is pinned without a venue.
#[inline]
#[must_use]
const fn topup_fits(remaining: i64, floor: i64, weight: u64, used: u64, day_max: u64) -> bool {
    let w = if weight > i64::MAX as u64 {
        i64::MAX
    } else {
        weight as i64
    };
    remaining > floor && remaining <= floor.saturating_add(w) && used.saturating_add(weight) <= day_max
}

/// The `InstrumentRoll` `venue_seq` layout, bits 0..32 and 56.
///
/// E6: no longer duplicated. §6.1 forbids this crate depending on the
/// market-data crate, which is why the unpack was restated here — but
/// the codec now lives in `core_types`, which this crate already
/// depends on, so the restatement is gone and this is the projection
/// onto the two fields the asset binding needs.
///
/// Bits 0..32 are the **outcome id**, NOT `enc`. `AssetTable::asset_id`
/// and `outcome_coin` do the `× 10 + side` themselves, so feeding them
/// `enc` would be a silent tenfold error naming a real other market.
///
/// `settled` is the STRICT reading (`core_types::roll_kind_strict`):
/// `None` for a kind byte no packer of ours writes, which the caller
/// refuses. The low-bit mask this used before read `0x03` as settled
/// while `strategy_bin15` read it as created — one frame, two answers
/// (E7 review, 2026-09-19); all three live readers share the strict
/// one now.
#[inline]
const fn unpack_roll(seq: u64) -> Option<(u32, bool)> {
    let (outcome, _twap_s, _family, _masked) = core_types::unpack_roll_seq(seq);
    match core_types::roll_kind_strict(seq) {
        Some(settled) => Some((outcome, settled)),
        None => None,
    }
}

/// Wall clock, nanoseconds. Read ONCE PER PUMP, never per fill.
fn now_ns() -> NsTs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(NsTs::MAX)) as NsTs)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Scope, HOST_TESTNET};
    use core_ring::Ring;
    use core_types::{Price, Qty, VenueId};

    const KEY: [u8; 32] = [0x21; 32];
    const ADDR: [u8; 20] = [0x22; 20];

    fn exchange() -> HlExchange<64> {
        exchange_at(HOST_TESTNET)
    }

    /// The same, against `host`. Tests that must not touch the
    /// network pass a loopback address: `HlHttp` resolves at
    /// construction and connects lazily, so nothing leaves until a
    /// post is attempted.
    fn exchange_at(host: &str) -> HlExchange<64> {
        let (p, _c) = Ring::<Fill, 64>::new().split();
        let tls = core_net::TlsTransport::default_client_config();
        HlExchange::new(
            &HlConfig::new(Scope::Testnet, host, 'b', KEY, ADDR).expect("cfg"),
            tls,
            p,
            std::env::temp_dir().join(format!("mv-hlx-{}.state", std::process::id())),
            100,
        )
        .expect("build")
    }

    fn order(sym: u32, kind: u8) -> Order {
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            kind,
            Price::from_raw(470_000),
            Qty::from_raw(25_000_000),
            7,
        );
        o.strategy_id = 3;
        o
    }

    /// **LAW E-4.** An unbound symbol is refused, and nothing is sent.
    #[test]
    fn an_unbound_symbol_is_refused_before_anything_is_signed() {
        let mut x = exchange();
        let e = x.submit(&order(42, ORDER_KIND_MAKER)).unwrap_err();
        assert_eq!(e, DispatchError::NoLiveRoute);
        assert_eq!(x.counters().refused_local, 1);
        assert_eq!(x.counters().submitted, 0);
        // Nothing left the host, so nothing was spent.
        assert_eq!(x.budget_remaining(), x.budget_remaining());
    }

    /// **The budget is checked before signing**, so a refusal cannot
    /// burn a nonce.
    #[test]
    fn a_spent_budget_refuses_before_the_signer_is_touched() {
        let mut x = exchange();
        // Instance 7 — the same instance `order()`'s client_oid names.
        x.assets_mut().bind(42, 3, 7, b"#42").expect("bind");
        // A cold budget has zero headroom by construction.
        assert!(x.budget_remaining() <= 0);
        let e = x.submit(&order(42, ORDER_KIND_MAKER)).unwrap_err();
        assert_eq!(e, DispatchError::SlotDisabled);
        assert_eq!(x.counters().refused_local, 1);
    }

    /// **LAW E-5.** Fills arrive on the lane, never through this call.
    /// Returning them here as well would book every fill twice.
    #[test]
    fn the_dispatcher_never_yields_a_fill_directly() {
        let mut x = exchange();
        assert!(x.try_next_fill().is_none());
        assert!(x.try_next_fill().is_none());
    }

    /// A post-only maker must never be able to take, and an unknown
    /// kind must be refused by BOTH arms or they describe different
    /// worlds.
    #[test]
    fn the_tif_mapping_cannot_turn_a_maker_into_a_taker() {
        assert_eq!(HlExchange::<8>::tif_of(ORDER_KIND_MAKER), Some(Tif::Alo));
        assert_eq!(HlExchange::<8>::tif_of(ORDER_KIND_IOC), Some(Tif::Ioc));
        for k in [2u8, 7, 255] {
            assert_eq!(
                HlExchange::<8>::tif_of(k),
                None,
                "kind {k} was mapped instead of refused; the paper arm refuses it"
            );
        }
    }

    /// `saturating_mul` clamps to a POSITIVE i64::MAX, which would
    /// sail past a `<= 0` guard — an overflow check that cannot catch
    /// an overflow.
    #[test]
    fn an_overflowing_price_is_refused_rather_than_clamped() {
        let mut x = exchange();
        x.assets_mut().bind(9, 3, 7, b"#9").expect("bind");
        let mut o = order(9, ORDER_KIND_MAKER);
        o.px = Price::from_raw(i64::MAX);
        // Refused for SOME local reason before anything is sent; the
        // budget is cold here, so assert only that nothing was sent.
        assert!(x.submit(&o).is_err());
        assert_eq!(x.counters().submitted, 0);
        assert_eq!(
            i64::MAX.checked_mul(ENGINE_TO_WIRE),
            None,
            "the guard depends on checked_mul refusing this"
        );
    }

    /// The fill router must survive a frame bigger than any buffer a
    /// steady-state frame would need. This is the bug the review
    /// found: a 64-slot scratch against a ~2,000-fill snapshot
    /// discarded every reconnect snapshot in silence.
    #[test]
    fn a_snapshot_sized_frame_does_not_vanish() {
        let mut x = exchange();
        // 400 fills in one frame — far past any steady-state size.
        let mut body = String::from(
            r#"{"channel":"userFills","data":{"isSnapshot":true,"fills":["#,
        );
        for i in 0..400u64 {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!(
                r#"{{"coin":"BTC","px":"1","sz":"1","side":"B","time":1,"oid":{},"tid":{}}}"#,
                i + 1,
                i + 1
            ));
        }
        body.push_str("]}}");
        let booked = x.route_fills(body.as_bytes());
        // Nothing is booked (no symbol binding yet) — but the frame
        // must have SCANNED, and every row must be accounted for.
        assert_eq!(booked, 0);
        assert_eq!(
            x.counters().fills_scan_failed,
            0,
            "a snapshot-sized frame was discarded as unscannable"
        );
        assert_eq!(x.counters().fills_unresolved, 400, "every row accounted for");
    }

    /// A venue timestamp that will not convert must NOT be clamped.
    ///
    /// `saturating_mul` would place the fill at `i64::MAX` ns — the
    /// year 2262 — in a tape that is read in time order. The fill is
    /// real, so it is still booked; the stamp is local and the counter
    /// says so.
    #[test]
    fn an_unconvertible_venue_timestamp_falls_back_to_the_local_clock() {
        let mut x = exchange();
        // u64::MAX ms cannot be multiplied into nanoseconds at all.
        let body = format!(
            r#"{{"channel":"userFills","data":{{"fills":[{{"coin":"BTC","px":"1","sz":"1","side":"B","time":{},"oid":1,"tid":1}}]}}}}"#,
            u64::MAX
        );
        x.route_fills(body.as_bytes());
        assert_eq!(
            x.counters().fills_bad_ts,
            1,
            "an unconvertible venue stamp was accepted silently"
        );
        assert_eq!(
            u64::MAX.checked_mul(1_000_000),
            None,
            "the guard depends on checked_mul refusing this"
        );
    }

    /// The router reads the socket's OWN receive buffer.
    ///
    /// This is the shape of the bug the audit found: `route_fills`
    /// took `&mut self`, so the pump's closure could not call it, so
    /// every frame was copied into a heap `Vec` first — on the thread
    /// that also signs and submits. `route_frame` exists to take
    /// disjoint borrows instead, and this test is what holds that
    /// property: it drives the router through a closure of exactly the
    /// shape [`UserWs::pump`] passes, while `ws` is borrowed.
    #[test]
    fn a_frame_routes_from_a_borrowed_slice_with_no_staging_copy() {
        let mut x = exchange();
        let frame = br#"{"channel":"userFills","data":{"fills":[{"coin":"BTC","px":"1","sz":"1","side":"B","time":1,"oid":7,"tid":7}]}}"#;
        let recv_ns = now_ns();
        let HlExchange {
            ws,
            assets,
            scratch,
            seen,
            budget,
            fills,
            counters,
            ..
        } = &mut x;
        // `ws` is borrowed for the whole closure, exactly as `pump`
        // borrows it. If this compiles, the production path needs no
        // copy; if it stops compiling, the copy is back.
        let _borrowed = &mut *ws;
        let mut feed = |payload: &[u8]| {
            HlExchange::<64>::route_frame(
                payload, recv_ns, assets, scratch, seen, budget, fills, counters,
            );
        };
        feed(&frame[..]);
        assert_eq!(x.counters().fills_unresolved, 1, "the frame was routed");
    }

    /// A frame from another channel is NOT a scan failure — the two
    /// must stay distinguishable or a discarded snapshot looks like an
    /// orderUpdates frame.
    #[test]
    fn another_channel_is_not_counted_as_a_failure() {
        let mut x = exchange();
        assert_eq!(x.route_fills(br#"{"channel":"orderUpdates","data":[]}"#), 0);
        assert_eq!(x.counters().fills_scan_failed, 0);
        // But a userFills frame that will not scan IS counted.
        assert_eq!(
            x.route_fills(br#"{"channel":"userFills","data":{"fills":[{"coin":"x"}]}}"#),
            0
        );
        assert_eq!(x.counters().fills_scan_failed, 1);
    }

    /// A SNAPSHOT is history. Re-adding its notional to the budget on
    /// every boot inflates the allowance — the permissive direction
    /// the budget module exists to prevent.
    #[test]
    fn a_snapshot_does_not_inflate_the_budget() {
        let mut x = exchange();
        let before = x.budget_remaining();
        let snap = br#"{"channel":"userFills","data":{"isSnapshot":true,"fills":[{"coin":"BTC","px":"1000","sz":"1000","side":"B","time":1,"oid":1,"tid":1}]}}"#;
        x.route_fills(snap);
        assert_eq!(
            x.budget_remaining(),
            before,
            "a replayed snapshot bought us request budget"
        );
        // A LIVE fill does accrue.
        let live = br#"{"channel":"userFills","data":{"isSnapshot":false,"fills":[{"coin":"BTC","px":"1000","sz":"1000","side":"B","time":1,"oid":2,"tid":2}]}}"#;
        x.route_fills(live);
        assert!(x.budget_remaining() > before, "a live fill must earn budget");
    }

    /// The engine's scale is 1e6; the venue's is 1e8.
    #[test]
    fn the_scale_to_the_wire_is_a_hundred() {
        assert_eq!(ENGINE_TO_WIRE, 100);
        let o = order(1, ORDER_KIND_MAKER);
        assert_eq!(o.px.raw() * ENGINE_TO_WIRE, 47_000_000, "0.47");
        assert_eq!(o.qty.raw() * ENGINE_TO_WIRE, 2_500_000_000, "25 contracts");
    }

    /// **LAW E-8's confirmation: an empty sweep table is not a clear
    /// venue.**
    ///
    /// A sweep entry is dropped two ways and only one of them is a
    /// confirmation. If `cancel_all_state` read the empty table alone
    /// it would report `Clear` over orders the arm gave up on — and
    /// the router would stop retrying and zero its resting count. The
    /// two breaks this test exists for are exactly those readings.
    #[test]
    fn an_abandoned_sweep_is_stranded_not_clear() {
        use clob_dispatcher::CancelAllState;
        let mut x = exchange_at("127.0.0.1");

        // Nothing asked, nothing pending: there is nothing of ours to
        // be resting.
        assert_eq!(x.cancel_all_state(), CancelAllState::Clear);

        // Asked, and a leg queued.
        x.cancel_all_mark = x.counters.sweep_left;
        x.queue_sweep(7, [0u8; crate::asset::COIN_MAX], 0);
        assert_eq!(
            x.cancel_all_state(),
            CancelAllState::Working,
            "queued is not cancelled"
        );

        // The sweep is dropped having CONFIRMED the leg is clean —
        // the `k == 0` path, which does not touch `sweep_left`.
        x.drop_sweep(0);
        assert_eq!(x.cancel_all_state(), CancelAllState::Clear);

        // Now the other ending: queued again, and abandoned with its
        // retries spent.
        x.cancel_all_mark = x.counters.sweep_left;
        x.queue_sweep(7, [0u8; crate::asset::COIN_MAX], 0);
        x.sweeps[0].tries = 1;
        x.spend_try(0);
        assert_eq!(x.sweeps_n, 0, "the table is empty either way");
        assert_ne!(x.counters.sweep_left, x.cancel_all_mark);
        assert_eq!(
            x.cancel_all_state(),
            CancelAllState::Stranded,
            "an empty table over orders we never cancelled is NOT clear"
        );

        // A fresh request re-marks, so the next sweep is judged on
        // its own outcome rather than the last one's.
        x.cancel_all_mark = x.counters.sweep_left;
        assert_eq!(x.cancel_all_state(), CancelAllState::Clear);
    }

    /// A cancel-all retried from every idle moment must not inflate
    /// `sweep_left` — that counter is what an operator arms on for a
    /// stranded quote, and a full table answered through `queue_sweep`
    /// would add one phantom stranding per leg per poll.
    #[test]
    fn a_retried_cancel_all_does_not_manufacture_stranded_legs() {
        let mut x = exchange_at("127.0.0.1");
        // Fill the pending table with legs the cancel-all did not put
        // there.
        for a in 0..MAX_PENDING_SWEEPS as u32 {
            x.queue_sweep(a, [0u8; crate::asset::COIN_MAX], 0);
        }
        assert_eq!(x.sweeps_n, MAX_PENDING_SWEEPS);
        let before = x.counters.sweep_left;

        // An unqueueable leg, asked for a hundred times.
        for _ in 0..100 {
            let _ = x.cancel_all();
        }
        assert_eq!(
            x.counters.sweep_left, before,
            "a retry is not a stranding"
        );
    }

    /// Counters exist so an operator can tell a refusal from a
    /// rejection from a drop. A dropped fill above all: that is a
    /// position the engine does not know it has.
    #[test]
    fn the_counters_distinguish_every_way_a_fill_can_fail_to_land() {
        let c = HlExecCounters::default();
        assert_eq!(c.fills_dropped, 0);
        assert_eq!(c.fills_foreign, 0);
        assert_eq!(c.fills_unresolved, 0);
        assert_eq!(c.refused_local, 0);
        assert_eq!(c.rejected, 0);
        assert_eq!(core::mem::align_of::<HlExecCounters>(), 64);
        // Pinned, not merely aligned. The block is copied whole on
        // every `/metrics` publish, and it has grown from two cache
        // lines to three, to FOUR for E5's cancel, modify and sweep
        // counters, and now to FIVE for `sent_unanswered`. Each growth
        // is meant to be a decision rather than a surprise, which is
        // what this assertion is for.
        assert_eq!(
            core::mem::size_of::<HlExecCounters>(),
            384,
            "HlExecCounters changed size — 45 eight-byte slots (360 B: 44 \
             u64/i64 + the u32 pair) rounded up to six 64-byte lines, with \
             room for three more before it grows to seven. S7-L1 added six \
             (the account-wide sweep, the day-spend read, the top-up) and \
             took it from five lines to six, on purpose."
        );
    }

    /// **An UNBOUND coin still books nothing.** That property is the
    /// whole reason the table answers by comparing bytes rather than
    /// by parsing `+<enc>`: a fill booked against a guessed symbol
    /// moves a position the member never took, silently and
    /// permanently, while a fill not booked is caught by
    /// reconciliation inside a minute.
    /// **A roll queues the leg it RETIRES, not the one it creates.**
    /// The sweep exists to take quotes off a dead instance, and the
    /// recording has to happen before the rebind overwrites the slot —
    /// after it, the table names the successor and the ended leg is
    /// unnameable.
    #[test]
    fn a_roll_queues_the_leg_it_retires_before_the_rebind_hides_it() {
        let mut x = exchange();
        // First roll: nothing was bound, so nothing is retired.
        x.on_venue_event(&roll(3253, 0, false, 4096));
        assert_eq!(x.sweeps_n, 0, "a first bind retires nothing");
        let first = x.assets.bound(4096).expect("bound").0;

        // Second roll onto the SAME symbols: two legs retire.
        x.on_venue_event(&roll(3254, 0, false, 4096));
        assert_eq!(x.sweeps_n, 2, "both legs of the ended instance");
        let queued: Vec<u32> = x.sweeps[..2].iter().map(|e| e.asset).collect();
        assert!(queued.contains(&first), "the OLD asset id, not the new one");
        assert!(
            !queued.contains(&x.assets.bound(4096).expect("rebound").0),
            "the successor is live and must never be swept"
        );
        // And the coin bytes travelled with it, so the sweep can match
        // the venue's own rows without deriving a name from an id.
        assert!(x.sweeps[0].coin_len > 1, "a real name, not an empty one");
        assert_eq!(x.sweeps[0].coin[0], b'#', "the FILL namespace, not `+`");
    }

    // ---------- E5: the budget counts what left the host ----------

    /// **The rule, stated once and asserted once.**
    ///
    /// Before E5 the counting sat after the `?` in `send_action`, so
    /// only a SUCCESSFUL post was counted. A request the venue
    /// received and answered unreadably — a stalled server, a
    /// connection that died mid-response — was never counted, and the
    /// governor drifted optimistic. `budget.rs` names that the wrong
    /// direction: under-counting means exceeding the venue's real
    /// address limit and then reading the rate-limit answer as a
    /// transport problem.
    #[test]
    fn every_request_that_may_have_left_the_host_counts_against_the_address() {
        type R = Result<(u16, core::ops::Range<usize>), crate::http::PostErr>;
        let ok: R = Ok((200, 0..1));
        assert!(
            HlExchange::<64>::counts_against_address(&ok),
            "a success obviously left"
        );

        let after: R = Err(crate::http::PostErr {
            err: crate::http::HttpErr::Timeout,
            left_host: true,
        });
        assert!(
            HlExchange::<64>::counts_against_address(&after),
            "THE case this exists for: the venue has it, we do not know what it did"
        );

        let before: R = Err(crate::http::PostErr::before_send(
            crate::http::HttpErr::Dns,
        ));
        assert!(
            !HlExchange::<64>::counts_against_address(&before),
            "a request that never reached a socket is not an action"
        );
        // And the two are NOT distinguishable by the error variant,
        // which is why the flag exists: `Disconnected` is both.
        let d_before: R = Err(crate::http::PostErr::before_send(
            crate::http::HttpErr::Disconnected,
        ));
        let d_after: R = Err(crate::http::PostErr {
            err: crate::http::HttpErr::Disconnected,
            left_host: true,
        });
        assert!(!HlExchange::<64>::counts_against_address(&d_before));
        assert!(HlExchange::<64>::counts_against_address(&d_after));
    }

    /// The predicate reaching the governor. Asserted here rather than
    /// only through a socket, because the failure this guards is a
    /// silent one: the budget simply reads lower than the venue's own
    /// count, and nothing says so until a rate-limit answer arrives
    /// looking like a transport problem.
    #[test]
    fn a_post_that_left_the_host_spends_budget_even_when_it_failed() {
        let mut x = exchange_at("127.0.0.1");
        let before = x.budget.spent();

        x.count_post(
            &Err(crate::http::PostErr::before_send(
                crate::http::HttpErr::Dns,
            )),
            1,
        );
        assert_eq!(x.budget.spent(), before, "nothing left, nothing spent");

        x.count_post(
            &Err(crate::http::PostErr {
                err: crate::http::HttpErr::Timeout,
                left_host: true,
            }),
            1,
        );
        assert_eq!(
            x.budget.spent(),
            before + 1,
            "the venue has it — it is spent whether or not we read the answer"
        );

        x.count_post(&Ok((200, 0..1)), 1);
        assert_eq!(x.budget.spent(), before + 2);

        // A BATCH is charged per item (§2.2: one address request per
        // order in the action), and a zero-item batch still costs the
        // one request it was.
        x.count_post(&Ok((200, 0..1)), 3);
        assert_eq!(x.budget.spent(), before + 5, "three orders, three requests");
        x.count_post(&Ok((200, 0..1)), 0);
        assert_eq!(x.budget.spent(), before + 6, "never less than the request itself");
    }

    /// The other half, end to end through `send_action`: a submit
    /// that could not reach a socket at all spends nothing.
    ///
    /// Driven at a port with no listener, so the connect fails before
    /// any byte is written — the one failure shape reachable without
    /// a server. The post-write half is pinned by the TLS loopback
    /// suite (`a_mid_body_disconnect_…`, `a_stalled_server_…`), which
    /// asserts `left_host` on a server that has already read the
    /// request.
    #[test]
    fn a_submit_that_never_reached_a_socket_spends_no_budget() {
        let mut x = exchange_at("127.0.0.1");
        x.assets.bind(7, 100_000_001, 1, b"#1").expect("bind");
        let before = x.budget.spent();
        let e = x.submit(&order(7, core_fill::ORDER_KIND_IOC));
        assert!(e.is_err(), "nothing is listening, so nothing was sent");
        assert_eq!(
            x.budget.spent(),
            before,
            "a request that never left must not be charged to the address"
        );
        assert_eq!(x.counters.sent_unanswered, 0, "and nothing is in doubt");
    }

    /// E7-F1: with nobody listening, the venue seed changes nothing —
    /// the budget `new` loaded (cold, for a fresh state path) stands
    /// and the source says so. The venue half is pinned by the
    /// `scan_rate_limit` / `from_venue` tests in `budget`.
    #[test]
    fn a_venue_that_does_not_answer_leaves_the_boot_budget_alone() {
        let mut x = exchange_at("127.0.0.1");
        assert_eq!(x.budget_source(), BudgetSource::Cold);
        let before = x.budget;
        assert_eq!(x.seed_budget_from_venue(), BudgetSource::Cold);
        assert_eq!(x.budget, before, "no answer, no change");
        assert!(x.budget_remaining() <= 0, "and cold still refuses");
    }

    /// **E7 session bound — the anchor is set ONCE, at the first
    /// reading, as the equity AT COST, and the signal is judged from
    /// there whether or not a leg is held.** A boot with no anchor
    /// file starts unanchored; zero USDC does not anchor; the first
    /// reading does — legs at cost included — and persists; buying a
    /// leg moves nothing (USDC down, cost up by the same premium); a
    /// settlement or a sale moves the delta; later readings never
    /// move the anchor; and a second `new` on the same directory
    /// restores it.
    #[test]
    fn the_session_anchor_is_the_first_equity_at_cost_and_survives_a_restart() {
        use crate::recon::AccountView;
        let dir = std::env::temp_dir().join(format!("mv-hlx-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let budget_path = dir.join(budget::DEFAULT_STATE_PATH);
        let build = || {
            let (p, _c) = Ring::<Fill, 64>::new().split();
            HlExchange::<64>::new(
                &HlConfig::new(Scope::Testnet, "127.0.0.1", 'b', KEY, ADDR).expect("cfg"),
                core_net::TlsTransport::default_client_config(),
                p,
                budget_path.clone(),
                100,
            )
            .expect("build")
        };
        let mut x = build();
        assert_eq!(x.pnl_anchor_usd_1e6(), 0, "no file, no anchor");
        assert_eq!(x.pnl_state_path(), dir.join(crate::anchor::DEFAULT_STATE_PATH));
        let sig = x.halt_signal();
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (0, 0), "unanchored: not judged");

        // Zero USDC: nothing to bound.
        assert!(!x.note_account(AccountView::new(0, 0, 0)));
        assert_eq!(x.pnl_anchor_usd_1e6(), 0);
        assert_eq!(x.halt_signal().pnl_judged, 0);

        // The first reading anchors — a held leg at cost included.
        assert!(x.note_account(AccountView::new(7_270_000, 1_360_000, 1)), "the anchor");
        assert_eq!(x.pnl_anchor_usd_1e6(), 8_630_000, "USDC plus the leg at cost");
        assert_eq!(x.session_pnl_usd_1e6(), 0);
        let sig = x.halt_signal();
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (1, 0), "judged while holding");

        // Buying another leg moves nothing: USDC down, cost up.
        assert!(!x.note_account(AccountView::new(5_270_000, 3_360_000, 2)));
        assert_eq!(x.session_pnl_usd_1e6(), 0, "a premium paid is not a loss");

        // Both legs settle: one pays 2 × $1, one pays nothing.
        assert!(!x.note_account(AccountView::new(7_270_000, 0, 0)));
        assert_eq!(x.pnl_anchor_usd_1e6(), 8_630_000, "never moved");
        assert_eq!(x.session_pnl_usd_1e6(), -1_360_000);
        assert_eq!(x.halt_signal().pnl_delta_usd_1e6, -1_360_000);

        // Later readings move the delta, never the anchor.
        assert!(!x.note_account(AccountView::new(23_630_000, 0, 0)));
        assert_eq!(x.session_pnl_usd_1e6(), 15_000_000);
        assert!(!x.note_account(AccountView::new(1_630_000, 2_000_000, 2)));
        let sig = x.halt_signal();
        assert_eq!((sig.pnl_judged, sig.pnl_delta_usd_1e6), (1, -5_000_000), "judged, held");
        assert_eq!(x.pnl_anchor_usd_1e6(), 8_630_000, "and keeps the anchor");

        // `reconcile` is what persists (it needs a venue); the module
        // it calls is pinned here so the restart half is real.
        let a = crate::anchor::PnlAnchor {
            address: ADDR,
            usdc_1e6: x.pnl_anchor_usd_1e6(),
            set_unix_s: 1,
        };
        crate::anchor::store(x.pnl_state_path(), a).expect("persists");
        let y = build();
        assert_eq!(y.pnl_anchor_usd_1e6(), 8_630_000, "a restart restores the anchor");
        let sig = y.halt_signal();
        assert_eq!(
            (sig.pnl_judged, sig.pnl_delta_usd_1e6),
            (0, 0),
            "restored but nothing reconciled yet: not judged, no $-anchor delta"
        );
        assert_eq!(y.arm_counters().pnl_anchor_usd_1e6, 8_630_000);
        assert_eq!(y.arm_counters().session_pnl_usd_1e6, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **S7-L1 (gap E) — the top-up buys only inside its band and
    /// under its ceiling.** Floor 2 000, weight 5 000, ceiling 30 000.
    #[test]
    fn a_topup_buys_only_between_the_floor_and_one_topup_above_it() {
        assert!(topup_fits(7_000, 2_000, 5_000, 0, 30_000), "the top of the band");
        assert!(topup_fits(2_001, 2_000, 5_000, 0, 30_000), "just over the floor");
        assert!(!topup_fits(7_001, 2_000, 5_000, 0, 30_000), "headroom: nothing to buy");
        assert!(!topup_fits(2_000, 2_000, 5_000, 0, 30_000), "at the floor: halted, sticky");
        assert!(!topup_fits(-50, 2_000, 5_000, 0, 30_000), "past the cliff");
        assert!(topup_fits(5_000, 2_000, 5_000, 25_000, 30_000), "the day's last one");
        assert!(!topup_fits(5_000, 2_000, 5_000, 25_001, 30_000), "over the ceiling");
        assert!(!topup_fits(5_000, 2_000, 5_000, u64::MAX, 30_000), "an unreadable day");
        // Saturates rather than wrapping: a weight past i64 reads as the
        // widest band, never as a negative one.
        assert!(topup_fits(5_000, 2_000, u64::MAX, 0, u64::MAX));
        assert!(!topup_fits(5_000, 2_000, u64::MAX, 1, u64::MAX - 1), "and the ceiling holds");
    }

    /// E7-F2: the venue's IoC miss is neither an acceptance nor a
    /// rejection. It is counted on its own, the caller still sees an
    /// error, and the reject streak — which is the operator's
    /// "the venue keeps saying NO" kill switch — does not move.
    #[test]
    fn an_ioc_miss_is_counted_apart_and_moves_no_streak() {
        let mut x = exchange_at("127.0.0.1");
        x.reject_streak = 2;
        let miss = HlOk {
            statuses: 1,
            errors: 1,
            ioc_misses: 1,
            ..HlOk::default()
        };
        let r = x.judge(Spend::Submit, Ok(HlResponse::Ok(miss)));
        assert!(matches!(r, Err(DispatchError::Http(200))), "nothing traded: {r:?}");
        assert_eq!(x.counters.ioc_missed, 1);
        assert_eq!(x.counters.rejected, 0, "a miss is not a refusal");
        assert_eq!(x.reject_streak, 2, "and it neither bumps nor resets the streak");

        // The same wording on a CANCEL is not a miss — there is no
        // such thing as a cancel that found no counterparty.
        let r = x.judge(Spend::Cancel, Ok(HlResponse::Ok(miss)));
        assert!(matches!(r, Err(DispatchError::Http(200))));
        assert_eq!(x.counters.rejected, 1);
        assert_eq!(x.reject_streak, 3);

        // A genuine refusal still counts and still climbs.
        let refusal = HlOk {
            statuses: 1,
            errors: 1,
            ..HlOk::default()
        };
        let r = x.judge(Spend::Submit, Ok(HlResponse::Ok(refusal)));
        assert!(matches!(r, Err(DispatchError::Http(200))));
        assert_eq!(x.counters.rejected, 2);
        assert_eq!(x.reject_streak, 4);
        assert_eq!(x.counters.ioc_missed, 1, "unchanged");

        // A fill ends the streak, as before.
        let filled = HlOk {
            statuses: 1,
            any_filled: true,
            oid: 42,
            ..HlOk::default()
        };
        let ok = x.judge(Spend::Submit, Ok(HlResponse::Ok(filled))).expect("filled");
        assert_eq!(ok.oid, 42);
        assert_eq!(x.reject_streak, 0);

        // An unreadable answer is a rejection AND a malformed error.
        let r = x.judge(Spend::Submit, Err(crate::response::ScanErr::Malformed));
        assert!(matches!(r, Err(DispatchError::JsonMalformed)));
        assert_eq!(x.counters.rejected, 3);
        assert_eq!(x.reject_streak, 1);
    }

    /// **A REPEAT of the live roll retires nothing.** The venue
    /// re-sends `outcomeCreated` on a reconnect snapshot and a replayed
    /// ring entry carries it too. Without the guard the queued asset is
    /// the very one the bind re-establishes as live, and the next idle
    /// would enumerate the account and cancel every one of our quotes
    /// on a LIVE leg — at the moment the member is quoting it.
    #[test]
    fn a_repeated_roll_never_queues_a_sweep_of_the_live_leg() {
        let mut x = exchange();
        x.on_venue_event(&roll(3253, 0, false, 4096));
        assert_eq!(x.sweeps_n, 0);

        // The SAME outcome again, twice.
        x.on_venue_event(&roll(3253, 0, false, 4096));
        x.on_venue_event(&roll(3253, 0, false, 4096));
        assert_eq!(
            x.sweeps_n, 0,
            "the leg the bind re-establishes as live is not retired"
        );

        // A genuinely NEW outcome still retires the old one.
        x.on_venue_event(&roll(3254, 0, false, 4096));
        assert_eq!(x.sweeps_n, 2);
    }

    /// **A cap never blocks an EXIT.** `AddressBudget::may_cancel` has
    /// said so since it was written and had no callers until E5; the
    /// refactor that gave three verbs one tail is exactly where that
    /// rule could have been inverted. At the floor a submit is refused
    /// and a cancel is not.
    ///
    /// Without this, a transient budget squeeze would turn into
    /// `sweep_left` — "orders left resting on a dead instance", the
    /// number E6's kill switch reads — while the real cause was our own
    /// governor, and the entry would be dropped for good.
    #[test]
    fn a_spent_budget_refuses_a_submit_and_never_a_cancel() {
        let mut x = exchange();
        // Spend it down to the floor.
        while x.budget.may_submit().is_ok() {
            x.budget.on_action_sent(1);
        }
        assert!(x.budget.may_submit().is_err(), "the floor is reached");
        assert!(x.budget.may_cancel(), "and an exit is still permitted");

        // The submit path refuses locally, before the signer.
        let before = x.counters.refused_local;
        let o = order(4096, 0);
        assert!(x.submit(&o).is_err());
        assert!(x.counters.refused_local > before);

        // The symbol must be BOUND, or the cancel is refused by the
        // table before the budget is ever consulted — and the test
        // would then pass whatever the budget policy is. (It did: this
        // assertion survived inverting `Spend::Cancel` to the submit
        // rule, which is exactly the adjacent-measurement shape this
        // lane keeps producing.)
        x.assets_mut()
            .bind(4096, 100_032_530, 3253, b"#32530")
            .expect("bind");

        // Now the cancel reaches `send_action`, gets PAST the budget,
        // and fails on the socket instead — there is no server here.
        // `SlotDisabled` is the budget's refusal, and it must not be
        // what comes back.
        let e = x
            .cancel_by_cloid(4096, 3, 3253)
            .expect_err("no server to talk to");
        assert!(
            !matches!(e, DispatchError::SlotDisabled),
            "a cancel must never be refused by the cap: {e:?}"
        );
    }

    /// The queue is idempotent per asset, bounded, and COUNTS what it
    /// cannot hold. A leg nobody swept and nobody counted is exactly
    /// the stranded quote LAW E-8 exists to prevent.
    #[test]
    fn the_sweep_queue_dedupes_and_counts_its_own_overflow() {
        let mut x = exchange();
        let coin = *b"#1234560000000000000";
        x.queue_sweep(77, coin, 7);
        x.queue_sweep(77, coin, 7);
        assert_eq!(x.sweeps_n, 1, "a second roll before the first swept");

        for a in 100..100 + MAX_PENDING_SWEEPS as u32 {
            x.queue_sweep(a, coin, 7);
        }
        assert_eq!(x.sweeps_n, MAX_PENDING_SWEEPS);
        assert!(x.counters.sweep_left > 0, "overflow is COUNTED, not dropped");
    }

    /// "Retry on the next idle" without a bound is a leg that burns the
    /// address budget forever. When the retries are spent the entry
    /// becomes `sweep_left` — the number E6 arms on.
    #[test]
    fn a_sweep_that_keeps_failing_is_given_up_on_and_counted() {
        let mut x = exchange();
        x.queue_sweep(77, *b"#1234560000000000000", 7);
        assert_eq!(x.sweeps_n, 1);

        for _ in 0..u32::from(SWEEP_TRIES) - 1 {
            x.spend_try(0);
            assert_eq!(x.sweeps_n, 1, "still pending while retries remain");
            assert_eq!(x.counters.sweep_left, 0);
        }
        x.spend_try(0);
        assert_eq!(x.sweeps_n, 0, "given up on");
        assert_eq!(x.counters.sweep_left, 1, "and counted, exactly once");
    }

    fn roll(outcome: u32, family: u8, settled: bool, sym: u32) -> ChannelEvent {
        // Built the way `ingress_hyperliquid::family::pack_roll_seq`
        // builds it, from its own source, NOT by calling our unpack in
        // reverse — a test that inverts the thing under test proves
        // only that it is self-consistent.
        let seq = u64::from(outcome)
            | ((60u64 & 0xFFFF) << 32)
            | ((u64::from(family) & 0xFF) << 48)
            | ((settled as u64) << 56);
        ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym,
            seq,
            0,
            1_000_000,
            2_000_000_000,
        )
    }

    /// The layout is DUPLICATED from the ingress (§6.1 forbids the
    /// dependency), so it is held honest here against a `venue_seq`
    /// built the ingress's way.
    #[test]
    fn a_hand_built_roll_seq_unpacks_the_way_the_ingress_packs_it() {
        // The ingress's own pinned vector: pack_roll_seq(2649, 60, 0, false).
        let seq = 2649u64 | (60u64 << 32);
        assert_eq!(unpack_roll(seq), Some((2649, false)));
        // The settled bit is bit 56, and the family byte must not leak
        // into the outcome id.
        let seq = 19_418u64 | (60u64 << 32) | (7u64 << 48) | (1u64 << 56);
        assert_eq!(unpack_roll(seq), Some((19_418, true)));
        // Bits 0..32 are the OUTCOME ID, not `enc`. Confusing them is a
        // silent tenfold error naming a real other market.
        let Some((o, _)) = unpack_roll(u64::from(u32::MAX)) else {
            panic!("a CREATED frame with a full outcome id must unpack");
        };
        assert_eq!(o, u32::MAX);
        // A kind byte no packer of ours writes is REFUSED, not read as
        // either kind — `0x03` used to mask to "settled" here while
        // `strategy_bin15` read it as "created" (E7 review).
        let seq = 2649u64 | (60u64 << 32) | (3u64 << 56);
        assert_eq!(unpack_roll(seq), None, "an unknown roll kind binds nothing");
    }

    /// LAW E-4's writer. One roll binds BOTH legs, and the No leg is
    /// the next ordinal.
    #[test]
    fn a_created_roll_binds_both_legs_of_the_family() {
        let mut x = exchange();
        assert!(x.assets().is_empty());
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        assert_eq!(x.counters().rolls_bound, 1);
        assert_eq!(x.counters().rolls_refused, 0);
        assert_eq!(x.assets().len(), 2, "one roll, two legs");

        // The venue's OWN names, in the FILL namespace.
        assert_eq!(x.assets().sym_of_coin(b"#194180"), Some(4096), "Yes");
        assert_eq!(x.assets().sym_of_coin(b"#194181"), Some(4097), "No is the next ordinal");

        // And the asset ids the venue will accept.
        assert_eq!(x.assets().lookup(4096, 19_418), Ok(100_194_180));
        assert_eq!(x.assets().lookup(4097, 19_418), Ok(100_194_181));
    }

    /// The instance is the OUTCOME ID, and `submit` asks for the one
    /// the order names. This is LAW E-4 actually doing its job: before
    /// the roll handler existed the lookup passed a hardcoded 0, so a
    /// stale asset id could never have been caught.
    #[test]
    fn an_order_naming_a_rolled_instance_is_refused() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        // An order that names the LIVE instance resolves.
        assert_eq!(x.assets().lookup(4096, 19_418), Ok(100_194_180));
        // The quarter rolls.
        x.on_venue_event(&roll(19_419, 0, false, 4096));
        assert_eq!(x.assets().lookup(4096, 19_419), Ok(100_194_190));

        // An order still naming the OLD instance is refused — this is
        // the order that would have gone to someone else's market.
        let mut o = order(4096, ORDER_KIND_MAKER);
        o.client_oid = 19_418;
        assert!(x.submit(&o).is_err());
        assert_eq!(x.counters().submitted, 0);
        assert!(
            matches!(
                x.assets().lookup(4096, core_types::instance_of(o.client_oid)),
                Err(crate::asset::AssetError::StaleInstance { .. })
            ),
            "the refusal must be STALE, not merely unbound"
        );
    }

    /// **The operator ruling, end to end.** A settlement carries no
    /// cloid, so nothing in the fill can say whose position closed.
    /// The slot is learned on the way OUT — from an order the member
    /// actually submitted — and read on the way back IN.
    #[test]
    fn a_settlement_books_against_the_slot_that_traded_the_leg() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        // A leg nothing has traded books NOTHING, however real the
        // settlement is.
        const SETTLE: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"1.0","sz":"2.0","side":"A","time":1757942400000,"oid":88,"tid":5001,"dir":"Settlement","fee":"0.0"}]}}"##;
        assert_eq!(x.route_fills(SETTLE), 0);
        assert_eq!(x.counters().fills_unowned, 1, "no member has traded this leg");
        assert_eq!(x.counters().fills_booked, 0);
        assert_eq!(x.counters().fills_settlement, 1, "still SEEN");

        // Now a member owns the leg.
        // Ownership now comes from an order the VENUE ACCEPTED, which
        // a unit test cannot produce — so it is stated directly. That
        // is the honest shape: the test asserts what happens once a
        // member has traded the leg, not that a refused submit teaches
        // the table (it must not, and `note_owner`'s caller is what
        // holds that).
        assert!(!x.assets_mut().note_owner(4096, 3), "not contested");
        assert_eq!(
            x.assets().owner_of_sym(4096),
            Some(3),
            "the table must learn the owner from the order"
        );

        // A DIFFERENT tid, so the ring does not dedupe it.
        const SETTLE2: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"1.0","sz":"2.0","side":"A","time":1757942400000,"oid":89,"tid":5002,"dir":"Settlement","fee":"0.0"}]}}"##;
        assert_eq!(x.route_fills(SETTLE2), 1, "the settlement must now BOOK");
        assert_eq!(x.counters().fills_booked, 1);
        assert_eq!(x.counters().fills_unowned, 1, "unchanged");
        assert_eq!(x.counters().fills_foreign, 0, "it must NOT go to the tape arm");
    }

    /// The narrowness IS the safety. A cloid-less row that is not a
    /// settlement is an order some other system placed on this
    /// account; attributing it by symbol would book a stranger's trade
    /// against a member, which is what LAW E-9 forbids.
    #[test]
    fn a_cloidless_row_that_is_not_a_settlement_is_still_foreign() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        // Ownership now comes from an order the VENUE ACCEPTED, which
        // a unit test cannot produce — so it is stated directly. That
        // is the honest shape: the test asserts what happens once a
        // member has traded the leg, not that a refused submit teaches
        // the table (it must not, and `note_owner`'s caller is what
        // holds that).
        assert!(!x.assets_mut().note_owner(4096, 3), "not contested");
        assert_eq!(x.assets().owner_of_sym(4096), Some(3), "owner is known");

        // Same leg, same account, no cloid — but `dir` says trade.
        const OTHER: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.7","sz":"2.0","side":"B","time":1757942400000,"oid":90,"tid":5003,"dir":"Buy","fee":"0.0"}]}}"##;
        assert_eq!(x.route_fills(OTHER), 0, "not ours to book");
        assert_eq!(x.counters().fills_foreign, 1);
        assert_eq!(x.counters().fills_booked, 0);
    }

    /// **The check that believes nothing**, driven against a real
    /// `spotClearinghouseState` body.
    #[test]
    fn reconciliation_finds_the_drift_it_exists_to_find() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        // The venue's own shape: USDC plus both legs, BALANCE
        // namespace (`+`), 1e8 quantities.
        const SHEET: &[u8] = br#"{"balances":[{"coin":"USDC","token":0,"total":"997.64","hold":"0.0"},{"coin":"+194180","total":"2.0","hold":"0.0"},{"coin":"+194181","total":"0.0","hold":"0.0"}]}"#;
        let mut bal = [crate::recon::SpotBalance::default(); 8];
        let n = crate::recon::scan_spot_state(SHEET, &mut bal).expect("scans");

        // We booked nothing; the venue says we hold 2 of the Yes leg.
        // That is a LOST FILL, and it is exactly what this check is
        // for.
        let (legs, worst) = HlExchange::<64>::compare(x.assets(), &bal[..n], SHEET);
        assert_eq!(legs, 1, "one leg disagreed");
        assert_eq!(worst, 2_000_000, "2.0 at 1e6");

        // Book the fill the venue already knew about, and they agree.
        const BUY: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(BUY), 1);
        let (legs, worst) = HlExchange::<64>::compare(x.assets(), &bal[..n], SHEET);
        assert_eq!(legs, 0, "the books now agree");
        assert_eq!(worst, 0);

        // A DOUBLE-COUNTED fill is caught in the other direction.
        const BUY2: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":2,"tid":2,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(BUY2), 1);
        let (legs, worst) = HlExchange::<64>::compare(x.assets(), &bal[..n], SHEET);
        assert_eq!(legs, 1, "we now believe twice what the venue holds");
        assert_eq!(worst, 2_000_000);
    }

    /// **An ENCUMBRANCE is not a position change.** `free = total -
    /// hold`, and `hold` is what a resting order has committed — on
    /// spot an ask holds the base token. The ledger is a pure position
    /// from fills. Comparing against `free` reported drift equal to
    /// the resting size for as long as a quote was live, which for a
    /// maker is continuously, and in the direction that looks like a
    /// double-counted fill.
    ///
    /// Every fixture in the first version of this file set
    /// `"hold":"0.0"`, which is why it passed.
    #[test]
    fn a_resting_order_is_not_drift() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        const BUY: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(BUY), 1);

        // The venue holds 2, and 1 of them is committed to a resting
        // ask. The POSITION is still 2.
        const SHEET: &[u8] = br#"{"balances":[{"coin":"USDC","token":0,"total":"997.64","hold":"0.0"},{"coin":"+194180","total":"2.0","hold":"1.0"}]}"#;
        let mut bal = [crate::recon::SpotBalance::default(); 8];
        let n = crate::recon::scan_spot_state(SHEET, &mut bal).expect("scans");
        assert_eq!(bal[1].hold_1e8, 100_000_000, "the fixture must HAVE a hold");
        assert_eq!(bal[1].free_1e8(), 100_000_000, "which free would report as 1");

        let (legs, worst) = HlExchange::<64>::compare(x.assets(), &bal[..n], SHEET);
        assert_eq!(
            (legs, worst),
            (0, 0),
            "a live quote is not a lost fill — this is what `free` got wrong"
        );
    }

    /// A fill from the instance that just rolled off still books into
    /// the lane and the tape — that is what the one-generation memory
    /// is for — but it must NOT credit the successor's ledger, which
    /// `bind` just zeroed and whose venue balance will never contain
    /// it. Left uncorrected it is permanent drift, and
    /// `recon_drift_max_qty_1e6` is a high-water mark that never
    /// clears.
    #[test]
    fn a_late_fill_from_the_old_instance_books_but_does_not_credit_the_successor() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        // The quarter rolls. `#194180` is now the PREVIOUS name.
        x.on_venue_event(&roll(19_419, 0, false, 4096));
        assert_eq!(x.assets().booked_qty(4096), Some(0), "a new instance starts flat");

        // A fill for the leg that just ended, arriving late.
        const LATE: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(LATE), 1, "it must still reach the lane");
        assert_eq!(x.counters().fills_booked, 1);
        assert_eq!(
            x.assets().booked_qty(4096),
            Some(0),
            "and must NOT appear in the successor's position"
        );

        // A fill for the CURRENT leg does credit it.
        const NOW: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194190","px":"0.68","sz":"3.0","side":"B","time":1757942400000,"oid":2,"tid":2,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(NOW), 1);
        assert_eq!(x.assets().booked_qty(4096), Some(3_000_000));
    }

    /// **E7-F3.** A disagreement reaches the halt's high-water mark
    /// only when two consecutive reconciliations see one. The first
    /// sighting is a level (the operator sees it), not a mark; the
    /// second confirms the smaller of the two; a sighting that clears
    /// before the next cycle leaves nothing. Mainnet 2026-09-19
    /// 15:32:22Z is the case: 2 contracts read against 0 booked in the
    /// same second as the fill, latched `recon-drift` over a fill
    /// booked milliseconds later.
    #[test]
    fn a_drift_must_survive_a_reconciliation_interval_before_it_can_halt() {
        let mut x = exchange();
        x.note_drift(2_000_000);
        assert_eq!(x.counters().recon_drift_max_qty_1e6, 0, "one sighting is not drift");
        assert_eq!(x.halt_signal().recon_drift_usd_1e6, 0, "and halts nothing");
        x.note_drift(0);
        assert_eq!(x.counters().recon_drift_max_qty_1e6, 0, "cleared: it was the race");

        x.note_drift(3_000_000);
        x.note_drift(2_000_000);
        assert_eq!(
            x.counters().recon_drift_max_qty_1e6,
            2_000_000,
            "two consecutive sightings confirm the smaller"
        );
        assert_eq!(x.halt_signal().recon_drift_usd_1e6, 2_000_000, "and the halt reads it");
        x.note_drift(0);
        x.note_drift(5_000_000);
        assert_eq!(x.counters().recon_drift_max_qty_1e6, 2_000_000, "a high-water mark never clears");
        x.note_drift(5_000_000);
        assert_eq!(x.counters().recon_drift_max_qty_1e6, 5_000_000);
    }

    /// A leg the venue does not mention reads as ZERO — and that is a
    /// drift, not a pass, if we booked something.
    #[test]
    fn a_leg_the_venue_does_not_mention_is_a_drift() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        const BUY: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(BUY), 1);

        // A sheet with no outcome legs at all.
        const EMPTY: &[u8] = br#"{"balances":[{"coin":"USDC","token":0,"total":"997.64","hold":"0.0"}]}"#;
        let mut bal = [crate::recon::SpotBalance::default(); 8];
        let n = crate::recon::scan_spot_state(EMPTY, &mut bal).expect("scans");
        let (legs, worst) = HlExchange::<64>::compare(x.assets(), &bal[..n], EMPTY);
        assert_eq!(legs, 1, "silence is not agreement");
        assert_eq!(worst, 2_000_000);
    }

    /// The reconciler's own side of the comparison is fed from fills
    /// that ENTERED THE LANE, and from nothing else — that is what
    /// makes the check independent of what any member believes.
    #[test]
    fn the_ledger_follows_booked_fills_and_resets_on_a_roll() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        assert_eq!(x.assets().booked_qty(4096), Some(0));

        // A BUY of 2 that books.
        const BUY: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.68","sz":"2.0","side":"B","time":1757942400000,"oid":1,"tid":1,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(BUY), 1);
        assert_eq!(x.assets().booked_qty(4096), Some(2_000_000), "+2 at 1e6");

        // A SELL of 1 that books — direction comes from the venue row.
        const SELL: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.70","sz":"1.0","side":"A","time":1757942400000,"oid":2,"tid":2,"cloid":"0x4d560300000000000000000000000001"}]}}"##;
        assert_eq!(x.route_fills(SELL), 1);
        assert_eq!(x.assets().booked_qty(4096), Some(1_000_000), "+2 -1");

        // A fill that does NOT book moves nothing. This one is
        // foreign: same leg, no cloid, not a settlement.
        const FOREIGN: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"0.70","sz":"9.0","side":"B","time":1757942400000,"oid":3,"tid":3,"dir":"Buy"}]}}"##;
        assert_eq!(x.route_fills(FOREIGN), 0);
        assert_eq!(
            x.assets().booked_qty(4096),
            Some(1_000_000),
            "a fill we did not book is not a position we hold"
        );

        // The quarter rolls: a NEW instance is a NEW position.
        x.on_venue_event(&roll(19_419, 0, false, 4096));
        assert_eq!(
            x.assets().booked_qty(4096),
            Some(0),
            "carrying the old quantity forward would have the reconciler \
             comparing a settled position against a fresh balance forever"
        );
    }

    /// **The sweep must not cancel its whole selection in one
    /// call.** E6 commit 3a put `on_idle` on the engine thread.
    #[test]
    fn the_sweep_budget_bounds_one_call_and_defers_the_rest() {
        // `ours_on_leg` can select up to `MAX_OPEN_ORDERS` (256) and
        // every cancel is an HTTPS round trip bounded by
        // `REQ_DEADLINE` (5 s). E6 commit 3a put `on_idle` on the
        // ENGINE THREAD, so an unbudgeted loop here is up to ~21
        // minutes of stall in one call — no ring drained, no
        // `shutdown_requested()`, and E6's halt machine unable to run
        // on the thread a dead venue is blocking.
        assert_eq!(sweep_plan(0), (0, 0));
        assert_eq!(sweep_plan(1), (1, 0));
        assert_eq!(
            sweep_plan(SWEEP_CANCELS_PER_IDLE),
            (SWEEP_CANCELS_PER_IDLE, 0),
            "a selection exactly at the budget is not deferred"
        );
        assert_eq!(
            sweep_plan(SWEEP_CANCELS_PER_IDLE + 1),
            (SWEEP_CANCELS_PER_IDLE, 1)
        );
        assert_eq!(
            sweep_plan(crate::recon::MAX_OPEN_ORDERS),
            (
                SWEEP_CANCELS_PER_IDLE,
                (crate::recon::MAX_OPEN_ORDERS - SWEEP_CANCELS_PER_IDLE) as u32
            ),
            "the worst case the venue can hand us is still one budget"
        );
        // Nothing is lost: send + defer is always the whole selection.
        let mut k = 0usize;
        while k <= crate::recon::MAX_OPEN_ORDERS {
            let (now, deferred) = sweep_plan(k);
            assert_eq!(now + deferred as usize, k, "the sweep dropped {k}");
            assert!(now <= SWEEP_CANCELS_PER_IDLE);
            k += 1;
        }
    }

    #[test]
    fn budget_exhaustion_is_not_a_failure_and_does_not_burn_a_retry() {
        // `SWEEP_TRIES` is 8. Folding deferral into the failure path
        // spends one per idle moment, so a 256-order sweep at 8 a call
        // would be abandoned after 64 with the rest reported as
        // `sweep_left` — quotes left resting on a retired instance,
        // which is exactly what LAW E-8 exists to prevent.
        assert_eq!(classify_sweep(0, 0, false), SweepOutcome::Done);
        assert_eq!(classify_sweep(0, 248, false), SweepOutcome::Deferred);
        assert_eq!(classify_sweep(1, 0, false), SweepOutcome::Retry);
        // A FAILURE wins over a deferral: the call has something to
        // retry, and reporting it as "went to plan" would spend no
        // retry on an order that really did not cancel.
        assert_eq!(classify_sweep(1, 248, false), SweepOutcome::Retry);
        // **TRUNCATION IS MORE WORK, NOT FAILURE.** The budget makes
        // the selection sit AT the ceiling for `(N - 256) / 8` calls,
        // so `Retry` here spends one of 8 tries every one of them and
        // a leg above ~312 orders is abandoned with the rest reported
        // as `sweep_left`. Still never `Done` — we could not see the
        // end of the selection — but deferral is what continues it.
        assert_eq!(classify_sweep(0, 0, true), SweepOutcome::Deferred);
        assert_eq!(classify_sweep(0, 248, true), SweepOutcome::Deferred);
    }

    /// One sweep entry driven to completion the way
    /// `sweep_one_pending` drives it, for a leg carrying `total`
    /// orders of ours. Returns `(idle moments, retries spent,
    /// stalls)`, or `None` if the entry was abandoned with orders
    /// still resting.
    ///
    /// **`truncated` is computed the way production computes it** —
    /// `k == oids.len()`, i.e. the selection filled the buffer — and
    /// `selected` is capped at `MAX_OPEN_ORDERS` because that is what
    /// the buffer can hold. An earlier version of this passed
    /// `truncated = false` unconditionally, including on the first
    /// call where production computes `true`, which is exactly why it
    /// could not see that truncation was burning a retry per call.
    fn run_sweep_to_completion(total: usize, shrinks: bool) -> Option<(usize, u8, u64)> {
        let mut left = total;
        let mut tries = SWEEP_TRIES;
        let mut defers: u16 = 0;
        let mut stalls = 0u64;
        let mut idles = 0usize;
        while left > 0 {
            let selected = left.min(crate::recon::MAX_OPEN_ORDERS);
            let truncated = selected == crate::recon::MAX_OPEN_ORDERS;
            let (now, deferred) = sweep_plan(selected);
            assert!(now > 0, "no progress with {left} left");
            idles += 1;
            assert!(idles < 100_000, "runaway");
            match classify_sweep(0, deferred, truncated) {
                SweepOutcome::Done => {}
                SweepOutcome::Deferred => {
                    if defers >= SWEEP_MAX_DEFERS {
                        stalls += 1;
                        defers = 0;
                        tries = tries.checked_sub(1)?;
                    } else {
                        defers = defers.saturating_add(1);
                    }
                }
                SweepOutcome::Retry => {
                    tries = tries.checked_sub(1)?;
                }
            }
            if shrinks {
                left -= now;
            }
        }
        Some((idles, tries, stalls))
    }

    #[test]
    fn a_full_selection_finishes_within_the_retries_it_has() {
        // A 256-order leg: exactly one buffer, truncated on the first
        // call and shrinking from there.
        let (idles, tries, stalls) =
            run_sweep_to_completion(crate::recon::MAX_OPEN_ORDERS, true)
                .expect("a 256-order sweep must not be abandoned");
        assert_eq!(idles, 32, "256 orders at 8 a call");
        assert_eq!(tries, SWEEP_TRIES, "deferral spent no retries");
        assert_eq!(stalls, 0);
    }

    /// **The regression the budget introduced, and the reason
    /// `truncated` had to stop meaning failure.**
    ///
    /// Commit 2's boot rule allows 64 open orders on each of 8 slots,
    /// so a leg of 512 is a valid configuration. With truncation
    /// classed as `Retry`, the selection sits at the 256 ceiling for
    /// `(512 - 256) / 8 = 32` calls, each spending one of 8 tries —
    /// abandoned long before the end, with the remainder reported as
    /// `sweep_left`: quotes resting on a retired instance.
    #[test]
    fn a_leg_larger_than_one_buffer_is_not_abandoned() {
        for total in [312usize, 313, 512, 1024] {
            let (idles, tries, stalls) = run_sweep_to_completion(total, true)
                .unwrap_or_else(|| panic!("a {total}-order sweep was abandoned"));
            assert_eq!(idles, total.div_ceil(SWEEP_CANCELS_PER_IDLE));
            assert_eq!(tries, SWEEP_TRIES, "{total}: deferral spent no retries");
            assert_eq!(stalls, 0, "{total}: nothing stalled");
        }
    }

    /// **The invariant deferral rests on, and what happens without
    /// it.**
    ///
    /// Deferral spends no retry, which is safe only while the venue's
    /// `frontendOpenOrders` view shrinks by what we cancelled. If it
    /// ever does not — a venue that keeps listing a cancelled order
    /// and accepts the re-cancel — an unguarded entry would issue 8
    /// HTTPS round trips every 2 ms for the life of the boot, on the
    /// engine thread. `SWEEP_MAX_DEFERS` makes it terminate anyway,
    /// and is sized so a sweep that is merely long is never mistaken
    /// for one that is stuck
    /// (`a_leg_larger_than_one_buffer_is_not_abandoned`).
    #[test]
    fn a_selection_that_never_shrinks_terminates_instead_of_spinning() {
        assert!(
            run_sweep_to_completion(crate::recon::MAX_OPEN_ORDERS, false).is_none(),
            "a non-shrinking selection must be abandoned, not looped on for ever"
        );
    }

    /// **A submit the venue never took must teach the table nothing.**
    /// Ownership is what lets a cloid-less settlement be booked, so a
    /// leg we merely TRIED to trade must not absorb one. The first
    /// version of this recorded ownership at intent, right after the
    /// asset lookup and before every other refusal.
    #[test]
    fn a_refused_submit_leaves_the_leg_unowned() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        // Refused by the BUDGET — after the asset lookup succeeded,
        // which is exactly where the old `note_owner` sat.
        assert!(x.budget_remaining() <= 0, "a cold budget refuses");
        let mut o = order(4096, ORDER_KIND_MAKER);
        o.client_oid = 19_418;
        o.strategy_id = 3;
        assert_eq!(x.submit(&o).unwrap_err(), DispatchError::SlotDisabled);
        assert_eq!(
            x.assets().owner_of_sym(4096),
            None,
            "a submit the venue never saw must not claim the leg"
        );

        // Refused for an UNBOUND symbol — before the lookup.
        let mut o2 = order(9_999, ORDER_KIND_MAKER);
        o2.strategy_id = 3;
        assert!(x.submit(&o2).is_err());
        assert_eq!(x.assets().owner_of_sym(9_999), None);

        // And so its settlement books nothing.
        const SETTLE: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"1.0","sz":"2.0","side":"A","time":1757942400000,"oid":88,"tid":8001,"dir":"Settlement","fee":"0.0"}]}}"##;
        assert_eq!(x.route_fills(SETTLE), 0);
        assert_eq!(x.counters().fills_unowned, 1);
    }

    /// Two members on one symbol is a configuration error, and the
    /// table refuses to pick between them — the same rule
    /// `sym_of_coin` applies to an ambiguous coin.
    #[test]
    fn a_leg_two_members_trade_stops_naming_an_owner() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        assert!(!x.assets_mut().note_owner(4096, 3));
        assert_eq!(x.assets().owner_of_sym(4096), Some(3));

        // A second member trades the same leg.
        assert!(x.assets_mut().note_owner(4096, 5), "must report the contest");
        assert_eq!(
            x.assets().owner_of_sym(4096),
            None,
            "a contested leg must name NO owner rather than the last writer"
        );
        // And it stays contested — the first member trading again does
        // not win it back.
        assert!(!x.assets_mut().note_owner(4096, 3));
        assert_eq!(x.assets().owner_of_sym(4096), None);

        // So its settlements are counted, never booked.
        const SETTLE: &[u8] = br##"{"channel":"userFills","data":{"fills":[{"coin":"#194180","px":"1.0","sz":"2.0","side":"A","time":1757942400000,"oid":88,"tid":7001,"dir":"Settlement","fee":"0.0"}]}}"##;
        assert_eq!(x.route_fills(SETTLE), 0);
        assert_eq!(x.counters().fills_unowned, 1);
        assert_eq!(x.counters().fills_booked, 0);
    }

    /// The owner survives the quarter-hour roll — the member trading
    /// the Yes leg of one instance is the member trading the Yes leg
    /// of its successor.
    #[test]
    fn the_owner_survives_a_roll() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        // Ownership now comes from an order the VENUE ACCEPTED, which
        // a unit test cannot produce — so it is stated directly. That
        // is the honest shape: the test asserts what happens once a
        // member has traded the leg, not that a refused submit teaches
        // the table (it must not, and `note_owner`'s caller is what
        // holds that).
        assert!(!x.assets_mut().note_owner(4096, 3), "not contested");
        assert_eq!(x.assets().owner_of_sym(4096), Some(3));

        x.on_venue_event(&roll(19_419, 0, false, 4096));
        assert_eq!(
            x.assets().owner_of_sym(4096),
            Some(3),
            "a roll must not make the successor's settlement unattributable"
        );
    }

    /// A settled roll keeps the binding. Dropping it would throw away
    /// the one-generation memory a fill in flight depends on.
    #[test]
    fn a_settled_roll_keeps_the_binding_it_was_told_about() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, 4096));
        x.on_venue_event(&roll(19_418, 0, true, 4096));

        assert_eq!(x.counters().rolls_settled, 1);
        assert_eq!(x.counters().rolls_bound, 1, "a settlement binds nothing");
        assert_eq!(
            x.assets().sym_of_coin(b"#194180"),
            Some(4096),
            "a fill still in flight across the settlement must resolve"
        );
    }

    /// A roll binds both legs or neither. A table with room for one
    /// must leave the Yes leg unbound rather than half-bind the
    /// family — and `rolls_refused` must then be telling the truth.
    #[test]
    fn a_roll_that_cannot_fit_both_legs_binds_neither() {
        let mut x = exchange();
        // Fill the table to one free slot.
        for i in 0..(crate::asset::ASSET_SLOTS as u32 - 1) {
            x.assets_mut()
                .bind(i + 100, 100_000_000 + i, 1, b"#0")
                .expect("bind");
        }
        let before = x.assets().len();
        x.on_venue_event(&roll(19_418, 0, false, 4096));

        assert_eq!(x.counters().rolls_refused, 1);
        assert_eq!(x.counters().rolls_bound, 0);
        assert_eq!(x.assets().len(), before, "a refused roll must bind NEITHER leg");
        assert_eq!(x.assets().sym_of_coin(b"#194180"), None, "not even the Yes leg");
    }

    /// `sym + 1` on a wire-derived symbol. `SYMBOL_ID_NONE` is
    /// `u32::MAX` and the ordinal field is 24 bits, so an unchecked
    /// add could carry into the VENUE byte and bind a leg in another
    /// venue's namespace — silently, because release disables overflow
    /// checks. This is the same defect class `asset_id` was hardened
    /// against, so it gets the same runtime refusal.
    #[test]
    fn a_symbol_that_cannot_take_a_no_leg_is_refused() {
        let mut x = exchange();
        x.on_venue_event(&roll(19_418, 0, false, core_types::SYMBOL_ID_NONE));
        assert_eq!(x.counters().rolls_refused, 1);
        assert!(x.assets().is_empty(), "SYMBOL_ID_NONE must bind nothing");

        // The top of the ordinal field: +1 would carry into the venue
        // byte and name another venue's symbol.
        let top = core_types::make_symbol_id(VenueId::Hyperliquid, core_types::SYMBOL_ORDINAL_MASK);
        x.on_venue_event(&roll(19_418, 0, false, top));
        assert_eq!(x.counters().rolls_refused, 2);
        assert!(x.assets().is_empty());

        // One below the top still binds — the guard refuses the wrap,
        // not the range.
        let ok = core_types::make_symbol_id(
            VenueId::Hyperliquid,
            core_types::SYMBOL_ORDINAL_MASK - 1,
        );
        x.on_venue_event(&roll(19_418, 0, false, ok));
        assert_eq!(x.counters().rolls_bound, 1);
    }

    #[test]
    fn a_roll_naming_no_outcome_binds_nothing() {
        let mut x = exchange();
        x.on_venue_event(&roll(0, 0, false, 4096));
        assert_eq!(x.counters().rolls_refused, 1);
        assert!(x.assets().is_empty(), "slot 0 of somebody's market");
        // Another venue's event on the same channel is not ours.
        let mut e = roll(19_418, 0, false, 4096);
        e.venue = VenueId::Binance as u8;
        x.on_venue_event(&e);
        assert!(x.assets().is_empty());
        // Nor is another channel.
        let mut e = roll(19_418, 0, false, 4096);
        e.channel = ChannelId::Mark as u8;
        x.on_venue_event(&e);
        assert!(x.assets().is_empty());
    }

    #[test]
    fn a_coin_no_roll_bound_is_never_booked_against_a_guess() {
        let mut x = exchange();
        // One leg bound; everything else must resolve to nothing.
        x.assets_mut()
            .bind(42, 100_032_530, 7, b"#32530")
            .expect("bind");
        for coin in [&b"BTC"[..], b"#3253", b"USDC", b"", b"\xff\xfe", b"#32531"] {
            assert_eq!(
                x.assets().sym_of_coin(coin),
                None,
                "a coin name resolved to a symbol no roll bound"
            );
        }
        assert_eq!(x.assets().sym_of_coin(b"#32530"), Some(42));
    }

    /// End to end: a fill whose coin IS bound reaches fill lane 3.
    /// Until this passed, `fills_booked` was structurally zero and the
    /// whole live fill path was source code nothing exercised.
    #[test]
    fn a_fill_whose_coin_is_bound_reaches_the_lane() {
        let mut x = exchange();
        x.assets_mut()
            .bind(42, 100_032_530, 7, b"#32530")
            .expect("bind");
        let frame = br##"{"channel":"userFills","data":{"fills":[{"coin":"#32530","px":"0.47","sz":"25","side":"B","time":1757942400000,"oid":77,"tid":9001,"cloid":"0x4d560300000000000000000012345678"}]}}"##;
        assert_eq!(x.route_fills(frame), 1, "the fill did not reach the lane");
        assert_eq!(x.counters().fills_booked, 1);
        assert_eq!(x.counters().fills_unresolved, 0);
        // Replaying the same tid books nothing more (LAW E-5).
        assert_eq!(x.route_fills(frame), 0, "a replayed tid double-booked");
        assert_eq!(x.counters().fills_booked, 1);
    }

    /// The LAW E-9 cloid of `(slot, client_oid)` as the action JSON
    /// renders it: magic `MV`, the slot byte, five reserved zeros,
    /// then the big-endian client id.
    fn cloid_json(slot: u8, client_oid: u64) -> String {
        format!("\"0x4d56{slot:02x}0000000000{client_oid:016x}\"")
    }

    /// **BX0-F3 — the trait's lifecycle verbs reach this arm.** The
    /// router drives every live verb through `OrderDispatch`; until
    /// BX0 this impl overrode `submit` only, so a live cancel or
    /// modify fell to the trait default, `Unsupported`, and never
    /// reached the venue. An unbound symbol makes the arm answer with
    /// its OWN refusal (LAW E-4) before anything is signed or sent, so
    /// no venue is needed. Break-and-watch: delete either override and
    /// its assert reads `Unsupported` with the arm's counters untouched.
    #[test]
    fn the_trait_cancel_and_modify_reach_the_cloid_verbs() {
        let mut x = exchange_at("127.0.0.1");
        let resting = order(42, ORDER_KIND_MAKER);
        assert_eq!(
            OrderDispatch::cancel(&mut x, &CancelReq::of(&resting, 2)),
            Err(DispatchError::NoLiveRoute),
            "the trait cancel never reached cancel_by_cloid"
        );
        assert_eq!(x.counters().cancels_refused, 1);

        let mut repl = order(42, ORDER_KIND_MAKER);
        repl.client_oid = (1 << 32) | 7;
        assert_eq!(
            OrderDispatch::modify(&mut x, &ModifyReq::new(resting.client_oid, repl)),
            Err(DispatchError::NoLiveRoute),
            "the trait modify never reached modify_by_cloid"
        );
        assert_eq!(x.counters().modifies_refused, 1);
        assert_eq!(x.counters().refused_local, 2, "one LAW E-4 refusal per verb");
        assert_eq!(x.counters().cancels_sent + x.counters().modifies_sent, 0);
    }

    /// **BX0-F3 — the trait modify forwards the two ids the right way
    /// round.** A transposed pair would compile, address the
    /// REPLACEMENT's id as the resting order and fail at the venue as
    /// "no such order" — or worse, replace some other quote. With the
    /// leg bound and the cold budget at its floor, the requote encodes
    /// in full and stops at the submit barrier (never a socket — the
    /// bench gate 60 posture), leaving the rendered action in `req`:
    /// the resting order must be the `oid` and the replacement the
    /// order's own `c`.
    #[test]
    fn the_trait_modify_names_the_resting_order_and_carries_the_replacement() {
        let mut x = exchange_at("127.0.0.1");
        x.assets_mut().bind(42, 3, 7, b"#42").expect("bind");
        let prev: u64 = (1 << 32) | 7;
        let mut repl = order(42, ORDER_KIND_MAKER);
        repl.client_oid = (2 << 32) | 7;
        assert_eq!(
            OrderDispatch::modify(&mut x, &ModifyReq::new(prev, repl)),
            Err(DispatchError::SlotDisabled),
            "a cold budget refuses a requote at the submit barrier"
        );
        assert_eq!(x.counters().modifies_refused, 1);
        assert_eq!(x.counters().encode_failures, 0, "the encode half ran to the end");
        let body = String::from_utf8_lossy(&x.req);
        let resting = format!("\"oid\":{}", cloid_json(3, prev));
        let replacement = format!("\"c\":{}", cloid_json(3, repl.client_oid));
        assert!(body.contains(&resting), "resting order not addressed by its cloid");
        assert!(body.contains(&replacement), "replacement does not carry its own cloid");
        // The stamping `ModifyReq::new` does (verb, prev id) must not
        // reach the wire: the action is byte-for-byte the one an
        // unstamped replacement encodes.
        let (_mp, end) = x.stage_modify(prev, &repl).expect("unstamped encode");
        let unstamped = x.req[..end].to_vec();
        let _ = OrderDispatch::modify(&mut x, &ModifyReq::new(prev, repl));
        assert_eq!(&x.req[..end], &unstamped[..], "the stamp leaked into the action");
    }

    /// **BX0-F3 — the trait cancel forwards the three fields that name
    /// the order.** A bound leg takes the cancel through the encode to
    /// the socket (a cancel is an exit: no budget bars it); nothing
    /// listens on loopback:443, so the send fails — but the action it
    /// rendered names this order's asset and cloid, not a neighbour's.
    #[test]
    fn the_trait_cancel_names_the_asset_and_the_cloid_it_was_given() {
        let mut x = exchange_at("127.0.0.1");
        x.assets_mut().bind(42, 3, 7, b"#42").expect("bind");
        let resting = order(42, ORDER_KIND_MAKER);
        let r = OrderDispatch::cancel(&mut x, &CancelReq::of(&resting, 2));
        assert!(
            !matches!(r, Err(DispatchError::Unsupported | DispatchError::SlotDisabled | DispatchError::NoLiveRoute)),
            "the cancel stopped before the socket: {r:?}"
        );
        let body = String::from_utf8_lossy(&x.req);
        let named = format!("\"asset\":3,\"cloid\":{}", cloid_json(3, resting.client_oid));
        assert!(body.contains(&named), "the cancel did not name this order: {named}");
    }
}
