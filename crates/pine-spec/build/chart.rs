// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! The `tests/conformance/chart.json` reader and rules CH1–CH7 (impl-00 §4.10). A malformed
//! file or a wrong key set is CH1.

use crate::json::{self, Node, Val};

/// One hour in milliseconds.
pub const HOUR_MS: i64 = 3_600_000;

/// The keys of `chart.json`, exactly (CH1).
pub const KEYS: [&str; 14] = [
    "schema",
    "frozen_utc",
    "tv_symbol",
    "symbol_key",
    "candles_descriptor",
    "timeframe_tv",
    "timeframe_s",
    "timezone",
    "chart_type",
    "bar_magnifier",
    "from_ms",
    "bars",
    "warmup_bars",
    "note",
];

/// The chart definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chart {
    /// `1`.
    pub schema: u32,
    /// Freeze day, `YYYY-MM-DD`.
    pub frozen_utc: String,
    /// TradingView spelling.
    pub tv_symbol: String,
    /// Canonical key.
    pub symbol_key: String,
    /// `candles.db` descriptor.
    pub candles_descriptor: String,
    /// `"60"`.
    pub timeframe_tv: String,
    /// `3600`.
    pub timeframe_s: u32,
    /// `"Etc/UTC"`.
    pub timezone: String,
    /// `"candles"`.
    pub chart_type: String,
    /// `false`.
    pub bar_magnifier: bool,
    /// Open of the first judged bar, UTC ms.
    pub from_ms: i64,
    /// Judged bars.
    pub bars: u32,
    /// Warm-up bars.
    pub warmup_bars: u32,
}

/// A chart rule violation: `<off>: CH<n>: <what>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartErr {
    /// Byte offset.
    pub off: u32,
    /// Rule number (`CH<n>`).
    pub rule: u8,
    /// What is wrong.
    pub what: String,
}

impl std::fmt::Display for ChartErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: CH{}: {}", self.off, self.rule, self.what)
    }
}

fn err<T>(off: u32, rule: u8, what: impl Into<String>) -> Result<T, ChartErr> {
    Err(ChartErr {
        off,
        rule,
        what: what.into(),
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` → UTC ms of its midnight, if well-formed.
pub fn midnight_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // Round-trip check: rejects 02-30 and friends.
    let (m2, d2) = if m == 12 { (1, 1) } else { (m + 1, 1) };
    let next = days_from_civil(if m == 12 { y + 1 } else { y }, m2, d2);
    if days >= next {
        return None;
    }
    Some(days * 86_400_000)
}

/// Read and check `chart.json` (CH1–CH7).
pub fn read(src: &[u8]) -> Result<Chart, ChartErr> {
    let root = match json::parse(src) {
        Ok(r) => r,
        Err(e) => return err(e.off, 1, format!("malformed JSON: {}", e.what)),
    };
    let fields = match &root.v {
        Val::Obj(f) => f,
        _ => return err(root.off, 1, "expected an object"),
    };
    for (k, v) in fields {
        if !KEYS.contains(&k.as_ref()) {
            return err(v.off, 1, format!("unknown key \"{k}\""));
        }
    }
    for k in KEYS {
        if !fields.iter().any(|(q, _)| q == k) {
            return err(root.off, 1, format!("missing key \"{k}\""));
        }
    }
    let get = |k: &str| -> &Node<'_> {
        match fields.iter().find(|(q, _)| q == k) {
            Some((_, v)) => v,
            None => unreachable!("checked above"),
        }
    };
    let s = |k: &str| -> Result<String, ChartErr> {
        let n = get(k);
        match &n.v {
            Val::Str(t) => Ok(t.to_string()),
            _ => err(n.off, 1, format!("\"{k}\": expected a string")),
        }
    };
    let i = |k: &str| -> Result<(i64, u32), ChartErr> {
        let n = get(k);
        match n.v {
            Val::Int(v) => Ok((v, n.off)),
            _ => err(n.off, 1, format!("\"{k}\": expected an integer")),
        }
    };
    let (schema, so) = i("schema")?;
    if schema != 1 {
        return err(so, 1, format!("schema {schema} is not 1"));
    }
    s("note")?;
    let frozen_utc = s("frozen_utc")?;
    let tv_symbol = s("tv_symbol")?;
    let symbol_key = s("symbol_key")?;
    let candles_descriptor = s("candles_descriptor")?;
    let timeframe_tv = s("timeframe_tv")?;
    let timezone = s("timezone")?;
    let chart_type = s("chart_type")?;
    let bm = get("bar_magnifier");
    let bar_magnifier = match bm.v {
        Val::Bool(b) => b,
        _ => return err(bm.off, 1, "\"bar_magnifier\": expected a boolean"),
    };
    let (tf_s, tfo) = i("timeframe_s")?;
    let (from_ms, fo) = i("from_ms")?;
    let (bars, bo) = i("bars")?;
    let (warm, wo) = i("warmup_bars")?;

    if !pine_ir::is_canonical_symbol_key(symbol_key.as_bytes()) {
        return err(
            get("symbol_key").off,
            2,
            format!("\"{symbol_key}\" is not canonical"),
        );
    }
    if tf_s != 3600 {
        return err(tfo, 3, format!("timeframe_s {tf_s} is not 3600"));
    }
    if timeframe_tv != "60" {
        return err(get("timeframe_tv").off, 3, "timeframe_tv is not \"60\"");
    }
    if from_ms <= 0 || from_ms % HOUR_MS != 0 {
        return err(
            fo,
            4,
            format!("from_ms {from_ms} is not a positive multiple of 3 600 000"),
        );
    }
    if bars % 24 != 0 || !(2160..=4320).contains(&bars) {
        return err(
            bo,
            5,
            format!("bars {bars} is not a multiple of 24 in 2160..=4320"),
        );
    }
    if warm % 24 != 0 || !(720..=u32::MAX as i64).contains(&warm) {
        return err(
            wo,
            5,
            format!("warmup_bars {warm} is not a multiple of 24 and >= 720"),
        );
    }
    if chart_type != "candles" {
        return err(get("chart_type").off, 6, "chart_type is not \"candles\"");
    }
    if bar_magnifier {
        return err(bm.off, 6, "bar_magnifier is not false");
    }
    if timezone != "Etc/UTC" {
        return err(get("timezone").off, 6, "timezone is not \"Etc/UTC\"");
    }
    let fz = match midnight_ms(&frozen_utc) {
        Some(ms) => ms,
        None => {
            return err(
                get("frozen_utc").off,
                7,
                "frozen_utc is not a YYYY-MM-DD date",
            )
        }
    };
    let to = from_ms + bars * HOUR_MS;
    if to > fz - 48 * HOUR_MS {
        return err(
            fo,
            7,
            format!("window end {to} is after frozen_utc midnight minus 48 h"),
        );
    }
    Ok(Chart {
        schema: schema as u32,
        frozen_utc,
        tv_symbol,
        symbol_key,
        candles_descriptor,
        timeframe_tv,
        timeframe_s: tf_s as u32,
        timezone,
        chart_type,
        bar_magnifier,
        from_ms,
        bars: bars as u32,
        warmup_bars: warm as u32,
    })
}
