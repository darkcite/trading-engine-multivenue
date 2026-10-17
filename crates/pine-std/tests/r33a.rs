// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33a — the B-group `ta.*` laws measured against TradingView's captured fixtures (issue register
//! CI-014 / CI-015 / CI-016 / CI-017; pins `pin:04-ewma-tv-arith`, `pin:U18-ta-na-propagation`,
//! `pin:06-cond-call-state`, `pin:U01-ema-seed`, `pin:U03-rising-falling` and the per-builtin
//! `pin:04-*-tv-order`). Each test runs the builtin over SYN-1 (impl-06 §4.1.1, the exact binary64
//! prelude) through `pine_std::testing` and compares every captured row of the named fixtures bit
//! for bit — the `.fix` files are read from `tests/conformance/fixtures/B/`.
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

/// SYN-1's step count (`bars 24484` in every syn fixture).
const SYN_BARS: usize = 24484;

/// SYN-1's columns: `syn`, `synna` (na when `syni < 100`), `synb` (`syni >= 500`, 1.0 / 0.0).
struct Syn {
    syn: Vec<f64>,
    synna: Vec<f64>,
    synb: Vec<f64>,
}

/// The prelude of every syn fixture, in its exact binary64 order.
fn syn1(n: usize) -> Syn {
    let mut s = 1.0f64;
    let (mut syn, mut synna, mut synb) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for _ in 0..n {
        let hi = (s / 127773.0).floor();
        let lo = s - hi * 127773.0;
        let t = 16807.0 * lo - 2836.0 * hi;
        s = if t > 0.0 { t } else { t + 2147483647.0 };
        let v = 1000.0 + (s - (s / 100000.0).floor() * 100000.0) / 100.0;
        let syni = (s - (s / 1000.0).floor() * 1000.0) as i64;
        syn.push(v);
        synna.push(if syni < 100 { f64::NAN } else { v });
        synb.push(if syni >= 500 { 1.0 } else { 0.0 });
    }
    Syn { syn, synna, synb }
}

/// TradingView's rows of fixture `id`: `(step, cells)`, a cell NaN for `na`.
fn tv(id: &str) -> Vec<(usize, Vec<f64>)> {
    let p = format!(
        "{}/../../tests/conformance/fixtures/B/{id}.fix",
        env!("CARGO_MANIFEST_DIR")
    );
    let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
    let mut rows = Vec::new();
    let mut on = false;
    for l in t.lines() {
        if l == "series" {
            on = true;
            continue;
        }
        if on {
            if l == "end" {
                break;
            }
            let mut c = l.split('\t');
            let k: usize = c.next().unwrap().parse().unwrap();
            let cells = c
                .map(|x| {
                    if x == "na" {
                        f64::NAN
                    } else {
                        x.parse().unwrap()
                    }
                })
                .collect();
            rows.push((k, cells));
        }
    }
    assert!(!rows.is_empty(), "{id}: no series rows");
    rows
}

/// One call: `(ov, inputs, output lanes, window length the site is sized for, guard column)`.
type Row = (OverloadId, Vec<(Lane, Src)>, Vec<Lane>, u32, Option<u16>);

/// Run `rows` over `n` one-minute bars with feed columns `cols` (NaN = na) in `dialect`; every
/// output of every call per bar, and the allocations counted from bar 64 on.
fn run(rows: &[Row], cols: &[&[f64]], n: usize, dialect: Dialect) -> (Vec<Vec<f64>>, u64) {
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins, outs, len, guard)| Call {
            ov: *id,
            inputs: ins,
            outputs: outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(*len),
            guard: *guard,
        })
        .collect();
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0; cols.len()],
        ..TestCtx::default()
    };
    let width: usize = rows.iter().map(|r| r.2.len()).sum();
    let mut got = vec![vec![0.0f64; width]; n];
    let bars: Vec<Bar> = (0..n)
        .map(|i| pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED))
        .collect();
    let mut before = 0u64;
    for (i, b) in bars.iter().enumerate() {
        if i == 64 {
            before = allocs();
        }
        for (c, col) in cols.iter().enumerate() {
            ctx.feed[c] = col[i];
        }
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

fn feed(c: u16) -> (Lane, Src) {
    (Lane::F64, Src::Feed(c))
}

fn int(v: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(v as u64))
}

fn flt(v: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(v.to_bits()))
}

fn one(id: OverloadId, ins: Vec<(Lane, Src)>, len: u32) -> Row {
    (id, ins, vec![Lane::F64], len, None)
}

/// Every captured row of `id`'s value column `col` (the `isna` column is `col − 1`) equals `ours`
/// bit for bit (`na` ↔ NaN); `ours[k]` is the series at step `k`.
fn check(id: &str, col: usize, ours: &[f64]) {
    let rows = tv(id);
    for (k, cells) in &rows {
        let (isna, want) = (cells[col - 1], cells[col]);
        let got = ours[*k];
        assert_eq!(
            isna == 1.0,
            got.is_nan(),
            "{id} step {k}: isna {isna}, ours {got}"
        );
        assert!(
            (want.is_nan() && got.is_nan()) || want.to_bits() == got.to_bits(),
            "{id} step {k}: TV {want:?}, ours {got:?}"
        );
    }
}

/// Column `c` of `g`.
fn col(g: &[Vec<f64>], c: usize) -> Vec<f64> {
    g.iter().map(|r| r[c]).collect()
}

/// `var float h = na; if synb { h := <call> }`: the call's value on the bars it ran, held.
fn held(g: &[Vec<f64>], c: usize, synb: &[f64]) -> Vec<f64> {
    let mut h = f64::NAN;
    g.iter()
        .zip(synb)
        .map(|(r, &b)| {
            if b != 0.0 {
                h = r[c];
            }
            h
        })
        .collect()
}

/// CI-014 `pin:U01-ema-seed` (B007 B009 B061): `ta.ema` is na for the first `length − 1` bars and
/// seeds at the Neumaier mean of the first `length` values (`pin:04-ewma-tv-arith`), in both
/// dialects.
#[test]
fn ema_seed_na_mask_b007() {
    let s = syn1(128);
    for d in [Dialect::V5, Dialect::V6] {
        let rows = vec![
            one(ov::TA_EMA, vec![feed(0), int(14)], 0),
            one(ov::TA_EMA, vec![feed(0), int(3)], 0),
            one(ov::TA_EMA, vec![feed(0), int(200)], 0),
        ];
        let (g, a) = run(&rows, &[&s.syn], 128, d);
        check("B007", 1, &col(&g, 0));
        check("B061", 1, &col(&g, 1));
        check("B009", 1, &col(&g, 2));
        assert_eq!(a, 0);
    }
}

/// CI-014 `pin:U18-ta-na-propagation` (B010 B016 B012 B018): `ta.ema` / `ta.rma` print na on an na
/// bar and resume from the held value; the RMA is Wilder's `(rma·(len − 1) + x) / len` (B013).
#[test]
fn ema_rma_na_bar_is_na_b010_b016() {
    let s = syn1(SYN_BARS);
    let rows = vec![
        one(ov::TA_EMA, vec![feed(1), int(14)], 0),
        one(ov::TA_RMA, vec![feed(1), int(14)], 0),
        one(ov::TA_EMA, vec![feed(0), int(20)], 0),
        one(ov::TA_EMA, vec![feed(1), int(20)], 0),
        one(ov::TA_RMA, vec![feed(0), int(20)], 0),
        one(ov::TA_RMA, vec![feed(1), int(20)], 0),
        one(ov::TA_RMA, vec![feed(0), int(14)], 0),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synna], SYN_BARS, Dialect::V6);
    check("B010", 1, &col(&g, 0));
    check("B016", 1, &col(&g, 1));
    check("B012", 1, &col(&g, 2));
    check("B012", 3, &col(&g, 3));
    check("B018", 1, &col(&g, 4));
    check("B018", 3, &col(&g, 5));
    check("B013", 1, &col(&g, 6));
    assert_eq!(a, 0);
}

/// CI-014 / CI-015 (B019 B022 B024 B089, `pin:04-wma-tv-order`): `ta.wma` sums oldest first,
/// unfused; over `synna` the window is indexed by bar, an na carried and printed as na; `ta.hma`
/// composes three such windows.
#[test]
fn wma_bar_indexed_carry_b022() {
    let s = syn1(SYN_BARS);
    let rows = vec![
        one(ov::TA_WMA, vec![feed(0), int(14)], 14),
        one(ov::TA_WMA, vec![feed(1), int(14)], 14),
        one(ov::TA_WMA, vec![feed(0), int(20)], 20),
        one(ov::TA_WMA, vec![feed(1), int(20)], 20),
        one(ov::TA_HMA, vec![feed(0), int(14)], 14),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synna], SYN_BARS, Dialect::V6);
    check("B019", 1, &col(&g, 0));
    check("B022", 1, &col(&g, 1));
    check("B024", 1, &col(&g, 2));
    check("B024", 3, &col(&g, 3));
    check("B089", 1, &col(&g, 4));
    assert_eq!(a, 0);
}

/// CI-016 `pin:06-cond-call-state` (B023 B041 B047): inside `if synb`, `ta.wma` and `ta.change`
/// index their history by bar (a skipped bar carries the last value) while `ta.macd`'s EMAs step
/// per execution.
#[test]
fn cond_call_state_b023_b041_b047() {
    let s = syn1(SYN_BARS);
    let g1 = Some(1u16);
    let rows: Vec<Row> = vec![
        (ov::TA_WMA, vec![feed(0), int(14)], vec![Lane::F64], 14, g1),
        (
            ov::TA_CHANGE_F,
            vec![feed(0), int(14)],
            vec![Lane::F64],
            14,
            g1,
        ),
        (
            ov::TA_MACD,
            vec![feed(0), int(12), int(26), int(9)],
            vec![Lane::F64; 3],
            0,
            g1,
        ),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synb], SYN_BARS, Dialect::V6);
    check("B023", 1, &held(&g, 0, &s.synb));
    check("B041", 1, &held(&g, 1, &s.synb));
    check("B047", 1, &held(&g, 4, &s.synb));
    assert_eq!(a, 0);
}

/// CI-014 (B043 B044 B045 B046 B048): `ta.macd` is three `ta.ema`s with TradingView's
/// arithmetic, seeds and na rule; the histogram is the compared column.
#[test]
fn macd_ema_law_b043_b046() {
    let s = syn1(SYN_BARS);
    let m = |c: u16, f: i64, sl: i64, g: i64| -> Row {
        (
            ov::TA_MACD,
            vec![feed(c), int(f), int(sl), int(g)],
            vec![Lane::F64; 3],
            0,
            None,
        )
    };
    let rows = vec![
        m(0, 12, 26, 9),
        m(0, 1, 1, 1),
        m(0, 200, 200, 200),
        m(1, 12, 26, 9),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synna], SYN_BARS, Dialect::V6);
    check("B043", 1, &col(&g, 2));
    check("B044", 1, &col(&g, 5));
    check("B045", 1, &col(&g, 8));
    check("B046", 1, &col(&g, 11));
    check("B048", 1, &col(&g, 2));
    check("B048", 3, &col(&g, 11));
    assert_eq!(a, 0);
}

/// CI-014 `pin:U18-ta-na-propagation` (B049 B052 B054): `ta.rsi`'s change is against the previous
/// bar's source, na included — an na bar and the one after it print na and move neither `rma`.
#[test]
fn rsi_bar_change_b052() {
    let s = syn1(SYN_BARS);
    let rows = vec![
        one(ov::TA_RSI, vec![feed(0), int(14)], 0),
        one(ov::TA_RSI, vec![feed(1), int(14)], 0),
        one(ov::TA_RSI, vec![feed(0), int(20)], 0),
        one(ov::TA_RSI, vec![feed(1), int(20)], 0),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synna], SYN_BARS, Dialect::V6);
    check("B049", 1, &col(&g, 0));
    check("B052", 1, &col(&g, 1));
    check("B054", 1, &col(&g, 2));
    check("B054", 3, &col(&g, 3));
    assert_eq!(a, 0);
}

/// CI-014 / CI-015 (B072 B073 B074 B075): `ta.swma`, `ta.alma`, `ta.linreg` in TradingView's
/// summation order over `synna`; `ta.percentrank` counts an na as never `≤` and prints 0 on an na
/// bar (`pin:04-swma-tv-order`, `pin:04-alma-tv-order`, `pin:04-linreg-tv-order`).
#[test]
fn swma_alma_linreg_percentrank_b072_b075() {
    let s = syn1(128);
    let rows = vec![
        one(ov::TA_SWMA, vec![feed(0)], 0),
        one(
            ov::TA_ALMA,
            vec![
                feed(0),
                int(9),
                flt(0.85),
                flt(6.0),
                (Lane::I64, Src::Const(0)),
            ],
            9,
        ),
        one(ov::TA_LINREG, vec![feed(0), int(14), int(0)], 14),
        one(ov::TA_PERCENTRANK, vec![feed(0), int(14)], 14),
    ];
    let (g, a) = run(&rows, &[&s.synna], 128, Dialect::V6);
    check("B072", 1, &col(&g, 0));
    check("B073", 1, &col(&g, 1));
    check("B074", 1, &col(&g, 2));
    check("B075", 1, &col(&g, 3));
    assert_eq!(a, 0);
}

/// CI-015 (B079 B080 B081 B087 B101): `ta.cci` / `ta.dev` take the mean from the Neumaier sum and
/// the deviations newest first; `ta.cmo`'s and `ta.cog`'s `math.sum` is the Neumaier sum; `ta.tsi`
/// is `ta.ema`'s law twice (`pin:04-dev-tv-order`, `pin:04-cog-tv-order`).
#[test]
fn warmup_group_b079_b101() {
    let s = syn1(128);
    let rows = vec![
        one(ov::TA_CCI, vec![feed(0), int(14)], 14),
        one(ov::TA_CMO, vec![feed(0), int(14)], 14),
        one(ov::TA_COG, vec![feed(0), int(14)], 14),
        one(ov::TA_DEV, vec![feed(0), int(14)], 14),
        one(ov::TA_TSI, vec![feed(0), int(14), int(14)], 0),
    ];
    let (g, a) = run(&rows, &[&s.syn], 128, Dialect::V6);
    check("B079", 1, &col(&g, 0));
    check("B080", 1, &col(&g, 1));
    check("B081", 1, &col(&g, 2));
    check("B087", 1, &col(&g, 3));
    check("B101", 1, &col(&g, 4));
    assert_eq!(a, 0);
}

/// CI-017 `pin:U03-rising-falling` (B064 v5, B065 v6, B066 v5, B067 v6): `ta.rising` /
/// `ta.falling` are the monotone reading — `false` (never na) while warming up — in both dialects.
#[test]
fn rising_falling_monotone_b064_b067() {
    let s = syn1(128);
    for (d, rise, fall) in [(Dialect::V5, "B064", "B066"), (Dialect::V6, "B065", "B067")] {
        let rows: Vec<Row> = vec![
            (
                ov::TA_RISING,
                vec![feed(0), int(3)],
                vec![Lane::I64],
                3,
                None,
            ),
            (
                ov::TA_FALLING,
                vec![feed(0), int(3)],
                vec![Lane::I64],
                3,
                None,
            ),
        ];
        let (g, a) = run(&rows, &[&s.syn], 128, d);
        for (id, c) in [(rise, 0usize), (fall, 1usize)] {
            for (k, cells) in tv(id) {
                assert_eq!(g[k][c], cells[0], "{id} step {k}");
            }
        }
        assert_eq!(a, 0);
    }
}
