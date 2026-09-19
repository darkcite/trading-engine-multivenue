#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 always-on engine wrapper (docs/m3-progress.md; mvp-plan §4-M3).
# launchd ProgramArguments target — every engine boot flows through
# here (RunAtLoad, KeepAlive relaunches, post-daily-restart).
#
# Laws:
# * .env is SOURCED (values never inlined in the plist, never echoed).
# * Universe refresh is BEST-EFFORT (claude_worker.universe_refresh,
#   a module, not a verb): failure boots on the existing file.
# * ONE ENGINE EVER: another live instance (e.g. an M2 smoke window)
#   makes this boot back off; KeepAlive retries until the lane is
#   free, so the standing engine resumes by itself.
# * G0 relink law: this script NEVER builds. It runs the release
#   binary as currently linked — `cargo build --release -p cli` is a
#   deliberate operator act; the next (re)start picks it up.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78

# launchd agents start with a minimal PATH — uv lives outside it.
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f "multivenue-engine run" >/dev/null 2>&1; then
  echo "engine-wrapper: another multivenue-engine is live — backing off" >&2
  sleep 30
  exit 1
fi

# Source .env for the worker refresh (engine loads it itself via
# dotenvy from the working directory). Values are never printed.
if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

( cd claude-worker && uv run python -m claude_worker.universe_refresh ) ||
  echo "engine-wrapper: universe refresh failed — booting with existing universe.toml" >&2

# RG2 (docs/regime-and-dashboard-plan.md §4.3): the regime detector's
# warm-up seed — the last ~25 h of 1-minute closes of the artifact's
# reference + breadth members, exported from candles.db right before
# the boot so the `slow` profile is judged from the first minute
# (three restarts a day would otherwise leave it blind for hours).
# Best-effort: no artifact ⇒ nothing to export; a failed export leaves
# a stale or absent seed and the engine warms live (boot tell
# `regime: seed absent`). The seed is DERIVED data (candles), never a
# capture window — the ≤ 2 h capture-window law is untouched.
# RG7 (plan §7.1, the seed hole): `--refresh-tail` gap-fills the 1 m
# candles of the artifact's OWN descriptors first (≤ 8 instruments, one
# or two REST pages each) so the seed reaches the boot minute — without
# it the hourly candles lane's lag left the fast profile UNKNOWN for
# up to an hour after every restart.
if [ -f "$HOME/multivenue/regime.toml" ]; then
  ( cd claude-worker && uv run python -m claude_worker.regime seed-out \
      --regime "$HOME/multivenue/regime.toml" \
      --out "$HOME/multivenue/regime-seed.tsv" \
      --refresh-tail --universe "$HOME/multivenue/universe.toml" ) ||
    echo "engine-wrapper: regime seed export failed — the detector warms live" >&2
fi

# XSD-4 (statarb doc 08 §3.7): the slot-2 xsd member's warm-up seed —
# the trailing 800 hourly closes of every descriptor its table names,
# exported from candles.db right before the boot so the 720 h z windows
# are warm at the first roll (without it the member is blind for 30
# days after every restart — three restarts a day would never let it
# trade). Best-effort like the regime seed: no table ⇒ nothing to
# export; a failed export leaves a stale or absent seed and the member
# warms live (boot tell `xsd: … seed_rows=0`). Rows at or after the
# boot hour are refused by the engine; the lane never writes them.
# DERIVED data (candles), never a capture window — the ≤ 2 h law holds.
# The TABLE itself is rotated by daily-restart.sh at the 00:10Z slot
# (monthly, by the file's age), never here.
if [ -f "$HOME/multivenue/xsd-table.tsv" ]; then
  ( cd claude-worker && uv run python -m claude_worker.xsd_author seed-out \
      --table "$HOME/multivenue/xsd-table.tsv" \
      --out "$HOME/multivenue/xsd-seed.tsv" ) ||
    echo "engine-wrapper: xsd seed export failed — the member warms live" >&2
fi

# BIN15 O5 (spec §7.1): the slot-3 member's warm-up seeds — 1 441
# stamped minute returns to fill the HAR window and 128 fitted pairs per
# tenor, cut from candles.db right before the boot. Without them the
# forecast needs 24 h of live minutes and 60 quarter-hours of settled
# pairs, and the restart lane fires three times a UTC day, so the member
# would never price at all. Best-effort like the two above: no
# `bin15.toml` ⇒ nothing to export; a failed export leaves a stale or
# absent seed and the member holds until its own window warms (boot tell
# `bin15: … seeds=0 daily_seeds=0`).
#
# TWO files per coin, and the second is not optional decoration: a pair
# belongs to ONE tenor, so the 15 m families read
# `bin15-seed-<COIN>.tsv` and the native-daily ones read
# `bin15-seed-<COIN>-1d.tsv`. `seed-all` writes exactly the set the
# artifact's `underlying` list implies, under the names boot looks up.
# DERIVED data (candles), never a capture window — the ≤ 2 h law holds.
if [ -f "$HOME/multivenue/bin15.toml" ]; then
  ( cd claude-worker && uv run python -m claude_worker.bin15_seed seed-all \
      --db "$HOME/multivenue/worker/candles.db" \
      --bin15 "$HOME/multivenue/bin15.toml" \
      --dir "$HOME/multivenue" ) ||
    echo "engine-wrapper: bin15 seed export failed — the member warms live" >&2
fi

# M5-prep #7b (operator ruling 7(b); remediation plan 2026-08-28): a
# committed ruleset's table is IN-MEMORY — every boot must re-stage +
# re-commit the registry's active ruleset or nothing AI-authored
# trades after a restart (live finding: vm_fires 0 after every
# midnight turn). Fired on EVERY boot (KeepAlive relaunches
# included); the script waits for ai.sock, serializes behind the
# worker law, and no-ops harmlessly when the registry has no
# committed row. The backgrounded child survives the exec below
# (it is reparented, not killed). Interpreter-invoked (zsh <path>)
# so it works regardless of the file's exec bit — the 2026-08-27
# exec-bit strip is exactly how the restart lane died.
( zsh "${0:A:h}/recommit-ruleset.sh" >> "$HOME/multivenue/logs/launchd/recommit.log" 2>&1 & )

# VM2 V8 (operator-authorized 2026-08-30): post-boot seed push —
# funding prints re-warm the VM's feature windows and PositionSeed
# restores open rows (MULTIVENUE_SEED_RULESET in .env names the
# committed artifact; unset ⇒ funding-only). The 45 s grace lets the
# #7b recommit above land FIRST (a seed against an inert VM is
# refused by design). Same reparented-background + interpreter-
# invoked laws as the recommit line.
( sleep 45 && zsh "${0:A:h}/seed-push.sh" >> "$HOME/multivenue/logs/launchd/seed-push.log" 2>&1 & )

# Operator ruling 2026-09-02: AI-pushed lanes only (ai-exec + vm,
# mask 48) — Rust-coded strategies disabled at boot.
#
# ICDP I4 (2026-09-03): the operator opts the slot-6 intrabar member in
# by writing `STRATEGY=ai+icdp` or `STRATEGY=ai+vrp` to
# ~/multivenue/strategy.conf (KEY=VALUE,
# sourced; absent ⇒ `ai`). Paper only — `icdp` never boots without
# `--paper` (belt and braces: no live dispatcher exists, and the plan's
# I4 law says the wrapper refuses it regardless). The artifact
# (~/multivenue/icdp.toml) must resolve, or the engine refuses the boot
# and KeepAlive relaunches — check the launchd log, then fix the file
# or drop the mask back to `ai`.
STRATEGY="ai"
if [ -f "$HOME/multivenue/strategy.conf" ]; then
  . "$HOME/multivenue/strategy.conf"
fi
# XSD-3 (2026-09-12): the slot-2 cross-sectional member joins the
# allow-list (`ai+xsd`, `ai+vrp+xsd`, `xsd`); it boots only when
# ~/multivenue/xsd.toml + xsd-table.tsv resolve (absent ⇒ the bit stays
# unset, the rest of the mask boots). Paper only, like every member.
#
# BIN15 O4b (2026-09-12): the slot-3 rolling-binary member joins with
# five names (`bin15`, `ai+bin15`, `ai+vrp+bin15`, `ai+xsd+bin15`,
# `ai+vrp+xsd+bin15`). UNLIKE xsd, an absent ~/multivenue/bin15.toml
# with the bit REQUESTED refuses the boot rather than clearing the bit
# (the icdp/F19 law) — booting `ai+bin15` silently as `ai` is how an
# operator comes to watch a member that was never there. `rule-tree` is
# GONE from the set: slot 3 is bin15, and the name was never in this
# allow-list.
case "$STRATEGY" in
  ai|ai+icdp|icdp|ai+vrp|vrp|ai+xsd|ai+vrp+xsd|xsd) ;;
  bin15|ai+bin15|ai+vrp+bin15|ai+xsd+bin15|ai+vrp+xsd+bin15) ;;
  *) echo "engine-wrapper: refusing STRATEGY=$STRATEGY (allowed: ai, ai+icdp, icdp, ai+vrp, vrp, ai+xsd, ai+vrp+xsd, xsd, bin15, ai+bin15, ai+vrp+bin15, ai+xsd+bin15, ai+vrp+xsd+bin15)" >&2; exit 78 ;;
esac

# XSD-1 (2026-09-12, measured live): a launchd agent inherits macOS's
# 256-descriptor SOFT limit, and the Binance lane is ONE socket per
# instrument per channel — the 110-perp research universe (124 usdm ×
# {bookTicker, markPrice} + spot + eapi ≈ 254 sockets, plus ~50 capture
# files, UDS, metrics) overran it at boot: `connect failed … Too many
# open files` on every slot past ~200, `ingress-ai: run loop error;
# rebinding` (the command plane could not bind), `vrp: state write
# failed`. Raise the soft limit here, before the exec, so the engine is
# born with headroom: 8192 is >30× today's need and far under
# `kern.maxfilesperproc`. Soft only (`-S`) — never lowers the hard
# limit. A refused raise is a tell, not a refusal to boot.
ulimit -S -n 8192 ||
  echo "engine-wrapper: ulimit -n 8192 refused (hard=$(ulimit -Hn)) — booting with $(ulimit -Sn)" >&2
echo "engine-wrapper: fd soft limit $(ulimit -Sn) (hard $(ulimit -Hn))" >&2
# E7 R0 (2026-09-19): the THIRD edit `exec.toml.example` names. A slot is
# armed only when ~/multivenue/strategy.conf carries BOTH
# `EXEC_TOML=$HOME/multivenue/exec.toml` and `ARM_LIVE=3` (the slots the
# artifact marks live, comma-separated). Both absent ⇒ the run line
# below is byte-identical to the paper fleet's. One of the two set alone
# is a misconfiguration and REFUSES the boot (exit 78, like a bad
# STRATEGY) rather than arming or silently ignoring it — the engine's
# own interlock (artifact ⇄ --arm-live must agree exactly) still has the
# last word once both reach it. `--paper` stays: it is the default and
# only the per-slot route table can move a slot off it.
EXEC_ARGS=()
if [ -n "${EXEC_TOML:-}" ] && [ -n "${ARM_LIVE:-}" ]; then
  if [ ! -f "$EXEC_TOML" ]; then
    echo "engine-wrapper: refusing to arm — EXEC_TOML=$EXEC_TOML is not a file" >&2
    exit 78
  fi
  EXEC_ARGS=(--exec "$EXEC_TOML" --arm-live "$ARM_LIVE")
  echo "engine-wrapper: ARMING slot(s) $ARM_LIVE via $EXEC_TOML — real orders will be submitted" >&2
elif [ -n "${EXEC_TOML:-}${ARM_LIVE:-}" ]; then
  echo "engine-wrapper: refusing — EXEC_TOML and ARM_LIVE must BOTH be set to arm (got EXEC_TOML='${EXEC_TOML:-}' ARM_LIVE='${ARM_LIVE:-}')" >&2
  exit 78
fi
exec ./target/release/multivenue-engine run --paper --strategy "$STRATEGY" "${EXEC_ARGS[@]}"
