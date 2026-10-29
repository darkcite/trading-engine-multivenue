#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# pmj-wrapper.sh — the EC2 standalone's engine wrapper: the Linux
# counterpart of scripts/engine-wrapper.sh for an engine that runs slot 11
# (pmj) ALONE (docs/risk-policy.md ### J16, R-PMJ-S77: a slot-11 boot refusal
# aborts its whole engine, so slot 11 gets an engine of its own). systemd's
# multivenue-pmj.service runs it as the user `mv`; it execs the engine, so the
# unit's MainPID is the engine itself.
#
# Laws (the Mac wrapper's, kept):
# * The env file is MULTIVENUE_ENV_FILE (the unit sets
#   /home/mv/multivenue/.env; unset, the same path; it must be absolute) —
#   the one file scripts/pmj-exec.sh, pmj-flip.sh, pmj-dust.sh and
#   exec-smoke.sh read by the same variable; no release carries a .env. It
#   is SOURCED (set -a), never printed; it must be a regular file (never a
#   symlink), mode 0600, owned by the running user — anything else refuses.
#   It may not SET (even empty) STRATEGY, ARM_LIVE, EXEC_TOML or PMJ_TOML:
#   the switches live in strategy.conf alone (one place to read what the
#   next boot arms).
# * ~/multivenue/strategy.conf is SOURCED exactly as the Mac wrapper sources it
#   (KEY=VALUE shell syntax, the last assignment wins).
# * STRATEGY must be `pmj` and `multivenue-engine strategy-mask` must answer
#   `2048<TAB>pmj`; any other member, `ai`, `all` or an absent file REFUSES
#   (exit 78) — this host runs slot 11 alone.
# * EXEC_TOML and ARM_LIVE: both or neither (one alone refuses, exit 78), and
#   ARM_LIVE may name slot 11 only; arming also needs a release built from a
#   git transport (BUILD_INFO transport=git: an archive build carries no gate
#   line and no sha). The engine's own interlock (exec_boot: the artifact <=>
#   --arm-live, the PMJ-DUST-GATE line built in, the frame, slot 11 alone)
#   still has the last word.
# * `multivenue-engine pmj-check` (offline: universe.toml + pmj.toml) passes
#   before the exec, so a configuration error exits 78 — the unit then ends
#   FAILED and pages, instead of relaunching into the engine's refusal.
# * A key for a member this engine cannot boot (XMM_TOML, HCV_*, HYPARB_*,
#   EVM_*, HYPEREVM_PATH) is a misconfiguration and refuses.
# * ONE ENGINE: another `multivenue-engine run` on this host backs off.
# * G0: it never builds; it runs the release this file belongs to
#   (/opt/multivenue/<sha>/, resolved through the `current` link at start), and
#   refuses a binary built with target-cpu=native on another CPU model.
#
# Not ported from the Mac wrapper, on purpose: the worker's universe refresh,
# the regime / xsd / bin15 seed exports, the ruleset recommit and seed push
# (no worker and no ai.sock on this host; STRATEGY=pmj reads none of them),
# and macOS's launchd fd limit (systemd's LimitNOFILE=8192 sets it).
set -u

SELF="$(readlink -f "$0")"
KIT="${SELF%/*}"
REPO="$(cd "$KIT/../.." && pwd -P)" || exit 78
BIN="$REPO/target/release/multivenue-engine"
MV="$HOME/multivenue"
CONF="$MV/strategy.conf"
export MULTIVENUE_ENV_FILE="${MULTIVENUE_ENV_FILE:-$MV/.env}"
ENV_FILE="$MULTIVENUE_ENV_FILE"

say() { echo "pmj-wrapper: $*" >&2; }
refuse() {
  say "refusing — $*"
  exit 78
}

cd "$REPO" || exit 78
[ -x "$BIN" ] || refuse "$BIN is missing (deploy-linux.sh builds and installs it)"

if [ -r "$REPO/BUILD_INFO" ]; then
  say "release $REPO: $(grep -E '^(sha|transport|built_utc)=' "$REPO/BUILD_INFO" | paste -sd' ' -)"
  built_cpu="$(sed -n 's/^build_cpu=//p' "$REPO/BUILD_INFO")"
  here_cpu="$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)"
  if [ -n "$built_cpu" ] && [ "$built_cpu" != "$here_cpu" ]; then
    refuse "this release was built with target-cpu=native on '$built_cpu', and this CPU is '$here_cpu' — redeploy (deploy-linux.sh) to rebuild here"
  fi
else
  say "release $REPO has no BUILD_INFO (not installed by install-release.sh?)"
fi

if pgrep -f 'multivenue-engin[e] run' >/dev/null 2>&1; then
  say "another multivenue-engine run is live on this host — backing off (ONE ENGINE)"
  sleep 30
  exit 1
fi

# --- .env: a regular 0600 file of ours, sourced, never printed -------------
case "$ENV_FILE" in
  /*) ;;
  *) refuse "MULTIVENUE_ENV_FILE is not an absolute path" ;;
esac
[ -L "$ENV_FILE" ] && refuse "$ENV_FILE is a symlink"
[ -e "$ENV_FILE" ] || refuse "$ENV_FILE is missing (copy env.template, fill it on the host, chmod 600)"
[ -f "$ENV_FILE" ] || refuse "$ENV_FILE is not a regular file"
read -r env_mode env_owner < <(stat -c '%a %U' "$ENV_FILE")
[ "$env_mode" = 600 ] || refuse "$ENV_FILE is mode $env_mode, not 600"
[ "$env_owner" = "$(id -un)" ] || refuse "$ENV_FILE is owned by $env_owner, not $(id -un)"
unset STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML
set -a
# shellcheck disable=SC1090 # the operator's file, by design
. "$ENV_FILE"
set +a
for k in STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML; do
  [ -z "${!k+x}" ] || refuse "$ENV_FILE sets $k — the switches belong in strategy.conf alone"
done
# core_config::Config::load requires these four (lib.rs env_req); an empty one
# would boot into a DNS failure, so it refuses here by NAME.
for k in POLYMARKET_CLOB_HOST POLYMARKET_GAMMA_HOST BINANCE_WS_HOST ALCHEMY_HOST; do
  [ -n "${!k:-}" ] || refuse "$k is empty or absent in $ENV_FILE"
done

# --- strategy.conf ----------------------------------------------------------
[ -f "$CONF" ] || refuse "$CONF is missing (strategy.conf.template: STRATEGY=pmj)"
STRATEGY=""
# shellcheck disable=SC1090 # the operator's file, by design
. "$CONF"
[ "$STRATEGY" = "pmj" ] ||
  refuse "STRATEGY='$STRATEGY' — this host runs slot 11 alone: STRATEGY=pmj (risk-policy J16, R-PMJ-S77)"
resolved="$("$BIN" strategy-mask "$STRATEGY")" || refuse "strategy-mask refused '$STRATEGY'"
[ "$resolved" = $'2048\tpmj' ] || refuse "strategy-mask answered '$resolved', not 2048<TAB>pmj"
say "STRATEGY=pmj — mask 2048, members +pmj+"

for k in XMM_TOML HCV_TOML HCV_EVENTS HYPARB_TOML HYPARB_LIVE EVM_TESTNET EVM_HYBRID HYPEREVM_PATH; do
  [ -z "${!k:-}" ] || refuse "$k is set — no member of this engine reads it"
done

ulimit -S -n 8192 2>/dev/null || say "ulimit -n 8192 refused (hard $(ulimit -Hn))"
say "fd soft limit $(ulimit -Sn) (hard $(ulimit -Hn))"

# --- the two switches -----------------------------------------------------
EXEC_ARGS=()
if [ -n "${EXEC_TOML:-}" ] && [ -n "${ARM_LIVE:-}" ]; then
  [ -f "$EXEC_TOML" ] || refuse "EXEC_TOML=$EXEC_TOML is not a file"
  IFS=, read -r -a parts <<<"$ARM_LIVE"
  named=0
  for p in "${parts[@]}"; do
    p="${p//[[:space:]]/}"
    p="${p#+}"
    [ -n "$p" ] || continue
    [[ "$p" =~ ^[0-9]+$ ]] || refuse "ARM_LIVE='$ARM_LIVE' has a part that is not a slot number"
    [ "$((10#$p))" = 11 ] || refuse "ARM_LIVE='$ARM_LIVE' names slot $((10#$p)) — this engine arms slot 11 only"
    named=1
  done
  [ "$named" = 1 ] || refuse "ARM_LIVE='$ARM_LIVE' names no slot"
  transport="$(sed -n 's/^transport=//p' "$REPO/BUILD_INFO" 2>/dev/null)"
  [ "$transport" = git ] ||
    refuse "arming needs a release built with git (deploy-linux.sh --with-git); $REPO is transport='${transport:-unknown}' — no sha, no PMJ gate line"
  EXEC_ARGS=(--exec "$EXEC_TOML" --arm-live "$ARM_LIVE")
  say "ARMING slot 11 via $EXEC_TOML — real orders will be submitted to Jupiter Predict"
elif [ -n "${EXEC_TOML:-}${ARM_LIVE:-}" ]; then
  refuse "EXEC_TOML and ARM_LIVE must BOTH be set to arm (got EXEC_TOML='${EXEC_TOML:-}' ARM_LIVE='${ARM_LIVE:-}')"
fi

PMJ_ARGS=()
if [ -n "${PMJ_TOML:-}" ]; then
  [ -f "$PMJ_TOML" ] || refuse "PMJ_TOML=$PMJ_TOML is not a file"
  PMJ_ARGS=(--pmj "$PMJ_TOML")
fi

# The raw tap (HK19), the Mac wrapper's grammar, for the labels this engine
# subscribes (cli parse_raw_tap_flags): pm (the static market), pm5 (slot 11's
# 5-minute pipeline), bn, hl — comma-separated, each once — or `all` alone.
TAP_ARGS=()
if [ -n "${RAW_TAP:-}" ]; then
  case "$RAW_TAP" in
    ,* | *, | *,,*) refuse "RAW_TAP has an empty label (got '$RAW_TAP')" ;;
  esac
  IFS=, read -r -a labels <<<"$RAW_TAP"
  seen=","
  for l in "${labels[@]}"; do
    case "$l" in
      pm | pm5 | bn | hl) ;;
      all) [ "$RAW_TAP" = all ] || refuse "RAW_TAP=all must stand alone (got '$RAW_TAP')" ;;
      *) refuse "RAW_TAP label '$l' is not one of pm, pm5, bn, hl, all" ;;
    esac
    case "$seen" in *",$l,"*) refuse "RAW_TAP names '$l' twice" ;; esac
    seen="$seen$l,"
  done
  case "${RAW_TAP_MODE:-rejects}" in
    rejects | all) ;;
    *) refuse "RAW_TAP_MODE must be rejects or all (got '${RAW_TAP_MODE:-}')" ;;
  esac
  TAP_ARGS=(--raw-tap "$RAW_TAP" --raw-tap-mode "${RAW_TAP_MODE:-rejects}")
  if [ -n "${RAW_TAP_BUDGET_MB:-}" ]; then
    [[ "$RAW_TAP_BUDGET_MB" =~ ^[1-9][0-9]*$ ]] || refuse "RAW_TAP_BUDGET_MB must be a positive integer"
    TAP_ARGS+=(--raw-tap-budget-mb "$RAW_TAP_BUDGET_MB")
  fi
  say "raw tap ${TAP_ARGS[*]}"
fi

# The offline configuration check: the universe and pmj.toml, as the boot reads them.
if ! tell="$("$BIN" pmj-check "${PMJ_ARGS[@]}" 2>&1)"; then
  refuse "pmj-check: $(printf '%s' "$tell" | tail -n 3 | tr '\n' ' ')"
fi
say "pmj-check: ${tell:0:400}"

exec "$BIN" run --paper --strategy pmj "${EXEC_ARGS[@]}" "${PMJ_ARGS[@]}" "${TAP_ARGS[@]}"
