// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! Every switch and pin sema owns, in each reading (impl-02 §4.2.3, I16).

use pine_ir::{
    symbol_hash, ChartInfo, CompileLimits, CompileOpts, Dialect, ExecMode, FileId, SymTable,
};

use crate::pins::{FnGlobalAssign, ForCounterShadow, ShadowBuiltin, VarQualifier};
use crate::sw::Readings;
use crate::{compile_with, LibCache};

pub(super) fn opts(dialect: Dialect) -> CompileOpts {
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
            collection_cap: 64,
            str_persist_bytes: 4096,
            str_cell_bytes: 128,
            str_scratch_bytes: 8192,
            intent_cap: 32,
            max_frame_bytes: 16 << 20,
            active_tiers: 0b0111,
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
        Ok(_) => vec![ACCEPTED],
        Err(ds) => {
            // A row the lane's pine-std snapshot does not serve yet (§4.5 step 9) is the
            // snapshot's refusal: a refusal made only of those reads as `NOT_IMPLEMENTED`.
            let v: Vec<u16> = ds
                .iter()
                .filter(|d| !(d.code.0 == 3024 && d.msg.contains(" unserved=1")))
                .map(|d| d.code.0)
                .collect();
            if v.is_empty() {
                vec![NOT_IMPLEMENTED]
            } else {
                v
            }
        }
    }
}

const NOT_IMPLEMENTED: u16 = 3255;

/// An `Ok` program (T19): what every pass accepting a script now returns.
const ACCEPTED: u16 = 0;

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
    assert_eq!(r.pins.shadow_builtin, ShadowBuiltin::Shadow);
    assert_eq!(codes(src, &r), vec![ACCEPTED]);
    r.pins.shadow_builtin = ShadowBuiltin::Refuse;
    assert_eq!(codes(src, &r), vec![3004]);
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
    assert_eq!(codes(src, &r), vec![ACCEPTED]);
    let src_in = "s = 0\nfor x in array.from(1, 2)\n    x = 5\n    s += x\nplot(s)\n";
    assert_eq!(codes(src_in, &r), vec![ACCEPTED]);
    r.pins.for_counter_shadow = ForCounterShadow::Refuse;
    assert_eq!(codes(src_in, &r), vec![3041]);
}

#[test]
fn readings_var_qualifier_both() {
    // v5: a reassigned variable whose values are all `const` is the join (`const`) under the
    // default `Join` (TradingView F083) and `series` under `Series` (§4.4.2), so a `simple`
    // parameter admits it, then refuses it.
    let src = "x = 5\nx := 6\nf(simple int a) => a\nplot(f(x))\n";
    let mut r = Readings::of(Dialect::V5);
    assert_eq!(r.pins.var_qualifier, VarQualifier::Join);
    assert_eq!(codes(src, &r), vec![ACCEPTED]);
    r.pins.var_qualifier = VarQualifier::Series;
    assert_eq!(codes(src, &r), vec![3008]);
    r.pins.var_qualifier = VarQualifier::Join;
    // A `series` value assigned raises the join to `series` (the worklist's second round).
    let src = "x = 5\nx := bar_index\nf(simple int a) => a\nplot(f(x))\n";
    assert_eq!(codes(src, &r), vec![3008]);
}

// ---- U13 `ta_in_loop` (§4.7.4, `hoist.rs`) ---------------------------------------------------

fn ta_in_loop(body: &str, reading: u8) -> Result<crate::ir::Ir, pine_ir::Diag> {
    let mut rd = Readings::of(Dialect::V6);
    rd.ta_in_loop = reading;
    let mut ir = super::ir_shapes::try_ir(body, Dialect::V6, &rd)?;
    crate::hoist::run(&mut ir, &rd)?;
    Ok(ir)
}

fn sma_sites(ir: &crate::ir::Ir) -> usize {
    let sma = pine_ir::OverloadId(pine_spec::lookup("ta.sma").expect("ta.sma").ov_lo);
    super::ir_shapes::all(ir)
        .iter()
        .filter(|s| matches!(s, crate::ir::Stmt::Op(crate::ir::Op::Call { ov, .. }) if *ov == sma))
        .count()
}

const LOOP3: &str = "s = 0.0\nfor i = 0 to 2\n    s += ta.sma(close, 5)\nplot(s)\n";

#[test]
fn readings_ta_in_loop_per_call() {
    let ir = ta_in_loop(LOOP3, pine_spec::swr::TA_IN_LOOP_PER_CALL).expect("lowers");
    assert_eq!(sma_sites(&ir), 1, "one site, advanced every iteration");
    assert!(ir
        .body
        .0
        .iter()
        .any(|s| matches!(s, crate::ir::Stmt::Loop { .. })));
}

#[test]
fn readings_ta_in_loop_per_iteration_unrolls() {
    use crate::ir::{Op, Stmt};
    let ir = ta_in_loop(LOOP3, pine_spec::swr::TA_IN_LOOP_PER_ITERATION).expect("lowers");
    assert_eq!(sma_sites(&ir), 3, "one site per copy");
    let Some(Stmt::Unrolled { copies }) = ir
        .body
        .0
        .iter()
        .find(|s| matches!(s, Stmt::Unrolled { .. }))
    else {
        panic!("{:?}", ir.body)
    };
    // Each copy starts by setting the counter to that iteration's constant.
    let ks: Vec<(u64, bool)> = copies
        .iter()
        .map(|c| match c.0[0] {
            Stmt::Op(Op::Mov { a, .. }) => ir.const_of(a).expect("a constant"),
            ref x => panic!("{x:?}"),
        })
        .collect();
    // An `int` counter's constants are binary64s (CAR-R33-5).
    assert_eq!(
        ks,
        [
            (0f64.to_bits(), true),
            (1f64.to_bits(), true),
            (2f64.to_bits(), true)
        ]
    );
}

#[test]
fn readings_ta_in_loop_per_iteration_dynamic_is_refused() {
    let r = pine_spec::swr::TA_IN_LOOP_PER_ITERATION;
    for body in [
        "s = 0.0\nfor i = 0 to bar_index % 3\n    s += ta.sma(close, 5)\nplot(s)\n",
        "s = 0.0\ni = 0\nwhile i < 3\n    s += ta.sma(close, 5)\n    i += 1\nplot(s)\n",
        "s = 0.0\nfor i = 0 to 2000\n    s += ta.sma(close, 5)\nplot(s)\n",
    ] {
        let e = ta_in_loop(body, r).expect_err(body);
        assert_eq!(e.code.0, 3013, "{body}: {e:?}");
    }
    // A loop without a stateful call is left alone.
    let ir = ta_in_loop(
        "s = 0\nfor i = 0 to bar_index % 3\n    s += i\nplot(s)\n",
        r,
    )
    .expect("lowers");
    assert!(ir
        .body
        .0
        .iter()
        .any(|s| matches!(s, crate::ir::Stmt::Loop { .. })));
}

/// U13 inside a request's expression: the loop of the detached `E` block unrolls too, and the
/// compiled child holds one `ta.sma` site per copy.
#[test]
fn readings_ta_in_loop_per_iteration_unrolls_in_a_request() {
    use crate::ir::Stmt;
    let body = "f() =>\n    s = 0.0\n    for i = 0 to 2\n        s += ta.sma(close, 5)\n    s\n\
                plot(request.security(syminfo.tickerid, \"D\", f()[1]))\n";
    let r = pine_spec::swr::TA_IN_LOOP_PER_ITERATION;
    let ir = ta_in_loop(body, r).expect("lowers");
    assert_eq!(ir.requests.len(), 1);
    let e = &ir.requests[0].e;
    assert!(e
        .0
        .iter()
        .any(|s| matches!(s, Stmt::Unrolled { copies } if copies.len() == 3)));
    assert!(!e.0.iter().any(|s| matches!(s, Stmt::Loop { .. })));
    // The whole program: the child owns the three per-copy sites.
    let prog = |reading: u8| {
        let mut rd = Readings::of(Dialect::V6);
        rd.ta_in_loop = reading;
        let src = format!("//@version=6\nindicator(\"t\")\n{body}");
        let mut syms = SymTable::new();
        let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
        compile_with(&ast, syms, &opts(Dialect::V6), &mut LibCache::empty(), &rd).expect("compiles")
    };
    let (unrolled, per_call) = (prog(r), prog(pine_spec::swr::TA_IN_LOOP_PER_CALL));
    let states = |p: &pine_ir::PineProgram| p.subprograms[0].program.layout.states.len();
    assert_eq!(states(&per_call), 1);
    assert_eq!(states(&unrolled), 3, "one site per copy, in the child");
    assert_eq!(
        unrolled.layout.states.len(),
        per_call.layout.states.len(),
        "the parent is unchanged"
    );
}

/// The whole program of `body` under U13 reading `reading` (v6).
fn compiled(body: &str, reading: u8) -> pine_ir::PineProgram {
    let mut rd = Readings::of(Dialect::V6);
    rd.ta_in_loop = reading;
    let src = format!("//@version=6\nindicator(\"t\")\n{body}");
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(&src, FileId::ROOT, &mut syms).expect("parses");
    compile_with(&ast, syms, &opts(Dialect::V6), &mut LibCache::empty(), &rd).expect("compiles")
}

/// CAR-R33-9: `restart` is the default in both dialects; it keeps one site per in-loop call (no
/// unrolling), sets `RESTART` beside `IN_LOOP` and doubles the block to `2b + 8`
/// (`ta.sma(close, 5)`: `b = 40 + 8 × 5`); the program verifies against pine-std's table, whose
/// `state_bytes_min` rule 23 now measures against the view `b`. `per_call` and `per_iteration`
/// are unchanged.
#[test]
fn readings_ta_in_loop_restart_flags_and_doubles_the_block() {
    use pine_ir::state_flag::{IN_LOOP, RESTART};
    use pine_spec::swr;
    for d in [Dialect::V5, Dialect::V6] {
        assert_eq!(Readings::of(d).ta_in_loop, swr::TA_IN_LOOP_RESTART, "{d:?}");
    }
    let ir = ta_in_loop(LOOP3, swr::TA_IN_LOOP_RESTART).expect("lowers");
    assert_eq!(sma_sites(&ir), 1, "one site: restart never unrolls");
    let b = 40 + 8 * 5;
    let shape = |reading: u8| -> Vec<(u8, u32)> {
        let p = compiled(LOOP3, reading);
        pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
        p.layout.states.iter().map(|s| (s.flags, s.bytes)).collect()
    };
    assert_eq!(
        shape(swr::TA_IN_LOOP_RESTART),
        [(IN_LOOP | RESTART, 2 * b + 8)]
    );
    assert_eq!(shape(swr::TA_IN_LOOP_PER_CALL), [(IN_LOOP, b)]);
    assert_eq!(
        shape(swr::TA_IN_LOOP_PER_ITERATION),
        [(IN_LOOP, b), (IN_LOOP, b), (IN_LOOP, b)]
    );
}

/// CAR-R33-9 scope: every `ta.*` site in a loop body at any depth — nested `for`, `while`,
/// `for…in`, a function inlined into the body — restarts; a conditional call outside every loop
/// (`pin:06-cond-call-state`, B023 B041 B047) and a non-`ta` stateful row in a loop
/// (`pin:02-ta-in-loop-restart-scope`: `math.random`) do not. r39: `math.sum` restarts like `ta.*`
/// (TradingView F128, capture 08).
#[test]
fn readings_ta_in_loop_restart_scope() {
    use pine_ir::state_flag::{IN_LOOP, RESTART};
    let rd = Readings::of(Dialect::V6);
    let w3 = 40 + 8 * 3;
    let one = |body: &str| -> (u8, u32) {
        let s = super::ir_shapes::sites_of(body, &rd).expect(body);
        let b: Vec<_> = s
            .iter()
            .filter(|x| x.kind == pine_ir::StateKind::Builtin)
            .collect();
        assert_eq!(b.len(), 1, "{body}: {s:?}");
        (b[0].flags, b[0].bytes)
    };
    let restarted = (IN_LOOP | RESTART, 2 * w3 + 8);
    for body in [
        "s = 0.0\nfor i = 0 to 1\n    for j = 0 to 1\n        s += ta.sma(close, 3)\nplot(s)\n",
        "s = 0.0\ni = 0\nwhile i < 3\n    s += ta.sma(close, 3)\n    i += 1\nplot(s)\n",
        "a = array.from(1.0, 2.0)\ns = 0.0\nfor x in a\n    s += ta.sma(close + x, 3)\nplot(s)\n",
        "f(x) => ta.sma(x, 3)\ns = 0.0\nfor i = 0 to 2\n    s += f(close + i)\nplot(s)\n",
        "s = 0.0\nfor i = 0 to 2\n    if close > open\n        s += ta.sma(close, 3)\nplot(s)\n",
    ] {
        assert_eq!(one(body), restarted, "{body}");
    }
    // Outside every loop: unchanged (a conditional call is `pin:06-cond-call-state`'s).
    let cond = "s = 0.0\nif close > open\n    s := ta.sma(close, 3)\nplot(s)\n";
    assert_eq!(one(cond), (0, w3));
    // `math.sum` (r39, F128): restarted like `ta.*`.
    let sum = "s = 0.0\nfor i = 0 to 2\n    s += math.sum(close, 3)\nplot(s)\n";
    assert_eq!(one(sum), restarted);
    // Not `ta.*`: `per_call`, the block as before.
    let rnd = "s = 0.0\nfor i = 0 to 2\n    s += math.random(0, 1, 42)\nplot(s)\n";
    let (f, _) = one(rnd);
    assert_eq!(f, IN_LOOP, "math.random keeps drawing");
}

/// CAR-R33-9 in a request: the child numbers the loop's `ta.sma` with `RESTART`; the parent
/// keeps none of it.
#[test]
fn readings_ta_in_loop_restart_in_a_request() {
    use pine_ir::state_flag::{IN_LOOP, RESTART};
    let body = "f() =>\n    s = 0.0\n    for i = 0 to 2\n        s += ta.sma(close, 5)\n    s\n\
                plot(request.security(syminfo.tickerid, \"D\", f()[1]))\n";
    let p = compiled(body, pine_spec::swr::TA_IN_LOOP_RESTART);
    let child = &p.subprograms[0].program.layout.states;
    assert_eq!(child.len(), 1);
    assert_eq!(
        (child[0].flags, child[0].bytes),
        (IN_LOOP | RESTART, 2 * (40 + 8 * 5) + 8)
    );
    assert!(p.layout.states.iter().all(|s| s.flags & RESTART == 0));
}

// ---- `pin:06-cond-call-state` (§4.7.5, `hoist.rs`) -------------------------------------------

fn cond_call(body: &str, pin: crate::pins::CondCallState) -> Result<crate::ir::Ir, pine_ir::Diag> {
    let mut rd = Readings::of(Dialect::V6);
    rd.pins.cond_call_state = pin;
    let mut ir = super::ir_shapes::try_ir(body, Dialect::V6, &rd)?;
    crate::hoist::run(&mut ir, &rd)?;
    Ok(ir)
}

const COND: &str = "x = 0.0\nif bar_index % 2 == 0\n    x := ta.sma(close * 2, 3)\nplot(x)\n";

fn top_sma(ir: &crate::ir::Ir) -> bool {
    let sma = pine_ir::OverloadId(pine_spec::lookup("ta.sma").expect("ta.sma").ov_lo);
    ir.body
        .0
        .iter()
        .any(|s| matches!(s, crate::ir::Stmt::Op(crate::ir::Op::Call { ov, .. }) if *ov == sma))
}

#[test]
fn readings_cond_call_state_executed_only_does_not() {
    let ir = cond_call(COND, crate::pins::CondCallState::ExecutedOnly).expect("lowers");
    assert!(!top_sma(&ir), "the call stays in its block");
    assert_eq!(sma_sites(&ir), 1);
}

#[test]
fn readings_cond_call_state_every_bar_hoists() {
    use crate::ir::{BinOpK, Op, Stmt};
    let ir = cond_call(COND, crate::pins::CondCallState::EveryBar).expect("lowers");
    assert!(top_sma(&ir), "the call runs every bar");
    assert_eq!(sma_sites(&ir), 1);
    // Its argument slice (`close * 2`) moved with it, before the region.
    let k_if = ir
        .body
        .0
        .iter()
        .position(|s| matches!(s, Stmt::If { .. }))
        .expect("the region");
    assert!(ir.body.0[..k_if].iter().any(|s| matches!(
        s,
        Stmt::Op(Op::Bin {
            op: BinOpK::Mul,
            ..
        })
    )));
}

/// `EveryBar` inside a request's expression: the conditional call of the detached `E` block is
/// hoisted before its region there.
#[test]
fn readings_cond_call_state_every_bar_hoists_in_a_request() {
    use crate::ir::{Op, Stmt};
    let body = "f() =>\n    x = 0.0\n    if bar_index % 2 == 0\n        x := ta.sma(close * 2, 3)\n    x\n\
                plot(request.security(syminfo.tickerid, \"D\", f()[1]))\n";
    let sma = pine_ir::OverloadId(pine_spec::lookup("ta.sma").expect("ta.sma").ov_lo);
    let top = |ir: &crate::ir::Ir| {
        ir.requests[0]
            .e
            .0
            .iter()
            .any(|s| matches!(s, Stmt::Op(Op::Call { ov, .. }) if *ov == sma))
    };
    let ir = cond_call(body, crate::pins::CondCallState::ExecutedOnly).expect("lowers");
    assert!(!top(&ir), "the call stays in its block");
    let ir = cond_call(body, crate::pins::CondCallState::EveryBar).expect("lowers");
    assert!(top(&ir), "the call runs on every child bar");
}

#[test]
fn readings_cond_call_state_every_bar_unhoistable_is_refused() {
    let body = "y = 0.0\nx = 0.0\nif bar_index % 2 == 0\n    y := close * 2\n    x := ta.sma(y, 3)\nplot(x)\n";
    let e = cond_call(body, crate::pins::CondCallState::EveryBar).expect_err("unhoistable");
    assert_eq!(e.code.0, 3039, "{e:?}");
    assert!(e.msg.contains("ta.sma"));
}

/// `Join` keeps a reassigned all-`const` variable `const`, but its reads are never folded to the
/// declaration's value: `x = 5; x := 6; plot(x)` plots 6.
#[test]
fn readings_var_qualifier_join_reassigned_reads_the_slot() {
    use pine_ir::{barflag, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    let src = "//@version=5\nindicator(\"t\")\nx = 5\nx := 6\nplot(x)\n";
    let mut r = Readings::of(Dialect::V5);
    r.pins.var_qualifier = VarQualifier::Join;
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parses");
    let o = opts(Dialect::V5);
    let p = compile_with(&ast, syms, &o, &mut LibCache::empty(), &r).expect("compiles");
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let out = vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut Ctx { now: 0 });
    assert_eq!(out, StepOutcome::Ok);
    assert_eq!(vm.plots()[0], 6.0);
}

/// U13 `per_iteration` with `break` / `continue` in the unrolled copies: `continue` skips to the
/// next copy, `break` past the last; the program verifies and computes the loop's value.
#[test]
fn readings_ta_in_loop_per_iteration_break_continue_execute() {
    use pine_ir::{barflag, StepOutcome};
    use pine_vm::testkit::{bar, Arena, Ctx};
    let mut rd = Readings::of(Dialect::V6);
    rd.ta_in_loop = pine_spec::swr::TA_IN_LOOP_PER_ITERATION;
    // Iterations 0..4: 1 is skipped (`continue`), 3 breaks: only 0 and 2 add `sma(close, 1)`.
    let src = "//@version=6\nindicator(\"t\")\ns = 0.0\nn = 0\nfor i = 0 to 4\n    \
               m = ta.sma(close, 1)\n    if i == 1\n        continue\n    if i == 3\n        \
               break\n    s += m\n    n += 1\nplot(s)\nplot(n)\n";
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parses");
    let p = compile_with(&ast, syms, &opts(Dialect::V6), &mut LibCache::empty(), &rd)
        .expect("compiles");
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    for i in 0..4u32 {
        let c = 10.0 + f64::from(i);
        assert_eq!(
            vm.step_bar(&bar(i, c, barflag::CONFIRMED), &mut ctx),
            StepOutcome::Ok
        );
        assert_eq!(vm.plots()[1], 2.0, "iterations 0 and 2 accumulate");
        assert_eq!(vm.plots()[0], 2.0 * c, "bar {i}");
    }
}
