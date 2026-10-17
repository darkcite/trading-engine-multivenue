// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33h acceptance — CAR-R33-1 (`BrokerView::netprofit_f64_bits`, the f64 running net profit)
//! and REQ-r33g-1 (`TvBroker::closed_cum_net`, TradingView's List of Trades `cum_pnl`), replayed
//! through the public surface. Each test rebuilds a captured E fixture's trades from its List of
//! Trades (every fill at its captured price and time, ×1e6), steps one bar per captured row and
//! compares every captured row: netprofit bit for bit (E086), equity bit for bit through the
//! captured open profit (E114), `cum_pnl` to the printed cent (E062). The `.fix` files are read
//! from `tests/conformance/fixtures/E/`.
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_broker::{TvBroker, TvStep};
use pine_ir::{
    intent_flag as f, scale, trade_flag, Bar, ChartInfo, CommissionType, IntentKind, IntentSide,
    OrderIntent, ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;
const HOUR_MS: i64 = 3_600_000;

/// A decimal text as ×1e6 (test-only parsing; at most six decimals).
fn d(s: &str) -> i64 {
    let (neg, s) = s.strip_prefix('-').map_or((false, s), |r| (true, r));
    let (i, frac) = s.split_once('.').unwrap_or((s, ""));
    assert!(frac.len() <= 6, "{s}");
    let mut v = i.parse::<i64>().unwrap_or_else(|e| panic!("{s}: {e:?}")) * E;
    let mut k = E / 10;
    for c in frac.chars() {
        v += i64::from(c.to_digit(10).unwrap_or_else(|| panic!("{s}"))) * k;
        k /= 10;
    }
    if neg {
        -v
    } else {
        v
    }
}

/// TradingView's cell as the f64 its text parses to.
fn cell(t: &str) -> f64 {
    t.parse().unwrap_or_else(|_| panic!("cell {t}"))
}

/// One List of Trades row.
#[derive(Clone)]
struct Trade {
    long: bool,
    entry_ms: i64,
    entry_px: i64,
    exit_ms: i64,
    exit_px: i64,
    exit_sig: String,
    qty: i64,
    cum_pnl: String,
}

/// One `.fix`: plot names, series rows (`time`, cells) and the List of Trades.
struct Fix {
    plots: Vec<String>,
    rows: Vec<(i64, Vec<String>)>,
    trades: Vec<Trade>,
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
                k.parse().unwrap_or_else(|_| panic!("{k}")),
                c[1..].iter().map(|s| s.to_string()).collect(),
            )),
            (2, "trades") => mode = 3,
            (3, "end") => mode = 4,
            (3, _) => trades.push(Trade {
                long: c[1] == "long",
                entry_ms: c[2].parse().unwrap_or_else(|_| panic!("{line}")),
                entry_px: d(c[3]),
                exit_ms: c[5].parse().unwrap_or(0),
                exit_px: if c[6] == "-" || c[6].is_empty() {
                    0
                } else {
                    d(c[6])
                },
                exit_sig: c[7].to_string(),
                qty: d(c[8]),
                cum_pnl: c[10].to_string(),
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
}

/// BTCUSDT.P on the captures: mintick 0.1, point value 1; 1 000 000 USD, margin 0.
fn meta(ct: CommissionType, cv_1e6: i64) -> ProgramMeta {
    ProgramMeta {
        chart: ChartInfo {
            symbol_hash: 1,
            timeframe_s: 3_600,
            venue: 0,
            _pad: [0; 3],
            mintick_1e6: 100_000,
            pointvalue_1e6: E,
            qty_step_1e6: 1,
            min_notional_1e6: 0,
        },
        decl: StrategyDecl {
            initial_capital_1e6: 1_000_000 * E,
            default_qty_value_1e6: E,
            pyramiding: 1,
            commission_type: ct,
            commission_value_1e6: cv_1e6,
            ..StrategyDecl::default()
        },
        ..ProgramMeta::default()
    }
}

const CFG: TvCfg = TvCfg {
    sizing: SizingMode::AsDeclared,
    day_offset_s: 0,
    _pad: 0,
};

/// A market `strategy.order` of `qty` (`buy` side), a fresh id per order.
fn order(n: u64, buy: bool, qty: i64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Order,
        side: if buy {
            IntentSide::Long
        } else {
            IntentSide::Short
        },
        id: StrKey(1000 + n),
        flags: f::QTY,
        qty_1e6: qty,
        seq: u32::try_from(n).unwrap_or(0),
        ..OrderIntent::default()
    }
}

/// A limit `strategy.order` (the entry of a bar whose open is another fill's price).
fn limit_order(n: u64, buy: bool, qty: i64, px: i64) -> OrderIntent {
    OrderIntent {
        flags: f::QTY | f::LIMIT,
        limit_1e6: px,
        ..order(n, buy, qty)
    }
}

fn close_all(n: u64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::CloseAll,
        seq: u32::try_from(n).unwrap_or(0),
        ..OrderIntent::default()
    }
}

/// The fills of the bar at `t`: trades leaving at its open, trades entering, trades entering and
/// leaving inside it (TV E062 trade 245), and the open — the exits' price, else the first entry's.
struct At<'a> {
    exits: Vec<&'a Trade>,
    entries: Vec<&'a Trade>,
    within: Vec<&'a Trade>,
    open: Option<i64>,
}

fn at(fx: &Fix, t: i64) -> At<'_> {
    let same = |x: &&Trade| x.entry_ms == t && x.exit_ms == t;
    let exits: Vec<&Trade> = fx
        .trades
        .iter()
        .filter(|x| x.exit_ms == t && !same(x))
        .collect();
    let entries: Vec<&Trade> = fx.trades.iter().filter(|x| x.entry_ms == t).collect();
    let within: Vec<&Trade> = fx.trades.iter().filter(same).collect();
    let open = exits
        .first()
        .map(|x| x.exit_px)
        .or_else(|| entries.first().map(|x| x.entry_px));
    At {
        exits,
        entries,
        within,
        open,
    }
}

/// Replays `fx`'s List of Trades on one bar per captured row: a fill at time `t` is an order the
/// bar before `t` sends, filled at the bar's open — exits first (one `close_all` when several
/// trades leave together or the exit is TradingView's `Close position order`, else a market
/// order against the oldest trade), then entries; an entry priced away from the bar's open (the
/// bar's exit took the open), and the exit of a trade that leaves inside its entry bar, rest as
/// limits the bar's path reaches. After each bar `check(row, broker)` runs.
fn replay(fx: &Fix, m: &ProgramMeta, mut check: impl FnMut(usize, &TvBroker)) -> TvBroker {
    let mut b = TvBroker::new(m, CFG).unwrap_or_else(|c| panic!("{c:?}"));
    let mut last_px = fx.trades.first().map_or(100 * E, |t| t.entry_px);
    let mut seq = 0u64;
    for (k, (t, _)) in fx.rows.iter().enumerate() {
        let a = at(fx, *t);
        let open = a.open.unwrap_or(last_px);
        let (mut hi, mut lo) = (open, open);
        for x in &a.entries {
            hi = hi.max(x.entry_px);
            lo = lo.min(x.entry_px);
        }
        for x in &a.within {
            hi = hi.max(x.exit_px);
            lo = lo.min(x.exit_px);
        }
        let bar = Bar {
            open_1e6: open,
            high_1e6: hi,
            low_1e6: lo,
            close_1e6: open,
            index: u32::try_from(k).unwrap_or(0),
            open_ms: *t,
            close_ms: *t + HOUR_MS,
            ..Bar::default()
        };
        last_px = open;
        // The orders for the next row's fills.
        let mut send: Vec<OrderIntent> = Vec::new();
        if let Some(n) = fx.rows.get(k + 1).map(|r| r.0) {
            let a = at(fx, n);
            if a.exits.len() > 1 || a.exits.iter().any(|x| x.exit_sig == "Close position order") {
                seq += 1;
                send.push(close_all(seq));
            } else if let Some(x) = a.exits.first() {
                seq += 1;
                send.push(order(seq, !x.long, x.qty));
            }
            for x in &a.entries {
                seq += 1;
                send.push(if Some(x.entry_px) == a.open {
                    order(seq, x.long, x.qty)
                } else {
                    limit_order(seq, x.long, x.qty, x.entry_px)
                });
            }
            for x in &a.within {
                seq += 1;
                send.push(limit_order(seq, !x.long, x.qty, x.exit_px));
            }
        }
        let mut step = b.open_bar(&bar);
        while let TvStep::Evaluate { intrabar, .. } = step {
            let s: &[OrderIntent] = if intrabar { &[] } else { &send };
            step = b.submit(s).unwrap_or_else(|e| panic!("row {t}: {e:?}"));
        }
        check(k, &b);
    }
    // The replay is faithful: every captured trade, at its captured times and prices.
    let v = *b.view();
    let mut i = 0u32;
    while i < v.closed_retained {
        let (a, x) = (b.closed_trade(i).expect("retained"), &fx.trades[i as usize]);
        assert_eq!(
            (a.entry_ms, a.entry_px_1e6, a.exit_ms, a.exit_px_1e6),
            (x.entry_ms, x.entry_px, x.exit_ms, x.exit_px),
            "trade {}",
            i + 1
        );
        assert_eq!(a.qty_1e6 > 0, x.long, "trade {} side", i + 1);
        i += 1;
    }
    assert_eq!(
        v.closed_retained as usize,
        fx.trades.len(),
        "every trade closed"
    );
    b
}

/// CAR-R33-1, `pin:04-trade-profit-f64` + the running sum — E086 `strategy.netprofit`: every
/// captured row (4320) is the view's `netprofit_f64_bits` bit for bit (−1595.800000000003 after
/// the first trade); the integer `netprofit_1e12 / 1e12` fits 72.
#[test]
fn netprofit_f64_bits_e086_every_row() {
    let fx = fix("E086");
    let j = fx.col("f_E086_net");
    let (mut rows, mut int_fit) = (0, 0);
    replay(&fx, &meta(CommissionType::Percent, 0), |k, b| {
        let tv = cell(&fx.rows[k].1[j]);
        let v = b.view();
        let got = f64::from_bits(v.netprofit_f64_bits);
        assert_eq!(
            got.to_bits(),
            tv.to_bits(),
            "row {}: {got} vs {tv}",
            fx.rows[k].0
        );
        int_fit += usize::from(scale::from_1e12(v.netprofit_1e12).to_bits() == tv.to_bits());
        rows += 1;
    });
    assert_eq!((rows, int_fit), (4320, 72));
}

/// CAR-R33-1 with fees, `pin:05-net-f64-fees-provisional` — E114 (0.1 % commission)
/// `strategy.equity`: every captured row (4320) is `(1e6 + netprofit) + openprofit` bit for bit,
/// the net profit from the view's bits (closed slices `gross − (entry_fee + exit_fee)`, less the
/// open trade's paid entry fee) and the open profit TradingView's own captured cell.
#[test]
fn netprofit_f64_bits_with_fees_e114_equity_every_row() {
    let fx = fix("E114");
    let (je, jo) = (fx.col("f_E114_eq"), fx.col("f_E114_oprofit"));
    let mut rows = 0;
    let b = replay(&fx, &meta(CommissionType::Percent, 100_000), |k, b| {
        let c = &fx.rows[k].1;
        let np = f64::from_bits(b.view().netprofit_f64_bits);
        let eq = (1_000_000.0 + np) + cell(&c[jo]);
        let tv = cell(&c[je]);
        assert_eq!(
            eq.to_bits(),
            tv.to_bits(),
            "row {}: {eq} vs {tv}",
            fx.rows[k].0
        );
        rows += 1;
    });
    assert_eq!(rows, 4320);
    // The integer law is untouched: the view's integer net carries the same fees.
    assert_eq!(
        b.view().netprofit_1e12,
        b.view().grossprofit_1e12 - b.view().grossloss_1e12
    );
}

/// Half a printed unit of `text` ×1e12 — the List of Trades tolerance (§3.16.5 money): TradingView
/// prints at most two decimals and drops trailing zeros (`37296.8` is ±0.05).
fn half_unit(text: &str) -> i128 {
    let dp = text.split_once('.').map_or(0, |(_, f)| f.len());
    5 * 10i128.pow(11 - u32::try_from(dp).unwrap_or(0))
}

/// REQ-r33g-1 — E062 (cash per order 1 USD, up to 16 trades open at once): a trade's `cum_pnl`
/// is `closed_cum_net`, the net profit when it closed — the still-open trades' paid entry fees
/// included (trade 1: −1602.94 = −1596.94 − six fees): 255 of 259 rows within half a printed
/// unit; the running sum of the closed trades' P&L fits 78. The four misses sit inside one
/// multi-trade `Close position order` each (trades 139–151 and two groups of 3), off by
/// 0.0004–0.0017 past the half cent; no exit-fee split or rounding tried fits them (r33h).
#[test]
fn closed_cum_net_e062_every_trade() {
    let fx = fix("E062");
    let b = replay(&fx, &meta(CommissionType::CashPerOrder, E), |_, _| {});
    let (mut miss, mut old_ok, mut run) = (Vec::new(), 0, 0i128);
    for (i, x) in fx.trades.iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(u32::MAX);
        let tv = i128::from(d(&x.cum_pnl)) * i128::from(E);
        let h = half_unit(&x.cum_pnl);
        let got = b.closed_cum_net(i).expect("retained");
        if (got - tv).abs() > h {
            miss.push(i + 1);
        }
        let t = b.closed_trade(i).expect("retained");
        assert_eq!(t.flags & trade_flag::OPEN, 0);
        run += t.pnl_1e12;
        old_ok += usize::from((run - tv).abs() <= h);
    }
    assert_eq!(miss, [146, 149, 167, 185]);
    assert_eq!((fx.trades.len(), old_ok), (259, 78));
    // Trade 1, to the cent: −1596.942857… less six open trades' 1 USD entry fees.
    let c1 = b.closed_cum_net(0).expect("trade 1");
    assert!((c1 - i128::from(d("-1602.94")) * i128::from(E)).abs() <= half_unit("-1602.94"));
    assert!(b.closed_cum_net(259).is_none());
}
