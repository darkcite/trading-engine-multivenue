// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline harness (B'3 T13, impl-Bp §3.6, §4.5). Allocation
//! and copying are free here: the boot runs once before the first record and
//! the report once after the last.
//!
//! `backtest --strategy pine` end to end: the plan (`--cut-plan`, or one cut
//! per `run-*` of `--replay-dir`), the window and spread checks, the model,
//! the manifest (one script), the first-cut symbol space, the route, the fee
//! gate, `StrategySet(BIT_PINE)` configured through impl-08's `PineBootCfg`
//! (R5) with `HostKind::Replay`, the drive, and the `pvm-t2-1` report.
//!
//! Literal readings:
//! - pin:Bp-screen-universe-flag (lead ruling) — the replay facts come from
//!   `--screen-universe` (default `~/multivenue/pine/screen-universe.tsv`);
//!   a missing or unreadable file refuses, exit 6.
//! - pin:Bp-replay-key-capture-descriptor — a canonical key resolves through
//!   its screen-universe row's `capture_descriptor` to the FIRST cut's
//!   `instrument-manifest.tsv` ordinal (§4.5 "Symbols across cuts"); a key
//!   without a row, a row without a capture descriptor, or a descriptor cut 0
//!   lacks refuses (exit 6). A cut 0 without a manifest cannot fix the route:
//!   refused too.
//! - pin:Bp-replay-dir-plan — `--replay-dir` is `CutSource::Local`: one cut
//!   per `run-*`, epoch order, `[0, last − first + 1)` of its own span.
//! - pin:Bp-window-default-days — without `--window-ms` the window is the
//!   whole UTC days the plan spans.
//! - pin:Bp-slot-caps-manifest — `SlotCaps8` = the script's own caps
//!   (`cap_instance`, `cap_day`, `max_open`); `max_order` unset (0) — §4.5
//!   "the manifest's own caps, so they never bind before the script's".
//! - pin:Bp-boot-at-first-cut — `boot_utc_ms` = cut 0's first wall ms; the
//!   oid epoch its UTC second (≥ 1), the paper rule
//!   (pin:08-oid-epoch-boot-seconds); the anchor is the identity (the drive
//!   hands the member wall ns).
//! - pin:Bp-warmup-cold-until-flag — no `--warmup-bars-root` yet: every row
//!   boots cold (no slice), impl-08 §4.3.6's legal reading of "no bars".
//! - pin:Bp-null-arms-t16 — `--null-arm` / seeds are parsed and validated;
//!   the null arms run from T16; the report says `null_arms_run: 0`.
//! - pin:Bp-gate-t18 — exit 3 (UNJUDGED) is T18's; a judged run exits 0.

use std::path::{Path, PathBuf};

use core_config::pine::{PineManifest, PineSlotCfg};
use core_types::SymbolId;
use strategy_core::Strategy;
use strategy_pine::{FeeModel, HostKind, PineBootCfg, SlotCaps8, SlotRoute};
use strategy_set::{StrategySet, BIT_PINE};

use super::arm::{one_script, EXIT_PRE_RECORD};
use super::cuts::{
    boundary_virt_ns, check_plan, parse_cut_plan, span_prepass, CutPlan, CutRoot, CutRow,
    CutSource, LocalSource, SplitLaw,
};
use super::drive::{drive_cuts, DriveCfg, DriveFail, DriveOut, PineCtx, Residency};
use super::fees::{fee_provenance, refuse_inside_git, FeeLedger, FeeSym};
use super::spool::SpoolSource;
use crate::backtest::fill::{FillEngine, MAX_OPEN_TOTAL};
use crate::backtest::keep::KeepPolicy;
use crate::backtest::{
    discover_runs, parse_model_params, parse_split, read_manifest_rows, HarnessError, MergedRec,
    RunDir,
};
use crate::pine::universe::{load_universe, UniverseRow};

const DAY_MS: i64 = 86_400_000;

/// The `backtest --strategy pine` argv, as the binary parsed it (§3.6).
#[derive(Debug, Clone, Default)]
pub struct PineArgv {
    /// `--pine-manifest`: the 00-INDEX §3.14 manifest (exactly one `[[script]]`).
    pub manifest: PathBuf,
    /// `--pine-dir`: where `file` entries resolve (default: the manifest's directory).
    pub pine_dir: Option<PathBuf>,
    /// `--screen-universe` (default `~/multivenue/pine/screen-universe.tsv`).
    pub screen_universe: Option<PathBuf>,
    /// `--replay-dir`: one cut per `run-*` (pin:Bp-replay-dir-plan).
    pub replay_dir: Option<PathBuf>,
    /// `--cut-plan`: a `pvm-cutplan-1` file.
    pub cut_plan: Option<PathBuf>,
    /// `--cut-spool`: the feeder's spool directory.
    pub cut_spool: Option<PathBuf>,
    /// `--cut-wait-s`: seconds to wait for one cut's READY marker.
    pub cut_wait_s: u64,
    /// `--split IS/OOS`.
    pub split: Option<String>,
    /// `--split-at-ms`: OOS from this UTC ms.
    pub split_at_ms: Option<i64>,
    /// `--window-ms <from_ms>:<to_ms>`.
    pub window_ms: Option<String>,
    /// `--hold-ms`: the spread rule's hold H.
    pub hold_ms: Option<i64>,
    /// `--fee-bps`, verbatim, in order.
    pub fee_bps: Vec<String>,
    /// `--latency-ns`.
    pub latency_ns: Option<u64>,
    /// `--latency-ns-venue`.
    pub latency_ns_venue: Vec<String>,
    /// `--stale-after-ms`.
    pub stale_after_ms: Vec<String>,
    /// `--fee-measured`, verbatim.
    pub fee_measured: Vec<String>,
    /// `--require-measured-fees`.
    pub require_measured_fees: bool,
    /// `--null-arm`, in row order.
    pub null_arm: Vec<String>,
    /// `--null-seed`.
    pub null_seed: Option<u64>,
    /// `--null-seeds`.
    pub null_seeds: Option<PathBuf>,
    /// `--null-replicates`.
    pub null_replicates: u32,
    /// `--trial-id`: the signal trial's id, 64 hex.
    pub trial_id: Option<String>,
    /// `--trial-ids`: every trial id, one 64-hex line each.
    pub trial_ids: Option<PathBuf>,
    /// `--out-dir` (default `~/multivenue/pine/tier2/<signal id>/`).
    pub out_dir: Option<PathBuf>,
}

/// What the binary prints and exits with.
#[derive(Debug, Clone)]
pub struct PineExit {
    /// 0 | 1 | 4 | 6 (3 from T18).
    pub code: u8,
    /// The `pvm-t2-1` report line (empty on a refusal or a harness error).
    pub stdout: String,
    /// The reason, for stderr.
    pub stderr: String,
    /// The signal arm's closed trades `(entry_ms, exit_ms)`, oldest retained first.
    pub closed_ms: Vec<(i64, i64)>,
}

fn pre(msg: String) -> DriveFail {
    DriveFail {
        exit: EXIT_PRE_RECORD,
        err: HarnessError::Usage(msg),
    }
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// pin:Bp-replay-dir-plan.
fn plan_from_runs(runs: &[RunDir]) -> Result<CutPlan, HarnessError> {
    let mut rows = Vec::with_capacity(runs.len());
    for (k, r) in runs.iter().enumerate() {
        let span = span_prepass(r)?;
        rows.push(CutRow {
            cut: k as u32,
            run_id: r
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            run_epoch_ns: r.epoch_ns,
            off_from_ns: 0,
            off_to_ns: span.last_ts_ns - span.first_ts_ns + 1,
            root: CutRoot::Local(r.path.clone()),
        });
    }
    Ok(CutPlan { rows })
}

fn parse_window(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once(':')?;
    let (a, b) = (a.parse::<i64>().ok()?, b.parse::<i64>().ok()?);
    (a < b).then_some((a, b))
}

/// The trial ids: signal first (T16 appends the nulls').
fn trial_ids(a: &PineArgv) -> Result<Vec<[u8; 32]>, DriveFail> {
    if let Some(id) = &a.trial_id {
        return parse_hex32(id)
            .map(|x| vec![x])
            .ok_or_else(|| pre(format!("--trial-id {id:?} is not 64 hex")));
    }
    let Some(p) = &a.trial_ids else {
        return Err(pre("--trial-id or --trial-ids is required".to_owned()));
    };
    let text =
        std::fs::read_to_string(p).map_err(|e| pre(format!("--trial-ids {}: {e}", p.display())))?;
    let mut out = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        out.push(parse_hex32(l).ok_or_else(|| pre(format!("--trial-ids: {l:?} is not 64 hex")))?);
    }
    if out.is_empty() {
        return Err(pre("--trial-ids names no id".to_owned()));
    }
    Ok(out)
}

/// What the boot fixed before the first record.
pub(crate) struct PineBooted {
    /// Boxed: the set is large, and moves of it overflowed a test thread.
    pub(crate) set: Box<StrategySet>,
    pub(crate) route: Vec<u32>,
    pub(crate) descriptors: Vec<(u32, String)>,
    pub(crate) script_sha: [u8; 32],
}

/// The pine arm boot (T13): manifest → one script → facts and the route in
/// cut 0's symbol space → `PineBootCfg` (R5) → `configure` → cold warm-up →
/// `on_start`.
pub(crate) fn boot_member(
    manifest_path: &Path,
    pine_dir: Option<&Path>,
    universe: &[UniverseRow],
    cut0: &Path,
    boot_utc_ms: i64,
    fills: &mut FillEngine,
) -> Result<PineBooted, DriveFail> {
    let (manifest, bytes): (PineManifest, Vec<u8>) =
        core_config::pine::load_manifest(manifest_path)
            .map_err(|e| pre(format!("pine: manifest {}: {e:?}", manifest_path.display())))?;
    let script = one_script(&manifest)?.clone();
    let cut0_rows = read_manifest_rows(cut0);
    if cut0_rows.is_empty() {
        return Err(pre(format!(
            "pine: cut 0 ({}) carries no instrument-manifest.tsv — the route cannot resolve",
            cut0.display()
        )));
    }
    let resolve = |key: &str| -> Option<SymbolId> {
        let row = universe.iter().find(|r| r.key == key)?;
        let desc = row.capture_descriptor.as_deref()?;
        cut0_rows.iter().find(|(_, d)| d == desc).map(|(s, _)| *s)
    };
    let symbols = crate::pine_boot::manifest_facts(&manifest, resolve, |key, sym| {
        crate::pine_boot::facts_from_screen_universe(key, sym, universe)
    })
    .map_err(pre)?;
    let mut route: Vec<u32> = symbols.iter().map(|f| f.sym).collect();
    route.sort_unstable();
    route.dedup();
    let descriptors: Vec<(u32, String)> = route
        .iter()
        .filter_map(|s| cut0_rows.iter().find(|(c, _)| c == s).cloned())
        .collect();
    let pairs: Vec<(SymbolId, &str)> = descriptors.iter().map(|(s, d)| (*s, d.as_str())).collect();
    crate::trade_route::resolve(&pairs).map_err(|r| pre(format!("pine: {r}")))?;
    let artifact_dir = pine_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let slot = PineSlotCfg::DEFAULT;
    let cfg = PineBootCfg {
        manifest_sha256: core_crypto::sha256(&bytes),
        artifact_dir,
        slot,
        slot_caps: SlotCaps8 {
            max_order_usd_1e6: 0,
            cap_instance_usd_1e6: script.cap_instance_1e6,
            cap_day_usd_1e6: script.cap_day_1e6,
            max_open_orders: script.max_open,
        },
        slot_venue: None,
        route: SlotRoute::Paper,
        host: HostKind::Replay,
        symbols,
        boot_state: Vec::new(),
        anchor: core_time::WallAnchor::new(0, 0),
        boot_utc_ms,
        oid_epoch: u32::try_from(boot_utc_ms / 1_000)
            .unwrap_or(u32::MAX)
            .max(1),
        activation_ns: crate::pine_boot::activation_table(),
        fee: FeeModel {
            taker_ppm: slot.fee_taker_ppm,
            maker_ppm: slot.fee_maker_ppm,
        },
        exec_sha256: [0; 32],
        tape: false,
        // §4.5: `PineBootCfg { route: Paper, replay: true, … }` (R5); no 8g
        // boot record on a tier-2 trial; pin:Bp-warmup-cold-until-flag.
        replay: true,
        boot_record: None,
        warmup: None,
        manifest,
    };
    let mut set = Box::new(StrategySet::new(BIT_PINE));
    set.pine_mut().configure(cfg).map_err(|e| {
        pre(format!(
            "pine: configure refused ({}): {} {:?}",
            e.code.0,
            e.detail,
            e.diags.iter().map(|d| &d.msg).collect::<Vec<_>>()
        ))
    })?;
    set.pine_mut().supply_warm(Vec::new()).map_err(|e| {
        pre(format!(
            "pine: warm-up refused ({}): {}",
            e.code.0, e.detail
        ))
    })?;
    let mut ctx = PineCtx {
        fills,
        now_ns: 0,
        virt_ns: 0,
        intake_refused: 0,
    };
    set.on_start(&mut ctx)
        .map_err(|e| pre(format!("pine: on_start refused: {e:?}")))?;
    Ok(PineBooted {
        set,
        route,
        descriptors,
        script_sha: script.sha256,
    })
}

/// `backtest --strategy pine`: every refusal before the first record exits
/// 6, a harness error after it 1, a script fault 4 (§3.6).
pub fn run_pine_main(a: &PineArgv) -> PineExit {
    match run_inner(a) {
        Ok(x) => x,
        Err(f) => PineExit {
            code: f.exit,
            stdout: String::new(),
            stderr: format!("backtest --strategy pine: {}", f.err),
            closed_ms: Vec::new(),
        },
    }
}

fn run_inner(a: &PineArgv) -> Result<PineExit, DriveFail> {
    let ids = trial_ids(a)?;
    let plan = match (&a.cut_plan, &a.replay_dir) {
        (Some(p), _) => {
            let bytes =
                std::fs::read(p).map_err(|e| pre(format!("--cut-plan {}: {e}", p.display())))?;
            parse_cut_plan(&bytes).map_err(|e| pre(e.to_string()))?
        }
        (None, Some(d)) => {
            let runs = discover_runs(d).map_err(|e| pre(e.to_string()))?;
            plan_from_runs(&runs).map_err(|e| pre(e.to_string()))?
        }
        (None, None) => return Err(pre("--cut-plan or --replay-dir is required".to_owned())),
    };
    if plan.rows.is_empty() {
        return Err(pre("the plan holds no cut".to_owned()));
    }
    let (from_ms, to_ms) = match &a.window_ms {
        Some(w) => {
            parse_window(w).ok_or_else(|| pre(format!("--window-ms {w:?}: want <from>:<to>")))?
        }
        None => {
            let f = (plan.rows[0].wall_from_ns() / 1_000_000) as i64;
            let t = plan.rows[plan.rows.len() - 1]
                .wall_to_ns()
                .div_ceil(1_000_000) as i64;
            (
                f - f.rem_euclid(DAY_MS),
                t + (DAY_MS - t.rem_euclid(DAY_MS)) % DAY_MS,
            )
        }
    };
    check_plan(&plan, from_ms, to_ms, a.hold_ms).map_err(|e| pre(e.to_string()))?;
    let law = match (a.split_at_ms, &a.split) {
        (Some(t), _) => SplitLaw::AtMs(t),
        (None, Some(s)) => {
            SplitLaw::Percent(parse_split(s).map_err(|e| pre(e.to_string()))?.is_pct)
        }
        (None, None) => return Err(pre("--split or --split-at-ms is required".to_owned())),
    };
    let model = parse_model_params(
        &a.fee_bps,
        a.latency_ns,
        &a.latency_ns_venue,
        &a.stale_after_ms,
        &[],
        None,
    )
    .map_err(|e| pre(e.to_string()))?;
    let stale = model.stale_after_ms;
    let uni_path = a.screen_universe.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("multivenue/pine/screen-universe.tsv")
    });
    let universe = load_universe(&uni_path).map_err(|e| {
        pre(format!(
            "pine: --screen-universe {}: {} (no facts, no replay)",
            uni_path.display(),
            e.message()
        ))
    })?;
    let out_dir = a.out_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("multivenue/pine/tier2")
            .join(hex32(&ids[0]))
    });
    refuse_inside_git("--out-dir", &out_dir)?;
    let mut src_local = LocalSource;
    let mut src_spool = a.cut_spool.as_ref().map(|d| {
        // pin:Bp-cut-wait-capped — `--cut-wait-s` bounded by the ≤ 2 h law.
        let w = std::time::Duration::from_secs(a.cut_wait_s).min(super::spool::MAX_WAIT);
        SpoolSource::new(d.clone(), w)
    });
    let cut0 = match src_spool.as_mut() {
        Some(s) => s.open(&plan.rows[0]),
        None => src_local.open(&plan.rows[0]),
    }
    .map_err(|e| pre(e.to_string()))?;
    let mut fills = FillEngine::new(model, boundary_virt_ns(&plan, law));
    let boot_ms = (plan.rows[0].wall_from_ns() / 1_000_000) as i64;
    let mut b = boot_member(
        &a.manifest,
        a.pine_dir.as_deref(),
        &universe,
        &cut0,
        boot_ms,
        &mut fills,
    )?;
    let fee_syms: Vec<FeeSym<'_>> = b
        .descriptors
        .iter()
        .map(|(_, d)| FeeSym {
            descriptor: d.as_str(),
            chart: true,
        })
        .collect();
    let prov = fee_provenance(
        &a.fee_bps,
        &a.fee_measured,
        &fee_syms,
        a.require_measured_fees,
    )?;
    let keep = KeepPolicy {
        trade_syms: &b.route,
        store_syms: Some(&b.route),
        rejudge_stale: true,
    };
    let dcfg = DriveCfg {
        plan: &plan,
        keep: &keep,
        stale_after_ms: stale,
        residency: Residency::Streamed,
    };
    let mut buf: Vec<MergedRec> = Vec::new();
    let r = match src_spool.as_mut() {
        Some(s) => drive_cuts(&dcfg, s, &mut *b.set, &mut fills, &mut buf),
        None => drive_cuts(&dcfg, &mut src_local, &mut *b.set, &mut fills, &mut buf),
    };
    let (out, code, fault) = match r {
        Ok(o) => (o, 0u8, None),
        Err(f) if f.exit == super::drive::EXIT_FAULT => {
            let fault = b.set.pine().fault(0);
            (DriveOut::default(), f.exit, Some((f, fault)))
        }
        Err(f) => return Err(f),
    };
    std::fs::create_dir_all(&out_dir).map_err(|e| DriveFail {
        exit: 1,
        err: HarnessError::Capture(format!("{}: {e}", out_dir.display())),
    })?;
    let mut charges = Vec::with_capacity(MAX_OPEN_TOTAL);
    fills.take_fee_charges(&mut charges);
    let mut ledger = FeeLedger::new(&prov);
    let arm = ledger.push_arm(&prov, "signal", &charges, |s| {
        b.descriptors
            .iter()
            .find(|(x, _)| *x == s)
            .map(|(_, d)| d.as_str())
    });
    ledger.write(&out_dir).map_err(|e| DriveFail {
        exit: 1,
        err: HarnessError::Capture(format!("fees.tsv: {e}")),
    })?;
    let p = b.set.pine();
    let rec = p.broker_record(0);
    let commission = rec.map_or(0, |r| r.commission_paid_1e12);
    let mut stderr = String::new();
    if let Some((f, k)) = &fault {
        stderr = match k {
            Some((kind, pc, bar)) => format!(
                "backtest --strategy pine: FAULTED ({kind:?}, pc {pc}, bar {bar}, sha {}) — {}",
                hex32(&b.script_sha),
                f.err
            ),
            None => format!("backtest --strategy pine: FAULTED — {}", f.err),
        };
    }
    let stdout = format!(
        "{{\"schema\":\"pvm-t2-1\",\"exit\":{code},\"signal_id\":\"{}\",\"trials\":{},\
         \"cuts\":{},\"records\":{},\"closed_trades\":{},\"commission_1e12\":{commission},\
         \"fee_rows\":{},\"fee_1e12\":{},\"fee_digest\":\"{}\",\"fees_measured\":{},\
         \"faulted\":{},\"null_arms_run\":0}}",
        hex32(&ids[0]),
        ids.len(),
        out.cuts.len(),
        out.records,
        p.closed_count(0),
        arm.rows,
        arm.fee_1e12,
        hex32(&prov.digest),
        ledger.fees_measured(),
        fault.is_some(),
    );
    let mut closed_ms = Vec::new();
    let mut i = 0u32;
    while let Some(t) = p.closed_trade(0, i) {
        closed_ms.push((t.entry_ms, t.exit_ms));
        i += 1;
    }
    Ok(PineExit {
        code,
        stdout,
        stderr,
        closed_ms,
    })
}

#[cfg(test)]
mod tests;
