// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: boot-only; allocation and copying are free here.
//!
//! The Pine member's artifacts (PVM lane, impl-08 §3.3): the contract §3.14 manifest
//! (`~/multivenue/pine/<name>.toml`, impl-08 §4.1.2) and the `[exec.slot.8.pine]` section of
//! `exec.toml` (impl-08 §4.8, re-baselined to slot 8 — pin:08-slot8-rename).
//!
//! Grammar: this crate's TOML subset (`icdp.rs`: decimal integers with no `_`, no leading zero
//! and no float; strings with no escapes; `#` comments), plus `[[script]]`, the three sub-tables
//! `[script.inputs]`, `[script.symbols]`, `[script.provenance]`, and quoted keys inside
//! `[script.symbols]` only. Every refusal names its key and line; nothing defaults (`6004` unknown
//! key, `6005` missing key, `6006` out of range or duplicate, `6012` provenance).
//!
//! Literal readings where impl-08 is silent (each a `pin:08-<slug>`):
//! - pin:08-malformed-value-6006: a value that fails the subset's grammar, or has the wrong type
//!   (an array, an integer where a string is due), is `6006` naming the key — out of the key's
//!   range. Inside `[script.provenance]` it is `6012` (the semicolon-listing case, §4.1.2).
//! - pin:08-provenance-codes: inside `[script.provenance]` a missing key, a malformed value and an
//!   out-of-range value are `6012`; an unknown key is `6004` and a duplicate `6006` (the general
//!   rules, which §4.1.2 does not carve out).
//! - pin:08-venue-range-6006: `venue` outside `exec`'s venue names is `6006` here (its range
//!   column); `6003` (venue parity with the slot) stays a load-time check.
//! - pin:08-empty-manifest-6005: a manifest with no `[[script]]` block is `6005`, key `script`.
//! - pin:08-manifest-io-6005: an unreadable or non-UTF-8 manifest file is `6005`, key `manifest`.
//! - pin:08-symbols-bare-keys: a `[script.symbols]` key "may" be quoted; a bare key there obeys
//!   the general `[a-z0-9_]+`. Its value is a string; canonical-key checks are `pine-ir`'s at load.
//! - pin:08-symbol-lowercase: `symbol`'s "lowercase" is read as "no ASCII uppercase byte".
//! - pin:08-upstream-hex-case: provenance `upstream_sha256` is "64 hex" (either case); the
//!   script's own `sha256` is "64 lowercase hex".
//! - pin:08-slot-default-none: `[exec.slot.8.pine]`'s three Live-required keys stay `None` when
//!   absent (so [`require_live`] can see the absence); their table defaults
//!   ([`EVENT_INSN_BUDGET_DEFAULT`], `accept`, the venue's stale window) apply at the consumer.

use std::path::Path;

use crate::icdp::{parse_value, strip_comment, Value};

/// Contract §3.3: unknown key.
pub const CODE_UNKNOWN_KEY: u16 = 6004;
/// Contract §3.3: missing required key.
pub const CODE_MISSING_KEY: u16 = 6005;
/// Contract §3.3: value out of range (also a duplicate key or block key).
pub const CODE_OUT_OF_RANGE: u16 = 6006;
/// Contract §3.3: provenance invalid.
pub const CODE_PROVENANCE: u16 = 6012;

/// A refusal naming its key and line; never a default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PineCfgError {
    /// Contract §3.3 code (`6004`, `6005`, `6006`, `6012`).
    pub code: u16,
    /// 1-based line of the offending key (the block header's for a missing key).
    pub line: usize,
    /// The key (or table) refused.
    pub key: String,
    /// Why, in words.
    pub why: String,
}

impl std::fmt::Display for PineCfgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} line {}: {}: {}",
            self.code, self.line, self.key, self.why
        )
    }
}

impl std::error::Error for PineCfgError {}

fn refuse(code: u16, line: usize, key: &str, why: impl Into<String>) -> PineCfgError {
    PineCfgError {
        code,
        line,
        key: key.to_owned(),
        why: why.into(),
    }
}

/// `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PineMode {
    /// `"bar_close"`.
    BarClose,
    /// `"realtime"`.
    Realtime,
}

/// `limit_primitive` (D21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PineLimitPrimitive {
    /// `"ioc"`.
    Ioc,
    /// `"maker"`.
    Maker,
}

/// One `[script.inputs]` value, untyped (the program types it, impl-08 §4.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PineInputRaw {
    /// An integer.
    Int(i64),
    /// A string.
    Str(String),
}

/// `[script.provenance]` (B18): optional; when present, every key is required and well-formed
/// (`6012`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PineProvenance {
    /// `sig` — not verified at boot ("checked by hand before `enabled = 1`").
    pub sig: String,
    /// `split_id`.
    pub split_id: String,
    /// `ledger_head`.
    pub ledger_head: String,
    /// `trials_n` (≥ 0).
    pub trials_n: i64,
    /// `upstream_sha256` (64 hex).
    pub upstream_sha256: [u8; 32],
    /// `shell_id`.
    pub shell_id: String,
    /// `pub_date_ms` (≥ 0).
    pub pub_date_ms: i64,
    /// `selected_at_ms` (≥ 0).
    pub selected_at_ms: i64,
    /// `select_tool_version` (≥ 1).
    pub select_tool_version: i64,
}

/// One `[[script]]` block. Field names are the contract §3.14 keys; every key is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PineScriptCfg {
    /// Line of the `[[script]]` header.
    pub line: usize,
    /// `sha256` of the `.pine` file (64 lowercase hex; `6001` at load).
    pub sha256: [u8; 32],
    /// `file`, relative to the manifest's directory (ends in `.pine`; no `/`, no `..`).
    pub file: String,
    /// `dialect`: 5 or 6 (`6002` at load).
    pub dialect: u8,
    /// `enabled`: the per-script arm (0 or 1).
    pub enabled: bool,
    /// `mode`.
    pub mode: PineMode,
    /// `symbol`: canonical key (lowercase, one `:`, ≤ 64 B).
    pub symbol: String,
    /// `timeframe_s` (`1..=u32::MAX` here, `barclock::is_allowed` at load).
    pub timeframe_s: u32,
    /// `venue`: an `exec` venue name (`6003` parity at load).
    pub venue: String,
    /// `max_bars_back` (`0..=100_000`; 0 = no cap declared).
    pub max_bars_back: u32,
    /// `coalesce_ms` (`0..=3_600_000`).
    pub coalesce_ms: u32,
    /// `coalesce_bps` (`0..=10_000`; 0 disables).
    pub coalesce_bps: u32,
    /// `insn_budget` per evaluation (`1_000..=50_000_000`).
    pub insn_budget: u32,
    /// `qty_notional_1e6`: the unit order, USD ×1e6 (`1..=1e12`).
    pub qty_notional_1e6: i64,
    /// `limit_primitive`.
    pub limit_primitive: PineLimitPrimitive,
    /// `cross_ticks` (`0..=100`).
    pub cross_ticks: u32,
    /// `cap_instance_1e6` (`1..=1e12`).
    pub cap_instance_1e6: i64,
    /// `cap_day_1e6` (`1..=1e14`).
    pub cap_day_1e6: i64,
    /// `max_open` (`1..=8`; a `maker` script needs ≥ 2, impl-08 §3.7).
    pub max_open: u32,
    /// `collection_cap` (`1..=1_048_576`).
    pub collection_cap: u32,
    /// `str_persist_bytes` (`0..=16_777_216`).
    pub str_persist_bytes: u32,
    /// `str_cell_bytes` (`8..=65_536`).
    pub str_cell_bytes: u32,
    /// `str_scratch_bytes` (`0..=16_777_216`).
    pub str_scratch_bytes: u32,
    /// `intent_cap` per evaluation (`1..=256`).
    pub intent_cap: u32,
    /// `[script.inputs]`, in file order.
    pub inputs: Vec<(String, PineInputRaw)>,
    /// `[script.symbols]` (TradingView ticker, canonical key), in file order.
    pub symbols: Vec<(String, String)>,
    /// `[script.provenance]`.
    pub provenance: Option<PineProvenance>,
}

/// The manifest: one or more `[[script]]` blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PineManifest {
    /// The blocks, in file order.
    pub scripts: Vec<PineScriptCfg>,
}

/// Every `[[script]]` key, in contract §3.14 order. All required.
pub const SCRIPT_KEYS: [&str; 23] = [
    "sha256",
    "file",
    "dialect",
    "enabled",
    "mode",
    "symbol",
    "timeframe_s",
    "venue",
    "max_bars_back",
    "coalesce_ms",
    "coalesce_bps",
    "insn_budget",
    "qty_notional_1e6",
    "limit_primitive",
    "cross_ticks",
    "cap_instance_1e6",
    "cap_day_1e6",
    "max_open",
    "collection_cap",
    "str_persist_bytes",
    "str_cell_bytes",
    "str_scratch_bytes",
    "intent_cap",
];

/// Every `[script.provenance]` key. All required when the table is present.
pub const PROVENANCE_KEYS: [&str; 9] = [
    "sig",
    "split_id",
    "ledger_head",
    "trials_n",
    "upstream_sha256",
    "shell_id",
    "pub_date_ms",
    "selected_at_ms",
    "select_tool_version",
];

/// The integer keys of `[[script]]` and their inclusive ranges (impl-08 §4.1.2).
pub const SCRIPT_INT_RANGES: [(&str, i64, i64); 17] = [
    ("dialect", 5, 6),
    ("enabled", 0, 1),
    ("timeframe_s", 1, u32::MAX as i64),
    ("max_bars_back", 0, 100_000),
    ("coalesce_ms", 0, 3_600_000),
    ("coalesce_bps", 0, 10_000),
    ("insn_budget", 1_000, 50_000_000),
    ("qty_notional_1e6", 1, 1_000_000_000_000),
    ("cross_ticks", 0, 100),
    ("cap_instance_1e6", 1, 1_000_000_000_000),
    ("cap_day_1e6", 1, 100_000_000_000_000),
    ("max_open", 1, 8),
    ("collection_cap", 1, 1_048_576),
    ("str_persist_bytes", 0, 16_777_216),
    ("str_cell_bytes", 8, 65_536),
    ("str_scratch_bytes", 0, 16_777_216),
    ("intent_cap", 1, 256),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    None,
    Script,
    Inputs,
    Symbols,
    Provenance,
}

/// A block being read: its header line, its keys (with lines), its sub-tables.
struct Block {
    line: usize,
    kv: Vec<(String, Value, usize)>,
    inputs: Vec<(String, PineInputRaw)>,
    symbols: Vec<(String, String)>,
    prov: Option<(usize, Vec<(String, Value, usize)>)>,
    seen_inputs: bool,
    seen_symbols: bool,
}

fn is_bare_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_input_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// Split `key = value`; a quoted key is returned with `quoted = true` and its body.
fn split_kv(line: &str) -> Option<(String, bool, &str)> {
    if let Some(rest) = line.strip_prefix('"') {
        let end = rest.find('"')?;
        let key = &rest[..end];
        let after = rest[end + 1..].trim_start();
        let val = after.strip_prefix('=')?;
        return Some((key.to_owned(), true, val));
    }
    let (k, v) = line.split_once('=')?;
    Some((k.trim().to_owned(), false, v))
}

fn hex32(s: &str, lowercase_only: bool) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' if !lowercase_only => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

fn find<'a>(kv: &'a [(String, Value, usize)], key: &str) -> Option<&'a (String, Value, usize)> {
    kv.iter().find(|(k, _, _)| k == key)
}

/// A required string of a `[[script]]` block.
fn req_str(
    kv: &[(String, Value, usize)],
    key: &str,
    hdr: usize,
) -> Result<(String, usize), PineCfgError> {
    match find(kv, key) {
        Some((_, Value::Str(s), ln)) => Ok((s.clone(), *ln)),
        Some((_, _, ln)) => Err(refuse(CODE_OUT_OF_RANGE, *ln, key, "must be a string")),
        None => Err(refuse(CODE_MISSING_KEY, hdr, key, "missing key")),
    }
}

/// A required integer of a `[[script]]` block, inside `lo..=hi`.
fn req_int(
    kv: &[(String, Value, usize)],
    key: &str,
    hdr: usize,
    lo: i64,
    hi: i64,
) -> Result<i64, PineCfgError> {
    match find(kv, key) {
        Some((_, Value::Int(v), ln)) => {
            if *v < lo || *v > hi {
                return Err(refuse(
                    CODE_OUT_OF_RANGE,
                    *ln,
                    key,
                    format!("{v} is out of range {lo}..={hi}"),
                ));
            }
            Ok(*v)
        }
        Some((_, _, ln)) => Err(refuse(CODE_OUT_OF_RANGE, *ln, key, "must be an integer")),
        None => Err(refuse(CODE_MISSING_KEY, hdr, key, "missing key")),
    }
}

fn range_of(key: &str) -> (i64, i64) {
    for (k, lo, hi) in SCRIPT_INT_RANGES {
        if k == key {
            return (lo, hi);
        }
    }
    debug_assert!(false, "no range for {key}");
    (0, 0)
}

fn int_key(kv: &[(String, Value, usize)], key: &str, hdr: usize) -> Result<i64, PineCfgError> {
    let (lo, hi) = range_of(key);
    req_int(kv, key, hdr, lo, hi)
}

/// `int_key` for a key whose range fits `u32` (every such row of [`SCRIPT_INT_RANGES`]).
fn u32_key(kv: &[(String, Value, usize)], key: &str, hdr: usize) -> Result<u32, PineCfgError> {
    let v = int_key(kv, key, hdr)?;
    u32::try_from(v).map_err(|_| refuse(CODE_OUT_OF_RANGE, hdr, key, "does not fit u32"))
}

fn finish_provenance(
    line: usize,
    kv: &[(String, Value, usize)],
) -> Result<PineProvenance, PineCfgError> {
    let s = |key: &str| -> Result<String, PineCfgError> {
        match find(kv, key) {
            Some((_, Value::Str(v), _)) => Ok(v.clone()),
            Some((_, _, ln)) => Err(refuse(CODE_PROVENANCE, *ln, key, "must be a string")),
            None => Err(refuse(CODE_PROVENANCE, line, key, "missing provenance key")),
        }
    };
    let n = |key: &str, lo: i64| -> Result<i64, PineCfgError> {
        match find(kv, key) {
            Some((_, Value::Int(v), ln)) => {
                if *v < lo {
                    return Err(refuse(
                        CODE_PROVENANCE,
                        *ln,
                        key,
                        format!("{v} must be >= {lo}"),
                    ));
                }
                Ok(*v)
            }
            Some((_, _, ln)) => Err(refuse(CODE_PROVENANCE, *ln, key, "must be an integer")),
            None => Err(refuse(CODE_PROVENANCE, line, key, "missing provenance key")),
        }
    };
    let up = s("upstream_sha256")?;
    let up_line = find(kv, "upstream_sha256").map_or(line, |e| e.2);
    let upstream_sha256 = hex32(&up, false).ok_or_else(|| {
        refuse(
            CODE_PROVENANCE,
            up_line,
            "upstream_sha256",
            "must be 64 hex",
        )
    })?;
    Ok(PineProvenance {
        sig: s("sig")?,
        split_id: s("split_id")?,
        ledger_head: s("ledger_head")?,
        trials_n: n("trials_n", 0)?,
        upstream_sha256,
        shell_id: s("shell_id")?,
        pub_date_ms: n("pub_date_ms", 0)?,
        selected_at_ms: n("selected_at_ms", 0)?,
        select_tool_version: n("select_tool_version", 1)?,
    })
}

fn finish_block(b: Block) -> Result<PineScriptCfg, PineCfgError> {
    let hdr = b.line;
    let kv = &b.kv;
    // Required keys are judged in §3.14 order, so the first refusal is deterministic.
    let (sha, sha_ln) = req_str(kv, "sha256", hdr)?;
    let sha256 = hex32(&sha, true).ok_or_else(|| {
        refuse(
            CODE_OUT_OF_RANGE,
            sha_ln,
            "sha256",
            "must be 64 lowercase hex",
        )
    })?;
    let (file, file_ln) = req_str(kv, "file", hdr)?;
    if !file.ends_with(".pine") || file.contains('/') || file.contains("..") {
        return Err(refuse(
            CODE_OUT_OF_RANGE,
            file_ln,
            "file",
            format!("\"{file}\" must end in .pine with no `/` and no `..`"),
        ));
    }
    let dialect = int_key(kv, "dialect", hdr)? as u8;
    let enabled = int_key(kv, "enabled", hdr)? == 1;
    let (mode_s, mode_ln) = req_str(kv, "mode", hdr)?;
    let mode = match mode_s.as_str() {
        "bar_close" => PineMode::BarClose,
        "realtime" => PineMode::Realtime,
        _ => {
            return Err(refuse(
                CODE_OUT_OF_RANGE,
                mode_ln,
                "mode",
                format!("\"{mode_s}\" must be \"bar_close\" or \"realtime\""),
            ))
        }
    };
    let (symbol, sym_ln) = req_str(kv, "symbol", hdr)?;
    let colons = symbol.bytes().filter(|&c| c == b':').count();
    if colons != 1 || symbol.len() > 64 || symbol.bytes().any(|c| c.is_ascii_uppercase()) {
        return Err(refuse(
            CODE_OUT_OF_RANGE,
            sym_ln,
            "symbol",
            format!("\"{symbol}\" must be lowercase with one `:` and at most 64 bytes"),
        ));
    }
    let timeframe_s = u32_key(kv, "timeframe_s", hdr)?;
    let (venue, venue_ln) = req_str(kv, "venue", hdr)?;
    if crate::exec::venue_id_from_name(&venue).is_none() {
        return Err(refuse(
            CODE_OUT_OF_RANGE,
            venue_ln,
            "venue",
            format!("\"{venue}\" is not an exec.toml venue name"),
        ));
    }
    let max_bars_back = u32_key(kv, "max_bars_back", hdr)?;
    let coalesce_ms = u32_key(kv, "coalesce_ms", hdr)?;
    let coalesce_bps = u32_key(kv, "coalesce_bps", hdr)?;
    let insn_budget = u32_key(kv, "insn_budget", hdr)?;
    let qty_notional_1e6 = int_key(kv, "qty_notional_1e6", hdr)?;
    let (lp, lp_ln) = req_str(kv, "limit_primitive", hdr)?;
    let limit_primitive = match lp.as_str() {
        "ioc" => PineLimitPrimitive::Ioc,
        "maker" => PineLimitPrimitive::Maker,
        _ => {
            return Err(refuse(
                CODE_OUT_OF_RANGE,
                lp_ln,
                "limit_primitive",
                format!("\"{lp}\" must be \"ioc\" or \"maker\""),
            ))
        }
    };
    let cross_ticks = u32_key(kv, "cross_ticks", hdr)?;
    let cap_instance_1e6 = int_key(kv, "cap_instance_1e6", hdr)?;
    let cap_day_1e6 = int_key(kv, "cap_day_1e6", hdr)?;
    let max_open = u32_key(kv, "max_open", hdr)?;
    // impl-08 §3.7: a maker script needs room for an entry and its exit — refused here, before
    // the broker's 5105.
    if limit_primitive == PineLimitPrimitive::Maker && max_open < 2 {
        let ln = find(kv, "max_open").map_or(hdr, |e| e.2);
        return Err(refuse(
            CODE_OUT_OF_RANGE,
            ln,
            "max_open",
            format!("{max_open} is below 2 for a `maker` script"),
        ));
    }
    let collection_cap = u32_key(kv, "collection_cap", hdr)?;
    let str_persist_bytes = u32_key(kv, "str_persist_bytes", hdr)?;
    let str_cell_bytes = u32_key(kv, "str_cell_bytes", hdr)?;
    let str_scratch_bytes = u32_key(kv, "str_scratch_bytes", hdr)?;
    let intent_cap = u32_key(kv, "intent_cap", hdr)?;
    let provenance = match &b.prov {
        Some((ln, pkv)) => Some(finish_provenance(*ln, pkv)?),
        None => None,
    };
    Ok(PineScriptCfg {
        line: hdr,
        sha256,
        file,
        dialect,
        enabled,
        mode,
        symbol,
        timeframe_s,
        venue,
        max_bars_back,
        coalesce_ms,
        coalesce_bps,
        insn_budget,
        qty_notional_1e6,
        limit_primitive,
        cross_ticks,
        cap_instance_1e6,
        cap_day_1e6,
        max_open,
        collection_cap,
        str_persist_bytes,
        str_cell_bytes,
        str_scratch_bytes,
        intent_cap,
        inputs: b.inputs,
        symbols: b.symbols,
        provenance,
    })
}

/// Parse a manifest; the first refusal wins (`6004`–`6006`, `6012`).
pub fn parse_manifest(src: &str) -> Result<PineManifest, PineCfgError> {
    let mut out = PineManifest::default();
    let mut table = Table::None;
    let mut cur: Option<Block> = None;

    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            match line {
                "[[script]]" => {
                    if let Some(b) = cur.take() {
                        out.scripts.push(finish_block(b)?);
                    }
                    cur = Some(Block {
                        line: ln,
                        kv: Vec::new(),
                        inputs: Vec::new(),
                        symbols: Vec::new(),
                        prov: None,
                        seen_inputs: false,
                        seen_symbols: false,
                    });
                    table = Table::Script;
                }
                "[script.inputs]" | "[script.symbols]" | "[script.provenance]" => {
                    let name = &line[1..line.len() - 1];
                    let Some(b) = cur.as_mut() else {
                        return Err(refuse(
                            CODE_UNKNOWN_KEY,
                            ln,
                            name,
                            "sub-table outside a [[script]] block",
                        ));
                    };
                    let dup = match line {
                        "[script.inputs]" => std::mem::replace(&mut b.seen_inputs, true),
                        "[script.symbols]" => std::mem::replace(&mut b.seen_symbols, true),
                        _ => b.prov.is_some(),
                    };
                    if dup {
                        return Err(refuse(CODE_OUT_OF_RANGE, ln, name, "duplicate table"));
                    }
                    table = match line {
                        "[script.inputs]" => Table::Inputs,
                        "[script.symbols]" => Table::Symbols,
                        _ => {
                            b.prov = Some((ln, Vec::new()));
                            Table::Provenance
                        }
                    };
                }
                other => {
                    let name = other.trim_start_matches('[').trim_end_matches(']');
                    return Err(refuse(CODE_UNKNOWN_KEY, ln, name, "unknown table"));
                }
            }
            continue;
        }
        let Some((key, quoted, val)) = split_kv(line) else {
            // No `=`: in provenance it is a malformed provenance line; anywhere else the whole
            // line is an unknown key.
            if table == Table::Provenance {
                return Err(refuse(CODE_PROVENANCE, ln, line, "expected `key = value`"));
            }
            return Err(refuse(CODE_UNKNOWN_KEY, ln, line, "expected `key = value`"));
        };
        let Some(b) = cur.as_mut() else {
            return Err(refuse(
                CODE_UNKNOWN_KEY,
                ln,
                &key,
                "key outside any [[script]] block",
            ));
        };
        if quoted && table != Table::Symbols {
            return Err(refuse(
                CODE_UNKNOWN_KEY,
                ln,
                &key,
                "a quoted key is legal only inside [script.symbols]",
            ));
        }
        match table {
            Table::None => {
                return Err(refuse(CODE_UNKNOWN_KEY, ln, &key, "key outside any table"));
            }
            Table::Script => {
                if !is_bare_key(&key) || !SCRIPT_KEYS.contains(&key.as_str()) {
                    return Err(refuse(CODE_UNKNOWN_KEY, ln, &key, "unknown key"));
                }
                if find(&b.kv, &key).is_some() {
                    return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, "duplicate key"));
                }
                let v =
                    parse_value(val, ln).map_err(|e| refuse(CODE_OUT_OF_RANGE, ln, &key, e.0))?;
                b.kv.push((key, v, ln));
            }
            Table::Inputs => {
                if !is_input_key(&key) {
                    return Err(refuse(CODE_UNKNOWN_KEY, ln, &key, "bad input name"));
                }
                if b.inputs.iter().any(|(k, _)| *k == key) {
                    return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, "duplicate key"));
                }
                let v = match parse_value(val, ln) {
                    Ok(Value::Int(i)) => PineInputRaw::Int(i),
                    Ok(Value::Str(s)) => PineInputRaw::Str(s),
                    Ok(_) => {
                        return Err(refuse(
                            CODE_OUT_OF_RANGE,
                            ln,
                            &key,
                            "must be an integer or a string",
                        ))
                    }
                    Err(e) => return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, e.0)),
                };
                b.inputs.push((key, v));
            }
            Table::Symbols => {
                if key.is_empty() || (!quoted && !is_bare_key(&key)) || key.contains('\\') {
                    return Err(refuse(CODE_UNKNOWN_KEY, ln, &key, "bad ticker key"));
                }
                if b.symbols.iter().any(|(k, _)| *k == key) {
                    return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, "duplicate key"));
                }
                match parse_value(val, ln) {
                    Ok(Value::Str(s)) => b.symbols.push((key, s)),
                    Ok(_) => return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, "must be a string")),
                    Err(e) => return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, e.0)),
                }
            }
            Table::Provenance => {
                if !is_bare_key(&key) || !PROVENANCE_KEYS.contains(&key.as_str()) {
                    return Err(refuse(CODE_UNKNOWN_KEY, ln, &key, "unknown provenance key"));
                }
                let Some((_, pkv)) = b.prov.as_mut() else {
                    return Err(refuse(CODE_PROVENANCE, ln, &key, "no provenance table"));
                };
                if find(pkv, &key).is_some() {
                    return Err(refuse(CODE_OUT_OF_RANGE, ln, &key, "duplicate key"));
                }
                let v = parse_value(val, ln).map_err(|e| refuse(CODE_PROVENANCE, ln, &key, e.0))?;
                pkv.push((key, v, ln));
            }
        }
    }
    if let Some(b) = cur.take() {
        out.scripts.push(finish_block(b)?);
    }
    if out.scripts.is_empty() {
        return Err(refuse(CODE_MISSING_KEY, 0, "script", "no [[script]] block"));
    }
    Ok(out)
}

/// Read + parse; returns the bytes so the caller hashes the exact artifact.
pub fn load_manifest(path: &Path) -> Result<(PineManifest, Vec<u8>), PineCfgError> {
    let bytes = std::fs::read(path).map_err(|e| {
        refuse(
            CODE_MISSING_KEY,
            0,
            "manifest",
            format!("{}: {e}", path.display()),
        )
    })?;
    let src = std::str::from_utf8(&bytes).map_err(|e| {
        refuse(
            CODE_MISSING_KEY,
            0,
            "manifest",
            format!("{}: not UTF-8 ({e})", path.display()),
        )
    })?;
    let m = parse_manifest(src)?;
    Ok((m, bytes))
}

// ---------------------------------------------------------------------------------------------
// `[exec.slot.8.pine]` (impl-08 §4.8)
// ---------------------------------------------------------------------------------------------

/// The `strategy-set` slot the Pine member owns (operator ruling 2026-10-16: slot 8, PAPER ONLY).
/// `cli::exec_boot` const-asserts it equals `strategy_set::SLOT_PINE`.
pub const PINE_SLOT: usize = 8;

/// The section's header text.
pub const PINE_SECTION: &str = "exec.slot.8.pine";

/// `event_insn_budget`'s table default, applied by the consumer when the key is absent
/// (pin:08-slot-default-none).
pub const EVENT_INSN_BUDGET_DEFAULT: u32 = 4_000_000;

/// Q1 (◆B): what a boot does with an adopted position (impl-08 §4.7.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootExitPolicy {
    /// `"accept"`.
    Accept,
    /// `"standing"`.
    Standing,
    /// `"protective"`.
    Protective,
    /// `"flatten"`.
    Flatten,
}

/// `[exec.slot.8.pine]` (impl-08 §4.8). `None` = a required-on-Live key that was absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PineSlotCfg {
    /// `min_coalesce_ms` (`1..=60_000`); Live: required.
    pub min_coalesce_ms: Option<u32>,
    /// `event_insn_budget` (`1_000..=1_000_000_000`); Live: required.
    pub event_insn_budget: Option<u32>,
    /// `boot_exit_policy`; Live: required.
    pub boot_exit_policy: Option<BootExitPolicy>,
    /// `max_frame_bytes` (`65_536..=268_435_456`, default 16 777 216).
    pub max_frame_bytes: u32,
    /// `quote_stale_ms` (`1..=60_000`; default: the venue's own stale window).
    pub quote_stale_ms: Option<u32>,
    /// `live_settle_ms` (`100..=60_000`, default 2000).
    pub live_settle_ms: u32,
    /// `refusal_streak_max` (`1..=1024`, default 16).
    pub refusal_streak_max: u32,
    /// `fee_taker_ppm` (`0..=100_000`, default 0).
    pub fee_taker_ppm: u32,
    /// `fee_maker_ppm` (`0..=100_000`, default 0).
    pub fee_maker_ppm: u32,
    /// `max_warm_lag_bars` (`0..=1000`, default 1).
    pub max_warm_lag_bars: u32,
}

impl PineSlotCfg {
    /// The §4.8 defaults (paper boots without the section) — equal to an empty section.
    pub const DEFAULT: Self = Self {
        min_coalesce_ms: None,
        event_insn_budget: None,
        boot_exit_policy: None,
        max_frame_bytes: 16_777_216,
        quote_stale_ms: None,
        live_settle_ms: 2000,
        refusal_streak_max: 16,
        fee_taker_ppm: 0,
        fee_maker_ppm: 0,
        max_warm_lag_bars: 1,
    };
}

/// The section's keys and their inclusive integer ranges (`boot_exit_policy` is a word).
pub const SLOT_INT_RANGES: [(&str, i64, i64); 9] = [
    ("min_coalesce_ms", 1, 60_000),
    ("event_insn_budget", 1_000, 1_000_000_000),
    ("max_frame_bytes", 65_536, 268_435_456),
    ("quote_stale_ms", 1, 60_000),
    ("live_settle_ms", 100, 60_000),
    ("refusal_streak_max", 1, 1024),
    ("fee_taker_ppm", 0, 100_000),
    ("fee_maker_ppm", 0, 100_000),
    ("max_warm_lag_bars", 0, 1000),
];

/// Every key `[exec.slot.8.pine]` accepts.
pub const SLOT_KEYS: [&str; 10] = [
    "min_coalesce_ms",
    "event_insn_budget",
    "boot_exit_policy",
    "max_frame_bytes",
    "quote_stale_ms",
    "live_settle_ms",
    "refusal_streak_max",
    "fee_taker_ppm",
    "fee_maker_ppm",
    "max_warm_lag_bars",
];

fn slot_int(kv: &[(String, Value, usize)], key: &str) -> Result<Option<u32>, PineCfgError> {
    let (mut lo, mut hi) = (0, 0);
    for (k, l, h) in SLOT_INT_RANGES {
        if k == key {
            (lo, hi) = (l, h);
        }
    }
    match find(kv, key) {
        Some((_, Value::Int(v), ln)) => {
            if *v < lo || *v > hi {
                return Err(refuse(
                    CODE_OUT_OF_RANGE,
                    *ln,
                    key,
                    format!("{v} is out of range {lo}..={hi}"),
                ));
            }
            Ok(Some(*v as u32))
        }
        Some((_, _, ln)) => Err(refuse(CODE_OUT_OF_RANGE, *ln, key, "must be an integer")),
        None => Ok(None),
    }
}

/// Parse `[exec.slot.8.pine]`'s collected lines (`exec::parse` has already refused an unknown
/// or duplicate key, `6004` / `6006`). Absent optional keys take the §4.8 defaults.
pub(crate) fn parse_slot8(kv: &[(String, Value, usize)]) -> Result<PineSlotCfg, PineCfgError> {
    let d = PineSlotCfg::DEFAULT;
    let boot_exit_policy = match find(kv, "boot_exit_policy") {
        Some((_, Value::Str(s), ln)) => Some(match s.as_str() {
            "accept" => BootExitPolicy::Accept,
            "standing" => BootExitPolicy::Standing,
            "protective" => BootExitPolicy::Protective,
            "flatten" => BootExitPolicy::Flatten,
            _ => {
                return Err(refuse(
                    CODE_OUT_OF_RANGE,
                    *ln,
                    "boot_exit_policy",
                    format!("\"{s}\" must be accept | standing | protective | flatten"),
                ))
            }
        }),
        Some((_, _, ln)) => {
            return Err(refuse(
                CODE_OUT_OF_RANGE,
                *ln,
                "boot_exit_policy",
                "must be a string",
            ))
        }
        None => None,
    };
    Ok(PineSlotCfg {
        min_coalesce_ms: slot_int(kv, "min_coalesce_ms")?,
        event_insn_budget: slot_int(kv, "event_insn_budget")?,
        boot_exit_policy,
        max_frame_bytes: slot_int(kv, "max_frame_bytes")?.unwrap_or(d.max_frame_bytes),
        quote_stale_ms: slot_int(kv, "quote_stale_ms")?,
        live_settle_ms: slot_int(kv, "live_settle_ms")?.unwrap_or(d.live_settle_ms),
        refusal_streak_max: slot_int(kv, "refusal_streak_max")?.unwrap_or(d.refusal_streak_max),
        fee_taker_ppm: slot_int(kv, "fee_taker_ppm")?.unwrap_or(d.fee_taker_ppm),
        fee_maker_ppm: slot_int(kv, "fee_maker_ppm")?.unwrap_or(d.fee_maker_ppm),
        max_warm_lag_bars: slot_int(kv, "max_warm_lag_bars")?.unwrap_or(d.max_warm_lag_bars),
    })
}

/// The Live requirements of impl-08 §4.8: the three Live-required keys present, else `6005`
/// naming the first absent one (at the section's line).
///
/// pin:08-require-live-pub: `pub`, not crate-internal as §3.3 sketches — slot 8 is PAPER ONLY by
/// ruling, so `exec::parse` refuses a live slot 8 before any Live requirement is judged and has
/// no caller for it; this is kept, tested, for the live-arming ruling.
pub fn require_live(cfg: &PineSlotCfg, section_line: usize) -> Result<(), PineCfgError> {
    if cfg.min_coalesce_ms.is_none() {
        return Err(refuse(
            CODE_MISSING_KEY,
            section_line,
            "min_coalesce_ms",
            "missing key",
        ));
    }
    if cfg.event_insn_budget.is_none() {
        return Err(refuse(
            CODE_MISSING_KEY,
            section_line,
            "event_insn_budget",
            "missing key",
        ));
    }
    if cfg.boot_exit_policy.is_none() {
        return Err(refuse(
            CODE_MISSING_KEY,
            section_line,
            "boot_exit_policy",
            "missing key",
        ));
    }
    Ok(())
}
