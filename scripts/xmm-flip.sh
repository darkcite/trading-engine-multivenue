#!/bin/zsh -f
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# XMM XH5e — flip slot 6 between PAPER and the LIVE mainnet probe
# (docs/risk-policy.md, "XH5e-2"; run it with docs/xmm-mainnet-arming.md open).
#
#   xmm-flip.sh status   # what the next boot runs, and what runs now
#   xmm-flip.sh live     # PREFLIGHT (sends nothing), then the switches
#   xmm-flip.sh paper    # back to paper; a slot 6 configured live is halted first
#
# The operator's command. It edits ONLY ~/multivenue/strategy.conf
# (XMM_TOML, EXEC_TOML, ARM_LIVE) and the [exec.slot.6] section of
# ~/multivenue/exec.toml, each after a timestamped .bak; it installs the
# probe's member as ~/multivenue/xmm-probe.toml; and `paper` may append
# slot 6's operator halt to exec.HALT. It RESTARTS NOTHING — the operator
# restarts, watching the checklist's post-boot reads (SIGTERM drains; never
# `launchctl kickstart -k`, which is SIGKILL — docs/local-setup.md):
#
#   pkill -TERM -f 'multivenue-engine run'     # launchd's KeepAlive relaunches
#
# THE ARTIFACTS are this checkout's (XMM XH5e-1), never edited in
# ~/multivenue: xmm-probe.toml (the member) and exec-slot6-probe.toml (the
# section). This must be the checkout launchd runs (the engine plist's
# WorkingDirectory), so the preflight runs the binary launchd will run.
#
# `live` refuses unless: slot 6 is a clean PAPER state (its section absent,
# paper or off; 6 not in ARM_LIVE; XMM_TOML unset or the paper artifact) and
# the running engine does not route slot 6 live; STRATEGY boots xmm;
# EXEC_TOML is unset or this exec.toml; exec.HALT names slot 6 for no reason
# but the operator's (those lines are archived, then dropped); it is outside
# the quiet windows, no scheduled drain is due within 15 min and
# daily-restart owes no gated restart (an unattended restart must not boot
# the flip) nor is the wrapper mid-boot; the binary is newer than this
# checkout's HEAD and knows `xmm-preflight`; and `multivenue-engine
# xmm-preflight` PASSES on the work
# copies — the edited exec.toml written beside the installed one (its anchor
# and top-up state are the boot's), ARM_LIVE with 6, the probe's member, the
# STRATEGY the wrapper boots. The preflight runs from the checkout in the
# environment the wrapper builds — launchd's minimal one (the terminal's
# exports dropped), the repo .env sourced as the wrapper sources it and,
# with HYPARB_TOML and EVM_TESTNET set, the wrapper's two testnet key names
# taken from ~/multivenue/.env the same way — in a subshell with tracing
# off, nothing printed. It opens no socket and sends nothing; nothing is
# installed unless it passes.
#
# `paper` first halts slot 6 when exec.toml configures it live or the
# running engine routes it live (exec.HALT gains `slot=6 reason=operator`:
# the arm cancels slot 6's orders alone, XMM XH5a), then sets
# [exec.slot.6] mode = "paper", drops 6 from ARM_LIVE (EXEC_TOML and
# ARM_LIVE both off when nothing else is armed) and points XMM_TOML back to
# the paper artifact. The restart boots paper; the checklist's rollback
# follows. Every step is logged to logs/xmm-flip.log.
set -u

MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
EXEC="$MV/exec.toml"
HALT="$MV/exec.HALT"
PAPER_XMM="$MV/xmm.toml"
PROBE_XMM="$MV/xmm-probe.toml"
GATE_PENDING="$MV/state/exec-gate-pending"
LOGDIR="$MV/logs"
FLIP_LOG="$LOGDIR/xmm-flip.log"
ENGINE_URL="http://127.0.0.1:9191"
ENGINE_PAT='venue-engin[e] run'
REPO_DIR="${0:A:h:h}"
# The engine's launchd job. MULTIVENUE_DIR, XMM_FLIP_PLIST and (kept for the
# preflight) MULTIVENUE_ENV_FILE are a sandbox test's stand-ins: honoured
# only with XMM_FLIP_SANDBOX=1, refused otherwise (a stray export must not
# point the flip at files launchd does not boot).
PLIST="${XMM_FLIP_PLIST:-$HOME/Library/LaunchAgents/com.multivenue.engine.plist}"
SANDBOX="${XMM_FLIP_SANDBOX:-}"
WRAPPER_PAT='scripts/engine-wrapper\.s[h]'
BIN="$REPO_DIR/target/release/multivenue-engine"
SRC_XMM="$REPO_DIR/xmm-probe.toml"
SRC_SEC="$REPO_DIR/exec-slot6-probe.toml"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
TAG="#xmm-flip# "

say() {
  print -r -- "xmm-flip: $*" >&2
  [ -d "$LOGDIR" ] && print -r -- "$(date -u +%FT%TZ) $*" >> "$FLIP_LOG"
}
die() { say "REFUSED/FAILED: $*"; exit 1; }

# ---- strategy.conf (as bin15-flip.sh reads and writes it) ---------------
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

# ARM_LIVE is a comma list of BUILT slots 0-7 (anything else: refuse, never
# guess). 8-15 are reserved since S16 (no member): the engine would refuse
# to arm one at boot, AFTER this script restarts it (a launchd relaunch
# loop), so the script refuses first. A member's own lane widens this.
arm_ok() {
  print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 !~ /^[0-7]$/ { bad = 1 } END { exit bad }'
}

# add|del: ARM_LIVE with slot 6 added or removed, ascending.
arm_with() {
  print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk -v op="$1" '
    NF && $1 != "6" { s[$1 + 0] = 1 }
    END { if (op == "add") s[6] = 1; out = ""; for (i = 0; i < 16; i++) if (i in s) out = out (out == "" ? "" : ",") i; print out }'
}

arm_has_6() { print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 == "6" { f = 1 } END { exit !f }'; }

# A strategy.conf path as the wrapper's shell would expand a leading $HOME,
# ${HOME} or ~.
expand_home() {
  local v="$1"
  v="${v/#\$\{HOME\}/$HOME}"; v="${v/#\$HOME/$HOME}"; v="${v/#\~/$HOME}"
  print -r -- "$v"
}

# EXEC_TOML must be unset or this script's exec.toml.
check_exec_toml() {
  local p
  p="$(expand_home "$(kv_get EXEC_TOML)")"
  [ -z "$p" ] || [ "$p" = "$EXEC" ] || die "EXEC_TOML=$p is not $EXEC — flip by hand"
}

# ---- exec.toml -----------------------------------------------------------

# The mode [exec.slot.6] names in FILE ("" when absent).
slot6_mode() {
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.6\][ \t]*(#.*)?$/) } s && /^[ \t]*mode[ \t]*=/ { gsub(/.*=[ \t]*"|".*/, ""); print; exit }' "$1"
}

# FILE is shaped as the edits below assume: [exec] with enabled = 1, and
# slot 6's header, if any, spelled exactly `[exec.slot.6]`.
exec_shape_ok() {
  awk '
    /^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\][ \t]*(#.*)?$/) }
    s && /^[ \t]*enabled[ \t]*=[ \t]*1[ \t]*(#.*)?$/ { on = 1 }
    { l = $0; gsub(/[ \t"]/, "", l) }
    l ~ /^\[exec\.slot\.6\]/ && $0 !~ /^[ \t]*\[exec\.slot\.6\][ \t]*(#.*)?$/ { odd = 1 }
    END { exit !(on && !odd) }' "$1"
}

# OUT: exec.toml with [exec.slot.6] replaced by the probe's section
# (appended when absent): its header, a provenance line inside it (so the
# next flip replaces it with the section, nothing piles up), its keys.
# Comments and blanks just above the NEXT header stay.
exec_live_into() {
  awk -v ts="$TS" -v sha="$(shasum -a 256 "$SRC_SEC" | cut -d' ' -f1)" '
    f { print; next }
    /^\[exec\.slot\.6\][ \t]*$/ { print; print "# installed by scripts/xmm-flip.sh " ts " from exec-slot6-probe.toml sha256 " sha; f = 1 }
    END { exit !f }' "$SRC_SEC" > "$1.sec" || die "cannot cut the section from $SRC_SEC"
  awk -v sec="$1.sec" '
    function put_sec(  l) { while ((getline l < sec) > 0) print l; close(sec) }
    /^[ \t]*\[/ {
      if (skip) { skip = 0; printf "%s", held; held = "" }
      if ($0 ~ /^[ \t]*\[exec\.slot\.6\][ \t]*(#.*)?$/) { put_sec(); done = 1; skip = 1; next }
    }
    skip { if (NF == 0 || $0 ~ /^[ \t]*#/) held = held $0 "\n"; else held = ""; next }
    { print }
    END { if (!done) { print ""; put_sec() } }' "$EXEC" > "$1" || die "cannot write $1"
  rm -f "$1.sec"
}

# OUT: exec.toml with [exec.slot.6] mode = "paper", nothing else touched.
exec_paper_into() {
  awk '
    /^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.6\][ \t]*(#.*)?$/) }
    s && /^[ \t]*mode[ \t]*=/ { print "mode = \"paper\""; next }
    { print }' "$EXEC" > "$1" || die "cannot write $1"
}

# ---- exec.HALT ---------------------------------------------------------------

# What parse_halt_file reads (halt.rs): per line, `slot=<n>` or a bare
# number, and `reason=<word>` (a bare line is the operator's). MODE
# `names`: exit 0 when a line halts slot 6; `drop`: print the file without
# slot 6's `operator` lines; `kept`: print slot 6's other lines.
halt_scan() {
  awk -v mode="$1" '
    /^[ \t]*#/ || NF == 0 { if (mode == "drop") print; next }
    {
      slot = ""; why = "operator"
      for (i = 1; i <= NF; i++) {
        if (substr($i, 1, 5) == "slot=") slot = substr($i, 6)
        else if (substr($i, 1, 7) == "reason=") why = substr($i, 8)
        else if (slot == "") slot = $i
      }
      six = (slot ~ /^\+?[0-9]+$/ && slot + 0 == 6)
      if (six) f = 1
      if (mode == "drop" && !(six && why == "operator")) print
      if (mode == "kept" && six && why != "operator") print
    }
    END { if (mode == "names") exit !f }' "$HALT"
}

halt_names_6() { [ -f "$HALT" ] && halt_scan names; }

# Archive exec.HALT, then drop slot 6's `operator` lines (`live` refused
# earlier on any other reason).
halt_drop_6() {
  cp -p "$HALT" "$LOGDIR/exec.HALT.xmm-flip-$TS" || die "cannot archive $HALT"
  halt_scan drop > "$HALT.new" || die "cannot rewrite $HALT"
  if awk 'NF && $0 !~ /^[ \t]*#/ { f = 1 } END { exit !f }' "$HALT.new"; then
    mv -f "$HALT.new" "$HALT"
  else
    rm -f "$HALT.new" "$HALT"
  fi
  say "exec.HALT: slot 6's operator halt cleared (archived as logs/exec.HALT.xmm-flip-$TS)"
}

# Append slot 6's operator halt (the file archived first when present; a
# leading newline, so a hand-edited last line without one cannot swallow
# it), then read it back as the engine parses it.
halt_add_6() {
  if [ -f "$HALT" ]; then
    cp -p "$HALT" "$LOGDIR/exec.HALT.xmm-flip-$TS" || die "cannot archive $HALT"
  fi
  print -r -- $'\nslot=6 reason=operator' >> "$HALT" || die "cannot write $HALT — halt slot 6 by hand NOW"
  halt_names_6 || die "$HALT does not read as halting slot 6 — halt it by hand NOW"
  say "exec.HALT: slot 6 halted (slot=6 reason=operator) — the arm cancels slot 6's orders alone"
}

# ---- the engine, the minute, the binary --------------------------------------

# Does the RUNNING engine route slot 6 live? The per-slot metric family is
# registered only for a live slot. No engine: not live. An engine that does
# not answer: refuse, never guess.
slot6_running_live() {
  local m
  m="$(curl -sf -m 3 "$ENGINE_URL/metrics" 2>/dev/null)"
  if [ -z "$m" ]; then
    pgrep -f "$ENGINE_PAT" > /dev/null && die "the running engine does not answer $ENGINE_URL/metrics — cannot tell whether slot 6 is live"
    return 1
  fi
  print -r -- "$m" | awk '$1 == "engine_exec_slot6_mode" { f = 1 } END { exit !f }'
}

# Refuse inside the quiet windows (00:00-00:30, 08:25-08:45, 16:00-16:20Z),
# from 15 min before to 5 min after a scheduled drain (scripts/
# daily-restart.sh: 00:10, 08:33, 16:05, 20:15, 21:15Z), while
# daily-restart owes a gated restart, and while the wrapper is mid-boot (it
# reads strategy.conf before it execs the engine): an unattended boot would
# take the flip before the operator watches it.
minute_ok() {
  local h m t d
  [ -f "$GATE_PENDING" ] && die "daily-restart owes a gated restart ($GATE_PENDING) — flip after it"
  pgrep -f "$WRAPPER_PAT" > /dev/null && die "the engine's wrapper is mid-boot — flip after the boot"
  h="$(date -u +%H)"; m="$(date -u +%M)"
  t=$(( 10#$h * 60 + 10#$m ))
  (( t < 30 || (t >= 505 && t < 525) || (t >= 960 && t < 980) )) && die "inside a quiet window (UTC $h:$m)"
  for d in 10 513 965 1215 1275; do
    (( (t - d + 1440) % 1440 <= 5 || (d - t + 1440) % 1440 <= 15 )) && die "a scheduled drain is due near $(printf '%02d:%02d' $(( d / 60 )) $(( d % 60 )))Z (UTC now $h:$m) — flip after it"
  done
  return 0
}

# The binary launchd boots: the checkout its plist runs (WorkingDirectory),
# built after its HEAD, with the preflight in it.
binary_ok() {
  local head_s bin_s wrap
  wrap="$(plutil -extract WorkingDirectory raw -o - "$PLIST" 2>/dev/null)"
  [ -n "$wrap" ] || die "cannot read the engine's WorkingDirectory from $PLIST"
  [ "${REPO_DIR:A}" = "${wrap:A}" ] || die "this is $REPO_DIR, but launchd runs $wrap — run that checkout's scripts/xmm-flip.sh"
  [ -x "$BIN" ] || die "$BIN is missing — cargo build --release -p cli"
  head_s="$(git -C "$REPO_DIR" log -1 --format=%ct HEAD 2>/dev/null)" || die "cannot read $REPO_DIR's HEAD"
  bin_s="$(stat -f %m "$BIN")" || die "cannot stat $BIN"
  (( bin_s >= head_s )) || die "$BIN predates $REPO_DIR's HEAD — cargo build --release -p cli, then flip"
  "$BIN" xmm-preflight --help > /dev/null 2>&1 || die "$BIN has no xmm-preflight (built before XMM XH5e) — merge, rebuild, then flip"
}

# The preflight, in the environment the wrapper builds under launchd: the
# terminal's exports dropped (launchd starts the wrapper from a minimal
# environment, its plist adding none), then scripts/engine-wrapper.sh's own
# steps — the repo .env sourced; with HYPARB_TOML and EVM_TESTNET set and
# neither testnet key in it, those two names taken from ~/multivenue/.env by
# name. A subshell with tracing off; nothing is printed but the report.
# ARGS: exec, arm list, member, strategy.
preflight() {
  (
    unsetopt xtrace verbose
    set +u
    # What the scrub below must not take: this script's own values, read
    # first (a terminal that exports one of their names would lose it).
    local v k ef bin="$BIN" repo="$REPO_DIR" pick=""
    [ -n "$(kv_get HYPARB_TOML)" ] && [ -n "$(kv_get EVM_TESTNET)" ] && pick=1
    for v in ${(k)parameters[(R)*export*]}; do
      case "$v" in
        HOME|USER|LOGNAME|TMPDIR|SHELL|LANG) ;;
        MULTIVENUE_ENV_FILE) [ -n "$SANDBOX" ] || unset "$v" 2> /dev/null ;;
        *) unset "$v" 2> /dev/null ;;
      esac
    done
    export PATH=/usr/bin:/bin:/usr/sbin:/sbin
    cd "$repo" || exit 97
    if [ -f .env ]; then set -a; . ./.env > /dev/null 2>&1; set +a; fi
    if [ -n "$pick" ] && [ -z "${HYPEREVM_TESTNET_KEY:-}${HYPERLIQUID_TESTNET_AGENT_KEY:-}" ]; then
      ef="${MULTIVENUE_ENV_FILE:-$HOME/multivenue/.env}"
      if [ -f "$ef" ]; then
        for k in HYPEREVM_TESTNET_KEY HYPERLIQUID_TESTNET_AGENT_KEY; do
          v="$(. "$ef" > /dev/null 2>&1; printf '%s' "${(P)k:-}")"
          [ -n "$v" ] && export "$k=$v"
        done
      fi
    fi
    exec "$bin" xmm-preflight --exec "$1" --arm-live "$2" --xmm "$3" --strategy "$4"
  )
}

# ---- the verbs -------------------------------------------------------------------

status() {
  local m x
  m="$(slot6_mode "$EXEC")"
  x="$(expand_home "$(kv_get XMM_TOML)")"
  print -r -- "strategy.conf: STRATEGY=$(kv_get STRATEGY) XMM_TOML=${x:-} EXEC_TOML=$(kv_get EXEC_TOML) ARM_LIVE=$(kv_get ARM_LIVE)"
  print -r -- "exec.toml [exec.slot.6] mode: ${m:-absent (paper)}"
  if [ -f "$PROBE_XMM" ]; then
    if cmp -s "$PROBE_XMM" "$SRC_XMM"; then
      print -r -- "$PROBE_XMM: the checkout's xmm-probe.toml"
    else
      print -r -- "$PROBE_XMM: DIFFERS from the checkout's xmm-probe.toml (the next live re-installs it)"
    fi
  fi
  if halt_names_6; then
    if [ -n "$(halt_scan kept)" ]; then
      print -r -- "exec.HALT halts slot 6 for: $(halt_scan kept | tr '\n' ' ')— a human reads it"
    else
      print -r -- "exec.HALT halts slot 6 (the operator's; the next live drops it)"
    fi
  fi
  if [ "$m" = live ] && arm_has_6 && [ "$x" = "$PROBE_XMM" ]; then
    print -r -- "slot 6: LIVE at the next restart (the probe; the network is HYPERLIQUID_WS_HOST's — mainnet unless set)"
  elif [ "$m" != live ] && ! arm_has_6 && { [ -z "$x" ] || [ "$x" = "$PAPER_XMM" ]; }; then
    print -r -- "slot 6: PAPER at the next restart"
  else
    print -r -- "slot 6: MIXED — the wrapper or the engine will refuse or boot something unintended; run live or paper"
  fi
  if slot6_running_live; then
    print -r -- "the running engine routes slot 6 LIVE"
  else
    print -r -- "the running engine does not route slot 6 live"
  fi
}

if [ -z "$SANDBOX" ] && [ -n "${MULTIVENUE_DIR:-}${XMM_FLIP_PLIST:-}" ]; then
  die "MULTIVENUE_DIR or XMM_FLIP_PLIST is set — a sandbox test's stand-ins; unset them (launchd boots ~/multivenue and its own plist)"
fi
[ -f "$CONF" ] || die "$CONF is missing"
[ -f "$EXEC" ] || die "$EXEC is missing"

case "${1:-}" in
  status) status ;;
  live)
    [ -f "$SRC_XMM" ] && [ -f "$SRC_SEC" ] || die "the probe's artifacts are missing from $REPO_DIR (XMM XH5e-1)"
    arm_ok || die "ARM_LIVE=$(kv_get ARM_LIVE) is not a list of built slots 0-7 (8-15 are reserved)"
    check_exec_toml
    exec_shape_ok "$EXEC" || die "$EXEC is not shaped as this script edits it ([exec] enabled = 1; [exec.slot.6] spelled exactly) — flip by hand"
    case "$(slot6_mode "$EXEC")" in
      ""|paper|off) ;;
      *) die "$EXEC already configures slot 6 $(slot6_mode "$EXEC") — run status, then paper first" ;;
    esac
    arm_has_6 && die "ARM_LIVE already names 6 — run status, then paper first"
    x="$(expand_home "$(kv_get XMM_TOML)")"
    [ -z "$x" ] || [ "$x" = "$PAPER_XMM" ] || die "XMM_TOML=$x is not the paper artifact — flip by hand"
    slot6_running_live && die "the running engine routes slot 6 live — run paper and restart first"
    strategy="$(kv_get STRATEGY)"
    if halt_names_6 && [ -n "$(halt_scan kept)" ]; then
      die "exec.HALT halts slot 6 for: $(halt_scan kept | tr '\n' ' ')— read logs, then archive it by hand"
    fi
    minute_ok
    binary_ok
    arm="$(arm_with add)"
    work_exec="$EXEC.flip-$TS"
    work_xmm="$PROBE_XMM.flip-$TS"
    work_conf="$CONF.flip-$TS"
    trap 'rm -f "$work_exec" "$work_exec.sec" "$work_xmm" "$work_conf" "$work_conf.new" "$HALT.new"' EXIT
    exec_live_into "$work_exec"
    chmod "$(stat -f %Lp "$EXEC")" "$work_exec" || die "cannot set $work_exec's mode"
    cp "$SRC_XMM" "$work_xmm" || die "cannot copy $SRC_XMM"
    say "PREFLIGHT (sends nothing): slot 6 live, ARM_LIVE=$arm, STRATEGY=$strategy …"
    preflight "$work_exec" "$arm" "$work_xmm" "$strategy" 2>&1 | tee -a "$FLIP_LOG"
    (( ${pipestatus[1]} == 0 )) || die "PREFLIGHT FAILED — nothing was changed"
    cp -p "$CONF" "$CONF.bak-$TS-xmmflip" && cp -p "$EXEC" "$EXEC.bak-$TS-xmmflip" || die "backup failed — nothing was changed"
    [ -f "$PROBE_XMM" ] && { cp -p "$PROBE_XMM" "$PROBE_XMM.bak-$TS-xmmflip" || die "backup failed — nothing was changed"; }
    cp -p "$CONF" "$work_conf" || die "cannot copy $CONF"
    kv_set XMM_TOML "$PROBE_XMM" "$work_conf"
    kv_set EXEC_TOML "$EXEC" "$work_conf"
    kv_set ARM_LIVE "$arm" "$work_conf"
    halt_names_6 && halt_drop_6
    mv -f "$work_xmm" "$PROBE_XMM" && mv -f "$work_exec" "$EXEC" && mv -f "$work_conf" "$CONF" \
      || die "install failed — restore the .bak-$TS-xmmflip copies"
    status
    say "LIVE at the next restart (backups: *.bak-$TS-xmmflip). Restart by hand with the checklist open (§B10): pkill -TERM -f 'multivenue-engine run'"
    ;;
  paper)
    arm_ok || die "ARM_LIVE=$(kv_get ARM_LIVE) is not a list of built slots 0-7 (8-15 are reserved)"
    check_exec_toml
    exec_shape_ok "$EXEC" || die "$EXEC is not shaped as this script edits it — flip by hand (halt slot 6 first: echo 'slot=6 reason=operator' >> $HALT)"
    # The safe side: halt unless the file says paper AND the engine answers
    # without routing slot 6 live (a halt on a paper slot is a no-op).
    met="$(curl -sf -m 3 "$ENGINE_URL/metrics" 2>/dev/null)"
    if [ "$(slot6_mode "$EXEC")" = live ] \
      || print -r -- "$met" | awk '$1 == "engine_exec_slot6_mode" { f = 1 } END { exit !f }' \
      || { [ -z "$met" ] && pgrep -f "$ENGINE_PAT" > /dev/null; }; then
      halt_add_6
    fi
    arm="$(arm_with del)"
    work_exec="$EXEC.flip-$TS"
    work_conf="$CONF.flip-$TS"
    trap 'rm -f "$work_exec" "$work_conf" "$work_conf.new" "$HALT.new"' EXIT
    exec_paper_into "$work_exec"
    chmod "$(stat -f %Lp "$EXEC")" "$work_exec" || die "cannot set $work_exec's mode"
    cp -p "$CONF" "$work_conf" || die "cannot copy $CONF"
    if [ -n "$arm" ]; then
      kv_set ARM_LIVE "$arm" "$work_conf"
    else
      kv_off ARM_LIVE "$work_conf"
      kv_off EXEC_TOML "$work_conf"
    fi
    [ "$(expand_home "$(kv_get XMM_TOML)")" = "$PROBE_XMM" ] && kv_off XMM_TOML "$work_conf"
    cp -p "$CONF" "$CONF.bak-$TS-xmmflip" && cp -p "$EXEC" "$EXEC.bak-$TS-xmmflip" || die "backup failed — slot 6 is halted, nothing else changed"
    mv -f "$work_exec" "$EXEC" && mv -f "$work_conf" "$CONF" || die "install failed — restore the .bak-$TS-xmmflip copies"
    status
    say "PAPER at the next restart (backups: *.bak-$TS-xmmflip). Restart (pkill -TERM -f 'multivenue-engine run'), then the checklist's rollback (§F21-23): none of ours resting at the venue, the position flattened by hand, HYPERLIQUID_XMM_* out of .env, the session's anchor moved aside."
    ;;
  *)
    print -r -- "usage: xmm-flip.sh status|live|paper" >&2
    exit 2
    ;;
esac
