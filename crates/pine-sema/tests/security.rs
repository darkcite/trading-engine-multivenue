// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.9 `tests/security.rs` — the T21 rows (targets and parameters: `3027`, `3042`,
//! `5001`, `3053`, `3044`, `3024`, `3032`). The cone, keys, dedup and depth rows are T22, the
//! leak rows T23.

mod common;

use common::{codes, compile_src, default_opts, NOT_IMPLEMENTED};
use pine_ir::{CompileOpts, Dialect, ExecMode};

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
