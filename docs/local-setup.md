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

# Python 3.12 via uv.
curl -LsSf https://astral.sh/uv/install.sh | sh
uv python install 3.12

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
#   ANTHROPIC_API_KEY       — for the offline claude-worker only
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

- `~/polymarket/logs/engine/` — engine text logs (rotated daily).
- `~/polymarket/logs/latency/*.hgrm` — HdrHistogram dumps.
- `~/polymarket/logs/worker/` — claude-worker logs.
- `~/polymarket/artifacts/` — claude-worker output artifacts
  (topic tags, parsed rules) consumed by the engine at boot.
- `~/polymarket/replay/` — on-disk replay log (see `docs/wire-format.md`).

Nothing is written outside `~/polymarket/` or the project directory.

## Troubleshooting

- **Build fails with "unknown target-feature"**: the Apple Silicon target
  in `.cargo/config.toml` uses `target-cpu=apple-m1`; Intel Macs need to
  edit that line to `target-cpu=native`.
- **`alloc-assert` fails**: a change introduced an allocation in a hot
  path. Run with `-- --nocapture` to see which assertion and which
  iteration first reported a non-zero delta.
- **Engine can't read `.env`**: confirm `chmod 600 .env` and that the
  process's cwd is the project root.
