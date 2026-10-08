#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# E3 execution gate (real-execution plan §5) — the TESTNET signature
# smoke, run as a PRE-RESTART GATE and in CI.
#
# THE LAW THIS ENFORCES
# ---------------------
#   A relinked binary that cannot sign a TESTNET order must never be
#   allowed to sign a MAINNET one.
#
# So this is not a phase that ends when E3 closes. It runs before every
# armed restart, forever, because the thing it checks — that THIS
# binary's signing still agrees with the venue's — is exactly the thing
# a rebuild can silently break. LAW E-3 (msgpack key order is part of
# the signature) has no compile-time guard: reorder two struct fields
# and every order is rejected with a valid signature over a digest the
# venue never computed.
#
# TWO FORMS
#   exec-smoke.sh --offline   the offline self-test alone: reproduce the
#                             36 SDK known-answer vectors and rebuild one
#                             action per TYPE from inputs. No network, no
#                             credentials. This is the CI form — CI has no
#                             testnet key and should not have one — and it
#                             is the half that covers LAW E-3 for ORDERS,
#                             which the network probe structurally cannot
#                             (it signs a cancel).
#   exec-smoke.sh             the offline self-test, then the venue phases.
#
# WHAT IT COSTS: nothing. No balance, no registered agent, no faucet.
# The probe cancels an order id that cannot exist, so what is under
# test is the signature and not the order. See the module docs in
# crates/exec-hyperliquid/src/smoke.rs for why a "does not exist"
# rejection naming OUR OWN recovered address is stronger evidence than
# an acceptance.
#
# IT CANNOT REACH MAINNET. The binary refuses any configuration that is
# not the testnet host AND the testnet source, and it reads its own
# HYPERLIQUID_TESTNET_* variables which are disjoint from the live
# arm's. There is no flag here, or in the binary, that points it at
# production.
#
# EXIT CODES — pinned by `smoke::tests::the_exit_codes_are_pinned`.
# Change them in one place and that test fails in the other.
#   0   PASS
#   1   could not run (bad/missing config, encode, sign, unparseable)
#   20  PHASE A FAILED — the venue would not verify OUR signature
#   21  PHASE B FAILED — a CORRUPTED signature was accepted (loudest)
#   22  venue unreachable
#   23  the OFFLINE self-test failed — this binary's own encoders no
#       longer reproduce the venue SDK's bytes. No network needed to
#       diagnose it and no venue to blame: the fault is in the artifact.
#   30-39 XMM XH4g, `--hl-perp --lifecycle` (the testnet battery on slot
#       6's gateway arm; pinned by `xmm_battery::tests`): 30 the account
#       check (funds, the budget, slot-6 orders resting without
#       --sweep-first) or the tap (it missed rows, or saw rows not of
#       slot 6), 31 the boot, 32 a cycle, 33 BadAloPx, 34 a TTL, 35 a
#       sweep, 36 the reconciliations or a halt, 37 the fills, 38
#       something LEFT ON THE ACCOUNT (loudest), 39 the dead man's stall.
#   40-48 XMM XH4i-8, `--hl-outcome --lifecycle --outcome <id>` (the
#       outcome battery on slot 3's gateway arm, HIP-4 legs; pinned by
#       `outcome_battery::tests`): 40 the account or the tap, 41 the boot
#       or the roll, 42 a verb, 43 BadAloPx, 44 the IoCs, 45 the sweep, 46
#       a reconciliation or a halt, 47 the legs' spellings or the arm's
#       counters, 48 something LEFT ON THE ACCOUNT (loudest).
#       A SIGINT/SIGTERM ends the battery short (cancel-all, a flat
#       read): the wrapper waits for it rather than dying first.
# The diagnostics start at 20 because clap exits 2 on any argument
# error: a release binary too old to have the arm would otherwise have
# reported itself as a signing failure.
# Every non-zero code refuses an armed restart. They differ only so an
# operator woken at 08:33Z knows in one line whether the venue was down
# or this binary cannot sign.
set -u

REPO_DIR="${0:A:h:h}"
BIN="$REPO_DIR/target/release/multivenue-engine"

# G0 law: the release binary is what boots, so the release binary is
# what gets vetted. Vetting a debug build would vouch for a binary that
# never runs.
if [ ! -x "$BIN" ]; then
  echo "exec-smoke: $BIN is missing — run: cargo build --release -p cli" >&2
  exit 1
fi

# A binary predating E3 has no `exec-smoke` arm and would die in clap.
# Say so in the one sentence that fixes it, rather than letting an
# argument error be read as a cryptographic one.
if ! "$BIN" exec-smoke --help >/dev/null 2>&1; then
  echo "exec-smoke: $BIN has no 'exec-smoke' arm — it predates E3." >&2
  echo "exec-smoke: rebuild it: cargo build --release -p cli" >&2
  exit 1
fi

# The operator's secrets live in ~/multivenue/.env (mode 0600) and the
# engine wrapper sources them the same way. `set -a` exports every one —
# the testnet agent key AND HYPERLIQUID_TESTNET_MASTER_ADDR, which the
# battery's reads, its user streams and its gateway subscribe for (the
# binary refuses to start without it). The BINARY never opens this
# file: code that read the operator's secrets would be one refactor
# away from logging them.
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

# Did the caller ask for the offline half only? The success message
# must not claim a venue verification that never happened — a gate that
# overstates what it proved is worse than one that proves less.
offline=0
dry=0
hlperp=0
hlout=0
for a in "$@"; do
  [ "$a" = "--hl-perp" ] && hlperp=1
  [ "$a" = "--hl-outcome" ] && hlout=1
  [ "$a" = "--offline" ] && offline=1
  # A dry run reaches no venue either, and must not inherit the
  # message that says one verified us. Same rule as --offline.
  [ "$a" = "--dry-run" ] && dry=1
done

# One line of JSON on stdout for CI; the human report on stderr. A
# Ctrl-C reaches the binary too (its handler ends the battery short);
# the wrapper ignores it so it still reads the code and says it.
trap ':' INT TERM
"$BIN" exec-smoke "$@"
rc=$?
trap - INT TERM

case "$rc" in
  0)  if [ "$dry" = 1 ]; then
        echo "exec-smoke: DRY RUN — nothing was sent and NOTHING was verified; the action above is what would go on the wire" >&2
      elif [ "$offline" = 1 ]; then
        echo "exec-smoke: PASS (offline) — this binary reproduces the venue SDK's bytes for every action type; the venue was NOT contacted" >&2
      else
        echo "exec-smoke: PASS — this binary's signature is verified by the venue" >&2
      fi ;;
  20) echo "exec-smoke: *** PHASE A FAILED *** this binary MUST NOT sign a mainnet order" >&2 ;;
  21) echo "exec-smoke: *** PHASE B FAILED *** a corrupted signature was accepted — nothing this run reports is meaningful" >&2 ;;
  22) echo "exec-smoke: venue unreachable — failing CLOSED (an unproven binary is unproven whatever the reason)" >&2 ;;
  23) echo "exec-smoke: *** OFFLINE SELF-TEST FAILED *** this binary does not reproduce the venue SDK's bytes — do not ship it" >&2 ;;
  38) echo "exec-smoke: *** XH4g: THE BATTERY LEFT A POSITION OR AN ORDER ON THE TESTNET ACCOUNT *** — clear it before the next run (the arm holds a boot over a position)" >&2 ;;
  3[0-79]) echo "exec-smoke: XH4g battery FAILED (exit $rc) — the report above names the phase" >&2 ;;
  48) echo "exec-smoke: *** XH4i-8: THE OUTCOME BATTERY LEFT A LEG OR AN ORDER ON THE TESTNET ACCOUNT *** — clear it before the next run (sell a leg with --fill --sell; a leg worth under \$1 at the bid cannot be sold — the venue's minimum — and waits for its settlement; the next run refuses while one is held)" >&2 ;;
  4[0-7]) echo "exec-smoke: XH4i-8 outcome battery FAILED (exit $rc) — the report above names the phase" >&2 ;;
  *) echo "exec-smoke: could not run (exit $rc)" >&2
     # 1 (refused before anything was sent) and 2 (clap) left nothing.
     if [ "$hlperp" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the XH4g battery ended abnormally — slot-6 orders or a position may remain on the TESTNET account; the next battery's account check refuses while slot 6's orders rest: clear them first ***" >&2
     fi
     if [ "$hlout" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the XH4i-8 battery ended abnormally — slot-3 orders or a HIP-4 leg may remain on the TESTNET account; the next battery's account check refuses while either is there: clear them first ***" >&2
     fi ;;
esac
exit $rc
