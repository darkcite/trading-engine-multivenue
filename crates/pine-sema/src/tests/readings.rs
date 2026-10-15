// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Every switch and pin sema owns, in each reading (impl-02 §4.2.3, I16).

use pine_ir::{
    symbol_hash, ChartInfo, CompileLimits, CompileOpts, Dialect, ExecMode, FileId, SymTable,
};

use crate::pins::{FnGlobalAssign, ForCounterShadow, ShadowBuiltin};
use crate::sw::Readings;
use crate::{compile_with, LibCache};

fn opts(dialect: Dialect) -> CompileOpts {
    let key = "binance_usdm:btcusdt";
    CompileOpts {
        dialect,
        mode: ExecMode::BarClose,
        live_bound: true,
        chart: ChartInfo {
            symbol_hash: symbol_hash(key.as_bytes()),
            timeframe_s: 3600,
            mintick_1e6: 100_000,
            pointvalue_1e6: 1_000_000,
            ..ChartInfo::default()
        },
        symbol_key: key.to_owned(),
        syminfo_strs: Default::default(),
        limits: CompileLimits {
            max_bars_back: 5000,
            insn_budget: 2_000_000,
            ..Default::default()
        },
        inputs: Vec::new(),
        ticker_map: Vec::new(),
        source_sha256: [0; 32],
    }
}

/// The codes of compiling a v5 `body` under `rd`.
fn codes(body: &str, rd: &Readings) -> Vec<u16> {
    let src = format!("//@version=5\nindicator(\"t\")\n{body}");
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    match compile_with(&ast, syms, &opts(Dialect::V5), &mut LibCache::empty(), rd) {
        Ok(_) => Vec::new(),
        Err(ds) => ds.iter().map(|d| d.code.0).collect(),
    }
}

const NOT_IMPLEMENTED: u16 = 3255;

#[test]
fn readings_defaults_are_the_dialect_rows() {
    for d in [Dialect::V5, Dialect::V6] {
        let r = Readings::of(d);
        assert!(r.check().is_ok());
        assert_eq!(r.pins, crate::pins::DEFAULT);
        assert_eq!(r.int_div, pine_spec::sw(pine_spec::Sw::IntDiv, d));
    }
    assert_ne!(
        Readings::of(Dialect::V5).int_div,
        Readings::of(Dialect::V6).int_div,
        "W1"
    );
}

#[test]
fn readings_unknown_switch_code_is_internal() {
    let mut r = Readings::of(Dialect::V5);
    r.round_ties = 7;
    assert_eq!(codes("plot(close)\n", &r), vec![NOT_IMPLEMENTED]);
    r.round_ties = Readings::of(Dialect::V5).round_ties;
    r.float_div_zero = 9;
    assert_eq!(codes("plot(close)\n", &r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn readings_shadow_builtin_both() {
    let src = "nz(float x) => x + 1\nplot(nz(1.0))\n";
    let mut r = Readings::of(Dialect::V5);
    assert_eq!(r.pins.shadow_builtin, ShadowBuiltin::Refuse);
    assert_eq!(codes(src, &r), vec![3004]);
    r.pins.shadow_builtin = ShadowBuiltin::Shadow;
    assert_eq!(codes(src, &r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn readings_fn_global_assign_both() {
    let src = "var g = 0\nf() =>\n    g := g + 1\n    g\nplot(f())\n";
    let mut r = Readings::of(Dialect::V5);
    assert_eq!(r.pins.fn_global_assign, FnGlobalAssign::Refuse);
    assert_eq!(codes(src, &r), vec![3040]);
    r.pins.fn_global_assign = FnGlobalAssign::Allow;
    assert_eq!(codes(src, &r), vec![NOT_IMPLEMENTED]);
}

#[test]
fn readings_for_counter_shadow_both() {
    let src = "s = 0\nfor i = 0 to 2\n    i = 5\n    s += i\nplot(s)\n";
    let mut r = Readings::of(Dialect::V5);
    assert_eq!(r.pins.for_counter_shadow, ForCounterShadow::Refuse);
    assert_eq!(codes(src, &r), vec![3041]);
    r.pins.for_counter_shadow = ForCounterShadow::Shadow;
    assert_eq!(codes(src, &r), vec![NOT_IMPLEMENTED]);
    let src_in = "s = 0\nfor x in array.from(1, 2)\n    x = 5\n    s += x\nplot(s)\n";
    assert_eq!(codes(src_in, &r), vec![NOT_IMPLEMENTED]);
    r.pins.for_counter_shadow = ForCounterShadow::Refuse;
    assert_eq!(codes(src_in, &r), vec![3041]);
}
