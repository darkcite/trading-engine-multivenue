# AGENTS.md — Multivenue Trading Engine

Tool-agnostic brief for any AI coding agent (Claude, Cursor, Codex, etc.) working in this repository. If you are Claude, also read `CLAUDE.md` for the richer context.

## Project summary

Pure-Rust, zero-allocation, zero-copy, single-writer, lock-free HFT engine executing an eight-slot strategy set across Binance, OKX, Deribit, Hyperliquid (incl. HIP-4 outcome markets), Bybit, Polymarket and Polygon RPC, with a per-slot paper/live execution lane (Hyperliquid arm; testnet first). v1 runs locally on macOS with free-tier external APIs only. Phase 7 migrates to a plain Linux box with no surrounding cloud services.

## Non-negotiable rules

1. **Zero runtime allocations in hot paths.** Verified by `make alloc-assert`.
1b. **Zero-copy.** Everything that can be zero-copy is; an unavoidable copy carries a `// COPY: what · bound · why · alternative rejected` line. Verified by `make copy-audit` (a ratchet against `scripts/copy-audit-baseline.txt`).
2. **No `dyn Trait` in hot paths.** Generics and monomorphization only.
3. **No `tokio`, `serde_json`, `ethers`, `alloy`, `reqwest`, or `async-std` in hot paths.**
4. **No `from x import y` in any Python file.** Full `import x` only.
5. **Secrets come from a single `.env` file** (chmod 600, git-ignored). No Keychain, no KMS, no secret manager.
6. **No cloud services at any phase.** Plain VMs only.
7. **No observability stack** (no Prometheus, no Grafana, no Datadog). TUI + log files + `/metrics` endpoint are enough.
8. **Every parser has a proptest and a cargo-fuzz target.**
9. **Every public function has unit tests.**
10. **Release builds compile with `panic = "abort"`; hot paths use `debug_assert!`, never `panic!`.**

## Build / test commands

```sh
cargo build --release --workspace
cargo nextest run --workspace
cargo test -p bench --test alloc_assertions --release -- --test-threads=1   # must show 0 B/op
cargo +nightly fuzz run <target> -- -max_total_time=300
make lint && make license-check && make copy-audit
cd claude-worker && uv run pytest
```

## Where things live

- `PLAN.md` — the architecture deep-dive and doctrine (its roadmap is historical).
- `docs/risk-policy.md` — caps, kill switches, LAWS E-1..E-9.
- `docs/arch/` (history) and `docs/research/` (git-excluded vault) — read ONLY when asked.
- `CLAUDE.md` — Claude-specific context (build commands, pitfalls, model routing).
- `crates/core-*/` — OS-agnostic primitives.
- `crates/ingress-*/` — external source adapters.
- `crates/strategy-*/` — strategies implementing `strategy-core::Strategy`.
- `crates/exec-router/`, `crates/exec-hyperliquid/`, `crates/signer-eip712/`, `crates/clob-dispatcher/` — execution.
- `fuzz/` — cargo-fuzz targets; integration tests live per-crate under each crate's `tests/`.
- `claude-worker/` — Python 3.14, offline only (daemon + operator verbs; never hot path).
- `docs/` — live docs; `docs/arch/` — closed historical plans and progress logs.
- `.claude/` — Claude subagents and slash commands.

## If you are proposing to add

- **`tokio`** anywhere the engine loop touches → **stop**.
- **`serde_json`** in an ingress parser → **stop**, write a byte scanner.
- **A cloud SDK** (`aws-sdk-*`, `google-cloud-*`, etc.) → **stop**.
- **Prometheus, Grafana, Terraform, Ansible** → **stop**.
- **Paid-API feeds** before Phase 6 → **stop**.
- **Anything that allocates in a hot path** → **stop**.

Open an ADR in `docs/adr/` before any of the above.
