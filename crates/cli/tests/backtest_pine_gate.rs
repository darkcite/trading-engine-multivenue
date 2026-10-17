// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PVM B'3 T18 (impl-Bp §7.6, I18): the argv refusals of `backtest
//! --strategy pine` exit 2 before any file is opened. The rule law itself
//! (U1–U7, `7002`, exit 3) is pinned in-crate (`backtest::pine::gate`,
//! `nullarm`, `run` tests; pin:Bp-b3-tests-in-crate).

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_multivenue-engine");

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cli-pine-gate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("replay")).unwrap();
    d
}

fn backtest(d: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut c = Command::new(BIN);
    c.args(["backtest", "--strategy", "pine", "--pine-manifest"])
        .arg(d.join("manifest.toml"))
        .arg("--replay-dir")
        .arg(d.join("replay"))
        .args(["--split", "70/30", "--fee-bps", "hl.perp:2:5"])
        .args(["--trial-id", &"ab".repeat(32)])
        .arg("--out-dir")
        .arg(d.join("out"))
        .args(extra);
    c.output().unwrap()
}

#[test]
fn strategy_pine_without_null_arm_exits_2_and_writes_nothing() {
    let d = scratch("nonull");
    let o = backtest(&d, &["--null-seed", "1"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!d.join("out").exists(), "nothing written");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn null_arm_without_a_seed_exits_2_and_writes_nothing() {
    let d = scratch("noseed");
    let o = backtest(&d, &["--null-arm", "sign-flip"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!d.join("out").exists(), "nothing written");
    let _ = std::fs::remove_dir_all(&d);
}
