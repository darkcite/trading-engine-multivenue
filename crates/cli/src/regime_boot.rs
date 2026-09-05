// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # regime_boot — `regime.toml` + `regime-seed.tsv` → detector params
//!
//! One resolver for the two consumers of the regime artifact:
//!
//! * the ENGINE boot (`multivenue-engine run`, RG2): every descriptor
//!   must resolve against the live boot universe — an unresolvable
//!   reference or member REFUSES the boot (`docs/regime-and-dashboard-plan.md`
//!   §4.6: unknown/missing keys are fatal, and so is a member the
//!   engine cannot feed);
//! * the HARNESS (`backtest` / `audit-pnl`, RG3 §4.8): the same file
//!   against a capture root's manifest, where an OLD root may predate a
//!   member — members that do not resolve are DROPPED with a count
//!   (`drop_unresolved_members = true`), the references must resolve.
//!
//! BOOT/OFFLINE DOCTRINE: runs once per boot / per harness invocation;
//! allocation is fine.

use std::path::Path;

use core_config::regime::{RegimeFile, SeedLine};
use core_regime::{RegimeParams, SeedRow, REGIME_MAX_MEMBERS};
use core_types::{RegimeLabelSet, SymbolId, SYMBOL_ID_NONE};
use tracing::info;

use crate::paper::RegimeBoot;

/// A parsed + resolved artifact (params, coded-member label overrides,
/// the artifact hash) plus what the resolver dropped.
pub struct ResolvedRegime {
    /// Validated detector parameters (descriptors → `SymbolId`s).
    pub params: RegimeParams,
    /// `[labels.<member>]` overrides as `(slot, set)`.
    pub labels: Vec<(u8, RegimeLabelSet)>,
    /// SHA-256 of the exact artifact bytes.
    pub hash: [u8; 32],
    /// Members that did not resolve (only with `drop_unresolved_members`).
    pub members_dropped: usize,
    /// RG8: `[labels] require = 1` — the set builder refuses an enabled
    /// signal-carrying coded member whose label is ANY.
    pub require_labels: bool,
}

/// Map a `[labels.<member>]` name to its strategy-set slot.
fn coded_member_slot(name: &str) -> Option<u8> {
    Some(match name {
        "latency_arb" => strategy_set::SLOT_LATENCY_ARB,
        "ev" => strategy_set::SLOT_EV,
        "cross_arb" => strategy_set::SLOT_CROSS_ARB,
        "rule_tree" => strategy_set::SLOT_RULE_TREE,
        "ai_exec" => strategy_set::SLOT_AI_EXEC,
        "icdp" => strategy_set::SLOT_ICDP,
        _ => return None,
    })
}

/// Resolve a parsed artifact through `resolve` (descriptor → sym).
/// References must resolve; members follow `drop_unresolved_members`.
pub fn resolve_regime_file(
    file: &RegimeFile,
    bytes: &[u8],
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    drop_unresolved_members: bool,
) -> Result<ResolvedRegime, String> {
    let must = |d: &str| -> Result<SymbolId, String> {
        resolve(d).ok_or_else(|| format!("regime: `{d}` is not in the universe"))
    };
    let btc = must(&file.btc)?;
    let fund = must(&file.fund)?;
    let mut members = [SYMBOL_ID_NONE; REGIME_MAX_MEMBERS];
    let mut n = 0usize;
    let mut members_dropped = 0usize;
    for m in &file.members {
        match resolve(m) {
            Some(sym) => {
                if n >= REGIME_MAX_MEMBERS {
                    return Err(format!("regime: more than {REGIME_MAX_MEMBERS} members"));
                }
                members[n] = sym;
                n += 1;
                info!(descriptor = %m, sym, "regime: member resolved");
            }
            None if drop_unresolved_members => {
                members_dropped += 1;
                info!(descriptor = %m, "regime: member absent from this root — dropped");
            }
            None => return Err(format!("regime: `{m}` is not in the universe")),
        }
    }
    let params = RegimeParams::new(btc, fund, members, n as u8, file.confirm_min, file.profiles);
    params.validate().map_err(|e| format!("regime: {e:?}"))?;
    let mut labels = Vec::with_capacity(file.labels.len());
    for l in &file.labels {
        let slot = coded_member_slot(&l.member)
            .ok_or_else(|| format!("regime: unknown coded member `{}`", l.member))?;
        labels.push((slot, l.set));
    }
    Ok(ResolvedRegime {
        params,
        labels,
        hash: core_crypto::sha256(bytes),
        members_dropped,
        require_labels: file.require_labels,
    })
}

/// Seed rows for the artifact's reference + members; rows for other
/// descriptors are dropped and counted (the worker exports the
/// universe's 1 m closes generously).
pub fn seed_rows_for(
    lines: &[SeedLine],
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    params: &RegimeParams,
) -> (Vec<SeedRow>, usize) {
    let members = &params.members[..params.n_members as usize];
    let mut seed = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for l in lines {
        match resolve(&l.descriptor) {
            Some(sym) if sym == params.btc_ref || members.contains(&sym) => {
                seed.push(SeedRow::new(sym, l.minute, l.close_1e6));
            }
            _ => dropped += 1,
        }
    }
    (seed, dropped)
}

/// ENGINE boot (RG2): read `regime.toml` (+ the seed file), resolve
/// every descriptor against the boot universe, build the detector's
/// parameters and the label overrides. `Ok(None)` = the DEFAULT
/// artifact is absent (detector stays unconfigured); an explicit
/// `--regime` path must exist; any unresolved descriptor refuses.
pub fn load_regime_boot(
    path: Option<&Path>,
    seed_path: Option<&Path>,
    descriptors: &ingress_ai::DescriptorTable,
) -> Result<Option<RegimeBoot>, String> {
    let explicit = path.is_some();
    let owned;
    let path: &Path = match path {
        Some(p) => p,
        None => {
            owned = core_config::regime::default_regime_path().map_err(|e| e.to_string())?;
            Path::new(&owned)
        }
    };
    if !explicit && !path.exists() {
        return Ok(None);
    }
    let (file, bytes) = core_config::regime::load(path).map_err(|e| e.to_string())?;
    let resolve = |d: &str| descriptors.resolve(d.as_bytes()).map(|(sym, _caps)| sym);
    let resolved = resolve_regime_file(&file, &bytes, &resolve, false)?;
    // Seed: absent file = warm live; present = every row of a member.
    let seed_owned;
    let seed_path: &Path = match seed_path {
        Some(p) => p,
        None => {
            seed_owned = core_config::regime::default_seed_path().map_err(|e| e.to_string())?;
            Path::new(&seed_owned)
        }
    };
    let mut seed = Vec::new();
    if seed_path.exists() {
        let lines = core_config::regime::load_seed(seed_path).map_err(|e| e.to_string())?;
        let (rows, dropped) = seed_rows_for(&lines, &resolve, &resolved.params);
        seed = rows;
        info!(
            path = %seed_path.display(),
            rows = seed.len(),
            dropped,
            "regime: seed file read"
        );
    }
    Ok(Some(RegimeBoot {
        params: resolved.params,
        labels: resolved.labels,
        seed,
        hash: resolved.hash,
        require_labels: resolved.require_labels,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_regime::ProfileParams;
    use core_types::{make_symbol_id, VenueId};

    fn file(members: &[&str]) -> RegimeFile {
        RegimeFile {
            btc: "binance-usdm:btcusdt".to_owned(),
            fund: "binance-usdm:btcusdt".to_owned(),
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            confirm_min: 3,
            profiles: [ProfileParams::FAST_DEFAULT, ProfileParams::SLOW_DEFAULT],
            labels: Vec::new(),
            require_labels: false,
        }
    }

    fn resolver(d: &str) -> Option<SymbolId> {
        match d {
            "binance-usdm:btcusdt" => Some(make_symbol_id(VenueId::Binance, 512)),
            "binance-usdm:ethusdt" => Some(make_symbol_id(VenueId::Binance, 513)),
            _ => None,
        }
    }

    #[test]
    fn strict_resolution_refuses_an_unknown_member_and_lenient_drops_it() {
        let f = file(&["binance-usdm:ethusdt", "binance-usdm:dogeusdt"]);
        let err = resolve_regime_file(&f, b"x", &resolver, false)
            .err()
            .expect("refused");
        assert!(err.contains("dogeusdt"), "{err}");
        let ok = resolve_regime_file(&f, b"x", &resolver, true).expect("lenient");
        assert_eq!(ok.params.n_members, 1);
        assert_eq!(ok.params.members[0], make_symbol_id(VenueId::Binance, 513));
        assert_eq!(ok.members_dropped, 1);
        assert_eq!(ok.hash, core_crypto::sha256(b"x"));
        // The references must resolve in BOTH modes.
        let mut bad = file(&[]);
        bad.fund = "okx:BTC-USDT-SWAP".to_owned();
        assert!(resolve_regime_file(&bad, b"x", &resolver, true).is_err());
    }

    #[test]
    fn seed_rows_keep_only_the_reference_and_members() {
        let f = file(&["binance-usdm:ethusdt"]);
        let r = resolve_regime_file(&f, b"x", &resolver, false).unwrap();
        let lines = vec![
            SeedLine {
                descriptor: "binance-usdm:btcusdt".to_owned(),
                minute: 10,
                close_1e6: 100,
            },
            SeedLine {
                descriptor: "binance-usdm:ethusdt".to_owned(),
                minute: 10,
                close_1e6: 200,
            },
            SeedLine {
                descriptor: "binance-usdm:solusdt".to_owned(),
                minute: 10,
                close_1e6: 300,
            },
        ];
        let (rows, dropped) = seed_rows_for(&lines, &resolver, &r.params);
        assert_eq!(rows.len(), 2);
        assert_eq!(dropped, 1);
        assert_eq!(rows[1].close_1e6, 200);
    }

    #[test]
    fn unknown_coded_member_label_refuses() {
        let mut f = file(&[]);
        f.labels.push(core_config::regime::LabelOverride {
            member: "nope".to_owned(),
            set: RegimeLabelSet::ANY,
        });
        assert!(resolve_regime_file(&f, b"x", &resolver, false).is_err());
        f.labels[0].member = "icdp".to_owned();
        let r = resolve_regime_file(&f, b"x", &resolver, false).unwrap();
        assert_eq!(r.labels[0].0, strategy_set::SLOT_ICDP);
    }
}
