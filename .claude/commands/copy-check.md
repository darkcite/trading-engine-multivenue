---
description: Run the zero-copy ratchet (scripts/copy-audit.sh) and fail on any NEW unmarked copy
allowed-tools: Bash(make *), Bash(scripts/copy-audit.sh*), Bash(bash scripts/copy-audit.sh*)
---

Run the zero-copy gate. It sits beside `/alloc-check`: that one asks
"does the hot path allocate?", this one asks "does it move bytes it did
not have to?" — a path can be 0 B/op and still memcpy a frame three
times.

## What to run

```sh
make copy-audit
```

## What it means

`scripts/copy-audit.sh` lists every byte-copy verb in the exec lane +
`core-net` that has no `// COPY:` justification within the eight lines
above it, and compares against the committed baseline
`scripts/copy-audit-baseline.txt` (pre-E1 legacy debt). A NEW unmarked
copy is exit 1. A baseline entry that no longer matches is debt paid.

## Output format

If it passes, print the one-line summary it prints
(`copy-audit: hits=… baselined=… new=0 paid=…`). If it fails, print
every NEW line it lists and, for each, say whether the copy can be
made zero-copy (name how) or needs a `// COPY:` line (say what the
line must state: what, bound, why unavoidable, alternative rejected).
Do not run `--update-baseline` and do not try to auto-fix.
