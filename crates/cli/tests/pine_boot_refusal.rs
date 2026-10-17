// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM impl-08 (F19) against the REAL binary: a `run` whose `--strategy` names `pine` refuses
//! with exit 1 in the `Cmd::Run` arm, BEFORE `run` boots anything — the config load included
//! (`--env-file` names a missing file, which a later refusal would report instead) — while
//! `strategy-mask` still resolves the name for the wrapper. Before the fix the set arm dropped slot 8 silently (`ai+pine` booted
//! as `ai`).

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_multivenue-engine");

fn run(strategy: &str, env_file: &std::path::Path) -> Output {
    Command::new(BIN)
        .args(["run", "--paper", "--strategy", strategy, "--env-file"])
        .arg(env_file)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn multivenue-engine")
}

#[test]
fn a_run_naming_pine_refuses_before_anything_boots() {
    let missing =
        std::env::temp_dir().join(format!("pine-refusal-{}-absent.env", std::process::id()));
    assert!(!missing.exists());
    for s in [
        "pine",
        "ai+pine",
        "ai+vrp+xsd+bin15+hyparb+xmm+hcv+pine",
        "pine+ai",
    ] {
        let out = run(s, &missing);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(1), "{s}: {text}");
        assert!(
            text.contains(cli::pine_boot::PINE_UNCONFIGURABLE),
            "{s}: {text}"
        );
        assert!(
            !text.contains("config load failed"),
            "{s}: refused first: {text}"
        );
        assert!(!text.contains("starting engine"), "{s}: {text}");
    }
    // The control (`run` itself is not entered here: a debug build's `run` frame overflows
    // the 8 MiB main stack on this host whatever the strategy — the refusal sits in the
    // `Cmd::Run` arm, ahead of it): the gate passes every mask without slot 8.
    for s in ["ai", "all", "ai+vrp+xsd+bin15+hyparb+xmm+hcv"] {
        let m = strategy_set::parse_strategy(s).unwrap();
        assert_eq!(cli::pine_boot::check_requested(m, false), Ok(()), "{s}");
    }
}

#[test]
fn strategy_mask_still_resolves_pine_for_the_wrapper() {
    let out = Command::new(BIN)
        .args(["strategy-mask", "ai+pine"])
        .output()
        .expect("spawn multivenue-engine");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "304\tai-exec+vm+pine\n"
    );
    let out = Command::new(BIN)
        .args(["strategy-mask", "all"])
        .output()
        .expect("spawn multivenue-engine");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "255\thyparb+vrp+xsd+bin15+ai-exec+vm+xmm+hcv\n",
        "`all` excludes pine"
    );
}

/// T19, F19 the other way: `--pine` without `pine` in `--strategy` refuses in the `Cmd::Run`
/// arm, before anything boots (the manifest is never read: it does not even exist).
#[test]
fn a_pine_manifest_without_pine_in_the_mask_refuses() {
    let missing = std::env::temp_dir().join(format!("pine-refusal-{}-b.env", std::process::id()));
    let manifest = std::env::temp_dir().join(format!("pine-{}-absent.toml", std::process::id()));
    let out = Command::new(BIN)
        .args(["run", "--paper", "--strategy", "ai", "--env-file"])
        .arg(&missing)
        .arg("--pine")
        .arg(&manifest)
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn multivenue-engine");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains(cli::pine_boot::PINE_NOT_REQUESTED), "{text}");
    assert!(
        !text.contains("config load failed"),
        "refused first: {text}"
    );
}

/// Risk review r15 (pin:08-run-legacy-strategy-refused): `--pine` beside a legacy standalone arm
/// (`ev`, `rule-tree`) refuses in the `Cmd::Run` arm too — before the config load — where the
/// old `.ok()?` let it through.
#[test]
fn pine_beside_a_legacy_strategy_refuses_before_anything_boots() {
    let missing =
        std::env::temp_dir().join(format!("pine-legacy-{}-absent.env", std::process::id()));
    assert!(!missing.exists());
    for s in ["ev", "rule-tree"] {
        let out = Command::new(BIN)
            .args([
                "run",
                "--paper",
                "--strategy",
                s,
                "--pine",
                "absent-manifest.toml",
            ])
            .arg("--env-file")
            .arg(&missing)
            .env_remove("RUST_LOG")
            .output()
            .expect("spawn multivenue-engine");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(1), "{s}: {text}");
        assert!(text.contains(cli::pine_boot::PINE_NOT_A_SET), "{s}: {text}");
        assert!(
            !text.contains("config load failed"),
            "{s}: refused first: {text}"
        );
    }
}
