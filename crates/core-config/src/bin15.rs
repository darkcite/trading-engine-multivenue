// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `bin15.toml` — the BIN15 member's artifact (spec §6.3).
//!
//! Integer-only TOML subset in the `vrp.rs` / `icdp.rs` style: one
//! `[bin15]` section, the shared single-section loop
//! ([`crate::icdp::parse_single_section`]) so every message an operator
//! sees is the one the other artifacts produce, and every bound checked
//! at parse rather than clamped at boot. The engine hashes the file
//! BYTES, exactly as `vrp.toml`, so the boot tell names the artifact
//! that is actually in force.
//!
//! Why the lookup tables live in the artifact and not in the code: they
//! are a FIT, and a fit is data the operator re-cuts without a rebuild.
//! `claude_worker.bin15_fit` writes them, `bin15.toml.example` carries
//! a generated set, and the bounds below are what stop a mis-cut table
//! from pricing.

use crate::icdp::{parse_single_section, Value};
use std::path::Path;

/// Points in the Φ table: `d = 0, 0.001, …, 4.096`. Mirrors
/// `strategy_bin15::price::PHI_POINTS`.
pub const PHI_POINTS: usize = 4097;

/// Points in each recalibration table. Mirrors
/// `strategy_bin15::price::RECAL_POINTS`.
pub const RECAL_POINTS: usize = 65;

/// The smallest `phi_lut[4096]` — the table's value at the clamp
/// `|d| = 4.096`, ×1e6 — the grammar admits.
///
/// The floor refuses a table that is not a CDF running out to
/// near-certainty: a truncated cut, a table on the wrong grid, a scale
/// slipped by an order. It is NOT a statement about the tail. A
/// fat-tailed map ends well short of the Gaussian's 999_979 — a
/// Student-t with a few degrees of freedom ends somewhere between 0.99
/// and 0.999 — and the old floor of 999_900, which only the Gaussian
/// clears, refused every one of them at boot. 0.98 at 4.1 σ-units is the
/// least any honest price map claims.
pub const PHI_LAST_MIN_1E6: u32 = 980_000;

/// Recalibration phases (early / mid / late).
pub const PHASES: usize = 3;

/// Hours in the optional hour-of-day table.
pub const HOURS: usize = 24;

/// Families one artifact may configure. Mirrors
/// `core_config::universe::HL_ROLLING_MAX`.
pub const BIN15_MAX_FAMILIES: usize = 8;

/// Distinct underlyings one artifact may configure.
pub const BIN15_MAX_UNDERLYINGS: usize = 4;

/// A `bin15.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bin15Error(pub String);

impl std::fmt::Display for Bin15Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bin15.toml: {}", self.0)
    }
}

impl std::error::Error for Bin15Error {}

fn err(msg: impl Into<String>) -> Bin15Error {
    Bin15Error(msg.into())
}

/// Every key the grammar accepts. An unknown key is a REFUSAL: a
/// typo'd `e_take_1e6` that silently took the default is a member
/// trading an edge nobody chose.
const BIN15_KEYS: [&str; 27] = [
    "families",
    "underlying",
    "tau_ns",
    "tail_refuse_ns",
    "requote_ttl_ns",
    "cap_instance_usd_1e6",
    "cap_day_usd_1e6",
    "phi_lut",
    "recal_early",
    "recal_mid",
    "recal_late",
    "hour_ln_off_1e9",
    // O4b defect fix: `scale_1e9` is read by `opt_int` below but was
    // absent from this list, so the ONE artifact the fitter actually
    // writes — which carries the stated `scale_1e9 = 980000000` — was
    // refused at the grammar before any bound could be checked. An
    // optional key still has to be a KNOWN key.
    "scale_1e9",
    // BIN15 P0 (F4): the underlying mark's shelf life. Optional; absent
    // = the STATED default 5 s, which is the parser's law for an
    // optional key — and the only honest default, because an artifact
    // written before this key existed was priced by a member with no
    // age gate at all and must not silently keep that behaviour.
    "mark_stale_ns",
    // BIN15 H90 (2026-10-13, rulings O-H90-1..8): the LATE-WINDOW entry.
    // Every key optional; absent `late_enabled` = 0 = the member exactly
    // as it was. `late_enabled = 1` requires `late_phi_lut`.
    "late_enabled",
    "late_tau_min_ns",
    "late_tau_max_ns",
    "late_p_min_1e6",
    "late_e_1e6",
    "late_persist_polls",
    "late_usd_1e6",
    "late_sigma_k_1e3",
    "late_phi_lut",
    // BIN15 fills (2026-10-23): absent = 0 = the law as it was.
    "late_refire_fresh",
    "late_limit_bound",
    "late_max_quote_age_ms",
    "late_lead",
];

/// BIN15 H90 G3 (2026-10-16, ruling O-GL-2): the keys of the retired arms
/// — the coverage entry, Arm A's takes and closes, the Arm B maker and its
/// null arm. An artifact that still states one is refused by NAME (not as
/// an unknown key), so the operator reads why: the arm it configured is
/// gone, and the line goes too. Vault doc 38 records the removed law.
const BIN15_RETIRED_KEYS: [&str; 13] = [
    "e_take_1e6",
    "h_quote_1e6",
    "tau_min_take_ns",
    "tau_min_quote_ns",
    "requote_thr_1e6",
    "clip_qty_1e6",
    "maker_enabled",
    "null_arm",
    "entry_usd_1e6",
    "e_entry_1e6",
    "entry_min_px_1e6",
    "entry_persist_polls",
    "entry_elapsed_max_ns",
];

/// `bin15.toml` as parsed. The strings stay descriptors: resolving them
/// against the universe is `bin15_boot`'s job, because only the cli
/// knows the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bin15File {
    /// Family keys, in the same order as `universe.toml`'s `rolling`.
    pub families: Vec<String>,
    /// Mark-source descriptor per distinct underlying.
    pub underlying: Vec<String>,
    /// The `core-vol` tenor of the 15 m families.
    pub tau_ns: u64,
    /// The last window before expiry in which nothing is taken, ns.
    pub tail_refuse_ns: u64,
    /// Order TTL, ns.
    pub requote_ttl_ns: u64,
    /// Notional cap per instance ×1e6.
    pub cap_instance_usd_1e6: i64,
    /// Notional cap per UTC day ×1e6.
    pub cap_day_usd_1e6: i64,
    /// `Φ(d) ×1e6`.
    pub phi_lut: Vec<u32>,
    /// `p' ×1e6` per phase (early, mid, late).
    pub recal: [Vec<u32>; PHASES],
    /// `ln σ̂` offset per UTC hour ×1e9; all zero when absent.
    pub hour_ln_off_1e9: [i64; HOURS],
    /// Variance-ratio scale on σ̂ ×1e9; `1e9` when absent.
    pub scale_1e9: i64,
    /// BIN15 P0 (F4): the oldest an underlying mark may be and still
    /// price a binary, ns. Absent = [`MARK_STALE_NS_DEFAULT`].
    pub mark_stale_ns: u64,
    /// BIN15 H90: `1` = the late-window entry is on. Absent = 0.
    pub late_enabled: u8,
    /// BIN15 H90: consecutive distinct snapshots of the favoured leg the
    /// late test must hold on, `[1, ENTRY_PERSIST_POLLS_MAX]`. Absent =
    /// [`LATE_PERSIST_POLLS_DEFAULT`].
    pub late_persist_polls: u8,
    /// BIN15 H90: the window's floor, ns to expiry. Absent =
    /// [`LATE_TAU_MIN_NS_DEFAULT`].
    pub late_tau_min_ns: u64,
    /// BIN15 H90: the window's ceiling, ns to expiry, at most
    /// [`LATE_TAU_MAX_NS_LIMIT`]. Absent = [`LATE_TAU_MAX_NS_DEFAULT`].
    pub late_tau_max_ns: u64,
    /// BIN15 H90: the least belief on the favoured side ×1e6. Absent =
    /// [`LATE_P_MIN_1E6_DEFAULT`].
    pub late_p_min_1e6: i64,
    /// BIN15 H90: the margin the ask must leave under that belief ×1e6.
    /// Absent = [`LATE_E_1E6_DEFAULT`].
    pub late_e_1e6: i64,
    /// BIN15 H90: the most one late entry spends ×1e6 USD — the whole
    /// resting ask up to this. Absent = [`LATE_USD_1E6_DEFAULT`].
    pub late_usd_1e6: i64,
    /// BIN15 H90: the late pricer's σ scale ×1e3. Absent =
    /// [`LATE_SIGMA_K_1E3_DEFAULT`].
    pub late_sigma_k_1e3: i64,
    /// BIN15 H90: `Φ(z) ×1e6` for `z = 0, 0.002, …, 8.192` — the late
    /// pricer's Gaussian table, cut by `claude_worker.bin15_fit late`.
    /// Empty when absent, which `late_enabled = 1` refuses.
    pub late_phi_lut: Vec<u32>,
    /// BIN15 fills B-1: `1` = after an unfilled late IoC, fire again only
    /// on another touch of the leg it bought on. Absent = 0.
    pub late_refire_fresh: u8,
    /// BIN15 fills B-3: `1` = the IoC's limit is the law's bound `p − e`
    /// on the grid, sweeping every level up to it. Absent = 0 (the touch).
    pub late_limit_bound: u8,
    /// BIN15 fills B-2: the oldest favoured-leg ask (since it moved to its
    /// price) the law fires on, ns, from `late_max_quote_age_ms` in
    /// `[0, LATE_MAX_QUOTE_AGE_MS_LIMIT]`.
    /// Absent = 0 = no limit.
    pub late_max_quote_age_ns: u64,
    /// BIN15 fills B-4 (ruling F-6): the lead book per underlying, in
    /// `underlying`'s order — a descriptor the engine subscribes to
    /// (`binance-usdm:btcusdt`). Its mid, through a 60 s basis onto the
    /// mark, replaces the HL perp in the late nowcast. Empty when absent:
    /// no lead, the law as it was.
    pub late_lead: Vec<String>,
}

/// Read and parse the artifact, returning it with its RAW BYTES so the
/// caller can hash exactly what it read.
pub fn load(path: &Path) -> Result<(Bin15File, Vec<u8>), Bin15Error> {
    let bytes = std::fs::read(path)
        .map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|e| err(format!("{}: not UTF-8 ({e})", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// The default artifact path: `~/multivenue/bin15.toml`.
pub fn default_bin15_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/bin15.toml")
}

/// The default seed directory: `~/multivenue/`.
pub fn default_seed_dir() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue")
}

pub(crate) fn ints(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<i64>, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Ints(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer array"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

pub(crate) fn strs(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<String>, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Strs(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string array"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

pub(crate) fn int(kv: &[(String, Value, usize)], key: &str) -> Result<i64, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

pub(crate) fn opt_int(kv: &[(String, Value, usize)], key: &str, default: i64) -> Result<i64, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Ok(default),
    }
}

/// BIN15 P0 (F4): the stated default shelf life of an underlying mark,
/// ns. Hyperliquid prints a mark every 1-3 s, so 5 s is two to five
/// missed prints — past ordinary jitter, well short of a dead feed.
pub const MARK_STALE_NS_DEFAULT: u64 = 5_000_000_000;

/// The floor `mark_stale_ns` may be configured at, ns: one mark
/// interval. Below it the gate fires between two healthy prints.
pub const MARK_STALE_NS_MIN: u64 = 1_000_000_000;

/// BIN15 S5: the most consecutive snapshots the persistence law may ask
/// for. The outcome legs' two-sided touch is the `l2Book` push, ~5.3 s
/// apart, so eight is ~40 s of an instance's 900: past it the entry is a
/// different (late) strategy, not a filter on this one.
pub const ENTRY_PERSIST_POLLS_MAX: u8 = 8;

/// BIN15 H90: points in the late Φ table, `z = 0, 0.002, …, 8.192`.
/// Mirrors `strategy_bin15::late::LATE_PHI_POINTS`.
pub const LATE_PHI_POINTS: usize = 4097;

/// BIN15 H90: the smallest `late_phi_lut[4096]` the grammar admits — the
/// table must run out to certainty within a ppm, or a decided instance's
/// 0.98 offer could never clear a 2 c margin.
pub const LATE_PHI_LAST_MIN_1E6: u32 = 999_999;

/// BIN15 H90 (ruling O-H90-7): the late window's stated floor, ns.
pub const LATE_TAU_MIN_NS_DEFAULT: u64 = 2_000_000_000;

/// BIN15 H90 (ruling O-H90-7): the late window's stated ceiling, ns.
pub const LATE_TAU_MAX_NS_DEFAULT: u64 = 30_000_000_000;

/// BIN15 H90: the late law prices inside the venue's settlement minute
/// and nowhere else; a ceiling past it is a different strategy.
pub const LATE_TAU_MAX_NS_LIMIT: u64 = 60_000_000_000;

/// BIN15 H90: the late window's least floor — one second, inside which
/// no order reaches the venue before the book clears.
pub const LATE_TAU_MIN_NS_LIMIT: u64 = 1_000_000_000;

/// BIN15 H90: the stated least belief, ×1e6 (0.90).
pub const LATE_P_MIN_1E6_DEFAULT: i64 = 900_000;

/// BIN15 H90: the stated margin, ×1e6 (2 c).
pub const LATE_E_1E6_DEFAULT: i64 = 20_000;

/// BIN15 H90: the stated persistence (ruling O-H90-5's F3 law).
pub const LATE_PERSIST_POLLS_DEFAULT: u8 = 2;

/// BIN15 H90 (ruling O-H90-6): the stated spend per late entry — the
/// whole resting ask up to $100.
pub const LATE_USD_1E6_DEFAULT: i64 = 100_000_000;

/// BIN15 H90: the stated σ scale ×1e3 (1.55, fitted in sample, vault doc 32).
pub const LATE_SIGMA_K_1E3_DEFAULT: i64 = 1_550;

/// BIN15 H90: the σ scale's bounds ×1e3 — a quarter to four times the
/// short-horizon RMS.
pub const LATE_SIGMA_K_1E3_RANGE: (i64, i64) = (250, 4_000);

/// BIN15 fills B-2: the largest stated `late_max_quote_age_ms` — the
/// late window's own ceiling ([`LATE_TAU_MAX_NS_LIMIT`]).
pub const LATE_MAX_QUOTE_AGE_MS_LIMIT: i64 = 60_000;

/// One optional 0/1 flag; absent = 0.
fn opt_flag(kv: &[(String, Value, usize)], key: &str) -> Result<u8, Bin15Error> {
    match opt_int(kv, key, 0)? {
        0 => Ok(0),
        1 => Ok(1),
        v => Err(err(format!("`{key}` must be 0 or 1 (got {v}); absent means 0"))),
    }
}

/// BIN15 H90: the late keys, parsed and bounded. Every STATED value is
/// bounded whether or not the law is on, so an artifact that turns it on
/// later cannot discover a bad value at that moment. One default depends
/// on the artifact: absent `late_usd_1e6` is the ruled $100 or the
/// artifact's own per-instance cap, whichever is smaller — an artifact
/// with no late key at all is today's member and must parse as it did.
fn late_keys(kv: &[(String, Value, usize)], file: &mut Bin15File) -> Result<(), Bin15Error> {
    file.late_enabled = opt_flag(kv, "late_enabled")?;
    let persist = opt_int(kv, "late_persist_polls", i64::from(LATE_PERSIST_POLLS_DEFAULT))?;
    file.late_persist_polls = match u8::try_from(persist) {
        Ok(p) if (1..=ENTRY_PERSIST_POLLS_MAX).contains(&p) => p,
        _ => {
            return Err(err(format!(
                "`late_persist_polls` must be in [1, {ENTRY_PERSIST_POLLS_MAX}] (got {persist})"
            )))
        }
    };
    file.late_tau_min_ns = opt_pos_u64(kv, "late_tau_min_ns", LATE_TAU_MIN_NS_DEFAULT)?;
    file.late_tau_max_ns = opt_pos_u64(kv, "late_tau_max_ns", LATE_TAU_MAX_NS_DEFAULT)?;
    if file.late_tau_min_ns < LATE_TAU_MIN_NS_LIMIT
        || file.late_tau_min_ns >= file.late_tau_max_ns
        || file.late_tau_max_ns > LATE_TAU_MAX_NS_LIMIT
    {
        return Err(err(format!(
            "`late_tau_min_ns` {} and `late_tau_max_ns` {} must satisfy \
             {LATE_TAU_MIN_NS_LIMIT} <= min < max <= {LATE_TAU_MAX_NS_LIMIT} — the late \
             law lives inside the venue's settlement minute",
            file.late_tau_min_ns, file.late_tau_max_ns
        )));
    }
    file.late_p_min_1e6 = opt_int(kv, "late_p_min_1e6", LATE_P_MIN_1E6_DEFAULT)?;
    if !(500_000..1_000_000).contains(&file.late_p_min_1e6) {
        return Err(err(format!(
            "`late_p_min_1e6` must be in [500000, 1000000) (got {}) — the favoured side \
             is believed at least even",
            file.late_p_min_1e6
        )));
    }
    file.late_e_1e6 = opt_int(kv, "late_e_1e6", LATE_E_1E6_DEFAULT)?;
    if !(100..500_000).contains(&file.late_e_1e6) {
        return Err(err(format!(
            "`late_e_1e6` must be in [100, 500000) (got {}) — one tick to half the interval",
            file.late_e_1e6
        )));
    }
    let usd_stated = kv.iter().any(|(k, _, _)| k == "late_usd_1e6");
    file.late_usd_1e6 = opt_int(
        kv,
        "late_usd_1e6",
        LATE_USD_1E6_DEFAULT.min(file.cap_instance_usd_1e6),
    )?;
    if (usd_stated || file.late_enabled == 1)
        && (file.late_usd_1e6 < 1_000_000 || file.late_usd_1e6 > file.cap_instance_usd_1e6)
    {
        return Err(err(format!(
            "`late_usd_1e6` {} must be in [1000000, cap_instance_usd_1e6 {}] — the venue's \
             $1 minimum to the per-instance cap",
            file.late_usd_1e6, file.cap_instance_usd_1e6
        )));
    }
    file.late_sigma_k_1e3 = opt_int(kv, "late_sigma_k_1e3", LATE_SIGMA_K_1E3_DEFAULT)?;
    let (klo, khi) = LATE_SIGMA_K_1E3_RANGE;
    if !(klo..=khi).contains(&file.late_sigma_k_1e3) {
        return Err(err(format!(
            "`late_sigma_k_1e3` must be in [{klo}, {khi}] (got {})",
            file.late_sigma_k_1e3
        )));
    }
    file.late_refire_fresh = opt_flag(kv, "late_refire_fresh")?;
    file.late_limit_bound = opt_flag(kv, "late_limit_bound")?;
    let age_ms = opt_int(kv, "late_max_quote_age_ms", 0)?;
    if !(0..=LATE_MAX_QUOTE_AGE_MS_LIMIT).contains(&age_ms) {
        return Err(err(format!(
            "`late_max_quote_age_ms` must be in [0, {LATE_MAX_QUOTE_AGE_MS_LIMIT}] (got {age_ms}) \
             — 0 is no limit"
        )));
    }
    file.late_max_quote_age_ns = age_ms as u64 * 1_000_000;
    if kv.iter().any(|(k, _, _)| k == "late_lead") {
        file.late_lead = strs(kv, "late_lead")?;
        if file.late_lead.len() != file.underlying.len()
            || file.late_lead.iter().any(String::is_empty)
        {
            return Err(err(format!(
                "`late_lead` must name one lead book per `underlying` entry, in its order \
                 (got {} for {})",
                file.late_lead.len(),
                file.underlying.len()
            )));
        }
    }
    if kv.iter().any(|(k, _, _)| k == "late_phi_lut") {
        file.late_phi_lut = table(
            kv,
            "late_phi_lut",
            LATE_PHI_POINTS,
            Some(500_000),
            Some(LATE_PHI_LAST_MIN_1E6),
            false,
        )?;
    }
    if file.late_enabled == 1 && file.late_phi_lut.is_empty() {
        return Err(err(
            "`late_enabled = 1` without `late_phi_lut` — the late pricer has no Φ. Cut \
             it with `python -m claude_worker.bin15_fit late`",
        ));
    }
    Ok(())
}

fn opt_pos_u64(
    kv: &[(String, Value, usize)],
    key: &str,
    default: u64,
) -> Result<u64, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => {
            u64::try_from(*v).map_err(|_| err(format!("`{key}` must be positive (got {v})")))
        }
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Ok(default),
    }
}

pub(crate) fn pos_u64(kv: &[(String, Value, usize)], key: &str) -> Result<u64, Bin15Error> {
    let v = int(kv, key)?;
    u64::try_from(v).map_err(|_| err(format!("`{key}` must be positive (got {v})")))
}

/// A monotone non-decreasing `u32` table of exactly `n` points, with
/// both ends pinned. Monotonicity is the property the pricer's linear
/// interpolation rests on: a table that dips makes `p̂` non-monotone in
/// the mark, and a non-monotone fair value is an arbitrage the member
/// would trade against itself.
pub(crate) fn table(
    kv: &[(String, Value, usize)],
    key: &str,
    n: usize,
    first: Option<u32>,
    last_min: Option<u32>,
    interior_open: bool,
) -> Result<Vec<u32>, Bin15Error> {
    let v = ints(kv, key)?;
    if v.len() != n {
        return Err(err(format!(
            "`{key}` must carry exactly {n} integers (got {})",
            v.len()
        )));
    }
    let mut out: Vec<u32> = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let x = v[i];
        if !(0..=1_000_000).contains(&x) {
            return Err(err(format!(
                "`{key}`[{i}] = {x} is outside 0..=1_000_000 — the tables are \
                 probabilities ×1e6"
            )));
        }
        if i > 0 && x < i64::from(out[i - 1]) {
            return Err(err(format!(
                "`{key}` is not monotone at [{i}]: {} then {x}. A dipping table \
                 makes the fair value non-monotone in the mark, which is an \
                 arbitrage against itself",
                out[i - 1]
            )));
        }
        out.push(x as u32);
        i += 1;
    }
    if let Some(f) = first {
        if out[0] != f {
            return Err(err(format!("`{key}`[0] must be {f} (got {})", out[0])));
        }
    }
    if let Some(m) = last_min {
        if out[n - 1] < m {
            return Err(err(format!(
                "`{key}`[{}] must be at least {m} (got {})",
                n - 1,
                out[n - 1]
            )));
        }
    }
    // BIN15 P1a (F1): a recalibration table may pin certainty ONLY at
    // the two endpoints, where the raw price itself said 0 or 1. An
    // INTERIOR bucket at 0 or 1e6 is a recalibration asserting a
    // certainty the model never held — which is exactly what the old
    // `clamp(.., 0, 1e6)` fitter produced for every slope over ~1.032,
    // and what the §0 entry rule then reads as an unbeatable edge over
    // any ask. This bound is a BOOT REFUSAL for such an artifact, and
    // that is the point: the live file has to be re-cut, not tolerated.
    if interior_open {
        let mut i = 1usize;
        while i < n - 1 {
            if out[i] == 0 || out[i] == 1_000_000 {
                return Err(err(format!(
                    "`{key}`[{i}] = {} pins CERTAINTY on an interior bucket. Only \
                     [0] and [{}] may be 0 or 1000000; an interior 0/1 is a \
                     recalibration manufacturing a belief the pricer never had. \
                     Re-cut the artifact (`python -m claude_worker.bin15_fit \
                     artifact`) — its slope bound is 1032 milli-units",
                    out[i],
                    n - 1
                )));
            }
            i += 1;
        }
    }
    Ok(out)
}

/// Refuse an artifact that still states a retired key, naming the key.
fn retired_keys(src: &str) -> Result<(), Bin15Error> {
    for (n, line) in src.lines().enumerate() {
        let t = line.trim_start();
        let key = t.split(|c: char| c == '=' || c.is_whitespace()).next().unwrap_or("");
        if BIN15_RETIRED_KEYS.contains(&key) && t[key.len()..].trim_start().starts_with('=') {
            return Err(err(format!(
                "line {}: `{key}` was retired with the old arms (BIN15 H90 G3, 2026-10-16) \
                 — the member trades the late-window law only; delete the line",
                n + 1
            )));
        }
    }
    Ok(())
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<Bin15File, Bin15Error> {
    retired_keys(src)?;
    let kv = parse_single_section(src, "bin15", &BIN15_KEYS)
        .map_err(|e| err(e.0))?;

    let families = strs(&kv, "families")?;
    let underlying = strs(&kv, "underlying")?;
    let mut hour = [0i64; HOURS];
    // The hour table is OPTIONAL and absent means ZERO — which is
    // bit-identical to a member with no hour-of-day correction, so an
    // operator who has not cut one is not silently running someone
    // else's.
    if kv.iter().any(|(k, _, _)| k == "hour_ln_off_1e9") {
        let v = ints(&kv, "hour_ln_off_1e9")?;
        if v.len() != HOURS {
            return Err(err(format!(
                "`hour_ln_off_1e9` must carry exactly {HOURS} integers (got {})",
                v.len()
            )));
        }
        let mut i = 0usize;
        while i < HOURS {
            hour[i] = v[i];
            i += 1;
        }
    }

    let mut file = Bin15File {
        families,
        underlying,
        tau_ns: pos_u64(&kv, "tau_ns")?,
        tail_refuse_ns: pos_u64(&kv, "tail_refuse_ns")?,
        requote_ttl_ns: pos_u64(&kv, "requote_ttl_ns")?,
        cap_instance_usd_1e6: int(&kv, "cap_instance_usd_1e6")?,
        cap_day_usd_1e6: int(&kv, "cap_day_usd_1e6")?,
        phi_lut: table(&kv, "phi_lut", PHI_POINTS, Some(500_000), Some(PHI_LAST_MIN_1E6), false)?,
        recal: [
            table(&kv, "recal_early", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
            table(&kv, "recal_mid", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
            table(&kv, "recal_late", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
        ],
        hour_ln_off_1e9: hour,
        scale_1e9: opt_int(&kv, "scale_1e9", 1_000_000_000)?,
        mark_stale_ns: opt_pos_u64(&kv, "mark_stale_ns", MARK_STALE_NS_DEFAULT)?,
        late_enabled: 0,
        late_persist_polls: LATE_PERSIST_POLLS_DEFAULT,
        late_tau_min_ns: LATE_TAU_MIN_NS_DEFAULT,
        late_tau_max_ns: LATE_TAU_MAX_NS_DEFAULT,
        late_p_min_1e6: LATE_P_MIN_1E6_DEFAULT,
        late_e_1e6: LATE_E_1E6_DEFAULT,
        late_usd_1e6: LATE_USD_1E6_DEFAULT,
        late_sigma_k_1e3: LATE_SIGMA_K_1E3_DEFAULT,
        late_phi_lut: Vec::new(),
        late_refire_fresh: 0,
        late_limit_bound: 0,
        late_max_quote_age_ns: 0,
        late_lead: Vec::new(),
    };

    if file.families.is_empty() || file.families.len() > BIN15_MAX_FAMILIES {
        return Err(err(format!(
            "`families` must carry 1..={BIN15_MAX_FAMILIES} keys (got {})",
            file.families.len()
        )));
    }
    if file.underlying.is_empty() || file.underlying.len() > BIN15_MAX_UNDERLYINGS {
        return Err(err(format!(
            "`underlying` must carry 1..={BIN15_MAX_UNDERLYINGS} descriptors (got {})",
            file.underlying.len()
        )));
    }
    if core_vol::tenor_of(file.tau_ns).is_none() {
        return Err(err(format!(
            "`tau_ns` {} is not a tenor `core-vol` forecasts. The BIN15 lane's \
             tenor is 900000000000 (15 m); the daily families price off the 8 h \
             tenor, which the member selects for itself",
            file.tau_ns
        )));
    }
    // One tick. An edge under the venue's own price granularity is an
    // edge the venue cannot express, so every touch would look wrong.
    if file.tail_refuse_ns >= file.tau_ns {
        return Err(err(format!(
            "`tail_refuse_ns` {} must be under `tau_ns` {} — otherwise the tail \
             swallows the whole instance and the pricer never prices",
            file.tail_refuse_ns, file.tau_ns
        )));
    }
    if file.cap_instance_usd_1e6 <= 0 || file.cap_day_usd_1e6 <= 0 {
        return Err(err(
            "`cap_instance_usd_1e6` and `cap_day_usd_1e6` must both be positive — a \
             zero cap is a member that cannot trade, spelled as if it could",
        ));
    }
    if file.cap_instance_usd_1e6 > file.cap_day_usd_1e6 {
        return Err(err(format!(
            "`cap_instance_usd_1e6` {} exceeds `cap_day_usd_1e6` {} — one instance \
             could spend the whole day's room",
            file.cap_instance_usd_1e6, file.cap_day_usd_1e6
        )));
    }
    // BIN15 P0 (F4). A gate under one mark interval holds on every
    // ordinary print gap and the member never prices; a gate at or over
    // the take floor cannot fire before `tau_min_take_ns` has already
    // closed the arm, which is a gate spelled as if it worked.
    if file.mark_stale_ns < MARK_STALE_NS_MIN {
        return Err(err(format!(
            "`mark_stale_ns` {} is under {MARK_STALE_NS_MIN} (one Hyperliquid mark \
             interval) — a gate that fires between two ordinary prints holds the \
             member on every tick",
            file.mark_stale_ns
        )));
    }
    if file.mark_stale_ns >= file.tau_ns {
        return Err(err(format!(
            "`mark_stale_ns` {} must be under `tau_ns` {} — a staleness gate wider \
             than the instance can never hold a price, so it is a gate that does \
             nothing and reads as if it did",
            file.mark_stale_ns, file.tau_ns
        )));
    }
    // BIN15 P3 (F6). One venue tick, because a margin under the price
    // granularity is a margin the venue cannot express; and under half
    // the interval, because a bound of 0.5 or more can never be cleared
    // by any ask on the preferred side (`belief` is at most 1e6 and the
    // ask is at least one tick).
    // Inside the price interval, floor included: 0 is "no floor" and a
    // floor at or over 1.0 could never be cleared by any ask, so the
    // entry arm would be silently off.
    // BIN15 S5. A ceiling at or past the tenor is later than any instance
    // lives, so it would never close one: a gate spelled as if it worked.
    if file.scale_1e9 <= 0 {
        return Err(err(format!(
            "`scale_1e9` must be positive (got {}); absent means 1000000000",
            file.scale_1e9
        )));
    }
    late_keys(&kv, &mut file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed example IS the grammar's contract, so it is
    /// compiled in and parsed. This is the test that catches a key the
    /// fitter emits and the key list does not know: `scale_1e9` was
    /// exactly that, and every bound-level test passed while the one
    /// real artifact was refused on line 44.
    const EXAMPLE: &str = include_str!("../../../bin15.toml.example");

    #[test]
    fn the_committed_example_parses_and_is_the_stated_fit() {
        let f = parse(EXAMPLE).expect("bin15.toml.example must parse");
        assert_eq!(f.families.len(), BIN15_MAX_FAMILIES, "ruling O-Q7: eight families");
        assert_eq!(f.families[0], "out:BTC:15m");
        assert_eq!(f.families[7], "native:HYPE:1d");
        assert_eq!(f.underlying.len(), BIN15_MAX_UNDERLYINGS);
        assert_eq!(f.tau_ns, 900_000_000_000, "the 15 m tenor");
        // The stated fit, spelled once here so a re-cut that changes it
        // is a deliberate edit to this assertion.
        assert_eq!(f.scale_1e9, 980_000_000, "the stated variance-ratio scale");
        assert_eq!(f.phi_lut[0], 500_000, "Phi(0) = 0.5 exactly");
        assert_eq!(f.phi_lut[1_000], 841_345, "Phi(1) = 0.841345");
        assert_eq!(f.phi_lut[1_960], 975_002, "Phi(1.96) = 0.975002");
        assert_eq!(f.phi_lut[PHI_POINTS - 1], 999_979, "Phi(4.096)");
        // The three recalibration slopes, read back off their own
        // tables: p = 0.75 under a slope s lands at 0.5 + 0.25 s.
        //
        // BIN15 P1a (F1): the shipped slopes are 1.000 — IDENTITY. The
        // fitted 1.104 / 1.165 / 1.219 are withdrawn; they were
        // measured on ~33 instances and their tables pinned interior
        // certainty, which the grammar now refuses outright. A re-fit
        // off the accumulated ledger edits these three numbers and
        // re-cuts the artifact.
        let at_075 = |ph: usize| f.recal[ph][48];
        assert_eq!(at_075(0), 750_000, "early slope 1.000 (identity)");
        assert_eq!(at_075(1), 750_000, "mid slope 1.000 (identity)");
        assert_eq!(at_075(2), 750_000, "late slope 1.000 (identity)");
        // And no interior bucket is certain, on any phase — the bound
        // the parser enforces, read off the file the operator ships.
        let mut ph = 0usize;
        while ph < PHASES {
            let mut k = 1usize;
            while k < RECAL_POINTS - 1 {
                assert!(
                    f.recal[ph][k] > 0 && f.recal[ph][k] < 1_000_000,
                    "recal[{ph}][{k}] = {} pins certainty",
                    f.recal[ph][k]
                );
                k += 1;
            }
            ph += 1;
        }
        // Antisymmetry about (0.5, 0.5): Yes and No price to one.
        let mut ph = 0usize;
        while ph < PHASES {
            let mut k = 0usize;
            while k < RECAL_POINTS {
                assert_eq!(
                    i64::from(f.recal[ph][k]) + i64::from(f.recal[ph][RECAL_POINTS - 1 - k]),
                    1_000_000,
                    "recal[{ph}] is not antisymmetric at [{k}]"
                );
                k += 1;
            }
            ph += 1;
        }
        // The hour table is ABSENT in the example, on purpose.
        assert_eq!(f.hour_ln_off_1e9, [0i64; HOURS]);
    }

    /// A minimal artifact whose tables are the shipped SHAPE (a real
    /// CDF ramp and an identity recalibration), built here so the test
    /// does not depend on the example file.
    ///
    /// BIN15 P1a (F1): `recal` mirrors the fitter's law — endpoints
    /// pinned, every interior bucket held one venue tick inside them —
    /// because that law is now a PARSER bound and a helper built the
    /// old way produces an artifact the grammar refuses. Which is the
    /// point: [`an_artifact_that_pins_interior_certainty_is_refused`]
    /// builds one deliberately.
    fn artifact() -> String {
        let mut phi: Vec<String> = Vec::with_capacity(PHI_POINTS);
        let mut i = 0usize;
        while i < PHI_POINTS {
            // A monotone ramp 0.5 → 1.0 over the table's span.
            let v = 500_000 + (i as i64 * 499_950) / (PHI_POINTS as i64 - 1);
            phi.push(v.to_string());
            i += 1;
        }
        let recal = |slope: i64| recal_with(slope, true);
        format!(
            "[bin15]\n\
             families = [\"out:BTC:15m\"]\n\
             underlying = [\"hyperliquid:BTC\"]\n\
             tau_ns = 900000000000\n\
             tail_refuse_ns = 10000000000\n\
             requote_ttl_ns = 1000000000\n\
             cap_instance_usd_1e6 = 1000000000\n\
             cap_day_usd_1e6 = 5000000000\n\
             phi_lut = [{}]\n\
             recal_early = [{}]\n\
             recal_mid = [{}]\n\
             recal_late = [{}]\n",
            phi.join(", "),
            recal(1_000),
            recal(1_000),
            recal(1_000),
        )
    }

    /// One recalibration table as text. `interior_open` false is the
    /// WITHDRAWN law — a flat `clamp(.., 0, 1e6)` that pins interior
    /// buckets at certainty for any slope over ~1.032.
    fn recal_with(slope: i64, interior_open: bool) -> String {
        let mut t: Vec<String> = Vec::with_capacity(RECAL_POINTS);
        let mut k = 0usize;
        while k < RECAL_POINTS {
            let p = k as i64 * 15_625;
            let raw = 500_000 + (p - 500_000) * slope / 1_000;
            let v = if !interior_open {
                raw.clamp(0, 1_000_000)
            } else if k == 0 {
                0
            } else if k == RECAL_POINTS - 1 {
                1_000_000
            } else {
                raw.clamp(100, 999_900)
            };
            t.push(v.to_string());
            k += 1;
        }
        t.join(", ")
    }

    /// DISTX (2026-09-23). A FAT-TAILED `phi_lut` is a belief, not a
    /// defect: a Student-t map ends short of the Gaussian's 999_979 at
    /// the clamp. The floor is [`PHI_LAST_MIN_1E6`]; the table
    /// that sits ON it parses, the one a unit under it is refused with
    /// the grammar's own words.
    #[test]
    fn a_fat_tailed_phi_table_parses_and_a_truncated_one_is_refused() {
        let with_last = |last: i64| {
            let mut phi: Vec<String> = Vec::with_capacity(PHI_POINTS);
            let mut i = 0usize;
            while i < PHI_POINTS {
                let v = 500_000 + (i as i64 * (last - 500_000)) / (PHI_POINTS as i64 - 1);
                phi.push(v.to_string());
                i += 1;
            }
            let src = artifact();
            let start = src.find("phi_lut = [").expect("the phi line");
            let end = start + src[start..].find('\n').expect("its end");
            format!("{}phi_lut = [{}]{}", &src[..start], phi.join(", "), &src[end..])
        };
        let f = parse(&with_last(990_000)).expect("a t-shaped end parses");
        assert_eq!(f.phi_lut[PHI_POINTS - 1], 990_000);
        let floor = i64::from(PHI_LAST_MIN_1E6);
        assert!(parse(&with_last(floor)).is_ok(), "the floor itself parses");
        let e = parse(&with_last(floor - 1)).expect_err("one unit under the floor");
        assert!(e.0.contains("must be at least 980000"), "{}", e.0);
    }

    /// BIN15 P1a (F1). The artifact that was LIVE on 2026-09-12 carried
    /// slope-1.104 / 1.165 / 1.219 tables whose end buckets were
    /// clamped flat at 0 and 1e6, so the member published `p̂ = 1.000000`
    /// off a raw price of 0.984 — a certainty the pricer never held, and
    /// one that beats every ask the venue can quote. The grammar now
    /// refuses it, which means that artifact REFUSES THE BOOT until it
    /// is re-cut. That refusal is the fix, not a side effect of it.
    #[test]
    fn an_artifact_that_pins_interior_certainty_is_refused() {
        let good = artifact();
        assert!(parse(&good).is_ok(), "the identity artifact parses");
        let bad = good.replace(
            &format!("recal_early = [{}]", recal_with(1_000, true)),
            &format!("recal_early = [{}]", recal_with(1_104, false)),
        );
        assert_ne!(bad, good, "the substitution must have happened");
        let e = parse(&bad).expect_err("the withdrawn shape must be refused");
        assert!(e.0.contains("CERTAINTY"), "{}", e.0);
        // And the reason is INTERIOR, not the endpoints: the accepted
        // table IS 0 at [0] and 1e6 at [64] — pinned ends are the law,
        // pinned interiors are the defect.
        let f = parse(&good).expect("parse");
        assert_eq!(f.recal[2][0], 0);
        assert_eq!(f.recal[2][RECAL_POINTS - 1], 1_000_000);
    }

    /// The slope the fitter may still ship — 1032 milli-units is the
    /// largest whose bucket-63 value stays under 1e6 — parses, and the
    /// one above it cannot even be built by the fitter (that bound is
    /// tested on the Python side). Here we prove the PARSER accepts a
    /// sharpened-but-honest table, so the bound above is not a blanket
    /// ban on recalibration.
    #[test]
    fn a_sharpened_table_that_stays_inside_the_interval_parses() {
        let good = artifact().replace(
            &format!("recal_mid = [{}]", recal_with(1_000, true)),
            &format!("recal_mid = [{}]", recal_with(1_032, true)),
        );
        let f = parse(&good).expect("a 1.032 table is honest");
        assert_eq!(f.recal[1][0], 0);
        assert_eq!(f.recal[1][RECAL_POINTS - 1], 1_000_000);
        assert!(f.recal[1][63] < 1_000_000 && f.recal[1][63] > f.recal[1][62]);
    }

    #[test]
    fn a_well_formed_artifact_parses_with_the_documented_defaults() {
        let f = parse(&artifact()).expect("parse");
        assert_eq!(f.families, vec!["out:BTC:15m".to_owned()]);
        assert_eq!(f.underlying, vec!["hyperliquid:BTC".to_owned()]);
        assert_eq!(f.tau_ns, 900_000_000_000);
        assert_eq!(f.phi_lut.len(), PHI_POINTS);
        assert_eq!(f.phi_lut[0], 500_000, "Φ(0) = 0.5");
        assert_eq!(f.recal[0].len(), RECAL_POINTS);
        assert_eq!(f.recal[0][0], 0);
        assert_eq!(f.recal[2][RECAL_POINTS - 1], 1_000_000);
        // ABSENT OPTIONALS ARE THE STATED DEFAULT, bit for bit.
        assert_eq!(f.hour_ln_off_1e9, [0i64; HOURS], "no hour table = no offset");
        assert_eq!(f.scale_1e9, 1_000_000_000, "no scale = no scaling");
    }

    /// O4b: the ONE artifact `claude_worker.bin15_fit` writes carries
    /// `scale_1e9`, and the key list refused it. An optional key that
    /// is not a KNOWN key is a refusal, so "absent means the default"
    /// never got a chance to apply — the boot failed at the grammar.
    #[test]
    fn the_stated_scale_is_a_known_key_and_round_trips() {
        let src = format!("{}scale_1e9 = 980000000\n", artifact());
        let f = parse(&src).expect("scale_1e9 is a known key");
        assert_eq!(f.scale_1e9, 980_000_000);
        // And it is still BOUNDED, not merely accepted.
        let bad = format!("{}scale_1e9 = 0\n", artifact());
        let e = parse(&bad).expect_err("a zero scale");
        assert!(e.0.contains("must be positive"), "{}", e.0);
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let src = artifact().replace("tail_refuse_ns =", "tail_refuse_s =");
        let e = parse(&src).expect_err("unknown key");
        assert!(e.0.contains("tail_refuse_s"), "{}", e.0);
    }

    /// BIN15 H90 G3: a key of a retired arm is refused BY NAME, with the
    /// line, so an old artifact says why it no longer boots.
    #[test]
    fn a_retired_arms_key_is_refused_by_name() {
        let mut i = 0usize;
        while i < BIN15_RETIRED_KEYS.len() {
            let key = BIN15_RETIRED_KEYS[i];
            let src = artifact().replace("[bin15]\n", &format!("[bin15]\n{key} = 1\n"));
            let e = parse(&src).expect_err("retired");
            assert!(e.0.contains(key) && e.0.contains("retired") && e.0.contains("line 2"), "{}", e.0);
            i += 1;
        }
        // A comment that names one is not a key.
        let src = artifact().replace("[bin15]\n", "[bin15]\n# e_take_1e6 = 30000 (retired)\n");
        assert!(parse(&src).is_ok());
    }

    #[test]
    fn a_non_monotone_table_is_refused() {
        let good = artifact();
        // Make Φ dip at index 10.
        let mut phi: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < PHI_POINTS {
            let v = if i == 10 {
                500_000
            } else {
                500_000 + (i as i64 * 499_950) / (PHI_POINTS as i64 - 1)
            };
            phi.push(v.to_string());
            i += 1;
        }
        let start = good.find("phi_lut = [").expect("phi");
        let end = good[start..].find("]\n").expect("close") + start;
        let src = format!(
            "{}phi_lut = [{}{}",
            &good[..start],
            phi.join(", "),
            &good[end..]
        );
        let e = parse(&src).expect_err("non-monotone");
        assert!(e.0.contains("not monotone"), "{}", e.0);
    }

    #[test]
    fn a_wrong_length_table_is_refused() {
        let src = artifact().replace("recal_mid = [0, ", "recal_mid = [0, 0, ");
        let e = parse(&src).expect_err("length");
        assert!(e.0.contains("exactly 65"), "{}", e.0);
    }

    #[test]
    fn every_numeric_bound_is_enforced() {
        let cases: [(&str, &str, &str); 4] = [
            ("tau_ns = 900000000000", "tau_ns = 1800000000000", "not a tenor"),
            (
                "tail_refuse_ns = 10000000000",
                "tail_refuse_ns = 900000000000",
                "swallows the whole instance",
            ),
            ("cap_day_usd_1e6 = 5000000000", "cap_day_usd_1e6 = 0", "must both be positive"),
            (
                "cap_instance_usd_1e6 = 1000000000",
                "cap_instance_usd_1e6 = 9000000000",
                "could spend the whole day",
            ),
        ];
        let mut i = 0usize;
        while i < cases.len() {
            let (from, to, want) = cases[i];
            let src = artifact().replace(from, to);
            let e = match parse(&src) {
                Err(e) => e,
                Ok(_) => panic!("{to} must be refused"),
            };
            assert!(e.0.contains(want), "{to}: got {}", e.0);
            i += 1;
        }
    }

    #[test]
    fn the_hour_table_must_be_all_24_hours_or_absent() {
        let src = format!("{}hour_ln_off_1e9 = [0, 0, 0]\n", artifact());
        let e = parse(&src).expect_err("hours");
        assert!(e.0.contains("exactly 24"), "{}", e.0);
        let mut hours: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < HOURS {
            hours.push((i as i64 * 1_000_000).to_string());
            i += 1;
        }
        let src = format!("{}hour_ln_off_1e9 = [{}]\n", artifact(), hours.join(", "));
        let f = parse(&src).expect("24 hours");
        assert_eq!(f.hour_ln_off_1e9[23], 23_000_000);
    }

    /// BIN15 H90: a late Φ table of the shipped SHAPE — 0.5 at z = 0,
    /// monotone, certainty at the end.
    fn late_lut_line() -> String {
        let mut v: Vec<String> = Vec::with_capacity(LATE_PHI_POINTS);
        let mut i = 0usize;
        while i < LATE_PHI_POINTS {
            let x = 500_000 + (i as i64 * 500_000) / (LATE_PHI_POINTS as i64 - 1);
            v.push(x.to_string());
            i += 1;
        }
        format!("late_phi_lut = [{}]\n", v.join(", "))
    }

    /// BIN15 H90: every late key is optional and absent is the member as it
    /// was — the law off, the ruled values stated, no table.
    #[test]
    fn the_late_keys_are_absent_by_default_and_off() {
        let f = parse(&artifact()).expect("parse");
        assert_eq!(f.late_enabled, 0, "absent = off, bit for bit");
        assert_eq!(f.late_tau_min_ns, LATE_TAU_MIN_NS_DEFAULT);
        assert_eq!(f.late_tau_max_ns, LATE_TAU_MAX_NS_DEFAULT);
        assert_eq!(f.late_p_min_1e6, 900_000);
        assert_eq!(f.late_e_1e6, 20_000);
        assert_eq!(f.late_persist_polls, 2);
        assert_eq!(f.late_usd_1e6, 100_000_000);
        assert_eq!(f.late_sigma_k_1e3, 1_550);
        assert!(f.late_phi_lut.is_empty());
        assert_eq!((f.late_refire_fresh, f.late_limit_bound), (0, 0), "the fills keys");
        assert_eq!(f.late_max_quote_age_ns, 0);
        assert!(f.late_lead.is_empty(), "no lead");
        // The committed example carries none of them, so it is off too.
        let ex = parse(EXAMPLE).expect("example");
        assert_eq!(ex.late_enabled, 0);
    }

    /// BIN15 H90: switched on with its table, every key round-trips.
    #[test]
    fn the_late_law_round_trips_when_switched_on_with_its_table() {
        let src = format!(
            "{}late_enabled = 1\nlate_tau_min_ns = 8000000000\nlate_tau_max_ns = 30000000000\n\
             late_p_min_1e6 = 920000\nlate_e_1e6 = 10000\nlate_persist_polls = 3\n\
             late_usd_1e6 = 50000000\nlate_sigma_k_1e3 = 1250\n{}",
            artifact(),
            late_lut_line()
        );
        let f = parse(&src).expect("the late law parses");
        assert_eq!(f.late_enabled, 1);
        assert_eq!((f.late_tau_min_ns, f.late_tau_max_ns), (8_000_000_000, 30_000_000_000));
        assert_eq!((f.late_p_min_1e6, f.late_e_1e6), (920_000, 10_000));
        assert_eq!(f.late_persist_polls, 3);
        assert_eq!(f.late_usd_1e6, 50_000_000);
        assert_eq!(f.late_sigma_k_1e3, 1_250);
        assert_eq!(f.late_phi_lut.len(), LATE_PHI_POINTS);
        assert_eq!(f.late_phi_lut[LATE_PHI_POINTS - 1], 1_000_000);
    }

    /// BIN15 fills (2026-10-23): the re-fire, bound and age keys round-trip
    /// and are bounded.
    #[test]
    fn the_late_fills_keys_round_trip_and_are_bounded() {
        let src = format!(
            "{}late_refire_fresh = 1\nlate_limit_bound = 1\nlate_max_quote_age_ms = 1500\n",
            artifact()
        );
        let f = parse(&src).expect("the fills keys parse");
        assert_eq!((f.late_refire_fresh, f.late_limit_bound), (1, 1));
        assert_eq!(f.late_max_quote_age_ns, 1_500_000_000);
        let n = f.underlying.len();
        let leads = vec!["\"binance-usdm:btcusdt\""; n].join(", ");
        let g = parse(&format!("{}late_lead = [{leads}]\n", artifact())).expect("a lead per underlying");
        assert_eq!(g.late_lead.len(), n);
        let empty = vec!["\"\""; n].join(", ");
        let e = parse(&format!("{}late_lead = [{empty}]\n", artifact())).expect_err("an empty lead");
        assert!(e.0.contains("one lead book per"), "{}", e.0);
        let more = vec!["\"binance-usdm:btcusdt\""; n + 1].join(", ");
        let e = parse(&format!("{}late_lead = [{more}]\n", artifact())).expect_err("a lead too many");
        assert!(e.0.contains("one lead book per"), "{}", e.0);
        for (line, want) in [
            ("late_refire_fresh = 2", "0 or 1"),
            ("late_limit_bound = -1", "0 or 1"),
            ("late_max_quote_age_ms = -1", "[0, 60000]"),
            ("late_max_quote_age_ms = 60001", "[0, 60000]"),
        ] {
            let e = parse(&format!("{}{line}\n", artifact())).expect_err(line);
            assert!(e.0.contains(want), "{line}: {}", e.0);
        }
    }

    /// BIN15 H90: on without a table is a refusal, and every bound holds
    /// whether or not the law is on.
    #[test]
    fn the_late_law_refuses_a_missing_table_and_every_bound() {
        let e = parse(&format!("{}late_enabled = 1\n", artifact())).expect_err("no table");
        assert!(e.0.contains("without `late_phi_lut`"), "{}", e.0);
        let bad: [(&str, &str); 14] = [
            ("late_enabled = 2", "0 or 1"),
            ("late_tau_min_ns = 999999999", "must satisfy"),
            ("late_tau_min_ns = 30000000000", "must satisfy"),
            ("late_tau_max_ns = 60000000001", "must satisfy"),
            ("late_tau_max_ns = 2000000000", "must satisfy"),
            ("late_p_min_1e6 = 499999", "[500000, 1000000)"),
            ("late_p_min_1e6 = 1000000", "[500000, 1000000)"),
            ("late_e_1e6 = 99", "[100, 500000)"),
            ("late_e_1e6 = 500000", "[100, 500000)"),
            ("late_persist_polls = 0", "[1, 8]"),
            ("late_persist_polls = 9", "[1, 8]"),
            ("late_usd_1e6 = 999999", "cap_instance_usd_1e6"),
            ("late_usd_1e6 = 1000000001", "cap_instance_usd_1e6"),
            ("late_sigma_k_1e3 = 249", "[250, 4000]"),
        ];
        let mut i = 0usize;
        while i < bad.len() {
            let (line, want) = bad[i];
            let e = parse(&format!("{}{line}\n", artifact())).expect_err(line);
            assert!(e.0.contains(want), "{line}: got {}", e.0);
            i += 1;
        }
        // A table that stops short of certainty is refused.
        let short = late_lut_line().replace("1000000]", "999998]");
        let e = parse(&format!("{}{short}", artifact())).expect_err("short table");
        assert!(e.0.contains("late_phi_lut"), "{}", e.0);
    }

    /// BIN15 H90: an artifact whose per-instance cap is under the ruled $100
    /// and that names no late key is today's member — it parses, the absent
    /// stake clamped to the cap. A STATED stake over the cap is refused.
    #[test]
    fn a_cap_under_the_ruled_stake_parses_with_the_law_absent() {
        let small = artifact().replace(
            "cap_instance_usd_1e6 = 1000000000",
            "cap_instance_usd_1e6 = 40000000",
        );
        let f = parse(&small).expect("a $40 cap with no late key parses as it did");
        assert_eq!((f.late_enabled, f.late_usd_1e6), (0, 40_000_000));
        let e = parse(&format!("{small}late_usd_1e6 = 100000000\n")).expect_err("over the cap");
        assert!(e.0.contains("cap_instance_usd_1e6 40000000"), "{}", e.0);
    }
}
