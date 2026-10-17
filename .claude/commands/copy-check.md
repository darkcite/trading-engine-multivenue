---
description: Run the zero-copy ratchet (scripts/copy-audit.sh) and fail on any NEW unmarked copy
allowed-tools: Bash(make copy-audit*), Bash(scripts/copy-audit.sh*), Bash(bash scripts/copy-audit.sh*)
---

Run the zero-copy gate. It sits beside `/alloc-check`: that one asks
"does the hot path allocate?", this one asks "does it move bytes it did
not have to?" — a path can be 0 B/op and still memcpy a frame three times.

## What to run

```sh
make copy-audit
```

## What it means

`scripts/copy-audit.sh` lists every byte-copy verb in the crates the
Makefile's `copy-audit` target names (the list of record: the exec lane,
core-net, core-ring, every ingress crate, the HYPARB crates, strategy-xmm,
strategy-hcv, core-settle, core-fill, the engine and the state writers)
that has no `// COPY:` line within the eight lines above it, against the
committed baseline `scripts/copy-audit-baseline.txt`. A NEW unmarked copy
is exit 1; a baseline entry that no longer matches is debt paid.

## Output format

Pass: the one-line summary it prints (`copy-audit: hits=… baselined=…
new=0 paid=…`). Fail: every NEW line and, for each, whether it can be made
zero-copy (name how) or needs a `// COPY:` line (what · bound · why
unavoidable · alternative rejected). Never run `--update-baseline` (the
operator's call) and do not auto-fix.
