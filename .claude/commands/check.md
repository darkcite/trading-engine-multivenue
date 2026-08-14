---
description: Fast compile-only check across the workspace
allowed-tools: Bash(cargo check*), Bash(make *)
---

Run `cargo check --workspace --all-targets` and report pass/fail.

Do not run tests, benches, or fuzzers — this is a latency-optimized
pre-commit gate.
