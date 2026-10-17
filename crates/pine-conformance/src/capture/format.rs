// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `capture/format.tsv` — TradingView's export format, as measured by probe-00 / probe-03
//! (impl-06 §3.10, §4.7.3). Every parser reads it; nothing hardcodes a TradingView spelling.
//! Offline capture tooling: allocation is free here.
//!
//! `pin:06-format-tsv-layout` — §4.7.3 names the keys, not the layout: one `key TAB value` line
//! each, `#` comment lines and blank lines ignored, each key at most once; an unknown key is an
//! error. Booleans are `yes` / `no`.
//!
//! `pin:06-export-columns-verify-in-recon` — §4.7.3's key list carries no column titles, yet the
//! List of Trades and the Performance Summary are read by title. The titles are therefore format
//! keys too (`col_*`, `type_*`, `open_signal`, `sum_*`, `time_column`), measured by probe-03 on the
//! Premium "strategy report" (the former Strategy Tester; recon 2026-10-17). Every one of them may be
//! renamed by the Premium UI and is verified in recon before the first capture session.

use std::path::Path;

use crate::fixture::FixError;
use crate::trades::PnlBasis;

/// `capture/format.tsv`, relative to the workspace root.
pub const FORMAT_TSV: &str = "tests/conformance/capture/format.tsv";

/// How the export prints a bar time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    /// Seconds since the epoch.
    UnixS,
    /// Milliseconds since the epoch.
    UnixMs,
    /// `YYYY-MM-DDTHH:MM[:SS]Z` (or with a space for `T`), UTC.
    Iso,
}

/// The List of Trades column titles (`pin:06-export-columns-verify-in-recon`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TradeColumns {
    /// `col_trade` — the trade number.
    pub trade: String,
    /// `col_type` — entry / exit and side.
    pub kind: String,
    /// `col_signal`.
    pub signal: String,
    /// `col_time`.
    pub time: String,
    /// `col_price`.
    pub price: String,
    /// `col_qty`.
    pub qty: String,
    /// `col_pnl`.
    pub pnl: String,
    /// `col_cum_pnl`.
    pub cum_pnl: String,
    /// `col_runup`.
    pub runup: String,
    /// `col_drawdown`.
    pub drawdown: String,
    /// `type_entry_long`, `type_entry_short`, `type_exit_long`, `type_exit_short`.
    pub types: [String; 4],
    /// `open_signal` — the exit row's signal text of a trade still open at export.
    pub open_signal: String,
}

/// The Performance Summary row titles (`pin:06-export-columns-verify-in-recon`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryRows {
    /// `sum_net`, `sum_gross_profit`, `sum_gross_loss`, `sum_commission`, `sum_max_drawdown`,
    /// `sum_closed`, `sum_wins`, `sum_losses`.
    pub titles: [String; 8],
}

/// The export format measured by probe-00 / probe-03 (§4.7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFormat {
    /// `digits` — significant digits the export prints (`< 17` → `dec` tolerances).
    pub digits: u8,
    /// `na` token.
    pub na: String,
    /// `inf` token.
    pub inf: String,
    /// `neg_inf` token.
    pub neg_inf: String,
    /// `delimiter`: `comma`, `tab`, `semicolon` or one ASCII byte.
    pub delimiter: u8,
    /// `eol`: `lf` / `crlf`.
    pub crlf: bool,
    /// `time_format`: `unix_s`, `unix_ms`, `iso`.
    pub time_format: TimeFormat,
    /// `time_zone` — must be `UTC` (`pin:06-export-utc-only`: the chart is UTC, §4.1.4).
    pub time_zone: String,
    /// `time_column` — the chart export's time column title.
    pub time_column: String,
    /// `ohlcv_columns` — the export's OHLCV column titles, comma-separated (dropped, I4).
    pub ohlcv_columns: Vec<String>,
    /// `titles_verbatim` — plot titles exported exactly as declared.
    pub titles_verbatim: bool,
    /// `data_window_exported` — `display.data_window` plots appear in the export.
    pub data_window_exported: bool,
    /// `pnl_basis`: `net` / `gross`.
    pub pnl_basis: PnlBasis,
    /// `size_digits` — decimals of a printed size.
    pub size_digits: u8,
    /// `duration_offset` — ms added to a printed trade time to reach the bar open (0 = none).
    pub duration_offset: i64,
    /// `p_negzero` — the export prints the sign of zero (§4.4.2).
    pub prints_negzero: bool,
    /// List of Trades titles.
    pub trades: TradeColumns,
    /// Performance Summary titles.
    pub summary: SummaryRows,
}

const KEYS: [&str; 15] = [
    "digits",
    "na",
    "inf",
    "neg_inf",
    "delimiter",
    "eol",
    "time_format",
    "time_zone",
    "ohlcv_columns",
    "titles_verbatim",
    "data_window_exported",
    "pnl_basis",
    "size_digits",
    "duration_offset",
    "time_column",
];

const TRADE_KEYS: [&str; 15] = [
    "col_trade",
    "col_type",
    "col_signal",
    "col_time",
    "col_price",
    "col_qty",
    "col_pnl",
    "col_cum_pnl",
    "col_runup",
    "col_drawdown",
    "type_entry_long",
    "type_entry_short",
    "type_exit_long",
    "type_exit_short",
    "open_signal",
];

const SUM_KEYS: [&str; 8] = [
    "sum_net",
    "sum_gross_profit",
    "sum_gross_loss",
    "sum_commission",
    "sum_max_drawdown",
    "sum_closed",
    "sum_wins",
    "sum_losses",
];

fn err(path: &str, line: u32, what: String) -> FixError {
    FixError {
        path: path.to_string(),
        line,
        what,
    }
}

/// Reads `<root>/tests/conformance/capture/format.tsv`.
pub fn load_format(root: &Path) -> Result<ExportFormat, FixError> {
    let p = root.join(FORMAT_TSV);
    // COPY: `format.tsv` (< 4 KiB) once per distil, offline — the parser reads owned text — a
    // memory map rejected (no third-party crate, contract §2).
    let text = std::fs::read_to_string(&p)
        .map_err(|e| err(&p.display().to_string(), 0, format!("unreadable: {e}")))?;
    parse_format(&text, &p.display().to_string())
}

/// Parses `format.tsv` text: every key of §4.7.3 plus `time_column`, `p_negzero` (optional, `no`),
/// the `col_*` / `type_*` / `open_signal` / `sum_*` titles (optional as a block: absent = this
/// export has no trade report).
pub fn parse_format(text: &str, path: &str) -> Result<ExportFormat, FixError> {
    let mut kv: Vec<(&str, &str, u32)> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let ln = i as u32 + 1;
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('\t') else {
            return Err(err(path, ln, "want `key<TAB>value`".into()));
        };
        let known = KEYS.contains(&k)
            || TRADE_KEYS.contains(&k)
            || SUM_KEYS.contains(&k)
            || k == "p_negzero";
        if !known {
            return Err(err(path, ln, format!("unknown key `{k}`")));
        }
        if kv.iter().any(|(k2, ..)| *k2 == k) {
            return Err(err(path, ln, format!("`{k}` twice")));
        }
        kv.push((k, v, ln));
    }
    let get = |k: &str| {
        kv.iter()
            .find(|(k2, ..)| *k2 == k)
            .map(|(_, v, l)| (*v, *l))
    };
    let need = |k: &str| get(k).ok_or_else(|| err(path, 0, format!("missing key `{k}`")));
    let yes_no = |k: &str, (v, l): (&str, u32)| match v {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(err(path, l, format!("`{k}` is yes|no"))),
    };
    let num = |k: &str, (v, l): (&str, u32)| {
        v.parse::<i64>()
            .map_err(|_| err(path, l, format!("`{k}` is an integer")))
    };
    let (digits, size_digits) = (
        num("digits", need("digits")?)?,
        num("size_digits", need("size_digits")?)?,
    );
    if !(1..=17).contains(&digits) || !(0..=18).contains(&size_digits) {
        return Err(err(path, 0, "digits 1..=17, size_digits 0..=18".into()));
    }
    let (dv, dl) = need("delimiter")?;
    let delimiter = match dv {
        "comma" => b',',
        "tab" => b'\t',
        "semicolon" => b';',
        s if s.len() == 1 && s.is_ascii() => s.as_bytes()[0],
        _ => return Err(err(path, dl, "delimiter".into())),
    };
    let (ev, el) = need("eol")?;
    let crlf = match ev {
        "lf" => false,
        "crlf" => true,
        _ => return Err(err(path, el, "eol is lf|crlf".into())),
    };
    let (tv, tl) = need("time_format")?;
    let time_format = match tv {
        "unix_s" => TimeFormat::UnixS,
        "unix_ms" => TimeFormat::UnixMs,
        "iso" => TimeFormat::Iso,
        _ => return Err(err(path, tl, "time_format is unix_s|unix_ms|iso".into())),
    };
    let (zv, zl) = need("time_zone")?;
    if zv != "UTC" {
        return Err(err(
            path,
            zl,
            "pin:06-export-utc-only: time_zone must be UTC".into(),
        ));
    }
    let (pv, pl) = need("pnl_basis")?;
    let pnl_basis = match pv {
        "net" => PnlBasis::Net,
        "gross" => PnlBasis::Gross,
        _ => return Err(err(path, pl, "pnl_basis is net|gross".into())),
    };
    let s = |k: &str| get(k).map_or(String::new(), |(v, _)| v.to_string());
    let has_trades = TRADE_KEYS.iter().any(|k| get(k).is_some());
    if has_trades {
        for k in TRADE_KEYS {
            need(k)?;
        }
    }
    let has_sum = SUM_KEYS.iter().any(|k| get(k).is_some());
    if has_sum {
        for k in SUM_KEYS {
            need(k)?;
        }
    }
    Ok(ExportFormat {
        digits: digits as u8,
        na: need("na")?.0.to_string(),
        inf: need("inf")?.0.to_string(),
        neg_inf: need("neg_inf")?.0.to_string(),
        delimiter,
        crlf,
        time_format,
        time_zone: zv.to_string(),
        time_column: need("time_column")?.0.to_string(),
        ohlcv_columns: need("ohlcv_columns")?
            .0
            .split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect(),
        titles_verbatim: yes_no("titles_verbatim", need("titles_verbatim")?)?,
        data_window_exported: yes_no("data_window_exported", need("data_window_exported")?)?,
        pnl_basis,
        size_digits: size_digits as u8,
        duration_offset: num("duration_offset", need("duration_offset")?)?,
        prints_negzero: match get("p_negzero") {
            Some(v) => yes_no("p_negzero", v)?,
            None => false,
        },
        trades: TradeColumns {
            trade: s("col_trade"),
            kind: s("col_type"),
            signal: s("col_signal"),
            time: s("col_time"),
            price: s("col_price"),
            qty: s("col_qty"),
            pnl: s("col_pnl"),
            cum_pnl: s("col_cum_pnl"),
            runup: s("col_runup"),
            drawdown: s("col_drawdown"),
            types: [
                s("type_entry_long"),
                s("type_entry_short"),
                s("type_exit_long"),
                s("type_exit_short"),
            ],
            open_signal: s("open_signal"),
        },
        summary: SummaryRows {
            titles: SUM_KEYS.map(s),
        },
    })
}
