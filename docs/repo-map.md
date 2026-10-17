# Repository map — the full directory guide

Moved verbatim from `CLAUDE.md` on 2026-10-17 (CLAUDE.md keeps a five-line
summary). Update it when a crate is added, linked or unlinked.

- `crates/core-*` — primitives: ring (SPSC only), time, config (every
  `*.toml` parser), alloc, io (PMLR writer/reader, `PmlrCapture`, atomic
  state files), net (mio + rustls transport, WS framing, `IoBuf`,
  `Keepalive`), parse (byte scanners + the `pb` protobuf walker), simd,
  crypto (SHA-256/HMAC/base64), types (wire PODs, `SymbolId`, `VenueId`),
  regime, vol (the HAR `VolEngine`, the whole-day `LongVolEngine` and its
  engine owner `LongVolSet`),
  settle (the Hypercall settlement law), fill, latency, metrics (fixed
  registry, 512 counters).
- `crates/ingress-{polymarket,binance,okx,deribit,hyperliquid,bybit,mexc,rpc,hyperevm,hypercall}` —
  one thread per source, `discovery.rs` = boot REST; `crates/ingress-ai` —
  the UDS+HMAC command plane and the ruleset validator.
- `crates/strategy-{set,core,hyparb,vm,ai-exec,vrp,xsd,bin15,xmm,hcv}` — the composed
  set and its members; `strategy-{latency-arb,cross-arb,ev,rule-tree,icdp}` are
  in-tree but unlinked/off. `book-builder`, `opt-registry`,
  `options-select`, `research-artifacts`.
- `crates/engine` — the single-threaded loop; `crates/engine-snapshot` —
  the seqlock `/state` snapshot; `crates/tui`.
- `crates/exec-router` + `crates/exec-hyperliquid` + `crates/signer-eip712`
  + `crates/clob-dispatcher` — the execution lane (see CURRENT STATE).
  `crates/exec-gateway` — the venue-agnostic async order-gateway kit (XMM
  XH4a: duplex rings, in-flight table, deadline queue, make-before-break).
  Its first user is `exec_hyperliquid::gateway` (XH4b: the HL WebSocket-
  post gateway thread — built and loopback-tested, wired to the engine in
  XH4f); `exec-hyperliquid`'s test-only feature `testvenue` is a scripted
  rustls Hyperliquid (never in a shipping build).
  `crates/exec-binance` + `crates/signer-ed25519` — the Binance arm (BX1–BX13:
  USDⓈ-M, COIN-M, spot, options, Stocks; its own gateway on the same kit;
  `docs/binance-runbook.md` holds the operator's procedures).
  `crates/exec-hypercall` — the Hypercall order arm (HC9, O-HC19): driven
  only by the operator's `hypercall-live` verbs
  (`scripts/hypercall-live.sh`; mainnet writes need `--confirm`); the
  engine refuses a live `hypercall` slot until slot 7's live-arming
  ruling (risk-policy "HYPERCALL — the order arm").
  `crates/signer-evm` + `crates/exec-hyperevm` — HYPARB's HyperEVM write
  path: configuration arms TESTNET only (`EVM_ARM_CHAIN_IDS = [998]`,
  compile-time asserted); mainnet only through a `MainnetAuthority`
  (L1: the operator's `evm-live` verbs; L5: the engine's three
  switches; `docs/hyparb-build-plan.md` §11, §16.14, §17.4, §17.5).
- `crates/cli` — `multivenue-engine` (run / audit-replay / capture-catalog /
  backtest / audit-pnl / exec-smoke …); `paper.rs` = the boot + metrics
  assembly; `exec_boot.rs` = the arming interlock.
- `crates/bench` — criterion + the alloc assertions (its `CountingAllocator`
  is process-global; keep other crates' tests out). `fuzz/` — targets.
- `claude-worker/` — the Python 3.14 worker: 8 frozen verbs + the modules
  (candles, funding, regime, pnl_report, window_root, library/compose,
  archive/objstore, dashboard, xsd_author, bin15_*, …). Integration tests
  live per crate under `tests/`.
- `scripts/` + `launchd/` — the wrapper, daily-restart, candles/regime/
  archive/retention cycles, `exec-smoke.sh`, `copy-audit.sh`.
- `.claude/` — agents (`alloc-auditor`, `zero-copy-auditor`,
  `risk-reviewer` on Opus 5.5, `parser-property-tester` on Sonnet), commands, hooks.
- `docs/arch/` — history (index in its README). `docs/research/` — the vault.
