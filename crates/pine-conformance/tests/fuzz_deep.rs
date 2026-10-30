// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 §4.8 — a bounded deep sweep (r35 hardening), `#[ignore]`d: it runs only on request and
//! reports, never asserts — a finding is triaged by the lead into `fuzz_meta`'s tables and
//! `tests/conformance/fuzz/regressions.tsv` (§4.8.5), never hidden here.
//!
//! ```text
//! PVM_FUZZ_FROM=1000 PVM_FUZZ_N=100000 PVM_FUZZ_SECS=3600 PVM_FUZZ_MODE=mode-vs-mode \
//! PVM_FUZZ_DIALECT=6 PVM_FUZZ_OUT=/path/findings.tsv \
//!   cargo test --release -p pine-conformance --test fuzz_deep -- --ignored --nocapture
//! ```
//!
//! Seeds `FROM .. FROM + N` under the current `GRAMMAR_VERSION`, stopping after `SECS` seconds;
//! `MODE` is `mode-vs-mode` (§4.8.4 a) or `interleaved` (§4.8.4 b); `DIALECT` 5 or 6. Each finding
//! is one TSV row `grammar seed mode dialect kind detail` (kind `diverges` / `panic`), and the last
//! line is a `# summary`. A debug build also meets the engine's `debug_assert!`s (a `panic` row).

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pine_conformance::fuzz::diff::{Harness, Interleaved, ModeVsMode};
use pine_conformance::fuzz::{generate, GrammarConfig, Predicate, Profile, GRAMMAR_VERSION};
use pine_conformance::run::DEFAULT_LIMITS;
use pine_ir::Dialect;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn run<P: Predicate, F: Fn() -> P>(mk: F, mode: &str, d: Dialect, out: &mut Vec<String>) -> u64 {
    let c = GrammarConfig::for_profile(Profile::Indicator, d, DEFAULT_LIMITS.active_tiers);
    let (from, n) = (
        env_u64("PVM_FUZZ_FROM", 1_000),
        env_u64("PVM_FUZZ_N", 10_000),
    );
    let budget = Duration::from_secs(env_u64("PVM_FUZZ_SECS", 600));
    let t0 = Instant::now();
    let mut p = mk();
    let (mut ran, mut last) = (0u64, from);
    for s in from..from.saturating_add(n) {
        if t0.elapsed() > budget {
            break;
        }
        last = s;
        let g = generate(s, &c);
        if g.expect_refusal {
            continue;
        }
        ran += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.diverges(&g.ast))) {
            Ok(Some(sig)) => out.push(format!(
                "{GRAMMAR_VERSION}\t{s}\t{mode}\t{}\tdiverges\t{sig:?}",
                d as u8
            )),
            Ok(None) => {}
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|m| m.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default()
                    .replace(['\t', '\n'], " ");
                out.push(format!(
                    "{GRAMMAR_VERSION}\t{s}\t{mode}\t{}\tpanic\t{msg}",
                    d as u8
                ));
                p = mk();
            }
        }
    }
    out.push(format!(
        "# summary grammar {GRAMMAR_VERSION} mode {mode} dialect {} seeds {from}..={last} ran {ran} \
         findings {} secs {}",
        d as u8,
        out.iter().filter(|l| !l.starts_with('#')).count(),
        t0.elapsed().as_secs()
    ));
    ran
}

#[test]
#[ignore = "r35 deep sweep: run on request (see the module doc)"]
fn deep_sweep() {
    let d = if env_u64("PVM_FUZZ_DIALECT", 6) == 5 {
        Dialect::V5
    } else {
        Dialect::V6
    };
    let mode = std::env::var("PVM_FUZZ_MODE").unwrap_or_else(|_| "mode-vs-mode".to_owned());
    let mut out = Vec::new();
    let ran = if mode == "interleaved" {
        let mk = || Interleaved {
            h: Harness::new(root(), "ilv-deep").expect("harness"),
            bars: 128,
        };
        run(mk, "interleaved", d, &mut out)
    } else {
        let mk = || ModeVsMode {
            h: Harness::new(root(), "mvm-deep").expect("harness"),
            bars: 128,
        };
        run(mk, "mode-vs-mode", d, &mut out)
    };
    for l in &out {
        println!("{l}");
    }
    if let Ok(path) = std::env::var("PVM_FUZZ_OUT") {
        let mut f = std::fs::File::create(path).expect("findings file");
        writeln!(f, "grammar\tseed\tmode\tdialect\tkind\tdetail").unwrap();
        for l in &out {
            writeln!(f, "{l}").unwrap();
        }
    }
    println!("deep_sweep ran {ran} programs");
}
