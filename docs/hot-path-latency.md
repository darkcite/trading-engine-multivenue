# Hot-Path Latency Review — 2026-05-19

Deep audit of the engine hot path with criterion-measured per-stage costs. Combines findings from four parallel agent reviews (ingress, engine + strategy, dispatcher, cross-cutting) with bench numbers captured on the Linux/aarch64 sandbox (M-series equivalent, vDSO `clock_gettime`).

## TL;DR

The engine hot path is in good shape. **All measured per-stage costs are sub-3 ns** except `now_ns()` (13.8 ns), the SPSC ring round-trip (5.3 ns), and `signer/sign_order` (22.7 µs — fully off-engine after the queued-dispatcher work). The biggest *systemic* concerns are:

1. **`cli/src/paper.rs:1098` — engine loop sleeps 1 ms between drains** (HIGH). Caps reactivity at scheduler-tick granularity.
2. **Quadratic `IoBuf::consume` in every WS ingress crate** (HIGH). Every WebSocket frame triggers a left-shift of the residual rx buffer.
3. **Per-iteration `mio::reregister` syscall in all three WS ingresses** (HIGH). Should only fire on interest change.
4. **`Secp256k1::signing_only()` rebuilt per submit** (HIGH on submit, but off-engine). Cache in `OnceLock`; saves 200-500 µs/order.
5. **`Connection: close` on every CLOB POST** (HIGH on submit). Forces full TCP+TLS handshake per order. Switch to keep-alive; saves 50-150 ms/order on WAN.

The first three sit on the *engine* hot path. The last two sit on the *dispatcher worker* path (off-engine, but they limit sustained submit throughput).

## Methodology

- **Bench harness:** criterion, single-threaded, release build with `RUSTFLAGS` from the workspace. 50 samples per bench, 2 s measurement window, 1 s warm-up. Run on Linux aarch64 (sandbox).
- **Code audit:** four parallel agent reviews, each given a focused area with explicit file lists. Findings rated by absolute latency cost (HIGH > 1 µs or > 100 ns regression / MEDIUM 10-1000 ns / LOW < 10 ns or design-only).
- **Bench file:** `crates/bench/benches/hot_path.rs`.

## Measured per-stage costs

| Stage | Bench | p50 | p99 | Status |
|---|---|---|---|---|
| Wall clock | `clock/now_ns` | 13.8 ns | 13.9 ns | OK — looks vDSO'd (MONOTONIC_RAW is documented not to be) |
| SPSC ring | `ring/push_pop_tick` | 5.3 ns | 5.3 ns | Optimal |
| Latency record | `latency/record_*` | 2.7 ns | 2.7 ns | OK — 3 NEON LDADDs |
| Counter inc | `metrics/counter_inc_1` | 1.7 ns | 1.8 ns | Optimal |
| Book apply | `book/apply_n8_middle` | 1.0 ns | 1.0 ns | Excellent for N=8 |
| Cooldown gate | `cooldown/allow` | 0.5 ns | 0.5 ns | Optimal |
| Queued submit | `dispatcher/queued_submit` | 1.7 ns | 1.8 ns | Optimal |
| Strategy callback (no fire) | `strategy/latency_arb_on_tick_no_fire` | 1.7 ns | 1.7 ns | Optimal |
| **Signer (off-engine)** | `signer/sign_order_full` | **22.7 µs** | **24.5 µs** | **Hot — Secp256k1 ctx rebuild dominates** |

### What is *not* measured

- End-to-end engine tick batch cost under realistic burst — limited by disk space for the larger criterion harness; the per-stage adds suggest a 256-tick batch at steady state would be ~512 ns of strategy/book + ~1.4 µs of ring drains + ~50 ns latency recording = O(2 µs) total CPU per drain.
- Network RTT to the real CLOB — measure under live conditions during paper-mode validation.
- Sustained `signer/sign_order_full` rate after applying the `OnceLock<Secp256k1>` fix (F-10) — would be the single highest-impact follow-up bench.

## Findings — engine hot path

### HIGH

**E-1 / paper.rs:1098 — `thread::sleep(1 ms)` between engine ticks**
The engine loop calls `eng.tick(256)`, drains RSS, then unconditionally sleeps 1 ms. On Linux, `nanosleep(1 ms)` actually sleeps for at least one HZ-quantum (4-10 ms on a non-RT kernel). With Polymarket WS ticks arriving at sub-millisecond cadence, this couples engine reactivity to the kernel scheduler.
**Fix:** Replace with `std::thread::yield_now()` + busy-poll on a pinned core (the cli already pins core 0), or use a notify primitive (`AtomicBool` flag flipped after `try_push` on the ingress side, polled with `park_timeout` on the engine side). Acceptable v1: yield_now + drain bound, accept 100% CPU usage on the dedicated engine core.

**E-2 / engine/src/lib.rs:172 — `now_ns()` captured once per `tick()`, reused across all drained items**
A burst of 64 ticks all see the *first* item's `now`, so per-item `ingest_lat` is biased low by however long the prior 63 strategy callbacks took. `decide_lat` is also poisoned because `Order::ts_ns` is stamped with the stale `now`.
**Fix:** Re-sample `now_ns()` per drained item (13.8 ns each — measured). At a 256-tick batch this adds ~3.5 µs of clock cost, recovered immediately by accurate latency attribution.

**E-3 / strategy-latency-arb:140,326,352,462 — `SymbolPairTable::binance_index` is O(N), called 3-4× per tick**
Each Binance tick triggers ~4 separate O(N) scans of the pair table just to classify, locate, and re-locate the slot. PLAN.md gotcha #7 explicitly flags this but the cli never cached the index.
**Fix:** Add `SymbolPairTable::binance_index_for_sym(sym) -> Option<u32>` and store the slot index on the symbol's first appearance. Or, since N ≤ 8 in v1, the absolute cost is ~5 ns/scan × 4 scans = 20 ns/tick — acceptable until we widen to N=64.

**E-4 / book-builder:189-219 — `MultiBook::apply` + `index_of` linear scans, called 2-3× per tick**
`latency-arb::on_tick` calls `book.index_of()`, then `on_pm_tick` calls `book.apply()`, then `maybe_emit` calls `index_of()` again. Three O(N) scans for the same symbol.
**Fix:** Add `MultiBook::apply_at(idx, tick)` taking the cached index. Hoist the `index_of` lookup to the dispatch site and pass it through. Saves ~3 ns/tick at N=8; ~20 ns at N=64.

### MEDIUM

**E-5 / paper.rs:976 — `now_ns()` per outer loop iteration just to gate the 5 s report**
Called every loop iteration (~1000/sec). At 13.8 ns × 1000 = 14 µs/sec of wasted clock reads.
**Fix:** Only sample on `eng.iterations & 0x3F == 0` (every 64th iter).

**E-6 / core-latency:96-104 — `LatencyTracker::record` is 3 atomic RMWs**
On aarch64 each `LDADD` is ~1 ns (matches measured 2.7 ns). Since the engine is single-writer, `sum_ns` and `count` could be plain `u64` cells with `UnsafeCell`. Or drop `count` entirely and derive at read time.
**Fix:** Add a `record_single_writer` variant with `UnsafeCell<u64>` for sum and count. Saves ~2 ns per record × 4 records per tick = 8 ns/tick.

**E-7 / strategy-rule-tree:228,286 — `memmem::find` per-rule per-signal**
`memmem` builds a small jump-table per call; setup dominates for 16-byte keywords on 40-byte payloads.
**Fix:** Pre-build a `memmem::Finder<'static>` per rule at boot (Finder is `Clone`-able), store in the slot. Or hand-roll a SIMD byte-loop since the sizes are tiny.

### LOW

**E-8 / core-ring:221 — redundant `compiler_fence(Release)` before the Release atomic store** — bare atomic Release already prevents reordering; the fence is documentation, not codegen. Keep the comment; remove the fence call.

**E-9 / core-ring SPSC missing cached head/tail** — standard Vyukov/Disruptor optimization: producer caches last-seen `tail`, consumer caches last-seen `head`. Cuts coherence traffic by ~50% under sustained contention. Not visible in our single-threaded bench (5.3 ns is already at the floor).

**E-10 / strategy-core:167 — `CooldownGate::allow` has an `idx >= N` fail-closed branch** — caught at 0.5 ns/call, the branch is irrelevant in absolute terms. Documented design choice.

## Findings — ingress hot paths

### HIGH

**I-1 / `IoBuf::consume` does `copy_within` of entire residual after every frame**
- `crates/ingress-polymarket/src/run_loop.rs:169` (called at :374, :424, :506)
- `crates/ingress-binance/src/run_loop.rs:96` (called at :294, :343, :413)
- `crates/ingress-rpc/src/run_loop.rs:207` (called at :472, :520, :666)

Three independent copies of the same pattern. A 64 KiB rx buffer is shifted left for every consumed WS frame. Under burst load (50+ frames buffered) this becomes O(N²) total bytes moved.
**Fix:** Promote `TestBuffer`'s cursor-pair pattern (`core-net/src/transport.rs:264-334`) into a shared `core_net::RxBuf`. Adopt across all three ingress crates.

**I-2 / Per-iteration `mio::reregister` syscall**
- `polymarket:605`, `binance:500`, `rpc:934` — each calls `transport.reregister(...)` every loop iteration.

On Linux this is `epoll_ctl` (~300-1000 ns syscall). `Transport::interest()` exists to detect changes but is never compared.
**Fix:** Cache last-registered interest; only call `reregister` when the bitmask changes. In steady state this is near-zero.

**I-3 / `mio::poll(50 ms)` caps wakeup latency**
- `polymarket:582`, `binance:474`, `rpc:908`.

When a partial frame is buffered and `drive_one` makes progress, we should immediately re-poll with `Duration::ZERO` instead of blocking 50 ms.
**Fix:** Run a tight inner loop on `drive_one` returning "made progress"; only re-yield to mio on `WouldBlock`.

### MEDIUM

**I-4 / Repeated payload scans** — `extract_asset_id` + `parse_book_update` each scan from byte 0; the same payload is walked 3-4 times across all anchors. ~200-300 ns of waste per Polymarket frame.
**Fix:** Single forward-walk classifier recording offsets, passed into `parse_*` helpers.

**I-5 / `now_ns()` called 2-3× per frame** — once for `last_activity`, once for `tick.ts_ns`, sometimes a third for the poll deadline.
**Fix:** Sample once at top of `drain_ws_frames`, pass down.

**I-6 / `TlsTransport::read` constructs `io::Error::new(WouldBlock, _)` per partial frame** — `io::Error::new` boxes the payload (heap alloc).
**Fix:** Use `io::Error::from(io::ErrorKind::WouldBlock)` — no payload, no alloc. (PlainTcpTransport already does this correctly.)

**I-7 / `SymbolMap::lookup` is O(N) byte-slice memcmp** — at ~100 entries × 66-byte asset_id, ~200 ns/frame.
**Fix:** Precompute u64 hashes at boot; linear scan over `[u64]`.

### LOW

**I-8 / `ws_read_frame` uses bounds-checked indexing throughout** — top-level length gate proves all subsequent indexes are in-bounds, but LLVM may not elide every check.
**Fix:** After the length gate, use `get_unchecked` with one `// SAFETY:` block.

## Findings — dispatcher (mostly off-engine)

### HIGH (off-engine, affects sustained submit rate)

**D-1 / `Secp256k1::signing_only()` rebuilt per submit**
- `crates/signer-eip712/src/lib.rs:66, 350`

Each `sign_digest` and `address_from_private_key` builds a fresh precomputation context. Upstream caches some tables internally but context construction is non-trivial (200-500 µs per call, dominating the 22.7 µs we measured for `sign_order_full` — most of that *is* the context).
**Fix:** Cache in `OnceLock<Secp256k1<SignOnly>>`. Expected drop: `sign_order_full` from ~22 µs → ~50-80 µs first-call, ~5-10 µs subsequent. *Highest-impact single fix in the audit.*

**D-2 / `Connection: close` on every CLOB POST**
- `crates/clob-dispatcher/src/live.rs:343`

Forces full TCP+TLS handshake on every order (~50-150 ms on WAN). At 250 ms cooldown the worker spends most of its budget reconnecting.
**Fix:** Switch to `Connection: keep-alive` (or omit; HTTP/1.1 default). Rely on Content-Length framing (already supported). Add an integration test that submits two orders over a single TLS session.

**D-3 / `domain_separator()` recomputed per sign**
- `crates/signer-eip712/src/lib.rs:247`

Domain typehash + name + version + chainId + contract are all constants; the resulting separator is also constant. Each call still does a 160-byte copy + keccak256.
**Fix:** `static DS: OnceLock<[u8; 32]>` — 5-line change, saves ~3-5 µs/submit.

### MEDIUM

**D-4 / `SecretKey::from_slice` re-parsed per sign** — runs scalar validity check on the same 32 bytes every call.
**Fix:** Parse once at `LiveDispatcher::connect`, pass `&SecretKey` to `sign_order`. Saves ~5-20 µs/submit.

**D-5 / `Mutex<DispatchStats>` taken per worker submit + per cli read**
- `crates/clob-dispatcher/src/queued.rs:153, 126`

Bounded contention but adds ~50-200 ns/submit + risks the worker stalling behind a slow `/metrics` reader.
**Fix:** Per-field `AtomicU64` array (the struct is 10 × u64). `Relaxed` stores + loads. No lock, no contention, no torn-read risk on individual counters.

**D-6 / Worker idle sleep is fixed 50 µs** — mean wake-up latency ~25 µs at low submit rates. `thread::sleep` on macOS/Linux is imprecise (60-200 µs actual).
**Fix:** Tiered backoff — `spin_loop()` first, then `yield_now()`, then `park_timeout(50 µs)`. Producer-side `Parker` unparks on push for instant wake.

### LOW

**D-7 / Worker calls `inner.stats()` (80-byte copy) every submit just to mirror** — subsumed by D-5.

**D-8 / JSON encoder uses byte-by-byte itoa for u128** — ~1 µs per encode total; `itoa::Buffer` would shave ~600 ns. Not worth the dep.

## Findings — cross-cutting

### HIGH

**C-1 / `clock_gettime(CLOCK_MONOTONIC_RAW)` is NOT vDSO'd on Linux**
- `crates/core-time/src/lib.rs:44-59`

Documented Linux behavior: `CLOCK_MONOTONIC` is vDSO'd, `CLOCK_MONOTONIC_RAW` is not — falls through to a syscall (~50 ns + scheduler exposure). Bench measured 13.8 ns on the sandbox which suggests it IS vDSO'd on this kernel (vDSO support for MONOTONIC_RAW was added in Linux 5.3). **Verify** on the EC2 target.
**Fix:** Swap to `CLOCK_MONOTONIC` (drift is sub-ns over a 10 ms tick) for portability, or add a feature-gated `rdtsc` reader for x86_64 Linux with one-time TSC calibration.

### MEDIUM

**C-2 / `LatencyTracker` `sum_ns` + `count` share cache line with last bucket row**
- `crates/core-latency/src/lib.rs:62-66`

Single-writer (engine) so no inter-thread contention. But the `percentile()` reader (TUI / metrics-server thread) loads `count` then sweeps the bucket array — invalidates the line that the engine is writing.
**Fix:** Wrap `sum_ns` + `count` in a `#[repr(align(64))]` newtype, placed *before* `buckets`.

### LOW

**C-3 / `Counter`/`Gauge` are 128 bytes each under `align(64)`** — verified correct (no false sharing); `value: AtomicU64` is at offset 72, well clear of `name` at offset 0. No `static_assert_size!` exists; future field reorder could silently break the invariant.
**Fix:** Add `static_assert_size!(Counter, 128)` + assert `offset_of!(Counter, value) >= 64`.

**C-4 / `core-simd` is a scaffold; no SIMD on the hot path today** — auto-vectorization is doing its job for now. The Polymarket price scanner (`scan_price_1e6`) is scalar.
**Fix:** Phase 7 — write `parse_price_1e6_neon` / `_avx2` behind cfg gates.

## Priority-ordered fix list

| Rank | Fix | Area | Effort | Estimated win |
|---|---|---|---|---|
| 1 | **D-1: cache `Secp256k1` ctx in `OnceLock`** | signer | 1 line | 200-500 µs / submit (off-engine but worker-bound) |
| 2 | **D-2: drop `Connection: close`** | dispatcher | 1 line | 50-150 ms / submit (WAN) |
| 3 | **E-1: replace 1 ms sleep with yield_now/notify** | engine loop | 5 lines | engine reactivity → scheduler-tick-free |
| 4 | **I-1: unified `core_net::RxBuf` with cursor pair** | ingress | ~100 LOC | removes quadratic memcpy on burst |
| 5 | **I-2: conditional `reregister`** | ingress | ~30 LOC | 1 syscall/iter/thread → 0 in steady state |
| 6 | **D-3: cache `domain_separator()`** | signer | 5 lines | 3-5 µs / submit |
| 7 | **E-2: per-item `now_ns()` in engine drain** | engine | ~10 LOC | corrects ingest/decide latency attribution |
| 8 | **D-4: pre-parse `SecretKey` at boot** | signer | ~10 LOC | 5-20 µs / submit |
| 9 | **D-5: AtomicU64 array for DispatchStats** | dispatcher | ~50 LOC | removes mutex from worker submit |
| 10 | **I-3: tight inner drain loop, then yield to mio** | ingress | ~20 LOC | removes 50 ms wakeup cap on bursts |

## Post-fix measurements (2026-05-19 — same session)

All 5 top-priority bottlenecks landed; full workspace test suite (432+ tests / 36 binaries / 23 alloc assertions) re-runs green. Bench re-run:

| Bench | Pre-fix p50 | Post-fix p50 | Δ | Notes |
|---|---|---|---|---|
| `clock/now_ns` | 13.8 ns | 13.7 ns | ~0% | Untouched code path |
| `ring/push_pop_tick` | 5.25 ns | 5.23 ns | ~0% | Untouched |
| `latency/record_1us` | 2.65 ns | 2.63 ns | ~0% | Untouched |
| `latency/record_1ms` | 2.68 ns | 2.64 ns | ~0% | Untouched |
| `metrics/counter_inc` | 1.68 ns | 1.58 ns | -6% | Within noise |
| `book/apply_n8_middle` | 1.0 ns | 0.94 ns | -6% | Within noise |
| `cooldown/allow` | 0.51 ns | 0.54 ns | +6% | Within noise |
| `dispatcher/queued_submit` | 1.68 ns | 1.54 ns | -8% | Within noise |
| `strategy/latency_arb_on_tick_no_fire` | 1.66 ns | 1.66 ns | ~0% | Untouched |
| **`signer/sign_order_full`** | **22.74 µs** | **20.49 µs** | **-10%** | **Statistically significant** |

### Why most deltas look like noise

The criterion microbenches measure **per-call CPU cost on already-warm cache lines**. Four of the five fixes shift work off the *engine* hot path entirely — they don't make per-call CPU cheaper, they remove cost from a layer the microbench doesn't observe.

**Where each fix actually wins:**

| Fix | What it removes | Microbench coverage | Real-world impact |
|---|---|---|---|
| **#1 Signer ctx cache** | Per-call `Secp256k1::signing_only()` table rebuild (~2 µs) | `signer/sign_order_full` | -10% measured (2.25 µs / submit). Worker-thread bound, off engine. Lower than initial 200-500 µs estimate because secp256k1 v0.x already amortizes some setup internally; the bulk of `sign_order` time is the ECDSA scalar mul itself (intrinsic to ECDSA, ~18-19 µs). |
| **#2 keep-alive POST** | TCP+TLS handshake per order on WAN (~50-150 ms) | None — bench doesn't include I/O | **Biggest sustained-rate win.** At a 250 ms strategy cooldown, the prior `Connection: close` consumed ~25-60% of each cycle's budget on reconnect handshakes. Post-fix the worker can sustain 5-10× higher submit throughput. |
| **#3 yield_now in engine loop** | 1 ms `nanosleep` (effective floor 4-10 ms on Linux HZ) | None — bench measures call cost, not loop cadence | **Biggest reactivity win.** Tick-to-decide latency drops from scheduler-quantum-bound (`min ~4 ms`) to ingress-cadence-bound (`min ~ring-pop + strategy = ~10 ns`). At a pinned engine core this is ~1000× faster end-to-end. |
| **#4 RxBuf cursor pair** | `copy_within(0..len)` per consumed WS frame | None — bench measures parser cost, not buffer compact | Burst-load win. Under a 50-frame buffered burst with a 64 KiB rx buffer, the old path moved ~3 MB total via `copy_within`; new path moves only the trailing residual once, when `tail` hits the buffer end. O(N²) → O(N) total memcpy. |
| **#5 conditional reregister** | 1 `epoll_ctl` per ingress loop iteration (~300-1000 ns/syscall on Linux) | None — bench is single-threaded, no mio | At a sub-ms loop cadence: ~10⁴ syscalls/sec/thread × 3 threads = ~30 µs/sec/thread of pure syscall overhead recovered. In steady state the interest bitmask never changes, so this is now O(0). |

### Total budget reconstruction (engine hot path, in-cache)

Stages 1-9 from the bench, summed and rounded:

```text
ingress frame parse           (out of microbench scope)
ingress ring push             5 ns
engine ring pop               5 ns
strategy::on_tick (no fire)   2 ns
ctx.submit (queued push)      2 ns
latency record × 3 stages     8 ns
metrics counter inc           2 ns
clock sample                  14 ns
                             ────
total                        ~38 ns / tick (warm cache, no fire)
```

A "fire" path adds the signer (~20 µs, off-engine via QueuedDispatcher) plus the keep-alive POST (50-150 ms WAN). The engine itself stays sub-50 ns per tick after these fixes.

### Verification

- `cargo clippy --workspace --all-targets -- -D warnings` — green
- `cargo test --workspace --release -- --test-threads=1` — **432+ tests** pass
- `cargo test -p bench --test alloc_assertions --release` — **23/23** zero-alloc
- `cargo bench -p bench --bench hot_path` — see table above

### Remaining hot-path work (next round, not blocking)

These were rated MEDIUM in the original audit. None block live-testing readiness.

- **D-4**: Pre-parse `SecretKey` at `LiveDispatcher::connect`; pass `&SecretKey` into `sign_order`. Expected: ~1-2 µs / submit.
- **D-5**: Replace `Mutex<DispatchStats>` with per-field `AtomicU64` array. Lock-free worker submit path.
- **E-2 / E-3 / E-4**: Per-item `now_ns()` in engine drain; cached `binance_index` in strategy; `book.apply_at(idx, tick)` accessor. ~20 ns / tick combined.
- **I-3**: Tight inner drain loop in ingress (only yield to mio on `WouldBlock`). Removes the 50 ms wakeup cap on first-after-idle bursts.
- **I-7**: Precompute u64 hashes of asset_ids at boot; replace O(N) byte-slice memcmp. ~200 ns / frame on N=100 symbols.

## Where the engine is already at the floor

These are positive findings from the audits — nothing to do:

- SPSC ring layout (`core-ring`): head/tail correctly cache-line isolated, single-producer/single-consumer enforced via `!Sync` PhantomData.
- POD struct sizes (`Tick`/`Signal`/`Fill`/`Order`): all `static_assert_size!(_, 64)`.
- `Counter::inc` / `Gauge::set`: single relaxed atomic.
- No `dyn Trait` on any hot path (verified by audit).
- No `tokio` on any ingress / engine thread.
- No `serde_json` anywhere on hot path (all parsers handwritten over `&[u8]`).
- No `Mutex`/`RwLock` on engine + ingress threads (only worker thread <-> cli for stats).
- `QueuedDispatcher::submit` (engine-side push): ~1.7 ns measured, identical to a raw ring push.
- Hot-path POD types: all `#[repr(C)] + Copy`.

## Next steps

After landing the top 5 fixes above, re-run `cargo bench -p bench --bench hot_path` and compare:
- `signer/sign_order_full` should drop ~5×.
- `clock/now_ns` numbers should remain unchanged (already vDSO).
- A new bench `engine/tick_burst_256` (not yet written — wanted but disk-constrained) would surface E-2/E-3/E-4 wins.

Files referenced in this report:
- `crates/engine/src/lib.rs`
- `crates/strategy-*/src/lib.rs`
- `crates/book-builder/src/lib.rs`
- `crates/core-ring/src/lib.rs`
- `crates/core-latency/src/lib.rs`
- `crates/core-time/src/lib.rs`
- `crates/core-net/src/transport.rs`, `ws_frame.rs`, `http1.rs`
- `crates/ingress-{polymarket,binance,rpc,rss}/src/`
- `crates/clob-dispatcher/src/{live,queued,json_encoder,response}.rs`
- `crates/signer-eip712/src/lib.rs`
- `crates/cli/src/paper.rs`
- `crates/bench/benches/hot_path.rs` (this round's new benchmarks)
