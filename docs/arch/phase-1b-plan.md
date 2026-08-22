# Phase 1b — Polymarket live ingress (mio + rustls + handshake + run-loop)

Status: **complete** (finished 2026-04-19, same-day).

Scope of this plan: take the Phase 1a codecs (ws_frame, parse_book_update)
and drive them against a live TLS socket. Strictly Polymarket this
session; Binance, RSS, and RPC loops are Phase 1c.

Integration target this session: **library + in-process TLS loopback
integration test**. The `cli` crate keeps its paper-mode stub. Wiring
cli to actually open a real wss:// socket is a Phase 1d task — it adds
network dependencies that we do not want guarding the alloc-assert and
proptest CI signal.

## 1. Non-negotiables (all still apply)

* **Zero alloc in steady state.** The allocation guard for the ingress
  run-loop asserts 0 B/op per frame iteration *excluding* the one-shot
  TLS handshake and the connect. Reconnects do re-run the handshake,
  so they're allowed to allocate — but not the steady stream of ticks
  between reconnects.
* **No tokio, no async-std, no reqwest, no tungstenite, no native-tls.**
  rustls 0.23 + mio 0.8 + our own codec. webpki-roots for trust anchors.
* **No `dyn Trait` in hot paths.** `run<T: Transport>` is monomorphised
  per transport — `TlsTransport` in production, `TestTransport` in
  integration tests. Both implement the same crate-internal trait; the
  compiler inlines through it.
* **Single writer.** The ingress thread is the sole producer for the
  tick ring. The consumer is the engine thread (Phase 1c+).
* **No panics in release.** Every error path is `debug_assert!` + silent
  drop or reconnect. `panic = "abort"` is already set for release builds.
* **Every new public fn has rustdoc.** `#[deny(missing_docs)]` at crate
  level is unchanged.
* **Every parser has proptest + fuzz.** `ws_handshake` server-response
  parser inherits the same acceptance gate as Phase 1a parsers.

## 2. Deliverables

### 2.1 `core-net::transport` — non-blocking TLS transport

A thin `Transport` trait plus a `TlsTransport` implementation over
`mio::net::TcpStream` + `rustls::ClientConnection`.

```rust
pub trait Transport {
    /// Readiness probe driven by mio. Called from the run-loop's
    /// `Poll::poll` hot path.
    fn interest(&self) -> mio::Interest;

    /// Drive the underlying I/O forward: pump rustls records,
    /// service TCP sockets. Returns `Status::Handshaking`,
    /// `Status::Ready`, `Status::Closed`, or `Status::Error`.
    fn pump(&mut self, ev: &mio::event::Event) -> Status;

    /// Appends decoded plaintext bytes from the TLS stream into
    /// `dst`. Returns bytes written. Never allocates.
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize>;

    /// Enqueue plaintext bytes for encryption + send. Zero-copy
    /// into rustls' preallocated buffer.
    fn write(&mut self, src: &[u8]) -> io::Result<usize>;

    /// Register/reregister with the mio Poll.
    fn register(&mut self, registry: &mio::Registry, token: mio::Token) -> io::Result<()>;
}
```

rustls 0.23 exposes exactly the non-blocking API we need:
`ClientConnection::read_tls` / `write_tls` push bytes to/from mio's
TcpStream, and `process_new_packets` advances the state machine
without blocking. We pre-size the rustls fragment buffer at
construction; no reallocations in steady state.

Trust anchors via `webpki-roots` (compiled-in) to keep the sandbox
tests deterministic. No filesystem scan of the system trust store in
Phase 1b — that lands when we enable production deploy.

### 2.2 `core-net::ws_handshake` — handwritten RFC 6455 handshake

No tungstenite, no httparse. Zero-alloc byte writer + byte scanner.

```rust
/// Write `GET / HTTP/1.1` + headers into `dst`. Returns bytes written.
pub fn write_client_handshake(
    dst: &mut [u8],
    host: &[u8],
    path: &[u8],
    sec_key: &[u8; 24],           // base64(16 random bytes)
) -> Result<usize, HandshakeErr>;

/// Generate a Sec-WebSocket-Key from an entropy seed.
/// `seed` is typically `core-time::now_ns() ^ pid`.
pub fn sec_websocket_key_from_seed(seed: u64) -> [u8; 24];

/// Expected `Sec-WebSocket-Accept` value for a given `sec_key`.
/// Equivalent to `base64(sha1(sec_key ++ GUID))`, stack-only.
pub fn expected_accept(sec_key: &[u8; 24]) -> [u8; 28];

/// Parse server `101 Switching Protocols` response. Returns
/// `Incomplete`, `Upgraded { header_end }`, or `Malformed`.
pub fn read_server_handshake(buf: &[u8]) -> HandshakeResult;
```

SHA-1 is small and specified — we inline a 300-line implementation
rather than pull in `sha1` (which is fine but means one more
workspace dep). Base64 we also handwrite: 32 lines, stack-only.

The Sec-WebSocket-Key scheme has an attack surface (a rogue server
could claim `Accept: x`, fail verification, we'd reject): as long as
verification is constant-time we don't leak key material, and the
key is per-connection ephemeral so a leak would be moot anyway. Use
a constant-time compare utility for the accept check.

### 2.3 `ingress-polymarket::run_loop` — event-driven run-loop

Signature:

```rust
pub fn run<T: Transport>(
    transport: T,
    tick_ring: &mut RingProducer<Tick, TICK_RING_SIZE>,
    symbol_map: &SymbolMap,    // maps asset_id → SymbolId
    stop: &AtomicBool,
) -> RunResult;
```

State machine:

1. **Connecting** — transport.pump() returns Handshaking until TLS ready.
2. **WS handshake** — `write_client_handshake` into transport tx buffer
   once; keep reading until `read_server_handshake` returns `Upgraded`.
3. **Steady state** — loop:
   * `transport.read(rx_buf.free_mut())` → advance cursor.
   * Call `ws_read_frame` on `rx_buf.filled()` until `Incomplete`.
   * On a `Frame`:
     * If masked (shouldn't be for server→client), unmask in place.
     * Dispatch by `WsOpcode`: Text → `parse_book_update` → `tick_ring.push`.
       Ping → reply Pong. Close → initiate reconnect.
     * `rx_buf.consume(header_len + payload_len)`.
4. **Error / disconnect** — exponential-backoff reconnect (100 ms →
   100 s cap, ±25% jitter). Backoff state is an in-stack `u64`.

Everything after step 2 is zero-alloc. The tick-ring producer has
already been preallocated by the caller; we never allocate from the
run-loop.

### 2.4 `core-io::pmlr` — binary replay log writer

Extends the existing `PreallocatedWriter` with a typed PMLR layer:

```rust
pub struct PmlrWriter {
    inner: PreallocatedWriter,
    header_written: bool,
}

impl PmlrWriter {
    pub fn open<P: AsRef<Path>>(
        path: P,
        slot_kind: SlotKind,
        epoch_ns: NsTs,
    ) -> io::Result<Self>;

    /// Append a 64 B record. Zero-alloc. Auto-flushes when staging
    /// hits the page boundary.
    pub fn append<R: AsBytes>(&mut self, record: &R) -> io::Result<()>;

    /// Drain staging to disk. Caller must fsync separately if durability matters.
    pub fn flush(&mut self) -> io::Result<()>;
}
```

Header format is the one already documented in `docs/wire-format.md`:
`b"PMLR"` magic, version=1, slot_kind, epoch_ns, 48 B reserved.

`AsBytes` is unsafe-marker-only (`unsafe trait AsBytes: Copy`). We
implement it for `Tick`, `Signal`, `Fill`, `Order` — the crates that
own those types. No generic `zerocopy` dep; the trait is one line.

### 2.5 Test surface

1. **`tests/ws_handshake_roundtrip.rs`** — client writes a handshake
   request, a model server verifies the Sec-WebSocket-Key, computes the
   expected Accept, writes a response; client parses it with
   `read_server_handshake` and `constant_time_eq`s the accept value.
2. **`tests/polymarket_tls_loopback.rs`** (in `ingress-polymarket`)
   — boots an in-process rustls *server* on `127.0.0.1:0` with a
   self-signed cert (one-time boot cost), runs `run_loop` against it,
   scripts one CLOB book frame onto the wire, asserts one `Tick`
   popped from the ring with the expected prices.
3. **`crates/bench/tests/alloc_assertions.rs`** — extend with
   `polymarket_run_loop_steady_state_is_zero_alloc`. Construct a
   `TestTransport` backed by two `VecDeque<u8>` (boot cost) and iterate
   `run_loop::drive_one(...)` 1_000 times over a preloaded frame;
   assert 0 B/op.
4. **`tests/pmlr_roundtrip.rs`** (in `core-io`) — write one of each
   slot kind, reopen, mmap, assert byte-for-byte equality against a
   manual slice.
5. **`fuzz/fuzz_targets/ws_handshake.rs`** — arbitrary bytes →
   `read_server_handshake`. Must never panic.

## 3. Sequencing

1. Add `webpki-roots` workspace dep; plumb rustls into core-net deps.
2. `core-net::ws_handshake` (pure codec — test-friendly, fast feedback).
3. `core-net::transport::Transport` + `TlsTransport` + `TestTransport`.
4. `ingress-polymarket::run_loop` + state machine.
5. `core-io::pmlr` writer + `AsBytes` marker.
6. Tests + alloc assertion + fuzz target.
7. `cargo check/test/clippy` + alloc harness.

## 4. Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` green
      (188 tests across 24 binaries; alloc assertions run separately)
- [x] `cargo test -p bench --test alloc_assertions --release -- --test-threads=1`
      shows **9 assertions pass, 0 B/op** (8 from Phase 1a + 1 from 1b)
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green (8 fuzz targets:
      polymarket_clob_frame, binance_agg_trade, binance_book_ticker,
      ws_frame, ws_handshake, rpc_response, rss_item, core_parse_price)
- [x] `docs/phase-1-plan.md` updated with Phase 1b completion line

### Two flakes fixed during the sweep (unrelated to 1b code):

* `core-alloc` doctest had a stale `(u64, u64)` destructure after
  `AllocGuard::delta()` grew a third `deallocs` return — updated to
  `(allocs, bytes, _deallocs)`.
* `core-time::now_ns_advances_over_a_busy_loop` used a fixed 1024-iter
  spin that races the clock on ARM cores with coarser backing counters
  (observed on aarch64 CI). Now spins until the clock advances, bounded
  at 1M iterations to guarantee termination.

## 5. Out of scope (explicitly deferred to Phase 1c)

* Binance @bookTicker / @trade WS run-loop.
* RSS HTTPS poller (it's a separate I/O shape — request/response, not
  long-lived WS).
* Polygon JSON-RPC subscribe/response reader.
* cli wiring of the real Polymarket endpoint.
* Production trust store (system certs). webpki-roots is fine for v1.
* mio 1.x upgrade. mio 0.8 is what the rest of the workspace already
  pins; upgrading is orthogonal to shipping Polymarket ingress.
