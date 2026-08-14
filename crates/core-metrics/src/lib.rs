//! # core-metrics
//!
//! Preallocated counter + gauge registry with a tiny Prometheus
//! text-format HTTP/1.1 server. Designed for the polymarket-engine
//! `/metrics` endpoint that ships in Phase 4.
//!
//! ## Properties
//!
//! * **Zero-alloc record path.** `counter.inc(n)` / `gauge.set(v)`
//!   are single relaxed atomics. Names are stored inline as 64-byte
//!   arrays so no `String` exists past boot.
//! * **No external HTTP framework.** A ~100-line `std::net`
//!   accept loop serves `GET /metrics` with a Prometheus body
//!   serialized into a preallocated buffer.
//! * **Single-threaded HTTP server.** Bind on `127.0.0.1`; the
//!   engine should NOT expose this on a public interface in v1.
//!
//! ## Capacity
//!
//! `MAX_COUNTERS` and `MAX_GAUGES` are fixed at 64 each — more than
//! enough for v1 (we have ~12 hot-path counters and ~8 gauges).
//! Raising them is a const change + recompile.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod registry;
pub mod server;

pub use registry::{
    Counter, CounterId, EncodeErr, Gauge, GaugeId, MetricsRegistry, RegErr, MAX_COUNTERS,
    MAX_GAUGES, NAME_MAX,
};
pub use server::{serve_metrics, MetricsServerErr};
