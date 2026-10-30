// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The shared script generator (impl-02 §7.6 `arb_script`, included by `#[path]`): small
//! strategies composed from statement templates over the earlier variables.

use pine_ir::Dialect;

/// How many statement templates [`stmt`] has.
pub const TEMPLATES: u8 = 20;

/// One statement defining `v{i}` from earlier variables `a`, `b`.
pub fn stmt(t: u8, i: usize, a: &str, b: &str, n: u32) -> String {
    let k = 1 + n % 9;
    match t {
        0 => format!("v{i} = ta.sma({a}, {n})\n"),
        1 => format!("v{i} = ta.ema({a}, {n})\n"),
        2 => format!("v{i} = {a}[{k}]\n"),
        3 => format!("v{i} = {a} + {b}\n"),
        4 => format!("v{i} = {a} > {b} ? {a} : {b}\n"),
        5 => format!("v{i} = nz({a}, 0.0)\n"),
        6 => format!("v{i} = math.max({a}, {b})\n"),
        7 => format!(
            "if ta.crossover({a}, {b})\n    strategy.entry(\"L\", strategy.long)\nv{i} = {a}\n"
        ),
        8 => format!("if {a} < {b}\n    strategy.close(\"L\")\nv{i} = {b}\n"),
        9 => format!("var float v{i} = 0.0\nv{i} := v{i} * 0.5 + nz({a}) * 0.5\n"),
        10 => format!("v{i} = ta.rsi({a}, {n})\n"),
        11 => format!("v{i} = {a} - ta.highest({a}, {n})\n"),
        12 => format!("int c{i} = bar_index % 3\nv{i} = c{i} == 0 ? {a} : {b}\n"),
        13 => format!("v{i} = 0.0\nfor j = 0 to 2\n    v{i} += {a}\n"),
        14 => format!("v{i} = {a}[bar_index % {k}]\n"),
        15 => format!("v{i} = {a} / {b}\n"),
        16 => format!("v{i} = ta.change({a})\n"),
        17 => format!("v{i} = -{a}\n"),
        // A UDT with a string field: creation, a string store, a field read (§4.14.7).
        18 => format!(
            "type T{i}\n    float f = 0.0\n    string s = \"x\"\n    int n\n\
             o{i} = T{i}.new({a})\no{i}.s := {a} > {b} ? \"u\" : \"d\"\n\
             c{i} = o{i}.copy()\nv{i} = o{i}.s == \"u\" ? c{i}.f : {b}\n"
        ),
        // A `var` object accumulating across bars.
        _ => format!(
            "type W{i}\n    float f = 0.0\nvar w{i} = W{i}.new()\n\
             w{i}.f := w{i}.f * 0.5 + nz({a}) * 0.5\nv{i} = w{i}.f\n"
        ),
    }
}

pub fn script(d: Dialect, parts: &[(u8, usize, usize, u32)]) -> String {
    let mut s = format!("//@version={}\nstrategy(\"p\")\nv0 = close\n", d as u8);
    for (j, &(t, a, b, n)) in parts.iter().enumerate() {
        let i = j + 1;
        let a = format!("v{}", a % i);
        let b = format!("v{}", b % i);
        s.push_str(&stmt(t, i, &a, &b, n));
    }
    s.push_str(&format!("plot(v{})\n", parts.len()));
    s
}
