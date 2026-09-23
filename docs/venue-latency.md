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
  Deribit `quote` `timestamp`, Hyperliquid `l2Book` `time`, MEXC spot
  `aggre.bookTicker` `sendTime` (protobuf, the probe's one binary
  parser) and MEXC futures `depth.full` `ts`/`cts`. Binance SPOT
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

### 2026-09-23 — MacBook Pro M4, operator's home network, 10 min from ~07:24Z (MX9)

`python -m claude_worker.latency_probe --minutes 10`, taken to measure
MEXC (the seventh venue, `docs/mexc-ingress-plan.md` MX9); every venue
ran. Venue offsets (venue − host): binance +96.1, binance-usdm +98.3,
okx +107.3, bybit +99.9, deribit +102.6, mexc +108.3, mexc-perp +105.1
ms (hyperliquid: no time endpoint) — **+96…+108 ms this run vs +59…+63
on 2026-09-03**; every venue moved together, i.e. the host clock, and
the feed delays below are on each venue's own clock, as the §1 law
requires.

| venue | REST edge | TCP ms | TLS ms | req RTT p50 / p90 ms | stream | feed delay p50 / p90 / p99 ms | n | **Δ = feed p50 + RTT/2** |
|---|---|---|---|---|---|---|---|---|
| binance (spot) | api.binance.com | 20.4 | 29.7 | 121.1 / 133.7 | aggTrade | 64.1 / 248.6 / 446.5 | 5 388 | 124.7 → 130 |
| binance-usdm | fapi.binance.com | 46.9 | 74.5 | 133.8 / 149.1 | bookTicker | 79.6 / 293.3 / 565.7 | 156 797 | (same lane) |
| okx | www.okx.com | 10.8 | 20.1 | 127.5 / 140.6 | bbo-tbt ts (trades 67.5 / 71.7 / 76.5, n 1 788) | 66.4 / 69.7 / 76.4 | 4 756 | 130.2 → 140 |
| bybit | api.bybit.com | 12.4 | 21.4 | 54.7 / 244.5 | orderbook.1 ts (cts 23.9 / 102.8 / 386.7; publicTrade 47.6 / 235.3 / 486.7, n 4 889) | 19.8 / 101.4 / 385.7 | 6 895 | 47.2 → 50 |
| deribit | www.deribit.com | 13.8 | 17.8 | 200.2 / 220.3 | quote timestamp (trades 328.5 / 650.0 / 914.3, n 1 232) | 103.1 / 234.4 / 606.1 | 3 795 | 203.2 → 210 |
| hyperliquid | api.hyperliquid.xyz | 16.2 | 30.2 | 139.3 / 221.4 | l2Book time (block-paced) | 273.9 / 423.3 / 569.4 | 113 | 343.6 → 350 |
| **mexc (spot)** | **api.mexc.com** | **8.3** | **21.0** | **131.4 / 164.3** | **aggre.bookTicker sendTime** | **81.9 / 94.4 / 338.0** | **60 024** | **147.6 → 150** |
| **mexc-perp** | **contract.mexc.com** | **14.2** | **22.7** | **134.2 / 232.0** | **depth.full ts (cts 65.3 / 92.3 / 354.9)** | **59.6 / 85.2 / 348.9** | **1 933** | **126.7 → 130** |
| polymarket | clob.polymarket.com | 8.0 | 22.2 | 218.6 / 268.9 | — (CLOB socket needs an asset id) | unmeasured | — | 200 kept |

**MEXC.** Spot 81.9 + 131.4/2 = 147.6 → 150 ms; futures 59.6 + 134.2/2
= 126.7 → 130 ms. One venue byte carries both classes, so the slower
binds: **Δ = 150 ms**. Stress (p90 feed + p90 RTT/2): spot 94.4 +
164.3/2 = 176.6 → 180, futures 85.2 + 232.0/2 = 201.2 → **210 ms**. The
stale default is the measured feed p99 rounded up, the per-venue
doctrine: 338 / 349 → **400 ms** (`VenueId::default_stale_after_ms`).
**Finding:** MEXC's kept-alive REST RTT is 131–134 ms from this location
— 2.4× Bybit's this run (54.7 ms), 3× its 2026-09-03 value (43.5 ms) —
and reproduces the Cowork-VM path. **MEXC is a research/capture venue
from here; nothing latency-sensitive belongs on it** (plan §1.6, R4).

Applied to `ModelParams::default()` / `core_fill::ACTIVATION_NS_DEFAULT`
the same day, **MEXC only**: `[pm 200, bn 130, okx 130, deribit 220,
hl 340, ai 0, bybit 60, mexc 150]` ms (slot 7 new). Stress for
`--latency-ns-venue`: mexc 210 ms.

**The other venues re-derived from this run are OBSERVED, NOT APPLIED**
— bn 124.7 → 130 (unchanged), okx 130.2 → 140 (applied 130), bybit
47.2 → 50 (applied 60), deribit 203.2 → 210 (applied 220), hl 343.6 →
350 (applied 340). Re-calibrating them is outside the MEXC lane and is
the operator's decision; the 2026-09-03 row stays the applied table for
them.

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

## 5. Engine-side feed delay (VT2, `core_time::FeedClock`) — per venue

Since VT2 (2026-09-03) the engine judges every stamped tick against the
venue's own fastest message (`docs/arch/venue-time-capture-plan.md` §2
doctrine 2): `delay = off_ms − (venue_ms − mono_ms)`, `off_ms = max` with
a 1 ms/min decay. This is a **max-relative** delay: `absolute ≈ relative +
floor`, the floor being the network's one-way minimum (§3's absolute p50
minus the relative p50 below ≈ 60–70 ms on every venue from this host —
the same 60 ms the venue-clock offsets show). The engine exports the
running EMA (`engine_ingress_<venue>_feed_delay_ema_ms`) and the stale
count; the harness re-derives the same numbers from a v3 capture
(`--stale-after-ms`, stale line per run).

### 2026-09-03 06:36–06:51Z — first v3 run, BTC instrument per venue (15 min)

Engine capture (`run-1788417289611943000`) vs an independent
`latency_probe` on the same host over the same wall window, both judged
by the same law (raw + comparison: `~/multivenue/research/latency-2026-09-03-vt2/`).

| venue (engine stream) | engine relative p50 / p90 / p99 ms | stale % @ default | probe relative p50 (same law) | probe absolute p50 | Δp50 |
|---|---|---|---|---|---|
| binance spot `bookTicker` ← `aggTrade` sentinel | 6 / 137 / 499 (per print) | 0.00 @ 1000 | 5 | 60.9 | +1 |
| binance-usdm `bookTicker` T | 9 / 112 / 356 | 0.00 @ 1000 | 16 (probe tail is its own jitter: p99 +967 ms excess on a 1:1 seq join) | 78.0 | −7 |
| okx `bbo-tbt` ts (all-instrument socket) | 2 / 40 / 475 | 1.30 @ 400 | 3 (single-instrument socket: p99 205, 0.18 %) | 70.3 | −1 |
| bybit `orderbook.1` cts | 6 / 46 / 244 | 0.21 @ 500 | 6 | 31.4 | 0 |
| deribit `quote` timestamp | 6 / 29 / 251 | 0.07 @ 600 | 4 | 112.9 | +2 |
| hyperliquid `bbo` time | 63 / 142 / 357 | 0.10 @ 700 | 38 (`l2Book`, 168 msgs — not comparable) | 290.6 | (+25) |
| polymarket `book`/`price_change` timestamp | EMA 5–38 (no probe lane) | 0.00 @ 1000 | — | — | — |

Findings: (1) the engine's delay estimator agrees with an independent
probe within ±7 ms p50 on every directly comparable venue — the VT2
done-tell; (2) the live verdict and the harness's offline re-judge agree
on 100.00 % of ticks on every venue once the sentinel latch law is
applied (`cli::backtest::stale`); (3) the OKX tail on the engine's
socket (1.3 % > 400 ms) is ~7× the probe's dedicated socket — socket load
is part of the engine's delay and is exactly what the harness must
replay; (4) Binance had no staleness episode in this window (the 8.9 %
of §3 was measured at 17:07Z) — thresholds stay per §2 doctrine 4 until
a longer v3 record says otherwise.

### 2026-09-23 09:00:58–09:01:58Z — MEXC's first engine boot (60 s, live engine)

The live paper engine (`run-1790153299745387000`, booted 08:48:19Z on the
MEXC go-live binary `bbc8553`, `[mexc]` = the Q-MX5 twelve). This is a single
QUIET minute, measured on request, so it is a first reading and not a
calibration. Stale % is the engine's own verdict over the ticks it EMITTED.
MEXC emits only BBO changes (plan D11), plus a re-emit when the stale
verdict flips. MEXC relative delay follows this section's law: offset
learned over the whole run, 1 ms/min decay.

| venue | ticks in the minute | stale % @ default | MEXC relative p50 / p90 / p99 ms |
|---|---|---|---|
| binance | 256 781 | 0.00 @ 1000 | |
| okx | 9 238 | 0.11 @ 400 | |
| deribit | 6 719 | 0.01 @ 600 | |
| hyperliquid | 3 386 | 0.00 @ 700 | |
| bybit | 9 699 | 0.07 @ 500 | |
| polymarket | 2 498 | 0.00 @ 1000 | |
| **mexc** (spot `aggre.bookTicker` + futures `depth.full`) | **1 429** | **0.35 @ 400** | spot **8 / 147 / 366** (n 1 043) · futures **8 / 41 / 163** (n 386) |

Findings:
1. The 400 ms MEXC default holds. Spot p99 relative is 366 ms, just under
   it, and futures has ample margin. Keep 400 until a longer quiet record
   says otherwise (§2 doctrine 4).
2. HOST I/O STALLS EVERY INGRESS AT ONCE. In the first 4 minutes after this
   boot, stale bursts hit bn/bybit/deribit/hl/mexc in the same 30 s buckets
   at 30–90 s and at 180 s: bybit 38 %, deribit 30 %, mexc 20 %. OKX alone
   stayed clean. The bursts coincided with heavy worker disk work on the
   same Mac: two `claude-worker fetch` runs and a manual candles cycle.
   Once that stopped, every venue fell back to ≈ 0 % (the table). The
   likely cause is capture writes on the ingress threads stalling behind
   disk contention. This is not venue feed delay, so never calibrate a
   threshold from a window that overlaps a worker cycle.
3. MEXC in its first 13.5 min: 424 k msgs → 43.7 k ticks (the D11
   dedupe), 0 reconnects (the D12 heartbeat holds), 0 parse errors,
   0 sub-drops.

