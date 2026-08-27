#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 always-on engine wrapper (docs/m3-progress.md; mvp-plan §4-M3).
# launchd ProgramArguments target — every engine boot flows through
# here (RunAtLoad, KeepAlive relaunches, post-daily-restart).
#
# Laws:
# * .env is SOURCED (values never inlined in the plist, never echoed).
# * Universe refresh is BEST-EFFORT (claude_worker.universe_refresh,
#   a module, not a verb): failure boots on the existing file.
# * ONE ENGINE EVER: another live instance (e.g. an M2 smoke window)
#   makes this boot back off; KeepAlive retries until the lane is
#   free, so the standing engine resumes by itself.
# * G0 relink law: this script NEVER builds. It runs the release
#   binary as currently linked — `cargo build --release -p cli` is a
#   deliberate operator act; the next (re)start picks it up.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78

# launchd agents start with a minimal PATH — uv lives outside it.
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f "multivenue-engine run" >/dev/null 2>&1; then
  echo "engine-wrapper: another multivenue-engine is live — backing off" >&2
  sleep 30
  exit 1
fi

# Source .env for the worker refresh (engine loads it itself via
# dotenvy from the working directory). Values are never printed.
if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

( cd claude-worker && uv run python -m claude_worker.universe_refresh ) ||
  echo "engine-wrapper: universe refresh failed — booting with existing universe.toml" >&2

exec ./target/release/multivenue-engine run --paper --strategy all
