// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group A — `na` / `nz` × five types, and W13 (impl-06 §4.2.1). GENERATOR-TIME ONLY.
//!
//! 96 cells: type row `t` (int 0, float 1, color 2, string 3 — `dialects 56`; bool-v5 4 —
//! `dialects 5`; bool-v6 5 — `dialects 6`) × kernel K01–K16, id `A(16·t + k)`. Held fixed: SYN-1,
//! `align step`, `window head 64`, `mode bar_close`, the isna/val pair.
//!
//! Refusal cells are data: a kernel builtin with no overload admitting the row's type in the
//! dialect (per `pine_spec::OVERLOADS`) is `refuse 3010`, or `3005` when the overload exists only in
//! the other dialect. The v6 bool row's `3007` cells are §4.2.1's table (the `bool n = na`
//! declaration, W13), not a builtin lookup.

use pine_ir::{DiagCode, Dialect, Ty};
use pine_spec::{kind, params, pf, tv};

use super::emit::{isna_val, sfx, Line, Prelude, Script};
use super::{Cell, Run};
use crate::fixture::{Dialects, Expect, FixtureId, Group, Oracle, Tol, Window};

/// A kernel's `tests` column (§4.2.1, backticks dropped).
///
/// `pin:06-a-titles` — a fixture's title is its kernel's `tests` column followed by ` · <type>`;
/// §4.2.1 gives no title column.
pub const K_TESTS: [&str; 16] = [
    "a declared na stays na",
    "na → value at a known step",
    "nz 1-arg default",
    "nz replacement",
    "na == na",
    "na != na",
    "na() on na",
    "na() on a value",
    "var across a conditional assignment",
    "history on the first bar",
    "na through four bars of history",
    "na through a user function",
    "na as a switch subject",
    "na in a collection",
    "na through an operation",
    "fixnan",
];

/// One type row.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// Row index `t`.
    pub t: u16,
    /// The type keyword.
    pub ty_kw: &'static str,
    /// The row's name in titles and comments.
    pub name: &'static str,
    /// The `pine_ir` type.
    pub ty: Ty,
    /// The sample `S`.
    pub s: &'static str,
    /// `pin:06-a-k13-arms` — K13's second arm value (§4.2.1 says "two value arms" without them).
    pub s2: &'static str,
    /// Declared dialects.
    pub dialects: Dialects,
}

/// The six rows of §4.2.1.
pub const ROWS: [Row; 6] = [
    Row {
        t: 0,
        ty_kw: "int",
        name: "int",
        ty: Ty::Int,
        s: "7",
        s2: "8",
        dialects: Dialects::BOTH,
    },
    Row {
        t: 1,
        ty_kw: "float",
        name: "float",
        ty: Ty::Float,
        s: "7.5",
        s2: "8.5",
        dialects: Dialects::BOTH,
    },
    Row {
        t: 2,
        ty_kw: "color",
        name: "color",
        ty: Ty::Color,
        s: "color.rgb(1, 2, 3)",
        s2: "color.rgb(4, 5, 6)",
        dialects: Dialects::BOTH,
    },
    Row {
        t: 3,
        ty_kw: "string",
        name: "string",
        ty: Ty::String,
        s: "\"ab\"",
        s2: "\"cd\"",
        dialects: Dialects::BOTH,
    },
    Row {
        t: 4,
        ty_kw: "bool",
        name: "bool",
        ty: Ty::Bool,
        s: "true",
        s2: "false",
        dialects: Dialects::V5,
    },
    Row {
        t: 5,
        ty_kw: "bool",
        name: "bool",
        ty: Ty::Bool,
        s: "true",
        s2: "false",
        dialects: Dialects::V6,
    },
];

/// `A(16·t + k)`.
pub fn id_of(t: u16, k: u16) -> FixtureId {
    FixtureId::from_parts(Group::A, 16 * t + k)
}

/// Whether some overload of builtin `name` admits `ty` at its parameter `param` in `d`: a
/// concrete parameter of exactly that type, or a type variable whose kinds include it. Parameters
/// are filtered by their own version mask.
///
/// `pin:06-a-admission` — "admitting T" is read on the parameter the kernel passes `n` to, with no
/// implicit int → float widening (every row's type has its own overload where Pine accepts it).
pub fn admits(name: &str, param: &str, ty: Ty, d: Dialect) -> bool {
    let e = pine_spec::lookup(name).unwrap_or_else(|| panic!("{name} is not in pine_spec"));
    e.overloads().iter().any(|ov| {
        if !ov.versions.has(d) {
            return false;
        }
        let Some(p) = params(ov).iter().find(|p| {
            p.versions.has(d) && p.flags & pf::SYNTHETIC == 0 && p.name.as_str() == param
        }) else {
            return false;
        };
        let var = tv::whole(p.sig.tv);
        if var == 0 {
            p.sig.ty.ty == ty
        } else {
            ov.tv_kinds[usize::from(var) - 1] & kind::of(ty) != 0
        }
    })
}

/// The refusal a builtin call on `ty` gets in `d`, if any: `3010` NoOverload, or `3005`
/// NotInDialect when the other dialect admits it.
pub fn builtin_refusal(name: &str, param: &str, ty: Ty, d: Dialect) -> Option<DiagCode> {
    if admits(name, param, ty, d) {
        return None;
    }
    let other = match d {
        Dialect::V5 => Dialect::V6,
        Dialect::V6 => Dialect::V5,
    };
    Some(if admits(name, param, ty, other) {
        DiagCode(3005)
    } else {
        DiagCode(3010)
    })
}

/// The builtin kernel `k` passes the row's operand to, and the receiving parameter. (K14's
/// `array.get` receives the array, not `n`: only `array.new`'s `initial_value` is checked.)
fn kernel_builtins(k: u16, r: &Row) -> &'static [(&'static str, &'static str)] {
    match k {
        3 | 4 => &[("nz", "source")],
        7 | 8 => &[("na", "x")],
        14 => &[("array.new", "initial_value")],
        15 if r.ty == Ty::Color => &[("color.new", "color")],
        16 => &[("fixnan", "source")],
        _ => &[],
    }
}

/// The v6 bool row's codes (§4.2.1 table, A081–A096); `None` = a value fixture (A090).
pub const V6_BOOL_CODES: [Option<u16>; 16] = [
    Some(3007),
    Some(3007),
    Some(3005),
    Some(3005),
    Some(3007),
    Some(3007),
    Some(3005),
    Some(3005),
    Some(3007),
    None,
    Some(3007),
    Some(3007),
    Some(3007),
    Some(3007),
    Some(3007),
    Some(3005),
];

/// The expectation of cell `(r, k)`: a data refusal, the v6 bool table, or `Pending`.
///
/// # Panics
/// When a `dialects 56` row's builtin admission differs between dialects: §4.3 rule 2 would twin
/// it, and the table has no such cell — the data changed under the generator (fail fast).
pub fn expectation(r: &Row, k: u16) -> Expect {
    if r.t == 5 {
        return match V6_BOOL_CODES[usize::from(k) - 1] {
            Some(c) => Expect::Refuse { code: DiagCode(c) },
            None => Expect::Pending,
        };
    }
    for &(b, p) in kernel_builtins(k, r) {
        let code = if r.dialects == Dialects::BOTH {
            let (c5, c6) = (
                builtin_refusal(b, p, r.ty, Dialect::V5),
                builtin_refusal(b, p, r.ty, Dialect::V6),
            );
            assert_eq!(
                c5.is_some(),
                c6.is_some(),
                "{b} × {} differs between dialects: §4.3 rule 2 would twin A{:03}",
                r.name,
                16 * r.t + k
            );
            c6.map(|_| DiagCode(3010))
        } else {
            builtin_refusal(b, p, r.ty, Dialect::V5)
        };
        if let Some(code) = code {
            return Expect::Refuse { code };
        }
    }
    Expect::Pending
}

/// `PROJ` (§4.2.0): makes any type plottable.
fn proj(r: &Row, e: &str) -> String {
    match r.ty {
        Ty::Bool => format!("({e} ? 1 : 0)"),
        Ty::Color => format!("color.r({e}) * 65536 + color.g({e}) * 256 + color.b({e})"),
        Ty::String => format!("str.length({e})"),
        _ => e.to_string(),
    }
}

/// The body of cell `(r, k)`.
///
/// `pin:06-a-v6-bool-substitution` — in the v6 bool row K03/K04/K07/K16 apply the function to
/// `x = synb` (§4.2.1: "use `bool x = synb` … and apply the function"); K08 keeps `bool x = S` and
/// its `n` arm becomes `false`; bool K10 (both rows) is `bool x = synb` · `e = x[1]` without `n`.
fn body(r: &Row, k: u16, id: FixtureId) -> Vec<Line> {
    let (n, x, e) = (sfx("n", id), sfx("x", id), sfx("e", id));
    let (ty, s, s2) = (r.ty_kw, r.s, r.s2);
    let v6b = r.t == 5;
    let mut b = Vec::new();
    let no_n = r.ty == Ty::Bool && k == 10 || v6b && matches!(k, 3 | 4 | 7 | 8 | 16);
    if !no_n {
        b.push(Line::top(format!("{ty} {n} = na")));
    }
    let top = |b: &mut Vec<Line>, t: String| b.push(Line::top(t));
    match k {
        1 => top(&mut b, format!("{e} = {n}")),
        2 => {
            top(&mut b, format!("{ty} {x} = _n > 32 ? {s} : {n}"));
            top(&mut b, format!("{e} = {x}"));
        }
        3 | 4 | 7 | 16 if v6b => {
            top(&mut b, format!("bool {x} = synb"));
            top(
                &mut b,
                match k {
                    3 => format!("{e} = nz({x})"),
                    4 => format!("{e} = nz({x}, {s})"),
                    7 => format!("{e} = na({x}) ? {s} : {x}"),
                    _ => format!("{e} = fixnan({x})"),
                },
            );
        }
        3 => top(&mut b, format!("{e} = nz({n})")),
        4 => top(&mut b, format!("{e} = nz({n}, {s})")),
        5 => top(&mut b, format!("{e} = {n} == {n} ? {s} : {n}")),
        6 => top(&mut b, format!("{e} = {n} != {n} ? {s} : {n}")),
        7 => top(&mut b, format!("{e} = na({n}) ? {s} : {n}")),
        8 => {
            top(&mut b, format!("{ty} {x} = {s}"));
            let arm = if v6b { "false".to_string() } else { n.clone() };
            top(&mut b, format!("{e} = na({x}) ? {arm} : {x}"));
        }
        9 => {
            top(&mut b, format!("var {ty} {x} = {n}"));
            top(&mut b, "if synb".to_string());
            b.push(Line::block(format!("{x} := {s}")));
            top(&mut b, format!("{e} = {x}"));
        }
        10 if r.ty == Ty::Bool => {
            top(&mut b, format!("bool {x} = synb"));
            top(&mut b, format!("{e} = {x}[1]"));
        }
        10 => {
            top(&mut b, format!("{ty} {x} = synb ? {s} : {n}"));
            top(&mut b, format!("{e} = {x}[1]"));
        }
        11 => {
            top(&mut b, format!("{ty} {x} = _n < 5 ? {n} : {s}"));
            top(&mut b, format!("{e} = {x}[4]"));
        }
        12 => {
            let (f, a) = (sfx("f", id), sfx("a", id));
            top(&mut b, format!("{f}({ty} {a}) => {a}"));
            top(&mut b, format!("{e} = {f}({n})"));
        }
        13 => {
            top(&mut b, format!("{e} = switch {n}"));
            b.push(Line::block(format!("{s} => {s}")));
            b.push(Line::block(format!("{s2} => {s}")));
            b.push(Line::block(format!("=> {n}")));
        }
        14 => {
            let arr = sfx("arr", id);
            top(&mut b, format!("{arr} = array.new<{ty}>(4, {n})"));
            top(&mut b, format!("{e} = array.get({arr}, 2)"));
        }
        15 => top(
            &mut b,
            match r.ty {
                Ty::Color => format!("{e} = color.new({n}, 50)"),
                Ty::Bool => format!("{e} = {n} and {s}"),
                _ => format!("{e} = {n} + {s}"),
            },
        ),
        _ => {
            debug_assert_eq!(k, 16);
            top(&mut b, format!("{e} = fixnan({n})"));
        }
    }
    b
}

/// Cell `(r, k)`.
pub fn cell(r: &Row, k: u16) -> Cell {
    let id = id_of(r.t, k);
    let e = sfx("e", id);
    let bool_row = r.ty == Ty::Bool;
    // §4.3 rule 4: every v6 bool cell is twinned with the v5 bool cell of the same kernel.
    let twin = match r.t {
        4 => Some(id_of(5, k)),
        5 => Some(id_of(4, k)),
        _ => None,
    };
    let mut pins = Vec::new();
    if k == 5 || k == 6 {
        pins.push("pin:06-na-compare".to_string());
    }
    // pin:06-a-w13-scope: both bool rows carry W13 (the v6 row is W13; the v5 row is its twin).
    if bool_row {
        pins.push("pin:W13-bool-na".to_string());
    }
    // A v6 bool has no `na()` (W13): only `f_<ID>_val = E ? 1 : 0`.
    let plots = if r.t == 5 {
        vec![(format!("{e} ? 1 : 0"), "val".to_string())]
    } else {
        isna_val(&e, &proj(r, &e))
    };
    let tols = vec![Tol::Exact; plots.len()];
    let expect = expectation(r, k);
    // pin:06-refusal-oracle: a refusal cell is our design's assertion (§0.3 D6-a, 100 %), so it is
    // `authored` with stamps `n/a`; TradingView's pasted refusal goes to the vault log (§4.7.5).
    let oracle = if matches!(expect, Expect::Refuse { .. }) {
        Oracle::Authored
    } else {
        Oracle::Tv
    };
    let title = format!("{} · {}", K_TESTS[usize::from(k) - 1], r.name);
    let script = Script {
        id,
        dialects: r.dialects,
        about: vec![
            K_TESTS[usize::from(k) - 1].to_string(),
            format!("K{k:02} × {}", r.name),
        ],
        twin,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::Syn1,
        body: body(r, k, id),
        plots,
    };
    Cell {
        script,
        title,
        pins,
        oracle,
        // pin:06-a-tier: §4.2.1 marks K14 tier 3; every other cell is read as tier 1.
        tier: if k == 14 { 3 } else { 1 },
        window: Window::Head(64),
        tols,
        expect,
        readings: Vec::new(),
        run: Run::default(),
    }
}

/// All 96 cells, A001–A096.
pub fn cells() -> Vec<Cell> {
    let mut out = Vec::with_capacity(96);
    for r in &ROWS {
        for k in 1..=16 {
            out.push(cell(r, k));
        }
    }
    out
}
