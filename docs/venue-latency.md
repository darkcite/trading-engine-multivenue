# Venue latency calibration — per deployment, per location (standing law)

The backtest harness (`multivenue-engine backtest`) and the shadow-P&L
auditor (`audit-pnl`) share one fill model (`crates/cli/src/backtest/fill.rs`).
An order becomes matchable `Δ_venue` after emit and is matched against
ticks **in local receive time**. `Δ_venue` therefore has to be

    Δ_venue  =  feed one-way (venue stamp → this host)  +  order one-way (this host → venue)

**for the host and network the engine runs on.** Both terms are location
facts: a different box, region, ISP or VPN changes every number and
silently mis-prices every P&L the harness reports. This document is the
procedure, the derivation, and the current measurement. **Rerun it on
every deployment and after every move, before trusting a backtest there.**

## 1. Procedure

```sh
cd claude-worker
uv run python -m claude_worker.latency_probe \
    --out ~/multivenue/research/latency-$(date -u +%F) --minutes 25
sntp time.apple.com      # host NTP offset, cross-check only
```

`claude_worker.latency_probe` (stdlib-only, never in an engine path) runs
for the requested minutes and measures, concurrently:

- **feed delay** per venue stream: `(t_recv_host + clock_offset) − venue_timestamp`
  for every stamped message — Binance USDM `bookTicker` `E`/`T`, Binance
  `aggTrade` `E`/`T`, OKX `bbo-tbt` `ts`, Bybit `orderbook.1` `ts`/`cts`,
  Deribit `quote` `timestamp`, Hyperliquid `l2Book` `time`. Binance SPOT
  `bookTicker` carries no timestamp; it is recorded for lead-lag only and
  its delivery is read off the USDM stream (the two share the delay
  pattern — receive-time cross-correlation is symmetric between them).
- **order-path RTT** proxy: DNS, TCP connect, TLS handshake, then 20
  kept-alive requests to the venue's public time endpoint on the REST
  edge an order would take; repeated every 3 minutes.
- **clock offset** venue − host from the same requests (`server_time −
  (t_send + rtt/2)`), so feed delays are on the venue's clock and the
  host's NTP error cancels. macOS drifts 50–70 ms; never compare a raw
  venue stamp to `time.time()`.

Outputs: `<out>/summary.json`, `<out>/rest.json`, and `<out>/<venue>.ndjson`
(one line per message: receive wall + `CLOCK_MONOTONIC_RAW` ns — the
engine's capture clock — venue stamps, top of book, venue sequence).

## 2. Derivation of the harness table

For each venue take the **p50** feed delay of its book stream and the
**p50** kept-alive request RTT:

    Δ_venue = round_up_10ms( feed_p50 + RTT_p50 / 2 )

`RTT/2` is the one-way order path (the venue's internal processing sits
inside the RTT). p50 is the honest central estimate; the doctrine's
conservative variant (p90 feed + p90 RTT/2) is a `--latency-ns-venue`
stress, not the default. Venues without a stamped feed (Polymarket's
CLOB socket needs an asset id; Hyperliquid's `l2Book` is block-paced)
keep a documented conservative value.

Apply to `ModelParams::default()` in `crates/cli/src/backtest.rs`, with
the measurement date, host and network in the comment, and append the
run to §3.

## 3. Measurements

### 2026-09-03 — MacBook Pro M4, operator's home network (UTC+7), 25 min from 17:07Z

Host clock vs NTP (`sntp time.apple.com`): +52…+62 ms; venue offsets
(venue − host) +59…+63 ms across all five stamped venues — consistent, so
the correction is real, not noise. Raw data:
`~/multivenue/research/latency-2026-09-03/` (probe output).

| venue | REST edge | TCP ms | TLS ms | req RTT p50 / p90 ms | stream | feed delay p50 / p90 / p99 ms | n | **Δ = feed p50 + RTT/2** |
|---|---|---|---|---|---|---|---|---|
| binance (spot) | api.binance.com (CloudFront) | 8.2 | 22.8 | 106.8 / 130.1 | aggTrade E (T +1) | 71.5 / 420 / 1800 | 16 610 | **130 ms** |
| binance-usdm | fapi.binance.com (CloudFront) | 10.8 | 27.1 | 107.8 / 131.9 | bookTicker E (T ≈ E) | 71.0 / 428 / 5850 | 589 185 | (same lane: 130) |
| okx | www.okx.com (Cloudflare) | 8.6 | 26.0 | 119.8 / 138.4 | bbo-tbt ts | 66.8 / 89.4 / 386 | 14 363 | **130 ms** |
| bybit | api.bybit.com (CloudFront) | 11.3 | 21.8 | 43.5 / 65.0 | orderbook.1 ts / cts | 28.5 / 132 / 454 (cts 32.1) | 11 757 | **60 ms** |
| deribit | www.deribit.com (Cloudflare) | 8.7 | 26.9 | 208.1 / 252.4 | quote timestamp | 107.5 / 179 / 594 | 12 353 | **220 ms** |
| hyperliquid | api.hyperliquid.xyz (CloudFront) | 10.3 | 31.2 | 123.8 / 207.1 | l2Book time (block-paced) | 272.4 / 384 / 607 | 293 | **340 ms** |
| polymarket | clob.polymarket.com (Cloudflare) | 8.0 | 27.6 | 227.2 / 353.2 | — (CLOB socket needs an asset id) | unmeasured | — | **200 ms** kept (§4.4) |

Applied to `ModelParams::default()` the same day: `[pm 200, bn 130, okx 130,
deribit 220, hl 340, ai 0, bybit 60]` ms (was `[200, 100, 100, 100, 600, 0, 100]`).
Stress variant (p90 feed + p90 RTT/2) for `--latency-ns-venue`: bn 500,
okx 160, bybit 170, deribit 310, hl 490 ms.

**Feed staleness structure (messages stamped > 500 ms before receipt):**
binance-usdm 8.9 % of messages, 38 episodes in 26 min (median 0.4 s, max
15.6 s, 4.6 % of wall time, max staleness 7.5 s); binance spot aggTrade
8.6 %, 36 episodes; okx 0.46 %; bybit 0.84 %. Receive gaps on the Binance
socket never exceeded 0.95 s, so the staleness is upstream (Binance's
publish pipeline / CDN edge), not this host's socket: the stream keeps
flowing while its content is seconds old.

## 4. Findings that this measurement settled (2026-09-03)

1. **Cross-venue lead-lag measured in receive time on this host is
   feed delivery, not price discovery.** In receive time the 100 ms
   return cross-correlation okx→binance is asymmetric (c(+1)=0.28 vs
   c(−1)=0.11, c(0)=0.44); in **venue time** (okx `ts` vs binance-usdm
   `T`) the peak moves to lag 0 and doubles (c(0)=0.69) and the residual
   asymmetry (c(+1)=0.24 vs c(−1)=0.12) is within the venue-clock
   alignment error (±RTT/2 ≈ 50 ms). The medians are equal (71 vs
   67 ms); what differs is the Binance **tail**: 8.9 % of its messages
   arrive > 500 ms stale, in episodes of 0.4 s median / 15.6 s max,
   ~1.5 per minute, while the socket keeps flowing. Any strategy that
   reads "venue A moved, venue B hasn't" from the engine's capture must
   first be re-checked in venue time; an order sent to the "lagging"
   venue during a staleness episode meets a book that already moved.
   (Strategy-level consequences live in the research vault, per the
   research-in-git law.)
2. **The engine's own ingest is not the problem.** Joined on venue
   sequence (173 622 Binance and 14 362 OKX messages matched 1:1), the
   engine's capture timestamp trails an independent 2-stream probe by
   +1.5 ms p50 on the Binance lane (p99 +188 ms) and leads it by 9 ms
   on OKX — the tail is upstream of both.
3. **`binance:btcusdt` (the M1 anchor id 7, venue byte 0) was taking
   Polymarket's Δ and fee column** in every backtest/audit-pnl to date.
   Fixed 2026-09-03 (`fill::model_venue_byte`, test
   `legacy_bn_anchor_sym_takes_binance_delta_and_fee`; audit-pnl interns
   the anchor under Binance).
4. **REST edges are CDNs.** `api.binance.com`, `fapi.binance.com`,
   `api.bybit.com`, `api.hyperliquid.xyz` resolve to CloudFront;
   `www.okx.com`, `www.deribit.com`, `clob.polymarket.com` to
   Cloudflare. TCP connect (5–20 ms) measures the edge; only the
   kept-alive request RTT measures the path to the venue — and DNS can
   hand out a far edge on one run and a near one on the next (Binance:
   10 ms vs 108 ms TCP connect across two runs, same request RTT).
