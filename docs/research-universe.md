# Research universe — what the loop agent can analyze (2026-08-29)

The single orientation file for a research session (the ai-session §4
semi-manual loop, and `serve` when Stage 3 keys it). It answers: which
venues exist, what instruments and channels each carries, where the
data lives, and which carrier a strategy idea must target. Per-boot
INSTRUMENT truth is always the run dir's `instrument-manifest.tsv`
(+ `options-manifest.tsv` for option ordinals) — this file is the map,
the manifests are the territory.

## 1. The two strategy carriers (architecture law)

1. **Ruleset VM (engine-resident, s5).** Vocabulary: ≤256 rows of
   `level_breach` (price level ×1e6 on one sym) and `cross_deviation`
   (sym vs ref-sym deviation ≥ edge_bps) → capped order intents.
   Push path: author JSON → `backtest` (frozen argv, gates in code) →
   `stage-ruleset` → `commit-ruleset`. **Executes on every tick with
   NO AI, no worker, no cron running**; survives until replaced or
   disabled (per-boot re-commit rides the #7b waiter). Backtestable
   natively; P&L audited per ruleset-hash by audit-pnl.
2. **Signal cron + Intent lane (worker-resident, s4).** Anything the
   VM cannot express (funding, breadth, cross-venue statistics)
   becomes a deterministic Python module emitting `order-intent`
   pushes (the carry_signal pattern, hourly launchd cadence). No LLM
   — but the CRON must run for entries/exits. Not natively
   backtestable through the frozen argv; audited the same way
   (stamped s4, per-tag).

Both carriers are paper-only until the operator opens the Stage-3
gate. Caps law for both: ≤$100/order, ≤$250/sym, ≤$1,000 total.

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
  1m/1h IV from opt-summary).
- **features/** per fetch run: per-sym BBO/mid/spread/tick-rate.
- **reports/**: audit-pnl daily per-strategy/per-hash modeled P&L
  (00:20Z timer) — the audit trail for BOTH carriers.
- **carry lane** (`~/multivenue/worker/carry/`): funding APR boards,
  batches, digests, position state (CVFC-1 + S1 pilot live).

## 4. Live strategy inventory (don't re-invent, benchmark against)

- s0 latency-arb: PM dailies × Binance leads (crypto 0:0/1:1 + NVDA
  2:2 equity pair — the BST thesis).
- s4 lane: CVFC-1 (armed, entry ≥20 APR pts) + S1 pilot (COTI
  position open) via the hourly carry cron.
- s5 VM: empty this boot; `cvfc-basis-kill` candidate stages when
  its backtest gates pass on new-sym capture.
- External research corpus: `EXTERNAL STRATEGIES TO ONBOARD/`
  (S1–S7/S2R book + CVFC-1 + uplift studies — measured priors,
  rejection tables, walk-forward bars worth reusing).

## 5. Standing constraints a strategy must respect

- **PM ≤6 tokens total** (3 markets) until the allocation-base slice
  lands: PM token ids run `42,2,3,4,5,6` and the 7th collides with
  the reserved `binance:btcusdt` anchor 7 (live-hit 2026-08-29;
  needs an operator-ruled core-config amendment to extend).
- Ruleset gates in code: OOS>0 · ≥50 trades · ≥2 days · DD ≤$200 ·
  caps bounds — exit 3 is final; new instruments need ~2 days of
  capture before a ruleset on them can pass.
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
