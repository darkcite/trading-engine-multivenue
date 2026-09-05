# Local setup — MacBook Pro M4

This document describes the minimal environment needed to build, test, and
run the engine locally in paper mode. No cloud, no managed services.

## Prerequisites

- macOS 14.5+ on Apple Silicon (M4 preferred; M1/M2/M3 also fine).
- Xcode command-line tools: `xcode-select --install`.
- A working C toolchain — confirm with `cc --version`.
- ~10 GB free disk for `target/` and the replay log.

## Toolchain

```sh
# Rust, pinned via rust-toolchain.toml (1.83.0 at time of writing).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Python 3.14 via uv.
curl -LsSf https://astral.sh/uv/install.sh | sh
uv python install 3.14

# cargo-nextest for faster test runs.
cargo install cargo-nextest --locked

# cargo-fuzz for the fuzz targets.
cargo install cargo-fuzz --locked
```

## First-time project setup

```sh
cd ~/Documents/Claude/Projects/Polymarket

# 1. Configure secrets.
cp .env.example .env
chmod 600 .env
# Edit .env and fill in:
#   POLYMARKET_EIP712_KEY   — your Polymarket API signing key (0x-prefixed hex)
#   ANTHROPIC_API_KEY       — read by `claude-worker serve` ONLY (verbs never need it)
#   AI_INGRESS_HMAC_KEY     — 64 hex chars shared by engine + worker (8f AI lane)
#   ALCHEMY_API_KEY         — managed Polygon RPC (free tier)

# 2. Build the workspace (debug first, release next).
cargo build --workspace
cargo build --release --workspace

# 3. Run the full test matrix.
make test            # cargo nextest run --workspace
make alloc-assert    # cargo test -p bench --test alloc_assertions --release
cd claude-worker && uv sync && uv run pytest && cd ..
```

## Running the engine in paper mode

```sh
# Configure secrets via .env (the binary parses .env, not the TOML
# — `config.example.toml` is operator reference for the knobs
# you'd pass as CLI flags).
cp .env.example .env && chmod 600 .env
$EDITOR .env

# Start the engine (paper mode — no live orders).
cargo run --release -p cli -- run --paper --env-file ./.env
```

The TUI (from `crates/tui`) opens in the same terminal. Ctrl-C triggers
graceful shutdown.

## Logs and artifacts

By convention:

- `~/multivenue/logs/engine/` — engine text logs (rotated daily).
- `~/multivenue/logs/latency/*.hgrm` — HdrHistogram dumps.
- `~/multivenue/logs/worker/` — claude-worker logs.
- `~/multivenue/artifacts/` — claude-worker output artifacts
  (topic tags, parsed rules) consumed by the engine at boot;
  `~/multivenue/artifacts/rulesets/<hash128-hex>.json` — staged
  ruleset artifacts the 8f ingress-ai side path resolves.
- `~/multivenue/replay/` — on-disk replay log (see `docs/wire-format.md`).
- `~/multivenue/worker/` — 8f worker state: `state.db` (SQLite: seq,
  dedupe, prompt cache, ruleset registry), `features/` (fetch
  output), `market-map.json` (operator market map + HIP-4 pairs).
- `~/multivenue/run/ai.sock` — the 8f AI-command UDS (engine listens,
  worker connects; dir 0700, socket 0600).

Nothing is written outside `~/multivenue/` or the project directory.

## Release binary on PATH (8h backtest harness)

The worker's `backtest` verb (and `tests/test_backtest_real.py`) spawn
`multivenue-engine` by NAME — PATH resolution is the pinned contract
(phase-8h-design §14/§15.3; an absolute path stays a `.env`-commentary
option only). After any harness change:

```sh
cargo build --release -p cli               # G0 law: relink before use
export PATH="$PWD/target/release:$PATH"    # or symlink into ~/bin
```

Without the release binary on PATH the real-harness pytest module
auto-skips (green, with a skip reason naming this runbook).

## Venue latency calibration (per deployment, per location — mandatory)

The harness's activation-Δ table (`crates/cli/src/backtest.rs`,
`ModelParams::default()`) is a **measurement of this host on this
network**, not a constant. On a new box, a new region, a new ISP or
behind a VPN, re-measure before trusting any backtest or audit-pnl
number:

```sh
cd claude-worker
uv run python -m claude_worker.latency_probe \
    --out ~/multivenue/research/latency-$(date -u +%F) --minutes 25
```

The module prints a per-venue table (TCP / TLS / kept-alive request RTT
to the REST edge, venue-clock offset, feed delay per stream) and writes
`summary.json` + per-message NDJSON. Derive `Δ_venue = feed delay p50 +
request RTT p50 / 2`, update the defaults, and record the run in
`docs/venue-latency.md`. Full procedure and rationale live there.

macOS note: the host clock is typically 50–70 ms off NTP (`sntp
time.apple.com`); the probe corrects feed delays with each venue's own
time endpoint, so never compare raw venue timestamps to `time.time()`.

## claude-worker (Phase 8f: serve daemon + operator verbs)

Python 3.14 via uv (`cd claude-worker && uv sync`). Two modes over one
code path (design §5.2):

```sh
# FULL-AUTO daemon (the only mode that reads ANTHROPIC_API_KEY):
cd claude-worker && uv run claude-worker serve

# SEMI-MANUAL operator verbs (no SDK, BaseConfig only):
uv run claude-worker fetch --news
uv run claude-worker backtest --ruleset R.json
uv run claude-worker positions --json
uv run claude-worker push --kind set-bias --sym 7 --px 0.02 --ttl-s 900
uv run claude-worker stage-ruleset --ruleset R.json --report R.report.json
uv run claude-worker commit-ruleset --ruleset R.json
```

Worker env keys (`.env.example` documents all): `AI_INGRESS_SOCK`,
`AI_INGRESS_HMAC_KEY`, `AI_RULESET_DIR`, `CLAUDE_WORKER_REPLAY_DIR`
(required — point at the engine `MULTIVENUE_LOG_DIR`),
`CLAUDE_WORKER_DB`, `CLAUDE_WORKER_FEATURES_DIR`,
`CLAUDE_WORKER_MARKET_MAP`, `RSS_FEEDS` (worker-only), and the 8h
research-loop keys `CLAUDE_WORKER_STRATEGIST_INTERVAL_S`,
`CLAUDE_WORKER_STRATEGIST_DAILY_CAP`,
`CLAUDE_WORKER_REST_BUDGET_PER_H` (design §7.5; the REST budget is
consumed by `fetch`, the strategist pair from H4). The semi-manual
playbook is `docs/prompts/ai-session.md`.

## Always-on standing engine (M3 data-ops lane)

One launchd-supervised paper engine on the full universe, restarted
gracefully at every UTC midnight → one run dir per UTC day (gap-free
days by construction; `capture-catalog` is the judge). **ONE ENGINE
EVER**: once installed, this instance IS the standing engine.

```sh
# install / reinstall (idempotent; reinstall = graceful restart)
./scripts/install-launchd.sh

# status / live tail
launchctl print gui/$UID/com.multivenue.engine | grep -E "state|pid"
tail -f ~/multivenue/logs/launchd/engine.out.log

# coverage truth (any time)
./target/release/multivenue-engine capture-catalog --dir ~/multivenue/logs
```

Pieces (templates in `launchd/`, rendered by the installer):

- `com.multivenue.engine` — KeepAlive; runs
  `scripts/engine-wrapper.sh`: one-engine pgrep guard → source `.env`
  (values never inlined in plists, never echoed) → best-effort
  `claude_worker.universe_refresh` (Gamma re-resolve of the PM
  up/down dailies from `~/multivenue/pm-dailies.toml` — today before
  16:00Z, else tomorrow; failure boots on the existing
  `universe.toml`) → `exec … run --paper --strategy all`.
- `com.multivenue.daily-restart` — 60 s poller; on a new UTC day
  SIGTERMs the engine (M1d-proven drain); KeepAlive relaunches
  through the wrapper. `StartInterval`, not calendar: launchd
  calendars are LOCAL-time (DST) and a slept-through midnight fires
  on wake instead.
- `com.multivenue.caffeinate` — `caffeinate -s -i` (no system/idle
  sleep on AC). For lid-closed operation also run the operator-level
  `sudo pmset -c sleep 0 && sudo pmset -a disablesleep 1` (revert
  with `disablesleep 0`), or keep the lid open on AC.

Operational laws:

- **M2 smoke windows** (or any manual boot): stop the standing lane
  first, restart it after —
  `launchctl bootout gui/$UID/com.multivenue.engine` (SIGTERM drain)
  … smoke … `launchctl bootstrap gui/$UID
  ~/Library/LaunchAgents/com.multivenue.engine.plist`. The wrapper's
  pgrep guard self-heals if the order is fumbled: the standing lane
  backs off while a foreign engine lives and resumes when it exits.
- **Relink law (G0)**: the wrapper never builds. Deploy = `cargo
  build --release -p cli`, then `launchctl kickstart -k` is WRONG
  (SIGKILL) — use `pkill -TERM -f "multivenue-engine run"`; KeepAlive
  relaunches on the new binary.
- **Worker verbs stay manual** and globally serialized (session law):
  the wrapper runs only the refresh MODULE (file rewrite, no state.db
  writes). After a notable universe change, run `uv run claude-worker
  fetch` once (`unresolved=0` is the done-tell).
- Uninstall: `for l in engine daily-restart caffeinate candles regime dashboard; do launchctl
  bootout gui/$UID/com.multivenue.$l; done` (+ delete the plists from
  `~/Library/LaunchAgents`).
- **Dashboard** (RG6, `docs/regime-and-dashboard-plan.md` §6.2):
  `com.multivenue.dashboard` (KeepAlive, installed by the same
  installer) runs `scripts/dashboard.sh` → the read-only operator page
  on <http://127.0.0.1:9292/> — the engine's 1 s `/state` snapshot
  (proxied same-origin from 9191, 2 s) beside the worker's state
  (rulesets, library + evidence, compositions, regime history +
  declarations + bands, nightly P&L, positions from the current run's
  fills, configs — never `.env`; 10 s). One file, no CDN, no controls.
  `curl -s 127.0.0.1:9191/state | python3 -m json.tool` is the raw
  engine view; `cd claude-worker && uv run python -m
  claude_worker.dashboard --once` prints the worker document. The
  server's cmdline is deliberately `~/multivenue/venv/bin/python3
  scripts/dashboard-serve.py` (a venv alias + a repo-root launcher):
  the lanes' overlap guard `pgrep -f 'claude[-_]worke[r]'` must never
  see a long-running server, or every lane (the boot-time recommit
  included) waits forever. Log: `~/multivenue/logs/launchd/dashboard.log`.
- **Retention** (`scripts/retention.sh`, runs once per UTC day from
  the restart poller; config `~/multivenue/retention.conf`, see
  `retention.conf.example`): KEEP-ALL until the log volume's free
  space drops under `MIN_FREE_GIB` (default 25), then the oldest run
  dirs are compressed (`tar -cz`, bsdtar-internal gzip — no external
  tools) into `~/multivenue/archive/` until `TARGET_FREE_GIB` (40) is
  free again — never the newest run dir, never anything younger than
  `PROTECT_DAYS` (7), archives never auto-deleted. `capture-catalog`
  reports per-run sizes; restoring =
  `tar -xzf archive/run-<ns>.tar.gz -C ~/multivenue/logs/`.
- **candles.db** (mvp-plan §9.4–§9.6; `claude_worker.candles` MODULE
  — never a verb; hourly `com.multivenue.candles` agent via
  `scripts/candles-cycle.sh`): worker-owned SQLite WAL at
  `~/multivenue/worker/candles.db`, PK
  `(venue, descriptor, tf, open_ts)`, descriptors in the map-name
  convention (`binance:btcusdt`, `binance-usdm:btcusdt`, …). Fetched
  bases ONLY: 1m (48 h) / 1h (90 d) / 1d (listing lifetime; OKX
  bounded 400 d). §9.6 gap-fill resumes from `max(open_ts)` per
  cycle under `CLAUDE_WORKER_CANDLES_BUDGET_PER_H` (30/venue —
  deliberately under the fetch verb's 60). Closed rest bars are
  immutable; disagreements land in `candle_conflicts`. Inspect:
  `sqlite3 ~/multivenue/worker/candles.db "SELECT descriptor, tf,
  count(*) FROM candles GROUP BY 1,2"`. Manual cycle:
  `./scripts/candles-cycle.sh` (skips itself if one is running).
  Each cycle also runs the §9.5/§9.7 tail: 5m/15m/4h derived exactly
  from the stored bases (source=derived, cached, complete closed
  windows only); PM capture-derived 1m mid-OHLC + tick-count (`n`
  column, volume NULL — never fabricated) from the replay root; and
  the Binance REST-vs-socket drift report (WARN over
  `CLAUDE_WORKER_CANDLES_DRIFT_WARN_BPS`). One-shot full-history PM
  fold: `uv run python -m claude_worker.candles --capture-backfill`.
- **Regime lane** (RG5, `docs/regime-and-dashboard-plan.md` §5.1;
  `claude_worker.regime` MODULE — never a verb; 5-minute
  `com.multivenue.regime` agent via `scripts/regime-cycle.sh`, installed
  by the same installer): honest no-op until `~/multivenue/regime.toml`
  exists (copy `regime.toml.example`, set `[breadth] members` to
  descriptors that exist in `universe.toml`; the engine reads it at its
  next restart). Each cycle measures the worker's words over
  `candles.db` (judged at the last minute the store holds — the candles
  lane is hourly, so `age_min` is normal), appends the 24 h history
  under `~/multivenue/worker/regime/`, and once per UTC day rewrites
  ONLY the six RV/funding percentile lines of `regime.toml` (`.bak`
  kept). The cycle NEVER declares. Operator lanes:
  `uv run python -m claude_worker.regime report` (measured words + raw
  values, declaration in force, engine words from `/metrics`, 24 h
  timeline), `… history`, `… refresh-params [--dry-run]`,
  `… declare --fast "trend:bull,shape:trend" [--slow measured] --ttl 900`
  (persists `declared.json` + one `SetRegime` frame per profile; a
  session-serialized socket verb like every worker invocation), and
  `… repush` (re-send the persisted declaration with its remaining TTL
  — `recommit` does this after every boot's re-commit). `serve` runs the
  same measurement as its `_REGIME` phase and auto-confirms it unless a
  fresher operator/strategist ruling is in force (§7 gate: no `serve`
  yet). The nightly `pnl_report` merges the harness's per-regime
  section (`regime` key + `regime …` summary lines; `pnl` prints them).

## Troubleshooting

- **Build fails with "unknown target-feature"**: the Apple Silicon target
  in `.cargo/config.toml` uses `target-cpu=apple-m1`; Intel Macs need to
  edit that line to `target-cpu=native`.
- **`alloc-assert` fails**: a change introduced an allocation in a hot
  path. Run with `-- --nocapture` to see which assertion and which
  iteration first reported a non-zero delta.
- **Engine can't read `.env`**: confirm `chmod 600 .env` and that the
  process's cwd is the project root.
