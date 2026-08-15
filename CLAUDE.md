# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo. Read `PLAN.md` for the full architecture.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that executes latency-arbitrage trades on Polymarket's CLOB. v1 runs locally on a MacBook Pro M4 using only free-tier external APIs. Claude (via the `claude-worker` Python process) acts as an offline strategy researcher — never in the hot path.

## Build / test / run

```sh
# build (debug)
cargo build --workspace

# build (release — what we actually deploy)
cargo build --release --workspace

# run the full test suite (unit + proptest + integration)
cargo nextest run --workspace

# run only fast tests (skip fuzz/bench compile)
make test-fast

# run fuzz targets for 5 minutes each (CI default)
cargo fuzz run polymarket_clob_frame -- -max_total_time=300

# run allocation assertions — this MUST show 0 B/op on hot paths
# --test-threads=1 is REQUIRED: CountingAllocator is process-global;
# parallel threads pollute each other's AllocGuard deltas (or use `make alloc-assert`)
cargo test -p bench --test alloc_assertions --release -- --test-threads=1

# criterion benches
cargo bench --workspace

# python claude-worker tests
cd claude-worker && uv run pytest

# start the engine locally (paper mode)
cargo run --release -p cli -- run --config ~/multivenue/config.toml --paper

# audit a capture run (every run writes PMLR capture to
# <MULTIVENUE_LOG_DIR>/run-<epoch_ns>/ — per-venue ticks/events/signals
# + optional --raw-tap payload tap)
cargo run --release -p cli -- audit-replay --dir ~/multivenue/logs/run-<ns>
```

## Hard architectural rules (do not violate — the build will fail if you do)

### Rust
- **Zero allocations in hot paths.** No `Vec::push`, no `format!`, no `to_string`, no `Box::new`, no `Vec::from`. Preallocate at boot, reuse forever. Enforced by `core-alloc::CountingAllocator` in tests.
- **No `dyn Trait` in hot paths.** Strategies are monomorphized via `Engine<S: Strategy>`. Generic dispatch, not virtual.
- **No `tokio` on hot path.** Tokio allocates and is cooperative-scheduled. Bootstrap only, if at all.
- **No `serde_json` on hot path.** Every WS/HTTP parser is a handwritten byte scanner over `&[u8]`.
- **No `ethers` / `alloy` full stacks.** We use `secp256k1` + `tiny-keccak` directly.
- **No `async-std`, no `reqwest`.** Hyper + rustls only for HTTP/2.
- **No `foreach`/iterator overhead in hot loops.** Raw indices, `get_unchecked` inside safe wrappers.
- **No bounds checks in hot loops.** Hot loops use `unsafe` blocks with `// SAFETY:` comments; safe wrappers uphold invariants.
- **No panics in release hot paths.** Use `debug_assert!`. Release builds have `panic = "abort"`.
- **Every POD struct in hot path is `#[repr(C)]` + `#[derive(Copy, Clone)]`.**
- **Every ring / cache-sensitive struct is `#[repr(align(64))]`.**
- **Strategies implement the `Strategy` trait from `strategy-core`.** No exceptions.
- **Every ingress parser has a property test + a fuzz target.** See §21.3 and §21.4 of PLAN.md.
- **Every public function has at least one happy-path and one failure-mode unit test.**

### Python (`claude-worker/`)
- **Full `import x` only. Never `from x import y`.** This is a codebase-wide preference.
- **No live Anthropic API calls in tests.** Mock at the SDK boundary.

### Secrets
- **`.env` file only.** No macOS Keychain, no AWS KMS, no Vault, no Secrets Manager.
- **`.env` is `chmod 600` and in `.gitignore`. `.env.example` is committed.**
- **Signing key loaded into an `mlock`'d page, zeroized on drop.**

### Deployment
- **No cloud services, at any phase.** Even Phase 7 EC2 is a plain Linux VM — no KMS, no SSM, no CloudWatch, no Terraform, no Ansible.
- **No observability stack.** TUI (`ratatui`) + log files + a trivial `/metrics` endpoint on `127.0.0.1`. No Prometheus, no Grafana.

## Directory guide

- `PLAN.md` — full architecture, phased roadmap, testing strategy.
- `crates/core-*/` — OS-agnostic primitives (rings, time, config, alloc, io, net, parse, simd, crypto).
- `crates/core-crypto/` — handwritten SHA-256 / HMAC-SHA256 / base64 (RFC 4648); no external crypto stacks.
- `crates/core-io/` — PMLR replay log writer/reader + `PmlrCapture` (per-ingress §6.5 capture sink) + raw tap (`PMRT`).
- `crates/ingress-*/src/discovery.rs` — per-venue boot REST discovery (8e): instrument universes, tick/lot metadata, coverage audit.
- `crates/ingress-*/` — one per external source (polymarket, binance, rpc).
- `crates/strategy-*/` — strategies implementing the `Strategy` trait.
- `crates/signer-eip712/` — audited-C-backed signer; do not replace with `ethers`.
- `crates/clob-dispatcher/` — persistent H/2 client; preallocated buffers.
- `crates/cli/` — the main binary.
- `crates/tui/` — read-only dashboard; snapshot-page driven.
- `crates/bench/` — criterion + dhat; allocation assertions live here too.
- Integration tests live per-crate under each crate's `tests/` directory.
  No workspace-level `tests/` is used.
- `fuzz/` — cargo-fuzz targets.
- `claude-worker/` — Python 3.14 worker: `serve` daemon + operator verbs
  (fetch/backtest/push/positions/stage-ruleset/commit-ruleset); Anthropic
  SDK constructed inside `serve` only; never in the hot path.
- `docs/` — wire formats, risk policy, local setup, migration.
- `.claude/` — subagents, slash commands, settings.

## Common pitfalls — if you're about to do one of these, stop

1. **Adding `tokio` to any `crates/core-*` or `crates/ingress-*`.** Use `mio` + handwritten state machines.
2. **Adding `serde_json` to an ingress parser.** Write a byte scanner; see `core-parse`.
3. **Adding `.collect::<Vec<_>>()` in a hot loop.** Preallocate a fixed-size array or InlineArray-equivalent.
4. **Using `String` in a hot path.** Symbols are `type SymbolId = u32;`. News payloads are `&[u8]`.
5. **Using `async fn` on anything the engine loop touches.** Hot path is synchronous.
6. **Adding a `from x import y` in `claude-worker/`.** Full `import x` only.
7. **Proposing to add Prometheus, Grafana, Terraform, AWS SDK, or any cloud service.** Deliberately excluded.
8. **Proposing to add paid API integrations (X, Benzinga, Blocknative) before Phase 6.** Gated on demonstrated P&L.
9. **Proposing to skip tests "because it's a small change".** Zero-alloc assertions exist because small changes have regressed them before.
10. **Trusting `cargo` runs inside the Cowork Linux sandbox.** The mounted-repo fingerprints go stale and produce FALSE GREENS (observed 2026-08-15). Compile and test on the Mac only. Corollary on the Mac: impossible-looking unresolved-import errors right after file edits = stale rmeta — `cargo clean -p <touched crates>` and retry.
11. **Trusting probe fixtures over live boots.** Venue wire drifts (OKX `preopen` empties, 27-byte XPERP ids, Deribit starbase reorder + sci-notation floats were all caught LIVE in 8e). New parsers get a live smoke run before being declared done; `--raw-tap` exists for exactly this.

## Preferred Claude models for tasks in this repo

- **Bulk artifact generation** (topic tagging): Haiku 4.5.
- **Reasoning** (rule parsing, news labeling): Sonnet 4.6.
- **Strategy proposals** (`claude-worker` serve strategist, ruleset
  drafts): Fable 5 (`MODEL_STRATEGIST = "claude-fable-5"`).
- **Hard work** (backtest review, architectural changes): Opus 4.6.

## When in doubt, read

- `PLAN.md` — everything.
- `docs/wire-format.md` — ring slot layouts, replay log format.
- `docs/risk-policy.md` — kill-switch and cap rules.
