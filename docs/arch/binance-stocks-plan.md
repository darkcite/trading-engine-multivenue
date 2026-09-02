# Binance Stocks Integration Plan (BST0–BST7)

**Status: DRAFT — authored 2026-08-29 on operator request. UNCOMMITTED, UNSCHEDULED
until the operator's explicit go.** Authority position: below
`docs/stage2-finish-plan.md` and `docs/mvp-completion-plan.md`; executes BEFORE M5
(operator ruling §0.2) but does NOT open, touch, or depend on the §7 Stage-3 entry
gate. Progress entries append to §8 of this file (stage2-finish-plan pattern).

---

## §0 Operator rulings (recorded 2026-08-29, via AskUserQuestion)

- **§0.1 Product scope: BOTH lanes** — bStocks tokenized-equity spot pairs AND
  TradFi USDⓈ-M perpetuals. The Binance 7,000-stock brokerage is OUT (app-only,
  no public market-data API).
- **§0.2 Timing: before M5.** Capture starts accruing equity history so the M5
  research loop has data. M5 itself still starts only on explicit operator go.
- **§0.3 Shape: REUSE the existing `[binance] spot` / `usdm` universe lists.**
  No new universe sections, no new ordinal bases, no new descriptor namespaces.
  The dedicated-sub-list alternative (`[binance] stocks` / `stock_perps`, base
  4096+, `binance-stock:` descriptors, ~8 parser touch points + worker mirror +
  `[pairs]` law amendment) is REJECTED for now; revisit only if M5 research
  analytics need the separation (recorded alternative, §7.6).
- **§0.4 M1-R1 AMENDED: Polymarket equity up/down dailies are ADMITTED.** The
  original ruling ("crypto up/down binaries only", `docs/arch/mvp-progress.md`)
  was editorial, not code (`core-config/src/universe.rs` `PmMarket` doc comment
  only) — this plan lifts it for equity dailies and extends the M3 refresh
  machinery accordingly (BST3). The doc comment is updated at BST3.

## §1 Product facts (researched 2026-08-29 — sources in §9)

- **bStocks** (tokenized securities, ADGM-regulated): launched 2026-06-11 with 5
  listings (CRCLB, MUB, NVDAB, SNDKB, TSLAB — all /USDT), 46+ by August, >$500M
  AUM in 7 weeks. 1:1 backed, BNB-Chain-portable, **trade 24/7 as ordinary spot
  USDT pairs** on the main exchange — added to Spot Trading Bots 2026-08-28 and
  to Margin collateral (GMEB, 2026-08-12), i.e. they are first-class spot API
  symbols. Naming convention: `<TICKER>B` + `USDT` (stream form `nvdabusdt`).
- **TradFi perpetuals** (USDⓈ-M futures): stock perps on the regular USDM
  segment. Latest batch launched 2026-08-28: TEMUSDT, MRKUSDT, IONQUSDT,
  MARAUSDT, PDDUSDT (underlyings = the Nasdaq/NYSE equities). Tick 0.01, min
  0.01 shares / 5 USDT, **24/7 trading**, funding every 8h capped ±2% (exempt
  from the 8.1 interval-adjustment rule), max 20x. The batch announcement says
  "expand the list" — earlier TradFi perps exist; BST0 enumerates the full set
  from `/fapi/v1/exchangeInfo` rather than trusting announcements.
- **Polymarket equity dailies**: daily "Up or Down" markets per ticker (NVDA,
  TSLA, SPY, …) plus a Stocks category. They resolve at the US close —
  **20:00Z (EDT) / 21:00Z (EST), trading days only** — NOT the 16:00Z crypto
  law. Slug format for equities is UNVERIFIED (BST0 probe).
- **Two dated Binance changes land 2026-08-31:**
  1. Spot `GET /api/v3/exchangeInfo` moves symbol permission data from
     `permissions` to `permissionSets`. Our discovery reads only
     `symbol`/`status`/`filters` (structural skip elsewhere) ⇒ expected no-op;
     BST0 re-probes on/after the date to pin it.
  2. TradFi-perp mark price: Price 2 basis moves from 30 s to 1-minute moving
     average. Data-semantics only (markPrice stream values); no code impact;
     research-side note.

## §2 Integration thesis

Both product lanes speak wire protocols the engine already implements end-to-end:

| Lane | Universe list | Host | Streams | Discovery | Descriptor |
|---|---|---|---|---|---|
| bStocks | `[binance] spot` (append) | `BINANCE_WS_HOST` | `/ws/<sym>@bookTicker` | per-symbol `/api/v3/exchangeInfo?symbol=` | `binance:<sym>` |
| TradFi perps | `[binance] usdm` (append) | `BINANCE_FUT_WS_HOST` | `@bookTicker` + `@markPrice` | `/fapi/v1/exchangeInfo` page | `binance-usdm:<sym>` |

Under §0.3 the ENGINE portion of this integration is **configuration, not code**:
append entries to `~/multivenue/universe.toml`, and the M1 multi-connection lane,
the exchangeInfo boot audit, SymbolId allocation (file-order ordinals; usdm base
512), PMLR capture, instrument-manifest.tsv, worker candles/funding lanes, and
audit-pnl attribution all pick the instruments up through existing seams. The only
NEW code in this plan is worker-side: the equity-dailies refresh family (BST3) and
test fixtures pinned from live probe bodies (BST2/BST6). **Zero hot-path changes ⇒
the alloc gate stays 38 by construction.**

The arb thesis is the existing one, transplanted: PM equity daily (up/down binary)
× Binance 24/7 stock price feed, exactly the crypto-dailies × `btcusdt` law, with
`[pairs] "P:B"` already wired to `binance.spot` indices — which §0.3 keeps true
for bStocks with no law amendment.

## §3 Workstreams

### BST0 — Live probes (no code, Mac only; pitfall-11 law: fixtures come from live bodies)

1. `GET /api/v3/exchangeInfo?symbol=NVDABUSDT` (and one more bStock): confirm
   status TRADING, PRICE_FILTER/LOT_SIZE present, record tickSize/stepSize.
   Save the body → BST2 fixture. Re-run on/after **08-31** (permissionSets).
2. `GET /fapi/v1/exchangeInfo`: enumerate ALL TradFi perps (don't trust the
   announcements); diff row shape vs crypto perps (`contractType`, any new
   fields — parse_row must skip them structurally, not BadRow). Save body.
3. WS smoke: `nvdabusdt@bookTicker` on stream host; `temusdt@bookTicker` +
   `temusdt@markPrice` on fstream — via a short `--raw-tap` boot (BST5 does the
   full one; this can fold into it if the operator prefers one boot).
4. Gamma probe: resolve one NVDA and one TSLA daily — pin the **equity slug
   format**, `endDate` (close time), weekend/holiday listing cadence (is
   Monday's market listed by Saturday?). These are the BST3 laws' inputs.
5. Klines: `/api/v3/klines?symbol=NVDABUSDT&interval=1m` and
   `/fapi/v1/klines?symbol=TEMUSDT` — confirm the worker candles adapters need
   nothing (expected: they don't).

**Done-tell:** a probe note appended to §8 with the five bodies' key facts;
open questions in §7 resolved or narrowed.

### BST1 — Universe config (config-only; append-never-reorder law)

- Append to `~/multivenue/universe.toml`:
  - `[binance] spot` += bStocks stream symbols (start: the liquid subset the
    operator picks from BST0's enumeration; e.g. `nvdabusdt`, `tslabusdt`,
    `crclbusdt`, …). Ordinals continue file-order in the spot block.
  - `[binance] usdm` += TradFi perp symbols (e.g. `temusdt`, `mrkusdt`,
    `ionqusdt`, `marausdt`, `pddusdt`). Ordinals continue in the base-512
    block; each gets the standard capture-only `@markPrice` connection.
  - `[polymarket] markets` += equity-daily token pairs (manually once for the
    first boot; BST3 automates) — equity entries append AFTER the crypto
    dailies block (ordering law, BST3.3).
  - `[pairs] map` += explicit `"P:B"` rows pairing each PM equity daily with
    its bStock spot index. Written ONCE here; refresh never touches `[pairs]`
    (standing law preserved).
- Update `universe.toml.example` comments: the equity tail convention + a
  pointer to this plan. No grammar change ⇒ no `core-config` edits, no
  `full_src()` fixture change, no `universe_toml` fuzz-corpus change.
- Caps check: spot ≤500, usdm ≤500, PM ≤64 markets/128 tokens — headroom is
  ample (M1 caps unchanged).

### BST2 — Boot discovery contingency (expected zero code)

The spot per-symbol lane and the usdm page lane already audit the new entries at
boot. From the recent lessons (binance `parse_filters` Truncated-vs-BadRow, Bybit
sub-1e-9 basePrecision truncation): add regression tests pinning the BST0 live
bodies (bStock row + TradFi perp row) through `BnDiscovery::ingest_body` /
`parse_row` / `parse_filters` in `crates/ingress-binance/src/discovery.rs` tests.
If (and only if) a probe body trips the parser, fix WITH the pinned fixture,
red→green. No new fuzz targets (no new parser; `binance_exchange_info` target
already covers the grammar — drop the probe bodies into its corpus).

### BST3 — PM equity dailies refresh (the real code; worker module, NOT a verb)

Extends `claude_worker/src/claude_worker/universe_refresh.py` + config. The
7-verb CLI surface stays FROZEN; frozen-202 pytest stays byte-untouched.

1. **Config:** new `[equity_dailies] underlyings = [...]` table in
   `~/multivenue/pm-dailies.toml` (slug names per BST0; example file updated).
   `[dailies]` (crypto) is untouched.
2. **Equity date law:** target date = the nearest US **trading day** whose
   daily is unresolved: today (America/New_York, stdlib zoneinfo — DST-correct)
   if before 16:00 ET and a trading day, else the next trading day (weekends +
   NYSE holidays via a small static holiday table for the current year,
   operator-maintained — no new dependency). Crypto keeps the 16:00Z law.
3. **Ordering law:** the rewritten `[polymarket] markets` array = crypto block
   (from `[dailies]`, existing order law) followed by equity block (from
   `[equity_dailies]`, config order) — so the `[pairs]` index space stays
   static and `[pairs]` is never rewritten. Both lists are order-aligned with
   their `binance.spot` counterparts by convention (comment law, as today).
4. **Family-independence law (amends the all-or-nothing best-effort law):** the
   crypto block failing ⇒ whole refresh aborts, file untouched (as today). The
   equity block failing (holiday, unlisted next-day market, Gamma miss) ⇒
   **reuse the previous file's equity entries verbatim** (parse the old array
   tail by count), refresh crypto, exit 0 with a logged `equity=stale` note. A
   stale resolved equity market is harmless (quiet subscription; expired
   dailies drop out of the observed universe — M1 law).
5. **Refresh slots:** equity dailies expire at US close, after the 16:05Z T2
   restart. Add TWO launchd restart slots — **20:15Z and 21:15Z** — so the
   next-day market loads ≤15 min after close in both EDT and EST (the pre-close
   slot of the pair resolves idempotently to the current market; restarts cost
   seconds of capture gap, same as existing T2 slots). Plist edit follows the
   M3 fleet pattern; `scripts/` exec-bit law (git diff --summary) applies.
6. **Tests:** date-law (DST boundary, weekend, holiday), slug candidates,
   family independence (equity miss keeps tail + refreshes crypto; crypto miss
   aborts), ordering, idempotence — mocked `get_fn`, no live calls. Pytest
   count grows above 477; frozen 202 untouched.
7. **M1-R1 doc updates:** `universe.rs` PmMarket comment + `universe.toml.example`
   + the CLAUDE.md universe runbook line (16:00Z note gains the equity clause).

### BST4 — Worker data lanes (expected zero code; verification only)

- **Candles:** `read_universe_lanes` derives targets from the universe lists ⇒
  bStocks land in the spot lane (`binance:<sym>`, `/api/v3/klines`) and TradFi
  perps in the usdm lane (`binance-usdm:<sym>`, `/fapi/v1/klines`)
  automatically. Verify rows appear in `candles.db` for 1m/1h/1d after one
  hourly timer pass; conflicts=0.
- **Funding:** the WS11 REST funding fold-in (`binance-usdm:` descriptors)
  covers TradFi perps automatically — verify rows. The live `@markPrice`
  STREAM, however, is subject to the standing "BN markPrice venue-side
  unreachable from this network" finding (CLAUDE.md, re-probed 2026-08-29,
  ack-then-silent): expect the perps' mark/funding EVENT capture to stay
  silent until that clears; bookTicker ticks and REST funding are unaffected.
- **iv_digest:** N/A (no stock options anywhere on the venue).
- **Fetch seeding:** one `claude-worker fetch` after the first boot; done-tell
  `unresolved=0` (PM equity tokens get Gamma meta + YES/NO pairs through the
  existing `CLAUDE_WORKER_UNIVERSE_FILE` seam; no `fetchers.py` change).
- **audit-pnl / manifests:** `instrument-manifest.tsv` carries the new
  descriptors every boot (D3 law) ⇒ attribution and §6 descriptor-keyed books
  work unchanged.

### BST5 — Live smoke + soak (one boot; one-engine law)

Standard M-phase exit shape: stop the standing launchd instance, `cargo build
--release -p cli` (G0 relink law), one zero-flag boot on the expanded universe,
then hand back to the fleet. Done-tells:

- Boot discovery: bStocks validated per-symbol; TradFi perps found in the usdm
  page; counts logged.
- PMLR capture: ticks for ≥1 bStock and ≥1 TradFi perp; PM equity-daily book
  ticks. (markPrice/funding EVENTS only if the standing BN-markPrice
  reachability finding has cleared — not a gate for this plan.)
- `audit-replay` on the run dir: integrity ZERO across venues.
- `claude-worker fetch`: unresolved=0, conflicts=0.
- First 20:15Z/21:15Z slot observed doing an equity refresh (or an idempotent
  no-op, per season).
- Then ≥24 h inside the standing fleet with the M4 pnl timer and hourly candles
  running clean (equity rows accruing).

### BST6 — Gates (stay-green law)

- `cargo nextest run --workspace` — 1349 + BST2 additions, all green.
- `cargo test -p bench --test alloc_assertions --release -- --test-threads=1`
  — **38/38, 0 B/op, unchanged** (no hot-path code in this plan; fresh
  `Compiling bench` guard applies).
- `cd claude-worker && uv run pytest` — 477 + BST3 additions; frozen 202
  byte-untouched.
- Fuzz: no new targets; drop BST0 bodies into `binance_exchange_info` corpus
  and run that target ≥300 s clean.
- `make license-check` (any new `.py` test files carry SPDX; no new deps ⇒ no
  `license-deps` run), `make lint`, `cargo fmt --check` on touched files.

### BST7 — Docs close

- CLAUDE.md CURRENT STATE: one entry (scope ruling, what landed, new
  stay-green numbers, the two refresh slots, M1-R1 amendment).
- `docs/m5-runbook-notes.md`: pointer — equity descriptors exist in candles.db
  and PMLR from <date>; research universe now includes equities.
- §8 of this file: the closure entry.

## §4 Explicit non-goals (this plan changes NOTHING here)

- **No execution, no executor, no venue dispatchers, no live ramp** — the §7
  Stage-3 entry gate is untouched and stays the operator's to open.
- No Binance brokerage lane (no public API). No stock options (none exist on
  the venue). No new crates, no PMLR format change (v2, kinds 0–7 untouched),
  no universe grammar change, no new descriptor namespaces, no verb changes.
- No strategy/ruleset work: equities enter the RESEARCH universe (capture +
  candles); ruleset drafting on them is M5's job.

## §5 Risks

- **Eligibility/geo (execution-side only):** bStocks and TradFi perps are
  jurisdiction-gated (ADGM prospectus; not offered to US persons). Public
  market DATA is account-less and unrestricted — this plan is data-only. The
  engine's arb as designed executes on POLYMARKET, with Binance as signal-side
  only, so no Binance order-side eligibility is implied even at Stage 3; any
  future decision to TRADE these instruments on Binance needs its own
  eligibility ruling. Flagged so it cannot be discovered late.
- **Day-old market:** TradFi perps launched 2026-08-28; spreads/liquidity
  unproven — capture will tell; research gates on data, not hope.
- **Weekend/holiday regime:** bStocks + perps trade 24/7 while the underlying
  is closed (thin, drifty) and PM equity dailies exist only for trading days.
  Engine needs no change (streams stay live); research must session-segment.
  The funding/mark behavior of perps over closed-market hours is exactly what
  the markPrice capture is for.
- **Corporate actions:** splits/dividends produce price discontinuities in
  candles and (for bStocks) token adjustments handled venue-side. Data-lane
  impact: none mechanically; research-side awareness note in m5-runbook.
  Delistings follow the append-never-reorder maintenance law.
- **BN markPrice reachability (standing):** the fstream `@markPrice` subscribe
  is ack-then-silent from this network (re-probed 2026-08-29). TradFi-perp
  mark/funding STREAM capture inherits that until it clears; funding data
  still lands via the REST fold-in. No lever in this plan — same standing
  item as before Stage 3.
- **08-31 changes** (§1): both expected no-ops for us; BST0 re-probe pins the
  exchangeInfo one the day after it lands.
- **Equity slug/cadence unknowns** (§7.1–7.2) gate BST3's final shape — probes
  first, code second.

## §6 Sequencing & effort

BST0 (½ day) → BST1+BST2 (½ day) → BST3 (1 day) → BST4 (verification, ½ day)
→ BST5 smoke+24h soak → BST6 gates → BST7 docs. Roughly **2½ working days of
effort + one soak day**, all before M5 per §0.2. Single session suffices;
checkpoint commits per repo law (operator-authorized, explicit-path staging,
`BST:` message prefix suggested).

## §7 Open questions (probe-gated)

1. Equity daily slug format on Gamma (e.g. `nvda-…` vs `nvidia-nvda-…`)? → BST0.4.
2. Holiday/weekend listing cadence (when does Monday's market list)? → BST0.4.
3. Full current TradFi perp + bStocks symbol lists (announcements lag) → BST0.1–0.2.
4. TradFi perp exchangeInfo row deltas (new fields, contractType value) → BST0.2.
5. Which subset the operator wants in the first universe (all vs liquid top-N)?
   → operator picks at BST1 from BST0's enumeration.
6. Revisit-trigger for the rejected dedicated-sub-list shape: only if M5
   research needs stocks isolated from crypto spot in per-list analytics.

## §8 Progress log

- 2026-08-29 — plan drafted (this document). Awaiting operator go per §0.2.

## §9 Sources (researched 2026-08-29)

- Binance: bStocks launch PR — https://www.prnewswire.com/news-releases/binance-exchange-launches-bstocks-tokenized-securities-11-backing-and-247-trading-302798876.html
- Binance: US stocks + bStocks preview PR — https://www.prnewswire.com/news-releases/binance-launches-us-stocks-trading-and-previews-bstocks-tokenized-securities-302787226.html
- Binance announcement: TradFi perpetuals launch 2026-08-28 (TEM/MRK/IONQ/MARA/PDD, 24/7, funding, tick) — https://www.binance.com/en/support/announcement/detail/32ac927d1cbe4aa3b527eca1c401a98f
- Binance announcement: TradFi mark-price P2 1-minute basis 2026-08-31 — https://www.binance.com/en/support/announcement/detail/1e9c1d2dff0b4d48a04c09f84a39fcb8
- Binance announcement: GMEB as margin collateral 2026-08-12 (+ ADGM/eligibility disclaimers) — https://www.binance.com/en/support/announcement/detail/2e02db3c8dcd4a03a365655c501aa7c9
- Binance announcement: bStocks added to Spot Trading Bots 2026-08-28 — https://www.binance.com/en/support/announcement/detail/4fea39f8b6e54e1b88e388fc90ce323e
- bStocks AUM >$500M — https://smbtech.au/news/binances-tokenized-stock-product-passes-500-million-mark-in-seven-weeks/
- Spot API exchangeInfo permissions→permissionSets change (effective 2026-08-31) — https://developers.binance.com/docs/binance-spot-api-docs
- Polymarket stocks category / equity dailies — https://polymarket.com/finance/stocks , https://polymarket.com/predictions/nvda , https://247wallst.com/companies/tsla/prediction-markets/
- 2026-08-29 — **BST0–BST6 EXECUTED (operator go; subset ruling
  3+3+8; both refresh slots approved).** Probes: bStocks 40+ (tick
  0.01/step 0.001; `<TICKER>B` naming is ambiguous vs crypto — use
  the fapi discriminator instead), **TradFi perps enumerable cleanly:
  `contractType=TRADIFI_PERPETUAL` + `underlyingType=EQUITY`, 148
  TRADING**; klines fine both lanes; **Gamma equity slug law =
  `<ticker>-up-or-down-on-<month>-<day>-<year>`, endDate 20:00Z
  (EDT), Monday listed by Saturday** (§7.1/7.2 resolved). Landed:
  universe 3 bStocks + 8 perps + NVDA daily (pairs "2:2"); BST3
  equity-dailies family in universe_refresh (ET date law + NYSE-2026
  holiday table + family-independence + tail-carry; 9 tests);
  restart slots 2015/2115 in daily-restart.sh; **BST2 real fix:
  `TRADIFI_PERPETUAL` classified as Perpetual (was Other⇒is_dated —
  148 live instruments misclassified) + 2 live-shape fixture pins.**
  Live: refresher printed `equity=3@2026-08-31` first try; equity
  ticks captured on a SATURDAY (TEM 120, TSLAB 148, NVDAB, SPYB);
  fetch conflicts=0. Gates: nextest 1351 · pytest 497 · alloc/lint/
  license per session log.
- 2026-08-29 — **TWO findings for the record.** (1) MY integration
  bug (fixed in minutes): a comment-insertion replaced `[polymarket]`
  inside the HEADER COMMENT of universe.toml, crash-looping the boot
  ("malformed section header"); repaired; lesson — anchor config
  edits on the section line with surrounding newlines, never a bare
  token. (2) **LATENT M1 CAP, operator decision needed:** PM token
  ids allocate `42,2,3,4,5,6,…` so the 7th token collides with the
  reserved `binance:btcusdt` anchor 7 ⇒ **PM is capped at 6 tokens
  (3 markets) TOTAL**. The operator-ruled 3-equity subset therefore
  runs NVDA-only beside BTC+ETH until an allocation-base amendment
  (own slice: PM overflow base clear of BN spot 7..506 and usdm
  512+, e.g. 1536+; core-config `universe.rs` + tests + migration
  note). TSLA/SPY dailies + their pairs re-enter with that slice.
- 2026-08-29 — **STOCKS VERIFIED + PAUSE (operator order).** Cross-
  validation vs independent venue reads: 9/11 instruments matched at
  0.00–0.18% deviation (fetch-age drift; e.g. NVDAB 218.755 vs
  218.670, SPY perp 770.895 vs 771.010, TEM 64.095 vs 63.980);
  MARA/IONQ confirmed in capture directly (25/36 ticks — thin
  Saturday names that printed after the fetch snapshot). PM NVDA
  daily live BOTH sides (23 ticks/token; up-token at 0.585). ALL 11
  stock instruments + the equity daily are receiving proper data.
  BST0–BST7 complete except the 08-31 permissionSets re-probe (due
  after the date) and the ≥24 h fleet soak (running passively).
  Research loop deliberately NOT started — operator-ordered pause.
