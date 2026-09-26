#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# HC9 — the Hypercall order arm's MAINNET operator verbs (ruling O-HC19;
# plan hc9-hc11 §2). There is no Hypercall testnet.
#
#   hypercall-live.sh status                                   # read-only
#   hypercall-live.sh simulate --symbol <SYM> [--price P --size Q]   # read-only
#   hypercall-live.sh recon                                    # read-only
#   hypercall-live.sh dust --symbol <SYM> [--price P --size Q] --confirm
#                     # ONE book_only bid (default 0.000001 @ $0.0005),
#                     # its cancel by client id, a reconcile: PASS = signed
#                     # on mainnet, cancelled, nothing filled
#   hypercall-live.sh cancel-all --confirm                     # every order of ours
#
# The two writes are refused without --confirm (exit 2). Exit 0 PASS,
# 1 FAIL.
#
# Keys come from the environment: HYPERCALL_WALLET (the owner, an
# address) and HYPERCALL_AGENT_KEY (the signer) from the REPO .env —
# or the file HYPERCALL_ENV_FILE names (a worktree has no .env of its
# own). This script sources it; the BINARY never opens it.
set -u

REPO_DIR="${0:A:h:h}"
BIN="${HYPERCALL_LIVE_BIN:-$REPO_DIR/target/release/multivenue-engine}"
ENV_FILE="${HYPERCALL_ENV_FILE:-$REPO_DIR/.env}"

if [ ! -x "$BIN" ]; then
  echo "hypercall-live: $BIN is missing — run: cargo build --release -p cli" >&2
  exit 1
fi
if ! "$BIN" hypercall-live --help >/dev/null 2>&1; then
  echo "hypercall-live: $BIN has no 'hypercall-live' arm — it predates HC9." >&2
  exit 1
fi
if [ -f "$ENV_FILE" ]; then
  set -a
  . "$ENV_FILE"
  set +a
fi

exec "$BIN" hypercall-live "$@"
