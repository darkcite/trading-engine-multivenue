// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! R-P5 (CAR-R27-1 applied): the UDT field rows `ARRAY_SORT_AT` and `ARRAY_SORT_INDICES_AT`
//! through `pine_std::testing` — objects built with `Src::Obj` (`O_NEW` + `O_SET_*`, as sema
//! emits `T.new`), the field named by sema's folded selector `index | (Ty << 32)`
//! (`pin:02-udt-field-selector`). Covered: ascending / descending by an int field, a float field
//! with na fields and a string field; `sort_indices`; an na element
//! (`pin:04-udt-sort-na-object`); a selector sema cannot have built; zero allocation after
//! warm-up. R-P16 (CI-052, F108): the three `binary_search*` field rows are gone (TradingView
//! refuses the field argument at compile; pine-sema's `pin:02-bsearch-field-refused`).
//!
//! COPY-DOCTRINE: test support, may allocate (outside the counted window).

use pine_ir::*;
use pine_spec::ov;
use pine_std::coll_block_bytes;
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;
thread_local! { static ALLOCS: Cell<u64> = const { Cell::new(0) }; }
// SAFETY: forwards to `System`; the counter is thread-local, so parallel tests do not mix.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static GA: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.with(|c| c.get())
}

/// One call: overload, inputs, output lanes, site bytes.
struct C {
    id: OverloadId,
    ins: Vec<(Lane, Src)>,
    outs: Vec<Lane>,
    bytes: u32,
}

fn c(id: OverloadId, ins: Vec<(Lane, Src)>, outs: &[Lane]) -> C {
    C {
        id,
        ins,
        outs: outs.to_vec(),
        bytes: 0,
    }
}

fn it(x: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(x as u64))
}
fn ft(x: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(x.to_bits()))
}
fn st(x: &'static str) -> (Lane, Src) {
    (Lane::I64, Src::Str(x))
}
fn o(k: usize) -> (Lane, Src) {
    (Lane::I64, Src::Out(k as u16, 0))
}
const NA_I: (Lane, Src) = (Lane::I64, Src::Na);
const I: &[Lane] = &[Lane::I64];

/// The selector sema folds for field `i` of type `ty` (`pin:02-udt-field-selector`).
fn sel(i: u32, ty: Ty) -> (Lane, Src) {
    it(i64::from(i) | (ty as i64) << 32)
}
/// `type T` · `int id` · `float px` · `string name` · `int qty`.
const ID: (u32, Ty) = (0, Ty::Int);
const PX: (u32, Ty) = (1, Ty::Float);
const NAME: (u32, Ty) = (2, Ty::String);
const QTY: (u32, Ty) = (3, Ty::Int);
fn f(x: (u32, Ty)) -> (Lane, Src) {
    sel(x.0, x.1)
}

/// A fresh `T.new(id, px, name, qty)` (its own Object site); `qty` `None` = na.
fn obj(id: i64, px: f64, name: &'static str, qty: Option<i64>) -> (Lane, Src) {
    let q = match qty {
        Some(q) => it(q),
        None => NA_I,
    };
    let fields: Vec<(Lane, Src)> = vec![it(id), ft(px), st(name), q];
    (Lane::I64, Src::Obj(fields.leak()))
}

const CAP: u32 = 8;

/// `array.new<T>(0)` of a UDT (`__elem` = `elem::udt(0)`).
fn udt_array() -> C {
    C {
        id: ov::ARRAY_NEW,
        ins: vec![
            it(i64::from(elem::udt(0))),
            it(0),
            it(i64::from(CAP)),
            it(0),
            NA_I,
        ],
        outs: I.to_vec(),
        bytes: coll_block_bytes(CollKind::Array, Ty::Udt, Ty::Void, CAP, 16),
    }
}

/// The array of call `a`, then one push per element: the five objects of the module's tests.
/// Ids 0..4; px 3, na, 1, 2, 0.5; names "c" "a" "b" "bb" "B"; qty 5, 2, na, 2, 9.
fn five(calls: &mut Vec<C>) -> usize {
    let a = calls.len();
    calls.push(udt_array());
    for x in [
        obj(0, 3.0, "c", Some(5)),
        obj(1, f64::NAN, "a", Some(2)),
        obj(2, 1.0, "b", None),
        obj(3, 2.0, "bb", Some(2)),
        obj(4, 0.5, "B", Some(9)),
    ] {
        calls.push(c(ov::ARRAY_PUSH, vec![o(a), x], &[]));
    }
    a
}

fn sort(calls: &mut Vec<C>, a: usize, desc: bool, s: (Lane, Src)) {
    calls.push(c(ov::ARRAY_SORT_AT, vec![o(a), it(desc as i64), s], &[]));
}

/// `array.sort_indices(a, order, s)` and one `array.get` per index; the gets' call indices.
fn order_of(calls: &mut Vec<C>, a: usize, desc: bool, s: (Lane, Src), n: usize) -> Vec<usize> {
    let k = calls.len();
    calls.push(C {
        id: ov::ARRAY_SORT_INDICES_AT,
        ins: vec![
            it(i64::from(elem::scalar(Ty::Int))),
            it(0),
            it(i64::from(CAP)),
            o(a),
            it(desc as i64),
            s,
        ],
        outs: I.to_vec(),
        bytes: coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, CAP, 16),
    });
    (0..n)
        .map(|j| {
            calls.push(c(ov::ARRAY_GET, vec![o(k), it(j as i64)], I));
            calls.len() - 1
        })
        .collect()
}

/// Run `calls` for `bars` bars (v6); every call's first output per bar, or the first fault; the
/// allocations of the bars after the first.
fn run(calls: &[C], bars: usize) -> (Result<Vec<Vec<f64>>, FaultKind>, u64) {
    let cs: Vec<Call> = calls
        .iter()
        .map(|k| Call {
            ov: k.id,
            inputs: &k.ins,
            outputs: &k.outs,
            site_bytes: k.bytes,
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &cs,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0],
        ..TestCtx::default()
    };
    let mut out = vec![vec![f64::NAN; calls.len()]; bars];
    let mut before = 0;
    for (i, row) in out.iter_mut().enumerate() {
        if i == 1 {
            before = allocs();
        }
        let b = pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED);
        if h.step_bar(&b, &mut ctx) != StepOutcome::Ok {
            return (Err(h.last_fault().expect("a fault").0), 0);
        }
        for (k, call) in calls.iter().enumerate() {
            if !call.outs.is_empty() {
                row[k] = h.out(k, 0);
            }
        }
    }
    let n = if bars > 1 { allocs() - before } else { 0 };
    (Ok(out), n)
}

fn read(row: &[f64], at: &[usize]) -> Vec<i64> {
    at.iter().map(|&k| row[k] as i64).collect()
}

/// `sort_indices` by each field type, both orders: na field last ascending / first descending;
/// floats numeric, strings bytewise ("B" < "a" < "b" < "bb" < "c"), int ties by index; and zero
/// allocation after the first bar.
#[test]
fn udt_sort_indices_by_int_float_string_fields() {
    let mut calls = Vec::new();
    let a = five(&mut calls);
    let cases: [((u32, Ty), bool, [i64; 5]); 8] = [
        (PX, false, [4, 2, 3, 0, 1]),
        (PX, true, [1, 0, 3, 2, 4]),
        (NAME, false, [4, 1, 2, 3, 0]),
        (NAME, true, [0, 3, 2, 1, 4]),
        (QTY, false, [1, 3, 0, 4, 2]),
        (QTY, true, [2, 4, 0, 1, 3]),
        (ID, false, [0, 1, 2, 3, 4]),
        (ID, true, [4, 3, 2, 1, 0]),
    ];
    let gets: Vec<Vec<usize>> = cases
        .iter()
        .map(|&(s, desc, _)| order_of(&mut calls, a, desc, f(s), 5))
        .collect();
    let size = calls.len();
    calls.push(c(ov::ARRAY_SIZE, vec![o(a)], I));
    let (r, n) = run(&calls, 4);
    for row in r.expect("runs") {
        for (k, (s, desc, want)) in cases.iter().enumerate() {
            assert_eq!(read(&row, &gets[k]), want, "field {s:?} desc {desc}");
        }
        assert_eq!(row[size], 5.0, "sort_indices left the source alone");
    }
    assert_eq!(n, 0, "sort_indices by field allocated after warm-up");
}

/// `sort` in place by a float (na last / first), a string and an int field, each result read
/// back as the permutation `sort_indices` by the unique `id` field gives (`[k]` = the position
/// of the object with id `k`); zero allocation after the first bar.
#[test]
fn udt_sort_in_place_by_field() {
    let mut calls = Vec::new();
    let a = five(&mut calls);
    let steps: [((u32, Ty), bool, [i64; 5]); 5] = [
        // px ascending: o4 o2 o3 o0 o1(na).
        (PX, false, [3, 4, 1, 2, 0]),
        // name descending: c bb b a B = o0 o3 o2 o1 o4.
        (NAME, true, [0, 3, 2, 1, 4]),
        // id descending: o4 o3 o2 o1 o0.
        (ID, true, [4, 3, 2, 1, 0]),
        // px descending, na first: o1 o0 o3 o2 o4.
        (PX, true, [1, 0, 3, 2, 4]),
        // name ascending: B a b bb c = o4 o1 o2 o3 o0.
        (NAME, false, [4, 1, 2, 3, 0]),
    ];
    let mut gets = Vec::new();
    for &(s, desc, _) in &steps {
        sort(&mut calls, a, desc, f(s));
        gets.push(order_of(&mut calls, a, false, f(ID), 5));
    }
    let (r, n) = run(&calls, 4);
    for row in r.expect("runs") {
        for (k, (s, desc, want)) in steps.iter().enumerate() {
            assert_eq!(
                read(&row, &gets[k]),
                want,
                "step {k}: field {s:?} desc {desc}"
            );
        }
    }
    assert_eq!(n, 0, "sort by field allocated after warm-up");
}

/// `pin:04-udt-sort-na-object`: an na element is `ScriptRefused` for `sort` and `sort_indices`
/// (every element is a key; checked before any write).
#[test]
fn udt_na_element_is_script_refused() {
    let three = |calls: &mut Vec<C>| {
        calls.push(udt_array());
        for x in [obj(0, 1.0, "a", Some(1)), NA_I, obj(2, 3.0, "c", Some(3))] {
            calls.push(c(ov::ARRAY_PUSH, vec![o(0), x], &[]));
        }
    };
    let mut calls = Vec::new();
    three(&mut calls);
    sort(&mut calls, 0, false, f(PX));
    assert_eq!(run(&calls, 1).0.err(), Some(FaultKind::ScriptRefused));
    let mut calls = Vec::new();
    three(&mut calls);
    order_of(&mut calls, 0, true, f(QTY), 0);
    assert_eq!(run(&calls, 1).0.err(), Some(FaultKind::ScriptRefused));
}

/// A selector sema cannot have built is `Internal` (sema and std disagree): a field past the
/// object's, a `bool` field type for a sort, stray high bits.
#[test]
fn udt_bad_selector_is_internal() {
    for s in [sel(7, Ty::Int), sel(0, Ty::Bool), it(1 << 40)] {
        let mut calls = Vec::new();
        let a = five(&mut calls);
        calls.push(c(ov::ARRAY_SORT_AT, vec![o(a), it(0), s], &[]));
        assert_eq!(run(&calls, 1).0.err(), Some(FaultKind::Internal), "{s:?}");
    }
}

/// F110 (TV, `pin:04-udt-sort-ties`): `array.sort` by a field is stable in both directions —
/// `(k, id)` pushed `(2,1) (1,2) (2,3) (1,4) (2,5) (1,6)` (`k` in `qty`), ascending by `k` → ids
/// `2 4 6 1 3 5`, descending → `1 3 5 2 4 6` (not the ascending result reversed); each array is
/// read back as positions by the unique `id` (`[j]` = the position of id `j + 1`); zero
/// allocation after the first bar.
#[test]
fn udt_field_sort_is_stable_f110() {
    let mut calls = Vec::new();
    let mut arrays = Vec::new();
    for _ in 0..2 {
        let a = calls.len();
        calls.push(udt_array());
        for (k, id) in [(2, 1), (1, 2), (2, 3), (1, 4), (2, 5), (1, 6)] {
            calls.push(c(
                ov::ARRAY_PUSH,
                vec![o(a), obj(id, 0.0, "x", Some(k))],
                &[],
            ));
        }
        arrays.push(a);
    }
    sort(&mut calls, arrays[0], false, f(QTY));
    sort(&mut calls, arrays[1], true, f(QTY));
    let asc = order_of(&mut calls, arrays[0], false, f(ID), 6);
    let desc = order_of(&mut calls, arrays[1], false, f(ID), 6);
    let (r, n) = run(&calls, 4);
    for row in r.expect("runs") {
        // ids 2 4 6 1 3 5: id 1 at 3, id 2 at 0, id 3 at 4, id 4 at 1, id 5 at 5, id 6 at 2.
        assert_eq!(read(&row, &asc), [3, 0, 4, 1, 5, 2], "ascending");
        // ids 1 3 5 2 4 6: id 1 at 0, id 2 at 3, id 3 at 1, id 4 at 4, id 5 at 2, id 6 at 5.
        assert_eq!(read(&row, &desc), [0, 3, 1, 4, 2, 5], "descending");
    }
    assert_eq!(n, 0, "stable sort by field allocated after warm-up");
}
