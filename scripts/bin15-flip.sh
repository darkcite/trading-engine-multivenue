#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# BIN15 S7-L1 — flip slot 3 between PAPER and LIVE on Hyperliquid mainnet
# (docs/risk-policy.md, "S7-L1 — slot 3 LIVE on Hyperliquid mainnet").
#
#   bin15-flip.sh status                 # what the next boot runs, and what runs now
#   bin15-flip.sh live  [--switch-only]  # preflight, then switch + restart + verify
#   bin15-flip.sh paper [--switch-only]  # halt slot 3 now, then switch + restart + verify
#
# PARITY BY CONSTRUCTION. LIVE boots the paper artifact itself with only
# the four LIVE_SIZING lines changed: the entry law, both arms, the
# families and every other line are paper's, byte for byte. `live`
# keeps the paper artifact as bin15.paper-s7.toml (the nightly accrual
# reads that copy while it exists) and writes the live cut to
# bin15.toml; `paper` puts it back. exec.toml's [exec.slot.3] becomes
# slot3_section below (only its mode, back to "paper", on the way
# back). strategy.conf gains EXEC_TOML and 3 in ARM_LIVE; any other
# armed slot (HYPARB's 0) is kept.
#
# THE RESTART (law O-6) happens only in a safe minute: :04–:10, :19–:25,
# :34–:40 or :49–:55 UTC (SIGTERM at least 3 min before a settlement
# minute, so the 75–90 s boot and up to 60 s of seeding end before it), never
# 00:05–00:59Z (the nightly restart and accrual), never from 10 min
# before to 5 min after a scheduled drain (scripts/daily-restart.sh:
# 00:10, 08:33, 16:05, 20:15, 21:15), never while daily-restart owes a
# gated restart. The script waits for one, re-checks, THEN switches the
# files, so no unattended drain boots a half-made flip. It sends SIGTERM
# (the live arm's shutdown sweep cancels every resting order of ours),
# waits for launchd's relaunch and checks the boot tells.
# `--switch-only` skips the restart and its waits: the next restart,
# possibly an unattended scheduled drain, applies the files.
#
# `live` refuses unless the COMMITTED docs/risk-policy.md holds the S7-L1
# entry, the state is a clean PAPER one with slot 3 not running live,
# exec.HALT does not name slot 3, the paper artifact carries no price
# floor and no sizing key outside LIVE_SIZING, and scripts/exec-smoke.sh
# passes on the binary launchd boots (checked unchanged before the
# switch). The
# session anchor is archived: every `live` starts a new session. `paper`
# first halts slot 3 through exec.HALT when it runs live (the arm cancels
# our resting orders at once), waits for the halt and for the venue to
# show none of ours resting, then switches and restarts; it fails
# loudly unless the old process's shutdown sweep left nothing and the
# venue still shows nothing of ours. It archives the anchor and the
# `operator` halt of slot 3; any other halt reason stays in exec.HALT
# for a human, and `live` refuses until it is archived by hand. Every
# edited file is backed up as <file>.bak-<stamp>-bin15flip and replaced
# atomically, archives go to logs/, every step is logged to
# logs/bin15-flip.log. It reads no secret.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
EXEC="$MV/exec.toml"
B15="$MV/bin15.toml"
PAPER_COPY="$MV/bin15.paper-s7.toml"
HALT="$MV/exec.HALT"
ANCHOR="$MV/exec-pnl-anchor.state"
BUDGET_STATE="$MV/exec-budget.state"
GATE_PENDING="$MV/state/exec-gate-pending"
LOGDIR="$MV/logs"
OUT_LOG="$LOGDIR/launchd/engine.out.log"
ERR_LOG="$LOGDIR/launchd/engine.err.log"
FLIP_LOG="$LOGDIR/bin15-flip.log"
ENGINE_URL="http://127.0.0.1:9191"
INFO_URL="https://api.hyperliquid.xyz/info"
REPO_DIR="${0:A:h:h}"
BIN="$REPO_DIR/target/release/multivenue-engine"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="#bin15-flip# "
CUT_MARK="# bin15-flip LIVE cut"
ENGINE_PAT='venue-engin[e] run'
# LAW E-9: the cloid encodes the slot; slot 3's cloids start with this.
CLOID_PREFIX="0x4d5603"

# The S7-L1 frame (risk-policy.md): the member's sizing lines, key=value.
LIVE_SIZING="entry_usd_1e6=2000000 cap_day_usd_1e6=50000000 clip_qty_1e6=4000000 cap_instance_usd_1e6=10000000"

say() {
  print -r -- "bin15-flip: $*" >&2
  [ -d "$LOGDIR" ] && print -r -- "$(date -u +%FT%TZ) $*" >> "$FLIP_LOG"
}
die() { say "REFUSED/FAILED: $*"; exit 1; }

# ---- strategy.conf ------------------------------------------------------
#
# An assignment counts as the wrapper's `.` would run it: indented or
# `export`ed too. The awk programs strip both before matching.

# The value of the last active KEY= line, unquoted ("" when none).
kv_get() {
  awk -v k="$1" '
    { l = $0; sub(/^[ \t]+/, "", l); sub(/^export[ \t]+/, "", l) }
    index(l, k "=") == 1 { v = substr(l, length(k) + 2) }
    END { gsub(/^["'\'']|["'\'']$/, "", v); print v }' "$CONF"
}

# KEY VALUE FILE: the first KEY= line, active or switched off by this
# script, becomes KEY=VALUE; later ones are dropped; a missing key is
# appended.
kv_set() {
  awk -v k="$1" -v v="$2" -v tag="$TAG" '
    { l = $0; if (index(l, tag) == 1) l = substr(l, length(tag) + 1); sub(/^[ \t]+/, "", l); sub(/^export[ \t]+/, "", l) }
    index(l, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
    { print }
    END { if (!done) print k "=" v }' "$3" > "$3.new" && mv "$3.new" "$3" || die "cannot edit $3"
}

# KEY FILE: every active KEY= line switched off (commented, tagged).
kv_off() {
  awk -v k="$1" -v tag="$TAG" '
    { l = $0; sub(/^[ \t]+/, "", l); sub(/^export[ \t]+/, "", l) }
    index(l, k "=") == 1 { print tag $0; next }
    { print }' "$2" > "$2.new" && mv "$2.new" "$2" || die "cannot edit $2"
}

# ARM_LIVE is a comma list of slots 0-7 (anything else: refuse, never guess).
arm_ok() {
  print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 !~ /^[0-7]$/ { bad = 1 } END { exit bad }'
}

# add|del: ARM_LIVE with slot 3 added or removed, ascending.
arm_with() {
  print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk -v op="$1" '
    NF && $1 != "3" { s[$1 + 0] = 1 }
    END { if (op == "add") s[3] = 1; out = ""; for (i = 0; i < 8; i++) if (i in s) out = out (out == "" ? "" : ",") i; print out }'
}

arm_has_3() { print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 == "3" { f = 1 } END { exit !f }'; }

# EXEC_TOML as the wrapper's shell would expand a leading $HOME, ${HOME} or ~.
exec_toml_path() {
  local v
  v="$(kv_get EXEC_TOML)"
  v="${v/#\$\{HOME\}/$HOME}"; v="${v/#\$HOME/$HOME}"; v="${v/#\~/$HOME}"
  print -r -- "$v"
}

# EXEC_TOML must be unset or this script's exec.toml.
check_exec_toml() {
  local p
  p="$(exec_toml_path)"
  [ -z "$p" ] || [ "$p" = "$EXEC" ] || die "EXEC_TOML=$p is not $EXEC — flip by hand"
}

# ---- exec.toml ----------------------------------------------------------

# The mode [exec.slot.3] names in FILE ("" when absent).
slot3_mode() {
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.3\][ \t]*(#.*)?$/) } s && /^[ \t]*mode[ \t]*=/ { gsub(/.*=[ \t]*"|".*/, ""); print; exit }' "$1"
}

# FILE is shaped as the edits below assume: [exec] with enabled = 1, and
# slot 3's header, if any, spelled exactly `[exec.slot.3]`.
exec_shape_ok() {
  awk '
    /^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\][ \t]*(#.*)?$/) }
    s && /^[ \t]*enabled[ \t]*=[ \t]*1[ \t]*(#.*)?$/ { on = 1 }
    { l = $0; gsub(/[ \t"]/, "", l) }
    l ~ /^\[exec\.slot\.3\]/ && $0 !~ /^[ \t]*\[exec\.slot\.3\][ \t]*(#.*)?$/ { odd = 1 }
    END { exit !(on && !odd) }' "$1"
}

# The live section: exec.toml.example's keys at the S7-L1 frame.
slot3_section() {
  cat <<EOF
[exec.slot.3]
# bin15 LIVE (S7-L1), written by scripts/bin15-flip.sh $TS.
# docs/risk-policy.md "S7-L1" holds the why of every number.
mode = "live"
name = "bin15"
venues = ["hyperliquid"]
max_order_usd_1e6 = 10000000
max_open_orders = 24
cap_day_usd_1e6 = 50000000
cap_instance_usd_1e6 = 200000000
request_budget_floor = 2000
request_topup_weight = 5000
request_topup_day_max = 30000
halt_on_reject_streak = 5
halt_on_recon_drift_usd_1e6 = 2000000
halt_on_ws_gap_ms = 30000
halt_on_asset_refusal_streak = 3
halt_on_recon_stale_ms = 300000
halt_on_gain_usd_1e6 = 0
halt_on_loss_usd_1e6 = 54000000
EOF
}

# OUT: exec.toml with [exec.slot.3] replaced by slot3_section (appended
# when absent). Comments and blanks just above the NEXT header stay.
exec_live_into() {
  slot3_section > "$1.sec" || die "cannot write $1.sec"
  awk -v sec="$1.sec" '
    function put_sec(  l) { while ((getline l < sec) > 0) print l; close(sec) }
    /^[ \t]*\[/ {
      if (skip) { skip = 0; printf "%s", held; held = "" }
      if ($0 ~ /^[ \t]*\[exec\.slot\.3\][ \t]*(#.*)?$/) { put_sec(); done = 1; skip = 1; next }
    }
    skip { if (NF == 0 || $0 ~ /^[ \t]*#/) held = held $0 "\n"; else held = ""; next }
    { print }
    END { if (!done) { print ""; put_sec() } }' "$EXEC" > "$1" || die "cannot write $1"
  rm -f "$1.sec"
}

# OUT: exec.toml with [exec.slot.3] mode = "paper", nothing else touched.
exec_paper_into() {
  awk '
    /^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.3\][ \t]*(#.*)?$/) }
    s && /^[ \t]*mode[ \t]*=/ { print "mode = \"paper\""; next }
    { print }' "$EXEC" > "$1" || die "cannot write $1"
}

# ---- bin15.toml -----------------------------------------------------------

is_live_cut() { [ -f "$1" ] && [ "$(head -1 "$1")" = "$CUT_MARK" ]; }

sha() { shasum -a 256 "$1" | cut -c1-16; }

# PAPER: refuse a price floor (the ruling is "none") and any sizing key
# outside LIVE_SIZING (it would go live unsized).
paper_guard() {
  awk -v sizing="$LIVE_SIZING" '
    BEGIN { n = split(sizing, kv, " "); for (i = 1; i <= n; i++) { split(kv[i], p, "="); want[p[1]] = 1 } }
    match($0, /^[a-z0-9_]+[ \t]*=/) {
      k = substr($0, 1, RLENGTH - 1); sub(/[ \t]+$/, "", k)
      if ((k ~ /_usd_1e6$/ || k ~ /_qty_1e6$/) && !(k in want)) { print "sizing key `" k "` is not in LIVE_SIZING" > "/dev/stderr"; bad = 1 }
      if (k == "entry_min_px_1e6") { v = substr($0, RLENGTH + 1); gsub(/[ \t"]/, "", v); if (v + 0 != 0) { print "a price floor is set (entry_min_px_1e6)" > "/dev/stderr"; bad = 1 } }
    }
    END { exit bad }' "$1"
}

# PAPER OUT: the paper artifact with the LIVE_SIZING lines changed, each
# required exactly once. Every other line is copied byte for byte.
cut_live() {
  { print -r -- "$CUT_MARK"
    print -r -- "# Paper artifact sha256 $(shasum -a 256 "$1" | cut -d' ' -f1), cut $TS by"
    print -r -- "# scripts/bin15-flip.sh: only $LIVE_SIZING differ."
    awk -v sizing="$LIVE_SIZING" '
      BEGIN { n = split(sizing, kv, " "); for (i = 1; i <= n; i++) { split(kv[i], p, "="); want[p[1]] = p[2] } }
      match($0, /^[a-z0-9_]+[ \t]*=/) {
        k = substr($0, 1, RLENGTH - 1); sub(/[ \t]+$/, "", k)
        if (k in want) { seen[k]++; print substr($0, 1, RLENGTH) " " want[k]; next }
      }
      { print }
      END { for (k in want) if (seen[k] != 1) { print "`" k "` appears " seen[k] + 0 " times" > "/dev/stderr"; bad = 1 }; exit bad }' "$1"
  } > "$2"
}

# ---- exec.HALT --------------------------------------------------------------

# What parse_halt_file reads (halt.rs): per line, `slot=<n>` or a bare
# number, and `reason=<word>` (a bare line is the operator's). MODE
# `names`: exit 0 when a line halts slot 3; `drop`: print the file
# without slot 3's `operator` lines; `kept`: print slot 3's other lines.
halt_scan() {
  awk -v mode="$1" '
    /^[ \t]*#/ || NF == 0 { next }
    {
      slot = ""; why = "operator"
      for (i = 1; i <= NF; i++) {
        if (substr($i, 1, 5) == "slot=") slot = substr($i, 6)
        else if (substr($i, 1, 7) == "reason=") why = substr($i, 8)
        else if (slot == "") slot = $i
      }
      three = (slot ~ /^\+?[0-9]+$/ && slot + 0 == 3)
      if (three) f = 1
      if (mode == "drop" && !(three && why == "operator")) print
      if (mode == "kept" && three && why != "operator") print
    }
    END { if (mode == "names") exit !f }' "$HALT"
}

halt_names_3() { [ -f "$HALT" ] && halt_scan names; }

# Archive exec.HALT, then drop slot 3's `operator` halt. Any other
# reason for slot 3 stays for a human (and `live` refuses on it).
halt_drop_3() {
  local kept
  cp -p "$HALT" "$LOGDIR/exec.HALT.bin15-flip-$TS" || die "cannot archive $HALT"
  kept="$(halt_scan kept)"
  halt_scan drop > "$HALT.new" || die "cannot rewrite $HALT"
  if [ -s "$HALT.new" ]; then mv -f "$HALT.new" "$HALT"; else rm -f "$HALT.new" "$HALT"; fi
  say "exec.HALT: slot 3's operator halt cleared (archived as logs/exec.HALT.bin15-flip-$TS)"
  [ -z "$kept" ] || say "exec.HALT KEEPS for a human: $kept — read the log, then archive it by hand before any live"
}

archive_anchor() {
  [ -f "$ANCHOR" ] || return 0
  mv "$ANCHOR" "$LOGDIR/exec-pnl-anchor.state.bin15-flip-$1-$TS" || die "cannot archive $ANCHOR"
  say "session anchor archived as logs/exec-pnl-anchor.state.bin15-flip-$1-$TS"
}

# ---- the engine, the venue, the minute --------------------------------------

# A dotted /state path (list indices numeric), "" when it does not answer.
state_get() {
  curl -s -m 3 "$ENGINE_URL/state" 2>/dev/null | /usr/bin/python3 -c '
import json
import sys
try:
    v = json.load(sys.stdin)
    for k in sys.argv[1].split("."):
        v = v[int(k)] if isinstance(v, list) else v[k]
    print(v)
except Exception:
    pass' "$1"
}

# Does the RUNNING engine route slot 3 live? The per-slot metric family
# is registered only for a live slot (its value lags up to 5 s). No
# engine: not live. An engine that does not answer: refuse, never guess.
slot3_running_live() {
  local m
  m="$(curl -sf -m 3 "$ENGINE_URL/metrics" 2>/dev/null)"
  if [ -z "$m" ]; then
    pgrep -f "$ENGINE_PAT" > /dev/null && die "the running engine does not answer $ENGINE_URL/metrics — cannot tell whether slot 3 is live"
    return 1
  fi
  print -r -- "$m" | awk '$1 == "engine_exec_slot3_mode" { f = 1 } END { exit !f }'
}

# slot 3's halt word on /state: a reason, "none", or "" (no answer).
slot3_halt() { state_get exec.halted.3; }

# The operator arm's master (the budget file's first field; public).
master_addr() {
  [ -f "$BUDGET_STATE" ] && awk -F'\t' 'NR == 1 && $1 ~ /^0x[0-9a-fA-F]+$/ && length($1) == 42 { print $1 }' "$BUDGET_STATE"
}

# How many orders of slot 3 rest at the venue for MASTER ("" = unreadable).
venue_resting() {
  curl -s -m 10 -X POST "$INFO_URL" -H 'Content-Type: application/json' \
    -d "{\"type\":\"frontendOpenOrders\",\"user\":\"$1\"}" 2>/dev/null | /usr/bin/python3 -c '
import json
import sys
try:
    rows = json.load(sys.stdin)
    n = 0
    for r in rows:
        if str(r.get("cloid") or "").startswith(sys.argv[1]) or str(r.get("coin", "")).startswith("#"):
            n += 1
    print(n)
except Exception:
    pass' "$CLOID_PREFIX"
}

# Require the venue to show none of slot 3's orders resting (≤ 30 s).
venue_clear() {
  local m n i
  m="$(master_addr)"
  [ -n "$m" ] || die "cannot read the master address from $BUDGET_STATE — check the venue UI for open orders by hand"
  for i in {1..10}; do
    n="$(venue_resting "$m")"
    [ "$n" = 0 ] && { say "venue: no order of slot 3 resting for $m ($1)"; return 0; }
    sleep 3
  done
  die "venue shows ${n:-an unreadable answer for} order(s) of slot 3 resting for $m ($1) — press Cancel-All in the Hyperliquid UI"
}

strip_ansi() { sed -e $'s/\x1b\\[[0-9;]*m//g'; }

safe_minute() {
  local h m t d
  [ -f "$GATE_PENDING" ] && return 1
  h="$(date -u +%H)"; m="$(date -u +%M)"
  t=$(( 10#$h * 60 + 10#$m )); m=$(( 10#$m ))
  (( (m >= 4 && m <= 10) || (m >= 19 && m <= 25) || (m >= 34 && m <= 40) || (m >= 49 && m <= 55) )) || return 1
  (( t >= 5 && t <= 59 )) && return 1
  for d in 10 513 965 1215 1275; do
    (( t >= d - 10 && t <= d + 5 )) && return 1
  done
  return 0
}

wait_safe_minute() {
  local i
  for i in {1..300}; do
    safe_minute && return 0
    (( i == 1 )) && say "waiting for a safe minute (UTC now $(date -u +%H:%M)) …"
    sleep 15
  done
  [ -f "$GATE_PENDING" ] && die "no safe minute within 75 min: daily-restart still owes a gated restart ($GATE_PENDING)"
  die "no safe minute within 75 min"
}

# ---- the binary ---------------------------------------------------------------

SMOKE_SHA=""

# exec-smoke on the binary launchd boots; remember which bytes passed.
# ARG: what a failure leaves behind (default: nothing changed).
smoke() {
  local before
  before="$(shasum -a 256 "$BIN" | cut -d' ' -f1)" || die "cannot hash $BIN"
  say "PREFLIGHT: exec-smoke on $BIN (TESTNET, sends no order) …"
  ( cd "$REPO_DIR" && ./scripts/exec-smoke.sh ) >&2 || die "exec-smoke FAILED — ${1:-nothing was changed}"
  SMOKE_SHA="$(shasum -a 256 "$BIN" | cut -d' ' -f1)"
  [ "$SMOKE_SHA" = "$before" ] || die "$BIN changed during exec-smoke — run the flip again"
}

# The bytes about to boot are the ones exec-smoke passed.
same_binary() {
  [ "$(shasum -a 256 "$BIN" | cut -d' ' -f1)" = "$SMOKE_SHA" ] || die "$BIN changed since exec-smoke passed — run the flip again"
}

# ---- the restart -----------------------------------------------------------

OLD_PID=""
OFF_OUT=0
OFF_ERR=0

# SIGTERM the engine; return once the old process has exited.
stop_engine() {
  OLD_PID="$(pgrep -f "$ENGINE_PAT" | head -1)"
  [ -n "$OLD_PID" ] || die "no running engine to restart"
  OFF_OUT="$(stat -f %z "$OUT_LOG" 2>/dev/null || print 0)"
  OFF_ERR="$(stat -f %z "$ERR_LOG" 2>/dev/null || print 0)"
  say "SIGTERM to pid $OLD_PID (UTC $(date -u +%H:%M:%S))"
  pkill -TERM -f "$ENGINE_PAT"
  local i
  for i in {1..60}; do
    kill -0 "$OLD_PID" 2>/dev/null || return 0
    sleep 1
  done
  die "pid $OLD_PID did not exit within 60 s of SIGTERM — look before anything else"
}

since_out() { tail -c +$(( OFF_OUT + 1 )) "$OUT_LOG" 2>/dev/null | strip_ansi; }
since_err() { tail -c +$(( OFF_ERR + 1 )) "$ERR_LOG" 2>/dev/null | strip_ansi; }

# Wait (≤ 6 min) for launchd's relaunch to answer on /state; print its pid.
await_boot() {
  local i p
  for i in {1..120}; do
    p="$(state_get boot.pid)"
    # A booting engine answers pid 0 first, and `kill -0 0` always succeeds.
    if [[ "$p" == <1-> ]] && [ "$p" != "$OLD_PID" ] && kill -0 "$p" 2>/dev/null; then
      print -r -- "$p"
      return 0
    fi
    sleep 3
  done
  return 1
}

verify_live() {
  local pid i seeded line
  pid="$(await_boot)" || die "no engine answered $ENGINE_URL/state within 6 min — see $ERR_LOG, then: bin15-flip.sh paper"
  [ "$(state_get boot.paper)" = 0 ] || die "pid $pid booted PAPER — see $ERR_LOG"
  [ "$(state_get exec.configured)" = 1 ] || die "pid $pid: exec not configured — see $ERR_LOG"
  slot3_running_live || die "pid $pid does not route slot 3 live — see $ERR_LOG"
  [ "$(slot3_halt)" = none ] || die "pid $pid: slot 3 STARTED HALTED ($(slot3_halt))"
  since_err | grep -a -Eq 'ARMING slot\(s\) ([0-9]+,)*3(,[0-9]+)* via' || die "no 'ARMING slot(s) … 3' tell in $ERR_LOG"
  line="$(since_out | grep -a "exec: hyperliquid arm ARMED" | tail -1)"
  [ -n "$line" ] || die "no 'hyperliquid arm ARMED' tell in $OUT_LOG"
  print -r -- "$line" | grep -q 'MAINNET' || die "the arm is not on MAINNET: $line"
  say "pid $pid ARMED: $(print -r -- "$line" | grep -oE 'master=[^ ]*|budget_remaining=[^ ]*|boot_sweep_left=[^ ]*' | tr '\n' ' ')"
  for i in {1..60}; do
    seeded="$(state_get exec.seeded)"
    [ "$seeded" = 1 ] && break
    sleep 3
  done
  if [ "$seeded" = 1 ]; then
    say "SLOT 3 LIVE — seeded; budget_remaining=$(state_get exec.arm_budget_remaining) anchor_usd_1e6=$(state_get exec.arm_pnl_anchor_usd_1e6) (0 until the first reconciliation)"
  else
    say "SLOT 3 LIVE but NOT SEEDED after 3 min — every live order is refused until a reconciliation agrees; watch exec.seeded and exec.arm_recon_* on $ENGINE_URL/state"
  fi
}

# WAS_LIVE: slot 3 ran live in the process just stopped.
verify_paper() {
  local was_live="$1" pid line left
  pid="$(await_boot)" || die "no engine answered $ENGINE_URL/state within 6 min — see $ERR_LOG"
  if [ "$was_live" = 1 ]; then
    line="$(since_out | grep -a "exec: shutdown sweep of resting orders done" | tail -1)"
    left="$(print -r -- "$line" | grep -oE 'left=[0-9]+' | cut -d= -f2)"
    [ "$left" = 0 ] || die "the old process's shutdown sweep did not confirm a clear venue (${line:-no drain line}) — master $(master_addr): press Cancel-All in the Hyperliquid UI"
    say "the old process's drain: $(print -r -- "$line" | grep -oE 'cancelled_total=[0-9]+|left=[0-9]+' | tr '\n' ' ')"
    venue_clear "after the restart"
  fi
  slot3_running_live && die "pid $pid still routes slot 3 LIVE — see $ERR_LOG"
  say "SLOT 3 PAPER — pid $pid, boot.paper=$(state_get boot.paper) exec.configured=$(state_get exec.configured)"
}

# ---- status ------------------------------------------------------------------

next_mode() {
  local m
  m="$(slot3_mode "$EXEC")"
  if arm_has_3 && [ -n "$(exec_toml_path)" ] && [ "$m" = live ] && is_live_cut "$B15" && [ -f "$PAPER_COPY" ]; then
    print LIVE
  elif ! arm_has_3 && [ "$m" != live ] && ! is_live_cut "$B15" && [ ! -f "$PAPER_COPY" ]; then
    print PAPER
  else
    print MIXED
  fi
}

status() {
  print -r -- "strategy.conf: STRATEGY=$(kv_get STRATEGY) ARM_LIVE=$(kv_get ARM_LIVE) EXEC_TOML=$(kv_get EXEC_TOML)"
  print -r -- "exec.toml [exec.slot.3] mode: $(slot3_mode "$EXEC" | grep . || print absent)"
  if is_live_cut "$B15"; then print -r -- "bin15.toml: LIVE cut $(sha "$B15")"; else print -r -- "bin15.toml: paper artifact $(sha "$B15")"; fi
  [ -f "$PAPER_COPY" ] && print -r -- "bin15.paper-s7.toml: $(sha "$PAPER_COPY") (the accrual reads it)"
  halt_names_3 && print -r -- "exec.HALT halts slot 3: $(halt_scan kept | grep . || print operator)"
  [ -f "$GATE_PENDING" ] && print -r -- "daily-restart owes a gated restart: $(cat "$GATE_PENDING")"
  print -r -- "next boot: slot 3 $(next_mode)"
  if [ -z "$(curl -sf -m 3 "$ENGINE_URL/metrics" 2>/dev/null | head -c 1)" ]; then print -r -- "running: no engine answers $ENGINE_URL"
  elif slot3_running_live; then print -r -- "running: pid $(state_get boot.pid), slot 3 LIVE, halted=$(slot3_halt)"
  else print -r -- "running: pid $(state_get boot.pid), slot 3 not live"; fi
}

# ---- the flips ---------------------------------------------------------------

WORK=""
trap '[ -n "$WORK" ] && rm -f "$WORK" "$WORK.b15" "$WORK.exec" "$WORK.exec.sec" "$WORK.conf"' EXIT

backup() {
  local f
  for f in "$@"; do
    [ -f "$f" ] && { cp -p "$f" "$f.bak-$TS-bin15flip" || die "backup of $f failed"; }
  done
}

# SRC DST: replace DST atomically (a temp file beside it, then mv),
# keeping DST's permissions.
install_file() {
  local tmp="$2.new-$TS"
  cp "$1" "$tmp" && chmod "$(stat -f %Lp "$2")" "$tmp" && mv -f "$tmp" "$2" \
    || die "install of $2 failed — restore $2.bak-$TS-bin15flip"
}

# Everything `live` requires, before and again after the wait.
live_checks() {
  local cur
  cur="$(next_mode)"
  [ "$cur" = LIVE ] && { status; say "already LIVE at the next boot — nothing to do"; exit 0; }
  [ "$cur" = PAPER ] || die "the state is MIXED — run: bin15-flip.sh paper"
  slot3_running_live && die "slot 3 runs LIVE in the running engine — run: bin15-flip.sh paper"
  git -C "$REPO_DIR" show HEAD:docs/risk-policy.md 2>/dev/null | grep -q '^### S7-L1 — slot 3 LIVE' \
    || die "no S7-L1 entry in the committed docs/risk-policy.md (phased loosening: no arming without it)"
  case "+$(kv_get STRATEGY)+" in *+bin15+*) ;; *) die "STRATEGY=$(kv_get STRATEGY) does not run bin15" ;; esac
  arm_ok || die "ARM_LIVE=$(kv_get ARM_LIVE) is not a comma list of slots 0-7 — fix it by hand"
  check_exec_toml
  exec_shape_ok "$EXEC" || die "$EXEC lacks [exec] enabled = 1, or spells slot 3's header oddly — fix it by hand"
  halt_names_3 && die "$HALT halts slot 3 ($(halt_scan kept | grep . || print operator)) — read the log, archive it into logs/ by hand, then flip"
  [ -f "$GATE_PENDING" ] && die "daily-restart owes a gated restart ($(cat "$GATE_PENDING")) — wait for it"
  paper_guard "$B15" || die "the paper artifact is not flippable as is (above)"
}

# The restart needs an engine to stop: refuse before anything is switched.
engine_up() { pgrep -f "$ENGINE_PAT" > /dev/null || die "no running engine — nothing was changed"; }

flip_live() {
  local switch_only="$1"
  live_checks
  WORK="$(mktemp "$MV/.bin15flip.XXXXXX")" || die "mktemp failed"
  cut_live "$B15" "$WORK.b15" || die "the paper artifact cannot be cut (above)"
  exec_live_into "$WORK.exec"
  [ "$(slot3_mode "$WORK.exec")" = live ] && exec_shape_ok "$WORK.exec" || die "the edited exec.toml does not set slot 3 live"
  smoke
  if [ "$switch_only" != 1 ]; then
    engine_up
    while true; do
      wait_safe_minute
      live_checks
      safe_minute && break
    done
    engine_up
  fi
  # Cut again at the minute, from the paper artifact booting now.
  cut_live "$B15" "$WORK.b15" || die "the paper artifact cannot be cut (above)"
  exec_live_into "$WORK.exec"
  cp "$CONF" "$WORK.conf" || die "cannot copy $CONF"
  kv_set EXEC_TOML "$EXEC" "$WORK.conf"
  kv_set ARM_LIVE "$(arm_with add)" "$WORK.conf"
  same_binary
  backup "$CONF" "$EXEC" "$B15"
  cp -p "$B15" "$PAPER_COPY" || die "cannot keep the paper artifact as $PAPER_COPY"
  archive_anchor pre-live
  install_file "$WORK.b15" "$B15"
  install_file "$WORK.exec" "$EXEC"
  install_file "$WORK.conf" "$CONF"
  say "switched to LIVE: bin15.toml $(sha "$B15") cut from paper $(sha "$PAPER_COPY"), ARM_LIVE=$(kv_get ARM_LIVE)"
  diff "$PAPER_COPY" "$B15" | grep '^[<>] [a-z]' >&2
  if [ "$switch_only" = 1 ]; then
    say "--switch-only: LIVE at the next restart — which may be an unattended scheduled drain"
    return 0
  fi
  stop_engine
  verify_live
}

flip_paper() {
  local switch_only="$1" cur was_live=0 h="" i
  cur="$(next_mode)"
  slot3_running_live && was_live=1
  [ "$cur" = PAPER ] && [ "$was_live" = 0 ] && { status; say "already PAPER — nothing to do"; exit 0; }
  if is_live_cut "$B15"; then
    [ -f "$PAPER_COPY" ] || die "bin15.toml is a LIVE cut and $PAPER_COPY is missing — restore bin15.toml from a .bak by hand"
    is_live_cut "$PAPER_COPY" && die "$PAPER_COPY is itself a LIVE cut — restore by hand"
  fi
  arm_ok || die "ARM_LIVE=$(kv_get ARM_LIVE) is not a comma list of slots 0-7 — fix it by hand"
  check_exec_toml
  [ "$switch_only" = 1 ] && [ "$was_live" = 1 ] && die "slot 3 runs LIVE — run: bin15-flip.sh paper (it halts slot 3 first)"
  [ "$switch_only" = 1 ] || engine_up
  WORK="$(mktemp "$MV/.bin15flip.XXXXXX")" || die "mktemp failed"
  if [ "$was_live" = 1 ]; then
    if [ "$(slot3_halt)" = none ]; then
      say "halting slot 3 now through exec.HALT — the live arm cancels our resting orders"
      printf '\n3\n' >> "$HALT" || die "cannot write $HALT"
    fi
    for i in {1..30}; do
      h="$(slot3_halt)"
      [ -n "$h" ] && [ "$h" != none ] && break
      sleep 1
    done
    [ -n "$h" ] && [ "$h" != none ] || die "slot 3 did not halt within 30 s — check $HALT and $OUT_LOG"
    say "slot 3 halted ($h)"
    venue_clear "after the halt"
  fi
  # Another slot stays armed: this restart is an armed one, so it is
  # gated too — after the halt, which must never wait on the testnet.
  [ -z "$(arm_with del)" ] || [ "$switch_only" = 1 ] || smoke "slot 3 stays HALTED (if it ran live); nothing was switched"
  if [ "$switch_only" != 1 ]; then
    while true; do
      wait_safe_minute
      [ "$was_live" = 1 ] && venue_clear "at the minute"
      safe_minute && break
    done
    engine_up
  fi
  exec_paper_into "$WORK.exec"
  [ "$(slot3_mode "$WORK.exec")" != live ] || die "the edited exec.toml still sets slot 3 live — fix $EXEC by hand"
  [ -z "$(arm_with del)" ] || exec_shape_ok "$WORK.exec" || die "$EXEC lacks [exec] enabled = 1, or spells slot 3's header oddly — fix it by hand"
  cp "$CONF" "$WORK.conf" || die "cannot copy $CONF"
  if [ -n "$(arm_with del)" ]; then
    kv_set ARM_LIVE "$(arm_with del)" "$WORK.conf"
  else
    kv_off ARM_LIVE "$WORK.conf"
    kv_off EXEC_TOML "$WORK.conf"
  fi
  [ -z "$SMOKE_SHA" ] || same_binary
  backup "$CONF" "$EXEC" "$B15"
  install_file "$WORK.exec" "$EXEC"
  install_file "$WORK.conf" "$CONF"
  if is_live_cut "$B15"; then
    install_file "$PAPER_COPY" "$B15"
  fi
  if [ -f "$PAPER_COPY" ]; then
    mv "$PAPER_COPY" "$LOGDIR/bin15.paper-s7.toml.bin15-flip-$TS" || die "cannot archive $PAPER_COPY"
  fi
  say "switched to PAPER: bin15.toml $(sha "$B15"), ARM_LIVE=$(kv_get ARM_LIVE)"
  if [ "$switch_only" = 1 ]; then
    say "--switch-only: PAPER at the next restart (the anchor and exec.HALT are left as they are)"
    return 0
  fi
  stop_engine
  archive_anchor session-end
  halt_names_3 && halt_drop_3
  verify_paper "$was_live"
}

[ -f "$CONF" ] || die "$CONF is missing"
[ -f "$EXEC" ] || die "$EXEC is missing"
[ -f "$B15" ] || die "$B15 is missing"
[ -d "$LOGDIR" ] || die "$LOGDIR is missing"

SWITCH_ONLY=0
[ "${2:-}" = "--switch-only" ] && SWITCH_ONLY=1
[ -z "${2:-}" ] || [ "$SWITCH_ONLY" = 1 ] || { print -r -- "usage: bin15-flip.sh status|live|paper [--switch-only]" >&2; exit 2; }

case "${1:-}" in
  status) status ;;
  live) say "live requested ${2:-}"; flip_live "$SWITCH_ONLY"; status ;;
  paper) say "paper requested ${2:-}"; flip_paper "$SWITCH_ONLY"; status ;;
  *) print -r -- "usage: bin15-flip.sh status|live|paper [--switch-only]" >&2; exit 2 ;;
esac
