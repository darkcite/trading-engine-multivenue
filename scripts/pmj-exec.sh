#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PMJ J16 — slot 11's order arm checked with slot 11's own wallet, through
# the binary that will sign (the scripts/pm15-exec.sh shape):
#
#   pmj-exec.sh self-test   # offline: RFC 8032, the guard's known answers on
#                           # the public Jupiter Predict fixtures, the keypair
#                           # file's parser
#   pmj-exec.sh key-check [--pmj PATH] [--cap-day-usd-1e6 N]
#                           # online, READ-ONLY (no order, no transaction):
#                           # pmj.toml's guard keys; FIRST the chain, from
#                           # public Solana RPC alone (no secret, no
#                           # Jupiter): the Predict program's deploy slot,
#                           # the vault's lookup-table entry and the deposit
#                           # mint's decimals against their pins; THEN the
#                           # 0600 keypair file, the wallet's balances
#                           # (amounts only; over N refuses), the API key
#                           # and the region; (PMJ J17) the wallet's
#                           # positions against pmj-held.state beside the
#                           # exec.toml in force — one unheld in two reads
#                           # 10 s apart refuses (78); one seen only in the
#                           # second read passes (it appeared between them:
#                           # the engine's own order in flight)
#
# The wallet — PMJ_WALLET_KEYFILE (the 0600 keypair file's absolute path,
# outside the repo and ~/multivenue), PMJ_WALLET_PUBKEY, JUP_API_KEY, and
# optionally PMJ_SOLANA_RPC (a bare host) — comes from the env file,
# MULTIVENUE_ENV_FILE (an absolute path; the meaning scripts/exec-smoke.sh
# gives it) when set, else the repo .env, sourced into the binary's
# environment exactly as scripts/engine-wrapper.sh sources it for the
# engine — only when it is a regular file (not a symlink) of this user's,
# mode 0600, that sets none of STRATEGY, ARM_LIVE, EXEC_TOML, PMJ_TOML (PMJ
# J17; else 78, the wording of deploy/ec2-pmj/pmj-wrapper.sh; the messages
# name the variable, never its value). This script prints none of it; the keypair
# file's bytes never pass through it. Exit: the binary's — 0 PASS; 77 a
# REVIEW TRIGGER (the Predict program's deploy slot, the vault's
# lookup-table entry or the deposit mint is not what was reviewed, or a
# pinned account no longer exists: scripts/daily-restart.sh halts a running
# slot 11 on it through exec.HALT and pages; the chain is checked first, so
# no 78 below can mask it); 78 any other refusal (the transport, a JSON-RPC
# error, a budget, the balances, the key, the files: a restart only
# defers). The refusal is on stderr.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${PMJ_BIN:-$REPO_DIR/target/release/multivenue-engine}"

case "${1:-}" in
  self-test|key-check) ;;
  *) echo "usage: pmj-exec.sh self-test | key-check [--pmj PATH] [--cap-day-usd-1e6 N]" >&2; exit 2 ;;
esac
verb="$1"
shift
if [ "$verb" = self-test ] && [ "$#" -gt 0 ]; then
  echo "pmj-exec: self-test takes no argument" >&2
  exit 2
fi
[ -x "$BIN" ] || { echo "pmj-exec: $BIN is missing — cargo build --release -p cli" >&2; exit 1; }
"$BIN" pmj-exec --help >/dev/null 2>&1 || { echo "pmj-exec: $BIN has no pmj-exec verb — it predates PMJ J16" >&2; exit 1; }
refuse() {
  echo "pmj-exec: refusing — $*" >&2
  exit 78
}
if [ "$verb" = key-check ]; then
  # The env file: MULTIVENUE_ENV_FILE when set (absolute; the meaning
  # scripts/exec-smoke.sh gives it — the operator's secrets file, wherever it
  # lives), else the repo's .env. Either is sourced only when it is a regular
  # 0600 file of ours (lstat: never a symlink), never printed (the check and
  # the wording of deploy/ec2-pmj/pmj-wrapper.sh), and sets no strategy.conf
  # key — PMJ J17, risk review round 3 MINOR 7. The refusals name the
  # variable, never its value; every one exits 78.
  ENV_FILE="${MULTIVENUE_ENV_FILE:-$REPO_DIR/.env}"
  ENV_LABEL="$REPO_DIR/.env"
  if [ -n "${MULTIVENUE_ENV_FILE:-}" ]; then
    ENV_LABEL=MULTIVENUE_ENV_FILE
    case "$MULTIVENUE_ENV_FILE" in
      /*) ;;
      *) refuse "MULTIVENUE_ENV_FILE is not an absolute path" ;;
    esac
  fi
  [ -L "$ENV_FILE" ] && refuse "$ENV_LABEL is a symlink"
  [ -e "$ENV_FILE" ] || refuse "$ENV_LABEL is missing (slot 11's wallet)"
  [ -f "$ENV_FILE" ] || refuse "$ENV_LABEL is not a regular file"
  if [ "$(uname -s)" = Darwin ]; then
    env_stat="$(stat -f '%Lp %Su' "$ENV_FILE")" || refuse "$ENV_LABEL cannot be stat'ed"
  else
    env_stat="$(stat -c '%a %U' "$ENV_FILE")" || refuse "$ENV_LABEL cannot be stat'ed"
  fi
  env_mode="${env_stat%% *}"
  env_owner="${env_stat#* }"
  [ "$env_mode" = 600 ] || refuse "$ENV_LABEL is mode $env_mode, not 600"
  [ "$env_owner" = "$(id -un)" ] || refuse "$ENV_LABEL is owned by $env_owner, not $(id -un)"
  # Sourced once in a subshell with the four strategy.conf keys unset: any
  # of them set afterwards was set by the file (the caller's own are not
  # judged).
  env_sets="$(unset STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML; . "$ENV_FILE" >/dev/null 2>&1
    for _k in STRATEGY ARM_LIVE EXEC_TOML PMJ_TOML; do [[ -v $_k ]] && print -rn -- "$_k "; done)"
  [ -z "$env_sets" ] || refuse "$ENV_LABEL sets ${env_sets% } — a strategy.conf key does not belong in the wallet's secrets file"
  unset env_stat env_mode env_owner env_sets ENV_LABEL
  set -a
  . "$ENV_FILE"
  set +a
fi
exec "$BIN" pmj-exec "--$verb" "$@"
