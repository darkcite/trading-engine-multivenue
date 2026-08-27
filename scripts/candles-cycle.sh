#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 candles.db hourly cycle (launchd com.multivenue.candles target).
# Sources .env (never inlined in plists, never echoed) and runs one
# §9.6 gap-fill cycle via the claude_worker.candles MODULE (not a
# verb — the 7-verb surface is frozen). Overlap guard: a still-running
# previous cycle wins; this one exits.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f "claude_worker.candles" >/dev/null 2>&1; then
  echo "candles-cycle: previous cycle still running — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker && exec uv run python -m claude_worker.candles
