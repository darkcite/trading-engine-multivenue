# Current state — the engine, its lanes and the open items

Moved out of `CLAUDE.md` on 2026-10-17: that file now carries the laws and the
rules only. This file is the RECORD; `/state` on `127.0.0.1:9191` is the
TRUTH — when they disagree, `/state` wins and this file is fixed. Rewrite the
affected paragraph at each lane close; the superseded text goes to
`docs/arch/claude-md-state-log-*.md`, as before.

## Corrections found 2026-10-17 (they supersede the dated text below — verify on `/state`)

- **Slot 3 (bin15) is LIVE** at the $2 / $5 dust frame since 2026-10-16, on
  its own WebSocket gateway arm (`scripts/bin15-flip.sh live --gateway`;
  operator ruling O-GL-3; risk-policy "BIN15 H90 G3–G4"). The member trades
  the H90 late entry only (G3: the coverage entry, Arm A, Arm B and the null
  arm are removed). **The agent's approval expires 2026-10-19 ~13:54Z.**
  Disarm: `scripts/bin15-flip.sh paper`, or `echo 3 >> ~/multivenue/exec.HALT`
  for an immediate halt.
- **Slot 9 (pm15, `strategy-pm15`) is built and runs PAPER in main's engine**
  since 2026-10-17 09:22Z (risk-policy "PM15 — slot 9"; rulings R-PM-1..10).
  `BUILT_MASK` = 0x02FF; `ALL_MASK` (= `all`) = 0x00FF, so `all` does not
  include pm15. Slots 8 and 10–15 stay reserved.

## Corrections found 2026-10-18 (they supersede the dated text below — verify on `/state`)

- **Slot 11 (pmj, `strategy-pmj`) is built and runs PAPER in main's engine**
  since 2026-10-18 13:06Z (`88f70509`; risk-policy "PMJ — slot 11", ### J1b–J4;
  rulings R-PMJ-0..3, R-PMJ-S1..S14). It trades slot 9's H90 law on
  Polymarket's **5-minute** up/down families only, addressed to Jupiter Predict
  (`VenueId::Jupiter` = 10), judged by the paper held-quote law at submit +
  2 s; a live slot 11 refuses the boot (no `PMJ-DUST-GATE` line yet), and
  nothing of the Jupiter arm (`exec-jupiter`) is built at boot.
  `BUILT_MASK` = **0x0AFF**; `ALL_MASK` stays 0x00FF. Slots 8, 10 and 12–15
  stay reserved.
- `~/multivenue/universe.toml` `[polymarket] rolling` gained the seven
  `updown:<asset>:5m` families (btc eth sol xrp hype doge bnb), appended after
  the 15-minute ones (R-PMJ-S12); `~/multivenue/pmj.toml` (sha256
  `d82576a2…52e0`, the dust frame: $5 an order, $10 a market, −$50 a session;
  the H90 values are pm15's, provisional) is read because `STRATEGY` names
  `pmj` (`…+pm15+pmj`). `scripts/pmj-flip.sh status` shows what the next boot
  runs for slot 11.
- The boot at 13:06Z also shipped `a946a668` (SETUP docs). Rollback kit:
  `~/multivenue/rollback-pmj-20261018/` (the pre-deploy `universe.toml`,
  `strategy.conf`, the `eee660b` binary and its wrapper).
- **2026-10-19 04:54Z: the engine boots `b5c263e6` (PMJ J6, ruling
  R-PMJ-S15):** the rolling legs' depth capture (`pm15-depth.pmlr`,
  `pm5-depth.pmlr`) keeps at most one record per leg per second
  (`[polymarket] rolling_depth_ms`, default 1000; the universe names no key,
  so the default is in force — boot log `depth=RollingDepth { on: true,
  min_ms: 1000 }`). Runs before this boot are every-change (~1.35 GB/h for
  the two); after it, ~8 MB/h each. Rollback kit for this boot:
  `~/multivenue/rollback-pmj-20261019/multivenue-engine.88f70509`.
- **2026-10-19 06:19Z: the engine boots `4690bcac` (PMJ J7, rulings
  R-PMJ-S17..S20):** each rolling pipeline spreads its families over one
  market socket per family (`[polymarket] rolling_market_sockets`, default 8;
  the universe names no key, so the default is in force — boot log `pm15: the
  families ride 7 market socket(s)` / `pm5: …`; gauges
  `engine_ingress_pm15_market_sockets_open` = `engine_ingress_pm5_market_sockets_open`
  = 7). A market-socket loss now withdraws only that family's touches. Runs
  before this boot rode one market socket per period. Restart: the `0010`
  stamp forced at 06:17:38Z (off the 15-minute grid), daily-restart's exec
  gate, drain and relaunch; masks 2815, `vm rows_active` 2, slots 3 / 9 / 11
  configured and enabled. Rollback: `rolling_market_sockets = 1` in
  `universe.toml` and a restart (this binary), or the kit
  `~/multivenue/rollback-pmj-20261019/multivenue-engine.b5c263e6` (then remove
  the key if it was added).
- **2026-10-19 08:35Z: the scheduled `0833` boot (`4690bcac`) took the new
  paper frames (rulings R-PMJ-S21, S22):** `pm15.toml` session loss −$3 500
  (per order and per market stay $250); `pmj.toml` $250 deposit (max
  $250.01), $250 per market, −$3 500 session. Boot tells `frame=$250/order:
  $250/market:-$3500/session` for both. By R-PM-8 −$3 500 is also slot 9's
  live session bound once armed; today slot 9 has no arm. Rollback:
  `~/multivenue/rollback-pmj-20261019/{pm15,pmj}.toml.pre-frame-0710`.
- **2026-10-19 08:49Z: the engine boots `ed4e14bd` (PMJ J8, rulings
  R-PMJ-S23..S26):** the rolling legs' paper IoC sweeps the touch and the
  depth behind it up to its limit (the venue's FAK) at submit + delta; slot 9
  decides no entry under `price_floor` 0.90 (`pm15.toml price_floor_1e6`,
  absent = 0.90; boot tell `…:floor0.900:…`); slot 11's IoCs are also judged
  in shadow at 350 ms and 1 s (`engine_paper_matcher_pmj_*`). **Slot 9 and
  11 paper P&L before and after this boot are different laws; P0's count is
  segmented here.** Restart: the `0010` stamp forced at 08:47:21Z (14 min
  after the `0833` drain, off the 15-minute grid), daily-restart's exec gate,
  drain and relaunch; boot 08:49:18Z, masks 2815, `vm rows_active` 2, slots
  3 / 9 / 11 configured and enabled, `market_sockets_open` 7 / 7. Rollback:
  `price_floor_1e6 = 0` in `pm15.toml` and a restart restores slot 9's
  frozen law (this binary; the sweep has no switch), or the kit
  `~/multivenue/rollback-pmj-20261019/multivenue-engine.4690bcac` (then
  remove the key if it was added).
- **2026-10-19 09:37Z: PMJ J9 installed (`e4f6b566`, rulings
  R-PMJ-S27..S30):** launchd `com.multivenue.pm-pnl` (:06, :21, :36, :51
  and at load) keeps slots 9 and 11's paper P&L since 2026-10-19 00:11Z in
  `~/multivenue/worker/pm/` (ledger, open fills, summary), settled on the
  venue's result; the dashboard (`com.multivenue.dashboard` restarted)
  shows it per fill-law segment in the pm15 and pmj blocks. The engine was
  not touched (same pid, `ed4e14bd`). Remove:
  `launchctl bootout gui/$UID/com.multivenue.pm-pnl`.
- **2026-10-19 11:28Z: `target/release` rebuilt at `dda68846` (PMJ J10, ruling
  R-PMJ-S42) — no restart.** J10 changes `audit-pnl` only (the nightly day
  report settles Polymarket's rolling windows, with their p-dependent fee);
  the engine's runtime code is unchanged. The running engine stays on
  `ed4e14bd` until the next scheduled restart, which boots `dda68846`;
  tonight's 00:20Z `pnl_report` already runs it. Rollback kit:
  `~/multivenue/rollback-pmj-20261019/multivenue-engine.ed4e14bd`.
- **2026-10-19 16:06Z: the scheduled `1605` restart booted `dda68846`** (run
  `run-1792425969573319000`) with PMJ J11's config (R-PMJ-S47 / S55):
  `pmj.toml` `paper_delay_ms` 5000 and `min_tau_s` 6 — slot 11's tell
  `law=6-20s … paper=+5000ms`, artifact hash `09e53291…`; slot 9's tell is
  unchanged (`law=2-20s … floor0.900`, `ef485100…`). `/state`: masks 2815,
  `vm rows_active` 2, `market_sockets_open` 7 / 7 on both periods, depth drops
  0; `exec.HALT` still present with slot 3 `ws-gap` (unchanged). No run booted
  between the `pmj.toml` edit (14:07:54Z) and J9's `J11_FROM_NS` (16:00Z), so
  slot 11's `j11` ledger segment starts exactly at this run. Rollback: restore
  `~/multivenue/rollback-pmj-20261019/pmj.toml.pre-j11` and restart (plus a J9
  `SEGMENTS` row).
- `exec.HALT` is present and slot 3 reads halted (`ws-gap`) on `/state` both
  BEFORE and after this restart — not changed by the PMJ deploy; the
  operator's to clear.
- The binary, boot time, mask and `exec.configured` lines below predate both:
  read `/state` `boot.git_sha` and its `exec` block.

## Corrections found 2026-10-20 (they supersede the dated text above — verify on `/state`)

- **2026-10-20 05:50Z: the engine boots `9a57e345` (EXEC O-WSG1..4, operator
  rulings 2026-10-20; risk-policy "O-WSG1..4").**
  - **The hold.** A user-stream gap and a stale reconciliation HOLD a live
    slot instead of halting it. The slot places nothing and its resting
    orders are asked back, but no `exec.HALT` is written.
  - **The resume.** The slot resumes by itself once the gap is back under
    the threshold, a reconciliation from reads issued since then has agreed
    (the gateway arm's `resync_pending`), and the hold's cancel is
    confirmed.
  - **Slot 3's gap threshold** is `halt_on_ws_gap_ms = 5000` in
    `~/multivenue/exec.toml` (30000 before; backup `exec.toml.bak-20261020T054906Z-wsg`).
  - **Boot tell:** `slot 3 HALTS … ws_gap_ms=5000`.
- **Slot 3's 10-18 05:15Z `ws-gap` latch is archived.** The file is now
  `~/multivenue/logs/exec.HALT.20261020T054906Z-wsg-archived`, and slot 3
  booted unhalted and LIVE at the $2 / $5 frame (`/state`: `halted` none,
  `held` none, seeded 1).
- **The restart:** the `0010` stamp forced at 05:49:06Z (a safe minute),
  then daily-restart's exec gate (PASS), the drain and the relaunch.
- **Watch:**
  - `/metrics`: `engine_exec_held_mask` (8 = slot 3 held),
    `engine_exec_holds_total` and `engine_exec_refused_held_total`;
  - `/state` `exec.held`;
  - the log: "exec: slot HELD …" and "exec: slot RESUMED …".
- **Rollback kit:** `~/multivenue/rollback-wsg-20261020/`, holding the
  dda68846 binary, `exec.toml.pre-wsg` and `exec.HALT.pre-wsg`.

## Slots and boot (moved verbatim from CLAUDE.md "What this is", 2026-10-16)

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that
executes systematic strategies across a multivenue universe — Binance
spot/USDM/COIN-M, OKX, Deribit, Hyperliquid (incl. HIP-4 outcome markets), Bybit,
MEXC (data-only), Hypercall options (slot 7's S1 member trades them in
PAPER since the 2026-09-26 go-live; the HC9 order arm runs as operator
verbs only),
Polymarket CLOB, Polygon RPC, plus a boot-selected options ladder.
Strategies are composed at boot from a 16-slot set (0–7 built, 8–15
reserved; S16) and may trade
**any subset** of that universe; **Polymarket is one venue among several, not
the target**. v1 runs on a MacBook Pro M4 on free-tier APIs. Claude (via the
`claude-worker` Python process, and in-session) is an **offline strategy
researcher** — never in the hot path.

Slots (`crates/strategy-set`, one `member_table!` row and one `SlotMask`
(u16) bit each; 16 since S16, 2026-10-15; `all` = `BUILT_MASK` 0x00FF = 255,
the enable mask today; 8–15 RESERVED, no member — arming one is refused at
boot, adding one is its own ruling; risk-policy "S16"):
0 hyparb (HyperEVM ↔ HL Core arb, since HYPARB H0 2026-09-23 — lands DARK,
O-H8; boots only with `~/multivenue/hyparb.toml` + the pool ingress, H5;
`latency-arb` unlinked, its name refuses the boot) · 1 vrp ·
2 xsd · 3 bin15 · 4 ai-exec (AI door 1, intents) · 5 ruleset VM (AI door 2,
tables) · 6 xmm (HL post-only maker, since XMM XH1 2026-09-26; quotes on the
queue law since XH2, paper only; `/state` `xmm` block, `engine_xmm_*`,
`[labels.xmm]` and audit-pnl prints since XH3; boots only with
`~/multivenue/xmm.toml`; `icdp` unlinked, its name refuses the boot,
`backtest --member icdp` stays) · 7 hcv (Hypercall S1 options vs the HAR
σ̂, hedged on HL perps, since HC11 2026-09-26 — PAPER only on the
held-quote law; in the live mask since the go-live, 2026-09-26 20:15Z
(O-HC25; it landed DARK, O-HC18); boots only with
`~/multivenue/hcv.toml`; a live slot 7 refuses the boot; its book persists
in `hcv-state.tsv` by contract since HC11b, written off the engine thread;
risk-policy "HYPERCALL — slot 7"). Member timers run per
slot (each on its own period). The engine boots the `STRATEGY` named in
`~/multivenue/strategy.conf` through `scripts/engine-wrapper.sh`: `+`-joined
member names or `ai` (= ai-exec+vm, 48), any order, parsed once by
`strategy_set::parse_strategy`; `multivenue-engine strategy-mask <STRATEGY>`
prints `<mask>\t<members>` or exits 78. Since the S16 deploy (2026-10-15)
the wrapper resolves `STRATEGY` through `strategy-mask` (any named combination; `all`
refused) instead of its allow-list; a binary rollback rolls the wrapper back
too.

## The record as of 2026-10-16 (moved verbatim from CLAUDE.md "CURRENT STATE")

Each lane's merge and go-live paragraphs, as this section carried them until
2026-10-13, are in `docs/arch/claude-md-state-log-2026-10-13.md`, verbatim.
The lane records stay where they always were: `docs/risk-policy.md` and
`docs/migration.md`, under the names cited below.

- **Every pre-execution lane is closed** (Stage 1–2, M1–M6, VM2, VT, regime +
  dashboard, S3 archive, ICDP, XSD, VRP, BIN15). Their records are in
  `docs/arch/` and the vault; nothing there is open work.
- **Live operation.**
  - One launchd engine (`com.multivenue.engine`) in PAPER, restarted at
    00:10/08:33/16:05/20:15/21:15Z by `daily-restart`.
  - Beside it: `caffeinate`, hourly `candles`, 5-min `regime`, nightly
    `retention` (PROTECT_DAYS 1, every delete gated on S3 `verify`),
    `archive` (04:30 local) and the read-only `dashboard`.
  - It runs the release build of HK, part 6 (`8c15fdc`, booted
    2026-10-16 07:17Z; `/state` `boot.git_sha` names the commit) with the mask
    `strategy.conf` names (`ai+vrp+xsd+bin15+hyparb+xmm+hcv`).
    `strategy.conf` also exports `BYBIT_REST_HOST=api.bytick.com`
    (HK17) and `RAW_TAP=pm,hl` (rejects only, HK-R23). `/state`
    `exec.configured` is 0 (no
    route table): nothing is armed.
  - After ANY restart, verify `vm_rows_active ≥ 1` on `/state`.
  - The worker runs from the working tree, so a worker edit is live at its
    lane's next cycle.
- **The execution lane (E1–E7; risk-policy "E6", "E7").**
  - `crates/exec-router`: per-slot `ExecMode` on `Order.strategy_id`;
    `RoutedDispatcher`; the E6 risk gate (per-order / open-orders / day /
    instance caps, the venue-fill ledger, the sticky halts, `exec.HALT`).
  - `crates/exec-hyperliquid`: msgpack + EIP-712 `Agent` signing pinned by
    SDK vectors; the `/exchange` arm; `userFills`; reconciliation; the
    address-budget governor; LAW E-8 sweeps. Its WebSocket order gateway is
    `exec_hyperliquid::gateway` (XMM XH4).
  - Armed ONLY by the two-switch interlock `--exec ~/multivenue/exec.toml
    --arm-live <slot>`. `LIVE_ARM_VENUES = [Hyperliquid, HyperEvm, Binance]`
    (HyperEvm on slot 0 only; Binance for at most one live slot, BX6).
  - The `Scope::Live` keys are in the REPO `.env`; HYPARB's testnet key is in
    `~/multivenue/.env` (the wrapper hands it over).
  - A market-data host and an exchange host on different networks refuse
    the boot. `halt_on_recon_stale_ms` is REQUIRED on every live slot.
  - Every restart of an armed engine passes `scripts/exec-smoke.sh` first
    (daily-restart does it itself).
- **Slot 3 (bin15) is PAPER.** The R0 session (mainnet, 2026-09-19: 6
  entries, −$5.105; risk-policy "E7 — MAINNET R0") ended on its session
  bound.
  - The live arm is restart-proof since S7-L1 (risk-policy "The live arm,
    restart-proof"). Arming is the operator's `scripts/bin15-flip.sh live`
    (`--gateway` for the WebSocket arm, XH4i); `bin15-flip.sh paper`
    disarms.
  - The venue's HIP-4 minimum order is **1 USDC**, and its fee is charged
    on SETTLEMENT (14 bps of the payout; `fees.toml`'s `prediction_settle`,
    harness `--fee-bps hl.prediction.settle`).
  - **The mainnet agent approval EXPIRES 2026-10-19.**
  - **BIN15 H90, the late-window entry** (τ 2–30 s; risk-policy "BIN15
    H90"): ON in PAPER since 2026-10-13 11:22Z (`bin15.toml`
    `late_enabled = 1`; the boot line reads
    `late=2-30s:p>=0.90:ask<=p-2c:persist2:$100:k1.550`). Not live.
  - The live dust test (`scripts/bin15-late-dust.sh`) PASSED twice on
    mainnet, 2026-10-13 and 2026-10-14: 8/8 IoCs filled at the ask and
    settled at 1 (vault doc 33 §9 / §9b).
  - `bin15_accrue report` scores doc 33 §3's bars from
    `late_entries.tsv` / `late_fires.tsv`. Doc 34 (OOS2) runs on or after
    2026-10-19.
- **Slot 0 (HYPARB; `docs/hyparb-build-plan.md` §16–§17, risk-policy
  "HYPARB — slot 0").**
  - The HyperEVM AMM ↔ HL Core arb, in PAPER with the TESTNET shadow (chain
    998 only, compile-time).
  - LIVE only through three switches (`scripts/hyparb-flip.sh`), on slot 0's
    own wallet. Real money only after paper's gate G1.
  - G1 is tallied by `claude_worker.hyparb_g1` (HK26): each closed session
    folded under the member's own P&L law into
    `~/multivenue/worker/hyparb/g1-sessions.tsv` nightly (0020 slot), the
    2-block replay into `g1-replay.tsv`; `hyparb_g1 status` gives the
    verdict. 2026-10-15: NOT PASSED — 833 judged AMM fills, Σ −$59.46 over
    114 sessions (fees and gas exceed the edge). The nightly report's
    slot-0 row is hedge-only (audit-pnl has no AMM judge): not G1.
  - HZ part B is not green from this Mac, so paper AMM fills are an upper
    bound on latency.
  - The operator's verbs: `scripts/evm-testnet.sh` (zsh — never `sh`) and
    `scripts/evm-live.sh … --confirm`. The testnet executor is `0x6c16…9cc5`
    on 998. The live-only P&L stop is +$50 / −$20. forge is not on this Mac.
- **Slot 6 (XMM, the HL post-only maker).** Paper, on the queue law.
  - Its gateway arm is wired (XH4f), and slot 3 can ride a gateway too
    (XH4i). One Hyperliquid gateway arm per engine (A-S7-85).
  - Nothing is armed on mainnet: no `HYPERLIQUID_XMM_*` keys exist.
  - XH4i-8, the outcome battery (`exec-smoke --hl-outcome --lifecycle
    --outcome <id>`, exit codes 40–48; testnet only, nothing armed), merged
    from `xmm` on 2026-10-14 (the operator's word; risk-policy "XH4i-8"); its
    loopback `run()` test is HK15.
  - A-S7-81 (Q5) is fixed in the shared binder (HK13): a repeat of the live
    roll leaves the legs alone on both arms. Left before BIN15 rides the
    gateway live: the operator's `bin15-flip.sh live --gateway`.
- **Slot 7 (hcv) and Hypercall, the eighth venue** (live since the
  2026-09-26 go-live; risk-policy "HYPERCALL — slot 7", "HYPERCALL — the
  order arm").
  - Slot 7 trades Hypercall options in PAPER, judged by the held-quote law.
    Its book persists in `hcv-state.tsv`.
  - The order arm (`crates/exec-hypercall`) runs only as the operator's
    `hypercall-live` verbs; the engine refuses a live slot 7.
  - **OPEN — HC11c**, the operator's ruling: at switch-on slot 7 spent its
    $50 premium cap on weekend quotes 11–19 vol points wide. It buys at the
    ask and marks at the mid, so the $20 day stop (a level, not a latch)
    tripped on the spread; σ̂ ignores the HAR set's weekday profile.
  - Fees `[fees.hypercall] option = "0:0"` are UNVERIFIED (O-HC7), as are
    `[fees.mexc]`'s published rates.
  - `hypercall_history`'s payouts lane waits for `CLAUDE_WORKER_HC_WALLETS`.
- **The Binance arm (BX0–BX13; `docs/binance-exec-plan.md`,
  `docs/binance-runbook.md`, risk-policy "BX — the Binance execution laws").**
  - `crates/exec-binance` covers:
    - USDⓈ-M and COIN-M (COIN-M IoC-only);
    - spot incl. bStocks (IoC);
    - options (IoC);
    - Stocks — battery-only: the engine refuses `equity`.
  - It is deployed DARK: no member arms it.
  - The mainnet R0 session (runbook §13) and the Stocks battery (§12) are
    the operator's.
- **Market data: ten ingress crates.**
  - MEXC (the seventh venue) is DATA-ONLY (O-MX1).
  - The Binance options feed is fstream's routed `optionMarkPrice` stream
    (BX0).
  - Since HK7b (2026-10-13) the Binance options join the AI boot universe as
    signal legs: an order on one is unroutable (`core_fill::signal_only_sym`).
  - Live smokes WITHOUT stopping the engine: `cargo test --release -p cli
    --test <name>_live_smoke -- --ignored --nocapture` for `mexc`,
    `binance_md`, `binance_coinm`, `hypercall` and `hyperevm`. Each runs under
    the `CARGO_TARGET_DIR` its module doc names, so the wrapper's
    `target/release/multivenue-engine` is never relinked (`*_SMOKE_SECS`
    ≤ 900).
  - Hyperliquid's venue law: a subscribe to a settled or unknown HIP-4 coin
    FINs the whole socket, and `outcomeMetaUpdates` never replays a roll.
    The HL thread never runs DNS; `hyperliquid: run-loop returned` names
    each session's end.
  - LuLu on this Mac blocks a freshly built binary's outbound connections
    until the operator allows it: a smoke's boot-REST `Timeout` is LuLu.
- **Zero-copy (risk-policy "ZC pass A", "ZC pass B", "HK2").**
  - Every ingress parser fills in place, and core-ring lends its slots.
  - The ten drains share `core_net::drain` and one oversize guard.
  - Open until rustls 0.24: core-net's rustls RX copy, and rustls' buffered
    API allocating per record (gate 72).
- **HAR (H1–H3).** The engine owns `core_vol::LongVolSet` (≤ 12 series, from
  `~/multivenue/har.toml`). Its state is written off the engine thread, and
  it is published as `/state.har`. The worker's `har_backfill` / `har_seed`
  ride `candles-cycle.sh`. Since HK7a the Hypercall index lands in
  candles.db as 1 m capture bars, a candidate primary per `har_seed compare`.
- **The HK housekeeping lane (risk-policy "HK", "HK, part 2").** HK1–HK10
  (2026-10-13) closed Section A of that day's survey; HK11–HK17 (2026-10-14)
  Section B and the day's findings; HK18–HK23 (2026-10-15) the follow-ups;
  HK24–HK33 ("HK, part 3", 2026-10-15) the rest of the housekeeping;
  HK36–HK37 ("HK, part 4", 2026-10-16): the bn / bybit / mexc
  `Tick.venue_seq` is the arrival ordinal keyed on the full venue id, and
  HL's empty `trades` snapshot counts on `…_trades_empty_total`, not as a
  parse error; HK39–HK40 ("HK, part 5", 2026-10-16): OKX `bbo-tbt` and
  Binance options join the ordinal, keyed on the venue `ts` and the mark
  element's `E` (Binance option touches now reach `TopOfBook`; ai-exec is
  the only reader); HK42 ("HK, part 6"): option ticks carry `E` as venue
  time, judged stale at 1 000 ms on their own clock, and count on
  `engine_ingress_binance_options_*`.
- **S16, sixteen strategy slots (risk-policy "S16", 2026-10-15).** S16-1..6
  on main and deployed 2026-10-15 22:32Z: member table, `STRATEGY` parser +
  `strategy-mask` (the wrapper resolves through it; `all` refused),
  `SlotMask`, exec router / ledger (`LEDGER_RESTING` 1 024 with a
  high-water mark) / boot, the HL and Binance arms, `/state` schema 3.
  Slots 8–15 are reserved: arming one is refused (S16-R5); a member there
  needs its own ruling. Release builds have `overflow-checks =
  false`: never write `1u8 << slot`; debug tests at slots 8/11/15 guard it.
- **The S3 archive (HK11, HK18, HK20).** Uploads are paced in 64 KiB pieces
  under `MULTIVENUE_S3_MAX_UPLOAD_MIBPS`, 20 MiB/s since HK20 (the link's
  ~5 MB/s binds; the feed delays stayed near their no-upload band). An
  uncapped push (8 MiB writes) stalled the feeds on 2026-10-14. A weekday's
  capture (6.0–6.4 GB stored) takes ~20 min; the nightly budget, 8400 s, is
  a ceiling. A SIGTERM aborts the upload in flight, and is deferred across
  each multipart initiate attempt so the abort can name it (HK21, HK27). The nightly
  cycle first runs `abort-stale` (incomplete uploads over 24 h; `uploads`
  lists them). `push-derived` keys keep their path under the store's root,
  `--path` too, and candles go up zstd'd.
- **Gates at HEAD (HK, part 4, 2026-10-16):**
  - clippy clean.
  - nextest 4 755 passed (10 skipped: the five `#[ignore]`d live smokes,
    four alloc child helpers and a fixture regenerator), under nextest
    0.9.148. A random `LEAK` at ~0.2 s under nextest < 0.9.145 on macOS is
    nextest's pipe race, not the test (HK23). Only the scrape hammer runs alone (`.config/nextest.toml`):
    the alloc tests' reds under load were libtest's runner thread, no longer
    counted (A-S7-101, HK14). nextest runs no doctests: core-ring's
    `Mailbox` `compile_fail` doctest needs `cargo test --doc -p core-ring`.
  - alloc 127/127 at the gates' pins (0 B/op, +4 ignored child helpers).
    `--test-threads=1` and a fresh `Compiling bench` are required.
  - `make copy-audit`: 27 baselined, new 0 (self-test OK).
  - `make license-check` OK.
  - `make bench-check` OK: 16 samples within 15 % on the M4 (HK9, HK16); a
    missing or stale result fails.
  - worker pytest 1 786 passed, 3 skipped (the three need the release
    binary on PATH), HK27.
  - `make py-lint` all checks passed (HK8a). `make lint` means clippy.
  - fuzz: each HK cut's touched targets 60 s, clean (risk-policy "HK1",
    "HK2", "HK4"). The longer runs are in each lane's record.
  - A red is a red: the old flake list was root-caused (HK4, HK8b, HK14) or
    runs one at a time (HK8c).
- **Open operator items (non-gating):**
  - **Disk:** 26 GiB free on 2026-10-14 after the worktree cleanup. Writers
    do not retry after ENOSPC; a restart is the recovery.
  - **S3:** bucket versioning and `AbortIncompleteMultipartUpload` are not
    set; the nightly `abort-stale` (HK21) now clears what a lost initiate
    answer orphans, so the rule is a second backstop, not the only one. The
    `s3.env` credential pair was once pasted in chat: rotate it.
  - **Format drift:** ~208 Rust files and ~106 worker files outside the HK
    lane's are not rustfmt/ruff-format clean (HK28); formatting them all is
    `cargo fmt --all` in effect — the operator's call.
  - **Polymarket parse errors: resolved (HK30).** They were the venue's
    post-trade `book` re-snapshots (", " separators); 0 since 13:46Z
    2026-10-15. `RAW_TAP=pm` stays as the standing rejects tap
    (`pm-raw.tap`, ≤ 64 MiB a run). Largest frame seen: 10 152 B against the
    64 KiB RX buffer (982 584 frames, 3.5 h; risk-policy "HK, part 3").
  - **`venue_seq` (HK31, HK32):** pm, hl (`bbo` + outcome `l2Book` touch)
    and deribit (`quote`) ticks carry a per-instrument arrival ordinal
    (`core_types::ArrivalSeq`), not ms bits; 0 = an older tick, counted on
    `seq_regressions_total` (baselines pm ~0, hl ~0, deribit ~115 an hour).
    Rulings open: BIN15 O8's touch source (l2Book vs bbo); the u32
    truncation of Binance/Bybit/MEXC seqs (no live book reads them).
  - **Memory:** a whole-root `audit-pnl` / `backtest` OOMs above ~27 GB; use
    bounded ≤ 2 h window roots.
  - **Regime:** `regime.toml [labels] require = 1` is NOT flipped live.
- **Reminder: upgrade to rustls 0.24 once 0.24.0 is released** (operator,
  2026-09-26: the only rustls work needed). 0.24 decrypts records in place,
  which fixes gate 72's copy and allocation. Pinned 0.23.38; as of
  2026-10-13 only `0.24.0-dev.*` is published (`cargo info rustls@0.24.0`
  says "could not find" until the release).

## Open items added 2026-10-17 (the Claude-setup review, `claude/claude-setup-review-2026-10-17.md`)

- **Worker API models — PREPARED, not applied** (operator ruling 2026-10-18:
  "prep now, rescore later"). `MODEL_BULK` (`claude-haiku-4-5`),
  `MODEL_REASONING` (`claude-sonnet-5`) and `MODEL_ANALYST` (`claude-opus-5`)
  are previous-generation. The ready-to-apply patch (5.5 ids, `MODEL_EFFORT`
  pinning the analyst at `high`, `anthropic >= 1.12.0`, tests, this repo's
  CLAUDE.md line) and its apply steps are in the vault:
  `docs/research/claude-setup/worker-models-5-5/`. It waits for Stage 3
  because step 5, the news gold-set rescore, makes API calls. Not applied on
  disk: the worker runs from the working tree.
- **`.claude/hooks/post-edit-fmt.sh` is REPORT-ONLY** (operator ruling HK28,
  2026-10-18): `rustfmt --check` / `ruff format --check` on the edited file,
  a stderr note, never a rewrite.
- **`AGENTS.md` is a symlink to `CLAUDE.md`** (operator ruling 2026-10-18):
  one file for every tool.
- **`scripts/git-hooks/pre-push` is INSTALLED** (operator ruling 2026-10-18,
  `.git/hooks/pre-push` → it): a push needs `MV_OPERATOR_PUSH=1 git push …`,
  on every surface. Remove with `rm .git/hooks/pre-push`.
- **`docs/risk-policy.md`** (~17 000 lines) mixes the normative laws with the
  lanes' append-only records; a split into a short normative file plus
  `docs/risk-record/<lane>.md` would let a reviewer read the law whole.
- `Makefile`'s `copy-audit` comment still says "all nine ingress crates"
  (there are ten).
