#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 restart lane, T2 slot generalization (capture-remediation plan
# 2026-08-28; outage doc 2026-08-27 §7 tier 2). Runs every 60 s from
# com.multivenue.daily-restart.
#
# UTC SLOTS (fire once per UTC day, on the first minute-tick at/after
# the slot time):
#   0000  day boundary — SIGTERM drain; wrapper reboot refreshes the
#         PM universe; retention pass (0000 only).
#   0830  Defect A revival: options settle 08:00Z on Deribit+OKX and
#         the frozen boot-time chain kills both sessions; a restart
#         re-runs discovery onto a live chain. 08:30 — not 08:05 —
#         clears Deribit's 9–19 min post-settlement removal lag.
#   1605  Defect B revival: PM up/down dailies resolve 16:00Z; the
#         wrapper's per-boot refresh subscribes the market that went
#         live at 16:00Z.
#   0020  ACTION slot (no drain): nightly shadow-P&L report for the
#         closed UTC day (M4 D2 — claude_worker.pnl_report module).
#
# Restart slots SIGTERM the engine (M1d-proven clean drain — capture
# flushed, run dir sealed); launchd KeepAlive relaunches through
# engine-wrapper.sh -> universe refresh -> fresh boot discovery ->
# fresh run dir + manifests (the per-run manifest law is BUILT for
# restarts). Multiple due slots (deploy day, wake-from-sleep
# catch-up) coalesce into ONE drain. Days now carry 3-4 run dirs —
# Aug-23 carried 3 and scored GAP-FREE; each dark window is ~10-40 s
# against the 300 s catalog tolerance.
#
# MIGRATION SEED (deploy safety): a MISSING slot stamp seeds to
# today WITHOUT firing — deploying this script mid-day never
# triggers a surprise drain. Force a slot NOW (also the sanctioned
# manual mid-day revive) with:
#   echo 19700101 > ~/multivenue/state/last-restart-utc-0000
# (fires within 60 s). The legacy single stamp `last-restart-utc-day`
# is ignored and left in place.
#
# StartInterval (not StartCalendarInterval) because launchd calendar
# fires in LOCAL time — a UTC-day law must not bend to DST.
set -u

STATE="$HOME/multivenue/state"
mkdir -p "$STATE"
REPO_DIR="${0:A:h:h}"
SCRIPTS_DIR="${0:A:h}"

today="$(date -u +%Y%m%d)"
now_hm="$(date -u +%H%M)"

# slot_ready <HHMM>: succeeds when now >= slot and the slot's stamp
# is not today. Side effect: an ABSENT stamp is seeded to today and
# reported not-ready (the migration seed above).
slot_ready() {
  local slot="$1"
  local stamp="$STATE/last-restart-utc-$slot"
  [ "$((10#$now_hm))" -ge "$((10#$slot))" ] || return 1
  if [ ! -f "$stamp" ]; then
    echo "$today" > "$stamp"
    echo "daily-restart: seeded slot $slot (no fire)" >&2
    return 1
  fi
  [ "$(cat "$stamp")" != "$today" ]
}

# slot_mark <HHMM>: stamp the slot as done for today.
slot_mark() {
  echo "$today" > "$STATE/last-restart-utc-$1"
}

drain=0
retention=0
fired=""
if slot_ready 0000; then
  slot_mark 0000
  drain=1
  retention=1
  fired="$fired 0000"
fi
if slot_ready 0830; then
  slot_mark 0830
  drain=1
  fired="$fired 0830"
fi
if slot_ready 1605; then
  slot_mark 1605
  drain=1
  fired="$fired 1605"
fi
# BST3.5 (binance-stocks-plan, operator-approved 2026-08-29): the
# equity-dailies pair — next-day PM equity markets load ≤15 min after
# the US close in BOTH seasons (20:00Z close in EDT, 21:00Z in EST;
# the pre-close slot of the pair resolves idempotently).
if slot_ready 2015; then
  slot_mark 2015
  drain=1
  fired="$fired 2015"
fi
if slot_ready 2115; then
  slot_mark 2115
  drain=1
  fired="$fired 2115"
fi

if [ "$drain" = 1 ]; then
  if pgrep -f "multivenue-engine run" >/dev/null 2>&1; then
    echo "daily-restart: UTC $today $now_hm slots$fired — SIGTERM drain" >&2
    pkill -TERM -f "multivenue-engine run" 2>/dev/null || true
  else
    echo "daily-restart: UTC $today $now_hm slots$fired — engine not running (KeepAlive will boot)" >&2
  fi
fi

if [ "$retention" = 1 ]; then
  # Once per UTC day: the M3 retention pass (keep-all until disk
  # pressure; scripts/retention.sh documents the policy).
  "$SCRIPTS_DIR/retention.sh" >&2 || true
fi

# 0020 ACTION slot — nightly shadow-P&L for the closed UTC day (the
# M4 D2 deferral landing here per the remediation plan). Worker-verb
# serialization law: any live claude-worker invocation defers this to
# the next minute (the stamp stays unmarked until it actually runs).
if slot_ready 0020; then
  if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
    echo "daily-restart: 0020 pnl deferred — worker busy (retry next minute)" >&2
  else
    slot_mark 0020
    (
      cd "$REPO_DIR" || exit 0
      # ICDP I6 (2026-09-03): the release dir on PATH — the module spawns
      # `multivenue-engine` by name (the §14 spawn contract) and launchd's
      # PATH never carried it: every 0020 run since Aug-23 died with
      # FileNotFoundError before audit-pnl started (restart.log).
      export PATH="$REPO_DIR/target/release:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
      if [ -f .env ]; then
        set -a
        . ./.env
        set +a
      fi
      cd claude-worker || exit 0
      echo "daily-restart: 0020 pnl_report (closed UTC day, per-run bounded, fee tier)" >&2
      # Day mode: one bounded audit-pnl per run of the closed UTC day
      # (never the whole root — ops debt c), merged into the day pair,
      # with the operator's tier from ~/multivenue/fees.toml when present.
      uv run python -m claude_worker.pnl_report --closed-day >&2 ||
        echo "daily-restart: pnl_report failed (non-fatal; tomorrow retries)" >&2
    )
  fi
fi
