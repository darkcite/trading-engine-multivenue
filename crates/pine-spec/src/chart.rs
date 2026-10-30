// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The frozen conformance chart (D10), from `tests/conformance/chart.json`. Definition only:
//! no price data. The capture stamp (`captured`, `YYYY-MM-DDTHH:MMZ`) and the TradingView build
//! are per capture and live in each fixture's header (contract §3.16.5, impl-06 §4.4.1), never here.

include!(concat!(env!("OUT_DIR"), "/chart_gen.rs"));

/// Bar length in milliseconds.
pub const TF_MS: i64 = TIMEFRAME_S as i64 * 1000;
/// Exclusive end of the judged window (UTC ms).
pub const TO_MS: i64 = FROM_MS + BARS as i64 * TF_MS;
/// First bar every replay feeds: the warm-up starts here (UTC ms).
pub const REPLAY_FROM_MS: i64 = FROM_MS - WARMUP_BARS as i64 * TF_MS;
/// `pine_ir::symbol_hash` of [`SYMBOL_KEY`] — the bar-file / `ChartInfo` key.
pub const SYMBOL_HASH: u64 = pine_ir::symbol_hash(SYMBOL_KEY.as_bytes());
/// Last UTC ms at which a 10 000-bar plan (Essential/Plus, TradingView Limitations page) still
/// loads [`REPLAY_FROM_MS`]: a capture after it cannot see the whole replay range.
pub const CAPTURE_DEADLINE_10K_MS: i64 = REPLAY_FROM_MS + 10_000 * TF_MS;
