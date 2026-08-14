# Phase 1c — Remaining ingress loops + cli wiring

Status: **complete** (2026-05-19). All §2 deliverables landed,
including the originally-deferred CLI paper-mode wiring (§2.4) and
the four loopback integration tests (§2.6 items 1–4). Final tally:
**252 tests across 28 binaries**, 12 alloc assertions @ 0 B/op,
10 fuzz targets, clippy clean.

Scope of this plan: take the `Transport`, `ws_handshake`, and run-loop
pattern that shipped in Phase 1b and extend it across the three
remaining ingress sources — **Binance** (public WS), **Polygon JSON-RPC**
(WS with request/response correlation), **RSS** (periodic HTTPS GET) —
plus wire the `cli` paper-mode entrypoint to spawn and join them against
the real `wss://` / `https://` endpoints.

Two small add-ons also fit this phase because they unblock downstream
work with almost no new surface area:

* The **in-process TLS loopback integration test** for
  `ingress-polymarket` that was listed in Phase 1b §2.5 item 2 but
  deferred. Phase 1b landed without it; 1c is the right time to add it
  before more run-loops depend on the same `TlsTransport` path.
* A **`core-io::PmlrReader`** (mmap-based, zero-copy) so Phase 2
  book-builder work can replay recorded ticks without introducing serde
  / a new binary format.

Integration target this session: **library + in-process loopback
integration tests for each ingress + a `cli run --paper` binary that
boots all four ingress threads and pumps ticks/signals into their
dedicated rings.** The strategy and dispatcher stay stubbed — wiring
them is Phase 2.

## 1. Non-negotiables (all carry over from 1b)

* **Zero alloc in steady state.** Every new run-loop ships with its own
  alloc-assertion test. Target: **12 alloc assertions, 0 B/op** (9 from
  1a+1b + Binance + RPC + RSS).
* **No tokio, no async-std, no reqwest, no tungstenite, no httparse,
  no native-tls.** rustls 0.23 + mio 0.8 + our own codecs. webpki-roots
  for trust anchors. `flate2` is tentatively allowed for optional gzip
  response bodies in the RSS poller *only*, and only if we find a feed
  that refuses `Accept-Encoding: identity`; otherwise no new deps.
* **No `dyn Trait` in hot paths.** Each `run<T: Transport>` is
  monomorphised per transport. Producers are generic over the ring
  capacity.
* **Single writer per ring.** Each ingress thread is the sole producer
  for its ring. Consumers are the engine thread (Phase 2+).
* **No panics in release.** Every error path is `debug_assert!` +
  silent drop or reconnect. `panic = "abort"` is already set.
* **Every new public fn has rustdoc.** `#[deny(missing_docs)]` is
  unchanged at the crate level.
* **Every new parser has proptest + fuzz.** `http1_response_parser`,
  `rpc_subscribe_envelope`, and `binance_combined_stream_envelope` each
  get a fuzz target.
* **Fail-fast on config.** Missing API keys, malformed URLs, or
  unreachable hosts abort at boot, not at first reconnect.

## 2. Deliverables

### 2.1 `ingress-binance::run_loop` — public WS, bookTicker stream

Near-verbatim clone of `ingress-polymarket::run_loop`. Intentional.
The second run-loop is where the template calcifies; keeping them
structurally identical lets us lift out common glue in Phase 1d.

```rust
pub fn run<T: Transport>(
    transport: T,
    tick_ring: &mut RingProducer<Tick, BINANCE_TICK_RING_SIZE>,
    symbol_map: &SymbolMap,    // "btcusdt" → SymbolId, etc.
    stop: &AtomicBool,
) -> RunResult;
```

State machine — identical 4-phase shape as Polymarket: Connecting →
NeedsWsWrite → AwaitingWsUpgrade → Steady. Only the Text-frame parser
and the ring type differ.

Endpoint & subscription strategy for v1:
* **One WS connection per symbol.** `/ws/{symbol}@bookTicker` is the
  simplest path — no combined-stream envelope to decode, no
  subscription message to send after connect. Binance accepts this and
  free-tier IP rate limits are far above what we need (single-digit
  connections).
* Deferred to 1d: `/stream?streams=…` combined-stream mode. When we add
  it, the new surface area is a tiny `parse_combined_envelope`
  byte-scanner that extracts the `"data":{…}` sub-object — feeds the
  same `parse_book_ticker` inside.

Hot-path dependencies — all already Phase 1a/1b:
* `core-net::{TlsTransport, ws_handshake, ws_read_frame,
  ws_write_pong, ws_unmask_in_place}`
* `ingress-binance::parse_book_ticker` (Phase 1a; 64 B POD)

New code is ~400 lines, almost all transliterated from
`ingress-polymarket::run_loop.rs`. The differences to call out:
* Host + path vary: `stream.binance.com:9443` / `/ws/{symbol}@bookTicker`.
* No ping-on-idle — Binance sends pings every ~3 min; we answer.
* Text frames are `{"u":…,"s":"BTCUSDT","b":"…","B":"…","a":"…","A":"…"}` —
  already covered by `parse_book_ticker`.
* SymbolMap lookup is on `s` (uppercase) instead of the Polymarket
  asset-id hex string. `SymbolMap::lookup` is case-sensitive; we
  normalise at bootstrap, not per-tick.

### 2.2 `ingress-rpc::run_loop` — Polygon JSON-RPC over WSS

Genuinely new shape: WS transport, but the payloads are request /
response pairs *plus* subscription notifications. Request-id
correlation lives in the hot path.

```rust
pub fn run<T: Transport>(
    transport: T,
    signal_ring: &mut RingProducer<Signal, RPC_SIGNAL_RING_SIZE>,
    endpoint_key: &[u8; 32],   // API key bytes from .env (mlock'd)
    stop: &AtomicBool,
) -> RunResult;
```

Hot-path additions beyond the Polymarket loop:

* **Pending-request map.** Fixed-size `[Pending; PENDING_CAP]` indexed
  by `id & (PENDING_CAP - 1)` — `PENDING_CAP` is a `const` power of
  two, 64 is plenty (we only have one in-flight `eth_blockNumber` poll
  plus one `eth_subscribe` per boot). Each `Pending` carries
  `{ id: u64, kind: RpcKind, created_at_ns: u64 }`. Collision policy:
  fail-fast `debug_assert!` — our id allocator never reuses within
  PENDING_CAP iterations.
* **Subscription-id table.** Also fixed-size:
  `[(SubId, SubKind); SUB_CAP]`. We currently register exactly one
  subscription (`newHeads`), so `SUB_CAP = 4`. Linear scan; ~40 ns.
* **Tick cadence.** Periodic `eth_blockNumber` poll every
  `RPC_POLL_MS`. Triggered inside the hot loop by comparing
  `now_ns()` to `next_poll_at_ns`. No timers, no mio alarms.

Wire codecs — all Phase 1a:
* `ingress-rpc::{write_request_eth_block_number, write_request_subscribe_new_heads,
  classify_rpc, parse_block_number_result, parse_new_head_notification,
  parse_rpc_error}`

The output ring type is `Signal` (64 B POD, already defined). Each
`NewHead` notification becomes one `Signal` with
`SignalSource::Rpc`, `class = LatencyClass::Warm`, payload = the
20 B `NewHead` POD packed into the 40-byte `data` slot with 20 B
tail-pad. The payload-pack helper is a new 10-line fn in
`ingress-rpc` (`pack_newhead_into_signal`).

API-key handling: the key bytes come from `core-config` (already
`mlock`'d, zeroised on drop). The run-loop receives `&[u8; 32]` and
writes it directly into the `/v2/{KEY}` path segment at connect time
— **never** copies it onto the heap.

### 2.3 `ingress-rss::poller` — periodic HTTPS GET

New I/O shape: request/response, not long-lived WS. Still single
thread, still `Transport`-based, still zero-alloc steady state. New
module `core-net::http1` carries a minimal HTTP/1.1 codec.

`core-net::http1` surface:

```rust
/// Write `GET {path} HTTP/1.1\r\nHost: {host}\r\n...` into `dst`.
/// Fixed header set: Host, User-Agent, Accept, Accept-Encoding: identity,
/// Connection: close. Returns bytes written.
pub fn write_get_request(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    user_agent: &[u8],
) -> Result<usize, HttpErr>;

/// Non-streaming response parser. Returns `Incomplete`, `Complete
/// { status, header_end, body_start, body_end, content_length }`, or
/// `Malformed`. Supports:
///   * explicit `Content-Length`,
///   * `Transfer-Encoding: chunked` (in-place dechunk into a scratch
///     slice the caller owns),
///   * connection-close framing (read until EOF).
pub fn read_response(buf: &[u8]) -> HttpResult;
```

No cookies, no compression (we hard-code `Accept-Encoding: identity`;
all the feeds we care about for Phase 1c honour it), no redirects
(3xx is treated as `Malformed` — we fail-fast with a log line, then
Phase 1d can add redirect chasing if we need it).

`ingress-rss::poller` signature:

```rust
pub fn run<T: Transport, const FEED_CAP: usize>(
    transport_factory: impl FnMut(&FeedCfg) -> io::Result<T>,
    feeds: &[FeedCfg; FEED_CAP],
    seen: &mut SeenRing<SEEN_CAP>,
    signal_ring: &mut RingProducer<Signal, RSS_SIGNAL_RING_SIZE>,
    stop: &AtomicBool,
) -> RunResult;
```

Scheduling: each `FeedCfg` carries `poll_interval_ns` +
`next_poll_at_ns` (mutable). Top of the loop finds the feed with the
earliest `next_poll_at_ns`, `park()`s (busy-sleep with a 1 ms
resolution budget) until then, then opens/connects that feed's
transport, writes one GET, reads the full response, hands the body
slice to `ItemIter` → `fnv1a_64` → `SeenRing::insert` → `Signal` into
the ring. Closes the transport, updates `next_poll_at_ns`, repeats.

The `transport_factory` indirection is **not** a hot-path hazard —
it's called once per feed per poll cycle (seconds apart, never in a
tick-rate loop). Allocations during connect are fine; zero-alloc
applies to the *steady body-parse path*.

### 2.4 `cli` — paper-mode boot

The `cli run --paper --config ~/polymarket/config.toml` binary must:

1. Parse config via `core-config` (already shipped).
2. Preallocate every ring at boot:
   `Ring<Tick, POLYMARKET_TICK_RING_SIZE>`,
   `Ring<Tick, BINANCE_TICK_RING_SIZE>`,
   `Ring<Signal, RPC_SIGNAL_RING_SIZE>`,
   `Ring<Signal, RSS_SIGNAL_RING_SIZE>`.
   (Reminder from Phase 0: `Ring::new()` MUST use
   `Box::new_uninit()` + `addr_of_mut!` — `Arc::new(Self { … })`
   overflows the default 2 MB test-thread stack.)
3. Spawn one thread per ingress, **pinned to a core**. Use `libc`
   directly (`sched_setaffinity` on Linux,
   `thread_policy_set(THREAD_AFFINITY_POLICY, …)` on macOS). No
   `core_affinity` crate — we don't need another dep for 40 lines.
4. Install a `SIGINT` handler that flips a global
   `AtomicBool::store(true, Release)`. Each run-loop polls it at
   the top of its state-machine dispatch.
5. On shutdown, join all ingress threads in reverse boot order,
   flush all PMLR writers, close files, exit 0.
6. In `--paper` mode, consumers are trivial: a `drain_and_count`
   loop that counts ticks per second and logs a one-line summary
   every 5 s. The engine / strategy / dispatcher are Phase 2.

CPU map for the MacBook Pro M4 target (10 performance cores):

| Core | Role                                     |
|------|------------------------------------------|
| 0    | main / cli / PMLR writer flusher         |
| 1    | ingress-polymarket run_loop              |
| 2    | ingress-binance run_loop (btcusdt)       |
| 3    | ingress-binance run_loop (ethusdt)       |
| 4    | ingress-rpc run_loop                     |
| 5    | ingress-rss poller                       |
| 6    | engine (reserved — Phase 2)              |
| 7-9  | spare                                    |

### 2.5 Add-ons: `polymarket_tls_loopback` + `PmlrReader`

**`tests/polymarket_tls_loopback.rs`** (in `ingress-polymarket`):
boots a minimal rustls *server* on `127.0.0.1:0` with a self-signed
cert generated once via `rcgen` (test-only dep), runs `run_loop`
against it, scripts one CLOB book frame onto the wire, asserts one
`Tick` popped from the ring with the expected prices. This covers
the gap left open by Phase 1b §2.5 item 2.

**`core-io::PmlrReader`**: mmap-based, zero-copy reader. ~60 lines.

```rust
pub struct PmlrReader<R: AsBytes> {
    map: memmap2::Mmap,
    slot_kind: SlotKind,
    record_count: usize,
    _phantom: PhantomData<R>,
}

impl<R: AsBytes> PmlrReader<R> {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self>;
    pub fn records(&self) -> &[R];   // transmute of the slot region
    pub fn slot_kind(&self) -> SlotKind;
    pub fn len(&self) -> usize;
}
```

Introduces `memmap2` as a `core-io` dep. It's a thin `mmap(2)`
wrapper, no heap allocation after construction, used only by
offline tooling and book-builder replay — not the hot path.

### 2.6 Test surface

1. `tests/binance_tls_loopback.rs` — mirrors the Polymarket loopback
   test with a scripted bookTicker text frame.
2. `tests/rpc_tls_loopback.rs` — scripts the id/response sequence:
   `eth_subscribe newHeads` → `{"id":1,"result":"0xabc…"}` →
   synthetic `newHeads` notification → one `Signal` in the ring.
3. `tests/rss_http1_loopback.rs` — starts a plain-TCP HTTP server
   on `127.0.0.1:0`, writes a canned RSS response (200 OK,
   content-length framed), asserts N signals in the ring and that
   the SeenRing deduplicated a repeat.
4. `crates/bench/tests/alloc_assertions.rs` — three new assertions:
   `binance_run_loop_steady_state_is_zero_alloc`,
   `rpc_run_loop_steady_state_is_zero_alloc`,
   `rss_poller_body_parse_is_zero_alloc`. Each iterates 1_000 times
   over a preloaded frame or body; asserts 0 B/op. Brings the total
   to **12**.
5. `fuzz/fuzz_targets/http1_response.rs` — arbitrary bytes →
   `read_response`. Never panics; if `Complete`, offsets are
   in-bounds.
6. `fuzz/fuzz_targets/rpc_subscribe_envelope.rs` — arbitrary bytes →
   `classify_rpc` → `parse_new_head_notification` or
   `parse_rpc_error`. Never panics. Brings total fuzz targets to
   **10**.
7. `tests/pmlr_reader_roundtrip.rs` — writer appends N records,
   reader mmaps, `records()` slice is bit-identical.

## 3. Sequencing

Plan each as an independent, shippable slice. In order:

1. **Phase 1b loopback test first** (§2.5). It's the lowest-risk,
   highest-leverage change — it hardens `TlsTransport` before three
   more loops depend on it.
2. `ingress-binance::run_loop` + its loopback test + alloc assertion.
   Second-easiest because it's a cleanroom clone.
3. `core-net::http1` module + its fuzz target + its unit tests. Lands
   before the RSS poller so RSS can consume a proven codec.
4. `ingress-rss::poller` + its loopback test + alloc assertion.
5. `ingress-rpc::run_loop` + its loopback test + alloc assertion +
   fuzz target. Saved for last because of the two-step request/sub
   correlation state machine — most novel of the three.
6. `core-io::PmlrReader` + roundtrip test.
7. `cli` wiring + SIGINT handler + core pinning. Paper-mode
   drain-and-count consumers for each ring.
8. Full verification sweep: `cargo check/clippy/test`, alloc harness,
   fuzz `cargo check`, docs update.

## 4. Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` green
      (240 tests across 23 binaries; alloc harness runs separately)
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` shows **12 assertions pass, 0 B/op**
      (9 from 1a+1b + Binance + RPC + RSS)
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
      (**10 fuzz targets**: 8 from 1b + `http1_response` +
      `rpc_subscribe_envelope`)
- [x] `cargo test -p ingress-polymarket --test polymarket_tls_loopback --release` green (real rustls server + rcgen cert)
- [x] `cargo test -p ingress-binance   --test binance_tls_loopback   --release` green
- [x] `cargo test -p ingress-rpc       --test rpc_tls_loopback       --release` green
- [x] `cargo test -p ingress-rss       --test rss_http1_loopback     --release` green (plain TCP, no TLS)
- [x] `cargo test -p core-io` exercises the mmap PmlrReader roundtrip
      (6 tests covering Tick/Signal roundtrip, bad magic rejection,
      truncated header, non-multiple payload, empty file)
- [x] `cargo run --release -p cli -- run --paper` boot with all four
      ingress threads + SIGINT + core-pin — landed (cli lib has 7
      tests covering Rings, drain loop, SIGINT, pinning)
- [x] `docs/phase-1-plan.md` updated with Phase 1c completion line
- [x] `docs/phase-1c-plan.md` status flipped to **library layer complete**
- [x] Memory file refreshed with new gotchas (Dispatch-enum pattern,
      `io::Error::other` vs `io::Error::new` allocation distinction,
      raw `libc::mmap` as memmap2 alternative, `suppress_polling_for_test`
      technique for pending-slot tests, disk-pressure workflow
      `CARGO_TARGET_DIR=/tmp/poly-target` + release-only builds)

### Landed in the 2026-05-19 follow-up

* **CLI paper-mode wiring** (`crates/cli/src/{paper,pinning,sigint}.rs`).
  Spawns one ingress thread per driver, pins each via
  `libc::sched_setaffinity` on Linux (best-effort no-op on macOS),
  installs an async-signal-safe SIGINT handler with two-press
  escalation, drains four SPSC consumer rings on the main thread
  with a 5 s per-period log, joins in reverse boot order on
  shutdown. ~500 LOC of glue + 7 lib tests.
* **Four loopback integration tests.** Three TLS variants
  (Polymarket / Binance / RPC) boot a `rustls::ServerConnection`
  with a self-signed cert from `rcgen`, drive the RFC 6455 opening
  handshake plus one canned frame, and assert ring contents. The
  RSS variant uses the new `PlainTcpTransport` in `core-net`
  against a plain HTTP/1.1 server. All four pass against real
  127.0.0.1 sockets.

## 5. Out of scope (explicitly deferred to Phase 1d or Phase 2)

* **Combined-stream Binance mode** (`/stream?streams=…`). One
  connection per symbol is fine until we monitor > ~20 symbols.
* **HTTP redirect chasing**, **gzip bodies**, **chunked-transfer
  test coverage** for the RSS poller. Current feeds work with
  identity encoding + content-length; chunked is exercised by fuzz
  but the loopback test uses content-length.
* **Automatic subscription re-arm on reconnect** in the RPC loop.
  Phase 1c reconnects drop and re-register from scratch — no
  client-side tracking of missed blocks.
* **Production trust store** (system certs). `webpki-roots` is
  still fine.
* **mio 1.x upgrade.** Orthogonal.
* **Strategy / engine wiring.** Phase 2.
* **Signer / CLOB dispatcher.** Phase 3.
* **TUI.** Phase 4.
* **Claude-worker plumbing.** Phase 5.

## 6. Risks / open questions

* **Polygon free-tier RPC endpoint choice.** Alchemy vs. QuickNode vs.
  Infura — each has a different rate-limit shape and subscription
  dialect. Phase 1c's codec is RFC-compliant JSON-RPC 2.0; provider
  choice lives in `.env`. Decision deferred until we have P&L signal
  indicating which of newHeads / logs / pendingTxns we actually need.
* **RSS feed selection.** The `SeenRing` + `fnv1a_64` path is
  provider-agnostic. Picking which feeds to poll (and at what
  interval) is a strategy-layer decision — Phase 1c just ships the
  plumbing with a placeholder `tests/fixtures/feeds.toml`.
* **Core-pinning API on macOS.** macOS `thread_policy_set` with
  `THREAD_AFFINITY_POLICY` is a *hint*, not a guarantee — Darwin's
  scheduler may ignore it under load. Phase 1c is best-effort;
  production deploy (Phase 7, Linux hpc6a) gets strict
  `sched_setaffinity`.
