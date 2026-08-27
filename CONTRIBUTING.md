# Contributing

## Licensing of contributions

By submitting a contribution you agree that it is licensed under the
**Apache License, Version 2.0**, per §5 of that license, with no additional
terms or conditions. No CLA is required — §5 is self-executing.

Only submit work you own or are licensed to relicense under Apache-2.0. If
a contribution derives from third-party material of any kind — code, a
strategy specification, a dataset, measured results — say so in the pull
request, and it will be recorded in [`NOTICE`](./NOTICE) before it is
merged. Material with unconfirmed provenance does not enter git history.

## Source file headers

Every `.rs`, `.py` and `.sh` file must begin with:

```
SPDX-License-Identifier: Apache-2.0
Copyright 2026 Anton (darkcite)
```

in that file's comment syntax — after the shebang in shell scripts, above
the `//!` module doc in Rust, above the module docstring in Python. Run:

```sh
make license-check
```

It fails on the first file missing the identifier, and also fails if
`claude-worker/LICENSE` or `claude-worker/NOTICE` have drifted from the
repo-root originals (`make sync-license` restores them).

Adding a dependency changes the license surface of the shipped binary. Run
`make license-deps` and commit the regenerated `THIRD-PARTY-NOTICES.md`
with the change; if `cargo deny check licenses` rejects the new license,
that is a decision to make deliberately in [`deny.toml`](./deny.toml), not
a line to append reflexively.

## Before you write code

Read [`CLAUDE.md`](./CLAUDE.md) first — it is the binding engineering
contract, not a style guide. The rules that fail the build:

- **Zero allocations in hot paths.** No `Vec::push`, `format!`,
  `to_string`, `Box::new`, `Vec::from`. Preallocate at boot, reuse forever.
  Enforced by `core-alloc::CountingAllocator`.
- **No `dyn Trait`, no `async`, no `tokio`, no `serde_json`, no `reqwest`,
  no `ethers`/`alloy`** anywhere the engine loop can reach.
- **Every ingress parser gets a property test *and* a fuzz target.**
- **Every public function gets a happy-path test and a failure-mode test.**
- **Python (`claude-worker/`): full `import x` only — never
  `from x import y`.**

## Checks that must be green

```sh
cargo nextest run --workspace                                    # 1240
cargo test -p bench --test alloc_assertions --release -- --test-threads=1   # 38, 0 B/op
cd claude-worker && uv run pytest                                # 439
make lint license-check
```

`--test-threads=1` on the allocation gate is required, not optional: the
counting allocator is process-global and parallel threads pollute each
other's deltas.

## Frozen surfaces

`claude-worker/src/claude_worker/backtest.py`, `cli.py` and the operator
verb surface are **frozen** — the Rust harness conforms to the worker, not
the other way round. Do not modify them without an explicit ruling.
