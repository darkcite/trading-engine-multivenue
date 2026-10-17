// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `pine-conf` — the operator binary (impl-06 §3.12). Argv parsed by hand (no `clap`); exit 0 =
//! green, 1 = a divergence or a gate failure, 2 = usage or an unreadable input. No clock.
//!
//! This edition implements `gen [--check]`, `sheets --dialect 5|6` (§4.7.5), `distil` (both forms,
//! §4.7.5), `status`, `barhash --bars DIR [--write]` (§4.7.4), `shrink` (§4.8.5, T22) and `pins` (the
//! §4.9.4 `pins.tsv` rows on stdout; `--decide` lands with `cmp.rs`, T16); every other subcommand is a usage error until its
//! task lands. `pin:06-conf-root` — the workspace root is the current directory (DOCTRINE: commands
//! run from the lane root); it must hold `tests/conformance/chart.json`.
//!
//! COPY-DOCTRINE: an offline operator tool; it reads fixture files and never runs a bar step.

use std::path::Path;
use std::process::ExitCode;

use pine_conformance::capture;
use pine_conformance::fixture::{Expect, Fixture, FixtureId, Group};
use pine_ir::Dialect;

const USAGE: &str = "usage: pine-conf gen [--check] | pine-conf sheets --dialect 5|6 \
| pine-conf distil --sheet S --chart CSV [--trades CSV --perf CSV] --utc T --build B --dialect 5|6 [--verify] \
| pine-conf distil --id ID --paste compiled|refused|runtime-error --utc T --build B --dialect 5|6 [--verify] \
| pine-conf status | pine-conf pins | pine-conf barhash --bars DIR [--write] \
| pine-conf shrink --grammar V --seed S --mode mode-vs-mode|interleaved [--budget STEPS] \
| pine-conf report --tv [--out MD] [--bars DIR] [--title T]";

/// The vault capture directory (§4.7.10, untracked) and its `manifest.tsv`.
///
/// `pin:06-distil-vault-manifest` — `distil` (not `--verify`) appends `sheet TAB fixtures TAB utc TAB
/// dialect TAB raw-sha256 TAB build` when the vault capture directory exists (P6 makes it); without
/// it, a note on stderr and nothing written.
const VAULT_CAPTURE: &str = "docs/research/pvm/capture";

/// `pin:06-status-uncapturable` — §4.7.6 records an uncapturable fixture "with a reason in the vault
/// log"; `status` reads the ids from the vault's `uncapturable.tsv` (`ID TAB reason`) when present.
const UNCAPTURABLE_TSV: &str = "docs/research/pvm/capture/uncapturable.tsv";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(".");
    if !root.join(pine_conformance::chart::CHART_JSON).is_file() {
        eprintln!("pine-conf: run from the workspace root (no tests/conformance/chart.json here)");
        return ExitCode::from(2);
    }
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["gen"] => match pine_conformance::gen::write(root) {
            Ok(n) => {
                let (w, c) = pine_conformance::gen::group_b::derived_counts();
                println!("pine-conf gen: wrote {n} fixture(s); B-wide {w}, B-chart {c}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("pine-conf gen: {e}");
                ExitCode::from(2)
            }
        },
        ["gen", "--check"] => {
            let diffs = pine_conformance::gen::check(root);
            for d in &diffs {
                println!("{d}");
            }
            if diffs.is_empty() {
                println!("pine-conf gen --check: byte-stable");
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        ["pins"] => {
            // §4.9.4's `pins.tsv` rows on stdout, from the `.fix` files on disk.
            let mut fixtures = Vec::new();
            for g in pine_conformance::gen::generate() {
                let parsed = std::fs::read_to_string(root.join(&g.fix_path))
                    .map_err(|e| format!("{}: {e}", g.fix_path))
                    .and_then(|t| {
                        pine_conformance::fixture::Fixture::parse(&t, &g.fix_path)
                            .map_err(|e| e.to_string())
                    });
                match parsed {
                    Ok(f) => fixtures.push(f),
                    Err(e) => {
                        eprintln!("pine-conf pins: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            print!("{}", pine_conformance::pins::tsv(&fixtures));
            ExitCode::SUCCESS
        }
        ["barhash", "--bars", dir] => barhash(root, dir, false),
        ["barhash", "--bars", dir, "--write"] => barhash(root, dir, true),
        ["sheets", "--dialect", d] => match dialect(d) {
            Some(d) => sheets(root, d),
            None => usage(),
        },
        ["status"] => status(root),
        ["distil", rest @ ..] => distil(root, rest),
        ["shrink", rest @ ..] => shrink(root, rest),
        ["report", "--tv", rest @ ..] => report_tv(root, rest),
        _ => usage(),
    }
}

/// `pine-conf report --tv [--out MD] [--bars DIR] [--title T]` (R-P1, 2026-10-17): every captured
/// TradingView fixture (`run::is_tv_captured`) through the real engine with the captured
/// `syminfo.tsv`, classified pass / differ / refused-by-us / fault-by-us / harness-gap
/// (`triage::classify_tv`), rendered by `report::render_tv` to `--out` (else stdout). Exit 0 = every
/// fixture passed, 1 = a non-pass, 2 = usage or an unreadable input.
///
/// `pin:06-report-tv-cli` — without `--bars` every `Chart` / `SynSig` fixture is a harness gap
/// (`NoBarsRoot`); the Mac passes its prepared bar root.
fn report_tv(root: &Path, rest: &[&str]) -> ExitCode {
    use pine_conformance::run::{self, Runner, RunnerOptions, SyminfoSource};
    use pine_conformance::triage;
    let mut out: Option<&str> = None;
    let mut bars: Option<&str> = None;
    let mut title = "Conformance run — engine vs the TradingView capture";
    let mut i = 0;
    while i < rest.len() {
        match (rest[i], rest.get(i + 1)) {
            ("--out", Some(v)) => out = Some(v),
            ("--bars", Some(v)) => bars = Some(v),
            ("--title", Some(v)) => title = v,
            _ => return usage(),
        }
        i += 2;
    }
    let mut opts = RunnerOptions::new(root.to_path_buf());
    opts.limits = run::TV_LIMITS;
    opts.run_tv_only = true;
    opts.bars_root = bars.map(std::path::PathBuf::from);
    let mut runner = match Runner::new(opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pine-conf report: {e:?}");
            return ExitCode::from(2);
        }
    };
    if runner.syminfo_source() != SyminfoSource::Captured {
        eprintln!("pine-conf report: tests/conformance/syminfo.tsv (the capture) is missing");
        return ExitCode::from(2);
    }
    let all = match run::load_tree(root) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("pine-conf report: {e}");
            return ExitCode::from(2);
        }
    };
    let tv: Vec<&Fixture> = all.iter().filter(|f| run::is_tv_captured(f)).collect();
    let mut rows = Vec::with_capacity(tv.len());
    for f in &tv {
        let c = runner.check_full(f);
        rows.push((*f, triage::classify_tv(f, &c)));
    }
    let captured = all
        .iter()
        .filter(|f| f.oracle.is_captured_kind() && f.expect != Expect::Pending)
        .count();
    let pending: Vec<&str> = all
        .iter()
        .filter(|f| f.oracle.is_captured_kind() && f.expect == Expect::Pending)
        .map(|f| f.id.as_str())
        .collect();
    let ctx = vec![
        format!(
            "{captured} captured fixtures (oracle tv / tv_only); ran {} — excluded this round: {} \
             (R-P3 redesign); pending, not captured: {}",
            rows.len(),
            run::TV_EXCLUDED.join(" "),
            pending.join(" ")
        ),
        "engine: pine-lang → pine-sema → verify → pine-vm → pine-std → TvBroker; syminfo: the \
         captured tests/conformance/syminfo.tsv; limits: DEFAULT_LIMITS with tier 3 active \
         (pin:06-tv-run-tier3); tv_only fixtures run (pin:06-tv-only-runs-in-report)"
            .to_string(),
        format!(
            "bars root: {} (pin:06-report-tv-cli)",
            bars.unwrap_or("none — every Chart / SynSig fixture is a harness gap")
        ),
        "R-P1: TradingView is the oracle — every non-pass is an engine (or harness) item; \
         pine-vm / pine-ir items go through a CAR"
            .to_string(),
        "Rt series bodies compare only their non-bookkeeping columns (rt, done, upd, k are \
         bookkeeping, pin:06-rt-bookkeeping-columns)"
            .to_string(),
    ];
    let text = pine_conformance::report::render_tv(title, &ctx, &rows);
    let c = pine_conformance::report::tv_counts(&rows);
    match out {
        Some(p) => {
            if let Err(e) = std::fs::write(p, &text) {
                eprintln!("pine-conf report: {p}: {e}");
                return ExitCode::from(2);
            }
        }
        None => print!("{text}"),
    }
    println!(
        "pine-conf report --tv: {} ran · pass {} · differ {} · refused-by-us {} · fault-by-us {} · harness-gap {}",
        rows.len(),
        c[0],
        c[1],
        c[2],
        c[3],
        c[4]
    );
    for (i, (k, cr, ids)) in pine_conformance::report::tv_clusters(&rows)
        .iter()
        .take(10)
        .enumerate()
    {
        println!(
            "  {:>2}. {:>3}  {:<11} {k}  [{}]",
            i + 1,
            ids.len(),
            triage::crate_name(*cr),
            ids.iter().take(6).copied().collect::<Vec<_>>().join(" ")
        );
    }
    if c[0] == rows.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `pine-conf shrink --grammar V --seed S --mode mode-vs-mode|interleaved [--budget STEPS]`
/// (§3.12, §4.8.5): regenerates `(V, S)`, checks the divergence and shrinks it; prints the
/// signature, the step count and the shrunk body. Exit 1 = a divergence (shrunk and printed), 0 =
/// none, 2 = usage or a grammar version this build does not generate.
///
/// `pin:06-shrink-cli-defaults` — §3.12 names no dialect, profile or bar count: v6, `Indicator`,
/// `DEFAULT_LIMITS.active_tiers` and 128 bars (the `fuzz_meta` sweeps' value).
fn shrink(root: &Path, rest: &[&str]) -> ExitCode {
    use pine_conformance::fuzz::{self, diff, GrammarConfig, Predicate, Profile, ShrinkBudget};
    let get = |k: &str| {
        rest.iter()
            .position(|a| *a == k)
            .and_then(|i| rest.get(i + 1).copied())
    };
    let (Some(v), Some(seed), Some(mode)) = (
        get("--grammar").and_then(|v| v.parse::<u16>().ok()),
        get("--seed").and_then(|s| s.parse::<u64>().ok()),
        get("--mode"),
    ) else {
        return usage();
    };
    let budget = match get("--budget").map(|b| b.parse::<u32>()) {
        None => ShrinkBudget::default(),
        Some(Ok(max_steps)) => ShrinkBudget { max_steps },
        Some(Err(_)) => return usage(),
    };
    if !fuzz::GRAMMAR_VERSIONS.contains(&v) {
        eprintln!(
            "pine-conf shrink: grammar {v} is not one this build generates ({:?})",
            fuzz::GRAMMAR_VERSIONS
        );
        return ExitCode::from(2);
    }
    let cfg = GrammarConfig {
        grammar_version: v,
        ..GrammarConfig::for_profile(
            Profile::Indicator,
            Dialect::V6,
            pine_conformance::run::DEFAULT_LIMITS.active_tiers,
        )
    };
    let g = fuzz::generate(seed, &cfg);
    let h = match diff::Harness::new(root.to_path_buf(), "cli") {
        Ok(h) => h,
        Err(e) => {
            eprintln!("pine-conf shrink: {e:?}");
            return ExitCode::from(2);
        }
    };
    fn go<P: Predicate>(g: &fuzz::Generated, p: &mut P, b: ShrinkBudget) -> ExitCode {
        let Some(sig) = p.diverges(&g.ast) else {
            println!("pine-conf shrink: seed {} does not diverge", g.seed);
            return ExitCode::SUCCESS;
        };
        let out = fuzz::shrink(g, &sig, p, b);
        let exhausted = if out.steps == b.max_steps {
            " (budget exhausted)"
        } else {
            ""
        };
        println!(
            "grammar {} seed {} {:?}\n{} → {} lines in {} steps{exhausted}\n{}",
            g.grammar_version,
            g.seed,
            out.signature,
            out.lines_before,
            out.lines_after,
            out.steps,
            fuzz::ast::render_body(&out.program)
        );
        ExitCode::from(1)
    }
    match mode {
        "mode-vs-mode" => go(&g, &mut diff::ModeVsMode { h, bars: 128 }, budget),
        "interleaved" => go(&g, &mut diff::Interleaved { h, bars: 128 }, budget),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn dialect(s: &str) -> Option<Dialect> {
    match s {
        "5" => Some(Dialect::V5),
        "6" => Some(Dialect::V6),
        _ => None,
    }
}

/// `pine-conf sheets --dialect d` (P2): writes `tests/conformance/sheets/v<d>/`, prints the counts.
fn sheets(root: &Path, d: Dialect) -> ExitCode {
    match pine_conformance::gen::sheet::write(root, d) {
        Ok(all) => {
            let fixtures: usize = all.iter().map(|s| s.ids.len()).sum();
            let plots: usize = all.iter().map(|s| s.plots).sum();
            for g in [Group::A, Group::B, Group::C, Group::D, Group::F] {
                let n = all.iter().filter(|s| s.ids[0].group() == g).count();
                if n > 0 {
                    println!("  {}: {n} sheet(s)", g.letter());
                }
            }
            println!(
                "pine-conf sheets: v{} {} sheet(s), {fixtures} fixture(s), {plots} plot(s)",
                d as u8,
                all.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pine-conf sheets: {e}");
            ExitCode::from(2)
        }
    }
}

fn read_fix(root: &Path, path: &str) -> Result<Fixture, String> {
    let t = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
    Fixture::parse(&t, path).map_err(|e| e.to_string())
}

/// `pine-conf status`: per group, the captured-kind fixtures captured / pending / uncapturable.
fn status(root: &Path) -> ExitCode {
    let unc: Vec<String> = std::fs::read_to_string(root.join(UNCAPTURABLE_TSV))
        .map(|t| {
            t.lines()
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(|l| l.split('\t').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // (group, captured-kind, captured, pending, uncapturable, other oracles)
    let mut rows: Vec<(Group, [u32; 5])> = Vec::new();
    for g in pine_conformance::gen::generate() {
        let f = match read_fix(root, &g.fix_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("pine-conf status: {e}");
                return ExitCode::from(2);
            }
        };
        let grp = f.id.group();
        let k = match rows.iter().position(|(x, _)| *x == grp) {
            Some(k) => k,
            None => {
                rows.push((grp, [0; 5]));
                rows.len() - 1
            }
        };
        let c = &mut rows[k].1;
        if !f.oracle.is_captured_kind() {
            c[4] += 1;
            continue;
        }
        c[0] += 1;
        if unc.iter().any(|u| u == f.id.as_str()) {
            c[3] += 1;
        } else if f.expect == Expect::Pending {
            c[2] += 1;
        } else {
            c[1] += 1;
        }
    }
    println!("group\ttv\tcaptured\tpending\tuncapturable\tother");
    let mut t = [0u32; 5];
    for (g, c) in &rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            g.letter(),
            c[0],
            c[1],
            c[2],
            c[3],
            c[4]
        );
        for i in 0..5 {
            t[i] += c[i];
        }
    }
    println!("total\t{}\t{}\t{}\t{}\t{}", t[0], t[1], t[2], t[3], t[4]);
    ExitCode::SUCCESS
}

fn hex(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `pine-conf distil` — the sheet form or the paste form (§3.12, §4.7.5).
fn distil(root: &Path, rest: &[&str]) -> ExitCode {
    let mut kv: Vec<(&str, &str)> = Vec::new();
    let mut verify = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--verify" => verify = true,
            k if k.starts_with("--") && i + 1 < rest.len() => {
                if kv.iter().any(|(x, _)| *x == k) {
                    return usage();
                }
                kv.push((k, rest[i + 1]));
                i += 1;
            }
            _ => return usage(),
        }
        i += 1;
    }
    let get = |k: &str| kv.iter().find(|(x, _)| *x == k).map(|(_, v)| *v);
    let (Some(utc), Some(build), Some(d)) = (get("--utc"), get("--build"), get("--dialect")) else {
        return usage();
    };
    let Some(d) = dialect(d) else {
        return usage();
    };
    let stamp = capture::CaptureStamp {
        utc: utc.to_string(),
        tv_build: build.to_string(),
        dialect: d,
    };
    let known = [
        "--sheet",
        "--chart",
        "--trades",
        "--perf",
        "--utc",
        "--build",
        "--dialect",
        "--id",
        "--paste",
    ];
    if kv.iter().any(|(k, _)| !known.contains(k)) {
        return usage();
    }
    let rep = match (get("--sheet"), get("--chart"), get("--id"), get("--paste")) {
        (Some(sheet), Some(chart), None, None) => distil_sheet(
            root,
            sheet,
            chart,
            get("--trades"),
            get("--perf"),
            &stamp,
            verify,
        ),
        (None, None, Some(id), Some(p)) => {
            let outcome = match p {
                "compiled" => capture::Paste::Compiled,
                "refused" => capture::Paste::Refused,
                "runtime-error" => capture::Paste::RuntimeError,
                _ => return usage(),
            };
            let Some(fid) = FixtureId::parse(id) else {
                return usage();
            };
            let fix = root.join(format!(
                "{}/fixtures/{}/{}.fix",
                pine_conformance::FIXTURE_ROOT,
                fid.group().letter(),
                fid.as_str()
            ));
            capture::record_paste(&fix, outcome, &stamp, verify)
        }
        _ => return usage(),
    };
    match rep {
        Ok(r) => {
            let ids = |v: &[FixtureId]| v.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(" ");
            let verb = if verify { "verified" } else { "written" };
            println!(
                "pine-conf distil: {verb} {} [{}]",
                r.written.len(),
                ids(&r.written)
            );
            println!(
                "pine-conf distil: differing {} [{}]",
                r.differing.len(),
                ids(&r.differing)
            );
            // pin:06-distil-56 — each I9 refusal is printed; the rest of the sheet was written.
            for (id, why) in &r.refused {
                eprintln!("pine-conf distil: refused {}: {why}", id.as_str());
            }
            if r.differing.is_empty() && r.refused.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("pine-conf distil: {e}");
            ExitCode::from(1)
        }
    }
}

fn distil_sheet(
    root: &Path,
    sheet: &str,
    chart: &str,
    trades: Option<&str>,
    perf: Option<&str>,
    stamp: &capture::CaptureStamp,
    verify: bool,
) -> Result<capture::DistilReport, String> {
    let fmt = capture::load_format(root).map_err(|e| e.to_string())?;
    let raw = std::fs::read(chart).map_err(|e| format!("{chart}: {e}"))?;
    let export = capture::parse_chart_csv(&raw, &fmt)?;
    let tr = match trades {
        Some(p) => {
            let b = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            Some(capture::parse_trades_csv(&b, &fmt)?)
        }
        None => None,
    };
    let sum = match perf {
        Some(p) => {
            let b = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            Some(capture::parse_summary_csv(&b, &fmt)?)
        }
        None => None,
    };
    let rep = capture::distil(
        Path::new(sheet),
        &export,
        tr.as_deref(),
        sum.as_ref(),
        stamp,
        verify,
    )?;
    if !verify {
        let dir = root.join(VAULT_CAPTURE);
        if dir.is_dir() {
            let name = Path::new(sheet)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(sheet);
            let ids: Vec<&str> = rep.written.iter().map(|x| x.as_str()).collect();
            let line = format!(
                "{name}\t{}\t{}\t{}\t{}\t{}\n",
                ids.join(","),
                stamp.utc,
                stamp.dialect as u8,
                hex(&core_crypto::sha256(&raw)),
                stamp.tv_build
            );
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("manifest.tsv"))
                .and_then(|mut f| f.write_all(line.as_bytes()))
                .map_err(|e| format!("{VAULT_CAPTURE}/manifest.tsv: {e}"))?;
        } else {
            eprintln!("pine-conf distil: no {VAULT_CAPTURE}/ — the vault manifest line was not written (P6)");
        }
    }
    Ok(rep)
}

/// `pine-conf barhash`: the §4.7.4 checkpoints over the chart's prepared rows in the replay range
/// `[REPLAY_FROM_MS, TO_MS)`, printed; `--write` also writes `tests/conformance/barhash.tsv`.
fn barhash(root: &Path, dir: &str, write: bool) -> ExitCode {
    use pine_conformance::chart::spec;
    let mut src = pine_conformance::bars::ChartSource::new(Some(std::path::PathBuf::from(dir)));
    let clamp = pine_screen::Clamp {
        from_open_ms: spec::REPLAY_FROM_MS,
        to_close_ms: spec::TO_MS,
    };
    let rows = match src.rows(spec::SYMBOL_KEY, spec::TIMEFRAME_S, clamp) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pine-conf barhash: {e:?}");
            return ExitCode::from(2);
        }
    };
    let cps = pine_conformance::capture::barhash(rows, spec::REPLAY_FROM_MS, spec::TO_MS);
    let n = pine_conformance::capture::window_rows(rows, spec::REPLAY_FROM_MS, spec::TO_MS);
    let tsv = pine_conformance::capture::barhash_tsv(&cps, n);
    print!("{tsv}");
    if write {
        let p = root.join("tests/conformance/barhash.tsv");
        if let Err(e) = std::fs::write(&p, tsv) {
            eprintln!("pine-conf barhash: {}: {e}", p.display());
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
