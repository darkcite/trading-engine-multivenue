// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `TradeRec` and `RunRecord` against TradingView's List of Trades and Performance Summary
//! (impl-06 §3.7, §4.4.3–§4.4.4) — impl-05's conformance gate. Compare-time code: allocates only to
//! build a divergence's text.

use crate::dec::{money_ok, scaled_half_away, Dec};
use crate::fixture::{Summary, TradeRow};
use pine_ir::{trade_flag, RunRecord, StrKey, Sym, SymTable, TradeRec};

/// How the List of Trades prints a trade's P&L (probe-03, §4.7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnlBasis {
    /// Net of commission (`pnl_1e12`).
    Net,
    /// Before commission (`pnl_1e12 + fee_1e12`).
    Gross,
}

/// Exit signal texts TradingView prints for exits that carry no user id (`capture/signals.tsv`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignalTable {
    /// A liquidation (`trade_flag::LIQUIDATION`).
    pub liquidation: String,
    /// A risk-rule flatten (`trade_flag::RISK_FLATTEN`).
    pub risk_flatten: String,
    /// `strategy.close`.
    pub close: String,
    /// `strategy.close_all`.
    pub close_all: String,
}

impl SignalTable {
    /// The `cause<TAB>text` lines of `signals.tsv`, keys `liquidation`, `risk_flatten`, `close`,
    /// `close_all`.
    ///
    /// `pin:06-signals-format` — §4.7.3 names the four causes, not the file layout: one
    /// `cause TAB text` line each, LF-terminated, any order, each exactly once.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut t = SignalTable::default();
        let mut seen = [false; 4];
        for (i, l) in text.lines().enumerate() {
            let Some((k, v)) = l.split_once('\t') else {
                return Err(format!("line {}: want `cause<TAB>text`", i + 1));
            };
            let (slot, ix) = match k {
                "liquidation" => (&mut t.liquidation, 0),
                "risk_flatten" => (&mut t.risk_flatten, 1),
                "close" => (&mut t.close, 2),
                "close_all" => (&mut t.close_all, 3),
                _ => return Err(format!("line {}: unknown cause `{k}`", i + 1)),
            };
            if seen[ix] {
                return Err(format!("line {}: `{k}` twice", i + 1));
            }
            seen[ix] = true;
            *slot = v.to_string();
        }
        if seen.iter().all(|s| *s) {
            Ok(t)
        } else {
            Err("signals.tsv names all four causes".into())
        }
    }
}

/// Where the lists part.
#[derive(Debug, Clone, PartialEq)]
pub enum TradeDiv {
    /// Compared first; suppresses the per-field diff.
    Count {
        /// Rows in the export.
        expected: usize,
        /// Our `TradeRec`s.
        actual: usize,
        /// The first index where the lists part.
        first_mismatch: usize,
    },
    /// One field of trade `n` (or of the summary, `n = 0`).
    Field {
        /// The export's trade number.
        n: u32,
        /// The field.
        field: &'static str,
        /// The export's text.
        expected: String,
        /// Ours.
        actual: String,
    },
}

fn field(n: u32, field: &'static str, expected: String, actual: String) -> Option<TradeDiv> {
    Some(TradeDiv::Field {
        n,
        field,
        expected,
        actual,
    })
}

/// Report-domain ×1e6 → computation-domain ×1e12 (exact: × 10⁶).
const fn e6_to_e12(v: i64) -> i128 {
    v as i128 * 1_000_000
}

/// `v / 10^scale` printed as TradingView prints money, prices and sizes: sign, integer digits, the
/// fraction without trailing zeros (`-1853700000` at scale 6 → `-1853.7`). Divergence text only.
///
/// `pin:06-trade-money-units` (CI-033 / CI-034, harness part) — a trade field's divergence shows
/// ours in TradingView's units and sign, never the raw ×1e6 / ×1e12 integer.
pub fn render_scaled(v: i128, scale: u32) -> String {
    let p = 10u128.pow(scale);
    let m = v.unsigned_abs();
    let (int, frac) = (m / p, m % p);
    let sign = if v < 0 { "-" } else { "" };
    if frac == 0 {
        return format!("{sign}{int}");
    }
    let f = format!("{frac:0w$}", w = scale as usize);
    format!("{sign}{int}.{}", f.trim_end_matches('0'))
}

/// `pin:06-trade-adverse-sign` (CI-033, harness part) — TradingView's List of Trades prints the
/// adverse excursion (`drawdown`) as a signed money value ≤ 0 (`-1853.7`, E005), the favorable one
/// (`runup`) ≥ 0; `TradeRec::max_drawdown_1e6` is the unsigned magnitude. The comparison negates
/// ours, so a drawdown compares in TradingView's sign; the value laws stay the broker's.
const fn adverse_signed(max_drawdown_1e6: i64) -> i64 {
    -max_drawdown_1e6
}

fn sym_text(syms: &SymTable, s: Sym) -> Option<&str> {
    if s == Sym::NONE {
        None
    } else {
        Some(syms.resolve(s))
    }
}

fn entry_sig_ok(e: &TradeRow, a: &TradeRec, syms: &SymTable) -> bool {
    match sym_text(syms, a.entry_sym) {
        Some(t) => t == e.entry_sig,
        None => pine_ir::str_key(e.entry_sig.as_bytes()) == a.entry_id,
    }
}

/// The exit-order names the broker and pine-std key for TradingView (`pin:05-exit-signal-text`,
/// impl-05 §4.6.2): `strategy.close(id)`'s prefix, then the ids the broker gives the exits it
/// generates. Divergence text only — the comparison is by key.
const CLOSE_ENTRY_PREFIX: &str = "Close entry(s) order ";
const BROKER_EXIT_NAMES: [&str; 5] = [
    "Close position order",
    "Margin call",
    "Close Position (Max intraday Loss)",
    "Close Position (Max consecutive loss days)",
    "Close Position (Max number of filled orders in one day)",
];

/// `pin:05-exit-signal-text` (CI-032, runner part) — the List of Trades' exit Signal is the exit
/// ORDER's id, and `exit_id` is its key: a key decides. `exit_sym` is the script's constant id
/// (display): a `strategy.close(id)` trade keys `Close entry(s) order <id>` while its `exit_sym`
/// stays `<id>` (pine-std `bi_close`, `pin:04-close-exit-id-read`), so a bare `<id>` never matches.
///
/// `pin:06-exit-cause-close` — `TradeRec` records no flag telling `strategy.close` from
/// `strategy.close_all`: an id-less exit that is neither a liquidation nor a risk flatten matches
/// either text.
fn exit_sig_ok(sig: &str, a: &TradeRec, syms: &SymTable, signals: &SignalTable) -> bool {
    if a.exit_id != StrKey::NONE && pine_ir::str_key(sig.as_bytes()) == a.exit_id {
        return true;
    }
    if let Some(t) = sym_text(syms, a.exit_sym) {
        return a.exit_id == StrKey::NONE && t == sig;
    }
    if a.flags & trade_flag::LIQUIDATION != 0 {
        return sig == signals.liquidation;
    }
    if a.flags & trade_flag::RISK_FLATTEN != 0 {
        return sig == signals.risk_flatten;
    }
    sig == signals.close || sig == signals.close_all
}

/// Our exit Signal as TradingView would print it, for a divergence's text: the text whose key is
/// `exit_id` among `exit_sym`'s, `Close entry(s) order <exit_sym>` and the broker's names; else
/// `exit_sym`'s text marked with the key, or `<cause>` for an id-less exit.
fn our_exit_sig(a: &TradeRec, syms: &SymTable) -> String {
    let key = |t: &str| pine_ir::str_key(t.as_bytes());
    let t = sym_text(syms, a.exit_sym);
    if a.exit_id == StrKey::NONE {
        return t.unwrap_or("<cause>").to_string();
    }
    if let Some(t) = t {
        if key(t) == a.exit_id {
            return t.to_string();
        }
        let c = format!("{CLOSE_ENTRY_PREFIX}{t}");
        if key(&c) == a.exit_id {
            return c;
        }
    }
    for n in BROKER_EXIT_NAMES {
        if key(n) == a.exit_id {
            return n.to_string();
        }
    }
    format!("{} (key {:#018x})", t.unwrap_or("<id>"), a.exit_id.0)
}

fn pnl_on(a: &TradeRec, basis: PnlBasis) -> i128 {
    match basis {
        PnlBasis::Net => a.pnl_1e12,
        PnlBasis::Gross => a.pnl_1e12 + a.fee_1e12,
    }
}

/// One trade; `cum` is the running closed P&L on `basis` through this trade.
fn compare_one(
    e: &TradeRow,
    a: &TradeRec,
    cum: i128,
    syms: &SymTable,
    signals: &SignalTable,
    basis: PnlBasis,
) -> Option<TradeDiv> {
    let n = e.n;
    if a.flags & trade_flag::PNL_NET == 0 {
        return field(
            n,
            "pnl_net_flag",
            "PNL_NET".into(),
            format!("{:#x}", a.flags),
        );
    }
    if e.long != (a.qty_1e6 > 0) {
        return field(n, "side", e.long.to_string(), (a.qty_1e6 > 0).to_string());
    }
    if e.entry_ms != a.entry_ms {
        return field(
            n,
            "entry_ms",
            e.entry_ms.to_string(),
            a.entry_ms.to_string(),
        );
    }
    let open = a.flags & trade_flag::OPEN != 0;
    if e.exit.is_none() != open {
        return field(n, "open", e.exit.is_none().to_string(), open.to_string());
    }
    if !entry_sig_ok(e, a, syms) {
        let ours = sym_text(syms, a.entry_sym).unwrap_or("<id>").to_string();
        return field(n, "entry_sig", e.entry_sig.clone(), ours);
    }
    let px = |v: i64| render_scaled(i128::from(v), 6);
    if scaled_half_away(e.entry_px, 6) != i128::from(a.entry_px_1e6) {
        return field(n, "entry_px", e.entry_px.render(), px(a.entry_px_1e6));
    }
    if scaled_half_away(e.qty, 6) != i128::from(a.qty_1e6.unsigned_abs()) {
        let q = render_scaled(i128::from(a.qty_1e6.unsigned_abs()), 6);
        return field(n, "qty", e.qty.render(), q);
    }
    if let Some(x) = &e.exit {
        if x.ms != a.exit_ms {
            return field(n, "exit_ms", x.ms.to_string(), a.exit_ms.to_string());
        }
        if !exit_sig_ok(&x.sig, a, syms, signals) {
            return field(n, "exit_sig", x.sig.clone(), our_exit_sig(a, syms));
        }
        if scaled_half_away(x.px, 6) != i128::from(a.exit_px_1e6) {
            return field(n, "exit_px", x.px.render(), px(a.exit_px_1e6));
        }
        if let Some(b) = e.bars {
            let ours = a.exit_bar.wrapping_sub(a.entry_bar);
            if b != ours {
                return field(n, "bars", b.to_string(), ours.to_string());
            }
        }
        // An open trade's P&L is marked at export time: compared on closed trades only.
        if let Some(p) = e.pnl {
            if !money_ok(pnl_on(a, basis), p) {
                return field(n, "pnl", p.render(), render_scaled(pnl_on(a, basis), 12));
            }
        }
        if let Some(c) = e.cum_pnl {
            if !money_ok(cum, c) {
                return field(n, "cum_pnl", c.render(), render_scaled(cum, 12));
            }
        }
    }
    let money = |name: &'static str, d: Option<Dec>, ours_1e6: i64| -> Option<TradeDiv> {
        let d = d?;
        if money_ok(e6_to_e12(ours_1e6), d) {
            None
        } else {
            field(n, name, d.render(), render_scaled(i128::from(ours_1e6), 6))
        }
    };
    money("runup", e.runup, a.max_runup_1e6)
        .or_else(|| money("drawdown", e.drawdown, adverse_signed(a.max_drawdown_1e6)))
}

/// The trades TradingView kept after trimming, against the broker's retained ones (CI-029, E109).
///
/// `pin:06-trade-trim-kept` — TradingView keeps the NEWEST 9 000 closed trades: E109's List of
/// Trades starts at trade #3243 = `closedtrades` 12 242 − 9 000 + 1 and ends at #12242, and its
/// `strategy.closedtrades.first_index` plot reads 3242; the TV broker retains the newest 9 000 too
/// (`closed_retained`, `first_index = closedtrades − closed_retained`). `evicted` = the broker's
/// dropped count: the first expected row must be trade `evicted + 1` (field `n`), and the running
/// `cum_pnl` starts at `cum_seed` = the dropped trades' P&L on `basis` (the broker's closed totals
/// minus the retained trades'), never at the export's own first `cum_pnl`.
pub fn compare_trades_kept(
    expected: &[TradeRow],
    actual: &[TradeRec],
    evicted: u32,
    cum_seed: i128,
    syms: &SymTable,
    signals: &SignalTable,
    basis: PnlBasis,
) -> Option<TradeDiv> {
    if evicted > 0 {
        if let Some(e) = expected.first() {
            if e.n != evicted + 1 {
                return field(e.n, "n", e.n.to_string(), (evicted + 1).to_string());
            }
        }
    }
    let mut cum = cum_seed;
    let mut first: Option<(usize, TradeDiv)> = None;
    let both = expected.len().min(actual.len());
    let mut i = 0usize;
    while i < both {
        let a = &actual[i];
        if a.flags & trade_flag::OPEN == 0 {
            cum += pnl_on(a, basis);
        }
        if let Some(d) = compare_one(&expected[i], a, cum, syms, signals, basis) {
            first = Some((i, d));
            break;
        }
        i += 1;
    }
    if expected.len() != actual.len() {
        return Some(TradeDiv::Count {
            expected: expected.len(),
            actual: actual.len(),
            first_mismatch: first.map_or(both, |(i, _)| i),
        });
    }
    first.map(|(_, d)| d)
}

/// The shape of a trimmed list on a tape that is not TradingView's (CI-029, E109).
///
/// `pin:06-trade-trim-kept` — E109 runs on SYN bars (`driver syn`, `align step`) while TradingView
/// traded its chart, so prices, times and money are not comparable; what is: TradingView kept the
/// newest closed trades — its rows are numbered `evicted + 1 …`, consecutively — and as many as the
/// broker retained closed. `actual` is closed-then-open; open trades are not in this comparison.
pub fn compare_kept_shape(
    expected: &[TradeRow],
    actual: &[TradeRec],
    evicted: u32,
) -> Option<TradeDiv> {
    let closed = actual
        .iter()
        .filter(|t| t.flags & trade_flag::OPEN == 0)
        .count();
    let mut i = 0usize;
    while i < expected.len() {
        let e = &expected[i];
        let want = evicted + i as u32 + 1;
        if e.n != want || e.exit.is_none() {
            return field(e.n, "n", e.n.to_string(), want.to_string());
        }
        i += 1;
    }
    (expected.len() != closed).then_some(TradeDiv::Count {
        expected: expected.len(),
        actual: closed,
        first_mismatch: expected.len().min(closed),
    })
}

/// The P&L on `basis` of the broker's retained closed trades (`actual` closed-then-open).
pub fn retained_pnl(actual: &[TradeRec], basis: PnlBasis) -> i128 {
    let mut s = 0i128;
    let mut i = 0usize;
    while i < actual.len() {
        if actual[i].flags & trade_flag::OPEN == 0 {
            s += pnl_on(&actual[i], basis);
        }
        i += 1;
    }
    s
}

/// §4.4.4: structural fields exact, prices exact after ×1e6 half-away, sizes exact, money within half a
/// printed unit; `actual` is closed-then-open in trade order.
pub fn compare_trades(
    expected: &[TradeRow],
    actual: &[TradeRec],
    syms: &SymTable,
    signals: &SignalTable,
    basis: PnlBasis,
) -> Option<TradeDiv> {
    compare_trades_kept(expected, actual, 0, 0, syms, signals, basis)
}

/// `closed_trades` =the number of closed `TradeRec`s (`RunRecord` counts wins and losses, not the total).
pub fn compare_summary(
    expected: &Summary,
    actual: &RunRecord,
    closed_trades: u32,
) -> Option<TradeDiv> {
    let money: [(&'static str, Dec, i128); 5] = [
        ("net", expected.net, actual.net_profit_1e12),
        (
            "gross_profit",
            expected.gross_profit,
            actual.gross_profit_1e12,
        ),
        ("gross_loss", expected.gross_loss, actual.gross_loss_1e12),
        (
            "commission",
            expected.commission,
            actual.commission_paid_1e12,
        ),
        (
            "max_drawdown",
            expected.max_drawdown,
            actual.max_drawdown_1e12,
        ),
    ];
    let mut i = 0usize;
    while i < money.len() {
        let (name, d, ours) = money[i];
        if !money_ok(ours, d) {
            return field(0, name, d.render(), ours.to_string());
        }
        i += 1;
    }
    let counts: [(&'static str, u32, u32); 3] = [
        ("closed", expected.closed, closed_trades),
        ("wins", expected.wins, actual.wins),
        ("losses", expected.losses, actual.losses),
    ];
    let mut j = 0usize;
    while j < counts.len() {
        let (name, e, a) = counts[j];
        if e != a {
            return field(0, name, e.to_string(), a.to_string());
        }
        j += 1;
    }
    None
}
