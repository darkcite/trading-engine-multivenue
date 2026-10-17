---
description: Run one fuzz target for a bounded time (default 60 s) and report execs/s, crashes and new corpus
argument-hint: "<target_name> [seconds]"
allowed-tools: Bash(cargo +nightly fuzz*)
---

Run one cargo-fuzz target for a bounded time. `+nightly` is REQUIRED on
this host (the 1.88 pin has no `-Z`).

## Arguments

- Word 1: the target. `cargo +nightly fuzz list` prints all of them (one
  per parser in `fuzz/fuzz_targets/`). Default `polymarket_clob_frame`.
- Word 2 (optional): seconds, default 60. Keep it ≤ 7200 (the ≤ 2 h law).

## What to run

```sh
cargo +nightly fuzz run <target> -- -max_total_time=<seconds>
```

## Output format

Executions/sec, crashes, new corpus entries. On a crash print the
offending input as a hex dump and stop — do not continue fuzzing.
