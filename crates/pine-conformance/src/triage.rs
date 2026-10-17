// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Triage (impl-06 §3.9, §4.6.1–§4.6.3): five buckets, first match wins, pure (I19).
//! Compare-time code: allocation is free here.

use pine_ir::DiagCode;

use crate::fixture::{Align, Driver, Expect, Fixture, Group, Oracle};
use crate::report::Verdict;
use crate::run::{RunError, RunOutcome, SkipReason};
use crate::sig::{match_signature, match_trade_signature, Kind};

/// The five buckets, in §4.6.1's evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Bucket {
    /// `RunError::ChartHash` (7004) — before running.
    ChartDrift = 0,
    /// `Skip(TierInactive)` or a first refusal code `3024` (DeferredBuiltin) — before running.
    Unimplemented = 1,
    /// An `oracle declined` fixture, or a `tv` fixture refused with a by-design code (§4.6.2).
    Declined = 2,
    /// `align time` and the bar-identity checkpoints disagree with `barhash.tsv`.
    DataRevision = 3,
    /// Everything else that diverges.
    SemanticBug = 4,
}

/// Who fixes it (§4.6.3 **Owner**).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// `pine-lang`.
    Pvm1,
    /// `pine-sema`.
    Pvm2,
    /// `pine-vm`.
    Pvm3,
    /// `pine-std`.
    Pvm4,
    /// `pine-broker`.
    Pvm5,
    /// This harness.
    Pvm6,
    /// The preparer / `core_time::barclock`.
    Pvm7,
    /// The operator (chart drift, data revision, impl-00 pins).
    Operator,
}

/// The owner as the report prints it.
pub const fn owner_name(o: Owner) -> &'static str {
    match o {
        Owner::Pvm1 => "PVM-1",
        Owner::Pvm2 => "PVM-2",
        Owner::Pvm3 => "PVM-3",
        Owner::Pvm4 => "PVM-4",
        Owner::Pvm5 => "PVM-5",
        Owner::Pvm6 => "PVM-6",
        Owner::Pvm7 => "PVM-7",
        Owner::Operator => "operator",
    }
}

/// The bucket as the report prints it.
pub const fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::ChartDrift => "ChartDrift",
        Bucket::Unimplemented => "Unimplemented",
        Bucket::Declined => "Declined",
        Bucket::DataRevision => "DataRevision",
        Bucket::SemanticBug => "SemanticBug",
    }
}

/// One fixture's triage.
#[derive(Debug, Clone)]
pub struct Triage {
    /// The bucket.
    pub bucket: Bucket,
    /// Who fixes it.
    pub owner: Owner,
    /// The failure shape, when one matched.
    pub signature: Option<Kind>,
    /// One line of why.
    pub detail: String,
}

/// What triage reads besides the fixture and its run.
pub struct TriageEnv {
    /// `pine_spec::chart::SHA256`.
    pub chart_sha256: [u8; 32],
    /// The bar-identity checkpoints against `barhash.tsv` (§4.7.4); `None` = not computed.
    pub barhash_ok: Option<bool>,
    /// `CompileLimits::active_tiers`.
    pub active_tiers: u8,
}

/// `3024` DeferredBuiltin.
pub const DEFERRED_BUILTIN: u16 = 3024;

/// §4.6.2's by-design refusal codes (inclusive ranges). `3025` and `3033` belong to no set.
pub const DECLINED_CODES: [(u16, u16); 6] = [
    (3016, 3023),
    (3026, 3032),
    (3042, 3053),
    (5001, 5001),
    (5002, 5002),
    (5100, 5109),
];

/// A by-design refusal code (§4.6.2).
pub fn is_declined_code(c: DiagCode) -> bool {
    DECLINED_CODES
        .iter()
        .any(|(lo, hi)| (*lo..=*hi).contains(&c.0))
}

/// `pin:06-triage-pin-owner` — "a pin fixture's owner is the pin's switch owner": `pin:NN-*` is
/// impl-NN's switch, PVM-N; `pin:00-*` (impl-00's table) is the operator's.
fn pin_owner(pin: &str) -> Option<Owner> {
    let nn = pin.strip_prefix("pin:")?.get(..2)?;
    Some(match nn {
        "00" => Owner::Operator,
        "01" => Owner::Pvm1,
        "02" => Owner::Pvm2,
        "03" => Owner::Pvm3,
        "04" => Owner::Pvm4,
        "05" => Owner::Pvm5,
        "06" => Owner::Pvm6,
        "07" => Owner::Pvm7,
        _ => return None,
    })
}

/// §4.6.3's owner of a value divergence, by group.
///
/// `pin:06-triage-group-owner` — where §4.6.3 offers two owners "by the kernel's operation" the
/// harness cannot see the kernel: A and F → PVM-3 (the ISA), C → PVM-3 for a realtime (`Rt`,
/// journal / `varip`) fixture else PVM-2, D → PVM-4 (alignment).
fn group_owner(f: &Fixture) -> Owner {
    match f.id.group() {
        Group::A | Group::F => Owner::Pvm3,
        Group::B => Owner::Pvm4,
        Group::C if f.driver == Driver::Rt => Owner::Pvm3,
        Group::C => Owner::Pvm2,
        Group::D => Owner::Pvm4,
        Group::E => Owner::Pvm5,
        Group::Z => Owner::Pvm1,
    }
}

/// Stage → owner for a run error (§4.6.3).
fn error_owner(f: &Fixture, e: &RunError) -> Owner {
    match e {
        RunError::Parse(_) => Owner::Pvm1,
        RunError::Compile(_) => match e.code().map(|c| c.0) {
            Some(5001..=5007) => Owner::Pvm4,
            _ => Owner::Pvm2,
        },
        RunError::Verify(_) => Owner::Pvm2,
        RunError::BrokerDecl(_) | RunError::Broker(_) | RunError::TradesEvicted => Owner::Pvm5,
        RunError::Fixture(_) | RunError::BarFile { .. } | RunError::Skip(_) => Owner::Pvm6,
        RunError::ChartHash { .. } => Owner::Operator,
        RunError::Fault { .. } => group_owner(f),
    }
}

/// The expected series rows of `f`, if its expectation has any.
fn expected_rows(f: &Fixture) -> Option<&[crate::fixture::Row]> {
    match &f.expect {
        Expect::Series(rows) => Some(rows),
        Expect::Strategy { series, .. } => Some(series),
        _ => None,
    }
}

/// Pure; first match wins in §4.6.1's order.
///
/// `pin:06-triage-non-failure` — §4.6.1 buckets divergences: a `Pass` / `Pending` verdict, or a skip
/// other than `TierInactive` (counted, I20), falls to `SemanticBug` with no signature and a detail
/// that says so; callers report only failures.
pub fn triage(
    f: &Fixture,
    r: &Result<RunOutcome, RunError>,
    v: &Verdict,
    env: &TriageEnv,
) -> Triage {
    let mk = |bucket, owner, signature, detail: String| Triage {
        bucket,
        owner,
        signature,
        detail,
    };
    // 1. ChartDrift.
    if matches!(r, Err(RunError::ChartHash { .. })) || f.chart_sha256 != env.chart_sha256 {
        return mk(
            Bucket::ChartDrift,
            Owner::Operator,
            None,
            "chart_sha256 differs from chart.json (7004)".into(),
        );
    }
    // 2. Unimplemented.
    // Tier `t` is bit `t − 1` of `active_tiers` (as `Runner::run_with` reads it).
    let tier_bit = if (1..=8).contains(&f.tier) {
        1u8 << (f.tier - 1)
    } else {
        0
    };
    let skip_tier = matches!(r, Err(RunError::Skip(SkipReason::TierInactive { .. })))
        || matches!(v, Verdict::Skipped(SkipReason::TierInactive { .. }));
    if skip_tier || tier_bit & env.active_tiers == 0 {
        return mk(
            Bucket::Unimplemented,
            Owner::Pvm4,
            None,
            format!("tier {} inactive", f.tier),
        );
    }
    if let Err(e) = r {
        if e.code().map(|c| c.0) == Some(DEFERRED_BUILTIN) {
            return mk(
                Bucket::Unimplemented,
                Owner::Pvm4,
                None,
                "refused 3024 DeferredBuiltin".into(),
            );
        }
    }
    // 3. Declined.
    if f.oracle == Oracle::Declined {
        return mk(
            Bucket::Declined,
            Owner::Pvm6,
            None,
            "oracle declined".into(),
        );
    }
    if f.oracle.is_captured_kind() {
        if let Err(e) = r {
            if let Some(c) = e.code().filter(|c| is_declined_code(*c)) {
                return mk(
                    Bucket::Declined,
                    Owner::Pvm6,
                    None,
                    format!(
                        "refused by design {}: reclassify in the generator table",
                        c.0
                    ),
                );
            }
        }
    }
    // 4. DataRevision (a Syn / Rt fixture reads no chart bar).
    let reads_chart = matches!(f.driver, Driver::Chart | Driver::SynSig);
    if f.align == Align::Time && reads_chart && env.barhash_ok == Some(false) {
        return mk(
            Bucket::DataRevision,
            Owner::Operator,
            None,
            "bar-identity checkpoints differ from barhash.tsv".into(),
        );
    }
    // 5. SemanticBug.
    let pin = f.pins.iter().find_map(|p| pin_owner(p));
    let owner_of_value = || pin.unwrap_or_else(|| group_owner(f));
    match (v, r) {
        (Verdict::DialectDivergence { .. }, _) => mk(
            Bucket::SemanticBug,
            Owner::Pvm2,
            Some(Kind::DialectDivergence),
            "v5 and v6 outcomes differ".into(),
        ),
        (Verdict::Fail(d), Ok(o)) => {
            let sig = match (d.plot, expected_rows(f), &d.trade, &f.expect) {
                (Some(p), Some(rows), _, _) => match_signature(f, rows, o, p),
                (_, _, Some(_), Expect::Strategy { trades, .. }) => {
                    match_trade_signature(trades, &o.trades, bar_ms(f), None)
                }
                _ => None,
            };
            mk(
                Bucket::SemanticBug,
                owner_of_value(),
                sig,
                d.error.clone().unwrap_or_else(|| "value divergence".into()),
            )
        }
        (Verdict::Fail(d), Err(e)) => mk(
            Bucket::SemanticBug,
            pin.unwrap_or_else(|| error_owner(f, e)),
            None,
            d.error.clone().unwrap_or_else(|| format!("{e:?}")),
        ),
        (Verdict::Skipped(s), _) => mk(
            Bucket::SemanticBug,
            Owner::Pvm6,
            None,
            format!("skipped: {s:?} (counted, not triaged)"),
        ),
        (Verdict::Pass | Verdict::Pending, _) => mk(
            Bucket::SemanticBug,
            owner_of_value(),
            None,
            "no divergence".into(),
        ),
    }
}

/// `pin:06-triage-bar-ms` — the chart bar's duration for `TradeTimeShift`: the chart timeframe
/// (`pine_spec::chart::TF_MS`) for a chart / `SynSig` fixture, one minute for `Rt`, and one step
/// (`1`, the row key unit) for `Syn`.
fn bar_ms(f: &Fixture) -> i64 {
    match f.driver {
        Driver::Chart | Driver::SynSig => crate::chart::spec::TF_MS,
        Driver::Rt => i64::from(crate::run::RT_TIMEFRAME_S) * 1000,
        Driver::Syn => 1,
    }
}
