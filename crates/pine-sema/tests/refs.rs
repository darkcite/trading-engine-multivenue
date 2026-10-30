// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.4's reference rules (§4.9, codes 3046–3048): the impl-06 C022–C031 twins, typed
//! from §4.9's descriptions (impl-06's fixtures are not in this lane's vault).

mod common;

use common::{codes, compile_src, default_opts};
use pine_ir::{Dialect, ExecMode};

fn v6(body: &str) -> String {
    format!("//@version=6\nindicator(\"t\")\n{body}")
}

fn run(body: &str) -> Vec<u16> {
    let r = compile_src(&v6(body), &default_opts(Dialect::V6, ExecMode::BarClose));
    let c = codes(&r);
    match r {
        Ok(p) => {
            if let Err(e) = pine_vm::verify(&p, &pine_std::BUILTINS) {
                panic!("rule {} pc {} detail {}\n{body}", e.rule, e.pc, e.detail);
            }
        }
        Err(ds) => {
            for d in ds {
                eprintln!("{} {}", d.code.0, d.msg);
            }
        }
    }
    c
}

const PT: &str = "type Pt\n    float x = 0.0\n    int n = 1\n";

#[test]
fn refs_creation_in_loop_is_refused() {
    assert_eq!(
        run("for i = 0 to 2\n    a = array.new<float>()\nplot(close)\n"),
        vec![3046]
    );
    let s = format!("{PT}for i = 0 to 2\n    p = Pt.new(i)\n    plot(p.x)\n");
    assert_eq!(run(&s), vec![3046]);
}

#[test]
fn refs_varip_reference_is_refused() {
    assert_eq!(
        run("varip a = array.new<float>()\nplot(close)\n"),
        vec![3048]
    );
    let s = format!("{PT}varip p = Pt.new(1.0)\nplot(p.x)\n");
    assert_eq!(run(&s), vec![3048]);
}

#[test]
fn refs_varip_field_is_refused() {
    assert_eq!(
        run("type T\n    varip int n = 0\nt = T.new()\nplot(t.n)\n"),
        vec![3048]
    );
}

/// impl-06 C030: a fresh object stored into a `var` variable.
#[test]
fn refs_fresh_object_into_var_is_escape() {
    let s = format!("{PT}var p = Pt.new(1.0)\np := Pt.new(close)\nplot(p.x)\n");
    assert_eq!(run(&s), vec![3047]);
    let s = format!("{PT}var Pt p = na\nif bar_index == 0\n    p := Pt.new(close)\nplot(p.x)\n");
    assert_eq!(run(&s), vec![3047]);
}

/// impl-06 C030's generated body, verbatim (SYN-1 + `var Pt o = na` assigned on bar 3, read
/// through `na(o)`): `dialects 56`, `refuse 3047` — the same code in v5 and v6 (I9).
const C030_BODY: &str = "var float _s = 1.0\nvar int   _n = 0\n_n := _n + 1\n\
_hi = math.floor(_s / 127773.0)\n_lo = _s - _hi * 127773.0\n_t  = 16807.0 * _lo - 2836.0 * _hi\n\
_s := _t > 0.0 ? _t : _t + 2147483647.0\n\
syn   = 1000.0 + (_s - math.floor(_s / 100000.0) * 100000.0) / 100.0\n\
syni  = int(_s - math.floor(_s / 1000.0) * 1000.0)\nsynb  = syni >= 500\n\
synna = syni < 100 ? na : syn\ntype Pt_C030\n    int x_C030\nvar Pt_C030 o_C030 = na\n\
if _n == 3\n    o_C030 := Pt_C030.new(7)\n\
plot(na(o_C030) ? 1 : 0, \"f_C030_isna\", display = display.data_window)\n\
plot(na(o_C030) ? na : o_C030.x_C030, \"f_C030_val\", display = display.data_window)\n";

#[test]
fn refs_c030_fixture_is_escape_in_both_dialects() {
    for d in [Dialect::V5, Dialect::V6] {
        let src = format!(
            "//@version={}\nindicator(\"C030\", overlay = false)\n{C030_BODY}",
            d as u8
        );
        let r = compile_src(&src, &default_opts(d, ExecMode::BarClose));
        if let Err(ds) = &r {
            for x in ds {
                eprintln!("v{}: {} {}", d as u8, x.code.0, x.msg);
            }
        }
        assert_eq!(codes(&r), vec![3047], "v{}", d as u8);
    }
}

/// impl-06 C026: a creation inside a helper called from a `var` initialiser is fresh.
#[test]
fn refs_helper_created_object_into_var_is_escape() {
    let s = format!("{PT}mk() => Pt.new(1.0)\nvar o = mk()\nplot(o.x)\n");
    assert_eq!(run(&s), vec![3047]);
    // A fresh object into a field of a var-born one is the same escape.
    let s = format!(
        "{PT}type Box\n    Pt p\nmk() => Pt.new(2.0)\nvar b = Box.new(mk())\nplot(b.p.x)\n"
    );
    assert_eq!(run(&s), vec![3047]);
    let s = format!(
        "{PT}type Box\n    Pt p\nvar b = Box.new(Pt.new(1.0))\nb.p := Pt.new(close)\nplot(b.p.x)\n"
    );
    assert_eq!(run(&s), vec![3047]);
}

/// impl-06 C027: a fresh element pushed into a `var` array.
#[test]
fn refs_fresh_element_into_var_array_is_escape() {
    let s = format!("{PT}var a = array.new<Pt>()\narray.push(a, Pt.new(close))\nplot(close)\n");
    assert_eq!(run(&s), vec![3047]);
}

/// impl-06 C031: a `var` object nested in a `var` initialiser — both sites var-born.
#[test]
fn refs_var_object_nested_in_var_initialiser_compiles() {
    let s = format!(
        "{PT}type Box\n    Pt p\nvar b = Box.new(Pt.new(1.0))\nb.p.x := b.p.x + close\nplot(b.p.x)\n"
    );
    assert_eq!(run(&s), vec![common::ACCEPTED]);
}

/// impl-06 C022–C025, C028: reference copies through plain variables, parameters and fields
/// compile — a plain variable is not persistent storage.
#[test]
fn refs_reference_copies_compile() {
    for body in [
        // C022: a copy of a reference aliases the object.
        "a = Pt.new(close)\nb = a\nb.x := 2.0\nplot(a.x)\n",
        // C023: a `var` object read through a plain alias.
        "var p = Pt.new(0.0)\nq = p\nq.x := q.x + 1.0\nplot(p.x)\n",
        // C024: a reference passed to a function and mutated there.
        "bump(Pt p) =>\n    p.x := p.x + 1.0\n    p.x\nvar p = Pt.new(0.0)\nplot(bump(p))\n",
        // C025: a fresh object reassigned into a plain variable every bar.
        "p = Pt.new(close)\np := Pt.new(close * 2)\nplot(p.x)\n",
        // C028: a `var` reference reassigned from another `var` one.
        "var a = Pt.new(1.0)\nvar b = Pt.new(2.0)\nb := a\nplot(b.x)\n",
        // A `var` object's copy is fresh but lands in a plain variable.
        "var a = Pt.new(1.0)\nc = a.copy()\nplot(c.x)\n",
    ] {
        let s = format!("{PT}{body}");
        assert_eq!(run(&s), vec![common::ACCEPTED], "{body}");
    }
}

/// A `string` field store (CAR-03-1 (a)): one candidate site, and the site dispatch over two.
#[test]
fn refs_string_field_stores_compile_and_verify() {
    let one = "type L\n    string s = \"a\"\nvar l = L.new()\nl.s := close > open ? \"up\" : \"dn\"\nplot(close)\n";
    assert_eq!(run(one), vec![common::ACCEPTED]);
    let two = "type L\n    string s = \"a\"\nx = L.new()\ny = L.new(\"b\")\nz = close > open ? x : y\nz.s := \"c\"\nplot(close)\n";
    assert_eq!(run(two), vec![common::ACCEPTED]);
}
