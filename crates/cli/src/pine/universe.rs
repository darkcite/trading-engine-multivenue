// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The screen universe TSV (impl-07 §3.12.2) — `~/multivenue/pine/screen-universe.tsv`.
//!
//! COPY-DOCTRINE: a cold operator file, read once per process; never reached by the engine loop.
//! Offline path: it may allocate.
//!
//! Operator-written; order is the thread and row order (append, never reorder). `#` lines are
//! comments. Fourteen tab-separated columns: `key descriptor capture_descriptor venue class
//! fee_venue mintick_1e6 pointvalue_1e6 qty_step_1e6 min_notional_1e6 ticker basecurrency currency
//! type`.

use std::path::Path;

use core_types::InstrumentClass;

use super::PineErr;
use crate::backtest::model_venue;

/// Number of columns in a universe row.
pub const UNIVERSE_COLS: usize = 14;

/// One universe row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniverseRow {
    /// Canonical `venue:symbol` (`pine_ir::is_canonical_symbol_key`).
    pub key: String,
    /// The candles.db descriptor, `None` for `-`.
    pub descriptor: Option<String>,
    /// The instrument-manifest descriptor in captures, `None` for `-`.
    pub capture_descriptor: Option<String>,
    /// `core_types::VenueId` raw byte.
    pub venue: u8,
    /// Instrument class.
    pub class: InstrumentClass,
    /// The `--fee-bps` venue label.
    pub fee_venue: String,
    /// `ChartInfo` integers.
    pub mintick_1e6: i64,
    /// `ChartInfo` integers.
    pub pointvalue_1e6: i64,
    /// `ChartInfo` integers.
    pub qty_step_1e6: i64,
    /// `ChartInfo` integers.
    pub min_notional_1e6: i64,
    /// `syminfo.ticker`.
    pub ticker: String,
    /// `syminfo.basecurrency`.
    pub basecurrency: String,
    /// `syminfo.currency`.
    pub currency: String,
    /// `syminfo.type`.
    pub sym_type: String,
}

impl UniverseRow {
    /// The fee-table venue byte of `fee_venue` (`backtest::model_venue`).
    pub fn fee_venue_byte(&self) -> usize {
        // Checked at parse time.
        model_venue(&self.fee_venue).unwrap_or(usize::MAX)
    }
}

fn opt(s: &str) -> Option<String> {
    if s == "-" {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Parse the universe text. A malformed row is a data refusal (exit 5) naming its line.
// pin:07-universe-refusal-exit-5: §3.11 lists no exit code for a malformed universe file; it is a
// data file named by argv, not an argv value, so it refuses as data (exit 5).
// pin:07-universe-fee-venue: `fee_venue` must be a `backtest::model_venue` label (the `--fee-bps`
// grammar's venue set), checked at load.
pub fn parse_universe(text: &str) -> Result<Vec<UniverseRow>, PineErr> {
    let mut rows = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let line_no = ln + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let bad = |why: &str| PineErr::Data(format!("universe line {line_no}: {why}"));
        if cols.len() != UNIVERSE_COLS {
            return Err(bad(&format!(
                "{} columns, want {UNIVERSE_COLS}",
                cols.len()
            )));
        }
        let key = cols[0];
        if !pine_ir::is_canonical_symbol_key(key.as_bytes()) {
            return Err(bad(&format!("{key:?} is not a canonical venue:symbol key")));
        }
        if rows.iter().any(|r: &UniverseRow| r.key == key) {
            return Err(bad(&format!("{key:?} named twice")));
        }
        let venue: u8 = cols[3]
            .parse()
            .map_err(|_| bad(&format!("venue {:?} is not a byte", cols[3])))?;
        let class = InstrumentClass::parse_label(cols[4])
            .ok_or_else(|| bad(&format!("unknown class {:?}", cols[4])))?;
        if model_venue(cols[5]).is_none() {
            return Err(bad(&format!("unknown fee_venue {:?}", cols[5])));
        }
        let int = |i: usize, name: &str| -> Result<i64, PineErr> {
            cols[i]
                .parse::<i64>()
                .map_err(|_| bad(&format!("{name} {:?} is not an integer", cols[i])))
        };
        rows.push(UniverseRow {
            key: key.to_owned(),
            descriptor: opt(cols[1]),
            capture_descriptor: opt(cols[2]),
            venue,
            class,
            fee_venue: cols[5].to_owned(),
            mintick_1e6: int(6, "mintick_1e6")?,
            pointvalue_1e6: int(7, "pointvalue_1e6")?,
            qty_step_1e6: int(8, "qty_step_1e6")?,
            min_notional_1e6: int(9, "min_notional_1e6")?,
            ticker: cols[10].to_owned(),
            basecurrency: cols[11].to_owned(),
            currency: cols[12].to_owned(),
            sym_type: cols[13].to_owned(),
        });
    }
    if rows.is_empty() {
        return Err(PineErr::Data("universe has no rows".to_owned()));
    }
    Ok(rows)
}

/// Read and parse the universe file.
pub fn load_universe(path: &Path) -> Result<Vec<UniverseRow>, PineErr> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PineErr::Data(format!("universe {}: {e}", path.display())))?;
    parse_universe(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = "binance_usdm:btcusdt\tbinance-usdm:btcusdt\tbinance-usdm:btcusdt\t1\tperp\tbn\t100000\t1000000\t1000\t5000000\tBTCUSDT\tBTC\tUSDT\tcrypto";

    #[test]
    fn universe_parses_a_row_and_skips_comments() {
        let rows = parse_universe(&format!("# comment\n\n{ROW}\n")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "binance_usdm:btcusdt");
        assert_eq!(rows[0].class, InstrumentClass::Perp);
        assert_eq!(rows[0].fee_venue_byte(), 1);
        assert_eq!(rows[0].mintick_1e6, 100_000);
    }

    #[test]
    fn universe_refuses_a_short_row_a_bad_key_and_a_bad_fee_venue() {
        assert!(matches!(parse_universe("a:b\tc\n"), Err(PineErr::Data(_))));
        assert!(matches!(
            parse_universe(&ROW.replace("binance_usdm:btcusdt\t", "BTC\t")),
            Err(PineErr::Data(_))
        ));
        assert!(matches!(
            parse_universe(&ROW.replace("\tbn\t", "\tnope\t")),
            Err(PineErr::Data(_))
        ));
        assert!(matches!(parse_universe("# only\n"), Err(PineErr::Data(_))));
    }
}
