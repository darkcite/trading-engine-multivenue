# Research universe — what the loop agent can analyze (2026-08-30)

The single orientation file for a research session (the ai-session §4
semi-manual loop, and `serve` when Stage 3 keys it). It answers: which
venues exist, what instruments and channels each carries, where the
data lives, and which carrier a strategy idea must target. Per-boot
INSTRUMENT truth is always the run dir's `instrument-manifest.tsv`
(+ `options-manifest.tsv` for option ordinals) — this file is the map,
the manifests are the territory.

## 1. The two strategy carriers (architecture law)

1. **Ruleset VM (engine-resident, s5).** Since VM2 (2026-08-30) the
   vocabulary is the GENERAL v2 grammar — §6 below is the authority
   on what it expresses (features over price/funding/depth/IV/clock,
   pair combines, position rows with holds/groups/confirms). The v1
   `level_breach`/`cross_deviation` rows still validate as sugar.
   Push path: author JSON → `backtest` (frozen argv, gates in code) →
   `stage-ruleset` → `commit-ruleset`. **Executes on every tick with
   NO AI, no worker, no cron running**; survives until replaced or
   disabled (per-boot re-commit rides the #7b waiter; the post-boot
   seed push re-warms funding windows and restores open positions).
   Backtestable natively; P&L audited per ruleset-hash by audit-pnl.
2. **Signal cron + Intent lane (worker-resident, s4).** Anything the
   VM cannot express (see §6 "what it cannot express") stays a
   deterministic Python module emitting `order-intent` pushes (the
   carry_signal pattern, launchd cadence). No LLM — but the CRON
   must run for entries/exits. Not natively backtestable through the
   frozen argv; audited the same way (stamped s4, per-tag).
   `claude_worker.parity` compares the two carriers from capture
   alone during migration windows.

Both carriers are paper-only until the operator opens the Stage-3
gate. Caps law for both (the 2026-08-29 $50k research tier):
≤$10k/order, ≤$20k/sym, ≤$100k table total.

## 2. Venues and what each carries

| Venue | Instruments (universe-configurable) | Real-time channels (captured) | Notes |
|---|---|---|---|
| **Polymarket** | Crypto up/down dailies (BTC, ETH; 16:00Z) + equity up/down dailies (NVDA; 20:00Z EDT/21:00Z EST, trading days; auto-refreshed) | CLOB book ticks | **The Stage-3 execution venue.** ≤6 tokens TOTAL today (M1 allocation cap — see §5) |
| **Binance spot** | Crypto pairs + **bStocks tokenized equities** (`<ticker>busdt`; 40+ listed, 3 configured: NVDAB/TSLAB/SPYB) | bookTicker ticks | 24/7 incl. market-closed hours |
| **Binance USDⓈ-M** | Crypto perps (14) + **TradFi stock perps** (148 listed, 8 configured: NVDA/TSLA/SPY/AAPL/TEM/MARA/IONQ/PDD) + dated futures (config-in) | bookTicker ticks; markPrice/funding STREAM venue-dark from this egress (REST fills funding) | TradFi = `TRADIFI_PERPETUAL`, 8h funding ±2% cap, 24/7 |
| **Binance options** | eapi REST discovery live (E2×K8 chains); WS venue-dark | (capture idles, armed) | `.env` lever when venue clears |
| **OKX** | Spot + swaps + futures + options (E2×K8) | bbo-tbt, trades, mark, funding, **L2 depth (books)**, opt-summary (full-family mark/IV/greeks) | Subscribe budget at 4,064/4,096 B — additions need arg-count care |
| **Deribit** | Perps + **USDC linear alt perps** (7 CVFC coins) + spot + options (E2×K8) + combos (config-in) | quote, book.100ms → **L2 depth**, trades, ticker+funding (hourly interest_8h), **DVOL** index, option ticker → opt-summary | The CVFC edge venue |
| **Hyperliquid** | Perp coins (7: BTC/ETH/SOL/XRP/DOGE/ADA/LTC; spot @idx + HIP-4 config-in) | bbo, l2Book, ctx (premium/funding hourly), allMids, outcomeMeta | Hourly funding = fastest funding signal |
| **Bybit** | Spot + linear perps (14: majors + CVFC coins + S1 pilot alts) | orderbook.1 ticks, publicTrade, tickers (mark/funding/OI) | Data-only sixth venue; intent-addressable since the 2026-08-29 unfreeze |

## 3. Derived/offline data (what research actually reads)

- **PMLR capture** (`~/multivenue/logs/run-*/`): per-venue ticks,
  events (funding/mark/gaps/SubDrop/DVOL), depth (okx+deribit,
  K=5/side), opt-summary, engine orders/fills, AI-command timeline,
  manifests. 37+ runs, 338M+ ticks, C6-blessed continuity.
- **candles.db**: 1m/1h/1d candles keyed venue+descriptor (all
  configured instruments incl. equities via klines) · `funding` table
  (5 venues, per-print rates — cadence law: deribit rows are hourly
  samples of interest_8h, ÷8 on daily sums) · `iv_digest` (per-sym
  1m/1h IV from opt-summary) · `depth_digest` (VM2 V6: hourly
  imbalance-OHLC / spread-bps / near-notional per depth-capable
  descriptor).
- **channel map** (`~/multivenue/worker/channel-map.tsv`, regenerate
  via `python -m claude_worker.channel_map`): per-descriptor channel
  capabilities — which features a row on that instrument may
  reference. `python -m claude_worker.coverage_audit` names the data
  holes per class; `python -m claude_worker.parity` compares s4 vs
  s5 from capture.
- **features/** per fetch run: per-sym BBO/mid/spread/tick-rate.
- **reports/**: audit-pnl daily per-strategy/per-hash modeled P&L
  (00:20Z timer) — the audit trail for BOTH carriers.
- **carry lane** (`~/multivenue/worker/carry/`): funding APR boards,
  batches, digests, position state (CVFC-1 + S1 pilot live).

## 4. Live strategy inventory (don't re-invent, benchmark against)

- s0 latency-arb: PM dailies × Binance leads (crypto 0:0/1:1 + NVDA
  2:2 equity pair — the BST thesis).
- s4 lane: CVFC-1 (armed, entry ≥20 APR pts) + S1 pilot (bn-usdm↔
  bybit-linear funding-spread pairs) via the hourly carry cron; the
  5-min xv cron carries the hl↔bn-usdm pair (its okx pair migrated).
- s5 VM: **xv-v2 LIVE since 2026-08-30 08:55Z** (`bfbc5349…`:
  okx:BTC-USDT ↔ binance:btcusdt mid reversion, enter 3.0 / exit
  1.0 bps, $3,000/leg; the 48 h s4-vs-s5 parity window runs —
  vm2-plan §9). Authored + gate-pending: cvfc-v2 `f7d79ce5…` /
  s1-v2 `0cf7433e…` (their hold/warmup laws need older roots),
  merged-v2 `79eaceec…` (the one-table combination).
- External research corpus: `EXTERNAL STRATEGIES TO ONBOARD/`
  (S1–S7/S2R book + CVFC-1 + uplift studies — measured priors,
  rejection tables, walk-forward bars worth reusing).

## 5. Standing constraints a strategy must respect

- **PM ≤6 tokens total** (3 markets) until the allocation-base slice
  lands: PM token ids run `42,2,3,4,5,6` and the 7th collides with
  the reserved `binance:btcusdt` anchor 7 (live-hit 2026-08-29;
  needs an operator-ruled core-config amendment to extend).
- Ruleset gates in code (2026-08-30 numbers): OOS net > $0 · ≥50
  legs (+ ≥10 round trips when ANY position row exists, D-3) · ≥1
  OOS trading day (2→1 by the MVP-tempo ruling) · DD ≤ $7,500 ·
  OBSERVED bounds ≤ 10k/20k/100k — exit 3 is final, no override.
  Referenced feature WINDOWS gate warmup table-globally: an apr72
  row zeroes any backtest on a root younger than 72 h.
- Backtests on this Mac: ALWAYS `--replay-dir <run-dir(s)>` — the
  whole-root merge exceeds 24 GiB RAM.
- Strict-cross fill law = taker-floor economics; maker fill ratios
  are unmeasurable in paper.
- BN real-time markPrice/funding + options WS: venue-dark from this
  egress; REST lanes cover both at native cadence.
- Equity sessions: bStocks/TradFi perps trade 24/7 but PM equity
  dailies exist only for US trading days — session-segment any
  equity study (weekend prints are thin and drifty).
- One engine, serialized worker verbs, paper caps — always.

## 6. What the ruleset grammar expresses (VM2 v2 — the s5 vocabulary)

A ruleset is `{"rows":[…]}`, ≤256 rows, each row one signal:

```
signal = combine( feat_a(instrument, window_a), feat_b(ref, window_b) )
```

- **Features (17):** `mid` `bid` `ask` · `roll_mean` `roll_ema`
  `roll_min` `roll_max` `roll_std` (per-sym minute windows,
  `window_min`, ≤8 distinct windows per sym) · `apr24` `apr72`
  (annualized funding from live prints — deribit ÷8 law engine-side)
  · `mark_px` `mark_iv` (options, from opt-summary) · `depth_imb`
  `depth_spread_bps` `depth_notional` (okx/deribit L2) ·
  `clock_to_funding` `clock_utc_sod`. A feature only validates on an
  instrument whose CHANNELS carry it — the channel map (§3) is the
  per-descriptor truth; rule 10 rejects mismatches at admit time.
- **Combines:** `diff` (natural units — apr spreads, IV spreads),
  `diff_bps` (relative price deviation), `ratio` (×1e9), or omit
  for the single-leg signal. `ref` may be another descriptor or
  absent; `ref_feature`/`ref_window_min` default to the a-side.
- **Entry:** `enter` (9-decimal precision survives funding-sized
  thresholds), `"abs": true` for |signal|, `cmp` `ge`/`le` for
  direction. Direction law: positive signal ⇒ ASK the instrument
  (sell the rich / short the higher-funding venue), negative ⇒ BID;
  refire rows may pin a `side` filter instead.
- **Confirm (optional):** `confirm_feature` + `confirm` +
  `confirm_abs` + `confirm_window_min`; `confirm_pair: true`
  computes the SAME combine over the confirm feature on both legs
  (the S1 sp3 pattern).
- **Position rows** (`exit` present): entry opens a tracked position
  (pair rows hedge both legs, equal notional per leg); the ONE exit
  law `signal × entry_sign ≤ exit` covers |signal| decay AND sign
  flip; `min_hold_s` gates exits, `max_hold_s` is an unconditional
  age-out, `group` N = mutual exclusion (first qualifying row wins —
  the MAX_POSITIONS pattern). Rows without `exit` are stateless
  refire rows (`horizon_ms` re-arm).
- **Sizing/caps:** `max_risk_usd` per LEG; rule 7 statically sums
  EVERY row × legs against 10k/20k/100k — group-blind by design, so
  a wide table means smaller legs (the merged-artifact arithmetic).
- **Instruments:** §9.4 descriptor STRINGS (`okx:BTC-USDT`,
  `binance-usdm:cotiusdt`, bare PM token ids, option names).
  Resolution to SymbolIds happens at stage time against the LIVE
  boot's manifest and re-resolves every boot (#7b) — ordinals
  reshuffle, descriptors are the identity. Restart continuity: the
  seed lane replays 73 h of funding prints and restores open
  positions by row.
- **What it CANNOT express** (stays s4): dynamic best-pair venue
  selection per coin (approximate with one row per pair sharing a
  group), global cross-row position-count caps beyond groups,
  anything needing REST/history at decision time, cross-coin
  breadth/rank statistics, and okx-option BBO execution (the
  offline caps law lists okx options as opt-summary-only even
  though the wire ticks — validator refinement pending).
- **Path to live:** author (`claude-worker/tools_author_v7.py` is
  the worked example) → `backtest --ruleset R --replay-dir D` (use
  a bounded run-dir root; whole-root merges exceed RAM) → gates
  pass → `stage-ruleset` → `commit-ruleset` → parity vs any cron
  predecessor (`claude_worker.parity`) → cron bootout on operator
  order.
