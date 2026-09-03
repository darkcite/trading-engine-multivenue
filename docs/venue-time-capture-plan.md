# Venue-time capture + staleness gate — plan (VT0–VT6)

**Status: OPEN — authored 2026-09-03 on the operator's "go with the plan".**
Owner doc for the work; progress entries go at the bottom (§9). This is
data-integrity work on the capture + harness, NOT Stage-3 executor work —
the §7 entry gate (`docs/arch/mvp-completion-plan.md`) is untouched.

## 1. Why

`docs/venue-latency.md` §3–4 (2026-09-03 measurement): the Binance feed to
this host is stale (> 500 ms) for 8.9 % of its messages, in ~1.5
episodes/min of 0.4 s median / 15.6 s max, up to 7.5 s of staleness,
**while the socket keeps flowing**. OKX 0.46 %, Bybit 0.84 %. The engine
stamps every tick only with its local parse-complete time; Binance spot
`bookTicker` carries no venue timestamp at all. Consequences:

- **The capture cannot tell a stale price from a current one.** A backtest
  replays a 5-second-old Binance mid as the book of the moment; a strategy
  can "trade" against it and book a mid-to-mid gain that was never
  available. The xv family looked profitable exactly this way
  (`docs/research/xv-q0-latency-2026-09-03.md`).
- **The live VM cannot tell either.** `FeatId::Mid` never goes stale
  (`features.rs:723`); a dead or lagging feed keeps producing signal and
  keeps pricing orders.
- **Nothing downstream can be trusted until this is fixed**: not the gate,
  not audit-pnl, not any cross-venue research. It is the cheapest of the
  three gaps and every later result depends on it.

## 2. Doctrine

1. **Venue time is data, not a clock.** We never re-time records by it;
   `ts_ns` (monotonic, local) stays the ordering key everywhere. Venue
   time rides alongside so age can be computed.
2. **Age is measured against the venue's own fastest message**, never
   against the host wall clock (macOS is 50–70 ms off NTP; venues are
   NTP'd to each other within a few ms). Per venue, per connection:
   `off_ms = max over the window of (venue_time_ms − mono_ms)` — the
   least-delayed message defines the offset; every message's delay is
   `off_ms − (venue_time_ms − mono_ms) ≥ 0`. Drift is bounded by a slow
   decay of `off_ms` (1 ms per minute) so a clock step re-learns.
3. **Stale means: the venue's book is unknown.** A stale tick MUST NOT
   fill a modeled order, MUST NOT feed a strategy signal (`mid` ABSENT —
   the channel law's "hold"), MUST still be captured (it is what the
   engine saw), and MUST NOT move the last-known good mark.
4. **Thresholds are per venue and come from the measurement**
   (`docs/venue-latency.md`), not from taste: default `stale_after_ms` =
   the venue's feed-delay p99 rounded up (bn 6000 → capped at 1000, okx
   400, bybit 500, deribit 600, hl 700, pm 1000), overridable per venue by
   flag. The Binance cap is deliberate: a 1 s-stale BTC book is unknown.
5. **Same size, same rings, same cadence.** `Tick` stays 64 B; the pad is
   spent. No new allocation, no new branch in the parse hot loop beyond
   one subtraction + compare per tick; no new syscalls.
6. **v2 captures remain readable forever** (venue time 0 = unknown; the
   harness then applies the v2 law: never stale). Readers accept ≤ 3.

## 3. Wire format (VT0) — `Tick` v3, PMLR header version 3

| offset | bytes | field | v2 | v3 |
|---:|---:|---|---|---|
| 48 | 1 | venue | ✓ | ✓ |
| 49 | 1 | **flags** | pad (0) | bit0 `TICK_FLAG_STALE`, bit1 `TICK_FLAG_VENUE_TIME_SENTINEL` (venue time came from the connection's sentinel stream, not this message) |
| 50 | 6 | _pad | pad (0) | pad (0) |
| 56 | 8 | **venue_time_ms** (`u64`) | pad (0) | venue timestamp ms, 0 = unknown |

`u64` at 56 keeps 8-byte alignment. `Tick::new` gains `venue_time_ms` +
`flags` (36 call sites, all ingress/tests). PMLR `VERSION` → 3 for every
kind (one number, one bump — kinds 1–7 unchanged; the header check
`ver > VERSION` becomes ≤ 3 accepted). Python `pmlr.py`: `VERSION_MAX` 3,
`TickRec` gains `flags` + `venue_time_ms` (v2 files read them as 0 —
document that the fields are meaningful only when `version() >= 3`).
`docs/wire-format.md` + `docs/migration.md` entries; the raw tap is
unchanged.

## 4. Per-venue venue-time source (VT2)

| venue | tick stream | venue time field | notes |
|---|---|---|---|
| okx | `bbo-tbt` | `ts` (book generation, ms) | direct |
| bybit | `orderbook.1` | `cts` (matching engine) — fall back to `ts` | direct |
| deribit | `quote.*` | `timestamp` | direct; `book.100ms` is batch-paced by design |
| binance-usdm | `bookTicker` | `T` (transaction) — `E` if absent | direct |
| hyperliquid | `bbo` / `l2Book` | `time` | block-paced; p50 272 ms is cadence, not delay — threshold 700 |
| polymarket | `book` / `price_change` | `timestamp` | ms string; direct |
| **binance spot** | `bookTicker` | **none** | add `aggTrade` on the SAME connection as the staleness **sentinel**: its `T` learns the connection's `off_ms` and delay; every `bookTicker` tick inherits the sentinel's latest delay with bit1 set. BTC trades ≈ 10/s, so the sentinel is at most ~100 ms behind the book on the pair that matters; thin spot pairs inherit a coarser sentinel (documented). The rejected alternative — `@depth@100ms` + REST snapshot to rebuild BBO with `E` — is the book-builder lane, a separate project. |

Ingress cost: one integer parse per message for the field (already
scanned past today), one subtraction, one compare, one OR into `flags`.
Zero allocation. The offset estimator is 3 × u64 per connection.

## 5. Consumers (VT3–VT4)

- **strategy-vm `features.rs`**: `FeatId::Mid/Bid/Ask` return ABSENT when
  the last tick carried `TICK_FLAG_STALE` (one `&` in the existing
  `last_tick_ns > 0 && v > 0` guard). Rows hold; no order prices off a
  stale mid. The same flag feeds `entry_blocked`/`exit_blocked` counters.
- **engine paper accounting** (`paper.rs` / positions view): marks skip
  stale ticks (last good mark stands).
- **backtest / audit-pnl `fill.rs`**: a stale tick neither fills nor
  marks (extends today's "one-sided ticks never mark and never fill"
  rule); the harness ALSO recomputes staleness for v3 files from
  `venue_time_ms` with the same estimator (so a threshold change is a
  replay, not a recapture) and reports `stale_ticks` / `stale_time_pct`
  per venue in the stderr summary + `--emit-detail` sidecar. v2 files:
  the v2 law (never stale) and a printed warning that the capture is
  stale-blind. Flag `--stale-after-ms <venue>:<ms>` beside the latency
  flags. **Schema-1 stdout is unchanged** (frozen worker contract).
- **capture-catalog / audit-replay**: per-venue stale-time % column.
- **metrics**: `engine_ingress_<venue>_stale_ticks_total`,
  `engine_ingress_<venue>_feed_delay_p50_ms` (gauge, cheap reservoir).

## 6. Phases + gates

| phase | scope | done-tell |
|---|---|---|
| **VT0** spec | this doc + `docs/wire-format.md` v3 tables + `docs/migration.md` entry | operator reads, no objections |
| **VT1** types + PMLR | `core-types` Tick v3 (+ `TICK_FLAG_*`), `Tick::new` signature, `core-io` VERSION 3 + reader ≤ 3, `pmlr.py` v3, size/align asserts, proptests, worker `test_pmlr.py` v2+v3 fixtures | nextest + pytest green; a v2 file reads with venue_time 0 |
| **VT2** ingress | okx / bybit / deribit / bn-usdm / hl / pm field extraction; bn-spot `aggTrade` sentinel on the same socket; per-connection `off_ms` estimator; flag set at parse | per-venue parser proptest + fuzz target updated; live smoke with `--raw-tap`: delays match `latency_probe` within ±10 ms p50 |
| **VT3** VM + paper | `features.rs` stale ⇒ ABSENT; paper marks skip stale; metrics | unit tests: a stale tick holds a would-fire row; alloc assertions 0 B/op (the tick path is in the bench) |
| **VT4** harness | fill.rs stale law; recompute-from-venue-time for v3; `--stale-after-ms`; sidecar + stderr; audit-pnl same; capture-catalog column | the Aug-30 xv window replays with stale-time % printed; filtered-root equivalence still byte-identical on v2 roots |
| **VT5** proof | 24 h of v3 capture; re-run the xv sweep (`tools_xv_sweep`) on a v3 root with the gate on vs off — the delta IS the stale-blindness cost, recorded in the research vault; `docs/venue-latency.md` gains the engine-side delay p50/p99 per venue | numbers in the vault; CLAUDE.md pitfall added ("a backtest on a v2 root is stale-blind") |
| **VT6** close | migration doc, README subcommand notes, this doc's close entry | stay-greens recorded |

Stay-greens at every phase: `cargo nextest run --workspace` · release alloc
assertions 0 B/op (`--test-threads=1`, fresh bench compile) · worker
pytest · `make lint` · `make license-check`. Live boots stay
operator-authorized (G0 relink law); VT2's smoke uses the standing engine
window after a scheduled restart.

## 7. Risks

- **Call-site breadth** (36 `Tick::new` sites, 6 ingress parsers, fuzz):
  mechanical, but the parse hot loops are bench-guarded — VT2 lands one
  venue at a time, each with its fuzz target re-run ≥ 300 s.
- **Binance spot sentinel is an approximation**: a stalled `bookTicker`
  with a flowing `aggTrade` on the same socket is not observed to happen
  (they share the pipeline), but bit1 marks the inference so research can
  separate the cases.
- **Offset estimator vs clock steps**: a venue clock step forward makes
  every message look delayed until the slow decay catches up (bounded:
  1 ms/min ⇒ a 100 ms step clears in 100 min). Log a counter; acceptable
  for v1, revisit if it fires.
- **Retention**: v3 does not change record size — no disk impact.

## 8. Out of scope (deliberately)

Taker/IOC fill law, cancel/TTL, fill-driven VM state (gap 2 of the
2026-09-03 answer — Stage-3, behind §7); fee defaults (gap 3 — an operator
ruling); the Binance spot book-builder lane; any strategy work.

## 9. Progress log

- 2026-09-03 — VT0 authored on the operator's go. Nothing else started.
- 2026-09-03 — **VT1 landed** (operator go the same day): `Tick` v3
  (`flags`@49, `venue_time_ms`@56, `TICK_FLAG_STALE` /
  `TICK_FLAG_VENUE_TIME_SENTINEL`, `Tick::new_stamped`, `Tick::is_stale`;
  `Tick::new` keeps its signature = the v2 shape, so the 77 existing
  call sites are untouched and VT2 moves the ingress sites to
  `new_stamped`), `core_io::VERSION` 3 + `PmlrReader::has_venue_time`,
  cli capture acceptance `MIN_PMLR_VERSION..=VERSION` (one
  `pmlr_version_accepted` law across backtest / audit-pnl /
  capture-catalog; v3 replays under the v2 law until VT4),
  `pmlr.py` v3 + `ticks_v3.pmlr` golden fixture (Rust writer; the v2/v1
  fixtures regenerated byte-identical), `docs/wire-format.md` +
  `docs/migration.md` entries. Done-tell: a v2 file reads with venue
  time 0 (`reader_reads_v2_tick_file_with_venue_time_zero_and_never_stale`,
  `test_ticks_v2_venue_time_fields_decode_as_zero`). Stay-greens
  recorded in the commit message.
- 2026-09-03 — **VT2 started: the shared estimator + OKX** (first
  venue). `core_time::FeedClock` (per-connection: `off_ms = max(venue −
  mono)` with the 1 ms/min decay, delay ≥ 0, `stale = delay >
  threshold`, threshold 0 = measure only; saturating arithmetic so a
  fuzzed stamp can never overflow; integer EMA gauge; unit tests for
  every doctrine-2 property + an ingress-okx proptest over arbitrary
  sequences). `VenueId::default_stale_after_ms` = the §2 doctrine-4
  table (pm 1000, bn 1000 cap, okx 400, deribit 600, hl 700, bybit
  500). `IngressStatus` gains `stale_ticks_total` + the
  `feed_delay_ema_ms` gauge — the slot is now exactly 128 B with zero
  slack (the gauge is a u16 in the diag triple's pad bytes; the next
  counter must reuse a field or grow to 192). Metrics:
  `engine_ingress_<venue>_stale_ticks_total`,
  `engine_ingress_<venue>_feed_delay_ema_ms`. Engine flag
  `--stale-after-ms <venue>:<ms>` (repeatable; harness venue labels;
  `parse_stale_after_ms` + tests). **OKX**: `bbo-tbt` `ts` (already
  parsed as `OkxBboFrame.ts_ns`) → `Tick::new_stamped(…, ts/1e6,
  stale·TICK_FLAG_STALE)`, one `now_ns()` serves the tick and the
  judgement, estimator reset on reconnect, `Driver::set_stale_after_ms`
  (boot), run-loop tests (`bbo_ticks_carry_venue_time_and_the_stale_judgement`,
  `stale_threshold_override_and_reconnect_reset_apply`); the OKX
  parser and its fuzz target are unchanged (the stamp was already
  extracted) — `okx_frame` re-run ≥ 300 s regardless. Remaining VT2
  venues: bybit, deribit, bn-usdm, hl, pm, then the bn-spot `aggTrade`
  sentinel. Live smoke (`--raw-tap`, delays vs `latency_probe` ±10 ms
  p50) is the VT2 done-tell and needs the operator's relink + boot.
- 2026-09-03 — **VT2 venue 2: Bybit.** `BybitBookFrame` gains
  `venue_time_ms` (`"cts"` matching-engine time preferred, envelope
  `"ts"` fallback, 0 when absent — a garbage stamp is "unknown", never
  a parse failure; the `"ts":` scan cannot false-match inside `"cts":`).
  One `FeedClock` per CONNECTION (the multi-conn lane builds one driver
  per socket; `spawn_bybit` applies the threshold to each), judged on
  EVERY push (one-sided deltas teach the offset too), stamped on the
  emitted tick, reset on reconnect. Tests: parser (cts > ts > 0,
  garbage), proptest (`orderbook1_roundtrips` now generates ts/cts),
  run-loop fresh/stale/fresh + override/reconnect. `bybit_ws_frame` fuzz
  ≥ 300 s. Remaining: deribit, bn-usdm, hl, pm, bn-spot sentinel.
- 2026-09-03 — **VT2 venue 3: Deribit.** `quote.*` `timestamp` (already
  parsed as `DeribitQuoteFrame.ts_ms`, the `venue_seq` source) now rides
  the slot in full via `Tick::new_stamped`; the driver's `FeedClock`
  (600 ms default, `--stale-after-ms deribit:<ms>`, reset on reconnect)
  judges every quote from the same `now_ns()`; stale quotes counted +
  gauge published. Tests: venue_time on the existing quote test,
  fresh/stale/fresh sequence, override + reconnect reset. Parser + fuzz
  target unchanged; `deribit_jsonrpc_frame` re-run ≥ 300 s. Remaining:
  bn-usdm, hl, pm, bn-spot sentinel.
- 2026-09-03 — **VT2 venue 4: Binance (USDS-M direct; spot deferred to
  the sentinel step).** `BookTickerFrame` gains `venue_time_ms` — `"T"`
  (transaction time) preferred, `"E"` (event time) fallback, 0 when
  absent: spot `bookTicker` carries neither, so spot ticks stay
  "unknown, never stale" until the aggTrade sentinel lands. One
  `FeedClock` per connection (the M1 multi-conn lane owns one driver per
  socket; `spawn_binance` / `spawn_binance_multi` apply the threshold to
  every bookTicker slot; eapi/markPrice slots never judge), reset on
  reconnect, stamped on the emitted tick, counted + gauge per tick.
  Tests: parser (T > E > 0, garbage), proptest with the three wire
  shapes (spot / E only / E+T), run-loop spot-unknown assertion,
  USDS-M fresh/stale/fresh sequence, override + reconnect reset.
  `binance_book_ticker` fuzz ≥ 300 s. Remaining: hl, pm, bn-spot
  sentinel.
- 2026-09-03 — **VT2 venue 5: Hyperliquid.** `bbo` `time` (already
  parsed as `HlBboFrame.ts_ns`, the venue_seq source) rides the slot;
  the driver's `FeedClock` (700 ms default = block cadence + delay,
  `--stale-after-ms hl:<ms>`, reset on reconnect) judges every bbo from
  the same `now_ns()`; stale ticks counted + gauge. Distinct from the
  §6.2 `HlStaleness` session monitor (l2Book cadence per coin, kills the
  session) — this flags individual ticks. Tests: venue_time on the
  existing bbo test, fresh/stale/fresh, override + reconnect reset.
  Parser + fuzz target unchanged; `hl_ws_frame` re-run ≥ 300 s.
  Remaining: pm, bn-spot sentinel.
- 2026-09-03 — **VT2 venue 6: Polymarket.** `scan_venue_time_ms` (the
  frame's quoted-ms `timestamp` in full) replaces `scan_venue_seq`;
  `venue_seq_of` keeps the low-32-bit venue_seq law; `parse_book_update`
  and `parse_price_change_row` (now taking the ms stamp) build the tick
  with `venue_time_ms` set and `flags = 0` — the parsers own no
  estimator, so the run loop's `judge_and_push_tick` judges against the
  connection's `FeedClock` (1000 ms default, `--stale-after-ms pm:<ms>`,
  reset on reconnect), sets the flag, counts, publishes the gauge.
  `handle_text_frame` takes `&mut Driver` (disjoint `rx` / `feed_clock`
  field borrows). Tests: parser venue_time + venue_seq_of + garbage,
  proptest (stamp rides in full), run-loop book+price_change stamps,
  fresh/stale/fresh, override + reconnect reset. `polymarket_clob_frame`
  fuzz ≥ 300 s. Remaining: the bn-spot aggTrade sentinel (VT2's last
  step), then the live smoke.
- 2026-09-03 — **VT2 venue 7: the Binance-spot aggTrade SENTINEL —
  VT2 code-complete.** A spot bookTicker slot (`Driver::new_spot_sentinel`,
  `BinanceConnSpec.spot_sentinel`, the legacy single-connection lane
  too) queues `{"method":"SUBSCRIBE","params":["<sym>@aggTrade"],"id":1}`
  on the SAME socket the moment the upgrade lands (stack scratch, no
  allocation). One substring probe per frame sorts the socket:
  `"e":"aggTrade"` ⇒ the print's `T` teaches the connection's
  `FeedClock`, its verdict + stamp are latched, and the print is
  captured as a `ChannelId::Trade` row (`venue_seq` = aggregate id,
  `venue_time_ms` = `T`, `v0` = px ×1e6, `v1` = qty ×1e6 NEGATED when
  `m:true` — the aggressor sold — the cross-venue convention; capture
  only, no engine lane); `{"result":…}` / `{"error":…}` ⇒ the SUBSCRIBE
  reply, a message not data; anything else ⇒ bookTicker, which INHERITS
  the sentinel's latest stamp + verdict with `TICK_FLAG_VENUE_TIME_SENTINEL`
  set (no print yet ⇒ 0 / never stale). USDS-M slots keep their direct
  `T`/`E` judgement. `TradeFrame` gains `ts_ms` / `agg_id` /
  `is_buyer_maker`. Tests: parser (agg id + maker flag), subscribe frame
  after upgrade (unmasked and byte-checked), plain slots subscribe
  nothing, over-long symbol refused, inherit unknown → fresh+bit1 →
  stale+bit1 (every tick between prints) → fresh, prints never reach
  the tick ring, Trade rows signed correctly, acks quiet.
  `binance_agg_trade` + `binance_book_ticker` fuzz ≥ 300 s each.
  **What is left of VT2 is the live smoke** (operator relink + boot,
  `--raw-tap`, delays vs `latency_probe` ±10 ms p50, and the first
  sighting of `engine_ingress_bn_stale_ticks_total` moving during a
  Binance staleness episode).
