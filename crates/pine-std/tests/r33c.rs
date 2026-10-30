// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33c — string, colour, time, `ta.sma` / `ta.vwma` and `ta.rci` laws measured against
//! TradingView's captured fixtures (pins `pin:04-na-reads-empty`, `pin:U07-str-match-none`,
//! `pin:U08-tostring-precision`, `pin:04-tf-change-first-false`, `pin:04-session-overnight-day`,
//! `pin:04-sma-fresh-sum`, `pin:04-rci-tv-arith`, `pin:04-rci-warmup`). Each test runs the
//! builtin through `pine_std::testing` and compares every captured row of the named fixture bit
//! for bit; the `.fix` files are read from `tests/conformance/fixtures/<G>/`. SYN-1 is impl-06
//! §4.1.1's exact binary64 prelude. B121 and D002 run on chart bars this tree does not hold, so
//! their law (`pin:04-sma-fresh-sum`) is held here against an independent replica instead (F031
//! is its syn-driven fixture).
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

/// SYN-1's columns: `syn`, `syni` (as f64), `synb` (`syni >= 500`, 1.0 / 0.0).
struct Syn {
    syn: Vec<f64>,
    syni: Vec<f64>,
    synb: Vec<f64>,
}

/// The prelude of every syn fixture, in its exact binary64 order.
fn syn1(n: usize) -> Syn {
    let mut s = 1.0f64;
    let (mut syn, mut syni, mut synb) = (
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
        let i = (s - (s / 1000.0).floor() * 1000.0) as i64;
        syn.push(v);
        syni.push(i as f64);
        synb.push(if i >= 500 { 1.0 } else { 0.0 });
    }
    Syn { syn, syni, synb }
}

/// TradingView's rows of fixture `id`: `(key, cells)`, a cell NaN for `na`.
fn tv(id: &str) -> Vec<(i64, Vec<f64>)> {
    let p = format!(
        "{}/../../tests/conformance/fixtures/{}/{id}.fix",
        env!("CARGO_MANIFEST_DIR"),
        &id[..1]
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
            let k: i64 = c.next().unwrap().parse().unwrap();
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

/// Run `rows` over bars opening at `opens` (bar `i` closes `span_ms` later) with feed columns
/// `cols` (NaN = na); every output of every call per bar, and the allocations counted from bar
/// 64 on (from the middle bar on in a shorter run).
fn run(rows: &[Row], cols: &[&[f64]], opens: &[i64], span_ms: i64) -> (Vec<Vec<f64>>, u64) {
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
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx {
        feed: vec![0.0; cols.len()],
        ..TestCtx::default()
    };
    let width: usize = rows.iter().map(|r| r.2.len()).sum();
    let n = opens.len();
    let mut got = vec![vec![0.0f64; width]; n];
    let bars: Vec<Bar> = (0..n)
        .map(|i| {
            let mut b = pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED);
            b.open_ms = opens[i];
            b.close_ms = opens[i] + span_ms;
            b
        })
        .collect();
    let start = if n > 64 { 64 } else { n / 2 };
    let mut before = 0u64;
    for (i, b) in bars.iter().enumerate() {
        if i == start {
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

/// One-minute bars `0..n` (the syn driver's step index is the row key).
fn minutes(n: usize) -> Vec<i64> {
    (0..n as i64).map(|i| i * 60_000).collect()
}

fn f(c: u16) -> (Lane, Src) {
    (Lane::F64, Src::Feed(c))
}

fn fi(c: u16) -> (Lane, Src) {
    (Lane::I64, Src::Feed(c))
}

fn int(v: i64) -> (Lane, Src) {
    (Lane::I64, Src::Const(v as u64))
}

fn flt(v: f64) -> (Lane, Src) {
    (Lane::F64, Src::Const(v.to_bits()))
}

fn st(x: &'static str) -> (Lane, Src) {
    (Lane::I64, Src::Str(x))
}

fn out(k: u16) -> (Lane, Src) {
    (Lane::I64, Src::Out(k, 0))
}

const NA_I: (Lane, Src) = (Lane::I64, Src::Na);

fn one(id: OverloadId, ins: Vec<(Lane, Src)>, lane: Lane, len: u32) -> Row {
    (id, ins, vec![lane], len, None)
}

/// Bit equality, `na` ↔ NaN.
fn same(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
}

/// Every captured row of `id`: `ours(key)` gives the row's cells, compared bit for bit.
fn check(id: &str, ours: impl Fn(i64) -> Vec<f64>) {
    for (k, cells) in tv(id) {
        let got = ours(k);
        assert_eq!(got.len(), cells.len(), "{id} row {k}: width");
        for (j, (&w, &g)) in cells.iter().zip(&got).enumerate() {
            assert!(same(w, g), "{id} row {k} column {j}: TV {w:?}, ours {g:?}");
        }
    }
}

/// `(isna, value)` of a value cell as the fixtures plot it.
fn isna_val(v: f64) -> Vec<f64> {
    if v.is_nan() {
        vec![1.0, f64::NAN]
    } else {
        vec![0.0, v]
    }
}

/// `pin:04-na-reads-empty` (A047): `color.new(na, 50)` is not na; its channels are `(0, 0, 0)`.
#[test]
fn color_new_of_na_is_black_a047() {
    let rows = vec![one(ov::COLOR_NEW, vec![NA_I, flt(50.0)], Lane::I64, 0)];
    let (g, a) = run(&rows, &[], &minutes(64), 60_000);
    check("A047", |k| {
        let c = g[k as usize][0];
        if c.is_nan() {
            return isna_val(f64::NAN);
        }
        let c = c as i64;
        let rgb = color::r(c) as f64 * 65536.0 + color::g(c) as f64 * 256.0 + color::b(c) as f64;
        isna_val(rgb)
    });
    assert_eq!(
        color::t(g[0][0] as i64),
        50,
        "the literal reading keeps transp"
    );
    assert_eq!(a, 0);
}

/// `pin:04-na-reads-empty` (A053 A054 A061 A063): an na string reads as `""` in `==`, `!=`
/// and `+`. A053 `na == na ? "ab" : na` → "ab"; A054 `na != na ? "ab" : na` → na; A061
/// `switch na` misses `"ab"` and `"cd"`; A063 `na + "ab"` → "ab".
#[test]
fn string_na_reads_empty_a053_a054_a061_a063() {
    let rows = vec![
        one(ov::OP_EQ_STR, vec![NA_I, NA_I], Lane::I64, 0),
        one(ov::OP_NE_STR, vec![NA_I, NA_I], Lane::I64, 0),
        one(ov::OP_EQ_STR, vec![NA_I, st("ab")], Lane::I64, 0),
        one(ov::OP_EQ_STR, vec![NA_I, st("cd")], Lane::I64, 0),
        one(ov::OP_ADD_STR, vec![NA_I, st("ab")], Lane::I64, 0),
        one(ov::STR_LENGTH, vec![out(4)], Lane::I64, 0),
    ];
    let (g, a) = run(&rows, &[], &minutes(64), 60_000);
    // `cond ? "ab" : na` as the fixtures plot it: `(isna, str.length)`.
    let pick = |hit: f64| isna_val(if hit == 1.0 { 2.0 } else { f64::NAN });
    check("A053", |k| pick(g[k as usize][0]));
    check("A054", |k| pick(g[k as usize][1]));
    check("A061", |k| {
        let r = &g[k as usize];
        pick(if r[2] == 1.0 || r[3] == 1.0 { 1.0 } else { 0.0 })
    });
    check("A063", |k| isna_val(g[k as usize][5]));
    assert_eq!(a, 0);
}

/// `pin:U07-str-match-none` (F063, both dialects): no match is na —
/// `str.match(str.tostring(syni), "x+")`.
#[test]
fn str_match_none_is_na_f063() {
    let s = syn1(64);
    let rows = vec![
        one(ov::STR_TOSTRING_I, vec![fi(0)], Lane::I64, 0),
        one(ov::STR_MATCH, vec![out(0), st("x+")], Lane::I64, 0),
    ];
    let (g, a) = run(&rows, &[&s.syni], &minutes(64), 60_000);
    check("F063", |k| {
        let v = g[k as usize][1];
        // `na(v) ? na : str.length(v)`: a match would be a non-na ref.
        if v.is_nan() {
            vec![1.0, f64::NAN]
        } else {
            vec![0.0, -1.0]
        }
    });
    assert_eq!(a, 0);
}

/// `pin:U08-tostring-precision` (F064, both dialects): the default `str.tostring(float)` keeps
/// up to 10 fraction digits — `str.length(str.tostring(syn / 3.0))`, `…(syn)` on all 64 rows.
#[test]
fn tostring_default_ten_digits_f064() {
    let s = syn1(64);
    let third: Vec<f64> = s.syn.iter().map(|x| x / 3.0).collect();
    let rows = vec![
        one(ov::STR_TOSTRING_F, vec![f(0)], Lane::I64, 0),
        one(ov::STR_LENGTH, vec![out(0)], Lane::I64, 0),
        one(ov::STR_TOSTRING_F, vec![f(1)], Lane::I64, 0),
        one(ov::STR_LENGTH, vec![out(2)], Lane::I64, 0),
    ];
    let (g, a) = run(&rows, &[&third, &s.syn], &minutes(64), 60_000);
    check("F064", |k| {
        let r = &g[k as usize];
        let mut v = isna_val(r[1]);
        v.extend(isna_val(r[3]));
        v
    });
    assert_eq!(a, 0);
}

/// `pin:04-tf-change-first-false` (F091): `timeframe.change("D")` is false on the first
/// execution, then true at each UTC day's first bar.
#[test]
fn timeframe_change_first_execution_false_f091() {
    let rows = vec![one(ov::TIMEFRAME_CHANGE, vec![st("D")], Lane::I64, 0)];
    let ts = minutes(3 * 1440);
    let (g, a) = run(&rows, &[], &ts, 60_000);
    check("F091", |k| vec![g[k as usize][0]]);
    for (i, r) in g.iter().enumerate() {
        assert_eq!(r[0], (i > 0 && i % 1440 == 0) as u8 as f64, "bar {i}");
    }
    assert_eq!(a, 0);
}

/// `pin:04-session-overnight-day` (D056): `time("60", "2200-0200:2")` is in session Sunday
/// 22:00 → Monday 02:00 (the Monday trading day) on every one of the 256 captured hourly bars.
#[test]
fn overnight_session_belongs_to_its_end_day_d056() {
    let rows = vec![one(
        ov::TIME_SSII_5_1,
        vec![st("60"), st("2200-0200:2"), int(0), int(0)],
        Lane::I64,
        0,
    )];
    let keys: Vec<i64> = tv("D056").iter().map(|r| r.0).collect();
    let (g, a) = run(&rows, &[], &keys, 3_600_000);
    check("D056", |k| {
        let i = keys.iter().position(|&x| x == k).unwrap();
        vec![g[i][0].is_nan() as u8 as f64]
    });
    assert_eq!(a, 0);
}

/// `pin:04-sma-fresh-sum` (F031): `ta.sma` is a fresh Neumaier sum `/ length` —
/// `(synb ? ta.sma(syn, 5) : 0.0) − ta.sma(syn, 5)` bit-exact on all 64 rows (the guarded call
/// steps per execution).
#[test]
fn sma_fresh_neumaier_sum_f031() {
    let s = syn1(64);
    let rows: Vec<Row> = vec![
        (ov::TA_SMA, vec![f(0), int(5)], vec![Lane::F64], 5, Some(1)),
        one(ov::TA_SMA, vec![f(0), int(5)], Lane::F64, 5),
    ];
    let (g, a) = run(&rows, &[&s.syn, &s.synb], &minutes(64), 60_000);
    check("F031", |k| {
        let k = k as usize;
        let probe = if s.synb[k] != 0.0 { g[k][0] } else { 0.0 };
        isna_val(probe - g[k][1])
    });
    assert_eq!(a, 0);
}

/// Neumaier's compensated sum, oldest first (the replica of `pin:04-sma-fresh-sum`).
fn neumaier(xs: &[f64]) -> f64 {
    let (mut s, mut c) = (0.0f64, 0.0f64);
    for &x in xs {
        let t = s + x;
        c += if s.abs() >= x.abs() {
            (s - t) + x
        } else {
            (x - t) + s
        };
        s = t;
    }
    s + c
}

/// `pin:04-sma-fresh-sum` (B121, D002 — chart bars, replica): `ta.vwma(src, len)` is
/// `(Σ src·vol / len) / (Σ vol / len)`, each Σ a fresh Neumaier sum oldest first, the products
/// unfused; `ta.sma` is `Σ / len` the same way. Volumes are whole (exact through the 1e-6 bar).
#[test]
fn vwma_and_sma_match_the_fresh_sum_replica_b121() {
    let s = syn1(600);
    let len = 14usize;
    let vol: Vec<f64> = s.syni.iter().map(|&i| 1.0 + i * 37.0).collect();
    let rows = [
        one(
            ov::TA_VWMA,
            vec![f(0), int(len as i64)],
            Lane::F64,
            len as u32,
        ),
        one(
            ov::TA_SMA,
            vec![f(0), int(len as i64)],
            Lane::F64,
            len as u32,
        ),
    ];
    let calls: Vec<Call> = rows
        .iter()
        .map(|(id, ins, outs, l, guard)| Call {
            ov: *id,
            inputs: ins,
            outputs: outs,
            site_bytes: OVERLOADS[id.0 as usize].state.block_bytes(*l),
            guard: *guard,
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
    let n = len as f64;
    for i in 0..s.syn.len() {
        let mut b = pine_vm::testkit::bar(i as u32, 1.0, barflag::CONFIRMED);
        b.volume_1e6 = vol[i] as i64 * 1_000_000;
        ctx.feed[0] = s.syn[i];
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok, "bar {i}");
        let (vwma, sma) = (h.out(0, 0), h.out(1, 0));
        if i + 1 < len {
            assert!(vwma.is_nan() && sma.is_nan(), "warm-up {i}");
            continue;
        }
        let w = i + 1 - len..=i;
        let pv: Vec<f64> = w.clone().map(|j| s.syn[j] * vol[j]).collect();
        let v: Vec<f64> = w.clone().map(|j| vol[j]).collect();
        let x: Vec<f64> = w.map(|j| s.syn[j]).collect();
        let want = (neumaier(&pv) / n) / (neumaier(&v) / n);
        assert!(same(vwma, want), "vwma bar {i}: {vwma:?} vs {want:?}");
        assert!(same(sma, neumaier(&x) / n), "sma bar {i}");
    }
}

/// `pin:04-rci-tv-arith`, `pin:04-rci-warmup` (B099): `ta.rci(syn, 14)` — na on rows 0…13,
/// then `100 · cov / (sd_x · sd_t)` of the window's ranks, bit-exact on all 128 rows.
#[test]
fn rci_rank_correlation_b099() {
    let s = syn1(128);
    let rows = vec![one(ov::TA_RCI, vec![f(0), int(14)], Lane::F64, 14)];
    let (g, a) = run(&rows, &[&s.syn], &minutes(128), 60_000);
    check("B099", |k| isna_val(g[k as usize][0]));
    assert_eq!(a, 0);
}

/// `ta.rci`'s edges (not captured; the stated readings): a strictly rising window is +100 and a
/// falling one −100 up to the law's own rounding (`100 · 2 / (√2 · √2)` for `length = 5`), a
/// flat window is na (`0 / 0`).
#[test]
fn rci_edges() {
    let up: Vec<f64> = (0..20).map(|i| i as f64).collect();
    let down: Vec<f64> = (0..20).map(|i| -(i as f64)).collect();
    let flat = vec![5.0; 20];
    let rows = vec![
        one(ov::TA_RCI, vec![f(0), int(5)], Lane::F64, 5),
        one(ov::TA_RCI, vec![f(1), int(5)], Lane::F64, 5),
        one(ov::TA_RCI, vec![f(2), int(5)], Lane::F64, 5),
    ];
    let (g, _) = run(&rows, &[&up, &down, &flat], &minutes(20), 60_000);
    for (i, r) in g.iter().enumerate() {
        if i < 5 {
            assert!(r.iter().all(|v| v.is_nan()), "warm-up {i}");
            continue;
        }
        let sd = 2.0f64.sqrt();
        let full = 100.0 * 2.0 / (sd * sd);
        assert_eq!((r[0], r[1]), (full, -full), "bar {i}");
        assert!(r[2].is_nan(), "flat {i}");
    }
}

/// `ta.pivot_point_levels(type, timeframe.change("D")[, developing])` over hourly bars opening
/// at `opens` with OHLC `px(i)`: per bar, the array's size and its 11 elements.
fn pivots(
    kind: &'static str,
    developing: bool,
    opens: &[i64],
    px: impl Fn(usize) -> [f64; 4],
) -> Vec<(f64, Vec<f64>)> {
    use pine_std::coll_block_bytes;
    let cap = 64u32;
    let ins = vec![
        int(elem::scalar(Ty::Float) as i64),
        int(0),
        int(cap as i64),
        st(kind),
        out(0),
        int(developing as i64),
    ];
    let tf = vec![st("D")];
    let size = vec![out(1)];
    let gets: Vec<Vec<(Lane, Src)>> = (0..11).map(|j| vec![out(1), int(j)]).collect();
    let (i64s, f64s) = ([Lane::I64], [Lane::F64]);
    let mut calls = vec![
        Call {
            ov: ov::TIMEFRAME_CHANGE,
            inputs: &tf,
            outputs: &i64s,
            site_bytes: OVERLOADS[ov::TIMEFRAME_CHANGE.0 as usize]
                .state
                .block_bytes(0),
            guard: None,
        },
        Call {
            ov: ov::TA_PIVOT_POINT_LEVELS,
            inputs: &ins,
            outputs: &i64s,
            site_bytes: coll_block_bytes(CollKind::Array, Ty::Float, Ty::Void, cap, 16),
            guard: None,
        },
        Call {
            ov: ov::ARRAY_SIZE,
            inputs: &size,
            outputs: &i64s,
            site_bytes: 0,
            guard: None,
        },
    ];
    for g in &gets {
        calls.push(Call {
            ov: ov::ARRAY_GET,
            inputs: g,
            outputs: &f64s,
            site_bytes: 0,
            guard: None,
        });
    }
    let mut h = Harness::build(&Script {
        calls: &calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: CHART,
    });
    let mut ctx = TestCtx::default();
    let mut got: Vec<[f64; 12]> = Vec::with_capacity(opens.len());
    let mut before = 0u64;
    for (i, &t) in opens.iter().enumerate() {
        if i == 64.min(opens.len() / 2) {
            before = allocs();
        }
        let [o, hi, lo, c] = px(i);
        let mut b = pine_vm::testkit::bar(i as u32, c, barflag::CONFIRMED);
        b.open_1e6 = scale::to_1e6(o).unwrap();
        b.high_1e6 = scale::to_1e6(hi).unwrap();
        b.low_1e6 = scale::to_1e6(lo).unwrap();
        b.open_ms = t;
        b.close_ms = t + 3_600_000;
        assert_eq!(h.step_bar(&b, &mut ctx), StepOutcome::Ok, "bar {i}");
        let mut row = [0.0f64; 12];
        for (j, v) in row.iter_mut().enumerate() {
            *v = h.out(2 + j, 0);
        }
        got.push(row);
    }
    assert_eq!(allocs() - before, 0, "allocation after warm-up");
    got.iter().map(|r| (r[0], r[1..].to_vec())).collect()
}

/// `pin:04-pivot-levels-provisional` (B112): `ta.pivot_point_levels("Traditional",
/// timeframe.change("D"))` is an 11-element array on every captured hourly bar.
#[test]
fn pivot_point_levels_is_eleven_levels_b112() {
    let keys: Vec<i64> = tv("B112").iter().map(|r| r.0).collect();
    let got = pivots("Traditional", false, &keys, |i| {
        let x = 80_000.0 + (i % 37) as f64;
        [x, x + 50.0, x - 50.0, x + 10.0]
    });
    check("B112", |k| {
        let i = keys.iter().position(|&x| x == k).unwrap();
        isna_val(got[i].0)
    });
}

/// The reference's anchor rule (not captured beyond the size): levels are na until the first
/// anchor that closes a period, then the previous period's levels hold until the next anchor;
/// `developing` recomputes from the period so far. Traditional `P = (H + L + C) / 3`.
#[test]
fn pivot_point_levels_anchor_and_developing() {
    // 2024-01-01 00:00 UTC, 72 hourly bars: three UTC days.
    let t0 = 1_704_067_200_000i64;
    let opens: Vec<i64> = (0..72).map(|i| t0 + i * 3_600_000).collect();
    let px = |i: usize| {
        let x = 100.0 + i as f64;
        [x, x + 0.5, x - 0.5, x + 0.25]
    };
    let held = pivots("Traditional", false, &opens, px);
    let dev = pivots("Traditional", true, &opens, px);
    let p = |h: f64, l: f64, c: f64| (h + l + c) / 3.0;
    for (i, (size, lv)) in held.iter().enumerate() {
        assert_eq!(*size, 11.0, "bar {i}");
        let day = i / 24;
        if day == 0 {
            assert!(lv.iter().all(|v| v.is_nan()), "before the first anchor {i}");
        } else {
            // The previous day: bars 24·(day − 1) … 24·day − 1.
            let (a, z) = (24 * (day - 1), 24 * day - 1);
            let want = p(px(z)[1], px(a)[2], px(z)[3]);
            assert!(same(lv[0], want), "held P bar {i}: {} vs {want}", lv[0]);
            assert!(same(lv[1], want * 2.0 - px(a)[2]), "R1 bar {i}");
        }
        let a = 24 * day;
        let want = p(px(i)[1], px(a)[2], px(i)[3]);
        assert!(same(dev[i].1[0], want), "developing P bar {i}");
    }
    let dm = pivots("DM", false, &opens, px);
    assert!(
        dm[30].1[3..].iter().all(|v| v.is_nan()),
        "DM defines P R1 S1"
    );
    assert!(dm[30].1[..3].iter().all(|v| !v.is_nan()));
}
