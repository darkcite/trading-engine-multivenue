// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # xsd — the cross-sectional member's parameter artifact (XSD-2)
//!
//! Parses `~/multivenue/xsd.toml` (or `--xsd <path>`): the operator's
//! knobs for `strategy-xsd` (statarb doc 08 §3.1), every one an INTEGER
//! in the member's own fixed point (×1e9 for z thresholds and bps,
//! ×1e6 USD for sizes, seconds for durations). The table, the seed and
//! the state are separate TSV artifacts (`xsd-table.tsv`,
//! `xsd-seed.tsv`, `xsd-state.tsv`) read by the cli's boot module.
//!
//! **BOOT/OFFLINE DOCTRINE:** runs once at process boot; allocations
//! are fine. The grammar is the `icdp.toml` TOML subset (one section,
//! `key = integer`, comments, no floats, no strings):
//!
//! ```toml
//! [xsd]
//! z_window_h = 720            # rolling z window, hours (40..=2160)
//! z_enter_1e9 = 3000000000    # 3.0
//! z_exit_1e9 = 0              # 0.0
//! z_stop_1e9 = 5000000000     # 5.0 (> z_enter)
//! consensus = 1               # partners that must agree (1..=3)
//! grid_n = 1                  # grid depth (1 = no scale-in; ≤ 8)
//! grid_step_1e9 = 500000000   # 0.5 per rung
//! max_hold_s = 864000         # 240 h
//! cooldown_s = 3600           # 1 h = the research's t = x + 1
//! ttl_s = 300                 # IoC ttl
//! position_usd_1e6 = 1000000000   # $1,000 per unit
//! max_positions = 82
//! max_gross_usd_1e6 = 100000000000 # $100,000
//! direction = 1               # +1 momentum, -1 revert
//! slip_1e9 = 0                # limit offset from the touch, bps (optional)
//! ```
//!
//! Durations must be whole hours where the member counts hours
//! (`max_hold_s`, `cooldown_s`); `ttl_s` is any positive second count.
//! Unknown keys, missing keys, duplicate keys, a second section and
//! every range violation are FATAL — the member re-validates the
//! resolved form, but a malformed file must never get that far.

use std::path::Path;

use super::icdp::{parse_value, strip_comment, Value};

/// Parse / load failure (message names the line where possible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XsdError(pub String);

impl ::core::fmt::Display for XsdError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        write!(f, "xsd.toml: {}", self.0)
    }
}

impl std::error::Error for XsdError {}

/// The parsed artifact — integers in the file's own units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XsdFile {
    /// Rolling z window, hours.
    pub z_window_h: u32,
    /// Entry threshold ×1e9.
    pub z_enter_1e9: i64,
    /// Revert exit threshold ×1e9.
    pub z_exit_1e9: i64,
    /// Stop threshold ×1e9.
    pub z_stop_1e9: i64,
    /// Partners that must agree.
    pub consensus: u8,
    /// Grid depth.
    pub grid_n: u8,
    /// Grid rung step ×1e9.
    pub grid_step_1e9: i64,
    /// Max hold, hours.
    pub max_hold_h: u32,
    /// Cooldown after an exit, hours.
    pub cooldown_h: u32,
    /// IoC ttl, seconds.
    pub ttl_s: u32,
    /// Notional per unit, USD ×1e6.
    pub position_usd_1e6: i64,
    /// Max simultaneously entered targets.
    pub max_positions: u16,
    /// Book cap USD ×1e6.
    pub max_gross_usd_1e6: i64,
    /// `+1` momentum, `−1` revert.
    pub direction: i8,
    /// Limit offset from the touch, bps ×1e9.
    pub slip_1e9: i64,
}

/// Default location beside `universe.toml`.
pub fn default_xsd_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/xsd.toml")
}

/// Default table location.
pub fn default_xsd_table_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/xsd-table.tsv")
}

/// Default seed location.
pub fn default_xsd_seed_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/xsd-seed.tsv")
}

/// Default state location.
pub fn default_xsd_state_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/xsd-state.tsv")
}

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT artifact it booted with.
pub fn load(path: &Path) -> Result<(XsdFile, Vec<u8>), XsdError> {
    let bytes = std::fs::read(path).map_err(|e| XsdError(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|_| XsdError(format!("{}: not UTF-8", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

const KEYS: [&str; 15] = [
    "z_window_h",
    "z_enter_1e9",
    "z_exit_1e9",
    "z_stop_1e9",
    "consensus",
    "grid_n",
    "grid_step_1e9",
    "max_hold_s",
    "cooldown_s",
    "ttl_s",
    "position_usd_1e6",
    "max_positions",
    "max_gross_usd_1e6",
    "direction",
    "slip_1e9",
];
/// Keys that may be absent (their default is 0).
const OPTIONAL: [&str; 1] = ["slip_1e9"];
const HOUR_S: i64 = 3600;

fn get(kv: &[(String, i64, usize)], key: &str) -> Result<i64, XsdError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, v, _)) => Ok(*v),
        None if OPTIONAL.contains(&key) => Ok(0),
        None => Err(XsdError(format!("missing [xsd] {key}"))),
    }
}

fn range(v: i64, lo: i64, hi: i64, key: &str) -> Result<i64, XsdError> {
    if v < lo || v > hi {
        return Err(XsdError(format!("{key} = {v} outside [{lo}, {hi}]")));
    }
    Ok(v)
}

fn whole_hours(v: i64, key: &str) -> Result<u32, XsdError> {
    if v < 0 || v % HOUR_S != 0 {
        return Err(XsdError(format!("{key} = {v} must be a non-negative whole number of hours")));
    }
    range(v / HOUR_S, 0, u32::MAX as i64, key).map(|h| h as u32)
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<XsdFile, XsdError> {
    let mut in_xsd = false;
    let mut seen = false;
    let mut kv: Vec<(String, i64, usize)> = Vec::new();
    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if line == "[xsd]" {
            if seen {
                return Err(XsdError(format!("line {ln}: duplicate [xsd] section")));
            }
            seen = true;
            in_xsd = true;
            continue;
        }
        if line.starts_with('[') {
            return Err(XsdError(format!("line {ln}: unknown section `{line}`")));
        }
        if !in_xsd {
            return Err(XsdError(format!("line {ln}: key outside [xsd]")));
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| XsdError(format!("line {ln}: expected `key = value`")))?;
        let key = key.trim();
        if !KEYS.contains(&key) {
            return Err(XsdError(format!("line {ln}: unknown [xsd] key `{key}`")));
        }
        if kv.iter().any(|(k, _, _)| k == key) {
            return Err(XsdError(format!("line {ln}: duplicate key `{key}`")));
        }
        let v = match parse_value(val, ln).map_err(|e| XsdError(e.0))? {
            Value::Int(v) => v,
            _ => return Err(XsdError(format!("line {ln}: `{key}` must be an integer"))),
        };
        kv.push((key.to_owned(), v, ln));
    }
    if !seen {
        return Err(XsdError("missing [xsd] section".to_owned()));
    }
    let z_window_h = range(get(&kv, "z_window_h")?, 40, 2160, "z_window_h")? as u32;
    let z_enter_1e9 = range(get(&kv, "z_enter_1e9")?, 1, i64::MAX, "z_enter_1e9")?;
    let z_exit_1e9 = range(get(&kv, "z_exit_1e9")?, 0, i64::MAX, "z_exit_1e9")?;
    let z_stop_1e9 = get(&kv, "z_stop_1e9")?;
    if z_stop_1e9 <= z_enter_1e9 {
        return Err(XsdError(format!("z_stop_1e9 = {z_stop_1e9} must exceed z_enter_1e9 = {z_enter_1e9}")));
    }
    let consensus = range(get(&kv, "consensus")?, 1, 3, "consensus")? as u8;
    let grid_n = range(get(&kv, "grid_n")?, 1, 8, "grid_n")? as u8;
    let grid_step_1e9 = range(get(&kv, "grid_step_1e9")?, 0, i64::MAX, "grid_step_1e9")?;
    if grid_n > 1 && grid_step_1e9 == 0 {
        return Err(XsdError("grid_step_1e9 must be positive when grid_n > 1".to_owned()));
    }
    let max_hold_h = whole_hours(get(&kv, "max_hold_s")?, "max_hold_s")?;
    if max_hold_h == 0 {
        return Err(XsdError("max_hold_s must be at least one hour".to_owned()));
    }
    let cooldown_h = whole_hours(get(&kv, "cooldown_s")?, "cooldown_s")?;
    let ttl_s = range(get(&kv, "ttl_s")?, 1, u32::MAX as i64, "ttl_s")? as u32;
    let position_usd_1e6 = range(get(&kv, "position_usd_1e6")?, 1, i64::MAX, "position_usd_1e6")?;
    let max_positions = range(get(&kv, "max_positions")?, 1, u16::MAX as i64, "max_positions")? as u16;
    let max_gross_usd_1e6 = range(get(&kv, "max_gross_usd_1e6")?, 1, i64::MAX, "max_gross_usd_1e6")?;
    let direction = get(&kv, "direction")?;
    if direction != 1 && direction != -1 {
        return Err(XsdError(format!("direction = {direction} must be 1 or -1")));
    }
    let slip_1e9 = range(get(&kv, "slip_1e9")?, 0, i64::MAX, "slip_1e9")?;
    Ok(XsdFile {
        z_window_h,
        z_enter_1e9,
        z_exit_1e9,
        z_stop_1e9,
        consensus,
        grid_n,
        grid_step_1e9,
        max_hold_h,
        cooldown_h,
        ttl_s,
        position_usd_1e6,
        max_positions,
        max_gross_usd_1e6,
        direction: direction as i8,
        slip_1e9,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "\
# xsd.toml — the operating point (doc 07 §3.4, R7 grid_n = 1)
[xsd]
z_window_h = 720
z_enter_1e9 = 3000000000   # 3.0
z_exit_1e9 = 0
z_stop_1e9 = 5000000000
consensus = 1
grid_n = 1
grid_step_1e9 = 500000000
max_hold_s = 864000
cooldown_s = 3600
ttl_s = 300
position_usd_1e6 = 1000000000
max_positions = 82
max_gross_usd_1e6 = 100000000000
direction = 1
";

    #[test]
    fn parses_the_operating_point() {
        let f = parse(EXAMPLE).expect("parse");
        assert_eq!(
            f,
            XsdFile {
                z_window_h: 720,
                z_enter_1e9: 3_000_000_000,
                z_exit_1e9: 0,
                z_stop_1e9: 5_000_000_000,
                consensus: 1,
                grid_n: 1,
                grid_step_1e9: 500_000_000,
                max_hold_h: 240,
                cooldown_h: 1,
                ttl_s: 300,
                position_usd_1e6: 1_000_000_000,
                max_positions: 82,
                max_gross_usd_1e6: 100_000_000_000,
                direction: 1,
                slip_1e9: 0,
            }
        );
        // The optional key lands when present.
        let f = parse(&format!("{EXAMPLE}slip_1e9 = 1000000000\n")).unwrap();
        assert_eq!(f.slip_1e9, 1_000_000_000);
    }

    fn expect_err(src: &str, needle: &str) {
        let e = parse(src).expect_err("must refuse");
        assert!(e.0.contains(needle), "{} lacks {needle:?}", e.0);
    }

    #[test]
    fn refuses_every_malformation() {
        expect_err("", "missing [xsd]");
        expect_err("z_window_h = 720\n", "outside [xsd]");
        expect_err(&EXAMPLE.replace("[xsd]", "[xsd]\n[xsd]"), "duplicate [xsd]");
        expect_err(&format!("{EXAMPLE}[other]\n"), "unknown section");
        expect_err(&format!("{EXAMPLE}foo = 1\n"), "unknown [xsd] key");
        expect_err(&format!("{EXAMPLE}grid_n = 2\n"), "duplicate key");
        expect_err(&EXAMPLE.replace("z_window_h = 720", "z_window_h = 39"), "z_window_h");
        expect_err(&EXAMPLE.replace("z_window_h = 720", "z_window_h = 2161"), "z_window_h");
        expect_err(&EXAMPLE.replace("z_stop_1e9 = 5000000000", "z_stop_1e9 = 3000000000"), "z_stop_1e9");
        expect_err(&EXAMPLE.replace("z_enter_1e9 = 3000000000", "z_enter_1e9 = 0"), "z_enter_1e9");
        expect_err(&EXAMPLE.replace("consensus = 1", "consensus = 4"), "consensus");
        expect_err(&EXAMPLE.replace("grid_n = 1", "grid_n = 9"), "grid_n");
        expect_err(
            &EXAMPLE
                .replace("grid_n = 1", "grid_n = 3")
                .replace("grid_step_1e9 = 500000000", "grid_step_1e9 = 0"),
            "grid_step_1e9",
        );
        expect_err(&EXAMPLE.replace("max_hold_s = 864000", "max_hold_s = 864001"), "whole number of hours");
        expect_err(&EXAMPLE.replace("max_hold_s = 864000", "max_hold_s = 0"), "max_hold_s");
        expect_err(&EXAMPLE.replace("cooldown_s = 3600", "cooldown_s = 1800"), "cooldown_s");
        expect_err(&EXAMPLE.replace("ttl_s = 300", "ttl_s = 0"), "ttl_s");
        expect_err(&EXAMPLE.replace("position_usd_1e6 = 1000000000", "position_usd_1e6 = 0"), "position_usd_1e6");
        expect_err(&EXAMPLE.replace("max_positions = 82", "max_positions = 0"), "max_positions");
        expect_err(&EXAMPLE.replace("direction = 1", "direction = 2"), "direction");
        expect_err(&EXAMPLE.replace("direction = 1", "direction = \"long\""), "must be an integer");
        expect_err(&EXAMPLE.replace("ttl_s = 300", ""), "missing [xsd] ttl_s");
        expect_err(&format!("{EXAMPLE}slip_1e9 = -1\n"), "slip_1e9");
        expect_err(&EXAMPLE.replace("z_exit_1e9 = 0", "z_exit_1e9 = 1.5"), "bad integer");
    }

    #[test]
    fn load_returns_the_exact_bytes_for_hashing() {
        let dir = std::env::temp_dir().join(format!("xsd-toml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("xsd.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        let (f, bytes) = load(&path).expect("load");
        assert_eq!(f.max_hold_h, 240);
        assert_eq!(bytes, EXAMPLE.as_bytes());
        assert!(load(&dir.join("absent.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
