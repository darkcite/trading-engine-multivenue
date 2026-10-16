// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.1 `tests/meta.rs`: one test per `ProgramMeta` field (I14, I23, I24). The
//! `inputs` and `decl` fields have their own batteries (`meta_inputs.rs`, `decl.rs`); a
//! sub-program's fields follow §4.16's child column (`meta_subprogram_fields_follow_the_child_column`).

mod common;

use common::{codes, compile_src, default_opts};
use pine_ir::{CompileOpts, Dialect, ExecMode, PineProgram, PlotKind, ScriptKind};

fn opts() -> CompileOpts {
    default_opts(Dialect::V6, ExecMode::BarClose)
}

fn prog_with(src: &str, o: &CompileOpts) -> PineProgram {
    match compile_src(src, o) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}\n{src}"),
    }
}

fn ind(body: &str) -> PineProgram {
    prog_with(&format!("//@version=6\nindicator(\"t\")\n{body}"), &opts())
}

fn strat(body: &str) -> PineProgram {
    prog_with(&format!("//@version=6\nstrategy(\"s\")\n{body}"), &opts())
}

#[test]
fn meta_source_sha256_is_the_opts_value_and_zero_in_children() {
    let mut o = opts();
    o.source_sha256 = [7; 32];
    let src = "//@version=6\nindicator(\"t\")\n\
               plot(request.security(syminfo.tickerid, \"D\", close[1]))\n";
    let p = prog_with(src, &o);
    assert_eq!(p.meta.source_sha256, [7; 32]);
    assert_eq!(p.subprograms.len(), 1);
    assert_eq!(p.subprograms[0].program.meta.source_sha256, [0; 32]);
}

#[test]
fn meta_spec_sha256_is_pine_spec() {
    assert_eq!(
        ind("plot(close)\n").meta.spec_sha256,
        pine_spec::SPEC_SHA256
    );
}

#[test]
fn meta_kind_follows_the_declaration() {
    assert_eq!(ind("plot(close)\n").meta.kind, ScriptKind::Indicator);
    assert_eq!(strat("plot(close)\n").meta.kind, ScriptKind::Strategy);
}

#[test]
fn meta_dialect_is_the_declared_one() {
    for d in [Dialect::V5, Dialect::V6] {
        let src = format!("//@version={}\nindicator(\"t\")\nplot(close)\n", d as u8);
        let p = prog_with(&src, &default_opts(d, ExecMode::BarClose));
        assert_eq!(p.meta.dialect, d);
        assert_eq!(p.dialect, d);
    }
}

#[test]
fn meta_max_bars_back_is_the_deepest_demand() {
    assert_eq!(ind("plot(close)\n").meta.max_bars_back, 0);
    assert_eq!(ind("plot(close[12])\n").meta.max_bars_back, 12);
    assert_eq!(
        ind("plot(ta.sma(close, 50))\nplot(close[12])\n")
            .meta
            .max_bars_back,
        50
    );
    let mut o = opts();
    o.limits.max_bars_back = 300;
    let p = prog_with(
        "//@version=6\nindicator(\"t\")\nk = bar_index % 4\nplot(close[k])\n",
        &o,
    );
    assert_eq!(p.meta.max_bars_back, 300, "a dynamic read uses the cap");
}

#[test]
fn meta_warmup_bars_is_the_longest_path() {
    assert_eq!(ind("plot(close)\n").meta.warmup_bars, 0);
    assert_eq!(ind("plot(ta.sma(close, 10))\n").meta.warmup_bars, 10);
    assert_eq!(
        ind("plot(ta.sma(close, 10))\nplot(ta.sma(close, 30))\n")
            .meta
            .warmup_bars,
        30
    );
}

#[test]
fn meta_insn_budget_is_the_manifest_value() {
    let mut o = opts();
    o.limits.insn_budget = 123_456;
    let p = prog_with("//@version=6\nindicator(\"t\")\nplot(close)\n", &o);
    assert_eq!(p.meta.insn_budget_per_eval, 123_456);
}

#[test]
fn meta_max_intents_is_the_static_bound() {
    assert_eq!(strat("plot(close)\n").meta.max_intents_per_eval, 0);
    let p =
        strat("strategy.entry(\"L\", strategy.long)\nif close > open\n    strategy.close(\"L\")\n");
    assert_eq!(p.meta.max_intents_per_eval, 2);
    assert_eq!(p.layout.intent_cap, 2);
}

#[test]
fn meta_intents_in_dynamic_loop_are_refused() {
    let src = "//@version=6\nstrategy(\"s\")\nn = bar_index % 3\nfor i = 0 to n\n    \
               strategy.entry(\"L\", strategy.long)\n";
    assert_eq!(codes(&compile_src(src, &opts())), vec![3021]);
}

#[test]
fn meta_intents_above_cap_are_refused() {
    let mut o = opts();
    o.limits.intent_cap = 1;
    let src = "//@version=6\nstrategy(\"s\")\nstrategy.entry(\"L\", strategy.long)\n\
               strategy.entry(\"S\", strategy.short)\n";
    assert_eq!(codes(&compile_src(src, &o)), vec![3021]);
}

#[test]
fn meta_uses_intrabar_for_varip_barstate_and_declaration_flags() {
    assert!(!ind("plot(close)\n").meta.uses_intrabar);
    assert!(ind("varip int n = 0\nn += 1\nplot(n)\n").meta.uses_intrabar);
    assert!(
        ind("plot(barstate.isconfirmed ? 1 : 0)\n")
            .meta
            .uses_intrabar
    );
    assert!(
        ind("plot(barstate.isrealtime ? 1 : 0)\n")
            .meta
            .uses_intrabar
    );
    let mut o = default_opts(Dialect::V6, ExecMode::Realtime);
    o.limits.intent_cap = 4;
    let p = prog_with(
        "//@version=6\nstrategy(\"s\", calc_on_every_tick = true)\nplot(close)\n",
        &o,
    );
    assert!(p.meta.uses_intrabar);
}

#[test]
fn meta_nonstandard_chart_is_false_on_ok_programs() {
    assert!(!ind("plot(close)\n").meta.nonstandard_chart);
    assert!(!strat("plot(ta.sma(close, 5))\n").meta.nonstandard_chart);
}

/// The root's `chart` is `opts.chart`; a child's is §4.11.8's — the target's hash and timeframe,
/// the parent's numbers for its own symbol, zeros for another.
#[test]
fn meta_chart_is_the_opts_chart_and_the_child_chart() {
    let o = opts();
    let p = prog_with(
        "//@version=6\nindicator(\"t\")\n\
         plot(request.security(syminfo.tickerid, \"D\", close[1]))\n\
         plot(request.security(\"BINANCE:ETHUSDT.P\", \"240\", close[1]))\n",
        &o,
    );
    assert_eq!(p.meta.chart, o.chart);
    assert_eq!(p.subprograms.len(), 2);
    let own = p.subprograms[0].program.meta.chart;
    assert_eq!(
        own,
        pine_ir::ChartInfo {
            timeframe_s: 86_400,
            ..o.chart
        }
    );
    let eth = "binance_usdm:ethusdt";
    assert_eq!(
        p.subprograms[1].program.meta.chart,
        pine_ir::ChartInfo {
            symbol_hash: pine_ir::symbol_hash(eth.as_bytes()),
            timeframe_s: 14_400,
            ..pine_ir::ChartInfo::default()
        }
    );
}

/// §4.16's sub-program column: `[0; 32]` source digest, the spec digest, `Indicator`, the
/// parent's budget, no intents, no intrabar use, a standard chart, the argument-less
/// `strategy()` fold, no inputs and `n_out` `SubOut` plots — under a strategy parent that has
/// inputs, `varip` and intents of its own.
#[test]
fn meta_subprogram_fields_follow_the_child_column() {
    let o = opts();
    let p = prog_with(
        "//@version=6\nstrategy(\"s\", pyramiding = 3)\nlen = input.int(5, \"L\")\n\
         varip int ticks = 0\nticks += 1\n\
         [a, b] = request.security(syminfo.tickerid, \"D\", [ta.sma(close, len)[1], open[1]])\n\
         if a > b\n    strategy.entry(\"L\", strategy.long)\nplot(a + ticks)\n",
        &o,
    );
    assert!(p.meta.uses_intrabar && p.meta.max_intents_per_eval > 0);
    assert!(!p.meta.inputs.is_empty());
    let c = &p.subprograms[0].program.meta;
    assert_eq!(c.source_sha256, [0; 32]);
    assert_eq!(c.spec_sha256, pine_spec::SPEC_SHA256);
    assert_eq!(c.kind, ScriptKind::Indicator);
    assert_eq!(c.dialect, Dialect::V6);
    assert_eq!(c.insn_budget_per_eval, o.limits.insn_budget);
    assert_eq!(c.max_intents_per_eval, 0);
    assert!(!c.uses_intrabar && !c.nonstandard_chart && !c.uses_lookahead);
    assert_eq!(
        c.decl,
        ind("plot(close)\n").meta.decl,
        "the argument-less fold"
    );
    assert!(c.inputs.is_empty());
    assert_eq!(c.plots.len(), 2);
    assert!(c.plots.iter().all(|x| x.kind == PlotKind::SubOut));
    assert!(c.entry_dirs.is_empty());
}

/// `uses_lookahead`: any sub-program of the tree, at any depth, with `lookahead`.
#[test]
fn meta_uses_lookahead_is_recursive() {
    let la = "barmerge.lookahead_on";
    let nested = format!(
        "plot(request.security(syminfo.tickerid, \"D\", \
         request.security(syminfo.tickerid, \"240\", close[1], lookahead = {la})))\n"
    );
    let p = ind(&nested);
    assert!(!p.subprograms[0].lookahead, "the outer request has none");
    assert!(p.subprograms[0].program.meta.uses_lookahead);
    assert!(p.meta.uses_lookahead, "through the child");
    let direct =
        format!("plot(request.security(syminfo.tickerid, \"D\", close[1], lookahead = {la}))\n");
    assert!(ind(&direct).meta.uses_lookahead);
    let none = "plot(request.security(syminfo.tickerid, \"D\", close[1]))\n";
    assert!(!ind(none).meta.uses_lookahead);
    assert!(!ind("plot(close)\n").meta.uses_lookahead);
}

#[test]
fn meta_every_sym_resolves_in_the_program_table() {
    let p = ind("plot(close, title = \"a\")\nplot(open, title = \"b\")\n");
    for pl in &p.meta.plots {
        if pl.title != pine_ir::Sym::NONE {
            assert!((pl.title.0 as usize) < p.syms.len());
            let t = p.syms.resolve(pl.title);
            assert!(t == "a" || t == "b", "{t}");
        }
    }
}

#[test]
fn meta_plots_index_equals_position() {
    let p = ind("hline(1.0)\nplot(close)\nplotcandle(open, high, low, close)\n");
    for (i, pl) in p.meta.plots.iter().enumerate() {
        assert_eq!(pl.index as usize, i);
    }
    assert_eq!(p.meta.plots.len(), 6);
    assert_eq!(p.meta.plots[5].kind, PlotKind::Hline, "hlines last");
    assert_eq!(p.layout.n_plots, 6);
}

#[test]
fn meta_entry_dirs_merge_by_id() {
    let p = strat(
        "strategy.entry(\"L\", strategy.long)\nif close > open\n    \
         strategy.entry(\"L\", strategy.short)\n",
    );
    assert_eq!(p.meta.entry_dirs.len(), 1);
    assert_eq!(
        p.meta.entry_dirs[0].dirs,
        pine_ir::entry_dir::LONG | pine_ir::entry_dir::SHORT
    );
}
