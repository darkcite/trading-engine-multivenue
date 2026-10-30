// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §4.11 — the tier-1 screener's criterion benches (G3: Mac medians, no target; no test
//! asserts a rate). Setup outside the timed closure; `black_box` on every `&Bar`.
//!
//! * `pine_screen/bar_loop` — K ∈ {1, 8, 24} × `pvm7_supertrend` over 100 000 synthetic 1-m bars;
//! * `pine_screen/bar_loop_sub` — `htf_close.pine` (one `request.security("240")` sub-program);
//! * `pine_screen/open_map` — `BarFile::open` (map + validate) of 100 000 rows;
//! * `pine_screen/prepare_trades` — 10⁶ prints → 1-m bars;
//! * `barclock/open_ms` — each allowed timeframe.
//!
//! Output goes to `target/criterion/<group>/<bench>/report/`.

use std::hint::black_box;
use std::path::{Path, PathBuf};

use core_time::barclock;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use pine_ir::{barflag, symbol_hash, Bar, ChartInfo};
use pine_screen::bars::{
    bars_from_candles, bars_from_trades, write_bar_file, CandleRow, TradePrint,
};
use pine_screen::{
    bar_path, bar_source, compile_trial, run_group, BarFile, FeeRow, Loaded, NoProbe, ScreenCfg,
    TrialBars, TrialOutcome, TrialSpec, Workspace, SCREEN_LIMITS, SEL_NOTIONAL_1E6,
};
use pine_sema::LibCache;

const KEY: &str = "binance_usdm:btcusdt";
/// 2026-01-01T00:00:00Z.
const T0: i64 = 1_767_225_600_000;
const M: i64 = 60_000;
const DAY: i64 = 86_400_000;
const N: usize = 100_000;

fn src(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn minute_rows() -> Vec<Bar> {
    let mut rows = Vec::with_capacity(N);
    let mut prev = 0i64;
    for i in 0..N {
        let t = i as f64;
        let c = ((100.0
            + 8.0 * (2.0 * std::f64::consts::PI * t / 7_200.0).sin()
            + 2.5 * (2.0 * std::f64::consts::PI * t / 1_860.0).sin())
            * 100.0)
            .round() as i64
            * 10_000;
        let o = if i == 0 { c } else { prev };
        let open_ms = T0 + i as i64 * M;
        rows.push(Bar {
            open_1e6: o,
            high_1e6: o.max(c) + 20_000,
            low_1e6: o.min(c) - 20_000,
            close_1e6: c,
            volume_1e6: 1_000_000,
            open_ms,
            close_ms: open_ms + M,
            index: i as u32,
            flags: barflag::CONFIRMED,
        });
        prev = c;
    }
    rows
}

/// The 1-m file and its 4-h buckets, written under a temp root.
fn write_files() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pine-screen-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let rows = minute_rows();
    let sh = symbol_hash(KEY.as_bytes());
    let src: Vec<CandleRow> = rows
        .iter()
        .map(|b| CandleRow {
            open_ms: b.open_ms,
            open_1e6: b.open_1e6,
            high_1e6: b.high_1e6,
            low_1e6: b.low_1e6,
            close_1e6: b.close_1e6,
            volume_1e6: b.volume_1e6,
        })
        .collect();
    let mut h4 = Vec::new();
    bars_from_candles(&src, 60, 14_400, &mut h4).expect("4-h");
    for (tf, r) in [(60u32, &rows), (14_400u32, &h4)] {
        let p = bar_path(&dir, KEY, tf).expect("path");
        write_bar_file(&p, r, tf, sh, 1, bar_source::CANDLES, false).expect("write");
    }
    dir
}

fn compile(source: &str) -> Loaded {
    let tm: Vec<(String, String)> = Vec::new();
    let spec = TrialSpec {
        source,
        symbol_key: KEY,
        chart: ChartInfo {
            symbol_hash: symbol_hash(KEY.as_bytes()),
            timeframe_s: 60,
            venue: 1,
            mintick_1e6: 10_000,
            pointvalue_1e6: 1_000_000,
            qty_step_1e6: 1_000,
            ..ChartInfo::default()
        },
        syminfo_strs: ["BTCUSDT", "BTC", "USDT", "crypto"],
        inputs: &[],
        ticker_map: &tm,
        limits: SCREEN_LIMITS,
    };
    compile_trial(&spec, &mut LibCache::empty()).unwrap_or_else(|e| panic!("{e:?}"))
}

fn cfg() -> ScreenCfg {
    ScreenCfg {
        from_ms: T0 + 2 * DAY,
        to_ms: T0 + 69 * DAY,
        notional_1e6: SEL_NOTIONAL_1E6,
        fee: FeeRow {
            venue: 1,
            class: 1,
            maker_bps: 2,
            taker_bps: 5,
        },
    }
}

fn bench_bar_loop(c: &mut Criterion) {
    let dir = write_files();
    let st = compile(&src("pine-lang/tests/scripts/pvm7_supertrend.pine"));
    let tb = TrialBars::open(&dir, KEY, 60, &[], None).expect("open");
    let cfg = cfg();
    let mut g = c.benchmark_group("pine_screen/bar_loop");
    g.sample_size(10);
    for k in [1usize, 8, 24] {
        let refs: Vec<&Loaded> = (0..k).map(|_| &st).collect();
        let mut ws = Workspace::new();
        let mut out = vec![TrialOutcome::default(); k];
        g.throughput(Throughput::Elements((N * k) as u64));
        g.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, _| {
            b.iter(|| {
                let s = run_group(&refs, &tb, &cfg, &mut ws, &mut out, &mut NoProbe).expect("run");
                black_box(s)
            });
        });
    }
    g.finish();
    let htf = compile(&src("pine-screen/tests/probes/htf_close.pine"));
    let needs = htf.htf_needs().to_vec();
    let tb = TrialBars::open(&dir, KEY, 60, &needs, None).expect("open with 4-h");
    let mut ws = Workspace::new();
    let mut out = vec![TrialOutcome::default(); 1];
    let mut g = c.benchmark_group("pine_screen/bar_loop_sub");
    g.sample_size(10);
    g.throughput(Throughput::Elements(N as u64));
    g.bench_function("htf_close", |b| {
        b.iter(|| {
            let s = run_group(&[&htf], &tb, &cfg, &mut ws, &mut out, &mut NoProbe).expect("run");
            black_box(s)
        });
    });
    g.finish();
    let p = bar_path(&dir, KEY, 60).expect("path");
    let mut g = c.benchmark_group("pine_screen/open_map");
    g.throughput(Throughput::Elements(N as u64));
    g.bench_function("100k", |b| {
        b.iter(|| {
            let f = BarFile::open(black_box(&p)).expect("open");
            black_box(&f.rows()[N - 1]);
        });
    });
    g.finish();
    let _ = std::fs::remove_dir_all(dir);
}

fn bench_prepare_trades(c: &mut Criterion) {
    const P: usize = 1_000_000;
    let mut prints = Vec::with_capacity(P);
    for i in 0..P {
        prints.push(TradePrint {
            t_ms: T0 + (i as i64) * 50,
            px_1e6: 100_000_000 + ((i % 97) as i64) * 10_000,
            qty_1e6: 1_000 + (i % 13) as i64,
            seq: i as u64,
        });
    }
    let cov = [(T0, T0 + (P as i64) * 50)];
    let mut out = Vec::new();
    let mut g = c.benchmark_group("pine_screen/prepare_trades");
    g.sample_size(10);
    g.throughput(Throughput::Elements(P as u64));
    g.bench_function("1m", |b| {
        b.iter(|| {
            black_box(bars_from_trades(black_box(&prints), 60, &cov, &mut out).expect("bars"))
        });
    });
    g.finish();
}

fn bench_barclock(c: &mut Criterion) {
    let mut g = c.benchmark_group("barclock/open_ms");
    for tf in barclock::ALLOWED_TF_S {
        g.bench_with_input(BenchmarkId::from_parameter(tf), &tf, |b, &tf| {
            let mut t = T0 + 12_345;
            b.iter(|| {
                t += 61_001;
                black_box(barclock::open_ms(black_box(t), tf))
            });
        });
    }
    g.finish();
}

criterion_group!(
    pine_screen_benches,
    bench_bar_loop,
    bench_prepare_trades,
    bench_barclock
);
criterion_main!(pine_screen_benches);
