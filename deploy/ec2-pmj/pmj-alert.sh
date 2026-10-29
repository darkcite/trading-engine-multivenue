#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# pmj-alert.sh — the engine unit's alarm, as far as this host can ring it
# alone (notifications for slot 11 are "Not now", R-PMJ-S75: NO relay is
# built here). Two writes, both under ~mv/multivenue:
#
#   state/pmj-alert          the MARKER: one line per alarm, appended (newest
#                            last; trimmed to 500 lines). A watcher compares
#                            its size or mtime; pmj-daily.sh appends to it too
#                            (review trigger, gate RED, a drain with no boot).
#   .notify-inbox/*.msg      a loud page in notify-send.sh's format, for the
#                            relay the README requires before any dust line
#                            (multivenue-notify here, or a Mac-side watcher).
#
#   pmj-alert.sh exit        multivenue-pmj.service's ExecStopPost: every time
#                            the engine process ends. SERVICE_RESULT=success
#                            (a clean SIGTERM drain, a stop) is silent; any
#                            other result writes the marker and, at most once
#                            per 15 minutes, a page (a relaunch loop pages
#                            once, not every RestartSec). Exit status 78 (the
#                            wrapper's refusal) leaves the page to on-failure.
#   pmj-alert.sh on-failure  multivenue-pmj-alert.service, started by the
#                            engine unit's OnFailure= when it ends FAILED (a
#                            wrapper refusal, a start timeout): marker + page,
#                            always.
#   pmj-alert.sh note TEXT   a line in the marker and a page (pmj-daily.sh).
set -u

MV="${HOME}/multivenue"
MARK="$MV/state/pmj-alert"
INBOX="$MV/.notify-inbox"
SELF="$(readlink -f "$0")"
REPO="$(cd "${SELF%/*}/../.." && pwd -P)"
UNIT="multivenue-pmj.service"
now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

mark() {
  mkdir -p "$MV/state"
  printf '%s %s\n' "$now" "$*" >>"$MARK"
  if [ "$(wc -l <"$MARK")" -gt 500 ]; then
    tail -n 500 "$MARK" >"$MARK.tmp" && mv -f "$MARK.tmp" "$MARK"
  fi
  chmod 600 "$MARK" 2>/dev/null || true
  echo "pmj-alert: $*" >&2
}

page() {
  "$REPO/scripts/notify-send.sh" -d "$INBOX" -l -s PMJ "$*" >/dev/null 2>&1 ||
    echo "pmj-alert: notify-send.sh failed — the marker stands" >&2
}

case "${1:-}" in
  exit)
    result="${SERVICE_RESULT:-unknown}"
    [ "$result" = success ] && exit 0
    mark "engine ended: result=$result code=${EXIT_CODE:-?} status=${EXIT_STATUS:-?}"
    [ "${EXIT_STATUS:-}" = 78 ] && exit 0
    last="$MV/state/pmj-alert-paged"
    if [ -f "$last" ] && [ $(($(date -u +%s) - $(stat -c %Y "$last"))) -lt 900 ]; then
      exit 0
    fi
    touch "$last"
    page "slot 11 (EC2 standalone) engine ended at $now: result=$result code=${EXIT_CODE:-?} status=${EXIT_STATUS:-?}. systemd relaunches it (backoff to 5 min); see ~mv/multivenue/logs/systemd/engine.err.log. Further pages for this held 15 min."
    ;;
  on-failure)
    info="$(systemctl show "$UNIT" -p Result -p ExecMainCode -p ExecMainStatus -p NRestarts 2>/dev/null | paste -sd' ' -)"
    mark "engine unit FAILED: $info"
    page "slot 11 (EC2 standalone) $UNIT FAILED at $now ($info). It does NOT relaunch: the last lines of ~mv/multivenue/logs/systemd/engine.err.log say why (status 78 = the wrapper refused a configuration). Fix, then: sudo systemctl start $UNIT."
    ;;
  note)
    shift
    [ $# -gt 0 ] || { echo "usage: pmj-alert.sh note TEXT" >&2; exit 64; }
    mark "$*"
    page "slot 11 (EC2 standalone) at $now: $*"
    ;;
  *)
    echo "usage: pmj-alert.sh exit | on-failure | note TEXT" >&2
    exit 64
    ;;
esac
exit 0
