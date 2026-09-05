#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 launchd installer (docs/local-setup.md runbook). Idempotent:
# renders launchd/*.plist templates (@REPO@/@HOME@), seeds state,
# (re)bootstraps the agents (engine, daily-restart, caffeinate,
# candles, regime — RG5 added the 5-minute regime cycle). Re-running = graceful restart of
# the standing engine (bootout sends SIGTERM per ExitTimeOut).
set -eu

SCRIPT_DIR="${0:A:h}"
REPO="${SCRIPT_DIR:h}"
AGENTS="$HOME/Library/LaunchAgents"
STATE="$HOME/multivenue/state"
LOGS="$HOME/multivenue/logs/launchd"

mkdir -p "$AGENTS" "$STATE" "$LOGS"
# Seed today's stamp so install does NOT immediately trigger the
# daily-restart kill of the engine we are about to start.
date -u +%Y%m%d > "$STATE/last-restart-utc-day"
# Live dailies config, if absent (operator-editable; see example).
if [ ! -f "$HOME/multivenue/pm-dailies.toml" ]; then
  cp "$REPO/pm-dailies.toml.example" "$HOME/multivenue/pm-dailies.toml"
fi

for label in com.multivenue.engine com.multivenue.daily-restart com.multivenue.caffeinate com.multivenue.candles com.multivenue.regime; do
  sed -e "s|@REPO@|$REPO|g" -e "s|@HOME@|$HOME|g" \
    "$REPO/launchd/$label.plist" > "$AGENTS/$label.plist"
  launchctl bootout "gui/$UID/$label" 2>/dev/null || true
  launchctl bootstrap "gui/$UID" "$AGENTS/$label.plist"
done

sleep 2
echo "--- launchd state ---"
for label in com.multivenue.engine com.multivenue.daily-restart com.multivenue.caffeinate com.multivenue.candles com.multivenue.regime; do
  launchctl print "gui/$UID/$label" 2>/dev/null | grep -E "^\s*(state|pid)" | head -2 |
    sed "s|^|$label: |"
done
echo "installed. logs: $LOGS  stamp: $STATE/last-restart-utc-day"
