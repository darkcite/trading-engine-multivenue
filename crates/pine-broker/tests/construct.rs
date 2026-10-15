// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.2 through the public constructor (`TvBroker::new`). The native-only codes (5108,
//! `max_open`, `Maker`) join when `NativeBroker::new` lands (T14); their unit tests are in
//! `src/decl.rs`.

use pine_broker::{code, EngineIntentBuf, TvBroker, TvStep};
use pine_ir::{
    decl_flag, fnv1a64_continue, ChartInfo, ProgramMeta, SizingMode, StrategyDecl, TvCfg,
};

fn meta() -> ProgramMeta {
    ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 60,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 10_000,
            pointvalue_1e6: 1_000_000,
            qty_step_1e6: 0,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 1_000_000 * 1_000_000,
            default_qty_value_1e6: 1_000_000,
            pyramiding: 1,
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

fn refused(m: &ProgramMeta, c: TvCfg) -> Option<u16> {
    TvBroker::new(m, c).err().map(|d| d.0)
}

#[test]
fn defaults_are_admitted() {
    assert!(TvBroker::new(&meta(), tv()).is_ok());
}

#[test]
fn bar_magnifier_is_refused_5100() {
    let mut m = meta();
    m.decl.flags |= decl_flag::BAR_MAGNIFIER;
    assert_eq!(refused(&m, tv()), Some(code::BAR_MAGNIFIER.0));
}

#[test]
fn nonstandard_chart_is_refused_5101() {
    let mut m = meta();
    m.nonstandard_chart = true;
    assert_eq!(refused(&m, tv()), Some(code::NONSTANDARD_CHART.0));
}

#[test]
fn history_tick_is_refused_5109() {
    let mut m = meta();
    m.decl.flags |= decl_flag::CALC_ON_EVERY_HISTORY_TICK;
    m.uses_intrabar = true;
    assert_eq!(refused(&m, tv()), Some(code::HISTORY_TICK.0));
}

#[test]
fn unknown_decl_flag_bit_is_refused_5105() {
    let mut m = meta();
    m.decl.flags |= 1 << 12;
    assert_eq!(refused(&m, tv()), Some(code::BAD_RISK_RULE.0));
}

#[test]
fn fill_limits_ticks_takes_any_value() {
    let mut m = meta();
    m.decl.fill_limits_ticks = u32::MAX;
    assert!(TvBroker::new(&m, tv()).is_ok());
}

#[test]
fn cof_without_uses_intrabar_is_refused_5107() {
    let mut m = meta();
    m.decl.flags |= decl_flag::CALC_ON_ORDER_FILLS;
    assert_eq!(refused(&m, tv()), Some(code::INTRABAR_ON_BAR_CLOSE.0));
}

#[test]
fn pyramiding_101_is_refused_5102() {
    let mut m = meta();
    m.decl.pyramiding = 101;
    assert_eq!(refused(&m, tv()), Some(code::PYRAMIDING_TOO_LARGE.0));
}

#[test]
fn zero_mintick_is_refused_5103() {
    let mut m = meta();
    m.chart.mintick_1e6 = 0;
    assert_eq!(refused(&m, tv()), Some(code::BAD_CHART_INFO.0));
}

#[test]
fn zero_capital_is_refused_5104() {
    let mut m = meta();
    m.decl.initial_capital_1e6 = 0;
    assert_eq!(refused(&m, tv()), Some(code::BAD_CAPITAL.0));
}

#[test]
fn negative_risk_bound_is_refused_5105() {
    let mut m = meta();
    m.decl.risk.max_drawdown_1e6 = -1;
    assert_eq!(refused(&m, tv()), Some(code::BAD_RISK_RULE.0));
}

#[test]
fn day_offset_of_a_full_day_is_refused_5105() {
    let c = TvCfg {
        day_offset_s: 86_400,
        ..tv()
    };
    assert_eq!(refused(&meta(), c), Some(code::BAD_RISK_RULE.0));
}

#[test]
fn zero_unit_notional_is_refused_5106() {
    let c = TvCfg {
        sizing: SizingMode::FixedNotional { notional_1e6: 0 },
        ..tv()
    };
    assert_eq!(refused(&meta(), c), Some(code::ZERO_UNIT_NOTIONAL.0));
}

#[test]
fn decl_hash_is_fnv_over_the_96_bytes() {
    let m = meta();
    let b = TvBroker::new(&m, tv()).unwrap_or_else(|_| unreachable!());
    let d = m.decl;
    let mut bytes = [0u8; 96];
    // The field-by-field little-endian image of the 96-byte struct (no implicit padding).
    let mut w = 0;
    let mut put = |x: &[u8]| {
        // COPY: test-only assembly of the reference byte image.
        bytes[w..w + x.len()].copy_from_slice(x);
        w += x.len();
    };
    put(&d.initial_capital_1e6.to_le_bytes());
    put(&d.default_qty_value_1e6.to_le_bytes());
    put(&d.commission_value_1e6.to_le_bytes());
    put(&d.margin_long_pct_1e6.to_le_bytes());
    put(&d.margin_short_pct_1e6.to_le_bytes());
    put(&d.slippage_ticks.to_le_bytes());
    put(&d.pyramiding.to_le_bytes());
    put(&[d.default_qty_type as u8, d.commission_type as u8]);
    put(&d.flags.to_le_bytes());
    put(&d.fill_limits_ticks.to_le_bytes());
    put(&d.risk.max_drawdown_1e6.to_le_bytes());
    put(&d.risk.max_intraday_loss_1e6.to_le_bytes());
    put(&d.risk.max_position_size_1e6.to_le_bytes());
    put(&d.risk.max_intraday_filled_orders.to_le_bytes());
    put(&d.risk.max_cons_loss_days.to_le_bytes());
    put(&[d.risk.allow_entry_in as u8]);
    put(&d.risk._pad);
    assert_eq!(
        b.record().decl_hash64,
        fnv1a64_continue(pine_ir::FNV1A64_OFFSET, &bytes)
    );
}

#[test]
fn tv_step_and_buf_have_their_frozen_sizes() {
    assert_eq!(core::mem::size_of::<TvStep>(), 128);
    assert_eq!(core::mem::size_of::<EngineIntentBuf>(), 2112);
    assert_eq!(core::mem::align_of::<EngineIntentBuf>(), 64);
}

fn native() -> pine_ir::NativeCfg {
    pine_ir::NativeCfg {
        unit_notional_1e6: 100 * 1_000_000,
        cap_instance_1e6: 1_000_000_000,
        cap_day_1e6: 10_000_000_000,
        min_notional_1e6: 10_000_000,
        qty_step_1e6: 1_000,
        tick_1e6: 10_000,
        quote_stale_ns: 2_000_000_000,
        cross_ticks: 5,
        max_open: 4,
        limit_primitive: pine_ir::LimitPrimitive::Ioc,
        live: 0,
        _pad: [0; 6],
    }
}

fn refused_native(m: &ProgramMeta, c: pine_ir::NativeCfg) -> Option<u16> {
    pine_broker::NativeBroker::new(m, c).err().map(|d| d.0)
}

#[test]
fn native_defaults_are_admitted() {
    assert_eq!(refused_native(&meta(), native()), None);
}

#[test]
fn maker_with_max_open_one_is_refused_5105() {
    let c = pine_ir::NativeCfg {
        limit_primitive: pine_ir::LimitPrimitive::Maker,
        max_open: 1,
        ..native()
    };
    assert_eq!(refused_native(&meta(), c), Some(code::BAD_RISK_RULE.0));
}

#[test]
fn max_open_nine_is_refused_5105() {
    let c = pine_ir::NativeCfg {
        max_open: 9,
        ..native()
    };
    assert_eq!(refused_native(&meta(), c), Some(code::BAD_RISK_RULE.0));
}

#[test]
fn zero_native_unit_notional_is_refused_5106() {
    let c = pine_ir::NativeCfg {
        unit_notional_1e6: 0,
        ..native()
    };
    assert_eq!(refused_native(&meta(), c), Some(code::ZERO_UNIT_NOTIONAL.0));
}

#[test]
fn pct_equity_on_live_is_refused_5108() {
    let mut m = meta();
    m.decl.default_qty_type = pine_ir::QtyType::PercentOfEquity;
    let c = pine_ir::NativeCfg {
        live: 1,
        ..native()
    };
    assert_eq!(refused_native(&m, c), Some(code::PCT_EQUITY_LIVE.0));
}

#[test]
fn pct_equity_on_paper_is_admitted() {
    let mut m = meta();
    m.decl.default_qty_type = pine_ir::QtyType::PercentOfEquity;
    assert_eq!(refused_native(&m, native()), None);
}
