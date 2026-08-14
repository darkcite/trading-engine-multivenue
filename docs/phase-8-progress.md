# Phase 8 — Progress log

Working notes per the Stage-1 operating agreement: each entry records
what is done, what is next, and open issues at a clean boundary.

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
