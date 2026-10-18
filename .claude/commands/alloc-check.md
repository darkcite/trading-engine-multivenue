---
description: Run the zero-allocation assertions (release, one thread) and fail on any non-zero byte count
allowed-tools: Bash(make alloc-assert*), Bash(cargo test -p bench*), Bash(cargo clean -p bench*)
---

Run the zero-allocation gate — a failure here is a hot-path regression that
must be fixed before merge.

## What to run

```sh
make alloc-assert
# = cargo test -p bench --test alloc_assertions --release -- --nocapture --test-threads=1
```

`--test-threads=1` is REQUIRED: the CountingAllocator is process-global, so
parallel test threads pollute each other's counts.

## Stale-binary guard

The log must show a fresh `Compiling bench`. If it does not, the run is a
false green: `cargo clean -p bench --release` and run again.

## Output format

All pass: `PASS — 0 B/op across N assertions (fresh Compiling bench)`.
Any fail: the failing assertion, the byte count it observed, and the most
likely cause (`git log -p` on the file it exercises). Do not try to auto-fix.
