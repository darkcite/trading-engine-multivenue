// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §7.11 — collections (T32, T33 core): the block sizes the creators accept (I32), stale
//! references and views (I15), the §4.16.5 fault table's core rows, W15 negative indices (I16),
//! writes through a view reaching the owner, and 0 B/op on the accessors — through
//! `pine_std::testing`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::ov;
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_std::{coll_block_bytes, VIEW_BLOCK_BYTES};
use pine_vm::testkit::bar;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
}
// SAFETY: forwards to `System`, counting calls only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ON.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

/// One call: overload, inputs, output lanes, site bytes, guard column.
struct C {
    id: OverloadId,
    ins: Vec<(Lane, Src)>,
    outs: Vec<Lane>,
    bytes: u32,
    guard: Option<u16>,
}

fn c(id: OverloadId, ins: Vec<(Lane, Src)>, outs: &[Lane]) -> C {
    C {
        id,
        ins,
        outs: outs.to_vec(),
        bytes: 0,
        guard: None,
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
fn o(k: u16) -> (Lane, Src) {
    (Lane::I64, Src::Out(k, 0))
}
const NA_I: (Lane, Src) = (Lane::I64, Src::Na);
const I: &[Lane] = &[Lane::I64];
const F: &[Lane] = &[Lane::F64];

/// The fault `id` returns for `k`: pine-vm admits `StrArenaFull` / `PoolFull` only from rows
/// declaring `STR_ALLOC` / `POOL` (`pin:04-coll-undeclared-fault`); otherwise `Internal`.
fn declared(id: OverloadId, k: FaultKind) -> FaultKind {
    let fl = pine_spec::OVERLOADS[id.0 as usize].bf;
    match k {
        FaultKind::StrArenaFull if fl & pine_vm::bf::STR_ALLOC == 0 => FaultKind::Internal,
        FaultKind::PoolFull if fl & pine_vm::bf::POOL == 0 => FaultKind::Internal,
        _ => k,
    }
}

fn enc(t: Ty) -> i64 {
    elem::scalar(t) as i64
}

/// A creator call: `id` with `[__elem, __key, __cap]` then `rest`, its block `bytes`.
fn new(id: OverloadId, t: Ty, cap: u32, bytes: u32, rest: Vec<(Lane, Src)>) -> C {
    let mut ins = vec![it(enc(t)), it(0), it(cap as i64)];
    ins.extend(rest);
    C {
        id,
        ins,
        outs: I.to_vec(),
        bytes,
        guard: None,
    }
}

fn float_block(cap: u32) -> u32 {
    coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, cap, 16)
}

/// Run `calls` over `feed.len()` bars in `d`; every call's first output per bar, or the first
/// fault. Counts this thread's allocations in the steps after the first.
fn run(calls: &[C], feed: &[Vec<f64>], d: Dialect) -> (Result<Vec<Vec<f64>>, FaultKind>, u64) {
    let cs: Vec<Call> = calls
        .iter()
        .map(|k| Call {
            ov: k.id,
            inputs: &k.ins,
            outputs: &k.outs,
            site_bytes: k.bytes,
            guard: k.guard,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &cs,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: feed.first().cloned().unwrap_or_default(),
        ..TestCtx::default()
    };
    let mut out = Vec::new();
    let mut allocs = 0;
    for (i, f) in feed.iter().enumerate() {
        ctx.feed.copy_from_slice(f);
        let a0 = ALLOCS.load(Ordering::Relaxed);
        ON.with(|c| c.set(i > 0));
        let r = h.step_bar(&bar(i as u32, 1.0, barflag::CONFIRMED), &mut ctx);
        ON.with(|c| c.set(false));
        allocs += ALLOCS.load(Ordering::Relaxed) - a0;
        if r != StepOutcome::Ok {
            return (Err(h.last_fault().unwrap().0), allocs);
        }
        out.push(
            (0..calls.len())
                .map(|k| {
                    if calls[k].outs.is_empty() {
                        f64::NAN
                    } else {
                        h.out(k, 0)
                    }
                })
                .collect(),
        );
    }
    (Ok(out), allocs)
}

fn once(calls: &[C], d: Dialect) -> Result<Vec<f64>, FaultKind> {
    run(calls, &[vec![0.0]], d).0.map(|mut v| v.remove(0))
}

/// I32, the harness half (the full sweep is `coll::tests` in the crate): a creator given exactly
/// `coll_block_bytes` builds, one given less than its non-cell part (or under one word per string
/// cell) is `Internal`; the cell a body derives is the manifest's — a string of exactly
/// `str_cell_bytes` stores, one byte more is `StrArenaFull`.
#[test]
fn coll_block_bytes_matches_layout() {
    for cap in [0u32, 1, 2, 3, 7, 64, 4096] {
        let b = float_block(cap);
        assert_eq!(b, 32 + 8 * cap);
        let size = cap.min(3) as i64;
        let r = once(
            &[
                new(
                    ov::ARRAY_NEW_FLOAT,
                    Ty::Float,
                    cap,
                    b,
                    vec![it(size), ft(1.5)],
                ),
                c(ov::ARRAY_SIZE, vec![o(0)], I),
            ],
            Dialect::V6,
        )
        .unwrap();
        assert_eq!(r[1], size as f64);
        if cap > 0 {
            let r = once(
                &[new(
                    ov::ARRAY_NEW_FLOAT,
                    Ty::Float,
                    cap,
                    b - 8,
                    vec![it(0), ft(1.5)],
                )],
                Dialect::V6,
            );
            assert_eq!(r, Err(FaultKind::Internal), "cap {cap}");
        }
        let kinds = [
            (Ty::Int, Ty::Int),
            (Ty::String, Ty::Float),
            (Ty::String, Ty::String),
        ];
        for (k, v) in kinds {
            let b = coll_block_bytes(CollKind::Map, v, k, cap, 24);
            let m = C {
                id: ov::MAP_NEW,
                ins: vec![it(enc(v)), it(enc(k)), it(cap as i64)],
                outs: I.to_vec(),
                bytes: b,
                guard: None,
            };
            let r = once(&[m, c(ov::MAP_SIZE, vec![o(0)], I)], Dialect::V6).unwrap();
            assert_eq!(r[1], 0.0);
            if cap > 0 {
                let m = C {
                    id: ov::MAP_NEW,
                    ins: vec![it(enc(v)), it(enc(k)), it(cap as i64)],
                    outs: I.to_vec(),
                    bytes: 32 + 8 * (2 * cap + cap.max(1).next_power_of_two()) - 8,
                    guard: None,
                };
                assert_eq!(once(&[m], Dialect::V6), Err(FaultKind::Internal));
            }
        }
    }
    // A 16-byte cell: 16 bytes of SCRATCH string store, 17 are StrArenaFull.
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    assert_eq!(sb, 32 + 8 * 4 + 4 * 16);
    for (tail, want) in [
        ("ijklmnop", Ok(())),
        ("ijklmnopq", Err(FaultKind::StrArenaFull)),
    ] {
        let calls = [
            new(
                ov::ARRAY_NEW_STRING,
                Ty::String,
                4,
                sb,
                vec![it(2), st("const")],
            ),
            c(ov::OP_ADD_STR, vec![st("abcdefgh"), st(tail)], I),
            c(ov::ARRAY_SET, vec![o(0), it(1), o(1)], &[]),
            c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]),
            c(ov::ARRAY_SIZE, vec![o(0)], I),
        ];
        let r = once(&calls, Dialect::V6);
        match want {
            Ok(()) => assert_eq!(r.unwrap()[4], 3.0),
            Err(k) => assert_eq!(r, Err(declared(ov::ARRAY_SET, k))),
        }
    }
    // Under one word per string cell: Internal.
    let r = once(
        &[new(
            ov::ARRAY_NEW_STRING,
            Ty::String,
            4,
            32 + 8 * 4 + 8 * 3,
            vec![it(0), NA_I],
        )],
        Dialect::V6,
    );
    assert_eq!(r, Err(FaultKind::Internal));
    assert_eq!(VIEW_BLOCK_BYTES, 32);
}

/// I15: a reference kept from an earlier creation, and a view whose owner was re-created, fault
/// `ScriptRefused` on every accessor.
#[test]
fn stale_refs_and_views_fault_script_refused() {
    // Feed: [create, keep]. Call 1 keeps a copy of the ref (math.abs is the identity on a
    // CollRef's bits: bit 63 is clear) and call 2 slices, both only when `keep` is set.
    let script = |acc: C| {
        let mut owner = new(
            ov::ARRAY_NEW_FLOAT,
            Ty::Float,
            8,
            float_block(8),
            vec![it(3), ft(2.0)],
        );
        owner.guard = Some(0);
        let mut keep = c(ov::MATH_ABS_I, vec![o(0)], I);
        keep.guard = Some(1);
        let mut view = new(
            ov::ARRAY_SLICE,
            Ty::Float,
            0,
            VIEW_BLOCK_BYTES,
            vec![o(0), it(0), it(2)],
        );
        view.guard = Some(1);
        let mut acc = acc;
        acc.guard = Some(2);
        vec![owner, keep, view, acc]
    };
    // Columns [create, keep, access]: the accessor runs on the last bar only, after the owner
    // was re-created without a new keep / slice.
    let keep_then_recreate = [
        vec![1.0, 1.0, 0.0],
        vec![0.0, 0.0, 0.0],
        vec![1.0, 0.0, 1.0],
    ];
    let alive = [vec![1.0, 1.0, 1.0], vec![0.0, 0.0, 1.0]];
    for via in [1u16, 2] {
        let accs = [
            c(ov::ARRAY_SIZE, vec![o(via)], I),
            c(ov::ARRAY_GET, vec![o(via), it(0)], F),
            c(ov::ARRAY_SET, vec![o(via), it(0), ft(1.0)], &[]),
            c(ov::ARRAY_PUSH, vec![o(via), ft(1.0)], &[]),
            c(ov::ARRAY_POP, vec![o(via)], F),
            c(ov::ARRAY_CLEAR, vec![o(via)], &[]),
        ];
        for acc in accs {
            let id = acc.id;
            let (r, _) = run(&script(acc), &alive, Dialect::V6);
            assert!(
                r.is_ok() || r == Err(FaultKind::CollectionBounds),
                "{id:?} via {via}: alive is fine ({r:?})"
            );
        }
        let accs = [
            c(ov::ARRAY_SIZE, vec![o(via)], I),
            c(ov::ARRAY_GET, vec![o(via), it(0)], F),
            c(ov::ARRAY_SET, vec![o(via), it(0), ft(1.0)], &[]),
            c(ov::ARRAY_PUSH, vec![o(via), ft(1.0)], &[]),
            c(ov::ARRAY_POP, vec![o(via)], F),
            c(ov::ARRAY_CLEAR, vec![o(via)], &[]),
            new(
                ov::ARRAY_SLICE,
                Ty::Float,
                0,
                VIEW_BLOCK_BYTES,
                vec![o(via), it(0), it(1)],
            ),
        ];
        for acc in accs {
            let id = acc.id;
            let (r, _) = run(&script(acc), &keep_then_recreate, Dialect::V6);
            assert_eq!(r, Err(FaultKind::ScriptRefused), "{id:?} via {via}");
        }
    }
    // A na reference is ScriptRefused too.
    assert_eq!(
        once(&[c(ov::ARRAY_SIZE, vec![NA_I], I)], Dialect::V6),
        Err(FaultKind::ScriptRefused)
    );
}

/// The §4.16.5 fault table, core rows: bounds, W15, capacity, views, creation arguments, cell
/// overflow.
#[test]
fn collection_fault_table_core_rows() {
    let b = float_block(4);
    let arr = |size: i64| {
        new(
            ov::ARRAY_NEW_FLOAT,
            Ty::Float,
            4,
            b,
            vec![it(size), ft(0.5)],
        )
    };
    let view = || {
        new(
            ov::ARRAY_SLICE,
            Ty::Float,
            0,
            VIEW_BLOCK_BYTES,
            vec![o(0), it(1), it(3)],
        )
    };
    let cb = Err(FaultKind::CollectionBounds);
    let cases: Vec<(Vec<C>, Dialect, Result<(), FaultKind>)> = vec![
        (
            vec![arr(3), c(ov::ARRAY_GET, vec![o(0), it(3)], F)],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(3), c(ov::ARRAY_GET, vec![o(0), NA_I], F)],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(3), c(ov::ARRAY_GET, vec![o(0), it(-1)], F)],
            Dialect::V5,
            cb,
        ),
        (
            vec![arr(3), c(ov::ARRAY_GET, vec![o(0), it(-4)], F)],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(3), c(ov::ARRAY_GET, vec![o(0), it(-3)], F)],
            Dialect::V6,
            Ok(()),
        ),
        (
            vec![arr(3), c(ov::ARRAY_SET, vec![o(0), it(3), ft(1.0)], &[])],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(4), c(ov::ARRAY_PUSH, vec![o(0), ft(1.0)], &[])],
            Dialect::V6,
            Err(declared(ov::ARRAY_PUSH, FaultKind::PoolFull)),
        ),
        (
            vec![arr(3), c(ov::ARRAY_PUSH, vec![o(0), ft(1.0)], &[])],
            Dialect::V6,
            Ok(()),
        ),
        (
            vec![arr(0), c(ov::ARRAY_POP, vec![o(0)], F)],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(4), view(), c(ov::ARRAY_PUSH, vec![o(1), ft(1.0)], &[])],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(4), view(), c(ov::ARRAY_POP, vec![o(1)], F)],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(4), view(), c(ov::ARRAY_CLEAR, vec![o(1)], &[])],
            Dialect::V6,
            cb,
        ),
        (
            vec![arr(4), view(), c(ov::ARRAY_GET, vec![o(1), it(2)], F)],
            Dialect::V6,
            cb,
        ),
        // The owner shrank under the view: element 1 of the view is owner element 2.
        (
            vec![
                arr(4),
                view(),
                c(ov::ARRAY_POP, vec![o(0)], F),
                c(ov::ARRAY_POP, vec![o(0)], F),
                c(ov::ARRAY_GET, vec![o(1), it(1)], F),
            ],
            Dialect::V6,
            cb,
        ),
        (
            vec![
                arr(4),
                new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(3), it(2)]),
            ],
            Dialect::V6,
            cb,
        ),
        (
            vec![
                arr(4),
                new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(0), it(5)]),
            ],
            Dialect::V6,
            cb,
        ),
        (
            vec![
                arr(4),
                new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(-1), it(2)]),
            ],
            Dialect::V6,
            cb,
        ),
        (vec![arr(-1)], Dialect::V6, Err(FaultKind::ScriptRefused)),
        (
            vec![new(
                ov::ARRAY_NEW_FLOAT,
                Ty::Float,
                4,
                b,
                vec![NA_I, ft(0.5)],
            )],
            Dialect::V6,
            Err(FaultKind::ScriptRefused),
        ),
        (vec![arr(5)], Dialect::V6, Err(FaultKind::PoolFull)),
        (
            vec![
                new(
                    ov::ARRAY_NEW_STRING,
                    Ty::String,
                    2,
                    coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 2, 8),
                    vec![it(0), NA_I],
                ),
                c(ov::OP_ADD_STR, vec![st("abcd"), st("efghi")], I),
                c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]),
            ],
            Dialect::V6,
            Err(declared(ov::ARRAY_PUSH, FaultKind::StrArenaFull)),
        ),
    ];
    for (k, (calls, d, want)) in cases.iter().enumerate() {
        assert_eq!(once(calls, *d).map(drop), *want, "case {k}");
    }
}

/// I16 (get/set): v6 accepts `−len..−1` (`−1` = the last), v5 refuses every negative index.
#[test]
fn negative_indices_follow_w15() {
    let b = float_block(4);
    let calls = |i: i64| {
        vec![
            new(ov::ARRAY_NEW_FLOAT, Ty::Float, 4, b, vec![it(0), ft(0.0)]),
            c(ov::ARRAY_PUSH, vec![o(0), ft(10.0)], &[]),
            c(ov::ARRAY_PUSH, vec![o(0), ft(20.0)], &[]),
            c(ov::ARRAY_PUSH, vec![o(0), ft(30.0)], &[]),
            c(ov::ARRAY_SET, vec![o(0), it(i), ft(99.0)], &[]),
            c(ov::ARRAY_GET, vec![o(0), it(i)], F),
            c(ov::ARRAY_GET, vec![o(0), it(i + 3)], F),
        ]
    };
    for i in -3..0i64 {
        let r = once(&calls(i), Dialect::V6).unwrap();
        assert_eq!((r[5], r[6]), (99.0, 99.0), "v6 index {i}");
        assert_eq!(
            once(&calls(i), Dialect::V5),
            Err(FaultKind::CollectionBounds)
        );
    }
    assert_eq!(
        once(&calls(-4), Dialect::V6),
        Err(FaultKind::CollectionBounds)
    );
}

/// A slice is a view: a set through it reaches the owner, a get through it reads the owner, a
/// resize through it is `CollectionBounds`; a slice of a slice resolves to the root.
#[test]
fn coll_slice_writes_reach_the_owner() {
    let b = float_block(8);
    let calls = vec![
        new(ov::ARRAY_NEW_FLOAT, Ty::Float, 8, b, vec![it(5), ft(1.0)]),
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(1), it(4)]),
        c(ov::ARRAY_SET, vec![o(1), it(0), ft(7.0)], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(1)], F),
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(1), it(1), it(3)]),
        c(ov::ARRAY_SET, vec![o(4), it(-1), ft(9.0)], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(3)], F),
        c(ov::ARRAY_SIZE, vec![o(4)], I),
        c(ov::ARRAY_SET, vec![o(0), it(2), ft(5.0)], &[]),
        c(ov::ARRAY_GET, vec![o(4), it(0)], F),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!(r[3], 7.0, "owner element 1");
    assert_eq!(r[6], 9.0, "owner element 3 through a slice of a slice");
    assert_eq!(r[7], 2.0);
    assert_eq!(r[9], 5.0, "a view reads the owner's current element");
}

/// Strings: a `CONST` element is its ref (zero-copy); a runtime-built one is copied into a cell
/// and its read needs `STR_ALLOC`, which pine-spec's `array.get` row lacks
/// (`pin:04-coll-read-str-alloc`, a CAR): `Internal` until the row carries it.
#[test]
fn string_elements_const_and_owned() {
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    let calls = vec![
        new(
            ov::ARRAY_NEW_STRING,
            Ty::String,
            4,
            sb,
            vec![it(2), st("kept")],
        ),
        c(ov::ARRAY_GET, vec![o(0), it(1)], I),
        c(ov::OP_EQ_STR, vec![o(1), st("kept")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!(r[2], 1.0);
    let calls = vec![
        new(ov::ARRAY_NEW_STRING, Ty::String, 4, sb, vec![it(0), NA_I]),
        c(ov::OP_ADD_STR, vec![st("ab"), st("cd")], I),
        c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(0)], I),
    ];
    let get_has_str_alloc =
        pine_spec::OVERLOADS[ov::ARRAY_GET.0 as usize].bf & pine_vm::bf::STR_ALLOC != 0;
    let r = once(&calls, Dialect::V6);
    if get_has_str_alloc {
        assert!(r.is_ok());
    } else {
        assert_eq!(r, Err(FaultKind::Internal));
    }
}

/// The accessors allocate nothing: a `var`-like owner (created on bar 0 only) pushed, read, set and
/// popped every bar, CONST strings included.
#[test]
fn coll_accessors_are_zero_alloc() {
    let b = float_block(64);
    let mut owner = new(ov::ARRAY_NEW_FLOAT, Ty::Float, 64, b, vec![it(0), ft(0.0)]);
    owner.guard = Some(0);
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 64, 32);
    let mut sowner = new(
        ov::ARRAY_NEW_STRING,
        Ty::String,
        64,
        sb,
        vec![it(1), st("x")],
    );
    sowner.guard = Some(0);
    let calls = vec![
        owner,
        c(ov::ARRAY_PUSH, vec![o(0), (Lane::F64, Src::Feed(1))], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(-1)], F),
        c(
            ov::ARRAY_SET,
            vec![o(0), it(0), (Lane::F64, Src::Feed(1))],
            &[],
        ),
        c(ov::ARRAY_POP, vec![o(0)], F),
        c(ov::ARRAY_PUSH, vec![o(0), ft(1.0)], &[]),
        c(ov::ARRAY_SIZE, vec![o(0)], I),
        sowner,
        c(ov::OP_ADD_STR, vec![st("a"), st("b")], I),
        c(ov::ARRAY_PUSH, vec![o(7), o(8)], &[]),
        c(ov::ARRAY_SET, vec![o(7), it(-1), st("y")], &[]),
        c(ov::ARRAY_POP, vec![o(7)], I),
        c(ov::ARRAY_GET, vec![o(7), it(0)], I),
    ];
    let feed: Vec<Vec<f64>> = (0..48)
        .map(|i| vec![(i == 0) as u8 as f64, i as f64])
        .collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    assert_eq!(r[47][6], 48.0);
    assert_eq!(a, 0, "collection accessors allocated");
}

fn map_new(k: Ty, v: Ty, cap: u32) -> C {
    C {
        id: ov::MAP_NEW,
        ins: vec![it(enc(v)), it(enc(k)), it(cap as i64)],
        outs: I.to_vec(),
        bytes: coll_block_bytes(CollKind::Map, v, k, cap, 16),
        guard: None,
    }
}

/// `map.get` / `map.remove` of a missing key is na (a v6 `bool` value: false), never a fault;
/// `contains` is false; string keys compare by bytes (a SCRATCH key finds a CONST one).
#[test]
fn map_get_on_absent_key_is_na_not_a_fault() {
    let calls = vec![
        map_new(Ty::Int, Ty::Float, 4),
        c(ov::MAP_PUT, vec![o(0), it(7), ft(1.5)], F),
        c(ov::MAP_GET, vec![o(0), it(8)], F),
        c(ov::MAP_REMOVE, vec![o(0), it(8)], F),
        c(ov::MAP_CONTAINS, vec![o(0), it(8)], I),
        c(ov::MAP_GET, vec![o(0), it(7)], F),
        c(ov::MAP_SIZE, vec![o(0)], I),
        map_new(Ty::String, Ty::Bool, 4),
        c(ov::MAP_PUT, vec![o(7), st("ab"), it(1)], I),
        c(ov::OP_ADD_STR, vec![st("a"), st("b")], I),
        c(ov::MAP_GET, vec![o(7), o(9)], I),
        c(ov::MAP_GET, vec![o(7), st("zz")], I),
        c(ov::MAP_CONTAINS, vec![o(7), o(9)], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert!(r[1].is_nan(), "put of a new key returns na");
    assert!(r[2].is_nan() && r[3].is_nan());
    assert_eq!((r[4], r[5], r[6]), (0.0, 1.5, 1.0));
    assert_eq!(
        (r[10], r[11], r[12]),
        (1.0, 0.0, 1.0),
        "absent v6 bool is false"
    );
    let r = once(&calls[..7], Dialect::V5).unwrap();
    assert!(r[2].is_nan());
}

/// An na key is a distinct key (`pin:04-collection-edges`); `−0.0` and `+0.0` are one float key.
#[test]
fn map_na_key_is_distinct() {
    let calls = vec![
        map_new(Ty::Int, Ty::Float, 4),
        c(ov::MAP_PUT, vec![o(0), NA_I, ft(1.0)], F),
        c(ov::MAP_PUT, vec![o(0), it(0), ft(2.0)], F),
        c(ov::MAP_GET, vec![o(0), NA_I], F),
        c(ov::MAP_GET, vec![o(0), it(0)], F),
        c(ov::MAP_SIZE, vec![o(0)], I),
        map_new(Ty::Float, Ty::Int, 4),
        c(ov::MAP_PUT, vec![o(6), ft(-0.0), it(3)], I),
        c(ov::MAP_GET, vec![o(6), ft(0.0)], I),
        c(ov::MAP_PUT, vec![o(6), ft(f64::NAN), it(4)], I),
        c(ov::MAP_GET, vec![o(6), ft(-f64::NAN)], I),
        c(ov::MAP_SIZE, vec![o(6)], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[3], r[4], r[5]), (1.0, 2.0, 2.0));
    assert_eq!((r[8], r[10], r[11]), (3.0, 4.0, 2.0));
}

/// I34: after any sequence of `put` / `remove`, `keys()` / `values()` list the live pairs in
/// first-insertion order, a replaced key keeping its position (a model over 400 random steps).
#[test]
fn map_preserves_insertion_order() {
    const CAP: usize = 8;
    // Feed: [put, remove, key, value, g0..g7 (i < size), init].
    let mut m = map_new(Ty::Int, Ty::Float, CAP as u32);
    m.guard = Some(12);
    let mut calls = vec![m];
    let mut put = c(
        ov::MAP_PUT,
        vec![o(0), (Lane::I64, Src::Feed(2)), (Lane::F64, Src::Feed(3))],
        F,
    );
    put.guard = Some(0);
    let mut rem = c(ov::MAP_REMOVE, vec![o(0), (Lane::I64, Src::Feed(2))], F);
    rem.guard = Some(1);
    calls.push(put);
    calls.push(rem);
    calls.push(new(
        ov::MAP_KEYS,
        Ty::Int,
        CAP as u32,
        coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, CAP as u32, 16),
        vec![o(0)],
    ));
    calls.push(new(
        ov::MAP_VALUES,
        Ty::Float,
        CAP as u32,
        coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, CAP as u32, 16),
        vec![o(0)],
    ));
    for i in 0..CAP {
        let mut g = c(ov::ARRAY_GET, vec![o(3), it(i as i64)], I);
        g.guard = Some(4 + i as u16);
        calls.push(g);
    }
    for i in 0..CAP {
        let mut g = c(ov::ARRAY_GET, vec![o(4), it(i as i64)], F);
        g.guard = Some(4 + i as u16);
        calls.push(g);
    }
    calls.push(c(ov::MAP_SIZE, vec![o(0)], I));
    let mut model: Vec<(i64, f64)> = Vec::new();
    let mut feed = Vec::new();
    let mut want = Vec::new();
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for step in 0..400 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let key = (s % 12) as i64;
        let value = (s >> 40) as f64 / 7.0;
        let pos = model.iter().position(|p| p.0 == key);
        let do_put = s % 3 != 0 && (pos.is_some() || model.len() < CAP);
        if do_put {
            match pos {
                Some(p) => model[p].1 = value,
                None => model.push((key, value)),
            }
        } else if let Some(p) = pos {
            model.remove(p);
        }
        let mut row = vec![do_put as u8 as f64, !do_put as u8 as f64, key as f64, value];
        for i in 0..CAP {
            row.push((i < model.len()) as u8 as f64);
        }
        row.push((step == 0) as u8 as f64);
        feed.push(row);
        want.push(model.clone());
    }
    let (r, a) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    for (b, m) in want.iter().enumerate() {
        assert_eq!(r[b][5 + 2 * CAP], m.len() as f64, "bar {b}: size");
        for (i, (k, v)) in m.iter().enumerate() {
            assert_eq!(r[b][5 + i], *k as f64, "bar {b}: key {i}");
            assert_eq!(
                r[b][5 + CAP + i].to_bits(),
                v.to_bits(),
                "bar {b}: value {i}"
            );
        }
    }
    assert_eq!(a, 0, "map bodies allocated");
}

/// I13 (in place): a SCRATCH string key / element is copied into the block and survives scratch
/// resets — later bars find it by bytes, and `keys()` copies its cell.
#[test]
fn collection_strings_survive_scratch_and_source_reuse() {
    let mut m = map_new(Ty::String, Ty::Int, 4);
    m.guard = Some(0);
    let mut cat = c(ov::OP_ADD_STR, vec![st("k"), st("ey")], I);
    cat.guard = Some(0);
    let mut put = c(ov::MAP_PUT, vec![o(0), o(1), it(42)], I);
    put.guard = Some(0);
    let calls = vec![
        m,
        cat,
        put,
        c(ov::OP_ADD_STR, vec![st("ke"), st("y")], I),
        c(ov::MAP_GET, vec![o(0), o(3)], I),
        c(ov::MAP_CONTAINS, vec![o(0), st("key")], I),
        new(
            ov::MAP_KEYS,
            Ty::String,
            4,
            coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16),
            vec![o(0)],
        ),
        c(ov::ARRAY_SIZE, vec![o(6)], I),
    ];
    let feed: Vec<Vec<f64>> = (0..20).map(|i| vec![(i == 0) as u8 as f64]).collect();
    let (r, _) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    for (b, row) in r.iter().enumerate() {
        assert_eq!((row[4], row[5], row[7]), (42.0, 1.0, 1.0), "bar {b}");
    }
}

/// `array.sort`: in place through the same reference, na last ascending / first descending,
/// `−0.0` before `+0.0`; ints with na too; 0 B/op on a `var`-like array pushed and sorted per bar.
#[test]
fn array_sort_is_in_place_and_zero_alloc() {
    let xs = [3.0, f64::NAN, -0.0, 0.0, -2.5, 7.0, f64::NAN, 1.0];
    let n = xs.len();
    let b = float_block(16);
    let mut calls = vec![new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        16,
        b,
        vec![it(0), ft(0.0)],
    )];
    for x in xs {
        calls.push(c(ov::ARRAY_PUSH, vec![o(0), ft(x)], &[]));
    }
    calls.push(c(
        ov::ARRAY_SORT_AF,
        vec![o(0), (Lane::I64, Src::Feed(0))],
        &[],
    ));
    let g0 = calls.len();
    for i in 0..n {
        calls.push(c(ov::ARRAY_GET, vec![o(0), it(i as i64)], F));
    }
    for (desc, want) in [
        (0.0, [-2.5, -0.0, 0.0, 1.0, 3.0, 7.0, f64::NAN, f64::NAN]),
        (1.0, [f64::NAN, f64::NAN, 7.0, 3.0, 1.0, 0.0, -0.0, -2.5]),
    ] {
        let r = run(&calls, &[vec![desc]], Dialect::V6).0.unwrap();
        for (i, &w) in want.iter().enumerate() {
            let g = r[0][g0 + i];
            assert!(
                (g.is_nan() && w.is_nan()) || g.to_bits() == w.to_bits(),
                "order {desc} element {i}: {g} vs {w}"
            );
        }
    }
    // Ints: na last ascending.
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 4, 16);
    let calls = vec![
        new(ov::ARRAY_NEW_INT, Ty::Int, 4, ib, vec![it(0), it(0)]),
        c(ov::ARRAY_PUSH, vec![o(0), it(5)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), NA_I], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(-9)], &[]),
        c(ov::ARRAY_SORT_AI, vec![o(0), it(0)], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(0)], I),
        c(ov::ARRAY_GET, vec![o(0), it(1)], I),
        c(ov::ARRAY_GET, vec![o(0), it(2)], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[5], r[6]), (-9.0, 5.0));
    assert!(r[7].is_nan(), "int na last");
    // 0 B/op: created on bar 0, one push and a sort every bar.
    let mut owner = new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        64,
        float_block(64),
        vec![it(0), ft(0.0)],
    );
    owner.guard = Some(0);
    let calls = vec![
        owner,
        c(ov::ARRAY_PUSH, vec![o(0), (Lane::F64, Src::Feed(1))], &[]),
        c(ov::ARRAY_SORT_AF, vec![o(0), it(1)], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(0)], F),
    ];
    let feed: Vec<Vec<f64>> = (0..60)
        .map(|i| vec![(i == 0) as u8 as f64, ((i * 37) % 23) as f64])
        .collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    assert_eq!(r.unwrap()[59][3], 22.0);
    assert_eq!(a, 0, "array.sort allocated");
}

/// The §4.9 tree, written out: four accumulators by index mod 4, `(s0 + s1) + (s2 + s3)`, then
/// the tail in order; NaN skipped.
fn tree_sum(xs: &[f64]) -> (f64, u32) {
    let mut s = [0.0f64; 4];
    let mut n = 0;
    let body = xs.len() / 4 * 4;
    for (i, &x) in xs[..body].iter().enumerate() {
        if !x.is_nan() {
            s[i % 4] += x;
            n += 1;
        }
    }
    let mut t = (s[0] + s[1]) + (s[2] + s[3]);
    for &x in &xs[body..] {
        if !x.is_nan() {
            t += x;
            n += 1;
        }
    }
    (t, n)
}

/// `array.sum` / `avg` equal the scalar reference bit for bit (na skipped; all na → na).
#[test]
fn array_reductions_match_scalar_bitwise() {
    let mut s = 0x2545_f491_4f6c_dd1du64;
    for len in 0..=13usize {
        let xs: Vec<f64> = (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                if s % 5 == 0 {
                    f64::NAN
                } else {
                    (s >> 11) as f64 / 3.0e9 - 1.0e6
                }
            })
            .collect();
        let mut calls = vec![new(
            ov::ARRAY_NEW_FLOAT,
            Ty::Float,
            16,
            float_block(16),
            vec![it(0), ft(0.0)],
        )];
        for &x in &xs {
            calls.push(c(ov::ARRAY_PUSH, vec![o(0), ft(x)], &[]));
        }
        calls.push(c(ov::ARRAY_SUM_F, vec![o(0)], F));
        calls.push(c(ov::ARRAY_AVG_F, vec![o(0)], F));
        let r = once(&calls, Dialect::V6).unwrap();
        let (sum, n) = tree_sum(&xs);
        let (gs, ga) = (r[calls.len() - 2], r[calls.len() - 1]);
        if n == 0 {
            assert!(gs.is_nan() && ga.is_nan(), "len {len}: all na → na");
        } else {
            assert_eq!(gs.to_bits(), sum.to_bits(), "len {len}: sum");
            assert_eq!(ga.to_bits(), (sum / n as f64).to_bits(), "len {len}: avg");
        }
    }
}

/// Part 1 accessors: first / last / shift / unshift / insert / remove keep Pine's order;
/// includes / indexof / lastindexof compare in place (strings by bytes, a SCRATCH needle
/// included); I16 for insert / remove (v6 `−1` = the last, `insert` at `−1` goes before it).
#[test]
fn array_part1_order_and_search() {
    let b = float_block(8);
    let calls = vec![
        new(ov::ARRAY_NEW_FLOAT, Ty::Float, 8, b, vec![it(0), ft(0.0)]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(1.0)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(2.0)], &[]),
        c(ov::ARRAY_UNSHIFT, vec![o(0), ft(0.5)], &[]), // 0.5 1 2
        c(ov::ARRAY_INSERT, vec![o(0), it(-1), ft(1.5)], &[]), // 0.5 1 1.5 2
        c(ov::ARRAY_INSERT, vec![o(0), it(4), ft(3.0)], &[]), // 0.5 1 1.5 2 3
        c(ov::ARRAY_REMOVE, vec![o(0), it(-2)], F),     // removes 2
        c(ov::ARRAY_SHIFT, vec![o(0)], F),              // removes 0.5 → 1 1.5 3
        c(ov::ARRAY_FIRST, vec![o(0)], F),
        c(ov::ARRAY_LAST, vec![o(0)], F),
        c(ov::ARRAY_PUSH, vec![o(0), ft(1.5)], &[]), // 1 1.5 3 1.5
        c(ov::ARRAY_INDEXOF, vec![o(0), ft(1.5)], I),
        c(ov::ARRAY_LASTINDEXOF, vec![o(0), ft(1.5)], I),
        c(ov::ARRAY_INCLUDES, vec![o(0), ft(9.0)], I),
        c(ov::ARRAY_INDEXOF, vec![o(0), ft(f64::NAN)], I),
        c(ov::ARRAY_SIZE, vec![o(0)], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[6], r[7], r[8], r[9]), (2.0, 0.5, 1.0, 3.0));
    assert_eq!(
        (r[11], r[12], r[13], r[14], r[15]),
        (1.0, 3.0, 0.0, -1.0, 4.0)
    );
    // v5: a negative index to insert / remove is CollectionBounds.
    assert_eq!(
        once(&calls[..5], Dialect::V5),
        Err(FaultKind::CollectionBounds)
    );
    // Strings: a SCRATCH element found by a CONST needle and the other way round.
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    let calls = vec![
        new(
            ov::ARRAY_NEW_STRING,
            Ty::String,
            4,
            sb,
            vec![it(1), st("x")],
        ),
        c(ov::OP_ADD_STR, vec![st("a"), st("b")], I),
        c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]),
        c(ov::ARRAY_INDEXOF, vec![o(0), st("ab")], I),
        c(ov::OP_ADD_STR, vec![st("x"), st("")], I),
        c(ov::ARRAY_INDEXOF, vec![o(0), o(4)], I),
        c(ov::ARRAY_INCLUDES, vec![o(0), st("zz")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[3], r[5], r[6]), (1.0, 0.0, 0.0));
}

/// I17 (arrays): in v6 a bool output is never na — `array.new_bool` without a value fills false,
/// `includes` is 0/1; in v5 an na bool element reads na.
#[test]
fn v6_bool_outputs_are_never_na() {
    let bb = coll_block_bytes(CollKind::Array, Ty::Bool, Ty::Void, 4, 16);
    let calls = |_: Dialect| {
        vec![
            new(ov::ARRAY_NEW_BOOL, Ty::Bool, 4, bb, vec![it(2), NA_I]),
            c(ov::ARRAY_GET, vec![o(0), it(1)], I),
            c(ov::ARRAY_INCLUDES, vec![o(0), it(1)], I),
            c(ov::ARRAY_POP, vec![o(0)], I),
            c(ov::ARRAY_FIRST, vec![o(0)], I),
        ]
    };
    let r = once(&calls(Dialect::V6), Dialect::V6).unwrap();
    assert_eq!((r[1], r[2], r[3], r[4]), (0.0, 0.0, 0.0, 0.0));
    let r = once(&calls(Dialect::V5), Dialect::V5).unwrap();
    assert!(r[1].is_nan() && r[3].is_nan() && r[4].is_nan());
    assert_eq!(r[2], 0.0);
}

fn split(src: (Lane, Src), sep: (Lane, Src), cap: u32) -> C {
    new(
        ov::STR_SPLIT,
        Ty::String,
        cap,
        coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, cap, 16),
        vec![src, sep],
    )
}

/// `str.split` of a `CONST` source stores `CONST` views (an element read succeeds without
/// `STR_ALLOC`, which an owned cell would need); a SCRATCH source's pieces are copied into cells
/// (found in place by `indexof`); empties kept; `""` splits code points; na → na.
#[test]
fn str_split_const_source_is_views() {
    let calls = vec![
        split(st("a,b,,c"), st(","), 8),
        c(ov::ARRAY_SIZE, vec![o(0)], I),
        c(ov::ARRAY_GET, vec![o(0), it(0)], I),
        c(ov::OP_EQ_STR, vec![o(2), st("a")], I),
        c(ov::ARRAY_GET, vec![o(0), it(2)], I),
        c(ov::OP_EQ_STR, vec![o(4), st("")], I),
        c(ov::ARRAY_GET, vec![o(0), it(3)], I),
        c(ov::OP_EQ_STR, vec![o(6), st("c")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[1], r[3], r[5], r[7]), (4.0, 1.0, 1.0, 1.0));
    let calls = vec![
        c(ov::OP_ADD_STR, vec![st("xx::y"), st("::zzz")], I),
        split(o(0), st("::"), 4),
        c(ov::ARRAY_SIZE, vec![o(1)], I),
        c(ov::ARRAY_INDEXOF, vec![o(1), st("zzz")], I),
        c(ov::ARRAY_INDEXOF, vec![o(1), st("y")], I),
        split(st("a\u{e9}b"), st(""), 4),
        c(ov::ARRAY_SIZE, vec![o(5)], I),
        c(ov::ARRAY_INDEXOF, vec![o(5), st("\u{e9}")], I),
        split(NA_I, st(","), 4),
        c(ov::OP_EQ_STR, vec![st(""), st("")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[2], r[3], r[4]), (3.0, 2.0, 1.0));
    assert_eq!((r[6], r[7]), (3.0, 1.0));
    assert!(r[8].is_nan(), "na source → na array");
    // More pieces than `__cap`: PoolFull (the row declares POOL).
    assert_eq!(
        once(&[split(st("a,b,c"), st(","), 2)], Dialect::V6),
        Err(FaultKind::PoolFull)
    );
}

/// T34 (first part-2 cut): `array.reverse` in place (a view reverses its window of the owner; a
/// string array moves its element words, the cells stay); `min` / `max` with `nth`
/// (`pin:04-array-nth`: rank of the non-na values, duplicates counted; past the count → na);
/// `range` (na skipped, all na → na); `every` / `some` (`pin:04-array-every-some`); 0 B/op.
#[test]
fn array_part2_reverse_rank_range_every_some() {
    let b = float_block(8);
    let calls = vec![
        new(ov::ARRAY_NEW_FLOAT, Ty::Float, 8, b, vec![it(0), ft(0.0)]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(5.0)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(f64::NAN)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(-2.0)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(9.0)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), ft(5.0)], &[]), // 5 na -2 9 5
        c(ov::ARRAY_MIN_F, vec![o(0), it(0)], F),    // 6: -2
        c(ov::ARRAY_MIN_F, vec![o(0), it(2)], F),    // 7: -2 5 5 9 → 5
        c(ov::ARRAY_MAX_F, vec![o(0), it(1)], F),    // 8: 9 5 5 -2 → 5 (second highest)
        c(ov::ARRAY_MAX_F, vec![o(0), it(3)], F),    // 9: -2
        c(ov::ARRAY_MAX_F, vec![o(0), it(4)], F),    // 10: past the 4 non-na → na
        c(ov::ARRAY_MIN_F, vec![o(0), it(-1)], F),   // 11: negative → na
        c(ov::ARRAY_RANGE_F, vec![o(0)], F),         // 12: 11
        c(ov::ARRAY_REVERSE, vec![o(0)], &[]),       // 5 9 -2 na 5
        c(ov::ARRAY_GET, vec![o(0), it(1)], F),      // 14: 9
        c(ov::ARRAY_GET, vec![o(0), it(3)], F),      // 15: na
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(1), it(4)]), // 16: 9 -2 na
        c(ov::ARRAY_REVERSE, vec![o(16)], &[]),      // owner: 5 na -2 9 5
        c(ov::ARRAY_GET, vec![o(0), it(1)], F),      // 18: na
        c(ov::ARRAY_GET, vec![o(0), it(3)], F),      // 19: 9
        c(ov::ARRAY_MIN_F, vec![o(16), it(0)], F),   // 20: -2 through the view
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[6], r[7], r[8], r[9]), (-2.0, 5.0, 5.0, -2.0));
    assert!(r[10].is_nan() && r[11].is_nan());
    assert_eq!(r[12], 11.0);
    assert_eq!(r[14], 9.0);
    assert!(r[15].is_nan());
    assert!(r[18].is_nan(), "the view reversed its window of the owner");
    assert_eq!((r[19], r[20]), (9.0, -2.0));
    // Ints: na skipped; an all-na array → na; range of one value is 0.
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 4, 16);
    let calls = vec![
        new(ov::ARRAY_NEW_INT, Ty::Int, 4, ib, vec![it(2), NA_I]),
        c(ov::ARRAY_MIN_I, vec![o(0), it(0)], I), // 1: na
        c(ov::ARRAY_RANGE_I, vec![o(0)], I),      // 2: na
        c(ov::ARRAY_PUSH, vec![o(0), it(-7)], &[]),
        c(ov::ARRAY_MAX_I, vec![o(0), it(0)], I), // 4: -7
        c(ov::ARRAY_RANGE_I, vec![o(0)], I),      // 5: 0
        c(ov::ARRAY_PUSH, vec![o(0), it(4)], &[]),
        c(ov::ARRAY_RANGE_I, vec![o(0)], I),     // 7: 11
        c(ov::ARRAY_MAX_I, vec![o(0), NA_I], I), // 8: na nth → na
        c(ov::ARRAY_SUM_I, vec![o(0)], I),       // 9: -3, na skipped
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert!(r[1].is_nan() && r[2].is_nan() && r[8].is_nan());
    assert_eq!((r[4], r[5], r[7], r[9]), (-7.0, 0.0, 11.0, -3.0));
    let calls = vec![
        new(ov::ARRAY_NEW_INT, Ty::Int, 4, ib, vec![it(2), NA_I]),
        c(ov::ARRAY_SUM_I, vec![o(0)], I), // all na → na
        c(ov::ARRAY_PUSH, vec![o(0), it(i64::MAX)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(2)], &[]),
        c(ov::ARRAY_SUM_I, vec![o(0)], I), // pin:04-array-sum-int: wraps
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert!(r[1].is_nan());
    assert_eq!(r[4], (i64::MAX.wrapping_add(2)) as f64);
    // Strings: reverse moves the element words (CONST refs and an owned cell alike).
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    let calls = vec![
        new(
            ov::ARRAY_NEW_STRING,
            Ty::String,
            4,
            sb,
            vec![it(1), st("x")],
        ),
        c(ov::OP_ADD_STR, vec![st("a"), st("b")], I),
        c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), st("z")], &[]), // x ab z
        c(ov::ARRAY_REVERSE, vec![o(0)], &[]),       // z ab x
        c(ov::ARRAY_INDEXOF, vec![o(0), st("z")], I),
        c(ov::ARRAY_INDEXOF, vec![o(0), st("ab")], I),
        c(ov::ARRAY_INDEXOF, vec![o(0), st("x")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[5], r[6], r[7]), (0.0, 1.0, 2.0));
    // every / some: v6 bools; an empty array is every = true, some = false.
    let bb = coll_block_bytes(CollKind::Array, Ty::Bool, Ty::Void, 4, 16);
    let calls = vec![
        new(ov::ARRAY_NEW_BOOL, Ty::Bool, 4, bb, vec![it(0), NA_I]),
        c(ov::ARRAY_EVERY, vec![o(0)], I), // 1: true
        c(ov::ARRAY_SOME, vec![o(0)], I),  // 2: false
        c(ov::ARRAY_PUSH, vec![o(0), it(1)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(1)], &[]),
        c(ov::ARRAY_EVERY, vec![o(0)], I), // 5: true
        c(ov::ARRAY_PUSH, vec![o(0), it(0)], &[]),
        c(ov::ARRAY_EVERY, vec![o(0)], I), // 7: false
        c(ov::ARRAY_SOME, vec![o(0)], I),  // 8: true
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[1], r[2], r[5], r[7], r[8]), (1.0, 0.0, 1.0, 0.0, 1.0));
    // v5: an na bool element is not true.
    let calls = vec![
        new(ov::ARRAY_NEW_BOOL, Ty::Bool, 4, bb, vec![it(1), NA_I]),
        c(ov::ARRAY_EVERY, vec![o(0)], I),
        c(ov::ARRAY_SOME, vec![o(0)], I),
    ];
    let r = once(&calls, Dialect::V5).unwrap();
    assert_eq!((r[1], r[2]), (0.0, 0.0));
    // 0 B/op: created on bar 0, a push, reverse, min/max/range every bar.
    let mut owner = new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        64,
        float_block(64),
        vec![it(0), ft(0.0)],
    );
    owner.guard = Some(0);
    let calls = vec![
        owner,
        c(ov::ARRAY_PUSH, vec![o(0), (Lane::F64, Src::Feed(1))], &[]),
        c(ov::ARRAY_REVERSE, vec![o(0)], &[]),
        c(ov::ARRAY_MAX_F, vec![o(0), it(2)], F),
        c(ov::ARRAY_RANGE_F, vec![o(0)], F),
    ];
    let feed: Vec<Vec<f64>> = (0..40)
        .map(|i| vec![(i == 0) as u8 as f64, ((i * 37) % 23) as f64])
        .collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    assert_eq!(r[39][4], 22.0);
    assert_eq!(a, 0, "array part 2 allocated");
}

/// T34: `binary_search*` over ascending float and int arrays (`pin:04-binary-search`): the
/// reference's examples (leftmost(3) on `-2 0 1 5 9` is 2, rightmost(3) is 3), first / last
/// occurrence of a duplicate, the two ends, an na value; an int array takes a float value.
#[test]
fn array_binary_search_rows() {
    let b = float_block(8);
    let mut calls = vec![new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        8,
        b,
        vec![it(0), ft(0.0)],
    )];
    for x in [-2.0, 0.0, 1.0, 5.0, 5.0, 9.0, f64::NAN] {
        calls.push(c(ov::ARRAY_PUSH, vec![o(0), ft(x)], &[]));
    }
    let g0 = calls.len();
    let probes: [(OverloadId, f64, f64); 11] = [
        (ov::ARRAY_BINARY_SEARCH_AF, 5.0, 3.0),
        (ov::ARRAY_BINARY_SEARCH_AF, 3.0, -1.0),
        (ov::ARRAY_BINARY_SEARCH_AF, f64::NAN, -1.0),
        (ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF, 3.0, 2.0),
        (ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF, 5.0, 3.0),
        (ov::ARRAY_BINARY_SEARCH_LEFTMOST_AF, -5.0, -1.0),
        (ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF, 3.0, 3.0),
        (ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF, 5.0, 4.0),
        (ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF, 10.0, 6.0),
        (ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AF, f64::NAN, -1.0),
        (ov::ARRAY_BINARY_SEARCH_AF, 9.0, 5.0),
    ];
    for (id, v, _) in probes {
        calls.push(c(id, vec![o(0), ft(v)], I));
    }
    let r = once(&calls, Dialect::V6).unwrap();
    for (k, (_, v, want)) in probes.iter().enumerate() {
        assert_eq!(r[g0 + k], *want, "probe {k} ({v})");
    }
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 4, 16);
    let calls = vec![
        new(ov::ARRAY_NEW_INT, Ty::Int, 4, ib, vec![it(0), it(0)]),
        c(ov::ARRAY_PUSH, vec![o(0), it(1)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(3)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(3)], &[]),
        c(ov::ARRAY_PUSH, vec![o(0), it(7)], &[]),
        c(ov::ARRAY_BINARY_SEARCH_AI, vec![o(0), ft(3.0)], I), // 5: 1
        c(ov::ARRAY_BINARY_SEARCH_RIGHTMOST_AI, vec![o(0), ft(3.0)], I), // 6: 2
        c(ov::ARRAY_BINARY_SEARCH_AI, vec![o(0), ft(3.5)], I), // 7: -1
        c(ov::ARRAY_BINARY_SEARCH_LEFTMOST_AI, vec![o(0), ft(3.5)], I), // 8: 2
        c(ov::ARRAY_BINARY_SEARCH_LEFTMOST_AI, vec![o(0), ft(0.0)], I), // 9: -1
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[5], r[6], r[7], r[8], r[9]), (1.0, 2.0, -1.0, 2.0, -1.0));
}
