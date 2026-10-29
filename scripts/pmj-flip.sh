#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PMJ J4 / J16 — flip slot 11 (pmj, H90 on Polymarket's 5-minute up/down
# markets through Jupiter Predict) between PAPER and LIVE at the DUST frame
# (docs/risk-policy.md, "PMJ — slot 11").
#
#   pmj-flip.sh status   # what the next boot runs for slot 11
#   pmj-flip.sh live     # the gates below, then the switches (DUST frame)
#   pmj-flip.sh paper    # slot 11 out of ARM_LIVE, its section (if any) paper
#
# LIVE is the DUST frame (plan-J5): [exec.slot.11] $5.01 an order and an
# instance, one open order, a $50 day cap, a -$50 session bound; pmj.toml's
# deposit $5, max order $5.01, $5 a market, -$50 a session, live_assets
# ["btc"]. The engine arms it only from a binary BUILT from a tree whose
# docs/risk-policy.md has the line `PMJ-DUST-GATE: ruled YYYY-MM-DD` outside
# code fences, in the tree AND at HEAD (cli build.rs ->
# cli::exec_boot::PMJ_GATES_BUILT), at or inside that frame — whatever this
# script or the files say; a frame beyond dust needs the dated
# `PMJ-LIVE-GATE: ruled YYYY-MM-DD` line too.
#
# `live` refuses unless:
#   - slot 11 would arm ALONE (risk-policy J16, R-PMJ-S77 — the engine
#     refuses a live slot 11 beside any other live slot, and launchd would
#     relaunch into that refusal): ARM_LIVE, read as the wrapper reads it
#     (strategy.conf sourced; each part trimmed, a leading `+` dropped,
#     compared as a number), names no slot but 11, and the exec.toml the
#     boot reads marks no other [exec.slot.N] live. These two are checked
#     first, before anything else is read;
#   - no dust battery holds the wallet (PMJ J17, risk review round 2 M3):
#     `multivenue-engine pmj-exec --dust-probe --exec <the exec.toml the
#     boot reads>` passes — no pmj-dust.HALT standing and no pmj-dust.lock
#     held (a non-blocking flock probe; macOS has no flock(1), so the
#     binary probes) beside that exec.toml or in $HOME/multivenue. A binary
#     that predates the probe refuses too (fail closed). The probe is per
#     $HOME and per host: another user's or another host's battery on the
#     same wallet is not seen;
#   - the COMMITTED docs/risk-policy.md has a line that BEGINS with
#     `PMJ-DUST-GATE: ruled YYYY-MM-DD` outside fenced code blocks (the
#     operator's ruling; a section quoting it opens nothing — the engine's
#     build.rs reads the same grammar, src/gate_line.rs);
#   - pmj.toml exists, `STRATEGY` names pmj, exec.HALT does not name slot
#     11, the exec.toml the boot reads exists;
#   - pmj.toml pins `guard_program_slot` (the reviewed Predict deploy);
#   - `multivenue-engine pmj-check` passes on the dust-frame pmj.toml;
#   - scripts/pmj-exec.sh key-check passes on it, with the wallet's deposit
#     balance at most the $50 day cap (risk-policy J3 requirement (a)) and
#     (PMJ J17) no position on the wallet that pmj-held.state beside the
#     exec.toml does not hold, in two reads 10 s apart (one seen only in
#     the second read passes) — a dust battery's legs are claimed or
#     settled first, or slot 11 arms on a fresh wallet. The key check
#     sources the wallet's env file itself: MULTIVENUE_ENV_FILE (absolute)
#     when set in this script's environment, else the repo .env (PMJ J17);
#     this script reads neither.
# Only then does it write, each file after a stamped .bak:
# ~/multivenue/strategy.conf (ARM_LIVE=11 — slot 11 alone — and EXEC_TOML), the
# [exec.slot.11] section of the exec.toml the boot reads (EXEC_TOML, else
# ~/multivenue/exec.toml), and pmj.toml's frame lines (PMJ_TOML, else
# ~/multivenue/pmj.toml); it sets the session anchor (pmj-pnl-anchor.state,
# beside exec.toml) aside into logs/ — every `live` starts a new session.
#
# `paper` edits strategy.conf (ARM_LIVE) and, when an [exec.slot.11]
# section exists, its `mode` line; then it restores pmj.toml's frame lines
# to R-PMJ-S22's paper frame (deposit $250, max order $250.01, $250 a
# market, -$3 500 a session; live_assets as is) after a stamped .bak, only
# when `pmj-check` passes on the result — the safe direction never waits
# on pmj.toml (a paper slot 11 at the dust frame trades nothing real).
#
# Neither restarts anything: the next gated restart applies it
# (scripts/daily-restart.sh — at a safe minute, through the exec gate, slot
# 3 being LIVE). To stop slot 11 before that, write `slot=11
# reason=operator` into exec.HALT. It reads no secret: the key check runs in
# its own process.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
HALT="$MV/exec.HALT"
LOGDIR="$MV/logs"
REPO_DIR="${0:A:h:h}"
BIN="${PMJ_BIN:-$REPO_DIR/target/release/multivenue-engine}"
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
ANCHOR="${EXEC:h}/pmj-pnl-anchor.state"

# Set KEY=VALUE in work copy $3: the first assignment (live or flipped
# off by this script) is replaced, any later one dropped, a missing key
# appended (pm15-flip's shape).
kv_set() {
  awk -v k="$1" -v v="$2" -v tag="$TAG" '
    { line = $0; if (index(line, tag) == 1) line = substr(line, length(tag) + 1) }
    index(line, k "=") == 1 { if (!done) { print k "=" v; done = 1 }; next }
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

# ARM_LIVE for a live slot 11: slot 11 ALONE (`live` has refused any
# other armed slot first — risk-policy J16, R-PMJ-S77).
arm_with_11() {
  print -r -- 11
}

# The value strategy.conf gives KEY as the wrapper sees it: the file
# SOURCED in a subshell (quotes, `export `, the last assignment winning),
# nothing it prints kept — daily-restart.sh's reading.
conf_sourced() {
  (
    unset "$1"
    set +u
    . "$CONF" >/dev/null 2>&1
    print -r -- "${(P)1:-}"
  )
}

# The slots ARM_LIVE names other than 11, comma-joined, read as the
# engine's parse_arm_live reads them: each comma-separated part trimmed
# (an empty one skipped), a leading `+` dropped, a decimal number compared
# as a number (`011` is 11); a part that is no number is listed as written
# (the engine would refuse it).
arm_live_others() {
  local spec part out=""
  spec="$(conf_sourced ARM_LIVE)"
  for part in "${(@s:,:)spec}"; do
    part="${part#"${part%%[![:space:]]*}"}"
    part="${part%"${part##*[![:space:]]}"}"
    [ -n "$part" ] || continue
    part="${part#+}"
    if [[ "$part" == <-> ]]; then
      (( 10#$part == 11 )) && continue
      part="$(( 10#$part ))"
    fi
    out="${out:+$out,}$part"
  done
  print -r -- "$out"
}

# The [exec.slot.N] other than 11 that the exec.toml the boot reads marks
# live, comma-joined (the engine's interlock counts every one).
exec_live_others() {
  [ -f "$EXEC" ] || return 0
  awk '
    /^[ \t]*\[/ {
      s = ""
      if (match($0, /^[ \t]*\[exec\.slot\.[0-9]+\]/)) { h = substr($0, RSTART, RLENGTH); gsub(/[^0-9]/, "", h); s = h + 0 }
    }
    s != "" && s != 11 && /^[ \t]*mode[ \t]*=[ \t]*"live"/ { if (!(s in seen)) { seen[s] = 1; out = out (out == "" ? "" : ",") s } }
    END { print out }' "$EXEC"
}

# The [exec.slot.11] section of the DUST frame (every key is
# exec.toml.example's; the parser requires each halt non-zero live).
slot11_section() {
  cat <<EOF
[exec.slot.11]
# pmj live (the DUST frame, plan-J5), written by scripts/pmj-flip.sh $TS.
# docs/risk-policy.md "PMJ — slot 11" holds the why of every number; the
# engine arms it only from a binary built with the PMJ-DUST-GATE line.
mode = "live"
name = "pmj"
venues = ["jupiter"]
max_order_usd_1e6 = 5010000
max_open_orders = 1
cap_instance_usd_1e6 = 5010000
cap_day_usd_1e6 = 50000000
halt_on_reject_streak = 5
halt_on_recon_drift_usd_1e6 = 1000000
halt_on_ws_gap_ms = 30000
halt_on_asset_refusal_streak = 3
halt_on_recon_stale_ms = 180000
halt_on_gain_usd_1e6 = 0
halt_on_loss_usd_1e6 = 50000000
EOF
}

# OUT $1: exec.toml with [exec.slot.11] replaced by slot11_section
# (appended when absent). Comments and blanks just above the NEXT header
# stay with it (pm15-flip's shape).
exec_into() {
  slot11_section > "$1.sec" || die "cannot write $1.sec"
  awk -v sec="$1.sec" '
    function put_sec(  l) { while ((getline l < sec) > 0) print l; close(sec) }
    /^[ \t]*\[/ {
      if (skip) { skip = 0; printf "%s", held; held = "" }
      if ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) { put_sec(); done = 1; skip = 1; next }
    }
    skip { if ($0 ~ /^[ \t]*(#.*)?$/) held = held $0 "\n"; else held = ""; next }
    { print }
    END { if (!done) { print ""; put_sec() } }' "$EXEC" > "$1" || die "cannot write $1"
  rm -f "$1.sec"
}

# OUT $1: pmj.toml with the frame lines $2..$5 (deposit, max order, per
# market, session loss; x1e6 USD) and, when $6 is non-empty, live_assets =
# $6; $7 names the frame. A key the file lacks is appended (pmj.toml is
# one [pmj] section, so an appended line is inside it); a key written over
# several lines is left for pmj-check to refuse. 1 on a write failure.
pmj_frame_into() {
  awk -v tag="# pmj-flip $TS: $7" -v dep="$2" -v mx="$3" -v mkt="$4" -v ses="$5" -v la="$6" '
    BEGIN {
      v["deposit_usd_1e6"] = dep; v["max_order_usd_1e6"] = mx
      v["usd_market_1e6"] = mkt; v["session_loss_usd_1e6"] = ses
      if (la != "") v["live_assets"] = la
    }
    /^[ \t]*[a-z_0-9]+[ \t]*=/ {
      k = $0; sub(/^[ \t]*/, "", k); sub(/[ \t]*=.*$/, "", k)
      if (k in v) { print k " = " v[k]; seen[k] = 1; next }
    }
    { print }
    END { first = 1; for (k in v) if (!(k in seen)) { if (first) { print tag; first = 0 } print k " = " v[k] } }' "$PMJ" > "$1"
}

set_anchor_aside() {
  [ -f "$ANCHOR" ] || return 0
  mkdir -p "$LOGDIR" && mv "$ANCHOR" "$LOGDIR/pmj-pnl-anchor.state.$TS" || die "cannot set $ANCHOR aside"
  echo "pmj-flip: the session anchor is set aside: $LOGDIR/pmj-pnl-anchor.state.$TS"
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
  [ -f "$ANCHOR" ] && echo "session anchor: $(cat "$ANCHOR")"
  if [ "$half" = 1 ]; then
    echo "strategy.conf: EXEC_TOML and ARM_LIVE must be both set or both unset — engine-wrapper.sh REFUSES this boot; fix strategy.conf by hand"
  elif [ "$arm11" = 1 ] && [ "$m" = live ]; then
    echo "slot 11: LIVE at the next restart (Jupiter, real money) — the engine arms it only if its binary was built with the PMJ-DUST-GATE line, else it REFUSES the boot"
  elif [ "$arm11" = 0 ] && [ "$m" != live ]; then
    echo "slot 11: PAPER at the next restart"
  else
    echo "slot 11: MIXED — the engine will REFUSE this boot; run: pmj-flip.sh live or paper"
  fi
}

case "${1:-}" in
  status) status ;;
  live)
    # Slot 11 arms only alone (R-PMJ-S77): checked first, before anything
    # else is read or run.
    others="$(arm_live_others)"
    [ -z "$others" ] \
      || die "ARM_LIVE names slot(s) $others — slot 11 arms only alone in its engine (risk-policy J16, R-PMJ-S77); run it on its own engine (the standalone)"
    others="$(exec_live_others)"
    [ -z "$others" ] \
      || die "$EXEC marks slot(s) $others live — slot 11 arms only alone in its engine (risk-policy J16, R-PMJ-S77); run it on its own engine (the standalone)"
    # PMJ J17: never beside a dust battery (its sticky halt or its lock).
    [ -x "$BIN" ] || die "$BIN is missing — cargo build --release -p cli"
    "$BIN" pmj-exec --dust-probe --exec "$EXEC" >/dev/null \
      || die "a dust battery holds this wallet (pmj-dust.HALT standing or pmj-dust.lock held beside $EXEC or in $HOME/multivenue), or $BIN predates PMJ J17's --dust-probe — slot 11 does not arm beside it (risk-policy J17)"
    git -C "$REPO_DIR" show HEAD:docs/risk-policy.md 2>/dev/null \
      | awk '{ t = $0; sub(/^[ \t]*/, "", t); c = substr(t, 1, 3) } c == "```" || c == "~~~" { if (f == "") f = c; else if (f == c) f = ""; next } f == "" && /^PMJ-DUST-GATE: ruled [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/ { found = 1 } END { exit !found }' \
      || die "the committed docs/risk-policy.md has no line beginning \`PMJ-DUST-GATE: ruled YYYY-MM-DD\` outside a code block — the dust gate is the operator's ruling (PMJ J16)"
    [ -f "$PMJ" ] || die "$PMJ is missing (slot 11's artifact)"
    case "$(kv_get STRATEGY)" in *pmj*) ;; *) die "STRATEGY does not name pmj" ;; esac
    halt_names_11 && die "exec.HALT names slot 11 — archive that line by hand first"
    [ -f "$EXEC" ] || die "$EXEC is missing — the exec artifact the boot reads"
    grep -Eq '^[ \t]*guard_program_slot[ \t]*=[ \t]*[1-9][0-9]*[ \t]*(#.*)?$' "$PMJ" \
      || die "$PMJ does not pin guard_program_slot — run scripts/pmj-exec.sh key-check, review the Predict program's deploy, then pin its slot"
    [ -x "$BIN" ] || die "$BIN is missing — cargo build --release -p cli"
    work_conf="$(mktemp "$MV/.strategy.conf.pmjflip.XXXXXX")"
    work_exec="$EXEC.pmjflip-$TS"
    work_pmj="$PMJ.pmjflip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$work_exec" "$work_exec.sec" "$work_pmj"' EXIT
    cp "$CONF" "$work_conf" || die "cannot copy $CONF"
    exec_into "$work_exec"
    pmj_frame_into "$work_pmj" 5000000 5010000 5000000 50000000 '["btc"]' "the DUST frame (plan-J5)" \
      || die "cannot write $work_pmj"
    echo "pmj-flip: pmj-check on the dust frame's pmj.toml …" >&2
    "$BIN" pmj-check --pmj "$work_pmj" >/dev/null || die "pmj-check refused the dust frame's pmj.toml — nothing was changed"
    echo "pmj-flip: the key check (the wallet, the reads, the pins; at most the \$50 day cap) …" >&2
    "$REPO_DIR/scripts/pmj-exec.sh" key-check --pmj "$work_pmj" --cap-day-usd-1e6 50000000 --exec "$EXEC" \
      || die "the key check FAILED — nothing was changed"
    cp -p "$CONF" "$CONF.bak-$TS-pmjflip" && cp -p "$EXEC" "$EXEC.bak-$TS-pmjflip" && cp -p "$PMJ" "$PMJ.bak-$TS-pmjflip" \
      || die "backup failed"
    kv_set EXEC_TOML "$EXEC" "$work_conf"
    kv_set ARM_LIVE "$(arm_with_11)" "$work_conf"
    set_anchor_aside
    cat "$work_exec" > "$EXEC" && cat "$work_pmj" > "$PMJ" && cat "$work_conf" > "$CONF" \
      || die "install failed — restore the .bak-$TS-pmjflip copies"
    status
    echo "pmj-flip: LIVE (the DUST frame) at the next gated restart, if the engine's binary carries the PMJ-DUST-GATE line. Backups: $CONF.bak-$TS-pmjflip, $EXEC.bak-$TS-pmjflip, $PMJ.bak-$TS-pmjflip"
    ;;
  paper)
    work_conf="$(mktemp "$MV/.strategy.conf.pmjflip.XXXXXX")"
    work_pmj="$PMJ.pmjflip-$TS"
    trap 'rm -f "$work_conf" "$work_conf.new" "$EXEC.pmjflip-$TS" "$work_pmj"' EXIT
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
    # R-PMJ-S22's paper frame back into pmj.toml (live_assets as is), only
    # when pmj-check passes on it; a failure warns and leaves pmj.toml as
    # it was — the switches above are already paper.
    if [ -f "$PMJ" ]; then
      if pmj_frame_into "$work_pmj" 250000000 250010000 250000000 3500000000 "" "the PAPER frame (R-PMJ-S22)" \
        && [ -x "$BIN" ] && "$BIN" pmj-check --pmj "$work_pmj" >/dev/null; then
        if cp -p "$PMJ" "$PMJ.bak-$TS-pmjflip" && cat "$work_pmj" > "$PMJ"; then
          echo "pmj-flip: pmj.toml is back at the paper frame (R-PMJ-S22). Backup: $PMJ.bak-$TS-pmjflip"
        else
          echo "pmj-flip: WARNING — pmj.toml's paper frame could not be installed; restore it by hand" >&2
        fi
      else
        echo "pmj-flip: WARNING — pmj-check refused the paper-frame pmj.toml (or $BIN is missing); pmj.toml is left as it was — restore R-PMJ-S22's frame by hand" >&2
      fi
    fi
    status
    echo "pmj-flip: PAPER at the next gated restart. Backup: $CONF.bak-$TS-pmjflip"
    ;;
  *)
    echo "usage: pmj-flip.sh status|live|paper" >&2
    exit 2
    ;;
esac
