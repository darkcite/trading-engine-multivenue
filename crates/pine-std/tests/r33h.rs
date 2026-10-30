// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33h — CAR-R33-1: the `strategy.*` reads of the broker's f64 net profit
//! (`BrokerView::netprofit_f64_bits`), measured against TradingView's captured E fixtures (pins
//! `pin:04-netprofit-f64`, `pin:04-equity-f64`, `pin:05-net-f64-fees-provisional`). Each test
//! forms the view a captured row implies — the net profit as the broker forms it (the f64 running
//! sum, in close order, of `(px(exit) − px(entry)) · size`, less `entry_fee + exit_fee` and the
//! open trade's paid entry fee), the open trade, the bar's close — reads the accessor through
//! `pine_std::testing` and compares every captured row bit for bit. The `.fix` files are read from
//! `tests/conformance/fixtures/E/`.
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
const IC: f64 = 1_000_000.0;

// ---- the capture ----------------------------------------------------------------------------

/// One `.fix`: plot names, series rows (`time`, cells as text) and List of Trades rows.
struct Fix {
    plots: Vec<String>,
    rows: Vec<(i64, Vec<String>)>,
    trades: Vec<Trade>,
}

/// One List of Trades row (prices and quantity ×1e6 from the text).
#[derive(Clone, Copy)]
struct Trade {
    long: bool,
    entry_ms: i64,
    entry_px: i64,
    exit_ms: Option<i64>,
    exit_px: i64,
    qty: i64,
}

impl Trade {
    /// Signed contracts ×1e6.
    fn q(&self) -> i64 {
        if self.long {
            self.qty
        } else {
            -self.qty
        }
    }
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

    /// The trade open at time `t` (entered at or before, not yet exited).
    fn open_at(&self, t: i64) -> Option<Trade> {
        self.trades
            .iter()
            .find(|x| x.entry_ms <= t && x.exit_ms.is_none_or(|e| e > t))
            .copied()
    }
}

fn cell(text: &str) -> f64 {
    text.parse().unwrap_or_else(|_| panic!("cell {text}"))
}

// ---- the broker's formation, restated (test side) -------------------------------------------

/// `pin:04-px-float-ticks`: ticks × the f64 mintick (0.1).
fn pxf(px_1e6: i64) -> f64 {
    assert_eq!(px_1e6 % 100_000, 0, "on the tick");
    (px_1e6 / 100_000) as f64 * 0.1
}

/// `pin:04-qty-float-1e8`.
fn qf(q_1e6: i64) -> f64 {
    (q_1e6 as i128 * 100) as f64 * 1e-8
}

/// The broker's percent commission ×1e12 of one fill (`pct_1e12(value_1e12(px, |q|, 1), cv)`,
/// truncated).
fn fee_1e12(px_1e6: i64, q_1e6: i64, cv_1e6: i64) -> i128 {
    px_1e6 as i128 * q_1e6.abs() as i128 * cv_1e6 as i128 / 100_000_000
}

/// The view's net profit at time `t` as the broker forms it (CAR-R33-1): the running f64 sum,
/// in close order, of each closed trade's `(px(exit) − px(entry)) · q − (fee_e + fee_x)`, less
/// the open trade's paid entry fee; and that fee ×1e12.
fn net_at(fx: &Fix, t: i64, cv_1e6: i64) -> (f64, i128) {
    let mut net = 0.0f64;
    let mut open_fee = 0i128;
    for x in &fx.trades {
        if x.entry_ms > t {
            continue;
        }
        let fe = fee_1e12(x.entry_px, x.qty, cv_1e6);
        match x.exit_ms {
            Some(e) if e <= t => {
                let fx_ = fee_1e12(x.exit_px, x.qty, cv_1e6);
                let g = (pxf(x.exit_px) - pxf(x.entry_px)) * qf(x.q());
                net += g - (scale::from_1e12(fe) + scale::from_1e12(fx_));
            }
            _ => open_fee += fe,
        }
    }
    (net - scale::from_1e12(open_fee), open_fee)
}

// ---- the harness ----------------------------------------------------------------------------

fn call(id: OverloadId) -> Call<'static> {
    Call {
        ov: id,
        inputs: &[],
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

fn rec(entry_px: i64, qty: i64) -> TradeRec {
    TradeRec {
        entry_ms: 1,
        entry_px_1e6: entry_px,
        qty_1e6: qty,
        exit_bar: u32::MAX,
        flags: trade_flag::PNL_NET | trade_flag::OPEN,
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

/// The view and open trade of one row: `np` the f64 net profit, the open trade (if any) at
/// `close`, its integer gross open profit as the broker's check (`open_profit_f64`).
fn set_row(ctx: &mut TestCtx, np: f64, open: Option<Trade>, close: i64) {
    ctx.open.clear();
    let mut gross = 0i128;
    if let Some(x) = open {
        ctx.open.push(rec(x.entry_px, x.q()));
        gross = (close - x.entry_px) as i128 * x.q() as i128;
    }
    ctx.broker = Some(BrokerView {
        initial_capital_1e12: 1_000_000 * scale::MONEY,
        netprofit_f64_bits: np.to_bits(),
        opentrades: ctx.open.len() as u32,
        openprofit_1e12: gross,
        ..BrokerView::default()
    });
}

/// The tick close whose open profit, added to `base`, gives `tv` (the bar's close a captured
/// equity row implies).
fn close_for(x: Trade, base: f64, tv: f64) -> i64 {
    let op = tv - base;
    ((x.entry_px as f64 + op * 1e6 / (x.q() as f64 / 1e6)) / 1e5).round() as i64 * 100_000
}

// ---- netprofit --------------------------------------------------------------------------------

/// `pin:04-netprofit-f64` — E086 `strategy.netprofit`: every captured row (4320) reads the view's
/// f64 bits, and the broker's formation of them is TradingView's bit for bit
/// (−1595.800000000003 after trade 1); `netprofit_1e12 / 1e12` fits 72.
#[test]
fn netprofit_reads_the_f64_bits_e086_every_row() {
    let fx = fix("E086");
    let j = fx.col("f_E086_net");
    let calls = [call(ov::STRATEGY_NETPROFIT_VAR)];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: Vec::with_capacity(1),
        ..TestCtx::default()
    };
    let (mut rows, mut counted) = (0, 0);
    for (k, (t, c)) in fx.rows.iter().enumerate() {
        let (np, _) = net_at(&fx, *t, 0);
        set_row(&mut ctx, np, None, 100 * E6);
        counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
        let tv = cell(&c[j]);
        assert_eq!(
            h.out(0, 0).to_bits(),
            tv.to_bits(),
            "{t}: {} vs {tv}",
            h.out(0, 0)
        );
        rows += 1;
    }
    assert_eq!(rows, 4320);
    assert_eq!(counted, 0, "allocations on the bar path");
}

/// E118 `strategy.netprofit_percent`: every captured row (4320) is `netprofit / initial_capital ×
/// 100` over the f64 net profit, in that order, bit for bit (`np × 100 / ic` fits 2890,
/// `np / (ic / 100)` 3311, the integer net 72).
#[test]
fn netprofit_percent_e118_every_row() {
    let fx = fix("E118");
    let j = fx.col("f_E118_npp");
    let calls = [call(ov::STRATEGY_NETPROFIT_PERCENT_VAR)];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: Vec::with_capacity(1),
        ..TestCtx::default()
    };
    let (mut rows, mut other, mut counted) = (0, 0, 0);
    for (k, (t, c)) in fx.rows.iter().enumerate() {
        let (np, _) = net_at(&fx, *t, 0);
        set_row(&mut ctx, np, None, 100 * E6);
        counted += step(&mut h, &mut ctx, k as u32, 100 * E6);
        let tv = cell(&c[j]);
        assert_eq!(
            h.out(0, 0).to_bits(),
            tv.to_bits(),
            "{t}: {} vs {tv}",
            h.out(0, 0)
        );
        other += usize::from((np * 100.0 / IC).to_bits() == tv.to_bits());
        rows += 1;
    }
    assert_eq!((rows, other), (4320, 2890));
    assert_eq!(counted, 0);
}

// ---- equity -----------------------------------------------------------------------------------

/// `pin:04-equity-f64` — E085 `strategy.equity` (commission-free): every captured row (4320) reads
/// `(initial_capital + netprofit) + openprofit` bit for bit, the open profit `open_profit_f64`'s
/// at the close on the tick that the row's value implies; `ic + (np + op)` fits 3697.
#[test]
fn equity_e085_every_row() {
    let fx = fix("E085");
    let j = fx.col("f_E085_eq");
    let calls = [call(ov::STRATEGY_EQUITY_VAR)];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: Vec::with_capacity(1),
        ..TestCtx::default()
    };
    let (mut rows, mut open_rows, mut other, mut counted) = (0, 0, 0, 0);
    for (k, (t, c)) in fx.rows.iter().enumerate() {
        let tv = cell(&c[j]);
        let (np, _) = net_at(&fx, *t, 0);
        let open = fx.open_at(*t);
        let close = open.map_or(100 * E6, |x| close_for(x, IC + np, tv));
        set_row(&mut ctx, np, open, close);
        counted += step(&mut h, &mut ctx, k as u32, close);
        assert_eq!(
            h.out(0, 0).to_bits(),
            tv.to_bits(),
            "{t}: {} vs {tv}",
            h.out(0, 0)
        );
        let op = open.map_or(0.0, |x| (pxf(close) - pxf(x.entry_px)) * qf(x.q()));
        other += usize::from((IC + (np + op)).to_bits() == tv.to_bits());
        open_rows += usize::from(open.is_some());
        rows += 1;
    }
    assert_eq!((rows, other), (4320, 3697));
    assert!(open_rows > 0);
    assert_eq!(counted, 0);
}

/// `pin:04-equity-f64` with `pin:05-net-f64-fees-provisional` — E114 `strategy.equity` at a 0.1 %
/// commission: every captured row (4320) reads `(initial_capital + netprofit) + openprofit` bit
/// for bit, the net profit's closed trades less `entry_fee + exit_fee` (each `fee_1e12 / 1e12`)
/// and the open trade's paid entry fee, the open profit gross at the close the row's captured
/// `openprofit` implies. The fee subtracted as one conversion (`(fe + fx)_1e12 / 1e12`) fits
/// 4196.
#[test]
fn equity_with_fees_e114_every_row() {
    let fx = fix("E114");
    let (je, jo) = (fx.col("f_E114_eq"), fx.col("f_E114_oprofit"));
    let calls = [call(ov::STRATEGY_EQUITY_VAR)];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: Vec::with_capacity(1),
        ..TestCtx::default()
    };
    let (mut rows, mut counted) = (0, 0);
    for (k, (t, c)) in fx.rows.iter().enumerate() {
        let (np, _) = net_at(&fx, *t, 100_000);
        let open = fx.open_at(*t);
        let close = open.map_or(100 * E6, |x| close_for(x, 0.0, cell(&c[jo])));
        set_row(&mut ctx, np, open, close);
        counted += step(&mut h, &mut ctx, k as u32, close);
        let tv = cell(&c[je]);
        assert_eq!(
            h.out(0, 0).to_bits(),
            tv.to_bits(),
            "{t}: {} vs {tv}",
            h.out(0, 0)
        );
        rows += 1;
    }
    assert_eq!(rows, 4320);
    assert_eq!(counted, 0);
    // The one-conversion fee reading misses rows.
    let mut one = 0;
    for (t, c) in &fx.rows {
        let mut net = 0.0f64;
        let mut ofee = 0i128;
        for x in &fx.trades {
            if x.entry_ms > *t {
                continue;
            }
            let fe = fee_1e12(x.entry_px, x.qty, 100_000);
            match x.exit_ms {
                Some(e) if e <= *t => {
                    let fee = fe + fee_1e12(x.exit_px, x.qty, 100_000);
                    net += (pxf(x.exit_px) - pxf(x.entry_px)) * qf(x.q()) - scale::from_1e12(fee);
                }
                _ => ofee += fe,
            }
        }
        let eq = (IC + (net - scale::from_1e12(ofee))) + cell(&c[jo]);
        one += usize::from(eq.to_bits() == cell(&c[je]).to_bits());
    }
    assert_eq!(one, 4196);
}

/// A mark that is not the bar's close (the integer open profit disagrees — a fill-price
/// execution): equity reads `equity_1e12 / 1e12`, as before r33h; with no broker, na.
#[test]
fn equity_falls_back_to_the_integer_off_the_close() {
    let calls = [
        call(ov::STRATEGY_EQUITY_VAR),
        call(ov::STRATEGY_NETPROFIT_VAR),
    ];
    let mut h = harness(&calls);
    let mut ctx = TestCtx {
        open: vec![rec(77_249_600_000, E6)],
        ..TestCtx::default()
    };
    let view = BrokerView {
        initial_capital_1e12: 1_000_000 * scale::MONEY,
        netprofit_f64_bits: (-1595.800000000003f64).to_bits(),
        opentrades: 1,
        openprofit_1e12: -300 * scale::MONEY,
        equity_1e12: 998_104_200_000_000_000,
        ..BrokerView::default()
    };
    ctx.broker = Some(view);
    step(&mut h, &mut ctx, 0, 76_964_300_000);
    assert_eq!(h.out(0, 0).to_bits(), 998_104.2f64.to_bits());
    assert_eq!(h.out(1, 0).to_bits(), (-1595.800000000003f64).to_bits());
    // At the close's integer: the f64 sum, `(ic + np) + op`.
    ctx.broker = Some(BrokerView {
        openprofit_1e12: -285_300_000 * E6 as i128,
        ..view
    });
    step(&mut h, &mut ctx, 1, 76_964_300_000);
    let op = (pxf(76_964_300_000) - pxf(77_249_600_000)) * qf(E6);
    assert_eq!(
        h.out(0, 0).to_bits(),
        ((IC + -1595.800000000003) + op).to_bits()
    );
    ctx.broker = None;
    step(&mut h, &mut ctx, 2, 76_964_300_000);
    assert!(h.out(0, 0).is_nan() && h.out(1, 0).is_nan());
}
