// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Phase-8g ruleset stage/commit side path: §4.2 validator + state
//! machine (grown from the 8f stub per design §5 — semantics kept,
//! the "drop after hash" is replaced by parse-and-validate into a
//! preallocated scratch [`RuleTable`]).
//!
//! Sits behind the §4.4 step-8 seam: the listener routes accepted
//! `RulesetStage` / `RulesetCommit` commands here after `try_push`.
//!
//! * **Stage**: resolve `AI_RULESET_DIR/<hash128-hex>.json` — the frame
//!   carries only hash128 (8f §13 decision 5), so the filename MUST be
//!   derivable from it; the convention (first 32 hex chars of the full
//!   SHA-256) is taught in `docs/prompts/ai-session.md` §4 step 5 —
//!   read the file and run the §4.2 validator ([`validate_ruleset`]):
//!   rule 1 recomputes the FULL SHA-256 (`core-crypto`) and requires
//!   its first 16 bytes to equal the frame's hash128, then rules 2–8
//!   scan the bytes into the side-path scratch table. Pass ⇒ stamp
//!   the candidate epoch and `try_push` the scratch into the §6
//!   `Ring<RuleTableSlot, RULE_TABLE_RING_SLOTS>` (8g item 4 — v2-typed
//!   slots since VM2 V4);
//!   push ok ⇒ staged state + `engine_ai_ruleset_staged_total`;
//!   push-full ⇒ REJECT (`engine_ai_ruleset_rejected_total` + the
//!   dedicated `engine_ai_table_push_fail_total`), staged/committed
//!   unchanged (§5). Any validator failure ⇒
//!   `engine_ai_ruleset_rejected_total`, nothing pushed.
//! * **Commit**: valid only for the currently staged hash ⇒ committed
//!   state flag (observable via `engine_ai_ruleset_committed_total`);
//!   anything else ⇒ rejected. A later successful Stage supersedes a
//!   Commit (committed state clears) — the worker-side registry
//!   (`state.py`) mirrors exactly this machine.
//!
//! ## Validator discipline (§4.2)
//!
//! Single forward pass, flat state machine, no recursion, over
//! `&[u8]` — **no `serde_json`** (untrusted bytes: the artifact file
//! is operator-installed but the frame that names it is
//! network-adjacent, and the file can be swapped on disk). Rules are
//! applied in §4.2 order at each scan position; across positions the
//! earliest-position failure wins (streaming — the deterministic
//! reading of "first failure wins" that needs no second pass). Rule 1
//! strictly precedes all parsing; rule 4's lower bound is checked
//! after grammar completes (rule order 2 < 4), its upper bound fires
//! the moment a 257th row opens.
//!
//! Trailing bytes (rule 2): only JSON-insignificant ASCII whitespace
//! may follow the closing brace — G1 documented interpretation: the
//! hash already pins content byte-exactly, and rejecting a trailing
//! newline would buy no integrity while breaking hand-installed
//! artifacts; any trailing NON-whitespace byte rejects.
//!
//! ## Allocation note (doctrine)
//!
//! `PathBuf::join` and `std::fs::read` allocate: **documented copy
//! #0**, operator cadence only — only Stage/Commit kinds are routed
//! here, never market data, and the frame has already been captured
//! and pushed. The validator seam itself ([`validate_ruleset`]) is
//! 0 B/op into the scratch table preallocated at construction (alloc
//! gate 34, `bench/tests/alloc_assertions.rs`); the admit→verify→
//! capture→push pump keeps its own 0 B/op gate.
//!
//! The stage-time table handoff is **documented copy #1** (§6):
//! scratch → ring slot via `try_push`, 32 KiB + 64 by value, once per
//! successful Stage — operator cadence, moves bytes, never the heap
//! (alloc gate 35). Copy #2 (ring slot → the vm member's staged
//! buffer at the engine pop) lands with item 7.

use std::path::PathBuf;
use std::sync::Arc;

use core_crypto::sha256;
use core_parse::{scan_price_1e6, scan_price_1e9, scan_u64};
use core_ring::Producer;
use core_types::{
    fnv1a_64, AiCmd, AiCmdKind, CombineOp, FeatId, RuleRow, RuleRowV2, RuleTableSlot,
    RuleTableV2, SymbolId, CMP_CONFIRM_ABS, CMP_CONFIRM_LE, CMP_CONFIRM_PAIR, CMP_ENTRY_ABS,
    CMP_ENTRY_LE, FEAT_NONE, GROUP_NONE, ROLL_WINDOW_MAX_MIN, ROW_FLAG_POSITION,
    RULE_TABLE_RING_SLOTS, RULE_TABLE_ROWS, SYMBOL_ID_NONE,
};

use crate::status::AiIngressStatus;

/// Bytes of the truncated ruleset identity carried in `px`+`qty`
/// (8f §13 decision 5).
pub const HASH128_LEN: usize = 16;

/// File-name suffix of ruleset artifacts in `AI_RULESET_DIR`.
const SUFFIX: &[u8; 5] = b".json";

// ---------------------------------------------------------------
// §4.2 rule-3 domain bounds and rule-7 caps
// ---------------------------------------------------------------

/// Rule 3: `edge_bps` upper bound.
pub const RULE_EDGE_BPS_MAX: u32 = 10_000;
/// Rule 3: `horizon_ms` lower bound.
pub const RULE_HORIZON_MS_MIN: u32 = 10;
/// Rule 3: `horizon_ms` upper bound (24 h).
pub const RULE_HORIZON_MS_MAX: u32 = 86_400_000;
/// Rule 3: `level` upper bound ×1e6 (Polymarket price domain `[0, 1]`).
pub const RULE_LEVEL_1E6_MAX: i64 = 1_000_000;
/// Rule 7: per-row notional cap ×1e6 — tighten-only mirror of the
/// `docs/risk-policy.md` max single-order notional ($100).
// Operator ruling 2026-08-29 (M5 $50k-book research tier; DD 15%):
// per-order $10k, per-sym $20k, table $100k (2x book). The $1k demo
// tier is recorded as superseded in docs/risk-policy.md.
pub const RULE_ROW_MAX_RISK_1E6: i64 = 10_000_000_000;
/// Rule 7: per-symbol Σ cap ×1e6 — mirror of the risk-policy max net
/// notional per symbol ($250).
pub const RULE_SYM_MAX_RISK_1E6: i64 = 20_000_000_000;
/// Rule 7: whole-table Σ cap ×1e6 — mirror of the risk-policy max net
/// notional total ($1 000).
pub const RULE_TABLE_MAX_RISK_1E6: i64 = 100_000_000_000;

/// §4.2 reject reason — one variant per rule, first failure wins
/// (streaming discipline, module docs). The ops surface stays the
/// single `engine_ai_ruleset_rejected_total` counter; the variant
/// exists for tests and future diagnostics.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RulesetReject {
    /// Rule 1 — full-SHA-256 prefix ≠ the frame's hash128.
    HashMismatch,
    /// Rule 2 — grammar: malformed shape, unknown/duplicate/missing
    /// key, string escape or control byte, trailing non-whitespace.
    Grammar,
    /// Rule 3 — number lexical/domain: exponent, fractional part in
    /// an integer field, negative where unsigned, NaN/Inf token,
    /// range breach, oversized literal.
    Number,
    /// Rule 4 — `rows` count ∉ `[1, 256]`.
    RowCount,
    /// Rule 5 — `name` not ASCII, len ∉ `[1, 64]`, or `name_h`
    /// collision with an earlier row.
    Name,
    /// Rule 6 — symbol legs: universe membership (both legs, no venue
    /// restriction — D2 as amended), `ref == sym`, or a `ref` present
    /// on `level_breach`.
    Symbol,
    /// Rule 7 — risk caps (tighten-only vs `docs/risk-policy.md`).
    Caps,
    /// Rule 8 — exact-duplicate row `(sym, trigger, side, ref/level)`
    /// (v1) / the v2 identity tuple (docs on the v2 arm).
    DuplicateRow,
    /// VM2 V4 (D-6) — a v2 row's `instrument`/`ref` descriptor did
    /// not resolve against the LIVE boot universe (fail-fast REFUSE;
    /// #7b re-commit re-resolves every boot).
    Descriptor,
    /// VM2 V4 rule 10 — a referenced feature does not exist for the
    /// resolved instrument's channels (no depth rows on a sym
    /// without depth), a window violates the feature's window law,
    /// or the table exceeds the rolling-bind budget.
    Feature,
    /// VM2 V4 rule 9 — position/refire shape violations: `exit`
    /// present without position semantics being expressible, or
    /// `min_hold_s`/`max_hold_s`/`group` on a refire row, or a
    /// position row without `exit`.
    Position,
}

// ---------------------------------------------------------------
// VM2 V4 (D-6): descriptor resolution + channel capabilities
// ---------------------------------------------------------------

/// Capability bit: the instrument has a BBO tick lane (price
/// features + rolling stats evaluate).
pub const CAP_PRICE: u8 = 1;
/// Capability bit: a funding channel exists (APR + clock features).
pub const CAP_FUNDING: u8 = 2;
/// Capability bit: an L2 depth subscription exists (depth features).
pub const CAP_DEPTH: u8 = 4;
/// Capability bit: an OptSummary channel exists (mark/IV features).
pub const CAP_OPT: u8 = 8;

/// §9.4-descriptor → (SymbolId, capability bits) table for the v2
/// grammar's commit-time resolution (D-6). Built by the bin from the
/// SAME allocation truth that writes `instrument-manifest.tsv`
/// (live), or from a run's manifest via [`caps_of_descriptor`]
/// (offline harness). Operator-cadence structure — allocation is
/// fine here (boot/backtest setup only).
pub struct DescriptorTable {
    /// Sorted by descriptor bytes (binary-searched per row leg).
    entries: Vec<(String, SymbolId, u8)>,
}

impl DescriptorTable {
    /// An empty table: every v2 descriptor rejects
    /// ([`RulesetReject::Descriptor`]) — the honest posture for
    /// pre-D3 captures; v1 rows are unaffected (sym universe).
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Build from (descriptor, sym, caps) rows; sorts and dedups
    /// (first entry wins on a duplicate name — allocation emits no
    /// duplicates; defensive only).
    pub fn from_entries(mut entries: Vec<(String, SymbolId, u8)>) -> Self {
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        entries.dedup_by(|a, b| a.0 == b.0);
        Self { entries }
    }

    /// Resolve a descriptor's bytes. `None` = not in the boot
    /// universe (rule-6/D-6 refuse).
    pub fn resolve(&self, desc: &[u8]) -> Option<(SymbolId, u8)> {
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = &self.entries[mid];
            match e.0.as_bytes().cmp(desc) {
                core::cmp::Ordering::Equal => return Some((e.1, e.2)),
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Entry count (diagnostics).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no descriptors resolve.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The OFFLINE capability law: capabilities inferred from the §9.4
/// descriptor string alone (the backtest's manifest path — manifests
/// carry no lane flags). PERMISSIVE by design where the string
/// under-determines the lane (a wrongly GRANTED capability only
/// yields absent-data-holds downstream, never a wrong fire; the
/// bin's live table is exact). The string law:
///
/// * `binance-opt:*` → OPT|PRICE (eapi tickers carry BBO).
/// * `deribit:`/`okx:` names ending `-C`/`-P` → OPT|PRICE.
/// * `deribit:*-PERPETUAL`, `okx:*-SWAP`, `binance-usdm:*`,
///   `bybit-linear:*`, `hyperliquid:<coin>` (no `#` prefix) →
///   PRICE|FUNDING (+DEPTH on okx/deribit).
/// * everything else (PM tokens, spot, dated futures, `#` outcome
///   coins) → PRICE (+DEPTH on okx/deribit non-options).
pub fn caps_of_descriptor(desc: &str) -> u8 {
    let (venue, name) = match desc.split_once(':') {
        Some((v, n)) => (v, n),
        None => return CAP_PRICE, // bare PM token id
    };
    let is_opt = name.ends_with("-C") || name.ends_with("-P");
    match venue {
        "binance-opt" => CAP_OPT | CAP_PRICE,
        "deribit" => {
            if is_opt {
                CAP_OPT | CAP_PRICE
            } else if name.ends_with("-PERPETUAL") {
                CAP_PRICE | CAP_FUNDING | CAP_DEPTH
            } else {
                CAP_PRICE | CAP_DEPTH
            }
        }
        "okx" => {
            if is_opt {
                CAP_OPT
            } else if name.ends_with("-SWAP") {
                CAP_PRICE | CAP_FUNDING | CAP_DEPTH
            } else {
                CAP_PRICE | CAP_DEPTH
            }
        }
        "binance-usdm" => CAP_PRICE | CAP_FUNDING,
        "bybit-linear" => CAP_PRICE | CAP_FUNDING,
        "hyperliquid" => {
            if name.starts_with('#') {
                CAP_PRICE
            } else {
                CAP_PRICE | CAP_FUNDING
            }
        }
        _ => CAP_PRICE,
    }
}

/// Capability bits a [`FeatId`] requires ([`CAP_PRICE`] & co).
#[inline]
pub fn feat_caps_required(f: FeatId) -> u8 {
    if f.requires_price() {
        CAP_PRICE
    } else if f.requires_funding() {
        CAP_FUNDING
    } else if f.requires_depth() {
        CAP_DEPTH
    } else if f.requires_opt_summary() {
        CAP_OPT
    } else {
        0
    }
}

// ---------------------------------------------------------------
// Byte cursor — flat scanner over `&[u8]`, zero-alloc
// ---------------------------------------------------------------

/// Forward-only cursor. All scanning is bounds-checked slice indexing
/// — this is operator-cadence control plane, not a hot loop; safety
/// beats the last nanosecond here.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    #[inline]
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    /// Skip JSON-insignificant whitespace.
    #[inline]
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    /// Consume exactly `want` (post-whitespace) or reject (rule 2).
    #[inline]
    fn eat(&mut self, want: u8) -> Result<(), RulesetReject> {
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == want {
            self.i += 1;
            Ok(())
        } else {
            Err(RulesetReject::Grammar)
        }
    }

    /// Consume `want` if it is the next significant byte.
    #[inline]
    fn eat_if(&mut self, want: u8) -> bool {
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == want {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Peek the next significant byte without consuming.
    #[inline]
    fn peek_is(&mut self, want: u8) -> bool {
        self.skip_ws();
        self.i < self.b.len() && self.b[self.i] == want
    }

    /// True when only trailing whitespace remains.
    #[inline]
    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.i >= self.b.len()
    }
}

// Row-object key presence bits (rule 2: keys at most once; per-arm
// required sets checked at row close). u32 — the union grammar has
// more than eight keys since VM2 V4.
const K_NAME: u32 = 1 << 0;
const K_FAMILY: u32 = 1 << 1;
const K_TRIGGER: u32 = 1 << 2;
const K_SYM: u32 = 1 << 3;
const K_SIDE: u32 = 1 << 4;
const K_EDGE: u32 = 1 << 5;
const K_HORIZON: u32 = 1 << 6;
const K_RISK: u32 = 1 << 7;
const K_INSTRUMENT: u32 = 1 << 8;
const K_REF: u32 = 1 << 9;
const K_FEATURE: u32 = 1 << 10;
const K_REF_FEATURE: u32 = 1 << 11;
const K_COMBINE: u32 = 1 << 12;
const K_WINDOW: u32 = 1 << 13;
const K_REF_WINDOW: u32 = 1 << 14;
const K_CMP: u32 = 1 << 15;
const K_ABS: u32 = 1 << 16;
const K_ENTER: u32 = 1 << 17;
const K_EXIT: u32 = 1 << 18;
const K_C_FEATURE: u32 = 1 << 19;
const K_C_WINDOW: u32 = 1 << 20;
const K_CONFIRM: u32 = 1 << 21;
const K_C_CMP: u32 = 1 << 22;
const K_C_ABS: u32 = 1 << 23;
const K_C_PAIR: u32 = 1 << 24;
const K_GROUP: u32 = 1 << 25;
const K_MIN_HOLD: u32 = 1 << 26;
const K_MAX_HOLD: u32 = 1 << 27;

/// The v1 arm's full required set (all eight, exactly like 8g).
const V1_REQUIRED: u32 =
    K_NAME | K_FAMILY | K_TRIGGER | K_SYM | K_SIDE | K_EDGE | K_HORIZON | K_RISK;
/// Keys legal ONLY in the v1 arm.
const V1_ONLY: u32 = K_TRIGGER | K_SYM | K_EDGE;
/// Keys legal ONLY in the v2 arm.
const V2_ONLY: u32 = K_INSTRUMENT
    | K_REF
    | K_FEATURE
    | K_REF_FEATURE
    | K_COMBINE
    | K_WINDOW
    | K_REF_WINDOW
    | K_CMP
    | K_ABS
    | K_ENTER
    | K_EXIT
    | K_C_FEATURE
    | K_C_WINDOW
    | K_CONFIRM
    | K_C_CMP
    | K_C_ABS
    | K_C_PAIR
    | K_GROUP
    | K_MIN_HOLD
    | K_MAX_HOLD;
/// The v2 arm's required set.
const V2_REQUIRED: u32 = K_NAME | K_INSTRUMENT | K_FEATURE | K_ENTER | K_HORIZON | K_RISK;

// Trigger-object key presence bits (v1 arm).
const T_TYPE: u8 = 1 << 0;
const T_REF: u8 = 1 << 1;
const T_LEVEL: u8 = 1 << 2;

/// Longest known token is `confirm_window_min` (18 B; VM2 V4 grew
/// this — `cross_deviation` was the v1 record-holder); anything
/// longer cannot match a key or enum value and rejects as rule 2.
const KEYWORD_CAP: usize = 24;

/// Descriptor strings are venue-prefixed instrument names; PM token
/// ids run ~78 digits. 128 covers every §9.4 descriptor with margin;
/// longer rejects as rule 2.
const DESCRIPTOR_CAP: usize = 128;

/// Scan a short JSON string (key or enum value) into `out`; rejects
/// escapes and control bytes (rule 2 — the grammar has no escaped
/// tokens) and anything longer than [`KEYWORD_CAP`].
fn scan_keyword(cur: &mut Cur<'_>, out: &mut [u8; KEYWORD_CAP]) -> Result<usize, RulesetReject> {
    cur.eat(b'"')?;
    let mut n = 0usize;
    loop {
        if cur.i >= cur.b.len() {
            return Err(RulesetReject::Grammar);
        }
        let c = cur.b[cur.i];
        cur.i += 1;
        if c == b'"' {
            return Ok(n);
        }
        if c == b'\\' || c < 0x20 {
            return Err(RulesetReject::Grammar);
        }
        if n >= KEYWORD_CAP {
            return Err(RulesetReject::Grammar);
        }
        out[n] = c;
        n += 1;
    }
}

/// Scan a §9.4 descriptor string (v2 `instrument`/`ref` values):
/// printable ASCII, no escapes, ≤ [`DESCRIPTOR_CAP`].
fn scan_descriptor(
    cur: &mut Cur<'_>,
    out: &mut [u8; DESCRIPTOR_CAP],
) -> Result<usize, RulesetReject> {
    cur.eat(b'"')?;
    let mut n = 0usize;
    loop {
        if cur.i >= cur.b.len() {
            return Err(RulesetReject::Grammar);
        }
        let c = cur.b[cur.i];
        cur.i += 1;
        if c == b'"' {
            if n == 0 {
                return Err(RulesetReject::Descriptor);
            }
            return Ok(n);
        }
        if c == b'\\' || c < 0x20 || c > 0x7E {
            return Err(RulesetReject::Grammar);
        }
        if n >= DESCRIPTOR_CAP {
            return Err(RulesetReject::Grammar);
        }
        out[n] = c;
        n += 1;
    }
}

/// Scan a row `name` string: escapes/control bytes reject as rule 2;
/// non-ASCII (> 0x7E) and len ∉ `[1, 64]` reject as rule 5. Returns
/// the FNV-1a 64 of the name bytes — the name itself is never stored
/// (design §3: names live only in the artifact + worker registry).
fn scan_name(cur: &mut Cur<'_>) -> Result<u64, RulesetReject> {
    cur.eat(b'"')?;
    let mut buf = [0u8; 64];
    let mut n = 0usize;
    loop {
        if cur.i >= cur.b.len() {
            return Err(RulesetReject::Grammar);
        }
        let c = cur.b[cur.i];
        cur.i += 1;
        if c == b'"' {
            break;
        }
        if c == b'\\' || c < 0x20 {
            return Err(RulesetReject::Grammar);
        }
        if c > 0x7E {
            return Err(RulesetReject::Name);
        }
        if n >= buf.len() {
            return Err(RulesetReject::Name);
        }
        buf[n] = c;
        n += 1;
    }
    if n == 0 {
        return Err(RulesetReject::Name);
    }
    Ok(fnv1a_64(&buf[..n]))
}

/// Scan an unsigned 32-bit integer field (rule 3: decimal only — a
/// fractional part, exponent, sign, NaN/Inf token, oversized literal
/// or u32 overflow rejects).
fn scan_u32_field(cur: &mut Cur<'_>) -> Result<u32, RulesetReject> {
    cur.skip_ws();
    if cur.i >= cur.b.len() {
        return Err(RulesetReject::Grammar);
    }
    let c = cur.b[cur.i];
    if c == b'-' || c == b'+' {
        return Err(RulesetReject::Number);
    }
    if c == b'N' || c == b'n' || c == b'I' || c == b'i' {
        // NaN / Infinity tokens — rule 3 by design wording.
        return Err(RulesetReject::Number);
    }
    if !c.is_ascii_digit() {
        return Err(RulesetReject::Grammar);
    }
    let (v, end) = match scan_u64(cur.b, cur.i) {
        Some(x) => x,
        None => return Err(RulesetReject::Grammar),
    };
    // `scan_u64` wraps on overflow — cap the literal at 10 digits
    // (u32::MAX is 10) so a wrapped value can never sneak back into
    // range.
    if end - cur.i > 10 {
        return Err(RulesetReject::Number);
    }
    if end < cur.b.len() {
        let t = cur.b[end];
        if t == b'.' {
            return Err(RulesetReject::Number); // integer fields reject fractions
        }
        if t == b'e' || t == b'E' {
            return Err(RulesetReject::Number); // no exponents (8e sci-notation lesson)
        }
    }
    cur.i = end;
    if v > u32::MAX as u64 {
        return Err(RulesetReject::Number);
    }
    Ok(v as u32)
}

/// Scan a decimal money/price field into ×1e6 fixed point via the
/// canonical `core-parse` scanner (rule 3: no exponents, no NaN/Inf;
/// more than 6 fractional digits truncate per the scanner's
/// documented contract; sign and range are the caller's domain checks).
fn scan_money_field(cur: &mut Cur<'_>) -> Result<i64, RulesetReject> {
    cur.skip_ws();
    if cur.i >= cur.b.len() {
        return Err(RulesetReject::Grammar);
    }
    let c = cur.b[cur.i];
    if c == b'N' || c == b'n' || c == b'I' || c == b'i' {
        return Err(RulesetReject::Number);
    }
    if c != b'-' && !c.is_ascii_digit() {
        return Err(RulesetReject::Grammar);
    }
    let (v, end) = match scan_price_1e6(cur.b, cur.i) {
        Some(x) => x,
        None => return Err(RulesetReject::Grammar),
    };
    if end < cur.b.len() {
        let t = cur.b[end];
        if t == b'e' || t == b'E' {
            return Err(RulesetReject::Number);
        }
    }
    cur.i = end;
    Ok(v)
}

/// VM2 V4: scan a signed threshold into the ×1e9 signal domain
/// (`enter`/`exit`/`confirm` carry NATURAL units — APR fractions,
/// bps, px — where 6 decimals are too coarse for funding rates).
/// Same lexical rules as [`scan_money_field`].
fn scan_signal_field(cur: &mut Cur<'_>) -> Result<i64, RulesetReject> {
    cur.skip_ws();
    if cur.i >= cur.b.len() {
        return Err(RulesetReject::Grammar);
    }
    let c = cur.b[cur.i];
    if c == b'N' || c == b'n' || c == b'I' || c == b'i' {
        return Err(RulesetReject::Number);
    }
    if c != b'-' && !c.is_ascii_digit() {
        return Err(RulesetReject::Grammar);
    }
    let (v, end) = match scan_price_1e9(cur.b, cur.i) {
        Some(x) => x,
        None => return Err(RulesetReject::Grammar),
    };
    if end < cur.b.len() {
        let t = cur.b[end];
        if t == b'e' || t == b'E' {
            return Err(RulesetReject::Number);
        }
    }
    cur.i = end;
    Ok(v)
}

/// Scan a JSON bool.
fn scan_bool_field(cur: &mut Cur<'_>) -> Result<bool, RulesetReject> {
    cur.skip_ws();
    if cur.b.len() >= cur.i + 4 && &cur.b[cur.i..cur.i + 4] == b"true" {
        cur.i += 4;
        return Ok(true);
    }
    if cur.b.len() >= cur.i + 5 && &cur.b[cur.i..cur.i + 5] == b"false" {
        cur.i += 5;
        return Ok(false);
    }
    Err(RulesetReject::Grammar)
}

/// Sorted-slice membership (rule 6, v1 arm). Binary search.
fn universe_contains(universe: &[u32], sym: u32) -> bool {
    let mut lo = 0usize;
    let mut hi = universe.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let v = universe[mid];
        if v == sym {
            return true;
        }
        if v < sym {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    false
}

/// Map a v2 `feature` token to its [`FeatId`].
fn feat_of_token(tok: &[u8]) -> Option<FeatId> {
    Some(match tok {
        b"mid" => FeatId::Mid,
        b"bid" => FeatId::Bid,
        b"ask" => FeatId::Ask,
        b"roll_mean" => FeatId::RollMean,
        b"roll_ema" => FeatId::RollEma,
        b"roll_min" => FeatId::RollMin,
        b"roll_max" => FeatId::RollMax,
        b"roll_std" => FeatId::RollStd,
        b"apr24" => FeatId::Apr24,
        b"apr72" => FeatId::Apr72,
        b"mark_px" => FeatId::MarkPx,
        b"mark_iv" => FeatId::MarkIv,
        b"depth_imb" => FeatId::DepthImb,
        b"depth_spread_bps" => FeatId::DepthSpreadBps,
        b"depth_notional" => FeatId::DepthNearNotional,
        b"clock_to_funding" => FeatId::ClockToFunding,
        b"clock_utc_sod" => FeatId::ClockUtcSod,
        _ => return None,
    })
}

/// Parse the v1 trigger object. Returns
/// `(trigger byte, ref_sym, level_1e6, trig_keys)`.
fn parse_trigger(cur: &mut Cur<'_>) -> Result<(u8, u32, i64, u8), RulesetReject> {
    cur.eat(b'{')?;
    let mut kw = [0u8; KEYWORD_CAP];
    let mut trig_keys = 0u8;
    let mut trigger = 0u8;
    let mut ref_sym = SYMBOL_ID_NONE;
    let mut level_1e6 = 0i64;
    loop {
        let n = scan_keyword(cur, &mut kw)?;
        cur.eat(b':')?;
        if &kw[..n] == b"type" {
            if trig_keys & T_TYPE != 0 {
                return Err(RulesetReject::Grammar);
            }
            trig_keys |= T_TYPE;
            let m = scan_keyword(cur, &mut kw)?;
            trigger = match &kw[..m] {
                b"cross_deviation" => RuleRow::TRIGGER_CROSS_DEVIATION,
                b"level_breach" => RuleRow::TRIGGER_LEVEL_BREACH,
                _ => return Err(RulesetReject::Grammar),
            };
        } else if &kw[..n] == b"ref" {
            if trig_keys & T_REF != 0 {
                return Err(RulesetReject::Grammar);
            }
            trig_keys |= T_REF;
            ref_sym = scan_u32_field(cur)?;
        } else if &kw[..n] == b"level" {
            if trig_keys & T_LEVEL != 0 {
                return Err(RulesetReject::Grammar);
            }
            trig_keys |= T_LEVEL;
            let v = scan_money_field(cur)?;
            if !(0..=RULE_LEVEL_1E6_MAX).contains(&v) {
                return Err(RulesetReject::Number);
            }
            level_1e6 = v;
        } else {
            return Err(RulesetReject::Grammar); // unknown trigger key
        }
        if cur.eat_if(b',') {
            continue;
        }
        cur.eat(b'}')?;
        break;
    }
    if trig_keys & T_TYPE == 0 {
        return Err(RulesetReject::Grammar); // type required
    }
    if trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
        if trig_keys & T_REF == 0 {
            return Err(RulesetReject::Grammar); // ref required (§4.1 shape)
        }
        if trig_keys & T_LEVEL != 0 {
            return Err(RulesetReject::Grammar); // level forbidden (§4.1 shape)
        }
    } else if trig_keys & T_LEVEL == 0 {
        return Err(RulesetReject::Grammar); // level required (§4.1 shape)
    }
    Ok((trigger, ref_sym, level_1e6, trig_keys))
}

/// Rolling-bind budget tracker (rule 10's bind arm): distinct
/// (sym, window) pairs across the whole table must fit the feature
/// engine's pool, and ≤ 8 windows per sym.
struct BindBudget {
    pairs: [(u32, u16); 512],
    n: usize,
}

impl BindBudget {
    fn new() -> Self {
        Self {
            pairs: [(0, 0); 512],
            n: 0,
        }
    }

    /// Record a windowed leg; false = budget exceeded.
    fn add(&mut self, sym: u32, win: u16) -> bool {
        let mut per_sym = 0usize;
        let mut i = 0;
        while i < self.n {
            let (s, w) = self.pairs[i];
            if s == sym {
                if w == win {
                    return true; // already counted
                }
                per_sym += 1;
            }
            i += 1;
        }
        if per_sym >= strategy_bind_cap_per_sym() || self.n >= strategy_bind_pool() {
            return false;
        }
        self.pairs[self.n] = (sym, win);
        self.n += 1;
        true
    }
}

/// The feature engine's per-sym window cap (mirrored — ingress-ai
/// must not depend on strategy-vm; the pin test keeps them equal).
#[inline]
const fn strategy_bind_cap_per_sym() -> usize {
    8
}

/// The feature engine's rolling-pool size (mirrored, pin-tested).
#[inline]
const fn strategy_bind_pool() -> usize {
    256
}

/// Per-row parse + rules 2–10 admission into `out.rows[count]`.
/// Cross-row state (rule 5 uniqueness, rule 7 sums, rule 8
/// duplicates, rule 10 bind budget) is recomputed against the
/// admitted prefix — O(n²) over ≤ 256 rows at operator cadence.
#[allow(clippy::too_many_lines)] // one row = one linear rule sequence; splitting hides the §4.2 order
fn parse_and_admit_row(
    cur: &mut Cur<'_>,
    universe: &[u32],
    descriptors: &DescriptorTable,
    out: &mut RuleTableV2,
    count: u32,
    total_risk_1e6: &mut i64,
    binds: &mut BindBudget,
) -> Result<(), RulesetReject> {
    cur.eat(b'{')?;
    let mut kw = [0u8; KEYWORD_CAP];
    let mut desc_buf = [0u8; DESCRIPTOR_CAP];
    let mut keys = 0u32;
    let mut trig_keys = 0u8;

    // v1 locals.
    let mut sym_raw = 0u32;
    let mut v1_ref = SYMBOL_ID_NONE;
    let mut edge_bps = 0u32;
    let mut level_1e6 = 0i64;
    let mut trigger = 0u8;

    // Shared / v2 locals.
    let mut name_h = 0u64;
    let mut family = 4u8; // "other" default (v2 arm; v1 requires it)
    let mut side = RuleRow::SIDE_BOTH;
    let mut horizon_ms = 0u32;
    let mut max_risk_1e6 = 0i64;
    let mut inst_sym = SYMBOL_ID_NONE;
    let mut inst_caps = 0u8;
    let mut ref_sym = SYMBOL_ID_NONE;
    let mut ref_caps = 0u8;
    let mut feat_a = FeatId::Mid;
    let mut feat_b: Option<FeatId> = None;
    let mut combine: Option<CombineOp> = None;
    let mut win_a = 0u16;
    let mut win_b = 0u16;
    let mut win_c = 0u16;
    let mut cmp_le = false;
    let mut cmp_abs = false;
    let mut enter_1e9 = 0i64;
    let mut exit_1e9 = 0i64;
    let mut feat_c: Option<FeatId> = None;
    let mut confirm_1e9 = 0i64;
    let mut c_le = false;
    let mut c_abs = false;
    let mut c_pair = false;
    let mut group = GROUP_NONE;
    let mut min_hold_s = 0u32;
    let mut max_hold_s = 0u32;

    macro_rules! once {
        ($bit:expr) => {
            if keys & $bit != 0 {
                return Err(RulesetReject::Grammar);
            }
            keys |= $bit;
        };
    }

    loop {
        let n = scan_keyword(cur, &mut kw)?;
        cur.eat(b':')?;
        let key = &kw[..n];
        if key == b"name" {
            once!(K_NAME);
            name_h = scan_name(cur)?;
        } else if key == b"family" {
            once!(K_FAMILY);
            let m = scan_keyword(cur, &mut kw)?;
            family = match &kw[..m] {
                b"crypto" => 0,
                b"politics" => 1,
                b"sports" => 2,
                b"macro" => 3,
                b"other" => 4,
                _ => return Err(RulesetReject::Grammar),
            };
        } else if key == b"trigger" {
            once!(K_TRIGGER);
            let (t, r, l, tk) = parse_trigger(cur)?;
            trigger = t;
            v1_ref = r;
            level_1e6 = l;
            trig_keys = tk;
        } else if key == b"sym" {
            once!(K_SYM);
            sym_raw = scan_u32_field(cur)?;
        } else if key == b"side" {
            once!(K_SIDE);
            let m = scan_keyword(cur, &mut kw)?;
            side = match &kw[..m] {
                b"bid" => 0,
                b"ask" => 1,
                b"both" => RuleRow::SIDE_BOTH,
                _ => return Err(RulesetReject::Grammar),
            };
        } else if key == b"edge_bps" {
            once!(K_EDGE);
            edge_bps = scan_u32_field(cur)?;
            if edge_bps > RULE_EDGE_BPS_MAX {
                return Err(RulesetReject::Number);
            }
        } else if key == b"horizon_ms" {
            once!(K_HORIZON);
            horizon_ms = scan_u32_field(cur)?;
            if !(RULE_HORIZON_MS_MIN..=RULE_HORIZON_MS_MAX).contains(&horizon_ms) {
                return Err(RulesetReject::Number);
            }
        } else if key == b"max_risk_usd" {
            once!(K_RISK);
            max_risk_1e6 = scan_money_field(cur)?;
            if max_risk_1e6 <= 0 {
                return Err(RulesetReject::Number); // a non-positive cap is meaningless
            }
        } else if key == b"instrument" {
            once!(K_INSTRUMENT);
            let m = scan_descriptor(cur, &mut desc_buf)?;
            match descriptors.resolve(&desc_buf[..m]) {
                Some((s, c)) => {
                    inst_sym = s;
                    inst_caps = c;
                }
                None => return Err(RulesetReject::Descriptor),
            }
        } else if key == b"ref" {
            once!(K_REF);
            let m = scan_descriptor(cur, &mut desc_buf)?;
            match descriptors.resolve(&desc_buf[..m]) {
                Some((s, c)) => {
                    ref_sym = s;
                    ref_caps = c;
                }
                None => return Err(RulesetReject::Descriptor),
            }
        } else if key == b"feature" {
            once!(K_FEATURE);
            let m = scan_keyword(cur, &mut kw)?;
            feat_a = match feat_of_token(&kw[..m]) {
                Some(f) => f,
                None => return Err(RulesetReject::Grammar),
            };
        } else if key == b"ref_feature" {
            once!(K_REF_FEATURE);
            let m = scan_keyword(cur, &mut kw)?;
            feat_b = match feat_of_token(&kw[..m]) {
                Some(f) => Some(f),
                None => return Err(RulesetReject::Grammar),
            };
        } else if key == b"combine" {
            once!(K_COMBINE);
            let m = scan_keyword(cur, &mut kw)?;
            combine = Some(match &kw[..m] {
                b"diff" => CombineOp::Diff,
                b"diff_bps" => CombineOp::DiffBps,
                b"ratio" => CombineOp::Ratio1e9,
                _ => return Err(RulesetReject::Grammar),
            });
        } else if key == b"window_min" {
            once!(K_WINDOW);
            let v = scan_u32_field(cur)?;
            if v == 0 || v > ROLL_WINDOW_MAX_MIN as u32 {
                return Err(RulesetReject::Feature);
            }
            win_a = v as u16;
        } else if key == b"ref_window_min" {
            once!(K_REF_WINDOW);
            let v = scan_u32_field(cur)?;
            if v == 0 || v > ROLL_WINDOW_MAX_MIN as u32 {
                return Err(RulesetReject::Feature);
            }
            win_b = v as u16;
        } else if key == b"confirm_window_min" {
            once!(K_C_WINDOW);
            let v = scan_u32_field(cur)?;
            if v == 0 || v > ROLL_WINDOW_MAX_MIN as u32 {
                return Err(RulesetReject::Feature);
            }
            win_c = v as u16;
        } else if key == b"cmp" {
            once!(K_CMP);
            let m = scan_keyword(cur, &mut kw)?;
            cmp_le = match &kw[..m] {
                b"ge" => false,
                b"le" => true,
                _ => return Err(RulesetReject::Grammar),
            };
        } else if key == b"abs" {
            once!(K_ABS);
            cmp_abs = scan_bool_field(cur)?;
        } else if key == b"enter" {
            once!(K_ENTER);
            enter_1e9 = scan_signal_field(cur)?;
        } else if key == b"exit" {
            once!(K_EXIT);
            exit_1e9 = scan_signal_field(cur)?;
        } else if key == b"confirm_feature" {
            once!(K_C_FEATURE);
            let m = scan_keyword(cur, &mut kw)?;
            feat_c = match feat_of_token(&kw[..m]) {
                Some(f) => Some(f),
                None => return Err(RulesetReject::Grammar),
            };
        } else if key == b"confirm" {
            once!(K_CONFIRM);
            confirm_1e9 = scan_signal_field(cur)?;
        } else if key == b"confirm_cmp" {
            once!(K_C_CMP);
            let m = scan_keyword(cur, &mut kw)?;
            c_le = match &kw[..m] {
                b"ge" => false,
                b"le" => true,
                _ => return Err(RulesetReject::Grammar),
            };
        } else if key == b"confirm_abs" {
            once!(K_C_ABS);
            c_abs = scan_bool_field(cur)?;
        } else if key == b"confirm_pair" {
            once!(K_C_PAIR);
            c_pair = scan_bool_field(cur)?;
        } else if key == b"group" {
            once!(K_GROUP);
            let v = scan_u32_field(cur)?;
            if v > 254 {
                return Err(RulesetReject::Number);
            }
            group = v as u8;
        } else if key == b"min_hold_s" {
            once!(K_MIN_HOLD);
            min_hold_s = scan_u32_field(cur)?;
        } else if key == b"max_hold_s" {
            once!(K_MAX_HOLD);
            max_hold_s = scan_u32_field(cur)?;
        } else {
            return Err(RulesetReject::Grammar); // unknown row key
        }
        if cur.eat_if(b',') {
            continue;
        }
        cur.eat(b'}')?;
        break;
    }

    // Arm selection (rule 2): a row is v1 XOR v2 — mixing shapes
    // rejects.
    let v1 = keys & V1_ONLY != 0;
    let v2 = keys & V2_ONLY != 0;
    if v1 && v2 {
        return Err(RulesetReject::Grammar);
    }

    let row: RuleRowV2 = if !v2 {
        // ---- v1 arm: the 8g shape, byte-exact (D-6 compat) ----
        if keys != V1_REQUIRED {
            return Err(RulesetReject::Grammar);
        }
        // Rule 6: universe membership for BOTH legs; `ref != sym`
        // for cross_deviation; `ref` ABSENT for level_breach.
        if !universe_contains(universe, sym_raw) {
            return Err(RulesetReject::Symbol);
        }
        if trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
            if !universe_contains(universe, v1_ref) {
                return Err(RulesetReject::Symbol);
            }
            if v1_ref == sym_raw {
                return Err(RulesetReject::Symbol);
            }
        } else if trig_keys & T_REF != 0 {
            return Err(RulesetReject::Symbol);
        }
        // Rule 8 (v1 identity law, preserved): duplicate
        // `(sym, trigger, side, ref/level)` — edge/horizon/risk/name
        // deliberately NOT part of it. Prior v1-mapped rows are
        // recognized by their sugar shape.
        let this_combine = if trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
            CombineOp::DiffBps as u8
        } else {
            CombineOp::LhsOnly as u8
        };
        let this_level_1e9 = level_1e6 * 1_000;
        let mut j = 0usize;
        while j < count as usize {
            let r = &out.rows[j];
            if r.flags == 0
                && r.sym == sym_raw
                && r.side == side
                && r.combine == this_combine
                && r.ref_sym == if trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
                    v1_ref
                } else {
                    SYMBOL_ID_NONE
                }
                && (this_combine != CombineOp::LhsOnly as u8 || r.enter_1e9 == this_level_1e9)
            {
                return Err(RulesetReject::DuplicateRow);
            }
            j += 1;
        }
        RuleRowV2::from_v1(&RuleRow::new(
            sym_raw,
            if trigger == RuleRow::TRIGGER_CROSS_DEVIATION {
                v1_ref
            } else {
                SYMBOL_ID_NONE
            },
            edge_bps,
            horizon_ms,
            level_1e6,
            max_risk_1e6,
            name_h,
            trigger,
            side,
            family,
        ))
    } else {
        // ---- v2 arm (VM2 V4) ----
        if keys & V2_REQUIRED != V2_REQUIRED {
            return Err(RulesetReject::Grammar);
        }
        let has_ref = keys & K_REF != 0;
        // Combine law: required with `ref`, forbidden without.
        let combine = match (has_ref, combine) {
            (true, Some(c)) => c,
            (false, None) => CombineOp::LhsOnly,
            _ => return Err(RulesetReject::Grammar),
        };
        if !has_ref
            && (keys & K_REF_FEATURE != 0 || keys & K_REF_WINDOW != 0 || c_pair)
        {
            return Err(RulesetReject::Grammar);
        }
        if has_ref && ref_sym == inst_sym {
            return Err(RulesetReject::Symbol);
        }
        let feat_b = feat_b.unwrap_or(feat_a);
        // Rule 9: position ⇔ `exit` present; holds/groups require it.
        let position = keys & K_EXIT != 0;
        if !position
            && (keys & K_MIN_HOLD != 0 || keys & K_MAX_HOLD != 0 || keys & K_GROUP != 0)
        {
            return Err(RulesetReject::Position);
        }
        if position && min_hold_s > 0 && max_hold_s > 0 && max_hold_s <= min_hold_s {
            return Err(RulesetReject::Position);
        }
        // Rule 3 window law: windows iff the feature rolls.
        let win_law = |f: FeatId, w: u16| -> bool {
            if f.requires_window() {
                w > 0
            } else {
                w == 0
            }
        };
        if !win_law(feat_a, win_a) || (has_ref && !win_law(feat_b, win_b)) {
            return Err(RulesetReject::Feature);
        }
        if !has_ref && win_b != 0 {
            return Err(RulesetReject::Feature);
        }
        // Confirm shape: threshold/cmp/abs/pair/window require the
        // feature.
        if feat_c.is_none()
            && (keys & K_CONFIRM != 0
                || keys & K_C_CMP != 0
                || keys & K_C_ABS != 0
                || keys & K_C_PAIR != 0
                || keys & K_C_WINDOW != 0)
        {
            return Err(RulesetReject::Grammar);
        }
        if let Some(fc) = feat_c {
            if keys & K_CONFIRM == 0 {
                return Err(RulesetReject::Grammar);
            }
            if !win_law(fc, win_c) {
                return Err(RulesetReject::Feature);
            }
        }
        // Rule 10: channel capabilities per resolved leg.
        if feat_caps_required(feat_a) & !inst_caps != 0 {
            return Err(RulesetReject::Feature);
        }
        if has_ref && feat_caps_required(feat_b) & !ref_caps != 0 {
            return Err(RulesetReject::Feature);
        }
        if let Some(fc) = feat_c {
            if feat_caps_required(fc) & !inst_caps != 0 {
                return Err(RulesetReject::Feature);
            }
            if c_pair && feat_caps_required(fc) & !ref_caps != 0 {
                return Err(RulesetReject::Feature);
            }
        }
        // Rule 10 bind budget (rolling windows are commit-bound).
        let bind_legs: [(u32, u16, FeatId); 4] = [
            (inst_sym, win_a, feat_a),
            (if has_ref { ref_sym } else { SYMBOL_ID_NONE }, win_b, feat_b),
            (inst_sym, win_c, feat_c.unwrap_or(FeatId::Mid)),
            (
                if c_pair { ref_sym } else { SYMBOL_ID_NONE },
                win_c,
                feat_c.unwrap_or(FeatId::Mid),
            ),
        ];
        let mut l = 0;
        while l < bind_legs.len() {
            let (s, w, f) = bind_legs[l];
            if s != SYMBOL_ID_NONE && w > 0 && f.requires_window() && !binds.add(s, w) {
                return Err(RulesetReject::Feature);
            }
            l += 1;
        }
        // Rule 8 (v2 identity): duplicate
        // `(sym, ref, feat_a/b/c, windows, combine, cmp_bits, flags,
        //   group, enter)` — two rows differing ONLY in
        // horizon/holds/risk/name/exit/confirm-threshold are the
        // same rule (the v1 spirit: thresholds-of-degree are not
        // identity, but a different window or comparison IS a
        // different rule).
        let flags = if position { ROW_FLAG_POSITION } else { 0 };
        let fc_byte = feat_c.map(|f| f as u8).unwrap_or(FEAT_NONE);
        let mut cmp_bits = 0u8;
        if cmp_le {
            cmp_bits |= CMP_ENTRY_LE;
        }
        if cmp_abs {
            cmp_bits |= CMP_ENTRY_ABS;
        }
        if c_le {
            cmp_bits |= CMP_CONFIRM_LE;
        }
        if c_abs {
            cmp_bits |= CMP_CONFIRM_ABS;
        }
        if c_pair {
            cmp_bits |= CMP_CONFIRM_PAIR;
        }
        let mut j = 0usize;
        while j < count as usize {
            let r = &out.rows[j];
            if r.sym == inst_sym
                && r.ref_sym == if has_ref { ref_sym } else { SYMBOL_ID_NONE }
                && r.feat_a == feat_a as u8
                && r.feat_b == feat_b as u8
                && r.feat_c == fc_byte
                && r.win_a == win_a
                && r.win_b == win_b
                && r.win_c == win_c
                && r.combine == combine as u8
                && r.cmp_bits == cmp_bits
                && r.flags == flags
                && r.group == group
                && r.enter_1e9 == enter_1e9
            {
                return Err(RulesetReject::DuplicateRow);
            }
            j += 1;
        }
        RuleRowV2::new(
            flags,
            side,
            group,
            feat_a,
            feat_b,
            fc_byte,
            combine,
            inst_sym,
            if has_ref { ref_sym } else { SYMBOL_ID_NONE },
            win_a,
            win_b,
            win_c,
            cmp_bits,
            enter_1e9,
            exit_1e9,
            confirm_1e9,
            min_hold_s,
            horizon_ms,
            0,
            max_risk_1e6,
            name_h,
            max_hold_s,
            family,
        )
    };

    // Rule 5 (close): name_h unique within the file.
    let mut j = 0usize;
    while j < count as usize {
        if out.rows[j].name_h == name_h {
            return Err(RulesetReject::Name);
        }
        j += 1;
    }

    // Rule 7: tighten-only caps. Two-leg position rows spend their
    // cap on BOTH legs — the ref sym's budget and the table total
    // count it too. Sums cannot overflow: 256 rows × 2 legs × $10k
    // ×1e6 ≪ i64::MAX.
    if row.max_risk_1e6 > RULE_ROW_MAX_RISK_1E6 {
        return Err(RulesetReject::Caps);
    }
    let legs: i64 = if row.flags & ROW_FLAG_POSITION != 0 && row.ref_sym != SYMBOL_ID_NONE {
        2
    } else {
        1
    };
    let mut sym_sum = row.max_risk_1e6;
    let mut ref_sum = if legs == 2 { row.max_risk_1e6 } else { 0 };
    let mut j = 0usize;
    while j < count as usize {
        let r = &out.rows[j];
        let r_two = r.flags & ROW_FLAG_POSITION != 0 && r.ref_sym != SYMBOL_ID_NONE;
        if r.sym == row.sym || (r_two && r.ref_sym == row.sym) {
            sym_sum += r.max_risk_1e6;
        }
        if legs == 2 && (r.sym == row.ref_sym || (r_two && r.ref_sym == row.ref_sym)) {
            ref_sum += r.max_risk_1e6;
        }
        j += 1;
    }
    if sym_sum > RULE_SYM_MAX_RISK_1E6 || ref_sum > RULE_SYM_MAX_RISK_1E6 {
        return Err(RulesetReject::Caps);
    }
    if *total_risk_1e6 + row.max_risk_1e6 * legs > RULE_TABLE_MAX_RISK_1E6 {
        return Err(RulesetReject::Caps);
    }
    *total_risk_1e6 += row.max_risk_1e6 * legs;

    out.rows[count as usize] = row;
    Ok(())
}

/// §4.2 validator — the full rule 1–10 battery over raw artifact
/// bytes into a caller-owned scratch table. 0 B/op after construction
/// (alloc gate 34); the `fs::read` that produces `bytes` is the
/// documented operator-cadence copy #0 and sits OUTSIDE this seam.
///
/// VM2 V4: rows may be v1-shaped (raw syms + trigger sugar — the D-6
/// one-release compat arm, validated against `universe`) or
/// v2-shaped (descriptor strings resolved through `descriptors`,
/// rules 9–10). Output rows are ALWAYS v2.
///
/// On success `out.rows[..out.len]` holds the validated rows and
/// `out.hash128` the artifact identity; `out.epoch` is untouched (the
/// side path stamps it — it is side-path state, not artifact state).
/// On ANY failure `out.len` is 0 — a rejected ruleset never leaves a
/// partially staged table (discard-on-reject contract, §11).
pub fn validate_ruleset(
    bytes: &[u8],
    expect_hash128: &[u8; HASH128_LEN],
    universe: &[u32],
    descriptors: &DescriptorTable,
    out: &mut RuleTableV2,
) -> Result<(), RulesetReject> {
    // Rule 1 FIRST — identity binding before any parse.
    let digest = sha256(bytes);
    let mut k = 0usize;
    while k < HASH128_LEN {
        if digest[k] != expect_hash128[k] {
            return Err(RulesetReject::HashMismatch);
        }
        k += 1;
    }

    // Discard-on-reject: len drops to 0 now; only a fully validated
    // scan restores it.
    out.len = 0;
    out.hash128 = [0u8; HASH128_LEN];

    let mut cur = Cur::new(bytes);
    let mut kw = [0u8; KEYWORD_CAP];
    cur.eat(b'{')?;
    let n = scan_keyword(&mut cur, &mut kw)?;
    if &kw[..n] != b"rows" {
        return Err(RulesetReject::Grammar);
    }
    cur.eat(b':')?;
    cur.eat(b'[')?;

    let mut count = 0u32;
    let mut total_risk_1e6 = 0i64;
    let mut binds = BindBudget::new();
    if !cur.peek_is(b']') {
        loop {
            if count as usize >= RULE_TABLE_ROWS {
                return Err(RulesetReject::RowCount); // rule 4 upper — a 257th row opens
            }
            parse_and_admit_row(
                &mut cur,
                universe,
                descriptors,
                out,
                count,
                &mut total_risk_1e6,
                &mut binds,
            )?;
            count += 1;
            if cur.eat_if(b',') {
                continue;
            }
            break;
        }
    }
    cur.eat(b']')?;
    cur.eat(b'}')?;
    if !cur.at_end() {
        return Err(RulesetReject::Grammar); // trailing non-whitespace (rule 2)
    }
    if count == 0 {
        return Err(RulesetReject::RowCount);
    }

    out.len = count;
    out.hash128 = *expect_hash128;
    Ok(())
}

// ---------------------------------------------------------------
// Side-path state machine
// ---------------------------------------------------------------

/// Stage/commit side-path state. Owned by the ingress-ai thread (the
/// seam closure captures it); counters live in the shared
/// [`AiIngressStatus`] slot so the cli mirrors the whole family from
/// one place. Single-writer: only the ingress thread touches this.
///
/// The scratch [`RuleTable`] (16 KiB + 64) is heap-allocated ONCE at
/// construction (boot) and reused for every Stage — the steady-state
/// validator path is 0 B/op (gate 34).
pub struct RulesetSidePath {
    dir: PathBuf,
    status: Arc<AiIngressStatus>,
    /// Sorted boot-universe SymbolId snapshot (§4.3) — universe
    /// membership is a boot-time fact; a symbol that later loses its
    /// feed still validates (the row just never triggers).
    universe: Arc<[u32]>,
    /// VM2 V4 (D-6): descriptor → (sym, caps) resolution for the v2
    /// grammar arm — built by the bin from the allocation truth (or
    /// from run manifests in the offline harness).
    descriptors: Arc<DescriptorTable>,
    /// Producer half of the §6 table-handoff ring (D1a). One
    /// `try_push` per validated Stage — documented copy #1; the
    /// engine owns the consumer half (parked at boot until item 7
    /// wires the pre-AI-drain pop).
    producer: Producer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    scratch: Box<RuleTableV2>,
    /// Monotonic successful-stage counter, stamped into
    /// `scratch.epoch` (diagnostics, §3). Advanced only when the
    /// ring push lands, so consumer-visible epochs are gapless.
    epoch: u32,
    staged: Option<[u8; HASH128_LEN]>,
    committed: Option<[u8; HASH128_LEN]>,
}

impl RulesetSidePath {
    /// New side-path rooted at `dir` (`AI_RULESET_DIR`, tilde already
    /// expanded by config). The directory is not required to exist at
    /// boot — a Stage against a missing dir is just a rejected stage.
    /// `universe` MUST be sorted ascending (binary-searched per row).
    /// `producer` is the push half of the §6 table-handoff ring; the
    /// bin parks the consumer half until item 7 wires the engine
    /// drain.
    pub fn new(
        dir: PathBuf,
        status: Arc<AiIngressStatus>,
        universe: Arc<[u32]>,
        descriptors: Arc<DescriptorTable>,
        producer: Producer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    ) -> Self {
        #[cfg(debug_assertions)]
        {
            let mut i = 1usize;
            while i < universe.len() {
                debug_assert!(
                    universe[i - 1] < universe[i],
                    "universe snapshot must be sorted strict-ascending"
                );
                i += 1;
            }
        }
        Self {
            dir,
            status,
            universe,
            descriptors,
            producer,
            // Documented boot-time allocation: the reusable scratch.
            scratch: Box::new(RuleTableV2::EMPTY),
            epoch: 0,
            staged: None,
            committed: None,
        }
    }

    /// Currently staged hash128, if any (test/diagnostic surface).
    #[inline]
    pub fn staged(&self) -> Option<[u8; HASH128_LEN]> {
        self.staged
    }

    /// Currently committed hash128, if any (test/diagnostic surface).
    #[inline]
    pub fn committed(&self) -> Option<[u8; HASH128_LEN]> {
        self.committed
    }

    /// The scratch table — meaningful (`len > 0`, epoch stamped) only
    /// after a successful [`Self::staged`] Stage. Test/diagnostic
    /// surface ONLY: the ring push at stage time (copy #1, §6) is the
    /// durable handoff; this is the parked source copy. A later
    /// REJECTED Stage — validator failure OR §5 push-full — clears it
    /// (discard-on-reject wipes `len`) even though `staged()` keeps
    /// the prior hash; the parked scratch is not a state guarantee.
    #[inline]
    pub fn staged_table(&self) -> &RuleTableV2 {
        &self.scratch
    }

    /// Reassemble the wire hash128 from the `px`+`qty` halves —
    /// delegates to [`AiCmd::ruleset_hash128`], THE shared §6 helper
    /// (8g item 5 moved it to `core-types` so the `strategy-vm`
    /// Commit flip reassembles through the same code path; a local
    /// copy here would let the two state machines drift).
    #[inline]
    fn cmd_hash128(cmd: &AiCmd) -> [u8; HASH128_LEN] {
        cmd.ruleset_hash128()
    }

    /// `<hash128-hex>.json` as a fixed stack buffer (37 ASCII bytes).
    fn file_name(hash128: &[u8; HASH128_LEN]) -> [u8; 2 * HASH128_LEN + SUFFIX.len()] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; 2 * HASH128_LEN + SUFFIX.len()];
        let mut i = 0;
        while i < HASH128_LEN {
            name[2 * i] = HEX[(hash128[i] >> 4) as usize];
            name[2 * i + 1] = HEX[(hash128[i] & 0x0f) as usize];
            i += 1;
        }
        name[2 * HASH128_LEN..].copy_from_slice(SUFFIX);
        name
    }

    /// Seam entry point (§4.4 step 8). Non-ruleset kinds are never
    /// routed here by the listener; they no-op defensively.
    pub fn on_cmd(&mut self, cmd: &AiCmd) {
        match cmd.kind() {
            Some(AiCmdKind::RulesetStage) => self.stage(Self::cmd_hash128(cmd)),
            Some(AiCmdKind::RulesetCommit) => self.commit(Self::cmd_hash128(cmd)),
            _ => {}
        }
    }

    fn stage(&mut self, hash128: [u8; HASH128_LEN]) {
        let name = Self::file_name(&hash128);
        // SAFETY: `name` is built exclusively from ASCII hex digits and
        // the ASCII ".json" suffix — always valid UTF-8.
        let name_str = unsafe { core::str::from_utf8_unchecked(&name) };
        // Documented copy #0: `join` + `fs::read` allocate — operator
        // cadence only (module docs).
        let file = self.dir.join(name_str);
        match std::fs::read(&file) {
            Ok(bytes) => {
                match validate_ruleset(
                    &bytes,
                    &hash128,
                    &self.universe,
                    &self.descriptors,
                    &mut self.scratch,
                ) {
                    Ok(()) => self.push_staged(hash128),
                    Err(_) => self.status.inc_ruleset_rejected(),
                }
            }
            Err(_) => self.status.inc_ruleset_rejected(),
        }
    }

    /// §5 stage handoff (8g item 4): stamp the candidate epoch and
    /// `try_push` the validated scratch into the §6 table ring —
    /// **documented copy #1** (32 KiB + 64 by value, once per Stage,
    /// operator cadence; moves bytes, never the heap — gate 35).
    ///
    /// Push ok ⇒ staged; a new Stage supersedes any previous Commit
    /// (the worker registry mirrors this — `state.py`), and a restage
    /// is simply a SECOND push: the engine's later pop overwrites its
    /// staged buffer (§6 engine-side supersede mirror).
    ///
    /// Push-full (2 undrained stages — impossible at operator cadence
    /// against a µs-drain engine loop, counted honestly anyway) ⇒
    /// REJECT: `ruleset_rejected` + the dedicated `table_push_fail`
    /// counter, staged/committed UNCHANGED (§5 — only a successful
    /// Stage supersedes a Commit), scratch discarded (the
    /// discard-on-reject contract; the never-staged table must not
    /// linger in the diagnostic surface).
    fn push_staged(&mut self, hash128: [u8; HASH128_LEN]) {
        // Candidate epoch: committed to `self.epoch` only if the push
        // lands, so consumer-visible epochs stay gapless-monotonic
        // (§3 "successful-stage counter"; wraps are harmless).
        let epoch = self.epoch.wrapping_add(1);
        self.scratch.epoch = epoch;
        // Documented copy #1 (§6): scratch → ring slot, by value.
        match self.producer.try_push(*self.scratch) {
            Ok(()) => {
                self.epoch = epoch;
                self.staged = Some(hash128);
                self.committed = None;
                self.status.inc_ruleset_staged();
            }
            Err(_) => {
                self.scratch.len = 0;
                self.status.inc_ruleset_rejected();
                self.status.inc_table_push_fail();
            }
        }
    }

    fn commit(&mut self, hash128: [u8; HASH128_LEN]) {
        if self.staged == Some(hash128) {
            // Observable through `engine_ai_ruleset_committed_total`;
            // the engine-side flip keys off the in-stream Commit
            // AiCmd (§6), not off this flag.
            self.committed = Some(hash128);
            self.status.inc_ruleset_committed();
        } else {
            self.status.inc_ruleset_rejected();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{VenueId, AI_SIDE_NONE, STRATEGY_SLOT_VM, SYMBOL_ID_NONE};

    /// Sorted test universe: 7 (reference leg), 42 (action leg), plus
    /// spares for cap/duplicate fixtures.
    const UNIVERSE: &[u32] = &[7, 42, 99, 100, 101, 102];

    fn arc_universe() -> Arc<[u32]> {
        Arc::from(UNIVERSE.to_vec())
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cw-ai-ruleset-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp ruleset dir");
        dir
    }

    fn ruleset_cmd(kind: AiCmdKind, hash128: [u8; HASH128_LEN]) -> AiCmd {
        let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            11,
            1,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            kind,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    fn install(dir: &std::path::Path, bytes: &[u8]) -> [u8; HASH128_LEN] {
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let name = RulesetSidePath::file_name(&h);
        let path = dir.join(core::str::from_utf8(&name).expect("ascii"));
        std::fs::write(path, bytes).expect("write ruleset artifact");
        h
    }

    /// Minimal VALID one-row ruleset (8f `{"rows":[]}` fixtures are
    /// now §4.2-rule-4 rejects), distinct per `name`.
    fn valid_json(name: &str) -> String {
        format!(
            r#"{{"rows":[{{"name":"{name}","family":"crypto","trigger":{{"type":"cross_deviation","ref":7}},"sym":42,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0}}]}}"#
        )
    }

    /// Side path + the consumer half of its table ring (kept so tests
    /// can pop what Stage pushed; dropping it is also legal — pushes
    /// only see head/tail).
    fn side_path(
        dir: &std::path::Path,
        status: &Arc<AiIngressStatus>,
    ) -> (
        RulesetSidePath,
        core_ring::Consumer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    ) {
        let (prod, cons) = core_ring::Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        (
            RulesetSidePath::new(
                dir.to_path_buf(),
                Arc::clone(status),
                arc_universe(),
                Arc::new(DescriptorTable::empty()),
                prod,
            ),
            cons,
        )
    }

    /// Raw byte view for the byte-identical handoff assertions —
    /// well-defined because `RuleTable` is `#[repr(C)]` POD with all
    /// padding explicitly declared and zeroed (§3, G1 pad amendment).
    fn table_bytes(t: &RuleTableV2) -> &[u8] {
        // SAFETY: `t` is a fully initialized `#[repr(C)]` POD with no
        // uninitialized (implicit) padding bytes; the slice borrows
        // `t` for its own lifetime and never outlives it.
        unsafe {
            core::slice::from_raw_parts(
                (t as *const RuleTableV2).cast::<u8>(),
                core::mem::size_of::<RuleTableV2>(),
            )
        }
    }

    /// Direct validator harness: hash always matches `bytes`.
    fn check(bytes: &[u8]) -> Result<(), RulesetReject> {
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        validate_ruleset(bytes, &h, UNIVERSE, &DescriptorTable::empty(), &mut out)
    }

    /// One-row JSON with substitutable fragments, for reject fixtures.
    fn row_json(
        name: &str,
        trigger: &str,
        sym: &str,
        side: &str,
        edge: &str,
        horizon: &str,
        risk: &str,
    ) -> String {
        format!(
            r#"{{"name":"{name}","family":"politics","trigger":{trigger},"sym":{sym},"side":"{side}","edge_bps":{edge},"horizon_ms":{horizon},"max_risk_usd":{risk}}}"#
        )
    }

    fn wrap_rows(rows: &[String]) -> String {
        let mut s = String::from(r#"{"rows":["#);
        let mut first = true;
        for r in rows {
            if !first {
                s.push(',');
            }
            s.push_str(r);
            first = false;
        }
        s.push_str("]}");
        s
    }

    // -----------------------------------------------------------
    // Side-path state machine (8f tests, fixtures upgraded to valid
    // rulesets; assertions extended to the staged table)
    // -----------------------------------------------------------

    #[test]
    fn stage_then_commit_happy_path() {
        let dir = temp_dir("happy");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        let h = install(&dir, valid_json("happy-row").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 1);
        assert_eq!(status.ruleset_rejected(), 0);
        assert_eq!(side.staged(), Some(h));
        assert_eq!(side.committed(), None);

        // 8g: the validated table is parked in the scratch.
        let t = side.staged_table();
        assert_eq!(t.len, 1);
        assert_eq!(t.epoch, 1);
        assert_eq!(t.hash128, h);
        assert_eq!(t.rows[0].sym, 42);
        assert_eq!(t.rows[0].ref_sym, 7);
        assert_eq!(t.rows[0].edge_bps, 80);
        assert_eq!(t.rows[0].horizon_ms, 1_500);
        assert_eq!(t.rows[0].max_risk_1e6, 50_000_000);
        assert_eq!(t.rows[0].name_h, fnv1a_64(b"happy-row"));
        // v1 sugar shape (VM2 V4): cross_deviation → |DiffBps| ≥ edge.
        assert_eq!(t.rows[0].combine, CombineOp::DiffBps as u8);
        assert_eq!(t.rows[0].cmp_bits, CMP_ENTRY_ABS);
        assert_eq!(t.rows[0].enter_1e9, 80_000_000_000);
        assert_eq!(t.rows[0].flags, 0, "refire mode");
        assert_eq!(t.rows[0].side, 0);
        assert_eq!(t.rows[0].family, 0);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, h));
        assert_eq!(status.ruleset_committed(), 1);
        assert_eq!(side.committed(), Some(h));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_missing_file_is_rejected() {
        let dir = temp_dir("missing");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, [0x11; HASH128_LEN]));
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.staged(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_hash_mismatch_is_rejected() {
        let dir = temp_dir("mismatch");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        // File exists under the claimed name but its bytes hash
        // differently — a tampered/mis-installed artifact (§4.2
        // rule 1, now inside the validator).
        let h = install(&dir, valid_json("tamper-target").as_bytes());
        let name = RulesetSidePath::file_name(&h);
        let path = dir.join(core::str::from_utf8(&name).expect("ascii"));
        std::fs::write(path, b"tampered").expect("overwrite artifact");

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.staged(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_invalid_content_is_rejected() {
        // Hash MATCHES the installed bytes, but the content fails the
        // §4.2 battery — the 8f stub would have staged this; 8g must
        // reject. `{"rows":[]}` is the canonical flipped fixture
        // (rule 4).
        let dir = temp_dir("invalid");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        let h = install(&dir, br#"{"rows":[]}"#);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.staged(), None);
        assert_eq!(side.staged_table().len, 0, "no partial stage");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_unstaged_or_wrong_hash_is_rejected() {
        let dir = temp_dir("commit-reject");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);

        // Nothing staged at all.
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, [0x22; HASH128_LEN]));
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(side.committed(), None);

        // Staged, but the commit names a different hash.
        let h = install(&dir, valid_json("commit-wrong").as_bytes());
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 1);
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, [0x33; HASH128_LEN]));
        assert_eq!(status.ruleset_rejected(), 2);
        assert_eq!(side.committed(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restage_supersedes_commit() {
        let dir = temp_dir("restage");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, mut cons) = side_path(&dir, &status);
        let h1 = install(&dir, valid_json("restage-one").as_bytes());
        let h2 = install(&dir, valid_json("restage-two").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, h1));
        assert_eq!(side.committed(), Some(h1));
        assert_eq!(side.staged_table().epoch, 1);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        assert_eq!(side.staged(), Some(h2));
        assert_eq!(side.committed(), None, "a new Stage supersedes the Commit");
        assert_eq!(status.ruleset_staged(), 2);
        assert_eq!(status.ruleset_committed(), 1);
        assert_eq!(status.ruleset_rejected(), 0);
        assert_eq!(side.staged_table().epoch, 2, "epoch is monotonic");
        assert_eq!(side.staged_table().hash128, h2);
        assert_eq!(side.staged_table().rows[0].name_h, fnv1a_64(b"restage-two"));

        // §5/§6: the restage is a SECOND push — the engine-side
        // supersede works because a later pop overwrites the staged
        // buffer. FIFO order with gapless epochs at the consumer.
        let t1 = cons.try_pop().expect("first Stage pushed a table");
        assert_eq!(t1.hash128, h1);
        assert_eq!(t1.epoch, 1);
        assert_eq!(t1.rows[0].name_h, fnv1a_64(b"restage-one"));
        let t2 = cons.try_pop().expect("restage pushed a second table");
        assert_eq!(t2.hash128, h2);
        assert_eq!(t2.epoch, 2);
        assert_eq!(t2.rows[0].name_h, fnv1a_64(b"restage-two"));
        assert!(cons.try_pop().is_none(), "exactly one push per Stage");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_restage_keeps_prior_staged_state() {
        // §5: "any fail ⇒ inc rejected (staged/committed unchanged)".
        let dir = temp_dir("keep-staged");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        let h1 = install(&dir, valid_json("keeper").as_bytes());
        let h2 = install(&dir, br#"{"rows":[]}"#);

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        assert_eq!(status.ruleset_staged(), 1);
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(
            side.staged(),
            Some(h1),
            "staged state survives a rejected restage"
        );
        // The scratch itself is discard-on-reject (len 0) — pinned
        // deliberately: the ring push copies the table out at stage
        // time (copy #1), so the parked scratch is a diagnostic, not
        // a state guarantee (see `staged_table` docs).
        assert_eq!(side.staged_table().len, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_pushes_table_byte_identical_at_consumer() {
        // §6 copy #1 moves bytes, not meaning: the popped slot is
        // byte-for-byte the validated (epoch-stamped) table.
        let dir = temp_dir("push-bytes");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, mut cons) = side_path(&dir, &status);
        let h = install(&dir, valid_json("push-bytes-row").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h));
        assert_eq!(status.ruleset_staged(), 1);

        let popped = cons.try_pop().expect("Stage must push one table");
        assert_eq!(popped.len, 1);
        assert_eq!(popped.epoch, 1);
        assert_eq!(popped.hash128, h);
        assert_eq!(popped.rows[0].name_h, fnv1a_64(b"push-bytes-row"));
        // The parked scratch is the push's source copy — the full
        // 16 KiB + 64 must match, padding included.
        assert_eq!(table_bytes(&popped), table_bytes(side.staged_table()));
        assert!(cons.try_pop().is_none(), "exactly one push per Stage");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_full_rejects_stage_and_keeps_state() {
        // §5: push-full (2 undrained stages) ⇒ REJECT — staged and
        // committed unchanged, `ruleset_rejected` AND the dedicated
        // `table_push_fail` counter increment, scratch discarded, the
        // undrained ring contents untouched.
        let dir = temp_dir("push-full");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, mut cons) = side_path(&dir, &status);
        let h1 = install(&dir, valid_json("full-one").as_bytes());
        let h2 = install(&dir, valid_json("full-two").as_bytes());
        let h3 = install(&dir, valid_json("full-three").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        assert_eq!(status.ruleset_staged(), 2);
        assert_eq!(status.table_push_fail(), 0);

        // Ring capacity RULE_TABLE_RING_SLOTS = 2, nothing drained:
        // the third Stage validates fine, then rejects at the push.
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h3));
        assert_eq!(status.ruleset_staged(), 2, "no stage counted on push-full");
        assert_eq!(status.ruleset_rejected(), 1);
        assert_eq!(status.table_push_fail(), 1);
        assert_eq!(side.staged(), Some(h2), "staged unchanged (§5)");
        assert_eq!(side.committed(), None, "committed unchanged (§5)");
        assert_eq!(side.staged_table().len, 0, "discard-on-reject");

        // The two undrained stages are intact — FIFO h1 then h2, and
        // the rejected stage pushed nothing.
        assert_eq!(cons.try_pop().expect("first stage").hash128, h1);
        assert_eq!(cons.try_pop().expect("second stage").hash128, h2);
        assert!(cons.try_pop().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_full_reject_does_not_supersede_commit() {
        // §5 "staged/committed unchanged" with a live Commit: only a
        // SUCCESSFUL Stage supersedes it — a push-full reject must
        // leave the committed state standing.
        let dir = temp_dir("push-full-commit");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        let h1 = install(&dir, valid_json("fc-one").as_bytes());
        let h2 = install(&dir, valid_json("fc-two").as_bytes());
        let h3 = install(&dir, valid_json("fc-three").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetCommit, h2));
        assert_eq!(side.committed(), Some(h2));

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h3));
        assert_eq!(status.table_push_fail(), 1);
        assert_eq!(side.staged(), Some(h2));
        assert_eq!(side.committed(), Some(h2), "reject does not supersede");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn epoch_is_gapless_monotonic_across_push_full_rejects() {
        // The candidate epoch commits only on a successful push — a
        // push-full reject burns nothing, so consumer-visible epochs
        // run 1, 2, 3, … with no gap.
        let dir = temp_dir("epoch-mono");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, mut cons) = side_path(&dir, &status);
        let h1 = install(&dir, valid_json("epoch-one").as_bytes());
        let h2 = install(&dir, valid_json("epoch-two").as_bytes());
        let h3 = install(&dir, valid_json("epoch-three").as_bytes());

        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h1));
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h2));
        // Full — rejected; the candidate epoch 3 is NOT consumed.
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h3));
        assert_eq!(status.table_push_fail(), 1);

        assert_eq!(cons.try_pop().expect("epoch 1").epoch, 1);
        assert_eq!(cons.try_pop().expect("epoch 2").epoch, 2);

        // Drained — the retried Stage lands with the gapless next
        // epoch.
        side.on_cmd(&ruleset_cmd(AiCmdKind::RulesetStage, h3));
        assert_eq!(status.ruleset_staged(), 3);
        let t = cons.try_pop().expect("epoch 3");
        assert_eq!(t.epoch, 3);
        assert_eq!(t.hash128, h3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_ruleset_kind_is_a_no_op() {
        let dir = temp_dir("noop");
        let status = Arc::new(AiIngressStatus::new());
        let (mut side, _cons) = side_path(&dir, &status);
        let cmd = AiCmd::new(
            11,
            1,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            0xFF,
            AI_SIDE_NONE,
            0,
            0,
        );
        side.on_cmd(&cmd);
        assert_eq!(status.ruleset_staged(), 0);
        assert_eq!(status.ruleset_committed(), 0);
        assert_eq!(status.ruleset_rejected(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_name_is_hash128_hex_json() {
        let mut h = [0u8; HASH128_LEN];
        h[0] = 0x10;
        h[1] = 0x32;
        h[15] = 0x01;
        let name = RulesetSidePath::file_name(&h);
        assert_eq!(
            core::str::from_utf8(&name).expect("ascii"),
            "10320000000000000000000000000001.json"
        );
    }

    // -----------------------------------------------------------
    // Validator — one test per §4.2 rule
    // -----------------------------------------------------------

    #[test]
    fn rule1_hash_prefix_must_match() {
        let bytes = valid_json("rule-one").into_bytes();
        let mut wrong = [0u8; HASH128_LEN];
        wrong[0] = 0xEE;
        let mut out = Box::new(RuleTableV2::EMPTY);
        assert_eq!(
            validate_ruleset(&bytes, &wrong, UNIVERSE, &DescriptorTable::empty(), &mut out),
            Err(RulesetReject::HashMismatch)
        );
        assert_eq!(out.len, 0);
        // Same bytes, right hash: passes (control).
        assert_eq!(check(&bytes), Ok(()));
    }

    #[test]
    fn rule2_grammar_strictness() {
        // Unknown row key.
        let r = row_json(
            "g1",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        )
        .replace(r#""edge_bps":80"#, r#""edge_bps":80,"bogus":1"#);
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // Duplicate row key.
        let r = row_json(
            "g2",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        )
        .replace(r#""sym":42"#, r#""sym":42,"sym":42"#);
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // Missing row key (drop horizon_ms).
        let r = row_json(
            "g3",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        )
        .replace(r#","horizon_ms":1500"#, "");
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // Trailing non-whitespace bytes.
        let mut b = valid_json("g4").into_bytes();
        b.push(b'x');
        assert_eq!(check(&b), Err(RulesetReject::Grammar));
        // Trailing whitespace is fine (documented interpretation).
        let mut b = valid_json("g5").into_bytes();
        b.extend_from_slice(b" \n");
        assert_eq!(check(&b), Ok(()));
        // Second top-level key.
        let b = valid_json("g6").replace("]}", r#"],"extra":1}"#);
        assert_eq!(check(b.as_bytes()), Err(RulesetReject::Grammar));
        // Unknown family / side / trigger type values.
        let r = row_json(
            "g7",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        )
        .replace(r#""family":"politics""#, r#""family":"memes""#);
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        let r = row_json(
            "g8",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "mid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        let r = row_json(
            "g9",
            r#"{"type":"sma_cross","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // level key on cross_deviation (§4.1 shape).
        let r = row_json(
            "g10",
            r#"{"type":"cross_deviation","ref":7,"level":0.5}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // ref missing on cross_deviation (§4.1 shape).
        let r = row_json(
            "g11",
            r#"{"type":"cross_deviation"}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // level missing on level_breach (§4.1 shape).
        let r = row_json(
            "g12",
            r#"{"type":"level_breach"}"#,
            "42",
            "ask",
            "0",
            "60000",
            "25.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
        // Escape in a string (no escapes in the grammar).
        let r = row_json(
            r"g\1",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Grammar)
        );
    }

    #[test]
    fn rule3_number_lexicals_and_ranges() {
        let mk = |edge: &str, horizon: &str, risk: &str, level: Option<&str>| {
            let trig = match level {
                Some(l) => format!(r#"{{"type":"level_breach","level":{l}}}"#),
                None => String::from(r#"{"type":"cross_deviation","ref":7}"#),
            };
            wrap_rows(&[row_json("n1", &trig, "42", "bid", edge, horizon, risk)])
        };
        // Exponents (Deribit sci-notation lesson).
        assert_eq!(
            check(mk("1e3", "1500", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "1500", "5e1", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        // Fractional part in an integer field.
        assert_eq!(
            check(mk("80.5", "1500", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        // Negative integer field.
        assert_eq!(
            check(mk("-80", "1500", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        // NaN / Infinity tokens.
        assert_eq!(
            check(mk("80", "1500", "NaN", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "1500", "Infinity", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        // Ranges: edge_bps ≤ 10_000; horizon ∈ [10, 86_400_000];
        // level ∈ [0, 1e6]; risk > 0.
        assert_eq!(
            check(mk("10001", "1500", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "9", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "86400001", "50.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("0", "60000", "25.0", Some("1.5")).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("0", "60000", "25.0", Some("-0.5")).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "1500", "0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        assert_eq!(
            check(mk("80", "1500", "-1.0", None).as_bytes()),
            Err(RulesetReject::Number)
        );
        // Boundary values pass (control).
        assert_eq!(check(mk("10000", "10", "50.0", None).as_bytes()), Ok(()));
        assert_eq!(
            check(mk("0", "86400000", "25.0", Some("1.0")).as_bytes()),
            Ok(())
        );
    }

    #[test]
    fn rule4_row_count_bounds() {
        // Lower bound: the flipped 8f fixture.
        assert_eq!(check(br#"{"rows":[]}"#), Err(RulesetReject::RowCount));
        // Upper bound: 257 rows (level_breach on one sym, distinct
        // levels; risk tiny so caps never fire first).
        let mut rows = Vec::new();
        for i in 0..257u32 {
            let trig = format!(r#"{{"type":"level_breach","level":0.{:06}}}"#, i + 1);
            rows.push(row_json(
                &format!("r{i}"),
                &trig,
                "42",
                "ask",
                "0",
                "60000",
                "0.01",
            ));
        }
        assert_eq!(
            check(wrap_rows(&rows).as_bytes()),
            Err(RulesetReject::RowCount)
        );
        // 256 rows pass (control — also the gate-34 shape).
        rows.truncate(256);
        assert_eq!(check(wrap_rows(&rows).as_bytes()), Ok(()));
    }

    #[test]
    fn rule5_name_constraints() {
        let mk = |name: &str| {
            wrap_rows(&[row_json(
                name,
                r#"{"type":"cross_deviation","ref":7}"#,
                "42",
                "bid",
                "80",
                "1500",
                "50.0",
            )])
        };
        // Empty.
        assert_eq!(check(mk("").as_bytes()), Err(RulesetReject::Name));
        // 65 bytes.
        let long = "a".repeat(65);
        assert_eq!(check(mk(&long).as_bytes()), Err(RulesetReject::Name));
        // 64 bytes passes (control).
        let max = "a".repeat(64);
        assert_eq!(check(mk(&max).as_bytes()), Ok(()));
        // Non-ASCII.
        assert_eq!(check(mk("café").as_bytes()), Err(RulesetReject::Name));
        // Duplicate name across rows (distinct levels so rule 8
        // cannot fire first).
        let r1 = row_json(
            "same-name",
            r#"{"type":"level_breach","level":0.01}"#,
            "42",
            "ask",
            "0",
            "60000",
            "10.0",
        );
        let r2 = row_json(
            "same-name",
            r#"{"type":"level_breach","level":0.02}"#,
            "42",
            "ask",
            "0",
            "60000",
            "10.0",
        );
        assert_eq!(
            check(wrap_rows(&[r1, r2]).as_bytes()),
            Err(RulesetReject::Name)
        );
    }

    #[test]
    fn rule6_symbol_legs() {
        // sym outside the universe.
        let r = row_json(
            "s1",
            r#"{"type":"cross_deviation","ref":7}"#,
            "43",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Symbol)
        );
        // ref outside the universe.
        let r = row_json(
            "s2",
            r#"{"type":"cross_deviation","ref":8}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Symbol)
        );
        // ref == sym.
        let r = row_json(
            "s3",
            r#"{"type":"cross_deviation","ref":42}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Symbol)
        );
        // ref present on level_breach (§4.2 rule 6, design-literal).
        let r = row_json(
            "s4",
            r#"{"type":"level_breach","level":0.012,"ref":7}"#,
            "42",
            "ask",
            "0",
            "60000",
            "25.0",
        );
        assert_eq!(
            check(wrap_rows(&[r]).as_bytes()),
            Err(RulesetReject::Symbol)
        );
        // Cross-venue legs are legal — D2 as amended (both legs = any
        // universe member; 7 and 42 stand in for different venues).
        let r = row_json(
            "s5",
            r#"{"type":"cross_deviation","ref":42}"#,
            "7",
            "both",
            "80",
            "1500",
            "50.0",
        );
        assert_eq!(check(wrap_rows(&[r]).as_bytes()), Ok(()));
        // Fail-closed: an empty universe rejects every row-bearing
        // ruleset. (No boot wires one since item 4 threads the real
        // discovery snapshot through `spawn_ai`; the property is kept
        // pinned so any future mis-wired boot stays closed.)
        let bytes = valid_json("s6").into_bytes();
        let digest = sha256(&bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        assert_eq!(
            validate_ruleset(&bytes, &h, &[], &DescriptorTable::empty(), &mut out),
            Err(RulesetReject::Symbol)
        );
    }

    #[test]
    fn rule7_caps_tighten_only() {
        // Operator ruling 2026-08-29 ($50k research tier): the rule-7
        // numbers moved to $10k/$20k/$100k — boundaries re-pinned.
        // Per-row cap: $10,000.01 > $10,000.
        let r = row_json(
            "c1",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "10000.01",
        );
        assert_eq!(check(wrap_rows(&[r]).as_bytes()), Err(RulesetReject::Caps));
        // Per-row boundary passes (control).
        let r = row_json(
            "c2",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "10000.0",
        );
        assert_eq!(check(wrap_rows(&[r]).as_bytes()), Ok(()));
        // Per-sym Σ: 10k + 10k + 100.0 = 20,100 > 20,000 on sym 42.
        let mut rows = Vec::new();
        for (i, risk) in [(0u32, "10000.0"), (1, "10000.0"), (2, "100.0")] {
            let trig = format!(r#"{{"type":"level_breach","level":0.{:06}}}"#, i + 1);
            rows.push(row_json(
                &format!("c-sym-{i}"),
                &trig,
                "42",
                "ask",
                "0",
                "60000",
                risk,
            ));
        }
        assert_eq!(check(wrap_rows(&rows).as_bytes()), Err(RulesetReject::Caps));
        // Table Σ: 11 × $10k across 6 syms (≤ 2 rows = $20k per sym,
        // AT the per-sym cap) breaches $100k at row 11.
        let syms = ["7", "42", "99", "100", "101", "102"];
        let mut rows = Vec::new();
        for i in 0..11u32 {
            let sym = syms[(i / 2) as usize];
            let trig = format!(r#"{{"type":"level_breach","level":0.{:06}}}"#, i + 1);
            rows.push(row_json(
                &format!("c-tab-{i}"),
                &trig,
                sym,
                "ask",
                "0",
                "60000",
                "10000.0",
            ));
        }
        assert_eq!(check(wrap_rows(&rows).as_bytes()), Err(RulesetReject::Caps));
    }

    #[test]
    fn rule8_exact_duplicate_rows() {
        // Identical (sym, trigger, side, ref) — name/edge/horizon/
        // risk differ and are deliberately not part of the identity.
        let r1 = row_json(
            "d1",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "80",
            "1500",
            "50.0",
        );
        let r2 = row_json(
            "d2",
            r#"{"type":"cross_deviation","ref":7}"#,
            "42",
            "bid",
            "90",
            "3000",
            "25.0",
        );
        assert_eq!(
            check(wrap_rows(&[r1, r2]).as_bytes()),
            Err(RulesetReject::DuplicateRow)
        );
        // Same sym+side, different level: NOT a duplicate (control).
        let r1 = row_json(
            "d3",
            r#"{"type":"level_breach","level":0.01}"#,
            "42",
            "ask",
            "0",
            "60000",
            "10.0",
        );
        let r2 = row_json(
            "d4",
            r#"{"type":"level_breach","level":0.02}"#,
            "42",
            "ask",
            "0",
            "60000",
            "10.0",
        );
        assert_eq!(check(wrap_rows(&[r1, r2]).as_bytes()), Ok(()));
    }

    #[test]
    fn validator_happy_path_roundtrips_the_design_example() {
        // §4.1 example, adapted to the test universe (hormuz sym 99).
        let json = r#"
        {
          "rows": [
            {
              "name": "btc-pm-lag",
              "family": "crypto",
              "trigger": {"type": "cross_deviation", "ref": 7},
              "sym": 42,
              "side": "bid",
              "edge_bps": 80,
              "horizon_ms": 1500,
              "max_risk_usd": 50.0
            },
            {
              "name": "hormuz-floor",
              "family": "politics",
              "trigger": {"type": "level_breach", "level": 0.012},
              "sym": 99,
              "side": "ask",
              "edge_bps": 0,
              "horizon_ms": 60000,
              "max_risk_usd": 25.0
            }
          ]
        }
        "#;
        let bytes = json.as_bytes();
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        assert_eq!(validate_ruleset(bytes, &h, UNIVERSE, &DescriptorTable::empty(), &mut out), Ok(()));
        assert_eq!(out.len, 2);
        assert_eq!(out.hash128, h);
        assert_eq!(out.epoch, 0, "epoch is the side path's to stamp");
        assert_eq!(out.rows[0].sym, 42);
        assert_eq!(out.rows[0].ref_sym, 7);
        // v1 sugar shapes (VM2 V4 — the from_v1 law).
        assert_eq!(out.rows[0].combine, CombineOp::DiffBps as u8);
        assert_eq!(out.rows[0].cmp_bits, CMP_ENTRY_ABS);
        assert_eq!(out.rows[0].enter_1e9, 80_000_000_000);
        assert_eq!(out.rows[0].side, 0);
        assert_eq!(out.rows[0].family, 0);
        assert_eq!(out.rows[0].edge_bps, 80);
        assert_eq!(out.rows[0].horizon_ms, 1_500);
        assert_eq!(out.rows[0].max_risk_1e6, 50_000_000);
        assert_eq!(out.rows[0].name_h, fnv1a_64(b"btc-pm-lag"));
        assert_eq!(out.rows[1].sym, 99);
        assert_eq!(out.rows[1].ref_sym, SYMBOL_ID_NONE);
        // Ask-side level_breach → LhsOnly(Bid) ≥ level.
        assert_eq!(out.rows[1].combine, CombineOp::LhsOnly as u8);
        assert_eq!(out.rows[1].feat_a, FeatId::Bid as u8);
        assert_eq!(out.rows[1].cmp_bits, 0);
        assert_eq!(out.rows[1].enter_1e9, 12_000_000);
        assert_eq!(out.rows[1].side, 1);
        assert_eq!(out.rows[1].family, 1);
        assert_eq!(out.rows[1].max_risk_1e6, 25_000_000);
        assert_eq!(out.rows[1].name_h, fnv1a_64(b"hormuz-floor"));
    }

    #[test]
    fn validator_reject_leaves_len_zero_after_prior_success() {
        // Discard-on-reject: a good stage then a bad one must not
        // leave stale rows visible.
        let good = valid_json("good-then-bad").into_bytes();
        let digest = sha256(&good);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        assert_eq!(validate_ruleset(&good, &h, UNIVERSE, &DescriptorTable::empty(), &mut out), Ok(()));
        assert_eq!(out.len, 1);

        let bad = br#"{"rows":[]}"#;
        let digest = sha256(bad);
        let mut hb = [0u8; HASH128_LEN];
        hb.copy_from_slice(&digest[..HASH128_LEN]);
        assert_eq!(
            validate_ruleset(bad, &hb, UNIVERSE, &DescriptorTable::empty(), &mut out),
            Err(RulesetReject::RowCount)
        );
        assert_eq!(out.len, 0, "reject discards the scratch");
    }
}

#[cfg(test)]
mod v2_grammar_tests {
    use super::*;
    use core_types::{FeatId, GROUP_NONE, ROW_FLAG_POSITION};

    /// v2 descriptor fixture: a perp pair with funding+depth, a spot
    /// ref, an option, and a PM token (price-only).
    fn descs() -> DescriptorTable {
        DescriptorTable::from_entries(vec![
            (
                "okx:BTC-USDT-SWAP".to_owned(),
                0x0200_0001,
                CAP_PRICE | CAP_FUNDING | CAP_DEPTH,
            ),
            ("binance:btcusdt".to_owned(), 0x0100_0007, CAP_PRICE),
            (
                "binance-usdm:btcusdt".to_owned(),
                0x0100_0200,
                CAP_PRICE | CAP_FUNDING,
            ),
            (
                "deribit:BTC-27MAR26-60000-C".to_owned(),
                0x0300_0201,
                CAP_OPT | CAP_PRICE,
            ),
            ("2875608808".to_owned(), 42, CAP_PRICE),
        ])
    }

    fn check_v2(json: &str) -> Result<(), RulesetReject> {
        let bytes = json.as_bytes();
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        validate_ruleset(bytes, &h, &[7, 42], &descs(), &mut out)
    }

    fn check_v2_table(json: &str) -> Result<Box<RuleTableV2>, RulesetReject> {
        let bytes = json.as_bytes();
        let digest = sha256(bytes);
        let mut h = [0u8; HASH128_LEN];
        h.copy_from_slice(&digest[..HASH128_LEN]);
        let mut out = Box::new(RuleTableV2::EMPTY);
        validate_ruleset(bytes, &h, &[7, 42], &descs(), &mut out)?;
        Ok(out)
    }

    /// The cvfc-v2 shape: funding spread pair with a 72 h confirm,
    /// groups, min/max holds.
    const CARRY: &str = r#"{"rows":[{"name":"carry-btc","family":"crypto","instrument":"okx:BTC-USDT-SWAP","ref":"binance-usdm:btcusdt","feature":"apr24","combine":"diff","enter":0.20,"exit":0.0,"confirm_feature":"apr72","confirm":0.30,"confirm_abs":true,"confirm_pair":true,"group":3,"min_hold_s":345600,"max_hold_s":864000,"horizon_ms":60000,"max_risk_usd":9900.0}]}"#;

    #[test]
    fn v2_position_pair_row_builds_correctly() {
        let t = check_v2_table(CARRY).expect("carry row valid");
        assert_eq!(t.len, 1);
        let r = &t.rows[0];
        assert_eq!(r.flags, ROW_FLAG_POSITION, "exit present ⇒ position");
        assert_eq!(r.sym, 0x0200_0001);
        assert_eq!(r.ref_sym, 0x0100_0200);
        assert_eq!(r.feat_a, FeatId::Apr24 as u8);
        assert_eq!(r.feat_b, FeatId::Apr24 as u8, "ref_feature defaults");
        assert_eq!(r.feat_c, FeatId::Apr72 as u8);
        assert_eq!(r.combine, CombineOp::Diff as u8);
        assert_eq!(r.enter_1e9, 200_000_000);
        assert_eq!(r.exit_1e9, 0);
        assert_eq!(r.confirm_1e9, 300_000_000);
        assert_eq!(
            r.cmp_bits,
            CMP_CONFIRM_ABS | CMP_CONFIRM_PAIR,
            "entry ge signed; confirm abs pair"
        );
        assert_eq!(r.group, 3);
        assert_eq!(r.min_hold_s, 345_600);
        assert_eq!(r.max_hold_s, 864_000);
        assert_eq!(r.edge_bps, 0, "no sugar mirror on native rows");
        assert_eq!(r.family, 0);
    }

    #[test]
    fn v2_lhs_only_and_negative_thresholds() {
        // Depth-imbalance refire row with a NEGATIVE threshold and
        // `le` — buy pressure exhaustion shape.
        let t = check_v2_table(
            r#"{"rows":[{"name":"imb","instrument":"okx:BTC-USDT-SWAP","feature":"depth_imb","enter":-0.5,"cmp":"le","horizon_ms":1000,"max_risk_usd":100.0,"side":"bid"}]}"#,
        )
        .expect("valid");
        let r = &t.rows[0];
        assert_eq!(r.flags, 0, "no exit ⇒ refire");
        assert_eq!(r.combine, CombineOp::LhsOnly as u8);
        assert_eq!(r.enter_1e9, -500_000_000);
        assert_eq!(r.cmp_bits, CMP_ENTRY_LE);
        assert_eq!(r.family, 4, "family defaults to other");
        assert_eq!(r.group, GROUP_NONE);
    }

    #[test]
    fn v2_unresolvable_descriptor_refuses() {
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:DOGE-USDT-SWAP","feature":"mid","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Descriptor),
            "D-6: unresolvable ⇒ REFUSE, fail-fast"
        );
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","ref":"okx:ETH-USDT-SWAP","feature":"mid","combine":"diff_bps","enter":4.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Descriptor),
            "ref resolves too or the row refuses"
        );
    }

    #[test]
    fn v2_rule10_channel_capabilities() {
        // Funding feature on a funding-less spot ref: reject.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","ref":"binance:btcusdt","feature":"apr24","combine":"diff","enter":0.2,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
        // Depth feature on a PM token: reject.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"2875608808","feature":"depth_imb","enter":0.5,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
        // IV on the option: fine.
        assert!(check_v2(
            r#"{"rows":[{"name":"x","instrument":"deribit:BTC-27MAR26-60000-C","feature":"mark_iv","enter":0.65,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
        )
        .is_ok());
        // Confirm feature capabilities count too.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"binance:btcusdt","feature":"mid","enter":1.0,"confirm_feature":"apr24","confirm":0.1,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
    }

    #[test]
    fn v2_rule3_window_law() {
        // Rolling feature REQUIRES a window…
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"roll_mean","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
        // …and a non-rolling feature forbids one.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","window_min":60,"enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
        // Over-cap window rejects.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"roll_mean","window_min":4321,"enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Feature)
        );
        // A valid z-score-ish shape passes (mid vs rolling mean).
        assert!(check_v2(
            r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","ref":"okx:BTC-USDT-SWAP","feature":"mid","ref_feature":"roll_mean","ref_window_min":60,"combine":"diff_bps","enter":25.0,"abs":true,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
        )
        .is_err(), "self-ref rejects (Symbol) — z-score uses the SAME sym only via distinct legs law");
        // The honest z-score shape: LhsOnly is not enough — the ref
        // must be a DIFFERENT instrument, so mid-vs-own-mean lands in
        // V5+ as a dedicated feature; today the reject is loud.
    }

    #[test]
    fn v2_rule9_position_shape() {
        // group without exit ⇒ Position reject.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"group":1,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Position)
        );
        // min_hold without exit ⇒ Position reject.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"min_hold_s":60,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Position)
        );
        // max_hold ≤ min_hold ⇒ Position reject.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"exit":0.5,"min_hold_s":600,"max_hold_s":600,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Position)
        );
    }

    #[test]
    fn v2_mixed_shape_and_grammar_rejects() {
        // v1 + v2 keys in one row: Grammar.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","sym":42,"instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Grammar)
        );
        // combine without ref: Grammar.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","combine":"diff","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Grammar)
        );
        // ref without combine: Grammar.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","ref":"binance:btcusdt","feature":"mid","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Grammar)
        );
        // confirm threshold without confirm_feature: Grammar.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"confirm":0.5,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
            ),
            Err(RulesetReject::Grammar)
        );
        // Unknown feature token: Grammar.
        assert_eq!(
            check_v2(
                r#"{"rows":[{"name":"x","instrument":"okx:BTC-USDT-SWAP","feature":"vibes","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0}]}"#,
            ),
            Err(RulesetReject::Grammar)
        );
    }

    #[test]
    fn v2_two_leg_caps_count_both_legs() {
        // One $9 900 two-leg row: table total = $19 800. Six such
        // rows on DISTINCT syms would blow the $100k table cap at
        // row 6 — but per-sym caps trip first on shared syms, so use
        // the SAME pair: row 3 breaches the $20k per-sym budget
        // ($9 900 × 3 = $29 700 > $20k on EACH leg's sym).
        let row = |n: u8, e: &str| format!(
            r#"{{"name":"r{n}","instrument":"okx:BTC-USDT-SWAP","ref":"binance-usdm:btcusdt","feature":"apr24","combine":"diff","enter":{e},"exit":0.0,"horizon_ms":1000,"max_risk_usd":9900.0}}"#
        );
        let two = format!(r#"{{"rows":[{},{}]}}"#, row(1, "0.20"), row(2, "0.25"));
        assert!(check_v2(&two).is_ok(), "2 × $9.9k per sym fits $20k");
        let three = format!(
            r#"{{"rows":[{},{},{}]}}"#,
            row(1, "0.20"),
            row(2, "0.25"),
            row(3, "0.30")
        );
        assert_eq!(check_v2(&three), Err(RulesetReject::Caps));
    }

    #[test]
    fn v2_duplicate_identity_rejects() {
        let row = r#"{"name":"a","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}"#;
        let dup = r#"{"name":"b","instrument":"okx:BTC-USDT-SWAP","feature":"mid","enter":1.0,"horizon_ms":9000,"max_risk_usd":20.0,"side":"bid"}"#;
        let json = format!(r#"{{"rows":[{row},{dup}]}}"#);
        assert_eq!(check_v2(&json), Err(RulesetReject::DuplicateRow));
    }

    #[test]
    fn v2_and_v1_rows_coexist_in_one_artifact() {
        // D-6 compat: a v1 row (raw sym, trigger sugar) beside a v2
        // row in the SAME artifact — both admit.
        let json = r#"{"rows":[{"name":"old","family":"crypto","trigger":{"type":"cross_deviation","ref":7},"sym":42,"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":50.0},{"name":"new","instrument":"okx:BTC-USDT-SWAP","feature":"apr24","enter":0.2,"horizon_ms":1000,"max_risk_usd":10.0,"side":"ask"}]}"#;
        let t = check_v2_table(json).expect("both arms admit");
        assert_eq!(t.len, 2);
        assert_eq!(t.rows[0].combine, CombineOp::DiffBps as u8);
        assert_eq!(t.rows[1].feat_a, FeatId::Apr24 as u8);
    }

    #[test]
    fn v2_bind_budget_per_sym_exhausts() {
        // 9 distinct windows on ONE sym exceeds the 8-per-sym bind
        // cap ⇒ Feature reject on the 9th.
        let mut rows = String::new();
        for w in 1..=9u32 {
            if w > 1 {
                rows.push(',');
            }
            rows.push_str(&format!(
                r#"{{"name":"w{w}","instrument":"okx:BTC-USDT-SWAP","feature":"roll_mean","window_min":{},"enter":1.0,"horizon_ms":1000,"max_risk_usd":1.0,"side":"bid"}}"#,
                w * 10
            ));
        }
        let json = format!(r#"{{"rows":[{rows}]}}"#);
        assert_eq!(check_v2(&json), Err(RulesetReject::Feature));
    }

    #[test]
    fn v2_signal_scanner_keeps_nine_decimals() {
        // Funding-rate-sized thresholds must survive: 0.0000593.
        let t = check_v2_table(
            r#"{"rows":[{"name":"tiny","instrument":"okx:BTC-USDT-SWAP","feature":"apr24","enter":0.0000593,"horizon_ms":1000,"max_risk_usd":10.0,"side":"bid"}]}"#,
        )
        .expect("valid");
        assert_eq!(t.rows[0].enter_1e9, 59_300);
    }
}
