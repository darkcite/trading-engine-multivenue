// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.1's `decl_*` refusals (§4.13.1). The `StrategyDecl` / `RiskRules` field tests
//! need `ProgramMeta` (T8 part 2 + T19).

mod common;

use common::*;
use pine_ir::{Dialect, ExecMode};

fn run(src: &str, mode: ExecMode) -> Vec<u16> {
    codes(&compile_src(src, &default_opts(Dialect::V6, mode)))
}

fn strategy(args: &str) -> String {
    format!("//@version=6\nstrategy(\"s\"{args})\nplot(close)\n")
}

#[test]
fn decl_missing_or_duplicate_declaration_is_refused() {
    assert_eq!(
        run("//@version=6\nplot(close)\n", ExecMode::BarClose),
        vec![3041]
    );
    assert_eq!(
        run(
            "//@version=6\nindicator(\"a\")\nindicator(\"b\")\nplot(close)\n",
            ExecMode::BarClose
        ),
        vec![3041]
    );
    assert_eq!(run(&strategy(""), ExecMode::BarClose), vec![ACCEPTED]);
}

#[test]
fn decl_declaration_outside_top_level_is_refused() {
    let c = run(
        "//@version=6\nindicator(\"t\")\nif close > open\n    \
         strategy.risk.allow_entry_in(strategy.direction.long)\nplot(close)\n",
        ExecMode::BarClose,
    );
    assert_eq!(c, vec![3008]);
    let c = run(
        "//@version=6\nindicator(\"t\")\nf() =>\n    strategy.risk.max_position_size(1)\n    1\n\
         plot(f())\n",
        ExecMode::BarClose,
    );
    assert_eq!(c, vec![3008]);
}

#[test]
fn decl_calc_on_order_fills_in_bar_close_is_refused() {
    let s = strategy(", calc_on_order_fills = true");
    assert_eq!(run(&s, ExecMode::BarClose), vec![3052]);
    assert_eq!(run(&s, ExecMode::Realtime), vec![ACCEPTED]);
}

#[test]
fn decl_calc_on_every_tick_in_bar_close_is_refused() {
    let s = strategy(", calc_on_every_tick = true");
    assert_eq!(run(&s, ExecMode::BarClose), vec![3052]);
    assert_eq!(run(&s, ExecMode::Realtime), vec![ACCEPTED]);
    let off = strategy(", calc_on_every_tick = false");
    assert_eq!(run(&off, ExecMode::BarClose), vec![ACCEPTED]);
}

/// R-P6 (delta 2026-10-17 §2.4): `calc_on_every_history_tick` joins rule `3052`.
#[test]
fn decl_calc_on_every_history_tick_in_bar_close_is_refused() {
    let s = strategy(", calc_on_every_history_tick = true");
    assert_eq!(run(&s, ExecMode::BarClose), vec![3052]);
    assert_eq!(run(&s, ExecMode::Realtime), vec![ACCEPTED]);
    let off = strategy(", calc_on_every_history_tick = false");
    assert_eq!(run(&off, ExecMode::BarClose), vec![ACCEPTED]);
}

#[test]
fn decl_close_entries_rule_other_than_fifo_any_is_refused() {
    let lifo = strategy(", close_entries_rule = \"LIFO\"");
    assert_eq!(run(&lifo, ExecMode::BarClose), vec![3007]);
    for ok in ["FIFO", "ANY"] {
        let s = strategy(&format!(", close_entries_rule = \"{ok}\""));
        assert_eq!(run(&s, ExecMode::BarClose), vec![ACCEPTED], "{ok}");
    }
}

#[test]
fn decl_currency_other_than_chart_is_refused() {
    let eur = strategy(", currency = currency.EUR");
    assert_eq!(run(&eur, ExecMode::BarClose), vec![3053]);
    for ok in ["currency.NONE", "\"USDT\""] {
        let s = strategy(&format!(", currency = {ok}"));
        assert_eq!(run(&s, ExecMode::BarClose), vec![ACCEPTED], "{ok}");
    }
}

#[test]
fn decl_risk_rule_twice_is_refused() {
    let src = "//@version=6\nstrategy(\"s\")\nstrategy.risk.max_position_size(1)\n\
               strategy.risk.max_position_size(2)\nplot(close)\n";
    assert_eq!(run(src, ExecMode::BarClose), vec![3041]);
    let once = "//@version=6\nstrategy(\"s\")\nstrategy.risk.max_position_size(1)\n\
                strategy.risk.max_intraday_filled_orders(5)\nplot(close)\n";
    assert_eq!(run(once, ExecMode::BarClose), vec![ACCEPTED]);
}

#[test]
fn decl_arguments_must_fold() {
    let s = strategy(", pyramiding = bar_index");
    assert_eq!(run(&s, ExecMode::BarClose), vec![3008]);
    // `pyramiding` is `const int` in pine-spec: an input is §4.5 step 5's `3008` there.
    let s = strategy(", pyramiding = input.int(2, \"P\")");
    assert_eq!(run(&s, ExecMode::BarClose), vec![3008]);
    let s = strategy(", pyramiding = 1 + 1");
    assert_eq!(run(&s, ExecMode::BarClose), vec![ACCEPTED]);
}

/// §4.10.4: the declaration's `max_bars_back` is the cap `C` when the manifest sets none, and a
/// declaration above the manifest's cap is `3017`.
#[test]
fn decl_max_bars_back_is_the_cap_without_a_manifest_one() {
    let src =
        "//@version=6\nindicator(\"t\", max_bars_back = 50)\nk = bar_index % 3\nplot(close[k])\n";
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.limits.max_bars_back = 0;
    let p = compile_src(src, &o).expect("D caps the dynamic read");
    assert_eq!(p.meta.max_bars_back, 50);
    let bare = "//@version=6\nindicator(\"t\")\nk = bar_index % 3\nplot(close[k])\n";
    assert_eq!(codes(&compile_src(bare, &o)), vec![3016]);
    o.limits.max_bars_back = 20;
    assert_eq!(codes(&compile_src(src, &o)), vec![3017]);
}
