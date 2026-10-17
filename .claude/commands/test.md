---
description: Run the merge gates (lint, nextest, alloc, copy-audit, license, bench, worker pytest + ruff) through the gate-runner agent and report one line per gate
allowed-tools: Agent, Bash(make *), Bash(cargo nextest*), Bash(cargo test*), Bash(uv run *), Bash(pgrep *)
---

Run the full gate battery. Delegate it to the `gate-runner` agent (Haiku)
so the logs stay out of this context; if that agent type is not available
(Cowork), spawn a general-purpose agent with the body of
`.claude/agents/gate-runner.md` as its prompt and `model: haiku`.

Gates, in order: `make lint` · `cargo nextest run --workspace` ·
`make alloc-assert` (one thread, fresh `Compiling bench`) · `make copy-audit`
(`new=0`) · `make license-check` · `make bench-check` ·
`cd claude-worker && uv run pytest` (only if `pgrep -f pytes[t]` and
`pgrep -f 'claude[-_]worke[r]'` are empty) · `make py-lint`.

There is no `cargo fmt --check` gate: HEAD was never fmt-clean (HK28).

## Output format

`GATE <name>: PASS|FAIL|STALE|SKIPPED — <headline>` per gate; the last 30
lines of each FAIL. Claim success only if every gate is PASS. Do not
auto-fix.
