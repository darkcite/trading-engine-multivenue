// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! §4.12's descriptors on pass 3's output (impl-02 §7.11.1's `meta_inputs_*` private twins).

use pine_ir::{input_flag, Dialect, FileId, InputKind, InputOverride, InputValue, Sym, SymTable};

use crate::infer::Typed;
use crate::inputs::manifest;
use crate::sw::Readings;
use crate::{imports, infer, resolve, LibCache};

fn typed(body: &str, ov: &[InputOverride]) -> (SymTable, Typed) {
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    let rd = Readings::of(Dialect::V6);
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, &rd, &mut diags);
    let t = infer::infer(&ast, &graph, &res, &mut syms, &rd, 0b0111, ov, &mut diags);
    diags.retain(|d| !(d.code.0 == 3024 && d.msg.contains(" unserved=1")));
    assert!(diags.is_empty(), "{diags:?}");
    (syms, t)
}

fn name(syms: &SymTable, s: Sym) -> &str {
    if s == Sym::NONE {
        ""
    } else {
        syms.resolve(s)
    }
}

#[test]
fn meta_inputs_are_in_source_order() {
    let (syms, t) = typed(
        "f() => input.int(1, \"first\")\nb = input.int(2, \"second\")\nplot(f() + f() + b)\n",
        &[],
    );
    let names: Vec<&str> = t.inputs.iter().map(|d| name(&syms, d.title)).collect();
    assert_eq!(
        names,
        ["first", "second"],
        "one site for two expansions, source order"
    );
}

#[test]
fn meta_inputs_name_is_variable_else_title() {
    let (syms, t) = typed(
        "len = input.int(14, \"Length\")\nplot(close, linewidth = input.int(1, \"Width\"))\n\
         plot(close, linewidth = input.int(2))\n",
        &[],
    );
    let names: Vec<&str> = t.inputs.iter().map(|d| name(&syms, d.name)).collect();
    assert_eq!(names, ["len", "Width", ""]);
    assert_eq!(t.inputs[0].kind, InputKind::Int);
    assert_eq!(t.inputs[0].default_bits, 14);
}

#[test]
fn meta_inputs_override_sets_value_and_flag() {
    let ov = [InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(20),
    }];
    let (_, t) = typed(
        "len = input.int(14, \"L\", minval = 1, maxval = 50, step = 2)\nplot(close)\n",
        &ov,
    );
    let d = t.inputs[0];
    assert_eq!(d.default_bits, 20);
    assert_ne!(d.flags & input_flag::OVERRIDDEN, 0);
    let want = input_flag::HAS_MIN | input_flag::HAS_MAX | input_flag::HAS_STEP;
    assert_eq!(d.flags & want, want);
    assert_eq!((d.min_bits, d.max_bits, d.step_bits), (1, 50, 2));
    let (_, t) = typed("len = input.int(14, \"L\")\nplot(close)\n", &[]);
    assert_eq!(t.inputs[0].flags & input_flag::OVERRIDDEN, 0);
}

#[test]
fn meta_inputs_manifest_form() {
    let syms = SymTable::new();
    use crate::infer::K;
    assert_eq!(
        manifest(K::Float(1.5), pine_ir::Ty::Float, &syms),
        Some(InputValue::Float1e6(1_500_000))
    );
    assert_eq!(
        manifest(K::Int(3), pine_ir::Ty::Int, &syms),
        Some(InputValue::Int(3))
    );
}

#[test]
fn meta_inputs_source_override_aliases_a_series() {
    let body = "src = input.source(close, \"Src\")\nplot(ta.sma(src, 10))\n";
    let (syms, t) = typed(body, &[]);
    let d = t.inputs[0];
    assert_eq!(d.kind, InputKind::Source);
    assert_eq!(syms.resolve(Sym(d.default_bits as u32)), "close");
    assert_eq!(t.sources.len(), 1);
    assert_eq!(syms.resolve(t.sources[0].1), "close");
    let ov = [InputOverride {
        name: "src".to_owned(),
        value: InputValue::Str("hlc3".to_owned()),
    }];
    let (syms, t) = typed(body, &ov);
    let d = t.inputs[0];
    assert_ne!(d.flags & input_flag::OVERRIDDEN, 0);
    assert_eq!(syms.resolve(Sym(d.default_bits as u32)), "hlc3");
    assert_eq!(syms.resolve(t.sources[0].1), "hlc3", "an alias, not a copy");
}

#[test]
fn meta_inputs_options_reach_resolve_input() {
    let ov = |v: i64| {
        [InputOverride {
            name: "n".to_owned(),
            value: InputValue::Int(v),
        }]
    };
    let body = "n = input.int(2, \"N\", options = [1, 2, 3])\nplot(close * n)\n";
    let (_, t) = typed(body, &ov(3));
    assert_eq!(t.inputs[0].default_bits, 3);
    assert_ne!(t.inputs[0].flags & input_flag::HAS_OPTIONS, 0);
}
