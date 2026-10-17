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

/// Spec 7d2ac894…: every collection row that stores or returns a string declares `STR_ALLOC`, and
/// every one that may take a cell or grow declares `POOL` (F10), so a full cell / arena is
/// `StrArenaFull` and a push beyond `__cap` `PoolFull` — no `Internal` fallback any more.
#[test]
fn collection_rows_declare_their_faults() {
    let fl = |id: OverloadId| pine_spec::OVERLOADS[id.0 as usize].bf;
    for id in [
        ov::ARRAY_GET,
        ov::ARRAY_SET,
        ov::ARRAY_POP,
        ov::ARRAY_SHIFT,
        ov::ARRAY_REMOVE,
        ov::ARRAY_FIRST,
        ov::ARRAY_LAST,
        ov::MAP_GET,
        ov::MAP_REMOVE,
    ] {
        assert!(fl(id) & pine_vm::bf::STR_ALLOC != 0, "{id:?}");
    }
    for id in [
        ov::ARRAY_PUSH,
        ov::ARRAY_UNSHIFT,
        ov::ARRAY_INSERT,
        ov::MAP_PUT,
    ] {
        let f = fl(id);
        assert!(
            f & pine_vm::bf::STR_ALLOC != 0 && f & pine_vm::bf::POOL != 0,
            "{id:?}"
        );
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
            Err(k) => assert_eq!(r, Err(k)),
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
            Err(FaultKind::PoolFull),
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
            Err(FaultKind::StrArenaFull),
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
/// and read back through `str_alloc` (the `array.get` row is `STR_ALLOC`, spec 7d2ac894…). I13 /
/// I14 through `array.get`: an element built in SCRATCH on bar 0 reads back by bytes on every
/// later bar (scratch reset in between); a string of exactly the cell's bytes round-trips, one
/// byte more is `StrArenaFull` and leaves the array as it was.
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
        c(ov::OP_EQ_STR, vec![o(3), st("abcd")], I),
    ];
    assert_eq!(once(&calls, Dialect::V6).unwrap()[4], 1.0);
    // I13 across bars: built and pushed on bar 0 only, read on every bar.
    let mut owner = new(ov::ARRAY_NEW_STRING, Ty::String, 4, sb, vec![it(0), NA_I]);
    owner.guard = Some(0);
    let mut cat = c(ov::OP_ADD_STR, vec![st("abcdefgh"), st("ijklmnop")], I);
    cat.guard = Some(0);
    let mut push = c(ov::ARRAY_PUSH, vec![o(0), o(1)], &[]);
    push.guard = Some(0);
    let calls = vec![
        owner,
        cat,
        push,
        c(ov::OP_ADD_STR, vec![st("scratch"), st("-noise")], I),
        c(ov::ARRAY_GET, vec![o(0), it(0)], I),
        c(ov::OP_EQ_STR, vec![o(4), st("abcdefghijklmnop")], I),
        c(ov::ARRAY_SIZE, vec![o(0)], I),
    ];
    let feed: Vec<Vec<f64>> = (0..12).map(|i| vec![(i == 0) as u8 as f64]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    for (b, row) in r.unwrap().iter().enumerate() {
        assert_eq!((row[5], row[6]), (1.0, 1.0), "bar {b}");
    }
    assert_eq!(a, 0, "array.get of an owned string allocated");
    // I14: 17 bytes into a 16-byte cell — StrArenaFull, nothing changed (a later call never runs).
    let calls = vec![
        new(
            ov::ARRAY_NEW_STRING,
            Ty::String,
            4,
            sb,
            vec![it(1), st("k")],
        ),
        c(ov::OP_ADD_STR, vec![st("abcdefgh"), st("ijklmnopq")], I),
        c(ov::ARRAY_SET, vec![o(0), it(0), o(1)], &[]),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::StrArenaFull));
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

/// `array.fill` (`pin:04-array-fill-no-grow`): `[from, to)` of the existing elements, `to` na =
/// the size; the size never changes; `from > to`, `to > size`, a negative or na `from` is
/// `CollectionBounds`. A runtime-built string fills each element with its own cell, re-fillable.
#[test]
fn array_fill_sets_existing_elements_only() {
    let b = float_block(8);
    let base = || {
        let mut v = vec![new(
            ov::ARRAY_NEW_FLOAT,
            Ty::Float,
            8,
            b,
            vec![it(0), ft(0.0)],
        )];
        for x in 1..=5 {
            v.push(c(ov::ARRAY_PUSH, vec![o(0), ft(x as f64)], &[]));
        }
        v
    };
    let mut calls = base();
    calls.push(c(ov::ARRAY_FILL, vec![o(0), ft(9.0), it(1), it(3)], &[]));
    for i in 0..5 {
        calls.push(c(ov::ARRAY_GET, vec![o(0), it(i)], F));
    }
    calls.push(c(ov::ARRAY_SIZE, vec![o(0)], I));
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!(&r[7..13], &[1.0, 9.0, 9.0, 4.0, 5.0, 5.0]);
    let mut calls = base();
    calls.push(c(ov::ARRAY_FILL, vec![o(0), ft(7.0), it(0), NA_I], &[]));
    calls.push(c(ov::ARRAY_SUM_F, vec![o(0)], F));
    calls.push(c(ov::ARRAY_SIZE, vec![o(0)], I));
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[7], r[8]), (35.0, 5.0));
    for (from, to) in [
        (it(3), it(2)),
        (it(0), it(6)),
        (it(-1), it(2)),
        (NA_I, it(2)),
        (it(0), it(-1)),
    ] {
        let mut calls = base();
        calls.push(c(ov::ARRAY_FILL, vec![o(0), ft(7.0), from, to], &[]));
        assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::CollectionBounds));
    }
    // Strings: 4 elements, each re-filled every bar with a SCRATCH string (cells released first).
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    let mut owner = new(
        ov::ARRAY_NEW_STRING,
        Ty::String,
        4,
        sb,
        vec![it(4), st("k")],
    );
    owner.guard = Some(0);
    let calls = vec![
        owner,
        c(ov::OP_ADD_STR, vec![st("xy"), st("z")], I),
        c(ov::ARRAY_FILL, vec![o(0), o(1), it(0), NA_I], &[]),
        c(ov::ARRAY_GET, vec![o(0), it(3)], I),
        c(ov::OP_EQ_STR, vec![o(3), st("xyz")], I),
    ];
    let feed: Vec<Vec<f64>> = (0..10).map(|i| vec![(i == 0) as u8 as f64]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    for row in r.unwrap() {
        assert_eq!(row[4], 1.0);
    }
    assert_eq!(a, 0, "array.fill allocated");
}

/// `array.variance` / `stdev` (`pin:04-array-variance-two-pass`): mean, then Σ(x − mean)², over
/// the non-na values; biased ÷ n, unbiased ÷ (n − 1); empty → na.
#[test]
fn array_variance_stdev_two_pass() {
    let b = float_block(8);
    let xs = [1.0, 2.0, f64::NAN, 3.0, 4.0];
    let mut calls = vec![new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        8,
        b,
        vec![it(0), ft(0.0)],
    )];
    for x in xs {
        calls.push(c(ov::ARRAY_PUSH, vec![o(0), ft(x)], &[]));
    }
    let k = calls.len();
    calls.push(c(ov::ARRAY_VARIANCE_F, vec![o(0), it(1)], F));
    calls.push(c(ov::ARRAY_VARIANCE_F, vec![o(0), it(0)], F));
    calls.push(c(ov::ARRAY_STDEV_F, vec![o(0), it(1)], F));
    calls.push(c(ov::ARRAY_STDEV_F, vec![o(0), it(0)], F));
    let r = once(&calls, Dialect::V6).unwrap();
    let ss = 5.0f64; // mean 2.5: 2.25 + 0.25 + 0.25 + 2.25
    assert_eq!(r[k], ss / 4.0);
    assert_eq!(r[k + 1], ss / 3.0);
    assert_eq!(r[k + 2], (ss / 4.0).sqrt());
    assert_eq!(r[k + 3], (ss / 3.0).sqrt());
    let calls = vec![
        new(ov::ARRAY_NEW_FLOAT, Ty::Float, 8, b, vec![it(1), ft(2.0)]),
        c(ov::ARRAY_VARIANCE_F, vec![o(0), it(1)], F),
        c(ov::ARRAY_VARIANCE_F, vec![o(0), it(0)], F),
        c(ov::ARRAY_CLEAR, vec![o(0)], &[]),
        c(ov::ARRAY_STDEV_F, vec![o(0), it(1)], F),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!(r[1], 0.0);
    assert!(r[2].is_nan() && r[4].is_nan());
}

/// `array.copy` / `array.from` / `array.concat` copy elements: a copy is independent of its
/// source; runtime-built strings get their own cells (also `concat(a, a)`); past `__cap` /
/// `cap` is `PoolFull`; a view as `concat`'s `id1` is `CollectionBounds`.
#[test]
fn array_copy_from_concat() {
    let b = float_block(4);
    let calls = vec![
        new(
            ov::ARRAY_FROM_AF,
            Ty::Float,
            4,
            b,
            vec![ft(1.5), ft(2.5), ft(3.5)],
        ),
        new(ov::ARRAY_COPY, Ty::Float, 4, b, vec![o(0)]),
        c(ov::ARRAY_SET, vec![o(0), it(1), ft(9.0)], &[]),
        c(ov::ARRAY_GET, vec![o(1), it(1)], F),
        c(ov::ARRAY_SIZE, vec![o(1)], I),
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(1), it(3)]),
        new(ov::ARRAY_COPY, Ty::Float, 4, b, vec![o(5)]),
        c(ov::ARRAY_SUM_F, vec![o(6)], F),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[3], r[4], r[7]), (2.5, 3.0, 12.5));
    // Past __cap.
    let calls = vec![
        new(
            ov::ARRAY_FROM_AF,
            Ty::Float,
            4,
            b,
            vec![ft(1.0), ft(2.0), ft(3.0)],
        ),
        new(ov::ARRAY_COPY, Ty::Float, 2, float_block(2), vec![o(0)]),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::PoolFull));
    let calls = vec![new(
        ov::ARRAY_FROM_AF,
        Ty::Float,
        2,
        float_block(2),
        vec![ft(1.0), ft(2.0), ft(3.0)],
    )];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::PoolFull));
    // Strings: from, copy and concat(a, a) of owned cells.
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 6, 16);
    let calls = vec![
        c(ov::OP_ADD_STR, vec![st("ab"), st("cd")], I),
        new(ov::ARRAY_FROM_AS, Ty::String, 6, sb, vec![o(0), st("k")]),
        new(ov::ARRAY_COPY, Ty::String, 6, sb, vec![o(1)]),
        c(ov::ARRAY_CONCAT, vec![o(1), o(1)], I),
        c(ov::ARRAY_CONCAT, vec![o(3), o(2)], I),
        c(ov::ARRAY_SET, vec![o(2), it(0), st("zz")], &[]),
        c(ov::ARRAY_SIZE, vec![o(1)], I),
        c(ov::ARRAY_GET, vec![o(1), it(2)], I),
        c(ov::OP_EQ_STR, vec![o(7), st("abcd")], I),
        c(ov::ARRAY_GET, vec![o(1), it(4)], I),
        c(ov::OP_EQ_STR, vec![o(9), st("abcd")], I),
        c(ov::ARRAY_GET, vec![o(1), it(5)], I),
        c(ov::OP_EQ_STR, vec![o(11), st("k")], I),
    ];
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[6], r[8], r[10], r[12]), (6.0, 1.0, 1.0, 1.0));
    // concat past cap; a view as id1.
    let calls = vec![
        new(
            ov::ARRAY_FROM_AF,
            Ty::Float,
            4,
            b,
            vec![ft(1.0), ft(2.0), ft(3.0)],
        ),
        c(ov::ARRAY_CONCAT, vec![o(0), o(0)], I),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::PoolFull));
    let calls = vec![
        new(ov::ARRAY_FROM_AF, Ty::Float, 4, b, vec![ft(1.0), ft(2.0)]),
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(0), it(1)]),
        c(ov::ARRAY_CONCAT, vec![o(1), o(0)], I),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::CollectionBounds));
}

/// `array.join` (`pin:04-array-join-render`): elements as `str.tostring` renders them, na as
/// `"NaN"`, separated by the separator (a SCRATCH one too); empty → `""`; a na separator → na;
/// 0 B/op.
#[test]
fn array_join_renders_in_place() {
    let b = float_block(4);
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 4, 16);
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16);
    let calls = vec![
        new(
            ov::ARRAY_FROM_AF,
            Ty::Float,
            4,
            b,
            vec![ft(1.5), ft(f64::NAN), ft(2.0)],
        ),
        c(ov::ARRAY_JOIN_AF, vec![o(0), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(1), st("1.5,NaN,2")], I),
        new(ov::ARRAY_FROM_AI, Ty::Int, 4, ib, vec![it(1), it(-2), NA_I]),
        c(ov::ARRAY_JOIN_AI, vec![o(3), st(", ")], I),
        c(ov::OP_EQ_STR, vec![o(4), st("1, -2, NaN")], I),
        c(ov::OP_ADD_STR, vec![st("b"), st("c")], I),
        new(
            ov::ARRAY_FROM_AS,
            Ty::String,
            4,
            sb,
            vec![st("a"), o(6), NA_I],
        ),
        c(ov::OP_ADD_STR, vec![st("-"), st("-")], I),
        c(ov::ARRAY_JOIN_AS, vec![o(7), o(8)], I),
        c(ov::OP_EQ_STR, vec![o(9), st("a--bc--NaN")], I),
        new(ov::ARRAY_SLICE, Ty::Float, 0, 32, vec![o(0), it(1), it(1)]),
        c(ov::ARRAY_JOIN_AF, vec![o(11), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(12), st("")], I),
        c(ov::ARRAY_JOIN_AF, vec![o(0), NA_I], I),
    ];
    let feed: Vec<Vec<f64>> = (0..6).map(|_| vec![0.0]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    for row in r.unwrap() {
        assert_eq!((row[2], row[5], row[10], row[13]), (1.0, 1.0, 1.0, 1.0));
        assert!(row[14].is_nan(), "na separator");
    }
    assert_eq!(a, 0, "array.join allocated");
}

/// String `array.sort` (bytewise, na last ascending / first descending, CONST and owned cells
/// mixed) and `array.sort_indices` (a stable order: ties by index; the source unchanged).
#[test]
fn array_sort_strings_and_sort_indices() {
    let sb = coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 8, 16);
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 8, 16);
    let b = float_block(8);
    let mut calls = vec![
        c(ov::OP_ADD_STR, vec![st("b"), st("b")], I),
        new(
            ov::ARRAY_FROM_AS,
            Ty::String,
            8,
            sb,
            vec![st("c"), o(0), NA_I, st("a"), st("bb"), st("B")],
        ),
        new(ov::ARRAY_SORT_INDICES_AS, Ty::Int, 8, ib, vec![o(1), it(0)]),
        c(ov::ARRAY_JOIN_AI, vec![o(2), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(3), st("5,3,1,4,0,2")], I),
        c(ov::ARRAY_SORT_AS, vec![o(1), it(1)], &[]),
        c(ov::ARRAY_JOIN_AS, vec![o(1), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(6), st("NaN,c,bb,bb,a,B")], I),
        c(ov::ARRAY_SORT_AS, vec![o(1), it(0)], &[]),
        c(ov::ARRAY_JOIN_AS, vec![o(1), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(9), st("B,a,bb,bb,c,NaN")], I),
        new(
            ov::ARRAY_FROM_AF,
            Ty::Float,
            8,
            b,
            vec![ft(2.0), ft(f64::NAN), ft(1.0), ft(2.0), ft(-0.0), ft(0.0)],
        ),
        new(
            ov::ARRAY_SORT_INDICES_AF,
            Ty::Int,
            8,
            ib,
            vec![o(11), it(1)],
        ),
        c(ov::ARRAY_JOIN_AI, vec![o(12), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(13), st("1,0,3,2,5,4")], I),
        c(ov::ARRAY_GET, vec![o(11), it(0)], F),
    ];
    calls.push(c(ov::ARRAY_SIZE, vec![o(12)], I));
    let feed: Vec<Vec<f64>> = (0..4).map(|_| vec![0.0]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    for row in r.unwrap() {
        assert_eq!(
            (row[4], row[7], row[10], row[14], row[15], row[16]),
            (1.0, 1.0, 1.0, 1.0, 2.0, 6.0)
        );
    }
    assert_eq!(a, 0, "sort / sort_indices allocated");
}

/// `map.copy` (same pairs, same order, owned strings in the copy's own cells, independent of the
/// source) and `map.put_all` (`id2`'s pairs in order: replace in place / append; new keys past
/// `cap` → `PoolFull`, checked before any write; `put_all(m, m)` changes nothing).
#[test]
fn map_copy_and_put_all() {
    let mcopy = |src: u16, cap: u32| C {
        id: ov::MAP_COPY,
        ins: vec![
            it(enc(Ty::String)),
            it(enc(Ty::String)),
            it(cap as i64),
            o(src),
        ],
        outs: I.to_vec(),
        bytes: coll_block_bytes(CollKind::Map, Ty::String, Ty::String, cap, 16),
        guard: None,
    };
    let keys = |m: u16| {
        new(
            ov::MAP_KEYS,
            Ty::String,
            4,
            coll_block_bytes(CollKind::Array, Ty::String, Ty::Void, 4, 16),
            vec![o(m)],
        )
    };
    let calls = vec![
        map_new(Ty::String, Ty::String, 4),                     // 0
        c(ov::OP_ADD_STR, vec![st("k"), st("1")], I),           // 1
        c(ov::OP_ADD_STR, vec![st("v"), st("1")], I),           // 2
        c(ov::MAP_PUT, vec![o(0), o(1), o(2)], I),              // 3
        c(ov::MAP_PUT, vec![o(0), st("k2"), st("v2")], I),      // 4
        mcopy(0, 4),                                            // 5
        c(ov::MAP_PUT, vec![o(0), st("k1"), st("changed")], I), // 6
        c(ov::MAP_GET, vec![o(5), st("k1")], I),                // 7
        c(ov::OP_EQ_STR, vec![o(7), st("v1")], I),              // 8
        keys(5),                                                // 9
        c(ov::ARRAY_JOIN_AS, vec![o(9), st(",")], I),           // 10
        c(ov::OP_EQ_STR, vec![o(10), st("k1,k2")], I),          // 11
        map_new(Ty::String, Ty::String, 4),                     // 12
        c(ov::MAP_PUT, vec![o(12), st("k2"), st("x")], I),      // 13
        c(ov::MAP_PUT, vec![o(12), st("k0"), st("y")], I),      // 14
        c(ov::MAP_PUT_ALL, vec![o(12), o(5)], &[]),             // 15
        keys(12),                                               // 16
        c(ov::ARRAY_JOIN_AS, vec![o(16), st(",")], I),          // 17
        c(ov::OP_EQ_STR, vec![o(17), st("k2,k0,k1")], I),       // 18
        c(ov::MAP_GET, vec![o(12), st("k2")], I),               // 19
        c(ov::OP_EQ_STR, vec![o(19), st("v2")], I),             // 20
        c(ov::MAP_GET, vec![o(12), st("k1")], I),               // 21
        c(ov::OP_EQ_STR, vec![o(21), st("v1")], I),             // 22
        c(ov::MAP_PUT_ALL, vec![o(12), o(12)], &[]),            // 23
        c(ov::MAP_SIZE, vec![o(12)], I),                        // 24
    ];
    let feed: Vec<Vec<f64>> = (0..4).map(|_| vec![0.0]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    for row in r.unwrap() {
        assert_eq!(
            (row[8], row[11], row[18], row[20], row[22], row[24]),
            (1.0, 1.0, 1.0, 1.0, 1.0, 3.0)
        );
    }
    assert_eq!(a, 0, "map.copy / put_all allocated");
    // New keys past cap: PoolFull, the target unchanged (checked first); copy past __cap too.
    let calls = vec![
        map_new(Ty::String, Ty::String, 2),
        c(ov::MAP_PUT, vec![o(0), st("a"), st("1")], I),
        map_new(Ty::String, Ty::String, 4),
        c(ov::MAP_PUT, vec![o(2), st("b"), st("2")], I),
        c(ov::MAP_PUT, vec![o(2), st("c"), st("3")], I),
        c(ov::MAP_PUT_ALL, vec![o(0), o(2)], &[]),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::PoolFull));
    let calls = vec![
        map_new(Ty::String, Ty::String, 4),
        c(ov::MAP_PUT, vec![o(0), st("a"), st("1")], I),
        c(ov::MAP_PUT, vec![o(0), st("b"), st("2")], I),
        mcopy(0, 1),
    ];
    assert_eq!(once(&calls, Dialect::V6), Err(FaultKind::PoolFull));
}

/// Brute force over the non-na values `v` (sorted ascending): `[median, mode, nearest(p),
/// linear(p)]` as the rows define them; `int` takes the lower middle and truncates toward zero.
fn bf_order(v: &[f64], p: f64, int: bool) -> [f64; 4] {
    if v.is_empty() {
        return [f64::NAN; 4];
    }
    let n = v.len();
    let med = if int {
        v[(n - 1) / 2]
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    };
    let (mut mode, mut best, mut i) = (v[0], 0, 0);
    while i < n {
        let mut j = i;
        while j < n && v[j] == v[i] {
            j += 1;
        }
        if j - i > best {
            best = j - i;
            mode = v[i];
        }
        i = j;
    }
    let (near, lin) = if (0.0..=100.0).contains(&p) {
        let r = (((p * n as f64) / 100.0).ceil() as usize).clamp(1, n);
        let x = (p * (n - 1) as f64) / 100.0;
        let k = x.floor() as usize;
        let fr = x - k as f64;
        let l = if fr == 0.0 || k + 1 >= n {
            v[k]
        } else {
            v[k] + fr * (v[k + 1] - v[k])
        };
        (v[r - 1], if int { l.trunc() } else { l })
    } else {
        (f64::NAN, f64::NAN)
    };
    [med, mode, near, lin]
}

/// Operator ruling 2026-10-16: `array.median / mode / percentile_nearest_rank /
/// percentile_linear_interpolation` (float and int) and the int `avg / variance / stdev` agree
/// with a sorted brute force on every bar of a growing array with na, duplicates, negatives and
/// `±0.0`, the array is left unchanged (no sort, no scratch), and the rows allocate nothing.
#[test]
fn array_order_statistics_and_int_moments_match_bruteforce() {
    for int in [false, true] {
        let (t, lane, new_id) = if int {
            (Ty::Int, Lane::I64, ov::ARRAY_NEW_INT)
        } else {
            (Ty::Float, Lane::F64, ov::ARRAY_NEW_FLOAT)
        };
        let out: &[Lane] = if int { I } else { F };
        let id = |f: OverloadId, i: OverloadId| if int { i } else { f };
        let mut owner = new(
            new_id,
            t,
            64,
            coll_block_bytes(CollKind::Array, t, Ty::Void, 64, 16),
            vec![it(0), if int { it(0) } else { ft(0.0) }],
        );
        owner.guard = Some(0);
        let p = (Lane::F64, Src::Feed(2));
        let mut calls = vec![
            owner,
            c(ov::ARRAY_PUSH, vec![o(0), (lane, Src::Feed(1))], &[]),
            c(id(ov::ARRAY_MEDIAN_F, ov::ARRAY_MEDIAN_I), vec![o(0)], out),
            c(id(ov::ARRAY_MODE_F, ov::ARRAY_MODE_I), vec![o(0)], out),
            c(
                id(
                    ov::ARRAY_PERCENTILE_NEAREST_RANK_F,
                    ov::ARRAY_PERCENTILE_NEAREST_RANK_I,
                ),
                vec![o(0), p],
                out,
            ),
            c(
                id(
                    ov::ARRAY_PERCENTILE_LINEAR_INTERPOLATION_F,
                    ov::ARRAY_PERCENTILE_LINEAR_INTERPOLATION_I,
                ),
                vec![o(0), p],
                out,
            ),
            c(ov::ARRAY_GET, vec![o(0), it(0)], out),
        ];
        if int {
            calls.push(c(ov::ARRAY_AVG_I, vec![o(0)], I));
            calls.push(c(ov::ARRAY_VARIANCE_I, vec![o(0), it(1)], I));
            calls.push(c(ov::ARRAY_STDEV_I, vec![o(0), it(0)], I));
        }
        let pcts = [
            0.0,
            10.0,
            25.0,
            33.3,
            50.0,
            87.5,
            100.0,
            -1.0,
            101.0,
            f64::NAN,
        ];
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let feed: Vec<Vec<f64>> = (0..60)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let x = if s % 9 == 0 {
                    f64::NAN
                } else if int {
                    ((s >> 20) % 13) as f64 - 6.0
                } else if s % 7 == 0 {
                    -0.0
                } else {
                    (((s >> 20) % 13) as f64 - 6.0) * 1.25
                };
                vec![(i == 0) as u8 as f64, x, pcts[i % pcts.len()]]
            })
            .collect();
        let (r, allocs) = run(&calls, &feed, Dialect::V6);
        let r = r.unwrap();
        assert_eq!(allocs, 0, "int {int}: allocated");
        let mut hist: Vec<f64> = Vec::new();
        for (i, f) in feed.iter().enumerate() {
            hist.push(f[1]);
            let mut v: Vec<f64> = hist.iter().copied().filter(|x| !x.is_nan()).collect();
            v.sort_by(|a, b| a.total_cmp(b));
            let want = bf_order(&v, f[2], int);
            for (k, &w) in want.iter().enumerate() {
                let g = r[i][2 + k];
                assert!(
                    (g.is_nan() && w.is_nan()) || g == w,
                    "int {int} bar {i} stat {k} p {}: {g} vs {w}",
                    f[2]
                );
            }
            // Unchanged: element 0 is still the first push.
            let g0 = r[i][6];
            assert!((g0.is_nan() && hist[0].is_nan()) || g0 == hist[0]);
            if int {
                let n = v.len() as f64;
                let (avg, var1, sd0) = if v.is_empty() {
                    (f64::NAN, f64::NAN, f64::NAN)
                } else {
                    let s: f64 = v.iter().sum();
                    let m = s / n;
                    let ss: f64 = v.iter().map(|x| (x - m) * (x - m)).sum();
                    let sd0 = if n > 1.0 {
                        (ss / (n - 1.0)).sqrt().trunc()
                    } else {
                        f64::NAN
                    };
                    ((s / n).trunc(), (ss / n).trunc(), sd0)
                };
                for (k, w) in [(7, avg), (8, var1), (9, sd0)] {
                    let g = r[i][k];
                    assert!(
                        (g.is_nan() && w.is_nan()) || g == w,
                        "int moments bar {i} col {k}: {g} vs {w}"
                    );
                }
            }
        }
    }
    // The documented ties: mode → the smallest of the most frequent; even int median → lower.
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 8, 16);
    let mut calls = vec![new(ov::ARRAY_NEW_INT, Ty::Int, 8, ib, vec![it(0), it(0)])];
    for x in [7, 3, 7, 3, 9, -1] {
        calls.push(c(ov::ARRAY_PUSH, vec![o(0), it(x)], &[]));
    }
    let k = calls.len();
    calls.push(c(ov::ARRAY_MODE_I, vec![o(0)], I));
    calls.push(c(ov::ARRAY_MEDIAN_I, vec![o(0)], I));
    calls.push(c(ov::ARRAY_AVG_I, vec![o(0)], I));
    let r = once(&calls, Dialect::V6).unwrap();
    assert_eq!((r[k], r[k + 1], r[k + 2]), (3.0, 3.0, 4.0));
}

/// Brute force of `pin:04-array-covariance-pairs`: pairs with an na side skipped, two passes.
fn bf_cov(xs: &[f64], ys: &[f64], biased: bool) -> f64 {
    let p: Vec<(f64, f64)> = xs
        .iter()
        .zip(ys)
        .filter(|(x, y)| !x.is_nan() && !y.is_nan())
        .map(|(x, y)| (*x, *y))
        .collect();
    if p.is_empty() {
        return f64::NAN;
    }
    let n = p.len() as f64;
    let (mut sa, mut sb) = (0.0, 0.0);
    for (x, y) in &p {
        sa += x;
        sb += y;
    }
    let (ma, mb) = (sa / n, sb / n);
    let mut sp = 0.0;
    for (x, y) in &p {
        sp += (x - ma) * (y - mb);
    }
    sp / if biased { n } else { n - 1.0 }
}

/// Brute force of `pin:04-array-percentrank`: `100 × #(x ≤ ref) / n` over the non-na values.
fn bf_prank(v: &[f64], k: usize, int: bool) -> f64 {
    let r = v[k];
    if r.is_nan() {
        return f64::NAN;
    }
    let live: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    let le = live.iter().filter(|x| **x <= r).count() as f64;
    let p = 100.0 * le / live.len() as f64;
    if int {
        p.trunc()
    } else {
        p
    }
}

/// r27 (R-P5): `array.covariance` (all four int / float pairings, biased and not) and
/// `array.percentrank` (float, int) agree with a brute force on every bar of two growing arrays
/// with na, duplicates and negatives, and allocate nothing after warm-up; the fault rows: an
/// index outside `[0, size)` (negative and na included) and arrays of different sizes are
/// `CollectionBounds`, a na reference `ScriptRefused`; one pair unbiased → na.
#[test]
fn array_covariance_and_percentrank_match_bruteforce() {
    let fb = coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, 64, 16);
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 64, 16);
    let mut a = new(ov::ARRAY_NEW_FLOAT, Ty::Float, 64, fb, vec![it(0), ft(0.0)]);
    a.guard = Some(0);
    let mut b = new(ov::ARRAY_NEW_INT, Ty::Int, 64, ib, vec![it(0), it(0)]);
    b.guard = Some(0);
    let idx = (Lane::I64, Src::Feed(3));
    let calls = vec![
        a,
        b,
        c(ov::ARRAY_PUSH, vec![o(0), (Lane::F64, Src::Feed(1))], &[]),
        c(ov::ARRAY_PUSH, vec![o(1), (Lane::I64, Src::Feed(2))], &[]),
        c(ov::ARRAY_COVARIANCE_AFAF, vec![o(0), o(0), it(1)], F),
        c(ov::ARRAY_COVARIANCE_AFAI, vec![o(0), o(1), it(1)], F),
        c(ov::ARRAY_COVARIANCE_AIAF, vec![o(1), o(0), it(0)], F),
        c(ov::ARRAY_COVARIANCE_AIAI, vec![o(1), o(1), it(0)], F),
        c(ov::ARRAY_PERCENTRANK_F, vec![o(0), idx], F),
        c(ov::ARRAY_PERCENTRANK_I, vec![o(1), idx], I),
    ];
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let feed: Vec<Vec<f64>> = (0..60u64)
        .map(|i| {
            let r = next();
            let x = if r % 7 == 0 {
                f64::NAN
            } else {
                (((r >> 20) % 13) as f64 - 6.0) * 1.25
            };
            let r2 = next();
            let y = if r2 % 6 == 0 {
                f64::NAN
            } else {
                ((r2 >> 20) % 11) as f64 - 5.0
            };
            let k = (next() % (i + 1)) as f64;
            vec![(i == 0) as u8 as f64, x, y, k]
        })
        .collect();
    let (r, allocs) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    assert_eq!(allocs, 0, "allocated after warm-up");
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for (i, f) in feed.iter().enumerate() {
        xs.push(f[1]);
        ys.push(f[2]);
        let k = f[3] as usize;
        let want = [
            bf_cov(&xs, &xs, true),
            bf_cov(&xs, &ys, true),
            bf_cov(&ys, &xs, false),
            bf_cov(&ys, &ys, false),
            bf_prank(&xs, k, false),
            bf_prank(&ys, k, true),
        ];
        for (j, w) in want.iter().enumerate() {
            let g = r[i][4 + j];
            assert!(
                (g.is_nan() && w.is_nan()) || g == *w,
                "bar {i} col {}: {g} vs {w}",
                4 + j
            );
        }
    }
    // Values on a fixed pair: x = [1, 2, 3, na], y = [2, 4, 7, 5] → pairs (1,2) (2,4) (3,7).
    let arr = |id: OverloadId, t: Ty, bytes: u32, vals: &[(Lane, Src)]| {
        let init = if t == Ty::Float { ft(0.0) } else { it(0) };
        let mut v = vec![new(id, t, 8, bytes, vec![it(0), init])];
        for x in vals {
            v.push(c(ov::ARRAY_PUSH, vec![o(0), *x], &[]));
        }
        v
    };
    let fb8 = coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, 8, 16);
    let ib8 = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 8, 16);
    let mut calls = arr(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        fb8,
        &[ft(1.0), ft(2.0), ft(3.0), (Lane::F64, Src::Na)],
    );
    let base = calls.len() as u16;
    let mut tail = arr(
        ov::ARRAY_NEW_INT,
        Ty::Int,
        ib8,
        &[it(2), it(4), it(7), it(5)],
    );
    for t in &mut tail {
        for x in &mut t.ins {
            if let (_, Src::Out(k, _)) = x {
                *k += base;
            }
        }
    }
    calls.extend(tail);
    let k = calls.len();
    calls.push(c(ov::ARRAY_COVARIANCE_AFAI, vec![o(0), o(base), it(1)], F));
    calls.push(c(ov::ARRAY_COVARIANCE_AFAI, vec![o(0), o(base), it(0)], F));
    calls.push(c(ov::ARRAY_PERCENTRANK_I, vec![o(base), it(3)], I));
    calls.push(c(ov::ARRAY_PERCENTRANK_F, vec![o(0), it(3)], F));
    calls.push(c(ov::ARRAY_PERCENTRANK_F, vec![o(0), it(1)], F));
    let r = once(&calls, Dialect::V6).unwrap();
    // means 2 and 13/3; Σ(dx·dy) = (−1)(−7/3) + 0 + (1)(8/3) = 5.
    assert!((r[k] - 5.0 / 3.0).abs() < 1e-12, "{}", r[k]);
    assert!((r[k + 1] - 2.5).abs() < 1e-12, "{}", r[k + 1]);
    // 5 is ≥ 2, 4, 5 of four → 75; an na reference → na; 2 is ≥ 1, 2 of three → 66.67.
    assert_eq!(r[k + 2], 75.0);
    assert!(r[k + 3].is_nan());
    assert_eq!(r[k + 4], 100.0 * 2.0 / 3.0);
    // Faults.
    let three = |extra: C| {
        let mut v = arr(
            ov::ARRAY_NEW_FLOAT,
            Ty::Float,
            fb8,
            &[ft(1.0), ft(2.0), ft(3.0)],
        );
        v.push(extra);
        once(&v, Dialect::V6)
    };
    for (ix, why) in [(it(3), "== size"), (it(-1), "negative"), (NA_I, "na")] {
        assert_eq!(
            three(c(ov::ARRAY_PERCENTRANK_F, vec![o(0), ix], F)),
            Err(FaultKind::CollectionBounds),
            "{why}"
        );
    }
    let mut sizes = arr(ov::ARRAY_NEW_FLOAT, Ty::Float, fb8, &[ft(1.0), ft(2.0)]);
    let b2 = sizes.len() as u16;
    sizes.push(new(
        ov::ARRAY_NEW_FLOAT,
        Ty::Float,
        8,
        fb8,
        vec![it(3), ft(0.0)],
    ));
    sizes.push(c(ov::ARRAY_COVARIANCE_AFAF, vec![o(0), o(b2), it(1)], F));
    assert_eq!(once(&sizes, Dialect::V6), Err(FaultKind::CollectionBounds));
    assert_eq!(
        three(c(ov::ARRAY_COVARIANCE_AFAF, vec![o(0), NA_I, it(1)], F)),
        Err(FaultKind::ScriptRefused)
    );
    let mut one = arr(ov::ARRAY_NEW_FLOAT, Ty::Float, fb8, &[ft(4.0)]);
    let k = one.len();
    one.push(c(ov::ARRAY_COVARIANCE_AFAF, vec![o(0), o(0), it(0)], F));
    one.push(c(ov::ARRAY_COVARIANCE_AFAF, vec![o(0), o(0), it(1)], F));
    let r = once(&one, Dialect::V6).unwrap();
    assert!(r[k].is_nan(), "one pair unbiased is 0/0");
    assert_eq!(r[k + 1], 0.0);
}

/// r27 inventory: the served tier-3 core rows no test named before — `array.new<T>`,
/// `new_color`, `from` (generic, bool, color), int `sort_indices` (both orders, na placement) and
/// `map.clear` — values and fault rows (`new<T>` negative size `ScriptRefused`, size past `__cap`
/// `PoolFull`), and 0 B/op over bars after the first.
#[test]
fn array_core_rows_new_from_sort_indices_map_clear() {
    let ib = coll_block_bytes(CollKind::Array, Ty::Int, Ty::Void, 4, 16);
    let fb = coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, 4, 16);
    let red = 0x00F2_3645_i64;
    let calls = vec![
        // 0..=2: array.new<int>(2, 5), get(1), size.
        new(ov::ARRAY_NEW, Ty::Int, 4, ib, vec![it(2), it(5)]),
        c(ov::ARRAY_GET, vec![o(0), it(1)], I),
        c(ov::ARRAY_SIZE, vec![o(0)], I),
        // 3..=4: array.new_color(3, red), get(-1) (v6 from the end).
        new(ov::ARRAY_NEW_COLOR, Ty::Color, 4, ib, vec![it(3), it(red)]),
        c(ov::ARRAY_GET, vec![o(3), it(-1)], I),
        // 5..=7: array.from(true, false, na) (bool), get(1), size.
        new(
            ov::ARRAY_FROM_AB,
            Ty::Bool,
            4,
            ib,
            vec![it(1), it(0), it(0)],
        ),
        c(ov::ARRAY_GET, vec![o(5), it(1)], I),
        c(ov::ARRAY_SIZE, vec![o(5)], I),
        // 8..=9: array.from(color…), get(0).
        new(ov::ARRAY_FROM_AC, Ty::Color, 4, ib, vec![it(red), it(0)]),
        c(ov::ARRAY_GET, vec![o(8), it(0)], I),
        // 10..=11: array.from<float>(1.5, na, -2.0), get(2).
        new(
            ov::ARRAY_FROM_AT,
            Ty::Float,
            4,
            fb,
            vec![ft(1.5), ft(f64::NAN), ft(-2.0)],
        ),
        c(ov::ARRAY_GET, vec![o(10), it(2)], F),
        // 12..=17: int sort_indices of [3, na, 1, 2] ascending / descending, joined.
        new(
            ov::ARRAY_FROM_AI,
            Ty::Int,
            4,
            ib,
            vec![it(3), NA_I, it(1), it(2)],
        ),
        new(
            ov::ARRAY_SORT_INDICES_AI,
            Ty::Int,
            4,
            ib,
            vec![o(12), it(0)],
        ),
        c(ov::ARRAY_JOIN_AI, vec![o(13), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(14), st("2,3,0,1")], I),
        new(
            ov::ARRAY_SORT_INDICES_AI,
            Ty::Int,
            4,
            ib,
            vec![o(12), it(1)],
        ),
        c(ov::ARRAY_JOIN_AI, vec![o(16), st(",")], I),
        c(ov::OP_EQ_STR, vec![o(17), st("1,0,3,2")], I),
        // 19..=24: map.clear empties the map; a later put works.
        map_new(Ty::Int, Ty::Float, 4),
        c(ov::MAP_PUT, vec![o(19), it(1), ft(1.0)], F),
        c(ov::MAP_PUT, vec![o(19), it(2), ft(2.0)], F),
        c(ov::MAP_CLEAR, vec![o(19)], &[]),
        c(ov::MAP_GET, vec![o(19), it(1)], F),
        c(ov::MAP_SIZE, vec![o(19)], I),
        c(ov::MAP_PUT, vec![o(19), it(2), ft(3.0)], F),
        c(ov::MAP_GET, vec![o(19), it(2)], F),
        c(ov::MAP_SIZE, vec![o(19)], I),
    ];
    let feed: Vec<Vec<f64>> = (0..4).map(|_| vec![0.0]).collect();
    let (r, a) = run(&calls, &feed, Dialect::V6);
    let r = r.unwrap();
    assert_eq!(a, 0, "allocated after warm-up");
    for row in &r {
        assert_eq!((row[1], row[2]), (5.0, 2.0), "new<int>");
        assert_eq!(row[4], red as f64, "new_color get(-1)");
        assert_eq!((row[6], row[7]), (0.0, 3.0), "from bool");
        assert_eq!(row[9], red as f64, "from color");
        assert_eq!(row[11], -2.0, "from<float>");
        assert_eq!(row[15], 1.0, "sort_indices ascending: na last");
        assert_eq!(row[18], 1.0, "sort_indices descending: na first");
        assert!(row[23].is_nan(), "cleared key is na");
        assert_eq!(row[24], 0.0, "cleared size");
        assert!(row[25].is_nan(), "put after clear is a new key");
        assert_eq!((row[26], row[27]), (3.0, 1.0));
    }
    // Fault rows of the generic creator.
    assert_eq!(
        once(
            &[new(ov::ARRAY_NEW, Ty::Int, 4, ib, vec![it(-1), it(0)])],
            Dialect::V6
        ),
        Err(FaultKind::ScriptRefused)
    );
    assert_eq!(
        once(
            &[new(ov::ARRAY_NEW, Ty::Int, 4, ib, vec![it(5), it(0)])],
            Dialect::V6
        ),
        Err(FaultKind::PoolFull)
    );
}
