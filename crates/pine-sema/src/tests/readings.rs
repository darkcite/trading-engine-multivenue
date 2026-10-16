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
    assert_eq!(r.pins.shadow_builtin, ShadowBuiltin::Refuse);
    assert_eq!(codes(src, &r), vec![3004]);
    r.pins.shadow_builtin = ShadowBuiltin::Shadow;
    assert_eq!(codes(src, &r), vec![ACCEPTED]);
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
    assert_eq!(codes(src_in, &r), vec![NOT_IMPLEMENTED]);
    r.pins.for_counter_shadow = ForCounterShadow::Refuse;
    assert_eq!(codes(src_in, &r), vec![3041]);
}

#[test]
fn readings_var_qualifier_both() {
    // v5: a reassigned variable whose values are all `const` is `series` by default and the
    // join (`const`) under `Join` (§4.4.2), so a `simple` parameter refuses it, then admits it.
    let src = "x = 5\nx := 6\nf(simple int a) => a\nplot(f(x))\n";
    let mut r = Readings::of(Dialect::V5);
    assert_eq!(r.pins.var_qualifier, VarQualifier::Series);
    assert_eq!(codes(src, &r), vec![3008]);
    r.pins.var_qualifier = VarQualifier::Join;
    assert_eq!(codes(src, &r), vec![ACCEPTED]);
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
    assert_eq!(ks, [(0, true), (1, true), (2, true)]);
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
