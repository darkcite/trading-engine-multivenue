// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.7 `tests/emit.rs`: emitted programs pass `pine_vm::verify` against pine-std's table.

mod common;

use common::{compile_src, default_opts};
use pine_ir::{op, Dialect, ExecMode, PineProgram, ScriptKind};

const SMA_CROSS: &str = "strategy(\"x\")
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
";

fn sma_cross(d: Dialect, mode: ExecMode) -> PineProgram {
    let src = format!("//@version={}\n{SMA_CROSS}", d as u8);
    match compile_src(&src, &default_opts(d, mode)) {
        Ok(p) => p,
        Err(ds) => panic!("{d:?} {mode:?}: {ds:#?}"),
    }
}

#[test]
fn emit_sma_cross_verifies_in_both_modes_and_dialects() {
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let p = sma_cross(d, mode);
            if let Err(e) = pine_vm::verify(&p, &pine_std::BUILTINS) {
                panic!(
                    "{d:?} {mode:?}: rule {} pc {} detail {}\n{:#?}",
                    e.rule, e.pc, e.detail, p.code
                );
            }
            assert_eq!(p.code.last().map(|i| i.op), Some(op::HALT));
            assert_eq!(p.meta.kind, ScriptKind::Strategy);
            assert_eq!(p.meta.dialect, d);
            assert_eq!(p.dialect, d);
            assert_eq!(p.mode, mode);
        }
    }
}

#[test]
fn emit_sma_cross_shape() {
    let p = sma_cross(Dialect::V6, ExecMode::BarClose);
    // Four stateful sites: two SMAs, the crossover, the crossunder.
    assert_eq!(p.layout.states.len(), 4);
    let calls = p.code.iter().filter(|i| i.op == op::CALL_BUILTIN).count();
    assert_eq!(calls, 7, "4 stateful + entry + close + plot");
    assert_eq!(p.layout.n_plots, 1);
    assert_eq!(p.meta.plots.len(), 1);
    assert_eq!(p.meta.max_intents_per_eval, 2);
    assert_eq!(p.layout.intent_cap, 2);
    assert_eq!(p.meta.max_bars_back, 30);
    assert!(p.meta.warmup_bars >= 30, "{}", p.meta.warmup_bars);
    assert_eq!(p.meta.entry_dirs.len(), 1);
    assert_eq!(p.meta.entry_dirs[0].dirs, pine_ir::entry_dir::LONG);
    assert_eq!(p.meta.spec_sha256, pine_spec::SPEC_SHA256);
    assert!(
        p.layout.rings.is_empty(),
        "the windows size the blocks, no ring"
    );
    // The init block: JNZ g first, then the constants.
    assert_eq!(p.code[0].op, op::JNZ);
}

#[test]
fn emit_realtime_layout_carries_a_journal_and_barclose_none() {
    let rt = sma_cross(Dialect::V6, ExecMode::Realtime);
    assert!(rt.layout.jrnl_bytes > 0);
    assert!(rt.layout.dirty_words > 0);
    let bc = sma_cross(Dialect::V6, ExecMode::BarClose);
    assert_eq!(bc.layout.jrnl_bytes, 0);
    assert_eq!(bc.layout.dirty_words, 0);
}

/// Runs the emitted SMA-cross on the VM with pine-std's bodies: `plot(f)` is the 10-bar SMA, and
/// an entry intent appears exactly on the bar the fast SMA crosses over the slow one.
#[test]
fn emit_sma_cross_executes() {
    use pine_ir::{barflag, IntentKind, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    for d in [Dialect::V5, Dialect::V6] {
        let p = sma_cross(d, ExecMode::BarClose);
        let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
        let mut arena = Arena::new(p.layout.total_bytes as usize);
        let mut vm = pine_vm::VmInstance::new(v, arena.slice());
        let mut ctx = Ctx { now: 0 };
        // 40 falling bars, then 40 rising: the fast SMA crosses over the slow one once.
        let closes: Vec<f64> = (0..80)
            .map(|i| {
                if i < 40 {
                    200.0 - i as f64
                } else {
                    160.0 + 3.0 * (i - 40) as f64
                }
            })
            .collect();
        let mut entries = Vec::new();
        for (i, &c) in closes.iter().enumerate() {
            let out = vm.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx);
            assert_eq!(out, StepOutcome::Ok, "{d:?} bar {i}");
            let sma10 = if i >= 9 {
                closes[i - 9..=i].iter().sum::<f64>() / 10.0
            } else {
                f64::NAN
            };
            let got = vm.plots()[0];
            if sma10.is_nan() {
                assert!(got.is_nan(), "{d:?} bar {i}: {got}");
            } else {
                assert!(
                    (got - sma10).abs() < 1e-9,
                    "{d:?} bar {i}: {got} vs {sma10}"
                );
            }
            if vm.intents().iter().any(|x| x.kind == IntentKind::Entry) {
                entries.push(i);
            }
        }
        let sma = |i: usize, n: usize| closes[i + 1 - n..=i].iter().sum::<f64>() / n as f64;
        let want: Vec<usize> = (30..80)
            .filter(|&i| sma(i, 10) > sma(i, 30) && sma(i - 1, 10) <= sma(i - 1, 30))
            .collect();
        assert_eq!(entries, want, "{d:?}");
        assert_eq!(want.len(), 1);
    }
}

/// pine-std's request: a nonzero constant `bars_back` of `time()` is refused at load (`3024`);
/// the default (0) compiles.
#[test]
fn emit_time_bars_back_constant_is_refused_at_load() {
    let o = default_opts(Dialect::V6, ExecMode::BarClose);
    let ok = "//@version=6\nindicator(\"t\")\nplot(time(\"60\"))\n";
    let bad = "//@version=6\nindicator(\"t\")\nplot(time(\"60\", bars_back = 2))\n";
    let r = compile_src(ok, &o);
    assert!(
        r.is_ok() || common::codes(&r) == vec![common::NOT_IMPLEMENTED],
        "{r:?}"
    );
    assert_eq!(common::codes(&compile_src(bad, &o)), vec![3024]);
}

/// Runs `src` for `closes.len()` bars and returns plot `k` per bar.
fn plots_of(src: &str, d: Dialect, mode: ExecMode, closes: &[f64], k: usize) -> Vec<f64> {
    use pine_ir::{barflag, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    let p = match compile_src(src, &default_opts(d, mode)) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}\n{src}"),
    };
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    let mut out = Vec::new();
    for (i, &c) in closes.iter().enumerate() {
        let o = vm.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx);
        assert_eq!(o, StepOutcome::Ok, "bar {i}: {:?}\n{src}", vm.last_fault());
        out.push(vm.plots()[k]);
    }
    out
}

/// Loops, `var`, history (constant and dynamic), `na` and the ternary compute what Pine
/// computes, in both dialects and both modes.
#[test]
fn emit_structures_execute() {
    let closes: Vec<f64> = (0..12).map(|i| 10.0 + i as f64).collect();
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let h = format!("//@version={}\nindicator(\"t\")\n", d as u8);
            // for: inclusive bound, three iterations.
            let s = format!("{h}v = 0.0\nfor j = 0 to 2\n    v += close\nplot(v)\n");
            let got = plots_of(&s, d, mode, &closes, 0);
            for (i, g) in got.iter().enumerate() {
                assert_eq!(*g, 3.0 * closes[i], "for {d:?} {mode:?} bar {i}");
            }
            // while with break.
            let s = format!(
                "{h}n = 0\nwhile true\n    n += 1\n    if n >= 4\n        break\nplot(n)\n"
            );
            assert!(
                plots_of(&s, d, mode, &closes, 0).iter().all(|&g| g == 4.0),
                "while"
            );
            // var accumulates once per bar.
            let s = format!("{h}var float acc = 0.0\nacc := acc + 1.0\nplot(acc)\n");
            let got = plots_of(&s, d, mode, &closes, 0);
            for (i, g) in got.iter().enumerate() {
                assert_eq!(*g, (i + 1) as f64, "var {d:?} {mode:?} bar {i}");
            }
            // constant and dynamic history, na before the first bar.
            let s = format!("{h}plot(close[2])\nk = bar_index % 3\nplot(close[k])\n");
            let a = plots_of(&s, d, mode, &closes, 0);
            let b = plots_of(&s, d, mode, &closes, 1);
            for i in 0..closes.len() {
                if i < 2 {
                    assert!(a[i].is_nan(), "close[2] at {i}");
                } else {
                    assert_eq!(a[i], closes[i - 2]);
                }
                assert_eq!(b[i], closes[i - i % 3], "close[k] at {i}");
            }
            // int history of a var int and the ternary over na.
            let s = format!("{h}var int c = 0\nc += 1\nx = na(c[1]) ? -1 : c[1]\nplot(x)\n");
            let got = plots_of(&s, d, mode, &closes, 0);
            for (i, g) in got.iter().enumerate() {
                let want = if i == 0 { -1.0 } else { i as f64 };
                assert_eq!(*g, want, "c[1] {d:?} {mode:?} bar {i}");
            }
        }
    }
}

/// A string variable (`S_PERSIST` into its own cell) and its history (a `STRING` ring).
#[test]
fn emit_strings_execute() {
    let closes: Vec<f64> = (0..8).map(|i| 10.0 + i as f64).collect();
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let h = format!("//@version={}\nindicator(\"t\")\n", d as u8);
            let s = format!(
                "{h}string s = bar_index % 2 == 0 ? \"even\" : \"odd\"\n\
                 plot(s == \"even\" ? 1 : 0)\nplot(s[1] == \"even\" ? 1 : 0)\n"
            );
            let r = compile_src(&s, &default_opts(d, mode));
            if common::codes(&r) == vec![common::NOT_IMPLEMENTED] {
                eprintln!("skipped: {:?}", r.err());
                continue; // a row the lane's pine-std does not serve yet
            }
            let a = plots_of(&s, d, mode, &closes, 0);
            let b = plots_of(&s, d, mode, &closes, 1);
            for i in 0..closes.len() {
                assert_eq!(a[i], if i % 2 == 0 { 1.0 } else { 0.0 }, "s at {i}");
                if i >= 1 {
                    assert_eq!(b[i], if i % 2 == 1 { 1.0 } else { 0.0 }, "s[1] at {i}");
                }
            }
        }
    }
}

/// §4.19 (I4): the same source, options and library cache give byte-identical programs.
#[test]
fn sema_is_deterministic_over_two_runs() {
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let a = pine_sema::canonical_bytes(&sma_cross(d, mode));
            let b = pine_sema::canonical_bytes(&sma_cross(d, mode));
            assert_eq!(a, b);
            assert_eq!(&a[..8], b"PVMPROG1");
        }
    }
    let v5 = pine_sema::canonical_bytes(&sma_cross(Dialect::V5, ExecMode::BarClose));
    let v6 = pine_sema::canonical_bytes(&sma_cross(Dialect::V6, ExecMode::BarClose));
    assert_ne!(v5, v6, "the dialect is part of the bytes");
}

/// impl-02 §4.15's worked emission (the parts that do not need the input fold below).
#[test]
fn emit_worked_example_shape() {
    use pine_ir::{input_flag, InputOverride, InputValue};
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    let p = compile_src(DEMO, &o).expect("compiles");
    assert_eq!(p.meta.kind, ScriptKind::Strategy);
    assert_eq!(p.meta.inputs.len(), 1);
    assert_eq!(p.meta.inputs[0].default_bits, 14);
    assert_eq!(p.meta.plots.len(), 1);
    assert_eq!(p.meta.max_intents_per_eval, 1);
    assert_eq!(p.layout.intent_cap, 1);
    assert_eq!(p.layout.n_plots, 1);
    assert_eq!(p.layout.states.len(), 1);
    assert!(p.layout.rings.is_empty(), "the window sizes the block");
    let ops: Vec<u8> = p.code.iter().map(|i| i.op).collect();
    let body = &ops[ops.iter().position(|&o| o == op::CALL_BUILTIN).unwrap()..];
    assert_eq!(
        body,
        [
            op::CALL_BUILTIN,
            op::F_MOV,
            op::F_GT,
            op::JZ,
            op::CALL_BUILTIN,
            op::CALL_BUILTIN,
            op::HALT
        ]
    );
    o.inputs = vec![InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(20),
    }];
    let q = compile_src(DEMO, &o).expect("compiles");
    assert_eq!(q.meta.inputs[0].default_bits, 20);
    assert_ne!(q.meta.inputs[0].flags & input_flag::OVERRIDDEN, 0);
}

const DEMO: &str = "//@version=6\nstrategy(\"demo\")\nlen = input.int(14, \"Length\")\n\
                    m = ta.sma(close, len)\nif close > m\n    strategy.entry(\"L\", strategy.long)\n\
                    plot(m)\n";

/// §4.15: `len` folds to 14 (the value in effect), so the window is the constant: a 152-byte
/// block, `max_bars_back` = `warmup_bars` = 14, and 200 bytes under `len = 20`.
#[test]
fn emit_worked_example_folds_the_input_window() {
    use pine_ir::{InputOverride, InputValue};
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    let p = compile_src(DEMO, &o).expect("compiles");
    assert_eq!(p.layout.states[0].bytes, 152, "align8(40 + 8 × 14)");
    assert_eq!(p.meta.max_bars_back, 14);
    assert_eq!(p.meta.warmup_bars, 14);
    o.inputs = vec![InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(20),
    }];
    assert_eq!(
        compile_src(DEMO, &o).expect("compiles").layout.states[0].bytes,
        200
    );
}

/// The overridden length sizes the window and the body: the plot is the 20-bar SMA.
#[test]
fn emit_input_length_executes_with_the_value_in_effect() {
    use pine_ir::{barflag, InputOverride, InputValue, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.inputs = vec![InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(20),
    }];
    let p = compile_src(DEMO, &o).expect("compiles");
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    let closes: Vec<f64> = (0..40).map(|i| 50.0 + (i * 7 % 11) as f64).collect();
    for (i, &c) in closes.iter().enumerate() {
        let out = vm.step_bar(&bar(i as u32, c, barflag::CONFIRMED), &mut ctx);
        assert_eq!(out, StepOutcome::Ok);
        let got = vm.plots()[0];
        if i < 19 {
            assert!(got.is_nan(), "bar {i}: {got}");
        } else {
            let want = closes[i - 19..=i].iter().sum::<f64>() / 20.0;
            assert!((got - want).abs() < 1e-9, "bar {i}: {got} vs {want}");
        }
    }
}
