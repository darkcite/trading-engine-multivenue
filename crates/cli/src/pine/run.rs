// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine run`, `pine hurdle`, `pine screen` (impl-07 §3.11, §4.4, §4.6, §4.11): fee rows through
//! `fee_flags::ordinary_cells`, the hurdle before any bar, symbol-major worker threads with one
//! `Workspace` each, groups of K candidates per bar read, **the single writer** of the run
//! directory (the main thread), and the throughput meter — the phase's only clock.
//!
//! COPY-DOCTRINE: a cold operator tool over files, run by hand; never reached by the engine loop.
//! Offline path: setup and the writer allocate; the bar loop is `pine_screen::run_group`'s.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use pine_ir::{trial_flag, ChartInfo, ExecMode, InputOverride, InputValue};
use pine_screen::bars::dec_to_1e6;
use pine_screen::hurdle::{hurdle_decide, hurdle_of, window_stats, Hurdle, HurdleVerdict};
use pine_screen::ids::{fee_digest, hex32, trial_id, TrialIdParts};
use pine_screen::ret::write_ret;
use pine_screen::rows::{RowsFile, RowsWriter};
use pine_screen::{
    bar_path, compile_trial, run_group, BarFile, CompileRefusal, FaultOrigin, FaultRecord, FeeRow,
    Loaded, NoProbe, Refusal, RowRec, RowsHdr, ScreenCfg, TrialBars, TrialOutcome, TrialSpec,
    TrialStatus, Workspace, MAX_GROUP, SCREEN_LIMITS, SEL_NOTIONAL_1E6,
};
use pine_sema::LibCache;

use super::universe::{load_universe, UniverseRow};
use super::{check_window, parse_timeframes, PineErr};
use crate::fee_flags::{ordinary_cells, parse_fee_bps_flag, FeeFlag, OrdinaryCells};

/// The run directory's `run.tsv` schema.
pub const RUN_SCHEMA: &str = "pvm7-run-2";

// ---------------------------------------------------------------------------------------------
// shared configuration
// ---------------------------------------------------------------------------------------------

/// The fee flag set, verbatim, parsed, resolved and digested.
#[derive(Debug, Clone)]
pub struct Fees {
    /// The flags exactly as given.
    pub verbatim: Vec<String>,
    /// Parsed.
    pub flags: Vec<FeeFlag>,
    /// `ordinary_cells(flags)`.
    pub cells: OrdinaryCells,
    /// `ids::fee_digest(verbatim)`.
    pub digest: [u8; 32],
}

/// Parse `--fee-bps` (exit 2 on a refused flag or none at all).
pub fn parse_fees(specs: &[String]) -> Result<Fees, PineErr> {
    if specs.is_empty() {
        return Err(PineErr::Usage(
            "--fee-bps is required (the worker's set, verbatim)".to_owned(),
        ));
    }
    let mut flags = Vec::with_capacity(specs.len());
    for s in specs {
        flags.push(parse_fee_bps_flag(s).map_err(PineErr::Usage)?);
    }
    let refs: Vec<&str> = specs.iter().map(String::as_str).collect();
    Ok(Fees {
        verbatim: specs.to_vec(),
        cells: ordinary_cells(&flags),
        flags,
        digest: fee_digest(&refs),
    })
}

/// A chart's fee row; `None` cell refuses the symbol (I22, exit 5).
pub fn fee_row(fees: &Fees, row: &UniverseRow) -> Result<FeeRow, PineErr> {
    let v = row.fee_venue_byte();
    let c = row.class.index();
    match fees.cells.get(v).and_then(|r| r[c]) {
        Some((maker_bps, taker_bps)) => Ok(FeeRow {
            venue: v as u8,
            class: c as u8,
            maker_bps,
            taker_bps,
        }),
        None => Err(PineErr::Data(format!(
            "{}: no --fee-bps row for {}.{} (I22: never a zero fee)",
            row.key,
            row.fee_venue,
            row.class.label()
        ))),
    }
}

/// `--notional-1e6` (exit 2 when ≤ 0).
pub fn notional(n: Option<i64>) -> Result<i64, PineErr> {
    let n = n.unwrap_or(SEL_NOTIONAL_1E6);
    if n <= 0 {
        return Err(PineErr::Usage(format!(
            "bad --notional-1e6 {n}: must be > 0"
        )));
    }
    Ok(n)
}

/// One priors row (§3.12.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prior {
    /// Script name (`<scripts>/<script>.pine`).
    pub script: String,
    /// Hold prior, bars.
    pub hold_bars: u32,
    /// Trade-rate prior.
    pub trades_per_1k_bars: u32,
}

/// Parse the priors table; the table IS the screen's script list, in order.
pub fn load_priors(path: &Path) -> Result<Vec<Prior>, PineErr> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PineErr::Data(format!("priors {}: {e}", path.display())))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let bad = || {
            PineErr::Data(format!(
                "priors {} line {}: malformed",
                path.display(),
                i + 1
            ))
        };
        if f.len() != 4 {
            return Err(bad());
        }
        out.push(Prior {
            script: f[0].to_owned(),
            hold_bars: f[1].parse().map_err(|_| bad())?,
            trades_per_1k_bars: f[2].parse().map_err(|_| bad())?,
        });
    }
    if out.is_empty() {
        return Err(PineErr::Data(format!("priors {}: no rows", path.display())));
    }
    Ok(out)
}

/// `--input <name>=<value>` in `[script.inputs]` syntax.
// pin:07-input-syntax: the value is a TOML scalar — `"…"` a string, `true`/`false` a bool, digits
// with no `.`/exponent an int, any other decimal a float (×1e6 through `dec_to_1e6`); anything
// else is an argv refusal (exit 2). The input's own type is the compiler's to check.
pub fn parse_input(s: &str) -> Result<InputOverride, PineErr> {
    let bad = || PineErr::Usage(format!("bad --input {s:?}: want <name>=<value>"));
    let (name, v) = s.split_once('=').ok_or_else(bad)?;
    if name.is_empty() {
        return Err(bad());
    }
    let value = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        InputValue::Str(v[1..v.len() - 1].to_owned())
    } else if v == "true" || v == "false" {
        InputValue::Bool(v == "true")
    } else if let Ok(i) = v.parse::<i64>() {
        InputValue::Int(i)
    } else if let Some(f) = dec_to_1e6(v.as_bytes()) {
        InputValue::Float1e6(f)
    } else {
        return Err(bad());
    };
    Ok(InputOverride {
        name: name.to_owned(),
        value,
    })
}

/// The `--lib-dir` cache (`3037` refusal → exit 5).
pub fn open_libs(lib_dir: Option<&PathBuf>) -> Result<LibCache, PineErr> {
    let root = match lib_dir {
        Some(d) => d.clone(),
        None => {
            // pin:07-lib-dir-home: the default `~/multivenue/pine/lib` resolves `$HOME`; without
            // it the default is refused (exit 2) — nothing is inferred from the working directory.
            let home = std::env::var_os("HOME")
                .ok_or_else(|| PineErr::Usage("--lib-dir not given and $HOME unset".to_owned()))?;
            PathBuf::from(home).join("multivenue/pine/lib")
        }
    };
    LibCache::open(root, core_crypto::sha256)
        .map_err(|d| PineErr::Data(format!("{} {}", d.code.0, d.msg)))
}

const fn mode_label(m: ExecMode) -> &'static str {
    match m {
        ExecMode::BarClose => "bar_close",
        ExecMode::Realtime => "realtime",
    }
}

fn hex(b: &[u8; 32]) -> String {
    String::from_utf8_lossy(&hex32(b)).into_owned()
}

fn chart_info(row: &UniverseRow, tf_s: u32) -> ChartInfo {
    ChartInfo {
        symbol_hash: pine_ir::symbol_hash(row.key.as_bytes()),
        timeframe_s: tf_s,
        venue: row.venue,
        mintick_1e6: row.mintick_1e6,
        pointvalue_1e6: row.pointvalue_1e6,
        qty_step_1e6: row.qty_step_1e6,
        min_notional_1e6: row.min_notional_1e6,
        ..ChartInfo::default()
    }
}

fn hurdle_line(script: &str, key: &str, tf: u32, h: &Hurdle, verdict: &str) -> String {
    format!(
        "{script}\t{key}\t{tf}\trt_fee_bps={}\trt_tick_bps={}\tcost_per_trade_bps={}\tmean_range_bps={}\tgross_per_trade_bps={}\tratio_1e4={}\thold_bars={}\ttaker_bps={}\t{verdict}",
        h.rt_fee_bps,
        h.rt_tick_bps,
        h.cost_per_trade_bps,
        h.mean_range_bps,
        h.gross_per_trade_bps,
        h.ratio_1e4,
        h.hold_bars,
        h.taker_bps
    )
}

const fn verdict_label(v: HurdleVerdict) -> &'static str {
    match v {
        HurdleVerdict::Clear => "clear",
        HurdleVerdict::Drop => "drop",
        HurdleVerdict::Annotated => "annotated",
    }
}

fn refusal_text(r: &Refusal) -> String {
    match r {
        Refusal::Broker(c) => format!("broker {}", c.0),
        Refusal::ShortWarmup { have, need } => format!("ShortWarmup have={have} need={need}"),
        Refusal::Coverage {
            first_open_ms,
            last_close_ms,
        } => format!("Coverage first_open_ms={first_open_ms} last_close_ms={last_close_ms}"),
        Refusal::UnitBelowStep {
            unit_qty_1e6,
            step_1e6,
        } => format!("UnitBelowStep unit_qty_1e6={unit_qty_1e6} step_1e6={step_1e6}"),
        Refusal::HtfMissing { symbol_hash, tf_s } => {
            format!("HtfMissing symbol_hash={symbol_hash:016x} tf_s={tf_s}")
        }
        Refusal::HtfCoverage { symbol_hash, tf_s } => {
            format!("HtfCoverage symbol_hash={symbol_hash:016x} tf_s={tf_s}")
        }
    }
}

/// Contract §3.13's incident line: `(FaultKind, pc, bar.index, source sha)`.
pub fn fault_text(f: &FaultRecord) -> String {
    let origin = match f.origin {
        FaultOrigin::Vm(k) => format!("{k:?}"),
        FaultOrigin::Broker(r) => format!("Broker({r:?})"),
    };
    format!(
        "{origin}\tpc={}\tbar_index={}\tsub={}\tsource_sha256={}",
        f.pc,
        f.bar_index,
        f.sub,
        hex(&f.source_sha256)
    )
}

fn compile_text(e: &CompileRefusal) -> String {
    let code = e.code().0;
    match e {
        CompileRefusal::Parse(ds) | CompileRefusal::Compile(ds) => match ds.first() {
            Some(d) => format!("{code} {}..{} {}", d.span.lo, d.span.hi, d.msg),
            None => format!("{code}"),
        },
        CompileRefusal::Verify(v) => format!("{code} verify {v:?}"),
        CompileRefusal::Inputs(names) => format!("{code} no input named {names:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// the per-chart engine (one worker's unit of work)
// ---------------------------------------------------------------------------------------------

/// One script of the screen: name, source, hold prior (`None` for `pine run`: never gated).
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    /// Script name.
    pub name: String,
    /// Source text.
    pub source: String,
    /// Hold prior; `None` = no gate (a single `run` is explicit).
    pub hold_bars: Option<u32>,
}

/// The window, notional, fees and knobs every chart shares.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Bars root.
    pub bars: PathBuf,
    /// `[from_ms, to_ms)`.
    pub from_ms: i64,
    /// Exclusive end.
    pub to_ms: i64,
    /// Selection notional.
    pub notional_1e6: i64,
    /// Fee set.
    pub fees: Fees,
    /// `--interleave K`.
    pub interleave: usize,
    /// `--hurdle-mode annotate`.
    pub annotate: bool,
    /// `--record-faults`.
    pub record_faults: bool,
    /// `pine hurdle`: compute, step nothing.
    pub hurdle_only: bool,
    /// `--input` overrides (single `run` only).
    pub inputs: Vec<InputOverride>,
}

/// Sort key of every line the writer emits: (universe position, priors order, timeframe order).
pub type Key = (usize, usize, usize);

/// One meter sample (a `run_group` call).
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// `ExecMode` of the group (its candidates share it).
    pub mode: u8,
    /// K.
    pub k: usize,
    /// Chart bars read.
    pub bars_read: u64,
    /// `bar_evals + tick_evals`.
    pub bar_steps: u64,
    /// Wall seconds around `run_group`.
    pub wall_s: f64,
    /// `Σ layout.total_bytes` of the group.
    pub ws_bytes: u64,
}

/// What a worker sends the writer.
#[derive(Debug)]
pub enum Msg {
    /// A `hurdle.tsv` line.
    Hurdle(Key, String),
    /// A `trials.tsv` line.
    Trial(Key, String),
    /// A row with its series (Completed or, with `--record-faults`, Faulted).
    Row(Key, RowRec, Vec<f64>),
    /// A `faults.log` line.
    Fault(Key, String),
    /// A bar file used: `key tf digest`.
    Bars(Key, String),
    /// A meter sample.
    Meter(Sample),
    /// A completed trial's text summary (`pine run`).
    Summary(String),
}

fn trial_line(
    script: &str,
    sha: &str,
    input: &str,
    key: &str,
    tf: u32,
    mode: &str,
    verdict: &str,
    status: &str,
    overloads: usize,
    id: &str,
) -> String {
    format!("{id}\t{script}\t{sha}\t{input}\t{key}\t{tf}\t{mode}\t{verdict}\t{status}\toverloads_reached={overloads}")
}

/// Screen one chart `(row, tf)`: fee row, hurdle per script, compile, groups of K, outcomes to
/// `send`. `Err` stops the whole verb with its exit code.
#[allow(clippy::too_many_arguments)]
pub fn screen_chart<F: FnMut(Msg)>(
    plan: &Plan,
    row: &UniverseRow,
    upos: usize,
    tf: u32,
    tf_pos: usize,
    scripts: &[ScriptEntry],
    libs: &mut LibCache,
    ws: &mut Workspace,
    send: &mut F,
) -> Result<(), PineErr> {
    let fee = fee_row(&plan.fees, row)?;
    let path = bar_path(&plan.bars, &row.key, tf)
        .map_err(|e| PineErr::Data(format!("{e} ({} @ {tf} s)", row.key)))?;
    let chart =
        BarFile::open(&path).map_err(|e| PineErr::Data(format!("{e} ({})", path.display())))?;
    let digest64 = chart.hdr().src_digest64;
    send(Msg::Bars(
        (upos, 0, tf_pos),
        format!("bars\t{}\t{tf}\t{digest64:016x}", row.key),
    ));
    // The scored rows: open ≥ from, close ≤ to (one pass per chart, reused for every script).
    let rows = chart.rows();
    let lo = chart.seek(plan.from_ms);
    let mut hi = lo;
    while hi < rows.len() && rows[hi].close_ms <= plan.to_ms {
        hi += 1;
    }
    let mut scratch = Vec::new();
    let (median_close, mean_range) = window_stats(&rows[lo..hi], &mut scratch);
    let cfg = ScreenCfg {
        from_ms: plan.from_ms,
        to_ms: plan.to_ms,
        notional_1e6: plan.notional_1e6,
        fee,
    };
    let syminfo = [
        row.ticker.as_str(),
        row.basecurrency.as_str(),
        row.currency.as_str(),
        row.sym_type.as_str(),
    ];
    let ticker_map: Vec<(String, String)> = Vec::new();
    // Candidates of this chart, priors order: (script position, Loaded, verdict).
    let mut cands: Vec<(usize, Loaded, &'static str)> = Vec::new();
    for (sp, s) in scripts.iter().enumerate() {
        let key: Key = (upos, sp, tf_pos);
        let (verdict, h) = match s.hold_bars {
            Some(hold) => {
                let h = hurdle_of(
                    fee.taker_bps,
                    row.mintick_1e6,
                    median_close,
                    mean_range,
                    hold,
                );
                (verdict_label(hurdle_decide(&h, plan.annotate)), h)
            }
            None => (
                "not_gated",
                hurdle_of(fee.taker_bps, row.mintick_1e6, median_close, mean_range, 0),
            ),
        };
        send(Msg::Hurdle(
            key,
            hurdle_line(&s.name, &row.key, tf, &h, verdict),
        ));
        if plan.hurdle_only {
            continue;
        }
        if verdict == "drop" {
            send(Msg::Trial(
                key,
                trial_line(
                    &s.name, "-", "-", &row.key, tf, "-", verdict, "dropped", 0, "-",
                ) + &format!(
                    "\thurdle={}",
                    hurdle_line(&s.name, &row.key, tf, &h, verdict).replace('\t', " ")
                ),
            ));
            continue;
        }
        let spec = TrialSpec {
            source: &s.source,
            symbol_key: &row.key,
            chart: chart_info(row, tf),
            syminfo_strs: syminfo,
            inputs: &plan.inputs,
            ticker_map: &ticker_map,
            limits: SCREEN_LIMITS,
        };
        match compile_trial(&spec, libs) {
            Ok(t) => cands.push((sp, t, verdict)),
            Err(CompileRefusal::Verify(v)) => {
                return Err(PineErr::Defect(format!(
                    "{} on {} @ {tf}: verify refused what sema emitted: {v:?}",
                    s.name, row.key
                )))
            }
            Err(e) => {
                if s.hold_bars.is_none() {
                    // A single `run`: exit 3 with the code and the span.
                    return Err(PineErr::Compile(format!(
                        "{}: {}",
                        s.name,
                        compile_text(&e)
                    )));
                }
                // pin:07-screen-compile-refusal: in a screen, a compile refusal is that trial's
                // `trials.tsv` line and the screen goes on (exit 3 is the single `run`'s).
                let sha = hex(&core_crypto::sha256(s.source.as_bytes()));
                send(Msg::Trial(
                    key,
                    trial_line(
                        &s.name,
                        &sha,
                        "-",
                        &row.key,
                        tf,
                        "-",
                        verdict,
                        "compile_refused",
                        0,
                        "-",
                    ) + &format!("\t{}", compile_text(&e).replace('\t', " ")),
                ));
            }
        }
    }
    if cands.is_empty() {
        return Ok(());
    }
    // pin:07-groups-by-mode: a group holds one `ExecMode` (bar-close candidates first, then
    // realtime, each in priors order) so the meter's (mode, K) cells are pure; the rows do not
    // depend on the grouping (I14).
    cands.sort_by_key(|c| c.1.mode() as u8);
    let k_max = plan.interleave.clamp(1, MAX_GROUP);
    let mut out = vec![TrialOutcome::default(); k_max];
    let mut g0 = 0;
    while g0 < cands.len() {
        let g1 = (g0 + k_max).min(cands.len());
        let group = &cands[g0..g1];
        let mut needs: Vec<(String, u32)> = Vec::new();
        for c in group {
            for n in c.1.htf_needs() {
                if !needs.contains(n) {
                    needs.push(n.clone());
                }
            }
        }
        let bars = TrialBars::open(&plan.bars, &row.key, tf, &needs, None)
            .map_err(|e| PineErr::Data(format!("{e} ({} @ {tf} s)", row.key)))?;
        let refs: Vec<&Loaded> = group.iter().map(|c| &c.1).collect();
        let ws_bytes: u64 = refs
            .iter()
            .map(|t| u64::from(t.program().layout.total_bytes))
            .sum();
        let t0 = Instant::now();
        let stats = run_group(
            &refs,
            &bars,
            &cfg,
            ws,
            &mut out[..group.len()],
            &mut NoProbe,
        );
        let wall_s = t0.elapsed().as_secs_f64();
        let stats =
            stats.map_err(|e| PineErr::Defect(format!("{} @ {tf}: run_group: {e:?}", row.key)))?;
        send(Msg::Meter(Sample {
            mode: refs[0].mode() as u8,
            k: group.len(),
            bars_read: stats.chart_bars,
            bar_steps: stats.bar_evals + stats.tick_evals,
            wall_s,
            ws_bytes,
        }));
        for (j, c) in group.iter().enumerate() {
            let (sp, t, verdict) = (c.0, &c.1, c.2);
            let s = &scripts[sp];
            let key: Key = (upos, sp, tf_pos);
            let o = out[j];
            let id = trial_id(&TrialIdParts {
                source_sha256: t.source_sha256(),
                input_digest: t.input_digest(),
                symbol_key: &row.key,
                tf_s: tf,
                mode: t.mode(),
                from_ms: plan.from_ms,
                to_ms: plan.to_ms,
                notional_1e6: plan.notional_1e6,
                fee_digest: &plan.fees.digest,
                bars_digest64: digest64,
            });
            let idh = hex(&id);
            let sha = hex(t.source_sha256());
            let inp = hex(t.input_digest());
            let mode = mode_label(t.mode());
            let ovl = t.overloads_reached().len();
            match o.status {
                TrialStatus::Completed => {
                    send(Msg::Trial(
                        key,
                        trial_line(
                            &s.name,
                            &sha,
                            &inp,
                            &row.key,
                            tf,
                            mode,
                            verdict,
                            "completed",
                            ovl,
                            &idh,
                        ),
                    ));
                    if s.hold_bars.is_none() {
                        send(Msg::Summary(summary_text(&s.name, &row.key, tf, &idh, &o)));
                    }
                    send(Msg::Row(
                        key,
                        RowRec {
                            trial_id: id,
                            summary: o.summary,
                        },
                        ws.day_returns(j).to_vec(),
                    ));
                }
                TrialStatus::Faulted(f) => {
                    let ft = fault_text(&f);
                    if !plan.record_faults {
                        return Err(PineErr::Fault(format!(
                            "{} on {} @ {tf}: {ft}",
                            s.name, row.key
                        )));
                    }
                    send(Msg::Trial(
                        key,
                        trial_line(
                            &s.name, &sha, &inp, &row.key, tf, mode, verdict, "faulted", ovl, &idh,
                        ) + &format!("\t{}", ft.replace('\t', " ")),
                    ));
                    send(Msg::Fault(
                        key,
                        format!("{idh}\t{}\t{}\t{tf}\t{ft}", s.name, row.key),
                    ));
                    debug_assert!(o.summary.flags & trial_flag::FAULTED != 0);
                    send(Msg::Row(
                        key,
                        RowRec {
                            trial_id: id,
                            summary: o.summary,
                        },
                        Vec::new(),
                    ));
                }
                TrialStatus::Refused(r) => {
                    let rt = refusal_text(&r);
                    if s.hold_bars.is_none() {
                        return Err(PineErr::Data(format!(
                            "{} on {} @ {tf}: {rt}",
                            s.name, row.key
                        )));
                    }
                    send(Msg::Trial(
                        key,
                        trial_line(
                            &s.name, &sha, &inp, &row.key, tf, mode, verdict, "refused", ovl, &idh,
                        ) + &format!("\t{rt}"),
                    ));
                }
            }
        }
        g0 = g1;
    }
    Ok(())
}

fn summary_text(script: &str, key: &str, tf: u32, id: &str, o: &TrialOutcome) -> String {
    let s = &o.summary;
    format!(
        "trial {id}\nscript {script}\nsymbol {key}\ntimeframe_s {tf}\nbars {}\ntrades {}\nclosed {}\nobs_days {}\nhold_bars_med {}\nhold_bars_p95 {}\nnet_bps_1e4 {}\ngross_bps_1e4 {}\nfee_bps_1e4 {}\nmax_concurrent_1e6 {}\nturnover_day_p99_1e6 {}\nsizing_notional_1e6 {}\nflags {:#x}\n",
        s.bars,
        s.trades,
        s.closed,
        s.obs_days,
        s.hold_bars_med,
        s.hold_bars_p95,
        s.net_bps_1e4,
        s.gross_bps_1e4,
        s.fee_bps_1e4,
        s.max_concurrent_1e6,
        s.turnover_day_p99_1e6,
        s.sizing_notional_1e6,
        s.flags
    )
}

// ---------------------------------------------------------------------------------------------
// the single writer
// ---------------------------------------------------------------------------------------------

/// Everything the writer gathered, sorted at the end (I15: byte-identical for every N and K).
#[derive(Default)]
pub struct Collected {
    /// `hurdle.tsv`.
    pub hurdles: BTreeMap<Key, String>,
    /// `trials.tsv`.
    pub trials: BTreeMap<Key, String>,
    /// `rows.t1`.
    pub rows: BTreeMap<Key, RowRec>,
    /// `faults.log`.
    pub faults: BTreeMap<Key, String>,
    /// `run.tsv` bar lines.
    pub bars: BTreeMap<Key, String>,
    /// Meter samples.
    pub meter: Vec<Sample>,
    /// `pine run`'s summary.
    pub summaries: Vec<String>,
    /// Trials stepped (not dropped, not refused before stepping).
    pub stepped: u64,
}

impl Collected {
    /// Take one message; a row's series is written at once when `ret_dir` is set.
    pub fn take(&mut self, m: Msg, ret_dir: Option<&Path>) -> Result<(), PineErr> {
        match m {
            Msg::Hurdle(k, l) => {
                self.hurdles.insert(k, l);
            }
            Msg::Trial(k, l) => {
                self.trials.insert(k, l);
            }
            Msg::Row(k, r, days) => {
                self.stepped += 1;
                if let Some(d) = ret_dir {
                    if r.summary.flags & trial_flag::FAULTED == 0 {
                        write_ret(d, &r.trial_id, &days)
                            .map_err(|e| PineErr::Defect(format!("ret-t1 write: {e}")))?;
                    }
                }
                self.rows.insert(k, r);
            }
            Msg::Fault(k, l) => {
                self.faults.insert(k, l);
            }
            Msg::Bars(k, l) => {
                self.bars.insert(k, l);
            }
            Msg::Meter(s) => self.meter.push(s),
            Msg::Summary(s) => self.summaries.push(s),
        }
        Ok(())
    }
}

fn write_lines<'a>(path: &Path, lines: impl Iterator<Item = &'a String>) -> Result<(), PineErr> {
    let mut s = String::new();
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    std::fs::write(path, s).map_err(|e| PineErr::Defect(format!("{}: {e}", path.display())))
}

fn binary_mtime() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(|| "unknown".to_owned(), |d| d.as_secs().to_string())
}

/// Refuse an existing `rows.t1` whose header differs (exit 5).
pub fn check_rows_hdr(out: &Path, hdr: &RowsHdr) -> Result<(), PineErr> {
    let p = out.join("rows.t1");
    if p.exists() {
        let f = RowsFile::open(&p).map_err(|e| PineErr::Data(format!("{}: {e:?}", p.display())))?;
        if f.hdr() != hdr {
            return Err(PineErr::Data(format!(
                "{}: an existing rows.t1 with another header (notional or fee set)",
                p.display()
            )));
        }
    }
    Ok(())
}

/// Write the run directory from `c` (rows.t1 replaced atomically).
// pin:07-rerun-replaces: a re-run into the same `--out` (same header) replaces rows.t1 and the TSVs
// rather than appending, so the directory always describes one run.
pub fn write_run_dir(out: &Path, plan: &Plan, c: &Collected, verb: &str) -> Result<(), PineErr> {
    let io = |e: std::io::Error| PineErr::Defect(format!("{}: {e}", out.display()));
    std::fs::create_dir_all(out).map_err(io)?;
    // run.tsv
    let mut r = String::new();
    let _ = writeln!(r, "schema\t{RUN_SCHEMA}");
    let _ = writeln!(r, "verb\t{verb}");
    for f in &plan.fees.verbatim {
        let _ = writeln!(r, "fee_bps\t{f}");
    }
    let _ = writeln!(r, "fee_digest\t{}", hex(&plan.fees.digest));
    let _ = writeln!(r, "notional_1e6\t{}", plan.notional_1e6);
    let _ = writeln!(r, "from_ms\t{}", plan.from_ms);
    let _ = writeln!(r, "to_ms\t{}", plan.to_ms);
    let l = SCREEN_LIMITS;
    let _ = writeln!(
        r,
        "screen_limits\tmax_bars_back={} insn_budget={} collection_cap={} str_persist_bytes={} str_cell_bytes={} str_scratch_bytes={} intent_cap={} max_frame_bytes={} active_tiers={}",
        l.max_bars_back, l.insn_budget, l.collection_cap, l.str_persist_bytes, l.str_cell_bytes,
        l.str_scratch_bytes, l.intent_cap, l.max_frame_bytes, l.active_tiers
    );
    let _ = writeln!(r, "binary_mtime\t{}", binary_mtime());
    for b in c.bars.values() {
        let _ = writeln!(r, "{b}");
    }
    std::fs::write(out.join("run.tsv"), r).map_err(io)?;
    // rows.t1, sorted (universe position, priors order, timeframe)
    let hdr = RowsHdr::new(1, plan.notional_1e6, plan.fees.digest);
    let tmp = out.join("rows.t1.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut w =
        RowsWriter::open(&tmp, &hdr).map_err(|e| PineErr::Defect(format!("rows.t1: {e:?}")))?;
    for rec in c.rows.values() {
        w.append(rec)
            .map_err(|e| PineErr::Defect(format!("rows.t1 append: {e:?}")))?;
    }
    w.finish()
        .map_err(|e| PineErr::Defect(format!("rows.t1: {e:?}")))?;
    std::fs::rename(&tmp, out.join("rows.t1")).map_err(io)?;
    write_lines(&out.join("trials.tsv"), c.trials.values())?;
    write_lines(&out.join("hurdle.tsv"), c.hurdles.values())?;
    write_lines(&out.join("faults.log"), c.faults.values())?;
    Ok(())
}

fn probe(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .replace(['\t', '\n'], " ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `throughput.tsv` (§4.11): the host line, one line per (mode, K), the knee per mode.
pub fn throughput_text(samples: &[Sample]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "host {}  chip {}  macOS {}  rustc {}  binary_mtime {}",
        probe("hostname", &[]),
        probe("sysctl", &["-n", "machdep.cpu.brand_string"]),
        probe("sw_vers", &["-productVersion"]),
        probe("rustc", &["-V"]),
        binary_mtime()
    );
    let _ = writeln!(
        s,
        "mode\tK\tinstances\tbars_read\tbar_steps\twall_s\tbars/s/core\tbar_steps/s/core\tws_bytes"
    );
    // (mode, K) → sums.
    let mut agg: BTreeMap<(u8, usize), (u64, u64, u64, f64, u64)> = BTreeMap::new();
    for x in samples {
        let e = agg.entry((x.mode, x.k)).or_default();
        e.0 += x.k as u64;
        e.1 += x.bars_read;
        e.2 += x.bar_steps;
        e.3 += x.wall_s;
        e.4 = e.4.max(x.ws_bytes);
    }
    let mut knee: [Option<(usize, f64)>; 2] = [None, None];
    for (&(mode, k), &(inst, br, bs, w, wsb)) in &agg {
        let (bps, sps) = if w > 0.0 {
            (br as f64 / w, bs as f64 / w)
        } else {
            (0.0, 0.0)
        };
        let label = if mode == 0 { "bar_close" } else { "realtime" };
        let _ = writeln!(
            s,
            "{label}\t{k}\t{inst}\t{br}\t{bs}\t{w:.6}\t{bps:.1}\t{sps:.1}\t{wsb}"
        );
        let m = usize::from(mode.min(1));
        // The knee: the K where bar_steps/s/core stops rising (the last rise).
        match knee[m] {
            Some((_, best)) if sps <= best => {}
            _ => knee[m] = Some((k, sps)),
        }
    }
    let kn = |o: Option<(usize, f64)>| o.map_or_else(|| "-".to_owned(), |(k, _)| k.to_string());
    let _ = writeln!(
        s,
        "knee: bar_close K={} ; realtime K={}",
        kn(knee[0]),
        kn(knee[1])
    );
    s
}

// ---------------------------------------------------------------------------------------------
// the verbs
// ---------------------------------------------------------------------------------------------

/// Arguments shared by `hurdle` and `screen` after validation.
pub struct ScreenInputs {
    /// The plan.
    pub plan: Plan,
    /// Universe rows, file order.
    pub universe: Vec<UniverseRow>,
    /// Timeframes, flag order.
    pub tfs: Vec<u32>,
    /// Scripts, priors order.
    pub scripts: Vec<ScriptEntry>,
}

/// Load the priors' scripts from `dir`.
pub fn load_scripts(dir: &Path, priors: &[Prior]) -> Result<Vec<ScriptEntry>, PineErr> {
    let mut out = Vec::with_capacity(priors.len());
    for p in priors {
        let path = dir.join(format!("{}.pine", p.script));
        let source = std::fs::read_to_string(&path)
            .map_err(|e| PineErr::Data(format!("{}: {e}", path.display())))?;
        out.push(ScriptEntry {
            name: p.script.clone(),
            source,
            hold_bars: Some(p.hold_bars),
        });
    }
    Ok(out)
}

/// Run every chart of `si` on `threads` symbol-major workers; the calling thread is the single
/// writer. Returns what was collected.
pub fn drive(
    si: &ScreenInputs,
    threads: usize,
    libs: &LibCache,
    ret_dir: Option<&Path>,
) -> Result<Collected, PineErr> {
    let mut c = Collected::default();
    let n = threads.max(1);
    let (tx, rx) = mpsc::channel::<Result<Msg, PineErr>>();
    let mut first_err: Option<PineErr> = None;
    std::thread::scope(|scope| {
        for j in 0..n {
            let tx = tx.clone();
            let mut libs = libs.clone();
            scope.spawn(move || {
                let mut ws = Workspace::new();
                let mut upos = j;
                while upos < si.universe.len() {
                    let row = &si.universe[upos];
                    for (tf_pos, &tf) in si.tfs.iter().enumerate() {
                        let mut send = |m: Msg| {
                            let _ = tx.send(Ok(m));
                        };
                        if let Err(e) = screen_chart(
                            &si.plan,
                            row,
                            upos,
                            tf,
                            tf_pos,
                            &si.scripts,
                            &mut libs,
                            &mut ws,
                            &mut send,
                        ) {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                    upos += n;
                }
            });
        }
        drop(tx);
        for m in rx {
            match m {
                Ok(m) => {
                    if first_err.is_none() {
                        if let Err(e) = c.take(m, ret_dir) {
                            first_err = Some(e);
                        }
                    }
                }
                Err(e) => {
                    // pin:07-first-error-wins: the first refusal received stops the verb; the
                    // lowest exit code is not preferred over arrival order.
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
    });
    match first_err {
        Some(e) => Err(e),
        None => Ok(c),
    }
}

/// `--interleave-sweep 1,4,8,…` → the K list (each 1..=MAX_GROUP; exit 2 otherwise).
pub fn parse_sweep(s: &str) -> Result<Vec<usize>, PineErr> {
    let mut out = Vec::new();
    for p in s.split(',') {
        let k: usize = p
            .trim()
            .parse()
            .map_err(|_| PineErr::Usage(format!("bad --interleave-sweep {s:?}")))?;
        if k == 0 || k > MAX_GROUP {
            return Err(PineErr::Usage(format!(
                "bad --interleave-sweep {s:?}: {k} outside 1..={MAX_GROUP}"
            )));
        }
        out.push(k);
    }
    Ok(out)
}

/// `--interleave` (exit 2 for 0 or > MAX_GROUP).
pub fn check_interleave(k: usize) -> Result<usize, PineErr> {
    if k == 0 || k > MAX_GROUP {
        return Err(PineErr::Usage(format!(
            "bad --interleave {k}: 1..={MAX_GROUP}"
        )));
    }
    Ok(k)
}

/// `--threads` (exit 2 for 0).
pub fn check_threads(n: usize) -> Result<usize, PineErr> {
    if n == 0 {
        return Err(PineErr::Usage("bad --threads 0".to_owned()));
    }
    Ok(n)
}

/// `--hurdle-mode drop|annotate` → annotate?
pub fn parse_hurdle_mode(s: &str) -> Result<bool, PineErr> {
    match s {
        "drop" => Ok(false),
        "annotate" => Ok(true),
        _ => Err(PineErr::Usage(format!(
            "bad --hurdle-mode {s:?}: want drop|annotate"
        ))),
    }
}

/// Window + timeframes + universe + scripts → the inputs (argv refusals first, then data).
#[allow(clippy::too_many_arguments)]
pub fn screen_inputs(
    scripts_dir: &Path,
    priors: &Path,
    bars: &Path,
    universe: &Path,
    timeframes: &str,
    from_ms: i64,
    to_ms: i64,
    fee_specs: &[String],
    notional_1e6: Option<i64>,
    interleave: usize,
    annotate: bool,
    record_faults: bool,
    hurdle_only: bool,
) -> Result<ScreenInputs, PineErr> {
    let tfs = parse_timeframes(timeframes)?;
    check_window(Some(from_ms), to_ms)?;
    let fees = parse_fees(fee_specs)?;
    let notional_1e6 = notional(notional_1e6)?;
    let interleave = check_interleave(interleave)?;
    let universe = load_universe(universe)?;
    let priors = load_priors(priors)?;
    let scripts = load_scripts(scripts_dir, &priors)?;
    // Every chart's fee row before any bar (I22: a symbol without one refuses the screen).
    for row in &universe {
        fee_row(&fees, row)?;
    }
    Ok(ScreenInputs {
        plan: Plan {
            bars: bars.to_path_buf(),
            from_ms,
            to_ms,
            notional_1e6,
            fees,
            interleave,
            annotate,
            record_faults,
            hurdle_only,
            inputs: Vec::new(),
        },
        universe,
        tfs,
        scripts,
    })
}

/// Exit 6 when every gated combination was dropped.
pub fn all_dropped(c: &Collected) -> bool {
    !c.hurdles.is_empty() && c.hurdles.values().all(|l| l.ends_with("\tdrop"))
}

/// `pine run`: one script × one symbol × one timeframe → one row + series + a text summary.
pub fn run_verb(a: &super::RunArgs) -> Result<String, PineErr> {
    let tfs = parse_timeframes(&a.timeframe.to_string())?;
    check_window(Some(a.from_ms), a.to_ms)?;
    let fees = parse_fees(&a.fee_bps)?;
    let notional_1e6 = notional(a.notional_1e6)?;
    let mut inputs = Vec::with_capacity(a.input.len());
    for i in &a.input {
        inputs.push(parse_input(i)?);
    }
    let universe = load_universe(&a.universe)?;
    let (upos, row) = universe
        .iter()
        .enumerate()
        .find(|(_, r)| r.key == a.symbol)
        .ok_or_else(|| PineErr::Data(format!("--symbol {}: not in the universe", a.symbol)))?;
    let source = std::fs::read_to_string(&a.script)
        .map_err(|e| PineErr::Data(format!("{}: {e}", a.script.display())))?;
    let name = a
        .script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script")
        .to_owned();
    let mut libs = open_libs(a.lib_dir.as_ref())?;
    let plan = Plan {
        bars: a.bars.clone(),
        from_ms: a.from_ms,
        to_ms: a.to_ms,
        notional_1e6,
        fees,
        interleave: 1,
        annotate: false,
        record_faults: false,
        hurdle_only: false,
        inputs,
    };
    if let Some(out) = &a.out {
        check_rows_hdr(out, &RowsHdr::new(1, notional_1e6, plan.fees.digest))?;
    }
    let scripts = [ScriptEntry {
        name,
        source,
        hold_bars: None,
    }];
    let mut msgs = Vec::new();
    let mut ws = Workspace::new();
    screen_chart(
        &plan,
        row,
        upos,
        tfs[0],
        0,
        &scripts,
        &mut libs,
        &mut ws,
        &mut |m| msgs.push(m),
    )?;
    let ret_dir = a.out.as_ref().map(|o| o.join("ret-t1"));
    if let Some(d) = &ret_dir {
        std::fs::create_dir_all(d).map_err(|e| PineErr::Defect(format!("{}: {e}", d.display())))?;
    }
    let mut c = Collected::default();
    for m in msgs {
        c.take(m, ret_dir.as_deref())?;
    }
    if let Some(out) = &a.out {
        write_run_dir(out, &plan, &c, "run")?;
    }
    if a.json {
        let mut s = String::from("{\"schema\":");
        super::json_str(&mut s, super::JSON_SCHEMA);
        s.push_str(",\"verb\":\"run\",\"rows\":[");
        for (i, r) in c.rows.values().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let m = &r.summary;
            s.push_str("{\"trial_id\":");
            super::json_str(&mut s, &hex(&r.trial_id));
            let _ = write!(
                s,
                ",\"bars\":{},\"trades\":{},\"closed\":{},\"obs_days\":{},\"net_bps_1e4\":{},\"gross_bps_1e4\":{},\"fee_bps_1e4\":{},\"sizing_notional_1e6\":{},\"flags\":{}}}",
                m.bars, m.trades, m.closed, m.obs_days, m.net_bps_1e4, m.gross_bps_1e4, m.fee_bps_1e4, m.sizing_notional_1e6, m.flags
            );
        }
        s.push_str("]}\n");
        return Ok(s);
    }
    Ok(c.summaries.concat())
}

/// `pine hurdle`: the fee-hurdle grid; reads bar files only, steps no bar.
pub fn hurdle_verb(a: &super::HurdleArgs) -> Result<String, PineErr> {
    let si = screen_inputs(
        &a.scripts,
        &a.priors,
        &a.bars,
        &a.universe,
        &a.timeframes,
        a.from_ms,
        a.to_ms,
        &a.fee_bps,
        None,
        1,
        false,
        false,
        true,
    )?;
    let c = drive(&si, 1, &LibCache::empty(), None)?;
    let mut s = String::new();
    if a.json {
        // pin:07-hurdle-json: the envelope carries the hurdle.tsv lines verbatim, in order.
        s.push_str("{\"schema\":");
        super::json_str(&mut s, super::JSON_SCHEMA);
        s.push_str(",\"verb\":\"hurdle\",\"lines\":[");
        for (i, l) in c.hurdles.values().enumerate() {
            if i > 0 {
                s.push(',');
            }
            super::json_str(&mut s, l);
        }
        s.push_str("]}\n");
    } else {
        for l in c.hurdles.values() {
            s.push_str(l);
            s.push('\n');
        }
    }
    if all_dropped(&c) {
        print!("{s}");
        return Err(PineErr::AllDropped(
            "every combination dropped by the hurdle".to_owned(),
        ));
    }
    Ok(s)
}

/// `pine screen`: tier 1 — scripts × universe × timeframes, threaded, grouped; the run directory.
pub fn screen_verb(a: &super::ScreenArgs) -> Result<String, PineErr> {
    let annotate = parse_hurdle_mode(&a.hurdle_mode)?;
    let threads = check_threads(a.threads)?;
    let sweep = match &a.interleave_sweep {
        Some(s) => Some(parse_sweep(s)?),
        None => None,
    };
    let mut si = screen_inputs(
        &a.scripts,
        &a.priors,
        &a.bars,
        &a.universe,
        &a.timeframes,
        a.from_ms,
        a.to_ms,
        &a.fee_bps,
        a.notional_1e6,
        a.interleave,
        annotate,
        a.record_faults,
        false,
    )?;
    let libs = open_libs(a.lib_dir.as_ref())?;
    let hdr = RowsHdr::new(1, si.plan.notional_1e6, si.plan.fees.digest);
    check_rows_hdr(&a.out, &hdr)?;
    std::fs::create_dir_all(&a.out)
        .map_err(|e| PineErr::Defect(format!("{}: {e}", a.out.display())))?;
    if let Some(ks) = sweep {
        // Throughput only, no rows (§3.11): one pass per K, the meter kept.
        let mut samples = Vec::new();
        for k in ks {
            si.plan.interleave = k;
            let c = drive(&si, threads, &libs, None)?;
            samples.extend(c.meter);
        }
        let t = throughput_text(&samples);
        std::fs::write(a.out.join("throughput.tsv"), &t)
            .map_err(|e| PineErr::Defect(format!("throughput.tsv: {e}")))?;
        return Ok(t);
    }
    let ret_dir = a.out.join("ret-t1");
    std::fs::create_dir_all(&ret_dir)
        .map_err(|e| PineErr::Defect(format!("{}: {e}", ret_dir.display())))?;
    let c = drive(&si, threads, &libs, Some(&ret_dir))?;
    write_run_dir(&a.out, &si.plan, &c, "screen")?;
    std::fs::write(a.out.join("throughput.tsv"), throughput_text(&c.meter))
        .map_err(|e| PineErr::Defect(format!("throughput.tsv: {e}")))?;
    if all_dropped(&c) {
        return Err(PineErr::AllDropped(
            "every combination dropped by the hurdle".to_owned(),
        ));
    }
    let completed = c
        .trials
        .values()
        .filter(|l| l.contains("\tcompleted\t"))
        .count();
    let faulted = c
        .trials
        .values()
        .filter(|l| l.contains("\tfaulted\t"))
        .count();
    let dropped = c
        .trials
        .values()
        .filter(|l| l.contains("\tdropped\t"))
        .count();
    let refused = c.trials.len() - completed - faulted - dropped;
    Ok(format!(
        "screen\t{}\ttrials={}\tcompleted={completed}\tfaulted={faulted}\tdropped={dropped}\trefused={refused}\trows={}\n",
        a.out.display(),
        c.trials.len(),
        c.rows.len()
    ))
}
