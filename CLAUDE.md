# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session should be able to start
from **this file + the current phase's design/progress docs** without
rereading the whole doc set. `PLAN.md` remains the architectural deep-dive.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that executes latency-arbitrage trades on Polymarket's CLOB. v1 runs locally on a MacBook Pro M4 using only free-tier external APIs. Claude (via the `claude-worker` Python process) acts as an offline strategy researcher — never in the hot path.

## CURRENT STATE (updated 2026-08-22 — keep this section current at every phase boundary)

- **Stage 1 (8a–8e) DONE**, G1 soak blessed 2026-08-15 (history: `docs/arch/phase-8-progress.md`).
- **Stage 2:** 8f CLOSED (`7ca91be`) · 8g CLOSED (`39e6542`, G7) · **8h IN PROGRESS**.
- **8h = autonomous research loop** (last Stage-2 phase): `data_fetcher` completion + strategist (Fable 5) + REAL `multivenue-engine backtest` harness + gates + auto-promotion + rollback. Exit criteria: Fable-5-authored ruleset auto-promoted after passing backtest, trading in paper, AND a forced-underperformance rollback demonstrated.
- **8h sessions:** H0 design LOCKED (**H-D1…H-D8**, all option (a)) · H1 harness substrate CLOSED (`3ad40a9`) · H2 §4 fill/fee/latency model CLOSED (`1ed6017`) · H3 data_fetcher §6 CLOSED (`76680db`) · H4 strategist §7 + §8.1/§8.2 auto-promotion CLOSED (`7bd0e42`) · **H5 rollback §8.3/§8.4 CLOSED (this commit)** — `monitor.py` + `ResearchCycle` own the walk-forward monitor (trailing 24 h / 6 h floor, run-granular window, `split=0/100`, net ≤ −$100 OR dd ≥ $200 ⇒ Disable-5 THEN frozen restage/commit of the prior; no-prior ⇒ disable-only + dark guard; report-clobber protection via active-copy scoring; events-ledger active resolution; §7.1 performance seam wired). The full autonomous loop is CODE-COMPLETE. Remaining: **H6 = close + operator-gated live demo ONLY**.
- **NEXT SESSION = H6.** The verbatim H6 kickoff prompt is the last section of `docs/phase-8h-progress.md` — paste it into the fresh session. H6 scope: final gates + the §13.6/§8.5 LIVE demo (real capture, one budget-capped real Fable-5 serve cycle, auto-promotion on a live paper boot, forced-underperformance rollback observed) + the phase-closing entry. **No new feature code.**
- **HARD OPERATOR REQUIREMENT (2026-08-22): when Stage 2 (8f+8g+8h) is FULLY implemented — i.e. at 8h/H6 close with the §12 exit criteria demonstrated — explicitly notify the operator that Stage 2 is complete, and do NOT start ANY Stage-3 work (executor, risk/8i+, venue dispatchers, live ramp — code, plans, or designs) without his explicit confirmation.**
- **Authority chain for 8h:** `docs/prompts/8h-kickoff.md` (frozen H0 prompt) → `docs/phase-8h-design.md` (LOCKED) + the **latest entry in `docs/phase-8h-progress.md`** supersedes the committed plan where they conflict.
- **Frozen contract:** `claude-worker/src/claude_worker/backtest.py` — argv `multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`, schema-1 JSON on stdout, GateThresholds numbers; the frozen 202 worker tests pin it (suite now larger, the 202 stay untouchable). **The harness conforms to the worker, never vice versa.** `backtest.py` and `cli.py` are byte-untouched through H5 (attribution rides `state.stage_ruleset`'s additive params; the monitor rides `run_backtest(split="0/100")` passthrough).
- **Baselines at H5 close:** workspace nextest 1081/1081 (+1 ignored fixture-regen), release alloc 36/36 0 B/op (`--test-threads=1`), worker pytest 354 (202 frozen + 2 real-harness + 43 fetchers + 79 H4 + 28 H5; 354 is the stay-green), fuzz `ruleset_json` 72.3M clean (untouched — no new untrusted-bytes parser exists on either side).
- **Push anomaly KNOWN** (origin/main divergence observed H0): record, never act.
- **Git discipline:** NO push, NO rebase, NO history rewrite, NO new branches, NO git ops without operator ask. Do NOT touch `.env`.
- 8h session notes go ONLY to `docs/phase-8h-progress.md`. If context runs short: write interim state + exact resume point + relaunch prompt there, then tell the operator.

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
# cargo-fuzz v0.13.2 installed; runs `+nightly`; in-repo `cargo install`
# trips the 1.88.0 toolchain pin — install `+stable` from $HOME if needed.

# run allocation assertions — this MUST show 0 B/op on hot paths
# --test-threads=1 is REQUIRED: CountingAllocator is process-global;
# parallel threads pollute each other's AllocGuard deltas (or use `make alloc-assert`)
# False-green guard: confirm a fresh `Compiling bench` in the log, or
# `cargo clean -p bench --release` and rerun (H2 correction: plain
# `-p bench` does NOT remove the release test bin on this toolchain).
cargo test -p bench --test alloc_assertions --release -- --test-threads=1

# criterion benches
cargo bench --workspace

# python claude-worker tests
cd claude-worker && uv run pytest

# start the engine locally (paper mode)
# G0 law: test gates build rlibs/test bins but NEVER relink the release
# binary — ALWAYS `cargo build --release -p cli` before any live boot.
# --polymarket-asset-id is REQUIRED (the market's clobTokenIds decimal
# string; boot refuses to start venue-blind). For AI-cmd work use the
# set path: --strategy all (bare latency-arb can't express AI toggles).
cargo build --release -p cli
cargo run --release -p cli -- run --paper --polymarket-asset-id <TOKEN_ID>

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
- **Offline paths (audit-replay, backtest) MAY allocate** — they are not hot paths; each such module carries a doctrine header saying so.

### Python (`claude-worker/`)
- **Full `import x` only. Never `from x import y`.** This is a codebase-wide preference.
- **No live Anthropic API calls in tests.** Mock at the SDK boundary.
- Anthropic SDK is constructed inside `serve` only; strategist model is `MODEL_STRATEGIST = "claude-fable-5"`.

### Secrets
- **`.env` file only.** No macOS Keychain, no AWS KMS, no Vault, no Secrets Manager.
- **`.env` is `chmod 600` and in `.gitignore`. `.env.example` is committed.**
- **Signing key loaded into an `mlock`'d page, zeroized on drop.**

### Deployment
- **No cloud services, at any phase.** Even Phase 7 EC2 is a plain Linux VM — no KMS, no SSM, no CloudWatch, no Terraform, no Ansible.
- **No observability stack.** TUI (`ratatui`) + log files + a trivial `/metrics` endpoint on `127.0.0.1`. No Prometheus, no Grafana.

## Directory guide

- `PLAN.md` — full architecture, phased roadmap, testing strategy.
- `docs/phase-8-plan.md` — Stage-2 parent plan (§8.2 worker/verbs, §8.7 research loop, §12 stage table, §13 risks).
- `docs/phase-8h-design.md` + `docs/phase-8h-progress.md` — the ACTIVE phase. Design LOCKED; progress log carries the latest word + the H1 kickoff prompt.
- `docs/prompts/8h-kickoff.md` — frozen H0 authority prompt. `docs/prompts/ai-session.md` — semi-manual AI-session prompt (pinned by `claude-worker/tests/test_session_scripted.py` — do not move or drift it).
- `docs/wire-format.md` — PMLR v2 ring-slot/replay-log formats. `docs/migration.md` — format/schema migration log.
- `docs/risk-policy.md` — kill-switch and cap rules.
- `docs/architecture.md` (+ `.svg`, `phase-8-architecture.svg`) — one-page orientation.
- `docs/local-setup.md` — Mac toolchain setup. `docs/hot-path-latency.md` — standing latency audit (referenced by PLAN.md + bench).
- `docs/options-support-plan.md` — Phase 9+ candidate, P&L-gated; nothing lands in 8h.
- `docs/arch/` — **CLOSED history** (phase 1–6 plans, Stage-1 progress, 8f/8g design+progress, G1 work order). See its README. Never write there; read only for archaeology.
- `crates/core-*/` — OS-agnostic primitives (rings, time, config, alloc, io, net, parse, simd, crypto).
- `crates/core-crypto/` — handwritten SHA-256 / HMAC-SHA256 / base64 (RFC 4648); no external crypto stacks.
- `crates/core-io/` — PMLR replay log writer/reader + `PmlrCapture` (per-ingress §6.5 capture sink) + raw tap (`PMRT`).
- `crates/ingress-*/src/discovery.rs` — per-venue boot REST discovery (8e): instrument universes, tick/lot metadata, coverage audit.
- `crates/ingress-*/` — one per external source (polymarket, binance, okx, deribit, hyperliquid, rpc). `crates/ingress-ai/` — AI command plane (UDS+HMAC, ruleset validate/stage/commit).
- `crates/strategy-*/` — strategies implementing the `Strategy` trait; `strategy-vm` — the 8g ruleset VM; `strategy-set` — composed strategy set (mask-49, compose-if-configured).
- `crates/signer-eip712/` — audited-C-backed signer; do not replace with `ethers`.
- `crates/clob-dispatcher/` — persistent H/2 client; preallocated buffers.
- `crates/cli/` — the main binary (`multivenue-engine`: run / audit-replay / backtest-in-progress).
- `crates/tui/` — read-only dashboard; snapshot-page driven.
- `crates/bench/` — criterion + dhat; allocation assertions live here too. `#[global_allocator]` CountingAllocator is process-global — keep new tests of other crates OUT of the bench crate.
- Integration tests live per-crate under each crate's `tests/` directory. No workspace-level `tests/` is used.
- `fuzz/` — cargo-fuzz targets.
- `claude-worker/` — Python 3.14 worker: `serve` daemon + operator verbs (fetch/backtest/push/positions/stage-ruleset/commit-ruleset); Anthropic SDK constructed inside `serve` only; never in the hot path.
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
12. **Long commands through the RustRover MCP terminal.** `execute_terminal_command` executeInShell=true has a ≤45 s window — long runs: `nohup … > /tmp/log &` then poll. zsh eats bare `===` in echo.
13. **Modifying the worker's frozen surfaces.** `backtest.py` argv/schema-1 and the verb surface are FROZEN; the harness conforms to the worker. The 202 pytest baseline stays untouched-green.

## macOS session facts (hard-won)

- AF_UNIX `sun_path` length cap bites long socket paths.
- `SO_RCVTIMEO` returns EINVAL on peer-closed UDS.
- `std::thread::scope` panic hangs without a StopOnDrop guard.
- `sample <pid>` is the go-to for diagnosing hangs.
- RustRover MCP must attach (`get_project_modules`) against the main checkout FIRST; if it won't attach, stop.

## Preferred Claude models for tasks in this repo

- **Bulk artifact generation** (topic tagging): Haiku 4.5.
- **Reasoning** (rule parsing, news labeling): Sonnet 4.6.
- **Strategy proposals** (`claude-worker` serve strategist, ruleset drafts): Fable 5 (`MODEL_STRATEGIST = "claude-fable-5"`).
- **Hard work** (backtest review, architectural changes): Opus 4.6.

## When in doubt, read (in this order)

1. This file's CURRENT STATE section — where we are, what's next.
2. `docs/phase-8h-design.md` + latest `docs/phase-8h-progress.md` entry — the active phase's law.
3. `docs/phase-8-plan.md` — Stage-2 parent plan.
4. `PLAN.md` — everything architectural.
5. `docs/wire-format.md` — ring slot layouts, replay log format.
6. `docs/risk-policy.md` — kill-switch and cap rules.
