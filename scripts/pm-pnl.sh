#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# The PMJ P&L lane (rulings R-PMJ-S27..S30; launchd com.multivenue.pm-pnl at
# :06, :21, :36 and :51, and at load — a 15-minute window resolves +116..218 s
# after its end). One pass of claude_worker.pm_paper_pnl over main's captures:
#
#   reads     ~/multivenue/logs/run-*/{engine-fills,pm15-events,pm5-events}.pmlr
#             + instrument-manifest.tsv; Gamma's public markets endpoint for a
#             window the capture never saw resolve
#   writes    ~/multivenue/worker/pm/paper-pnl.tsv   the append-only settled ledger
#             ~/multivenue/worker/pm/open.tsv        the fills not yet settled
#             ~/multivenue/worker/pm/paper-pnl.json  the summary the dashboard reads
#
# It never touches the engine and needs no secret: .env is NOT sourced.
#
# CMDLINE LAW (as scripts/dashboard.sh): every worker lane's overlap guard is
#   pgrep -f 'claude[-_]worke[r]'
# so the pass runs through the venv alias (~/multivenue/venv) and the
# repo-root launcher scripts/pm-pnl.py — no argv names the worker.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78

VENV="$REPO/claude-worker/.venv"
ALIAS="$HOME/multivenue/venv"
if [ ! -x "$VENV/bin/python3" ]; then
  echo "pm-pnl: worker venv missing at $VENV — run 'uv sync' in claude-worker/" >&2
  exit 78
fi
mkdir -p "$HOME/multivenue"
if [ "$(readlink "$ALIAS" 2>/dev/null)" != "$VENV" ]; then
  rm -f "$ALIAS"
  ln -s "$VENV" "$ALIAS"
fi

exec "$ALIAS/bin/python3" "$REPO/scripts/pm-pnl.py" --root "$HOME/multivenue"
