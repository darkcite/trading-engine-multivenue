// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `strategy-mask` against the REAL binary (S16-1c)
//!
//! The launchd wrapper resolves `STRATEGY` through this subcommand —
//! `<mask>\t<members in slot order>` on stdout and exit 0, or the refusal
//! with the valid names on stderr and exit 78 — so the output's shape is
//! a contract with `scripts/engine-wrapper.sh`.

use std::process::{Command, Output};

fn strategy_mask(value: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_multivenue-engine"))
        .args(["strategy-mask", value])
        .output()
        .expect("spawn multivenue-engine")
}

#[test]
fn a_set_prints_its_mask_and_its_members_in_slot_order() {
    for (value, line) in [
        ("ai+vrp", "50\tvrp+ai-exec+vm\n"),
        ("vrp+ai", "50\tvrp+ai-exec+vm\n"),
        ("ai+vm", "48\tai-exec+vm\n"),
        ("bin15+xmm", "72\tbin15+xmm\n"),
        ("all", "255\thyparb+vrp+xsd+bin15+ai-exec+vm+xmm+hcv\n"),
    ] {
        let out = strategy_mask(value);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{value}: {err}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), line, "{value}");
        assert!(
            out.stderr.is_empty(),
            "{value}: no tracing, no log line: {err}"
        );
    }
}

#[test]
fn a_refusal_exits_78_and_names_the_valid_set() {
    for value in ["nope", "", "ai+", "all+ai", "AI", "-x", "-h", "--help"] {
        let out = strategy_mask(value);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{value:?}: {err}");
        assert!(out.stdout.is_empty(), "{value:?}");
        assert!(
            err.contains("valid: `+`-joined names from hyparb, vrp,"),
            "{value:?}: {err}"
        );
    }
}
