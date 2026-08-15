# claude-worker

AI-ingress worker for the multivenue trading engine (Phase 8f rewrite).
One library core, two frontends over the same code path:

- **`claude-worker serve`** — the only daemon mode (full-auto): news watcher,
  strategist, backtester, commander on cadences. The only mode that reads
  `ANTHROPIC_API_KEY` and constructs an SDK client.
- **Operator verbs** (semi-manual) — `fetch`, `backtest`, `push`, `positions`,
  `stage-ruleset`, `commit-ruleset`. No daemon, no SDK client; a Claude
  session (primed by `docs/prompts/ai-session.md`) is the reasoning brain.

Frames go to the engine over a UDS socket as 82-byte HMAC-tagged `AiCmd`
frames; gates for ruleset stage/commit live in code, not prompts, and bind
identically in both modes. See `docs/phase-8f-design.md` for the authority.

Python 3.14, uv-managed. Run tests: `uv run pytest`.
Convention: full `import x` only — never `from x import y` (enforced by test).
