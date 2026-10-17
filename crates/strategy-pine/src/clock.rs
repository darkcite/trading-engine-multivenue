// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.4.1 (T12): the event clock **E** and the time domains. E is the running maximum of
//! the consumed inputs' `ts_ns` (ticks and Trade events of pine symbols); fills never advance it.
//! Every time quantity the member hands out is derived from E or from event data — never from a
//! clock (P5): `ctx.now_ns()` is not read anywhere on the decision path.
//!
//! | quantity | value |
//! |---|---|
//! | `now_ns` to `NativeBroker` (UTC ns) | `anchor.wall_of(E)` |
//! | `Quote::ts_ns` (UTC ns) | `anchor.wall_of(tick.ts_ns)` |
//! | `BarCtx::now_ms` (UTC ms) | `ms_from_ns(sat_i64(now_ns))` |
//! | `Order::ts_ns`, `CancelReq::ts_ns` (monotonic ns) | E |
//!
//! Hot module (I4): no allocation, no clock.

use crate::rt::Core;
use core_types::{Tick, TICK_FLAG_STALE};
use pine_ir::Quote;
use strategy_core::PineCounters;

/// `i64::try_from(x).unwrap_or(i64::MAX)`: no `as` on the time path.
#[inline(always)]
pub(crate) fn sat_i64(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// `E = max(E, ts)`; a regression leaves E and counts (§4.4.1).
#[inline]
pub(crate) fn advance(core: &mut Core, c: &mut PineCounters, ts_ns: core_time::NsTs) {
    if ts_ns < core.e_ns {
        c.event_ts_regressions = c.event_ts_regressions.wrapping_add(1);
    } else {
        core.e_ns = ts_ns;
    }
}

/// The broker's `now_ns`: UTC ns of E.
#[inline]
pub(crate) fn now_ns(core: &Core) -> u64 {
    core.anchor.wall_of(core.e_ns)
}

/// `BarCtx::now_ms` live: `ms_from_ns(sat_i64(anchor.wall_of(E)))`.
#[inline]
pub(crate) fn now_ms(core: &Core) -> i64 {
    pine_ir::scale::ms_from_ns(sat_i64(now_ns(core)))
}

/// §4.4.2: the `Quote` row of one tick (§4.12 copy #4).
#[inline]
pub(crate) fn quote_of(core: &Core, t: &Tick) -> Quote {
    // COPY: one `Quote` (48 B) per tick of a pine symbol (§4.12 #4) — the broker takes `&Quote`
    // and `Tick` is another layout — a borrow of the tick rejected: the layouts differ.
    Quote {
        bid_1e6: t.bid_px.raw(),
        ask_1e6: t.ask_px.raw(),
        bid_qty_1e6: t.bid_qty.raw(),
        ask_qty_1e6: t.ask_qty.raw(),
        ts_ns: core.anchor.wall_of(t.ts_ns),
        flags: u32::from(t.flags & TICK_FLAG_STALE),
        _pad: 0,
    }
}
