#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# HYPARB H8 — the HyperEVM TESTNET write path's operator verbs.
#
#   evm-testnet.sh status  [--hyparb <toml>]
#   evm-testnet.sh fund    --amount-wei <wei> [--hyparb <toml>]
#   evm-testnet.sh deploy  [--hyparb <toml>]
#   evm-testnet.sh mint    --token 0x… --amount-raw <n> [--hyparb <toml>]
#   evm-testnet.sh battery [--hyparb <toml>]     # DONE(H8); exit 0 = pass
#   evm-testnet.sh shadow-smoke [--decisions N] [--read-url https://…]
#                                                # the engine's shadow, hybrid,
#                                                # on synthetic decisions
#
# CHAIN 998 ONLY, by construction: `exec-hyperevm` cannot name chain 999
# and the binary refuses an endpoint that answers any other chain id.
# Testnet is an exec battery, not a market — no number these verbs print
# is a mainnet result.
#
# The wallet key comes from the environment: HYPEREVM_TESTNET_KEY, else
# (operator ruling 2026-09-23) the Hyperliquid TESTNET agent key. This
# script sources the operator's .env exactly as exec-smoke.sh does; the
# BINARY never opens that file.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${EVM_TESTNET_BIN:-$REPO_DIR/target/release/multivenue-engine}"

if [ ! -x "$BIN" ]; then
  echo "evm-testnet: $BIN is missing — run: cargo build --release -p cli" >&2
  exit 1
fi
if ! "$BIN" evm-testnet --help >/dev/null 2>&1; then
  echo "evm-testnet: $BIN has no 'evm-testnet' arm — it predates HYPARB H8." >&2
  exit 1
fi

ENV_FILE="${MULTIVENUE_ENV_FILE:-$HOME/multivenue/.env}"
if [ -f "$ENV_FILE" ]; then
  set -a
  . "$ENV_FILE"
  set +a
elif [ -f "$REPO_DIR/.env" ]; then
  set -a
  . "$REPO_DIR/.env"
  set +a
fi

exec "$BIN" evm-testnet "$@"
