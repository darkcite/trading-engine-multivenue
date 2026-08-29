# M5 semi-manual runbook notes (WS11)

Small operator snippets for the M5 research loop that the daemon
deliberately does NOT automate. Additive to `docs/local-setup.md`;
the pinned `docs/prompts/ai-session.md` is untouched by law
(`test_session_scripted.py`).

## 1. Positions digest WITH the map's HIP-4 pairs (ruling #7a, M5 lane)

The daemon's digest calls `gather_positions_payload` with NO pairs
by design (the map file's pairs are a cli concern —
capture-remediation plan §12 delta 5). The semi-manual M5 lane wants
the paired view. One serialized invocation (worker law:
`pgrep -f claude-worker` first, source `.env` into the shell for the
worker env seam):

```sh
cd ~/trading-engine-multivenue/claude-worker
uv run python - <<'PY'
import json
import pathlib

import claude_worker.cli
import claude_worker.strategist

map_path = pathlib.Path("~/multivenue/worker/market-map.json").expanduser()
replay = pathlib.Path("~/multivenue/logs").expanduser()

m = claude_worker.cli.load_market_map(map_path)
payload = claude_worker.strategist.gather_positions_payload(
    replay, hip4_pairs=list(m.hip4_pairs)
)
print(claude_worker.strategist.positions_digest_text(payload))
PY
```

Notes:

- `m.hip4_pairs` is the strict-loaded map's pair list; an empty list
  renders the same digest the daemon produces (per-sym netting +
  totals) — the pairs only ADD the HIP-4 paired-view rows.
- Read-only end to end (fills + ticks of the latest run dir); safe
  beside a running engine, but still serialize like every worker
  invocation (one SQLite/seq namespace).

## 2. Offline data lanes available to M5 (all MODULES, never verbs)

Run serialized, any order, idempotent:

```sh
uv run python -m claude_worker.candles     # §9 candle store cycle
uv run python -m claude_worker.iv_digest   # §9.8 IV snapshots
uv run python -m claude_worker.refdata     # WS4/7/8/9: 24h vol + OI snapshots
uv run python -m claude_worker.funding     # WS11: funding-rate history
uv run python -m claude_worker.pnl_report  # M4.3 shadow-P&L (D2 manual lane)
```

All five write beside each other in `~/multivenue/worker/candles.db`
(tables: `candles`, `candle_conflicts`, `iv_digest`, `refdata`,
`funding`) keyed `venue + descriptor` (§9.4 — never bare SymbolIds).

## 3. D4 ruling record (WS11)

Ruling D4 closes with reading **(i)** — zero code: candles.db's
non-options descriptors already cover the full non-options universe,
and options ride `iv_digest`. Reading (ii) (REST OHLCV for ~192
option instruments) stays un-built unless the operator overrules.
