// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BIN15 H90 — the LATE-WINDOW pricer. It lives in `core-twaplock` since
//! PM15-4 (2026-10-17), moved verbatim, so slot 9's Polymarket law shares
//! its settlement arithmetic; this module re-exports it, and every
//! `strategy_bin15::late::…` path (the member, `bin15_boot`, the `parity`
//! harness) is unchanged.

pub use core_twaplock::late::*;
