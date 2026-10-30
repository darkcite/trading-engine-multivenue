# CLAUDE.md — Multivenue Trading Engine

This file front-loads what every session needs — the standing laws, the hard
rules, how to build and test, and where everything else lives. It is
deliberately self-sufficient and deliberately carries NO live state: what is
running and armed is `/state` on `127.0.0.1:9191`; the current-state record is
`docs/state.md` (rewritten at each lane close); history is `docs/arch/`.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine for
systematic strategies across Binance (spot, USDⓈ-M, COIN-M, options), OKX,
Deribit, Hyperliquid (incl. HIP-4 outcome markets), HyperEVM, Bybit, MEXC
(data-only), Hypercall options, Polymarket CLOB and Polygon RPC. Strategies
are composed at boot from a 16-slot set and may trade **any subset**;
**Polymarket is one venue among several, not the target**. v1 runs on a
MacBook Pro M4 on free-tier APIs. Claude (the `claude-worker` process, and
in-session) is an **offline strategy researcher** — never in the hot path.

Slots (`crates/strategy-set`: one `member_table!` row and one `SlotMask`
(u16) bit each; `BUILT_MASK` 0x0BFF, `ALL_MASK` = `all` 0x00FF; timers per
slot): 0 hyparb (HyperEVM AMM ↔ HL Core arb; needs `~/multivenue/hyparb.toml`
+ the pool ingress; EVM writes TESTNET only, chain 998) · 1 vrp · 2 xsd ·
3 bin15 (HIP-4 15-minute binaries, H90 late law only) · 4 ai-exec (AI door
1, intents) · 5 vm (AI door 2, tables; `ai` = 4+5 = mask 48) · 6 xmm (HL
post-only maker; needs `xmm.toml`) · 7 hcv (Hypercall options vs HAR σ̂;
needs `hcv.toml`; a live slot 7 refuses the boot) · 8 pine (third-party Pine
scripts, `strategy-pine`, PVM impl-08; PAPER only — boots with `--pine
<manifest>`; live is refused at parse (`[exec.slot.8]`), by `--arm-live 8`
and for the AI's enable; not in `all`) · 9 pm15 (H90 on Polymarket
15-minute up/down; needs `pm15.toml`; not in `all`) · 11 pmj (H90 on
Polymarket 5-minute up/down through Jupiter Predict; needs `pmj.toml`;
PAPER only — a live slot 11 refuses the boot; not in `all`). Slots 10 and
12–15 are RESERVED: arming one is refused; a member there needs its own
ruling (S16-R5). `latency-arb` and `icdp` are unlinked and refuse the boot.
Paper/live, caps and boot tells per slot: `/state`, `docs/state.md`, the
slot's risk-policy section.

`~/multivenue/strategy.conf` names `STRATEGY` (`+`-joined members or `ai`);
`scripts/engine-wrapper.sh` resolves it through `multivenue-engine
strategy-mask <STRATEGY>` (prints `<mask>\t<members>` or exits 78; `all`
refused). A binary rollback rolls the wrapper back too.

## Live safety — before you touch the running engine

- **Assume real money is at stake.** A slot can be LIVE (slot 3 has been
  since 2026-10-16). Read `/state` (`boot.git_sha`, `exec`, the slot blocks)
  and `docs/state.md` before any restart, arming, slot, universe or config
  change. Never take armed/live state from a project doc or memory.
- Restarts only at a safe minute the operator approves, off the 15-minute
  grid; an armed engine passes `scripts/exec-smoke.sh` first (daily-restart
  does it itself). After ANY restart verify `vm_rows_active ≥ 1` on `/state`.
- Arming is the operator's: the two-switch interlock `--exec
  ~/multivenue/exec.toml --arm-live <slot>` and the lane flip scripts
  (`scripts/bin15-flip.sh`, `scripts/hyparb-flip.sh`).
  `LIVE_ARM_VENUES = [Hyperliquid, HyperEvm, Binance]`. Immediate halt:
  `echo <slot> >> ~/multivenue/exec.HALT`.
- Venue agent approvals expire (dates in `docs/state.md`); past expiry every
  live order is refused and the reject streak halts the slot.

## Where to look — and where not to

- **Authority, highest first:** this file → `docs/risk-policy.md` (caps, kill
  switches, LAWS E-1..E-9 + E-11, BX-n, one section per slot; ~17 000 lines:
  `grep -n '^## '` and read by section, never whole) → `docs/state.md` →
  `PLAN.md` → the lane's vault log. Memory and claude.ai project docs are
  context, never authority.
- **Current docs:** `PLAN.md`, `docs/state.md`, `docs/repo-map.md`,
  `docs/wire-format.md`, `docs/migration.md`, `docs/local-setup.md`,
  `docs/venue-latency.md`, `docs/hot-path-latency.md`,
  `docs/research-universe.md`, `docs/binance-runbook.md`,
  `docs/hyparb-build-plan.md`, `docs/ai-strategy-pipeline.md` + its three
  `.svg` sheets, `docs/prompts/ai-session.md` (pinned by a worker test —
  never move it), every `*.toml.example` (each is its parser's contract).
- **`docs/arch/` is HISTORY and `docs/research/` is the git-excluded vault:
  read either ONLY when the operator explicitly asks** — never for
  orientation. The archived documents that still bind are named where they
  bind (Licensing, the S3 S-LAWs, the ≤ 2 h law). Lane working notes: a
  dated vault log, never git, never this file.

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
  path). No `serve`, no Anthropic API calls from `claude-worker` or anything
  the engine reaches, until the operator opens it. **Exempt (O-AR0,
  2026-10-19):** the AIRES research workspace — Claude Managed Agents driven
  from the Mac by the research controller; its own key `MV_RESEARCH_API_KEY`
  in `.env` (O-AR9), never the worker's; it never touches 9191, `ai.sock`,
  exec keys or venue credentials; research only.
- **Execution laws:** a live slot never falls back to paper (E-1); every live
  submit AND modify passes the risk gate, a cancel is never blocked by a cap;
  the HTTP response is the ACK, the `userFills` stream is the FILL (E-5); a
  requote is a MODIFY (E-7); the roll takes its own quotes back (E-8); the
  cloid encodes the slot (E-9); caps stay exactly paper's (O-E4; slot 3's
  S7-L1 caps are the operator's own entry); fees are
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
- **Pushing a message to the operator (any AI session):**
  `scripts/notify-send.sh [-l] [-s SOURCE] TEXT` (POSIX sh — a Cowork shell or
  the Mac; stdin when no TEXT) or `multivenue-notify send` drops a file into
  an inbox that main's notifier relays to Telegram as `[AI:SOURCE]` (`-l`
  rings, default silent, ≤ 60 an hour). Use it for a finished long run, a
  gate verdict or a finding the operator must see — not for routine
  narration; never a secret or a key in a message.
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

## Gates (a red is a red)

`make lint` · `cargo nextest run --workspace` (10 expected skips; a `LEAK`
at ~0.2 s under nextest < 0.9.145 is nextest's pipe race, HK23; no doctests
— `cargo test --doc -p core-ring` for the `Mailbox` one) · `make
alloc-assert` (0 B/op; one thread + a fresh `Compiling bench`) · `make
copy-audit` (`new=0`) · `make license-check` · `make bench-check` · `cd
claude-worker && uv run pytest` · `make py-lint` · fuzz: each touched target
60 s clean. Counts at HEAD: `docs/state.md`. `gate-runner` (Haiku) runs the
battery and returns one line per gate.

## Universe config (`~/multivenue/universe.toml`)

Read ONCE at boot; changes apply on restart (the daily-restart lane refreshes
the PM dailies, which expire 16:00Z). Grammar: `universe.toml.example` +
`core-config::universe`. **Append, never reorder** — `SymbolId`s are file-order
ordinals; the worker map keeps the OLD sym for a reordered name by design.
Polymarket: `clobTokenIds` from the Gamma lane, `"<yes>:<no>"` pairs, ≤ 6
tokens (token 7 collides with anchor id 7). Binance: lowercase stream
symbols, one socket per symbol (254 at the current universe — the wrapper
raises the launchd fd soft limit to 8192). COIN-M (BX2): `coinm`
`<coin>usd_perp` (ordinals from 3072), `coinm_dated` `<coin>usd_<yymmdd>`
(from 3584), `binance-coinm:` descriptors, two sockets each on dstream;
sizes are contracts (100 USD of face on BTC, 10 on the others). MEXC: spot UPPERCASE
(`BTCUSDT`), perp `BASE_QUOTE` (`BTC_USDT`), perp ordinals from 512. After
a restart run `claude-worker fetch` once; `unresolved=0` is the done-tell.

## Hard architectural rules (do not violate — the gates will fail)

### Rust
- **Zero allocations in hot paths.** No `Vec::push`, `format!`, `to_string`,
  `Box::new`, `Vec::from`. Preallocate at boot, reuse forever. Enforced by
  `core-alloc::CountingAllocator` (`make alloc-assert`).
- **Zero-copy (operator ruling 2026-09-19).** Everything that CAN be done
  zero-copy IS: scanners borrow the rx buffer and return offsets, encoders
  render into the FINAL wire buffer, signers hash in place
  (`keccak256_parts`), PODs are copied once into their ring slot
  (`Producer::try_push_ref`) and read there in place
  (`Consumer::try_pop_ref` lends the slot through a `Popped` guard). A copy that
  cannot be avoided carries, within the eight lines above it,
  `// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>` —
  the way `unsafe` carries `// SAFETY:`. Designed copies: kernel↔user,
  rustls' plaintext window, rx-tail compaction, the ring-slot publish,
  ≤ 64 B PODs by value. Every ingress parser fills the caller's frame IN
  PLACE (`&mut Frame` → `bool`, written once, untouched on `false`); no
  frame rides a run loop's `Dispatch` (each const-asserted ≤ 64 B); a
  top-K change gate flips a `core_types::DepthPair`, never copies.
  Enforced by `make copy-audit` (the exec lane, core-net, core-ring, all
  ten ingress crates, the HYPARB crates, strategy-xmm, strategy-hcv,
  core-settle, core-fill, the engine and the state-writer threads; a
  RATCHET against
  `scripts/copy-audit-baseline.txt` — only the operator grows the baseline;
  `scripts/copy-audit-selftest.sh` proves its `#[cfg(test)]` reading first)
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
- **N-DOCTRINE (O-NT-A, 2026-10-24) — the one-way Telegram notifier is
  permitted beside that rule.** `multivenue-notify` (`crates/notify`; launchd
  `com.multivenue.notify`; `~/multivenue/notify.toml` absent = off) runs out of process,
  is never linked into or reached by the engine, only READS `/state` (and
  `/metrics`, for the HL gateway's session anchor and worth) on loopback and
  the run's `engine-{fills,orders,events}.pmlr`, takes only
  `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` from `.env` (launched under
  `env -i`), writes only its own state dir and log, talks only to
  `api.telegram.org` and never reads Telegram updates. Deploying, reloading
  or rolling it back restarts no engine. `docs/risk-policy.md` "NOTIFY —
  N-DOCTRINE".
- **Signing keys** load into an `mlock`'d page and zeroize on drop.

## Directory guide (the full map: `docs/repo-map.md`)

`crates/core-*` primitives (ring is SPSC only) · `crates/ingress-*` one
thread per source (`discovery.rs` = boot REST; `ingress-ai` = UDS+HMAC) ·
`crates/strategy-*` the set and members (`latency-arb`, `cross-arb`, `ev`,
`rule-tree`, `icdp` unlinked) · `crates/engine` the loop · `crates/exec-*`,
`signer-*`, `clob-dispatcher`, `exec-gateway` the order path · `crates/cli`
`multivenue-engine` (`exec_boot.rs` = the arming interlock) · `crates/bench`
· `crates/notify` `multivenue-notify` (the one-way Telegram notifier,
N-DOCTRINE) · `fuzz/` · `claude-worker/` · `scripts/` + `launchd/` · `.claude/`.

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
   caught LIVE; new parsers get a live smoke (`--raw-tap`, or — while the
   engine is armed live — a standalone `#[ignore]` smoke that never stops
   it, the `crates/cli/tests/mexc_live_smoke.rs` shape).
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
16. **Assuming inbound traffic keeps a socket alive.** A venue can require a
    CLIENT heartbeat however busy its feed: MEXC futures closes the socket
    60 s after the last client `ping` while streaming (caught only live).
    `Keepalive::poll` pings only after inbound silence; such a venue needs
    `Keepalive::poll_client_heartbeat` (the ping is due N s after the last
    ping SENT).

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

## Model routing

- Main session: Opus 5.5. Raise effort (`/effort high|xhigh`) for exec,
  signer, risk-policy and live-arming work; Fable 5.1 (`/model fable`, ~2.5×
  Opus) only for multi-lane architecture or research synthesis after Opus
  at xhigh fell short.
- Named agents pin their own model: `risk-reviewer` (xhigh),
  `zero-copy-auditor`, `alloc-auditor` → `claude-opus-5-5` (O-8);
  `parser-property-tester` → `claude-sonnet-5-5`; `Explore`, `gate-runner` →
  `claude-haiku-5-5`.
- Ad-hoc delegation (`model`): search, file location, log triage, gate runs
  → haiku; doc/status sync, test scaffolding, refactors with a written spec
  → sonnet; anything judging hot-path reachability, a money path, venue
  semantics or a law → opus. Never delegate a live-arming decision below Opus.
- Cowork registers no `.claude/agents/` and runs no `.claude/settings.json`
  hooks: spawn general-purpose with the agent file's body as the prompt and
  its pinned model. There, this file's laws are the only guard.

## Worker API models (`claude-worker/src/claude_worker/config.py`)

`MODEL_BULK = "claude-haiku-4-5"` (tagging, labels) · `MODEL_REASONING =
"claude-sonnet-5"` (news labeling, rule parsing) · `MODEL_STRATEGIST =
"claude-fable-5-1"` (`serve` strategist; `anthropic >= 1.4.0`) ·
`MODEL_ANALYST = "claude-opus-5"` (NEWS tier 3, `serve` only). Pinned by
`tests/test_config.py`: change the constant, the test and this line
together, and verify the id against the installed SDK's
`anthropic/types/model.py` (a wrong string fails at the first keyed `serve`
cycle — the Stage-3 gate). Three are previous-generation; the move to the
5.5 ids is the operator's call (`docs/state.md`, open items). A Python SDK
bump needs no `make license-deps`.

## Context and compaction

When compacting, preserve: the lane and its vault log path, every file
modified, the gate commands run with their verdict lines, and every operator
ruling given in this session, verbatim.

## When in doubt, read (in this order)

1. This file — the laws, then "Live safety".
2. `/state` and `docs/state.md` — what is running, armed and open.
3. `docs/risk-policy.md` by section; `PLAN.md`; `docs/wire-format.md` for bytes.
4. The lane's own vault log, if the operator points you at one.
