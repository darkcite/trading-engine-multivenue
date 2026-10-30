// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `strategy.risk.*` and the day boundary — event time only, no clock (impl-05 §4.10, §4.11).
//! The broker calls `roll_day` at each bar's day, `check` at every tick and after every fill tick,
//! `note_fill` per filled order; a trip returns `true` and the broker flattens (TV: a market fill at
//! the detecting tick, flagged `RISK_FLATTEN`). `allow_entry_in` / `max_position_size` are §4.7.5's.

use crate::money::pct_1e12;
use crate::signal;
use pine_ir::{decl_flag, BrokerCounters, StrKey, StrategyDecl};

/// No halt.
const HALT_NONE: u8 = 0;
/// Halted until the day rolls.
const HALT_DAY: u8 = 1;
/// Halted for good.
const HALT_FOREVER: u8 = 2;

/// The risk state of one broker.
pub(crate) struct Risk {
    max_drawdown_1e6: i64,
    max_intraday_loss_1e6: i64,
    max_intraday_filled_orders: u32,
    max_cons_loss_days: u32,
    dd_pct: bool,
    il_pct: bool,
    day: Option<i64>,
    day_open_equity_1e12: i128,
    /// Initial capital ×1e12.
    initial_1e12: i128,
    /// The closed-equity peak (`initial + netprofit`), the `max_drawdown` reference.
    closed_peak_1e12: i128,
    fills_today: u32,
    loss_streak: u32,
    halt: u8,
    /// The last trip's exit-order id (`pin:05-exit-signal-text`, CI-032).
    trip: StrKey,
}

impl Risk {
    /// The rules of `d` (a zero field declares no rule).
    pub(crate) fn new(d: &StrategyDecl) -> Self {
        Self {
            max_drawdown_1e6: d.risk.max_drawdown_1e6,
            max_intraday_loss_1e6: d.risk.max_intraday_loss_1e6,
            max_intraday_filled_orders: d.risk.max_intraday_filled_orders,
            max_cons_loss_days: d.risk.max_cons_loss_days,
            dd_pct: d.flags & decl_flag::DRAWDOWN_PCT != 0,
            il_pct: d.flags & decl_flag::INTRADAY_LOSS_PCT != 0,
            day: None,
            day_open_equity_1e12: 0,
            initial_1e12: pine_ir::scale::usd_1e6_to_1e12(d.initial_capital_1e6),
            closed_peak_1e12: pine_ir::scale::usd_1e6_to_1e12(d.initial_capital_1e6),
            fills_today: 0,
            loss_streak: 0,
            halt: HALT_NONE,
            trip: StrKey::NONE,
        }
    }

    /// The exit-order id TradingView gives the last trip's flatten (`crate::signal`).
    pub(crate) fn trip_signal(&self) -> StrKey {
        self.trip
    }

    /// While halted, only `Cancel` / `CancelAll` intents commit.
    pub(crate) fn halted(&self) -> bool {
        self.halt != HALT_NONE
    }

    /// Enter `day` (§4.11): at a roll the closed day's equity change updates the losing-day
    /// streak, a day-scoped halt lifts, and `max_cons_loss_days` is checked. `true` on a trip.
    ///
    /// `pin:U10-risk-rules` (r33d, shared with native): a losing day is one whose **equity** —
    /// open profit included — fell from the day's opening mark to its last, not its `netprofit`.
    /// TV E094 (`max_cons_loss_days(2)`): day 20561 holds trade 1 open and falling (equity
    /// −1595.8, no close), day 20562 closes it and falls on trade 2 (−1819); the trip flattens
    /// trade 2 at day 20563's open (73758.4) — on `netprofit` day 20561 is flat and nothing trips.
    pub(crate) fn roll_day(
        &mut self,
        day: i64,
        equity_1e12: i128,
        counters: &mut BrokerCounters,
    ) -> bool {
        if self.day == Some(day) {
            return false;
        }
        let had_day = self.day.is_some();
        if had_day {
            let change = equity_1e12.saturating_sub(self.day_open_equity_1e12);
            self.loss_streak = if change < 0 { self.loss_streak + 1 } else { 0 };
            if self.halt == HALT_DAY {
                self.halt = HALT_NONE;
            }
        }
        self.day = Some(day);
        self.day_open_equity_1e12 = equity_1e12;
        self.fills_today = 0;
        if had_day
            && self.halt != HALT_FOREVER
            && self.max_cons_loss_days > 0
            && self.loss_streak >= self.max_cons_loss_days
        {
            counters.risk_trips_cons_loss_days += 1;
            self.trip = signal::RISK_CONS_LOSS_DAYS;
            self.halt = HALT_FOREVER;
            return true;
        }
        false
    }

    /// One filled order today.
    pub(crate) fn note_fill(&mut self) {
        self.fills_today = self.fills_today.saturating_add(1);
    }

    /// Check the equity and order-count rules at a tick (§4.10): `equity` the marked equity,
    /// `net` the closed-trade net profit. `true` on a trip.
    ///
    /// `pin:U10-risk-rules` (r33d, shared with native): `max_drawdown` measures the **closed**
    /// equity (`initial + netprofit`) from its peak — TV E091 (`max_drawdown(2000, cash)`): trades
    /// 2, 4 and 8 run open drawdowns past 2 000 from the equity peak without a trip; trade 9's
    /// close takes the closed equity 3 169.2 below its peak and trading stops there (9 trades).
    pub(crate) fn check(
        &mut self,
        equity_1e12: i128,
        net_1e12: i128,
        counters: &mut BrokerCounters,
    ) -> bool {
        if self.halt == HALT_FOREVER {
            return false;
        }
        let closed_eq = self.initial_1e12.saturating_add(net_1e12);
        self.closed_peak_1e12 = self.closed_peak_1e12.max(closed_eq);
        if self.max_drawdown_1e6 > 0 {
            let peak = self.closed_peak_1e12;
            let bound = if self.dd_pct {
                pct_1e12(peak, self.max_drawdown_1e6)
            } else {
                pine_ir::scale::usd_1e6_to_1e12(self.max_drawdown_1e6)
            };
            if peak.saturating_sub(closed_eq) >= bound {
                counters.risk_trips_drawdown += 1;
                self.trip = signal::RISK_MAX_DRAWDOWN;
                self.halt = HALT_FOREVER;
                return true;
            }
        }
        if self.halt == HALT_DAY {
            return false;
        }
        if self.max_intraday_loss_1e6 > 0 && self.day.is_some() {
            let bound = if self.il_pct {
                pct_1e12(self.day_open_equity_1e12, self.max_intraday_loss_1e6)
            } else {
                pine_ir::scale::usd_1e6_to_1e12(self.max_intraday_loss_1e6)
            };
            if self.day_open_equity_1e12.saturating_sub(equity_1e12) >= bound {
                counters.risk_trips_intraday_loss += 1;
                self.trip = signal::RISK_INTRADAY_LOSS;
                self.halt = HALT_DAY;
                return true;
            }
        }
        // `pin:U10-risk-rules` (r33d, shared with native): the fill that **reaches** the bound
        // trips — TV E093 (`max_intraday_filled_orders(2)`): trade 1's exit at 00:00 and trade 2's
        // entry at 02:00 the same day are 2 fills; trade 2 is flattened at its entry tick (75577.4,
        // `Close Position (Max number of filled orders in one day)`), not on a third fill.
        if self.max_intraday_filled_orders > 0
            && self.fills_today >= self.max_intraday_filled_orders
        {
            counters.risk_trips_intraday_orders += 1;
            self.trip = signal::RISK_INTRADAY_ORDERS;
            self.halt = HALT_DAY;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::Risk;
    use pine_ir::{decl_flag, BrokerCounters, RiskRules, StrategyDecl};

    const E12: i128 = 1_000_000_000_000;

    fn decl(r: RiskRules, flags: u16) -> StrategyDecl {
        StrategyDecl {
            risk: r,
            flags,
            initial_capital_1e6: 1_000 * 1_000_000,
            ..StrategyDecl::default()
        }
    }

    #[test]
    fn drawdown_trips_for_good_at_the_bound() {
        // `pin:U10-risk-rules` (r33d, E091): the closed equity `1 000 + net` from its peak.
        let mut c = BrokerCounters::default();
        let mut r = Risk::new(&decl(
            RiskRules {
                max_drawdown_1e6: 100 * 1_000_000,
                ..RiskRules::default()
            },
            0,
        ));
        // An open drawdown far past the bound does not trip while nothing closed.
        assert!(!r.check(500 * E12, 0, &mut c));
        assert!(!r.check(1_000 * E12 - 99 * E12, -99 * E12, &mut c));
        assert!(r.check(1_000 * E12 - 100 * E12, -100 * E12, &mut c));
        assert!(r.halted());
        // For good: a later day does not lift it.
        r.roll_day(5, 0, &mut c);
        assert!(r.halted());
        assert_eq!(c.risk_trips_drawdown, 1);
        // 10 % of the closed-equity peak (1 050 after a +50 close): 105.
        let mut p = Risk::new(&decl(
            RiskRules {
                max_drawdown_1e6: 10_000_000,
                ..RiskRules::default()
            },
            decl_flag::DRAWDOWN_PCT,
        ));
        assert!(!p.check(1_050 * E12, 50 * E12, &mut c));
        assert!(!p.check(946 * E12, -54 * E12, &mut c));
        assert!(p.check(945 * E12, -55 * E12, &mut c));
    }

    #[test]
    fn intraday_rules_halt_until_the_day_rolls() {
        let mut c = BrokerCounters::default();
        let rules = RiskRules {
            max_intraday_loss_1e6: 50_000_000,
            max_intraday_filled_orders: 2,
            ..RiskRules::default()
        };
        let mut r = Risk::new(&decl(rules, 0));
        r.roll_day(1, 1_000 * E12, &mut c);
        assert!(r.check(950 * E12, 0, &mut c));
        assert!(r.halted());
        r.roll_day(2, 950 * E12, &mut c);
        assert!(!r.halted());
        r.note_fill();
        assert!(!r.check(950 * E12, 0, &mut c));
        r.note_fill();
        // `pin:U10-risk-rules` (r33d, E093): the fill that reaches the bound trips.
        assert!(r.check(950 * E12, 0, &mut c), "2 filled orders reach 2");
        assert_eq!(
            (c.risk_trips_intraday_loss, c.risk_trips_intraday_orders),
            (1, 1)
        );
    }

    #[test]
    fn consecutive_losing_days_trip_at_the_roll() {
        // `pin:U10-risk-rules` (r33d, E094): a losing day is an equity fall, open profit included.
        let mut c = BrokerCounters::default();
        let mut r = Risk::new(&decl(
            RiskRules {
                max_cons_loss_days: 2,
                ..RiskRules::default()
            },
            0,
        ));
        assert!(!r.roll_day(1, 1_000 * E12, &mut c));
        assert!(!r.roll_day(2, 999 * E12, &mut c));
        assert!(r.roll_day(3, 997 * E12, &mut c));
        assert_eq!(c.risk_trips_cons_loss_days, 1);
        assert!(r.halted());
        // A rising day resets the streak.
        let mut q = Risk::new(&decl(
            RiskRules {
                max_cons_loss_days: 2,
                ..RiskRules::default()
            },
            0,
        ));
        assert!(!q.roll_day(1, 1_000 * E12, &mut c));
        assert!(!q.roll_day(2, 999 * E12, &mut c));
        assert!(!q.roll_day(3, 1_001 * E12, &mut c));
        assert!(!q.roll_day(4, 1_000 * E12, &mut c));
        assert!(!q.halted());
    }
}
