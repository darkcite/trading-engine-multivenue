# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session starts from **this file +
the current lane's docs** and nothing else. It carries the CURRENT state and
the STANDING laws only — history lives in `docs/arch/` (see "Where to look").

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that
executes systematic strategies across a multivenue universe — Binance
spot/USDM, OKX, Deribit, Hyperliquid (incl. HIP-4 outcome markets), Bybit,
Polymarket CLOB, Polygon RPC, plus a boot-selected options ladder. Strategies
are composed at boot from an 8-slot set and may trade **any subset** of that
universe; **Polymarket is one venue among several, not the target**. v1 runs
on a MacBook Pro M4 on free-tier APIs. Claude (via the `claude-worker` Python
process, and in-session) is an **offline strategy researcher** — never in the
hot path.

Slots (`crates/strategy-set`, one enable bit each; `all` = `BUILT_MASK` 127):
0 latency-arb (the original PM strategy, OFF in every wrapper mask) · 1 vrp ·
2 xsd · 3 bin15 · 4 ai-exec (AI door 1, intents) · 5 ruleset VM (AI door 2,
tables) · 6 icdp · 7 open. The engine boots the mask named in
`~/multivenue/strategy.conf` through `scripts/engine-wrapper.sh` (allow-list
in the script; `ai` = 48 is the floor every name includes).

## Where to look — and where not to

- **Current docs (read as needed):** `PLAN.md` (architecture deep-dive),
  `docs/risk-policy.md` (caps, kill switches, LAWS E-1..E-9, the exec lane's
  record), `docs/wire-format.md`, `docs/migration.md`, `docs/local-setup.md`,
  `docs/venue-latency.md`, `docs/hot-path-latency.md`,
  `docs/research-universe.md`, `docs/ai-strategy-pipeline.md` + the three
  sheets `docs/{ai-strategy-pipeline,phase-8-architecture-v2,engine-memory-cpu}.svg`,
  `docs/prompts/ai-session.md` (pinned by a worker test — never move it),
  `exec.toml.example` and the other `*.toml.example` files (each is its
  parser's contract).
- **`docs/arch/` is HISTORY and `docs/research/` is the git-excluded research
  vault. Read either ONLY when the operator explicitly asks for it** (a
  closed phase, a ruling's provenance, a research finding). Never as part of
  orientation, never to "check for context". The few archived documents that
  are still standing authorities are named where they bind (the Licensing
  rules, the S3 archive's S-LAWs, the ≤ 2 h window law) — cite them from
  there; do not go reading around them.
- Working notes for a lane: a dated log in the vault (`docs/research/…`) —
  never in git, never in this file.

## CURRENT STATE (2026-09-19)

- **MVP complete; every pre-execution lane closed** (Stage 1–2, M1–M6, VM2,
  VT, regime + dashboard, S3 archive, ICDP, XSD, VRP, BIN15). Their closing
  records are in `docs/arch/` and the vault; nothing there is open work.
- **Live operation:** one launchd engine (`com.multivenue.engine`) in PAPER,
  restarted at 00:10/08:30/16:05Z by `daily-restart`, plus `caffeinate`,
  hourly `candles`, 5-min `regime`, nightly `retention` (PROTECT_DAYS 1,
  every delete gated on S3 `verify`), `archive` (04:30 local) and the
  read-only `dashboard`. The live mask is whatever `strategy.conf` names;
  slots 1–3 and 6 have all run in paper. After ANY restart verify
  `vm_rows_active ≥ 1` on `/state`.
- **Real-execution lane E1–E6 LANDED, reviewed and re-tested in E7
  (2026-09-19).** `crates/exec-router` (per-slot `ExecMode` on
  `Order.strategy_id`, `RoutedDispatcher`, the E6 risk gate: per-order /
  open-orders / day / instance caps, the venue-fill ledger, six sticky halts
  incl. recon-STALE, `exec.HALT`) + `crates/exec-hyperliquid` (msgpack +
  EIP-712 `Agent` signing pinned by 25 SDK vectors, mio+rustls `/exchange`
  arm with the request body rendered in place, `userFills` WS pumped from
  `on_idle` on the engine thread, reconciliation, address-budget governor,
  LAW E-8 sweeps). Armed ONLY by the two-switch interlock `--exec
  ~/multivenue/exec.toml --arm-live <slot>`; `LIVE_ARM_VENUES = [Hyperliquid]`;
  a market-data host and an exchange host on different networks refuse the
  boot. `halt_on_recon_stale_ms` is REQUIRED on every live slot. Record:
  `docs/risk-policy.md` "E6" + "E7".
- **LIVE ON MAINNET since 2026-09-19 13:04Z — E7 R0, slot 3 (bin15)
  armed** (operator ruling; testnet has no 15-minute family, so R0 ran
  there only as the exec battery, vault doc 23). The launchd engine
  boots `STRATEGY=ai+vrp+xsd+bin15` with `EXEC_TOML`/`ARM_LIVE=3` from
  `~/multivenue/strategy.conf`; `~/multivenue/exec.toml` (order $2, open
  2, day $8, instance $2) and `bin15.toml` (entry $2 at ask ≥ 0.70,
  cap_instance $2, cap_day $8, maker off) size a 9.8 USDC spot bankroll.
  The four `Scope::Live` keys live in the REPO `.env`; the mainnet agent
  approval EXPIRES 2026-10-19. The venue's HIP-4 minimum order is
  **1 USDC** (measured; `GRID_MIN_NOTIONAL_1E6`) and its fee is charged
  on SETTLEMENT (14 bps of the payout, 13.44 with the referral discount,
  measured 13:15Z; the trade is free) — `fees.toml`'s
  `prediction_settle` key, `--fee-bps hl.prediction.settle`. Record + the first hour's two findings (E7-F1 budget seed,
  E7-F2 IoC-miss classification): `docs/risk-policy.md` "E7 — MAINNET
  R0". Bars R0–R3: risk-policy "E7". Every restart of an armed engine
  passes `scripts/exec-smoke.sh` first (daily-restart does it itself).
- **Gates at HEAD:** nextest 2585 (1 skipped) · alloc 62/62 at 0 B/op ·
  clippy clean · `make license-check` OK · `make copy-audit` new=0 ·
  worker pytest 1153 (3 skipped) · fuzz `hl_*` 3 × 300 s clean. Known
  isolation-disproven flakes: `ai_exec_on_ai_is_zero_alloc` (debug profile),
  `scrape_hammer_all_succeed_without_conn_errors`,
  `hl_userws_loopback::a_frame_larger_than_the_buffer_is_refused_not_grown`
  (red under parallel load, green alone), the worker's UDS-fixture
  family (`test_recommit…`, `test_commit_ruleset_happy_by_hash_then_by_file`)
  — rerun in isolation before believing a red. `make py-lint` (ruff) is
  RED at HEAD and has been for weeks; `make lint` means clippy.
- **Open operator items (non-gating):** S3 bucket versioning +
  `AbortIncompleteMultipartUpload` not set; the `s3.env` credential pair was
  once pasted in chat — rotate; disk headroom on the Data volume is the
  operator's lever (writers do not retry after ENOSPC — restart is the
  recovery); whole-root `audit-pnl`/`backtest` OOMs above ~27 GB — use
  bounded ≤ 2 h window roots; BN eapi-WS is unreachable from this network
  (`BINANCE_EAPI_WS_HOST` in `.env` + restart activates it); `regime.toml
  [labels] require = 1` is NOT flipped live; `.claude/settings.json` still
  names `claude-opus-4-6` as the session model (the three review agents are
  pinned to `claude-opus-5`).

## Standing operator laws (survive every archival)

- **Git:** NO push, NO rebase, NO history rewrite, NO branches, NO git
  operation without the operator's ask. Pushes are the operator's, by hand
  (`origin/main` moving without a session push is normal). Staging is
  **explicit-path only** — never `git add -A`/`-u`; another lane's dirty
  file is not yours to stage. Never `cargo fmt --all` (HEAD was never
  fmt-clean). Git write-ops run ON THE MAC (RustRover terminal), never
  through the Cowork mount (it leaves stale `.git` locks).
- **Secrets:** `.env` only, `chmod 600`, git-ignored, never read, printed or
  edited by a session. `AI_INGRESS_HMAC_KEY` lives there permanently; worker
  shells need `set -a; source .env; set +a` + the release dir on PATH.
- **ONE ENGINE EVER** (9191 + `ai.sock` are singletons): the launchd instance
  IS the engine; any smoke boot stops it via `launchctl` and restarts it
  after. G0: test gates never relink the release binary — `cargo build
  --release -p cli` before any live boot, and check `stat -f '%Sm'
  target/release/multivenue-engine` against the last `crates/cli` change
  before trusting a harness number.
- **Worker verbs and pytest are globally serialized:** `pgrep -f
  'claude[-_]worke[r]'` / `pgrep -f pytes[t]` first; one SQLite seq
  namespace, one writer. The CMDLINE LAW: a long-running process must never
  match that guard in its argv (the dashboard execs a venv alias for this
  reason).
- **The frozen worker contract:** `claude-worker/src/claude_worker/backtest.py`
  argv `multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`,
  schema-1 JSON on stdout, the 8 verbs, the 202 frozen pytest pin — the
  harness conforms to the worker, never vice versa.
- **The ≤ 2 h law (2026-09-03/05, absolute):** no capture window, data gate,
  test, soak or protect time may exceed 2 hours. A window is a ≤ 2 h `ts_ns`
  slice of one run cut into a bounded symlink root; more data ⇒ pool
  DISJOINT windows that already exist, never schedule a wait. Gates are
  stated in fills/ticks/windows, never hours or days.
- **Research never enters git (2026-09-02, absolute):** data, strategy
  research, backtest/P&L reports, any doc ABOUT a researched strategy live in
  `docs/research/` (git-excluded) or the G8 external vault. `make
  license-check` refuses a tracked file there; no `git add -f`. A plan may
  record a ruling or a law; the substance stays out. Research one-shots are
  `claude-worker/tools_*.py` (git-excluded, never named in a tracked doc).
- **The Stage-3 entry gate** (`docs/arch/mvp-completion-plan.md` §7, forward
  binding): no AI-promoted member goes live before one keyed `serve` cycle
  with auto-promotion and one monitor-triggered rollback are observed live.
  **Waived for BIN15 only** (O-E1, 2026-09-15: hand-coded, no AI promotion
  path). No `serve`, no Anthropic API calls until the operator opens it.
- **Execution laws:** a live slot never falls back to paper (E-1); every live
  submit AND modify passes the risk gate, a cancel is never blocked by a cap;
  the HTTP response is the ACK, the `userFills` stream is the FILL (E-5); a
  requote is a MODIFY (E-7); the roll takes its own quotes back (E-8); the
  cloid encodes the slot (E-9); caps stay exactly paper's (O-E4); fees are
  MEASURED from `userFills.fee`, never read from a doc. Full text:
  `docs/risk-policy.md`.
- **Venue latency is measured, never assumed** — per host AND per location
  (`python -m claude_worker.latency_probe` → `docs/venue-latency.md` → the
  harness Δ table) before trusting any backtest number on a new box.
- **Retention:** PROTECT_DAYS 1 (24 h local, older in object storage on
  demand), every delete gated on `multivenue-archive verify`. **S-DOCTRINE:**
  the no-cloud rule binds the TRADING PATH; the cold archive lives entirely
  outside the engine (handwritten SigV4, no SDK, no `delete_object` verb).
- **Detached runs on the Mac:** `launchctl submit` relaunches on exit until
  `launchctl remove` — a wrapper's LAST line removes its own label; nohup
  children of MCP terminals die with the window.
- **If context runs short:** write interim state + the exact resume point +
  a relaunch prompt to the lane's vault log, then tell the operator.

## Build / test / run

```sh
cargo build --workspace                       # debug
cargo build --release --workspace             # release — what we deploy
cargo nextest run --workspace                 # unit + proptest + integration
make test-fast                                # skip fuzz/bench compile

# fuzz: `+nightly` is REQUIRED on this host (the 1.88 pin has no -Z)
cargo +nightly fuzz run polymarket_clob_frame -- -max_total_time=300

# allocation assertions — MUST show 0 B/op; --test-threads=1 is REQUIRED
# (CountingAllocator is process-global). False-green guard: the log must
# show a fresh `Compiling bench`, else `cargo clean -p bench --release`.
cargo test -p bench --test alloc_assertions --release -- --test-threads=1

cargo bench --workspace                       # criterion
cd claude-worker && uv run pytest             # worker (serialize with pgrep first)

make lint             # clippy --all-targets -D warnings (a gate)
make license-check    # SPDX + LICENSE/NOTICE + the research-in-git guard (a gate)
make copy-audit       # zero-copy ratchet vs scripts/copy-audit-baseline.txt (a gate)
make license-deps     # ONLY when a dependency changed (cargo-deny + cargo-about)

# engine, paper (the universe comes from ~/multivenue/universe.toml)
cargo build --release -p cli
cargo run --release -p cli -- run --paper --strategy ai
# armed (E7, TESTNET): the two-switch interlock, both switches or a refusal
cargo run --release -p cli -- run --strategy ai+vrp+xsd+bin15 \
  --exec ~/multivenue/exec.toml --arm-live 3
# offline consumers (MAY allocate): audit-replay / capture-catalog /
# backtest --ruleset R --replay-dir D --split 70/30 / audit-pnl / exec-smoke
```

## Universe config (`~/multivenue/universe.toml`)

Read ONCE at boot; changes apply on restart (the daily-restart lane refreshes
the PM dailies, which expire 16:00Z). Grammar: `universe.toml.example` +
`core-config::universe`. **Append, never reorder** — `SymbolId`s are file-order
ordinals; the worker map keeps the OLD sym for a reordered name by design.
Polymarket: `clobTokenIds` from the Gamma lane, `"<yes>:<no>"` pairs, ≤ 6
tokens (token 7 collides with anchor id 7). Binance: lowercase stream
symbols, one socket per symbol (254 at the current universe — the wrapper
raises the launchd fd soft limit to 8192). After a restart run
`claude-worker fetch` once; `unresolved=0` is the done-tell.

## Hard architectural rules (do not violate — the gates will fail)

### Rust
- **Zero allocations in hot paths.** No `Vec::push`, `format!`, `to_string`,
  `Box::new`, `Vec::from`. Preallocate at boot, reuse forever. Enforced by
  `core-alloc::CountingAllocator` (`make alloc-assert`).
- **Zero-copy (operator ruling 2026-09-19).** Everything that CAN be done
  zero-copy IS: scanners borrow the rx buffer and return offsets, encoders
  render into the FINAL wire buffer, signers hash in place
  (`keccak256_parts`), PODs move once into their ring slot. A copy that
  cannot be avoided carries, within the eight lines above it,
  `// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>` —
  the way `unsafe` carries `// SAFETY:`. Designed copies: kernel↔user,
  rustls' plaintext window, rx-tail compaction, the ring-slot publish,
  ≤ 64 B PODs by value. Enforced by `make copy-audit` (a RATCHET against
  `scripts/copy-audit-baseline.txt`; only the operator grows the baseline)
  and the `zero-copy-auditor` agent. A cold operator module may opt out with
  a `//! COPY-DOCTRINE:` header — never anything the engine loop reaches.
- **No `dyn Trait` in hot paths.** `Engine<S: Strategy, D: OrderDispatch>` is
  monomorphized.
- **No `tokio`, `serde_json`, `reqwest`, `async-std`, `ethers`, `alloy` on
  the hot path** — mio + rustls + handwritten byte scanners; `secp256k1` +
  `tiny-keccak` directly. `.claude/hooks/no-forbidden-crates.sh` blocks the
  edit.
- **No iterators/`foreach` and no bounds checks in hot loops** — raw indices,
  `get_unchecked` inside safe wrappers with `// SAFETY:`.
- **No panics in release hot paths** — `debug_assert!`; release is
  `panic = "abort"`. Fail-fast beats graceful recovery on the trading path.
- **Every hot POD is `#[repr(C)]` + `Copy`; every ring / cache-sensitive
  struct is `#[repr(align(64))]` and size-asserted.**
- **Strategies implement `strategy-core::Strategy`**; every ingress parser has
  a proptest AND a cargo-fuzz target; every public fn has a happy-path and a
  failure-mode test.
- **Offline paths (audit-replay, backtest, the operator tools) MAY
  allocate** — each such module says so in a doctrine header.

### Python (`claude-worker/`)
- **Full `import x` only. Never `from x import y`.** (ruff + a pytest enforce it.)
- No live Anthropic API calls in tests; the SDK is constructed inside `serve`
  only. Model constants live in `config.py` and are pinned by `test_config.py`.

### Licensing (enforced by `make license-check`)
- Every new `.rs` / `.py` / `.sh` starts with the two-line SPDX record
  (`// SPDX-License-Identifier: Apache-2.0` / `// Copyright 2026 Anton
  (darkcite)`; Python `#`; shell AFTER the shebang). Every new crate:
  `license.workspace = true` (`fuzz/Cargo.toml` carries the literal).
- A dependency change ⇒ `make license-deps` and commit the regenerated
  `THIRD-PARTY-NOTICES.md`. Never vendor third-party source. No binary leaves
  the host without `LICENSE` + `NOTICE` + `THIRD-PARTY-NOTICES.md`.
- Repo-wide rewrites: `git diff --summary` must show no mode changes (a
  `> tmp && mv` pass strips exec bits).
- Authority (standing, archived): `docs/arch/license-audit-2026-08-27.md`;
  contributor copy `CONTRIBUTING.md`.

### Deployment
- **No cloud services on the trading path, no observability stack** — TUI +
  log files + `/metrics` and `/state` on `127.0.0.1:9191`. Phase 7 is a plain
  Linux VM.
- **Signing keys** load into an `mlock`'d page and zeroize on drop.

## Directory guide

- `crates/core-*` — primitives: ring (SPSC only), time, config (every
  `*.toml` parser), alloc, io (PMLR writer/reader, `PmlrCapture`, atomic
  state files), net (mio + rustls transport, WS framing, `IoBuf`,
  `Keepalive`), parse (byte scanners), simd, crypto (SHA-256/HMAC/base64),
  types (wire PODs, `SymbolId`, `VenueId`), regime, vol, fill, latency,
  metrics (fixed registry, 512 counters).
- `crates/ingress-{polymarket,binance,okx,deribit,hyperliquid,bybit,rpc}` —
  one thread per source, `discovery.rs` = boot REST; `crates/ingress-ai` —
  the UDS+HMAC command plane and the ruleset validator.
- `crates/strategy-{set,core,vm,ai-exec,vrp,xsd,bin15,icdp}` — the composed
  set and its members; `strategy-{latency-arb,cross-arb,ev,rule-tree}` are
  in-tree but unlinked/off. `book-builder`, `opt-registry`,
  `options-select`, `research-artifacts`.
- `crates/engine` — the single-threaded loop; `crates/engine-snapshot` —
  the seqlock `/state` snapshot; `crates/tui`.
- `crates/exec-router` + `crates/exec-hyperliquid` + `crates/signer-eip712`
  + `crates/clob-dispatcher` — the execution lane (see CURRENT STATE).
- `crates/cli` — `multivenue-engine` (run / audit-replay / capture-catalog /
  backtest / audit-pnl / exec-smoke …); `paper.rs` = the boot + metrics
  assembly; `exec_boot.rs` = the arming interlock.
- `crates/bench` — criterion + the alloc assertions (its `CountingAllocator`
  is process-global; keep other crates' tests out). `fuzz/` — targets.
- `claude-worker/` — the Python 3.14 worker: 8 frozen verbs + the modules
  (candles, funding, regime, pnl_report, window_root, library/compose,
  archive/objstore, dashboard, xsd_author, bin15_*, …). Integration tests
  live per crate under `tests/`.
- `scripts/` + `launchd/` — the wrapper, daily-restart, candles/regime/
  archive/retention cycles, `exec-smoke.sh`, `copy-audit.sh`.
- `.claude/` — agents (`alloc-auditor`, `zero-copy-auditor`,
  `risk-reviewer`, `parser-property-tester`, all Opus 5), commands, hooks.
- `docs/arch/` — history (index in its README). `docs/research/` — the vault.

## Common pitfalls — if you're about to do one of these, stop

1. Adding `tokio` / `serde_json` / a cloud SDK anywhere near the hot path.
2. `.collect::<Vec<_>>()`, `String`, or `async fn` on anything the engine
   loop touches.
3. `from x import y` in `claude-worker/`.
4. Proposing Prometheus/Grafana/Terraform, or paid API integrations before
   demonstrated P&L.
5. Skipping tests "because it's a small change" — the alloc and copy gates
   exist because small changes regressed them.
6. **Trusting `cargo` inside the Cowork Linux sandbox** — stale fingerprints
   give FALSE GREENS. Compile and test on the Mac only. On the Mac,
   impossible unresolved-import errors right after edits = stale rmeta —
   `cargo clean -p <crate>` and retry.
7. Trusting probe fixtures over live boots — venue wire drifts were only ever
   caught LIVE; new parsers get a live smoke (`--raw-tap`).
8. Long commands through the RustRover terminal — it is ~45 s regardless of
   the timeout; `nohup … > <log> 2>&1 &` then poll. A REUSED terminal can
   return stale mixed output; use `reuseExistingTerminalWindow=false` for
   evidence.
9. Modifying the worker's frozen surfaces (`backtest.py` argv/schema-1, the
   verb surface, the 202 pytest pin).
10. A new `.rs`/`.py`/`.sh` without the SPDX header, a new crate without
    `license.workspace = true`, a dependency without `make license-deps`.
11. Committing ANY research material or a `tools_*.py` one-shot, or naming a
    concrete one-shot in a tracked doc (cite the owner doc instead).
12. Trusting a backtest / audit-pnl number from a v2 (pre-2026-09-03) root —
    it is STALE-BLIND and an upper bound; judged numbers need a v3 root and a
    ≤ 2 h window cut by `ts_ns` with the events file cut too.
13. Trusting harness numbers from a release binary older than the last
    `crates/cli` change — `cargo build --release -p cli` first (G0).
14. **Re-sending a file through the Cowork file bridge after a first send of
    the same path** — it reports `written` and changes nothing (a cached
    upload). A file goes through the bridge ONCE; every later change is an
    in-place edit on the mount (`device_bash` python/sed) verified by
    `sha256sum` on both sides. `.claude/` is refused by the bridge — edit it
    on the mount. A file under the container's `/mnt/user-data/outputs/` is
    snapshotted at its first write — stage each version under a new name, or
    carry the bytes in a base64 python script.
15. `pkill -f <pattern>` can match your own polling shell — bracket one letter
    (`multivenue-engin[e]`).

## macOS session facts

- AF_UNIX `sun_path` length cap bites long socket paths; `SO_RCVTIMEO` returns
  EINVAL on a peer-closed UDS; Darwin's pthread mutex heap-allocates on first
  lock (which is why `SnapshotCell` is a seqlock).
- `std::thread::scope` panic hangs without a StopOnDrop guard; `sample <pid>`
  diagnoses hangs.
- A launchd agent is born with a 256-fd SOFT limit — the wrapper raises it.
- CPU pinning (`sched_setaffinity`) does nothing on macOS; every "pinned"
  thread floats — the core map in the sheets is intent.
- RustRover MCP must attach (`get_project_modules`) against the main checkout
  first; if it won't attach, stop.

## Preferred Claude models

- Bulk artifact generation: `MODEL_BULK = "claude-haiku-4-5"`. Reasoning:
  `MODEL_REASONING = "claude-sonnet-5"`. Strategy proposals (`serve`
  strategist): `MODEL_STRATEGIST = "claude-fable-5-1"` (needs `anthropic >=
  1.4.0`). NEWS tier-3 event analyst (`serve` only): `MODEL_ANALYST =
  "claude-opus-5"`. Hard work — reviews, architecture, the three review agents:
  Opus 5 (`claude-opus-5`).
- These are constants in `claude-worker/src/claude_worker/config.py`, pinned
  by `tests/test_config.py` — change the doc and the constant together, and
  verify a model id against the installed SDK's `anthropic/types/model.py`
  (a wrong string fails at the first keyed `serve` cycle, which IS the
  Stage-3 gate). `THIRD-PARTY-NOTICES.md` covers the Cargo graph only; a
  Python SDK bump needs no `make license-deps`.

## When in doubt, read (in this order)

1. This file — CURRENT STATE and the standing laws.
2. `docs/risk-policy.md` — the execution laws and the E6/E7 record.
3. `PLAN.md` — everything architectural; `docs/wire-format.md` for bytes.
4. The lane's own vault log, if the operator points you at one.
