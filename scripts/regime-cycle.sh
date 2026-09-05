#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# RG5 regime cycle (launchd com.multivenue.regime target, every 5 min;
# docs/regime-and-dashboard-plan.md §5.1). Sources .env (never inlined
# in plists, never echoed) and runs ONE `claude_worker.regime cycle`:
# the worker-measured words over candles.db appended to the 24 h
# history under ~/multivenue/worker/regime/, plus the daily percentile
# refresh of ~/multivenue/regime.toml (RV / funding p30/p70 — the
# engine applies them at its next T2 restart). A MODULE, not a verb —
# the 8-verb surface is frozen. The cycle NEVER declares: a
# declaration is the AI's / operator's call (`regime declare`).
#
# Absent artifact = honest no-op (exit 0) — nothing to measure.
#
# Overlap guard: ANY live claude-worker invocation wins (the global
# worker-serialization law); this cycle skips and the next slot
# retries. StartInterval jobs need no label self-removal.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if [ ! -f "$HOME/multivenue/regime.toml" ]; then
  exit 0
fi

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "regime-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker || exit 78
uv run python -m claude_worker.regime cycle ||
  echo "regime-cycle: cycle failed (non-fatal; next slot retries)" >&2
