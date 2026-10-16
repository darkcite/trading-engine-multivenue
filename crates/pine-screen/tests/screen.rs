// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.6 — the compile, commit, fault, determinism and flag tests, and T9's sub-program,
//! lookahead and interleave tests. Probe scripts are inline or under `tests/probes/` (ours).

mod common;

use common::*;
use pine_ir::{barflag, op, ExecMode, InputOverride, InputValue, ProgramMeta, StrategyDecl};
use pine_screen::{
    run_group, run_trial, screen_decl, CompileRefusal, FaultOrigin, Loaded, NoProbe, Refusal,
    RowRec, ScreenErr, TrialOutcome, TrialStatus, Workspace, CAPITAL_UNITS,
};

const SMA_CROSS: &str = "//@version=6
strategy(\"sma cross\", overlay=true)
f = ta.sma(close, 10)
s = ta.sma(close, 30)
if ta.crossover(f, s)
    strategy.entry(\"L\", strategy.long)
if ta.crossunder(f, s)
    strategy.close(\"L\")
plot(f)
";

/// A loop of 10^8 iterations at bar_index 50 exhausts the 2 000 000 budget.
const BUDGET_AT_50: &str = "//@version=6
strategy(\"budget\")
var float x = 0.0
if bar_index == 50
    for i = 0 to 100000000
        x += 1
plot(x)
";

/// Re-emits one far limit order on every evaluation.
const COMMIT_PROBE: &str = "//@version=6
strategy(\"commit\")
strategy.entry(\"L\", strategy.long, limit = 1.0)
";

/// A far limit order only when `barstate.islast`.
const LAST_FLAG: &str = "//@version=6
strategy(\"last\")
if barstate.islast
    strategy.entry(\"L\", strategy.long, limit = 1.0)
";

const WITH_INPUT: &str = "//@version=6
strategy(\"inp\")
len = input.int(14, \"len\")
plot(ta.sma(close, len))
";

fn bars() -> pine_screen::TrialBars {
    bars_of(&sine_closes(720))
}

fn ok(src: &str) -> Loaded {
    compile(src).unwrap_or_else(|e| panic!("{e:?} code {:?}\n{src}", e.code()))
}

#[test]
fn compile_trial_is_live_bound() {
    // lookahead_leak.pine: request.security "240" close, lookahead_on, no offset → Compile([3043]).
    match compile(&probe_src("lookahead_leak.pine")) {
        Err(CompileRefusal::Compile(ds)) => {
            let codes: Vec<u16> = ds.iter().map(|d| d.code.0).collect();
            assert!(codes.contains(&3043), "{codes:?}");
        }
        Err(e) => panic!("expected Compile([3043]), got {e:?}"),
        Ok(_) => panic!("a leaking lookahead compiled under live_bound"),
    }
}

#[test]
fn compile_trial_retries_realtime_on_3052() {
    let tick = SMA_CROSS.replace(
        "strategy(\"sma cross\", overlay=true)",
        "strategy(\"sma cross\", overlay=true, calc_on_every_tick=true)",
    );
    let t = ok(&tick);
    assert_eq!(t.mode(), ExecMode::Realtime);
    assert!(t.meta().uses_intrabar);
    assert_eq!(ok(SMA_CROSS).mode(), ExecMode::BarClose);
}

#[test]
fn compile_trial_refuses_an_unknown_input_6004() {
    let nope = [InputOverride {
        name: "nope".to_owned(),
        value: InputValue::Int(3),
    }];
    match compile_with(WITH_INPUT, &nope) {
        Err(e @ CompileRefusal::Inputs(_)) => {
            assert_eq!(e.code().0, 6004);
            if let CompileRefusal::Inputs(names) = e {
                assert_eq!(names, vec!["nope".to_owned()]);
            }
        }
        Err(e) => panic!("expected Inputs, got {e:?}"),
        Ok(_) => panic!("an unknown override was ignored"),
    }
    let len = [InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(20),
    }];
    let a = compile_with(WITH_INPUT, &len).expect("a known override compiles");
    let b = compile(WITH_INPUT).expect("defaults compile");
    assert_ne!(
        a.input_digest(),
        b.input_digest(),
        "the digest is of the inputs in effect"
    );
    let same = [InputOverride {
        name: "len".to_owned(),
        value: InputValue::Int(14),
    }];
    let c = compile_with(WITH_INPUT, &same).expect("compiles");
    assert_eq!(
        c.input_digest(),
        b.input_digest(),
        "a variant that folds to the defaults"
    );
}

#[test]
fn compile_trial_is_deterministic() {
    let a = ok(SMA_CROSS);
    let b = ok(SMA_CROSS);
    assert_eq!(a.program().code, b.program().code);
    assert_eq!(a.program().consts, b.program().consts);
    assert_eq!(a.program().layout, b.program().layout);
    assert_eq!(a.input_digest(), b.input_digest());
    assert_eq!(a.source_sha256(), b.source_sha256());
    assert_eq!(
        a.source_sha256(),
        &core_crypto::sha256(SMA_CROSS.as_bytes())
    );
    assert_eq!(a.overloads_reached(), b.overloads_reached());
    assert!(
        a.overloads_reached().windows(2).all(|w| w[0] < w[1]),
        "sorted, distinct"
    );
}

#[test]
fn compile_trial_refuses_a_parse_error() {
    let e = compile("//@version=6\nstrategy(\"x\"\nplot(close)\n")
        .err()
        .expect("refused");
    assert!(matches!(e, CompileRefusal::Parse(_)), "{e:?}");
    assert!((1000..3002).contains(&e.code().0), "{:?}", e.code());
}

#[test]
fn loaded_verify_refusal_is_ours() {
    let good = ok(SMA_CROSS);
    let mut p = good.program().clone();
    // Drop the final HALT: the verifier refuses a program that can run off its end.
    assert_eq!(p.code.last().map(|i| i.op), Some(op::HALT));
    p.code.pop();
    match Loaded::from_program_for_tests(p, SMA_CROSS) {
        Err(CompileRefusal::Verify(e)) => assert_eq!(e.code().0, 4000 + e.rule),
        Err(e) => panic!("expected Verify, got {e:?}"),
        Ok(_) => panic!("the verifier accepted a program without HALT"),
    }
    let ok2 = Loaded::from_program_for_tests(good.program().clone(), SMA_CROSS).expect("verifies");
    assert_eq!(ok2.input_digest(), good.input_digest());
}

#[test]
fn screen_commits_after_every_evaluation() {
    let t = ok(COMMIT_PROBE);
    let b = bars();
    let cfg = cfg(2, 30);
    for k_n in [1usize, 3] {
        let group: Vec<&Loaded> = (0..k_n).map(|_| &t).collect();
        let mut out = vec![TrialOutcome::default(); k_n];
        let mut rec = Rec::default();
        run_group(&group, &b, &cfg, &mut Workspace::new(), &mut out, &mut rec).expect("runs");
        for (k, o) in out.iter().enumerate() {
            assert_eq!(o.status, TrialStatus::Completed);
            let evals = rec.evals.iter().filter(|e| e.k as usize == k).count() as u64;
            assert_eq!(evals, 720);
            let c = &o.record.counters;
            assert_eq!(
                c.intents_committed, evals,
                "K={k_n} k={k}: one commit per evaluation"
            );
            assert_eq!(
                c.intents_modified,
                evals - 1,
                "K={k_n} k={k}: each re-emit is a modify"
            );
        }
    }
}

#[test]
fn screen_fault_ends_only_that_trial() {
    let a = ok(SMA_CROSS);
    let f = ok(BUDGET_AT_50);
    let b = bars();
    let cfg = cfg(2, 30);
    let mut out = vec![TrialOutcome::default(); 3];
    run_group(
        &[&a, &f, &a],
        &b,
        &cfg,
        &mut Workspace::new(),
        &mut out,
        &mut Rec::default(),
    )
    .expect("a script fault is not the call's");
    match out[1].status {
        TrialStatus::Faulted(r) => {
            assert_eq!(r.origin, FaultOrigin::Vm(pine_ir::FaultKind::InsnBudget));
            assert_eq!(r.bar_index, 50);
            assert!(!r.sub);
            assert_ne!(r.pc, u32::MAX);
            assert_eq!(&r.source_sha256, f.source_sha256());
        }
        s => panic!("expected Faulted, got {s:?}"),
    }
    let s = out[1].summary;
    assert_eq!(
        (s.faults, s.obs_days, s.net_bps_1e4, s.fee_bps_1e4),
        (1, 0, 0, 0)
    );
    assert_eq!(
        s.flags,
        pine_ir::trial_flag::FIXED_NOTIONAL | pine_ir::trial_flag::FAULTED
    );
    let alone = run_trial(&a, &b, &cfg, &mut Workspace::new()).expect("runs");
    assert_eq!(out[0], alone, "the faulted neighbour changes nothing");
    assert_eq!(out[2], alone);
}

#[test]
fn screen_rows_do_not_depend_on_k_or_grouping() {
    let a = ok(SMA_CROSS);
    let c = ok(COMMIT_PROBE);
    let l = ok(LAST_FLAG);
    let b = bars();
    let cfg = cfg(2, 30);
    let one = |t: &Loaded| run_trial(t, &b, &cfg, &mut Workspace::new()).expect("runs");
    let singles = [one(&a), one(&c), one(&l)];
    let mut out = vec![TrialOutcome::default(); 3];
    let mut ws = Workspace::new();
    run_group(
        &[&a, &c, &l],
        &b,
        &cfg,
        &mut ws,
        &mut out,
        &mut Rec::default(),
    )
    .expect("runs");
    for k in 0..3 {
        assert_eq!(out[k], singles[k], "k={k}");
        let x = RowRec {
            trial_id: [1; 32],
            summary: out[k].summary,
        }
        .to_le_bytes();
        let y = RowRec {
            trial_id: [1; 32],
            summary: singles[k].summary,
        }
        .to_le_bytes();
        assert_eq!(x, y);
    }
}

#[test]
fn screen_never_sets_unjudged() {
    let b = bars();
    let cfg = cfg(2, 30);
    for src in [SMA_CROSS, COMMIT_PROBE, LAST_FLAG] {
        let o = run_trial(&ok(src), &b, &cfg, &mut Workspace::new()).expect("runs");
        let c = &o.record.counters;
        assert_eq!(
            (
                c.skipped_cap,
                c.skipped_max_open,
                c.ioc_remainder_cancelled,
                c.ioc_sent
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(o.summary.flags & pine_ir::trial_flag::UNJUDGED, 0);
    }
}

#[test]
fn screen_flags_intrabar_programs() {
    let tick = SMA_CROSS.replace("overlay=true)", "overlay=true, calc_on_every_tick=true)");
    let b = bars();
    let cfg = cfg(2, 30);
    let o = run_trial(&ok(&tick), &b, &cfg, &mut Workspace::new()).expect("runs");
    assert_ne!(o.summary.flags & pine_ir::trial_flag::INTRABAR_APPROX, 0);
    let o = run_trial(&ok(SMA_CROSS), &b, &cfg, &mut Workspace::new()).expect("runs");
    assert_eq!(o.summary.flags & pine_ir::trial_flag::INTRABAR_APPROX, 0);
}

#[test]
fn screen_decl_changes_exactly_four_fields() {
    let t = ok(SMA_CROSS);
    let d: StrategyDecl = t.meta().decl;
    let cfg = cfg(2, 30);
    let s = screen_decl(&d, &cfg);
    assert_eq!(s.commission_type, pine_ir::CommissionType::Percent);
    assert_eq!(s.commission_value_1e6, 5 * 10_000);
    assert_eq!(s.slippage_ticks, 0);
    assert_eq!(
        s.initial_capital_1e6,
        CAPITAL_UNITS * i64::from(d.pyramiding.max(1)) * cfg.notional_1e6
    );
    let back = StrategyDecl {
        commission_type: d.commission_type,
        commission_value_1e6: d.commission_value_1e6,
        slippage_ticks: d.slippage_ticks,
        initial_capital_1e6: d.initial_capital_1e6,
        ..s
    };
    assert_eq!(back, d, "nothing else changes");
}

#[test]
fn screen_last_bar_flags() {
    let b = bars();
    for r in b.chart().rows() {
        assert_eq!(
            r.flags & (barflag::LAST | barflag::LAST_CONFIRMED_HISTORY),
            0
        );
    }
    let t = ok(LAST_FLAG);
    let mut out = [TrialOutcome::default()];
    let mut rec = Rec::default();
    run_group(
        &[&t],
        &b,
        &cfg(2, 30),
        &mut Workspace::new(),
        &mut out,
        &mut rec,
    )
    .expect("runs");
    let both = barflag::LAST | barflag::LAST_CONFIRMED_HISTORY;
    let flagged: Vec<u32> = rec
        .evals
        .iter()
        .filter(|e| e.flags & both == both)
        .map(|e| e.index)
        .collect();
    assert_eq!(flagged, vec![719], "only the final stepped row");
    assert_eq!(out[0].record.counters.intents_committed, 1);
    // A window ending earlier moves the flag to its own last row.
    let mut rec = Rec::default();
    run_group(
        &[&t],
        &b,
        &cfg(2, 20),
        &mut Workspace::new(),
        &mut out,
        &mut rec,
    )
    .expect("runs");
    assert_eq!(
        rec.evals.last().map(|e| (e.index, e.flags & both)),
        Some((479, both))
    );
    assert_eq!(rec.evals.len(), 480);
}

#[test]
fn screen_refuses_short_warmup_and_coverage() {
    let t = ok(SMA_CROSS);
    let need = t.meta().warmup_bars;
    assert!(need > 24, "{need}");
    let b = bars();
    let o = run_trial(&t, &b, &cfg(1, 30), &mut Workspace::new()).expect("a refusal is per trial");
    assert_eq!(
        o.status,
        TrialStatus::Refused(Refusal::ShortWarmup { have: 24, need })
    );
    assert_eq!(o.summary, Default::default());
    // Rows stop a day early: 29 days of bars, a window to day 30.
    let short = bars_of(&sine_closes(29 * 24));
    let o = run_trial(&t, &short, &cfg(2, 30), &mut Workspace::new()).expect("per trial");
    assert!(
        matches!(o.status, TrialStatus::Refused(Refusal::Coverage { .. })),
        "{:?}",
        o.status
    );
}

#[test]
fn screen_refuses_a_unit_below_the_step() {
    // qty_step 1 BTC at ~$60 000 with N = $1 000.
    let closes: Vec<i64> = sine_closes(720).iter().map(|c| c * 600).collect();
    let b = bars_of(&closes);
    let mut ch = chart();
    ch.qty_step_1e6 = 1_000_000;
    let tm: Vec<(String, String)> = Vec::new();
    let spec = pine_screen::TrialSpec {
        source: SMA_CROSS,
        symbol_key: CHART_KEY,
        chart: ch,
        syminfo_strs: ["BTCUSDT", "BTC", "USDT", "crypto"],
        inputs: &[],
        ticker_map: &tm,
        limits: pine_screen::SCREEN_LIMITS,
    };
    let t = pine_screen::compile_trial(&spec, &mut pine_sema::LibCache::empty()).expect("ok");
    let o = run_trial(&t, &b, &cfg(2, 30), &mut Workspace::new()).expect("per trial");
    match o.status {
        TrialStatus::Refused(Refusal::UnitBelowStep {
            unit_qty_1e6,
            step_1e6,
        }) => {
            assert_eq!(step_1e6, 1_000_000);
            assert!(unit_qty_1e6 < step_1e6 && unit_qty_1e6 > 0);
        }
        s => panic!("{s:?}"),
    }
}

#[test]
fn screen_broker_refusal_is_per_trial() {
    let mag = SMA_CROSS.replace("overlay=true)", "overlay=true, use_bar_magnifier=true)");
    let m = ok(&mag);
    let a = ok(SMA_CROSS);
    let b = bars();
    let cfg = cfg(2, 30);
    let mut out = vec![TrialOutcome::default(); 2];
    run_group(
        &[&m, &a],
        &b,
        &cfg,
        &mut Workspace::new(),
        &mut out,
        &mut Rec::default(),
    )
    .expect("runs");
    assert_eq!(
        out[0].status,
        TrialStatus::Refused(Refusal::Broker(pine_ir::DiagCode(5100)))
    );
    assert_eq!(out[1].status, TrialStatus::Completed);
    assert_eq!(
        out[1],
        run_trial(&a, &b, &cfg, &mut Workspace::new()).expect("runs")
    );
}

#[test]
fn screen_window_is_day_aligned() {
    let t = ok(SMA_CROSS);
    let b = bars();
    let mut c = cfg(2, 30);
    c.from_ms += 1;
    assert_eq!(
        run_trial(&t, &b, &c, &mut Workspace::new()),
        Err(ScreenErr::Window)
    );
    let mut c = cfg(2, 30);
    c.to_ms = c.from_ms;
    assert_eq!(
        run_trial(&t, &b, &c, &mut Workspace::new()),
        Err(ScreenErr::Window)
    );
    let mut c = cfg(2, 30);
    c.notional_1e6 = 0;
    assert_eq!(
        run_trial(&t, &b, &c, &mut Workspace::new()),
        Err(ScreenErr::Window)
    );
    let mut out: [TrialOutcome; 0] = [];
    assert_eq!(
        run_group(
            &[&t],
            &b,
            &cfg(2, 30),
            &mut Workspace::new(),
            &mut out,
            &mut Rec::default()
        ),
        Err(ScreenErr::GroupSize)
    );
    assert!(!ScreenErr::Window.is_defect());
}

#[test]
fn screen_meta_reports_what_the_screen_reads() {
    // Guard on the fields the loop depends on (a sema change here changes the runner's input).
    let t = ok(SMA_CROSS);
    let m: &ProgramMeta = t.meta();
    assert!(m.warmup_bars >= 30);
    assert!(!m.uses_intrabar);
    assert!(!m.uses_lookahead);
}

/// impl-07 §4.0 #1, typed from the spec text (T2 lands the file; this is its body verbatim).
const PVM7_SUPERTREND: &str = "// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//@version=6
//
// Supertrend trend-follow. OUR CODE, typed from the public rule: an ATR band is drawn either side of
// hl2, the band ratchets in the direction of the trend, and the trend flips when the close crosses
// it. Long when the close moves above the line, flat when it moves below. Nothing here was copied.
strategy(\"PVM7 Supertrend\", overlay = true, initial_capital = 100000,
     default_qty_type = strategy.fixed, default_qty_value = 1, pyramiding = 0)

atrLen = input.int(10, \"ATR length\", minval = 1, maxval = 200)
factor = input.float(3.0, \"Factor\", minval = 0.5, maxval = 10.0, step = 0.1)

[st, stDir] = ta.supertrend(factor, atrLen)
up = close > st

if up and not up[1]
    strategy.entry(\"L\", strategy.long)
if not up and up[1]
    strategy.close(\"L\")

plot(st, \"Supertrend\", color = up ? color.green : color.red)
";

/// T8's acceptance: `pvm7_supertrend` screens `binance_usdm:btcusdt` 1 h on a synthetic file and
/// produces a row.
#[test]
fn t8_pvm7_supertrend_screens_and_produces_a_row() {
    let t = ok(PVM7_SUPERTREND);
    assert_eq!(t.mode(), ExecMode::BarClose);
    let b = bars();
    // ATR warm-up is 141 bars here: a 7-day (168-row) warm-up, 23 scored days.
    let cfg = cfg(7, 30);
    let mut ws = Workspace::new();
    let o = run_trial(&t, &b, &cfg, &mut ws).expect("runs");
    assert_eq!(o.status, TrialStatus::Completed);
    let s = o.summary;
    assert_eq!((s.bars, s.obs_days, s.faults), (720, 23, 0));
    assert!(s.closed >= 2, "the sine flips the band: {s:?}");
    assert_eq!(s.gross_bps_1e4, s.net_bps_1e4 + s.fee_bps_1e4);
    assert_eq!(s.sizing_notional_1e6, cfg.notional_1e6);
    assert_eq!(ws.day_returns(0).len(), 23);
    let again = run_trial(&t, &b, &cfg, &mut Workspace::new()).expect("runs");
    assert_eq!(again, o);
    eprintln!(
        "pvm7_supertrend: closed {} trades {} net {} fee {} hold med {} p95 {} maxconc {}",
        s.closed,
        s.trades,
        s.net_bps_1e4,
        s.fee_bps_1e4,
        s.hold_bars_med,
        s.hold_bars_p95,
        s.max_concurrent_1e6
    );
}

/// §7.6 `screen_defects_fail_fast`. HONEST VARIANT (named in the report): instead of a
/// `pine_std::testing` program, the compiled SMA-cross with a `TRAP` inserted before its final
/// `HALT` — every path that reached `HALT` now reaches `TRAP` (jump targets before it are
/// unchanged; one at the old `HALT` index now lands on the `TRAP`) — a strategy program that
/// verifies and raises `FaultKind::Internal` on bar 0.
#[test]
fn screen_defects_fail_fast() {
    let good = ok(SMA_CROSS);
    let mut p = good.program().clone();
    let n = p.code.len();
    assert_eq!(p.code[n - 1].op, op::HALT);
    p.code.insert(
        n - 1,
        pine_ir::Insn {
            op: op::TRAP,
            flags: 0,
            dst: 0,
            a: 0,
            b: 0,
        },
    );
    let t = Loaded::from_program_for_tests(p, SMA_CROSS).expect("a TRAP before HALT verifies");
    let b = bars();
    match run_trial(&t, &b, &cfg(2, 30), &mut Workspace::new()) {
        Err(
            e @ ScreenErr::VmDefect {
                kind: pine_ir::FaultKind::Internal,
                bar_index: 0,
                ..
            },
        ) => {
            assert!(e.is_defect());
        }
        other => panic!("expected VmDefect(Internal) at bar 0, got {other:?}"),
    }
}

// ---- T9: sub-programs (§4.3.2, §4.3.6; I10, I13, I15, I32) ----

/// 48 hourly rows (two days) — twelve 4-h buckets.
fn closes48() -> Vec<i64> {
    sine_closes(48)
}

fn ok_probe(name: &str) -> Loaded {
    ok(&probe_src(name))
}

fn group(trials: &[&Loaded], b: &pine_screen::TrialBars, rec: &mut Rec) -> Vec<TrialOutcome> {
    let mut out = vec![TrialOutcome::default(); trials.len()];
    run_group(trials, b, &cfg(0, 2), &mut Workspace::new(), &mut out, rec).expect("runs");
    out
}

#[test]
fn screen_steps_sub_programs_first() {
    let t = ok_probe("htf_close.pine");
    assert_eq!(t.program().subprograms.len(), 1);
    let b = bars_with_4h(&closes48(), Some((0, 12)));
    let mut rec = Rec::default();
    let out = group(&[&t], &b, &mut rec);
    assert_eq!(out[0].status, TrialStatus::Completed, "{:?}", out[0].status);
    assert_eq!(rec.pubs.len(), 12, "every 4-h bar is published once");
    let rows = b.chart().rows();
    for (j, p) in rec.pubs.iter().enumerate() {
        assert_eq!(p.htf_index, j as u32);
        let r = p.chart_row as usize;
        assert_eq!(
            rows[r].close_ms, p.htf_close_ms,
            "published at the row its close reaches"
        );
        let first_eval = rec
            .evals
            .iter()
            .position(|e| e.index == p.chart_row)
            .expect("row eval");
        assert_eq!(
            p.evals_before, first_eval,
            "before that row's first on_eval, never earlier"
        );
    }
}

#[test]
fn screen_lookahead_steps_the_containing_bar() {
    let t = ok_probe("htf_lookahead.pine");
    assert!(t.program().subprograms[0].lookahead);
    // Eleven 4-h bars: the last one closes at 44 h — rows 44..47 have no containing bar.
    let b = bars_with_4h(&closes48(), Some((0, 11)));
    let mut rec = Rec::default();
    let out = group(&[&t], &b, &mut rec);
    assert_eq!(out[0].status, TrialStatus::Completed, "{:?}", out[0].status);
    assert_eq!(rec.pubs.len(), 11);
    let rows = b.chart().rows();
    for p in &rec.pubs {
        let r = p.chart_row as usize;
        assert_eq!(
            rows[r].open_ms, p.htf_open_ms,
            "published at the row it contains"
        );
        let first_eval = rec
            .evals
            .iter()
            .position(|e| e.index == p.chart_row)
            .expect("row eval");
        assert_eq!(p.evals_before, first_eval);
    }
    let last = rec.evals.iter().map(|e| e.index).max().expect("evals");
    assert_eq!(last, 43, "the lookahead tail clamp (§4.3.6)");
    let flagged: Vec<u32> = rec
        .evals
        .iter()
        .filter(|e| e.flags & barflag::LAST != 0)
        .map(|e| e.index)
        .collect();
    assert_eq!(flagged, vec![43], "LAST on the clamped final row only");
}

#[test]
fn screen_htf_missing_and_coverage_refuse() {
    let a = ok(SMA_CROSS);
    let h = ok_probe("htf_close.pine");
    let key = pine_ir::symbol_hash(CHART_KEY.as_bytes());
    let alone = |b: &pine_screen::TrialBars| {
        let mut out = vec![TrialOutcome::default(); 1];
        run_group(
            &[&a],
            b,
            &cfg(0, 2),
            &mut Workspace::new(),
            &mut out,
            &mut NoProbe,
        )
        .expect("runs");
        out[0]
    };
    // No 4-h file.
    let b = bars_with_4h(&closes48(), None);
    let out = group(&[&a, &h, &a], &b, &mut Rec::default());
    assert_eq!(
        out[1].status,
        TrialStatus::Refused(Refusal::HtfMissing {
            symbol_hash: key,
            tf_s: 14_400
        })
    );
    let one = alone(&b);
    assert_eq!(
        (out[0], out[2]),
        (one, one),
        "the others equal their K = 1 rows"
    );
    // A 4-h file starting one bucket late.
    let b = bars_with_4h(&closes48(), Some((1, 11)));
    let out = group(&[&a, &h, &a], &b, &mut Rec::default());
    assert_eq!(
        out[1].status,
        TrialStatus::Refused(Refusal::HtfCoverage {
            symbol_hash: key,
            tf_s: 14_400
        })
    );
    let one = alone(&b);
    assert_eq!((out[0], out[2]), (one, one));
}

#[test]
fn screen_htf_missing_through_trial_bars_open() {
    let dir = TmpDir::new("htf-absent");
    let key = pine_ir::symbol_hash(CHART_KEY.as_bytes());
    let rows = hourly_rows(&closes48());
    let p = pine_screen::bar_path(&dir.0, CHART_KEY, 3600).expect("path");
    std::fs::create_dir_all(p.parent().expect("dir")).expect("mkdir");
    pine_screen::bars::write_bar_file(
        &p,
        &rows,
        3600,
        key,
        0,
        pine_screen::bar_source::CANDLES,
        false,
    )
    .expect("write");
    let h = ok_probe("htf_close.pine");
    let b =
        pine_screen::TrialBars::open(&dir.0, CHART_KEY, 3600, h.htf_needs(), None).expect("open");
    let out = group(&[&h], &b, &mut Rec::default());
    assert!(matches!(
        out[0].status,
        TrialStatus::Refused(Refusal::HtfMissing { .. })
    ));
}

#[test]
fn screen_sub_programs_are_deduplicated_across_the_group() {
    let h = ok_probe("htf_close.pine");
    let h2 = ok_probe("htf_close.pine");
    let b = bars_with_4h(&closes48(), Some((0, 12)));
    let mut rec = Rec::default();
    let out = group(&[&h, &h2], &b, &mut rec);
    assert_eq!(
        rec.pubs.len(),
        12,
        "one instance, one slot for both readers (P4)"
    );
    assert!(rec.pubs.iter().all(|p| p.slot == 0));
    let single = group(&[&h], &b, &mut Rec::default());
    assert_eq!(
        (out[0], out[1]),
        (single[0], single[0]),
        "K > 1 equals K = 1"
    );
}

#[test]
fn screen_sub_program_fault_faults_its_readers() {
    let s = ok_probe("sub_budget.pine");
    let a = ok(SMA_CROSS);
    let b = bars_with_4h(&closes48(), Some((0, 12)));
    let out = group(&[&s, &s, &a], &b, &mut Rec::default());
    for (k, o) in out.iter().take(2).enumerate() {
        match o.status {
            TrialStatus::Faulted(r) => {
                assert!(r.sub, "k={k}");
                assert_eq!(r.origin, FaultOrigin::Vm(pine_ir::FaultKind::InsnBudget));
                assert_eq!(r.bar_index, 10, "the HTF bar's index");
                assert_eq!(&r.source_sha256, s.source_sha256());
            }
            st => panic!("k={k}: expected Faulted, got {st:?}"),
        }
        assert_eq!(o.summary.faults, 1);
    }
    let alone = group(&[&a], &b, &mut Rec::default());
    assert_eq!(out[2], alone[0], "the third runs to the end");
}

#[test]
fn screen_nested_sub_programs_flatten_children_first_and_dedup() {
    let t = ok_probe("htf_nested.pine");
    assert_eq!(t.program().subprograms.len(), 2);
    assert_eq!(t.program().subprograms[0].program.subprograms.len(), 1);
    let key = pine_ir::symbol_hash(CHART_KEY.as_bytes());
    let rows = hourly_rows(&closes48());
    let f4 = pine_screen::BarFile::from_rows(
        agg_rows(&rows, 4, 0),
        14_400,
        key,
        0,
        pine_screen::bar_source::CANDLES,
    )
    .expect("4-h");
    let fd = pine_screen::BarFile::from_rows(
        agg_rows(&rows, 24, 0),
        86_400,
        key,
        0,
        pine_screen::bar_source::CANDLES,
    )
    .expect("1-d");
    let chart =
        pine_screen::BarFile::from_rows(rows, 3600, key, 0, pine_screen::bar_source::CANDLES)
            .expect("1-h");
    let b = pine_screen::TrialBars::from_files(chart, vec![(key, 14_400, f4), (key, 86_400, fd)]);
    let mut rec = Rec::default();
    let out = group(&[&t], &b, &mut rec);
    assert_eq!(out[0].status, TrialStatus::Completed, "{:?}", out[0].status);
    // The nested 4-h request and the top-level one share slot 0 (children first); the daily is 1.
    let s0: Vec<_> = rec.pubs.iter().filter(|p| p.slot == 0).collect();
    let s1: Vec<_> = rec.pubs.iter().filter(|p| p.slot == 1).collect();
    assert_eq!((s0.len(), s1.len(), rec.pubs.len()), (12, 2, 14));
    assert!(s0
        .iter()
        .all(|p| p.htf_close_ms - p.htf_open_ms == 4 * H_MS));
    assert!(s1.iter().all(|p| p.htf_close_ms - p.htf_open_ms == DAY_MS));
    for d in &s1 {
        let i_parent = rec.pubs.iter().position(|p| p == *d).expect("parent");
        let i_child = rec
            .pubs
            .iter()
            .position(|p| p.slot == 0 && p.chart_row == d.chart_row)
            .expect("the child publishes on the same row");
        assert!(i_child < i_parent, "children before parents");
    }
}
