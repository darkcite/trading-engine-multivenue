// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Per-strategy execution routing — E1 of the real-execution lane.
//!
//! The engine has ONE `Engine<S: Strategy, D: OrderDispatch>` and,
//! before this crate, one global `--paper` / `--live` switch. That is the
//! wrong granularity the moment one member is ready to trade real
//! money while the others are still modelling: a single process-wide
//! flag can only arm all of them or none.
//!
//! This crate moves the decision from the process to the
//! **`Order.strategy_id` byte**, which every order already carries and
//! which `strategy_set`'s `StampCtx` already stamps per member. So:
//!
//! * no wire change, no new `Order` field, no capture-format bump;
//! * no member changes — a member does not know or care;
//! * one masked byte load and one branch per submit.
//!
//! ## The pieces
//!
//! * [`ExecMode`] — paper / live / off, with **paper as the default
//!   for every unanswerable question**.
//! * [`ExecRoute`] — the boot-fixed, cache-line-resident table.
//! * [`RoutedDispatcher`] — the compositing `OrderDispatch` over a
//!   paper arm and a live arm, enforcing LAW E-1 and LAW E-2.
//! * [`NullLiveDispatcher`] — the refusing live arm E1 ships with,
//!   replaced by the Hyperliquid exchange arm in E2/E3.
//! * [`RouteCounters`] — what the router did, for `/metrics` and
//!   `/state`.
//!
//! ## Doctrine
//!
//! Every type here is `#[repr(C)]` where it is shared, cache-line
//! aligned where it is hot, `Copy` where it is a POD, and free of
//! `dyn`, allocation and locks on every path the engine loop touches.
//! The arms are type parameters precisely so the whole dispatch
//! monomorphises: there is no virtual call between a member's intent
//! and the venue.
//!
//! ## What E1 deliberately does NOT do
//!
//! * It does not build a live arm (E2/E3).
//! * It does not consume venue fills — those ride the engine's own
//!   fill lane 3 from E4 (see [`routed`]'s module note).
//! * It does not cancel or modify (E5).
//! * It does not *enforce* the per-slot caps it carries; the risk gate
//!   is E6. The caps are parsed, validated, published in the boot tell
//!   and carried in the table so that E6 adds enforcement and nothing
//!   else.

#![deny(missing_docs)]
// This crate owns every `unsafe` block in the routing path — the
// masked `get_unchecked` loads that make `mode()` / `venue_allowed()`
// branchless. Make "every unsafe block carries a justified SAFETY
// comment" a COMPILER rule rather than a review rule, matching
// `clob-dispatcher`'s crate root.
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

pub mod counters;
pub mod halt;
pub mod ledger;
pub mod mode;
pub mod null;
pub mod route;
pub mod routed;

pub use counters::{RiskRefusal, RouteCounters};
pub use halt::{HaltReason, HaltState};
pub use ledger::{Ledger, LedgerCounters, LEDGER_RESTING, LEDGER_ROWS};
pub use mode::ExecMode;
pub use null::NullLiveDispatcher;
pub use route::{ExecRoute, ExecRouteErr, HaltLimits, SlotCaps, EXEC_SLOTS, EXEC_VENUES};
pub use routed::RoutedDispatcher;
