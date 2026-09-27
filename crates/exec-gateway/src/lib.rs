// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # exec-gateway
//!
//! The venue-agnostic half of an asynchronous order gateway (XMM XH4a;
//! ruling O-XH13: built once, reused by Hyperliquid in XH4b, MEXC in XM2
//! and Binance in BX6).
//!
//! ## The shape it serves
//!
//! ```text
//! engine thread ── C (64 B) ──▶ [cmd ring] ──▶ gateway thread (one per venue account)
//!       ▲                                          │ coalesce → encode → sign → frame → socket
//!       └──────── E (64 B) ◀── [event ring] ◀──────┘ replies / order updates / fills, parsed in place
//! ```
//!
//! The engine thread never touches a venue socket: an order becomes one
//! command pushed into a bounded SPSC ring ([`mod@duplex`]), a copy of 64 B
//! and no syscall. The gateway thread owns the sockets, the signing key's
//! use, the in-flight posts ([`pending::Pending`]), the deadlines
//! ([`timer::TimerQueue`]: post timeouts, the engine-sent TTL cancels,
//! keepalives), the connection pair ([`mbb::Mbb`], make-before-break) and
//! its wait policy ([`idle::Idle`]). What the engine thread may read of
//! the gateway (counters, the halt word, latency percentiles) lives in one
//! shared, cache-line-grouped block of atomics ([`stats::GatewayStats`]).
//!
//! ## Laws (risk-policy "XMM — slot 6", XH4a amendment: XG-1 … XG-5)
//!
//! * **XG-1 — the engine's push is a ring write.** No syscall (no
//!   doorbell: [`idle::Idle`]), no allocation, never blocking. A full
//!   command ring refuses the command (counted): the caller fails a
//!   growing order closed; a refused cancel stays its to retry.
//! * **XG-2 — a lost event halts the arm.** The event ring (64 B slots,
//!   `NE ≥ 4·NC`, const-asserted) must never fill: a full ring is counted
//!   and sets the halt word, then fails a `debug_assert!`
//!   ([`duplex::EvtTx::emit`]). The halt word is advisory until a reader
//!   exists: the gateway enforces it itself from XH4b, the engine reads
//!   it from XH4f.
//! * **XG-3 — deadlines.** A growing command the gateway cannot time out
//!   (its [`timer::TimerQueue`] full) is not sent; a cancel always is. A
//!   full queue is a sizing bug that halts.
//! * **XG-4 — make-before-break.** A replaced connection drains (no new
//!   post; its replies still land) and is retired when empty or when its
//!   budget runs out; a dial has a budget too. The backoff resets only
//!   after a healthy session that moved data ([`mbb::Mbb`]).
//! * **XG-5 — one halt registry.** Every reason lives in [`stats`]; the
//!   first reason wins.
//! * **Zero allocation after construction; no `unsafe`.** Every table is
//!   sized by a const generic and allocated once (alloc gate 95); the
//!   rings' unsafe lives in `core-ring`.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc
)]

pub mod duplex;
pub mod idle;
pub mod mbb;
pub mod pending;
pub mod rate;
pub mod stats;
pub mod timer;

pub use duplex::{duplex, CmdRx, CmdTx, EngineEnd, EvtRx, EvtTx, GatewayEnd};
pub use idle::Idle;
pub use mbb::{Mbb, MbbAction, SlotState};
pub use pending::{Pending, PendingErr};
pub use rate::{RateWindow, RATE_WINDOW_S};
pub use stats::{GatewayStats, Lat, HALT_EVENT_RING_FULL, HALT_NONE};
pub use timer::{Expired, TimerFull, TimerId, TimerQueue};

/// A `Box<[V; N]>` filled with `v`, built on the heap at boot (a table of
/// hundreds of records would not fit a gateway or engine thread's stack
/// as a temporary). The one copy: the pending table, the timer heap, and
/// the Hyperliquid gateway's and arm's tables are built with it.
#[must_use]
pub fn boxed_array<V: Copy, const N: usize>(v: V) -> Box<[V; N]> {
    match vec![v; N].into_boxed_slice().try_into() {
        Ok(b) => b,
        Err(_) => unreachable!("a vec of N elements converts to [V; N]"),
    }
}
