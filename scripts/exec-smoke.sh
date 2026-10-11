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
# IT CANNOT REACH MAINNET — but for three tools, by the operator's hand.
# The binary refuses any Hyperliquid configuration that is not the
# testnet host AND the testnet source, and it reads its own
# HYPERLIQUID_TESTNET_* variables which are disjoint from the live
# arm's; the Binance futures, spot and options battery and probe dial
# Demo hosts only. The exceptions: BX11's account tools
# (`--binance-setup`, `--binance-flatten`) under `--bn-network mainnet`
# READ the live Binance account with the engine's own key and CHANGE it
# only with `--confirm`; and BX10's Binance Stocks tools — Stocks has no
# Demo — `--binance-probe equity` READS the live account (places
# nothing) and `--binance equity --lifecycle` TRADES REAL MONEY only
# with `--bn-confirm-mainnet`; and BX13's R0 ramp, `--binance … --lifecycle
# --bn-mainnet`, READS the live account and TRADES REAL MONEY only with
# `--bn-confirm-mainnet`. The word is the operator's, never a session's.
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
#       A SIGINT/SIGTERM ends the battery short (cancel-all, a flat
#       read): the wrapper waits for it rather than dying first.
#   50-59 BX7b, the Binance DEMO tools (pinned by `bn_demo::tests`). The
#       battery (`--binance … --lifecycle`): 50 the account check, 51 the
#       boot, 52 a maker cycle, 53 the post-only refusal, 54 the TTL, 55
#       the sweep, 56 a reconciliation or a halt, 57 the fills — each also
#       a clean run short of the plan's bar in that phase. Both: 58
#       something LEFT ON THE DEMO ACCOUNT, or an end that could not read
#       it (loudest). The probe
#       (`--binance-probe`): 59 a probe failed (an arming-precondition
#       probe refused, or a probe that could not run). They read only the
#       BINANCE_DEMO_* variables and dial Demo hosts only — but the
#       Binance Stocks battery and probe (BX10, `--binance equity`,
#       `--binance-probe equity`): MAINNET, the live key; their 50-59 are
#       read for the LIVE account (docs/binance-runbook.md §12), 64 is
#       the battery without --bn-confirm-mainnet and 66 its disclaimer
#       unsigned (486410; nothing placed). BX13, the R0 ramp
#       (`--bn-mainnet`): MAINNET, the live key; its 50-58, 65 and 66
#       are read for the LIVE account (docs/binance-runbook.md §13), and
#       64 is the run without --bn-confirm-mainnet (read, the plan
#       printed, nothing sent).
#   60-64 BX11, the Binance ACCOUNT TOOLS (pinned by `bn_ops::tests`):
#       `--binance-setup <um|cm>` and `--binance-flatten <um|cm>`. 60 a
#       read failed (a setting, the account, the orders or the book), 61
#       the venue refused a setup change (or a setting read back otherwise)
#       or a flatten's cancel (or one was not sent; a refused close is the
#       final read's 63), 62 the flatten found an order of the arm's resting
#       and was not given --bn-cancel-first (nothing sent: is the engine
#       running?), 63 a position STILL OPEN after the flatten's tries
#       (loudest), 64 mainnet without --confirm (read, every change
#       printed, nothing sent). Demo unless --bn-network mainnet.
#   65  BX11, the battery's gap drill (`--bn-drill gap`): once the
#       gateway was stopped, the venue's dead-man was not seen to cancel
#       the makers (none in time, too soon, or a maker filled); the run
#       cancelled any left by id. A drill that fails before the gateway
#       stops is 52. The flatten drill (`--bn-drill flatten`) ends with the
#       flatten's own code (60-63); before the gateway stops, 57, 56 or 52.
#       Once a drill stopped the gateway (and no signal came) the end runs
#       the flatten on the run's network (Demo; with --bn-mainnet the R0
#       gap drill's on the live account, under the word), then reads the
#       account (58 if not flat).
#   66  BX8, the spot battery (`--binance spot`): the gateway's permission
#       check refused the boot — the account lacks a group the spot row
#       needs (the bStock check, `--bn-spot-symbol`); NOTHING was placed.
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
binance=0
bnops=0
drill=0
mainnet=0
bnmainnet=0
eqprobe=0
eqbattery=0
prev=""
for a in "$@"; do
  [ "$a" = "--hl-perp" ] && hlperp=1
  # The account tools first: their names also start `--binance`.
  case "$a" in
    --binance-setup*|--binance-flatten*) bnops=1 ;;
    --binance-probe=equity) binance=1; eqprobe=1 ;;
    --binance=equity) binance=1; eqbattery=1 ;;
    --binance*) binance=1 ;;
    --bn-drill*) drill=1 ;;
    --bn-network=mainnet) mainnet=1 ;;
    --bn-mainnet) bnmainnet=1 ;;
  esac
  [ "$prev" = "--bn-network" ] && [ "$a" = "mainnet" ] && mainnet=1
  [ "$prev" = "--binance-probe" ] && [ "$a" = "equity" ] && eqprobe=1
  [ "$prev" = "--binance" ] && [ "$a" = "equity" ] && eqbattery=1
  [ "$a" = "--offline" ] && offline=1
  # A dry run reaches no venue either, and must not inherit the
  # message that says one verified us. Same rule as --offline.
  [ "$a" = "--dry-run" ] && dry=1
  prev="$a"
done
account="DEMO"
if [ "$bnops" = 1 ] && [ "$mainnet" = 1 ]; then
  account="LIVE (mainnet)"
  echo "exec-smoke: *** --bn-network mainnet: the account tool READS THE LIVE BINANCE ACCOUNT with the engine's own key; it changes it only with --confirm ***" >&2
fi
if [ "$eqprobe" = 1 ]; then
  echo "exec-smoke: *** --binance-probe equity: READS THE LIVE Binance Stocks account on MAINNET with the engine's own key (Stocks has no Demo); it places nothing — with --bn-stream-confirm it also opens a listenKey and watches the order stream ***" >&2
fi
if [ "$bnmainnet" = 1 ]; then
  account="LIVE (mainnet)"
  echo "exec-smoke: *** --bn-mainnet: THE R0 RAMP BATTERY TRADES ON THE LIVE BINANCE ACCOUNT — REAL MONEY, the engine's own key. With --bn-confirm-mainnet it runs R0's cycles and round trips at each row's minimum size (fees and the spread on every trade); without it, it reads the account and prints its plan only. A dedicated account, nothing resting; one Ctrl-C only ***" >&2
fi
if [ "$eqbattery" = 1 ]; then
  account="LIVE Binance Stocks (mainnet)"
  echo "exec-smoke: *** --binance equity: THE BINANCE STOCKS BATTERY TRADES ON MAINNET — REAL MONEY (Stocks has no Demo). With --bn-confirm-mainnet it places a resting BUY and cancels it, buys at the minimum size and sells it back (two fees at the \$0.35 floor, the spread twice); without it, it reads and prints its plan only. A dedicated account; one Ctrl-C only ***" >&2
fi

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
      elif [ "$bnops" = 1 ]; then
        echo "exec-smoke: PASS (Binance account tool, $account account) — the report above names every read, change and answer" >&2
      elif [ "$eqprobe" = 1 ]; then
        echo "exec-smoke: PASS (Binance Stocks read-only probe, MAINNET) — the report above names every answer; nothing was placed" >&2
      elif [ "$eqbattery" = 1 ]; then
        echo "exec-smoke: PASS (Binance Stocks battery, MAINNET) — the bar is met; the report above names every order and answer; nothing rests on the LIVE account and its executions match what the run booked" >&2
      elif [ "$bnmainnet" = 1 ] && [ "$drill" = 1 ]; then
        echo "exec-smoke: PASS (Binance drill, MAINNET) — the drill's bar held (its \`r0\` line is the R0 record); nothing was left on the LIVE account" >&2
      elif [ "$bnmainnet" = 1 ]; then
        echo "exec-smoke: PASS (R0 ramp, MAINNET) — R0's bar is met: the \`r0\` lines above are the R0 record; nothing was left on the LIVE account" >&2
      elif [ "$drill" = 1 ]; then
        echo "exec-smoke: PASS (Binance Demo drill) — the drill's bar held (the report names it); nothing was left on the Demo account" >&2
      elif [ "$binance" = 1 ]; then
        echo "exec-smoke: PASS (Binance Demo) — the report above names every answer; nothing was left on the Demo account" >&2
      else
        echo "exec-smoke: PASS — this binary's signature is verified by the venue" >&2
      fi ;;
  20) echo "exec-smoke: *** PHASE A FAILED *** this binary MUST NOT sign a mainnet order" >&2 ;;
  21) echo "exec-smoke: *** PHASE B FAILED *** a corrupted signature was accepted — nothing this run reports is meaningful" >&2 ;;
  22) echo "exec-smoke: venue unreachable — failing CLOSED (an unproven binary is unproven whatever the reason)" >&2 ;;
  23) echo "exec-smoke: *** OFFLINE SELF-TEST FAILED *** this binary does not reproduce the venue SDK's bytes — do not ship it" >&2 ;;
  38) echo "exec-smoke: *** XH4g: THE BATTERY LEFT A POSITION OR AN ORDER ON THE TESTNET ACCOUNT *** — clear it before the next run (the arm holds a boot over a position)" >&2 ;;
  3[0-79]) echo "exec-smoke: XH4g battery FAILED (exit $rc) — the report above names the phase" >&2 ;;
  58) if [ "$bnmainnet" = 1 ]; then
        echo "exec-smoke: *** BX13: SOMETHING WAS LEFT ON THE LIVE (MAINNET) BINANCE ACCOUNT, OR ITS END COULD NOT READ IT *** — REAL MONEY: the report's \`left\` names it; cancel the order and close the position now (--binance-flatten <um|cm> --bn-network mainnet --confirm --bn-cancel-first, or the Binance app)" >&2
      elif [ "$eqbattery" = 1 ]; then
        echo "exec-smoke: *** BX10: SOMETHING WAS LEFT ON THE LIVE (MAINNET) BINANCE STOCKS ACCOUNT, OR ITS END COULD NOT READ IT *** — REAL MONEY: the report's \`left\` names it; an order left rests WITHOUT its TTL until its session closes — cancel it, and sell a holding left, in the Binance app now" >&2
      else
        echo "exec-smoke: *** BX7b: SOMETHING WAS LEFT ON THE BINANCE DEMO ACCOUNT, OR ITS END COULD NOT READ IT *** — the report names it; read the account by hand and clear it before the next run" >&2
      fi ;;
  59) if [ "$eqprobe" = 1 ]; then
        echo "exec-smoke: *** BX10: THE BINANCE STOCKS READ-ONLY PROBE FAILED *** — the report above names it; nothing was placed" >&2
      else
        echo "exec-smoke: *** BX7b: A BINANCE DEMO PROBE FAILED *** — the report above names it" >&2
      fi ;;
  5[0-7]) if [ "$bnmainnet" = 1 ]; then
        echo "exec-smoke: BX13 R0 ramp (MAINNET) FAILED or SHORT (exit $rc) — the report above names the phase and the \`r0\` lines each bar. A refusal before the boot placed nothing; past it, the end closed, swept and read the LIVE account: \`left\` empty means flat with nothing resting" >&2
      elif [ "$eqbattery" = 1 ]; then
        echo "exec-smoke: BX10 Binance Stocks battery (MAINNET) FAILED (exit $rc) — the report above names the phase. A refusal before the first order (the account check, the boot) placed nothing; past it, the end swept and read the LIVE account: \`left\` empty means no order rests and the ticker's executions net to what the run booked" >&2
      else
        echo "exec-smoke: BX7b Binance Demo battery FAILED (exit $rc) — the report above names the phase" >&2
      fi ;;
  60) echo "exec-smoke: *** BX11: A BINANCE READ FAILED *** — the report names it and every change or order sent before it: read the $account account by hand" >&2 ;;
  61) echo "exec-smoke: *** BX11: THE VENUE REFUSED A SETUP CHANGE OR A FLATTEN'S CANCEL *** — the report names its answer; read the $account account before trying again" >&2 ;;
  62) echo "exec-smoke: BX11 flatten: an order of the arm's rests — is the engine running? NOTHING was sent; stop it first, or pass --bn-cancel-first" >&2 ;;
  63) echo "exec-smoke: *** BX11 FLATTEN: A POSITION IS STILL OPEN AFTER ITS TRIES *** — the report names it; close it by hand on the $account account" >&2 ;;
  64) if [ "$bnmainnet" = 1 ]; then
        echo "exec-smoke: BX13: no --bn-confirm-mainnet — the LIVE account was read and the R0 battery's plan is printed above; NOTHING was sent" >&2
      elif [ "$eqbattery" = 1 ]; then
        echo "exec-smoke: BX10: no --bn-confirm-mainnet — the Binance Stocks account was read and the battery's plan is printed above; NOTHING was sent" >&2
      else
        echo "exec-smoke: BX11: mainnet without --confirm — the account was read and every change it would send is printed above; NOTHING was sent" >&2
      fi ;;
  65) echo "exec-smoke: *** BX11 DRILL: THE VENUE'S DEAD-MAN WAS NOT SEEN TO CANCEL THE MAKERS *** — the report names why; the run cancelled any left by id and read the $account account" >&2 ;;
  66) if [ "$eqbattery" = 1 ]; then
        echo "exec-smoke: BX10: the venue refused the first place — 486410, the US equity disclaimer is unsigned on this account: sign it in the Binance app (the tool never does); NOTHING was placed" >&2
      else
        echo "exec-smoke: BX8: the permission check refused the boot — the $account account lacks a group the spot row needs (the bStock check); NOTHING was placed" >&2
      fi ;;
  *) echo "exec-smoke: could not run (exit $rc)" >&2
     # 1 (refused before anything was sent) and 2 (clap) left nothing.
     if [ "$hlperp" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the XH4g battery ended abnormally — slot-6 orders or a position may remain on the TESTNET account; the next battery's account check refuses while slot 6's orders rest: clear them first ***" >&2
     fi
     if [ "$eqbattery" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the Binance Stocks battery ended abnormally — an order may rest on the LIVE (MAINNET) account WITHOUT its TTL until its session closes, or a holding remain: cancel and sell it in the Binance app now ***" >&2
     elif [ "$bnmainnet" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the R0 ramp battery ended abnormally — orders or a position may remain on the LIVE (MAINNET) account: REAL MONEY — read it and clear it now (--binance-flatten <um|cm> --bn-network mainnet --confirm --bn-cancel-first, or the Binance app) ***" >&2
     elif [ "$binance" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the Binance Demo tool ended abnormally — orders or a position may remain on the DEMO account: read it before the next run ***" >&2
     fi
     if [ "$bnops" = 1 ] && [ "$rc" != 1 ] && [ "$rc" != 2 ]; then
       echo "exec-smoke: *** the Binance account tool ended abnormally — a change or an order may have reached the $account account: read it before anything else ***" >&2
     fi ;;
esac
exit $rc
