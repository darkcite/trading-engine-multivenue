---
description: Run the full Rust + Python test matrix and report pass/fail per suite
allowed-tools: Bash(cargo nextest*), Bash(cargo test*), Bash(make *), Bash(uv run *), Bash(cd *)
---

Run the full test matrix. Do not claim success unless all four suites pass.

## What to run, in order

1. `cargo fmt --all -- --check` — style must be clean.
2. `cargo clippy --workspace --all-targets -- -D warnings` — no warnings.
3. `cargo nextest run --workspace` — unit + proptest + integration.
4. `cargo test -p bench --test alloc_assertions --release -- --nocapture`
   — must show 0 B/op on every hot-path assertion.
5. `cd claude-worker && uv run pytest` — Python side.

## Output format

Report each step as `STEP N: PASS|FAIL — <headline>`. If any step fails,
show the last 30 lines of its output and stop. Do not try to auto-fix.
