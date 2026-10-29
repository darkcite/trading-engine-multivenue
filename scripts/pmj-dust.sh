#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PMJ J17 — the dust battery (plan-J5): small real $5 Limit BUYs on Jupiter
# Predict from slot 11's own wallet, BTC 5-minute only, every order recorded
# in ~/multivenue/research/pmj/dust/<battery>.jsonl (outside git), through
# the binary that would sign for the engine (the scripts/pmj-exec.sh shape):
#
#   pmj-dust.sh --battery ID --orders N --path execute [--dry-run]
#               [--seed S] [--pmj PATH]
#
#   --dry-run   builds, guards and records; never signs, never lands. It is
#               still NETWORKED (Jupiter, Gamma, the CLOB): run it only from
#               a region Jupiter serves.
#   --path rpc  refuses (landing path B, R-PMJ-S79 PROVISIONAL).
#
# The binary refuses before anything networked unless it was BUILT with the
# `PMJ-DUST-GATE: ruled YYYY-MM-DD` line in docs/risk-policy.md; while slot
# 11 is live anywhere on this host (strategy.conf's ARM_LIVE, any
# ~/multivenue/exec*.toml, the engine on 127.0.0.1:9191); while exec.HALT
# names slot 11 or pmj-dust.HALT stands (beside the exec.toml the engine
# boots from — strategy.conf's EXEC_TOML, absolute, else
# ~/multivenue/exec.toml — or in ~/multivenue; an unreadable one stands);
# while another pmj-dust holds ~/multivenue/pmj-dust.lock (the ledger and
# the lock live in ~/multivenue whatever EXEC_TOML says); on unreviewed
# pmj.toml guard keys, an unpinned guard_program_slot,
# guard_max_sol_lamports over 6 000 000 or guard_max_cu_price over 100 000
# (R-PMJ-S80, PROVISIONAL); on a failed offline self-test. Then, once a
# run, the chain half of the key check (public Solana RPC): a review
# trigger writes pmj-dust.HALT and exits 77. Its own interlock: $5 an
# order, $50 of deposits a UTC day (~/multivenue/pmj-dust-ledger.tsv, each
# order RESERVED and fsynced before it is signed), 40 orders a battery,
# BTC 5-minute only, the wallet at most $50 and at most 0.15 SOL at start
# (78), no order with less SOL than pmj.toml min_sol_lamports + 6 619 500
# lamports (75: one order's SOL bound; fund min_sol_lamports + n × 6 619 500
# for n orders), no execute within 1 s of its window's close (jup-gw's
# LATE rule), 2 h a run. Claims are never sent: the claimable positions are
# listed at the end, read-only.
#
# The wallet — PMJ_WALLET_KEYFILE (the 0600 keypair file, outside the repo
# and ~/multivenue), PMJ_WALLET_PUBKEY, JUP_API_KEY — comes from the env
# file: MULTIVENUE_ENV_FILE (an absolute path; the meaning
# scripts/exec-smoke.sh gives it) when set, else the repo .env — sourced
# only when it is a regular file (not a symlink) of this user's, mode
# 0600, that sets none of STRATEGY, ARM_LIVE, EXEC_TOML, PMJ_TOML (else 78;
# the messages name the variable, never its value), as scripts/pmj-exec.sh
# sources it. MULTIVENUE_ENV_FILE itself is the wrapper's: the binary never
# sees it. The binary then runs in
# a cleared environment (the effect of `env -i`): HOME, a fixed PATH, those
# three, and PMJ_SOLANA_RPC when set — every other variable is un-exported
# first (no value is ever put on a command line, where `ps` would show it;
# zsh adds only `_`, the binary's own path). MULTIVENUE_DIR set to anything
# but $HOME/multivenue refuses, 78 (the ledger, the halts and the lock live
# there). This script prints none of it; the keypair file's bytes never
# pass through it. Exit: the binary's — 0 the orders asked were placed; 75
# stopped, not sticky (3 failures in a row — a build, a discovery fetch, a
# balance read, an unread /state, a stale decision, an execute that never
# left the host — the day cap, the battery's 40, the 2 h bound); 77 a STICKY
# stop: pmj-dust.HALT was written with the
# reason (a guard refusal, an order not terminal by its deadline, the debit
# check, an unknown position, exec.HALT or slot 11 live mid-battery, an
# unreconcilable earlier reservation, a fill outside (0, 1) or above its
# limit or over the signed contracts, SOL falling past what a build may
# cost, a review trigger at the start) — nothing signs again until the
# operator reconciles the wallet and removes it; 78 refused before any
# order. The reason is on stderr.
#
# One wallet, one trader: while a battery holds pmj-dust.lock or its
# pmj-dust.HALT stands, slot 11 does not arm (the engine's boot and
# `pmj-flip.sh live` refuse). Before arming slot 11 after a battery, let
# every order end, then claim (by hand) or let settle every position it
# left — `pmj-exec --key-check` refuses while the wallet holds, in two
# reads 10 s apart, a position pmj-held.state does not — or arm slot 11 on
# a fresh wallet. These checks are per $HOME and per host: another user's
# ~/multivenue or another host running the same wallet is not seen.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${PMJ_BIN:-$REPO_DIR/target/release/multivenue-engine}"

if [ "$#" -eq 0 ]; then
  echo "usage: pmj-dust.sh --battery ID --orders N --path execute [--dry-run] [--seed S] [--pmj PATH]" >&2
  exit 2
fi
[ -x "$BIN" ] || { echo "pmj-dust: $BIN is missing — cargo build --release -p cli" >&2; exit 1; }
"$BIN" pmj-dust --help >/dev/null 2>&1 || { echo "pmj-dust: $BIN has no pmj-dust verb — it predates PMJ J17" >&2; exit 1; }
refuse() {
  echo "pmj-dust: refusing — $*" >&2
  exit 78
}
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
if [ -n "${MULTIVENUE_DIR:-}" ] && [ "$MULTIVENUE_DIR" != "$HOME/multivenue" ]; then
  refuse "MULTIVENUE_DIR is set ($MULTIVENUE_DIR) — the ledger, the halts and the lock live in $HOME/multivenue only; unset it"
fi
# The cleared environment (the effect of `env -i`, no value on a command
# line): un-export everything but the allowlist — zsh's own LOGNAME, PWD,
# OLDPWD and SHLVL included — then refuse if anything else is still
# exported.
for _pmj_n in ${(k)parameters}; do
  [[ ${parameters[$_pmj_n]} == *export* ]] || continue
  case $_pmj_n in
    HOME|PATH|PMJ_WALLET_KEYFILE|PMJ_WALLET_PUBKEY|JUP_API_KEY|PMJ_SOLANA_RPC) ;;
    *) typeset +x -- "$_pmj_n" 2>/dev/null || unset -- "$_pmj_n" 2>/dev/null ;;
  esac
done
for _pmj_n in ${(k)parameters}; do
  [[ ${parameters[$_pmj_n]} == *export* ]] || continue
  case $_pmj_n in
    HOME|PATH|PMJ_WALLET_KEYFILE|PMJ_WALLET_PUBKEY|JUP_API_KEY|PMJ_SOLANA_RPC) ;;
    *) refuse "$_pmj_n is still exported (the binary runs in a cleared environment)" ;;
  esac
done
unset _pmj_n
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
exec "$BIN" pmj-dust "$@"
