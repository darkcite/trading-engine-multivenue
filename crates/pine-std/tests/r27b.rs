// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r27b conformance bodies and laws (issue register CI-013, CI-015, CI-015a, CI-041): `ta.max`,
//! `ta.min`, `ta.range`, `math.round(x, p)`, `timeframe.from_seconds`, `session.isfirstbar` /
//! `islastbar`, and the variance family's fresh compensated moments — values, na, faults, and
//! zero allocation after warm-up, through `pine_std::testing`.
//!
//! COPY-DOCTRINE: test support, may allocate (outside the counted window).

use pine_ir::*;
use pine_spec::{ov, OVERLOADS};
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

/// One row per call: `(ov, inputs, output lanes, window length the site is sized for)`.
type Row = (OverloadId, Vec<(Lane, Src)>, Vec<Lane>, u32);

/// Run `rows` over `bars` (feed column 0 = `feed[i]`, NaN = na); every output of every call per
/// bar, and the allocations counted after bar `warm` (the harness's own vectors preallocated).
fn run(rows: &[Row], feed: &[f64], bars: &[Bar], warm: usize) -> (Vec<Vec<f64>>, u64) {
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins, outs, len)| Call {
            ov: *id,
            inputs: ins,
            outputs: outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(*len),
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0],
        ..TestCtx::default()
    };
    let width: usize = rows.iter().map(|r| r.2.len()).sum();
    let mut got = vec![vec![0.0f64; width]; bars.len()];
    let mut before = 0u64;
    for (i, b) in bars.iter().enumerate() {
        if i == warm {
            before = allocs();
        }
        ctx.feed[0] = feed[i];
        assert_eq!(h.step_bar(b, &mut ctx), StepOutcome::Ok, "bar {i}");
        let mut c = 0usize;
        for (k, r) in rows.iter().enumerate() {
            for j in 0..r.2.len() {
                got[i][c] = h.out(k, j);
                c += 1;
            }
        }
    }
    (got, allocs() - before)
}

/// `n` one-minute confirmed bars.
fn minute_bars(n: usize) -> Vec<Bar> {
    (0..n)
        .map(|i| pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED))
        .collect()
}

fn feed_f() -> (Lane, Src) {
    (Lane::F64, Src::Feed(0))
}

fn int(v: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(v as u64))
}

fn same(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a == b
}

/// SYN-1's first `n` `syn` values (impl-06 §4.1.1, the prelude's exact binary64 order).
fn syn1(n: usize) -> Vec<f64> {
    let (mut s, mut out) = (1.0f64, Vec::with_capacity(n));
    while out.len() < n {
        let hi = (s / 127773.0).floor();
        let lo = s - hi * 127773.0;
        let t = 16807.0 * lo - 2836.0 * hi;
        s = if t > 0.0 { t } else { t + 2147483647.0 };
        out.push(1000.0 + (s - (s / 100000.0).floor() * 100000.0) / 100.0);
    }
    out
}

/// CI-013 (B092 B094): the all-time extremes skip na, are na until the first value, and
/// allocate nothing; B092 / B094's first rows on SYN-1.
#[test]
fn ta_max_min_are_alltime_extremes_skipping_na() {
    let f = [f64::NAN, 3.0, f64::NAN, 1.0, 5.0, f64::NAN, 2.0, 7.0, 0.5];
    let rows: Vec<Row> = vec![
        (ov::TA_MAX, vec![feed_f()], vec![Lane::F64], 0),
        (ov::TA_MIN, vec![feed_f()], vec![Lane::F64], 0),
    ];
    let (g, a) = run(&rows, &f, &minute_bars(f.len()), 2);
    let hi = [f64::NAN, 3.0, 3.0, 3.0, 5.0, 5.0, 5.0, 7.0, 7.0];
    let lo = [f64::NAN, 3.0, 3.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.5];
    for i in 0..f.len() {
        assert!(same(g[i][0], hi[i]), "max bar {i}: {}", g[i][0]);
        assert!(same(g[i][1], lo[i]), "min bar {i}: {}", g[i][1]);
    }
    assert_eq!(a, 0);
    let syn = syn1(13);
    let (g, _) = run(&rows, &syn, &minute_bars(13), 0);
    // B092 rows 0, 1, 8, 12; B094 row 0.
    assert_eq!(
        (g[0][0], g[1][0], g[8][0], g[12][0]),
        (1168.07, 1752.49, 1779.23, 1844.92)
    );
    assert_eq!(g[0][1], 1168.07);
}

/// CI-013 (B098): `ta.range` = highest − lowest of the newest `length` non-na values, na while
/// warming up, the int overload exact in the i64 lane; a length beyond the site's window faults
/// `ScriptRefused`.
#[test]
fn ta_range_float_and_int() {
    let f = [4.0, f64::NAN, 9.0, 1.0, 6.0, f64::NAN, 2.0, 8.0];
    let rows: Vec<Row> = vec![
        (ov::TA_RANGE_F, vec![feed_f(), int(3)], vec![Lane::F64], 3),
        (
            ov::TA_RANGE_I,
            vec![(Lane::I64, Src::Feed(0)), int(3)],
            vec![Lane::I64],
            3,
        ),
    ];
    let (g, a) = run(&rows, &f, &minute_bars(f.len()), 3);
    // accepted: 4 | 9 | 1 | 6 | 2 | 8 → windows of 3 from the third accepted value.
    let want = [f64::NAN, f64::NAN, f64::NAN, 8.0, 8.0, 8.0, 5.0, 6.0];
    for i in 0..f.len() {
        assert!(same(g[i][0], want[i]), "float bar {i}: {}", g[i][0]);
        assert!(same(g[i][1], want[i]), "int bar {i}: {}", g[i][1]);
    }
    assert_eq!(a, 0);
    // A length beyond the site's window (sized for 3) faults.
    let over: Vec<Row> = vec![(ov::TA_RANGE_F, vec![feed_f(), int(4)], vec![Lane::F64], 3)];
    let calls: Vec<Call> = over
        .iter()
        .map(|(id, ins, outs, len)| Call {
            ov: *id,
            inputs: ins,
            outputs: outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(*len),
            guard: None,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![1.0],
        ..TestCtx::default()
    };
    let b = pine_vm::testkit::bar(0, 1.0, barflag::CONFIRMED);
    assert_ne!(h.step_bar(&b, &mut ctx), StepOutcome::Ok);
    assert_eq!(h.last_fault().map(|x| x.0), Some(FaultKind::ScriptRefused));
}

/// CI-013 (F016): `math.round(x, precision)` = `round(x·10^p)/10^p`, ties up (v6), na
/// propagates; F016's `math.round(syn, 3)` prints the 3-decimal rounding of `syn` (row 23:
/// `syn` = 1971.5700000000002, TradingView 1971.57).
#[test]
fn math_round_precision() {
    let f = [1168.07, 2.345, -2.345, 1.5, f64::NAN, 1234.5];
    let rows: Vec<Row> = vec![
        (ov::MATH_ROUND_F, vec![feed_f(), int(3)], vec![Lane::F64], 0),
        (ov::MATH_ROUND_F, vec![feed_f(), int(2)], vec![Lane::F64], 0),
        (
            ov::MATH_ROUND_F,
            vec![feed_f(), int(-2)],
            vec![Lane::F64],
            0,
        ),
        (
            ov::MATH_ROUND_F,
            vec![feed_f(), (Lane::I64, Src::Na)],
            vec![Lane::F64],
            0,
        ),
    ];
    let (g, a) = run(&rows, &f, &minute_bars(f.len()), 1);
    assert_eq!(a, 0);
    assert_eq!(g[0][0], 1168.07);
    assert_eq!(g[1][1], (2.345f64 * 100.0).round() / 100.0);
    assert_eq!(g[3][1], 1.5);
    assert_eq!(g[5][2], 1200.0);
    assert!(g[4][0].is_nan() && g[0][3].is_nan());
    for (i, x) in syn1(64).into_iter().enumerate() {
        let (g, _) = run(&rows[..1], &[x], &minute_bars(1), 0);
        assert_eq!(g[0][0], (x * 1000.0).round() / 1000.0, "F016 row {i}");
        if i == 23 {
            assert_eq!((x, g[0][0]), (1971.5700000000002, 1971.57));
        }
    }
}

/// CI-013 (F075 F076 F092): `timeframe.from_seconds` — the string's length as F075 / F092
/// print it (2, 2, 2 in both dialects; 7 s → 3), the reference's remarks, na → na; nothing
/// allocated after warm-up.
#[test]
fn timeframe_from_seconds_lengths() {
    let cases: [(i64, f64); 11] = [
        (86_400, 2.0),     // "1D"
        (604_800, 2.0),    // "1W"
        (3_600, 2.0),      // "60"
        (7, 3.0),          // "10S"
        (1, 2.0),          // "1S"
        (4, 2.0),          // "5S"
        (604_799, 2.0),    // "7D"
        (40_000_000, 3.0), // "12M"
        (2_628_003, 2.0),  // "1M"
        (86_399, 2.0),     // "1D"
        (1_441 * 60, 2.0), // "2D"
    ];
    let mut rows: Vec<Row> = Vec::new();
    for (s, _) in cases {
        let k = rows.len() as u16;
        rows.push((ov::TIMEFRAME_FROM_SECONDS, vec![int(s)], vec![Lane::I64], 0));
        rows.push((
            ov::STR_LENGTH,
            vec![(Lane::I64, Src::Out(k, 0))],
            vec![Lane::I64],
            0,
        ));
    }
    let k = rows.len() as u16;
    rows.push((
        ov::TIMEFRAME_FROM_SECONDS,
        vec![(Lane::I64, Src::Na)],
        vec![Lane::I64],
        0,
    ));
    rows.push((
        ov::STR_LENGTH,
        vec![(Lane::I64, Src::Out(k, 0))],
        vec![Lane::I64],
        0,
    ));
    let (g, a) = run(&rows, &[0.0; 4], &minute_bars(4), 1);
    for (i, (s, len)) in cases.iter().enumerate() {
        assert_eq!(g[3][2 * i + 1], *len, "from_seconds({s})");
    }
    assert!(g[3][2 * cases.len() + 1].is_nan(), "na seconds → na");
    assert_eq!(a, 0);
}

/// CI-041 (D058): on a 1-hour UTC chart the 00:00 bar is the session's first and the 23:00 bar
/// its last; a daily bar is both; stateless.
#[test]
fn session_first_last_bar_on_utc_day() {
    const H: i64 = 3_600_000;
    let mut bars: Vec<Bar> = (0..49)
        .map(|i| {
            let mut b = pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED);
            b.open_ms = 1_790_899_200_000 + H * i as i64; // 2026-10-02T00:00Z
            b.close_ms = b.open_ms + H;
            b
        })
        .collect();
    let mut d = bars[48];
    d.close_ms = d.open_ms + 24 * H;
    bars.push(d);
    let rows: Vec<Row> = vec![
        (ov::SESSION_ISFIRSTBAR_VAR, vec![], vec![Lane::I64], 0),
        (ov::SESSION_ISLASTBAR_VAR, vec![], vec![Lane::I64], 0),
    ];
    let (g, a) = run(&rows, &[0.0; 50], &bars, 1);
    for (i, r) in g.iter().enumerate().take(49) {
        assert_eq!(r[0], (i % 24 == 0) as u8 as f64, "first, bar {i}");
        assert_eq!(r[1], (i % 24 == 23) as u8 as f64, "last, bar {i}");
    }
    assert_eq!((g[49][0], g[49][1]), (1.0, 1.0), "a daily bar is both");
    assert_eq!(a, 0);
}

/// CI-015 / CI-015a (B025 B077 B078 B103; `pin:04-var-fresh-sum`, `pin:04-bbw-percent`):
/// the variance family is bit-exact against TradingView's captured rows on SYN-1, including
/// the rows from 15 on where rolling sums drifted.
#[test]
fn variance_family_matches_tradingview_rows() {
    let syn = syn1(16);
    let rows: Vec<Row> = vec![
        (
            ov::TA_VARIANCE,
            vec![feed_f(), int(14), (Lane::I64, Src::Const(1))],
            vec![Lane::F64],
            14,
        ),
        (
            ov::TA_STDEV,
            vec![feed_f(), int(14), (Lane::I64, Src::Const(1))],
            vec![Lane::F64],
            14,
        ),
        (
            ov::TA_BB,
            vec![feed_f(), int(14), (Lane::F64, Src::Const(2.0f64.to_bits()))],
            vec![Lane::F64, Lane::F64, Lane::F64],
            14,
        ),
        (
            ov::TA_BBW,
            vec![feed_f(), int(14), (Lane::F64, Src::Const(2.0f64.to_bits()))],
            vec![Lane::F64],
            14,
        ),
    ];
    let (g, a) = run(&rows, &syn, &minute_bars(16), 14);
    assert_eq!(a, 0);
    assert!(g[12][0].is_nan() && g[12][5].is_nan());
    // B103 row 15, B025 row 13, B077 row 15 (upper), B078 rows 13 and 15.
    assert_eq!(g[15][0], 53839.442181632854);
    assert_eq!(g[13][1], 233.6118903529658);
    assert_eq!(g[15][3], 1870.220842068578);
    assert_eq!(g[13][5], 64.43888212862731);
    assert_eq!(g[15][5], 66.0050694392415);
}

/// fuzz_meta seeds 286 / 456: `ta.tsi`'s change `src − prev` is na when both are the same
/// infinity (IEEE ∞ − ∞); it is not fed to the smoothers (`pin:04-ta-composed`), so a repeated
/// ∞ steps exactly as an na bar after it (prev holds ∞ either way), with no debug assertion.
#[test]
fn tsi_infinite_minus_infinite_change_is_not_fed() {
    let rows: Vec<Row> = vec![(
        ov::TA_TSI,
        vec![feed_f(), int(2), int(3)],
        vec![Lane::F64],
        0,
    )];
    let mut a = syn1(24);
    a[10] = f64::INFINITY;
    a[11] = f64::INFINITY;
    let mut b = a.clone();
    b[11] = f64::NAN;
    let bars = minute_bars(a.len());
    let (ga, n) = run(&rows, &a, &bars, 12);
    let (gb, _) = run(&rows, &b, &bars, 12);
    for i in 0..a.len() {
        assert_eq!(ga[i][0].to_bits(), gb[i][0].to_bits(), "bar {i}");
    }
    assert!(!ga[9][0].is_nan());
    assert_eq!(n, 0);
}
