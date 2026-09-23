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
const BIN15_KEYS: [&str; 25] = [
    "families",
    "underlying",
    "tau_ns",
    "e_take_1e6",
    "h_quote_1e6",
    "tau_min_take_ns",
    "tau_min_quote_ns",
    "tail_refuse_ns",
    "requote_ttl_ns",
    "requote_thr_1e6",
    "clip_qty_1e6",
    "cap_instance_usd_1e6",
    "cap_day_usd_1e6",
    "maker_enabled",
    "null_arm",
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
    // BIN15 O9: the coverage-entry notional. Optional; absent = 0 =
    // the edge law alone, which is what every artifact before
    // 2026-09-13 carries.
    "entry_usd_1e6",
    // BIN15 P0 (F4): the underlying mark's shelf life. Optional; absent
    // = the STATED default 5 s, which is the parser's law for an
    // optional key — and the only honest default, because an artifact
    // written before this key existed was priced by a member with no
    // age gate at all and must not silently keep that behaviour.
    "mark_stale_ns",
    // BIN15 P3 (F6): the coverage entry's price bound. Optional; absent
    // = the STATED default 20000 (2 c). An artifact written before this
    // key existed drove an arm that paid ANY ask, so the default has to
    // be the bound and not "off".
    "e_entry_1e6",
    // BIN15 R0 (2026-09-19): the coverage entry's price FLOOR. Optional;
    // absent = 0 = no floor, which is the 2026-09-13 law bit for bit.
    "entry_min_px_1e6",
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
    /// Arm A edge ×1e6.
    pub e_take_1e6: i64,
    /// Arm B half-spread ×1e6.
    pub h_quote_1e6: i64,
    /// τ floor for a take, ns.
    pub tau_min_take_ns: u64,
    /// τ floor for a quote, ns.
    pub tau_min_quote_ns: u64,
    /// The last window before expiry in which nothing is taken, ns.
    pub tail_refuse_ns: u64,
    /// Order TTL, ns.
    pub requote_ttl_ns: u64,
    /// Arm B re-quote threshold ×1e6.
    pub requote_thr_1e6: i64,
    /// Per-order clip ×1e6 contracts.
    pub clip_qty_1e6: i64,
    /// Notional cap per instance ×1e6.
    pub cap_instance_usd_1e6: i64,
    /// Notional cap per UTC day ×1e6.
    pub cap_day_usd_1e6: i64,
    /// `1` = Arm B on.
    pub maker_enabled: u8,
    /// `1` = alternate model / null arm by instance parity.
    pub null_arm: u8,
    /// `Φ(d) ×1e6`.
    pub phi_lut: Vec<u32>,
    /// `p' ×1e6` per phase (early, mid, late).
    pub recal: [Vec<u32>; PHASES],
    /// `ln σ̂` offset per UTC hour ×1e9; all zero when absent.
    pub hour_ln_off_1e9: [i64; HOURS],
    /// Variance-ratio scale on σ̂ ×1e9; `1e9` when absent.
    pub scale_1e9: i64,
    /// BIN15 O9: coverage-entry notional ×1e6 USD; `0` = off.
    pub entry_usd_1e6: i64,
    /// BIN15 P0 (F4): the oldest an underlying mark may be and still
    /// price a binary, ns. Absent = [`MARK_STALE_NS_DEFAULT`].
    pub mark_stale_ns: u64,
    /// BIN15 P3 (F6): the coverage entry's margin over its own belief
    /// ×1e6. Absent = [`E_ENTRY_1E6_DEFAULT`].
    pub e_entry_1e6: i64,
    /// BIN15 R0 (2026-09-19): the lowest preferred-side ask the coverage
    /// entry will pay ×1e6. Absent = 0 = no floor.
    pub entry_min_px_1e6: i64,
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

fn ints(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<i64>, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Ints(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer array"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

fn strs(kv: &[(String, Value, usize)], key: &str) -> Result<Vec<String>, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Strs(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string array"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

fn int(kv: &[(String, Value, usize)], key: &str) -> Result<i64, Bin15Error> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("missing `{key}`"))),
    }
}

fn opt_int(kv: &[(String, Value, usize)], key: &str, default: i64) -> Result<i64, Bin15Error> {
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

/// BIN15 P3 (F6): the stated margin the coverage entry demands over its
/// own belief before paying an ask ×1e6 — 2 cents.
///
/// The entry's bar is `p > a` (buying at ask `a` with belief `p` has an
/// EV per dollar of premium of `p/a − 1`). This is the cushion on top:
/// model error plus the exit-leg fee at the operator tier.
pub const E_ENTRY_1E6_DEFAULT: i64 = 20_000;

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

fn pos_u64(kv: &[(String, Value, usize)], key: &str) -> Result<u64, Bin15Error> {
    let v = int(kv, key)?;
    u64::try_from(v).map_err(|_| err(format!("`{key}` must be positive (got {v})")))
}

/// One 0/1 flag. A value that is neither is a refusal: "2" in a boolean
/// field means the operator believed something the grammar does not say.
fn flag(kv: &[(String, Value, usize)], key: &str) -> Result<u8, Bin15Error> {
    match int(kv, key)? {
        0 => Ok(0),
        1 => Ok(1),
        v => Err(err(format!("`{key}` must be 0 or 1 (got {v})"))),
    }
}

/// A monotone non-decreasing `u32` table of exactly `n` points, with
/// both ends pinned. Monotonicity is the property the pricer's linear
/// interpolation rests on: a table that dips makes `p̂` non-monotone in
/// the mark, and a non-monotone fair value is an arbitrage the member
/// would trade against itself.
fn table(
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

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<Bin15File, Bin15Error> {
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

    let file = Bin15File {
        families,
        underlying,
        tau_ns: pos_u64(&kv, "tau_ns")?,
        e_take_1e6: int(&kv, "e_take_1e6")?,
        h_quote_1e6: int(&kv, "h_quote_1e6")?,
        tau_min_take_ns: pos_u64(&kv, "tau_min_take_ns")?,
        tau_min_quote_ns: pos_u64(&kv, "tau_min_quote_ns")?,
        tail_refuse_ns: pos_u64(&kv, "tail_refuse_ns")?,
        requote_ttl_ns: pos_u64(&kv, "requote_ttl_ns")?,
        requote_thr_1e6: int(&kv, "requote_thr_1e6")?,
        clip_qty_1e6: int(&kv, "clip_qty_1e6")?,
        cap_instance_usd_1e6: int(&kv, "cap_instance_usd_1e6")?,
        cap_day_usd_1e6: int(&kv, "cap_day_usd_1e6")?,
        maker_enabled: flag(&kv, "maker_enabled")?,
        null_arm: flag(&kv, "null_arm")?,
        phi_lut: table(&kv, "phi_lut", PHI_POINTS, Some(500_000), Some(PHI_LAST_MIN_1E6), false)?,
        recal: [
            table(&kv, "recal_early", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
            table(&kv, "recal_mid", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
            table(&kv, "recal_late", RECAL_POINTS, Some(0), Some(1_000_000), true)?,
        ],
        hour_ln_off_1e9: hour,
        scale_1e9: opt_int(&kv, "scale_1e9", 1_000_000_000)?,
        entry_usd_1e6: opt_int(&kv, "entry_usd_1e6", 0)?,
        mark_stale_ns: opt_pos_u64(&kv, "mark_stale_ns", MARK_STALE_NS_DEFAULT)?,
        e_entry_1e6: opt_int(&kv, "e_entry_1e6", E_ENTRY_1E6_DEFAULT)?,
        entry_min_px_1e6: opt_int(&kv, "entry_min_px_1e6", 0)?,
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
    if file.e_take_1e6 < 100 {
        return Err(err(format!(
            "`e_take_1e6` must be at least 100 (one 1e-4 tick); got {}",
            file.e_take_1e6
        )));
    }
    if file.h_quote_1e6 < 100 {
        return Err(err(format!(
            "`h_quote_1e6` must be at least 100 (one tick); got {}",
            file.h_quote_1e6
        )));
    }
    if file.tau_min_take_ns >= file.tau_ns || file.tau_min_quote_ns >= file.tau_ns {
        return Err(err(format!(
            "`tau_min_take_ns` {} and `tau_min_quote_ns` {} must both be under \
             `tau_ns` {} — a floor at or above the tenor means the arm never fires",
            file.tau_min_take_ns, file.tau_min_quote_ns, file.tau_ns
        )));
    }
    if file.tail_refuse_ns >= file.tau_min_take_ns {
        return Err(err(format!(
            "`tail_refuse_ns` {} must be under `tau_min_take_ns` {} — otherwise \
             the tail swallows the whole take window and the counter that says \
             so would be `skipped_tail` for every tick of every instance",
            file.tail_refuse_ns, file.tau_min_take_ns
        )));
    }
    if file.requote_thr_1e6 <= 0 {
        return Err(err(format!(
            "`requote_thr_1e6` must be positive (got {}) — a zero threshold \
             re-quotes on every tick and pays the spread for nothing",
            file.requote_thr_1e6
        )));
    }
    if file.clip_qty_1e6 <= 0
        || file.cap_instance_usd_1e6 <= 0
        || file.cap_day_usd_1e6 <= 0
    {
        return Err(err(
            "`clip_qty_1e6`, `cap_instance_usd_1e6` and `cap_day_usd_1e6` must all \
             be positive — a zero cap is a member that cannot trade, spelled as if \
             it could",
        ));
    }
    if file.entry_usd_1e6 < 0 {
        return Err(err(format!(
            "`entry_usd_1e6` must not be negative (got {}); absent means 0 = off",
            file.entry_usd_1e6
        )));
    }
    if file.entry_usd_1e6 > file.cap_instance_usd_1e6 {
        return Err(err(format!(
            "`entry_usd_1e6` {} exceeds `cap_instance_usd_1e6` {} — every coverage \
             entry would be clipped by the cap it is meant to sit under",
            file.entry_usd_1e6, file.cap_instance_usd_1e6
        )));
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
    if file.mark_stale_ns >= file.tau_min_take_ns {
        return Err(err(format!(
            "`mark_stale_ns` {} must be under `tau_min_take_ns` {} — a staleness \
             gate wider than the take window can never hold a take, so it is a \
             gate that does nothing and reads as if it did",
            file.mark_stale_ns, file.tau_min_take_ns
        )));
    }
    // BIN15 P3 (F6). One venue tick, because a margin under the price
    // granularity is a margin the venue cannot express; and under half
    // the interval, because a bound of 0.5 or more can never be cleared
    // by any ask on the preferred side (`belief` is at most 1e6 and the
    // ask is at least one tick).
    if file.e_entry_1e6 < 100 {
        return Err(err(format!(
            "`e_entry_1e6` must be at least 100 (one 1e-4 tick); got {}. Zero would \
             make the coverage entry pay ANY ask, which is the market order this \
             bound exists to stop",
            file.e_entry_1e6
        )));
    }
    if file.e_entry_1e6 >= 500_000 {
        return Err(err(format!(
            "`e_entry_1e6` {} is at or over half the interval — no ask on the \
             preferred side can ever clear it, so the entry arm would never fire \
             and every instance would read as a quiet book",
            file.e_entry_1e6
        )));
    }
    // Inside the price interval, floor included: 0 is "no floor" and a
    // floor at or over 1.0 could never be cleared by any ask, so the
    // entry arm would be silently off.
    if file.entry_min_px_1e6 < 0 || file.entry_min_px_1e6 >= 1_000_000 {
        return Err(err(format!(
            "`entry_min_px_1e6` must be in [0, 1000000) (got {}); absent means 0 = no floor",
            file.entry_min_px_1e6
        )));
    }
    if file.scale_1e9 <= 0 {
        return Err(err(format!(
            "`scale_1e9` must be positive (got {}); absent means 1000000000",
            file.scale_1e9
        )));
    }
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
             e_take_1e6 = 30000\n\
             h_quote_1e6 = 25000\n\
             tau_min_take_ns = 60000000000\n\
             tau_min_quote_ns = 120000000000\n\
             tail_refuse_ns = 10000000000\n\
             requote_ttl_ns = 1000000000\n\
             requote_thr_1e6 = 5000\n\
             clip_qty_1e6 = 500000000\n\
             cap_instance_usd_1e6 = 1000000000\n\
             cap_day_usd_1e6 = 5000000000\n\
             maker_enabled = 1\n\
             null_arm = 1\n\
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
        assert_eq!(f.entry_min_px_1e6, 0, "no floor = the 2026-09-13 entry law");
    }

    /// BIN15 R0 (2026-09-19): the coverage entry's price floor is a
    /// KNOWN optional key (O4b's lesson), it round-trips, and it is
    /// bounded to the open interval below 1.0 — a floor of 1.0 could
    /// never be cleared and would switch the entry arm off in silence.
    #[test]
    fn the_entry_floor_is_a_known_key_that_round_trips_and_is_bounded() {
        let src = format!("{}entry_min_px_1e6 = 700000\n", artifact());
        let f = parse(&src).expect("entry_min_px_1e6 is a known key");
        assert_eq!(f.entry_min_px_1e6, 700_000);
        let zero = format!("{}entry_min_px_1e6 = 0\n", artifact());
        assert_eq!(parse(&zero).expect("an explicit 0").entry_min_px_1e6, 0);
        let bad: [&str; 2] = ["entry_min_px_1e6 = 1000000", "entry_min_px_1e6 = -1"];
        let mut i = 0usize;
        while i < bad.len() {
            let src = format!("{}{}\n", artifact(), bad[i]);
            let e = parse(&src).expect_err(bad[i]);
            assert!(e.0.contains("[0, 1000000)"), "{}: got {}", bad[i], e.0);
            i += 1;
        }
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
        let src = artifact().replace("e_take_1e6 =", "e_take_bps =");
        let e = parse(&src).expect_err("unknown key");
        assert!(e.0.contains("e_take_bps"), "{}", e.0);
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
        let cases: [(&str, &str, &str); 8] = [
            ("tau_ns = 900000000000", "tau_ns = 1800000000000", "not a tenor"),
            ("e_take_1e6 = 30000", "e_take_1e6 = 99", "one 1e-4 tick"),
            ("h_quote_1e6 = 25000", "h_quote_1e6 = 0", "one tick"),
            (
                "tau_min_take_ns = 60000000000",
                "tau_min_take_ns = 900000000000",
                "must both be under",
            ),
            (
                "tail_refuse_ns = 10000000000",
                "tail_refuse_ns = 60000000000",
                "swallows the whole take window",
            ),
            ("requote_thr_1e6 = 5000", "requote_thr_1e6 = 0", "must be positive"),
            ("clip_qty_1e6 = 500000000", "clip_qty_1e6 = 0", "must all"),
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
    fn a_flag_that_is_not_zero_or_one_is_refused() {
        let src = artifact().replace("maker_enabled = 1", "maker_enabled = 2");
        let e = parse(&src).expect_err("flag");
        assert!(e.0.contains("must be 0 or 1"), "{}", e.0);
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
}
