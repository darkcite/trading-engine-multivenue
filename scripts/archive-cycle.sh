#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# Stage A of the S3 archive lane: push every CLOSED capture run that is not yet
# complete in the bucket, oldest first, under a wall budget. Run once a day by
# launchd (com.multivenue.archive) at a quiet local hour.
#
# Why this is its OWN agent rather than a step inside retention.sh:
#
#   * daily-restart.sh runs every 60 s and calls retention.sh SYNCHRONOUSLY at
#     the 0000Z slot. launchd will not start a second copy while one is running,
#     so a slow upload there delays every later slot — including the 0020Z pnl
#     report that the G2 gate accumulates from.
#   * CMDLINE LAW: the boot-time ruleset recommit waits on
#     `pgrep -f 'claude[-_]worke[r]'` for five minutes and then gives up until
#     the next boot, leaving vm_rows_active 0 for hours. A venv path containing
#     `claude-worker` inside the restart lane would trip exactly that. Hence the
#     ~/multivenue/venv alias plus the repo-root launcher: neither string
#     matches the guard.
#
# Uploading never deletes anything. Deletion is retention.sh's stage B, and only
# after `verify` says the bucket holds the run.
set -u

REPO="${MULTIVENUE_REPO:-${0:A:h:h}}"
cd "$REPO" || exit 78

# launchd hands us a minimal PATH and no .env. The archiver reads its own
# credential file, so we need PATH only for `capture-catalog`, which the
# manifest embeds so the agent can answer coverage questions without pulling a
# byte of PMLR. Its absence is recorded, never fatal.
export PATH="$REPO/target/release:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

CONF="$HOME/multivenue/retention.conf"
S3_CYCLE_BUDGET_S=600
ARCHIVE_MODE="compress"
[ -f "$CONF" ] && . "$CONF"

# An honest no-op: the same switch that disables stage B disables stage A, so
# there is exactly one lever and rollback cannot half-apply.
[ "$ARCHIVE_MODE" = "s3" ] || exit 0

VENV="$REPO/claude-worker/.venv"
ALIAS="$HOME/multivenue/venv"
[ -x "$VENV/bin/python3" ] || { echo "archive-cycle: venv missing" >&2; exit 78; }
[ "$(readlink "$ALIAS" 2>/dev/null)" = "$VENV" ] || { rm -f "$ALIAS"; ln -s "$VENV" "$ALIAS"; }

# Single instance. Bracketed so a polling shell quoting the same string cannot
# match itself (the `pkill -f` self-match trap).
if pgrep -f 'archive-ru[n].py' >/dev/null 2>&1; then
  echo "archive-cycle: already running — skipping" >&2
  exit 0
fi
# Never contend with the boot recommit: it has a five-minute patience and
# losing that race costs the VM rows until the next boot.
if pgrep -f 'recommit-rulese[t].sh' >/dev/null 2>&1; then
  echo "archive-cycle: boot recommit live — skipping" >&2
  exit 0
fi

[ -n "${MULTIVENUE_S3_ENV_FILE:-}" ] && export MULTIVENUE_S3_ENV_FILE

# Background QoS: this shares the box with the ingress threads whose venue-time
# staleness gate judges the data being captured WHILE the upload runs. An
# upload that adds delivery delay degrades the very data it is preserving.
exec taskpolicy -b nice -n 19 \
  "$ALIAS/bin/python3" "$REPO/scripts/archive-run.py" \
  push-pending --budget-s "$S3_CYCLE_BUDGET_S"
