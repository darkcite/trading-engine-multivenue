// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `HcExchange` — the Hypercall arm as an [`OrderDispatch`].
//!
//! One slot, one wallet, synchronous REST on the caller's thread (the
//! HL E3 shape: a submit blocks for its round trip, bounded by
//! `core_net`'s request deadline), the private socket and the
//! reconciler pumped from [`OrderDispatch::on_idle`], and venue fills
//! handed out by [`OrderDispatch::try_next_fill`] from the arm's own
//! queue (the HyparbLive precedent — no engine fill lane).
//!
//! ## A submit, in order
//!
//! 1. the order is this arm's slot and venue, and its instrument is in
//!    the table (refused before anything is spent);
//! 2. the kind maps to `(tif, route)`: IoC → `ioc` + `best_execution`
//!    (take the providers' firm RPI at our limit or better) — the ONLY
//!    kind: the engine's maker is post-only and the venue has none (the
//!    verbs' resting order is [`HcExchange::place_resting`]);
//! 3. a row for the ACK is free (at most [`MAX_ORDERS`] working);
//! 4. the rate governor passes it — **before the nonce and the
//!    signature**, so a refusal spends neither;
//! 5. render + sign in place, post, read the answer fail-closed.
//!
//! A refusal is counted and returned; nothing here books a fill.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clob_dispatcher::{
    CancelAllState, DispatchError, DispatchStats, HaltSignal, LiveArmCounters, OrderDispatch,
};
use core_fill::ORDER_KIND_IOC;
use core_net::{HttpsReq, Method, PostErr};
use core_types::{CancelReq, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId};
use rustls::ClientConfig;
use signer_eip712::hypercall::{hc_domain_separator, HC_CHAIN_ID_MAINNET};

use crate::budget::HcBudget;
use crate::cloid;
use crate::config::HcExecConfig;
use crate::events::{self, FillIds, Msg};
use crate::nonce::{now_ms, HcNonce};
use crate::recon::{self, ReconErr};
use crate::render::{self, Route, Tif};
use crate::response::{self, Accepted, Refused, Why};
use crate::table::HcInstruments;
use crate::ws::{HcUserWs, WsErr};

/// Working orders the arm tracks (E6's `max_open_orders` ceiling).
pub const MAX_ORDERS: usize = 64;
/// Venue fills waiting for [`OrderDispatch::try_next_fill`].
pub const FILL_Q: usize = 128;
/// Retirements waiting for [`OrderDispatch::try_next_retired`].
pub const RETIRED_Q: usize = 64;
/// Request head window.
pub const HEAD_CAP: usize = 1024;
/// Request body window.
pub const BODY_CAP: usize = 2048;
/// Response buffer: a reconcile page of orders or fills.
pub const RESP_CAP: usize = 1 << 20;
/// Reconciliation cadence (a ceiling: it rides the idle path).
pub const RECON_EVERY: Duration = Duration::from_secs(60);
/// How long one idle pump may drain the private socket.
pub const PUMP_BUDGET: Duration = Duration::from_millis(2);
/// Reconnect backoff for the private socket.
pub const WS_BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];
/// The last refusal's reason, kept for the log and the verbs.
pub const REASON_MAX: usize = 160;

/// What the arm did — `/state`, `/metrics` and the verbs' report.
#[repr(C, align(64))]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct HcCounters {
    /// Places sent.
    pub submitted: u64,
    /// Places the venue accepted.
    pub accepted: u64,
    /// Places the venue refused (incl. auth and unreadable answers).
    pub rejected: u64,
    /// IoCs that traded nothing (not a reject).
    pub ioc_missed: u64,
    /// Refused before sending (slot, venue, table, kind, rows, render).
    pub refused_local: u64,
    /// Refused by the rate governor.
    pub refused_budget: u64,
    /// Requests that failed after bytes left the host (outcome unknown).
    pub sent_unanswered: u64,
    /// 401/403 answers.
    pub auth_refused: u64,
    /// 429 answers.
    pub rate_limited: u64,
    /// Cancels accepted.
    pub cancels_ok: u64,
    /// Cancels refused (incl. not found).
    pub cancels_refused: u64,
    /// Replaces accepted.
    pub replaces_ok: u64,
    /// Fills booked (queued for the engine).
    pub fills_booked: u64,
    /// Fills seen twice (a reconnect's replay), not booked again.
    pub fills_dup: u64,
    /// Fills on an instrument not in the table (reconcile sees them).
    pub fills_unknown_symbol: u64,
    /// Fills of an order id the arm did not place this boot.
    pub fills_unknown_order: u64,
    /// `Fill`/`OrderUpdate` messages that did not read.
    pub msgs_bad: u64,
    /// Fills dropped because the queue was full (must stay 0).
    pub fills_dropped: u64,
    /// Order updates applied.
    pub updates: u64,
    /// Private-socket (re)connects.
    pub ws_connects: u64,
    /// Private-socket connect failures.
    pub ws_connect_failures: u64,
    /// Reconciliations that read.
    pub recon_ok: u64,
    /// Reconciliations that failed to read.
    pub recon_failed: u64,
    /// Legs whose venue position differed from the booked one.
    pub recon_drift_legs: u64,
    /// Venue positions on instruments the table does not hold.
    pub recon_unseen_legs: u64,
    /// Our open orders at the venue the arm did not know (a past boot).
    pub orphans_seen: u64,
    /// …of which the sweep cancelled.
    pub orphans_cancelled: u64,
    /// Open orders on the wallet that are not ours (left alone).
    pub foreign_open: u64,
}

#[derive(Copy, Clone, Debug)]
struct Row {
    client_oid: u64,
    order_id: u64,
    sym: SymbolId,
    qty_1e6: i64,
    filled_1e6: i64,
    buy: bool,
    live: bool,
}

const FREE: Row = Row {
    client_oid: 0,
    order_id: 0,
    sym: core_types::SYMBOL_ID_NONE,
    qty_1e6: 0,
    filled_1e6: 0,
    buy: false,
    live: false,
};

const EMPTY_FILL: Fill = Fill::new(
    0,
    core_types::SYMBOL_ID_NONE,
    Side::Bid,
    Price::from_raw(0),
    Qty::from_raw(0),
    0,
);

/// The book the private socket updates: separate from the socket so a
/// pump can borrow both (disjoint fields).
struct Book {
    slot: u8,
    rows: [Row; MAX_ORDERS],
    /// Round-robin cursor for [`Book::free_row`]: a finished order's row
    /// is the LAST to be reused, so its late fills still find their
    /// member id (reusing the first dead row stripped them).
    next_row: usize,
    /// The socket said `Error` after authentication: the pump drops it.
    ws_error: bool,
    fills: [Fill; FILL_Q],
    f_head: usize,
    f_len: usize,
    retired: [(u64, u8); RETIRED_Q],
    r_head: usize,
    r_len: usize,
    fill_ids: FillIds,
    /// Booked position per table row, contracts ×1e6 (signed).
    pos: Box<[i64]>,
    /// Last fill price per table row (drift valuation).
    last_px: Box<[i64]>,
    counters: HcCounters,
}

impl Book {
    fn row_by_order(&mut self, order_id: u64) -> Option<&mut Row> {
        let mut i = 0usize;
        while i < MAX_ORDERS {
            if self.rows[i].order_id == order_id && self.rows[i].order_id != 0 {
                return Some(&mut self.rows[i]);
            }
            i += 1;
        }
        None
    }

    fn live_row_by_oid(&self, client_oid: u64) -> Option<usize> {
        let mut i = 0usize;
        while i < MAX_ORDERS {
            if self.rows[i].live && self.rows[i].client_oid == client_oid {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn free_row(&mut self) -> Option<usize> {
        let mut k = 0usize;
        while k < MAX_ORDERS {
            let i = (self.next_row + k) % MAX_ORDERS;
            if !self.rows[i].live {
                self.next_row = (i + 1) % MAX_ORDERS;
                return Some(i);
            }
            k += 1;
        }
        None
    }

    fn live_count(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_ORDERS {
            n += usize::from(self.rows[i].live);
            i += 1;
        }
        n
    }

    fn retire(&mut self, client_oid: u64) {
        if self.r_len >= RETIRED_Q {
            return;
        }
        let at = (self.r_head + self.r_len) % RETIRED_Q;
        self.retired[at] = (client_oid, self.slot);
        self.r_len += 1;
    }

    fn push_fill(&mut self, f: Fill) {
        if self.f_len >= FILL_Q {
            self.counters.fills_dropped = self.counters.fills_dropped.wrapping_add(1);
            return;
        }
        let at = (self.f_head + self.f_len) % FILL_Q;
        self.fills[at] = f;
        self.f_len += 1;
        self.counters.fills_booked = self.counters.fills_booked.wrapping_add(1);
    }

    /// One private-socket payload.
    fn on_payload(&mut self, p: &[u8], table: &HcInstruments) {
        match events::parse(p) {
            Msg::Fill(f) => {
                if !self.fill_ids.first_time(f.fill_id) {
                    self.counters.fills_dup = self.counters.fills_dup.wrapping_add(1);
                    return;
                }
                let Some((idx, sym)) = table.find(&p[f.symbol.clone()]) else {
                    self.counters.fills_unknown_symbol =
                        self.counters.fills_unknown_symbol.wrapping_add(1);
                    return;
                };
                let oid = match self.row_by_order(f.order_id) {
                    Some(r) => r.client_oid,
                    None => {
                        self.counters.fills_unknown_order =
                            self.counters.fills_unknown_order.wrapping_add(1);
                        0
                    }
                };
                let signed = if f.buy { f.qty_1e6 } else { -f.qty_1e6 };
                self.pos[idx] = self.pos[idx].saturating_add(signed);
                self.last_px[idx] = f.px_1e6;
                let side = if f.buy { Side::Bid } else { Side::Ask };
                let fill = Fill::new(
                    core_time::now_ns(),
                    sym,
                    side,
                    Price::from_raw(f.px_1e6),
                    Qty::from_raw(f.qty_1e6),
                    oid,
                )
                .with_attribution(self.slot, core_types::FILL_ORIGIN_VENUE);
                self.push_fill(fill);
            }
            Msg::Update(u) => {
                self.counters.updates = self.counters.updates.wrapping_add(1);
                let mut retire = None;
                if let Some(r) = self.row_by_order(u.order_id) {
                    r.filled_1e6 = u.filled_1e6.max(r.filled_1e6);
                    if r.live && !u.status.is_working() {
                        r.live = false;
                        if r.filled_1e6 < r.qty_1e6 {
                            retire = Some(r.client_oid);
                        }
                    }
                }
                if let Some(oid) = retire {
                    self.retire(oid);
                }
            }
            Msg::Bad => self.counters.msgs_bad = self.counters.msgs_bad.wrapping_add(1),
            // An `Error` on an authenticated socket (a dropped
            // subscription, a slow-consumer notice): the stream can no
            // longer be trusted to carry every fill — reconnect.
            Msg::Error(_) => self.ws_error = true,
            _ => {}
        }
    }
}

/// Monotonic milliseconds — the rate governor's clock (a wall clock
/// stepped backwards would stall it, cancels included). The nonce keeps
/// the wall clock (`nonce::now_ms`): the venue reads it.
#[inline]
fn mono_ms() -> u64 {
    core_time::now_ns() / 1_000_000
}

/// Does a refusal's text say the order does not exist (case-free
/// "not found" / "not_found" / "unknown order")?
fn says_not_found(r: &[u8]) -> bool {
    const NEEDLES: [&[u8]; 3] = [b"not found", b"not_found", b"unknown order"];
    let mut k = 0usize;
    while k < NEEDLES.len() {
        let n = NEEDLES[k];
        let mut i = 0usize;
        while i + n.len() <= r.len() {
            if r[i..i + n.len()].eq_ignore_ascii_case(n) {
                return true;
            }
            i += 1;
        }
        k += 1;
    }
    false
}

/// Why the arm could not be built.
#[derive(Debug)]
pub enum HcBootErr {
    /// The key did not parse.
    Key,
    /// The REST client could not be prepared.
    Http(core_net::PostErrKind),
    /// The private socket could not be prepared.
    Ws(WsErr),
}

impl core::fmt::Display for HcBootErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Key => write!(f, "hypercall exec: the signing key did not parse"),
            Self::Http(e) => write!(f, "hypercall exec: {e}"),
            Self::Ws(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HcBootErr {}

/// What one reconciliation found (the verbs print it).
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct ReconReport {
    /// Venue positions listed.
    pub positions: usize,
    /// Legs whose position differed from the booked one.
    pub drift_legs: u32,
    /// Venue positions on instruments the table does not hold.
    pub unseen_legs: u32,
    /// The worst drift, USD ×1e6.
    pub drift_usd_1e6: i64,
    /// Open orders on the wallet.
    pub open_orders: usize,
    /// …of which ours but unknown to this boot.
    pub orphans: usize,
    /// …of which not ours.
    pub foreign: usize,
    /// Free collateral, USD ×1e6.
    pub available_1e6: i64,
    /// The venue and the arm agree on every leg.
    pub agreed: bool,
}

/// The arm (module doc).
pub struct HcExchange {
    wallet: [u8; 20],
    sk: secp256k1::SecretKey,
    dom: [u8; 32],
    http: HttpsReq,
    ws: HcUserWs,
    table: HcInstruments,
    book: Book,
    nonce: HcNonce,
    budget: HcBudget,
    reject_streak: u32,
    unknown_sym_streak: u32,
    ws_alive_ns: u64,
    ws_backoff: usize,
    ws_next: Instant,
    recon_next: Instant,
    recon_agreed_ns: u64,
    recon_drift_usd_1e6: i64,
    reconciled: bool,
    seeded: bool,
    venue_scratch: Box<[i64]>,
    orphans: [u64; MAX_ORDERS],
    orphans_n: usize,
    reason: [u8; REASON_MAX],
    reason_len: usize,
    last_why: Option<Why>,
}

impl HcExchange {
    /// Build the arm for `slot` over `table`, talking to
    /// `cfg.rest_host():port` and `cfg.ws_host():port`. Boot-only:
    /// allocates every buffer the arm will use. Opens nothing.
    ///
    /// # Errors
    ///
    /// [`HcBootErr`].
    pub fn new(
        cfg: &HcExecConfig,
        tls: Arc<ClientConfig>,
        table: HcInstruments,
        slot: u8,
        port: u16,
    ) -> Result<Self, HcBootErr> {
        let sk = cfg.secret_key().map_err(|_| HcBootErr::Key)?;
        let http = HttpsReq::new(cfg.rest_host(), port, tls.clone(), HEAD_CAP, BODY_CAP, RESP_CAP)
            .map_err(HcBootErr::Http)?;
        let ws = HcUserWs::new(cfg.ws_host(), port, tls, cfg.wallet()).map_err(HcBootErr::Ws)?;
        let n = crate::table::HC_TABLE_MAX;
        let now = Instant::now();
        Ok(Self {
            wallet: *cfg.wallet(),
            sk,
            dom: hc_domain_separator(HC_CHAIN_ID_MAINNET),
            http,
            ws,
            table,
            book: Book {
                slot,
                rows: [FREE; MAX_ORDERS],
                next_row: 0,
                ws_error: false,
                fills: [EMPTY_FILL; FILL_Q],
                f_head: 0,
                f_len: 0,
                retired: [(0, 0); RETIRED_Q],
                r_head: 0,
                r_len: 0,
                fill_ids: FillIds::new(),
                pos: vec![0i64; n].into_boxed_slice(),
                last_px: vec![0i64; n].into_boxed_slice(),
                counters: HcCounters::default(),
            },
            nonce: HcNonce::new(),
            budget: HcBudget::new(),
            reject_streak: 0,
            unknown_sym_streak: 0,
            ws_alive_ns: 0,
            ws_backoff: 0,
            ws_next: now,
            recon_next: now,
            recon_agreed_ns: 0,
            recon_drift_usd_1e6: 0,
            reconciled: false,
            seeded: false,
            venue_scratch: vec![0i64; n].into_boxed_slice(),
            orphans: [0; MAX_ORDERS],
            orphans_n: 0,
            reason: [0; REASON_MAX],
            reason_len: 0,
            last_why: None,
        })
    }

    /// The slot this arm trades.
    #[must_use]
    pub const fn slot(&self) -> u8 {
        self.book.slot
    }

    /// The counters.
    #[must_use]
    pub const fn counters(&self) -> &HcCounters {
        &self.book.counters
    }

    /// The last refusal's stated reason (the venue's words), if any.
    #[must_use]
    pub fn last_reason(&self) -> &[u8] {
        &self.reason[..self.reason_len]
    }

    /// What the venue's last refusal was — `None` after an acceptance,
    /// or when the last failure never reached a venue answer.
    #[must_use]
    pub const fn last_refusal(&self) -> Option<Why> {
        self.last_why
    }

    /// The booked position on `sym`, contracts ×1e6.
    #[must_use]
    pub fn position(&self, sym: SymbolId) -> i64 {
        self.table.find_sym(sym).map_or(0, |i| self.book.pos[i])
    }

    /// Working orders the arm tracks.
    #[must_use]
    pub fn live_orders(&self) -> usize {
        self.book.live_count()
    }

    /// The instrument table.
    #[must_use]
    pub const fn table(&self) -> &HcInstruments {
        &self.table
    }

    /// Is the private socket up?
    #[must_use]
    pub fn ws_connected(&self) -> bool {
        self.ws.is_connected()
    }

    fn note_reason(&mut self, r: &Refused) {
        self.last_why = Some(r.why);
        self.reason_len = 0;
        if let Some(span) = r.reason.clone() {
            let src = &self.http.resp()[span];
            let n = src.len().min(REASON_MAX);
            // COPY: ≤ 160 B of the venue's refusal text out of the
            // response buffer, per refusal — the next request reuses
            // that buffer and the reason must outlive it for the log.
            self.reason[..n].copy_from_slice(&src[..n]);
            self.reason_len = n;
        }
    }

    fn post_failed(&mut self, e: &PostErr) -> DispatchError {
        self.last_why = None;
        if e.left_host {
            self.book.counters.sent_unanswered = self.book.counters.sent_unanswered.wrapping_add(1);
        }
        DispatchError::Disconnected
    }

    fn judge_refusal(&mut self, r: &Refused) -> DispatchError {
        self.note_reason(r);
        let c = &mut self.book.counters;
        match r.why {
            Why::IocMissed => {
                c.ioc_missed = c.ioc_missed.wrapping_add(1);
                return DispatchError::Http(200);
            }
            Why::Auth => c.auth_refused = c.auth_refused.wrapping_add(1),
            Why::RateLimited => {
                c.rate_limited = c.rate_limited.wrapping_add(1);
                self.budget.on_429(mono_ms(), 0);
            }
            _ => {}
        }
        c.rejected = c.rejected.wrapping_add(1);
        self.reject_streak = self.reject_streak.saturating_add(1);
        match r.why {
            Why::Unreadable => DispatchError::JsonMalformed,
            Why::Auth => DispatchError::SignerRejected,
            Why::RateLimited => DispatchError::Http(429),
            Why::Server => DispatchError::Http(500),
            _ => DispatchError::Http(200),
        }
    }

    fn refuse_local(&mut self, e: DispatchError) -> DispatchError {
        self.book.counters.refused_local = self.book.counters.refused_local.wrapping_add(1);
        e
    }

    /// An order naming an instrument the table does not hold — the
    /// Hypercall form of LAW E-4's stale instance (an expired series):
    /// the ONE local refusal the router's `asset_refusal_streak` counts.
    fn refuse_unknown(&mut self) -> DispatchError {
        self.unknown_sym_streak = self.unknown_sym_streak.saturating_add(1);
        self.refuse_local(DispatchError::NoLiveRoute)
    }

    /// The engine's kinds the venue can honour. **IoC only**: the
    /// engine's `ORDER_KIND_MAKER` is POST-ONLY, and Hypercall offers no
    /// post-only time in force (`gtc` | `ioc` | `fok`) — a `gtc` would
    /// cross and take, so paper (which never crosses a maker) and live
    /// would disagree (E-1). A resting order is the verbs' own
    /// [`HcExchange::place_resting`].
    const fn route_of(kind: u8) -> Option<(Tif, Route)> {
        match kind {
            ORDER_KIND_IOC => Some((Tif::Ioc, Route::BestExecution)),
            _ => None,
        }
    }

    fn place(&mut self, order: &Order) -> Result<(), DispatchError> {
        let Some((tif, route)) = Self::route_of(order.kind) else {
            return Err(self.refuse_local(DispatchError::NoLiveRoute));
        };
        self.place_as(order, tif, route)
    }

    /// **The verbs' resting order** — a `gtc` + `book_only` limit that
    /// CAN cross (the venue has no post-only). The dust smoke prices it
    /// where nothing can: a bid of $0.0005. Never reached by the engine.
    ///
    /// # Errors
    ///
    /// As a submit.
    pub fn place_resting(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.place_as(order, Tif::Gtc, Route::BookOnly)
    }

    fn place_as(&mut self, order: &Order, tif: Tif, route: Route) -> Result<(), DispatchError> {
        if order.venue != VenueId::Hypercall as u8 || order.strategy_id != self.book.slot {
            return Err(self.refuse_local(DispatchError::NoLiveRoute));
        }
        if self.table.name_of(order.sym).is_none() {
            return Err(self.refuse_unknown());
        }
        let Some(row) = self.book.free_row() else {
            return Err(self.refuse_local(DispatchError::QueueFull));
        };
        if self.budget.place(mono_ms()).is_err() {
            self.book.counters.refused_budget = self.book.counters.refused_budget.wrapping_add(1);
            return Err(DispatchError::SlotDisabled);
        }
        let Some(nonce) = self.nonce.next(now_ms()) else {
            return Err(self.refuse_local(DispatchError::SignerRejected));
        };
        let cid = cloid::encode(self.book.slot, order.client_oid);
        let rendered = {
            let p = render::Place {
                wallet: &self.wallet,
                symbol: self.table.name_of(order.sym).unwrap_or(b""),
                buy: order.side == Side::Bid,
                px_1e6: order.px.raw(),
                qty_1e6: order.qty.raw(),
                tif,
                route,
                client_id: &cid,
                nonce,
            };
            render::place(self.http.body_mut(), &p, &self.sk, &self.dom)
        };
        let n = match rendered {
            Ok(n) => n,
            Err(_) => return Err(self.refuse_local(DispatchError::EncodeOverflow)),
        };
        self.book.counters.submitted = self.book.counters.submitted.wrapping_add(1);
        let (http, range) = match self.http.request(Method::Post, b"/order", &[], n) {
            Ok(x) => x,
            Err(e) => return Err(self.post_failed(&e)),
        };
        match response::scan_place(http, &self.http.resp()[range.clone()]) {
            Ok(a) => {
                self.accept(row, order, &a);
                Ok(())
            }
            Err(r) => {
                let r = Refused {
                    why: r.why,
                    reason: r.reason.map(|s| range.start + s.start..range.start + s.end),
                };
                Err(self.judge_refusal(&r))
            }
        }
    }

    fn accept(&mut self, row: usize, order: &Order, a: &Accepted) {
        self.last_why = None;
        self.reject_streak = 0;
        self.unknown_sym_streak = 0;
        self.book.counters.accepted = self.book.counters.accepted.wrapping_add(1);
        self.book.rows[row] = Row {
            client_oid: order.client_oid,
            order_id: a.order_id,
            sym: order.sym,
            qty_1e6: order.qty.raw(),
            filled_1e6: a.filled_1e6,
            buy: order.side == Side::Bid,
            live: a.status.is_working(),
        };
        if !a.status.is_working() && a.filled_1e6 < order.qty.raw() {
            self.book.retire(order.client_oid);
        }
    }

    /// Cancel `client_oid` of this arm's slot by its client id. An order
    /// the arm does not track (a past boot's) is still named — the id
    /// is deterministic.
    ///
    /// # Errors
    ///
    /// The refusal.
    pub fn cancel_oid(&mut self, client_oid: u64) -> Result<Accepted, DispatchError> {
        if self.budget.cancel(mono_ms()).is_err() {
            self.book.counters.refused_budget = self.book.counters.refused_budget.wrapping_add(1);
            return Err(DispatchError::SlotDisabled);
        }
        let Some(nonce) = self.nonce.next(now_ms()) else {
            return Err(self.refuse_local(DispatchError::SignerRejected));
        };
        let cid = cloid::encode(self.book.slot, client_oid);
        let n = match render::cancel_cloid(
            self.http.body_mut(),
            &self.wallet,
            &cid,
            nonce,
            &self.sk,
            &self.dom,
        ) {
            Ok(n) => n,
            Err(_) => return Err(self.refuse_local(DispatchError::EncodeOverflow)),
        };
        let (http, range) = match self.http.request(Method::Delete, b"/order_cloid", &[], n) {
            Ok(x) => x,
            Err(e) => return Err(self.post_failed(&e)),
        };
        match response::scan_cancel(http, &self.http.resp()[range.clone()]) {
            Ok(a) => {
                self.book.counters.cancels_ok = self.book.counters.cancels_ok.wrapping_add(1);
                if let Some(i) = self.book.live_row_by_oid(client_oid) {
                    let r = &mut self.book.rows[i];
                    r.live = false;
                    r.filled_1e6 = r.filled_1e6.max(a.filled_1e6);
                    if r.filled_1e6 < r.qty_1e6 {
                        self.book.retire(client_oid);
                    }
                }
                Ok(a)
            }
            Err(r) => {
                self.book.counters.cancels_refused = self.book.counters.cancels_refused.wrapping_add(1);
                let r = Refused {
                    why: r.why,
                    reason: r.reason.map(|s| range.start + s.start..range.start + s.end),
                };
                self.note_reason(&r);
                if r.why == Why::RateLimited {
                    self.budget.on_429(mono_ms(), 0);
                }
                // ONLY an explicit "not found" means the order is gone.
                // Any other refusal (a used nonce, a 4xx, an answer that
                // did not read) leaves it possibly RESTING: the row stays
                // live and the sweep asks again — reading those as "gone"
                // was a cancel-all that failed open.
                Err(match r.why {
                    Why::Rejected if says_not_found(&self.reason[..self.reason_len]) => {
                        DispatchError::NoSuchOrder
                    }
                    Why::Rejected | Why::RateLimited | Why::Server => DispatchError::Http(http),
                    Why::Unreadable | Why::IocMissed => DispatchError::JsonMalformed,
                    Why::Auth => DispatchError::SignerRejected,
                })
            }
        }
    }

    fn replace(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        let order = req.order();
        if order.venue != VenueId::Hypercall as u8 || order.strategy_id != self.book.slot {
            return Err(self.refuse_local(DispatchError::NoLiveRoute));
        }
        let Some(prev) = self.book.live_row_by_oid(req.prev_client_oid()) else {
            return Err(DispatchError::NoSuchOrder);
        };
        let prev_row = self.book.rows[prev];
        if prev_row.sym != order.sym || prev_row.buy != (order.side == Side::Bid) {
            return Err(DispatchError::IdentityMismatch);
        }
        let Some((tif, _)) = Self::route_of(order.kind) else {
            return Err(self.refuse_local(DispatchError::NoLiveRoute));
        };
        if self.table.name_of(order.sym).is_none() {
            return Err(self.refuse_unknown());
        }
        if self.budget.place(mono_ms()).is_err() {
            self.book.counters.refused_budget = self.book.counters.refused_budget.wrapping_add(1);
            return Err(DispatchError::SlotDisabled);
        }
        let Some(nonce) = self.nonce.next(now_ms()) else {
            return Err(self.refuse_local(DispatchError::SignerRejected));
        };
        let cid = cloid::encode(self.book.slot, order.client_oid);
        let rendered = {
            let p = render::Replace {
                wallet: &self.wallet,
                order_id: prev_row.order_id,
                symbol: self.table.name_of(order.sym).unwrap_or(b""),
                buy: order.side == Side::Bid,
                px_1e6: order.px.raw(),
                qty_1e6: order.qty.raw(),
                tif,
                client_id: &cid,
                nonce,
            };
            render::replace(self.http.body_mut(), &p, &self.sk, &self.dom)
        };
        let n = match rendered {
            Ok(n) => n,
            Err(_) => return Err(self.refuse_local(DispatchError::EncodeOverflow)),
        };
        self.book.counters.submitted = self.book.counters.submitted.wrapping_add(1);
        let (http, range) = match self.http.request(Method::Put, b"/order", &[], n) {
            Ok(x) => x,
            Err(e) => return Err(self.post_failed(&e)),
        };
        match response::scan_place(http, &self.http.resp()[range.clone()]) {
            Ok(a) => {
                self.book.counters.replaces_ok = self.book.counters.replaces_ok.wrapping_add(1);
                // Atomic at the venue: the old order is gone, the new
                // one stands in its row.
                self.book.rows[prev].live = false;
                self.accept(prev, order, &a);
                Ok(())
            }
            Err(r) => {
                let r = Refused {
                    why: r.why,
                    reason: r.reason.map(|s| range.start + s.start..range.start + s.end),
                };
                Err(self.judge_refusal(&r))
            }
        }
    }

    /// Pump the private socket once (reconnecting on its backoff). True
    /// when anything was read or dialled.
    pub fn pump(&mut self) -> bool {
        let now = Instant::now();
        if !self.ws.is_connected() {
            if now < self.ws_next {
                return false;
            }
            match self.ws.connect() {
                Ok(()) => {
                    self.book.counters.ws_connects = self.book.counters.ws_connects.wrapping_add(1);
                    self.ws_backoff = 0;
                    self.ws_alive_ns = core_time::now_ns();
                    // LAW D8: a reconnect reconciles before new risk.
                    self.recon_next = Instant::now();
                }
                Err(_) => {
                    self.book.counters.ws_connect_failures =
                        self.book.counters.ws_connect_failures.wrapping_add(1);
                    self.ws_next = Instant::now() + WS_BACKOFF[self.ws_backoff];
                    self.ws_backoff = (self.ws_backoff + 1).min(WS_BACKOFF.len() - 1);
                }
            }
            return true;
        }
        let table = &self.table;
        let book = &mut self.book;
        match self.ws.pump(PUMP_BUDGET, |p| book.on_payload(p, table)) {
            Ok(n) => {
                if self.book.ws_error {
                    // The venue said `Error` on the fill stream: drop it,
                    // reconnect on the backoff, reconcile on the way back.
                    self.book.ws_error = false;
                    self.ws.disconnect();
                    self.ws_next = Instant::now() + WS_BACKOFF[self.ws_backoff];
                    return true;
                }
                // Liveness is a pump that read without error — venue
                // pings count, a quiet wallet is not a dead socket.
                self.ws_alive_ns = core_time::now_ns();
                n > 0
            }
            Err(_) => {
                self.ws_next = Instant::now() + WS_BACKOFF[self.ws_backoff];
                true
            }
        }
    }

    fn read(&mut self, target: &[u8]) -> Result<(u16, core::ops::Range<usize>), ReconErr> {
        if self.budget.read(mono_ms()).is_err() {
            return Err(ReconErr::Refused);
        }
        let r = self
            .http
            .request(Method::Get, target, &[], 0)
            .map_err(|_| ReconErr::Refused)?;
        if r.0 == 429 {
            self.book.counters.rate_limited = self.book.counters.rate_limited.wrapping_add(1);
            self.budget.on_429(mono_ms(), 0);
        }
        Ok(r)
    }

    /// One reconciliation against the venue, now (module doc of
    /// [`crate::recon`]). The FIRST since boot adopts the venue's
    /// positions as the booked ones (the venue is the truth about what
    /// a past boot left); every later one compares.
    ///
    /// # Errors
    ///
    /// A read that failed; counted, the arm stays un-reconciled.
    pub fn reconcile(&mut self) -> Result<ReconReport, ReconErr> {
        let r = self.reconcile_inner();
        match r {
            Ok(rep) => {
                self.book.counters.recon_ok = self.book.counters.recon_ok.wrapping_add(1);
                if rep.agreed {
                    self.reconciled = true;
                    self.recon_agreed_ns = core_time::now_ns();
                }
                self.recon_drift_usd_1e6 = rep.drift_usd_1e6;
            }
            Err(_) => {
                self.book.counters.recon_failed = self.book.counters.recon_failed.wrapping_add(1);
            }
        }
        self.recon_next = Instant::now() + RECON_EVERY;
        r
    }

    fn reconcile_inner(&mut self) -> Result<ReconReport, ReconErr> {
        let mut rep = ReconReport::default();
        let mut target = [0u8; 96];
        let n = render::wallet_target(&mut target, b"/portfolio", &self.wallet, b"")
            .map_err(|_| ReconErr::Unreadable)?;
        let (http, range) = self.read(&target[..n])?;
        let len = self.table.len();
        let mut k = 0usize;
        while k < len {
            self.venue_scratch[k] = 0;
            k += 1;
        }
        let mut unseen = 0u32;
        let mut unseen_usd = 0i64;
        {
            let table = &self.table;
            let scratch = &mut self.venue_scratch;
            let last_px = &mut self.book.last_px;
            let acct = recon::scan_portfolio(http, &self.http.resp()[range], |sym, amount, entry| {
                match table.find(sym) {
                    Some((i, _)) => {
                        scratch[i] = amount;
                        if entry > 0 {
                            last_px[i] = entry;
                        }
                    }
                    None if amount != 0 => {
                        unseen += 1;
                        unseen_usd = unseen_usd.max(recon::notional_1e6(amount, entry.max(1_000_000)));
                    }
                    None => {}
                }
            })?;
            rep.positions = acct.positions;
            rep.available_1e6 = acct.available_1e6;
        }
        let mut drift = unseen_usd;
        let mut legs = 0u32;
        let mut i = 0usize;
        while i < len {
            if !self.seeded {
                self.book.pos[i] = self.venue_scratch[i];
            } else {
                let d = i128::from(self.venue_scratch[i]) - i128::from(self.book.pos[i]);
                if d != 0 {
                    legs += 1;
                    let px = if self.book.last_px[i] > 0 { self.book.last_px[i] } else { 1_000_000 };
                    let d = i64::try_from(d).unwrap_or(i64::MAX);
                    drift = drift.max(recon::notional_1e6(d, px));
                }
            }
            i += 1;
        }
        self.seeded = true;
        rep.unseen_legs = unseen;
        rep.drift_legs = legs;
        rep.drift_usd_1e6 = drift;
        self.book.counters.recon_drift_legs =
            self.book.counters.recon_drift_legs.wrapping_add(u64::from(legs));
        self.book.counters.recon_unseen_legs =
            self.book.counters.recon_unseen_legs.wrapping_add(u64::from(unseen));

        // The wallet's open orders: ours this boot, ours from a past
        // boot (orphans — queued for the sweep), or not ours.
        let n = render::wallet_target(&mut target, b"/orders", &self.wallet, b"&status=open")
            .map_err(|_| ReconErr::Unreadable)?;
        let (http, range) = self.read(&target[..n])?;
        let slot = self.book.slot;
        let mut open = 0usize;
        let mut n_orphans = 0usize;
        let mut foreign = 0usize;
        {
            let body = &self.http.resp()[range];
            let book = &self.book;
            let orphans = &mut self.orphans;
            recon::scan_orders(http, body, |o| {
                open += 1;
                match cloid::decode(&body[o.client_id.clone()]) {
                    Some((s, oid)) if s == slot => {
                        if book.live_row_by_oid(oid).is_none() && n_orphans < MAX_ORDERS {
                            orphans[n_orphans] = oid;
                            n_orphans += 1;
                        }
                    }
                    _ => foreign += 1,
                }
            })?;
        }
        self.orphans_n = n_orphans;
        self.book.counters.orphans_seen =
            self.book.counters.orphans_seen.wrapping_add(n_orphans as u64);
        self.book.counters.foreign_open = foreign as u64;
        rep.open_orders = open;
        rep.orphans = n_orphans;
        rep.foreign = foreign;
        rep.agreed = legs == 0 && unseen == 0;
        Ok(rep)
    }

    /// Cancel every order of ours the last reconciliation found and this
    /// boot did not place (S7-L1's boot sweep): the count cancelled.
    pub fn sweep_orphans(&mut self) -> usize {
        let mut done = 0usize;
        let n = self.orphans_n;
        let mut i = 0usize;
        while i < n {
            let oid = self.orphans[i];
            if self.cancel_oid(oid).is_ok() {
                done += 1;
            }
            i += 1;
        }
        self.orphans_n = 0;
        self.book.counters.orphans_cancelled =
            self.book.counters.orphans_cancelled.wrapping_add(done as u64);
        done
    }

    /// `POST /risk/simulate/orders` for one resting leg (never mutates):
    /// the HTTP status and the body, for the verbs to print.
    ///
    /// # Errors
    ///
    /// The render or transport failure.
    pub fn simulate(
        &mut self,
        sym: SymbolId,
        buy: bool,
        px_1e6: i64,
        qty_1e6: i64,
    ) -> Result<(u16, &[u8]), DispatchError> {
        let Some(name) = self.table.name_of(sym) else {
            return Err(DispatchError::NoLiveRoute);
        };
        let n = render::simulate(self.http.body_mut(), &self.wallet, name, buy, px_1e6, qty_1e6)
            .map_err(|_| DispatchError::EncodeOverflow)?;
        if self.budget.read(mono_ms()).is_err() {
            return Err(DispatchError::SlotDisabled);
        }
        let (http, range) = self
            .http
            .request(Method::Post, b"/risk/simulate/orders", &[], n)
            .map_err(|_| DispatchError::Disconnected)?;
        Ok((http, &self.http.resp()[range]))
    }

    /// A read the verbs print verbatim (`/portfolio`, `/orders`, `/fills`).
    ///
    /// # Errors
    ///
    /// The transport failure or the governor.
    pub fn get_wallet(&mut self, path: &[u8], tail: &[u8]) -> Result<(u16, &[u8]), DispatchError> {
        let mut target = [0u8; 128];
        let n = render::wallet_target(&mut target, path, &self.wallet, tail)
            .map_err(|_| DispatchError::EncodeOverflow)?;
        let (http, range) = self.read(&target[..n]).map_err(|_| DispatchError::Disconnected)?;
        Ok((http, &self.http.resp()[range]))
    }
}

impl OrderDispatch for HcExchange {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.place(order)
    }

    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        if req.strategy_id != self.book.slot {
            return Err(self.refuse_local(DispatchError::NoLiveRoute));
        }
        self.cancel_oid(req.client_oid).map(|_| ())
    }

    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        self.replace(req)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        let b = &mut self.book;
        if b.f_len == 0 {
            return None;
        }
        let f = b.fills[b.f_head];
        b.f_head = (b.f_head + 1) % FILL_Q;
        b.f_len -= 1;
        Some(f)
    }

    fn try_next_retired(&mut self) -> Option<(u64, u8)> {
        let b = &mut self.book;
        if b.r_len == 0 {
            return None;
        }
        let r = b.retired[b.r_head];
        b.r_head = (b.r_head + 1) % RETIRED_Q;
        b.r_len -= 1;
        Some(r)
    }

    fn stats(&self) -> DispatchStats {
        let c = &self.book.counters;
        DispatchStats {
            accepted: c.accepted,
            rejected: c.rejected.wrapping_add(c.refused_local),
            fills_seen: c.fills_booked,
            ..DispatchStats::default()
        }
    }

    fn on_idle(&mut self) -> bool {
        let pumped = self.pump();
        let mut did = pumped;
        if self.ws.is_connected() && Instant::now() >= self.recon_next {
            let _ = self.reconcile();
            did = true;
        }
        did
    }

    fn halt_signal(&self) -> HaltSignal {
        let now = core_time::now_ns();
        // No observation yet, or a socket that is up: no gap.
        let gap = if self.ws_alive_ns == 0 || self.ws.is_connected() {
            0
        } else {
            now.saturating_sub(self.ws_alive_ns)
        };
        let age = if self.recon_agreed_ns == 0 {
            0
        } else {
            now.saturating_sub(self.recon_agreed_ns)
        };
        HaltSignal::new(
            gap,
            self.recon_drift_usd_1e6,
            self.reject_streak,
            self.unknown_sym_streak,
            false,
            self.reconciled,
            age,
        )
    }

    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        let mut last = Ok(());
        let mut i = 0usize;
        while i < MAX_ORDERS {
            if self.book.rows[i].live {
                let oid = self.book.rows[i].client_oid;
                match self.cancel_oid(oid) {
                    Ok(_) => {}
                    // Gone at the venue: the row is done.
                    Err(DispatchError::NoSuchOrder) => self.book.rows[i].live = false,
                    // Possibly still resting: the row stays live, the
                    // state reads Working, the router asks again.
                    Err(e) => last = Err(e),
                }
            }
            i += 1;
        }
        last
    }

    fn cancel_all_state(&self) -> CancelAllState {
        if self.book.live_count() == 0 {
            CancelAllState::Clear
        } else {
            CancelAllState::Working
        }
    }

    fn arm_counters(&self) -> LiveArmCounters {
        let c = &self.book.counters;
        LiveArmCounters {
            submitted: c.submitted,
            rejected: c.rejected,
            ioc_missed: c.ioc_missed,
            refused_local: c.refused_local.wrapping_add(c.refused_budget),
            sent_unanswered: c.sent_unanswered,
            fills_booked: c.fills_booked,
            fills_unresolved: c.fills_unknown_symbol,
            fills_dropped: c.fills_dropped,
            fills_scan_failed: c.msgs_bad,
            recon_ok: c.recon_ok,
            recon_failed: c.recon_failed,
            recon_drift_legs: c.recon_drift_legs,
            recon_unseen_legs: c.recon_unseen_legs,
            ws_reconnects: c.ws_connects,
            ws_connect_failures: c.ws_connect_failures,
            ..LiveArmCounters::default()
        }
    }

    fn on_shutdown(&mut self) {
        let _ = self.cancel_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> (Book, HcInstruments) {
        let mut t = HcInstruments::new();
        t.insert(513, b"BTC-20261002-100000-C").unwrap();
        let b = Book {
            slot: 7,
            rows: [FREE; MAX_ORDERS],
            next_row: 0,
            ws_error: false,
            fills: [EMPTY_FILL; FILL_Q],
            f_head: 0,
            f_len: 0,
            retired: [(0, 0); RETIRED_Q],
            r_head: 0,
            r_len: 0,
            fill_ids: FillIds::new(),
            pos: vec![0i64; 4].into_boxed_slice(),
            last_px: vec![0i64; 4].into_boxed_slice(),
            counters: HcCounters::default(),
        };
        (b, t)
    }

    const FILL: &[u8] = br#"{"type":"Fill","order_id":42,"fill_id":1,"symbol":"BTC-20261002-100000-C","side":"buy","price":"12.5","size":"0.1","timestamp":1,"fee":"0"}"#;

    #[test]
    fn a_fill_books_once_attributed_to_the_slot_and_the_member_id() {
        let (mut b, t) = book();
        b.rows[0] = Row {
            client_oid: 900,
            order_id: 42,
            sym: 513,
            qty_1e6: 100_000,
            filled_1e6: 0,
            buy: true,
            live: true,
        };
        b.on_payload(FILL, &t);
        b.on_payload(FILL, &t);
        assert_eq!(b.counters.fills_booked, 1);
        assert_eq!(b.counters.fills_dup, 1);
        let f = b.fills[0];
        assert_eq!((f.sym, f.strategy_id, f.order_id), (513, 7, 900));
        assert_eq!((f.px.raw(), f.qty.raw(), f.side), (12_500_000, 100_000, Side::Bid));
        assert_eq!(f.origin, core_types::FILL_ORIGIN_VENUE);
        assert_eq!(b.pos[0], 100_000);
    }

    #[test]
    fn an_update_that_ends_an_unfilled_order_retires_it() {
        let (mut b, t) = book();
        b.rows[3] = Row {
            client_oid: 5,
            order_id: 77,
            sym: 513,
            qty_1e6: 1_000_000,
            filled_1e6: 0,
            buy: false,
            live: true,
        };
        b.on_payload(br#"{"type":"OrderUpdate","order_id":77,"status":"PARTIALLY_FILLED","filled_size":"0.4"}"#, &t);
        assert!(b.rows[3].live);
        b.on_payload(br#"{"type":"OrderUpdate","order_id":77,"status":"CANCELED","filled_size":"0.4"}"#, &t);
        assert!(!b.rows[3].live);
        assert_eq!((b.r_len, b.retired[0]), (1, (5, 7)));
    }

    #[test]
    fn rows_are_reused_round_robin_so_late_fills_keep_their_member_id() {
        let (mut b, t) = book();
        let a = b.free_row().unwrap();
        b.rows[a] = Row { client_oid: 1, order_id: 11, sym: 513, qty_1e6: 1, filled_1e6: 1, buy: true, live: false };
        let next = b.free_row().unwrap();
        assert_ne!(next, a, "the order that just finished is the LAST row reused");
        b.on_payload(br#"{"type":"Fill","order_id":11,"fill_id":3,"symbol":"BTC-20261002-100000-C","side":"buy","price":"1","size":"0.000001","timestamp":1}"#, &t);
        assert_eq!(b.fills[0].order_id, 1, "the late fill still finds its member id");
        assert_eq!(b.counters.fills_unknown_order, 0);
    }

    #[test]
    fn an_error_on_the_stream_asks_for_a_reconnect() {
        let (mut b, t) = book();
        b.on_payload(br#"{"type":"Error","message":"subscription dropped"}"#, &t);
        assert!(b.ws_error);
    }

    #[test]
    fn only_an_explicit_not_found_reads_as_gone() {
        assert!(says_not_found(b"order not found"));
        assert!(says_not_found(b"ORDER_NOT_FOUND"));
        assert!(says_not_found(b"Unknown order id"));
        assert!(!says_not_found(b"nonce already used"));
        assert!(!says_not_found(b""));
    }

    #[test]
    fn unknown_symbols_and_orders_are_counted_not_guessed() {
        let (mut b, t) = book();
        b.on_payload(br#"{"type":"Fill","order_id":1,"fill_id":9,"symbol":"ETH-1","side":"sell","price":"1","size":"1","timestamp":1}"#, &t);
        assert_eq!((b.counters.fills_unknown_symbol, b.counters.fills_booked), (1, 0));
        b.on_payload(FILL, &t);
        assert_eq!((b.counters.fills_unknown_order, b.fills[0].order_id), (1, 0));
        b.on_payload(br#"{"type":"Fill","order_id":1}"#, &t);
        assert_eq!(b.counters.msgs_bad, 1);
    }
}
