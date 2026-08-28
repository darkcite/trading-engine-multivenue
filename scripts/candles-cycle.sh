#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 candles.db hourly cycle (launchd com.multivenue.candles target).
# Sources .env (never inlined in plists, never echoed) and runs one
# §9.6 gap-fill cycle via the claude_worker.candles MODULE, then the
# §9.8 IV digest (D3 — the C6+ cadence hookup, landed by the
# 2026-08-28 remediation plan; rolling window default 26 h; skips
# honestly on pre-manifest runs). Modules, not verbs — the 8-verb
# surface is frozen.
#
# Overlap guard: ANY live claude-worker invocation wins (the global
# worker-serialization law, not just a previous candles cycle); this
# cycle skips and the next hour retries — §9.6 gap-fill resumes by
# construction, and the digest's rolling window covers the gap.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "candles-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker || exit 78
uv run python -m claude_worker.candles ||
  echo "candles-cycle: candles failed (non-fatal; next hour retries)" >&2
# D3: the IV digest rides the same serialized window.
uv run python -m claude_worker.iv_digest ||
  echo "candles-cycle: iv_digest failed (non-fatal; next hour retries)" >&2
