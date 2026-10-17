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
//!
//! `pin:06-trades-columns-recon` — recon 2026-10-17 read the List of Trades header of the Premium
//! "Download .csv" (17 columns, UTF-8 with BOM, comma). Each column is a `col_*` key; the keys added
//! for it (`col_size_value`, `col_return_pct`, `col_commission`, `col_fav_exc`, `col_fav_exc_pct`,
//! `col_adv_exc`, `col_adv_exc_pct`, `col_cum_pnl_pct`, `col_bars`) are named here, not by §4.7.3.
//! The excursion columns replace the guessed `col_runup` / `col_drawdown` (retired keys, refused
//! with their successor named). The `type_*` values and the date format were not read: VERIFY.
//!
//! `pin:06-format-ccy` — the money titles carry the symbol currency as a suffix (`Price USDT`):
//! a `col_*` title spells it `{ccy}`, and [`load_format`] substitutes `syminfo.tsv`'s `currency`
//! (every fixture runs on the one conformance chart, §4.1.4). Only `col_*` titles are substituted.

use std::path::Path;

use crate::fixture::FixError;
use crate::trades::PnlBasis;

/// The currency placeholder of a `col_*` title (`pin:06-format-ccy`).
pub const CCY: &str = "{ccy}";

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

/// How the List of Trades prints its `col_time` cells (`trades_time_format`).
///
/// `pin:06-trades-time-format` — §4.7.3 leaves "the date format" of the List of Trades a format key
/// to VERIFY at probe-03; probe-03 and the E001 capture (2026-10-17) print `YYYY-MM-DD HH:MM`: UTC
/// (`time_zone`), the fill bar's open, no zone suffix, no seconds. The key is required like every
/// other §4.7.3 key (§4.4 says nothing of it); the chart's `time_format` is a separate key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradesTimeFormat {
    /// `ymd_hm` — `YYYY-MM-DD HH:MM`, UTC.
    YmdHm,
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
    /// `col_fav_exc` — the favorable excursion in money (→ `TradeRow::runup`).
    pub fav_exc: String,
    /// `col_adv_exc` — the adverse excursion in money (→ `TradeRow::drawdown`).
    pub adv_exc: String,
    /// `col_bars` — the duration in bars (→ `TradeRow::bars`, less `duration_offset`).
    pub bars: String,
    /// Columns the export carries that no `.fix` field holds (§4.4.1's trade row): checked present,
    /// never read — `col_size_value`, `col_return_pct`, `col_commission`, `col_fav_exc_pct`,
    /// `col_adv_exc_pct`, `col_cum_pnl_pct`.
    pub unread: [String; 6],
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
    /// `time_format`: `unix_s`, `unix_ms`, `iso` — the chart export's time column.
    pub time_format: TimeFormat,
    /// `trades_time_format`: `ymd_hm` — the List of Trades `col_time` cells
    /// (`pin:06-trades-time-format`).
    pub trades_time_format: TradesTimeFormat,
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
    /// `duration_offset` — the duration convention of §4.7.2: `0` when the printed duration is
    /// `exit_bar − entry_bar`, `1` when it is `+ 1`.
    ///
    /// `pin:06-duration-offset-bars` — read literally from §4.7.2 / R12 (the convention, 0 or 1),
    /// now that the export prints `Duration (bars)` (recon 2026-10-17): `bars = printed −
    /// duration_offset`. It never shifts a trade time (§4.4.4: TradingView prints the bar time).
    pub duration_offset: i64,
    /// `p_negzero` — the export prints the sign of zero (§4.4.2).
    pub prints_negzero: bool,
    /// List of Trades titles.
    pub trades: TradeColumns,
    /// Performance Summary titles.
    pub summary: SummaryRows,
}

const KEYS: [&str; 16] = [
    "digits",
    "na",
    "inf",
    "neg_inf",
    "delimiter",
    "eol",
    "time_format",
    "trades_time_format",
    "time_zone",
    "ohlcv_columns",
    "titles_verbatim",
    "data_window_exported",
    "pnl_basis",
    "size_digits",
    "duration_offset",
    "time_column",
];

/// The `col_*` keys whose title [`ExportFormat::resolve_ccy`] substitutes — the List of Trades
/// columns, in the recon header's order (`pin:06-trades-columns-recon`).
const COL_KEYS: [&str; 17] = [
    "col_trade",
    "col_type",
    "col_time",
    "col_signal",
    "col_price",
    "col_qty",
    "col_size_value",
    "col_pnl",
    "col_return_pct",
    "col_commission",
    "col_fav_exc",
    "col_fav_exc_pct",
    "col_adv_exc",
    "col_adv_exc_pct",
    "col_cum_pnl",
    "col_cum_pnl_pct",
    "col_bars",
];

/// Keys a `format.tsv` may no longer carry, with their successor (recon 2026-10-17).
const RETIRED: [(&str, &str); 2] = [
    ("col_runup", "col_fav_exc"),
    ("col_drawdown", "col_adv_exc"),
];

const TRADE_KEYS: [&str; 22] = [
    "col_trade",
    "col_type",
    "col_time",
    "col_signal",
    "col_price",
    "col_qty",
    "col_size_value",
    "col_pnl",
    "col_return_pct",
    "col_commission",
    "col_fav_exc",
    "col_fav_exc_pct",
    "col_adv_exc",
    "col_adv_exc_pct",
    "col_cum_pnl",
    "col_cum_pnl_pct",
    "col_bars",
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

impl ExportFormat {
    /// Whether a `col_*` title still spells [`CCY`].
    pub fn has_ccy(&self) -> bool {
        self.trade_titles().iter().any(|t| t.contains(CCY))
    }

    /// Substitutes `ccy` for [`CCY`] in every `col_*` title (`pin:06-format-ccy`).
    pub fn resolve_ccy(&mut self, ccy: &str) -> Result<(), String> {
        if ccy.is_empty() || ccy.contains(CCY) || ccy.chars().any(char::is_control) {
            return Err(format!("`{ccy}` is not a currency (pin:06-format-ccy)"));
        }
        let c = &mut self.trades;
        let [u0, u1, u2, u3, u4, u5] = &mut c.unread;
        let titles: [&mut String; 17] = [
            &mut c.trade,
            &mut c.kind,
            &mut c.time,
            &mut c.signal,
            &mut c.price,
            &mut c.qty,
            &mut c.pnl,
            &mut c.cum_pnl,
            &mut c.fav_exc,
            &mut c.adv_exc,
            &mut c.bars,
            u0,
            u1,
            u2,
            u3,
            u4,
            u5,
        ];
        for t in titles {
            if t.contains(CCY) {
                *t = t.replace(CCY, ccy);
            }
        }
        Ok(())
    }

    /// The 17 `col_*` titles, in [`COL_KEYS`] order.
    pub fn trade_titles(&self) -> [&str; 17] {
        let c = &self.trades;
        [
            &c.trade,
            &c.kind,
            &c.time,
            &c.signal,
            &c.price,
            &c.qty,
            &c.unread[0],
            &c.pnl,
            &c.unread[1],
            &c.unread[2],
            &c.fav_exc,
            &c.unread[3],
            &c.adv_exc,
            &c.unread[4],
            &c.cum_pnl,
            &c.unread[5],
            &c.bars,
        ]
    }

    /// The `col_*` key of the title at `i` of [`ExportFormat::trade_titles`].
    pub fn trade_key(i: usize) -> &'static str {
        COL_KEYS[i]
    }
}

/// Reads `<root>/tests/conformance/capture/format.tsv`; a `{ccy}` in a `col_*` title is resolved
/// from `<root>/tests/conformance/syminfo.tsv` (`pin:06-format-ccy`), which is read only then.
pub fn load_format(root: &Path) -> Result<ExportFormat, FixError> {
    let p = root.join(FORMAT_TSV);
    // COPY: `format.tsv` (< 4 KiB) once per distil, offline — the parser reads owned text — a
    // memory map rejected (no third-party crate, contract §2).
    let text = std::fs::read_to_string(&p)
        .map_err(|e| err(&p.display().to_string(), 0, format!("unreadable: {e}")))?;
    let mut fmt = parse_format(&text, &p.display().to_string())?;
    if fmt.has_ccy() {
        let s = root.join(crate::chart::SYMINFO_TSV);
        let sp = s.display().to_string();
        // COPY: `syminfo.tsv` (8 lines) once per distil, offline — as `format.tsv` above.
        let si = std::fs::read_to_string(&s)
            .map_err(|e| err(&sp, 0, format!("unreadable ({CCY} in format.tsv): {e}")))?;
        let si = crate::chart::parse_syminfo(&si).map_err(|e| err(&sp, 0, e))?;
        fmt.resolve_ccy(&si.currency).map_err(|e| err(&sp, 0, e))?;
    }
    Ok(fmt)
}

/// Parses `format.tsv` text: every key of §4.7.3 plus `time_column`, `trades_time_format`
/// (`pin:06-trades-time-format`), `p_negzero` (optional, `no`),
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
        if let Some((_, now)) = RETIRED.iter().find(|(old, _)| *old == k) {
            return Err(err(
                path,
                ln,
                format!("retired key `{k}`: now `{now}` (pin:06-trades-columns-recon)"),
            ));
        }
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
    let (ttv, ttl) = need("trades_time_format")?;
    let trades_time_format = match ttv {
        "ymd_hm" => TradesTimeFormat::YmdHm,
        _ => {
            return Err(err(
                path,
                ttl,
                "trades_time_format is ymd_hm (pin:06-trades-time-format)".into(),
            ))
        }
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
    let (ov, ol) = need("duration_offset")?;
    let duration_offset = num("duration_offset", (ov, ol))?;
    if !(0..=1).contains(&duration_offset) {
        return Err(err(
            path,
            ol,
            "duration_offset is 0|1 (pin:06-duration-offset-bars)".into(),
        ));
    }
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
        trades_time_format,
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
        duration_offset,
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
            fav_exc: s("col_fav_exc"),
            adv_exc: s("col_adv_exc"),
            bars: s("col_bars"),
            unread: [
                s("col_size_value"),
                s("col_return_pct"),
                s("col_commission"),
                s("col_fav_exc_pct"),
                s("col_adv_exc_pct"),
                s("col_cum_pnl_pct"),
            ],
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
