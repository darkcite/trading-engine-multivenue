// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 §7.2: the Pine PODs' layouts (I2) and the five defaulted accessors.

use strategy_core::{
    PineCounters, PineIncident, PineScriptView, PineSeqRec, PineTapeRec, StrategyCounters,
    PINE_VIEW_SCRIPTS,
};

#[test]
fn pine_pods_sizes_and_alignment() {
    assert_eq!(core::mem::size_of::<PineScriptView>(), 128);
    assert_eq!(core::mem::size_of::<PineIncident>(), 128);
    assert_eq!(core::mem::align_of::<PineIncident>(), 64);
    assert_eq!(core::mem::size_of::<PineSeqRec>(), 128);
    assert_eq!(core::mem::align_of::<PineSeqRec>(), 64);
    assert_eq!(core::mem::size_of::<PineTapeRec>(), 64);
    assert_eq!(core::mem::align_of::<PineTapeRec>(), 64);
    assert_eq!(core::mem::size_of::<PineCounters>(), 42 * 8);
    assert_eq!(PINE_VIEW_SCRIPTS, 8);
}

#[test]
fn pine_script_view_has_no_interior_padding() {
    // 12 eight-byte words, 5 u32, the u32 symbol id, 8 u8 — summed field by field.
    let fields = 12 * 8 + 5 * 4 + core::mem::size_of::<core_types::SymbolId>() + 8;
    assert_eq!(fields, core::mem::size_of::<PineScriptView>());
    // The trailing u8 run ends the struct exactly: the last field sits at 127.
    let v = PineScriptView::default();
    let base = core::ptr::addr_of!(v) as usize;
    assert_eq!(
        core::ptr::addr_of!(v.entries_suppressed) as usize - base,
        127
    );
    assert_eq!(core::ptr::addr_of!(v.sym) as usize - base, 116);
    // The tape record's integer fields fill its 64 B with no hole either.
    let t = PineTapeRec::default();
    let tb = core::ptr::addr_of!(t) as usize;
    assert_eq!(core::ptr::addr_of!(t.c) as usize - tb, 60);
}

struct Bare;
impl StrategyCounters for Bare {}

#[test]
fn default_accessors_report_nothing() {
    let b = Bare;
    assert_eq!(b.pine_counters(), PineCounters::default());
    let mut v = [PineScriptView::default(); PINE_VIEW_SCRIPTS];
    assert_eq!(b.pine_scripts_view(&mut v), 0);
    let mut i = [PineIncident::default(); 2];
    assert_eq!(b.pine_incidents_view(&mut i, 0), 0);
    let mut t = [PineTapeRec::default(); 2];
    assert_eq!(b.pine_tape_view(&mut t, 0), 0);
    let mut s = [PineSeqRec::default(); 2];
    assert_eq!(b.pine_seq_view(&mut s, 0), 0);
    assert_eq!(
        v[0],
        PineScriptView::default(),
        "the default writes nothing"
    );
}
