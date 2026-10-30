// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T13, impl-Bp §4.5). Allocation and
//! copying are free here.
//!
//! The pine arm's pieces that stand without impl-08's R4/R7/R8: the
//! one-script rule (exit 6 before any record) and `StrategySet` as the
//! drive's [`PineMember`] (the set's slot 8 is the runtime, §4.5).
//!
//! - pin:Bp-r3-withdrawn — R3 is withdrawn (§3.9 status, 2026-09-23): in
//!   `HostKind::Replay` the member settles its own dispositions at every
//!   callback (impl-08 §4.5), so the set takes the model's `OrderDone` as
//!   a no-op; the model still produces it (the null arms and T15 read it).
//! - pin:Bp-fault-by-counter — the fault abort reads the set's
//!   `PineCounters::scripts_faulted` (a public counter) after each record;
//!   impl-08 R4 `fault(script)` adds the `(FaultKind, pc, bar)` triple.
//! - pin:Bp-one-script-zero — a manifest with no `[[script]]` refuses like
//!   one with two (exit 6): a trial is exactly one candidate.

use core_config::pine::{PineManifest, PineScriptCfg};
use strategy_core::{Ctx, StrategyCounters};
use strategy_set::StrategySet;

use super::drive::{DriveFail, PineMember};
use crate::backtest::fill::OrderDone;
use crate::backtest::HarnessError;

/// The exit of a refusal before the first replayed record (§3.6,
/// impl-10 R-T2-8): nothing is written.
pub(crate) const EXIT_PRE_RECORD: u8 = 6;

/// §4.5 "One script per trial": the manifest's one `[[script]]`, or the
/// exit-6 refusal (two scripts on one fill model would contend for the
/// same displayed size).
pub(crate) fn one_script(m: &PineManifest) -> Result<&PineScriptCfg, DriveFail> {
    match m.scripts.as_slice() {
        [one] => Ok(one),
        many => Err(DriveFail {
            exit: EXIT_PRE_RECORD,
            err: HarnessError::Usage(format!(
                "--strategy pine: the manifest holds {} [[script]] blocks; a tier-2 trial \
                 replays exactly one",
                many.len()
            )),
        }),
    }
}

impl PineMember for StrategySet {
    fn on_order_done<C: Ctx>(&mut self, _done: &OrderDone, _ctx: &mut C) {}

    fn faulted(&self) -> bool {
        self.pine().pine_counters().scripts_faulted > 0
    }

    fn note_fill_fee(&mut self, client_oid: u64, fee_1e12: i128) {
        self.pine_mut().note_fill_fee(client_oid, fee_1e12);
    }

    /// pin:Bp-fill-stamp-paper — a replayed fill is a model fill: slot 8's
    /// attribution and the PAPER origin (whose fill time is the fill's own
    /// stamp through the member's anchor, impl-08 `fills.rs`).
    fn stamp_fill(&self, fill: &mut core_types::Fill) {
        fill.strategy_id = strategy_set::SLOT_PINE;
        fill.origin = core_types::FILL_ORIGIN_PAPER;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(sym: &str) -> String {
        format!(
            "[[script]]\nsha256 = \"{}\"\nfile = \"s.pine\"\ndialect = 6\nenabled = 1\n\
             mode = \"bar_close\"\nsymbol = \"{sym}\"\ntimeframe_s = 3600\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
             coalesce_bps = 0\ninsn_budget = 2000000\nqty_notional_1e6 = 12000000\n\
             limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 12000000\n\
             cap_day_1e6 = 48000000\nmax_open = 4\ncollection_cap = 4096\n\
             str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
             intent_cap = 32\n",
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        )
    }

    #[test]
    fn a_manifest_with_two_scripts_exits_6() {
        let two = format!("{}{}", script("hyperliquid:btc"), script("hyperliquid:eth"));
        let m = core_config::pine::parse_manifest(&two).unwrap();
        let e = one_script(&m).unwrap_err();
        assert_eq!(e.exit, 6);
        assert!(e.err.to_string().contains("2 [[script]]"));
        let one = core_config::pine::parse_manifest(&script("hyperliquid:btc")).unwrap();
        assert_eq!(one_script(&one).unwrap().symbol, "hyperliquid:btc");
    }

    #[test]
    fn a_set_with_no_faulted_script_is_not_faulted() {
        // The set booted with slot 8 only and nothing loaded: no fault.
        let set = StrategySet::new(strategy_set::BIT_PINE);
        assert!(!PineMember::faulted(&set));
    }
}
