// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T15, impl-Bp §4.11). Allocation and
//! copying are free here: the accumulators are reserved at boot and the rows
//! and series written once after the last record.
//!
//! Every row B'3 writes comes out of impl-07's one accumulator,
//! `pine_screen::rows::TrialAccum`, one per arm, fed in record order:
//! R7's bar marks (drained after EVERY record — the member's ring holds
//! `BAR_MARKS_CAP`), R4's closed trades as `closed_count` grows, and the
//! commission R8 booked from the harness fee law. The rows go through
//! `RowsWriter` (`RowsHdr { tier: 2 }`), the series through `write_ret`.
//!
//! Literal readings:
//! - pin:Bp-baseline-first-window-mark — §4.11 "at the first bar mark with
//!   `open_ms ≥ from_ms`": that mark's equity and the commission read after
//!   its record; the mark itself is then stepped (`on_bar`).
//! - pin:Bp-baseline-none-in-window — a completed arm no mark of which
//!   reached the window is baselined at finish on its last equity and
//!   commission (flat series, net 0): `finish` requires a baseline.
//! - pin:Bp-closed-retained-9000 — R4's `closed_trade` is RETAINED-indexed;
//!   the broker retains `min(closedtrades, 9000)` (impl-05 §3.6.1, TV's
//!   cap), so absolute trade `k` is retained `k − (count − min(count,
//!   9000))`; strategy-pine exposes no `closed_retained` (request R4b).
//! - pin:Bp-open-trades-unread — R4 exposes no open-trade read-back, so the
//!   signal arm feeds no `on_open_trade_at_end` (request R4c); `trades`
//!   counts the closed ones only until it lands.
//! - pin:Bp-accum-reserve-window — `reserve(window bars + 2, window days)`:
//!   bars = the window span over the timeframe; the accumulator clamps a
//!   longer hold into its last bucket and ignores a bar index past it.
//! - pin:Bp-fee-drain-per-cut — the model's pine fee charges are drained
//!   into the ledger after every cut (and once at the end), so the charge
//!   vector never holds more than one cut's fills.
//! - pin:Bp-faulted-no-series — a FAULTED row is written; it gets no series
//!   file (impl-07 §4.5.2) and its report `ret_sha256` is empty.

use std::path::Path;

use pine_ir::{Bar, SizingMode, TradeRec, TrialSummary};
use pine_screen::rows::{AccumCfg, RowRec, RowsHdr, RowsWriter, TrialAccum, TrialEnd};
use strategy_set::StrategySet;

use super::drive::{DriveHook, SignalRec};
use super::fees::{ArmFees, FeeLedger, FeeProvenance};
use crate::backtest::fill::{FeeCharge, FillEngine, MAX_OPEN_TOTAL};

/// pin:Bp-closed-retained-9000.
pub(crate) const CLOSED_RETAINED_CAP: u32 = 9_000;

const DAY_MS: i64 = 86_400_000;

/// The accumulator's configuration for one tier-2 arm (§4.11).
pub(crate) fn accum_cfg(
    notional_1e6: i64,
    pointvalue_1e6: i64,
    from_ms: i64,
    to_ms: i64,
) -> AccumCfg {
    AccumCfg {
        sizing: SizingMode::FixedNotional { notional_1e6 },
        pointvalue_1e6,
        from_ms,
        to_ms,
        intrabar_approx: false,
    }
}

/// One arm's `TrialAccum` and what feeding it needs.
pub(crate) struct ArmAccum {
    acc: TrialAccum,
    from_ms: i64,
    based: bool,
    closed_seen: u32,
    last_equity_1e12: i128,
    last_commission_1e12: i128,
}

impl ArmAccum {
    /// Reserved and begun (pin:Bp-accum-reserve-window).
    pub(crate) fn new(cfg: &AccumCfg, tf_s: u32) -> Self {
        let span = (cfg.to_ms - cfg.from_ms).max(0);
        let days = u32::try_from(span.div_euclid(DAY_MS)).unwrap_or(u32::MAX);
        let tf_ms = i64::from(tf_s.max(1)) * 1_000;
        let bars = u32::try_from(span / tf_ms + 2).unwrap_or(u32::MAX);
        let mut acc = TrialAccum::new();
        acc.reserve(bars, days);
        acc.begin(cfg);
        Self {
            acc,
            from_ms: cfg.from_ms,
            based: false,
            closed_seen: 0,
            last_equity_1e12: 0,
            last_commission_1e12: 0,
        }
    }

    /// One bar mark (R7) with the arm's commission at that instant.
    pub(crate) fn on_mark(&mut self, bar: &Bar, equity_1e12: i128, commission_1e12: i128) {
        if !self.based && bar.open_ms >= self.from_ms {
            self.acc.baseline(equity_1e12, commission_1e12);
            self.based = true;
        }
        self.acc.on_bar(bar, equity_1e12);
        self.last_equity_1e12 = equity_1e12;
        self.last_commission_1e12 = commission_1e12;
    }

    /// One closed trade, once.
    pub(crate) fn on_closed(&mut self, t: &TradeRec) {
        self.acc.on_closed_trade(t);
    }

    /// The row (§4.11 `finish`). pin:Bp-baseline-none-in-window.
    pub(crate) fn finish(
        &mut self,
        commission_1e12: i128,
        max_insns: u32,
        end: TrialEnd,
        extra_flags: u32,
    ) -> TrialSummary {
        if !self.based && end == TrialEnd::Completed {
            self.acc
                .baseline(self.last_equity_1e12, self.last_commission_1e12);
            self.based = true;
        }
        self.acc
            .finish(commission_1e12, max_insns, end, extra_flags)
    }

    /// The daily series of the last `finish` (empty for a FAULTED row).
    pub(crate) fn day_returns(&self) -> &[f64] {
        self.acc.day_returns()
    }
}

/// The signal arm beside the drive: R7 marks and R4 closes after every
/// record, the fee ledger per cut.
pub(crate) struct SignalHook<'p> {
    pub(crate) arm: ArmAccum,
    marks: [(Bar, i128); strategy_pine::BAR_MARKS_CAP],
    charges: Vec<FeeCharge>,
    pub(crate) ledger: FeeLedger,
    prov: &'p FeeProvenance,
    descriptors: &'p [(u32, String)],
    /// The signal arm's `fees.tsv` tally (every drain summed).
    pub(crate) fees: ArmFees,
    /// The most charges one drain carried (the per-cut bound, pinned by a test).
    pub(crate) max_drain: usize,
}

impl<'p> SignalHook<'p> {
    pub(crate) fn new(
        arm: ArmAccum,
        prov: &'p FeeProvenance,
        descriptors: &'p [(u32, String)],
    ) -> Self {
        Self {
            arm,
            marks: [(Bar::default(), 0); strategy_pine::BAR_MARKS_CAP],
            charges: Vec::with_capacity(MAX_OPEN_TOTAL),
            ledger: FeeLedger::new(prov),
            prov,
            descriptors,
            fees: ArmFees::default(),
            max_drain: 0,
        }
    }

    /// Drain the model's fee charges into the ledger (pin:Bp-fee-drain-per-cut).
    pub(crate) fn drain_fees(&mut self, fills: &mut FillEngine) {
        fills.take_fee_charges(&mut self.charges);
        self.max_drain = self.max_drain.max(self.charges.len());
        let d = self.descriptors;
        let a = self
            .ledger
            .push_arm(self.prov, "signal", &self.charges, |s| {
                d.iter().find(|(x, _)| *x == s).map(|(_, n)| n.as_str())
            });
        self.fees.rows += a.rows;
        self.fees.fee_1e12 += a.fee_1e12;
        self.fees.undeclared += a.undeclared;
        self.charges.clear();
    }
}

impl DriveHook<StrategySet> for SignalHook<'_> {
    fn after_record(&mut self, m: &mut StrategySet, _fills: &mut FillEngine, _rec: &SignalRec<'_>) {
        let n = m.pine_mut().take_bar_marks(0, &mut self.marks);
        let p = m.pine();
        if n > 0 {
            let c = p.broker_record(0).map_or(0, |r| r.commission_paid_1e12);
            let mut j = 0;
            while j < n {
                let (bar, eq) = self.marks[j];
                self.arm.on_mark(&bar, eq, c);
                j += 1;
            }
        }
        let count = p.closed_count(0);
        let first = count - count.min(CLOSED_RETAINED_CAP);
        while self.arm.closed_seen < count {
            let k = self.arm.closed_seen;
            if k >= first {
                if let Some(t) = p.closed_trade(0, k - first) {
                    self.arm.on_closed(t);
                }
            }
            self.arm.closed_seen += 1;
        }
    }

    fn after_cut(&mut self, _m: &mut StrategySet, fills: &mut FillEngine) {
        self.drain_fees(fills);
    }
}

/// One written arm: its id, row, and series digest (`None` = FAULTED).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArmRow {
    pub(crate) trial_id: [u8; 32],
    pub(crate) summary: TrialSummary,
    pub(crate) ret_sha256: Option<[u8; 32]>,
}

/// §4.11 "Writing": one `RowRec` per arm, in the order given (trial-id
/// order: signal first, then the nulls mode-major), into one
/// `RowsHdr { tier: 2 }` file; one series per non-FAULTED arm.
pub(crate) fn write_rows(
    rows_out: &Path,
    ret_dir: &Path,
    notional_1e6: i64,
    fee_digest: [u8; 32],
    arms: &mut [(ArmRow, &[f64])],
) -> Result<(), String> {
    if let Some(d) = rows_out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::create_dir_all(ret_dir).map_err(|e| format!("{}: {e}", ret_dir.display()))?;
    let mut w = RowsWriter::open(rows_out, &RowsHdr::new(2, notional_1e6, fee_digest))
        .map_err(|e| format!("--rows-out {}: {e:?}", rows_out.display()))?;
    for (a, series) in arms.iter_mut() {
        w.append(&RowRec {
            trial_id: a.trial_id,
            summary: a.summary,
        })
        .map_err(|e| format!("--rows-out: {e:?}"))?;
        if a.summary.flags & pine_ir::trial_flag::FAULTED == 0 {
            let sha = pine_screen::ret::write_ret(ret_dir, &a.trial_id, series)
                .map_err(|e| format!("--ret-dir {}: {e}", ret_dir.display()))?;
            a.ret_sha256 = Some(sha);
        }
    }
    w.finish().map_err(|e| format!("--rows-out: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(index: u32, open_ms: i64) -> Bar {
        Bar {
            open_ms,
            close_ms: open_ms + 60_000,
            index,
            ..Bar::default()
        }
    }

    #[test]
    fn the_baseline_is_the_first_window_mark() {
        let cfg = accum_cfg(1_000_000, 1_000_000, 0, DAY_MS);
        let mut a = ArmAccum::new(&cfg, 60);
        a.on_mark(&bar(0, -60_000), 5, 0); // warm-up side: not the baseline
        a.on_mark(&bar(1, 0), 10_000_000_000, 0);
        a.on_mark(&bar(2, 60_000), 10_000_000_000 + 1_000_000_000_000, 0);
        let s = a.finish(0, 7, TrialEnd::Completed, 0);
        // +1 USD over the 1 USD notional = 10 000 bps = ×1e4 → 1e8.
        assert_eq!(s.net_bps_1e4, 100_000_000);
        assert_eq!(s.gross_bps_1e4, s.net_bps_1e4 + s.fee_bps_1e4);
        assert_eq!(s.insn_p9999, 7);
        assert_eq!(a.day_returns().len(), 1);
    }

    #[test]
    fn an_arm_with_no_window_mark_finishes_flat() {
        let cfg = accum_cfg(1_000_000, 1_000_000, DAY_MS, 2 * DAY_MS);
        let mut a = ArmAccum::new(&cfg, 60);
        a.on_mark(&bar(0, 0), 123, 0);
        let s = a.finish(0, 0, TrialEnd::Completed, 0);
        assert_eq!(s.net_bps_1e4, 0);
        assert_eq!(a.day_returns(), &[0.0]);
    }

    #[test]
    fn a_faulted_arm_has_no_series() {
        let cfg = accum_cfg(1_000_000, 1_000_000, 0, DAY_MS);
        let mut a = ArmAccum::new(&cfg, 60);
        a.on_mark(&bar(1, 0), 0, 0);
        let s = a.finish(0, 0, TrialEnd::Faulted, 0);
        assert_ne!(s.flags & pine_ir::trial_flag::FAULTED, 0);
        assert!(a.day_returns().is_empty());
    }
}
