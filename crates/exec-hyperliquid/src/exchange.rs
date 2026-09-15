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
use crate::response::{scan, HlResponse};
use crate::sign::{sign_action, Network, Vault};
use crate::userws::{scan_user_fills, to_fill, to_fill_as, Routed, TidRing, UserFill, SNAPSHOT_RING};
use crate::userws_conn::UserWs;

/// Engine 1e6 → venue 1e8.
const ENGINE_TO_WIRE: i64 = 100;

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
    /// Earliest instant a reconnect may be attempted.
    ws_retry_at: Instant,
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
            ws_retry_at: Instant::now(),
            scratch: vec![UserFill::default(); SNAPSHOT_RING].into_boxed_slice(),
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
            &self.assets,
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
    /// needs (none of which is `ws`, and `assets` only by shared
    /// reference) lets the scan read the socket's own receive buffer
    /// in place: zero copy, zero allocation.
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
        assets: &AssetTable,
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
            let Some(sym) = assets.sym_of_coin(f.coin.of(payload)) else {
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
            Ok(n) => n > 0,
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
                }
                self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
                DispatchError::NoLiveRoute
            })?;

        // 2. Budget BEFORE signing: a signed action that is then
        //    discarded has still burned a nonce.
        if self.budget.may_submit().is_err() {
            self.counters.refused_local = self.counters.refused_local.wrapping_add(1);
            return Err(DispatchError::SlotDisabled);
        }

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
        let mut body = [0u8; MAX_REQ_BODY];
        let nonce = self.nonce.next(now_ms());
        let sig = sign_action(&self.sk, &mp[..mp_n], nonce, Vault::None, None, self.network)
            .map_err(|_| {
                self.counters.encode_failures = self.counters.encode_failures.wrapping_add(1);
                DispatchError::SignerRejected
            })?;
        let n = enc(
            envelope(&mut body, &aj[..aj_n], nonce, &sig, None, None),
            &mut self.counters,
        )?;

        let (_status, range) = self.http.post(&body[..n]).map_err(|_| {
            self.counters.rejected = self.counters.rejected.wrapping_add(1);
            DispatchError::Disconnected
        })?;
        // Every action that left the host counts against the address,
        // whatever the venue said about it.
        self.budget.on_action_sent();

        let resp = self.http.resp();
        let slice = &resp[range];
        match scan(slice) {
            // LAW E-5: this is the ACK. It tells us the venue took the
            // order; it never books a fill.
            Ok(HlResponse::Ok(ok)) if ok.accepted() => {
                self.counters.submitted = self.counters.submitted.wrapping_add(1);
                // WHO trades this leg — recorded on ACCEPTANCE, not on
                // intent. The venue settles a binary with a cloid-less
                // fill, so attribution has to come from somewhere that
                // is not the fill, and the only authority that cannot
                // be wrong is a member whose order the venue took. A
                // submit refused by the budget, the scale guards, the
                // signer or the venue never traded, and a leg we have
                // not traded must not absorb a settlement.
                if self.assets.note_owner(order.sym, order.strategy_id) {
                    self.counters.owner_contested =
                        self.counters.owner_contested.wrapping_add(1);
                }
                Ok(())
            }
            // The venue understood us and said NO. Distinct from an
            // answer we could not read: E6's `halt_on_reject_streak`
            // counts this one, and conflating the two would have it
            // halt on a parser bug or miss a venue refusing every
            // order.
            Ok(_) => {
                self.counters.rejected = self.counters.rejected.wrapping_add(1);
                Err(DispatchError::Http(200))
            }
            Err(_) => {
                self.counters.rejected = self.counters.rejected.wrapping_add(1);
                Err(DispatchError::JsonMalformed)
            }
        }
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

    fn on_idle(&mut self) -> bool {
        let worked = self.pump_user_events();
        self.persist_budget();
        worked
    }
}

/// The `InstrumentRoll` `venue_seq` layout, bits 0..32 and 56.
///
/// **Duplicated from `ingress_hyperliquid::family::pack_roll_seq`, not
/// imported** — §6.1 forbids this crate depending on the market-data
/// crate, and `strategy_bin15` re-writes the same unpack for the same
/// reason. `a_hand_built_roll_seq_unpacks_the_way_the_ingress_packs_it`
/// is what keeps the three in agreement.
///
/// Bits 0..32 are the **outcome id**, NOT `enc`. `AssetTable::asset_id`
/// and `outcome_coin` do the `× 10 + side` themselves, so feeding them
/// `enc` would be a silent tenfold error naming a real other market.
#[inline]
const fn unpack_roll(seq: u64) -> (u32, bool) {
    ((seq & 0xFFFF_FFFF) as u32, (seq >> 56) & 1 == 1)
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

    fn cfg() -> HlConfig {
        HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR).expect("cfg")
    }

    fn exchange() -> HlExchange<64> {
        let (p, _c) = Ring::<Fill, 64>::new().split();
        let tls = core_net::TlsTransport::default_client_config();
        HlExchange::new(
            &cfg(),
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
        // every `/metrics` publish, and it has already grown from two
        // cache lines to three; the next field should be a decision
        // rather than a surprise.
        assert_eq!(
            core::mem::size_of::<HlExecCounters>(),
            192,
            "HlExecCounters changed size — 17 u64 in three 64-byte lines"
        );
    }

    /// **An UNBOUND coin still books nothing.** That property is the
    /// whole reason the table answers by comparing bytes rather than
    /// by parsing `+<enc>`: a fill booked against a guessed symbol
    /// moves a position the member never took, silently and
    /// permanently, while a fill not booked is caught by
    /// reconciliation inside a minute.
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
