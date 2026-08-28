// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-metrics
//!
//! Preallocated counter + gauge registry with a tiny Prometheus
//! text-format HTTP/1.1 server. Designed for the multivenue-engine
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
//! `MAX_COUNTERS = 256` / `MAX_GAUGES = 384` (Phase 8a headroom for
//! five venues' loss-accounting counters and per-bucket gauges).
//! Still fixed arrays, still lock-free; raising further is a const
//! change + recompile.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod ingress_status;
pub mod registry;
pub mod server;

pub use ingress_status::{
    err_site_name, io_kind_code, io_kind_name, IngressState, IngressStatus, SessionErrSnapshot,
    ERR_SITE_DRIVE, ERR_SITE_KEEPALIVE, ERR_SITE_POLL, ERR_SITE_PUMP, ERR_SITE_REGISTER,
    ERR_SITE_REREGISTER, ERR_SITE_SUBSCRIBE_MISSING, ERR_SITE_VENUE_ERROR,
};
pub use registry::{
    Counter, CounterId, EncodeErr, Gauge, GaugeId, MetricsRegistry, RegErr, MAX_COUNTERS,
    MAX_GAUGES, NAME_MAX,
};
pub use server::{serve_metrics, MetricsServeEvent, MetricsServerErr};
