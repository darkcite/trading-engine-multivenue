// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Data switches the VM owns. Each row names the pin whose conformance
//! fixture decides it (impl-06 maps pin -> fixture ids); a row changes by a
//! CAR on contract §3.6.5 and an edit of this data.

use pine_ir::{abi, barflag};

/// How one `barstate.*` slot is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bs {
    /// Constant false.
    False,
    /// `bar_index == 0` — this evaluation's `bar_index` (`bars_seen - 1` in a bar,
    /// `bars_seen` in a tick).
    FirstBar,
    /// Any of these `Bar.flags` bits set.
    Any(u32),
    /// None of these `Bar.flags` bits set.
    None(u32),
    /// True unless one of these `Bar.flags` bits is set and a `step_tick` already ran
    /// on this bar before this evaluation began (`ticks_this_bar > 0`).
    NewUnless(u32),
}

/// One row: the slot, its `step_bar` rule, its `step_tick` rule.
pub(crate) struct BsRow {
    pub slot: u16,
    pub bar: Bs,
    pub tick: Bs,
}

/// `pin:U17-barstate-table` — contract §3.6.5 as amended by A2 (CAR-03-4), row for row.
pub(crate) const BARSTATE: [BsRow; 7] = [
    BsRow { slot: abi::I_BS_ISFIRST, bar: Bs::FirstBar, tick: Bs::FirstBar },
    BsRow { slot: abi::I_BS_ISCONFIRMED, bar: Bs::Any(barflag::CONFIRMED), tick: Bs::False },
    BsRow { slot: abi::I_BS_ISLAST, bar: Bs::Any(barflag::LAST | barflag::REALTIME), tick: Bs::Any(barflag::LAST | barflag::REALTIME) },
    BsRow { slot: abi::I_BS_ISNEW, bar: Bs::NewUnless(barflag::REALTIME), tick: Bs::NewUnless(barflag::REALTIME) },
    BsRow { slot: abi::I_BS_ISREALTIME, bar: Bs::Any(barflag::REALTIME), tick: Bs::Any(barflag::REALTIME) },
    BsRow { slot: abi::I_BS_ISHISTORY, bar: Bs::None(barflag::REALTIME), tick: Bs::None(barflag::REALTIME) },
    BsRow { slot: abi::I_BS_ISLASTCONFHIST, bar: Bs::Any(barflag::LAST_CONFIRMED_HISTORY), tick: Bs::False },
];

/// One slot's value. `ticked`: a `step_tick` already ran on this bar before this evaluation.
#[inline(always)]
pub(crate) fn eval(rule: Bs, flags: u32, bar_index: i64, ticked: bool) -> bool {
    match rule {
        Bs::False => false,
        Bs::FirstBar => bar_index == 0,
        Bs::Any(m) => flags & m != 0,
        Bs::None(m) => flags & m == 0,
        Bs::NewUnless(m) => !(flags & m != 0 && ticked),
    }
}
