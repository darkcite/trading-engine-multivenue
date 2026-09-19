#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 launchd installer (docs/local-setup.md runbook). Idempotent:
# renders launchd/*.plist templates (@REPO@/@HOME@), seeds state,
# (re)bootstraps the agents (engine, daily-restart, caffeinate,
# candles, regime — RG5 added the 5-minute regime cycle; news — the
# NEWS lane's 60 s aggregation cycle; dashboard — RG6 added the 9292
# page server; llm — the NEWS lane's local model sidecar). Re-running =
# graceful restart of the standing engine (bootout sends SIGTERM per
# ExitTimeOut).
#
# ARMED-ENGINE GUARD (CLAUDE.md: "every restart of an armed engine passes
# scripts/exec-smoke.sh first"). This script boots out and re-bootstraps ALL
# labels, the engine among them, and it does NOT run the smoke — so re-running
# it while a LIVE-ARMED engine stands would restart the mainnet-armed engine
# without its pre-boot check. It therefore refuses, and names the two ways
# forward, rather than doing it silently. To install or refresh ONE label
# without touching the engine, render and bootstrap that label by hand:
#
#   sed -e "s|@REPO@|$REPO|g" -e "s|@HOME@|$HOME|g" \
#     launchd/com.multivenue.<name>.plist > ~/Library/LaunchAgents/com.multivenue.<name>.plist
#   launchctl bootout   "gui/$UID/com.multivenue.<name>" 2>/dev/null || true
#   launchctl bootstrap "gui/$UID" ~/Library/LaunchAgents/com.multivenue.<name>.plist
set -eu

SCRIPT_DIR="${0:A:h}"
REPO="${SCRIPT_DIR:h}"
AGENTS="$HOME/Library/LaunchAgents"
STATE="$HOME/multivenue/state"
LOGS="$HOME/multivenue/logs/launchd"

# The guard. `--arm-live` in the engine's argv is the two-switch interlock's
# live half, so its presence IS the definition of an armed engine.
ARMED="$(pgrep -fl 'multivenue-engin[e]' 2>/dev/null | grep -c -- '--arm-live' || true)"
if [ "${ARMED:-0}" -gt 0 ] && [ "${ALLOW_ARMED_RESTART:-0}" != "1" ]; then
  echo "REFUSING: a LIVE-ARMED engine is running and this script restarts it" >&2
  echo "  without scripts/exec-smoke.sh, which CLAUDE.md requires of every" >&2
  echo "  restart of an armed engine." >&2
  echo "" >&2
  echo "  To install or refresh one label, do it by hand (see the header)." >&2
  echo "  To proceed anyway: run scripts/exec-smoke.sh, then re-run with" >&2
  echo "  ALLOW_ARMED_RESTART=1 $0" >&2
  exit 3
fi

mkdir -p "$AGENTS" "$STATE" "$LOGS"
# Seed today's stamp so install does NOT immediately trigger the
# daily-restart kill of the engine we are about to start.
date -u +%Y%m%d > "$STATE/last-restart-utc-day"
# Live dailies config, if absent (operator-editable; see example).
if [ ! -f "$HOME/multivenue/pm-dailies.toml" ]; then
  cp "$REPO/pm-dailies.toml.example" "$HOME/multivenue/pm-dailies.toml"
fi

for label in com.multivenue.engine com.multivenue.daily-restart com.multivenue.caffeinate com.multivenue.candles com.multivenue.regime com.multivenue.news com.multivenue.llm com.multivenue.dashboard com.multivenue.archive; do
  sed -e "s|@REPO@|$REPO|g" -e "s|@HOME@|$HOME|g" \
    "$REPO/launchd/$label.plist" > "$AGENTS/$label.plist"
  launchctl bootout "gui/$UID/$label" 2>/dev/null || true
  launchctl bootstrap "gui/$UID" "$AGENTS/$label.plist"
done

sleep 2
echo "--- launchd state ---"
for label in com.multivenue.engine com.multivenue.daily-restart com.multivenue.caffeinate com.multivenue.candles com.multivenue.regime com.multivenue.news com.multivenue.llm com.multivenue.dashboard com.multivenue.archive; do
  launchctl print "gui/$UID/$label" 2>/dev/null | grep -E "^\s*(state|pid)" | head -2 |
    sed "s|^|$label: |"
done
echo "installed. logs: $LOGS  stamp: $STATE/last-restart-utc-day"
