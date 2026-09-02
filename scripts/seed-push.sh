#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# VM2 V8: post-boot seed push (D-1 funding prints + D-2 position
# restores). NOT WIRED into launchd yet — the V8 runbook (vm2-plan §9)
# carries the one-line engine-wrapper.sh hookup, applied on operator
# order. Safe while the VM is inert: FundingSeeds fold into feature
# windows regardless of table state; PositionSeeds only flow when
# MULTIVENUE_SEED_RULESET names the committed artifact.
#
# Waits for the engine socket + the fresh run's instrument manifest
# (both appear within seconds of boot), then runs the seeds MODULE
# once (module, not verb — the 8-verb surface is frozen). Serialized
# like every worker invocation.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
SOCK="${AI_INGRESS_SOCK:-$HOME/multivenue/run/ai.sock}"
LOGS="${MULTIVENUE_LOG_DIR:-$HOME/multivenue/logs}"
RULESET="${MULTIVENUE_SEED_RULESET:-}"

cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

# Engine + manifest wait (boot takes seconds; give it two minutes).
i=0
while [ $i -lt 24 ]; do
  newest=$(ls -d "$LOGS"/run-* 2>/dev/null | sort | tail -1)
  if [ -S "$SOCK" ] && [ -n "$newest" ] && [ -f "$newest/instrument-manifest.tsv" ]; then
    break
  fi
  sleep 5
  i=$((i + 1))
done
if [ ! -S "$SOCK" ]; then
  echo "seed-push: engine socket never appeared — giving up (next boot retries)" >&2
  exit 0
fi

# Worker serialization law — but WAIT rather than skip-once (VM2 V8
# outage review 2026-09-02: two boots lost their seed push to a
# transient collision). Up to 5 minutes, 30 s poll.
j=0
while pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; do
  if [ $j -ge 10 ]; then
    echo "seed-push: a worker invocation stayed live 5 min — skipping (next boot retries)" >&2
    exit 0
  fi
  sleep 30
  j=$((j + 1))
done

cd claude-worker || exit 78
if [ -n "$RULESET" ]; then
  uv run python -m claude_worker.seeds --ruleset "$RULESET" ||
    echo "seed-push: seeds failed (non-fatal)" >&2
else
  uv run python -m claude_worker.seeds ||
    echo "seed-push: seeds failed (non-fatal)" >&2
fi
