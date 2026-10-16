// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine report` (impl-07 §3.11, §4.9–§4.11): for each hand-off of a screen, the tier-1 SHADOW
//! run over the hand-off's score window (its row under `t2_trial_id` in `rows.t1shadow`), B'3's
//! tier-2 and null rows joined from `rows.t2`, `delta::delta_of`, the per-script aggregate (a
//! median with n and IQR — never a mean, never a rank), and the three deliverables:
//! `report/delta.json` + `report/delta.txt`, `report/wiring.json`, `report/throughput.json`.
//! Until B'3 lands there is no `rows.t2`: every delta row is `T2_PENDING` (I26: a refusal, never
//! a zero). Rows are ordered by (script, symbol, timeframe); no ordering argument exists.
//!
//! pin:07-report-every-handoff: every `<run>/handoff/<hex>/` holding a `handoff.tsv` is reported;
//! a `pine handoff` re-run over a changed tape ADDS a directory (its `t2_trial_id` differs) and
//! never deletes the old one (B'3's outputs live in it), so both appear until the operator
//! removes the stale one.
//!
//! COPY-DOCTRINE: a cold operator tool over files, run by hand; never reached by the engine loop.
//! Offline path: it may allocate.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use core_time::barclock;
use pine_ir::{trial_flag, ExecMode, OverloadId};
use pine_screen::delta::{aggregate, delta_flag, delta_of, Aggregate, DeltaRow};
use pine_screen::handoff::{read_handoff, Handoff, MIN_CLOSED_T2, TARGET_CLOSED_T2};
use pine_screen::hurdle::policy;
use pine_screen::ret::{read_ret, ret_path, write_ret};
use pine_screen::rows::{RowsFile, RowsWriter};
use pine_screen::{
    bar_path, compile_trial, run_trial, BarFile, CompileRefusal, RowRec, RowsHdr, ScreenCfg,
    TrialBars, TrialSpec, TrialStatus, Workspace, SCREEN_LIMITS,
};

use super::handoff::{read_hurdle_verdicts, read_run_dir, read_trials, RunDir};
use super::run::{
    chart_info, fault_text, fee_row, hex, mode_label, open_libs, parse_fees, refusal_text,
};
use super::universe::{load_universe, UniverseRow};
use super::{json_str, PineErr, JSON_SCHEMA};

const DAY_MS: i64 = 86_400_000;

fn data(m: String) -> PineErr {
    PineErr::Data(m)
}

fn defect(m: String) -> PineErr {
    PineErr::Defect(m)
}

/// One hand-off's report line.
struct Line {
    h: Handoff,
    script: String,
    /// The shadow row, or why there is none.
    t1: Result<RowRec, String>,
    t2: Option<RowRec>,
    null: Option<RowRec>,
    delta: Option<DeltaRow>,
    /// Pearson correlation of the two tiers' daily series (beside the row, not a gate).
    corr: Option<f64>,
    /// bar_agreement: (compared, differing by more than one tick) when trade-print bars exist.
    agree: Option<(u64, u64)>,
}

/// One edge of `wiring.json`: `ok` is `None` when this verb cannot measure it (`–`).
struct Edge {
    edge: &'static str,
    ok: Option<bool>,
    evidence: String,
}

fn script_of(h: &Handoff) -> String {
    h.script
        .strip_suffix(".pine")
        .unwrap_or(&h.script)
        .to_owned()
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 2 {
        return None;
    }
    let (ma, mb) = (
        a[..n].iter().sum::<f64>() / n as f64,
        b[..n].iter().sum::<f64>() / n as f64,
    );
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    if saa <= 0.0 || sbb <= 0.0 {
        return None;
    }
    Some(sab / (saa.sqrt() * sbb.sqrt()))
}

/// The tier-1 shadow run of one hand-off (§4.9 step 1).
fn shadow(
    h: &Handoff,
    row: &UniverseRow,
    libs: &mut pine_sema::LibCache,
    ws: &mut Workspace,
    counters: &mut [u64; 3],
) -> Result<Result<RowRec, String>, PineErr> {
    let sp = h.dir.join(&h.script);
    let source =
        std::fs::read_to_string(&sp).map_err(|e| data(format!("{}: {e}", sp.display())))?;
    let syminfo = [
        row.ticker.as_str(),
        row.basecurrency.as_str(),
        row.currency.as_str(),
        row.sym_type.as_str(),
    ];
    // pin:07-shadow-no-overrides: a screen compiles every trial with no `--input` override and no
    // ticker map (`run.rs` screen_chart), so the shadow run passes the same empty lists.
    let spec = TrialSpec {
        source: &source,
        symbol_key: &h.symbol_key,
        chart: chart_info(row, h.timeframe_s),
        syminfo_strs: syminfo,
        inputs: &[],
        ticker_map: &[],
        limits: SCREEN_LIMITS,
    };
    let t = match compile_trial(&spec, libs) {
        Ok(t) => t,
        Err(CompileRefusal::Verify(v)) => {
            return Err(defect(format!("{}: verify refused: {v:?}", h.script)))
        }
        Err(e) => return Ok(Err(format!("compile {}", super::run::compile_text(&e)))),
    };
    if *t.source_sha256() != h.source_sha256 || t.mode() != h.mode {
        return Err(defect(format!(
            "{}: the shadow compile is not the hand-off's program (sha/mode)",
            h.script
        )));
    }
    let fees = parse_fees(&h.fee_flags)?;
    let fee = fee_row(&fees, row)?;
    let bars = match TrialBars::open(
        &h.warmup_bars_root,
        &h.symbol_key,
        h.timeframe_s,
        t.htf_needs(),
        None,
    ) {
        Ok(b) => b,
        Err(e) => return Ok(Err(format!("bars {e}"))),
    };
    let cfg = ScreenCfg {
        from_ms: h.score_from_ms,
        to_ms: h.score_to_ms,
        notional_1e6: h.sizing_notional_1e6,
        fee,
    };
    let o = run_trial(&t, &bars, &cfg, ws)
        .map_err(|e| defect(format!("{}: run_trial: {e:?}", h.script)))?;
    let c = &o.record.counters;
    counters[0] += c.intents_committed;
    counters[1] += c.intents_modified;
    counters[2] += c.sized_to_zero;
    match o.status {
        TrialStatus::Completed => Ok(Ok(RowRec {
            trial_id: h.t2_trial_id,
            summary: o.summary,
        })),
        // pin:07-shadow-fault-row: a faulted shadow run is still a row (FAULTED → NOT_JUDGED in
        // the delta, I26), the fault printed on stderr; it never stops the report.
        TrialStatus::Faulted(f) => {
            debug_assert!(o.summary.flags & trial_flag::FAULTED != 0);
            eprintln!(
                "pine report: {} shadow run faulted: {}",
                h.script,
                fault_text(&f)
            );
            Ok(Ok(RowRec {
                trial_id: h.t2_trial_id,
                summary: o.summary,
            }))
        }
        TrialStatus::Refused(r) => Ok(Err(refusal_text(&r))),
    }
}

/// `pine report`.
pub fn report_verb(a: &super::ReportArgs) -> Result<String, PineErr> {
    let rd = read_run_dir(&a.run)?;
    if let Some(tb) = &a.trade_bars {
        let same = std::path::absolute(tb).ok() == std::path::absolute(&rd.bars_root).ok();
        if same {
            return Err(PineErr::Usage(
                "--trade-bars must be a root of its own, never the candles root".to_owned(),
            ));
        }
    }
    // The hand-offs, (script, symbol, timeframe) order.
    let hroot = a.run.join("handoff");
    let mut hs: Vec<Handoff> = Vec::new();
    if let Ok(rdir) = std::fs::read_dir(&hroot) {
        let mut dirs: Vec<PathBuf> = rdir
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("handoff.tsv").is_file())
            .collect();
        dirs.sort();
        for d in dirs {
            hs.push(read_handoff(&d).map_err(|e| data(format!("{}: {e:?}", d.display())))?);
        }
    }
    if hs.is_empty() {
        return Err(PineErr::Handoff(format!(
            "no hand-off directory under {} (run pine handoff first)",
            hroot.display()
        )));
    }
    hs.sort_by(|x, y| {
        (script_of(x), &x.symbol_key, x.timeframe_s).cmp(&(
            script_of(y),
            &y.symbol_key,
            y.timeframe_s,
        ))
    });
    for h in &hs {
        if h.sizing_notional_1e6 != rd.notional_1e6 || h.fee_digest != rd.fee_digest {
            return Err(data(format!(
                "{}: notional or fee set differs from run.tsv",
                h.dir.display()
            )));
        }
    }
    // B'3's rows: same tier-2 header or refused (§4.9 step 2).
    let t2_path = a.t2_rows.clone().unwrap_or_else(|| a.run.join("rows.t2"));
    let t2_file = if t2_path.exists() {
        let f =
            RowsFile::open(&t2_path).map_err(|e| data(format!("{}: {e:?}", t2_path.display())))?;
        let want = RowsHdr::new(2, rd.notional_1e6, rd.fee_digest);
        if *f.hdr() != want {
            return Err(data(format!(
                "{}: not comparable — tier {} notional {} fee digest {} (want tier 2, {}, {})",
                t2_path.display(),
                f.hdr().tier,
                f.hdr().sizing_notional_1e6,
                hex(&f.hdr().fee_digest),
                rd.notional_1e6,
                hex(&rd.fee_digest)
            )));
        }
        Some(f)
    } else {
        None
    };
    let universe = load_universe(&rd.universe)?;
    let hurdles = read_hurdle_verdicts(&a.run)?;
    let mut libs = open_libs(Some(&rd.lib_dir))?;
    let mut ws = Workspace::new();
    let mut counters = [0u64; 3];
    let shadow_ret = a.run.join("ret-t1shadow");
    std::fs::create_dir_all(&shadow_ret)
        .map_err(|e| defect(format!("{}: {e}", shadow_ret.display())))?;
    let mut lines: Vec<Line> = Vec::with_capacity(hs.len());
    for h in hs {
        let row = universe
            .iter()
            .find(|r| r.key == h.symbol_key)
            .ok_or_else(|| data(format!("{}: not in the universe", h.symbol_key)))?;
        let t1 = shadow(&h, row, &mut libs, &mut ws, &mut counters)?;
        if let Ok(r) = &t1 {
            if r.summary.flags & trial_flag::FAULTED == 0 {
                write_ret(&shadow_ret, &h.t2_trial_id, ws.day_returns(0))
                    .map_err(|e| defect(format!("ret-t1shadow: {e}")))?;
            }
        }
        let t2 = t2_file
            .as_ref()
            .and_then(|f| f.find(&h.t2_trial_id))
            .copied();
        let null = t2_file
            .as_ref()
            .and_then(|f| f.find(&h.null_trial_id))
            .copied();
        let script = script_of(&h);
        let delta = t1.as_ref().ok().map(|r| {
            let mut d = delta_of(r, t2.as_ref(), null.as_ref());
            // pin:07-hurdle-fail-from-verdict: the host ORs HURDLE_FAIL in when the screen's
            // hurdle verdict for (script, symbol, tf) was `annotated`.
            if hurdles.iter().any(|x| {
                x.0 == script && x.1 == h.symbol_key && x.2 == h.timeframe_s && x.3 == "annotated"
            }) {
                d.flags |= delta_flag::HURDLE_FAIL;
            }
            d
        });
        let corr = match (
            read_ret(&ret_path(&shadow_ret, &h.t2_trial_id)),
            read_ret(&ret_path(&h.ret_out, &h.t2_trial_id)),
        ) {
            (Ok(x), Ok(y)) if t2.is_some() => pearson(&x, &y),
            _ => None,
        };
        let agree = match &a.trade_bars {
            Some(tb) => bar_agreement(&h, row, tb, &rd.bars_root),
            None => None,
        };
        lines.push(Line {
            h,
            script,
            t1,
            t2,
            null,
            delta,
            corr,
            agree,
        });
    }
    // rows.t1shadow (tier 1, the same notional and fee set), atomically.
    let hdr = RowsHdr::new(1, rd.notional_1e6, rd.fee_digest);
    let tmp = a.run.join("rows.t1shadow.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut w =
        RowsWriter::open(&tmp, &hdr).map_err(|e| defect(format!("rows.t1shadow: {e:?}")))?;
    let mut shadow_rows = 0u32;
    for l in &lines {
        if let Ok(r) = &l.t1 {
            w.append(r)
                .map_err(|e| defect(format!("rows.t1shadow append: {e:?}")))?;
            shadow_rows += 1;
        }
    }
    w.finish()
        .map_err(|e| defect(format!("rows.t1shadow: {e:?}")))?;
    std::fs::rename(&tmp, a.run.join("rows.t1shadow"))
        .map_err(|e| defect(format!("rows.t1shadow: {e}")))?;
    // Aggregates, per script (first-appearance order = script order).
    let mut scripts: Vec<String> = Vec::new();
    for l in &lines {
        if !scripts.contains(&l.script) {
            scripts.push(l.script.clone());
        }
    }
    let mut scratch = Vec::new();
    let mut aggs: Vec<(String, Aggregate)> = Vec::new();
    for s in &scripts {
        let rows: Vec<DeltaRow> = lines
            .iter()
            .filter(|l| &l.script == s)
            .filter_map(|l| l.delta)
            .collect();
        aggs.push((s.clone(), aggregate(&rows, &mut scratch)));
    }
    let rdir = a.run.join("report");
    std::fs::create_dir_all(&rdir).map_err(|e| defect(format!("{}: {e}", rdir.display())))?;
    let run_name = a.run.file_name().map_or_else(
        || a.run.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let txt = delta_txt(&run_name, &rd, &lines, &aggs);
    let wr = |name: &str, body: &str| {
        std::fs::write(rdir.join(name), body).map_err(|e| defect(format!("report/{name}: {e}")))
    };
    wr("delta.txt", &txt)?;
    wr("delta.json", &delta_json(&run_name, &rd, &lines, &aggs))?;
    let edges = wiring(
        &a.run,
        &rd,
        &lines,
        &universe,
        &mut libs,
        t2_file.as_ref(),
        shadow_rows,
        counters,
        a.trade_bars.is_some(),
    )?;
    wr("wiring.json", &wiring_json(&edges))?;
    wr(
        "throughput.json",
        &throughput_json(&a.run.join("throughput.tsv")),
    )?;
    let found = lines.iter().filter(|l| l.t2.is_some()).count();
    let mut out = String::new();
    if a.text {
        out.push_str(&txt);
        out.push('\n');
        out.push_str(&wiring_text(&edges));
    }
    let _ = writeln!(
        out,
        "report\t{}\thandoffs={}\tshadow_rows={shadow_rows}\tt2_rows={}\tt2_found={found}\t{}",
        a.run.display(),
        lines.len(),
        if t2_file.is_some() {
            "present"
        } else {
            "absent"
        },
        rdir.display()
    );
    Ok(out)
}

/// Trade-print bars vs the candles over the hand-off's window span: (compared, differing).
// pin:07-bar-agreement: bars are matched by `open_ms` over `[first window from, last window to)`;
// "differ by more than one tick" is any of O/H/L/C differing by more than `mintick_1e6`. Late
// prints (§4.2.4) are not recorded in a bar file; the wiring line says so.
fn bar_agreement(
    h: &Handoff,
    row: &UniverseRow,
    trades: &Path,
    candles: &Path,
) -> Option<(u64, u64)> {
    let tp = bar_path(trades, &h.symbol_key, h.timeframe_s).ok()?;
    let cp = bar_path(candles, &h.symbol_key, h.timeframe_s).ok()?;
    let (tf, cf) = (BarFile::open(&tp).ok()?, BarFile::open(&cp).ok()?);
    let from = h.windows.first()?.from_ms;
    let to = h.windows.last()?.to_ms;
    let (tr, cr) = (tf.rows(), cf.rows());
    let (mut i, mut j) = (tf.seek(from), cf.seek(from));
    let (mut n, mut diff) = (0u64, 0u64);
    let tick = row.mintick_1e6.max(1);
    while i < tr.len() && j < cr.len() && tr[i].open_ms < to && cr[j].open_ms < to {
        match tr[i].open_ms.cmp(&cr[j].open_ms) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let (x, y) = (&tr[i], &cr[j]);
                n += 1;
                if (x.open_1e6 - y.open_1e6).abs() > tick
                    || (x.high_1e6 - y.high_1e6).abs() > tick
                    || (x.low_1e6 - y.low_1e6).abs() > tick
                    || (x.close_1e6 - y.close_1e6).abs() > tick
                {
                    diff += 1;
                }
                i += 1;
                j += 1;
            }
        }
    }
    Some((n, diff))
}

fn b4(v: i64) -> String {
    format!("{:+.1}", v as f64 / 1e4)
}

fn ob4(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_owned(), b4)
}

const FLAG_NAMES: [(u32, &str); 5] = [
    (delta_flag::THIN, "THIN"),
    (delta_flag::T2_PENDING, "T2_PENDING"),
    (delta_flag::NOT_JUDGED, "NOT_JUDGED"),
    (delta_flag::INTRABAR_APPROX, "INTRABAR_APPROX"),
    (delta_flag::HURDLE_FAIL, "HURDLE_FAIL"),
];

fn flag_names(f: u32) -> Vec<&'static str> {
    FLAG_NAMES
        .iter()
        .filter(|(b, _)| f & b != 0)
        .map(|(_, n)| *n)
        .collect()
}

fn tf_label(tf: u32) -> String {
    match tf {
        t if t % 604_800 == 0 => format!("{}w", t / 604_800),
        t if t % 86_400 == 0 => format!("{}d", t / 86_400),
        t if t % 3_600 == 0 => format!("{}h", t / 3_600),
        t if t % 60 == 0 => format!("{}m", t / 60),
        t => format!("{t}s"),
    }
}

/// `report/delta.txt` (§4.9's table shape).
// pin:07-trd-is-closed: the table's `trd` columns print `closed` (what THIN reads).
// pin:07-fee-printed-negative: `fee_bps_1e4` is the cost (`net = gross − fee`); the table prints
// it negated, as §4.9's shape does (`t1_fee -28.1`).
fn delta_txt(run: &str, rd: &RunDir, lines: &[Line], aggs: &[(String, Aggregate)]) -> String {
    let mut s = String::new();
    let dg = hex(&rd.fee_digest);
    let _ = writeln!(
        s,
        "PVM-7 tier-1 -> tier-2 delta     run {run}   notional ${}   fees: {} (digest {}…)",
        rd.notional_1e6 / 1_000_000,
        rd.fee_flags.join(" "),
        &dg[..4]
    );
    let _ = writeln!(
        s,
        "policy: CAPTURE_FRACTION {:.2}  HURDLE_MARGIN {:.2}  TARGET_CLOSED {TARGET_CLOSED_T2}  MIN_CLOSED {MIN_CLOSED_T2}\n",
        policy::CAPTURE_FRACTION_1E4 as f64 / 1e4,
        policy::HURDLE_MARGIN_1E4 as f64 / 1e4
    );
    let _ = writeln!(
        s,
        "{:<18} {:<24} {:>3} {:>5} | {:>6} {:>9} {:>7} {:>7} | {:>6} {:>9} {:>7} {:>7} | {:>7} {:>5} {:>6} {:>5}  flags",
        "script", "sym", "tf", "days", "t1_trd", "t1_gross", "t1_fee", "t1_net", "t2_trd", "t2_gross",
        "t2_fee", "t2_net", "d_net", "surv", "null", "corr"
    );
    for l in lines {
        let h = &l.h;
        let days = (h.score_to_ms - h.score_from_ms) / DAY_MS;
        let head = format!(
            "{:<18} {:<24} {:>3} {:>5}",
            l.script,
            h.symbol_key,
            tf_label(h.timeframe_s),
            days
        );
        match (&l.t1, &l.delta) {
            (Ok(t1), Some(d)) => {
                let m = &t1.summary;
                let (t2c, t2g, t2f) = match (&l.t2, d.t2_net_bps_1e4) {
                    (Some(r), Some(_)) => (
                        r.summary.closed.to_string(),
                        b4(r.summary.gross_bps_1e4),
                        b4(-r.summary.fee_bps_1e4),
                    ),
                    (Some(r), None) => (r.summary.closed.to_string(), "-".into(), "-".into()),
                    _ => ("-".into(), "-".into(), "-".into()),
                };
                let fl = flag_names(d.flags);
                let _ = writeln!(
                    s,
                    "{head} | {:>6} {:>9} {:>7} {:>7} | {:>6} {:>9} {:>7} {:>7} | {:>7} {:>5} {:>6} {:>5}  {}",
                    m.closed,
                    b4(m.gross_bps_1e4),
                    b4(-m.fee_bps_1e4),
                    b4(m.net_bps_1e4),
                    t2c,
                    t2g,
                    t2f,
                    ob4(d.t2_net_bps_1e4),
                    ob4(d.d_net_bps_1e4),
                    d.survival_1e4
                        .map_or_else(|| "-".to_owned(), |v| format!("{:.2}", v as f64 / 1e4)),
                    ob4(d.null_net_bps_1e4),
                    l.corr.map_or_else(|| "-".to_owned(), |c| format!("{c:.2}")),
                    if fl.is_empty() { "-".to_owned() } else { fl.join(",") }
                );
            }
            _ => {
                let why = l.t1.as_ref().err().map_or("-", String::as_str);
                let _ = writeln!(s, "{head} | shadow run refused: {why}");
            }
        }
    }
    let _ = writeln!(
        s,
        "units: bps of the notional (printed ÷1e4, one decimal).  d_net = t2_net − t1_net.\nsurv = t2_net / t1_net, only when t1_net > 0 and not THIN.  corr = Pearson of the daily series.  Rows ordered by script, symbol, timeframe.\n"
    );
    let _ = writeln!(
        s,
        "per-script aggregate over rows without THIN, T2_PENDING, NOT_JUDGED, INTRABAR_APPROX, HURDLE_FAIL"
    );
    let _ = writeln!(
        s,
        "{:<18} {:>3} {:>14} {:>11} {:>13}   excluded(thin,pending,notjudged,approx,hurdle)",
        "script", "n", "median_d_net", "iqr_d_net", "median_surv"
    );
    for (sc, g) in aggs {
        let (md, iq) = if g.n == 0 {
            ("-".to_owned(), "-".to_owned())
        } else {
            (b4(g.median_d_net_bps_1e4), b4(g.iqr_d_net_bps_1e4))
        };
        let e = g.excluded;
        let _ = writeln!(
            s,
            "{:<18} {:>3} {:>14} {:>11} {:>13}   {},{},{},{},{}",
            sc,
            g.n,
            md,
            iq,
            g.median_survival_1e4
                .map_or_else(|| "-".to_owned(), |v| format!("{:.2}", v as f64 / 1e4)),
            e[0],
            e[1],
            e[2],
            e[3],
            e[4]
        );
    }
    s.push_str("NO RANKING IS PRINTED.\n");
    s
}

fn jopt(v: Option<i64>) -> String {
    v.map_or_else(|| "null".to_owned(), |x| x.to_string())
}

fn jrow(s: &mut String, r: Option<&RowRec>) {
    match r {
        None => s.push_str("null"),
        Some(r) => {
            let m = &r.summary;
            s.push_str("{\"trial_id\":");
            json_str(s, &hex(&r.trial_id));
            let _ = write!(
                s,
                ",\"bars\":{},\"trades\":{},\"closed\":{},\"obs_days\":{},\"gross_bps_1e4\":{},\"fee_bps_1e4\":{},\"net_bps_1e4\":{},\"max_concurrent_1e6\":{},\"turnover_day_p99_1e6\":{},\"flags\":{}}}",
                m.bars, m.trades, m.closed, m.obs_days, m.gross_bps_1e4, m.fee_bps_1e4,
                m.net_bps_1e4, m.max_concurrent_1e6, m.turnover_day_p99_1e6, m.flags
            );
        }
    }
}

fn delta_json(run: &str, rd: &RunDir, lines: &[Line], aggs: &[(String, Aggregate)]) -> String {
    let mut s = String::from("{\"schema\":");
    json_str(&mut s, JSON_SCHEMA);
    s.push_str(",\"verb\":\"report\",\"deliverable\":\"delta\",\"run\":");
    json_str(&mut s, run);
    let _ = write!(s, ",\"notional_1e6\":{},\"fee_flags\":[", rd.notional_1e6);
    for (i, f) in rd.fee_flags.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json_str(&mut s, f);
    }
    s.push_str("],\"fee_digest\":");
    json_str(&mut s, &hex(&rd.fee_digest));
    let _ = write!(
        s,
        ",\"policy\":{{\"capture_fraction_1e4\":{},\"hurdle_margin_1e4\":{},\"target_closed\":{TARGET_CLOSED_T2},\"min_closed\":{MIN_CLOSED_T2}}},\"rows\":[",
        policy::CAPTURE_FRACTION_1E4,
        policy::HURDLE_MARGIN_1E4
    );
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let h = &l.h;
        s.push_str("{\"script\":");
        json_str(&mut s, &l.script);
        s.push_str(",\"symbol\":");
        json_str(&mut s, &h.symbol_key);
        let _ = write!(
            s,
            ",\"timeframe_s\":{},\"mode\":\"{}\",\"score_from_ms\":{},\"score_to_ms\":{},\"windows\":{},\"t1_trial_id\":",
            h.timeframe_s,
            mode_label(h.mode),
            h.score_from_ms,
            h.score_to_ms,
            h.windows.len()
        );
        json_str(&mut s, &hex(&h.t1_trial_id));
        s.push_str(",\"t2_trial_id\":");
        json_str(&mut s, &hex(&h.t2_trial_id));
        s.push_str(",\"null_trial_id\":");
        json_str(&mut s, &hex(&h.null_trial_id));
        s.push_str(",\"t1_shadow\":");
        jrow(&mut s, l.t1.as_ref().ok());
        s.push_str(",\"shadow_refusal\":");
        match &l.t1 {
            Err(w) => json_str(&mut s, w),
            Ok(_) => s.push_str("null"),
        }
        s.push_str(",\"t2\":");
        jrow(&mut s, l.t2.as_ref());
        s.push_str(",\"null\":");
        jrow(&mut s, l.null.as_ref());
        match &l.delta {
            Some(d) => {
                let _ = write!(
                    s,
                    ",\"t1_net_bps_1e4\":{},\"t2_net_bps_1e4\":{},\"d_net_bps_1e4\":{},\"survival_1e4\":{},\"null_net_bps_1e4\":{},\"flags\":[",
                    d.t1_net_bps_1e4,
                    jopt(d.t2_net_bps_1e4),
                    jopt(d.d_net_bps_1e4),
                    jopt(d.survival_1e4),
                    jopt(d.null_net_bps_1e4)
                );
                for (k, n) in flag_names(d.flags).iter().enumerate() {
                    if k > 0 {
                        s.push(',');
                    }
                    json_str(&mut s, n);
                }
                s.push(']');
            }
            None => s.push_str(",\"flags\":[]"),
        }
        let _ = write!(
            s,
            ",\"corr\":{},\"bar_agreement\":{}}}",
            l.corr
                .filter(|c| c.is_finite())
                .map_or_else(|| "null".to_owned(), |c| format!("{c:.6}")),
            l.agree.map_or_else(
                || "null".to_owned(),
                |(n, d)| format!("{{\"compared\":{n},\"differ_gt_1_tick\":{d}}}")
            )
        );
    }
    s.push_str("],\"aggregate\":[");
    for (i, (sc, g)) in aggs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"script\":");
        json_str(&mut s, sc);
        let e = g.excluded;
        let _ = write!(
            s,
            ",\"n\":{},\"median_d_net_bps_1e4\":{},\"iqr_d_net_bps_1e4\":{},\"median_survival_1e4\":{},\"excluded\":{{\"thin\":{},\"t2_pending\":{},\"not_judged\":{},\"intrabar_approx\":{},\"hurdle_fail\":{}}}}}",
            g.n,
            if g.n == 0 { "null".to_owned() } else { g.median_d_net_bps_1e4.to_string() },
            if g.n == 0 { "null".to_owned() } else { g.iqr_d_net_bps_1e4.to_string() },
            jopt(g.median_survival_1e4),
            e[0],
            e[1],
            e[2],
            e[3],
            e[4]
        );
    }
    s.push_str("],\"ranking\":null}\n");
    s
}

/// The wiring edges (§4.10). An edge this verb cannot measure is `ok: null` with the reason.
#[allow(clippy::too_many_arguments)]
fn wiring(
    run: &Path,
    rd: &RunDir,
    lines: &[Line],
    universe: &[UniverseRow],
    libs: &mut pine_sema::LibCache,
    t2: Option<&RowsFile>,
    shadow_rows: u32,
    counters: [u64; 3],
    trade_bars: bool,
) -> Result<Vec<Edge>, PineErr> {
    let mut e = Vec::new();
    // source_is_ours
    let priors = match &rd.priors {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    let screened: Vec<(String, String)> = priors
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() == 4).then(|| (f[0].to_owned(), f[3].to_owned()))
        })
        .collect();
    let (mut files, mut spdx) = (0u32, 0u32);
    if let Some(dir) = &rd.scripts {
        if let Ok(rdir) = std::fs::read_dir(dir) {
            for ent in rdir.filter_map(Result::ok) {
                let n = ent.file_name().to_string_lossy().into_owned();
                if n.starts_with("pvm7_") && n.ends_with(".pine") {
                    files += 1;
                    let t = std::fs::read_to_string(ent.path()).unwrap_or_default();
                    let mut it = t.lines();
                    if it.next() == Some("// SPDX-License-Identifier: Apache-2.0")
                        && it.next().is_some_and(|l| l.starts_with("// Copyright"))
                    {
                        spdx += 1;
                    }
                }
            }
        }
    }
    e.push(Edge {
        edge: "source_is_ours",
        ok: Some(files > 0 && spdx == files),
        evidence: format!(
            "{files} files pvm7_*.pine ({} screened), {spdx} SPDX pairs on lines 1-2; tracked-file check under docs/research/ not run (no git in this verb)",
            screened.len()
        ),
    });
    // parse / compile / verify / builtins: each screened script once, on the first chart.
    let (mut parsed, mut compiled, mut verified) = (0u32, 0u32, 0u32);
    let (mut bc, mut rt) = (0u32, 0u32);
    let mut frames: Vec<u32> = Vec::new();
    let mut reached: BTreeSet<OverloadId> = BTreeSet::new();
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let first = universe.first();
    let tf0 = rd.timeframes.first().copied().unwrap_or(3600);
    for (name, builtins) in &screened {
        for b in builtins.split(',') {
            if !b.is_empty() {
                declared.insert(b.to_owned());
            }
        }
        let (Some(row), Some(dir)) = (first, &rd.scripts) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(dir.join(format!("{name}.pine"))) else {
            continue;
        };
        let spec = TrialSpec {
            source: &src,
            symbol_key: &row.key,
            chart: chart_info(row, tf0),
            syminfo_strs: [
                row.ticker.as_str(),
                row.basecurrency.as_str(),
                row.currency.as_str(),
                row.sym_type.as_str(),
            ],
            inputs: &[],
            ticker_map: &[],
            limits: SCREEN_LIMITS,
        };
        match compile_trial(&spec, libs) {
            Ok(t) => {
                parsed += 1;
                compiled += 1;
                verified += 1;
                match t.mode() {
                    ExecMode::BarClose => bc += 1,
                    ExecMode::Realtime => rt += 1,
                }
                frames.push(t.program().layout.total_bytes);
                reached.extend(t.overloads_reached().iter().copied());
            }
            Err(CompileRefusal::Parse(_)) => {}
            Err(CompileRefusal::Verify(_)) => {
                parsed += 1;
                compiled += 1;
            }
            Err(_) => parsed += 1,
        }
    }
    let n = screened.len() as u32;
    frames.sort_unstable();
    let fr = if frames.is_empty() {
        "-".to_owned()
    } else {
        format!(
            "{} / {} / {}",
            frames[0],
            frames[(frames.len() - 1) / 2],
            frames[frames.len() - 1]
        )
    };
    e.push(Edge {
        edge: "parse",
        ok: Some(n > 0 && parsed == n),
        evidence: format!("{parsed}/{n} parse Ok"),
    });
    e.push(Edge {
        edge: "compile",
        ok: Some(n > 0 && compiled == n),
        evidence: format!("{compiled}/{n} compile Ok (live_bound); {bc} BarClose, {rt} Realtime"),
    });
    e.push(Edge {
        edge: "verify",
        ok: Some(n > 0 && verified == n),
        evidence: format!("{verified}/{n} verify Ok; frame bytes min / med / max {fr}"),
    });
    // pin:07-builtins-by-id: an OverloadId → Pine-name table is pine-std's, not this crate's; the
    // line counts distinct overload ids reached against the distinct names declared in the priors
    // and does not list the unreached by name (ok = null).
    e.push(Edge {
        edge: "builtins",
        ok: None,
        evidence: format!(
            "{} distinct overloads reached; {} distinct builtins declared in the priors; unreached-by-name not computed here",
            reached.len(),
            declared.len()
        ),
    });
    // bars_prepared / bars_mapped / barclock
    let (mut f_ok, mut rows_n, mut holes, mut aligned) = (0u32, 0u64, 0u64, 0u64);
    let mut digests = BTreeSet::new();
    for (key, tf, dg) in &rd.bar_files {
        digests.insert(dg.clone());
        let Ok(p) = bar_path(&rd.bars_root, key, *tf) else {
            continue;
        };
        let Ok(f) = BarFile::open(&p) else {
            continue;
        };
        f_ok += 1;
        holes += u64::from(f.hdr().holes);
        for r in f.rows() {
            rows_n += 1;
            aligned += u64::from(barclock::open_ms(r.open_ms, *tf) == r.open_ms);
        }
    }
    let nf = rd.bar_files.len() as u32;
    e.push(Edge {
        edge: "bars_prepared",
        ok: Some(nf > 0 && f_ok == nf),
        evidence: format!(
            "{nf} files, {rows_n} rows, {holes} holes, {} src_digest64 distinct; dropped partial buckets: prepare's count, not in the files",
            digests.len()
        ),
    });
    e.push(Edge {
        edge: "bars_mapped",
        ok: Some(nf > 0 && f_ok == nf),
        evidence: format!("{f_ok}/{nf} files opened through BarFile::open (read-only map); pine-screen holds no SQLite code (static, not re-checked here)"),
    });
    e.push(Edge {
        edge: "barclock",
        ok: Some(rows_n > 0 && aligned == rows_n),
        evidence: format!("{aligned}/{rows_n} rows satisfy open_ms(open, tf) == open"),
    });
    let agree: Vec<(u64, u64)> = lines.iter().filter_map(|l| l.agree).collect();
    e.push(Edge {
        edge: "bar_agreement",
        ok: if agree.is_empty() { None } else { Some(true) },
        evidence: if !trade_bars {
            "no --trade-bars root given".to_owned()
        } else if agree.is_empty() {
            "no trade-print bar file for any hand-off span".to_owned()
        } else {
            let (c, d) = agree.iter().fold((0, 0), |a, x| (a.0 + x.0, a.1 + x.1));
            format!("{} spans: {c} bars compared, {d} differ by more than one tick; late prints not in bar files", agree.len())
        },
    });
    // hurdle
    let hv = read_hurdle_verdicts(run)?;
    let mut by_tf: Vec<(u32, u32)> = Vec::new();
    for h in &hv {
        if h.3 == "drop" {
            match by_tf.iter_mut().find(|x| x.0 == h.2) {
                Some(x) => x.1 += 1,
                None => by_tf.push((h.2, 1)),
            }
        }
    }
    by_tf.sort_unstable();
    let dropped: u32 = by_tf.iter().map(|x| x.1).sum();
    let mut bt = String::new();
    for (tf, k) in &by_tf {
        let _ = write!(bt, " {}:{k}", tf_label(*tf));
    }
    e.push(Edge {
        edge: "hurdle",
        ok: Some(!hv.is_empty()),
        evidence: format!(
            "{} combinations, {dropped} dropped (by timeframe:{}), 0 without a fee row (I22 refuses the screen otherwise)",
            hv.len(),
            if bt.is_empty() { " none".to_owned() } else { bt }
        ),
    });
    // tier1
    let trials = read_trials(run)?;
    let faults = std::fs::read_to_string(run.join("faults.log"))
        .map(|t| t.lines().count())
        .unwrap_or(0);
    let steps = std::fs::read_to_string(run.join("throughput.tsv"))
        .map(|t| {
            t.lines()
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    (f.len() == 9 && f[0] != "mode")
                        .then(|| f[4].parse::<u64>().ok())
                        .flatten()
                })
                .sum::<u64>()
        })
        .unwrap_or(0);
    let (mut approx, mut unj) = (0u32, 0u32);
    if let Ok(f) = RowsFile::open(&run.join("rows.t1")) {
        for r in f.recs() {
            approx += u32::from(r.summary.flags & trial_flag::INTRABAR_APPROX != 0);
            unj += u32::from(r.summary.flags & trial_flag::UNJUDGED != 0);
        }
    }
    e.push(Edge {
        edge: "tier1",
        ok: Some(!trials.is_empty() && faults == 0 && unj == 0),
        evidence: format!(
            "{} trials, {steps} bar-steps, {faults} faults, {approx} INTRABAR_APPROX, {unj} UNJUDGED rows"
            ,
            trials.len()
        ),
    });
    // pin:07-intents-from-shadow: the screen persists no RunRecord, so the BrokerCounters sums are
    // the shadow runs' (this verb's own tier-1 runs).
    e.push(Edge {
        edge: "intents",
        ok: Some(counters[0] > 0),
        evidence: format!(
            "shadow runs: {} intents_committed, {} intents_modified, {} sized_to_zero",
            counters[0], counters[1], counters[2]
        ),
    });
    // handoff
    let (mut w, mut pull, mut bytes) = (0usize, 0u64, 0u64);
    for l in lines {
        w += l.h.windows.len();
        if let Ok(t) = std::fs::read_to_string(l.h.dir.join("cost.tsv")) {
            for line in t.lines() {
                match line.split_once('\t') {
                    Some(("runs_to_pull", v)) => pull += v.parse::<u64>().unwrap_or(0),
                    Some(("est_bytes", v)) => bytes += v.parse::<u64>().unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    e.push(Edge {
        edge: "handoff",
        ok: Some(!lines.is_empty()),
        evidence: format!(
            "{} survivors, {w} windows, {pull} runs to pull, est. {bytes} bytes",
            lines.len()
        ),
    });
    // tier2
    let found = lines.iter().filter(|l| l.t2.is_some()).count();
    let unj2 = lines
        .iter()
        .filter_map(|l| l.t2.as_ref())
        .filter(|r| r.summary.flags & trial_flag::UNJUDGED != 0)
        .count();
    e.push(Edge {
        edge: "tier2",
        ok: t2.map(|_| found == lines.len()),
        evidence: match t2 {
            None => "rows.t2 absent (B'3 has not run): every row T2_PENDING".to_owned(),
            Some(_) => format!(
                "rows.t2 present, {found} of {} rows found, {unj2} UNJUDGED, 0 header mismatches",
                lines.len()
            ),
        },
    });
    // report
    let mut ids: BTreeSet<[u8; 32]> = BTreeSet::new();
    for l in lines {
        ids.insert(l.h.t2_trial_id);
        ids.insert(l.h.null_trial_id);
    }
    let orphans = t2.map_or(0, |f| {
        f.recs()
            .iter()
            .filter(|r| !ids.contains(&r.trial_id))
            .count()
    });
    e.push(Edge {
        edge: "report",
        ok: Some(shadow_rows as usize == lines.len() && orphans == 0),
        evidence: format!("{shadow_rows} shadow rows, {found} joined, {orphans} orphans"),
    });
    Ok(e)
}

fn wiring_json(edges: &[Edge]) -> String {
    let mut s = String::from("{\"schema\":");
    json_str(&mut s, JSON_SCHEMA);
    s.push_str(",\"verb\":\"report\",\"deliverable\":\"wiring\",\"edges\":[");
    for (i, x) in edges.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"edge\":");
        json_str(&mut s, x.edge);
        let _ = write!(
            s,
            ",\"ok\":{},\"evidence\":",
            x.ok.map_or("null", |b| if b { "true" } else { "false" })
        );
        json_str(&mut s, &x.evidence);
        s.push('}');
    }
    s.push_str("]}\n");
    s
}

fn wiring_text(edges: &[Edge]) -> String {
    let mut s = String::from("edge                ok  counter / evidence\n");
    for x in edges {
        let mark = match x.ok {
            Some(true) => "ok",
            Some(false) => "NO",
            None => "-",
        };
        let _ = writeln!(s, "{:<19} {:<3} {}", x.edge, mark, x.evidence);
    }
    s
}

/// `report/throughput.json` from the screen's `throughput.tsv` (§4.11; the meter is `run.rs`'s).
fn throughput_json(p: &Path) -> String {
    let mut s = String::from("{\"schema\":");
    json_str(&mut s, JSON_SCHEMA);
    s.push_str(",\"verb\":\"report\",\"deliverable\":\"throughput\",");
    let Ok(t) = std::fs::read_to_string(p) else {
        s.push_str("\"host\":null,\"rows\":[],\"knee\":null,\"note\":\"no throughput.tsv in the run directory\"}\n");
        return s;
    };
    let mut host = String::new();
    let mut knee = String::new();
    let mut rows = Vec::new();
    for l in t.lines() {
        if l.starts_with("host ") {
            host = l.to_owned();
        } else if l.starts_with("knee:") {
            knee = l.to_owned();
        } else {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() == 9 && f[0] != "mode" {
                rows.push(f);
            }
        }
    }
    s.push_str("\"host\":");
    json_str(&mut s, &host);
    s.push_str(",\"rows\":[");
    for (i, f) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"mode\":");
        json_str(&mut s, f[0]);
        let num = |x: &str| {
            if x.parse::<f64>().is_ok_and(f64::is_finite) {
                x.to_owned()
            } else {
                "null".to_owned()
            }
        };
        let _ = write!(
            s,
            ",\"k\":{},\"instances\":{},\"bars_read\":{},\"bar_steps\":{},\"wall_s\":{},\"bars_per_s_core\":{},\"bar_steps_per_s_core\":{},\"ws_bytes\":{}}}",
            num(f[1]),
            num(f[2]),
            num(f[3]),
            num(f[4]),
            num(f[5]),
            num(f[6]),
            num(f[7]),
            num(f[8])
        );
    }
    s.push_str("],\"knee\":");
    json_str(&mut s, &knee);
    s.push_str("}\n");
    s
}
