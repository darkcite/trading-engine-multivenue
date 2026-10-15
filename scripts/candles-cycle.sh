#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 candles.db hourly cycle (launchd com.multivenue.candles target).
# Sources .env (never inlined in plists, never echoed) and runs one
# §9.6 gap-fill cycle via the claude_worker.candles MODULE, then the
# §9.8 IV digest (D3 — the C6+ cadence hookup, landed by the
# 2026-08-28 remediation plan; rolling window default 26 h; skips
# honestly on pre-manifest runs). Modules, not verbs — the 8-verb
# surface is frozen.
#
# Overlap guard: ANY live claude-worker invocation wins (the global
# worker-serialization law, not just a previous candles cycle); this
# cycle skips and the next hour retries — §9.6 gap-fill resumes by
# construction, and the digest's rolling window covers the gap.
set -u

REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
  echo "candles-cycle: a worker invocation is live — skipping" >&2
  exit 0
fi

if [ -f .env ]; then
  set -a
  . ./.env
  set +a
fi

cd claude-worker || exit 78
uv run python -m claude_worker.candles ||
  echo "candles-cycle: candles failed (non-fatal; next hour retries)" >&2
# W7 (2026-09-11): the VRP boot seed rides the same serialized window,
# immediately after the gap-fill so it cuts the freshest candles.
#
# It used to be cut BY HAND only. The engine's own vrp-state.tsv carries
# the rolling window across a restart, so a stale seed costs nothing
# while that file survives -- but lose it (a fresh machine, a wiped
# state dir) and the member boots WARM on whatever the seed last held,
# forecasting today's vol from however old those returns are. Nothing
# refuses that; only `last_min_ts_ms` in the boot tell reveals it.
# Hourly bounds the exposure to an hour. Atomic (tmp + rename), so a
# boot never reads a torn file.
#
# F27: guarded on the artifact. `seed-out --vrp` REQUIRES vrp.toml and
# exits non-zero without it, so on a host that does not run the member
# this line printed a failure every hour -- noise that trains an
# operator to ignore the one hour it means something.
if [ -f "$HOME/multivenue/vrp.toml" ]; then
  uv run python -m claude_worker.vrp_seed seed-out \
    --db "$HOME/multivenue/worker/candles.db" \
    --vrp "$HOME/multivenue/vrp.toml" \
    --out "$HOME/multivenue/vrp-seed.tsv" ||
    echo "candles-cycle: vrp_seed failed (non-fatal; next hour retries)" >&2
fi
# HAR H3.6 (rulings 2026-09-26): the long-tenor HAR's hourly lane, guarded
# on the artifact like vrp_seed (F27) -- no har.toml, no HAR, no noise.
# One serialized window, three steps, each non-fatal:
#  1. har_backfill: every har.toml source -- the feed AND each fallback --
#     gap-filled to the last closed minute (the ruling "keep fallbacks
#     live"; one page a source in the hourly case, one for a source that
#     stopped printing). The page budget bounds a catch-up after downtime and
#     the per-source share keeps one lagging source from starving the rest;
#     a walk they stop keeps what it stored and resumes next hour.
#  2. har_seed seed-out: every series' seed-<NAME>.tsv, atomically -- the
#     boot's history wherever the engine's own state-<NAME>.tsv is absent
#     or behind (the engine merges both at boot, core_vol::merge_rows).
#  3. har_seed compare --json-out: each feed against its fallbacks over the
#     last 14 days -> har/drift.json, the dashboard's live amber rule.
if [ -f "$HOME/multivenue/har.toml" ]; then
  uv run python -m claude_worker.har_backfill \
    --har-toml "$HOME/multivenue/har.toml" \
    --db "$HOME/multivenue/worker/candles.db" \
    --max-pages 400 --max-pages-per-source 48 ||
    echo "candles-cycle: har_backfill failed or stopped short (non-fatal; next hour resumes)" >&2
  uv run python -m claude_worker.har_seed seed-out \
    --har-toml "$HOME/multivenue/har.toml" \
    --db "$HOME/multivenue/worker/candles.db" \
    --out-dir "$HOME/multivenue/har" ||
    echo "candles-cycle: har_seed seed-out failed (non-fatal; next hour retries)" >&2
  uv run python -m claude_worker.har_seed compare \
    --har-toml "$HOME/multivenue/har.toml" \
    --db "$HOME/multivenue/worker/candles.db" \
    --days 14 --json-out "$HOME/multivenue/har/drift.json" >/dev/null ||
    echo "candles-cycle: har_seed compare failed (non-fatal; next hour retries)" >&2
fi
# D3: the IV digest rides the same serialized window.
uv run python -m claude_worker.iv_digest ||
  echo "candles-cycle: iv_digest failed (non-fatal; next hour retries)" >&2
# VM2 V6 (D-8): the depth digest rides the same serialized window
# (same rolling-window/skip laws as the IV digest).
uv run python -m claude_worker.depth_digest ||
  echo "candles-cycle: depth_digest failed (non-fatal; next hour retries)" >&2
# 2026-09-05: the WS11 funding-history lane rides here too. It used to
# ride com.multivenue.carry (deleted at the 2026-09-02 bootout) and the
# `funding` table silently froze at 2026-09-02 14:00Z — the boot
# FundingSeed frames, the regime FUND dims and the per-window
# funding-seed.tsv all read it. Idempotent (INSERT OR IGNORE), one
# newest page per instrument, best-effort.
uv run python -m claude_worker.funding ||
  echo "candles-cycle: funding failed (non-fatal; next hour retries)" >&2
# HC6 go-live (ruling O-HC2; scheduled 2026-09-26, O-HC29): the Hypercall
# research-store puller rides beside the funding lane — public REST only:
# the venue's trades by its own cursor, the options summaries of
# `[hypercall] underlyings`, and the settlement payouts of the wallets in
# CLAUDE_WORKER_HC_WALLETS (.env; the lane skips while it is unset — the
# wallets stay out of git by the research law). Guarded on the section,
# like the lanes above on their artifacts: no Hypercall, no noise.
if grep -q '^\[hypercall\]' "$HOME/multivenue/universe.toml" 2>/dev/null; then
  uv run python -m claude_worker.hypercall_history \
    --universe "$HOME/multivenue/universe.toml" \
    --db "$HOME/multivenue/worker/candles.db" ||
    echo "candles-cycle: hypercall_history failed (non-fatal; next hour retries)" >&2
fi
