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
`CLAUDE_WORKER_MARKET_MAP`, `RSS_FEEDS` (worker-only). The
semi-manual playbook is `docs/prompts/ai-session.md`.

## Troubleshooting

- **Build fails with "unknown target-feature"**: the Apple Silicon target
  in `.cargo/config.toml` uses `target-cpu=apple-m1`; Intel Macs need to
  edit that line to `target-cpu=native`.
- **`alloc-assert` fails**: a change introduced an allocation in a hot
  path. Run with `-- --nocapture` to see which assertion and which
  iteration first reported a non-zero delta.
- **Engine can't read `.env`**: confirm `chmod 600 .env` and that the
  process's cwd is the project root.
