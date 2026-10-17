// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `pine-broker` — TradingView broker emulator + native order mapper (PVM-5)
//!
//! Two brokers over one order model and one book:
//! * `TvBroker` — fills intents against a historical `Bar` along TradingView's documented
//!   intrabar path. Conformance and the tier-1 screen.
//! * `NativeBroker` — turns committed intents into `EngineIntent`s for the host's dispatcher;
//!   the position comes from `on_fill` only (LAW E-5). Paper, live, tier-2 replay.
//!
//! Doctrine: `pine-ir` is the only dependency; no clock (P5); no global state (P2); no `f64`
//! (D18: price/qty `i64 ×1e6`, money `i128 ×1e12`) — save the script-visible net profit of
//! `visible` (CAR-R33-1), which no broker law reads; no `dyn`; one heap allocation per broker, in
//! `new`; no allocation after it; intents COMMIT (W21).

#![deny(missing_docs, unused_must_use, unreachable_pub, unsafe_op_in_unsafe_fn)]
#![deny(
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

mod book;
mod buf;
mod closed;
mod decl;
mod exit;
mod fault;
mod intent;
mod money;
mod native;
mod orders;
mod risk;
mod signal;
mod size;
mod switch;
mod tv;
mod visible;

pub use buf::EngineIntentBuf;
pub use fault::{code, BrokerFault, InternalDefect, ScriptRefusal};
pub use native::NativeBroker;
pub use tv::{TvBroker, TvStep};

/// Fixed capacities. Each overflow is a named refusal, never a growth (crate-private: hosts read
/// `BrokerView`, not these). INCREMENT: the remaining §3.1 capacities land with their tables.
pub(crate) mod cap {
    /// Pending entry / `strategy.order` / close orders per broker.
    pub(crate) const ORDERS: usize = 64;
    /// Live `strategy.exit` rules.
    pub(crate) const EXIT_RULES: usize = 32;
    /// Exit leg sets (one per rule × covered trade), §4.5.
    pub(crate) const LEG_SETS: usize = 128;
    /// Live OCA groups.
    pub(crate) const OCA_GROUPS: usize = 16;
    /// Open trades; a power of two (ring index).
    pub(crate) const LOTS: usize = 128;
    /// Closed trades retained — TradingView's documented 9000 (§4.6.6).
    pub(crate) const CLOSED_RETAIN: usize = 9000;
    /// v5 only, TV mode: orders a script may create; the 9001st faults the run that creates it
    /// (`pin:05-v5-order-limit`, CI-005 / E110: RE10138 "Maximum number of orders (9000) was
    /// reached.", §4.6.6).
    pub(crate) const V5_ORDERS: u64 = 9000;
    /// Native: sent orders tracked — one IoC, ≤ `max_open − 1` resting makers and ≤ `MAX_OPEN`
    /// replaced predecessors (§4.12.6).
    pub(crate) const SENT: usize = 16;
    /// Native: retired client-oid records kept for late fills (a power of two).
    pub(crate) const ATTRIB_RING: usize = 1024;
    /// Largest `pyramiding` accepted (`5102` above it).
    pub(crate) const PYRAMIDING_MAX: u32 = 100;
    /// Native: largest `cross_ticks` accepted (`5105` above it).
    pub(crate) const MAX_CROSS_TICKS: u32 = 1000;
    /// Native: largest `max_open` accepted (`5105` above it) — `core_fill::MAX_OPEN_PER_SYM`
    /// (`crates/core-fill/src/lib.rs:66`), restated because this crate may not depend on `core-fill`.
    pub(crate) const MAX_OPEN: u32 = 8;
    /// Native: single-direction ids (their `ProgramMeta::entry_dirs` rows OR-merged) retained for
    /// the D19 boot check (A6.3; `pin:05-entry-dirs-cap` — the smallest ids are kept, any other id
    /// never reaches the boot trade, fail-closed).
    pub(crate) const BOOT_IDS: usize = 256;
}
