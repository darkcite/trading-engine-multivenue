# CLAUDE.md state log — archived 2026-10-13

Verbatim text lifted out of `CLAUDE.md` on 2026-10-13 by the HK housekeeping
lane (ruling HK-R2: fix the stale lines and slim CLAUDE.md, its history moved
here). Nothing here is open work. The current state was restated in CLAUDE.md
"CURRENT STATE"; where a line below disagrees with CLAUDE.md, CLAUDE.md wins.
The lane records the paragraphs cite stay in `docs/risk-policy.md` and
`docs/migration.md`.

One section follows: the CURRENT STATE bullets as they stood at `c3e38dc`
(the HK9 commit), covering the execution lane, BIN15 (R0, S7-L1, H90), MEXC,
BX0 and the zero-copy passes, HYPARB, the Hyperliquid reconnect loop,
Hypercall + HAR H1–H3, the gate line and the open operator items. One line
was incomplete in the original ("R0's record:" ended the slot-3 bullet) and
is kept as it was.

---

## CURRENT STATE (2026-09-19)

- **MVP complete; every pre-execution lane closed** (Stage 1–2, M1–M6, VM2,
  VT, regime + dashboard, S3 archive, ICDP, XSD, VRP, BIN15). Their closing
  records are in `docs/arch/` and the vault; nothing there is open work.
- **Live operation:** one launchd engine (`com.multivenue.engine`) in PAPER,
  restarted at 00:10/08:30/16:05Z by `daily-restart`, plus `caffeinate`,
  hourly `candles`, 5-min `regime`, nightly `retention` (PROTECT_DAYS 1,
  every delete gated on S3 `verify`), `archive` (04:30 local) and the
  read-only `dashboard`. The live mask is whatever `strategy.conf` names;
  slots 1–3 and 6 have all run in paper. After ANY restart verify
  `vm_rows_active ≥ 1` on `/state`.
- **Real-execution lane E1–E6 LANDED, reviewed and re-tested in E7
  (2026-09-19).** `crates/exec-router` (per-slot `ExecMode` on
  `Order.strategy_id`, `RoutedDispatcher`, the E6 risk gate: per-order /
  open-orders / day / instance caps, the venue-fill ledger, six sticky halts
  incl. recon-STALE, `exec.HALT`) + `crates/exec-hyperliquid` (msgpack +
  EIP-712 `Agent` signing pinned by 29 SDK vectors (two HIP-3 rows since HC10), mio+rustls `/exchange`
  arm with the request body rendered in place, `userFills` WS pumped from
  `on_idle` on the engine thread, reconciliation, address-budget governor,
  LAW E-8 sweeps). Armed ONLY by the two-switch interlock `--exec
  ~/multivenue/exec.toml --arm-live <slot>`; `LIVE_ARM_VENUES = [Hyperliquid, HyperEvm]` (HyperEvm on slot 0 only, L5);
  a market-data host and an exchange host on different networks refuse the
  boot. `halt_on_recon_stale_ms` is REQUIRED on every live slot. Record:
  `docs/risk-policy.md` "E6" + "E7".
- **Slot 3 (bin15) is PAPER since the R0 session bound halted it** (R0:
  6 entries, −$5.105, 2026-09-19; S7 law in paper since 2026-09-24). The
  live arm is restart-proof since S7-L1 (2026-09-24, `docs/risk-policy.md`
  "The live arm, restart-proof": SIGTERM drains, account-wide cancel at
  boot and shutdown, day cap from the venue's fills, session bound at
  cost, exits exempt from the order caps, `request_topup_*`). Arming is
  the operator's `scripts/bin15-flip.sh live` (the S7-L1 entry in
  `docs/risk-policy.md`); `bin15-flip.sh paper` disarms. R0's record:
- **BIN15 H90 — the late-window entry (τ 2–30 s, the P1b TWAP-lock pricer)
  is BUILT, PAPER ONLY, and OFF until `bin15.toml` carries `late_enabled = 1`
  with its `late_phi_lut`** (vault docs 32/33, rulings O-H90-1..10;
  risk-policy "BIN15 H90"; commits `82d3b97` L1+L2, `d3914b5` L3, `8df4515`
  L4). Its stores are `late_entries.tsv` / `late_fires.tsv`; `bin15_accrue
  report` scores doc 33 §3's bars. The live dust test is the operator's:
  `scripts/bin15-late-dust.sh --dry-run`, then `--confirm` (≤ $10, before
  the mainnet agent approval expires 2026-10-19).
- **E7 R0 was LIVE ON MAINNET 2026-09-19 13:04Z — slot 3 (bin15)
  armed** (operator ruling; testnet has no 15-minute family, so R0 ran
  there only as the exec battery, vault doc 23). The launchd engine
  boots `STRATEGY=ai+vrp+xsd+bin15` with `EXEC_TOML`/`ARM_LIVE=3` from
  `~/multivenue/strategy.conf`; `~/multivenue/exec.toml` (order $2, open
  2, day $8, instance $2) and `bin15.toml` (entry $2 at ask ≥ 0.70,
  cap_instance $2, cap_day $8, maker off) size a 9.8 USDC spot bankroll.
  The four `Scope::Live` keys live in the REPO `.env`; the mainnet agent
  approval EXPIRES 2026-10-19. The venue's HIP-4 minimum order is
  **1 USDC** (measured; `GRID_MIN_NOTIONAL_1E6`) and its fee is charged
  on SETTLEMENT (14 bps of the payout, 13.44 with the referral discount,
  measured 13:15Z; the trade is free) — `fees.toml`'s
  `prediction_settle` key, `--fee-bps hl.prediction.settle`. Record + the first hour's two findings (E7-F1 budget seed,
  E7-F2 IoC-miss classification): `docs/risk-policy.md` "E7 — MAINNET
  R0". Bars R0–R3: risk-policy "E7". Every restart of an armed engine
  passes `scripts/exec-smoke.sh` first (daily-restart does it itself).
- **MEXC, the seventh market-data venue — LANDED 2026-09-23, DATA-ONLY**
  (`docs/mexc-ingress-plan.md` MX0–MX9; ruling O-MX1: no exec arm, no
  fill lane, not tradeable). `VenueId::Mexc = 7`, tick/event lane 6,
  `crates/ingress-mexc` (spot = protobuf over WS BINARY, walked by
  `core_parse::pb`; futures = JSON; one thread, 15 spot / 13 perp
  symbols per socket), capture label `mexc`, descriptors `mexc:` /
  `mexc-perp:` (xStocks and TradFi perps are ordinary rows), in the AI
  boot universe (Q-MX6). Stale default 400 ms, Δ 150 ms, both measured;
  REST RTT 131–134 ms from here — a capture/research venue, nothing
  latency-sensitive on it. **LIVE since 2026-09-23 08:48Z** in the
  paper engine (plan §0 GO-LIVE): `[mexc]` = the Q-MX5 twelve in
  `~/multivenue/universe.toml`, `[fees.mexc]` (UNVERIFIED published
  rates) in the live `fees.toml`, and the five MEXC news sources enabled
  (operator trusted the `www.mexc.co` origin). A boot funding-SEED
  failure is an error log, never a boot refusal (that perp runs with
  Funding `v1 = 0`). Live smoke WITHOUT stopping the engine (ruling
  MX9-EXEC): `CARGO_TARGET_DIR=/tmp/mx9-target cargo test
  --release -p cli --test mexc_live_smoke -- --ignored --nocapture`
  (`MEXC_SMOKE_SECS` ≤ 900; the separate target dir leaves the
  wrapper's `target/release/multivenue-engine` untouched). Its two live
  catches: spot republishes an unchanged BBO every 10 ms (a tick is a
  BBO CHANGE — plan D11) and futures closes a busy socket 60 s after the
  last CLIENT ping (D12, pitfall 16). Wire law: `docs/wire-format.md`
  "Capture files"; rulings Q-MX1…Q-MX7: plan §0.
- **BX0 — the Binance fix-now pass, LIVE since the 16:05Z restart of
  2026-09-23 (`0a1f6ff`, verified)** — `docs/binance-exec-plan.md` §2/§5 BX0. F1: USDⓈ-M markPrice
  on fstream's ROUTED `/market/ws/` path (the legacy `/ws/` URL answers
  101 and stays silent since 2026-04-23 — the old "partial venue fault"),
  plus F1b: a dated contract's live `"r":"0.00000000","T":0` is not
  funding. F2: Binance options on `/market/stream?streams=<uly>@optionMarkPrice`
  (the nbstream `/eoptions` route 404s since the 2025-12 options
  migration — it was never this network); one push = one underlying's
  whole chain, the lane keeps its table's rows; `BINANCE_EAPI_WS_HOST`
  defaults to `fstream.binance.com`. F3: `HlExchange` finally overrides
  the trait's `cancel`/`modify` (the router got `Unsupported` before);
  inherent `modify` → `modify_by_cloid`. F4: `TCP_NODELAY` on every
  socket core-net opens. After the restart the regime FUND reference
  (`binance-usdm:btcusdt`) gets live funding prints for the first time
  (`docs/migration.md`). Live smoke WITHOUT stopping the engine:
  `CARGO_TARGET_DIR=/tmp/bx0-target cargo test --release -p cli --test
  binance_md_live_smoke -- --ignored --nocapture` (`BN_SMOKE_SECS` ≤ 900).
  The live `.env`'s `BINANCE_EAPI_WS_HOST` line was switched to
  `fstream.binance.com` BY A SESSION on the operator's explicit ask
  (2026-09-23; that one line, rewritten in place, nothing read out) — the
  one sanctioned exception, the secrets law stands. F3's arm-level test
  split is operator-accepted (the composed router test moves to BX3).
  Remaining BX0 items: plan §5 BX0 "Operator actions". Zero-copy pass
  (O-BX12d, the second BX0 commit, live from the 00:10Z restart):
  `ingress-binance` joined `make copy-audit`, which had been blind past
  any mid-file test-only method (routed.rs / exchange.rs / run_loop.rs)
  — fixed, and `scripts/copy-audit-selftest.sh` proves the reading
  first; bookTicker / markPrice parse IN PLACE (`&mut Frame` → `bool`);
  `core_net::ws_write_text_frame_parts` writes a frame from payload
  parts. Open: core-net's rustls RX copy and rustls' buffered-API
  allocation per record (MEASURED since the HYPARB merge — bench gate
  72, one per record each way; `docs/risk-policy.md`).
  In-place pass over the other seven ingress crates (okx, deribit,
  hyperliquid, mexc, bybit, polymarket, rpc; `docs/risk-policy.md` "the
  other ingress crates parse in place too"): 30 parsers fill in place,
  frames left the dispatch values, `DepthPair` replaced the top-K
  copies, the fuzz targets start from a poisoned frame. ZC pass B
  (2026-09-24, risk-policy "ZC pass B"): those seven crates and core-ring
  joined `make copy-audit` — the WS Ping echo goes rx → tx everywhere
  (the exec lane's user-WS too), fixed-shape requests are masked into tx
  from parts (`core_net::queue_masked_{text,binary}_frame_parts`),
  false-premise copies are gone, the rest marked. ZC pass A (2026-09-24,
  risk-policy "ZC pass A"): core-ring lends its slots — `try_push_ref`
  copies once into the slot, `try_pop_ref` lends it in place through a
  `Popped` guard, the by-value API is gone; per-slot `UnsafeCell`, 128 B
  index padding, `split()` once (Miri-clean, Stacked and Tree Borrows);
  the engine reads all nine lanes in place. Then (2026-09-25, risk-policy
  "core-ring caches the other side's index") each handle keeps its own
  index private and caches the other side's (Rigtorp): the M4 round trip
  170 → ~100 ns, the saturated stream 50 → 4–9 ns, same-core push+pop
  +0.5 ns; the ingress drain loops judge progress by
  `Producer::published()`. `make bench-check` compares (2026-09-25,
  risk-policy "`make bench-check` compares"): results found by
  criterion's own ids, a missing or stale one fails, the baseline re-taken
  on the M4. The I-3 drain loops (risk-policy "The I-3 drain loops read
  past a full rx") share `core_net::drain`: a step whose read filled rx is
  driven again, as one that published or moved its state; at most 8 steps
  per connection per poll, then a poll that does not sleep. Since HK2
  (2026-10-13) all ten drains share one oversize guard
  (`core_net::incomplete_frame`: a frame larger than rx fails the session),
  a `drive_one` parses what arrived with an EOF its read met before it
  closes, and the HL order gateway reads a closing socket's last answers
  (risk-policy "HK2"). Batch
  subscribes render straight into tx (2026-09-26, risk-policy "Batch
  subscribes render straight into tx"): `core_net::queue_masked_text_frame_rendered`
  counts the payload, writes the header, renders behind it and masks in
  place — OKX, Deribit, Bybit, Polymarket and MEXC dropped their stack
  scratches and the copy into tx; HyperEVM's request bodies stay copied.
- **HYPARB — slot 0, MERGED to main 2026-09-23 (H0–H9d), DARK**
  (`docs/hyparb-build-plan.md` §16; risk-policy "HYPARB — slot 0"). The
  HyperEVM AMM ↔ HL Core arb: `crates/strategy-hyparb` over `core-amm`,
  `ingress-hyperevm`, `signer-evm`, `exec-hyperevm` and
  `contracts/hyparb-executor/` (bytecode committed, reproduced by
  `scripts/hyparb-executor-repro.sh`). PAPER only and in NO configured mask:
  `strategy.conf` is untouched (O-H8), and the member boots only with
  `--hyparb ~/multivenue/hyparb.toml` plus the pool ingress (`[hyperevm]
  pools` in universe.toml, `--hyperevm-path` on `HYPEREVM_WS_HOST`).
  **Go-live PREPARED 2026-09-24 (plan §16.18):** both are in the live
  config — `~/multivenue/hyparb.toml` (`mode = "testnet"`: paper + the
  testnet shadow, 14 pools, 4 perp-hedged coins; the marked P&L LEVEL
  `/state` `hyparb.pnl_session_usd_1e6`, no paper stop since L0) and
  `[hyperevm]` in `universe.toml` — and the wrapper hands the shadow
  its testnet key from `~/multivenue/.env`; turning it on is the
  operator's four `strategy.conf` lines (§16.18 runbook).
  **LIVE-ON-MAINNET build started 2026-09-24 (plan §17, rulings
  O-HL1..O-HL5):** paper = testnet, live = HyperEVM mainnet + real HL
  hedges from slot 0's OWN wallet; the P&L stop (+$50 / −$20, combined
  equity) is LIVE-only; the reused H9d executor; real money only after
  paper's gate G1. Phases L0, L1 (done: mainnet writes only as the
  operator's `scripts/evm-live.sh` verbs behind `--confirm`, a
  `MainnetAuthority`) … L6. HZ part B is NOT green from
  this Mac (a tx fired at head N lands in N+1 ~56 %): paper AMM fills
  are an upper bound on latency. **L2–L5 LANDED (plan §17.5):** slot 0
  arms LIVE only through three switches (artifact `mode = "live"`,
  `exec.toml [exec.slot.0]` live on `["hyperliquid","hyperevm"]`,
  `--arm-live 0`) with its own arm `cli::hyparb_live::HyparbLive` behind
  `exec_router::SlotSplit`; `scripts/hyparb-flip.sh status|live|paper`
  flips it (preflight = `evm-live arm-smoke --no-trade`); the runbook is
  plan §17.6. In PAPER its one real write path is the
  HyperEVM TESTNET shadow — chain 998 only, compile-time; `mode =
  "testnet"` + `--evm-testnet` (+ `--evm-hybrid` for mainnet reads).
  **DONE(H8) live** 2026-09-23 17:37Z (plan §16.16): the executor
  `0x6c16…9cc5` on 998 is owned by wallet 0 (the HL TESTNET agent key's
  address). Operator verbs: `scripts/evm-testnet.sh` (zsh — never `sh`)
  `status|fund|deploy|mint|battery|shadow-smoke`, standalone, never
  stopping the engine. Shared-code fixes it carries, live at the next
  release build + restart: slot 3's `HlHttp` never reuses a keep-alive
  the venue closed (H9c); `TlsTransport::read`'s `WouldBlock` stopped
  allocating (3 per drain loop, every TLS socket); a wrapping chunk size
  in `http1::walk_chunks` no longer aborts the process (every venue's
  boot REST).
- **The Hyperliquid reconnect loop — fixed 2026-09-26 (`7235201`), LIVE
  since the 09:03Z restart, verified at the 09:15Z roll** (risk-policy
  "The Hyperliquid reconnect loop — dead HIP-4 instances"). The venue drops the whole socket (a bare FIN) on
  a subscribe to a settled or unknown HIP-4 coin, and `outcomeMetaUpdates`
  pushes a roll once and replays none to a later subscriber (both probed),
  so a reconnect that re-subscribed a family's settled instance died every
  ~1.2 s until a restart (15 190 times on 09-26 before the operator's
  07:18Z revive). Now a reconnect retires dead instances and re-reads
  `/info outcomeMeta` for their successors (to the address resolved at
  boot — the HL thread never runs DNS; 3 s; at most once a minute);
  staleness stops judging an instance at its expiry; the backoff resets
  only after a session that lived 30 s (`core_net::should_reset_backoff`:
  the seven outer loops, and since O-HC16 the Hypercall and MEXC internal
  reconnects — MEXC per slot); `hyperliquid: run-loop returned` names each end
  (`err_site`, `io_kind`, `venue_code`, `lived_ms`, `acks`/`acks_expected`).
  Its ~1.5/s "parse errors" were BIN15 O8's one-sided outcome `bbo` drops,
  counted on their own from the next release build + restart
  (`engine_ingress_hyperliquid_outcome_bbo_one_sided_total`; risk-policy
  "The one-sided outcome `bbo` is not a parse error"). HK1 (2026-10-13)
  closed its two open items: `roll_health`'s retry runs under a fresh
  deadline, so the second strike is reachable (it stops the asking; the
  instance stays bound, never marked dormant), and a roll's unsubscribe
  echoes count in `engine_ingress_hyperliquid_unsub_acks_total`, not as
  parse errors (risk-policy "HK1").
- **HYPERCALL — the eighth market-data venue, MERGED to main 2026-09-26
  (branch `hypercall`: HC0–HC7, HAR H1–H3, O-HC8), LIVE since the 20:15Z
  restart of 2026-09-26 — the data, the HAR set and slot 7 in PAPER** (vault `docs/research/hypercall/hypercall-integration-plan-2026-09-26.md`,
  rulings O-HC1..O-HC10: no keys, no exec arm; HC9–HC11 each a separate
  ruling after the research's R1 gate — HC8, the sign-only signer, came
  forward under O-HC17: `signer_eip712::hypercall`, every SDK type, 38
  SDK vectors byte-exact, gate 83; HC9 — the order arm
  `crates/exec-hypercall` — came forward under O-HC19 as the operator's
  `hypercall-live` verbs, never armed by the engine — its mainnet dust
  PASSED 2026-09-26 after the nonce fix `84fc32b` (the nonce is wall ms);
  HC10 bound the HIP-3 hedge perps; HC11 built slot 7, `strategy-hcv`,
  DARK paper, and HC11b made its book persist across restarts. **The
  go-live** (rulings O-HC25..O-HC29; runbook and log: vault
  `docs/research/hypercall/hypercall-golive-2026-09-26.md`) put into
  `~/multivenue`: `[hypercall]` (12 underlyings, 576 instruments), the
  `fees.toml` hypercall lines, the 8 `xyz:` hedge coins (HL coin table
  32/32), 7 TradFi USDM feeds, the W1 block, `har.toml` + 12 warm
  seeds, `hcv.toml` and `STRATEGY=…+hcv`; `hypercall_history` rides
  `candles-cycle.sh` hourly (`0a7303a`; its payouts lane waits for
  `CLAUDE_WORKER_HC_WALLETS`); `[events]` is live since 2026-09-26
  12:03Z, O-HC14. Slot 7's book restore is proven live (the 21:15Z
  restart, 4 positions). OPEN — plan HC11c, the operator's ruling: at
  the switch-on slot 7 spent its $50 premium cap in ~40 s on weekend
  quotes 11–19 vol points wide — it buys at the ask and sells at the
  bid against σ̂ but marks at the mid, so the $20 day stop tripped on
  the spread; that stop is a level, not a latch; σ̂ ignores the HAR
  set's weekday profile).
  `VenueId::Hypercall = 9`, tick
  lane 7, `crates/ingress-hypercall` (one public WS: the one-frame
  indicative subscribe, quotes kept crossed or one-sided as published,
  per-provider sides on `ChannelId::ProviderQuote` = 14, index `Mark`s
  on `hypercall-idx:<U>`; a REST `/options-summary` poller thread over
  an SPSC handoff; the I-3 drain, `core_net::drain`, since the merge),
  capture label `hypercall`, `/state` ingress row 10,
  `[hypercall]` in universe.toml (all 12 underlyings, E 3 × K 8 × {C,P}
  ≤ 1 024 instruments); its options are in the AI boot universe (O-HC17:
  signal and reference legs, orders unroutable; the index syms stay out).
  `crates/core-settle`
  replicates the venue's settlement (median-of-means over a 1 s
  sample-and-hold grid, both bucket orders until the HC7 gate picks one;
  bench gate 80); worker lanes `hypercall_history` (the research store)
  and `hypercall_settle` (the shadow). Fees `[fees.hypercall] option =
  "0:0"` UNVERIFIED (O-HC7). Live smoke WITHOUT stopping the engine:
  `crates/cli/tests/hypercall_live_smoke.rs` (`#[ignore]`, `HC_SMOKE_SECS`
  ≤ 900). The merge carries **HAR H1–H3** — `core_vol::LongVolEngine`
  (whole-day tenors 1–40 d, the empty-day law, the lifted
  `ols_fit_1e9`; bench gate 81; worker mirror `vol_ref.LongVolEngine`
  pinned by `tests/fixtures/vol/long-{1,2}.*`; `claude_worker.har_seed`),
  owned since H3 by the ENGINE: `core_vol::LongVolSet` (≤ 12 series,
  held by `StrategySet` in the regime's seat on its own 1 s timer, day
  closes staggered one a poll — ~38 µs each on the M4; bench gate 82; slot 7 reads its forecasts, O-HC22) from
  `~/multivenue/har.toml` (`core_config::har`; `--har`/`--har-dir`; NOT
  passed by the wrapper — an absent default file is the pre-H3 engine),
  restored at boot from `~/multivenue/har/seed-<NAME>.tsv` merged with the
  engine's own `state-<NAME>.tsv` (`core_vol::merge_rows`; written OFF the
  engine thread since H3.7 — each close copies the engine into a
  `core_ring::Mailbox`, the `har-state-writer` thread renders and fsyncs,
  `cli::har_writer`), published as
  `/state.har` + `engine_har_*`; the worker's `har_backfill` (every source
  kept live, `--import-from`) and `har_seed` ride `candles-cycle.sh`
  hourly when `har.toml` exists (`har/drift.json`: the dashboard's drift
  alert) — and the **scheduled-events feed** (O-HC8:
  `news.toml [events]` → `scheduled-events.json` every news cycle; the
  reader `claude_worker.news.scheduled`). `core_regime::math::isqrt_i128(2)`
  is now 1, the floor root (it returned 2).
- **Gates at HEAD (branch `hypercall`: HC11b, after HC8–HC11 on B1–B5 of its
  2026-09-26 plan; Foundry as of the HYPARB merge):**
  nextest 3583 (6
  skipped — the `#[ignore]`d `mexc_live_smoke`, `binance_md_live_smoke`,
  `hyperevm_live_smoke` and `hypercall_live_smoke` among them) · alloc
  126/126 at the gates' pins (0 B/op; BIN15 H90's late window 52b, the
  Binance arm's 94/94b (BX12),
  108/109 (BX8), 110/111 (BX9) and 112/113 (BX10) among them; gate 85
  is slot 7 over the
  held-quote law, its book handed to the writer's mailbox inside the
  measured loop; gates 72, 77b and 101 pin `HttpsPost`, `HttpsReq`
  and `HlHttp` (XMM HD2) at exactly 2 per request — rustls; +4 ignored
  child helpers) · clippy clean · `make license-check` OK · `make
  bench-check` OK at the merge (the M4 baseline, 2026-09-25: all 11
  samples within 15 % at load ~10; `vol/long_day_close_warm`,
  `vol/long_state_copy_warm` and `signer/hypercall_place_order` are
  measured, not baselined) · `make
  copy-audit` new=0 (self-test OK; 28 baselined over the exec lane,
  core-net, core-ring, all ten ingress crates, the HYPARB crates,
  strategy-xmm, strategy-hcv, core-settle, core-fill, the engine and the
  state writers — `har_writer` and the generic `persist` thread) ·
  Foundry 11 unit +
  5 mainnet-fork (session sandbox; forge is not on this Mac) ·
  worker pytest 1643 passed, 5 skipped (its one red,
  `test_news_lanes::test_report_prints_the_funnel`, is a date time-bomb —
  its fixture fell out of the 24 h window) · fuzz
  (poisoned start) `okx_frame`, `deribit_{jsonrpc_frame,option_ticker,vol_index}`,
  `hl_{ws_frame,l2book,outcome_spec}`, `mexc_ws_frame`, `bybit_ws_frame`,
  `polymarket_clob_frame`, `rpc_{response,subscribe_envelope}`,
  `binance_{book_ticker,mark_price}` 60 s each clean; earlier: `hl_*` 3 × 300 s,
  `pb_scan`, `mexc_instruments`, `binance_eapi_mark_array` 300 s,
  `binance_eapi`, `binance_exchange_info` 120 s clean; HYPARB (pre-merge)
  `amm_tick_walk`, `amm_map_payload`, `hyperevm_decode`, `evm_rlp` 240 s,
  `evm_exec_response` 540 s, `http1_response` 300 s clean; every fuzz bin
  checks at the merge; ZC pass B re-ran `deribit_vol_index` 300 s,
  `rpc_subscribe_envelope` 120 s and the seven ingress frame targets 60 s;
  ZC pass A: `cargo +nightly fuzz build` OK, Miri on core-ring clean
  (Stacked and Tree Borrows, `-Zmiri-many-seeds=0..16`; again for the
  H3.7 mailbox, whose invariance a `compile_fail` doctest pins — nextest
  runs no doctests); the Hypercall
  merge: every fuzz bin builds, `hypercall_{ws_frame,markets,summary}` and
  `http1_response` 60 s each clean (also at HC3)
  · live smokes (2026-09-26): Binance 60 s (the header-first subscribe
  renders); MEXC 60 s and Hypercall 90 s at B2 (13:24–13:26Z, 992 and
  1 558 ticks) — 0 parse errors, 0 reconnects, 0 ring drops each
  (LuLu on this Mac blocks a freshly built binary's outbound connections
  until the operator allows it — a smoke's boot-REST `Timeout` is LuLu,
  not the code). Known
  isolation-disproven flakes: `ai_exec_on_ai_is_zero_alloc`,
  `hl_outcome_meta_parsers_are_zero_alloc` and
  `settlement_window_and_median_of_means_are_zero_alloc` (debug profile),
  `scrape_hammer_all_succeed_without_conn_errors`,
  `hl_userws_loopback::a_frame_larger_than_the_buffer_is_refused_not_grown`
  (red under parallel load, green alone), `ws_frame_roundtrip_is_zero_alloc`,
  `guard_reports_zero_when_nothing_allocates` and
  `xmm_queue_law_place_land_fill_modify_cancel_are_zero_alloc` (bench,
  debug profile, under full-workspace nextest load), the worker's UDS-fixture
  family (`test_recommit…`, `test_commit_ruleset_happy_by_hash_then_by_file`)
  — rerun in isolation before believing a red. `make py-lint` (ruff) is
  RED at HEAD and has been for weeks; `make lint` means clippy.
- **Open operator items (non-gating):** S3 bucket versioning +
  `AbortIncompleteMultipartUpload` not set; the `s3.env` credential pair was
  once pasted in chat — rotate; disk headroom on the Data volume is the
  operator's lever (writers do not retry after ENOSPC — restart is the
  recovery); whole-root `audit-pnl`/`backtest` OOMs above ~27 GB — use
  bounded ≤ 2 h window roots; `regime.toml
  [labels] require = 1` is NOT flipped live; `.claude/settings.json` still
  names `claude-opus-4-6` as the session model (the three review agents are
  pinned to `claude-opus-5-5` — ruling O-8, 2026-09-23).
- **Reminder — upgrade to rustls 0.24 once 0.24.0 is released** (operator,
  2026-09-26: the only rustls work we need). 0.24 decrypts records in
  place — the fix for core-net's rustls RX copy and allocation per record
  (gate 72). Today: pinned 0.23.38, only `0.24.0-dev.*` published;
  `cargo info rustls@0.24.0` answers "could not find" until it is out.
