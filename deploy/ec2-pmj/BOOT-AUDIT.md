# Boot audit: the engine with `STRATEGY=pmj` and `universe.toml.pmj`

What `multivenue-engine run --paper --strategy pmj` starts and dials when it is
launched by `pmj-wrapper.sh` with `universe.toml.pmj`. The code was read at
`df543c33` (PMJ J16) plus the working tree of 2026-10-29 ~19:30Z. Line numbers
are `crates/cli/src/bin/multivenue-engine.rs` unless another file is named.
That file is being edited concurrently (its lines moved by 21 between two
reads the same afternoon), so each citation also names its code anchor; trust
the anchor over the number. Nothing in this audit changed code. Section C
lists the changes that would be needed.

## A. Boot, in order

| step | what | where | hosts |
|---|---|---|---|
| 1 | `Config::load`: requires `POLYMARKET_CLOB_HOST`, `POLYMARKET_GAMMA_HOST`, `BINANCE_WS_HOST`, `ALCHEMY_HOST` (`env_req`) | `crates/core-config/src/lib.rs:229-291` (239-241, 256) | none |
| 2 | universe: parse, allocate, then `assert_bootable` (≥ 1 static PM market **and** ≥ 1 Binance spot symbol) | `universe_boot.rs:253`, `crates/core-config/src/universe.rs:2521-2537` | none |
| 3 | exec route table (`--exec` absent → every slot paper; no `engine_exec_*` family) | 4391 (`let exec_boot = match cli::exec_boot::resolve`) | none |
| 4 | boot discovery `run_all` | 4438 (`let discovery = match cli::boot_discovery::run_all`); `paper.rs:12832`; any `Err` is fatal: 4455 `"boot discovery failed"` → exit 1 | see below |
| 4a | HL: `POST /info` (the six coins) | `paper.rs:12306` (`run_hl`) | `api.hyperliquid.xyz` |
| 4b | Binance spot `exchangeInfo` (`btcusdt`) — **forced by step 2** | `paper.rs:12457-12480` (`run_bn`) | `api.binance.com` |
| 4c | Binance USDⓈ-M `exchangeInfo` (`bnbusdt`) | `paper.rs:12516-12545` (`run_bn`, the futures pages) | `fapi.binance.com` |
| 4d | PM static market, `GET /markets?clob_token_ids=` — **forced by step 2**. A transport, HTTP or parse failure here is **fatal at boot** (`paper.rs:11818-11826` `"pm: discovery fetch failed"` → `run_all`'s `run_pm(...)?` at `paper.rs:12987` → 4455). Only a token the venue no longer lists is "missing", a warning | `paper.rs:11800-11870` (`run_pm`) | `gamma-api.polymarket.com` |
| 4e | OKX, Deribit, Bybit, MEXC, Hypercall, the options chains | `paper.rs:12847-12990` (`run_all`) | none: no section, no call |
| 5 | DNS of the static market's socket, unconditional | 4558 (`WssEndpoint::resolve(&cfg.polymarket_clob_host`) | `ws-subscriptions-clob.polymarket.com` |
| 6 | HL WS endpoint and `/info` resolved | 4689-4742 (`let hl_boot = match`) | (DNS only) |

`discovery.any_missing` refuses only the legacy `--live` flag (4459-4468, `if discovery.any_missing`).
`--live` conflicts with `--exec`, so a token the venue no longer lists never
refuses a paper or an armed boot. It only logs. **But Gamma being unreachable
or answering an error for that token IS fatal** (step 4d), and so is any
other discovery failure: the static market costs the standalone a boot
dependency it gains nothing from. Until C1/C2 land, use the **longest-dated
active** market available. M1-R1 allows only crypto up/down markets there, so
that is the longest-dated live crypto up/down market — or Anton rules an
exception for this placeholder.

## B. Threads, and what each dials

| thread | pinned to (hard-coded core id) | started because | dials |
|---|---|---|---|
| main: the engine loop (`run_engine_loop`, `paper.rs:10329`, via `paper.rs:4522`) | **nothing** (the `pin_current_thread_to_core(0)` that `paper.rs:11110-11112` mentions does not exist) | always | — |
| `polymarket` (the static market) | 1 | **always** (5087 `let pm_handle = match spawn_polymarket`; `pm_tokens[0]` at 5078) | `wss://ws-subscriptions-clob.polymarket.com/ws/market` |
| `binance` (M1 multi lane) | 2 | always (5126 `let bn_handle = if bn_total`/5213, or the single lane at 5243-5275 with `bn_spot[0]`) | `stream.binance.com` (`btcusdt@bookTicker` + its aggTrade sentinel, **forced**); `fstream.binance.com` (`bnbusdt@bookTicker`, `/market/ws/bnbusdt@markPrice`) |
| `hyperliquid` | 7 | `[hyperliquid] coins` (5413 `spawn_hyperliquid`) | `wss://api.hyperliquid.xyz/ws`; `/info` on reconnect |
| `pm5-discovery` | none | `[polymarket] rolling` `:5m` keys (5761 `cli::spawn_pm5`; `paper.rs:1089`) | `gamma-api.polymarket.com` (`/events?slug=`), `clob.polymarket.com` (`/clob-markets/`) — `crates/ingress-polymarket/src/pm15_discovery.rs:92-94` |
| `ingress-pm5` | 13 (5773) — **absent on an 8-vCPU host**: `sched_setaffinity` fails, the thread logs and runs unpinned (`paper.rs:11441-11456`) | the same | 7 × `wss://ws-subscriptions-clob.polymarket.com/ws/market` (one per family, `rolling_market_sockets = 8`; `pm15_loop.rs:69`); `wss://ws-live-data.polymarket.com` (RTDS, Chainlink; `rtds.rs:35`) |
| `metrics-http` | none | `--metrics`, on by default (6042) | binds `127.0.0.1:9191` |
| `jup-gw` | none | **live slot 11 only** (6990 `cli::pmj_live::boot_pmj_arm` → `pmj_live.rs:1029`) | `api.jup.ag` (one keep-alive connection) |

These are not started:

- **rpc:** no `--polygon-path` (5789). `ALCHEMY_HOST` is never dialed.
- **hyperevm:** no `--hyperevm-path` (5827).
- **ingress-ai and `ai.sock`:** `AI_INGRESS_HMAC_KEY` is unset (5887-5935; `env.template` leaves it out).
- **pm15:** no `:15m` key.
- **OKX, Deribit, Bybit, MEXC, Hypercall:** no section.
- **TUI:** not started.
- **The legacy CLOB dispatcher:** it is `--live` only.

**Outside the engine, on this host:**

- **`pmj-exec --key-check`** (the daily gate and the flip): `api.mainnet-beta.solana.com` (`PMJ_SOLANA_RPC`, `pmj_exec.rs:467`) and `api.jup.ag`.
- **pmj-daily's claimable listing:** `api.jup.ag` (read only).
- **The archiver:** the S3 endpoint in `s3.env`.
- **No worker runs here.** There is no universe refresh, no recommit, no seed push and no candles.

## C. What it would take for the standalone to dial no other venue

The BIN15 standalone met "no other venue's hosts at boot" by pruning a branch.
On `main`'s binary, slot 11 still needs Polymarket (Gamma, the CLOB, the
market sockets, RTDS), Hyperliquid (six perps) and Binance USDⓈ-M (`bnbusdt`).
`pmj.toml`'s `perp` descriptors name these as the nowcasts. The other hosts
are forced. These are the changes, none of them made:

- **C1. `assert_bootable`** (`crates/core-config/src/universe.rs:2521-2537`, called at `crates/cli/src/universe_boot.rs:253`).
  - **Today:** it refuses a universe with no static PM market, or with no Binance spot symbol. That forces a `markets` token and `spot = ["btcusdt"]`.
  - **The change:** accept a universe whose only Polymarket content is `[polymarket] rolling`, and whose Binance content is futures alone. The latency-arb pair anchor it protects belongs to an unlinked member.
  - **Downstream, empty `pairs`:** `EngineConfig.pairs` (6010) feeds only the legacy arms, and `pm_tokens[0]` at 6131 is `rule-tree`'s. Both need checking for empty input.
- **C2. The static Polymarket lane is unconditional:**
  - the DNS resolve at 4558;
  - the symbol map and `pm_tokens[0].sym` at 5070-5086;
  - `spawn_polymarket` at 5087.

  It also makes the Gamma lookup of that token a fatal boot dependency (step 4d).

  **The change:** spawn it only when `boot.allocated.pm_tokens` is non-empty. Otherwise drop `pm_prod`, the unspawned-venue shape that pm15 already uses at 5692-5695 (`if pm15_keys.is_empty()`). `run_pm` (`paper.rs:11800`) already makes no request for an empty list.
- **C3. The Binance single-symbol lane** (5243-5275) indexes `bn_spot[0]`. With C1 it must be guarded for a spot-less universe. The multi lane is taken whenever any futures are configured (5126), so `usdm = ["bnbusdt"]` alone already avoids it.
- **C4. Pinning:**
  - **Today:**
    - the core ids are hard-coded (pm 1, bn 2, hl 7, pm5 13, pm15 12, hypercall 11, hyperevm 10, ai 4, rpc 3);
    - the engine loop is never pinned;
    - on an 8-vCPU host the pm5 ingress cannot be pinned.
  - **The change:** a core-map knob (a flag or an environment variable, read at boot), and a pin of the main thread before `run_engine_loop`. Until then the kit moves the main thread from outside (`pmj-pin.sh`).
- **C5. The required environment** (`crates/core-config/src/lib.rs:239-241, 256`):
  - **Today:** `BINANCE_WS_HOST` and `ALCHEMY_HOST` are required even when nothing dials them.
  - **The change:** make them optional, with defaults like the other hosts.
- **C6. The stale comment:** `paper.rs:11110-11112` says the loop is pinned to core 0 "in main". It is not (C4).

## D. Other findings the kit works around

- **C7. `.cargo/config.toml:26-30`** sets `-C target-cpu=znver3` for `x86_64-unknown-linux-gnu`, which is an AMD target. c7i is Intel (Sapphire Rapids). LLVM may then emit znver3-only instructions, SSE4a for instance, which an Intel CPU lacks.
  - **The kit:** builds with `RUSTFLAGS="-C target-cpu=native"` on the host (RUSTFLAGS replaces the config's `target.<triple>.rustflags`), reads the flags rustc actually ran with back from the verbose build log into `BUILD_INFO` (`rustflags_observed`; a `znver` there refuses the install), records the CPU model, and the wrapper refuses to start on a different model.
  - **The fix outside the kit:** change that line to `x86-64-v3`, or to `native` for host builds.
- **C8. `crates/cli/build.rs:34-35, 60-77`:** it reads `git rev-parse` and `git show HEAD:docs/risk-policy.md`. A `git archive` tree has no `.git`, so the result is `boot.git_sha = "unknown"`, and the PMJ gate flags are 0 even when the committed policy carries the dated line.
  - **The kit:** `deploy-linux.sh --with-git` ships a one-commit shallow repository, installed as `/opt/multivenue/<sha>-git`. The wrapper refuses to arm a release whose `BUILD_INFO` says `transport=archive`, and `install-release.sh` refuses to make one current while slot 11 is armed.
  - **The fix outside the kit:** an alternative would be for build.rs to accept a commit id that has been verified against `git get-tar-commit-id`. That is a change to a safety gate, so it is the operator's decision.
- **C9. `scripts/pmj-flip.sh` `live`:** it reads the dust line through `git -C <release> show HEAD:`. That needs the `--with-git` release, plus a `safe.directory` entry for `mv`. `install-release.sh` adds the entry.
- **C10. `scripts/retention.sh:68`** uses macOS `df -g`. `pmj-daily.sh` ports it with GNU `df -P -BG`. A portable form in the repo would let one script serve both hosts.
- **C11. `scripts/notify-send.sh`** defaults to `<repo>/.notify-inbox`. The release is root-owned, so the kit links that path to `~mv/multivenue/.notify-inbox`. No notifier relays from this host (R-PMJ-S75 "Not now").
