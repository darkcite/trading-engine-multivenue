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

## Review agents (all pinned to Opus 5 — operator ruling 2026-09-19)

- `alloc-auditor` — zero-ALLOCATION gate (`make alloc-assert`).
- `zero-copy-auditor` — zero-COPY gate (`make copy-audit`,
  `scripts/copy-audit.sh` + `scripts/copy-audit-baseline.txt`). The
  rule: everything that can be done zero-copy is; an unavoidable copy
  carries a `// COPY:` line (what · bound · why · alternative rejected),
  the way `unsafe` carries `// SAFETY:`.
- `risk-reviewer` — LAWS E-1..E-9 and `docs/risk-policy.md`.
- `parser-property-tester` — proptest + fuzz for byte scanners.

Run the two auditors after any change to a socket, parser, encoder,
signer or ring path; run the risk reviewer before any exec-lane merge.
The three verdict agents are read-only by construction.
