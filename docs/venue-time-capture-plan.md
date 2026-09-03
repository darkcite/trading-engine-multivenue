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
| **VT5** proof | **≤ 2 h WINDOWS ONLY (operator law 2026-09-03, §6.1).** On a bounded symlink root of ONE ≤ 2 h v3 window: backtest + audit-pnl with the default gate vs `--stale-after-ms <venue>:0` (the delta IS the stale-blindness cost); re-run the xv sweep (`tools_xv_sweep`) gate on vs off on the same window (≥ 30 gate-off modeled fills, else the window is degenerate for xv and another is cut); ICDP I0 re-validation per the vault plan's G1 (pooled windows, §6.1); every number in the research vault; `docs/venue-latency.md` gains the engine-side delay p50/p99 per venue (from `feed_delay_ema_ms` + the harness stale summary) | numbers in the vault; CLAUDE.md pitfall added ("a backtest on a v2 root is stale-blind") |
| **VT6** close | migration doc, README subcommand notes, this doc's close entry | stay-greens recorded |

Stay-greens at every phase: `cargo nextest run --workspace` · release alloc
assertions 0 B/op (`--test-threads=1`, fresh bench compile) · worker
pytest · `make lint` · `make license-check`. Live boots stay
operator-authorized (G0 relink law); VT2's smoke uses the standing engine
window after a scheduled restart.

### 6.1 Capture-window law (operator ruling 2026-09-03 — absolute)

**No capture window or data gate may exceed 2 hours.** The earlier "24 h
of v3 capture" in the VT5 row (and the "≥ 48 h / two disjoint 24 h
windows" in the vault's merged ICDP×VT plan) is VOID. Phases never wait
for data; they design the gate around what ≤ 2 h can prove, and pool
windows that already exist when more is needed.

- **A window** is a contiguous slice of ONE run of at most 2 h by `ts_ns`
  (slot offset 0, monotonic — `docs/wire-format.md`), materialized as a
  bounded root by the vault symfilter one-shot (which gains a
  `--from-s/--to-s` bound relative to the run's first slot; header
  `epoch_ns` copied byte-for-byte so the harness's directory-name check
  holds). Whole-run and whole-root replays are out (they also OOM —
  CLAUDE.md ops debt c).
- **Pooling** is over DISJOINT windows. Windows may all come from a
  single run (operator ruling); more runs add independence but are never
  waited for. A pool is grown by cutting another window from capture
  that already exists — never by scheduling a capture.
- **Every gate states its sample in per-window fills / ticks / bars and
  a window count N**, never in hours. VT5's xv on/off delta: ≥ 30
  gate-off modeled fills in the window. The ICDP I0 gate G1 (vault plan
  §5): N ≥ 4 windows, leave-one-window-out cross-fitting, pooled
  top-decile trade floors — the substance stays in the vault.
- A v2 window is admissible only for stale-BLIND comparisons (it prints
  `stale-blind(v2)`); a judged number needs a v3 window.

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
- 2026-09-03 — **VT3 landed.** strategy-vm `features.rs`: `FeatSym`
  gains `tick_stale` (in the old `_pad0` byte — no size change); a stale
  tick sets it and returns before touching the BBO or sampling the
  rolling rings (one byte compare on the hot path); `Mid/Bid/Ask` read
  ABSENT while it is set (the channel law's "hold" — the vm's existing
  `entry_blocked` / `exit_blocked` counters record the consequence);
  the next fresh tick clears it and restores the reads with its own
  values. Tests: features (stale ⇒ ABSENT, ring not sampled, last good
  quote untouched, fresh restores) and vm (the same would-fire quote
  flagged stale holds the row; fresh fires). Paper marks: the engine
  keeps no in-process marks (paper fills are empty, P&L is offline), so
  the "last good mark stands" law lands where the marks live —
  `claude_worker.features.collect_marks` skips stale ticks on v3 files
  (v2 files keep the v2 law) with a golden `ticks_v3.pmlr` test. The
  metrics half of VT3 shipped with VT2. Not touched: capture-derived
  candles (`candles.py`, PM only) still fold stale ticks — a VT5 note.
- 2026-09-03 — **VT4 landed (harness stale law).** New
  `cli::backtest::stale` — `StaleJudge` (one `core_time::FeedClock` per
  (venue, sym) per run, judged in FILE order on the RAW sym, thresholds
  from `ModelParams::stale_after_ms`) REWRITES `TICK_FLAG_STALE` on every
  v3 tick from its `venue_time_ms` (the sentinel bit survives); a v2
  file's ticks stay never-stale and the lane is `stale_blind`. So a
  threshold change is a REPLAY: the same capture under `pm:0` / `pm:5000`
  reproduces the golden P&L byte for byte. `fill.rs::on_record` returns
  on a stale tick before the mark and the fill scan (`stale_ticks_skipped`
  counted in `ModelOutcome` → `HarnessStats`). `ModelParams` gains
  `stale_after_ms: [u32; 7]` (`VenueId::stale_after_ms_defaults()`, the
  table the engine flag shares); `parse_model_params` takes the
  `--stale-after-ms <venue>:<ms>` specs on `backtest` and `audit-pnl`
  (bin + `BacktestConfig` / `AuditPnlConfig`). Surfaces: the backtest
  stderr model line prints the threshold table + skipped count and every
  run line gains `stale: pm=1/4 (4285bps) bn=0/1 (0bps)` / `stale-blind(v2)`
  (`render_stale_line`, shared verbatim by audit-pnl's per-run report);
  the `--emit-detail` sidecar is **`detail_version` 2** — `model` carries
  `stale_after_ms`, a new `stale` block carries `ticks_skipped` + per-run
  per-lane `{ticks, stale_ticks, stale_time_bps, stale_blind}`. Schema-1
  stdout unchanged (frozen). capture-catalog: per-lane `stale_captured`
  (the ingress's boot-time verdict, NOT a re-judge; `null`/`stale-blind v2`
  on v2 files) in JSON + summary. Tests: stale unit (re-judge, bit1 kept,
  stale-time accounting, v2 blind, threshold 0); harness golden-minus-
  one-stale-tick (trades 2→1, days 2→1, trough + final mark unchanged,
  stderr + sidecar exact), replay-not-recapture (`pm:0`, `pm:5000`, v2
  rewrite ⇒ golden + stale-blind); audit-pnl (stale crossing tick never
  fills, `pm:0` restores the +$1 markout); catalog v3/v2 lanes. The
  existing goldens are unchanged because `Tick::new` ticks carry stamp 0
  (unknown ⇒ fresh). Deviation from §5: `stale_time_pct` is rendered as
  integer basis points of the file span (deterministic, no floats).
  Not done here: the Aug-30 xv-window replay (a v2 root — it prints
  `stale-blind(v2)`, which is the point; the gate on/off delta needs the
  VT5 v3 capture).
- 2026-09-03 — **Capture-window law amendment (operator ruling, absolute):
  no capture window or data gate may exceed 2 hours.** The VT5 row's
  "24 h of v3 capture" is VOID and rewritten; new §6.1 states the law
  (window = ≤ 2 h `ts_ns` slice of one run as a bounded root; pool
  DISJOINT windows that already exist, single-run pools admissible;
  gates count per-window fills/ticks/bars + N windows, never hours; v2
  windows only for stale-blind comparisons). The ICDP I0 gate G1 was
  re-ruled the same day via AskUserQuestion — shape A: N ≥ 4 disjoint
  ≤ 2 h v3 windows, leave-one-window-out cross-fitting, 15 s cells gate
  on pooled top-decile floors, 1 m cells report only (the 2 h yield at
  1 m is ≈ 11 top-decile trades per instrument-window — un-gateable);
  substance in the vault's merged ICDP×VT plan §5. CLAUDE.md CURRENT
  STATE carries the law. Nothing else changed; VT2 live smoke + VT5
  still wait on the operator's relink + boot (G0).
- 2026-09-03 — **VT2 LIVE SMOKE — PASSED (VT2 closed).** Relink
  (`cargo build --release -p cli`, 13:31 +07) + restart through the
  sanctioned lever (`echo 19700101 > ~/multivenue/state/last-restart-utc-0000`;
  the 0000 slot drained + KeepAlive rebooted pid 63755 at 06:34:03Z) on
  the operator's "do all by yourself". First v3 run
  `run-1788417289611943000`: every `*-ticks.pmlr` header is `PMLR 0300`;
  `engine_vm_rows_active 1` (the #7b recommit held); the new metrics
  are live for all six venues (`engine_ingress_<venue>_stale_ticks_total`,
  `_feed_delay_ema_ms`); capture-catalog on the run prints
  `stale_captured` per lane (pm 0 / bn 0 / okx 87 / deribit 39 / hl 1 /
  bybit 182 at 8 min; `harness=ok`). **Delay check vs an independent
  `latency_probe` run (15 min, 06:36–06:51Z, both sides judged by the
  same `FeedClock` law on the BTC instrument; raw + comparison in
  `~/multivenue/research/latency-2026-09-03-vt2/`):** Δp50
  engine−probe = binance-spot (sentinel) +1 ms · binance-usdm −7 ·
  okx −1 · bybit 0 · deribit +2 — all within the ±10 ms done-tell;
  hyperliquid +25 is NOT comparable (engine `bbo` vs probe `l2Book`,
  and the probe's HL collector kept only 168 messages). A 1:1 join on
  the Binance-USDM update id (176 074 of 179 419 ticks matched) puts
  the probe's receive-side EXCESS over the engine at p50 0.0 / p90 +72
  / p99 +967 ms — the probe's own > 1 s tail (1.5 % of its messages)
  is Python-side jitter; the engine saw 0 stale USDM ticks in the
  window. **Live-verdict = offline-re-judge agreement 100.00 % on every
  venue** — after one harness fix found by this smoke: the VT4
  `StaleJudge` re-judged sentinel-inherited (bit1) ticks against their
  own `ts_ns`, adding the time since the print and flagging quiet
  seconds (3.3 % re-judged vs 0.0 % live on `binance:btcusdt` in the
  first 3 min). Fixed in `cli::backtest::stale` (sentinel law: a
  repeated inherited stamp latches the first verdict; a new stamp is
  judged afresh; module doc + test
  `sentinel_stamp_is_judged_once_and_latched_until_the_next_print`).
  Engine-side observations worth keeping: the OKX BTC-USDT lane on the
  engine's all-instrument socket runs 1.3 % > 400 ms (p99 475 ms)
  where the probe's single-instrument socket runs 0.18 % (p99 205) —
  socket load is part of the engine's delay and the estimator measures
  exactly that; Deribit's 35–39 stale ticks were all in the boot
  flood's first minute (subscription burst); Binance had NO staleness
  episode in the window (the 8.9 % measurement was taken at 17:07Z —
  the "counter moving during an episode" sighting is deferred to the
  next episode, watched during VT5). Numbers per venue go to
  `docs/venue-latency.md` §5 (engine-side delay).
