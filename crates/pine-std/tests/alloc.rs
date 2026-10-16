// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! 0 B/op per body family (the audit found only `tests/rank.rs` counting): `str.match`
//! recompiling a changed pattern mid-run (A7.1), `strategy.*` → `push_intent`, `time.*` (session
//! parse, tz lookup), `ta.correlation`, `ta.vwap`, and the A7.2 string builders. A counting
//! global allocator in this test binary counts this thread's allocations inside each measured
//! step (every step after the first); each test asserts zero.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::{ov, params, DefaultKind, OVERLOADS};
use pine_std::testing::{Call, Harness, Script, Src, TestCtx, CHART};
use pine_vm::testkit::bar;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    /// Count only this thread's allocations inside a measured step (tests run in parallel).
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

/// One call row: overload, window, output lanes.
type Row = (OverloadId, Vec<(Lane, Src)>, Vec<Lane>);

/// Step `rows` over `n` bars (feed columns from `feed(i)`, bar open `open(i)`); every step must
/// be `Ok`. Returns every row's first output per bar and this thread's allocation count inside
/// the steps after the first.
fn run(
    rows: &[Row],
    d: Dialect,
    intent_cap: u32,
    n: usize,
    feed: impl Fn(usize) -> Vec<f64>,
    open: impl Fn(usize) -> i64,
) -> (Vec<Vec<f64>>, u64) {
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins, outs)| Call {
            ov: *id,
            inputs: ins,
            outputs: outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(8),
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: d,
        intent_cap,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: feed(0),
        ..TestCtx::default()
    };
    let mut out = Vec::with_capacity(n);
    let mut allocs = 0;
    for i in 0..n {
        let f = feed(i);
        ctx.feed.copy_from_slice(&f);
        let mut b = bar(i as u32, 100.0 + (i % 7) as f64, barflag::CONFIRMED);
        b.open_ms = open(i);
        b.close_ms = b.open_ms + 60_000;
        ctx.now_ms = b.close_ms - 1;
        let measured = i > 0;
        let a0 = ALLOCS.load(Ordering::Relaxed);
        ON.with(|c| c.set(measured));
        let r = h.step_bar(&b, &mut ctx);
        ON.with(|c| c.set(false));
        if measured {
            allocs += ALLOCS.load(Ordering::Relaxed) - a0;
        }
        assert_eq!(r, StepOutcome::Ok, "bar {i}: {:?}", h.last_fault());
        out.push((0..rows.len()).map(|k| h.out(k, 0)).collect());
    }
    (out, allocs)
}

fn st(x: &'static str) -> (Lane, Src) {
    (Lane::I64, Src::Str(x))
}
fn it(x: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(x as u64))
}
fn ft(x: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(x.to_bits()))
}
fn o(k: u16) -> (Lane, Src) {
    (Lane::I64, Src::Out(k, 0))
}
fn minutes(i: usize) -> i64 {
    // 2024-01-07 (a Sunday) 00:00 UTC, one bar a minute.
    1_704_585_600_000 + i as i64 * 60_000
}

#[test]
fn str_match_recompiling_mid_run_is_zero_alloc() {
    // The pattern alternates between "x[0-9]+" and "[0-9]+" every bar: a recompile per bar.
    let rows: Vec<Row> = vec![
        (
            ov::STR_SUBSTRING,
            vec![st("x[0-9]+"), (Lane::I64, Src::Feed(0)), it(7)],
            vec![Lane::I64],
        ),
        (ov::STR_MATCH, vec![st("ax123b"), o(0)], vec![Lane::I64]),
        (ov::STR_LENGTH, vec![o(1)], vec![Lane::I64]),
    ];
    let (g, a) = run(&rows, Dialect::V6, 0, 64, |i| vec![(i % 2) as f64], minutes);
    for (i, r) in g.iter().enumerate() {
        assert_eq!(
            r[2],
            (4 - (i % 2)) as f64,
            "bar {i}: the pattern really changed"
        );
    }
    assert_eq!(a, 0, "str.match recompile allocated");
}

/// The window of `id` in `d`: `set` by name, else the spec default, else na.
fn win(id: OverloadId, d: Dialect, set: &[(&str, Src)]) -> Vec<(Lane, Src)> {
    let mut v = Vec::new();
    for p in params(&OVERLOADS[id.0 as usize]) {
        if !p.versions.has(d) {
            continue;
        }
        let lane = if p.sig.ty.ty == Ty::Float {
            Lane::F64
        } else {
            Lane::I64
        };
        let src = match set.iter().find(|(n, _)| *n == p.name.as_str()) {
            Some((_, s)) => *s,
            None => match p.default.kind {
                DefaultKind::Int | DefaultKind::Float | DefaultKind::Bool => {
                    Src::Const(p.default.bits)
                }
                DefaultKind::Str => Src::Str(""),
                DefaultKind::Value => Src::Const(0),
                _ => Src::Na,
            },
        };
        v.push((lane, src));
    }
    v
}

#[test]
fn strategy_push_intent_is_zero_alloc() {
    let d = Dialect::V6;
    let rows: Vec<Row> = vec![
        (
            ov::STRATEGY_ENTRY,
            win(
                ov::STRATEGY_ENTRY,
                d,
                &[
                    ("id", Src::Str("L")),
                    ("direction", Src::Const(0)),
                    ("qty", Src::Feed(0)),
                    ("comment", Src::Str("go")),
                ],
            ),
            vec![],
        ),
        (
            ov::STRATEGY_CLOSE,
            win(ov::STRATEGY_CLOSE, d, &[("id", Src::Str("L"))]),
            vec![],
        ),
        (
            ov::STRATEGY_CANCEL,
            win(ov::STRATEGY_CANCEL, d, &[("id", Src::Str("L"))]),
            vec![],
        ),
    ];
    let (_, a) = run(&rows, d, 8, 64, |i| vec![1.0 + i as f64], minutes);
    assert_eq!(a, 0, "strategy.* push_intent allocated");
}

#[test]
fn time_session_parse_and_tz_lookup_are_zero_alloc() {
    let rows: Vec<Row> = vec![
        (
            ov::TIME_SSII_5_1,
            vec![st("1"), st("0930-1600:23456"), it(0), it(0)],
            vec![Lane::I64],
        ),
        (
            ov::TIME_SSII_5_2,
            vec![
                st("1"),
                st("2200-0200"),
                st("America/New_York"),
                it(0),
                it(0),
            ],
            vec![Lane::I64],
        ),
        (
            ov::TIME_SSII_5_2,
            vec![st("1"), st(""), st("UTC+5:30"), it(0), it(0)],
            vec![Lane::I64],
        ),
    ];
    let (g, a) = run(&rows, Dialect::V6, 0, 2880, |_| vec![0.0], minutes);
    assert!(g.iter().any(|r| !r[0].is_nan()) && g.iter().any(|r| r[0].is_nan()));
    assert_eq!(a, 0, "time.* allocated");
}

#[test]
fn ta_correlation_is_zero_alloc() {
    let rows: Vec<Row> = vec![(
        ov::TA_CORRELATION,
        vec![(Lane::F64, Src::Feed(0)), (Lane::F64, Src::Feed(1)), it(8)],
        vec![Lane::F64],
    )];
    let f = |i: usize| {
        let x = ((i * 37) % 11) as f64;
        let y = if i % 13 == 5 {
            f64::NAN
        } else {
            x * 0.5 + (i % 3) as f64
        };
        vec![x, y]
    };
    let (g, a) = run(&rows, Dialect::V6, 0, 4096, f, minutes);
    assert!(g.iter().any(|r| r[0].is_finite()));
    assert_eq!(a, 0, "ta.correlation allocated");
}

#[test]
fn ta_vwap_is_zero_alloc() {
    let rows: Vec<Row> = vec![
        (
            ov::TA_VWAP_FFF,
            vec![
                (Lane::F64, Src::Feed(0)),
                (Lane::I64, Src::Feed(1)),
                ft(2.0),
            ],
            vec![Lane::F64; 3],
        ),
        (
            ov::TA_VWAP_F,
            vec![(Lane::F64, Src::Feed(0)), (Lane::I64, Src::Feed(1))],
            vec![Lane::F64],
        ),
    ];
    let f = |i: usize| vec![100.0 + (i % 17) as f64, (i % 50 == 0) as u8 as f64];
    let (g, a) = run(&rows, Dialect::V6, 0, 1024, f, minutes);
    assert!(g.iter().all(|r| r[0].is_finite()));
    assert_eq!(a, 0, "ta.vwap allocated");
}

#[test]
fn a7_string_builders_are_zero_alloc() {
    use Ty::{Float as F, Int as I};
    let mut types = 0u64;
    for (k, t) in [F, I].iter().enumerate() {
        types |= (*t as u64) << (4 * k);
    }
    let rows: Vec<Row> = vec![
        // 0: a SCRATCH source that changes per bar.
        (
            ov::STR_SUBSTRING,
            vec![st("abcDEFghiJKL"), (Lane::I64, Src::Feed(0)), it(12)],
            vec![Lane::I64],
        ),
        (ov::STR_UPPER, vec![o(0)], vec![Lane::I64]),
        (ov::STR_LOWER, vec![o(0)], vec![Lane::I64]),
        (
            ov::STR_REPLACE,
            vec![o(0), st("h"), st("HH"), it(0)],
            vec![Lane::I64],
        ),
        (
            ov::STR_REPLACE_ALL,
            vec![o(0), st("J"), st("--")],
            vec![Lane::I64],
        ),
        (ov::STR_REPEAT, vec![o(0), it(3), st(",")], vec![Lane::I64]),
        (
            ov::STR_FORMAT_F,
            vec![
                it(types as i64),
                st("{0,number,#.##} / {1}"),
                (Lane::F64, Src::Feed(1)),
                it(7),
            ],
            vec![Lane::I64],
        ),
        (
            ov::STR_FORMAT_TIME,
            vec![
                (Lane::I64, Src::Reserved(Lane::I64, abi::I_TIME)),
                st("yyyy-MM-dd HH:mm Z"),
                st("Asia/Tokyo"),
            ],
            vec![Lane::I64],
        ),
        (ov::STR_LENGTH, vec![o(6)], vec![Lane::I64]),
    ];
    let f = |i: usize| vec![(i % 6) as f64, 1.0 + i as f64 / 7.0];
    let (g, a) = run(&rows, Dialect::V6, 0, 256, f, minutes);
    assert!(g.iter().all(|r| r[8] >= 5.0), "str.format wrote its output");
    assert_eq!(a, 0, "A7 string builders allocated");
}
