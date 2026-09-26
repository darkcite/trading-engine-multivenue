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
    CancelReq, Fill, ModifyReq, NsTs, Order, OrderIdentity, Price, Qty, Side, SymbolId, Tick,
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
    /// aggregate `rejected`. Used by `PaperDispatcher` (QueueFull on a
    /// submit; the lifecycle variants on a cancel or modify the matcher
    /// refused) and `LiveDispatcher` (every variant).
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

    /// **HYPARB L5 — an accepted order that ended without a (further)
    /// fill**, as `(client_oid, slot)`: a swap that reverted or was
    /// never mined, the unfilled remainder of an IoC. The router
    /// retires its resting row, or `max_open_orders` would count
    /// orders that no longer exist and stall the slot. Defaulted to
    /// none: an arm whose orders end only by fill or by the member's
    /// own cancel has nothing to say here.
    #[inline]
    fn try_next_retired(&mut self) -> Option<(u64, u8)> {
        None
    }

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

    /// HYPARB H2: show the dispatcher one pool-event signal (the HyperEVM
    /// ingress's `Signal`: `sym` = the pool or `SYMBOL_ID_NONE`, the
    /// 40-byte `core_amm::payload`), so a PAPER one can keep pool state
    /// and judge its open AMM swaps on each new head.
    ///
    /// Defaulted to nothing for the same reason as [`Self::observe_tick`]:
    /// a live dispatcher learns about fills from the chain, never from a
    /// curve. Only [`PaperDispatcher`] overrides it.
    #[inline]
    fn observe_amm(&mut self, _sym: SymbolId, _payload: &[u8; 40], _now_ns: NsTs) {}

    /// XMM XH2: show the dispatcher one trade print, so a PAPER one can
    /// judge its post-only makers on a queue venue by the queue law
    /// (`core_fill::queue`): prints consume the queue ahead, then fill.
    ///
    /// Defaulted to nothing for the reason [`Self::observe_tick`] is.
    #[inline]
    fn observe_trade(&mut self, _print: &core_types::TradePrint, _now_ns: NsTs) {}

    /// XMM XH2: write the next order event (`RESTING`, `REJECTED`,
    /// `CANCELED`, `FILLED`) about an order this dispatcher models into
    /// `out` and say so — into the caller's storage rather than returned
    /// inside an `Option` (128 B by value; `OrderEvent` has no niche), the
    /// `TradePrint::read_trade_event` shape. The paper arm emits the
    /// events the live gateway (XH4) will, so a member's order state
    /// machine runs the same in both. Defaulted to none: an arm with no
    /// order events has nothing to say.
    #[inline]
    fn try_next_order_event(&mut self, _out: &mut core_types::OrderEvent) -> bool {
        false
    }

    /// XMM XH2: track the touch of `sym` for the queue law, so the first
    /// post-only order placed on it meets a known book. Boot calls it
    /// for every instrument a queue member may trade. Defaulted to
    /// nothing: a live arm learns its queue from the venue.
    #[inline]
    fn track_queue_sym(&mut self, _sym: SymbolId) {}

    /// X1: what the paper matcher has done. All zeros for a dispatcher
    /// that does not model fills, which is how `/metrics` reads for a
    /// live boot.
    // COPY: `MatcherCounters` (176 B, size-pinned) crosses the trait by
    // value on the 5 s metrics cadence only — cold, and a reference
    // would pin the matcher's borrow across the mirror.
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
    /// **WHO DRIVES IT.** Two drivers, one per boot mode, never both:
    /// the legacy Polymarket `--live` path through `DispatcherWorker`
    /// on its own thread, and — since E6 commit 3a — the engine loop
    /// itself (`Engine::drive_dispatcher_idle`, paced by
    /// `cli::paper::IdlePacer` at "a quiet tick, or 2 ms") on the
    /// `--exec` path, where `RoutedDispatcher` forwards it to both
    /// arms. An earlier version of this doc said nothing called it on
    /// `--exec`; that was true until 3a and is a safety claim now, so
    /// it is corrected rather than left.
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

    /// **E6 — a fill was BOOKED into the engine.**
    ///
    /// Defaulted to a no-op like every other hook here. The one
    /// implementor is `exec_router::RoutedDispatcher`, whose exposure
    /// ledger has to count what the venue actually filled.
    ///
    /// ## Why this hook has to exist at all
    ///
    /// The router cannot learn about a live fill any other way.
    /// [`Self::on_venue_event`] carries a `ChannelEvent`, which is
    /// market data and never a fill. [`Self::try_next_fill`] carries
    /// the PAPER arm's fills only — a venue fill never passes through
    /// it, because the live arm pushes into the engine's own fill lane
    /// 3 and the engine drains that lane directly (see
    /// `exec_router::routed`'s module note, "Where live fills come
    /// from"). So the component that refuses orders against an
    /// exposure cap sat downstream of nothing that could tell it a
    /// position had changed.
    ///
    /// ## What the caller must guarantee
    ///
    /// **Called once per fill, on the engine thread, BEFORE the
    /// strategy sees the same fill** — the same ordering
    /// [`Self::observe_tick`] and [`Self::on_venue_event`] have, and
    /// load-bearing for the same reason: a member handed a fill may
    /// submit in the same call, and a ledger that had not yet booked
    /// it would size the refusal against a stale position. `engine`
    /// pins the order with a test.
    ///
    /// **Both drains, both origins.** The engine calls this from the
    /// fill-lane drain AND from the dispatcher fill pump, and passes
    /// paper fills as readily as venue ones. Filtering on
    /// [`core_types::FILL_ORIGIN_VENUE`] is the LEDGER's job and is
    /// done in one place there, so that a paper fill which somehow
    /// reached lane 3 cannot inflate a live exposure number.
    #[inline]
    fn on_fill_booked(&mut self, _fill: &Fill) {}

    /// **E6 commit 3 — what the venue relationship looks like from
    /// inside the arm.**
    ///
    /// Defaulted to an all-zero signal, which trips nothing: a
    /// dispatcher that models rather than trades has no venue to be
    /// unhappy about. The one implementor is the Hyperliquid arm.
    ///
    /// Three of E6's halt triggers — the request-budget floor,
    /// reconciliation drift and the user-event stream gap — are
    /// visible ONLY inside the arm, which owns the sockets. The
    /// component that has to REFUSE is the router. This is how the
    /// first reaches the second: a `Copy` POD the router polls, on
    /// the same thread, with no lock and no cross-arm reach — the
    /// same shape [`Self::exec_counters`] and
    /// [`Self::matcher_counters`] already have.
    ///
    /// Polled, not pushed, because a push would mean the arm deciding
    /// policy. The arm reports what it saw; the router owns the
    /// thresholds and the latch.
    #[inline]
    fn halt_signal(&self) -> HaltSignal {
        HaltSignal::default()
    }

    /// **HYPARB L4 — the signal for ONE live slot.**
    ///
    /// The router polls this once per live slot and judges each slot
    /// only against the arm that trades it: with two live arms behind
    /// one router (`exec_router::SlotSplit`), slot 0's reconciler, its
    /// stream and its P&L bound must never halt slot 3, nor the
    /// reverse. An arm that serves every slot from one venue
    /// relationship answers its one signal for all of them — the
    /// default, and exactly the behaviour before L4.
    #[inline]
    fn halt_signal_for(&self, _slot: u8) -> HaltSignal {
        self.halt_signal()
    }

    /// **E6 commit 3 — REQUEST that every order come off the venue.**
    ///
    /// This is a request, not a confirmation. [`Self::cancel_all_state`]
    /// is the confirmation, and the two are separate methods because
    /// on a real venue they are separated by minutes: this arm queues
    /// a sweep per live leg, and the sweep drains over many idle
    /// moments, asking the venue what is resting and cancelling by
    /// oid.
    ///
    /// Collapsing the two — reading `Ok(())` as "the venue is clear"
    /// — is a fail-OPEN error, and one this lane made: the caller
    /// stops retrying and zeroes its resting count while the orders
    /// are still working.
    ///
    /// Defaulted to `Ok(())` — a paper matcher's orders are not on
    /// any venue, so there is nothing to take back and reporting
    /// failure would halt a boot that has nothing at risk.
    ///
    /// `Err` means the request itself did not land. The caller halts
    /// anyway — waiting for a successful cancel before refusing would
    /// keep submitting into the condition that tripped the halt — and
    /// asks again from the idle path.
    ///
    /// # Errors
    /// Whatever stopped the arm accepting the request.
    #[inline]
    fn cancel_all(&mut self) -> Result<(), DispatchError> {
        Ok(())
    }

    /// **Is the venue actually clear of this arm's orders?**
    ///
    /// Polled after [`Self::cancel_all`] until it answers
    /// [`CancelAllState::Clear`]. Three-valued rather than a `bool`
    /// for the same reason the ledger's lookup is: the caller's
    /// response to "still working" and to "gave up" are opposite —
    /// wait, versus ask again — and a `bool` would force one of them
    /// to be guessed.
    ///
    /// Defaulted to `Clear`: an arm that cancels synchronously really
    /// is clear by the time `cancel_all` has returned.
    #[inline]
    fn cancel_all_state(&self) -> CancelAllState {
        CancelAllState::Clear
    }

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

    /// **E7: what the LIVE ARM did**, when there is one.
    ///
    /// The exchange arm's own counters — fills booked and refused,
    /// reconciliations, sweeps, the address budget's headroom — used
    /// to reach nothing: `stats()` returned a default and every one
    /// of them was a unit-test fact. Four of the five blocking
    /// findings of the E7 review were invisible for exactly that
    /// reason. The router forwards its live arm's answer; a paper
    /// dispatcher reports the zero set.
    #[inline]
    fn arm_counters(&self) -> LiveArmCounters {
        LiveArmCounters::default()
    }

    /// **S7-L1 — the engine is stopping: take every resting order of
    /// ours off the venue, NOW.**
    ///
    /// Called once, from `Engine::stop`, after the members' `on_stop`.
    /// Blocking and bounded — unlike [`Self::cancel_all`], which queues
    /// work for idle moments that at shutdown will not come. A restart
    /// boots every member flat: a paper matcher's resting orders die
    /// with its process, and a live quote left on the venue would fill
    /// into a position no member of the next process knows it has.
    ///
    /// Defaulted to a no-op: a dispatcher that models rather than
    /// trades has nothing resting anywhere but in its own memory.
    #[inline]
    fn on_shutdown(&mut self) {}

    /// **S7-L1 — what the VENUE says `slot` bought today**:
    /// `(UTC day number, filled BUY notional since that day's 00:00Z,
    /// USD ×1e6)`, from the venue's own fill history, or `None` until
    /// the arm has read it.
    ///
    /// The router adopts it into its day cap (never lowering it), which
    /// is what makes `cap_day_usd_1e6` survive a restart: the router's
    /// own count starts at zero on every boot, the venue's does not.
    /// The day number is `wall_ms / 86 400 000` — the ledger's day
    /// epoch. Defaulted to `None`: a paper matcher has no history
    /// outside its own memory.
    #[inline]
    fn venue_day_bought(&self, _slot: usize) -> Option<(u64, i64)> {
        None
    }
}

/// **E7 — the live arm's operator numbers**, carried across the
/// `OrderDispatch` boundary the same way [`ExecCounters`] is. A
/// zeroed set means "no live arm". Cold: read once a second for
/// `/state` and every 5 s for `/metrics`, so it crosses by value.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveArmCounters {
    /// Orders the venue accepted (resting or filled).
    pub submitted: u64,
    /// Orders the venue understood and refused.
    pub rejected: u64,
    /// IoCs the venue understood and could not match (E7-F2) — not
    /// refusals, not in the reject streak.
    pub ioc_missed: u64,
    /// Submits refused locally before any packet left.
    pub refused_local: u64,
    /// The LAW E-4 subset of `refused_local`: the order named an
    /// instance the table has rolled past.
    pub refused_stale: u64,
    /// Actions that reached the wire and whose answer was never read.
    /// Non-zero = there may be an order at the venue no local book
    /// has an id for; reconciliation is what finds it.
    pub sent_unanswered: u64,
    /// Fills that reached fill lane 3.
    pub fills_booked: u64,
    /// Fills whose coin no roll had bound (LAW E-4 on the fill path).
    pub fills_unresolved: u64,
    /// Fills whose cloid was not ours.
    pub fills_foreign: u64,
    /// Fills the lane could not take. **A position the engine does
    /// not know it has.**
    pub fills_dropped: u64,
    /// Rows refused by the converter (sign, scale).
    pub fills_refused: u64,
    /// `userFills` frames that did not scan — a dropped snapshot.
    pub fills_scan_failed: u64,
    /// Settlements on a leg no member traded.
    pub fills_unowned: u64,
    /// Reconciliations that parsed.
    pub recon_ok: u64,
    /// Reconciliations that did not (transport or parse).
    pub recon_failed: u64,
    /// Legs that disagreed at the last comparison.
    pub recon_drift_legs: u64,
    /// Venue-held outcome legs the last comparison never looked at.
    pub recon_unseen_legs: u64,
    /// LAW E-8 sweeps abandoned with orders possibly still resting.
    pub sweep_left: u64,
    /// Sweeps that stopped shrinking and were force-terminated.
    pub sweep_stalled: u64,
    /// Legs a cancel-all could not queue.
    pub cancel_all_unqueued: u64,
    /// User-event socket reconnects.
    pub ws_reconnects: u64,
    /// User-event socket connect failures.
    pub ws_connect_failures: u64,
    /// Roll events that bound both legs.
    pub rolls_bound: u64,
    /// Roll events refused (malformed, no room, no outcome).
    pub rolls_refused: u64,
    /// Symbols two slots both traded — a configuration error.
    pub owner_contested: u64,
    /// The address request budget's remaining headroom, per the
    /// governor. Negative = past the venue's cliff.
    pub budget_remaining: i64,
    /// **E7 session bound** — the equity AT COST (spot USDC plus the
    /// held legs' cost basis, USD ×1e6) the session's P&L is measured
    /// from: the account at its first reconciliation, persisted across
    /// restarts. `0` = not anchored yet.
    pub pnl_anchor_usd_1e6: i64,
    /// **E7 session bound** — the equity at cost minus the anchor at
    /// the last reconciliation, USD ×1e6, signed. A LEVEL; `0` while
    /// not anchored. The number `halt_on_gain_usd_1e6` /
    /// `halt_on_loss_usd_1e6` are judged against — legs held or not
    /// (S7-L1).
    pub session_pnl_usd_1e6: i64,
    /// **S7-L1** — cancels the account-wide sweep (boot and shutdown)
    /// had the venue confirm. A total.
    pub sweep_all_cancelled: u64,
    /// **S7-L1** — orders of ours the last account-wide sweep could
    /// not confirm gone. A level; `u64::MAX` = the venue's open orders
    /// could not be read, which is NOT clear.
    pub sweep_all_left: u64,
    /// **S7-L1** — request-weight top-ups the venue accepted.
    pub topup_ok: u64,
    /// **S7-L1** — request-weight top-up checks that failed.
    pub topup_failed: u64,
    /// **S7-L1** — day-spend reads (`userFillsByTime`) that parsed.
    pub day_sync_ok: u64,
    /// **S7-L1** — day-spend reads that did not. While the first read
    /// of a boot keeps failing, a slot under restart safety stays
    /// unseeded and refuses every live place.
    pub day_sync_failed: u64,
}

// S7-L1: both counter blocks cross the `OrderDispatch` boundary BY
// VALUE (`// COPY:` at `exec_router::RoutedDispatcher::exec_counters`),
// and the byte bounds those comments state have drifted twice. Pinned.
const _: () = assert!(core::mem::size_of::<LiveArmCounters>() == 272);
const _: () = assert!(core::mem::size_of::<ExecCounters>() == 568);

/// **What the venue has told us about a requested cancel-all.**
///
/// `OrderDispatch::cancel_all` asks; this answers. The distinction is
/// not pedantry: on Hyperliquid a cancel-all becomes one queued sweep
/// per live leg, each of which asks the venue what is resting and
/// cancels by oid, over many idle moments. There is a long interval
/// during which the request has been accepted and the orders are
/// still working.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CancelAllState {
    /// **The venue said nothing of ours is resting.** The only value
    /// that entitles a caller to act as though its orders are gone.
    #[default]
    Clear = 0,
    /// A sweep is in flight. The arm is still working on it, so the
    /// caller waits rather than asking again — the sweep already
    /// running IS the retry.
    Working = 1,
    /// **The arm has stopped, and the venue was never confirmed
    /// clear.** A sweep spent its retries and was abandoned, or a leg
    /// could not be queued at all. The caller must ask again; a fresh
    /// request gives the leg fresh retries.
    ///
    /// Never `Clear` by omission: a leg nobody swept and nobody
    /// confirmed is exactly the stranded quote LAW E-8 exists to
    /// prevent, and reporting it as clear is how it would become
    /// invisible.
    Stranded = 2,
}

/// **E6 commit 3 — the venue relationship, as the arm sees it.**
///
/// Every field is a RAW OBSERVATION, never a decision: the arm
/// reports, the router owns the thresholds. That split is what lets
/// two slots with different `halt_on_*` numbers reach different
/// conclusions from one signal.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HaltSignal {
    /// Nanoseconds since the venue's user-event stream was last known
    /// alive. `0` means "no observation" — a boot that has not
    /// connected yet, or an arm with no stream — and trips nothing,
    /// because an arm that has never been up is not an arm that has
    /// gone quiet.
    ///
    /// **LAW E-5: the WS stream is the FILL.** A gap is not a quiet
    /// market; it is the arm trading with no idea what has filled.
    pub ws_gap_ns: u64,
    /// Worst reconciliation drift observed, USD ×1e6, as
    /// `exec_hyperliquid::recon::drift_qty_to_usd_1e6` measures it.
    /// Not a running total: the worst single disagreement.
    pub recon_drift_usd_1e6: i64,
    /// CONSECUTIVE orders the venue understood and refused. Reset by
    /// an acceptance. Distinct from a total: a venue refusing every
    /// order is a different fact from a venue that has refused a few
    /// over a long boot.
    pub reject_streak: u32,
    /// CONSECUTIVE submits refused locally because the order named an
    /// instance that has rolled (LAW E-4). Reset by an acceptance.
    pub asset_refusal_streak: u32,
    /// `1` when the address request budget is at or below its floor.
    pub budget_floor_breached: u8,
    /// `1` once the arm has reconciled against the venue at least
    /// once since boot AND that reconciliation agreed on every leg it
    /// looked at and covered every leg the venue holds.
    ///
    /// Not a halt trigger — the opposite. It is what lets the router
    /// call `Ledger::mark_seeded` and stop refusing every live place,
    /// and it rides here because it is the same question ("what does
    /// the arm know about the venue?") answered by the same poll.
    pub reconciled: u8,
    /// `1` when the E7 session bound may be judged: the arm has an
    /// anchor and has read the account since boot. Held legs no longer
    /// withdraw it — the arm values them at COST, so an open
    /// position's premium is not a loss and an unsettled win is not a
    /// gain (S7-L1, gap B: a book that was never flat was never
    /// judged). `0` otherwise.
    pub pnl_judged: u8,
    /// Explicit padding.
    _pad: [u8; 5],
    /// Nanoseconds since the last reconciliation that AGREED. `0` =
    /// never (the seeding interlock already refuses that case). Once
    /// an arm has agreed with the venue, a reconciler that stops
    /// answering or stops agreeing is measured here — the first cut
    /// had no such term, so a `/info` endpoint that started failing
    /// left "the single most valuable safety net in the plan"
    /// silently disabled while the arm kept trading (E7 review,
    /// 2026-09-19). Compared against `halt_on_recon_stale_ms`.
    pub recon_age_ns: u64,
    /// E7 session bound: the account's equity at cost (spot USDC plus
    /// the held legs' cost basis) minus the anchor, USD ×1e6, as of
    /// the last reconciliation. Meaningful only while
    /// [`Self::pnl_judged`] is set; `0` otherwise.
    pub pnl_delta_usd_1e6: i64,
}

impl HaltSignal {
    /// Build a signal with the session bound unobserved (`pnl_judged`
    /// 0). The arm is the only caller; see [`Self::with_pnl`].
    #[inline]
    #[must_use]
    pub const fn new(
        ws_gap_ns: u64,
        recon_drift_usd_1e6: i64,
        reject_streak: u32,
        asset_refusal_streak: u32,
        budget_floor_breached: bool,
        reconciled: bool,
        recon_age_ns: u64,
    ) -> Self {
        Self {
            ws_gap_ns,
            recon_drift_usd_1e6,
            reject_streak,
            asset_refusal_streak,
            budget_floor_breached: budget_floor_breached as u8,
            reconciled: reconciled as u8,
            pnl_judged: 0,
            _pad: [0; 5],
            recon_age_ns,
            pnl_delta_usd_1e6: 0,
        }
    }

    /// E7: attach the session-bound reading — `judged` says the bound
    /// may be judged, `delta` is the equity at cost minus the anchor.
    #[inline]
    #[must_use]
    pub const fn with_pnl(mut self, judged: bool, delta_usd_1e6: i64) -> Self {
        self.pnl_judged = judged as u8;
        self.pnl_delta_usd_1e6 = delta_usd_1e6;
        self
    }
}

const _: () = assert!(core::mem::size_of::<HaltSignal>() == 48);

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
    /// **E6: refused by the risk gate** — the SUM of every clamp's
    /// refusals (`max_order_usd`, `cap_instance_usd`, `cap_day_usd`,
    /// `max_open_orders`, the seeding interlock, the halt latch).
    /// Only the `max_order_usd` share still means "the member and the
    /// operator disagreed"; the rest mean the operator's ceiling was
    /// reached, which is the clamp working. An alert belongs on
    /// `exec_router::RouteCounters::refused_max_order`, not here.
    pub refused_risk: u64,
    /// **E7 review ruling** — cancels that reached the live arm on an
    /// `Off` slot. Off refuses every PLACE; a cancel can only reduce
    /// risk and is the one path that can take back an order a
    /// previous boot left resting, so it passes and is counted.
    pub cancel_on_off: u64,
    /// **E6 commit 4: refused because the slot is HALTED.** A
    /// subset of `refused_risk`, broken out because a halt is the one
    /// refusal reason an operator must not have to infer.
    pub refused_halted: u64,
    /// **E6 commit 4: refused because the ledger has never been
    /// reconciled.** Expected non-zero for a few seconds after every
    /// boot and zero thereafter; a value that keeps climbing means
    /// the arm never reached the venue.
    pub refused_unseeded: u64,
    /// Halt edges — slots that went from running to halted.
    pub halts: u64,
    /// Cancel-all requests the arm would not accept.
    pub cancel_all_failures: u64,
    /// Polls on which the arm reported it had given up with the venue
    /// unconfirmed. **The stranded-quote number.**
    pub cancel_all_stranded: u64,
    /// Slots halted by reading `exec.HALT` at boot rather than by a
    /// trigger in this process. `0` with `halt_file_present` set
    /// means an operator wrote a halt file that halted NOTHING.
    pub halt_file_adopted: u8,
    /// `1` when a readable `exec.HALT` was seen at all.
    pub halt_file_present: u8,
    /// `1` once the ledger has been reconciled against the venue.
    /// Until then every live PLACE is refused, so a boot stuck at `0`
    /// is a boot that is not trading.
    pub seeded: u8,
    /// Per-slot `HaltReason as u8`; `0` = running.
    pub halted: [u8; EXEC_COUNTER_SLOTS],
    /// Per-slot live submits.
    pub live_submits_by_slot: [u64; EXEC_COUNTER_SLOTS],
    /// Per-slot refusals — every reason (off, no-route, and the risk
    /// gate's six).
    pub refused_by_slot: [u64; EXEC_COUNTER_SLOTS],
    /// **E7 — the venue-fill ledger's own alarms**, which reached no
    /// surface at all in E6 (the module called them operator-visible;
    /// they were unit-test-visible). A venue fill on a leg whose bind
    /// was refused: **the risk gate has stopped seeing a real
    /// position.**
    pub ledger_fills_unbound: u64,
    /// A sell that took a leg below zero: the router and the venue
    /// disagree and cannot self-heal.
    pub ledger_sells_below_zero: u64,
    /// Roll binds refused for want of a row.
    pub ledger_binds_refused: u64,
    /// Resting-table rows that could not be taken — the
    /// `max_open_orders` count ratchets toward permanent refusal.
    pub ledger_resting_full: u64,
    /// Two rows under one `(client_oid, slot)` key.
    pub ledger_resting_ambiguous: u64,
    /// Settle frames that matched no bound row.
    pub ledger_settles_unmatched: u64,
    /// The live arm's own numbers. Zeroed when there is no arm.
    pub arm: LiveArmCounters,
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
    /// HYPARB H2: AMM swaps that filled (also counted in `fills`).
    pub amm_fills: u64,
    /// HYPARB H2: AMM swaps judged and not filled, or dropped by a
    /// chain-wide stream break — a swap never rests.
    pub amm_canceled: u64,
    /// HYPARB H2: AMM fills smaller than the order (the active range's
    /// boundary or the price limit stopped the walk).
    pub amm_partial: u64,
    /// HYPARB H2: AMM swaps canceled because their pool was not
    /// judgeable (never snapshotted, stale after a gap, edge-bound).
    pub amm_not_live: u64,
    /// XMM XH2: post-only orders the queue law took (also in `intake`).
    pub queue_placed: u64,
    /// XMM XH2: queue orders that landed and rest.
    pub queue_rested: u64,
    /// XMM XH2: queue orders rejected on landing for crossing
    /// (`BAD_ALO_PX`) — information, not a fault (XH-7).
    pub queue_rejected_alo: u64,
    /// XMM XH2: queue orders cancelled — requested, replaced or expired.
    pub queue_canceled: u64,
    /// XMM XH2: queue fills (also in `fills`).
    pub queue_fills: u64,
    /// XMM XH2: order events dropped because the event ring was full
    /// between two pumps. Must stay 0: the engine pumps every iteration.
    pub order_events_overflow: u64,
}

const _: () = assert!(::core::mem::size_of::<MatcherCounters>() == 176);

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
    out: [Fill; FILL_OUT],
    out_head: usize,
    out_len: usize,
    /// Activation Δ by model venue byte ([`core_fill::ACTIVATION_NS_DEFAULT`]
    /// verbatim; a byte past its end is refused in [`Self::submit`]
    /// before it can index anything).
    activation_ns: [u64; core_types::VENUE_COUNT],
    seq: u64,
    /// HYPARB H2: pool state for AMM swaps, rebuilt from the pool-event
    /// signals ([`Self::observe_amm`]) — `core_fill::AmmBook`, the same
    /// book the harness replays.
    amm: core_fill::AmmBook,
    /// XMM XH2: post-only makers on a queue venue, judged by the queue
    /// law — `core_fill::QueueBook`, the same book the harness replays.
    /// Its own table (32 orders, 8 symbols): the queue member cannot
    /// crowd the strict-cross members' 64 slots, nor they its.
    queue: core_fill::QueueBook,
    /// HC11 (O-HC21): Hypercall IoCs, judged by the held-quote law —
    /// the provider's quote in force 2 s after submit
    /// (`core_fill::held`). Its own table: the strict-cross members'
    /// 64 slots are not shared with it.
    held: core_fill::held::HeldBook,
    /// XMM XH2: order events waiting for [`Self::try_next_order_event`],
    /// FIFO.
    events: [core_types::OrderEvent; ORDER_EVENT_OUT],
    ev_head: usize,
    ev_len: usize,
    /// What the matcher did.
    pub counters: MatcherCounters,
}

/// XMM XH2: the paper matcher's order-event ring. One engine iteration
/// drains many ticks and prints before it pumps, and one queue call can
/// emit three events per queue order, so the ring holds several calls'
/// worth; an overflow is counted (`order_events_overflow`), never
/// silent.
const ORDER_EVENT_OUT: usize = 256;

/// The paper matcher's fill ring. X1 sized it at [`core_fill::MAX_OPEN_TOTAL`]
/// ("one tick can produce at most that many fills"); XMM XH2's prints
/// can partially fill a queue order once per print, and one iteration
/// drains many prints before the pump, so it is four times that. The
/// size changes nothing until the old ring would have overflowed.
const FILL_OUT: usize = 4 * core_fill::MAX_OPEN_TOTAL;

const EMPTY_ORDER_EVENT: core_types::OrderEvent = core_types::OrderEvent::new(
    0,
    core_types::VenueId::Hyperliquid,
    core_types::SYMBOL_ID_NONE,
    0,
    core_types::STRATEGY_ID_NONE,
    core_types::ORDER_EVENT_NONE,
    core_types::ORDER_EVENT_REASON_NONE,
    0,
);

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
            out: [EMPTY_FILL; FILL_OUT],
            out_head: 0,
            out_len: 0,
            activation_ns: d,
            seq: 0,
            amm: core_fill::AmmBook::new(),
            queue: core_fill::QueueBook::new(),
            held: core_fill::held::HeldBook::new(),
            events: [EMPTY_ORDER_EVENT; ORDER_EVENT_OUT],
            ev_head: 0,
            ev_len: 0,
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
                amm_fills: 0,
                amm_canceled: 0,
                amm_partial: 0,
                amm_not_live: 0,
                queue_placed: 0,
                queue_rested: 0,
                queue_rejected_alo: 0,
                queue_canceled: 0,
                queue_fills: 0,
                order_events_overflow: 0,
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
        // MX2 / O-MX1: MEXC (venue byte 7) is data-only — its activation
        // slot exists because the table is venue-byte indexed, but no
        // order on it is ever modelled, exactly as the harness's
        // `tradeable_venue_byte` refuses it. Before MX2 the byte sat past
        // the table's end and was refused by the length check; this
        // keeps that behaviour bit for bit.
        //
        // HC11 (O-HC21): a Hypercall (venue byte 9) IoC is judged by the
        // held-quote law in its own table; anything else on Hypercall —
        // a maker, which the venue cannot honour post-only — is
        // unmodellable.
        if venue == core_types::VenueId::Hypercall.to_u8() {
            if order.kind == core_fill::ORDER_KIND_IOC && px > 0 && qty > 0 {
                let o = core_fill::held::HeldOrder::new(
                    now_ns.saturating_add(core_fill::held::HELD_DELAY_NS),
                    order.sym,
                    order.side,
                    px,
                    qty,
                    order.client_oid,
                    order.strategy_id,
                    order.venue,
                );
                if self.held.submit(o) {
                    self.counters.intake = self.counters.intake.wrapping_add(1);
                } else if core_fill::held::held_index(order.sym).is_none() {
                    self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
                } else {
                    self.counters.rejected_open_cap =
                        self.counters.rejected_open_cap.wrapping_add(1);
                }
            } else {
                self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
            }
            return;
        }
        //
        // HYPARB H2: HyperEVM (venue byte 8) takes AMM swaps on a known
        // pool slot and nothing else; every other venue takes makers and
        // IoCs and never a swap.
        let kind_ok = if venue == core_types::VenueId::HyperEvm.to_u8() {
            order.kind == core_fill::ORDER_KIND_AMM_SWAP
                && core_fill::amm_pool_index(order.sym).is_some()
        } else {
            order.kind == core_fill::ORDER_KIND_MAKER || order.kind == core_fill::ORDER_KIND_IOC
        };
        if venue as usize >= core_fill::ACTIVATION_NS_DEFAULT.len()
            || venue == core_types::VenueId::Mexc.to_u8()
            || px <= 0
            || qty <= 0
            || !kind_ok
        {
            self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
            return;
        }
        // XMM XH2: a post-only maker on a queue venue is judged by the
        // queue law, in its own table. No order before XH2 carried the
        // flag, so every other path below is untouched.
        if core_fill::judged_by_queue(venue, order.kind, order.flags) {
            self.submit_queue(order, venue, now_ns);
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
            Resting::None => return self.cancel_queue(req),
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
            Resting::None => return self.modify_queue(req, now_ns),
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
        // HC11 (O-HC21): the held-quote law — every due Hypercall IoC is
        // judged at the first record of ANY venue STAMPED at or after its
        // due instant, against the quote held BEFORE this record is
        // applied (so nothing after the instant leaks into the verdict);
        // then a Hypercall quote becomes the one in force. The record's
        // own stamp is the clock, not the drain instant: a quote stamped
        // before the due instant but drained after it is still in force.
        if self.held.pending_len() > 0 {
            self.judge_held(tick.ts_ns, now_ns);
        }
        if core_types::symbol_venue_byte(sym) == core_types::VenueId::Hypercall.to_u8() {
            self.held.on_quote(tick);
        }
        // XMM XH2: the queue law's book. Every record of the symbol lands
        // what is due (a cancel or an expiry is never held back by a quiet
        // or degraded feed); only fresh two-sided evidence teaches it the
        // touch. One table lookup for any other venue.
        if core_fill::queue_venue(core_types::symbol_venue_byte(sym)) {
            let touch = if core_fill::is_fill_evidence(tick) {
                core_fill::Touch::of(tick)
            } else {
                core_fill::Touch::default()
            };
            self.queue.on_book(sym, touch, now_ns);
            self.drain_queue();
        }
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

    /// HYPARB H2: apply one pool-event signal and, on a new HEAD, judge
    /// every activated AMM swap in emit order against its pool
    /// (`core_fill::amm` states the law). A chain-wide GAP cancels every
    /// open swap: across a stream break a transaction's fate is unknown.
    pub fn observe_amm(&mut self, sym: SymbolId, payload: &[u8; 40], now_ns: NsTs) {
        match self.amm.observe(sym, payload) {
            core_fill::AmmObs::Head { .. } => self.judge_amm_orders(now_ns),
            core_fill::AmmObs::Gap => {
                let mut i = 0usize;
                while i < self.open_len {
                    if self.open[i].ident.kind == core_fill::ORDER_KIND_AMM_SWAP {
                        self.counters.amm_canceled = self.counters.amm_canceled.wrapping_add(1);
                        self.remove_open(i);
                        continue;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
    }

    /// The HEAD pass: TTL first (a clock fact), then each activated swap
    /// is judged ONCE and leaves the table either way.
    fn judge_amm_orders(&mut self, now_ns: NsTs) {
        let mut i = 0usize;
        while i < self.open_len {
            let o = self.open[i];
            if o.ident.kind != core_fill::ORDER_KIND_AMM_SWAP {
                i += 1;
                continue;
            }
            if core_fill::expired_at(now_ns, o.expiry_ns) {
                self.counters.ttl_expired = self.counters.ttl_expired.wrapping_add(1);
                self.remove_open(i);
                continue;
            }
            if now_ns < o.t_active_ns {
                i += 1;
                continue;
            }
            let v = match core_fill::amm_pool_index(o.ident.sym) {
                Some(idx) => self.amm.judge(idx, o.ident.side, o.px_1e6, o.remaining_1e6),
                // Unreachable: `submit` admits only pool slots.
                None => core_fill::AmmVerdict::NOT_LIVE,
            };
            match v.verdict {
                core_fill::Verdict::Fill { px_1e6, qty_1e6 } => {
                    self.push_fill(&o, px_1e6, qty_1e6, now_ns);
                    self.counters.amm_fills = self.counters.amm_fills.wrapping_add(1);
                    if qty_1e6 < o.remaining_1e6 {
                        self.counters.amm_partial = self.counters.amm_partial.wrapping_add(1);
                    }
                }
                _ => {
                    self.counters.amm_canceled = self.counters.amm_canceled.wrapping_add(1);
                    if v.pool_not_live() {
                        self.counters.amm_not_live = self.counters.amm_not_live.wrapping_add(1);
                    }
                }
            }
            self.remove_open(i);
        }
    }

    /// HYPARB H2: the AMM book's own counters.
    #[inline]
    #[must_use]
    pub const fn amm_book_counters(&self) -> core_fill::AmmBookCounters {
        self.amm.counters
    }

    /// XMM XH2: track `sym`'s touch for the queue law (see
    /// [`OrderDispatch::track_queue_sym`]). A ninth symbol is refused
    /// and counted with the open-cap refusals: its orders will be too.
    pub fn track_queue_sym(&mut self, sym: SymbolId) {
        if self.queue.track(sym).is_err() {
            self.counters.rejected_open_cap = self.counters.rejected_open_cap.wrapping_add(1);
        }
    }

    /// XMM XH2: one trade print for the queue law — the prints of a
    /// queue venue consume the queue ahead of our post-only makers and
    /// fill them. One table lookup for any other venue.
    pub fn observe_trade(&mut self, print: &core_types::TradePrint, now_ns: NsTs) {
        if !core_fill::queue_venue(core_types::symbol_venue_byte(print.sym)) {
            return;
        }
        let sell = print.aggressor == core_types::TRADE_AGGRESSOR_SELL;
        self.queue.on_print(print.sym, print.px_1e6, print.qty_1e6, sell, now_ns);
        self.drain_queue();
    }

    /// XMM XH2: write the next order event into `out`, FIFO; `false`
    /// when there is none.
    pub fn try_next_order_event(&mut self, out: &mut core_types::OrderEvent) -> bool {
        if self.ev_len == 0 {
            return false;
        }
        // COPY: one 64 B slot out of the ring into the caller's scratch —
        // the event must outlive the slot, because the member is handed it
        // through a ctx that holds `&mut` this dispatcher.
        *out = self.events[self.ev_head];
        self.ev_head = (self.ev_head + 1) % ORDER_EVENT_OUT;
        self.ev_len -= 1;
        true
    }

    /// XMM XH2: queue orders held (pending or resting).
    #[inline]
    #[must_use]
    pub const fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// The counters, with the queue law's own folded in.
    #[must_use]
    pub const fn counters_snapshot(&self) -> MatcherCounters {
        let q = self.queue.counters;
        let mut c = self.counters;
        c.queue_placed = q.placed;
        c.queue_rested = q.rested;
        c.queue_rejected_alo = q.rejected_alo;
        c.queue_canceled = q.canceled;
        c.queue_fills = q.fills;
        c
    }

    /// XMM XH2: take a post-only maker into the queue law. It lands at
    /// the first record of its symbol at or after `now + Δ_venue`; its
    /// TTL is a cancel scheduled at `emit + ttl`. A refusal (the table
    /// or its symbols are full) is counted and ANSWERED — the member
    /// waits on an event for every post-only order it sends.
    fn submit_queue(&mut self, order: &Order, venue: u8, now_ns: NsTs) {
        let expiry = core_fill::expiry_at(order.ts_ns, order.ttl_ns);
        let p = core_fill::QueuePlace {
            client_oid: order.client_oid,
            sym: order.sym,
            side: order.side,
            slot: order.strategy_id,
            px_1e6: order.px.raw(),
            qty_1e6: order.qty.raw(),
            ready_ns: now_ns.saturating_add(self.activation_ns[venue as usize]),
            expiry_ns: if expiry == 0 { core_fill::QUEUE_NEVER } else { expiry },
        };
        match self.queue.place(&p) {
            Ok(()) => self.counters.intake = self.counters.intake.wrapping_add(1),
            Err(_) => {
                self.counters.rejected_open_cap = self.counters.rejected_open_cap.wrapping_add(1);
                self.push_event(
                    now_ns,
                    order.sym,
                    order.client_oid,
                    order.strategy_id,
                    core_types::ORDER_EVENT_REJECTED,
                    core_types::ORDER_EVENT_REASON_OTHER,
                );
            }
        }
    }

    /// XMM XH2: a cancel that found nothing in the strict table. A queue
    /// order of that slot and id gets a cancel that lands at the first
    /// record at or after `ts + Δ_venue` — ahead of that block's prints —
    /// and `Ok` says the cancel was SENT: the order may still fill before
    /// it lands, and its `CANCELED` (or `FILLED`) event is the answer.
    fn cancel_queue(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        let o = match self.queue.lookup(req.client_oid, req.strategy_id) {
            Ok(o) => o,
            Err(core_fill::QueueRefusal::Ambiguous) => return self.ambiguous(),
            Err(_) => return self.no_such(),
        };
        if o.sym != req.sym || core_types::symbol_venue_byte(o.sym) != req.venue {
            self.counters.identity_mismatch = self.counters.identity_mismatch.wrapping_add(1);
            return Err(DispatchError::IdentityMismatch);
        }
        let venue = core_types::symbol_venue_byte(o.sym) as usize;
        let at = req.ts_ns.saturating_add(self.activation_ns[venue]);
        match self.queue.cancel(req.client_oid, req.strategy_id, at) {
            Ok(()) => {
                self.counters.cancels = self.counters.cancels.wrapping_add(1);
                Ok(())
            }
            Err(_) => self.no_such(),
        }
    }

    /// XMM XH2: a modify that found nothing in the strict table. On a
    /// queue order it is the venue's modify: a cancel of the old order
    /// and a new post-only order, both landing at `now + Δ_venue`, the
    /// new one at the BACK of its queue and inheriting the old one's
    /// expiry (E-7). Fail-closed: an old order no longer held is
    /// `NoSuchOrder` and places nothing, and one that leaves the book
    /// while the modify flies gets its replacement REJECTED on landing
    /// (`core_fill::queue`). A full table is `QueueFull`, the old order
    /// untouched.
    fn modify_queue(&mut self, req: &ModifyReq, now_ns: NsTs) -> Result<(), DispatchError> {
        let new = req.order();
        let venue = core_types::symbol_venue_byte(new.sym);
        let queued = core_fill::judged_by_queue(venue, new.kind, new.flags);
        match self.queue.lookup(req.prev_client_oid(), new.strategy_id) {
            Ok(o) => {
                if !queued || o.sym != new.sym || o.side != new.side {
                    self.counters.identity_mismatch = self.counters.identity_mismatch.wrapping_add(1);
                    return Err(DispatchError::IdentityMismatch);
                }
            }
            Err(core_fill::QueueRefusal::Ambiguous) => return self.ambiguous(),
            // Fail-closed: the venue does not modify an order that is no
            // longer on its book, so nothing is placed.
            Err(_) => return self.no_such(),
        }
        let px = new.px.raw();
        let qty = new.qty.raw();
        if px <= 0 || qty <= 0 {
            self.counters.unroutable = self.counters.unroutable.wrapping_add(1);
            return Err(DispatchError::Unroutable);
        }
        let expiry = core_fill::expiry_at(new.ts_ns, new.ttl_ns);
        let p = core_fill::QueuePlace {
            client_oid: new.client_oid,
            sym: new.sym,
            side: new.side,
            slot: new.strategy_id,
            px_1e6: px,
            qty_1e6: qty,
            ready_ns: now_ns.saturating_add(self.activation_ns[venue as usize]),
            expiry_ns: if expiry == 0 { core_fill::QUEUE_NEVER } else { expiry },
        };
        match self.queue.modify(req.prev_client_oid(), &p) {
            Ok(()) => {
                self.counters.modifies = self.counters.modifies.wrapping_add(1);
                Ok(())
            }
            Err(_) => {
                // The table is full: refused whole, and the old order is
                // left exactly as it was — "a refused modify changes
                // NOTHING" (E5). Said to the caller, not swallowed.
                self.counters.rejected_open_cap = self.counters.rejected_open_cap.wrapping_add(1);
                Err(DispatchError::QueueFull)
            }
        }
    }

    #[inline]
    fn no_such(&mut self) -> Result<(), DispatchError> {
        self.counters.no_such_order = self.counters.no_such_order.wrapping_add(1);
        Err(DispatchError::NoSuchOrder)
    }

    #[inline]
    fn ambiguous(&mut self) -> Result<(), DispatchError> {
        self.counters.ambiguous_order = self.counters.ambiguous_order.wrapping_add(1);
        Err(DispatchError::AmbiguousOrder)
    }

    /// Move the queue law's events out: a fill into the fill ring (the
    /// engine pumps fills BEFORE order events, so a member sees a fill
    /// before the `FILLED` it causes), everything else into the event
    /// ring.
    fn drain_queue(&mut self) {
        while let Some(e) = self.queue.try_next_event() {
            if e.kind == core_fill::QUEUE_EVENT_FILL {
                self.push_fill_of(e.sym, e.side, e.client_oid, e.slot, e.px_1e6, e.qty_1e6, e.t_ns);
            } else {
                self.push_event(e.t_ns, e.sym, e.client_oid, e.slot, e.kind, e.reason);
            }
        }
    }

    #[inline]
    fn push_event(
        &mut self,
        ts_ns: NsTs,
        sym: SymbolId,
        client_oid: u64,
        strategy_id: u8,
        kind: u8,
        reason: u8,
    ) {
        if self.ev_len >= ORDER_EVENT_OUT {
            debug_assert!(false, "paper matcher order-event ring overflowed");
            self.counters.order_events_overflow = self.counters.order_events_overflow.wrapping_add(1);
            return;
        }
        let venue = match core_types::VenueId::from_u8(core_types::symbol_venue_byte(sym)) {
            Some(v) => v,
            None => core_types::VenueId::Hyperliquid,
        };
        let slot = (self.ev_head + self.ev_len) % ORDER_EVENT_OUT;
        self.events[slot] =
            core_types::OrderEvent::new(ts_ns, venue, sym, client_oid, strategy_id, kind, reason, 0);
        self.ev_len += 1;
    }

    /// Pop the next modelled fill, FIFO.
    pub fn try_next_fill(&mut self) -> Option<Fill> {
        if self.out_len == 0 {
            return None;
        }
        let f = self.out[self.out_head];
        self.out_head = (self.out_head + 1) % FILL_OUT;
        self.out_len -= 1;
        Some(f)
    }

    #[inline]
    fn push_fill(&mut self, o: &Pending, px_1e6: i64, qty_1e6: i64, now_ns: NsTs) {
        self.push_fill_of(
            o.ident.sym,
            o.ident.side,
            o.client_oid,
            o.ident.strategy_id,
            px_1e6,
            qty_1e6,
            now_ns,
        );
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn push_fill_of(
        &mut self,
        sym: SymbolId,
        side: Side,
        client_oid: u64,
        strategy_id: u8,
        px_1e6: i64,
        qty_1e6: i64,
        now_ns: NsTs,
    ) {
        if self.out_len >= FILL_OUT {
            // The engine pumps every iteration, so this is unreachable —
            // which is exactly why it is counted rather than trusted.
            debug_assert!(false, "paper matcher out ring overflowed");
            self.counters.out_overflow = self.counters.out_overflow.wrapping_add(1);
            return;
        }
        let slot = (self.out_head + self.out_len) % FILL_OUT;
        self.out[slot] = Fill::new(
            now_ns,
            sym,
            side,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            client_oid,
        )
        .with_attribution(strategy_id, core_types::FILL_ORIGIN_PAPER);
        self.out_len += 1;
        self.counters.fills = self.counters.fills.wrapping_add(1);
    }

    /// HC11: judge the held-quote law's due IoCs (collected first: the
    /// book and the fill ring are both `self`'s). A fill rides the fill
    /// lane; a miss is an order event — `CANCELED`, reason `EXPIRED` — so
    /// the member frees the instrument at the verdict instead of on a
    /// timer.
    fn judge_held(&mut self, stamp_ns: NsTs, now_ns: NsTs) {
        let mut due = [(0u64, core_types::SYMBOL_ID_NONE, Side::Bid, 0u8, core_fill::Verdict::Cancel);
            core_fill::held::HELD_PENDING];
        let mut n = 0usize;
        self.held.judge_due(stamp_ns, |o, v| {
            if n < due.len() {
                due[n] = (o.client_oid, o.sym, o.side, o.strategy_id, v);
                n += 1;
            }
        });
        let mut k = 0usize;
        while k < n {
            let (oid, sym, side, slot, v) = due[k];
            match v {
                core_fill::Verdict::Fill { px_1e6, qty_1e6 } => {
                    self.push_fill_of(sym, side, oid, slot, px_1e6, qty_1e6, now_ns);
                }
                _ => {
                    self.counters.ioc_canceled = self.counters.ioc_canceled.wrapping_add(1);
                    self.push_event(
                        now_ns,
                        sym,
                        oid,
                        slot,
                        core_types::ORDER_EVENT_CANCELED,
                        core_types::ORDER_EVENT_REASON_EXPIRED,
                    );
                }
            }
            k += 1;
        }
    }

    /// Hypercall IoCs pending under the held-quote law (tests, `/state`).
    #[inline]
    #[must_use]
    pub const fn held_pending(&self) -> usize {
        self.held.pending_len()
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
    /// itself boot-constructed once by the engine, and its fixed arrays
    /// (≈ 60 KiB since XMM XH2: the strict table, the fill and
    /// order-event rings, the AMM and queue books; ≈ 93 KiB since HC11's
    /// held-quote book, 1 024 quotes) are the same memory
    /// either way. It moves by value only at boot — handed on a few times
    /// before the loop starts (the boot's `engine_loop_set_full` →
    /// `run_engine_loop` → `Engine::new`, plus `RoutedDispatcher::new` on
    /// an armed boot), never after.
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
        self.matcher.counters_snapshot()
    }

    /// X1: orders the matcher is currently holding (HC11: the held-quote
    /// law's pending IoCs included).
    #[inline]
    #[must_use]
    pub const fn open_orders(&self) -> usize {
        self.matcher.open_len() + self.matcher.queue_len() + self.matcher.held_pending()
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
        // A refusal is recorded here, on the same `DispatchStats` the
        // 5 s tick mirrors. `rejected_lifecycle` had no production
        // writer at all until the E7 review — a documented operator
        // surface that read a permanent zero.
        let r = self.matcher.cancel(req);
        if let Err(e) = r {
            self.stats.record_rejection(e);
        }
        r
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
        let r = self.matcher.modify(req, req.order().ts_ns);
        if let Err(e) = r {
            self.stats.record_rejection(e);
        }
        r
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
    fn observe_amm(&mut self, sym: SymbolId, payload: &[u8; 40], now_ns: NsTs) {
        self.matcher.observe_amm(sym, payload, now_ns);
    }

    #[inline]
    fn observe_trade(&mut self, print: &core_types::TradePrint, now_ns: NsTs) {
        self.matcher.observe_trade(print, now_ns);
    }

    #[inline]
    fn try_next_order_event(&mut self, out: &mut core_types::OrderEvent) -> bool {
        self.matcher.try_next_order_event(out)
    }

    #[inline]
    fn track_queue_sym(&mut self, sym: SymbolId) {
        self.matcher.track_queue_sym(sym);
    }

    #[inline]
    fn matcher_counters(&self) -> MatcherCounters {
        self.matcher.counters_snapshot()
    }

    #[inline]
    fn open_paper_orders(&self) -> usize {
        self.matcher.open_len() + self.matcher.queue_len() + self.matcher.held_pending()
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

    /// MX2 / O-MX1: MEXC has an activation slot but is data-only — a
    /// MEXC order is refused as `unroutable`, never modelled.
    #[test]
    fn a_mexc_order_is_unroutable_data_only() {
        let mut m = PaperMatcher::new();
        let sym = core_types::make_symbol_id(VenueId::Mexc, 1);
        let o = Order::new(
            1_000,
            VenueId::Mexc,
            sym,
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(1_000_000),
            Qty::from_raw(1_000_000),
            1,
        );
        m.submit(&o, 1_000);
        assert_eq!(m.counters.unroutable, 1);
        assert_eq!(m.counters.intake, 0);
        assert_eq!(m.open_len(), 0);
    }

    /// HC11 (O-HC21): a Hypercall IoC is taken by the held-quote law (the
    /// quote in force 2 s after submit decides); a Hypercall maker — the
    /// venue has no post-only — and an index symbol are unroutable.
    #[test]
    fn a_hypercall_ioc_fills_at_the_quote_held_two_seconds_later() {
        let mut m = PaperMatcher::new();
        let opt = core_types::make_symbol_id(VenueId::Hypercall, 513);
        let idx = core_types::make_symbol_id(VenueId::Hypercall, 3);
        let tick = |bid: i64, ask: i64, ts: u64| {
            Tick::new_stamped(
                ts,
                VenueId::Hypercall,
                opt,
                0,
                Price::from_raw(bid),
                Qty::from_raw(4_000_000),
                Price::from_raw(ask),
                Qty::from_raw(4_000_000),
                0,
                0,
            )
        };
        let order = |kind, sym| {
            let mut o = Order::new(
                1_000,
                VenueId::Hypercall,
                sym,
                Side::Bid,
                kind,
                Price::from_raw(1_050_000),
                Qty::from_raw(1_000_000),
                77,
            );
            o.strategy_id = 7;
            o
        };
        m.observe_tick(&tick(900_000, 1_100_000, 500), 500);
        m.submit(&order(core_fill::ORDER_KIND_MAKER, opt), 1_000);
        m.submit(&order(core_fill::ORDER_KIND_IOC, idx), 1_000);
        assert_eq!(m.counters.unroutable, 2);
        m.submit(&order(core_fill::ORDER_KIND_IOC, opt), 1_000);
        assert_eq!((m.counters.intake, m.held_pending()), (1, 1));
        // The quote improves inside the 2 s; a record before the due
        // instant judges nothing.
        m.observe_tick(&tick(950_000, 1_040_000, 1_000_000_000), 1_000_000_000);
        assert!(m.try_next_fill().is_none());
        // Due at 1_000 + 2 s: the record at that instant is judged against
        // the quote in force BEFORE it (1.04), not its own (1.20).
        // A quote stamped just BEFORE the due instant but drained after it
        // is still the one in force: nothing is judged on it…
        m.observe_tick(&tick(960_000, 1_030_000, 2_000_000_999), 2_000_005_000);
        assert!(m.try_next_fill().is_none());
        m.observe_tick(&tick(1_150_000, 1_200_000, 2_000_001_000), 2_000_006_000);
        let f = m.try_next_fill().expect("filled at the held ask");
        // …and the verdict reads it (1.03), not the next record's (1.20).
        assert_eq!((f.px.raw(), f.qty.raw(), f.order_id, f.strategy_id), (1_030_000, 1_000_000, 77, 7));
        assert_eq!(f.origin, core_types::FILL_ORIGIN_PAPER);
        assert_eq!(m.held_pending(), 0);
        let mut ev = core_types::OrderEvent::ZERO;
        assert!(!m.try_next_order_event(&mut ev), "a fill is no order event");
        // Worse than the limit at the due instant: nothing — and the miss
        // is told to the slot as an expiry.
        m.submit(&order(core_fill::ORDER_KIND_IOC, opt), 3_000_000_000);
        m.observe_tick(&tick(1_150_000, 1_200_000, 5_000_000_001), 5_000_000_001);
        assert!(m.try_next_fill().is_none());
        assert_eq!(m.counters.ioc_canceled, 1);
        assert!(m.try_next_order_event(&mut ev));
        assert_eq!(
            (ev.kind, ev.reason, ev.client_oid, ev.strategy_id, ev.venue),
            (
                core_types::ORDER_EVENT_CANCELED,
                core_types::ORDER_EVENT_REASON_EXPIRED,
                77,
                7,
                VenueId::Hypercall.to_u8()
            )
        );
    }

    /// HC11: a Hypercall IoC waiting for its held-quote verdict is an open
    /// paper order (the engine's shutdown drain and `/metrics` read it).
    #[test]
    fn a_held_ioc_counts_as_open_until_its_verdict() {
        let mut d = PaperDispatcher::new();
        let opt = core_types::make_symbol_id(VenueId::Hypercall, 513);
        let mut o = Order::new(
            1_000,
            VenueId::Hypercall,
            opt,
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(1_000_000),
            Qty::from_raw(1_000_000),
            5,
        );
        o.strategy_id = 7;
        OrderDispatch::submit(&mut d, &o).expect("the paper dispatcher takes it");
        assert_eq!((OrderDispatch::open_paper_orders(&d), d.open_orders()), (1, 1));
        let t = Tick::new_stamped(
            2_000_001_000,
            VenueId::Hypercall,
            opt,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
            0,
            0,
        );
        OrderDispatch::observe_tick(&mut d, &t, 2_000_001_000);
        assert_eq!(OrderDispatch::open_paper_orders(&d), 0, "judged: a miss, gone");
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

    // ---------------- HYPARB H2: the AMM arm ----------------

    mod amm {
        use super::*;
        use core_amm::payload::{
            encode_gap, encode_head, encode_snapshot, encode_state, FAMILY_V3,
        };
        use core_amm::{price_1e18_from_sqrt, sqrt_at_tick};
        use core_types::{make_symbol_id, SYMBOL_ID_NONE};

        const POOL: SymbolId = make_symbol_id(VenueId::HyperEvm, 1);
        const TICK: i32 = -230_543;
        const L: u128 = 50_000_000_000_000_000_000;
        const BLOCK_NS: u64 = 1_000_000_000;

        fn swap(side: Side, px: i64, qty: i64, oid: u64) -> Order {
            let mut o = Order::new(
                1_000,
                VenueId::HyperEvm,
                POOL,
                side,
                core_fill::ORDER_KIND_AMM_SWAP,
                Price::from_raw(px),
                Qty::from_raw(qty),
                oid,
            );
            o.strategy_id = 0;
            o
        }

        fn live(m: &mut PaperMatcher) -> i64 {
            let (lo, hi) = sqrt_at_tick(TICK);
            let snap = encode_snapshot(7, FAMILY_V3, -240_000, -220_000, 0, 500, 10, 18, 6);
            m.observe_amm(POOL, &snap.unwrap(), 0);
            m.observe_amm(POOL, &encode_state(TICK, lo, hi, L, true).unwrap(), 0);
            (price_1e18_from_sqrt(lo, hi, 18, 6) / 1_000_000_000_000) as i64
        }

        fn head(m: &mut PaperMatcher, block: u64, now: u64) {
            m.observe_amm(SYMBOL_ID_NONE, &encode_head(block, 1, 1).unwrap(), now);
        }

        #[test]
        fn a_swap_is_judged_once_at_the_first_head_after_one_block() {
            let mut m = PaperMatcher::new();
            let mid = live(&mut m);
            m.submit(&swap(Side::Ask, mid * 99 / 100, 1_000_000, 9), 0);
            assert_eq!(m.open_len(), 1);
            head(&mut m, 8, BLOCK_NS - 1);
            assert_eq!(m.open_len(), 1, "not before the block it could land in");
            head(&mut m, 9, BLOCK_NS);
            assert_eq!(m.open_len(), 0, "judged once, gone either way");
            let f = m.try_next_fill().expect("a fill");
            assert_eq!(f.sym, POOL);
            assert_eq!(f.qty.raw(), 1_000_000);
            assert!(f.px.raw() < mid, "the fee and the impact are in the price");
            assert_eq!(f.strategy_id, 0);
            assert_eq!(m.counters.amm_fills, 1);
            assert_eq!(m.counters.fills, 1);
        }

        #[test]
        fn a_swap_that_cannot_meet_its_limit_cancels_and_never_rests() {
            let mut m = PaperMatcher::new();
            let mid = live(&mut m);
            m.submit(&swap(Side::Bid, mid, 1_000_000, 1), 0);
            head(&mut m, 9, BLOCK_NS);
            assert_eq!(m.open_len(), 0);
            assert!(m.try_next_fill().is_none());
            assert_eq!(m.counters.amm_canceled, 1);
            assert_eq!(
                m.counters.amm_not_live, 0,
                "the price refused it, not the pool"
            );
        }

        #[test]
        fn a_pool_never_snapshotted_cancels_as_not_live() {
            let mut m = PaperMatcher::new();
            m.submit(&swap(Side::Ask, 1, 1_000_000, 1), 0);
            head(&mut m, 9, BLOCK_NS);
            assert_eq!(m.counters.amm_canceled, 1);
            assert_eq!(m.counters.amm_not_live, 1);
        }

        #[test]
        fn a_chain_gap_cancels_every_open_swap() {
            let mut m = PaperMatcher::new();
            let mid = live(&mut m);
            m.submit(&swap(Side::Ask, mid / 2, 1_000_000, 1), 0);
            m.submit(&swap(Side::Bid, mid * 2, 1_000_000, 2), 0);
            m.observe_amm(SYMBOL_ID_NONE, &encode_gap(8).unwrap(), 10);
            assert_eq!(m.open_len(), 0);
            assert_eq!(m.counters.amm_canceled, 2);
            assert!(m.try_next_fill().is_none());
        }

        #[test]
        fn two_swaps_on_one_pool_see_each_others_impact_in_emit_order() {
            let mut m = PaperMatcher::new();
            let mid = live(&mut m);
            m.submit(&swap(Side::Bid, mid * 101 / 100, 1_000_000, 1), 0);
            m.submit(&swap(Side::Bid, mid * 101 / 100, 1_000_000, 2), 0);
            head(&mut m, 9, BLOCK_NS);
            let a = m.try_next_fill().unwrap();
            let b = m.try_next_fill().unwrap();
            assert_eq!((a.order_id, b.order_id), (1, 2));
            assert!(
                b.px.raw() > a.px.raw(),
                "the second buy pays for the first's impact"
            );
        }

        #[test]
        fn the_ttl_cancels_a_swap_before_its_head() {
            let mut m = PaperMatcher::new();
            let mid = live(&mut m);
            let mut o = swap(Side::Ask, mid / 2, 1_000_000, 1);
            o.ttl_ns = 500_000_000;
            m.submit(&o, 0);
            head(&mut m, 9, BLOCK_NS);
            assert_eq!(m.counters.ttl_expired, 1);
            assert_eq!(m.counters.amm_fills, 0);
        }

        #[test]
        fn hyperevm_takes_swaps_only_and_swaps_go_nowhere_else() {
            let mut m = PaperMatcher::new();
            let mut ioc = swap(Side::Ask, 1, 1, 1);
            ioc.kind = core_fill::ORDER_KIND_IOC;
            m.submit(&ioc, 0);
            let mut off = swap(Side::Ask, 1, 1, 2);
            off.sym = make_symbol_id(VenueId::HyperEvm, 200);
            m.submit(&off, 0);
            let mut elsewhere = mk_order(Side::Bid, core_fill::ORDER_KIND_AMM_SWAP, 1, 1, 3, 0);
            elsewhere.kind = core_fill::ORDER_KIND_AMM_SWAP;
            m.submit(&elsewhere, 0);
            assert_eq!(m.open_len(), 0);
            assert_eq!(m.counters.unroutable, 3);
        }

        #[test]
        fn the_paper_dispatcher_forwards_pool_events_to_its_matcher() {
            let mut d = PaperDispatcher::new();
            let (lo, hi) = sqrt_at_tick(TICK);
            let snap = encode_snapshot(7, FAMILY_V3, -240_000, -220_000, 0, 500, 10, 18, 6);
            OrderDispatch::observe_amm(&mut d, POOL, &snap.unwrap(), 0);
            OrderDispatch::observe_amm(
                &mut d,
                POOL,
                &encode_state(TICK, lo, hi, L, true).unwrap(),
                0,
            );
            assert_eq!(d.matcher.amm_book_counters().snapshots, 1);
        }
    }

    // ---------------- XMM XH2: the queue law on the paper arm ----------------

    const HL_PERP: SymbolId = 0x0400_0005;
    /// Δ_hl is 340 ms.
    const HL_DELTA: NsTs = 340_000_000;
    const XMM_SLOT: u8 = 6;

    fn hl_order(side: Side, px: i64, qty: i64, oid: u64, ts: NsTs) -> Order {
        let mut o = Order::new(
            ts,
            VenueId::Hyperliquid,
            HL_PERP,
            side,
            core_fill::ORDER_KIND_MAKER,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        )
        .with_post_only();
        o.strategy_id = XMM_SLOT;
        o
    }

    fn hl_tick(bid: i64, bid_q: i64, ask: i64, ask_q: i64) -> Tick {
        Tick::new(
            1_000,
            VenueId::Hyperliquid,
            HL_PERP,
            0,
            Price::from_raw(bid),
            Qty::from_raw(bid_q),
            Price::from_raw(ask),
            Qty::from_raw(ask_q),
        )
    }

    fn hl_print(px: i64, qty: i64, sell: bool) -> core_types::TradePrint {
        core_types::TradePrint::new(
            0,
            VenueId::Hyperliquid,
            HL_PERP,
            7,
            0,
            px,
            qty,
            if sell { core_types::TRADE_AGGRESSOR_SELL } else { core_types::TRADE_AGGRESSOR_BUY },
        )
    }

    /// A matcher tracking `HL_PERP` whose book is 100 (2 shown) / 101.
    fn hl_matcher() -> PaperMatcher {
        let mut m = PaperMatcher::new();
        m.track_queue_sym(HL_PERP);
        m.observe_tick(&hl_tick(100_000_000, 2_000_000, 101_000_000, 2_000_000), 0);
        m
    }

    fn next_event(m: &mut PaperMatcher) -> Option<core_types::OrderEvent> {
        let mut e = core_types::OrderEvent::ZERO;
        if m.try_next_order_event(&mut e) {
            Some(e)
        } else {
            None
        }
    }

    fn next_event_of<D: OrderDispatch>(d: &mut D) -> Option<core_types::OrderEvent> {
        let mut e = core_types::OrderEvent::ZERO;
        if d.try_next_order_event(&mut e) {
            Some(e)
        } else {
            None
        }
    }

    fn event_kinds(m: &mut PaperMatcher) -> Vec<(u64, u8, u8)> {
        let mut v = Vec::new();
        while let Some(e) = next_event(m) {
            assert_eq!((e.strategy_id, e.sym, e.venue), (XMM_SLOT, HL_PERP, VenueId::Hyperliquid as u8));
            v.push((e.client_oid, e.kind, e.reason));
        }
        v
    }

    #[test]
    fn a_post_only_hl_maker_joins_the_queue_and_fills_from_prints() {
        let mut m = hl_matcher();
        m.submit(&hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000), 1_000);
        assert_eq!((m.open_len(), m.queue_len(), m.counters.intake), (0, 1, 1));
        // Before Δ nothing lands; the first book at or after it does.
        m.observe_tick(&hl_tick(100_000_000, 2_000_000, 101_000_000, 2_000_000), HL_DELTA);
        assert!(event_kinds(&mut m).is_empty());
        m.observe_tick(&hl_tick(100_000_000, 3_000_000, 101_000_000, 2_000_000), AFTER_DELTA + 60_000_000);
        assert_eq!(event_kinds(&mut m), [(9, core_types::ORDER_EVENT_RESTING, 0)]);
        // A seller takes 2.5: 2 ahead, then half of ours — at OUR price.
        m.observe_trade(&hl_print(100_000_000, 2_500_000, true), 500_000_000);
        let f = m.try_next_fill().expect("a queue fill");
        assert_eq!((f.px.raw(), f.qty.raw(), f.order_id, f.strategy_id), (100_000_000, 500_000, 9, XMM_SLOT));
        assert_eq!(f.ts_ns, 500_000_000);
        assert!(event_kinds(&mut m).is_empty(), "a partial fill is no order event");
        m.observe_trade(&hl_print(100_000_000, 9_000_000, true), 600_000_000);
        assert_eq!(m.try_next_fill().map(|f| f.qty.raw()), Some(500_000));
        assert_eq!(event_kinds(&mut m), [(9, core_types::ORDER_EVENT_FILLED, 0)]);
        assert_eq!(m.queue_len(), 0);
        let c = m.counters_snapshot();
        assert_eq!((c.queue_placed, c.queue_rested, c.queue_fills, c.fills), (1, 1, 2, 2));
    }

    #[test]
    fn a_plain_hl_maker_keeps_the_strict_cross_law() {
        let mut m = hl_matcher();
        let mut o = hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000);
        o.flags = 0;
        m.submit(&o, 1_000);
        assert_eq!((m.open_len(), m.queue_len()), (1, 0));
        m.observe_tick(&hl_tick(100_000_000, 3_000_000, 101_000_000, 2_000_000), AFTER_DELTA + 60_000_000);
        m.observe_trade(&hl_print(99_000_000, 9_000_000, true), 500_000_000);
        assert!(m.try_next_fill().is_none(), "prints are the queue law's evidence only");
        assert!(next_event(&mut m).is_none());
    }

    #[test]
    fn a_crossing_post_only_order_is_rejected_bad_alo_px_as_an_event() {
        let mut m = hl_matcher();
        m.submit(&hl_order(Side::Bid, 101_000_000, 1_000_000, 9, 1_000), 1_000);
        m.observe_tick(&hl_tick(100_000_000, 3_000_000, 101_000_000, 2_000_000), AFTER_DELTA + 60_000_000);
        assert_eq!(
            event_kinds(&mut m),
            [(9, core_types::ORDER_EVENT_REJECTED, core_types::ORDER_EVENT_REASON_BAD_ALO_PX)]
        );
        assert_eq!(m.counters_snapshot().queue_rejected_alo, 1);
        assert_eq!(m.queue_len(), 0);
    }

    #[test]
    fn a_queue_cancel_lands_after_delta_ahead_of_that_blocks_prints() {
        let mut m = hl_matcher();
        // Nothing shown at 100 when it lands, so nothing is ahead.
        m.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), 500);
        let o = hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000);
        m.submit(&o, 1_000);
        m.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), HL_DELTA + 1_000);
        event_kinds(&mut m);
        let at = 400_000_000;
        assert_eq!(m.cancel(&CancelReq::of(&o, at)), Ok(()));
        assert_eq!(m.counters.cancels, 1);
        // Before the cancel lands the order still fills …
        m.observe_trade(&hl_print(100_000_000, 400_000, true), at + HL_DELTA - 1);
        assert_eq!(m.try_next_fill().map(|f| f.qty.raw()), Some(400_000));
        // … and the print of the block it lands in does not.
        m.observe_trade(&hl_print(100_000_000, 9_000_000, true), at + HL_DELTA);
        assert!(m.try_next_fill().is_none());
        assert_eq!(
            event_kinds(&mut m),
            [(9, core_types::ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED)]
        );
    }

    #[test]
    fn a_stale_or_one_sided_tick_still_lands_a_queue_cancel() {
        let mut m = hl_matcher();
        m.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), 500);
        let o = hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000);
        m.submit(&o, 1_000);
        m.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), HL_DELTA + 1_000);
        event_kinds(&mut m);
        m.cancel(&CancelReq::of(&o, 400_000_000)).expect("sent");
        // A one-sided book lands it; it teaches the law no touch.
        m.observe_tick(&hl_tick(0, 0, 101_000_000, 2_000_000), 400_000_000 + HL_DELTA);
        assert_eq!(
            event_kinds(&mut m),
            [(9, core_types::ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED)]
        );
    }

    #[test]
    fn a_queue_cancel_of_nothing_or_of_another_market_is_refused() {
        let mut m = hl_matcher();
        let o = hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000);
        assert_eq!(m.cancel(&CancelReq::of(&o, 5)), Err(DispatchError::NoSuchOrder));
        m.submit(&o, 1_000);
        let mut other = o;
        other.sym = 0x0400_0006;
        assert_eq!(m.cancel(&CancelReq::of(&other, 5)), Err(DispatchError::IdentityMismatch));
        m.submit(&hl_order(Side::Ask, 101_000_000, 1_000_000, 9, 1_000), 1_000);
        assert_eq!(m.cancel(&CancelReq::of(&o, 5)), Err(DispatchError::AmbiguousOrder));
        let c = m.counters_snapshot();
        assert_eq!((c.no_such_order, c.identity_mismatch, c.ambiguous_order), (1, 1, 1));
        assert_eq!(m.queue_len(), 2, "nothing was cancelled");
    }

    #[test]
    fn a_queue_modify_replaces_and_a_lost_race_places_nothing() {
        let mut m = hl_matcher();
        let o = hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000);
        m.submit(&o, 1_000);
        m.observe_tick(&hl_tick(100_000_000, 2_000_000, 102_000_000, 2_000_000), HL_DELTA + 1_000);
        event_kinds(&mut m);
        let at = 400_000_000;
        let new = hl_order(Side::Bid, 101_000_000, 1_000_000, 10, at);
        assert_eq!(m.modify(&ModifyReq::new(9, new), at), Ok(()));
        m.observe_tick(&hl_tick(100_000_000, 2_000_000, 102_000_000, 2_000_000), at + HL_DELTA);
        assert_eq!(
            event_kinds(&mut m),
            [
                (10, core_types::ORDER_EVENT_RESTING, 0),
                (9, core_types::ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_REPLACED),
            ]
        );
        assert_eq!(m.counters.modifies, 1);
        // Fail-closed: the old id is gone now, so a modify of it is
        // refused and places nothing.
        let again = hl_order(Side::Bid, 100_000_000, 1_000_000, 11, at + HL_DELTA);
        assert_eq!(
            m.modify(&ModifyReq::new(9, again), at + HL_DELTA),
            Err(DispatchError::NoSuchOrder)
        );
        assert_eq!((m.queue_len(), m.counters.no_such_order), (1, 1));
    }

    #[test]
    fn a_full_table_refuses_a_modify_and_leaves_the_old_order() {
        let mut m = hl_matcher();
        let mut oid = 1u64;
        while m.queue_len() < core_fill::QUEUE_MAX_ORDERS {
            m.submit(&hl_order(Side::Bid, 99_000_000, 1_000_000, oid, 1_000), 1_000);
            oid += 1;
        }
        let new = hl_order(Side::Bid, 99_500_000, 1_000_000, 100, 5_000);
        assert_eq!(m.modify(&ModifyReq::new(1, new), 5_000), Err(DispatchError::QueueFull));
        let o = m.queue.get(1, XMM_SLOT).expect("the old order is untouched");
        assert_eq!(o.cancel_ns, core_fill::QUEUE_NEVER);
        assert!(next_event(&mut m).is_none(), "said to the caller, not as an event");
    }

    #[test]
    fn a_queue_modify_that_changes_side_or_drops_the_flag_is_refused() {
        let mut m = hl_matcher();
        m.submit(&hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000), 1_000);
        let flip = hl_order(Side::Ask, 102_000_000, 1_000_000, 10, 5);
        assert_eq!(m.modify(&ModifyReq::new(9, flip), 5), Err(DispatchError::IdentityMismatch));
        let mut plain = hl_order(Side::Bid, 99_000_000, 1_000_000, 10, 5);
        plain.flags = 0;
        assert_eq!(m.modify(&ModifyReq::new(9, plain), 5), Err(DispatchError::IdentityMismatch));
        // A plain modify of nothing is the strict table's answer, as before XH2.
        assert_eq!(m.modify(&ModifyReq::new(77, plain), 5), Err(DispatchError::NoSuchOrder));
        assert_eq!(m.queue_len(), 1);
    }

    #[test]
    fn prints_of_other_venues_and_untracked_books_do_nothing() {
        let mut m = PaperMatcher::new();
        let mut p = hl_print(100_000_000, 1_000_000, true);
        p.sym = DERIBIT_PERP;
        m.observe_trade(&p, 5);
        m.observe_trade(&hl_print(100_000_000, 1_000_000, true), 5);
        m.observe_tick(&hl_tick(100_000_000, 1, 101_000_000, 1), 5);
        assert!(m.try_next_fill().is_none());
        assert!(next_event(&mut m).is_none());
        // Placing tracks the symbol itself, but the first arrival then
        // meets no book: it rests unchecked.
        m.submit(&hl_order(Side::Bid, 105_000_000, 1_000_000, 9, 1_000), 1_000);
        m.observe_tick(&hl_tick(100_000_000, 1, 101_000_000, 1), AFTER_DELTA + 60_000_000);
        assert_eq!(event_kinds(&mut m), [(9, core_types::ORDER_EVENT_RESTING, 0)]);
    }

    #[test]
    fn a_full_queue_answers_with_a_rejected_event() {
        let mut m = hl_matcher();
        let mut oid = 1u64;
        while m.queue_len() < core_fill::QUEUE_MAX_ORDERS {
            m.submit(&hl_order(Side::Bid, 99_000_000, 1_000_000, oid, 1_000), 1_000);
            oid += 1;
        }
        m.submit(&hl_order(Side::Bid, 99_000_000, 1_000_000, oid, 1_000), 1_000);
        assert_eq!(
            event_kinds(&mut m),
            [(oid, core_types::ORDER_EVENT_REJECTED, core_types::ORDER_EVENT_REASON_OTHER)]
        );
        assert_eq!(m.counters.rejected_open_cap, 1);
    }

    #[test]
    fn the_paper_dispatcher_forwards_the_queue_law() {
        let mut d = PaperDispatcher::new();
        d.track_queue_sym(HL_PERP);
        d.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), 0);
        d.submit(&hl_order(Side::Bid, 100_000_000, 1_000_000, 9, 1_000)).expect("paper submit");
        assert_eq!(d.open_paper_orders(), 1);
        assert_eq!(d.open_orders(), 1);
        d.observe_tick(&hl_tick(100_000_000, 0, 101_000_000, 2_000_000), AFTER_DELTA + 60_000_000);
        d.observe_trade(&hl_print(100_000_000, 1_000_000, true), 500_000_000);
        assert_eq!(d.try_next_fill().map(|f| f.qty.raw()), Some(1_000_000));
        assert_eq!(next_event_of(&mut d).map(|e| e.kind), Some(core_types::ORDER_EVENT_RESTING));
        assert_eq!(next_event_of(&mut d).map(|e| e.kind), Some(core_types::ORDER_EVENT_FILLED));
        assert!(next_event_of(&mut d).is_none());
        assert_eq!(OrderDispatch::matcher_counters(&d).queue_fills, 1);
        assert_eq!(d.matcher_counters().queue_placed, 1);
    }

    #[test]
    fn the_trait_defaults_carry_no_queue_law() {
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
        d.track_queue_sym(HL_PERP);
        d.observe_trade(&hl_print(100_000_000, 1_000_000, true), 5);
        assert!(next_event_of(&mut d).is_none());
        assert_eq!(d.matcher_counters(), MatcherCounters::default());
    }
}
