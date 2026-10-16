// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! §4.13.1's fold on pass 3's output (impl-02 §7.11.1's `decl_*` field tests, private twins until
//! `ProgramMeta`, T20).

use pine_ir::{
    decl_flag, AllowEntry, CommissionType, Dialect, FileId, QtyType, RiskRules, ScriptKind,
    StrategyDecl, SymTable,
};

use super::readings::opts;
use crate::decl::{self, Decl};
use crate::sw::Readings;
use crate::{imports, infer, resolve, LibCache};

/// One unit at the contract's ×1e6 scale.
const E6: i64 = 1_000_000;

/// Passes 1–3 and the declaration fold of `src`; the codes without the snapshot's `unserved=1`.
fn fold(src: &str, d: Dialect) -> (Decl, Vec<u16>) {
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parses");
    let rd = Readings::of(d);
    let o = opts(d);
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, &rd, &mut diags);
    let t = infer::infer(&ast, &graph, &res, &mut syms, &rd, 0b0111, &[], &mut diags);
    let decl = decl::check(&ast, &t, &o, &syms, &mut diags);
    diags.retain(|x| !(x.code.0 == 3024 && x.msg.contains(" unserved=1")));
    (decl, diags.iter().map(|x| x.code.0).collect())
}

fn ok(src: &str, d: Dialect) -> Decl {
    let (decl, codes) = fold(src, d);
    assert!(codes.is_empty(), "{codes:?}");
    decl
}

fn v6(args: &str) -> Decl {
    ok(
        &format!("//@version=6\nstrategy(\"s\"{args})\nplot(close)\n"),
        Dialect::V6,
    )
}

#[test]
fn decl_strategy_fields_follow_the_declaration() {
    let d = v6(
        ", initial_capital = 5000, default_qty_type = strategy.cash, \
                default_qty_value = 250.5, commission_type = strategy.commission.cash_per_order, \
                commission_value = 0.25, margin_long = 50, margin_short = 25.5, slippage = 3, \
                pyramiding = 2",
    );
    assert_eq!(d.kind, ScriptKind::Strategy);
    let s = d.strategy;
    assert_eq!(s.initial_capital_1e6, 5_000 * E6);
    assert_eq!(s.default_qty_type, QtyType::Cash);
    assert_eq!(s.default_qty_value_1e6, 250_500_000);
    assert_eq!(s.commission_type, CommissionType::CashPerOrder);
    assert_eq!(s.commission_value_1e6, 250_000);
    assert_eq!(s.margin_long_pct_1e6, 50 * E6);
    assert_eq!(s.margin_short_pct_1e6, 25_500_000);
    assert_eq!((s.slippage_ticks, s.pyramiding), (3, 2));
    assert_eq!(s.flags, 0);
    assert!(!d.uses_intrabar);
}

#[test]
fn decl_strategy_fields_default_by_dialect() {
    // v6: 100 000 capital, 100 % of equity; v5: 1 000 000, 1 contract (pine-spec's rows).
    let s = v6("").strategy;
    assert_eq!(s.initial_capital_1e6, 100_000 * E6);
    assert_eq!(s.default_qty_type, QtyType::PercentOfEquity);
    assert_eq!(s.default_qty_value_1e6, 100 * E6);
    assert_eq!(s.commission_type, CommissionType::Percent);
    assert_eq!(
        (s.commission_value_1e6, s.slippage_ticks, s.pyramiding),
        (0, 0, 0)
    );
    let s5 = ok("//@version=5\nstrategy(\"s\")\nplot(close)\n", Dialect::V5).strategy;
    assert_eq!(s5.initial_capital_1e6, 1_000_000 * E6);
    assert_eq!(s5.default_qty_type, QtyType::FixedContracts);
    assert_eq!(s5.default_qty_value_1e6, 1_000_000);
}

#[test]
fn decl_strategy_fields_na_and_negative_counts_are_refused() {
    let src = "//@version=6\nstrategy(\"s\", initial_capital = na)\nplot(close)\n";
    assert_eq!(fold(src, Dialect::V6).1, vec![3007]);
    let src = "//@version=6\nstrategy(\"s\", slippage = -1)\nplot(close)\n";
    assert_eq!(fold(src, Dialect::V6).1, vec![3007]);
    let src = "//@version=6\nstrategy(\"s\", pyramiding = -2)\nplot(close)\n";
    assert_eq!(fold(src, Dialect::V6).1, vec![3007]);
}

#[test]
fn decl_margin_defaults_follow_the_dialect() {
    let s6 = v6("").strategy;
    assert_eq!(
        (s6.margin_long_pct_1e6, s6.margin_short_pct_1e6),
        (100 * E6, 100 * E6)
    );
    let s5 = ok("//@version=5\nstrategy(\"s\")\nplot(close)\n", Dialect::V5).strategy;
    assert_eq!((s5.margin_long_pct_1e6, s5.margin_short_pct_1e6), (0, 0));
}

#[test]
fn decl_flags_follow_the_declaration() {
    for (arg, bit) in [
        ("process_orders_on_close = true", decl_flag::ORDERS_ON_CLOSE),
        (
            "fill_orders_on_standard_ohlc = true",
            decl_flag::FILL_ON_STANDARD_OHLC,
        ),
        ("close_entries_rule = \"ANY\"", decl_flag::CLOSE_ANY),
        ("use_bar_magnifier = true", decl_flag::BAR_MAGNIFIER),
        (
            "calc_on_every_history_tick = true",
            decl_flag::CALC_ON_EVERY_HISTORY_TICK,
        ),
    ] {
        assert_eq!(v6(&format!(", {arg}")).strategy.flags, bit, "{arg}");
    }
    assert_eq!(v6(", close_entries_rule = \"FIFO\"").strategy.flags, 0);
}

#[test]
fn decl_history_tick_flag_sets_uses_intrabar() {
    assert!(v6(", calc_on_every_history_tick = true").uses_intrabar);
    assert!(!v6(", calc_on_every_history_tick = false").uses_intrabar);
}

#[test]
fn decl_fill_limits_ticks_is_recorded() {
    assert_eq!(
        v6(", backtest_fill_limits_assumption = 4")
            .strategy
            .fill_limits_ticks,
        4
    );
    assert_eq!(v6("").strategy.fill_limits_ticks, 0);
}

#[test]
fn decl_risk_rules_are_folded() {
    let src =
        "//@version=6\nstrategy(\"s\")\nstrategy.risk.allow_entry_in(strategy.direction.long)\n\
               strategy.risk.max_position_size(2.5)\nstrategy.risk.max_intraday_filled_orders(7)\n\
               strategy.risk.max_cons_loss_days(3)\nplot(close)\n";
    let d = ok(src, Dialect::V6);
    assert_eq!(
        d.strategy.risk,
        RiskRules {
            max_position_size_1e6: 2_500_000,
            max_intraday_filled_orders: 7,
            max_cons_loss_days: 3,
            allow_entry_in: AllowEntry::Long,
            ..RiskRules::default()
        }
    );
    assert_eq!(d.strategy.flags & decl_flag::DRAWDOWN_PCT, 0);
    // `max_drawdown` / `max_intraday_loss` take `type = strategy.percent_of_equity` / `cash`
    // (const int codes, I26); pine-spec types that parameter `string` (a data defect the lead
    // fixes there), so the call is `3007` until then and its fold is not reachable here.
    let dd = "//@version=6\nstrategy(\"s\")\nstrategy.risk.max_drawdown(10, strategy.percent_of_equity)\n\
              plot(close)\n";
    assert_eq!(fold(dd, Dialect::V6).1, vec![3007]);
}

#[test]
fn decl_indicator_gets_strategy_defaults() {
    let d = ok("//@version=6\nindicator(\"i\")\nplot(close)\n", Dialect::V6);
    assert_eq!(d.kind, ScriptKind::Indicator);
    assert_eq!(d.strategy, v6("").strategy);
    let d5 = ok("//@version=5\nindicator(\"i\")\nplot(close)\n", Dialect::V5);
    let s5 = ok("//@version=5\nstrategy(\"s\")\nplot(close)\n", Dialect::V5);
    assert_eq!(d5.strategy, s5.strategy);
}

#[test]
fn decl_padding_is_zero() {
    let d = v6(", slippage = 1");
    assert_eq!(d.strategy.risk._pad, [0; 7]);
    let StrategyDecl { risk, .. } = d.strategy;
    assert_eq!(risk._pad, [0; 7]);
}

#[test]
fn decl_unsupported_parameters_given_a_non_default_value_are_deferred() {
    let (_, c) = fold(
        "//@version=6\nstrategy(\"s\", calc_bars_count = 500)\nplot(close)\n",
        Dialect::V6,
    );
    assert_eq!(c, vec![3024]);
    assert!(fold(
        "//@version=6\nstrategy(\"s\", calc_bars_count = 0)\nplot(close)\n",
        Dialect::V6
    )
    .1
    .is_empty());
    for (tf, want) in [
        ("\"240\"", vec![3024]),
        ("\"\"", vec![]),
        ("\"60\"", vec![]), // the chart's own period (3600 s)
        ("na", vec![]),
    ] {
        let src = format!("//@version=6\nindicator(\"i\", timeframe = {tf})\nplot(close)\n");
        assert_eq!(fold(&src, Dialect::V6).1, want, "{tf}");
    }
    let gaps = "//@version=6\nindicator(\"i\", timeframe_gaps = false)\nplot(close)\n";
    assert_eq!(fold(gaps, Dialect::V6).1, vec![3024]);
    let gaps = "//@version=6\nindicator(\"i\", timeframe_gaps = true)\nplot(close)\n";
    assert!(fold(gaps, Dialect::V6).1.is_empty());
}
