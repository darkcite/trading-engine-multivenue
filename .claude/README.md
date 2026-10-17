# .claude/

Project-local Claude Code config for the multivenue trading engine.

## Layout

- `settings.json` — main model, tool permissions (the git and `.env` laws as
  `deny` / `ask` rules), env, the status line, hooks. Claude Code CLI only:
  Cowork sessions do not load it, so the laws in `CLAUDE.md` stay the guard there.
- `agents/` — specialized subagents invoked via the Task tool.
- `commands/` — custom slash commands (e.g. `/run-paper`, `/alloc-check`).
- `hooks/` — `no-forbidden-crates.sh` (PreToolUse Write|Edit: banned crates,
  `from x import y`), `guard-bash.py` (PreToolUse Bash: push / rebase /
  branch / bulk staging / `cargo fmt --all` / `make fmt` / unbracketed
  `pkill -f` / reading `.env`), `post-edit-fmt.sh` (PostToolUse formatter).
- `statusline.sh` — model and context use (needs `jq`; falls back to a label).
- `skills/` — optional reusable skills for this project.

## Editing rules

Do not add `tokio`, `serde_json`, `ethers`, `alloy`, `reqwest`, or any cloud
SDK to the Rust workspace. The `no-forbidden-crates.sh` hook blocks edits that
introduce these.

Do not add `from x import y` to Python files. Ruff
(`isort.force-single-line`) plus a pytest test
(`tests/test_imports_are_full.py`) enforce this.

## Review agents (the three verdict agents pinned to Opus 5.5 — `claude-opus-5-5`, operator ruling O-8 2026-09-23; Opus 5 since 2026-09-19)

- `alloc-auditor` — zero-ALLOCATION gate (`make alloc-assert`).
- `zero-copy-auditor` — zero-COPY gate (`make copy-audit`,
  `scripts/copy-audit.sh` + `scripts/copy-audit-baseline.txt`). The
  rule: everything that can be done zero-copy is; an unavoidable copy
  carries a `// COPY:` line (what · bound · why · alternative rejected),
  the way `unsafe` carries `// SAFETY:`.
- `risk-reviewer` — LAWS E-1..E-9 + E-11, BX-n and each slot's section of
  `docs/risk-policy.md` (read by section, never whole; effort xhigh).
- `parser-property-tester` — proptest + fuzz for byte scanners (Sonnet 5.5).
- `Explore` — read-only code search on Haiku 5.5 (overrides the built-in,
  which runs on the main model).
- `gate-runner` — runs the merge gates on Haiku 5.5 and returns one line
  per gate.

Every agent pins `model` and `effort` in its frontmatter (routing rules:
CLAUDE.md "Model routing"). In Cowork these agent types are not
registered: spawn a general-purpose agent with the file's body as the
prompt and its pinned model.

Run the two auditors after any change to a socket, parser, encoder,
signer or ring path; run the risk reviewer before any exec-lane merge.
The three verdict agents are read-only by construction.
