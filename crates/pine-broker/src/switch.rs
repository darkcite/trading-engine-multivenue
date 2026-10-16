// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pin switch table (impl-05 §4.2): every Pine behaviour TradingView's documentation does not
//! pin down, with its default and the fixture that will decide it. `SWITCHES` holds the defaults
//! (I38); `#[cfg(test)]` `with_switches` constructors build a broker on the other reading. When a
//! fixture decides against a default, this constant changes in the same commit as the capture.

/// `path_tie` — open equidistant from high and low (`pin:05-path-tie`, §4.8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PathTie {
    /// O→L→H→C (default).
    LowFirst = 0,
    /// O→H→L→C.
    HighFirst = 1,
}

/// `pyramiding` — price-based entries generated on different ticks (`pin:05-pyramiding-check`,
/// §4.7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PyramidCheck {
    /// The open-entry-trade count at fill time decides (default).
    FillTime = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// The count at generation time decides.
    GenerationTime = 1,
}

/// `margin_funds` — an entry needing more margin than available funds
/// (`pin:05-margin-funds-check`, §4.9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum MarginFunds {
    /// The entry is rejected (default).
    Reject = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// The entry is reduced to the affordable quantity.
    Reduce = 1,
}

/// `forced_fill_slippage` — margin-call and risk-flatten fills (`pin:U09-margin-call-size`,
/// `pin:U10-risk-rules`, §4.9.2, §4.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ForcedFillSlippage {
    /// Slipped like market orders (default).
    Slipped = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Unslipped.
    Unslipped = 1,
}

/// `close_immediately` — `immediately = true` (`pin:U11-close-immediately`, §4.3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CloseImmediately {
    /// Same tick (documented; default).
    SameTick = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Next tick.
    NextTick = 1,
}

/// `exit_reservation` — exit quantities after a partial close (`pin:05-exit-reservation`,
/// §4.5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ExitReservation {
    /// Re-derive reservations in rule order (default).
    RuleOrder = 0,
    /// Every live exit set shrinks by each exit fill.
    ReduceByFill = 1,
}

/// `exit_qty` — explicit exit `qty` covering several trades (`pin:05-exit-qty-per-trade`,
/// §4.5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ExitQty {
    /// Per covered trade (default).
    PerTrade = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Per call.
    PerCall = 1,
}

/// `trail_arm_fire` — a trail armed inside a path segment (`pin:05-trail-same-segment`, §4.8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum TrailArmFire {
    /// Fires from the next segment (default).
    NextSegment = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Fires in the same segment.
    SameSegment = 1,
}

/// `cash_per_order` — `commission.cash_per_order` on a split fill (`pin:05-cash-per-order`,
/// §4.8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CashPerOrder {
    /// Per fill, split pro rata over the fill's slices (default).
    PerFill = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Per order (first fill only).
    PerOrder = 1,
}

/// `pct_equity_at` — equity for `percent_of_equity` sizing (`pin:05-pct-equity-time`, §4.7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PctEquityAt {
    /// Equity at fill time (default).
    FillTime = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Equity at order time.
    OrderTime = 1,
}

/// `price_rounding` — script prices in TV mode (`pin:05-price-rounding`, §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PriceRounding {
    /// Rounded to `chart.mintick_1e6`, half away from zero (default).
    NearestTick = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Used as given.
    None = 1,
}

/// `cof_close_tick` — a `calc_on_order_fills` execution after a fill at the bar's close
/// (`pin:05-cof-close-tick`, §4.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CofCloseTick {
    /// No execution (default).
    No = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// An execution.
    Yes = 1,
}

/// `cof_next_tick` — orders born at an intrabar execution (`pin:05-cof-next-tick`, §4.8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CofNextTick {
    /// Eligible from the next path point (default).
    NextPoint = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Eligible for the rest of the current segment.
    RestOfSegment = 1,
}

/// `fill_limits_price` — a verified limit whose segment starts beyond its verification level
/// (`pin:05-fill-limits`, §4.8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum FillLimitsPrice {
    /// At its limit (default).
    AtLimit = 0,
    /// At the segment's start, as with `fill_limits_ticks = 0`.
    AtStart = 1,
}

/// One reading of every §4.2 switch. 14 bytes, held by value in each broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Switches {
    /// §4.8.2.
    pub(crate) path_tie: PathTie,
    /// §4.7.4.
    pub(crate) pyramiding: PyramidCheck,
    /// §4.9.1.
    pub(crate) margin_funds: MarginFunds,
    /// §4.9.2, §4.10.
    pub(crate) forced_fill_slippage: ForcedFillSlippage,
    /// §4.3.4.
    pub(crate) close_immediately: CloseImmediately,
    /// §4.5.4.
    pub(crate) exit_reservation: ExitReservation,
    /// §4.5.3.
    pub(crate) exit_qty: ExitQty,
    /// §4.8.5.
    pub(crate) trail_arm_fire: TrailArmFire,
    /// §4.8.6.
    pub(crate) cash_per_order: CashPerOrder,
    /// §4.7.1.
    pub(crate) pct_equity_at: PctEquityAt,
    /// §4.1.
    pub(crate) price_rounding: PriceRounding,
    /// §4.3.2.
    pub(crate) cof_close_tick: CofCloseTick,
    /// §4.8.3.
    pub(crate) cof_next_tick: CofNextTick,
    /// §4.8.6.
    pub(crate) fill_limits_price: FillLimitsPrice,
}

const _: () = assert!(core::mem::size_of::<Switches>() == 14);
const _: () = assert!(core::mem::align_of::<Switches>() == 1);

/// The defaults (I38): the §4.2 table's "default" column.
pub(crate) const SWITCHES: Switches = Switches {
    path_tie: PathTie::LowFirst,
    pyramiding: PyramidCheck::FillTime,
    margin_funds: MarginFunds::Reject,
    forced_fill_slippage: ForcedFillSlippage::Slipped,
    close_immediately: CloseImmediately::SameTick,
    exit_reservation: ExitReservation::RuleOrder,
    exit_qty: ExitQty::PerTrade,
    trail_arm_fire: TrailArmFire::NextSegment,
    cash_per_order: CashPerOrder::PerFill,
    pct_equity_at: PctEquityAt::FillTime,
    price_rounding: PriceRounding::NearestTick,
    cof_close_tick: CofCloseTick::No,
    cof_next_tick: CofNextTick::NextPoint,
    fill_limits_price: FillLimitsPrice::AtLimit,
};

#[cfg(test)]
mod tests {
    use super::{Switches, SWITCHES};

    #[test]
    fn switches_hold_the_defaults() {
        // Every default is discriminant 0, so the table's default column is the zero reading.
        let s: Switches = SWITCHES;
        let d = [
            s.path_tie as u8,
            s.pyramiding as u8,
            s.margin_funds as u8,
            s.forced_fill_slippage as u8,
            s.close_immediately as u8,
            s.exit_reservation as u8,
            s.exit_qty as u8,
            s.trail_arm_fire as u8,
            s.cash_per_order as u8,
            s.pct_equity_at as u8,
            s.price_rounding as u8,
            s.cof_close_tick as u8,
            s.cof_next_tick as u8,
            s.fill_limits_price as u8,
        ];
        let mut i = 0;
        while i < d.len() {
            assert_eq!(d[i], 0, "switch {i} is not on its default");
            i += 1;
        }
    }
}
