//! # universe — boot-universe config file (Stage 2.5, M1)
//!
//! Parses `~/multivenue/universe.toml` (or `--universe <path>`) into
//! the per-venue instrument lists the engine boots with, and owns the
//! M1 SymbolId allocation law (`docs/mvp-progress.md`, M1a design).
//!
//! **BOOT/OFFLINE DOCTRINE:** this module runs exactly once at process
//! boot, before any ingress thread spawns. Allocations are permitted —
//! nothing here is on the hot path.
//!
//! The grammar is a deliberate TOML SUBSET, parsed by hand (no serde,
//! no external toml crate — house rule: we own every parser):
//!
//! - `# comment` lines and trailing `# comments` (quote-aware: a `#`
//!   inside a quoted string — e.g. the Hyperliquid HIP-4 coin
//!   `"#330"` — never starts a comment);
//! - `[section]` headers — known sections only;
//! - `key = value` where value is a quoted string array (single-line
//!   or multiline, trailing comma allowed) or a bare `true`/`false`;
//! - strings are plain `"…"` with NO escape sequences (a `\` inside a
//!   string is an error — token ids and venue symbols never need one).
//!
//! Anything else — unknown sections, unknown keys, dotted keys, inline
//! tables, floats, dates — is a FATAL parse error: fail-fast beats a
//! silently ignored typo'd venue list.

use std::path::Path;

use core_types::{make_symbol_id, SymbolId, VenueId};

// ---------------------------------------------------------------
// Public constants (M1 allocation law + caps)
// ---------------------------------------------------------------

/// Maximum Polymarket MARKET entries per boot (a YES/NO pair is one
/// entry). Bounds the single market-channel subscribe frame.
pub const PM_MARKETS_MAX: usize = 64;

/// Maximum instruments per venue list. Keeps per-venue ordinals well
/// inside their blocks (Binance USDM base 512) with room to spare.
pub const VENUE_LIST_MAX: usize = 500;

/// Ordinal base for Binance USDS-M futures ids:
/// `make_symbol_id(Binance, BN_USDM_ORDINAL_BASE + j + 1)`. Keeps
/// futures ordinals disjoint from spot ordinals by construction.
pub const BN_USDM_ORDINAL_BASE: u32 = 512;

/// Legacy flat id for the FIRST Polymarket token (pre-8e convention;
/// preserves the H6 demo lineage, worker-map seeds and latency-arb
/// defaults). Every later PM token gets a namespaced id.
pub const LEGACY_PM_ANCHOR_SYM: SymbolId = 42;

/// Legacy flat id for the FIRST Binance spot symbol (the clap-default
/// mirror `binance:btcusdt ↔ 7` the worker knows). Every later spot
/// symbol gets a namespaced id.
pub const LEGACY_BN_ANCHOR_SYM: SymbolId = 7;

const PM_TOKEN_LEN_MIN: usize = 10;
const PM_TOKEN_LEN_MAX: usize = 80;
const BN_SYMBOL_LEN_MAX: usize = 32;
const INSTRUMENT_LEN_MAX: usize = 48;
const HL_COIN_LEN_MAX: usize = 32;

// ---------------------------------------------------------------
// Error type
// ---------------------------------------------------------------

/// Parse/validation error with a 1-based source line (0 = not tied to
/// a source line, e.g. an allocation collision or an IO failure).
/// Surfaced once at boot, then fatal — the message is the product.
#[derive(Debug, PartialEq, Eq)]
pub struct UniverseError {
    /// 1-based line number in the config file; 0 when not line-tied.
    pub line: usize,
    /// Human-readable cause, including the offending token where safe.
    pub msg: String,
}

impl ::core::fmt::Display for UniverseError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        if self.line > 0 {
            write!(f, "universe config line {}: {}", self.line, self.msg)
        } else {
            write!(f, "universe config: {}", self.msg)
        }
    }
}

impl std::error::Error for UniverseError {}

fn err(line: usize, msg: impl Into<String>) -> UniverseError {
    UniverseError {
        line,
        msg: msg.into(),
    }
}

// ---------------------------------------------------------------
// Parsed model
// ---------------------------------------------------------------

/// One Polymarket market entry (M1-R1: crypto up/down binaries only —
/// enforced editorially by the operator-curated list, not by code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PmMarket {
    /// A single CLOB token id (decimal string).
    Single(String),
    /// A YES/NO token-id pair (`"<yes>:<no>"` in the file).
    YesNo {
        /// YES-side token id — the tradable leg pairs reference.
        yes: String,
        /// NO-side token id.
        no: String,
    },
}

impl PmMarket {
    /// The market's tradable leg: the single token, or the YES side.
    pub fn yes_token(&self) -> &str {
        match self {
            Self::Single(t) => t,
            Self::YesNo { yes, .. } => yes,
        }
    }
}

/// The parsed universe file. Empty lists are valid at PARSE level —
/// the boot refusal law (`assert_bootable`) is applied by the caller
/// after allocation, so tests and tooling can parse partial files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Universe {
    /// `[polymarket] markets` — market entries in file order.
    pub pm_markets: Vec<PmMarket>,
    /// `[binance] spot` — lowercase stream symbols in file order.
    pub binance_spot: Vec<String>,
    /// `[binance] usdm` — USDS-M futures stream symbols in file order.
    pub binance_usdm: Vec<String>,
    /// `[okx] instruments` — instIds in file order.
    pub okx_instruments: Vec<String>,
    /// `[okx] depth` — subscribe the 400-level books channel (§4.5).
    pub okx_depth: bool,
    /// `[deribit] instruments` — instrument names in file order.
    pub deribit_instruments: Vec<String>,
    /// `[deribit] depth` — subscribe the change_id-chained book (§4.5).
    pub deribit_depth: bool,
    /// `[hyperliquid] coins` — coin names (HIP-4 `#<enc>` and spot
    /// `@<idx>` forms are ordinary items) in file order.
    pub hl_coins: Vec<String>,
    /// `[pairs] map` — latency-arb pairs as
    /// `(pm market index, binance spot index)`, both 0-based file
    /// order. Empty = the default pair (0,0) is injected at
    /// allocation when both sides exist.
    pub pairs: Vec<(u32, u32)>,
}

// ---------------------------------------------------------------
// Allocated model (the M1 SymbolId law, applied)
// ---------------------------------------------------------------

/// One allocated Polymarket token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmToken {
    /// Allocated SymbolId (token[0] → [`LEGACY_PM_ANCHOR_SYM`]).
    pub sym: SymbolId,
    /// The CLOB token id (decimal string) — also the §9.4 descriptor.
    pub token_id: String,
    /// 0-based index of the market entry this token belongs to.
    pub market_index: u32,
    /// True for a `Single` token and for the YES side of a pair.
    pub is_yes: bool,
}

/// One allocated non-PM instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    /// Allocated SymbolId.
    pub sym: SymbolId,
    /// The venue-native name exactly as configured.
    pub name: String,
    /// §9.4 descriptor (`binance:<sym>`, `binance-usdm:<sym>`,
    /// `okx:<instId>`, `deribit:<name>`, `hyperliquid:<coin>`).
    pub descriptor: String,
}

/// The full allocated universe: every configured instrument with its
/// SymbolId + descriptor, duplicate-checked, plus resolved
/// latency-arb pairs. Consumed by cli boot (venue tables, symbol
/// maps, `build_ai_universe`, `EngineConfig::pairs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllocatedUniverse {
    /// Flattened PM tokens (a YES/NO entry contributes YES then NO).
    pub pm_tokens: Vec<PmToken>,
    /// Binance spot instruments.
    pub bn_spot: Vec<Instrument>,
    /// Binance USDS-M futures instruments.
    pub bn_usdm: Vec<Instrument>,
    /// OKX instruments.
    pub okx: Vec<Instrument>,
    /// Deribit instruments.
    pub deribit: Vec<Instrument>,
    /// Hyperliquid coins.
    pub hl: Vec<Instrument>,
    /// Latency-arb pairs as `(pm YES-token sym, bn spot sym)`.
    pub pairs: Vec<(SymbolId, SymbolId)>,
}

// ---------------------------------------------------------------
// Default path
// ---------------------------------------------------------------

/// The M1-R2 default config location, `~/multivenue/universe.toml`,
/// tilde-expanded against `$HOME` (same rule as the crate's other
/// home-anchored defaults).
pub fn default_universe_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/universe.toml")
}

/// Read + parse a universe file. IO failures surface as a line-0
/// [`UniverseError`] naming the path.
pub fn load(path: &Path) -> Result<Universe, UniverseError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| err(0, format!("cannot read {}: {e}", path.display())))?;
    parse(&src)
}

// ---------------------------------------------------------------
// Parser
// ---------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Section {
    None,
    Polymarket,
    Binance,
    Okx,
    Deribit,
    Hyperliquid,
    Pairs,
}

/// Which typed slot a `key = …` line targets.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Slot {
    PmMarkets,
    BnSpot,
    BnUsdm,
    OkxInstr,
    OkxDepth,
    DeribitInstr,
    DeribitDepth,
    HlCoins,
    PairsMap,
}

/// Per-slot element validator for array slots.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ElemKind {
    PmMarket,
    BnSymbol,
    Instrument,
    HlCoin,
    PairRef,
}

struct PendingArray {
    slot: Slot,
    kind: ElemKind,
    items: Vec<String>,
    /// True when the next non-`]` token must be a comma (an element
    /// was just completed). Carried across lines for multiline arrays.
    need_comma: bool,
    /// Line where the array opened — for the unterminated-array error.
    opened_at: usize,
}

#[derive(Default)]
struct Builder {
    pm_markets: Option<Vec<String>>,
    bn_spot: Option<Vec<String>>,
    bn_usdm: Option<Vec<String>>,
    okx_instr: Option<Vec<String>>,
    okx_depth: Option<bool>,
    deribit_instr: Option<Vec<String>>,
    deribit_depth: Option<bool>,
    hl_coins: Option<Vec<String>>,
    pairs_map: Option<Vec<String>>,
}

/// Parse universe-file source text. See the module docs for the
/// grammar. Returns the first error encountered (fail-fast).
pub fn parse(src: &str) -> Result<Universe, UniverseError> {
    let mut section = Section::None;
    let mut b = Builder::default();
    let mut pending: Option<PendingArray> = None;

    let mut line_no = 0usize;
    for raw in src.lines() {
        line_no += 1;
        let cut = comment_cut(raw);
        let line = raw[..cut].trim();
        if line.is_empty() {
            continue;
        }

        // Continuation of a multiline array.
        if let Some(p) = pending.as_mut() {
            let closed = scan_array_fragment(line, line_no, p)?;
            if closed {
                let done = pending.take().expect("pending checked above");
                store_array(&mut b, done.slot, done.items, line_no)?;
            }
            continue;
        }

        // Section header.
        if let Some(rest) = line.strip_prefix('[') {
            let Some(name) = rest.strip_suffix(']') else {
                return Err(err(line_no, format!("malformed section header `{line}`")));
            };
            section = match name.trim() {
                "polymarket" => Section::Polymarket,
                "binance" => Section::Binance,
                "okx" => Section::Okx,
                "deribit" => Section::Deribit,
                "hyperliquid" => Section::Hyperliquid,
                "pairs" => Section::Pairs,
                other => {
                    return Err(err(line_no, format!("unknown section `[{other}]`")));
                }
            };
            continue;
        }

        // key = value
        let Some(eq) = line.find('=') else {
            return Err(err(line_no, format!("expected `key = value`, got `{line}`")));
        };
        let key = line[..eq].trim();
        let value = line[eq + 1..].trim();
        if key.is_empty() {
            return Err(err(line_no, "empty key before `=`"));
        }
        let slot = slot_for(section, key)
            .ok_or_else(|| err(line_no, unknown_key_msg(section, key)))?;

        match slot {
            Slot::OkxDepth | Slot::DeribitDepth => {
                let v = match value {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(err(
                            line_no,
                            format!("`{key}` expects `true` or `false`, got `{other}`"),
                        ));
                    }
                };
                store_bool(&mut b, slot, v, line_no)?;
            }
            _ => {
                let Some(after_bracket) = value.strip_prefix('[') else {
                    return Err(err(
                        line_no,
                        format!("`{key}` expects a `[\"…\", …]` string array"),
                    ));
                };
                let mut p = PendingArray {
                    slot,
                    kind: elem_kind(slot),
                    items: Vec::new(),
                    need_comma: false,
                    opened_at: line_no,
                };
                let closed = scan_array_fragment(after_bracket.trim_start(), line_no, &mut p)?;
                if closed {
                    store_array(&mut b, p.slot, p.items, line_no)?;
                } else {
                    pending = Some(p);
                }
            }
        }
    }

    if let Some(p) = pending {
        return Err(err(p.opened_at, "array `[` is never closed with `]`"));
    }

    finalize(b)
}

/// Byte index where a quote-aware trailing comment starts (or the
/// full length when there is none). A `#` inside a `"…"` string —
/// e.g. the HIP-4 coin `"#330"` — never starts a comment.
fn comment_cut(raw: &str) -> usize {
    let bytes = raw.as_bytes();
    let mut in_str = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'#' if !in_str => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn slot_for(section: Section, key: &str) -> Option<Slot> {
    match (section, key) {
        (Section::Polymarket, "markets") => Some(Slot::PmMarkets),
        (Section::Binance, "spot") => Some(Slot::BnSpot),
        (Section::Binance, "usdm") => Some(Slot::BnUsdm),
        (Section::Okx, "instruments") => Some(Slot::OkxInstr),
        (Section::Okx, "depth") => Some(Slot::OkxDepth),
        (Section::Deribit, "instruments") => Some(Slot::DeribitInstr),
        (Section::Deribit, "depth") => Some(Slot::DeribitDepth),
        (Section::Hyperliquid, "coins") => Some(Slot::HlCoins),
        (Section::Pairs, "map") => Some(Slot::PairsMap),
        _ => None,
    }
}

fn unknown_key_msg(section: Section, key: &str) -> String {
    match section {
        Section::None => format!("`{key}` appears before any [section] header"),
        _ => format!("unknown key `{key}` in {section:?} section"),
    }
}

fn elem_kind(slot: Slot) -> ElemKind {
    match slot {
        Slot::PmMarkets => ElemKind::PmMarket,
        Slot::BnSpot | Slot::BnUsdm => ElemKind::BnSymbol,
        Slot::OkxInstr | Slot::DeribitInstr => ElemKind::Instrument,
        Slot::HlCoins => ElemKind::HlCoin,
        Slot::PairsMap => ElemKind::PairRef,
        Slot::OkxDepth | Slot::DeribitDepth => unreachable!("bool slots have no elements"),
    }
}

/// Scan one (comment-stripped, trimmed) fragment of an array body.
/// Returns `Ok(true)` when the closing `]` was consumed. The fragment
/// may be empty (a blank continuation line).
fn scan_array_fragment(
    fragment: &str,
    line_no: usize,
    p: &mut PendingArray,
) -> Result<bool, UniverseError> {
    let bytes = fragment.as_bytes();
    let mut i = 0usize;
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            return Ok(false); // continue on the next line
        }
        match bytes[i] {
            b']' => {
                let rest = fragment[i + 1..].trim();
                if !rest.is_empty() {
                    return Err(err(line_no, format!("unexpected `{rest}` after `]`")));
                }
                return Ok(true);
            }
            b',' => {
                if !p.need_comma {
                    return Err(err(line_no, "unexpected `,` in array"));
                }
                p.need_comma = false;
                i += 1;
            }
            b'"' => {
                if p.need_comma {
                    return Err(err(line_no, "missing `,` between array elements"));
                }
                let (s, next) = scan_string(fragment, i, line_no)?;
                validate_elem(p.kind, &s, line_no)?;
                p.items.push(s);
                p.need_comma = true;
                i = next;
            }
            other => {
                return Err(err(
                    line_no,
                    format!("unexpected `{}` in array (elements are quoted strings)", other as char),
                ));
            }
        }
    }
}

/// Scan a `"…"` string starting at `start` (which must index a `"`).
/// No escape sequences: a `\` inside the string is an error. Returns
/// the string contents and the index just past the closing quote.
fn scan_string(
    fragment: &str,
    start: usize,
    line_no: usize,
) -> Result<(String, usize), UniverseError> {
    let bytes = fragment.as_bytes();
    debug_assert_eq!(bytes[start], b'"');
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let s = &fragment[start + 1..i];
                return Ok((s.to_string(), i + 1));
            }
            b'\\' => {
                return Err(err(
                    line_no,
                    "escape sequences are not supported inside strings",
                ));
            }
            b if b < 0x20 => {
                return Err(err(line_no, "control character inside string"));
            }
            _ => i += 1,
        }
    }
    Err(err(line_no, "unterminated string (strings cannot span lines)"))
}

// ---------------------------------------------------------------
// Element validation
// ---------------------------------------------------------------

fn validate_elem(kind: ElemKind, s: &str, line_no: usize) -> Result<(), UniverseError> {
    match kind {
        ElemKind::PmMarket => validate_pm_entry(s, line_no),
        ElemKind::BnSymbol => validate_bn_symbol(s, line_no),
        ElemKind::Instrument => validate_name(s, INSTRUMENT_LEN_MAX, "instrument", line_no),
        ElemKind::HlCoin => validate_name(s, HL_COIN_LEN_MAX, "coin", line_no),
        ElemKind::PairRef => validate_pair_ref(s, line_no).map(|_| ()),
    }
}

fn is_pm_token(s: &str) -> bool {
    (PM_TOKEN_LEN_MIN..=PM_TOKEN_LEN_MAX).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_digit())
}

fn validate_pm_entry(s: &str, line_no: usize) -> Result<(), UniverseError> {
    let mut parts = s.split(':');
    let first = parts.next().unwrap_or("");
    match (parts.next(), parts.next()) {
        (None, _) => {
            if is_pm_token(first) {
                Ok(())
            } else {
                Err(err(
                    line_no,
                    format!(
                        "bad PM token id `{s}` (want {PM_TOKEN_LEN_MIN}..={PM_TOKEN_LEN_MAX} \
                         decimal digits, or `yes:no`)"
                    ),
                ))
            }
        }
        (Some(second), None) => {
            if !is_pm_token(first) || !is_pm_token(second) {
                return Err(err(
                    line_no,
                    format!("bad PM `yes:no` entry `{s}` (both sides must be token ids)"),
                ));
            }
            if first == second {
                return Err(err(
                    line_no,
                    "PM `yes:no` entry has identical sides".to_string(),
                ));
            }
            Ok(())
        }
        (Some(_), Some(_)) => Err(err(
            line_no,
            format!("bad PM entry `{s}` (more than one `:`)"),
        )),
    }
}

fn validate_bn_symbol(s: &str, line_no: usize) -> Result<(), UniverseError> {
    let ok = !s.is_empty()
        && s.len() <= BN_SYMBOL_LEN_MAX
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(err(
            line_no,
            format!("bad Binance symbol `{s}` (want lowercase [a-z0-9], 1..={BN_SYMBOL_LEN_MAX})"),
        ))
    }
}

fn validate_name(s: &str, max: usize, what: &str, line_no: usize) -> Result<(), UniverseError> {
    let ok = !s.is_empty()
        && s.len() <= max
        && s.bytes().all(|b| (0x21..=0x7E).contains(&b) && b != b'"');
    if ok {
        Ok(())
    } else {
        Err(err(
            line_no,
            format!("bad {what} `{s}` (want printable ASCII, no spaces, 1..={max})"),
        ))
    }
}

fn validate_pair_ref(s: &str, line_no: usize) -> Result<(u32, u32), UniverseError> {
    let bad = || {
        err(
            line_no,
            format!("bad pair `{s}` (want `P:B` with 0-based decimal indices)"),
        )
    };
    let mut parts = s.split(':');
    let p = parts.next().ok_or_else(bad)?;
    let bsym = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() || p.is_empty() || bsym.is_empty() {
        return Err(bad());
    }
    let p: u32 = p.parse().map_err(|_| bad())?;
    let bsym: u32 = bsym.parse().map_err(|_| bad())?;
    Ok((p, bsym))
}

// ---------------------------------------------------------------
// Builder storage + finalize
// ---------------------------------------------------------------

fn store_array(
    b: &mut Builder,
    slot: Slot,
    items: Vec<String>,
    line_no: usize,
) -> Result<(), UniverseError> {
    let dup = |name: &str| err(line_no, format!("duplicate key `{name}`"));
    match slot {
        Slot::PmMarkets => {
            if b.pm_markets.replace(items).is_some() {
                return Err(dup("markets"));
            }
        }
        Slot::BnSpot => {
            if b.bn_spot.replace(items).is_some() {
                return Err(dup("spot"));
            }
        }
        Slot::BnUsdm => {
            if b.bn_usdm.replace(items).is_some() {
                return Err(dup("usdm"));
            }
        }
        Slot::OkxInstr => {
            if b.okx_instr.replace(items).is_some() {
                return Err(dup("instruments"));
            }
        }
        Slot::DeribitInstr => {
            if b.deribit_instr.replace(items).is_some() {
                return Err(dup("instruments"));
            }
        }
        Slot::HlCoins => {
            if b.hl_coins.replace(items).is_some() {
                return Err(dup("coins"));
            }
        }
        Slot::PairsMap => {
            if b.pairs_map.replace(items).is_some() {
                return Err(dup("map"));
            }
        }
        Slot::OkxDepth | Slot::DeribitDepth => unreachable!("bool slots never store arrays"),
    }
    Ok(())
}

fn store_bool(b: &mut Builder, slot: Slot, v: bool, line_no: usize) -> Result<(), UniverseError> {
    match slot {
        Slot::OkxDepth => {
            if b.okx_depth.replace(v).is_some() {
                return Err(err(line_no, "duplicate key `depth`"));
            }
        }
        Slot::DeribitDepth => {
            if b.deribit_depth.replace(v).is_some() {
                return Err(err(line_no, "duplicate key `depth`"));
            }
        }
        _ => unreachable!("array slots never store bools"),
    }
    Ok(())
}

fn check_unique(list: &[String], what: &str) -> Result<(), UniverseError> {
    for i in 1..list.len() {
        for j in 0..i {
            if list[i] == list[j] {
                return Err(err(0, format!("duplicate {what} `{}`", list[i])));
            }
        }
    }
    Ok(())
}

fn check_cap(len: usize, cap: usize, what: &str) -> Result<(), UniverseError> {
    if len > cap {
        return Err(err(0, format!("too many {what} ({len} > cap {cap})")));
    }
    Ok(())
}

fn finalize(b: Builder) -> Result<Universe, UniverseError> {
    // PM entries: parse the string forms into the typed model.
    let mut pm_markets: Vec<PmMarket> = Vec::new();
    let raw_pm = b.pm_markets.unwrap_or_default();
    for i in 0..raw_pm.len() {
        let s = raw_pm[i].as_str();
        match s.split_once(':') {
            None => pm_markets.push(PmMarket::Single(s.to_string())),
            Some((yes, no)) => pm_markets.push(PmMarket::YesNo {
                yes: yes.to_string(),
                no: no.to_string(),
            }),
        }
    }
    let binance_spot = b.bn_spot.unwrap_or_default();
    let binance_usdm = b.bn_usdm.unwrap_or_default();
    let okx_instruments = b.okx_instr.unwrap_or_default();
    let deribit_instruments = b.deribit_instr.unwrap_or_default();
    let hl_coins = b.hl_coins.unwrap_or_default();

    // Caps.
    check_cap(pm_markets.len(), PM_MARKETS_MAX, "PM markets")?;
    check_cap(binance_spot.len(), VENUE_LIST_MAX, "Binance spot symbols")?;
    check_cap(binance_usdm.len(), VENUE_LIST_MAX, "Binance usdm symbols")?;
    check_cap(okx_instruments.len(), VENUE_LIST_MAX, "OKX instruments")?;
    check_cap(deribit_instruments.len(), VENUE_LIST_MAX, "Deribit instruments")?;
    check_cap(hl_coins.len(), VENUE_LIST_MAX, "Hyperliquid coins")?;

    // Within-list duplicates (a duplicate = double subscribe + two
    // ids for one stream — always a config mistake).
    let mut pm_tokens_flat: Vec<String> = Vec::new();
    for i in 0..pm_markets.len() {
        match &pm_markets[i] {
            PmMarket::Single(t) => pm_tokens_flat.push(t.clone()),
            PmMarket::YesNo { yes, no } => {
                pm_tokens_flat.push(yes.clone());
                pm_tokens_flat.push(no.clone());
            }
        }
    }
    check_unique(&pm_tokens_flat, "PM token id")?;
    check_unique(&binance_spot, "Binance spot symbol")?;
    check_unique(&binance_usdm, "Binance usdm symbol")?;
    check_unique(&okx_instruments, "OKX instrument")?;
    check_unique(&deribit_instruments, "Deribit instrument")?;
    check_unique(&hl_coins, "Hyperliquid coin")?;

    // Pairs: re-parse (validated per element already), range-check.
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    let raw_pairs = b.pairs_map.unwrap_or_default();
    for i in 0..raw_pairs.len() {
        let (p, bsym) = validate_pair_ref(&raw_pairs[i], 0)?;
        if p as usize >= pm_markets.len() {
            return Err(err(
                0,
                format!("pair `{}` references PM market {p} but only {} configured", raw_pairs[i], pm_markets.len()),
            ));
        }
        if bsym as usize >= binance_spot.len() {
            return Err(err(
                0,
                format!("pair `{}` references Binance spot {bsym} but only {} configured", raw_pairs[i], binance_spot.len()),
            ));
        }
        if pairs.contains(&(p, bsym)) {
            return Err(err(0, format!("duplicate pair `{}`", raw_pairs[i])));
        }
        pairs.push((p, bsym));
    }

    Ok(Universe {
        pm_markets,
        binance_spot,
        binance_usdm,
        okx_instruments,
        okx_depth: b.okx_depth.unwrap_or(false),
        deribit_instruments,
        deribit_depth: b.deribit_depth.unwrap_or(false),
        hl_coins,
        pairs,
    })
}

// ---------------------------------------------------------------
// Allocation (the M1 SymbolId law)
// ---------------------------------------------------------------

/// Apply the M1 allocation law to a parsed universe (see the module
/// docs and `docs/mvp-progress.md` M1a design):
///
/// - PM tokens flatten in file order (YES then NO per pair entry);
///   token\[0\] → [`LEGACY_PM_ANCHOR_SYM`], token\[i≥1\] →
///   `make_symbol_id(Polymarket, i+1)`.
/// - Binance spot\[0\] → [`LEGACY_BN_ANCHOR_SYM`], spot\[i≥1\] →
///   `make_symbol_id(Binance, i+1)`; usdm\[j\] →
///   `make_symbol_id(Binance, BN_USDM_ORDINAL_BASE + j + 1)`.
/// - OKX/Deribit/HL: `make_symbol_id(venue, i+1)` (the standing 8e
///   flag-order convention, now file-order).
/// - Any duplicate id across the whole universe is a fatal error
///   (the flat legacy anchors could only collide at absurd list
///   sizes — the check makes it impossible SILENTLY).
/// - Empty `[pairs]` with both sides non-empty injects the default
///   pair (market 0 × spot 0).
pub fn allocate(u: &Universe) -> Result<AllocatedUniverse, UniverseError> {
    allocate_with_anchors(u, LEGACY_PM_ANCHOR_SYM, LEGACY_BN_ANCHOR_SYM)
}

/// [`allocate`] with explicit anchor ids for PM token\[0\] and
/// Binance spot\[0\] — the cli's legacy `--polymarket-sym-id` /
/// `--binance-sym-id` single-market override lane. The universe-wide
/// duplicate check covers custom anchors like any other id.
pub fn allocate_with_anchors(
    u: &Universe,
    pm_anchor: SymbolId,
    bn_anchor: SymbolId,
) -> Result<AllocatedUniverse, UniverseError> {
    let mut out = AllocatedUniverse::default();

    // PM tokens.
    let mut flat = 0u32;
    for m in 0..u.pm_markets.len() {
        let midx = m as u32;
        let push_token = |token: &str, is_yes: bool, flat: &mut u32, out: &mut AllocatedUniverse| {
            let sym = if *flat == 0 {
                pm_anchor
            } else {
                make_symbol_id(VenueId::Polymarket, *flat + 1)
            };
            out.pm_tokens.push(PmToken {
                sym,
                token_id: token.to_string(),
                market_index: midx,
                is_yes,
            });
            *flat += 1;
        };
        match &u.pm_markets[m] {
            PmMarket::Single(t) => push_token(t, true, &mut flat, &mut out),
            PmMarket::YesNo { yes, no } => {
                push_token(yes, true, &mut flat, &mut out);
                push_token(no, false, &mut flat, &mut out);
            }
        }
    }

    // Binance spot + usdm.
    for i in 0..u.binance_spot.len() {
        let sym = if i == 0 {
            bn_anchor
        } else {
            make_symbol_id(VenueId::Binance, i as u32 + 1)
        };
        let name = u.binance_spot[i].clone();
        out.bn_spot.push(Instrument {
            sym,
            descriptor: format!("binance:{name}"),
            name,
        });
    }
    for j in 0..u.binance_usdm.len() {
        let sym = make_symbol_id(VenueId::Binance, BN_USDM_ORDINAL_BASE + j as u32 + 1);
        let name = u.binance_usdm[j].clone();
        out.bn_usdm.push(Instrument {
            sym,
            descriptor: format!("binance-usdm:{name}"),
            name,
        });
    }

    // OKX / Deribit / HL — the standing per-venue convention.
    for i in 0..u.okx_instruments.len() {
        let name = u.okx_instruments[i].clone();
        out.okx.push(Instrument {
            sym: make_symbol_id(VenueId::Okx, i as u32 + 1),
            descriptor: format!("okx:{name}"),
            name,
        });
    }
    for i in 0..u.deribit_instruments.len() {
        let name = u.deribit_instruments[i].clone();
        out.deribit.push(Instrument {
            sym: make_symbol_id(VenueId::Deribit, i as u32 + 1),
            descriptor: format!("deribit:{name}"),
            name,
        });
    }
    for i in 0..u.hl_coins.len() {
        let name = u.hl_coins[i].clone();
        out.hl.push(Instrument {
            sym: make_symbol_id(VenueId::Hyperliquid, i as u32 + 1),
            descriptor: format!("hyperliquid:{name}"),
            name,
        });
    }

    // Universe-wide duplicate-id check (fail fast, name both sides).
    let mut all: Vec<(SymbolId, &str)> = Vec::new();
    for t in &out.pm_tokens {
        all.push((t.sym, t.token_id.as_str()));
    }
    for group in [&out.bn_spot, &out.bn_usdm, &out.okx, &out.deribit, &out.hl] {
        for inst in group {
            all.push((inst.sym, inst.descriptor.as_str()));
        }
    }
    for i in 1..all.len() {
        for j in 0..i {
            if all[i].0 == all[j].0 {
                return Err(err(
                    0,
                    format!(
                        "SymbolId collision: {} and {} both allocate id {} — \
                         shrink the colliding list (legacy anchors 42/7 reserve those ids)",
                        all[j].1, all[i].1, all[i].0
                    ),
                ));
            }
        }
    }

    // Pairs: explicit refs, or the default (0,0) when both sides exist.
    if u.pairs.is_empty() {
        if !u.pm_markets.is_empty() && !u.binance_spot.is_empty() {
            out.pairs.push((pm_market_yes_sym(&out, 0), out.bn_spot[0].sym));
        }
    } else {
        for k in 0..u.pairs.len() {
            let (p, bsym) = u.pairs[k];
            out.pairs.push((pm_market_yes_sym(&out, p), out.bn_spot[bsym as usize].sym));
        }
    }

    Ok(out)
}

/// The YES-token sym of market entry `market_index` (first flattened
/// token of that entry). Callers pass range-checked indices (parse
/// enforced them); debug-asserted here.
fn pm_market_yes_sym(a: &AllocatedUniverse, market_index: u32) -> SymbolId {
    for t in &a.pm_tokens {
        if t.market_index == market_index && t.is_yes {
            return t.sym;
        }
    }
    debug_assert!(false, "pair references a market with no YES token");
    SymbolId::MAX
}

/// The M1 boot refusal law for config-driven boots: at least one PM
/// market (venue-blind refusal, extended) and at least one Binance
/// spot symbol (the latency-arb pair anchor; relaxing this is
/// deliberately deferred past M1), which together guarantee at least
/// one pair.
pub fn assert_bootable(a: &AllocatedUniverse) -> Result<(), UniverseError> {
    if a.pm_tokens.is_empty() {
        return Err(err(0, "no Polymarket markets configured — boot refuses to run venue-blind"));
    }
    if a.bn_spot.is_empty() {
        return Err(err(0, "no Binance spot symbols configured — the latency-arb pair anchor is required in M1"));
    }
    debug_assert!(!a.pairs.is_empty(), "both sides exist ⇒ default pair injected");
    Ok(())
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "57748138085022719760345772310040703848567377822400132842014290209986511882046";
    const T2: &str = "1234567890123456789";
    const T3: &str = "9876543210987654321";

    fn full_src() -> String {
        format!(
            r##"
# full example (mirrors universe.toml.example)
[polymarket]
markets = [
  "{T1}",
  "{T2}:{T3}",   # a YES/NO pair
]

[binance]
spot = ["btcusdt", "ethusdt"]
usdm = ["btcusdt"]

[okx]
instruments = ["BTC-USDT", "ETH-USDT-SWAP"]
depth = false

[deribit]
instruments = ["BTC-PERPETUAL"]
depth = true

[hyperliquid]
coins = ["BTC", "#330"]

[pairs]
map = ["0:0", "1:1"]
"##
        )
    }

    #[test]
    fn full_example_parses() {
        let u = parse(&full_src()).expect("full example must parse");
        assert_eq!(u.pm_markets.len(), 2);
        assert_eq!(u.pm_markets[0], PmMarket::Single(T1.to_string()));
        assert_eq!(
            u.pm_markets[1],
            PmMarket::YesNo {
                yes: T2.to_string(),
                no: T3.to_string()
            }
        );
        assert_eq!(u.binance_spot, vec!["btcusdt", "ethusdt"]);
        assert_eq!(u.binance_usdm, vec!["btcusdt"]);
        assert_eq!(u.okx_instruments, vec!["BTC-USDT", "ETH-USDT-SWAP"]);
        assert!(!u.okx_depth);
        assert_eq!(u.deribit_instruments, vec!["BTC-PERPETUAL"]);
        assert!(u.deribit_depth);
        assert_eq!(u.hl_coins, vec!["BTC", "#330"]);
        assert_eq!(u.pairs, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn full_example_allocates_per_the_law() {
        let a = allocate(&parse(&full_src()).unwrap()).expect("allocates");
        // PM: token[0] = legacy 42; then namespaced ordinals 2, 3.
        assert_eq!(a.pm_tokens[0].sym, 42);
        assert!(a.pm_tokens[0].is_yes);
        assert_eq!(a.pm_tokens[1].sym, make_symbol_id(VenueId::Polymarket, 2));
        assert_eq!(a.pm_tokens[2].sym, make_symbol_id(VenueId::Polymarket, 3));
        assert!(!a.pm_tokens[2].is_yes);
        assert_eq!(a.pm_tokens[2].market_index, 1);
        // BN: spot[0] = legacy 7; spot[1] namespaced; usdm block base.
        assert_eq!(a.bn_spot[0].sym, 7);
        assert_eq!(a.bn_spot[0].descriptor, "binance:btcusdt");
        assert_eq!(a.bn_spot[1].sym, make_symbol_id(VenueId::Binance, 2));
        assert_eq!(
            a.bn_usdm[0].sym,
            make_symbol_id(VenueId::Binance, BN_USDM_ORDINAL_BASE + 1)
        );
        assert_eq!(a.bn_usdm[0].descriptor, "binance-usdm:btcusdt");
        // Standing convention venues.
        assert_eq!(a.okx[0].sym, make_symbol_id(VenueId::Okx, 1));
        assert_eq!(a.okx[1].descriptor, "okx:ETH-USDT-SWAP");
        assert_eq!(a.deribit[0].sym, make_symbol_id(VenueId::Deribit, 1));
        assert_eq!(a.hl[1].descriptor, "hyperliquid:#330");
        // Pairs resolve to (yes sym, spot sym).
        assert_eq!(a.pairs, vec![(42, 7), (make_symbol_id(VenueId::Polymarket, 2), make_symbol_id(VenueId::Binance, 2))]);
        assert_bootable(&a).expect("bootable");
    }

    #[test]
    fn empty_source_parses_empty_and_refuses_boot() {
        let u = parse("").expect("empty parses");
        assert_eq!(u, Universe::default());
        let a = allocate(&u).expect("empty allocates");
        assert!(a.pairs.is_empty());
        let e = assert_bootable(&a).unwrap_err();
        assert!(e.msg.contains("venue-blind"), "{e}");
    }

    #[test]
    fn custom_anchors_flow_through() {
        let src = format!("[polymarket]\nmarkets=[\"{T1}\"]\n[binance]\nspot=[\"btcusdt\"]\n");
        let u = parse(&src).unwrap();
        let a = allocate_with_anchors(&u, 99, 1234).unwrap();
        assert_eq!(a.pm_tokens[0].sym, 99);
        assert_eq!(a.bn_spot[0].sym, 1234);
        assert_eq!(a.pairs, vec![(99, 1234)]);
    }

    #[test]
    fn default_pair_injected_when_map_absent() {
        let src = format!("[polymarket]\nmarkets=[\"{T1}\"]\n[binance]\nspot=[\"btcusdt\"]\n");
        let a = allocate(&parse(&src).unwrap()).unwrap();
        assert_eq!(a.pairs, vec![(42, 7)]);
    }

    #[test]
    fn hash_inside_string_is_not_a_comment() {
        let src = "[hyperliquid]\ncoins = [\"#330\"] # trailing comment\n";
        let u = parse(src).unwrap();
        assert_eq!(u.hl_coins, vec!["#330"]);
    }

    #[test]
    fn multiline_array_with_trailing_comma_and_comments() {
        let src = format!(
            "[binance]\nspot = [\n  \"btcusdt\",  # main\n\n  \"ethusdt\",\n]\n"
        );
        let u = parse(&src).unwrap();
        assert_eq!(u.binance_spot, vec!["btcusdt", "ethusdt"]);
    }

    #[test]
    fn unknown_section_is_fatal() {
        let e = parse("[binanse]\n").unwrap_err();
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("unknown section"), "{e}");
    }

    #[test]
    fn unknown_key_is_fatal() {
        let e = parse("[binance]\nspots = [\"btcusdt\"]\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("unknown key"), "{e}");
    }

    #[test]
    fn key_outside_section_is_fatal() {
        let e = parse("spot = [\"btcusdt\"]\n").unwrap_err();
        assert!(e.msg.contains("before any [section]"), "{e}");
    }

    #[test]
    fn duplicate_key_is_fatal() {
        let e = parse("[binance]\nspot=[\"a1\"]\nspot=[\"b2\"]\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("duplicate key"), "{e}");
    }

    #[test]
    fn bad_bool_is_fatal() {
        let e = parse("[okx]\ndepth = yes\n").unwrap_err();
        assert!(e.msg.contains("expects `true` or `false`"), "{e}");
    }

    #[test]
    fn missing_equals_is_fatal() {
        let e = parse("[okx]\ndepth true\n").unwrap_err();
        assert!(e.msg.contains("expected `key = value`"), "{e}");
    }

    #[test]
    fn unterminated_string_is_fatal() {
        let e = parse("[binance]\nspot = [\"btcusdt\n").unwrap_err();
        assert!(e.msg.contains("unterminated string"), "{e}");
    }

    #[test]
    fn escape_in_string_is_fatal() {
        let e = parse("[binance]\nspot = [\"btc\\usdt\"]\n").unwrap_err();
        assert!(e.msg.contains("escape sequences"), "{e}");
    }

    #[test]
    fn unclosed_array_is_fatal_at_open_line() {
        let e = parse("[binance]\nspot = [\n  \"btcusdt\",\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.msg.contains("never closed"), "{e}");
    }

    #[test]
    fn content_after_close_bracket_is_fatal() {
        let e = parse("[binance]\nspot = [\"a1\"] junk\n").unwrap_err();
        assert!(e.msg.contains("after `]`"), "{e}");
    }

    #[test]
    fn missing_comma_between_elements_is_fatal() {
        let e = parse("[binance]\nspot = [\"a1\" \"b2\"]\n").unwrap_err();
        assert!(e.msg.contains("missing `,`"), "{e}");
    }

    #[test]
    fn double_comma_is_fatal() {
        let e = parse("[binance]\nspot = [\"a1\",, \"b2\"]\n").unwrap_err();
        assert!(e.msg.contains("unexpected `,`"), "{e}");
    }

    #[test]
    fn uppercase_bn_symbol_rejected() {
        let e = parse("[binance]\nspot = [\"BTCUSDT\"]\n").unwrap_err();
        assert!(e.msg.contains("bad Binance symbol"), "{e}");
    }

    #[test]
    fn pm_entry_shapes_rejected() {
        let cases: Vec<String> = vec![
            "123".into(),                     // too short
            "a2345678901".into(),             // non-digit
            "1:2".into(),                     // pair sides too short
            format!("{T1}:{T1}"),             // identical sides
            format!("{T1}:{T2}:{T3}"),        // two colons
        ];
        for bad in &cases {
            let src = format!("[polymarket]\nmarkets=[\"{bad}\"]\n");
            let e = parse(&src).unwrap_err();
            assert!(
                e.msg.contains("PM"),
                "entry `{bad}` should be rejected with a PM message, got: {e}"
            );
        }
    }

    #[test]
    fn duplicate_pm_token_across_entries_rejected() {
        let src = format!("[polymarket]\nmarkets=[\"{T1}\", \"{T1}:{T2}\"]\n");
        let e = parse(&src).unwrap_err();
        assert!(e.msg.contains("duplicate PM token"), "{e}");
    }

    #[test]
    fn pair_out_of_range_rejected() {
        let src = format!(
            "[polymarket]\nmarkets=[\"{T1}\"]\n[binance]\nspot=[\"btcusdt\"]\n[pairs]\nmap=[\"1:0\"]\n"
        );
        let e = parse(&src).unwrap_err();
        assert!(e.msg.contains("references PM market 1"), "{e}");
    }

    #[test]
    fn duplicate_pair_rejected() {
        let src = format!(
            "[polymarket]\nmarkets=[\"{T1}\"]\n[binance]\nspot=[\"btcusdt\"]\n[pairs]\nmap=[\"0:0\",\"0:0\"]\n"
        );
        let e = parse(&src).unwrap_err();
        assert!(e.msg.contains("duplicate pair"), "{e}");
    }

    #[test]
    fn pm_markets_cap_enforced() {
        let mut src = String::from("[polymarket]\nmarkets=[\n");
        for i in 0..(PM_MARKETS_MAX + 1) {
            src.push_str(&format!("\"{:010}1234567890\",\n", i));
        }
        src.push_str("]\n");
        let e = parse(&src).unwrap_err();
        assert!(e.msg.contains("too many PM markets"), "{e}");
    }

    #[test]
    fn anchor_collision_caught_by_dup_check() {
        // 42 PM tokens: token[41] would take namespaced ordinal 42 =
        // flat 42 (venue byte 0) — colliding with the anchor.
        let mut src = String::from("[polymarket]\nmarkets=[\n");
        for i in 0..42 {
            src.push_str(&format!("\"{:010}9876543210\",\n", i));
        }
        src.push_str("]\n");
        let u = parse(&src).expect("parses (under the entries cap)");
        let e = allocate(&u).unwrap_err();
        assert!(e.msg.contains("SymbolId collision"), "{e}");
        assert!(e.msg.contains("42"), "{e}");
    }

    #[test]
    fn load_missing_file_is_line0_error() {
        let e = load(Path::new("/nonexistent/universe-m1-test.toml")).unwrap_err();
        assert_eq!(e.line, 0);
        assert!(e.msg.contains("cannot read"), "{e}");
    }

    #[test]
    fn error_display_includes_line() {
        let e = err(3, "boom");
        assert_eq!(e.to_string(), "universe config line 3: boom");
        let e0 = err(0, "boom");
        assert_eq!(e0.to_string(), "universe config: boom");
    }

    // -----------------------------------------------------------
    // Property tests (house rule §21.3: every parser gets one)
    // -----------------------------------------------------------

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn dedup(v: Vec<String>) -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            for s in v {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
            out
        }

        proptest! {
            /// Generated well-formed configs parse back to exactly the
            /// generated lists, and allocation yields unique ids.
            #[test]
            fn generated_configs_round_trip(
                pm in proptest::collection::vec("[0-9]{12,40}", 0..6),
                spot in proptest::collection::vec("[a-z0-9]{3,12}", 0..6),
                usdm in proptest::collection::vec("[a-z0-9]{3,12}", 0..6),
                okx in proptest::collection::vec("[A-Z0-9-]{3,20}", 0..6),
                depth in proptest::bool::ANY,
            ) {
                let pm = dedup(pm);
                let spot = dedup(spot);
                let usdm = dedup(usdm);
                let okx = dedup(okx);
                let mut src = String::new();
                src.push_str("[polymarket]\nmarkets = [\n");
                for t in &pm { src.push_str(&format!("  \"{t}\", # id\n")); }
                src.push_str("]\n[binance]\n");
                src.push_str(&format!("spot = [{}]\n",
                    spot.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
                src.push_str(&format!("usdm = [{}]\n",
                    usdm.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
                src.push_str(&format!("[okx]\ninstruments = [{}]\ndepth = {depth}\n",
                    okx.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
                let u = parse(&src).expect("generated config must parse");
                prop_assert_eq!(u.binance_spot.clone(), spot);
                prop_assert_eq!(u.binance_usdm.clone(), usdm);
                prop_assert_eq!(u.okx_instruments.clone(), okx);
                prop_assert_eq!(u.okx_depth, depth);
                prop_assert_eq!(u.pm_markets.len(), pm.len());
                let a = allocate(&u).expect("generated config must allocate");
                let mut ids: Vec<SymbolId> = Vec::new();
                for t in &a.pm_tokens { ids.push(t.sym); }
                for g in [&a.bn_spot, &a.bn_usdm, &a.okx] {
                    for i in g { ids.push(i.sym); }
                }
                let n = ids.len();
                ids.sort_unstable();
                ids.dedup();
                prop_assert_eq!(ids.len(), n, "allocation must be duplicate-free");
            }

            /// The parser never panics on printable-ASCII noise — every
            /// outcome is Ok or a structured UniverseError.
            #[test]
            fn parse_never_panics_on_noise(s in "[ -~\\n\\t]{0,512}") {
                let _ = parse(&s);
            }
        }
    }
}
