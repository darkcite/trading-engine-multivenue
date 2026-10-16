// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Shared helpers: synthetic 1-h bar files, the chart and trial specs, a recording probe.
#![allow(dead_code)] // each test binary uses a subset

use pine_ir::{barflag, symbol_hash, Bar, ChartInfo, InputOverride};
use pine_screen::{
    bar_source, compile_trial, BarFile, CompileRefusal, FeeRow, Loaded, LoopProbe, ScreenCfg,
    TrialBars, TrialSpec, SCREEN_LIMITS, SEL_NOTIONAL_1E6,
};
use pine_sema::LibCache;

/// The chart every test screens.
pub const CHART_KEY: &str = "binance_usdm:btcusdt";
/// 2026-01-01T00:00:00Z.
pub const T0_MS: i64 = 1_767_225_600_000;
/// One hour.
pub const H_MS: i64 = 3_600_000;
/// One day.
pub const DAY_MS: i64 = 86_400_000;

/// The chart facts: mintick 0.01, linear, qty step 0.001.
pub fn chart() -> ChartInfo {
    ChartInfo {
        symbol_hash: symbol_hash(CHART_KEY.as_bytes()),
        timeframe_s: 3600,
        mintick_1e6: 10_000,
        pointvalue_1e6: 1_000_000,
        qty_step_1e6: 1_000,
        ..ChartInfo::default()
    }
}

/// Price → ×1e6 on the 0.01 grid.
pub fn px(v: f64) -> i64 {
    (v * 100.0).round() as i64 * 10_000
}

/// Hourly rows from `closes`: open = previous close, high/low 0.05 outside the body.
pub fn hourly_rows(closes_1e6: &[i64]) -> Vec<Bar> {
    let mut rows = Vec::with_capacity(closes_1e6.len());
    for (i, &c) in closes_1e6.iter().enumerate() {
        let o = if i == 0 { c } else { closes_1e6[i - 1] };
        let open_ms = T0_MS + i as i64 * H_MS;
        rows.push(Bar {
            open_1e6: o,
            high_1e6: o.max(c) + 50_000,
            low_1e6: o.min(c) - 50_000,
            close_1e6: c,
            volume_1e6: 1_000_000,
            open_ms,
            close_ms: open_ms + H_MS,
            index: i as u32,
            flags: barflag::CONFIRMED,
        });
    }
    rows
}

/// A few sine cycles around 100, on the cent grid (`n` hourly closes).
pub fn sine_closes(n: usize) -> Vec<i64> {
    (0..n)
        .map(|i| {
            let t = i as f64;
            px(100.0
                + 8.0 * (2.0 * std::f64::consts::PI * t / 120.0).sin()
                + 2.5 * (2.0 * std::f64::consts::PI * t / 31.0).sin())
        })
        .collect()
}

/// The chart file built in memory.
pub fn bars_of(closes_1e6: &[i64]) -> TrialBars {
    let f = BarFile::from_rows(
        hourly_rows(closes_1e6),
        3600,
        symbol_hash(CHART_KEY.as_bytes()),
        0,
        bar_source::CANDLES,
    )
    .expect("synthetic rows validate");
    TrialBars::from_files(f, Vec::new())
}

/// The screen configuration: window `[T0 + from_day, T0 + to_day)`, $1 000, taker 5 bps.
pub fn cfg(from_day: i64, to_day: i64) -> ScreenCfg {
    ScreenCfg {
        from_ms: T0_MS + from_day * DAY_MS,
        to_ms: T0_MS + to_day * DAY_MS,
        notional_1e6: SEL_NOTIONAL_1E6,
        fee: FeeRow {
            venue: 0,
            class: 0,
            maker_bps: 2,
            taker_bps: 5,
        },
    }
}

/// Compile `src` on the chart with `inputs`.
pub fn compile_with(src: &str, inputs: &[InputOverride]) -> Result<Loaded, CompileRefusal> {
    let tm: Vec<(String, String)> = Vec::new();
    let spec = TrialSpec {
        source: src,
        symbol_key: CHART_KEY,
        chart: chart(),
        syminfo_strs: ["BTCUSDT", "BTC", "USDT", "crypto"],
        inputs,
        ticker_map: &tm,
        limits: SCREEN_LIMITS,
    };
    compile_trial(&spec, &mut LibCache::empty())
}

/// Compile `src` with its declared inputs.
pub fn compile(src: &str) -> Result<Loaded, CompileRefusal> {
    compile_with(src, &[])
}

/// One recorded evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct Eval {
    /// Candidate.
    pub k: u32,
    /// The evaluated bar's index.
    pub index: u32,
    /// Its flags.
    pub flags: u32,
    /// `step_tick`.
    pub intrabar: bool,
    /// The plots after it.
    pub plots: Vec<f64>,
}

/// One recorded publication of a sub-program's HTF bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pub {
    /// Store slot.
    pub slot: u32,
    /// The HTF bar's index, open and close.
    pub htf_index: u32,
    pub htf_open_ms: i64,
    pub htf_close_ms: i64,
    /// The chart row current at the publication.
    pub chart_row: u32,
    /// `evals.len()` when it happened (orders publications against evaluations).
    pub evals_before: usize,
}

/// A probe that records every evaluation (test-side allocation is fine).
#[derive(Default)]
pub struct Rec {
    /// Evaluations, in order.
    pub evals: Vec<Eval>,
    /// Publications, in order.
    pub pubs: Vec<Pub>,
    /// `before_loop` / `after_loop` calls.
    pub brackets: (u32, u32),
}

impl LoopProbe for Rec {
    fn before_loop(&mut self) {
        self.brackets.0 += 1;
    }
    fn after_loop(&mut self) {
        self.brackets.1 += 1;
    }
    fn on_publish(&mut self, slot: u32, htf: &Bar, chart_row: u32) {
        self.pubs.push(Pub {
            slot,
            htf_index: htf.index,
            htf_open_ms: htf.open_ms,
            htf_close_ms: htf.close_ms,
            chart_row,
            evals_before: self.evals.len(),
        });
    }
    fn on_eval(&mut self, k: u32, bar: &Bar, intrabar: bool, plots: &[f64]) {
        self.evals.push(Eval {
            k,
            index: bar.index,
            flags: bar.flags,
            intrabar,
            plots: plots.to_vec(),
        });
    }
}

/// A per-test directory under `std::env::temp_dir()`, removed on drop.
pub struct TmpDir(pub std::path::PathBuf);

impl TmpDir {
    /// A fresh, empty directory named after `tag`, the process and a counter.
    pub fn new(tag: &str) -> TmpDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let k = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("pine-screen-{tag}-{}-{k}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        TmpDir(p)
    }

    /// `self / rel`.
    pub fn join(&self, rel: &str) -> std::path::PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A probe script of `tests/probes/` (ours), read from disk.
pub fn probe_src(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/probes")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The `tf_s`-second bars of `rows` (hourly), complete buckets only, from bucket `skip` on;
/// `index` from 0.
pub fn agg_rows(rows: &[Bar], per: usize, skip: usize) -> Vec<Bar> {
    let mut out = Vec::new();
    let mut i = skip * per;
    while i + per <= rows.len() {
        let g = &rows[i..i + per];
        out.push(Bar {
            open_1e6: g[0].open_1e6,
            high_1e6: g.iter().map(|b| b.high_1e6).max().unwrap_or(0),
            low_1e6: g.iter().map(|b| b.low_1e6).min().unwrap_or(0),
            close_1e6: g[per - 1].close_1e6,
            volume_1e6: g.iter().map(|b| b.volume_1e6).sum(),
            open_ms: g[0].open_ms,
            close_ms: g[per - 1].close_ms,
            index: out.len() as u32,
            flags: barflag::CONFIRMED,
        });
        i += per;
    }
    out
}

/// The hourly chart of `closes` plus, when `htf4` is `Some(n)`, the 4-h file of its buckets
/// `skip..` limited to `n` bars.
pub fn bars_with_4h(closes_1e6: &[i64], htf4: Option<(usize, usize)>) -> TrialBars {
    let h = symbol_hash(CHART_KEY.as_bytes());
    let rows = hourly_rows(closes_1e6);
    let mut htf = Vec::new();
    if let Some((skip, n)) = htf4 {
        let mut r = agg_rows(&rows, 4, skip);
        r.truncate(n);
        let f = BarFile::from_rows(r, 14_400, h, 0, bar_source::CANDLES).expect("4-h rows");
        htf.push((h, 14_400, f));
    }
    let chart = BarFile::from_rows(rows, 3600, h, 0, bar_source::CANDLES).expect("rows");
    TrialBars::from_files(chart, htf)
}
