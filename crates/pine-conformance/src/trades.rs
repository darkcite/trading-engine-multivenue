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

/// `pin:06-exit-cause-close` — `TradeRec` records no flag telling `strategy.close` from
/// `strategy.close_all`: an id-less exit that is neither a liquidation nor a risk flatten matches
/// either text.
fn exit_sig_ok(sig: &str, a: &TradeRec, syms: &SymTable, signals: &SignalTable) -> bool {
    if let Some(t) = sym_text(syms, a.exit_sym) {
        return t == sig;
    }
    if a.exit_id != StrKey::NONE && pine_ir::str_key(sig.as_bytes()) == a.exit_id {
        return true;
    }
    if a.flags & trade_flag::LIQUIDATION != 0 {
        return sig == signals.liquidation;
    }
    if a.flags & trade_flag::RISK_FLATTEN != 0 {
        return sig == signals.risk_flatten;
    }
    sig == signals.close || sig == signals.close_all
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
    if scaled_half_away(e.entry_px, 6) != i128::from(a.entry_px_1e6) {
        return field(
            n,
            "entry_px",
            e.entry_px.render(),
            a.entry_px_1e6.to_string(),
        );
    }
    if scaled_half_away(e.qty, 6) != i128::from(a.qty_1e6.unsigned_abs()) {
        return field(n, "qty", e.qty.render(), a.qty_1e6.to_string());
    }
    if let Some(x) = &e.exit {
        if x.ms != a.exit_ms {
            return field(n, "exit_ms", x.ms.to_string(), a.exit_ms.to_string());
        }
        if !exit_sig_ok(&x.sig, a, syms, signals) {
            let ours = sym_text(syms, a.exit_sym).unwrap_or("<cause>").to_string();
            return field(n, "exit_sig", x.sig.clone(), ours);
        }
        if scaled_half_away(x.px, 6) != i128::from(a.exit_px_1e6) {
            return field(n, "exit_px", x.px.render(), a.exit_px_1e6.to_string());
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
                return field(n, "pnl", p.render(), pnl_on(a, basis).to_string());
            }
        }
        if let Some(c) = e.cum_pnl {
            if !money_ok(cum, c) {
                return field(n, "cum_pnl", c.render(), cum.to_string());
            }
        }
    }
    let money = |name: &'static str, d: Option<Dec>, ours_1e6: i64| -> Option<TradeDiv> {
        let d = d?;
        if money_ok(e6_to_e12(ours_1e6), d) {
            None
        } else {
            field(n, name, d.render(), ours_1e6.to_string())
        }
    };
    money("runup", e.runup, a.max_runup_1e6)
        .or_else(|| money("drawdown", e.drawdown, a.max_drawdown_1e6))
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
    let mut cum = 0i128;
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

/// `closed_trades` = the number of closed `TradeRec`s (`RunRecord` counts wins and losses, not the total).
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
