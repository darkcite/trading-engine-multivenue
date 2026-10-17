// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The three export shapes (impl-06 §3.10): chart data, List of Trades, Performance Summary —
//! parsed under [`ExportFormat`]. Offline capture tooling: allocation is free here.
//!
//! `pin:06-csv-quoting` — RFC 4180 quoting (a field in `"`, `""` an escaped quote); the delimiter
//! and line end come from `format.tsv`; a trailing empty line is ignored.

use super::format::{ExportFormat, TimeFormat, CCY};
use crate::dec::{parse_dec, Dec};
use crate::fixture::{Cell, ExitCols, Summary, TradeRow};

/// One chart-data export: column titles (time column excluded) and rows of `(open_ms, cells)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartExport {
    /// Column titles after the time column, in export order (OHLCV included).
    pub titles: Vec<String>,
    /// `(bar open ms, one cell per title)`, in export order.
    pub rows: Vec<(i64, Vec<Cell>)>,
    /// `ExportFormat::digits` at parse time — `distil` sets `dec` tolerances from it.
    ///
    /// `pin:06-export-carries-digits` — §3.10's `distil` takes no format, yet §4.4.2's `dec` rule
    /// depends on `format.tsv`'s digits: the parsed export carries them.
    pub digits: u8,
}

/// Splits the export into records of fields. A leading UTF-8 BOM is dropped (recon 2026-10-17: the
/// List of Trades "Download .csv" carries one).
pub fn records(bytes: &[u8], fmt: &ExportFormat) -> Result<Vec<Vec<String>>, String> {
    let text = core::str::from_utf8(bytes).map_err(|_| "export is not UTF-8".to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let d = char::from(fmt.delimiter);
    let mut out = Vec::new();
    let mut rec: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut at_start = true;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            c if c == d => {
                rec.push(std::mem::take(&mut field));
                at_start = true;
            }
            '\r' if fmt.crlf && chars.peek() == Some(&'\n') => {}
            '\n' => {
                rec.push(std::mem::take(&mut field));
                out.push(std::mem::take(&mut rec));
                at_start = true;
            }
            c => {
                field.push(c);
                at_start = false;
            }
        }
    }
    if quoted {
        return Err("unterminated quoted field".into());
    }
    if !field.is_empty() || !rec.is_empty() {
        rec.push(field);
        out.push(rec);
    }
    Ok(out)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A printed time → UTC ms.
pub fn parse_time(s: &str, fmt: &ExportFormat) -> Result<i64, String> {
    let bad = || format!("bad time `{s}`");
    match fmt.time_format {
        TimeFormat::UnixS => s
            .parse::<i64>()
            .ok()
            .and_then(|v| v.checked_mul(1000))
            .ok_or_else(bad),
        TimeFormat::UnixMs => s.parse::<i64>().map_err(|_| bad()),
        TimeFormat::Iso => {
            let t = s.strip_suffix('Z').ok_or_else(bad)?;
            let b = t.as_bytes();
            if b.len() != 16 && b.len() != 19 {
                return Err(bad());
            }
            if b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' {
                return Err(bad());
            }
            let n = |a: usize, z: usize| t[a..z].parse::<i64>().map_err(|_| bad());
            let sec = if b.len() == 19 {
                if b[16] != b':' {
                    return Err(bad());
                }
                n(17, 19)?
            } else {
                0
            };
            let days = days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?);
            Ok(((days * 24 + n(11, 13)?) * 60 + n(14, 16)?) * 60_000 + sec * 1000)
        }
    }
}

/// One exported value → a `.fix` cell (`na`, `inf`, `-inf`, or the printed decimal kept verbatim).
pub fn parse_value(s: &str, fmt: &ExportFormat) -> Result<Cell, String> {
    if s == fmt.na {
        return Ok(Cell::Na);
    }
    if s == fmt.inf {
        return Ok(Cell::Val {
            v: f64::INFINITY,
            text: "inf".into(),
        });
    }
    if s == fmt.neg_inf {
        return Ok(Cell::Val {
            v: f64::NEG_INFINITY,
            text: "-inf".into(),
        });
    }
    let ok = !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'));
    let v: f64 = if ok {
        s.parse().map_err(|_| format!("bad value `{s}`"))?
    } else {
        return Err(format!("bad value `{s}`"));
    };
    Ok(Cell::Val {
        v,
        text: s.trim_start_matches('+').replace('E', "e"),
    })
}

/// The chart-data export.
pub fn parse_chart_csv(bytes: &[u8], fmt: &ExportFormat) -> Result<ChartExport, String> {
    let recs = records(bytes, fmt)?;
    let (head, body) = recs.split_first().ok_or("empty export")?;
    let tcol = head
        .iter()
        .position(|t| *t == fmt.time_column)
        .ok_or_else(|| format!("no `{}` column", fmt.time_column))?;
    let titles: Vec<String> = head
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != tcol)
        .map(|(_, t)| t.clone())
        .collect();
    let mut rows = Vec::with_capacity(body.len());
    for (ri, r) in body.iter().enumerate() {
        if r.len() != head.len() {
            return Err(format!(
                "row {}: {} fields, header has {}",
                ri + 2,
                r.len(),
                head.len()
            ));
        }
        let t = parse_time(&r[tcol], fmt)?;
        let mut cells = Vec::with_capacity(titles.len());
        for (i, f) in r.iter().enumerate() {
            if i != tcol {
                cells.push(parse_value(f, fmt).map_err(|e| format!("row {}: {e}", ri + 2))?);
            }
        }
        rows.push((t, cells));
    }
    Ok(ChartExport {
        titles,
        rows,
        digits: fmt.digits,
    })
}

fn dec(s: &str, what: &str) -> Result<Dec, String> {
    parse_dec(s.as_bytes()).map_err(|e| format!("{what} `{s}`: {e:?}"))
}

fn opt_dec(s: &str, what: &str) -> Result<Option<Dec>, String> {
    if s.is_empty() {
        Ok(None)
    } else {
        dec(s, what).map(Some)
    }
}

/// The List of Trades: two records per trade (an entry and an exit), any order of trade numbers in
/// the file; trades are returned by number.
///
/// `pin:06-trades-two-rows` — the entry record carries `time`, `price`, `signal`; the exit record
/// carries the exit columns, `qty`, `pnl`, `cum_pnl`, the excursions (→ `runup`, `drawdown`) and
/// the duration (→ `bars`, less `duration_offset`, `pin:06-duration-offset-bars`). An exit record
/// whose signal is `open_signal` marks a trade still open (no exit columns, no P&L, no `bars`).
///
/// `pin:06-trades-row-order` — recon 2026-10-17 read the header only, not which record of a trade
/// comes first: entries are read before exits, whatever their order in the file.
///
/// Every `col_*` title of `format.tsv` must be a header column (a missing one is named); the
/// columns no `.fix` field holds are checked, never read.
pub fn parse_trades_csv(bytes: &[u8], fmt: &ExportFormat) -> Result<Vec<TradeRow>, String> {
    let c = &fmt.trades;
    if c.trade.is_empty() {
        return Err("format.tsv carries no List of Trades columns".into());
    }
    if fmt.has_ccy() {
        return Err(format!(
            "a `col_*` title still spells `{CCY}`: resolve it from syminfo.tsv (pin:06-format-ccy)"
        ));
    }
    let recs = records(bytes, fmt)?;
    let (head, body) = recs.split_first().ok_or("empty export")?;
    let titles = fmt.trade_titles();
    let mut missing: Vec<String> = Vec::new();
    for (i, t) in titles.iter().enumerate() {
        if !head.iter().any(|h| h == t) {
            missing.push(format!("`{t}` ({})", ExportFormat::trade_key(i)));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "no {} column (pin:06-export-columns-verify-in-recon)",
            missing.join(", ")
        ));
    }
    let col = |t: &str| head.iter().position(|h| h == t).unwrap_or(usize::MAX);
    let ix = [
        col(&c.trade),
        col(&c.kind),
        col(&c.signal),
        col(&c.time),
        col(&c.price),
        col(&c.qty),
        col(&c.pnl),
        col(&c.cum_pnl),
        col(&c.fav_exc),
        col(&c.adv_exc),
        col(&c.bars),
    ];
    let mut out: Vec<TradeRow> = Vec::new();
    for pass_exits in [false, true] {
        parse_trade_pass(body, fmt, &ix, pass_exits, &mut out)?;
    }
    out.sort_by_key(|t| t.n);
    Ok(out)
}

/// One pass of [`parse_trades_csv`] over the body: entries (`exits = false`) or exits.
fn parse_trade_pass(
    body: &[Vec<String>],
    fmt: &ExportFormat,
    ix: &[usize; 11],
    exits: bool,
    out: &mut Vec<TradeRow>,
) -> Result<(), String> {
    let c = &fmt.trades;
    let mut exited: Vec<u32> = Vec::new();
    for (ri, r) in body.iter().enumerate() {
        let line = ri + 2;
        let f = |k: usize| r.get(ix[k]).map(String::as_str).unwrap_or("");
        let n: u32 = f(0)
            .parse()
            .map_err(|_| format!("row {line}: trade number"))?;
        let ty = c
            .types
            .iter()
            .position(|t| t == f(1))
            .ok_or_else(|| format!("row {line}: unknown type `{}`", f(1)))?;
        if (ty >= 2) != exits {
            continue;
        }
        let long = ty % 2 == 0;
        let t = parse_time(f(3), fmt)?;
        if ty < 2 {
            if out.iter().any(|x| x.n == n) {
                return Err(format!("row {line}: trade {n} entered twice"));
            }
            out.push(TradeRow {
                n,
                long,
                entry_ms: t,
                entry_px: dec(f(4), "price")?,
                entry_sig: f(2).to_string(),
                exit: None,
                qty: Dec {
                    neg: false,
                    m: 0,
                    d: 0,
                },
                pnl: None,
                cum_pnl: None,
                runup: None,
                drawdown: None,
                bars: None,
            });
            continue;
        }
        if exited.contains(&n) {
            return Err(format!("row {line}: trade {n} exited twice"));
        }
        exited.push(n);
        let tr = out
            .iter_mut()
            .find(|x| x.n == n)
            .ok_or_else(|| format!("row {line}: exit of trade {n}, which has no entry"))?;
        if tr.long != long {
            return Err(format!("row {line}: trade {n} changes side"));
        }
        tr.qty = dec(f(5), "qty")?;
        tr.runup = opt_dec(f(8), "favorable excursion")?;
        tr.drawdown = opt_dec(f(9), "adverse excursion")?;
        if f(2) != c.open_signal {
            tr.exit = Some(ExitCols {
                ms: t,
                px: dec(f(4), "price")?,
                sig: f(2).to_string(),
            });
            tr.pnl = opt_dec(f(6), "pnl")?;
            tr.cum_pnl = opt_dec(f(7), "cum_pnl")?;
            tr.bars = match f(10) {
                "" => None,
                b => {
                    let printed: i64 = b
                        .parse()
                        .map_err(|_| format!("row {line}: duration `{b}` is not a bar count"))?;
                    let bars = u32::try_from(printed - fmt.duration_offset).map_err(|_| {
                        format!("row {line}: duration {printed} less duration_offset < 0")
                    })?;
                    Some(bars)
                }
            };
        }
    }
    Ok(())
}

/// The Performance Summary: records `title, value[, …]`; the first value column (TradingView's
/// "All") is read.
///
/// `pin:06-summary-csv` — Premium exports the strategy report as XLSX (recon 2026-10-17); this
/// parser reads its offline CSV rendition, one row per metric, the first value column.
pub fn parse_summary_csv(bytes: &[u8], fmt: &ExportFormat) -> Result<Summary, String> {
    let t = &fmt.summary.titles;
    if t[0].is_empty() {
        return Err("format.tsv carries no Performance Summary rows".into());
    }
    let recs = records(bytes, fmt)?;
    let get = |k: usize| -> Result<&str, String> {
        recs.iter()
            .find(|r| r.first().is_some_and(|x| *x == t[k]))
            .and_then(|r| r.get(1))
            .map(String::as_str)
            .ok_or_else(|| format!("no `{}` row (pin:06-export-columns-verify-in-recon)", t[k]))
    };
    let count = |k: usize| -> Result<u32, String> {
        get(k)?
            .parse()
            .map_err(|_| format!("`{}` is a count", t[k]))
    };
    Ok(Summary {
        net: dec(get(0)?, "net")?,
        gross_profit: dec(get(1)?, "gross_profit")?,
        gross_loss: dec(get(2)?, "gross_loss")?,
        commission: dec(get(3)?, "commission")?,
        max_drawdown: dec(get(4)?, "max_drawdown")?,
        closed: count(5)?,
        wins: count(6)?,
        losses: count(7)?,
    })
}
