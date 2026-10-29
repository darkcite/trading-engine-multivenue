#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# pmj-daily.sh — the EC2 standalone's daily lane: the Linux port of the parts
# of scripts/daily-restart.sh a slot-11-only engine needs. Run every minute (at
# second 30 UTC) by multivenue-pmj-daily.timer, as the user `mv`.
#
# ONE UTC SLOT, 0020 (PMJ_DAILY_SLOT): fires once per UTC day on the first tick
# at or after 00:20Z — the day's drain (the run dir closes, so retention can
# archive it), the retention pass and the read-only claimable listing.
# A missing stamp seeds to today WITHOUT firing (the Mac's migration seed);
# force the slot now — the sanctioned manual restart — with
#   echo 19700101 > /home/mv/multivenue/state/last-restart-utc-0020
#
# THE SAFE MINUTE: second 30 of a minute m with m mod 5 = 1 (00:21:30,
# 00:26:30, 00:31:30, …). A 5-minute window opens at second 0 of every
# m mod 5 = 0 minute, so the drain starts 90 s into a window:
#   - 96 s after the PREVIOUS window's last possible submit (slot 11 decides
#     at tau in [6, 20) s before a window's end, R-PMJ-S55) — and before the
#     restart, /metrics must show no slot-11 order in flight (below);
#   - 148 s before the NEXT window's strike reports begin (law (iii),
#     R-PMJ-S61: [open - 62, open - 2] s, 238..298 s into this window). The
#     stop's drain (60 s at most before SIGKILL) and a ~20-40 s boot are back
#     well before that;
#   - 90 s or more off every :00/:15/:30/:45 end (m = 1, 16, 31, 46 start
#     90 s after one; m = 11, 26, 41, 56 finish 90 s or more before the next).
# The clock is re-read right before the restart: a drain whose gate ran past
# second 50 waits for the next safe minute (it would still be back by ~210 s,
# before 238). A drain owed at another minute waits for the next safe one
# (state/pmj-gate-pending, the Mac's file).
# EVERY DRAIN COSTS THE CURRENT WINDOW: the restarted engine never trades a
# window whose strike it did not see, and this window's strike was reported
# before the drain — it trades from the next window on (one 5-minute window
# a day, plus one per forced restart).
#
# IN FLIGHT: with slot 11 live (/state boot.paper 0 — on this engine only
# slot 11 can be), the drain first reads /metrics: the arm's
# engine_pmj_arm_submitted_total - engine_pmj_arm_done_total is the number
# of its orders not yet retired (every row it opens retires exactly once into
# `done`: refused, filled or closed, released, or lost — exec-jupiter arm.rs
# `retire`). Above 0, or unreadable while live, the drain waits for the next
# safe minute: logged, NOT a gate failure; one alert after an hour of such
# waits. (jup-gw's landed - done - lost is not used: an in-doubt order that
# lands later counts in in_doubt AND landed, and one never landed is
# released into done without landing.)
#
# THE PMJ GATE (risk-policy ### J16; daily-restart.sh's `pmj-gate` blocks,
# ported line for line): armed when strategy.conf's ARM_LIVE names 11 (the file
# sourced in a subshell; parts trimmed, `+` dropped, compared as numbers) or
# the EXEC_TOML in force or any ~/multivenue/exec*.toml marks [exec.slot.11]
# live (an unreadable one arms it; `enabled = 0` does not). Armed:
#   - exec.HALT beside the EXEC_TOML in force naming 11 (as the router reads
#     it) or not readable whole -> the drain is DEFERRED, no check, no failure
#     counted, paged silently once per halt-file mtime;
#   - else scripts/pmj-exec.sh self-test, then key-check at the slot's
#     cap_day_usd_1e6 (the in-force file's, else the arming file's), with
#     MULTIVENUE_ENV_FILE exported (the unit sets /home/mv/multivenue/.env:
#     the one env file; no release carries a .env). A failure defers (owed),
#     backs off 1 / 5 / 15 / 60 minutes, writes state/pmj-gate-RED after
#     three;
#   - exit 77 (a REVIEW TRIGGER: the Predict deploy, the vault entry or the
#     mint not what was reviewed, or a pinned account closed) also appends
#     `slot=11 reason=operator` ONCE to that exec.HALT — the running router
#     halts slot 11 within about a second — and alerts (pmj-alert.sh: the
#     marker and a loud page).
# Not armed: the owed drain proceeds at the safe minute and the gate's failure
# state is cleared (the Mac drops a restart only the gate owed; here the owed
# file also carries the day's drain, which must happen to close the run).
#
# NOT PORTED, on purpose: E3's exec-smoke gate (it proves the Hyperliquid
# EIP-712 signer; this engine can arm no Hyperliquid or Binance slot — the
# wrapper refuses any STRATEGY but pmj and the engine refuses a live slot
# beside 11), the 0833/1605/2015/2115 slots (option chains, PM dailies, equity
# dailies: none on this engine), the xsd rotation and the 0020 worker reports
# (no worker on this host).
#
# THE DRAIN (Linux, no launchd): `systemctl try-restart --no-block` on the
# engine unit, as `mv`, allowed by ONE polkit rule (host-setup.sh) for this
# unit's try-restart alone: systemd SIGTERMs the engine (the M1d
# drain, 60 s before SIGKILL) and boots the release `current` points at. A
# manual job resets systemd's restart counter, so the unit's RestartSteps
# backoff never stretches the daily dark window. Without polkit it falls back
# to SIGTERM on the unit's MainPID (an automatic restart: the counter then
# grows; logged). try-restart never starts a STOPPED unit (pmj-kill.sh --stop).
# Then /state is awaited until a NEW boot.pid answers, and the boot is logged
# (sha, paper, slot 11 enabled, its halt word).
#
# RETENTION (scripts/retention.sh, ported: GNU df instead of macOS `df -g`):
# ~/multivenue/retention.conf; with ARCHIVE_MODE=s3 every run dir is deleted
# ONLY after `archive-run.py verify` exits 0, oldest first, the first
# unverified run stops the sweep; the newest run and anything younger than
# PROTECT_DAYS are never touched. Uploading (stage A) is NOT here: it is
# multivenue-pmj-archive.timer (scripts/archive-cycle.sh), as on the Mac.
#
# CLAIMS ARE NEVER SENT (J15/J16): once a day, when .env names JUP_API_KEY and
# PMJ_WALLET_PUBKEY, the wallet's positions are READ (GET /prediction/v1/
# positions; the key goes to curl on stdin, never in argv) and the claimable
# ones listed in the log; the answer is kept in logs/pmj-positions-<day>.json.
#
# Paging: pmj-alert.sh note — a line in state/pmj-alert (the marker) and a
# loud page into ~/multivenue/.notify-inbox; the halt deferral pages silently.
# No notifier relays the inbox from this host until one is deployed (R-PMJ-S75
# "Not now"); a relay is a README prerequisite before any dust line.
set -u
shopt -s nullglob

SELF="$(readlink -f "$0")"
KIT="${SELF%/*}"
REPO_DIR="$(cd "$KIT/../.." && pwd -P)" || exit 78
SCRIPTS_DIR="$REPO_DIR/scripts"
MV="$HOME/multivenue"
STATE="$MV/state"
mkdir -p "$STATE"
UNIT="${PMJ_UNIT:-multivenue-pmj.service}"
STATE_URL="${PMJ_STATE_URL:-http://127.0.0.1:9191/state}"
METRICS_URL="${PMJ_METRICS_URL:-http://127.0.0.1:9191/metrics}"
# The one env file (the unit sets it; scripts/pmj-exec.sh reads it by name).
export MULTIVENUE_ENV_FILE="${MULTIVENUE_ENV_FILE:-$MV/.env}"
SLOT="${PMJ_DAILY_SLOT:-0020}"
INBOX="$MV/.notify-inbox"
STRATEGY_CONF="$MV/strategy.conf"

log() { printf '%s pmj-daily: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }

[[ "$SLOT" =~ ^[0-2][0-9][0-5][0-9]$ ]] || { log "PMJ_DAILY_SLOT='$SLOT' is not HHMM"; exit 78; }
case "$MULTIVENUE_ENV_FILE" in
  /*) ;;
  *) log "MULTIVENUE_ENV_FILE is not an absolute path"; exit 78 ;;
esac
read -r today now_hm now_m < <(date -u '+%Y%m%d %H%M %M')
now_m=$((10#$now_m))
ALERT="$KIT/pmj-alert.sh"

notify() {
  "$SCRIPTS_DIR/notify-send.sh" -d "$INBOX" "$@" >/dev/null 2>&1
}

# slot_ready <HHMM>: now >= slot and the stamp is not today; an ABSENT stamp
# is seeded to today and reported not ready (daily-restart.sh's seed).
slot_ready() {
  local slot="$1" stamp="$STATE/last-restart-utc-$1"
  [ "$((10#$now_hm))" -ge "$((10#$slot))" ] || return 1
  if [ ! -f "$stamp" ]; then
    echo "$today" >"$stamp"
    log "seeded slot $slot (no fire)"
    return 1
  fi
  [ "$(cat "$stamp")" != "$today" ]
}
slot_mark() { echo "$today" >"$STATE/last-restart-utc-$1"; }
# Second 30 of an m mod 5 = 1 minute (90 s into a 5-minute window; never
# within 90 s of a :00/:15/:30/:45 end).
safe_minute() { [ $((now_m % 5)) -eq 1 ]; }
# The clock re-read right before the restart: still that minute's first 50 s.
still_safe() {
  local m sec
  read -r m sec < <(date -u '+%M %S')
  m=$((10#$m))
  sec=$((10#$sec))
  [ $((m % 5)) -eq 1 ] && [ "$sec" -le 50 ]
}
alert() { "$ALERT" note "$*" >/dev/null 2>&1 || log "pmj-alert.sh failed: $*"; }

# >>> pmj-gate (definitions; daily-restart.sh's, in bash)
PMJ_GATE_PENDING="$STATE/pmj-gate-pending"
PMJ_GATE_FAILS="$STATE/pmj-gate-fails"
PMJ_GATE_NEXT="$STATE/pmj-gate-next"
PMJ_GATE_RED="$STATE/pmj-gate-RED"
PMJ_GATE_HALT_PAGED="$STATE/pmj-gate-halt-paged"
PMJ_GATE_ALERT_AFTER=3
PMJ_GATE_TRIGGER=""
PMJ_GATE_FILE=""

# The value strategy.conf gives KEY as the wrapper sees it: SOURCED in a
# subshell, nothing it prints kept.
pmj_conf_get() {
  [ -f "$STRATEGY_CONF" ] || return 0
  (
    unset "$1"
    set +u
    # shellcheck disable=SC1090 # the operator's file, sourced as the wrapper does
    . "$STRATEGY_CONF" >/dev/null 2>&1
    printf '%s\n' "${!1:-}"
  )
}

# 0 = the ARM_LIVE spec $1 names slot 11 as the engine's parse_arm_live reads it.
pmj_spec_names_11() {
  local part
  local -a parts
  IFS=, read -r -a parts <<<"$1"
  for part in "${parts[@]}"; do
    part="${part//[[:space:]]/}"
    part="${part#+}"
    [[ "$part" =~ ^[0-9]+$ ]] || continue
    [ "$((10#$part))" = 11 ] && return 0
  done
  return 1
}

# The EXEC_TOML in force: strategy.conf's, else the default.
pmj_exec_toml() {
  local f
  f="$(pmj_conf_get EXEC_TOML)"
  printf '%s\n' "${f:-$MV/exec.toml}"
}

# 0 = file $1 has an [exec.slot.11] section whose mode is live.
pmj_file_arms_11() {
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) } s && /^[ \t]*mode[ \t]*=[ \t]*"live"/ { found = 1 } END { exit !found }' "$1"
}

pmj_gate_armed() {
  local f
  PMJ_GATE_FILE=""
  if pmj_spec_names_11 "$(pmj_conf_get ARM_LIVE)"; then
    PMJ_GATE_TRIGGER="strategy.conf's ARM_LIVE names slot 11"
    return 0
  fi
  for f in "$(pmj_exec_toml)" "$MV"/exec*.toml; do
    [ -f "$f" ] || continue
    if [ ! -r "$f" ]; then
      PMJ_GATE_TRIGGER="$f (UNREADABLE — arming the pmj gate rather than assuming slot 11 is not live)"
      return 0
    fi
    if grep -Eq '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*0' "$f"; then
      continue
    fi
    if pmj_file_arms_11 "$f"; then
      PMJ_GATE_TRIGGER="$f marks [exec.slot.11] live"
      PMJ_GATE_FILE="$f"
      return 0
    fi
  done
  PMJ_GATE_TRIGGER=""
  return 1
}

# [exec.slot.11] cap_day_usd_1e6 of file $1, digits only; empty when none.
pmj_cap_of() {
  local v
  [ -r "$1" ] || return 0
  v="$(awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) } s && /^[ \t]*cap_day_usd_1e6[ \t]*=/ { v = $0; sub(/^[^=]*=[ \t]*/, "", v); sub(/[ \t]*(#.*)?$/, "", v); print v; exit }' "$1")"
  printf '%s\n' "${v//_/}"
}

pmj_gate_cap_day() {
  local v
  v="$(pmj_cap_of "$(pmj_exec_toml)")"
  if [ -z "$v" ] && [ -n "$PMJ_GATE_FILE" ]; then
    v="$(pmj_cap_of "$PMJ_GATE_FILE")"
  fi
  printf '%s\n' "$v"
}

pmj_gate_note_failure() {
  local rc="$1" where="$2" n delay
  n=1
  [ -f "$PMJ_GATE_FAILS" ] && n=$(($(cat "$PMJ_GATE_FAILS") + 1))
  echo "$n" >"$PMJ_GATE_FAILS"
  case "$n" in
    1) delay=60 ;;
    2) delay=300 ;;
    3) delay=900 ;;
    *) delay=3600 ;;
  esac
  echo "$(($(date -u +%s) + delay))" >"$PMJ_GATE_NEXT"
  log "pmj gate FAILED at the $where (exit $rc), attempt $n; next pmj key-check in $((delay / 60)) min"
  if [ "$n" -ge "$PMJ_GATE_ALERT_AFTER" ]; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) pmj gate RED: $where exit $rc, $n consecutive failures" >"$PMJ_GATE_RED"
    log "*** $PMJ_GATE_RED written — restarts are deferred until the pmj key-check passes ***"
    [ "$n" = "$PMJ_GATE_ALERT_AFTER" ] && alert "pmj gate RED: $where exit $rc, $n consecutive failures — restarts deferred until the key check passes"
  fi
}

pmj_gate_clear() {
  rm -f "$PMJ_GATE_FAILS" "$PMJ_GATE_NEXT" "$PMJ_GATE_RED" "$PMJ_GATE_HALT_PAGED"
}

# The exec.HALT beside the EXEC_TOML in force.
pmj_halt_path() {
  local h
  h="$(pmj_exec_toml)"
  printf '%s\n' "$(dirname -- "$h")/exec.HALT"
}

pmj_halt_mtime() { stat -c %Y -- "$1" 2>/dev/null || echo unknown; }

# 0 = the halt file $1 would refuse slot 11's live boot: it names slot 11 as
# the router's parse_halt_file reads it, or exists but cannot be read whole.
pmj_halt_blocks() {
  local h="$1"
  [ -e "$h" ] || return 1
  { [ -f "$h" ] && [ -r "$h" ]; } || return 0
  [ "$(($(wc -c <"$h")))" -le 640 ] || return 0
  if command -v iconv >/dev/null 2>&1 && ! iconv -f UTF-8 -t UTF-8 "$h" >/dev/null 2>&1; then
    return 0
  fi
  awk '
    {
      line = $0
      sub(/\r$/, "", line); sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
      if (line == "" || substr(line, 1, 1) == "#") next
      n = split(line, w, /[ \t]+/)
      slot = ""
      for (i = 1; i <= n; i++) {
        f = w[i]
        if (f ~ /^slot=/) {
          v = substr(f, 6)
          slot = (v ~ /^[+]?[0-9]+$/) ? v : ""
        } else if (f ~ /^reason=/) {
          continue
        } else if (slot == "" && f ~ /^[+]?[0-9]+$/) {
          slot = f
        }
      }
      if (slot != "") { sub(/^[+]/, "", slot); if (slot + 0 == 11) found = 1 }
    }
    END { exit !found }' "$h"
}

pmj_halt_page() {
  local m
  m="$(pmj_halt_mtime "$1")"
  if [ -f "$PMJ_GATE_HALT_PAGED" ] && [ "$(cat "$PMJ_GATE_HALT_PAGED")" = "$m" ]; then
    return 0
  fi
  echo "$m" >"$PMJ_GATE_HALT_PAGED"
  notify -s PMJ "slot 11 (EC2 standalone): exec.HALT names slot 11 — restarts deferred until the halt line is archived (after review) or slot 11 is flipped to paper (pmj-flip.sh paper)." ||
    log "pmj: notify-send.sh failed"
}

# Review trigger (key-check exit 77): halt the RUNNING slot 11 through the
# exec.HALT beside the EXEC_TOML in force (once per halt) and page.
pmj_gate_review_halt() {
  local h when
  h="$(pmj_halt_path)"
  when="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ -f "$h" ] && grep -Eq '^[[:space:]]*(slot=)?0*11([[:space:]]|$)' "$h"; then
    log "pmj REVIEW TRIGGER — $h already halts slot 11"
    return 0
  fi
  if [ -s "$h" ] && [ -n "$(tail -c 1 "$h")" ]; then
    printf '\n' >>"$h"
  fi
  if printf 'slot=11 reason=operator\n' >>"$h"; then
    log "*** pmj REVIEW TRIGGER — slot 11 HALTED: slot=11 appended to $h ***"
    if [ "$(($(wc -c <"$h")))" -gt 640 ]; then
      log "*** $h is over the router's 640 B — it will NOT be read: HALT SLOT 11 BY HAND ***"
    fi
  else
    log "*** pmj REVIEW TRIGGER — could NOT append to $h: HALT SLOT 11 BY HAND ***"
  fi
  alert "REVIEW TRIGGER at $when: pmj key-check exit 77 (the Predict deploy, the vault entry or the deposit mint is not what was reviewed, or a pinned account closed). slot=11 written to $h; restarts deferred. Review before clearing."
  pmj_halt_mtime "$h" >"$PMJ_GATE_HALT_PAGED"
}

# 0 = slot 11's wallet and venue code are proven for this arming.
pmj_gate_pass() {
  local rc now next cap pmj
  "$SCRIPTS_DIR/pmj-exec.sh" self-test >/dev/null
  rc=$?
  if [ "$rc" != 0 ]; then
    pmj_gate_note_failure "$rc" "pmj self-test"
    return 1
  fi
  now="$(date -u +%s)"
  if [ -f "$PMJ_GATE_NEXT" ]; then
    next="$(cat "$PMJ_GATE_NEXT")"
    if [ "$now" -lt "$next" ]; then
      log "pmj key-check backing off — $(((next - now + 59) / 60)) min until the next attempt"
      return 1
    fi
  fi
  cap="$(pmj_gate_cap_day)"
  case "$cap" in
    '' | *[!0-9]*)
      pmj_gate_note_failure 78 "[exec.slot.11] cap_day_usd_1e6 read (none in the EXEC_TOML in force)"
      return 1
      ;;
  esac
  pmj="$(pmj_conf_get PMJ_TOML)"
  if [ -n "$pmj" ]; then
    "$SCRIPTS_DIR/pmj-exec.sh" key-check --pmj "$pmj" --cap-day-usd-1e6 "$cap" >/dev/null
  else
    "$SCRIPTS_DIR/pmj-exec.sh" key-check --cap-day-usd-1e6 "$cap" >/dev/null
  fi
  rc=$?
  if [ "$rc" = 0 ]; then
    pmj_gate_clear
    return 0
  fi
  if [ "$rc" = 77 ]; then
    pmj_gate_review_halt
    pmj_gate_note_failure "$rc" "pmj key-check REVIEW TRIGGER"
    return 1
  fi
  pmj_gate_note_failure "$rc" "pmj key-check"
  return 1
}
# <<< pmj-gate

# 0 = no slot-11 order in flight, or slot 11 not live (a paper boot builds no
# arm). Live: the arm's submitted - done (every row it opens retires once into
# `done`). Above 0, or unreadable, the drain waits — logged, not a failure;
# after 12 such waits in a row (an hour of safe minutes), one alert.
INFLIGHT_WAITS="$STATE/pmj-drain-waits"
inflight_wait() {
  local n=1
  [ -f "$INFLIGHT_WAITS" ] && n=$(($(cat "$INFLIGHT_WAITS" 2>/dev/null || echo 0) + 1))
  echo "$n" >"$INFLIGHT_WAITS"
  log "$* — the drain waits for the next safe minute (not a gate failure; wait $n)"
  [ "$n" = 12 ] && alert "the daily drain has waited 12 safe minutes in a row (an hour): $*"
  return 1
}
inflight_clear() {
  local paper metrics sub done_n
  paper="$(curl -fsS --max-time 2 "$STATE_URL" 2>/dev/null | jq -r '.boot.paper // empty' 2>/dev/null)"
  case "$paper" in
    1) rm -f "$INFLIGHT_WAITS"; return 0 ;;
    0) ;;
    *) inflight_wait "in-flight check: /state does not answer boot.paper"; return 1 ;;
  esac
  metrics="$(curl -fsS --max-time 3 "$METRICS_URL" 2>/dev/null)"
  sub="$(awk '$1 == "engine_pmj_arm_submitted_total" { print $2; exit }' <<<"$metrics")"
  done_n="$(awk '$1 == "engine_pmj_arm_done_total" { print $2; exit }' <<<"$metrics")"
  if ! [[ "$sub" =~ ^[0-9]+$ ]] || ! [[ "$done_n" =~ ^[0-9]+$ ]]; then
    inflight_wait "in-flight check: slot 11 is live and /metrics carries no engine_pmj_arm_submitted_total / engine_pmj_arm_done_total"
    return 1
  fi
  if [ "$sub" -gt "$done_n" ]; then
    inflight_wait "slot 11 has $((sub - done_n)) order(s) in flight (arm submitted $sub, done $done_n; jup-gw $(awk '$1 ~ /^engine_pmj_gw_(landed|in_doubt|done|lost)_total$/ { sub(/^engine_pmj_gw_/, "", $1); sub(/_total$/, "", $1); printf "%s %s ", $1, $2 }' <<<"$metrics"))"
    return 1
  fi
  log "in-flight check: slot 11 live, no order in flight (arm submitted $sub, done $done_n)"
  rm -f "$INFLIGHT_WAITS"
  return 0
}

# The drain: a polkit try-restart of the unit (fallback: SIGTERM to MainPID),
# then the new boot. 0 = done or moot (restarted, or no engine runs); 1 =
# nothing was signalled and the drain stays owed.
engine_drain() {
  local pid exe deadline seen how nres
  pid="$(systemctl show -p MainPID --value "$UNIT" 2>/dev/null)"
  if ! [[ "${pid:-0}" =~ ^[1-9][0-9]*$ ]]; then
    log "UTC $today $now_hm slots$1 — $UNIT has no running engine (stopped units stay stopped): nothing to drain"
    return 0
  fi
  exe="$(readlink "/proc/$pid/exe" 2>/dev/null)"
  case "$exe" in
    */multivenue-engine | */multivenue-engine" (deleted)") ;;
    *)
      log "MainPID $pid of $UNIT is '$exe', not the engine — not restarting it now (the wrapper is still starting?)"
      return 1
      ;;
  esac
  inflight_clear || return 1
  if ! still_safe; then
    log "the gate ran past the safe minute's first 50 s — the drain waits for the next safe minute"
    return 1
  fi
  nres="$(systemctl show -p NRestarts --value "$UNIT" 2>/dev/null)"
  if systemctl try-restart --no-block "$UNIT" >/dev/null 2>&1; then
    how="systemctl try-restart (polkit)"
  elif kill -TERM "$pid"; then
    how="SIGTERM to MainPID — polkit refused the restart; systemd's restart counter (NRestarts ${nres:-?}) grows: fix the rule (host-setup.sh)"
  else
    log "neither try-restart nor kill -TERM $pid worked"
    return 1
  fi
  log "UTC $today $now_hm slots$1 — drain of pid $pid ($exe) by $how"
  deadline=$((SECONDS + 300))
  while [ "$SECONDS" -lt "$deadline" ]; do
    sleep 3
    seen="$(curl -fsS --max-time 2 "$STATE_URL" 2>/dev/null | jq -r '.boot.pid // empty' 2>/dev/null)"
    if [ -n "$seen" ] && [ "$seen" != "$pid" ]; then
      log "booted: $(curl -fsS --max-time 2 "$STATE_URL" 2>/dev/null | jq -r '"pid=\(.boot.pid) git_sha=\(.boot.git_sha) paper=\(.boot.paper) strategy=\(.boot.strategy) slot11_enabled=\(.slots[11].enabled) slot11_halt=\(.exec.halted[11] // "-")"' 2>/dev/null) exe=$(readlink "/proc/$seen/exe" 2>/dev/null) NRestarts=$(systemctl show -p NRestarts --value "$UNIT" 2>/dev/null)"
      return 0
    fi
  done
  log "*** no new boot answered /state within 300 s of the drain — check: systemctl status $UNIT; logs/systemd/engine.err.log ***"
  alert "the daily drain's engine did not answer /state within 300 s (systemctl status $UNIT; engine.err.log)"
  return 0
}

# Retention (scripts/retention.sh, ported to GNU coreutils; the policy and
# its laws unchanged).
retention_sweep() {
  local LOG_ROOT="${MULTIVENUE_LOG_DIR:-$MV/logs}" CONF="$MV/retention.conf"
  local MIN_FREE_GIB=25 TARGET_FREE_GIB=40 PROTECT_DAYS=7 ARCHIVE_DIR="$MV/archive" ARCHIVE_MODE="compress"
  local VENV_ALIAS="$MV/venv" free now_s d name epoch_ns age_days out
  local -a candidates contents
  # shellcheck disable=SC1090 # the operator's KEY=VALUE file, as retention.sh sources it
  [ -f "$CONF" ] && . "$CONF"
  if [ "$ARCHIVE_MODE" = "s3" ]; then
    if [ -x "$REPO_DIR/claude-worker/.venv/bin/python3" ] && [ -f "$REPO_DIR/scripts/archive-run.py" ]; then
      [ "$(readlink "$VENV_ALIAS" 2>/dev/null)" = "$REPO_DIR/claude-worker/.venv" ] ||
        { rm -f "$VENV_ALIAS"; ln -s "$REPO_DIR/claude-worker/.venv" "$VENV_ALIAS"; }
      [ -n "${MULTIVENUE_S3_ENV_FILE:-}" ] && export MULTIVENUE_S3_ENV_FILE
    else
      log "retention: ARCHIVE_MODE=s3 but the archiver is not installed in $REPO_DIR — using compress"
      ARCHIVE_MODE="compress"
    fi
  fi
  [ -d "$LOG_ROOT" ] || { log "retention: no log root $LOG_ROOT"; return 0; }
  free="$(df -P -BG "$LOG_ROOT" | awk 'NR == 2 { sub(/G$/, "", $4); print $4 }')"
  if [ "${free:-0}" -ge "$MIN_FREE_GIB" ]; then
    return 0
  fi
  log "retention: free ${free}GiB < ${MIN_FREE_GIB}GiB — archiving the oldest run dirs ($ARCHIVE_MODE, protect ${PROTECT_DAYS}d)"
  mkdir -p "$ARCHIVE_DIR"
  now_s="$(date +%s)"
  candidates=("$LOG_ROOT"/run-*)
  # Equal-width ns epochs: name order is age order; the newest is the live run.
  [ "${#candidates[@]}" -le 1 ] && return 0
  unset 'candidates[${#candidates[@]}-1]'
  for d in "${candidates[@]}"; do
    [ -d "$d" ] || continue
    free="$(df -P -BG "$LOG_ROOT" | awk 'NR == 2 { sub(/G$/, "", $4); print $4 }')"
    [ "${free:-0}" -ge "$TARGET_FREE_GIB" ] && break
    name="${d##*/}"
    epoch_ns="${name#run-}"
    [[ "$epoch_ns" =~ ^[0-9]+$ ]] || continue
    age_days=$(((now_s - epoch_ns / 1000000000) / 86400))
    if [ "$age_days" -lt "$PROTECT_DAYS" ]; then
      log "retention: $name is ${age_days}d old (< protect ${PROTECT_DAYS}d) — stopping"
      break
    fi
    contents=()
    for f in "$d"/* "$d"/.[!.]*; do
      [ -f "$f" ] && contents+=("$f")
    done
    if [ "${#contents[@]}" -eq 0 ]; then
      if rmdir "$d" 2>/dev/null; then
        log "retention: $name is empty (a boot that died) — removed"
      else
        log "retention: $name holds no capture file — skipping"
      fi
      continue
    fi
    if [ "$ARCHIVE_MODE" = "s3" ]; then
      if "$VENV_ALIAS/bin/python3" "$REPO_DIR/scripts/archive-run.py" verify "$name" >&2; then
        log "retention: $name verified in the bucket — removing the local copy"
        rm -r "$d"
      else
        log "retention: $name NOT verified in the bucket — stopping"
        break
      fi
    elif [ "$ARCHIVE_MODE" = "move" ]; then
      log "retention: move $name -> $ARCHIVE_DIR/"
      mv "$d" "$ARCHIVE_DIR/" || { log "retention: move failed — stopping"; break; }
    else
      out="$ARCHIVE_DIR/$name.tar.gz"
      log "retention: compress $name -> $out"
      if tar -czf "$out" -C "$LOG_ROOT" "$name" && [ -s "$out" ]; then
        rm -r "$d"
      else
        log "retention: compression failed — original kept, stopping"
        rm -f "$out"
        break
      fi
    fi
  done
}

# The wallet's positions, READ ONLY; claims are never sent (J15/J16).
claimable_listing() {
  (
    local envf="$MULTIVENUE_ENV_FILE" mode owner k
    if [ -L "$envf" ]; then
      log "claimable: $envf is a symlink — not sourced"
      exit 0
    fi
    if [ ! -e "$envf" ]; then
      log "claimable: no $envf — no positions read"
      exit 0
    fi
    if [ ! -f "$envf" ]; then
      log "claimable: $envf is not a regular file — not sourced"
      exit 0
    fi
    read -r mode owner < <(stat -c '%a %U' "$envf")
    if [ "$mode" != 600 ] || [ "$owner" != "$(id -un)" ]; then
      log "claimable: $envf is mode $mode owner $owner, not 600 $(id -un) — not sourced"
      exit 0
    fi
    unset STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML
    set +u
    set -a
    # shellcheck disable=SC1090 # the operator's .env, checked above, sourced as pmj-exec.sh sources it
    . "$envf" >/dev/null 2>&1
    set +a
    for k in STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML; do
      if [ -n "${!k+x}" ]; then
        log "claimable: $envf sets $k — the switches belong in strategy.conf; no positions read"
        exit 0
      fi
    done
    if [ -z "${JUP_API_KEY:-}" ] || [ -z "${PMJ_WALLET_PUBKEY:-}" ]; then
      log "claimable: JUP_API_KEY / PMJ_WALLET_PUBKEY not in .env — no positions read"
      exit 0
    fi
    [[ "$PMJ_WALLET_PUBKEY" =~ ^[1-9A-HJ-NP-Za-km-z]{32,44}$ ]] ||
      { log "claimable: PMJ_WALLET_PUBKEY is not a base58 key — no positions read"; exit 0; }
    out="$MV/logs/pmj-positions-$today.json"
    if ! printf 'header = "x-api-key: %s"\n' "$JUP_API_KEY" |
      curl -fsS --max-time 15 -K - -o "$out.tmp" \
        "https://api.jup.ag/prediction/v1/positions?ownerPubkey=$PMJ_WALLET_PUBKEY" 2>/dev/null; then
      rm -f "$out.tmp"
      log "claimable: the positions read failed (transport or HTTP) — nothing listed"
      exit 0
    fi
    chmod 600 "$out.tmp" && mv "$out.tmp" "$out"
    if lines="$(jq -r '
      (if type == "object" and has("data") then .data elif type == "array" then . else [] end) as $d
      | "claimable: positions=\($d | length) claimable=\([$d[] | select(.claimable == true)] | length) (read only; claims are never sent)",
        ($d[] | select(.claimable == true)
          | "claimable:   \(.pubkey // .positionPubkey // "?") market=\(.marketId // "?") isYes=\(.isYes // "?") contracts=\(.contracts // .contractsMicro // "?") payout=\(.payoutUsd // .payoutAmountUsd // "?")")' \
      "$out" 2>/dev/null)"; then
      while IFS= read -r l; do log "$l"; done <<<"$lines"
    else
      log "claimable: the answer is not the expected JSON — kept in $out"
    fi
  )
}

# --- the tick ---------------------------------------------------------------
fired=""
retention=0
if slot_ready "$SLOT"; then
  slot_mark "$SLOT"
  fired=" $SLOT"
  retention=1
  echo "$fired" >>"$PMJ_GATE_PENDING"
  log "slot $SLOT fired — a drain is owed (taken at the next safe minute: :30 of m mod 5 = 1)"
fi

if [ -f "$PMJ_GATE_PENDING" ]; then
  owed="$(tr '\n' ' ' <"$PMJ_GATE_PENDING" | tr -s ' ')"
  if ! safe_minute; then
    [ -n "$fired" ] && log "minute $now_m is not a safe minute — the drain waits (owed:$owed)"
  elif pmj_gate_armed && pmj_halt_blocks "$(pmj_halt_path)"; then
    log "pmj: exec.HALT names slot 11 — restart deferred; archive the halt line or pmj-flip.sh paper"
    pmj_halt_page "$(pmj_halt_path)"
  elif pmj_gate_armed; then
    log "$PMJ_GATE_TRIGGER — the pmj key-check decides this restart"
    if pmj_gate_pass; then
      log "pmj key-check PASSED — restart may proceed"
      engine_drain "$owed" && rm -f "$PMJ_GATE_PENDING"
    else
      log "restart DEFERRED by the pmj gate (owed:$owed); retention still runs"
    fi
  else
    if [ -f "$PMJ_GATE_FAILS" ] || [ -f "$PMJ_GATE_RED" ]; then
      log "slot 11 is armed nowhere any more — clearing the pmj gate's failure state"
    fi
    pmj_gate_clear
    engine_drain "$owed" && rm -f "$PMJ_GATE_PENDING"
  fi
fi

if [ "$retention" = 1 ]; then
  retention_sweep
  claimable_listing
fi
exit 0
