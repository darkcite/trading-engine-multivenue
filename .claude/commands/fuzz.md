---
description: Run one fuzz target for a bounded time (default 60s) and report coverage
argument-hint: "<target_name> [seconds]"
allowed-tools: Bash(cargo fuzz*)
---

Run a fuzz target for a bounded time. If `$ARGUMENTS` is empty, default to
`polymarket_clob_frame 60`.

## Parse arguments

- Word 1: target name (one of `polymarket_clob_frame`, `binance_agg_trade`,
  `core_parse_price`).
- Word 2 (optional): seconds (integer).

## What to run

```sh
cargo fuzz run <target> -- -max_total_time=<seconds>
```

## Output format

Report: executions/sec, any crashes, any new corpus entries. If a crash was
found, print the offending input hex-dump and stop — do not continue fuzzing.
