// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §7.8 — the fuzzer's own tests (T20–T22): purity and totality of `generate`, the derived
//! alphabet, the real engine under modes (a) and (b), and the shrinker. Any divergence the fuzzer
//! finds in our engine is a FINDING: it is pinned in a findings table below (and in
//! `tests/conformance/fuzz/regressions.tsv`), never hidden by weakening a test.

use std::path::PathBuf;

use pine_conformance::fuzz::ast::{self, Expr, HistRef, Program, Stmt};
use pine_conformance::fuzz::diff::{Harness, Interleaved, ModeVsMode};
use pine_conformance::fuzz::grammar::{alphabet, FUZZ_DENY, FUZZ_DENY_NS};
use pine_conformance::fuzz::{
    generate, shrink, GrammarConfig, Predicate, Profile, ShrinkBudget, Signature, GRAMMAR_VERSION,
};
use pine_conformance::run::{RunError, DEFAULT_LIMITS};
use pine_ir::{DiagCode, Dialect};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn cfg(d: Dialect) -> GrammarConfig {
    GrammarConfig::for_profile(Profile::Indicator, d, DEFAULT_LIMITS.active_tiers)
}

#[test]
fn generate_is_pure_over_10k_seeds() {
    let c = cfg(Dialect::V6);
    let fwd: Vec<[u8; 32]> = (0..10_000u64)
        .map(|s| core_crypto::sha256(generate(s, &c).source.as_bytes()))
        .collect();
    let mut s = 10_000u64;
    while s > 0 {
        s -= 1;
        let g = generate(s, &c);
        assert_eq!(
            core_crypto::sha256(g.source.as_bytes()),
            fwd[s as usize],
            "seed {s}"
        );
        assert_eq!(
            g.source,
            ast::render(&g.ast),
            "seed {s}: source is the tree's rendering"
        );
        assert_eq!((g.seed, g.grammar_version), (s, GRAMMAR_VERSION));
    }
    let mut distinct = fwd.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() > 9_900,
        "{} distinct programs",
        distinct.len()
    );
}

#[test]
fn generate_never_panics_over_10k_seeds() {
    for d in [Dialect::V5, Dialect::V6] {
        let c = cfg(d);
        let line = match d {
            Dialect::V5 => "//@version=5\n",
            Dialect::V6 => "//@version=6\n",
        };
        let mut ill = 0u32;
        for s in 0..10_000u64 {
            let g = generate(s, &c);
            assert!(g.source.starts_with(line), "seed {s}");
            assert_eq!(
                ast::plot_ixs(&g.ast).len(),
                usize::from(c.plots),
                "seed {s}"
            );
            ill += u32::from(g.expect_refusal);
        }
        // ill_typed_permille 50: ~5 % of programs, at most one site each.
        assert!(
            (100..=900).contains(&ill),
            "v{}: {ill} ill-typed of 10 000",
            d as u8
        );
    }
}

fn calls(e: &Expr, out: &mut Vec<u16>) {
    match e {
        Expr::Call { ov, args, .. } => {
            out.push(*ov);
            for a in args {
                calls(a, out);
            }
        }
        Expr::Bin(_, a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            calls(a, out);
            calls(b, out);
        }
        Expr::Not(a) | Expr::IsNa(a) | Expr::Nz(a) => calls(a, out),
        Expr::Tern(c, a, b) => {
            calls(c, out);
            calls(a, out);
            calls(b, out);
        }
        _ => {}
    }
}

fn stmt_calls(list: &[Stmt], out: &mut Vec<u16>) {
    for s in list {
        match s {
            Stmt::Decl { e, .. } | Stmt::Assign { e, .. } | Stmt::Plot { e, .. } => calls(e, out),
            Stmt::If { cond, then, els } => {
                calls(cond, out);
                stmt_calls(then, out);
                stmt_calls(els, out);
            }
            Stmt::For { body, .. } => stmt_calls(body, out),
        }
    }
}

#[test]
fn alphabet_is_a_subset_of_active_tiers() {
    for d in [Dialect::V5, Dialect::V6] {
        for mask in [0b0001u8, 0b0011, 0b0111, 0b1111] {
            let a = alphabet(d, mask);
            assert!(!a.is_empty());
            for b in &a {
                let ov = &pine_spec::OVERLOADS[b.ov as usize];
                let e = &pine_spec::ENTRIES[ov.entry as usize];
                assert_ne!(
                    e.tier.bit() & mask,
                    0,
                    "{}: tier {:?} outside {mask:#06b}",
                    b.name,
                    e.tier
                );
                assert!(ov.versions.has(d), "{}: not in v{}", b.name, d as u8);
                assert_eq!(e.full_str(), b.name);
                assert!(!FUZZ_DENY.contains(&b.name), "{} is denied", b.name);
                assert!(
                    !FUZZ_DENY_NS.contains(&e.ns.as_str()),
                    "{} is denied",
                    b.name
                );
            }
        }
        // Monotone in the mask: a landed tier only grows the alphabet.
        let t1: Vec<u16> = alphabet(d, 0b0001).iter().map(|b| b.ov).collect();
        let t12: Vec<u16> = alphabet(d, 0b0011).iter().map(|b| b.ov).collect();
        assert!(t1.iter().all(|o| t12.contains(o)));
        // Every call a program makes is a generable alphabet row.
        let c = cfg(d);
        let gen_ok: Vec<u16> = alphabet(d, c.active_tiers)
            .iter()
            .filter(|b| b.ret.is_some())
            .map(|b| b.ov)
            .collect();
        assert!(gen_ok.len() > 20, "{} generable builtins", gen_ok.len());
        for s in 0..2_000u64 {
            let mut cs = Vec::new();
            stmt_calls(&generate(s, &c).ast.stmts, &mut cs);
            for ov in cs {
                assert!(
                    gen_ok.contains(&ov),
                    "seed {s}: overload {ov} outside the alphabet"
                );
            }
        }
    }
}

/// §4.8.3: 2 000 seeds compile with zero `DiagCode(3024)` (DeferredBuiltin). The other outcomes are
/// counted and printed; an ill-typed program must produce a `Diag` (I16's "never a panic").
#[test]
fn no_generated_program_is_refused_for_deferred_builtin() {
    let mut h = Harness::new(root(), "deferred").expect("harness");
    let c = cfg(Dialect::V6);
    let (mut ok, mut ill_refused) = (0u32, 0u32);
    let mut refused: Vec<(u64, Option<DiagCode>)> = Vec::new();
    let mut panicked: Vec<u64> = Vec::new();
    for s in 0..2_000u64 {
        let g = generate(s, &c);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.compile(&g)));
        let Ok(r) = r else {
            println!("COMPILE PANIC seed {s}\n{}", ast::render_body(&g.ast));
            panicked.push(s);
            h = Harness::new(root(), "deferred").expect("harness");
            continue;
        };
        match r {
            Ok(_) => {
                assert!(!g.expect_refusal, "seed {s}: an ill-typed program compiled");
                ok += 1;
            }
            Err(e) => {
                let code = e.code();
                let msg = match &e {
                    RunError::Compile(ds) | RunError::Parse(ds) => ds
                        .iter()
                        .map(|d| d.msg.as_str())
                        .collect::<Vec<_>>()
                        .join(" | "),
                    other => format!("{other:?}"),
                };
                assert_ne!(code, Some(DiagCode(3024)), "seed {s}: {msg}\n{}", g.source);
                if g.expect_refusal {
                    assert!(
                        matches!(e, RunError::Compile(_) | RunError::Parse(_)),
                        "seed {s}"
                    );
                    ill_refused += 1;
                } else {
                    if refused.len() < 5 {
                        println!(
                            "WELL-TYPED REFUSED seed {s}: {msg}\n{}",
                            ast::render_body(&g.ast)
                        );
                    }
                    refused.push((s, code));
                }
            }
        }
    }
    let mut hist: Vec<(Option<u16>, u32)> = Vec::new();
    for (_, c) in &refused {
        let k = c.map(|c| c.0);
        match hist.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => e.1 += 1,
            None => hist.push((k, 1)),
        }
    }
    println!(
        "2000 seeds: compiled {ok}, ill-typed refused {ill_refused}, well-typed refused {} {hist:?}",
        refused.len()
    );
    for (s, c) in refused.iter().take(5) {
        println!("WELL-TYPED REFUSED seed {s}: {c:?}");
    }
    println!("compile panics: {panicked:?}");
    assert!(ok > 1_000, "{ok} of 2000 compiled");
    // §4.8.2: a well-typed generated program never fails to compile.
    assert!(
        refused.is_empty(),
        "{} well-typed programs refused",
        refused.len()
    );
    assert_eq!(
        panicked, COMPILE_PANIC_FINDINGS,
        "compile-panic findings moved"
    );
}

/// §4.8.2 / §7.8: every deliberately ill-typed program (`ill_typed_permille`, one bad site) is
/// refused at parse or compile with at least one `Diag` — never compiled, never a panic — in both
/// dialects over 2 000 seeds each.
#[test]
fn ill_typed_programs_produce_a_diag_not_a_panic() {
    for d in [Dialect::V5, Dialect::V6] {
        let c = cfg(d);
        let mut h = Harness::new(root(), "ill").expect("harness");
        let mut n = 0u32;
        for s in 0..2_000u64 {
            let g = generate(s, &c);
            if !g.expect_refusal {
                continue;
            }
            n += 1;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.compile(&g)));
            let Ok(r) = r else {
                panic!("v{} seed {s}: compile panicked\n{}", d as u8, g.source);
            };
            match r {
                Err(RunError::Compile(ds) | RunError::Parse(ds)) => {
                    assert!(
                        !ds.is_empty(),
                        "v{} seed {s}: refused with no Diag",
                        d as u8
                    );
                }
                other => panic!(
                    "v{} seed {s}: an ill-typed program gave {:?}\n{}",
                    d as u8,
                    other.map(|_| ()),
                    g.source
                ),
            }
        }
        assert!(n >= 20, "v{}: {n} ill-typed programs of 2 000", d as u8);
    }
}

fn loop_trips(list: &[Stmt], prod: u64, max: u16, s: u64, worst: &mut u64) {
    for st in list {
        match st {
            Stmt::For { lo, hi, body, .. } => {
                let trips = (i64::from(*hi) - i64::from(*lo)).unsigned_abs() + 1;
                assert!(trips <= u64::from(max), "seed {s}: {trips} trips > {max}");
                let p = prod.saturating_mul(trips);
                *worst = (*worst).max(p);
                loop_trips(body, p, max, s, worst);
            }
            Stmt::If { then, els, .. } => {
                loop_trips(then, prod, max, s, worst);
                loop_trips(els, prod, max, s, worst);
            }
            _ => {}
        }
    }
}

/// §4.8.2 / §7.8: the grammar's only loop is `for i = lo to hi` with literal bounds whose trip
/// count is within `max_loop_trips` (no `while`, no counter is a writable slot), so a bar's
/// iterations are bounded by the product along the nesting — checked over 10 000 seeds in both
/// dialects; and 300 programs run their 64 bars to the end in the VM with no `InsnBudget` fault
/// (termination observed, not only argued).
#[test]
fn generated_loops_always_terminate() {
    for d in [Dialect::V5, Dialect::V6] {
        let c = cfg(d);
        let (mut worst, mut loops) = (0u64, 0u32);
        for s in 0..10_000u64 {
            let g = generate(s, &c);
            let mut w = 0u64;
            loop_trips(&g.ast.stmts, 1, c.max_loop_trips, s, &mut w);
            loops += u32::from(w > 0);
            worst = worst.max(w);
        }
        println!(
            "v{}: {loops} programs loop, worst nest product {worst}",
            d as u8
        );
        assert!(loops > 1_000, "v{}: {loops} looping programs", d as u8);
    }
    let c = cfg(Dialect::V6);
    let mut h = Harness::new(root(), "loops").expect("harness");
    let mut ran = 0u32;
    for s in 0..300u64 {
        let g = generate(s, &c);
        if g.expect_refusal {
            continue;
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            h.run(&g.ast, pine_ir::ExecMode::BarClose, 64)
        }));
        let Ok(r) = r else {
            panic!("seed {s}: the run panicked\n{}", g.source);
        };
        if let Err(RunError::Fault { kind, .. }) = &r {
            assert_ne!(
                *kind,
                pine_ir::FaultKind::InsnBudget,
                "seed {s}\n{}",
                g.source
            );
        }
        ran += 1;
    }
    assert!(ran > 250, "{ran} programs ran");
}

/// Seeds (grammar 1, v6, `Indicator`) whose compile panics inside the engine — FINDINGS for the
/// owner (the compiler must return a `Diag`, never panic); each is a `regressions.tsv` row.
/// `pin:06-fuzz-panic-table` — detected with `catch_unwind` in this test only; nothing passes.
const COMPILE_PANIC_FINDINGS: &[u64] = &[];

/// Findings of modes (a)/(b): `(mode, grammar, seed)` — each also a `regressions.tsv` row.
const MODE_FINDINGS: &[(&str, u16, u64)] = &[];

/// Engine panics (debug assertions) a sweep has met — FINDINGS by message class, each with its
/// owner. `pin:06-fuzz-panic-class` — a seed that panics is recorded (the predicate is rebuilt and
/// the sweep goes on) and its message must be a row here; an unknown panic is red. A row whose
/// class `mode_vs_mode_agrees_on_1000_seeds` no longer meets is red too (a fixed finding leaves).
///
/// Fixed 2026-10-17 (r21), rows retired: "set_f64_arg on an i64 entry" (pine-std `bi_extreme`:
/// `ta.highestbars`/`ta.lowestbars` now write their `int` result via `set_i64_arg`; seed 4,
/// B088/B091) and "!a.is_nan() && !b.is_nan()" (pine-std `change_pairs`: an `inf − inf` change
/// pair is skipped, `pin:04-change-pair-nan`; seed 692 `ta.cmo(math.exp(syni), 12)`).
const KNOWN_PANICS: &[(&str, &str)] = &[];

/// `(programs run, divergent seeds, panicking seeds with their message)`.
fn sweep<P: Predicate, F: Fn() -> P>(
    mk: F,
    mode: &str,
    seeds: u64,
) -> (u32, Vec<u64>, Vec<(u64, String)>) {
    let c = cfg(Dialect::V6);
    let mut p = mk();
    let mut ran = 0u32;
    let (mut div, mut panics) = (Vec::new(), Vec::new());
    for s in 0..seeds {
        let g = generate(s, &c);
        if g.expect_refusal {
            continue;
        }
        ran += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.diverges(&g.ast))) {
            Ok(Some(sig)) => {
                println!(
                    "{mode} DIVERGES seed {s}: {sig:?}\n{}",
                    ast::render_body(&g.ast)
                );
                div.push(s);
            }
            Ok(None) => {}
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|m| m.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                panics.push((s, msg));
                p = mk();
            }
        }
    }
    (ran, div, panics)
}

fn check_panics(mode: &str, panics: &[(u64, String)]) {
    println!(
        "{mode}: {} panicking seeds, first {:?}",
        panics.len(),
        panics.iter().take(3).collect::<Vec<_>>()
    );
    for (s, m) in panics {
        assert!(
            KNOWN_PANICS.iter().any(|(k, _)| m.contains(k)),
            "{mode} seed {s}: an unknown engine panic: {m}"
        );
    }
}

fn known(mode: &str) -> Vec<u64> {
    MODE_FINDINGS
        .iter()
        .filter(|(m, g, _)| *m == mode && *g == GRAMMAR_VERSION)
        .map(|(_, _, s)| *s)
        .collect()
}

/// §4.8.4 (a): bar-close vs realtime with one tick per bar, bit-identical plots.
#[test]
fn mode_vs_mode_agrees_on_1000_seeds() {
    let mk = || ModeVsMode {
        h: Harness::new(root(), "mvm").expect("harness"),
        bars: 128,
    };
    let (ran, div, panics) = sweep(mk, "mode-vs-mode", 1_000);
    println!("mode-vs-mode: {ran} programs, {} divergent", div.len());
    assert!(ran > 900);
    check_panics("mode-vs-mode", &panics);
    // A class this sweep no longer meets is a fixed finding: its row leaves KNOWN_PANICS.
    if cfg!(debug_assertions) {
        for (k, _) in KNOWN_PANICS {
            assert!(
                panics.iter().any(|(_, m)| m.contains(k)),
                "KNOWN_PANICS `{k}` no longer met: a fixed finding leaves the table"
            );
        }
    }
    assert_eq!(div, known("mode-vs-mode"), "mode-vs-mode findings moved");
}

/// §4.8.4 (b): two instances stepped alternately, then `reset()` and re-run.
#[test]
fn interleaved_instances_agree_on_1000_seeds() {
    let mk = || Interleaved {
        h: Harness::new(root(), "ilv").expect("harness"),
        bars: 128,
    };
    let (ran, div, panics) = sweep(mk, "interleaved", 1_000);
    println!("interleaved: {ran} programs, {} divergent", div.len());
    assert!(ran > 900);
    check_panics("interleaved", &panics);
    assert_eq!(div, known("interleaved"), "interleaved findings moved");
}

/// §4.8.5: every `regressions.tsv` row of this grammar still reproduces under mode (a) — a row
/// with a fixture id still diverges; an id `-` row (an engine-panic finding,
/// `pin:06-regressions-panic-rows`) still panics with a [`KNOWN_PANICS`] message while its class
/// is open, and once the class is fixed (it left the table) the row stays and must run clean —
/// no panic, no divergence. A row that stops reproducing is as much a finding as one that starts.
#[test]
fn regressions_still_reproduce() {
    let text =
        std::fs::read_to_string(root().join(pine_conformance::fuzz::REGRESSIONS_TSV)).unwrap();
    let c = cfg(Dialect::V6);
    let mut rows = 0;
    for l in text.lines().filter(|l| !l.starts_with('#')).skip(1) {
        let cols: Vec<&str> = l.split('\t').collect();
        assert_eq!(cols.len(), 4, "{l}");
        let (v, seed): (u16, u64) = (cols[0].parse().unwrap(), cols[1].parse().unwrap());
        assert!(
            matches!(cols[3], "SemanticBug" | "Unimplemented" | "Declined"),
            "{l}"
        );
        if v != GRAMMAR_VERSION {
            continue;
        }
        rows += 1;
        let g = generate(seed, &c);
        let mut p = ModeVsMode {
            h: Harness::new(root(), "regr").expect("harness"),
            bars: 128,
        };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.diverges(&g.ast)));
        match (cols[2], r) {
            ("-", Err(payload)) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|m| m.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                assert!(
                    KNOWN_PANICS.iter().any(|(k, _)| msg.contains(k)),
                    "{l}: {msg}"
                );
            }
            // The panic findings are pine-vm `debug_assert!` trips; a release build compiles them
            // out, so there the row only has to run (pin:06-regressions-panic-debug-only).
            ("-", Ok(_)) if !cfg!(debug_assertions) => {}
            // A fixed panic finding runs clean: its class left KNOWN_PANICS (which
            // `mode_vs_mode_agrees_on_1000_seeds` keeps honest — a listed class it no longer
            // meets is red), and the row stays as a regression that must neither panic nor diverge.
            ("-", Ok(None)) => {}
            ("-", Ok(Some(sig))) => panic!("{l}: the fixed panic finding now diverges: {sig:?}"),
            (_, Ok(Some(_))) => {}
            (_, other) => panic!("{l}: no longer diverges ({:?})", other.ok()),
        }
    }
    assert!(rows >= 2, "{rows} rows replayed");
}

// ---- T22: the shrinker against a planted divergence.

fn reads_hist(e: &Expr) -> bool {
    match e {
        Expr::Hist(HistRef::Syn(_), _) => true,
        Expr::Bin(_, a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            reads_hist(a) || reads_hist(b)
        }
        Expr::Not(a) | Expr::IsNa(a) | Expr::Nz(a) => reads_hist(a),
        Expr::Tern(c, a, b) => reads_hist(c) || reads_hist(a) || reads_hist(b),
        Expr::Call { args, .. } => args.iter().any(reads_hist),
        _ => false,
    }
}

/// A planted bug: "a plot whose expression reads a prelude series' history diverges". Pure and
/// structural, so the shrinker is tested without depending on a real engine divergence. It also
/// requires the program to still render and compile-shape (≥ 1 plot).
struct Planted {
    calls: u32,
}

impl Predicate for Planted {
    fn diverges(&mut self, p: &Program) -> Option<Signature> {
        self.calls += 1;
        p.stmts.iter().find_map(|s| match s {
            Stmt::Plot { e, ix } if reads_hist(e) => Some(Signature {
                kind: pine_conformance::sig::Kind::ConstantOffset,
                plot_ix: *ix,
                na_mismatch: false,
                sign: 1,
                ulp_decade: 3,
            }),
            _ => None,
        })
    }
}

/// A large planted program: the first seed whose body is > 30 lines and that diverges.
fn planted_seed() -> (pine_conformance::fuzz::Generated, Signature) {
    let c = cfg(Dialect::V6);
    let mut p = Planted { calls: 0 };
    for s in 0..10_000u64 {
        let g = generate(s, &c);
        if g.expect_refusal || ast::body_lines(&g.ast) <= 30 {
            continue;
        }
        if let Some(sig) = p.diverges(&g.ast) {
            return (g, sig);
        }
    }
    panic!("no planted seed in 10 000");
}

#[test]
fn shrink_is_monotone_and_terminates() {
    let (g, sig) = planted_seed();
    for budget in [5u32, 2_000] {
        let mut p = Planted { calls: 0 };
        let out = shrink(&g, &sig, &mut p, ShrinkBudget { max_steps: budget });
        assert!(
            out.steps <= budget && p.calls == out.steps,
            "steps {} calls {}",
            out.steps,
            p.calls
        );
        let mut prev = pine_conformance::fuzz::shrink::measure(&g.ast);
        for m in &out.trace {
            assert!(
                *m < prev,
                "trace not strictly decreasing: {m:?} after {prev:?}"
            );
            prev = *m;
        }
        assert_eq!(prev, pine_conformance::fuzz::shrink::measure(&out.program));
        assert!(out.lines_after <= out.lines_before);
        if budget == 5 {
            assert_eq!(out.steps, 5, "a tiny budget is exhausted, and reported");
        } else {
            // A fixed point: shrinking the result again keeps nothing.
            let g2 = pine_conformance::fuzz::Generated {
                ast: out.program.clone(),
                source: out.source.clone(),
                ..g.clone()
            };
            let again = shrink(
                &g2,
                &sig,
                &mut Planted { calls: 0 },
                ShrinkBudget::default(),
            );
            assert!(again.trace.is_empty(), "not a fixed point");
        }
    }
}

#[test]
fn shrink_preserves_the_signature() {
    let (g, sig) = planted_seed();
    let mut p = Planted { calls: 0 };
    let out = shrink(&g, &sig, &mut p, ShrinkBudget::default());
    assert_eq!(out.signature, sig);
    assert_eq!(p.diverges(&out.program), Some(sig));
    assert_eq!(out.source, ast::render(&out.program));
}

#[test]
fn shrink_reduces_a_planted_divergence_to_under_15_lines() {
    let (g, sig) = planted_seed();
    let mut p = Planted { calls: 0 };
    let out = shrink(&g, &sig, &mut p, ShrinkBudget::default());
    println!(
        "planted seed {}: {} → {} lines in {} steps\n{}",
        g.seed,
        out.lines_before,
        out.lines_after,
        out.steps,
        ast::render_body(&out.program)
    );
    assert!(out.lines_before > 30);
    assert!(out.lines_after < 15, "{} lines", out.lines_after);
}
