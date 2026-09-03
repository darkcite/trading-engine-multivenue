// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # icdp — the intrabar strategy's parameter artifact (ICDP I5)
//!
//! Parses `~/multivenue/icdp.toml` (or `--icdp <path>`) — a DATA
//! artifact generated offline by the research vault's params one-shot
//! from the fitted arrays (never hand-authored; it is regenerated, not
//! edited). Every number is an INTEGER in the strategy's own fixed
//! point (×1e9 for features/weights/thresholds/bps, ×1e6 USD for the
//! notional), so nothing here parses floats and the Rust composite is
//! bit-identical to the Python fit that produced the file.
//!
//! **BOOT/OFFLINE DOCTRINE:** runs once at process boot; allocations
//! are fine. The grammar is a deliberate TOML SUBSET, hand-parsed like
//! `universe.rs` (house rule: we own every parser):
//!
//! ```toml
//! # generated 2026-09-03 by the vault one-shot; sha256 logged at boot
//! [icdp]
//! tf_ms = 15000            # bar length
//! delta_ms = 3750          # decision offset (< tf_ms)
//!
//! [[instrument]]
//! descriptor = "binance:btcusdt"
//! mu = [12, -3, 0, 5, 1]           # ×1e9, feature order r_early, imb, micro, ofi, r_prev
//! inv_sd = [1000000000, ...]       # ×1e9 (1e9 / sd)
//! w = [500000000, ...]             # ×1e9
//! b = 0                            # ×1e9
//! thr = 400000000                  # ×1e9 (|s| must exceed)
//! notional_usd_1e6 = 10000000000   # $10,000
//! spread_cap_1e9 = 2000000000      # 2 bps
//! entry_slip_1e9 = 1000000000      # 1 bps
//! exit_slip_1e9 = 2000000000       # 2 bps
//! ```
//!
//! Values: signed decimal integers (optional `-`, ≤ 19 digits, no `_`),
//! `[a, b, …]` arrays of them (single line), quoted strings without
//! escapes. Comments (`#`) anywhere outside a string. Unknown
//! sections/keys, missing keys, duplicate keys, wrong array lengths,
//! more than [`ICDP_MAX_INSTRUMENTS`] instruments — all FATAL.

use std::path::Path;

/// Feature count the artifact must carry per instrument.
pub const ICDP_NF: usize = 5;
/// Instrument cap (mirrors the strategy table).
pub const ICDP_MAX_INSTRUMENTS: usize = 32;

/// Parse / load failure (message names the line where possible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcdpError(pub String);

impl ::core::fmt::Display for IcdpError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        write!(f, "icdp.toml: {}", self.0)
    }
}

impl std::error::Error for IcdpError {}

/// One `[[instrument]]` block, unresolved (descriptor string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcdpInstrument {
    /// §9.4 descriptor (`venue:name`) resolved by the bin against the
    /// boot universe.
    pub descriptor: String,
    /// Feature means ×1e9.
    pub mu: [i64; ICDP_NF],
    /// Inverse sds ×1e9 (each > 0).
    pub inv_sd: [i64; ICDP_NF],
    /// Weights ×1e9.
    pub w: [i64; ICDP_NF],
    /// Intercept ×1e9.
    pub b: i64,
    /// Threshold ×1e9 (> 0).
    pub thr: i64,
    /// Notional USD ×1e6 (> 0).
    pub notional_usd_1e6: i64,
    /// Spread cap bps ×1e9 (> 0).
    pub spread_cap_1e9: i64,
    /// Entry slip bps ×1e9 (≥ 0).
    pub entry_slip_1e9: i64,
    /// Exit slip bps ×1e9 (≥ 0).
    pub exit_slip_1e9: i64,
}

/// The parsed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcdpFile {
    /// Bar length ms (> 0).
    pub tf_ms: u64,
    /// Decision offset ms (< tf_ms).
    pub delta_ms: u64,
    /// Instruments in file order.
    pub instruments: Vec<IcdpInstrument>,
}

/// Default location beside `universe.toml`.
pub fn default_icdp_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/icdp.toml")
}

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT artifact it booted with.
pub fn load(path: &Path) -> Result<(IcdpFile, Vec<u8>), IcdpError> {
    let bytes = std::fs::read(path)
        .map_err(|e| IcdpError(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|_| IcdpError(format!("{}: not UTF-8", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Str(String),
    Int(i64),
    Ints(Vec<i64>),
}

/// Strip a trailing comment (quote-aware) and surrounding whitespace.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'#' if !in_str => return line[..i].trim(),
            _ => {}
        }
        i += 1;
    }
    line.trim()
}

fn parse_int(s: &str, ln: usize) -> Result<i64, IcdpError> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t),
    };
    if digits.is_empty() || digits.len() > 19 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(IcdpError(format!("line {ln}: bad integer `{t}`")));
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(IcdpError(format!("line {ln}: leading zero in `{t}`")));
    }
    let mut v: i64 = 0;
    for b in digits.bytes() {
        v = v
            .checked_mul(10)
            .and_then(|x| x.checked_add((b - b'0') as i64))
            .ok_or_else(|| IcdpError(format!("line {ln}: integer overflow `{t}`")))?;
    }
    Ok(if neg { -v } else { v })
}

fn parse_value(s: &str, ln: usize) -> Result<Value, IcdpError> {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix('"') {
        let body = inner
            .strip_suffix('"')
            .ok_or_else(|| IcdpError(format!("line {ln}: unterminated string")))?;
        if body.contains('"') || body.contains('\\') {
            return Err(IcdpError(format!("line {ln}: strings carry no escapes")));
        }
        return Ok(Value::Str(body.to_owned()));
    }
    if let Some(inner) = t.strip_prefix('[') {
        let body = inner
            .strip_suffix(']')
            .ok_or_else(|| IcdpError(format!("line {ln}: unterminated array")))?;
        let mut out = Vec::new();
        for part in body.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue; // trailing comma
            }
            out.push(parse_int(p, ln)?);
        }
        return Ok(Value::Ints(out));
    }
    Ok(Value::Int(parse_int(t, ln)?))
}

fn take_ints(kv: &[(String, Value, usize)], key: &str, ln: usize) -> Result<[i64; ICDP_NF], IcdpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Ints(v), l)) => {
            if v.len() != ICDP_NF {
                return Err(IcdpError(format!(
                    "line {l}: `{key}` must carry exactly {ICDP_NF} integers"
                )));
            }
            let mut out = [0i64; ICDP_NF];
            out.copy_from_slice(v);
            Ok(out)
        }
        Some((_, _, l)) => Err(IcdpError(format!("line {l}: `{key}` must be an integer array"))),
        None => Err(IcdpError(format!("instrument at line {ln}: missing `{key}`"))),
    }
}

fn take_int(kv: &[(String, Value, usize)], key: &str, ln: usize) -> Result<i64, IcdpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(IcdpError(format!("line {l}: `{key}` must be an integer"))),
        None => Err(IcdpError(format!("instrument at line {ln}: missing `{key}`"))),
    }
}

fn take_str(kv: &[(String, Value, usize)], key: &str, ln: usize) -> Result<String, IcdpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Str(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(IcdpError(format!("line {l}: `{key}` must be a string"))),
        None => Err(IcdpError(format!("instrument at line {ln}: missing `{key}`"))),
    }
}

const INSTRUMENT_KEYS: [&str; 10] = [
    "descriptor",
    "mu",
    "inv_sd",
    "w",
    "b",
    "thr",
    "notional_usd_1e6",
    "spread_cap_1e9",
    "entry_slip_1e9",
    "exit_slip_1e9",
];

fn finish_instrument(kv: &[(String, Value, usize)], ln: usize) -> Result<IcdpInstrument, IcdpError> {
    for (k, _, l) in kv {
        if !INSTRUMENT_KEYS.contains(&k.as_str()) {
            return Err(IcdpError(format!("line {l}: unknown instrument key `{k}`")));
        }
    }
    let inst = IcdpInstrument {
        descriptor: take_str(kv, "descriptor", ln)?,
        mu: take_ints(kv, "mu", ln)?,
        inv_sd: take_ints(kv, "inv_sd", ln)?,
        w: take_ints(kv, "w", ln)?,
        b: take_int(kv, "b", ln)?,
        thr: take_int(kv, "thr", ln)?,
        notional_usd_1e6: take_int(kv, "notional_usd_1e6", ln)?,
        spread_cap_1e9: take_int(kv, "spread_cap_1e9", ln)?,
        entry_slip_1e9: take_int(kv, "entry_slip_1e9", ln)?,
        exit_slip_1e9: take_int(kv, "exit_slip_1e9", ln)?,
    };
    if inst.descriptor.is_empty() {
        return Err(IcdpError(format!("instrument at line {ln}: empty descriptor")));
    }
    if inst.inv_sd.iter().any(|v| *v <= 0) {
        return Err(IcdpError(format!("instrument at line {ln}: inv_sd must be > 0")));
    }
    if inst.thr <= 0 || inst.notional_usd_1e6 <= 0 || inst.spread_cap_1e9 <= 0 {
        return Err(IcdpError(format!(
            "instrument at line {ln}: thr / notional / spread cap must be > 0"
        )));
    }
    if inst.entry_slip_1e9 < 0 || inst.exit_slip_1e9 < 0 {
        return Err(IcdpError(format!("instrument at line {ln}: slips must be ≥ 0")));
    }
    Ok(inst)
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<IcdpFile, IcdpError> {
    #[derive(PartialEq, Eq)]
    enum Sec {
        None,
        Icdp,
        Instrument(usize),
    }
    let mut sec = Sec::None;
    let mut tf_ms: Option<u64> = None;
    let mut delta_ms: Option<u64> = None;
    let mut seen_icdp = false;
    let mut cur: Vec<(String, Value, usize)> = Vec::new();
    let mut instruments: Vec<IcdpInstrument> = Vec::new();

    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if line == "[[instrument]]" {
            if let Sec::Instrument(start) = sec {
                instruments.push(finish_instrument(&cur, start)?);
                cur.clear();
            }
            if instruments.len() >= ICDP_MAX_INSTRUMENTS {
                return Err(IcdpError(format!(
                    "line {ln}: more than {ICDP_MAX_INSTRUMENTS} instruments"
                )));
            }
            sec = Sec::Instrument(ln);
            continue;
        }
        if line == "[icdp]" {
            if let Sec::Instrument(start) = sec {
                instruments.push(finish_instrument(&cur, start)?);
                cur.clear();
            }
            if seen_icdp {
                return Err(IcdpError(format!("line {ln}: duplicate [icdp] section")));
            }
            seen_icdp = true;
            sec = Sec::Icdp;
            continue;
        }
        if line.starts_with('[') {
            return Err(IcdpError(format!("line {ln}: unknown section `{line}`")));
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| IcdpError(format!("line {ln}: expected `key = value`")))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(IcdpError(format!("line {ln}: bad key `{key}`")));
        }
        let value = parse_value(val, ln)?;
        match sec {
            Sec::None => return Err(IcdpError(format!("line {ln}: key outside any section"))),
            Sec::Icdp => match (key, value) {
                ("tf_ms", Value::Int(v)) if v > 0 => {
                    if tf_ms.replace(v as u64).is_some() {
                        return Err(IcdpError(format!("line {ln}: duplicate tf_ms")));
                    }
                }
                ("delta_ms", Value::Int(v)) if v >= 0 => {
                    if delta_ms.replace(v as u64).is_some() {
                        return Err(IcdpError(format!("line {ln}: duplicate delta_ms")));
                    }
                }
                ("tf_ms", _) | ("delta_ms", _) => {
                    return Err(IcdpError(format!("line {ln}: `{key}` must be a non-negative integer")))
                }
                _ => return Err(IcdpError(format!("line {ln}: unknown [icdp] key `{key}`"))),
            },
            Sec::Instrument(_) => {
                if cur.iter().any(|(k, _, _)| k == key) {
                    return Err(IcdpError(format!("line {ln}: duplicate key `{key}`")));
                }
                cur.push((key.to_owned(), value, ln));
            }
        }
    }
    if let Sec::Instrument(start) = sec {
        instruments.push(finish_instrument(&cur, start)?);
    }
    let tf_ms = tf_ms.ok_or_else(|| IcdpError("missing [icdp] tf_ms".to_owned()))?;
    let delta_ms = delta_ms.ok_or_else(|| IcdpError("missing [icdp] delta_ms".to_owned()))?;
    if delta_ms >= tf_ms {
        return Err(IcdpError("delta_ms must be < tf_ms".to_owned()));
    }
    if instruments.is_empty() {
        return Err(IcdpError("no [[instrument]] blocks".to_owned()));
    }
    let mut i = 0usize;
    while i < instruments.len() {
        let mut j = 0usize;
        while j < i {
            if instruments[j].descriptor == instruments[i].descriptor {
                return Err(IcdpError(format!(
                    "duplicate instrument `{}`",
                    instruments[i].descriptor
                )));
            }
            j += 1;
        }
        i += 1;
    }
    Ok(IcdpFile {
        tf_ms,
        delta_ms,
        instruments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r##"
# generated by the vault one-shot
[icdp]
tf_ms = 15000   # bar
delta_ms = 3750

[[instrument]]
descriptor = "binance:btcusdt"   # the "#anchor" is not a comment
mu = [12, -3, 0, 5, 1]
inv_sd = [1000000000, 2000000000, 3000000000, 4000000000, 5000000000]
w = [500000000, -250000000, 0, 125000000, -1]
b = -7
thr = 400000000
notional_usd_1e6 = 10000000000
spread_cap_1e9 = 2000000000
entry_slip_1e9 = 1000000000
exit_slip_1e9 = 2000000000

[[instrument]]
descriptor = "okx:BTC-USDT"
mu = [0, 0, 0, 0, 0,]
inv_sd = [1, 1, 1, 1, 1]
w = [1, 1, 1, 1, 1]
b = 0
thr = 1
notional_usd_1e6 = 1
spread_cap_1e9 = 1
entry_slip_1e9 = 0
exit_slip_1e9 = 0
"##;

    #[test]
    fn parses_the_generated_shape() {
        let f = parse(GOOD).expect("parse");
        assert_eq!(f.tf_ms, 15_000);
        assert_eq!(f.delta_ms, 3_750);
        assert_eq!(f.instruments.len(), 2);
        let a = &f.instruments[0];
        assert_eq!(a.descriptor, "binance:btcusdt");
        assert_eq!(a.mu, [12, -3, 0, 5, 1]);
        assert_eq!(a.inv_sd[4], 5_000_000_000);
        assert_eq!(a.w[1], -250_000_000);
        assert_eq!(a.b, -7);
        assert_eq!(a.thr, 400_000_000);
        assert_eq!(a.notional_usd_1e6, 10_000_000_000);
        assert_eq!(f.instruments[1].mu, [0; 5], "trailing comma tolerated");
    }

    #[test]
    fn refuses_every_malformation() {
        let bad = [
            ("", "missing"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 10\n[[instrument]]\ndescriptor = \"x\"\nmu=[1,1,1,1,1]\ninv_sd=[1,1,1,1,1]\nw=[1,1,1,1,1]\nb=0\nthr=1\nnotional_usd_1e6=1\nspread_cap_1e9=1\nentry_slip_1e9=0\nexit_slip_1e9=0\n", "delta_ms must be < tf_ms"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n", "no [[instrument]]"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[[instrument]]\ndescriptor = \"x\"\nmu=[1,1,1,1]\ninv_sd=[1,1,1,1,1]\nw=[1,1,1,1,1]\nb=0\nthr=1\nnotional_usd_1e6=1\nspread_cap_1e9=1\nentry_slip_1e9=0\nexit_slip_1e9=0\n", "exactly 5"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[[instrument]]\ndescriptor = \"x\"\nmu=[1,1,1,1,1]\ninv_sd=[0,1,1,1,1]\nw=[1,1,1,1,1]\nb=0\nthr=1\nnotional_usd_1e6=1\nspread_cap_1e9=1\nentry_slip_1e9=0\nexit_slip_1e9=0\n", "inv_sd must be > 0"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[[instrument]]\ndescriptor = \"x\"\nmu=[1,1,1,1,1]\ninv_sd=[1,1,1,1,1]\nw=[1,1,1,1,1]\nb=0\nthr=1\nnotional_usd_1e6=1\nspread_cap_1e9=1\nentry_slip_1e9=0\nexit_slip_1e9=0\nextra=1\n", "unknown instrument key"),
            ("[icdp]\ntf_ms = 1.5\ndelta_ms = 1\n", "bad integer"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[other]\n", "unknown section"),
            ("tf_ms = 10\n", "outside any section"),
            ("[icdp]\ntf_ms = 10\ntf_ms = 10\n", "duplicate tf_ms"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[[instrument]]\ndescriptor = \"x\"\ndescriptor = \"y\"\n", "duplicate key"),
            ("[icdp]\ntf_ms = 10\ndelta_ms = 1\n[[instrument]]\ndescriptor = \"a\\\"b\"\n", "no escapes"),
        ];
        for (src, needle) in bad {
            let e = parse(src).expect_err(needle);
            assert!(e.0.contains(needle), "{needle}: got {e}");
        }
        // duplicate descriptors
        let dup = GOOD.replace("okx:BTC-USDT", "binance:btcusdt");
        assert!(parse(&dup).unwrap_err().0.contains("duplicate instrument"));
    }

    #[test]
    fn load_returns_the_exact_bytes_for_hashing() {
        let dir = std::env::temp_dir().join(format!("icdp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("icdp.toml");
        std::fs::write(&p, GOOD).unwrap();
        let (f, bytes) = load(&p).expect("load");
        assert_eq!(f.instruments.len(), 2);
        assert_eq!(bytes, GOOD.as_bytes());
        assert!(load(&dir.join("absent.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
