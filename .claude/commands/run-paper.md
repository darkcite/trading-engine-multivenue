---
description: How to smoke-test a change against live venues without breaking ONE ENGINE (the launchd engine is the only engine)
disable-model-invocation: true
allowed-tools: Bash(cargo test --release -p cli --test *)
---

The launchd instance (`com.multivenue.engine`) IS the engine: port 9191 and
`ai.sock` are singletons, so never boot a second one beside it
(`make run-paper` / `cargo run … run --paper` would collide). Slot 3 can be
LIVE: read `/state` (`exec` block) before touching the running engine.

## Smoke a change WITHOUT stopping the engine (default)

```sh
cargo test --release -p cli --test <name>_live_smoke -- --ignored --nocapture
```

`<name>` ∈ `mexc`, `binance_md`, `binance_coinm`, `hypercall`, `hyperevm`.
Each runs under the `CARGO_TARGET_DIR` its module doc names, so the
wrapper's `target/release/multivenue-engine` is never relinked
(`*_SMOKE_SECS` ≤ 900). A boot-REST `Timeout` on a fresh binary is LuLu.

## A real paper boot (the operator's, at a safe minute)

Stopping and restarting the launchd engine is the operator's call: an armed
engine passes `scripts/exec-smoke.sh` first, restarts avoid the 15-minute
grid, and after ANY restart `/state` must show `vm_rows_active ≥ 1`. Ask
before doing any of it.
