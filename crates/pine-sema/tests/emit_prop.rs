// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.6 / §7.7: `prop_emitted_programs_verify` — every program sema returns `Ok` for
//! passes `pine_vm::verify` against pine-std's table, in both dialects and both modes, and runs
//! 64 bars on the VM without a fault. The generator composes small strategies from statement
//! templates over the earlier variables (T28 grows it into `arb_script`).

mod common;

use common::{compile_src, default_opts};
use pine_ir::{barflag, Dialect, ExecMode, StepOutcome};
use pine_vm::testkit::{bar, Arena, Ctx};
use proptest::prelude::*;

/// One statement defining `v{i}` from earlier variables `a`, `b`.
fn stmt(t: u8, i: usize, a: &str, b: &str, n: u32) -> String {
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
        _ => format!("v{i} = -{a}\n"),
    }
}

fn script(d: Dialect, parts: &[(u8, usize, usize, u32)]) -> String {
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

fn run(src: &str, d: Dialect, mode: ExecMode) -> bool {
    let Ok(p) = compile_src(src, &default_opts(d, mode)) else {
        return false;
    };
    // compile_src already asserted verify; run it.
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    for i in 0..64u32 {
        let c = 100.0 + 10.0 * (f64::from(i) * 0.37).sin() + f64::from(i % 7);
        let out = vm.step_bar(&bar(i, c, barflag::CONFIRMED), &mut ctx);
        assert_eq!(out, StepOutcome::Ok, "bar {i} {:?}\n{src}", vm.last_fault());
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn prop_emitted_programs_verify(
        parts in prop::collection::vec((0u8..18, 0usize..8, 0usize..8, 2u32..40), 1..8)
    ) {
        for d in [Dialect::V5, Dialect::V6] {
            for mode in [ExecMode::BarClose, ExecMode::Realtime] {
                run(&script(d, &parts), d, mode);
            }
        }
    }
}

/// The generator reaches emission: every template compiles to an `Ok` program on its own.
#[test]
fn prop_templates_each_compile() {
    for t in 0u8..18 {
        for d in [Dialect::V5, Dialect::V6] {
            let src = script(d, &[(t, 0, 0, 5)]);
            assert!(
                run(&src, d, ExecMode::BarClose),
                "template {t} v{}:\n{src}\n{:?}",
                d as u8,
                compile_src(&src, &default_opts(d, ExecMode::BarClose)).err()
            );
        }
    }
}
