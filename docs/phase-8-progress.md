# Phase 8 — Progress log

Working notes per the Stage-1 operating agreement: each entry records
what is done, what is next, and open issues at a clean boundary.

## 2026-08-15 (eighth entry) — G1 first soak (6 h per operator directive): data completeness PROVEN from capture; deribit runtime gap monitor over-counts (67 counted, zero lost); HIP-4 #enc streamed all 6 h

Operator-run observation soak per §12 G1 row (24 h → 6 h, operator
directive). No code changes; no git ops; tree clean at b931c59; this
entry left uncommitted.

### Setup

- Launch 2026-08-14T21:19:30Z → SIGINT 2026-08-15T03:19:30Z: exactly
  6 h 0 m, single pid throughout, clean exit — all six run-loops
  `res=Stopped` within 36 ms of ONE SIGINT (the once-observed doubled
  signal was not needed).
- CLI: `run --paper` · PM Xi-2027 YES token (8e-validated id) · okx
  BTC-USDT,ETH-USDT,BTC-USDT-SWAP · deribit BTC/ETH-PERPETUAL · hl
  `BTC,ETH,SOL,#10810` · `--polygon-path /` · `--raw-tap deribit,rpc`.
- HIP-4 pre-launch probe (outcomeMeta): BTC 1d priceBinary =
  outcome 1081 (targetPrice 63385, expiry 20260815-0600) → Yes coin
  `#10810`; boot coverage hl 4/4 of 572 · okx 3/3 of 1951 ·
  deribit 2/2 of 92 · pm 1/1; zero boot ERRORs.
- RPC leg: still the keyless stand-in
  (`polygon-bor-rpc.publicnode.com`) — parse errors EXPECTED + tapped.
- Binary currency: mtime predates b931c59 (commit followed the build);
  `cargo build --release -p cli` no-op ("Finished 0.19s") = fingerprint
  proof the binary matches committed sources.

### Samples (16 over 6 h; raw table in /tmp/soak-6h-notes.md)

T+6h counters: msgs pm 6 021 · bn 710 775 · okx 568 502 · deribit
356 387 · hl 361 640 · rpc 10 591 (Σ ≈ 2.01 M). reconnects bn 9 ·
okx 10 · hl 38 · deribit 0 · pm 0 · rpc 0; resubscribes 0 everywhere.
gaps_total: deribit 67, all other venues 0. ring_drops_total 0
everywhere all run; capture_io_errors 0; engine `dropped=0` on every
5 s summary; HL `Stale` lines: 0 (8d.1 budget fix holds); disk steady
52 Gi free; capture dir 142 MB (inside the predicted 100–200 MB band).

### audit-replay (full output /tmp/soak-6h-audit.txt)

- Coverage matrix: every configured venue×channel stream present,
  including the outcome coin (hl sym 0x04000004 = #10810): ticks 1 343,
  book 4 056, trades 1 353.
- Integrity totals — ALL SIX venues: tick_seq_regressions=0
  trade_holes=0 trade_ids_missing=0 book_chain_breaks=0. Per-stream
  regr/holes/missing/chain_breaks = 0 on every seq-bearing stream.
- Cadence: every emitted verdict IN-BAND — okx ticks ×3, okx mark,
  hl book ×4 (≈5.3 s period vs 2–6 s band; 4 029/4 056 in-bucket —
  outcome coin included).
- Raw taps: deribit tap 64 B header-only — ZERO rejects across 356 k
  msgs / 6 h (starbase fix regression-clean; §6.6 deribit
  trade_holes==0 met). rpc tap records=14 395 rejects=14 395; previews
  are `eth_subscription` newHeads with `"blobGasUsed":null` etc. —
  exactly the stand-in shape diagnosed in the seventh entry.

### Incidents (observed only; no interventions, engine never restarted by hand)

1. **Network weather, two episodes** (21:43–21:51, 23:32–23:49 UTC)
   plus scattered singles: clustered multi-venue loop exits (okx+hl
   within 3 s at 23:41; okx ×3 in 31 s at 23:48–:49) — shared-local-
   path signature, not venue faults. Supervisor recovered every one;
   no state ever stuck (>15 min rule never approached). The ~7
   per-coin hl book inter-arrivals in the 6–10 s bucket line up with
   reconnect instants — reconnects are paired with visible, benign
   stream effects.
2. **deribit gaps_total=67 is a runtime-monitor over-count, not data
   loss**: zero rejects, zero deribit disconnects/resubscribes, and
   the offline re-derivation shows holes=0/chain_breaks=0 on both
   instruments. Increments arrived in bursts inside the weather
   windows. Suspect: cross-stream trade_seq comparison (ticker-implied
   vs trade-channel) during stalls, and/or gap increments lack a
   paired ChannelEvent/log line. Filed as the run's one real defect —
   in the MONITOR, not the feed. No code touched (operator rule).
3. **Metrics endpoint**: two single-scrape accept failures
   ("connection error: Resource temporarily unavailable (os error
   35)" ~01:23:22, ~01:46) — unhandled EAGAIN in the accept path;
   recovered on next request; the error line prints without
   timestamp/target prefix. Robustness nit.
4. **capture_records gauges publish only after a run-loop exit**:
   venues that never cycled report 0 despite growing pmlr files; okx
   froze at its first-cycle value until later cycles. Cosmetic;
   io_errors=0 + file growth + audit are the real signal.
5. Runbook nit: live metric names carry `_total`; the monitoring
   grep from the soak directive silently missed the error-class
   counters — corrected during the run.

### HIP-4 observations

`#10810` streamed the entire 6 h (numbers above; book IN-BAND).
outcomeMeta events: 0 — expected: the 06:00 UTC settlement fell 2.7 h
after window end; operator chose strict 6 h over shifting/extending,
so the 8d lifecycle observation (settlement + successor id) remains
open. hl trade `regr≈n/2` on all four coins (e.g. 17 094/34 214) =
equal-timestamp batch siblings; excluded from integrity totals;
benign wire trait — audit-tool display note filed.

### §6.6 verdict (6 h basis)

- `ring_drops_total == 0` everywhere: **PASS**.
- Coverage = 100 % of configured symbols (boot + audit matrix): **PASS**.
- Cadence verdicts IN-BAND (okx ticks/mark, hl book): **PASS**.
- deribit trade_holes == 0 + zero tap rejects (starbase regression
  check): **PASS**.
- capture_io_errors == 0: **PASS**.
- rpc tap rejects: expected-nonzero per keyless stand-in: **PASS
  (as-expected)**.
- Zero unexplained gaps: **QUALIFIED** — substance PASS (offline
  re-derivation proves nothing was lost anywhere), letter FAIL (the
  67 deribit increments have no paired logged venue event to point
  to). Operator ruling requested: bless on substance, or require the
  monitor pairing fix and re-run.
- Overall: **feed completeness demonstrated** — the question "are we
  getting all the data?" answers YES from capture evidence; one
  monitor defect + two metrics nits to fix before/alongside the 24 h
  gate run.

### Open items

1. Deribit gap-monitor false positives: root-cause the counting site;
   add per-increment ChannelEvent/log pairing so §6.6's letter is
   mechanically checkable; then re-soak.
2. ALCHEMY_HOST + key still pending — rpc leg stays expected-reject.
3. HIP-4 settlement lifecycle unobserved — next soak window must span
   06:00 UTC (a 24 h run covers it by construction).
4. Metrics accept-loop EAGAIN handling; capture_records gauge
   publish cadence; audit display note for hl equal-timestamp trade
   batches.
5. The full 24 h §6.6 soak remains required for G1 as written.

## 2026-08-15 (seventh entry) — 8e COMPLETE: discovery + capture + audit-replay live-verified; Deribit ~1.3% ROOT-CAUSED AND FIXED (zero rejects, zero gaps)

Stage-1's last code phase. Everything below is uncommitted (no git ops
per standing rule); tree state = 4ca8ca1 + the full 8e diff (≈50
files). G1 24 h soaks are now unblocked and operator-run.

### Delivered

- **Probes first (house pattern, again paid for itself)**: real
  response shapes of all four discovery endpoints captured before any
  parser was written (`/tmp/8e-probe/*.json`). Live facts the docs
  don't carry: Deribit REST emits bare floats incl. scientific
  notation (`"tick_size": 1e-05`); HL `perpDexs[0]` is `null` (native
  dex slot); PM Gamma `clobTokenIds` is a double-encoded JSON string;
  OKX rows mix quoted decimals with bare numbers/bools/arrays.
- **core-net**: `http1::write_post_request` (HL `/info` is POST) +
  proptest; new `boot_http` module — boot-only blocking HTTPS/1.1
  fetcher over rustls (documented alloc exception; body as
  `Range<usize>` into the caller's buffer; Content-Length early-exit,
  in-place dechunk, budget + deadline), TLS-loopback tested (8 tests).
- **core-parse**: `scan_number_sci_1e9`/`scan_number_sci_1e6` (shared
  scaled body — sci-notation numerics), `skip_ws`/`skip_string`/
  `skip_json_value` (iterative, depth-capped structural skipper).
  Unit + proptests.
- **Boot REST discovery, all four venues** (`discovery.rs` per ingress
  crate; boot-only fixed-cap tables; every parser proptested + fuzzed:
  `okx_instruments`, `deribit_instruments`, `hl_info`,
  `pm_gamma_markets` registered in fuzz/Cargo.toml):
  - OKX: 3 instType pages; `tickSz`/`lotSz`/`ctVal` ×1e9 captured;
    `OkxSymbolTable` rows now carry `OkxInstType` — **the `-SWAP`
    suffix hack is deleted**; mark-price gates on Swap|Futures,
    funding on Swap only.
  - Deribit: BTC+ETH+USDC `kind=future` (options excluded v1);
    `tick_size` + `tick_size_steps` (≤4) + `contract_size` +
    `min_trade_amount` ×1e9; dotted-name invariant enforced.
  - HL: meta/spotMeta/perpDexs/outcomeMeta; full asset-id scheme
    (perp = universe index; spot `@N` = 10000+N; HIP-3 `dex:COIN`
    name-validated, per-dex meta deferred; HIP-4 `#enc` =
    100_000_000+enc with side-count validation). Closes the 8d
    deferral.
  - PM: Gamma by `clob_token_ids` — completes D1 venue-side: flags
    (active/acceptingOrders/enableOrderBook/closed/negRisk), tick +
    min-size, sibling (No-side) token via `sibling_of`.
  - cli boot sequence (rate-paced 150 ms / 1.05 s / 250 ms), §6.1
    coverage log line + `engine_ingress_<venue>_coverage_configured`
    gauge per venue; any fetch/parse failure fatal in BOTH modes;
    missing configured symbol fatal in `--live`, warned in paper.
    BN/RPC: no discovery (documented).
- **PMLR capture wired into the shipped run path (the sixth-entry
  defect, fixed)**: new `core_types::{Capture, NullCapture,
  ChannelEvent, ChannelId}` (64-B POD, `SlotKind::Event = 5`; 4 stays
  reserved for AiCmd), `core_io::PmlrCapture` (per-venue
  `<label>-{ticks,events,signals}.pmlr` + optional bounded
  `<label>-raw.tap`, 1 s flush cadence, sticky-disable-on-io-error
  policy documented, `Drop` drains). Every ingress run loop takes a
  monomorphized `capture: &mut C` (no dyn) with hooks: tick-before-
  push (ring-dropped ticks still captured), per-channel events,
  parse_reject at every parse-error site, raw_frame pre-classify,
  maybe_flush per poll. `MULTIVENUE_LOG_DIR` tilde-expands; run dir =
  `<log_dir>/run-<epoch_ns>`. Gauges:
  `engine_ingress_<venue>_capture_{io_errors,records}`.
  wire-format.md + migration.md updated (ChannelEvent layout, tap
  format `PMRT` v1, SlotKind table).
- **`--raw-tap <csv|all>` / `--raw-tap-mode <rejects|all>` /
  `--raw-tap-budget-mb`** on `run` (off by default; budget-bounded
  first-N capture — documented deviation from §6.5's ring sketch).
- **`audit-replay --dir`** subcommand: venue×channel coverage matrix,
  per-symbol rates + inter-arrival histograms vs CORRECTED cadence
  bands (okx bbo 10 ms floor, okx books/mark + deribit book 100–200 ms,
  HL l2Book 2–6 s band around the measured ~3.3 s, event-driven = no
  band), integrity re-derivations (tick seq regressions; book chain
  breaks honoring snapshot/heartbeat/reset rules; **trade holes
  derived for Deribit only** — OKX trade seqIds share the book-wide
  sequence and legitimately jump), raw-tap reject previews. Golden
  synthesized-run tests.
- **Deferred small items closed**: TUI `ingest_health` bits 4/5/6 =
  okx/deribit/hl (appended, never renumbered); okx/deribit/hl WS+REST
  hosts moved from inline cli consts to core-config env keys with
  defaults (`OKX_WS_PUBLIC_HOST`, `OKX_REST_HOST`, `DERIBIT_WS_HOST`,
  `DERIBIT_REST_HOST`, `HYPERLIQUID_WS_HOST`, `HYPERLIQUID_API_HOST`;
  .env.example updated).
- **Bench**: all six run-loop steady-state alloc assertions now run
  with a REAL `PmlrCapture` (tap `All`) — the whole capture path is
  proven **0 B/op** in-window, incl. staging flushes; okx pins exact
  capture counts.

### Live-wire finds (three boots against real venues during 8e)

1. **OKX pre-listing rows**: `state:"preopen"` instruments (caught on
   `JP225-USDT-SWAP` the day before listing) carry EMPTY
   `tickSz`/`lotSz` and `instIdCode:null`. Parser now accepts empty
   numerics on non-live rows only (live rows without tick/lot =
   BadRow); preopen is excluded from the live universe.
2. **OKX long instIds**: FUTURES page now lists pre-market perps like
   `MOODENG-USD_UM_XPERP-310815` (27 bytes) — `OKX_INST_ID_MAX`
   raised 24 → 32.
3. **Deribit ~1.3% ROOT-CAUSED — two parser defects, zero venue
   fault** (first `--raw-tap deribit` run captured 7 reject payloads):
   (a) the starbase engine rollout **reordered trade rows**
   (`timestamp`/`price`/`direction` now precede `trade_seq`; new
   `starbase_match_id`/`starbase_timestamp` fields follow) — 8c's
   `"trade_seq":`-marker row slicing straddled two rows, so
   single-trade frames lost their head fields (the rejects) and every
   reject holed the monitor (the phantom trade-seq gaps);
   (b) round amounts arrive in **scientific notation**
   (`"amount": 1.0e3`) which the strict decimal scanner rejected.
   Fixed: object-extent row slicing (order-tolerant both wire
   layouts) + sci-capable numeric scans across the Deribit parsers;
   regression test uses the real tapped bytes. **DeribitTradeSeq's
   strictly-sequential policy stands, now evidence-based**: the final
   run's 158 trades re-derived offline show holes=0.

### Verification

- Mac gates (authoritative): `cargo nextest run --workspace`
  **824/824** (697 baseline + 127 added by 8e); release alloc gate
  **30/30, 0 B/op** — all six ingress steady-state fns with live
  capture + tap. Fuzz `cargo check` clean (4 new targets).
- **Final live confirmation run** (~3 min, all seven connectors, tap
  on deribit): coverage okx 3/3 of 1951 · deribit 2/2 of 92 · hl 3/3
  of 572 (= 232 perps + 324 spot + 8×2 outcomes, exact) · pm 1/1;
  every parse_errors/gaps/ring_drops/capture_io_errors counter **0**
  across all venues; deribit 1664+ msgs **0 rejects 0 gaps** (was
  ~1.3% + gaps); deribit-raw.tap = header only (zero rejects all
  run); capture run dir fully populated (18 pmlr files + tap);
  `audit-replay` over it: all okx tick/mark verdicts IN-BAND, all
  chain_breaks=0, holes=0.

### Open / notes for the operator

1. **RPC stays open** pending the real `ALCHEMY_HOST`+key: the
   stand-in's `eth_subscription` newHeads (nulled blob fields etc.)
   are now raw-tapped (`--raw-tap rpc`) — first run with the real
   endpoint will confirm or give us the exact diff in one pass.
2. G1 §6.6 soaks: run per-venue + all-venue 24 h with capture on
   (always on now) and judge with
   `multivenue-engine audit-replay --dir <run dir>`; add
   `--raw-tap deribit` if any deribit counter moves.
3. OKX trades still use `"tradeId":`-marker row slicing (its needed
   fields all sit after the marker on today's wire — live-verified
   clean) — same latent fragility class as the Deribit find;
   candidate for object-extent slicing in a later pass.
4. Binance REST discovery deliberately out of Phase-8 scope
   (plan §4 has no BN discovery bullet).
5. HIP-3 per-dex meta (`{"type":"meta","dex":...}`) not fetched v1 —
   a configured `dex:COIN` validates the dex name only (market-data
   subscribe keys on the coin string, so Stage-1 correctness holds).
6. Session facts for the next session: the Cowork-sandbox mount now
   gives cargo FALSE GREENS (stale-fingerprint skips) — compile/test
   ONLY on the Mac; after file edits, impossible-looking unresolved-
   import errors on the Mac = stale rmeta → `cargo clean -p <crates>`
   and retry. The engine stops on SIGINT (a doubled signal was needed
   once — likely a racing command, not a defect; watch it).
7. Paper orders again fired off live PM ticks (latency-arb default
   pairing) — mechanically fine, economically meaningless; strategy
   config remains a soak-time concern.

## 2026-08-14 (sixth entry) — first live all-connector test: 3 wire defects found + fixed; ALL SEVEN CONNECTORS DELIVERING

Operator-ordered live paper run on the Mac (first ever boot of the
engine against real venues; 8d committed as 576758e beforehand).
Setup: fresh `.env` from the example (hosts only, no secrets),
`--polymarket-asset-id` = the "Strait of Hormuz normal by Aug 31"
YES token (live two-sided book), `--okx-symbols
BTC-USDT,ETH-USDT,BTC-USDT-SWAP`, `--deribit-symbols
BTC-PERPETUAL,ETH-PERPETUAL`, `--hl-coins BTC,ETH,SOL`,
`--polygon-path /` against a keyless public Polygon WSS
(`polygon-bor-rpc.publicnode.com`) as an Alchemy stand-in.

### Wire defects found live, root-caused, fixed, tests updated

1. **Hyperliquid staleness budget vs real cadence.** Plan §4.3's
   "l2Book full snapshot every block, ≥ 0.5 s" is wrong on the wire:
   a probe (14-sub connection) showed pushes are **timer-paced ~1 /
   3.3 s per coin**, uniform across BTC/ETH/SOL regardless of book
   activity — the 2 s budget tripped every session by construction
   (`Stale` loop every ~3 s). `HL_STALENESS_BUDGET_NS` 2 s → **10 s**
   (≈3× observed period); plan §4.3/§6.2 amended with a dated
   correction. After the fix: 0 gaps, stable sessions.
2. **Polymarket endpoint was wrong AND the subscribe was missing.**
   `.env.example`'s `clob.polymarket.com` is REST-only; the real-time
   host is `ws-subscriptions-clob.polymarket.com` and the cli's
   hardcoded `/ws/` path 404s — the market channel lives at
   `/ws/market`. Worse: the Phase-1 run loop **never sent any
   subscribe frame** — the venue stays silent without
   `{"assets_ids":[...],"type":"market"}`. D1 had masked all of it
   (PM had never once been proven against the venue). Fixed:
   `.env.example` host corrected; cli path → `/ws/market`;
   `write_market_subscribe` + queue-at-Steady added to
   ingress-polymarket (Driver::new now takes the asset id;
   spawn_polymarket threads it through); PM migrated onto
   `core_net::IoBuf` per the 8a opportunistic-migration note.
3. **Polymarket Phase-1 parser format was fictional.** Live wire:
   `book` events arrive **array-wrapped** with **object levels**
   (`{"price":"..","size":".."}`) sorted **worst→best** (top-of-book
   = LAST element), and `price_change` events carry per-row
   `best_bid`/`best_ask` — top-of-book needs no ladder. The old
   parser (`"bids":[["px","sz"]]`, first element = top) could never
   match a real frame. Rewritten: `parse_book_update` (last-level
   walk, per-event slicing at `{"market":"` for multi-asset frames),
   new `parse_price_change_row` (touch from the row; touch size only
   when the changed level IS the touch, else 0 — documented),
   `scan_venue_seq`; sibling-asset events/rows (the venue groups by
   market, not token) are skipped silently by design. All fixtures
   (lib, run_loop, loopback, bench) moved to the live shapes.

### Live counter snapshot (fixed binary, ~90 s, all states Up=2)

| connector | msgs | parse_err | gaps | drops | note |
|---|---|---|---|---|---|
| polymarket | 68 | **0** | 0 | 0 | book + price_change → Ticks; strategy fired paper orders off live PM ticks |
| binance | 16 632 | 0 | 0 | 0 | clean |
| okx | 9 944 | 0 | 0 | 0 | clean (first live run) |
| deribit | 3 596 | 48 | 29 | 0 | **~1.3% rejects + trade-seq gaps — open item, needs 8e `--raw-tap`** (probe of the same channels showed no anomalous shapes; heartbeat protocol proven live — 0 reconnects) |
| hyperliquid | 3 459 | 0 | 0 | 0 | clean after budget fix; HIP-4 sub surface untouched |
| rpc | 62 | 83 | 0 | 0 | substitute public endpoint's JSON differs from Alchemy's — needs the real `ALCHEMY_HOST`+key for a fair test |
| rss | up | — | — | — | thread up (dead path by design until 8f) |

### New defect discovered (8e work item)

- **PMLR replay capture is not wired into the shipped `run` path at
  all** — no PMLR writer is constructed anywhere in cli;
  `~/multivenue/logs` stays empty after a live run. Plan §6.5's
  "every parsed slot already flows to PMLR replay logs" is false for
  the running binary (it holds only in tests/tools). Capture is the
  8h backtest dataset — wire it in 8e alongside the audit tool.
  (Also note `MULTIVENUE_LOG_DIR=~/...` is not tilde-expanded by
  core-config — use an absolute path when wiring it.)

### Open / notes

1. Deribit reject/gap rate (~1.3%) is the one unexplained data-loss
   signal across all venues — first target for 8e `--raw-tap`.
2. `.env` (hosts only, chmod 600) now exists on the Mac; RPC needs
   the operator's Alchemy key for the real endpoint.
3. 24 h soaks still operator-run, now unblocked on all five
   market-data venues.
4. Paper orders fired during the test (latency-arb pairing the PM
   market vs btcusdt by default sym wiring) — mechanically correct,
   economically meaningless; strategy configuration is a soak-time
   concern, not an ingress one.

## 2026-08-14 (fifth entry) — 8d ingress-hyperliquid: CODE COMPLETE (soak + operator go pending)

### Delivered

- **`crates/ingress-hyperliquid`** (new): channels per plan §4.3/§4.4
  — `bbo` → Tick (`VenueId::Hyperliquid` = 4), `l2Book` header
  (full-snapshot venue, ≤ 20 levels/side — level counts + touch
  lifted, every level strictly validated), `trades` multi-row walk
  (side `B`/`A`, **unquoted** `time`/`tid` — HL sends bare numbers,
  unlike OKX), `activeAssetCtx` (funding ×1e9 / mark / oracle / OI),
  `allMids` (entry count), `outcomeMetaUpdates` (HIP-4 lifecycle
  kinds + optional `#<enc>` + time; deliberately shape-tolerant —
  schema may grow as permissionless deploys leave testnet) — all
  `#[repr(C, align(64))]` 64-B PODs. `fastAssetCtxs` skipped v1
  (DEFLATE in hot path, §4.3).
- **HIP-4**: outcome coins `#<enc>` flow the ordinary coin path
  (`HlCoinTable` row like any other — no special surface);
  `activeAssetCtx` gating skips `#`/`@` coins (`dex:COIN` HIP-3
  perps not skipped); loopback + bench prove the `#330` → Tick
  roundtrip end-to-end.
- **Integrity (§6.2 row)**: stateless snapshots ⇒ no chain —
  `HlStaleness` per-coin monitor: `l2Book` venue time must
  **strictly advance** within budget (default 2 s = 2× block
  cadence) on the local clock; armed only after full ack
  verification; trip ⇒ `gaps_total`++ (documented: the §6.4 counter
  set has no dedicated stale counter — gap + `RunResult::Stale` is
  the staleness signature) ⇒ cli reconnects; missed data recovered
  by the next snapshot by construction.
- **Subscribe acks**: one frame per subscription (venue has no batch
  form; 16 coins ⇒ ≤ 66 frames, far inside 2000 client msgs/min);
  per-sub `subscriptionResponse` echoes verified through an
  expected/found **u128 bitmask** (4 per-coin channels × 16 coins +
  2 global = 66 bits); ack deadline `HL_SUB_ACK_BUDGET_NS` (5 s) ⇒
  session `Error` **without** debug_assert (ack latency is a venue
  timing condition, not a code invariant); `{"channel":"error"}` ⇒
  fail-fast (debug_assert + session error, okx pattern).
  `session_health` (pub) carries both checks — called from `run`,
  unit-tested directly.
- **Keepalive** `{"method":"ping"}` / `{"channel":"pong"}`;
  oversize-frame guard (rx 256 KiB, okx-sized — snapshots ≈ 2 KiB);
  tx 16 KiB (all subscribe frames queue in one drive cycle).
- **Decisions in the crate header**: `Tick.venue_seq` = `time` ms
  as u32 (same policy as Deribit; wraps ~49.7 d; full-width times
  live in the monitors); units: px USD ×1e6 (outcome coins:
  collateral units in [0,1]), **sz base-coin units ×1e6** (unlike
  Deribit's USD notionals), funding ×1e9; `POST /info` discovery
  (`meta`/`spotMeta`/`perpDexs`/`outcomeMeta`) deferred to 8e —
  coins from `--hl-coins` until then.
- **Tests**: 51 lib/run-loop (happy + failure per public fn incl.
  `session_health` deadline miss, oversize guard, HIP-4 tick) with
  4 proptests (bbo roundtrip, l2Book level counts,
  staleness-never-fires-in-budget model, no-parser-panics); TLS
  loopback (`tests/hl_tls_loopback.rs`, rcgen): happy path with
  byte-asserted 9-frame subscribe sequence + HIP-4 roundtrip +
  verification proof (deterministic ping-sync before close),
  staleness → `Stale` + gap proof, `{"method":"ping"}` emission +
  idle timeout, missed-acks → `Error`. Fuzz: `hl_ws_frame` (9
  parsers, no-panic), `hl_l2book` (render/parse differential +
  `HlStaleness` shadow-model differential) registered in
  `fuzz/Cargo.toml`, `cargo check` clean. Bench:
  `hl_parsers_are_zero_alloc` +
  `hl_run_loop_steady_state_is_zero_alloc` (1 004-frame steady
  drain over the real handshake + 9-ack verification path,
  `session_health` and WS-Ping pong renders inside the measured
  window, 0 B/op).
- **cli wiring**: `--hl-coins` (flag order = ordinal;
  `make_symbol_id(Hyperliquid, i+1)`; **no depth flag** — `l2Book`
  is always subscribed, it feeds the staleness monitor);
  `HL_KEEPALIVE` 50 s probe / 60 s idle (venue cuts at 60 s);
  `spawn_hyperliquid` on core 7 (§9 map); lane-4 producer connected
  — the last dropped tick lane goes live; `RunResult::Stale` arm
  reconnects like `IdleTimeout`, logged distinctly, no gap
  double-count; `engine_ingress_hyperliquid_*` §6.4 counters +
  state gauge (`ingress_last` grown 5→6, hyperliquid appended —
  nothing renumbered); `TICK_RING_CAP == TICK_RING_SIZE`
  const-assert; endpoint consts `api.hyperliquid.xyz:443 /ws`
  inline (core-config entry rides with 8e); 2 new cli tests
  (coin-table flag parsing incl. `#330`, bad-spec rejection).

### Verification (both platforms)

- Sandbox (rustc 1.88; reused the orphaned `/tmp/rustup` toolchain
  read-only — no reinstall): workspace excl. bench **664/0**
  (607 baseline + 57: 51 lib + 4 loopback + 2 cli); release alloc
  gate **30/30, 0 B/op** (28 + 2; `CARGO_PROFILE_RELEASE_DEBUG=0`
  for disk — semantics identical, Mac gate ran the true profile);
  fuzz `cargo check` clean (own target dir).
- Mac (nextest via RustRover MCP): **693/693** (634 baseline + 59;
  the +2 beyond the sandbox delta = the two new HL alloc assertions
  also running in debug under nextest, which includes the bench
  crate the sandbox run excludes); release alloc gate **30/30** on
  the true deploy profile. No spurious debug-parallel alloc
  failures this run.

### Deferred / notes

1. **HL `POST /info` discovery** (`meta`, `spotMeta`, `perpDexs`,
   `outcomeMeta`; asset-id scheme capture) deferred to the 8e boot
   coverage audit, its consumer — same disposition as OKX/Deribit.
2. TUI `ingest_health` hyperliquid bit — folded into the 8e TUI
   touch with okx's/deribit's; health fully visible via `/metrics`.
3. 24 h soak (§12 8d exit: BTC daily outcome streamed via `#<enc>`,
   `outcomeMetaUpdates` handled) is operator-run; not started. The
   soak's outcome coin comes from the protocol-run BTC daily
   (plan §13) — enc via `outcomeMeta` discovery (8e) or supplied
   directly in `--hl-coins` as `#<enc>`.
4. Sandbox hygiene for next session: this boot's dirs (`/tmp/ch3`,
   `/tmp/tt3`, `/tmp/ttf-8d`, `/tmp/l8d`) join the orphan list —
   pick fresh names. The orphaned `/tmp/rustup` 1.88.0 toolchain is
   world-executable: **reuse it** (PATH straight to its toolchain
   `bin/`, skip rustup-init; saves ~700 MB and minutes). Disk floor
   this session: 395 MB free — survived by deleting the release
   tree mid-sequence and rebuilding it last after dropping debug.
5. Everything uncommitted (8d in full); no git ops performed.

## 2026-08-14 (fourth entry) — 8c ingress-deribit: CODE COMPLETE (soak + operator go pending)

### Delivered

- **`crates/ingress-deribit`** (new): JSON-RPC 2.0 over WS per plan
  §4.2. Classifier + byte-scanner parsers (`quote` → Tick,
  `ticker.100ms` — mark/index/`current_funding`×1e9/OI/price limits
  in one 64-B frame, `trades.100ms` multi-row, `book.100ms` header
  with `DEPTH_CAP=64` level counting — excess counted not stored),
  all `#[repr(C, align(64))]` 64-B PODs; fixed-cap
  `DeribitSymbolTable` (rejects dotted names — channel-name parsing
  invariant); `DeribitBookChain` (snapshot always re-roots; change
  must link `prev_change_id == last`; anything else Gap + re-arm;
  documented `i64::MIN` sentinel-collision note, unreachable on
  wire); `DeribitTradeSeq` **strictly sequential** (`last+1` chains;
  jump = Gap, repeat/backwards = Regression — stronger than OKX's
  monotonic rule); JSON-RPC writers (`write_subscribe_all` — ONE
  batched call, subscribe costs 3000 of 30 000 credits;
  `write_set_heartbeat`, `write_test`, `write_book_op`) rendering
  into stack scratch via fixed `fmt_u64`.
- **Heartbeat protocol** (8c exit criterion): no WS ping — on
  upgrade→Steady the driver queues `public/set_heartbeat
  {"interval":15}` then the batched subscribe, both correlated
  through `core_net::subs::PendingTable` (monotonic ids from 1);
  venue `test_request` heartbeats answered with `public/test` in the
  same drive cycle; `KeepaliveAction::SendPing` = proactive
  `public/test` probe. Subscribe **result verification**: expected
  vs found channel-name bitmask (u64, 16 syms × 4 channels); any
  configured channel missing from the result echo ⇒ session error
  (fail-fast).
- **run_loop** on the 8a contract: `run(..., status, keepalive)`
  final params, Up exactly at upgrade→Steady, try_push fail ⇒
  ring_drops, idle ⇒ IdleTimeout; book Gap ⇒ unsubscribe+subscribe
  resync (fresh ids, pending-tracked) + gaps/resubscribes counters;
  trades gaps counted, deliberately never resubscribed (venue does
  not replay); venue `error` responses fail the session; two-phase
  borrow dispatch throughout; **oversize-frame guard**: rx full +
  frame incomplete ⇒ session error (book snapshots are unbounded;
  RX_BUF 4 MiB ≈ 2× deepest observed book) — no livelock possible.
- **Decisions documented in the crate header**: `Tick.venue_seq` =
  quote `timestamp_ms as u32` (quotes carry no seq; monotonic across
  reconnects, wraps ~49.7 d; same-ms quotes collapse at TopOfBook —
  full-width ids live in the monitors, u32 truncation only at the
  Tick boundary, same as OKX); amounts are **USD notional** for
  perps/futures ⇒ `Qty(1e6)` carries USD×1e6; `tick_size_steps` /
  `contract_size` arrive with 8e REST discovery — capture does not
  quantize.
- **Tests**: 44 unit/run-loop + 3 proptests (incl. book-chain
  never-chains-across-a-break model test); TLS loopback
  (`tests/deribit_tls_loopback.rs`, rcgen): happy path with
  server-side **heartbeat proof** (test_request answered by
  `public/test`, id increment asserted), book gap ⇒
  unsubscribe→subscribe observed server-side in order, idle timeout
  with proactive-probe assertion. Fuzz: `deribit_jsonrpc_frame`
  (all parsers), `deribit_book` (chain-model differential)
  registered in `fuzz/Cargo.toml`, `cargo check` clean. Bench:
  `deribit_parsers_are_zero_alloc` +
  `deribit_run_loop_steady_state_is_zero_alloc` (1000+-frame steady
  drain over the real handshake + set_heartbeat + subscribe-result
  path, **test_request answers rendered inside the measured
  window**, 0 B/op).
- **cli wiring**: `--deribit-symbols` (flag order = ordinal;
  `make_symbol_id(Deribit, i+1)`), `--deribit-depth`;
  `DERIBIT_KEEPALIVE` 20 s probe / 30 s idle (~2× the 15 s heartbeat
  interval — venue closes on unanswered test_request); `spawn_deribit`
  on core 6 (§9 map); VenueId::Deribit lane 3 producer connected
  (unspawned ⇒ dropped-producer); `engine_ingress_deribit_*` §6.4
  counters + state gauge (ingress_last grown 4→5, deribit appended);
  `TICK_RING_CAP == TICK_RING_SIZE` const-assert. Endpoint consts
  `www.deribit.com:443 /ws/api/v2` inline (core-config entry rides
  with 8e, same as OKX).

### Verification (both platforms)

- Sandbox (rustc 1.88): workspace excl. bench **607/0** (555 + 52);
  release alloc gate **28/28, 0 B/op** (26 + 2; sandbox release run
  used `CARGO_PROFILE_RELEASE_DEBUG=0` for disk — semantics
  identical, Mac gate ran the true profile); fuzz `cargo check`
  clean.
- Mac (nextest): **634/634** (580 + 54; "2 leaky" flagged on the TLS
  loopback server threads — pass status unaffected); release alloc
  gate **28/28** on the true deploy profile. One spurious
  debug-parallel failure of the pre-existing
  `latency_arb_on_tick_is_zero_alloc` under full-workspace load;
  passes repeatedly in isolation — the release gate (authoritative)
  is clean.

### Deferred / notes

1. **Deribit REST `get_instruments` discovery** (tick_size +
   `tick_size_steps`, contract_size, min_trade_amount capture; BTC +
   ETH + USDC futures/perps, options excluded v1) deferred to the 8e
   boot-coverage audit, its consumer — same disposition as OKX.
2. `book.{instr}.raw` / `trades.{instr}.raw` (auth-gated cadence)
   ride with the 8j dispatcher's authenticated socket.
3. TUI `ingest_health` deribit bit — folded into the 8e TUI touch
   with okx's; health fully visible via `/metrics`.
4. 24 h soak (§12 8c exit: chain clean + heartbeat protocol proven
   live) is operator-run; not started.
5. Sandbox hygiene for next session: the previous sandbox's caches
   (`/tmp/tt`, `/tmp/ch`, `/tmp/ttf`, `/tmp/ws-test.log`) are
   uid-orphaned (`nobody`) and unremovable — use fresh dirs
   (`/tmp/tt2`, `/tmp/ch2`, `/tmp/ttf-8c`, `/tmp/l8c`). Disk at 87%
   after full debug+release builds.
6. Everything uncommitted (8c in full); no git ops performed.

## 2026-08-14 (third entry) — 8b ingress-okx: CODE COMPLETE (soak + operator go pending)

### Delivered

- **`crates/ingress-okx`** (new): classifier + byte-scanner parsers
  (`bbo-tbt` → Tick, `trades`, `mark-price`, `funding-rate` ×1e9,
  `books` header), all `#[repr(C, align(64))]` 64-B PODs; fixed-cap
  `OkxSymbolTable` (instId → namespaced SymbolId, ≤16);
  `OkxSeqChain` implementing the §4.1 chain rules (snapshot
  `prevSeqId==-1`, idle heartbeat `prev==seq`, maintenance reset
  `seq<prev` with chain intact, true break ⇒ Gap + re-arm);
  `TradeSeqMonitor` (monotonic, equal legal); batched
  subscribe/unsubscribe writers (one op — 480 ops/h budget);
  fnv1a `sub_id_of`. Checksum deliberately NOT implemented
  (deprecated, always 0).
- **run_loop** on the 8a contract: `run(..., status, keepalive)` final
  params, Up exactly at upgrade→Steady, try_push failure ⇒ ring_drops,
  idle ⇒ IdleTimeout; literal `ping` text keepalive queued+flushed on
  `SendPing` (25 s vs venue 30 s cutoff); books Gap ⇒
  unsubscribe+subscribe resync + gaps/resubscribes counters; venue
  `error` events fail the session (fail-fast; debug_assert in debug);
  trades multi-row walk (≤16 seq-checked rows/push);
  mark/funding gated to `-SWAP` instIds until discovery carries
  instType.
- **core-parse**: `scan_price_1e9` (+ shared `scan_fractional_n`) —
  funding rates would truncate to ~1 count at 1e6.
- **Tests**: 44 lib/run-loop tests, 3 proptests; TLS loopback
  (`tests/okx_tls_loopback.rs`, rcgen): happy tick path, books gap ⇒
  resync frames observed server-side, idle-timeout with literal-ping
  assertion. Fuzz: `okx_frame`, `okx_book_seq` (chain-model
  differential) registered in `fuzz/Cargo.toml`, `cargo check` clean.
  Bench: `okx_parsers_are_zero_alloc` +
  `okx_run_loop_steady_state_is_zero_alloc` (1000-frame steady drain,
  real handshake, 0 B/op).
- **cli wiring**: `--okx-symbols` (flag order = ordinal;
  `make_symbol_id(Okx, i+1)`), `--okx-depth`; `OKX_KEEPALIVE`
  25 s/40 s; `spawn_okx` on core 5; VenueId::Okx lane producer
  connected (unspawned ⇒ dropped-producer, as before);
  `engine_ingress_okx_*` §6.4 counters + state gauge;
  `TICK_RING_CAP == TICK_RING_SIZE` const-assert. Endpoint consts
  `ws.okx.com:8443/ws/v5/public` inline (core-config entry rides with
  8e).

### Verification (both platforms)

- Sandbox (rustc 1.88): workspace excl. bench **555/0**; release alloc
  gate **26/26, 0 B/op**.
- Mac (nextest): **580/580**; release alloc gate **26/26**.

### Deferred / notes

1. **OKX REST instrument discovery** (instruments endpoint,
   tickSz/lotSz/ctVal capture, instType-driven channel gating)
   deferred to the 8e boot-coverage-audit work item, which is its
   consumer; `-SWAP` suffix gating stands in until then.
2. TUI `ingest_health` bitmask (bit0..3 pm/bn/rpc/rss) not extended —
   cross-crate contract with `DashboardState`; okx health fully
   visible via `/metrics`. Fold into the 8e TUI touch.
3. 24 h soak (§12 8b exit: seq-chain clean, drops=0) is operator-run;
   not started.
4. Everything uncommitted (see summary in chat); no git ops performed.

## 2026-08-14 (second entry) — 8a macOS-verified after seqlock fix; 8a BLESSED; 8b begins

- **Mac verification** (RustRover MCP, rustc 1.88.0 via rust-toolchain.toml):
  first-ever Mac run of the alloc suite FAILED
  `dashboard_snapshot_read_is_zero_alloc` — 1 alloc / 64 B in
  `SnapshotCell` publish+read; `cargo nextest run --workspace
  --no-fail-fast` = 524/525 with that as the sole failure.
- **Root cause** (confirmed in the toolchain's own std source): Darwin is
  absent from std's futex `Mutex` cfg list and falls back to the pthread
  implementation, which lazily heap-boxes its 64-byte `pthread_mutex_t`
  on first `lock()`. `SnapshotCell` was `Mutex<DashboardState>` → first
  `publish()` allocates once. Linux's futex `Mutex` never allocates, so
  the sandbox green was platform-conditional; the test could never pass
  on macOS with a std `Mutex`. Pre-existing since the TUI phase — not an
  8a regression; exposed by the first Mac gate run.
- **Fix (operator-approved):** `SnapshotCell` rewritten as a
  single-writer seqlock — `AtomicU64` version (odd = write in flight),
  Acquire-RMW enter / Release-store exit, reader volatile-copy +
  Acquire fence + version revalidation. `#[repr(C, align(64))]`,
  payload on its own cache line, `new()` now `const`, wait-free
  publisher, poison paths deleted, `tracing` dep dropped from `tui`.
  New failure-mode test: cross-thread torn-read hammer (100k publishes).
- **Post-fix verification:**
  - Mac: nextest **526/526**; release alloc gate **24/24 — 0 B/op now
    holds on macOS**.
  - Sandbox (fresh env, rustup 1.88 minimal at /tmp/rustup): workspace
    excl. bench **503/0**; release alloc gate **24/24**.
  - Count notes: +1 test = the new tui torn-read test; nextest runs no
    doctests (sandbox `cargo test` does) — accounts for prior 525/526 skew.
- **8a BLESSED** (operator directive "fix then straight to 8b").
  Uncommitted at time of writing: `crates/tui/src/lib.rs`,
  `crates/tui/Cargo.toml`, `crates/bench/tests/alloc_assertions.rs`
  (comment only), this file.

## 2026-08-14 — 8a Foundations: COMPLETE (pending operator go for 8b)

### Defect dispositions (D1–D10)

| # | Status | Disposition |
|---|--------|-------------|
| D1 | **Fixed** | `--polymarket-asset-id` is now a **required** CLI arg; the PM `SymbolMap` is built from `(asset_id, --polymarket-sym-id)` instead of `iter::empty()`. Venue-wide Gamma/CLOB REST discovery lands with the 8e boot coverage audit (same REST-client infrastructure). **CLI breaking change** — existing `run` invocations must add the flag. |
| D2 | **Disposed per plan §3.3** | Lane engine keeps `sig_cons` bound to the RPC ring ("as today"); RSS signals still drain at cli level. The real fix (RSS → claude-worker) is Stage 2 §8.1 and is out of Stage-1 scope by direction. |
| D3 | **Fixed** | Engine `tick()` now pumps `disp.try_next_fill()` into `Strategy::on_fill` (budgeted), plus drains four per-venue fill lanes. Fill-lane *producers* arrive with the 8j dispatchers. |
| D4 | **Fixed** | Every ingress `try_push` failure increments `IngressStatus::ring_drops`; mirrored every 5 s into `engine_ingress_<venue>_ring_drops_total` (+ msgs/bytes/parse_errors/gaps/resubscribes/reconnects). |
| D5 | **Fixed** | `last_activity_ns` now has a reader: `core_net::Keepalive::poll` forces `RunResult::IdleTimeout` after the per-venue idle budget (PM 30 s, BN 45 s, RPC 30 s). |
| D6 | **Fixed** | Proactive masked WS protocol pings on PM/BN (10 s / 15 s); RPC documents its `eth_blockNumber` poll as the probe and uses keepalive for the deadline only. OKX/Deribit/HL venue-specific ping bytes plug into the same `KeepaliveCfg` in 8b–8d. |
| D7 | **Fixed** | `core_metrics::IngressStatus` (`#[repr(C, align(64))]`, single-writer, Relaxed atomics) per ingress thread; state transitions published from inside the run loops (Up exactly at WS-upgrade). Gauges now carry 0=Down/1=Connecting/2=Up/3=Backoff; TUI `ingest_health` is a real per-thread bitmask. |
| D8 | **Fixed** | `core_net::Backoff` — capped exponential 500 ms → 8 s, equal-jitter (splitmix64, per-thread seed), reset only after a session that moved messages. Replaces the flat 500 ms sleep. |
| D9 | **Fixed** | All four PODs have fully explicit zeroed padding; `Tick.venue` @48, `Order.venue` @40; byte-level offset tests. `AsBytes` contract is now true. |
| D10 | **Fixed** | `TopOfBook::apply` → `ApplyOutcome {Applied, AppliedGap, Stale, WrongSymbol}` + `gaps()`/`stale_drops()` counters; `MultiBook::apply/apply_at` propagate. Caller owns resync policy per §6.2 (BN gaps legitimate; OKX chain breaks are not). |

### §3 structural work

- **core-types**: `VenueId` (u8, wire-stable), SymbolId namespacing
  (bits 31..24 venue, helpers `make_symbol_id`/`symbol_venue_byte`/
  `symbol_ordinal`/`symbol_bucket_mix`), venue byte in `Tick`/`Order`.
- **PMLR v2**: `VERSION = 2`; reader accepts v1 (venue-less) and
  exposes `version()`. `docs/wire-format.md` + `docs/migration.md`
  updated in the same change.
- **Engine lanes**: `tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; 5]`,
  `fill_lanes: [...; 4]`, `fill_lane_of(VenueId)`, fixed VenueId drain
  order, per-lane `max_per_ring` budget; staleness buckets use the
  venue-mixed hash. `TICK_RING_SIZE = 16_384` standardized (BN ring
  8192 → 16384). Unspawned venues = dropped-producer rings (§3.3).
- **core-net**: `write_client_handshake_with_headers` (+CR/LF-injection
  debug assert), `IoBuf` lifted public, `subs` module (generic
  `PendingTable`/`SubTable`/`SubId` + `queue_masked_{binary,text}_frame`),
  `Keepalive`/`KeepaliveCfg`/`KeepaliveAction`, `Backoff`.
  `ingress-rpc` refactored onto all of it (−155 LOC of duplication).
- **core-crypto** (new crate): handwritten SHA-256 / HMAC-SHA256 /
  base64; NIST FIPS 180-4 + RFC 4231 + RFC 4648 vectors; zero deps;
  core-net's WS handshake base64 dedupes onto it.
- **core-metrics**: `MAX_COUNTERS 64→256`, `MAX_GAUGES 128→384`,
  `/metrics` response buffer 64→128 KiB, `IngressStatus`/`IngressState`.
- **cli**: `Rings`/`Consumers` carry lane arrays; spawn wrappers own
  status slots + per-thread `Backoff` + venue `KeepaliveCfg`; metrics
  loop mirrors §6.4 counters as monotonic deltas.

### Verification (Linux sandbox, rustc 1.88)

- Workspace tests (excl. bench): **502 passed, 0 failed** — includes
  new tests for every 8a feature (happy + failure mode per house rule).
- `cargo test -p bench --test alloc_assertions --release -- --test-threads=1`:
  **24 passed, 0 failed — 0 B/op maintained** on PM/BN/RPC run-loop
  steady state (now including status/keepalive threading) and the
  lane-engine drain.
- Per-crate ingress suites include the new idle-timeout and
  masked-ping emission tests (PM/BN) and the Deribit-style
  silent-transport timeout for RPC.
- **Deviation:** `cargo-nextest` is not installed in the sandbox;
  plain `cargo test` was used. Operator should re-run
  `cargo nextest run --workspace` and `make alloc-assert` on the Mac
  before blessing 8a.

### Open issues / notes for next session

1. `--polymarket-asset-id` is now required (D1). Supply the market's
   CLOB token id; boot fails fast without it.
2. PM/BN keep their private `IoBuf` copies (rpc migrated). Migrate
   opportunistically when 8b-pattern work touches those files; new
   venues must use `core_net::IoBuf` from day one.
3. Venue REST discovery (OKX instruments, Deribit instruments, HL
   `/info`, PM Gamma) is 8b–8e work; SymbolId namespacing helpers are
   ready for it. Existing single-pair CLI flags still allocate plain
   ids — flag-driven ids migrate to `make_symbol_id` when discovery
   lands (8e coverage audit needs it).
4. Fill-lane producers intentionally absent until 8j; the engine's
   dispatcher pump is the only fill source in paper mode.
5. Sandbox disk is near-full; build caches live at /tmp/tt (target)
   and /tmp/ch (cargo home) with debuginfo off.

### Next

**8b — ingress-okx** (blocked on operator go): clone the ingress
template onto `core_net::subs` + `Keepalive`, channels `bbo-tbt`,
`trades`, `mark-price`, `funding-rate` (+`books` behind `--okx-depth`),
`seqId`/`prevSeqId` chain monitor honoring reset + idle-heartbeat
rules, literal `ping` keepalive at 25 s, REST instrument discovery,
proptest + fuzz targets (`okx_frame`, `okx_book_seq`), TLS loopback
tests with gap injection + idle timeout, alloc assertions.
