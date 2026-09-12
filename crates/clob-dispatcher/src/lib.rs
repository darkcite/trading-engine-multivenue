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

use core_types::{Fill, NsTs, Order, Price, Qty, Side, SymbolId, Tick};

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
        }
        self.rejected = self.rejected.wrapping_add(1);
    }
}

/// Trait implemented by the real dispatcher AND the paper-mode stub.
/// Strategies don't implement this; the engine owns one.
pub trait OrderDispatch {
    /// Submit an order. Non-blocking for paper mode; one network
    /// round-trip in live mode.
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError>;

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
    /// I1 expiry (`emit + ttl`); 0 = never.
    expiry_ns: u64,
    sym: SymbolId,
    side: Side,
    kind: u8,
    strategy_id: u8,
    venue: u8,
    px_1e6: i64,
    remaining_1e6: i64,
    client_oid: u64,
}

const EMPTY_PENDING: Pending = Pending {
    seq: 0,
    t_active_ns: 0,
    expiry_ns: 0,
    sym: core_types::SYMBOL_ID_NONE,
    side: Side::Bid,
    kind: core_fill::ORDER_KIND_MAKER,
    strategy_id: core_types::STRATEGY_ID_NONE,
    venue: 0,
    px_1e6: 0,
    remaining_1e6: 0,
    client_oid: 0,
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
            if self.open[i].sym == order.sym {
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
            sym: order.sym,
            side: order.side,
            kind: order.kind,
            strategy_id: order.strategy_id,
            venue,
            px_1e6: px,
            remaining_1e6: qty,
            client_oid: order.client_oid,
        };
        self.seq = self.seq.wrapping_add(1);
        self.open_len += 1;
        self.counters.intake = self.counters.intake.wrapping_add(1);
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
            if self.open[i].sym == sym && core_fill::expired_at(now_ns, self.open[i].expiry_ns) {
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
            if o.sym != sym || now_ns < o.t_active_ns {
                i += 1;
                continue;
            }
            if o.kind == core_fill::ORDER_KIND_IOC {
                match core_fill::judge_ioc(
                    o.side,
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
                o.side,
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
            o.sym,
            o.side,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            o.client_oid,
        )
        .with_attribution(o.strategy_id, core_types::FILL_ORIGIN_PAPER);
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
    use core_types::{Price, Qty, Side, VenueId};

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
}
