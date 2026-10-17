// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.5.7–§4.5.8 (T14): fills. Decode the client oid against this boot's epoch (I25),
//! book through `NativeBroker::on_fill` — the only writer of the position (I30, E-5) — with the
//! measured fee or the model rounded up (I31), report a paper IoC's terminal remainder
//! `on_cancelled` right after its fill (I27), watch late fills (§4.5.8), and owe the
//! `calc_on_order_fills` evaluation (§4.4.5).
//!
//! Hot module (I4): no allocation, no clock (`time_ms` comes from the fill), index loops.
//!
//! Literal readings:
//! - pin:08-fill-measured-fee-absent is RETIRED by R8 (impl-Bp §3.9, impl-08 §3.7):
//!   `core_types::Fill` still carries no fee, so the host hands it over with
//!   `PineStrategy::note_fill_fee(client_oid, fee_1e12)` immediately before `on_fill`; a fill
//!   whose oid matches the noted one books that fee, any other books the model.
//! - pin:08-noted-fee-one-fill: the noted fee is consumed by the very next `on_fill` whatever it
//!   decodes to (dropped fills included); a mismatched oid is a host sequencing defect
//!   (`debug_assert!`; counted on the cold `Core::noted_fee_mismatched` in release, read by
//!   `PineStrategy::noted_fee_mismatched`) and the model is booked.
//! - pin:08-noted-fee-non-negative: a noted fee below 0 is refused fail-fast with `assert!` in
//!   every mode (never clamped to the model): the replay host's fee is a commission, never a
//!   rebate (`backtest::fill` asserts `fee_1e12 >= 0` on the same value). Only the replay host
//!   notes fees (R8), so the engine host's fill path never evaluates it.
//! - pin:08-fill-script-out-of-range: a fill of this boot's epoch naming a script index past the
//!   loaded scripts is counted `fills_foreign` and dropped (no broker owns it).
//! - pin:08-late-flip-closeall-t16 is RETIRED (T16): a late reducer fill that flips the sign
//!   counts `late_reducer_flips` (ALERT), writes a `LATE_FLIP` incident and commits one host
//!   `CloseAll` (`arm::late_flip_guard`, §4.5.8). On the paper matcher a flip needs a fill after
//!   the oid's disposition, which one judgment per IoC never produces; the guard is built anyway.

use crate::boot::HostKind;
use crate::clock::sat_i64;
use crate::oid::decode_client_oid;
use crate::rt::Core;
use crate::settle::{self, kind, RETIRED_CAP};
use crate::SLOT_PINE;
use core_types::{Fill, Side, FILL_ORIGIN_PAPER};
use pine_broker::BrokerFault;
use strategy_core::PineCounters;

/// The model fee ×1e12: `ceil(|px·q| · pointvalue / 1e6 · ppm / 1e6)` in `i128`, saturating
/// (§4.5.7; upward like `fill.rs`).
#[inline]
#[must_use]
pub(crate) fn model_fee_1e12(px_1e6: i64, qty_1e6: i64, pointvalue_1e6: i64, ppm: u32) -> i128 {
    // |px × q| (×1e12) × pv (×1e6) × ppm (×1e6), one ceiling division by 1e12: no intermediate
    // truncation before the round-up.
    const DEN: i128 = 1_000_000_000_000;
    let num = i128::from(px_1e6)
        .saturating_mul(i128::from(qty_1e6))
        .saturating_abs()
        .saturating_mul(i128::from(pointvalue_1e6))
        .saturating_mul(i128::from(ppm));
    let q = num / DEN;
    if num % DEN != 0 {
        q.saturating_add(1)
    } else {
        q
    }
}

/// §4.5.7 `on_fill`. Returns the script that took the fill (its `calc_on_order_fills` latch and
/// reconcile are owed by the caller's callback), or `None` when dropped.
pub(crate) fn on_fill(core: &mut Core, c: &mut PineCounters, f: &Fill) -> Option<usize> {
    // R8 (pin:08-noted-fee-one-fill): the host's fee for THIS fill, consumed here.
    let noted = core.noted_fee.take();
    if let Some((oid_n, fee_n)) = noted {
        // pin:08-noted-fee-non-negative: fail fast, never clamp.
        assert!(
            fee_n >= 0,
            "pine: note_fill_fee: a negative fee (pin:08-noted-fee-non-negative)"
        );
        if oid_n != f.order_id {
            // pin:08-noted-fee-one-fill: counted in release (the model books below).
            core.noted_fee_mismatched = core.noted_fee_mismatched.wrapping_add(1);
            debug_assert_eq!(oid_n, f.order_id, "pine: note_fill_fee named another fill");
        }
    }
    // The set routes attributed fills only; a replay host's fills are unattributed (§4.5.6).
    if core.host == HostKind::Engine && f.strategy_id != SLOT_PINE {
        c.fills_foreign = c.fills_foreign.wrapping_add(1);
        return None;
    }
    let Some((s, oid)) = decode_client_oid(core.epoch, f.order_id) else {
        // A previous boot's oid (HL's userFills snapshot, §4.5.3): never reaches a broker.
        c.fills_prior_boot = c.fills_prior_boot.wrapping_add(1);
        return None;
    };
    let i = usize::from(s);
    if i >= core.scripts.len() {
        c.fills_foreign = c.fills_foreign.wrapping_add(1);
        return None;
    }
    c.fills = c.fills.wrapping_add(1);
    let q_abs = f.qty.raw().saturating_abs();
    let q = if f.side == Side::Bid {
        q_abs
    } else {
        q_abs.saturating_neg()
    };
    let px = f.px.raw();
    // The disposition record knows the primitive; a retired oid charges taker unless the ring
    // remembers a maker (§4.5.7).
    let rec = settle::find(core, i, oid);
    let late = rec.is_none();
    let (maker, reduce) = match rec {
        Some(k) => {
            let r = core.scripts[i].settles[k];
            (r.kind == kind::MAKER, r.reduce)
        }
        None => retired_of(core, i, oid),
    };
    let ppm = if maker {
        core.fee.maker_ppm
    } else {
        core.fee.taker_ppm
    };
    let pv = core.scripts[i].program.meta.chart.pointvalue_1e6;
    let fee = match noted {
        Some((oid_n, fee_n)) if oid_n == f.order_id => fee_n,
        _ => model_fee_1e12(px, q_abs, pv, ppm),
    };
    let time_ms = if f.origin == FILL_ORIGIN_PAPER {
        pine_ir::scale::ms_from_ns(sat_i64(core.anchor.wall_of(f.ts_ns)))
    } else {
        pine_ir::scale::ms_from_ns(sat_i64(f.ts_ns))
    };
    let bar_index = core.rows[core.scripts[i].chart_row].next_index;
    let pos0 = core.scripts[i].broker.view().position_size_1e6;
    match core.scripts[i]
        .broker
        .on_fill(oid, q, px, fee, bar_index, time_ms)
    {
        Ok(()) => {}
        Err(BrokerFault::Internal(d)) => panic!("pine: broker defect in on_fill: {d:?}"),
        Err(BrokerFault::Script(r)) => {
            crate::fault::broker_refusal(core, c, i, r);
            return None;
        }
    }
    core.scripts[i].n_fills = core.scripts[i].n_fills.wrapping_add(1);
    if core.tape.on() {
        // §4.9.2 `FILL`: origin, side, bar, oid, px, signed qty, fee (i64, saturated), time_ms.
        let mut r = crate::tape::rec(
            strategy_core::pine_tape::FILL,
            i,
            f.origin,
            u8::from(f.side != Side::Bid),
            0,
        );
        r.bar_index = bar_index;
        r.oid = oid;
        r.x0 = px;
        r.x1 = q;
        r.x2 = crate::tape::w(i64::try_from(fee).unwrap_or(i64::MAX));
        r.aux = crate::tape::w(time_ms);
        crate::tape::dec(core, c, r);
    }
    // D13 step 5: a flattening script's fill owes it a `flatten` call.
    crate::fault::due_script(core, i);
    if late {
        // A fill for an oid already reported `on_cancelled` (§4.5.5).
        c.late_fills_after_cancel = c.late_fills_after_cancel.wrapping_add(1);
        let pos1 = core.scripts[i].broker.view().position_size_1e6;
        if reduce == 1 && i128::from(pos0) * i128::from(pos1) < 0 {
            // ALERT, a `LATE_FLIP` incident and one host `CloseAll` (§4.5.8, §4.7.8's mechanism).
            c.late_reducer_flips = c.late_reducer_flips.wrapping_add(1);
            crate::arm::late_flip_guard(core, c, i);
        }
    } else if let Some(k) = rec {
        let r = &mut core.scripts[i].settles[k];
        r.filled = r.filled.saturating_add(q_abs);
        let (judge, filled, qty) = (r.kind == kind::JUDGE, r.filled, r.qty);
        if judge {
            // The paper matcher judges an IoC once: this fill is terminal.
            if filled < qty {
                settle::report_cancelled(core, c, i, oid);
            } else {
                core.scripts[i].settles[k] = settle::Settle::FREE;
            }
        } else if filled >= qty {
            // A maker filled at its size: the broker retired it.
            core.scripts[i].settles[k] = settle::Settle::FREE;
        }
    }
    settle::streak_ok(core, i);
    if core.scripts[i].cof {
        core.scripts[i].cof_pending = true;
    }
    // §4.5.1: exit legs activate on fills.
    core.dirty |= 1u32 << i;
    Some(i)
}

/// `(maker, reduce)` of a retired oid of script `i` from the 64-entry ring; `(false, 0)` (taker,
/// not a reducer) when it is older than the ring.
fn retired_of(core: &Core, i: usize, oid: u32) -> (bool, u8) {
    let mut k = 0usize;
    while k < RETIRED_CAP {
        let r = core.retired[k];
        if r.oid == oid && usize::from(r.script) == i {
            return (r.maker == 1, r.reduce);
        }
        k += 1;
    }
    (false, 0)
}

#[cfg(test)]
mod tests {
    use super::model_fee_1e12;

    /// I31: the model fee rounds UP (`fill.rs`'s rule) in `i128`, at the point value.
    #[test]
    fn model_fee_rounds_up() {
        // 0.118 @ 101.02 = 11.92036 USD; 450 ppm = 0.005364162 USD → ×1e12, exact.
        assert_eq!(
            model_fee_1e12(101_020_000, 118_000, 1_000_000, 450),
            5_364_162_000
        );
        // One unit past a whole ×1e12 rounds up: 1e-12 USD of value at 1 ppm → 1 (not 0).
        assert_eq!(model_fee_1e12(1, 1, 1_000_000, 1), 1);
        // Sign-free, zero ppm is zero, the point value scales.
        assert_eq!(model_fee_1e12(100_000_000, -2_000_000, 1_000_000, 0), 0);
        assert_eq!(
            model_fee_1e12(100_000_000, -2_000_000, 500_000, 1_000),
            model_fee_1e12(100_000_000, 2_000_000, 1_000_000, 500)
        );
        // Saturates instead of overflowing.
        assert!(model_fee_1e12(i64::MAX, i64::MAX, i64::MAX, 100_000) > 0);
    }
}
