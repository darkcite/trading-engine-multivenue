#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PM15-6e — flip slot 9 (pm15, H90 on Polymarket's 15-minute up/down
# markets) between PAPER and LIVE (docs/risk-policy.md, "PM15").
#
#   pm15-flip.sh status   # what the next boot runs for slot 9
#   pm15-flip.sh live     # the gates below, then the switches
#   pm15-flip.sh paper    # back to paper
#
# LIVE is the R-PM-8 frame: $250 per order and per market, a -$500
# session loss bound, no day cap — `cap_day_usd_1e6` is $100 000, a value
# the member cannot reach (two live families x 96 windows x $250 =
# $48 000 a UTC day), because the config law refuses a day cap of 0
# ("UNSET, never unlimited"). Slot 9 trades from its own account on its
# own arm (exec_polymarket); `pm15.toml` is the same artifact in both
# modes, its `live_assets` (BTC and ETH, R-PM-4) the families that may
# arm.
#
# It edits ONLY ~/multivenue/exec.toml's [exec.slot.9] section and
# ~/multivenue/strategy.conf (ARM_LIVE, EXEC_TOML), each after a stamped
# .bak, and sets the session anchor (pm15-pnl-anchor.state) aside into
# logs/ — every `live` starts a new session, judged from the pUSD of its
# first boot. It restarts NOTHING: the next gated restart applies it
# (scripts/daily-restart.sh — at a safe minute, through the exec gate,
# slot 3 being LIVE). To stop slot 9 before that, write `slot=9
# reason=operator` into exec.HALT.
#
# `live` refuses unless:
#   - the COMMITTED docs/risk-policy.md has a line that BEGINS with
#     `PM15-LIVE-GATE: P0 PASS` (P0's result) or `PM15-LIVE-GATE: ruled`
#     (an operator ruling) — R-PM-1: one of them gates live;
#   - ~/multivenue/pm15.toml exists and `STRATEGY` names pm15;
#   - exec.HALT does not name slot 9;
#   - scripts/pm15-exec.sh key-check passes (the account, the venue gate,
#     the relayer key) on the binary launchd boots.
# It reads no secret: the key check runs in its own process.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
EXEC="$MV/exec.toml"
PM15="$MV/pm15.toml"
HALT="$MV/exec.HALT"
ANCHOR="$MV/pm15-pnl-anchor.state"
LOGDIR="$MV/logs"
REPO_DIR="${0:A:h:h}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="#pm15-flip# "

die() { echo "pm15-flip: $*" >&2; exit 1; }

[ -f "$CONF" ] || die "$CONF is missing"
[ -f "$EXEC" ] || die "$EXEC is missing"

# The value strategy.conf gives KEY (its last assignment), unquoted.
kv_get() {
  awk -F= -v k="$1" '$1 == k { v = substr($0, length(k) + 2) } END { gsub(/^["'\'']|["'\'']$/, "", v); print v }' "$CONF"
}

# Set KEY=VALUE in work copy $3: the first assignment (live or flipped
# off by this script) is replaced, any later one dropped, a missing key
# appended.
kv_set() {
  awk -v k="$1" -v v="$2" -v tag="$TAG" '
    { line = $0; if (index(line, tag) == 1) line = substr(line, length(tag) + 1) }
    index(line, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
    { print }
    END { if (!done) print k "=" v }' "$3" > "$3.new" && mv "$3.new" "$3"
}

# Comment KEY out of work copy $2 (tagged, so `live` can restore it).
kv_off() {
  awk -v k="$1" -v tag="$TAG" 'index($0, k "=") == 1 { print tag $0; next } { print }' "$2" > "$2.new" && mv "$2.new" "$2"
}

# ARM_LIVE with slot 9 added (`add`) or removed (`del`), sorted.
arm_with() {
  local cur="$(kv_get ARM_LIVE)" op="$1"
  print -r -- "$cur" | tr ',' '\n' | awk -v op="$op" 'NF && $1 != "9" { s[$1] = 1 } END { if (op == "add") s["9"] = 1; n = 0; for (k in s) a[n++] = k; for (i = 0; i < n; i++) for (j = i + 1; j < n; j++) if (a[j] + 0 < a[i] + 0) { t = a[i]; a[i] = a[j]; a[j] = t } out = ""; for (i = 0; i < n; i++) out = out (i ? "," : "") a[i]; print out }'
}

# The [exec.slot.9] section of mode $1 (the R-PM-8 frame; every key is
# exec.toml.example's).
slot9_section() {
  cat <<EOF
[exec.slot.9]
# pm15 $1 (R-PM-8), written by scripts/pm15-flip.sh $TS.
# docs/risk-policy.md "PM15" holds the why of every number.
mode = "$1"
name = "pm15"
venues = ["polymarket"]
max_order_usd_1e6 = 250000000
max_open_orders = 8
cap_instance_usd_1e6 = 250000000
cap_day_usd_1e6 = 100000000000
halt_on_reject_streak = 5
halt_on_recon_drift_usd_1e6 = 5000000
halt_on_ws_gap_ms = 30000
halt_on_asset_refusal_streak = 3
halt_on_recon_stale_ms = 180000
halt_on_gain_usd_1e6 = 0
halt_on_loss_usd_1e6 = 500000000
EOF
}

# OUT: exec.toml with [exec.slot.9] replaced by slot9_section $1
# (appended when absent). Comments and blanks just above the NEXT header
# stay with it.
exec_into() {
  slot9_section "$1" > "$2.sec" || die "cannot write $2.sec"
  awk -v sec="$2.sec" '
    function put_sec(  l) { while ((getline l < sec) > 0) print l; close(sec) }
    /^[ \t]*\[/ {
      if (skip) { skip = 0; printf "%s", held; held = "" }
      if ($0 ~ /^[ \t]*\[exec\.slot\.9\][ \t]*(#.*)?$/) { put_sec(); done = 1; skip = 1; next }
    }
    skip { if ($0 ~ /^[ \t]*(#.*)?$/) held = held $0 "\n"; else held = ""; next }
    { print }
    END { if (!done) { print ""; put_sec() } }' "$EXEC" > "$2" || die "cannot write $2"
  rm -f "$2.sec"
}

# The mode [exec.slot.9] names now (empty: no section, paper).
slot9_mode() {
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.9\]/) } s && /^[ \t]*mode[ \t]*=/ { gsub(/.*=[ \t]*"|".*/, ""); print }' "$EXEC"
}

halt_names_9() {
  [ -f "$HALT" ] && grep -Eq '^[ \t]*(slot=)?9([ \t]|$)' "$HALT"
}

set_anchor_aside() {
  [ -f "$ANCHOR" ] || return 0
  mkdir -p "$LOGDIR" && mv "$ANCHOR" "$LOGDIR/pm15-pnl-anchor.state.$TS" || die "cannot set $ANCHOR aside"
  echo "pm15-flip: the session anchor is set aside: $LOGDIR/pm15-pnl-anchor.state.$TS"
}

status() {
  local m="$(slot9_mode)" arm="$(kv_get ARM_LIVE)"
  echo "strategy.conf: STRATEGY=$(kv_get STRATEGY) ARM_LIVE=${arm:-} EXEC_TOML=$(kv_get EXEC_TOML)"
  echo "exec.toml [exec.slot.9] mode: ${m:-absent (paper)}"
  halt_names_9 && echo "exec.HALT names slot 9: it boots halted"
  [ -f "$ANCHOR" ] && echo "session anchor: $(cat "$ANCHOR")"
  case ",$arm," in *,9,*) arm9=1 ;; *) arm9=0 ;; esac
  if [ "$arm9" = 1 ] && [ "$m" = live ]; then
    echo "slot 9: LIVE at the next restart (Polymarket, real money)"
  elif [ "$arm9" = 0 ] && [ "$m" != live ]; then
    echo "slot 9: PAPER at the next restart"
  else
    echo "slot 9: MIXED — the engine will REFUSE this boot; run live or paper"
  fi
}

case "${1:-}" in
  status) status ;;
  live)
    git -C "$REPO_DIR" show HEAD:docs/risk-policy.md 2>/dev/null | grep -Eq '^PM15-LIVE-GATE: (P0 PASS|ruled)' \
      || die "the committed docs/risk-policy.md has no line beginning \`PM15-LIVE-GATE: P0 PASS\` or \`PM15-LIVE-GATE: ruled\` (R-PM-1)"
    [ -f "$PM15" ] || die "$PM15 is missing (slot 9's artifact)"
    case "$(kv_get STRATEGY)" in *pm15*) ;; *) die "STRATEGY does not name pm15" ;; esac
    halt_names_9 && die "exec.HALT names slot 9 — archive that line by hand first"
    exec_toml="$(kv_get EXEC_TOML)"
    [ -z "$exec_toml" ] || [ "$exec_toml" = "$EXEC" ] || die "EXEC_TOML=$exec_toml is not $EXEC — flip that file by hand"
    echo "pm15-flip: the key check (the account, the venue gate, the relayer key) …" >&2
    "$REPO_DIR/scripts/pm15-exec.sh" key-check || die "the key check FAILED — nothing was changed"
    work_conf="$(mktemp "$MV/.strategy.conf.pm15flip.XXXXXX")"
    work_exec="$EXEC.pm15flip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$work_exec" "$work_exec.sec"' EXIT
    cp "$CONF" "$work_conf" && exec_into live "$work_exec"
    cp -p "$CONF" "$CONF.bak-$TS-pm15flip" && cp -p "$EXEC" "$EXEC.bak-$TS-pm15flip" || die "backup failed"
    kv_set EXEC_TOML "$EXEC" "$work_conf"
    kv_set ARM_LIVE "$(arm_with add)" "$work_conf"
    set_anchor_aside
    cat "$work_exec" > "$EXEC" && cat "$work_conf" > "$CONF" || die "install failed — restore the .bak-$TS-pm15flip copies"
    status
    echo "pm15-flip: LIVE at the next gated restart. Backups: $CONF.bak-$TS-pm15flip, $EXEC.bak-$TS-pm15flip"
    ;;
  paper)
    work_conf="$(mktemp "$MV/.strategy.conf.pm15flip.XXXXXX")"
    work_exec="$EXEC.pm15flip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$work_exec" "$work_exec.sec"' EXIT
    cp "$CONF" "$work_conf"
    if [ -n "$(slot9_mode)" ]; then
      exec_into paper "$work_exec"
    else
      cp "$EXEC" "$work_exec"
    fi
    cp -p "$CONF" "$CONF.bak-$TS-pm15flip" && cp -p "$EXEC" "$EXEC.bak-$TS-pm15flip" || die "backup failed"
    arm="$(arm_with del)"
    if [ -n "$arm" ]; then
      kv_set ARM_LIVE "$arm" "$work_conf"
    else
      kv_off ARM_LIVE "$work_conf"
      kv_off EXEC_TOML "$work_conf"
    fi
    set_anchor_aside
    cat "$work_exec" > "$EXEC" && cat "$work_conf" > "$CONF" || die "install failed — restore the .bak-$TS-pm15flip copies"
    status
    echo "pm15-flip: PAPER at the next gated restart. Backups: $CONF.bak-$TS-pm15flip, $EXEC.bak-$TS-pm15flip"
    ;;
  *)
    echo "usage: pm15-flip.sh status|live|paper" >&2
    exit 2
    ;;
esac
