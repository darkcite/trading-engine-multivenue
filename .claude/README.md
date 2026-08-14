# .claude/

Project-local Claude Code config for the multivenue trading engine.

## Layout

- `settings.json` — model routing, tool permissions, env, hooks.
- `agents/` — specialized subagents invoked via the Task tool.
- `commands/` — custom slash commands (e.g. `/run-paper`, `/alloc-check`).
- `hooks/` — shell scripts that enforce hard rules around edits.
- `skills/` — optional reusable skills for this project.

## Editing rules

Do not add `tokio`, `serde_json`, `ethers`, `alloy`, `reqwest`, or any cloud
SDK to the Rust workspace. The `no-forbidden-crates.sh` hook blocks edits that
introduce these.

Do not add `from x import y` to Python files. Ruff
(`isort.force-single-line`) plus a pytest test
(`tests/test_imports_are_full.py`) enforce this.
