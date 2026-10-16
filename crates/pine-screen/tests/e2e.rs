// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The lane's headline proof (impl-07 §4.3): a hand-typed Pine strategy runs end to end —
//! pine-lang parse → pine-sema compile → pine_vm::verify → VmInstance with pine-std bodies →
//! pine-broker `TvBroker` — through `pine_screen::run_group`, and the fills are TradingView's.

mod common;

use common::*;
use pine_ir::{trial_flag, ExecMode, ScriptKind};
use pine_screen::{run_group, run_trial, RowRec, TrialOutcome, TrialStatus, Workspace};

const BODY: &str = "strategy(\"sma cross\", overlay=true)
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
";

/// The same rule with the broker's read-back plotted (position size, average price).
const OBSERVED: &str = "strategy(\"sma cross\", overlay=true)
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
plot(strategy.position_size)
plot(strategy.position_avg_price)
";

const N_BARS: usize = 720; // 30 days of 1-h bars

fn src(v: u8, body: &str) -> String {
    format!("//@version={v}\n{body}")
}

/// The test's own SMA over the same closes (f64, as the script sees them).
fn sma(c: &[f64], i: usize, n: usize) -> f64 {
    c[i + 1 - n..=i].iter().sum::<f64>() / n as f64
}

/// Expected trades from the closes alone: (entry signal bar, exit signal bar or None).
fn expected_trades(c: &[f64], end: usize) -> (Vec<usize>, Vec<usize>, Vec<(usize, Option<usize>)>) {
    let mut ups = Vec::new();
    let mut downs = Vec::new();
    for i in 30..end {
        let (f0, s0, f1, s1) = (
            sma(c, i, 10),
            sma(c, i, 30),
            sma(c, i - 1, 10),
            sma(c, i - 1, 30),
        );
        // Near-ties would make the comparison depend on summation order: the series avoids them.
        assert!(
            (f0 - s0).abs() > 1e-7,
            "near-tie at bar {i}: pick another synthetic series"
        );
        if f0 > s0 && f1 <= s1 {
            ups.push(i);
        }
        if f0 < s0 && f1 >= s1 {
            downs.push(i);
        }
    }
    let mut trades = Vec::new();
    for &x in &ups {
        if x + 1 >= end {
            continue; // the order never fills: no bar after the signal
        }
        let y = downs.iter().copied().find(|&y| y > x);
        let exit = y.filter(|&y| y + 1 < end);
        trades.push((x, exit));
    }
    (ups, downs, trades)
}

#[test]
fn e2e_sma_cross_runs_through_vm_and_tv_broker() {
    let closes_1e6 = sine_closes(N_BARS);
    let closes: Vec<f64> = closes_1e6.iter().map(|&c| c as f64 / 1e6).collect();
    let bars = bars_of(&closes_1e6);
    let rows = bars.chart().rows();
    let cfg = cfg(2, 30);
    for v in [6u8, 5] {
        // ---- compile + verify (compile_trial runs pine_vm::verify) ----
        let t = compile(&src(v, BODY)).unwrap_or_else(|e| panic!("v{v}: {e:?} {:?}", e.code()));
        assert_eq!(t.mode(), ExecMode::BarClose, "v{v}");
        assert_eq!(t.meta().kind, ScriptKind::Strategy);
        assert!(t.htf_needs().is_empty());
        assert!(!t.overloads_reached().is_empty());
        pine_vm::verify(t.program(), &pine_std::BUILTINS).expect("verifies");

        // ---- the run, recorded ----
        let mut ws = Workspace::new();
        let mut out = [TrialOutcome::default()];
        let mut rec = Rec::default();
        let stats = run_group(&[&t], &bars, &cfg, &mut ws, &mut out, &mut rec)
            .unwrap_or_else(|e| panic!("v{v}: {e:?}"));
        let o = out[0];
        assert_eq!(
            o.status,
            TrialStatus::Completed,
            "v{v}: every bar steps without fault"
        );
        assert_eq!(stats.chart_bars, N_BARS as u64);
        assert_eq!(
            stats.bar_evals, N_BARS as u64,
            "one bar-close evaluation per bar"
        );
        assert_eq!(stats.tick_evals, 0);
        assert_eq!(rec.brackets, (1, 1));
        assert_eq!(rec.evals.len(), N_BARS);
        // plot(f) is the 10-bar SMA on every bar the VM evaluated through pine-screen.
        for e in &rec.evals {
            let i = e.index as usize;
            let got = e.plots[0];
            if i < 9 {
                assert!(got.is_nan(), "v{v} bar {i}: {got}");
            } else {
                let want = sma(&closes, i, 10);
                assert!((got - want).abs() < 1e-9, "v{v} bar {i}: {got} vs {want}");
            }
        }

        // ---- the trades ----
        let (_, _, trades) = expected_trades(&closes, N_BARS);
        let closed_all = trades.iter().filter(|t| t.1.is_some()).count();
        let open_at_end = trades.len() - closed_all;
        assert!(
            closed_all >= 2,
            "v{v}: the series must cross both ways at least twice"
        );
        let r = &o.record;
        assert_eq!(
            (r.wins + r.losses) as usize,
            closed_all,
            "v{v}: closed trades (wins + losses) vs crossover→crossunder pairs"
        );
        assert_eq!(
            r.fills as usize,
            2 * closed_all + open_at_end,
            "v{v}: fills"
        );
        let closed_in_window = trades
            .iter()
            .filter(|t| t.1.is_some_and(|y| rows[y + 1].open_ms >= cfg.from_ms))
            .count();
        assert_eq!(
            o.summary.closed as usize, closed_in_window,
            "v{v}: summary.closed"
        );
        assert_eq!(
            o.summary.trades as usize,
            closed_in_window + open_at_end,
            "v{v}: summary.trades"
        );
        assert_eq!(o.summary.bars, N_BARS as u32);
        assert_eq!(o.summary.obs_days, 28);
        assert_eq!(o.summary.faults, 0);
        assert_eq!(o.summary.flags, trial_flag::FIXED_NOTIONAL);
        assert_eq!(
            o.summary.gross_bps_1e4,
            o.summary.net_bps_1e4 + o.summary.fee_bps_1e4
        );
        assert!(o.summary.fee_bps_1e4 > 0, "taker 5 bps on every fill");
        assert_eq!(ws.day_returns(0).len(), 28);
        let sum: i64 = ws
            .day_returns(0)
            .iter()
            .map(|d| (d * 1e4).round() as i64)
            .sum();
        assert!((sum - o.summary.net_bps_1e4).abs() <= 28, "§4.5.2 bound");
        let c = &r.counters;
        assert_eq!(
            (
                c.skipped_cap,
                c.skipped_max_open,
                c.ioc_remainder_cancelled,
                c.ioc_sent
            ),
            (0, 0, 0, 0),
            "I20: TvBroker never counts the native gate"
        );

        // ---- the fills, observed through the broker's read-back ----
        let obs = compile(&src(v, OBSERVED)).expect("observed variant compiles");
        let mut ws2 = Workspace::new();
        let mut out2 = [TrialOutcome::default()];
        let mut rec2 = Rec::default();
        run_group(&[&obs], &bars, &cfg, &mut ws2, &mut out2, &mut rec2).expect("runs");
        assert_eq!(out2[0].status, TrialStatus::Completed);
        assert_eq!(
            out2[0].record.fills, r.fills,
            "plots do not change the trades"
        );
        assert_eq!(out2[0].summary.net_bps_1e4, o.summary.net_bps_1e4);
        let pos = |i: usize| rec2.evals[i].plots[1];
        let avg = |i: usize| rec2.evals[i].plots[2];
        for &(x, exit) in &trades {
            // Not filled on the signal bar's close ...
            assert_eq!(
                pos(x),
                0.0,
                "v{v}: flat at the crossover bar {x}'s evaluation"
            );
            // ... but at the NEXT bar's open (TradingView's default market-order fill).
            let open_next = rows[x + 1].open_1e6 as f64 / 1e6;
            assert!(pos(x + 1) > 0.0, "v{v}: long at bar {}", x + 1);
            assert!(
                (avg(x + 1) - open_next).abs() < 1e-9,
                "v{v}: entry of the bar-{x} crossover fills at bar {}'s open {open_next}, got {}",
                x + 1,
                avg(x + 1)
            );
            // The unit is $1 000 of notional at the fill price (qty step 0.001).
            let notional = pos(x + 1) * open_next;
            assert!(
                (notional - 1000.0).abs() <= 0.001 * open_next + 1e-9,
                "unit {notional}"
            );
            if let Some(y) = exit {
                assert!(pos(y) > 0.0, "v{v}: still long at the crossunder bar {y}");
                assert_eq!(
                    pos(y + 1),
                    0.0,
                    "v{v}: flat after the close at bar {}",
                    y + 1
                );
            }
        }
        if v == 6 {
            let (x, _) = trades[0];
            eprintln!(
                "e2e v{v}: {} trades ({} closed, {} open at end); first entry signal bar {x}, \
                 filled at bar {} open {:.2}; net {} bps×1e4, fee {} bps×1e4",
                trades.len(),
                closed_all,
                open_at_end,
                x + 1,
                rows[x + 1].open_1e6 as f64 / 1e6,
                o.summary.net_bps_1e4,
                o.summary.fee_bps_1e4
            );
        }

        // ---- bit-determinism over two runs ----
        let mut ws3 = Workspace::new();
        let again = run_trial(&t, &bars, &cfg, &mut ws3).expect("runs");
        assert_eq!(again, o, "v{v}: identical outcome");
        let a = RowRec {
            trial_id: [7; 32],
            summary: o.summary,
        }
        .to_le_bytes();
        let b = RowRec {
            trial_id: [7; 32],
            summary: again.summary,
        }
        .to_le_bytes();
        assert_eq!(a, b);
        let ra: Vec<u64> = ws.day_returns(0).iter().map(|d| d.to_bits()).collect();
        let rb: Vec<u64> = ws3.day_returns(0).iter().map(|d| d.to_bits()).collect();
        assert_eq!(ra, rb, "v{v}: identical return series bits");
        // A reused workspace gives the same row (the arena and accumulator start cold).
        let reused = run_trial(&t, &bars, &cfg, &mut ws3).expect("runs");
        assert_eq!(reused, o, "v{v}: a reused workspace");
    }
}
