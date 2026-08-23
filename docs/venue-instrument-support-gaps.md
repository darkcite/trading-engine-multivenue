# Venue and instrument support — required additions

**Status: inventory. Nothing here is authorized, designed, or sequenced.** This
is a list of market-data, instrument-class and reference-data support that does
not exist in the engine today. Every "not present" was verified against the
code, with the file path.

Date: 2026-08-23.

---

## 1. The add list

### New venue

- [ ] **Bybit** — nothing exists: no crate, no `VenueId` variant, no config
      section, no parser, no fuzz target. Comprises:
  - [ ] `VenueId::Bybit = 6` (5 is `Ai`; 255 reserved for `SYMBOL_ID_NONE`'s
        venue byte; `VenueId::from_u8` currently rejects everything ≥ 6)
  - [ ] `crates/ingress-bybit` — v5 public WS: `tickers` (BBO + funding rate +
        next funding time + mark), `publicTrade`, `orderbook` behind a
        `--bybit-depth` flag
  - [ ] REST discovery — `/v5/market/instruments-info` (liveness, tick/lot,
        contract class) and `/v5/market/tickers` (24h turnover, OI)
  - [ ] Handwritten byte-scanner parser (no `serde_json`), property test, and
        `bybit_ws_frame` / `bybit_instruments` fuzz targets
  - [ ] Capture: `bybit-ticks.pmlr` / `bybit-events.pmlr`; `bybit:` prefix in
        `instrument-manifest.tsv`
  - [ ] `[bybit]` universe section
  - [ ] A sixth `claude-worker` candle lane (`candles.py` has five today)

### New market-data channels

- [ ] **Binance USDM `<sym>@markPrice`** — carries mark price, index, funding
      rate and next-funding time in one frame. The spot and USDM lanes are
      `bookTicker` only (`crates/ingress-binance/src/run_loop.rs:12,181`)
- [ ] **Deribit: emit `current_funding_1e9`** — the parser already populates it
      (`crates/ingress-deribit/src/lib.rs:531`); the capture site writes only
      mark price and OI, dropping it

### New instrument classes

- [ ] **Binance dated futures (`CURRENT_QUARTER`)** — symbols are discoverable
      (`BN_DISCOVERY_SYMBOL_MAX` is sized to include delivery names), but
      `contractType` and `deliveryDate` are never parsed (they appear only in a
      discovery test fixture) and there is no dated-future BBO lane
- [ ] **Deribit perp-vs-dated gating** — `settlement_period` is parsed
      (`discovery.rs:489`) but never used; perps and dated futures currently
      receive identical channel treatment
- [ ] **Deribit spot** — only `kind=future` and `kind=option` pages are fetched

### New reference data (REST)

- [ ] **24h quote volume** — captured on **no venue**. `/api/v3/ticker/24hr`,
      `/fapi/v1/ticker/24hr`, `/v5/market/tickers`, OKX `tickers`: none exists
      anywhere in the repo, and `OkxChannel` has no such variant at all
- [ ] **Open interest** — needed on Binance (`/fapi/v1/openInterest`), Bybit and
      OKX. Present on WS for Deribit (`ticker`) and Hyperliquid
      (`activeAssetCtx`); no REST OI fetch exists on any venue
- [ ] **Tick / lot / contract size** — missing on Binance (`BnSymbolRow`
      captures only `symbol` + a trading flag), Bybit and Hyperliquid. Present
      on OKX, Deribit and Polymarket

### New data series

- [ ] **Deribit DVOL** (volatility index) — absent from the repo entirely: no
      `get_volatility_index_data`, no WS channel, no descriptor

### New engine plumbing

- [ ] **A funding carrier from ingress to `Strategy`.** Funding exists only as
      capture-only `ChannelEvent` written to `<venue>-events.pmlr`; there is no
      funding type on any engine ring, and the `Strategy` callback surface is
      `Tick` / `Signal` / `Fill` / `AiCmd` / `RuleTable` / timer
- [ ] **L2 depth to a `Strategy`.** The OKX and Deribit book channels are
      header-only capture, and `book-builder` is top-of-book fed by the Tick
      stream; there is no depth callback

### New offline / historical lanes

- [ ] **Funding history backfill** — does not exist on any venue.
      `claude-worker` has zero occurrences of "funding"
- [ ] **Bybit candles** — the sixth REST candle lane (see the venue add above)

### Constant and doc edits entailed by a sixth venue

- [ ] `VENUE_LABELS` — `crates/cli/src/backtest.rs:81` **and** its private
      mirror at `crates/cli/src/audit_replay.rs:37`
- [ ] `TRADEABLE_VENUES = 5` — `crates/cli/src/backtest/fill.rs:86`
- [ ] `MODEL_VENUE_LABELS: [(&str, VenueId); 5]` —
      `crates/cli/src/backtest.rs:86` — plus `ModelParams.fee_bps: [(u32,u32); 5]`
      and `latency_ns: [_; 5]`, and the hand-rendered JSON and stderr model
      lines that iterate them
- [ ] `audit-replay`'s venue × channel coverage matrix
- [ ] `docs/wire-format.md` and `docs/migration.md`
- [ ] Optional: the capture catalog's per-channel coverage row. The catalog
      deliberately aggregates non-tick files into per-run `other_files` counts
      and names "a dedicated coverage row" as its extension point — a new
      channel is size-visible for free but not channel-visible

---

## 2. Venue detail

### 2.1 Binance

Present: spot and USDM as `bookTicker` only; the M2.4 eapi options lane (mark
price, mark IV, greeks → `OptSummary`) — code-complete, WS host currently
unreachable from this network (the `BINANCE_EAPI_WS_HOST` lever).

Missing: `@markPrice` (mark + funding), dated-future contract semantics and BBO,
REST 24h volume, REST open interest, tick/lot metadata.

### 2.2 Bybit

Nothing present. `core-config`'s `Section` enum is exactly
`{Polymarket, Binance, Okx, Deribit, Hyperliquid, Pairs}`.

### 2.3 Deribit

Present and complete for options: `OptSummary` carries mark price, mark IV,
underlying, open interest and all four greeks; `crates/options-select` and the
per-run `options-manifest.tsv` handle chain selection and offline symbol
resolution. Perpetual and dated futures both arrive through `kind=future`.

Missing: the funding emit, DVOL, a spot lane, `settlement_period` gating.

### 2.4 OKX

The only venue with a real funding lane. Channel gating by `instType`
(`crates/ingress-okx/src/run_loop.rs:488-513`):

| instType | `bbo-tbt` | `trades` | `mark-price` | `funding-rate` |
|---|---|---|---|---|
| SPOT | yes | yes | — | — |
| SWAP | yes | yes | **yes** | **yes** |
| FUTURES (dated) | **yes** | yes | **yes** | — (Swap-only, correctly) |

Dated futures already arrive with BBO and mark price. Missing: 24h volume (no
`tickers` channel exists in `OkxChannel`), open interest.

### 2.5 Hyperliquid

`[hyperliquid] coins` already accepts spot assets (`@<idx>`) and outcome coins
(`#<enc>`) alongside perp coins; every coin gets `bbo`, `l2Book` and `trades`.
`activeAssetCtx` — the frame carrying funding, mark and OI — is gated to perps
(`coin_wants_asset_ctx` skips `@` and `#` prefixes); `activeSpotAssetCtx` is
deliberately not subscribed.

Missing: 24h volume, tick/lot metadata. The wire field `premium` is parsed
nowhere.

---

## 3. Instrument classes

| Class | Binance | Bybit | OKX | Deribit | HL |
|---|---|---|---|---|---|
| Spot | present | **add** | present | **add** | present |
| Linear perpetual swap | present (BBO only) | **add** | present | present | present |
| Dated / quarterly futures | **add** (semantics + BBO) | — | present | present (ungated) | — |
| Options, single leg | present | — | present | present | — |
| Option combos (one order, two legs) | — | — | — | **add** | — |

---

## 4. Channel matrix

`+` = to be added. `·` = not applicable to that venue.

| Channel / datum | Binance | Bybit | OKX | Deribit | HL |
|---|---|---|---|---|---|
| BBO (spot) | yes | **+** | yes | · | yes |
| BBO (perp) | yes | **+** | yes | yes | yes |
| BBO (dated future) | **+** | · | yes | yes | · |
| BBO (option) | yes | · | yes | yes | · |
| Mark price (perp) | **+** | **+** | yes | yes | yes |
| Mark price (dated) | **+** | · | yes | yes | · |
| Funding rate | **+** | **+** | yes | **+** (parsed, dropped) | yes |
| Open interest | **+** (REST) | **+** | **+** | yes | yes |
| 24h quote volume | **+** | **+** | **+** | **+** | **+** |
| L2 depth to a `Strategy` | **+** | **+** | **+** | **+** | **+** |
| Trades | **+** | **+** | yes | yes | yes |
| Option mark / IV / greeks | yes | · | yes | yes | · |
| Volatility index (DVOL) | · | · | · | **+** | · |

---

## 5. Reference and historical data

| Datum | Status |
|---|---|
| Funding history | **absent on every venue.** `claude-worker` has zero occurrences of "funding" |
| 1h / daily closes | REST candle lanes exist for Binance spot, Binance USDM, OKX, Deribit and Hyperliquid; Polymarket comes from local replay capture. **Bybit would be a sixth lane** |
| 24h quote volume | **absent on every venue** |
| Open interest | WS only, on Deribit (`ticker`) and Hyperliquid (`activeAssetCtx`). No REST fetch anywhere. OKX `opt-summary` and Binance eapi options carry none |
| Contract type / delivery date | **absent** (Binance) |
| Volatility index (DVOL) | **absent** |
| Tick / lot / contract size | present on OKX, Deribit, Polymarket; **absent on Binance, Bybit, Hyperliquid** |

---

## 6. Config surface

`core-config`'s universe grammar parses exactly: `binance_spot`,
`binance_usdm`, `binance_options`, `okx_instruments`, `okx_depth`,
`okx_options`, `deribit_instruments`, `deribit_depth`, `deribit_options`,
`hl_coins`, `pairs`. Unknown sections and keys are fatal, so every addition is a
deliberate grammar change.

Three limits:

1. **No Bybit section.**
2. **A dated future cannot be named as a distinct class.** Spot-vs-perp is
   expressible only on Binance (`spot` / `usdm` are separate keys); OKX and
   Deribit use one flat `instruments` list where the class is implicit in the
   instrument id and resolved at discovery; Hyperliquid uses one flat `coins`
   list where `@idx` implies spot.
3. **`VENUE_LIST_MAX = 500`** per venue list, with the append-never-reorder
   SymbolId law (file-order ordinals; reordering a still-listed instrument
   re-syms it on the next boot).

---

## 7. Out of scope

Order submission on any venue, venue order dispatchers, option combo *orders*,
API-key handling and sub-account isolation, position/margin/NAV accounting,
`Order` / `Fill` perp and fee semantics, exchange-side caps, and RiskGate/8i
enforcement are all **Stage-3** and are named here only so this list is not read
as including them. The gate is `docs/mvp-completion-plan.md` §7 and it is the
operator's to open. Everything above this section is capture / ingress /
reference-data class.
