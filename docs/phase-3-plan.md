# Phase 3 — Signer, CLOB dispatcher, `--live` gating

Status: **complete** (2026-05-19). All §2 deliverables landed.
Final tally: **309 tests across 29 binaries**, 15 zero-alloc
assertions + 1 budgeted per-sign assertion, 10 fuzz targets,
clippy clean. CLI `--live` flag wired with Secrets gating.

This is the plan for the live-trading path. The strategy already
emits `Order` values through `ctx.submit` and the engine forwards
them to a `PaperDispatcher` that just counts. Phase 3 replaces the
paper dispatcher with a real `LiveDispatcher` that EIP-712 signs
the order and POSTs it to the Polymarket CLOB over HTTP/2.

**Safety first.** Live trading is OFF by default. A `--live` flag
is required AND the binary must load `Secrets` from the `.env`
file. Either step missing → fail-fast with a clear error.

## Scope

* `crates/signer-eip712` — promote from "keccak + sign_digest" to:
  * Polymarket CTF Exchange domain separator (chainId 137,
    verifyingContract baked in as a constant).
  * Order typehash + struct-hash builder.
  * `sign_order(&Order, &key) -> [u8; 65]` end-to-end.
  * Known-answer tests on the typehash + struct hash + domain
    separator. The signature itself is non-deterministic (RFC 6979
    nonce derivation has known answers, but secp256k1's
    `sign_ecdsa_recoverable` adds a randomization step in some
    paths — we assert structural properties only).
* `crates/clob-dispatcher` — promote `LiveDispatcher` stub to a
  real hyper+rustls HTTP/2 client:
  * Single in-flight POST at a time (Phase 3 deliberately serial).
  * Preallocated JSON request buffer (zero-alloc encode of
    `Order` into the buffer; one `String::from_utf8_lossy` only at
    boot).
  * Preallocated response buffer + a handwritten JSON scanner
    (`order_id` / `error`).
  * `DispatchError` extended with `Http(u16)` and
    `JsonMalformed`.
* `crates/cli` — add `--live`, gate behind Secrets load:
  * `--paper` (default true) → unchanged: `PaperDispatcher`.
  * `--live` → load `Secrets`, build `LiveDispatcher`, refuse to
    run if `cfg.paper_mode == true` from env.
  * Mode is logged at boot with `tracing::info`.

Explicitly out of scope:
* Real-time fill confirmation feed. The dispatcher returns the
  CLOB order id; we mark `accepted += 1`. Live `Fill` ingestion
  arrives in Phase 4 alongside Polymarket WS `order` channel.
* Order cancellation. Phase 3 only POSTs new orders. Cancels are
  Phase 4.
* Order book state reconciliation. Phase 4.
* HdrHistogram latency tracking. Phase 4 alongside TUI.

## Non-negotiables (all carry over)

* **Zero alloc in steady state.** `LiveDispatcher::submit` must be
  zero-alloc once the connection is established. The hyper client
  pool may reuse internal buffers; we verify with the alloc
  harness.
* **No `serde_json` on the request side.** Hand-roll the JSON
  encoder using preallocated buffers + `itoa` for integers.
* **No `serde_json` on the response side.** `core_parse` scanners
  pluck `order_id` and `error` strings; copy into a fixed `[u8;
  64]` field on the response struct.
* **Fail-fast on signer or dispatcher errors at boot.** Wrong
  domain, malformed key, unreachable host → abort.
* **`mlock`'d signing key in `core-config::Secrets`** — already in
  place; verify on Phase 3 boot that the key is non-zero.

## Deliverables

### 3.1 `signer-eip712` expansion

Constants baked in (Polymarket Polygon CTF Exchange, Phase 3 v1):

```rust
/// EIP-712 domain name.
pub const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
/// EIP-712 domain version.
pub const DOMAIN_VERSION: &str = "1";
/// Polygon mainnet chain id.
pub const CHAIN_ID: u64 = 137;
/// CTF Exchange contract on Polygon.
pub const VERIFYING_CONTRACT: [u8; 20] = hex!("4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
```

Order typehash (canonical EIP-712 encoded type string keccak'd at
build time):

```rust
const ORDER_TYPE: &str = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

/// Precomputed `keccak256(ORDER_TYPE)`.
pub const ORDER_TYPEHASH: [u8; 32] = /* lazy_static OR compile-time const_keccak */;
```

(We'll compute the typehash at runtime in a `static OnceLock` since
no const-keccak crate matches our doctrine. Single boot-time
allocation.)

New surface:

```rust
pub struct OrderToSign<'a> {
    pub salt: u64,
    pub maker: [u8; 20],
    pub signer: [u8; 20],
    pub taker: [u8; 20],
    pub token_id: [u8; 32],  // bigint as raw bytes
    pub maker_amount: u128,
    pub taker_amount: u128,
    pub expiration: u64,
    pub nonce: u64,
    pub fee_rate_bps: u16,
    pub side: u8,            // 0=Buy, 1=Sell (Polymarket convention)
    pub signature_type: u8,  // 0=EOA, 1=POLY_PROXY, 2=POLY_GNOSIS_SAFE
}

pub fn domain_separator() -> [u8; 32];
pub fn order_struct_hash(o: &OrderToSign<'_>) -> [u8; 32];
pub fn order_eip712_hash(o: &OrderToSign<'_>) -> [u8; 32];
pub fn sign_order(o: &OrderToSign<'_>, key: &[u8; 32]) -> Result<[u8; 65], SignError>;
```

### 3.2 `clob-dispatcher::LiveDispatcher`

```rust
pub struct LiveDispatcher {
    client: hyper::client::conn::http2::SendRequest<...>,  // single conn
    host: String,                                          // boot-only alloc
    path: String,                                          // boot-only alloc
    req_buf: Box<[u8]>,                                    // 8 KiB preallocated
    resp_buf: Box<[u8]>,                                   // 8 KiB preallocated
    stats: DispatchStats,
    signer_key: [u8; 32],                                  // copied from Secrets
    next_salt: u64,                                        // local-rng seeded
}

impl LiveDispatcher {
    pub fn connect(
        host: &str,
        path: &str,
        signer_key: [u8; 32],
    ) -> io::Result<Self>;
}

impl OrderDispatch for LiveDispatcher {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        // 1. Build OrderToSign from Order + maker addr from key
        // 2. Sign → 65-byte signature
        // 3. Encode JSON into req_buf (zero-alloc; itoa for ints,
        //    hex-encode addresses + 32-byte ids)
        // 4. Send POST /order with body req_buf[..n]
        // 5. Read response; parse {order_id, error}
        // 6. accepted += 1 OR rejected += 1
    }
    // ...
}
```

JSON shape (Polymarket CLOB REST API):

```json
{
  "order": {
    "salt": "...",
    "maker": "0x...",
    ...
  },
  "owner": "<api-key>",
  "orderType": "GTC"
}
```

The exact shape is from Polymarket's docs; the codec is a thin
serializer, not a parser, so we hand-roll it.

### 3.3 `cli` — `--live` gate

```text
$ polymarket-engine run --paper      # default — PaperDispatcher
$ polymarket-engine run --live       # requires .env Secrets present
$ POLYMARKET_MODE=paper polymarket-engine run --live   # ERROR — env conflict
```

* `--live` mutually exclusive with `--paper`.
* `--live` triggers `Secrets::load` at boot; failure aborts.
* The engine boot logs `mode = live | paper` and the verifying
  contract address.

### 3.4 Test surface

* `signer-eip712`:
  - Known-answer test for `domain_separator()` (computed against
    the spec inline, asserts byte-identity).
  - Known-answer test for `ORDER_TYPEHASH`.
  - Known-answer test for `order_struct_hash` on a canned
    `OrderToSign`.
  - Round-trip test: sign → recover signer pubkey, assert it
    matches the input key's pubkey.
* `clob-dispatcher`:
  - JSON encoder unit tests (canonical Order → expected byte
    output).
  - JSON response scanner tests (success + error envelopes).
  - Loopback test: hyper+rustls server on 127.0.0.1:0 returns a
    canned response; assert `LiveDispatcher::submit` accepts +
    increments stats. (rcgen already a test dep.)
* `bench/tests/alloc_assertions.rs` — 2 new assertions:
  - `signer_sign_order_is_zero_alloc` (with caveat: secp256k1 may
    allocate; if so, we widen the budget but document it).
  - `live_dispatcher_encode_is_zero_alloc` (just the JSON
    serialization; not the network call).

## Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` — **309 tests across 29 binaries** ✔
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` — **15 zero-alloc + 1 budgeted = 16 / 16**.
      `signer_sign_order_per_call_budget_holds` documents
      libsecp256k1's per-sign ~208 B context allocation.
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
- [x] `cargo run --release -p cli -- run --live` requires
      `POLYMARKET_EIP712_KEY` via `Secrets::load` — fails cleanly
      with `EngineLoopResult::Failed` when absent.
- [x] `docs/phase-3-plan.md` flipped to **complete**
- [x] Memory file refreshed

## Risks / open questions

* **Polymarket EIP-712 spec compliance.** We hard-code the
  typehash + domain. Wrong constants → 100% of orders rejected
  with a signature error. Mitigation: cross-check against the
  upstream Polymarket Python SDK's known-answer hashes.
* **secp256k1 allocation.** The `secp256k1` crate's `signing_only`
  context allocates once at boot. `sign_ecdsa_recoverable` may
  allocate per-call on some platforms; if our alloc assertion
  shows a non-zero count, we document the per-sign budget rather
  than chase it.
* **hyper HTTP/2 buffer reuse.** hyper internally manages its own
  buffer pool; we expect ~0 allocs after warm-up but cannot
  guarantee it without an upstream patch. The alloc assertion is
  on the JSON-encode side only; the send side runs through a
  separate "allowed-allocs" budget in soak tests.
* **CTF Exchange contract address.** The verifying contract has
  been stable since 2021 but is technically upgradable via the
  proxy admin. Phase 3 bakes the current address; Phase 4 may add
  a config-driven override if Polymarket upgrades.

## Sequencing

1. **signer-eip712 expansion** + KAT tests.
2. **clob-dispatcher JSON encoder** + unit tests (no network yet).
3. **LiveDispatcher** with hyper+rustls + loopback test.
4. **CLI `--live` flag** + Secrets gate + boot-time validation.
5. **Alloc assertions** + final sweep + doc updates.
