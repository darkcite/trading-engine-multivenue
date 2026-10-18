---
description: Run the hot-path bench gate against the checked-in baseline, then summarize median / p99
allowed-tools: Bash(make bench-check*), Bash(cargo bench*)
---

## What to run

```sh
make bench-check     # the gate: hot_path vs crates/bench/baselines/hot_path.json, 15 % tolerance on the M4
```

Only if asked for the full picture: `cargo bench --workspace` (criterion).

## Output format

Gate verdict first (`bench-check: PASS|FAIL`), then a table `bench`,
`median_ns`, `p99_ns`, `change_vs_baseline_pct`; call out any regression
beyond the tolerance. Never edit a baseline file.
