// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! The `pine-corpus` command line (impl-09 §3.4, amended by AMEND-harvest: `import`,
//! `import-lib`, `--n all`). argv is parsed by hand (no `clap`). Exit codes: 0 done, 1 internal
//! error, 2 usage, 3 input refused (any `CorpusError` except `Canary`, or a refused ledger row),
//! 4 canary.

use std::path::PathBuf;
use std::process::ExitCode;

use pine_corpus::{build, intake, layout, CorpusError};

const USAGE: &str = "usage:
  pine-corpus build   --corpus <dir> --tiers <mask> --n-strategies <N|all> --n-indicators <M|all>
                      [--weights <file>] [--asof-ms <ms>]
  pine-corpus dedup   --corpus <dir> --tiers <mask>
  pine-corpus hist    --corpus <dir> --tiers <mask>
  pine-corpus rank    --corpus <dir> --tiers <mask> --n-strategies <N|all> --n-indicators <M|all>
                      [--weights <file>] [--asof-ms <ms>]
  pine-corpus libs    --corpus <dir>
  pine-corpus verify  --corpus <dir>
  pine-corpus explain --corpus <dir> --slug <slug> [--profile exact|fork]
  pine-corpus scan    --file <path.pine>
  pine-corpus import     --corpus <dir> --tsv <manual.tsv> --intake <dir> [--check]
  pine-corpus import-lib --corpus <dir> --tsv <libs.tsv> --intake <dir> [--check]";

struct Args {
    verb: String,
    kv: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.kv
            .iter()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
    }
    fn need(&self, k: &str) -> Result<&str, ExitCode> {
        self.get(k).ok_or_else(|| usage(&format!("missing {k}")))
    }
}

fn usage(why: &str) -> ExitCode {
    eprintln!("pine-corpus: {why}\n{USAGE}");
    ExitCode::from(2)
}

fn parse(argv: &[String]) -> Result<Args, ExitCode> {
    let Some(verb) = argv.first() else {
        return Err(usage("no verb"));
    };
    let mut kv = Vec::new();
    let mut flags = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--check" {
            flags.push(a.clone());
            i += 1;
            continue;
        }
        if !a.starts_with("--") || i + 1 >= argv.len() {
            return Err(usage(&format!("bad argument `{a}`")));
        }
        kv.push((a.clone(), argv[i + 1].clone()));
        i += 2;
    }
    Ok(Args {
        verb: verb.clone(),
        kv,
        flags,
    })
}

fn tiers(s: &str) -> Result<u8, ExitCode> {
    let v = match s.strip_prefix("0b") {
        Some(b) => u8::from_str_radix(b, 2).ok(),
        None => s.parse::<u8>().ok(),
    };
    match v {
        Some(t) if t > 0 && t < 16 => Ok(t),
        _ => Err(usage("--tiers is a mask in 1..=15 (decimal or 0b binary)")),
    }
}

fn n_of(s: &str) -> Result<u32, ExitCode> {
    if s == "all" {
        return Ok(u32::MAX);
    }
    s.parse().map_err(|_| usage("--n-* is an integer or `all`"))
}

fn refused(e: &CorpusError) -> ExitCode {
    eprintln!("pine-corpus: {e}");
    match e {
        CorpusError::Canary { .. } => ExitCode::from(4),
        _ => ExitCode::from(3),
    }
}

fn corpus(a: &Args) -> Result<layout::Corpus, ExitCode> {
    let c = a.need("--corpus")?;
    layout::Corpus::open(&PathBuf::from(c)).map_err(|e| refused(&e))
}

fn opts(a: &Args, rank: bool) -> Result<build::BuildOpts, ExitCode> {
    let t = tiers(a.need("--tiers")?)?;
    let (ns, ni) = if rank {
        (
            n_of(a.need("--n-strategies")?)?,
            n_of(a.need("--n-indicators")?)?,
        )
    } else {
        (0, 0)
    };
    let weights = match a.get("--weights") {
        Some(f) => {
            let text = std::fs::read_to_string(f).map_err(|e| usage(&format!("--weights: {e}")))?;
            pine_corpus::rank::parse_weights(&text)
                .map_err(|l| usage(&format!("--weights: bad line `{l}`")))?
        }
        None => pine_corpus::rank::Weights::default(),
    };
    let asof_ms = match a.get("--asof-ms") {
        Some(s) => Some(s.parse().map_err(|_| usage("--asof-ms is an integer"))?),
        None => None,
    };
    Ok(build::BuildOpts {
        tiers: t,
        n_strategies: ns,
        n_indicators: ni,
        weights,
        asof_ms,
        write_hist: true,
        write_rank: rank,
    })
}

fn print_report(r: &intake::Report, check: bool) {
    for v in &r.rows {
        let key = v.key.as_deref().unwrap_or("-");
        if v.refused.is_empty() {
            println!(
                "line {}: {key}: accepted{}",
                v.line,
                if v.appends { "" } else { " (unchanged)" }
            );
        } else {
            for why in &v.refused {
                println!("line {}: {key}: REFUSED: {why}", v.line);
            }
        }
        for w in &v.warnings {
            println!("line {}: {key}: warning: {w}", v.line);
        }
    }
    println!(
        "{}: {} accepted, {} refused, {} appended, {} files written",
        if check { "check" } else { "import" },
        r.accepted(),
        r.refused(),
        r.appended,
        r.written
    );
}

fn run(a: &Args) -> Result<(), ExitCode> {
    match a.verb.as_str() {
        "build" | "rank" | "dedup" | "hist" => {
            let c = corpus(a)?;
            let rank = matches!(a.verb.as_str(), "build" | "rank");
            let mut o = opts(a, rank)?;
            if a.verb == "dedup" {
                o.write_hist = false;
            }
            if a.verb == "rank" {
                o.write_hist = false;
            }
            let s = build::build(&c, &o).map_err(|e| refused(&e))?;
            println!(
                "scripts {} parsed_ok {} collapse_ppm {}{} prescreen_clean {} asof_ms {} corpus_sha256 {}",
                s.scripts,
                s.parsed_ok,
                s.collapse_ppm,
                if s.canary_reported {
                    " (outside [200000, 750000]: reported, not a stop — manual intake or N < 200)"
                } else {
                    ""
                },
                s.prescreen_clean,
                s.asof_ms,
                pine_corpus::hex64(&s.corpus_sha256)
            );
            for (c, n) in &s.excluded {
                println!("excluded {} {}", c.0, n);
            }
            Ok(())
        }
        "libs" => {
            let c = corpus(a)?;
            let p = build::libs(&c).map_err(|e| refused(&e))?;
            println!(
                "wants {} over_depth {} cycles {} case_collisions {} bad_segments {} resolved {}",
                p.wants.len(),
                p.over_depth.len(),
                p.cycles.len(),
                p.case_collisions.len(),
                p.bad_segments.len(),
                p.resolved
            );
            for w in &p.wants {
                println!(
                    "want {} depth {} dependents {}",
                    w.key.text(),
                    w.depth,
                    w.dependents
                );
            }
            for cy in &p.cycles {
                println!("cycle {}", pine_corpus::libs::cycle_text(cy));
            }
            Ok(())
        }
        "verify" => {
            let c = corpus(a)?;
            let r = pine_corpus::verify::run(&c).map_err(|e| refused(&e))?;
            println!(
                "sources {} missing {} lock_failures {} conservation {:?} records {}",
                r.sources,
                r.missing.len(),
                r.lock_failures.len(),
                r.conservation,
                r.records
            );
            for (s, f) in &r.missing {
                println!("missing {s} {f}");
            }
            for k in &r.lock_failures {
                println!("lock {k}");
            }
            if r.clean() {
                Ok(())
            } else {
                Err(ExitCode::from(3))
            }
        }
        "explain" => {
            let c = corpus(a)?;
            let slug = a.need("--slug")?;
            let idx = pine_corpus::index::load(&c.index_path()).map_err(|e| refused(&e))?;
            let Some(r) = idx.records.iter().find(|r| r.core.slug == slug) else {
                return Err(usage(&format!("no record for slug `{slug}`")));
            };
            let Some(h) = r.core.sha256 else {
                return Err(usage("the record has no source"));
            };
            let src = std::fs::read_to_string(c.src_path(&h))
                .map_err(|e| refused(&CorpusError::Io(e)))?;
            let profiles: Vec<pine_corpus::Profile> = match a.get("--profile") {
                Some("exact") => vec![pine_corpus::Profile::Exact],
                Some("fork") => vec![pine_corpus::Profile::Fork],
                None => vec![pine_corpus::Profile::Exact, pine_corpus::Profile::Fork],
                Some(_) => return Err(usage("--profile is exact | fork")),
            };
            for p in profiles {
                let mut buf = Vec::new();
                println!("== {p:?}");
                match pine_corpus::canon::stream(&src, p, &mut buf) {
                    Ok(()) => {
                        let mut out = String::new();
                        pine_corpus::render(&buf, &mut out);
                        print!("{out}");
                    }
                    Err(codes) => println!("parse failed: {codes:?}"),
                }
            }
            Ok(())
        }
        "scan" => {
            let f = a.need("--file")?;
            let src = std::fs::read_to_string(f).map_err(|e| refused(&CorpusError::Io(e)))?;
            let s = pine_corpus::scan::scan(&src);
            println!(
                "dialect {:?} kind {:?} parse_codes {:?}",
                s.dialect, s.kind, s.parse_codes
            );
            for i in &s.imports {
                println!("import {}", i.text());
            }
            for u in &s.uses {
                println!(
                    "use {} calls {} values {} types {}",
                    u.full, u.calls, u.values, u.types
                );
            }
            for u in &s.unknown {
                println!("unknown {u}");
            }
            for m in &s.methods_unresolved {
                println!("unresolved {m}");
            }
            println!(
                "inputs {} k_numeric {} plots top {} nested {} hline {}",
                s.inputs.len(),
                pine_corpus::inputs::k_numeric(&s.inputs),
                s.plots.plot_top,
                s.plots.plot_nested,
                s.plots.hline
            );
            for p in [pine_corpus::Profile::Exact, pine_corpus::Profile::Fork] {
                let mut buf = Vec::new();
                if pine_corpus::canon::stream(&src, p, &mut buf).is_ok() {
                    let mut out = String::new();
                    pine_corpus::render(&buf, &mut out);
                    println!(
                        "== {p:?} {}",
                        pine_corpus::hex64(&pine_corpus::sha256(&buf))
                    );
                    print!("{out}");
                }
            }
            Ok(())
        }
        "import" | "import-lib" => {
            let c = corpus(a)?;
            let tsv = PathBuf::from(a.need("--tsv")?);
            let intake_dir = PathBuf::from(a.need("--intake")?);
            let check = a.flags.iter().any(|f| f == "--check");
            let src = intake::Source {
                tsv: &tsv,
                intake: &intake_dir,
            };
            let r = if a.verb == "import" {
                intake::import(&c, &src, check)
            } else {
                intake::import_lib(&c, &src, check)
            }
            .map_err(|e| refused(&e))?;
            print_report(&r, check || r.refused() > 0);
            if r.refused() > 0 {
                Err(ExitCode::from(3))
            } else {
                Ok(())
            }
        }
        v => Err(usage(&format!("unknown verb `{v}`"))),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = match parse(&argv) {
        Ok(a) => a,
        Err(c) => return c,
    };
    match run(&a) {
        Ok(()) => ExitCode::SUCCESS,
        Err(c) => c,
    }
}
