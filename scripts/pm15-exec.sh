#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# PM15-6c/6e — slot 9's order arm checked against Polymarket with slot 9's
# own account, through the binary that will sign:
#
#   pm15-exec.sh self-test    # offline: every pinned signing vector
#   pm15-exec.sh key-check    # online, read-only: CLOB V2, the account open,
#                             # pUSD, the exchange's allowance, the relayer key
#   pm15-exec.sh miss-probe   # online, ONE order: a $1 FAK BUY priced at the
#                             # tick on BTC's current window — it cannot fill;
#                             # the venue's "no match" proves the order path
#   pm15-exec.sh polybolt-probe [--assets btc,eth] [--seconds 120]
#                             # online, no order: PolyBolt's prices and TWAPs
#                             # on the account's API credentials (PM15-7)
#
# The account — PM15_SIG_TYPE, PM15_OWNER_KEY, PM15_WALLET, PM15_API_KEY,
# PM15_API_SECRET, PM15_API_PASSPHRASE, and PM15_RELAYER_API_KEY or
# PM15_REDEEM=hand — comes from the repo .env, sourced into the binary's
# environment exactly as scripts/engine-wrapper.sh sources it for the
# engine. This script prints none of it. Exit: the binary's (0 PASS, 78 a
# refusal on stderr).
set -u

REPO_DIR="${0:A:h:h}"
BIN="${PM15_BIN:-$REPO_DIR/target/release/multivenue-engine}"

case "${1:-}" in
  self-test|key-check|miss-probe|polybolt-probe) ;;
  *) echo "usage: pm15-exec.sh self-test|key-check|miss-probe|polybolt-probe [args]" >&2; exit 2 ;;
esac
[ -x "$BIN" ] || { echo "pm15-exec: $BIN is missing — cargo build --release -p cli" >&2; exit 1; }
"$BIN" pm-exec --help >/dev/null 2>&1 || { echo "pm15-exec: $BIN has no pm-exec verb — it predates PM15-6c" >&2; exit 1; }
if [ "$1" != self-test ]; then
  [ -f "$REPO_DIR/.env" ] || { echo "pm15-exec: $REPO_DIR/.env is missing (slot 9's account)" >&2; exit 1; }
  set -a
  . "$REPO_DIR/.env"
  set +a
fi
if [ "$1" = polybolt-probe ]; then
  shift
  exec "$BIN" pm15-polybolt-probe "$@"
fi
exec "$BIN" pm-exec "--$1"
