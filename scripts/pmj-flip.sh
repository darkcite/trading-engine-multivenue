#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PMJ J4 — slot 11 (pmj, H90 on Polymarket's 5-minute up/down markets
# through Jupiter Predict) is PAPER ONLY (docs/risk-policy.md, "PMJ — slot
# 11"): this script shows what the next boot runs for it and keeps it
# paper; it cannot arm it.
#
#   pmj-flip.sh status   # what the next boot runs for slot 11
#   pmj-flip.sh paper    # slot 11 out of ARM_LIVE, its section (if any) paper
#   pmj-flip.sh live     # REFUSED: out of scope until a PMJ-DUST-GATE ruling
#
# `paper` edits ONLY ~/multivenue/strategy.conf (ARM_LIVE) and, when an
# [exec.slot.11] section exists in the exec.toml the boot reads (EXEC_TOML,
# else ~/multivenue/exec.toml), that section's `mode` line — each after a
# stamped .bak. It restarts NOTHING: the next gated restart applies it
# (scripts/daily-restart.sh — at a safe minute, through the exec gate, slot
# 3 being LIVE). The engine itself refuses a live slot 11 whatever this
# script or the files say (cli::exec_boot, PMJ_SLOT). It reads no secret.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
HALT="$MV/exec.HALT"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="#pmj-flip# "

die() { echo "pmj-flip: $*" >&2; exit 1; }

[ -f "$CONF" ] || die "$CONF is missing"

# The value strategy.conf gives KEY (its last assignment), unquoted.
kv_get() {
  awk -F= -v k="$1" '$1 == k { v = substr($0, length(k) + 2) } END { gsub(/^["'\'']|["'\'']$/, "", v); print v }' "$CONF"
}

# The files the next boot reads (engine-wrapper.sh sources strategy.conf,
# so a leading $HOME or ~ expands there): EXEC_TOML, PMJ_TOML, or the
# defaults.
EXEC_CONF="$(kv_get EXEC_TOML)"
EXEC="${EXEC_CONF:-$MV/exec.toml}"
EXEC="${EXEC/#\$HOME/$HOME}"
EXEC="${EXEC/#\~/$HOME}"
PMJ_CONF="$(kv_get PMJ_TOML)"
PMJ="${PMJ_CONF:-$MV/pmj.toml}"
PMJ="${PMJ/#\$HOME/$HOME}"
PMJ="${PMJ/#\~/$HOME}"

# Set KEY=VALUE in work copy $3 (the first assignment replaced, any later
# one dropped, a missing key appended).
kv_set() {
  awk -v k="$1" -v v="$2" '
    index($0, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
    { print }
    END { if (!done) print k "=" v }' "$3" > "$3.new" && mv "$3.new" "$3"
}

# Comment KEY out of work copy $2 (tagged with $TAG; pm15-flip's shape).
kv_off() {
  awk -v k="$1" -v tag="$TAG" 'index($0, k "=") == 1 { print tag $0; next } { print }' "$2" > "$2.new" && mv "$2.new" "$2"
}

# ARM_LIVE without slot 11, sorted.
arm_without_11() {
  print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 != "11" { s[$1] = 1 } END { n = 0; for (k in s) a[n++] = k; for (i = 0; i < n; i++) for (j = i + 1; j < n; j++) if (a[j] + 0 < a[i] + 0) { t = a[i]; a[i] = a[j]; a[j] = t } out = ""; for (i = 0; i < n; i++) out = out (i ? "," : "") a[i]; print out }'
}

# The mode [exec.slot.11] names now (empty: no section, paper).
slot11_mode() {
  [ -f "$EXEC" ] || return 0
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\]/) } s && /^[ \t]*mode[ \t]*=/ { gsub(/.*=[ \t]*"|".*/, ""); print }' "$EXEC"
}

halt_names_11() {
  [ -f "$HALT" ] && grep -Eq '^[ \t]*(slot=)?11([ \t]|$)' "$HALT"
}

status() {
  local m="$(slot11_mode)" arm="$(kv_get ARM_LIVE)" ec="$(kv_get EXEC_TOML)" half=0
  echo "strategy.conf: STRATEGY=$(kv_get STRATEGY) ARM_LIVE=${arm:-} EXEC_TOML=$(kv_get EXEC_TOML) PMJ_TOML=${PMJ_CONF:-}"
  echo "exec.toml [exec.slot.11] mode: ${m:-absent (paper)}"
  [ -f "$PMJ" ] && echo "pmj.toml: $PMJ present" || echo "pmj.toml: $PMJ absent (a STRATEGY naming pmj refuses the boot)"
  case "$(kv_get STRATEGY)" in *pmj*) echo "STRATEGY names pmj: slot 11 runs (PAPER)" ;; *) echo "STRATEGY does not name pmj: slot 11 idle" ;; esac
  halt_names_11 && echo "exec.HALT names slot 11: it boots halted"
  case ",$arm," in *,11,*) arm11=1 ;; *) arm11=0 ;; esac
  # engine-wrapper.sh refuses EXEC_TOML without ARM_LIVE and the reverse.
  if { [ -n "$ec" ] && [ -z "$arm" ]; } || { [ -z "$ec" ] && [ -n "$arm" ]; }; then half=1; fi
  if [ "$half" = 1 ]; then
    echo "strategy.conf: EXEC_TOML and ARM_LIVE must be both set or both unset — engine-wrapper.sh REFUSES this boot; fix strategy.conf by hand"
  elif [ "$arm11" = 1 ] || [ "$m" = live ]; then
    echo "slot 11: marked LIVE somewhere — the engine will REFUSE this boot (PMJ-DUST-GATE); run: pmj-flip.sh paper"
  else
    echo "slot 11: PAPER at the next restart"
  fi
}

case "${1:-}" in
  status) status ;;
  live)
    die "live is out of scope: slot 11 is PAPER only until a PMJ-DUST-GATE line in docs/risk-policy.md (and the engine refuses a live slot 11 regardless)"
    ;;
  paper)
    work_conf="$(mktemp "$MV/.strategy.conf.pmjflip.XXXXXX")"
    trap 'rm -f "$work_conf" "$work_conf.new" "$EXEC.pmjflip-$TS"' EXIT
    cp "$CONF" "$work_conf"
    cp -p "$CONF" "$CONF.bak-$TS-pmjflip" || die "backup failed"
    # Slot 11 leaves ARM_LIVE; when it was the only armed slot, ARM_LIVE
    # and EXEC_TOML are commented out together (the wrapper refuses one
    # without the other: an empty ARM_LIVE would keep the engine down).
    case ",$(kv_get ARM_LIVE)," in
      *,11,*)
        if [ -n "$(arm_without_11)" ]; then
          kv_set ARM_LIVE "$(arm_without_11)" "$work_conf"
        else
          kv_off ARM_LIVE "$work_conf"
          kv_off EXEC_TOML "$work_conf"
        fi
        ;;
    esac
    if [ "$(slot11_mode)" = live ]; then
      cp -p "$EXEC" "$EXEC.bak-$TS-pmjflip" || die "backup failed"
      awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\]/) } s && /^[ \t]*mode[ \t]*=/ { print "mode = \"paper\""; next } { print }' "$EXEC" > "$EXEC.pmjflip-$TS" \
        || die "cannot write $EXEC.pmjflip-$TS"
      cat "$EXEC.pmjflip-$TS" > "$EXEC" || die "install failed — restore $EXEC.bak-$TS-pmjflip"
    fi
    cat "$work_conf" > "$CONF" || die "install failed — restore $CONF.bak-$TS-pmjflip"
    status
    echo "pmj-flip: PAPER at the next gated restart. Backup: $CONF.bak-$TS-pmjflip"
    ;;
  *)
    echo "usage: pmj-flip.sh status|paper (live is refused)" >&2
    exit 2
    ;;
esac
