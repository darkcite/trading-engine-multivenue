// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # engine-snapshot
//!
//! RG6 (`docs/regime-and-dashboard-plan.md` §6.1): the engine's
//! observability snapshot and the plumbing that carries it off the
//! engine thread.
//!
//! * [`EngineSnapshot`] — ONE `#[repr(C, align(64))]` `Copy` POD
//!   (≈ 24 KB) holding every dashboard-facing datum: boot identity,
//!   loop counters, latency percentiles, the regime detector's
//!   observables, per-slot strategy counters, the vm's active table
//!   row-by-row, the icdp member, the AI plane, per-venue ingress
//!   health, capture health and the last 64 orders + 64 fills.
//! * [`SnapshotCell`] — the single-writer **seqlock** (generic over
//!   its POD; the `tui` crate's Phase-8a cell, generalized). The
//!   engine thread publishes once per second (one memcpy — the
//!   deliberate, documented copy of plan §8); readers (the `/state`
//!   server thread, the TUI) copy it out under the version bracket.
//! * [`encode_state_json`] — the hand-written JSON writer behind
//!   `GET /state`. No serde, no allocation: it writes straight into
//!   the HTTP response buffer and refuses (never truncates) when the
//!   buffer is too small.
//! * [`RecentRing`] — the fixed order/fill rings the engine keeps for
//!   the `recent` section (the snapshot embeds them by value).
//!
//! ## Doctrine
//!
//! Nothing here runs per tick. The engine's publish is a 1 s timer
//! branch in the cli loop; the JSON encode runs on the server thread.
//! Both are zero-alloc anyway (alloc gate in `crates/bench`) — the
//! snapshot is a plain value, the writer a cursor over a borrowed
//! buffer. Timestamps in the snapshot are ENGINE-MONOTONIC ns; the
//! header carries the wall clock of the same instant so a reader
//! converts (`wall = wall_ns + (t - mono_ns)`).

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

mod cell;
mod json;
mod snapshot;

pub use cell::SnapshotCell;
pub use json::{encode_state_json, JsonOverflow, STATE_JSON_MAX};
pub use snapshot::{
    AiSnapshot, BootInfo, CaptureSnapshot, EngineSnapshot, IcdpSnapshot, IngressSnapshot,
    LatencySnapshot, LoopCounters, RecentRing, VmSnapshot, BOOT_TEXT_MAX, RECENT_FILLS,
    RECENT_ORDERS, RUN_DIR_MAX, SNAPSHOT_SCHEMA, SNAPSHOT_SLOTS, SNAPSHOT_VENUES, SLOT_NAMES,
    VENUE_NAMES,
};
