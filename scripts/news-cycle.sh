#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# NEWS aggregation cycle (launchd com.multivenue.news target, every 60 s;
# NEWS spec §12). Sources .env (never inlined in a plist, never echoed)
# and runs ONE `claude_worker.news cycle`: every due source fetched,
# parsed, tier-0'd and stored under ~/multivenue/worker/news/. A MODULE,
# not a verb — the 8-verb surface is frozen.
#
# NO MODEL IS REACHED HERE. Tier 0 is free; the cascade runs inside
# `serve` (Stage 3) and from the operator's own session lanes.
#
# Absent artifact = honest no-op (exit 0) — nothing configured to poll.
#
# Overlap guard: ANY live claude-worker invocation wins (the global
# worker-serialization law), INCLUDING `serve` — at Stage 3 the
# aggregator runs inside serve and this job becomes a no-op by design.
# StartInterval jobs need no label self-removal.
#
# CMDLINE LAW: this cycle's argv carries `claude_worker` and it exits in
# under 30 s (the lane stops taking new sources at 25 s), so it can never
# be the long-running process that blocks another lane's guard.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if [ ! -f "$HOME/multivenue/news.toml" ]; then
  exit 0
fi

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "news-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker || exit 78
uv run python -m claude_worker.news cycle ||
  echo "news-cycle: cycle failed (non-fatal; next slot retries)" >&2
