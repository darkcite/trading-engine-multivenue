# Multivenue Trading Engine

Pure-Rust, zero-allocation, zero-copy, single-writer, lock-free HFT engine
that executes latency-arbitrage trades on Polymarket's CLOB.

**v1 runs locally on a MacBook Pro M4.** Phase 7 migrates to a plain Linux
EC2 box — no cloud services, no observability stack, no IaC.

## Start here

- [PLAN.md](./PLAN.md) — authoritative architecture, phased roadmap, testing strategy.
- [CLAUDE.md](./CLAUDE.md) — context front-loaded for Claude sessions.
- [AGENTS.md](./AGENTS.md) — tool-agnostic brief for any AI coding agent.
- [docs/](./docs/) — wire format, risk policy, local setup, migration notes, live phase docs (8h). Closed historical plans/logs live in [docs/arch/](./docs/arch/).

## Quick start

```sh
# 1. Secrets
cp .env.example .env && chmod 600 .env
$EDITOR .env

# 2. Build
cargo build --release --workspace

# 3. Run tests
cargo nextest run --workspace
cargo test -p bench --test alloc_assertions --release -- --test-threads=1   # must show 0 B/op (or `make alloc-assert`)

# 4. Paper-mode run (Phase 8: multivenue — supply per-venue symbols)
cargo run --release -p cli -- run --paper --env-file ./.env \
  --polymarket-asset-id <CLOB token id> \
  --okx-symbols BTC-USDT,BTC-USDT-SWAP \
  --deribit-symbols BTC-PERPETUAL \
  --hl-coins BTC,ETH

# 5. Audit a capture run (written to <MULTIVENUE_LOG_DIR>/run-<ns>/)
cargo run --release -p cli -- audit-replay --dir ~/multivenue/logs/run-<ns>
```

Every ingress thread writes PMLR replay capture (per-venue tick /
event / signal logs + optional `--raw-tap` payload tap) into a
per-run directory under `MULTIVENUE_LOG_DIR`; `audit-replay` turns a
run into per-symbol rates, cadence-band checks, integrity
re-derivations and a venue × channel coverage matrix.

`config.example.toml` ships a non-authoritative example of the
operational knobs (strategies, symbol pairs, thresholds). The
binary itself is driven by env vars + CLI flags — the TOML is for
operator reference only.

`make help` lists the full set of developer targets.

## Layout

```
crates/
  core-*/           OS-agnostic primitives (rings, time, config, parse, ...)
  ingress-*/        External source adapters (polymarket, binance, okx, deribit, hyperliquid, rpc)
  strategy-*/       Strategies implementing `strategy-core::Strategy`
  signer-eip712/    EIP-712 signer (secp256k1 + tiny-keccak, no ethers)
  clob-dispatcher/  Persistent HTTP/2 client w/ preallocated buffers
  engine/           Engine<S: Strategy>
  cli/              Main binary
  tui/              ratatui dashboard (read-only)
  bench/            criterion + dhat + alloc assertions
fuzz/               cargo-fuzz targets (integration tests are per-crate under each crate's tests/)
claude-worker/      Python 3.14, offline Claude researcher (no hot path)
.claude/            Subagents, slash commands, settings
docs/               Live docs (arch/ = closed historical plans/logs)
```

## Hard rules

1. **Zero runtime allocations in hot paths.** Enforced by `cargo test --test alloc_assertions`.
2. **No `tokio`, `serde_json`, `ethers`, `alloy`, `reqwest`, `async-std` on hot paths.**
3. **No `dyn Trait` in hot paths.** Monomorphization only.
4. **No `from x import y` in Python files.** Full `import x` only.
5. **Secrets = a single `.env` file (chmod 600, git-ignored).** No Keychain, no KMS.
6. **No cloud services at any phase.** Plain VMs only.
7. **No observability stack.** TUI + log files + loopback `/metrics` only.

See [CLAUDE.md](./CLAUDE.md) for the complete list and the "stop if you're
about to do this" list of common pitfalls.

## License

Private. All rights reserved.
