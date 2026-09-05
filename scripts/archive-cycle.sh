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

# Network QoS — measured, not assumed.
#
# `taskpolicy -b` puts the process in macOS's BACKGROUND traffic class. A short
# burst shows no penalty (4 MiB transfers measured 5.21 vs 5.47 MiB/s), which is
# exactly why it looked free at first. A SUSTAINED transfer is throttled hard:
# the real backfill crawled at ~0.3 MiB/s under it while an unthrottled probe on
# the same link, at the same moment, got 5.76 MiB/s — and `sample` showed the
# process parked in _ssl__SSLSocket_write -> poll, blocked on the socket. Off it,
# the same work runs at ~5 MiB/s, a 13x difference.
#
# So it is OFF by default. The justification is duration, not indifference: one
# day of capture is ~1.8 GiB stored, which is ~6 minutes at 5 MiB/s, in a quiet
# local hour. During those minutes feed delivery does take a measurable hit
# (venue delay EMAs rose to ~200 ms transiently), and the venue-time staleness
# gate judges ticks while the upload runs — so this is a real, bounded cost.
#
# Set S3_NICE_NETWORK=1 in retention.conf to put the background class back: the
# upload then takes hours instead of minutes but is invisible to the feeds.
NET_PREFIX=""
if [ "${S3_NICE_NETWORK:-0}" = "1" ]; then
  NET_PREFIX="taskpolicy -b"
fi

# shellcheck disable=SC2086  # NET_PREFIX is a deliberate word-split
exec $NET_PREFIX nice -n 19 \
  "$ALIAS/bin/python3" "$REPO/scripts/archive-run.py" \
  push-pending --budget-s "$S3_CYCLE_BUDGET_S"
