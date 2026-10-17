// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.9 **R7** (adopted by impl-08 §3.7): bar marks — each confirmed chart bar with
//! `BrokerView::equity_1e12` after its evaluation, drained by the replay host after every record
//! (`PineStrategy::take_bar_marks`) into `TrialAccum::on_bar`.
//!
//! Hot module on the replay host (I4): a fixed ring per script boxed at `on_start`, POD stores,
//! index loops; no allocation, no clock.
//!
//! Literal readings:
//! - pin:08-bar-marks-replay-only: only a `HostKind::Replay` member records marks (B'3 is the one
//!   reader); the engine host records none and carries no ring, so paper pays nothing.
//! - pin:08-bar-mark-every-chart-confirm: a mark is written for every live confirmed bar of the
//!   script's chart row, after `live_script` (its evaluation, or none when the script is
//!   disabled), with the equity the view holds then. Warm-up bars (`on_start`'s walk) are not
//!   marks: their intents are discarded (§4.3.6) and no host is draining yet.
//! - pin:08-bar-marks-undrained-panics: a row confirms at most one bar per callback, so a host
//!   that drains after every record never holds more than one; a full ring means the host did
//!   not drain — a sequencing defect, fail fast (pin:08-boot-sequencing-asserts' rule).

use crate::boot::HostKind;
use crate::rt::Core;
use crate::BAR_MARKS_CAP;
use pine_ir::Bar;

/// R7: record script `i`'s mark for its confirmed chart bar `bar` (replay host only).
#[inline]
pub(crate) fn record(core: &mut Core, i: usize, bar: &Bar) {
    if core.host != HostKind::Replay {
        return;
    }
    let n = usize::from(core.scripts[i].marks_n);
    assert!(
        n < BAR_MARKS_CAP,
        "pine: bar marks not drained (take_bar_marks after every record)"
    );
    let eq = core.scripts[i].broker.view().equity_1e12;
    // COPY: one `(Bar, i128)` (128 B) into the script's mark slot — R7's by-value shape — a
    // borrow rejected: the row overwrites its last bar at the next confirm.
    core.marks[i * BAR_MARKS_CAP + n] = (*bar, eq);
    core.scripts[i].marks_n = u8::try_from(n + 1).unwrap_or(u8::MAX);
}

/// R7: move script `i`'s marks, oldest first, into `out` (as many as fit); the rest stay, oldest
/// first. Returns the count moved.
pub(crate) fn take(core: &mut Core, i: usize, out: &mut [(Bar, i128)]) -> usize {
    let n = usize::from(core.scripts[i].marks_n);
    let k = n.min(out.len());
    let base = i * BAR_MARKS_CAP;
    let mut j = 0usize;
    while j < k {
        // COPY: one mark (128 B) out to the host's buffer — R7's `&mut [(Bar, i128)]` contract —
        // a borrow rejected: the slot is reused at the next confirm.
        out[j] = core.marks[base + j];
        j += 1;
    }
    // Keep the rest, oldest first (only when the host's buffer was short).
    let mut r = k;
    while r < n {
        // COPY: one mark (128 B) down the ring — cold, only on a short host buffer — a head index
        // rejected: one `marks_n` keeps the ring trivially ordered.
        core.marks[base + r - k] = core.marks[base + r];
        r += 1;
    }
    core.scripts[i].marks_n = u8::try_from(n - k).unwrap_or(0);
    k
}
