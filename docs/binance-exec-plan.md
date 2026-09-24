# Binance Execution Plan (BX0–BX14): fix-now pass first; then spot, USDⓈ-M + COIN-M futures (incl. TradFi stock perps), European options (long + short BTC/ETH), bStocks, Binance Stocks

**Status: v2, 2026-09-23 — BX0 LIVE.** The fix-now pass (F1–F4) is committed in the main checkout (`0a1f6ff`) and went live at the 16:05Z routine restart, verified (§5 BX0). Its zero-copy pass (O-BX12d) is the second BX0 commit and goes live at the 00:10Z restart (§12). BX1–BX14 are not started. v1 was written the same day at the operator's ask ("draft execution for binance spot, futures, options, stocks"). v2 records **20 operator rulings**, collected in five AskUserQuestion rounds, plus the instruction to make the fix-now pass the **first step**; three more (O-BX12b–d) followed the BX0 build.

**What this adds.** A second live arm, `VenueId::Binance = 1`, next to Hyperliquid. It sits behind the same two-switch interlock (`--exec` + `--arm-live`), the same risk gate and the same E-laws.

**What v1 scope looks like:**
- **No member goes live in v1 (O-BX6).** Every product and account mode is proven by its own exec battery on mainnet at minimum size, until a member passes the Stage-3 gate the normal way.

**Where the work happens:**
- **BX0** is the fix-now pass. It lands in the **main checkout**, because it fixes the running engine and ships at the next routine restart (D1).
- **BX1 onward** happens in a **separate worktree and branch** (O-BX15).

**Precedents:**
- The E-lane (E0–E7: `exec-router`, `exec-hyperliquid`, `docs/risk-policy.md` E-1…E-9) for everything about execution.
- The MEXC plan (`docs/mexc-ingress-plan.md`) for the shape of this document.
- HYPARB for the worktree setup.

**What has already happened:**
- The probes in §1.2 and BX0's K6 ran on the operator's Mac, against public endpoints only, with no keys. The scripts live outside the repo.
- The operator committed this plan (`022710a`). BX0 then changed the files its §12 entries list and committed them on the operator's word (O-BX12d): `0a1f6ff` (the fixes), then the zero-copy pass.
- The session's only git write operations are those two BX0 commits. The engine was never stopped.

---

## §0 Operator rulings: RECORDED 2026-09-23 (AskUserQuestion, five rounds)

| id | ruling | what it moves |
|---|---|---|
| **O-BX1** | "Stocks" in v1 is three things: **TradFi perps** (USDⓈ-M `TRADIFI_PERPETUAL`), **bStocks** (spot pairs) **and the Binance Stocks brokerage arm** (`/sapi/v1/equity/*`, routed to Alpaca). | new product `equity`; BX10 |
| **O-BX2** | **Every account structure is supported**: a dedicated classic sub-account, a **shared main account** (classic), **Portfolio Margin** (papi) and **PM Pro**. | account-mode layer in BX6; three futures modes in BX7 |
| **O-BX2a** | On a shared account, the engine uses an **owned-symbol list**. A foreign order or fill on an owned symbol is a **drift HALT**. Activity on any other symbol is ignored. | BX-8 rewritten; recon scope; D3 |
| **O-BX2b** | Account-mode build order: **all modes together** (classic, PM, PM Pro in one phase). | BX7 |
| **O-BX3** | Futures v1 is **USDⓈ-M + COIN-M**: perps, quarterlies and TradFi. | COIN-M ingress (BX2), inverse ledger law (BX3), ws-dapi (BX7) |
| **O-BX3a** | **COIN-M is IoC-only until a probe shows `countdownCancelAll` restored.** It was suspended 2026-06-29. | arm refusal `refused_no_deadman`; K16; generalised by D2 |
| **O-BX5** | Ed25519 comes from **`ring`**. | BX4 |
| **O-BX6** | **No member goes live.** Each product and mode ramps its exec battery only (R0) until a member passes the Stage-3 gate the normal way. | BX13 |
| **O-BX7** | v1 runs **on the Mac**. Tokyo stays a separate lane. | BX14 not in v1 |
| **O-BX8** | A Binance halt **latches only the slots whose venue mask names Binance**. The cancel-all it fires **fans out to every live arm**, Hyperliquid included. | router §3.4 |
| **O-BX9** | A halt **cancels only**. Flattening is an operator tool (reduce-only IoC). | BX11 |
| **O-BX10** | Options: **writing is allowed on BTC and ETH.** | short-option law BX-18; the account must be in Long & Short Sell mode |
| **O-BX10a** | Short-option exposure is capped with the **venue's initial-margin formula**. `RISK_LEVEL_CHANGE → REDUCE_ONLY` is `VenueLock`. | ledger option rows; index feed; D6 |
| **O-BX11** | **TradFi perps trade 24/7.** There is no session gate on new orders. | the session is observed, not gated (D4) |
| **O-BX12** | **The four live-engine problems are fixed FIRST (BX0).** They ship through the gates, then a **release build**, then **go live at the next routine daily restart**. | BX0 |
| **O-BX12a** | `TCP_NODELAY` goes on **every** TLS socket: one line in `TlsTransport::connect`. | BX0 F4 |
| **O-BX12b** | After the BX0 build: **the session edits the live `.env`**, one line only, to `BINANCE_EAPI_WS_HOST=fstream.binance.com`. No other line is read or changed; the secrets law otherwise stands. | BX0 F2 goes live at the restart |
| **O-BX12c** | **F3's test substitution is accepted**: the arm-level trait tests plus the router's spy-arm tests stand in for the router-level loopback test, which moves to BX3. | BX0 F3; BX3 |
| **O-BX12d** | **Commit BX0.** **Fix every pre-existing zero-copy finding in `ingress-binance`**, and put the crate in `make copy-audit`'s scope. | BX0, a second commit |
| **O-BX14** | The merge order against HYPARB is decided **when BX3 (router) starts**. | BX3 |
| **O-BX15** | BX1 onward is built in a **separate worktree and branch**, following HYPARB: `~/trading-engine-multivenue-binance` on branch `binance`, merged at a quiet point the operator names. | process |
| **O-BX16** | The Binance Stocks arm is a **full arm with a lifecycle battery, and no member uses it**. | BX10 |
| **K12** | Account eligibility: **all products are enabled** for this account (spot, USDⓈ-M/COIN-M, European options, Binance Stocks). | closed |

**Defaulted, not asked:**
- **O-BX4, transport.** Order entry uses the WS API with JSON requests for spot, UM and CM. It uses REST where no WS API exists: options, Portfolio Margin (papi) and Binance Stocks. SBE and FIX remain optional lanes (BX-S, BX-F).
- **O-BX13, UM dead-man.** `countdownCancelAll` is armed per symbol only while that symbol has resting orders: countdown 30 s, heartbeat 10 s.

**Derived, not ruled.** These are stated so a later reading does not mistake them for operator law.

| id | derivation | why |
|---|---|---|
| **D1** | BX0 lands in the **main checkout**, not the worktree. | O-BX12 ships it to the running engine at the next restart. The launchd wrapper execs the main checkout's `target/release/multivenue-engine`. |
| **D2** | **No resting order without a venue dead-man** (law BX-17), generalising O-BX3a. **Spot is IoC-only**, because spot has no dead-man. **PM futures are IoC-only** unless K13 finds a papi countdown. **Binance Stocks is the one exception**: it has no IoC at all (DAY/GTC only), so its orders are DAY with a **mandatory gateway TTL**. | O-BX3a's reasoning applied to every product lacking a countdown |
| **D3** | The owned list's granularity: **spot = base assets**, because balances are per asset; **UM/CM = symbols**, because one-way positions are per symbol; **options = underlyings**; **equities = tickers**. | O-BX2a made concrete per product |
| **D4** | TradFi sessions (`tradingSchedule`, `tradingSession`) are **observed and journaled** as a gauge, never gated. | O-BX11 |
| **D5** | For Binance Stocks, `equity_session` (RTH / EXTENDED / 24H), `equity_tokenize` and `equity_quote` are **required artifact keys with no default**. The battery exercises all sessions. | O-BX16: no member exists to choose them |
| **D6** | A **margin halt**, `MarginRisk = 11`, is added. The arm computes each product's margin ratio itself, because `RISK_LEVEL_CHANGE` is sent only to VIP and market-maker accounts. `MARGIN_CALL` is an immediate observation. | O-BX10 (writing) and futures leverage make margin the binding risk |

---

## §1 Venue facts

### 1.1 The trading surfaces, by product × account mode

Sources are in §11.

| product | engine class | order entry | user data (fills) | auth | amend | venue dead-man | test env |
|---|---|---|---|---|---|---|---|
| **Spot** (incl. **bStocks**) | `Spot` | **WS API** `wss://ws-api.binance.com:443/ws-api/v3` (JSON requests; JSON or SBE responses) · REST · FIX 4.4 | **Same WS API socket:** `userDataStream.subscribe` after `session.logon`, or `.subscribe.signature` (any key type). One subscription per account per connection. The listenKey REST endpoints were **removed 2026-02-20**. | Ed25519 `session.logon`; after it, no per-request signature (`timestamp` still sent) | `order.amend.keepPriority` (quantity ↓ only, **keeps priority**; capped by the `MAX_NUM_ORDER_AMENDS` filter, 10 today; weight 4; unfilled-order cost 0) · `order.cancelReplace` (one request) | **none**: no cancel-on-disconnect on the WS API, and none on FIX Logon | Spot testnet; **Demo Mode** `demo-ws-api` |
| **USDⓈ-M**, classic | `Perp` / `Dated` | **WS API** `wss://ws-fapi.binance.com/ws-fapi/v1` (JSON) · REST `fapi` | **Separate socket:** `wss://fstream.binance.com/private/ws?listenKey=…&events=…`. listenKey lasts 60 min, managed through WS `userDataStream.start/ping`. Several keys can share one `/private/stream`. | Ed25519 `session.logon` | `order.modify`: LIMIT only, **loses priority**, < 10 000 per order; batch ≤ 5 | `POST /fapi/v1/countdownCancelAll` per symbol, weight 10 | `demo-fapi`; WS API `testnet.binancefuture.com/ws-fapi/v1` |
| **COIN-M**, classic | `Perp` / `Dated`, **inverse** | **WS API** `wss://ws-dapi.binance.com/ws-dapi/v1` (order.place/modify/cancel/status, account.*) | `wss://dstream.binance.com/ws/<listenKey>`; the key comes from `POST /dapi/v1/listenKey` (the ws-dapi method list has no `userDataStream`) | Ed25519 `session.logon` | modify (LIMIT; both price and quantity required) | countdown **suspended 2026-06-29**; restoration UNVERIFIED (K16) → **IoC-only** (O-BX3a) | `demo-dapi` |
| **Portfolio Margin** (UM + CM + cross margin) | as above | **papi REST only** `https://papi.binance.com`: `/papi/v1/um/order`, `/papi/v1/cm/order` (POST / PUT modify / DELETE), `…/allOpenOrders`. **fapi/dapi trading and user data are disabled** once PM is on. | `wss://fstream.binance.com/pm/ws/<listenKey>` (`POST /papi/v1/listenKey`). Events: `ORDER_TRADE_UPDATE` with `fs` = UM/CM, `ACCOUNT_UPDATE`, `ACCOUNT_CONFIG_UPDATE`, `riskLevelChange`, `liabilityChange`, `openOrderLoss`, `executionReport` (margin) | HMAC / RSA documented; **Ed25519 UNVERIFIED (K13)** | `PUT` modify (LIMIT; loses priority) | **none listed** (absence UNVERIFIED, K13) → IoC-only (D2) | UNVERIFIED |
| **PM Pro** | as above | the **normal fapi/dapi** endpoints ("existing trading API endpoints … remain available"); **WS API usability UNVERIFIED (K14)** | `wss://fstream.binance.com/pm-classic/ws/<listenKey>` (`POST /fapi/v1/listenKey`); `PM_PRO_ACCOUNT_UPDATE` every 5 s | per fapi / dapi | per fapi / dapi | per fapi / dapi | UNVERIFIED |
| **European options** (crypto + XAU/XAG) | `Option` | **REST only** `https://eapi.binance.com`. **Options stay outside PM.** | `wss://fstream.binance.com/private/ws/<listenKey>` (moved in the 2025-12-14/15 migration) | HMAC-SHA256 documented, **per request**; Ed25519 UNVERIFIED (K3) | **none**: cancel + new | `countdownCancelAll` per underlying (minimum 5 s) + `countdownCancelAllHeartBeat` (weight 10 of 400/min). **Once it fires, new orders get `-2010` until a heartbeat or a zero countdown.** Listed under "Market Maker Endpoints"; whether it is MM-only is UNVERIFIED (K3) | `demo-fapi` + `demo-fstream` |
| **Binance Stocks** (brokerage → Alpaca) | `Spot` (cash equity, long-only) | REST `/sapi/v1/equity/`: `order/place`, `order/cancel`, `order/cancel-all`, `order/open-orders`, `order/detail`, `order/history`, `trade/history`. The paths differ in the Go SDK (K15). | `wss://nbstream.binance.com/equity`, `{listenKey}@orderReport` (UNVERIFIED, K15); fallback is a REST poll | SAPI signing; Ed25519 UNVERIFIED (K15). `POST /sapi/v1/equity/account/disclaimer` must be accepted first (otherwise 486410) | none known: cancel + new (K15) | **none**; orders are DAY/GTC (D2 exception) | UNVERIFIED |

### 1.2 Measured on 2026-09-23, 09:35Z, from the Mac

The Mac is in Bangkok. All endpoints were public and no keys were used. Pitfall 7 applies: these are live bodies, and every parser still gets its own live smoke.

**Network.** These numbers are one TCP connect at a time. The "Tokyo" attribution is inferred from RTT and the AWS address space.

| host | answered by | TCP connect min / median |
|---|---|---|
| `ws-api.binance.com` | 3.115.207.77 (Tokyo) | 113.4 / 116.6 ms |
| `ws-fapi.binance.com` | 13.192.51.94 (Tokyo) | 105.6 / 116.4 ms |
| `ws-dapi.binance.com` | 13.192.51.94 | 110.0 / 119.3 ms |
| `fstream` / `stream` | Tokyo | 106–107 / 110–117 ms |
| `papi.binance.com` | 175.41.211.210 | 117.5 / 128.8 ms |
| `api` / `fapi` / `eapi` | CloudFront edges (18.65.x / 3.166.x / 18.172.x) | 38.1 / 7.8 / 9.9 ms: **the TCP connection ends at a Bangkok edge; the order still crosses to Tokyo** |

**WS API round trip at the application level.** Six sequential `time` requests per host, after the handshake:

| endpoint | round trip |
|---|---|
| spot `ws-api` | 114.9–126.4 ms |
| `ws-fapi` | 105.0–137.0 ms (5 of 6 ≤ 113.2) |
| `ws-dapi` | 101.9–115.7 ms |

**Clock.** Binance server time minus local time was about **+146 ms**. That is one REST sample through CloudFront, so it is noisy. The same day's `latency_probe` run gives the better number: `docs/venue-latency.md:99-108` has binance +96.1 ms and binance-usdm +98.3 ms, **with every venue moving together (+96 to +108 ms)**.

**So this is the Mac's host-clock offset, not Binance's.** Under the venue-latency law, `ws-api`, `ws-fapi` and `ws-dapi` join `claude_worker.latency_probe` and `docs/venue-latency.md` before BX13.

**Rate limits**, from live `exchangeInfo`:

| product | REQUEST_WEIGHT (per IP) | ORDERS (per account) |
|---|---|---|
| spot | 6 000 / min (RAW_REQUESTS 300 000 / 5 min) | **100 / 10 s, 200 000 / day** |
| USDⓈ-M | 2 400 / min | **300 / 10 s, 1 200 / min** |
| COIN-M | 2 400 / min | 1 200 / min |
| **options** | **400 / min** | **30 / 10 s, 100 / min** |

- Since the CM-UM integration (complete 2026-06-30), **UM and CM share ONE weight pool and ONE ORDERS pool** across fapi and dapi. Each `exchangeInfo` still prints the full numbers.
- PM's documented limits are 6 000 weight / min and 1 200 orders / min (`GET /papi/v1/rateLimit/order`).
- Binance Stocks: 200 / min per user on placement, 50 / min on mint/redeem, SAPI weight on everything else.

**Instruments**, live samples:

- **Spot BTCUSDT:**
  - Tick 0.01, step 0.00001, `NOTIONAL` min 5.
  - `MAX_NUM_ORDERS` 200, `MAX_NUM_ORDER_AMENDS` 10.
  - `PERCENT_PRICE_BY_SIDE`: bid ×1.2 / ×0.5, ask ×2 / ×0.8.
  - STP default `EXPIRE_MAKER`; allowed `EXPIRE_TAKER`, `EXPIRE_MAKER`, `EXPIRE_BOTH`, `DECREMENT`, `TRANSFER`.
  - `amendAllowed`, `pegInstructionsAllowed` and `cancelReplaceAllowed` are all true.
- **bStock TSLABUSDT:** `TRADING`, tick 0.01, step 0.001, `NOTIONAL` min 5. Its permission set starts `[SPOT, MARGIN, TRD_GRP_004, TRD_GRP_005, …]` and has 207 groups.
- **USDⓈ-M symbol counts:**
  - `PERPETUAL` TRADING 571.
  - **`TRADIFI_PERPETUAL` TRADING 201**: `EQUITY` 163, `HK_EQUITY` 15, `COMMODITY` 8, `KR_EQUITY` 8, `PREMARKET` 4, `CN_EQUITY` 2, `FX` 1.
  - `PERPETUAL` SETTLING 130, and 2 + 2 quarterlies.
- **UM BTCUSDT:**
  - Tick 0.10, step 0.001, **`MIN_NOTIONAL` 50**, `PERCENT_PRICE` ±5 %.
  - TIF `[GTC IOC FOK GTX GTD]`, `pricePrecision` 2 / `quantityPrecision` 3, `maxMoveOrderLimit` 10 000.
  - STOP/TP types are refused on `/fapi/v1/order` (`-4120`); they moved to the Algo Service on 2025-12-09.
- **XAUUSDT** (`COMMODITY`): tick 0.01, step 0.001, **min notional 5**, ±2 %.
- **TSLAUSDT** (`EQUITY`): tick 0.01, step 0.01, min notional 5, ±3 %.
- **COIN-M:** `BTCUSD_PERP` contract size **100 USD**, `ETHUSD_PERP` **10 USD**, `quantityPrecision` 0 (quantity is **contracts**), TIF `[GTC IOC FOK GTX]` (no GTD, no RPI, no `TRADE_LITE`).
- **Options:**
  - **1 758 symbols**: BTC 752, ETH 600, BNB 128, SOL 74, **XAU 72, XAG 46**, DOGE 46, XRP 40.
  - **`nakedSell` is true only on the BTC and ETH contracts**, which matches O-BX10.
  - `BTC-260925-145000-C`: unit 1, **tick 5.000 USDT**, step 0.01. DOGE: unit 1000.

**Streams:**

| path | result |
|---|---|
| fstream `/ws/btcusdt@markPrice` (**the engine's path**) | 101, **0 frames in 7 s** → F1 |
| fstream `/market/ws/btcusdt@markPrice` | 101, 2 frames in 7 s (the 3 s cadence) |
| fstream `/ws/btcusdt@bookTicker` (the engine's path) | 101, 2 428 frames in 3 s |
| fstream `/public/ws/btcusdt@bookTicker` | 101, 296 frames in 3 s. One sample each, so **the tick lane stays until K7**. |
| nbstream `/eoptions/ws/<opt>@ticker` (**the engine's host and prefix**) | **HTTP 404** → F2 |
| fstream `/public/ws/<opt>@ticker` / `@bookTicker` | 101, 0 frames in 7 s. `@ticker` is not a documented options stream. `/public` carries `@bookTicker`, `@depth…`, `@optionTrade` and `@optionTicker…`; K6 probes those names on an at-the-money strike. |
| fstream `/market/ws/btcusdt@optionMarkPrice` | 101, 7 frames in 7 s. An array of every BTC option carrying `mp` (mark), `i` (index), `bo`/`ao` (best bid/ask). |
| dstream `/ws/btcusd_perp@bookTicker` | 101, 33 frames in 3 s |

### 1.3 Order semantics that shape the design

- **Spot:**
  - Order types: `LIMIT`, `MARKET`, `LIMIT_MAKER`, `STOP_LOSS[_LIMIT]`, `TAKE_PROFIT[_LIMIT]`. TIF `GTC`, `IOC`, `FOK`.
  - A `LIMIT_MAKER` that would cross is rejected with `-2010`.
  - **Spot has no GTD.**
  - The Price Range Execution Rule, rolled out from 2026-03-09, can expire an IoC with `eR = EXECUTION_RULE_PRICE_RANGE_EXCEEDED`.
  - `CANCEL_ONLY` became a symbol status on 2026-07-07.
- **UM:**
  - `order.place` accepts LIMIT and MARKET only. TIF `GTC`, `IOC`, `FOK`, `GTX`, `GTD` (≥ now + 600 s) or `RPI`.
  - **STP `NONE` has been refused since 2024-12-10**; the default is `EXPIRE_MAKER`.
  - `newClientOrderId` must match `^[\.A-Z\:/a-z0-9_-]{1,36}$`. A duplicate among open orders gets `-4116`.
  - **Modify** sends both price and quantity. A new quantity ≤ `executedQty` **cancels** the order, as does a GTX order whose new price would cross. `modifyId` is echoed; `reduceOnly` on modify arrived 2026-09-15.
  - `TRADE_LITE` is the lower-latency fill event. It carries no commission.
- **COIN-M:**
  - Quantity is in **contracts**.
  - No GTD, RPI or `TRADE_LITE`.
  - The position mode (`dualSidePosition`) and the STP configuration are **shared with UM** since 2026-06-30.
- **PM (papi):**
  - UM orders are LIMIT/MARKET, with GTD, priceMatch and STP.
  - Conditional orders use `/papi/v1/um/algo/order`.
  - Modifies lose priority.
  - Acks omit average price and the cumulative fields since 2026-08-03.
- **Options:**
  - LIMIT only. TIF `GTC`, `IOC`, `FOK`, `GTX`; `postOnly`, `reduceOnly` and `isMmp` are flags.
  - STP modes are `EXPIRE_TAKER`, `EXPIRE_MAKER` and `EXPIRE_BOTH`; always send the mode.
  - **There is no amend.** Batch place takes at most 10 orders.
  - The `orderId` can exceed 2^53, so parse it as an integer.
  - Writing errors: `-6057 WRITER_CANT_NAKED_SELL`, `-6052` / `-6053` (short-position limits), `-2027` (insufficient margin).
- **Binance Stocks:**
  - `orderType` MARKET or LIMIT; `timeInForce` DAY or GTC.
  - `tradingSession` is `RTH` (09:30–16:00 ET), `EXTENDED` (04:00–20:00 ET) or `24H`.
  - `quantity` **or** `notional`. `quoteAsset` defaults to USDC.
  - The `tokenize` flag defaults to true; its semantics are in K15.
  - Status goes **`NEW` → `ACCEPTED`** once the upstream broker acknowledges.
  - Fee: platform fee of **$0.35 minimum, or 10 bps above $350**. This is a per-order minimum, which today's `fees.toml` grammar cannot express (BX10).
- **Unknown execution status (every product):**
  - An HTTP 5xx or `-1007` timeout means "the execution status is UNKNOWN".
  - UM has three 503 variants; only "Unknown error" is ambiguous.
  - `-1008` means the request failed and was not counted; reduce-only orders are exempt from it.

### 1.4 Venue rules this engine must govern

**Spot unfilled order count:**
- Counted per account, across every key, IP and API, FIX included.
- Fills decrement it; cancels do not.
- A breach returns `-1015` / HTTP 429.

**UM Quantitative Trading Rules** (FAQ updated 2026-08-31). They are judged **per symbol per 10-minute cycle**:

| metric | definition |
|---|---|
| `UFR` | unfilled ratio |
| **`ICR`** | invalid cancels: **a cancel less than 5 s after placement**; counted over GTC, RPI, GTX and GTD orders |
| `IFER` | expired IoC/FOK ratio |
| **`DR`** | dust ratio: **an order under $50 is dust** |
| `BC` | bans in a rolling 24 h |

- For VIP 4–8 a ratio is recorded from ≥ 10 000 orders (5 000 for ICR and IFER). A ban hits at ≥ 0.99 (DR ≥ 0.9).
- For Regular and VIP 1–3 the recording threshold is **divided by 1.2^(N−1)**, where N is the number of symbols with open orders.
- Penalties: a 5-minute symbol lock, then 2 h, then **account-wide reduce-only** once 10 or more symbols are restricted.
- **An account with positions or open orders in ≥ 50 symbols may be made reduce-only.**
- Errors: `-4400`; `-4401` (`LARGE_POSITION_SYM_RULE`) also forces reduce-only. Monitor with `GET /fapi/v1/apiTradingStatus`.
- **Every exec battery order is dust** (min notional 5–50 USD). The battery therefore stays far below the recording thresholds by construction, and BX13's bars are stated so they cannot approach them.

**Other rules:**
- **REQUEST_WEIGHT is per IP and shared with the worker.** The candles and funding lanes on this Mac hit the same hosts from the same IP. For UM, WS API weight is a separate per-IP bucket from REST; the spot docs don't say either way.
- A 429 must be backed off. Continuing escalates to a **418 IP ban lasting 2 minutes to 3 days**.
- Delisting moves a UM symbol to `SETTLING`. Delivery and delisting closes arrive as `settlement_autoclose-…` fills; liquidation as `autoclose-…`; ADL as `adl_autoclose…`.
- **Options margin:**
  - Initial margin for a short = `[max(Index×10 %, Index×15 % − OTM) × Unit + Mark] × |pos|`.
  - Maintenance = `[max(Index×5 %, Index×7.5 % − OTM) × Unit + Mark] × |pos|`, plus 0.0019 × Index × Unit per the clearing procedures.
  - Margin call at 80 %; **liquidation at maintenance ≥ 95 % of adjusted equity**.
  - `RISK_LEVEL_CHANGE` (`NORMAL` / `REDUCE_ONLY`) "only applies to VIP and Market Maker accounts".

### 1.5 Stocks: the four surfaces. Three are in v1 (O-BX1)

1. **TradFi perps.** USDⓈ-M `contractType TRADIFI_PERPETUAL`, 201 trading.
   - They trade **24/7 and are not gated (O-BX11)**. When the underlying market is closed, pricing is EWMA-smoothed.
   - **Mark-vs-index deviation limits** are ±5 % in equity sessions and ±3 % on weekends and holidays, with commodities at ±3 % always. These are *not* order caps. The order band is `PERCENT_PRICE`.
   - The funding interval is per symbol (`GET /fapi/v1/fundingInfo`); 8 h (±2 %) is typical. Dividends arrive as `SPECIAL_FUNDING_FEE`.
   - **An agreement must be signed once**: `POST /fapi/v1/stock/contract`, or `POST /papi/v1/um/stock/contract` under PM. Without it, orders fail with `-4411` (third-party report; K4).
   - Sessions come from `GET /fapi/v1/tradingSchedule` and the `tradingSession` stream (D4: observed only).
   - The engine already ingests these as ordinary `binance-usdm:` rows. That is BST2's work, credited at `ingress-binance/src/discovery.rs:258-264`.
2. **bStocks.** Ordinary spot symbols (`TSLABUSDT`, `NVDABUSDT` …), live since 2026-06-11.
   - They trade 24/7 and are not offered to US persons.
   - They are gated by permission groups (`TRD_GRP_*`).
   - They already parse as spot rows (`discovery.rs:632-643`).
3. **Binance Stocks.** Real US equities through Nest Trading → Alpaca, live since 2026-06-01, with an API since 2026-07-20.
   - Wire and limits: §1.1 / §1.3.
   - **This is a broker path, not a matching engine.** Acks wait on the upstream broker, and quotes are about 5 s stale.
   - **In v1 as a full arm plus its battery, with no member (O-BX16).**
4. **US stock and ETF options** (2026-09-01). American-style, routed to Alpaca, **with no API endpoints**. Out of scope.

The eapi also lists **commodity options** (XAU/XAG). They are long-only for retail and need `POST /eapi/v1/stock/contract`. Writing stays BTC/ETH only (O-BX10).

### 1.6 Account prerequisites

These are the operator's checklist. None of them blocks a code phase.

1. **Eligibility.** All products are enabled for this account (K12). The arm still treats `-4402` (region) as a **fatal boot refusal**. `-4403` is a regional **leverage cap**, and the leverage assertion reports it.
2. **Accounts.** The account structures of O-BX2:
   - **Dedicated classic sub-account.** The engine owns everything in it.
   - **Shared main account, classic.** The artifact's owned list is required (O-BX2a, D3).
   - **Portfolio Margin.** Needs VIP 1–9 or more than 50 000 USDT. It is activated under Futures → Account Mode, with no open orders, positions or negative balances in UM, CM or Margin, and with isolated and multi-assets modes off. The mode can be switched at most 10 times per 24 h. **Enabling PM strips the Futures permission from existing keys, so a new key is needed afterwards.**
   - **PM Pro.** Needs VIP 7–9 or more than 10 M USDT, plus an application and a legal agreement.
3. **API key (Ed25519).**
   - Generate the keypair on the Mac. Register the **public** key; the private seed goes to `.env` only.
   - Permissions: Reading, Spot & Margin Trading, Futures, European Options, and Stocks as the UI names it. **Never Withdrawals.**
   - Restrict it to the engine's egress IP (R9).
   - If K13 or K15 show that papi or SAPI-equity refuse Ed25519, an HMAC or RSA key is added **for that product only**.
4. **UM/CM account settings.**
   - One-way mode, which is shared by UM and CM; multi-assets off; cross margin; leverage at or below the artifact maximum.
   - The engine **asserts these and never sets them**; `exec-smoke --binance setup` sets them.
5. **Agreements and modes.**
   - The TradFi Perps agreement.
   - The TradFi Options agreement, only if XAU/XAG options are used.
   - **"Long & Short Sell" mode**, required for O-BX10.
   - The Binance Stocks disclaimer (`POST /sapi/v1/equity/account/disclaimer`).
6. **BNB burn:** off, so commission is charged in the quote or base asset (BX-14).

### 1.7 Test environments

- **Demo Mode.**
  - Spot: `demo-api` / `demo-ws-api` / `demo-stream`. Futures: `demo-fapi` / `demo-dapi` / `demo-fstream`. Options: `demo-fapi` + `demo-fstream`.
  - Keys come from demo.binance.com.
  - Filters and limits are live-identical; prices are only "similar".
- **Spot testnet** (`testnet.binance.vision`): REST, WS API and FIX.
- **PM, PM Pro and Binance Stocks demo availability is UNVERIFIED (K13–K15).** Where no demo exists, the battery runs on mainnet at minimum size, and **only the operator arms it**.
- **Neither demo nor testnet is ever mixed with mainnet market data inside the engine** (BX-16). They exist only for the standalone battery. This is operator ruling O-E3 (2026-09-15): testnet for CI, mainnet for the ramp.

---

## §2 What the tree and the live engine hand this lane

Each finding below was verified by a direct read or a live measurement, not inferred. The citations are `path:line` at the current HEAD.

**F1–F4 are fixed by BX0, the fix-now pass (O-BX12).**

**F1 — The USDⓈ-M markPrice lane is dark.** → **BX0 — FIXED 2026-09-23**
- `crates/cli/src/bin/multivenue-engine.rs:2881-2888` subscribes `/ws/{sym}@markPrice`.
- Binance retired the legacy URLs for `/market` streams on **2026-04-23**, announced 2026-03-06. Unmigrated connections receive only `/public` streams.
- Measured: the legacy path delivered 0 frames in 7 s; `/market/ws/…@markPrice` delivered 2.
- ~~The running engine agrees: `msgs_total` − `ticks_total` = 5 since the 08:48Z boot.~~ **Corrected at BX0:** a mark frame counts once in `msgs_total` AND in `ticks_total` (the WS5 handler), so the two counters move together and their gap says nothing about this lane. The direct probe is the evidence, re-measured at K6 (§5 BX0).
- **So no Binance USDⓈ-M Mark, index or Funding events have reached a capture since the cut-over.**
- The perp exposure law (§3.5) needs this mark.
- **F1b (found at BX0's K6):** the live DATED frame is `"r":"0.00000000","T":0`, not the `"r":""` WS5 was written against. The WS5 parser would have read every dated contract as a perpetual paying 0 %: a `Funding` row per push per contract, on the event lane. Fixed with F1: funding needs a rate AND `T` > 0.

**F2 — The Binance options WS lane points at a retired endpoint.** It is not blocked by the network. → **BX0 — FIXED 2026-09-23**
- `multivenue-engine.rs:2923` hard-codes `/eoptions/stream?streams=…@ticker/…@index` on nbstream. Measured: **HTTP 404**.
- The options docs now list only fstream `/public` (quotes) and `/market` (`<uly>@optionMarkPrice`, `!index@arr`).
- No official page states that `/eoptions` was retired. The evidence is this 404 and Tardis, a third-party data vendor.
- So CLAUDE.md's "BN eapi-WS is unreachable from this network" is a misdiagnosis, and **an `.env` host change cannot fix it**: the path and the channel names are in code.
- Short-option exposure (§3.5) needs the mark and index this lane carries.
- K6 (BX0): every `<uly>@optionMarkPrice` element also carries the underlying's index `i`, so this lane needs no `!index@arr`; the real-time per-option `<symbol>@bookTicker` exists on `/public` (a BX2 input).

**F3 — The Hyperliquid arm never overrides the trait's `cancel` or `modify`.** → **BX0 — FIXED 2026-09-23**
- `cancel_by_cloid` (`exec-hyperliquid/src/exchange.rs:1743`) and `modify(prev, &Order)` (`:1891`) are **inherent** methods.
- `impl OrderDispatch for HlExchange` (`:1924` onward) overrides neither.
- So `RoutedDispatcher::cancel` and `::modify` (`exec-router/src/routed.rs:964`, `:1013`) reach the trait default, `Err(DispatchError::Unsupported)` (`clob-dispatcher/src/lib.rs:446-460`).
- It is invisible while bin15 runs with the maker off.

**F4 — `TCP_NODELAY` is never set on a TLS transport.** → **BX0 — FIXED 2026-09-23**
- `core-net/src/transport.rs:140-152` never calls `set_nodelay`. The only non-test call is `boot_http.rs:171`.
- With Nagle on, a **pipelined** order socket holds each small write until the previous segment is ACKed: about one 110 ms RTT from here.
- O-BX12a: set it on every TLS socket.

**F5 — The router holds exactly one live arm.** `live: L` (`routed.rs:76-80`); the boot builds `HlExchange` or `NullLiveDispatcher` (`multivenue-engine.rs:3905`, `:4024-4029`). → **BX3** (`LiveSet`)

**F6 — The halt signal is venue-wide.** One `live.halt_signal()` feeds every live slot (`routed.rs:1110-1140`). With two arms, a Binance streak would halt bin15. → **BX3** (per-venue signal; O-BX8)

**F7 — Binance has no fill lane.** `NUM_FILL_LANES = 4`, `fill_lane_of(Binance) = None` (`engine/src/lib.rs:159-190`). → **BX3** (lane 4)

**F8 — The ledger is shaped for HIP-4.**
- It relies on "contracts ×1e6 = USD ×1e6" and has no shorts.
- For an unbound leg, a BUY adds its base-unit quantity as if it were dollars (`ledger.rs:36-60, 102, 475-513`).
- → **BX3** (instrument rows: linear, inverse, long and short option, equity)

**F9 — Queued submits need a release path.** The router books resting orders on `Ok` (`routed.rs:931-944`). A queued arm's `Ok` means "will be sent", so venue rejects and IoC expiries need a way back to `ledger.on_cancel`. → **BX3** (`try_next_retired`)

**F10 — Orders on `binance:btcusdt` would route to Polymarket.**
- `spot[0]` gets the legacy flat id 7, whose venue byte is 0 (`core-config/src/universe.rs:131-134, 1562-1568`).
- Members stamp `Order.venue = symbol_venue_byte(sym)`: xsd `:589`, icdp `:507`, vm `:634`, vrp `:1275`, ai-exec `:446`.
- The fill model's `debug_assert_eq!` at `cli/src/backtest/fill.rs:1068` pins that equality, because captures hold orders.
- → **BX3** (route alias; the Order bytes stay untouched)

**F11 — Exchange filters are thrown away after boot.**
- `BnSymbolRow` (`ingress-binance/src/discovery.rs:41-62`) keeps only tick and step of the filters, and drops even those after the coverage audit.
- `TRADIFI_PERPETUAL` folds into `Perpetual` (`:264`).
- `EapiOptionRow` (`eapi.rs:80-95`) keeps no `unit`, tick, step or minQty.
- → **BX2**

**F12 — The key types do not yet cover Binance.**
- `SecretKeyBytes` is exactly 32 B and mlock'd (`core-config/src/lib.rs:351-399`). That fits an Ed25519 seed; it does not fit a 64-character HMAC secret.
- `core-crypto` has no Ed25519 and no hex.
- `ring` 0.17 is only transitive.
- → **BX4**

**F13 — The HL order path blocks the engine thread.**
- Its POST is synchronous, with a 5 s deadline (`exec-hyperliquid/src/http.rs:53`). That fits bin15's cadence.
- For Binance it would stall the tick loop about 110 ms per order from here.
- → **BX6** (a gateway thread)

---

## §3 Architecture

### 3.1 Threads, rings, single writers

```
 ingress threads ──ticks / events / opt summaries──▶ ENGINE THREAD  (single writer: members, router, ledger)
      member ─Order─▶ RoutedDispatcher ─risk gate─▶ LiveSet<Hl, Bn>
                                                    ├─ venue 4 → HlExchange  (unchanged, except BX0 F3)
                                                    └─ venue 1 → BnArm       (thin, engine thread)
      BnArm::submit: row · owned? · quantize · governor · cid · push BnCmd (64 B) ──┐  SPSC cmd ring (1 024)
 ┌──────────────── bn-gateway THREAD (single writer of every Binance socket) ◀──────┘
 │ classic : spot WS API (orders + user data, ONE socket) · UM WS API · CM WS API
 │ PM      : papi REST keep-alive (UM/CM orders, modify, cancel)  +  fstream /pm user stream
 │ PM Pro  : fapi/dapi (WS API if K14)                            +  fstream /pm-classic user stream
 │ always  : fstream /private (UM + options listenKeys on one socket) · dstream (CM key, classic)
 │           eapi REST (options) · fapi/dapi REST (countdown, recon) · sapi equity REST · nbstream /equity (K15)
 │ open-order table · TTL wheel · clock offset · recon · margin ratios · owned list · journal
 ├── acks / rejects / retired / status ──▶ SPSC evt ring (1 024) ──▶ BnArm::on_idle (engine thread)
 └── fills ─────────────────────────────▶ fill lane 4 (SPSC 1 024) ──▶ engine → on_fill_booked → ledger → member
```

Exactly two writers exist. The **engine thread** produces the command ring and consumes the event ring and lane 4. The **gateway thread** owns every Binance socket and every Binance table. There are no locks.

Every ring is `core-ring`'s cache-aligned SPSC, which is "SPSC only" per CLAUDE.md. That meets and exceeds the operator's "bounded crossbeam over `std::mpsc`" doctrine. MPMC machinery on a single-producer lane is cost without benefit.

### 3.2 The engine-thread half: `BnArm`

`BnArm` implements `OrderDispatch`. On submit it only validates and pushes, with a target under 200 ns:

1. **Find the row, checking the alias first.**
   - Compare the full `SymbolId` against the alias table (`LEGACY_BN_ANCHOR_SYM` 7 → spot[0]).
   - Then, **only if the venue byte is Binance**, index a flat ordinal map.
   - The order matters because the anchor's ordinal (7) equals spot[6]'s (`universe.rs:1563-1568`). An ordinal-only map would collide.
2. **Refuse, and count the reason**, when any of these hold:
   - the row is not live-tradable (§3.6);
   - the product is not armed;
   - the symbol is **not owned** on a shared account (`refused_not_owned`, O-BX2a);
   - another slot owns it (BX-7);
   - **the order is a maker on a product with no working dead-man** (`refused_no_deadman`, BX-17);
   - it is a short option on a non-writable contract (`refused_not_writable`, BX-18).
3. **Quantize** (BX-5):
   - BUY price floors to the tick; SELL price ceils; quantity floors to the step.
   - Every row carries **magic reciprocals**, so there is no hardware divide.
   - Refuse when quantity < minQty, when notional < minNotional, or when price is outside the local `PERCENT_PRICE` band while a fresh mark exists.
4. **Governor** (§3.9): ORDERS windows, unfilled-order count, QTR, breadth, and the Stocks placement limit. It refuses synchronously, so the router never books a doomed order.
5. **Push** the `BnCmd` (64 B). A full ring returns `QueueFull`, so the router books nothing.

`on_idle` drains up to 64 `BnEvt`s. From them it updates the governor, the streaks, the counters and the margin observations, and it queues `Retired` records for the router. `halt_signal_venue(Binance)` then reports observations only; the router owns the thresholds, the E6 split.

```rust
/// Engine → gateway. One per verb. 64 B, one cache line, moved by value into its slot.
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct BnCmd {
    pub client_oid: u64,       // @0  member's id (current)
    pub prev_client_oid: u64,  // @8  modify: the order being replaced; else 0
    pub px_1e6: i64,           // @16 already on the tick
    pub qty_1e6: i64,          // @24 already on the step (COIN-M: contracts ×1e6)
    pub ttl_deadline_ns: u64,  // @32 CLOCK_MONOTONIC_RAW; 0 = none (IoC)
    pub row: u16,              // @40 instrument row
    pub verb: u8,              // @42 PLACE / CANCEL / MODIFY / CANCEL_ALL / FLATTEN (operator tool only)
    pub kind: u8,              // @43 ORDER_KIND_MAKER / ORDER_KIND_IOC
    pub side: u8,              // @44 Side
    pub slot: u8,              // @45 strategy_id
    pub flags: u8,             // @46 bit0 amend-eligible (qty↓, px unchanged) · bit1 opens-short (options)
    pub _pad: [u8; 17],        // @47
}
const _: () = assert!(core::mem::size_of::<BnCmd>() == 64);

/// Router-facing retirement (F9). 16 B.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Retired { pub client_oid: u64, pub slot: u8, pub why: u8, pub _pad: [u8; 6] }
// why: REJECTED · EXPIRED (IoC remainder, GTX-would-take, price-range, DAY end) · CANCELED_VENUE (STP,
//      liquidation, a modify the venue turned into a cancel) · CANCELED_TTL · CANCELED_MEMBER · FILLED
```

### 3.3 The gateway thread

```
loop {
    // 1. latency path first: every queued command, rendered into ONE tx burst per connection
    while let Some(cmd) = cmd_rx.try_pop_ref() { render(&cmd) }  // WS API JSON, or REST query + signature
    if rendered { mask_in_place(); rustls.write(); flush() } // ONE write_tls/send per burst — the E7 lesson
    // 2. sockets
    poll(0) → per ready conn: read → frames → WS responses (ACK/reject) | user events (fills, state, margin)
                              | REST bodies (PM/options/equity ACKs, recon)
    // 3. coarse timers, checked every ~1 ms
    TTL cancels · server-ping echo · listenKey keepalives (30 min) · countdown heartbeats · clock sync (60 s)
    · recon + margin ratios (60 s) · equity order poll if K15 finds no orderReport (1 s) · status event (100 ms)
    · 23 h session rotation (make-before-break)
}
```

**Rendering.**
- Requests are pre-templated per `(row, verb, kind)`. The static bytes (symbol, side, type, TIF, STP, `newOrderRespType=ACK`) are fixed at boot.
- Per order, the gateway writes only the id (27 B), the price and quantity digits, and the timestamp.
- REST requests (papi, eapi, sapi) are signed over the rendered query **in place**. Digits come from a two-digit lookup table.

**Sessions, per account mode (O-BX2):**

| mode | order entry | user data |
|---|---|---|
| **classic** | spot, UM and CM WS API sessions, each with `session.logon` (Ed25519, sorted-parameter payload) | spot on the same socket; UM on fstream `/private`; CM on dstream `/ws/<key>`, with the key from `POST /dapi/v1/listenKey` |
| **PM** | papi REST keep-alive for UM and CM (no WS API) | fstream `/pm/ws/<key>` (`POST /papi/v1/listenKey`) |
| **PM Pro** | fapi/dapi, over WS API if K14 allows it, else REST | fstream `/pm-classic/ws/<key>` (`POST /fapi/v1/listenKey`) |

**Every mode shares:**
- **Options**: eapi REST, with the key on the shared fstream `/private` socket.
- **Binance Stocks**: sapi REST, plus the orderReport stream (K15).
- **Arrival order.** A fill can arrive before its ACK, because the user stream is a different socket from order entry. The open-order table is keyed from submit time, so either order resolves.
- **Rotation.** Connections live at most 24 h. At about 23 h, in a quiet moment, B is opened, logged in and subscribed; sending moves to B; A closes. A `serverShutdown` event forces this immediately.

### 3.4 Router deltas (BX3)

These go through `risk-reviewer`, and the review reads the diff together with `docs/risk-policy.md`.

1. **`LiveSet<A, B>` with `MaybeArm<T> { Absent(NullLiveDispatcher), Present(T) }`.**
   - It is enum dispatch, not `dyn`, and one monomorphization serves every arm combination. The boot's `if venue_live(Hyperliquid) { … } else { … }` (`multivenue-engine.rs:3905` / `:4020`) disappears.
   - `submit`, `cancel` and `modify` route by the **route venue** (item 4).
   - `on_idle` calls `a | b`, the non-short-circuit OR.
   - `cancel_all` fans out (O-BX8); `cancel_all_state` takes the worst of the two (`Stranded` > `Working` > `Clear`).
   - `arm_counters()` **sums only the monotonic counters**. The levels (`budget_remaining`, `pnl_anchor_usd_1e6`, `session_pnl_usd_1e6`, at `clob-dispatcher/src/lib.rs:749-761`) stay HL's in the flat `exec.arm_*` block (`engine-snapshot/src/json.rs:298-321`). Every arm's levels also appear in an additive per-venue `/state` block.
   - It nests for a third arm, HYPARB's, as `LiveSet<LiveSet<Hl,Bn>,Evm>`.
2. **Per-venue halt signal.** A defaulted `halt_signal_venue(&self, venue: u8) -> HaltSignal { self.halt_signal() }`. Per live slot, the router merges the signals of the venues in **its** mask:
   - **max** of `reject_streak`, `asset_refusal_streak`, `recon_drift_usd_1e6`, `ws_gap_ns` and `recon_age_ns`;
   - **OR** of `budget_floor_breached`, the new `venue_lock` and the new `margin_risk`;
   - **AND** of `reconciled` and `pnl_judged` (was `pnl_flat` before S7-L1);
   - **sum** of `pnl_delta_usd_1e6`.

   `HaltSignal` is asserted at 48 B (`clob-dispatcher/src/lib.rs:898`) and has 5 pad bytes. The two new `u8` flags take two of them, so **the size is unchanged**. The ledger is seeded only when every present arm reports reconciled.
3. **Retirement.** A defaulted `try_next_retired(&mut self) -> Option<Retired>`, whose default is `None`, so HL is bit-identical.
   - The router's `on_idle` drains up to 64 per call. Each one calls `ledger.on_cancel(client_oid, slot)` and increments `retired[why]`.
4. **Route alias.**
   - **It is not stored in `ExecRoute`**, whose 640 B layout and `halts` offset are pinned by a test (`route.rs:250-286, 547-552`).
   - Instead, `RoutedDispatcher` and `LiveSet` each hold `[RouteAlias; 4]`, where `#[repr(C)] struct RouteAlias { sym: SymbolId, venue: u8, _pad: [u8; 3] }`. Today's only entry is `LEGACY_BN_ANCHOR_SYM` → Binance, or the `--binance-sym-id` override.
   - The Order bytes, the captures and `backtest/fill.rs:1068` are untouched.
5. **Ledger instrument rows and the price feed** (§3.5).
6. **Two new halt reasons.** Both are mirrored in `engine-snapshot`'s words and pinned like the others (`exec_boot.rs:721-750`).
   - **`VenueLock = 10`**, for a venue-imposed restriction:
     - a 418 ban, `-4400` or `-4401` reduce-only, or `-4402` region;
     - options `RISK_LEVEL_CHANGE → REDUCE_ONLY`;
     - a fired options countdown (the subsequent `-2010`);
     - an account-mode or permission change mid-session (BX-19).
   - **`MarginRisk = 11`** (D6). The arm computes each product's margin ratio at every recon, and immediately on `MARGIN_CALL` / `ACCOUNT_UPDATE`:
     - UM/CM: maintenance margin over margin balance;
     - PM: `uniMMR`;
     - options: maintenance margin over adjusted equity.

     The halt fires when the ratio reaches the slot's `halt_on_margin_ratio_1e6`.
   - **Why not reuse `BudgetFloor`.** That reason means "our own governor hit its floor". Naming a venue restriction the same way would reproduce the defect shape E6 recorded, a name describing a different property than the one it tests.

### 3.5 The ledger's instrument rows (BX3)

A new `InstrumentRow` table sits beside the untouched `FamilyRow` table.

- **Size.** `LEDGER_INSTRUMENTS = 256`, `#[repr(C, align(64))]`. Each row holds `sym`, `class`, `law`, `unit_1e6`, `mark_1e6`, `index_1e6`, and a per-slot **signed** `pos_1e6: [i64; 8]`.
- **Binding.** Rows are **bound at boot** by discovery (`bind_instrument`), so LAW E-4's "bound, never derived" holds.
- **Unbound legs.** An unbound Binance sym is refused by the arm before the router sees it.

| law | position | exposure (USD ×1e6) | day turnover (cap_day) | short |
|---|---|---|---|---|
| **Spot / bStocks / equity** | base ×1e6 | `abs(inv) × mark` | buys | refused (no margin); `shorts_refused` |
| **Linear** perp / dated / TradFi | base ×1e6, signed | `abs(pos) × mark` | **the part of the quantity that increases abs(pos)**, on either side | allowed |
| **Inverse** (COIN-M) | contracts ×1e6, signed | `abs(pos) × contract_size_usd`, independent of price | the increasing part | allowed |
| **Option, long** | contracts ×1e6 | `pos × unit × mark` (premium at risk) | buys | — |
| **Option, short** (BTC/ETH only, O-BX10) | contracts ×1e6, negative | **venue IM**: `[max(index×10 %, index×15 % − OTM) × unit + mark] × abs(pos)` (O-BX10a) | sells that open or increase a short | allowed only where `nakedSell` |
| **Prediction** (HIP-4) | unchanged | unchanged | unchanged | unchanged |

**The price feed.** This is new, and it closes F1/F2's loop:
- **Linear and inverse rows** take `mark` from Binance `Mark` ChannelEvents (v0 = mark ×1e6, v1 = index ×1e6). They arrive through the router's existing `on_venue_event`, which today returns early for anything but `InstrumentRoll` (`routed.rs:1175-1206`).
- **Option rows** take `mark` and `index` from the restored options lane, through a new defaulted hook, `on_opt_summary(&OptSummary)`. The engine forwards what it already drains for the members.
- **Until a row's first mark arrives**, exposure falls back to the last venue fill price. A short option with no index yet is **refused**, because the IM formula cannot be priced.

**The one-price rule.** The increase test prices the touched row at **one price** on both sides of the comparison: the order's price for linear rows, and the current mark and index for option IM. A price move can therefore never make a reducing order look like an increase. `the_exposure_clamp_never_refuses_an_order_that_reduces_it` extends to every signed law.

**Settlement fills** (`FILL_FLAG_SETTLEMENT`: options expiry, UM/CM delivery) zero the row and add no turnover.

### 3.6 The instrument table and the numeric law

```rust
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct BnInstHot {
    pub sym: u32,              // @0
    pub tick_1e6: u32,         // @4   ≤ 4 294 USD per tick — a larger tick is refused at boot
    pub step_1e6: u32,         // @8
    pub product: u8,           // @12  spot / usdm / coinm / options / equity
    pub class: u8,             // @13  core_types::InstrumentClass
    pub flags: u8,             // @14  live-ok · tradfi · bstock · dated · inverse · writable · no-deadman
    pub law: u8,               // @15  the ledger exposure law (§3.5)
    pub tick_magic: u64,       // @16  u64 ÷ tick by multiply-shift (proptest-pinned against `/`)
    pub step_magic: u64,       // @24
    pub min_qty_1e6: i64,      // @32
    pub min_notional_1e6: i64, // @40
    pub unit_1e6: i64,         // @48  option unit (1 / 100 / 1 000) · COIN-M contract size USD · else 1e6
    pub px_dec: u8,            // @56  wire decimals of the tick
    pub qty_dec: u8,           // @57
    pub tick_sh: u8,           // @58
    pub step_sh: u8,           // @59
    pub _pad: [u8; 4],         // @60
}
const _: () = assert!(core::mem::size_of::<BnInstHot>() == 64);
```

- **`BnInstWire`** holds the upper-case wire symbol (≤ 32 B + len), for example `BTC-260925-145000-C` or `BTCUSD_PERP`. Only the gateway reads it.
- **Ordinal blocks.** The existing blocks are spot ≤ 500, usdm 513–1012, eapi 1025– and dated 2049–2548. New ones:
  - **COIN-M perps `BN_COINM_ORDINAL_BASE = 3072`**;
  - **COIN-M dated `3584`**;
  - **equities `BN_EQUITY_ORDINAL_BASE = 4096`**.

  All are disjoint by construction, since `VENUE_LIST_MAX = 500`. After the alias check, the flat map is `[u16; 8192]` (16 KiB).
- **Live-tradable** means all of the following, and anything else is refused for live at boot, never rounded:
  - `status == TRADING`;
  - the tick and step are **exact multiples of 1e-6**;
  - the product is armed;
  - the permissions are held (bStock groups; the equity disclaimer);
  - on a shared account, the instrument is owned.

  SETTLING and `CANCEL_ONLY` rows stay bound for cancels and fills only.

### 3.7 Client order id: LAW E-9 carried

```
mv  EEEEEEEE  S  OOOOOOOOOOOOOOOO      27 chars, [a-z0-9] only
│   │         │  └ client_oid, 16 lower-hex (u64)
│   │         └ slot, 1 hex
│   └ boot epoch = low 32 bits of unix seconds at boot, 8 hex
└ prefix
```

- **Valid wherever it has been checked.** It fits the spot regex `^[a-zA-Z0-9-_]{1,36}$` and the UM/CM pattern. The options and Binance Stocks charsets are UNVERIFIED (K3, K15), hence `[a-z0-9]`.
- **Decoding** reads fixed offsets and parses hex with SWAR.
- **Foreign ids.**
  - `autoclose-` (liquidation), `adl_autoclose` (ADL) and `settlement_autoclose-` go to the **owner** slot (BX-7), the last with `FILL_FLAG_SETTLEMENT`.
  - Any other id on an owned instrument is **foreign** (BX-8). On a dedicated account it is recon drift; on a shared account it is a drift HALT on that instrument (O-BX2a).
  - An `mv` id from another epoch is an orphan, swept at boot.

### 3.8 Verb × product × mode mapping

| engine verb | spot | UM / CM (classic, PM Pro¹) | UM / CM (PM) | options | Binance Stocks |
|---|---|---|---|---|---|
| place, maker | **refused (BX-17)** | UM `order.place` `LIMIT`+`GTX`; **CM refused until K16** | refused unless K13 finds a papi countdown | `LIMIT`+`postOnly`, only if K3 shows the countdown is ours to use; else refused | `LIMIT` `DAY` (**the BX-17 exception**, TTL mandatory) |
| place, IoC | `order.place` `LIMIT`+`IOC` | `order.place` `LIMIT`+`IOC` | `POST /papi/v1/{um,cm}/order` `IOC` | `LIMIT`+`IOC` | no IoC exists; refused |
| TTL | gateway cancel | gateway cancel | gateway cancel | gateway cancel | gateway cancel (+ DAY expiry) |
| cancel | `order.cancel` | `order.cancel` | `DELETE /papi/v1/{um,cm}/order` | `DELETE /eapi/v1/order` | `POST …/equity/order/cancel` |
| modify qty ↓ | **`order.amend.keepPriority`** | `order.modify` (price and quantity; loses priority) | `PUT /papi/v1/{um,cm}/order` | cancel + new | cancel + new (K15) |
| modify price | **`order.cancelReplace`** (1 request) | `order.modify` | `PUT …/order` | cancel + new | cancel + new |
| cancel-all (halt) | `openOrders.cancelAll` per symbol | `DELETE /{fapi,dapi}/v1/allOpenOrders` | `DELETE /papi/v1/{um,cm}/allOpenOrders` | `DELETE /eapi/v1/allOpenOrdersByUnderlying` | `POST …/equity/order/cancel-all` |
| **ACK** | WS response | WS response | HTTP response | HTTP response | HTTP response (`NEW`; `ACCEPTED` arrives later as state) |
| **FILL** | `executionReport` `x=TRADE` | UM: **`TRADE_LITE` ⊕ `ORDER_TRADE_UPDATE`**, deduplicated on (symbol, `t`); CM: `ORDER_TRADE_UPDATE` | `ORDER_TRADE_UPDATE` (`fs` = UM/CM) | `ORDER_TRADE_UPDATE` | `orderReport` (K15), else `trade/history` poll |

¹ PM Pro uses the classic endpoints, over the WS API if K14 allows it, else fapi/dapi REST. Its user stream is `/pm-classic`.

**IoC-only products get no cancel or modify verbs in v1.** Under BX-17 those are spot, COIN-M until K16, PM unless K13, and options unless K3. For them the cancel, modify and requote verbs are **not built** (no dead code); the table names the venue verb each will use once its gate opens. **A halt's cancel-all on an IoC-only product is a confirmed no-op**: `openOrders.status` is empty, so the state is `Clear`.

**On every order:**
- `newOrderRespType=ACK`.
- `selfTradePreventionMode` explicit, from the artifact. UM/CM refuse `NONE`.
- `newClientOrderId` = the cid.
- `timestamp` + `recvWindow` (BX-6).
- `returnRateLimits=false` on hot WS requests, with a 10 s cold probe reconciling the governor.

**Modify edge cases.**
- **A UM/CM modify keeps the venue clientOrderId.** The open-order entry is keyed by the *original* cid and carries `current_client_oid`, so fills reach the member under the id the router's resting row was renamed to.
- **A modify the venue turns into a cancel** (new quantity ≤ executed, or a GTX that would cross) retires the order as `CANCELED_VENUE`, never as a silent success.

### 3.9 Governors: fail-closed, figures from live `exchangeInfo` and the docs

| budget | scope | limit | refused at |
|---|---|---|---|
| ORDERS spot | account | 100 / 10 s, 200 000 / day | `orders_frac` of either window |
| ORDERS UM + CM (**one pool** since 2026-06-30) | account | 300 / 10 s, 1 200 / min | `orders_frac` |
| ORDERS PM | account | 1 200 / min (`GET /papi/v1/rateLimit/order`) | `orders_frac` |
| ORDERS options | account | **30 / 10 s, 100 / min** | `orders_frac` |
| Binance Stocks placement | user | **200 / min** | `orders_frac` |
| Spot unfilled-order count | account | per the venue's windows; fills decrement it | `orders_frac` |
| UM QTR (UFR · ICR · IFER · DR) | per symbol per 10-minute cycle | FAQ table + the Regular/VIP 1–3 scaling | `qtr_frac × threshold`, once the cycle count passes the recording threshold |
| UM/CM breadth | account | < 50 symbols with **positions or open orders** | slot `max_symbols` (default 20) |
| REQUEST_WEIGHT (REST) | **IP, shared with the worker** | spot 6 000; UM + CM **2 400 together**; PM 6 000; eapi **400**, per min | the gateway keeps a reserved floor |

**Where the governor lives.** The order-side governors are on the engine thread, which is authoritative because every submit passes through `BnArm`. REST weight is on the gateway, the only REST caller.

**Venue responses.**
- `-1015`, 429 or `-1003` is a `BudgetFloor` observation.
- 418, `-4400`, `-4401` or `-4402` is `VenueLock`.
- `-4403` is a leverage cap, caught by the boot assertion.
- Nothing is ever retried in a loop.

### 3.10 Reconciliation, ownership, orphans, in-doubt orders, settlement, margin

**Every 60 s, and on every reconnect:**

| product · mode | positions / balances | open orders |
|---|---|---|
| spot | `account.status` | `openOrders.status` |
| UM · classic / PM Pro | WS `v2/account.position` + `v2/account.balance` | REST `GET /fapi/v1/openOrders` |
| CM · classic / PM Pro | WS `account.position` + `account.balance` (ws-dapi) | REST `GET /dapi/v1/openOrders` |
| UM / CM · PM | `GET /papi/v1/account`, `/papi/v1/balance`, `/papi/v1/um/positionRisk`, `/papi/v1/cm/positionRisk` | `GET /papi/v1/{um,cm}/openOrders` |
| options | `GET /eapi/v1/position` + `/marginAccount` (48 weight / min of 400) | `GET /eapi/v1/openOrders` |
| Binance Stocks | positions endpoint K15 | `GET …/equity/order/open-orders` |

**Comparing.**
- Booked positions (from lane-4 fills, per owner slot) are compared with the venue's.
- Drift across two cycles becomes `recon_drift_usd_1e6`. Venue legs we never booked are counted as unseen.
- **`reconciled = 1` only when both are zero.**
- **On a shared account (O-BX2a, D3), only owned instruments are compared.**
  - Spot compares **owned base assets**. Quote-asset balances are observed for "insufficient balance" risk but not reconciled, because the operator's own trades move them.
  - A foreign fill or open order on an owned instrument is a **drift HALT**. Everything unowned is ignored.

**Orphans.** Before the first `reconciled`, every `mv` order from a foreign epoch is cancelled. Foreign open orders on owned instruments are reported, and on a shared account they refuse the boot.

**In-doubt orders** (BX-11). An order whose ACK was lost is **never resent**. It is resolved by a status query and stays booked as resting until then.

**Settlement.**
- Options expire at 08:00 UTC on the 30-minute index average. Short-option **assignment** settles the same way, in cash.
- Both are booked from `/eapi/v1/exerciseRecord` as `FILL_FLAG_SETTLEMENT` fills.
- UM/CM delivery arrives as `settlement_autoclose-` fills.
- **Never inferred from a vanished position.**

**Margin** (BX-20, D6). Each product's margin ratio is computed at every recon and on every `MARGIN_CALL` / `ACCOUNT_UPDATE`. It feeds `HaltSignal.margin_risk`.

**Session bound** (E7 machinery):
- UM `totalMarginBalance`; CM per-asset balance marked to USD; PM `accountEquity`; options `equity`.
- Spot and equities: realized P&L plus inventory at mid (equities at the ~5 s REST quote).

### 3.11 Kill switches

1. **Router halt** (latched per venue mask) → `cancel_all` on **every** arm (O-BX8) → per-product sweeps plus confirmation → `Clear` / `Working` / `Stranded`. **No automatic flatten** (O-BX9).
2. **Venue dead-man** (BX-17: no resting order without one):
   - UM `countdownCancelAll` per symbol while resting (30 s / 10 s);
   - CM **none** until K16 → IoC-only;
   - PM per K13;
   - options per K3;
   - spot none → IoC-only;
   - Binance Stocks none → DAY orders plus the mandatory gateway TTL.
3. **`exec.HALT` and `--halt-slot`**: unchanged.
4. **Disable the key in the Binance UI.** It works with the host unreachable.
5. **Operator flatten tool**, `exec-smoke --binance flatten --product … [--symbol …]`: reduce-only IoC, per product. It is never wired to a trigger.

### 3.12 Keys, configuration, interlocks

**Environment variables.** They live in `.env`, which a session never reads, prints or edits.
- `BINANCE_API_KEY`, `BINANCE_ED25519_SEED` (64 hex → `SecretKeyBytes`).
- Product-specific fallbacks, **only if a K-probe demands them**: `BINANCE_OPT_HMAC_SECRET` (K3), `BINANCE_PAPI_HMAC_SECRET` or an RSA key (K13), `BINANCE_EQUITY_*` (K15). They need `SecretKeyBytes<N>`; `ring` also signs RSA-PKCS1-SHA256.
- Hosts: `BINANCE_WSAPI_HOST`, `BINANCE_FUT_WSAPI_HOST`, `BINANCE_COINM_WSAPI_HOST`, `BINANCE_FUT_PRIVATE_WS_HOST`, `BINANCE_COINM_WS_HOST`, `BINANCE_PAPI_HOST`, `BINANCE_SAPI_HOST`, `BINANCE_EQUITY_WS_HOST`, plus the existing REST hosts.

**Interlocks.**
- `Scope::{Live, Demo}`. HL's is `{Live, Testnet}` (`exec-hyperliquid/src/config.rs:97-104`); here Demo Mode takes Testnet's role.
- **Network interlock** (BX-16): one network for the arm and for market data.

**`exec.toml`.** Unknown keys stay fatal.
- `request_budget_floor` is required only when `venues` names hyperliquid.
- `halt_on_margin_ratio_1e6` is required when `venues` names binance and a futures or options product is armed.

```toml
[exec.binance]
network = "mainnet"              # mainnet only inside the engine (BX-16)
account_mode = "classic"         # classic | pm | pm_pro          (O-BX2; asserted at boot, BX-19)
account_scope = "dedicated"      # dedicated | shared
owned_spot_assets = []           # shared scope only (O-BX2a, D3): e.g. ["BTC", "ETH"]
owned_usdm = []                  #   e.g. ["BTCUSDT", "XAUUSDT"]
owned_coinm = []                 #   e.g. ["BTCUSD_PERP"]
owned_option_underlyings = []    #   e.g. ["BTCUSDT"]
owned_equities = []              #   e.g. ["AAPL"]
products = ["usdm", "coinm", "spot", "options", "equity"]
options_write = 1                # O-BX10: shorts only where the contract says nakedSell (BTC/ETH today)
equity_session = "RTH"           # D5: REQUIRED when equity is armed — RTH | EXTENDED | 24H
equity_tokenize = 0              # D5: REQUIRED — semantics per K15
equity_quote = "USDC"            # D5: REQUIRED
recv_window_ms = 1000            # BX-6 — tightened after the clock is measured
stp_mode = "EXPIRE_TAKER"
countdown_ms = 30000             # O-BX13 (UM, while resting)
heartbeat_ms = 10000
orders_frac_1e6 = 800000
qtr_frac_1e6 = 800000
recon_every_ms = 60000
spin = 1

[exec.slot.N]                    # plus the existing required keys
max_symbols = 20                 # < 50 (venue rule, positions + open orders)
min_maker_ttl_ms = 5000          # ICR 5 s rule
halt_on_margin_ratio_1e6 = 600000   # D6: halt at 60 % maintenance / equity (venue liquidates at 95 %)
```

**Boot assertions**, which never mutate the account. Each one refuses the boot on a mismatch (BX-19):
- **The account mode matches `account_mode`.** The PM, classic and PM Pro discriminator is K17.
- One-way mode, shared by UM and CM; multi-assets off; leverage at or below the maximum.
- Key permissions.
- bStock permission groups.
- The equity disclaimer accepted; a 486410 refuses the `equity` product.
- Every owned instrument exists and is bound.

**Refusals discovered at first use.**
- The TradFi agreement: `-4411` is a per-instrument fatal refusal.
- Long & Short Sell mode: the first `-6057` is a per-underlying fatal refusal (K18).

Every account mutation lives in the operator tool `exec-smoke --binance setup`.

### 3.13 Observability

- **Counters.** `LiveArmCounters` per venue, published in `/state` `exec.arms.binance`.
- **Gauges:**
  - account mode and scope, owned-list sizes;
  - governor headroom per window;
  - QTR ratios;
  - **margin ratio per product**;
  - clock offset;
  - WS API round trip p50 / p99.9 (`core-latency`);
  - in-doubt count;
  - the owner table;
  - TradFi session per live symbol (D4).
- **TUI.** One line.
- **`binance-exec.pmlr` journal.** It records ACKs, rejects with code, fills with commission, asset and maker flag, cancels, expiries with reason, recon verdicts, margin samples, and equity `ACCEPTED` transitions. It is the source for **BX-14** and a `bn_shadow` reconciliation tool.

---

## §4 Laws: Binance analogues of E-1…E-9, carried into `docs/risk-policy.md` in BX11

- **BX-1 (E-1).** A live Binance slot never falls back to paper. An order the arm cannot route is refused and counted.
- **BX-2 (E-5).** The response is the **ACK**; the user stream is the **FILL**.
  - `TRADE_LITE` and `ORDER_TRADE_UPDATE` are deduplicated on (symbol, `tradeId`).
  - We request `ACK`, so no fill is ever booked from an order response.
- **BX-3 (E-3).** Signature payloads are proved by **known-answer vectors**: RFC 8032 §7.1, the official connectors' vectors, the docs' worked HMAC example, and RSA-PKCS1 vectors if K13 needs RSA. They run in `tests/` and the boot self-test, and are never taken on trust from prose.
- **BX-4 (E-4).** An instrument is live-tradable only if boot discovery **bound** it. It is never derived from a name at runtime.
- **BX-5 (numeric).** Integer quantization against the bound tick and step: BUY floors, SELL ceils, quantity floors.
  - Local violations are refused and never sent. A venue filter reject is a defect and feeds the streak.
- **BX-6 (clock).** Timestamp = local wall time + the **measured** offset. `recvWindow` is the stale-order guard, set from measurement, never 5 000 ms. `-1021` / `-5028` trigger a resync and count toward the streak.
- **BX-7 (ownership).** One live slot owns a (product, symbol) for the life of a boot. Venue-originated fills (liquidation, ADL, delivery, assignment) go to the owner.
- **BX-8 (the account's scope).**
  - On a **dedicated** account, anything the engine did not place is a reconciliation fact.
  - On a **shared** account, the artifact's owned list defines the engine's universe. **Foreign activity on an owned instrument is a drift HALT**; unowned instruments are invisible to recon (O-BX2a).
- **BX-9 (E-9).** The cid encodes epoch, slot and `client_oid`. Foreign-epoch `mv` orders are orphans, swept before `reconciled`.
- **BX-10 (E-7, adapted).** A requote uses the venue's one-request verb: spot amend or cancelReplace, UM/CM modify, PM `PUT`. Options and Binance Stocks have none, so cancel + new is the **documented exception**, counted against the product's order budget.
- **BX-11 (in-doubt).** A lost ACK is never resent. The order is resolved by query and stays booked until then.
- **BX-12 (governed budgets).** Every venue budget in §3.9 is governed fail-closed. A 429, 418, `-4400` or `-4401` is a halt observation, never a retry.
- **BX-13 (TTL).** The gateway enforces a maker's TTL, because Binance has no short GTD. On UM the minimum maker TTL is ≥ 5 s (ICR).
- **BX-14 (fees measured).** Commission from the stream writes `fees.toml`'s Binance rows once each class has **N commission-bearing fills**, counted in fills, never days.
  - The Binance Stocks per-order **minimum** ($0.35) needs a `fees.toml` grammar extension. It is recorded only as measured.
  - BNB burn is off.
- **BX-15 (scan failure).** A user-data frame the arm cannot scan is a halt observation, never a skip.
- **BX-16 (one network).** The arm's hosts and the market-data hosts are one network. Demo and testnet are used only by the standalone battery.
- **BX-17 (no resting order without a dead-man; D2 generalising O-BX3a).** A maker order is accepted only on a product and mode whose venue countdown is armed and working.
  - That is UM classic and PM Pro today. Options, PM and COIN-M depend on K3, K13 and K16.
  - Spot is IoC-only.
  - **Binance Stocks is the sole exception** (no IoC exists): DAY orders with a mandatory gateway TTL.
- **BX-18 (short options).** Shorts are allowed only where the contract's `nakedSell` is true (BTC/ETH), and only in Long & Short Sell mode.
  - Exposure is the **venue IM formula** (§3.5).
  - `-6057` is a per-underlying fatal refusal.
  - `RISK_LEVEL_CHANGE → REDUCE_ONLY` is `VenueLock`.
- **BX-19 (account mode).** The account mode and scope are **asserted at boot and watched in session**. A mismatch refuses the boot. A mid-session mode or permission change is `VenueLock`.
- **BX-20 (margin).** The arm computes every product's margin ratio. At or above `halt_on_margin_ratio_1e6`, or on any `MARGIN_CALL`, it raises `MarginRisk`. **A cancel still passes and no flatten happens** (O-BX9). The operator flattens by tool.

---

## §5 Phases BX0–BX14

**Standing rules for every phase:**
- Every gate in CLAUDE.md "Build / test / run" must be green at each phase boundary. **A red gate stops the lane.**
- **Each phase leaves a green tree with nothing armed**, the MX2 resting-point precedent.
- **Commits happen only on the operator's word**: one per phase, prefix `BX<n>:`, explicit-path staging, never pushed.
- **No dead code** (the operator's doctrine). A verb whose only use is gated behind an UNVERIFIED capability or a later ruling is **not built** until that gate opens. Example: spot amend is not built while spot is IoC-only (BX-17).
- LOC figures are estimates. They are scaled from `exec-hyperliquid` (16 513 lines in `src/`, 18 609 with `tests/`) and `exec-router` (7 377).

### BX0: Fix-now pass. FIRST STEP (O-BX12), main checkout (D1)

**What.** The four live-engine defects F1–F4, independent of the Binance arm.

**How it ships:**
- Each fix lands with its gates.
- Then **`cargo build --release -p cli`** (G0; check `stat -f '%Sm' target/release/multivenue-engine`).
- It goes live at the **next routine daily restart** (00:10 / 08:30 / 16:05Z). The engine is never stopped by hand.
- The engine runs its exec slot in paper (bin15 has been in paper since 2026-09-20), so F3 and F4 reach no live order at deploy time.

**F1: markPrice back on the routed path.**
- **Change:** `multivenue-engine.rs:2881-2888` switches to `/market/ws/{sym}@markPrice`, via a named path constant beside the spec builder. The host stays `BINANCE_FUT_WS_HOST`. **bookTicker stays on `/ws/`**, which still delivers (§1.2), until K7.
- **Tests:**
  - A spec-builder test pins both paths.
  - A live-shaped fixture is added. The measured frame carries a new `ap` field; the key-scanning parser tolerates it, and the fixture proves that.
- **Live smoke:** a standalone `#[ignore]` test, `crates/cli/tests/binance_md_live_smoke.rs`, in the `mexc_live_smoke` shape (its own `CARGO_TARGET_DIR`, **never stops the engine**). It requires ≥ 1 mark frame per 3 s on three perps and one dated future.
- **Docs:**
  - `docs/wire-format.md`: Binance Mark and Funding are live again.
  - `docs/migration.md`: captures regain the Mark and Funding events from the first post-restart boot, additively. The three standing pool guards (backtest schema-1 `188d18e3…`, detail sidecar `e3f6b8ef…`, audit-pnl `725d1d27…`) are computed over existing pools and do not move.
- **Post-restart tell** (corrected at BX0 — the `msgs − ticks` form was wrong, a mark frame counts in both): the new run dir's `bn-events.pmlr` grows ≈ 5 KB/s (a Mark row per USDⓈ-M instrument and a Funding row per perp every ~3 s), and the Binance tick rate rises ≈ (perps + dated) / 3 per second.

**F2: options WS moved to fstream.**
- **Step 0**, public, run from the Mac like MX0: **K6**. Confirm the documented `/public` option channels (`@bookTicker`, `@optionTicker…`) on an at-the-money strike, and the `/market` `<uly>@optionMarkPrice` and `!index@arr` shapes. Keep the frames as fixtures.
- **Change:**
  - The eapi slot (`multivenue-engine.rs:2890-2929`) builds `/market/stream?streams=<uly>@optionMarkPrice/…/!index@arr`. The measured array carries mark `mp`, index `i` and best bid/ask `bo`/`ao`.
  - If K6 shows the mark stream is not enough for quotes, a `/public/stream?streams=<opt>@bookTicker/…` slot is added.
  - `BINANCE_EAPI_WS_HOST` defaults to `fstream.binance.com` (`core-config/src/lib.rs:104-112` doc, `:209-210` default). `/eoptions/` is retired.
- **Parser** (`ingress-binance/src/eapi.rs`): an array-frame scanner.
  - Zero-alloc and span-borrowing.
  - One `OptSummary` per selected symbol; the rest skipped by table lookup.
  - A `never_panics` proptest and a cargo-fuzz target, `bn_eapi_mark_array`.
- **Operator note:** if the live `.env` pins `BINANCE_EAPI_WS_HOST` to nbstream, the operator edits it. Sessions never touch `.env`.
- **Live smoke:** the same `binance_md_live_smoke.rs`. Selected options yield `OptSummary` records carrying mark and index, with zero parse errors.
- **Docs:** the options channel map in `docs/wire-format.md`. CLAUDE.md's "BN eapi-WS is unreachable from this network" line is corrected; O-BX12 is the operator's ask for it.
- **Post-restart tell:** the boot line `binance: options mark-array slot … host=fstream.binance.com`; the new run dir's `bn-opt-summary.pmlr` grows past its 64 B header within seconds; `engine_ingress_binance_options_selected` > 0.

**F3: HL trait `cancel` / `modify` wired.**
- **Change** (`exec-hyperliquid/src/exchange.rs`):
  - The inherent `modify` is renamed **`modify_by_cloid`**, so no trait method and inherent method share a name.
  - `impl OrderDispatch for HlExchange` gains:
    - `fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> { self.cancel_by_cloid(req.sym, req.strategy_id, req.client_oid) }`
    - `fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> { self.modify_by_cloid(req.prev_client_oid(), req.order()) }`
- **Tests:**
  - A router-level test: a live cancel and a live modify through `RoutedDispatcher<Paper, HlExchange>` reach the arm on a loopback venue, and the ledger retires or renames the resting row.
  - **Break-and-watch:** with the override removed, the test fails with `Unsupported`.
- **Gates:**
  - `risk-reviewer` on the diff together with `docs/risk-policy.md` (E5's record becomes true through the router).
  - `exec-smoke` plus the lifecycle battery on testnet.
  - The `hl_*` alloc gates stay at 0 B/op.

**F4: `TCP_NODELAY` on every TLS socket (O-BX12a).**
- **Change:** `TlsTransport::connect` (`core-net/src/transport.rs:140-152`) calls `sock.set_nodelay(true)` right after `TcpStream::connect`. The same line goes in the plain-TCP `connect` (`:585`). This one choke point covers every ingress, HL's `/exchange` client and HL's user WS.
- **Tests:**
  - A loopback test asserts `nodelay()` on a freshly connected transport.
  - Every TLS loopback suite stays green.
  - The alloc and copy gates are unchanged.

**Gates for the pass:**
- `cargo nextest run --workspace`; alloc at 0 B/op (count unchanged); `make lint`; `make license-check`; `make copy-audit` new = 0.
- Fuzz `bn_eapi_mark_array` for 300 s; the standalone live smoke.
- Agents: `risk-reviewer` (F3), `zero-copy-auditor` (F2), `parser-property-tester` (F2).

**Size:** about 410 src + 670 test.

**BX0 status: LIVE since the 16:05Z routine restart, 2026-09-23** (main checkout, `0a1f6ff`; release binary built 12:29Z). Verified after the restart: the boot line `binance: options mark-array slot … host=fstream.binance.com … selected=64`; `engine_ingress_binance_options_selected` 64; within two minutes the new run's `bn-opt-summary.pmlr` held 143 KB and `bn-events.pmlr` 292 KB; 0 parse errors, 0 reconnects. The changed files are listed in §12.

**K6, step 0: measured 2026-09-23 ~11:04Z** from the Mac, public endpoints, no keys. The trimmed frames are the test fixtures and the fuzz seeds.
- **Options mark array.** `/market/stream?streams=btcusdt@optionMarkPrice/ethusdt@optionMarkPrice` pushes one unfragmented text frame per underlying about every 1 s: BTC 245.6 KB with 752 elements, ETH about 194 KB with 600.
  - An element is at most 336 B and carries all 20 keys (`s mp E e i P bo ao bq aq b a hl ll vo rf d t g v`), none empty.
  - Every element of a frame carries the same underlying index `i`. `s` is in the venue's case.
- **Mis-routing is silent.** The same streams on `/stream?…` or `/public/stream?…` upgrade (101) and then deliver nothing. No error arrives.
- **Per-option book.** `/public` `<symbol>@bookTicker` on at-the-money strikes: 8–28 frames per 15 s.
- **USDⓈ-M.** `/market/ws/btcusdt@markPrice` every 3 s, with two new keys, `ap` and `st`. Legacy `/ws/`: 0 frames. The dated `btcusdt_260925@markPrice` carries `"r":"0.00000000","T":0` (→ F1b).

**What landed, and where it departs from the text above:**
- **F1**, as written. `cli::bn_usdm_specs(host, name, sym)` returns both specs: bookTicker on `BN_USDM_BOOK_TICKER_PREFIX = "/ws/"`, markPrice on `BN_USDM_MARK_PRICE_PREFIX = "/market/ws/"` (`crates/cli/src/paper.rs`). The boot uses it.
- **F1b**, new, found at K6. `parse_mark_price` (`ingress-binance/src/lib.rs`) reports funding only for a parsed rate AND `T` ≠ 0. The round-trip proptest pins `has_funding == (T ≠ 0)`.
- **F2**, with five departures:
  1. **No `!index@arr`.** Every mark element carries `i`; it becomes `OptSummary.underlying`.
  2. **No `/public` bookTicker slot.** The array's `bo` / `ao` / `bq` / `aq` quote at the 1 s cadence the retired `@ticker` had. The real-time per-option book is a BX2 input.
  3. **Parser shape** (`ingress-binance/src/eapi.rs`; the WS half is rebuilt, the REST discovery half is byte-identical):
     - `EapiArrayCursor` walks the array one borrowed element at a time.
     - `eapi_elem_symbol` borrows the symbol. It is escape-aware: an escaped symbol is refused.
     - `EapiSymbolTable` (64 rows, filled at boot in the venue's case) resolves it. An unselected element is skipped on a table miss.
     - `parse_eapi_mark(elem, &mut EapiMarkFrame)` fills an 88 B frame in place. It is an out-param because a by-value return above 64 B is a hidden copy.
     - `EapiLane`, `parse_eapi_ticker`, `parse_eapi_index` and `EapiTickerFrame` are deleted.
  4. **Run loop** (`run_loop.rs`): one pass per frame (split, stream-suffix check, walk). Each selected element yields one `OptSummary`, plus a Tick when it has a bid or an ask. The options slot's receive buffer is 2 MiB (BTC measured 245.6 KB). The reject tap copies at most 512 B of a bad frame.
  5. **Fuzz target name:** `binance_eapi_mark_array`, not `bn_eapi_mark_array`, beside the rewritten `binance_eapi`.
  - `BINANCE_EAPI_WS_HOST` defaults to `fstream.binance.com`, as written.
- **F3**, as written except for the test. **The router-level test was replaced** by three arm-level trait tests plus the router's existing spy-arm tests. `RoutedDispatcher<P, L: OrderDispatch>` is generic, so its `self.live.cancel` / `.modify` are exactly the trait methods the arm tests call. The composed `RoutedDispatcher<Paper, HlExchange>` on a loopback venue is carried to BX3, where `LiveSet` rewires the router anyway. **The operator accepted this substitution** (O-BX12c). Break-and-watch: without the overrides, the tests read `Unsupported`. Gate 60 now drives the trait `modify`.
- **F4**, as written. `every_connect_turns_nagle_off` asserts `nodelay()` on a plain and a TLS transport. Break-and-watch: each assert failed without its line.
- **Alloc gates**, count unchanged at 64: the Binance run-loop gate gained an options phase (1 000 live-shaped pushes, 0 allocations); the option-analytics gate walks the live array.
- **Live smoke** `crates/cli/tests/binance_md_live_smoke.rs`, as planned. It also asserts that dated futures emit no Funding and that nothing reconnects.

**Gates on the final tree (2026-09-23, 12:24–12:29Z):**
- `cargo nextest run --workspace`: 2 738 run, 2 738 passed, 3 skipped (the `#[ignore]`d live smokes among them).
- Alloc: 64 of 64 at 0 B/op, from a fresh `Compiling bench`.
- `make lint` clean. `make license-check`: 436 files OK. `make copy-audit`: hits 33, baselined 33, new 0; the baseline did not grow.
- Fuzz: `binance_eapi_mark_array` 300 s, 11 854 732 runs; `binance_eapi` 120 s, 3 565 426 runs. No crash.
- Live smoke (public, no keys): three runs green — 60 s, 90 s, and 60 s on the final tree. The 90 s run: each perp 30 Mark and 30 Funding rows; the dated `BTCUSDT_260925` 29 Mark and 0 Funding; 16 options with 89–90 `OptSummary` rows each, plus BBO ticks; 0 parse errors, 0 reconnects, 0 drops.
- Agents: `risk-reviewer` on F3 named six paths the router can reach only since BX0. They stay UNVERIFIED until the maker goes live (`docs/risk-policy.md`, "BX0-F3"). `zero-copy-auditor` and `parser-property-tester` on F2: every finding was fixed (the out-param parse, the bounded reject tap, the escape-aware symbol, the `// COPY:` placement).
- **Not run:** `exec-smoke` and the testnet lifecycle battery (F3). This pass made no keyed venue call. They are the bar before bin15's maker goes live.

**Operator actions** (updated after O-BX12b–d):
1. **`.env`: done (O-BX12b).** It pinned `BINANCE_EAPI_WS_HOST=nbstream.binance.com`, which would have kept F2 dark. The session rewrote that one line to `fstream.binance.com` in place: mode 600 kept, the other 141 lines untouched, nothing read out. A project-wide IDE search at BX0 had returned that line.
2. **Commit: done (O-BX12d)**, staging the §12 paths explicitly.
3. **F3 test substitution: accepted (O-BX12c).**
4. **The regime word may move after the restart.** `regime.toml [refs] fund = "binance-usdm:btcusdt"`: FUND_SIGN and FUND_LEVEL get live funding prints for the first time since 2026-04-23. The boot seed carries price only.
5. **Before the maker goes live:** `exec-smoke --requote` and the six risk-reviewer paths.
6. **Zero-copy follow-ups: done (O-BX12d)**: the second BX0 commit, below.

**The zero-copy pass (O-BX12d, the second BX0 commit; live from the 00:10Z restart).** `ingress-binance` joined `make copy-audit`. The audit had been blind past the first test-only method in `routed.rs`, `exchange.rs` and `run_loop.rs`. It now skips a `#[cfg(test)]` item and nothing more, refuses what it cannot delimit, and proves both on fixtures first (`scripts/copy-audit-selftest.sh`).
- **Removed:** the 128 B `Option` returns on the bookTicker and markPrice hot path (now in-place parses); the sentinel SUBSCRIBE scratch (the request is now written from parts, its symbol read from the slot's own path, through the new `core_net::ws_write_text_frame_parts`); the Ping-echo scratch; `MultiConn`'s host/path `Vec`s; the `filterType` buffer. Discovery rows are parsed in place, and `options-select` sizes its output once.
- **Marked:** the designed boot copies.
- **Auditor:** PASS (`zero-copy-auditor`, Opus). Every cold finding was acted on.
- **Open:** core-net's rustls RX copy, and a possible allocation per record in rustls' buffered API. UNVERIFIED; it needs an allocation count over a real TLS loopback.
- Details: `docs/risk-policy.md`, "BX0 — `ingress-binance` joins the zero-copy gate". Gates: §12.

### BX1: Probes. The worktree opens here (O-BX15)

- **Worktree.** `~/trading-engine-multivenue-binance` on branch `binance` is created at the start of BX1. O-BX15 approves this git operation for this purpose; every later git operation still needs the operator's word.
- **Public half: done** (§1.2).
- **Keyed half:** K1–K5 and K13–K18.
  - Run with **Demo Mode keys** where a demo exists.
  - Where no demo exists (PM, PM Pro, Stocks: K13–K15), run **read-only mainnet calls** with the operator's key. No order is placed.
  - The research one-shots are git-excluded.
- **Exit:** every UNVERIFIED in §10 is closed, or carried as a named risk.

### BX2: Market data and discovery

**Discovery retention (F11).**
- `BnSymbolRow` gains minQty, maxQty, minNotional, the percent-price multipliers, the precisions, a **TradFi flag + `underlyingType`**, and a permission-group digest.
- `EapiOptionRow` gains `unit`, tick, step, minQty and `nakedSell`.
- The boot keeps a `BnInstTable` (§3.6).
- Files: `ingress-binance/src/{discovery,eapi}.rs`, `cli/src/paper.rs:8265-8404` (`run_bn`) and `:8416` onward (`run_bn_options`).

**COIN-M ingress (O-BX3).**
- **Universe:** `[binance] coinm = []` and `coinm_dated = []`, with ordinal bases 3072 and 3584.
- **Discovery:** `GET /dapi/v1/exchangeInfo`, keeping `contractSize`, `contractType`, the precisions and the filters.
- **Feeds** on `BINANCE_COINM_WS_HOST` (default `dstream.binance.com`):
  - dstream `/ws/<sym>@bookTicker` (measured working);
  - `<sym>@markPrice`, whose path is K10.
- **Descriptors:** `binance-coinm:`. The class is `Perp` or `Dated`; inverse is a row flag, not a new class. They are mirrored in `core-config/src/instrument_class.rs`, `claude_worker/instrument_class.py` and the shared TSV fixture.
- **Fees:** COIN-M shares the bn perp/dated rows, split only if the measured fees differ (BX-14).
- **Worker:** candles (`/dapi/v1/klines`), funding (`/dapi/v1/fundingRate`), the channel map, `pnl_report`.
- **fd budget:** two sockets per symbol, counted against the wrapper's 8 192.

**Equity discovery (O-BX1).** `GET /sapi/v1/equity/market/exchangeInfo` supplies the rows in block 4096 for `[binance] equities = []`. **There is no equity market-data ingress in v1**, because there is no member (§7 R17).

**K6 and K7 measurements.**

**Gates:** a proptest and a fuzz target for every new parser, loopback, alloc, and a standalone COIN-M live smoke.

**Size:** about 1 300 src + 1 150 test, including about 250 + 200 in the worker.

### BX3: Router, ledger, lanes

This is the risk core. `risk-reviewer` reviews the diff together with `docs/risk-policy.md`. **The merge order against HYPARB is ruled at BX3's start (O-BX14).**

**Crate changes:**

| crate | change |
|---|---|
| `clob-dispatcher` | `halt_signal_venue`, `try_next_retired`, `on_opt_summary` (all defaulted); `Retired` POD; `HaltSignal` gains `venue_lock` and `margin_risk` in its pad bytes (48 B unchanged) |
| `exec-router` | new `src/live_set.rs` (`LiveSet`, `MaybeArm`); the `RouteAlias` table in `routed.rs` (**not** in `ExecRoute`); per-slot merged signals; the retired drain; the Mark handling in `on_venue_event`; `ledger.rs` instrument rows with all six laws (§3.5); `halt.rs` `VenueLock = 10`, `MarginRisk = 11`; `counters.rs` |
| `engine` | `NUM_FILL_LANES` 4 → 5, `fill_lane_of(Binance) = Some(4)`, the 4-lane arrays at `engine/src/lib.rs:1435-1444`; forward `OptSummary` to the dispatcher |
| tests | the const asserts at `bench/tests/alloc_assertions.rs:1235` and `cli/tests/ruleset_engine_wiring.rs:41`; the 4-lane arrays at `alloc_assertions.rs:1323` and `ruleset_engine_wiring.rs:187` |
| `cli` | `Rings.fill` (`paper.rs:547`, `:593`); lane wiring (`multivenue-engine.rs:2621-2631`); `exec_boot.rs`: `LIVE_ARM_VENUES` gains Binance (re-pin `:662`), per-venue required keys, `halt_on_margin_ratio_1e6`; one `LiveSet` boot path; `cli/Cargo.toml:54` comment |
| `core-config` | `exec.rs`: `[exec.binance]` + slot keys (§3.12); `exec.toml.example` stays bootable (`exec_boot.rs:494-526`) |
| `engine-snapshot` | the `VenueLock` and `MarginRisk` words |

**Guard:**
- An HL-only boot is behaviourally identical.
- Every existing `routed` / `ledger` / `halt` test passes unchanged.
- The `routed_*` alloc gates stay at 0 B/op, and the E7 battery stays green.

**New tests** (break-and-watch on each guard):
- per-venue latch isolation;
- the alias applies only to the anchor;
- retirement releases the resting row;
- the signed, inverse and short-option projections never refuse a reducing order;
- turnover counts only the increasing part;
- settlement adds no turnover;
- a short option with no index is refused.

**New alloc gates:** `liveset_route_steady_state`, `routed_retired_drain_steady_state`, `ledger_instrument_rows_steady_state`, `ledger_price_feed_steady_state`.

**Size:** about 2 100 src + 2 800 test.

**Carried from BX0 (risk-reviewer, 2026-09-23).** `LiveSet` must forward `cancel` and `modify` to the arm that owns the order — F3 was exactly a forgotten override hidden behind a trait default of `Unsupported`. BX3 either makes `OrderDispatch::cancel`/`modify` REQUIRED methods (no default, so a forgotten override stops compiling) or adds the router + real-arm composition test BX0 substituted: `RoutedDispatcher<Paper, LiveSet<…>>` against scripted venues, with the ledger's retire and rename asserted.

### BX4: Signing and keys

- **New crate `signer-ed25519`**, backed by `ring` (O-BX5).
  - The keypair lives in an mlock'd box zeroed by hand on drop.
  - It signs once per WS session logon, and per request on the REST products.
- **An RSA-PKCS1-SHA256 path**, also from `ring`, only if K13 or K15 require RSA.
- **HMAC-SHA256** through `core-crypto`, plus a new `hex_encode`.
- **`SecretKeyBytes<N>`**, only if a 64-byte HMAC secret is needed.
- **Vectors:** RFC 8032 §7.1, the connectors' canonical payloads, the docs' HMAC example, and RSA vectors if used.
- **Boot self-test:** a flipped byte refuses the boot.
- `make license-deps` runs because `ring` becomes a direct dependency, with no new license.
- **Size:** about 450 src + 550 test.

### BX5: Transport (`core-net`)

`TCP_NODELAY` already landed in BX0.

**Reused as-is:** `http1.rs` (zero-alloc HTTP/1.1 codec with chunked support), `write_client_handshake_with_headers` (`ws_handshake.rs:337`), WS frame read/write, `subs::PendingTable`, `Keepalive::poll_client_heartbeat`.

**What BX5 adds:**
- A **keep-alive REST client** over `http1`: GET, POST, PUT and DELETE; custom headers (`X-MBX-APIKEY`); reconnect-on-close.
  - It serves papi, eapi, sapi and the fapi/dapi cold calls.
  - HL's `HlHttp` migration onto it is deferred.
- A **WS API session helper**: `PendingTable` request ids, server-ping echo, fragmented-frame refusal.

**Size:** about 500 src + 500 test.

### BX6: `crates/exec-binance` core

This phase is offline: loopback only.

| file | responsibility |
|---|---|
| `lib.rs` | module map; doctrine header |
| `config.rs` | `BnConfig::from_env(Scope)`; hosts per product and mode; key loading; BX-16 |
| `mode.rs` | `AccountMode {Classic, Pm, PmPro}`, `AccountScope {Dedicated, Shared}`, the owned tables per product (D3), the boot assertions (BX-19) |
| `inst.rs` | `BnInstHot` / `BnInstWire`; the alias-first lookup; the live-tradable predicate; magic reciprocals |
| `cid.rs` | the 27-byte codec; the foreign-prefix classifier |
| `num.rs` | quantize; the fixed-decimal renderer |
| `cmd.rs` | `BnCmd`, `BnEvt`, `Retired`; size asserts |
| `arm.rs` | **`BnArm: OrderDispatch`**, including the BX-17 / BX-18 refusals |
| `gov.rs` | every §3.9 budget |
| `margin.rs` | the per-product margin-ratio formulas (BX-20) |
| `gateway.rs` | the thread: loop, burst render + single flush, timers, rotation |
| `oot.rs` | open-order table (fixed 1 024, `current_client_oid`, in-doubt set) |
| `ttl.rs` | deadline wheel |
| `clock.rs` | venue offset (min-RTT) |
| `wsapi.rs` | the generic WS API session over `PendingTable` |
| `rest.rs` | the signed REST request builder: the query rendered in place, then the signature appended |
| `userstream.rs` | listenKey lifecycles; the fstream `/private`, `/pm`, `/pm-classic` and dstream sockets |
| `recon.rs` | the comparison law, including shared-scope filtering |
| `journal.rs` | the `binance-exec.pmlr` writer (cold) |
| `selftest.rs` | boot vectors |

Every scanner gets a `never_panics` proptest and a cargo-fuzz target: `bn_wsapi_frame`, `bn_user_event`, `bn_rest_body`, `bn_cid`.

**End-to-end loopback test:** arm → rings → gateway → a fake venue over rustls (`rcgen` certificates) → lane 4. It must book fills and retirements exactly.

**Size:** about 5 000 src + 3 800 test.

### BX7: Futures. UM + COIN-M × classic / PM / PM Pro, all together (O-BX2b)

**Build:**
- **Classic:** ws-fapi and ws-dapi sessions; fstream `/private` (UM) and dstream (CM) user streams; REST countdown, cancel-all and openOrders.
- **PM:** papi REST order, modify, cancel and allOpenOrders for UM and CM; the fstream `/pm` stream; papi recon; `uniMMR`.
- **PM Pro:** fapi/dapi, over WS API if K14 allows; the `/pm-classic` stream; `PM_PRO_ACCOUNT_UPDATE`.
- **Rules:**
  - TradFi trades 24/7 (O-BX11) and needs its agreement.
  - The inverse law applies to CM.
  - **CM is IoC-only until K16** (O-BX3a). **PM is IoC-only unless K13** finds a countdown (BX-17).
  - The account-mode discriminator (K17) and the boot assertions (BX-19).
- **Batteries:** `exec-smoke --binance {um,cm} --mode {classic,pm,pm_pro} --lifecycle`. They run on Demo where it exists. Otherwise they run on mainnet at minimum size, **armed by the operator only**.

**Size:** about 3 500 src + 2 600 test.

### BX8: Spot, including bStocks. IoC-only (BX-17)

**Build:**
- `session.logon` + `userDataStream.subscribe` on one socket.
- `order.place` (`LIMIT`+`IOC`), `order.status` (in-doubt), `openOrders.status` + `account.status` (recon).
- The unfilled-order mirror; the permission-group check.

**Not built** (no dead code): amend, cancelReplace, order.cancel and cancelAll. They arrive together when a spot dead-man exists or a ruling allows spot makers. **A halt's spot cancel-all is a confirmed no-op**: an IoC-only product has nothing resting.

**Exit, on Demo:**
- IoC fills reach lane 4.
- `binance:btcusdt` routes through the alias.
- The bStock permission check refuses a missing group.

**Size:** about 700 src + 600 test.

### BX9: Options, long and short BTC/ETH (O-BX10)

**Build:**
- REST place, cancel, batch cancel and cancel-by-underlying, signed per request (K3).
- **Makers only if K3 shows the countdown is usable by this account** (BX-17); otherwise IoC-only.
- **Writing:**
  - `nakedSell` gate;
  - the IM law (§3.5);
  - `-6057` is a per-underlying fatal refusal;
  - margin ratio = maintenance margin over adjusted equity (BX-20);
  - `RISK_LEVEL_CHANGE` → `VenueLock` where the account receives it.
- `exerciseRecord` settlement, including **short assignment**.
- A TradFi Options (XAU/XAG) flag, long-only by venue rule.

**Exit, on Demo:**
- A long IoC fills; a short IoC opens a BTC short.
- The IM exposure matches the venue's `initialMargin` field within one tick of the index.
- Expiry settles through `exerciseRecord`.
- If makers are allowed, one post-only order is placed and cancelled.

**Size:** about 1 700 src + 1 400 test.

### BX10: Binance Stocks arm. Full arm plus battery, no member (O-BX1, O-BX16)

**Build:**
- REST `/sapi/v1/equity/*` (exact paths per K15): place, cancel, cancel-all, open-orders, detail, trade/history.
- The disclaimer assertion.
- The `orderReport` stream, or a 1 s poll (K15).
- The `NEW → ACCEPTED → …` state machine, with `ACCEPTED` journaled.
- **DAY orders with the mandatory gateway TTL** (the BX-17 exception).
- The D5 required keys; the 200 / min governor.
- The per-order fee minimum, journaled (BX-14).
- Recon from open orders + positions (K15).
- The session sent per order, from the artifact.

**Battery** (`exec-smoke --binance equity --lifecycle`, mainnet minimum size, operator-armed; a Demo exists only per K15):
- A far-from-market DAY limit order, then cancel it.
- One marketable limit fill at minimum size.
- Clean recon, and `ACCEPTED` observed.

**Size:** about 1 400 src + 1 000 test.

### BX11: Kill switches, observability, docs

**Build:**
- Sweeps plus confirmation per product × mode; dead-man wiring.
- The **flatten tool** (O-BX9).
- `/state`, the TUI line, and the journal's wire format.
- **A "BX" section in `docs/risk-policy.md`** carrying BX-1…BX-20.
- Runbooks in `docs/local-setup.md`; `.env.example`; the `exec.toml.example` blocks (pinned bootable).
- CLAUDE.md CURRENT STATE, on ask.

**Drills:**
- Kill the gateway: `WsGap` fires, and the venue countdown cancels.
- Withhold recon: `ReconStale` fires.
- Force a 429 on Demo: `BudgetFloor` fires.
- Force a margin ratio over its threshold (loopback): `MarginRisk` fires.

**Size:** about 900 src + 700 test + docs.

### BX12: Gates

- `cargo nextest run --workspace`.
- **Alloc at 0 B/op:** the BX3 set plus:
  - `bn_arm_submit_is_zero_alloc`, `bn_gateway_render_flush_is_zero_alloc`;
  - `bn_wsapi_response_scan_is_zero_alloc`, `bn_user_event_to_fill_is_zero_alloc`;
  - `bn_recon_compare_is_zero_alloc`, `bn_margin_ratio_is_zero_alloc`;
  - `bn_rest_render_sign_is_zero_alloc`.
- **`make copy-audit`:** `crates/exec-binance` and `crates/signer-ed25519` join `scripts/copy-audit.sh:58-61`. Only the operator grows the baseline.
- **Fuzz:** `bn_wsapi_frame`, `bn_user_event`, `bn_rest_body`, `bn_cid`, `bn_eapi_mark_array` (BX0) and the COIN-M parsers, each for 300 s.
- **TLS loopback tests:** `bn_wsapi_tls_loopback`, `bn_userstream_loopback`, `bn_rest_loopback`, `bn_gateway_e2e_loopback`.
- `make lint`, `make license-check`, `make license-deps`.
- **Agents:**
  - Opus 5.5 (ruling O-8, 2026-09-23; Opus 5 before): `risk-reviewer` (BX0 F3, BX3, BX7–BX11), `zero-copy-auditor` (BX0 F2, BX5–BX10), `alloc-auditor`.
  - Sonnet: `parser-property-tester` (BX0 F2, BX2, BX6).

**Size:** about 1 600 lines of test (loopback and fuzz).

### BX13: Ramp. Battery only (O-BX6)

**How it runs.** The battery drives the **production dispatcher stack**, `RoutedDispatcher<Paper, LiveSet<Absent, BnArm>>`: the same types the engine monomorphizes. It runs from a scripted order list, so **no member exists and none is armed**. R0 therefore proves the risk gate, the ledger, the arm, the gateway and lane 4 on mainnet. The engine-loop wiring is proven by BX6's end-to-end loopback.

**R0 bars, per product × mode that is armed.** Minimum notional; counted in orders, fills and cycles, never hours:
- ≥ 20 lifecycle cycles where the product may rest orders; ≥ 20 IoC round trips where it may not.
- ≥ 5 fills.
- Clean recon for ≥ 3 consecutive cycles.
- The dead-man fired where one exists.
- The first measured commission rows (BX-14).
- Zero scan failures.
- Margin ratios sampled.

**After R0.** Arming a member is outside v1. It needs the member's own Stage-3 gate and then an artifact edit.

### BX14: Tokyo (separate lane, O-BX7)

Not in v1. The plan is unchanged from v1: an AZ-ID RTT sweep, `isolcpus`, a pinned busy-poll gateway, io_uring SQPOLL, kTLS, an Elastic IP, and **the whole engine moving**.

### Optional lanes, each needing its own ruling

- **BX-S, spot SBE responses** (schema 3:5; the SBE schema law).
- **BX-F, FIX SBE order entry plus drop copy**, only on a measured p99.9 gain.

v1 already absorbs COIN-M, PM and Binance Stocks.

---

## §6 Zero-copy, zero-allocation, HFT doctrine

**Copies.** The rule is that each unavoidable copy carries `// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>` within the eight lines above it.

| copy | bound | why it is unavoidable | alternative rejected |
|---|---|---|---|
| kernel ↔ user | one burst / one read | syscall boundary | io_uring registered buffers (BX14) |
| rustls plaintext window | ≤ 1 KiB per order frame | TLS encrypts from its own buffer | kTLS (BX14) |
| rx-tail compaction | ≤ one frame | a partial frame must survive the next read | — |
| WS client mask | payload | XOR **in place** | — |
| ring-slot publish: `BnCmd` / `BnEvt` / `Fill` / `Retired` | 64 / 64 / 64 / 16 B | POD by value | — |
| listenKeys (UM, CM, options, PM or PM Pro) → `[u8; 64]` each | 64 B, once per ≤ 60 min | the rx buffer is reused; the key is needed for the keepalive and the stream path | re-requesting per use (spends weight, races expiry) |

**Not copies, by construction:**
- Requests render into the final tx buffer.
- REST signatures are computed over the rendered bytes in place.
- Scanners return spans; prices and quantities are parsed in place.
- Ids are decoded by offset.
- Equity poll bodies are scanned in the rx buffer.

**Allocation.** Every table is fixed at boot. Steady state is **0 B/op**, gated as listed in BX12. UM's full `exchangeInfo` (2–3 MB) is read at boot only. Runtime status changes arrive as reject or expire reasons and through the news lane's Binance announcements.

**Doctrine checklist:**
- **No `dyn`** (`LiveSet` is enum dispatch).
- **No iterators in hot loops** (raw indices with `get_unchecked` behind `// SAFETY:`).
- **No release panics** (`debug_assert!`; `panic = "abort"`).
- **Every hot POD is `#[repr(C)]` + `Copy` and size-asserted**, and `align(64)` where contended.
- **Magic-reciprocal quantization; LUT rendering.**
- **SPSC rings** (`core-ring`).
- **Single writer per socket and per table.**
- **Busy-poll gateway.** It floats on macOS (CLAUDE.md), which is fine for v1 on the Mac (O-BX7).
- **Fail-fast:** a scan failure halts.
- **One syscall per command burst.**
- **No dead code** (§5).

---

## §7 Risks

| id | risk | mitigation |
|---|---|---|
| **R1** | **Latency from here.** WS API round trip is 102–137 ms, and the **host clock** runs +96 to +146 ms behind every venue. | v1 is battery-only (O-BX6). BX-6 clock law, with ws-api / ws-fapi / ws-dapi added to `latency_probe`. BX14 for anything faster. |
| **R2** | **QTR bans** (ICR 5 s, IFER, DR, ≥ 50 symbols). | Governor, `min_maker_ttl_ms`, `max_symbols`, an `apiTradingStatus` gauge, `VenueLock`. Battery sizes sit far below the recording thresholds. |
| **R3** | **REST weight is per IP and shared with the worker.** | WS API order entry where it exists; a reserved floor; worker bursts staggered while live. |
| **R4** | **IoC-only on spot, COIN-M (until K16) and PM (unless K13)** (BX-17). | Accepted by ruling (O-BX3a, generalised in D2). The maker verbs for those products are not built until the gate opens. |
| **R5** | **Options are narrow** (30 / 10 s, 100 / min, 400 weight, no amend). **Writing adds gap risk.** A gap through strike can exceed the IM before a recon, and the venue liquidates at 95 % maintenance. | The IM cap (O-BX10a). `MarginRisk` at `halt_on_margin_ratio` (default 60 %), sampled on **every** `ACCOUNT_UPDATE` / `MARGIN_CALL`, not only every 60 s. BTC/ETH only. No automatic flatten (O-BX9); the operator tool exists. |
| **R6** | **Venue churn**, already realised as F1 and F2. | BX0; live smokes before every deploy; the news lane's Binance feed; K-probes. |
| **R7** | **Account-mode drift.** A PM switch **strips the Futures permission from existing keys**; hedge mode or leverage can be changed by hand. | Boot assertions; `VenueLock` on a mid-session change (BX-19). PM needs a new key after activation (§1.6). |
| **R8** | **Key compromise.** | The Ed25519 seed never leaves the Mac. No withdraw permission. IP allow-list. Disable in the UI. |
| **R9** | **A dynamic home IP breaks the IP allow-list.** | A static egress IP, or BX14's Elastic IP. K9. |
| **R10** | **The ledger change regresses the HIP-4 law.** | `FamilyRow` untouched; the byte-identity guard; `risk-reviewer`; break-and-watch. |
| **R11** | **The alias touches the E-1 path.** | The anchor is checked by full `SymbolId`; paper is untouched; `fill.rs:1068` stays true. |
| **R12** | **TradFi trades 24/7** (O-BX11): EWMA off-hours pricing, weekend deviation limits, dividends, per-symbol funding. | Accepted by ruling. Sessions are observed and journaled (D4); `fundingInfo` is read per symbol. |
| **R13** | **Eligibility.** | Confirmed (K12). `-4402` stays a fatal boot refusal. |
| **R14** | **Scope:** about 35 k lines. **"All modes together" (O-BX2b) is the longest route to the first live order.** | Accepted by ruling. Hard phase gates; BX0 ships value first. |
| **R15** | **Parallel lanes** (HYPARB). | Ruled at BX3 (O-BX14). The worktree (O-BX15) isolates the edits until then. |
| **R16** | **PM order entry is REST-only.** Higher latency, its own 6 000 / 1 200 pool, and a new key after activation. | Measured in BX1 (K13). PM is IoC-only unless a countdown exists. |
| **R17** | **Binance Stocks is a broker path.** Acks wait on the broker (`ACCEPTED`), quotes are about 5 s stale, the engine has **no equity market data**, and there is a $0.35 per-order fee minimum. | v1 is battery-only (O-BX16). An equity quote ingress is a **prerequisite before any member**. The fee grammar extension is recorded (BX-14). |
| **R18** | **Shared account.** The operator's own trades on an owned instrument **halt the arm by design** (O-BX2a). | Curate the owned list; the halt is the safe failure. |
| **R19** | **COIN-M P&L is in coin.** | The session bound converts at mark; the margin ratio is per asset. |

---

## §8 Non-goals: this plan changes nothing here

- **No member goes live (O-BX6).** No AI-promoted member, no `serve`, no Anthropic API. The Stage-3 gate is untouched.
- **No automatic flatten (O-BX9).**
- **No maker paths where no venue dead-man exists** (BX-17). The unused verbs are not built.
- No spot margin or borrow. No conditional or algo orders. No RPI, no SOR, no order lists, no pegged or iceberg orders.
- **XAU/XAG options are long-only**, by venue rule. **US stock options are out** (no API).
- **No Tokyo in v1 (O-BX7). No SBE or FIX in v1** (optional lanes).
- **The HL arm changes only through BX0's F3 and F4.** The `HlHttp` migration is a separate ruling.
- No cloud service on the trading path. **No git operation** beyond the worktree approved by O-BX15, without the operator's word. **No research material in git.**

---

## §9 Sequencing and effort

| phase | depends on | est. LOC (src + test) | note |
|---|---|---|---|
| **BX0 fix-now pass** | — | 410 + 670 | **first step**; main checkout; live at the next restart |
| BX1 probes | BX0 | 0 | the worktree opens (O-BX15) |
| BX2 market data + discovery (+ COIN-M ingress) | BX1 | 1 300 + 1 150 | |
| BX3 router / ledger / lanes | BX1 | 2 100 + 2 800 | risk core; HYPARB order ruled here |
| BX4 signing / keys | BX1 (K3, K13, K15) | 450 + 550 | beside BX3 |
| BX5 transport | — | 500 + 500 | beside BX3 |
| BX6 exec-binance core | BX2–BX5 | 5 000 + 3 800 | offline, loopback |
| BX7 futures × 3 modes | BX6 | 3 500 + 2 600 | O-BX2b |
| BX8 spot + bStocks | BX6 | 700 + 600 | IoC-only |
| BX9 options (long + short) | BX6 | 1 700 + 1 400 | |
| BX10 Binance Stocks | BX6 | 1 400 + 1 000 | |
| BX11 kill / observability / docs | BX7 | 900 + 700 | |
| BX12 gates | each phase | + 1 600 | |
| BX13 ramp | BX7–BX12 | battery artifacts | battery-only |
| **total** | | **≈ 17 960 src + ≈ 17 370 test ≈ 35 k lines** | |

**Critical path:** BX0 → BX1 → (BX2 ∥ BX3 ∥ BX4 ∥ BX5) → BX6 → BX7 → BX11 / BX12 → BX13. BX8, BX9 and BX10 follow BX6 in any order.

---

## §10 Open questions and UNVERIFIED items

| id | question | closed by |
|---|---|---|
| **K1** | Spot `session.logon` + `userDataStream.subscribe`: one socket for responses and `executionReport`? | BX1 (Demo) |
| **K2** | UM: the ACK vs `TRADE_LITE` ordering, and `TRADE_LITE`'s latency lead | BX7 battery |
| **K3** | Options: is Ed25519 accepted on eapi REST? The cid charset? **Is `countdownCancelAll` usable by a non-MM account?** The STP default? | BX1 (Demo) |
| **K4** | TradFi: the error before the agreement is signed; `tradingSchedule` shape | BX7 battery |
| **K5** | `apiTradingStatus` shape; the Regular-tier QTR thresholds | BX1 |
| **K6** | The documented option channels on fstream `/public` / `/market`, at an at-the-money strike | **BX0 step 0** (public) |
| **K7** | bookTicker rate on legacy `/ws/` vs `/public/`, ≥ 20 disjoint short windows | BX2 (public) |
| **K8** | Any spot cancel-on-disconnect? None is documented. | BX1 |
| **K9** | IP-restriction policy for trading-enabled keys | operator |
| **K10** | dstream markPrice path shape; whether fstream's merged UM+CM universe (`st` field) can carry COIN-M | BX2 (public) |
| **K11** | A Demo WS API host for futures? | BX1 |
| ~~K12~~ | ~~Eligibility~~: **closed**, all products enabled | operator, 2026-09-23 |
| **K13** | **PM:** Ed25519 on papi, or HMAC/RSA only? Any papi countdown or batch? A PM demo? | BX1 (read-only mainnet) |
| **K14** | **PM Pro:** is the fapi/dapi **WS API** usable? | BX1 (only if eligible) |
| **K15** | **Binance Stocks:** exact paths (docs vs Go SDK); key types; does `orderReport` exist; cid charset; modify; positions endpoint; **`tokenize` semantics**; a demo? | BX1 |
| **K16** | **Has COIN-M `countdownCancelAll` been restored** since 2026-06-29? | BX1 → gates CM makers (O-BX3a) |
| **K17** | The API discriminator for classic vs PM vs PM Pro | BX1 |
| **K18** | Is Long & Short Sell mode detectable by API, or only through the first `-6057`? | BX1 (Demo) |

---

## §11 Sources

**Measured.** The §1.2 probe, 2026-09-23 ~09:35Z, from the Mac: public endpoints only, no keys. The live engine's `/metrics`.

**Operator rulings.** AskUserQuestion, 2026-09-23, five rounds (§0).

**Venue documentation**, fetched 2026-09-23:
- **Spot API:** CHANGELOG (newest entry 2026-09-18), WebSocket API, User Data Stream, FIX, SBE, FAQs (amend keep priority, STP, order-count decrement, price-range rules), Demo Mode.
- **USDⓈ-M:** general info, WS API general info (2026-09-22), user data, common definitions, error codes, change log (→ 2026-09-21), the trade pages (New / Modify / Batch / Auto-Cancel / TradFi-Perps / Trading-Schedule / Algo), and the WebSocket change notice plus announcement `ebf9b0aa9eca4ff3804eef6fb09ba32a` (2026-03-06).
- **COIN-M:** general info, WS API, user data, trade, and the **CM-UM Integration Notice**.
- **Portfolio Margin:** papi general info, trade, user data, rate limit; the **PM API FAQ `ccf04078…` (2026-01-20)**, activation FAQ `7ee6b3f6…` (2026-02-20), trading rules (2026-05-19), **PM vs PM Pro FAQ `ee2d73ec…` (2026-01-02)**, PM Pro API FAQ `1fc7ab7c…`, PM Pro user data.
- **Quantitative Trading Rules FAQ** (2026-08-31).
- **Options:** general info (2026-09-22), trade, market-maker endpoints, user data (2026-09-22), error codes, change log; the migration announcement `95f0d5ff…` (2025-12-10); writing opened `e80dd709…` (BTC, 2025-08-04) and `7372ff01…` (ETH, 2026-01-16); **the options margin FAQ `1ceb77f8…` (2026-07-28)** and the Clearing Procedures (the IM and MM formulas); commodity options `068cb34c…` (2026-07-29).
- **Stocks:** TradFi perps launch `ecf7318c…` and the batches; bStocks (PR Newswire, 2026-06-12); **Binance Stocks `developers.binance.com/docs/stocks/general-info`** and its trade / market-data / account pages; announcement `8c8fb680…` (2026-06-01); US stock options (2026-09-01).
- **Restricted locations:** the `dev.binance.vision` thread `13879`.
- **Third-party, used only where marked:** QuantConnect Lean #9797, freqtrade (`-4411`), arbitron / DCD (Tokyo AZ), Tardis (options channels).

**Repo facts** were read directly at the cited lines through the RustRover MCP. They were re-verified by an independent pass: 43 of 46 citations exact, the rest corrected. `docs/arch/` and `docs/research/` were **not** opened.

---

## §12 Progress log

- **2026-09-23, v1.** The plan was drafted.
  - BX0 public probes run from the Mac; the repo surveyed read-only.
  - Findings F1–F13 recorded.
  - Two verification passes: 43 of 46 repo citations exact; 15 of 15 venue claims confirmed. Corrections applied.
  - Placed at `docs/binance-exec-plan.md`, untracked.
- **2026-09-23, v2.**
  - **20 operator rulings recorded** in five AskUserQuestion rounds (§0).
  - **The fix-now pass is now BX0, the first step** (O-BX12).
  - Scope widened to COIN-M, all account modes (classic dedicated or shared, PM, PM Pro, built together), options writing on BTC/ETH, and the Binance Stocks arm.
  - **No member goes live**; v1 ramps batteries only.
  - The work moves to a worktree from BX1.
  - Phases renumbered BX0–BX14. Laws BX-17…BX-20 added. Derivations D1–D6 stated.
  - Nothing built, no git operation, engine untouched.
- **2026-09-23, BX0 built and committed** (main checkout).
  - K6 measured the routed shapes (§5 BX0). F1b was found there and fixed with F1.
  - F1–F4 fixed. F2 departs from the plan text in five places; F3's router-level test was substituted, pending the operator's acceptance (§5 BX0).
  - Every gate green on the final tree; fuzz and three live smokes green. The release binary was built at 12:29Z.
  - Docs: `docs/wire-format.md`, `docs/migration.md` (two entries), `docs/risk-policy.md` ("BX0-F3"), CLAUDE.md, `.env.example`, this plan.
  - **Changed files (22):** `.env.example`, `CLAUDE.md`, `crates/bench/tests/alloc_assertions.rs`, `crates/cli/src/bin/multivenue-engine.rs`, `crates/cli/src/lib.rs`, `crates/cli/src/options_manifest.rs`, `crates/cli/src/paper.rs`, `crates/cli/tests/binance_md_live_smoke.rs` (new), `crates/core-config/src/lib.rs`, `crates/core-net/src/transport.rs`, `crates/engine/src/lib.rs`, `crates/exec-hyperliquid/src/exchange.rs`, `crates/ingress-binance/src/eapi.rs`, `crates/ingress-binance/src/lib.rs`, `crates/ingress-binance/src/run_loop.rs`, `docs/binance-exec-plan.md`, `docs/migration.md`, `docs/risk-policy.md`, `docs/wire-format.md`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/binance_eapi.rs`, `fuzz/fuzz_targets/binance_eapi_mark_array.rs` (new). The fuzz corpus seeds are git-ignored.
  - Operator rulings after the build (§0): the session switched the live `.env` line to fstream (O-BX12b); F3's test substitution accepted (O-BX12c); commit BX0, and fix the zero-copy follow-ups (O-BX12d).
  - Git: before the operator's word, one read-only `git status` / `git log`; then the BX0 commit `0a1f6ff`, explicit paths. The engine was never stopped.
  - Live at the 16:05Z routine restart, verified (§5 BX0).
- **2026-09-23, BX0 zero-copy pass** (the second BX0 commit, O-BX12d).
  - `ingress-binance` is in `make copy-audit`'s scope. The sweep's `#[cfg(test)]` blind spot is fixed and self-tested (`scripts/copy-audit-selftest.sh`, which the target runs first). The baseline went from 33 to 32 entries (debt paid) and did not grow.
  - bookTicker and markPrice parse in place. The sentinel SUBSCRIBE is written from parts (`core_net::ws_write_text_frame_parts`). The Ping-echo, `MultiConn` host/path and `filterType` copies are gone. Discovery rows are parsed in place, and `options-select` is sized once.
  - `zero-copy-auditor`: PASS, with every cold finding acted on. The rustls RX copy and a possible allocation are escalated (UNVERIFIED).
  - Gates: clippy clean; nextest 2742 passed (3 skipped); alloc 64/64 at 0 B/op; license-check OK; copy-audit hits=32 baselined=32 new=0 paid=0 after its self-test; fuzz binance_book_ticker 60.1 M, binance_mark_price 15.2 M, binance_exchange_info 3.83 M and binance_eapi 3.22 M runs at 120 s each, no crash; live smoke 60 s green (0 parse errors, 0 reconnects; the spot sentinel drew 695 prints and 11 756 book ticks inherited their stamp), now with a spot sentinel slot.
  - Release binary built 16:18Z, live from the 00:10Z routine restart.
  - **Changed files (19):** `CLAUDE.md`, `Makefile`, `crates/bench/tests/alloc_assertions.rs`, `crates/cli/src/paper.rs`, `crates/cli/tests/binance_md_live_smoke.rs`, `crates/core-net/src/lib.rs`, `crates/core-net/src/ws_frame.rs`, `crates/ingress-binance/src/discovery.rs`, `crates/ingress-binance/src/eapi.rs`, `crates/ingress-binance/src/lib.rs`, `crates/ingress-binance/src/run_loop.rs`, `crates/options-select/src/lib.rs`, `docs/binance-exec-plan.md`, `docs/risk-policy.md`, `fuzz/fuzz_targets/binance_book_ticker.rs`, `fuzz/fuzz_targets/binance_mark_price.rs`, `scripts/copy-audit-baseline.txt`, `scripts/copy-audit-selftest.sh` (new), `scripts/copy-audit.sh`.
