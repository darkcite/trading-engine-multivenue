// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Broker faults (impl-05 §3.2, 00-INDEX §3.12) and the broker's slice of the frozen diagnostic
//! codes (00-INDEX §3.3, 5100–5199).

/// The script asked for more than its declared bounds allow. D13: a backtest host aborts the run;
/// `strategy-pine` disables the script and flattens it (00-INDEX §3.13). Never fatal to a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ScriptRefusal {
    /// `cap::ORDERS`, `cap::EXIT_RULES` or `cap::LEG_SETS` is full.
    OrderTableFull = 1,
    /// `cap::LOTS` open trades.
    LotTableFull = 2,
    /// `cap::OCA_GROUPS` live groups.
    OcaGroupsFull = 3,
    /// `EngineIntentBuf::CAP` reached — unreachable by construction (§4.12.6); a defence.
    TooManyIntents = 4,
    /// A script-supplied quantity cannot be honoured: explicit `qty ≤ 0` or `qty_percent ∉ (0, 100]`
    /// (`pin:05-bad-qty`). A size that merely floors to zero is a counted drop (`sized_to_zero`).
    Unsized = 5,
    /// The N-rule ratio of an explicit entry `qty` to the default order left `[1/16, 16]` (§4.7.2).
    SizingRatioOutOfRange = 6,
}

/// Our defect. Fails fast in every mode (00-INDEX §3.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum InternalDefect {
    /// The book disagrees with itself, or a fill contradicts the order it names (side, or more
    /// than the order's quantity).
    BookDesync = 1,
    /// A fill names client-oid 0 or one this broker never issued. The book is not touched.
    UnknownFill = 2,
    /// The TV walk was driven out of order (`submit` with no `Evaluate` outstanding).
    PathCursor = 3,
    /// A fill tried to consume more than the open trades hold.
    LotUnderflow = 4,
    /// An intent's structure violates `intent_flag` / padding / field-bit rules (§4.4.1) — a
    /// `pine-std` defect (00-INDEX §3.12, A2).
    MalformedIntent = 5,
}

/// Every error a broker method returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrokerFault {
    /// A script fault — D13 isolates it in live.
    Script(ScriptRefusal),
    /// Ours — fail fast everywhere.
    Internal(InternalDefect),
}

/// Names for the broker's slice of 00-INDEX §3.3 (5100–5199). Consumers may match either the
/// name or the number; the numbers are frozen.
pub mod code {
    use pine_ir::DiagCode;
    /// `use_bar_magnifier = true`.
    pub const BAR_MAGNIFIER: DiagCode = DiagCode(5100);
    /// `ProgramMeta::nonstandard_chart`.
    pub const NONSTANDARD_CHART: DiagCode = DiagCode(5101);
    /// `pyramiding` above `cap::PYRAMIDING_MAX`.
    pub const PYRAMIDING_TOO_LARGE: DiagCode = DiagCode(5102);
    /// Instrument facts unusable: `ChartInfo` or `NativeCfg` tick / step / point value / minimum.
    pub const BAD_CHART_INFO: DiagCode = DiagCode(5103);
    /// `initial_capital_1e6 ≤ 0`.
    pub const BAD_CAPITAL: DiagCode = DiagCode(5104);
    /// A `strategy.risk.*` rule, a declaration value or a native risk limit outside its domain
    /// (00-INDEX §3.3: "bad risk rule or declaration value").
    pub const BAD_RISK_RULE: DiagCode = DiagCode(5105);
    /// A unit notional `≤ 0`.
    pub const ZERO_UNIT_NOTIONAL: DiagCode = DiagCode(5106);
    /// `calc_on_order_fills` on a program sema did not mark `uses_intrabar`.
    pub const INTRABAR_ON_BAR_CLOSE: DiagCode = DiagCode(5107);
    /// `percent_of_equity` default sizing on a Live route (D22).
    pub const PCT_EQUITY_LIVE: DiagCode = DiagCode(5108);
    /// `calc_on_every_history_tick = true` (`decl_flag::CALC_ON_EVERY_HISTORY_TICK`, A2).
    pub const HISTORY_TICK: DiagCode = DiagCode(5109);
}

#[cfg(test)]
mod tests {
    use super::{code, BrokerFault, InternalDefect, ScriptRefusal};

    #[test]
    fn codes_are_the_frozen_5100_range() {
        let all = [
            code::BAR_MAGNIFIER,
            code::NONSTANDARD_CHART,
            code::PYRAMIDING_TOO_LARGE,
            code::BAD_CHART_INFO,
            code::BAD_CAPITAL,
            code::BAD_RISK_RULE,
            code::ZERO_UNIT_NOTIONAL,
            code::INTRABAR_ON_BAR_CLOSE,
            code::PCT_EQUITY_LIVE,
            code::HISTORY_TICK,
        ];
        let mut i = 0;
        while i < all.len() {
            assert_eq!(u32::from(all[i].0), 5100 + u32::try_from(i).unwrap());
            i += 1;
        }
    }

    #[test]
    fn fault_discriminants_are_frozen() {
        assert_eq!(ScriptRefusal::OrderTableFull as u8, 1);
        assert_eq!(ScriptRefusal::SizingRatioOutOfRange as u8, 6);
        assert_eq!(InternalDefect::BookDesync as u8, 1);
        assert_eq!(InternalDefect::MalformedIntent as u8, 5);
        assert_ne!(
            BrokerFault::Script(ScriptRefusal::Unsized),
            BrokerFault::Internal(InternalDefect::MalformedIntent)
        );
    }
}
