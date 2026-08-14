# claude-worker

Offline Python 3.12 process that uses the Anthropic SDK to do **non-hot-path**
strategy-research work for the Rust latency-arb engine. This process is
**never** invoked from the engine's hot path. All interaction with the engine
happens via files on disk (artifacts produced here are consumed at engine boot).

## What it does

- **Bulk topic tagging** of historical ticks / news payloads using `claude-haiku-4-5`.
- **Rule parsing** — turning natural-language research notes into structured
  strategy rules — using `claude-sonnet-4-6`.
- **Backtest review** (Phase 6+) using `claude-opus-4-6`.

## What it does NOT do

- No live Anthropic calls in tests (all calls are mocked at the SDK boundary).
- No network from the engine's hot path.
- No `from x import y` — codebase rule. Use full `import x` only.

## Setup

```sh
uv sync            # install deps + dev deps
uv run pytest      # run the test suite
uv run ruff check  # lint
uv run mypy src    # type-check
```

## Configuration

Secrets come from the project-root `.env` file. The worker reads
`ANTHROPIC_API_KEY` at startup and fails fast if it is missing.
