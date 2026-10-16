# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session starts from **this file +
the current lane's docs** and nothing else. It carries the CURRENT state and
the STANDING laws only — history lives in `docs/arch/` (see "Where to look").

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that
executes systematic strategies across a multivenue universe — Binance
spot/USDM/COIN-M, OKX, Deribit, Hyperliquid (incl. HIP-4 outcome markets), Bybit,
MEXC (data-only), Hypercall options (slot 7's S1 member trades them in
PAPER since the 2026-09-26 go-live; the HC9 order arm runs as operator
verbs only),
Polymarket CLOB, Polygon RPC, plus a boot-selected options ladder.
Strategies are composed at boot from a 16-slot set (0–7 built, 8–15
reserved; S16) and may trade
**any subset** of that universe; **Polymarket is one venue among several, not
the target**. v1 runs on a MacBook Pro M4 on free-tier APIs. Claude (via the
`claude-worker` Python process, and in-session) is an **offline strategy
researcher** — never in the hot path.

Slots (`crates/strategy-set`, one `member_table!` row and one `SlotMask`
(u16) bit each; 16 since S16, 2026-10-15; `all` = `BUILT_MASK` 0x00FF = 255,
the enable mask today; 8–15 RESERVED, no member — arming one is refused at
boot, adding one is its own ruling; risk-policy "S16"):
0 hyparb (HyperEVM ↔ HL Core arb, since HYPARB H0 2026-09-23 — lands DARK,
O-H8; boots only with `~/multivenue/hyparb.toml` + the pool ingress, H5;
`latency-arb` unlinked, its name refuses the boot) · 1 vrp ·
2 xsd · 3 bin15 · 4 ai-exec (AI door 1, intents) · 5 ruleset VM (AI door 2,
tables) · 6 xmm (HL post-only maker, since XMM XH1 2026-09-26; quotes on the
queue law since XH2, paper only; `/state` `xmm` block, `engine_xmm_*`,
`[labels.xmm]` and audit-pnl prints since XH3; boots only with
`~/multivenue/xmm.toml`; `icdp` unlinked, its name refuses the boot,
`backtest --member icdp` stays) · 7 hcv (Hypercall S1 options vs the HAR
σ̂, hedged on HL perps, since HC11 2026-09-26 — PAPER only on the
held-quote law; in the live mask since the go-live, 2026-09-26 20:15Z
(O-HC25; it landed DARK, O-HC18); boots only with
`~/multivenue/hcv.toml`; a live slot 7 refuses the boot; its book persists
in `hcv-state.tsv` by contract since HC11b, written off the engine thread;
risk-policy "HYPERCALL — slot 7"). Member timers run per
slot (each on its own period). The engine boots the `STRATEGY` named in
`~/multivenue/strategy.conf` through `scripts/engine-wrapper.sh`: `+`-joined
member names or `ai` (= ai-exec+vm, 48), any order, parsed once by
`strategy_set::parse_strategy`; `multivenue-engine strategy-mask <STRATEGY>`
prints `<mask>\t<members>` or exits 78. Since the S16 deploy (2026-10-15)
the wrapper resolves `STRATEGY` through `strategy-mask` (any named combination; `all`
refused) instead of its allow-list; a binary rollback rolls the wrapper back
too.

## Where to look — and where not to

- **Current docs (read as needed):** `PLAN.md` (architecture deep-dive),
  `docs/risk-policy.md` (caps, kill switches, LAWS E-1..E-9 + E-11 — the
  HIP-4 settlement law, "E8" — the exec lane's
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

## CURRENT STATE (2026-10-14)

Each lane's merge and go-live paragraphs, as this section carried them until
2026-10-13, are in `docs/arch/claude-md-state-log-2026-10-13.md`, verbatim.
The lane records stay where they always were: `docs/risk-policy.md` and
`docs/migration.md`, under the names cited below.

- **Every pre-execution lane is closed** (Stage 1–2, M1–M6, VM2, VT, regime +
  dashboard, S3 archive, ICDP, XSD, VRP, BIN15). Their records are in
  `docs/arch/` and the vault; nothing there is open work.
- **Live operation.**
  - One launchd engine (`com.multivenue.engine`) in PAPER, restarted at
    00:10/08:33/16:05/20:15/21:15Z by `daily-restart`.
  - Beside it: `caffeinate`, hourly `candles`, 5-min `regime`, nightly
    `retention` (PROTECT_DAYS 1, every delete gated on S3 `verify`),
    `archive` (04:30 local) and the read-only `dashboard`.
  - It runs the release build of HK, part 5 (`30ed1b4`, booted
    2026-10-16 06:47Z; `/state` `boot.git_sha` names the commit) with the mask
    `strategy.conf` names (`ai+vrp+xsd+bin15+hyparb+xmm+hcv`).
    `strategy.conf` also exports `BYBIT_REST_HOST=api.bytick.com`
    (HK17) and `RAW_TAP=pm,hl` (rejects only, HK-R23). `/state`
    `exec.configured` is 0 (no
    route table): nothing is armed.
  - After ANY restart, verify `vm_rows_active ≥ 1` on `/state`.
  - The worker runs from the working tree, so a worker edit is live at its
    lane's next cycle.
- **The execution lane (E1–E7; risk-policy "E6", "E7").**
  - `crates/exec-router`: per-slot `ExecMode` on `Order.strategy_id`;
    `RoutedDispatcher`; the E6 risk gate (per-order / open-orders / day /
    instance caps, the venue-fill ledger, the sticky halts, `exec.HALT`).
  - `crates/exec-hyperliquid`: msgpack + EIP-712 `Agent` signing pinned by
    SDK vectors; the `/exchange` arm; `userFills`; reconciliation; the
    address-budget governor; LAW E-8 sweeps. Its WebSocket order gateway is
    `exec_hyperliquid::gateway` (XMM XH4).
  - Armed ONLY by the two-switch interlock `--exec ~/multivenue/exec.toml
    --arm-live <slot>`. `LIVE_ARM_VENUES = [Hyperliquid, HyperEvm, Binance]`
    (HyperEvm on slot 0 only; Binance for at most one live slot, BX6).
  - The `Scope::Live` keys are in the REPO `.env`; HYPARB's testnet key is in
    `~/multivenue/.env` (the wrapper hands it over).
  - A market-data host and an exchange host on different networks refuse
    the boot. `halt_on_recon_stale_ms` is REQUIRED on every live slot.
  - Every restart of an armed engine passes `scripts/exec-smoke.sh` first
    (daily-restart does it itself).
- **Slot 3 (bin15) is PAPER.** The R0 session (mainnet, 2026-09-19: 6
  entries, −$5.105; risk-policy "E7 — MAINNET R0") ended on its session
  bound.
  - The live arm is restart-proof since S7-L1 (risk-policy "The live arm,
    restart-proof"). Arming is the operator's `scripts/bin15-flip.sh live`
    (`--gateway` for the WebSocket arm, XH4i); `bin15-flip.sh paper`
    disarms.
  - The venue's HIP-4 minimum order is **1 USDC**, and its fee is charged
    on SETTLEMENT (14 bps of the payout; `fees.toml`'s `prediction_settle`,
    harness `--fee-bps hl.prediction.settle`).
  - **The mainnet agent approval EXPIRES 2026-10-19.**
  - **BIN15 H90, the late-window entry** (τ 2–30 s; risk-policy "BIN15
    H90"): ON in PAPER since 2026-10-13 11:22Z (`bin15.toml`
    `late_enabled = 1`; the boot line reads
    `late=2-30s:p>=0.90:ask<=p-2c:persist2:$100:k1.550`). Not live.
  - The live dust test (`scripts/bin15-late-dust.sh`) PASSED twice on
    mainnet, 2026-10-13 and 2026-10-14: 8/8 IoCs filled at the ask and
    settled at 1 (vault doc 33 §9 / §9b).
  - `bin15_accrue report` scores doc 33 §3's bars from
    `late_entries.tsv` / `late_fires.tsv`. Doc 34 (OOS2) runs on or after
    2026-10-19.
- **Slot 0 (HYPARB; `docs/hyparb-build-plan.md` §16–§17, risk-policy
  "HYPARB — slot 0").**
  - The HyperEVM AMM ↔ HL Core arb, in PAPER with the TESTNET shadow (chain
    998 only, compile-time).
  - LIVE only through three switches (`scripts/hyparb-flip.sh`), on slot 0's
    own wallet. Real money only after paper's gate G1.
  - G1 is tallied by `claude_worker.hyparb_g1` (HK26): each closed session
    folded under the member's own P&L law into
    `~/multivenue/worker/hyparb/g1-sessions.tsv` nightly (0020 slot), the
    2-block replay into `g1-replay.tsv`; `hyparb_g1 status` gives the
    verdict. 2026-10-15: NOT PASSED — 833 judged AMM fills, Σ −$59.46 over
    114 sessions (fees and gas exceed the edge). The nightly report's
    slot-0 row is hedge-only (audit-pnl has no AMM judge): not G1.
  - HZ part B is not green from this Mac, so paper AMM fills are an upper
    bound on latency.
  - The operator's verbs: `scripts/evm-testnet.sh` (zsh — never `sh`) and
    `scripts/evm-live.sh … --confirm`. The testnet executor is `0x6c16…9cc5`
    on 998. The live-only P&L stop is +$50 / −$20. forge is not on this Mac.
- **Slot 6 (XMM, the HL post-only maker).** Paper, on the queue law.
  - Its gateway arm is wired (XH4f), and slot 3 can ride a gateway too
    (XH4i). One Hyperliquid gateway arm per engine (A-S7-85).
  - Nothing is armed on mainnet: no `HYPERLIQUID_XMM_*` keys exist.
  - XH4i-8, the outcome battery (`exec-smoke --hl-outcome --lifecycle
    --outcome <id>`, exit codes 40–48; testnet only, nothing armed), merged
    from `xmm` on 2026-10-14 (the operator's word; risk-policy "XH4i-8"); its
    loopback `run()` test is HK15.
  - A-S7-81 (Q5) is fixed in the shared binder (HK13): a repeat of the live
    roll leaves the legs alone on both arms. Left before BIN15 rides the
    gateway live: the operator's `bin15-flip.sh live --gateway`.
- **Slot 7 (hcv) and Hypercall, the eighth venue** (live since the
  2026-09-26 go-live; risk-policy "HYPERCALL — slot 7", "HYPERCALL — the
  order arm").
  - Slot 7 trades Hypercall options in PAPER, judged by the held-quote law.
    Its book persists in `hcv-state.tsv`.
  - The order arm (`crates/exec-hypercall`) runs only as the operator's
    `hypercall-live` verbs; the engine refuses a live slot 7.
  - **OPEN — HC11c**, the operator's ruling: at switch-on slot 7 spent its
    $50 premium cap on weekend quotes 11–19 vol points wide. It buys at the
    ask and marks at the mid, so the $20 day stop (a level, not a latch)
    tripped on the spread; σ̂ ignores the HAR set's weekday profile.
  - Fees `[fees.hypercall] option = "0:0"` are UNVERIFIED (O-HC7), as are
    `[fees.mexc]`'s published rates.
  - `hypercall_history`'s payouts lane waits for `CLAUDE_WORKER_HC_WALLETS`.
- **The Binance arm (BX0–BX13; `docs/binance-exec-plan.md`,
  `docs/binance-runbook.md`, risk-policy "BX — the Binance execution laws").**
  - `crates/exec-binance` covers:
    - USDⓈ-M and COIN-M (COIN-M IoC-only);
    - spot incl. bStocks (IoC);
    - options (IoC);
    - Stocks — battery-only: the engine refuses `equity`.
  - It is deployed DARK: no member arms it.
  - The mainnet R0 session (runbook §13) and the Stocks battery (§12) are
    the operator's.
- **Market data: ten ingress crates.**
  - MEXC (the seventh venue) is DATA-ONLY (O-MX1).
  - The Binance options feed is fstream's routed `optionMarkPrice` stream
    (BX0).
  - Since HK7b (2026-10-13) the Binance options join the AI boot universe as
    signal legs: an order on one is unroutable (`core_fill::signal_only_sym`).
  - Live smokes WITHOUT stopping the engine: `cargo test --release -p cli
    --test <name>_live_smoke -- --ignored --nocapture` for `mexc`,
    `binance_md`, `binance_coinm`, `hypercall` and `hyperevm`. Each runs under
    the `CARGO_TARGET_DIR` its module doc names, so the wrapper's
    `target/release/multivenue-engine` is never relinked (`*_SMOKE_SECS`
    ≤ 900).
  - Hyperliquid's venue law: a subscribe to a settled or unknown HIP-4 coin
    FINs the whole socket, and `outcomeMetaUpdates` never replays a roll.
    The HL thread never runs DNS; `hyperliquid: run-loop returned` names
    each session's end.
  - LuLu on this Mac blocks a freshly built binary's outbound connections
    until the operator allows it: a smoke's boot-REST `Timeout` is LuLu.
- **Zero-copy (risk-policy "ZC pass A", "ZC pass B", "HK2").**
  - Every ingress parser fills in place, and core-ring lends its slots.
  - The ten drains share `core_net::drain` and one oversize guard.
  - Open until rustls 0.24: core-net's rustls RX copy, and rustls' buffered
    API allocating per record (gate 72).
- **HAR (H1–H3).** The engine owns `core_vol::LongVolSet` (≤ 12 series, from
  `~/multivenue/har.toml`). Its state is written off the engine thread, and
  it is published as `/state.har`. The worker's `har_backfill` / `har_seed`
  ride `candles-cycle.sh`. Since HK7a the Hypercall index lands in
  candles.db as 1 m capture bars, a candidate primary per `har_seed compare`.
- **The HK housekeeping lane (risk-policy "HK", "HK, part 2").** HK1–HK10
  (2026-10-13) closed Section A of that day's survey; HK11–HK17 (2026-10-14)
  Section B and the day's findings; HK18–HK23 (2026-10-15) the follow-ups;
  HK24–HK33 ("HK, part 3", 2026-10-15) the rest of the housekeeping;
  HK36–HK37 ("HK, part 4", 2026-10-16): the bn / bybit / mexc
  `Tick.venue_seq` is the arrival ordinal keyed on the full venue id, and
  HL's empty `trades` snapshot counts on `…_trades_empty_total`, not as a
  parse error; HK39–HK40 ("HK, part 5", 2026-10-16): OKX `bbo-tbt` and
  Binance options join the ordinal, keyed on the venue `ts` and the mark
  element's `E` (Binance option touches now reach `TopOfBook`; ai-exec is
  the only reader).
- **S16, sixteen strategy slots (risk-policy "S16", 2026-10-15).** S16-1..6
  on main and deployed 2026-10-15 22:32Z: member table, `STRATEGY` parser +
  `strategy-mask` (the wrapper resolves through it; `all` refused),
  `SlotMask`, exec router / ledger (`LEDGER_RESTING` 1 024 with a
  high-water mark) / boot, the HL and Binance arms, `/state` schema 3.
  Slots 8–15 are reserved: arming one is refused (S16-R5); a member there
  needs its own ruling. Release builds have `overflow-checks =
  false`: never write `1u8 << slot`; debug tests at slots 8/11/15 guard it.
- **The S3 archive (HK11, HK18, HK20).** Uploads are paced in 64 KiB pieces
  under `MULTIVENUE_S3_MAX_UPLOAD_MIBPS`, 20 MiB/s since HK20 (the link's
  ~5 MB/s binds; the feed delays stayed near their no-upload band). An
  uncapped push (8 MiB writes) stalled the feeds on 2026-10-14. A weekday's
  capture (6.0–6.4 GB stored) takes ~20 min; the nightly budget, 8400 s, is
  a ceiling. A SIGTERM aborts the upload in flight, and is deferred across
  each multipart initiate attempt so the abort can name it (HK21, HK27). The nightly
  cycle first runs `abort-stale` (incomplete uploads over 24 h; `uploads`
  lists them). `push-derived` keys keep their path under the store's root,
  `--path` too, and candles go up zstd'd.
- **Gates at HEAD (HK, part 4, 2026-10-16):**
  - clippy clean.
  - nextest 4 754 passed (10 skipped: the five `#[ignore]`d live smokes,
    four alloc child helpers and a fixture regenerator), under nextest
    0.9.148. A random `LEAK` at ~0.2 s under nextest < 0.9.145 on macOS is
    nextest's pipe race, not the test (HK23). Only the scrape hammer runs alone (`.config/nextest.toml`):
    the alloc tests' reds under load were libtest's runner thread, no longer
    counted (A-S7-101, HK14). nextest runs no doctests: core-ring's
    `Mailbox` `compile_fail` doctest needs `cargo test --doc -p core-ring`.
  - alloc 127/127 at the gates' pins (0 B/op, +4 ignored child helpers).
    `--test-threads=1` and a fresh `Compiling bench` are required.
  - `make copy-audit`: 27 baselined, new 0 (self-test OK).
  - `make license-check` OK.
  - `make bench-check` OK: 16 samples within 15 % on the M4 (HK9, HK16); a
    missing or stale result fails.
  - worker pytest 1 786 passed, 3 skipped (the three need the release
    binary on PATH), HK27.
  - `make py-lint` all checks passed (HK8a). `make lint` means clippy.
  - fuzz: each HK cut's touched targets 60 s, clean (risk-policy "HK1",
    "HK2", "HK4"). The longer runs are in each lane's record.
  - A red is a red: the old flake list was root-caused (HK4, HK8b, HK14) or
    runs one at a time (HK8c).
- **Open operator items (non-gating):**
  - **Disk:** 26 GiB free on 2026-10-14 after the worktree cleanup. Writers
    do not retry after ENOSPC; a restart is the recovery.
  - **S3:** bucket versioning and `AbortIncompleteMultipartUpload` are not
    set; the nightly `abort-stale` (HK21) now clears what a lost initiate
    answer orphans, so the rule is a second backstop, not the only one. The
    `s3.env` credential pair was once pasted in chat: rotate it.
  - **Format drift:** ~208 Rust files and ~106 worker files outside the HK
    lane's are not rustfmt/ruff-format clean (HK28); formatting them all is
    `cargo fmt --all` in effect — the operator's call.
  - **Polymarket parse errors: resolved (HK30).** They were the venue's
    post-trade `book` re-snapshots (", " separators); 0 since 13:46Z
    2026-10-15. `RAW_TAP=pm` stays as the standing rejects tap
    (`pm-raw.tap`, ≤ 64 MiB a run). Largest frame seen: 10 152 B against the
    64 KiB RX buffer (982 584 frames, 3.5 h; risk-policy "HK, part 3").
  - **`venue_seq` (HK31, HK32):** pm, hl (`bbo` + outcome `l2Book` touch)
    and deribit (`quote`) ticks carry a per-instrument arrival ordinal
    (`core_types::ArrivalSeq`), not ms bits; 0 = an older tick, counted on
    `seq_regressions_total` (baselines pm ~0, hl ~0, deribit ~115 an hour).
    Rulings open: BIN15 O8's touch source (l2Book vs bbo); the u32
    truncation of Binance/Bybit/MEXC seqs (no live book reads them).
  - **Memory:** a whole-root `audit-pnl` / `backtest` OOMs above ~27 GB; use
    bounded ≤ 2 h window roots.
  - **Regime:** `regime.toml [labels] require = 1` is NOT flipped live.
- **Reminder: upgrade to rustls 0.24 once 0.24.0 is released** (operator,
  2026-09-26: the only rustls work needed). 0.24 decrypts records in place,
  which fixes gate 72's copy and allocation. Pinned 0.23.38; as of
  2026-10-13 only `0.24.0-dev.*` is published (`cargo info rustls@0.24.0`
  says "could not find" until the release).

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
- **Signing keys** load into an `mlock`'d page and zeroize on drop.

## Directory guide

- `crates/core-*` — primitives: ring (SPSC only), time, config (every
  `*.toml` parser), alloc, io (PMLR writer/reader, `PmlrCapture`, atomic
  state files), net (mio + rustls transport, WS framing, `IoBuf`,
  `Keepalive`), parse (byte scanners + the `pb` protobuf walker), simd,
  crypto (SHA-256/HMAC/base64), types (wire PODs, `SymbolId`, `VenueId`),
  regime, vol (the HAR `VolEngine`, the whole-day `LongVolEngine` and its
  engine owner `LongVolSet`),
  settle (the Hypercall settlement law), fill, latency, metrics (fixed
  registry, 512 counters).
- `crates/ingress-{polymarket,binance,okx,deribit,hyperliquid,bybit,mexc,rpc,hyperevm,hypercall}` —
  one thread per source, `discovery.rs` = boot REST; `crates/ingress-ai` —
  the UDS+HMAC command plane and the ruleset validator.
- `crates/strategy-{set,core,hyparb,vm,ai-exec,vrp,xsd,bin15,xmm,hcv}` — the composed
  set and its members; `strategy-{latency-arb,cross-arb,ev,rule-tree,icdp}` are
  in-tree but unlinked/off. `book-builder`, `opt-registry`,
  `options-select`, `research-artifacts`.
- `crates/engine` — the single-threaded loop; `crates/engine-snapshot` —
  the seqlock `/state` snapshot; `crates/tui`.
- `crates/exec-router` + `crates/exec-hyperliquid` + `crates/signer-eip712`
  + `crates/clob-dispatcher` — the execution lane (see CURRENT STATE).
  `crates/exec-gateway` — the venue-agnostic async order-gateway kit (XMM
  XH4a: duplex rings, in-flight table, deadline queue, make-before-break).
  Its first user is `exec_hyperliquid::gateway` (XH4b: the HL WebSocket-
  post gateway thread — built and loopback-tested, wired to the engine in
  XH4f); `exec-hyperliquid`'s test-only feature `testvenue` is a scripted
  rustls Hyperliquid (never in a shipping build).
  `crates/exec-binance` + `crates/signer-ed25519` — the Binance arm (BX1–BX13:
  USDⓈ-M, COIN-M, spot, options, Stocks; its own gateway on the same kit;
  `docs/binance-runbook.md` holds the operator's procedures).
  `crates/exec-hypercall` — the Hypercall order arm (HC9, O-HC19): driven
  only by the operator's `hypercall-live` verbs
  (`scripts/hypercall-live.sh`; mainnet writes need `--confirm`); the
  engine refuses a live `hypercall` slot until slot 7's live-arming
  ruling (risk-policy "HYPERCALL — the order arm").
  `crates/signer-evm` + `crates/exec-hyperevm` — HYPARB's HyperEVM write
  path: configuration arms TESTNET only (`EVM_ARM_CHAIN_IDS = [998]`,
  compile-time asserted); mainnet only through a `MainnetAuthority`
  (L1: the operator's `evm-live` verbs; L5: the engine's three
  switches; `docs/hyparb-build-plan.md` §11, §16.14, §17.4, §17.5).
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
  `risk-reviewer` on Opus 5.5, `parser-property-tester` on Sonnet), commands, hooks.
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

## Preferred Claude models

- Bulk artifact generation: `MODEL_BULK = "claude-haiku-4-5"`. Reasoning:
  `MODEL_REASONING = "claude-sonnet-5"`. Strategy proposals (`serve`
  strategist): `MODEL_STRATEGIST = "claude-fable-5-1"` (needs `anthropic >=
  1.4.0`). NEWS tier-3 event analyst (`serve` only): `MODEL_ANALYST =
  "claude-opus-5"`. Hard work — reviews, architecture, the three review agents:
  Opus 5.5 (`claude-opus-5-5`, ruling O-8 2026-09-23; the agents' frontmatter
  pins, not a worker constant).
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
