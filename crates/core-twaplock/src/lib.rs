// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-twaplock — pricing a binary settled on a 60-second TWAP, late in its window
//!
//! The H90 law (BIN15 slot 3 on HIP-4; PM15 slot 9 on Polymarket's 15-minute
//! markets) buys the favoured side of a binary in its last seconds, when
//! part of the settlement average is already behind and the rest is a
//! short walk. Two venues, two ways of seeing the reference:
//!
//! - [`late`] — HIP-4: a last-value grid of the oracle mark on the arrival
//!   clock (moved verbatim from `strategy_bin15::late`, which re-exports
//!   it; `parity-3` pins it);
//! - [`report`] — Polymarket: Chainlink's 1 Hz reports on their own second.
//!
//! Both end in [`late::price_twap`], one settlement law. Integer, no
//! allocation after construction, no float.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod late;
pub mod report;
