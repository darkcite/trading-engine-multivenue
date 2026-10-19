// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (PMJ J1b): arbitrary bytes → the rolling families' ingress
//! driver (`ingress_polymarket::pm15_loop::Pm15Driver`), for BOTH periods
//! — the 15-minute pipeline (slot 9) and the 5-minute one (slot 11).
//!
//! Each run binds one window whose token ids are the one-digit `"1"` /
//! `"2"` (so fuzzed `"asset_id":"1"` rows reach the ladder, the touch,
//! the order law and the change-gated top of book), then feeds the bytes
//! as a market frame, as an RTDS frame, and through the roll clock past
//! the window's end and its retirement.
//!
//! Exercises `book15` (the ladder's snapshot load, its rows and its
//! `top_k`), the frame classifier, `tick_size_change`, `last_trade_price`,
//! `market_resolved`, the RTDS report parser and the roll schedule. None
//! of it may panic or read out of bounds on any input. (The rig allocates
//! per run; the driver's steady state is bench gates 114 and 133.)

#![no_main]

use core_ring::Ring;
use core_types::pm_roll::PmPeriod;
use core_types::{ChannelEvent, NullCapture, Tick};
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_loop::{Pm15Counters, Pm15Driver, Pm15Out, Pm15Wire, PM15_EVENT_MASK};
use libfuzzer_sys::fuzz_target;

struct NullWire;

impl Pm15Wire for NullWire {
    fn is_open(&self) -> bool {
        true
    }
    fn queue(&mut self, _parts: &[&[u8]]) -> bool {
        true
    }
}

fuzz_target!(|data: &[u8]| {
    let periods = [(PmPeriod::M15, 1_792_216_800u64), (PmPeriod::M5, 1_792_308_000u64)];
    let mut k = 0usize;
    while k < periods.len() {
        let (period, open) = periods[k];
        k += 1;
        let w = period.window_of_open(open).expect("on the grid");
        let mut spec = PmWindowSpec::EMPTY;
        spec.window = w;
        spec.tick_1e6 = 10_000;
        spec.min_size_1e6 = 5_000_000;
        spec.up.b[0] = b'1';
        spec.up.len = 1;
        spec.down.b[0] = b'2';
        spec.down.len = 1;
        let (mut tick_tx, mut tick_rx) =
            Ring::<Tick, { ingress_polymarket::DEFAULT_TICK_RING_CAP }>::new().split();
        let (mut ev_tx, mut ev_rx) =
            Ring::<ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
        let status = core_metrics::IngressStatus::new();
        let counters = Pm15Counters::new();
        let mut cap = NullCapture;
        let mut wire = NullWire;
        // R-PMJ-S15: the deployed depth policy, so the spacing gate is fuzzed too.
        let mut d = Pm15Driver::new_for(period, &[b"btc"], 5_000).with_depth_policy(true, 1_000);
        let now = (open + 10) * 1_000_000_000;
        let mut out = Pm15Out {
            tick: &mut tick_tx,
            event: &mut ev_tx,
            event_mask: PM15_EVENT_MASK,
            status: &status,
            counters: &counters,
            capture: &mut cap,
        };
        d.offer(&spec, now, now, &mut out, &mut wire);
        let _ = d.on_market_open(&mut wire);
        d.on_market_text(data, now, now, &mut out);
        d.perform(now, &mut out, &mut wire);
        d.on_rtds_text(data, now, &mut out);
        // Past the end and the retirement of either period.
        let late = now + 2_000 * 1_000_000_000;
        d.on_clock(late, late, &mut out, &mut wire);
        d.on_clock(late + 1, late + 1, &mut out, &mut wire);
        d.on_market_text(data, late, late, &mut out);
        d.perform(late, &mut out, &mut wire);
        d.on_market_down(late, &mut out);
        while tick_rx.try_pop_ref().is_some() {}
        while ev_rx.try_pop_ref().is_some() {}
    }
});
