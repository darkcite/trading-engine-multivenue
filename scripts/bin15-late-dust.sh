#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# BIN15 H90 L4 — the late window's LIVE dust test (ruling O-H90-3; vault
# doc 33 §L4 and §4). On the live out:BTC:15m instance, at each offset
# before expiry (default 25, 15, 8, 3 s): read both legs' l2Book, buy the
# favourite at its ask, ceil($1 / ask) whole contracts, IoC, expiresAfter
# 2 s — one JSON line each; then the account's fills and the settlement
# read back, and the spot USDC before and after.
#
#   bin15-late-dust.sh --dry-run                    # MAINNET reads only, signs nothing
#   bin15-late-dust.sh --network testnet            # a rehearsal on the smoke's account
#   bin15-late-dust.sh --confirm                    # MAINNET: <= 4 IoCs of ~$2, $10 hard cap
#   bin15-late-dust.sh --confirm --offsets 20,10,5,2 --instances 2
#
# Guards (the binary's): $10 total per invocation (a constant), --max-orders
# 1..8 (default 4), BTC 15m only, a quiet account (no open order, free spot
# USDC of $2 per planned order up to $10, the address budget, the agent's
# extraAgents approval valid an hour more), client ids that are not the
# engine's (no slot books a probe
# fill), mainnet sends refused without --confirm. This script refuses while
# strategy.conf arms slot 3 (the engine would sign as the same agent: nonce
# collisions) and while another probe runs.
#
# Exit 0 PASS (every order answered, nothing rested, nothing in doubt),
# 1 FAIL, 2 refused before anything was signed, 3 refused here.
#
# Keys come from the environment: HYPERLIQUID_* (mainnet, slot 3's account)
# or HYPERLIQUID_TESTNET_* from the REPO .env — or the file
# BIN15_DUST_ENV_FILE names. This script sources it; the BINARY never opens
# it. The run's log lands in $MULTIVENUE_DIR/research/outcome/h90/dust/.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${BIN15_DUST_BIN:-$REPO_DIR/target/release/multivenue-engine}"
ENV_FILE="${BIN15_DUST_ENV_FILE:-$REPO_DIR/.env}"
MV="${MULTIVENUE_DIR:-$HOME/multivenue}"
CONF="$MV/strategy.conf"
LOGDIR="$MV/research/outcome/h90/dust"

die() { print -r -- "bin15-late-dust: $*" >&2; exit 3; }

[ -x "$BIN" ] || die "$BIN is missing — run: cargo build --release -p cli"
"$BIN" hl-outcome-probe --help >/dev/null 2>&1 \
  || die "$BIN has no 'hl-outcome-probe' verb — it predates BIN15 H90 L4; rebuild"

# strategy.conf's ARM_LIVE (a comma list of slots): slot 3 armed means the
# engine signs as this agent — refuse rather than race its nonces.
kv_get() {
  awk -v k="$1" '
    { l = $0; sub(/^[ \t]+/, "", l); sub(/^export[ \t]+/, "", l) }
    index(l, k "=") == 1 { v = substr(l, length(k) + 2) }
    END { gsub(/^["'\'']|["'\'']$/, "", v); print v }' "$CONF"
}
if [ -f "$CONF" ]; then
  if print -r -- "$(kv_get ARM_LIVE)" | tr ',' '\n' | awk 'NF && $1 == "3" { f = 1 } END { exit !f }'; then
    die "strategy.conf arms slot 3 (ARM_LIVE=$(kv_get ARM_LIVE)) — the engine signs as this agent; flip slot 3 to paper first"
  fi
fi

if pgrep -f "multivenue-engine hl-outcome-probe" >/dev/null 2>&1; then
  die "another hl-outcome-probe is running — one signer at a time"
fi

if [ -f "$ENV_FILE" ]; then
  set -a
  . "$ENV_FILE"
  set +a
fi

mkdir -p "$LOGDIR" || die "cannot create $LOGDIR"
LOG="$LOGDIR/dust-$(date -u +%Y%m%dT%H%M%SZ).log"
print -r -- "bin15-late-dust: log $LOG" >&2
"$BIN" hl-outcome-probe "$@" 2>&1 | tee "$LOG"
rc=${pipestatus[1]}
print -r -- "bin15-late-dust: exit $rc — log $LOG" >&2
exit $rc
