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
#                           # and the region
#
# The wallet — PMJ_WALLET_KEYFILE (the 0600 keypair file's absolute path,
# outside the repo and ~/multivenue), PMJ_WALLET_PUBKEY, JUP_API_KEY, and
# optionally PMJ_SOLANA_RPC (a bare host) — comes from the repo .env,
# sourced into the binary's environment exactly as scripts/engine-wrapper.sh
# sources it for the engine. This script prints none of it; the keypair
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
if [ "$verb" = key-check ]; then
  [ -f "$REPO_DIR/.env" ] || { echo "pmj-exec: $REPO_DIR/.env is missing (slot 11's wallet)" >&2; exit 1; }
  set -a
  . "$REPO_DIR/.env"
  set +a
fi
exec "$BIN" pmj-exec "--$verb" "$@"
