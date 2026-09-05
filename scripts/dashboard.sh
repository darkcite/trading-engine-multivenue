#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# RG6 dashboard server (launchd com.multivenue.dashboard target, KeepAlive;
# docs/regime-and-dashboard-plan.md §6.2). Serves the read-only operator
# page on 127.0.0.1:9292 from the worker venv. Sources .env (never
# inlined in plists, never echoed) for the worker paths
# (CLAUDE_WORKER_REPLAY_DIR, CLAUDE_WORKER_DB, …).
#
# CMDLINE LAW: every worker lane's overlap guard is
#   pgrep -f 'claude[-_]worke[r]'
# and a long-running server whose cmdline carried "claude_worker" or
# "claude-worker" (a `-m claude_worker.dashboard`, the venv's own path
# `claude-worker/.venv/bin/python3`) would block EVERY lane forever —
# including the boot-time recommit that restores the live ruleset. So:
#   * the venv is aliased as ~/multivenue/venv (a directory symlink —
#     Python finds pyvenv.cfg through the unresolved path, so the alias
#     IS the venv), and
#   * the entry point is scripts/dashboard-serve.py (repo root scripts/,
#     not claude-worker/).
# The resulting cmdline `~/multivenue/venv/bin/python3 …/scripts/
# dashboard-serve.py` matches no guard. Verify after install:
#   pgrep -f 'claude[-_]worke[r]'   → prints nothing while the page serves.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

VENV="$REPO/claude-worker/.venv"
ALIAS="$HOME/multivenue/venv"
if [ ! -x "$VENV/bin/python3" ]; then
  echo "dashboard: worker venv missing at $VENV — run 'uv sync' in claude-worker/" >&2
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

exec "$ALIAS/bin/python3" "$REPO/scripts/dashboard-serve.py"
