# Multivenue Trading Engine

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88.0%20pinned-orange.svg)](./rust-toolchain.toml)
[![Python](https://img.shields.io/badge/python-3.14-blue.svg)](./claude-worker/pyproject.toml)

Pure-Rust, zero-allocation, zero-copy, single-writer, lock-free HFT engine
that executes latency-arbitrage trades on Polymarket's CLOB against a
multivenue reference universe (Binance spot/USDM, OKX, Deribit,
Hyperliquid, Polygon RPC — plus a boot-selected options ladder).

**v1 runs locally on a MacBook Pro M4** on free-tier external APIs only.
Phase 7 migrates to a plain Linux EC2 box — no cloud services, no
observability stack, no IaC, at any phase.

Claude (via the `claude-worker` Python process) is an **offline strategy
researcher** — it proposes rulesets, backtests them and stages them
through an HMAC'd UDS command plane. It is never in the hot path.

## Status

| Phase | State |
|---|---|
| Stage 1 (8a–8e) | **CLOSED** — G1 soak blessed |
| Stage 2 (8f–8h) | **CLOSED** — autonomous research loop code-complete, E2E-proven semi-manual |
| M1 — universe config + venue breadth | **CLOSED** |
| M2 — options ingestion (Deribit / OKX / IV channel) | **CLOSED** |
| M3 — continuous data ops (launchd fleet) | **COMPLETE**, calendar-waiting C6 |
| M4 — shadow-P&L attribution | **CLOSED** |
| M5 — research loop on the full universe | **NEXT**, on explicit operator go |
| M6 — MVP soak + sign-off | pending |
| Stage 3 — executor / live ramp | **GATED** — see `docs/mvp-completion-plan.md` §7 |

Stay-green baselines: **1240** nextest · **38** alloc assertions at 0 B/op ·
**439** worker pytest. `CLAUDE.md` carries the authoritative CURRENT STATE
section; the per-phase progress logs under `docs/` carry the latest word.

## Start here

- [CLAUDE.md](./CLAUDE.md) — front-loaded context + CURRENT STATE. Read this first.
- [docs/mvp-completion-plan.md](./docs/mvp-completion-plan.md) — the M-phase authority (§9 data storage is binding).
- [PLAN.md](./PLAN.md) — full architecture, phased roadmap, testing strategy.
- [AGENTS.md](./AGENTS.md) — tool-agnostic brief for any AI coding agent.
- [docs/](./docs/) — wire format, risk policy, local setup, migration notes, per-phase design/progress logs. Closed history lives in [docs/arch/](./docs/arch/).

## Quick start

```sh
# 1. Secrets  (.env only — chmod 600, git-ignored, never printed)
cp .env.example .env && chmod 600 .env
$EDITOR .env

# 2. Universe  (M1: the boot universe is a file, not flags)
cp universe.toml.example ~/multivenue/universe.toml
$EDITOR ~/multivenue/universe.toml

# 3. Build  (G0 law: test gates never relink the release binary —
#    always build -p cli before a live boot)
cargo build --release -p cli

# 4. Tests
cargo nextest run --workspace
cargo test -p bench --test alloc_assertions --release -- --test-threads=1   # MUST show 0 B/op
cd claude-worker && uv run pytest

# 5. Paper-mode run — zero flags, universe comes from the file
cargo run --release -p cli -- run --paper --strategy all
```

Polymarket crypto up/down dailies **expire 16:00Z** — refresh
`universe.toml` via the Gamma lane before a manual boot (the M3 launchd
fleet automates this). After a restart, run `claude-worker fetch` once;
`unresolved=0` is the done-tell.

Legacy fallback (no `universe.toml`): per-venue flags are required —
`--polymarket-asset-id <token id> --okx-symbols … --deribit-symbols … --hl-coins …`.
Boot refuses to start venue-blind.

## Engine subcommands

```sh
multivenue-engine run --paper --strategy all        # spawn ingress + engine, drain until SIGINT
multivenue-engine print-config --env-file ./.env    # resolved non-secret config; smoke-tests the loader
multivenue-engine audit-replay  --dir <run dir>     # per-symbol rates, cadence bands, integrity, venue×channel matrix
multivenue-engine capture-catalog --dir <log root>  # per-run spans, UTC-day continuity, gap map, backtest/monitor views
multivenue-engine backtest --ruleset R --replay-dir D --split 70/30   # deterministic VM replay; schema-1 JSON on stdout
multivenue-engine audit-pnl --dir <log root>        # M4 shadow P&L: logged intents through the strict-cross fill model
```

Every ingress thread writes **PMLR** replay capture (per-venue tick /
event / signal logs, the `engine-orders.pmlr` intent log, plus an optional
`--raw-tap` payload tap) into a per-run directory under
`MULTIVENUE_LOG_DIR`. Each run also drops `instrument-manifest.tsv` (and
`options-manifest.tsv` when an options ladder is live) — options ordinals
reshuffle per boot **by design**, so every offline consumer resolves
symbols through the manifest, never a bare `SymbolId` across runs.

`backtest`'s argv and schema-1 JSON are a **frozen contract** with
`claude-worker`. The harness conforms to the worker, never vice versa.

## claude-worker (offline researcher)

Python 3.14 package under [`claude-worker/`](./claude-worker). One `serve`
daemon plus the frozen operator verb surface:

```
serve  fetch  backtest  push  positions  stage-ruleset  commit-ruleset  pnl
```

Frame-sending verbs (`push`, `stage-ruleset`, `commit-ruleset`) open the
HMAC'd UDS; read-only verbs never touch the socket — a data pull must not
signal AI liveness to the engine. The Anthropic SDK is constructed inside
`serve` **only**; the strategist model is `claude-fable-5`. Verbs are
globally serialized (one SQLite sequence namespace) — `pgrep -f
claude-worker` before invoking one.

## Continuous operation (M3)

```sh
./scripts/install-launchd.sh    # idempotent: renders launchd/*.plist, seeds state, bootstraps the agents
```

Installs the standing engine (`com.multivenue.engine`), the 00:00Z
SIGTERM restart + universe refresh (`com.multivenue.daily-restart`), the
hourly candles agent (`com.multivenue.candles`) and `caffeinate`.
**One engine ever** — port 9191 and `ai.sock` are singletons; any smoke
boot must stop the standing instance first and restart it after.

## Layout

```
crates/
  core-*/            OS-agnostic primitives (alloc, config, crypto, io, latency,
                     metrics, net, parse, ring, simd, time, types)
  ingress-*/         External sources: polymarket, binance, okx, deribit,
                     hyperliquid, rpc + ingress-ai (UDS/HMAC command plane)
  options-select/    Boot-only options-chain selection law (shared by venues)
  book-builder/      Order-book construction
  strategy-*/        core, latency-arb, cross-arb, ev, rule-tree, vm (ruleset VM),
                     set (mask-49 composed set), ai-exec
  research-artifacts/ Ruleset artifacts + validation
  signer-eip712/     EIP-712 signer (secp256k1 + tiny-keccak, no ethers/alloy)
  clob-dispatcher/   Persistent HTTP/2 client, preallocated buffers
  engine/            Engine<S: Strategy>
  cli/               Main binary (multivenue-engine)
  tui/               ratatui dashboard (read-only)
  bench/             criterion + dhat + allocation assertions
fuzz/                cargo-fuzz targets (integration tests are per-crate under tests/)
claude-worker/       Python 3.14 offline Claude researcher (never in the hot path)
launchd/  scripts/   M3 launchd plists + install / restart / candles / retention
docs/                Live docs (arch/ = closed historical plans and logs)
.claude/             Subagents, slash commands, settings
```

`make help` lists the full developer target set.
`config.example.toml` is **operator reference only** — the binary is driven
by env vars + CLI flags + `universe.toml`.

## Hard rules

1. **Zero runtime allocations in hot paths.** Enforced by `alloc_assertions` (0 B/op).
2. **No `tokio`, `serde_json`, `ethers`, `alloy`, `reqwest`, `async-std` on hot paths.** Handwritten byte scanners over `&[u8]`; `mio` + state machines.
3. **No `dyn Trait` in hot paths.** Monomorphization only (`Engine<S: Strategy>`).
4. **No panics in release hot paths.** `debug_assert!` + `panic = "abort"`; fail fast.
5. **Every POD hot-path struct is `#[repr(C)] + Copy`; every ring is `#[repr(align(64))]`.**
6. **Every ingress parser has a property test + a fuzz target.**
7. **No `from x import y` in Python.** Full `import x` only.
8. **Secrets = a single `.env` file** (chmod 600, git-ignored). No Keychain, no KMS, no Vault.
9. **No cloud services at any phase.** Plain VMs only.
10. **No observability stack.** TUI + log files + loopback `/metrics` only.
11. **No git operations without the operator's explicit ask.** No push, rebase, history rewrite or new branches.

See [CLAUDE.md](./CLAUDE.md) for the complete list plus the "stop if you're
about to do this" pitfalls.

## License

Licensed under the **Apache License, Version 2.0** — see [LICENSE](./LICENSE)
and [NOTICE](./NOTICE).

```
Copyright 2026 Anton (darkcite)

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

Every source file carries `SPDX-License-Identifier: Apache-2.0`; `make
license-check` fails the build if one does not. The license covers
documentation source and diagrams (`docs/`, `PLAN.md`, `*.svg`) on the same
terms — Apache-2.0's definition of "Source" is explicit that it includes
documentation source.

Contributions are accepted under Apache-2.0 §5 — a submitted contribution
is licensed under the same terms, without additional conditions. See
[CONTRIBUTING.md](./CONTRIBUTING.md).

### Third-party dependencies

No third-party source is vendored in this tree; dependencies are resolved
at build time from crates.io and PyPI and carry their own licenses. A
compiled binary does link them, so attribution for the binary is generated
by `make license-deps` (cargo-about) into `THIRD-PARTY-NOTICES.md`. **No
distributed binary may ship without `LICENSE`, `NOTICE` and
`THIRD-PARTY-NOTICES.md` alongside it.** `make license-deps` also runs
`cargo deny check licenses` against the allowlist in
[deny.toml](./deny.toml).

### Trademarks

Polymarket, Binance, OKX, Deribit and Hyperliquid are trademarks of their
respective owners. This project is not affiliated with, endorsed by, or
sponsored by any of them, and names them only to describe
interoperability. Per Apache-2.0 §6, this license grants no trademark
rights.

## Disclaimer

This software trades real money on live venues. It is provided **as is**,
without warranty of any kind, and nothing here is financial advice. Running
it in non-paper mode is entirely at your own risk. See
[docs/risk-policy.md](./docs/risk-policy.md) for the kill-switch and cap rules.
