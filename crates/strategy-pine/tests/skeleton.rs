// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 T4: the inert shell (I3) and the manifest codes' equality with the contract's
//! `pine_ir::DiagCode` table (I6, the `strategy-pine` half).

use core_time::NsTs;
use core_types::{Order, Price, Qty, Tick, VenueId};
use pine_ir::{DiagCode, DiagRange};
use strategy_core::{
    Ctx, PineCounters, PineIncident, PineScriptView, PineSeqRec, PineTapeRec, Strategy,
    StrategyCounters, StrategyError, SubmitErr,
};
use strategy_pine::{PineStrategy, MAX_SCRIPTS, NOT_CONFIGURED, SLOT_PINE};

struct NoCtx {
    submits: u32,
}

impl Ctx for NoCtx {
    fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
        self.submits += 1;
        Ok(())
    }
    fn now_ns(&self) -> NsTs {
        0
    }
}

/// `new` is usable in a const context.
const SHELL: PineStrategy = PineStrategy::new();

#[test]
fn new_is_const_and_inert() {
    let p = SHELL;
    assert_eq!(p.pine_counters(), PineCounters::default());
    let mut v = [PineScriptView::default(); MAX_SCRIPTS];
    assert_eq!(p.pine_scripts_view(&mut v), 0);
    let mut i = [PineIncident::default(); 4];
    assert_eq!(p.pine_incidents_view(&mut i, 0), 0);
    let mut t = [PineTapeRec::default(); 4];
    assert_eq!(p.pine_tape_view(&mut t, 0), 0);
    let mut s = [PineSeqRec::default(); 4];
    assert_eq!(p.pine_seq_view(&mut s, 0), 0);
    assert_eq!(p.timer_period_ns(), u64::MAX);
    assert_eq!(p.strategy_kind(), "pine");
    assert_eq!((p.orders_emitted(), p.orders_dropped()), (0, 0));
}

#[test]
fn on_start_without_configure_refuses() {
    let mut p = PineStrategy::default();
    let mut c = NoCtx { submits: 0 };
    match p.on_start(&mut c) {
        Err(StrategyError::Config(m)) => assert_eq!(m, NOT_CONFIGURED),
        Ok(()) => panic!("an unconfigured slot 8 must refuse the boot (F19)"),
    }
    // The shell drops every callback, counts it, and never submits.
    let t = Tick::new(
        0,
        VenueId::Hyperliquid,
        1,
        1,
        Price::from_raw(1_000_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(1_000_100),
        Qty::from_raw(1_000_000),
    );
    p.on_tick(&t, &mut c);
    p.on_timer(1, &mut c);
    p.on_stop(&mut c);
    assert_eq!(c.submits, 0);
    assert_eq!(p.pine_counters().callbacks_ignored, 2);
    assert_eq!(p.pine_counters().submits, 0);
}

#[test]
fn the_slot_is_eight_and_the_parser_s() {
    assert_eq!(SLOT_PINE, 8);
    assert_eq!(usize::from(SLOT_PINE), core_config::pine::PINE_SLOT);
}

/// I6: `core_config::pine`'s `u16` codes are the contract §3.3 host codes.
#[test]
fn manifest_codes_equal_the_contract_diag_codes() {
    for (code, want) in [
        (core_config::pine::CODE_UNKNOWN_KEY, 6004u16),
        (core_config::pine::CODE_MISSING_KEY, 6005),
        (core_config::pine::CODE_OUT_OF_RANGE, 6006),
        (core_config::pine::CODE_PROVENANCE, 6012),
    ] {
        assert_eq!(DiagCode(code), DiagCode(want));
        assert_eq!(DiagCode(code).range(), DiagRange::Host);
    }
}
