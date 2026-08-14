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

use core_types::{Fill, Order};

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
            DispatchError::QueueFull => {
                NetworkErr::new(NetworkSource::Clob, NetworkErrKind::Io)
            }
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
}

/// Paper-mode dispatcher — records submissions, never calls out.
#[derive(Debug, Default)]
pub struct PaperDispatcher {
    stats: DispatchStats,
}

impl PaperDispatcher {
    /// Construct empty.
    pub const fn new() -> Self {
        Self {
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
    fn submit(&mut self, _order: &Order) -> Result<(), DispatchError> {
        self.stats.accepted = self.stats.accepted.wrapping_add(1);
        Ok(())
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    fn stats(&self) -> DispatchStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{Price, Qty, Side};

    #[test]
    fn paper_dispatcher_counts_submissions() {
        let mut d = PaperDispatcher::new();
        let o = Order::new(
            0,
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
