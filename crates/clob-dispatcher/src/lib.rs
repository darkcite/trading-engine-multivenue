// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # clob-dispatcher
//!
//! HTTP/1.1 dispatcher to the Polymarket CLOB.
//!
//! ## Architecture
//!
//! * **[`PaperDispatcher`]** — never touches the network. Counts
//!   submissions and exits. Default in `--paper` mode (which is the
//!   only mode wired into the cli at this point).
//! * **[`LiveDispatcher`]** — opens a [`core_net::TlsTransport`] to
//!   the configured CLOB host, EIP-712-signs each order via
//!   `signer_eip712`, serialises the order + signature into JSON in
//!   a preallocated buffer, POSTs via the handwritten HTTP/1.1
//!   codec in `core_net::http1`, and parses the response with a
//!   zero-alloc scanner. Synchronous: the strategy's
//!   `ctx.submit` blocks for one network round-trip. Acceptable
//!   under the Phase 2 cooldown (250 ms per market).
//!
//! ## Why HTTP/1.1, not /2
//!
//! `hyper 1.x` is async-only and would drag `tokio` onto the
//! engine thread, violating the project's no-tokio rule. The CLOB
//! REST API speaks HTTP/1.1 happily; we already have a tested
//! zero-alloc HTTP/1.1 codec in `core-net`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod json_encoder;
pub mod live;
pub mod queued;
pub mod response;

pub use json_encoder::{encode_signed_order, JsonEncodeErr};
pub use live::{LiveDispatcher, LiveDispatcherErr, MAX_REQ_BODY, MAX_RESP_BUF};
pub use queued::{DispatcherWorker, QueuedDispatcher, ORDER_RING_CAP};
pub use response::{parse_clob_response, ClobResponse, ResponseScanErr};

use core_types::{
    CancelReq, Fill, ModifyReq, NsTs, Order, OrderIdentity, Price, Qty, Side, Tick,
};

/// Dispatcher error modes.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// Backpressure — the dispatcher's ring is full.
    QueueFull,
    /// Network went away; reconnect pending.
    Disconnected,
    /// Signer refused the order (invalid key, missing fields).
    SignerRejected,
    /// JSON encoder overflowed the request buffer.
    EncodeOverflow,
    /// Response wasn't HTTP/1.1 or didn't parse.
    JsonMalformed,
    /// Non-2xx response from the CLOB.
    Http(u16),
    /// E1: the order's strategy slot is `ExecMode::Off` — the operator
    /// has stopped that slot. Not an error condition; a refusal.
    SlotDisabled,
    /// E1: the order's strategy slot is `ExecMode::Live` but the order
    /// names a venue the slot has no live route to (or no live arm is
    /// compiled for it).
    ///
    /// **LAW E-1 — a live slot never falls back to paper.** This is
    /// the error that law is made of: the order is refused and
    /// counted, never handed to the paper matcher, because a modelled
    /// fill wearing live semantics would corrupt every downstream
    /// reader of the tape.
    NoLiveRoute,
    /// E5: this dispatcher does not implement the verb that was
    /// called. The default [`OrderDispatch::cancel`] and
    /// [`OrderDispatch::modify`] bodies return it, so a dispatcher
    /// that has not been taught to take an order back refuses loudly
    /// rather than returning `Ok` and doing nothing.
    Unsupported,
    /// E5: no resting order carries that client id. Already filled,
    /// already expired, or already taken back — a RACE, not a
    /// failure. Distinct from `Ok` because `Ok` means *this call*
    /// removed it, and a caller that cannot tell the two apart will
    /// believe its cancel beat a fill that beat it.
    NoSuchOrder,
    /// E5: a resting order of that slot DOES carry that client id,
    /// but the request describes a different order — see
    /// [`core_types::OrderIdentity`] for the five fields that make
    /// one. A modify may change price, size and client id; changing
    /// anything else is not a modify, and is refused rather than
    /// performed.
    ///
    /// The two verbs compare different subsets, because they assert
    /// different things. A **modify** carries a whole replacement
    /// `Order` and therefore asserts all five. A **cancel** carries
    /// no side and no kind, so it asserts `sym` and `venue` only —
    /// `strategy_id` is the lookup key rather than an assertion, and
    /// the two fields a `CancelReq` cannot name are not silently
    /// treated as claims it made.
    IdentityMismatch,
    /// E5: the order is not modellable as built — a non-positive
    /// price or size, a venue byte with no activation entry, or a
    /// `kind` that is neither maker nor IoC. The same condition the
    /// matcher's `unroutable` counter names.
    ///
    /// `submit` counts it and returns `Ok` (the INTENT is what the
    /// capture records, and the harness drops it the same way);
    /// `modify` returns it, because there `Ok` would claim a resting
    /// order had been repriced when it had not.
    ///
    /// (`Unroutable` continues below.)
    ///
    /// `modify` tests only the price/size half of that list, and that
    /// is sufficient rather than lazy: the identity check runs first,
    /// so the replacement's venue and `kind` are already pinned equal
    /// to the resting order's — which `submit` validated on the way
    /// in. Price and size are the only two fields a modify can make
    /// unmodellable.
    Unroutable,
    /// E5: MORE THAN ONE resting order answers to that client id
    /// within that strategy slot, so the request does not name a
    /// single order.
    ///
    /// **Not hypothetical.** Every member allocates `client_oid` from
    /// its own counter starting at 1, some reset it, and one rides a
    /// 14-bit sequence — so ids repeat. Slot scoping makes them
    /// unique in normal operation; this is what is returned when it
    /// does not. Refused rather than resolved FIFO, because taking
    /// back one of two identical quotes and reporting success leaves
    /// the caller believing both are gone.
    AmbiguousOrder,
    /// **E6: the risk gate refused it.** The request exceeded a clamp
    /// the OPERATOR set in `exec.toml`, checked before dispatch and
    /// independently of whatever the member believes about its own
    /// caps.
    ///
    /// Not an error condition; a refusal, like `SlotDisabled`. The
    /// order never left the process.
    RiskRefused,
}

/// Convert a `DispatchError` to the cross-crate
/// [`core_net::NetworkErr`] boundary type. `QueueFull` is *local*
/// back-pressure, not a network condition — it maps to
/// `NetworkErrKind::Io` with `NetworkSource::Clob` and code `0` so
/// the boundary type can carry it without inventing a new kind.
/// Callers that care about back-pressure specifically should still
/// match on `DispatchError::QueueFull` before the conversion.
impl From<DispatchError> for core_net::NetworkErr {
    fn from(e: DispatchError) -> Self {
        use core_net::{NetworkErr, NetworkErrKind, NetworkSource};
        match e {
            DispatchError::QueueFull => NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Io),
            DispatchError::Disconnected => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Disconnected)
            }
            DispatchError::SignerRejected => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Auth)
            }
            DispatchError::EncodeOverflow => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Malformed)
            }
            DispatchError::JsonMalformed => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Malformed)
            }
            DispatchError::Http(code) => {
                // 4xx → auth/malformed surface; 5xx → server-side
                // disconnect-equivalent (retryable). Tag the kind so
                // is_retryable returns the right answer at the
                // boundary too.
                let kind = if (400..500).contains(&code) {
                    NetworkErrKind::Auth
                } else if (500..600).contains(&code) {
                    NetworkErrKind::Disconnected
                } else {
                    NetworkErrKind::Malformed
                };
                NetworkErr::with_code(NetworkSource::Clob, kind, code)
            }
            // E1 routing refusals are LOCAL decisions, not network
            // conditions — the order never left the process. They map
            // to `Malformed` (the order was not addressable as sent)
            // rather than to `Disconnected`, so `is_retryable` at the
            // boundary answers "no": retrying a refused route just
            // refuses again.
            //
            // E5 adds three more local decisions to the same bucket:
            // an unimplemented verb, an id that is not resting, and a
            // request that describes a different order. None of them
            // touched a socket, and none of them becomes true by
            // being retried.
            DispatchError::SlotDisabled
            | DispatchError::NoLiveRoute
            | DispatchError::Unsupported
            | DispatchError::NoSuchOrder
            | DispatchError::IdentityMismatch
            | DispatchError::Unroutable
            | DispatchError::AmbiguousOrder
            | DispatchError::RiskRefused => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Malformed)
            }
        }
    }
}

/// Lock-free mirror of [`DispatchStats`] shared between the
/// queued dispatcher worker thread and the engine-side reader.
/// Each field is a `Relaxed` atomic — coherent per-field, not
/// across fields (a snapshot can see a partial accept/reject
/// imbalance of at most one). The cli's 5 s tick is the only
/// reader; the worker is the only writer.
#[derive(Debug, Default)]
pub struct DispatchStatsAtomic {
    pub(crate) accepted: std::sync::atomic::AtomicU64,
    pub(crate) rejected: std::sync::atomic::AtomicU64,
    pub(crate) rejected_queue_full: std::sync::atomic::AtomicU64,
    pub(crate) rejected_network: std::sync::atomic::AtomicU64,
    pub(crate) rejected_signer: std::sync::atomic::AtomicU64,
    pub(crate) rejected_encode: std::sync::atomic::AtomicU64,
    pub(crate) rejected_http_4xx: std::sync::atomic::AtomicU64,
    pub(crate) rejected_http_5xx: std::sync::atomic::AtomicU64,
    pub(crate) rejected_malformed: std::sync::atomic::AtomicU64,
    pub(crate) fills_seen: std::sync::atomic::AtomicU64,
    /// E1: mirrors [`DispatchStats::rejected_routing`].
    pub(crate) rejected_routing: std::sync::atomic::AtomicU64,
    /// E5: mirrors [`DispatchStats::rejected_lifecycle`].
    pub(crate) rejected_lifecycle: std::sync::atomic::AtomicU64,
}

impl DispatchStatsAtomic {
    /// Snapshot every field with `Relaxed` loads. Cheap (~10 ns
    /// total). Read coherence is per-field, not cross-field.
    pub fn snapshot(&self) -> DispatchStats {
        use std::sync::atomic::Ordering::Relaxed;
        DispatchStats {
            accepted: self.accepted.load(Relaxed),
            rejected: self.rejected.load(Relaxed),
            rejected_queue_full: self.rejected_queue_full.load(Relaxed),
            rejected_network: self.rejected_network.load(Relaxed),
            rejected_signer: self.rejected_signer.load(Relaxed),
            rejected_encode: self.rejected_encode.load(Relaxed),
            rejected_http_4xx: self.rejected_http_4xx.load(Relaxed),
            rejected_http_5xx: self.rejected_http_5xx.load(Relaxed),
            rejected_malformed: self.rejected_malformed.load(Relaxed),
            fills_seen: self.fills_seen.load(Relaxed),
            rejected_routing: self.rejected_routing.load(Relaxed),
            rejected_lifecycle: self.rejected_lifecycle.load(Relaxed),
        }
    }

    /// Worker-side bulk store from a freshly computed
    /// [`DispatchStats`]. Each field is a separate `Relaxed`
    /// store.
    pub fn store_from(&self, s: &DispatchStats) {
        use std::sync::atomic::Ordering::Relaxed;
        self.accepted.store(s.accepted, Relaxed);
        self.rejected.store(s.rejected, Relaxed);
        self.rejected_queue_full
            .store(s.rejected_queue_full, Relaxed);
        self.rejected_network.store(s.rejected_network, Relaxed);
        self.rejected_signer.store(s.rejected_signer, Relaxed);
        self.rejected_encode.store(s.rejected_encode, Relaxed);
        self.rejected_http_4xx.store(s.rejected_http_4xx, Relaxed);
        self.rejected_http_5xx.store(s.rejected_http_5xx, Relaxed);
        self.rejected_malformed.store(s.rejected_malformed, Relaxed);
        self.fills_seen.store(s.fills_seen, Relaxed);
        self.rejected_routing.store(s.rejected_routing, Relaxed);
        self.rejected_lifecycle
            .store(s.rejected_lifecycle, Relaxed);
    }
}

/// Aggregate counters exposed on `/metrics`. Rejection causes are
/// broken out so the operator can distinguish back-pressure (which
/// means tune the ring) from network errors (which mean a flaky
/// connection) from CLOB-side rejections (which mean a malformed
/// order or upstream degradation).
#[derive(Debug, Copy, Clone, Default)]
pub struct DispatchStats {
    /// Orders accepted by the CLOB (2xx + order_id).
    pub accepted: u64,
    /// Orders rejected — sum of the breakdown counters below. Kept
    /// for one-glance display; the breakdown is the actionable view.
    pub rejected: u64,
    /// Rejected because the dispatcher's queue was full
    /// (back-pressure).
    pub rejected_queue_full: u64,
    /// Rejected because of a network failure / TLS reset / DNS
    /// flake. Transient — strategy cooldown should not advance.
    pub rejected_network: u64,
    /// Rejected because the local signer refused the order.
    /// Indicates a strategy bug or bad key configuration.
    pub rejected_signer: u64,
    /// Rejected because the local JSON encoder couldn't fit the
    /// body. Indicates a strategy bug.
    pub rejected_encode: u64,
    /// CLOB returned HTTP 4xx (bad request, unauthorized, etc.).
    /// Usually a malformed order; strategy bug.
    pub rejected_http_4xx: u64,
    /// CLOB returned HTTP 5xx (server error). Upstream
    /// degradation; usually transient.
    pub rejected_http_5xx: u64,
    /// CLOB returned a 2xx but the body did not parse as JSON or
    /// did not carry an `orderID`/`error` field.
    pub rejected_malformed: u64,
    /// Fills observed.
    pub fills_seen: u64,
    /// E1: rejected by the execution router before any arm saw the
    /// order — the slot was `Off`, or it was `Live` with no route to
    /// the order's venue. Bucketed together rather than split across
    /// the network/signer/encode causes above, because a routing
    /// refusal is none of those things and counting it as one would
    /// send an operator looking at the wrong subsystem.
    pub rejected_routing: u64,
    /// E5: refusals of a LIFECYCLE verb — `cancel` or `modify`. The
    /// verb is not implemented by this dispatcher, the client id is
    /// not resting, the request describes a different order, or the
    /// replacement is not modellable.
    ///
    /// Kept apart from `rejected_routing` because a routing refusal
    /// means an order never left, while these mean an order that DID
    /// leave could not be acted on afterwards — an operator chasing
    /// the two looks in different places.
    pub rejected_lifecycle: u64,
}

impl DispatchStats {
    /// Increment the per-category counter for `e` and bump the
    /// aggregate `rejected`. Used by both `PaperDispatcher` (only
    /// QueueFull reachable) and `LiveDispatcher` (every variant).
    #[inline]
    pub fn record_rejection(&mut self, e: DispatchError) {
        match e {
            DispatchError::QueueFull => {
                self.rejected_queue_full = self.rejected_queue_full.wrapping_add(1);
            }
            DispatchError::Disconnected => {
                self.rejected_network = self.rejected_network.wrapping_add(1);
            }
            DispatchError::SignerRejected => {
                self.rejected_signer = self.rejected_signer.wrapping_add(1);
            }
            DispatchError::EncodeOverflow => {
                self.rejected_encode = self.rejected_encode.wrapping_add(1);
            }
            DispatchError::JsonMalformed => {
                self.rejected_malformed = self.rejected_malformed.wrapping_add(1);
            }
            DispatchError::Http(code) => {
                if (400..500).contains(&code) {
                    self.rejected_http_4xx = self.rejected_http_4xx.wrapping_add(1);
                } else if (500..600).contains(&code) {
                    self.rejected_http_5xx = self.rejected_http_5xx.wrapping_add(1);
                } else {
                    // Treat unexpected codes (1xx/3xx leaking
                    // through despite our 3xx rejection in
                    // read_response) as malformed.
                    self.rejected_malformed = self.rejected_malformed.wrapping_add(1);
                }
            }
            // E6's risk refusal joins the ROUTING bucket rather than
            // the lifecycle one: like `SlotDisabled`, it is a local
            // decision about whether this request may be dispatched at
            // all, taken before anything left.
            DispatchError::SlotDisabled
            | DispatchError::NoLiveRoute
            | DispatchError::RiskRefused => {
                self.rejected_routing = self.rejected_routing.wrapping_add(1);
            }
            // E5 lifecycle refusals. Listed one by one rather than
            // behind a `_` so that a SIXTH variant added later is a
            // compile error here — the whole point of this match is
            // that no error may reach `/metrics` without a category
            // somebody chose for it.
            DispatchError::Unsupported
            | DispatchError::NoSuchOrder
            | DispatchError::IdentityMismatch
            | DispatchError::Unroutable
            | DispatchError::AmbiguousOrder => {
                self.rejected_lifecycle = self.rejected_lifecycle.wrapping_add(1);
            }
        }
        self.rejected = self.rejected.wrapping_add(1);
    }

    /// E1 (plan §0.1-3): field-wise sum of two snapshots.
    ///
    /// `RoutedDispatcher` holds two arms and `/metrics` wants one
    /// number per counter, so the two are summed here rather than at
    /// each call site. Saturating, not wrapping: these are
    /// operator-facing totals and a counter that wrapped to zero
    /// during an incident is worse than one pinned at the maximum.
    ///
    /// **Cold path** — the 5 s metrics tick is the only caller.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            accepted: self.accepted.saturating_add(other.accepted),
            rejected: self.rejected.saturating_add(other.rejected),
            rejected_queue_full: self
                .rejected_queue_full
                .saturating_add(other.rejected_queue_full),
            rejected_network: self.rejected_network.saturating_add(other.rejected_network),
            rejected_signer: self.rejected_signer.saturating_add(other.rejected_signer),
            rejected_encode: self.rejected_encode.saturating_add(other.rejected_encode),
            rejected_http_4xx: self.rejected_http_4xx.saturating_add(other.rejected_http_4xx),
            rejected_http_5xx: self.rejected_http_5xx.saturating_add(other.rejected_http_5xx),
            rejected_malformed: self
                .rejected_malformed
                .saturating_add(other.rejected_malformed),
            fills_seen: self.fills_seen.saturating_add(other.fills_seen),
            rejected_routing: self.rejected_routing.saturating_add(other.rejected_routing),
            rejected_lifecycle: self
                .rejected_lifecycle
                .saturating_add(other.rejected_lifecycle),
        }
    }
}

/// Trait implemented by the real dispatcher AND the paper-mode stub.
/// Strategies don't implement this; the engine owns one.
pub trait OrderDispatch {
    /// Submit an order. Non-blocking for paper mode; one network
    /// round-trip in live mode.
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError>;

    /// **E5 — take one resting order back.**
    ///
    /// `Ok(())` means *this call* removed the order. `NoSuchOrder`
    /// means it was already gone. The difference is the whole point
    /// of the method returning a `Result` at all.
    ///
    /// Defaulted to [`DispatchError::Unsupported`] rather than
    /// `Ok(())`: a dispatcher that cannot cancel must say so, because
    /// a silent success here is a strategy believing a live quote was
    /// pulled while the venue still holds it.
    #[inline]
    fn cancel(&mut self, _req: &CancelReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    /// **E5, LAW E-7 — replace a resting order in place.**
    ///
    /// Price, size and client id may change; the five fields of
    /// [`core_types::OrderIdentity`] may not, and
    /// `req.order().ts_ns`/`ttl_ns` are ignored (the modified order
    /// keeps the original's expiry). Defaulted like `cancel`, for the
    /// same reason.
    #[inline]
    fn modify(&mut self, _req: &ModifyReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    /// Pop the next fill, if any.
    fn try_next_fill(&mut self) -> Option<Fill>;

    /// Snapshot of dispatch counters.
    fn stats(&self) -> DispatchStats;

    /// X1: show the dispatcher a market tick, so a PAPER one can judge
    /// its open orders against it.
    ///
    /// Defaulted to nothing, because a live dispatcher learns about
    /// fills from the venue and has no business inventing them from a
    /// book. Only [`PaperDispatcher`] overrides it.
    #[inline]
    fn observe_tick(&mut self, _tick: &Tick, _now_ns: NsTs) {}

    /// X1: what the paper matcher has done. All zeros for a dispatcher
    /// that does not model fills, which is how `/metrics` reads for a
    /// live boot.
    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        MatcherCounters::default()
    }

    /// X1: orders the paper matcher is holding; `0` when there is none.
    #[inline]
    fn open_paper_orders(&self) -> usize {
        0
    }

    /// E4: the worker has nothing to submit right now.
    ///
    /// Defaulted to nothing, like every other hook on this trait. A
    /// LIVE dispatcher overrides it, because it owns sockets the
    /// engine never touches — the venue's user-event stream, the
    /// reconciliation timer, the budget's state file — and those need
    /// a thread to run on. The worker's idle moment IS that thread.
    ///
    /// Why here rather than a second thread: the dispatcher worker
    /// already owns the venue relationship (HTTP, nonce, budget), and
    /// the fill lane needs exactly ONE writer. A second thread would
    /// need a lock around all of it, on the path that books fills.
    ///
    /// Called instead of sleeping, so an implementation that returns
    /// immediately must not spin — `DispatcherWorker::run` sleeps only
    /// when this reports it did nothing.
    ///
    /// **It also BLOCKS.** An implementation may poll a socket or
    /// fsync a state file here, on the same thread that submits
    /// orders. That is the right thread for it — the alternative is a
    /// lock on the fill path — but the idle wait is no longer tens of
    /// microseconds, and a caller that needs a bound must impose one.
    ///
    /// **WHERE IT DOES NOT REACH, TODAY.** `DispatcherWorker` is
    /// constructed in exactly one production place: the legacy
    /// Polymarket `--live` path. The `--exec` path hands its
    /// `RoutedDispatcher` straight to the engine loop, so nothing
    /// calls this hook there — which is the one path that can arm
    /// Hyperliquid. `RoutedDispatcher` forwards `on_idle` to both arms
    /// so the plumbing is ready, but the worker that would drive it is
    /// not wired on that path, and wiring it is an arming-path change
    /// that belongs with E7 rather than being inferred here.
    ///
    /// Returns whether it did any work.
    #[inline]
    fn on_idle(&mut self) -> bool {
        false
    }

    /// A venue event the dispatcher may need to act on.
    ///
    /// Defaulted to a no-op, like [`Self::on_idle`], because every
    /// dispatcher that models rather than trades ignores it. The one
    /// implementor is the Hyperliquid arm, which needs
    /// `ChannelId::InstrumentRoll` to bind its asset table — LAW E-4
    /// says an asset id is bound by a roll and never derived, and this
    /// is how the roll reaches the thing that binds.
    ///
    /// **Called BEFORE the strategy sees the same event**, and that
    /// ordering is load-bearing rather than incidental: a member handed
    /// a roll may submit into the new instance in the same call, and a
    /// dispatcher that had not yet bound would refuse the order it was
    /// just told how to route. `engine` pins the order with a test.
    ///
    /// On the engine thread, like `submit` — not the worker's. The
    /// `--exec` path has no worker (see [`Self::on_idle`]), so this is
    /// the only hook that actually reaches the live arm today.
    #[inline]
    fn on_venue_event(&mut self, _event: &core_types::ChannelEvent) {}

    /// E1: what the execution ROUTER did, when there is one.
    ///
    /// Defaulted to an unconfigured set — `configured == 0` — exactly
    /// as `matcher_counters` defaults to zeros for a dispatcher that
    /// models nothing. Only `exec_router::RoutedDispatcher` overrides
    /// it. The cli reads `configured` at boot to decide whether to
    /// register the `engine_exec_*` metric family at all, which is what
    /// keeps `/metrics` byte-identical on a boot with no `--exec`.
    #[inline]
    fn exec_counters(&self) -> ExecCounters {
        ExecCounters::default()
    }
}

/// Strategy slots [`ExecCounters`] reports on. Mirrors
/// `exec_router::EXEC_SLOTS`; the two are asserted equal in
/// `exec-router`'s tests so they cannot drift apart.
pub const EXEC_COUNTER_SLOTS: usize = 8;

/// E1: the execution router's counters, carried across the
/// `OrderDispatch` boundary so the engine loop can mirror them to
/// `/metrics` without knowing the router's concrete type.
///
/// Lives here rather than in `exec-router` because `exec-router`
/// depends on this crate; putting it the other way round would be a
/// dependency cycle. `MatcherCounters` sits here for the same reason.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecCounters {
    /// `0` = there is no router (the default). `1` = a router is in
    /// force. Nothing else distinguishes "router present, idle" from
    /// "no router" — and the difference decides whether `/metrics`
    /// grows an `engine_exec_*` family.
    pub configured: u8,
    /// Per-slot `ExecMode as u8`. Only meaningful when `configured`.
    pub modes: [u8; EXEC_COUNTER_SLOTS],
    /// Orders routed to the live arm.
    pub live_submits: u64,
    /// Orders routed to the paper matcher.
    pub paper_submits: u64,
    /// Refused because the slot is `Off`.
    pub refused_off: u64,
    /// Refused because a live slot named a venue it has no route to
    /// (LAW E-1 — refused, never downgraded to paper).
    pub refused_no_route: u64,
    /// **E6: refused by the risk gate** — the request's notional
    /// exceeded the slot's operator-set `max_order_usd`. A second
    /// opinion over the member's own caps; a non-zero value means the
    /// two disagreed.
    pub refused_risk: u64,
    /// Per-slot live submits.
    pub live_submits_by_slot: [u64; EXEC_COUNTER_SLOTS],
    /// Per-slot refusals (off + no-route).
    pub refused_by_slot: [u64; EXEC_COUNTER_SLOTS],
}

/// X1 counters — what the matcher did, mirrored to `/metrics` as
/// `engine_paper_matcher_*`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MatcherCounters {
    /// Orders accepted into the open table.
    pub intake: u64,
    /// Orders refused because a cap was already full. The submit still
    /// returns `Ok` — the INTENT was accepted for the capture, exactly
    /// as the harness counts `rejected_sym_cap` and drops the order.
    pub rejected_open_cap: u64,
    /// Orders refused as unmodellable: a non-tradeable venue byte, a
    /// non-positive price or size, or a `kind` that is neither maker
    /// nor IoC. Never guessed at.
    pub unroutable: u64,
    /// Modelled fills produced.
    pub fills: u64,
    /// IoCs that met their judgement tick and did not fill (or filled
    /// only part). **This is the F7 counter** — the VRP member's
    /// option entry lived here for two live campaigns while the member
    /// believed it held the position.
    pub ioc_canceled: u64,
    /// Orders canceled by the I1 TTL before any fill evidence.
    pub ttl_expired: u64,
    /// Fills dropped because the out ring was full between two pumps.
    /// Must stay 0: the engine pumps every iteration.
    pub out_overflow: u64,
    /// E5: resting orders this matcher took back on a `cancel`.
    /// Counts the ones it REMOVED — a cancel that found nothing is
    /// `no_such_order`, never this.
    pub cancels: u64,
    /// E5: resting orders repriced/resized in place on a `modify`.
    pub modifies: u64,
    /// E5: cancels and modifies naming a client id that is not
    /// resting. Expected to be non-zero in normal operation — it is
    /// the count of races lost to a fill or a TTL, not of errors.
    pub no_such_order: u64,
    /// E5: cancels and modifies whose request described a DIFFERENT
    /// order than the one resting under that slot's client id.
    /// Unlike `no_such_order` this one is a bug: nothing in normal
    /// operation changes an order's identity, so any non-zero value
    /// names a caller that built the wrong request.
    ///
    /// The lookup is scoped to the requesting SLOT, so another
    /// member's order carrying the same id reads as `no_such_order`
    /// (there is no order of yours with that id) and never as this.
    pub identity_mismatch: u64,
    /// E5: cancels and modifies naming an id that more than one of
    /// that slot's own resting orders answers to. **Must stay 0** —
    /// it means a member reused a client id while the first order was
    /// still resting, and until it does the slot cannot name its own
    /// orders.
    pub ambiguous_order: u64,
}

/// One order the paper matcher is holding.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct Pending {
    /// Emit sequence — FIFO fill priority, exactly as the harness's
    /// open table is emit-ordered by construction.
    seq: u64,
    /// Virtual activation: `submit + Δ_venue`.
    t_active_ns: u64,
    /// I1 expiry (`emit + ttl`); 0 = never. **A modify inherits
    /// this** — see [`PaperMatcher::modify`].
    expiry_ns: u64,
    px_1e6: i64,
    remaining_1e6: i64,
    client_oid: u64,
    /// The five fields a modify may not change. One value so the
    /// "is this the same order" test is one `==`.
    ident: OrderIdentity,
    /// MODEL venue byte — `symbol_venue_byte(sym)`, which indexes the
    /// activation table. NOT `ident.venue` (the [`core_types::VenueId`]
    /// the strategy addressed): the two are different namespaces and
    /// conflating them would index the activation table with a routing
    /// byte.
    model_venue: u8,
    /// Explicit tail padding — makes `Pending` exactly one cache line,
    /// so the matcher's scan loop touches one line per open order.
    _pad: [u8; 7],
}

const _: () = assert!(::core::mem::size_of::<Pending>() == 64);

/// What [`PaperMatcher::find_resting`] found. `Many` is a distinct
/// answer rather than "the first one", because taking back one of two
/// quotes that answer to the same name and reporting success leaves
/// the caller believing both are gone.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Resting {
    /// Exactly one — its index in the open table.
    One(usize),
    /// No resting order of that slot carries that id.
    None,
    /// Two or more do, so the request names no single order.
    Many,
}

const EMPTY_PENDING: Pending = Pending {
    seq: 0,
    t_active_ns: 0,
    expiry_ns: 0,
    px_1e6: 0,
    remaining_1e6: 0,
    client_oid: 0,
    ident: OrderIdentity {
        sym: core_types::SYMBOL_ID_NONE,
        venue: 0,
        strategy_id: core_types::STRATEGY_ID_NONE,
        side: Side::Bid,
        kind: core_fill::ORDER_KIND_MAKER,
    },
    model_venue: 0,
    _pad: [0; 7],
};

const EMPTY_FILL: Fill = Fill::new(
    0,
    core_types::SYMBOL_ID_NONE,
    Side::Bid,
    Price::from_raw(0),
    Qty::from_raw(0),
    0,
);

/// X1: the engine-side paper matcher.
///
/// Judges the orders a paper boot submits against the ticks the engine
/// is already receiving, using `core_fill` — the SAME law
/// `cli::backtest::fill` runs offline. Until this existed, members
/// inferred their positions from SUBMITS, and the VRP member spent two
/// live campaigns believing it held a hedged option while the harness,
/// replaying the same capture, held a naked perp.
///
/// Zero-alloc: two fixed arrays, `while`-index loops, no `dyn`, no
/// `String`. Boot-constructed once inside [`PaperDispatcher`].
#[repr(C, align(64))]
pub struct PaperMatcher {
    open: [Pending; core_fill::MAX_OPEN_TOTAL],
    open_len: usize,
    /// Fills waiting for [`Self::try_next_fill`], FIFO.
    out: [Fill; core_fill::MAX_OPEN_TOTAL],
    out_head: usize,
    out_len: usize,
    /// Activation Δ by model venue byte (index 7 is unused padding so
    /// a garbage byte cannot index out of bounds).
    activation_ns: [u64; 8],
    seq: u64,
    /// What the matcher did.
    pub counters: MatcherCounters,
}

impl Default for PaperMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PaperMatcher {
    /// A matcher with the MEASURED activation table
    /// ([`core_fill::ACTIVATION_NS_DEFAULT`]) and nothing open.
    #[must_use]
    pub const fn new() -> Self {
        let d = core_fill::ACTIVATION_NS_DEFAULT;
        Self {
            open: [EMPTY_PENDING; core_fill::MAX_OPEN_TOTAL],
            open_len: 0,
            out: [EMPTY_FILL; core_fill::MAX_OPEN_TOTAL],
            out_head: 0,
            out_len: 0,
            activation_ns: [d[0], d[1], d[2], d[3], d[4], d[5], d[6], 0],
            seq: 0,
            counters: MatcherCounters {
                intake: 0,
                rejected_open_cap: 0,
                unroutable: 0,
                fills: 0,
                ioc_canceled: 0,
                ttl_expired: 0,
                out_overflow: 0,
                cancels: 0,
                modifies: 0,
                no_such_order: 0,
                identity_mismatch: 0,
                ambiguous_order: 0,
            },
        }
    }

    /// Open orders held.
    #[inline]
    #[must_use]
    pub const fn open_len(&self) -> usize {
        self.open_len
    }

    /// Take an order into the open table.
    ///
    /// A cap refusal is COUNTED, not returned: the engine's contract is
    /// that a paper submit succeeds (the intent is what the capture
    /// records), and the harness does the same — it counts
    /// `rejected_sym_cap` and drops the order. A refusal that surfaced
    /// as `Err` would make a member treat a modelling limit as a venue
    /// rejection.
    pub fn submit(&mut self, order: &Order, now_ns: NsTs) {
        let venue = core_types::symbol_venue_byte(order.sym);
        let px = order.px.raw();
        let qty = order.qty.raw();
        if venue as usize >= core_fill::ACTIVATION_NS_DEFAULT.len()
            || px <= 0
            || qty <= 0
            || (order.kind != core_fill::ORDER_KIND_MAKER
                && order.kind != core_fill::ORDER_KIND_IOC)
        {
            self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
            return;
        }
        let mut sym_count = 0usize;
        let mut i = 0usize;
        while i < self.open_len {
            if self.open[i].ident.sym == order.sym {
                sym_count += 1;
            }
            i += 1;
        }
        if sym_count >= core_fill::MAX_OPEN_PER_SYM || self.open_len >= core_fill::MAX_OPEN_TOTAL {
            self.counters.rejected_open_cap = self.counters.rejected_open_cap.wrapping_add(1);
            return;
        }
        self.open[self.open_len] = Pending {
            seq: self.seq,
            t_active_ns: now_ns.saturating_add(self.activation_ns[venue as usize]),
            expiry_ns: core_fill::expiry_at(order.ts_ns, order.ttl_ns),
            px_1e6: px,
            remaining_1e6: qty,
            client_oid: order.client_oid,
            ident: OrderIdentity::of(order),
            model_venue: venue,
            _pad: [0; 7],
        };
        self.seq = self.seq.wrapping_add(1);
        self.open_len += 1;
        self.counters.intake = self.counters.intake.wrapping_add(1);
    }

    /// Which of `strategy_id`'s resting orders carries `client_oid`.
    ///
    /// **Keyed on `(client_oid, strategy_id)`, and NOT on the id
    /// alone.** A `client_oid` is unique only within the member that
    /// issued it: every member allocates from its own counter
    /// starting at 1, `strategy-xsd` resets its counter on a reset
    /// path that does not clear this table, and `strategy-bin15`
    /// rides a 14-bit sequence. All enabled members share ONE open
    /// table, so id 1 belongs to as many orders as there are members
    /// quoting. Searching by id alone would find another slot's order
    /// and report the caller's own correct cancel as a caller bug —
    /// and, worse, could take back a quote belonging to a different
    /// member.
    ///
    /// The slot is the NAMESPACE, not an assertion the caller is
    /// making; that is why it belongs in the key and the remaining
    /// identity fields do not. `sym` in the key would make a
    /// right-id/wrong-market request indistinguishable from one whose
    /// order already filled, and those two need opposite responses —
    /// so identity is checked *after* the lookup.
    ///
    /// Scans the whole table rather than returning the first hit,
    /// because "more than one" is its own answer
    /// ([`Resting::Many`]). At most [`core_fill::MAX_OPEN_TOTAL`]
    /// slots, on a path that runs per requote and not per tick.
    #[inline]
    fn find_resting(&self, client_oid: u64, strategy_id: u8) -> Resting {
        let mut found = Resting::None;
        let mut i = 0usize;
        while i < self.open_len {
            let o = self.open[i];
            if o.client_oid == client_oid && o.ident.strategy_id == strategy_id {
                found = match found {
                    Resting::None => Resting::One(i),
                    _ => return Resting::Many,
                };
            }
            i += 1;
        }
        found
    }

    /// **E5 — take one resting order back.**
    ///
    /// `Ok(())` means this call removed it. It does not mean "the
    /// order is not resting", which is the weaker fact and has its
    /// own name.
    ///
    /// No fill is produced and no counter of the fill family moves: a
    /// cancelled order simply stops existing, exactly as the I1 TTL
    /// sweep treats an expired one.
    pub fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        let i = match self.find_resting(req.client_oid, req.strategy_id) {
            Resting::One(i) => i,
            Resting::None => {
                self.counters.no_such_order = self.counters.no_such_order.wrapping_add(1);
                return Err(DispatchError::NoSuchOrder);
            }
            Resting::Many => {
                self.counters.ambiguous_order = self.counters.ambiguous_order.wrapping_add(1);
                return Err(DispatchError::AmbiguousOrder);
            }
        };
        // `strategy_id` was the lookup key, so two identity fields are
        // left for the cancel to assert: `sym` and `venue`. It carries
        // no side and no kind, so those two are NOT compared — a
        // cancel does not claim anything about them, and pretending it
        // did would refuse correct requests.
        let id = self.open[i].ident;
        if id.sym != req.sym || id.venue != req.venue {
            self.counters.identity_mismatch = self.counters.identity_mismatch.wrapping_add(1);
            return Err(DispatchError::IdentityMismatch);
        }
        self.remove_open(i);
        self.counters.cancels = self.counters.cancels.wrapping_add(1);
        Ok(())
    }

    /// **E5, LAW E-7 — replace a resting order in place.**
    ///
    /// ## What changes, and what does not
    ///
    /// Changed: `px_1e6`, `remaining_1e6`, `client_oid`.
    ///
    /// Kept: `expiry_ns` and `seq`. `seq` because the open array is
    /// itself the FIFO (`remove_open` shifts to preserve it) and
    /// `core_fill` models no queue at all, so there is no priority to
    /// lose or gain. `expiry_ns` because **a reprice must not extend a
    /// quote's life**: an Arm B that repriced every 333 ms could
    /// otherwise hold a quote forever past the TTL its ruleset set.
    /// `req.order().ttl_ns` is therefore read for nothing — but
    /// `req.order().ts_ns` is the caller's decision clock and IS read,
    /// by [`PaperDispatcher::modify`], as this function's `now_ns`.
    ///
    /// Re-armed: `t_active_ns`, to `now + Δ_venue`.
    ///
    /// ## Why `t_active_ns` is re-armed and not preserved
    ///
    /// Δ is the measured time an instruction takes to reach the
    /// venue, and it is why an order cannot fill on a tick that
    /// arrived before it. The new price is an instruction like any
    /// other: it is not at the venue for Δ. Preserving `t_active_ns`
    /// would let a modify fill at a price the venue had not yet been
    /// told about — a fabricated fill, which is the one class of
    /// error this matcher exists to prevent.
    ///
    /// The cost of the conservative choice is that the OLD price,
    /// which really is still resting during the flight window, cannot
    /// fill either — so the model under-fills a modify by at most one
    /// Δ. Under-filling is recoverable; inventing a fill is not.
    pub fn modify(&mut self, req: &ModifyReq, now_ns: NsTs) -> Result<(), DispatchError> {
        let i = match self.find_resting(req.prev_client_oid(), req.order().strategy_id) {
            Resting::One(i) => i,
            Resting::None => {
                self.counters.no_such_order = self.counters.no_such_order.wrapping_add(1);
                return Err(DispatchError::NoSuchOrder);
            }
            Resting::Many => {
                self.counters.ambiguous_order = self.counters.ambiguous_order.wrapping_add(1);
                return Err(DispatchError::AmbiguousOrder);
            }
        };
        if self.open[i].ident != req.identity() {
            self.counters.identity_mismatch = self.counters.identity_mismatch.wrapping_add(1);
            return Err(DispatchError::IdentityMismatch);
        }
        let px = req.order().px.raw();
        let qty = req.order().qty.raw();
        if px <= 0 || qty <= 0 {
            // Same bar as `submit`: a non-positive price or size is
            // not modellable. Same counter, same name — but returned
            // rather than swallowed, because the resting order is
            // still there at its old price and a caller told `Ok`
            // would believe otherwise.
            self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
            return Err(DispatchError::Unroutable);
        }
        let venue = self.open[i].model_venue;
        self.open[i].px_1e6 = px;
        self.open[i].remaining_1e6 = qty;
        self.open[i].client_oid = req.order().client_oid;
        self.open[i].t_active_ns = now_ns.saturating_add(self.activation_ns[venue as usize]);
        self.counters.modifies = self.counters.modifies.wrapping_add(1);
        Ok(())
    }

    /// Judge every open order of this tick's sym against it.
    ///
    /// Order of business, mirroring the harness exactly: the I1 TTL
    /// sweep FIRST (expiry is a clock fact, so it runs on stale and
    /// one-sided ticks too, and a bar that has closed cannot fill into
    /// the next one), then — only on fresh two-sided evidence — the
    /// fill pass in emit order over one shared displayed-size budget.
    pub fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        let sym = tick.sym;
        let mut i = 0usize;
        while i < self.open_len {
            if self.open[i].ident.sym == sym
                && core_fill::expired_at(now_ns, self.open[i].expiry_ns)
            {
                self.counters.ttl_expired = self.counters.ttl_expired.wrapping_add(1);
                self.remove_open(i);
                continue;
            }
            i += 1;
        }
        if !core_fill::is_fill_evidence(tick) {
            return;
        }
        let touch = core_fill::Touch::of(tick);
        let mut ask_budget = touch.ask_qty_1e6;
        let mut bid_budget = touch.bid_qty_1e6;
        let mut i = 0usize;
        while i < self.open_len {
            let o = self.open[i];
            if o.ident.sym != sym || now_ns < o.t_active_ns {
                i += 1;
                continue;
            }
            if o.ident.kind == core_fill::ORDER_KIND_IOC {
                match core_fill::judge_ioc(
                    o.ident.side,
                    o.px_1e6,
                    o.remaining_1e6,
                    touch,
                    &mut ask_budget,
                    &mut bid_budget,
                ) {
                    core_fill::Verdict::Fill { px_1e6, qty_1e6 } => {
                        self.push_fill(&o, px_1e6, qty_1e6, now_ns);
                    }
                    _ => {
                        self.counters.ioc_canceled = self.counters.ioc_canceled.wrapping_add(1);
                    }
                }
                self.remove_open(i);
                continue;
            }
            match core_fill::judge_maker(
                o.ident.side,
                o.px_1e6,
                o.remaining_1e6,
                touch,
                &mut ask_budget,
                &mut bid_budget,
            ) {
                core_fill::Verdict::Fill { px_1e6, qty_1e6 } => {
                    self.push_fill(&o, px_1e6, qty_1e6, now_ns);
                    let remaining = o.remaining_1e6 - qty_1e6;
                    if remaining > 0 {
                        self.open[i].remaining_1e6 = remaining;
                        i += 1;
                    } else {
                        self.remove_open(i);
                    }
                }
                _ => i += 1,
            }
        }
    }

    /// Pop the next modelled fill, FIFO.
    pub fn try_next_fill(&mut self) -> Option<Fill> {
        if self.out_len == 0 {
            return None;
        }
        let f = self.out[self.out_head];
        self.out_head = (self.out_head + 1) % core_fill::MAX_OPEN_TOTAL;
        self.out_len -= 1;
        Some(f)
    }

    #[inline]
    fn push_fill(&mut self, o: &Pending, px_1e6: i64, qty_1e6: i64, now_ns: NsTs) {
        if self.out_len >= core_fill::MAX_OPEN_TOTAL {
            // The engine pumps every iteration and one tick can produce
            // at most MAX_OPEN_TOTAL fills, so this is unreachable —
            // which is exactly why it is counted rather than trusted.
            debug_assert!(false, "paper matcher out ring overflowed");
            self.counters.out_overflow = self.counters.out_overflow.wrapping_add(1);
            return;
        }
        let slot = (self.out_head + self.out_len) % core_fill::MAX_OPEN_TOTAL;
        self.out[slot] = Fill::new(
            now_ns,
            o.ident.sym,
            o.ident.side,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            o.client_oid,
        )
        .with_attribution(o.ident.strategy_id, core_types::FILL_ORIGIN_PAPER);
        self.out_len += 1;
        self.counters.fills = self.counters.fills.wrapping_add(1);
    }

    /// Remove open slot `i`, shifting the tail left so emit order — the
    /// FIFO priority — is preserved.
    #[inline]
    fn remove_open(&mut self, i: usize) {
        let mut k = i;
        while k + 1 < self.open_len {
            self.open[k] = self.open[k + 1];
            k += 1;
        }
        self.open_len -= 1;
    }
}

/// Paper-mode dispatcher — records submissions, never calls out, and
/// (X1) MODELS their fills through `core_fill`.
pub struct PaperDispatcher {
    stats: DispatchStats,
    /// X1: the matcher. Inline rather than boxed — the dispatcher is
    /// itself boot-constructed once by the engine, and ~8 KiB of
    /// fixed arrays inside it is the same memory either way.
    matcher: PaperMatcher,
}

impl Default for PaperDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PaperDispatcher {
    /// X1: what the matcher has done.
    #[inline]
    #[must_use]
    pub const fn matcher_counters(&self) -> MatcherCounters {
        self.matcher.counters
    }

    /// X1: orders the matcher is currently holding.
    #[inline]
    #[must_use]
    pub const fn open_orders(&self) -> usize {
        self.matcher.open_len()
    }

    /// Construct empty.
    pub const fn new() -> Self {
        Self {
            matcher: PaperMatcher::new(),
            stats: DispatchStats {
                accepted: 0,
                rejected: 0,
                rejected_queue_full: 0,
                rejected_network: 0,
                rejected_signer: 0,
                rejected_encode: 0,
                rejected_http_4xx: 0,
                rejected_http_5xx: 0,
                rejected_malformed: 0,
                fills_seen: 0,
                rejected_routing: 0,
                rejected_lifecycle: 0,
            },
        }
    }
}

impl OrderDispatch for PaperDispatcher {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.stats.accepted = self.stats.accepted.wrapping_add(1);
        // X1: the order goes into the matcher's open table. `Ok` either
        // way — a modelling cap is not a venue rejection, and the
        // INTENT is what the capture records.
        self.matcher.submit(order, order.ts_ns);
        Ok(())
    }

    /// E5 — straight through to the matcher. **No `stats.accepted`
    /// bump**: `accepted` counts orders that entered the book, and a
    /// cancel takes one out. Counting a cancel as an acceptance would
    /// make `/metrics` show two orders where one was placed and
    /// pulled.
    #[inline]
    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        self.matcher.cancel(req)
    }

    /// E5 — likewise. A modify replaces an order rather than adding
    /// one, so `accepted` does not move here either; the matcher's
    /// own `modifies` counter is what records it.
    ///
    /// `now_ns` comes from the replacement's `ts_ns` — the same
    /// source `submit` uses, so a replayed boot judges a modify
    /// against the same clock it judges a submit against.
    ///
    /// **That makes `ts_ns` load-bearing on a modify**, unlike the
    /// genuinely ignored `ttl_ns`. A replacement carrying a stale or
    /// zero `ts_ns` re-arms the activation delta into the past and is
    /// fillable at the NEW price on the next tick. This is the same
    /// contract `submit` has always had — a submit with a stale
    /// `ts_ns` activates early in exactly the same way — so it is a
    /// property of the clock argument, not a hazard E5 introduced. It
    /// is written down here because the first caller will be a
    /// requote loop, which is precisely where a reused or forgotten
    /// timestamp is easy to write.
    #[inline]
    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        self.matcher.modify(req, req.order().ts_ns)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        let f = self.matcher.try_next_fill();
        if f.is_some() {
            self.stats.fills_seen = self.stats.fills_seen.wrapping_add(1);
        }
        f
    }

    fn stats(&self) -> DispatchStats {
        self.stats
    }

    #[inline]
    fn observe_tick(&mut self, tick: &Tick, now_ns: NsTs) {
        self.matcher.observe_tick(tick, now_ns);
    }

    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        self.matcher.counters
    }

    #[inline]
    fn open_paper_orders(&self) -> usize {
        self.matcher.open_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, Side, SymbolId, VenueId};

    // ---------------- X1: the paper matcher ----------------

    const DERIBIT_PERP: SymbolId = 0x0300_0001;

    fn mk_order(side: Side, kind: u8, px: i64, qty: i64, oid: u64, ttl_ns: u64) -> Order {
        let mut o = Order::new(
            1_000,
            VenueId::Deribit,
            DERIBIT_PERP,
            side,
            kind,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.ttl_ns = ttl_ns;
        o.strategy_id = 1;
        o
    }

    fn mk_tick(bid: i64, bid_q: i64, ask: i64, ask_q: i64) -> Tick {
        Tick::new(
            1_000,
            VenueId::Deribit,
            DERIBIT_PERP,
            0,
            Price::from_raw(bid),
            Qty::from_raw(bid_q),
            Price::from_raw(ask),
            Qty::from_raw(ask_q),
        )
    }

    /// Δ_deribit is 220 ms: an order cannot fill on a tick that arrives
    /// before it could have reached the venue.
    const AFTER_DELTA: NsTs = 1_000 + 300_000_000;

    #[test]
    fn a_marketable_ioc_fills_at_the_touch_and_is_attributed() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 1_000_000, 77, 0), 1_000);
        assert_eq!(m.counters.intake, 1);
        assert_eq!(m.open_len(), 1);
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000), AFTER_DELTA);
        let f = m.try_next_fill().expect("a fill");
        assert_eq!(f.px.raw(), 100_000_000, "the ASK, not our limit");
        assert_eq!(f.qty.raw(), 1_000_000);
        assert_eq!(f.order_id, 77, "the client_oid comes back");
        assert_eq!(f.sym, DERIBIT_PERP);
        assert_eq!(f.side, Side::Bid);
        assert_eq!(f.strategy_id, 1, "routed to the member that asked");
        assert_eq!(f.origin, core_types::FILL_ORIGIN_PAPER, "MODELLED, not real");
        assert!(m.try_next_fill().is_none(), "one order, one fill");
        assert_eq!(m.open_len(), 0, "an IoC never rests");
        assert_eq!(m.counters.fills, 1);
        assert_eq!(m.counters.ioc_canceled, 0);
    }

    /// THE F7 CASE, end to end through the matcher: a mid-priced IoC on
    /// a real spread produces NOTHING, and the member finds out only by
    /// absence. This is what the VRP member's option entry did twice,
    /// live, while it believed it held the position.
    #[test]
    fn a_mid_priced_ioc_on_a_real_spread_produces_no_fill_at_all() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 100_000_000, 1_000_000, 5, 0), 1_000);
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 101_000_000, 5_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_none(), "nothing filled");
        assert_eq!(m.counters.ioc_canceled, 1);
        assert_eq!(m.counters.fills, 0);
        assert_eq!(m.open_len(), 0, "and it is gone — judged ONCE");
    }

    #[test]
    fn nothing_fills_before_the_venue_could_have_seen_it() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 1_000_000, 5, 0), 1_000);
        // Δ_deribit = 220 ms; this tick is 100 ms after the submit.
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000), 1_000 + 100_000_000);
        assert!(m.try_next_fill().is_none());
        assert_eq!(m.open_len(), 1, "still waiting for activation");
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_some());
    }

    #[test]
    fn a_stale_or_one_sided_tick_is_not_evidence() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 1_000_000, 5, 0), 1_000);
        let mut stale = mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000);
        stale.flags |= core_types::TICK_FLAG_STALE;
        m.observe_tick(&stale, AFTER_DELTA);
        assert_eq!(m.open_len(), 1, "VT4: a stale book fills nothing");
        m.observe_tick(&mk_tick(0, 0, 100_000_000, 5_000_000), AFTER_DELTA);
        assert_eq!(m.open_len(), 1, "nor a one-sided one");
        assert!(m.try_next_fill().is_none());
    }

    #[test]
    fn a_maker_rests_until_it_is_strictly_crossed_and_may_fill_in_parts() {
        let mut m = PaperMatcher::new();
        m.submit(
            &mk_order(Side::Bid, core_fill::ORDER_KIND_MAKER, 100_000_000, 1_000_000, 9, 0),
            1_000,
        );
        // At the touch: a queue, not a fill.
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_none());
        assert_eq!(m.open_len(), 1, "a maker rests");
        // Strictly through it, but only 300k displayed.
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 99_000_000, 300_000), AFTER_DELTA);
        let f = m.try_next_fill().expect("a partial");
        assert_eq!(f.px.raw(), 100_000_000, "at OUR limit — we were crossed");
        assert_eq!(f.qty.raw(), 300_000);
        assert_eq!(m.open_len(), 1, "the remainder keeps resting");
        // The rest, next tick.
        m.observe_tick(&mk_tick(99_000_000, 5_000_000, 99_000_000, 5_000_000), AFTER_DELTA);
        assert_eq!(m.try_next_fill().expect("the rest").qty.raw(), 700_000);
        assert_eq!(m.open_len(), 0);
    }

    #[test]
    fn the_ttl_cancels_before_the_tick_can_fill_it() {
        let mut m = PaperMatcher::new();
        // Emitted at ts_ns 1_000 with a 1 s TTL.
        m.submit(
            &mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 1_000_000, 3, 1_000_000_000),
            1_000,
        );
        // A tick that WOULD fill it, arriving after the bar closed.
        m.observe_tick(
            &mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000),
            1_000 + 1_000_000_000,
        );
        assert!(m.try_next_fill().is_none(), "the bar had closed");
        assert_eq!(m.counters.ttl_expired, 1);
        assert_eq!(m.counters.ioc_canceled, 0, "expiry is not a cancel");
        assert_eq!(m.open_len(), 0);
    }

    #[test]
    fn the_caps_refuse_without_failing_the_submit() {
        let mut m = PaperMatcher::new();
        let mut i = 0u64;
        while i < core_fill::MAX_OPEN_PER_SYM as u64 {
            m.submit(
                &mk_order(Side::Bid, core_fill::ORDER_KIND_MAKER, 1_000_000, 1_000_000, i + 1, 0),
                1_000,
            );
            i += 1;
        }
        assert_eq!(m.open_len(), core_fill::MAX_OPEN_PER_SYM);
        m.submit(
            &mk_order(Side::Bid, core_fill::ORDER_KIND_MAKER, 1_000_000, 1_000_000, 99, 0),
            1_000,
        );
        assert_eq!(m.counters.rejected_open_cap, 1);
        assert_eq!(m.open_len(), core_fill::MAX_OPEN_PER_SYM, "not taken");
        assert_eq!(m.counters.intake, core_fill::MAX_OPEN_PER_SYM as u64);
    }

    #[test]
    fn an_unmodellable_order_is_counted_never_guessed_at() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 0, 1_000_000, 1, 0), 1_000);
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 1_000_000, 0, 2, 0), 1_000);
        m.submit(&mk_order(Side::Bid, 2, 1_000_000, 1_000_000, 3, 0), 1_000);
        assert_eq!(m.counters.unroutable, 3);
        assert_eq!(m.open_len(), 0);
        assert_eq!(m.counters.intake, 0);
    }

    /// One tick's displayed size cannot fill two orders twice, and the
    /// order it fills them in is the order they were submitted in.
    #[test]
    fn two_orders_share_one_displayed_size_in_emit_order() {
        let mut m = PaperMatcher::new();
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 600_000, 1, 0), 1_000);
        m.submit(&mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 600_000, 2, 0), 1_000);
        m.observe_tick(&mk_tick(99_000_000, 1_000_000, 100_000_000, 1_000_000), AFTER_DELTA);
        let a = m.try_next_fill().expect("first");
        let b = m.try_next_fill().expect("second");
        assert_eq!(a.order_id, 1, "FIFO by emit order");
        assert_eq!(a.qty.raw(), 600_000);
        assert_eq!(b.order_id, 2);
        assert_eq!(b.qty.raw(), 400_000, "only what was left of the size");
        assert!(m.try_next_fill().is_none());
    }

    #[test]
    fn the_dispatcher_models_fills_and_counts_them_as_seen() {
        let mut d = PaperDispatcher::new();
        let o = mk_order(Side::Bid, core_fill::ORDER_KIND_IOC, 101_000_000, 1_000_000, 42, 0);
        d.submit(&o).expect("paper submits always succeed");
        assert_eq!(d.stats().accepted, 1);
        assert_eq!(d.open_orders(), 1);
        assert!(d.try_next_fill().is_none(), "no tick, no fill");
        d.observe_tick(&mk_tick(99_000_000, 5_000_000, 100_000_000, 5_000_000), AFTER_DELTA);
        let f = d.try_next_fill().expect("modelled");
        assert_eq!(f.order_id, 42);
        assert_eq!(d.stats().fills_seen, 1);
        assert_eq!(d.matcher_counters().fills, 1);
        assert_eq!(d.open_orders(), 0);
    }

    #[test]
    fn paper_dispatcher_counts_submissions() {
        let mut d = PaperDispatcher::new();
        let o = Order::new(
            0,
            VenueId::Polymarket,
            1,
            Side::Bid,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            0,
        );
        d.submit(&o).unwrap();
        d.submit(&o).unwrap();
        assert_eq!(d.stats().accepted, 2);
    }

    #[test]
    fn record_rejection_routes_each_variant_to_its_bucket() {
        let mut s = DispatchStats::default();
        s.record_rejection(DispatchError::QueueFull);
        s.record_rejection(DispatchError::Disconnected);
        s.record_rejection(DispatchError::SignerRejected);
        s.record_rejection(DispatchError::EncodeOverflow);
        s.record_rejection(DispatchError::JsonMalformed);
        s.record_rejection(DispatchError::Http(404));
        s.record_rejection(DispatchError::Http(500));
        s.record_rejection(DispatchError::Http(599));
        s.record_rejection(DispatchError::Http(200)); // unexpected non-4xx/5xx
        assert_eq!(s.rejected_queue_full, 1);
        assert_eq!(s.rejected_network, 1);
        assert_eq!(s.rejected_signer, 1);
        assert_eq!(s.rejected_encode, 1);
        assert_eq!(s.rejected_http_4xx, 1);
        assert_eq!(s.rejected_http_5xx, 2);
        // JsonMalformed + the 200 fallback both go to malformed.
        assert_eq!(s.rejected_malformed, 2);
        // Aggregate matches sum.
        assert_eq!(s.rejected, 9);
    }

    #[test]
    fn dispatch_error_maps_to_network_err_with_clob_source() {
        use core_net::{NetworkErr, NetworkErrKind, NetworkSource};
        let e: NetworkErr = DispatchError::Disconnected.into();
        assert_eq!(e.source, NetworkSource::Clob);
        assert_eq!(e.kind, NetworkErrKind::Disconnected);
        assert_eq!(e.code, 0);
        assert!(e.is_retryable());

        let e: NetworkErr = DispatchError::SignerRejected.into();
        assert_eq!(e.kind, NetworkErrKind::Auth);
        assert!(!e.is_retryable());

        let e: NetworkErr = DispatchError::Http(401).into();
        assert_eq!(e.code, 401);
        assert_eq!(e.kind, NetworkErrKind::Auth);
        assert!(!e.is_retryable());

        let e: NetworkErr = DispatchError::Http(503).into();
        assert_eq!(e.code, 503);
        assert_eq!(e.kind, NetworkErrKind::Disconnected);
        assert!(e.is_retryable(), "5xx must be retryable at boundary");

        let e: NetworkErr = DispatchError::QueueFull.into();
        // QueueFull is local back-pressure; the boundary tag is
        // Clob/Io but is_retryable is false (Io is the bucket we
        // park it in, not a true retryable condition).
        assert_eq!(e.kind, NetworkErrKind::Io);
        assert!(e.is_retryable(), "Io is in the retryable set");
    }

    // ---------------- E5: cancel and modify ----------------

    /// Δ_deribit is 220 ms. A second window past it, for the tests
    /// that need "now" to be strictly after a re-armed activation.
    const AFTER_TWO_DELTAS: NsTs = AFTER_DELTA + 300_000_000;

    fn maker(px: i64, qty: i64, oid: u64, ttl_ns: u64) -> Order {
        mk_order(Side::Bid, core_fill::ORDER_KIND_MAKER, px, qty, oid, ttl_ns)
    }

    /// A tick whose ASK is below `px`, so a bid at `px` strictly
    /// crosses it and the maker law produces a fill.
    fn crossing_tick(ask: i64) -> Tick {
        mk_tick(ask - 2_000_000, 5_000_000, ask, 5_000_000)
    }

    #[test]
    fn a_cancel_removes_the_resting_order_and_says_that_it_did() {
        let mut m = PaperMatcher::new();
        let o = maker(100_000_000, 1_000_000, 42, 0);
        m.submit(&o, 1_000);
        assert_eq!(m.open_len(), 1);
        assert_eq!(m.cancel(&CancelReq::of(&o, 2_000)), Ok(()));
        assert_eq!(m.open_len(), 0, "the order is gone");
        assert_eq!(m.counters.cancels, 1);
        assert_eq!(m.counters.no_such_order, 0);
        // And it cannot fill afterwards, which is the point.
        m.observe_tick(&crossing_tick(99_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_none(), "a cancelled order cannot fill");
    }

    /// **The load-bearing one.** A fill beat the cancel. `Ok` here
    /// would tell the strategy its own cancel pulled the quote, and
    /// the strategy would go on believing it has no position — while
    /// the fill it already booked says otherwise.
    #[test]
    fn a_cancel_that_a_fill_beat_is_a_race_and_never_reports_success() {
        let mut m = PaperMatcher::new();
        let o = maker(100_000_000, 1_000_000, 7, 0);
        m.submit(&o, 1_000);
        m.observe_tick(&crossing_tick(99_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_some(), "it filled first");
        assert_eq!(m.open_len(), 0);

        assert_eq!(
            m.cancel(&CancelReq::of(&o, AFTER_DELTA)),
            Err(DispatchError::NoSuchOrder),
            "the cancel did NOT remove it — the fill did"
        );
        assert_eq!(m.counters.cancels, 0, "nothing was cancelled");
        assert_eq!(m.counters.no_such_order, 1);
    }

    #[test]
    fn a_cancel_naming_the_right_id_on_the_wrong_market_is_refused_not_performed() {
        let mut m = PaperMatcher::new();
        let o = maker(100_000_000, 1_000_000, 9, 0);
        m.submit(&o, 1_000);
        let mut bad = CancelReq::of(&o, 2_000);
        bad.sym = 0x0300_0002; // same venue, different instrument
        assert_eq!(m.cancel(&bad), Err(DispatchError::IdentityMismatch));
        assert_eq!(m.open_len(), 1, "the real order is untouched");
        assert_eq!(m.counters.identity_mismatch, 1);
        assert_eq!(m.counters.cancels, 0);
        assert_eq!(
            m.counters.no_such_order, 0,
            "this is a caller bug, not a lost race — different names"
        );
    }

    #[test]
    fn a_modify_changes_price_size_and_id_and_nothing_else() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        let mut new = maker(97_000_000, 2_000_000, 2, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(m.modify(&ModifyReq::new(1, new), AFTER_DELTA), Ok(()));
        assert_eq!(m.open_len(), 1, "replaced in place, not added");
        assert_eq!(m.counters.modifies, 1);
        assert_eq!(m.counters.intake, 1, "a modify is not an intake");

        // It now fills at the NEW price and the NEW size, under the
        // NEW id — after the re-armed delta.
        m.observe_tick(&crossing_tick(96_000_000), AFTER_TWO_DELTAS);
        let f = m.try_next_fill().expect("the repriced order fills");
        assert_eq!(f.order_id, 2, "the new client id books the fill");
        assert_eq!(
            f.px.raw(),
            97_000_000,
            "a maker fills at its OWN price — and 97M is the new one, \
             so the reprice really landed"
        );
        assert_eq!(f.qty.raw(), 2_000_000, "the new size");
    }

    /// **The ruling.** A strategy that reprices every 333 ms must not
    /// be able to keep a quote alive past the TTL its ruleset gave
    /// it: the modify inherits the ORIGINAL expiry and the
    /// replacement's own `ttl_ns` is read for nothing.
    #[test]
    fn a_modify_inherits_the_original_expiry_and_cannot_extend_a_quotes_life() {
        let mut m = PaperMatcher::new();
        // Submitted at ts 1_000 with a 400 ms TTL → expires at
        // 400_001_000, which AFTER_DELTA (300_001_000) has not
        // reached but AFTER_TWO_DELTAS (600_001_000) has.
        let old = maker(100_000_000, 1_000_000, 1, 400_000_000);
        m.submit(&old, 1_000);
        // Reprice with a ONE HOUR ttl on the replacement.
        let mut new = maker(97_000_000, 1_000_000, 2, 3_600_000_000_000);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(m.modify(&ModifyReq::new(1, new), AFTER_DELTA), Ok(()));

        // A stale tick past the ORIGINAL expiry. The TTL sweep runs on
        // it because expiry is a clock fact.
        m.observe_tick(&mk_tick(0, 0, 0, 0), AFTER_TWO_DELTAS);
        assert_eq!(m.open_len(), 0, "the original TTL still fired");
        assert_eq!(m.counters.ttl_expired, 1);
        assert!(m.try_next_fill().is_none());
    }

    /// The conservative half of the modify model: the new price is an
    /// instruction like any other and is not at the venue for Δ.
    /// Filling it sooner would be a fabricated fill.
    #[test]
    fn a_modify_cannot_fill_at_the_new_price_before_the_venue_could_know_it() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        let mut new = maker(97_000_000, 1_000_000, 2, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(m.modify(&ModifyReq::new(1, new), AFTER_DELTA), Ok(()));

        // One nanosecond after the modify: the venue cannot have it.
        m.observe_tick(&crossing_tick(96_000_000), AFTER_DELTA + 1);
        assert!(
            m.try_next_fill().is_none(),
            "a modify that filled instantly would be inventing a fill"
        );
        assert_eq!(m.open_len(), 1, "still resting, still waiting on delta");

        // And once the delta has passed, it does fill.
        m.observe_tick(&crossing_tick(96_000_000), AFTER_TWO_DELTAS);
        assert!(m.try_next_fill().is_some());
    }

    /// Four of the five identity fields are ASSERTIONS the modify
    /// makes about the order it names, and changing one is refused.
    /// (The fifth, `strategy_id`, is the lookup key — see the test
    /// below it.)
    #[test]
    fn a_modify_that_changes_identity_is_refused_and_leaves_the_order_alone() {
        for mutate in [0u8, 1, 2, 3] {
            let mut m = PaperMatcher::new();
            let old = maker(100_000_000, 1_000_000, 1, 0);
            m.submit(&old, 1_000);
            let mut new = maker(97_000_000, 1_000_000, 2, 0);
            match mutate {
                0 => new.sym = 0x0300_0002,
                1 => new.venue = VenueId::Binance as u8,
                2 => new.side = Side::Ask,
                _ => new.kind = core_fill::ORDER_KIND_IOC,
            }
            assert_eq!(
                m.modify(&ModifyReq::new(1, new), AFTER_DELTA),
                Err(DispatchError::IdentityMismatch),
                "identity field {mutate} must not be changeable by a modify"
            );
            assert_eq!(m.open_len(), 1);
            assert_eq!(m.counters.modifies, 0);
            assert_eq!(m.counters.identity_mismatch, 1);
        }
    }

    /// `strategy_id` is the NAMESPACE the id lives in, so a modify
    /// naming a different slot is not asking about this order at all
    /// — it is asking about an id that slot does not hold. The answer
    /// is `NoSuchOrder`, and `identity_mismatch` stays 0 so its
    /// must-stay-0 contract survives ordinary multi-member operation.
    #[test]
    fn a_modify_that_names_another_slot_is_looking_in_another_namespace() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        let mut new = maker(97_000_000, 1_000_000, 2, 0);
        new.strategy_id = 5;
        assert_eq!(
            m.modify(&ModifyReq::new(1, new), AFTER_DELTA),
            Err(DispatchError::NoSuchOrder)
        );
        assert_eq!(m.counters.no_such_order, 1);
        assert_eq!(m.counters.identity_mismatch, 0);
        assert_eq!(m.open_len(), 1);
    }

    #[test]
    fn a_modify_of_an_order_that_already_filled_is_a_race_not_a_reprice() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        m.observe_tick(&crossing_tick(99_000_000), AFTER_DELTA);
        assert!(m.try_next_fill().is_some());
        let mut new = maker(97_000_000, 1_000_000, 2, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(
            m.modify(&ModifyReq::new(1, new), AFTER_DELTA),
            Err(DispatchError::NoSuchOrder)
        );
        assert_eq!(m.counters.modifies, 0);
        assert_eq!(m.counters.no_such_order, 1);
    }

    #[test]
    fn a_modify_to_a_non_positive_size_is_refused_and_the_old_price_still_rests() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        let mut new = maker(97_000_000, 0, 2, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(
            m.modify(&ModifyReq::new(1, new), AFTER_DELTA),
            Err(DispatchError::Unroutable)
        );
        assert_eq!(m.counters.unroutable, 1);
        assert_eq!(m.counters.modifies, 0);
        // The ORIGINAL price is still the resting one — `Ok` here
        // would have told the caller otherwise.
        m.observe_tick(&crossing_tick(99_000_000), AFTER_DELTA);
        let f = m.try_next_fill().expect("the original order is intact");
        assert_eq!(f.order_id, 1, "still the old id");
        assert_eq!(f.qty.raw(), 1_000_000, "still the old size");
    }

    /// A dispatcher that has not been taught the verbs must SAY so.
    /// `Ok(())` as a default would be a strategy believing a quote
    /// was pulled that nothing ever pulled.
    #[test]
    fn the_default_lifecycle_verbs_refuse_rather_than_silently_succeed() {
        struct Deaf;
        impl OrderDispatch for Deaf {
            fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
                Ok(())
            }
            fn try_next_fill(&mut self) -> Option<Fill> {
                None
            }
            fn stats(&self) -> DispatchStats {
                DispatchStats::default()
            }
        }
        let mut d = Deaf;
        let o = maker(100_000_000, 1_000_000, 1, 0);
        assert_eq!(
            d.cancel(&CancelReq::of(&o, 1_000)),
            Err(DispatchError::Unsupported)
        );
        assert_eq!(
            d.modify(&ModifyReq::new(1, o)),
            Err(DispatchError::Unsupported)
        );
    }

    /// A lifecycle refusal is its own `DispatchStats` category. Mixing
    /// it into `rejected_routing` would send an operator looking at
    /// the route table for an order that was routed fine and then
    /// could not be acted on.
    #[test]
    fn a_lifecycle_refusal_is_counted_apart_from_a_routing_refusal() {
        let mut s = DispatchStats::default();
        s.record_rejection(DispatchError::NoLiveRoute);
        s.record_rejection(DispatchError::NoSuchOrder);
        s.record_rejection(DispatchError::IdentityMismatch);
        s.record_rejection(DispatchError::Unsupported);
        s.record_rejection(DispatchError::Unroutable);
        assert_eq!(s.rejected_routing, 1);
        assert_eq!(s.rejected_lifecycle, 4);
        assert_eq!(s.rejected, 5);
    }

    /// The paper dispatcher's `accepted` counts orders that entered
    /// the book. A cancel takes one out and a modify replaces one, so
    /// neither may bump it — `/metrics` would otherwise report two
    /// orders where one was placed and pulled.
    #[test]
    fn a_cancel_or_modify_does_not_count_as_an_accepted_order() {
        let mut d = PaperDispatcher::new();
        let o = maker(100_000_000, 1_000_000, 1, 0);
        assert!(d.submit(&o).is_ok());
        assert_eq!(d.stats().accepted, 1);
        let mut new = maker(97_000_000, 1_000_000, 2, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(d.modify(&ModifyReq::new(1, new)), Ok(()));
        assert_eq!(d.stats().accepted, 1, "a modify is not an acceptance");
        let mut c = CancelReq::of(&o, AFTER_DELTA);
        c.client_oid = 2;
        assert_eq!(d.cancel(&c), Ok(()));
        assert_eq!(d.stats().accepted, 1, "nor is a cancel");
        assert_eq!(d.open_paper_orders(), 0);
    }

    /// **The multi-member case, which the first cut got wrong.**
    ///
    /// Every member allocates `client_oid` from its own counter
    /// starting at 1, and all enabled members share ONE open table.
    /// So id 1 belongs to as many resting orders as there are members
    /// quoting. A lookup keyed on the id alone finds whichever
    /// member submitted first — reporting one member's correct cancel
    /// as a caller bug, and, on a matching identity, taking back the
    /// wrong member's quote.
    #[test]
    fn one_slots_cancel_cannot_touch_another_slots_order_of_the_same_id() {
        let mut m = PaperMatcher::new();
        let mut a = maker(100_000_000, 1_000_000, 1, 0);
        a.strategy_id = 0;
        let mut b = maker(101_000_000, 1_000_000, 1, 0); // SAME client id
        b.strategy_id = 5;
        m.submit(&a, 1_000);
        m.submit(&b, 1_000);
        assert_eq!(m.open_len(), 2);

        // Slot 5 cancels ITS id 1. Slot 0's order must survive.
        assert_eq!(m.cancel(&CancelReq::of(&b, 2_000)), Ok(()));
        assert_eq!(m.open_len(), 1);
        m.observe_tick(&crossing_tick(99_000_000), AFTER_DELTA);
        let f = m.try_next_fill().expect("slot 0's order is still resting");
        assert_eq!(f.strategy_id, 0, "the WRONG member's quote was pulled");
        assert_eq!(m.counters.identity_mismatch, 0, "no caller was wrong here");
    }

    /// The other half: a slot that names an id only ANOTHER slot
    /// holds is told the order is not there — which is true of its
    /// own orders — and not that it built a bad request.
    #[test]
    fn an_id_that_only_another_slot_holds_reads_as_no_such_order() {
        let mut m = PaperMatcher::new();
        let mut a = maker(100_000_000, 1_000_000, 1, 0);
        a.strategy_id = 0;
        m.submit(&a, 1_000);
        let mut c = CancelReq::of(&a, 2_000);
        c.strategy_id = 5;
        assert_eq!(m.cancel(&c), Err(DispatchError::NoSuchOrder));
        assert_eq!(m.counters.no_such_order, 1);
        assert_eq!(
            m.counters.identity_mismatch, 0,
            "must-stay-0 has to survive ordinary multi-member operation"
        );
        assert_eq!(m.open_len(), 1);
    }

    /// A member that reuses a client id while the first order is
    /// still resting cannot name either of them. Refused rather than
    /// resolved FIFO: removing one and reporting success would leave
    /// the caller believing both were gone.
    #[test]
    fn a_reused_client_id_within_one_slot_is_ambiguous_not_first_wins() {
        let mut m = PaperMatcher::new();
        let a = maker(100_000_000, 1_000_000, 1, 0);
        let b = maker(101_000_000, 1_000_000, 1, 0); // same slot, same id
        m.submit(&a, 1_000);
        m.submit(&b, 1_000);
        assert_eq!(m.open_len(), 2);
        assert_eq!(
            m.cancel(&CancelReq::of(&a, 2_000)),
            Err(DispatchError::AmbiguousOrder)
        );
        let mut new = maker(97_000_000, 1_000_000, 9, 0);
        new.ts_ns = AFTER_DELTA;
        assert_eq!(
            m.modify(&ModifyReq::new(1, new), AFTER_DELTA),
            Err(DispatchError::AmbiguousOrder)
        );
        assert_eq!(m.counters.ambiguous_order, 2);
        assert_eq!(m.open_len(), 2, "neither was touched");
    }

    /// **`ts_ns` is the modify's decision clock**, not the ignored
    /// bookkeeping field `ttl_ns` is. Pinned because three doc sites
    /// once said both were "read for nothing", and a requote loop
    /// that believed it would re-arm the activation delta into the
    /// past — making the repriced order fillable at the NEW price on
    /// the very next tick.
    #[test]
    fn a_modify_whose_ts_ns_is_stale_activates_early_which_is_why_it_is_not_ignored() {
        let mut m = PaperMatcher::new();
        let old = maker(100_000_000, 1_000_000, 1, 0);
        m.submit(&old, 1_000);
        let mut new = maker(97_000_000, 1_000_000, 2, 0);
        new.ts_ns = 0; // the mistake this test exists to make visible
        // The dispatcher passes `req.order().ts_ns` as the clock.
        assert_eq!(m.modify(&ModifyReq::new(1, new), new.ts_ns), Ok(()));
        // 0 + Δ is long past, so the reprice is live immediately.
        m.observe_tick(&crossing_tick(96_000_000), AFTER_DELTA);
        let f = m
            .try_next_fill()
            .expect("a stale ts_ns makes the reprice fill at once");
        assert_eq!(f.px.raw(), 97_000_000, "at the NEW price, with no flight time");

        // And with the clock stamped correctly it does NOT.
        let mut m2 = PaperMatcher::new();
        m2.submit(&old, 1_000);
        let mut good = maker(97_000_000, 1_000_000, 2, 0);
        good.ts_ns = AFTER_DELTA;
        assert_eq!(m2.modify(&ModifyReq::new(1, good), good.ts_ns), Ok(()));
        m2.observe_tick(&crossing_tick(96_000_000), AFTER_DELTA + 1);
        assert!(m2.try_next_fill().is_none());
    }

    /// `Pending` is exactly one cache line and its identity is one
    /// comparable value — both are load-bearing for the matcher's
    /// scan loop and for there being ONE statement of what "the same
    /// order" means.
    #[test]
    fn the_open_table_slot_is_one_cache_line_with_one_identity_value() {
        assert_eq!(::core::mem::size_of::<Pending>(), 64);
        assert_eq!(::core::mem::size_of::<OrderIdentity>(), 8);
        let a = maker(1, 1, 1, 0);
        let mut b = maker(2, 2, 2, 0); // different px, qty, oid
        assert_eq!(
            OrderIdentity::of(&a),
            OrderIdentity::of(&b),
            "price, size and id are NOT identity"
        );
        b.side = Side::Ask;
        assert_ne!(OrderIdentity::of(&a), OrderIdentity::of(&b));
    }
}
