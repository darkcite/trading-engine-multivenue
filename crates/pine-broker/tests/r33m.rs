// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! r33m acceptance — CAR-R33K-1 (`BrokerView::max_drawdown_pct_f64_bits`,
//! `pin:04-max-dd-pct-equity-f64`), replayed through the public `TvBroker` surface on TV E118
//! (`strategy.max_drawdown_percent`, 45 long trades of 1 BTCUSDT.P, commission-free, every fill a
//! market order at a bar's open). The List of Trades comes from `tests/conformance/fixtures/E/
//! E118.fix`; the chart's OHLC is not in the tree, so the replay steps one bar per captured row,
//! flat at the last fill price (the fill points, which TradingView marks too), except at the 35
//! rows where the captured `f_E118_mddp` rises: there the bar is the captured BTCUSDT.P 1 h OHLC
//! (`fullbars-tvohlc-r33j`), embedded below. A running maximum only changes where it rises, so
//! every row of the capture (4320) must equal the view's bits — the 35 rising rows by the law on
//! the real path, the rest by keeping the maximum (no flat fill point may exceed it).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_broker::{TvBroker, TvStep};
use pine_ir::{
    intent_flag as f, Bar, ChartInfo, CommissionType, IntentKind, IntentSide, OrderIntent,
    ProgramMeta, SizingMode, StrKey, StrategyDecl, TvCfg,
};

const E: i64 = 1_000_000;
const HOUR_MS: i64 = 3_600_000;

/// The captured BTCUSDT.P 1 h bars (`open_ms`, open, high, low, close ×1e6) of the 35 E118 rows
/// whose `f_E118_mddp` differs from the row before — trades 1, 2, 10 and 11.
#[rustfmt::skip]
const RISE: [(i64, i64, i64, i64, i64); 35] = [
    (1_776_481_200_000, 77_249_600_000, 77_254_300_000, 76_903_200_000, 76_964_300_000),
    (1_776_484_800_000, 76_964_400_000, 77_131_800_000, 76_824_100_000, 77_065_900_000),
    (1_776_499_200_000, 76_951_000_000, 77_004_200_000, 76_460_000_000, 76_695_100_000),
    (1_776_502_800_000, 76_695_100_000, 76_695_100_000, 76_357_100_000, 76_557_100_000),
    (1_776_506_400_000, 76_557_100_000, 76_557_200_000, 76_091_800_000, 76_150_000_000),
    (1_776_510_000_000, 76_150_000_000, 76_150_000_000, 75_710_600_000, 76_002_000_000),
    (1_776_528_000_000, 76_124_800_000, 76_176_600_000, 75_584_100_000, 75_897_900_000),
    (1_776_535_200_000, 75_783_900_000, 75_805_100_000, 75_395_900_000, 75_603_700_000),
    (1_776_567_600_000, 75_450_000_000, 75_607_900_000, 75_314_000_000, 75_473_000_000),
    (1_776_578_400_000, 75_505_100_000, 75_505_100_000, 75_234_200_000, 75_359_700_000),
    (1_776_582_000_000, 75_359_600_000, 75_363_200_000, 74_824_300_000, 75_205_600_000),
    (1_776_618_000_000, 75_345_500_000, 75_348_400_000, 74_560_000_000, 74_742_400_000),
    (1_776_621_600_000, 74_742_300_000, 74_876_900_000, 74_550_000_000, 74_736_200_000),
    (1_776_628_800_000, 74_918_000_000, 74_920_200_000, 74_375_000_000, 74_591_600_000),
    (1_776_632_400_000, 74_591_700_000, 74_776_300_000, 74_304_500_000, 74_379_500_000),
    (1_776_636_000_000, 74_379_400_000, 74_379_500_000, 73_700_200_000, 74_005_000_000),
    (1_776_643_200_000, 73_758_400_000, 74_322_900_000, 73_669_000_000, 74_228_100_000),
    (1_780_488_000_000, 67_040_100_000, 67_142_100_000, 66_760_000_000, 67_050_800_000),
    (1_780_491_600_000, 67_050_700_000, 67_220_200_000, 66_605_100_000, 66_884_800_000),
    (1_780_498_800_000, 66_841_000_000, 66_880_000_000, 66_036_200_000, 66_046_500_000),
    (1_780_502_400_000, 66_046_600_000, 66_359_400_000, 65_508_300_000, 66_037_400_000),
    (1_780_513_200_000, 66_006_500_000, 66_030_700_000, 65_199_000_000, 65_428_600_000),
    (1_780_516_800_000, 65_428_500_000, 65_581_600_000, 64_711_100_000, 64_971_700_000),
    (1_780_527_600_000, 64_882_100_000, 64_957_700_000, 64_050_800_000, 64_117_700_000),
    (1_780_531_200_000, 64_117_700_000, 64_412_900_000, 62_911_900_000, 63_314_400_000),
    (1_780_534_800_000, 63_314_500_000, 63_478_500_000, 62_066_600_000, 62_140_300_000),
    (1_780_538_400_000, 62_140_400_000, 63_598_700_000, 61_344_800_000, 63_198_400_000),
    (1_780_639_200_000, 62_294_100_000, 62_614_200_000, 61_067_600_000, 61_916_800_000),
    (1_780_959_600_000, 63_407_600_000, 63_567_600_000, 62_939_700_000, 63_057_900_000),
    (1_780_963_200_000, 63_057_900_000, 63_157_300_000, 62_381_200_000, 62_522_600_000),
    (1_781_010_000_000, 62_611_600_000, 62_717_700_000, 62_125_000_000, 62_244_400_000),
    (1_781_013_600_000, 62_244_400_000, 62_371_600_000, 61_075_900_000, 61_511_600_000),
    (1_781_017_200_000, 61_511_600_000, 61_680_900_000, 61_001_000_000, 61_100_700_000),
    (1_781_020_800_000, 61_100_600_000, 61_490_000_000, 60_755_000_000, 61_367_500_000),
    (1_781_082_000_000, 61_228_800_000, 61_396_400_000, 60_691_900_000, 61_302_700_000),
];

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

/// One List of Trades row.
struct Trade {
    long: bool,
    entry_ms: i64,
    entry_px: i64,
    exit_ms: i64,
    exit_px: i64,
    close_position: bool,
    qty: i64,
}

/// E118's `f_E118_mddp` column (`time`, text) and its List of Trades.
fn e118() -> (Vec<(i64, String)>, Vec<Trade>) {
    let p = format!(
        "{}/../../tests/conformance/fixtures/E/E118.fix",
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
            (1, k) => rows.push((k, c[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>())),
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
                close_position: c[7] == "Close position order",
                qty: d(c[8]),
            }),
            _ => {}
        }
    }
    let j = plots
        .iter()
        .position(|p| p == "f_E118_mddp")
        .unwrap_or_else(|| panic!("no plot f_E118_mddp"));
    let col = rows
        .into_iter()
        .map(|(k, c)| {
            let t = k.parse().unwrap_or_else(|_| panic!("{k}"));
            (t, c[j].clone())
        })
        .collect();
    (col, trades)
}

/// BTCUSDT.P on the captures: mintick 0.1, point value 1; 1 000 000 USD, fixed 1, no commission.
fn meta() -> ProgramMeta {
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
            commission_type: CommissionType::Percent,
            commission_value_1e6: 0,
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
fn order(n: u32, buy: bool, qty: i64) -> OrderIntent {
    OrderIntent {
        kind: IntentKind::Order,
        side: if buy {
            IntentSide::Long
        } else {
            IntentSide::Short
        },
        id: StrKey(1000 + u64::from(n)),
        flags: f::QTY,
        qty_1e6: qty,
        seq: n,
        ..OrderIntent::default()
    }
}

/// The fill price at the open of the bar at `t` (exit first, else the entry), if any.
fn fill_at(trades: &[Trade], t: i64) -> Option<i64> {
    let x = trades.iter().find(|x| x.exit_ms == t).map(|x| x.exit_px);
    x.or_else(|| trades.iter().find(|x| x.entry_ms == t).map(|x| x.entry_px))
}

/// CAR-R33K-1 — E118 `strategy.max_drawdown_percent`: every captured row (4320) is the view's
/// `max_drawdown_pct_f64_bits` bit for bit (`(peak − eq) / peak * 100`, `eq = (ic + NP) + op`,
/// the peak the closed equity after each close): 0.03464000000000233 at trade 1's entry-bar low,
/// 0.185920000000007 at trade 2's low (the peak still the initial capital), 0.36576484147764066
/// in trade 10 (the closed-equity peak 1003158.2), up to 1.1628474950411563 in trade 11.
#[test]
fn max_drawdown_pct_f64_bits_e118_every_row() {
    let (rows, trades) = e118();
    assert_eq!((rows.len(), trades.len()), (4320, 45));
    // The embedded bars are exactly the rows where the capture rises.
    let mut rises = Vec::new();
    let mut prev = "0";
    for (t, txt) in &rows {
        if txt != prev {
            rises.push(*t);
            prev = txt;
        }
    }
    let embedded: Vec<i64> = RISE.iter().map(|r| r.0).collect();
    assert_eq!(rises, embedded, "the rising rows");
    let mut b = TvBroker::new(&meta(), CFG).unwrap_or_else(|c| panic!("{c:?}"));
    let mut last_px = trades[0].entry_px;
    let (mut seq, mut ok, mut real) = (0u32, 0usize, 0usize);
    for (k, (t, txt)) in rows.iter().enumerate() {
        let fill = fill_at(&trades, *t);
        let (o, h, l, c) = match RISE.iter().find(|r| r.0 == *t) {
            Some(r) => {
                // A fill on a real bar is at its open (market orders, E118).
                assert!(fill.is_none_or(|p| p == r.1), "row {t}: fill off the open");
                real += 1;
                (r.1, r.2, r.3, r.4)
            }
            None => {
                let p = fill.unwrap_or(last_px);
                (p, p, p, p)
            }
        };
        if let Some(p) = fill {
            last_px = p;
        }
        let bar = Bar {
            open_1e6: o,
            high_1e6: h,
            low_1e6: l,
            close_1e6: c,
            index: u32::try_from(k).unwrap_or(0),
            open_ms: *t,
            close_ms: *t + HOUR_MS,
            ..Bar::default()
        };
        // The orders for the next row's fills: the exit (TradingView's `Close position order`
        // is `close_all`), then the entry.
        let mut send: Vec<OrderIntent> = Vec::new();
        if let Some((n, _)) = rows.get(k + 1) {
            if let Some(x) = trades.iter().find(|x| x.exit_ms == *n) {
                seq += 1;
                send.push(if x.close_position {
                    OrderIntent {
                        kind: IntentKind::CloseAll,
                        seq,
                        ..OrderIntent::default()
                    }
                } else {
                    order(seq, !x.long, x.qty)
                });
            }
            if let Some(x) = trades.iter().find(|x| x.entry_ms == *n) {
                seq += 1;
                send.push(order(seq, x.long, x.qty));
            }
        }
        let mut step = b.open_bar(&bar);
        while let TvStep::Evaluate { intrabar, .. } = step {
            let s: &[OrderIntent] = if intrabar { &[] } else { &send };
            step = b.submit(s).unwrap_or_else(|e| panic!("row {t}: {e:?}"));
        }
        let tv: f64 = txt.parse().unwrap_or_else(|_| panic!("cell {txt}"));
        let got = f64::from_bits(b.view().max_drawdown_pct_f64_bits);
        assert_eq!(got.to_bits(), tv.to_bits(), "row {t}: {got:?} vs {txt}");
        ok += 1;
    }
    assert_eq!((ok, real), (4320, 35));
    // The replay is faithful: every captured trade, at its captured times and prices.
    let v = *b.view();
    assert_eq!((v.closedtrades, v.opentrades), (45, 0));
    let mut i = 0u32;
    while i < v.closed_retained {
        let (a, x) = (b.closed_trade(i).expect("retained"), &trades[i as usize]);
        assert_eq!(
            (
                a.entry_ms,
                a.entry_px_1e6,
                a.exit_ms,
                a.exit_px_1e6,
                a.qty_1e6
            ),
            (x.entry_ms, x.entry_px, x.exit_ms, x.exit_px, x.qty),
            "trade {}",
            i + 1
        );
        i += 1;
    }
    assert_eq!(v.max_drawdown_pct_f64_bits, 1.1628474950411563f64.to_bits());
}
