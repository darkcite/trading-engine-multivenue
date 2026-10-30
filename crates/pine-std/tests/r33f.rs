// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33f — the float formation of script-visible `strategy.*` values, measured against
//! TradingView's captured E fixtures (pins `pin:04-qty-float-1e8` (CI-058),
//! `pin:04-px-float-ticks`, `pin:04-trade-profit-f64`, `pin:04-openprofit-f64`,
//! `pin:04-avg-trade-sig10`, `pin:04-max-pct-zero-before-trade`). Each test feeds the broker view
//! / trade records a captured row implies through `pine_std::testing`, reads the accessor and
//! compares every captured row of the named fixture bit for bit; the `.fix` files are read from
//! `tests/conformance/fixtures/E/`. The integer a row implies is taken from TradingView's own
//! text (a quantity ×1e6, a price on the 0.1 tick) — what the law under test maps to the f64.
//!
//! COPY-DOCTRINE: test support, may allocate (outside the counted window).

use pine_ir::*;
use pine_spec::ov;
use pine_std::testing::{Call, Harness, Script, TestCtx, CHART};
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

/// BTCUSDT.P on the captures: mintick 0.1, point value 1.
const BTC: ChartInfo = ChartInfo {
    mintick_1e6: 100_000,
    pointvalue_1e6: 1_000_000,
    ..CHART
};
const E6: i64 = 1_000_000;

// ---- the capture ----------------------------------------------------------------------------

/// One `.fix`: plot names, series rows (`time`, cells as text) and List of Trades rows.
struct Fix {
    plots: Vec<String>,
    rows: Vec<(i64, Vec<String>)>,
    trades: Vec<Trade>,
}

/// One List of Trades row (prices and quantity as integers ×1e6 from the text; an open trade's
/// exit cells read `-`).
#[derive(Clone, Copy)]
struct Trade {
    long: bool,
    entry_ms: i64,
    entry_px: i64,
    exit_ms: Option<i64>,
    exit_px: i64,
    qty: i64,
}

fn e6(text: &str) -> i64 {
    let v: f64 = text.parse().unwrap_or_else(|_| panic!("number {text}"));
    (v * 1e6).round() as i64
}

fn fix(id: &str) -> Fix {
    let p = format!(
        "{}/../../tests/conformance/fixtures/E/{id}.fix",
        env!("CARGO_MANIFEST_DIR")
    );
    let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
    let (mut plots, mut rows, mut trades) = (Vec::new(), Vec::new(), Vec::new());
    let mut mode = 0;
    for line in t.lines() {
        let c: Vec<&str> = line.split('\t').collect();
        match (mode, c[0]) {
            (0, "plot") => plots.push(c[1].to_string()),
            (0, "series") => mode = 1,
            (1, "end") => mode = 2,
            (1, k) => rows.push((
                k.parse().unwrap(),
                c[1..].iter().map(|s| s.to_string()).collect(),
            )),
            (2, "trades") => mode = 3,
            (3, "end") => mode = 4,
            (3, _) => trades.push(Trade {
                long: c[1] == "long",
                entry_ms: c[2].parse().unwrap(),
                entry_px: e6(c[3]),
                exit_ms: c[5].parse().ok(),
                exit_px: if c[6] == "-" || c[6].is_empty() {
                    0
                } else {
                    e6(c[6])
                },
                qty: e6(c[8]),
            }),
            _ => {}
        }
    }
    Fix {
        plots,
        rows,
        trades,
    }
}

impl Fix {
    fn col(&self, name: &str) -> usize {
        self.plots
            .iter()
            .position(|p| p == name)
            .unwrap_or_else(|| panic!("no plot {name}"))
    }

    /// The trades closed by time `t` (exit at or before `t`).
    fn closed_by(&self, t: i64) -> Vec<Trade> {
        self.trades
            .iter()
            .filter(|x| x.exit_ms.is_some_and(|e| e <= t))
            .copied()
            .collect()
    }

    /// The trade open at time `t` (entered at or before, not yet exited).
    fn open_at(&self, t: i64) -> Option<Trade> {
        self.trades
            .iter()
            .find(|x| x.entry_ms <= t && x.exit_ms.is_none_or(|e| e > t))
            .copied()
    }
}

/// TradingView's cell as the f64 its text parses to (`na` → NaN).
fn cell(text: &str) -> f64 {
    if text == "na" {
        f64::NAN
    } else {
        text.parse().unwrap_or_else(|_| panic!("cell {text}"))
    }
}

fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

// ---- the harness ----------------------------------------------------------------------------

fn call<'a>(id: OverloadId, inputs: &'a [(Lane, pine_std::testing::Src)]) -> Call<'a> {
    Call {
        ov: id,
        inputs,
        outputs: &[Lane::F64],
        site_bytes: 0,
        guard: None,
    }
}

fn harness(calls: &[Call<'_>]) -> Harness {
    Harness::build(&Script {
        calls,
        mode: ExecMode::BarClose,
        dialect: Dialect::V6,
        intent_cap: 0,
        chart: BTC,
    })
}

fn view() -> BrokerView {
    BrokerView {
        initial_capital_1e12: 1_000_000 * scale::MONEY,
        ..BrokerView::default()
    }
}

fn rec(entry_px: i64, exit_px: i64, qty: i64, open: bool) -> TradeRec {
    TradeRec {
        entry_ms: 1,
        exit_ms: if open { 0 } else { 2 },
        entry_px_1e6: entry_px,
        exit_px_1e6: if open { 0 } else { exit_px },
        qty_1e6: qty,
        exit_bar: if open { u32::MAX } else { 1 },
        pnl_1e12: 0,
        fee_1e12: 0,
        flags: trade_flag::PNL_NET | if open { trade_flag::OPEN } else { 0 },
        ..TradeRec::default()
    }
}

/// Step one evaluation on a bar closing at `close_1e6`; the allocations it made.
fn step(h: &mut Harness, ctx: &mut TestCtx, k: u32, close_1e6: i64) -> u64 {
    let mut b = pine_vm::testkit::bar(k, 100.0, barflag::CONFIRMED);
    b.close_1e6 = close_1e6;
    let a0 = allocs();
    let r = h.step_bar(&b, ctx);
    let n = allocs() - a0;
    assert_eq!(r, StepOutcome::Ok);
    n
}

const K0: [(Lane, pine_std::testing::Src); 1] = [(Lane::I64, pine_std::testing::Src::Const(0))];

// ---- CI-058: quantities -----------------------------------------------------------------------

/// `pin:04-qty-float-1e8` — every distinct captured `position_size` / `opentrades.size` text of
/// the E fixtures reads back bit for bit from its ×1e6 integer through `position_size`,
/// `opentrades.size(0)` and `closedtrades.size(0)` (567/567; `q / 1e6` fits 495).
#[test]
fn qty_float_law_every_captured_size_text() {
    let dir = format!(
        "{}/../../tests/conformance/fixtures/E",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut texts: Vec<String> = Vec::new();
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().into_string().unwrap();
            n.strip_suffix(".fix").map(str::to_string)
        })
        .collect();
    ids.sort();
    for id in &ids {
        let f = fix(id);
        for (j, p) in f.plots.iter().enumerate() {
            if !(p.ends_with("_pos") || p.ends_with("_sz")) {
                continue;
            }
            for (_, c) in &f.rows {
                if j < c.len() && c[j] != "na" && !texts.contains(&c[j]) {
                    texts.push(c[j].clone());
                }
            }
        }
    }
    assert_eq!(texts.len(), 567, "distinct captured quantity texts");
    let calls = [
        call(ov::STRATEGY_POSITION_SIZE_VAR, &[]),
        call(ov::STRATEGY_OPENTRADES_SIZE, &K0),
        call(ov::STRATEGY_CLOSEDTRADES_SIZE, &K0),
    ];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        broker: Some(view()),
        open: vec![rec(E6, 0, 0, true)],
        closed: vec![rec(E6, E6, 0, false)],
        ..TestCtx::default()
    };
    let (mut fit, mut old, mut counted) = (0, 0, 0);
    for (k, t) in texts.iter().enumerate() {
        let tv = cell(t);
        let q = e6(t);
        ctx.broker = Some(BrokerView {
            position_size_1e6: q,
            opentrades: 1,
            closedtrades: 1,
            closed_retained: 1,
            ..view()
        });
        ctx.open[0].qty_1e6 = q;
        ctx.closed[0].qty_1e6 = q;
        counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
        let got = [h.out(0, 0), h.out(1, 0), h.out(2, 0)];
        assert!(got.iter().all(|g| same(*g, tv)), "{t}: {got:?}");
        fit += 1;
        old += usize::from(scale::from_1e6(q).to_bits() == tv.to_bits());
    }
    assert_eq!((fit, old), (567, 495));
    assert_eq!(counted, 0, "allocations on the bar path");
    // The examples of CI-058.
    for (q, t) in [
        (1_543_003, "1.5430030000000001"),
        (13, "0.000013000000000000001"),
        (13_258_776, "13.258776000000001"),
    ] {
        assert!(texts.iter().any(|x| x == t), "{t} is captured");
        assert_eq!(e6(t), q);
    }
}

// ---- prices -----------------------------------------------------------------------------------

/// `pin:04-px-float-ticks` — E082 `position_avg_price`, E107 `opentrades.entry_price(0)` and E108
/// `closedtrades.entry_price(closedtrades − 1)`: every captured row reads back bit for bit as
/// ticks × mintick (45 distinct prices each; `px / 1e6` fits 24).
#[test]
fn px_float_law_e082_e107_e108_every_row() {
    for (id, plot, ov_id, n_rows) in [
        (
            "E082",
            "f_E082_avg",
            ov::STRATEGY_POSITION_AVG_PRICE_VAR,
            2393,
        ),
        (
            "E107",
            "f_E107_ep",
            ov::STRATEGY_OPENTRADES_ENTRY_PRICE,
            2394,
        ),
        (
            "E108",
            "f_E108_ep",
            ov::STRATEGY_CLOSEDTRADES_ENTRY_PRICE,
            4248,
        ),
    ] {
        let f = fix(id);
        let j = f.col(plot);
        let ins: &[(Lane, pine_std::testing::Src)] = if ov_id == ov::STRATEGY_POSITION_AVG_PRICE_VAR
        {
            &[]
        } else {
            &K0
        };
        let calls = [call(ov_id, ins)];
        let mut h = harness(&calls);
        let mut ctx = TestCtx {
            open: vec![rec(E6, 0, E6, true)],
            closed: vec![rec(E6, E6, E6, false)],
            ..TestCtx::default()
        };
        let (mut rows, mut old, mut counted) = (0, 0, 0);
        for (k, (t, c)) in f.rows.iter().enumerate() {
            if c[j] == "na" {
                continue;
            }
            let tv = cell(&c[j]);
            let px = e6(&c[j]);
            assert_eq!(px % 100_000, 0, "{id} {t}: on the tick");
            ctx.broker = Some(BrokerView {
                position_avg_price_1e6: px,
                opentrades: 1,
                closedtrades: 1,
                closed_retained: 1,
                ..view()
            });
            ctx.open[0].entry_px_1e6 = px;
            ctx.closed[0].entry_px_1e6 = px;
            counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
            assert!(same(h.out(0, 0), tv), "{id} {t}: {} vs {tv}", h.out(0, 0));
            rows += 1;
            old += usize::from(scale::from_1e6(px).to_bits() == tv.to_bits());
        }
        assert_eq!(rows, n_rows, "{id}");
        assert!(old < rows, "{id}: px / 1e6 does not fit every row");
        assert_eq!(counted, 0);
    }
    // E082's first open position: 75577.4 → 75577.40000000001.
    assert_eq!((755_774f64 * 0.1).to_bits(), 75577.40000000001f64.to_bits());
}

// ---- trade profit -----------------------------------------------------------------------------

/// `pin:04-trade-profit-f64` — E108 `closedtrades.profit(closedtrades − 1)`: every captured row
/// (4248, 45 trades) reads `(exit − entry) · size` over the tick-formed prices, bit for bit
/// (−1595.800000000003); a trade that paid a fee keeps `pnl_1e12 / 1e12`.
#[test]
fn trade_profit_e108_every_row() {
    let f = fix("E108");
    let j = f.col("f_E108_profit");
    let jc = f.col("f_E108_comm");
    let calls = [call(ov::STRATEGY_CLOSEDTRADES_PROFIT, &K0)];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        closed: vec![rec(E6, E6, E6, false)],
        ..TestCtx::default()
    };
    let (mut rows, mut counted) = (0, 0);
    let mut seen = Vec::new();
    for (k, (t, c)) in f.rows.iter().enumerate() {
        if c[j] == "na" {
            continue;
        }
        assert_eq!(c[jc], "0", "commission-free");
        let closed = f.closed_by(*t);
        let x = *closed.last().unwrap();
        let q = if x.long { x.qty } else { -x.qty };
        ctx.broker = Some(BrokerView {
            closedtrades: 1,
            closed_retained: 1,
            ..view()
        });
        ctx.closed[0] = rec(x.entry_px, x.exit_px, q, false);
        // The record's integer P&L, as the broker books it (not what the read returns).
        ctx.closed[0].pnl_1e12 = (x.exit_px - x.entry_px) as i128 * q as i128;
        counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
        let tv = cell(&c[j]);
        assert!(same(h.out(0, 0), tv), "{t}: {} vs {tv}", h.out(0, 0));
        rows += 1;
        if !seen.contains(&x.entry_ms) {
            seen.push(x.entry_ms);
        }
    }
    assert_eq!((rows, seen.len()), (4248, 45));
    assert_eq!(counted, 0);
    // A fee-paying trade keeps the broker's integer.
    ctx.closed[0] = TradeRec {
        fee_1e12: scale::MONEY,
        pnl_1e12: -1_596_800_000 * E6 as i128,
        ..rec(77_249_600_000, 75_653_800_000, E6, false)
    };
    step(&mut h, &mut ctx, 0, 100 * E6);
    assert_eq!(h.out(0, 0).to_bits(), (-1596.8f64).to_bits());
}

// ---- open profit ------------------------------------------------------------------------------

/// `pin:04-openprofit-f64` — E114 `strategy.openprofit` (0.1 % commission): every captured row
/// (4320) reads the f64 `(close − entry) · size`, gross of the entry fee, bit for bit
/// (−285.3000000000029); the bar's close is the tick the row's value implies. A broker whose
/// integer open profit is not the close's (a fill-price mark) keeps `openprofit_1e12 / 1e12`.
#[test]
fn openprofit_e114_every_row() {
    let f = fix("E114");
    let j = f.col("f_E114_oprofit");
    let calls = [call(ov::STRATEGY_OPENPROFIT_VAR, &[])];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: Vec::with_capacity(1),
        ..TestCtx::default()
    };
    let (mut rows, mut open_rows, mut old, mut counted) = (0, 0, 0, 0);
    for (k, (t, c)) in f.rows.iter().enumerate() {
        let tv = cell(&c[j]);
        ctx.open.clear();
        let (close, gross) = match f.open_at(*t) {
            None => (100 * E6, 0i128),
            Some(x) => {
                let q = if x.long { x.qty } else { -x.qty };
                // The close on the 0.1 tick that the captured value implies.
                let close = ((x.entry_px as f64 + tv * 1e6 / (q as f64 / 1e6)) / 1e5).round()
                    as i64
                    * 100_000;
                ctx.open.push(rec(x.entry_px, 0, q, true));
                open_rows += 1;
                (close, (close - x.entry_px) as i128 * q as i128)
            }
        };
        ctx.broker = Some(BrokerView {
            opentrades: ctx.open.len() as u32,
            openprofit_1e12: gross,
            ..view()
        });
        counted += step(&mut h, &mut ctx, k as u32, close);
        assert!(same(h.out(0, 0), tv), "{t}: {} vs {tv}", h.out(0, 0));
        rows += 1;
        old += usize::from(scale::from_1e12(gross).to_bits() == tv.to_bits());
    }
    assert_eq!(rows, 4320);
    assert!(open_rows > 0 && old < rows);
    assert_eq!(counted, 0);
    // A mark off the close (the broker's integer disagrees): the integer reads.
    ctx.open.clear();
    ctx.open.push(rec(77_249_600_000, 0, E6, true));
    ctx.broker = Some(BrokerView {
        opentrades: 1,
        openprofit_1e12: -300 * scale::MONEY,
        ..view()
    });
    step(&mut h, &mut ctx, 0, 76_964_300_000);
    assert_eq!(h.out(0, 0).to_bits(), (-300f64).to_bits());
    // The close's: E114 row 1776481200000.
    ctx.broker = Some(BrokerView {
        opentrades: 1,
        openprofit_1e12: -285_300_000 * E6 as i128,
        ..view()
    });
    step(&mut h, &mut ctx, 1, 76_964_300_000);
    assert_eq!(h.out(0, 0).to_bits(), (-285.3000000000029f64).to_bits());
}

// ---- E118 -------------------------------------------------------------------------------------

/// `pin:04-avg-trade-sig10` and `pin:04-max-pct-zero-before-trade` — E118: `avg_trade`,
/// `avg_winning_trade`, `avg_losing_trade` read the broker's exact sums over the closed trades,
/// divided by the count, rounded to 10 significant digits — every captured row (4320 × 3), bit
/// for bit; `max_drawdown_percent` reads 0 on every row before the first trade (51).
#[test]
fn e118_averages_sig10_and_mddp_zero() {
    let f = fix("E118");
    let (ja, jw, jl, jm) = (
        f.col("f_E118_avg"),
        f.col("f_E118_avgw"),
        f.col("f_E118_avgl"),
        f.col("f_E118_mddp"),
    );
    let calls = [
        call(ov::STRATEGY_AVG_TRADE_VAR, &[]),
        call(ov::STRATEGY_AVG_WINNING_TRADE_VAR, &[]),
        call(ov::STRATEGY_AVG_LOSING_TRADE_VAR, &[]),
        call(ov::STRATEGY_MAX_DRAWDOWN_PERCENT_VAR, &[]),
    ];
    let mut h = harness(&calls);
    let mut ctx = TestCtx::default();
    let (mut rows, mut pre, mut counted) = (0, 0, 0);
    let first_entry = f.trades.iter().map(|x| x.entry_ms).min().unwrap();
    for (k, (t, c)) in f.rows.iter().enumerate() {
        let closed = f.closed_by(*t);
        let (mut net, mut gp, mut gl, mut w, mut l) = (0i128, 0i128, 0i128, 0u32, 0u32);
        for x in &closed {
            let q = if x.long { x.qty } else { -x.qty };
            let p = (x.exit_px - x.entry_px) as i128 * q as i128;
            net += p;
            if p > 0 {
                gp += p;
                w += 1;
            } else if p < 0 {
                gl -= p;
                l += 1;
            }
        }
        ctx.broker = Some(BrokerView {
            closedtrades: closed.len() as u32,
            wintrades: w,
            losstrades: l,
            netprofit_1e12: net,
            grossprofit_1e12: gp,
            grossloss_1e12: gl,
            ..view()
        });
        counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
        for (out, jj) in [(0, ja), (1, jw), (2, jl)] {
            let tv = cell(&c[jj]);
            assert!(
                same(h.out(out, 0), tv),
                "{t} out {out}: {} vs {tv}",
                h.out(out, 0)
            );
        }
        if *t < first_entry {
            assert_eq!(c[jm], "0");
            assert_eq!(h.out(3, 0).to_bits(), 0f64.to_bits());
            pre += 1;
        }
        rows += 1;
    }
    assert_eq!((rows, pre), (4320, 51));
    assert_eq!(counted, 0);
    // 420.06666666666666 → 420.0666667 (row 1777212000000).
    ctx.broker = Some(BrokerView {
        closedtrades: 3,
        netprofit_1e12: 1_260_200_000 * E6 as i128,
        ..view()
    });
    step(&mut h, &mut ctx, 0, 100 * E6);
    assert_eq!(h.out(0, 0).to_bits(), 420.0666667f64.to_bits());
}
