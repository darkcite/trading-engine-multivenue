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

/// UDT objects (§4.14.7, T15): `O_NEW` + every field stored, field reads and stores, reference
/// aliasing, `copy()` (method and function forms), a `var` object and a nested one, the v6 `bool`
/// default, an `na` int field, and `string` field stores — one site, and the site dispatch.
#[test]
fn emit_objects_execute() {
    let closes: Vec<f64> = (0..8).map(|i| 10.0 + i as f64).collect();
    const PT: &str = "type Pt\n    float x = 0.0\n    int n = 1\n";
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let h = format!("//@version={}\nindicator(\"t\")\n{PT}", d as u8);
            // A `var` object accumulates across bars.
            let s = format!(
                "{h}var p = Pt.new(0.0)\np.x := p.x + close\np.n += 1\nplot(p.x)\nplot(p.n)\n"
            );
            let (a, b) = (
                plots_of(&s, d, mode, &closes, 0),
                plots_of(&s, d, mode, &closes, 1),
            );
            let mut acc = 0.0;
            for i in 0..closes.len() {
                acc += closes[i];
                assert_eq!(a[i], acc, "var object {d:?} {mode:?} bar {i}");
                assert_eq!(b[i], (i + 2) as f64, "var int field {d:?} {mode:?} bar {i}");
            }
            // References alias; `copy()` does not.
            let s = format!(
                "{h}a = Pt.new(close)\nb = a\nb.x := b.x * 2\nc = a.copy()\nc.x := 0.0\n\
                 e = Pt.copy(a)\nplot(a.x)\nplot(c.x)\nplot(e.x)\n"
            );
            let (a, c, e) = (
                plots_of(&s, d, mode, &closes, 0),
                plots_of(&s, d, mode, &closes, 1),
                plots_of(&s, d, mode, &closes, 2),
            );
            for i in 0..closes.len() {
                assert_eq!(a[i], 2.0 * closes[i], "alias {d:?} {mode:?} bar {i}");
                assert_eq!(c[i], 0.0, "copy {d:?} {mode:?} bar {i}");
                assert_eq!(e[i], 2.0 * closes[i], "T.copy {d:?} {mode:?} bar {i}");
            }
            // A `var` object nested in a `var` initialiser.
            let s = format!(
                "{h}type Box\n    Pt p\nvar b = Box.new(Pt.new(0.0))\nb.p.x := b.p.x + 1\nplot(b.p.x)\n"
            );
            let a = plots_of(&s, d, mode, &closes, 0);
            for (i, g) in a.iter().enumerate() {
                assert_eq!(*g, (i + 1) as f64, "nested {d:?} {mode:?} bar {i}");
            }
            // An `int` field without a default is `na`; a `bool` one is `false` in v6.
            let s = format!(
                "{h}type Q\n    int n\n    bool f\nq = Q.new()\nplot(na(q.n) ? 1 : 0)\nplot(q.f ? 1 : 0)\n"
            );
            let (a, b) = (
                plots_of(&s, d, mode, &closes, 0),
                plots_of(&s, d, mode, &closes, 1),
            );
            assert!(a.iter().all(|&g| g == 1.0), "na int field {d:?} {mode:?}");
            assert!(b.iter().all(|&g| g == 0.0), "bool field {d:?} {mode:?}");
            // `string` fields: defaults, a store through one site, and the site dispatch.
            let s = format!(
                "{h}type L\n    string s = \"a\"\nx = L.new()\ny = L.new(\"b\")\n\
                 z = close > 12 ? x : y\nz.s := \"c\"\nvar l = L.new()\n\
                 l.s := close > 13 ? \"hi\" : \"lo\"\n\
                 plot(x.s == \"c\" ? 1 : 0)\nplot(y.s == \"c\" ? 1 : 0)\n\
                 plot(y.s == \"b\" ? 1 : 0)\nplot(l.s == \"hi\" ? 1 : 0)\n"
            );
            let r = compile_src(&s, &default_opts(d, mode));
            if common::codes(&r) == vec![common::NOT_IMPLEMENTED] {
                eprintln!("skipped: {:?}", r.err());
                continue; // a string row the lane's pine-std does not serve yet
            }
            let p: Vec<Vec<f64>> = (0..4).map(|k| plots_of(&s, d, mode, &closes, k)).collect();
            for (i, &c) in closes.iter().enumerate() {
                let up = c > 12.0;
                assert_eq!(
                    p[0][i],
                    if up { 1.0 } else { 0.0 },
                    "x.s {d:?} {mode:?} bar {i}"
                );
                assert_eq!(
                    p[1][i],
                    if up { 0.0 } else { 1.0 },
                    "y.s {d:?} {mode:?} bar {i}"
                );
                assert_eq!(
                    p[2][i],
                    if up { 1.0 } else { 0.0 },
                    "y.s b {d:?} {mode:?} bar {i}"
                );
                assert_eq!(
                    p[3][i],
                    if c > 13.0 { 1.0 } else { 0.0 },
                    "l.s {d:?} {mode:?} {i}"
                );
            }
        }
    }
}

/// §4.14.7: a handle from two `Object` sites — `I_MOD` by 65 536, one compare, two
/// `S_PERSIST` / `O_SET_I` arms.
#[test]
fn emit_udt_string_field_dispatch_on_two_sites() {
    let s = "//@version=6\nindicator(\"t\")\ntype L\n    string s = \"a\"\nx = L.new()\n\
             y = L.new(\"b\")\nz = close > open ? x : y\nz.s := \"c\"\nplot(close)\n";
    let p = compile_src(s, &default_opts(Dialect::V6, ExecMode::BarClose)).expect("compiles");
    let m = p
        .code
        .iter()
        .position(|i| i.op == op::I_MOD)
        .expect("I_MOD");
    let k = p.code[m].b;
    // The divisor is the constant 65 536 (an init-block `I_CONST` of that slot).
    assert!(p
        .code
        .iter()
        .any(|i| i.op == op::I_CONST && i.dst == k && p.consts[usize::from(i.a)] == 65_536));
    let tail = &p.code[m..];
    let count = |o: u8| tail.iter().filter(|i| i.op == o).count();
    assert_eq!(count(op::I_EQ), 1, "one compare for two candidates");
    assert_eq!(count(op::S_PERSIST), 2, "two arms");
    assert_eq!(count(op::O_SET_I), 2, "two arms");
    // The two arms persist into the two sites' own staging cells.
    let cells: Vec<u16> = tail
        .iter()
        .filter(|i| i.op == op::S_PERSIST)
        .map(|i| i.b)
        .collect();
    assert_ne!(cells[0], cells[1]);
}

fn v6p(body: &str) -> PineProgram {
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    match compile_src(&src, &default_opts(Dialect::V6, ExecMode::BarClose)) {
        Ok(p) => p,
        Err(e) => panic!("{e:#?}\n{src}"),
    }
}

/// The init block's end: the target of the `JNZ g` at pc 0.
fn init_end(p: &PineProgram) -> usize {
    assert_eq!(p.code[0].op, op::JNZ);
    ((u32::from(p.code[0].dst) << 16) | u32::from(p.code[0].a)) as usize
}

/// I8: one `CALL_BUILTIN` per `Builtin` site, in `SiteId` order; one `O_NEW` per `Object` site.
#[test]
fn emit_one_instruction_per_site() {
    let p = v6p(
        "type P\n    float v = 0.0\na = ta.sma(close, 5)\nb = ta.sma(close, 9)\no = P.new(a)\n\
         plot(o.v + b)\n",
    );
    let sites: Vec<u16> = p
        .code
        .iter()
        .filter(|i| i.op == op::CALL_BUILTIN && i.dst != pine_ir::SiteId::NONE.0)
        .map(|i| i.dst)
        .chain(p.code.iter().filter(|i| i.op == op::O_NEW).map(|i| i.a))
        .collect();
    let mut sorted = sites.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), sites.len(), "no site emitted twice");
    assert_eq!(
        sorted,
        (0..p.layout.states.len() as u16).collect::<Vec<_>>()
    );
}

/// I9: no constant, move or builtin output writes a reserved slot.
#[test]
fn emit_never_writes_a_reserved_slot() {
    use pine_ir::{abi, argwin};
    let p = sma_cross(Dialect::V6, ExecMode::BarClose);
    for i in &p.code {
        match i.op {
            op::F_CONST | op::F_MOV => assert!(i.dst >= abi::RESERVED_F, "{i:?}"),
            op::I_CONST | op::I_MOV => assert!(i.dst >= abi::RESERVED_I, "{i:?}"),
            op::CALL_BUILTIN => {
                let w = usize::from(i.b);
                let n = usize::from(p.argwin[w]);
                let n_out = pine_std::BUILTINS.entries[usize::from(i.a)].n_out as usize;
                for e in &p.argwin[w + 1 + n - n_out..w + 1 + n] {
                    let floor = if argwin::is_i64(*e) {
                        abi::RESERVED_I
                    } else {
                        abi::RESERVED_F
                    };
                    assert!(argwin::slot(*e) >= floor, "{i:?}");
                }
            }
            _ => {}
        }
    }
}

/// Every `Const` slot is written once, in the init block between the guard and the body.
#[test]
fn emit_constants_are_written_once_in_the_init_block() {
    let p = v6p("x = close * 2.5 + 2.5\ny = bar_index % 7 + 7\nplot(x + y)\n");
    let end = init_end(&p);
    let mut seen = std::collections::BTreeSet::new();
    for i in &p.code[1..end] {
        if i.op == op::F_CONST || i.op == op::I_CONST {
            assert!(seen.insert((i.op, i.dst)), "{i:?} written twice");
        }
    }
    assert!(!seen.is_empty());
}

/// The `NA_CLEAR`s of class-V slots sit in the init block, before the guard's store.
#[test]
fn emit_class_v_slots_are_cleared_in_the_init_block() {
    let p = v6p("type P\n    float v = 0.0\nvar o = P.new()\no.v += 1\nplot(o.v)\n");
    let end = init_end(&p);
    let clears: Vec<usize> = (0..p.code.len())
        .filter(|&k| p.code[k].op == op::NA_CLEAR)
        .collect();
    assert!(!clears.is_empty(), "the var handle's validity is read");
    assert!(clears.iter().all(|&k| k < end));
}

#[test]
fn emit_mod_is_floored_opcode() {
    let p = v6p("a = bar_index % 3\nb = close % 2.5\nplot(a + b)\n");
    assert!(p.code.iter().any(|i| i.op == op::I_MOD));
    assert!(p.code.iter().any(|i| i.op == op::F_MOD));
}

#[test]
fn emit_float_div_sets_na_on_nonfinite_by_default() {
    use pine_ir::flg::NA_ON_NONFINITE;
    let p = v6p("a = close / open\nb = close % open\nc = math.pow(close, open)\nplot(a + b + c)\n");
    for o in [op::F_DIV, op::F_MOD, op::F_POW] {
        let i = p.code.iter().find(|i| i.op == o).expect("emitted");
        assert_ne!(i.flags & NA_ON_NONFINITE, 0, "{o:#x}");
    }
}

#[test]
fn emit_lowered_builtins_never_call() {
    let p = v6p(
        "x = bar_index > 2 ? close : na\na = na(x) ? 1.0 : 0.0\nb = nz(x)\nc = float(bar_index)\n\
         d = int(close)\ne = math.abs(close - open)\nf = math.sqrt(close)\n\
         g = math.pow(close, 2)\nh = math.max(close, open)\nplot(a + b + c + d + e + f + g + h)\n",
    );
    let calls = p.code.iter().filter(|i| i.op == op::CALL_BUILTIN).count();
    assert_eq!(calls, 1, "only the plot calls");
}

#[test]
fn emit_budget_too_small_is_refused() {
    let mut o = default_opts(Dialect::V6, ExecMode::BarClose);
    o.limits.insn_budget = 10;
    let src = "//@version=6\nindicator(\"t\")\nx = close * 2 + open * 3 - high / low\nplot(x)\n";
    assert_eq!(common::codes(&compile_src(src, &o)), vec![3023]);
}

#[test]
fn emit_plot_indices_put_hlines_last() {
    let p = v6p("hline(1.0)\nplot(close)\nplotcandle(open, high, low, close)\n");
    let kinds: Vec<pine_ir::PlotKind> = p.meta.plots.iter().map(|x| x.kind).collect();
    assert_eq!(kinds.len(), 6);
    assert_eq!(kinds[5], pine_ir::PlotKind::Hline);
    assert!(kinds[..5].iter().all(|&k| k != pine_ir::PlotKind::Hline));
}

#[test]
fn emit_for_counts_down_when_from_exceeds_to() {
    let s = "//@version=6\nindicator(\"t\")\ns = 0\nm = 0\nfor i = 5 to 1\n    s += i\n    \
             m := m * 10 + i\nplot(s)\nplot(m)\n";
    let closes = [10.0, 11.0];
    assert!(plots_of(s, Dialect::V6, ExecMode::BarClose, &closes, 0)
        .iter()
        .all(|&g| g == 15.0));
    assert!(plots_of(s, Dialect::V6, ExecMode::BarClose, &closes, 1)
        .iter()
        .all(|&g| g == 54_321.0));
}

#[test]
fn emit_budget_precedes_work_linear_calls() {
    let wma = pine_spec::lookup("ta.wma").expect("ta.wma");
    let lin = (wma.ov_lo..wma.ov_hi)
        .any(|o| pine_spec::overload(pine_ir::OverloadId(o)).sx & pine_spec::sx::WORK_LINEAR != 0);
    if !lin {
        eprintln!("skipped: ta.wma carries no sx::WORK_LINEAR in this pine-spec");
        return;
    }
    let p = v6p("plot(ta.wma(close, 300))\n");
    let c = p
        .code
        .iter()
        .position(|i| i.op == op::CALL_BUILTIN)
        .expect("call");
    let b = p.code[c - 1];
    assert_eq!((b.op, b.a, b.b), (op::BUDGET, 300, 0));
    let q = v6p("plot(ta.sma(close, 300))\n");
    assert!(!q.code.iter().any(|i| i.op == op::BUDGET));
}

#[test]
fn emit_captures_are_never_eliminated() {
    let p = v6p("x = close * 3.0\ny = x - open\nplot(y)\n");
    assert!(p.code.iter().any(|i| i.op == op::F_MUL));
    assert!(p.code.iter().any(|i| i.op == op::F_SUB));
}

#[test]
fn emit_var_guard_flag_is_varip_for_varip() {
    let p = v6p("varip int n = 0\nn += 1\nplot(n)\n");
    let end = init_end(&p);
    // The guard tests its flag (`JZ f → init`); the flag lives in the varip tail.
    let guard = p.code[end..]
        .iter()
        .find(|i| i.op == op::JZ || i.op == op::JNZ)
        .expect("the varip declaration's guard");
    assert!(
        u32::from(guard.b) >= p.layout.i64_varip_from,
        "flag {} below the varip tail {}",
        guard.b,
        p.layout.i64_varip_from
    );
}

#[test]
fn emit_string_stores_persist_into_own_cells() {
    let p = v6p("s = close > open ? \"u\" : \"d\"\nt = s\nt := \"x\"\nplot(s == t ? 1 : 0)\n");
    // I11: each cell has exactly one destination slot.
    let mut by_cell: Vec<(u16, u16)> = p
        .code
        .iter()
        .filter(|i| i.op == op::S_PERSIST)
        .map(|i| (i.b, i.dst))
        .collect();
    by_cell.sort_unstable();
    by_cell.dedup();
    let mut cells: Vec<u16> = by_cell.iter().map(|c| c.0).collect();
    cells.dedup();
    assert_eq!(cells.len(), by_cell.len(), "one dst per cell");
    assert_eq!(cells.len(), 2, "s and t own one cell each");
}

/// Collections (impl-02 §4.7.1, impl-04 §3.5): a creator's block is sized by
/// `pine_std::coll_block_bytes`, and `array.push` / `array.get` / `array.size` and `map.put` /
/// `map.get` execute on the VM in both dialects and both modes.
#[test]
fn emit_collections_execute() {
    let closes: Vec<f64> = (0..6).map(|i| 10.0 + i as f64).collect();
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let h = format!("//@version={}\nindicator(\"t\")\n", d as u8);
            let s = format!(
                "{h}var a = array.new_float(0)\narray.push(a, close)\n\
                 plot(array.size(a))\nplot(array.get(a, 0))\n\
                 var m = map.new<string, float>()\nmap.put(m, \"k\", close)\n\
                 plot(map.get(m, \"k\"))\n"
            );
            let p = match compile_src(&s, &default_opts(d, mode)) {
                Ok(p) => p,
                Err(e) => panic!("{d:?} {mode:?}: {e:#?}"),
            };
            let coll: Vec<_> = p
                .layout
                .states
                .iter()
                .filter(|s| s.kind == pine_ir::StateKind::Collection)
                .collect();
            assert_eq!(coll.len(), 2, "{:?}", p.layout.states);
            let o = default_opts(d, mode).limits;
            let arr = pine_std::coll_block_bytes(
                pine_ir::CollKind::Array,
                pine_ir::Ty::Float,
                pine_ir::Ty::Void,
                o.collection_cap,
                o.str_cell_bytes,
            );
            let map = pine_std::coll_block_bytes(
                pine_ir::CollKind::Map,
                pine_ir::Ty::Float,
                pine_ir::Ty::String,
                o.collection_cap,
                o.str_cell_bytes,
            );
            assert_eq!((coll[0].bytes, coll[1].bytes), (arr, map));
            let size = plots_of(&s, d, mode, &closes, 0);
            let first = plots_of(&s, d, mode, &closes, 1);
            let got = plots_of(&s, d, mode, &closes, 2);
            for i in 0..closes.len() {
                assert_eq!(size[i], (i + 1) as f64, "size {d:?} {mode:?} bar {i}");
                assert_eq!(first[i], closes[0], "get {d:?} {mode:?} bar {i}");
                assert_eq!(got[i], closes[i], "map {d:?} {mode:?} bar {i}");
            }
        }
    }
}

/// §4.14.1: a `ValueSrc::Call` value read three times is computed by one prelude call.
#[test]
fn emit_prelude_computes_referenced_call_values_once() {
    // The first `ValueSrc::Call` bar value the lane's pine-std serves (`hl2` waits for its row).
    let mut pick = None;
    for name in ["hl2", "hlc3", "ohlc4", "hlcc4", "ta.tr"] {
        let Some(e) = pine_spec::lookup(name) else {
            continue;
        };
        let Some(v) = e.values().iter().find(|v| v.versions.has(Dialect::V6)) else {
            continue;
        };
        let ov = pine_ir::OverloadId(v.bits as u16);
        if v.src == pine_spec::ValueSrc::Call
            && matches!(pine_std::served(ov), pine_std::Served::Body)
        {
            pick = Some((name, ov.0));
            break;
        }
    }
    let Some((name, ov)) = pick else {
        eprintln!("skipped: no served prelude value in the lane's pine-std");
        return;
    };
    let p = v6p(&format!(
        "plot({name})\nplot({name} * 2)\nplot({name} + 1)\n"
    ));
    let n = p
        .code
        .iter()
        .filter(|i| i.op == op::CALL_BUILTIN && i.a == ov)
        .count();
    assert_eq!(n, 1, "{name}");
    let s = format!("//@version=6\nindicator(\"t\")\nplot({name})\nplot({name} * 2)\n");
    let closes = [10.0, 12.0, 11.0];
    let a = plots_of(&s, Dialect::V6, ExecMode::BarClose, &closes, 0);
    let b = plots_of(&s, Dialect::V6, ExecMode::BarClose, &closes, 1);
    assert!(!a[2].is_nan(), "{name}: {a:?}");
    for i in 0..closes.len() {
        let same = (a[i].is_nan() && b[i].is_nan()) || b[i] == 2.0 * a[i];
        assert!(same, "{name} bar {i}: {} vs {}", b[i], a[i]);
    }
}

/// W1: v5 `const int / const int` folds to an `int`; a v5 `series` division and every v6
/// division are float (`F_I2F` + `F_DIV`).
#[test]
fn emit_int_div_follows_w1() {
    let closes = [1.0, 2.0];
    let one = |d: Dialect, body: &str| {
        let s = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
        plots_of(&s, d, ExecMode::BarClose, &closes, 0)
    };
    assert_eq!(one(Dialect::V5, "plot(7 / 2)\n"), vec![3.0, 3.0]);
    assert_eq!(one(Dialect::V6, "plot(7 / 2)\n"), vec![3.5, 3.5]);
    assert_eq!(
        one(Dialect::V5, "a = bar_index + 7\nplot(a / 2)\n"),
        vec![3.5, 4.0]
    );
    assert_eq!(
        one(Dialect::V6, "a = bar_index + 7\nplot(a / 2)\n"),
        vec![3.5, 4.0]
    );
    let s = "//@version=5\nindicator(\"t\")\na = bar_index + 7\nplot(a / 2)\n";
    let p = compile_src(s, &default_opts(Dialect::V5, ExecMode::BarClose)).expect("compiles");
    assert!(p.code.iter().any(|i| i.op == op::F_DIV));
    assert!(p.code.iter().any(|i| i.op == op::F_I2F));
}

/// `pin:02-for-step`: a `by` step that does not fold is refused by default (`3008`).
#[test]
fn emit_for_step_non_constant_is_refused_by_default() {
    for d in [Dialect::V5, Dialect::V6] {
        let s = format!(
            "//@version={}\nindicator(\"t\")\nst = bar_index % 2 + 1\nv = 0\n\
             for i = 0 to 10 by st\n    v += 1\nplot(v)\n",
            d as u8
        );
        let r = compile_src(&s, &default_opts(d, ExecMode::BarClose));
        assert_eq!(common::codes(&r), vec![3008], "{d:?}");
    }
}

/// W13 via `bool_na`: an `if` without `else` yielding a `bool` is `false` in v6, `na` in v5.
#[test]
fn emit_bool_if_without_else_is_false_in_v6_na_in_v5() {
    // v6 has no `na` bool (`na(b)` is v5 only), so each dialect reads `b` its own way.
    for (d, read, want) in [
        (Dialect::V5, "plot(na(b) ? -1 : b ? 1 : 0)", -1.0),
        (Dialect::V6, "plot(b ? 1 : 0)", 0.0),
    ] {
        let body = format!("b = if close > 1000\n    true\n{read}\n");
        let s = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
        let got = plots_of(&s, d, ExecMode::BarClose, &[10.0, 11.0], 0);
        assert_eq!(got, vec![want, want], "{d:?}");
    }
}

/// §4.12.2: an override changes the folded constant and its `InputDesc` (`default_bits`, the
/// `OVERRIDDEN` flag) and nothing else.
#[test]
fn emit_input_override_changes_only_the_value() {
    let src = "//@version=6\nindicator(\"t\")\nlen = input.int(14, \"L\")\nplot(close * len)\n";
    let base = default_opts(Dialect::V6, ExecMode::BarClose);
    let mut over = base.clone();
    over.inputs = vec![pine_ir::InputOverride {
        name: "len".to_owned(),
        value: pine_ir::InputValue::Int(20),
    }];
    let a = compile_src(src, &base).expect("compiles");
    let b = compile_src(src, &over).expect("compiles");
    assert_eq!(a.code, b.code);
    assert_eq!(a.argwin, b.argwin);
    assert_eq!(a.layout, b.layout);
    assert_eq!(a.consts.len(), b.consts.len());
    let diff: Vec<usize> = (0..a.consts.len())
        .filter(|&i| a.consts[i] != b.consts[i])
        .collect();
    assert_eq!(diff.len(), 1, "{:?} {:?}", a.consts, b.consts);
    let (da, db) = (&a.meta.inputs[0], &b.meta.inputs[0]);
    assert_eq!((da.default_bits, db.default_bits), (14, 20));
    assert_eq!(da.flags & pine_ir::input_flag::OVERRIDDEN, 0);
    assert_ne!(db.flags & pine_ir::input_flag::OVERRIDDEN, 0);
    assert_eq!((da.name, da.ty, da.kind), (db.name, db.ty, db.kind));
}

/// MG-23: a `for` bound is computed once before the loop in v5 and re-read in the head on every
/// iteration in v6 — a body that lowers the bound stops a v6 loop early.
#[test]
fn emit_for_bound_follows_mg23() {
    let body = "n = 3\nv = 0\nfor i = 0 to n\n    v += 1\n    n := 1\nplot(v)\n";
    for (d, want) in [(Dialect::V5, 4.0), (Dialect::V6, 2.0)] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let s = format!("//@version={}\nindicator(\"t\")\n{body}", d as u8);
            let got = plots_of(&s, d, mode, &[10.0, 11.0], 0);
            assert_eq!(got, vec![want, want], "{d:?} {mode:?}");
        }
    }
}

/// §4.17 `ValueSrc::Fold`: `syminfo.*` / `timeframe.*` fold from the options (chart 60 minutes,
/// mintick 0.1, pointvalue 1, ticker `BTCUSDT`) and lower as constants.
#[test]
fn emit_fold_values_execute() {
    let closes = [1.0, 2.0];
    for d in [Dialect::V5, Dialect::V6] {
        let h = format!("//@version={}\nindicator(\"t\")\n", d as u8);
        let cases = [
            ("syminfo.mintick", 0.1),
            ("syminfo.pointvalue", 1.0),
            ("timeframe.multiplier", 60.0),
            ("timeframe.isminutes ? 1 : 0", 1.0),
            ("timeframe.isintraday ? 1 : 0", 1.0),
            ("timeframe.isdwm ? 1 : 0", 0.0),
            ("timeframe.period == \"60\" ? 1 : 0", 1.0),
            ("syminfo.ticker == \"BTCUSDT\" ? 1 : 0", 1.0),
            ("syminfo.currency == \"USDT\" ? 1 : 0", 1.0),
            ("str.length(syminfo.tickerid) > 0 ? 1 : 0", 1.0),
        ];
        for (e, want) in cases {
            let src = format!("{h}plot({e})\n");
            let got = plots_of(&src, d, ExecMode::BarClose, &closes, 0);
            assert_eq!(got, vec![want; 2], "{e} v{}", d as u8);
        }
    }
}

/// Every `tests/scripts/*.pine` (dialect from its `//@version=`) compiled in both modes.
fn corpus() -> Vec<(String, ExecMode, PineProgram)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("tests/scripts")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "pine"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).expect("readable");
        let d = if src.starts_with("//@version=5") {
            Dialect::V5
        } else {
            Dialect::V6
        };
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            match compile_src(&src, &default_opts(d, mode)) {
                Ok(p) => out.push((name, mode, p)),
                Err(e) => panic!("{name} {mode:?}: {e:#?}"),
            }
        }
    }
    out
}

/// `p` and, depth first, every sub-program of it.
fn tree(p: &PineProgram) -> Vec<&PineProgram> {
    let mut v = vec![p];
    for s in &p.subprograms {
        v.extend(tree(&s.program));
    }
    v
}

/// §7.7: every test script, in both modes, passes the verifier with all its sub-programs.
#[test]
fn emit_every_test_script_verifies() {
    let c = corpus();
    assert!(c.len() >= 16, "{} programs", c.len());
    for (name, mode, p) in &c {
        for (k, q) in tree(p).into_iter().enumerate() {
            if let Err(e) = pine_vm::verify(q, &pine_std::BUILTINS) {
                panic!("{name} {mode:?} program {k}: rule {} pc {}", e.rule, e.pc);
            }
        }
    }
}

/// I9 (and pine-vm's rule 22): every `CALL_BUILTIN` window has `n ≥ n_out`, every entry inside
/// its lane, every output a user slot — not reserved, not a constant, not one of the window's
/// inputs, not another output — and the code ends in `HALT`; over the corpus and its children.
#[test]
fn emit_windows_are_in_range_and_outputs_are_user_slots() {
    use pine_ir::{abi, argwin};
    let mut windows = 0usize;
    for (name, mode, p) in &corpus() {
        for q in tree(p) {
            assert_eq!(
                q.code.last().map(|i| i.op),
                Some(op::HALT),
                "{name} {mode:?}"
            );
            // (is_i64, slot) of every constant slot.
            let consts: std::collections::BTreeSet<(bool, u16)> = q
                .code
                .iter()
                .filter_map(|i| match i.op {
                    op::F_CONST => Some((false, i.dst)),
                    op::I_CONST => Some((true, i.dst)),
                    _ => None,
                })
                .collect();
            for i in q.code.iter().filter(|i| i.op == op::CALL_BUILTIN) {
                windows += 1;
                let w = usize::from(i.b);
                let n = usize::from(q.argwin[w]);
                let n_out = pine_std::BUILTINS.entries[usize::from(i.a)].n_out as usize;
                assert!(n >= n_out, "{name} {mode:?}: {i:?}");
                let win = &q.argwin[w + 1..w + 1 + n];
                for e in win {
                    let len = if argwin::is_i64(*e) {
                        q.layout.i64_len
                    } else {
                        q.layout.f64_len
                    };
                    assert!(u32::from(argwin::slot(*e)) < len, "{name} {mode:?}: {i:?}");
                }
                let (ins, outs) = win.split_at(n - n_out);
                for (k, e) in outs.iter().enumerate() {
                    let (i64l, s) = (argwin::is_i64(*e), argwin::slot(*e));
                    let floor = if i64l {
                        abi::RESERVED_I
                    } else {
                        abi::RESERVED_F
                    };
                    assert!(s >= floor, "{name} {mode:?}: output reserved {i:?}");
                    assert!(
                        !consts.contains(&(i64l, s)),
                        "{name} {mode:?}: output a constant {i:?}"
                    );
                    assert!(
                        !ins.contains(e),
                        "{name} {mode:?}: output aliases an input {i:?}"
                    );
                    assert!(
                        !outs[..k].contains(e),
                        "{name} {mode:?}: two outputs alias {i:?}"
                    );
                }
            }
        }
    }
    assert!(windows > 0);
}

/// W3: v5 `and`/`or` are strict (`B_AND`/`B_OR`, the right operand always evaluated); v6 are
/// lazy (jumps, no `B_AND`/`B_OR`): a call-site `var` in the right operand advances only on the
/// bars the left one does not decide.
#[test]
fn emit_lazy_and_or_is_jumps_in_v6_and_strict_in_v5() {
    const F: &str = "f() =>\n    var int k = 0\n    k += 1\n    k\n";
    let closes = [1.0, 2.0, 3.0, 4.0, 5.0];
    for (op_txt, body, v5, v6) in [
        (
            "and",
            "b = bar_index >= 3 and f() >= 2\n",
            [0.0, 0.0, 0.0, 1.0, 1.0],
            [0.0, 0.0, 0.0, 0.0, 1.0],
        ),
        (
            "or",
            "b = bar_index < 3 or f() >= 2\n",
            [1.0; 5],
            [1.0, 1.0, 1.0, 0.0, 1.0],
        ),
    ] {
        for (d, want) in [(Dialect::V5, v5), (Dialect::V6, v6)] {
            let s = format!(
                "//@version={}\nindicator(\"t\")\n{F}{body}plot(b ? 1 : 0)\n",
                d as u8
            );
            assert_eq!(
                plots_of(&s, d, ExecMode::BarClose, &closes, 0),
                want,
                "{op_txt} {d:?}"
            );
            let p = compile_src(&s, &default_opts(d, ExecMode::BarClose)).expect("compiles");
            let strict = p
                .code
                .iter()
                .filter(|i| i.op == op::B_AND || i.op == op::B_OR)
                .count();
            assert_eq!(strict > 0, d == Dialect::V5, "{op_txt} {d:?}");
        }
    }
}

/// U14 `selected_only` (the default in both dialects): only the chosen arm runs — a call-site
/// `var` in the arm advances on the bars that select it, and no `F_SEL`/`I_SEL` is emitted.
#[test]
fn emit_ternary_evaluates_the_selected_arm_only_by_default() {
    const F: &str = "f() =>\n    var int k = 0\n    k += 1\n    k\n";
    let closes = [1.0, 2.0, 3.0, 4.0, 5.0];
    for d in [Dialect::V5, Dialect::V6] {
        let s = format!(
            "//@version={}\nindicator(\"t\")\n{F}x = bar_index >= 3 ? f() : 0\nplot(x)\n",
            d as u8
        );
        assert_eq!(
            plots_of(&s, d, ExecMode::BarClose, &closes, 0),
            vec![0.0, 0.0, 0.0, 1.0, 2.0],
            "{d:?}"
        );
        let p = compile_src(&s, &default_opts(d, ExecMode::BarClose)).expect("compiles");
        assert!(
            !p.code
                .iter()
                .any(|i| i.op == op::F_SEL || i.op == op::I_SEL),
            "{d:?}"
        );
    }
}
