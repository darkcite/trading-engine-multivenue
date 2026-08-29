#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M5 xv position-aware cross-venue reversion, 5-minute cycle
# (com.multivenue.xv; operator rulings 2026-08-29: 5-min cadence,
# $10k legs, $50k research tier). Reads LIVE capture tick tails —
# no REST, no fetch dependency; pushes only when the fresh batch
# carries intents AND the engine is up. Worker-serialization pgrep
# guard (any live claude-worker invocation wins; next cycle retries —
# state is rolling, a skipped cycle just re-evaluates).
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "xv-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

XV_DIR="$HOME/multivenue/worker/xv"
cd claude-worker || exit 78

DIGEST_LINE=$(uv run python -m claude_worker.xv_signal | tail -1) || {
  echo "xv-cycle: xv_signal failed (non-fatal; next cycle retries)" >&2
  exit 0
}
echo "xv-cycle: ${DIGEST_LINE}"

BATCH=$(ls -t "$XV_DIR"/batch-*.json 2>/dev/null | head -1)
[ -z "${BATCH}" ] && exit 0
N=$(python3 -c "import json,sys; print(len(json.load(open(sys.argv[1]))['intents']))" "$BATCH")
if [ "${N}" = "0" ]; then
  exit 0
fi
if ! pgrep -f 'multivenue-engin[e] run' >/dev/null 2>&1; then
  echo "xv-cycle: ${N} intents but the engine is DOWN — not pushing" >&2
  exit 0
fi
echo "xv-cycle: pushing ${N} intent(s) from ${BATCH##*/}"
sh "$XV_DIR/push.sh" ||
  echo "xv-cycle: push.sh failed (transport?)" >&2
