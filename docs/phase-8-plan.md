# Phase 8 — Multivenue Expansion (OKX, Deribit, Hyperliquid/HIP-4) + AI-Ingress

Status: PROPOSED (2026-08-14). Companion to `PLAN.md`; explicitly supersedes the
`PLAN.md` §2.2 non-goal "Multi-venue execution". All venue facts below were verified
against official docs on 2026-08-14; source URLs inline.

Scope decisions recorded from the operator (Anton):

1. **Venue role: full execution everywhere.** OKX, Deribit and Hyperliquid are wired
   for market data *and* order routing. Hyperliquid includes HIP-4 outcome markets.
2. **AI authority:** the AI-Ingress can (a) enable/disable existing strategies at
   runtime, (b) drive a dedicated strategy that trades on AI commands, and (c) design
   new strategies and push them to the runtime (two-tier design, §8.6).
3. **Transport:** Unix domain socket at the process boundary + shared-memory SPSC
   ring inside the engine (§8.3).
4. **Autonomous research loop (2026-08-14 amendment):** claude-worker
   periodically fetches market data, analyzes it with Fable 5
   (`claude-fable-5`) for candidate strategies, backtests them against engine
   replay logs, and on a passing backtest pushes them to the runtime —
   Tier-1 rulesets autonomously, Tier-2 crates operator-gated (§8.7).

Staging (operator directive): **Stage 1 — prove the code successfully captures
ALL public market data from ALL venues** (8a–8e; gate G1 = §6.6 acceptance on
every venue). No Stage-2/3 work merges before G1 passes. Stage 2 — AI-Ingress +
research loop (8f–8h). Stage 3 — risk, execution, live (8i–8k). Details in §12.

---

## 1. Current state — what the audit found

The repo audit (2026-08-14) surfaced facts that shape this plan:

- **The ingress template is solid and cloneable.** All ingress crates share one shape:
  `mio` + `core_net::Transport` (TLS/Test), handwritten WS framing (`core-net`),
  byte-scanner parsers (`core-parse`), SPSC `core_ring::Ring<T, N>` publish,
  per-thread reconnect loop. `docs/architecture.md:17` already blesses "a new
  `ingress-<venue>` crate cloned from the `ingress-binance` shape".
- **The engine fan-in is the structural blocker.** `Engine<S, D>` hardwires exactly
  two tick consumers (PM, BN) + one signal consumer + one fill consumer, with a
  drain arm per field (`crates/engine/src/lib.rs`). Three more venues cannot be
  added by copy-paste without redesign (§3.3).
- **No venue identity exists.** No `VenueId` anywhere; `Tick` and `Order` carry no
  venue byte; `SymbolId` namespacing is manual CLI flags. (§3.1)
- **The RSS path is dead code in the shipped binary.** The engine's signal consumer
  is bound to the **RPC** ring; RSS signals are popped at cli level and discarded
  (`paper.rs:1111-1118`); the payload is a link *hash*, not text, so
  `strategy-rule-tree` keyword matching could never fire on it. Removing RSS costs
  zero trading behaviour. (§8.1)
- **claude-worker is far smaller than PLAN.md describes.** Two one-shot CLI
  commands (topic tagger on Haiku, rule parser on Sonnet), no daemon, no network
  input, no UDS bridge, no news labeler, no backtest reviewer. Artifacts are loaded
  at boot only. (§7)
- **risk-policy.md enforcement sites are fictional.** No `RiskGate`, no
  `on_new_order`, no `kill_if_exceeded`, no halt state machine exist. Full
  execution on three new venues makes building `crates/risk` a hard prerequisite
  for any live order (§10).

### 1.1 Known data-loss defects (fix in 8a — these answer "are we getting all data?" today: no)

| # | Defect | Site | Effect |
|---|--------|------|--------|
| D1 | PM symbol map built from `std::iter::empty()` | `multivenue-engine.rs:303` | Every Polymarket frame fails lookup → **zero PM ticks produced by the shipped binary** |
| D2 | Engine signal consumer bound to RPC ring only | `paper.rs:1082-1089` | RSS/news signals never reach any strategy |
| D3 | `try_next_fill` never called; fill ring producer dropped at boot | `paper.rs:1079-1080` | Fills are never consumed; `on_fill` is dead |
| D4 | `Producer::try_push` results discarded in every ingress | e.g. `ingress-binance/run_loop.rs:460` | Ring-full drops are invisible — no counter |
| D5 | `Driver.last_activity_ns` written, never read | all WS run loops | No idle detection; half-open TCP only caught on `Ok(0)` |
| D6 | No proactive ping on any feed | all WS run loops | Venue idle timeouts (OKX 30 s, HL 60 s) will drop us |
| D7 | Ingress state gauges fake (`up = iterations > 0` for all) | `paper.rs:1164-1174` | TUI/metrics health is fiction (`ingest_health = 0b1111`) |
| D8 | Reconnect backoff flat 500 ms (doc claims exponential) | `run_loop.rs` all ingresses | Hammering during venue outage |
| D9 | Implicit 8-byte tail padding on `Tick`/`Signal`/`Fill` vs `unsafe impl AsBytes` contract | `core-types` | Uninitialized bytes written to PMLR replay logs |
| D10 | `book_builder::TopOfBook::apply` drops `venue_seq <= last` silently | `book-builder/lib.rs:65-77` | Out-of-order/gap events uncounted, no resync trigger |

---

## 2. Target architecture (delta view)

Rendered diagram: `docs/phase-8-architecture.svg`.

```
              ┌───────────────┐  ┌───────────────┐  ┌───────────────┐
  existing →  │ingress-polymkt│  │ingress-binance│  │ ingress-rpc   │
              └──────┬────────┘  └──────┬────────┘  └──────┬────────┘
  new →       ┌──────┴──┐ ┌────────┐ ┌──┴──────────┐ ┌─────┴────────┐
              │ingress- │ │ingress-│ │ ingress-    │ │ ingress-ai   │◄─UDS── claude-worker
              │okx      │ │deribit │ │ hyperliquid │ │ (AiCmd ring) │        (daemon: RSS + research loop)
              └───┬─────┘ └───┬────┘ └──────┬──────┘ └─────┬────────┘
                  ▼           ▼             ▼              ▼
        tick lanes [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES]   ai_cmd ring
                  └───────────┴──────┬──────┴──────────────┘
                                     ▼
                     Engine<StrategySet, VenueRouter>   (single thread, core 0)
                                     │ Order{venue}
                  ┌──────────┬───────┴────────┬──────────────┐
                  ▼          ▼                ▼              ▼
            clob-dispatch  okx-dispatch  deribit-dispatch  hl-dispatch (HIP-4 incl.)
                  │          │                │              │
                  └──────────┴─── fill lanes ─┴──────────────┘ → Engine
```

RSS is deleted from the Rust side entirely; news intelligence moves into
claude-worker, which emits structured `AiCmd`s. Claude remains out of the hot path:
its commands arrive as just another slow feed (`docs/architecture.md:23` pattern,
`SignalSource::ClaudeWorker = 3` already reserved in `core-types`).

---

## 3. Naming, identity, wire-format changes (8a)

### 3.1 VenueId and SymbolId namespacing

```rust
// core-types
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum VenueId { Polymarket = 0, Binance = 1, Okx = 2, Deribit = 3, Hyperliquid = 4, Ai = 5 }
```

`SymbolId` stays `u32`: bits 31..24 = venue, bits 23..0 = per-venue ordinal
(16.7 M instruments per venue). `SYMBOL_ID_NONE = u32::MAX` remains reserved
(venue 255 unused). Staleness bucketing mixes the venue byte:
`bucket = (sym ^ (sym >> 24)) & 63` — replaces `sym & 63` so venues don't
collide on low bits. Per-venue symbol tables are built at boot from REST
discovery (§4 per venue) — this also fixes D1 (the PM map is populated from
Gamma/CLOB REST instead of `iter::empty()`).

### 3.2 Tick / Order gain a venue byte; padding made explicit

`Tick`: `_pad: [u8; 8]` → `venue: u8, _pad: [u8; 15]` (uses the implicit tail
padding; still 64 B). `Order`: `_pad1: [u8; 16]` → `venue: u8, _pad1: [u8; 15]`.
All padding becomes explicit and zeroed, resolving D9 (the `AsBytes` contract).
`docs/wire-format.md` is updated; PMLR `VERSION` bumps 1 → 2; a
`docs/migration.md` entry describes reading v1 logs (venue = Polymarket/Binance
inferred from slot kind + sym is not possible for v1 — v1 logs are declared
readable but venue-less). `static_assert_size!` unchanged (64 B).

### 3.3 Engine fan-in generalization

All venue tick rings standardize on one capacity, `TICK_RING_SIZE = 16_384`
(64 B × 16_384 = 1 MiB per ring; 5 rings = 5 MiB — irrelevant on 24 GB+):

```rust
// engine
pub const NUM_TICK_LANES: usize = 5;              // indexed by VenueId
pub const NUM_FILL_LANES: usize = 4;              // pm, okx, deribit, hl
tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
fill_lanes: [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
sig_cons:   Consumer<Signal, SIGNAL_RING_SIZE>,   // rpc (as today, D2 fixed: see §8)
ai_cons:    Consumer<AiCmd,  AI_RING_SIZE>,       // new, 1_024
```

`Engine::tick()` drains lanes in fixed index order with the same
`max_per_ring` budget; unspawned venues get a permanently-empty ring (a
`try_pop() → None` is two atomic loads — negligible). This replaces the
per-venue consumer-field-plus-drain-arm pattern and is the one-time cost that
makes venues 4..N mechanical. The `paper.rs:129` const-assert block collapses
to a single `TICK_RING_SIZE` equality.

### 3.4 core-net extensions

- `write_client_handshake_with_headers(dst, host, path, sec_key, extra: &[(&[u8], &[u8])])`
  — needed for any header-auth venue; existing function becomes a thin wrapper.
- Lift the subscribe/pending machinery out of `ingress-rpc` (`queue_ws_binary_frame`,
  fixed `Pending`/`subs` tables, resubscribe-on-`Steady` pattern) into
  `core-net::subs` so OKX/Deribit/HL clone it instead of re-implementing.
- Proactive ping scheduling: `KeepaliveCfg { interval_ns, kind }` driven off
  `last_activity_ns` (fixes D5/D6); venue-specific ping bytes provided by the
  ingress crate (OKX literal `ping` text frame, HL `{"method":"ping"}`, Deribit
  JSON-RPC `public/test` reply — §5).
- Reconnect backoff: flat 500 ms → capped exponential (500 ms → 8 s, jitter from
  `mask_counter`), fixing D8.

### 3.5 core-crypto (new small crate)

OKX login and the AI-Ingress HMAC need SHA-256/HMAC-SHA256; `ws_handshake.rs`
already inlines SHA-1 + base64. Extract into `core-crypto`: `sha256`,
`hmac_sha256`, `base64_encode` — handwritten, const-vector tested, zero-alloc,
no new dependencies. `signer-eip712`'s keccak/secp256k1 stay where they are.

### 3.6 Metrics headroom

`core_metrics::MetricsRegistry` grows `MAX_COUNTERS 64 → 256`,
`MAX_GAUGES 128 → 384` (still fixed arrays, still lock-free). Per-ingress drop
counters (D4) are wired: every `try_push` failure increments
`engine_ingress_<venue>_ring_drops_total`.

---

## 4. Venue market-data ingress (8b–8d)

Each venue is a new `ingress-<venue>` crate cloned from the `ingress-binance`
shape, with the venue-specific parts being the subscribe protocol, the parser,
and the integrity monitor. Mandatory per-crate deliverables are in §11. All
parsing is in-place over `&[u8]`; the single unavoidable copy per event is the
64-byte parsed slot into the ring (`try_push` moves the POD — ownership
transfer; same as every existing ingress). Documented here per the zero-copy
doctrine.

### 4.1 `ingress-okx` (8b — first, simplest, validates the 8a refactor)

Docs: `https://www.okx.com/docs-v5/en/`.

- **Endpoint:** `wss://ws.okx.com:8443/ws/v5/public` (demo: `wspap.okx.com`).
  Private (execution, §5): `.../ws/v5/private`.
- **Subscriptions** (one JSON `op:"subscribe"` frame, args batched):
  `bbo-tbt` (1 level, 10 ms, free — feeds `Tick`), `trades`, `mark-price`
  (200 ms), `funding-rate` (30–90 s push; interval varies 1h–8h — read
  `fundingTime`), optionally `books` (400 levels, 100 ms diffs, free) behind a
  `--okx-depth` flag for ladder work. The 10 ms L2 channels
  (`books50-l2-tbt`/`books-l2-tbt`) are VIP4+ — out of scope on free tier.
- **Integrity:** the book `checksum` field is **deprecated (always 0) — do not
  implement CRC32.** Continuity is `seqId`/`prevSeqId`: snapshot has
  `prevSeqId:-1`; each update's `prevSeqId` must equal the prior `seqId`;
  idle heartbeat updates have `prevSeqId == seqId` (~60 s); maintenance may
  legitimately *reset* (`seqId < prevSeqId`). Chain break ⇒ resubscribe (fresh
  snapshot) + `gaps_total` increment. `trades` also carries `seqId`.
- **Keepalive:** literal text frame `ping` → `pong` if nothing received for
  25 s (server cuts at 30 s idle).
- **Limits:** 3 connection attempts/s per IP; 480 sub/unsub ops per hour per
  connection (so: subscribe once, batched); REST `GET /api/v5/market/books`
  40 req/2 s (snapshot re-seed budget), `GET /api/v5/public/instruments`
  20 req/2 s.
- **Discovery:** `GET /api/v5/public/instruments?instType=SPOT|SWAP|FUTURES`
  at boot → symbol table (instId → SymbolId ordinal), `tickSz`/`lotSz`/`ctVal`
  captured into a per-venue `InstrumentTable` (fixed-cap, boot-only alloc).
  Note: OKX also lists `instType=EVENTS` — prediction-market event contracts
  with a `event-contract-markets` WS channel. Not wired in Phase 8; recorded as
  the natural third prediction venue for Phase 9 (§13).
  **Landed 2026-08-15 (8e), two live-wire corrections:** pre-listing
  rows (`state:"preopen"`) carry EMPTY `tickSz`/`lotSz` +
  `instIdCode:null` (accepted as non-live, excluded from the tradable
  universe); pre-market futures ids run to 27 bytes
  (`MOODENG-USD_UM_XPERP-310815`) — `OKX_INST_ID_MAX` is 32. The
  `instType`-driven channel gating replaced the `-SWAP` suffix hack as
  planned (mark: Swap|Futures; funding: Swap only).
- **Parser:** OKX pushes arrays of JSON strings (`["411.8","10","0","4"]`) —
  `core-parse::scan_price_1e6` handles them directly; key-matched field scan
  like `ingress-binance` so field reorder is tolerated.

### 4.2 `ingress-deribit` (8c)

Docs: `https://docs.deribit.com/`. JSON-RPC 2.0 over WS:
`wss://www.deribit.com/ws/api/v2` (test: `test.deribit.com`).

- **Subscriptions** (batched into few `public/subscribe` calls — subscribe
  costs 3000 credits from a 30 000 pool ⇒ ~3.3 calls/s; batch all channels per
  call): `quote.{instr}` (BBO → `Tick`), `ticker.{instr}.100ms` (mark, index,
  `current_funding`, price limits, OI), `trades.{instr}.100ms`, and with an
  authenticated connection `book.{instr}.raw` / `trades.{instr}.raw` (auth is
  free — no volume tier; since §5 authenticates anyway, raw cadence comes for
  free on the same socket).
- **Integrity:** book notifications chain `change_id` → `prev_change_id`;
  mismatch ⇒ official guidance is resubscribe. First notification is a full
  book (`type:"snapshot"`, unbounded depth — parse caps levels at
  `DEPTH_CAP = 64`, excess counted not stored). `trades` carry `trade_seq`
  (per-instrument monotonic — gap-checkable).
  **Corrected 2026-08-15 (8e raw-tap):** the live wire runs Deribit's
  *starbase* engine — trade rows are REORDERED (`timestamp`/`price`/
  `direction` precede `trade_seq`; `starbase_match_id`/
  `starbase_timestamp` follow) and round floats arrive in scientific
  notation (`"amount": 1.0e3`). This root-caused the 8d.1 sixth-entry
  "~1.3 % rejects + trade-seq gaps": marker-based row slicing + strict
  decimal scanning, both ours. Fixed with JSON-object-extent slicing +
  sci-capable scans; the strictly-sequential `trade_seq` policy is
  confirmed on the fixed parser (live run: 0 rejects, 0 holes).
- **Keepalive:** no WS ping; call `public/set_heartbeat {interval: 15}` (min
  10 s); server then emits `test_request` messages that **must** be answered
  with `public/test` or the connection is closed. This is a small JSON-RPC
  state machine in the run loop (reuse the `core-net::subs` pending table).
- **Limits:** 32 connections per IP; non-matching-engine ~20 req/s sustained;
  `public/get_instruments` 1 req/s (burst 50) — discovery paces itself.
- **Discovery:** `GET /api/v2/public/get_instruments?currency=BTC&kind=future`
  (+ ETH, USDC; options excluded v1 — chain size). Captures `tick_size` **and
  `tick_size_steps`** (price-dependent ticks — the fixed-point path must round
  to the step for the price band), `contract_size`, `min_trade_amount`.
  Amounts are USD for perps/futures — normalization to `Qty(1e6)` documented
  in the crate header.

### 4.3 `ingress-hyperliquid` (8d)

Docs: `https://hyperliquid.gitbook.io/hyperliquid-docs/`.
WS `wss://api.hyperliquid.xyz/ws`; REST `POST https://api.hyperliquid.xyz/info`.

- **Subscriptions** (`{"method":"subscribe","subscription":{...}}`):
  `bbo {coin}` (pushed only on BBO change → `Tick`), `l2Book {coin}`
  (**full snapshot every block, ≥ 0.5 s cadence, ≤ 20 levels/side — no diffs,
  no seq**), `trades {coin}`, `activeAssetCtx {coin}` (funding, oracle, mark,
  OI), `allMids` (cheap whole-venue mid sweep), and `outcomeMetaUpdates`
  (HIP-4 lifecycle: `outcomeCreated` / `outcomeSettled` / `questionUpdated` /
  `questionSettled`). `fastAssetCtxs` (DEFLATE-compressed) is skipped in v1 —
  decompression in the hot path is avoidable complexity.
- **Integrity:** stateless snapshots make gap detection moot — the monitor is
  pure staleness: the `time` field must advance within the configured budget
  per subscribed coin, else flag + reconnect. Missed data during reconnect is
  recovered by the next snapshot by construction.
  **Corrected 2026-08-14 (8d live test):** the docs' "snapshot every block,
  ≥ 0.5 s" does not describe the wire — a live probe (14-sub connection)
  showed `l2Book` pushes are timer-paced per subscription at ~1 push / 3.3 s
  per coin, uniform across coins regardless of book activity. Default budget
  is therefore **10 s** (≈ 3× observed period), not 2 s.
- **Keepalive:** `{"method":"ping"}` every 50 s (server cuts at 60 s idle).
- **Limits:** 10 WS connections/IP, 30 new/min, 1000 subscriptions, 2000
  client→server msgs/min; REST 1200 weight/min (`l2Book`/`meta` weight 2/20).
- **Discovery at boot** (`POST /info`):
  - `{"type":"meta"}` → native perps universe (asset = index in `universe`).
  - `{"type":"perpDexs"}` → HIP-3 builder dexs; per-dex `meta?dex=` →
    coin `"{dex}:{coin}"`, asset `100000 + dex_idx*10000 + idx`.
  - `{"type":"spotMeta"}` → spot pairs, asset `10000 + idx`, coin `"@{idx}"`.
  - **`{"type":"outcomeMeta"}` → HIP-4 outcome markets.**

### 4.4 HIP-4 specifics (the reason Hyperliquid is here)

HIP-4 "outcome markets" are Hyperliquid's on-chain prediction-market primitive:
fully collateralized fixed-range contracts, no leverage, no liquidations —
directly comparable to Polymarket binary markets, with **zero fees currently**
(no maker rebates on outcomes; builder codes only on sells).

- **Identity:** encoding `= 10*outcome + side` (side 0 = Yes, 1 = No); coin
  string `#<encoding>`, asset id `100_000_000 + encoding`. Ordinary
  `l2Book`/`bbo`/`trades` subscriptions work on coin `#<enc>` — no separate
  API surface for market data.
- **Merged books:** Yes and No share one book — buy Yes @ p ≡ sell No @ 1−p;
  matching is price-side-time priority. The book-builder treats the Yes side
  as canonical and derives No (`px_no = 1e6 − px_yes` in our fixed point).
- **Questions:** groups of outcomes where exactly one settles Yes, linked via
  `negate`/`merge`. Settlement: Yes → `settleFraction` quote tokens, No →
  `1 − settleFraction` (binary: 1/0; scalar outcomes may settle fractional).
- **What exists on mainnet now:** protocol-run **recurring daily BTC binary**
  settling 06:00 UTC to the HyperCore mark price (linear interpolation between
  the two mark updates straddling settlement). Permissionless deployment
  (templates, staking, deployer fee scales) is **testnet-only** at the moment —
  so v1 enumerates via `outcomeMeta` and tracks `outcomeMetaUpdates`, rather
  than assuming market breadth. The BTC daily is directly arbable against
  Polymarket BTC dailies — that is the first cross-venue prediction pair.
- **Inventory ops** (execution side, §5.3): `/exchange` `userOutcome` actions —
  `splitOutcome` (X quote → X Yes + X No), `mergeOutcome`, `mergeQuestion`,
  `negateOutcome`. `splitOutcome`/`mergeOutcome` are the flatten primitives:
  holding equal Yes+No is riskless collateral, which changes what "position"
  means for the risk book (§10).

### 4.5 Book-builder depth policy

v1 keeps `TopOfBook` (`Tick`) as the strategy-facing model on every venue:
OKX `bbo-tbt`, Deribit `quote`, HL `bbo` are all direct BBO feeds — no ladder
maintenance required for the trading path. Depth channels (OKX `books`,
Deribit `book.*`, HL `l2Book`) are consumed for **capture + integrity**
(sequence chains, §6) and optionally a `PriceLadder<32>` per configured symbol
(fixed array, `#[repr(align(64))]`) for EV sizing in 8g+. D10 fix: `TopOfBook::apply`
gets gap/out-of-order counters and a resync callback instead of silent drop.

---

## 5. Execution layer (8j) — full execution everywhere

`clob_dispatcher::OrderDispatch { submit, try_next_fill, stats }` is the seam;
it stays. Each venue gets its own dispatcher crate implementing it; the engine
routes by `Order.venue` through a monomorphized router — no `dyn`:

```rust
pub struct VenueRouter<P, O, D, H> { pm: P, okx: O, der: D, hl: H }
impl<P: OrderDispatch, O: OrderDispatch, D: OrderDispatch, H: OrderDispatch>
    OrderDispatch for VenueRouter<P, O, D, H> {
    #[inline(always)]
    fn submit(&mut self, o: &Order) -> Result<(), SubmitErr> {
        match o.venue { 0 => self.pm.submit(o), 2 => self.okx.submit(o),
                        3 => self.der.submit(o), 4 => self.hl.submit(o),
                        _ => Err(SubmitErr::BadVenue) }
    }
}
```

One predictable branch on a hot byte; each arm is a static call. Paper mode:
`VenueRouter<PaperDispatcher, PaperDispatcher, PaperDispatcher, PaperDispatcher>`.
Fill plumbing is fixed (D3): each live dispatcher thread owns a
`Producer<Fill, FILL_RING_SIZE>`; the engine drains all fill lanes (§3.3) and
`Strategy::on_fill` becomes real.

### 5.1 `okx-dispatcher`

Private WS (`/ws/v5/private`): login `op:"login"` with
`sign = Base64(HMAC-SHA256(ts + "GET" + "/users/self/verify", secret))`
(30 s expiry — `core-crypto`), then `op:"order"`/`op:"cancel-order"` over the
same socket; subscribe `orders` channel for acks/fills. Keys:
`OKX_API_KEY` / `OKX_API_SECRET` / `OKX_API_PASSPHRASE` in `.env`.

### 5.2 `deribit-dispatcher`

Same JSON-RPC socket as ingress or a second connection (32/IP budget is ample):
`public/auth` `grant_type=client_credentials` → `access_token`
(connection-scoped; re-auth on reconnect), then `private/buy` / `private/sell`
/ `private/cancel`, subscribe `user.orders.{instr}.raw` + `user.trades` for
fills. Keys: `DERIBIT_CLIENT_ID` / `DERIBIT_CLIENT_SECRET`.

### 5.3 `hl-dispatcher` (largest work item)

`POST /exchange` over the existing hyper/rustls H2 stack. Signing pipeline:
action → **handwritten msgpack encoder** (field order is normative!) → +nonce
(+vault, +expiresAfter) → keccak-256 → EIP-712 "phantom agent" signature —
reusing `signer-eip712`'s secp256k1 + tiny-keccak. New module
`signer-eip712::hl` (or sibling crate `signer-hl`): fixed-schema msgpack
encoding into a preallocated buffer (no general msgpack library), known
failure modes handled by construction: trim trailing zeros on price/size
strings, lowercase addresses before signing.

- **Agent wallet:** one API/agent wallet per process (`ApproveAgent` done once
  via a CLI helper), key in `.env` (`HYPERLIQUID_AGENT_KEY`), loaded into an
  `mlock`'d page and zeroized on drop — same treatment as
  `POLYMARKET_EIP712_KEY`.
- **Nonces:** atomic ms-timestamp counter (per-signer top-100 set rule; must
  be > min(set), unused, within (T−2 d, T+1 d)).
- **Batching:** orders/cancels coalesced ~100 ms per official latency
  guidance; ALO-only batches are validator-prioritized; cancels use the
  `fast` flag. Address-based action budget (1 req / 1 USDC traded, 10 k
  initial buffer) is tracked by a preallocated token bucket.
- **HIP-4:** standard `order` action on asset `100_000_000 + enc`;
  `userOutcome` split/merge actions exposed as `OrderKind::{Split, Merge}`
  variants so strategies can flatten Yes+No inventory atomically.

### 5.4 Rate-limit governors

Every dispatcher embeds a fixed-size token bucket (`#[repr(align(64))]`,
refill computed from `now_ns()` deltas — no syscalls beyond the existing clock
read). Order submission that would breach the venue budget fails fast with
`SubmitErr::Throttled` and a counter — never queues unboundedly.

---

## 6. Feed-completeness verification (8e) — "are we actually getting all the data?"

Layered program; every layer is preallocated counters + fixed tables, no
allocation after boot.

### 6.1 Boot-time subscription audit

At startup, each ingress enumerates the venue's instrument universe via REST
(§4 discovery calls, rate-limit paced) and diffs it against the configured
subscription set. Output: a coverage report log line per venue
(`configured=N subscribed=N universe=M`) and gauge
`engine_ingress_<venue>_coverage_configured`. `--live` refuses to start if a
configured symbol is absent from the venue universe (fail-fast doctrine).

### 6.2 Continuous integrity monitors (per channel, in the ingress thread)

| Venue | Mechanism | Detection | Action |
|---|---|---|---|
| OKX | `seqId`/`prevSeqId` chain (books, trades) | `prevSeqId != last_seqId`, honoring reset rule (`seqId < prevSeqId` = maintenance) and idle heartbeats (`prev == seq`) | resubscribe channel; `gaps_total++` |
| Deribit | `change_id`/`prev_change_id` chain; `trade_seq` monotonic | mismatch / seq gap | resubscribe (official guidance); `gaps_total++` |
| Hyperliquid | snapshot cadence | `time` not advancing within the staleness budget per coin (default 10 s; live-measured push period ~3.3 s/coin — corrected 2026-08-14) | reconnect; staleness counts into `gaps_total` |
| Binance | `u` (updateId) monotonic | regression | count only (gaps are legitimate for bookTicker) |
| Polymarket | `timestamp` monotonic + periodic CLOB REST book cross-check | regression / divergence beyond tolerance | resync from REST; counters |
| RPC | block number monotonic via existing 2 s poll | regression/stall | existing reconnect |

### 6.3 Liveness watchdogs

`last_activity_ns` finally gets a reader (D5): per-connection deadline drives
proactive pings per §4 keepalive specs (D6) and forces reconnect on miss.
Deribit's mandatory `test_request → public/test` reply lives here.

### 6.4 Loss accounting

Per-ingress counters, all in the metrics registry: `msgs_total`, `bytes_total`,
`parse_errors_total`, `gaps_total`, `resubscribes_total`, `reconnects_total`,
`ring_drops_total` (D4). The TUI ingress-health row switches from the fake
`0b1111` (D7) to real per-thread state published through a
`#[repr(align(64))]` per-ingress status slot (state enum + last_activity +
counters snapshot), single-writer per ingress thread.

### 6.5 Capture and offline audit

**Implemented 2026-08-15 (8e).** The original "every parsed slot already
flows to PMLR replay logs" claim was FALSE for the shipped binary (sixth
progress entry); 8e wired it for real: each ingress thread owns a
`core_io::PmlrCapture` (monomorphized `core_types::Capture` sink — the
run loops' `C: Capture` parameter) writing
`<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/<venue>-{ticks,events,signals}.pmlr`.
Non-tick channels are captured as the new 64-B `ChannelEvent` POD
(`SlotKind::Event = 5`; 4 stays reserved for `AiCmd`); BBO flows as
`Tick`. Ticks are captured *before* the ring push so ring-dropped ticks
remain auditable. Capture I/O errors sticky-disable capture (never the
session) and surface via `engine_ingress_<venue>_capture_io_errors`.
The **audit tool** shipped as `multivenue-engine audit-replay --dir`:
per-symbol message rates, inter-arrival histograms vs expected venue
cadence — corrected bands: 10 ms-floor OKX bbo-tbt, 100–200 ms OKX
books/mark + Deribit book, **~3.3 s/coin HL l2Book** (2–6 s band), the
rest event-driven — integrity re-derivations (book chains honoring
snapshot/heartbeat/reset; trade holes derived for Deribit only — OKX
trade seqIds share the book-wide sequence and legitimately jump), and a
venue × channel coverage matrix. `--raw-tap <venues>` captures raw WS
payloads (`<venue>-raw.tap`, `PMRT` v1; budget-bounded first-N — a
documented deviation from the "bounded ring" sketch; rejects-only by
default, off in prod) for parser-vs-wire differential audits — its
first use root-caused the Deribit rejects (§4.2 correction).

### 6.6 Acceptance

24 h paper soak with all venues: zero unexplained gaps (every `gaps_total`
increment paired with a logged venue event), `ring_drops_total == 0` at
default capacities, coverage report = 100% of configured symbols, and the
audit tool's observed rates within expected cadence bands.

---

## 7. How the current AI agents work (as-is reference)

Recorded so the redesign has a truthful baseline; PLAN.md §4/§10 describe a
system that is mostly unbuilt.

**Runtime agents (`claude-worker/`, Python 3.14, ~450 lines):** a single Typer
CLI (`claude-worker`) with two one-shot, synchronous subcommands. There is no
daemon, no scheduler, no network input — the operator runs it by hand.

1. `tag-topics -i items.ndjson -o tags.ndjson` — **topic tagger**, model
   `claude-haiku-4-5` (`MODEL_BULK`), temperature 0. Input: NDJSON lines
   `{id, text}`. One API call per item via the sole SDK touchpoint
   (`anthropic_client.complete()`). Output NDJSON `{id, family, impact,
   reason}` with `family ∈ {crypto, politics, sports, macro, other}`,
   `impact ∈ {low, med, high}`. Malformed model output degrades silently to
   `other/low`.
2. `parse-rules -i notes.txt -o rules.json` — **rule parser**, model
   `claude-sonnet-4-6` (`MODEL_REASONING`). Input: one research note per line.
   Output: JSON array `{name, family, trigger, edge_bps, horizon_ms,
   max_risk_usd}`; strict validation (`RuleParseError` on bad JSON/bounds).

`MODEL_HARD = claude-opus-4-6` is declared and used nowhere — the backtest
reviewer, news labeler, SQLite prompt cache, and `research_bridge` UDS from
PLAN §10 do not exist.

**Engine consumption — boot only, files only:** `research-artifacts` (hand-
rolled scanners, no serde) loads the files at startup: `--artifacts-path` →
`ArtifactTable<8>` → `strategy-ev` (keys must be the decimal SymbolId, not an
asset id; Python emits no probability, Rust derives `high→0.70`,
`med/low→0.50` — med and low are indistinguishable); `--rules-path` →
`RulesTable<8>` → `strategy-rule-tree` (the `trigger` field is parsed then
discarded; the rule→keyword mapping is a hardcoded `b"halving"` stub;
`max_risk_usd` is stored and read by nothing). No hot-reload, no file watch —
new intelligence requires an engine restart.

**Dev-time agents (`.claude/agents/`, not runtime):** `alloc-auditor` (Sonnet,
zero-alloc gate), `parser-property-tester` (Sonnet, writes proptests/fuzz
targets), `risk-reviewer` (Opus, read-only verdict `APPROVE|BLOCK|NEEDS-DOCS`
on risk-relevant diffs). These review code in this repo; they never touch the
running system.

---

## 8. AI-Ingress redesign (8f)

### 8.1 Remove `ingress-rss`

The RSS path is dead in the shipped binary (§1, D2): the engine's signal
consumer is bound to the RPC ring; RSS signals are drained at cli level and
discarded; the 40-byte payload is `fnv1a(link)` + length — no text, so
rule-tree keywords could never match. Unstructured text needs an LLM anyway;
that work belongs in claude-worker. Per the "never leave unused code" rule,
removal is total, in one commit, after 8f cutover:

- Delete `crates/ingress-rss/`, `fuzz/fuzz_targets/rss_item.rs` (+
  `fuzz/Cargo.toml` entry), the two bench alloc-assertions, the
  `crates/strategy-news/` corpse, and all `Rings.rss_signal` /
  `Consumers.rss_signal` / `spawn_rss` / `RssFeed` / `RSS_FEEDS` /
  `engine_rss_*` metric plumbing (76 references in `paper.rs` alone; full
  sweep list from the 2026-08-14 audit).
- `SignalSource::Rss = 1` stays reserved in the enum (wire-format stability;
  documented as retired).
- `core-net::http1` is **not** orphaned: it becomes the REST client for §4
  discovery and §6 snapshot cross-checks (gains `write_post_request` for the
  Hyperliquid `/info` POST). `.claude/agents/parser-property-tester.md` scope
  updated from `ingress-rss` to the new ingress crates.
- CPU core 4 (formerly RSS) goes to `ingress-ai`.
- RSS polling itself moves to claude-worker (§8.2) — feeds stay `.env`-listed,
  now read by Python.

### 8.2 claude-worker becomes a daemon

> **Operator directive 2026-08-15:** the existing `claude-worker/` (one-shot
> Typer CLI) does NOT fit this design and will NOT be incrementally migrated.
> Delete it completely and reimplement from scratch per §8.2–§8.3
> (Python 3.14, daemon-first). Old code is read-only reference (SDK usage,
> prompt/rule formats); no wholesale porting; tests written fresh against the
> new design. 8f exit criteria gain: old claude-worker fully deleted.

> **Operator directive 2026-08-15 (dual-mode amendment):** the worker must
> operate in either of two modes over ONE code path:
> **(a) full-auto** — `claude-worker serve`, the daemon loop below, LLM calls
> via the Anthropic SDK (`ANTHROPIC_API_KEY`);
> **(b) semi-manual** — no daemon: the operator opens a Claude session with
> the predefined prompt `docs/prompts/ai-session.md`; the session performs
> the reasoning (triage / labeling / strategist) itself and drives the same
> pipeline through the operator verbs (§8.2.1), pushing to ingress-ai over
> the same UDS. Mode is chosen by invocation — no mode flag, no divergent
> logic; the SDK client is constructed only by `serve`.

Entrypoint `claude-worker serve` (the rewritten worker's only **daemon**
mode; semi-manual operator verbs in §8.2.1):

- **news_watcher** — polls the RSS/Atom allowlist (httpx, 15–60 s cadence per
  PLAN §8.3), dedupes by `(feed, guid)` in SQLite, triages headlines with
  Haiku, escalates interesting ones to Sonnet for labeling
  (market-mapped, direction, confidence, half-life).
- **commander** — turns labeled events + operator policy into `AiCmd` frames
  (§8.4) and writes them to the UDS; emits `Heartbeat` every 5 s.
- **data_fetcher** — periodically assembles the research dataset: the
  engine's own PMLR replay logs (primary — free, engine-true, reads
  `CLAUDE_WORKER_REPLAY_DIR`, pointed at the engine's log dir) plus venue REST
  history (secondary, rate-budgeted: OKX candles, Deribit chart data, HL
  `candleSnapshot`), summarized into compact per-symbol feature files.
- **strategist** — `claude-fable-5` (`MODEL_STRATEGIST`, replaces the unused
  `MODEL_HARD`); batch cadence (default every 6 h). Reviews feature summaries,
  live strategy performance, and microstructure notes; proposes candidate
  Tier-1 rulesets with an explicit thesis and expected edge, plus Tier-2 crate
  drafts. Never connected to the socket directly — outputs go through the
  §8.7 backtest gates.
- **backtester** — drives the Rust `cli backtest` harness (§8.7) over the
  replay logs and enforces the promotion gates; only gate-passing rulesets
  reach the commander.
- SQLite prompt cache keyed `(model, prompt_version_hash, content_hash)`
  (PLAN §10.2) finally gets built — a polling daemon without it burns budget.
- House rules hold: full `import x` only, SDK mocked at the boundary in tests,
  `httpx` added to `pyproject.toml` (PLAN §17.3 already lists it).

### 8.2.1 Semi-manual mode — operator session drives the verbs

No background process. The CLI exposes the daemon's pipeline stages as
one-shot **operator verbs**, each a thin wrapper over the same functions the
`serve` loop calls (one code path, both modes; the daemon subsystems are the
library, `serve` and the verbs are two frontends):

- `claude-worker fetch` — data_fetcher one-shot: replay logs + rate-budgeted
  venue REST → per-symbol feature files; prints the paths for the session to
  read.
- `claude-worker backtest --ruleset r.json` — drives `cli backtest` (§8.7.3)
  over the replay logs and writes the machine-readable gate report.
- `claude-worker push --kind <AiCmdKind> [--sym S --px P --qty Q ...]` —
  frames a single `AiCmd` (toggles, SetFairValue, SetParam, OrderIntent,
  HaltRequest) with HMAC onto the UDS; a Heartbeat frame precedes every
  verb-initiated push.
- `claude-worker stage-ruleset r.json --report rep.json` /
  `claude-worker commit-ruleset --hash H` — the §8.6 double-buffer pair.
  **Gates bind in code in both modes:** `stage-ruleset` refuses any ruleset
  not hash-linked to a passing backtest report; there is deliberately no
  override flag (operator directive: same gates, both modes — retune
  thresholds in worker config instead).

The session's reasoning replaces the SDK: in semi-manual mode
`ANTHROPIC_API_KEY` is not required and never read — the verbs never
construct the SDK client. Heartbeats exist only around verb invocations; when
the session ends, ai-exec's staleness fail-safe (> 15 s, §8.5) plus per-command
`ttl_ns` expire AI-derived state. Silence fails safe in both modes, by design.

Deliverable with 8f: `docs/prompts/ai-session.md` — the predefined session
kickoff prompt (required reading list, verb cheatsheet, gate rules, AiCmd
field semantics, worked examples). It is part of the test surface: a scripted
"session" (shell driving the verbs against the fake UDS server from the test
suite) proves the semi-manual path end-to-end without any live model call.

### 8.3 Transport: UDS at the process boundary, SPSC ring inside

- `ingress-ai` (new crate) owns a `mio`-driven **Unix domain socket** listener
  at `~/multivenue/run/ai.sock` (0600, parent dir 0700), accepts a single
  client, verifies peer credentials (`LOCAL_PEERCRED` on macOS, `SO_PEERCRED`
  on Linux) and an HMAC per frame.
- Verified commands are pushed into a `core_ring::Ring<AiCmd, 1024>` — the
  **shared-memory ring** leg: cache-aligned, lock-free, single-writer, the
  same SPSC machinery as every other ingress. The engine drains it as a lane.
- Zero-copy note (doctrine): the frame is parsed in place from the rx buffer;
  the one unavoidable copy is the 64-byte slot into the ring (ownership
  transfer, identical to all ingresses). Documented here.
- A true cross-process mmap ring (Python writing slots directly) is
  **rejected for now**: CPython cannot express acquire/release atomics without
  a C extension, and the command rate (~1/s) is seven orders of magnitude
  below what the UDS hop handles. Revisit only if command rate ever matters.

### 8.4 `AiCmd` — new 64-byte POD (`core-types`)

```
offset  field        type  notes
0..8    ts_ns        u64   worker send time
8..12   seq          u32   strictly increasing per session; gap ⇒ counter (§6 applies to the AI feed too)
12..16  sym          u32   venue-namespaced SymbolId or SYMBOL_ID_NONE
16..24  px           i64   fixed-point 1e6 (fair value / intent price / param value)
24..32  qty          i64   fixed-point 1e6
32..40  ttl_ns       u64   expiry relative to ts_ns; expired-on-pop ⇒ dropped + counter
40      kind         u8    see below
41      venue        u8    VenueId
42      strategy_id  u8    StrategySet slot index
43      side         u8    Side or 0xFF
44..46  param_id     u16   for SetParam
46..48  flags        u16   bit0: expire_on_silence
48..64  _pad         [u8;16] explicit, zeroed
```

Kinds: `Heartbeat=0, EnableStrategy=1, DisableStrategy=2, SetFairValue=3,
SetBias=4, SetParam=5, OrderIntent=6, RulesetStage=7, RulesetCommit=8,
HaltRequest=9`. **There is deliberately no Resume** — risk-policy.md's sticky
halt requires manual restart, so the command cannot exist.

Wire frame: `[len u16][AiCmd 64 B][HMAC-SHA256 tag, 16 B]`, key
`AI_INGRESS_HMAC_KEY` from `.env` (`core-crypto`). Every accepted command is
PMLR-logged (`SlotKind::AiCmd = 4`) — the audit trail is replayable by
construction. `Strategy` gains a defaulted method (monomorphized, no `dyn`):

```rust
fn on_ai<C: Ctx>(&mut self, cmd: &AiCmd, ctx: &mut C) {}
```

### 8.5 StrategySet — runtime enable/disable + the AI-driven strategy

Today `Engine<S>` is monomorphized over ONE strategy chosen by `--strategy`.
Runtime on/off requires all strategies compiled in, statically composed:

```rust
pub struct StrategySet {
    latency_arb: LatencyArb<64>, ev: EvStrategy<8>, cross_arb: CrossArb,
    rule_tree: RuleTree<8>, ai_exec: AiExec<64>, vm: StrategyVm,   // §8.6
    enabled: u8,   // bitmask, one bit per member
}
impl Strategy for StrategySet {   // static fan-out, one predictable branch per member
    fn on_tick<C: Ctx>(&mut self, t: &Tick, ctx: &mut C) {
        if self.enabled & 1 != 0 { self.latency_arb.on_tick(t, ctx); }
        if self.enabled & 2 != 0 { self.ev.on_tick(t, ctx); }
        /* ... */
    }
}
```

`EnableStrategy`/`DisableStrategy` flip bits (Enable refused while halted;
Disable always honored). `--strategy` becomes the initial mask (single name =
back-compatible single bit). Memory note: each member keeps its own
`MultiBook` in v1 (N copies of top-of-book state — cheap); a shared read-only
book is a later refactor.

**`strategy-ai-exec`** (new): the strategy that trades on AI commands.
Maintains a per-symbol fair-value table (fixed array, TTL-expired) fed by
`SetFairValue`; quotes/takes when the venue book deviates from AI fair beyond
an edge parameter; honors `OrderIntent` directly (clamped by RiskGate, §10;
paper-only until 8i lands). If the AI heartbeat goes stale (> 15 s), all
AI-derived quotes are pulled and intents refused — silence fails safe.

### 8.6 "AI designs new strategies and pushes them to runtime" — two tiers

**Tier 1 — runtime, bounded: `strategy-vm`.** A table-driven strategy the AI
can reprogram live without codegen. Per configured symbol the engine computes
a fixed feature vector (mid, spread, top-level imbalance, cross-venue basis vs
venue *j*, book age, AI-fair delta); a ruleset is ≤ 256 rows of
`(feature_id, cmp, threshold_1e6)` predicates and `(side, px_offset, qty,
cooldown_ns, cap)` actions — all fixed-size arrays, evaluated branch-lean in
`on_tick`. Deployment is double-buffered: `RulesetStage{hash}` makes the
`ingress-ai` side thread (NOT the hot path) load the ruleset artifact file,
validate bounds (symbols exist, qty/caps within risk policy), and fill the
inactive table; `RulesetCommit{hash}` flips an atomic index at a tick
boundary. The AI can push a genuinely new trading behavior at runtime with
bounded semantics and a full audit trail.

**Tier 2 — offline, full power.** The strategist agent writes a real
`strategy-*` crate. Gates, in order: `cargo nextest` + new-code unit tests →
alloc assertions (0 B/op) → fuzz smoke → `risk-reviewer` subagent verdict →
**operator approval** → rebuild + supervised restart (positions re-synced from
venue REST at boot). "Push to runtime" here means a minutes-scale redeploy —
the price of unrestricted Rust.

**Rejected: `dlopen` hot-loading of AI-generated cdylibs.** It would put an
indirect call on every event (the `dyn` doctrine exists for a reason), load
unaudited AI-generated native code into the trading process, and make a panic
across the FFI boundary UB. Documented as a rejected alternative per house
style.

**Safety invariants (all enforced in code, not prose):** the engine never
blocks on AI; every command is HMAC'd, TTL'd, seq-checked, and PMLR-logged;
`DisableStrategy` is always honored; `EnableStrategy` is refused while halted;
no Resume exists; `SetParam` may only tighten caps (loosening requires restart
+ the risk-policy phased-loosening procedure); AI silence expires AI-derived
state rather than freezing it in.

### 8.7 Autonomous research loop (8h): fetch → analyze (Fable 5) → backtest → push

The full loop, run by `claude-worker serve` on a configurable cadence in
full-auto mode; in semi-manual mode the operator's Claude session executes
the same stages through the §8.2.1 verbs (reasoning in-session, gates
unchanged):

1. **Fetch** (data_fetcher, §8.2): PMLR replay logs are the primary dataset —
   they contain exactly what the engine saw, at zero API cost. Venue REST
   history fills gaps (warm-up windows, symbols not yet captured). This is why
   the G1 gate precedes this phase: garbage capture ⇒ garbage backtests ⇒
   confidently wrong strategies.
2. **Analyze** (strategist, `claude-fable-5`): proposes candidate Tier-1
   rulesets as JSON artifacts, each with a written thesis, target symbols,
   expected edge, and self-declared caps (≤ risk-policy bounds).
3. **Backtest — in Rust, not Python.** New subcommand
   `cli backtest --ruleset r.json --replay <dir> --split 70/30
   [--fees-model <venue-table>] [--latency-ns N]`. It replays PMLR logs
   through the **real `strategy-vm` evaluator** — the same code path that will
   run in production, so there is no Python reimplementation to diverge.
   Conservative fill model: cross-only fills at the touch, configurable
   latency penalty, per-venue fee table (HIP-4 currently 0, Polymarket per
   docs). Output: machine-readable report (trades, net P&L, max drawdown, hit
   rate, per-symbol breakdown, in-sample vs out-of-sample).
4. **Gates** (backtester; thresholds in worker config, not in prompts):
   out-of-sample net P&L > 0 after fees and latency penalty; ≥ 50 trades
   spanning ≥ 2 trading days; max drawdown ≤ configured cap; every ruleset
   bound within risk-policy limits. Fail any ⇒ archived with the report,
   never pushed.
5. **Push.** Passing **Tier-1 rulesets are pushed autonomously**:
   `RulesetStage{hash}` → side-thread validation → `RulesetCommit{hash}`
   (§8.6) — into paper mode first; live only under RiskGate caps (§10).
   **Tier-2 crates never auto-push**: the backtest report attaches to the
   draft and the operator approves — native code entering the trading process
   stays human-gated.
6. **Monitor + rollback.** The worker compares live (paper) performance of its
   own rulesets against backtest expectation; sustained underperformance
   (default: below the backtest's 10th-percentile P&L path for 24 h) triggers
   automatic revert to the prior ruleset or `DisableStrategy` on the VM slot.
   Every promotion and demotion travels the AiCmd trail, so the whole loop is
   PMLR-replayable after the fact.

Model routing after this change: Haiku 4.5 bulk tagging (unchanged),
Sonnet 4.6 news labeling (unchanged), **`claude-fable-5` for strategy
research and backtest review** — CLAUDE.md's "Preferred Claude models" table
updates accordingly.

---

## 9. Configuration and ops

New `.env` keys (all mirrored in `.env.example`; secrets `mlock`'d where
private-key material):

```
OKX_WS_PUBLIC_HOST / OKX_WS_PRIVATE_HOST / OKX_REST_HOST
OKX_API_KEY / OKX_API_SECRET / OKX_API_PASSPHRASE
DERIBIT_WS_HOST / DERIBIT_CLIENT_ID / DERIBIT_CLIENT_SECRET
HYPERLIQUID_WS_HOST / HYPERLIQUID_API_HOST / HYPERLIQUID_AGENT_KEY   (mlock'd)
AI_INGRESS_SOCK (default ~/multivenue/run/ai.sock) / AI_INGRESS_HMAC_KEY
ANTHROPIC_API_KEY   (full-auto mode only — read by `claude-worker serve`;
                     the §8.2.1 operator verbs never load the SDK)
```

`RSS_FEEDS` moves to claude-worker's environment. Per-venue symbol config
follows the existing flag pattern (`--okx-symbols BTC-USDT,ETH-USDT`, ids
allocated per §3.1 — the manual `--*-sym-id` flags retire).

Thread/core map (advisory on macOS — `pin_current_thread_to_core` is a warn
no-op there; numbers bind on Phase-7 Linux): engine 0, polymarket 1,
binance 2, rpc 3, **ai 4** (freed by RSS), okx 5, deribit 6, hyperliquid 7,
dispatchers 8+, metrics/TUI unpinned.

---

## 10. `crates/risk` — prerequisite for any live order (8i)

risk-policy.md names enforcement sites (`RiskGate`, `Engine::on_new_order`,
`kill_if_exceeded`) that do not exist. Full execution on three venues makes
this the gating work item; live trading on any new venue is blocked on it.

- `RiskGate` (preallocated, `#[repr(align(64))]`): per-symbol and total
  notional caps, per-symbol and total open-order caps, single-order notional
  cap — checked in `Ctx::submit` before dispatch, per venue and globally.
- Position book fed by the (now real, D3) fill lanes; **HIP-4 aware**: equal
  Yes+No holdings net to riskless collateral, so exposure is `|yes − no|`, and
  `Split`/`Merge` order kinds mutate inventory without market risk.
- Sticky halt state machine implementing the six risk-policy triggers;
  halt ⇒ cancel-all per venue, refuse submits, require manual restart.
- The `risk-reviewer` subagent gates the PR (its mandate names exactly this:
  "a new order-submission path must route through the same risk checks" —
  both the AI path and three new dispatchers qualify).

---

## 11. Testing (house standard, per CLAUDE.md and PLAN §21)

| Deliverable | Requirement |
|---|---|
| Every new parser (`okx`, `deribit`, `hl`, `ai` frame) | property test + fuzz target registered in `fuzz/Cargo.toml`: `okx_frame`, `okx_book_seq`, `deribit_jsonrpc_frame`, `deribit_book`, `hl_ws_frame`, `hl_l2book`, `ai_cmd_frame` |
| `hl` msgpack encoder + `core-crypto` | fuzz (`hl_msgpack_encode`) + known-answer vectors mirrored from the HL Python SDK; SHA-256/HMAC NIST vectors |
| Every ingress | `tests/<venue>_tls_loopback.rs` (rcgen self-signed, scripted server: subscribe ack, data, gap injection, idle-timeout, reconnect) |
| Alloc discipline | new entries in `crates/bench/tests/alloc_assertions.rs` (parsers, encoders, dispatcher steady-state, `StrategySet` fan-out, `strategy-vm` eval, `ingress-ai` frame path) — `--test-threads=1`, 0 B/op |
| Integrity monitors | unit tests per §6.2 row: gap, reset (OKX maintenance), idle-heartbeat, resubscribe storm cap |
| Execution | paper dispatchers against venue testnets (OKX demo `wspap`, `test.deribit.com`, HL testnet); signed-payload golden tests |
| `cli backtest` harness | golden replay fixture with known P&L; determinism test (same log ⇒ bit-identical report); fee/latency-model unit tests |
| Python | pytest with SDK mocked; `test_imports_are_full` stays green; daemon loop + backtest gates tested with a fake UDS server and canned `cli backtest` reports |
| System | 24 h all-venue paper soak meeting §6.6; `cli audit-replay` coverage matrix reviewed |

---

## 12. Phasing and acceptance

**Stage 1 — data completeness (the operator-mandated first gate).**

| Phase | Content | Exit criteria | Est. |
|---|---|---|---|
| 8a | Foundations: §3 refactors + defects D1–D10 | existing venues green on lane engine; alloc suite green; PMLR v2; PM ticks actually flowing (D1) | 4–6 d |
| 8b | OKX MD ingress | 24 h soak, seq-chain clean, drops = 0 | 2–3 d |
| 8c | Deribit MD ingress | same + heartbeat protocol proven | 2–3 d |
| 8d | Hyperliquid MD + HIP-4 discovery | BTC daily outcome streamed via `#<enc>`; `outcomeMetaUpdates` handled | 3–4 d |
| 8e | Feed-completeness harness + `audit-replay` | **DELIVERED 2026-08-15** — discovery + capture + audit live-verified; Deribit rejects root-caused/fixed (progress log, seventh entry) | 2–3 d (overlaps) |
| **G1** | **Gate: §6.6 acceptance on ALL five venues** — 24 h soak, zero unexplained gaps, `ring_drops_total == 0`, 100% configured coverage, cadences in-band | **no Stage-2/3 work merges before G1** (operator directive 2026-08-15: first soak shortened to 6 h) | — |

**Stage 2 — AI-Ingress and the research loop** (consumes Stage-1 capture).

| Phase | Content | Exit criteria | Est. |
|---|---|---|---|
| 8f | AI-Ingress + RSS removal + StrategySet + `strategy-ai-exec` + worker daemon, dual-mode (from-scratch rewrite — old claude-worker deleted, §8.2 directive; §8.2.1 verbs + `docs/prompts/ai-session.md`) | AI cmd → strategy toggle observed live; RSS fully deleted; old claude-worker fully deleted; heartbeat/staleness proven; both modes proven (auto loop with mocked SDK; scripted semi-manual verb session vs fake UDS) | 5–7 d |
| 8g | `strategy-vm` (Tier 1) | hand-authored ruleset staged, committed, trading in paper | 4–6 d |
| 8h | Research loop (§8.7): data_fetcher + strategist (Fable 5) + `cli backtest` + gates + rollback | Fable-5-authored ruleset auto-promoted after passing backtest, trading in paper; forced-underperformance rollback demonstrated | 5–7 d |

**Stage 3 — risk, execution, live.**

| Phase | Content | Exit criteria | Est. |
|---|---|---|---|
| 8i | `crates/risk` | risk-policy.md enforcement sites real; risk-reviewer APPROVE | 3–5 d |
| 8j | Execution dispatchers (paper→testnet): OKX, Deribit, HL(+HIP-4) | testnet fills consumed via fill lanes; golden signing tests | 8–12 d |
| 8k | Live ramp: HL HIP-4 first (zero fees, tiny caps), then OKX, then Deribit | per-venue P&L reports per risk-policy phased-loosening | ongoing |

Roughly 6–8 focused weeks end-to-end. 8b–8d are independent after 8a and can
interleave; 8e is built alongside them; nothing after G1 starts before G1
passes.

---

## 13. Risks and open questions

- **HIP-4 breadth:** mainnet currently has the protocol-run recurring BTC
  daily; permissionless deployment is testnet-only. The Polymarket↔HIP-4 arb
  starts with BTC dailies; breadth arrives with permissionless deploys —
  `outcomeMetaUpdates` means we pick up new markets automatically.
- **Hyperliquid latency floor:** WS pushes are per-block (~0.5 s+); sub-block
  latency requires running a node — explicitly out of Phase 8 scope.
- **OKX `EVENTS` instrument class** (CEX prediction markets, own WS channel)
  is the natural third prediction venue — Phase 9 candidate, not wired now.
- **HL signing fragility:** msgpack field order, trailing-zero trimming, and
  address casing are known failure modes; mitigated by SDK-mirrored golden
  vectors, but this stays the highest-defect-risk component (8j).
- **Anthropic budget:** a polling daemon without the prompt cache would burn
  spend; the cache (§8.2) ships with `serve`, not after it.
- **Binance futures stream** (PLAN §8.2, mark/liquidations) folds into the
  lane model later as a second Binance connection — not part of Phase 8.
- **Tier-2 restarts** lose in-memory state by design; position resync from
  venue REST at boot is part of 8j's dispatcher work.
- **Backtest overfitting:** Fable 5 proposing rulesets scored on the same
  data family it studied invites curve-fitting; mitigations are the fixed
  out-of-sample split, minimum-trade/day gates, conservative fill model, and
  live-vs-backtest monitoring with auto-rollback (§8.7.6). Gate thresholds
  live in config so tightening them needs no code change.
- **macOS:** no pinning, no `sched_setaffinity` — all core numbers are
  aspirational until Phase-7 Linux hardware.

## 14. Documents and files this plan touches

`PLAN.md` (§2.2 non-goal reversal, §4 diagram, §8.1 source matrix),
`CLAUDE.md` (directory guide, pitfalls, model routing — `claude-fable-5` for
strategy research), `docs/architecture.md`,
`docs/wire-format.md` (PMLR v2, `AiCmd`, venue bytes), `docs/migration.md`
(v1→v2 log note), `docs/risk-policy.md` (per-venue caps table),
`.env.example`, `config.example.toml` (reference blocks),
`.claude/agents/parser-property-tester.md` (scope), `README.md`.

---

*Venue facts verified 2026-08-14 against: OKX `https://www.okx.com/docs-v5/en/`
(order-book channel + sequence-id rules, rate limits, WS login), Deribit
`https://docs.deribit.com/` (book/quote/ticker subscriptions, heartbeat,
rate limits, auth), Hyperliquid
`https://hyperliquid.gitbook.io/hyperliquid-docs/` (websocket subscriptions,
rate limits, asset IDs, signing, HIP-3, HIP-4 outcome markets + deployer
actions, contract specifications).*


