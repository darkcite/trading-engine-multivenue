// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.9 `tests/security.rs` — the T21 rows (targets and parameters: `3027`, `3042`,
//! `5001`, `3053`, `3044`, `3024`, `3032`), the T22 rows (the cone, the child IR and options,
//! keys, dedup, order, depth and counts: `3028`–`3031`) and the T23 rows (leak-freedom, `3043`).

mod common;

use common::{codes, compile_src, default_opts, NOT_IMPLEMENTED};
use pine_ir::{op, CompileOpts, Dialect, ExecMode, PineProgram};

fn opts() -> CompileOpts {
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.ticker_map = vec![(
        "BINANCE:ETHUSDT.P".to_owned(),
        "binance_usdm:ethusdt".to_owned(),
    )];
    o
}

fn run_with(body: &str, o: &CompileOpts) -> Vec<u16> {
    let r = compile_src(&format!("//@version=6\nindicator(\"t\")\n{body}"), o);
    if let Err(ds) = &r {
        for d in ds {
            eprintln!("{} {}", d.code.0, d.msg);
        }
    }
    codes(&r)
}

fn run(body: &str) -> Vec<u16> {
    run_with(body, &opts())
}

/// Accepted by pass 3: the program then waits for T22 (`not implemented`).
fn accepted(v: &[u16]) -> bool {
    v == [NOT_IMPLEMENTED] || v == [common::ACCEPTED]
}

#[test]
fn security_dynamic_symbol_is_refused() {
    let s = "t = close > open ? \"A\" : \"B\"\nplot(request.security(t, \"D\", close))\n";
    assert_eq!(run(s), vec![3027]);
}

#[test]
fn security_dynamic_timeframe_is_refused() {
    let s =
        "f = close > open ? \"D\" : \"W\"\nplot(request.security(syminfo.tickerid, f, close))\n";
    assert_eq!(run(s), vec![3027]);
}

#[test]
fn security_chart_symbol_forms_are_accepted() {
    for sym in ["\"\"", "syminfo.tickerid"] {
        let v = run(&format!("plot(request.security({sym}, \"D\", close))\n"));
        assert!(accepted(&v), "{sym}: {v:?}");
    }
    let v = run("plot(request.security(syminfo.tickerid, timeframe.period, close))\n");
    assert!(accepted(&v), "{v:?}");
}

#[test]
fn security_mapped_ticker_is_accepted() {
    let v = run("plot(request.security(\"BINANCE:ETHUSDT.P\", \"60\", close))\n");
    assert!(accepted(&v), "{v:?}");
}

#[test]
fn security_unmapped_ticker_is_refused() {
    for ign in ["true", "false"] {
        let s = format!(
            "plot(request.security(\"NASDAQ:AAPL\", \"D\", close, ignore_invalid_symbol = {ign}))\n"
        );
        assert_eq!(run(&s), vec![3042], "{ign}");
    }
}

#[test]
fn security_timeframe_off_the_bar_clock_is_5001() {
    for tf in ["45", "2D", "5S", "3M"] {
        let s = format!("plot(request.security(syminfo.tickerid, \"{tf}\", close))\n");
        assert_eq!(run(&s), vec![5001], "{tf}");
    }
}

#[test]
fn security_malformed_timeframe_is_5001() {
    for tf in ["1H", "H"] {
        let s = format!("plot(request.security(syminfo.tickerid, \"{tf}\", close))\n");
        assert_eq!(run(&s), vec![5001], "{tf}");
    }
}

#[test]
fn security_string_output_is_refused() {
    let s = "x = request.security(syminfo.tickerid, \"D\", syminfo.ticker)\nplot(close)\n";
    assert_eq!(run(s), vec![3044]);
}

#[test]
fn security_reference_output_is_refused() {
    // An object (the `array` form waits for pine-std's served `array.*` rows).
    let s = "type P\n    float v = 0.0\n\
             x = request.security(syminfo.tickerid, \"D\", P.new(close))\nplot(close)\n";
    assert_eq!(run(s), vec![3024]);
}

#[test]
fn security_currency_conversion_is_refused() {
    let s = "plot(request.security(syminfo.tickerid, \"D\", close, currency = currency.EUR))\n";
    assert_eq!(run(s), vec![3053]);
}

/// impl-06 D062 (impl-02 §3.2 row 53, CAR-02-1): `request.currency_rate` of two constant,
/// byte-different currencies is `3053` in either dialect; byte-equal ones are pine-std's `1.0`.
#[test]
fn security_currency_rate_of_two_constants_is_refused() {
    let d062 = "h_D062 = request.currency_rate(currency.USD, currency.EUR)\n\
                plot(na(h_D062) ? 1 : 0, \"f_D062_isna\", display = display.data_window)\n\
                plot(na(h_D062) ? na : h_D062, \"f_D062_val\", display = display.data_window)\n";
    for d in [Dialect::V5, Dialect::V6] {
        let r = compile_src(
            &format!("//@version={}\nindicator(\"D062\")\n{d062}", d as u8),
            &default_opts(d, ExecMode::BarClose),
        );
        if let Err(ds) = &r {
            for x in ds {
                eprintln!("v{}: {} {}", d as u8, x.code.0, x.msg);
            }
        }
        assert_eq!(codes(&r), vec![3053], "v{}", d as u8);
    }
    assert_eq!(
        run("plot(request.currency_rate(\"USD\", currency.EUR, true))\n"),
        vec![3053]
    );
    // §4.11: otherwise an ordinary call, emitted and verified (byte-equal constants, a dynamic
    // argument).
    let same = "plot(request.currency_rate(currency.USD, \"USD\"))\n";
    assert_eq!(run(same), vec![common::ACCEPTED]);
    let dynamic = "c = close > open ? \"USD\" : \"EUR\"\nplot(request.currency_rate(c, \"USD\"))\n";
    assert_eq!(run(dynamic), vec![common::ACCEPTED]);
}

#[test]
fn security_calc_bars_count_is_refused() {
    let s = "plot(request.security(syminfo.tickerid, \"D\", close, calc_bars_count = 50))\n";
    assert_eq!(run(s), vec![3024]);
}

#[test]
fn security_tuple_elements_over_127_are_refused() {
    // 64 + 64 elements: 128 > 127.
    let items: Vec<String> = (0..64).map(|i| format!("close + {i}")).collect();
    let tup = items.join(", ");
    let names: Vec<String> = (0..64).map(|i| format!("a{i}")).collect();
    let names2: Vec<String> = (0..64).map(|i| format!("b{i}")).collect();
    let s = format!(
        "[{}] = request.security(syminfo.tickerid, \"D\", [{tup}])\n\
         [{}] = request.security(syminfo.tickerid, \"W\", [{tup}])\nplot(close)\n",
        names.join(", "),
        names2.join(", ")
    );
    let v = run(&s);
    assert!(v.contains(&3032), "{v:?}");
}

#[test]
fn security_currency_none_is_accepted() {
    let s = "plot(request.security(syminfo.tickerid, \"D\", close, currency = currency.NONE))\n";
    let v = run(s);
    assert!(accepted(&v), "{v:?}");
}

// ---- T22: the cone, the child, keys, dedup, order, depth ----------------------------------

fn prog_with(body: &str, o: &CompileOpts) -> PineProgram {
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    match compile_src(&src, o) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}\n{src}"),
    }
}

fn prog(body: &str) -> PineProgram {
    prog_with(body, &opts())
}

fn calls_of(p: &PineProgram, ov: pine_ir::OverloadId) -> usize {
    p.code
        .iter()
        .filter(|i| i.op == op::CALL_BUILTIN && i.a == ov.0)
        .count()
}

fn verify_tree(p: &PineProgram) {
    if let Err(e) = pine_vm::verify(p, &pine_std::BUILTINS) {
        panic!("rule {} pc {} detail {}", e.rule, e.pc, e.detail);
    }
    for s in &p.subprograms {
        verify_tree(&s.program);
    }
}

#[test]
fn security_expression_becomes_a_child_program() {
    let p = prog("plot(request.security(syminfo.tickerid, \"D\", close))\n");
    assert_eq!(p.subprograms.len(), 1);
    let s = &p.subprograms[0];
    assert_eq!(s.symbol_key, common::CHART_KEY);
    assert_eq!(
        s.symbol_hash,
        pine_ir::symbol_hash(common::CHART_KEY.as_bytes())
    );
    assert_eq!(
        (s.timeframe_s, s.n_out, s.lookahead, s.gaps),
        (86_400, 1, false, false)
    );
    assert_eq!(s.program.code.last().map(|i| i.op), Some(op::HALT));
    assert_eq!(s.program.meta.plots.len(), 1);
    assert_eq!(s.program.meta.plots[0].kind, pine_ir::PlotKind::SubOut);
    verify_tree(&p);
    assert_eq!(calls_of(&p, pine_spec::ov::REQUEST_SECURITY), 1);
}

#[test]
fn security_child_has_its_own_site_space() {
    let p = prog(
        "a = ta.ema(close, 5)\nm = ta.sma(close, 20)\nplot(a)\n\
                  plot(request.security(syminfo.tickerid, \"D\", m))\n",
    );
    let c = &p.subprograms[0].program;
    assert_eq!(c.layout.states.len(), 1, "{:?}", c.layout.states);
    assert_eq!(c.layout.states[0].site, pine_ir::SiteId(0));
}

#[test]
fn security_cone_pulls_in_transitive_definitions() {
    let p = prog("m = ta.sma(close, 20)\nplot(request.security(syminfo.tickerid, \"D\", m))\n");
    let c = &p.subprograms[0].program;
    assert_eq!(calls_of(c, pine_spec::ov::TA_SMA), 1);
    // The parent keeps its own (its value feeds nothing the parent plots, but it is a call).
    assert_eq!(calls_of(&p, pine_spec::ov::TA_SMA), 1);
}

#[test]
fn security_cone_includes_var_carry_definitions() {
    let with = prog(
        "var float acc = 0.0\nh = request.security(syminfo.tickerid, \"D\", acc)\n\
                     acc += close\nplot(h)\n",
    );
    let c = &with.subprograms[0].program;
    assert!(c.code.iter().any(|i| i.op == op::F_ADD), "{:?}", c.code);
}

#[test]
fn security_cone_stops_at_bar_series() {
    let p = prog("plot(request.security(syminfo.tickerid, \"D\", close))\n");
    let c = &p.subprograms[0].program;
    // Only the capture: the child reads its own `close` slot.
    assert!(c.layout.states.is_empty());
    assert_eq!(
        c.code.iter().filter(|i| i.op == op::CALL_BUILTIN).count(),
        1,
        "{:?}",
        c.code
    );
}

#[test]
fn security_cone_excludes_the_request_site_condition() {
    let plain = prog("h = request.security(syminfo.tickerid, \"D\", close)\nplot(h)\n");
    let cond = prog(
        "c = close > open\nfloat h = na\nif c\n    \
                     h := request.security(syminfo.tickerid, \"D\", close)\nplot(h)\n",
    );
    assert_eq!(plain.subprograms[0].key, cond.subprograms[0].key);
    let c = &cond.subprograms[0].program;
    assert!(!c.code.iter().any(|i| i.op == op::F_GT), "{:?}", c.code);
}

#[test]
fn security_parent_history_of_result_is_a_parent_ring() {
    let p = prog("h = request.security(syminfo.tickerid, \"D\", close)\nplot(h[1])\n");
    assert!(!p.layout.rings.is_empty());
    assert!(p.subprograms[0].program.layout.rings.is_empty());
}

#[test]
fn security_tuple_request_is_one_call_per_element() {
    let p = prog(
        "[a, b] = request.security(syminfo.tickerid, \"D\", [close, open])\n\
                  plot(a)\nplot(b)\n",
    );
    assert_eq!(calls_of(&p, pine_spec::ov::REQUEST_SECURITY), 2);
    assert_eq!(p.subprograms.len(), 1);
    assert_eq!(p.subprograms[0].n_out, 2);
    assert_eq!(p.subprograms[0].program.meta.plots.len(), 2);
}

#[test]
fn security_identical_requests_share_one_entry() {
    let p = prog(
        "a = request.security(syminfo.tickerid, \"D\", ta.sma(close, 5))\n\
                  b = request.security(syminfo.tickerid, \"D\", ta.sma(close, 5))\nplot(a + b)\n",
    );
    assert_eq!(calls_of(&p, pine_spec::ov::REQUEST_SECURITY), 2);
    assert_eq!(p.subprograms.len(), 1);
}

#[test]
fn security_subprograms_in_first_reference_order() {
    let p = prog(
        "a = request.security(syminfo.tickerid, \"W\", close)\n\
                  b = request.security(syminfo.tickerid, \"D\", close)\n\
                  c = request.security(syminfo.tickerid, \"W\", close)\nplot(a + b + c)\n",
    );
    let tfs: Vec<u32> = p.subprograms.iter().map(|s| s.timeframe_s).collect();
    assert_eq!(tfs, vec![604_800, 86_400]);
}

#[test]
fn security_nested_request_lives_in_the_child() {
    let p = prog(
        "plot(request.security(syminfo.tickerid, \"D\", \
                  request.security(syminfo.tickerid, \"W\", close)))\n",
    );
    assert_eq!(p.subprograms.len(), 1);
    assert_eq!(p.subprograms[0].timeframe_s, 86_400);
    let c = &p.subprograms[0].program;
    assert_eq!(c.subprograms.len(), 1);
    assert_eq!(c.subprograms[0].timeframe_s, 604_800);
    verify_tree(&p);
}

#[test]
fn security_depth_above_three_is_refused() {
    let nest = |n: usize| {
        let mut e = "close".to_owned();
        for _ in 0..n {
            e = format!("request.security(syminfo.tickerid, \"D\", {e})");
        }
        format!("plot({e})\n")
    };
    assert_eq!(run(&nest(3)), vec![common::ACCEPTED]);
    assert_eq!(run(&nest(4)), vec![3031]);
}

#[test]
fn security_more_than_64_subprograms_is_refused() {
    let req = |n: usize| {
        let mut s = String::new();
        for i in 0..n {
            s += &format!("x{i} = request.security(syminfo.tickerid, \"D\", close + {i})\n");
        }
        let sum: Vec<String> = (0..n).map(|i| format!("x{i}")).collect();
        s + &format!("plot({})\n", sum.join(" + "))
    };
    assert_eq!(run(&req(64)), vec![common::ACCEPTED]);
    let r = compile_src(
        &format!("//@version=6\nindicator(\"t\")\n{}", req(65)),
        &opts(),
    );
    let ds = common::diags(r);
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].code.0, 3031);
    assert!(ds[0].msg.contains("65"), "{}", ds[0].msg);
}

#[test]
fn security_self_reference_is_a_cycle() {
    let s =
        "var float x = 0.0\nx := request.security(syminfo.tickerid, \"D\", x[1] + 1)\nplot(x)\n";
    assert_eq!(run(s), vec![3030]);
}

#[test]
fn security_capture_in_expression_is_refused() {
    let s = "f() =>\n    alert(\"x\")\n    close\nplot(request.security(syminfo.tickerid, \"D\", f()))\n";
    assert_eq!(run(s), vec![3028]);
}

#[test]
fn security_barstate_in_cone_is_refused() {
    let s = "plot(request.security(syminfo.tickerid, \"D\", barstate.isconfirmed ? 1.0 : 0.0))\n";
    assert_eq!(run(s), vec![3029]);
}

#[test]
fn security_varip_in_cone_is_refused() {
    let s = "varip float v = 0.0\nv += 1\nplot(request.security(syminfo.tickerid, \"D\", v))\n";
    assert_eq!(run(s), vec![3029]);
}

#[test]
fn security_strategy_value_in_cone_is_refused() {
    let src = "//@version=6\nstrategy(\"t\")\n\
               plot(request.security(syminfo.tickerid, \"D\", strategy.position_size))\n";
    assert_eq!(codes(&compile_src(src, &opts())), vec![3029]);
}

#[test]
fn security_loop_variable_in_cone_is_refused() {
    let s = "s = 0.0\nfor i = 0 to 2\n    s += request.security(syminfo.tickerid, \"D\", close + i)\nplot(s)\n";
    assert_eq!(run(s), vec![3029]);
}

#[test]
fn security_child_mode_follows_parent_and_lookahead() {
    for (mode, look, want) in [
        (ExecMode::BarClose, false, ExecMode::BarClose),
        (ExecMode::BarClose, true, ExecMode::Realtime),
        (ExecMode::Realtime, false, ExecMode::Realtime),
        (ExecMode::Realtime, true, ExecMode::Realtime),
    ] {
        let mut o = default_opts(Dialect::V6, mode);
        o.live_bound = false;
        let la = if look {
            "barmerge.lookahead_on"
        } else {
            "barmerge.lookahead_off"
        };
        let p = prog_with(
            &format!("plot(request.security(syminfo.tickerid, \"D\", close, lookahead = {la}))\n"),
            &o,
        );
        assert_eq!(p.subprograms[0].program.mode, want, "{mode:?} {look}");
        assert_eq!(p.subprograms[0].lookahead, look);
    }
}

#[test]
fn security_child_history_htf_is_two_ltf_is_ratio_plus_one() {
    let d = prog("plot(request.security(syminfo.tickerid, \"D\", close))\n");
    assert_eq!(d.subprograms[0].history, 2);
    let l = prog("plot(request.security(syminfo.tickerid, \"5\", close))\n");
    assert_eq!(l.subprograms[0].history, 13);
}

#[test]
fn security_child_chart_for_other_symbol_is_zeroed() {
    let p = prog("plot(request.security(\"BINANCE:ETHUSDT.P\", \"60\", close))\n");
    let s = &p.subprograms[0];
    assert_eq!(s.symbol_key, "binance_usdm:ethusdt");
    let ch = s.program.meta.chart;
    assert_eq!(
        ch.symbol_hash,
        pine_ir::symbol_hash(b"binance_usdm:ethusdt")
    );
    assert_eq!(ch.timeframe_s, 3600);
    assert_eq!((ch.mintick_1e6, ch.pointvalue_1e6, ch.venue), (0, 0, 0));
    let own = prog("plot(request.security(syminfo.tickerid, \"D\", close))\n");
    assert_eq!(
        own.subprograms[0].program.meta.chart.mintick_1e6,
        opts().chart.mintick_1e6
    );
}

// ---- T23: leak-freedom -------------------------------------------------------------------

fn look(expr: &str, live: bool) -> Vec<u16> {
    let mut o = opts();
    o.live_bound = live;
    run_with(
        &format!(
            "plot(request.security(syminfo.tickerid, \"240\", {expr}, lookahead = barmerge.lookahead_on))\n"
        ),
        &o,
    )
}

#[test]
fn security_lookahead_unoffset_live_bound_is_refused() {
    // impl-06 D051/D052 twins (pine-screen's request included).
    assert_eq!(look("close", true), vec![3043]);
    assert_eq!(look("high - low", true), vec![3043]);
}

#[test]
fn security_lookahead_offset_live_bound_is_admitted() {
    // impl-06 D017/D018 twins.
    for e in ["close[1]", "high[1] - low[1]"] {
        assert_eq!(look(e, true), vec![common::ACCEPTED], "{e}");
    }
    let mut o = opts();
    o.live_bound = true;
    let p = prog_with(
        "plot(request.security(syminfo.tickerid, \"D\", close[1], lookahead = barmerge.lookahead_on))\n",
        &o,
    );
    assert!(p.meta.uses_lookahead);
    assert!(p.subprograms[0].lookahead);
}

#[test]
fn security_lookahead_not_live_bound_compiles() {
    assert_eq!(look("close", false), vec![common::ACCEPTED]);
}

#[test]
fn security_lookahead_through_stateful_call_of_past_inputs_is_admitted() {
    assert_eq!(look("ta.sma(close[1], 5)", true), vec![common::ACCEPTED]);
}

#[test]
fn security_lookahead_of_a_bar_reading_call_is_refused() {
    for e in ["hl2", "ta.atr(14)", "ta.sma(close, 5)"] {
        assert_eq!(look(e, true), vec![3043], "{e}");
    }
}

/// `3029`: a folded `syminfo.*` / `timeframe.*` value in the cone of a request for another
/// symbol / timeframe (§4.11.4; the fold lowers into an `ir.ctx_slots` slot, §4.17).
fn ctx_mix(body: &str) -> bool {
    let r = compile_src(&format!("//@version=6\nindicator(\"t\")\n{body}"), &opts());
    codes(&r) == [3029]
}

#[test]
fn security_syminfo_of_other_symbol_is_refused() {
    let eth = "\"BINANCE:ETHUSDT.P\"";
    let s = format!("plot(request.security({eth}, \"60\", math.round_to_mintick(close)))\n");
    assert_eq!(run(&s), vec![3029]);
    let s = format!("m = math.round_to_mintick(close)\nplot(request.security({eth}, \"60\", m))\n");
    assert_eq!(
        run(&s),
        vec![3029],
        "through a parent variable: the cone, not the expression"
    );
    let s = "plot(request.security(syminfo.tickerid, \"D\", math.round_to_mintick(close)))\n";
    assert_eq!(run(s), vec![common::ACCEPTED], "the chart's own symbol");
    assert!(ctx_mix(&format!(
        "plot(request.security({eth}, \"60\", close * syminfo.mintick))\n"
    )));
    assert!(ctx_mix(
        "plot(request.security(syminfo.tickerid, \"D\", close * timeframe.multiplier))\n"
    ));
    assert!(ctx_mix(&format!(
        "t = syminfo.ticker\nplot(request.security({eth}, \"60\", t == \"BTCUSDT\" ? close : 0))\n"
    )));
    let own = "plot(request.security(syminfo.tickerid, \"60\", close * syminfo.mintick))\n";
    let r = compile_src(&format!("//@version=6\nindicator(\"t\")\n{own}"), &opts());
    assert!(r.is_ok(), "the chart's own symbol and timeframe: {r:?}");
}

/// I19: an admitted `lookahead_on` cone never reads its current bar — run each child on the VM,
/// then again with the last bar perturbed: every output of the last bar is unchanged.
#[test]
fn leak_admitted_cones_ignore_the_current_bar() {
    use pine_ir::{barflag, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    let last = |child: &PineProgram, closes: &[f64]| -> Vec<f64> {
        let v = pine_vm::verify(child, &pine_std::BUILTINS).expect("the child verifies");
        let mut arena = Arena::new(child.layout.total_bytes as usize);
        let mut vm = pine_vm::VmInstance::new(v, arena.slice());
        let mut ctx = Ctx { now: 0 };
        for (i, &c) in closes.iter().enumerate() {
            let o = vm.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx);
            assert_eq!(o, StepOutcome::Ok, "bar {i}: {:?}", vm.last_fault());
        }
        vm.plots()[..child.meta.plots.len()].to_vec()
    };
    let closes: Vec<f64> = (0..20).map(|i| 100.0 + (i * 7 % 5) as f64).collect();
    let mut bumped = closes.clone();
    *bumped.last_mut().unwrap() += 50.0;
    for e in [
        "close[1]",
        "ta.sma(close[1], 5)",
        "[close[1], high[2] - low[1]]",
    ] {
        let mut o = opts();
        o.live_bound = true;
        let body = if e.starts_with('[') {
            format!(
                "[a, b] = request.security(syminfo.tickerid, \"D\", {e}, \
                 lookahead = barmerge.lookahead_on)\nplot(a + b)\n"
            )
        } else {
            format!(
                "plot(request.security(syminfo.tickerid, \"D\", {e}, \
                 lookahead = barmerge.lookahead_on))\n"
            )
        };
        let p = prog_with(&body, &o);
        let child = &p.subprograms[0].program;
        let a = last(child, &closes);
        let b = last(child, &bumped);
        assert!(!a.iter().any(|x| x.is_nan()), "{e}: {a:?}");
        assert_eq!(a, b, "{e}");
    }
    // The control: an unoffset cone (admitted only without a live bound) does move.
    let mut o = opts();
    o.live_bound = false;
    let p = prog_with(
        "plot(request.security(syminfo.tickerid, \"D\", close, lookahead = barmerge.lookahead_on))\n",
        &o,
    );
    let child = &p.subprograms[0].program;
    assert_ne!(last(child, &closes), last(child, &bumped));
}
