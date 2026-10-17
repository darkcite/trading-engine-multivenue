// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The conformance chart (impl-06 §4.1.4): `pine_spec::chart` (no second reader of impl-00's
//! `chart.json`), the `tests/conformance/syminfo.tsv` reader, and the `ChartInfo` / `CompileOpts`
//! builders. LOAD-TIME ONLY: allocates freely.

use std::path::Path;

use pine_ir::{ChartInfo, CompileLimits, CompileOpts, Dialect, ExecMode};

pub use pine_spec::chart as spec;

/// `chart.json`, relative to the workspace root.
pub const CHART_JSON: &str = "tests/conformance/chart.json";
/// `syminfo.tsv`, relative to the workspace root.
pub const SYMINFO_TSV: &str = "tests/conformance/syminfo.tsv";

/// `STOP_MS = TO_MS − 2·TF_MS` (§4.1.3): SYN-T's last advancing bar ends here.
pub const STOP_MS: i64 = spec::TO_MS - 2 * spec::TF_MS;

/// SHA-256 of `chart.json` as it is on disk now (I1 re-hashes it, so a stale build cannot mask an
/// edit).
pub fn chart_json_sha256(root: &Path) -> std::io::Result<[u8; 32]> {
    Ok(core_crypto::sha256(&std::fs::read(root.join(CHART_JSON))?))
}

/// What `CompileOpts` needs and `chart.json` does not (§4.1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Syminfo {
    /// `syminfo.mintick` ×1e6.
    pub mintick_1e6: i64,
    /// `syminfo.pointvalue` ×1e6.
    pub pointvalue_1e6: i64,
    /// `syminfo.ticker`.
    pub ticker: String,
    /// `syminfo.basecurrency`.
    pub basecurrency: String,
    /// `syminfo.currency`.
    pub currency: String,
    /// `syminfo.type`.
    pub type_: String,
    /// `ChartInfo::venue` (`1` = `VenueId::Binance`).
    pub venue: u8,
    /// `TvCfg::day_offset_s`.
    pub day_offset_s: i32,
}

/// The `syminfo.tsv` keys, in file order.
pub const SYMINFO_KEYS: [&str; 8] = [
    "mintick_1e6",
    "pointvalue_1e6",
    "ticker",
    "basecurrency",
    "currency",
    "type",
    "venue",
    "day_offset_s",
];

/// Parse `syminfo.tsv`.
///
/// `pin:06-syminfo-format` — §4.1.4 names the keys, not the layout: one `key TAB value` line per
/// key, in the table's order, LF-terminated, no header; anything else is an error naming the line.
pub fn parse_syminfo(text: &str) -> Result<Syminfo, String> {
    let lines: Vec<&str> = text
        .strip_suffix('\n')
        .ok_or("syminfo.tsv ends with LF")?
        .split('\n')
        .collect();
    if lines.len() != SYMINFO_KEYS.len() {
        return Err(format!(
            "{} line(s), want {}",
            lines.len(),
            SYMINFO_KEYS.len()
        ));
    }
    let mut v: Vec<&str> = Vec::with_capacity(8);
    for (i, (l, k)) in lines.iter().zip(SYMINFO_KEYS).enumerate() {
        match l.split_once('\t') {
            Some((key, val)) if key == k && !val.is_empty() && !val.contains('\t') => v.push(val),
            _ => return Err(format!("line {}: want `{k}<TAB><value>`", i + 1)),
        }
    }
    let int = |i: usize| -> Result<i64, String> {
        v[i].parse::<i64>()
            .map_err(|_| format!("line {}: `{}` is not an integer", i + 1, v[i]))
    };
    let mintick_1e6 = int(0)?;
    if mintick_1e6 < 1 {
        return Err("line 1: mintick_1e6 < 1 (ChartInfo refuses a tick finer than 1e-6)".into());
    }
    Ok(Syminfo {
        mintick_1e6,
        pointvalue_1e6: int(1)?,
        ticker: v[2].to_string(),
        basecurrency: v[3].to_string(),
        currency: v[4].to_string(),
        type_: v[5].to_string(),
        venue: u8::try_from(int(6)?).map_err(|_| "line 7: venue is a byte".to_string())?,
        day_offset_s: i32::try_from(int(7)?).map_err(|_| "line 8: day_offset_s".to_string())?,
    })
}

/// `ChartInfo` of the conformance chart: `pine_spec::chart`'s hash and timeframe, `syminfo.tsv`'s
/// numbers, `qty_step_1e6 = 0` and `min_notional_1e6 = 0` (TradingView's tester has neither).
pub fn chart_info(si: &Syminfo) -> ChartInfo {
    ChartInfo {
        symbol_hash: spec::SYMBOL_HASH,
        timeframe_s: spec::TIMEFRAME_S,
        venue: si.venue,
        _pad: [0; 3],
        mintick_1e6: si.mintick_1e6,
        pointvalue_1e6: si.pointvalue_1e6,
        qty_step_1e6: 0,
        min_notional_1e6: 0,
    }
}

/// `CompileOpts::ticker_map` (§4.1.4): `SYMBOL` → `SYMBOL_KEY`, and `BINANCE:ETHUSDT.P` →
/// `binance_usdm:ethusdt` (D047, the same class rule).
pub fn ticker_map() -> Vec<(String, String)> {
    vec![
        (spec::SYMBOL.to_string(), spec::SYMBOL_KEY.to_string()),
        (
            "BINANCE:ETHUSDT.P".to_string(),
            "binance_usdm:ethusdt".to_string(),
        ),
    ]
}

/// The `CompileOpts` every fixture compiles under. `live_bound` is `false` except for the I12
/// `live_refuse` check; `limits` is `run::DEFAULT_LIMITS` once T15 lands.
pub fn compile_opts(
    si: &Syminfo,
    dialect: Dialect,
    mode: ExecMode,
    live_bound: bool,
    limits: CompileLimits,
    source_sha256: [u8; 32],
) -> CompileOpts {
    CompileOpts {
        dialect,
        mode,
        live_bound,
        chart: chart_info(si),
        symbol_key: spec::SYMBOL_KEY.to_string(),
        syminfo_strs: [
            si.ticker.clone(),
            si.basecurrency.clone(),
            si.currency.clone(),
            si.type_.clone(),
        ],
        limits,
        inputs: Vec::new(),
        ticker_map: ticker_map(),
        source_sha256,
    }
}

/// `core_crypto::sha256` of a file's bytes (I2 `script_sha256`, §4.1.5's one SHA-256).
pub fn sha256_file(p: &Path) -> std::io::Result<[u8; 32]> {
    Ok(core_crypto::sha256(&std::fs::read(p)?))
}
