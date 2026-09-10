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
//! qty_1e6               = 1000000          # one contract
//! band_qty_1e6          = 50000            # hedge rebalance band
//! underlying_descriptor = "deribit:BTC-PERPETUAL"
//! hedge_descriptor      = "deribit:BTC-PERPETUAL"
//! ```
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

const VRP_KEYS: [&str; 9] = [
    "theta_1e9",
    "tau_ns",
    "epsilon_ns",
    "selection_ns",
    "rebalance_ns",
    "qty_1e6",
    "band_qty_1e6",
    "underlying_descriptor",
    "hedge_descriptor",
];

fn take_int(kv: &[(String, Value, usize)], key: &str) -> Result<i64, VrpError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(err(format!("line {l}: `{key}` must be an integer"))),
        None => Err(err(format!("[vrp]: missing `{key}`"))),
    }
}

fn take_pos_u64(kv: &[(String, Value, usize)], key: &str) -> Result<u64, VrpError> {
    let v = take_int(kv, key)?;
    u64::try_from(v).map_err(|_| err(format!("`{key}` must be > 0 (got {v})")))
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
    };

    if file.theta_1e9 <= 0 {
        return Err(err(format!(
            "`theta_1e9` must be > 0 (got {})",
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
    if file.qty_1e6 <= 0 {
        return Err(err(format!(
            "`qty_1e6` must be > 0 (got {})",
            file.qty_1e6
        )));
    }
    if file.band_qty_1e6 <= 0 {
        return Err(err(format!(
            "`band_qty_1e6` must be > 0 (got {}) — a zero band rebalances on \
             every tick and pays the spread for nothing",
            file.band_qty_1e6
        )));
    }
    if file.underlying_descriptor.is_empty() || file.hedge_descriptor.is_empty() {
        return Err(err("descriptors must be non-empty".to_owned()));
    }
    Ok(file)
}

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
        let row = parse_seed_row(line, ln)?;
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

/// Default seed location beside `vrp.toml`.
pub fn default_seed_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/vrp-seed.tsv")
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
}
