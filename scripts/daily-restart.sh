#!/bin/zsh
# M3 daily graceful restart (docs/m3-progress.md; mvp-plan §4-M3).
# Runs every 60 s from com.multivenue.daily-restart; on the FIRST
# tick of a new UTC day it SIGTERMs the engine (the M1d-proven clean
# drain — capture flushed, run dir sealed). launchd KeepAlive then
# relaunches through engine-wrapper.sh → universe refresh → a fresh
# run dir. Result: one run dir per UTC day, dark seconds only —
# gap-free days BY CONSTRUCTION (the capture-catalog gap tolerance,
# 300 s, judges exactly this window).
#
# StartInterval (not StartCalendarInterval) because launchd calendar
# fires in LOCAL time — a UTC-day law must not bend to DST.
set -u

STAMP="$HOME/multivenue/state/last-restart-utc-day"
mkdir -p "${STAMP:h}"
today="$(date -u +%Y%m%d)"
last="$(cat "$STAMP" 2>/dev/null || echo none)"
if [ "$today" != "$last" ]; then
  echo "$today" > "$STAMP"
  if pgrep -f "multivenue-engine run" >/dev/null 2>&1; then
    echo "daily-restart: UTC day $today — SIGTERM drain" >&2
    pkill -TERM -f "multivenue-engine run" 2>/dev/null || true
  fi
fi
