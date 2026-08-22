# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session should be able to start
from **this file + the current phase's design/progress docs** without
rereading the whole doc set. `PLAN.md` remains the architectural deep-dive.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that executes latency-arbitrage trades on Polymarket's CLOB. v1 runs locally on a MacBook Pro M4 using only free-tier external APIs. Claude (via the `claude-worker` Python process) acts as an offline strategy researcher — never in the hot path.

## CURRENT STATE (updated 2026-08-22 — keep this section current at every phase boundary)

- **Stage 1 (8a–8e) DONE**, G1 soak blessed 2026-08-15 (history: `docs/arch/phase-8-progress.md`).
- **Stage 2 CLOSED (this commit, 2026-08-22, under the operator-AMENDED §12):** 8f CLOSED (`7ca91be`) · 8g CLOSED (`39e6542`, G7) · **8h CLOSED** (H0 LOCKED · H1 `3ad40a9` · H2 `1ed6017` · H3 `76680db` · H4 `7bd0e42` · H5 `0e47429` · H6a `044c398` · H6b-SEMI = this commit). The autonomous research loop (harness + fill model + data_fetcher + strategist + §8.1 auto-promotion + §8.3 monitor/§8.5 rollback) is CODE-COMPLETE and E2E-proven; the closure record is the last entry of `docs/phase-8h-progress.md`.
- **What was demonstrated LIVE (semi-manual lane, rulings below):** H6a — Fable-5-authored `d8aea5f4…` gates-passed on the §8.5-sanctioned capA and promoted through the FROZEN verbs onto the live engine (vm_rows 2, epoch 1, vm_fires 1, paper). H6b-SEMI — second Fable-5 ruleset `92feb9ea…` (levels 0.45/0.58, $8 caps) gates-PASS first try (OOS +$60.30 / 63 trades / 2 days), promoted (Stage seq=20 / Commit seq=22); then the manual §8.5-shaped ROLLBACK: Disable-5 (mask 49→17) → restage/commit of the prior from its bound paths (staged 2 / committed 3) → operator-act re-enable + re-commit (mask →49, **epoch →2, prior live, vm_fires 0→1**); audit-replay renders the whole chain (seqs 19–32, gaps 0) with ZERO integrity violations. **Standing finding:** Commit is mask-gated at the vm member (8g gating pin) ⇒ the post-rollback operator procedure is **enable + re-commit** (staged prior survives the disabled window; applies without restaging).
- **OPERATOR RULINGS (2026-08-22, standing):** `ANTHROPIC_API_KEY` will NOT be provisioned until Stage 3 — NO `serve`, NO Anthropic API calls, EVERYTHING semi-manual via the ai-session §4 verbs. §12 was AMENDED accordingly: the §8.1 auto-promotion + §8.3 monitor-rollback LIVE proofs are **deferred to the Stage-3 ENTRY GATE** (`docs/mvp-completion-plan.md` §7: key → one keyed serve cycle + one monitor rollback observed BEFORE any executor work). `AI_INGRESS_HMAC_KEY` is PERMANENT in `.env` (engine dotenvy + worker BaseConfig read it; worker verbs need `.env` sourced into the shell — see the H6b wrapper pattern in the closure entry; NEVER read/print `.env`).
- **M1 CLOSED (2026-08-22, commits `c477bb9`+`bad65d6`+close):** universe config file (`~/multivenue/universe.toml`, TOML-subset parser in core-config, M1 SymbolId law: PM[0]→42/BN-spot[0]→7 anchors + namespaced ordinals, USDM base 512) · PM multi-market (one connection, N-id subscribe) · Binance multi-connection lane (one thread, N single-stream conns, spot+`BINANCE_FUT_WS_HOST`) · BN exchangeInfo discovery audit · worker universe-file seeding at the fetch seam (`CLAUDE_WORKER_UNIVERSE_FILE`). Exit proven live: ONE zero-flag boot ran PM 4 tokens + BN 3 conns + OKX/Deribit/HL; audit integrity zero all five venues; map unresolved=0; nextest 1139 / alloc 36 / pytest 363 / fuzz +2 targets clean. Legacy flag boots stay byte-identical. **Operational: the up/down dailies expire 16:00Z — refresh `universe.toml` via the Gamma lane before each boot (M3 automates).** Log: `docs/mvp-progress.md`.
- **IN PROGRESS (operator go 2026-08-22): M2 ∥ M3 in PARALLEL sessions.** M2 = options ladder (Deribit → OKX → mark/IV channel → Binance eapi; §9.8). M3 = data ops (capture-catalog + launchd always-on + retention + candles.db per §9.4–§9.7). Per-phase logs: `docs/m2-progress.md` / `docs/m3-progress.md` (create on first entry); kickoff prompts at the end of `docs/mvp-progress.md`; the **Parallel session protocol below is LAW for both sessions**. Then M4…M6. Plan §9 (PMLR canonical, candles.db keyed venue+descriptor, fetch 1m/1h/1d + derive, backtest reads PMLR only) is BINDING.
- **HARD OPERATOR REQUIREMENT (standing): Stage-2 completion has been notified (H6b-SEMI close). Do NOT start ANY Stage-3 work (executor, risk/8i+, venue dispatchers, live ramp — code, plans, or designs) without the operator's explicit confirmation; the Stage-3 ENTRY GATE is `docs/mvp-completion-plan.md` §7 and it is the operator's to open. M1 also starts only on explicit operator go.**
- **Authority chain now:** `docs/mvp-completion-plan.md` (M-phases; §9 binding) → the M-phase progress log's latest entry → this file. For 8h archaeology: `docs/prompts/8h-kickoff.md` → `docs/phase-8h-design.md` (LOCKED) → `docs/phase-8h-progress.md` (closure entry last).
- **Frozen contract (unchanged):** `claude-worker/src/claude_worker/backtest.py` — argv `multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`, schema-1 JSON on stdout, GateThresholds numbers; the frozen 202 worker tests pin it. **The harness conforms to the worker, never vice versa.** `backtest.py` and `cli.py` byte-untouched through H6b-SEMI (five sessions).
- **Baselines at Stage-2 close (H6b-SEMI, all re-verified this session):** workspace nextest 1081/1081 (+1 ignored fixture-regen), release alloc 36/36 0 B/op (`--test-threads=1`, corrected clean-guard), worker pytest 354 (202 frozen + 2 real-harness + 43 fetchers + 79 H4 + 28 H5; 354 is the stay-green), fuzz `ruleset_json` 72.3M clean (untouched — no new untrusted-bytes parser on either side).
- **Push anomaly KNOWN** (origin/main divergence observed H0): record, never act.
- **Git discipline:** NO push, NO rebase, NO history rewrite, NO new branches, NO git ops without operator ask. Do NOT touch `.env`.
- If context runs short: write interim state + exact resume point + relaunch prompt to the active M-phase progress doc, then tell the operator.

## Parallel M2/M3 session protocol (LAW while both phases run, operator go 2026-08-22)

Two Claude sessions share this ONE checkout. Violating this section corrupts the other lane's work.

- **Git staging is explicit-path ONLY.** `git add <your owned paths>` — NEVER `git add -A`/`-u`. Check `git status` first; the other lane's dirty files are NOT yours to stage, commit, or clean. Commit messages prefixed `M2:` / `M3:`. Commits remain operator-authorized (checkpoint pattern).
- **Ownership.** M2 owns: `crates/ingress-deribit`, `crates/ingress-okx`, `crates/ingress-binance` (eapi), their tests + fuzz targets, `docs/wire-format.md` + `docs/migration.md` (M2.3), `docs/m2-progress.md`. M3 owns: the cli capture-catalog module + bin arm, `claude-worker` (candles.db, refresh automation), `docs/local-setup.md` runbook additions, `~/Library/LaunchAgents` plist, `docs/m3-progress.md`. SHARED — small additive edits, sequential commits, note in your log: `crates/cli` (bin + paper.rs), `crates/core-config`, `crates/core-io`, `universe.toml.example`, `.env.example`, `docs/mvp-progress.md`.
- **ONE ENGINE EVER** (9191 + ai.sock are singletons). Once M3 installs the launchd instance, it is THE standing engine; any smoke boot first `pgrep -f multivenue-engine`, stops the standing instance (`launchctl`), and restarts it after. G0 relink law applies per boot; a relink under a running engine takes effect at its next restart.
- **Worker verbs globally serialized** (one SQLite seq namespace across sessions): `pgrep -f claude-worker` before any verb; never overlap fetch/push/stage/commit with the other session.
- **Cargo shares one target dir** — concurrent builds/tests BLOCK on the file lock. Wait; never kill the other session's build. Long-runners: nohup with per-lane prefixes (`/tmp/m2-*`, `/tmp/m3-*`).
- **Sequencing pin:** M2.3 (mark/IV wire-format migration) starts only AFTER M3's capture-catalog first commit lands (M2's ladder order makes this natural). Whichever side lands second extends the other (the catalog gains the new channel's row).

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
# M1: the universe comes from ~/multivenue/universe.toml (zero flags);
# legacy fallback: no config file ⇒ --polymarket-asset-id is REQUIRED
# (boot refuses venue-blind). For AI-cmd work use the set path:
# --strategy all (bare latency-arb can't express AI toggles).
cargo build --release -p cli
cargo run --release -p cli -- run --paper --strategy all
# legacy (no universe.toml):
cargo run --release -p cli -- run --paper --polymarket-asset-id <TOKEN_ID>

# audit a capture run (every run writes PMLR capture to
# <MULTIVENUE_LOG_DIR>/run-<epoch_ns>/ — per-venue ticks/events/signals
# + optional --raw-tap payload tap)
cargo run --release -p cli -- audit-replay --dir ~/multivenue/logs/run-<ns>
```

## Universe config — adding markets/instruments (M1 runbook)

- The boot universe lives in `~/multivenue/universe.toml` (TOML subset; grammar documented in `universe.toml.example` + `core-config::universe`; `--universe <path>` overrides; file absent ⇒ legacy flag boot). Read ONCE at boot — **changes apply on restart** (brief capture gap, one new run dir; M3 automates the cadence).
- **Polymarket** (crypto up/down binaries only — M1-R1): resolve `clobTokenIds` via the Gamma lane (`https://gamma-api.polymarket.com/markets?slug=<slug>`), append `"<yes>:<no>"` (pair) or `"<token>"` (single leg) to `[polymarket] markets`. Caps: 64 market entries / 128 tokens. Latency-arb wiring: `[pairs] map = ["P:B"]` (market index × `binance.spot` index, 0-based file order).
- **Append, never reorder.** SymbolIds are file-order ordinals (PM token[0]→42, BN spot[0]→7, everything else `make_symbol_id(venue, ordinal)`; USDM base 512). Reordering a still-listed instrument re-syms it next boot, and the worker map keeps the OLD sym by design (conflict reported on every fetch until the stale `market-map.json` entry is pruned). Wholesale replacement is clean — the daily up/down refresh (markets expire 16:00Z) drops old ids from the observed universe (dead map names are harmless) and adds fresh ones.
- **Binance**: append to `spot` / `usdm` (lowercase stream symbols) — the multi-connection lane grows one conn; the exchangeInfo audit validates at boot. **OKX/Deribit/HL**: append to their instrument/coin lists; 8e discovery validates.
- After restart, run `claude-worker fetch` once — the `CLAUDE_WORKER_UNIVERSE_FILE` seam seeds map names (§9.4 descriptors), Gamma meta and YES/NO pairs; `unresolved=0` in the fetch output is the done-tell.
- One-off boots without editing the file: the per-venue CLI flags still override (`--polymarket-asset-id X` etc.).

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
