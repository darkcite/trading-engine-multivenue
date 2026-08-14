---
description: Run criterion benches and print a one-page summary of median / p99 latency
allowed-tools: Bash(cargo bench*), Bash(cargo run --release*)
---

Run criterion benches across the workspace and summarize the results.

## What to run

```sh
cargo bench --workspace
```

## Output format

Table with columns: `bench`, `median_ns`, `p99_ns`, `change_vs_baseline_pct`.
Call out any regression larger than +5% at the p99. Do not edit baseline files.
