// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.5 `tests/series.rs`: rings, depths, caps, flags and warm-up (§4.10, §4.18).

mod common;

use common::{codes, compile_src, default_opts};
use pine_ir::{op, ring_flag, CompileOpts, Dialect, ExecMode, PineProgram};

fn opts(d: Dialect) -> CompileOpts {
    default_opts(d, ExecMode::BarClose)
}

fn prog(d: Dialect, body: &str, o: &CompileOpts) -> PineProgram {
    let src = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
    match compile_src(&src, o) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}\n{src}"),
    }
}

fn v6(body: &str) -> PineProgram {
    prog(Dialect::V6, body, &opts(Dialect::V6))
}

/// The capacities of every ring, ascending.
fn caps(p: &PineProgram) -> Vec<u32> {
    let mut v: Vec<u32> = p.layout.rings.iter().map(|r| r.mask + 1).collect();
    v.sort_unstable();
    v
}

fn refused(d: Dialect, body: &str, o: &CompileOpts) -> Vec<u16> {
    let src = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
    codes(&compile_src(&src, o))
}

#[test]
fn series_const_index_sets_depth_k_plus_one() {
    assert_eq!(caps(&v6("x = close * 2\nplot(x[3])\n")), vec![4]);
}

#[test]
fn series_max_over_uses_wins() {
    assert_eq!(
        caps(&v6("x = close * 2\nplot(x[1])\nplot(x[5])\n")),
        vec![8]
    );
}

#[test]
fn series_depth_does_not_propagate_transitively() {
    // `y` is read at depth 3, `x` (a fresh value each bar) at depth 2: 4 and 4, never 8.
    let p = v6("y = close * 2\nx = y[3]\nplot(x[2])\n");
    assert_eq!(caps(&p), vec![4, 4]);
}

#[test]
fn series_capacity_is_next_power_of_two_min_two() {
    assert_eq!(caps(&v6("x = close * 2\nplot(x[1])\n")), vec![2]);
    assert_eq!(caps(&v6("x = close * 2\nplot(x[2])\n")), vec![4]);
}

#[test]
fn series_ring_flags_follow_type_and_dialect() {
    let flags = |d: Dialect, decl: &str| {
        let p = prog(d, &format!("{decl}\nplot(close)\n"), &opts(d));
        assert_eq!(p.layout.rings.len(), 1, "{decl}");
        p.layout.rings[0].flags
    };
    assert_eq!(flags(Dialect::V6, "x = close * 2\ny = x[1]"), 0, "float");
    // CAR-R33-5: an `int` is a binary64 in the f64 lane — its ring tracks `na` as NaN.
    let int = flags(Dialect::V6, "x = bar_index > 2 ? bar_index : na\ny = x[1]");
    assert_eq!(int, 0, "na-capable int: an f64 ring");
    let b6 = flags(Dialect::V6, "x = close > open\ny = x[1]");
    assert_eq!(b6, ring_flag::INIT_FALSE, "v6 bool");
    let b5 = flags(Dialect::V5, "x = close > open\ny = x[1]");
    assert_ne!(b5 & ring_flag::NA_TRACKED, 0, "v5 bool");
    assert_eq!(b5 & ring_flag::INIT_FALSE, 0, "v5 bool");
    let s = flags(
        Dialect::V6,
        "string x = close > open ? \"u\" : \"d\"\ny = x[1] == \"u\"",
    );
    assert_eq!(s, ring_flag::STRING | ring_flag::NA_TRACKED, "string");
}

#[test]
fn series_depth_one_slot_gets_no_ring() {
    assert!(v6("x = close * 2\nplot(x[0])\n").layout.rings.is_empty());
}

#[test]
fn series_var_declaration_needs_no_ring() {
    assert!(v6("var float a = 0.0\na := a + close\nplot(a)\n")
        .layout
        .rings
        .is_empty());
}

#[test]
fn series_dynamic_index_uses_cap() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 100;
    let p = prog(Dialect::V6, "k = bar_index % 3\nplot(close[k])\n", &o);
    assert_eq!(caps(&p), vec![128]);
    assert!(p
        .code
        .iter()
        .any(|i| i.op == op::HIST_F && i.flags & pine_ir::flg::K_DYNAMIC != 0));
}

#[test]
fn series_dynamic_index_without_cap_is_unbounded_lookback() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 0;
    assert_eq!(
        refused(Dialect::V6, "k = bar_index % 3\nplot(close[k])\n", &o),
        vec![3016]
    );
}

#[test]
fn series_const_index_beyond_cap_is_lookback_exceeds_cap() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 5000;
    assert_eq!(refused(Dialect::V6, "plot(close[6000])\n", &o), vec![3017]);
}

#[test]
fn series_script_declaration_above_manifest_is_refused() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 5000;
    let src = "//@version=6\nstrategy(\"s\", max_bars_back = 6000)\nplot(close)\n";
    assert_eq!(codes(&compile_src(src, &o)), vec![3017]);
}

#[test]
fn series_script_declaration_is_the_cap_without_manifest() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 0;
    let src =
        "//@version=6\nindicator(\"t\", max_bars_back = 300)\nk = bar_index % 3\nplot(close[k])\n";
    let p = compile_src(src, &o).expect("compiles");
    assert_eq!(caps(&p), vec![512]);
}

#[test]
fn series_manifest_cap_above_hard_limit_is_refused() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 100_001;
    assert_eq!(refused(Dialect::V6, "plot(close)\n", &o), vec![3017]);
}

/// CAR-R33-2 (`pin:03-hist-dynamic-offset`, TradingView F088): an `na` dynamic offset reads the
/// current bar — `NZ_I kk, k, 0` before the read — and the read is guarded by a
/// `runtime.error` call (F087 / F089: a negative offset, or one beyond the buffer, faults).
#[test]
fn series_na_dynamic_offset_reads_the_current_bar() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 10;
    let p = prog(
        Dialect::V6,
        "k = bar_index > 2 ? 1 : na\nplot(close[k])\n",
        &o,
    );
    let h = p
        .code
        .iter()
        .position(|i| i.op == op::HIST_F)
        .expect("HIST_F");
    assert_ne!(p.code[h].flags & pine_ir::flg::K_DYNAMIC, 0);
    let nz = p.code[..h]
        .iter()
        .find(|i| i.op == op::NZ_I && i.dst == p.code[h].b)
        .unwrap_or_else(|| panic!("{:?}", p.code));
    // The replacement is the constant 0 (the init block's `I_CONST`).
    let zero = p
        .code
        .iter()
        .find(|i| i.op == op::I_CONST && i.dst == nz.b)
        .expect("I_CONST");
    assert_eq!(p.consts[zero.a as usize], 0);
    assert!(
        !p.code.iter().any(|i| i.op == op::NZ_I
            && p.code.iter().any(|c| c.op == op::I_CONST
                && c.dst == i.b
                && p.consts[c.a as usize] == u64::MAX)),
        "no `NZ_I kk, k, −1` is left"
    );
    let rt = pine_spec::lookup("runtime.error")
        .expect("runtime.error")
        .ov_lo;
    assert!(
        p.code[..h]
            .iter()
            .any(|i| i.op == op::CALL_BUILTIN && u32::from(i.a) == u32::from(rt)),
        "the guard's runtime.error"
    );
}

#[test]
fn series_meta_max_bars_back_is_the_global_max() {
    let p = v6("x = close * 2\nplot(x[7])\nplot(ta.sma(close, 20))\nplot(close[3])\n");
    assert_eq!(p.meta.max_bars_back, 20);
    let q = v6("x = close * 2\nplot(x[40])\nplot(ta.sma(close, 20))\n");
    assert_eq!(q.meta.max_bars_back, 40);
}

#[test]
fn series_string_history_uses_a_string_ring() {
    let p = v6("string s = close > open ? \"u\" : \"d\"\nplot(s[1] == \"u\" ? 1 : 0)\n");
    let r = p
        .layout
        .rings
        .iter()
        .find(|r| r.flags & ring_flag::STRING != 0)
        .expect("a STRING ring");
    assert!(r.cells_off >= p.layout.str_persist_off);
    let end = u64::from(r.cells_off) + u64::from(r.mask + 1) * u64::from(p.layout.str_cell_bytes);
    assert!(end <= u64::from(p.layout.str_persist_off + p.layout.str_persist_bytes));
}

#[test]
fn series_warmup_is_the_longest_path() {
    let p = v6("plot(ta.sma(ta.sma(close, 10), 20)[3])\n");
    assert_eq!(p.meta.warmup_bars, 33);
}

#[test]
fn series_warmup_recursive_uses_the_factor() {
    let p = v6("plot(ta.ema(close, 10))\n");
    assert_eq!(p.meta.warmup_bars, 140);
}

#[test]
fn series_max_bars_back_function_sets_a_variable_cap() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 0;
    let p = prog(
        Dialect::V6,
        "x = close * 2\nmax_bars_back(x, 50)\nk = bar_index % 3\nplot(x[k])\n",
        &o,
    );
    assert_eq!(caps(&p), vec![64]);
}

/// §4.10.4: `max_bars_back(x, n)` above `MAX_BARS_BACK_HARD`, or above a manifest `M > 0`, is
/// `3017`.
#[test]
fn series_max_bars_back_function_above_the_caps_is_refused() {
    let mut o = opts(Dialect::V6);
    o.limits.max_bars_back = 5000;
    let body = "x = close * 2\nmax_bars_back(x, 6000)\nk = bar_index % 3\nplot(x[k])\n";
    assert_eq!(refused(Dialect::V6, body, &o), vec![3017]);
    o.limits.max_bars_back = 0;
    let body = "x = close * 2\nmax_bars_back(x, 100001)\nk = bar_index % 3\nplot(x[k])\n";
    assert_eq!(refused(Dialect::V6, body, &o), vec![3017]);
}
