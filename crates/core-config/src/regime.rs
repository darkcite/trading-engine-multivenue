// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # regime — the detector's parameter artifact (RG2)
//!
//! Parses `~/multivenue/regime.toml` (or `--regime <path>`), the DATA
//! artifact `regime.toml.example` documents (plan §4.6). Same
//! TOML-subset scanner and the same strictness as `icdp.rs`: integers
//! only, quoted strings without escapes, single-line arrays, `#`
//! comments; unknown sections/keys, missing keys, duplicate keys and
//! out-of-range values are all FATAL (the boot refuses). Descriptors
//! stay strings here — the bin resolves them against the LIVE boot
//! universe (the D-6 truth `icdp.toml` uses) and builds
//! `core_regime::RegimeParams`.
//!
//! **BOOT/OFFLINE DOCTRINE:** runs once at process boot; allocations are
//! fine. Also home of the seed-file reader (`regime-seed.tsv`, plan
//! §4.3) for the same reason.
//!
//! ```toml
//! [refs]        btc = "binance-usdm:btcusdt"   fund = "binance-usdm:btcusdt"
//! [breadth]     members = ["binance-usdm:ethusdt", …]      # ≤ 31, never the ref
//! [hysteresis]  confirm_min = 3
//! [profile.fast]  trend_w_min = 60 … fund_p70_1e9 = 0     # every key of core_regime::ProfileParams
//! [profile.slow]  …
//! [labels.icdp]   off = "soft"   term1 = ["fast:shape:trend"]   # optional, ≤ 4 terms
//! ```

use std::path::Path;

use core_regime::{ProfileParams, REGIME_MAX_MEMBERS};
use core_types::regime::{RegimeLabelBuilder, REGIME_LABEL_TERMS};
use core_types::{RegimeLabelSet, RegimeTerm, REGIME_OFF_HARD, REGIME_OFF_SOFT, REGIME_PROFILES};

use crate::icdp::{parse_value, strip_comment, IcdpError, Value};

/// Parse / load failure (message names the line where possible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegimeConfigError(pub String);

impl ::core::fmt::Display for RegimeConfigError {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        write!(f, "regime.toml: {}", self.0)
    }
}

impl std::error::Error for RegimeConfigError {}

impl From<IcdpError> for RegimeConfigError {
    fn from(e: IcdpError) -> Self {
        Self(e.0)
    }
}

/// One `[labels.<member>]` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelOverride {
    /// The coded member's name (`latency_arb`, `ev`, `cross_arb`,
    /// `rule_tree`, `ai_exec`, `icdp`).
    pub member: String,
    /// The parsed label set.
    pub set: RegimeLabelSet,
}

/// The parsed artifact (descriptors unresolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegimeFile {
    /// TREND / SHAPE / VOL / STRETCH price reference descriptor.
    pub btc: String,
    /// Funding reference descriptor.
    pub fund: String,
    /// Breadth members in file order (`≤ REGIME_MAX_MEMBERS`, unique,
    /// never the ref).
    pub members: Vec<String>,
    /// Confirm law length (≥ 1).
    pub confirm_min: u8,
    /// `[profile.fast]`, `[profile.slow]` — validated by `core_regime`.
    pub profiles: [ProfileParams; REGIME_PROFILES],
    /// `[labels.*]` overrides in file order.
    pub labels: Vec<LabelOverride>,
}

/// Default location beside `universe.toml`.
pub fn default_regime_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/regime.toml")
}

/// Default location of the worker-written seed file.
pub fn default_seed_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/regime-seed.tsv")
}

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT artifact it booted with.
pub fn load(path: &Path) -> Result<(RegimeFile, Vec<u8>), RegimeConfigError> {
    let bytes =
        std::fs::read(path).map_err(|e| RegimeConfigError(format!("{}: {e}", path.display())))?;
    let src = std::str::from_utf8(&bytes)
        .map_err(|_| RegimeConfigError(format!("{}: not UTF-8", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

const PROFILE_KEYS: [&str; 18] = [
    "trend_w_min",
    "shape_w_min",
    "vol_w_min",
    "stretch_w_min",
    "rel_w_min",
    "trend_thr_bps_1e9",
    "breadth_q_1e9",
    "er_lo_enter_1e9",
    "er_lo_exit_1e9",
    "er_hi_enter_1e9",
    "er_hi_exit_1e9",
    "rv_p30_bps_1e9",
    "rv_p70_bps_1e9",
    "stretch_k_1e9",
    "rel_thr_bps_1e9",
    "fund_prints",
    "fund_p30_1e9",
    "fund_p70_1e9",
];

const MEMBER_NAMES: [&str; 6] = [
    "latency_arb",
    "ev",
    "cross_arb",
    "rule_tree",
    "ai_exec",
    "icdp",
];

type Kv = Vec<(String, Value, usize)>;

fn take_int(kv: &Kv, key: &str, sec: &str) -> Result<i64, RegimeConfigError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Int(v), _)) => Ok(*v),
        Some((_, _, l)) => Err(RegimeConfigError(format!(
            "line {l}: `{key}` must be an integer"
        ))),
        None => Err(RegimeConfigError(format!("[{sec}]: missing `{key}`"))),
    }
}

fn take_u16(kv: &Kv, key: &str, sec: &str) -> Result<u16, RegimeConfigError> {
    let v = take_int(kv, key, sec)?;
    u16::try_from(v)
        .map_err(|_| RegimeConfigError(format!("[{sec}]: `{key}` must fit u16, got {v}")))
}

fn take_str(kv: &Kv, key: &str, sec: &str) -> Result<String, RegimeConfigError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Str(v), _)) => Ok(v.clone()),
        Some((_, _, l)) => Err(RegimeConfigError(format!(
            "line {l}: `{key}` must be a string"
        ))),
        None => Err(RegimeConfigError(format!("[{sec}]: missing `{key}`"))),
    }
}

fn take_strs(kv: &Kv, key: &str, sec: &str) -> Result<Vec<String>, RegimeConfigError> {
    match kv.iter().find(|(k, _, _)| k == key) {
        Some((_, Value::Strs(v), _)) => Ok(v.clone()),
        Some((_, Value::Ints(v), _)) if v.is_empty() => Ok(Vec::new()), // `[]`
        Some((_, _, l)) => Err(RegimeConfigError(format!(
            "line {l}: `{key}` must be a string array"
        ))),
        None => Err(RegimeConfigError(format!("[{sec}]: missing `{key}`"))),
    }
}

fn only_keys(kv: &Kv, allowed: &[&str], sec: &str) -> Result<(), RegimeConfigError> {
    for (k, _, l) in kv {
        if !allowed.contains(&k.as_str()) {
            return Err(RegimeConfigError(format!(
                "line {l}: unknown [{sec}] key `{k}`"
            )));
        }
    }
    Ok(())
}

fn finish_profile(kv: &Kv, sec: &str) -> Result<ProfileParams, RegimeConfigError> {
    only_keys(kv, &PROFILE_KEYS, sec)?;
    let pp = ProfileParams::new(
        take_u16(kv, "trend_w_min", sec)?,
        take_u16(kv, "shape_w_min", sec)?,
        take_u16(kv, "vol_w_min", sec)?,
        take_u16(kv, "stretch_w_min", sec)?,
        take_u16(kv, "rel_w_min", sec)?,
        take_u16(kv, "fund_prints", sec)?,
        take_int(kv, "trend_thr_bps_1e9", sec)?,
        take_int(kv, "breadth_q_1e9", sec)?,
        take_int(kv, "er_lo_enter_1e9", sec)?,
        take_int(kv, "er_lo_exit_1e9", sec)?,
        take_int(kv, "er_hi_enter_1e9", sec)?,
        take_int(kv, "er_hi_exit_1e9", sec)?,
        take_int(kv, "rv_p30_bps_1e9", sec)?,
        take_int(kv, "rv_p70_bps_1e9", sec)?,
        take_int(kv, "stretch_k_1e9", sec)?,
        take_int(kv, "rel_thr_bps_1e9", sec)?,
        take_int(kv, "fund_p30_1e9", sec)?,
        take_int(kv, "fund_p70_1e9", sec)?,
    );
    pp.validate().map_err(|e| {
        RegimeConfigError(format!(
            "[{sec}]: {e:?} (core_regime::ProfileParams::validate)"
        ))
    })?;
    Ok(pp)
}

/// Parse a label term list into one `RegimeTerm` (the row/coded grammar).
/// `rel:` terms are refused when `allow_rel` is false (coded members).
pub fn parse_label_terms(
    terms: &[String],
    allow_rel: bool,
) -> Result<RegimeTerm, RegimeConfigError> {
    let mut b = RegimeLabelBuilder::new();
    for t in terms {
        if !allow_rel && t.contains("rel:") {
            return Err(RegimeConfigError(format!(
                "`{t}`: rel: terms are per-symbol (rows only)"
            )));
        }
        b.add(t.as_bytes())
            .map_err(|e| RegimeConfigError(format!("`{t}`: {e:?}")))?;
    }
    Ok(b.finish())
}

fn finish_labels(kv: &Kv, member: &str, ln: usize) -> Result<LabelOverride, RegimeConfigError> {
    let sec = format!("labels.{member}");
    only_keys(kv, &["off", "term1", "term2", "term3", "term4"], &sec)?;
    let off = match take_str(kv, "off", &sec)?.as_str() {
        "soft" => REGIME_OFF_SOFT,
        "hard" => REGIME_OFF_HARD,
        other => {
            return Err(RegimeConfigError(format!(
                "line {ln}: [{sec}] off must be \"soft\" or \"hard\", got `{other}`"
            )))
        }
    };
    let mut terms: Vec<RegimeTerm> = Vec::new();
    for (i, key) in ["term1", "term2", "term3", "term4"].iter().enumerate() {
        let present = kv.iter().any(|(k, _, _)| k == key);
        if !present {
            continue;
        }
        if i != terms.len() {
            return Err(RegimeConfigError(format!(
                "line {ln}: [{sec}] terms must be contiguous from term1"
            )));
        }
        let strs = take_strs(kv, key, &sec)?;
        if strs.is_empty() {
            return Err(RegimeConfigError(format!(
                "line {ln}: [{sec}] {key} is empty"
            )));
        }
        terms.push(parse_label_terms(&strs, false)?);
    }
    if terms.is_empty() {
        return Err(RegimeConfigError(format!(
            "line {ln}: [{sec}] needs at least term1"
        )));
    }
    debug_assert!(terms.len() <= REGIME_LABEL_TERMS);
    let set = RegimeLabelSet::from_terms(&terms, off)
        .ok_or_else(|| RegimeConfigError(format!("line {ln}: [{sec}] too many terms")))?;
    Ok(LabelOverride {
        member: member.to_owned(),
        set,
    })
}

/// Parse the artifact text.
pub fn parse(src: &str) -> Result<RegimeFile, RegimeConfigError> {
    #[derive(Clone, PartialEq, Eq)]
    enum Sec {
        None,
        Refs,
        Breadth,
        Hysteresis,
        Profile(usize, usize),
        Labels(String, usize),
    }
    let mut sec = Sec::None;
    let mut cur: Kv = Vec::new();
    let mut refs: Option<Kv> = None;
    let mut breadth: Option<Kv> = None;
    let mut hyst: Option<Kv> = None;
    let mut profiles: [Option<ProfileParams>; REGIME_PROFILES] = [None; REGIME_PROFILES];
    let mut labels: Vec<LabelOverride> = Vec::new();

    fn close(
        sec: &Sec,
        cur: &mut Kv,
        refs: &mut Option<Kv>,
        breadth: &mut Option<Kv>,
        hyst: &mut Option<Kv>,
        profiles: &mut [Option<ProfileParams>; REGIME_PROFILES],
        labels: &mut Vec<LabelOverride>,
    ) -> Result<(), RegimeConfigError> {
        let kv = std::mem::take(cur);
        match sec {
            Sec::None => {}
            Sec::Refs => *refs = Some(kv),
            Sec::Breadth => *breadth = Some(kv),
            Sec::Hysteresis => *hyst = Some(kv),
            Sec::Profile(p, _) => {
                let name = if *p == 0 {
                    "profile.fast"
                } else {
                    "profile.slow"
                };
                profiles[*p] = Some(finish_profile(&kv, name)?);
            }
            Sec::Labels(member, ln) => labels.push(finish_labels(&kv, member, *ln)?),
        }
        Ok(())
    }

    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let name = inner
                .strip_suffix(']')
                .ok_or_else(|| RegimeConfigError(format!("line {ln}: bad section `{line}`")))?
                .trim();
            close(
                &sec,
                &mut cur,
                &mut refs,
                &mut breadth,
                &mut hyst,
                &mut profiles,
                &mut labels,
            )?;
            sec = match name {
                "refs" if refs.is_none() => Sec::Refs,
                "breadth" if breadth.is_none() => Sec::Breadth,
                "hysteresis" if hyst.is_none() => Sec::Hysteresis,
                "profile.fast" if profiles[0].is_none() => Sec::Profile(0, ln),
                "profile.slow" if profiles[1].is_none() => Sec::Profile(1, ln),
                "refs" | "breadth" | "hysteresis" | "profile.fast" | "profile.slow" => {
                    return Err(RegimeConfigError(format!(
                        "line {ln}: duplicate section [{name}]"
                    )))
                }
                other => match other.strip_prefix("labels.") {
                    Some(member) if MEMBER_NAMES.contains(&member) => {
                        if labels.iter().any(|l| l.member == member) {
                            return Err(RegimeConfigError(format!(
                                "line {ln}: duplicate section [labels.{member}]"
                            )));
                        }
                        Sec::Labels(member.to_owned(), ln)
                    }
                    Some(member) => {
                        return Err(RegimeConfigError(format!(
                            "line {ln}: unknown coded member `{member}` in [labels.{member}]"
                        )))
                    }
                    None => {
                        return Err(RegimeConfigError(format!(
                            "line {ln}: unknown section [{other}]"
                        )))
                    }
                },
            };
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| RegimeConfigError(format!("line {ln}: expected `key = value`")))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(RegimeConfigError(format!("line {ln}: bad key `{key}`")));
        }
        if sec == Sec::None {
            return Err(RegimeConfigError(format!(
                "line {ln}: key outside any section"
            )));
        }
        if cur.iter().any(|(k, _, _)| k == key) {
            return Err(RegimeConfigError(format!(
                "line {ln}: duplicate key `{key}`"
            )));
        }
        cur.push((key.to_owned(), parse_value(val, ln)?, ln));
    }
    close(
        &sec,
        &mut cur,
        &mut refs,
        &mut breadth,
        &mut hyst,
        &mut profiles,
        &mut labels,
    )?;

    let refs = refs.ok_or_else(|| RegimeConfigError("missing [refs]".to_owned()))?;
    only_keys(&refs, &["btc", "fund"], "refs")?;
    let btc = take_str(&refs, "btc", "refs")?;
    let fund = take_str(&refs, "fund", "refs")?;
    if btc.is_empty() || fund.is_empty() {
        return Err(RegimeConfigError("[refs]: empty descriptor".to_owned()));
    }
    let breadth = breadth.ok_or_else(|| RegimeConfigError("missing [breadth]".to_owned()))?;
    only_keys(&breadth, &["members"], "breadth")?;
    let members = take_strs(&breadth, "members", "breadth")?;
    if members.len() > REGIME_MAX_MEMBERS {
        return Err(RegimeConfigError(format!(
            "[breadth]: more than {REGIME_MAX_MEMBERS} members"
        )));
    }
    for (i, m) in members.iter().enumerate() {
        if m.is_empty() {
            return Err(RegimeConfigError(
                "[breadth]: empty member descriptor".to_owned(),
            ));
        }
        if *m == btc {
            return Err(RegimeConfigError(format!(
                "[breadth]: member `{m}` is the btc ref"
            )));
        }
        if members[..i].contains(m) {
            return Err(RegimeConfigError(format!(
                "[breadth]: duplicate member `{m}`"
            )));
        }
    }
    let hyst = hyst.ok_or_else(|| RegimeConfigError("missing [hysteresis]".to_owned()))?;
    only_keys(&hyst, &["confirm_min"], "hysteresis")?;
    let confirm = take_int(&hyst, "confirm_min", "hysteresis")?;
    if !(1..=255).contains(&confirm) {
        return Err(RegimeConfigError(format!(
            "[hysteresis]: confirm_min must be in 1..=255, got {confirm}"
        )));
    }
    let fast = profiles[0].ok_or_else(|| RegimeConfigError("missing [profile.fast]".to_owned()))?;
    let slow = profiles[1].ok_or_else(|| RegimeConfigError("missing [profile.slow]".to_owned()))?;
    Ok(RegimeFile {
        btc,
        fund,
        members,
        confirm_min: confirm as u8,
        profiles: [fast, slow],
        labels,
    })
}

// ---------------------------------------------------------------
// Seed file — `regime-seed.tsv` (plan §4.3)
// ---------------------------------------------------------------

/// One seed row, descriptor unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedLine {
    /// §9.4 descriptor.
    pub descriptor: String,
    /// Wall minute index (`epoch_seconds / 60`).
    pub minute: i64,
    /// Minute close ×1e6 (> 0).
    pub close_1e6: i64,
}

/// Parse the worker-written seed file: `descriptor \t minute \t close_1e6`
/// per line, `#` comments, blank lines ignored. A malformed line is
/// FATAL (the file is generated — a bad line means a bad generator).
pub fn parse_seed(src: &str) -> Result<Vec<SeedLine>, RegimeConfigError> {
    let mut out = Vec::new();
    for (idx, raw) in src.lines().enumerate() {
        let ln = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        let descriptor = it
            .next()
            .ok_or_else(|| RegimeConfigError(format!("seed line {ln}: empty")))?
            .trim()
            .to_owned();
        let minute = it
            .next()
            .ok_or_else(|| RegimeConfigError(format!("seed line {ln}: missing minute")))?;
        let close = it
            .next()
            .ok_or_else(|| RegimeConfigError(format!("seed line {ln}: missing close")))?;
        if it.next().is_some() {
            return Err(RegimeConfigError(format!(
                "seed line {ln}: too many columns"
            )));
        }
        let minute = crate::icdp::parse_int(minute, ln)?;
        let close_1e6 = crate::icdp::parse_int(close, ln)?;
        if descriptor.is_empty() || minute < 0 || close_1e6 <= 0 {
            return Err(RegimeConfigError(format!(
                "seed line {ln}: bad row `{line}`"
            )));
        }
        out.push(SeedLine {
            descriptor,
            minute,
            close_1e6,
        });
    }
    Ok(out)
}

/// Read + parse the seed file.
pub fn load_seed(path: &Path) -> Result<Vec<SeedLine>, RegimeConfigError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| RegimeConfigError(format!("{}: {e}", path.display())))?;
    parse_seed(&src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::regime::{DIM_SHAPE, SHAPE_TREND};

    const EXAMPLE: &str = include_str!("../../../regime.toml.example");

    #[test]
    fn parses_the_committed_example() {
        let f = parse(EXAMPLE).expect("regime.toml.example must parse");
        assert_eq!(f.btc, "binance-usdm:btcusdt");
        assert_eq!(f.fund, "binance-usdm:btcusdt");
        assert_eq!(f.members.len(), 4);
        assert_eq!(f.confirm_min, 3);
        assert_eq!(f.profiles[0], ProfileParams::FAST_DEFAULT);
        assert_eq!(f.profiles[1], ProfileParams::SLOW_DEFAULT);
        assert!(
            f.labels.is_empty(),
            "the example's [labels] block is commented out"
        );
    }

    fn with_labels() -> String {
        format!(
            "{EXAMPLE}\n[labels.icdp]\noff = \"hard\"\nterm1 = [\"fast:shape:trend\", \"slow:trend:bull|neutral\"]\nterm2 = [\"fast:shape:trend\", \"slow:trend:bear\"]\n[labels.latency_arb]\noff = \"soft\"\nterm1 = [\"fast:vol:!high\"]\n"
        )
    }

    #[test]
    fn labels_parse_into_label_sets() {
        let f = parse(&with_labels()).unwrap();
        assert_eq!(f.labels.len(), 2);
        let icdp = &f.labels[0];
        assert_eq!(icdp.member, "icdp");
        assert_eq!(icdp.set.n, 2);
        assert_eq!(icdp.set.off, REGIME_OFF_HARD);
        assert_eq!(
            icdp.set.terms[0].fast.0 >> (8 * DIM_SHAPE as u32) & 0xFF,
            1 << SHAPE_TREND
        );
        assert_eq!(
            icdp.set.terms[1].slow.0 & 0xFF,
            1 << core_types::regime::TREND_BEAR
        );
        let la = &f.labels[1];
        assert_eq!(la.set.n, 1);
        assert_eq!(la.set.off, REGIME_OFF_SOFT);
    }

    fn expect_err(src: &str, needle: &str) {
        match parse(src) {
            Err(e) => assert!(e.0.contains(needle), "got `{}`, wanted `{needle}`", e.0),
            Ok(_) => panic!("expected an error containing `{needle}`"),
        }
    }

    #[test]
    fn refusals() {
        expect_err(
            &EXAMPLE.replace("confirm_min = 3", "confirm_min = 0"),
            "confirm_min",
        );
        expect_err(
            &EXAMPLE.replace("[hysteresis]", "[hysteresis]\nfoo = 1"),
            "unknown [hysteresis] key",
        );
        expect_err(
            &EXAMPLE.replace("trend_w_min = 60\n", "trend_w_min = 60\ntrend_w_min = 61\n"),
            "duplicate key",
        );
        expect_err(
            &EXAMPLE.replace("shape_w_min = 60", "shape_w_min = 61"),
            "Window",
        );
        expect_err(
            &EXAMPLE.replace("[profile.slow]", "[profile.fast]"),
            "duplicate section",
        );
        expect_err(&EXAMPLE.replace("[breadth]", "[bread]"), "unknown section");
        expect_err(
            &EXAMPLE.replace("binance-usdm:ethusdt", "binance-usdm:btcusdt"),
            "is the btc ref",
        );
        expect_err(
            &EXAMPLE.replace("\"binance-usdm:solusdt\"", "\"binance-usdm:ethusdt\""),
            "duplicate member",
        );
        expect_err(
            &EXAMPLE.replace("er_hi_exit_1e9 = 550000000", "er_hi_exit_1e9 = 650000000"),
            "Bands",
        );
        expect_err(
            &EXAMPLE.replacen("[refs]", "[refs]\nextra = \"x\"", 1),
            "unknown [refs] key",
        );
        expect_err(
            &EXAMPLE.replacen("[refs]\n", "", 1),
            "key outside any section",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.mystery]\noff = \"soft\"\nterm1 = [\"fast:vol:low\"]\n"),
            "unknown coded member",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.icdp]\noff = \"maybe\"\nterm1 = [\"fast:vol:low\"]\n"),
            "off must be",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.icdp]\noff = \"soft\"\nterm2 = [\"fast:vol:low\"]\n"),
            "contiguous",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.icdp]\noff = \"soft\"\nterm1 = [\"fast:rel:lagging\"]\n"),
            "rel: terms",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.icdp]\noff = \"soft\"\nterm1 = [\"fast:mood:happy\"]\n"),
            "UnknownDim",
        );
        expect_err(
            &format!("{EXAMPLE}\n[labels.icdp]\noff = \"soft\"\n"),
            "needs at least term1",
        );
        expect_err(&format!("{EXAMPLE}\n[labels.icdp]\noff = \"soft\"\nterm1 = [\"fast:vol:low\"]\n[labels.icdp]\noff = \"soft\"\nterm1 = [\"fast:vol:low\"]\n"), "duplicate section [labels.icdp]");
        let no_profile = EXAMPLE.split("[profile.slow]").next().unwrap().to_owned();
        expect_err(&no_profile, "missing [profile.slow]");
    }

    #[test]
    fn empty_members_is_legal_btc_alone() {
        let src = EXAMPLE.replace(
            "members = [\"binance-usdm:ethusdt\", \"binance-usdm:solusdt\", \"binance-usdm:bnbusdt\", \"binance-usdm:xrpusdt\"]",
            "members = []",
        );
        let f = parse(&src).unwrap();
        assert!(f.members.is_empty());
    }

    #[test]
    fn seed_file_parses_and_refuses_junk() {
        let good = "# descriptor\tminute\tclose_1e6\nbinance-usdm:btcusdt\t29800000\t100000000\n\nbinance-usdm:ethusdt\t29800000\t3000000000\n";
        let rows = parse_seed(good).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].descriptor, "binance-usdm:ethusdt");
        assert_eq!(rows[1].minute, 29_800_000);
        assert_eq!(rows[1].close_1e6, 3_000_000_000);
        assert!(parse_seed("binance-usdm:btcusdt\t29800000\n").is_err());
        assert!(parse_seed("binance-usdm:btcusdt\t29800000\t0\n").is_err());
        assert!(parse_seed("binance-usdm:btcusdt\t-1\t5\n").is_err());
        assert!(parse_seed("binance-usdm:btcusdt\tx\t5\n").is_err());
        assert!(parse_seed("a\t1\t2\t3\n").is_err());
        assert!(parse_seed("\t1\t2\n").is_err());
    }
}
