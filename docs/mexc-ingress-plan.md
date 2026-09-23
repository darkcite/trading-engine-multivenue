# MEXC Ingress Plan (MX0–MX9) — spot · futures · stocks · tradfi · funding

**Status: LIVE 2026-09-23 08:48Z in the paper engine — MX1–MX9 all executed (MX0
2026-09-20). Data-only. Committed `b7678dd` (engine + crate + config), `bbc8553` (worker +
news) and the docs commit that carries this line (§0 GO-LIVE).**
Authored 2026-09-20 on operator request; the seven open questions were ruled 2026-09-23 (§0, §9);
what was built and measured is §11.
Adds a SEVENTH market-data venue (`VenueId::Mexc = 7`, tick lane 6). Data-only with an
exec-ready shape (operator ruling O-MX1). Opens nothing: no Anthropic API, no `serve`,
no Stage-3 gate, no execution.

Reference precedent throughout: **Bybit / WS9** (`VenueId::Bybit = 6`, landed 2026-08-29)
for venue mechanics, and **BST** (`docs/arch/binance-stocks-plan.md`, BST0–BST6 executed
2026-08-29) for equities/TradFi doctrine — equities ride the EXISTING `Spot`/`Perp`
instrument classes; no new `InstrumentClass` is created.

---

## §0 Operator rulings (via AskUserQuestion — O-MX1…3 on 2026-09-20; Q-MX1…7 and MX9-EXEC on 2026-09-23)

| id | ruling |
|---|---|
| **O-MX1** | **Data-only now, exec-ready structure.** Capture + backtest + research only. No `exec-mexc`, no keys, no risk gate, no `ExecMode` arm wired. The crate is *shaped* so an exec arm can be added later without rework (§4 D10), but nothing live. |
| **O-MX2** | **All four classes in v1**: crypto spot + crypto perps + xStocks spot equities + TradFi perps. One universe, one venue, four lanes. |
| **O-MX3** | **The protobuf decoder lives in `core-parse`** as a new varint/PB module beside the JSON scanners, with its own proptest + cargo-fuzz target. Reusable by any future PB venue; the venue crate stays thin. |
| **Q-MX1** · 2026-09-23 | **`venue_seq` carried everywhere; NO gap events; regressions counted.** Spot book `version`, spot trade = the LEADING DIGITS of the `tradeId` string, futures book `version`, futures deal `i`; `Tick.venue_seq` = the low 32 bits, `ChannelEvent.venue_seq` the full `u64`. No `TradeGap`/`BookGap` — the §6.2 chain law does not apply (sampled/snapshot streams skip versions by design). Instead a new counter family `engine_ingress_<venue>_seq_regressions_total`, registered for every venue and incremented only by MEXC, counts a seq strictly below the last-seen one. §4 D3. |
| **Q-MX2** · 2026-09-23 | **`push.ticker` on every perp** (3 subscriptions per perp ⇒ 13 perps per futures connection, 39 subs under the measured ≥ 40). |
| **Q-MX3** · 2026-09-23 | **Funding `v1` seeded per perp at boot** from REST `GET /api/v1/contract/funding_rate/{SYM}` (`nextSettleTime`, `collectCycle`) and advanced by THAT symbol's cycle; unseeded ⇒ `v1 = 0`. The worker funding lane stays the authority for history. §4 D4. |
| **Q-MX4** · 2026-09-23 | **`fees.toml` takes the published rates, flagged UNVERIFIED**: `[fees.mexc] spot = "0:5"`, `perp = "1:4"` (the dearest configured plate, FX), bare `mexc = "1:4"`. `fees.toml.example` carries it. |
| **Q-MX5** · 2026-09-23 | **First universe**: spot `BTCUSDT ETHUSDT SOLUSDT AAPLXUSDT SPYXUSDT` + perp `BTC_USDT ETH_USDT XAU_USDT USOIL_USDT EUR_USDT SPY_USDT AAPLSTOCK_USDT` — the proposal, unchanged. |
| **Q-MX6** · 2026-09-23 | **MEXC IS in `build_ai_universe`** — AI rulesets may address it. Caps: `mexc:` → `CAP_PRICE`, `mexc-perp:` → `CAP_PRICE \| CAP_FUNDING`. Supersedes the "derived, not ruled" omission below. |
| **Q-MX7** · 2026-09-23 | **MEXC joins the news lane**: source kinds `json-mexc-ann` (the listings / delistings / maintenance announcement sections on `www.mexc.co` — `www.mexc.com` answers 403 from this network) and `ping-mexc` (spot + futures REST hosts); five rows in `news.toml.example`. **Trusting the `www.mexc.co` origin is still an open operator approval.** |
| **MX9-EXEC** · 2026-09-23 | **The launchd engine is LIVE on mainnet, so MX9's `--raw-tap` engine boot (steps 1–2) is REPLACED** by a standalone live smoke that never stops the engine: `crates/cli/tests/mexc_live_smoke.rs` (`#[ignore]`), run with a separate `CARGO_TARGET_DIR` so `target/release/multivenue-engine` — the wrapper's binary — is untouched. Git: nothing committed; the operator stages and commits. |
| **GO-LIVE** · 2026-09-23 (second session) | (a) Commit the lane as three commits by area (engine+crate+config · worker+news · docs), no push. (b) **A boot `funding_rate/{SYM}` seed failure is NOT fatal** — error log, that perp boots unseeded (Funding `v1 = 0`); symbol discovery failures stay fatal. (c) Trust `www.mexc.co` and enable all five MEXC news sources in the live `news.toml`. (d) Go live NOW: `[mexc]` with the Q-MX5 twelve in `~/multivenue/universe.toml`, release build, engine restart (paper). |

Derived, not ruled (stated here so a later reading does not mistake them for operator law):
MEXC is **not** added to `build_ai_universe` (the Bybit precedent deliberately omitted the
sixth venue); MEXC **is** added to `caps_of_descriptor` on both sides of the Rust↔Python
mirror, because the funding lane needs `CAP_FUNDING` to be expressible.
**Superseded 2026-09-23 by Q-MX6** for `build_ai_universe` (MEXC is in it; Bybit stays out);
the `caps_of_descriptor` half stands.

---

## §1 Product facts — MEASURED 2026-09-20, 09:43–09:52Z

Everything in this section came from live bodies on the operator's machine, not from a
doc. Sources in §10. **Pitfall 7 applies: these are fixtures-from-live, and they must be
re-smoked with `--raw-tap` before any parser is trusted.**
Re-smoked live 2026-09-23 by the MX9 smoke (ruling MX9-EXEC); the measurements below are
kept as taken, and every correction is dated in **§1.7**.

### 1.1 Spot market data — Protocol Buffers, not JSON

| fact | measured value |
|---|---|
| WS endpoint | `wss://wbs-api.mexc.com/ws` |
| Wire format | **Protocol Buffers (proto3), WS opcode BINARY** |
| Subscribe | ONE text frame `{"method":"SUBSCRIPTION","params":[…]}` |
| Ack | ONE text frame **enumerating per-param outcomes** — `{"id":0,"code":0,"msg":"Subscribed successful! [a,b,c]. Not Subscribed successfully! [d]. Reason： Blocked! "}` (**2026-09-23: the live success ack is a plain comma echo — §1.7**) |
| **Subscriptions per connection** | **30** — measured: 32 requested, **30 of 32** ever pushed data |
| Connection lifetime | ≤ 24 h (venue-documented) |
| Idle reap | 30 s with no subscription, 60 s with an inactive one (venue-documented) |
| Symbol case | UPPERCASE, verbatim (`BTCUSDT`, `AAPLXUSDT`) |
| Observed frame rate | ~100 frames/s for 2 channels at `@10ms` on one symbol |

**Channels used (both `@10ms`, the fast tier — `@100ms` also works):**

```
spot@public.aggre.bookTicker.v3.api.pb@10ms@<SYM>   -> BBO  (Tick)
spot@public.aggre.deals.v3.api.pb@10ms@<SYM>        -> prints (ChannelId::Trade)
```

**`spot@public.increase.depth.v3.api.pb@<SYM>` is REFUSED** — the ack returns
`Not Subscribed successfully! … Reason： Blocked!`. Incremental depth is unavailable on
the public tier. **There is therefore no MEXC spot book to build**, only a BBO. This
matches the engine's existing BBO-tick doctrine (Bybit `orderbook.1`, Binance
`bookTicker`) and removes the whole snapshot/delta reconciliation class of work.

**Exact wire layout — decoded from a live 133-byte frame** (`PushDataV3ApiWrapper`):

```
0A 34 "spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT"   f1  channel   (string)
1A 07 "BTCUSDT"                                               f3  symbol    (string)
30 A7 D3 D2 F1 8B 34                                          f6  sendTime  (varint, ms)
DA 13 3E                                                      f315 publicAggreBookTicker
   0A 08 "80535.88"                                              f1 bidPrice     (ASCII string)
   12 08 "0.380497"                                              f2 bidQuantity  (ASCII string)
   1A 08 "80535.89"                                              f3 askPrice     (ASCII string)
   22 0A "0.33336356"                                            f4 askQuantity  (ASCII string)
   2A 0B "81721676217"                                           f5 version      (ASCII string)
   30 8E D3 D2 F1 8B 34                                          f6 lastOrderCreateTime (varint)
```

(2026-09-23: `0A 34` is a 52-byte channel — the `@100ms` spelling; the `@10ms` string is 51
bytes, `0A 33`. The layout itself re-decoded exactly as above on live frames — §1.7.)

Deals frame (486 B observed), `f314 publicAggreDeals` tag `D2 13`:

```
D2 13 A2 03                        f314, len 418
  0A 4A                              f1 deals[0], len 74
     0A 08 "80535.88"                  f1 price      (ASCII string)
     12 0A "0.01362099"                f2 quantity   (ASCII string)
     18 02                             f3 tradeType  (varint; 1 = buy, 2 = sell)
     20 B6 D9 D2 F1 8B 34              f4 time       (varint, ms)
     2A 29 "730292425431437318X0_…"    f5 tradeId    (string)
  0A 49 …                            f1 deals[1] …
```

**The decisive property: every price and quantity is an ASCII decimal STRING inside the
protobuf.** A length-delimited PB field is `tag-varint | len-varint | bytes`, so the
payload bytes are a borrowed `&[u8]` slice of the rx buffer that feeds
`core_parse::scan_price_1e6` unchanged. **No protobuf library, no codegen, no
intermediate representation, zero allocation, zero copy.** The whole decoder is a
varint reader plus a forward tag walk.

Oneof body tags (derived `(field << 3) | 2`, the two starred are **verified live**):

| field | message | tag bytes |
|---|---|---|
| 301 | `PublicDealsV3Api` | `EA 12` |
| 302 | `PublicIncreaseDepthsV3Api` | `F2 12` |
| 303 | `PublicLimitDepthsV3Api` | `FA 12` |
| 305 | `PublicBookTickerV3Api` | `8A 13` |
| 313 | `PublicAggreDepthsV3Api` | `CA 13` |
| **314** | **`PublicAggreDealsV3Api`** | **`D2 13`** ★ |
| **315** | **`PublicAggreBookTickerV3Api`** | **`DA 13`** ★ |

Note `createTime` (f5) was **absent** in every observed frame; only `sendTime` (f6) is
populated. proto3 permits any field order — the scanner must be an order-agnostic
forward walk, not a positional template (§4 D5).

### 1.2 Futures market data — plain JSON, uncompressed

| fact | measured value |
|---|---|
| WS endpoint | `wss://contract.mexc.com/edge` |
| Wire format | **JSON text, UNCOMPRESSED** (docs claim depth compression is default since 2025-04-09 — **measured false when `compress` is omitted**) |
| Subscribe | one frame per symbol per channel: `{"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}` |
| Ack | `{"channel":"rs.sub.depth.full","data":"success","ts":…}` — **carries NO symbol** |
| **Subscriptions per connection** | **≥ 40** — 40 requested, 40 acked `success`, 39 pushing inside 25 s (no cap hit; the 30-cap is spot-only) |
| Keepalive | `{"method":"ping"}` → `{"channel":"pong","data":<ms>,"ts":<ms>}` |
| Symbol case | `BASE_QUOTE` uppercase with underscore (`BTC_USDT`, `XAU_USDT`, `AAPLSTOCK_USDT`) |

**Measured push cadence — this is the load-bearing measurement of the whole plan:**

| channel | median gap | min | max | verdict |
|---|---|---|---|---|
| `push.depth.full` (limit 5) | **269 ms** | 0 | 695 | **the BBO source** |
| `push.depth` (incremental) | 211 ms | 27 | 376 | not needed — no book is built |
| `push.ticker` | **2 748 ms** | 2 716 | 2 793 | **far too slow for BBO — capture-only** |

`push.ticker` carries `bid1`/`ask1`, but at a ~2.75 s cadence it is a *statistics* feed,
not a quote feed. Using it as the tick source would put a 2.75 s-stale book into the
engine. **`sub.depth.full` with `limit: 5` is the futures BBO** — a self-contained top-5
snapshot with `version` and `cts`, needing no state machine and no reconciliation.
(2026-09-23: the 269 ms median is BTC_USDT's. `depth.full` pushes on CHANGE, so a quiet
perp — EUR, SPY, AAPLSTOCK — goes 4–5 s between pushes; §1.7.)

Channel payloads:

```jsonc
// push.depth.full
{"symbol":"BTC_USDT","data":{"cts":1789897581009,
  "asks":[[80468.7,3446,2],[80469.1,1239,1],…],   // [price, volume, orderCount]
  "bids":[[80468.6,31288,7],…],
  "version":41925002140},"channel":"push.depth.full","ts":1789897581013}

// push.deal
{"symbol":"BTC_USDT","data":[{"p":80489,"v":11,"T":1,"O":3,"M":1,
  "t":1789897547210,"i":"16270106116","cts":"1789897547210"}],"channel":"push.deal",…}
//   T: 1 = buy aggressor, 2 = sell aggressor;  i = numeric trade id (venue_seq)

// push.ticker  (capture-only: Mark / Funding / OI in ONE channel, the Bybit `tickers` shape)
{"symbol":"XAU_USDT","data":{"lastPrice":4377.18,"bid1":4377.16,"ask1":4377.2,
  "indexPrice":4377.63,"fairPrice":4377.21,"fundingRate":0,"holdVol":92415393,
  "volume24":24315543,"amount24":106460536.13,"timestamp":1789897545754,…},…}
```

### 1.3 Instrument inventory (measured 2026-09-20)

| surface | count | notes |
|---|---|---|
| Spot symbols (`/api/v3/exchangeInfo`) | **1 966** | `status:"1"` (not `"TRADING"`), per-symbol `makerCommission`/`takerCommission` |
| — of which xStocks equities | **79** `*X/USDT` | `conceptPlates:["Innovation","Tokenized Stocks"]`, `fullName:"Apple xStock"` |
| Perp contracts (`/api/v1/contract/detail`) | **1 174** | all `futureType:1`; settle USDT 1051 / USDC 78 / USD1 35 |
| — on the `mc-trade-zone-tradfi` plate | **431** | all `state:0`, all `apiAllowed:true` |
| — `mc-trade-zone-Stock` | 400 | single-name equity perps |
| — `mc-trade-zone-stockindex` | 66 | SPY, QQQSTOCK, TQQQ, SQQQ … |
| — `mc-trade-zone-ETF` | 60 | |
| — `mc-trade-zone-Commodities` / `metals` / `OIL` / `Forex` | 19 / 13 / — / — | XAU, XAUT, SILVER, USOIL, UKOIL, EUR, JPY |

**Both equity planes are structurally ordinary.** xStocks parse as plain spot rows and
TradFi perps as plain contract rows — exactly the BST2 outcome on Binance ("expected zero
code", and it was). **Discovery needs no equity-specific branch.**
(2026-09-23 counts: spot 1 954 rows, all live; perp 1 185 rows, 1 134 live — §1.7.)

Worked examples:

```
AAPLXUSDT   spot   permissions:["SPOT"]  maker 0  taker 0.0005  basePrec 3  "Apple xStock"
SPYXUSDT    spot   permissions:["SPOT"]  maker 0  taker 0.0005  basePrec 3  "SP500 xStock"
AAPLSTOCK_USDT perp ctSize 0.01 priceUnit 0.01 maker 0 taker 0      indexOrigin
                    [BINANCE_FUTURE, BITGET_FUTURE, BINANCETICKER, PYTH, KAIKO]
SPY_USDT       perp ctSize 0.001 priceUnit 0.01 maker 0 taker 0
XAU_USDT       perp ctSize 0.001 priceUnit 0.01 maker 0 taker 0.0002
USOIL_USDT     perp ctSize 0.01  priceUnit 0.01 maker 0 taker 0.0001
EUR_USDT       perp ctSize 1     priceUnit 0.0001 maker 0.0001 taker 0.0004
BTC_USDT       perp ctSize 0.0001 priceUnit 0.1  maker 0 taker 0.0002
```

### 1.4 Funding

```
GET /api/v1/contract/funding_rate/BTC_USDT      (rate limit 20 / 2 s)
{"success":true,"code":0,"data":{"symbol":"BTC_USDT","fundingRate":0.0001,
 "maxFundingRate":0.0018,"minFundingRate":-0.0018,"collectCycle":8,
 "nextSettleTime":1789920000000,"timestamp":1789897303485,
 "idxPrice":80564.4,"fairPrice":80536.9}}
```

`collectCycle: 8` → **8-hour funding**, the same cadence as Binance / OKX / Bybit, so
MEXC joins the existing `funding_period_s => 28_800` arm and the
`strategy-vm` discrete-8h-print arm without new machinery. Caps ±0.18 %/period measured
on BTC. Funding is available three ways: REST history (worker lane), `sub.funding.rate`
(event-driven), and inline on `push.ticker` — §4 D4 picks one.
**Corrected 2026-09-23: `collectCycle` is PER SYMBOL — 8 h on BTC/ETH/SPY/AAPLSTOCK,
4 h on XAU_USDT, USOIL_USDT and EUR_USDT (REST `funding_rate/{SYM}`, 08:05Z). "8-hour funding" is wrong for part of the TradFi plane;
28 800 s is only the nominal fallback (§1.7, §4 D4).**

### 1.5 Fees

Fees are published **per symbol** by both REST surfaces (`makerCommission`/
`takerCommission` on spot rows; `makerFeeRate`/`takerFeeRate` on contract rows). That is
better than a doc but it is **still not a measured fill** — the standing law is
*"fees are MEASURED from `userFills.fee`, never read from a doc"*, and under O-MX1 there
are no fills. **`fees.toml` therefore takes the venue's published regular tier with a
dated provenance comment, flagged as UNVERIFIED, and the law is satisfied only if MEXC is
ever armed.** Observed: spot maker 0 / taker 5 bps (including xStocks); crypto perps
maker 0 / taker 2 bps; the `mc-trade-zone-0fees` plate (407 of the 431 TradFi contracts)
at **maker 0 / taker 0**; FX perps the dearest at maker 1 / taker 4 bps.

### 1.6 Latency — MEASURED, and NOT YET VALID FOR THE ENGINE

**Superseded 2026-09-23 by the Mac measurement (§1.7, `docs/venue-latency.md` §3) — the
gate this subsection names is met; the Cowork-VM numbers below are kept as the record.**

> **Read this before quoting any number below.** These were taken from the **Cowork Linux
> VM's egress**, which is not the engine's network stack. Under the standing law —
> *venue latency is measured per host AND per location* — they are an **upper bound and a
> shape, not the Δ**. `python -m claude_worker.latency_probe` must be run **on the Mac**
> and hand-transcribed into `docs/venue-latency.md` §3 before any value reaches
> `ModelParams::default()`. MX9 gates on that.

| measurement | spot (`api.mexc.com`) | futures (`contract.mexc.com`) |
|---|---|---|
| Steady-state REST RTT, kept-alive, median | 138.4 ms | 128.4 ms |
| … min (n = 15) | 117.4 ms | 119.8 ms |
| Clock offset venue−host (NTP min-RTT rule) | +56.8 ms | +58.7 ms |

Spot feed delay, `sendTime` → local receive, n = **29 669** over 30 symbols at `@10ms`,
corrected by the +56.8 ms offset: **p50 ≈ 59 ms · p90 ≈ 90 ms · p99 ≈ 217 ms · max ≈ 262 ms**.

The offset-independent part — which survives whatever the true asymmetry is — is the
**jitter**: p90−p50 ≈ 31 ms, p99−p50 ≈ 158 ms, max−p50 ≈ 203 ms.

Implied `Δ = feed p50 + RTT/2` ≈ **118 ms** (min RTT) to **128 ms** (median RTT) from this
network — a range, not a point, and the spread is the asymmetry uncertainty. For orientation only: the
existing applied vector is `[pm 200, bn 130, okx 130, deribit 220, hl 340, ai 0, bybit 60]`
(`docs/venue-latency.md:85`). A ~120 ms Δ would place MEXC at the Binance/OKX end — but
from a path with a **128 ms REST RTT**, which is 3× Bybit's 43.5 ms. **If the Mac
reproduces that RTT, MEXC is a research/capture venue and nothing latency-sensitive may
ever be built on it.** That is a finding, not a blocker, under O-MX1.

### 1.7 Corrections and additions — MEASURED LIVE 2026-09-23 (MX9, the Mac, operator's home network)

Taken by the MX9 live smoke (`crates/cli/tests/mexc_live_smoke.rs`) and the latency probe
on the engine's own host. Where one of these contradicts §1.1–§1.6, this wins.

**Spot wire.**

| frame | live text |
|---|---|
| success ack | `{"id":0,"code":0,"msg":"<param>,<param>"}` — a plain comma echo of the subscribed params |
| pong | `{"id":0,"code":0,"msg":"PONG"}` |
| refusal | `{"id":0,"code":0,"msg":"Not Subscribed successfully! [p1,p2].  Reason： Blocked! "}` — **`code` stays 0**, and an UNKNOWN symbol is also `Blocked!` |

So `Blocked!` does not distinguish a tier refusal from a symbol the venue does not list, and
a per-param refusal is recognised by its `msg` text, not by `code` (a non-zero `code` would
refuse the whole request). The §1.1 frame layout re-decoded exactly; the hex example's
`0A 34` is the `@100ms` channel length (above).

**Spot republishes an UNCHANGED BBO every 10 ms.** Over a 120 s window
`aggre.bookTicker@10ms` pushed ~11 936 times per symbol, 96.5–99.9 % of pushes
byte-identical to the previous one. Futures `depth.full` is 26–79 % BBO-identical (a deeper
level moved). → §4 D11: a tick is a BBO change.

**Futures wire.** Every frame is TEXT (compression is off when `compress` is omitted —
re-confirmed). Acks `{"channel":"rs.sub.<ch>","data":"success",…}`. An unknown contract
answers `{"channel":"rs.error","data":"Contract [NOPE_USDT] not exists",…}`; an unknown
method gets NO answer. `push.deal`'s `data` is an ARRAY of prints. `push.ticker` carries
`fairPrice`, `indexPrice`, `holdVol`, `timestamp` and `fundingRate`.

**Futures requires a CLIENT heartbeat however busy the feed.** The venue closes the socket
60 s after the last client `ping` even while it is streaming `depth.full`:
`rs.error "more than 60 seconds no response, close the channel"`. Caught by the live smoke;
a quiet-time-only keepalive never pings a busy connection. → §4 D12.

**`depth.full` cadence.** BTC_USDT gap p50 269 ms / p99 764 ms / max 1.1 s — the MX4 exit
(p99 < 1 s) is met on the liquid benchmark. Quiet perps (EUR_USDT, SPY_USDT,
AAPLSTOCK_USDT) go 4–5 s between pushes because the channel pushes on change, so the exit
is judged on BTC_USDT only. `push.ticker` (~2.75 s) stays capture-only.

**Funding cycle is per symbol.** `collectCycle` = 8 h on BTC/ETH/SPY/AAPLSTOCK and
**4 h on XAU_USDT, USOIL_USDT and EUR_USDT**. `funding_period_s(Mexc) = 28 800` is only the nominal
fallback; the per-symbol cycle rides the boot REST seed (Q-MX3). The worker's history lane
stores each settled print at its own `settleTime`, whatever the cycle.

**Discovery.** Spot `exchangeInfo` 1 954 rows (all live); perp `contract/detail` 1 185 rows
(1 134 live). All 12 Q-MX5 instruments live.

**Latency on the Mac** (`python -m claude_worker.latency_probe --minutes 10`, from ~07:24Z;
full table `docs/venue-latency.md` §3):

| class | REST edge | TCP / TLS ms | RTT p50 / p90 ms | offset venue−host | stream | feed p50 / p90 / p99 ms | n | Δ |
|---|---|---|---|---|---|---|---|---|
| spot | api.mexc.com | 8.3 / 21.0 | 131.4 / 164.3 | +108.3 ms | `aggre.bookTicker` | 81.9 / 94.4 / 338.0 | 60 024 | 147.6 → **150** |
| futures | contract.mexc.com | 14.2 / 22.7 | 134.2 / 232.0 | +105.1 ms | `depth.full` | 59.6 / 85.2 / 348.9 (cts 65.3 / 92.3 / 354.9) | 1 933 | 126.7 → 130 |

One venue byte carries both classes, so the slower binds: **Δ = 150 ms, applied** to
`ModelParams::default()` / `core_fill::ACTIVATION_NS_DEFAULT`. Stress (p90 feed + p90
RTT/2): spot 180, futures 210. The REST RTT (131–134 ms — 2.4× Bybit's 54.7 this run, 3×
its 43.5 on 2026-09-03) reproduces the Cowork-VM path: **R4 confirmed — MEXC is a
research/capture venue from this location; nothing latency-sensitive belongs on it.** The
feed p99 sets the stale default (§4 D8: 400 ms).

---

## §2 Integration thesis — what MEXC buys that the six current venues do not

Three things, in descending confidence.

**T1 — A second, independent source for the equity/TradFi plane.** BST put equities in the
engine through Binance alone (bStocks spot + `TRADIFI_PERPETUAL` perps). One venue is a
feed; two venues are a *pair*. MEXC brings 79 tokenized equities on spot and 431 TradFi
perps, so every cross-sectional and cross-venue construct the engine already runs —
`xsd`, the `xv` family, the carry surface — becomes *expressible* on equities, indices,
metals, energy and FX for the first time.

**T2 — An intra-venue spot-vs-perp basis on the same underlying, with no cross-venue leg.**
MEXC lists both `AAPLXUSDT` (spot xStock) and `AAPLSTOCK_USDT` (perp) — the same
underlying, one venue, one clock, one connection class each. Measured at 09:44Z Sunday,
with US equity markets closed:

| pair | spot mid | perp mid | spot rich by |
|---|---|---|---|
| AAPL | 334.805 | 334.085 | **+21.6 bps** |
| SPY | 766.875 | 762.285 | **+60.2 bps** |

Both legs were live, 1-tick-wide, with real 24 h volume, on a Sunday. The perp leg is
**maker 0 / taker 0**; the spot leg is maker 0 / taker 5 bps. A 60 bps dislocation against
a 5 bps round-trip is the kind of number that is almost always wrong for a reason — which
is exactly why it should be *captured and measured offline*, not traded. See R5.

**T3 — Funding on non-crypto underlyings.** An 8-hour funding print on gold, crude, EUR,
JPY and SPY does not exist anywhere else in the universe. The regime and carry lanes
consume funding as a first-class input; this widens that input's domain beyond crypto for
the first time.
(2026-09-23: the cycle is 4 h on gold and EUR, not 8 h (§1.7); and SPY is not new —
Binance's configured TradFi `SPY` perp already carries funding. Gold, crude and FX stand.)

### The caveat that governs all three

`indexOrigin` on `AAPLSTOCK_USDT` is `[BINANCE_FUTURE, BITGET_FUTURE, BINANCETICKER, PYTH,
KAIKO]`. **MEXC derives its equity-perp index partly from Binance's TradFi perp — which
this engine already ingests.** The MEXC↔Binance equity pair is therefore *index-linked*,
not independent: what looks like a basis is a lead/lag on a shared reference. The standing
`xv` verdict (2026-09-02) closed exactly this shape on `okx:BTC-USDT` vs `binance:btcusdt`
— a ~300 ms lead the post-only-at-mid executor could not capture — and the
`xv` family authoring law requires entry at ≥ ~4.5σ of the pair's deviation or the row
breaches caps. **Any MEXC pair row inherits those laws unread at its peril.** T1–T3 justify
*capture*; they justify nothing else until a vault study says otherwise.

---

## §3 Workstreams MX0–MX9

LOC figures are estimates derived from the Bybit crate's actual sizes (lib 587 +
run_loop 1121 + discovery 395 non-test, 930 test) and from a file-by-file sweep of every
venue-shaped surface in the tree. **Every gate in CLAUDE.md "Build / test / run" must be
green at each phase boundary; a red gate stops the lane.**

### MX0 — Live probes (DONE — this document is the output)

No code. Everything in §1 was captured from live bodies on 2026-09-20. **Exit: met.**
The probe scripts are research one-shots and stay out of git (`/tmp` on the Mac, never
`claude-worker/tools_*.py` unless they are needed again).

### MX1 — `core-parse`: protobuf primitives (O-MX3)

The only genuinely new primitive in the plan. `core-parse` today has **no varint,
protobuf or LEB128 helper** (grepped `crates/` + `fuzz/`: zero hits), and **no ingress
parses a non-JSON payload** — MEXC spot is the first.

Add to `crates/core-parse/src/`:

```rust
pub fn scan_varint(buf: &[u8], pos: Pos) -> Option<(u64, Pos)>;      // ≤10 bytes, overflow-safe
pub fn scan_pb_tag(buf: &[u8], pos: Pos) -> Option<(u32, u8, Pos)>;  // (field_no, wire_type, pos)
pub fn scan_pb_len(buf: &[u8], pos: Pos) -> Option<(Pos, Pos)>;      // (start, end) of a len-delimited payload
pub fn skip_pb_field(buf: &[u8], wire_type: u8, pos: Pos) -> Option<Pos>;
```

Laws: no allocation; no bounds-check panics (every accessor returns `Option`); a malformed
varint returns `None` rather than looping; `SKIP_PB_MAX_DEPTH` mirrors the existing
`SKIP_VALUE_MAX_DEPTH = 16`. Wire types 0 (varint), 2 (len-delimited) and 5/1 (fixed) are
handled; groups (3/4, deprecated) are rejected outright.

- **Files:** `crates/core-parse/src/lib.rs` (+ module), `fuzz/fuzz_targets/pb_scan.rs`, `fuzz/Cargo.toml`.
- **LOC:** ~180 src + ~220 test/proptest + ~30 fuzz.
- **Exit:** proptest `scan_varint` never panics on arbitrary bytes and round-trips a
  reference encoder; `cargo +nightly fuzz run pb_scan -- -max_total_time=300` clean;
  zero-alloc assertion added in MX8.

### MX2 — Venue identity + the lane widening sweep

The mechanical half, and the dangerous half — **the compiler catches the `match VenueId`
sites and catches nothing else.**

Compiler-caught (safe): `core-types::VenueId` + `from_u8` + `default_stale_after_ms` +
`funding_period_s` (`crates/core-types/src/lib.rs:55,80,102,3112`);
`engine::tick_lane_of` → `Some(6)`, `NUM_TICK_LANES 6 → 7`, and `Mexc => None` in
`depth_lane_of` / `opt_lane_of` / `fill_lane_of` (`crates/engine/src/lib.rs:81,88,118,141,169`
— verified: `NUM_TICK_LANES: usize = 6`, `VenueId::Bybit => Some(5)`);
`core-config::exec::VENUE_NAMES` + both error strings;
`strategy-vm::features.rs:427` (place MEXC in the 8h discrete-print arm, deliberately).

**NOT compiler-caught — the hand-sweep list** (every fixed array sized by venue count):

| file:line | item |
|---|---|
| `core-types/src/lib.rs:117` | `stale_after_ms_defaults() -> [u32; 7]` → `8` (verified) |
| `core-fill/src/lib.rs:89` | `ACTIVATION_NS_DEFAULT: [u64; 7]` → `8` — **measured Δ only (MX9)** |
| `engine-snapshot/src/snapshot.rs:31,60,595` | `SNAPSHOT_VENUES = 7` → `8`, `VENUE_NAMES` |
| `cli/src/backtest.rs:110,121,337–385` | `VENUE_LABELS [7]`, `MODEL_VENUE_LABELS [6]`, and all six `ModelParams` tables |
| `cli/src/backtest/fill.rs:111,116` | `TRADEABLE_VENUES`, `tradeable_venue_byte` (+ ~18 test fixtures) |
| `cli/src/audit_replay.rs:40`, `audit_pnl.rs:483,1003` | `VENUE_LABELS`, `stale_after_ms` |
| `cli/src/paper.rs:2346,3362,3589,3818,6571,6578` | `seen [&str;7]`, `ingress_last_tick_age [GaugeId;7]`, `parse_stale_after_ms -> [u32;7]`, counter snapshots |

`exec-router::EXEC_VENUES = 8` already covers byte 7 exactly (verified) — **venue 8 would
need it raised; this one does not.** `crates/tui` iterates `VENUE_NAMES.len()` — no change.
`core-io` PMLR needs **no version bump**: `VERSION = 3` bumps only on a slot-layout change,
and a new `VenueId` is not one. `instrument-manifest.tsv` stays two columns by law.

- **LOC:** ~130 across ~14 files.
- **Exit:** `cargo nextest run --workspace` green with the new venue present but unconfigured;
  `cargo build --release -p cli` succeeds; a boot with no `[mexc]` section is **bit-for-bit
  the pre-MX2 boot** (the Bybit opt-in precedent).

### MX3 — `crates/ingress-mexc`: the SPOT arm

New crate. SPDX two-liner on every file; `license.workspace = true`; deps
`core-{types,parse,net,ring,metrics,time}` + `memchr` + `mio`; dev-dep `proptest`.

- `src/lib.rs` — `classify()` (opcode + first-tag dispatch), `MexcChannel`,
  `MexcSymbolTable`, the PB walkers `parse_aggre_book_ticker()` / `parse_aggre_deals()`,
  `write_subscribe()` (renders into stack scratch), `parse_sub_ack()` (the per-param echo).
- `src/run_loop.rs` — `Driver` / `State` / `RunResult` / `MexcConn` / `run_multi` /
  `drive_one` / `note_transport_ready`, mirroring `ingress-bybit/src/run_loop.rs` beat for
  beat: `RX_BUF_SIZE 256 KiB`, `TX_BUF_SIZE 16 KiB`, `TICK_RING_CAP` (**named
  `TICK_RING_CAP`, not `DEFAULT_…` — so `paper.rs`'s const-assert block picks it up, which
  bybit's naming quietly escaped**), slot-index-as-`mio::Token`, 50 ms poll, the six raw-index
  passes, `establishment_expired` budget, `Backoff`, `Keepalive`.
- **LOC:** ~750 src + ~420 test.
- **Exit:** every parser has a `never_panics` proptest; `classify` + both parsers are
  0 B/op under `AllocGuard`; a golden frame captured in MX0 decodes to the exact expected
  `Tick` and `ChannelEvent`.

### MX4 — `crates/ingress-mexc`: the FUTURES arm

Same thread, same producer, same `Driver` shape, different host/path and a **JSON** scanner
— the Bybit spot/linear two-connection-class pattern exactly, with per-connection symbol
tables (`BTCUSDT` spot and `BTC_USDT` perp are different instruments sharing one venue).

- `parse_depth_full()` → BBO `Tick` from `asks[0]`/`bids[0]`, `version` → `venue_seq`,
  `cts` → `venue_time_ms`.
- `parse_deal()` → `ChannelId::Trade`, `i` → `venue_seq`, `T` → the cross-venue sign.
- `parse_ticker()` → `Mark` + `Funding` + `Ticker` capture events (§4 D4).
- Ack handling: `rs.sub.*` carries **no symbol**, so subscription confirmation is
  *first-data-per-symbol within the establishment budget*, never an echo match (§4 D7).
- **LOC:** ~620 src + ~380 test.
- **Exit:** as MX3, plus a cadence assertion in the soak (MX9) that `depth.full` beats
  1 s p99 — if it does not, the futures tick source is wrong and the phase reopens.
  (2026-09-23: judged on BTC_USDT only — `depth.full` pushes on change and quiet perps go
  4–5 s between pushes; BTC p99 764 ms, met. §1.7.)

### MX5 — Discovery + universe config

`src/discovery.rs` — boot REST against `GET /api/v3/exchangeInfo` (spot: `status == "1"`,
`permissions` contains `SPOT`, record `baseAssetPrecision`/`quotePrecision`) and
`GET /api/v1/contract/detail` (perp: `state == 0`, `apiAllowed == true`, record
`contractSize`/`priceUnit`/`volUnit`). **No equity-specific branch** (§1.3). A configured
symbol the venue does not list is a fatal boot misconfiguration (the Deribit-combo
precedent). Note `exchangeInfo` is **1.6 MB**; parse it streaming out of the rx buffer,
never buffer-then-scan.
(2026-09-23, as built: a discovery fetch or parse failure is fatal; a configured symbol the
venue does not list — or lists as not live — sets the shared `any_missing` flag, which
refuses only a legacy `--live` boot and WARNs on a paper / `--exec` boot, exactly as for
every other venue. The boot also fetches one `funding_rate/{SYM}` per live configured perp,
paced 110 ms for the 20 req / 2 s limit — the Q-MX3 seed.)

`crates/core-config/src/universe.rs` — `Section::Mexc`, the dispatch arm (unknown section
stays fatal), `Slot::MexcSpot`/`MexcPerp`, `ElemKind::MexcSymbol` + two validators (spot
`[A-Z0-9]`, perp `[A-Z0-9_]`), builder fields, `store_array` arms, `check_cap`,
`check_unique`, `Universe`/`Allocated` fields, and the allocation loop with
`MEXC_PERP_ORDINAL_BASE: u32 = 512` (the Binance spot/usdm split, reused verbatim —
`VENUE_LIST_MAX = 500 < 512` keeps the blocks disjoint by construction, verified).

```toml
[mexc]
# MX5. Spot symbols are UPPERCASE, no separator (BTCUSDT, AAPLXUSDT);
# tokenized xStocks equities are ORDINARY spot rows — no separate key.
# spot[i] -> make_symbol_id(Mexc, i+1); perp ordinals from base 512.
# Spot rides ONE connection per 30 subscriptions (venue cap, measured);
# 2 channels per symbol => 15 symbols per spot connection.
spot = []   # e.g. ["BTCUSDT", "ETHUSDT", "AAPLXUSDT", "SPYXUSDT"]

# Perp symbols are BASE_QUOTE (BTC_USDT, XAU_USDT, AAPLSTOCK_USDT).
# TradFi/commodity/FX/index perps are ORDINARY rows — no separate key.
# BBO comes from depth.full(limit 5) at ~270 ms, NOT push.ticker (~2.75 s).
perp = []   # e.g. ["BTC_USDT", "XAU_USDT", "USOIL_USDT", "EUR_USDT", "SPY_USDT"]
```

Descriptors: `mexc:<SYM>` (Spot) and `mexc-perp:<SYM>` (Perp) — mirrored in
`core-config/src/instrument_class.rs`, `claude_worker/instrument_class.py`, and the shared
fixture `claude-worker/tests/fixtures/fees/descriptor-classes.tsv`.
`.env.example` + `core-config/src/lib.rs`: `MEXC_WS_HOST` (default `wbs-api.mexc.com`),
`MEXC_FUT_WS_HOST` (`contract.mexc.com`), `MEXC_REST_HOST` (`api.mexc.com`),
`MEXC_FUT_REST_HOST` (`contract.mexc.com`) — four, because MEXC splits spot and futures
across different hosts on both planes.

- **LOC:** ~330 src (crate + config) + ~200 test.

### MX6 — Boot wiring, metrics, capture

`crates/cli/src/paper.rs` — `MexcConnSpec`, `spawn_mexc` (a near-verbatim `spawn_bybit`),
`MEXC_KEEPALIVE`, `PmlrCapture::open(run_dir, "mexc", …)` + `set_tap_venue_byte(…, 7)`,
`log_pin_outcome`, thread name `ingress-mexc-x<N>`, `--raw-tap` label arm.
`crates/cli/src/bin/multivenue-engine.rs` — `rings.tick[6]`/`event[6]` split, consumer
arrays, discovery arg, the spawn block, and the `drop(prod)` else-branch (the §3.3
unspawned-venue shape).

Metrics: 12 counters + 6 gauges, the per-venue family (`register_ingress_counters`,
`register_capture_gauges`, `register_coverage_gauge`, `_state`, `_last_tick_age_seconds`).
Headroom is fine — `MAX_COUNTERS = 512`, `MAX_GAUGES = 384` (verified), against roughly
253 / 248 in use. `Observability::build` panics at every boot test if that is wrong, so it
is self-guarding.

Capture: the uniform file set under the existing container version —
`mexc-ticks.pmlr` (kind 0), `-events.pmlr` (5), `-signals.pmlr` / `-opt-summary.pmlr` /
`-depth.pmlr` (header-only).

- **LOC:** ~430 across `paper.rs` + `multivenue-engine.rs` + `cli/Cargo.toml` + root `Cargo.toml`.

### MX7 — Worker lanes

| module | change |
|---|---|
| `frames.py` | `VENUE_MEXC: int = 7` |
| `candles.py` | host consts, **2 lanes** (`mexc`, `mexc-perp`), interval maps (spot `/api/v3/klines` vs contract `/api/v1/contract/kline`), page size, 1d floor, `parse_mexc_kline`, `_mexc_url`, dispatch arm, `budget_key` (**two hosts, two budgets** — unlike Bybit's shared `api.bybit.com`), `hosts` entry |
| `funding.py` | selection arm (`[mexc] perp` → all rows are perps), REST arm, `parse_mexc_funding`, docstring law |
| `refdata.py` | `parse_mexc_ticker` → (turnover24h, openInterest) from `holdVol`/`amount24` |
| `fetchers.py` | `MEXC_PERP_ORDINAL_BASE = 512`, two `_propose_list` seedings |
| `latency_probe.py` | `parse_mexc` (PB-aware — **the first binary parser in the probe**), `_t_mexc`, one `VenueSpec` row |
| `channel_map.py` | `mexc` → `CAP_PRICE`; `mexc-perp` → `CAP_PRICE \| CAP_FUNDING` (**lockstep with `ingress-ai/src/ruleset.rs` — same commit, by that module's own law**) |
| `instrument_class.py` | two arms + the shared TSV fixture |
| `pnl_report.py` | `FEE_VENUES += ("mexc",)` — an unknown venue in `fees.toml` is fatal |
| `bartest.py`, `seeds.py`, `cli.py`, `labeling.py`, `dashboard/dashboard.html:525` | one line each |

`universe_refresh.py`, `coverage_audit.py` (joins by descriptor), `regime.py`,
`window_root.py` (globs `*.pmlr`), `archive.py`/`objstore.py`, `config.py` need **nothing**
— verified by grep, not assumed.

- **LOC:** ~280 src + ~185 test. **Serialize every worker run behind the
  `pgrep -f 'claude[-_]worke[r]'` / `pgrep -f pytes[t]` guard** — one SQLite seq namespace,
  one writer.

### MX8 — Gates

- `cargo nextest run --workspace` — the 2585 baseline plus the new tests.
- **`cargo test -p bench --test alloc_assertions --release -- --test-threads=1` at 0 B/op**,
  with `mexc_parsers_are_zero_alloc` + `mexc_run_loop_steady_state_is_zero_alloc`. **Note:
  Bybit shipped with NEITHER, and with no `tests/<venue>_tls_loopback.rs` — it is the only
  ingress without one. This plan does not inherit that gap: MEXC gets both, plus
  `tests/mexc_tls_loopback.rs` (~250 LOC).** False-green guard: the log must show a fresh
  `Compiling bench`, else `cargo clean -p bench --release`.
- `make lint` (clippy `-D warnings`), `make license-check`, `make license-deps` **only if a
  dependency changed** (this plan adds none).
- **`make copy-audit` will NOT see the new crate** — `scripts/copy-audit.sh` defaults to
  `exec-router exec-hyperliquid signer-eip712 clob-dispatcher core-net` (verified at
  `scripts/copy-audit.sh:58-60`). Run `scripts/copy-audit.sh crates/ingress-mexc
  crates/core-parse` **explicitly** and run the `zero-copy-auditor` agent over both. Only
  the operator may grow the baseline.
- `cargo +nightly fuzz run mexc_ws_frame -- -max_total_time=300`, likewise
  `mexc_instruments` and `pb_scan`.
- G0: `cargo build --release -p cli` before any live boot; check
  `stat -f '%Sm' target/release/multivenue-engine` against the last `crates/cli` change.

### MX9 — Live smoke, soak, measured Δ, docs close

**As executed 2026-09-23:** steps 1–2 were REPLACED by ruling MX9-EXEC — the engine is live
on mainnet and was never stopped; the standalone smoke runs the production path (boot
discovery + funding seeds, then `cli::spawn_mexc` with its PMLR capture and raw tap) on the
Q-MX5 universe for a bounded window (`MEXC_SMOKE_SECS`, default 90, capped 900):
`CARGO_TARGET_DIR=/tmp/mx9-target cargo test --release -p cli --test mexc_live_smoke --
--ignored --nocapture`. It caught two live facts no fixture had (§1.7 → D11, D12). Step 3's
soak is that bounded window, not an engine capture: MEXC's first engine capture is the first
boot after `[mexc]` is configured. Steps 4–6 are done (Δ 150 ms applied, `venue-latency.md`
§3; docs; the `capture_catalog` per-day `venue_ticks` array now carries every venue).

1. **ONE ENGINE EVER**: stop the launchd instance via `launchctl`, boot with `--raw-tap`,
   restart it after. Verify `vm_rows_active ≥ 1` on `/state` after the restart.
2. `--raw-tap` smoke on a small universe: 4 crypto spot + 2 xStocks + 4 crypto perps +
   4 TradFi perps. Confirm the PB tag bytes against live frames (pitfall 7 — venue wire
   drift is only ever caught live).
3. **Bounded soak within the ≤ 2 h law** — a ≤ 2 h `ts_ns` window cut into a bounded
   symlink root, events file cut too; more data ⇒ pool DISJOINT windows, never schedule a
   wait. Gates stated in **fills/ticks/windows, never hours**.
4. `python -m claude_worker.latency_probe` **on the Mac** → `docs/venue-latency.md` §3 +
   §5 rows → derive Δ → apply to `ModelParams::default()` and
   `core-fill::ACTIVATION_NS_DEFAULT` with the date, host and network in the comment.
   **This is the gate on §1.6; nothing before it may use a latency number.**
5. Docs: `docs/wire-format.md` (**and fix the stale `VenueId` line at :23-26, which still
   lists only six venues — Bybit was never added**), `docs/migration.md` (one atomic entry
   in the WS9 shape at `:3913-3954`), `docs/research-universe.md`, `docs/ai-strategy-pipeline.md`,
   `CLAUDE.md` CURRENT STATE, `README.md`, `PLAN.md`, `AGENTS.md`, the four `*.toml.example`s.
6. **Fix the pre-existing `capture_catalog.rs` off-by-one** — its per-day `venue_ticks`
   array iterates 6 of a 7-wide venue list and silently omits Bybit (documented at
   `claude_worker/data_source.py:103-104`). A seventh venue makes the omission wider; fix
   it here or the MEXC capture is invisible to the catalog.

---

## §4 Design decisions

**D1 — One thread, N connections.** Single-writer law: one `ingress-mexc` thread, one tick
producer, one event producer, `mio::Poll` with slot-index-as-token. Connection count is
forced by the venue: spot caps at **30 subscriptions**, and 2 channels/symbol ⇒ **15
symbols per spot connection**; futures takes **≥ 40** subs and needs 2–3 per symbol
(`depth.full` + `deal`, plus `ticker` on the funding-bearing subset) ⇒ ~13–20 symbols per
futures connection. A 40-instrument universe is ~5 sockets. Compare Binance's 254.
*As built (Q-MX2): `ticker` is on every perp, so futures is 3 channels × 13 symbols = 39
subscriptions per connection (`MEXC_FUT_SYMBOLS_PER_CONN`); spot 2 × 15 = 30
(`MEXC_SPOT_SYMBOLS_PER_CONN`). The Q-MX5 universe is 2 sockets.*

**D2 — BBO source per class, chosen on measurement.** Spot:
`aggre.bookTicker@10ms`. Futures: **`depth.full` limit 5 at ~270 ms**, *not* `push.ticker`
at ~2.75 s. This is the single most consequential decision in the plan and it is backed by
the cadence table in §1.2.

**D3 — `venue_seq` and the §6.2 chain law APPLY to this venue** — unlike Bybit. Spot
`bookTicker.version` and futures `depth.full.version` are monotonic; futures `deal.i` is a
numeric trade id. So MEXC emits `TradeGap` (9) and `BookGap` (10) 1:1 with `gaps_total`,
and gets a §6.2 chain-monitor row. **Spot deals carry `tradeId` as a string
(`"730292425431437318X0_…X0"`) that is NOT a plain integer** — it must be classified
(parse the leading digits, or declare `venue_seq = 0` for spot trades and say so in
`wire-format.md`, the Bybit-style opt-out). **Open question Q-MX1.**

**Amended 2026-09-23 (ruling Q-MX1) — the chain law does NOT apply; `venue_seq` is carried
and regressions are counted.** Carried: spot book `version` (ASCII digits, body field 5),
spot trade = the LEADING DIGITS of `tradeId` (`trade_id_seq`: 0 when there is no leading
digit or the run overflows `u64`), futures book `version`, futures deal `i`.
`Tick.venue_seq` = `v & 0xFFFF_FFFF` (the Bybit truncation law); `ChannelEvent.venue_seq` =
the full `u64`. These are sampled/snapshot streams that skip versions by design, so
**no `TradeGap`/`BookGap` is ever emitted and `gaps_total` stays 0**. The driver instead
counts a REGRESSION — a value strictly below the last-seen full-width value of the same
symbol × stream (book / trade) — on `engine_ingress_mexc_seq_regressions_total` (the family
is registered for every venue; only MEXC increments it). 0 = absent, never counts and never
overwrites; the last-seen values are process-lifetime, so a lagging node behind a reconnect
is still caught; a trade push carrying several prints is judged by its LARGEST id.

**D4 — Channel/event mapping.** The per-venue law to be written into
`docs/wire-format.md:479-486` alongside the `mexc` label:

| source | emits | v0 | v1 |
|---|---|---|---|
| spot `aggre.bookTicker` | `Tick` (no ChannelId) | bid/ask ×1e6 | — |
| spot `aggre.deals` | `Trade` (0) | px ×1e6 | qty ×1e6, **negated when the aggressor SOLD** (`tradeType == 2`) |
| perp `depth.full` | `Tick` | bid/ask ×1e6 | — |
| perp `deal` | `Trade` (0) | px ×1e6 | qty ×1e6, negated on `T == 2` |
| perp `ticker` | `Mark` (2) | `fairPrice` ×1e6 | `indexPrice` ×1e6 (**the Binance `@markPrice` shape**) |
| perp `ticker` | `Funding` (3) | rate ×1e9 | next-funding ms (**the OKX/Binance shape**) |
| perp `ticker` | `Ticker` (4) | 0 | `holdVol` ×1e6, venue contract units (**the Bybit shape**) |
| either, on a failed sub | `SubDrop` (11) | venue error code (0 = missing-from-echo) | venue-local channel discriminant |

Funding rides `push.ticker` (already subscribed, already parsed) rather than a separate
`sub.funding.rate` — one channel, one parser, no extra subscription against the cap.
`nextSettleTime` is **not on `push.ticker`**, so `v1` is seeded from the REST
`funding_rate` endpoint at boot and advanced by `+28 800 s` per settlement; the worker
funding lane is the authority for history. **Open question Q-MX3.**

**Amended 2026-09-23 (as built).** (1) Funding `v1` (ruling Q-MX3): each perp is seeded at
boot from `funding_rate/{SYM}` (`nextSettleTime`, `collectCycle`) and advanced by THAT
symbol's `collectCycle × 1 h` — not a fixed 28 800 s, because the cycle is 4 h on XAU and
EUR (§1.7) — exactly when an event's venue time REACHES the latched instant
(`funding_next_settle_ms`, arithmetic, never a loop), so the vm's settled-print law sees
`v1` move when a period settles. Unseeded or `collectCycle == 0` ⇒ `v1 = 0`. (2) `SubDrop`:
MEXC names no numeric code, so `v0` = 1 (`SUB_DROP_REFUSED`) for a param listed in the spot
`Not Subscribed` echo or a futures `rs.sub.*` non-success / `rs.error`, and the spot ack's
own non-zero `code` for a whole-request refusal; `v1` = the venue-local `MexcChannel`
discriminant (0 spot bookTicker · 1 spot deals · 2 futures depth.full · 3 futures deal ·
4 futures ticker · −1 unknown). (3) Futures `Tick` quantities and `Trade.v1` are in venue
CONTRACTS ×1e6 (multiply by `contractSize` for base units); spot is base units. (4) Venue
time: spot `createTime` else `sendTime`; futures `cts` else `ts`; a print its own `t`/`time`
else the push's stamp. (5) A `Tick` is a BBO change (D11).

**D5 — The PB scanner is an order-agnostic forward walk.** proto3 permits any field order
and omits defaults (observed: `createTime` absent). The scanner reads
`tag → wire_type → dispatch`, keeps a small `Option<Span>` per field of interest, skips
everything else via `skip_pb_field`, and materialises the `Tick` only at end-of-message.
It must never assume the body tag comes last, never assume `symbol` precedes the body, and
must tolerate unknown field numbers (forward compatibility — MEXC has already replaced
this WS once).

**D6 — No new `InstrumentClass`.** xStocks are `Spot`; TradFi/equity/FX/metal perps are
`Perp`. This is the BST ruling applied unchanged, and it keeps the fee table's second
index at `INSTRUMENT_CLASSES = 5`.

**D7 — Ack semantics differ by class, and neither is Bybit's.** Spot returns a **per-param
echo** inside one text ack (better than Bybit's whole-request ack) — the WS2 per-arg drop
machinery works at param granularity, and `Blocked!` is a distinguishable reason code.
Futures returns `rs.sub.<method>: "success"` with **no symbol at all** — strictly worse, so
futures confirmation is *first-data-per-symbol inside the establishment budget*, and the
`establishment_expired` reaper handles the empty-session case. A failed ack **at boot**
(nothing ever confirmed on the driver) is fatal; on reconnect it is a non-fatal drop with a
`SubDrop` event.

**Amended 2026-09-23 (live ack text, §1.7).** The spot success ack is a plain comma echo
(`"msg":"<param>,<param>"`), not `Subscribed successful! […]`; a refusal keeps `code: 0`
and lists the failed params in `Not Subscribed successfully! [p1,p2]` followed by
`Reason： …` (a FULL-WIDTH colon), and an unknown symbol is also `Blocked!`. So a spot pair
is confirmed when it is requested and NOT listed as failed (or by its first data), and
`code != 0` refuses the whole request. Futures: an unknown contract answers `rs.error`
(`"Contract [X] not exists"`) — a refusal NAMING the contract; an unknown method answers
nothing; any other `rs.error` (the 60 s heartbeat notice, D12) refuses nothing and is a
session error on the exit line, not a drop.

**Amended 2026-09-23 (post-review): a refused symbol never blinds its socket (R10).** The
first cut made ANY refusal on a never-confirmed driver fatal, so one delisted `[mexc]` row
refused in a boot ack took its 14 socket neighbours dark on every reconnect. As built: the
spot pairs an ack confirms are confirmed FIRST, and its refused params are per-param drops
unless the ack refuses EVERY pair of a never-confirmed driver (the only spot boot
fail-fast); a futures `rs.error` naming a contract is a per-symbol `SubDrop` and never
fatal (the establishment budget reaps a session that confirms nothing); a non-`success`
`rs.sub.*` (names no symbol; never observed live) keeps the never-confirmed fail-fast.
"Never confirmed" is a process-lifetime flag, not cleared by a reconnect.

**D8 — `default_stale_after_ms`.** The venue byte indexes ONE table, but the two classes
have very different cadences (spot ~10 ms; futures ~270 ms median / 695 ms max). A single
value must cover the slower class: **provisionally 1 500 ms**, revised from the MX9
measurement. If that proves too coarse for spot, the honest fix is the existing operator
override `--stale-after-ms mexc:<ms>`, not a per-class table (which no venue has today).

**Amended 2026-09-23: `default_stale_after_ms(Mexc)` = 400 ms** — the measured feed-delay
p99 (spot `aggre.bookTicker` 338 ms, futures `depth.full` 349 ms) rounded up, the per-venue
doctrine every other venue follows. The provisional 1 500 sized the threshold to the futures
push CADENCE, but the `FeedClock` judges feed DELAY; a quiet book simply pushes nothing.
Override unchanged: `--stale-after-ms mexc:<ms>` (engine, `backtest`, `audit-pnl`).

**D9 — Ordinal blocks.** Spot `make_symbol_id(Mexc, i+1)`; perp
`make_symbol_id(Mexc, MEXC_PERP_ORDINAL_BASE + j + 1)` with base 512. 24-bit ordinal width,
`VENUE_LIST_MAX = 500` keeps blocks disjoint (verified). Append-never-reorder: `SymbolId`s
are file-order ordinals, and the worker map keeps the OLD sym for a reordered name **by
design** — reordering `[mexc]` silently corrupts history.

**D10 — The "exec-ready" shape (O-MX1), and what it does NOT mean.** Reserve
`("mexc", 7)` in `core-config/src/exec.rs::VENUE_NAMES` so `exec.toml` can *name* the venue;
keep `fill_lane_of(Mexc) => None` and add no `ExecMode` arm, no dispatcher, no signer.
`EXEC_VENUES = 8` already covers byte 7. **Nothing in this plan can place an order**, and
arming MEXC would require its own plan, its own E-law record and its own operator ruling.

**D11 — A tick is a BBO CHANGE (added 2026-09-23, measured live).** MEXC republishes
unchanged quotes (spot every 10 ms, 96.5–99.9 % identical; futures `depth.full` 26–79 %
BBO-identical — §1.7). The house doctrine is tick = BBO change (Binance `bookTicker`, Bybit
`orderbook.1`, OKX `bbo-tbt` push on change), so the driver drops a push whose
`(bid px, bid qty, ask px, ask qty)` equals the last one EMITTED for that symbol. The
identical push is still a msg (`msgs_total`), still teaches the feed clock its stamp and
still feeds the seq-regression check (D3); it takes no ring slot and writes no capture row.
Emitted regardless: the first quote of every session (after each reconnect), an unchanged
quote whose VT2 stale verdict FLIPPED (the vm mirrors the latest tick's flag), and one whose
previous emission the ring dropped.

**D12 — Client heartbeat on a fixed cadence (added 2026-09-23, measured live).** MEXC
futures closes a socket 60 s after the last CLIENT `ping` however busy the feed (§1.7).
`core_net::Keepalive::poll_client_heartbeat` makes the ping due `ping_interval` after the
last ping SENT (before the first, after the session's connect instant — never after the
last inbound byte, which a busy feed keeps at "now"), whatever arrives inbound; the
reconnect rule is unchanged (no inbound byte for `idle_timeout`). MEXC uses it on BOTH
classes; `MEXC_KEEPALIVE` = ping 15 s / idle 40 s, probes `{"method":"PING"}` (spot) and
`{"method":"ping"}` (futures). Every other venue keeps `Keepalive::poll` (quiet-time pings).

---

## §5 Zero-copy / zero-allocation compliance

The zero-copy ruling (2026-09-19) requires that everything that CAN be done zero-copy IS,
and that every unavoidable copy carries a `// COPY: <what> <bound> — <why unavoidable> —
<alternative rejected>` within the eight lines above it.

**Zero-copy by construction:** `ws_read_frame` returns a `PayloadSpan` (a byte range, not a
slice) into the rx buffer; the PB walker returns `(start, end)` spans; price/quantity
strings are scanned **in place** by `scan_price_1e6` straight off the rx bytes; the
subscribe payload renders into stack scratch and is masked into the tx `IoBuf` in place;
the symbol table maps a borrowed `&[u8]` to a `SymbolId` with no owned `String`.

**Designed copies, each needing its `// COPY:` justification** — all five are pre-existing
sanctioned classes, so the plan adds **no new copy category**:

| copy | bound | why unavoidable |
|---|---|---|
| kernel → user (`transport.read`) | rx buffer size | syscall boundary |
| rustls plaintext window | TLS record | the TLS library's own buffer |
| rx-tail compaction (`IoBuf::copy_within`) | ≤ one frame | a partial frame must survive the next read |
| WS unmask in place | payload len | in-place XOR, no second buffer |
| ring-slot publish | 64 B `Tick` / `ChannelEvent` POD | the POD moves once into its slot by value |

**Zero-alloc:** the `Driver` preallocates rx (256 KiB), tx (16 KiB), the per-symbol BBO
array and the symbol table at boot. `host`/`path` `Vec<u8>`s are boot-time and sanctioned.
Steady state must show **0 B/op** under `AllocGuard`. No `Vec::push`, `format!`,
`to_string`, `Box::new`, `collect`, no iterators in hot loops (raw indices +
`get_unchecked` inside safe wrappers with `// SAFETY:`), no `dyn Trait`, no `tokio` /
`serde_json` / `reqwest` (the `.claude/hooks/no-forbidden-crates.sh` hook blocks the edit),
`debug_assert!` not `panic!`, fail-fast on the trading path.

---

## §6 Risks

| id | risk | mitigation |
|---|---|---|
| **R1** | **PB is a moving target.** MEXC already replaced its JSON WS with this one; a field renumber breaks the scanner silently (a missing field reads as absent, not as an error). | Order-agnostic walk (D5); a boot assertion that the first N frames per channel yield a complete `Tick`; `parse_errors_total` alerting; `--raw-tap` on every new deploy. |
| **R2** | **30-sub cap × 24 h connection life** ⇒ forced daily reconnect churn and a hard ceiling of 15 spot symbols per socket. | Connections are cheap here (~5 sockets for 40 instruments); the daily-restart lane already rebuilds the boot universe; `Backoff` + `establishment_expired` already handle reconnects. |
| **R3** | **Spot incremental depth is `Blocked!`** — BBO only, forever, on the public tier. | Matches the engine's BBO-tick doctrine; no book is built on any venue except where explicitly configured. Document it so no one re-discovers it. |
| **R4** | **Latency.** 128 ms REST RTT and a ~123 ms implied Δ **from the Cowork VM**. If the Mac reproduces it, MEXC is 3× Bybit's path. | MX9 measures on the Mac before any Δ is applied. Under O-MX1 this changes nothing that is built — it changes what may ever be built on top. **CONFIRMED 2026-09-23 on the Mac: RTT 131–134 ms, Δ 150 ms — research/capture venue only (§1.7).** |
| **R5** | **The equity basis (§2 T2) is probably not an edge.** `indexOrigin` shows MEXC's equity index partly derives from Binance's TradFi perp; the xv verdict closed exactly this shape; weekend equity prints are thin and drifty (the BST §5 flag); a 60 bps gap against 5 bps fees invites a size or borrow constraint that is not in the quote. | Capture only. Any study goes to the vault under the `xv` authoring laws (≥ ~4.5σ entry) and the size-comparison traps. **Nothing in this plan trades it.** |
| **R6** | **`capture_catalog.rs` already omits venue 6**; venue 7 widens a live blind spot. | Fixed in MX9 §6 as part of the lane, not deferred. **Fixed 2026-09-23** — the per-day `venue_ticks` array carries every venue. |
| **R7** | fd budget — a launchd agent is born with a 256-fd soft limit. | The wrapper already raises it to 8192; ~5 more sockets is noise against Binance's 254. |
| **R8** | `docs/wire-format.md:23-26` still lists six venues (Bybit was never added), so the doc is already wrong before MEXC arrives. | Fixed in MX9 §5. **Fixed 2026-09-23.** |
| **R9** | **Symbol-name collision across venues.** `BTCUSDT` now exists on Binance spot, Bybit spot AND MEXC spot; `BTC_USDT` on MEXC perp. | `SymbolId` namespaces by venue byte; descriptors namespace by prefix (`mexc:` / `mexc-perp:`); `check_unique` runs cross-venue at boot. Human-facing reports are the hazard — keep descriptors, never bare symbols. |
| **R10** | **MEXC lists and delists aggressively** (1 966 spot symbols, many thin); `state`/`apiAllowed` can flip between boots. | Boot discovery refuses a configured symbol the venue no longer lists (fail-fast). Prefer the liquid majors + the named equity/TradFi set; do not chase the long tail. **Corrected 2026-09-23 (as built):** the miss sets the shared `any_missing` flag — a legacy `--live` boot refuses, a paper / `--exec` boot WARNs and continues (every venue's contract); the row stays allocated and its subscription meets the venue's refusal (D7); the news lane (Q-MX7) watches the delistings section. A refused row is a per-symbol `SubDrop` and never blinds its socket's other symbols (D7, post-review amendment). |
| **R11** | Scope. This is ~4 000–5 000 LOC across ~60 files and four classes at once (O-MX2). | Phase boundaries are hard gates; MX2 lands a fully-green tree with the venue present but unconfigured, so the lane can pause at any boundary without a dirty tree. |

---

## §7 Non-goals — this plan changes NOTHING here

- **No execution.** No `exec-mexc`, no signer, no keys, no `ExecMode` arm, no risk-gate
  entry, no `LIVE_ARM_VENUES` change. The E7 mainnet lane (slot 3, bin15, Hyperliquid) is
  untouched.
- **No new strategy, no new slot.** Slot 7 stays open. No `xsd`/`xv`/carry row is authored
  here; §2 is a thesis for *capture*, and every measured study belongs in the vault.
- **No Stage-3 movement.** No `serve`, no Anthropic API calls, no AI promotion path.
- **No new `InstrumentClass`**, no PMLR version bump, no change to the frozen worker
  contract (`backtest.py` argv, schema-1 JSON, the 8 verbs, the 202 pytest pin).
- **No paid API, no cloud service on the trading path**, no observability stack.
- **No git operations** — no branch, no commit, no push, no `git add -A`. Staging is
  explicit-path only and the operator's call. Git write-ops run on the Mac in the RustRover
  terminal, never through the Cowork mount (stale `.git` locks).
- **No research material in git.** Measured product facts and wire shapes belong in this
  plan (the BST §1 precedent); any P&L, backtest or strategy study belongs in
  `docs/research/` or the vault.

---

## §8 Sequencing and effort

| phase | depends on | est. LOC (src + test) | note |
|---|---|---|---|
| MX0 probes | — | 0 | **done** |
| MX1 core-parse PB | — | 180 + 250 | the only new primitive; independently gated |
| MX2 identity + sweep | — | 130 | can land before MX1; leaves a green, unconfigured tree |
| MX3 spot arm | MX1, MX2 | 750 + 420 | |
| MX4 futures arm | MX3 | 620 + 380 | shares the `Driver` |
| MX5 discovery + config | MX2 | 330 + 200 | |
| MX6 boot wiring | MX3–MX5 | 430 | |
| MX7 worker | MX5 | 280 + 185 | serialize behind the pgrep guard |
| MX8 gates | MX3–MX7 | 330 + 70 fuzz | alloc + loopback tests bybit never had |
| MX9 live + docs | all | 90 docs | **the measured-Δ gate lives here** |
| **total** | | **≈ 3 140 src + ≈ 1 505 test + ≈ 190 docs/config ≈ 4 835 LOC** across ~60 files | |

MX1 and MX2 are independent and can be done in either order; MX2 first gives a green tree
with `VenueId::Mexc` present and nothing configured, which is the cheapest safe resting
point. MX3→MX4→MX6 is the critical path.
**All phases executed by 2026-09-23 (§11).** The estimates above are left as written.

---

## §9 Open questions — ALL ANSWERED 2026-09-23 (rulings in §0)

| id | question | gated by | **answered 2026-09-23** |
|---|---|---|---|
| **Q-MX1** | Spot `tradeId` is `"730292425431437318X0_730292425431437319X0"` — not an integer. Parse the leading digits as `venue_seq`, or declare spot trades seq-less (`venue_seq = 0`, the Bybit opt-out) and apply the §6.2 chain law to the **book** only? | operator ruling; affects D3 and the `wire-format.md` label paragraph | Leading digits; `venue_seq` on all four streams; the chain law applies to NONE (no gap events); seq regressions counted (D3 amended). |
| **Q-MX2** | `push.ticker` at 2.75 s is the only source of `fundingRate`/`holdVol`/`indexPrice`. Subscribe it for **every** perp (cost: 1 sub each, against a ≥ 40 cap) or only for the funding-bearing subset a strategy actually reads? | operator; affects connection count | Every perp (13 perps / 39 subs per connection). |
| **Q-MX3** | `nextFundingTime` is absent from `push.ticker`. Seed `Funding.v1` from REST at boot and advance by 8 h, or emit `v1 = 0` and let the worker funding lane own the schedule entirely? | D4 | Seeded per perp from REST, advanced by that symbol's own `collectCycle` (4 h or 8 h), `v1 = 0` when unseeded (D4 amended). |
| **Q-MX4** | `fees.toml` under O-MX1 has no measured fill to derive from (§1.5). Record the venue's published per-symbol rates as UNVERIFIED, or leave MEXC out of `fees.toml` until it is ever armed (and accept that `pnl_report` treats an unknown venue as fatal)? | operator; affects MX7 | Published rates, UNVERIFIED: spot 0:5, perp 1:4 (the dearest configured plate). |
| **Q-MX5** | Which instruments actually go in the first configured boot? §1.3 gives 1 966 + 1 174 candidates; a smoke universe should be ~14. Proposal: `BTCUSDT ETHUSDT SOLUSDT AAPLXUSDT SPYXUSDT` spot + `BTC_USDT ETH_USDT XAU_USDT USOIL_USDT EUR_USDT SPY_USDT AAPLSTOCK_USDT` perp. | operator | The proposal, as written (all 12 live 2026-09-23). |
| **Q-MX6** | MEXC in `build_ai_universe`? Bybit was deliberately excluded; including MEXC would let AI rulesets address it. | operator; Stage-3 adjacent | Yes — MEXC is in; Bybit stays out. |
| **Q-MX7** | Does MEXC join the news lane (`news/sources.py` has per-venue announcement + status parsers)? MEXC delists aggressively (R10), which is exactly what that lane is for. | operator; ~90 LOC | Yes — `json-mexc-ann` (3 sections on `www.mexc.co`) + `ping-mexc` (2 hosts); the `www.mexc.co` origin awaits the operator's trust approval. |

---

## §10 Sources — all measured 2026-09-20 09:43–09:52Z unless noted

**Live probes** (stdlib RFC-6455 client + `curl`, from the operator's machine):
`GET https://api.mexc.com/api/v3/{ping,time,exchangeInfo,ticker/bookTicker}` ·
`GET https://contract.mexc.com/api/v1/contract/{ping,detail,ticker,funding_rate/{sym}}` ·
`wss://wbs-api.mexc.com/ws` (bookTicker/deals `@10ms` and `@100ms`, increase.depth refused) ·
`wss://contract.mexc.com/edge` (`sub.ticker`, `sub.deal`, `sub.depth`, `sub.depth.full`,
`sub.funding.rate`, `ping`).

**Venue documentation** (for the claims the probes did not cover — connection lifetime,
idle reap, REST rate limits): MEXC Spot v3 API — Websocket Market Streams and Protocol
Buffers Integration (`mexc.com/api-docs/spot-v3/…`); MEXC Contract v1 API
(`mexcdevelop.github.io/apidocs/contract_v1_en/`); MEXC tokenized-stocks support article.
**Proto definitions read verbatim** from `github.com/mexcdevelop/websocket-proto`
(`PushDataV3ApiWrapper`, `PublicAggreDealsV3Api`, `PublicAggreBookTickerV3Api`,
`PublicIncreaseDepthsV3Api`, `PublicBookTickerV3Api`, `PublicDealsV3Api`,
`PublicLimitDepthsV3Api`).

**Repo facts** — verified by direct read at the cited line, not inferred:
`crates/core-types/src/lib.rs:55-125,3112` · `crates/engine/src/lib.rs:81,88-95,118,141,169` ·
`crates/core-metrics/src/registry.rs:17,22` · `crates/core-config/src/universe.rs:47,90` ·
`crates/exec-router/src/route.rs:61` · `scripts/copy-audit.sh:58-60` ·
`docs/wire-format.md:23-26,160-177,479-486` · `docs/venue-latency.md:74-88,151-158` ·
`docs/migration.md:3913-3954` · `docs/research-universe.md:39-48` ·
`docs/arch/binance-stocks-plan.md` (BST0–BST7) · `universe.toml.example:112-122` ·
`fees.toml.example:165-204` · `crates/ingress-bybit/**` (the reference implementation).

**2026-09-23 (MX9, the Mac):** the live smoke `crates/cli/tests/mexc_live_smoke.rs` against
`wbs-api.mexc.com` / `contract.mexc.com` and the two REST hosts; `python -m
claude_worker.latency_probe --minutes 10` (§1.7, `docs/venue-latency.md` §3); REST
`funding_rate/{SYM}` and `funding_rate/history` for the per-symbol cycle.

---

## §11 Progress log

- **2026-09-20** — MX0 executed (live probes, §1). Plan authored. Operator rulings O-MX1
  (data-only, exec-ready shape), O-MX2 (all four classes), O-MX3 (PB decoder in
  `core-parse`) recorded via AskUserQuestion. Seven open questions Q-MX1…Q-MX7 raised.
  Nothing built; no file outside this document touched; no git operation performed.
- **2026-09-23** — **MX1–MX9 implemented; nothing committed** (the operator stages and
  commits). Q-MX1…Q-MX7 and MX9-EXEC ruled via AskUserQuestion (§0); the launchd engine was
  never stopped. Built:
  - **MX1** `crates/core-parse/src/pb.rs` — `scan_varint` / `scan_pb_tag` / `scan_pb_len` /
    `scan_pb_field` (`PbField`), no allocation, `Option` everywhere, groups rejected;
    proptest + fuzz target `pb_scan`.
  - **MX2** `VenueId::Mexc = 7`, tick/event lane 6 (`NUM_TICK_LANES` 7), no depth / opt /
    fill lane; the hand-sweep to 8-wide tables (`stale_after_ms_defaults`,
    `ACTIVATION_NS_DEFAULT`, `ModelParams`, `SNAPSHOT_VENUES`, `VENUE_LABELS`, raw-tap and
    `--stale-after-ms` parsers); `mexc` in `MODEL_VENUE_LABELS` but NOT tradeable
    (`TRADEABLE_VENUES` stays 6); `("mexc", 7)` reserved in `core-config::exec` (D10);
    `funding_period_s(Mexc) = 28 800` as the nominal fallback.
  - **MX3/MX4** `crates/ingress-mexc` (`lib` / `spot` / `futures` / `run_loop` /
    `discovery`): one thread, N single-class connections (spot 15 symbols, futures 13),
    the order-agnostic PB walk, the futures JSON scanners, D3/D4/D7/D11/D12 as amended,
    `tests/mexc_tls_loopback.rs`.
  - **MX5** boot discovery (`exchangeInfo`, `contract/detail`, one `funding_rate/{SYM}` per
    live perp); `[mexc] spot / perp` (perp ordinal base 512); descriptors `mexc:` (Spot) /
    `mexc-perp:` (Perp) mirrored Rust↔Python + the shared TSV fixture; `MEXC_WS_HOST`,
    `MEXC_FUT_WS_HOST`, `MEXC_REST_HOST`, `MEXC_FUT_REST_HOST`.
  - **MX6** `cli::spawn_mexc` + `MexcConnSpec`, `boot_discovery::run_mexc`, capture label
    `mexc` (uniform file set + `--raw-tap mexc`), the `engine_ingress_mexc_*` family and the
    new ALL-venue `engine_ingress_<venue>_seq_regressions_total`, a `mexc` row on `/state`
    and the TUI, `build_ai_universe` gains the MEXC syms (Q-MX6).
  - **MX7** worker: `VENUE_MEXC = 7`, candle lanes `mexc` / `mexc-perp` (two hosts, two
    budgets), the funding history lane (per-symbol cycle), refdata, fetchers,
    `latency_probe` rows `mexc` / `mexc-perp` (its first binary parser), channel map,
    instrument class, `pnl_report` fee venue, the catalog venue reader; news kinds
    `json-mexc-ann` + `ping-mexc` and five `news.toml.example` rows; `fees.toml.example`
    `[fees.mexc]`; `universe.toml.example` `[mexc]`; `.env.example` hosts.
  - **MX8** `mexc_parsers_are_zero_alloc` + `mexc_run_loop_steady_state_is_zero_alloc`;
    fuzz targets `mexc_ws_frame`, `mexc_instruments`, `pb_scan`.
  - **MX9** the standalone live smoke (MX9-EXEC). Two live catches no fixture had: spot
    republishes an unchanged BBO every 10 ms → **D11** (tick = BBO change); futures closes a
    busy socket after 60 s without a client ping → **D12**
    (`Keepalive::poll_client_heartbeat`). Latency probe on the Mac → **Δ 150 ms applied**
    (spot binds; futures 130), **stale default 400 ms** (D8). Corrections in §1.7 (live ack
    text, per-symbol 4 h/8 h funding, the `@100ms` hex length, `depth.full` cadence on quiet
    perps). `capture_catalog` off-by-one fixed (R6); `wire-format.md` VenueId line fixed
    (R8); docs closed (`wire-format`, `migration`, `venue-latency`, `research-universe`,
    `ai-strategy-pipeline`, `CLAUDE.md`, `README`, `PLAN`, `AGENTS`, `exec.toml.example`).
  - **Not live yet:** MEXC enters the running universe only when `[mexc]` is added to
    `~/multivenue/universe.toml` and the engine restarts; the `www.mexc.co` news origin
    awaits the operator's trust approval.
  - **Post-review (same day).** An adversarial review found and fixed: the first
    `poll_client_heartbeat` anchored the first ping on inbound activity, so a busy socket
    STILL never pinged (now anchored on session start); the BBO dedupe now re-emits when
    the stale verdict flips and after a ring drop; the alloc test exercises both emit and
    dedupe paths. A second finding changed D7: a refused symbol no longer blinds its
    socket (R10). Final live smoke (`mexc_live_smoke`, 300 s, all 12 Q-MX5 instruments,
    run under heavy concurrent test + fuzz CPU load): **0 reconnects, 0 sub-drops, 0 parse
    errors**, 1 seq regression, 156 757 msgs → 9 054 ticks (94 % republications dropped by
    D11), no crossed book, every perp's Funding `v1` on the event lane at its own cycle
    (16:00Z for 8 h, 12:00Z for 4 h). Its stale shares (spot 2–4 %, futures 0–2 % of
    EMITTED ticks at 400 ms) are load-contaminated and counted on change ticks only — the
    engine-side §5 measurement of `docs/venue-latency.md` is due at the first engine boot
    with `[mexc]`. Ruled at go-live (§0 GO-LIVE b): a boot `funding_rate/{SYM}` seed failure is an error log, never a boot refusal — that perp runs with `v1 = 0` until the next restart.
- **2026-09-23 (second session) — GO-LIVE (§0 GO-LIVE).** Two rulings closed the last
  code items. First, a boot `funding_rate/{SYM}` SEED failure is an error log, not a boot
  refusal (that perp boots with Funding `v1 = 0`). Second, a spot bookTicker with one side
  emptied by proto3 omission is now 0/0, meaning no quote and no parse error; a body with
  no side at all is still rejected as drift (R1). Gates: nextest 2722 (2 skipped), alloc
  64/64, clippy, license-check, copy-audit new=0, pytest 1510 (3 skipped; one pre-existing
  date time bomb deselected). Commits: `b7678dd` (engine + crate + config) and `bbc8553`
  (worker + news). Nothing was pushed.
  - Live config, each file backed up as `*.bak-20260923T0845Z`: `[mexc]` = the Q-MX5 twelve
    appended to `~/multivenue/universe.toml`; `[fees.mexc]` (UNVERIFIED, Q-MX4) in the
    live `fees.toml`; the five MEXC news sources in the live `news.toml` (all five probed
    live: 20 items per announcement section, both pings answered). Release built from
    `bbc8553`; the paper engine was restarted the daily-restart way (SIGTERM drain, launchd
    KeepAlive) at 08:48:19Z.
  - Boot: discovery matched 12/12 (universe 3 090 live) with 7 funding seeds; two sockets;
    stale 400 ms; `vm_rows_active` 2. `claude-worker fetch` mapped all 12 MEXC descriptors
    into the market map. The run leaves 138 unresolved syms, all Deribit/OKX option chains
    and HL rolling slots — none MEXC. The first candles cycle backfilled `mexc` /
    `mexc-perp` bars (budget-paced; it resumes each cycle). The funding lane took the 7
    perps: 5 751 points, 0 failed.
  - §5 first reading (`docs/venue-latency.md` §5, one quiet minute): MEXC 0.35 % stale at
    400 ms; relative delay spot 8/147/366 ms, futures 8/41/163 ms. Every other venue was at
    ≈ 0 %, confirming that the early post-boot stale bursts (all venues at once, OKX excepted)
    were host disk contention from worker jobs, not MEXC.

