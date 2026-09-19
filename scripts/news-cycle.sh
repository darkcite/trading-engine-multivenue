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
# Overlap guard, ONE WAY: ANY live claude-worker invocation wins (the
# global worker-serialization law), INCLUDING `serve` — at Stage 3 the
# aggregator runs inside serve and this job becomes a no-op by design.
# StartInterval jobs need no label self-removal.
#
# CMDLINE LAW, and why this one is asymmetric: a cycle is network-bound
# and runs 25-31 s of its slot (40-50 sources fetched in series), so a
# cmdline carrying `claude_worker` would make the 5-minute regime cycle
# skip roughly half its slots and the hourly candles cycle skip whenever
# the two met. It therefore execs `scripts/news-cycle-run.py` through the
# venv ALIASED at ~/multivenue/venv, exactly as `dashboard.sh` does, so
# its argv carries no `claude_worker`/`claude-worker` substring at all.
# Verify after install:  pgrep -f 'claude[-_]worke[r]'  prints nothing
# while a cycle is running.
#
# That is only safe because this lane SHARES NO WRITER: `news.db` is the
# only SQLite connection in `claude_worker.news`, and the seq namespace
# the law protects lives in `state.db`, which this lane never opens. So:
# it yields to every other lane and blocks none of them. It still refuses
# to overlap a news cycle of its own.
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

if pgrep -f 'news-cycle-ru[n]\.py' >/dev/null 2>&1; then
  echo "news-cycle: the previous cycle has not finished — skipping" >&2
  exit 0
fi

VENV="$REPO/claude-worker/.venv"
ALIAS="$HOME/multivenue/venv"
if [ ! -x "$VENV/bin/python3" ]; then
  echo "news-cycle: worker venv missing at $VENV — run 'uv sync' in the worker dir" >&2
  exit 78
fi
mkdir -p "$HOME/multivenue"
if [ "$(readlink "$ALIAS" 2>/dev/null)" != "$VENV" ]; then
  rm -f "$ALIAS"
  ln -s "$VENV" "$ALIAS"
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

"$ALIAS/bin/python3" "$REPO/scripts/news-cycle-run.py" ||
  echo "news-cycle: cycle failed (non-fatal; next slot retries)" >&2
