// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-hyperevm (HYPARB H3)
//!
//! HyperEVM pool-event ingress: one JSON-RPC WebSocket session that
//!
//! 1. subscribes `newHeads` and the pool logs (`Swap`, `Mint`, `Burn`,
//!    and Algebra's `Fee` / `SwapFee`) of every pool in the table;
//! 2. snapshots every pool — price, liquidity, fee, spacing and the
//!    initialised ticks around the price — at ONE pinned block over the
//!    same session, with the O-H4 archive probe;
//! 3. publishes the snapshot, then every event after it, as `Signal`s
//!    whose 40-byte payload is `core_amm::payload` — the member decodes
//!    exactly what was encoded here, and the capture tape carries the
//!    maps a replay needs.
//!
//! Three pool families (O-H19): Uniswap V3 ABI, Slipstream (Hybra CL)
//! and Algebra Integral v1.0 / v1.2 ([`PoolFamily`]).
//!
//! Template: `ingress-rpc` (transport states, two-phase frame dispatch,
//! capture-before-push, the observability slot). Zero-alloc after
//! [`Driver::new`]; every scanner is a byte scan over the rx buffer and
//! every ABI word is read straight out of its hex digits ([`hex`]).
//!
//! **Not wired yet.** The engine lane, the tap venue byte and the
//! `SignalSource::HyperEvm` variant land with H0/H3b after MEXC's MX2;
//! until then [`SIGNAL_SOURCE_HYPEREVM`] names the wire value.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod hex;
pub mod logs;
pub mod pools;
pub mod rpc;
pub mod run_loop;
pub mod snapshot;

pub use logs::{parse_log, payloads, LogErr, LogMeta, PoolLog, SUBSCRIBED_TOPICS};
pub use pools::{PoolEntry, PoolFamily, PoolTable, PoolTableErr, HYPEREVM_MAX_POOLS};
pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, HyperEvmCounters, Phase, RpcKind, RunResult,
    State, StopFlag, SubKind, DEFAULT_POOL_RING_CAP, DEFAULT_SNAPSHOT_RADIUS, HOLD_CAP,
    PENDING_CAP, RPC_POLL_NS, RX_BUF_SIZE, SIGNAL_SOURCE_HYPEREVM, SUB_CAP, TX_BUF_SIZE,
};
pub use snapshot::{
    Call, ReadKind, SnapCounters, SnapErr, SnapState, Snapshotter, CALLDATA_MAX, MAP_NODES,
    MAX_BITMAP_WORDS, PROBE_DEPTH,
};

#[cfg(test)]
mod testnode;
