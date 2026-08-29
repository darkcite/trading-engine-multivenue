#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M5 external-strategies hourly cycle (launchd com.multivenue.carry
# target; operator "go" 2026-08-29). One cycle, aligned to the top of
# the funding hour (:02 — CVFC-1 §2 wants the decision on settled
# funding; the WS11 lane fetches, this cycle then decides):
#
#   1. claude_worker.funding      — newest funding points, all venues
#   2. claude_worker.carry_signal — CVFC-1 + S1 pilot decisions →
#                                   batch JSON + push.sh + digest
#   3. push.sh                    — ONLY when the fresh batch carries
#                                   intents AND the engine is up; every
#                                   order is a paper intent under the
#                                   §4.2 validator caps ($100/order)
#
# Modules + the push VERB — the 8-verb surface stays frozen. Overlap
# guard: ANY live claude-worker invocation wins (the global worker-
# serialization law); this cycle skips and the next hour retries —
# funding resumes by construction and carry_signal is idempotent over
# its rolling state.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

echo "carry-cycle: start $(date -u +%Y-%m-%dT%H:%M:%SZ)"

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "carry-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

CARRY_DIR="$HOME/multivenue/worker/carry"
cd claude-worker || exit 78

uv run python -m claude_worker.funding ||
  echo "carry-cycle: funding failed (non-fatal; next hour retries)" >&2

# Capture the digest path the module prints last ("[digest] <path>").
DIGEST_LINE=$(uv run python -m claude_worker.carry_signal | tail -1) || {
  echo "carry-cycle: carry_signal failed (non-fatal; next hour retries)" >&2
  exit 0
}
echo "carry-cycle: ${DIGEST_LINE}"

# Newest batch of THIS cycle: push only when it carries intents.
BATCH=$(ls -t "$CARRY_DIR"/batch-*.json 2>/dev/null | head -1)
if [ -z "${BATCH}" ]; then
  echo "carry-cycle: no batch produced" >&2
  exit 0
fi
N_INTENTS=$(python3 -c "import json,sys; print(len(json.load(open(sys.argv[1]))['intents']))" "$BATCH")
if [ "${N_INTENTS}" = "0" ]; then
  echo "carry-cycle: 0 intents — nothing to push"
  exit 0
fi
if ! pgrep -f 'multivenue-engin[e] run' >/dev/null 2>&1; then
  echo "carry-cycle: ${N_INTENTS} intents but the engine is DOWN — not pushing (state already advanced; the digest records the entry)" >&2
  exit 0
fi
echo "carry-cycle: pushing ${N_INTENTS} intent(s) from ${BATCH##*/}"
sh "$CARRY_DIR/push.sh" ||
  echo "carry-cycle: push.sh failed (transport? check engine + ai.sock)" >&2
echo "carry-cycle: done $(date -u +%Y-%m-%dT%H:%M:%SZ)"
