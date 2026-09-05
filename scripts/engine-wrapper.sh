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
# by writing `STRATEGY=ai+icdp` to ~/multivenue/strategy.conf (KEY=VALUE,
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
case "$STRATEGY" in
  ai|ai+icdp|icdp) ;;
  *) echo "engine-wrapper: refusing STRATEGY=$STRATEGY (allowed: ai, ai+icdp, icdp)" >&2; exit 78 ;;
esac
exec ./target/release/multivenue-engine run --paper --strategy "$STRATEGY"
