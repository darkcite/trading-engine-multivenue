#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M5-prep #7b post-boot ruleset re-commit (operator ruling 7(b);
# remediation plan 2026-08-28). Fired by engine-wrapper.sh on every
# boot as a backgrounded, interpreter-invoked child. The actual law
# lives in claude_worker/recommit.py (a MODULE, never a verb — the
# 8-verb surface is frozen; iv_digest precedent): wait for ai.sock,
# look up the most recently COMMITTED gates-passed registry row,
# re-stage it from its bound paths, re-commit its hash. No committed
# row = honest no-op.
#
# Serialization: (a) only one waiter — a concurrent instance (rapid
# KeepAlive churn) yields; (b) the worker law — any live
# claude-worker invocation defers us (bounded wait, then give up;
# the next boot retries).
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

echo "recommit: boot waiter starting $(date -u +%Y-%m-%dT%H:%M:%SZ)" >&2

# (a) single-waiter guard: any OTHER live instance of this script
# wins (bracketed pattern per the pkill session fact; filter our own
# pid — pgrep -f matches this shell's own cmdline too).
others="$(pgrep -f 'recommit-rulese[t].sh' 2>/dev/null | grep -vx "$$" || true)"
if [ -n "$others" ]; then
  echo "recommit: another waiter is live (pids: $others) — yielding" >&2
  exit 0
fi

# Give the engine a head start so the sock exists quickly; the
# module's own --wait-sock-seconds does the precise waiting.
sleep 10

# (b) worker-serialization wait: bounded at ~5 min, 5 s cadence.
i=0
while pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "recommit: worker busy > 5 min — giving up (next boot retries)" >&2
    exit 0
  fi
  sleep 5
done

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker || exit 78
exec uv run python -m claude_worker.recommit --wait-sock-seconds 180
