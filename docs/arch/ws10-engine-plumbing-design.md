# WS10 — Engine plumbing design: funding carrier + L2 depth to `Strategy`

**Status: DESIGN FOR OPERATOR REVIEW — no code lands until this doc
is approved** (stage2-finish-plan WS10: these two changes touch
`strategy-core`, the rings and the wire format — the highest-blast-
radius items in the whole Stage-2 finish list).

Written 2026-08-29, in the WS2–WS9-coded tree (six venues, tick
lanes 0–5, `ChannelId` 0–12). Everything below is sized against that
tree.

---

## 1. What exists today (the gap, precisely)

- **Funding** is parsed on four venues (OKX `funding-rate`, Deribit
  perp tickers since WS3, Binance USDM `@markPrice` since WS5, Bybit
  linear `tickers` since WS9) and becomes capture-only
  `ChannelId::Funding` events in `<venue>-events.pmlr`. No engine
  ring type carries it; the `Strategy` surface is
  `on_tick / on_signal / on_fill / AiCmd / on_ruleset_table /
  on_timer`.
- **L2 depth**: the OKX `books` (400 levels) and Deribit
  `book.100ms` channels are HEADER-only capture (seq-chain
  integrity + counts); `book-builder` is top-of-book fed by the
  `Tick` stream. Bybit WS9 deliberately subscribes `orderbook.1`
  only. No depth callback exists.

## 2. Design A — the funding carrier (RECOMMENDED to land first)

### 2.1 Shape: per-venue **venue-event lanes** carrying `ChannelEvent`

Reuse the existing 64-byte `#[repr(C)] Copy` **`ChannelEvent`** POD
as the ring slot — the §6.5 capture record IS the right carrier
in-process too (`ts_ns, sym, venue, channel, venue_seq,
venue_time_ms, v0, v1`; funding = `channel 3, v0 = rate ×1e9, v1 =
next-funding ms` — cross-venue semantics already pinned in
docs/wire-format.md by WS3/WS5/WS9). No new wire type, no
wire-format change, nothing to migrate.

- **Rings**: `NUM_EVENT_LANES = NUM_TICK_LANES = 6`,
  `Ring<ChannelEvent, EVENT_RING_SIZE = 1024>` per venue, same
  boot-split pattern as tick lanes (unspawned venue ⇒ producer
  dropped ⇒ permanently-empty ring, two atomic loads per iteration).
  1024 slots × 64 B × 6 = 384 KiB boot-time, cache-aligned.
- **Producer side**: each ingress thread gains ONE extra `try_push`
  at the exact sites that already build the funding `ChannelEvent`
  for capture (OKX funding arm, Deribit ticker arm, BN markPrice
  arm, Bybit tickers arm). Capture stays first (the §6.5
  capture-before-push law); ring-full drops count into a new
  `IngressStatus.event_ring_drops` (or reuse `ring_drops`? — NO:
  separate counter, funding loss ≠ tick loss; slot still ≤ 128 B at
  95 B of fields).
- **Gating**: a per-ingress `event_lane_mask: u16` (bitmask over
  `ChannelId`) chosen at spawn: v1 ships with ONLY
  `1 << Funding as u16` set, so the lane carries funding and nothing
  else. Mark/OI/DVOL can be flipped on later without new plumbing —
  this is the reviewable knob that keeps the lane from becoming an
  unbounded firehose.
- **Engine drain**: after the tick lanes, before the AI lane; budget
  `max_per_ring` like ticks. Dispatch to a NEW default-no-op
  callback:

  ```rust
  fn on_venue_event<C: Ctx>(&mut self, _e: &ChannelEvent, _c: &mut C) {}
  ```

  Default no-op keeps every existing strategy + the frozen worker
  surface untouched (the trait gains a defaulted method — no
  implementor changes, `strategy-set` forwards it to members like
  `on_tick`).
- **Ruleset VM**: NOT in scope — the vm's row grammar stays frozen;
  a funding-aware trigger is a research-side proposal for its own
  slice (worker `backtest.py` untouched by law).

### 2.2 Zero-alloc / latency accounting

One `try_push` of a Copy 64-B slot per funding update
(1–10 Hz/venue — noise vs the tick path); engine adds one
empty-ring check per venue per iteration (6 × 2 relaxed loads).
Alloc gate: the existing bench harness grows one assertion (event
lane push + drain = 0 B/op). No branches added to the tick
dispatch path itself.

### 2.3 Blast radius (files)

`core-types` (nothing — ChannelEvent reused) · `engine`
(lanes array + drain + `Engine::new` signature ⇒ bench/wiring-test
lane arrays again) · `strategy-core` (defaulted method) ·
`strategy-set` (forward) · 4 ingress crates (one push each +
counter) · `core-metrics` (one counter) · `cli` (ring split + spawn
plumbing + producer handoff into the 4 spawns + metrics mirror).
Estimated ~1 day, mechanical after WS9's lane precedent.

## 3. Design B — L2 depth to `Strategy` (land SECOND, own slice)

### 3.1 The honest constraint

OKX `books` and Deribit `book.100ms` are DELTA streams: getting real
depth to a strategy requires maintaining a book per instrument
**inside the ingress thread** — moving (bounded) book maintenance
into the hot path. That is the cost; a design that pretends deltas
can be forwarded raw just moves the same work into the engine thread
with worse cache behavior. Proposal:

- **In-ingress bounded ladder** per subscribed-depth instrument:
  fixed arrays `[(px_1e6, qty_1e6); DEPTH_LADDER_CAP = 64]` per
  side, price-sorted, in-place delta application (insert/replace/
  delete by linear scan — 64 entries, one cache line group;
  measured before widening). Overflow beyond 64 levels: track a
  beyond-cap count (the Deribit DEPTH_CAP excess precedent) —
  conservative, never wrong about the top.
- **Carrier POD**: `DepthTopK` — 192 bytes:

  ```text
  ts_ns u64 · sym u32 · venue u8 · k u8 · flags u8 · _pad u8
  bids [(i64 px, i64 qty); 5] · asks [(i64 px, i64 qty); 5]
  ```

  Top-5 per side (K = 5 covers the research asks on the table;
  ladder cap 64 is the maintenance bound, K is the carrier bound).
  New `slot_kind = 7` IF we also capture it (recommended: capture
  the top-K stream to `<venue>-depth.pmlr` — it is exactly the
  research feed the gaps doc wants) ⇒ wire-format + migration
  entries, PMLR container version unchanged.
- **Emission**: push only when the top-K changed (byte-compare of
  the previous emitted slot — 192-B memcmp on book update, cheap) —
  book channels run 10–20 Hz/instrument on our venues.
- **Ring + callback**: per-venue `Ring<DepthTopK, 4096>` (subscribed
  venues only — OKX/Deribit behind their existing `--*-depth`
  flags), `Strategy::on_depth(&DepthTopK)` defaulted no-op.
- **Integrity**: the existing seq-chain monitors stay the law; a
  chain break resyncs (existing behavior) AND clears the ladder +
  emits a `flags = STALE` DepthTopK so a strategy never trades a
  known-broken book.

### 3.2 Blast radius (files)

`core-types` (DepthTopK + size checks) · `core-io` (slot kind 7 +
capture file) · okx/deribit ingress (ladder module + emission; the
400-level OKX parse walk grows a level extractor — today only the
header is parsed) · engine/strategy-core/strategy-set/cli as in A ·
wire-format/migration/audit-replay (depth file stats). Estimated
1.5–2 days including the ladder proptests (apply random delta
sequences vs a BTreeMap reference model).

## 4. Sequencing + review asks

1. **A first** (small, mechanical, immediately useful to research);
   **B second** as its own commit series.
2. Both land dark: no strategy consumes the callbacks in Stage 2 —
   the deliverable is the PLUMBING (gaps §1), proven by unit tests +
   the WS13 alloc/soak gates. The first consumer is Stage-3/M5
   research work.
3. **Operator decisions requested:**
   - D-A1: approve the `ChannelEvent`-as-carrier reuse (vs a
     dedicated `FundingUpdate` POD — rejected above for wire-format
     economy; say the word and B's dedicated-POD shape applies to A
     too).
   - D-A2: `EVENT_RING_SIZE` 1024 ok? (funding cadence says yes.)
   - D-B1: approve slot_kind 7 + `<venue>-depth.pmlr` capture (the
     research feed), or keep depth in-process only.
   - D-B2: K = 5 / ladder cap 64 ok?
   - D-B3: confirm B waits for A to land + soak, or run both in one
     series.

**Until the operator answers, WS10 stays design-only; WS11/WS12
proceed (they do not depend on WS10).**
