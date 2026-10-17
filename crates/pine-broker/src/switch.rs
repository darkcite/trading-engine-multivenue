// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The pin switch table (impl-05 §4.2): every Pine behaviour TradingView's documentation does not
//! pin down, with its default and the fixture that will decide it. `SWITCHES` holds the defaults
//! (I38); `#[cfg(test)]` `with_switches` constructors build a broker on the other reading. When a
//! fixture decides against a default, this constant changes in the same commit as the capture.

use crate::money::{ceil_tick, floor_tick, round_tick};

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
    /// Per order kind, to the grid tick the market reaches no earlier than the script's level
    /// (default): a buy limit and a sell stop round down (toward −∞), a sell limit and a buy stop
    /// round up (toward +∞), on `chart.mintick_1e6`. `pin:05-price-rounding` decided (r31) by the
    /// captured List of Trades read on TV's bars against the r29 readings, a reading counting
    /// only if it matches every trade of its fixture:
    /// * buy limit — E113 (`close − 12.4` ticks): `floor_tick` 45/45, `nearest_tick` 0/45,
    ///   `ceil_tick` 0/45;
    /// * sell limit — E129 (`close + 12.4 / 12.6` ticks by `syni` parity): `ceil_tick` 45/45,
    ///   `nearest_tick` 19/45, `floor_tick` 0/45;
    /// * buy stop — E131 (`close + 12.4 / 12.6`): `ceil_tick` 45/45, `nearest_tick` 19/45,
    ///   `floor_tick` 0/45;
    /// * sell stop, an exit leg — E130 (`strategy.exit(stop = avg − 20.4 / 20.6` ticks)):
    ///   `floor_tick` 92/92 stop exits (42 of them separate the readings), `nearest_tick` 69/92,
    ///   `ceil_tick` 50/92 — the exit leg's absolute price is rounded like an order's.
    ///
    /// An exit leg takes its order's rule (a long's take-profit is a sell limit, its stop-loss a
    /// sell stop; mirrored for a short) and a stop-limit rounds each price by its own rule: the
    /// exit-leg limit, the buy-stop leg and the stop-limit are uncaptured (literal reading of the
    /// four captured kinds). Tick-distance legs (`profit`, `loss`, `trail_points`), the trail
    /// activation `trail_price` and `trail_offset` are not script prices and stay as given.
    NeverEarlier = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Rounded to `chart.mintick_1e6`, half away from zero (the pre-capture default; refuted by
    /// E113, 0/45).
    NearestTick = 1,
    /// Used as given (also the native broker's grid, [`PriceGrid::AS_GIVEN`]).
    None = 2,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Rounded down (toward −∞) for every kind — R-P12's provisional default after E113 (CI-034);
    /// refuted for sell limits by E129 (0/45) and for buy stops by E131 (0/45).
    FloorTick = 3,
}

/// A script price's grid (`pin:05-price-rounding`, §4.1): the tick (0 = prices as given) and the
/// reading. Held by value in each commit and exit-sync context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct PriceGrid {
    /// `chart.mintick_1e6`; 0 = prices as given.
    pub(crate) tick_1e6: i64,
    /// The `price_rounding` reading.
    pub(crate) rounding: PriceRounding,
}

const _: () = assert!(core::mem::size_of::<PriceGrid>() == 16);
const _: () = assert!(core::mem::align_of::<PriceGrid>() == 8);

impl PriceGrid {
    /// Prices as given: the native broker (the venue gate rounds), TV under `price_rounding =
    /// None`.
    pub(crate) const AS_GIVEN: Self = Self {
        tick_1e6: 0,
        rounding: PriceRounding::None,
    };

    /// TV mode: `mintick_1e6` under `rounding` (`None` → [`Self::AS_GIVEN`]).
    pub(crate) const fn tv(mintick_1e6: i64, rounding: PriceRounding) -> Self {
        match rounding {
            PriceRounding::None => Self::AS_GIVEN,
            _ => Self {
                tick_1e6: mintick_1e6,
                rounding,
            },
        }
    }

    /// A limit price on the grid; `buy`: a buy limit (an entry long, `strategy.order` buy, a
    /// short's take-profit).
    pub(crate) const fn limit(self, px_1e6: i64, buy: bool) -> i64 {
        self.on_grid(px_1e6, buy)
    }

    /// A stop price on the grid; `buy`: a buy stop (an entry long, `strategy.order` buy, a short's
    /// stop-loss).
    pub(crate) const fn stop(self, px_1e6: i64, buy: bool) -> i64 {
        self.on_grid(px_1e6, !buy)
    }

    /// `down`: `NeverEarlier`'s direction is toward −∞ (a buy limit, a sell stop). Identity when
    /// `tick_1e6 ≤ 0`.
    const fn on_grid(self, px_1e6: i64, down: bool) -> i64 {
        match self.rounding {
            PriceRounding::NeverEarlier => {
                if down {
                    floor_tick(px_1e6, self.tick_1e6)
                } else {
                    ceil_tick(px_1e6, self.tick_1e6)
                }
            }
            PriceRounding::NearestTick => round_tick(px_1e6, self.tick_1e6),
            PriceRounding::None => px_1e6,
            PriceRounding::FloorTick => floor_tick(px_1e6, self.tick_1e6),
        }
    }
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
    /// At the segment's start, as with `fill_limits_ticks = 0` (default). `pin:05-fill-limits`
    /// decided by E120 (CI-034 / CI-036): a buy limit 50 ticks through the market with
    /// `backtest_fill_limits_assumption = 3` filled at the next bar's open on all 45 entries, not
    /// at its limit — TradingView's docs ("still executes verified limit orders at their specified
    /// prices") describe a limit reached inside the bar; noted for the lead.
    AtStart = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// At its limit (the pre-capture default; refuted by E120).
    AtLimit = 1,
}

/// `history_tick` — `calc_on_every_history_tick = true` in TV mode (R-P6; impl-05 §3.7, §4.8.9).
/// `Refuse` until CAR-D1 is applied and the history-tick fixtures are captured; the native broker
/// refuses the flag (`5109`) on every reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum HistoryTick {
    /// `5109` at construction (default).
    Refuse = 0,
    #[cfg_attr(not(test), allow(dead_code))] // constructed by `with_switches` tests only.
    /// Evaluate at the history path points (`pin:05-hist-tick-*`, provisional).
    PathPoints = 1,
}

/// One reading of every §4.2 switch. 15 bytes, held by value in each broker.
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
    /// §3.7, §4.8.9.
    pub(crate) history_tick: HistoryTick,
}

const _: () = assert!(core::mem::size_of::<Switches>() == 15);
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
    price_rounding: PriceRounding::NeverEarlier,
    cof_close_tick: CofCloseTick::No,
    cof_next_tick: CofNextTick::NextPoint,
    fill_limits_price: FillLimitsPrice::AtStart,
    history_tick: HistoryTick::Refuse,
};

#[cfg(test)]
mod tests {
    use super::{PriceGrid, PriceRounding, Switches, SWITCHES};

    #[test]
    fn price_grid_rounds_each_kind_never_earlier() {
        // `pin:05-price-rounding` (r31): buy limit / sell stop down, sell limit / buy stop up.
        let g = PriceGrid::tv(100_000, PriceRounding::NeverEarlier);
        let px = 77_239_640_000; // 77239.64 on a 0.1 tick
        assert_eq!(g.limit(px, true), 77_239_600_000);
        assert_eq!(g.limit(px, false), 77_239_700_000);
        assert_eq!(g.stop(px, true), 77_239_700_000);
        assert_eq!(g.stop(px, false), 77_239_600_000);
        // On the grid: unchanged by every kind.
        assert_eq!(g.limit(77_239_600_000, false), 77_239_600_000);
        assert_eq!(g.stop(77_239_600_000, true), 77_239_600_000);
        // `None` is the native grid; the other readings ignore the kind.
        assert_eq!(
            PriceGrid::tv(100_000, PriceRounding::None),
            PriceGrid::AS_GIVEN
        );
        assert_eq!(PriceGrid::AS_GIVEN.stop(px, true), px);
        let f = PriceGrid::tv(100_000, PriceRounding::FloorTick);
        assert_eq!(
            (f.limit(px, false), f.stop(px, true)),
            (77_239_600_000, 77_239_600_000)
        );
        let n = PriceGrid::tv(100_000, PriceRounding::NearestTick);
        assert_eq!(
            (n.limit(px, true), n.stop(px, false)),
            (77_239_600_000, 77_239_600_000)
        );
    }

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
            s.history_tick as u8,
        ];
        let mut i = 0;
        while i < d.len() {
            assert_eq!(d[i], 0, "switch {i} is not on its default");
            i += 1;
        }
    }
}
