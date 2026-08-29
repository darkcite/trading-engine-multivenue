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

## 2026-08-29 — M5 SESSION 1: external-strategies onboarding (operator go: "start onboarding in semi-manual mode")

Authority: the external-strategies onboarding plan (OPERATOR-LOCAL,
deliberately uncommitted by ruling 2026-08-29 — lives beside the
strategy docs on this machine; executed as its §4 checklist).
Operator rulings this session: assessment→go, S1 pilot = FIXED ~8
names, ai-session.md validated first.

**Done, in order (all six venues Steady throughout, sub_drops 0):**

1. `ai-session.md` VALIDATED against the live verb surface (push
   kinds/args match; §4 flow sound). Two findings: the `pnl` verb
   (M4/D1) is absent from the cookbook (doc is test-pinned — left
   untouched, cosmetic); `frames.VENUE_BYBIT=6` exists but the
   `push` verb's `_VENUES` table does not expose "bybit" (cli.py
   FROZEN) ⇒ S1's Bybit legs are not intent-addressable — v1 runs
   S1 as signal+digest only. A one-line D1-style unfreeze (add
   "bybit" to `_VENUES` + engine-side venue-byte accept check) is
   the candidate ruling if S1 paper legs are wanted.
2. UNIVERSE WIDENED (backup `universe.toml.pre-m5onboard`; append,
   never reorder): +13 BN usdm (CVFC 6 + S1 pilot 7), +7 Deribit
   `*_USDC-PERPETUAL`, +5 HL coins, +13 Bybit linear. OKX swaps
   DELIBERATELY not added: the single batched okx subscribe sits at
   4 064 of the venue's 4 096-byte request cap. Revive-lever
   restart → discovery 9/9 deribit · hl 7 · bn 16/16 · bybit 15;
   fetch seeded 34 map entries, unresolved=0 in the mapped universe
   (the 128 = the per-boot options class, by law).
3. **D3 manifest bug FOUND+FIXED** (`options_manifest.rs`):
   `render_instruments` was missing the WS5 `bn_dated`, WS6
   `deribit_combos`, WS9 `bybit_spot`/`bybit_linear` blocks — the
   "EVERY instrument" law silently violated since WS9. Fixed in
   allocation order + test pins all four; relink + re-restart;
   manifest now 245 rows incl. 15 bybit descriptors.
4. **WS11 HL funding-lane bug FOUND+FIXED** (`funding.py`): HL
   `fundingHistory` pages ASCENDING from startTime, so the fixed
   now−33d start returned the OLDEST ~500-row page every cycle —
   the series stalled ~12 days behind and could never converge.
   Fix: resume from the newest stored point +1 (test pinned).
   Post-fix: HL newest 0.4 h old; funding table 19 160+2 044 points
   / 44 descriptors, cycles idempotent.
5. **`claude_worker.carry_signal` LANDED** (module, never a verb):
   CVFC-1 §2 spec (24h-mean APR spreads over ordered venue pairs,
   entry ≥20 pts, exit <0 after ≥96 h, ≤5 positions, majors
   excluded, one-cycle re-entry cooldown) + S1 pilot signal
   (50%/30% confirms) from the funding table ONLY (no network; the
   WS11 lane owns fetching; cadence law incl. deribit
   interest_8h ÷ 8 — R4 §9 unit test). Emits batch JSON + push.sh
   (exact verb lines) + digest + rolling state under
   `~/multivenue/worker/carry/`. 9 new pytest (worker suite 483;
   frozen 202 untouched).
6. **`cvfc-basis-kill.json` AUTHORED** (the T1 kill-tripwire as VM
   rows: 10 × `cross_deviation`, HL leg sym vs Deribit-leg ref,
   150 bps, $8 caps — the honest VM content; a standing S7 ruleset
   without its evi gate would be their REJECTED naive-evi row).
   Frozen backtest on the widened run dir: **exit 3 GATE REFUSED —
   bounds+DD PASS (shape valid), min_trades/min_days FAIL** — the
   new syms have ~1 h of capture. By design; re-run + stage+commit
   at Day 2 (see next steps). NOTE: a whole-root backtest was
   killed mid-run (the known ~27 GB > 24 GiB merge) — ALWAYS pass
   `--replay-dir <run-dir>` on this Mac.
7. **First signal cycle on real data** — the machinery agrees with
   their research AND dates it: HL premium now +7.7..+10.9% (their
   melt-up 15–25% era cooled); Deribit alt discount HALF-CLOSED in
   6 days (ADA −12.0% vs doc −17.3%, DOGE −1.6% vs −10.2%; their
   "convergence is a when, not an if" measured independently); best
   pair today ≈ +12 pts < the 20-pt entry ⇒ **0 entries, correctly**.
   COTI's S1 anomaly PERSISTS (−381.8%/24h, −105.9%/3d — their
   scanner had −313% on Aug-5; 24 days of persistence).
8. **AI-path seam PROVEN live**: `positions` (empty book) → ONE
   demonstration order-intent (HL BTC bid 78 000 × 0.0001 ≈ $7.8,
   TTL 600 s) → `sent seq=34` (cross-boot seq continuity) →
   `ai_cmds_total 2`, zero hmac/malformed/gap/drops/expired →
   ai-cmds.pmlr 2 slots → **engine-orders.pmlr: 1 order stamped
   strategy_id 4 (ai-exec)** beside 4 503 s0 orders.

**Standing cadence from here (Day 2+):** hourly (or per-session)
`funding` cycle → `carry_signal` → review digest → run `push.sh`
when intents appear; after ≥2 days of new-sym capture: re-run the
tripwire backtest → stage-ruleset → commit-ruleset (H6b §4 flow);
daily `audit-pnl --dir <run>` / `pnl` verb — s4-tagged orders =
this lane. Watch: CVFC spread re-widening past 20 (the compression
may be temporary), COTI persistence, bybit-venue unfreeze ruling.

**Addendum (same session): the bybit unfreeze + the first real entry.**
Operator APPROVED the D1-pattern unfreeze: "bybit" joined the push
verb's `_VENUES` (cli.py; ruling cited in-code and in the new
`test_push_order_intent_accepts_bybit_venue`). Engine-side needed
NOTHING — the AiCmd shape law already admits any VenueId except Ai,
and VenueId 6 = Bybit since WS9. carry_signal then gained full S1
execution (long the more-negative venue / short the other; exit at
directional <10% or age >10 d; ≤4 positions; one test reworked).
Worker suite **485 passed** (frozen 202 untouched; test_cli 65).
NEXT CYCLE ENTERED FOR REAL: **COTI short=bybit long=bn at
spread24 −381.8% / 3d −105.9%** — push.sh reviewed + executed
(seq 36/38), ai_cmds_total 6, zero rejects, **engine-orders.pmlr
now carries 3 strategy_id-4 orders** (the demo + both COTI legs).
The external-strategies lane is live in paper end-to-end; audit-pnl
picks the s4 tag up from the next daily report.

**Addendum 2 (same session): the cadence is UNATTENDED.** Operator
"go" → `scripts/carry-cycle.sh` + `com.multivenue.carry` launchd
agent (hourly at :02, aligned to the funding hour; candles-agent
pattern: pgrep worker-serialization guard, .env sourced, modules +
the push verb, engine-up check before pushing, everything logged to
`launchd/carry.log`). Kickstart proof run 12:45Z: funding idempotent
(+0), signal cycled, 0 new intents (COTI held; CVFC under bar), push
correctly skipped. The strategies now run engine + cron only — no
human, no LLM in the loop; the fleet gains its sixth agent.
