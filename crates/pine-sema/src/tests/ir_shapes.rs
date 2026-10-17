// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §7.12 `ir_shapes.rs`: the IR `lower.rs` builds (§4.14.2–§4.14.6), operators first,
//! then the structures (T11).

use pine_ir::{Diag, Dialect, FileId, Lane, OverloadId, SymTable};
use pine_lang::Ast;

use crate::ir::{BinOpK, Block, CmpK, CondKind, Ir, LoopKind, Op, SlotKind, Stmt, UnOpK, VSlot};
use crate::pins::{ForStep, IntCast, NaCompare, NaCondition, OnceFlag, V5NumToBool};
use crate::sw::Readings;
use crate::{imports, infer, lower, mono, resolve, LibCache};

pub(super) fn try_ir(body: &str, d: Dialect, rd: &Readings) -> Result<Ir, Diag> {
    let v = d as u8;
    try_src(&format!("//@version={v}\nindicator(\"t\")\n{body}"), d, rd)
}

fn try_src(src: &str, d: Dialect, rd: &Readings) -> Result<Ir, Diag> {
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parses");
    let mut diags = Vec::new();
    let graph = imports::load(&ast, &mut syms, &mut LibCache::empty(), &mut diags);
    let res = resolve::resolve(&ast, &graph, &syms, rd, &mut diags);
    let t = infer::infer(&ast, &graph, &res, &mut syms, rd, 0b0111, &[], &mut diags);
    let files: Vec<Option<&Ast>> = vec![Some(&ast)];
    let tree = mono::build(&t, &files, &syms, &mut diags);
    diags.retain(|x| !(x.code.0 == 3024 && x.msg.contains(" unserved=1")));
    assert!(diags.is_empty(), "{diags:?}");
    lower::lower(&t, &tree, &res, &files, &syms, rd, d, KNOBS)
}

/// The tests' knobs: `collection_cap` 64, mintick 0.01.
const KNOBS: lower::Knobs<'static> = lower::Knobs {
    cap: 64,
    mintick_1e6: 10_000,
    pointvalue_1e6: 1_000_000,
    timeframe_s: 3600,
    strs: [pine_ir::Sym::NONE; 6],
    own: &[],
};

pub(super) fn ir_with(body: &str, d: Dialect, rd: &Readings) -> Ir {
    try_ir(body, d, rd).expect("lowers")
}

/// Every statement of `b`, depth first.
fn walk<'a>(b: &'a Block, out: &mut Vec<&'a Stmt>) {
    for s in &b.0 {
        out.push(s);
        match s {
            Stmt::If { then, els, .. } => {
                walk(then, out);
                walk(els, out);
            }
            Stmt::Loop {
                head, body, step, ..
            } => {
                walk(head, out);
                walk(body, out);
                walk(step, out);
            }
            Stmt::Unrolled { copies } => {
                for c in copies {
                    walk(c, out);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn all(ir: &Ir) -> Vec<&Stmt> {
    let mut v = Vec::new();
    walk(&ir.body, &mut v);
    v
}

fn block_ops(b: &Block) -> Vec<&Op> {
    b.0.iter()
        .filter_map(|s| match s {
            Stmt::Op(o) => Some(o),
            _ => None,
        })
        .collect()
}

fn ifs(ir: &Ir) -> Vec<&Stmt> {
    all(ir)
        .into_iter()
        .filter(|s| matches!(s, Stmt::If { .. }))
        .collect()
}

fn ir(body: &str, d: Dialect) -> Ir {
    ir_with(body, d, &Readings::of(d))
}

fn ops(ir: &Ir) -> Vec<&Op> {
    ir.body
        .0
        .iter()
        .filter_map(|s| match s {
            Stmt::Op(o) => Some(o),
            _ => None,
        })
        .collect()
}

fn op_id(name: &str) -> OverloadId {
    OverloadId(pine_spec::lookup(name).expect(name).ov_lo)
}

fn calls(ir: &Ir) -> Vec<OverloadId> {
    ops(ir)
        .iter()
        .filter_map(|o| match o {
            Op::Call { ov, .. } => Some(*ov),
            _ => None,
        })
        .collect()
}

#[test]
fn ir_shapes_operators_int_float_mix_widens() {
    let r = ir("x = bar_index + close\nplot(x)\n", Dialect::V6);
    let o = ops(&r);
    let Op::Un {
        op: UnOpK::I2f,
        dst: w,
        a,
    } = o[0]
    else {
        panic!("{o:?}");
    };
    assert!(matches!(
        r.slots[a.0 as usize].kind,
        SlotKind::Reserved { .. }
    ));
    let Op::Bin {
        op: BinOpK::Add,
        dst,
        a: l,
        ..
    } = o[1]
    else {
        panic!("{o:?}");
    };
    assert_eq!(l, w, "the int side is widened in place of the operand");
    assert_eq!(r.slots[dst.0 as usize].lane, Lane::F64);
}

#[test]
fn ir_shapes_operators_string_plus_calls_add_str() {
    let r = ir(
        "s = str.tostring(close) + \"!\"\nplot(close)\n",
        Dialect::V6,
    );
    let c = calls(&r);
    assert_eq!(c.len(), 3, "str.tostring, $op.add_str, plot");
    assert_eq!(c[1], op_id("$op.add_str"));
}

#[test]
fn ir_shapes_operators_int_division_follows_w1() {
    // v6 `fraction`: both sides widened, `F_DIV`.
    let r = ir("x = bar_index / 2\nplot(x)\n", Dialect::V6);
    let o = ops(&r);
    assert!(matches!(o[0], Op::Un { op: UnOpK::I2f, .. }), "{o:?}");
    assert!(matches!(o[1], Op::Un { op: UnOpK::I2f, .. }), "{o:?}");
    assert!(matches!(
        o[2],
        Op::Bin {
            op: BinOpK::Div {
                na_on_nonfinite: true
            },
            ..
        }
    ));
    // v5 `const_trunc`: two constants fold (§4.17) to `int` 3.
    let r = ir("x = 7 / 2\nplot(x)\n", Dialect::V5);
    let o = ops(&r);
    let Op::Mov { a, .. } = o[0] else {
        panic!("{o:?}")
    };
    assert_eq!(r.const_of(*a), Some((3, true)));
    assert_eq!(r.slots[a.0 as usize].lane, Lane::I64);
}

#[test]
fn ir_shapes_operators_mod_keeps_its_opcode_with_u15() {
    let r = ir(
        "x = close % 3.0\ny = bar_index % 3\nplot(x + y)\n",
        Dialect::V6,
    );
    let o = ops(&r);
    assert!(o.iter().any(|x| matches!(
        x,
        Op::Bin {
            op: BinOpK::Mod {
                na_on_nonfinite: true
            },
            ..
        }
    )));
    assert!(o.iter().any(|x| matches!(
        x,
        Op::Bin {
            op: BinOpK::Mod {
                na_on_nonfinite: false
            },
            ..
        }
    )));
}

#[test]
fn ir_shapes_operators_float_equality_follows_u06() {
    let body = "b = close == open\nplot(close)\n";
    let r = ir(body, Dialect::V6);
    assert!(ops(&r)
        .iter()
        .any(|x| matches!(x, Op::Cmp { op: CmpK::Eq, .. })));
    let mut rd = Readings::of(Dialect::V6);
    rd.float_eq_v6 = pine_spec::swr::FLOAT_EQ_V6_ROUND9;
    let r = ir_with(body, Dialect::V6, &rd);
    assert_eq!(calls(&r)[0], op_id("$op.eq_round9"));
    // v5 is always `F_EQ`.
    let r = ir_with(body, Dialect::V5, &Readings::of(Dialect::V5));
    assert!(calls(&r).iter().all(|&c| c != op_id("$op.eq_round9")));
}

#[test]
fn ir_shapes_operators_ne_follows_na_compare() {
    let body = "b = close != open\nplot(close)\n";
    let r = ir(body, Dialect::V6);
    assert!(ops(&r)
        .iter()
        .any(|x| matches!(x, Op::Cmp { op: CmpK::Ne, .. })));
    let mut rd = Readings::of(Dialect::V6);
    rd.pins.na_compare = NaCompare::NeTrue;
    let r = ir_with(body, Dialect::V6, &rd);
    let o = ops(&r);
    let i = o
        .iter()
        .position(|x| matches!(x, Op::Cmp { op: CmpK::Eq, .. }))
        .expect("an ==");
    assert!(matches!(o[i + 1], Op::Un { op: UnOpK::Not, .. }));
}

#[test]
fn ir_shapes_operators_strings_compare_by_call() {
    let r = ir(
        "b = str.tostring(close) == \"1\"\nplot(close)\n",
        Dialect::V6,
    );
    assert_eq!(calls(&r)[1], op_id("$op.eq_str"));
}

#[test]
fn ir_shapes_casts_and_lowered_builtins_are_opcodes() {
    let r = ir(
        "a = int(close)\nb = float(bar_index)\nc = na(close)\nd = nz(close)\n\
         e = math.max(close, open)\nf = math.abs(bar_index)\nplot(close)\n",
        Dialect::V6,
    );
    let o = ops(&r);
    assert!(o.iter().any(|x| matches!(
        x,
        Op::Un {
            op: UnOpK::F2i { round: false },
            ..
        }
    )));
    assert!(o.iter().any(|x| matches!(x, Op::Un { op: UnOpK::I2f, .. })));
    assert!(o.iter().any(|x| matches!(x, Op::NaCheck { .. })));
    assert!(o.iter().any(|x| matches!(x, Op::Nz { .. })));
    assert!(o.iter().any(|x| matches!(
        x,
        Op::Bin {
            op: BinOpK::Max,
            ..
        }
    )));
    assert!(o.iter().any(|x| matches!(x, Op::Un { op: UnOpK::Abs, .. })));
    assert_eq!(calls(&r).len(), 1, "only plot stays a call");
    let mut rd = Readings::of(Dialect::V6);
    rd.pins.int_cast = IntCast::RoundHalfAway;
    let r = ir_with("a = int(close)\nplot(close)\n", Dialect::V6, &rd);
    assert!(ops(&r).iter().any(|x| matches!(
        x,
        Op::Un {
            op: UnOpK::F2i { round: true },
            ..
        }
    )));
}

#[test]
fn ir_shapes_constants_are_one_slot_program_wide() {
    let r = ir(
        "a = close + 1.5\nb = open + 1.5\nplot(a + b)\n",
        Dialect::V6,
    );
    let consts: Vec<_> = r
        .const_list
        .iter()
        .filter(|c| c.1 == 1.5f64.to_bits())
        .collect();
    assert_eq!(consts.len(), 1);
}

#[test]
fn ir_shapes_input_source_aliases_the_series() {
    let r = ir("src = input.source(hlc3, \"S\")\nplot(src)\n", Dialect::V6);
    assert!(r
        .slots
        .iter()
        .any(|s| matches!(s.kind, SlotKind::Prelude { .. })));
}

#[test]
fn ir_shapes_user_calls_are_inlined_per_call_site() {
    let r = ir(
        "f(x, n) =>\n    m = x * n\n    m + 1\na = f(close, 2)\nb = f(open, 3)\nplot(a + b)\n",
        Dialect::V6,
    );
    let o = ops(&r);
    // Two expansions: `n` widened (int → float) and `x * n`, `+ 1` per site; no call of `f`.
    let muls = o
        .iter()
        .filter(|x| {
            matches!(
                x,
                Op::Bin {
                    op: BinOpK::Mul,
                    ..
                }
            )
        })
        .count();
    assert_eq!(muls, 2);
    assert_eq!(calls(&r).len(), 1, "only plot stays a call");
    // Each expansion's local `m` is its own slot, owned by its Instantiation.
    let ms: Vec<u32> = r
        .slots
        .iter()
        .filter(|s| s.kind == SlotKind::Var && s.inst != 0)
        .map(|s| s.inst)
        .collect();
    assert_eq!(ms, [1, 2]);
}

// ---- T11: structures ------------------------------------------------------------------------

/// The `F_CONST` value of a `Const` slot.
fn kf(ir: &Ir, v: VSlot) -> Option<f64> {
    ir.const_of(v).filter(|c| c.1).map(|c| f64::from_bits(c.0))
}

#[test]
fn ir_shapes_structures_switch_tests_arms_in_order() {
    let r = ir(
        "x = switch bar_index % 3\n    0 => 10.0\n    1 => 20.0\n    => 30.0\nplot(x)\n",
        Dialect::V6,
    );
    // The subject is evaluated once, before the first test.
    let mods = all(&r)
        .iter()
        .filter(|s| {
            matches!(
                s,
                Stmt::Op(Op::Bin {
                    op: BinOpK::Mod { .. },
                    ..
                })
            )
        })
        .count();
    assert_eq!(mods, 1);
    let Some(Stmt::If {
        then,
        els,
        kind: CondKind::Switch,
        ..
    }) = r.body.0.iter().find(|s| matches!(s, Stmt::If { .. }))
    else {
        panic!("{:?}", r.body);
    };
    let Op::Mov { a, .. } = block_ops(then)[0] else {
        panic!("{then:?}");
    };
    assert_eq!(kf(&r, *a), Some(10.0));
    // The second arm is tested only in the first arm's `else`; the default arm is the last `else`.
    let Some(Stmt::If {
        then: t2,
        els: e2,
        kind: CondKind::Switch,
        ..
    }) = els.0.iter().find(|s| matches!(s, Stmt::If { .. }))
    else {
        panic!("{els:?}");
    };
    let Op::Mov { a: a2, .. } = block_ops(t2)[0] else {
        panic!("{t2:?}");
    };
    assert_eq!(kf(&r, *a2), Some(20.0));
    let Op::Mov { a: a3, .. } = block_ops(e2)[0] else {
        panic!("{e2:?}");
    };
    assert_eq!(kf(&r, *a3), Some(30.0));
}

#[test]
fn ir_shapes_structures_for_in_rereads_size() {
    let r = ir(
        "a = array.new_float(3, 1.0)\ns = 0.0\nfor x in a\n    s += x\nplot(s)\n",
        Dialect::V6,
    );
    let Some(Stmt::Loop {
        head,
        body,
        kind: LoopKind::ForIn,
        ..
    }) = r.body.0.iter().find(|s| matches!(s, Stmt::Loop { .. }))
    else {
        panic!("{:?}", r.body);
    };
    let call_of = |o: &Op| match o {
        Op::Call { ov, .. } => Some(*ov),
        _ => None,
    };
    // The head re-reads the size every iteration; the body starts with the element read.
    assert_eq!(
        block_ops(head).iter().find_map(|o| call_of(o)),
        Some(op_id("array.size"))
    );
    assert_eq!(call_of(block_ops(body)[0]), Some(op_id("array.get")));
}

#[test]
fn ir_shapes_structures_loop_value_starts_na() {
    let r = ir("v = for i = 0 to 3\n    i * 2.0\nplot(v)\n", Dialect::V6);
    let at = r
        .body
        .0
        .iter()
        .position(|s| matches!(s, Stmt::Loop { .. }))
        .expect("a loop");
    let Stmt::Op(Op::NaSet { a: res }) = &r.body.0[at - 1] else {
        panic!("{:?}", r.body);
    };
    let Stmt::Loop {
        body, step, trip, ..
    } = &r.body.0[at]
    else {
        unreachable!()
    };
    assert_eq!(*trip, Some(4), "0 to 3 inclusive");
    let Some(Op::Mov { dst, .. }) = block_ops(body).last().copied() else {
        panic!("{body:?}");
    };
    assert_eq!(dst, res, "each iteration writes the loop's value");
    assert!(matches!(
        block_ops(step)[0],
        Op::Bin {
            op: BinOpK::Add,
            ..
        }
    ));
}

#[test]
fn ir_shapes_structures_for_counts_down_and_bounds_follow_mg23() {
    let r = ir("s = 0\nfor i = 5 to 1\n    s += i\nplot(s)\n", Dialect::V6);
    let Some(Stmt::Loop {
        head, step, trip, ..
    }) = r.body.0.iter().find(|s| matches!(s, Stmt::Loop { .. }))
    else {
        panic!()
    };
    assert_eq!(*trip, Some(5));
    assert!(matches!(block_ops(head)[0], Op::Cmp { op: CmpK::Ge, .. }));
    assert!(matches!(
        block_ops(step)[0],
        Op::Bin {
            op: BinOpK::Sub,
            ..
        }
    ));
    // A dynamic bound: re-evaluated in the head under v6 `each_iteration`, once under v5.
    let body = "s = 0\nfor i = 0 to bar_index % 4\n    s += i\nplot(s)\n";
    for (d, in_head) in [(Dialect::V6, true), (Dialect::V5, false)] {
        let r = ir(body, d);
        let Some(Stmt::Loop { head, .. }) =
            r.body.0.iter().find(|s| matches!(s, Stmt::Loop { .. }))
        else {
            panic!()
        };
        let m = block_ops(head).iter().any(|o| {
            matches!(
                o,
                Op::Bin {
                    op: BinOpK::Mod { .. },
                    ..
                }
            )
        });
        assert_eq!(m, in_head, "{d:?}");
        // The direction is a slot: the head compares SEL-selected bounds.
        assert!(block_ops(head).iter().any(|o| matches!(o, Op::Sel { .. })));
    }
}

#[test]
fn ir_shapes_structures_for_step_follows_pin() {
    let rd = Readings::of(Dialect::V6);
    let e = try_ir(
        "s = 0\nfor i = 0 to 4 by 0\n    s += i\nplot(s)\n",
        Dialect::V6,
        &rd,
    )
    .expect_err("a zero step");
    assert_eq!(e.code.0, 3007);
    let dynamic = "s = 0\nfor i = 0 to 4 by bar_index % 2 + 1\n    s += i\nplot(s)\n";
    // CI-004 / F085: the default `RuntimeFault` guards the loop with `runtime.error`.
    let r = ir_with(dynamic, Dialect::V6, &rd);
    let ov = pine_spec::ov::RUNTIME_ERROR;
    assert!(all(&r)
        .iter()
        .any(|s| matches!(s, Stmt::Op(Op::Call { ov: o, .. }) if *o == ov)));
    let mut refuse = rd;
    refuse.pins.for_step = ForStep::RefuseDynamic;
    let e = try_ir(dynamic, Dialect::V6, &refuse).expect_err("a dynamic step");
    assert_eq!(e.code.0, 3008);
    let mut abs = rd;
    abs.pins.for_step = ForStep::RuntimeAbs;
    let r = ir_with(dynamic, Dialect::V6, &abs);
    assert!(all(&r)
        .iter()
        .any(|s| matches!(s, Stmt::Op(Op::Un { op: UnOpK::Abs, .. }))));
}

#[test]
fn ir_shapes_structures_if_value_without_else() {
    // A float result is `na`; a v6 `bool` result is `false` (`bool_na = never`).
    let r = ir("x = if close > open\n    1.0\nplot(x)\n", Dialect::V6);
    let Some(Stmt::If {
        els,
        kind: CondKind::If,
        na_else: true,
        ..
    }) = ifs(&r).first().copied()
    else {
        panic!()
    };
    assert!(matches!(block_ops(els)[0], Op::NaSet { .. }));
    let r = ir(
        "b = if close > open\n    true\nplot(b ? 1 : 0)\n",
        Dialect::V6,
    );
    let Some(Stmt::If { els, .. }) = ifs(&r).first().copied() else {
        panic!()
    };
    let Op::Mov { a, .. } = block_ops(els)[0] else {
        panic!("{els:?}")
    };
    assert_eq!(r.const_of(*a), Some((0, true)));
}

#[test]
fn ir_shapes_structures_na_condition_follows_pin() {
    let body = "x = if close > open\n    1.0\nelse\n    2.0\nplot(x)\n";
    let mut rd = Readings::of(Dialect::V5);
    rd.pins.na_condition = NaCondition::NaResult;
    let r = ir_with(body, Dialect::V5, &rd);
    let at = r
        .body
        .0
        .iter()
        .position(|s| matches!(s, Stmt::If { .. }))
        .expect("if");
    assert!(matches!(r.body.0[at - 1], Stmt::Op(Op::NaSet { .. })));
    assert!(matches!(r.body.0[at], Stmt::If { na_else: false, .. }));
    let r = ir(body, Dialect::V5);
    let at = r
        .body
        .0
        .iter()
        .position(|s| matches!(s, Stmt::If { .. }))
        .expect("if");
    assert!(!matches!(r.body.0[at - 1], Stmt::Op(Op::NaSet { .. })));
    assert!(matches!(r.body.0[at], Stmt::If { na_else: true, .. }));
}

/// `pin:02-v5-num-to-bool` (F082: TradingView's v5 `bool(na)` is `na`): v5 `bool(f)` is the
/// comparison against zero, then `NA_CHECK f` selecting the `na` constant; v6 and the `False`
/// reading keep the bare comparison (an `na` operand gives `false`).
#[test]
fn v5_num_to_bool_f082() {
    let body = "float f = na\nb = bool(f)\nplot(b ? 1 : 0)\n";
    let shape = |r: &Ir| -> Option<bool> {
        let o = ops(r);
        let at = o
            .iter()
            .position(|x| matches!(x, Op::Cmp { op: CmpK::Ne, .. }))?;
        let &Op::Cmp { dst, a, .. } = o[at] else {
            return None;
        };
        match (o.get(at + 1), o.get(at + 2)) {
            (
                Some(Op::NaCheck { dst: c, a: x }),
                Some(Op::Sel {
                    dst: d,
                    a: na,
                    cond,
                }),
            ) if *x == a && *cond == *c && *d == dst => Some(r.const_of(*na) == Some((0, false))),
            _ => Some(false),
        }
    };
    assert_eq!(shape(&ir(body, Dialect::V5)), Some(true));
    assert_eq!(shape(&ir(body, Dialect::V6)), Some(false));
    let mut rd = Readings::of(Dialect::V5);
    rd.pins.v5_num_to_bool = V5NumToBool::False;
    assert_eq!(shape(&ir_with(body, Dialect::V5, &rd)), Some(false));
}

#[test]
fn ir_shapes_structures_and_or_follow_w3() {
    let body = "c = close > open and high > low\nplot(c ? 1 : 0)\n";
    // v6 `lazy`: the right operand is a LazyAndOr region.
    let r = ir(body, Dialect::V6);
    let Some(Stmt::If {
        then,
        kind: CondKind::LazyAndOr,
        ..
    }) = ifs(&r).first().copied()
    else {
        panic!("{:?}", r.body)
    };
    assert!(matches!(block_ops(then)[0], Op::Cmp { op: CmpK::Gt, .. }));
    // v5 `strict`: `B_AND` over both.
    let r = ir(body, Dialect::V5);
    assert!(ops(&r).iter().any(|o| matches!(
        o,
        Op::Bin {
            op: BinOpK::And,
            ..
        }
    )));
    assert!(!ifs(&r).iter().any(|s| matches!(
        s,
        Stmt::If {
            kind: CondKind::LazyAndOr,
            ..
        }
    )));
}

#[test]
fn ir_shapes_structures_ternary_follows_u14() {
    let body = "x = close > open ? close : open\nplot(x)\n";
    let r = ir(body, Dialect::V6);
    assert!(matches!(
        ifs(&r)[..],
        [Stmt::If {
            kind: CondKind::Ternary,
            ..
        }]
    ));
    let mut rd = Readings::of(Dialect::V6);
    rd.ternary_arms = pine_spec::swr::TERNARY_ARMS_BOTH;
    let r = ir_with(body, Dialect::V6, &rd);
    assert!(ifs(&r).is_empty());
    let o = ops(&r);
    let n = o.len();
    // `Mov r ← open; SEL r ← close if c`, then the declaration's store and the plot.
    assert!(
        matches!(
            o[n - 4..],
            [
                Op::Mov { .. },
                Op::Sel { .. },
                Op::Mov { .. },
                Op::Call { .. }
            ]
        ),
        "{o:?}"
    );
}

#[test]
fn ir_shapes_structures_var_varip_once_are_guards() {
    let r = ir(
        "var float acc = 0.0\nvarip int n = 0\nacc += close\nn += 1\nplot(acc + n)\n",
        Dialect::V6,
    );
    let g: Vec<bool> = ifs(&r)
        .iter()
        .map(|s| match s {
            Stmt::If {
                cond,
                then,
                els,
                kind: CondKind::Guard,
                ..
            } => {
                assert!(then.0.is_empty());
                assert!(matches!(
                    block_ops(els).last(),
                    Some(Op::Load { dst, .. }) if dst == cond
                ));
                match r.slots[cond.0 as usize].kind {
                    SlotKind::Flag { varip } => varip,
                    k => panic!("{k:?}"),
                }
            }
            s => panic!("{s:?}"),
        })
        .collect();
    assert_eq!(g, [false, true]);
    assert!(r.slots.iter().any(|s| s.kind == SlotKind::Varip));
    // `once`: the flag per `pin:02-once-flag`, the condition inside the guard.
    let body = "x = 0\nonce close > open\n    x := 1\nplot(x)\n";
    for (pin, varip) in [(OnceFlag::VarLike, false), (OnceFlag::VaripLike, true)] {
        let mut rd = Readings::of(Dialect::V6);
        rd.pins.once_flag = pin;
        let r = ir_with(body, Dialect::V6, &rd);
        let Some(Stmt::If {
            cond,
            els,
            kind: CondKind::Guard,
            ..
        }) = ifs(&r).first().copied()
        else {
            panic!()
        };
        assert_eq!(r.slots[cond.0 as usize].kind, SlotKind::Flag { varip });
        assert!(els.0.iter().any(|s| matches!(
            s,
            Stmt::If {
                kind: CondKind::If,
                ..
            }
        )));
    }
}

#[test]
fn ir_shapes_structures_block_local_follows_u16() {
    let body = "if close > open\n    y = close * 2\n    plot(y)\n";
    let r = ir(body, Dialect::V6);
    let Some(Stmt::If { els, .. }) = ifs(&r).first().copied() else {
        panic!()
    };
    assert!(els.0.is_empty(), "carry: nothing on the skipping path");
    let mut rd = Readings::of(Dialect::V6);
    rd.block_local = pine_spec::swr::BLOCK_LOCAL_NA;
    let r = ir_with(body, Dialect::V6, &rd);
    let Some(Stmt::If { els, .. }) = ifs(&r).first().copied() else {
        panic!()
    };
    let Op::NaSet { a } = block_ops(els)[0] else {
        panic!("{els:?}")
    };
    assert_eq!(r.slots[a.0 as usize].kind, SlotKind::Var);
}

#[test]
fn ir_shapes_structures_break_continue_and_while() {
    let r = ir(
        "i = 0\nwhile i < 10\n    i += 1\n    if i == 3\n        continue\n    if i == 7\n        break\nplot(i)\n",
        Dialect::V6,
    );
    let st = all(&r);
    assert!(st.iter().any(|s| matches!(
        s,
        Stmt::Loop {
            kind: LoopKind::While,
            trip: None,
            ..
        }
    )));
    assert!(st.iter().any(|s| matches!(s, Stmt::Break)));
    assert!(st.iter().any(|s| matches!(s, Stmt::Continue)));
}

#[test]
fn ir_shapes_structures_sma_cross_strategy_lowers() {
    // The lane's end-to-end target, through lowering.
    let src = "//@version=6\nstrategy(\"x\")\nf = ta.sma(close, 10)\ns = ta.sma(close, 30)\nif ta.crossover(f, s)\n    strategy.entry(\"L\", strategy.long)\nif ta.crossunder(f, s)\n    strategy.close(\"L\")\nplot(f)\n";
    let rd = Readings::of(Dialect::V6);
    let mut r = try_src(src, Dialect::V6, &rd).expect("lowers");
    assert_eq!(ifs(&r).len(), 2);
    crate::hoist::run(&mut r, &rd).expect("hoists");
    let cx = crate::sites::SiteCtx {
        d: Dialect::V6,
        cap: 500,
        collection_cap: 64,
        str_cell_bytes: 128,
        udt_fields: &[],
    };
    let sites = crate::sites::number(&r, &cx).expect("numbers");
    // Two `ta.sma` windows, then `ta.crossover` and `ta.crossunder` (pre-order).
    assert_eq!(sites.len(), 4, "{sites:?}");
    assert_eq!((sites[0].bytes, sites[1].bytes), (40 + 8 * 10, 40 + 8 * 30));
    assert_eq!(r.entry_dirs().len(), 1);
}

// ---- T12: argument windows ------------------------------------------------------------------

fn call_args(ir: &Ir) -> Vec<(OverloadId, Vec<crate::ir::Arg>, Vec<VSlot>)> {
    all(ir)
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Op(Op::Call { ov, args, outs, .. }) => Some((*ov, args.clone(), outs.clone())),
            _ => None,
        })
        .collect()
}

fn rows(ov: OverloadId, d: Dialect) -> usize {
    pine_spec::params(pine_spec::overload(ov))
        .iter()
        .filter(|p| p.versions.has(d))
        .count()
}

#[test]
fn ir_shapes_windows_synthetics_come_first() {
    use crate::ir::Arg;
    use pine_ir::PlotKind;
    let r = ir(
        "plot(close)\nhline(1.0)\nplot(open)\na = array.new_float(3, 1.0)\nplot(array.get(a, 0))\n",
        Dialect::V6,
    );
    let c = call_args(&r);
    let plot_ix = |k: usize| match c[k].1[0] {
        Arg::Const { bits, .. } => bits,
        ref a => panic!("{a:?}"),
    };
    // `__plot`: code order, every `hline` after every other index.
    let (plot, hline) = (op_id("plot"), op_id("hline"));
    let ixs: Vec<(OverloadId, u64)> = (0..c.len())
        .filter(|&k| c[k].0 == plot || c[k].0 == hline)
        .map(|k| (c[k].0, plot_ix(k)))
        .collect();
    assert_eq!(ixs, [(plot, 0), (hline, 3), (plot, 1), (plot, 2)]);
    let kinds: Vec<PlotKind> = r.plots.iter().map(|p| p.0).collect();
    assert_eq!(
        kinds,
        [
            PlotKind::Plot,
            PlotKind::Plot,
            PlotKind::Plot,
            PlotKind::Hline
        ]
    );
    // A collection creator: `__elem`, `__key` (0 for an array), `__cap`, then the Pine inputs.
    let (_, args, outs) = c
        .iter()
        .find(|x| x.0 == op_id("array.new_float"))
        .expect("the creator");
    let elem = u64::from(r.slots[outs[0].0 as usize].ty.a);
    let consts: Vec<u64> = args[..3]
        .iter()
        .map(|a| match a {
            Arg::Const { bits, .. } => *bits,
            a => panic!("{a:?}"),
        })
        .collect();
    assert_eq!(consts, [elem, 0, 64]);
    assert_eq!(args.len(), rows(op_id("array.new_float"), Dialect::V6));
}

#[test]
fn ir_shapes_windows_defaults_are_const_slots() {
    use crate::ir::Arg;
    let r = ir("plot(bar_index)\n", Dialect::V6);
    let c = call_args(&r);
    let (ov, args, _) = &c[0];
    assert_eq!(*ov, op_id("plot"));
    // One input per row of the dialect; the series is the int widened into a temporary.
    assert_eq!(args.len(), rows(*ov, Dialect::V6));
    let Arg::Slot(s) = args[1] else {
        panic!("{:?}", args[1])
    };
    assert_eq!(r.slots[s.0 as usize].kind, SlotKind::Temp);
    assert!(matches!(ops(&r)[0], Op::Un { op: UnOpK::I2f, .. }));
    // Every omitted parameter is a constant (a `Const` slot, a spec string) or a built-in value.
    for a in &args[2..] {
        match a {
            Arg::Slot(v) => assert!(
                matches!(
                    r.slots[v.0 as usize].kind,
                    SlotKind::Const | SlotKind::Reserved { .. } | SlotKind::Prelude { .. }
                ),
                "{a:?} {:?}",
                r.slots[v.0 as usize]
            ),
            Arg::Str(_) | Arg::Const { .. } => {}
        }
    }
}

// ---- T12: history and user-function arguments -----------------------------------------------

#[test]
fn ir_shapes_history_reads_slots_and_materialises_expressions() {
    use crate::ir::K as Off;
    let r = ir(
        "a = close[0]\nb = close[2]\nc = (high - low)[1]\nn = bar_index % 3\nd = close[n]\nplot(a + b + c + d)\n",
        Dialect::V6,
    );
    let h: Vec<(VSlot, Off)> = ops(&r)
        .iter()
        .filter_map(|o| match o {
            Op::Hist { src, k, .. } => Some((*src, *k)),
            _ => None,
        })
        .collect();
    assert_eq!(h.len(), 3, "`close[0]` is the slot itself");
    assert!(matches!(
        r.slots[h[0].0 .0 as usize].kind,
        SlotKind::Reserved { .. }
    ));
    assert_eq!(h[0].1, Off::Const(2));
    // `(high - low)[1]` reads the temporary the subtraction writes.
    assert_eq!(r.slots[h[1].0 .0 as usize].kind, SlotKind::Temp);
    assert_eq!(h[1].1, Off::Const(1));
    // A dynamic offset goes through `NZ_I kk, k, −1`.
    let Off::Dyn(kk) = h[2].1 else {
        panic!("{:?}", h[2])
    };
    assert!(ops(&r)
        .iter()
        .any(|o| matches!(o, Op::Nz { dst, .. } if *dst == kk)));
}

#[test]
fn ir_shapes_user_calls_bind_named_and_default_arguments() {
    let r = ir(
        "f(x, n = 2, m = 3.0) => x * n + m\na = f(close, m = 1.5)\nplot(a)\n",
        Dialect::V6,
    );
    let o = ops(&r);
    // `+ m`: the named argument.
    let Some(Op::Bin {
        op: BinOpK::Add, b, ..
    }) = o
        .iter()
        .find(|x| {
            matches!(
                x,
                Op::Bin {
                    op: BinOpK::Add,
                    ..
                }
            )
        })
        .copied()
    else {
        panic!("{o:?}")
    };
    assert_eq!(kf(&r, *b), Some(1.5));
    let Some(Op::Bin {
        op: BinOpK::Mul,
        b: nb,
        ..
    }) = o
        .iter()
        .find(|x| {
            matches!(
                x,
                Op::Bin {
                    op: BinOpK::Mul,
                    ..
                }
            )
        })
        .copied()
    else {
        panic!("{o:?}")
    };
    // `n` keys as `int` (its default's type); the body widens it for `x * n`.
    let Some(Op::Un { a: two, .. }) = o
        .iter()
        .find(|x| matches!(x, Op::Un { op: UnOpK::I2f, dst, .. } if dst == nb))
        .copied()
    else {
        panic!("{o:?}")
    };
    assert_eq!(r.const_of(*two), Some((2, true)));
}

#[test]
fn ir_shapes_tuples_bind_outputs_in_order() {
    let r = ir(
        "[m, u, l] = ta.bb(close, 20, 2)\nf(x) => [x, x * 2]\n[a, b] = f(close)\nplot(m + u + l + a + b)\n",
        Dialect::V6,
    );
    // One call, three outputs, each stored into its binder in order.
    let c = call_args(&r);
    let (_, _, outs) = c.iter().find(|x| x.0 == op_id("ta.bb")).expect("ta.bb");
    assert_eq!(outs.len(), 3);
    let o = ops(&r);
    let k = o
        .iter()
        .position(|x| matches!(x, Op::Call { ov, .. } if *ov == op_id("ta.bb")))
        .expect("call");
    for (j, out) in outs.iter().enumerate() {
        let Op::Mov { dst, a } = o[k + 1 + j] else {
            panic!("{:?}", o[k + 1 + j])
        };
        assert_eq!(a, out);
        assert_eq!(r.slots[dst.0 as usize].kind, SlotKind::Var);
    }
    // The user tuple: `a` is `close` itself, `b` the product.
    let movs: Vec<(VSlot, VSlot)> = o
        .iter()
        .filter_map(|x| match x {
            Op::Mov { dst, a } => Some((*dst, *a)),
            _ => None,
        })
        .collect();
    let (_, a_src) = movs[3];
    assert!(matches!(
        r.slots[a_src.0 as usize].kind,
        SlotKind::Reserved { .. }
    ));
}

// ---- T12: objects and strings ---------------------------------------------------------------

const POINT: &str = "type P\n    float x\n    int n = 3\n    string s\n    bool b\n";

#[test]
fn ir_shapes_objects_new_stores_every_field() {
    let r = ir(
        &format!("{POINT}p = P.new(close, s = \"a\")\nplot(p.x + p.n)\n"),
        Dialect::V6,
    );
    let o = ops(&r);
    let k = o
        .iter()
        .position(|x| matches!(x, Op::ObjNew { .. }))
        .expect("O_NEW");
    let sets: Vec<(u16, VSlot)> = o[k..]
        .iter()
        .filter_map(|x| match x {
            Op::ObjSet { field, src, .. } => Some((*field, *src)),
            _ => None,
        })
        .collect();
    assert_eq!(sets.iter().map(|s| s.0).collect::<Vec<_>>(), [0, 1, 2, 3]);
    // `x` the argument; `n` its declared default; `s` through its staging slot; `b` v6 `false`.
    assert!(matches!(
        r.slots[sets[0].1 .0 as usize].kind,
        SlotKind::Reserved { .. }
    ));
    assert_eq!(r.const_of(sets[1].1), Some((3, true)));
    assert_eq!(
        r.slots[sets[2].1 .0 as usize].kind,
        SlotKind::Stage { site: 0, field: 2 }
    );
    assert!(o
        .iter()
        .any(|x| matches!(x, Op::Persist { dst, .. } if *dst == sets[2].1)));
    assert_eq!(r.const_of(sets[3].1), Some((0, true)));
    // Field reads.
    assert_eq!(
        o.iter().filter(|x| matches!(x, Op::ObjGet { .. })).count(),
        2
    );
}

#[test]
fn ir_shapes_objects_copy_is_shallow_with_string_cells() {
    let r = ir(
        &format!("{POINT}p = P.new()\nq = P.copy(p)\nq.x := 2.0\nplot(q.x)\n"),
        Dialect::V6,
    );
    let o = ops(&r);
    let news: Vec<usize> = (0..o.len())
        .filter(|&k| matches!(o[k], Op::ObjNew { .. }))
        .collect();
    assert_eq!(news.len(), 2, "two Object sites");
    // The copy reads each field of the source and stores it; the string field re-persists into
    // the copy's own staging cell (site 1).
    let after = &o[news[1]..];
    let gets = after
        .iter()
        .filter(|x| matches!(x, Op::ObjGet { .. }))
        .count();
    assert!(gets >= 4);
    assert!(after.iter().any(|x| matches!(
        x,
        Op::ObjSet { field: 2, src, .. }
            if r.slots[src.0 as usize].kind == SlotKind::Stage { site: 1, field: 2 }
    )));
    // A string field store on an arbitrary handle is lowered unstaged: refs.rs (§4.9, T15)
    // stages it through each candidate site's staging slot.
    let r = ir(
        &format!("{POINT}p = P.new()\np.s := \"z\"\nplot(close)\n"),
        Dialect::V6,
    );
    assert!(ops(&r).iter().any(|x| matches!(
        x,
        Op::ObjSet { field: 2, src, .. }
            if r.slots[src.0 as usize].kind == SlotKind::Const
    )));
}

#[test]
fn ir_shapes_strings_variables_persist_every_store() {
    let r = ir(
        "s = \"a\"\ns := s + \"b\"\nvar string t = \"c\"\nplot(close, title = \"x\")\n",
        Dialect::V6,
    );
    let p: Vec<VSlot> = all(&r)
        .iter()
        .filter_map(|s| match s {
            Stmt::Op(Op::Persist { dst, .. }) => Some(*dst),
            _ => None,
        })
        .collect();
    assert_eq!(p.len(), 3, "declaration, `:=`, the `var` initialiser");
    assert!(p.iter().all(|v| r.slots[v.0 as usize].cell));
}

// ---- amendment A6: entry directions ---------------------------------------------------------

#[test]
fn ir_shapes_entry_dirs_list_constant_ids() {
    use pine_ir::entry_dir::{LONG, SHORT};
    let src = "//@version=6\nstrategy(\"x\")\nd = close > open ? strategy.long : strategy.short\nstrategy.entry(\"L\", strategy.long)\nstrategy.order(\"S\", strategy.short)\nstrategy.entry(\"B\", d)\nstrategy.entry(\"L\", strategy.long)\nstrategy.entry(str.tostring(bar_index), strategy.long)\n";
    let r = try_src(src, Dialect::V6, &Readings::of(Dialect::V6)).expect("lowers");
    let rows: Vec<(u64, u8)> = r.entry_dirs().iter().map(|e| (e.id.0, e.dirs)).collect();
    let mut want = vec![
        (pine_ir::str_key(b"L").0, LONG),
        (pine_ir::str_key(b"S").0, SHORT),
        (pine_ir::str_key(b"B").0, LONG | SHORT),
    ];
    want.sort_unstable();
    assert_eq!(
        rows, want,
        "sorted by id, one row per id; a non-constant id adds none"
    );
}

// ---- T14: sites -----------------------------------------------------------------------------

fn sites_of(body: &str, rd: &Readings) -> Result<Vec<crate::sites::Site>, Diag> {
    let mut ir = try_ir(body, Dialect::V6, rd)?;
    crate::hoist::run(&mut ir, rd)?;
    let cx = crate::sites::SiteCtx {
        d: Dialect::V6,
        cap: 500,
        collection_cap: 64,
        str_cell_bytes: 128,
        udt_fields: &[4],
    };
    crate::sites::number(&ir, &cx)
}

#[test]
fn sites_same_fn_two_call_sites_get_disjoint_state() {
    // impl-02 §4.7.3's worked example.
    let body = "f(src, len) =>\n    ta.sma(src, len) + ta.sma(src, len * 2)\na = f(close, 10)\nb = f(close, 10)\nc = f(open, 10)\nplot(a + b + c)\n";
    let s = sites_of(body, &Readings::of(Dialect::V6)).expect("numbers");
    let bytes: Vec<u32> = s.iter().map(|x| x.bytes).collect();
    assert_eq!(bytes, [120, 200, 120, 200, 120, 200]);
    assert!(s
        .iter()
        .all(|x| x.kind == pine_ir::StateKind::Builtin && x.flags == 0));
}

#[test]
fn sites_window_lengths_and_loops() {
    use pine_ir::state_flag::IN_LOOP;
    let rd = Readings::of(Dialect::V6);
    // A series length takes the cap; a loop body's site is IN_LOOP; an object site is sized by
    // its fields.
    let body = format!(
        "{POINT}n = bar_index % 5 + 1\na = ta.sma(close, n)\ns = 0.0\nfor i = 0 to 2\n    s += ta.sma(close, 3)\np = P.new()\nplot(a + s)\n"
    );
    let s = sites_of(&body, &rd).expect("numbers");
    assert_eq!(s[0].bytes, 40 + 8 * 500);
    assert_eq!(s[1].flags & IN_LOOP, IN_LOOP);
    assert_eq!(s[1].bytes, 40 + 8 * 3);
    assert_eq!(
        (s[2].kind, s[2].bytes),
        (pine_ir::StateKind::Object, 32 + 8 * 4)
    );
    // A non-positive constant length is `3007`.
    let e = sites_of("plot(ta.sma(close, 0))\n", &rd).expect_err("zero");
    assert_eq!(e.code.0, 3007);
}
