# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session should be able to start
from **this file + the current phase's design/progress docs** without
rereading the whole doc set. `PLAN.md` remains the architectural deep-dive.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that executes latency-arbitrage trades on Polymarket's CLOB. v1 runs locally on a MacBook Pro M4 using only free-tier external APIs. Claude (via the `claude-worker` Python process) acts as an offline strategy researcher — never in the hot path.

## CURRENT STATE (updated 2026-09-02 — MVP COMPLETE; keep this section current at every phase boundary)

- **ALL STAGES/PHASES THROUGH M6 ARE CLOSED. MVP COMPLETE (operator ruling 2026-09-02).** Stage 1 (8a–8e, G1 blessed) · Stage 2 (8f/8g/8h, H6b-SEMI) · Stage-2 finish WS0–WS13 (gates + live phase green, §5.4 rustls-backpressure root-cause fixed + live-proven) · M1–M4 · M5 (closed at the 2026-09-02 bootout) · VM2 ruleset-VM v2 V0–V9 · M6 (closed by operator ruling at the VM2 boundary; close entry = last of `docs/arch/mvp-progress.md`). **The 2026-09-02 docs pass archived every closed plan/log to `docs/arch/` (see its README index): `stage2-finish-plan.md`, `mvp-completion-plan.md`, `vm2-plan.md`, `m5-runbook-notes.md`, `binance-stocks-plan.md`; second wave on operator order: `license-audit-2026-08-27.md` + `research-tools-exclusion-plan.md` (both REMAIN standing authorities from the archive — the Makefile gates cite them there), `architecture.md`+`.svg`, `options-support-plan.md`.** For any closed-phase archaeology start at `docs/arch/README.md`.
- **OPEN (2026-09-03, operator go): `docs/venue-time-capture-plan.md` VT0–VT6** — Tick v3 venue time + staleness gate (capture is stale-blind: Binance feed 8.9 % of messages > 500 ms stale; see `docs/venue-latency.md`). **VT0 + VT1 landed 2026-09-03** (Tick v3 `flags`@49 / `venue_time_ms`@56, `Tick::new_stamped`, PMLR VERSION 3, cli acceptance `MIN_PMLR_VERSION..=VERSION`, `pmlr.py` v3 + `ticks_v3.pmlr` fixture); **VT2 (ingress venue-time extraction + bn-spot sentinel) is next.** The standing engine still writes v2 until the operator's next authorized relink (G0). Not Stage-3 work. Until VT4 lands, every backtest/audit-pnl on a v2 root is stale-blind and cross-venue results are upper bounds. Operator rulings D1–D4 (IoC model pre-Stage-3, fee tier 2/5, PROTECT_DAYS 5, 8-major 15 s/1 m universe) are recorded in the research vault's merged ICDP×VT plan.
- **THE ONLY STAGE-3 GATE LEFT: the Stage-3 ENTRY GATE — `docs/arch/mvp-completion-plan.md` §7, FORWARD-BINDING from the archive (as is its §9 data-pipeline law).** It is the operator's to open: `ANTHROPIC_API_KEY` provisioned → one keyed Fable-5 `serve` cycle with §8.1 auto-promotion observed live + one §8.3 monitor-triggered §8.5 rollback observed live — BEFORE any executor/risk/dispatcher/live-ramp work. **Do NOT start ANY Stage-3 work (code, plans, or designs) without the operator's explicit confirmation.** Until then: NO `serve`, NO Anthropic API calls; everything semi-manual via the ai-session §4 verbs (`docs/prompts/ai-session.md`, pinned).
- **LIVE OPERATION (engine-only since the 2026-09-02 bootout; AI-ONLY MASK since the same day's operator ruling):** the engine boots `--strategy ai` = **mask 48 (ai-exec + vm) — ALL Rust-coded strategies (latency-arb) DISABLED at boot** (wrapper `scripts/engine-wrapper.sh`; boot-log tell: `composed mask=48 latency_arb=false`). launchd fleet = `com.multivenue.engine` + caffeinate + daily-restart (T2 00:00/08:30/16:05Z) + hourly candles+iv + retention (**PROTECT_DAYS 5** via `~/multivenue/retention.conf`). `com.multivenue.carry`/`com.multivenue.xv` crons are DELETED. The VM carries xv (`bfbc5349…`, okx-only 3.0/1.0 bps $3,000/leg; the hl pair ruled dead). **Carry is DARK** — revisit shape: regate merged candidate `b9883c1a…` (carry legs $2,750 under the Rule-7 leg-counted $100k table cap) on a healthy root (Aug-29→31), then stage/commit; §6 stage law refuses failing reports, NO override. Paper mode everywhere; PM ≤6 tokens (M1 cap: token 7 collides with anchor id 7); universe dailies expire 16:00Z (T2 refreshes). **After ANY restart verify `vm_rows_active 1`** (the #7b recommit now retries the stale-sock race — fixed + live-proven 2026-09-02). Restart-lane revive lever: `echo 19700101 > ~/multivenue/state/last-restart-utc-0000`.
- **STAY-GREENS (2026-09-02, V9 battery): nextest 1420 · release alloc 39/39 0 B/op · worker pytest 600 (frozen 202 inside) · `make lint` green · `make license-check` green.** Fuzz = VM2-V4's standing ≥300 s record. Two known launchd-context test flakes (isolation-disproven, recorded in the archived vm2-plan §8 close entry): `ai_exec_on_ai_is_zero_alloc` (debug profile) and `scrape_hammer_all_succeed_without_conn_errors` — rerun in isolation before believing a red.
- **OPS DEBTS (standing, non-gating):** (a) **disk headroom** — the Data volume ran 100 % full Sep-2 (capture ENOSPC-wedged all lanes; writers do NOT retry after ENOSPC — engine restart is the recovery); ~360 GB non-project data is the operator's lever; (b) **the 00:20Z nightly pnl timer is DEAD since Aug-23** (one report ever; needs a recommit-style lane revival); (c) whole-root audit-pnl/backtest OOM at ~27–44 GB roots — bounded symlink roots are the working shape (17 GB ≈ 8 GB RSS), streaming mode is the fix-shape; (d) BN markPrice/eapi-WS venue-side unreachable from this network — activation = `BINANCE_EAPI_WS_HOST` in `.env` + restart, no code change.
- **STANDING OPERATOR RULINGS + LAWS (survive all archival):** the frozen worker contract — `claude-worker/src/claude_worker/backtest.py` argv `multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`, schema-1 JSON, GateThresholds (`min_trading_days` 1 since the MVP-tempo amendment); the harness conforms to the worker, never vice versa; the frozen 202 pytest pin. `AI_INGRESS_HMAC_KEY` permanent in `.env` (worker shells need `set -a; source .env; set +a` + the release dir on PATH — the H6b wrapper pattern; NEVER read/print `.env`). Post-rollback procedure = enable + re-commit (Commit is mask-gated at the vm member). Detached/long runs on the Mac: `launchctl submit` jobs — **NOT one-shots: launchd relaunches the job every time it exits until `launchctl remove <label>`, so the wrapper's LAST line must remove its own label** (two overlapping relaunches corrupted a research root on 2026-09-02); nohup children of MCP terminals die with the window; `python -m claude_worker.cli` is a silent no-op — use the `claude-worker` console script.
- **LICENCE PASS standing (2026-08-27, authority `docs/arch/license-audit-2026-08-27.md`):** per-file SPDX enforced by `make license-check`; `make license-deps` on any dependency change; `THIRD-PARTY-NOTICES.md` committed. `make lint` is green as of 2026-09-02 and stays a gate.
- **Pushes are the OPERATOR's, done manually.** `origin/main` advancing without any session having pushed is NORMAL. (The old "push anomaly KNOWN / origin-main divergence" note was a misreading — operator-corrected 2026-08-27. `origin/main`'s reflog is full of `update by push` because the operator pushes by hand.) Sessions still NEVER push: that rule is unchanged.
- **Git discipline:** NO push, NO rebase, NO history rewrite, NO new branches, NO git ops without operator ask. Do NOT touch `.env`.
- If context runs short: write interim state + exact resume point + relaunch prompt to the active work's log doc (post-MVP: a dated session-notes doc in `docs/`), then tell the operator.

## Parallel M2/M3 session protocol (BOTH CLOSED — C6 closed 2026-08-29; the M2/M3 OWNERSHIP SPLIT IS DISSOLVED. The one-engine law, the serialized-worker-verbs law, and explicit-path staging remain STANDING LAWS for every session; the rest of this section is historical record)

Two Claude sessions shared this ONE checkout during M2/M3. (Standing inheritance for every session since: the one-engine law, the worker serialization law, and explicit-path staging.)

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

# licence gates — run license-check before every commit (offline, ~1 s:
# SPDX coverage on all tracked .rs/.py/.sh, claude-worker LICENSE/NOTICE
# drift, the fuzz manifest's non-inheritable license key).
make license-check
# only when a dependency changed (needs cargo-deny + cargo-about installed);
# regenerates THIRD-PARTY-NOTICES.md, which is COMMITTED, not release-time.
make license-deps

# start the engine locally (paper mode)
# G0 law: test gates build rlibs/test bins but NEVER relink the release
# binary — ALWAYS `cargo build --release -p cli` before any live boot.
# M1: the universe comes from ~/multivenue/universe.toml (zero flags);
# legacy fallback: no config file ⇒ --polymarket-asset-id is REQUIRED
# (boot refuses venue-blind). DEPLOYED FLAG since 2026-09-02:
# --strategy ai (mask 48 = ai-exec + vm; Rust-coded strategies
# disabled by operator ruling). --strategy all (mask 49) re-adds
# latency-arb; bare latency-arb can't express AI toggles.
cargo build --release -p cli
cargo run --release -p cli -- run --paper --strategy ai
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

### Licensing — EVERY new file (enforced by `make license-check`)
- **Every new `.rs`, `.py` and `.sh` file MUST carry the two-line SPDX record as its first lines.** No exceptions — tests, fuzz targets, one-off scripts, throwaway harnesses. Adding a file without it fails `make license-check`, which is a gate, not advice.
  ```rust
  // SPDX-License-Identifier: Apache-2.0
  // Copyright 2026 Anton (darkcite)
  ```
  ```python
  # SPDX-License-Identifier: Apache-2.0
  # Copyright 2026 Anton (darkcite)
  ```
- **Placement is exact:** Rust — above the `//!` inner-doc block and above any `#![...]` inner attribute (comments may legally precede both). Python — above the module docstring (`__doc__` still resolves). Shell — **after** the shebang, never before it.
- **Every new crate's `Cargo.toml` gets `license.workspace = true`.** The one manifest that cannot inherit is `fuzz/Cargo.toml` (workspace-`exclude`d by cargo-fuzz convention) — it carries a literal `license = "Apache-2.0"`; keep the two in sync.
- **Adding/changing/removing a dependency changes the license surface of the shipped binary.** Run `make license-deps` (cargo-deny + cargo-about) and commit the regenerated `THIRD-PARTY-NOTICES.md` with that change. If `cargo deny check licenses` rejects a new license, that is a deliberate decision in `deny.toml` — read what the license obliges before appending a line, never rubber-stamp it.
- **RESEARCH-IN-GIT LAW (operator ruling 2026-09-02, absolute): researched data, strategy research, backtest/P&L reports, and ANY docs about researched strategies NEVER enter git.** They live in the git-excluded research vaults: `docs/research/` for engine-generated output (reports, trade lists, research findings); the external-material vault per `docs/arch/license-audit-2026-08-27.md` G8 for third-party strategy material. Both are gitignored; `make license-check` refuses any tracked file under `docs/research/`, and the G8 naming gate already polices the external vault. No `git add -f`, no exceptions — a plan/progress log may record decisions and laws, but the research substance itself stays out.
- **Never vendor third-party source into this tree.** Deps are unmodified crates.io/PyPI packages, which is what makes the `NOTICE` claim true. If material of any provenance must land in-tree, record it in `NOTICE` first.
- **No binary leaves the build host without `LICENSE` + `NOTICE` + `THIRD-PARTY-NOTICES.md` beside it.** Stage-3 / Phase-7 gate.
- **Repo-wide file rewrites: verify `git diff --summary` is empty of mode changes.** A `> tmp && mv` pass creates new inodes at the umask and silently strips exec bits — it did exactly that to the five `scripts/*.sh` launchd scripts on 2026-08-27. `--numstat` does NOT show mode changes and will not catch it.
- Authority: `docs/arch/license-audit-2026-08-27.md`. Contributor-facing copy: `CONTRIBUTING.md`.

### Secrets
- **`.env` file only.** No macOS Keychain, no AWS KMS, no Vault, no Secrets Manager.
- **`.env` is `chmod 600` and in `.gitignore`. `.env.example` is committed.**
- **Signing key loaded into an `mlock`'d page, zeroized on drop.**

### Deployment
- **No cloud services, at any phase.** Even Phase 7 EC2 is a plain Linux VM — no KMS, no SSM, no CloudWatch, no Terraform, no Ansible.
- **No observability stack.** TUI (`ratatui`) + log files + a trivial `/metrics` endpoint on `127.0.0.1`. No Prometheus, no Grafana.
- **Venue latency is measured, never assumed — per deployment AND per location.** The harness's activation-Δ table (`crates/cli/src/backtest.rs` `ModelParams::default()`) is a measurement of the current host + network; on any new box/region/ISP/VPN run `python -m claude_worker.latency_probe` and re-derive it per `docs/venue-latency.md` before trusting any backtest/audit-pnl number there. Receive-time lead-lag between venues on this host is dominated by feed delivery (Binance p90 ≈ 1.3 s tail, 2026-09-03) — check cross-venue signals in venue time first.

## Directory guide

- `PLAN.md` — full architecture, phased roadmap, testing strategy.
- `docs/arch/mvp-completion-plan.md` — ARCHIVED at MVP close, but **§7 (Stage-3 ENTRY GATE) and §9 (data-pipeline law) remain FORWARD-BINDING** from there.
- `docs/research-universe.md` — the research catalog. `docs/ai-strategy-pipeline.md` (+ `.svg`) — the pipeline explainer.
- OPEN research items (scalping plan DRAFT awaiting the operator's §13 ruling; ICDP research awaiting operator review) live OUTSIDE git in the git-excluded research vaults — see the research-in-git law below.
- `docs/prompts/ai-session.md` — semi-manual AI-session prompt (pinned by `claude-worker/tests/test_session_scripted.py` — do not move or drift it).
- `docs/wire-format.md` — PMLR v2 ring-slot/replay-log formats. `docs/migration.md` — format/schema migration log.
- `docs/risk-policy.md` — kill-switch and cap rules.
- `docs/arch/license-audit-2026-08-27.md` — Apache-2.0 compliance audit + application record; the authority for the "Licensing" hard rules. `CONTRIBUTING.md` — the contributor-facing copy. `deny.toml` / `about.toml` / `about.hbs` — the dependency licence gate.
- `docs/local-setup.md` — Mac toolchain setup. `docs/hot-path-latency.md` — standing latency audit (referenced by PLAN.md + bench).
- `docs/arch/` — **CLOSED history** (phase 1–6 plans, Stage-1 progress, 8f/8g/8h design+progress, Stage-2 parent plan, M1/M2/M4 logs, the Aug-27/28 outage+remediation docs, WS10 design, archived prompts). See its README index. Never write there; read only for archaeology.
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
- `claude-worker/` — Python 3.14 worker: `serve` daemon + operator verbs (fetch/backtest/push/positions/stage-ruleset/commit-ruleset); Anthropic SDK constructed inside `serve` only; never in the hot path. **Research one-shots (`claude-worker/tools_*.py`) are deliberately git-excluded** — `tools_` is a reserved prefix there; findings go to `docs/research/` (git-excluded), outputs are the sha256-named artifacts in `~/multivenue/artifacts/rulesets`, and anything that earns a caller moves into `src/claude_worker/`. Authority: `docs/arch/research-tools-exclusion-plan.md`.
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
14. **Creating a new `.rs`/`.py`/`.sh` without the SPDX header, or a new crate without `license.workspace = true`.** See "Licensing" above. `make license-check` fails on it, and a header-less file lifted out of the repo carries no license signal at all — §4(c) obliges downstream to retain notices that then do not exist. Write the two lines when you create the file, not later.
15. **Adding a dependency without re-running `make license-deps`.** A new crate changes what the shipped binary must attribute. `THIRD-PARTY-NOTICES.md` is committed, not generated at release time, precisely so it cannot be missing when a binary ships.
16b. **Committing ANY research material — backtest records, P&L reports, strategy research docs, trade lists.** The research-in-git law (Licensing section) is absolute: the substance lives in the git-excluded vaults (`docs/research/`, or the G8 external vault), never in git — the 2026-09-02 history purge of two tracked backtest records is the precedent.
16. **Committing a `claude-worker/tools_*.py` research one-shot — or naming a specific one in a permanent doc.** The class is git-excluded by policy, and `make license-check` fails on BOTH: a tracked one-shot (`git add -f` is the only way one comes back), and any tracked file that names a concrete one-shot, or the external research corpus, outside that class's owning authority doc. Naming the class pattern to state the law is fine; naming a file you do not ship is how a permanent doc comes to point at nothing — cite the owner doc instead (`docs/arch/research-tools-exclusion-plan.md`; external corpus → `docs/arch/license-audit-2026-08-27.md` G8). If a one-shot genuinely needs tracking it becomes a module in `src/claude_worker/` with tests, not a forced add.

## macOS session facts (hard-won)

- AF_UNIX `sun_path` length cap bites long socket paths.
- `SO_RCVTIMEO` returns EINVAL on peer-closed UDS.
- `std::thread::scope` panic hangs without a StopOnDrop guard.
- `sample <pid>` is the go-to for diagnosing hangs.
- RustRover MCP must attach (`get_project_modules`) against the main checkout FIRST; if it won't attach, stop.
- A REUSED RustRover MCP terminal can return STALE/MIXED output from earlier commands (burned two M2.3 investigation loops on phantom lines that were in no file). Evidence-critical reads: `reuseExistingTerminalWindow=false`; files are the only ground truth.
- The RustRover MCP terminal window is ~45 s REGARDLESS of the requested timeout — long runs: `nohup … > /tmp/<lane>-*.log 2>&1 &` then poll the log file.
- Concurrent pytest runs across sessions COLLIDE (shared socket/tmp fixtures; twice-observed in M2). Treat pytest like a serialized worker verb: `pgrep -f pytest` first, never overlap lanes.
- `tests/test_recommit.py::test_recommit_restages_and_recommits_active_row` is INTERMITTENTLY FLAKY under the full suite — observed once on 2026-08-30 as `len(fake_uds.frames) == 0` (no frame reached the `FakeUdsServer`), i.e. the UDS fixture, not the assertion under test. Characterized immediately: 6/6 green in isolation, 598/598 green on the two full reruns that followed. Same family as the AF_UNIX/`SO_RCVTIMEO` facts above. Re-run before believing it; a single red here is not a regression signal.
- `pkill -f <pattern>` can match a POLLING shell that quotes the same string (cosmetic exit 143) — bracket one letter of the pattern (`multivenue-engin[e]`).

## Preferred Claude models for tasks in this repo

- **Bulk artifact generation** (topic tagging): Haiku 4.5.
- **Reasoning** (rule parsing, news labeling): Sonnet 4.6.
- **Strategy proposals** (`claude-worker` serve strategist, ruleset drafts): Fable 5 (`MODEL_STRATEGIST = "claude-fable-5"`).
- **Hard work** (backtest review, architectural changes): Opus 4.6.

## When in doubt, read (in this order)

1. This file's CURRENT STATE section — where we are, what's next.
2. `docs/arch/mvp-completion-plan.md` §7 + §9 — the Stage-3 entry gate and the binding data law (forward-binding from the archive).
3. `PLAN.md` — everything architectural.
4. `docs/wire-format.md` — ring slot layouts, replay log format.
5. `docs/risk-policy.md` — kill-switch and cap rules.
6. `docs/arch/README.md` — the index to ALL closed plans/logs (everything through M6/VM2).
