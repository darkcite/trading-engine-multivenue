// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group E — the broker (impl-06 §4.2.5, E001–E121). GENERATOR-TIME ONLY.
//!
//! `Driver::SynSig` (SYN-T decides *when*, the chart *at what price*), `align time`,
//! `window time FROM_MS TO_MS`, `dialects 56`, `mode bar_close`, oracle `tv` (body pending until
//! capture), unless a row states otherwise. Every fixture but E109/E110/E119 plots
//! `f_<ID>_pos = strategy.position_size`. Row notation as groups D/F: `@` stands for `_<ID>`; each
//! leading `>` is one block level.
//!
//! Pins this file takes (literal readings where §4.2.5 is silent):
//! * `pin:06-e-tier` — tier 1: every `strategy.*` name the group calls is `pine_spec` tier 1.
//! * `pin:06-e-header-layout` — the baseline header's five lines are kept for every fixture; a varied
//!   argument is substituted in place, an omitted one (W18's margins, E089's `default_qty_type`)
//!   drops out of its line, and an argument the baseline lacks (`close_entries_rule`, `currency`,
//!   `fill_orders_on_standard_ohlc`, `backtest_fill_limits_assumption`) goes on a sixth line.
//! * `pin:06-e-price-grid` — every price an order names is `math.round_to_mintick(…)` of `close` or
//!   `strategy.position_avg_price` times a constant (on the grid, so `pin:05-price-rounding` can
//!   confound no fixture but E113); tick distances are literals (`profit = 2000` …).
//! * `pin:06-e-ord-legs` — an E-ord shape's other leg is the simplest market leg; `close` /
//!   `close_all` are exercised from a short entry so they are not E001's bytes.
//! * `pin:06-e-pyr-ids` — E-pyr is `E(33 + 2·shape + (pyramiding − 2))`, shapes entry_market,
//!   entry_limit, entry_stop, order_market (signal entries without a flat guard); E041–E043 are
//!   long/short reversals at `pyramiding` 1, 2, 3.
//! * `pin:06-e-siz-values` — percent_of_equity at `default_qty_value = 10`, cash at `100000`.
//! * `pin:06-e-oca-shapes` — "two entries" is the breakout pair (buy stop above, sell stop below);
//!   "entry + exit" is an add (`strategy.order` long limit below) and an exit (`strategy.order` short
//!   limit above) in one group while long. `pin:06-e-oca-reduce-commission` — E061/E062's
//!   `cash_per_order` fee is `commission_value = 1` (a zero fee could not count the orders).
//! * `pin:06-e-costs-values` — percent `0.1`, cash_per_contract / cash_per_order `2`, slippage `5`.
//! * `pin:06-e-path-selection` — a script cannot choose which bar fills its order, so E073–E075 arm a
//!   bracket every bar while in the position and plot the bar's class `f_<ID>_cls` (1 open nearer the
//!   high, −1 nearer the low, 0 the tie) for triage: E073 long ±0.3 %, E074 short ±0.3 %, E075 long
//!   ±0.1 % (both legs inside nearly every bar's range).
//! * `pin:06-e-risk-values` — E091 `max_drawdown(2000, strategy.cash)`, E092
//!   `max_intraday_loss(500, strategy.cash)`, E093 `(2)`, E094 `(2)`, E095 long only on the reversal
//!   shape, E096 `max_position_size(2)` at `pyramiding = 3`.
//! * `pin:06-e089-default-type` — "header `default_qty_type`": E089 omits it, so the declaration's
//!   own default (`strategy.fixed`) is what is compared.
//! * `pin:06-e121-shape` — E121's four legs: a below-market limit entry (`syni < 20`), a stop-limit
//!   entry (`20 ≤ syni < 40`), a take-profit `strategy.exit` re-issued while long, and a
//!   `strategy.order` short limit above on `syni > 980`.
//!
//! Readings of the E-pin5 rows predict List-of-Trades rows, which depend on the chart's prices: they
//! wait for the captured bars (lead ruling, no invented readings).

use pine_ir::{DiagCode, ExecMode};

use super::emit::{self, Line, Prelude, Script};
use super::group_c::RT_DEFAULT;
use super::{Cell, Run};
use crate::chart::{spec, STOP_MS};
use crate::fixture::{Align, Dialects, Driver, Expect, FixtureId, Group, Oracle, Tol, Window};
use crate::syn::SynSeq;

/// `CurrencyConversion` (CAR-02-1): E119's declined code.
pub const CURRENCY_CONVERSION: DiagCode = DiagCode(3053);

/// SYN-T's entry signal threshold: `syni < ENTER_BELOW`.
pub const ENTER_BELOW: i64 = 20;
/// SYN-T's exit signal threshold: `syni > EXIT_ABOVE`.
pub const EXIT_ABOVE: i64 = 980;
/// §4.2.5 "Trade counts": a market shape's cycles over the window lie in `[8, 60]`.
pub const CYCLES_IN: (u32, u32) = (8, 60);

const ON_LO: &str = "_on and syni < 20";
const ON_HI: &str = "_on and syni > 980";
const FLAT: &str = "strategy.position_size == 0";
const LONG: &str = "strategy.position_size > 0";
const SHORT: &str = "strategy.position_size < 0";
const BELOW: &str = "math.round_to_mintick(close * 0.995)";
const ABOVE: &str = "math.round_to_mintick(close * 1.005)";
const ABOVE2: &str = "math.round_to_mintick(close * 1.006)";
const FAR_BELOW: &str = "math.round_to_mintick(close * 0.97)";

/// The strategy declaration's varied arguments (the baseline is [`Hdr::base`]).
#[derive(Debug, Clone, PartialEq)]
struct Hdr {
    capital: &'static str,
    qty_type: Option<&'static str>,
    qty_value: &'static str,
    pyramiding: u8,
    comm_type: &'static str,
    comm_value: &'static str,
    slippage: &'static str,
    margin: Option<(&'static str, &'static str)>,
    on_close: bool,
    on_fills: bool,
    every_tick: bool,
    extra: Vec<&'static str>,
}

impl Hdr {
    fn base() -> Self {
        Self {
            capital: "1000000",
            qty_type: Some("strategy.fixed"),
            qty_value: "1",
            pyramiding: 1,
            comm_type: "strategy.commission.percent",
            comm_value: "0",
            slippage: "0",
            margin: Some(("0", "0")),
            on_close: false,
            on_fills: false,
            every_tick: false,
            extra: Vec::new(),
        }
    }

    /// The declaration text (`pin:06-e-header-layout`).
    fn render(&self, id: FixtureId) -> String {
        let mut l2 = Vec::new();
        if let Some(t) = self.qty_type {
            l2.push(format!("default_qty_type = {t}"));
        }
        l2.push(format!("default_qty_value = {}", self.qty_value));
        l2.push(format!("pyramiding = {}", self.pyramiding));
        let mut l4 = Vec::new();
        if let Some((a, b)) = self.margin {
            l4.push(format!("margin_long = {a}"));
            l4.push(format!("margin_short = {b}"));
        }
        l4.push(format!("process_orders_on_close = {}", self.on_close));
        let close5 = if self.extra.is_empty() { ")" } else { "," };
        let mut lines = vec![
            format!(
                "strategy(\"{}\", overlay = false, initial_capital = {},",
                id.as_str(),
                self.capital
            ),
            format!("{},", l2.join(", ")),
            format!(
                "commission_type = {}, commission_value = {}, slippage = {},",
                self.comm_type, self.comm_value, self.slippage
            ),
            format!("{},", l4.join(", ")),
            format!(
                "calc_on_order_fills = {}, calc_on_every_tick = {}, use_bar_magnifier = false{close5}",
                self.on_fills, self.every_tick
            ),
        ];
        if !self.extra.is_empty() {
            lines.push(format!("{})", self.extra.join(", ")));
        }
        lines.join(&format!("\n{}", emit::continuation()))
    }
}

/// How the generator counts a market shape's cycles with the `SynSeq` twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Flat → in on `syni < 20`, in → flat on `syni > 980` (either direction).
    Cycle,
    /// Long on `syni < 10`, short on `syni > 989` ([`REV_BELOW`]): one cycle per flip.
    Reverse,
}

struct Row {
    n: u16,
    ds: Dialects,
    twin: u16,
    title: String,
    pins: Vec<&'static str>,
    hdr: Hdr,
    body: Vec<String>,
    plots: Vec<(String, String)>,
    oracle: Oracle,
    expect: Expect,
    run: Run,
    window: Window,
    prelude: bool,
    kind: Option<Kind>,
}

fn synsig() -> Run {
    Run {
        driver: Driver::SynSig,
        align: Align::Time,
        ..Run::default()
    }
}

fn pos() -> (String, String) {
    ("strategy.position_size".to_string(), "pos".to_string())
}

fn when(cond: &str, acts: &[&str]) -> Vec<String> {
    let mut o = vec![format!("if {cond}")];
    o.extend(acts.iter().map(|a| format!(">{a}")));
    o
}

fn and(a: &str, b: &str) -> String {
    format!("{a} and {b}")
}

fn last() -> Vec<String> {
    when("_last", &["strategy.cancel_all()", "strategy.close_all()"])
}

/// The E-ord shapes (§4.2.5), reused by E-tim, E-pyr, E-siz and the later blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    EntryMarket,
    EntryLimit,
    EntryStop,
    EntryStopLimit,
    OrderMarket,
    OrderLimit,
    Close,
    CloseAll,
    ExitProfit,
    ExitLoss,
    ExitStop,
    ExitTrail,
    Cancel,
    CancelAll,
}

const ORD: [(Shape, &str); 14] = [
    (Shape::EntryMarket, "entry market"),
    (Shape::EntryLimit, "entry limit"),
    (Shape::EntryStop, "entry stop"),
    (Shape::EntryStopLimit, "entry stop-limit"),
    (Shape::OrderMarket, "order market"),
    (Shape::OrderLimit, "order limit"),
    (Shape::Close, "close"),
    (Shape::CloseAll, "close_all"),
    (Shape::ExitProfit, "exit profit"),
    (Shape::ExitLoss, "exit loss"),
    (Shape::ExitStop, "exit stop"),
    (Shape::ExitTrail, "exit trail"),
    (Shape::Cancel, "cancel"),
    (Shape::CancelAll, "cancel_all"),
];

fn kind_of(s: Shape) -> Option<Kind> {
    match s {
        Shape::EntryMarket | Shape::OrderMarket | Shape::Close | Shape::CloseAll => {
            Some(Kind::Cycle)
        }
        _ => None,
    }
}

/// A shape's body without the `_last` block. `guard` = the flat guard on the entry signal (off for
/// E-pyr's pyramided entries).
fn shape_body(s: Shape, guard: bool) -> Vec<String> {
    let enter = if guard {
        and(ON_LO, FLAT)
    } else {
        ON_LO.to_string()
    };
    let close_long = |act: &str| when(&and(ON_HI, LONG), &[act]);
    let entry = |args: &str| format!("strategy.entry(\"L1\", strategy.long{args})");
    let exit_leg = |args: &str| when(LONG, &[&format!("strategy.exit(\"X1\", \"L1\", {args})")]);
    let mut o = Vec::new();
    match s {
        Shape::EntryMarket => {
            o.extend(when(&enter, &[&entry("")]));
            o.extend(close_long("strategy.close(\"L1\")"));
        }
        Shape::EntryLimit => {
            o.extend(when(&enter, &[&entry(&format!(", limit = {BELOW}"))]));
            o.extend(close_long("strategy.close(\"L1\")"));
        }
        Shape::EntryStop => {
            o.extend(when(&enter, &[&entry(&format!(", stop = {ABOVE}"))]));
            o.extend(close_long("strategy.close(\"L1\")"));
        }
        Shape::EntryStopLimit => {
            o.extend(when(
                &enter,
                &[&entry(&format!(", stop = {ABOVE}, limit = {ABOVE2}"))],
            ));
            o.extend(close_long("strategy.close(\"L1\")"));
        }
        Shape::OrderMarket => {
            o.extend(when(&enter, &["strategy.order(\"O1\", strategy.long)"]));
            o.extend(close_long(
                "strategy.order(\"X1\", strategy.short, strategy.position_size)",
            ));
        }
        Shape::OrderLimit => {
            o.extend(when(
                &enter,
                &[&format!(
                    "strategy.order(\"O1\", strategy.long, limit = {BELOW})"
                )],
            ));
            o.extend(close_long(
                "strategy.order(\"X1\", strategy.short, strategy.position_size)",
            ));
        }
        Shape::Close | Shape::CloseAll => {
            o.extend(when(&enter, &["strategy.entry(\"S1\", strategy.short)"]));
            let act = if s == Shape::Close {
                "strategy.close(\"S1\")"
            } else {
                "strategy.close_all()"
            };
            o.extend(when(&and(ON_HI, SHORT), &[act]));
        }
        Shape::ExitProfit => {
            o.extend(when(&enter, &[&entry("")]));
            o.extend(exit_leg("profit = 2000"));
        }
        Shape::ExitLoss => {
            o.extend(when(&enter, &[&entry("")]));
            o.extend(exit_leg("loss = 2000"));
        }
        Shape::ExitStop => {
            o.extend(when(&enter, &[&entry("")]));
            o.extend(exit_leg(
                "stop = math.round_to_mintick(strategy.position_avg_price * 0.99)",
            ));
        }
        Shape::ExitTrail => {
            o.extend(when(&enter, &[&entry("")]));
            o.extend(exit_leg("trail_points = 2000, trail_offset = 500"));
        }
        Shape::Cancel | Shape::CancelAll => {
            o.extend(when(&enter, &[&entry(&format!(", limit = {FAR_BELOW}"))]));
            let act = if s == Shape::Cancel {
                "strategy.cancel(\"L1\")"
            } else {
                "strategy.cancel_all()"
            };
            o.extend(when(&and(ON_HI, FLAT), &[act]));
            o.extend(close_long("strategy.close(\"L1\")"));
        }
    }
    o
}

fn row(n: u16, title: &str, hdr: Hdr, mut body: Vec<String>, kind: Option<Kind>) -> Row {
    body.extend(last());
    Row {
        n,
        ds: Dialects::BOTH,
        twin: 0,
        title: title.to_string(),
        pins: Vec::new(),
        hdr,
        body,
        plots: vec![pos()],
        oracle: Oracle::Tv,
        expect: Expect::Pending,
        run: synsig(),
        window: Window::Time {
            from_ms: spec::FROM_MS,
            to_ms: spec::TO_MS,
        },
        prelude: true,
        kind,
    }
}

fn shaped(n: u16, title: &str, hdr: Hdr, s: Shape) -> Row {
    row(n, title, hdr, shape_body(s, true), kind_of(s))
}

/// The reversal shape's thresholds: `syni < 10` long, `syni > 989` short. At E-ord's 20 / 980 a
/// flip-every-signal shape counts 89 cycles over the window, outside [`CYCLES_IN`]; these halve it
/// (§4.2.5 "thresholds are chosen so …").
pub const REV_BELOW: i64 = 10;
/// See [`REV_BELOW`].
pub const REV_ABOVE: i64 = 989;

fn reverse_body() -> Vec<String> {
    let mut b = when(
        "_on and syni < 10",
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(
        "_on and syni > 989",
        &["strategy.entry(\"S1\", strategy.short)"],
    ));
    b
}

fn s(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

/// E001–E056: E-ord, E-tim, E-pyr, E-siz (T10).
fn part1() -> Vec<Row> {
    let mut o = Vec::new();
    // E-ord (E001–E014).
    for (i, (sh, t)) in ORD.iter().enumerate() {
        o.push(shaped(
            i as u16 + 1,
            &format!("{t}, baseline"),
            Hdr::base(),
            *sh,
        ));
    }
    // E-tim (E015–E032): E(15 + 3·shape + flag).
    let tim = [
        (Shape::EntryMarket, "entry market"),
        (Shape::EntryLimit, "entry limit"),
        (Shape::EntryStop, "entry stop"),
        (Shape::ExitProfit, "exit profit"),
        (Shape::ExitStop, "exit stop"),
        (Shape::Close, "close"),
    ];
    let flags = [
        "process_orders_on_close",
        "calc_on_order_fills",
        "calc_on_every_tick",
    ];
    for (si, (sh, t)) in tim.iter().enumerate() {
        for (fi, f) in flags.iter().enumerate() {
            let mut h = Hdr::base();
            match fi {
                0 => h.on_close = true,
                1 => h.on_fills = true,
                _ => h.every_tick = true,
            }
            let mut r = shaped((15 + 3 * si + fi) as u16, &format!("{t}, {f}"), h, *sh);
            if fi > 0 {
                r.run.mode = ExecMode::Realtime;
            }
            o.push(r);
        }
    }
    // E-pyr (E033–E044).
    let pyr = [
        (Shape::EntryMarket, "entry market"),
        (Shape::EntryLimit, "entry limit"),
        (Shape::EntryStop, "entry stop"),
        (Shape::OrderMarket, "order market"),
    ];
    for (si, (sh, t)) in pyr.iter().enumerate() {
        for p in 2u8..=3 {
            let mut h = Hdr::base();
            h.pyramiding = p;
            o.push(row(
                (33 + 2 * si + usize::from(p - 2)) as u16,
                &format!("{t}, pyramiding {p}"),
                h,
                shape_body(*sh, false),
                kind_of(*sh),
            ));
        }
    }
    for p in 1u8..=3 {
        let mut h = Hdr::base();
        h.pyramiding = p;
        o.push(row(
            40 + u16::from(p),
            &format!("long/short reversal, pyramiding {p}"),
            h,
            reverse_body(),
            Some(Kind::Reverse),
        ));
    }
    let mut h = Hdr::base();
    h.pyramiding = 2;
    h.extra = vec!["close_entries_rule = \"ANY\""];
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(
        &and(ON_LO, LONG),
        &["strategy.entry(\"L2\", strategy.long)"],
    ));
    b.extend(when(
        &and(ON_HI, "strategy.opentrades == 2"),
        &["strategy.close(\"L2\")"],
    ));
    b.extend(when(
        &and(ON_HI, "strategy.opentrades == 1"),
        &["strategy.close(\"L1\")"],
    ));
    o.push(row(44, "close_entries_rule ANY", h, b, None));
    // E-siz (E045–E056).
    let siz = [
        ("strategy.percent_of_equity", "10"),
        ("strategy.cash", "100000"),
    ];
    let siz_shapes = [
        (Shape::EntryMarket, "entry market"),
        (Shape::EntryLimit, "entry limit"),
        (Shape::Close, "close"),
    ];
    for (ti, &(qt, qv)) in siz.iter().enumerate() {
        for (si, (sh, t)) in siz_shapes.iter().enumerate() {
            let mut h = Hdr::base();
            h.qty_type = Some(qt);
            h.qty_value = qv;
            o.push(shaped(
                (45 + 3 * ti + si) as u16,
                &format!("{t}, default_qty_type {}", &qt["strategy.".len()..]),
                h,
                *sh,
            ));
        }
    }
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long, qty = 3)"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    o.push(row(51, "call-site qty", Hdr::base(), b, Some(Kind::Cycle)));
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(
        LONG,
        &["strategy.exit(\"X1\", \"L1\", qty_percent = 50, profit = 2000)"],
    ));
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    o.push(row(52, "qty_percent on an exit", Hdr::base(), b, None));
    let mut h = Hdr::base();
    h.qty_value = "5";
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(
        &and(ON_HI, LONG),
        &["strategy.close(\"L1\", qty = 2)"],
    ));
    o.push(row(53, "default_qty_value interplay", h, b, None));
    let mut h = Hdr::base();
    h.qty_type = Some("strategy.cash");
    let mut r = shaped(54, "a size rounding to zero", h, Shape::EntryMarket);
    r.pins.push("pin:05-qty-rounding");
    o.push(r);
    let mut h = Hdr::base();
    h.qty_type = Some("strategy.percent_of_equity");
    h.qty_value = "150";
    h.margin = Some(("100", "100"));
    let mut r = shaped(
        55,
        "a size above equity, margin_long 100",
        h,
        Shape::EntryMarket,
    );
    r.pins.push("pin:05-margin-funds-check");
    o.push(r);
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long, qty = -1)"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    let mut r = row(56, "a negative qty", Hdr::base(), b, None);
    r.pins.push("pin:05-bad-qty");
    o.push(r);
    o
}

/// E057–E121: E-oca … E-fill (T11).
fn part2() -> Vec<Row> {
    let mut o = Vec::new();
    // E-oca (E057–E062): E(57 + 2·type + shape).
    for (ti, t) in ["none", "cancel", "reduce"].iter().enumerate() {
        let oca = format!("oca_name = \"G\", oca_type = strategy.oca.{t}");
        let mut h = Hdr::base();
        if ti == 2 {
            h.comm_type = "strategy.commission.cash_per_order";
            h.comm_value = "1";
        }
        let mut b = when(
            &and(ON_LO, FLAT),
            &[
                &format!("strategy.entry(\"LA\", strategy.long, stop = {ABOVE}, {oca})"),
                &format!("strategy.entry(\"SB\", strategy.short, stop = {BELOW}, {oca})"),
            ],
        );
        b.extend(when(
            &and(ON_HI, "strategy.position_size != 0"),
            &["strategy.close_all()"],
        ));
        o.push(row(
            (57 + 2 * ti) as u16,
            &format!("oca {t}, two entries"),
            h.clone(),
            b,
            None,
        ));
        let mut b = when(
            &and(ON_LO, FLAT),
            &["strategy.entry(\"L1\", strategy.long)"],
        );
        b.extend(when(
            LONG,
            &[
                &format!("strategy.order(\"A1\", strategy.long, 1, limit = {BELOW}, {oca})"),
                &format!("strategy.order(\"X1\", strategy.short, 1, limit = {ABOVE}, {oca})"),
            ],
        ));
        b.extend(when(
            &and(ON_HI, "strategy.position_size != 0"),
            &["strategy.close_all()"],
        ));
        o.push(row(
            (58 + 2 * ti) as u16,
            &format!("oca {t}, entry + exit"),
            h,
            b,
            None,
        ));
    }
    // E-costs (E063–E068): E(63 + 2·type + slippage).
    let costs = [
        ("strategy.commission.percent", "0.1", "percent"),
        (
            "strategy.commission.cash_per_contract",
            "2",
            "cash_per_contract",
        ),
        ("strategy.commission.cash_per_order", "2", "cash_per_order"),
    ];
    for (ci, &(ct, cv, t)) in costs.iter().enumerate() {
        for (si, &sl) in ["0", "5"].iter().enumerate() {
            let mut h = Hdr::base();
            h.comm_type = ct;
            h.comm_value = cv;
            h.slippage = sl;
            o.push(shaped(
                (63 + 2 * ci + si) as u16,
                &format!("commission {t}, slippage {sl}"),
                h,
                Shape::EntryMarket,
            ));
        }
    }
    // E-margin (E069–E072).
    for (n, ds, twin) in [(69u16, Dialects::V5, 70u16), (70, Dialects::V6, 69)] {
        let mut h = Hdr::base();
        h.margin = None;
        h.qty_type = Some("strategy.percent_of_equity");
        h.qty_value = "250";
        let mut r = shaped(n, "no margin arguments", h, Shape::EntryMarket);
        r.ds = ds;
        r.twin = twin;
        r.pins.push("pin:W18-margin-default");
        o.push(r);
    }
    for (n, m, sh, t) in [
        (71u16, ("50", "0"), Shape::EntryMarket, "margin_long 50"),
        (72, ("0", "50"), Shape::Close, "margin_short 50"),
    ] {
        let mut h = Hdr::base();
        h.margin = Some(m);
        h.qty_type = Some("strategy.percent_of_equity");
        h.qty_value = "195";
        let mut r = shaped(n, t, h, sh);
        r.pins
            .extend(["pin:05-margin-funds-check", "pin:U09-margin-call-size"]);
        o.push(r);
    }
    // E-path (E073–E080).
    let cls = (
        "open - low > high - open ? 1 : open - low < high - open ? -1 : 0".to_string(),
        "cls".to_string(),
    );
    for (n, t, long, k) in [
        (73u16, "open nearer the high", true, "0.003"),
        (74, "open nearer the low", false, "0.003"),
        (75, "the tie", true, "0.001"),
    ] {
        let (id, dir, cond, lim, stp) = if long {
            ("L1", "strategy.long", LONG, "1 +", "1 -")
        } else {
            ("S1", "strategy.short", SHORT, "1 -", "1 +")
        };
        let mut b = when(
            &and(ON_LO, FLAT),
            &[&format!("strategy.entry(\"{id}\", {dir})")],
        );
        b.extend(when(
            cond,
            &[&format!(
                "strategy.exit(\"X1\", \"{id}\", limit = math.round_to_mintick(strategy.position_avg_price * ({lim} {k})), stop = math.round_to_mintick(strategy.position_avg_price * ({stp} {k})))"
            )],
        ));
        let mut r = row(n, t, Hdr::base(), b, None);
        r.plots.push(cls.clone());
        o.push(r);
    }
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long, limit = math.round_to_mintick(close * 0.99))"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    o.push(row(76, "a limit crossed in a gap", Hdr::base(), b, None));
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long, stop = math.round_to_mintick(close * 1.01))"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    o.push(row(77, "a stop crossed in a gap", Hdr::base(), b, None));
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(
        LONG,
        &["strategy.exit(\"X1\", \"L1\", trail_points = 200, trail_offset = 100)"],
    ));
    o.push(row(
        78,
        "a trail that arms and fires in one segment",
        Hdr::base(),
        b,
        None,
    ));
    let mut h = Hdr::base();
    h.slippage = "50000";
    o.push(shaped(
        79,
        "slippage past the bar's range",
        h,
        Shape::EntryMarket,
    ));
    let mut h = Hdr::base();
    h.on_close = true;
    o.push(shaped(
        80,
        "process_orders_on_close with a limit",
        h,
        Shape::EntryLimit,
    ));
    // E-state (E081–E086).
    for (n, e, k) in [
        (81u16, "", ""),
        (82, "strategy.position_avg_price", "avg"),
        (83, "strategy.opentrades", "open"),
        (84, "strategy.closedtrades", "closed"),
        (85, "strategy.equity", "eq"),
        (86, "strategy.netprofit", "net"),
    ] {
        let t = if e.is_empty() {
            "strategy.position_size".to_string()
        } else {
            e.to_string()
        };
        let mut r = shaped(
            n,
            &format!("{t} read back"),
            Hdr::base(),
            Shape::EntryMarket,
        );
        if !e.is_empty() {
            r.plots.push((e.to_string(), k.to_string()));
        }
        o.push(r);
    }
    // E-hdr (E087–E090).
    let mut h = Hdr::base();
    h.capital = "50000";
    o.push(shaped(87, "initial_capital", h, Shape::EntryMarket));
    let mut h = Hdr::base();
    h.extra = vec!["currency = currency.NONE"];
    o.push(shaped(88, "currency NONE", h, Shape::EntryMarket));
    let mut h = Hdr::base();
    h.qty_type = None;
    o.push(shaped(89, "header default_qty_type", h, Shape::EntryMarket));
    let mut h = Hdr::base();
    h.extra = vec!["fill_orders_on_standard_ohlc = true"];
    o.push(shaped(
        90,
        "fill_orders_on_standard_ohlc",
        h,
        Shape::EntryMarket,
    ));
    // E-risk (E091–E096).
    for (n, call, t) in [
        (
            91u16,
            "strategy.risk.max_drawdown(2000, strategy.cash)",
            "max_drawdown",
        ),
        (
            92,
            "strategy.risk.max_intraday_loss(500, strategy.cash)",
            "max_intraday_loss",
        ),
        (
            93,
            "strategy.risk.max_intraday_filled_orders(2)",
            "max_intraday_filled_orders",
        ),
        (
            94,
            "strategy.risk.max_cons_loss_days(2)",
            "max_cons_loss_days",
        ),
        (
            95,
            "strategy.risk.allow_entry_in(strategy.direction.long)",
            "allow_entry_in",
        ),
        (
            96,
            "strategy.risk.max_position_size(2)",
            "max_position_size",
        ),
    ] {
        let mut h = Hdr::base();
        let mut b = vec![call.to_string()];
        let kind = match n {
            95 => {
                b.extend(reverse_body());
                Some(Kind::Reverse)
            }
            96 => {
                h.pyramiding = 3;
                b.extend(shape_body(Shape::EntryMarket, false));
                None
            }
            _ => {
                b.extend(shape_body(Shape::EntryMarket, true));
                None
            }
        };
        let mut r = row(n, &format!("strategy.risk.{t}"), h, b, kind);
        r.pins.push("pin:U10-risk-rules");
        o.push(r);
    }
    // E-imm (E097, E098).
    for (n, act, t) in [
        (
            97u16,
            "strategy.close(\"L1\", immediately = true)",
            "strategy.close immediately",
        ),
        (
            98,
            "strategy.close_all(immediately = true)",
            "strategy.close_all immediately",
        ),
    ] {
        let mut b = when(
            &and(ON_LO, FLAT),
            &["strategy.entry(\"L1\", strategy.long)"],
        );
        b.extend(when(&and(ON_HI, LONG), &[act]));
        let mut r = row(n, t, Hdr::base(), b, Some(Kind::Cycle));
        r.pins.push("pin:U11-close-immediately");
        o.push(r);
    }
    // E-w23 (E099–E104): v5 / v6 twins.
    let legs = [
        (
            "limit = math.round_to_mintick(strategy.position_avg_price * 1.01), profit = 1000",
            "limit + profit",
        ),
        (
            "stop = math.round_to_mintick(strategy.position_avg_price * 0.99), loss = 1000",
            "stop + loss",
        ),
        (
            "trail_price = math.round_to_mintick(strategy.position_avg_price * 1.01), trail_points = 1000, trail_offset = 200",
            "trail_price + trail_points",
        ),
    ];
    for (li, (args, t)) in legs.iter().enumerate() {
        for (di, ds) in [Dialects::V5, Dialects::V6].iter().enumerate() {
            let n = (99 + 2 * li + di) as u16;
            let mut b = when(
                &and(ON_LO, FLAT),
                &["strategy.entry(\"L1\", strategy.long)"],
            );
            b.extend(when(
                LONG,
                &[&format!("strategy.exit(\"X1\", \"L1\", {args})")],
            ));
            let mut r = row(n, &format!("exit {t}"), Hdr::base(), b, None);
            r.ds = *ds;
            r.twin = if di == 0 { n + 1 } else { n - 1 };
            r.pins.push("pin:W23-exit-legs");
            o.push(r);
        }
    }
    // E-w21 (E105, E106).
    let ctr = [
        "varip int k@ = 0",
        "varip int tl@ = 0",
        "if time != tl@",
        ">tl@ := time",
        ">k@ := 0",
        "k@ := k@ + 1",
    ];
    let mut b = s(&ctr);
    b.extend(when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    ));
    b.extend(when(
        "k@ == 1 and strategy.position_size > 0 and nz(strategy.position_size[1]) == 0",
        &["strategy.order(\"W\", strategy.long, 1)"],
    ));
    b.extend(when(&and(ON_HI, LONG), &["strategy.close_all()"]));
    let mut h = Hdr::base();
    h.on_fills = true;
    let mut r = row(105, "an order placed in a fill re-execution", h, b, None);
    r.run.mode = ExecMode::Realtime;
    r.pins.push("pin:W21-intrabar-commit");
    o.push(r);
    let mut b = s(&ctr);
    b.extend(when(
        "barstate.isrealtime and k@ == 1 and strategy.position_size == 0",
        &["strategy.entry(\"L1\", strategy.long)"],
    ));
    b.extend(when(
        "barstate.isrealtime and barstate.isconfirmed and strategy.position_size > 0",
        &["strategy.close(\"L1\")"],
    ));
    let mut h = Hdr::base();
    h.every_tick = true;
    let mut r = row(106, "an entry on a realtime bar's first update", h, b, None);
    // No `_last` block and no SYN-T: the drive is SynTape (§4.1.6).
    r.body.truncate(r.body.len() - 3);
    r.prelude = false;
    r.oracle = Oracle::TvOnly;
    r.window = Window::Tail(RT_DEFAULT.realtime);
    r.run = Run {
        driver: Driver::Rt,
        mode: ExecMode::Realtime,
        rt: Some(RT_DEFAULT),
        ..Run::default()
    };
    r.pins.push("pin:W21-intrabar-commit");
    o.push(r);
    // E-trade (E107, E108).
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long)"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    b.extend(when(
        "_last",
        &[
            "strategy.cancel_all()",
            "strategy.entry(\"LZ\", strategy.long)",
        ],
    ));
    let mut r = row(107, "one trade left open", Hdr::base(), b, None);
    r.body.truncate(r.body.len() - 3);
    r.plots.extend([
        (
            "strategy.opentrades > 0 ? strategy.opentrades.entry_price(0) : na".to_string(),
            "ep".to_string(),
        ),
        (
            "strategy.opentrades > 0 ? strategy.opentrades.size(0) : na".to_string(),
            "sz".to_string(),
        ),
    ]);
    o.push(r);
    let mut r = shaped(
        108,
        "the last closed trade's accessors",
        Hdr::base(),
        Shape::EntryMarket,
    );
    let lc = "strategy.closedtrades - 1";
    for (f, k) in [
        ("profit", "profit"),
        ("commission", "comm"),
        ("entry_price", "ep"),
    ] {
        r.plots.push((
            format!("strategy.closedtrades > 0 ? strategy.closedtrades.{f}({lc}) : na"),
            k.to_string(),
        ));
    }
    o.push(r);
    // E-limit (E109, E110).
    for (n, ds) in [(109u16, Dialects::V6), (110, Dialects::V5)] {
        let b = s(&[
            "var bool t@ = false",
            "t@ := not t@",
            "if t@",
            ">strategy.entry(\"L\", strategy.long)",
            "else",
            ">strategy.close(\"L\")",
        ]);
        let mut r = row(n, "two orders per trade, every step", Hdr::base(), b, None);
        r.body.truncate(r.body.len() - 3);
        r.prelude = false;
        r.ds = ds;
        r.window = Window::Tail(64);
        r.run = Run::default();
        r.plots = vec![("strategy.closedtrades".to_string(), "closed".to_string())];
        if n == 109 {
            r.pins.push("pin:05-trade-trim");
            r.plots.extend([
                (
                    "strategy.closedtrades.first_index".to_string(),
                    "first".to_string(),
                ),
                (
                    "na(strategy.closedtrades.entry_bar_index(0)) ? 1 : 0".to_string(),
                    "isna0".to_string(),
                ),
            ]);
        } else {
            r.pins.push("pin:05-v5-order-limit");
            r.oracle = Oracle::TvOnly;
        }
        o.push(r);
    }
    // E-pin5 (E111–E117).
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L\", strategy.long, qty = 20)"],
    );
    b.extend(when(
        LONG,
        &[
            "strategy.exit(\"TP\", \"L\", qty = 19, profit = 20000)",
            "strategy.exit(\"SL\", \"L\", qty = 20, loss = 1000)",
        ],
    ));
    let mut r = row(111, "a reserved partial exit", Hdr::base(), b, None);
    r.pins.push("pin:05-exit-reservation");
    o.push(r);
    let mut h = Hdr::base();
    h.pyramiding = 2;
    h.qty_value = "2";
    let mut b = when(
        &and(ON_LO, "strategy.opentrades < 2"),
        &["strategy.entry(\"L\", strategy.long)"],
    );
    b.extend(when(
        LONG,
        &["strategy.exit(\"X\", \"L\", qty = 1, profit = 2000, loss = 2000)"],
    ));
    let mut r = row(112, "an exit qty over two trades", h, b, None);
    r.pins.push("pin:05-exit-qty-per-trade");
    o.push(r);
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L\", strategy.long, limit = close - 12.4 * syminfo.mintick)"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L\")"]));
    let mut r = row(113, "a limit 0.4 tick off the grid", Hdr::base(), b, None);
    r.pins.push("pin:05-price-rounding");
    o.push(r);
    let mut h = Hdr::base();
    h.comm_value = "0.1";
    let mut r = shaped(
        114,
        "openprofit net of the entry fee",
        h,
        Shape::EntryMarket,
    );
    r.plots.extend([
        ("strategy.openprofit".to_string(), "oprofit".to_string()),
        ("strategy.equity".to_string(), "eq".to_string()),
    ]);
    r.pins.push("pin:05-openprofit-fees");
    o.push(r);
    let mut h = Hdr::base();
    h.pyramiding = 3;
    let mut b = s(&["var int at@ = -2"]);
    b.extend(when(
        ON_LO,
        &[
            "at@ := bar_index",
            "strategy.entry(\"B\", strategy.long, limit = math.round_to_mintick(close * 0.995))",
        ],
    ));
    b.extend(when(
        "bar_index == at@ + 1",
        &["strategy.entry(\"B\", strategy.long, limit = math.round_to_mintick(close * 0.99))"],
    ));
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"B\")"]));
    let mut r = row(115, "a same-id limit entry re-placed", h, b, None);
    r.pins.push("pin:05-same-id-entry");
    o.push(r);
    let mut b = s(&["var bool done@ = false"]);
    b.extend(when(
        &and(ON_LO, FLAT),
        &[
            "strategy.entry(\"L\", strategy.long)",
            "if not done@",
            ">strategy.exit(\"X\", profit = 2000, loss = 2000)",
            ">done@ := true",
        ],
    ));
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L\")"]));
    let mut r = row(116, "an exit issued once while flat", Hdr::base(), b, None);
    r.pins.push("pin:05-exit-persist-flat");
    o.push(r);
    let mut b = when(
        &and(ON_LO, FLAT),
        &[
            "strategy.entry(\"L\", strategy.long)",
            "strategy.order(\"P\", strategy.long, 1, limit = math.round_to_mintick(close * 0.95))",
        ],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close_all()"]));
    let mut r = row(117, "close_all with a pending entry", Hdr::base(), b, None);
    r.pins.push("pin:05-close-all-keeps-pending");
    o.push(r);
    // E-edge (E118).
    let mut r = shaped(
        118,
        "strategy.* summary built-ins",
        Hdr::base(),
        Shape::EntryMarket,
    );
    for (e, k) in [
        ("netprofit_percent", "npp"),
        ("max_drawdown_percent", "mddp"),
        ("avg_trade", "avg"),
        ("avg_winning_trade", "avgw"),
        ("avg_losing_trade", "avgl"),
    ] {
        r.plots.push((format!("strategy.{e}"), k.to_string()));
    }
    r.pins.push("pin:04-strategy-edges");
    o.push(r);
    // E-fx (E119).
    let mut h = Hdr::base();
    h.extra = vec!["currency = currency.EUR"];
    // `pin:06-e119-plot` — §4.4.1's grammar wants ≥ 1 `plot` line: E119 keeps E001's `pos` plot,
    // never compared (the load refuses first).
    let mut r = shaped(119, "a non-chart currency", h, Shape::EntryMarket);
    r.oracle = Oracle::Declined;
    r.expect = Expect::Refuse {
        code: CURRENCY_CONVERSION,
    };
    o.push(r);
    // E-fill (E120, E121).
    let mut h = Hdr::base();
    h.extra = vec!["backtest_fill_limits_assumption = 3"];
    let mut b = when(
        &and(ON_LO, FLAT),
        &["strategy.entry(\"L1\", strategy.long, limit = close + 50 * syminfo.mintick)"],
    );
    b.extend(when(&and(ON_HI, LONG), &["strategy.close(\"L1\")"]));
    let mut r = row(120, "a buy limit through the market", h.clone(), b, None);
    r.pins.push("pin:05-fill-limits");
    o.push(r);
    let mut b = when(
        &and(ON_LO, FLAT),
        &[&format!(
            "strategy.entry(\"L1\", strategy.long, limit = {BELOW})"
        )],
    );
    b.extend(when(
        &and("_on and syni >= 20 and syni < 40", FLAT),
        &[&format!(
            "strategy.entry(\"L2\", strategy.long, stop = {ABOVE}, limit = {ABOVE2})"
        )],
    ));
    b.extend(when(
        LONG,
        &["strategy.exit(\"TP\", limit = math.round_to_mintick(strategy.position_avg_price * 1.01))"],
    ));
    b.extend(when(
        &and(ON_HI, LONG),
        &[&format!(
            "strategy.order(\"O\", strategy.short, strategy.position_size, limit = {ABOVE})"
        )],
    ));
    let mut r = row(121, "four price legs at the verification level", h, b, None);
    r.pins.push("pin:05-fill-limits");
    o.push(r);
    o
}

fn line(t: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    let depth = t.bytes().take_while(|&c| c == b'>').count();
    Line {
        depth: depth as u8,
        text: t[depth..].replace('@', &sfx),
    }
}

fn block(n: u16) -> &'static str {
    match n {
        1..=14 => "E-ord",
        15..=32 => "E-tim",
        33..=44 => "E-pyr",
        45..=56 => "E-siz",
        57..=62 => "E-oca",
        63..=68 => "E-costs",
        69..=72 => "E-margin",
        73..=80 => "E-path",
        81..=86 => "E-state",
        87..=90 => "E-hdr",
        91..=96 => "E-risk",
        97..=98 => "E-imm",
        99..=104 => "E-w23",
        105..=106 => "E-w21",
        107..=108 => "E-trade",
        109..=110 => "E-limit",
        111..=117 => "E-pin5",
        118 => "E-edge",
        119 => "E-fx",
        _ => "E-fill",
    }
}

fn cell(r: Row) -> Cell {
    let id = FixtureId::from_parts(Group::E, r.n);
    let sfx = format!("_{}", id.as_str());
    let tols = r.plots.iter().map(|_| Tol::Exact).collect();
    let plots = r
        .plots
        .into_iter()
        .map(|(e, k)| (e.replace('@', &sfx), k))
        .collect();
    let pins: Vec<String> = r.pins.iter().map(|p| p.to_string()).collect();
    let twin = (r.twin != 0).then(|| FixtureId::from_parts(Group::E, r.twin));
    let script = Script {
        id,
        dialects: r.ds,
        about: vec![r.title.clone(), block(r.n).to_string()],
        twin,
        pins: pins.clone(),
        decl: Some(r.hdr.render(id)),
        prelude: if r.prelude {
            Prelude::SynT {
                from_ms: spec::FROM_MS,
                stop_ms: STOP_MS,
            }
        } else {
            Prelude::None
        },
        body: r.body.iter().map(|t| line(t, id)).collect(),
        plots,
    };
    Cell {
        script,
        title: r.title,
        pins,
        oracle: r.oracle,
        tier: 1,
        window: r.window,
        tols,
        expect: r.expect,
        readings: Vec::new(),
        run: r.run,
    }
}

/// §4.9.6/§4.9.7's fixture assignments the blocks above do not already carry: impl-05's pins that
/// name whole blocks, and `pin:04-strategy-edges` (E-state, E-trade; E118 carries it already).
fn table_pins(n: u16) -> Vec<&'static str> {
    let mut o = Vec::new();
    match n {
        33..=40 => o.push("pin:05-pyramiding-check"),
        45 => o.push("pin:05-pct-equity-time"),
        61 | 62 => o.push("pin:05-cash-per-order"),
        75 => o.push("pin:05-path-tie"),
        78 => o.push("pin:05-trail-same-segment"),
        16 | 19 | 22 | 25 | 28 | 31 | 105 => {
            o.extend(["pin:05-cof-close-tick", "pin:05-cof-next-tick"]);
        }
        _ => {}
    }
    if (73..=86).contains(&n) {
        o.push("pin:05-drawdown-sampling");
    }
    if (81..=86).contains(&n) || n == 107 || n == 108 {
        o.push("pin:04-strategy-edges");
    }
    o
}

fn rows() -> Vec<Row> {
    let mut rs = part1();
    rs.extend(part2());
    rs.sort_by_key(|r| r.n);
    for r in &mut rs {
        r.pins.extend(table_pins(r.n));
    }
    rs
}

/// Every group-E cell (E001–E121).
pub fn cells() -> Vec<Cell> {
    rows().into_iter().map(cell).collect()
}

/// SYN-T's advancing steps over the window: `(STOP_MS − FROM_MS) / TF_MS`.
pub const WINDOW_STEPS: usize = ((STOP_MS - spec::FROM_MS) / spec::TF_MS) as usize;

/// Cycles a market shape completes over the window, counted with the `SynSeq` twin: a signal on
/// step `i` fills at step `i + 1`'s open, before that step's evaluation.
pub fn count_cycles(k: Kind) -> u32 {
    let mut q = SynSeq::new();
    let mut pos = 0i8;
    let mut n = 0u32;
    for _ in 0..WINDOW_STEPS {
        let s = q.step().syni;
        match k {
            Kind::Cycle => {
                if pos == 0 && s < ENTER_BELOW {
                    pos = 1;
                } else if pos == 1 && s > EXIT_ABOVE {
                    pos = 0;
                    n += 1;
                }
            }
            Kind::Reverse => {
                if s < REV_BELOW && pos <= 0 {
                    n += u32::from(pos < 0);
                    pos = 1;
                } else if s > REV_ABOVE && pos >= 0 {
                    n += u32::from(pos > 0);
                    pos = -1;
                }
            }
        }
    }
    n
}

/// Every market shape's id with its cycle count (`group_e_market_cycles_in_range`).
pub fn market_cycles() -> Vec<(FixtureId, u32)> {
    rows()
        .iter()
        .filter_map(|r| {
            r.kind
                .map(|k| (FixtureId::from_parts(Group::E, r.n), count_cycles(k)))
        })
        .collect()
}
