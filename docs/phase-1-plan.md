# Phase 1 — Ingress layer (codec-first)

Status: **Phase 1 complete — 1a + 1b + 1c (2026-05-19).** CLI
paper-mode wiring and four loopback integration tests landed in the
2026-05-19 follow-up; see `docs/phase-1c-plan.md`. Phase 2 (engine
wire-up, book-builder) is next.

**Phase 1a acceptance — green**
- `cargo check --workspace --all-targets` ✔
- `cargo clippy --workspace --all-targets -- -D warnings` ✔
- `cargo test --workspace --lib --bins` — 143 tests ✔
- `cargo test -p bench --test alloc_assertions --release -- --test-threads=1` — 8 assertions @ 0 B/op ✔
- `cargo check --manifest-path fuzz/Cargo.toml` — 7 fuzz targets compile ✔

**Phase 1b acceptance — green (2026-04-19)**
- `cargo check --workspace --all-targets` ✔
- `cargo clippy --workspace --all-targets -- -D warnings` ✔
- `cargo test --workspace --release --exclude bench` — 188 tests ✔
- `cargo test -p bench --test alloc_assertions --release -- --test-threads=1` — **9 assertions @ 0 B/op** (+1 for `ingress-polymarket::run_loop` steady-state) ✔
- `cargo check --manifest-path fuzz/Cargo.toml` — **8 fuzz targets** compile (added `ws_handshake`) ✔
- Shipped: `core-net::Transport` (mio+rustls+webpki-roots) + `TestTransport`; `core-net::ws_handshake` (handwritten RFC 6455 opening handshake, SHA-1+base64 in-crate); `ingress-polymarket::run_loop` (event-driven state machine, zero-alloc steady state); `core-io::pmlr` (binary replay log writer with `AsBytes` marker); workspace-wide `AsBytes` POD marker trait.

**Phase 1c acceptance — green (2026-05-19)**
- `cargo check --workspace --all-targets` ✔
- `cargo clippy --workspace --all-targets -- -D warnings` ✔
- `cargo test --workspace --release --exclude bench` — **252 tests across 28 binaries** ✔
- `cargo test -p bench --test alloc_assertions --release -- --test-threads=1` — **12 assertions @ 0 B/op** (+3: Binance run-loop, RPC run-loop, RSS body-parse) ✔
- `cargo check --manifest-path fuzz/Cargo.toml` — **10 fuzz targets** compile (added `http1_response`, `rpc_subscribe_envelope`) ✔
- Shipped: `ingress-binance::run_loop`, `core-net::http1`, `ingress-rss::poller`, `ingress-rpc::run_loop`, `core-io::PmlrReader`.
- Follow-up landed 2026-05-19: `cli` library (`paper`, `pinning`, `sigint` modules + paper-mode binary) with libc-driven core pinning, async-signal-safe SIGINT handler, drain-and-count consumer; `core-net::PlainTcpTransport` for plain-HTTP loopback; four loopback integration tests against real 127.0.0.1 sockets (3 rustls-server-backed + 1 plain-TCP). New: 12 additional tests (7 cli + 4 loopback + 1 already-present alloc).

Scope of this plan: Phase 1a — pure byte-level codecs and parsers. Phase 1b
(mio + rustls event loop, `Transport` trait, live reconnect, run-loops)
is a separate plan and a separate PR.

Splitting this way keeps each surface independently testable and keeps
`alloc-assert` green without the noise of a real socket in the test
binary.

## 1. Non-negotiables (all still apply)

- Zero allocation in the hot path. `CountingAllocator` sees 0 B/op for
  every new parser. No `Vec::push`, no `String::from`, no `format!`,
  no `serde_json`, no `reqwest`, no `tokio`, no `dyn Trait`.
- `unsafe` only where it removes a bounds check that measurably helps
  a hot loop; every `unsafe` block carries a `// SAFETY:` comment.
- Every public parser has (a) at least one happy-path unit test,
  (b) at least one failure-mode unit test, (c) a `proptest` roundtrip,
  (d) a `cargo-fuzz` target. This is the acceptance gate.
- Every public function carries rustdoc — `#[deny(missing_docs)]` is
  on at crate level.

## 2. Deliverables for Phase 1a

### 2.1 `core-net` — WebSocket frame codec (no IO)

Pure `&[u8]` / `&mut [u8]` API. Fits into `FixedBuf` end-to-end; never
allocates.

| Export | Shape | Notes |
|---|---|---|
| `WsOpcode` | `#[repr(u8)]` enum | Continuation=0x0, Text=0x1, Binary=0x2, Close=0x8, Ping=0x9, Pong=0xA |
| `WsFrameHeader` | 14-byte POD struct | `fin: bool`, `opcode: WsOpcode`, `masked: bool`, `payload_len: u64`, `header_len: u8`, `mask: [u8; 4]` |
| `WsReadResult` | enum | `Incomplete`, `Frame { header, payload_range }`, `Malformed` |
| `ws_read_frame(buf: &[u8]) -> WsReadResult` | fn | Parses RFC 6455 frame header from `buf`; returns the `Range<usize>` into `buf` that spans the (still-masked) payload. |
| `ws_unmask_in_place(buf: &mut [u8], mask: [u8; 4])` | fn | XOR unmask in place. SIMD-friendly: 8-byte aligned stride with scalar fallback. |
| `ws_write_text_frame(dst: &mut [u8], payload: &[u8], mask: [u8; 4]) -> Result<usize, WsWriteErr>` | fn | Writes a masked client text frame; returns `bytes_written`. Fails if `dst` is too small. |
| `WsWriteErr::BufferTooSmall` | enum | Single variant — preallocated buffer exhausted. Non-allocating. |

**Key constraints / gotchas:**

- RFC 6455 allows a 7-bit, 16-bit, or 64-bit extended length field.
  All three paths exercised by proptest, each at the length boundary
  (125 / 126 / 65535 / 65536).
- Client frames are always masked; server frames never. We validate
  that on read for the server direction.
- `Incomplete` is not an error — the caller keeps reading from the
  TCP socket and calls back in.
- The unmask loop uses `u64` XOR chunks with a `[u8; 4]` mask expanded
  to `[u8; 8]` at frame-entry. Scalar fallback for the tail. This
  hot path is property-tested against a naive byte-by-byte unmask.

Fuzz target: `ws_frame.rs` — arbitrary bytes into `ws_read_frame`;
must never panic, must always leave the buffer well-defined.

Alloc assertion: 10 000 random-payload roundtrips through
`ws_write_text_frame` + `ws_read_frame` + `ws_unmask_in_place` allocate
0 bytes.

### 2.2 `ingress-binance` — `bookTicker` parser

Current state: only `aggTrade` parser exists. `bookTicker` is the
cheap top-of-book feed we actually want for latency-arb.

| Export | Shape |
|---|---|
| `BookTickerFrame` | `{ sym: SymbolId, bid_px_1e6: i64, bid_qty_1e6: i64, ask_px_1e6: i64, ask_qty_1e6: i64, update_id: u64 }` |
| `parse_book_ticker(buf: &[u8], sym: SymbolId) -> Option<BookTickerFrame>` | zero-alloc byte scanner |

Frame shape (stable Binance spec):

```json
{"u":12345,"s":"BTCUSDT","b":"65000.00","B":"1.2","a":"65001.00","A":"0.8"}
```

Fuzz target: `binance_book_ticker.rs`.
Proptest: symbol-agnostic random prices/qtys roundtrip through the
parser.
Alloc assertion: 10 000 parses, 0 B/op.

### 2.3 `ingress-rpc` — Alchemy/QuickNode response parsers

Phase 1a is **just parsers**. The transport (hyper+rustls for
`eth_blockNumber` polling, WS for `eth_subscribe`) lands in Phase 1b.

Frame shapes:

```json
// plain response
{"jsonrpc":"2.0","id":3,"result":"0x1a2b3c"}
// subscription notification
{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xcd...", "result":{"number":"0x1a","hash":"0xab...", ...}}}
// error
{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"..."}}
```

| Export | Shape |
|---|---|
| `RpcFrameKind` | enum `Response`, `Subscription`, `Error`, `Unknown` |
| `classify_rpc(buf: &[u8]) -> RpcFrameKind` | single-pass classifier using `find_field` |
| `parse_hex_u64(buf: &[u8], pos: Pos) -> Option<(u64, Pos)>` | `0x`-prefixed hex → u64 |
| `parse_block_number_result(buf: &[u8]) -> Option<(u64, u64)>` | returns `(id, block)` from a response frame |
| `parse_new_head_notification(buf: &[u8]) -> Option<NewHead>` | returns `{ block_number, block_hash: [u8; 32] }`, copied into a POD struct |
| `RpcError` | `{ code: i32, message_range: core::ops::Range<usize> }` — zero-copy error extraction |

Fuzz targets: `rpc_response.rs`, `rpc_subscription.rs`, `rpc_hex.rs`.
Alloc assertion: 0 B/op on every parser.

Also in scope: RPC **request** serializer. Preallocated stack buffer;
no allocation. `write_request_eth_block_number(dst: &mut [u8], id: u64) -> usize`
etc.

### 2.4 `ingress-rss` — item iterator + dedupe ring + FNV-1a hash

The Phase 0 `first_item` only returns the head item. We upgrade to:

| Export | Shape |
|---|---|
| `ItemIter<'a>` | `Iterator<Item = FeedItem<'a>>` — **not an allocating iterator**; holds `(&'a [u8], Pos)` only |
| `feed_items(buf: &'a [u8]) -> ItemIter<'a>` | constructor |
| `extract_cdata(inner: &[u8]) -> &[u8]` | strips `<![CDATA[ ... ]]>` wrappers (range-only, no copy) |
| `fnv1a_64(bytes: &[u8]) -> u64` | FNV-1a 64-bit, branchless inner loop |
| `SeenRing<const N: usize>` | preallocated cache-aligned ring of `u64` link hashes; `contains`, `insert` |

`SeenRing::contains` is O(N) worst case but N is a fixed 1024-ish; fits
in a single L1 cache line × 32 stride. Good enough for RSS cadence.

Fuzz targets: `rss_item.rs` — arbitrary XML-shaped bytes into
`feed_items` + `fnv1a_64`. Proptest: URL strings roundtrip through the
FNV hash with predictable collision rate.

### 2.5 Cross-cutting tests (under `crates/bench/tests/alloc_assertions.rs`)

Add four new tests, each running 10 000 iterations under the
`CountingAllocator` and asserting 0 B/op:

1. `ws_frame_roundtrip_is_zero_alloc`
2. `binance_book_ticker_is_zero_alloc`
3. `rpc_block_number_is_zero_alloc`
4. `rss_items_and_fnv_is_zero_alloc`

Still run with `--test-threads=1` (the process-global counter issue
noted in memory/project_polymarket_engine.md).

## 3. Fuzz corpus

Add seeds to `fuzz/corpus/` for each new target:

- `ws_frame/ping`, `ws_frame/masked_text_short`, `ws_frame/binary_65536`
- `binance_book_ticker/btcusdt`, `binance_book_ticker/tiny_price`
- `rpc_response/ok`, `rpc_response/error`
- `rpc_subscription/newhead`
- `rss_item/simple_cdata`, `rss_item/unicode`

## 4. Acceptance checklist (tick in commits / PR body)

- [ ] `cargo check --workspace --all-targets` — clean
- [ ] `cargo test --workspace --lib --bins` — all suites green
- [ ] `cargo test -p bench --test alloc_assertions --release -- --test-threads=1`
      — **8 total** assertions pass (4 existing + 4 new), 0 B/op
- [ ] `cargo fuzz run <target> -- -max_total_time=60` — no crash on any
      new target
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --all` — clean
- [ ] Every `pub fn` / `pub struct` carries rustdoc
- [ ] No new tokio / serde_json / reqwest dependencies
- [ ] Post-edit hook didn't flag anything

## 5. Out of scope for Phase 1a (explicitly deferred to Phase 1b)

- `mio` event loop and poll integration
- `rustls` TLS handshake + session lifecycle
- TCP reconnect with exponential backoff
- `ratatui` live dashboard
- `/metrics` Prometheus text-format endpoint
- HdrHistogram dumps to `~/polymarket/logs/latency/*.hgrm`
- `strategy-latency-arb` paper-mode writer
- `claude-worker` live API calls (Phase 0 scaffold remains)

Tracking these in PLAN.md §4 and §19 for the Phase 1b session.

## 6. Sequencing

1. `core-net::ws_frame` (foundation — everything else follows).
2. `ingress-binance::parse_book_ticker`.
3. `ingress-rpc` parsers + request writer.
4. `ingress-rss` iterator + dedupe + FNV.
5. New fuzz targets.
6. New alloc assertions.
7. `cargo check` / `test` / `clippy` / `alloc-assert` full sweep.
8. Update PLAN.md status line, save memory update.

Each step is independently verifiable — if we blow the session budget
we stop at the last green checkpoint, not mid-parser.
