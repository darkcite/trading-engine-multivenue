// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.1 `tests/meta.rs`: one test per `ProgramMeta` field (I14, I23, I24). The
//! `inputs` and `decl` fields have their own batteries (`meta_inputs.rs`, `decl.rs`); the
//! sub-program fields (`uses_lookahead`, children's `source_sha256` / `chart`) wait for T21–T23.

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
    let p = prog_with("//@version=6\nindicator(\"t\")\nplot(close)\n", &o);
    assert_eq!(p.meta.source_sha256, [7; 32]);
    assert!(
        p.subprograms.is_empty(),
        "no request.* yet: no child to check (T21–T23)"
    );
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

#[test]
fn meta_chart_is_the_opts_chart() {
    let o = opts();
    let p = prog_with("//@version=6\nindicator(\"t\")\nplot(close)\n", &o);
    assert_eq!(p.meta.chart, o.chart);
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
