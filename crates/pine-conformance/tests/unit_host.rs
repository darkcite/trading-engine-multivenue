// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.5 (host half) — the §3.15.3 loop: HTF stepping by close, lookahead's containing bar,
//! `bars_seen`, the store, the broker view, the one flagged copy and the aligned arena.

use pine_conformance::chart::{compile_opts, spec, Syminfo};
use pine_conformance::host::{last_bar, step_chart_bar, AlignedBuf, HostCtx, SeriesStore, SubRun};
use pine_ir::{barflag, Bar, BarCtx, Dialect, ExecMode, FileId, PineProgram, SymTable, TvCfg};
use pine_vm::VmInstance;
/// `syminfo.tsv` is a probe output (§4.1.4) not yet captured; the host tests use a hand-made one.
fn syminfo() -> Syminfo {
    Syminfo {
        mintick_1e6: 100_000,
        pointvalue_1e6: 1_000_000,
        ticker: "BTCUSDT.P".to_string(),
        basecurrency: "BTC".to_string(),
        currency: "USDT".to_string(),
        type_: "crypto".to_string(),
        venue: 0,
        day_offset_s: 0,
    }
}

fn compile(src: &str) -> PineProgram {
    let si = syminfo();
    let limits = pine_ir::CompileLimits {
        max_bars_back: 5000,
        insn_budget: 2_000_000,
        collection_cap: 4096,
        str_persist_bytes: 4096,
        str_cell_bytes: 128,
        str_scratch_bytes: 8192,
        intent_cap: 32,
        max_frame_bytes: 64 << 20,
        active_tiers: 0b0011,
    };
    let opts = compile_opts(
        &si,
        Dialect::V6,
        ExecMode::BarClose,
        false,
        limits,
        core_crypto::sha256(src.as_bytes()),
    );
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parse");
    let mut libs = pine_sema::LibCache::empty();
    pine_sema::compile(&ast, syms, &opts, &mut libs).unwrap_or_else(|d| {
        panic!(
            "compile: {:?}",
            d.iter().map(|d| d.code.0).collect::<Vec<_>>()
        )
    })
}

/// `n` hourly chart rows from `FROM_MS`, re-indexed from 0, a moving close.
fn chart_rows(n: u32) -> Vec<Bar> {
    rows_on(spec::FROM_MS, 3_600_000, n)
}

fn rows_on(from: i64, tf_ms: i64, n: u32) -> Vec<Bar> {
    (0..n)
        .map(|i| {
            let px = 100_000_000 + i64::from(i % 17) * 1_000_000;
            Bar {
                open_1e6: px,
                high_1e6: px + 500_000,
                low_1e6: px - 500_000,
                close_1e6: px + 250_000,
                volume_1e6: 1_000_000,
                open_ms: from + i64::from(i) * tf_ms,
                close_ms: from + i64::from(i + 1) * tf_ms,
                index: i,
                flags: barflag::CONFIRMED,
            }
        })
        .collect()
}

/// HTF rows covering the chart rows `[from, from + n·1h)` (epoch-aligned since `FROM_MS` is a day).
fn htf_rows(tf_h: u32, n_chart: u32) -> Vec<Bar> {
    rows_on(
        spec::FROM_MS,
        i64::from(tf_h) * 3_600_000,
        n_chart / tf_h + 1,
    )
}

const TWO_HTF: &str = "//@version=6
indicator(\"h\")
a = request.security(syminfo.tickerid, \"240\", close)
b = request.security(syminfo.tickerid, \"720\", close)
plot(a, \"pa\")
plot(b, \"pb\")
";

const LOOKAHEAD: &str = "//@version=6
indicator(\"l\")
a = request.security(syminfo.tickerid, \"240\", close, lookahead = barmerge.lookahead_on)
plot(a, \"pa\")
";

/// Drives `prog` over 48 chart rows; after every chart bar calls `check(k, bar, subs, store)`.
fn drive(
    prog: &PineProgram,
    mut check: impl FnMut(u32, &Bar, &[SubRun<'_>], &SeriesStore, &VmInstance<'_>),
) {
    let rows = chart_rows(48);
    assert_eq!(
        spec::FROM_MS % (12 * 3_600_000),
        0,
        "FROM_MS on the 12h grid"
    );
    let n_sub = prog.subprograms.len();
    assert!(n_sub >= 1);
    let htf: Vec<Vec<Bar>> = prog
        .subprograms
        .iter()
        .map(|sp| htf_rows(sp.timeframe_s / 3600, 48))
        .collect();
    let verified: Vec<_> = prog
        .subprograms
        .iter()
        .map(|sp| pine_vm::verify(&sp.program, &pine_std::BUILTINS).expect("verify sub"))
        .collect();
    let mut arenas: Vec<AlignedBuf> = prog
        .subprograms
        .iter()
        .map(|sp| AlignedBuf::new(sp.program.layout.total_bytes))
        .collect();
    let mut subs: Vec<SubRun<'_>> = Vec::new();
    for ((i, (v, a)), sp) in verified
        .into_iter()
        .zip(arenas.iter_mut())
        .enumerate()
        .zip(prog.subprograms.iter())
    {
        subs.push(SubRun {
            vm: VmInstance::new(v, a.as_mut_slice()),
            slot: i,
            n_out: sp.n_out,
            lookahead: sp.lookahead,
            bars: &htf[i],
            next: 0,
        });
    }
    let specs: Vec<(u64, u16, u32)> = prog
        .subprograms
        .iter()
        .enumerate()
        .map(|(i, sp)| (sp.key, sp.n_out, htf[i].len() as u32))
        .collect();
    let mut store = SeriesStore::with_slots(&specs);
    let v = pine_vm::verify(prog, &pine_std::BUILTINS).expect("verify main");
    let mut main = AlignedBuf::new(prog.layout.total_bytes);
    let mut vm = VmInstance::new(v, main.as_mut_slice());
    let mut broker = None;
    let mut evals = 0u64;
    for (k, row) in rows.iter().enumerate() {
        let lb;
        let bar = if k + 1 == rows.len() {
            lb = last_bar(row);
            &lb
        } else {
            row
        };
        if step_chart_bar(bar, &mut subs, &mut store, &mut vm, &mut broker, &mut evals).is_err() {
            panic!("fault at chart bar {k}");
        }
        check(k as u32, bar, &subs, &store, &vm);
    }
    assert!(evals >= 48);
}

#[test]
fn host_steps_htf_bars_by_close() {
    let prog = compile(TWO_HTF);
    assert_eq!(prog.subprograms.len(), 2);
    drive(&prog, |_, bar, subs, store, _| {
        for (s, sr) in subs.iter().enumerate() {
            let due = sr
                .bars
                .iter()
                .filter(|h| h.close_ms <= bar.close_ms)
                .count();
            assert_eq!(sr.next, due, "sub {s} at chart bar {}", bar.index);
            assert_eq!(store.len_of(sr.slot) as usize, due);
        }
    });
}

#[test]
fn host_steps_the_containing_htf_bar_for_lookahead() {
    let prog = compile(LOOKAHEAD);
    assert_eq!(prog.subprograms.len(), 1);
    assert!(prog.subprograms[0].lookahead);
    drive(&prog, |_, bar, subs, store, _| {
        let sr = &subs[0];
        // Stepped: every HTF bar opening at or before the chart bar's open — the containing one
        // included — once each; `next` never revisits it.
        let due = sr.bars.iter().filter(|h| h.open_ms <= bar.open_ms).count();
        assert_eq!(sr.next, due, "chart bar {}", bar.index);
        assert_eq!(store.len_of(sr.slot) as usize, due);
        let v = store.view(prog.subprograms[0].key, 0).expect("published");
        assert!(v.last_confirmed);
        assert_eq!(*v.open_ms.last().unwrap(), sr.bars[due - 1].open_ms);
    });
}

#[test]
fn host_bars_seen_matches_bar_index() {
    let prog = compile(TWO_HTF);
    drive(&prog, |_, bar, subs, _, vm| {
        assert_eq!(vm.bars_seen(), u64::from(bar.index) + 1);
        for sr in subs {
            assert_eq!(sr.vm.bars_seen(), sr.next as u64);
            if sr.next > 0 {
                assert_eq!(sr.vm.bars_seen(), u64::from(sr.bars[sr.next - 1].index) + 1);
            }
        }
    });
}

#[test]
fn series_store_views_are_contiguous() {
    let mut st = SeriesStore::with_slots(&[(11, 2, 4), (22, 1, 3)]);
    assert!(st.view(11, 0).is_none(), "nothing published yet");
    for e in 0..3i64 {
        st.publish(0, &[e as f64, 10.0 + e as f64], e * 10, e * 10 + 10, true);
    }
    // A developing element, replaced, then confirmed in place.
    st.publish(0, &[7.0, 70.0], 30, 40, false);
    assert!(!st.view(11, 0).unwrap().last_confirmed);
    st.publish(0, &[3.0, 13.0], 30, 40, false);
    st.publish(0, &[3.5, 13.5], 30, 40, true);
    st.publish(1, &[42.0], 0, 100, true);
    let v0 = st.view(11, 0).unwrap();
    let v1 = st.view(11, 1).unwrap();
    assert_eq!(v0.values, &[0.0, 1.0, 2.0, 3.5]);
    assert_eq!(v1.values, &[10.0, 11.0, 12.0, 13.5]);
    assert_eq!(v0.open_ms, &[0, 10, 20, 30]);
    assert_eq!(v0.close_ms, &[10, 20, 30, 40]);
    assert!(v0.last_confirmed);
    assert_eq!(st.view(22, 0).unwrap().values, &[42.0]);
    assert!(st.view(22, 1).is_none());
    assert!(st.view(33, 0).is_none());
}

#[test]
fn host_ctx_broker_is_the_tv_broker_view() {
    let prog = compile(
        "//@version=6
strategy(\"s\")
plot(close, \"pc\")
",
    );
    let si = syminfo();
    let broker = pine_broker::TvBroker::new(
        &prog.meta,
        TvCfg {
            sizing: pine_ir::SizingMode::AsDeclared,
            day_offset_s: si.day_offset_s,
            _pad: 0,
        },
    )
    .unwrap_or_else(|c| panic!("TvBroker::new {}", c.0));
    let store = SeriesStore::with_slots(&[]);
    let ctx = HostCtx {
        store: &store,
        broker: Some(&broker),
        now_ms: 5,
    };
    assert!(std::ptr::eq(ctx.broker().unwrap(), broker.view()));
    assert_eq!(ctx.open_trade(0).is_some(), broker.open_trade(0).is_some());
    assert_eq!(
        ctx.closed_trade(0).is_some(),
        broker.closed_trade(0).is_some()
    );
    assert_eq!(ctx.now_ms(), 5);
    let none = HostCtx {
        store: &store,
        broker: None,
        now_ms: 0,
    };
    assert!(none.broker().is_none() && none.open_trade(0).is_none());
}

#[test]
fn last_bar_is_the_only_flagged_copy() {
    let rows = chart_rows(5);
    let lb = last_bar(&rows[4]);
    assert_eq!(
        lb.flags,
        barflag::CONFIRMED | barflag::LAST | barflag::LAST_CONFIRMED_HISTORY
    );
    assert_eq!(
        Bar {
            flags: rows[4].flags,
            ..lb
        },
        rows[4]
    );
    // The rows are untouched: no other bar carries the flags.
    assert!(rows
        .iter()
        .all(|b| b.flags & (barflag::LAST | barflag::LAST_CONFIRMED_HISTORY) == 0));
    // In a run, only the final chart bar is flagged.
    let prog = compile(TWO_HTF);
    drive(&prog, |k, bar, _, _, _| {
        let flagged = bar.flags & barflag::LAST != 0;
        assert_eq!(flagged, k == 47);
    });
}

#[test]
fn aligned_buf_is_64_aligned() {
    for len in [0u32, 1, 63, 64, 65, 4096, 12_345] {
        let mut b = AlignedBuf::new(len);
        let s = b.as_mut_slice();
        assert_eq!(s.len(), len as usize);
        assert_eq!(s.as_ptr() as usize % 64, 0);
        assert!(s.iter().all(|&x| x == 0));
    }
}

#[test]
fn host_commits_after_every_evaluation() {
    // calc_on_order_fills: the fill bar is evaluated twice (the fill tick, then the close). The
    // broker refuses `open_bar` (PathCursor) unless every Evaluate was answered by `submit`, so a
    // clean run with a two-evaluation bar proves the host committed after each.
    let src = "//@version=6
strategy(\"c\", calc_on_order_fills = true)
if bar_index == 1
    strategy.entry(\"L\", strategy.long)
plot(strategy.position_size, \"ps\")
";
    let si = syminfo();
    let opts = compile_opts(
        &si,
        Dialect::V6,
        ExecMode::Realtime,
        false,
        pine_conformance::run::DEFAULT_LIMITS,
        core_crypto::sha256(src.as_bytes()),
    );
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parse");
    let prog = pine_sema::compile(&ast, syms, &opts, &mut pine_sema::LibCache::empty())
        .unwrap_or_else(|d| {
            panic!(
                "compile {:?}",
                d.iter().map(|d| d.code.0).collect::<Vec<_>>()
            )
        });
    let broker = pine_broker::TvBroker::new(
        &prog.meta,
        TvCfg {
            sizing: pine_ir::SizingMode::AsDeclared,
            day_offset_s: 0,
            _pad: 0,
        },
    )
    .unwrap_or_else(|c| panic!("TvBroker::new {}", c.0));
    let mut broker = Some(broker);
    let v = pine_vm::verify(&prog, &pine_std::BUILTINS).expect("verify");
    let mut arena = AlignedBuf::new(prog.layout.total_bytes);
    let mut vm = VmInstance::new(v, arena.as_mut_slice());
    let mut store = SeriesStore::with_slots(&[]);
    let rows = chart_rows(8);
    let mut per_bar = Vec::new();
    let mut evals = 0u64;
    for row in &rows {
        let before = evals;
        if step_chart_bar(row, &mut [], &mut store, &mut vm, &mut broker, &mut evals).is_err() {
            panic!("fault at bar {}", row.index);
        }
        per_bar.push(evals - before);
    }
    assert_eq!(per_bar[0], 1);
    assert!(per_bar.iter().all(|&n| n >= 1));
    assert!(
        per_bar.contains(&2),
        "a fill bar evaluates twice: {per_bar:?}"
    );
    assert!(broker.unwrap().view().position_size_1e6 > 0);
}

/// A fixture of ours over a script written to a temp file (the runner resolves an absolute
/// `script` as is).
fn temp_fixture(
    tag: &str,
    src: &str,
    ds: pine_conformance::fixture::Dialects,
) -> pine_conformance::fixture::Fixture {
    use pine_conformance::fixture::{Align, Driver, Expect, Fixture, Oracle, Window};
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let any = std::fs::read_to_string(root.join("tests/conformance/fixtures/A/A051.fix")).unwrap();
    let mut f = Fixture::parse(&any, "A051.fix").expect("A051 parses");
    let dir = std::env::temp_dir().join(format!("pine-conf-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.pine");
    std::fs::write(&path, src).unwrap();
    f.script = path.display().to_string();
    f.script_sha256 = core_crypto::sha256(src.as_bytes());
    f.dialects = ds;
    f.driver = Driver::Syn;
    f.align = Align::Step;
    f.bars = Some(8);
    f.window = Window::Head(8);
    f.oracle = Oracle::Authored;
    f.tier = 1;
    f.live_refuse = None;
    f.plots = vec![pine_conformance::fixture::PlotSpec {
        title: "p".into(),
        tol: pine_conformance::fixture::Tol::Exact,
    }];
    f.expect = Expect::Pending;
    f
}

fn runner(limits: pine_ir::CompileLimits) -> pine_conformance::run::Runner {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut o = pine_conformance::run::RunnerOptions::new(root);
    o.limits = limits;
    pine_conformance::run::Runner::new(o).expect("runner")
}

#[test]
fn run_dual_dialect_asserts_invariance() {
    use pine_conformance::fixture::Dialects;
    use pine_conformance::report::{Verdict, NO_ROW};
    use pine_conformance::run::dialect_divergence;
    let f = temp_fixture(
        "dual",
        "//@version=6\nindicator(\"t\")\nplot(bar_index * 2, \"p\")\n",
        Dialects::BOTH,
    );
    let mut r = runner(pine_conformance::run::DEFAULT_LIMITS);
    let v6 = r.run(&f, Dialect::V6);
    let v5 = r.run(&f, Dialect::V5);
    assert_eq!(dialect_divergence(&v6, &v5), None);
    assert!(matches!(r.check(&f), Verdict::Pending));
    let a = v6.as_ref().expect("v6 runs");
    assert_eq!(a.keys, (0..8).collect::<Vec<i64>>());
    assert_eq!(a.plots[5], 10.0);
    // A planted v5/v6 difference on row 5.
    let mut planted = v5.expect("v5 runs");
    planted.plots[5] = 11.0;
    assert_eq!(dialect_divergence(&v6, &Ok(planted)), Some((5, 0)));
    // One dialect refused, the other ran.
    let refused = Err(pine_conformance::run::RunError::Skip(
        pine_conformance::run::SkipReason::TvOnly,
    ));
    assert_eq!(
        dialect_divergence(&v6, &refused),
        Some((NO_ROW, usize::MAX))
    );
}

#[test]
fn run_reports_fault_with_pc_and_bar() {
    use pine_conformance::fixture::{Dialects, Expect};
    use pine_conformance::report::Verdict;
    use pine_conformance::run::RunError;
    let mut f = temp_fixture(
        "fault",
        "//@version=6
indicator(\"t\")
s = 0.0
if bar_index == 3
    for i = 0 to 1000000
        s += i
plot(s, \"p\")
",
        Dialects::V6,
    );
    let mut r = runner(pine_ir::CompileLimits {
        insn_budget: 5_000,
        ..pine_conformance::run::DEFAULT_LIMITS
    });
    match r.run(&f, Dialect::V6) {
        Err(RunError::Fault {
            kind,
            pc,
            bar_index,
            sub,
        }) => {
            assert_eq!(kind, pine_ir::FaultKind::InsnBudget);
            assert_ne!(pc, u32::MAX, "pc from VmInstance::last_fault");
            assert_eq!(bar_index, 3);
            assert!(!sub);
        }
        other => panic!("expected an InsnBudget fault, got {other:?}"),
    }
    assert_eq!(r.fault_row(), Some(3));
    f.expect = Expect::Fault {
        kind: pine_ir::FaultKind::InsnBudget,
        row: 3,
    };
    assert!(matches!(r.check(&f), Verdict::Pass));
    f.expect = Expect::Fault {
        kind: pine_ir::FaultKind::InsnBudget,
        row: 4,
    };
    assert!(matches!(r.check(&f), Verdict::Fail(_)));
}
