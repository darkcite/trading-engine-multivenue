// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pmj.toml` — the PMJ member's artifact (slot 11; risk-policy "PMJ — slot
//! 11", project doc `claude/pmj-plan-2026-10-18.md`).
//!
//! The `pm15.toml` grammar ([`crate::pm15`]), one `[pmj]` section, every
//! bound checked at parse; the engine hashes the file BYTES so the boot tell
//! names the artifact in force. It carries:
//!
//! - **the law** — the H90 values (provisional: the 15-minute law's until
//!   the 5-minute research freezes its own) plus `price_floor_1e6`;
//! - **the Jupiter keys** — `min_tau_s` (the keeper's lead), the deposit
//!   per order and the per-order maximum the arm enforces, the market cap,
//!   the session loss bound, `max_slippage_bps` (PARSED, UNUSED until J0
//!   shows the venue lets an order set it);
//! - **the paper law's key** — `paper_delay_ms`, the keeper's delay the
//!   paper dispatcher judges an order at;
//! - **the transaction guard** (LAW PMJ-G, `exec_jupiter::tx::guard`) —
//!   `guard_programs`, `guard_alts`, `guard_deposit_account`,
//!   `guard_max_sol_lamports`, `guard_max_cu_price`: base58 keys kept as
//!   text here (checked for the alphabet and length) and decoded by the
//!   live arm's boot (J5). **Empty lists refuse every transaction** — the
//!   default until J0's decoded programs and tables are reviewed in;
//! - **the create-order guard's pins** (PMJ W5, `exec_jupiter::tx::guard_order`)
//!   — `guard_vault`, the vault every order and claim must name (absent =
//!   none: every one refused), and `guard_vault_alt` = `"<table>:<entry>"`,
//!   the lookup-table entry that loads it (absent = the vault must be a
//!   static key; the table must be one of `guard_alts`);
//! - **`guard_cosigners`** — the DRAFT co-signer amendment (R-PMJ-S44,
//!   vault `pmj-g-amendment-draft.md`): at most one pinned read-only
//!   co-signer. **Absent or empty = LAW PMJ-G as written (exactly one
//!   signature, ours)**, and it stays empty in every committed config: the
//!   amendment becomes law only after Anton's own J0 build confirms the
//!   co-signer and he rules;
//! - **`build_extra`** (PMJ W5) — at most four `"name:value"` members the
//!   order build's body carries (J0's limit-order request field), the
//!   value a literal `[A-Za-z0-9_.]{1,32}` or exactly `{limit_micro}` /
//!   `{limit_usd}` (the member's limit). Absent = today's body; the guard
//!   is the law whatever the request says.
//! - **the live wallet's keys** (PMJ J16, read only by the live arm's boot
//!   and `pmj-exec --key-check`; a paper boot reads none of them) —
//!   `guard_deposit_mint`, the deposit mint (USDC or JupUSD, the two
//!   6-decimal USD mints the cap check reads in micro dollars; R-PMJ-S46 is
//!   undecided, so there is NO default: absent = the live boot refuses);
//!   `min_sol_lamports`, the wallet's SOL floor at boot (absent = 0 = unset:
//!   the live boot refuses); `guard_program_slot`, the Predict program's
//!   reviewed deploy slot (absent = 0 = unpinned; the key check compares
//!   it with the chain — a new deploy is a review trigger).

use crate::bin15::{int, opt_int, pos_u64, strs, table};
use crate::icdp::{parse_single_section, Value};
use std::path::Path;

/// Points in the late Φ table (`core_twaplock::late::LATE_PHI_POINTS`).
pub const PMJ_PHI_POINTS: usize = 4097;

/// The least `phi_lut[4096]` the grammar admits.
pub const PMJ_PHI_LAST_MIN_1E6: u32 = 999_999;

/// Families one artifact may configure (`core_types::pm_roll::PM5_ROLLING_MAX`).
pub const PMJ_MAX_FAMILIES: usize = 8;

/// The widest late window, ns to the end.
pub const PMJ_TAU_MAX_NS_LIMIT: u64 = 60_000_000_000;

/// The most one order or one market may spend, ×1e6 USD — a guard on a
/// slipped scale, not the frame.
pub const PMJ_USD_LIMIT_1E6: i64 = 10_000_000_000;

/// The venue's least deposit, ×1e6 USD ($5).
pub const PMJ_MIN_DEPOSIT_1E6: i64 = 5_000_000;

/// What the member's sizing may add above the deposit, ×1e6 USD: it buys
/// whole hundredths of a share and one more when the rounding left the
/// order under $5, and a hundredth at a price under $1 costs under $0.01.
/// The per-order maximum is at least the deposit plus this (risk review
/// J4 round 1: the arm refuses an order above it).
pub const PMJ_SIZING_SLACK_1E6: i64 = 10_000;

/// Program ids / lookup tables the guard may list
/// (`exec_jupiter::tx::{GUARD_PROGRAMS_MAX, GUARD_ALTS_MAX}`).
pub const PMJ_GUARD_PROGRAMS_MAX: usize = 16;
/// See [`PMJ_GUARD_PROGRAMS_MAX`].
pub const PMJ_GUARD_ALTS_MAX: usize = 8;
/// Co-signers the dormant amendment may pin (R-PMJ-S44): one.
pub const PMJ_GUARD_COSIGNERS_MAX: usize = 1;
/// `build_extra` members (`exec_jupiter::render::BUILD_EXTRA_MAX`).
pub const PMJ_BUILD_EXTRA_MAX: usize = 4;

/// PMJ J16: the deposit mints `guard_deposit_mint` may name (R-PMJ-S46):
/// `(name, base58)`. Both are 6-decimal USD stablecoins, so a balance in
/// base units is micro dollars — the unit of `[exec.slot.11]
/// cap_day_usd_1e6` the live boot judges the wallet against.
pub const PMJ_DEPOSIT_MINTS: [(&str, &str); 2] = [
    ("USDC", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
    ("JupUSD", "JuprjznTrTSp2UFa3ZBUFgwdAmtZCq4MQCwysN55USD"),
];

/// PMJ J16: the widest `min_sol_lamports` the grammar admits (10 SOL) — a
/// guard on a slipped scale.
pub const PMJ_MIN_SOL_LAMPORTS_LIMIT: i64 = 10_000_000_000;

/// PMJ J16: the widest `guard_program_slot` the grammar admits.
pub const PMJ_PROGRAM_SLOT_LIMIT: i64 = 1_000_000_000_000;

/// Program ids a co-signer may never be: a co-signer is a signing key, not
/// a program or an account the guard reasons about.
const NOT_A_COSIGNER: [&str; 5] = [
    "11111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    "ComputeBudget111111111111111111111111111111",
];

/// The members the build body renders itself: a `build_extra` may not
/// name one (`exec_jupiter::render::BODY_MEMBERS`).
const BODY_MEMBERS: [&str; 6] = ["ownerPubkey", "marketId", "isYes", "isBuy", "depositAmount", "depositMint"];

/// A `pmj.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmjError(pub String);

impl std::fmt::Display for PmjError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pmj.toml: {}", self.0)
    }
}

impl std::error::Error for PmjError {}

fn err(msg: impl Into<String>) -> PmjError {
    PmjError(msg.into())
}

/// Every key the grammar accepts; an unknown key is a refusal.
const PMJ_KEYS: [&str; 36] = [
    "families",
    "perp",
    "tau_min_ns",
    "tau_max_ns",
    "p_min_1e6",
    "e_1e6",
    "persist",
    "sigma_k_1e3",
    "price_floor_1e6",
    "min_tau_s",
    "deposit_usd_1e6",
    "max_order_usd_1e6",
    "usd_market_1e6",
    "session_loss_usd_1e6",
    "max_orders_market",
    "order_ttl_ns",
    "max_slippage_bps",
    "paper_delay_ms",
    "live_assets",
    "phi_lut",
    "guard_programs",
    "guard_alts",
    "guard_deposit_account",
    "guard_max_sol_lamports",
    "guard_max_cu_price",
    // PMJ J12: crate::strike_law's three keys.
    "strike_law",
    "sigma_res_ratio_1e3",
    "sigma_res_min_ppm",
    // PMJ J13: the same-side family cap per window.
    "max_side_families",
    // PMJ W5: the create-order guard's pins, the dormant co-signer, the
    // limit-order request.
    "guard_vault",
    "guard_vault_alt",
    "guard_cosigners",
    "build_extra",
    // PMJ J16: the live wallet's keys (the live boot and the key check).
    "guard_deposit_mint",
    "min_sol_lamports",
    "guard_program_slot",
];

/// `pmj.toml` as parsed. Descriptors and keys stay strings: `pmj_boot`
/// resolves the descriptors, the live arm's boot decodes the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmjFile {
    /// The families' assets, equal to the universe's `:5m` rolling assets
    /// as a sequence.
    pub families: Vec<String>,
    /// Per family, the fast-venue instrument descriptor of the nowcast.
    pub perp: Vec<String>,
    /// The late window's floor, ns to the end.
    pub tau_min_ns: u64,
    /// The late window's ceiling, ns to the end.
    pub tau_max_ns: u64,
    /// The least belief on the favoured side ×1e6.
    pub p_min_1e6: i64,
    /// The margin the ask must leave under it ×1e6.
    pub e_1e6: i64,
    /// Consecutive report seconds, `1..=8`.
    pub persist: u8,
    /// The pricer's σ scale ×1e3, `250..=4000`.
    pub sigma_k_1e3: i64,
    /// The favourite's ask must be at least this ×1e6 (absent = 0.90).
    pub price_floor_1e6: i64,
    /// No order closer to the end than this, s (absent = 3).
    pub min_tau_s: u64,
    /// What one order deposits ×1e6 USD (≥ $5).
    pub deposit_usd_1e6: i64,
    /// The per-order maximum the arm enforces ×1e6 USD (absent = the
    /// deposit + [`PMJ_SIZING_SLACK_1E6`]; never under that).
    pub max_order_usd_1e6: i64,
    /// Most one market holds ×1e6 USD.
    pub usd_market_1e6: i64,
    /// The session loss bound ×1e6 USD (positive).
    pub session_loss_usd_1e6: i64,
    /// IoCs at most per market (absent = 4).
    pub max_orders_market: u8,
    /// An IoC's time-to-live, ns (absent = 5 s).
    pub order_ttl_ns: u64,
    /// The slippage bound, bps — parsed, UNUSED (absent = 0).
    pub max_slippage_bps: u32,
    /// The keeper's delay the paper law judges an order at, ms (absent =
    /// 2 000).
    pub paper_delay_ms: u64,
    /// The assets that may arm live (absent = none); a subset of
    /// `families`. Nothing arms in this pass.
    pub live_assets: Vec<String>,
    /// `Φ(z) ×1e6` for `z = 0, 0.002, …, 8.192`.
    pub phi_lut: Vec<u32>,
    /// The guard's program ids, base58 (absent = none: refuse all).
    pub guard_programs: Vec<String>,
    /// The guard's lookup tables, base58 (absent = none).
    pub guard_alts: Vec<String>,
    /// The wallet's USDC token account, base58 (absent = none: every token
    /// debit refused).
    pub guard_deposit_account: String,
    /// The most lamports the wallet may send per transaction (absent = 0).
    pub guard_max_sol_lamports: u64,
    /// The highest compute unit price, micro-lamports (absent = 0).
    pub guard_max_cu_price: u64,
    /// PMJ W5: the vault every order and claim names, base58 (absent =
    /// none: every one refused).
    pub guard_vault: String,
    /// PMJ W5: `"<table base58>:<entry>"`, the lookup-table entry that
    /// loads the vault (absent = the vault must be a static key).
    pub guard_vault_alt: String,
    /// The pinned co-signer, base58 (absent = none: exactly one signature,
    /// LAW PMJ-G as before; at most one, R-PMJ-S44 — dormant).
    pub guard_cosigners: Vec<String>,
    /// PMJ W5: the build body's extra `"name:value"` members (absent =
    /// none: today's body).
    pub build_extra: Vec<String>,
    /// PMJ J12: the strike law and law (iii)'s residual
    /// ([`crate::strike_law`]; absent = the first law).
    pub strike_law: crate::strike_law::StrikeLaw,
    /// PMJ J13: at most this many families hold, or are buying, one side of
    /// one window (absent = 0, off; at most the family count).
    pub max_side_families: u8,
    /// PMJ J16: the deposit mint, base58 — one of [`PMJ_DEPOSIT_MINTS`]
    /// (absent = none: the live boot refuses; no default, R-PMJ-S46).
    pub guard_deposit_mint: String,
    /// PMJ J16: the wallet's SOL floor at the live boot, lamports (absent =
    /// 0 = unset: the live boot refuses).
    pub min_sol_lamports: u64,
    /// PMJ J16: the Predict program's reviewed deploy slot (absent = 0 =
    /// unpinned; `pmj-exec --key-check` compares it with the chain).
    pub guard_program_slot: u64,
}

/// Read and parse the artifact, returning it with its RAW BYTES.
///
/// # Errors
/// [`PmjError`] — unreadable, not UTF-8, or the grammar refused it.
pub fn load(path: &Path) -> Result<(PmjFile, Vec<u8>), PmjError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/pmj.toml`.
///
/// # Errors
/// `HOME` is not set.
pub fn default_pmj_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/pmj.toml")
}

fn in_range(key: &str, v: i64, lo: i64, hi: i64) -> Result<i64, PmjError> {
    if !(lo..=hi).contains(&v) {
        return Err(err(format!("`{key}` = {v} is outside {lo}..={hi}")));
    }
    Ok(v)
}

/// An optional string array (absent or `[]` = empty).
fn opt_strs(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<String>, PmjError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        None => Ok(Vec::new()),
        Some((_, Value::Strs(v), _)) => Ok(v.clone()),
        Some((_, Value::Ints(v), _)) if v.is_empty() => Ok(Vec::new()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string array"))),
    }
}

/// An optional string (absent = empty).
fn opt_str(kv: &[(String, Value, usize)], key: &str) -> Result<String, PmjError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        None => Ok(String::new()),
        Some((_, Value::Str(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string"))),
    }
}

/// A 32-byte key in base58: 32..=44 characters of the Bitcoin alphabet.
fn b58_key(key: &str, i: usize, v: &str) -> Result<(), PmjError> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if !(32..=44).contains(&v.len()) || !v.bytes().all(|b| ALPHABET.contains(&b)) {
        return Err(err(format!("`{key}`[{i}] = `{v}` is not a base58 key")));
    }
    Ok(())
}

/// One `build_extra` member: `name:value` (the grammar in the module docs).
fn build_extra_item(i: usize, spec: &str) -> Result<&str, PmjError> {
    let Some((name, value)) = spec.split_once(':') else {
        return Err(err(format!("`build_extra`[{i}] = `{spec}` is not `name:value`")));
    };
    let mut nb = name.bytes();
    let name_ok = name.len() <= 32
        && nb.next().is_some_and(|b| b.is_ascii_alphabetic())
        && nb.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !name_ok {
        return Err(err(format!("`build_extra`[{i}]: the name `{name}` is not [A-Za-z][A-Za-z0-9_]{{0,31}}")));
    }
    if BODY_MEMBERS.iter().any(|m| m.eq_ignore_ascii_case(name)) {
        return Err(err(format!("`build_extra`[{i}]: `{name}` is a member the body renders itself")));
    }
    let value_ok = value == "{limit_micro}"
        || value == "{limit_usd}"
        || ((1..=32).contains(&value.len())
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'));
    if !value_ok {
        return Err(err(format!(
            "`build_extra`[{i}]: the value `{value}` is neither [A-Za-z0-9_.]{{1,32}} nor `{{limit_micro}}` / `{{limit_usd}}`"
        )));
    }
    Ok(name)
}

/// Parse the artifact.
///
/// # Errors
/// [`PmjError`] — the first rule the source breaks.
pub fn parse(src: &str) -> Result<PmjFile, PmjError> {
    let kv = parse_single_section(src, "pmj", &PMJ_KEYS).map_err(|e| err(e.0))?;
    let m = |e: crate::bin15::Bin15Error| err(e.0.replace("bin15", "pmj"));
    let families = strs(&kv, "families").map_err(m)?;
    let perp = strs(&kv, "perp").map_err(m)?;
    if families.is_empty() || families.len() > PMJ_MAX_FAMILIES {
        return Err(err(format!("`families` must name 1..={PMJ_MAX_FAMILIES} assets")));
    }
    let mut i = 0usize;
    while i < families.len() {
        let a = &families[i];
        if a.is_empty()
            || a.len() > 15
            || !a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(err(format!(
                "`families`[{i}] = `{a}`: an asset is 1..=15 bytes of [a-z0-9]"
            )));
        }
        if families[..i].contains(a) {
            return Err(err(format!("`families`[{i}] = `{a}` repeats")));
        }
        i += 1;
    }
    if perp.len() != families.len() {
        return Err(err(format!(
            "`perp` carries {} descriptors for {} families — one fast-venue instrument per family",
            perp.len(),
            families.len()
        )));
    }
    let tau_min_ns = pos_u64(&kv, "tau_min_ns").map_err(m)?;
    let tau_max_ns = pos_u64(&kv, "tau_max_ns").map_err(m)?;
    if tau_min_ns == 0 || tau_min_ns >= tau_max_ns || tau_max_ns > PMJ_TAU_MAX_NS_LIMIT {
        return Err(err(format!(
            "the late window must satisfy 0 < tau_min_ns < tau_max_ns ≤ {PMJ_TAU_MAX_NS_LIMIT}"
        )));
    }
    let p_min_1e6 = in_range("p_min_1e6", int(&kv, "p_min_1e6").map_err(m)?, 500_000, 999_999)?;
    let e_1e6 = in_range("e_1e6", int(&kv, "e_1e6").map_err(m)?, 0, p_min_1e6 - 1)?;
    let persist = in_range("persist", int(&kv, "persist").map_err(m)?, 1, 8)? as u8;
    let sigma_k_1e3 = in_range("sigma_k_1e3", int(&kv, "sigma_k_1e3").map_err(m)?, 250, 4_000)?;
    let price_floor_1e6 = in_range(
        "price_floor_1e6",
        opt_int(&kv, "price_floor_1e6", 900_000).map_err(m)?,
        500_000,
        999_999,
    )?;
    let min_tau_s = in_range("min_tau_s", opt_int(&kv, "min_tau_s", 3).map_err(m)?, 0, 59)? as u64;
    if min_tau_s.saturating_mul(1_000_000_000) >= tau_max_ns {
        return Err(err("`min_tau_s` must be under the late window's ceiling (tau_max_ns)"));
    }
    let deposit_usd_1e6 = in_range(
        "deposit_usd_1e6",
        int(&kv, "deposit_usd_1e6").map_err(m)?,
        PMJ_MIN_DEPOSIT_1E6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let max_order_usd_1e6 = in_range(
        "max_order_usd_1e6",
        opt_int(&kv, "max_order_usd_1e6", deposit_usd_1e6 + PMJ_SIZING_SLACK_1E6).map_err(m)?,
        deposit_usd_1e6 + PMJ_SIZING_SLACK_1E6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let usd_market_1e6 = in_range(
        "usd_market_1e6",
        int(&kv, "usd_market_1e6").map_err(m)?,
        deposit_usd_1e6,
        PMJ_USD_LIMIT_1E6,
    )?;
    let session_loss_usd_1e6 = in_range(
        "session_loss_usd_1e6",
        int(&kv, "session_loss_usd_1e6").map_err(m)?,
        1,
        PMJ_USD_LIMIT_1E6,
    )?;
    let max_orders_market =
        in_range("max_orders_market", opt_int(&kv, "max_orders_market", 4).map_err(m)?, 1, 16)? as u8;
    let order_ttl_ns = in_range(
        "order_ttl_ns",
        opt_int(&kv, "order_ttl_ns", 5_000_000_000).map_err(m)?,
        1_000_000_000,
        30_000_000_000,
    )? as u64;
    let max_slippage_bps =
        in_range("max_slippage_bps", opt_int(&kv, "max_slippage_bps", 0).map_err(m)?, 0, 10_000)? as u32;
    let paper_delay_ms =
        in_range("paper_delay_ms", opt_int(&kv, "paper_delay_ms", 2_000).map_err(m)?, 0, 30_000)? as u64;
    if paper_delay_ms.saturating_mul(1_000_000) >= order_ttl_ns.saturating_mul(2) {
        return Err(err(
            "`paper_delay_ms` must be under twice `order_ttl_ns` (the member writes an order off then)",
        ));
    }
    // The keeper's lead bounds the paper delay: an order decided at the late
    // window's floor must still be judged before the close (risk review J4
    // round 1), or paper books fills the venue could never give.
    let lead_ns = tau_min_ns.max(min_tau_s.saturating_mul(1_000_000_000));
    if paper_delay_ms.saturating_mul(1_000_000) >= lead_ns {
        return Err(err(format!(
            "`paper_delay_ms` = {paper_delay_ms} must be under the keeper's lead, max(tau_min_ns, min_tau_s) = {} ms \
             — so an order decided at the window's floor is still judged before the close",
            lead_ns / 1_000_000
        )));
    }
    let live_assets = if kv.iter().any(|(k, _, _)| k == "live_assets") {
        strs(&kv, "live_assets").map_err(m)?
    } else {
        Vec::new()
    };
    let mut i = 0usize;
    while i < live_assets.len() {
        if !families.contains(&live_assets[i]) {
            return Err(err(format!("`live_assets`[{i}] = `{}` is not a family", live_assets[i])));
        }
        i += 1;
    }
    let phi_lut = table(&kv, "phi_lut", PMJ_PHI_POINTS, Some(500_000), Some(PMJ_PHI_LAST_MIN_1E6), false)
        .map_err(m)?;
    let guard_programs = opt_strs(&kv, "guard_programs")?;
    let guard_alts = opt_strs(&kv, "guard_alts")?;
    if guard_programs.len() > PMJ_GUARD_PROGRAMS_MAX || guard_alts.len() > PMJ_GUARD_ALTS_MAX {
        return Err(err(format!(
            "the guard lists at most {PMJ_GUARD_PROGRAMS_MAX} programs and {PMJ_GUARD_ALTS_MAX} lookup tables"
        )));
    }
    let mut i = 0usize;
    while i < guard_programs.len() {
        b58_key("guard_programs", i, &guard_programs[i])?;
        i += 1;
    }
    let mut i = 0usize;
    while i < guard_alts.len() {
        b58_key("guard_alts", i, &guard_alts[i])?;
        i += 1;
    }
    let guard_deposit_account = opt_str(&kv, "guard_deposit_account")?;
    if !guard_deposit_account.is_empty() {
        b58_key("guard_deposit_account", 0, &guard_deposit_account)?;
    }
    let guard_vault = opt_str(&kv, "guard_vault")?;
    if !guard_vault.is_empty() {
        b58_key("guard_vault", 0, &guard_vault)?;
    }
    let guard_deposit_mint = opt_str(&kv, "guard_deposit_mint")?;
    if !guard_deposit_mint.is_empty() && !PMJ_DEPOSIT_MINTS.iter().any(|(_, m)| *m == guard_deposit_mint) {
        return Err(err(format!(
            "`guard_deposit_mint` = `{guard_deposit_mint}` is neither USDC ({}) nor JupUSD ({}) — the \
             two 6-decimal USD mints the live boot's cap check reads in micro dollars (R-PMJ-S46)",
            PMJ_DEPOSIT_MINTS[0].1, PMJ_DEPOSIT_MINTS[1].1
        )));
    }
    let guard_vault_alt = opt_str(&kv, "guard_vault_alt")?;
    if !guard_vault_alt.is_empty() {
        let Some((table, entry)) = guard_vault_alt.split_once(':') else {
            return Err(err(format!("`guard_vault_alt` = `{guard_vault_alt}` is not `<table>:<entry>`")));
        };
        b58_key("guard_vault_alt", 0, table)?;
        if entry.is_empty() || !entry.bytes().all(|b| b.is_ascii_digit()) || entry.parse::<u8>().is_err() {
            return Err(err(format!("`guard_vault_alt`: the entry `{entry}` is not 0..=255")));
        }
        if guard_vault.is_empty() {
            return Err(err("`guard_vault_alt` names where the vault loads from: `guard_vault` must name the vault"));
        }
        if !guard_alts.iter().any(|a| a == table) {
            return Err(err(format!("`guard_vault_alt`'s table `{table}` is not one of `guard_alts`")));
        }
    }
    let guard_cosigners = opt_strs(&kv, "guard_cosigners")?;
    if guard_cosigners.len() > PMJ_GUARD_COSIGNERS_MAX {
        return Err(err(format!(
            "`guard_cosigners` pins at most {PMJ_GUARD_COSIGNERS_MAX} co-signer (R-PMJ-S44)"
        )));
    }
    let mut i = 0usize;
    while i < guard_cosigners.len() {
        let c = &guard_cosigners[i];
        b58_key("guard_cosigners", i, c)?;
        if guard_programs.contains(c)
            || guard_alts.contains(c)
            || *c == guard_deposit_account
            || *c == guard_vault
            || *c == guard_deposit_mint
            || NOT_A_COSIGNER.contains(&c.as_str())
        {
            return Err(err(format!(
                "`guard_cosigners`[{i}] = `{c}` is a program, a lookup table, the deposit account or the vault — \
                 a co-signer is a signing key, not an account the guard reasons about"
            )));
        }
        i += 1;
    }
    let build_extra = opt_strs(&kv, "build_extra")?;
    if build_extra.len() > PMJ_BUILD_EXTRA_MAX {
        return Err(err(format!("`build_extra` carries at most {PMJ_BUILD_EXTRA_MAX} members")));
    }
    let mut i = 0usize;
    while i < build_extra.len() {
        let name = build_extra_item(i, &build_extra[i])?;
        let mut j = 0usize;
        while j < i {
            if build_extra[j].split_once(':').map(|(n, _)| n) == Some(name) {
                return Err(err(format!("`build_extra`[{i}]: `{name}` repeats")));
            }
            j += 1;
        }
        i += 1;
    }
    let strike_law = crate::strike_law::parse(&kv, families.len()).map_err(err)?;
    let max_side_families = in_range(
        "max_side_families",
        opt_int(&kv, "max_side_families", 0).map_err(m)?,
        0,
        families.len() as i64,
    )? as u8;
    let guard_max_sol_lamports = in_range(
        "guard_max_sol_lamports",
        opt_int(&kv, "guard_max_sol_lamports", 0).map_err(m)?,
        0,
        1_000_000_000,
    )? as u64;
    let guard_max_cu_price = in_range(
        "guard_max_cu_price",
        opt_int(&kv, "guard_max_cu_price", 0).map_err(m)?,
        0,
        10_000_000,
    )? as u64;
    let min_sol_lamports = in_range(
        "min_sol_lamports",
        opt_int(&kv, "min_sol_lamports", 0).map_err(m)?,
        0,
        PMJ_MIN_SOL_LAMPORTS_LIMIT,
    )? as u64;
    let guard_program_slot = in_range(
        "guard_program_slot",
        opt_int(&kv, "guard_program_slot", 0).map_err(m)?,
        0,
        PMJ_PROGRAM_SLOT_LIMIT,
    )? as u64;
    Ok(PmjFile {
        families,
        perp,
        tau_min_ns,
        tau_max_ns,
        p_min_1e6,
        e_1e6,
        persist,
        sigma_k_1e3,
        price_floor_1e6,
        min_tau_s,
        deposit_usd_1e6,
        max_order_usd_1e6,
        usd_market_1e6,
        session_loss_usd_1e6,
        max_orders_market,
        order_ttl_ns,
        max_slippage_bps,
        paper_delay_ms,
        live_assets,
        phi_lut,
        guard_programs,
        guard_alts,
        guard_deposit_account,
        guard_max_sol_lamports,
        guard_max_cu_price,
        guard_vault,
        guard_vault_alt,
        guard_cosigners,
        build_extra,
        strike_law,
        max_side_families,
        guard_deposit_mint,
        min_sol_lamports,
        guard_program_slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phi() -> String {
        let mut v: Vec<String> = Vec::with_capacity(PMJ_PHI_POINTS);
        let mut i = 0usize;
        while i < PMJ_PHI_POINTS {
            v.push(format!("{}", 500_000 + (i as u64 * 499_999 / 4_096)));
            i += 1;
        }
        format!("[{}]", v.join(", "))
    }

    fn src(extra: &str) -> String {
        format!(
            "[pmj]\nfamilies = [\"btc\", \"eth\"]\nperp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]\n\
             tau_min_ns = 2000000000\ntau_max_ns = 20000000000\np_min_1e6 = 900000\ne_1e6 = 20000\n\
             persist = 2\nsigma_k_1e3 = 1550\ndeposit_usd_1e6 = 5000000\nusd_market_1e6 = 10000000\n\
             session_loss_usd_1e6 = 50000000\n{extra}phi_lut = {}\n",
            phi()
        )
    }

    #[test]
    fn the_dust_frame_parses_with_its_stated_defaults() {
        let f = parse(&src("")).expect("valid");
        assert_eq!(f.families, vec!["btc", "eth"]);
        assert_eq!((f.price_floor_1e6, f.min_tau_s), (900_000, 3));
        assert_eq!((f.deposit_usd_1e6, f.max_order_usd_1e6, f.usd_market_1e6), (5_000_000, 5_010_000, 10_000_000));
        assert_eq!((f.max_orders_market, f.order_ttl_ns), (4, 5_000_000_000));
        assert_eq!((f.max_slippage_bps, f.paper_delay_ms), (0, 2_000));
        assert!(f.live_assets.is_empty());
        assert!(f.guard_programs.is_empty() && f.guard_alts.is_empty(), "refuse all by default");
        assert_eq!((f.guard_deposit_account.as_str(), f.guard_max_sol_lamports, f.guard_max_cu_price), ("", 0, 0));
        assert_eq!(
            (f.guard_deposit_mint.as_str(), f.min_sol_lamports, f.guard_program_slot),
            ("", 0, 0),
            "PMJ J16: no deposit mint (no default, R-PMJ-S46), no SOL floor, no deploy pin"
        );
        let g = parse(&src(
            "guard_programs = [\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\"]\n\
             guard_deposit_account = \"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v\"\n\
             guard_max_sol_lamports = 3000000\nguard_max_cu_price = 50000\nmax_order_usd_1e6 = 25000000\n\
             paper_delay_ms = 1500\nmin_tau_s = 4\nprice_floor_1e6 = 920000\nmax_slippage_bps = 50\n",
        ))
        .expect("valid");
        assert_eq!(g.guard_programs.len(), 1);
        let h = parse(&src("min_tau_s = 0\npaper_delay_ms = 1999\nmax_order_usd_1e6 = 5010000\n")).expect("valid");
        assert_eq!((h.min_tau_s, h.paper_delay_ms, h.max_order_usd_1e6), (0, 1_999, 5_010_000));
        assert_eq!((g.max_order_usd_1e6, g.paper_delay_ms, g.min_tau_s, g.price_floor_1e6), (25_000_000, 1_500, 4, 920_000));
    }

    #[test]
    fn every_bound_refuses() {
        let base = src("");
        for (from, to) in [
            ("deposit_usd_1e6 = 5000000", "deposit_usd_1e6 = 4999999"),
            ("usd_market_1e6 = 10000000", "usd_market_1e6 = 4000000"),
            ("tau_max_ns = 20000000000", "tau_max_ns = 2000000000"),
            ("p_min_1e6 = 900000", "p_min_1e6 = 400000"),
            ("persist = 2", "persist = 9"),
            ("session_loss_usd_1e6 = 50000000", "session_loss_usd_1e6 = 0"),
            ("families = [\"btc\", \"eth\"]", "families = [\"btc\", \"btc\"]"),
            ("perp = [\"hyperliquid:BTC\", \"hyperliquid:ETH\"]", "perp = [\"hyperliquid:BTC\"]"),
        ] {
            assert!(base.contains(from), "{from}");
            assert!(parse(&base.replacen(from, to, 1)).is_err(), "{to}");
        }
        for extra in [
            "price_floor_1e6 = 1000000\n",
            "min_tau_s = 20\n",
            "max_order_usd_1e6 = 4000000\n",
            "max_order_usd_1e6 = 5009999\n",
            "paper_delay_ms = 3000\n",
            "min_tau_s = 0\n",
            "order_ttl_ns = 100\n",
            "max_slippage_bps = 10001\n",
            "paper_delay_ms = 10000\n",
            "live_assets = [\"sol\"]\n",
            "guard_programs = [\"0OIl\"]\n",
            "guard_deposit_account = \"not-base58-0OIl-not-base58-0OIl-x\"\n",
            "cap_day_usd_1e6 = 1\n",
        ] {
            assert!(parse(&src(extra)).is_err(), "{extra}");
        }
    }

    /// A `pm15.toml` copied whole is not a `pmj.toml`: its section, its
    /// per-order size key (`usd_order_1e6`, not the deposit) and a missing
    /// deposit each refuse — the 5-minute artifact is derived, never aliased.
    #[test]
    fn a_pm15_artifact_is_not_a_pmj_one() {
        let base = src("");
        assert!(parse(&base.replacen("[pmj]", "[pm15]", 1)).is_err(), "the section");
        assert!(parse(&src("usd_order_1e6 = 250000000\n")).is_err(), "pm15's size key");
        assert!(parse(&base.replacen("deposit_usd_1e6 = 5000000\n", "", 1)).is_err(), "no deposit");
    }

    #[test]
    fn the_side_cap_parses_off_by_default_and_within_the_families() {
        assert_eq!(parse(&src("")).expect("valid").max_side_families, 0, "absent = off");
        assert_eq!(parse(&src("max_side_families = 0\n")).expect("valid").max_side_families, 0);
        assert_eq!(parse(&src("max_side_families = 1\n")).expect("valid").max_side_families, 1);
        assert_eq!(parse(&src("max_side_families = 2\n")).expect("valid").max_side_families, 2);
        for bad in ["max_side_families = 3\n", "max_side_families = -1\n", "max_side_families = \"2\"\n"] {
            assert!(parse(&src(bad)).is_err(), "{bad}: more than the two families, negative, or not an int");
        }
    }

    #[test]
    fn law_iii_parses_and_its_keys_need_it() {
        use crate::strike_law::{STRIKE_LAW_MEAN60, STRIKE_LAW_TRAP62};
        let f = parse(&src("")).expect("valid");
        assert_eq!(f.strike_law.law, STRIKE_LAW_MEAN60, "absent = the first law");
        let f = parse(&src("strike_law = \"mean60\"\n")).expect("valid");
        assert_eq!(f.strike_law, crate::strike_law::StrikeLaw::default());
        let f = parse(&src(
            "strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 5]\n",
        ))
        .expect("valid");
        assert_eq!(
            (f.strike_law.law, f.strike_law.sigma_res_ratio_1e3, f.strike_law.sigma_res_min_ppm.clone()),
            (STRIKE_LAW_TRAP62, 130, vec![3, 5])
        );
        for (bad, why) in [
            ("strike_law = \"trap60\"\n", "an unknown law"),
            ("strike_law = 1\n", "not a string"),
            ("strike_law = \"trap62\"\n", "no residual"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\n", "no floors"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 0\nsigma_res_min_ppm = [3, 5]\n", "ratio 0"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 1001\nsigma_res_min_ppm = [3, 5]\n", "ratio > 1000"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3]\n", "one floor for two families"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 0]\n", "a floor of 0"),
            ("strike_law = \"trap62\"\nsigma_res_ratio_1e3 = 130\nsigma_res_min_ppm = [3, 101]\n", "a floor over 100"),
            ("sigma_res_ratio_1e3 = 130\n", "law (iii)'s key under the first law"),
            ("strike_law = \"mean60\"\nsigma_res_min_ppm = [3, 5]\n", "law (iii)'s key under the first law"),
        ] {
            assert!(parse(&src(bad)).is_err(), "{why}: {bad}");
        }
    }

    // --- PMJ J16: the live wallet's keys

    #[test]
    fn the_live_wallet_keys_parse_within_their_bounds() {
        for (name, mint) in PMJ_DEPOSIT_MINTS {
            let f = parse(&src(&format!(
                "guard_deposit_mint = \"{mint}\"\nmin_sol_lamports = 50000000\nguard_program_slot = 462839696\n"
            )))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((f.guard_deposit_mint.as_str(), f.min_sol_lamports, f.guard_program_slot), (mint, 50_000_000, 462_839_696));
        }
        for bad in [
            // A base58 key that is not one of the two USD mints (SOL's wrapped mint).
            "guard_deposit_mint = \"So11111111111111111111111111111111111111112\"\n",
            "guard_deposit_mint = \"0OIl\"\n",
            "guard_deposit_mint = [\"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v\"]\n",
            "min_sol_lamports = -1\n",
            "min_sol_lamports = 10000000001\n",
            "guard_program_slot = -1\n",
            "guard_program_slot = 1000000000001\n",
        ] {
            assert!(parse(&src(bad)).is_err(), "{bad}");
        }
        // A co-signer may not be the deposit mint.
        let mint = PMJ_DEPOSIT_MINTS[0].1;
        let both = format!("guard_deposit_mint = \"{mint}\"\nguard_cosigners = [\"{mint}\"]\n");
        assert!(parse(&src(&both)).is_err());
    }

    // --- PMJ W5: the co-signer (dormant), the vault pins, the build extras

    const COSIGNER: &str = "AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf";
    const VAULT: &str = "2y9Ad2GD7gwiMkkMu4bBK5216Pv9YJsBkSHAGwN3rBuJ";
    const ALT: &str = "6awx2Pq4uxeHsBcmutEqErz79C4TUg4EQEiBX5T5BVfX";

    #[test]
    fn guard_cosigners_absent_means_none() {
        let f = parse(&src("")).expect("valid");
        assert!(f.guard_cosigners.is_empty(), "LAW PMJ-G: one signature");
        let f = parse(&src("guard_cosigners = []\n")).expect("valid");
        assert!(f.guard_cosigners.is_empty());
    }

    #[test]
    fn one_cosigner_parses() {
        let f = parse(&src(&format!("guard_cosigners = [\"{COSIGNER}\"]\n"))).expect("valid");
        assert_eq!(f.guard_cosigners, vec![COSIGNER.to_string()]);
    }

    #[test]
    fn two_cosigners_are_refused() {
        let two = format!("guard_cosigners = [\"{COSIGNER}\", \"{VAULT}\"]\n");
        assert!(parse(&src(&two)).is_err());
    }

    #[test]
    fn a_malformed_cosigner_is_refused() {
        for bad in ["guard_cosigners = [\"0OIl\"]\n", "guard_cosigners = \"AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf\"\n", "guard_cosigners = [1]\n"] {
            assert!(parse(&src(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_cosigner_equal_to_a_program_table_or_the_deposit_account_is_refused() {
        for extra in [
            format!("guard_programs = [\"{COSIGNER}\"]\nguard_cosigners = [\"{COSIGNER}\"]\n"),
            format!("guard_alts = [\"{COSIGNER}\"]\nguard_cosigners = [\"{COSIGNER}\"]\n"),
            format!("guard_deposit_account = \"{COSIGNER}\"\nguard_cosigners = [\"{COSIGNER}\"]\n"),
            format!("guard_vault = \"{COSIGNER}\"\nguard_cosigners = [\"{COSIGNER}\"]\n"),
            "guard_cosigners = [\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\"]\n".to_string(),
            "guard_cosigners = [\"ComputeBudget111111111111111111111111111111\"]\n".to_string(),
            "guard_cosigners = [\"ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL\"]\n".to_string(),
        ] {
            assert!(parse(&src(&extra)).is_err(), "{extra}");
        }
    }

    #[test]
    fn the_vault_and_its_table_entry_parse_and_bind_each_other() {
        let f = parse(&src("")).expect("valid");
        assert_eq!((f.guard_vault.as_str(), f.guard_vault_alt.as_str()), ("", ""), "absent: no vault, every order refused");
        let f = parse(&src(&format!("guard_vault = \"{VAULT}\"\n"))).expect("a static vault");
        assert_eq!(f.guard_vault, VAULT);
        let ok = format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}:1\"\n");
        let f = parse(&src(&ok)).expect("the table entry");
        assert_eq!(f.guard_vault_alt, format!("{ALT}:1"));
        for bad in [
            "guard_vault = \"0OIl\"\n".to_string(),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault_alt = \"{ALT}:1\"\n"),
            format!("guard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}:1\"\n"),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}\"\n"),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}:256\"\n"),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}:+1\"\n"),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"{ALT}:\"\n"),
            format!("guard_alts = [\"{ALT}\"]\nguard_vault = \"{VAULT}\"\nguard_vault_alt = \"0OIl:1\"\n"),
        ] {
            assert!(parse(&src(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn build_extras_parse_within_their_grammar() {
        let f = parse(&src("")).expect("valid");
        assert!(f.build_extra.is_empty(), "absent: today's body");
        let f = parse(&src(
            "build_extra = [\"orderType:limit\", \"limitPriceUsd:{limit_micro}\", \"limitPrice:{limit_usd}\", \"slip_bps:0.5\"]\n",
        ))
        .expect("valid");
        assert_eq!(f.build_extra.len(), 4);
        for bad in [
            "build_extra = [\"a:1\", \"b:1\", \"c:1\", \"d:1\", \"e:1\"]\n",
            "build_extra = [\"noColon\"]\n",
            "build_extra = [\"1a:x\"]\n",
            "build_extra = [\"a-b:x\"]\n",
            "build_extra = [\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:x\"]\n",
            "build_extra = [\"a:\"]\n",
            "build_extra = [\"a:x y\"]\n",
            "build_extra = [\"a:{limit}\"]\n",
            "build_extra = [\"a:vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv\"]\n",
            "build_extra = [\"depositAmount:1\"]\n",
            "build_extra = [\"ISBUY:false\"]\n",
            "build_extra = [\"a:1\", \"a:2\"]\n",
            "build_extra = \"a:1\"\n",
        ] {
            assert!(parse(&src(bad)).is_err(), "{bad}");
        }
    }
}
