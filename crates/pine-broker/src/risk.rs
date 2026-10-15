// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `strategy.risk.*` and the day boundary — event time only, no clock (impl-05 §4.10, §4.11).
//! The broker calls `roll_day` at each bar's day, `check` at every tick and after every fill tick,
//! `note_fill` per filled order; a trip returns `true` and the broker flattens (TV: a market fill at
//! the detecting tick, flagged `RISK_FLATTEN`). `allow_entry_in` / `max_position_size` are §4.7.5's.

use crate::money::pct_1e12;
use pine_ir::{decl_flag, BrokerCounters, StrategyDecl};

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
    day_open_net_1e12: i128,
    fills_today: u32,
    loss_streak: u32,
    halt: u8,
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
            day_open_net_1e12: 0,
            fills_today: 0,
            loss_streak: 0,
            halt: HALT_NONE,
        }
    }

    /// While halted, only `Cancel` / `CancelAll` intents commit.
    pub(crate) fn halted(&self) -> bool {
        self.halt != HALT_NONE
    }

    /// Enter `day` (§4.11): at a roll the closed day's net change updates the losing-day streak,
    /// a day-scoped halt lifts, and `max_cons_loss_days` is checked. `true` on a trip.
    pub(crate) fn roll_day(
        &mut self,
        day: i64,
        equity_1e12: i128,
        net_1e12: i128,
        counters: &mut BrokerCounters,
    ) -> bool {
        if self.day == Some(day) {
            return false;
        }
        let had_day = self.day.is_some();
        if had_day {
            let change = net_1e12.saturating_sub(self.day_open_net_1e12);
            self.loss_streak = if change < 0 { self.loss_streak + 1 } else { 0 };
            if self.halt == HALT_DAY {
                self.halt = HALT_NONE;
            }
        }
        self.day = Some(day);
        self.day_open_equity_1e12 = equity_1e12;
        self.day_open_net_1e12 = net_1e12;
        self.fills_today = 0;
        if had_day
            && self.halt != HALT_FOREVER
            && self.max_cons_loss_days > 0
            && self.loss_streak >= self.max_cons_loss_days
        {
            counters.risk_trips_cons_loss_days += 1;
            self.halt = HALT_FOREVER;
            return true;
        }
        false
    }

    /// One filled order today.
    pub(crate) fn note_fill(&mut self) {
        self.fills_today = self.fills_today.saturating_add(1);
    }

    /// Check the equity and order-count rules at a tick (§4.10). `true` on a trip.
    pub(crate) fn check(
        &mut self,
        equity_1e12: i128,
        peak_1e12: i128,
        counters: &mut BrokerCounters,
    ) -> bool {
        if self.halt == HALT_FOREVER {
            return false;
        }
        if self.max_drawdown_1e6 > 0 {
            let bound = if self.dd_pct {
                pct_1e12(peak_1e12, self.max_drawdown_1e6)
            } else {
                pine_ir::scale::usd_1e6_to_1e12(self.max_drawdown_1e6)
            };
            if peak_1e12.saturating_sub(equity_1e12) >= bound {
                counters.risk_trips_drawdown += 1;
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
                self.halt = HALT_DAY;
                return true;
            }
        }
        if self.max_intraday_filled_orders > 0 && self.fills_today > self.max_intraday_filled_orders
        {
            counters.risk_trips_intraday_orders += 1;
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
            ..StrategyDecl::default()
        }
    }

    #[test]
    fn drawdown_trips_for_good_at_the_bound() {
        let mut c = BrokerCounters::default();
        let mut r = Risk::new(&decl(
            RiskRules {
                max_drawdown_1e6: 100 * 1_000_000,
                ..RiskRules::default()
            },
            0,
        ));
        assert!(!r.check(1_000 * E12 - 99 * E12, 1_000 * E12, &mut c));
        assert!(r.check(1_000 * E12 - 100 * E12, 1_000 * E12, &mut c));
        assert!(r.halted());
        // For good: a later day does not lift it.
        r.roll_day(5, 0, 0, &mut c);
        assert!(r.halted());
        assert_eq!(c.risk_trips_drawdown, 1);
        // 10 % of peak.
        let mut p = Risk::new(&decl(
            RiskRules {
                max_drawdown_1e6: 10_000_000,
                ..RiskRules::default()
            },
            decl_flag::DRAWDOWN_PCT,
        ));
        assert!(p.check(900 * E12, 1_000 * E12, &mut c));
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
        r.roll_day(1, 1_000 * E12, 0, &mut c);
        assert!(r.check(950 * E12, 1_000 * E12, &mut c));
        assert!(r.halted());
        r.roll_day(2, 950 * E12, -50 * E12, &mut c);
        assert!(!r.halted());
        r.note_fill();
        r.note_fill();
        assert!(!r.check(950 * E12, 1_000 * E12, &mut c));
        r.note_fill();
        assert!(
            r.check(950 * E12, 1_000 * E12, &mut c),
            "more than 2 filled orders"
        );
        assert_eq!(
            (c.risk_trips_intraday_loss, c.risk_trips_intraday_orders),
            (1, 1)
        );
    }

    #[test]
    fn consecutive_losing_days_trip_at_the_roll() {
        let mut c = BrokerCounters::default();
        let mut r = Risk::new(&decl(
            RiskRules {
                max_cons_loss_days: 2,
                ..RiskRules::default()
            },
            0,
        ));
        assert!(!r.roll_day(1, 0, 0, &mut c));
        assert!(!r.roll_day(2, 0, -E12, &mut c));
        assert!(r.roll_day(3, 0, -2 * E12, &mut c));
        assert_eq!(c.risk_trips_cons_loss_days, 1);
        assert!(r.halted());
    }
}
