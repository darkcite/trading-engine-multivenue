# MEXC Ingress Plan (MX0–MX9) — spot · futures · stocks · tradfi · funding

**Status: DRAFT — authored 2026-09-20 on operator request. UNCOMMITTED, UNSCHEDULED.**
Adds a SEVENTH market-data venue (`VenueId::Mexc = 7`, tick lane 6). Data-only with an
exec-ready shape (operator ruling O-MX1). Opens nothing: no Anthropic API, no `serve`,
no Stage-3 gate, no execution.

Reference precedent throughout: **Bybit / WS9** (`VenueId::Bybit = 6`, landed 2026-08-29)
for venue mechanics, and **BST** (`docs/arch/binance-stocks-plan.md`, BST0–BST6 executed
2026-08-29) for equities/TradFi doctrine — equities ride the EXISTING `Spot`/`Perp`
instrument classes; no new `InstrumentClass` is created.

---

## §0 Operator rulings (recorded 2026-09-20, via AskUserQuestion)

| id | ruling |
|---|---|
| **O-MX1** | **Data-only now, exec-ready structure.** Capture + backtest + research only. No `exec-mexc`, no keys, no risk gate, no `ExecMode` arm wired. The crate is *shaped* so an exec arm can be added later without rework (§4 D10), but nothing live. |
| **O-MX2** | **All four classes in v1**: crypto spot + crypto perps + xStocks spot equities + TradFi perps. One universe, one venue, four lanes. |
| **O-MX3** | **The protobuf decoder lives in `core-parse`** as a new varint/PB module beside the JSON scanners, with its own proptest + cargo-fuzz target. Reusable by any future PB venue; the venue crate stays thin. |

Derived, not ruled (stated here so a later reading does not mistake them for operator law):
MEXC is **not** added to `build_ai_universe` (the Bybit precedent deliberately omitted the
sixth venue); MEXC **is** added to `caps_of_descriptor` on both sides of the Rust↔Python
mirror, because the funding lane needs `CAP_FUNDING` to be expressible.

---

## §1 Product facts — MEASURED 2026-09-20, 09:43–09:52Z

Everything in this section came from live bodies on the operator's machine, not from a
doc. Sources in §10. **Pitfall 7 applies: these are fixtures-from-live, and they must be
re-smoked with `--raw-tap` before any parser is trusted.**

### 1.1 Spot market data — Protocol Buffers, not JSON

| fact | measured value |
|---|---|
| WS endpoint | `wss://wbs-api.mexc.com/ws` |
| Wire format | **Protocol Buffers (proto3), WS opcode BINARY** |
| Subscribe | ONE text frame `{"method":"SUBSCRIPTION","params":[…]}` |
| Ack | ONE text frame **enumerating per-param outcomes** — `{"id":0,"code":0,"msg":"Subscribed successful! [a,b,c]. Not Subscribed successfully! [d]. Reason： Blocked! "}` |
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

### MX5 — Discovery + universe config

`src/discovery.rs` — boot REST against `GET /api/v3/exchangeInfo` (spot: `status == "1"`,
`permissions` contains `SPOT`, record `baseAssetPrecision`/`quotePrecision`) and
`GET /api/v1/contract/detail` (perp: `state == 0`, `apiAllowed == true`, record
`contractSize`/`priceUnit`/`volUnit`). **No equity-specific branch** (§1.3). A configured
symbol the venue does not list is a fatal boot misconfiguration (the Deribit-combo
precedent). Note `exchangeInfo` is **1.6 MB**; parse it streaming out of the rx buffer,
never buffer-then-scan.

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

**D8 — `default_stale_after_ms`.** The venue byte indexes ONE table, but the two classes
have very different cadences (spot ~10 ms; futures ~270 ms median / 695 ms max). A single
value must cover the slower class: **provisionally 1 500 ms**, revised from the MX9
measurement. If that proves too coarse for spot, the honest fix is the existing operator
override `--stale-after-ms mexc:<ms>`, not a per-class table (which no venue has today).

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
| **R4** | **Latency.** 128 ms REST RTT and a ~123 ms implied Δ **from the Cowork VM**. If the Mac reproduces it, MEXC is 3× Bybit's path. | MX9 measures on the Mac before any Δ is applied. Under O-MX1 this changes nothing that is built — it changes what may ever be built on top. |
| **R5** | **The equity basis (§2 T2) is probably not an edge.** `indexOrigin` shows MEXC's equity index partly derives from Binance's TradFi perp; the xv verdict closed exactly this shape; weekend equity prints are thin and drifty (the BST §5 flag); a 60 bps gap against 5 bps fees invites a size or borrow constraint that is not in the quote. | Capture only. Any study goes to the vault under the `xv` authoring laws (≥ ~4.5σ entry) and the size-comparison traps. **Nothing in this plan trades it.** |
| **R6** | **`capture_catalog.rs` already omits venue 6**; venue 7 widens a live blind spot. | Fixed in MX9 §6 as part of the lane, not deferred. |
| **R7** | fd budget — a launchd agent is born with a 256-fd soft limit. | The wrapper already raises it to 8192; ~5 more sockets is noise against Binance's 254. |
| **R8** | `docs/wire-format.md:23-26` still lists six venues (Bybit was never added), so the doc is already wrong before MEXC arrives. | Fixed in MX9 §5. |
| **R9** | **Symbol-name collision across venues.** `BTCUSDT` now exists on Binance spot, Bybit spot AND MEXC spot; `BTC_USDT` on MEXC perp. | `SymbolId` namespaces by venue byte; descriptors namespace by prefix (`mexc:` / `mexc-perp:`); `check_unique` runs cross-venue at boot. Human-facing reports are the hazard — keep descriptors, never bare symbols. |
| **R10** | **MEXC lists and delists aggressively** (1 966 spot symbols, many thin); `state`/`apiAllowed` can flip between boots. | Boot discovery refuses a configured symbol the venue no longer lists (fail-fast). Prefer the liquid majors + the named equity/TradFi set; do not chase the long tail. |
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

---

## §9 Open questions

| id | question | gated by |
|---|---|---|
| **Q-MX1** | Spot `tradeId` is `"730292425431437318X0_730292425431437319X0"` — not an integer. Parse the leading digits as `venue_seq`, or declare spot trades seq-less (`venue_seq = 0`, the Bybit opt-out) and apply the §6.2 chain law to the **book** only? | operator ruling; affects D3 and the `wire-format.md` label paragraph |
| **Q-MX2** | `push.ticker` at 2.75 s is the only source of `fundingRate`/`holdVol`/`indexPrice`. Subscribe it for **every** perp (cost: 1 sub each, against a ≥ 40 cap) or only for the funding-bearing subset a strategy actually reads? | operator; affects connection count |
| **Q-MX3** | `nextFundingTime` is absent from `push.ticker`. Seed `Funding.v1` from REST at boot and advance by 8 h, or emit `v1 = 0` and let the worker funding lane own the schedule entirely? | D4 |
| **Q-MX4** | `fees.toml` under O-MX1 has no measured fill to derive from (§1.5). Record the venue's published per-symbol rates as UNVERIFIED, or leave MEXC out of `fees.toml` until it is ever armed (and accept that `pnl_report` treats an unknown venue as fatal)? | operator; affects MX7 |
| **Q-MX5** | Which instruments actually go in the first configured boot? §1.3 gives 1 966 + 1 174 candidates; a smoke universe should be ~14. Proposal: `BTCUSDT ETHUSDT SOLUSDT AAPLXUSDT SPYXUSDT` spot + `BTC_USDT ETH_USDT XAU_USDT USOIL_USDT EUR_USDT SPY_USDT AAPLSTOCK_USDT` perp. | operator |
| **Q-MX6** | MEXC in `build_ai_universe`? Bybit was deliberately excluded; including MEXC would let AI rulesets address it. | operator; Stage-3 adjacent |
| **Q-MX7** | Does MEXC join the news lane (`news/sources.py` has per-venue announcement + status parsers)? MEXC delists aggressively (R10), which is exactly what that lane is for. | operator; ~90 LOC |

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

---

## §11 Progress log

- **2026-09-20** — MX0 executed (live probes, §1). Plan authored. Operator rulings O-MX1
  (data-only, exec-ready shape), O-MX2 (all four classes), O-MX3 (PB decoder in
  `core-parse`) recorded via AskUserQuestion. Seven open questions Q-MX1…Q-MX7 raised.
  Nothing built; no file outside this document touched; no git operation performed.
