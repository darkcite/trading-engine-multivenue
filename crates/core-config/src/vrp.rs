// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # vrp — the VRP member's parameter artifact (VRP V4)
//!
//! Parses `~/multivenue/vrp.toml` (or `--vrp <path>`). Same doctrine as
//! [`super::icdp`]: a deliberate TOML SUBSET, hand-parsed because the
//! house owns every parser; every number an INTEGER in the strategy's
//! own fixed point, because a float in a config file is a float in the
//! decision that reads it.
//!
//! ```toml
//! # ~/multivenue/vrp.toml
//! [vrp]
//! theta_1e9             = 100000000        # θ = 0.10, the log-space band half-width
//! tau_ns                = 28800000000000   # 8 h — a MEASURED tenor (see below)
//! epsilon_ns            = 300000000000     # exit this long before expiry
//! selection_ns          = 600000000000     # pick the strike this long before E−τ
//! rebalance_ns          = 3600000000000    # hedge check cadence (hourly)
//! qty_1e6               = 1000000          # one contract = one whole coin
//! band_qty_1e6          = 50000            # hedge rebalance band
//! underlying_descriptor = "deribit:BTC-PERPETUAL"
//! hedge_descriptor      = "deribit:BTC-PERPETUAL"
//! sides                 = "both"           # OPTIONAL: both | short | long
//! ```
//!
//! ## Optional keys are absent-is-bit-identical
//!
//! `sides` is the first key this artifact carries that a live file may
//! not have. Absent means [`strategy_vrp::SIDES_BOTH`] in spirit — the
//! shape every measurement to date was made on — so an existing
//! `vrp.toml` boots to exactly the behaviour it booted to before. The
//! `core-regime` hysteresis keys set that precedent and it holds here.
//!
//! ## `tau_ns` is checked against the evidence, not against a range
//!
//! Kill criterion 4 of the edge spec is that the 12 h and 24 h cells are
//! NOT to be traded — E1 is absent there, and the 24 h cells lose money
//! at every threshold. So `tau_ns` is validated by
//! [`core_vol::tenor_of`], the same function the forecast engine itself
//! consults, rather than by a `>= 0` test here. There is exactly one
//! place in the tree that knows which tenors are tradeable, and a config
//! file cannot talk its way past it.

use std::path::Path;

use super::icdp::{parse_int, parse_value, strip_comment, IcdpError, Value};

/// Hard ceiling on `qty_1e6`: one contract ×1e6 — which is also exactly
/// the Deribit per-order cap after the operator's 2026-09-10 amendment
/// (one whole coin, `strategy_core::CAPS_DERIBIT`). The runtime gate in
/// `strategy-vrp` is still the enforcement — it knows the venue and the
/// side — but catching a typed extra zero at boot beats catching it as
/// a counted refusal at every decision for the rest of the day.
pub const VRP_QTY_MAX_1E6: i64 = 1_000_000;

/// Parse / load failure. Reuses [`IcdpError`]'s shape so the two
/// artifacts report identically; the message names the file.
pub type VrpError = IcdpError;

/// Build a [`VrpError`]. A type alias cannot be a tuple constructor, and
/// a second error enum for the same failure mode would be a second thing
/// to keep in sync.
#[inline]
fn err(msg: String) -> VrpError {
    IcdpError(msg)
}

/// The parsed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VrpFile {
    /// Band half-width in log space ×1e9 (θ = 0.10 ⇒ `100_000_000`).
    pub theta_1e9: i64,
    /// Hold length ns. Must be a tenor [`core_vol::tenor_of`] accepts.
    pub tau_ns: u64,
    /// Exit this many ns before expiry (the E−ε law).
    pub epsilon_ns: u64,
    /// Select the strike this many ns before the entry instant E−τ.
    pub selection_ns: u64,
    /// Hedge rebalance cadence ns.
    pub rebalance_ns: u64,
    /// Option position size ×1e6 (one contract = `1_000_000`).
    pub qty_1e6: i64,
    /// Hedge rebalance band ×1e6: rebalance only when the target moves
    /// by at least this much.
    pub band_qty_1e6: i64,
    /// §9.4 descriptor of the instrument whose minute closes feed the
    /// forecast.
    pub underlying_descriptor: String,
    /// §9.4 descriptor of the instrument the delta hedge trades.
    pub hedge_descriptor: String,
    /// Q4: which arms of the band may be traded — `0` both (the
    /// default, and what an absent key means), `1` short vol only,
    /// `2` long vol only.
    pub sides: u8,
}

/// Default location beside `universe.toml`.
pub fn default_vrp_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/vrp.toml")
}

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT artifact it booted with.
pub fn load(path: &Path) -> Result<(VrpFile, Vec<u8>), VrpError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|_| err(format!("{}: not UTF-8", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

const VRP_KEYS: [&str; 10] = [
    "theta_1e9",
    "tau_ns",
    "epsilon_ns",
    "selection_ns",
    "rebalance_ns",
    "qty_1e6",
    "band_qty_1e6",
    "underlying_descriptor",
    "hedge_descriptor",
    "sides",
];

/// `sides` values, mirroring `strategy_vrp::SIDES_*`. The member owns
/// the constants; this is the grammar that reaches them.
const SIDES_NAMES: [(&str, u8); 3] = [("both", 0), ("short", 1), ("long", 2)];

/// Upper bound on `theta_1e9`: `ln 2 ≈ 0.69` doubles the forecast, and
/// θ = 2.0 log points is a band no implied vol on a traded chain can
/// leave. A θ past this is a typed extra zero, not a policy.
pub const VRP_THETA_MAX_1E9: i64 = 2_000_000_000;

fn take_int(kv: &[(String, Value, usize)], key: &str) -> Result<i64, VrpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("[vrp]: missing `{key}`"))),
    }
}

/// F24: strictly positive. The message always said "must be > 0" and
/// the code accepted 0, which is not cosmetic: `selection_ns = 0` makes
/// the decision band zero-width so every campaign is silently
/// `decisions_late`, and `rebalance_ns = 0` re-hedges on every tick.
fn take_pos_u64(kv: &[(String, Value, usize)], key: &str) -> Result<u64, VrpError> {
    let v = take_int(kv, key)?;
    if v <= 0 {
        return Err(err(format!("`{key}` must be > 0 (got {v})")));
    }
    u64::try_from(v).map_err(|_| err(format!("`{key}` must be > 0 (got {v})")))
}

/// Q4/Q12: an OPTIONAL string key. Absent is not an error — it is the
/// default the caller names.
fn take_opt_enum(
    kv: &[(String, Value, usize)],
    key: &str,
    names: &[(&str, u8)],
    default: u8,
) -> Result<u8, VrpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        None => Ok(default),
        Some((_, Value::Str(v), l)) => names
            .iter()
            .find(|(n, _)| *n == v.as_str())
            .map(|(_, code)| *code)
            .ok_or_else(|| {
                let allowed: Vec<&str> = names.iter().map(|(n, _)| *n).collect();
                err(format!(
                    "line {l}: `{key}` must be one of {} (got `{v}`)",
                    allowed.join(" | ")
                ))
            }),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string"))),
    }
}

fn take_str(kv: &[(String, Value, usize)], key: &str) -> Result<String, VrpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Str(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be a string"))),
        None => Err(err(format!("[vrp]: missing `{key}`"))),
    }
}

/// Parse the artifact text. Unknown sections/keys, missing keys,
/// duplicate keys, a float anywhere, or a `tau_ns` the evidence does not
/// support are all FATAL.
pub fn parse(src: &str) -> Result<VrpFile, VrpError> {
    let mut kv: Vec<(String, Value, usize)> = Vec::new();
    let mut in_section = false;
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            let name = name
                .strip_suffix(']')
                .ok_or_else(|| err(format!("line {ln}: unterminated section header")))?;
            if name != "vrp" {
                return Err(err(format!("line {ln}: unknown section `[{name}]`")));
            }
            if in_section {
                return Err(err(format!("line {ln}: duplicate `[vrp]`")));
            }
            in_section = true;
            continue;
        }
        if !in_section {
            return Err(err(format!("line {ln}: key outside `[vrp]`")));
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| err(format!("line {ln}: expected `key = value`")))?;
        let k = k.trim().to_owned();
        if !VRP_KEYS.contains(&k.as_str()) {
            return Err(err(format!("line {ln}: unknown key `{k}`")));
        }
        if kv.iter().any(|(existing, _, _)| *existing == k) {
            return Err(err(format!("line {ln}: duplicate key `{k}`")));
        }
        kv.push((k, parse_value(v, ln)?, ln));
    }
    if !in_section {
        return Err(err("missing `[vrp]` section".to_owned()));
    }

    let file = VrpFile {
        theta_1e9: take_int(&kv, "theta_1e9")?,
        tau_ns: take_pos_u64(&kv, "tau_ns")?,
        epsilon_ns: take_pos_u64(&kv, "epsilon_ns")?,
        selection_ns: take_pos_u64(&kv, "selection_ns")?,
        rebalance_ns: take_pos_u64(&kv, "rebalance_ns")?,
        qty_1e6: take_int(&kv, "qty_1e6")?,
        band_qty_1e6: take_int(&kv, "band_qty_1e6")?,
        underlying_descriptor: take_str(&kv, "underlying_descriptor")?,
        hedge_descriptor: take_str(&kv, "hedge_descriptor")?,
        sides: take_opt_enum(&kv, "sides", &SIDES_NAMES, 0)?,
    };

    if file.theta_1e9 <= 0 || file.theta_1e9 > VRP_THETA_MAX_1E9 {
        return Err(err(format!(
            "`theta_1e9` must be in 1..={VRP_THETA_MAX_1E9} (got {})",
            file.theta_1e9
        )));
    }
    if core_vol::tenor_of(file.tau_ns).is_none() {
        return Err(err(format!(
            "`tau_ns` {} is not a tradeable tenor: E1 is measured at 4 h and 8 h \
             and is absent by 12 h, so kill criterion 4 forbids the longer cells",
            file.tau_ns
        )));
    }
    if file.epsilon_ns >= file.tau_ns {
        return Err(err(format!(
            "`epsilon_ns` {} must be < `tau_ns` {} — the exit cannot precede the entry",
            file.epsilon_ns, file.tau_ns
        )));
    }
    if file.rebalance_ns > file.tau_ns {
        return Err(err(format!(
            "`rebalance_ns` {} exceeds `tau_ns` {} — the hedge would never rebalance",
            file.rebalance_ns, file.tau_ns
        )));
    }
    if file.qty_1e6 <= 0 || file.qty_1e6 > VRP_QTY_MAX_1E6 {
        return Err(err(format!(
            "`qty_1e6` must be in 1..={VRP_QTY_MAX_1E6} (got {}) — one contract is one \
             whole coin of underlying exposure, and the binding limit is the runtime \
             notional cap, which refuses an entry whose worst-case (delta = 1) hedge \
             would breach `docs/risk-policy.md`",
            file.qty_1e6
        )));
    }
    if file.band_qty_1e6 <= 0 || file.band_qty_1e6 > file.qty_1e6 {
        return Err(err(format!(
            "`band_qty_1e6` must be in 1..=`qty_1e6` {} (got {}) — a zero band \
             rebalances on every tick and pays the spread for nothing, and a band \
             wider than the whole position never rebalances at all",
            file.qty_1e6, file.band_qty_1e6
        )));
    }
    // F24: the decision band `[E−τ, E−τ+selection]` must close BEFORE
    // the hedge freezes at `E−ε`, or a campaign could authorise an entry
    // it may not hedge.
    if file.selection_ns >= file.tau_ns.saturating_sub(file.epsilon_ns) {
        return Err(err(format!(
            "`selection_ns` {} must be < `tau_ns` − `epsilon_ns` ({}) — the decision \
             band has to close before the hedge freezes at E−ε",
            file.selection_ns,
            file.tau_ns.saturating_sub(file.epsilon_ns)
        )));
    }
    if file.underlying_descriptor.is_empty() || file.hedge_descriptor.is_empty() {
        return Err(err("descriptors must be non-empty".to_owned()));
    }
    Ok(file)
}

/// The newest `vrp-seed.tsv` grammar this binary reads — the worker's
/// `claude_worker.vrp_seed.SEED_VERSION`. A v1 seed is bare triples; a
/// v2 seed is TAGGED (`V`/`P`/`R`). Anything higher carries rows this
/// code has never seen, and reading it would mean guessing.
pub const SEED_VERSION_MAX: i64 = 2;

/// Parse one `expiry_ts_ms\tx_1e9\ty_1e9` seed row (V5's `vrp-seed.tsv`).
/// Integers only — the seed is fitted output, and a float parser on the
/// boot path is how a rounding difference becomes a different fit.
pub fn parse_seed_row(line: &str, ln: usize) -> Result<(u64, i64, i64), VrpError> {
    let t = line.trim();
    let mut it = t.split('\t');
    let (a, b, c, rest) = (it.next(), it.next(), it.next(), it.next());
    match (a, b, c, rest) {
        (Some(a), Some(b), Some(c), None) => {
            let ts = parse_int(a, ln)?;
            if ts <= 0 {
                return Err(err(format!("line {ln}: expiry_ts_ms must be > 0")));
            }
            Ok((ts as u64, parse_int(b, ln)?, parse_int(c, ln)?))
        }
        _ => Err(err(format!(
            "line {ln}: want `expiry_ts_ms<TAB>x_1e9<TAB>y_1e9`"
        ))),
    }
}

/// W3: one minute's return, stamped with the minute it belongs to
/// (ms since the epoch) so two independently-cut series can be lined
/// up against each other.
pub type MinuteReturn = (u64, i64);

/// The step between consecutive minute stamps. A series is CONTIGUOUS
/// when every neighbouring pair differs by exactly this.
pub const MINUTE_STEP_MS: u64 = 60_000;

/// W3: pull the `R <min_ts_ms> <r_1e9>` rows out of a seed or state
/// file, oldest first.
///
/// Every other tag is ignored rather than refused, because this runs
/// over BOTH files and neither one is only returns: `vrp-seed.tsv`
/// also carries the fitted pairs, `vrp-state.tsv` also carries the
/// QLIKE window, the kill flag and the live campaign.
///
/// Rows must be strictly increasing in time. They are replayed into a
/// ring whose eviction arm assumes chronological order, so an
/// out-of-order file is not a cosmetic problem — it evicts the wrong
/// return and silently produces a different window.
pub fn parse_returns(src: &str) -> Result<Vec<MinuteReturn>, VrpError> {
    let mut out: Vec<MinuteReturn> = Vec::new();
    let mut prev_ts = 0u64;
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        if it.next() != Some("R") {
            continue;
        }
        let (a, b, rest) = (it.next(), it.next(), it.next());
        let (Some(a), Some(b), None) = (a, b, rest) else {
            return Err(err(format!("line {ln}: want `R<TAB>min_ts_ms<TAB>r_1e9`")));
        };
        let ts = parse_int(a, ln)?;
        if ts <= 0 {
            return Err(err(format!("line {ln}: min_ts_ms must be > 0")));
        }
        let ts = ts as u64;
        if ts <= prev_ts && prev_ts != 0 {
            return Err(err(format!(
                "line {ln}: return rows must be strictly increasing in min_ts_ms \
                 (got {ts} after {prev_ts})"
            )));
        }
        prev_ts = ts;
        out.push((ts, parse_int(b, ln)?));
    }
    Ok(out)
}

/// W3: reconcile the two rolling-window sources into the one series the
/// member replays, newest last, and say where each kept minute came
/// from.
///
/// `seed` is the worker's cut from `candles.db` — a full day, but as
/// stale as the last hourly refresh. `state` is what the engine itself
/// wrote before it went down — current to the second, but only as long
/// as the last uptime, which on this fleet can be twenty minutes.
/// NEITHER is enough alone, which is why both exist.
///
/// The rules, in order:
///
/// 1. **Union by minute.** Where both carry the same minute, `state`
///    wins: it is the engine's own observation of the live tape, and
///    the candle is a REST-derived aggregate of that same minute. The
///    two are different derivations of the same quantity, so a union
///    splices two series — a handful of splice points move a
///    1440-minute `Σ r²` by far less than a cold window that cannot
///    forecast at all, and the caller reports the split.
/// 2. **Longest contiguous suffix.** A gap means the window is not the
///    24 h it would claim to be, so everything at or before the newest
///    gap is dropped rather than silently counted.
/// 3. **Capped** at the ring the member replays into.
///
/// Returns `(merged, from_seed, from_state)` over the KEPT rows.
#[must_use]
pub fn merge_returns(
    seed: &[MinuteReturn],
    state: &[MinuteReturn],
    cap: usize,
) -> (Vec<MinuteReturn>, usize, usize) {
    let mut by_min: std::collections::BTreeMap<u64, (i64, bool)> =
        std::collections::BTreeMap::new();
    for (ts, r) in seed {
        by_min.insert(*ts, (*r, false));
    }
    // Second, so a shared minute resolves to the engine's own view.
    for (ts, r) in state {
        by_min.insert(*ts, (*r, true));
    }
    let all: Vec<(u64, i64, bool)> = by_min.iter().map(|(t, (r, s))| (*t, *r, *s)).collect();
    if all.is_empty() || cap == 0 {
        return (Vec::new(), 0, 0);
    }
    // Walk back from the newest while the step stays exactly one minute.
    let mut start = all.len() - 1;
    while start > 0 && all.len() - start < cap && all[start].0 - all[start - 1].0 == MINUTE_STEP_MS
    {
        start -= 1;
    }
    let kept = &all[start..];
    let mut from_seed = 0usize;
    let mut from_state = 0usize;
    let mut merged = Vec::with_capacity(kept.len());
    for (ts, r, is_state) in kept {
        if *is_state {
            from_state += 1;
        } else {
            from_seed += 1;
        }
        merged.push((*ts, *r));
    }
    (merged, from_seed, from_state)
}

/// F2: pull the `P <expiry_ts_ms> <x_1e9> <y_1e9>` rows out of a STATE
/// file, oldest first. The shape [`parse_returns`] has for `R` rows, and
/// for the same reason: the state file is not only pairs.
///
/// Rows must be strictly increasing in `expiry_ts_ms`. The engine writes
/// its pair ring in chronological order and one expiry settles once, so
/// a repeat or a reversal is a corrupt file, not an unusual one.
pub fn parse_pairs(src: &str) -> Result<Vec<(u64, i64, i64)>, VrpError> {
    let mut out: Vec<(u64, i64, i64)> = Vec::new();
    let mut prev_ts = 0u64;
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        if it.next() != Some("P") {
            continue;
        }
        let (a, b, c, rest) = (it.next(), it.next(), it.next(), it.next());
        let (Some(a), Some(b), Some(c), None) = (a, b, c, rest) else {
            return Err(err(format!(
                "line {ln}: want `P<TAB>expiry_ts_ms<TAB>x_1e9<TAB>y_1e9`"
            )));
        };
        let ts = parse_int(a, ln)?;
        if ts <= 0 {
            return Err(err(format!("line {ln}: expiry_ts_ms must be > 0")));
        }
        let ts = ts as u64;
        if ts <= prev_ts && prev_ts != 0 {
            return Err(err(format!(
                "line {ln}: pair rows must be strictly increasing in expiry_ts_ms \
                 (got {ts} after {prev_ts})"
            )));
        }
        prev_ts = ts;
        out.push((ts, parse_int(b, ln)?, parse_int(c, ln)?));
    }
    Ok(out)
}

/// F2: reconcile the two fitted-pair sources into the one series the
/// member replays, oldest first.
///
/// The engine used to push BOTH: the worker's seed at boot, and then
/// every `P` row of its own state file — which renders the WHOLE ring
/// and is therefore a superset of that same seed. `push_pair` has no
/// identity by expiry, so shared expiries went into the OLS twice. Live
/// proof, every boot since 2026-09-11: `seed applied seed_pairs=90 …
/// state restored pairs=90 … total_pairs=128` — 38 expiries counted
/// twice, and after F1 with two DIFFERENT `y` each.
///
/// The rules, mirroring [`merge_returns`]:
///
/// 1. **Union by `expiry_ts_ms`.** On a collision `state` wins: it is
///    the engine's own settlement of that expiry, formed from the
///    minutes it actually observed.
/// 2. **Chronological**, oldest first — the ring is order-sensitive
///    once it wraps.
/// 3. **Capped** at the ring, keeping the NEWEST `cap`.
///
/// Returns `(merged, from_seed, from_state)` over the KEPT rows.
#[must_use]
pub fn merge_pairs(
    seed: &[(u64, i64, i64)],
    state: &[(u64, i64, i64)],
    cap: usize,
) -> (Vec<(u64, i64, i64)>, usize, usize) {
    if cap == 0 {
        return (Vec::new(), 0, 0);
    }
    let mut by_expiry: std::collections::BTreeMap<u64, (i64, i64, bool)> =
        std::collections::BTreeMap::new();
    for (ts, x, y) in seed {
        by_expiry.insert(*ts, (*x, *y, false));
    }
    // Second, so a shared expiry resolves to the engine's own settlement.
    for (ts, x, y) in state {
        by_expiry.insert(*ts, (*x, *y, true));
    }
    let all: Vec<(u64, i64, i64, bool)> = by_expiry
        .iter()
        .map(|(t, (x, y, s))| (*t, *x, *y, *s))
        .collect();
    let start = all.len().saturating_sub(cap);
    let mut from_seed = 0usize;
    let mut from_state = 0usize;
    let mut merged = Vec::with_capacity(all.len() - start);
    for (ts, x, y, is_state) in &all[start..] {
        if *is_state {
            from_state += 1;
        } else {
            from_seed += 1;
        }
        merged.push((*ts, *x, *y));
    }
    (merged, from_seed, from_state)
}

/// Parse a whole `vrp-seed.tsv`: `#` comments and blank lines skipped,
/// every other line an `expiry_ts_ms\tx_1e9\ty_1e9` triple. Rows are
/// returned in FILE order, which the cutter writes oldest first — the
/// pair ring is order-sensitive once it wraps, so a re-ordered seed is a
/// different (and undeclared) fit.
pub fn parse_seed(src: &str) -> Result<Vec<(u64, i64, i64)>, VrpError> {
    let mut out = Vec::new();
    let mut prev_ts = 0u64;
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // W3: a v2 seed is TAGGED — `V`, `P` and `R` rows side by
        // side. A v1 seed is bare triples. Both parse here; only the
        // `P` rows are pairs either way.
        let mut head = line.split('\t');
        let body = match head.next() {
            Some("R") => continue,
            // F25: the state file's version row is checked and the
            // seed's was not, so a v3 seed written by a newer worker
            // would have been read as a v2 one — silently, on the boot
            // path, into the fit the member trades on.
            Some("V") => {
                let v = parse_int(head.next().unwrap_or("").trim(), ln)?;
                if !(1..=SEED_VERSION_MAX).contains(&v) {
                    return Err(err(format!(
                        "line {ln}: seed version {v} is not one this binary reads \
                         (1..={SEED_VERSION_MAX})"
                    )));
                }
                continue;
            }
            Some("P") => line.get(2..).unwrap_or("").trim(),
            _ => line,
        };
        let row = parse_seed_row(body, ln)?;
        if row.0 <= prev_ts && prev_ts != 0 {
            return Err(err(format!(
                "line {ln}: seed rows must be strictly increasing in expiry_ts_ms \
                 (got {} after {prev_ts})",
                row.0
            )));
        }
        prev_ts = row.0;
        out.push(row);
    }
    Ok(out)
}

/// Read + parse a seed file.
pub fn load_seed(path: &Path) -> Result<Vec<(u64, i64, i64)>, VrpError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| err(format!("{}: {e}", path.display())))?;
    parse_seed(&src)
}

/// Default seed location beside `vrp.toml` — the WORKER's bootstrap cut.
pub fn default_seed_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/vrp-seed.tsv")
}

/// V8a: default location of the ENGINE's own persisted state, beside the
/// seed. Written by the engine, read by the engine, never hand-authored
/// — the seed bootstraps a first boot, this carries everything since.
pub fn default_state_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/vrp-state.tsv")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "\
# generated by the vault one-shot
[vrp]
theta_1e9             = 100000000
tau_ns                = 28800000000000
epsilon_ns            = 300000000000
selection_ns          = 600000000000
rebalance_ns          = 3600000000000
qty_1e6               = 1000000
band_qty_1e6          = 50000
underlying_descriptor = \"deribit:BTC-PERPETUAL\"
hedge_descriptor      = \"deribit:BTC-PERPETUAL\"
";

    #[test]
    fn the_reference_artifact_parses() {
        let f = parse(GOOD).expect("parses");
        assert_eq!(f.theta_1e9, 100_000_000);
        assert_eq!(f.tau_ns, 28_800_000_000_000);
        assert_eq!(f.qty_1e6, 1_000_000);
        assert_eq!(f.band_qty_1e6, 50_000);
        assert_eq!(f.hedge_descriptor, "deribit:BTC-PERPETUAL");
    }

    #[test]
    fn an_untradeable_tenor_is_fatal() {
        // The whole point: a config file cannot open a cell the
        // evidence does not support.
        for tau in ["43200000000000", "86400000000000", "1"] {
            let src = GOOD.replace("28800000000000", tau);
            let err = parse(&src).expect_err("must refuse");
            assert!(
                err.0.contains("not a tradeable tenor"),
                "tau {tau}: {}",
                err.0
            );
        }
        // 4 h is measured, so it parses.
        let src = GOOD.replace("28800000000000", "14400000000000");
        // ε and the rebalance cadence must shrink with τ.
        let src = src
            .replace("3600000000000", "1800000000000")
            .replace("600000000000", "300000000000");
        assert!(parse(&src).is_ok(), "4 h is a measured tenor");
    }

    #[test]
    fn floats_and_unknown_keys_are_fatal() {
        assert!(parse(&GOOD.replace("100000000", "0.10")).is_err(), "float θ");
        assert!(parse(&GOOD.replace("theta_1e9", "theta")).is_err(), "unknown key");
        assert!(parse(&GOOD.replace("[vrp]", "[vrpx]")).is_err(), "unknown section");
        assert!(
            parse(&format!("{GOOD}theta_1e9 = 1\n")).is_err(),
            "duplicate key"
        );
        assert!(parse(&GOOD.replace("[vrp]\n", "")).is_err(), "key outside a section");
        assert!(parse("").is_err(), "empty file");
    }

    #[test]
    fn nonsense_values_are_fatal() {
        assert!(parse(&GOOD.replace("theta_1e9             = 100000000", "theta_1e9             = 0")).is_err());
        assert!(parse(&GOOD.replace("qty_1e6               = 1000000", "qty_1e6               = 0")).is_err());
        // A typed extra zero is caught at boot, not at every decision.
        assert!(
            parse(&GOOD.replace("qty_1e6               = 1000000", "qty_1e6               = 10000000"))
                .is_err(),
            "10 contracts is a typo, not a size"
        );
        assert!(parse(&GOOD.replace("band_qty_1e6          = 50000", "band_qty_1e6          = 0")).is_err());
        // ε ≥ τ would exit before it entered.
        assert!(parse(&GOOD.replace("epsilon_ns            = 300000000000", "epsilon_ns            = 28800000000000")).is_err());
        // A rebalance cadence longer than the hold never fires.
        assert!(parse(&GOOD.replace("rebalance_ns          = 3600000000000", "rebalance_ns          = 99800000000000")).is_err());
        // A missing key is fatal, not defaulted.
        assert!(parse(&GOOD.replace("qty_1e6               = 1000000\n", "")).is_err());
    }

    #[test]
    fn seed_rows_are_integers_or_nothing() {
        assert_eq!(
            parse_seed_row("1757462400000\t24659086751\t24700000000", 1).unwrap(),
            (1_757_462_400_000, 24_659_086_751, 24_700_000_000)
        );
        assert!(parse_seed_row("1757462400000\t24659086751", 1).is_err(), "short");
        assert!(parse_seed_row("1757462400000\t2.5\t3", 1).is_err(), "float");
        assert!(parse_seed_row("0\t1\t2", 1).is_err(), "non-positive ts");
        assert!(parse_seed_row("a\tb\tc", 1).is_err(), "not integers");
        // Negative x/y are legal: they are logs, and a log can be < 0.
        assert!(parse_seed_row("1\t-5\t-6", 1).is_ok());
    }

    #[test]
    fn a_seed_file_parses_in_order_and_refuses_disorder() {
        let good = "# vrp-seed.tsv\n\n1000\t10\t11\n2000\t20\t21\n3000\t30\t31\n";
        let rows = parse_seed(good).expect("parses");
        assert_eq!(rows, vec![(1000, 10, 11), (2000, 20, 21), (3000, 30, 31)]);
        // Comments and blank lines are skipped, not counted.
        assert_eq!(parse_seed("# only a comment\n\n").unwrap(), vec![]);
        // Out of order, or a repeat, is fatal: the pair ring is
        // order-sensitive once it wraps, so a shuffled seed is a
        // different fit than the one the cutter measured.
        assert!(parse_seed("2000\t1\t2\n1000\t3\t4\n").is_err());
        assert!(parse_seed("2000\t1\t2\n2000\t3\t4\n").is_err());
        // And a malformed row anywhere refuses the whole file rather
        // than silently seeding a shorter history.
        assert!(parse_seed("1000\t1\t2\nnonsense\n").is_err());
    }

    // ---------------------------------------------------------------
    // W3 — reconciling the two rolling-window sources
    // ---------------------------------------------------------------

    const T0: u64 = 1_789_000_000_000;

    fn series(from_min: u64, n: u64, val: i64) -> Vec<MinuteReturn> {
        (0..n)
            .map(|i| (T0 + (from_min + i) * MINUTE_STEP_MS, val))
            .collect()
    }

    #[test]
    fn parse_returns_reads_r_rows_and_ignores_everything_else() {
        let src = "# a comment\nV\t2\nP\t1789000000000\t1\t2\n\
                   R\t1789000000000\t-500\nQ\t1\t2\nR\t1789000060000\t700\n";
        let got = parse_returns(src).expect("parses");
        assert_eq!(got, vec![(1_789_000_000_000, -500), (1_789_000_060_000, 700)]);
        // And a v2 SEED — tagged, with both row kinds — still yields
        // its pairs. (`Q` above is a state-file tag; `parse_seed` is
        // for seed files and refuses tags it was never given.)
        let seed_v2 = "# cut by the worker\nV\t2\nP\t1789000000000\t1\t2\n\
                       R\t1789000000000\t-500\n";
        assert_eq!(parse_seed(seed_v2).expect("pairs"), vec![(1_789_000_000_000, 1, 2)]);
        assert_eq!(parse_returns(seed_v2).expect("returns").len(), 1);
        // A v1 seed — bare triples, every seed cut before 2026-09-11 —
        // still parses, and simply carries no window.
        assert_eq!(parse_seed("1000\t1\t2\n").expect("v1"), vec![(1000, 1, 2)]);
        assert!(parse_returns("1000\t1\t2\n").expect("v1").is_empty());
    }

    #[test]
    fn parse_returns_refuses_disorder_and_a_zero_stamp() {
        // The ring's eviction arm assumes chronological order, so a
        // shuffled file does not produce an odd window — it produces a
        // DIFFERENT one, silently.
        assert!(parse_returns("R\t1789000060000\t1\nR\t1789000000000\t2\n").is_err());
        assert!(parse_returns("R\t1789000000000\t1\nR\t1789000000000\t2\n").is_err());
        assert!(parse_returns("R\t0\t1\n").is_err(), "0 is not a minute");
        assert!(parse_returns("R\t1789000000000\n").is_err(), "short row");
        assert!(parse_returns("R\t1789000000000\t1\t2\n").is_err(), "long row");
    }

    /// Where both sources carry the same minute the ENGINE's own view
    /// wins: it observed the live tape, the candle is a REST-derived
    /// aggregate of that same minute.
    #[test]
    fn the_engine_wins_a_shared_minute_and_the_seed_fills_the_rest() {
        // Seed: minutes 0..100 as −1. State: minutes 90..100 as +1.
        let seed = series(0, 100, -1);
        let state = series(90, 10, 1);
        let (merged, from_seed, from_state) = merge_returns(&seed, &state, 1_000);
        assert_eq!(merged.len(), 100, "union, not concatenation");
        assert_eq!(from_state, 10);
        assert_eq!(from_seed, 90);
        assert_eq!(merged[89], (T0 + 89 * MINUTE_STEP_MS, -1), "seed-only minute");
        assert_eq!(merged[90], (T0 + 90 * MINUTE_STEP_MS, 1), "contested ⇒ state");
        assert_eq!(merged[99], (T0 + 99 * MINUTE_STEP_MS, 1));
    }

    /// Neither source is enough alone, which is the whole reason both
    /// exist: the state covers only the last uptime, the seed only up
    /// to the worker's last hourly refresh.
    #[test]
    fn the_two_sources_together_span_what_neither_covers() {
        // The worker's cut ends an hour ago; the engine booted 20 min ago.
        let seed = series(0, 1_380, 7); // 23 h, stale by 60 min
        let state = series(1_380, 60, 9); // the last hour, live
        let (merged, from_seed, from_state) = merge_returns(&seed, &state, 1_536);
        assert_eq!(merged.len(), 1_440, "a full 24 h window from two partial ones");
        assert_eq!((from_seed, from_state), (1_380, 60));
    }

    /// A gap means the window is not the 24 h it would claim to be, so
    /// everything at or before the newest gap is dropped rather than
    /// silently counted as contiguous.
    #[test]
    fn a_gap_truncates_to_the_newest_contiguous_run() {
        let mut rows = series(0, 50, 1);
        rows.extend(series(60, 30, 2)); // 10-minute hole at 50..60
        let (merged, _, _) = merge_returns(&rows, &[], 1_000);
        assert_eq!(merged.len(), 30, "only the run AFTER the hole survives");
        assert_eq!(merged[0], (T0 + 60 * MINUTE_STEP_MS, 2));
        // Contiguous by construction.
        for w in merged.windows(2) {
            assert_eq!(w[1].0 - w[0].0, MINUTE_STEP_MS);
        }
    }

    #[test]
    fn the_merge_is_capped_by_the_ring_and_keeps_the_newest() {
        let rows = series(0, 2_000, 3);
        let (merged, _, _) = merge_returns(&rows, &[], 1_536);
        assert_eq!(merged.len(), 1_536);
        assert_eq!(
            merged[merged.len() - 1].0,
            T0 + 1_999 * MINUTE_STEP_MS,
            "the newest minute is always kept"
        );
    }

    #[test]
    fn an_empty_merge_is_empty_not_a_panic() {
        assert_eq!(merge_returns(&[], &[], 1_536).0.len(), 0);
        assert_eq!(merge_returns(&series(0, 10, 1), &[], 0).0.len(), 0);
        // A single minute is trivially contiguous.
        assert_eq!(merge_returns(&series(0, 1, 5), &[], 16).0.len(), 1);
    }

    // ---------------- F2 / F24 / F25 / Q4 ----------------

    #[test]
    fn merge_pairs_unions_by_expiry_and_state_wins() {
        let seed = [(1u64, 10i64, 11i64), (2, 20, 21)];
        let state = [(2u64, 200i64, 201i64), (3, 30, 31)];
        let (merged, from_seed, from_state) = merge_pairs(&seed, &state, 128);
        assert_eq!(
            merged,
            vec![(1, 10, 11), (2, 200, 201), (3, 30, 31)],
            "union by expiry, chronological, and the ENGINE's settlement wins"
        );
        assert_eq!((from_seed, from_state), (1, 2));
        // The live shape: the state file renders the whole ring, so it
        // is a superset of the seed plus whatever settled since.
        let seed: Vec<(u64, i64, i64)> =
            (0..90u64).map(|i| (1_000 + i, i as i64, -(i as i64))).collect();
        let mut state = seed.clone();
        state.push((2_000, 7, 8));
        let (merged, from_seed, from_state) = merge_pairs(&seed, &state, 128);
        assert_eq!(merged.len(), 91, "91 expiries, not 181");
        assert_eq!((from_seed, from_state), (0, 91), "state won every collision");
    }

    #[test]
    fn merge_pairs_keeps_the_newest_and_survives_the_empty_cases() {
        let seed: Vec<(u64, i64, i64)> =
            (0..200u64).map(|i| (1_000 + i, i as i64, 0)).collect();
        let (merged, _, _) = merge_pairs(&seed, &[], 128);
        assert_eq!(merged.len(), 128);
        assert_eq!(merged[127].0, 1_199, "the newest expiry is always kept");
        assert_eq!(merged[0].0, 1_072);
        assert!(merge_pairs(&[], &[], 128).0.is_empty());
        assert!(merge_pairs(&seed, &[], 0).0.is_empty());
    }

    #[test]
    fn parse_pairs_reads_only_p_rows_and_refuses_a_shuffled_file() {
        let src = "# a state file\nV\t4\nR\t1789000000000\t5\n\
                   P\t1000\t10\t11\nQ\t1\t2\nP\t2000\t20\t21\nK\t1\n";
        assert_eq!(
            parse_pairs(src).expect("parses"),
            vec![(1_000, 10, 11), (2_000, 20, 21)]
        );
        // The ring is order-sensitive once it wraps and one expiry
        // settles exactly once.
        assert!(parse_pairs("P\t2000\t1\t2\nP\t1000\t3\t4\n").is_err());
        assert!(parse_pairs("P\t1000\t1\t2\nP\t1000\t3\t4\n").is_err());
        assert!(parse_pairs("P\t0\t1\t2\n").is_err(), "an unstamped pair");
        assert!(parse_pairs("P\t1000\t1\n").is_err(), "a short row");
        assert!(parse_pairs("P\t1000\t1\t2\t3\n").is_err(), "a long row");
        assert!(parse_pairs("").expect("empty is legal").is_empty());
    }

    /// Rewrite one `[vrp]` key's value, keeping every other line.
    fn with_key(key: &str, value: &str) -> String {
        let mut out = String::new();
        for line in GOOD.lines() {
            if line.trim_start().starts_with(key) && line.contains('=') {
                out.push_str(&format!("{key} = {value}\n"));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn zero_is_refused_for_every_positive_key() {
        // F24: the message always said "must be > 0" and the code took
        // 0. `selection_ns = 0` makes the decision band zero-width, so
        // every campaign is silently `decisions_late`; `rebalance_ns =
        // 0` re-hedges on every tick.
        for key in ["tau_ns", "epsilon_ns", "selection_ns", "rebalance_ns"] {
            let e = parse(&with_key(key, "0"))
                .expect_err("a zero must be refused");
            assert!(e.0.contains(key), "the message names the key: {}", e.0);
        }
        // And the rewriter leaves an artifact it does not match alone.
        assert!(parse(&with_key("nothing_here", "0")).is_ok());
    }

    #[test]
    fn the_new_bounds_refuse_what_the_runtime_would_have_to_catch() {
        // θ past 2.0 log points is a typed extra zero, not a policy.
        assert!(parse(&with_key("theta_1e9", "2000000001")).is_err());
        assert!(
            parse(&with_key("theta_1e9", "2000000000")).is_ok(),
            "the boundary itself is legal"
        );
        // A band wider than the whole position never rebalances.
        assert!(parse(&with_key("band_qty_1e6", "1000001")).is_err());
        assert!(parse(&with_key("band_qty_1e6", "1000000")).is_ok(), "equal is legal");
        // The decision band has to close before the hedge freezes at E−ε.
        assert!(parse(&with_key("selection_ns", "28500000000000")).is_err());
    }

    #[test]
    fn seed_version_3_is_refused() {
        // The state file's version row was checked and the seed's was
        // not, so a v3 seed from a newer worker would have been read as
        // a v2 one — silently, on the boot path, into the fit.
        assert!(parse_seed("V\t2\nP\t1000\t1\t2\n").is_ok());
        assert!(parse_seed("V\t1\nP\t1000\t1\t2\n").is_ok());
        let e = parse_seed("V\t3\nP\t1000\t1\t2\n").expect_err("v3 is not readable");
        assert!(e.0.contains("seed version"), "{}", e.0);
        assert!(parse_seed("V\t0\n").is_err());
        assert!(parse_seed("V\tx\n").is_err());
    }

    #[test]
    fn sides_is_optional_and_absent_is_both() {
        // Q12: an existing `vrp.toml` boots to exactly what it booted
        // to before — the core-regime hysteresis precedent.
        assert_eq!(parse(GOOD).expect("parses").sides, 0);
        for (name, code) in [("both", 0u8), ("short", 1), ("long", 2)] {
            let src = format!("{GOOD}sides = \"{name}\"\n");
            assert_eq!(parse(&src).expect("parses").sides, code, "{name}");
        }
        let src = format!("{GOOD}sides = \"neither\"\n");
        let e = parse(&src).expect_err("an unknown arm is refused");
        assert!(e.0.contains("both | short | long"), "{}", e.0);
        let src = format!("{GOOD}sides = 1\n");
        assert!(parse(&src).is_err(), "and it is a string key");
    }
}
