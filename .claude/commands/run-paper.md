---
description: Build release, run the engine in paper mode against ~/polymarket/config.toml
argument-hint: "[--config <path>]"
allowed-tools: Bash(cargo build*), Bash(cargo run --release*), Bash(make *)
---

Build the workspace in release and start the engine in paper mode. No real
orders are submitted. Use this to smoke-test a fresh checkout or a new change.

## What to run

```sh
cargo build --release --workspace
cargo run --release -p cli -- run --paper --config $ARGUMENTS
```

If `$ARGUMENTS` is empty, default to `~/polymarket/config.toml`. If that file
does not exist, tell the user to copy `config.example.toml` first.
