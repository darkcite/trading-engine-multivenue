#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# HYPARB L5 — flip slot 0 between PAPER and LIVE (plan §17.5, O-HL1).
#
#   hyparb-flip.sh status   # which mode strategy.conf + exec.toml name now
#   hyparb-flip.sh live     # PREFLIGHT (sends nothing), then the switches
#   hyparb-flip.sh paper    # back to paper + the testnet shadow
#
# PAPER = the member on ~/multivenue/hyparb.toml (mode = "testnet": paper
# fills + the chain-998 shadow). LIVE = the SAME member on
# ~/multivenue/hyparb-live.toml (mode = "live"): real swaps on HyperEVM
# mainnet through the H9d executor and real Hyperliquid perp hedges,
# both from slot 0's own wallet X, with the +$50 / −$20 session bound.
#
# The operator's command. It edits ONLY ~/multivenue/strategy.conf
# (HYPARB_LIVE, HYPARB_TOML, ARM_LIVE, EXEC_TOML, EVM_TESTNET,
# EVM_HYBRID, STRATEGY) and the [exec.slot.0] section of
# ~/multivenue/exec.toml, each after a timestamped .bak. It restarts
# NOTHING: the next restart applies it —
#
#   scripts/exec-smoke.sh && launchctl kickstart -k gui/$(id -u)/com.multivenue.engine
#
# `live` refuses unless the preflight passes on the exact files the
# engine will boot: the live artifact's grammar and live checks, the
# edited exec.toml + ARM_LIVE through the engine's own interlock, the
# mainnet chain, X funded, the executor's code and owner, X's
# Hyperliquid account bound and reconciled, a refused unfundable swap.
# A key it needs comes from the repo .env through scripts/evm-live.sh;
# this script reads no secret.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
EXEC="$MV/exec.toml"
PAPER_TOML="${HYPARB_PAPER_TOML:-$MV/hyparb.toml}"
LIVE_TOML="${HYPARB_LIVE_TOML:-$MV/hyparb-live.toml}"
REPO_DIR="${0:A:h:h}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="#hyparb-flip# "

die() { echo "hyparb-flip: $*" >&2; exit 1; }

[ -f "$CONF" ] || die "$CONF is missing"
[ -f "$EXEC" ] || die "$EXEC is missing"

# The value strategy.conf gives KEY (its last assignment), unquoted.
kv_get() {
  awk -F= -v k="$1" '$1 == k { v = substr($0, length(k) + 2) } END { gsub(/^["'\'']|["'\'']$/, "", v); print v }' "$CONF"
}

# Set KEY=VALUE in the file named by $2 (a work copy): the first
# assignment (live or flipped off by this script) is replaced, any later
# one is dropped, and a missing key is appended.
kv_set() {
  awk -v k="$1" -v v="$2" -v tag="$TAG" '
    { line = $0; if (index(line, tag) == 1) line = substr(line, length(tag) + 1) }
    index(line, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
    { print }
    END { if (!done) print k "=" v }' "$3" > "$3.new" && mv "$3.new" "$3"
}

# Comment KEY out of work copy $2 (tagged, so `paper` can restore it).
kv_off() {
  awk -v k="$1" -v tag="$TAG" 'index($0, k "=") == 1 { print tag $0; next } { print }' "$2" > "$2.new" && mv "$2.new" "$2"
}

# Is KEY commented out by this script in work copy $2? Its value if so.
kv_tagged() {
  awk -v k="$1" -v tag="$TAG" 'index($0, tag k "=") == 1 { v = substr($0, length(tag) + length(k) + 2) } END { print v }' "$2"
}

# ARM_LIVE with slot 0 added (`add`) or removed (`del`), sorted.
arm_with() {
  local cur="$(kv_get ARM_LIVE)" op="$1"
  print -r -- "$cur" | tr ',' '\n' | awk -v op="$op" 'NF && $1 != "0" { s[$1] = 1 } END { if (op == "add") s["0"] = 1; n = 0; for (k in s) a[n++] = k; for (i = 0; i < n; i++) for (j = i + 1; j < n; j++) if (a[j] + 0 < a[i] + 0) { t = a[i]; a[i] = a[j]; a[j] = t } out = ""; for (i = 0; i < n; i++) out = out (i ? "," : "") a[i]; print out }'
}

# STRATEGY with hyparb in it (the wrapper's allow-list decides).
strategy_with_hyparb() {
  local s="$(kv_get STRATEGY)"
  case "$s" in
    *hyparb*) print -r -- "$s" ;;
    "") print -r -- "ai+hyparb" ;;
    *) print -r -- "$s+hyparb" ;;
  esac
}

# Write work copy $2 of exec.toml with [exec.slot.0] mode = $1; exit 3
# when the section is absent, 4 when it has no `mode` line.
exec_mode() {
  awk -v mode="$1" '
    /^[ \t]*\[/ { insec = ($0 ~ /^[ \t]*\[exec\.slot\.0\][ \t]*(#.*)?$/); if (insec) found = 1 }
    insec && /^[ \t]*mode[ \t]*=/ { print "mode = \"" mode "\""; done = 1; next }
    { print }
    END { if (!found) exit 3; if (!done) exit 4 }' "$EXEC" > "$2"
}

# The slot 0 section `live` appends when exec.toml has none: sized for
# the minimal wallet (≈ $60 of executor inventory + 40 USDC of margin).
# Every key is exec.toml.example's; the P&L bound is O-HL5's.
slot0_section() {
  cat <<EOF

# Slot 0 — hyparb, added by scripts/hyparb-flip.sh $TS (plan §17.5).
# Its state (X's HL budget, the anchors) lives in hyparb/ beside this file.
[exec.slot.0]
mode = "$1"
name = "hyparb"
venues = ["hyperliquid", "hyperevm"]
max_order_usd_1e6 = 30000000
max_open_orders = 8
cap_day_usd_1e6 = 2000000000
cap_instance_usd_1e6 = 60000000
request_budget_floor = 2000
halt_on_reject_streak = 5
halt_on_recon_drift_usd_1e6 = 5000000
halt_on_ws_gap_ms = 30000
halt_on_asset_refusal_streak = 3
halt_on_recon_stale_ms = 120000
halt_on_gain_usd_1e6 = 50000000
halt_on_loss_usd_1e6 = 20000000
EOF
}

status() {
  local live="$(kv_get HYPARB_LIVE)" toml="$(kv_get HYPARB_TOML)" arm="$(kv_get ARM_LIVE)"
  local m="$(awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.0\]/) } s && /^[ \t]*mode[ \t]*=/ { gsub(/.*=[ \t]*"|".*/, ""); print }' "$EXEC")"
  echo "strategy.conf: STRATEGY=$(kv_get STRATEGY) HYPARB_LIVE=${live:-} HYPARB_TOML=${toml:-} EVM_TESTNET=$(kv_get EVM_TESTNET) ARM_LIVE=${arm:-} EXEC_TOML=$(kv_get EXEC_TOML)"
  echo "exec.toml [exec.slot.0] mode: ${m:-absent (paper)}"
  case ",$arm," in *,0,*) arm0=1 ;; *) arm0=0 ;; esac
  if [ "$live" = 1 ] && [ "$arm0" = 1 ] && [ "$m" = live ]; then
    echo "slot 0: LIVE at the next restart (mainnet, real money) via $toml"
  elif [ -z "$live" ] && [ "$arm0" = 0 ] && [ "$m" != live ]; then
    echo "slot 0: PAPER at the next restart via ${toml:-the default hyparb.toml}"
  else
    echo "slot 0: MIXED — the wrapper or the engine will REFUSE this; run live or paper"
  fi
}

case "${1:-}" in
  status) status ;;
  live)
    [ -f "$LIVE_TOML" ] || die "$LIVE_TOML is missing (the live artifact, plan §17.5)"
    work_conf="$(mktemp "$MV/.strategy.conf.flip.XXXXXX")"
    work_exec="$EXEC.flip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$work_exec"' EXIT
    cp "$CONF" "$work_conf"
    exec_mode live "$work_exec"
    rc=$?
    if [ "$rc" = 3 ]; then
      cp "$EXEC" "$work_exec" && slot0_section live >> "$work_exec"
    elif [ "$rc" != 0 ]; then
      die "$EXEC has an [exec.slot.0] section without a mode line — fix it by hand"
    fi
    arm="$(arm_with add)"
    exec_toml="$(kv_get EXEC_TOML)"
    [ -n "$exec_toml" ] || exec_toml="$EXEC"
    [ "$exec_toml" = "$EXEC" ] || die "EXEC_TOML=$exec_toml is not $EXEC — flip that file by hand"
    echo "hyparb-flip: PREFLIGHT on mainnet (nothing is sent) …" >&2
    "$REPO_DIR/scripts/evm-live.sh" arm-smoke --hyparb "$LIVE_TOML" \
      --exec "$work_exec" --arm-live "$arm" --no-trade --confirm \
      || die "PREFLIGHT FAILED — nothing was changed"
    cp -p "$CONF" "$CONF.bak-$TS-flip" && cp -p "$EXEC" "$EXEC.bak-$TS-flip" || die "backup failed"
    kv_set HYPARB_LIVE 1 "$work_conf"
    kv_set HYPARB_TOML "$LIVE_TOML" "$work_conf"
    kv_set EXEC_TOML "$EXEC" "$work_conf"
    kv_set ARM_LIVE "$arm" "$work_conf"
    kv_set STRATEGY "$(strategy_with_hyparb)" "$work_conf"
    kv_off EVM_TESTNET "$work_conf"
    kv_off EVM_HYBRID "$work_conf"
    cat "$work_exec" > "$EXEC" && cat "$work_conf" > "$CONF" || die "install failed — restore the .bak-$TS-flip copies"
    status
    echo "hyparb-flip: LIVE at the next restart. Backups: $CONF.bak-$TS-flip, $EXEC.bak-$TS-flip"
    ;;
  paper)
    work_conf="$(mktemp "$MV/.strategy.conf.flip.XXXXXX")"
    work_exec="$EXEC.flip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$work_exec"' EXIT
    cp "$CONF" "$work_conf"
    exec_mode paper "$work_exec"
    rc=$?
    if [ "$rc" = 3 ]; then
      cp "$EXEC" "$work_exec"
    elif [ "$rc" != 0 ]; then
      die "$EXEC has an [exec.slot.0] section without a mode line — fix it by hand"
    fi
    [ -f "$PAPER_TOML" ] || die "$PAPER_TOML is missing (the paper artifact)"
    arm="$(arm_with del)"
    cp -p "$CONF" "$CONF.bak-$TS-flip" && cp -p "$EXEC" "$EXEC.bak-$TS-flip" || die "backup failed"
    kv_off HYPARB_LIVE "$work_conf"
    kv_set HYPARB_TOML "$PAPER_TOML" "$work_conf"
    kv_set EVM_TESTNET 1 "$work_conf"
    hyb="$(kv_tagged EVM_HYBRID "$work_conf")"
    [ -n "$hyb" ] && kv_set EVM_HYBRID "$hyb" "$work_conf"
    if [ -n "$arm" ]; then
      kv_set ARM_LIVE "$arm" "$work_conf"
    else
      kv_off ARM_LIVE "$work_conf"
      kv_off EXEC_TOML "$work_conf"
    fi
    kv_set STRATEGY "$(strategy_with_hyparb)" "$work_conf"
    cat "$work_exec" > "$EXEC" && cat "$work_conf" > "$CONF" || die "install failed — restore the .bak-$TS-flip copies"
    status
    echo "hyparb-flip: PAPER at the next restart. Backups: $CONF.bak-$TS-flip, $EXEC.bak-$TS-flip"
    ;;
  *)
    echo "usage: hyparb-flip.sh status|live|paper" >&2
    exit 2
    ;;
esac
