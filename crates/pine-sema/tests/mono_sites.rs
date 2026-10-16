// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.4's `mono_*` rows (§4.6). The `sites_*` rows and `mono_two_call_sites_get_two_state_sets`
//! need sites (T14), `mono_no_call_or_ret_is_emitted` emission (T19); the Instantiation tree they
//! rest on is asserted by the private twins in `src/tests/mono.rs`.

mod common;

use common::*;
use pine_ir::{Dialect, ExecMode};
use pine_sema::limits::{MAX_MONO_PER_FN, MAX_MONO_TOTAL, MAX_PARAMS};

fn run(body: &str) -> Vec<u16> {
    codes(&compile_src(
        &format!("//@version=6\nindicator(\"t\")\n{body}"),
        &default_opts(Dialect::V6, ExecMode::BarClose),
    ))
}

fn refusal(body: &str) -> Vec<pine_ir::Diag> {
    diags(compile_src(
        &format!("//@version=6\nindicator(\"t\")\n{body}"),
        &default_opts(Dialect::V6, ExecMode::BarClose),
    ))
}

#[test]
fn mono_explosion_is_refused() {
    // 66 functions × 63 call sites: no function above MAX_MONO_PER_FN, the program above
    // MAX_MONO_TOTAL.
    let per = MAX_MONO_PER_FN - 1;
    let fns = MAX_MONO_TOTAL / per + 2;
    let mut body = String::new();
    for f in 0..fns {
        body.push_str(&format!("g{f}(x) => x + {f}\n"));
    }
    for f in 0..fns {
        for k in 0..per {
            body.push_str(&format!("a{f}_{k} = g{f}(close)\n"));
        }
    }
    body.push_str("plot(close)\n");
    let d = refusal(&body);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code.0, 3013);
    assert!(d[0].msg.contains("MAX_MONO_TOTAL"), "{}", d[0].msg);
    // At the cap exactly: compiles.
    let mut ok = String::new();
    ok.push_str("g(x) => x + 1\n");
    for k in 0..per {
        ok.push_str(&format!("a{k} = g(close)\n"));
    }
    ok.push_str("plot(close)\n");
    assert_eq!(run(&ok), vec![ACCEPTED]);
}

#[test]
fn mono_per_fn_cap_is_refused() {
    let mut body = "helper(x) => x * 2\n".to_owned();
    for k in 0..=MAX_MONO_PER_FN {
        body.push_str(&format!("a{k} = helper(close)\n"));
    }
    body.push_str("plot(close)\n");
    let d = refusal(&body);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code.0, 3013);
    assert!(d[0].msg.contains("`helper`"), "{}", d[0].msg);
    assert!(d[0].notes.iter().any(|n| n.1.contains("`helper`")));
    // Nested: 8 × 9 = 72 expansions of `inner` through 8 sites of `outer`.
    let mut nested = "inner(x) => x + 1\nouter(x) =>\n".to_owned();
    for k in 0..9 {
        nested.push_str(&format!("    v{k} = inner(x)\n"));
    }
    nested.push_str("    x\n");
    for k in 0..8 {
        nested.push_str(&format!("b{k} = outer(close)\n"));
    }
    nested.push_str("plot(close)\n");
    let d = refusal(&nested);
    assert_eq!(d.iter().map(|x| x.code.0).collect::<Vec<_>>(), vec![3013]);
    assert!(d[0].msg.contains("`inner`"), "{}", d[0].msg);
}

#[test]
fn mono_too_many_params_is_refused() {
    let ps: Vec<String> = (0..=MAX_PARAMS).map(|k| format!("p{k}")).collect();
    let body = format!("f({}) => p0\nplot(close)\n", ps.join(", "));
    assert_eq!(run(&body), vec![3014]);
    let ps: Vec<String> = (0..MAX_PARAMS).map(|k| format!("p{k}")).collect();
    let args: Vec<&str> = (0..MAX_PARAMS).map(|_| "close").collect();
    let body = format!("f({}) => p0\nplot(f({}))\n", ps.join(", "), args.join(", "));
    assert_eq!(run(&body), vec![ACCEPTED]);
}

#[test]
fn mono_direct_recursion_is_refused() {
    let d = refusal("f(x) => f(x - 1)\nplot(f(close))\n");
    assert_eq!(d.iter().map(|x| x.code.0).collect::<Vec<_>>(), vec![3012]);
    assert!(d[0].msg.contains("f → f"), "{}", d[0].msg);
    assert_eq!(d[0].notes.len(), 2);
}

#[test]
fn mono_mutual_recursion_is_refused() {
    let d = refusal("f(x) => g(x)\ng(x) => f(x)\nplot(f(close))\n");
    assert_eq!(d.iter().map(|x| x.code.0).collect::<Vec<_>>(), vec![3012]);
    assert!(d[0].msg.contains("f → g → f"), "{}", d[0].msg);
    assert_eq!(d[0].notes.len(), 3);
}

#[test]
fn mono_overload_calling_other_overload_is_not_recursion() {
    let body = "f(float x) => x * 2\nf(int x) => f(float(x))\nplot(f(3))\n";
    assert_eq!(run(body), vec![ACCEPTED]);
}
