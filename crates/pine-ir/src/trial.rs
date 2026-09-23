// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The per-trial row that crosses from the tier-1/tier-2 runners (PVM-7,
//! `pine-screen`) into selection (PVM-10, `pvm-select`) — contract §3.16,
//! B19. Report domain: USD ×1e6, P&L in bps ×1e4 of the sizing notional.

/// [`TrialSummary::flags`] bits.
pub mod trial_flag {
    /// The UNJUDGED gate tripped (lane B'): too many cap rejections,
    /// `max_open` refusals or IoC remainders for the numbers to be judged.
    /// A consumer must not score an UNJUDGED trial.
    pub const UNJUDGED: u32 = 1 << 0;
    /// Sized with `SizingMode::FixedNotional` (comparable across scripts).
    pub const FIXED_NOTIONAL: u32 = 1 << 1;
    /// The program `uses_intrabar` and was screened bar-close only: the
    /// number carries an unmeasured tier-1 → tier-2 term.
    pub const INTRABAR_APPROX: u32 = 1 << 2;
    /// The run ended on a script fault (backtest hosts abort; the row
    /// records it).
    pub const FAULTED: u32 = 1 << 3;
}

/// The file extension of a trial's daily return series:
/// `sel/ret/<trial_id>.f64` — little-endian `f64`, one value per UTC day of
/// the trial window, oldest first, no header (its SHA-256 is in the ledger):
/// that day's net P&L, fees included, in bps of the row's
/// `sizing_notional_1e6` (not ×1e4); a flat day is `0.0` (contract §3.16.2).
pub const RET_SERIES_EXT: &str = "f64";

/// One trial's summary. `#[repr(C)]` so a runner appends it to a shard file
/// without serialisation. 88 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TrialSummary {
    /// Bars stepped.
    pub bars: u32,
    /// Trades (open and closed).
    pub trades: u32,
    /// Closed trades; the hold statistics come from these.
    pub closed: u32,
    /// Daily observations written to the return series.
    pub obs_days: u32,
    /// Median hold, bars.
    pub hold_bars_med: u32,
    /// p95 hold, bars (sizes the purge in PBO).
    pub hold_bars_p95: u32,
    /// p99.99 instructions per evaluation (sizes `insn_budget`).
    pub insn_p9999: u32,
    /// Faults.
    pub faults: u32,
    /// Net P&L, bps ×1e4 of `sizing_notional_1e6`, summed; fees included.
    pub net_bps_1e4: i64,
    /// Gross P&L, bps ×1e4 — defined as `net_bps_1e4 + fee_bps_1e4`, exactly.
    pub gross_bps_1e4: i64,
    /// Fees, bps ×1e4 (≥ 0).
    pub fee_bps_1e4: i64,
    /// Max concurrent notional, USD ×1e6 (sizes `cap_instance_1e6`).
    pub max_concurrent_1e6: i64,
    /// p99 daily turnover, USD ×1e6 (sizes `cap_day_1e6`).
    pub turnover_day_p99_1e6: i64,
    /// The unit notional the trial was sized at, USD ×1e6.
    pub sizing_notional_1e6: i64,
    /// [`trial_flag`] bits.
    pub flags: u32,
    /// Always zero.
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<TrialSummary>() == 88);
const _: () = assert!(core::mem::align_of::<TrialSummary>() == 8);
