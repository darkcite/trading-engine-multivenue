// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (`backtest --strategy pine`, PVM lane B'3,
//! impl-Bp §3.6–§3.8, §4.5–§4.7). Allocation and copying are free here: the
//! replay is an operator tool, never the engine loop. None of these modules
//! is a §7.10 hot arm.
//!
//! Repo drift (impl-Bp was written at `90c9c39`): the spec's "slot 7" is
//! `strategy_set::SLOT_PINE` (8) (pin:Bp-slot-pine).
//!
//! - [`cuts`] — the `pvm-cutplan-1` plan, `check_plan`, the span pre-pass,
//!   the §4.6 rebase and boundary law, the local cut source (T10).
//! - [`drive`] — the per-record loop of §4.5 over a [`drive::PineMember`],
//!   `PineCtx` (synchronous intake) and the `OrderDone` delivery (T11).
//! - [`spool`] — the B'3 side of the spool handshake (T12).
//! - [`arm`] — T13's pieces that stand without impl-08 R4/R7/R8: the
//!   one-script rule and `StrategySet` as the drive's member.
//! - [`fees`] — T14's boot half: the fee flag set and digest, the
//!   missing / unknown-class / unmeasured refusals, the `fees.tsv` header,
//!   the outside-git rule.
//!
//! pin:Bp-b3-tests-in-crate — until T13 gives `backtest --strategy pine` its
//! argv surface, the §7.4 tests these tasks own run as unit tests here
//! against a test-only stub member, under their §7.4 names.

pub(crate) mod arm;
pub(crate) mod cuts;
pub(crate) mod drive;
pub(crate) mod fees;
/// T13: `backtest --strategy pine` end to end (boot, drive, report).
pub(crate) mod run;
pub(crate) mod spool;
