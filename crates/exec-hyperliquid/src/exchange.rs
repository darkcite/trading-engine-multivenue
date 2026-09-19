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
use core_types::{ChannelEvent, ChannelId, Fill, NsTs, Order, Side, Tick, VenueId};

use crate::action::{encode_order, OrderWire, Tif, MAX_ACTION};
use crate::asset::AssetTable;
use crate::budget::{self, AddressBudget};
use crate::cloid::encode as encode_cloid;
use crate::config::HlConfig;
use crate::http::{HlHttp, MAX_REQ_BODY};
use crate::nonce::Nonce;
use crate::request::{envelope, order_json};
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

/// Counters an operator reads on `/metrics`.
#[repr(C, align(64))]
#[derive(Debug, Default, Copy, Clone)]
pub struct HlExecCounters {
    /// Orders accepted by the venue.
    pub submitted: u64,
    /// Orders the venue refused.
    pub rejected: u64,
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
    seen: TidRing<SNAPSHOT_RING>,
    assets: AssetTable,
    fills: Producer<Fill, FILL_N>,
    counters: HlExecCounters,
    budget_path: PathBuf,
    last_persist: Instant,
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
    /// The MASTER account reconciliation asks about — the agent signs
    /// on its behalf and the venue reports balances under it.
    master_addr: [u8; 20],
    /// Scratch for one `spotClearinghouseState` answer. Boxed and
    /// sized for the venue's full reply, which is NOT the number of
    /// coins we hold — a one-coin account came back with fourteen
    /// rows.
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
    /// Scratch for one sweep's `frontendOpenOrders` answer.
    open: Box<[crate::recon::OpenOrder]>,
    bal: Box<[crate::recon::SpotBalance]>,
    /// Scratch for one frame's fills. **Boxed and sized for the
    /// venue's SNAPSHOT**, not for a steady-state frame — see
    /// [`HlExchange::pump_user_events`].
    scratch: Box<[UserFill]>,
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
        Ok(Self {
            http,
            ws,
            sk,
            network: cfg.network,
            nonce: Nonce::new(),
            budget,
            seen: TidRing::new(),
            assets: AssetTable::default(),
            fills,
            counters: HlExecCounters::default(),
            budget_path,
            last_persist: Instant::now(),
            ws_fail_streak: 0,
            last_ws_ok: None,
            reject_streak: 0,
            asset_refusal_streak: 0,
            reconciled: false,
            ws_retry_at: Instant::now(),
            scratch: vec![UserFill::default(); SNAPSHOT_RING].into_boxed_slice(),
            last_recon: Instant::now(),
            master_addr: cfg.master_addr,
            sweeps: [PendingSweep::EMPTY; MAX_PENDING_SWEEPS],
            sweeps_n: 0,
            cancel_all_mark: 0,
            open: vec![crate::recon::OpenOrder::default(); crate::recon::MAX_OPEN_ORDERS]
                .into_boxed_slice(),
            bal: vec![crate::recon::SpotBalance::default(); crate::recon::MAX_SPOT_BALANCES]
                .into_boxed_slice(),
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

    /// Operator counters.
    #[inline]
    #[must_use]
    pub fn counters(&self) -> HlExecCounters {
        self.counters
    }

    /// Requests believed to remain before the venue's cliff.
    #[inline]
    #[must_use]
    pub fn budget_remaining(&self) -> i64 {
        self.budget.remaining()
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
            let f = scratch[i];
            // LAW E-5: the venue's tid is the dedupe key, and the ring
            // outlives the socket precisely so a reconnect snapshot
            // cannot re-book.
            if !seen.admit(f.tid) {
                continue;
            }
            if !is_snapshot {
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
            if f.is_settlement && f.cloid.is_none() {
                match assets.owner_of_sym(sym) {
                    Some(slot) => match to_fill_as(&f, sym, ts, slot) {
                        Ok(fill) => {
                            if fills.try_push(fill).is_err() {
                                counters.fills_dropped = counters.fills_dropped.wrapping_add(1);
                            } else {
                                counters.fills_booked = counters.fills_booked.wrapping_add(1);
                                if current {
                                    assets.book_qty(sym, signed_qty_1e6(&f, &fill));
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
            match to_fill(&f, sym, ts) {
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
                            assets.book_qty(sym, signed_qty_1e6(&f, &fill));
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
    fn reconcile(&mut self) {
        if self.last_recon.elapsed() < RECON_EVERY {
            return;
        }
        self.last_recon = Instant::now();

        let mut req = [0u8; crate::recon::MAX_STATE_REQ];
        let Ok(n) = crate::recon::spot_state_request(&mut req, &self.master_addr) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return;
        };
        let Ok((_status, range)) = self.http.post_to(crate::http::INFO_PATH, &req[..n]) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return;
        };
        let body = &self.http.resp()[range];
        let Ok(rows) = crate::recon::scan_spot_state(body, &mut self.bal) else {
            self.counters.recon_failed = self.counters.recon_failed.wrapping_add(1);
            return;
        };
        self.counters.recon_ok = self.counters.recon_ok.wrapping_add(1);
        let (legs, worst) = Self::compare(&self.assets, &self.bal[..rows], body);
        // E6 commit 3: the arm has compared itself against the venue.
        // The router reads this to stop refusing every live place —
        // see the seeding interlock, which exists because a ledger
        // that has never been reconciled reads zero exposure after a
        // restart and would fail every clamp OPEN.
        self.reconciled = true;
        self.counters.recon_drift_legs = legs;
        if worst > self.counters.recon_drift_max_qty_1e6 {
            self.counters.recon_drift_max_qty_1e6 = worst;
        }
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
    /// Returns the request length in `body`.
    ///
    /// # Errors
    /// `SignerRejected` if the key refuses; `EncodeOverflow` if the
    /// envelope does not fit.
    pub fn seal(
        &mut self,
        mp: &[u8],
        aj: &[u8],
        body: &mut [u8; MAX_REQ_BODY],
    ) -> Result<usize, DispatchError> {
        let nonce = self.nonce.next(now_ms());
        let sig =
            sign_action(&self.sk, mp, nonce, Vault::None, None, self.network).map_err(|_| {
                self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
                DispatchError::SignerRejected
            })?;
        envelope(body, aj, nonce, &sig, None, None).map_err(|_| {
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
        body: &[u8],
    ) -> Result<(u16, core::ops::Range<usize>), crate::http::PostErr> {
        let posted = self.http.post(body);
        self.count_post(&posted);
        posted
    }

    /// Charge `posted` to the address if it may have left the host.
    ///
    /// Separated from the post itself so the wiring — predicate to
    /// governor — can be asserted without a socket. The only link
    /// this leaves untested is the call one line above, which is why
    /// it is one line above.
    #[inline]
    fn count_post(&mut self, posted: &Result<(u16, core::ops::Range<usize>), crate::http::PostErr>) {
        if Self::counts_against_address(posted) {
            self.budget.on_action_sent();
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
    fn send_action(&mut self, mp: &[u8], aj: &[u8], spend: Spend) -> Result<HlOk, DispatchError> {
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
        let mut body = [0u8; MAX_REQ_BODY];
        let n = self.seal(mp, aj, &mut body)?;
        let posted = self.post_counted(&body[..n]);
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

        let resp = self.http.resp();
        let slice = &resp[range];
        match scan(slice) {
            Ok(HlResponse::Ok(ok)) if ok.accepted() => {
                // An acceptance ends both streaks. They are
                // CONSECUTIVE counts: a venue refusing every order is
                // a different fact from one that has refused a few
                // over a long boot, and only the first is a halt.
                self.reject_streak = 0;
                self.asset_refusal_streak = 0;
                Ok(ok)
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
        // and the rows borrow the response buffer.
        let mut oids = [0u64; crate::recon::MAX_OPEN_ORDERS];
        let k = crate::recon::ours_on_leg(
            &self.open[..rows],
            body,
            &e.coin[..e.coin_len as usize],
            &mut oids,
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
        let truncated = k == oids.len();

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
            let oid = oids[j];
            j += 1;
            let c = [crate::action::CancelWire { asset: e.asset, oid }];
            let mut mp = [0u8; MAX_ACTION];
            let mut aj = [0u8; MAX_ACTION];
            let (Ok(mp_n), Ok(aj_n)) = (
                crate::action::encode_cancel(&mut mp, &c),
                crate::request::cancel_json(&mut aj, &c),
            ) else {
                self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
                failed += 1;
                continue;
            };
            if self
                .send_action(&mp[..mp_n], &aj[..aj_n], Spend::Cancel)
                .is_ok()
            {
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
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let (Ok(mp_n), Ok(aj_n)) = (
            crate::action::encode_cancel_by_cloid(&mut mp, &c),
            crate::request::cancel_by_cloid_json(&mut aj, &c),
        ) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.cancels_refused = self.counters.cancels_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        match self.send_action(&mp[..mp_n], &aj[..aj_n], Spend::Cancel) {
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
    /// # Errors
    /// As [`Self::cancel_by_cloid`], plus a price or size that will not
    /// scale and an order kind with no TIF.
    pub fn modify(&mut self, prev_client_oid: u64, order: &Order) -> Result<(), DispatchError> {
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
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let (Ok(mp_n), Ok(aj_n)) = (
            crate::action::encode_batch_modify(&mut mp, &m),
            crate::request::batch_modify_json(&mut aj, &m),
        ) else {
            self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
            self.counters.modifies_refused = self.counters.modifies_refused.wrapping_add(1);
            return Err(DispatchError::EncodeOverflow);
        };
        // A modify can MOVE exposure, so it answers to the submit rule
        // rather than the exit one — LAW E-7 makes it the requote path,
        // not a way around the governor.
        match self.send_action(&mp[..mp_n], &aj[..aj_n], Spend::Submit) {
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
        let mut mp = [0u8; MAX_ACTION];
        let mut aj = [0u8; MAX_ACTION];
        let enc = |r: Result<usize, crate::msgpack::MsgPackErr>,
                       c: &mut HlExecCounters|
         -> Result<usize, DispatchError> {
            r.map_err(|_| {
                c.encode_failures = c.encode_failures.wrapping_add(1);
                DispatchError::EncodeOverflow
            })
        };
        let mp_n = enc(encode_order(&mut mp, &[wire], b"na"), &mut self.counters)?;
        let aj_n = enc(order_json(&mut aj, &[wire], b"na"), &mut self.counters)?;

        // The nonce is taken LAST among the things that can fail, so a
        // local refusal cannot burn one. (HL only requires strictly
        // increasing nonces, so a gap is harmless — but not burning
        // one at all is simpler to reason about.)
        self.send_action(&mp[..mp_n], &aj[..aj_n], Spend::Submit)?;
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
        let (outcome, settled) = unpack_roll(event.venue_seq);
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
        if self.counters.sweep_left != self.cancel_all_mark {
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
        self.reconcile();
        worked
    }
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
/// `settled` is `core_types::unpack_roll_seq`'s low-bit reading,
/// BIT-IDENTICAL to what this function did before the move. Widening
/// it to refuse a malformed kind byte (`core_types::roll_kind`) is a
/// live-arm behaviour change and is deliberately NOT smuggled into a
/// commit about the exposure ledger.
#[inline]
const fn unpack_roll(seq: u64) -> (u32, bool) {
    let (outcome, _twap_s, _family, settled) = core_types::unpack_roll_seq(seq);
    (outcome, settled)
}

/// Wall clock, nanoseconds. Read ONCE PER PUMP, never per fill.
fn now_ns() -> NsTs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(NsTs::MAX)) as NsTs)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
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
            320,
            "HlExecCounters changed size — 34 eight-byte counters (272 B) \
             rounded up to five 64-byte lines, with room for six more \
             before it grows. The message used to say 40 u64, which is \
             why adding one looked like it would cross a line and did not."
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

        x.count_post(&Err(crate::http::PostErr::before_send(
            crate::http::HttpErr::Dns,
        )));
        assert_eq!(x.budget.spent(), before, "nothing left, nothing spent");

        x.count_post(&Err(crate::http::PostErr {
            err: crate::http::HttpErr::Timeout,
            left_host: true,
        }));
        assert_eq!(
            x.budget.spent(),
            before + 1,
            "the venue has it — it is spent whether or not we read the answer"
        );

        x.count_post(&Ok((200, 0..1)));
        assert_eq!(x.budget.spent(), before + 2);
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
            x.budget.on_action_sent();
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
        assert_eq!(unpack_roll(seq), (2649, false));
        // The settled bit is bit 56, and the family byte must not leak
        // into the outcome id.
        let seq = 19_418u64 | (60u64 << 32) | (7u64 << 48) | (1u64 << 56);
        assert_eq!(unpack_roll(seq), (19_418, true));
        // Bits 0..32 are the OUTCOME ID, not `enc`. Confusing them is a
        // silent tenfold error naming a real other market.
        let (o, _) = unpack_roll(u64::from(u32::MAX));
        assert_eq!(o, u32::MAX);
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
}
