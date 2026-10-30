// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: load time only — pine-sema runs inside compile() and nothing it defines is
//! reached after compile() returns.
//!
//! impl-02 §4.8 / I10: `na` capability and slot classes over lowered IR.

use pine_ir::{Dialect, Lane};

use super::ir_shapes::{all, ir_with};
use crate::ir::{Arg, BinOpK, Ir, Op, SlotKind, Stmt, VSlot};
use crate::nacap::{self, Class, NaCap};
use crate::sw::Readings;

const SCRIPTS: &[&str] = &[
    "a = bar_index + 1\nb = a * 2\nc = na(a) ? 1 : 0\nplot(b + c)\n",
    "x = bar_index > 3 ? 1 : na\ny = x + 1\nz = nz(y, 0)\nplot(z)\n",
    "var int n = 0\nn += 1\nm = n[1]\nplot(na(m) ? -1 : m)\n",
    "u = close > open\nv = u and bar_index > 2\nplot(v ? 1 : 0)\n",
    "s = bar_index % 2 == 0 ? \"a\" : \"b\"\nplot(s == \"a\" ? 1 : 0)\n",
    "k = int(close)\nq = k / 2\nr = k % 3\nplot(q + r)\n",
];

fn classes(body: &str, d: Dialect) -> (Ir, NaCap) {
    let ir = ir_with(body, d, &Readings::of(d));
    let na = nacap::run(&ir, d);
    (ir, na)
}

/// The i64 operands an op reads, with the destination whose `VLD` would read them.
fn vld_reads(o: &Op) -> Option<(VSlot, Vec<VSlot>)> {
    match o {
        Op::Un { dst, a, .. } | Op::Mov { dst, a } => Some((*dst, vec![*a])),
        // `I_DIV` / `I_MOD` are emitted without `VLD` (they always write validity, B8).
        Op::Bin {
            op: BinOpK::Div { .. } | BinOpK::Mod { .. },
            ..
        } => None,
        Op::Bin { dst, a, b, .. } => Some((*dst, vec![*a, *b])),
        Op::Sel { dst, a, .. } => Some((*dst, vec![*a, *dst])),
        Op::Load {
            dst,
            c: Arg::Slot(s),
        } => Some((*dst, vec![*s])),
        _ => None,
    }
}

fn exempt(ir: &Ir, v: VSlot) -> bool {
    matches!(
        ir.slots[v.0 as usize].kind,
        SlotKind::Const | SlotKind::Reserved { .. }
    )
}

/// I10: an op that maintains its destination's validity (`VLD`, the destination is class A)
/// never reads the validity of a class-N i64 operand; every `Cmp` with a class-A operand
/// (`NA_AWARE`) reads two non-N operands. `Const` slots (written valid in the init block) and
/// reserved slots (written valid by the VM before every evaluation) are exempt by §4.8.
#[test]
fn nacap_no_vld_operand_is_class_n() {
    for d in [Dialect::V5, Dialect::V6] {
        for body in SCRIPTS {
            let (ir, na) = classes(body, d);
            let i64 = |v: VSlot| ir.slots[v.0 as usize].lane == Lane::I64;
            for s in all(&ir) {
                let Stmt::Op(o) = s else { continue };
                if let Some((dst, ops)) = vld_reads(o) {
                    if i64(dst) && na.a(dst) {
                        for v in ops {
                            if i64(v) && !exempt(&ir, v) {
                                assert_ne!(na.of(v), Class::N, "{d:?} {o:?}\n{body}");
                            }
                        }
                    }
                }
                if let Op::Cmp { a, b, .. } = o {
                    if i64(*a) && (na.a(*a) || na.a(*b)) {
                        for v in [*a, *b] {
                            if !exempt(&ir, v) {
                                assert_ne!(na.of(v), Class::N, "{d:?} {o:?}\n{body}");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// I10: no slot is class A that cannot be `na`. CAR-R33-5 (r39): an `int` is a binary64 in the f64
/// lane — its `na` is NaN, never a validity class — so the i64 classes are the `bool` / `string`
/// / `color` slots' (and the builtins' i64 `int` temporaries, `crate::marshal`). f64 slots are
/// always N.
#[test]
fn na_capability_classes_are_minimal() {
    let vars = |ir: &Ir, na: &NaCap| -> Vec<Class> {
        (0..ir.slots.len())
            .filter(|&i| ir.slots[i].kind == SlotKind::Var && ir.slots[i].lane == Lane::I64)
            .map(|i| na.of(VSlot(i as u32)))
            .collect()
    };
    for d in [Dialect::V5, Dialect::V6] {
        for body in SCRIPTS {
            let (ir, na) = classes(body, d);
            for i in 0..ir.slots.len() {
                let s = &ir.slots[i];
                if s.ty.ty == pine_ir::Ty::Int && s.kind == SlotKind::Var {
                    assert_eq!(s.lane, Lane::F64, "{d:?} an int variable\n{body}");
                }
                if s.lane == Lane::F64 {
                    assert_eq!(na.of(VSlot(i as u32)), Class::N);
                }
            }
        }
        // `s` (a string of two literal arms) is not A.
        let (ir, na) = classes(SCRIPTS[4], d);
        assert!(vars(&ir, &na).iter().all(|&c| c != Class::A), "{d:?}");
    }
    // A v6 bool is never na; in v5 `u and …` over a comparison is not A either.
    let (ir, na) = classes(SCRIPTS[3], Dialect::V6);
    assert!(vars(&ir, &na).iter().all(|&c| c != Class::A));
}
