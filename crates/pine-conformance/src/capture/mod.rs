// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Export in, `.fix` out (impl-06 §3.10, §4.7): the bar-identity hash (§4.7.4, T13), the export
//! format (`format`), the three export shapes (`csv`) and the distiller (`distil`, T18).

pub mod barhash;
pub mod csv;
pub mod distil;
pub mod format;

pub use barhash::{barhash, barhash_tsv, window_rows};
pub use csv::{
    parse_chart_csv, parse_summary_csv, parse_trade_time, parse_trades_csv, ChartExport,
};
pub use distil::{distil, record_paste, CaptureStamp, DistilReport, Paste};
pub use format::{load_format, parse_format, ExportFormat, TradesTimeFormat};
