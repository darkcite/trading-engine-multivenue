---
description: Run the zero-allocation assertions in release mode and fail on any non-zero byte count
allowed-tools: Bash(cargo test*), Bash(make *)
---

Run the zero-allocation assertions. This is the **most important** gate in
the repo — a failure here means a regression that must be fixed before merge.

## What to run

```sh
cargo test -p bench --test alloc_assertions --release -- --nocapture
```

## Output format

If all pass, print a one-line "PASS — 0 B/op across N assertions". If any
fail, print the failing assertion, the byte count it observed, and the
git diff line that most likely caused the regression (use `git log -p` on
the relevant file). Do not try to auto-fix.
