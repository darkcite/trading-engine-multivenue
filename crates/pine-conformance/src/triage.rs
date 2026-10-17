// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Triage (impl-06 §3.9, §4.6.1–§4.6.3): five buckets, first match wins, pure (I19).
//! Compare-time code: allocation is free here.

use pine_ir::DiagCode;

use crate::fixture::{Align, Driver, Expect, Fixture, Group, Oracle};
use crate::report::{Verdict, NO_ROW};
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

// --------------------------------------------------------------------------------------------
// The TradingView divergence report (R-P1, 2026-10-17: TradingView is the oracle). Compare-time
// code: allocation is free here.
// --------------------------------------------------------------------------------------------

/// One captured fixture's class against TradingView.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TvClass {
    /// Every assertion held.
    Pass,
    /// Both sides produced an outcome of the same kind and it differs (a value, a trade, a
    /// refusal code, a fault kind or row), or TradingView refused / faulted where we ran.
    Differ,
    /// We refused at load (parse, sema, verify, a broker declaration) where TradingView ran.
    RefusedByUs,
    /// We faulted at run time (script or broker) where TradingView ran or refused.
    FaultByUs,
    /// The harness could not judge it (no bars root, a fixture / plot-title error, a skip).
    HarnessGap,
}

/// The class as the report prints it.
pub const fn tv_class_name(c: TvClass) -> &'static str {
    match c {
        TvClass::Pass => "pass",
        TvClass::Differ => "differ",
        TvClass::RefusedByUs => "refused-by-us",
        TvClass::FaultByUs => "fault-by-us",
        TvClass::HarnessGap => "harness-gap",
    }
}

/// The crate most likely at fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Crate {
    /// `pine-lang`.
    Lang,
    /// `pine-sema`.
    Sema,
    /// `pine-std`.
    Std,
    /// `pine-vm` (a CAR under R-P1).
    Vm,
    /// `pine-broker`.
    Broker,
    /// This harness (or the bars it needs).
    Harness,
}

/// The crate as the report prints it.
pub const fn crate_name(c: Crate) -> &'static str {
    match c {
        Crate::Lang => "pine-lang",
        Crate::Sema => "pine-sema",
        Crate::Std => "pine-std",
        Crate::Vm => "pine-vm",
        Crate::Broker => "pine-broker",
        Crate::Harness => "harness",
    }
}

/// One fixture's line in the divergence report.
#[derive(Debug, Clone)]
pub struct TvTriage {
    /// The class.
    pub class: TvClass,
    /// The crate most likely at fault (`Harness` for a pass).
    pub krate: Crate,
    /// The cluster key: fixtures sharing it most likely share one fix.
    pub cluster: String,
    /// The first difference, one line (empty for a pass).
    pub first: String,
    /// A remark (I9, `live_refuse`, Rt bookkeeping columns not compared).
    pub note: Option<String>,
}

/// `pin:06-tv-crate-of-code` — the crate that owns a diagnostic code (contract §3.3 ranges):
/// 1000–2999 pine-lang, 3000–4999 pine-sema (4000 + rule is a verify refusal of what sema
/// emitted), 5001–5099 pine-std, 5100–5199 pine-broker, the rest the harness; `3024`
/// DeferredBuiltin is pine-std's (an unserved body, §4.6.1 `Unimplemented`, owner PVM-4).
pub fn crate_of_code(c: DiagCode) -> Crate {
    match c.0 {
        DEFERRED_BUILTIN => Crate::Std,
        1000..=2999 => Crate::Lang,
        3000..=4999 => Crate::Sema,
        5000..=5099 => Crate::Std,
        5100..=5199 => Crate::Broker,
        _ => Crate::Harness,
    }
}

/// `pin:06-tv-crate-of-value` — a value divergence's crate by group (§4.6.3's owners, one per
/// group as `group_owner` reads them): A, F → pine-vm; B, D → pine-std; C → pine-sema (pine-vm
/// for `Rt`); E → pine-broker; Z → pine-lang.
///
/// A numbered pin `pin:01-*` … `pin:05-*` names its owner first (`pin_owner`, as §4.6.3 reads a pin
/// fixture's owner); `pin:06-*` and lettered pins fall to the group.
fn crate_of_value(f: &Fixture) -> Crate {
    if let Some(c) = pinned_crate(f) {
        return c;
    }
    match group_owner(f) {
        Owner::Pvm1 => Crate::Lang,
        Owner::Pvm2 => Crate::Sema,
        Owner::Pvm3 => Crate::Vm,
        Owner::Pvm4 => Crate::Std,
        Owner::Pvm5 => Crate::Broker,
        Owner::Pvm6 | Owner::Pvm7 | Owner::Operator => Crate::Harness,
    }
}

/// The crate a numbered `pin:01-*` … `pin:05-*` of `f` names.
fn pinned_crate(f: &Fixture) -> Option<Crate> {
    f.pins.iter().find_map(|p| match pin_owner(p) {
        Some(Owner::Pvm1) => Some(Crate::Lang),
        Some(Owner::Pvm2) => Some(Crate::Sema),
        Some(Owner::Pvm3) => Some(Crate::Vm),
        Some(Owner::Pvm4) => Some(Crate::Std),
        Some(Owner::Pvm5) => Some(Crate::Broker),
        _ => None,
    })
}

/// `pin:06-tv-crate-of-fault` — collection bounds and `runtime.error` (ScriptRefused) are pine-std's
/// kernels; every other fault kind is the VM's (budget, arenas, journal, division, casts).
fn crate_of_fault(k: pine_ir::FaultKind) -> Crate {
    match k {
        pine_ir::FaultKind::CollectionBounds | pine_ir::FaultKind::ScriptRefused => Crate::Std,
        _ => Crate::Vm,
    }
}

/// `pin:06-tv-trades-evicted-gap` — `TradesEvicted` is the runner declining to compare a trimmed
/// trade list (pin:05-trade-trim): a harness gap, not a broker fault.
fn is_harness_err(e: &RunError) -> bool {
    matches!(
        e,
        RunError::Fixture(_)
            | RunError::ChartHash { .. }
            | RunError::BarFile { .. }
            | RunError::Skip(_)
            | RunError::TradesEvicted
    )
}

/// What a value cluster is about: the most specific pin (the last one that is not the generic
/// `pin:04-ta-reference`), else a group-B fixture's builtin (its title's first word), else the
/// group.
fn subject(f: &Fixture) -> String {
    if let Some(p) = f
        .pins
        .iter()
        .rev()
        .find(|p| p.as_str() != "pin:04-ta-reference")
    {
        return p.clone();
    }
    if f.id.group() == Group::B {
        if let Some(w) = f.title.split_whitespace().next() {
            return w.split('(').next().unwrap_or(w).to_string();
        }
    }
    format!("group {}", f.id.group().letter())
}

/// The first `Diag` of a refusal as `code @offset: message` (one line, ≤ 140 chars).
fn refusal_text(e: &RunError) -> String {
    let d = match e {
        RunError::Parse(ds) | RunError::Compile(ds) => ds.first(),
        _ => None,
    };
    let s = match d {
        Some(d) => format!("{} @{}: {}", d.code.0, d.span.lo, d.msg),
        None => format!("{e:?}"),
    };
    one_line(&s, 140)
}

fn one_line(s: &str, max: usize) -> String {
    let t: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    if t.chars().count() > max {
        let mut u: String = t.chars().take(max).collect();
        u.push('…');
        u
    } else {
        t
    }
}

fn stage_crate(e: &RunError) -> Crate {
    match e {
        RunError::Parse(_) => Crate::Lang,
        RunError::Verify(_) => Crate::Sema,
        RunError::BrokerDecl(_) | RunError::Broker(_) | RunError::TradesEvicted => Crate::Broker,
        RunError::Fault { kind, .. } => crate_of_fault(*kind),
        _ => e.code().map_or(Crate::Harness, crate_of_code),
    }
}

fn err_key(e: &RunError) -> String {
    match e {
        RunError::Fault { kind, .. } => format!("{kind:?}"),
        RunError::Broker(b) => format!("broker {b:?}"),
        RunError::TradesEvicted => "TradesEvicted".into(),
        RunError::Verify(v) => format!("verify rule {}", v.rule),
        _ => e
            .code()
            .map_or_else(|| format!("{e:?}"), |c| c.0.to_string()),
    }
}

/// The value cell's first difference as `row K · <title> · ours A vs TV E`.
fn value_first(f: &Fixture, d: &crate::report::Divergence, o: &RunOutcome) -> String {
    let (Some(p), Some(k)) = (d.plot, d.first_out_of_bound) else {
        return d
            .error
            .as_deref()
            .map_or_else(|| "value divergence".into(), |e| one_line(e, 160));
    };
    let title = f.plots.get(p).map_or("?", |s| s.title.as_str());
    let exp = expected_rows(f)
        .and_then(|rows| rows.iter().find(|r| r.key == k))
        .and_then(|r| r.cells.get(p))
        .map_or("?".to_string(), |c| match c {
            crate::fixture::Cell::Na => "na".into(),
            crate::fixture::Cell::Val { text, .. } => text.clone(),
        });
    let ours = match crate::sig::actual_at(o, k, p) {
        Some(a) if a.is_nan() => "na".to_string(),
        Some(a) => format!("{a:?}"),
        None => "(absent)".into(),
    };
    format!("row {k} · {title} · ours {ours} vs TV {exp}")
}

/// R-P1 triage of one captured fixture from [`crate::run::Runner::check_full`]'s result.
pub fn classify_tv(f: &Fixture, c: &crate::run::Checked) -> TvTriage {
    let mk = |class, krate, cluster: String, first: String| TvTriage {
        class,
        krate,
        cluster,
        first,
        note: None,
    };
    let rt_note = (0..f.plots.len())
        .any(|p| crate::run::is_rt_bookkeeping(f, p))
        .then(|| "Rt series: bookkeeping columns (rt, done, upd, k) not compared".to_string());
    let with_note = |mut t: TvTriage, extra: Option<String>| {
        t.note = match (rt_note.clone(), extra) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        t
    };
    // A Parse expectation (group Z): no run, the accept bit.
    let Some(r) = &c.primary else {
        return match &c.expectation {
            Verdict::Pass => mk(TvClass::Pass, Crate::Harness, String::new(), String::new()),
            Verdict::Fail(d) => {
                let e = d.error.clone().unwrap_or_default();
                if e.starts_with("parse accept = false") {
                    mk(
                        TvClass::RefusedByUs,
                        Crate::Lang,
                        "refused-by-us · pine-lang · parse rejects what TradingView accepts".into(),
                        e,
                    )
                } else if e.starts_with("parse accept = true") {
                    mk(
                        TvClass::Differ,
                        Crate::Lang,
                        "differ · pine-lang · parse accepts what TradingView rejects".into(),
                        e,
                    )
                } else {
                    mk(
                        TvClass::HarnessGap,
                        Crate::Harness,
                        "harness-gap · parse fixture not runnable".into(),
                        one_line(&e, 160),
                    )
                }
            }
            v => mk(
                TvClass::HarnessGap,
                Crate::Harness,
                "harness-gap · parse fixture not judged".into(),
                format!("{v:?}"),
            ),
        };
    };
    // Why the full verdict is not the expectation's (I9, live_refuse).
    let extra = match (&c.verdict, &c.expectation) {
        (Verdict::DialectDivergence { first_row, plot }, _) => Some(if *first_row == NO_ROW {
            "our v5 ≢ v6 (I9): outcomes differ".to_string()
        } else {
            format!("our v5 ≢ v6 (I9) at row {first_row}, plot {plot}")
        }),
        (Verdict::Fail(d), Verdict::Pass) => d.error.clone(),
        _ => None,
    };
    // The harness could not judge it.
    if let Err(e) = r {
        if is_harness_err(e) {
            let key = match e {
                RunError::Skip(s) => format!("{s:?}"),
                RunError::Fixture(fe) if fe.what.starts_with("plot title") => {
                    "plot title not found in the program".into()
                }
                RunError::Fixture(_) => "fixture refused (7003)".into(),
                RunError::ChartHash { .. } => "chart hash (7004)".into(),
                RunError::TradesEvicted => {
                    "trade list trimmed, not compared (pin:05-trade-trim)".into()
                }
                _ => "bar file (7001)".into(),
            };
            return mk(
                TvClass::HarnessGap,
                Crate::Harness,
                format!("harness-gap · {key}"),
                one_line(&format!("{e:?}"), 160),
            );
        }
    }
    if let Verdict::Pass = c.expectation {
        return match extra {
            Some(x) => with_note(
                mk(
                    TvClass::Differ,
                    Crate::Sema,
                    "differ · pine-sema · v5/v6 or live-bound outcome (I9 / I12)".into(),
                    x,
                ),
                None,
            ),
            None => with_note(
                mk(TvClass::Pass, Crate::Harness, String::new(), String::new()),
                None,
            ),
        };
    }
    let t = match (&f.expect, r) {
        (Expect::Refuse { code }, Err(e)) => {
            let k = crate_of_code(*code);
            let class = if matches!(e, RunError::Fault { .. } | RunError::Broker(_)) {
                TvClass::FaultByUs
            } else {
                TvClass::Differ
            };
            mk(
                class,
                k,
                format!(
                    "{} · {} · TV refuses {} · ours {}",
                    tv_class_name(class),
                    crate_name(k),
                    code.0,
                    err_key(e)
                ),
                format!("TV refuses {} · ours {}", code.0, refusal_text(e)),
            )
        }
        (Expect::Refuse { code }, Ok(_)) => {
            let k = crate_of_code(*code);
            mk(
                TvClass::Differ,
                k,
                format!(
                    "differ · {} · TV refuses {} · ours runs",
                    crate_name(k),
                    code.0
                ),
                format!("TV refuses {} · ours ran to the end", code.0),
            )
        }
        (Expect::Fault { kind, row }, Err(e @ RunError::Fault { kind: k, .. })) => {
            let kc = pinned_crate(f).unwrap_or_else(|| crate_of_fault(*kind));
            mk(
                TvClass::Differ,
                kc,
                format!(
                    "differ · {} · TV faults {kind:?} · ours {k:?}",
                    crate_name(kc)
                ),
                format!(
                    "TV faults {kind:?} on row {row} · ours {} on row {:?}",
                    err_key(e),
                    c.fault_row
                ),
            )
        }
        (Expect::Fault { kind, row }, Ok(_)) => {
            let kc = pinned_crate(f).unwrap_or_else(|| crate_of_fault(*kind));
            mk(
                TvClass::Differ,
                kc,
                format!(
                    "differ · {} · TV faults {kind:?} · ours runs",
                    crate_name(kc)
                ),
                format!("TV faults {kind:?} on row {row} · ours ran to the end"),
            )
        }
        (_, Err(e @ (RunError::Fault { .. } | RunError::Broker(_) | RunError::TradesEvicted))) => {
            let k = match e {
                RunError::Fault { kind, .. } if f.id.group() == Group::E => {
                    if crate_of_fault(*kind) == Crate::Std {
                        Crate::Std
                    } else {
                        Crate::Broker
                    }
                }
                _ => stage_crate(e),
            };
            mk(
                TvClass::FaultByUs,
                k,
                format!("fault-by-us · {} · {}", crate_name(k), err_key(e)),
                format!("{e:?} at row {:?}", c.fault_row),
            )
        }
        (_, Err(e)) => {
            let k = stage_crate(e);
            mk(
                TvClass::RefusedByUs,
                k,
                format!("refused-by-us · {} · {}", crate_name(k), err_key(e)),
                refusal_text(e),
            )
        }
        // pin:06-tv-rt-strategy-gap retired r27b (CI-030): an `Rt` strategy runs with a `TvBroker`
        // (pin:06-rt-broker, §4.5.3 amended), so E106 is judged like any strategy.
        (_, Ok(o)) => match &c.expectation {
            Verdict::Fail(d) if d.trade.is_some() => {
                let tk = match &d.trade {
                    Some(crate::trades::TradeDiv::Count { .. }) => "trade count".to_string(),
                    Some(crate::trades::TradeDiv::Field { field, .. }) => {
                        format!("trade field {field}")
                    }
                    None => unreachable!("is_some above"),
                };
                mk(
                    TvClass::Differ,
                    Crate::Broker,
                    format!("differ · pine-broker · {tk}"),
                    one_line(&format!("{:?}", d.trade.as_ref().expect("is_some")), 200),
                )
            }
            Verdict::Fail(d) => {
                let k = crate_of_value(f);
                let sig = d
                    .plot
                    .zip(expected_rows(f))
                    .and_then(|(p, rows)| match_signature(f, rows, o, p))
                    .map_or("value", crate::sig::kind_name);
                mk(
                    TvClass::Differ,
                    k,
                    format!("differ · {} · {} · {sig}", crate_name(k), subject(f)),
                    value_first(f, d, o),
                )
            }
            v => mk(
                TvClass::HarnessGap,
                Crate::Harness,
                "harness-gap · verdict not judged".into(),
                format!("{v:?}"),
            ),
        },
    };
    with_note(t, extra)
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
