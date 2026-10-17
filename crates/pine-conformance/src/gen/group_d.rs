// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group D — `request.security` and chart time (impl-06 §4.2.4). GENERATOR-TIME ONLY.
//!
//! Chart-driven, `align time`, `tail 256`, `mode bar_close`, `dialects 56`, no prelude, unless a row
//! states otherwise. Row notation as group F: `@` stands for `_<ID>`; each leading `>` is one block
//! level.
//!
//! `pin:06-d-no-wrap` — a `request.security` call is emitted on one line: §4.2.0 permits wrapping
//! inside parentheses but never requires it (D005's example wraps; its text otherwise matches).

use pine_ir::{DiagCode, ExecMode};

use super::emit::{Line, Prelude, Script};
use super::group_c::RT_DEFAULT;
use super::{Cell, Run};
use crate::fixture::{
    Align, Dialects, Driver, Expect, FixtureId, Group, Oracle, RtSpec, Tol, Window,
};

/// `LookaheadLeak` (I12): an unoffset `lookahead_on` refused when compiled `live_bound = true`.
pub const LOOKAHEAD_LEAK: DiagCode = DiagCode(3043);

/// D053/D054's drive: SynTape on a 60 s chart with a 5-bar HTF (`"5"`).
pub const RT_D: RtSpec = RtSpec {
    htf_mult: 5,
    ..RT_DEFAULT
};

const TICK: &str = "syminfo.tickerid";

struct Row {
    n: u16,
    ds: Dialects,
    twin: u16,
    title: String,
    pins: Vec<&'static str>,
    body: Vec<String>,
    plots: Vec<(String, String, Tol)>,
    window: Window,
    oracle: Oracle,
    expect: Expect,
    tier: u8,
    run: Run,
}

fn chart() -> Run {
    Run {
        driver: Driver::Chart,
        align: Align::Time,
        ..Run::default()
    }
}

fn row(n: u16, title: &str, body: Vec<String>, plots: Vec<(String, String, Tol)>) -> Row {
    Row {
        n,
        ds: Dialects::BOTH,
        twin: 0,
        title: title.to_string(),
        pins: Vec::new(),
        body,
        plots,
        window: Window::Tail(256),
        oracle: Oracle::Tv,
        expect: Expect::Pending,
        // Tier 2 is the `request.*` family; D046's array is tier 3; D034 requests nothing.
        tier: match n {
            46 => 3,
            34 => 1,
            _ => 2,
        },
        run: chart(),
    }
}

fn iv(e: &str, k: &str) -> Vec<(String, String, Tol)> {
    vec![
        (format!("na({e}) ? 1 : 0"), format!("isna{k}"), Tol::Exact),
        (format!("na({e}) ? na : {e}"), format!("val{k}"), Tol::Exact),
    ]
}

fn sec(tf: &str, expr: &str, extra: &str) -> String {
    format!("request.security({TICK}, \"{tf}\", {expr}{extra})")
}

fn one(n: u16, title: &str, call: String) -> Row {
    row(n, title, vec![format!("h@ = {call}")], iv("h@", ""))
}

fn with_expect(mut r: Row, o: Oracle, code: u16) -> Row {
    r.oracle = o;
    r.expect = Expect::Refuse {
        code: DiagCode(code),
    };
    r
}

/// D-matrix (D001–D016): `D(8·tf + 4·gaps + 2·la + expr + 1)`.
fn matrix() -> Vec<Row> {
    let mut o = Vec::new();
    for (t, tf) in ["240", "D"].iter().enumerate() {
        for (g, gaps) in ["off", "on"].iter().enumerate() {
            for (l, la) in ["off", "on"].iter().enumerate() {
                for (x, expr) in ["close", "ta.sma(close, 5)"].iter().enumerate() {
                    let n = (8 * t + 4 * g + 2 * l + x + 1) as u16;
                    let call = sec(
                        tf,
                        expr,
                        &format!(
                            ", gaps = barmerge.gaps_{gaps}, lookahead = barmerge.lookahead_{la}"
                        ),
                    );
                    let mut r = one(
                        n,
                        &format!("{tf} {expr}, gaps {gaps}, lookahead {la}"),
                        call,
                    );
                    if l == 1 {
                        r.run.live_refuse = Some(LOOKAHEAD_LEAK);
                    }
                    o.push(r);
                }
            }
        }
    }
    o
}

/// The per-bar update machinery of the realtime rows (as group C's C-rt).
const RT_PRE: &[&str] = &[
    "varip int tl@ = 0",
    "varip int upd@ = 0",
    "varip int done@ = 0",
    "varip int k@ = 0",
    "if time != tl@",
    ">tl@ := time",
    ">upd@ := 0",
    ">done@ := 0",
    ">k@ := 0",
    "upd@ += 1",
    "if barstate.isconfirmed",
    ">done@ := 1",
];

/// D-rt (D053, D054): `k` counts the bar's updates where `h != close` (gaps off) or `na(h)` (gaps
/// on); the relation is read on rows with `rt = 1`, `done = 1` (`pin:06-c-rt-window`).
fn rt(n: u16, gaps_on: bool) -> Row {
    let mut b: Vec<String> = RT_PRE.iter().map(|s| s.to_string()).collect();
    let extra = if gaps_on {
        ", gaps = barmerge.gaps_on"
    } else {
        ""
    };
    b.push(format!("h@ = {}", sec("5", "close", extra)));
    b.push(if gaps_on {
        "k@ += na(h@) ? 1 : 0".to_string()
    } else {
        "k@ += h@ != close ? 1 : 0".to_string()
    });
    let plots = vec![
        (
            "barstate.isrealtime ? 1 : 0".to_string(),
            "rt".to_string(),
            Tol::Exact,
        ),
        ("done@".to_string(), "done".to_string(), Tol::Exact),
        ("k@".to_string(), "k".to_string(), Tol::Exact),
    ];
    let title = if gaps_on {
        "updates where the 5-minute value is na, gaps on"
    } else {
        "updates where the 5-minute value differs from close"
    };
    let mut r = row(n, title, b, plots);
    r.pins.push("pin:U12-lookahead-off-realtime");
    r.window = Window::Tail(RT_D.realtime);
    r.run = Run {
        driver: Driver::Rt,
        align: Align::Step,
        mode: ExecMode::Realtime,
        rt: Some(RT_D),
        live_refuse: None,
    };
    r
}

/// D017–D062.
///
/// `pin:06-d-expr-tf` — D-expr (D025–D028) requests `"240"`; D028's function is
/// `f() => close + 1.0`. `pin:06-d-coerce-shape` — a D-coerce row binds `a` and `b` and plots
/// `a == b ? 0 : 1`; a refused or declined timeframe binds one call and plots its isna/val pair.
/// `pin:06-d-unmapped-ticker` — D048/D049's unmapped ticker is `NASDAQ:AAPL` (valid on
/// TradingView, absent from `ticker_map`). `pin:06-d-boundary-time` — D041–D044 plot the HTF value
/// and its `time` as two isna/val pairs under the row's gaps; D045 plots `time` alone; D046 plots
/// `array.size` of the lower-timeframe array. `pin:06-d-time-readings-after-capture` — D055–D057
/// compare `align time` tails whose keys exist only after capture, so they carry no readings yet.
fn rest() -> Vec<Row> {
    let s = |x: &[&str]| x.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let mut o = Vec::new();
    for (n, tf) in [(17u16, "240"), (18, "D")] {
        o.push(one(
            n,
            &format!("{tf} close[1] with lookahead on"),
            sec(tf, "close[1]", ", lookahead = barmerge.lookahead_on"),
        ));
    }
    for (n, tf) in [
        (19u16, "60"),
        (20, "240"),
        (21, "D"),
        (22, "W"),
        (23, "5"),
        (24, "1"),
    ] {
        let mut r = one(n, &format!("close at {tf}"), sec(tf, "close", ""));
        if n >= 23 {
            r.pins.push("pin:06-ltf-intrabar");
        }
        o.push(r);
    }
    o.push(one(25, "close[1] at 240", sec("240", "close[1]", "")));
    let mut d26 = row(
        26,
        "a tuple [close, open] at 240",
        vec![format!("[c@, o@] = {}", sec("240", "[close, open]", ""))],
        iv("c@", "1"),
    );
    d26.plots.extend(iv("o@", "2"));
    o.push(d26);
    o.push(one(
        27,
        "ta.ema(close, 10) at 240",
        sec("240", "ta.ema(close, 10)", ""),
    ));
    let mut d28 = one(28, "a user function at 240", sec("240", "f@()", ""));
    d28.body.insert(0, "f@() => close + 1.0".to_string());
    o.push(d28);
    let pairs: [(u16, &str, &str); 5] = [
        (29, "\"60\"", "timeframe.period"),
        (30, "\"D\"", "\"1D\""),
        (31, "\"W\"", "\"1W\""),
        (32, "\"M\"", "\"1M\""),
        (33, "\"240\"", "input.timeframe(\"240\")"),
    ];
    for (n, a, b) in pairs {
        o.push(row(
            n,
            &format!("{a} against {b}"),
            vec![
                format!("a@ = request.security({TICK}, {a}, close)"),
                format!("b@ = request.security({TICK}, {b}, close)"),
            ],
            vec![(
                "a@ == b@ ? 0 : 1".to_string(),
                "val".to_string(),
                Tol::Exact,
            )],
        ));
    }
    o.push(row(
        34,
        "timeframe.in_seconds(\"240\") against 14400",
        vec![
            "a@ = timeframe.in_seconds(\"240\")".to_string(),
            "b@ = 14400".to_string(),
        ],
        vec![(
            "a@ == b@ ? 0 : 1".to_string(),
            "val".to_string(),
            Tol::Exact,
        )],
    ));
    for (n, tf, declined) in [
        (35u16, "1H", false),
        (36, "H", false),
        (37, "5S", true),
        (38, "2D", true),
        (39, "3M", true),
        (40, "45", true),
    ] {
        let r = one(n, &format!("timeframe {tf}"), sec(tf, "close", ""));
        let o2 = if declined {
            Oracle::Declined
        } else {
            Oracle::Authored
        };
        o.push(with_expect(r, o2, 5001));
    }
    for (n, tf, gaps) in [
        (41u16, "240", "off"),
        (42, "240", "on"),
        (43, "D", "off"),
        (44, "D", "on"),
    ] {
        let g = format!(", gaps = barmerge.gaps_{gaps}");
        let mut r = row(
            n,
            &format!("{tf} value and time, gaps {gaps}"),
            vec![
                format!("h@ = {}", sec(tf, "close", &g)),
                format!("t@ = {}", sec(tf, "time", &g)),
            ],
            iv("h@", "1"),
        );
        r.plots.extend(iv("t@", "2"));
        o.push(r);
    }
    o.push(row(
        45,
        "W time, the Monday anchor",
        vec![format!("t@ = {}", sec("W", "time", ""))],
        iv("t@", ""),
    ));
    o.push(row(
        46,
        "request.security_lower_tf at 5",
        vec![format!(
            "a@ = request.security_lower_tf({TICK}, \"5\", close)"
        )],
        iv("array.size(a@)", ""),
    ));
    o.push(one(
        47,
        "BINANCE:ETHUSDT.P at 60",
        "request.security(\"BINANCE:ETHUSDT.P\", \"60\", close)".to_string(),
    ));
    for (n, b) in [(48u16, "true"), (49, "false")] {
        let r = one(
            n,
            &format!("an unmapped ticker, ignore_invalid_symbol = {b}"),
            format!(
                "request.security(\"NASDAQ:AAPL\", \"60\", close, ignore_invalid_symbol = {b})"
            ),
        );
        o.push(with_expect(r, Oracle::Declined, 3042));
    }
    o.push(row(
        50,
        "the chart's own symbol and timeframe equal close",
        vec![format!("h@ = {}", sec("60", "close", ""))],
        vec![(
            "h@ == close ? 0 : 1".to_string(),
            "val".to_string(),
            Tol::Exact,
        )],
    ));
    for (n, ds, twin) in [(51u16, Dialects::V5, 52u16), (52, Dialects::V6, 51)] {
        let mut r = one(
            n,
            "an unoffset lookahead_on compiled live-bound",
            sec("240", "close", ", lookahead = barmerge.lookahead_on"),
        );
        r.ds = ds;
        r.twin = twin;
        o.push(with_expect(r, Oracle::Authored, 3043));
    }
    o.push(rt(53, false));
    o.push(rt(54, true));
    let mut d55 = row(
        55,
        "time and time_close at a timeframe below the chart's",
        s(&[
            "a@ = (time(\"1\") - time) / 60000",
            "b@ = (time_close(\"1\") - time) / 60000",
        ]),
        iv("a@", "1"),
    );
    d55.plots.extend(iv("b@", "2"));
    let mut d56 = row(
        56,
        "an overnight session under a day filter",
        s(&["t@ = time(\"60\", \"2200-0200:2\")"]),
        vec![("na(t@) ? 1 : 0".to_string(), "isna".to_string(), Tol::Exact)],
    );
    let d57 = with_expect(
        row(
            57,
            "a five-range session string",
            s(&["t@ = time(\"60\", \"0000-0100,0200-0300,0400-0500,0600-0700,0800-0900\")"]),
            vec![("na(t@) ? 1 : 0".to_string(), "isna".to_string(), Tol::Exact)],
        ),
        Oracle::Declined,
        5002,
    );
    let mut d57 = d57;
    for r in [&mut d55, &mut d56, &mut d57] {
        r.pins.push("pin:04-time-edges");
        r.tier = 1;
    }
    o.extend([d55, d56, d57]);
    let mut d58 = row(
        58,
        "the exchange time zone and the trading day",
        Vec::new(),
        [
            "syminfo.timezone == \"Etc/UTC\" ? 1 : 0",
            "(time - time_tradingday) / 3600000",
            "session.isfirstbar ? 1 : 0",
            "session.islastbar ? 1 : 0",
        ]
        .iter()
        .enumerate()
        .map(|(i, e)| (e.to_string(), format!("val{}", i + 1), Tol::Exact))
        .collect(),
    );
    let mut d59 = row(
        59,
        "ta.vwap(close) and its daily reset",
        s(&["v@ = ta.vwap(close)"]),
        vec![
            ("na(v@) ? 1 : 0".to_string(), "isna".to_string(), Tol::Exact),
            (
                "na(v@) ? na : v@".to_string(),
                "val".to_string(),
                Tol::Ulp(64),
            ),
        ],
    );
    for r in [&mut d58, &mut d59] {
        r.pins.push("pin:04-exchange-tz");
        r.tier = 1;
    }
    o.extend([d58, d59]);
    let mut d60 = row(
        60,
        "the first loaded bar",
        s(&["var int t0@ = time"]),
        vec![
            ("t0@".to_string(), "t0".to_string(), Tol::Exact),
            ("bar_index".to_string(), "bar_index".to_string(), Tol::Exact),
        ],
    );
    d60.pins.push("pin:00-dataset-start");
    d60.oracle = Oracle::TvOnly;
    d60.tier = 1;
    o.push(d60);
    o.push(with_expect(
        one(
            61,
            "a currency-converted request",
            sec("240", "close", ", currency = currency.EUR"),
        ),
        Oracle::Declined,
        3053,
    ));
    o.push(with_expect(
        one(
            62,
            "request.currency_rate",
            "request.currency_rate(currency.USD, currency.EUR)".to_string(),
        ),
        Oracle::Declined,
        3053,
    ));
    o
}

fn line(s: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    let depth = s.bytes().take_while(|&c| c == b'>').count();
    Line {
        depth: depth as u8,
        text: s[depth..].replace('@', &sfx),
    }
}

fn cell(r: Row) -> Cell {
    let id = FixtureId::from_parts(Group::D, r.n);
    let sfx = format!("_{}", id.as_str());
    let tols = r.plots.iter().map(|p| p.2).collect();
    let plots = r
        .plots
        .into_iter()
        .map(|(e, k, _)| (e.replace('@', &sfx), k))
        .collect();
    let pins: Vec<String> = r.pins.iter().map(|p| p.to_string()).collect();
    let twin = (r.twin != 0).then(|| FixtureId::from_parts(Group::D, r.twin));
    let block = match r.n {
        1..=16 => "D-matrix",
        17..=18 => "D-leakfree",
        19..=24 => "D-tf",
        25..=28 => "D-expr",
        29..=40 => "D-coerce",
        41..=46 => "D-boundary",
        47..=50 => "D-symbol",
        51..=52 => "D-refuse",
        53..=54 => "D-rt",
        55..=57 => "D-time",
        58..=59 => "D-tz",
        60 => "D-start",
        _ => "D-fx",
    };
    let script = Script {
        id,
        dialects: r.ds,
        about: vec![r.title.clone(), block.to_string()],
        twin,
        pins: pins.clone(),
        decl: None,
        prelude: Prelude::None,
        body: r.body.iter().map(|s| line(s, id)).collect(),
        plots,
    };
    Cell {
        script,
        title: r.title,
        pins,
        oracle: r.oracle,
        tier: r.tier,
        window: r.window,
        tols,
        expect: r.expect,
        readings: Vec::new(),
        run: r.run,
    }
}

/// Every group-D cell (D001–D062).
pub fn cells() -> Vec<Cell> {
    let mut rows = matrix();
    rows.extend(rest());
    rows.sort_by_key(|r| r.n);
    rows.into_iter().map(cell).collect()
}
