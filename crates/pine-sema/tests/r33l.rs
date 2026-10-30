// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33l acceptance: the laws TradingView's captures decided for C032 (a UDT field default that
//! is a constant expression but not a literal), F083 (`pin:02-var-qualifier`) and E089 (I9),
//! asserted against the captured rows bit for bit. Each fixture's script carries its own SYN-1
//! driver, so the bars only number the rows.

mod common;

use common::{codes, compile_src, default_opts};
use pine_ir::{barflag, Dialect, ExecMode, PineProgram, QtyType, StepOutcome};
use pine_vm::testkit::{bar, Arena, Ctx};

/// One captured fixture: its dialects and its rows.
struct Fix {
    src: String,
    dialects: Vec<Dialect>,
    rows: Vec<Vec<f64>>,
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance/fixtures")
}

/// `na` is NaN; every other cell is TradingView's shortest decimal, which parses to its f64.
fn cell(s: &str) -> f64 {
    if s == "na" {
        f64::NAN
    } else {
        s.parse().unwrap_or_else(|_| panic!("cell {s}"))
    }
}

/// The header's dialects and the TradingView `series` block (a `reading` block is a reading's
/// prediction, not the capture, and is not read).
fn load(id: &str) -> Fix {
    let g = &id[..1];
    let src = std::fs::read_to_string(root().join(g).join(format!("{id}.pine"))).expect("pine");
    let fix = std::fs::read_to_string(root().join(g).join(format!("{id}.fix"))).expect("fix");
    let mut dialects = Vec::new();
    let mut rows = Vec::new();
    let mut in_series = false;
    for line in fix.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if in_series {
            if f[0] == "end" {
                break;
            }
            rows.push(f[1..].iter().map(|c| cell(c)).collect());
            continue;
        }
        match f[0] {
            "dialects" => {
                for ch in f[1].chars() {
                    dialects.push(if ch == '5' { Dialect::V5 } else { Dialect::V6 });
                }
            }
            "series" => in_series = true,
            "reading" => break,
            _ => {}
        }
    }
    Fix {
        src,
        dialects,
        rows,
    }
}

/// `//@version=N` rewritten to dialect `d` (the conformance runner's `dialect_text`).
fn in_dialect(src: &str, d: Dialect) -> String {
    let v = d as u8;
    src.replace("//@version=5", &format!("//@version={v}"))
        .replace("//@version=6", &format!("//@version={v}"))
}

fn compile(id: &str, src: &str, d: Dialect) -> PineProgram {
    match compile_src(&in_dialect(src, d), &default_opts(d, ExecMode::BarClose)) {
        Ok(p) => p,
        Err(e) => panic!("{id} {d:?}: {e:#?}"),
    }
}

/// Every plot of `p` over `n` bars, row-major.
fn run(p: &PineProgram, n: usize) -> Vec<Vec<f64>> {
    let v = pine_vm::verify(p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let o = vm.step_bar(&bar(i as u32, 100.0, barflag::CONFIRMED), &mut ctx);
        assert_eq!(o, StepOutcome::Ok, "row {i}: {:?}", vm.last_fault());
        out.push(vm.plots().to_vec());
    }
    out
}

/// The captured row `i` against our plots, bit for bit (NaN for `na`).
fn same_row(want: &[f64], got: &[f64]) -> bool {
    want.iter()
        .zip(got)
        .all(|(w, g)| (w.is_nan() && g.is_nan()) || w.to_bits() == g.to_bits())
}

/// Run `id` in every captured dialect; returns the rows matched, which must be all of them.
fn check(id: &str) -> usize {
    let fx = load(id);
    assert!(!fx.dialects.is_empty(), "{id}: no dialects");
    assert!(!fx.rows.is_empty(), "{id}: no rows");
    for &d in &fx.dialects {
        let p = compile(id, &fx.src, d);
        let got = run(&p, fx.rows.len());
        for (i, want) in fx.rows.iter().enumerate() {
            assert!(
                same_row(want, &got[i]),
                "{id} {d:?} row {i}: ours {:?} vs TV {want:?}",
                got[i]
            );
        }
    }
    fx.rows.len()
}

/// C032 — `type Pt` · `Side s = Side.long` · `o = Pt.new()` · `o.s == Side.long ? 1 : 0`: the
/// default is an enum member, which folds to its ordinal (§4.3, §4.17); 64 of 64 rows (v5, v6).
#[test]
fn r33l_field_default_enum_member_c032() {
    assert_eq!(check("C032"), 64);
}

/// The constant forms pass 3 folds are every UDT field default lowering accepts (§4.3.1 "a field
/// default must fold", §4.17): an enum member (its ordinal — the second member here), a `const`
/// built-in value (`color.red`), `na`, a `const` global, arithmetic over constants, an `int`
/// default of a `float` field (widened, §4.4.3), a `bool`, a string concatenation, a signed
/// literal. Each is read back through `T.new()` in v5 and v6.
#[test]
fn r33l_field_defaults_fold_every_constant_form() {
    let fields = [
        ("E e = E.b", "o.e == E.b ? 1 : 0", 1.0),
        ("E e = E.b", "o.e == E.a ? 1 : 0", 0.0),
        ("color c = color.red", "o.c == color.red ? 1 : 0", 1.0),
        ("float n = na", "na(o.n) ? 1 : 0", 1.0),
        ("int g = K", "o.g", 3.0),
        ("int x = 2 * 3 + 1", "o.x", 7.0),
        ("float f = 1", "o.f / 4", 0.25),
        ("bool t = true", "o.t ? 1 : 0", 1.0),
        ("string s = \"ab\" + \"c\"", "str.length(o.s)", 3.0),
        ("float h = -1.5", "o.h", -1.5),
    ];
    for (decl, read, want) in fields {
        let src = format!(
            "//@version=6\nindicator(\"t\")\nenum E\n    a\n    b\nconst int K = 3\n\
             type T\n    {decl}\no = T.new()\nplot({read})\n"
        );
        for d in [Dialect::V5, Dialect::V6] {
            let p = compile(decl, &src, d);
            let got = run(&p, 2);
            for row in &got {
                assert_eq!(row[0], want, "{decl} · {read} ({d:?})");
            }
        }
    }
}

/// F083 (v5) — `len = 5` · `len := 6` · `ta.ema(syn, len)`: TradingView compiled it, so
/// `pin:02-var-qualifier` is `Join` (the join of the values assigned, `const` here, admitted by
/// `length`'s `simple`); `Series` would refuse `3008`. 1 of 1 captured rows (window `head 1`;
/// row 0 is `na` for either length, so the capture cannot tell 5 from 6). The v6 twin F084
/// stays refused (MG-8: mutable variables are always `series` in v6).
#[test]
fn r33l_var_qualifier_join_f083() {
    assert_eq!(check("F083"), 1);
    let f84 = load("F084");
    let r = compile_src(
        &in_dialect(&f84.src, Dialect::V6),
        &default_opts(Dialect::V6, ExecMode::BarClose),
    );
    assert_eq!(codes(&r), vec![3008]);
}

/// E089 (I9) — the script omits `default_qty_type`, and pine-spec's `strategy()` rows give it a
/// per-dialect default: v5 `strategy.fixed`, v6 `strategy.percent_of_equity` (the v6 one is what
/// TradingView's v6 capture sized: 1 % of equity). That default is the only difference between
/// our two compilations' declarations; with the argument written out they are equal.
#[test]
fn r33l_e089_dialects_differ_only_in_the_default_qty_type_row() {
    let fx = load("E089");
    let (v6, v5) = (
        compile("E089", &fx.src, Dialect::V6).meta.decl,
        compile("E089", &fx.src, Dialect::V5).meta.decl,
    );
    assert_eq!(v6.default_qty_type, QtyType::PercentOfEquity);
    assert_eq!(v5.default_qty_type, QtyType::FixedContracts);
    let mut same = v5;
    same.default_qty_type = v6.default_qty_type;
    assert_eq!(same, v6);
    let written = fx.src.replace(
        "default_qty_value = 1,",
        "default_qty_type = strategy.percent_of_equity, default_qty_value = 1,",
    );
    assert_ne!(written, fx.src);
    assert_eq!(
        compile("E089", &written, Dialect::V5).meta.decl,
        compile("E089", &written, Dialect::V6).meta.decl
    );
}
