// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Construction checks (impl-05 §3.7, 00-INDEX §3.3 5100–5199) and `decl_hash64` (§4.16).
//!
//! Checked in the §3.7 order; the first failure is returned. Load time only: nothing here runs on
//! a step path.

use crate::cap;
use crate::fault::code;
use pine_ir::{
    decl_flag, fnv1a64_continue, ChartInfo, DiagCode, LimitPrimitive, NativeCfg, ProgramMeta,
    QtyType, SizingMode, StrategyDecl, TvCfg, FNV1A64_OFFSET,
};

/// The nine `decl_flag` bits; any other bit is an unknown fill-changing parameter (5105, CAR-05-1).
const KNOWN_FLAGS: u16 = decl_flag::ORDERS_ON_CLOSE
    | decl_flag::CALC_ON_ORDER_FILLS
    | decl_flag::CALC_ON_EVERY_TICK
    | decl_flag::FILL_ON_STANDARD_OHLC
    | decl_flag::CLOSE_ANY
    | decl_flag::BAR_MAGNIFIER
    | decl_flag::DRAWDOWN_PCT
    | decl_flag::INTRADAY_LOSS_PCT
    | decl_flag::CALC_ON_EVERY_HISTORY_TICK;

/// 100 % at the ×1e6 percent scale.
const PCT_100: i64 = 100 * 1_000_000;

/// Seconds in a day: `TvCfg::day_offset_s` must lie strictly inside `±DAY_S`.
const DAY_S: i32 = 86_400;

/// 5100, 5101, 5109, 5107, 5102 — the declaration and program checks both brokers share, in order.
/// `history_ticks`: the TV broker evaluates at the history path points (`HistoryTick::PathPoints`,
/// R-P6), so `calc_on_every_history_tick` is not refused (`5109` lifted) and joins
/// `calc_on_order_fills` in the `5107` pairing check (lead answer 13: a mis-paired program refuses
/// `5107`, no new code). The native broker always passes `false` (R-P6: it keeps `5109`).
fn check_program(meta: &ProgramMeta, history_ticks: bool) -> Result<(), DiagCode> {
    let flags = meta.decl.flags;
    if flags & decl_flag::BAR_MAGNIFIER != 0 {
        return Err(code::BAR_MAGNIFIER);
    }
    if meta.nonstandard_chart {
        return Err(code::NONSTANDARD_CHART);
    }
    let hist = flags & decl_flag::CALC_ON_EVERY_HISTORY_TICK != 0;
    if hist && !history_ticks {
        return Err(code::HISTORY_TICK);
    }
    if (flags & decl_flag::CALC_ON_ORDER_FILLS != 0 || hist) && !meta.uses_intrabar {
        return Err(code::INTRABAR_ON_BAR_CLOSE);
    }
    if meta.decl.pyramiding.max(1) > cap::PYRAMIDING_MAX {
        return Err(code::PYRAMIDING_TOO_LARGE);
    }
    Ok(())
}

/// 5103 — the chart's instrument facts (both brokers).
fn chart_is_unusable(c: &ChartInfo) -> bool {
    c.mintick_1e6 <= 0
        || c.pointvalue_1e6 <= 0
        || c.qty_step_1e6 < 0
        || c.min_notional_1e6 < 0
        || c.timeframe_s == 0
}

/// 5105 — the declaration's own domain (both brokers).
fn decl_is_out_of_domain(d: &StrategyDecl) -> bool {
    let r = &d.risk;
    if r.max_drawdown_1e6 < 0 || r.max_intraday_loss_1e6 < 0 || r.max_position_size_1e6 < 0 {
        return true;
    }
    if d.flags & decl_flag::DRAWDOWN_PCT != 0 && r.max_drawdown_1e6 > PCT_100 {
        return true;
    }
    if d.flags & decl_flag::INTRADAY_LOSS_PCT != 0 && r.max_intraday_loss_1e6 > PCT_100 {
        return true;
    }
    if d.commission_value_1e6 < 0 || d.margin_long_pct_1e6 < 0 || d.margin_short_pct_1e6 < 0 {
        return true;
    }
    if d.default_qty_value_1e6 <= 0 {
        return true;
    }
    let mut i = 0;
    while i < r._pad.len() {
        if r._pad[i] != 0 {
            return true;
        }
        i += 1;
    }
    // `fill_limits_ticks` takes any value (§4.8.6).
    d.flags & !KNOWN_FLAGS != 0
}

/// Every §3.7 refusal for `TvBroker::new`, in order. `history_ticks`: the `history_tick` switch
/// reads `PathPoints` (§4.2; `5109` lifted).
pub(crate) fn check_tv(
    meta: &ProgramMeta,
    cfg: &TvCfg,
    history_ticks: bool,
) -> Result<(), DiagCode> {
    check_program(meta, history_ticks)?;
    if chart_is_unusable(&meta.chart) {
        return Err(code::BAD_CHART_INFO);
    }
    if meta.decl.initial_capital_1e6 <= 0 {
        return Err(code::BAD_CAPITAL);
    }
    if decl_is_out_of_domain(&meta.decl)
        || cfg.day_offset_s <= -DAY_S
        || cfg.day_offset_s >= DAY_S
        || cfg._pad != 0
    {
        return Err(code::BAD_RISK_RULE);
    }
    if let SizingMode::FixedNotional { notional_1e6 } = cfg.sizing {
        if notional_1e6 <= 0 {
            return Err(code::ZERO_UNIT_NOTIONAL);
        }
    }
    Ok(())
}

/// The native configuration's own 5105 domain.
fn native_cfg_is_out_of_domain(cfg: &NativeCfg) -> bool {
    let mut pad_set = false;
    let mut i = 0;
    while i < cfg._pad.len() {
        pad_set |= cfg._pad[i] != 0;
        i += 1;
    }
    cfg.cap_instance_1e6 <= 0
        || cfg.cap_day_1e6 <= 0
        || cfg.quote_stale_ns <= 0
        || cfg.max_open < 1
        || cfg.max_open > cap::MAX_OPEN
        // One venue slot is the IoC's: a maker script with one slot could rest nothing (§4.12.3).
        || (cfg.limit_primitive == LimitPrimitive::Maker && cfg.max_open < 2)
        || cfg.cross_ticks > cap::MAX_CROSS_TICKS
        || cfg.live > 1
        || pad_set
}

/// Every §3.7 refusal for `NativeBroker::new`, in order.
pub(crate) fn check_native(meta: &ProgramMeta, cfg: &NativeCfg) -> Result<(), DiagCode> {
    check_program(meta, false)?;
    if chart_is_unusable(&meta.chart)
        || cfg.tick_1e6 <= 0
        || cfg.qty_step_1e6 < 0
        || cfg.min_notional_1e6 < 0
    {
        return Err(code::BAD_CHART_INFO);
    }
    if meta.decl.initial_capital_1e6 <= 0 {
        return Err(code::BAD_CAPITAL);
    }
    if decl_is_out_of_domain(&meta.decl) || native_cfg_is_out_of_domain(cfg) {
        return Err(code::BAD_RISK_RULE);
    }
    if cfg.unit_notional_1e6 <= 0 {
        return Err(code::ZERO_UNIT_NOTIONAL);
    }
    if cfg.live == 1 && meta.decl.default_qty_type == QtyType::PercentOfEquity {
        return Err(code::PCT_EQUITY_LIVE);
    }
    Ok(())
}

/// FNV-1a 64 from `FNV1A64_OFFSET` over every `StrategyDecl` field in declaration order, little
/// endian (§4.16) — equal to FNV-1a over the struct's 96 bytes on a little-endian target.
pub(crate) fn decl_hash64(d: &StrategyDecl) -> u64 {
    let r = &d.risk;
    let mut h = FNV1A64_OFFSET;
    h = fnv1a64_continue(h, &d.initial_capital_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &d.default_qty_value_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &d.commission_value_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &d.margin_long_pct_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &d.margin_short_pct_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &d.slippage_ticks.to_le_bytes());
    h = fnv1a64_continue(h, &d.pyramiding.to_le_bytes());
    h = fnv1a64_continue(h, &[d.default_qty_type as u8, d.commission_type as u8]);
    h = fnv1a64_continue(h, &d.flags.to_le_bytes());
    h = fnv1a64_continue(h, &d.fill_limits_ticks.to_le_bytes());
    h = fnv1a64_continue(h, &r.max_drawdown_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &r.max_intraday_loss_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &r.max_position_size_1e6.to_le_bytes());
    h = fnv1a64_continue(h, &r.max_intraday_filled_orders.to_le_bytes());
    h = fnv1a64_continue(h, &r.max_cons_loss_days.to_le_bytes());
    h = fnv1a64_continue(h, &[r.allow_entry_in as u8]);
    fnv1a64_continue(h, &r._pad)
}

#[cfg(test)]
mod tests {
    //! One test per §3.7 code (§7.2's names; `tests/construct.rs` repeats them through the public
    //! constructors once `TvBroker::new` / `NativeBroker::new` land, T11 / T14).

    use super::{check_native, decl_hash64};
    use crate::fault::code;
    use pine_ir::{
        decl_flag, fnv1a64_continue, AllowEntry, ChartInfo, CommissionType, DiagCode,
        LimitPrimitive, NativeCfg, ProgramMeta, QtyType, RiskRules, SizingMode, StrategyDecl,
        TvCfg, FNV1A64_OFFSET,
    };

    /// `check_tv` on the default `history_tick = PathPoints` reading (R-P18).
    fn check_tv(m: &ProgramMeta, c: &TvCfg) -> Result<(), DiagCode> {
        super::check_tv(m, c, true)
    }

    #[test]
    fn history_tick_lifted_under_path_points_5107_when_mis_paired() {
        // R-P6 / lead answers 6, 13 (`pin:05-hist-tick-refuse`), R-P18 (the default): `PathPoints`
        // lifts 5109 for the TV broker only; a history-tick program sema did not mark
        // `uses_intrabar` refuses 5107; the native broker keeps 5109; 5100 / 5101 still come first.
        let mut m = meta();
        m.decl.flags |= decl_flag::CALC_ON_EVERY_HISTORY_TICK;
        assert_eq!(check_tv(&m, &tv()), Err(code::INTRABAR_ON_BAR_CLOSE));
        m.uses_intrabar = true;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
        assert_eq!(check_native(&m, &native()), Err(code::HISTORY_TICK));
        m.nonstandard_chart = true;
        assert_eq!(check_tv(&m, &tv()), Err(code::NONSTANDARD_CHART));
    }

    fn meta() -> ProgramMeta {
        ProgramMeta {
            chart: ChartInfo {
                symbol_hash: 1,
                timeframe_s: 60,
                venue: 0,
                _pad: [0; 3],
                mintick_1e6: 100_000,
                pointvalue_1e6: 1_000_000,
                qty_step_1e6: 1_000,
                min_notional_1e6: 0,
            },
            decl: StrategyDecl {
                initial_capital_1e6: 1_000_000 * 1_000_000,
                default_qty_value_1e6: 1_000_000,
                pyramiding: 1,
                margin_long_pct_1e6: 100 * 1_000_000,
                margin_short_pct_1e6: 100 * 1_000_000,
                ..StrategyDecl::default()
            },
            ..ProgramMeta::default()
        }
    }

    fn tv() -> TvCfg {
        TvCfg {
            sizing: SizingMode::AsDeclared,
            day_offset_s: 0,
            _pad: 0,
        }
    }

    fn native() -> NativeCfg {
        NativeCfg {
            unit_notional_1e6: 100 * 1_000_000,
            cap_instance_1e6: 1_000_000_000,
            cap_day_1e6: 10_000_000_000,
            min_notional_1e6: 10_000_000,
            qty_step_1e6: 1_000,
            tick_1e6: 100_000,
            quote_stale_ns: 2_000_000_000,
            cross_ticks: 5,
            max_open: 4,
            limit_primitive: LimitPrimitive::Ioc,
            live: 0,
            _pad: [0; 6],
        }
    }

    #[test]
    fn defaults_are_admitted_by_both() {
        assert_eq!(check_tv(&meta(), &tv()), Ok(()));
        assert_eq!(check_native(&meta(), &native()), Ok(()));
    }

    #[test]
    fn bar_magnifier_is_refused_5100() {
        let mut m = meta();
        // 5100 precedes every later check, here 5101.
        m.decl.flags |= decl_flag::BAR_MAGNIFIER;
        m.nonstandard_chart = true;
        assert_eq!(check_tv(&m, &tv()), Err(code::BAR_MAGNIFIER));
        assert_eq!(check_native(&m, &native()), Err(code::BAR_MAGNIFIER));
    }

    #[test]
    fn nonstandard_chart_is_refused_5101() {
        let mut m = meta();
        m.nonstandard_chart = true;
        m.decl.flags |= decl_flag::CALC_ON_EVERY_HISTORY_TICK;
        assert_eq!(check_tv(&m, &tv()), Err(code::NONSTANDARD_CHART));
        assert_eq!(check_native(&m, &native()), Err(code::NONSTANDARD_CHART));
    }

    #[test]
    fn history_tick_is_refused_5109_by_native_and_under_refuse() {
        // R-P18: the TV default admits it; `history_tick = Refuse` (selected explicitly) and the
        // native broker refuse 5109.
        let mut m = meta();
        m.decl.flags |= decl_flag::CALC_ON_EVERY_HISTORY_TICK;
        m.uses_intrabar = true;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
        assert_eq!(super::check_tv(&m, &tv(), false), Err(code::HISTORY_TICK));
        assert_eq!(check_native(&m, &native()), Err(code::HISTORY_TICK));
    }

    #[test]
    fn cof_without_uses_intrabar_is_refused_5107() {
        let mut m = meta();
        m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
        assert_eq!(check_tv(&m, &tv()), Err(code::INTRABAR_ON_BAR_CLOSE));
        assert_eq!(
            check_native(&m, &native()),
            Err(code::INTRABAR_ON_BAR_CLOSE)
        );
        m.uses_intrabar = true;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
    }

    #[test]
    fn pyramiding_101_is_refused_5102() {
        let mut m = meta();
        m.decl.pyramiding = 100;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
        m.decl.pyramiding = 101;
        assert_eq!(check_tv(&m, &tv()), Err(code::PYRAMIDING_TOO_LARGE));
        assert_eq!(check_native(&m, &native()), Err(code::PYRAMIDING_TOO_LARGE));
        // 0 reads as 1.
        m.decl.pyramiding = 0;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
    }

    #[test]
    fn zero_mintick_is_refused_5103() {
        let mut m = meta();
        m.chart.mintick_1e6 = 0;
        assert_eq!(check_tv(&m, &tv()), Err(code::BAD_CHART_INFO));
        assert_eq!(check_native(&m, &native()), Err(code::BAD_CHART_INFO));
        let cases: [fn(&mut ProgramMeta); 4] = [
            |m| m.chart.pointvalue_1e6 = 0,
            |m| m.chart.qty_step_1e6 = -1,
            |m| m.chart.min_notional_1e6 = -1,
            |m| m.chart.timeframe_s = 0,
        ];
        let mut i = 0;
        while i < cases.len() {
            let mut m = meta();
            cases[i](&mut m);
            assert_eq!(check_tv(&m, &tv()), Err(code::BAD_CHART_INFO), "case {i}");
            i += 1;
        }
        let ncases: [fn(&mut NativeCfg); 3] = [
            |c| c.tick_1e6 = 0,
            |c| c.qty_step_1e6 = -1,
            |c| c.min_notional_1e6 = -1,
        ];
        let mut i = 0;
        while i < ncases.len() {
            let mut c = native();
            ncases[i](&mut c);
            assert_eq!(
                check_native(&meta(), &c),
                Err(code::BAD_CHART_INFO),
                "case {i}"
            );
            i += 1;
        }
    }

    #[test]
    fn zero_capital_is_refused_5104() {
        let mut m = meta();
        m.decl.initial_capital_1e6 = 0;
        assert_eq!(check_tv(&m, &tv()), Err(code::BAD_CAPITAL));
        assert_eq!(check_native(&m, &native()), Err(code::BAD_CAPITAL));
    }

    #[test]
    fn negative_risk_bound_is_refused_5105() {
        let cases: [fn(&mut ProgramMeta); 10] = [
            |m| m.decl.risk.max_drawdown_1e6 = -1,
            |m| m.decl.risk.max_intraday_loss_1e6 = -1,
            |m| m.decl.risk.max_position_size_1e6 = -1,
            |m| {
                m.decl.flags |= decl_flag::DRAWDOWN_PCT;
                m.decl.risk.max_drawdown_1e6 = 100 * 1_000_000 + 1;
            },
            |m| {
                m.decl.flags |= decl_flag::INTRADAY_LOSS_PCT;
                m.decl.risk.max_intraday_loss_1e6 = 100 * 1_000_000 + 1;
            },
            |m| m.decl.commission_value_1e6 = -1,
            |m| m.decl.margin_long_pct_1e6 = -1,
            |m| m.decl.margin_short_pct_1e6 = -1,
            |m| m.decl.default_qty_value_1e6 = 0,
            |m| m.decl.risk._pad[6] = 1,
        ];
        let mut i = 0;
        while i < cases.len() {
            let mut m = meta();
            cases[i](&mut m);
            assert_eq!(check_tv(&m, &tv()), Err(code::BAD_RISK_RULE), "case {i}");
            assert_eq!(
                check_native(&m, &native()),
                Err(code::BAD_RISK_RULE),
                "case {i}"
            );
            i += 1;
        }
        // A cash bound above 100e6 is not a percent and is admitted.
        let mut m = meta();
        m.decl.risk.max_drawdown_1e6 = 100 * 1_000_000 + 1;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
    }

    #[test]
    fn unknown_decl_flag_bit_is_refused_5105() {
        let mut m = meta();
        m.decl.flags |= 1 << 9;
        assert_eq!(check_tv(&m, &tv()), Err(code::BAD_RISK_RULE));
        assert_eq!(check_native(&m, &native()), Err(code::BAD_RISK_RULE));
    }

    #[test]
    fn fill_limits_ticks_takes_any_value() {
        let mut m = meta();
        m.decl.fill_limits_ticks = u32::MAX;
        assert_eq!(check_tv(&m, &tv()), Ok(()));
        assert_eq!(check_native(&m, &native()), Ok(()));
    }

    #[test]
    fn maker_with_max_open_one_is_refused_5105() {
        let mut c = native();
        c.limit_primitive = LimitPrimitive::Maker;
        c.max_open = 1;
        assert_eq!(check_native(&meta(), &c), Err(code::BAD_RISK_RULE));
        c.max_open = 2;
        assert_eq!(check_native(&meta(), &c), Ok(()));
        c.limit_primitive = LimitPrimitive::Ioc;
        c.max_open = 1;
        assert_eq!(check_native(&meta(), &c), Ok(()));
    }

    #[test]
    fn max_open_nine_is_refused_5105() {
        let mut c = native();
        c.max_open = 8;
        assert_eq!(check_native(&meta(), &c), Ok(()));
        c.max_open = 9;
        assert_eq!(check_native(&meta(), &c), Err(code::BAD_RISK_RULE));
        c.max_open = 0;
        assert_eq!(check_native(&meta(), &c), Err(code::BAD_RISK_RULE));
        let cases: [fn(&mut NativeCfg); 7] = [
            |c| c.cap_instance_1e6 = 0,
            |c| c.cap_day_1e6 = 0,
            |c| c.quote_stale_ns = 0,
            |c| c.cross_ticks = 1001,
            |c| c.live = 2,
            |c| c._pad[5] = 1,
            |c| c.max_open = u32::MAX,
        ];
        let mut i = 0;
        while i < cases.len() {
            let mut c = native();
            cases[i](&mut c);
            assert_eq!(
                check_native(&meta(), &c),
                Err(code::BAD_RISK_RULE),
                "case {i}"
            );
            i += 1;
        }
        let mut c = native();
        c.cross_ticks = 1000;
        assert_eq!(check_native(&meta(), &c), Ok(()));
    }

    #[test]
    fn day_offset_of_a_full_day_is_refused_5105() {
        let mut c = tv();
        c.day_offset_s = 86_399;
        assert_eq!(check_tv(&meta(), &c), Ok(()));
        c.day_offset_s = -86_399;
        assert_eq!(check_tv(&meta(), &c), Ok(()));
        c.day_offset_s = 86_400;
        assert_eq!(check_tv(&meta(), &c), Err(code::BAD_RISK_RULE));
        c.day_offset_s = -86_400;
        assert_eq!(check_tv(&meta(), &c), Err(code::BAD_RISK_RULE));
        let mut c = tv();
        c._pad = 1;
        assert_eq!(check_tv(&meta(), &c), Err(code::BAD_RISK_RULE));
    }

    #[test]
    fn zero_unit_notional_is_refused_5106() {
        let c = TvCfg {
            sizing: SizingMode::FixedNotional { notional_1e6: 0 },
            ..tv()
        };
        assert_eq!(check_tv(&meta(), &c), Err(code::ZERO_UNIT_NOTIONAL));
        let c = TvCfg {
            sizing: SizingMode::FixedNotional { notional_1e6: 1 },
            ..tv()
        };
        assert_eq!(check_tv(&meta(), &c), Ok(()));
        let mut n = native();
        n.unit_notional_1e6 = 0;
        assert_eq!(check_native(&meta(), &n), Err(code::ZERO_UNIT_NOTIONAL));
    }

    #[test]
    fn pct_equity_on_live_is_refused_5108() {
        let mut m = meta();
        m.decl.default_qty_type = QtyType::PercentOfEquity;
        let mut c = native();
        c.live = 1;
        assert_eq!(check_native(&m, &c), Err(code::PCT_EQUITY_LIVE));
        // TV mode has no Live route.
        assert_eq!(check_tv(&m, &tv()), Ok(()));
    }

    #[test]
    fn pct_equity_on_paper_is_admitted() {
        let mut m = meta();
        m.decl.default_qty_type = QtyType::PercentOfEquity;
        assert_eq!(check_native(&m, &native()), Ok(()));
    }

    #[test]
    fn decl_hash_is_fnv_over_the_96_bytes() {
        assert_eq!(core::mem::size_of::<StrategyDecl>(), 96);
        let d = StrategyDecl {
            initial_capital_1e6: 0x0102_0304_0506_0708,
            default_qty_value_1e6: -2,
            commission_value_1e6: 3,
            margin_long_pct_1e6: 4,
            margin_short_pct_1e6: 5,
            slippage_ticks: 6,
            pyramiding: 7,
            default_qty_type: QtyType::Cash,
            commission_type: CommissionType::CashPerOrder,
            flags: 0x0123,
            fill_limits_ticks: 9,
            risk: RiskRules {
                max_drawdown_1e6: 10,
                max_intraday_loss_1e6: 11,
                max_position_size_1e6: 12,
                max_intraday_filled_orders: 13,
                max_cons_loss_days: 14,
                allow_entry_in: AllowEntry::Short,
                _pad: [1, 2, 3, 4, 5, 6, 7],
            },
        };
        // SAFETY: `StrategyDecl` is `#[repr(C)]`, 96 bytes, with no implicit padding (asserted
        // above and by pine-ir's layout); every byte is initialised, so viewing it as `[u8; 96]`
        // reads no uninitialised memory.
        let bytes: [u8; 96] = unsafe { core::mem::transmute(d) };
        let raw = fnv1a64_continue(FNV1A64_OFFSET, &bytes);
        if cfg!(target_endian = "little") {
            assert_eq!(decl_hash64(&d), raw);
        }
        let mut e = d;
        e.risk._pad[0] = 0;
        assert_ne!(decl_hash64(&d), decl_hash64(&e));
    }
}
