#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# HYPARB L1 — the HyperEVM MAINNET operator verbs (ruling O-HL1; plan §17).
#
#   evm-live.sh status [--token 0x…]…                  # read-only
#   evm-live.sh deploy --confirm                         # the H9d executor, from X
#   evm-live.sh wrap   --amount-wei <wei> --confirm      # HYPE -> WHYPE -> executor
#   evm-live.sh swap   --pool 0x… --zero-for-one true|false \
#                      --amount-raw <n> --min-out-raw <n> --confirm
#   evm-live.sh sweep  --token 0x… --amount-raw <n> --confirm   # executor -> X
#
#   add --network testnet to run the same verb on chain 998 ([testnet]);
#   add --hyparb <toml> for another artifact (default ~/multivenue/hyparb.toml).
#
# MAINNET BY DEFAULT. Every mainnet write spends real HYPE and is refused
# without --confirm. The binary checks the endpoint's chain id, the
# executor's bytes, and that X is slot 0's own wallet (O-HL3).
#
# Keys come from the environment: HYPEREVM_MAINNET_KEY and
# HYPERLIQUID_HYPARB_MASTER_ADDR from the REPO .env; with --network
# testnet, the testnet key from the operator's ~/multivenue/.env as
# evm-testnet.sh reads it. This script sources those files; the BINARY
# never opens either.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${EVM_LIVE_BIN:-$REPO_DIR/target/release/multivenue-engine}"

if [ ! -x "$BIN" ]; then
  echo "evm-live: $BIN is missing — run: cargo build --release -p cli" >&2
  exit 1
fi
if ! "$BIN" evm-live --help >/dev/null 2>&1; then
  echo "evm-live: $BIN has no 'evm-live' arm — it predates HYPARB L1." >&2
  exit 1
fi

if [ -f "$REPO_DIR/.env" ]; then
  set -a
  . "$REPO_DIR/.env"
  set +a
fi
TESTNET=0
for a in "$@"; do
  case "$a" in
    testnet|--network=testnet) TESTNET=1 ;;
  esac
done
if [ "$TESTNET" = 1 ]; then
  ENV_FILE="${MULTIVENUE_ENV_FILE:-$HOME/multivenue/.env}"
  if [ -f "$ENV_FILE" ]; then
    set -a
    . "$ENV_FILE"
    set +a
  fi
fi

exec "$BIN" evm-live "$@"
