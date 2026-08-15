# Multivenue Trading Engine — Architecture & Plan

**Version:** 0.5 (`.env` secrets, no observability stack, testing pyramid added)
**Author:** Anton
**Date:** 2026-04-19 (initial); refreshed 2026-05-20

---

## Status snapshot (2026-05-20)

* Phases 0–6 + Phase 7-prep all landed; live-test gates being closed.
* **All four strategies shipped + CLI-selectable** via `--strategy {latency-arb|ev|cross-arb|rule-tree}`.
* `QueuedDispatcher` + worker thread = engine never blocks on network.
* Top-5 hot-path bottleneck fixes landed (signer ctx cache, keep-alive POST,
  yield_now-replaces-sleep, RxBuf cursor pair, conditional `reregister`).
* MEDIUM-priority hot-path follow-ups landed: pre-parsed `SecretKey`,
  lock-free `DispatchStatsAtomic`, per-item `now_ns()` in engine drain,
  `MultiBook::apply_at(idx, tick)`, tight ingress drain loop, hashed
  `SymbolMap` lookups.
* Tests: 432+ across 36 binaries, 23 alloc assertions, 10 fuzz targets,
  criterion `hot_path` baseline showing ~38 ns/tick warm.
* See `docs/hot-path-latency.md` for the full audit + bench history.

---

**Primary edge (v1):** Strategy B — Latency / Information Arbitrage, constrained to free data sources
**Staged edges (v2+):** A (true-probability mispricing), C (cross-market arb), D (resolution-structure exploitation)
**Stack:** Pure Rust (single process, compile-time `Strategy` trait, monomorphized)
**Deploy target (v1):** Local only — MacBook Pro M4 (Apple Silicon, arm64, 500 GB SSD)
**Cloud / colo:** deferred to Phase 7 (migration plan in §20)
**Paid data feeds:** deferred to Phase 6 (X filtered stream, Benzinga Pro, Blocknative mempool)
**AI role:** Claude as *strategy researcher* — offline/out-of-band only; never in hot path

---

## 1. Executive Summary

A single-binary Rust engine that runs **entirely on Anton's MacBook Pro M4** using **only free-tier external APIs** for v1. The goal is to prove the alpha on a honest, reproducible, zero-cost footing before spending on paid data or cloud infrastructure.

What this means in practice:

1. All ingest happens against free sources: Polymarket CLOB WS (free), Binance WS (free), public RSS news feeds (free), Alchemy Polygon WSS on the free tier (300M CU/mo) with QuickNode free as failover.
2. **X/Twitter filtered stream** and **Benzinga Pro News** are paid services and are **excluded from v1**. They're Phase-6 upgrades.
3. **Blocknative Mempool Platform** free tier is too throttled for real trading; **no mempool in v1**. Also Phase 6.
4. **No cloud services anywhere.** No hpc6a, no EBS, no KMS, no SSM, no CloudWatch, no Terraform, no Ansible. Even when we migrate to EC2 in Phase 7 it is a **plain Linux VM** — just a box, no surrounding managed services.
5. **Secrets = a single `.env` file.** Not Keychain, not KMS, not Vault. Loaded by `dotenvy` at boot, parsed into the process environment. `.env.example` is committed; `.env` is git-ignored and `chmod 600`.
6. **No observability stack.** No Prometheus, no Grafana, no external metrics. Live introspection happens via the built-in `ratatui` TUI and periodic HdrHistogram dumps to a log file. A trivial `/metrics` HTTP endpoint is exposed on `127.0.0.1` so it can be `curl`'d if useful, but nothing is required to consume it.
7. Deployment = `launchd` unit on Mac; logs/artifacts/replay = local SSD.
8. We **cannot** achieve the original 3 ms p99.9 target on macOS — no `io_uring`, no `isolcpus`, no hugepages, affinity is hint-only. We measure real numbers on Mac and treat the Linux migration as Phase 7.
9. **Tests first-class.** Unit tests per crate, integration tests at workspace level, property tests for parsers, allocation-counting tests enforcing 0 B/op on hot paths, fuzz targets for every byte-scanner. Detail in §21.

What survives unchanged from v0.3:
- Pure-Rust single-process architecture.
- `Strategy` trait + `Engine<S>` monomorphized dispatch; zero vtable cost.
- Zero-alloc / zero-copy / single-writer / lock-free doctrine.
- Full replay log + backtest harness.
- Claude as offline strategy researcher.

**v1 working edge on free tier:** Binance price moves → Polymarket crypto markets is the one reliably fast leg. RSS → politics/macro markets is slower (seconds to minutes) but still beats retail. Alchemy-driven on-chain event → protocol market is cold-path only on free tier quotas. This is a narrower edge than v0.3 — intentionally so, to derisk the build.

---

## 2. Goals, Non-Goals, Constraints

### 2.1 Goals
- **Self-sufficient local deployment.** One binary on the Mac, no cloud services, no paid APIs.
- **Zero runtime allocation** in hot paths. Enforced via `dhat` and a counting allocator in debug builds.
- **Single-writer, lock-free** transport between stages. SPSC rings via `crossbeam` + custom cache-line-padded atomics.
- **Zero-copy network parsing** wherever macOS permits. Any copy annotated `// COPY: reason=<...>`.
- **Fully replayable.** Every inbound wire byte persisted to a local SSD replay log; harness re-feeds into the same binary.
- **Fail-fast.** `panic = "abort"` in release; `debug_assert!` for invariants; crash over degrade.
- **Compile-time strategy dispatch.** No `dyn`, no virtual calls in hot path.

### 2.2 Non-Goals (v1)
- Linux-grade latency on macOS. Not achievable; documented and measured.
- Paid data feeds.
- **Any cloud service, at any phase.** Even Phase 7 EC2 stays plain — no AWS-managed anything around it.
- **Any secret manager.** `.env` is the only secret mechanism.
- **Any external observability stack.** No Prometheus, Grafana, Datadog, CloudWatch, etc. TUI + log files + trivial `/metrics` endpoint only.
- Market-making.
- ~~Multi-venue execution.~~ **Superseded 2026-08-14 by
  `docs/phase-8-plan.md`** (operator directive): OKX, Deribit and
  Hyperliquid (incl. HIP-4 outcome markets) are wired for market data
  *and* — in Stage 3 — order routing. Stage-1 market-data capture on
  all venues landed 2026-08-15 (8a–8e).
- GUI. CLI + ratatui TUI only.

### 2.3 Hard Constraints
- **Host:** MacBook Pro M4, 500 GB SSD, Apple Silicon (arm64). Total project disk budget ~50 GB.
- **OS:** macOS. This means:
  - No `io_uring` → replay writer uses `pwrite(2)` from a dedicated thread via an SPSC byte ring.
  - No `isolcpus` / `nohz_full` / `rcu_nocbs`.
  - CPU affinity is a **hint only** via Mach `thread_policy_set(THREAD_AFFINITY_POLICY)`. The scheduler may ignore it.
  - No hugepages (posix-style).
  - No `MSG_ZEROCOPY`.
  - Raw sockets require elevated entitlements; we do not use them.
  - `mlockall` works but pages can still be evicted under memory pressure; we keep project RSS < 4 GB and run plugged-in.
- **Network:** residential ISP. Real v1 latencies to Polymarket/Binance/Alchemy vary with ISP routing. Document baseline; colo is Phase 7.
- **Polymarket geoblocks US IPs.** Anton is non-US and outside the US — compatible.
- **Polymarket signing key:** MetaMask-exported, stored in the project `.env` file (chmod 600, git-ignored). Loaded once at boot into an `mlock`'d page and zeroized on drop. No Keychain, no KMS, no secret manager.

### 2.4 Expected Performance on Mac vs Future Colo
| Metric | v1 (Mac, free tier) | Phase 7 target (Linux colo, paid tier) |
|---|---|---|
| Tick-to-order p99.9 | 20–100 ms (best-effort) | < 3 ms |
| News-to-order (Binance → Polymarket) | 50–500 ms | < 20 ms |
| News-to-order (RSS → Polymarket) | 5–60 s | n/a (replaced by X/Benzinga) |
| Mempool access | none | Blocknative WSS |
| Replay write latency | ~1–5 ms (Mac NVMe via pwrite) | <100 µs (io_uring + local NVMe) |

These gaps are expected; the point of v1 is to prove strategy logic, not latency.

---

## 3. Edge Thesis (free-tier-constrained)

### 3.1 Revised strategy-B legs for v1

**B.1 — Binance price move → crypto market.** PRIMARY. Binance WS is free and fast. Polymarket has many BTC/ETH/alt "Will X hit Y by Z" markets. We compute the implied Δprob from a Binance print and decide whether Polymarket's book is stale. This is the only leg where v1 can plausibly generate live alpha.

**B.2 — Public RSS headline → politics/macro market.** SECONDARY. Latency is seconds to minutes — RSS polls, not streams. Still beats casual participants for major inflection events. Treat as opportunistic, not a primary income leg.

**B.3 — On-chain event (via Alchemy free tier) → protocol market.** COLD. Free tier (300M CU/mo, ~12.5 req/s) supports event-log subscriptions for a handful of contracts. Good enough for protocol-exploit and governance-vote markets that don't fire often.

### 3.2 What gets deferred to Phase 6 (paid APIs)
- X/Twitter filtered stream v2 (Basic tier ~$200/mo+).
- Benzinga Pro News API (~$300–$500/mo+).
- Blocknative Mempool Platform (paid plan for usable throughput).
- Bloomberg / Dow Jones feeds (enterprise).

### 3.3 Staging (unchanged from v0.3)
- v1: Strategy B (B.1 primary, B.2 secondary, B.3 cold). Paper → tiny live.
- v2: Strategy A.
- v2.5: Strategy C.
- v3: Strategy D.
- Phase 6: paid data upgrade.
- Phase 7: Linux colo migration.

---

## 4. System Architecture (single Rust process, local Mac)

```
+------------------------------------------------------------------+
| RUST PROCESS — one binary on macOS, best-effort affinity         |
|                                                                  |
|  [ingress::polymarket]  [ingress::binance]  [ingress::rss]       |
|  [ingress::rpc (Alchemy primary, QuickNode failover)]            |
|         \                      |                   /             |
|          \                     v                  /              |
|           \         +--------------------+       /               |
|            +------->| signal_ring (SPSC) |<-----+                |
|                     +---------+----------+                       |
|                               |                                  |
|  +------------------+         v          +---------------------+ |
|  | tick_ring (SPSC) |<--[book_builder]-->| order_ack_ring      | |
|  +---------+--------+                    +---------------------+ |
|            |                                       ^             |
|            v                                       |             |
|       +----+----------------------+                |             |
|       | Engine<S: Strategy>       |---------------->            |
|       |   - decision              |   +--------------------+    |
|       |   - risk book             |-->| order_dispatch_ring|    |
|       |   - position book         |   +---------+----------+    |
|       +---------------------------+             |                |
|                                                 v                |
|                              +---------------------------------+ |
|                              | clob_dispatcher (EIP-712 signer)| |
|                              | persistent HTTPS → CLOB REST    | |
|                              +---------------------------------+ |
|                                                                  |
|  [replay_writer] SPSC byte ring -> pwrite() on dedicated thread  |
|                  -> ~/multivenue/replay/ on internal SSD         |
|  [metrics] HdrHistogram snapshotted each 1 s -> localhost Prom   |
|  [research_bridge] UDS -> claude_worker (separate process)       |
+------------------------------------------------------------------+
                         ^
                         | unix domain socket + files
                         v
+------------------------------------------------------------------+
| claude_worker (Python 3.14, cold path)                           |
|   - rule parser (Sonnet 4.6)                                     |
|   - topic tagger (Haiku 4.5, bulk)                               |
|   - news labeler (Sonnet, warm, 500 ms timeout fallthrough)      |
|   - backtest reviewer (Opus 4.6, batch only)                     |
|   - aggressive SQLite prompt cache                               |
+------------------------------------------------------------------+
```

Gone from v0.3: Blocknative mempool adapter, Benzinga adapter, X adapter. All three re-enter in Phase 6.

---

## 5. Rust Workspace Layout

```
polymarket/
├── PLAN.md                       (this document)
├── README.md
├── CLAUDE.md                     project context for Claude sessions (§22)
├── AGENTS.md                     tool-agnostic agent brief (§22)
├── .env.example                  committed; copied to .env and filled in
├── .env                          git-ignored; real secrets (chmod 600)
├── .gitignore
├── rust-toolchain.toml           (channel = "1.85")
├── Cargo.toml                    (workspace)
├── .cargo/config.toml            (rustflags for apple-m1)
├── .claude/
│   ├── settings.json             team-shared Claude settings
│   ├── agents/                   subagent definitions (§22)
│   │   ├── hft-reviewer.md
│   │   ├── strategy-researcher.md
│   │   └── backtest-reviewer.md
│   ├── commands/                 project slash commands (§22)
│   │   ├── research.md
│   │   ├── backtest.md
│   │   └── alloc-check.md
│   └── skills/                   project-specific skills (optional)
├── crates/
│   ├── core-types/               POD structs (#[repr(C)], Copy)
│   ├── core-ring/                SPSC rings, cache-line padding, seqlock slots
│   ├── core-time/                monotonic clock; on arm64 we read CNTVCT_EL0
│   ├── core-alloc/               CountingAllocator, arena, pool
│   ├── core-io/                  pwrite-based replay writer (io_uring later)
│   ├── core-net/                 rustls bootstrap, persistent WS client, H/2 client
│   ├── core-parse/               handwritten byte scanners, NEON numeric parse
│   ├── core-simd/                NEON on Apple Silicon; AVX2 gated for Phase 7
│   ├── core-config/              loads `.env` via `dotenvy`, typed accessors
│   ├── ingress-polymarket/       CLOB WS adapter
│   ├── ingress-binance/          spot + futures WS (free)
│   ├── ingress-rss/              RSS poller (public feeds only in v1)
│   ├── ingress-rpc/              Alchemy primary + QuickNode failover (free tiers)
│   ├── book-builder/             order book, ladders, top-of-book
│   ├── signer-eip712/            secp256k1-based EIP-712 signer
│   ├── clob-dispatcher/          REST client, persistent H/2 to CLOB
│   ├── risk/                     position/risk books, kill switch, quota guard
│   ├── strategy-core/            Strategy trait, Engine<S>, Ctx, Event enum,
│   │                              StrategyCounters, CooldownGate<N>
│   ├── strategy-latency-arb/     Strategy B — cross-venue mid-vs-mid (SHIPPED)
│   ├── strategy-ev/              Strategy A — model-vs-market via claude-worker
│   │                              artifacts (SHIPPED)
│   ├── strategy-cross-arb/       Strategy C — sum-of-probabilities deviation
│   │                              detector (SHIPPED)
│   ├── strategy-rule-tree/       Strategy D — rule artifacts from
│   │                              claude-worker/rule_parser.py (SHIPPED)
│   ├── research-artifacts/       NDJSON tag table + JSON rule table loader
│   ├── cli/                      main binary (clap) — `--strategy` selector
│   ├── tui/                      read-only ratatui dashboard
│   └── bench/                    criterion benches (`hot_path`) + dhat alloc
│                                  assertions (23/23 zero-alloc + budgeted)
├── fuzz/                         cargo-fuzz targets for parsers (§21)
├── claude-worker/                Python 3.14 (uv-managed venv)
│   ├── pyproject.toml
│   ├── src/
│   │   ├── anthropic_client.py
│   │   ├── cli.py
│   │   ├── config.py
│   │   ├── rule_parser.py
│   │   └── topic_tagger.py
│   └── tests/                    pytest suites
├── artifacts/                    (generated, versioned JSON)
├── ops/
│   ├── launchd/                  macOS unit files
│   └── brew/                     brew bundle Brewfile
└── docs/
    ├── wire-format.md
    ├── risk-policy.md
    ├── local-setup.md
    ├── migration-to-linux.md
    └── runbooks/
```

**Removed since v0.4:** `core-keychain/`, `ops/grafana-local/`, `strategy-news/` (Strategy D's rule-tree consumes the news path). **Added:** `CLAUDE.md`, `AGENTS.md`, `.env.example`, `.claude/`, `core-config/`, `fuzz/`, `research-artifacts/`, `strategy-cross-arb/`, `strategy-rule-tree/`.

---

## 6. Strategy Abstraction (compile-time, zero dispatch cost)

Unchanged from v0.3.

```rust
// crates/strategy-core/src/lib.rs

pub trait Strategy: Sized + Send {
    fn on_start(&mut self, ctx: &mut Ctx<'_>);

    #[inline(always)]
    fn on_tick(&mut self, tick: &Tick, book: &Book, ctx: &mut Ctx<'_>);

    #[inline(always)]
    fn on_signal(&mut self, sig: &Signal, ctx: &mut Ctx<'_>);

    #[inline(always)]
    fn on_fill(&mut self, fill: &Fill, ctx: &mut Ctx<'_>);

    #[inline(always)]
    fn on_timer(&mut self, now_ns: u64, ctx: &mut Ctx<'_>);

    fn on_stop(&mut self, ctx: &mut Ctx<'_>) {}
}
```

```rust
pub struct Engine<S: Strategy> {
    strategy: S,
    tick_ring: RingConsumer<Tick>,
    sig_ring:  RingConsumer<Signal>,
    fill_ring: RingConsumer<Fill>,
    order_out: RingProducer<Order>,
    books:     BookStore,
    risk:      RiskBook,
    clock:     Clock,
}

impl<S: Strategy> Engine<S> {
    #[inline(always)]
    pub fn run(&mut self) -> ! {
        self.strategy.on_start(&mut self.ctx());
        loop {
            while let Some(t) = self.tick_ring.try_pop() {
                self.books.apply(&t);
                let book = self.books.get(t.market_id);
                self.strategy.on_tick(&t, book, &mut self.ctx());
            }
            while let Some(s) = self.sig_ring.try_pop() {
                self.strategy.on_signal(&s, &mut self.ctx());
            }
            while let Some(f) = self.fill_ring.try_pop() {
                self.risk.apply_fill(&f);
                self.strategy.on_fill(&f, &mut self.ctx());
            }
            self.strategy.on_timer(self.clock.now_ns(), &mut self.ctx());
        }
    }
}
```

Multiple strategies = multiple monomorphized `Engine<_>` instances, one per best-effort-pinned thread.

---

## 7. Polymarket Integration

### 7.1 Endpoints (free — no change)
- **CLOB REST** `https://clob.polymarket.com`
- **CLOB WS** `wss://ws-subscriptions-clob.polymarket.com/ws/` — `market` + `user` channels.
- **Gamma** `https://gamma-api.polymarket.com` — market catalog, rule text.
- **Polygon RPC** via **Alchemy** (primary, free tier) + **QuickNode** (failover, free tier).
- **Data API / subgraph** — backtests only, cold path.

### 7.2 Signer key and all other secrets — `.env` file
All secrets live in a single `.env` file at the project root (and a deployment-time copy at `~/multivenue/.env`). `chmod 600`, owner-only, git-ignored. The `.env.example` is committed with placeholder values.

`.env.example`:
```
# Polymarket EIP-712 signer (MetaMask-exported hex, no 0x prefix)
POLYMARKET_EIP712_KEY=CHANGEME

# External service API keys
ANTHROPIC_API_KEY=sk-ant-CHANGEME
ALCHEMY_API_KEY=CHANGEME
QUICKNODE_API_KEY=CHANGEME
BINANCE_API_KEY=                  # optional; leave blank for public streams

# Runtime config
MULTIVENUE_REPLAY_DIR=/Users/anton/multivenue/replay
MULTIVENUE_ARTIFACTS_DIR=/Users/anton/multivenue/artifacts
MULTIVENUE_CACHE_DIR=/Users/anton/multivenue/cache
MULTIVENUE_LOG_DIR=/Users/anton/Library/Logs/polymarket
```

At boot, the `core-config` crate loads `.env` via `dotenvy` into the process environment, then reads typed values:

```rust
// core-config: sketch — one load, zero allocations after boot
pub struct Config {
    pub eip712_key: SecretKey,        // mlock'd, zeroized on Drop
    pub anthropic_api_key: String,    // held in one place, never cloned
    pub alchemy_api_key: String,
    pub quicknode_api_key: String,
    pub replay_dir: PathBuf,
    // ...
}

impl Config {
    pub fn load() -> Self {
        dotenvy::dotenv().ok();       // idempotent; no-op if .env missing in dev
        let key_hex = std::env::var("POLYMARKET_EIP712_KEY")
            .expect("POLYMARKET_EIP712_KEY missing");
        let mut key_bytes = mlock_page_32();
        hex::decode_to_slice(&key_hex, &mut key_bytes)
            .expect("EIP712 key must be 32 bytes hex");
        let eip712_key = SecretKey::from_bytes(&key_bytes);
        key_bytes.zeroize();
        // ... remaining env vars
        Config { eip712_key, /* ... */ }
    }
}
```

The signing key bytes live in a single `mlock`'d page and are zeroized on drop. No Keychain, no KMS, no secret manager. Ownership of the machine (or the EC2 instance) is the security boundary.

### 7.3 Signer implementation
- `secp256k1` (libsecp256k1-sys) for signing. Hand-rolled EIP-712 typed-data hash, no `ethers`/`alloy` full stacks.
- Nonce manager: single-threaded monotonic `u64` per API key.
- Zeroization via the `zeroize` crate; `mlock` on the key page.

### 7.4 CLOB REST dispatcher
- Persistent HTTPS via `hyper` + `hyper-rustls`, one long-lived H/2 connection.
- Preallocated body buffer `[u8; 2048]` and header set.
- `TCP_NODELAY`; `SO_SNDBUF`/`SO_RCVBUF` tuned (macOS limits apply).
- **Documented copy:** serialized order → hyper send buffer is one bounded memcpy. `// COPY: order body → hyper send buffer; unavoidable (hyper owns socket)`.

---

## 8. External Signal Pipeline (free-tier only)

### 8.1 Source matrix

*Amended 2026-08-15 (Phase 8e): three venue WS feeds + per-venue boot
REST discovery added; all live-verified. RSS retires to claude-worker
in Stage 2 (phase-8-plan §8.1).*

| Source | Tier | Transport | Hot path? | Purpose |
|---|---|---|---|---|
| Polymarket CLOB WS | free | WSS | YES | ticks, fills |
| Binance spot WS (combined streams) | free | WSS | YES | crypto price moves — B.1 primary |
| Binance futures WS | free | WSS | YES | mark price, liquidations — B.1 primary |
| OKX public WS v5 (bbo-tbt · trades · mark · funding · books) | free | WSS | YES | multivenue ticks + capture (Phase 8b) |
| Deribit JSON-RPC WS (quote · ticker · trades · book) | free | WSS | YES | multivenue ticks + capture (Phase 8c) |
| Hyperliquid WS (bbo · l2Book · trades · ctx · HIP-4 `#enc`) | free | WSS | YES | multivenue ticks + outcome markets (Phase 8d) |
| Venue REST discovery (OKX instruments · Deribit get_instruments · HL POST /info · Gamma by token) | free | HTTPS, boot-only | NO | instrument universes, tick/lot metadata, §6.1 coverage audit (Phase 8e) |
| Public RSS feeds (crypto + politics + macro) | free | HTTP poll | WARM | headlines — B.2 secondary (retiring to claude-worker, Stage 2) |
| Alchemy Polygon WSS | free tier (300M CU/mo) | WSS | COLD/WARM | on-chain event logs — B.3 |
| QuickNode Polygon WSS | free tier (~10M CU/mo) | WSS | STANDBY | failover only |
| Gamma REST | free | HTTPS poll | COLD | market catalog refresh, 15-min cadence |
| Polymarket data subgraph | free | HTTPS | COLD | backtest only |
| *(X filtered stream)* | *paid, deferred* | — | — | Phase 6 |
| *(Benzinga Pro)* | *paid, deferred* | — | — | Phase 6 |
| *(Blocknative mempool)* | *paid, deferred* | — | — | Phase 6 |

### 8.2 Binance (B.1 leg — primary v1 alpha)
- `wss://stream.binance.com:9443/stream?streams=btcusdt@aggTrade/btcusdt@bookTicker/ethusdt@aggTrade/...`
- `wss://fstream.binance.com/stream` for perpetual futures (mark price, liquidations).
- No API key required for market data; we avoid using one so we don't tie a KYC'd account to the activity.
- Rust `ingress-binance` adapter:
  - Preallocated RX buffer `[u8; 1<<20]`.
  - Handwritten byte scanner over the compact Binance JSON.
  - Outputs `PriceSignal { symbol_id, px_e8: i64, qty_e8: i64, ts_ns, kind }` into `signal_ring`.
  - Symbols map via Claude-generated `artifacts/topics/binance_symbols.json`.

### 8.3 RSS (B.2 leg — secondary)
Free public RSS / Atom feeds. Allowlist starter set (verify URLs at implementation):
- **General macro / politics:**
  - AP Top News — `https://apnews.com/index.rss`-class feeds
  - BBC World News RSS
  - Reuters World feed (via the public front-page RSS they still offer)
  - White House Briefings RSS
  - Federal Reserve press releases RSS
  - SEC Press Releases RSS
  - CFTC Press Releases RSS
- **Crypto news:**
  - CoinDesk RSS
  - CoinTelegraph RSS
  - TheBlock RSS
  - Decrypt RSS
  - Bitcoin Magazine RSS
- **Regulatory / legal:** CourtListener RSS for specific dockets, EU Commission press RSS.

Implementation:
- Poller on a cold thread, 15–60 s cadence per feed.
- Deduplicate by `(feed_id, item_guid)`.
- Aho-Corasick over `&[u8]` against the Claude-generated keyword automaton.
- Matches → `NewsSignal` on `signal_ring`. Timestamp = earlier of `pubDate` and `received_at`.
- Mark all RSS signals with `latency_class = Slow` so the strategy knows to size them down.

### 8.4 Alchemy + QuickNode (B.3 leg — cold/warm)
- **Primary:** Alchemy Polygon free tier. 300M CU/mo, ~12.5 req/s sustained. WSS supports `eth_subscribe` on logs.
- **Subscriptions:**
  - Event logs on Polymarket Exchange + CTF contracts.
  - Event logs on USDC.e for our funder/proxy addresses only.
  - `newHeads` for freshness/clock-skew sanity.
- **Failover:** QuickNode free tier (~10M CU/mo). Held open, idle, rotated on primary WSS heartbeat silence > 2 s OR error-rate > 5 % / 60 s.
- **Quota guard:** a counter in `ingress-rpc` tracks CU/req consumption; if we'd exhaust the Alchemy monthly envelope at current rate we downgrade subscription scope (drop USDC.e transfer watching first, then drop CTF, keep Exchange).
- **No `eth_getLogs` polling** in v1 — all event delivery is push-based WSS subscriptions to stay within free limits.
- **No mempool** in v1.

### 8.5 Signal-to-market map
- Claude writes `artifacts/topics/{market_id}.json`.
- At boot the engine loads all artifacts into perfect-hash `topic_id → &[market_id]` structures (`phf` at runtime).
- All matching is `&[u8]`-based, zero-alloc.

---

## 9. Risk Controls

Same as v0.3 core risks, plus local-deployment and free-tier specifics:

1. Per-market / per-family / global notional caps (v1 caps deliberately tiny; §15).
2. Max open-orders in flight.
3. Latency kill switch — p99.9 tick-to-order over sliding window above configured limit → halt.
4. Slippage guard — fill vs quote > N bps → halve size for that market, session-long.
5. Oracle divergence guard.
6. Gas + USDC.e balance headroom.
7. **Managed-RPC failover guard** — two-signal health check, atomic rotation primary↔secondary.
8. **Free-tier quota guard** — compute-units / requests consumed vs monthly envelope; at 80 % → alert, at 95 % → auto-downgrade subscription scope.
9. **RSS feed staleness** — any feed silent > N minutes → healthcheck warning, no trading halt.
10. **Mac thermal / power guard** — if `pmset` reports on battery or the system reports thermal throttling → halt new positions and alert. (Battery mode on a laptop is not a trading environment.)
11. **Global kill switch** backed by an atomic flag, triggered on any critical anomaly.

---

## 10. Claude as Strategy Researcher (free-tier budget)

### 10.1 Model selection by task
- **Haiku 4.5** — bulk topic tagging across thousands of markets. Cheapest, good enough for label-style tasks.
- **Sonnet 4.6** — rule parsing, news labeling, most reasoning tasks.
- **Opus 4.6** — backtest review, hard ambiguity cases. Batch only.

### 10.2 Budget discipline
- Aggressive **SQLite prompt cache** keyed by `(model, prompt_version_hash, content_hash)`. Most prompts are idempotent over a market's lifetime.
- Pre-filter with cheap logic before calling Claude:
  - Haiku runs over all markets once, classifies into broad topics.
  - Sonnet only sees markets Haiku flagged as "needs deeper parse" or "ambiguous".
  - Opus only sees backtest reports or markets a strategy actually traded.
- Realistic monthly API cost at v1 scope (few hundred active markets): expected single-digit dollars.

### 10.3 Process model
- `claude-worker` is a Python 3.14 process started by `launchd`; the
  unit invokes `claude-worker serve` (the 8f full-auto daemon —
  operator verbs run ad hoc, no daemon).
- UDS for on-demand requests from Rust engine; filesystem for artifacts.
- Hot path never waits on Claude. Warm news-labeler has a strict 500 ms timeout; on timeout the engine proceeds on keyword heuristics alone.

### 10.4 Python doctrine (user preference)
Full `import` only; never `from ... import ...`:

```python
# claude-worker/src/rule_parser.py
import json
import pathlib
import anthropic

client = anthropic.Anthropic()

def parse_market(market: dict) -> dict:
    resp = client.messages.create(
        model="claude-sonnet-4-6",
        max_tokens=2048,
        system=_SYSTEM,
        messages=[{"role": "user", "content": json.dumps(market)}],
    )
    return json.loads(resp.content[0].text)
```

### 10.5 Anthropic API key
- Lives in `.env` as `ANTHROPIC_API_KEY`.
- The Python worker calls `dotenv.load_dotenv()` at startup (full `import dotenv` per preference); the Anthropic SDK reads the env var directly.
- Anthropic account funded with minimum buffer; v1 monthly spend budgeted in single dollars.

### 10.6 Agent SDK role
- Backtest reviewer uses the Claude Agent SDK — batch jobs only, kicked off via `make research`. Never hot-path.

---

## 11. Local Environment — Dev and v1 Prod (MacBook Pro M4)

For v1 the Mac **is** production. Same toolchain runs dev and live trading.

### 11.1 Install checklist (one-time, in order)

```sh
# toolchains
brew install rustup-init git cmake pkg-config openssl@3 libgit2 uv python@3.14
rustup-init -y --default-toolchain 1.85.0 --profile default
rustup component add rust-src clippy rustfmt llvm-tools-preview
rustup target add aarch64-apple-darwin
rustup target add x86_64-unknown-linux-gnu   # kept for Phase 7 migration; not used in v1

# cross-compile prep for Phase 7 (optional at v1)
brew install zig
cargo install cargo-zigbuild

# cargo utilities
cargo install cargo-nextest cargo-bloat cargo-llvm-cov cargo-expand
cargo install cargo-criterion cargo-audit cargo-deny

# runtime deps for dev
brew install jq ripgrep fd pv

# python worker env
cd claude-worker
uv sync
```

### 11.2 `rust-toolchain.toml`
```toml
[toolchain]
channel = "1.85.0"
components = ["rust-src", "clippy", "rustfmt", "llvm-tools-preview"]
targets = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
profile = "minimal"
```

### 11.3 `.cargo/config.toml`
```toml
[build]
rustflags = []

[target.aarch64-apple-darwin]
rustflags = [
  "-C", "target-cpu=apple-m1",
  "-C", "link-arg=-Wl,-dead_strip",
]

# Phase 7 target, stays defined for migration-day
[target.x86_64-unknown-linux-gnu]
linker = "x86_64-linux-gnu-gcc"
rustflags = [
  "-C", "target-cpu=znver3",
  "-C", "target-feature=+avx2,+bmi2,+adx,+aes,+pclmulqdq",
  "-C", "link-arg=-Wl,--gc-sections",
]

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
debug = 1

[profile.bench]
inherits = "release"
debug = 2
```

### 11.4 macOS-specific degradations (acknowledged and documented)

| Capability | Linux (future) | macOS (v1 reality) |
|---|---|---|
| Thread pinning | `sched_setaffinity` — hard pin | `thread_policy_set(THREAD_AFFINITY_POLICY)` — **hint only** |
| Kernel isolation | `isolcpus`, `nohz_full`, `rcu_nocbs` | none |
| Async I/O | `io_uring` | none — use dedicated writer thread with `pwrite` |
| Zero-copy TX | `MSG_ZEROCOPY`, `SO_ZEROCOPY` | none — extra memcpy in kernel |
| Hugepages | explicit, sized | none (macOS "super pages" are automatic; no userspace control) |
| Raw sockets | root/setcap | requires elevated entitlements; not used |
| Memory locking | `mlockall(MCL_CURRENT \| MCL_FUTURE)` | works; OS may still evict under pressure |
| RDTSC-equivalent clock | `rdtscp` on x86 | `CNTVCT_EL0` on arm64 (inline asm) |

All of these are abstracted behind traits in `core-io`, `core-time`, etc., so the Linux migration in Phase 7 is a matter of swapping implementations, not changing call sites.

### 11.5 Power / thermal posture
- Plugged in at all times during trading.
- macOS **High Power Mode** enabled (Settings → Battery → Energy Mode).
- System sleep disabled while trading: `caffeinate -di polymarket-cli` or via `pmset`.
- Close heavy background apps (Slack, Chrome) — they contend for CPU.
- Target ambient: cool room; thermal throttling shows up in latency histograms.

### 11.6 Secrets in dev == prod
- Single `.env` file at the project root during dev and at `~/multivenue/.env` for the deployed binary.
- `chmod 600 .env`.
- `.env` is in `.gitignore`; `.env.example` (placeholders only) **is** committed.
- No Keychain, no KMS, no Vault, no 1Password CLI hook. One file, owner-read, loaded by `dotenvy` at boot.

---

## 12. Local Deployment (macOS, not AWS)

### 12.1 File layout on disk
| Path | Size cap | Purpose |
|---|---|---|
| `~/multivenue/bin/` | <100 MB | release binary |
| `~/multivenue/.env` | <4 KB | secrets, `chmod 600` |
| `~/multivenue/config.toml` | <4 KB | non-secret runtime config (strategy caps, feed URLs) |
| `~/multivenue/replay/` | 50 GB | binary replay log, rolled hourly, retention 7 days |
| `~/multivenue/artifacts/` | 5 GB | Claude-generated JSON |
| `~/multivenue/cache/` | 5 GB | SQLite prompt cache, gamma snapshots |
| `~/Library/Logs/polymarket/` | 1 GB | log files (HdrHistogram dumps, events) |
| project repo (Rust + Python) | 10–20 GB | source + `target/` |

Total footprint: **~70–80 GB**. Fits comfortably on 500 GB SSD.

### 12.2 launchd unit
`~/Library/LaunchAgents/com.polymarket.engine.plist` (user agent, runs after login):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>                 <string>com.polymarket.engine</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/anton/multivenue/bin/polymarket-cli</string>
        <string>run</string>
        <string>--config</string>
        <string>/Users/anton/multivenue/config.toml</string>
    </array>
    <key>RunAtLoad</key>             <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>    <false/>
    </dict>
    <key>ProcessType</key>           <string>Interactive</string>
    <key>Nice</key>                  <integer>-10</integer>
    <key>StandardOutPath</key>       <string>/Users/anton/Library/Logs/polymarket/stdout.log</string>
    <key>StandardErrorPath</key>     <string>/Users/anton/Library/Logs/polymarket/stderr.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_BACKTRACE</key>    <string>1</string>
    </dict>
    <key>WorkingDirectory</key>      <string>/Users/anton/polymarket</string>
</dict>
</plist>
```

The binary itself calls `dotenvy::dotenv()` at startup to load `/Users/anton/multivenue/.env` into the process environment — no shell wrapper, no `envsubst`, no `direnv` integration needed.

Load: `launchctl load -w ~/Library/LaunchAgents/com.polymarket.engine.plist`.
A second plist starts `claude-worker` similarly.

### 12.3 Best-effort CPU placement
M4 has performance + efficiency cores. We want trading threads on performance cores. We set **Quality of Service (QoS) classes** to `QOS_CLASS_USER_INTERACTIVE` for hot threads and `QOS_CLASS_UTILITY` for cold threads — macOS uses QoS to decide P-core vs E-core scheduling. Combined with `THREAD_AFFINITY_POLICY` hints across different L2 groups, this is the best we can do on macOS.

```rust
// core-time / core-ring sketch
unsafe {
    libc::pthread_set_qos_class_self_np(
        libc::QOS_CLASS_USER_INTERACTIVE,
        0,
    );
}
```

Result: directional, not deterministic. Linux Phase 7 fixes this.

### 12.4 Networking posture
- Wired Ethernet if possible (USB-C/Thunderbolt adapter). Wi-Fi introduces 5–50 ms jitter spikes.
- `TCP_NODELAY` on every socket.
- Disable background iCloud sync during trading (`killall bird`) — iCloud can saturate upstream in bursts.
- Measure baseline RTT to `clob.polymarket.com`, `stream.binance.com`, `polygon-mainnet.g.alchemy.com` at daily boot; alert if p50 exceeds norm by > 2×.

### 12.5 Observability (no stack, by design)
- **TUI (`ratatui`)** is the primary live-introspection tool. Reads a shm snapshot page updated every 100 ms — zero hot-path impact. Shows: per-strategy PnL, positions, tick/sig rates, ring depths, p50/p99/p99.9 tick-to-order, heartbeat state.
- **HdrHistogram dumps** written to `~/Library/Logs/polymarket/latency-YYYYMMDD.log` every 1 s by a cold thread. Human- and script-readable text (`p50=... p99=...`). Off-the-shelf grep/awk/`pv` is enough.
- **Event log** (`events-YYYYMMDD.log`) with one line per order sent, fill received, kill-switch trigger. Cold path only.
- **`/metrics` endpoint** on `127.0.0.1:9191` serves the same counters in Prometheus text format, for anyone who later wants to point a scraper at it. Running a scraper is not part of the plan.
- **Ring drop / backpressure counters** and **per-thread heartbeats** exposed via both TUI and `/metrics`; missed heartbeat = FailFast.
- No Prometheus process, no Grafana, no external metrics store in v1. The replay log is the authoritative historical record.

### 12.6 Backup and retention
- Replay log: rolled hourly, compressed (`zstd -3`) once sealed, retained 7 days local.
- Weekly rsync of sealed logs and artifacts to an external SSD (user-managed, not a cloud service).
- Prompt cache and artifacts are reproducible from Claude calls, so they are not strictly backed up.

---

## 13. External Services and Accounts (free tiers)

| Service | Purpose | Tier in v1 | Notes |
|---|---|---|---|
| Polymarket CLOB | trading | free | MetaMask funder; non-US wallet |
| Anthropic | Claude API | pay-as-you-go, budgeted single-digit $/mo | Haiku/Sonnet/Opus routed by task |
| Alchemy | Polygon RPC primary | **free (300M CU/mo)** | WSS event subscriptions for Exchange, CTF, USDC.e |
| QuickNode | Polygon RPC failover | **free (~10M CU/mo)** | idle, rotated on primary degrade |
| Binance | WS market data | free, no API key required | only quotes/trades — no trading |
| Public RSS feeds | news | free | polling, not streaming; see §8.3 |
| *(X / Twitter)* | *filtered stream* | *paid — deferred to Phase 6* | |
| *(Benzinga Pro)* | *structured news* | *paid — deferred to Phase 6* | |
| *(Blocknative)* | *mempool* | *paid — deferred to Phase 6* | |
| Centralized exchange (funding source) | clean USDC.e withdrawal | user's existing non-US CEX | one-time per top-up |

---

## 14. Backtesting & Replay

Unchanged:
- Harness reads the binary wire log and re-feeds it into the same `ingress::*` adapters via a `replay` transport.
- Clock injected from log timestamps.
- Strategy code identical; dispatcher swapped for a simulated book.
- Claude's offline reviewer consumes the output directory and produces a verdict + parameter-tuning PRs (Phase 7 — separate `claude-worker/src/` script; see roadmap below).

---

## 15. Phased Roadmap (reshaped for local + free tier)

### Phase 0 — Local scaffold (1–2 weeks)
- Rust workspace, `rust-toolchain`, cargo config.
- `core-ring`, `core-time`, `core-io` (pwrite writer), `core-net`, `core-parse`, `core-types`, `core-config` (dotenvy + `mlock`'d key page).
- `ingress-polymarket` + `book-builder` → tick ring → CLI pretty-printer.
- `dhat` soak: 0 allocations over 1 h on the hot path.
- launchd plist working; claude-worker shell scaffold.

### Phase 1 — Binance + RSS + Alchemy + Claude artifacts (2–3 weeks)
- `ingress-binance`, `ingress-rss`, `ingress-rpc` (Alchemy + QuickNode failover).
- `claude-worker/topic_tagger.py` (Haiku) + `rule_parser.py` (Sonnet) producing artifacts for ~500 markets.
- `strategy-latency-arb` in paper mode (writes would-be orders to replay log).
- TUI (`ratatui`) live dashboard wired up; HdrHistogram dumps to `~/multivenue/logs/latency/*.hgrm`; `/metrics` endpoint on `127.0.0.1:9191` exposing counters in Prometheus text format (no scraper consuming it).

### Phase 2 — Tiny live trading on Mac (2 weeks)
- `signer-eip712`, `clob-dispatcher`.
- Live at $10–$50 notional per market; total exposure capped at $500.
- Measure real tick-to-order distribution on the Mac. Accept 20–100 ms p99.9 as v1 reality.
- 48 h soak; 0 B/op in Release.
- Decide: does strategy B (primarily the Binance leg) generate positive net P&L at this scale after CLOB fees and Polygon gas? If yes → continue. If no → research pivot.

### Phase 3 — Strategy A groundwork (3–4 weeks)
- Per-family probability models in `strategy-ev` (stub → real).
- Model-vs-mid EV as a second decision leg.
- Scale caps modestly (say 5×) if Phase 2 was green.

### Phase 4 — Strategy C (3 weeks)
- Linked-market graph, sum-of-probabilities violations, synthetic hedges.

### Phase 5 — Strategy D (ongoing)
- Claude rule-tree parser rollout; ambiguity gating; human-in-the-loop on flagged markets.

### Phase 6 — Paid data upgrade (1–2 weeks)
Only undertaken if Phases 2–5 clear a P&L bar that justifies the monthly spend.
- Enable `ingress-x` (Twitter v2 Basic or Pro).
- Enable `ingress-benzinga` (Benzinga Pro API).
- Enable `ingress-mempool` (Blocknative Mempool Platform).
- Re-tune topic map to incorporate tagged news.

### Phase 7 — Linux migration (plain EC2 box; no cloud services)
Only undertaken if paid feeds improve edge and macOS is clearly the latency bottleneck.
- Rent a plain EC2 instance (instance type TBD when we get there — decided by measured bottleneck, not up front). **Nothing else from AWS**: no KMS, no SSM, no CloudWatch, no Secrets Manager, no Terraform, no Grafana Cloud, no observability bolt-ons. Just a Linux VM.
- Build `x86_64-unknown-linux-gnu` locally on Mac via `cargo-zigbuild`, `scp` the binary to the instance, run it under `systemd` (or even plain `nohup` + `tmux` — whichever is simplest).
- Swap `core-io` from `pwrite` to `io_uring` (already trait-gated).
- `core-config` stays identical — same `.env` file, `scp`'d over with `chmod 600`.
- Enable OS-level kernel tuning (`isolcpus`, `nohz_full`, hugepages, hard CPU pinning) — these are Linux features, not cloud services.
- Re-measure tick-to-order; target the original < 3 ms p99.9.

Everything outside that list is deliberately absent. If we later decide a specific AWS managed service is worth adding, it goes through a separate ADR, not this plan.

---

## 16. Answers to Prior Open Questions (v1 posture)

| Question | Answer |
|---|---|
| Self-host Polygon? | **No.** Managed free tiers (Alchemy primary + QuickNode failover) cover v1. |
| X vs RSS trust? | **RSS only for v1** (free). X + Benzinga deferred to Phase 6. |
| Funder key? | **MetaMask-exported key in `.env` file** (chmod 600). Loaded via `dotenvy` at boot into an `mlock`'d page; zeroized on `Drop`. |
| Read panel? | **Rust `ratatui` TUI.** Local only. |
| Legal / KYC? | **Non-US resident + clean funder wallet + non-US-sourced USDC.** Sufficient for v1 onboarding. |
| Cloud / AWS? | **None in v1.** Phase 7 migration plan in §20. |
| Paid APIs? | **None in v1.** Phase 6 upgrade when P&L justifies spend. |

---

## 17. External Dependencies — Full Reference

### 17.1 Rust crates (allowlist, pinned in `Cargo.toml`)
- `crossbeam` 0.8 — bounded SPSC channels (fallback for non-custom rings).
- `memmap2` 0.9 — shm-file-backed rings (used for TUI snapshot page, not Linux hugepages).
- `bytemuck` 1.16, `zerocopy` 0.7 — POD casting.
- `rustls` 0.23 — TLS bootstrap only.
- `hyper` 1.x + `hyper-rustls` — persistent H/2 client for CLOB REST and RPC HTTPS.
- `http` 1.x — headers/URIs.
- `ring` 0.17 — crypto primitives.
- `secp256k1` 0.29 + `libsecp256k1-sys` — EIP-712 signing (audited C backend).
- `tiny-keccak` 2.0 — keccak256.
- `zeroize` 1.8 — key hygiene.
- `aho-corasick` 1.1 — keyword matching.
- `memchr` 2.7 — byte scanning.
- `phf` 0.11 — perfect-hash topic tables.
- `ahash` 0.8 — offline maps only.
- `tracing` 0.1 + `tracing-subscriber` 0.3 — cold-path structured logs (debug only).
- `hdrhistogram` 7 — latency stats.
- `ratatui` 0.28 + `crossterm` — TUI.
- `clap` 4 — CLI.
- `criterion` 0.5 — benches.
- `dhat` 0.3 — allocation profiling.
- `parking_lot` 0.12 — bootstrap only; never in hot path.
- **`dotenvy` 0.15** — `.env` loader (bootstrap only, cold path).
- **`hex` 0.4** — key hex decoding.
- **`kqueue` 1.0** (via `mio`) — macOS readiness.

**Testing / bench:**
- `proptest` 1.x — property-based tests for parsers (§21).
- `cargo-fuzz` + `libfuzzer-sys` — fuzz targets for every byte scanner.
- `dhat` 0.3 — alloc assertions in tests.
- `insta` 1.x — snapshot tests for artifact schemas.
- `criterion` 0.5 — bench.
- `mockito` 1.x — mock HTTP/WS servers in integration tests.

Not in the dependency tree (denied): `tokio` on hot path, `serde_json` on hot path, `ethers`, `alloy` (full), `async-std`, `io-uring` (v1 macOS), `reqwest` (pulls in unwanted async runtime on hot path), any secret-manager SDK.

### 17.2 Mac packages (brew bundle `Brewfile`)
```
tap "homebrew/bundle"

brew "rustup-init"
brew "git"
brew "cmake"
brew "pkg-config"
brew "openssl@3"
brew "libgit2"
brew "uv"
brew "python@3.14"
brew "zig"
brew "jq"
brew "ripgrep"
brew "fd"
brew "pv"
```

### 17.3 Python (`claude-worker`)
- Python 3.14 (uv-managed).
- `anthropic` — Anthropic SDK.
- `httpx` — for Gamma REST (cold).
- `pydantic` 2 — artifact schema validation.
- `sqlite-utils` — prompt cache.
- No `from ... import ...` imports anywhere.

### 17.4 Services / accounts — see §13.

---

## 18. Doctrine Compliance Checklist

- [x] No runtime allocations in hot paths. Verified by `dhat` + counting allocator.
- [x] Zero-copy network parsing end-to-end on macOS (to the limits macOS offers); every copy annotated `// COPY: reason=<...>`.
- [x] Single-writer principle everywhere (SPSC rings).
- [x] Lock-free (`AtomicU64` seqlock slots + cache-line padding).
- [x] Compile-time strategy dispatch — `Engine<S: Strategy>`; no `dyn`.
- [x] Cache-aligned structs (`#[repr(align(64))]`, `#[repr(C)]`).
- [x] No iterator overhead in hot loops (raw indices + `get_unchecked`).
- [x] No bounds checks in hot loops (unsafe isolated inside safe APIs).
- [x] No `panic!` in release hot paths; `debug_assert!` only; `panic = "abort"`.
- [x] NEON for numeric parsing on Apple Silicon; AVX2 gated for Phase 7 linux-x86_64 target.
- [x] Fixed arrays for books, ladders, risk book.
- [x] Logging out-of-band, compiled out of hot path in Release.
- [x] No stringly-typed symbols — `type SymbolId = u32;` everywhere.
- [x] Fail-fast: `panic = "abort"`, watchdog FailFast on missed heartbeats.
- [x] Python Claude worker uses full `import x` (no `from ... import ...`).
- [x] All external deps pinned and `cargo audit` / `cargo deny` enforced in CI.

### 18.1 Doctrine items **intentionally unmet on v1 macOS**, tracked for Phase 7
- [ ] Hard CPU pinning — macOS offers only hints. Will land with `sched_setaffinity` + `isolcpus` on Linux.
- [ ] `io_uring`-based replay writer — macOS has no equivalent. pwrite on dedicated thread for now.
- [ ] `MSG_ZEROCOPY` TX path.
- [ ] Hugepages for shm rings.
- [ ] Deterministic < 3 ms p99.9 tick-to-order latency.

---

## 19. Risks & Watch-outs

1. **macOS scheduling variability** — realistic v1 p99.9 is 20–100 ms, not 3 ms. Strategies that rely on microsecond edges will not work at v1 scale. We target signals where 100 ms still catches Polymarket's lag.
2. **Free-tier quotas** — Alchemy 300M CU/mo is generous but a subscription storm (e.g., many linked contracts) can burn it. Quota guard in §9.
3. **RSS latency** is measured in seconds to minutes. B.2 edge is real only for inflection events, not day-to-day noise.
4. **No mempool in v1** — a whole class of MEV-adjacent signals is invisible. Phase 6.
5. **Polymarket ToS / geoblock evolution** — re-read quarterly.
6. **Chainalysis after-the-fact flagging** of funder — rotation runbook on standby.
7. **Claude API outage** — hot path never waits; warm labeler falls back to keyword heuristics on 500 ms timeout.
8. **Mac power events** — battery mode or thermal throttle triggers the guard in §9.10.
9. **Paid-upgrade break-even** — Phase 6 only triggered when P&L justifies $500–$2k/mo of feeds.

---

## 20. Migration Plan to Production (Phase 7)

The architecture is designed so the Linux/colo migration is a **swap of implementations behind traits**, not a redesign.

### 20.1 What stays
- All crate boundaries.
- `Strategy` trait, `Engine<S>`, every strategy implementation.
- Risk book, signer, dispatcher, book builder, parsers.
- Python Claude worker.
- Replay log format.
- All doctrine (zero-alloc, single-writer, lock-free, compile-time dispatch).

### 20.2 What swaps (trait impls, one per module)
| Module | v1 (macOS) | Phase 7 (plain Linux EC2) |
|---|---|---|
| `core-io::ReplayWriter` | `pwrite` dedicated thread | `io_uring` batched `IORING_OP_WRITE` |
| `core-config` | `dotenvy` on `.env` | **unchanged** — same `.env`, same loader |
| `core-time::ThreadAffinity` | `thread_policy_set` hint | `sched_setaffinity` hard pin |
| `core-time::HighResClock` | `CNTVCT_EL0` | `rdtscp` |
| `core-net::Tls` | rustls on macOS | rustls on Linux |
| deployment | `launchd` plist | `systemd` unit (or `nohup`, whichever is simplest) |
| observability | TUI + log files + `/metrics` on localhost | **unchanged** — TUI + log files + `/metrics` on localhost |

### 20.3 What gets added in Phase 7
- Paid feeds (if Phase 6 already done): `ingress-x`, `ingress-benzinga`, `ingress-mempool`.
- Linux kernel tuning (`isolcpus`, `nohz_full`, hugepages, `sched_setaffinity`, IRQ affinity). These are Linux features, not cloud services.

### 20.4 What is explicitly **not** added in Phase 7
- No Terraform, Ansible, Pulumi, or any IaC tool.
- No AWS managed service of any kind (KMS, SSM, Secrets Manager, CloudWatch, S3 for logs, etc.).
- No Prometheus, Grafana, Datadog, or external metrics stack.
- No log-shipping service.
- No multi-host architecture.

One binary, one `.env`, one Linux box. Adding anything else requires its own ADR.

### 20.5 Expected effort
- 1–2 engineer-weeks assuming no surprises, because every OS-specific touch point lives behind a trait in `core-*` and `core-config` is unchanged.
- The riskiest replacement is the replay writer (macOS `pwrite` → `io_uring`). It has its own integration test suite (§21) before flip.

---

---

## 21. Testing Strategy

Tests are not optional in an HFT codebase — a silent regression here means real money lost. Every crate ships tests; the workspace ships integration tests; parsers ship property tests and fuzz targets; hot paths ship allocation assertions.

### 21.1 Test pyramid

```
           +--------------------+
           | end-to-end replay  |   few, slow, production-like
           +--------------------+
          /                      \
         /   integration tests    \   per-ingress + engine + strategy
        +--------------------------+
       /                            \
      /     property + fuzz tests    \   byte-level parser correctness
     +--------------------------------+
    /                                  \
   /            unit tests               \   per-crate, per-function
  +--------------------------------------+
```

### 21.2 Unit tests (per crate, `src/` + `#[cfg(test)] mod tests`)
Every crate has inline unit tests. Non-exhaustive but representative coverage:

- **`core-ring`** — single-producer/single-consumer correctness, cache-line padding verified via layout assertions, wrap-around behavior at `u64::MAX / cap_mask`, seqlock torn-read rejection.
- **`core-parse`** — scanner correctness on every known Polymarket/Binance JSON frame shape, numeric edge cases (leading zeros, scientific notation rejection, fixed-point overflow).
- **`core-time`** — monotonicity, no backward jumps, NEON vs scalar parity.
- **`core-config`** — `.env` loading, `chmod` check refusal if world-readable, missing-var fail-fast.
- **`book-builder`** — multi-update consistency, crossed-book detection, zero-size level eviction.
- **`signer-eip712`** — known-answer tests against Polymarket-published test vectors; zeroization verified by reading the key page after `Drop` (debug builds).
- **`risk`** — kill-switch triggers, quota-guard math, notional cap enforcement.
- **`strategy-core`** — `Engine<S>` drains all three rings in order; timer fires at expected cadence.
- **`strategy-latency-arb`** — given a synthetic signal + stale book, produces an order; given a fresh book, does not.

Rule: **every public function has at least one happy-path and one failure-mode unit test.** Enforced in code review.

### 21.3 Property-based tests (`proptest`)
Parsers and numeric paths get property tests rather than example tests:

```rust
proptest! {
    #[test]
    fn parse_binance_roundtrip(px in 0u64..1_000_000_000_000u64, qty in 0u64..1_000_000_000u64) {
        let frame = synth_binance_frame(px, qty);
        let parsed = parse_binance_frame(&frame).unwrap();
        prop_assert_eq!(parsed.px_e8, px);
        prop_assert_eq!(parsed.qty_e8, qty);
    }

    #[test]
    fn book_applies_idempotently(updates in vec(any::<LevelUpdate>(), 0..1000)) {
        let mut book1 = Book::new();
        let mut book2 = Book::new();
        for u in &updates { book1.apply(u); book2.apply(u); book2.apply(u); }
        prop_assert_eq!(book1, book2);   // reapplying the last update is a no-op
    }
}
```

### 21.4 Fuzz targets (`cargo-fuzz` / `libfuzzer-sys`)
Every byte scanner has a fuzz target:
- `fuzz_targets/polymarket_clob_frame.rs`
- `fuzz_targets/binance_combined_frame.rs`
- `fuzz_targets/rss_atom_item.rs`
- `fuzz_targets/eth_log_subscription.rs`

Fuzzers run in CI on a budget (e.g., 5 min per target per PR). A corpus seed per target is committed under `fuzz/corpus/`. Goal: **no panic, no UB, no unreachable_unchecked trip** on any input.

### 21.5 Allocation assertions (0 B/op enforcement)
Hot-path tests use a custom `CountingAllocator` registered via `#[global_allocator]` that counts allocations after a `freeze()` point:

```rust
#[test]
fn engine_tick_path_allocates_zero() {
    let mut engine = Engine::<LatencyArbStrategy>::new_for_test();
    engine.warm_up();                     // allocations allowed
    core_alloc::ALLOC_COUNTER.freeze();   // now count
    for tick in synthetic_ticks(1_000_000) {
        engine.push_tick(tick);
        engine.run_once();
    }
    assert_eq!(core_alloc::ALLOC_COUNTER.count(), 0);
}
```

These tests run on every CI build. Failing = PR rejected.

In addition, the `bench/` crate runs `dhat` over each benchmark and fails the build if `total_blocks > 0` in the steady-state window.

### 21.6 Integration tests (workspace-level `tests/`)
- `tests/replay_roundtrip.rs` — record a synthetic day via the writer, replay it through the engine, assert identical order stream.
- `tests/mock_polymarket_ws.rs` — `mockito`-driven CLOB WS; engine subscribes, book builder consumes, strategy produces paper orders.
- `tests/mock_binance_ws.rs` — synthetic Binance feed + Polymarket stale book + expected latency-arb order.
- `tests/mock_alchemy_wss.rs` — simulated log subscription, failover trigger to mock QuickNode, assert no lost events.
- `tests/signer_vectors.rs` — canonical Polymarket-published EIP-712 vectors.
- `tests/quota_guard.rs` — simulate approaching CU cap; assert subscription downgrade fires.
- `tests/kill_switch.rs` — inject anomalies; assert halt.
- `tests/env_loading.rs` — missing/malformed `.env` → process refuses to start with clear message.

### 21.7 End-to-end replay tests (`tests/e2e/`)
A handful of committed replay-log samples (small, curated, privacy-scrubbed) live in `tests/e2e/fixtures/`. Each one:
- Replays through the full binary.
- Compares generated order stream byte-for-byte against a committed `expected.jsonl`.
- Any deviation = regression.

These run nightly locally (plus on-demand before any strategy parameter change) — not on every PR.

### 21.8 Python tests (`claude-worker/tests/`)
- `pytest` for each worker module.
- Prompt-hash cache behavior verified: same input → cached output.
- Artifact schema validated with `pydantic`.
- No live Anthropic API calls in CI — responses mocked at the SDK boundary.

### 21.9 CI layout
`make test` runs, in order:
1. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.
2. `cargo test --workspace` (unit + proptest + integration).
3. `cargo bench --no-run` (ensures benches compile).
4. `cargo fuzz run <target> -- -max_total_time=300` per target.
5. `uv run pytest` in `claude-worker/`.
6. `cargo audit && cargo deny check`.

`make test-fast` skips fuzz + bench compile for quick iteration.

### 21.10 Manual smoke before live flip
Before any size bump or new strategy goes live:
1. Replay last 24 h against the new binary; diff order stream; human review deltas.
2. 1 h paper soak against live feeds; 0 B/op + heartbeat + kill-switch smoke.
3. Tiny live (one market, $10 cap) for 1 h; compare fills to paper.
4. Only then scale.

---

## 22. Claude-Specific Files (CLAUDE.md, AGENTS.md, `.claude/`)

A future Claude session editing this codebase should not have to rediscover our doctrine. These files front-load the context.

### 22.1 `CLAUDE.md` (project root)
Loaded automatically by Claude Code at session start. Contains:
- One-line project purpose.
- Build/test/run commands (copy-pasteable).
- Architectural invariants (zero-alloc, single-writer, no `dyn` in hot path, no `tokio`/`serde_json` on hot path, no `from … import …` in Python, no cloud services).
- Directory guide (point to `PLAN.md` for depth).
- Common pitfalls ("if you're about to add tokio to `crates/core-*`, stop").

A skeleton is committed now at the project root (`/Users/darkcite/Documents/Claude/Projects/Polymarket/CLAUDE.md`).

### 22.2 `AGENTS.md` (project root)
Tool-agnostic twin of `CLAUDE.md` — read by Claude, Cursor, Codex, and any other agent tool that understands the format. Shorter than `CLAUDE.md`; just the rules an agent must follow.

Committed at the project root.

### 22.3 `.claude/settings.json`
Team-shared Claude Code settings for this repo. Defines:
- `permissions` — allow/deny lists for tools.
- `hooks` — pre-commit, pre-tool-use hooks (e.g., run `cargo fmt` before an Edit commits).
- `env` — project-level env (non-secret; secrets stay in `.env`).

### 22.4 `.claude/agents/` — subagents

Each `.md` file defines a specialized agent Claude can spawn via the Task tool.

- **`hft-reviewer.md`** — reads a diff, flags violations of the zero-alloc / single-writer / no-`dyn` doctrine. Blocks merge if findings are "red".
- **`strategy-researcher.md`** — given a market_id or market JSON, calls into `claude-worker` to produce/refresh artifacts (topic tag, rule tree). Runs offline.
- **`backtest-reviewer.md`** — reads a backtest report, flags overfitting / sample leakage / P&L anomalies, proposes parameter-tuning PR body. Runs batch.

### 22.5 `.claude/commands/` — slash commands

Each `.md` file is a named slash command.

- **`/research <market_id>`** — kick off `claude-worker` rule parser + topic tagger for that market.
- **`/backtest <YYYY-MM-DD>`** — run replay harness for that day, produce report, invoke backtest-reviewer.
- **`/alloc-check`** — run `cargo test --test alloc_assertions` and `cargo bench` with `dhat`; fail if any allocations.
- **`/dev-up`** — one-shot: load `.env`, start `claude-worker`, start engine in paper mode, open TUI.

### 22.6 `.claude/skills/` (optional)
Project-specific skills loaded into any Claude session in this repo:
- **`polymarket-clob-schema`** — concise reference of CLOB WS frame layouts; saves round-tripping to docs.
- **`eip712-polymarket`** — EIP-712 typed-data payload shape for CLOB orders, with a worked signing example.

Only created when the benefit outweighs maintenance cost; nothing in v1 Phase 0 yet.

### 22.7 `.gitignore` entries related to Claude/secrets
```
# secrets
.env
.env.*
!.env.example

# claude-code local state
.claude/local/
.claude/cache/

# artifacts
artifacts/rules/*.json
artifacts/topics/*.json
!artifacts/.keep
```

---

*End of plan v0.5. Next action: approve and kick off Phase 0 local scaffold.*
