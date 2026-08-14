# Phase 2 — Book builder + Strategy + Engine wire-up

Status: **complete** (2026-05-19). All §2 deliverables landed.
Final tally: **280 tests across 28 binaries**, 14 alloc assertions
@ 0 B/op, 10 fuzz targets, clippy clean. Paper-mode binary boots
the real `Engine<LatencyArb<8>, PaperDispatcher>` via the four
ingress threads.

This is the plan for the trading core: turn the four ingress
streams (Polymarket ticks, Binance ticks, RPC newHeads signals,
RSS news signals) into would-be orders via a real strategy. Phase 2
stays in **paper mode** — the order goes into a paper-only ring;
no signer, no clob-dispatcher, no real fills. Live trading is
Phase 3.

## Scope

* `crates/book-builder` — promote the stub `TopOfBook` to a
  `MultiBook` that stores N book slots indexed by `SymbolId`. Each
  slot is still a top-of-book; full L2 ladders are deferred to
  Phase 3 if the strategy proves out without depth.
* `crates/strategy-latency-arb` — implement the actual leg-B logic:
  cross-venue mid-vs-mid compare against a fixed-point threshold,
  emit `Order` via `ctx.submit`. One Binance symbol → one Polymarket
  market for v1; the symbol map is the only call-site that
  allocates and only at boot.
* `crates/engine` — switch from one `tick_cons` to **two**
  `tick_cons` (Polymarket + Binance), drain both per iteration,
  dispatch to `Strategy::on_tick`. Signals + fills stay as today.
* `crates/cli` — replace `drain_and_count_loop` with a real engine
  loop wired up via the same four ingress threads. Paper-mode
  output: per-5s tick / signal / would-be-order counts logged
  via `tracing`.

Explicitly out of scope (Phase 3 or later):
* `signer-eip712` (real ECDSA signing). Phase 3.
* `clob-dispatcher` (HTTP/2 to Polymarket). Phase 3.
* Full L2 ladder. Phase 3 only if the strategy needs depth.
* TUI dashboard. Phase 4.
* Claude-worker artifacts. Phase 5.

## Non-negotiables (all carry over)

* **Zero alloc in steady state.** `MultiBook::apply` and
  `LatencyArb::on_tick` must compile down to a fixed instruction
  count with no `Vec::push`, no `format!`, no `dyn`.
* **No dynamic dispatch.** Strategy stays generic via the existing
  `Engine<S: Strategy, D: OrderDispatch>` monomorphization.
* **Compile-time configured size limits.** The number of tracked
  symbols is a `const` chosen at boot; `MultiBook` is
  `MultiBook<const N: usize>`.
* **Every new public fn has rustdoc.** `#[deny(missing_docs)]`
  stays on.
* **Every parser-like fn has at least one unit test + a property
  test where useful.** Apply functions also get an alloc assertion.

## Deliverables

### 2.1 `book-builder::MultiBook<const N: usize>`

```rust
pub struct MultiBook<const N: usize> {
    /// Fixed-capacity table of (SymbolId, TopOfBook) entries.
    /// `sym == SYMBOL_ID_NONE` means the slot is free.
    entries: [TopOfBook; N],
    /// Number of populated slots. Lookup walks 0..count linearly.
    count: u32,
}

impl<const N: usize> MultiBook<N> {
    pub const fn empty() -> Self;
    /// Insert a fresh symbol slot. Allocations at boot only.
    /// Returns `Err(BookFull)` if the table is at capacity.
    pub fn track(&mut self, sym: SymbolId) -> Result<(), BookFull>;
    /// Apply a tick. Linear scan over `count` slots; cache-warm.
    pub fn apply(&mut self, tick: &Tick);
    /// Snapshot the current top-of-book for `sym`, if tracked.
    pub fn snapshot(&self, sym: SymbolId) -> Option<TopOfBook>;
    /// Iterate over all tracked books.
    pub fn iter(&self) -> impl Iterator<Item = &TopOfBook>;
}
```

* `N` is small (≤ 64 in v1) — linear scan beats a `HashMap`.
* `apply` ignores ticks for untracked symbols (silent drop), so the
  same stream can feed multiple consumers.
* Cache-aligned: each slot is one cache line, the array is also
  cache-aligned.

### 2.2 `strategy-latency-arb::LatencyArb`

State (no heap after boot):

```rust
pub struct LatencyArb<const N: usize> {
    /// Polymarket → Binance symbol map. Built at boot.
    /// `polymarket_sym → binance_sym` (None if not paired).
    venue_map: SymbolPairTable<N>,
    /// One book slot per tracked Polymarket symbol.
    book: MultiBook<N>,
    /// Latest Binance mid by Binance SymbolId.
    binance_mid: [Price; N],
    binance_seen: [bool; N],
    /// Threshold in fixed-point 1e6 units. Single value v1; per-
    /// symbol in v2.
    threshold_1e6: i64,
    /// Order qty in fixed-point.
    qty: Qty,
    /// Cooldown ns — minimum time between emitted orders per
    /// Polymarket sym. Default 250 ms.
    cooldown_ns: u64,
    last_emit_ns: [u64; N],
    /// Cumulative counters for paper-mode summaries.
    pub ticks_seen: u64,
    pub orders_emitted: u64,
}
```

Decision rule (Phase 2 v1):

```text
on Polymarket tick T (sym = ps):
  book.apply(T)
  let bs = venue_map.binance_for(ps)?      // None → ignore
  if !binance_seen[bs] { return }
  let pm_mid = book.snapshot(ps).mid()
  let bn_mid = binance_mid[bs]
  let delta = (pm_mid - bn_mid)            // i64, 1e6-scaled
  if delta.abs() < threshold_1e6 { return }
  if (now - last_emit_ns[ps]) < cooldown_ns { return }
  // Polymarket is rich relative to Binance → SELL ps.
  // Polymarket is cheap relative to Binance → BUY ps.
  let side = if delta > 0 { Side::Ask } else { Side::Bid };
  let order = Order::new(now, ps, side, ..., px=pm_mid, qty=self.qty, ...)
  ctx.submit(order)?
  last_emit_ns[ps] = now
  orders_emitted += 1

on Binance tick T (sym = bs):
  binance_mid[bs] = T.mid()
  binance_seen[bs] = true
```

* Pure compute, fully inlined, no branches that depend on heap.
* `SymbolPairTable<N>` is a small `[(SymbolId, SymbolId); N]`
  linear-scan map. Inserts at boot only.

### 2.3 `engine` — two tick consumers

Change the `Engine` signature to accept two tick consumers:

```rust
pub struct Engine<S: Strategy, D: OrderDispatch> {
    strat: S,
    disp: D,
    pm_tick_cons: Consumer<Tick, PM_TICK_RING_SIZE>,
    bn_tick_cons: Consumer<Tick, BN_TICK_RING_SIZE>,
    sig_cons:     Consumer<Signal, SIGNAL_RING_SIZE>,
    fill_cons:    Consumer<Fill,   FILL_RING_SIZE>,
    last_timer_ns: NsTs,
    pub iterations: u64,
}
```

`tick(max_per_ring)` drains both tick rings each iteration. The
strategy is the dispatch arbiter — it inspects `tick.sym` and
routes internally.

Sizes:
* `PM_TICK_RING_SIZE = pwl::DEFAULT_TICK_RING_CAP` (16 384)
* `BN_TICK_RING_SIZE = bwl::DEFAULT_TICK_RING_CAP` (8 192)
* No change to `SIGNAL_RING_SIZE` / `FILL_RING_SIZE`.

### 2.4 `cli` — paper-mode engine wiring

`cli::paper::engine_loop` replaces `drain_and_count_loop`:

* Allocate all four ingress rings as today.
* Allocate the order ring + a `PaperDispatcher` from
  `clob-dispatcher`.
* Build `LatencyArb<N>` with a hard-coded symbol pair from CLI args
  or config (Phase 2 ships one pair: `("BTCUSDT", polymarket BTC>$100k
  market id, threshold=2_000_000 (= $2.00))`).
* Build `Engine::new(strat, disp, pm_tick_cons, bn_tick_cons,
  sig_cons, fill_cons)`.
* Loop: `engine.tick(256)` + 1 ms park. Every 5 s emit a one-line
  summary (ticks drained, orders emitted, fills observed).
* On SIGINT, `engine.stop()` and reverse-order thread join.

### 2.5 Test surface

* `book-builder::MultiBook` unit tests:
  - `apply` updates the right slot
  - `apply` ignores untracked symbols
  - `track` returns `BookFull` on overflow
  - `snapshot` returns `None` for untracked
  - cache alignment
* `strategy-latency-arb` unit tests:
  - threshold trigger on rich Polymarket → Ask
  - threshold trigger on cheap Polymarket → Bid
  - sub-threshold → no order
  - cooldown suppresses duplicate orders
  - missing Binance mid → no order
* `engine` updated unit tests:
  - Counter strategy still drains a single tick ring
  - Two-ring drain calls `on_tick` for each
* `bench/tests/alloc_assertions.rs` — two new assertions:
  - `multi_book_apply_is_zero_alloc`
  - `latency_arb_on_tick_is_zero_alloc`
  Brings total to **14 alloc assertions**.
* No new fuzz targets — apply paths take POD inputs, not byte
  streams.

## Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` — **280 tests across 28 binaries** ✔ (+28 from Phase 1c)
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` — **14 assertions pass, 0 B/op** (+2:
      `multi_book_apply`, `latency_arb_on_tick`)
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
      (still 10 fuzz targets — no new parsers)
- [x] `cargo run --release -p cli -- run --paper` wired through
      `Engine::tick` + `LatencyArb<8>` + `PaperDispatcher` —
      manual live check still pending (no live network in
      sandbox).
- [x] `docs/phase-2-plan.md` flipped to **complete**
- [x] Memory file refreshed with Phase 2 gotchas

## Sequencing

1. **MultiBook + tests** — smallest unit, lands first. ~150 LOC.
2. **Engine: two tick consumers + tests update** — refactor.
   ~80 LOC change.
3. **LatencyArb logic + tests** — meatiest. ~300 LOC.
4. **Alloc assertions** — 2 new tests in
   `crates/bench/tests/alloc_assertions.rs`. ~80 LOC.
5. **CLI wiring** — replace drain-and-count with engine loop.
   ~150 LOC change.
6. **Sweep + docs + memory refresh.**

## Risks / open questions

* **Symbol mapping.** The Binance → Polymarket pairing is hand-
  curated in v1. Wrong pairing → no edge. Mitigation: ship one
  hard-coded pair, validate by hand against live markets, expand
  once the orchestration is proven.
* **Mid-vs-mid is naive.** Real edge is mid-vs-best-bid (when
  selling) and mid-vs-best-ask (when buying), with fee subtraction.
  v1 ships mid-vs-mid; v2 refines once we see paper P&L.
* **No latency budget enforcement.** Phase 2 doesn't yet kill an
  order if the Binance tick is > T ns old. Phase 3 adds a freshness
  check before `ctx.submit`.
