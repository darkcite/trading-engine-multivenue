# Migration notes

This document tracks **format and schema migrations** — places where a bump
to a wire-format version, an on-disk file layout, or a config key has
ripple effects the operator needs to know about.

Each entry is atomic: one version bump per section. Do not batch.

## 2026-09-26 — HIP-3 builder-dex perps: real asset ids and size decimals (HC10)

**What changed**

- `ingress_hyperliquid::discovery`:
  - `perpDexs` keeps each dex's index;
  - `HlDiscovery::ingest_dex_meta` reads a builder dex's own `{"type":"meta","dex":…}`;
  - a `dex:COIN` coin resolves to `100_000 + dex_idx × 10_000 + index` with that meta's `szDecimals` (`builder_asset_id`, `dex_of`, `has_dex`).
  - A coin its dex's meta does not list no longer resolves. A dex whose meta was not ingested stays name-validated at asset 0, as before.
- **The engine's HL boot discovery** also fetches the meta of every builder dex a configured coin names. It fetches only dexes the venue's own `perpDexs` listed, 250 ms apart like the other discovery requests. A failed fetch is a warning; market data addresses coins by string.
- `exec_hyperliquid::asset`: `BUILDER_BASE`, `SPOT_BASE`, `AssetKind`, `kind_of`, `is_perp_id`. The mirrored id law is held together with the ingress one by the cli test `builder_asset_ids_agree`.
- The HYPARB binder (`cli::hyparb_live`) binds a builder-dex coin by its derived id (never id 0).
- `core_types::hl_px`: the HL perp price law (five significant figures, `6 − szDecimals` decimals, integers always legal), its rounding, and the size step.
- `tests/fixtures/hl/vectors.tsv`: two HIP-3 order rows (asset `110_002` IoC buy, `110_009` ALO reduce-only sell) from the official `hyperliquid-python-sdk` 0.24.0.
  - The 27 older rows regenerate byte-identical.
  - The header now names the SDK version.
  - `VECTOR_ROWS` is 29.
- The `hl_info` fuzz target drives `ingest_dex_meta`.

**Impact**

- None at runtime today: no `xyz:` coin is in the live `[hyperliquid] coins`. At go-live (O-HC20: the eight hedge coins appended), the boot adds one `/info` request (the `xyz` meta) and logs each hedge coin's asset id and `szDecimals`.

**Migration steps**

1. None. The go-live's `[hyperliquid] coins` append is the operator's (O-HC20).

**Rollback**

- Remove the `xyz:` coins from `[hyperliquid] coins`.

## 2026-09-26 — `crates/exec-hypercall`: the Hypercall order arm and its operator verbs (HC9, O-HC19)

**What changed**

- `crates/exec-hypercall` (new): `HcExchange`, a complete `OrderDispatch` for Hypercall options:
  - `POST /order`, `PUT /order` and `DELETE /order_cloid` bodies are rendered in place and signed over their own spans (HC8, D7);
  - a fail-closed answer scanner;
  - the private fills socket (`Authenticate` the owner, then `fills` and `order_updates`);
  - reconciliation over `/portfolio`, `/orders?status=open` and `/fills`;
  - a sliding-window rate governor for the Default tier;
  - the E-9 client id (`HC`, version, slot, `client_oid`, check).
- `multivenue-engine hypercall-live <status|simulate|recon|dust|cancel-all>` and `scripts/hypercall-live.sh`, the mainnet operator verbs. The two writes need `--confirm`.
- `exec_boot` refuses a live slot on `hypercall` with a reason of its own. It names what slot 7's live-arming ruling must settle; the risk-policy entry "HYPERCALL — the order arm" has the detail.
- `make copy-audit` covers `crates/exec-hypercall`.
- New fuzz targets `hypercall_response` and `hypercall_userws`.
- New alloc gate 84.

**Impact**

- None on the running engine: nothing in it constructs the arm, and a live `hypercall` slot refuses the boot. The new keys (`HYPERCALL_WALLET`, `HYPERCALL_AGENT_KEY`) are read by the verbs only.

**Migration steps**

1. The operator adds `HYPERCALL_WALLET` (the owner address) and `HYPERCALL_AGENT_KEY` (the signer) to the repo `.env` (`chmod 600`). A session never writes `.env`.
2. `cargo build --release -p cli` (or a separate `CARGO_TARGET_DIR` for a standalone smoke — G0).
3. Run the read-only verbs first: `scripts/hypercall-live.sh status` and `recon`, then `simulate --symbol <SYM>`.
4. The operator's dust smoke: `scripts/hypercall-live.sh dust --symbol <SYM> --confirm`. PASS means the order was signed on mainnet, cancelled, and nothing filled.

**Rollback**

- Nothing to roll back at runtime. Remove the two `.env` lines to disable the verbs.

## 2026-09-26 — `signer_eip712::hypercall`: the venue's EIP-712 actions, sign-only (HC8, O-HC17)

**What changed**

- `signer-eip712` gains a parameterised domain (`Eip712Domain`,
  `domain_separator_of` — boot-time), `eip712_digest`, and public word
  encoders (`enc_string`, `enc_address`, `enc_u64`, `enc_bool`, `enc_i256`
  — sign-extended). The Polymarket and Hyperliquid paths are unchanged;
  the tests pin that the generic domain reproduces both cached separators.
- `signer_eip712::hypercall`: the domain (`Hypercall`, `1`, chain 999 /
  998, the zero contract), the 18 type strings of the official SDK
  0.1.0 plus the route-less legacy `PlaceOrder` (19), their typehashes as
  compile-time constants, a struct hash per type over BORROWED spans (the
  request body's own bytes — D7), `hc_eip712_digest`, `sign_hc_with_key`
  and the hot path's `sign_place_order_with_key`.
- `crates/signer-eip712/tests/fixtures/hypercall/vectors.tsv`: 38
  known-answer vectors generated by the SDK's builders + ethers 6.17.0
  (test keys only; the header says how). Every one is byte-exact —
  struct hash, digest, 65-byte signature.
- Alloc gate 83 (`hypercall_sign_with_key_is_zero_alloc`): place,
  replace, cancel by client id and an RFQ accept, 0 B/op. Criterion
  `signer/hypercall_place_order`.

**Impact**

- None at runtime: nothing calls it yet. No keys, no network, no exec arm
  (O-HC1 stands; HC9 is its own ruling). The RFQ ids' mapping from the
  body's strings to `bytes32` is not in the SDK and is HC9's to take from
  the venue.

**Migration steps**

1. None.

**Rollback**

- Revert the commit.

## 2026-09-26 — Hypercall options in the AI boot universe (O-HC17)

**What changed**

- `cli::build_ai_universe` takes the Hypercall option syms the boot
  selected (`discovery.hypercall_options`); the `hypercall-idx:<U>` index
  syms stay out (capture-only `Mark`s, caps 0).
- The v1 ruleset grammar's rule 6 now admits them; the v2 grammar already
  resolved `hypercall:` descriptors through the `DescriptorTable` (HC4).

**Impact**

- None until `[hypercall]` is configured live (O-HC12): an empty chain adds
  nothing. Then `ai: ruleset boot-universe snapshot built symbols=` grows by
  the chain's size. An order leg on a Hypercall option stays `unroutable`
  in the paper matcher and the harness (HC1) — the Q-MX6 shape.

**Migration steps**

1. None.

**Rollback**

- Revert the commit.

## 2026-09-26 — `candles.toml`: `extra` instruments and a lane's `backfill_1m_h` (HAR W1)

**What changed**

- Two lane keys in the candles lane's policy file (`~/multivenue/candles.toml`,
  worker-only; grammar in `candles.toml.example`):
  - `extra = ["<sym>", …]` — instruments the universe does not name,
    fetched by that lane like its own under the lane's descriptor
    (`[hyperliquid] extra = ["xyz:SP500"]` → `hyperliquid:xyz:SP500`); a lane
    the universe lacks is created, a symbol it already names is not doubled;
  - `backfill_1m_h = <1..8760>` — that lane's 1 m backfill for an EMPTY
    series, over `CLAUDE_WORKER_CANDLES_BACKFILL_1M_H` and the 48 h default.
- A malformed value of either ignores the whole file (the file's standing
  law: a typo only widens back to §9.5).
- The candles cycle reads the policy before its "no candle-lane instruments"
  exit, so a universe with no lane can still run `extra` lanes.
- `claude_worker.candles` builds every lane from one table (`LANE_FORMS`);
  the lanes read from `universe.toml` are unchanged.
- The HL lane is PACED to half of HL's per-IP weight minute (1 200, shared
  with the engine on this host): a rolling-minute budget,
  `CLAUDE_WORKER_CANDLES_HL_WEIGHT_PER_MIN` (default 600), booked per page at
  HL's published weights (`candleSnapshot`: 20 + 1 per 60 candles). Before,
  a fresh store's first HL cycle (and W1's — ~90 pages, ~3 300 weight) could
  spend the IP's minute two or three times over.

**Why**

- HAR W1: the ten `xyz` perps (HL dex `xyz`) do not fit the engine's HL
  coin table, and HL keeps only the newest ~5 000 minutes at 1 m — accrual
  must start in the worker, from the first cycle's 96 h reach.

**Impact**

- None until the live `candles.toml` names an `extra` (the example's W1
  block, after this branch reaches `main`). Then ~20 more HL
  `candleSnapshot` calls an hour (ten coins × 1 m + 1 h), inside the lane's
  demand-sized budget.
- The pacing: a steady hourly cycle (~900 HL weight with the extras) takes
  about a minute longer; W1's first cycle about six minutes. The worker
  serialisation guard is held that much longer.

**Migration steps**

1. After the merge: uncomment the example's W1 block into
   `~/multivenue/candles.toml`; the next hourly cycle backfills 96 h of 1 m
   and 90 d of 1 h for each coin.

**Rollback**

- Remove the block (the rows already stored stay; nothing reads them but
  the HAR seed).

## 2026-09-26 — the healthy-session backoff in core-net; Hypercall and MEXC reconnect by it (O-HC16)

**What changed**

- `core_net::{HEALTHY_SESSION_MIN_NS, should_reset_backoff}` — moved from
  `cli/src/paper.rs` unchanged; the seven outer spawn loops call it from
  core-net.
- `ingress-hypercall`: the internal reconnect (`HcConn`) resets its
  backoff only after a session that moved market data (the venue's
  `ticks_total` against its value at session start) AND lived ≥ 30 s, or
  that ended in the keepalive's 60 s-silence trip. Before: any confirmed
  subscription reset it.
- `ingress-mexc`: the same law per `run_multi` slot, over the new
  `Driver::session_ticks` (the ticks that connection published this
  session); the keepalive's silence trip is the quiet trip. Before: a
  confirmed pair reset it on a venue close or a keepalive trip.

**Impact**

- A Hypercall or MEXC venue that drops sessions young now sees those
  reconnects climb 0.5 → 8 s (≤ 7.5 connects a minute per connection),
  as the seven outer loops have since `7235201`. A healthy session's end
  still redials after ~0.5 s. `engine_ingress_{hypercall,mexc}_reconnects_total`
  count as before.

**Migration steps**

1. None. Live at the first release build + restart after the branch
   merges (MEXC is live; Hypercall is not yet configured).

**Rollback**

- Revert the commit.

## 2026-09-26 — the long-tenor state files are written off the engine thread (HAR H3.7)

**What changed**

- A new thread, `har-state-writer` (`cli::har_writer`), is spawned at boot
  when `har.toml` configures the long-tenor set. At each series' UTC day
  close the engine thread copies that series' engine whole (~201 KiB,
  `core_vol::LongVolEngine::copy_to`) into the series' mailbox — one
  `core_ring::Mailbox` per series, new in core-ring: a single-slot SPSC
  hand-off by ownership, filled and read in place, never waiting — at the
  close's own 1 s poll. The writer renders `state-<NAME>.tsv` and runs
  create/write/fsync/rename. The engine loop no longer renders or fsyncs a
  HAR state in its 5 s report block.
- The file is unchanged — path, header, rows: one renderer,
  `core_vol::render_state_file`, serves the writer and the shutdown write.
- A failed write keeps the state and is retried every 5 s (the F18 warning,
  `kind="har"`, once a minute); the engine's offers are refused meanwhile,
  never waited on, and the newest state follows the first retry that lands
  — a file never goes backwards.
- Shutdown: the writer is stopped and joined, then the forced synchronous
  write of every series runs on the engine thread as before (a state the
  writer still held is superseded by it).
- The writer failing to spawn is an error log (`har: the state writer
  thread did not spawn`) and the pre-H3.7 write on the engine loop — never
  a refusal.
- `engine_har_*` are computed by `strategy_core::har_gauges` (values
  unchanged; bench gate 82 now runs it).
- `docs/hot-path-latency.md`: the staggered day closes run
  00:01:00–00:01:12Z (the minute stamped 00:00Z is delivered at its close),
  not "by 00:00:12Z".

**Why**

- Ruling O-HC15 (H3.7): the render and the fsync — a disk round trip —
  ran on the engine thread twelve times a UTC day. The hand-off that
  replaces them costs the loop 2.56 µs per close, warm (bench
  `vol/long_state_copy_warm` on the M4).

**Impact**

- One more thread, parked 250 ms between passes; the twelve mailbox slots
  are ~2.4 MiB of heap boxed at boot. A boot without `har.toml` spawns
  nothing and boxes nothing. Nothing on disk changes.

**Migration steps**

1. None. Live at the first release build + restart after the branch
   merges, and only on a boot that finds a `har.toml` (`~/multivenue/har.toml`
   or `--har`; none is live yet — the HAR go-live steps of the H3 plan §15
   are staged with O-HC12).

**Rollback**

- Revert the commit; the files it writes are the files the engine loop
  wrote before.

## 2026-09-26 — `/state` schema 2: `xmm` replaces `icdp`; `engine_xmm_*`; `[labels.xmm]`; audit-pnl reads the queue venue's prints (XMM XH3)

**What changed**

- `/state` `"v"` is **2**. The `icdp` object and `boot.icdp_hash` are
  GONE (slot 6 has been xmm since XH1; they read zeros). New: the `xmm`
  object (`configured`, `n_perps`, the member's 17 counters flat, and
  `perps[]` — one row per quoted perp: `hl_sym`, `lead_sym`, `pos_1e6`,
  the follower touch, our bid/ask price and state (0 none, 1 sent,
  2 resting, 3 cancelling), `stale_flags`, and the leader's / follower's
  age in ms, `-1` = never), `boot.xmm_hash` (sha256 of `xmm.toml`) and
  `boot.xmm_coins` (the row names, `"BTC,ETH,SOL,XRP"`).
- `/metrics`: the 14 `engine_icdp_*_total` counters are GONE; new
  `engine_xmm_{placed,modifies,lead_cancels,requote_cancels,pull_cancels,
  expiry_cancels,gated,gate_overflow,capped,rejected_alo,rejected_other,
  canceled,filled,fills,unmatched,ctx_refused,stuck}_total`,
  `engine_xmm_perps` and `engine_xmm_p{0..3}_pos_1e6`.
- `regime.toml`: `[labels.xmm]` parses (the label is carried and never
  consulted — the HORIZON law, bin15's precedent), so `[labels]
  require = 1` can boot xmm enabled. The same list now names `bin15`
  (which the boot always mapped) and no longer `rule_tree` (which it
  never could): `[labels.bin15]` parses; `[labels.rule_tree]` is refused
  at the grammar instead of at resolve — the same refusal, earlier.
- `audit-pnl` reads the queue venue's trade prints (`hl-events.pmlr`
  `Trade` rows) for a run with a post-only maker in it — only inside that
  run's ticks — and every engine tracks the syms a post-only maker
  placed on, so slot-6 queue orders fill there by the queue law (before,
  never). Such a run prints one more stderr line
  (`queue-orders=… queue-prints=…`); every other run reports byte for
  byte as before, and the stdout JSON shape is unchanged
  (`audit_pnl_version` stays 2).
- The queue law (`core_fill::queue`, post-only makers on Hyperliquid —
  only xmm): off the parity switch, a record of ANY symbol of the venue
  lands what is due on every tracked perp (the block is the venue's), so
  a quiet book no longer holds a cancel back. The paper matcher,
  `backtest --member xmm` and `audit-pnl` follow; `xmm-parity` keeps the
  simulator's per-symbol clock.
- The dashboard (`dashboard.html`) shows an xmm block under the slot
  table and `xmm.toml` in the config panel (the worker's
  `config.xmm = {hash, quoted}` replaced `config.icdp`); it renders an
  older engine's `/state` v1 as "n/a".
- The xmm boot tell says `phase=XH3(paper)`; `/state` `slots[6].
  orders_dropped` now reads the member's refused sends.

**Operator action**: none required. To run xmm in paper, follow the
runbook in `docs/risk-policy.md` (XMM, "XH3 amendment"): the release
build first; the pre-flight that every descriptor resolves (an
unresolvable one refuses the WHOLE boot); `~/multivenue/xmm.toml` (copy
`xmm.toml.example`, the probe); `+xmm` in `STRATEGY` last (the allowed
name is `ai+vrp+xsd+bin15+hyparb+xmm`), outside the quiet windows; a
supervised restart and the post-boot checks.

**Rollback**: revert the XH3 commit. A `/state` reader that required
`icdp` must read `"v"` first; the dashboard reads either.

## 2026-09-26 — The queue law; `ORDER_FLAG_POST_ONLY`; paper order events; the xmm policy; `xmm-parity` (XMM XH2)

**What changed**

- `Order.flags` bit 1 = `ORDER_FLAG_POST_ONLY` (byte 14 — every older
  `Order` reads 0). A maker carrying it on Hyperliquid is judged by the
  queue law (`core_fill::queue`) in BOTH the engine's paper matcher and
  the harness; every other order keeps the strict-cross law. Only the
  xmm member sets it, so no existing member's paper fills move.
- The paper matcher emits order events (`RESTING`, `REJECTED`,
  `CANCELED`, `FILLED`) for queue orders — the order-event lane's first
  producer — pumped after the fill pump. The exec router forwards trade
  prints to the paper arm (`observe_trade`).
- `/metrics` gains `engine_paper_matcher_queue_{placed,rested,
  rejected_alo,canceled,fills}_total`,
  `engine_paper_matcher_order_events_overflow_total` (must stay 0) and
  `engine_set_order_events_unrouted_total`.
  `engine_paper_matcher_open_orders` now counts queue orders too.
- `MatcherCounters` gains six fields (repr(C); no size assert; not on
  any wire).
- The paper matcher's fill ring is 256 (was 64); behaviour is unchanged
  until the old ring would have overflowed.
- `strategy-xmm` implements the LEAD θ policy (plan §5.2); with
  `maker_enabled = 1` it places post-only orders and arms a 100 ms safety
  timer. `XmmPerp` gains `lot_1e6` (from the HL `szDecimals` table in
  `core_config::xmm::XMM_COINS`); `XmmParams` gains three research-only
  fields (`sim_parity`, `quote_from_ns`, `quote_until_ns`) that
  `xmm.toml` cannot set.
- New verb `multivenue-engine xmm-parity` (research: the XH2 parity
  replay). The frozen `backtest` argv and outputs are unchanged.

**Operator action**: none. No `strategy.conf` names xmm until XH3.

**Rollback**: revert the XH2 commit; nothing on disk changes format
(the flag bit was padding).

## 2026-09-26 — Strategy slot 6 = `xmm`; `icdp` unlinked; the trade and order-event lanes; `Order.flags`; HL perp depth captured (XMM XH1)

**What changed**

- Slot 6 of `crates/strategy-set` is the `strategy-xmm` member
  (`SLOT_XMM` / `BIT_XMM`, still bit 6 = 64). `strategy-icdp` is
  UNLINKED, not deleted (ruling O-XH1): it stays in the workspace with its
  own tests, its bench alloc gate and `backtest --member icdp --icdp …`,
  but no engine path composes it. Slot 7 stays free.
- Mask names: `xmm` (64), `ai+xmm` (112) and
  `ai+vrp+xsd+bin15+hyparb+xmm` (127) are new; `icdp` and `ai+icdp` are
  GONE — `--strategy icdp` refuses the boot ("unknown --strategy value").
  `run --icdp` is gone; `run --xmm <path>` names the artifact (default
  `~/multivenue/xmm.toml`). A requested xmm bit with no artifact REFUSES
  the boot (the F19 law).
- New artifact `~/multivenue/xmm.toml` (single `[xmm]` section, 22
  integer keys, every one required; `xmm.toml.example` is the probe;
  money-cap ceilings clip $10 000 · per perp $5 000 000 · gross
  $20 000 000 · resting $1 000 000). The member is DARK at XH1:
  configured, it places nothing.
- `exec.toml`: a LIVE slot 6 refuses the boot until XH4 gives xmm its own
  Hyperliquid arm (O-XH3/O-XH7) — it would otherwise share slot 3's.
- **Per-slot timers.** `StrategySet::on_timer` runs each member only
  when ITS OWN `timer_period_ns()` has elapsed since its own last call
  (and the regime detector on its own 1 s); a fast member timer no
  longer speeds up everyone's. Identical for every existing member: each
  has a 1 s period or an empty `on_timer` (`u64::MAX`).
- **Trade lane.** `ingress-hyperliquid` pushes every parsed print onto a
  new engine lane (`TradePrint`, `Ring<_, 16384>`, drained after the
  tick lanes → `Strategy::on_trade`, default no-op). The capture is
  unchanged (`ChannelId::Trade` rows are written first); a full lane
  drops the print and counts it.
- **Order-event lane.** `OrderEvent` (`Ring<_, 1024>`, drained after the
  fill lanes and the dispatcher's fill pump — E6: a fill waiting in the
  same iteration is booked first → `Strategy::on_order_event`, default
  no-op), routed to the
  placing slot ALONE (the X1 fill law); an out-of-range or disabled slot
  is counted in `StrategySet::order_events_unrouted`. No producer yet
  (XH2's paper model, XH4's gateway).
- **`Order.flags` @14** (was the first `_pad0` byte): bit 0 =
  `ORDER_FLAG_REDUCE_ONLY`. Nothing sets it before XH4.
- **Hyperliquid perp depth** is written to `hl-depth.pmlr` (top-5,
  change-gated, the outcome legs' law) as research capture for the
  queue-ahead study. `backtest` drops HL depth rows whose descriptor
  classes as a perp, so no replay merges them.
- `backtest --member xmm [--xmm <path>]`: the member on the frozen fill
  law, with the HL trade prints merged ONLY for that member.
- `regime.toml`: `[labels.icdp]` is refused at the grammar ("unknown
  coded member"), exactly as `[labels.ev]` / `[labels.cross_arb]` /
  `[labels.latency_arb]` are. Slot 6 takes no label yet: with
  `[labels] require = 1` an enabled xmm REFUSES the boot (fail-closed)
  until its gate is wired.
- Slot-6 labels: `/state` `slots[6].name`, `audit-pnl`
  `strategies[].label` for `strategy_id 6`, `exec_boot::SLOT_NAMES[6]`
  and the dashboard read `xmm`. The `/state` `icdp` object stays on the
  wire, reading zeros, until XH3 replaces it (the worker reads it).
- Metric `engine_ingress_<venue>_trade_ring_drops_total` (one per
  ingress; only Hyperliquid can move it).
- `scripts/engine-wrapper.sh`: the allow-list drops `icdp` / `ai+icdp`
  and gains the three xmm names; optional `XMM_TOML=<path>` in
  `strategy.conf` passes `--xmm` (not a file, or no xmm in `STRATEGY`,
  REFUSES — exit 78).

**Why**

- XMM-HL (plan v2, rulings O-XH1…O-XH15): the Binance-led post-only
  maker takes slot 6. A label, mask or audit row evidenced for icdp must
  never silently apply to a different member.

**Impact**

- On-disk formats: `Order` gains a flags byte in former padding
  (wire-additive: every older Order reads as no flags). `hl-depth.pmlr`
  gains perp rows (additive; older captures have none). The slot NUMBER
  is wire-stable: rows under `strategy_id 6` in a capture taken BEFORE
  2026-09-26 are icdp paper rows wearing the `xmm` label.
- Replays: every existing member's merge is unchanged — trade prints
  merge only for `--member xmm`, perp depth for nobody.
- Config keys: `regime.toml [labels.icdp]` refuses the boot; an
  `exec.toml` marking slot 6 live refuses the boot.

**Migration steps**

1. `~/multivenue/strategy.conf`: if `STRATEGY` names `icdp` or
   `ai+icdp`, change it (e.g. to `ai`) BEFORE the restart onto this
   build — the wrapper refuses those names (O-XH1: the operator removes
   icdp). The live file checked 2026-09-26 names no icdp mask.
2. A `regime.toml` carrying `[labels.icdp]` drops the section.
3. Nothing enables xmm: it stays out of `strategy.conf` until XH3.

**Rollback**

- Revert the XH1 commit; no data or config migration to undo (an
  `hl-depth.pmlr` with perp rows reads fine on the old build, whose
  replay then merges them).

## 2026-09-26 — HL: the one-sided outcome `bbo` drop counts on its own (`engine_ingress_hyperliquid_outcome_bbo_one_sided_total`)

**What changed**
- A HIP-4 outcome leg's `bbo` with its ask `null` — BIN15 O8's policy
  drop — is no longer a parse error: new gauge
  `engine_ingress_hyperliquid_outcome_bbo_one_sided_total`; the frame
  counts in `engine_ingress_hyperliquid_msgs_total` (never in
  `…_ticks_total`) and is no longer tapped as a parse reject (it is still
  tapped raw).

**Impact**
- Metrics: HL `…_parse_errors_total` falls by ≈ 1.5/s — its whole steady
  rate on 2026-09-26 — and `…_msgs_total` rises by the same. It does not
  go to zero: each mid-session roll still adds its unsubscribe echoes and
  the frames in flight for the retired coins.
- Captures: with the raw tap on, these frames stop appearing as rejects.

**Migration steps**
1. None: the routine restart of a binary built from this change.

**Rollback**
- Safe: an older binary counts them as parse errors again and the gauge
  is gone.

## 2026-09-26 — HL reconnect hygiene: lone `created` rolls, the `run-loop returned` fields, the healthy-session backoff (`7235201`)

**What changed**
- A reconnect retires a family on a settled or expired HIP-4 instance and
  re-reads `/info outcomeMeta` for its successor. In captures such an
  instance has no `settled` InstrumentRoll (its later push matches no
  slot), and the re-discovered successor is a lone `created`, stamped when
  adopted (`docs/wire-format.md`).
- `hyperliquid: run-loop returned` gains `err_site`, `io_kind`,
  `venue_code`, `lived_ms`, `acks`, `acks_expected`; core-metrics err sites
  10 `peer-eof` and 11 `peer-close`.
- The seven loops sharing `should_reset_backoff` (Polymarket, Binance,
  OKX, Deribit, Hyperliquid, RPC, HyperEVM) reset their reconnect backoff
  only after a session that moved data AND lived ≥ 30 s, or a venue-quiet
  trip.
- `HYPERLIQUID_API_HOST` is resolved once at boot (exit 1 if it does not
  resolve) for the between-session re-read; boot discovery already
  required it whenever HL is configured.

**Impact**
- Offline: a reader that pairs `settled` → `created` per slot sees a lone
  `created` after a reconnect that retired an instance.
  `backtest::binary::link_successors` links by created time and skips a
  successor adopted later than `SUCCESSOR_MAX_LAG_NS` after the expiry, as
  it does any capture gap.
- Logs: greps on `run-loop returned res=` still match.
- A venue that drops sessions young now sees reconnects climb 0.5 → 8 s
  instead of ~1 s.

**Migration steps**
1. None: live since the 09:03Z restart of 2026-09-26.

**Rollback**
- Unsafe: an older binary re-subscribes dead instances on a reconnect —
  the 1.2 s loop this fixed.
## 2026-09-26 — `/state.har` and the `engine_har_*` gauges (HAR H3.5)

**What changed**

- `/state` gains a `har` object (additive — `"v"` does not move; placed before
  `recent`): `configured` (series the set runs), `hash` (SHA-256 of the
  `har.toml` loaded), `dropped` (series the file names whose feed the boot
  universe does not carry), the set's counters flat (`minutes_rolled`,
  `closes`, `day_closes`, `held`, `forced`, `day_close_ns_max`,
  `day_close_ns_last`, `epoch`), `tenors_d` = `[1,2,3,5,7,14,21,30,40]`
  and `series` — one object per series in `har.toml` order: `name`,
  `feed` (symbol id), `warm`, `days` (closed days resident, ≤ 64),
  `empty_days`, `gaps`, `newest_day_ms`, `day_age_s` (WALL-derived: whole
  seconds since the newest closed day ended; `-1` = none), `last_min_ms`,
  `open_minutes`, `epoch`, and per tenor `raw_1e6` / `fit_1e6` (σ
  annualised ×1e6; `0` = none), `pairs`, `fitted` and `fit_beats_raw`
  (`0`/`1` arrays), plus `weekday_1e6` (Monday first; `1e6` = a mean day)
  and `weekday_n`.
- `EngineSnapshot` grows 28 352 → 30 656 B at the Hypercall merge (twelve
  176 B rows over XMM XH3's `xmm` block; the
  32 KiB test bound holds). A row is rebuilt only at its series' UTC day
  close (or the boot restore) and copied by the 1 s publish.
- Four gauges, always registered: `engine_har_series_configured`,
  `engine_har_series_warm`, `engine_har_day_age_max_s` (the stalest
  series; `-1` = none closed a day) and `engine_har_day_close_ns_max`.

**Why**

- The ruling "publish a profile" and the H3 plan's §7 panel: the fit and
  the raw fold side by side (L2), and a series that stopped recalibrating
  must be visible (`day_age_s` > 93 600 is the red rule).

**Impact**

- A boot without `har.toml` renders `"har":{"configured":0,…,"series":[]}`
  and `engine_har_series_configured 0`; every other byte of `/state` and
  every other metric is unchanged. Readers keyed on names are unaffected.

**Migration steps**

1. None. The dashboard panel reads the object from H3.6.

**Rollback**

- Revert the commit; nothing on disk depends on it.

## 2026-09-26 — the long-tenor HAR runs in the engine: `har.toml`, `--har`, `~/multivenue/har/` (HAR H3.2–H3.4)

**What changed**

- `~/multivenue/har.toml` (new, optional; `har.toml.example` is its
  contract, `core_config::har` its parser — the engine's line grammar, not
  standard TOML): up to 12 `[[series]]` of `name` (1–12 of `[A-Z0-9]`),
  `feed` and an optional one-line `fallback` array (≤ 4, newest first) of
  `<venue>:<instrument>` descriptors. `claude_worker.har_config` reads the
  same grammar with the same messages.
- `multivenue-engine run` gains `--har <path>` and `--har-dir <dir>`
  (default `~/multivenue/har`). Without `--har` the default file is read if
  present; absent = the pre-H3 engine, bit for bit.
- `~/multivenue/har/seed-<NAME>.tsv` (the worker's cut, H3.6 hourly) and
  `state-<NAME>.tsv` (the engine's own rows: written at each of the series'
  UTC day closes and at shutdown) share one row grammar (`V 1`, then
  `D`/`C`/`A`/`P`/`Q`; `core_vol::parse_rows`, `LongVolEngine::write_rows`,
  byte-identical to `har_seed.seed_rows`). At boot the two merge by
  `core_vol::merge_rows` (the §6 day-merge law) and restore the series'
  `LongVolEngine`, held by `StrategySet` (`core_vol::LongVolSet`). No member
  reads it.

**Why**

- The HAR H3 rulings (2026-09-26): the long-tenor HAR runs in the ENGINE,
  one series per Hypercall underlying, seeded from the external backfill.

**Impact**

- Failure isolation: only an explicit `--har` that cannot be read refuses
  the boot. A default file that does not parse turns the service off (a
  named error); a series whose feed the universe lacks is dropped (named),
  the rest run; a seed or state that does not parse is dropped for its
  series. `engine-wrapper.sh` does NOT pass `--har` (a KeepAlive loop is
  the cost of an unreadable explicit path) — the default path is the
  switch.
- Cost: ~2.5 MB boxed at configure (twelve engines); per tick one hash
  probe; per minute one close per quoting series; per UTC day one close
  per series, staggered one a 1 s poll.

**Migration steps**

1. At the restart the operator names: write `~/multivenue/har.toml` (the
   example's twelve), append the seven TradFi perps to `[binance] usdm`,
   import the dry run's rows and run `claude_worker.har_backfill` once,
   cut the seeds (H3.6: then hourly by `candles-cycle.sh`).

**Rollback**

- Remove `~/multivenue/har.toml` (the next boot runs without the service);
  `~/multivenue/har/` may stay — nothing else reads it.

## 2026-09-26 — `core_regime::math::isqrt_i128` is the floor root at v = 2

**What changed**

- `isqrt_i128(2)` returned 2: the Newton start `v/2 + 1` equals `v` at 2,
  so the loop never ran. `v < 4` is now answered directly (1), and a
  test pins the floor law on every input up to 10 000. Python's
  `math.isqrt` — every mirror's — always said 1.

**Why**

- Found by the HAR H1 review as a Rust ↔ Python parity break (the long
  tape now carries the case: a day whose `Σ r²` is exactly 2).

**Impact**

- Every caller (core-vol, core-regime, strategy-bin15/-vm/-xsd) moves only
  for an input of exactly 2 — a sum of squared bps×1e9 returns that no
  market produces; every existing fixture is byte-identical and green.

**Migration steps**

1. None.

**Rollback**

- Revert the commit.

## 2026-09-26 — the scheduled-events feed on the news/event plane (O-HC8)

**What changed**

- `news.toml [events]` (new, optional; `news.toml.example` documents it):
  `horizon_days` (45), `lookback_days` (120), `macro` (the underlyings
  FOMC and BLS releases apply to) and `scheduled` — one inline table per
  dated event: `at` (ISO-8601 with its zone), `kind` (`earnings` |
  `lockup` | `macro` | `other`), `underlyings` (the `[hypercall]`
  spelling), `label`, `confirmed` (0 = an estimated date; default 1).
  A malformed table refuses the registry whole, like every other table.
- `claude_worker.news.scheduled` (new): every `news cycle` now also
  writes `scheduled-events.json` beside `calendar.json` — the events in
  `[now − lookback, now + horizon]`, each tagged with the underlyings it
  moves: the `[events] scheduled` entries, plus the Fed calendar's FOMC
  statements (one per UTC day, placed at the earliest row that states
  its time) and `[calendar] bls_releases` as `macro` on `[events]
  macro`. Every source's events at one instant and of one kind are ONE
  event (underlyings united, details joined, confirmed only if every part
  was): the event law sums a jump per event and must not count one twice.
  The module is also the READER: `load_feed(path)` → a `Feed` that keeps
  the window it vouches for (an absent, reshaped or other-version file
  covers nothing), and `events_in(feed, underlying, t, T)` — the S1 event
  law's `(t, T]`, or `None` (UNKNOWN, not empty) outside the window.
- `claude_worker.news.detect`: the Fed and BLS readings are lifted into
  `fed_rows(registry, store)` and `bls_rows(registry)`, which the 7-day
  calendar and the feed both take (the calendar's output is unchanged;
  its tests are untouched and green) — and `_fed_at` now dates a meeting
  whose day range crosses a month end (`"30-1"`) in the NEXT month. The
  cycle's detail line gains `scheduled=<n>` (`-1`: the write failed).

**Why**

- Operator ruling O-HC8: the S1 event law's calendar (earnings, CPI/FOMC,
  lock-ups) comes from the news/event plane as ONE scheduled-events feed
  that R1 and any future member both read.

**Impact**

- None on the engine (nothing reads the feed yet). One more small file
  write per cycle. An absent `[events]` table writes a feed that carries
  nothing.

**Migration steps**

1. Add `[events]` to `~/multivenue/news.toml` (the operator's file) with
   `macro` and the dated events of the Hypercall underlyings; the next
   cycle writes the feed.

**Rollback**

- Revert the commit; delete `~/multivenue/worker/news/scheduled-events.json`.

## 2026-09-26 — the long-tenor HAR in the worker: the parity mirror, the shared tape, `har_seed` (HAR H2)

**What changed**

- `claude_worker.vol_ref` gains `LongVolEngine` (+ `long_tenor_of`,
  `ANNUALISE_LONG_1E9`, the day constants), the bit-exact mirror of
  `core_vol::LongVolEngine`, and the lifted `ols_fit_1e9` /
  `trailing_means_1e9` that its `VolEngine` now delegates to (the V4
  fixtures stay byte-identical and green).
- A shared tape, `claude-worker/tests/fixtures/vol/long-1.{input,expected}.tsv`:
  ~210 days of a regime walk through the day clock, a hole, a short day,
  two empty days, refusals, the fits and QLIKE windows filling, the 1 d
  pair ring wrapping, a whole-ring silence that clears, and a restore
  into a fresh engine (seeds accepted and refused, the withheld fit,
  `refresh`, a live continuation). The expected rows are WRITTEN by
  `crates/core-vol/tests/long_parity.rs` (`HAR_LONG_PARITY_WRITE=long-1`)
  and replayed row by row by `tests/test_vol_ref_long.py`; a second
  harness test fails if a regenerated tape stops exercising any branch.
- `claude_worker.har_seed` (new module, `python -m …`, never a verb):
  `show` (the forecast table per tenor, raw beside fit, annualised, with
  the QLIKE tell), `seed-out` (the v1 seed rows `V`/`D`/`C`/`A`/`P`/`Q`
  the H3 reader will apply in file order — the lookahead law holds:
  nothing at or after `--now-ms` is read) and `compare` (the per-day
  `Σ r²` agreement of two series — the measurement a `--fallback` must
  pass before its days are trusted).

**Why**

- Plan phase H2: the engine's long-tenor numbers must be reproducible
  offline to the bit, and the boot seed must be cut by the same law.

**Impact**

- None on the engine or any worker verb. The `candles-cycle.sh` /
  `engine-wrapper.sh` seed hooks the plan lists for H2 are NOT added: they
  would run for a `har.toml` that has no parser until H3 (whose feed
  descriptor per series, `StrategySet` ownership and `/state.har` are
  operator rulings).

**Migration steps**

1. None. To read the long tenors of a series today:
   `python -m claude_worker.har_seed show --descriptor binance-usdm:btcusdt`.

**Rollback**

- Revert the `HAR:` H2 commit.

## 2026-09-26 — `core_vol::LongVolEngine`: the HAR over whole days, 1–40 d (HAR H1)

**What changed**

- `crates/core-vol/src/long.rs` (new, re-exported at the crate root):
  `LongVolEngine`, the long-tenor sibling of `VolEngine`, fed by the same
  minute-close law but keeping one `Σ r²` per UTC day.
  - A day ring of 64 calendar-contiguous days. A UTC day with no minute
    is pushed EMPTY, and the EMPTY-DAY LAW holds: no return is formed
    across it (the next close only primes), no pair forms through it, no
    arm forms while it is in the 30-day fold window — ABSENT DATA HOLDS
    over biased pairs that would sit in the ring for months (the review
    measured a 40-day silence dragging the raw 1 d σ from 17.3 % to
    11.9 % while "warm"). A silence of 64 days or more clears the ring
    and keeps every pair and fit.
  - The fold over `[1, 7, 30]` completed days, `VolEngine`'s integer
    steps in days; the grid is EVERY whole day `1 ..= 40`
    (`long_tenor_of`), with `ANNUALISE_LONG_1E9` computed at compile
    time by the law that reproduces `ANNUALISE_15M/4H/8H_1E9` (1 d
    19 111 514 854, 1 w 7 223 473 640, 1 M 3 489 269 264, pinned).
  - At each day close every tenor ARMS (its `x` and the fitted forecast
    made from it) and SETTLES its arm from `τ` closes ago against the
    `τ` days since: overlapping, daily-stepped pairs keyed by the
    target's first day, 128 per tenor, a rolling fit per tenor, and a
    QLIKE tell of the raw fold against the fit AS ARMED.
  - Writer accessors (`day_at`, `open_day`, `arm_at`, `pair_at`,
    `qlike_at`) and their exact inverses (`seed_day`, `seed_open`,
    `seed_arm`, `seed_pair`, `seed_qlike`, `refresh`) for the H3 state
    file; a round trip is identical and continues identically (tested).
    A seed the writer could never have produced (a "none" or an
    out-of-range log-vol, a day sum above 1e32) is refused, never
    repaired — the review showed one wrapping the fit's sums.
  - 206 080 B, `#[repr(C, align(64))]`, size-asserted; no allocation
    after `new()` — bench gate 81 (`long_vol_is_zero_alloc`: 60 day
    closes over the whole grid under the guard, 0 B/op).
- `core_vol::ols_fit_1e9` (new, public): `VolEngine::refit`'s closed-form
  OLS lifted into a free function that BOTH engines call, and the QLIKE
  trailing means into a crate-private helper. `VolEngine`'s outputs are
  unchanged: the shared parity fixtures (`claude-worker/tests/fixtures/vol/`)
  are byte-identical and green before and after (gate G3).

**Why**

- The long-tenor plan (vault `docs/research/vol/har-long-tenor-plan-2026-09-19.md`
  §3.1, phase H1) under ruling O-HC5: weekly/monthly tenors plus the
  1–40 d grid that spans a Hypercall option chain, on all 12 series.

**Impact**

- None on the engine: no owner holds a `LongVolEngine` yet (H3 — the
  feed descriptor per series, `StrategySet` ownership and `/state.har`
  are operator rulings). No member reads it; nothing here is a trading
  signal (plan law L4).

**Migration steps**

1. None.

**Rollback**

- Revert the `HAR:` H1 commit.

## 2026-09-26 — `crates/core-settle`: the Hypercall settlement law, replicated; the settlement shadow (HC7)

**What changed**

- `crates/core-settle` (new, no dependencies): the law Hypercall settles
  by, and the window that feeds it.
  - `SettleWindow`: a 1 s sample-and-hold grid over `[T − 30 min, T]`
    (1 800 points; a price from before the window carries in; a print
    stamped after `T`, out of order or non-positive is refused).
  - `median_of_means(samples, order, scratch)`: trim ⌊5 % · n⌋ from each
    tail, ⌊√n′⌋ buckets of ⌊n′/k⌋ (the last takes the rest), the median of
    the bucket means. `BucketOrder::{Sorted, Time}` both, because the
    venue's docs do not say which and the first measurement did not
    separate them.
  - Fixed point ×1e6, `i128` sums, `select_nth_unstable` at every trim
    and bucket boundary (no full sort), stack-only. Bench gate 80: a
    window filled, closed and settled under both orders, 20 expiries,
    0 B/op.
- `claude_worker.hypercall_settle` (new module, never a verb): the
  Python MIRROR of the law (pinned bit for bit against the Rust tests'
  table) and the shadow — for every expiry `hc_payouts` pins, the
  captured index Marks (`hypercall-events.pmlr`) over the window, the law
  under both orders, `settle_err_bps` per expiry, and the HC7 gate line
  per order (median |err| ≤ 1 bp and max ≤ 3 bp over ≥ 20 consecutive
  full-coverage expiries on ≥ 3 underlyings).

**Why**

- HC7 of the Hypercall plan: paper settlement of a held Hypercall option
  must use the venue's own law, and the gate must choose the bucket
  order from data before any member books with it.

**Impact**

- None on the engine: nothing books with the law until the gate passes
  and a member is ruled in (HC11).

**Migration steps**

1. None. To accumulate gate evidence: keep `[hypercall]` capturing
   (HC5), run `hypercall_history` with the provider wallets (HC6), then
   `python -m claude_worker.hypercall_settle`.

**Rollback**

- Revert the HC7 commit.

## 2026-09-26 — worker lanes for Hypercall: the research-store puller, the fee table (HC6)

**What changed**

- `claude_worker.hypercall_history` (new module, `python -m …` — never a
  worker verb): one cycle pulls three public REST lanes under one
  per-hour budget (`CLAUDE_WORKER_HC_BUDGET_PER_H`, default 120) into
  tables beside candles in `candles.db`:
  - `hc_trades` — `/trades` forward from the table's own
    `max(trade_id)` (the venue's exclusive cursor), with the maker and
    taker wallets the WS never sends;
  - `hc_payouts` — `/settlement-payouts` for the wallets named by
    `--wallets` / `CLAUDE_WORKER_HC_WALLETS` (none = the lane is skipped;
    no wallet is compiled in). `--settle-prices` prints the derived
    (underlying, expiry) → S table (S = K ± intrinsic) and flags an
    expiry whose rows disagree — the reference HC7 is judged against;
  - `hc_summary` — one `/options-summary?…&include_rfq_provider_quotes=true`
    per `[hypercall] underlyings` entry: mark, IV, underlying, OI, best
    bid/ask, provider count.
  Host: `HYPERCALL_REST_HOST`. Serialized like every worker invocation
  (`pgrep` first; WAL + busy timeout).
- `frames.VENUE_HYPERCALL = 9`.
- Fees: `pnl_report.FEE_VENUES` accepts `hypercall`; `fees.toml.example`
  carries `hypercall = "0:0"` and `[fees.hypercall] option = "0:0"`,
  UNVERIFIED (ruling O-HC7: the launch configuration — every public trade
  row so far carries zero fees), with the published future schedule as a
  commented fee-on STRESS variant (`option = "2:5"`,
  `option_cap = "5:1250"`; the settlement leg is not expressible yet).

**Impact**

- **Store:** three new tables in `candles.db`, created on first run.
- **Config:** optional env keys; the live `fees.toml` is the operator's
  (see below).

**Migration steps**

1. Operator: add the two `hypercall` lines to `~/multivenue/fees.toml`
   (copy them from `fees.toml.example`) before the first report that
   includes a Hypercall run — an unknown venue table is fatal, and a
   missing one charges the harness default.
2. Optional: schedule `python -m claude_worker.hypercall_history` beside
   the funding lane, with the provider wallets in
   `CLAUDE_WORKER_HC_WALLETS` (the research vault names them).

**Rollback**

- Revert the HC6 commit; the tables are inert without the module.

## 2026-09-26 — the Hypercall ingress is spawned: two threads, `/state` row 10, `--raw-tap hypercall`, the venue metrics (HC5)

**What changed**

- `cli::spawn_hypercall` (paper.rs) runs the HC3 crate whenever the HC4
  discovery selected a chain:
  - `ingress-hypercall` owns the public socket, the tick lane 7 / event
    / opt lane 3 producers and the `"hypercall"` capture (+ the raw-tap
    venue byte 9);
  - `hypercall-poller` runs the REST `/options-summary` cycle on its own
    thread (a request may block up to its deadline) and hands its rows
    to the ingress thread over an SPSC ring. It resolves its host on its
    own thread: a DNS failure ends the poller and leaves the quotes
    running.
  - The bin builds the universe table from the discovered chain and the
    index table from `allocated.hypercall_idx`; the staleness threshold
    is the venue default (500 ms) or `--stale-after-ms hypercall:<ms>`.
  - Hypercall events stay capture-only: the event-lane mask is the v1
    law (`EVENT_LANE_FUNDING`) and the venue has no funding.
- `/state`: `SNAPSHOT_VENUES` 9 → 10; `hypercall` is the tenth ingress
  row, appended after `hyperevm` (append, never reorder). The TUI's
  ingress panel grows one row (derived).
- `--raw-tap` accepts `hypercall` (and `all` includes it).
- Metrics (`/metrics`):
  - the standard per-venue set — `engine_ingress_hypercall_state`,
    `_last_tick_age_seconds`, the 13 §6.4 counters + `_feed_delay_ema_ms`,
    `_capture_{io_errors,records}`, `_coverage_configured`, and
    `engine_ingress_hypercall_options_selected`;
  - the venue family, 31 gauges mirrored from `HcCounters`
    (`cli::HC_METRIC_NAMES`): closes by slow-consumer cause, subscribe
    sets, one-sided / empty / crossed quotes, provider sides, ClockSyncs,
    listings by action, foreign trades, venue errors, the publish-lag /
    quoted-instruments / providers-max / index-age / clock-RTT gauges,
    snapshot requests, and the poller's ok / err / rows / foreign rows /
    snapshots / handoff drops / last round.
  - A worst-case boot (every exec slot live) still fits the fixed
    registry (a test builds it).
- `crates/cli/tests/hypercall_live_smoke.rs` (`#[ignore]`): discovery,
  then the production spawn against the REAL venue for a bounded window,
  WITHOUT the engine (the MX9 shape — nothing binds 9191 or `ai.sock`).
  Run from the Cowork container on 2026-09-26 (4 underlyings × E1 × K4,
  90 s): 2 402 messages, 1 947 BBO ticks + 176 index Marks, 0 parse
  errors, 0 reconnects, one subscribe set, 688 provider sides, 6 polls
  → 48 summary rows, the capture holding every tick. The container's
  egress re-signs TLS, so that run passed `HC_SMOKE_CA_BUNDLE` (test-only;
  the engine trusts only the compiled-in roots).

**Why**

- HC5 of the Hypercall plan: the venue captures once `[hypercall]` is
  configured.

**Impact**

- **On-disk formats:** a boot with `[hypercall]` writes
  `hypercall-{ticks,events,opt-summary,signals,depth}.pmlr` (the uniform
  file set; signals and depth header-only).
- **`/state`:** one more ingress row (the array is append-only; readers
  that index the first nine are unaffected).
- **`/metrics`:** ~50 new names; nothing renamed.
- **Threads:** two more when enabled.

**Migration steps**

1. To enable (the operator's step, at a restart he chooses — the session
   never restarts the engine): add `[hypercall]` to
   `~/multivenue/universe.toml` (O-HC2: the twelve underlyings,
   `expiries = 3`, `strikes = 8`), `cargo build --release -p cli`, run the
   live smoke (above) standalone first, then restart. After the restart:
   `vm_rows_active ≥ 1` on `/state` (the restart law) and
   `engine_ingress_hypercall_options_selected` = the selected chain.

**Rollback**

- Remove `[hypercall]` (the pre-HC5 boot, bit for bit), or revert the HC5
  commit.

## 2026-09-26 — `[hypercall]` universe section, boot discovery, manifests, the descriptor caps (HC4)

**What changed**

- `universe.toml` gains an optional `[hypercall]` section
  (`universe.toml.example`; parser `core-config::universe`):
  - `underlyings` — Hypercall's own names, UPPERCASE `[A-Z0-9]`,
    1..=12 bytes (`SP500`, `SPCX`, `BTC`, …);
  - `expiries` / `strikes` — the shared M2 options-policy law (E 1..=4,
    K even 2..=32, defaults 2 / 8) under this section's own key names;
  - `summary_every_s` — each underlying's REST `/options-summary` period,
    60..=3600, default 300;
  - `underlyings × expiries × strikes × 2 ≤ 1024`: the universe must fit
    the ONE indicative subscribe frame (the venue's D3 law).
  - Allocation: each underlying's settlement index is
    `hypercall-idx:<U>` at ordinal `i + 1` of venue byte 9
    (`AllocatedUniverse::hypercall_idx`). The options are
    boot-discovered and take ordinals from `OPT_ORDINAL_BASE` (513 up)
    in selection order — they reshuffle every boot, like Deribit's.
- `core-config::Config`: `HYPERCALL_WS_HOST` and `HYPERCALL_REST_HOST`
  (both default `api.hypercall.xyz`; `.env.example`).
- Boot discovery (`cli::boot_discovery::run_all`) gains the Hypercall arm
  when `underlyings` is non-empty: ONE `GET /markets` (≈ 4.3 MB, body cap
  32 MiB), one forward scan, then the capped chain per underlying — the
  nearest series OUTSIDE the provider's 2 h pre-expiry blackout × the K
  strikes nearest the venue's index. A configured underlying the venue
  does not list refuses the boot; one that selects nothing marks it
  `any_missing` (fatal live, a warning in paper).
- Manifests and the live descriptor table carry the venue:
  - `options-manifest.tsv`: `hypercall\t<sym>\t<instrument>` rows;
  - `instrument-manifest.tsv`: `hypercall-idx:<U>` rows after the
    `mexc-perp:` block, `hypercall:<instrument>` rows last;
  - the ruleset `DescriptorTable` gets the same rows (its membership is
    the manifest's by construction).
- The capability string law (`ingress_ai::caps_of_descriptor` and its
  pinned Python mirror `claude_worker.channel_map.caps_of_descriptor`):
  `hypercall:` options → `OPT|PRICE`; `hypercall-idx:` → none (the index
  has no tick and no summary, so no ruleset feature can read it).
- `opt-registry` is UNCHANGED: a Hypercall-keyed registry (and a
  premium-denomination field) belongs to the first consumer that needs
  one — the HC11 member, on its own ruling — not to a data-only lane.

**Why**

- HC4 of the Hypercall plan (rulings O-HC1…O-HC10): the O-HC2 universe
  (12 underlyings × E3 × K8 × {C,P} = 576) is chosen at boot the way the
  M2 law chooses Deribit's chain, and every offline consumer resolves
  its reshuffled ordinals through the manifests.

**Impact**

- **Config keys:** one optional section and two optional env keys. A
  file without `[hypercall]` (or with `underlyings = []`) parses to the
  pre-HC4 universe bit for bit; no discovery runs.
- **On-disk formats:** manifest rows appear only with the section.
- **API:** `boot_discovery::run_all`, `options_manifest::render`,
  `render_instruments` and `build_descriptor_entries` each gain a
  Hypercall parameter.

**Migration steps**

1. None. Do NOT enable `[hypercall]` before HC5: until the ingress is
   spawned, discovery would run and the manifests would name instruments
   nothing captures.

**Rollback**

- Revert the HC4 commit.

## 2026-09-26 — `crates/ingress-hypercall`: the Hypercall market-data ingress, data-only (HC3)

**What changed**

- New crate `crates/ingress-hypercall` (ruling O-HC1: no keys, no exec
  arm). Not spawned yet: the bin wiring is HC5, so nothing changes at
  runtime.
  - ONE public WebSocket, one thread: `ClockSync` at connect, then one
    `Subscribe` per channel — `indicative_market_data` naming the whole
    universe in ONE frame (the D3 law: 3 or 12 frames were closed 1008
    within 0.12 s, measured 2026-09-25), plus `index_prices`, `trades`,
    `market_updates`.
  - A slow-consumer close (1008 + a reason JSON) is counted by cause and
    torn down; the reconnect re-subscribes in one frame and asks the
    REST poller for a full snapshot round (quotes that changed in the
    gap are not replayed).
  - A REST poller thread (`GET /options-summary?currency=<U>` over the
    HC2 `HttpsReq`, staggered, default 300 s per underlying) hands its
    `OptSummary` rows over an SPSC ring; the ingress thread captures them
    and pushes them onto opt lane 3 — every file and lane keeps one
    writer.
  - The capture law — BBO-change ticks (one-sided and crossed quotes
    kept), `Trade`, `Mark` on the index syms, `ProviderQuote` (14) for
    multi-provider quotes, the summary rows — is `docs/wire-format.md`
    "Hypercall (HC3)".
  - The drain loop judges progress by frames CONSUMED, not ticks
    published (the I-3 gap of the tick-judged loops), capped at 64 steps
    per iteration.
  - `discovery`: the one-pass `/markets` scan and the capped-chain
    selection (the M2 law via `options-select`, plus the provider's 2 h
    pre-expiry blackout) as pure functions over a trimmed real body; the
    boot wires them at HC4.
- `core_types::ChannelId::ProviderQuote = 14` (NEW, additive): one side
  of one market-maker's indicative quote, for a quote with ≥ 2 providers.
  `venue_seq` packs the provider's index, the side, the provider count
  and the wallet's low 32 bits (`docs/wire-format.md`, the
  `ChannelEvent` table). `ChannelId::from_u8(14)` now resolves.
- Parsers fill in place (the poison law) with in-place object/array
  walkers; the subscribe frame is serialised from parts straight into
  the masked tx buffer; the poller's rows are read in place in the
  handoff slot.
- Gates:
  - bench gate 78: every Hypercall parser, the lookup, the subscribe
    parts and the summary scan, 0 B/op;
  - bench gate 79: the run loop's steady state (real handshake, then
    300 rounds of the measured wire mix incl. a server Ping and the
    handoff) with a real `PmlrCapture`, 0 B/op;
  - fuzz targets `hypercall_ws_frame`, `hypercall_markets`,
    `hypercall_summary` (poisoned start);
  - `make copy-audit` covers the crate (new = 0).

**Why**

- HC3 of the Hypercall plan: the venue's public market data — option
  premia and IV on 12 underlyings incl. tokenised equities — as a
  capture/research lane (O-HC1).

**Impact**

- **On-disk formats:** none until HC5 spawns the ingress (then the
  `hypercall-*` capture files, the label reserved at HC1).
- **API:** additive (a new crate).

**Migration steps**

1. None.

**Rollback**

- Revert the HC3 commit.

## 2026-09-25 — core-net: any-method HTTP/1.1 heads and `HttpsReq`; `HttpsPost` on the shared keep-alive engine (HC2)

**What changed**

- `core_net::http1` gains ONE head writer for every request:
  - `Method` (`Get`/`Post`/`Put`/`Delete`, `#[repr(u8)]`), `Header<'a> =
    (&[u8], &[u8])` and `ReqHead`: method, host, origin-form target, UA,
    optional `Content-Type`, extra headers, keep-alive.
  - `request_head_len` sizes the head; `write_request_head` renders it;
    `write_request` renders the head, then the body.
  - `Content-Length` goes on every method but a bodiless `GET`. A `DELETE`
    may carry a body; Hypercall cancels with one.
  - `write_get_request` / `write_post_request` are thin wrappers and emit
    the same bytes as before (pinned by their golden tests).
  - `HttpErr::BadHead` is new: a CR / LF / NUL in any field (header
    injection), a target that is not visible-ASCII `/…`, an empty host, or
    a bad header name. Refused, never rewritten. A proptest proves no field
    can smuggle a line in.
- `core_net::HttpsReq` is new: a keep-alive HTTPS client for ONE host and
  ANY request.
  - `request(method, target, extra, body_len) -> (status, body_range)`.
    The body is rendered in place (`body_mut`); the head is rendered per
    request flush against it; ONE contiguous slice goes out in ONE write.
  - The User-Agent is `multivenue-engine/1` and every request sends
    `Accept-Encoding: identity`.
- `crate::https_conn::KeepAlive` (private) now holds the connection engine
  that used to live inside `HttpsPost`, **moved unchanged**: the dial, the
  idle-close probe before reuse, the one-exchange cycle, the retire rules
  and the `left_host` law. `HttpsPost` and `HttpsReq` both run on it.
  `HttpsPost`'s public API, wire bytes and allocation profile are
  unchanged: gate 72 is still exactly 2 per post, and its loopback suite
  and the HYPARB arm loopback pass untouched.
- `PostErrKind::BadRequest` is new (`HttpsReq` only): a head refused
  before any byte left, `left_host == false`.
- New alloc gates:
  - 77a: the head writer is 0 B/op.
  - 77b: the `HttpsReq` keep-alive cycle is exactly 2 allocations per
    request, gate 72's rustls record residue.
- New TLS loopback `core-net/tests/https_req_tls_loopback.rs` covers:
  - every method on one connection, and extra headers verbatim;
  - no `Content-Length` on a bodiless GET;
  - idle close and announced close;
  - chunked framing;
  - a 600 KB answer over many records, and `Overflow` when it does not fit;
  - refusals that never dial.

**Why**

- Hypercall's REST surface spans many paths and methods on one host. The
  data plane needs a runtime `GET /options-summary?currency=<U>` (HC3). The
  exec plane will need `PUT` replace, `DELETE` cancel with a JSON body, and
  signed `X-Hypercall-*` headers on MMP reads (HC9).
- `HttpsPost` is POST-only with a boot-fixed path. `boot_http::https_get`
  is boot-only and blocking.

**Impact**

- **On-disk / config / wire formats:** none.
- **API:** additive. `HttpErr` and `PostErrKind` each gain a variant.
  Outside core-net only the `http1_response` fuzz target matched one
  exhaustively (the fuzz crate is its own workspace, so no workspace gate
  builds it); it now covers `BadHead` and checks the framing law on every
  input — `BadHead` exactly when a field would break the head.

**Migration steps**

1. None.

**Rollback**

- Revert the HC2 commit.

## 2026-09-25 — VenueId 9 = Hypercall; tick lane 7, opt lane 3; `VENUE_COUNT` 10 (HC1)

**What changed**

- `VenueId` gains `Hypercall = 9`, appended; the first unassigned byte is now
  10. `core_types::VENUE_COUNT` goes 9 → 10, so every venue-indexed table
  grows by one slot:
  - `VenueId::stale_after_ms_defaults`: Hypercall 500 ms (HC0: the
    quote-stamp feed-delay p99 was 447.6 ms);
  - `core_fill::ACTIVATION_NS_DEFAULT` / `ModelParams`: Hypercall 130 ms
    (HC0, `docs/venue-latency.md` §3);
  - `core-config::exec::VENUE_NAMES`: `hypercall` = 9, reserved so an
    artifact can name it — naming it arms nothing.
- Engine lanes:
  - `engine::NUM_TICK_LANES` 7 → 8: `tick_lane_of(Hypercall) = 7`, for
    option BBO ticks off the indicative feed. Event lanes follow the tick
    geometry, so there are 8 of them.
  - `engine::NUM_OPT_LANES` 3 → 4: `opt_lane_of(Hypercall) = 3`, for the
    REST `/options-summary` mark row.
  - No depth lane and no fill lane: data-only, ruling O-HC1.
  - The bin splits the three new rings and drops their producers (the
    unspawned-venue shape). The ingress spawn is HC5.
- Capture and harness labels:
  - `hypercall` is appended to the capture `VENUE_LABELS` (backtest,
    audit-replay, audit-pnl, capture-catalog). A run without Hypercall
    files is read exactly as before.
  - `hypercall` is appended to the model labels: `--fee-bps hypercall:…`,
    `--latency-ns-venue hypercall:…`, `--stale-after-ms hypercall:…`.
  - The rendered fee table gains a trailing `hypercall` entry: text
    ` hypercall=0:0`, JSON `"hypercall":{…}` after `"hyperevm"`.
  - Capture-catalog `venue_ticks` arrays have 10 entries.
- The backtest merge's lane-ordinal bands are widened from a stride of 8 to
  16 (`LORD_BAND`). Ticks = vi, events 16+vi, depth 32+vi, opt 48+vi,
  synthetic marks 64+vi, regime 80, pool signals 96.
  - At the stride of 8, the ninth label (`hyperevm`) already shared tick
    lord 8 with `pm`'s events and synthetic lord 48 with the regime lane.
    The tenth label would have broken the `(lord, idx)` injectivity the
    sort's totality relies on.
  - Band ORDER is unchanged, so every existing replay merges record for
    record as before.
- `clob-dispatcher::PaperMatcher` refuses venue byte 9 as `unroutable`
  explicitly. Before HC1 the byte sat past the activation table's end and
  the length check refused it; the new slot would otherwise have made it
  routable. `backtest::fill::tradeable_venue_byte(9)` stays `false`.
- Descriptor law, in Rust, Python and the shared
  `descriptor-classes.tsv`:
  - `hypercall:<UND>-<YYYYMMDD>-<STRIKE>-<C|P>` → `option`. Decimal strikes
    are allowed; any other name → `none`.
  - `hypercall-idx:<UND>` → `spot`: the settlement index, capture-only.

**Why**

- Hypercall integration, data-only (plan
  `docs/research/hypercall/hypercall-integration-plan-2026-09-26.md`,
  rulings O-HC1…O-HC10). HC1 is identity and lanes only. The venue is
  present but unconfigured: no ingress thread, no `[hypercall]` section
  yet (HC4 and HC5).

**Impact**

- **On-disk formats:** no PMLR bump. A new venue byte is not a slot-layout
  change. No `hypercall-*.pmlr` file exists until HC5.
- **Config keys:** `exec.toml` venue names accept `hypercall`; nothing arms
  it. `--stale-after-ms` / `--fee-bps` / `--latency-ns-venue` accept the
  `hypercall` label.
- **Wire formats:** `VenueId` byte 9 is assigned.
- **Memory:** three more preallocated rings (tick 1 MiB, event, opt) that
  nothing produces into. The engine drains them empty: two atomic loads
  per lane per iteration.

**Migration steps**

1. None. A boot with no `[hypercall]` section is the pre-HC1 boot in
   behaviour.

**Rollback**

- Revert the HC1 commit. There is no data or config migration to undo.

## 2026-09-24 — `scripts/bin15-flip.sh`: slot 3 PAPER ⇄ LIVE in one command (BIN15 S7-L1)

**What changed**
- `scripts/bin15-flip.sh status|live|paper [--switch-only]` edits
  `~/multivenue/strategy.conf` (`EXEC_TOML`, slot 3 in `ARM_LIVE`; other
  armed slots are kept), `[exec.slot.3]` of `exec.toml` and `bin15.toml`,
  then restarts the engine in a safe minute and checks the boot tells.
- LIVE: `bin15.toml` is a cut of the paper artifact — first line
  `# bin15-flip LIVE cut`, only the four sizing lines changed — and the
  paper artifact waits as `bin15.paper-s7.toml` (the nightly accrual
  reads it while it exists). PAPER puts it back and archives the copy.
- Lines the script switches off in `strategy.conf` carry the tag
  `#bin15-flip# `; backups are `<file>.bak-<stamp>-bin15flip`; the session
  anchor and slot 3's `operator` halt are archived into `logs/` (any
  other halt reason stays in `exec.HALT` for a human, and `live` refuses
  on it); every step is logged to `logs/bin15-flip.log`.

**Operator action**
- None until arming. Do not hand-edit a LIVE cut: flip to paper, change
  the paper artifact, flip again.

## 2026-09-24 — the live arm made restart-proof: SIGTERM drains, account-wide cancel at boot and shutdown, the day cap from the venue, the session bound at cost, exits exempt from the order caps, `request_topup_*` (BIN15 S7-L1)

**What changed**
- `SIGTERM` now takes the SIGINT path (`cli::sigint`): the drain runs —
  member state flushed, `Engine::stop`, threads joined — under a 30 s
  `alarm` (`DRAIN_DEADLINE_S`) that kills a drain that hangs. Before,
  the restart lane's SIGTERM killed the process where it stood.
- `clob_dispatcher::OrderDispatch` gains two defaulted methods:
  `on_shutdown()` (called by `Engine::stop` after the members'
  `on_stop`) and `venue_day_bought(slot) -> Option<(u64, i64)>`.
- `HlExchange::cancel_ours_everywhere` — every resting order whose cloid
  is ours, on any outcome leg, cancelled by oid (asset id read off the
  venue's `#<enc>`). Run at boot before `exec: hyperliquid arm ARMED`
  (new fields `boot_swept`, `boot_sweep_left`) and at shutdown (log line
  `exec: shutdown sweep of resting orders done`).
- The router's day cap adopts the venue's own day spend (`userFillsByTime`
  since 00:00Z, our BUYS per slot, `exec_hyperliquid::dayspend`), never
  lowering its own; the arm reports `reconciled` only once it has read
  it. `HlHttp`'s response buffer is 1 MiB (was 16 KiB) to hold that page.
- The E7 session bound is judged on EQUITY AT COST (spot USDC plus the
  held legs' `entryNtl`) at every reconciliation, legs held or not.
  `clob_dispatcher::HaltSignal::pnl_flat` is renamed `pnl_judged`;
  `SpotBalance` gains `entry_ntl_1e8`; `recon::account_view` returns an
  `AccountView`. A flat anchor file written before reads the same.
- The risk gate never refuses a sell no larger than the slot's holding
  on that leg under `max_order_usd_1e6` or `max_open_orders`
  (`Ledger::held_on_sym_1e6`).
- `exec.toml` `[exec.slot.<n>]` gains two OPTIONAL keys,
  `request_topup_weight` and `request_topup_day_max` (both or neither;
  weight 1 000–100 000, day ≤ 1 000 000): `reserveRequestWeight`
  top-ups of the address request budget, paid from the PERPS balance.
  The boot tell's HALTS line gains `topup=<weight>/<day_max>`. New state
  file `exec-topup.state` beside `exec-budget.state`
  (`<master>\t<day>\t<used>\t<anchor s>\t<cost ×1e6>`).
- `HlExchange::enable_restart_safety` (the operator arm's boot calls it):
  seeding waits for the day-spend read and a clean account-wide sweep.
- `budget::scan_rate_limit` returns `nRequestsSurplus` too and
  `AddressBudget::from_venue` takes it; the SDK vectors are 27 rows
  (`reserve_weight`, `reserve_weight_testnet`) and the self-test checks
  five encoders.
- `LiveArmCounters` gains `sweep_all_cancelled`, `sweep_all_left`,
  `topup_ok`, `topup_failed`, `day_sync_ok`, `day_sync_failed` (272 B;
  `ExecCounters` 568 B, both const-asserted); `/metrics` gains
  `engine_exec_hl_sweep_all_cancelled_total`,
  `engine_exec_hl_topup_ok_total`, `engine_exec_hl_topup_failed_total`,
  `engine_exec_hl_day_sync_ok_total`, `engine_exec_hl_day_sync_failed_total`.
- `budget::scan_rate_limit` counts `nRequestsSurplus` only beside an
  `nRequestsCap` of exactly `10 000 + ⌊cumVlm⌋`.
- `exec_router::Ledger`'s day epoch rolls FORWARD only: a stamp from an
  earlier day is booked into the current day instead of wiping it.
- `scripts/daily-restart.sh`'s 00:20Z accrual passes
  `--bin15 ~/multivenue/bin15.paper-s7.toml` when that paper copy exists
  (the arming step creates it), else `~/multivenue/bin15.toml` as before.

**What to do**
- Nothing for a paper engine: every change is inert without a live
  Hyperliquid slot, except the SIGTERM drain, which every restart now
  runs.
- Before arming a live slot with top-ups: add both `request_topup_*`
  keys to `~/multivenue/exec.toml` and fund the master's PERPS balance.

## 2026-09-24 — slot 0 can arm LIVE (three switches); `hyparb.toml mode = "live"`; `OrderDispatch::try_next_retired`; `evm-live arm-smoke`; `scripts/hyparb-flip.sh` (HYPARB L2–L5)

**What changed**
- `hyparb.toml` accepts `mode = "live"` (live checks: `[mainnet]
  executor`, perp-only coins, USD-quoted traded pools).
- `exec.toml` accepts a live `[exec.slot.0]` on exactly `["hyperliquid",
  "hyperevm"]`; `hyperevm` on any other slot refuses; `NEVER_LIVE_SLOTS`
  is gone. Slot 0's state files: `<exec.toml dir>/hyparb/` (X's HL
  budget and anchor, `hyparb-equity-anchor.state`).
- The wrapper: `HYPARB_LIVE=1` + `HYPARB_TOML` + `ARM_LIVE` naming 0 +
  `HYPEREVM_MAINNET_KEY` (repo `.env`) → `--hyparb <toml>` without
  `--evm-testnet`; exclusive with `EVM_TESTNET` / `EVM_HYBRID`.
- `clob_dispatcher::OrderDispatch` gains `try_next_retired` (default
  none) and `halt_signal_for` (default = `halt_signal`); the router
  drains the first on idle and judges each live slot by the second.
- New verb `evm-live arm-smoke [--no-trade] [--exec --arm-live]`; new
  script `scripts/hyparb-flip.sh status|live|paper`.
- `exec.toml.example` documents slot 0 (shipped paper).

**What to do**
- Nothing for the running engine: every existing config boots as before
  (slot 0 paper). To go live: plan §17.6.

## 2026-09-24 — `hyparb.toml [mainnet]`, `multivenue-engine evm-live`, `scripts/evm-live.sh` (HYPARB L1)

**What changed**
- `hyparb.toml` gains an OPTIONAL `[mainnet]` block (`endpoint`, and
  `executor` once deployed), valid in every mode; the member ignores it
  until live mode (plan §17.3 L5).
- New verb `multivenue-engine evm-live status|deploy|wrap|swap|sweep`
  (wrapper `scripts/evm-live.sh`, zsh): HyperEVM MAINNET by default,
  `--network testnet` for the dry run; every mainnet write needs
  `--confirm`.
- New environment names (repo `.env`, written by the operator):
  `HYPEREVM_MAINNET_KEY`, `HYPERLIQUID_HYPARB_MASTER_ADDR`.
- `exec_hyperevm::Network` gains `Mainnet`; `EvmArm::new` refuses it
  and `EvmArm::new_mainnet` takes a `MainnetAuthority`.

**What to do**
- Nothing for the running engine. Before the first mainnet verb: the
  operator's steps 1–5 of the live plan (the wallet, the two `.env`
  lines, the funding), then add `[mainnet]` to `~/multivenue/hyparb.toml`.

## 2026-09-24 — `hyparb.toml` refuses `halt_on_gain/loss_usd_1e6`; `/state` `hyparb.pnl_halt` and `engine_hyparb_pnl_halt` removed (HYPARB L0)

**What changed**
- Ruling O-HL5: the P&L stop is LIVE-only. The paper member's session
  stop (added at the go-live prep, `1d6960b`) is removed; its marked
  level stays (`/state` `hyparb.pnl_session_usd_1e6`, gauge
  `engine_hyparb_pnl_session_usd_1e6`).
- `[hyparb]` refuses `halt_on_gain_usd_1e6` / `halt_on_loss_usd_1e6` by
  name, with a pointer to `exec.toml [exec.slot.0]`.
- `/state` `hyparb` loses `pnl_halt`; the metrics family loses
  `engine_hyparb_pnl_halt` (27 counters, 36 gauges); the boot tell loses
  `pnl_stop_usd_1e6=`.

**What to do**
- Remove the two lines from `~/multivenue/hyparb.toml` in the SAME
  deploy as a release binary at or after L0 (done on this host with a
  `.bak`). A binary at `1d6960b` still accepts them; a binary at L0
  refuses them.

## 2026-09-24 — the BIN15 entry's R2-exact polls and today's-law control: `ctl_*` sidecar keys, 17-column `first_fires.tsv` (BIN15 S5b)

**What changed**
- The persistence run, for `entry_persist_polls` above 1, counts POLLS
  only: the reprice a new snapshot of the preferred leg triggers on its
  arrival (doc 27 R2's poll instant exactly). A poll extends the run,
  starts one on a flipped side, or breaks it on a failed test. The
  reprices between polls (marks, the other leg's ticks) never move the
  run; they may fire only a run already complete (a retry after a
  refused emit). S5's mark rescue, where a mark could start a run on a
  snapshot that failed at its arrival, is gone. `entry_persist_polls = 1`
  (and absent) is unchanged: every reprice is judged, the pre-S5 law.
- The elapsed ceiling follows the polls: past it, a poll that would begin
  or break a run closes the instance, and so does any reprice between
  polls when no run is under way.
- The member records a second counterfactual, the CONTROL: the first
  reprice on which today's law held (the artifact's floor, the compiled
  default bound `core_config::bin15::E_ENTRY_1E6_DEFAULT`, persist 1, no
  ceiling). `Bin15Params` gains `entry_control_e_1e6`; the cli sets it
  at boot, and it is not an artifact key (the grammar is unchanged).
  `FamilyState` gains `entry_ctl_ok_ts`, `entry_ctl_ok_px_1e6` and
  `entry_ctl_ok_yes`; both counterfactual prices narrow to `i32` so the
  struct stays 512 B.
- The `--emit-detail` sidecar gains keys (readers by key ignore them),
  but one existing key changes shape (below):
  - `bin15_entries` rows gain `ctl_fire_ts_ns`, `ctl_fire_px_1e6` and
    `ctl_fire_is_yes` (`null` when today's law never held on the
    instance).
  - `bin15_first_fires` rows gain `ctl_ts_ns`, `ctl_offset_s`,
    `ctl_is_yes` and `ctl_px_1e6`. The block now holds every instance
    the member did not enter on which EITHER test held, so the artifact's
    own `ts_ns`, `offset_s`, `is_yes` and `px_1e6` may be `null` — which
    a pre-S5b worker cannot read (it fails on the row).
  - `bin15_entry_law` gains `control_e_entry_1e6`.
- `bin15_accrue`: `first_fires.tsv` rows grow from 13 to 17 columns
  (`ctl_ts_ns`, `ctl_offset_s`, `ctl_is_yes`, `ctl_px_1e6`; a test that
  never held is `0 / 0 / -1 / 0`, on either side). 13-column rows still
  read, with no control. The merge keeps the earliest of EACH
  counterfactual on its own, and the store is ordered by instance
  start. `report` pairs the entries with the artifact's unpersisted
  trigger and with today's law, on the SAME instances and on the ones
  the member declined, and says the §5 fill bar is gated at the first
  live step (ruling 2026-09-24).

**Why**
- Plan 28 S7 moves `e_entry` along with the persistence law. From then
  on the artifact's own first fire is no longer today's law, and doc 27
  §5's CONFIRM bar ("≥ +5 pts over today's law on the same instances")
  needs today's law recorded on those instances. The control is that
  record, from the same reprice stream.
- R2-exact: the research's polls were book snapshots judged at their
  arrival. The mark rescue let a run begin between polls, which R2
  never did.

**Impact**
- An artifact without the S5 keys behaves exactly as before.
- The harness and the worker ship together (one commit): a pre-S5b
  worker fails on a `bin15_first_fires` row with a null `ts_ns`, and a
  pre-S5b harness writes no control.
- With `entry_persist_polls` above 1, a run can start only at a
  snapshot's arrival, so some entries come one poll later than under S5.
- The next accrual rewrites `first_fires.tsv` at 17 columns. A pre-S5b
  reader refuses a 17-column row, so run the matching worker.
- Wire formats: none. Config keys: none.

**Migration steps**
1. None. The next 00:20Z accrual widens the store.

**Rollback**
- Revert the commit, then move `first_fires.tsv` aside before a pre-S5b
  worker reads it. Cutting it back to 13 columns is not enough: a row
  on which only the control held (`ts_ns` 0, `px_1e6` 0) would reach
  the S5 report as a declined fire at price 0, which it cannot score.

## 2026-09-24 — the BIN15 coverage entry's persistence and elapsed ceiling: `entry_persist_polls`, `entry_elapsed_max_ns`, the counterfactual first fires (BIN15 S5)

**What changed**
- `bin15.toml` gains two optional keys; the grammar knows 27.
  - `entry_persist_polls` (`[1, 8]`, absent = 1). The coverage entry
    fires only once its price test (the floor, and `ask ≤ belief −
    e_entry`) has held on this many consecutive distinct book snapshots
    of the preferred leg, on one side. A snapshot is one write of the
    leg's touch: the `l2Book` push, ~5.3 s apart while the venue
    publishes the outcome `bbo` one-sided (BIN15 O8). The run is judged
    per snapshot, as the research's polls were (except its first poll,
    which a mark may establish mid-snapshot). A new snapshot extends
    the run when the test holds and breaks it when it fails. A reprice on
    the same snapshot (a mark) neither extends nor breaks a run; it can
    only start one, on a snapshot that failed at its arrival. A flip of
    the preferred side starts a new run. A reprice that never reaches
    the gate (a stale or one-sided book, a take in flight) is not a
    poll.
  - `entry_elapsed_max_ns` (absent = 0 = no ceiling; otherwise under
    `tau_ns`). A run must BEGIN within this long of the instance's start
    (expiry − 900 s). Past it a run already under way may still fire; a
    reprice that would begin a run, or break one, closes the instance
    for good, counted once.
  - Both absent (every artifact before S5) is today's law bit for bit:
    the first passing reprice fires.
- The member: `Bin15Params` gains both keys. `FamilyState` gains the
  run (`entry_hits`, `entry_yes`, `entry_last_touch_ts`), the ceiling's
  flag (`entry_closed`) and the counterfactual (`entry_first_ok_ts`,
  `entry_first_ok_px_1e6`, `entry_first_ok_yes`); it stays 512 B.
- Counters: `skipped_entry_persist` (per reprice) and
  `skipped_entry_elapsed` (per instance), published as
  `engine_bin15_skipped_entry_{persist,elapsed}_total` on `/metrics`.
  The bin15 family goes from 31 to 33 counters. The harness's
  `member: bin15` line prints both.
- The boot tell's `entry=` term adds `&persist<n>` (from 2) and
  `&el<=<s>s` when they are set. An artifact without them prints the
  pre-S5 line exactly.
- The `--emit-detail` sidecar gains additive keys; readers by key
  ignore them.
  - `bin15_entries` rows gain `first_fire_ts_ns`, `first_fire_px_1e6`
    and `first_fire_is_yes`: the counterfactual on that instance. That
    is when the entry's price test first held, the ask then, and its
    side — the pre-S5 law's entry attempt, logged and never traded.
    The price test is the artifact's own (its floor and `e_entry`): the
    counterfactual differs from the member only in persistence and the
    ceiling.
  - A new block, `bin15_first_fires`, holds the same for every instance
    the old law would have bought and this window's member did not
    enter (the persistence law or the ceiling declined it). Its rows
    are labelled like an entry.
  - A new key, `bin15_entry_law` (`persist_polls`, `elapsed_max_ns`):
    the law the member ran under. The harness's `member: bin15` line
    prints it as `entry=`.
- `bin15_accrue` keeps a third store,
  `~/multivenue/worker/bin15/first_fires.tsv` (13 columns, worker
  state, never git; `--first-fires` sets the path on `accrue`, `report`,
  `status` and `relabel`).
  - One row per outcome, stamped with the entry law it was accrued
    under. Across window cuts the earliest fire is kept. A row accrued
    under another law only lends its label, and an incoming entry of an
    instance stored under another law is not merged (counted), so a
    stored pair never mixes two laws.
  - Venue-clocked only: a sidecar off the venue clock drops its fires
    and says how many. `relabel` fills an unknown label from the
    captures, as it does for the entries.
  - `report` prints the old law beside the PAPER entries, one block per
    entry law: on the SAME instances (hit, equal-dollar EV per $1, mean
    price) and on the instances the member declined. Under the old law
    itself the pairs are identical by construction and are only
    counted. `status` counts the store.
- `bin15_fit.KNOBS` and `bin15.toml.example` carry
  `entry_persist_polls = 1` and `entry_elapsed_max_ns = 0`, so the
  example stays the old law.

**Why**
- Plan 28 S5 (doc 27 R2): the persistence rule is the one entry
  hypothesis the research left standing. Ruling O-4 measures it in ONE
  paper run, against the old law's first fire on the same instances,
  rather than on a second paper family.

**Impact**
- With the keys absent nothing changes. The persistence count and the
  ceiling never bind, and every existing coverage test is unchanged.
- An artifact that sets them changes WHICH instances are bought and
  WHEN: fewer entries, and later. `skipped_entry_persist` counts the
  reprices that waited. A closed instance is silent after, so
  `skipped_entry_price` stops counting its refusals too.
- The accrual replays each closed day under the artifact in force at
  00:20Z, so the day an artifact changes is the first day accrued under
  the new law. The report keeps the laws apart.
- The accrual writes one more file. `entries.tsv` keeps its width.
- Wire formats: none. Config keys: two, both optional.

**Migration steps**
1. None for the defaults.
2. To measure the rule (plan 28 S7): set the keys, and any
   `e_entry_1e6`, in `~/multivenue/bin15.toml` by hand. Restart at a
   safe minute and read the boot tell's `entry=` term. The next 00:20Z
   accrual starts `first_fires.tsv`.

**Rollback**
- Remove the two keys from the artifact to return to the old law.
- To revert the commit, remove the keys FIRST: a pre-S5 binary refuses
  an artifact that carries them (unknown keys are refused).
  `first_fires.tsv` is inert to older readers.

## 2026-09-24 — the BIN15 accrual stores on the venue's law and clock: `bin15_accrue relabel`, 12/14-column rows, G6.1 on the venue label, the pnl-report label tag (BIN15 S4)

**What changed**
- `claude_worker.hip4` mirrors the settlement law of
  `cli::backtest::binary`: `settle_reference_1e6`, `payout_1e6`,
  `y_next_strike`, `link_successors`, `Instance` and the evidence
  constants. A new shared fixture pins it: `settle-1`, 532 queries,
  with the gap bound tested to the nanosecond.
  `backtest::binary::tests::the_settlement_law_matches_its_python_mirror`
  writes the expected file under `BIN15_SETTLE_WRITE=1`, and pytest
  asserts it.
- New `claude_worker.bin15_tape` reads a capture on the VENUE clock. A
  capture is a `run-*` directory, or a `part-*` pull with its
  `anchor.json`. The reader returns:
  - marks per underlying;
  - created rolls, named by their slot descriptor;
  - the span the pre-S2 anchor law gave the run, and that law's offset
    against the venue (`delta_ns`).

  A capture without v3 venue time is not a tape.
- `bin15_accrue relabel [--pull DIR]… [--runs DIR]… [--dry-run]`
  corrects both stores IN PLACE. Before writing, it keeps a
  `.bak-relabel-<UTC>` copy of each file it changes. Every engine row
  that a capture covers gets:
  - LAW E-11's `y`, with the old label kept as `y_engine`;
  - the venue-published `y_next_strike`;
  - its stamp moved onto the venue clock, with `offset_s` recomputed.

  Rows that no capture covers are kept and counted. A rerun converts
  nothing.
- Store formats (worker state, never git): ledger rows go from 10 to 12
  columns and entry rows from 12 to 14. The two new columns are
  `y_engine` and `y_next_strike`.
  - **The width is the law.** A wide row is on the venue's law and
    clock. A narrow row carries the engine's label on the anchor clock,
    is never scored as the venue's, and waits for `relabel`.
  - Readers accept 8/10/12 columns (ledger) and 11/12/14 (entries).
  - The accrual writes wide rows from a sidecar whose runs all say
    `"wall":"venue"` (an S2+ binary, which settles on E-11) and narrow
    rows otherwise.
  - Both merges let a venue row replace an engine row with the same
    identity, never the reverse.
- G6.1 (`bin15_ledger.calibration`) scores only settled VENUE rows
  priced outside the settlement window (`tau_ns ≥ W/3`, S3). The
  calibration lane prints how many rows it left out. A relabelled row
  priced BEFORE S3 carries `τ + W/3 ≥ W/3`, so its last minute cannot be
  told apart and stays in LATE.
- `y_engine = -1` means the row has no engine label: it was accrued on
  the venue law, or the engine could not settle it. On a row that an
  S1+ binary accrued from an anchor-clocked run, `y_engine` is E-11
  read on that clock. A venue row whose `y` is unknown takes the
  captures' label when a later relabel finds the evidence; a known
  label is never changed.
- `bin15_accrue report` prints the venue label's numbers first, then the
  label each entry was accrued with beside them; the two are never mixed
  in one number. It adds a stake-weighted EV-per-$ line. Entry phases
  are keyed on the pricing horizon, as the engine keys its recalibration.
- `pnl_report`: the day JSON gains `bin15_label`, read from the audits'
  own settlement lines:
  - `venue` when the binary prints `law=twap[T-w,T]`;
  - `engine(T,T+60)` for a pre-S1 binary;
  - `mixed` when the units disagree;
  - absent when no HIP-4 instance settled.

  The bin15 summary line carries `label=…`.

**Why**
- The stores were labelled with the minute AFTER the expiry, on a
  harness clock 22–42 s early (S1, S2). The runs are archived after a
  day, so the correction is made in place from the captures on disk
  (plan 28 S4, ruling O-3).

**Impact**
- The worker's own readers (G6.1, `report`, the merges) read the
  venue's label and never mix it with the engine's. Research readers
  that parse the stores by column position ignore the width law; they
  must filter on the row width before they quote a number. Findings
  scored on the engine's label are re-scored in the vault (doc 28-A).
- The shared `settle-1` fixture pins `settle_reference_1e6` and
  `payout_1e6`. `link_successors` and the tape reader are pinned by the
  worker's own tests. The first live relabel was also cross-checked
  against the S1–S3 harness on one day, 20/20 instance labels.
- Wire formats and config keys: none.

**Migration steps**
1. Run `python -m claude_worker.bin15_accrue relabel --pull <pull root>
   --runs ~/multivenue/logs` once.
2. Re-run it after any accrual made by an older binary. Rows that
   binary writes are narrow and convert on the next run.
3. Never run it while an accrual is running; keep clear of the 00:20Z
   slot. The lane refuses to replace a store that changed while it was
   reading the captures, and says so; rerun it.
4. Never accrue one day under two clock laws into the same store: an
   old binary then an S2+ one, or a relabel then a re-accrual. The
   ledger dedupes on the stamp, so rows of the same day on two clocks
   do not collide and would be counted twice.

**Rollback**
- Restore the OLDEST `.bak-relabel-*` copy of each store, then revert
  the commit. Later backups already hold 12/14-column rows, and the old
  readers refuse those. Rows accrued after the relabel are lost unless
  they are re-accrued.

## 2026-09-24 — the BIN15 pricer prices the venue's settlement window: horizon `τ − 2W/3`, the running TWAP inside the window, τ floors on the clock (BIN15 S3)

**What changed**
- `strategy_bin15::price::pricing_horizon_ns(to_expiry, twap)` — the
  variance-time of the TWAP over `[T − W, T]` (LAW E-11) seen `τ` before
  the expiry: `τ − ⌊2W/3⌋` while the window lies ahead, `⌊⌊τ²/W⌋·τ/3W⌋`
  inside it, `τ` for `W = 0`. The member priced `τ + W/3` (a window
  AFTER `T`). `twap_moneyness_1e9` and `fair_value_twap` (ruling O-2):
  inside the window the member prices `X = A_known + τ·S − W·K` against
  `S·σ·√(τ³/3)`; outside it `fair_value_twap` IS `fair_value` at the
  horizon. `fair_value` is arithmetically unchanged (its Φ →
  recalibration tail is now the shared `fair_of_d`).
- The member: `FamilyState` gains `twap_sum: i128`, `twap_last_ts: u64`
  and `twap_gap: u8`, zeroed at every roll — one more cache line (448 →
  512 B, now const-asserted).
  `on_mark` folds the mark being replaced into every live window it
  reaches through `core_types::binary_twap_segment` (the harness's own
  arithmetic); `try_reprice` prices through `fair_value_twap` and HOLDS
  (`skipped_stale`) inside a window whose evidence has a hole — an open
  this instance never saw a mark in force at, or a piece longer than
  `core_types::BINARY_SETTLE_MARK_GAP_MAX_NS` (10 s; the harness's
  `binary::SETTLE_MARK_GAP_MAX_NS` now re-exports it).
- `tau_min_take_ns` / `tau_min_quote_ns` are judged on the TIME TO
  EXPIRY. They were judged on the pricing horizon, so at `W = 60 s` the
  take arm closed at 40 s to expiry, not the configured 60 s.
- `price::Fair` gains `horizon_ns` (the horizon the price was computed
  at); the member stores it as `last_tau_ns` instead of re-deriving it.
  The view's `tau_s` (`engine_bin15_f<i>_tau_s`) is that horizon ROUNDED
  UP to whole seconds (it floored): inside the window the horizon is
  sub-second for the last ~22 s, and `0` is "never priced". `backtest
  --member`: the sidecar ledger row's `tau_ns` is the member's own
  `last_tau_ns` (it was re-derived as `expiry − wall + twap/3`).
- Python mirror `claude_worker.bin15_ref`: `pricing_horizon_ns`,
  `twap_moneyness_1e9`, `fair_value_twap`, `binary_twap_segment`,
  `binary_settle_open_ns`; `Fair` gains `den_1e9`. New parity fixture
  `parity-2` (records `T` / `S` / `M` / `W`, 1 224 ops), bit for bit on
  both sides. `parity-1` is untouched: nothing it pins moved, so ruling
  O-5's regeneration was not needed. The alloc gate for the member
  (`bin15_member_roll_tick_reprice_take_is_zero_alloc`) gains an
  in-window phase, so the running-TWAP branch is measured at 0 B too.

**Why**
- The venue settles on the TWAP ENDING at `T` (LAW E-11). With the
  window after `T` the horizon was 60 s too long at `W = 60 s` — a lead's
  `d` too small by `√((τ + 20 s)/(τ − 40 s))`, ×1.32 at `τ = 120 s` — and
  the member was blind inside the last minute, where the average is
  partly decided (plan 28 S3, ruling O-2).

**Impact**
- Every `p̂` of a TWAP-settled family moves, most at small `τ`; inside
  the last minute `p̂` follows the running average. At the shipped
  floors (take 60 s = `W`) nothing fires inside the window: the
  in-window price is the ledger's and `/state`'s.
- Takes between 40 s and 60 s to expiry, which the horizon-judged floor
  allowed at `W = 60 s`, no longer fire.
- Recalibration phases are keyed on the horizon, so each phase boundary
  now falls 60 s EARLIER in an instance's life: EARLY → MID at 640 s to
  expiry (was 580 s), MID → LATE at 280 s (was 220 s). The recal tables
  shipped and installed today are the identity, so they correct nothing
  twice — but every table FITTED on `d` (the installed Student-t
  `phi_lut`, `scale_1e9`) was fitted on `d` at the old horizon, which S3
  rescales by `√((τ + 20 s)/(τ − 40 s))` (×1.03 at 900 s … ×1.32 at
  120 s). Those fits must be redone on rows an S3 binary priced — an
  operator action, and a precondition before slot 3 is re-armed live on
  S3 prices; the DISTX re-proof checks the integer price map
  (`p_raw = Φ̂(d)`), not calibration.
- On-disk formats: the ledger's `tau_ns` keeps its meaning — the horizon
  the price was computed at. Rows from an S3 binary carry the new
  horizon; rows before it carry `τ + W/3`. A row priced inside the
  window reads `tau_ns < W/3` (20 s at `W = 60 s`; a row before S3 never
  does, since its `τ + W/3 ≥ W/3`). Config keys, wire formats: none.
  Metrics: none renamed; `skipped_stale` also counts the in-window
  evidence hold, and `tau_s` rounds up (above).

**Migration steps**
1. Rebuild and restart at a safe minute (O-6). The live member prices on
   the new law from the boot.
2. A calibration or DISTX re-proof that predicts the engine's `p̂` from a
   ledger row must price through `bin15_ref.fair_value_twap`, and must
   drop rows priced inside the window (`tau_ns < W/3`): their known
   average is not a ledger column.
3. Until BIN15 S4's readers land, `bin15_ledger.calibration` files rows
   priced inside the window in LATE (their `p̂` carries a partly decided
   average and flatters it); S4 leaves them out of G6.1 and counts
   them. Read no G6.1 verdict over S3 rows before S4.

**Rollback**
- Revert the commit and restart. Rows accrued in between keep the new
  horizon in `tau_ns`.

## 2026-09-24 — the harness WALL clock is the venue's for v3 captures with a Hyperliquid lane (`backtest`, `--member`, `audit-pnl`) (BIN15 S2)

**What changed**
- New `cli::backtest::clock`: `VenueOffsetFit` (a fixed `[i64; 64]`) takes
  the first 64 Hyperliquid ticks whose `venue_time_ms > 0` in file order
  and its median of `venue_time_ms·1e6 − ts_ns` is the run's offset;
  `RunClock::wall_of` maps a capture stamp to its WALL instant —
  `ts + offset` on such a run, the old `epoch + (ts − ts_first)` (clamped
  at the anchor) otherwise, bit for bit. A fit that would put the run's
  first record more than `EPOCH_SKEW_TOLERANCE_NS` (2 s) BEFORE the run
  directory's epoch — impossible for a true clock, since the directory is
  named at boot — is dominated by stale snapshots and is REFUSED: the run
  keeps the anchor law and says `wall=anchor VENUE-FIT-REFUSED …`. The
  refusal is judged on the FIRST SAMPLED HL stamp, so `backtest` and
  `audit-pnl` (which pin different first records) cannot disagree about
  it.
- Detail sidecar, ADDITIVE: each `runs[]` entry gains `"wall":"venue"` or
  `"wall":"refused"`; the key is absent on the anchor law, so an anchor
  root's sidecar is byte-identical.
- `backtest` / `backtest --member`: every merged record's `wall_ns` comes
  from the run's `RunClock`; `audit-pnl`: the same clock for its merge,
  the option settlement index stamps, the capped-fee index book and the
  HIP-4 schedule (one mapping, where it used to restate
  `epoch + (raw − ts_first)` four times). The VIRTUAL clock (replay
  order) is untouched.
- Stderr: `run[i] … wall=venue offset_ns=<n> (the anchor law ran <s> s
  early)` (backtest), the same tell on the member summary's
  `run-<epoch>:` line and as `audit-pnl: run-<epoch>: wall=venue …`.
  SILENCE means the anchor law (no venue stamp), so a capture without
  one reports byte for byte as before.
- Worker: `claude_worker.hip4.venue_offset_first_n` mirrors the harness's
  FIT exactly (integer median of the first 64 stamped pairs; the refusal
  below is the harness's own);
  `hip4.venue_offset_from_samples` is `venue_wall_offset_ns`'s own law,
  now a pure function. Shared fixture `tests/fixtures/bin15/clock-1.*`
  (the expected offset written by `crates/cli` under
  `BIN15_CLOCK_WRITE=1`, generator `gen_clock_input.py`).

**Why**
- The run directory is named at boot and its first tick lands 10–25 s
  later, so the old law ran 23–42 s EARLY against the venue on this host
  (vault doc 27 §4; median 24.75 s). LAW E-11's window `[T − 60 s, T]` on
  that clock is a different minute. On the 2026-09-13→23 captures the
  first-64 median agrees with the worker's 4 000-sample law within
  0.34 s on every run.

**Impact**
- Every WALL instant on a v3 root with an `hl-ticks.pmlr` lane moves
  later by that run's error: HIP-4 settlement windows, a member's `now`
  (it reads wall instants), UTC-day binning near midnight, and — on
  roots that also carry Deribit — the option settlement and fee-index
  instants. Roots without HL venue stamps (v2, or no HL lane) are
  byte-identical.
- The regime replay anchors its minute grid on the FIRST record's wall
  instant and walks the virtual clock from there (`RegimeReplay::build`),
  so on a multi-run venue-clock root a later run's regime grid is off
  its own wall instants by the difference between the two runs' anchor
  errors (seconds; one run, one window: exact). A window's seed files
  are still cut at the old-law instant (`window_root`'s cut epoch), which
  now sits up to ~40 s before the replay's first wall instant — no
  look-ahead, at most a minute's seam.
- **The standing 8-window VM pool guard MOVES, by design.** The pool
  (`~/multivenue/worker/windows/`, 2026-09-05) is v3 with an HL lane, and
  its anchor law ran 24.4–24.6 s early on every window. Re-run on the
  same command (`backtest --ruleset …/fde6f733….json --replay-dir
  ~/multivenue/worker/windows/ --split 0/100 --emit-detail …`): schema-1
  `188d18e3b1ded762…` → `23bb8e843c08ce1d5c2d18b6382f86b4d9491cfbe35f4e718563a829e34dfe9b`,
  detail sidecar `5868f723dacb0c2e…` → `3b986d43d2dc462b…`. The whole
  difference is the regime replay: its seed rows and minute grid follow
  the wall clock (seed rows 10 745 → 10 738, minutes 1 171 → 1 170), the
  fast profile's labels shift and the vm fires 98 times instead of 97
  (the P&L consequence is in the vault, BIN15 session log 29). The live
  detector runs on true wall minutes, which is what the replay now reads;
  the new hashes are the pool's reference from here on.
- **The accrual stores and the cutover.** Rows a S2 binary accrues from
  a window whose sidecar says `"wall":"venue"` are on the venue clock
  already; rows accrued before S2, or from a window that is anchor-clocked
  (no HL venue stamp) or `"wall":"refused"`, are on the anchor clock. `bin15_accrue relabel` (BIN15 S4) converts only rows that do
  not yet carry its columns, so it may be re-run after any accrual; do
  NOT re-accrue a pre-S2 day with an S2 binary into the same store — the
  ledger dedupes on `(ts_ns, family, outcome)` and the re-clocked stamp
  is a new key (the entries store dedupes on the outcome and is safe).
- On-disk formats, config keys, wire formats: none.

**Migration steps**
1. Rebuild the release binary before trusting any replay (G0).
2. The accrual stores are reclocked in place by
   `claude_worker.bin15_accrue relabel` (BIN15 S4).

**Rollback**
- Revert the commit.

## 2026-09-24 — HIP-4 settlement: the TWAP window ENDING at the expiry, time-weighted; the venue-published cross-check; `bin15_ledger`/`bin15_entries` gain `y_next_strike` (+ `settle_px_1e6`) (BIN15 S1)

**What changed**
- `cli::backtest::binary::settle_value` settles on `TWAP[expiry − twap,
  expiry] >= strike` — LAW E-11 (`docs/risk-policy.md` "E8") — instead of
  the mean over `[expiry, expiry + twap]`. The mean is TIME-weighted: each
  mark counts for as long as it was the mark (the last mark before the
  window carries into it, the last one inside carries to the expiry, `dt`
  clipped at the edges, `i128` sum, one truncating divide).
  `SETTLE_MIN_MARKS` (3) is unchanged and counted INSIDE the window, and
  the evidence is now strict: a mark must be in force at the window's
  open and no piece spanning the window (the carry-in and the carry to
  the expiry included) may exceed `SETTLE_MARK_GAP_MAX_NS` (10 s), or the
  instance is unsettleable (counted — the old law settled on whatever
  marks the window happened to hold). `twap_ns == 0` (native dailies)
  still reads the last mark at or before the expiry. New shared pieces: `core_types::binary_settle_open_ns`,
  `core_types::binary_twap_segment`; `binary::settle_reference_1e6` (the
  TWAP itself) and `binary::payout_1e6` (the one `>=`).
- `BinaryInstance::settle_ns()` is gone; `settle_open_ns()` (= expiry −
  twap) replaces it. The value is knowable AT the expiry, so the harness
  schedule settles a slot at `expiry` (`BinarySettle.settle_ns ==
  halt_ns`), and an instance is settleable once the window reaches its
  expiry (was expiry + twap). An order still resting on the slot at that
  instant is cancelled and counted in `settled_sym_orders_canceled` (the
  venue clears the book at `T`; with halt == settle the F12 guard would
  never see the sym). The successor trades from `T` — before, the harness
  held a rolled slot halted for the first `twap` of every new instance,
  which the venue never did. `instances_from_events` keeps ONE instance
  per outcome (a boot re-announces the live instance), as audit-pnl's
  collector always did.
- The venue-published cross-check: `BinaryInstance.next_strike_1e6` (the
  successor's strike = the venue's settlement price, linked by
  `binary::link_successors` — same slot, created between the window's
  open and `expiry + 120 s`), `BinaryInstance::y_next_strike()` (`1e6` /
  `0` / `-1` unknown or a tie), and two counts on
  `BinaryRegistration`: `next_strike_checked`,
  `settle_disagree_next_strike`.
- Stderr: `binary: instances=… settled=… unsettleable=… law=twap[T-w,T]
  next_strike_checked=… settle_disagree_next_strike=…` (backtest and
  `--member`); audit-pnl's `bin15 settlement table` line gains the same
  three fields. A non-zero `settle_disagree_next_strike` is a finding.
- Detail sidecar, ADDITIVE (`detail_version` stays 7): every
  `bin15_ledger` row gains `y_next_strike`; every `bin15_entries` row
  gains `y_next_strike` and `settle_px_1e6` (the TWAP `y` was read from,
  `null` with `y`). `binary::settle_values_by_outcome` →
  `settle_labels_by_outcome` (`BinaryLabel { value_1e6, px_1e6,
  y_next_strike }`).

**Why**
- The venue's rules text names the minute BEFORE the expiry (vault doc 27
  R0), and the account's own 15 venue settlements of 2026-09-19 agree with
  it 15/15 (the old window 14/15; vault doc 29 S0). The old label was
  wrong on ~8 % of instances.

**Impact**
- On-disk formats: sidecar keys added (above). The worker's readers read
  by key and ignore them until BIN15 S4 teaches the accrual to store them
  (with defaults, so an older binary's sidecar still reads).
- Config keys: none. Wire formats: none.
- Every replayed `y`, every bin15 replay P&L and every audit-pnl binary
  settlement on a HIP-4 root moves on the instances whose two windows
  disagree. Roots with no `InstrumentRoll` are byte-identical (no
  schedule is built).

**Migration steps**
1. Rebuild the release binary before trusting any bin15 replay (G0).
2. **This law needs the venue clock (BIN15 S2).** Until S2, the harness
   wall clock runs 23–42 s early against the venue, so `[T − 60, T]` on
   the harness clock reads roughly `[T − 36 s, T + 24 s]` of the venue's
   marks. Judge no label change before S2 is in the binary.
3. The accrual stores are relabelled in place by `claude_worker.bin15_accrue
   relabel` (BIN15 S4), which keeps the old label as `y_engine`.

**Rollback**
- Revert the commit. The relabelled stores keep `y_engine`, so the old
  label stays readable.

## 2026-09-24 — the HYPARB executor's bytecode: `hyperswapV3SwapCallback` (HYPARB H9d)

**What changed**

- `contracts/hyparb-executor/HyparbExecutor.{bin,runtime.bin}` are
  regenerated: the contract answers `hyperswapV3SwapCallback` (Hyperswap
  V3 pools) besides `uniswapV3SwapCallback` and `algebraSwapCallback`.
  Creation bytecode 2,335 → 2,346 B; `evm-testnet deploy` sends the new
  bytes (the binary embeds them).

**Impact**

- An executor deployed from the H7b bytes reverts every swap against a
  Hyperswap V3 pool (the testnet battery pool is one). Paper is
  unaffected; nothing on mainnet was ever deployed.

**Migration steps**

1. `evm-testnet deploy` from a binary built at or after H9d, then set
   `[testnet] executor` to the new address and `mint` its inventory.
   The old executor keeps its minted testnet tokens (there is no sweep
   verb; the contract's owner-only `sweep` would move them).

**Rollback**

- Revert the H9d commit (the deployed contract stays where it is; point
  `[testnet] executor` back at the old address).

## 2026-09-23 — `engine_hyparb_evm_no_wallet_total` → `_superseded_total`, `engine_hyparb_evm_dark`; the shadow boots DARK instead of refusing; slot 0 can never be armed live (HYPARB H9)

**What changed**

- **Metric renamed:** `engine_hyparb_evm_no_wallet_total` is now
  `engine_hyparb_evm_superseded_total` — decisions replaced by a newer
  one while the shadow's single swap was in flight (wallet 0 only; the
  executor accepts its owner alone). Same position in the family; the
  series never left a testnet boot.
- **Boot:** in `mode = "testnet"`, an unreachable endpoint (transport,
  DNS, a non-200 such as a rate limit) or an unfunded wallet 0 no longer
  aborts the engine — the shadow stays DARK (ERROR log, nothing sent) and
  the paper member runs. A verified wrong chain, a 999 read without
  `--evm-hybrid`, a bad key/URL, or an executor wallet 0 does not own
  (boot reads `owner()`) still refuses.
- **`exec.toml`:** a live slot 0 (`hyparb`) refuses the boot
  (`exec_boot::NEVER_LIVE_SLOTS`).
- **`evm-testnet battery`:** (b) and (c2) are self-transfers from
  wallets 0..2; a new line (0) prints the executor-owner check.
- **`scripts/copy-audit.sh`** also audits `ingress-hyperevm`,
  `core-amm` and `strategy-hyparb` by default; the baseline lost one
  paid entry (`http1.rs`'s `copy_within`, now marked).
- `.env.example` documents `HYPEREVM_TESTNET_KEY` (and that the E3
  gate's testnet agent key is its fallback).
- **New gauge `engine_hyparb_evm_dark`** (registered unconditionally,
  0 on every paper boot): 1 when `mode = "testnet"` booted with the
  shadow dark. The family is 18 counters + 5 gauges.
- **Internal API:** `StrategyCounters::hyparb_decisions(after, out)` is
  replaced by `hyparb_decision_log() -> (&[HyparbDecision], u64)` (the
  log borrowed in place); the shadow's steady state moved to
  `cli::evm_shadow` (`cli::evm_testnet` re-exports it).

**Impact**

- `/metrics`: one series renamed and one gauge added (0 on paper
  boots). No wire format, capture or state file changed.

**Migration steps**

1. None. A dashboard or alert keyed on `_no_wallet_total` (none exists)
   would move to `_superseded_total`.

**Rollback**

- Revert the H9 commit.

## 2026-09-23 — the EVM write path linked: `[testnet]`, `--evm-hybrid`, `evm-testnet`, `engine_hyparb_evm_*` (HYPARB H8)

**What changed**

- **`hyparb.toml` gains `[testnet]`** (at most once): `endpoint`
  (https, chain 998), `wallets` (1..=8), and the OPTIONAL targets
  `executor`, `pool`, `amount_raw`. Optional in `mode = "paper"`;
  `mode = "testnet"` now requires it with every target set and a
  `[[coin]]` named `"HYPE"` (the gas coin). `mode = "testnet"` +
  `--evm-testnet` no longer refuses as "not linked".
- **`run --evm-hybrid`** (O-H12, requires `--evm-testnet`): the pool
  ingress may read chain 999 while the write path writes chain 998. At
  boot the engine asks the READ endpoint (`https://$HYPEREVM_WS_HOST` +
  `--hyperevm-path`) and the WRITE endpoint for `eth_chainId`; anything
  but same-chain, or exactly 999 → 998 with the switch, refuses the
  boot. The ARMED tell is logged at WARN.
- **Testnet mode shadows every paper AMM decision** with one swap on the
  `[testnet] pool` through the executor (thread `evm-shadow`); the paper
  book stays the P&L source.
- **Keys:** `HYPEREVM_TESTNET_KEY` in the operator's `.env`, else — by
  the 2026-09-23 ruling — `HYPERLIQUID_TESTNET_AGENT_KEY`. Wallets 1..
  are derived from it; `evm-testnet fund` funds them from wallet 0.
- **`multivenue-engine evm-testnet status|fund|deploy|mint|battery|shadow-smoke`**
  and `scripts/evm-testnet.sh` (sources the `.env` like
  `exec-smoke.sh`). Chain 998 only.
- **Metrics, registered unconditionally:** `engine_hyparb_evm_*` — 18
  counters (decisions, lost, dropped, no_wallet, bid_refused, sends,
  accepted, maybe_sent, refused_{fee,rate,nonce,funds,other}, not_sent,
  mined_ok, mined_reverted, timeouts, syncs) and 4 gauges
  (wallets_ready, halted, gas_paid_gwei, last_block). Zero on every
  paper boot.
- **Wrapper:** `EVM_HYBRID=1` adds `--evm-hybrid` on top of
  `HYPARB_TOML` + `EVM_TESTNET=1`; alone it refuses (exit 78). The live
  `ai+vrp+xsd+bin15` + `EXEC_TOML`/`ARM_LIVE` line is unchanged.

**Impact**

- `/metrics`: additive series only. A paper boot is otherwise unchanged.
- `hyparb.toml`: additive section; an existing paper artifact parses as
  before.

**Migration steps**

1. None for paper. For the testnet smoke: fund wallet 0 on HyperEVM
   testnet, `evm-testnet fund`, `deploy`, set `[testnet]`, `mint`.

**Rollback**

- Revert the H8 commit.

## 2026-09-23 — `/state` `hyparb` object, `engine_hyparb_*`, `engine_paper_matcher_amm_*`, `backtest --member hyparb`, `pnl_report --hyparb-ladder` (HYPARB H6)

**What changed**

- **`/state` gains a `"hyparb"` object.** ADDITIVE — `"v": 1` stays:
  `configured`, `n_pools`, `n_coins`, `halted`, the member's counters
  (flat: `pool_events` … `arbs_buy` / `arbs_sell` … `gas_charged_usd_1e6`,
  `pnl_predicted_usd_1e6`, `amm_notional_usd_1e6`,
  `funding_earned_usd_1e6`), `pools` (the first 64: `sym`, `live`,
  `map_ok`, `hedge_venue`, `fee_pips`, `mid_1e6`, `basis_bps_1e6`,
  `arbs`, `pnl_predicted_usd_1e6`) and `coins` (≤ 8: `perp_sym`,
  `spot_sym`, both depths, both quoted costs, `inventory_1e6`,
  `perp_pos_1e6`, `funding_1e9`). `EngineSnapshot` grows by ≈ 3.8 KiB
  (still under its 32 KiB pin).
- **Metrics, registered unconditionally:** `engine_hyparb_*` — 27
  counters (the member's counters as deltas, incl. `side_buy` /
  `side_sell` and the three money sums) and 35 gauges
  (`engine_hyparb_funding_earned_usd_1e6`, `_halted`, `_pools_live`,
  `engine_hyparb_c<0..3>_{perp,spot}_depth_usd_1e6`,
  `_{perp,spot}_cost_bps_1e6`, `_inventory_1e6`,
  `engine_hyparb_p<0..3>_{basis_bps_1e6,pnl_predicted_usd_1e6,live}`);
  `engine_paper_matcher_amm_{fills,canceled,partial,not_live}_total`.
- **`backtest --member hyparb --hyparb <toml> [--hyparb-universe <toml>]`**:
  the harness loads `hyperevm-signals.pmlr` (a lane ONLY this member
  reads — every other replay merges byte for byte as before), drives the
  paper matcher's AMM judge and the member in the engine's order, and
  subtracts the member's OOS gas from the OOS net.
- **`python -m claude_worker.pnl_report --closed-day --hyparb-ladder
  [path]`**: each unit is also replayed through the member's correction
  ladder (r0 naive → r1 depth cap → r2 latency → r3 the artifact); the
  day report gains an additive `hyparb` key (slot 0's paper rows + the
  ladder) and summary lines. Off by default.
- The AMM order's limit is its quote's LAST-unit price
  (`core_amm::limit_px_1e6`, replacing `avg_px_1e6`): the judge and the
  chain both bound the marginal price, so an average-price limit filled
  about half the quote.

**Impact**

- `/state` and `/metrics`: additive keys and series only.
- Worker: no frozen surface touched (the ladder is a module flag; the
  `pnl` verb reads the same files).

**Migration steps**

1. None.

**Rollback**

- Revert the H6 commit.

## 2026-09-23 — `hyparb.toml`, `--hyparb`, `--evm-testnet`, wrapper `HYPARB_TOML` / `EVM_TESTNET` / `HYPEREVM_PATH` (HYPARB H5)

**What changed**

- New artifact `~/multivenue/hyparb.toml` (grammar: `hyparb.toml.example`
  + `core_config::hyparb`): one `[hyparb]` section, 1–8 `[[coin]]` blocks
  (Hyperliquid perp / spot DESCRIPTORS, lot, venue minimum), 1–128
  `[[pool]]` blocks (an address that must be in `universe.toml
  [hyperevm] pools`, each token's hedge coin or `"USD"`, `trade`, an
  optional per-pool cap). Integers only; unknown / duplicate keys refuse.
- New boot flags `--hyparb <path>` (default the path above) and
  `--evm-testnet` (O-H5: the second switch; must agree with the
  artifact's `mode = "testnet"`, and refused while the EVM write path is
  not linked). Slot 0 enters the configured mask ONLY when the artifact
  resolves; requested-but-absent refuses the boot, and so does slot 0
  without the pool ingress (`--hyperevm-path` + a non-empty
  `[hyperevm] pools`). A runtime ingress failure darkens the member,
  never the engine (O-H15).
- `scripts/engine-wrapper.sh`: a STRATEGY carrying `hyparb` also passes
  `--hyperevm-path "${HYPEREVM_PATH:-/}"`; `HYPEREVM_PATH` alone passes the
  path for capture without the member. `HYPARB_TOML` + `EVM_TESTNET=1`
  (both or neither — exit 78, the `EXEC_TOML` / `ARM_LIVE` shape) pass
  `--hyparb <file> --evm-testnet`. A STRATEGY without `hyparb` and without
  those variables produces the exact pre-H5 command line.

**Impact**

- Config keys: one new artifact, three new optional `strategy.conf`
  variables. Nothing changes for a STRATEGY without `hyparb`.
- On-disk formats: none.

**Migration steps**

1. None for the live engine (O-H8: `strategy.conf` is not edited; slot 0
   joins the live mask only at go-live, after `[hyperevm]` and the
   artifact are in place on a binary built from `main`).

**Rollback**

- Revert the H5 commit.

## 2026-09-23 — `[hyperevm] pools`, the `hyperevm` capture label, `/state` venue 9 (HYPARB H3b)

**What changed**

- `universe.toml` gains an OPTIONAL `[hyperevm]` section:
  `pools = ["0x<40 lowercase hex>:<v3|slipstream|algebra>:<dec0>:<dec1>", …]`
  (≤ 128, append-only; `pools[i]` → `make_symbol_id(HyperEvm, i+1)`,
  descriptor `hyperevm:0x<address>`, class `Spot` in the descriptor law —
  Rust, Python mirror and the shared fixture). Absent = the pre-H3b boot.
- New boot flag `--hyperevm-path <path>` and env `HYPEREVM_WS_HOST`
  (default `rpc.purroofgroup.com`, O-H15). The HyperEVM ingress runs only
  with BOTH the flag and a non-empty `[hyperevm] pools`; its signals feed
  the engine's new pool lane (`engine::POOL_RING_SIZE` = 4,096).
- Every snapshot now also reads `token0()` / `token1()` and both tokens'
  `decimals()`; a value that differs from the configured decimals fails
  that pool (`dec_mismatch`).
- Capture label `hyperevm` (`hyperevm-signals.pmlr`; ticks/events/depth
  header-only) — appended to `VENUE_LABELS` in backtest / audit-replay /
  capture-catalog (the catalog's `venue_ticks` array gains a 9th, zero
  entry) and to `--raw-tap` (`hyperevm`).
- `/state`: `SNAPSHOT_VENUES` 8 → 9 (`hyperevm` appended after `mexc`).
  Metrics: `engine_ingress_hyperevm_state`,
  `engine_ingress_hyperevm_last_tick_age_seconds`, the
  `engine_ingress_hyperevm_*` counter family and the `hyperevm` capture
  gauges (registered unconditionally, like every venue's).

**Impact**

- Config keys: new optional `[hyperevm]` section and `HYPEREVM_WS_HOST`.
  **The live `main` binary does not know `[hyperevm]` — never add it to
  the live `~/multivenue/universe.toml` before this lane merges to `main`**
  (a hyparb smoke boots with `--universe <copy>`).
- On-disk formats: one new capture label; no layout change.

**Migration steps**

1. None for the live engine.

**Rollback**

- Revert the H3b commit.

## 2026-09-23 — `Order.kind` 2 = AMM swap; the AMM fill law; `SNAPSHOT` carries decimals (HYPARB H2)

**What changed**

- `core_fill::ORDER_KIND_AMM_SWAP = 2`: a swap against a HyperEVM pool
  (`sym` = the pool, `qty` token0 × 1e6, `px` the worst average price,
  token1 per token0 × 1e6; Ask sells token0, Bid buys it). HyperEVM
  (venue 8) takes this kind on a pool slot and nothing else; no other
  venue takes it. Kind 2 was never emitted before (a reserved byte).
- The engine's paper matcher and the harness judge swaps with
  `core_fill::AmmBook` (law: `core_fill::amm` module doc). New
  `OrderDispatch::observe_amm(sym, &payload, now)` (defaulted no-op);
  the engine calls it for every `SignalSource::HyperEvm` signal before
  the member's `on_signal`.
- `core_amm::payload` `SNAPSHOT`: `dec0 u8 @23 · dec1 u8 @24` (≤ 36) and a
  fee < 100 % are now part of the layout; the decoder refuses anything
  else. No capture carries a HyperEVM label yet (H3b), so no tape exists
  in the old form.
- Harness: `tradeable_venue_byte` accepts 8 (swaps only),
  `TRADEABLE_VENUES` 6 → 7; an AMM fill books at 0 bps (the pool fee is
  in the fill price). `MatcherCounters` gains
  `amm_{fills,canceled,partial,not_live}`.

**Impact**

- Wire formats: none on disk (`Order.kind` 2 was unused; the payload
  has no tape yet). Schema-1: unchanged (AMM fills are ordinary fills;
  the AMM counters live outside `ModelOutcome`).

**Migration steps**

1. None.

**Rollback**

- Revert the H2 commit.

## 2026-09-23 — Strategy slot 0 = `hyparb`; `latency-arb` unlinked (HYPARB H0)

**What changed**

- Slot 0 of `crates/strategy-set` is the `strategy-hyparb` member
  (`SLOT_HYPARB` / `BIT_HYPARB`, still bit 1 = 1). `strategy-latency-arb`
  is UNLINKED, not deleted (ruling O-H1): it stays in the workspace, its
  own tests and the `bench` alloc gate / `core-io` replay test still
  build it, but no engine path composes it.
- Mask names: `hyparb` (1), `ai+hyparb` (49), `ai+vrp+xsd+bin15+hyparb`
  (63) are new; `latency-arb` is GONE as a name — `--strategy
  latency-arb` refuses the boot ("unknown --strategy value"). Its
  standalone paper/`--live` arm is deleted with it, so
  `STRATEGY_SET_NAMES` now equals `MASK_TABLE` exactly (no exemption).
- `run --strategy` defaults to `ai` (was `latency-arb`).
- The hyparb member lands DARK (O-H8): at H0 it is a stub and is never
  in the boot's configured mask, so `--strategy hyparb` refuses as "no
  requested member is configured" and the composite names boot with
  bit 0 cleared until HYPARB H5 lands its boot artifact.
- Slot-0 labels: `/state` `slots[0].name`, `audit-pnl` `strategies[].label`
  for `strategy_id 0`, `exec_boot::SLOT_NAMES[0]` and the dashboard read
  `hyparb`. Gauge `engine_strategy_latency_arb_active` →
  `engine_strategy_hyparb_active` (same F29 semantics: bare kind
  `hyparb` or slot 0 enabled in the set).
- `regime.toml`: `[labels.hyparb]` is the slot-0 section;
  `[labels.latency_arb]` is refused at the grammar ("unknown coded
  member"), exactly as `[labels.ev]` / `[labels.cross_arb]` are.
- `scripts/engine-wrapper.sh` allow-list gains the three names.

**Why**

- HYPARB (HyperEVM ↔ HL Core arbitrage) takes slot 0 (O-H2). A label,
  mask or audit row evidenced for latency-arb must never silently apply
  to a different member.

**Impact**

- On-disk formats: none. The slot NUMBER is wire-stable: rows under
  `strategy_id 0` in a capture taken BEFORE 2026-09-23 are latency-arb
  rows wearing the `hyparb` label (latency-arb was OFF in every wrapper
  mask, so a live capture carries none).
- Config keys: `regime.toml [labels.latency_arb]` refuses the boot.
- Metrics: `engine_strategy_latency_arb_active` is renamed.

**Migration steps**

1. None for the live engine: `strategy.conf` names no slot-0 mask and is
   not edited (O-H8).
2. A `regime.toml` carrying `[labels.latency_arb]` renames the section
   to `[labels.hyparb]` or drops it.

**Rollback**

- Revert the H0 commit; no data or config migration to undo.

## 2026-09-23 — VenueId 8 = HyperEvm; venue tables sized by `VENUE_COUNT`; exec venue mask u16 (HYPARB H0)

**What changed**

- `VenueId` gains `HyperEvm = 8` (append-only; the first unassigned byte
  is now 9) and `SignalSource::HyperEvm = 5` (the HyperEVM ingress
  publishes `Signal`s — H3). HyperEvm rides NO tick, depth, option or
  fill lane (`engine::*_lane_of` → `None`); its AMM fills are judged in
  process (H2).
- `core_types::VENUE_COUNT = 9` is now the single size of every
  venue-indexed table: `VenueId::stale_after_ms_defaults` (HyperEvm
  2 500 ms — the HZ head p99 was 2 281 ms), `core_fill::ACTIVATION_NS_DEFAULT`
  (HyperEvm 1 000 ms = one block), `ModelParams` (`fee_bps`,
  `fee_open_bps`, `fee_settle_bps`, `latency_ns`, `stale_after_ms`,
  `opt_fee`), `parse_stale_after_ms`, `clob-dispatcher` activation,
  `core-config::exec::VENUE_NAMES` (`hyperevm` = 8).
- Harness labels: `hyperevm` joins the backtest model labels
  (`--fee-bps hyperevm:…`, `--latency-ns-venue hyperevm:…`,
  `--stale-after-ms hyperevm:…`); the rendered fee table gains a
  trailing `hyperevm` entry (text ` hyperevm=0:0`, JSON `"hyperevm":{…}`
  after `"mexc"`).
- `exec-router`: `EXEC_VENUES` 8 → 16 and the per-slot venue mask
  `u8` → `u16` (`venue_mask_at -> Option<u16>`); the route table keeps
  its 64-byte-bounded layout (pad 8).
- Not yet: `SNAPSHOT_VENUES` / capture `VENUE_LABELS` gain `hyperevm`
  with the ingress wiring (H3b), `TRADEABLE_VENUES` with the AMM fill
  law (H2).

**Why**

- HYPARB's DEX leg (O-H11): HyperEVM is a venue of its own, and a ninth
  venue does not fit an 8-bit venue mask.

**Impact**

- On-disk formats: none (no capture label yet).
- Config keys: `exec.toml` venue names accept `hyperevm`; nothing arms
  it (no exec arm until H7c/H8, testnet only — O-H5).
- Wire formats: `VenueId` byte 8 and `SignalSource` byte 5 are assigned.

**Migration steps**

1. None.

**Rollback**

- Revert the H0 commit.
## 2026-09-23 — Binance options on fstream's routed `/market` path (`<uly>@optionMarkPrice`); `BINANCE_EAPI_WS_HOST` default `nbstream.binance.com` → `fstream.binance.com` (BX0-F2)

**What changed**

- The options slot dials `/market/stream?streams=<uly>@optionMarkPrice/…`
  — one stream per `options_underlyings` entry, lowercase
  (`cli::bn_options_path`) — instead of the per-option
  `/eoptions/stream?streams=<opt>@ticker/…/<uly>@index`. Each push is ONE
  array holding the underlying's whole listed chain; the lane keeps the
  boot-selected rows by table lookup
  (`ingress_binance::eapi::{EapiArrayCursor, eapi_elem_symbol,
  parse_eapi_mark}`). `parse_eapi_ticker`, `parse_eapi_index` and the
  per-underlying index cache (`EapiLane`) are gone: every element carries
  the underlying's index (`i`) itself.
- `BINANCE_EAPI_WS_HOST` default `fstream.binance.com` (was
  `nbstream.binance.com`); `.env.example` updated. The boot provenance
  line (`binance: options mark-array slot …`) names host, path,
  underlyings and the selected count.
- The options slot's rx buffer 512 KiB → 2 MiB (a BTC push measured
  245.6 KB).
- `boot_discovery::Outcome::bn_options` is `(symbol, sym)` — the OKX
  shape; the underlying index had no reader left — and
  `EapiSymbolTable::insert(symbol, sym)` keys the venue-case symbol.
- New fuzz target `binance_eapi_mark_array`; `binance_eapi` drops the two
  retired parsers.

**Why**

- BX0-F2 (`docs/binance-exec-plan.md` §2, §5): the venue's 2025-12
  options migration moved these streams onto fstream. nbstream
  `/eoptions/…` answers HTTP 404 — the 2026-08-22 "unreachable from this
  network" diagnosis was that, and a host override alone could not cure
  it: the path and the stream names changed too. Measured 2026-09-23
  (K6): 101 and one push per underlying per ~1 s on `/market`; a wrong
  route (`/stream?…`, `/public/stream?…@optionMarkPrice`) upgrades and
  then carries nothing.

**Impact**

- Captures: `bn-opt-summary.pmlr` stops being header-only — one 64 B
  record per selected option per push (~1/s each; 64 options ≈ 4 KB/s),
  `underlying_px_1e9` = the element's index, flags = mark_px only — and
  `bn-ticks.pmlr` gains the options' BBO ticks (`venue_seq` 0). Additive
  rows; no layout change.
- Engine: Binance opt lane 2 carries summaries again →
  `Strategy::on_opt_summary` (the vm feature engine's mark/IV features on
  `binance-opt:` descriptors). The VRP member's configured underlying is
  Deribit (`vrp.toml`), so its chain is untouched.
- Offline: pools that include post-BX0 windows gain Binance option rows;
  existing pools — and the three standing guards (backtest schema-1
  `188d18e3…`, detail sidecar `e3f6b8ef…`, audit-pnl `725d1d27…`) — do
  not move.

**Migration steps**

1. **Operator, before the restart:** if the repo `.env` sets
   `BINANCE_EAPI_WS_HOST=nbstream.binance.com`, change it to
   `fstream.binance.com` or delete the line — an explicit `.env` value
   wins over the new default, and nbstream keeps the lane dark. Sessions
   never edit `.env`; on the Mac that one line was switched on 2026-09-23
   by a session at the operator's explicit ask (plan O-BX12b).
2. The routine restart of a binary built from this change. Verify: the
   boot line shows `host=fstream.binance.com`; the new run dir's
   `bn-opt-summary.pmlr` grows past its 64 B header within seconds;
   `engine_ingress_binance_parse_errors_total` stays flat.

**Rollback**

- Safe: an older binary dials its old path again (404 on nbstream,
  silence on fstream) and the lane goes dark; nothing else changes.

## 2026-09-23 — Binance USDⓈ-M `markPrice` on fstream's routed `/market` path: `Mark`/`Funding` rows return; a dated contract's zero rate is not funding (BX0-F1)

**What changed**

- The USDⓈ-M markPrice slots dial `/market/ws/<sym>@markPrice` (was
  `/ws/<sym>@markPrice`) through `cli::bn_usdm_specs` — one builder for
  the engine boot and the live smoke. The bookTicker slots stay on
  `/ws/<sym>@bookTicker`, which still delivers.
- `ingress_binance::parse_mark_price`: `has_funding` needs a parseable
  rate AND a next settlement `T` > 0 — the live dated shape is
  `"r":"0.00000000","T":0`, where the WS5-era wire sent `"r":""`; rate
  and next-funding read 0 otherwise. The `ap` / `st` keys the frame
  gained are skipped.
- New standalone `#[ignore]` live smoke
  `crates/cli/tests/binance_md_live_smoke.rs` (F1 + F2; own
  `CARGO_TARGET_DIR`, never stops the engine).
- No layout, version or config change.

**Why**

- BX0-F1: Binance stopped serving `/market` streams on the legacy `/ws/`
  URLs on 2026-04-23; the upgrade still answers 101 and the socket stays
  silent (measured 2026-09-23: 0 frames in 7 s, against one per 3 s on
  `/market/ws/`). The 2026-08-29 "venue-side partial fault" on
  markPrice / aggTrade / kline / forceOrder was this routing: exactly the
  `/market` streams went quiet while the `/public` ones flowed.

**Impact**

- Captures: from the first boot on this build, `bn-events.pmlr` carries a
  `Mark` row (`v0` mark ×1e6, `v1` index ×1e6) per USDⓈ-M perp and dated
  contract about every 3 s, and a `Funding` row (rate ×1e9, next-funding
  ms) per perp. No capture made before that boot holds a Binance Mark or
  Funding row.
- Engine: Binance `Funding` events reach the venue-event lane again →
  the strategy set → the regime detector's FUND reference
  (`regime.toml [refs] fund = "binance-usdm:btcusdt"`), which has had NO
  live funding print (its boot seed is price-only): FUND_SIGN /
  FUND_LEVEL start reading live prints, so the regime word can change
  after the restart. The vm feature engine's Binance funding law gets its
  prints too.
- Offline: `backtest` / `audit-pnl` replay `Funding` rows, so a pool that
  includes post-BX0 windows feeds the regime FUND input the live engine
  now has; existing pools and the three standing guards do not move.
- Metrics: a mark frame counts once in `engine_ingress_binance_msgs_total`
  AND `…_ticks_total`, exactly like a bookTicker frame, so the Binance
  tick rate rises by about (perps + dated) / 3 per second — ≈ 41/s at the
  current 124 perps. (Plan v2's "msgs − ticks" tell was wrong: the two
  counters move together.)

**Migration steps**

1. None: the routine restart of a binary built from this change. Verify:
   the new run dir's `bn-events.pmlr` grows by ≈ 5 KB/s (≈ 83 rows/s of
   64 B), and `vm_rows_active ≥ 1` on `/state` as after any restart.

**Rollback**

- Safe: an older binary dials the silent legacy path again and the lane
  goes dark — its Mark and Funding rows stop, nothing else changes.

## 2026-09-23 — VenueId 7 = MEXC + tick lane 6 (MX2–MX9, the seventh venue)

**What changed**

- `VenueId` gains `Mexc = 7` (append-only; the first unassigned byte is
  now 8). MEXC rides TICK and EVENT LANE 6 (`engine::tick_lane_of`);
  `NUM_TICK_LANES` 6 → 7; no depth, options or fill lane.
- Every venue-indexed table widens 7 → 8:
  `VenueId::stale_after_ms_defaults`, `core_fill::ACTIVATION_NS_DEFAULT`,
  `ModelParams` (`fee_bps`, `fee_open_bps`, `fee_settle_bps`,
  `latency_ns`, `stale_after_ms`, `opt_fee`), `parse_stale_after_ms`;
  `engine_snapshot::SNAPSHOT_VENUES` 7 → 8 (`mexc` appended AFTER
  `rpc` — the `/state` array position). `TRADEABLE_VENUES` stays 6:
  MEXC has model columns but is NOT tradeable (ruling O-MX1).
- New capture label `mexc` — the uniform set `mexc-ticks.pmlr`,
  `mexc-events.pmlr`, and header-only `-signals` / `-opt-summary` /
  `-depth`; `mexc-raw.tap` under `--raw-tap mexc`. `mexc` appended to
  `VENUE_LABELS` in `backtest`, `audit-replay` and `capture-catalog`.
  Per-venue capture law (ticks are BBO changes, `venue_seq` without the
  chain law, the event mapping): `docs/wire-format.md` "Capture files".
- `[mexc]` universe section: `spot` (UPPERCASE `BTCUSDT`; xStocks
  `AAPLXUSDT` are ordinary rows) → `make_symbol_id(Mexc, i+1)`; `perp`
  (`BTC_USDT`; TradFi rows ordinary) → ordinals from
  `MEXC_PERP_ORDINAL_BASE = 512`. Descriptors `mexc:<SYM>` (Spot) and
  `mexc-perp:<SYM>` (Perp), mirrored in `core-config::instrument_class`,
  `claude_worker.instrument_class` and the shared TSV fixture.
- Config hosts (optional; defaults in `.env.example`): `MEXC_WS_HOST`
  (`wbs-api.mexc.com`), `MEXC_FUT_WS_HOST` (`contract.mexc.com`),
  `MEXC_REST_HOST` (`api.mexc.com`), `MEXC_FUT_REST_HOST`
  (`contract.mexc.com`) — MEXC splits spot and futures on both planes.
- Metrics: the `engine_ingress_mexc_*` family (counters, `_state`,
  `_last_tick_age_seconds`, `_feed_delay_ema_ms`, capture and coverage
  gauges) and a NEW counter on EVERY venue,
  `engine_ingress_<venue>_seq_regressions_total` (ruling Q-MX1 — only
  MEXC increments it; every other venue reads 0).
- `/state`: the `ingress` array gains a `mexc` row after `rpc`; the TUI
  shows it as the eighth venue row.
- Harness / audit / engine flags: `mexc` is a model label —
  `--fee-bps mexc[.<class>]:<m>:<t>`, `--latency-ns-venue mexc:<ns>`,
  `--stale-after-ms mexc:<ms>` (engine `run`, `backtest`, `audit-pnl`),
  `--raw-tap mexc`. Defaults: stale **400 ms**, Δ **150 ms** (measured
  2026-09-23, `docs/venue-latency.md` §3). The `backtest` summary line
  and the detail sidecar's `stale_after_ms` object gain a `mexc` key
  (additive; `detail_version` stays 7).
- `cli::build_ai_universe` takes a sixth argument `mexc_syms:
  &[SymbolId]`: every MEXC instrument the boot allocated joins the
  ruleset validator's boot universe (ruling Q-MX6; Bybit stays out).
  Caps: `mexc:` → `CAP_PRICE`, `mexc-perp:` → `CAP_PRICE | CAP_FUNDING`,
  on both sides of the Rust↔Python mirror.
- `core-config::exec::VENUE_NAMES` reserves `mexc` = 7 — an `exec.toml`
  may NAME it; nothing arms it (no `ExecMode` arm, no dispatcher).
- `capture-catalog` JSON: the per-day `venue_ticks` array carries EVERY
  venue in `VENUE_LABELS` order (8 entries; it was the first 6 of 7 and
  silently omitted Bybit).
- `fees.toml`: `[fees] mexc = "1:4"`, `[fees.mexc] spot = "0:5"`,
  `perp = "1:4"` — the published rates, UNVERIFIED (ruling Q-MX4).
  `news.toml`: source kinds `json-mexc-ann` and `ping-mexc`, five
  example rows (`venue = "mexc"`).
- Worker: `VENUE_MEXC = 7`; candle lanes `mexc` / `mexc-perp`; funding
  lane `mexc-perp`; `latency_probe` rows `mexc` / `mexc-perp`.

**Why**

- `docs/mexc-ingress-plan.md` MX0–MX9 — the seventh venue; rulings
  O-MX1…O-MX3 (2026-09-20) and Q-MX1…Q-MX7 (2026-09-23).

**Impact**

- On-disk formats: new per-venue capture files under the existing
  container version (PMLR v3 — no bump, no slot-layout change); ticks
  and events may carry venue byte 7. Pre-MX2 `audit-replay` /
  `backtest` binaries skip the `mexc` label's files and treat venue
  byte 7 as corruption — decode with a post-MX2 binary.
- Config keys: `[mexc] spot/perp` (additive); four optional env hosts;
  the `fees.toml` and `news.toml` MEXC entries.
- Wire formats: append-only enum growth on `VenueId`.
- Operator surfaces: one new counter per venue, the `mexc` metrics
  family and `/state` row, a longer `venue_ticks` array.

**Migration steps**

1. None until a `[mexc]` section is configured; the venue is entirely
   opt-in. **A boot with no `[mexc]` behaves as before**: no MEXC
   discovery, thread, socket or capture file — lane 6 idles, and the
   new metrics and `/state` row read zero.
2. To enable: append `[mexc]` to `~/multivenue/universe.toml` (append,
   never reorder) and restart the engine. The `www.mexc.co`
   announcement origin needs the operator's trust approval before its
   `news.toml` rows are enabled.

**Rollback**

- Safe while `[mexc]` stays empty (old binaries reject the section as
  an unknown-section parse error — remove it before rolling back).
  Remove the `mexc` lines from `fees.toml` too (an older `pnl_report`
  refuses an unknown fee venue) and the MEXC rows from `news.toml` (an
  older worker refuses the unknown kinds).

## 2026-09-20 — E7 session bound: `exec.toml` gains OPTIONAL `halt_on_gain_usd_1e6` / `halt_on_loss_usd_1e6`, `HaltReason` 8/9 (`pnl-gain`/`pnl-loss`), `HaltSignal` 40 → 48 B, `exec-pnl-anchor.state`

**No wire-format change.** One state file, two config keys, two halt
reasons, two gauges and two `/state` fields are NEW; every existing
file and key reads as before.

* `exec.toml` `[exec.slot.<n>]`: `halt_on_gain_usd_1e6`,
  `halt_on_loss_usd_1e6` (USD ×1e6, `>= 0`, OPTIONAL even on a live
  slot; `0`/absent = no bound on that side). `SLOT_KEYS` 13 → 15. The
  template documents both commented out.
* `exec_router::HaltReason`: `PnlGain = 8` (`pnl-gain`), `PnlLoss = 9`
  (`pnl-loss`); `engine_snapshot::HALT_REASON_WORDS` 8 → 10 (pinned
  against the enum by `cli::exec_boot`). `exec.HALT` may now carry
  `reason=pnl-gain` / `reason=pnl-loss`.
* `clob_dispatcher::HaltSignal`: `pnl_flat: u8` at the former padding
  and `pnl_delta_usd_1e6: i64` appended — 40 → 48 B (size-asserted).
  `HaltLimits` gains `pnl_gain_usd_1e6` / `pnl_loss_usd_1e6`
  (`with_pnl_bound`), 32 → 48 B; `ExecRoute` 512 → 640 B (the halt
  table, lines 5–10; the hot arrays and the offset-256 halt table
  start are unchanged).
* `clob_dispatcher::LiveArmCounters`: `pnl_anchor_usd_1e6`,
  `session_pnl_usd_1e6` (i64, appended). `HlExecCounters`:
  `anchor_persist_failed` (appended; the block stays five lines).
* `/state` `exec` object (additive, `"v": 1` untouched):
  `arm_pnl_anchor_usd_1e6`, `arm_session_pnl_usd_1e6`. `/metrics`
  gauges `engine_exec_hl_pnl_anchor_usd_1e6`,
  `engine_exec_hl_session_pnl_usd_1e6`.
* New state file `exec-pnl-anchor.state` beside `exec-budget.state`
  (one line `<0x master>\t<usdc ×1e6>\t<unix s>`), written once at the
  first FLAT reconciliation of a session, read at boot for the same
  master only. **Delete it to start a new session**; it is not touched
  by any restart.
* Boot tells: ARMED gains `pnl_anchor_usd_1e6=… pnl_anchor_state=…`;
  HALTS gains `session_bound=+$<gain>/-$<loss>` (pinned).

Why and how it is judged: `docs/risk-policy.md` "E7 — the SESSION
BOUND".

**Behavioural, same build (E7-F3):** `recon-drift` now needs the
disagreement to be seen by TWO consecutive reconciliations (60 s
apart) before it reaches `recon_drift_max_qty_1e6` and the halt; the
`/state` level `arm_recon_drift_legs` is unchanged and still shows a
single sighting. Record: risk-policy "E7 — MAINNET R0", 15:32:22Z.


**No wire-format change, no file-layout change, no config change.**
`hl-depth.pmlr` (kind 7, 192 B `DepthTopK` slots, present in every
capture set since WS10-B and header-only for Hyperliquid until now)
receives one snapshot per `l2Book` push of an outcome coin whose top
five levels on either side changed (`ingress_hyperliquid::parse_l2book_depth`,
change-gated per coin slot in the run loop). Perps and spot write
nothing; there is no HL depth ring; `bbo`/tick/event capture is
untouched. Readers (`core_io::PmlrReader<DepthTopK>`,
`claude_worker.pmlr.DepthReader`, `depth_digest`) need no change.

Why: the intraday study (vault doc 25) could see only the touch of a
book the venue re-sends every 5.3 s — measured on mainnet the same day:
`l2Book` is a venue timer, 5.33 s median for perps and outcome legs
alike, 41 % of pushes moving the outcome touch; the outcome legs' `bbo`
is pushed on change (sub-second) but still carries `null` for the ask.
The full snapshot is therefore the only view of an outcome book, and
its top five levels are now kept whenever they move (~0.2 rows/s per
leg at today's activity, 192 B each).

Ripple: a capture set from this build on carries kind-7 rows for the
outcome legs; the retention/archive lanes treat the file as they
already do for OKX/Deribit (size-gated). `ingress_hyperliquid::scan_side_levels`
takes an output slice (empty for the header read) — the header parser's
behaviour and every existing tick is bit-identical.

## 2026-09-19 — `fees.toml` gains `<class>_settle` → `--fee-bps <venue>.<class>.settle:<m>:<t>` (the HIP-4 settlement fee, measured)

**No wire-format change, no file-layout change.** One config key and
one harness flag form are NEW; every existing file and flag reads as
before.

Why: the first mainnet settlement (2026-09-19 13:15:09Z, `userFills`)
charged **0.002688 USDC on a $2.00 payout** while both trade rows and
the losing settlement charged 0 — 14 bps of the payout (13.44 with the
account's 4 % referral discount), on the payout only. The fee model had
two legs per class (`prediction` for closing fills and settlement,
`prediction_open` for opening fills) and could not say "the trade is
free, the payout pays": setting the ordinary pair would also charge a
closing trade on the book, which the venue does not.

What changed: `ModelParams.fee_settle_bps[venue][class]: Option<(m, t)>`,
set by `--fee-bps <venue>.<class>.settle:<m>:<t>`; `FillEngine::settle_binary`
charges its second number on `payout × contracts` when present and the
class's ordinary taker number when absent (bit-identical for every venue
and class without one — every settlement charged before this entry).
`claude_worker.pnl_report.load_fee_flags` emits it from
`[fees.<venue>] <class>_settle = "<m>:<t>"`; `fees.toml.example`'s
`[fees.hl]` carries `prediction_settle = "14:14"` and the four measured
rows. `audit-pnl` passes the table through like the open pair.

Ripple: a report on a root with BIN15 settlements changes by the
settlement fee once the operator's `~/multivenue/fees.toml` carries the
key (the nightly `pnl_report --closed-day` reads that file; a fees file
without the key charges the old 0). A worker or binary OLDER than this
entry refuses a fees file that carries `<class>_settle` (an unknown key
is fatal by the grammar's own law) — relink before adding the line.

## 2026-09-19 — Exec arm: `ioc_missed` counter (E7-F2) and the budget seeded from `userRateLimit` (E7-F1)

**No wire-format change, no file-layout change, no config key change.**
Two `/metrics` surfaces move:

- `engine_exec_hl_ioc_missed_total` is NEW (`LiveArmCounters.ioc_missed`,
  `LIVE_ARM_COUNTER_NAMES` 24 → 25). An IoC the venue understood and
  could not match (`"Order could not immediately match…"`,
  `historicalOrders` status `iocCancelRejected`) now counts here and NOT
  in `engine_exec_hl_rejected_total`, and moves neither `reject_streak`
  nor `asset_refusal_streak`. Before this entry every miss was a
  "venue rejection" and fed `halt_on_reject_streak` — two of them did
  in the first mainnet hour. A dashboard summing `rejected` for
  "orders that did not trade" must add `ioc_missed`.
- The ARMED boot tell gains `budget_source` (`Venue` / `File` / `Cold`)
  and `budget_remaining`. `HlExchange::seed_budget_from_venue` (called
  by the boot, never by `new`) replaces the loaded budget with the
  venue's `nRequestsUsed` / `cumVlm`; the state file `exec-budget.state`
  is the fallback and the persistence between reads, the cold assumption
  the fallback for a venue that does not answer at boot. A fresh address
  no longer boots into a `budget-floor` halt.

Ripple: `HlExecCounters` 34 → 35 counters (still 320 B), `LiveArmCounters`
208 → 216 B. `recon.rs`'s two `/info` renderers collapse into
`user_info_request` (the third caller is `budget::rate_limit_request`).

## 2026-09-19 — HIP-4 minimum order notional: $10 → **1 USDC** in `GRID_MIN_NOTIONAL_1E6` and `PREDICTION_MIN_NOTIONAL_1E6`

**No wire-format change, no file-layout change, no config key change.**
Two constants move: `strategy_bin15::GRID_MIN_NOTIONAL_1E6` (the member's
`on_grid` check) and `cli::backtest::fill::PREDICTION_MIN_NOTIONAL_1E6`
(the harness's O3 grid refusal), both `10_000_000` → `1_000_000`.

Why: the venue's floor was MEASURED at 1 USDC twice — 2026-09-15 (E4
phase D: size 1 @ 0.68 = $0.68 refused `Order must have minimum value of
1 USDC.`, 2 @ 0.68 filled) and 2026-09-19 (testnet 15415: 4 @ 0.30 =
$1.20 placed/modified/cancelled, 3 @ 0.30 = $0.90 refused with the same
string). The $10 came from third-party docs on 2026-09-12 (the PERPS
rule) and survived the measurement because the measurement was recorded
in `docs/risk-policy.md` and never carried into the constants.

Ripple — the member: `emit_take` still floors the size to whole
contracts, then `on_grid` requires notional ≥ $1. The smallest
`entry_usd_1e6` that survives at EVERY ask ≥ 0.50 is now **$2**
(`2000000`: the floored order lands in [$1.33, $2.00]); `$1` itself
breaches the floor at every ask that does not divide it (0.51 → 1
contract = $0.51, refused). The R0 artifact's `$12` was sized for the
wrong floor and is a ceiling, not a requirement.

Ripple — the harness: a prediction-class order with notional in
[$1, $10) that was `prediction_grid_refused` before now fills (subject
to every other law). `backtest` / `audit-pnl` numbers on a root that
contains such orders differ from any report produced before this entry;
every BIN15 artifact run to date sized entries at ≥ $12, so the
recorded reports should be unchanged in practice (verify on a bounded
window before quoting one). Every existing root still
replays byte for byte — the capture is untouched; only the scorer's
refusal threshold moved.

## 2026-09-20 — tier-1 triage prompt `triage-v2` → `triage-v3` (impact rubric only)

**No schema change, no parser change, no config change.** `build_triage_prompt_v3`
keeps v2's JSON shape, closed vocabularies and item fencing byte for byte;
`parse_triage_v2` reads a v3 answer unchanged. Only the impact rubric differs, and
`TRIAGE_PROMPT_VERSION_V3` is what keeps the two apart in the prompt cache — a v2
answer was given under a different definition of `high` and must never be replayed
for a v3 question. Both builders and both version constants remain exported; the
cascade asks v3.

Why (operator ruling 2026-09-20, on 150 Opus-adjudicated items): v2 defined `high`
by a list of event KINDS including "maintenance" and "regulatory action". The gold
set says `regulatory` is med 22 times against high 3, `maintenance` med 10 against
high 2, and `fomc` — 5 of 5 high — was not in v2's list at all. **94 % of the local
model's med→high errors were items v2 itself declares high**, and a frontier model
erred in the same place; mechanically adopting v2's rule into the gold set made
agreement worse for both (0.627 → 0.580 and 0.630 → 0.521). So `high` now names the
ACTION it earns — "this one report alone justifies interrupting an analyst NOW" —
with event kinds as illustrations rather than as the test.

Second change, same ruling: the med/low line is the one that actually gates the
lane (`ESCALATE_IMPACTS = ("med", "high")`), so the prompt now states the tiebreak
— when the call is close, answer `med`, because a missed event costs more than a
wasted look. A tagger given no tiebreak picks its own, and this lane's is not
symmetric.

Ripple: every tier-1 `prompt_cache` entry is cold on the first v3 pass — by design,
and the calls are re-paid once. `triage` rows written from here carry
`prompt_version = "triage-v3"`; any comparison spanning the boundary must split on
that column rather than pooling. Record: `docs/research/feeds/04-gold-set-and-gates-2026-09-20.md`.

## 2026-09-19 — Deribit spot rows subscribe `quote` + `book` only (no `trades`); capture carries no Trade rows for `BTC_USDC`

**No wire-format change, no file-layout change, no config change.** The
Deribit subscribe batch and its verification mask drop the `trades.<spot>.100ms`
channel for static SPOT rows (a configured name with no `-`, i.e. `BTC_USDC`).
Futures/perps keep quote/ticker/trades/book; options keep quote + ticker; combos
keep quote. The one law is `ingress_deribit::row_wants_channel`.

Why: on 2026-09-17 ~16:00Z Deribit made its USDC spot pairs Coinbase-routed —
`trades.BTC_USDC.100ms` is accepted by `public/subscribe` and silently absent
from the echo (REST `get_last_trades_by_instrument` answers `11060
not_supported_for_coinbase_routed_spot`). Under the boot fail-fast that one
absent name refused every Deribit session for two days (1,539 reconnects in one
run, `ticks 0`), and slot 1 (vrp) saw no option summary. Record:
`docs/arch/deribit-spot-trades-outage-2026-09-19.md`.

Ripple: from the first boot on the fixed binary, `deribit-events.pmlr` holds no
`channel=0` (Trade) rows for the spot sym — there have been none since 09-17
anyway. Nothing in the engine, the harness or the worker reads a Deribit spot
print; the spot row exists for its BBO (`quote`) and `book` capture (WS6).
Offline surfaces that counted Deribit event rows by channel see the spot Trade
series end at 2026-09-17T16:07Z.

## 2026-09-19 — the Hyperliquid ingress announces BOOT-bound rolling families with one created `InstrumentRoll` at the first Steady (E7 R0)

**No wire or format change; one more record per boot.** `bind_live`
binds each family's live instance into the coin table before the ingress
thread starts, but no `InstrumentRoll` event was ever written for it —
and that event is the only way the bin15 member and the exec arm's
asset table learn which instance a slot means. They stayed dormant until
the venue's next `outcomeCreated` push: ≤ 15 min for the 15-minute
family, up to a day for a daily one, so with three restarts a day the
daily families were live for the member ~2.5 h in 24 (zero daily-family
fills across six paper days, vault doc 21).

Now `run_loop::emit_boot_rolls` writes, ONCE per process at the first
Steady (never on a reconnect — `Bin15Strategy::bind` flattens the
instance it rebinds), a created roll per non-dormant family, in exactly
the shape `perform_roll` writes. Offline readers already take "the newest
created roll at or before the instant" (`claude_worker.hip4.instance_at`),
so a boot roll for an instance the previous run also announced is read as
what it is. `hl-events.pmlr` of a run therefore starts with up to eight
roll rows; `rolls_total` (the ingress counter) does NOT count them — it
still counts venue rolls only.

## 2026-09-19 — `bin15.toml` gains an OPTIONAL `entry_min_px_1e6` (the coverage entry's price floor); `BIN15_KEYS` 24 → 25 (BIN15 R0)

**No wire change, no artifact re-cut required.** A `bin15.toml` without
the key parses and boots bit for bit as before: absent = 0 = no floor.
Present, the value must be in `[0, 1000000)` (a floor at or over 1.0
could never be cleared and would switch the entry arm off in silence —
refused at parse with its line). The member refuses a coverage entry
whose preferred-side ask is under the floor on the existing
`skipped_entry_price` counter, without burning the instance (the next
reprice asks again), exactly as the price bound does.

Why: the 2026-09-13→18 paper tape (vault doc 21) shows the entry's losses
concentrate where the venue disagrees with the model — preferred-side
asks under 0.50 hit 39 % against a 47.6 c price — and its wins where the
two agree on a favourite (0.70+, 85 % against 79.6 c). The testnet
research artifact sets `500000` (0.50) or `700000` (0.70); the shipped
example carries `0`.

Surfaces: `core_config::bin15::Bin15File.entry_min_px_1e6`,
`strategy_bin15::Bin15Params.entry_min_px_1e6`, the boot tell's entry
law (`entry=every-15m@$12<=p_hat-2c&ask>=50c`; unchanged at 0),
`claude_worker.bin15_fit.KNOBS` (the fitter writes the key; the example
was re-rendered — one added line). `bin15_ref` / the parity fixture are
untouched: the pricer did not change.

Also recorded here because it bites at R0: `emit_take` floors the size
to whole contracts and `on_grid` then requires notional ≥
`GRID_MIN_NOTIONAL_1E6`, so an entry that does not divide the floor
exactly can land under it and be `skipped_grid`. (Written on 2026-09-19
against a $10 floor, which was wrong — the venue's floor is 1 USDC; the
corrected arithmetic is in the next entry.)

## 2026-09-19 — Real-execution lane E5–E7: `Order.verb`@42 + `prev_client_oid`@56, `Fill.flags`@15, `HaltSignal` 32 → 40 B, `ExecCounters` grows, `/state` `exec` gains `ledger_*`/`arm_*`, `exec.toml` gains a REQUIRED `halt_on_recon_stale_ms`, `core-metrics::MAX_COUNTERS` 256 → 512

Recorded at the E7 review (2026-09-19); the E5/E6 phases landed the wire
changes without an entry here. **No PMLR version bump** — every field is
wire-additive into bytes that were explicit zeroed padding, and 0 is the
meaning every pre-existing record already had.

**What changed — on-disk (`engine-orders.pmlr`, `engine-fills.pmlr`)**

- `Order.verb` at offset 42 (`u8`; E5 commit 4a): `0` PLACE / `1`
  CANCEL / `2` MODIFY. Every Order captured before E5 reads `0`, which
  is what every one of them was. `Order.prev_client_oid` at offset 56
  (`u64`): the resting order a CANCEL / MODIFY acts on, `0` for a place.
  A CANCEL record carries `px`, `qty`, `side`, `kind`, `client_oid` all
  zero. **The capture IS replayable with cancels in it** — the harness
  (`BacktestCtx`, `FillEngine`, `PaperMatcher`) consumes all three verbs
  from ONE stream; a `cli/src/paper.rs` doc claiming otherwise was
  corrected in this pass. `docs/wire-format.md` carries the byte map.
- `Fill.flags` at offset 15 (`u8`; E4): bit 0 = `FILL_FLAG_SETTLEMENT`
  (a venue settlement print, px 1.0 / 0.0). Pre-E4 fills read `0` = no
  flags. `Fill.origin` (offset 14, `FILL_ORIGIN_VENUE` = 0) is unchanged.

**What changed — in-process ABI (no file)**

- `clob_dispatcher::HaltSignal` 32 → **40 B**: `recon_age_ns: u64`
  appended (0 = no successful reconciliation yet — the "0 = no
  observation" rule `ws_gap_ns` uses). `HaltSignal::new` takes 7 args.
- `clob_dispatcher::ExecCounters` gains `cancel_on_off`, six
  `ledger_*` counters and `arm: LiveArmCounters` (24 `u64` + a
  `budget_remaining: i64`). `OrderDispatch::arm_counters()` is a new
  trait method with a default (zeros); only `HlExchange` implements it.
- `core_net::Transport::flush()` is a new trait method with a default
  no-op; `TlsTransport` drains `write_tls` to `WouldBlock`.
- `exec_router::HaltLimits::new` takes 5 args (`recon_stale_ms` last);
  `HaltReason::ReconStale = 7` (`"recon-stale"` in `exec.HALT` and the
  snapshot's `HALT_REASON_WORDS[7]`). `ExecRoute` stays 512 B (halts at
  offset 256).
- `core_metrics::MAX_COUNTERS` 256 → 512 (the fixed registry; the arm's
  24 + the ledger's 6 counters did not fit).
- `core_types::roll_kind_strict(seq) -> Option<bool>` — the ONE reader
  of the roll kind byte; `0x03` and every other unknown kind is `None`
  and refused by all three live consumers.

**What changed — operator surfaces**

- `~/multivenue/exec.toml`: `halt_on_recon_stale_ms` is a NEW key,
  **REQUIRED non-zero on every `mode = "live"` slot** (example 300000).
  A live slot without it REFUSES the boot — deliberate: an arm whose
  reconciliation has gone stale must halt, and a config that cannot say
  when is a config that never halts. `off` slots now let CANCELS through
  to the arm (`cancel_on_off` counted); submits/modifies stay refused.
- `/metrics`: `engine_exec_ledger_{fills_unbound,sells_below_zero,binds_refused,resting_full,resting_ambiguous,settles_unmatched}_total`;
  `engine_exec_hl_{submitted,rejected,refused_local,refused_stale,sent_unanswered,fills_booked,fills_unresolved,fills_foreign,fills_dropped,fills_refused,fills_scan_failed,fills_unowned,recon_ok,recon_failed,recon_drift_legs,recon_unseen_legs,sweep_left,sweep_stalled,cancel_all_unqueued,ws_reconnects,ws_connect_failures,rolls_bound,rolls_refused,owner_contested}_total`;
  gauge `engine_exec_hl_budget_remaining`.
- `/state` `exec` object (additive, `"v": 1` untouched): `ledger_fills_unbound`,
  `ledger_sells_below_zero`, `ledger_resting_full`, `ledger_resting_ambiguous`,
  `arm_fills_booked`, `arm_fills_dropped`, `arm_fills_unresolved`,
  `arm_sent_unanswered`, `arm_recon_ok`, `arm_recon_failed`,
  `arm_recon_drift_legs`, `arm_recon_unseen_legs`, `arm_sweep_left`,
  `arm_budget_remaining`.
- Boot tells: `caps-ENFORCED order=$… open=… day=$… instance=$…` and
  `HALTS reject_streak=… asset_refusals=… recon_drift=$… recon_stale_ms=… ws_gap_ms=… budget_floor=… halt_file=…`
  (pinned); `exec: hyperliquid arm ARMED host=… network=testnet|MAINNET …`;
  a boot with `HYPERLIQUID_WS_HOST` and `HYPERLIQUID_EXCHANGE_HOST` on
  different networks is REFUSED (LAW E-4's precondition).
- New gate: `make copy-audit` (`scripts/copy-audit.sh` +
  `scripts/copy-audit-baseline.txt`); new command `/copy-check`; new
  agent `.claude/agents/zero-copy-auditor`.

**Why**

- E5 needed the verbs on the tape (one stream, so a cancel can never be
  replayed before its own place); E6 needed the arm's counters and the
  ledger's alarms on a surface; the E7 review found `reconciled` could
  latch on a parse and never un-latch, which is what `recon_age_ns` +
  the required key close.

**Impact / migration steps**

1. Pre-E5 captures replay unchanged (every new field reads 0 = place /
   no flags). No tool needs a flag.
2. Before the first `--exec` boot with a live slot: add
   `halt_on_recon_stale_ms = 300000` (or the operator's number) to every
   live `[exec.slot.<n>]`, or the boot refuses with the key named.
3. Dashboards reading `/state.exec` gain keys; nothing was renamed.

**Rollback**

- A binary before E5 reads `verb`/`prev_client_oid` as padding and
  replays every record as a PLACE — a capture with cancels in it is
  therefore NOT correctly replayable by a pre-E5 binary (it would see
  the cancel's zero-priced record as a place of nothing). Keep the
  post-E5 binary for post-E5 captures.
- `exec.toml` files with `halt_on_recon_stale_ms` are refused by a pre-E7
  binary as an unknown key (law 1: every key is KNOWN); remove the line
  to boot an older binary.

## 2026-09-12 — `audit-pnl` settles HIP-4 binaries; the calibration ledger; the boot seed hook (BIN15 O5)

**No wire change, no schema bump, no restart.** `audit_pnl_version` stays
1, `detail_version` stays 7, and `docs/wire-format.md` is untouched by
this phase — stated explicitly because the phase before it DID change the
wire. What changes is a REPORT surface's arithmetic on roots that carry a
HIP-4 instrument, and no root does until the operator's go-live restart.

**The prerequisite O3 named.** O3's deviation 1 recorded that the
`audit-pnl` surface was not wired for binaries and that this was "an O5
prerequisite, not a silent gap", because nothing produced a binary fill
for it to score. It does now, so a position held through its expiry
becomes CASH at the payout instead of marking out at a book the venue
CLEARED at `T`. On the synthetic root that pins it, the difference is
+$60 against −$19 on the same tape.

**The collector is not the backtest's, and that split is deliberate.**
`backtest` reads instances and marks out of its own `merged` timeline,
where a sym is the universe's ordinal and the No leg is therefore
`sym_yes + 1`. `audit-pnl` INTERNS by descriptor, so neither holds: the
legs are paired by NAME (`binary::no_descriptor_of`) and the underlying
is found by name (`binary::underlying_descriptor_of`). Both surfaces then
call ONE law, `binary::apply_binary_settlements` — which
`register_binary_model` is now expressed in terms of. Two collectors, one
law; not two laws.

- **The schedule is built in its own pass and never enters the event
  stream.** Nothing in the replay loop consumes a `Mark` or an
  `InstrumentRoll`, so admitting them to `evs` would change the merged
  stream — and therefore the regime replay's input and every number
  downstream — to carry records nobody reads. **`Mark` is kept ONLY for
  a HIP-4 underlying the manifest actually carries**, the same
  restriction the backtest merge got at O4b and for the same reason
  (`Mark` is also OKX's mark-price channel). **Guard: the 8-window pool's
  `audit-pnl` JSON is byte-identical across this lane at
  `725d1d2748de79df`.**

- **A roll whose UNDERLYING is not in the manifest is counted, not
  guessed** (`unpaired_rolls`) — there is no mark series to settle
  against, so its positions mark out and the report says so. The `[no]`
  row is deliberately NOT required: its descriptor is derived from the
  Yes leg's by name, a settlement registered for a sym nobody traded is
  harmless, and refusing the whole instance over a missing manifest row
  would make a perfectly settleable Yes position mark out.

- **New stderr line, silent when there is nothing to say:**
  `audit-pnl: bin15 settlement table: N of M instance(s) settleable (K reaching their instant) from R roll row(s), marks=… unpaired_rolls=…`.
  SETTLEABLE is the number that matters — an instance can reach its
  settlement instant inside the window and still have no payout, because
  the window does not hold enough of its underlying's marks to average.
  Every root captured before slot 3 goes live prints nothing at all.

- **`pnl_report`'s `bin15` row needed no change.** It reads `label`
  straight from the audit-pnl JSON, and `strategy_label(3)` became
  `"bin15"` at O4b. The day report carries the row the moment slot 3
  emits an order.

- **`claude_worker.bin15_ledger` (NEW)** — merges `--emit-detail`
  sidecars into `~/multivenue/worker/bin15/ledger.tsv` (worker state,
  never git) and reads G6.1 out of it: the per-phase calibration table
  and a PASS / FAIL / INSUFFICIENT verdict, exit 0 only on PASS (the
  regime-soak law — a gate that exits 0 on "not measured yet" is a gate a
  script passes by not measuring). Merging is deduplicated by
  `(ts_ns, family, outcome)` and EXISTING wins, because a re-cut window
  can carry a row whose `y` is unknown again and letting it overwrite
  would quietly un-settle the ledger.

  **What it cannot answer, recorded so nobody asks it to:** G6.2 (Arm A
  ≥ +2 c/contract) and G6.3 (Arm B paired model − null ≥ 0) are P&L
  questions about FILLS and are read from the day report's `bin15` rows.
  A per-arm Brier score off the ledger would look like an answer and
  would not be one — the member computes the same `p_hat` under both
  arms, so their beliefs are the same distribution by construction.

- **`scripts/engine-wrapper.sh` cuts the bin15 seeds before every boot**,
  beside the regime and xsd hooks and with the same best-effort law (no
  `bin15.toml` ⇒ nothing to export; a failure leaves the member to warm
  live). TWO files per coin: a pair belongs to one tenor.
  `claude_worker.bin15_seed.DEFAULT_DB` is `~/multivenue/worker/candles.db`,
  the same spelling `xsd_author` uses — the sibling `~/multivenue/candles.db`
  is a 0-byte stray on this host and seeding from it would produce an
  empty seed and a silent cold boot.

**DEFERRED, deliberately.** `engine-snapshot`'s `Bin15Snapshot` (spec
§7.4, marked optional there): the per-family gauges
`engine_bin15_f<n>_{p_hat_1e6,pos_yes_1e6,pos_no_1e6,live_outcome}` already
publish the same four numbers, and `/state` carries a byte-exact `"v": 1`
contract that is not worth moving for a duplicate view before the member
has traded once.

**STILL THE OPERATOR'S, and this session did none of it:** installing
`~/multivenue/bin15.toml`, adding the eight families to `universe.toml`
`[hyperliquid] rolling`, setting `strategy.conf` to mask 62, and the
restart. The desk gate (G6.1–G6.4) cannot be evaluated until live capture
accrues ≥ 200 settled instances; the 8 standing pool windows carry no
HIP-4 instrument, so `--member bin15` on them yields an empty ledger.

## 2026-09-12 — `crates/strategy-bin15` takes slot 3 from `strategy-rule-tree`; `AiCmdKind::SetBinarySpec = 13`; `bin15.toml`; `--member bin15` (BIN15 O4b)

**This phase DOES change the wire** (unlike O3 and O4a): `AiCmd.kind`
gains `13 = SetBinarySpec`. It is additive — byte 13 was unassigned, the
frame is the same 64 bytes, and no existing kind moved — so an old
worker and a new engine interoperate in both directions. `docs/wire-format.md`
carries the row.

**THE HEADLINE IS THE SLOT BOUNDARY.** `SLOT_BIN15 = 3` / `BIT_BIN15 = 8`
replace `SLOT_RULE_TREE` / `BIT_RULE_TREE` (ruling O-Q1, the XSD-S
precedent). `strategy-rule-tree` is UNLINKED from `strategy-set` — the
crate and its tests stay in the tree — and **`--strategy rule-tree` is
now a boot refusal**, `mask_for_name("rule-tree")` returning `None`.
Mask 8 means bin15 from this commit on, and a mask NUMBER recorded
before it does not mean what it says: `audit-pnl`'s slot label 3 reads
`"bin15"` where it read `"rule-tree"`, so a report over capture from
before this commit mislabels slot 3. Nothing ever ran in that slot
live (`all` has been 123/127 without it), so the boundary is
book-keeping rather than data loss — but it is a boundary, and the date
is this commit's.

New mask names: `bin15` 8, `ai+bin15` 56, `ai+vrp+bin15` 58,
`ai+xsd+bin15` 60, `ai+vrp+xsd+bin15` 62. `scripts/engine-wrapper.sh`
allow-lists all five. **bin15 REFUSES a boot whose artifact is absent
when its bit was requested** (the icdp/F19 law) where xsd clears the bit
instead — booting `ai+bin15` as `ai` is how an operator comes to watch a
member that was never there.

**What changed, beyond the slot**

- **`bin15.toml` + `bin15.toml.example`** (`core_config::bin15`) — the
  artifact, integer-only, hashed by BYTES into the boot tell
  (`bin15: artifact configured hash=… families=… dormant=… seeds=… daily_seeds=…`).
  The three lookup tables and the knobs are DATA, re-cut without a
  rebuild by `python -m claude_worker.bin15_fit artifact`.

  **Every array is ONE LINE, deliberately.** `core_config::icdp::parse_value`
  (which this grammar reuses) reads an array off a single trimmed line;
  a pretty-printed multi-line array is an unterminated array to it. The
  committed example's `phi_lut` line is 32 786 characters and that is
  not an oversight to tidy up.

- **DEVIATION, recorded: the lookup tables' provenance.** Spec §6.3 names
  `bin/bt15.py`'s walk-forward fit as the source. That one-shot is
  git-excluded research and is not in this tree, so it cannot be
  imported or re-run. It does not need to be — the spec states every
  number: Φ is the standard normal CDF (mathematics, not a fit),
  computed exactly in `decimal` at 60 digits; the recalibration is the
  three stated slopes 1.104 / 1.165 / 1.219 through the middle of the
  unit interval, clamped at both ends; `scale_1e9 = 980000000`;
  `hour_ln_off_1e9` is OMITTED, and absent means zero, which is
  bit-identical to no hour-of-day correction. An operator re-fitting
  later passes `--slope-early` / `--scale-1e9`; no code changes.

- **`scale_1e9` was refused by its own grammar.** `BIN15_KEYS` listed 20
  keys and not that one, while `parse` read it with `opt_int` — so the
  ONE artifact the fitter writes was rejected at the key check before
  any bound could be tested, and every bound-level test passed. Fixed
  (21 keys); `core_config::bin15` now compiles in
  `bin15.toml.example` and asserts it parses, which is the test that
  catches this class.

- **DEFECT FIXED: σ̂ was 10 000× too large.** `refresh_sigma` squared
  `core-vol`'s σ̂ and divided by the tenor's minutes. But `core-vol`
  reports vol in RAW BPS ×1e9 — a fraction ×1e13, because one bp is
  1e-4 — while `price::fair_value` wants a per-minute variance as a
  fraction² ×1e18. The square therefore had to come down by 1e8
  (`BPS2_TO_FRAC2_1E8`). Without it every `d` collapsed toward zero and
  the member priced EVERY binary at almost exactly 0.5, while
  `reprices`, `takes_submitted` and `takes_filled` all climbed exactly
  as they would if it were working — a spread harvester with a
  model-shaped counter set. Pinned from both ends by
  `the_per_minute_variance_is_in_the_pricers_units_not_core_vols`: an
  exact identity against `sigma_hat_1e9`, and a realistic 25 bps lead
  landing at 1.6σ rather than a rounding error. **Nothing short of
  predicting the number catches this**, which is why the harness arm
  was what found it.

- **DEFECT FIXED: `log_moneyness_1e9` overflowed `i128`.** `u * u` ran
  BEFORE the range check, so a mis-parsed `threshold:` of 0.000001
  against a BTC mark (`u ≈ 7.7e19`, square 6e39 against a 1.7e38
  ceiling) panicked in debug and wrapped in release. The range check now
  comes first (`U_CLAMP_1E9`, `|u| ≤ 100`), and beyond the band the
  answer is `None` — ABSENT DATA HOLDS — rather than a saturated
  near-certainty the member would cross a book for. Found by the parity
  fixture, which feeds it deliberately.

- **DEFECT FIXED: one seed file cannot serve two tenors.** Boot pushed
  the same `(x, y)` cloud into BOTH forecast engines of an underlying. A
  pair is tenor-specific — `x = ln har_τ`, `y = ln` realised over that
  same `τ`; the 15 m tenor folds four HAR windows and the 8 h tenor
  folds three — so the daily line was fitted on the 15 m regressor, and
  because both are log-vols of the same series the result looked
  plausible. **New on-disk file: `bin15-seed-<COIN>-1d.tsv`**, the 8 h
  tenor's pairs, read beside `bin15-seed-<COIN>.tsv` and pushed only to
  `FAMILY_NATIVE_DAILY`. Both files are OPTIONAL and absent means that
  tenor holds until its own pairs accrue, which is the same law an
  absent seed always had — so **nothing to migrate, and a host with only
  the 15 m file loses nothing it had**. The minute window stays in the
  15 m file alone: it is a property of the price series, not of the
  horizon, and replaying one underlying's minutes twice would push them
  through a ring that assumes chronological order. `V 2 / R / P` is the
  VRP seed grammar unchanged, written by `vrp_seed.write_seed_tsv`
  itself rather than a second copy of it.

- **`backtest --member bin15`** (`--bin15 <toml>`, `--bin15-seed-dir <dir>`)
  on the Tier-3 arm. Offline the artifact's own `families` list plays
  the part `universe.toml`'s `[hyperliquid] rolling` plays at boot, and
  the Yes ordinals are a pure function of the family index
  (`family::rolling_sym`), so a replay rebuilds them without the file.
  The order check still has something real to check — **the CAPTURE's
  family indices**: a window that rolls family 5 against an artifact
  configuring four refuses the run, because binding nothing for it and
  reporting a clean zero looks exactly like a member with nothing to do.
  `--bin15-seed-dir` defaults to the FIRST run directory, not
  `~/multivenue`: a replay is a closed world, and folding the live cut
  into a backtest of a month-old window replays a forecast that had not
  been fitted yet.

- **HARNESS MERGE WIDENED, and the restriction on it is load-bearing.**
  `load_run` kept only `Funding` and `AssetCtx` event channels, so
  `InstrumentRoll` and `Mark` never reached the merge — O3's settlement
  registration and the bin15 member both read `merged`, so a capture
  full of rolls merged to nothing. `InstrumentRoll` is now kept
  unconditionally; **`Mark` is kept ONLY for a HIP-4 UNDERLYING**.
  That is not tidiness: `Mark` is also OKX's mark-price channel, which
  every historical root carries in bulk and which no consumer reads, and
  admitting those would add records to the merge on every root ever
  captured — moving `merged_records`, the IS/OOS boundary and therefore
  every pooled VM number. A root with no HIP-4 instrument has an empty
  underlying set and merges byte for byte as it always did; the standing
  8-window pooled guards were re-run at this commit and are unchanged.

- **The calibration ledger.** `--emit-detail` gains an additive
  top-level `bin15_ledger` array on `--member bin15` runs only
  (`{ts_ns, family, outcome, tau_ns, p_hat_1e6, p_raw_1e6, arm, y}`,
  one sample per live instance per 30 s). `detail_version` STAYS 7: a
  key that appears only on one member's runs is additive, and a bump
  would move the pooled sidecar guard and the three tests that pin the
  prefix. `render_detail` gained a trailing `extra: &str`; the VM path
  passes `""`, so its bytes are unchanged. A row is written only when
  the record it sits on actually RE-PRICED the instance — `p_hat`
  survives a held re-price (the tail, a cold forecast, a one-sided
  book), and a sample taken whenever one merely exists records a belief
  nobody acted on against a `tau` it no longer has. `y` is joined from
  the harness's own settlement map, so a `p̂` is scored against the
  number the fill model paid out; `null` where the window cannot derive
  the payout.

- **`engine_bin15_*`: 22 counters + 32 per-family gauges**
  (`engine_bin15_f<0..7>_{p_hat_1e6,pos_yes_1e6,pos_no_1e6,live_outcome}`).
  Registration is unconditional like every other family's, so a mask
  without slot 3 still exposes the rows at zero — which is what lets an
  operator tell "off" from "broken". **Headroom note, measured at this
  commit: the live engine registered 198 counters before this family
  and 220 after, against `MAX_COUNTERS = 256` — 36 left.** The next
  member that needs more has to raise the constant rather than discover
  `RegErr::Full` at a live boot, which refuses the boot. Pinned by
  `the_bin15_family_is_22_counters_and_32_gauges`.

- `strategy-core` gains `Bin15FamilyView` + `bin15_counters()` /
  `bin15_families_view()` trait defaults; `engine-snapshot`'s
  `SLOT_NAMES[3]` is `"bin15"`; `regime.toml`'s `[labels.rule_tree]`
  section is now `[labels.bin15]` — **an operator file carrying the old
  key is refused at the grammar**, which is the one config edit this
  commit requires of a host that had one (no live `regime.toml` did).

**No restart is required by this commit and none was performed.** Slot 3
is unconfigured without `~/multivenue/bin15.toml`, which no host has;
the live mask is 54 and does not include bit 8. Going live is O5.

## 2026-09-12 — `core-vol` gains the 15 m tenor and a fourth HAR window; 4 h/8 h bit-identical; `sigma_hat_1e9`; IV-optional arming pinned (BIN15 O4a)

**No wire layout, no on-disk format, no config key, no restart.** The
persisted vol window is a series of RETURNS (`ret_chrono` out,
`seed_return` in) and the rolling sums are rebuilt from it, so the new
15-minute sum warms itself on the next boot from the file that is
already there. Nothing to migrate; this entry exists because a HAR
window is a law, and because the 4 h/8 h forecast had to be proved
unmoved rather than asserted.

**What changed**

- **`HAR_WINDOWS` is `[15, 60, 240, 1440]`** — the 15-minute term
  appended at the FRONT, and `sum_sq` widened to four. `HAR_WARM_MINUTES`
  is still the LAST element, so warm-up is still 1440 minutes.

- **Which windows a tenor folds is now `Tenor::first_window`, not the
  array's length.** 4 h and 8 h carry `first_window = 1`: they fold
  `w ∈ {1, 2, 3}` — the same three squares over the same three windows
  — and divide by the same 3. 15 m carries `0`: four terms over 4.
  `har_1e9` divides by `HAR_WINDOWS.len() - first_window`, so the
  divisor is not a literal any more and cannot drift from the fold.

- **`tenor_of` accepts `900_000_000_000`** with
  `ANNUALISE_15M_1E9 = 187_253_838_412`. That constant comes out of the
  same 525 960-minute year as the other two and is rounded the same way:
  `sqrt(525960/15) × 1e9 = 187_253_838_411.93`, against
  `...602.98 → 46_813_459_603` (4 h) and `...736.07 → 33_102_114_736`
  (8 h). A test reproduces all three from `isqrt(525960/τ_min × 1e18)`.
  The doc above `tenor_of` now says WHY 15 m is allowed where 12 h is
  not: kill criterion 4 forbids the longer *IV* cells because E1 is
  measured at 4 h/8 h and gone by 12 h; BIN15's comparator is a venue's
  binary book, not an implied vol, so kill-4 does not bind it.

- **`VolEngine::sigma_hat_1e9(tau_ns)`** — `exp(ln σ̂)`, raw `bps ×1e9`,
  no annualiser and no band. `bounds` exists for the VRP lane, which
  compares against a Deribit implied vol; BIN15 needs the per-τ vol
  itself to build a per-minute variance for its binary pricer. Same
  saturation law as `annualised_1e9` (both ends of `exp_1e9`'s range
  mean "the fit ran off the table"), and a cold-path contract: once per
  minute per underlying, never per tick.

- **IV-optional arming is PINNED, not added.** `arm_hold_at_with_offset`
  already stored `pend_rv_iv_1e9 = 0` for `mark_iv_1e9 <= 0`, and
  `observe_settlement` already scored QLIKE only when
  `pend_rv_iv_1e9 > 0`. A HIP-4 outcome market has no implied vol, so
  that is the normal BIN15 case, and it is now a documented law with a
  test on both sides: the hold still forms its `(x, y)` pair and still
  refits — that is the forecast the member trades — and scores NO QLIKE
  row, because scoring a zero would make the HAR look infinitely better
  than an IV nobody quoted.

**BIT-IDENTITY GUARD — PASSED, and stronger than a diff.** The
instruction was to regenerate `parity-1` and diff every 4 h/8 h row
against HEAD's. Instead the file was **never regenerated**:
`claude-worker/tests/fixtures/vol/parity-1.expected.tsv` is untouched at
its HEAD bytes and both halves of the parity pair are green against it
(`cargo nextest run -p core-vol --test parity`, `uv run pytest
tests/test_vol_ref.py`). A file that was never written cannot have been
written back to the same content by accident, so this proves what a diff
would have proved and one thing more.

To keep it provable, `CORE_VOL_PARITY_WRITE` now takes a FIXTURE NAME
(`=parity-15m`); `=1` still writes all of them, for a deliberate law
change. A lane adding a fixture names its own and cannot silently
re-bless the other lane's tape.

**New fixture `parity-15m`** (19 `Q` rows, 17 `G` rows, 1 658 closes, so
the minute ring wraps): the cold row, a warm-ring-unfitted row, the fit,
**eight IV-less holds settled from the engine's own window with
`qlike_n` staying 0**, four IV-bearing holds taking it to 5, an `O`
regime offset in force, a `D` disarm forming no pair, an explicit `S`
settlement and a settlement with nothing armed. `G` is a NEW op with its
OWN row shape (`row G sigma_hat ln_sigma_hat`) — deliberately not a new
column on the `Q` row, because a column would have changed every
`parity-1` row and destroyed the guard above.

**Python mirror** (`claude_worker.vol_ref`): `HAR_WINDOWS`, `sum_sq`
sized from it, `tenor_of` returning a 3-tuple `(tau_min, annualise_1e9,
first_window)` — existing callers index `[0]`/`[1]` and are unaffected —
`har_1e9` folding from `first_window`, and `sigma_hat_1e9`.

**Operator-visible consequence, deliberate and worth knowing.**
`tenor_of` is the validator for `vrp.toml`'s `tau_ns`
(`core_config::vrp`) and for `Bin15Params`/`VrpParams` at boot. So a
`vrp.toml` written with `tau_ns = 900000000000` is now ACCEPTED where it
used to be refused. No existing file changes meaning and no live config
uses that value; the VRP lane's own files were not touched. Flagged
rather than guarded because the guard would have to live in
`core-config::vrp`, which this lane must not edit, and because
`tenor_of` is deliberately one shared list of the tenors the crate can
forecast — not a per-lane allow-list.

**Tests pinning it:** `the_15m_window_is_additive_for_the_longer_tenors`
(folds the three long windows by hand and demands `har_1e9` to the unit,
then demands 15 m differ), `sigma_hat_is_the_exponential_of_the_log_forecast`,
`an_iv_less_hold_forms_a_pair_and_no_qlike_row`,
`only_the_measured_tenors_exist` (extended with the `first_window` split
and the annualiser reproduction) — each with a same-named mirror in
`claude-worker/tests/test_vol_ref.py`.

## 2026-09-12 — harness: the HIP-4 price/size grid, per-instance binary settlement from `InstrumentRoll` + `Mark`, charge-once on the settlement leg (BIN15 O3)

**Offline only.** No engine code, no wire layout, no capture file, no
restart. Every existing root replays byte for byte — see the guard
below.

**What changed**

- **The venue's GRID is now refused, not filled.** A `Prediction`-class
  order on Hyperliquid is rejected when it is off the 1e-4 price tick,
  a fractional contract, outside `[0.001, 0.999]`, or under the 10 USDC
  minimum — counted in the new `ModelOutcome::prediction_grid_refused`.
  The venue would refuse these outright, so filling them invents P&L
  the strategy could never have had. The two constraints INTERACT: at
  0.001 the notional floor takes 10 000 contracts, so a 100-lot order
  at the bottom of the band is refused for its size, not its price.
  Keyed on the KNOWN class, like `fee_rate_for` — an unknown-class sym
  is not gridded, because guessing a venue's tick from a shape the
  descriptor law could not read is how a harness silently drops orders
  it should have filled. Every other venue and class is untouched.

- **Per-instance settlement: a QUEUE per sym, not a field.** This is
  the one real difference from the option law. An option settles once
  and stays settled; a rolling HIP-4 slot hosts 96 instances a day, so
  `FillEngine` gained `set_binary_settle(sym, BinarySettle { halt_ns,
  settle_ns, value_1e6 })` — entries sorted by `halt_ns`, consumed
  oldest-first. At `halt_ns` the sym enters the F12 guard set (the
  venue clears the book at expiry, so no fill may happen); at
  `settle_ns` the mark is pinned to the payout, whatever is open closes
  at it, and **the sym comes back OUT of the settled set** so the next
  instance trades. `ModelOutcome::binary_settled` counts the
  settlements, and unlike `opt_settled` it can exceed the number of
  syms.

- **The payout comes from our own tape.** New
  `crates/cli/src/backtest/binary.rs` reads the run's
  `ChannelId::InstrumentRoll` events (created rolls only — the settled
  rows name the instance that is ending, which the created row already
  described) and computes each instance's value from the
  `ChannelId::Mark` series of its UNDERLYING perp: the MEAN over
  `[expiry, expiry + twap]`, or the last mark at or before the expiry
  when the family settles at `T`. `>= strike` settles ITM (the venue's
  own rule).

  **An instance whose evidence this window does not hold is COUNTED,
  never guessed.** Fewer than 3 marks in the window, or a settlement
  instant past the window's end, leaves it unregistered: its position
  marks out at the last book price, which is the honest answer when the
  payout is unknown. Scoring it as worthless would be a 100 %
  directional opinion dressed up as arithmetic. The stderr report line
  `binary: instances=N settled=M unsettleable=U` is what an operator
  reads for this, and `unsettleable > 0` bounds what a binary member's
  P&L can be said to mean over a ≤ 2 h window that cuts through
  expiries.

- **A slot's underlying comes from the DESCRIPTOR**, not the event:
  `hyperliquid:out:BTC:15m[yes]` names both the slot and the coin, and
  the same table carries `hyperliquid:BTC`. The roll event carries a
  family INDEX, which means nothing across runs — `binary::underlying_map`
  is the resolution, built from the load pass's existing
  descriptor→sym join.

**BYTE-IDENTITY GUARD — PASSED.** The standing 8-window VM pool,
before and after O3 on the same command
(`backtest --ruleset fde6f733… --replay-dir ~/multivenue/worker/windows/
--split 0/100 --emit-detail`):

| artifact | sha256 (16) | verdict |
|---|---|---|
| schema-1 stdout | `188d18e3b1ded762` | unchanged |
| detail sidecar | `e3f6b8efd7b7a1d0` | unchanged |

`188d18e3…` is the same schema-1 hash the VRP lane recorded for this
pool at P1 and P2 — so BIN15 O1, O2 and O3 have now all left it
untouched. The `binary:` line is ABSENT on that root (no rolling slots
in it), which is what keeps its stderr identical too.

**Deviations from the plan, recorded**

1. **The `audit-pnl` surface is NOT wired** (the plan names three).
   That surface has its own loader, which today admits only Funding and
   AssetCtx channel events into its `Payload`; admitting
   `InstrumentRoll` and `Mark` means widening its event filter and its
   interner keying. Nothing produces a binary fill for it to score
   until O5 enables paper, so this is an **O5 prerequisite**, not a
   silent gap. `backtest` and `backtest --member` both have it.
2. **No `SynthFill.origin` field.** The plan asked for a settlement
   `SynthFill` tagged `settlement`; `SynthFill` has no origin concept,
   and adding one would change the `--emit-detail` schema. The option
   precedent closes through `Book::settle` and counts it, which is what
   binaries do — a settlement is not a market fill, and the sidecar's
   fill rows stay fills.
3. The §5.2 integration test runs over a synthetic MERGED timeline
   in-crate rather than a written run dir: driving an order into a PMLR
   root needs a VM ruleset that trades a prediction descriptor, and no
   member does until O4. The manifest join and the PMLR I/O it skips
   are already pinned by `crates/cli/tests/backtest_harness.rs`.
4. `unpack_roll_seq` is restated in `backtest::binary` rather than
   imported: the harness must not depend on an ingress crate (the
   `OptSettleRef`-restates-the-intrinsic precedent).

## 2026-09-12 — Hyperliquid rolling-instrument families: `HL_MAX_COINS` 16 → 32, the ack mask splits, `ChannelId::InstrumentRoll = 13`, HL `Mark` rows (BIN15 O2)

**What changed**

- **`ChannelId::InstrumentRoll = 13` (NEW, additive).** A rolling HIP-4
  family's `SymbolId` slot is stable for the life of the process while
  the venue instrument under it changes — 96 times a day for the BTC
  15-minute family. `instrument-manifest.tsv` is two columns by law and
  names only the slot, so this event is the ONLY record of which
  instance a slot meant at a time. `venue_seq` packs
  `outcome | twap_s << 32 | family << 48 | settled << 56`; `sym` is the
  family's Yes slot; `v0` strike ×1e6; `v1` expiry ns. Two events per
  roll, in the venue's order: the outgoing instance's `settled`, then
  the successor's `created`. `ChannelId::from_u8(13)` now resolves —
  **a reader that treated 13 as corrupt must be updated**; nothing in
  tree did.

- **Hyperliquid now emits `ChannelId::Mark` rows** (channel 2, which
  already existed for OKX/Binance), beside its AssetCtx row and off the
  same `activeAssetCtx` frame: `v0` = `markPx` ×1e6, `v1` = `oraclePx`
  ×1e6. The ctx parser always lifted both and capture never carried
  either — and a HIP-4 strike IS the perp mark at creation while its
  settlement is a mark TWAP, so neither was reconstructible from our own
  tape. **CAPTURE SIZE: ≈ 3 rows/s per perp ⇒ ≈ 16 MB/day/perp of
  `hl-events.pmlr`** (≈ 66 MB/day for four perps). Always captured;
  on the event lane only when rolling families are configured.

- **`HL_MAX_COINS` 16 → 32, and the ack mask SPLIT in two.** A family
  costs two coin rows (Yes and No), and the eight ruled families would
  alone exhaust 16. 32 × `CHANNELS_PER_COIN` = 128 fills `MaskBits`
  (`u128`) EXACTLY, so the two venue-global ack bits no longer fit above
  the per-coin ones: `ALL_MIDS_BIT` / `OUTCOME_META_BIT` are now
  `GlobalBits` (`u8`) values 1 and 2, `expected_mask` returns
  `(MaskBits, GlobalBits)`, and a session verifies on
  `found == expected && found_global == expected_global`. In-process
  only — no captured byte changes. `MAX_SUBS` follows the constant to
  130, and **`TX_BUF_SIZE` 16 KiB → 64 KiB**: 130 subscribes queue in
  one drive cycle (≈ 13 KiB, which 16 KiB barely covered) and a roll
  queues 12 more mid-session; a full tx is a session-killing
  `io::Error`, so the margin is worth the 48 KiB.

- **Coin-table rows may now be EMPTY.** `HlCoinTable::reserve(sym)`
  appends a reserved row (stable sym, no instrument) and `rebind(idx,
  coin)` points it at one, keeping the sym; `lookup`, `index_of_coin`,
  `expected_mask` and the subscribe sweep all skip empty rows, so a
  DORMANT family costs nothing on the wire and is not waited on for
  verification. `CoinTableErr` gains `NoSuchRow` (a programming error,
  not operator input) — **exhaustive matches on that enum need the new
  arm**; the one in `build_hl_coin_table` was updated.

- **New universe key `[hyperliquid] rolling`** —
  `<out|native>:<COIN>:<15m|1d>`, capped at 8, unique, validated at
  parse. **Absent or empty is the pre-BIN15 boot, bit for bit.** The
  crossed forms (`out:…:1d`, `native:…:15m`) are REFUSED: neither
  exists on the venue, and accepting one would mean guessing its
  settlement law. Each entry allocates TWO manifest rows —
  `hyperliquid:out:BTC:15m[yes]` / `[no]` — from an ordinal pool based
  at **4096**, above every `coins` ordinal, so no configured sym moves.
  Config-file only: a rolling family is a capture-and-strategy
  contract, not something to spell on a command line.

- **A WALL-CLOCK law, and the defect it prevents.** A HIP-4 expiry is an
  epoch instant (`time:20260912-0630`); every clock in the ingress loop
  is monotonic-since-boot. Comparing them directly makes every expiry
  look 55 years away and a family matches NOTHING, silently. The driver
  now carries a `core_time::WallAnchor` taken at `set_families` (boot,
  two syscalls, never re-taken — a reconnect is not a new clock) and
  judges `match_spec` on `wall_of(now)`. The loopback roll test caught
  this; it is pinned by an injected anchor so the scenario is
  deterministic on any machine.

- **New metrics** (gauges, read from an `Arc<HlRollStatus>` on the
  status set — `IngressStatus` is size-locked at 128 B and venue-generic,
  so venue counters could not go there):
  `engine_ingress_hyperliquid_rolls_total`,
  `..._rolls_ignored_unmatched_total` (the other deployers' markets on
  the shared lifecycle channel — **expected to be large**),
  `..._family_ack_timeouts_total`, `..._families_dormant`.

- **`claude_worker.pmlr` gained a ChannelEvent reader** (`channel_events()`,
  `ChannelEventRec`) — there was none at all before; every consumer that
  needed events read ticks. New module `claude_worker.hip4`: the
  description grammar as the Python MIRROR (pinned against the engine by
  one shared fixture, `claude-worker/tests/fixtures/hip4/descriptions.tsv`,
  read by both suites), plus `read_rolls(run_dir)` / `instance_at()` —
  the sidecar every offline consumer uses to know which instance a slot
  meant. The capture catalog needed NO change: it is channel-agnostic by
  design (its own module docs say so).

- **Gate counts moved.** Alloc gate **50 → 51**
  (`hl_family_roll_is_zero_alloc`); `audit-replay`'s venue×channel
  matrix gains an `instrument_roll` column (9 → 10).

**Deviations from the plan, recorded**

1. Families attach through `Driver::set_families(families, roll_status,
   wall)` rather than as a 5th `Driver::new` parameter. Zero churn on
   the five existing call sites, and "no call ⇒ pre-BIN15 behaviour" is
   the same absent-is-bit-identical shape the VRP and regime lanes use.
2. The roll alloc gate drives **64** rolls, not 1 000: the whole
   scripted stream is injected before the guard opens
   (`inject_incoming` may copy, so it cannot be measured) and one
   `drive_one` may consume every frame at once — 64 rolls is what
   `TX_BUF_SIZE` holds in a single drain. The law under test is
   per-roll allocation, which 64 exercises exactly as 1 000 would.
3. `AiCmdKind::SetBinarySpec = 13` (the worker's spec override, ruling
   O-Q2) is **NOT built**: §4 of the spec never specifies it and §4.7's
   acceptance does not mention it. Byte 13 of `AiCmdKind` stays
   unclaimed rather than being improvised.
4. `HL_ROLLING_MAX` (8) and `HL_ROLLING_ORDINAL_BASE` (4096) are
   MIRRORED in `core_config::universe` — that crate cannot depend on an
   ingress crate. The same split as `coins`, whose real cap
   (`HL_MAX_COINS`) is enforced in the cli's table build; boot refuses
   anything the looser check let through.

**To activate.** Add `rolling = [...]` under `[hyperliquid]` in
`~/multivenue/universe.toml` and restart. Nothing else changes: without
the key every path above is the pre-BIN15 one.

## 2026-09-12 — HIP-4 outcome grammar; `outcomeMetaUpdates` read at its LIVE shape; `hl.prediction` fee class with a charge-once open pair (BIN15 O1)

**What changed**

- **`parse_outcome_meta` now reads the shape the venue actually sends.**
  This is a PARSER CORRECTION, not a format bump. The live
  `outcomeMetaUpdates` push (probed 2026-09-12) is
  `{"data":[{"outcomeCreated":{"outcome":N,…}}]}` or
  `{"data":[{"outcomeSettled":N}]}` — an array of kind-keyed objects with
  **no `coin` key and no top-level `time`**. The pre-BIN15 parser looked
  only for `"coin":"#<enc>"`, so on every live frame it returned
  `enc = OUTCOME_ENC_NONE` and `ts_ns = 0`; the in-tree fixture that
  carried `"coin":"#330"` was not the venue's shape. `enc` is now
  `10 * outcome_id` (the **Yes** side; No is `enc + 1`), read from the
  kind's own key with the generic `"outcome"` key as a second try, and
  the legacy coin scan kept as the fallback so the old fixture still
  parses to `enc = 330` byte for byte.

  **Operator-visible consequence: none in capture.** `enc` is not a
  captured field — `ChannelId::OutcomeMeta` resolves its `sym` through a
  `#<enc>` coin key, which the live shape does not have, so those rows
  keep `sym = SYMBOL_ID_NONE`. Mapping an enc to a slot sym is BIN15 O2
  (the rolling-instrument pool); `docs/wire-format.md`'s ChannelId row is
  clarified, not changed.

- **New, additive: the outcome DESCRIPTION grammar.**
  `ingress_hyperliquid::outcome_meta_description` hands back the raw
  `description` value **borrowed from the rx buffer** (zero copy), and
  `discovery::{HlOutcomeSpec, parse_outcome_spec, parse_hl_time_ns}`
  parse it. A HIP-4 market carries its whole economics in that one
  `|`-delimited `key:value` string, so the key SET is what identifies
  the law: `perp:`+`threshold:`+`seconds:`+`time:` ⇒ `OutBinaryPrice`,
  `perp:`+`target:` ⇒ `OutPriceTouch`,
  `class:priceBinary`+`underlying:`+`expiry:`+`targetPrice:` ⇒
  `NativePriceBinary`, anything else ⇒ `Unknown`. Two laws worth
  knowing: the string is TOKENISED on `|` rather than searched key by
  key (`priceDescription` is free text, and prose containing
  `threshold:` must not become the strike), and a required key that is
  PRESENT but unparseable demotes the grammar to `Unknown` instead of
  reporting a confident wrong number. `parse_hl_time_ns` is integer
  days-from-civil, `const`, and rejects impossible dates.

- **`core_config::instrument_class`: the `hyperliquid` namespace is no
  longer uniformly `Perp`.** `#<enc>` and the `out:` / `native:`
  rolling-family slot descriptors class as **`Prediction`**; everything
  else stays `Perp`. Mirrored in `claude_worker.instrument_class` and
  pinned by the one shared fixture
  (`claude-worker/tests/fixtures/fees/descriptor-classes.tsv`, now 43
  rows), which both suites read.

  **KNOWN MIS-CLASS, deliberately left: `hyperliquid:@<idx>` (spot)
  still classes as `Perp`.** No Hyperliquid spot leg has ever been
  traded here and re-classing one would move a fee class on a live
  venue for no current caller. The fixture pins today's answer so that
  changing it has to be a deliberate fixture edit — the XSD-F precedent.

- **`fees.toml` gains a CHARGE-ONCE key shape: `<class>_open`.**
  `fees.toml.example` `[fees.hl]` now carries
  `prediction = "2:5"` and `prediction_open = "0:0"`, with the source
  line and an **UNVERIFIED** mark (ruling O-Q8 — no BIN15 order has been
  filled on this venue, so both rows are carried, not measured).
  HIP-4 charges nothing to open and the whole fee on the close or the
  settlement; a single per-class pair cannot express that, because it is
  the same instrument on both legs and only the fill's direction
  relative to the position already held tells them apart.

  Plumbing: `ModelParams::fee_open_bps: [[Option<(u32,u32)>; 5]; 7]`,
  the flag grammar `--fee-bps <venue>.<class>.open:<m>:<t>` (the bare
  and `<venue>.<class>` forms are unchanged and never set an open pair),
  `claude_worker.pnl_report.load_fee_flags` mapping the key, and
  `FillEngine::fee_rate_for(venue, sym, opening)` beside `fee_rate`.
  `opening` is computed from the model's OWN position in the full book
  before the fill — never from the submit.

  **BIT-IDENTICAL for every pre-BIN15 row**, and the tests say so:
  `absent_open_pair_is_bit_identical` walks all 7 venues × 5 classes and
  both fill directions with no open pair set and requires
  `fee_rate_for == fee_rate`, and compares end-to-end fee TOTALS with
  the field absent against an open pair that merely restates its class
  pair. Two deliberate narrowings: the dearest-class FALLBACK is not
  eligible for an open pair (with no single known class there is nothing
  to read, and charging a guess as "free to open" would flatter P&L),
  and `fee_class_unknown_fills` still moves exactly as before because
  `fee_rate` runs first on every path. Schema-1 stdout and the
  `render_fee_table_{text,json}` renderers are UNTOUCHED — a test pins
  that the JSON is identical with and without the new field.

- **Gate counts moved.** `crates/bench/tests/alloc_assertions.rs` gains
  `hl_outcome_meta_parsers_are_zero_alloc` (the two live frames through
  the lifecycle parser, the zero-copy accessor and the grammar, 10 000
  iterations): **the alloc gate goes 49 → 50**. A new fuzz target
  `fuzz/fuzz_targets/hl_outcome_spec.rs` is registered in
  `fuzz/Cargo.toml` with the literal `license = "Apache-2.0"` key (that
  manifest cannot inherit — cargo-fuzz excludes it from the workspace).

**Nothing to do.** No wire layout changed, no capture file changed, no
engine restart is required by this entry, and a `fees.toml` without a
`<class>_open` key behaves exactly as it did.

## 2026-09-12 — `/state` gains a `vrp` object; `stale_skips` splits in two; `engine_strategy_vrp_active` starts telling the truth (VRP P6)

**What changed**

- **`/state` gains a `"vrp"` object.** ADDITIVE — `"v": 1` stays, no
  reader breaks, and `engine-snapshot` is still inside its 32 KB bound.
  It carries the member's counters plus what the counters never said:

  ```json
  "vrp":{"configured":1,"hash":"…","state_epoch":9,
         "regime_offset_1e9":-99000000,"last_settle_value_1e6":1250000,
         "campaign":{"expiry_ns":"…","sym":50332169,"strike_1e6":79000000000,
                     "right":0,"side":-1,"opt_qty_1e6":-100000,
                     "perp_qty_1e6":49000,"entry_done":1},
         "pending":{"opt_oid":"77","hedge_oid":"0"}, … }
  ```

  WHICH contract is held, at what strike, with what on each leg, and
  what is still in flight. The failure modes an operator has to read are
  relationships between those fields — a **naked hedge** is
  `opt_qty_1e6 == 0` beside a non-zero `perp_qty_1e6` (the F7 defect,
  live for two campaigns before P1), and a **stuck leg** is an
  `opt_oid`/`hedge_oid` that does not clear. The byte-exact pin test in
  `crates/engine-snapshot/tests/encode.rs` is extended, not regenerated:
  the head pin is untouched because the section is appended after
  `icdp`.

- **The TUI gains a `vrp:` line** (header grows from 6 rows to 7) — the
  campaign, then `dec/hold(cost)/ent/hdg/settle` and the regime offset.
  `hedge_abandoned`, `settled_unpriced` and `killed` print there ONLY
  when non-zero, so a healthy line stays readable and an unhealthy one
  is unmissable.

- **OPERATOR-VISIBLE — `engine_vrp_stale_skips_total` splits in two**
  (F30). It used to count both "the venue sent an option record with
  nothing usable in it" and "a rung wanted to act and the mark it needed
  was stale". Deribit publishes summaries for instruments with no book
  all day, so the first drowned the second, and the number an operator
  watched for lost decisions was dominated by routine noise. From this
  release:

  | counter | what it counts |
  |---|---|
  | `engine_vrp_records_ignored_total` | option records DROPPED on arrival — no mark flag, a non-positive mark/IV/underlying, or a coin price that does not convert. **Routine.** |
  | `engine_vrp_stale_skips_total` | a rebalance or a settlement skipped because the member's own cached mark was stale or absent. **An action it wanted to take and could not.** |

  **Expect `stale_skips` to drop to near zero and `records_ignored` to
  carry the old volume.** That is the split, not a fix. Any alert or
  dashboard reading `engine_vrp_stale_skips_total` should be left
  pointed at it — it now means what its name says.

  One honest note recorded with the split: the DECISION rung's own stale
  guard is unreachable as the member is wired today. `decide` runs only
  from `on_opt_summary`, which has just refreshed the cached mark from
  the very record that brought it there, so the price is positive and
  the age is zero by construction. The guard is kept (two compares on a
  once-per-campaign path, and it is what would have to hold the day
  `decide` is reached from a tick) and commented as such. The live
  `stale_skips` come from the rebalance and settle rungs, which run on
  the perp lane.

- **OPERATOR-VISIBLE — `engine_strategy_vrp_active` was always 0**
  (F29). It was set from `strategy_kind == "vrp"`, and the live engine
  runs the SET, whose kind is `"set"` — so a gauge named "is the VRP
  member active" could never read 1, and an alert on it could never
  fire. It now reads 1 when the bare strategy is `vrp` **or** the set
  has slot 1 enabled right now; a runtime `DisableStrategy(1)` drops it
  back to 0, which is the point. `engine_strategy_latency_arb_active`
  had the same defect and is fixed the same way.

**Ops**

- `scripts/candles-cycle.sh` guards the VRP seed cut on
  `[ -f ~/multivenue/vrp.toml ]` (F27). `seed-out --vrp` REQUIRES the
  artifact and exits non-zero without it, so on a host that does not run
  the member the cycle printed a failure every hour — noise that trains
  an operator to ignore the one hour it means something.
- `docs/local-setup.md` gains the F26 runbook entry for
  `engine_vrp_settled_unpriced_total`: what produces it (a deferred
  08:00Z settle followed by the 08:30Z reboot, after which Deribit has
  dropped the expired instrument from the boot chain), and how to
  reconcile that one expiry by hand from the state-file backup's `C`
  row.
- `docs/risk-policy.md` gains a **VRP member alerts** table:
  `hedge_abandoned`, `entries_unfilled` and `settled_unpriced`, each
  with what it means and what to do, plus `holds_cost` and
  `settle_index_fallback` as weekly diagnostics.

**Action required**: none at boot. After the next restart, expect
`engine_vrp_stale_skips_total` to go quiet and
`engine_vrp_records_ignored_total` to carry its old rate, and expect
`engine_strategy_vrp_active` to read 1 for the first time.

## 2026-09-12 — VRP config errors stop blaming `icdp.toml`; the live chain stops parsing instrument names (VRP P5)

**What changed**

- **OPERATOR-VISIBLE — every VRP config refusal named the wrong file.**
  `core_config::vrp::VrpError` was `pub type VrpError = IcdpError`, and
  `IcdpError`'s `Display` writes `"icdp.toml: {msg}"`. So a bad
  `vrp.toml`, `vrp-seed.tsv` or `vrp-state.tsv` refused the boot with a
  message pointing an operator at a file they had not touched. It is now
  its own newtype and its messages read `vrp: line 12: unknown key
  ...`. The message BODIES are unchanged, and `VrpError(pub String)`
  keeps the `.0` field callers and tests already use.

- **The live boot no longer parses option instrument NAMES.**
  `boot_discovery::Outcome::deribit_options` was
  `Vec<(String, SymbolId)>`; it is now `cli::paper::DiscoveredOption =
  (String, SymbolId, strike_1e9, expiration_ts_ms, right)`. Discovery
  already parsed those three numbers out of the venue's REST JSON and
  threw them away, and `vrp_boot::build_registry` then recovered them by
  taking the instrument name apart again — two laws for one fact, one of
  them a string parser. The registry now uses the venue's numbers
  through `OptInstrument::from_discovery` and falls back to the name
  parser ONLY when a field is absent, counting those rows.
  - `build_registry` returns `(registry, refused, rows_from_name)`.
  - `VrpBoot` gains `rows_from_name`; the boot tell gains
    `chain_rows_from_name` and `backtest --member vrp` gains
    `chain_from_name=`.
  - **Expect `chain_rows_from_name=0` on a live boot.** Non-zero there
    means discovery dropped fields it used to carry. Offline it equals
    the chain length by design: a capture's options manifest holds names
    and nothing else, so the harness keeps the name parser.
  - A field that is PRESENT and unrepresentable is a REFUSAL, not a
    reason to fall back — `from_discovery` refuses a row rather than
    rounding it, and quietly rebuilding that row from its name would
    defeat the check.

**De-duplication (no behaviour change; each item is byte-identical by
construction)**

- The minute roll is `core_time::BarClock` at `tf = 60 s`, `delta = 0`.
  `bar_id(mono)` IS `anchor.wall_of(mono) / MINUTE_NS`, the division the
  member spelled out; `the_bar_clock_roll_is_the_division_it_replaces`
  pins the two over a full day of instants either side of every
  boundary, because a roll that moved by one would re-key every `R` row
  in `vrp-state.tsv` against the worker's seed.
- `notional_1e6` / `size_ok` move from `VrpStrategy` to
  `strategy_core::risk`, beside the caps table they read. icdp's config
  leg check calls `size_ok` instead of comparing by hand — strictly
  stronger, since the per-SYMBOL cap is now checked too.
- `intrinsic_1e6` moves to `opt_registry`. It had two verbatim copies:
  the member's and `OptSettleRef::value_1e6`'s body. The harness reaches
  it there without depending on a strategy crate, which is why the copy
  existed.
- `backtest::opt::quote_px_usd_1e6` is now
  `coin_to_usd_1e6(px_1e6 × 1000, u, cs)` — ONE conversion in the tree.
  The wrapper carries the three orders of magnitude between a quote
  price (coin ×1e6) and a mark (coin ×1e9), checked, so a corrupt
  capture still yields `None`.
- The single-section TOML loop is `icdp::parse_single_section(src,
  section, keys)`, beside the `strip_comment` / `parse_value` primitives
  every config in the crate already shares. `vrp.rs` held the only copy;
  every message it produced is unchanged with `vrp` substituted for the
  section name.

**No action required.** Nothing here changes a decision, a price or a
fill. The one thing to watch after the next restart is
`chain_rows_from_name` in the `vrp: artifact configured` tell: it should
read 0.

## 2026-09-12 — the VRP decision moves to a regime-corrected band, a median IV and a cost-aware θ; settlement moves to the venue's delivery TWAP (VRP P4 / R3, R5, R6, R7)

**What changed**

- **`vrp.toml` gains four OPTIONAL keys** (`core_config::vrp` `VRP_KEYS`
  17 → 21), all ×1e9 additive offsets on `ln σ̂`. Absent keys are `0`,
  and a zero offset is `bounds` **bit for bit**, so an existing file
  still parses and decides exactly as it did:

  | key | §3.3 train-only value | applies when |
  |---|---|---|
  | `regime_fast_vol_low_1e9` | `20000000` | fast profile's effective word is `vol:low` |
  | `regime_fast_vol_high_1e9` | `-99000000` | fast profile's effective word is `vol:high` |
  | `regime_slow_vol_low_1e9` | `77000000` | slow profile's effective word is `vol:low` |
  | `regime_slow_vol_high_1e9` | `-165000000` | slow profile's effective word is `vol:high` |

  `vol:normal` and UNKNOWN are `0` **by construction** — there is no key
  for them, because the §3.3 fit measured everything else against
  `vol:normal` as the baseline. The two profiles' offsets ADD. A value
  past ±`VRP_THETA_MAX_1E9` is REFUSED at boot: an intercept larger than
  the widest legal θ is a units error, not a correction.

  **The live file sets none of these**, so P4 changes no decision until
  an operator adds them. The four values above are the ones to paste.

- **The §3.3 target is log realised VOL, not log variance.** Confirmed
  from the regime-edge source before a line was written:
  `rg_lib.fwd_rv` returns `sqrt(Σ r²)` and `rg_har.build` takes its
  `log`. The coefficients therefore enter `vrp.toml` **unhalved**. Had
  the fit been on log variance every number above would be half what it
  is, and nothing in the engine could have told the difference — which
  is why this sentence exists.

- **`core_vol::VolEngine` gains `bounds_with_offset` and
  `arm_hold_at_with_offset`**; `bounds`/`arm_hold_at` delegate with `0`
  and are unchanged. The QLIKE comparison scores the **offset** forecast
  (`pend_ln_sigma += off` at arm): kill criterion 3 exists to judge the
  forecaster the member actually decided on, and arming on the
  uncorrected `ln σ̂` would score one nobody is running. Mirrored in
  `claude_worker.vol_ref` (`bounds_with_offset`, `arm_hold_with_offset`)
  and pinned by the shared fixture, which gains an `O <off_1e9>` op —
  sticky, `0` for every row written before P4, so rows 0–16 of
  `parity-1.expected.tsv` are byte-identical.

- **OPERATOR-VISIBLE — settlement now prices at the venue's DELIVERY
  TWAP.** Deribit settles a daily expiry on the average of its index
  over the 30 minutes before expiry, not on the last print. Both the
  member and the harness now accumulate `Σ px·dt / Σ dt` over
  `[expiry − 30 min, expiry)`, each sample carrying the wall gap BEHIND
  it capped at 60 s, and settle on that average whenever the window
  carries at least **10 minutes** of samples. Below the floor the P0 law
  (the last index at/before expiry) still prices it, and says so:
  counter `engine_vrp_settle_index_fallback_total`, and `settle=last`
  instead of `settle=twap30` on the harness's per-contract line. A
  restart INSIDE the window loses the accumulator, which shows up as a
  fallback rather than as a silent change of law.

  **V0 (a) — the documented proxy.** Both sides average the option
  record's `underlying_px_1e9`, which is the expiry's **FORWARD**, not
  the index Deribit delivers against; the engine does not capture the
  index. Measured basis: **−2.2 bps**. Harness and member use the same
  proxy, so they agree with each other, and both are biased the same way
  against the venue.

- **The decision runs on the MEDIAN implied vol of the selection
  window**, not on whatever the venue published at E−τ. The member keeps
  the selected contract's last 64 `mark_iv_1e9` prints in a wrapping
  ring and takes the lower median of the filled prefix (stack copy,
  insertion sort, once per campaign — gate 49 pins zero allocations).
  One decision per campaign used to ride on one print, and a single wide
  quote at the decision instant was enough to open or close it. Below 8
  samples there is no median to take: the last print decides and
  `engine_vrp_iv_median_fallback_total` moves.

- **The comparison runs at a COST-AWARE band.** `θ` itself is never
  changed (edge spec §2.2); the band the comparison uses is
  `θ + ln((premium + c)/premium)` where `c` is the half-spread of the
  option's own touch plus `min(opt_fee_index_bps × index,
  opt_fee_prem_bps × premium)/1e4`. R4b's diagnosis is that the side
  call is 78 % right and still loses 8.4 bps because fees are 13–50 % of
  the premium — so a campaign the cost cannot pay for is not an edge,
  and `engine_vrp_holds_cost_total` counts exactly the HOLDs that θ
  ALONE would have traded. **This tightens live behaviour with the live
  file unchanged**: at the current fee keys a decision must clear roughly
  8 % more than θ alone asked for. That is the intended effect and the
  only P4 item that moves a live number without an operator edit.

**Ripple**

- New counters on `strategy_core::VrpCounters` (and therefore on the
  `backtest --member vrp` counters line and the `engine_vrp_*` family):
  `settle_index_fallback`, `iv_median_fallback`, `holds_cost`. New gauge
  `engine_vrp_regime_offset_1e6` = the intercept in force at the last
  decision. New boot tell `vrp: regime intercepts` prints the four
  effective values — an all-zero table is bit-identical to no table, so
  the tell is the only way to tell a loaded correction from a missing
  one.
- `strategy-set`: `push_vm_regime_view` → `push_regime_views`, now
  pushing the view to the vrp member as well as the vm (three call
  sites: boot, gate refresh, minute roll). Same cadence as before —
  minute roll, effective change, declaration; never per tick.
- Harness: `OptSettleRef` gains the accumulator and `observe()`, which
  is now the ONE place both settlement laws live; `audit-pnl` and
  `backtest` print `settle=twap30|last` with the covered seconds per
  contract. `HarnessStats` is `Copy` by design, so the per-contract
  lines travel in a new `SummaryExtras` bundle rather than in it.
- `crates/cli/src/backtest/opt.rs` restates the three delivery-window
  constants (the harness must not depend on a strategy crate);
  `the_delivery_window_matches_the_members` holds the two copies
  together.
- Gate 49 `vrp_member_p4_decision_paths_are_zero_alloc` — the regime
  push on a minute cadence, the ring, the median sort, the TWAP and the
  cost term over a whole campaign: 0 allocations, 0 B.

**No action required** unless you want the regime correction: add the
four keys above to `~/multivenue/vrp.toml`. Everything else takes effect
at the next boot.

## 2026-09-12 — `vrp.toml` gains the EXECUTION modes; the hedge now RESTS by default (VRP P3 / R1, R2)

**What changed**

- **`vrp.toml` gains seven OPTIONAL keys** (`core_config::vrp`
  `VRP_KEYS` 10 → 17). Absent keys take the defaults below, so an
  existing file still parses byte for byte:

  | key | values | default |
  |---|---|---|
  | `entry_mode` | `ioc` \| `maker` | **`ioc`** — unchanged |
  | `entry_patience_ns` | integer ns | `0` = the whole remaining decision band |
  | `entry_fallback` | `abandon` \| `cross` | **`abandon`** — unchanged |
  | `hedge_mode` | `taker` \| `maker` | **`maker`** — CHANGED, see below |
  | `hedge_patience_ns` | integer ns | `30_000_000_000` (30 s) |
  | `opt_fee_index_bps` | integer | `3` (Deribit) |
  | `opt_fee_prem_bps` | integer | `1250` (Deribit) |

  New refusals: `entry_patience_ns > selection_ns` (a maker entry cannot
  rest past the band that authorised it) and
  `hedge_patience_ns >= rebalance_ns` (a hedge still resting when the
  next rebalance is due would chase two targets).

- **OPERATOR ACTION — `hedge_mode` defaults to `maker`.** At the next
  boot the delta hedge RESTS at the passive side of the perp touch for
  30 s before crossing, instead of taking at the touch immediately.
  P3.0 measured that at **1.83 bps of spot per campaign against 5.12**
  for the taker, with a hedge error ~100× smaller than the difference
  (`docs/research/vrp/vrp-p30-execution-2026-09-12.md`). To keep the old
  behaviour, add `hedge_mode = "taker"` to `~/multivenue/vrp.toml`.
  The **fallback is unconditional**: a rest that reaches its deadline
  crosses at the then-current touch, so the hedge still always
  completes. The rest and its cross are ONE attempt against
  `HEDGE_RETRIES_MAX` — charging the handover a retry would spend the
  ladder on a mode change.

- **`band_qty_1e6`'s measured value is 150000**, not the 50000 the live
  file sets. The live file sets it EXPLICITLY, so nothing moves until an
  operator edits that line; the number is recorded so the edit is a
  decision rather than a guess.

- **The member reads the selected option's own QUOTE lane.**
  `VrpStrategy::on_tick` gains a branch for `selected_sym` that caches
  its touch. **The denomination law applies there too** — an option
  quote is COIN on the wire (VRP V2a), so it is converted through
  `opt_registry::coin_to_usd_1e6` against the last summary's underlying,
  and a quote arriving before the first summary of the campaign is
  DROPPED rather than booked as dollars.

- **The maker entry's limit**: short vol rests an ASK at
  `max(mark, bid)`, long vol a BID at `min(mark, ask)` — never worse
  than the mark the decision was taken at, and never marketable on
  arrival (the maker law needs a STRICT cross).

- **The cross fallback is a DECISION, not a retry.** It fires only if
  the signal still clears a COST-AWARE band at the touch price:
  `theta_eff = theta + ln((premium + cost)/premium)` where `cost` is the
  crossed half-spread plus the venue's capped fee. `theta` itself is
  never changed (edge spec §2.2). A refusal is a HOLD and is counted
  apart from a plain unfilled entry.

- **Four new counters / metrics**, all additive:
  `engine_vrp_entry_maker_submitted_total`,
  `engine_vrp_entry_crossed_total`,
  `engine_vrp_entry_cost_refused_total`,
  `engine_vrp_hedge_crossed_total`. The boot log gains a
  `vrp: execution modes` line naming every one of the seven keys in
  force.

**What did NOT change**

The entry path's default (`ioc` at the mark), `vrp-state.tsv` v4, the
seed grammar, the caps, the regime gate, every harness surface, and
`detail_version` 7 / `audit_pnl_version` 1. Three member tests that pin
the TAKER ladder now say `hedge_mode = HEDGE_MODE_TAKER` explicitly
rather than inheriting it — the law they pin is unchanged.

**Gates**: nextest 1876 (+8) · alloc **48/48** 0 B/op (gate 48 = the
execution modes, including the option quote lane's conversion) · lint ·
license 328.

## 2026-09-12 — `backtest` and `audit-pnl` share ONE option model; `--member vrp`; the campaign pool (VRP P2)

**What changed**

- **One option-model registration (F9, F10).** `backtest::opt` gains
  `OptTerms`, `OptLoadOut`, `OptModelRegistration`,
  `register_option_model` and `apply_settlements`, and `OptSettleRef`
  (with `value_1e6`) MOVES there from `audit_pnl.rs`. All three report
  surfaces — `backtest`, `backtest --member` and `audit-pnl` — now
  configure the mark-fill law, the capped-fee index leg, the expiry
  classifier and the European cash settlement from that one place.
  - **F9:** the D-7 mark-fill law now applies to EXACTLY the syms the
    load pass SYNTHESISED a mark tick for. It used to apply to every
    option sym carrying a mark, which since VRP V2a is every Deribit
    option — they all have a quote lane — so a zero-spread mark tick
    overwrote the option's own top of book on every summary record.
    On any real capture the synthesised set is EMPTY, so
    `opt_mark_syms` reads 0 where it used to read the whole chain.
  - **F10:** `set_opt_settle` was called from `audit_pnl.rs` alone. A
    contract held through its expiry now becomes cash at the European
    intrinsic in the backtest too, instead of marking out at whatever
    mid the dead instrument last printed (Deribit keeps quoting an
    expired instrument for 9–19 min).
  - The two byte-for-byte copies of the inline registration loop (in
    `backtest.rs` and `backtest/member.rs`) are gone.
- **Fill-engine fixes.**
  - **F12:** a SETTLED sym fills nothing in pass (b) — every open order
    on it is canceled and counted `settled_sym_orders_canceled`.
  - **F13:** an option QUOTE tick whose sym printed a Deribit summary in
    the run and has no registry row is DROPPED and counted
    (`opt_quotes_unregistered`); `registry_from_manifest_rows` returns
    its refused inserts (`opt_registry_refused`). Both printed. Before
    this the book answered `NotAnOption` and left a COIN number in a USD
    field — the units defect V2a exists to remove, by the one path V2a
    did not close.
  - **F14:** `audit-pnl` calls `finish()` BEFORE `per_sym_detail()`.
    `finish` is what runs the settlement sweep, so the rows it printed
    showed an expired contract as an open position.
  - **F15:** the capped option fee's index leg is read at the FILL
    instant from a wall-stamped `UnderlyingBook`
    (`FillEngine::attach_underlying_book`), not from whatever index the
    run last printed. Absent book ⇒ the last-index fallback, byte for
    byte as before.
  - **F17:** `Side::Bid => (mark + h).max(1)` — on a settled OTM
    contract the mark IS zero, and a resting bid booked a fill at a
    price of nothing.
- **`backtest --member vrp` (Q10; F16 closed).** `MemberKind::Vrp`
  drives `strategy-vrp` through the harness on the wall clock:
  `vrp.toml` via `--vrp`, the fitted pairs via `--vrp-seed` (default:
  the FIRST run dir's own `vrp-seed.tsv`), the option chain from the
  capture's NEWEST manifest, identity `WallAnchor`, and NO state — a
  replay always starts flat (the XSD law). `BacktestConfig.vrp_seed`
  and `AuditPnlConfig.vrp_seed` were declared at VRP V5 and read by
  nothing; the first moves to `MemberSpec.vrp_seed`, the second is
  DELETED along with `audit-pnl --vrp-seed` — that verb replays logged
  orders and never constructs the member.
- **The campaign pool (`window_root --campaign vrp`).** A VRP campaign
  is nine instants over nine hours and the ≤ 2 h capture law forbids
  replaying the hours between them, so the pool for this member is the
  NINE slices it decides in, per UTC day: the decision window
  `[entry − selection − 5 min, entry + selection + 5 min]`, the first
  5 min after every hour boundary strictly inside `(entry, expiry)`,
  and `[expiry − 10 min, expiry + 20 min]`. Pool
  `~/multivenue/worker/windows-vrp/`, pruned by COUNT
  (5 days × 9 windows). Each cut gets its own `vrp-seed.tsv` as of its
  first instant, which is where the member's 24 h warm-up comes from —
  the tape between the slices is not in the pool.
- **`claude_worker.vrp_shadow`** reconciles the engine's own book
  (`engine-orders.pmlr` + `engine-fills.pmlr`, attributed to slot 1)
  against `audit-pnl` replaying the same capture: per-sym fills, per-sym
  position, and the order count that frames them. Exit 0 only when every
  leg agrees. This is the V8 acceptance instrument and P1's live one.
- **`pmlr.FillRec` gains `strategy_id` and `origin`** — the Python
  mirror of P1's wire-additive `Fill` change. Every capture written
  before P1 reads `(STRATEGY_ID_NONE, FILL_ORIGIN_VENUE)` there,
  because those three bytes were explicit zeroed padding.

**The guards (P2's binding gates)**

Two roots, before and after. The option-free root is one VM pool window
with its `*-opt-summary.pmlr` files truncated to their headers and
`options-manifest.tsv` emptied — no option record reaches the model, so
the whole change is inert and every surface must be identical:

```sh
E=./target/release/multivenue-engine
RS=~/multivenue/artifacts/rulesets/fde6f733e72649e0c6452b009d0f7c3c.json
$E backtest --ruleset $RS --replay-dir <option-free root> --split 0/100 \
   --emit-detail <sidecar> > <stdout>          # surfaces 1 and 2
$E backtest --member xsd --replay-dir <option-free root> --split 0/100   # surface 3
$E audit-pnl --dir <option-free root>                                    # surface 4
$E backtest --ruleset $RS --replay-dir ~/multivenue/worker/windows/ \
   --split 0/100 --emit-detail <sidecar> > <stdout>   # the 8-window pool
```

| surface | sha256 |
|---|---|
| option-free schema-1 stdout | `0ad5feaf672b2f78addb4ab43b8fde5c47fe38d36b1d2ac1d820217083b2759d` |
| option-free detail sidecar (v7) | `3fca866e051381ee931d58a6701ea13dcd2f01a2e1bce58dbed25032200bf392` |
| option-free `--member xsd` schema-1 | `da67d68d20b3b63649d2bde1a7b123d5ab10377d9b76e91809dff5cde9eb1ccc` |
| option-free `audit-pnl` stdout | `65d46286eb82cc569b5eab0c9bdfb48feb7de907385ad66fdb00736cb0dbc098` |
| 8-window pool schema-1 stdout | `188d18e3b1ded76256ea0f5206c29880195b2af3686745ecbe85f85ce9105189` |

All five identical before and after. The pool's DETAIL sidecar is NOT
identical and must not be: it carries `"options":{"mark_syms":…}`, which
goes 64 → 0 on that root (`8695c421…` → `e3f6b8ef…`). That number
changing is the F9 fix, and the report now says it in one line:
`opt_mark_syms=0 quote_lane_syms=122`.

**What did NOT change**

`detail_version` stays **7** and `audit_pnl_version` stays **1**: the
new counts (`opt_mark_syms`, `quote_lane_syms`, `opt_settled`,
`quote_ticks_unregistered`, `registry_refused`) are printed on the human
report only, because the JSON surfaces are what the guards above pin.
The rendered `OPTIONS MARK-FILL LAW (D-7)` sentence is unchanged for the
same reason — it is inside the guarded sidecar; the misleading "no
options book exists in the capture" wording is corrected in the doc
comments and in `--option-spread-frac`'s `--help` instead.

**Operator action**

- `audit-pnl --vrp-seed` is GONE. Any script passing it must drop the
  flag; it never did anything.
- `backtest --member vrp` is new and additive — the frozen worker argv
  never passes `--member`.
- Reports over option-carrying roots will show `opt_mark_syms=0` where
  they used to show the chain size, and may now show `opt_settled=N`
  where an expiry falls inside the window. Both are the fix.

## 2026-09-12 — the engine MODELS its paper fills; `Fill` carries its origin and its member (VRP P1 / X1)

**What changed**

- **New crate `crates/core-fill` — the ONE modelled fill law.** IoC
  judged once at the touch, makers on a STRICT cross at their own
  limit, one shared displayed-size budget per tick in emit order, the
  I1 TTL, and `is_fill_evidence` (a stale or one-sided tick fills
  nothing). It also owns `ORDER_KIND_MAKER/IOC`, `MAX_OPEN_PER_SYM/TOTAL`
  and `ACTIVATION_NS_DEFAULT` — the MEASURED activation table, which
  `ModelParams::default()` now reads from here so a re-measurement
  cannot land in the harness and not in the engine.
  `cli::backtest::fill` pass (b) calls it; `strategy-icdp`,
  `strategy-xsd` and `strategy-vrp` import `ORDER_KIND_IOC` instead of
  each defining `= 1`.
- **`PaperMatcher` in `clob-dispatcher`, and `PaperDispatcher` uses it.**
  `OrderDispatch` gains defaulted `observe_tick` / `matcher_counters` /
  `open_paper_orders`; the engine calls `disp.observe_tick(&t, now)` in
  the tick-lane drain, immediately before `strat.on_tick`. So the fills
  the existing pump has always popped are now real in paper mode, and
  `engine-fills.pmlr` stops being header-only.
  **The matcher is engine-wide**: VM, ai-exec, icdp and xsd get fills
  too. Their `on_fill` remains a no-op — consuming them is each lane's
  own decision, and XSD doc 09 §3.3(ii) records the same F7-class
  defect waiting there.
- **`Fill` gains `strategy_id` (offset 13) and `origin` (offset 14)**,
  wire-additive into what was explicit zeroed padding; `Fill::new`
  defaults them to `STRATEGY_ID_NONE` / `FILL_ORIGIN_VENUE`, and
  `with_attribution` stamps a modelled one. No capture in existence
  carries anything there — paper mode never persisted a fill — so the
  reader-compat surface is zero, exactly as `Order.strategy_id` at
  M4.1. `docs/wire-format.md` updated.
- **`StrategySet::on_fill` routes an ATTRIBUTED fill to its slot alone**
  (`STRATEGY_ID_NONE` still fans out — that is every venue fill). A
  fill for a disabled or unbuilt slot is counted
  `engine_set_fills_unrouted_total`, never delivered.
- **VRP: positions come from FILLS (F7).** The member's doctrine clause
  1 said the opposite and was wrong. What changed in the member:
  - the entry submit sets `opt_pending` and **emits no hedge** — the
    hedge goes out from `on_fill`, once the option exists. Hedging on
    the submit is what put a naked perp on against an option the model
    never held, twice, live (`orders=2 fills=1 ioc_canceled=1`);
  - `move_hedge` sets `hedge_pending` and never moves `perp_pos`; one
    hedge order in flight at a time;
  - `on_fill` matches `Fill::order_id` against the leg in flight and
    moves the book; a fill for no leg is `fills_ignored`;
  - a `sweep_pendings` on every callback turns an unanswered order into
    a decision — the matcher signals a cancel only by ABSENCE. An
    unfilled entry is a HOLD (`entries_unfilled`) and DISARMS the
    forecast; an unfilled hedge retries at the then-current touch up to
    `HEDGE_RETRIES_MAX = 3`, then counts `hedge_abandoned` (an operator
    alert). Deadline = `submit + ORDER_TTL_NS + ACTIVATION_SLACK_NS`;
  - **settlement emits NO option order.** The position becomes cash at
    the intrinsic (`engine_vrp_last_settle_value_1e6`). A closing IoC
    would be judged against the venue's post-expiry quotes — Deribit
    keeps quoting 9–19 min (F12) — so it could fill cheaper than
    settlement or twice at two prices, while the harness settles the
    same position from the capture by itself (VX-A). The hold folds
    into the forecast AT EXPIRY; the perp unwind is a real order and
    the campaign ends when it fills;
  - pending legs are deliberately NOT persisted: every order carries a
    60 s TTL and the matcher's table is process-local, so an order in
    flight at a restart is dead by construction.
- Metrics: `engine_paper_matcher_{intake,fills,ioc_canceled,ttl_expired,rejected_open_cap,unroutable,out_overflow}_total`
  + `engine_paper_matcher_open_orders`, `engine_set_fills_unrouted_total`,
  and on the VRP family `entries_submitted`, `entries_unfilled`,
  `hedge_unfilled`, `hedge_abandoned`, `fills`, `fills_ignored`,
  `last_settle_value_1e6`. **`engine_paper_matcher_ioc_canceled_total`
  is the F7 counter** and `entries_submitted − entries` is the gap the
  member used to report as `entries` outright.

**The byte-identity guard (P1's binding gate)**

The harness's own output is UNCHANGED by all of the above. Run on the
8-window VM pool, before and after:

```sh
./target/release/multivenue-engine backtest \
  --ruleset ~/multivenue/artifacts/rulesets/fde6f733e72649e0c6452b009d0f7c3c.json \
  --replay-dir ~/multivenue/worker/windows/ --split 0/100 \
  --emit-detail <sidecar> > <stdout>
```

| surface | sha256 |
|---|---|
| schema-1 stdout, before AND after | `188d18e3b1ded76256ea0f5206c29880195b2af3686745ecbe85f85ce9105189` |
| detail sidecar (v7), before AND after | `8695c421afb111d9b2ccfdfa138d38684d341de797a75a68a43de54b0a76762f` |

`diff` empty on both. The only stderr differences are log timestamps and
the sidecar's own filename.

**Operator action**

None at the artifact level — no file format changed and `vrp.toml` is
untouched. The next restart makes the matcher live, after which
`engine_vrp_entries_total` counts FILLS and will read lower than it did:
that is the defect being measured, not a regression.

**What did NOT change**

The harness's fill law (the guard above is the proof), `vrp-state.tsv`
v4, the seed grammar, the caps, the regime gate, and every other
member's behaviour. The live dispatcher is untouched — `observe_tick`
defaults to nothing, because a live dispatcher learns about fills from
the venue and has no business inventing them from a book.

## 2026-09-12 — `vrp-state.tsv` v3 → v4; the settlement `y` is REALISED vol; one push per pair; the VRP boot is mask-gated (VRP P0 / R0)

**What changed**

- **`core-vol`: the settlement `y`.** `VolEngine` gained `pend_arm_k` /
  `pend_tau_min` (recorded in `arm_hold_at`) and
  `realised_since_arm_1e9()` — `isqrt(Σ r²)` over the `tau_min` returns
  pushed since the arm, `None` before the hold is over or once its
  oldest minute has left the ring. `VrpStrategy::realised_rv_1e9` now
  calls it. It used to call `har_1e9(τ)`, the HAR FORECAST evaluated at
  expiry, so live pairs were `(ln HAR(E−τ), ln HAR(E))` — two smoothed
  values sharing 16 of 24 hours of window — while the worker's seed
  pairs were `(ln HAR, ln RV)`. Two live consequences: the OLS slope
  drifts toward 1 as live pairs displace seed pairs, and kill criterion
  3 scored QLIKE against a `y` that was almost `σ̂` itself (the host read
  `engine_vrp_qlike_har_1e6` 362 against `qlike_iv_1e6` 125503 — a 347×
  gap, and a `har_beats_iv` that could never go false). **Every `P` and
  `Q` row the engine has formed to date is in the wrong domain and must
  be stripped from `~/multivenue/vrp-state.tsv` at the next restart.**
- `VolEngine::disarm()` (F4), called on a failed submit and whenever a
  hold ends with no realised window: an armed engine with no position
  would pair its `x` against a hold nobody held.
- **`core-vol` parity floors (F5).** `ln_sigma_hat_1e9` and `qlike_1e9`
  used `/` on a signed intermediate where the Python mirror uses `//`.
  Both are now `core_regime::math::floor_div`. This changes no value on
  the existing corpus (`b > 0`, `ln_u > 0` throughout) and changes the
  value on a downward-sloping fit and on every settlement whose realised
  vol came in UNDER the forecast. `claude-worker/tests/fixtures/vol/parity-1`
  was regenerated with `CORE_VOL_PARITY_WRITE=1` and now carries both
  branches plus a completed 8 h hold; the emitted row gained a 15th
  column, `realised`. Regenerate with
  `CORE_VOL_PARITY_WRITE=1 cargo nextest run -p core-vol --test parity`
  then `cd claude-worker && uv run pytest tests/test_vol_ref.py`.
- **`vrp-state.tsv` VERSION 3 → 4.** Two changes:
  - `R` stamps are the minutes that HAPPENED. `VolEngine` keeps
    `ret_ts_ms` beside `ret_1e9` (12 KiB) and `ret_chrono` returns
    `(min_ts_ms, r_1e9)`. `render_state` used to back-derive the stamps
    as `last − (n−1−i)·60 000`, which labels every hole contiguous — and
    there is a hole at every boot — after which `merge_returns` (state
    wins) overwrote the worker's correct minutes with shifted ones.
    `VolEngine::gaps()` counts a non-contiguous push; the boot tell
    prints `gaps=`.
  - A new `X <last_min_ts_ms> <prev_px_1e6>` row carries the close the
    first live return after the next boot is formed against
    (`VolEngine::seed_prev_px`). Without it that close only primed
    `prev_px` and the minute was lost — five times a day.
  A v4 reader accepts v1–v4 and refuses v5. An older binary refuses a v4
  file outright, which is the point of the bump.
- **One push per pair (F2).** `restore_state`'s `P` arm now VALIDATES and
  COUNTS its rows and pushes none of them — the `R` arm's law, for the
  same reason. `core_config::vrp::{parse_pairs, merge_pairs}` union the
  worker's seed with the state's rows by `expiry_ts_ms` (state wins,
  chronological, capped at `PAIR_RING`), and `cli::vrp_boot` pushes the
  union once. `VrpBoot.seed` is renamed `VrpBoot.pairs` and gains
  `pairs_from_seed` / `pairs_from_state`. Live proof of the defect, at
  every boot since 2026-09-11: `seed applied seed_pairs=90 … state
  restored pairs=90 … total_pairs=128` from a 90-pair seed — 38 expiries
  in the OLS twice. Both parsers now also refuse a repeated or reversed
  expiry.
- **The boot is mask-gated (F19).** `cli::vrp_boot::vrp_wanted(requested)`
  gates the whole load, as icdp and xsd already were, and a
  requested-but-absent artifact REFUSES the boot. Before this,
  `STRATEGY=ai` with a present-but-corrupt `vrp.toml` refused the boot —
  so the wrapper's documented rollback, "drop the mask back to `ai`",
  could not escape a corrupt VRP file while KeepAlive relaunched into
  the same refusal — and `ai+vrp` with an ABSENT `vrp.toml` booted
  silently as `ai`.
- **`--vrp-state <path>` (F22).** The state path was hard-wired to
  `~/multivenue/vrp-state.tsv`, so any `--vrp <other.toml>` smoke boot
  read AND REWROTE the standing engine's state. It now follows the
  artifact's own directory when `--vrp` is explicit, or the new flag.
- **State durability, both writers (F18/F21/F23).** New
  `crates/cli/src/state_file.rs` `write_atomic` = write, **`sync_all`**,
  rename; `vrp_boot::write_state` and `xsd_boot::write_state` are
  one-line wrappers over it (on APFS a crash between write and rename
  could leave a zero-length file, which boots as "first boot"
  silently). Both writers moved OUT of the `--metrics` gate in
  `paper.rs` — position persistence depended on an observability flag —
  and are now called from ONE `flush_member_state!` site, in the 5 s
  report block beside the capture flushes and again **unconditionally
  after `eng.stop()`**. The restart lane SIGTERMs five times a UTC day
  and the 00:10Z slot sits on the edge of the VRP decision band, so an
  entry at 00:09:57 was never written and the reboot restored
  `entry_done = 0`. A failing state write now warns once a minute per
  writer instead of every 5 s.
- **Member (F8/F20/F31, Q4).** The perp hedge is priced at the PERP's own
  touch — a BUY at the ask, a SELL at the bid — and not at the option
  record's forward, which V0 measured 2.22 bps below the perp mid (under
  touch-or-better a hedge SELL filled and a hedge BUY almost never did,
  so the modelled hedge ratcheted short); no fresh perp tick ⇒ no hedge,
  counted. A spent decision is persisted the instant it is spent
  (`bump_state()` after `entry_done = true`). The chain scan runs only
  inside the selection window, recomputed once per campaign
  (`engine_vrp_select_scans_total`) instead of on every option record
  for ~23 h 50 m a day.
- **`vrp.toml` gains OPTIONAL `sides = "both" | "short" | "long"`** (Q4;
  default `both`, absent = bit-identical — the core-regime hysteresis
  precedent). A refused arm is a HOLD counted as
  `engine_vrp_holds_side_total`, distinct from `holds`. New parser
  bounds (F24/F25): `take_pos_u64` refuses 0 (it accepted it while the
  message said otherwise — `selection_ns = 0` made every campaign
  silently `decisions_late`), `theta_1e9 ≤ 2e9`, `band_qty_1e6 ≤
  qty_1e6`, `selection_ns < tau_ns − epsilon_ns`, and the SEED's `V` row
  is version-checked (`SEED_VERSION_MAX = 2`) as the state's always was.
- Metrics: `engine_vrp_holds_side_total`, `engine_vrp_select_scans_total`
  (the family is now 18 counters + 4 gauges). Boot tells gain
  `pairs=/pairs_from_seed=/pairs_from_state=`, `gaps=`, `prev_px_1e6=`.

**Operator action**

1. Relink (`cargo build --release -p cli`) — G0.
2. Re-cut the seed: `python -m claude_worker.vrp_seed seed-out …`.
3. **STRIP `~/multivenue/vrp-state.tsv` to its `V`, `R` and `X` rows**
   between the SIGTERM and the KeepAlive relaunch (back the file up
   first). The `P` and `Q` rows are in the wrong domain (F1) and the `P`
   rows are double-counted (F2); `R` rows are unaffected. Do the restart
   outside `[23:35, 00:15]Z` and `[07:00, 08:35]Z`, with no open
   campaign (`grep '^C' vrp-state.tsv` empty).
4. Expect `vrp: seed applied … pairs_from_seed=90 pairs_from_state=0
   pairs=90`, `vrp: state restored pairs=0 qlike=0 … returns=1536`,
   `vrp: forecast WARM … gaps=0`, `composed mask=… vrp=true`.

**What did NOT change**

The forecast law itself on the existing corpus (the floor_div fix is a
no-op on every value in `parity-1` before this commit's additions), the
`R`-row grammar, `vrp-seed.tsv`'s format, the QLIKE ring, the caps, the
regime gate, and every other member. `strategy-xsd`'s own
`render_state` / `parse_state` are untouched — only the durability of
the write it goes through changed.

## 2026-09-12 — xsd artifacts authored by the worker, boot seed + monthly rotation hooks, `numpy` a base dependency (XSD-4)

**What changed**

- `claude-worker/src/claude_worker/xsd_author.py` (NEW module, not a
  verb — it touches no `state.db`, no `ai.sock`, no seq namespace):
  `python -m claude_worker.xsd_author author | seed-out | status`.
  `author` writes `~/multivenue/xsd-table.tsv` from ONE formation window
  (2160 hourly closes, `candles.db` only) with the research screen's
  arithmetic (statarb doc 07 §3.2 / the vault's `sa_screen.py`: OLS β on
  log closes, batched Engle-Granger ADF, admissibility |corr| ≥ 0.30 ·
  half-life 2–240 h · σ ≥ 25 bps · ¼ ≤ |β| ≤ 4 · median dollar volume ≥
  $5M/24 per bar, K = 3 most negative ADF per target) plus three laws the
  engine forced: every name must close ≥ $0.005 over the whole window
  (×1e6 `Price` quantisation — the parity finding), a target with fewer
  than K admissible partners is OUT, and a name missing ≤ 3 bars of the
  window is gap-filled (forward, leading bars backward; a REST-lane
  artefact, not a market gap) while more missing bars drop it. The header
  carries the window and every knob but NO timestamp, so the same window
  renders the same bytes; **the file's sha256 is the table identity** the
  engine restores positions under. `author` NEVER writes an empty table
  (exit 3, the old file stays — the engine would refuse the boot on one).
  `seed-out` writes `~/multivenue/xsd-seed.tsv` (800 trailing complete
  hours of the table's descriptors, `descriptor \t open_ms \t close_1e6`,
  never the in-progress hour). `status` prints age / rows / hash /
  `rotation_due=yes|no` (30 days). `~/multivenue/xsd-universe.tsv` is the
  screen's input (one descriptor per line; the research's 110 names,
  exported once by the vault one-shot). Tests `tests/test_xsd_author.py`.
- `scripts/engine-wrapper.sh`: after the regime seed, before the recommit,
  `xsd_author seed-out` when `xsd-table.tsv` exists — best-effort; a failed
  export boots with the member warming live (`seed_rows=0`).
- `scripts/daily-restart.sh`: at the **0010 slot only, BEFORE the drain**,
  when `xsd.toml` + `xsd-universe.tsv` exist and no worker verb is live:
  `status`, and on `rotation_due=yes` / `table absent` re-run `author`.
  The boot that follows reads the new table, seeds its descriptors, and
  the engine flattens positions held under the old hash (`xsd: state
  discarded (table hash changed)`) — the research's fold end. Operator
  ruling 2026-09-12: automatic, monthly, at the 00:10Z restart.
- `claude-worker/pyproject.toml`: `numpy>=2.5` moves into the BASE
  dependencies (it was `kronos`-group only). `uv.lock` re-resolved (2 lines).
  **Host note:** a plain `uv sync` UNINSTALLS the non-default groups — on
  the forecaster host re-run `uv sync --group kronos` afterwards (done on
  the Mac 2026-09-12; torch/pandas/einops restored).
- `~/multivenue/xsd.toml` installed from `xsd.toml.example` (the ruled
  operating point: $1,000 per grid unit, grid 1, 82 positions, $100k gross).
  `~/multivenue/strategy.conf` = `ai+vrp+xsd` (mask 54) — live at the
  XSD-5 restart.

**Why**

Doc 08 phase XSD-4 + the operator's 2026-09-12 rulings: the member is
inert without a table; the table must come from the same arithmetic the
research was proven on; a 720 h z window without a seed is 30 days blind
after every restart (three a day); rotation is the research's 30-day
trading window.

**Migration**

- Worker hosts: `cd claude-worker && uv sync` (numpy). Kronos hosts:
  `uv sync --group kronos`.
- First table: `~/multivenue/research/.venv/bin/python
  docs/research/statarb/bin/xsd_universe_export.py` (vault) →
  `uv run python -m claude_worker.xsd_author author` → `seed-out` →
  `cp xsd.toml.example ~/multivenue/xsd.toml` → restart. The 2026-09-12
  live table: universe 110 → alive 52 / targets 46 / rows 138 (price law
  18, liquidity 40, fewer-than-K 6, gap-filled 8, holes 0), hash
  `25ff43ef…`; seed 48 descriptors × 800 h = 38,400 rows.
- Dropping the member: `STRATEGY=ai+vrp` in `strategy.conf` + restart
  (the artifacts may stay; the wrapper's seed-out then costs a second).
- No wire, ring or schema change; every capture / backtest / worker
  frozen surface is byte-identical. Engine untouched (no relink needed
  beyond the XSD-3 binary already linked).

## 2026-09-12 — slot 2 WIRED: `strategy-xsd` in the set, four operator artifacts, `--member xsd` (XSD-3)

**What changed**

- `strategy-set`: `BIT_XSD` (4) is back in `BUILT_MASK` (`all` = 127 again);
  `mask_for_name` accepts `xsd` (4), `ai+xsd` (52), `ai+vrp+xsd` (54);
  `StrategySet` owns an `XsdStrategy` at slot 2 with the full fan-out,
  `xsd_mut()` / `xsd()`, `slot_counters`, label + gate routing;
  `StrategyCounters::{xsd_counters, xsd_state_epoch, xsd_positions_view}`
  forward to the member. `ai` 48 / `ai+vrp` 50 / `ai+icdp` 112 unchanged.
- cli: `crates/cli/src/xsd_boot.rs` resolves the member's artifacts (all
  under `~/multivenue/`, none in git): `xsd.toml` (integer params —
  ABSENT default ⇒ not configured, bit unset; `xsd.toml.example`),
  `xsd-table.tsv` (`target \t partner \t beta_1e9 [\t rank \t t_adf_1e6]`;
  unresolvable rows dropped + counted, zero usable ⇒ refuse; sha256 of the
  bytes = the table identity), `xsd-seed.tsv` (`descriptor \t open_ms \t
  close_1e6`; rows at/after the boot hour dropped; absent legal),
  `xsd-state.tsv` (engine-written: `V 1` · `H <table_hash>` · `P descriptor
  side d grid_units qty_1e6 notional_1e6 entry_hour last_add_hour`; restored
  under the same hash, FLATTENED under a changed one at each target's first
  fresh tick; a descriptor the universe lacks REFUSES the boot — move the
  file aside). Flags `--xsd` / `--xsd-table` / `--xsd-seed` / `--xsd-state`
  on `run`; the set builder configures + seeds + restores and prints
  `xsd: artifact configured hash=… table_hash=… targets= pairs= syms=
  rows_dropped= seed_rows= seed_dropped=` and one of `xsd: no state` /
  `xsd: state restored positions=N` / `xsd: state discarded (table hash
  changed) positions_to_flatten=N`; the 5 s block rewrites the state file
  when the member's epoch moves (atomic rename, failures logged).
- Metrics: `engine_xsd_{rolls,decisions,entries_decided,adds_decided,entries,
  adds,exits_revert,exits_stop,exits_maxhold,exits_rotation,exits_regime,
  intents_carried,entries_cancelled,caps_rejected,holds_absent,
  regime_blocked,seed_rows,seed_dropped}_total` + gauges
  `engine_xsd_pairs_warm`, `engine_xsd_positions`. `/state` shows the slot
  through the generic per-slot row (a dedicated xsd block is deferred with
  the TUI row — the `"v": 1` contract is untouched).
- Regime: `[labels.xsd]` accepted (`core-config::regime` MEMBER_NAMES,
  `regime_boot`); slot 2 joins RG8's `require = 1` list.
- Harness: `backtest --member xsd [--xsd <toml>] [--xsd-table <tsv>]
  [--xsd-seed <tsv>]` on the Tier-3 arm — descriptors resolve against the
  capture's newest manifest, the seed keeps hours before the replay's first
  wall hour, no state file (a replay starts flat). The frozen worker argv is
  untouched.
- `scripts/engine-wrapper.sh` allow-list gains `ai+xsd`, `ai+vrp+xsd`, `xsd`.
  `~/multivenue/strategy.conf` is NOT changed by this entry (`ai+vrp` stays
  live until XSD-5).

**Why**

Ruling R3 (slot 2) + doc 08 phase XSD-3: the member proven bit-identical to
the research (XSD-2) gets its boot path, so XSD-4's worker artifacts and
XSD-5's paper run have something to feed.

**Ripple effects**

- A boot with the xsd bit requested and NO `xsd.toml` / table at the default
  paths behaves like a boot without the bit (the vrp law) — `--strategy
  ai+vrp+xsd` on today's host boots `ai+vrp` and logs `xsd: artifact absent`.
- `EnableStrategy` for slot 2 now enables an (unconfigured, inert) member
  instead of a counted refusal — the pre-XSD-S behaviour.
- A `regime.toml` with `[labels] require = 1` refuses an `xsd`-carrying
  mask until `[labels.xsd]` is written (R8: xsd boots ANY today).

**Migration steps**

1. `cargo build --release -p cli` (the harness and the boot path).
2. Nothing else until XSD-4 writes the artifacts.

**Rollback**

- Revert the commit; no on-disk format moved. A stray `xsd-state.tsv` is
  ignored by a binary without the flag.

## 2026-09-12 — strategy-set slot 2: `cross-arb` OUT, held for `xsd` (XSD-S)

**What changed**

- `strategy-cross-arb` is UNLINKED from the composed set (the `strategy-ev`
  precedent of 2026-09-10): the crate stays in the workspace (its own tests and
  the bench alloc gate keep it honest) but `strategy-set` and `cli` no longer
  depend on it. Slot 2 is VACANT — `SLOT_XSD = 2` / `BIT_XSD = 4` are defined
  and the number is wire-stable, but the bit is OUTSIDE `BUILT_MASK` until the
  member lands (XSD-3): `StrategySet::new` clears it, `EnableStrategy` for slot
  2 is refused and counted (`engine_ai_enable_refused_total`) exactly like the
  reserved slot 7.
- `--strategy cross-arb` is a BOOT REFUSAL ("unknown --strategy value"): the
  standalone arm, `engine_loop_cross_arb_full`, `configure_cross_arb`, the
  `--groups <SPEC>` flag and the `cross_groups` parameter of
  `engine_loop_set_full` are gone; `config.example.toml` loses
  `[strategy.cross_arb]`. `xsd` / `ai+xsd` / `ai+vrp+xsd` are NOT accepted yet —
  a name with no member behind it would boot an inert set and read as "running".
- Labels: `[labels.cross_arb]` in `regime.toml` is refused at the grammar
  ("unknown coded member" — the `[labels.ev]` law); `[labels.xsd]` is refused
  too until XSD-3. The RG8 `require = 1` slot list drops slot 2 for now.
- Display: `audit_pnl::strategy_label(2)` = `"xsd"`, `engine-snapshot`
  `SLOT_NAMES[2]` = `"xsd"` (`/state` `"v": 1` is unchanged in shape — one
  string differs), the worker dashboard's slot array (`ev` → `vrp`, `cross-arb`
  → `xsd` — the slot-1 word had been missed at VRP V7), `docs/wire-format.md`
  offset-41 legend. The `engine_strategy_cross_arb_active` gauge is REMOVED
  (it could only ever flip for the standalone arm).
- Masks: `ai` 48, `ai+vrp` 50, `ai+icdp` 112 are bit-identical; `all` is
  **123** (was 127 with the slot filled) — the vacated bit is simply absent.

**Why**

Operator ruling R3 (2026-09-12): the xsd member takes slot 2 rather than slot
7 (Kronos's) or a `u16` widening — cross-arb never ran under any live mask
since the 2026-09-02 "AI lanes only" ruling and its `--groups` config was
never passed by the wrapper. The swap lands in two commits so the vacated
state is its own gate: every existing mask bit-identical, then the member.

**Ripple effects**

- A capture taken BEFORE this boundary carries cross-arb rows under
  `Order.strategy_id` 2 and an `audit-pnl` on it labels them `xsd`; there is
  no way to tell from the row itself — the boundary date is the record. Live
  captures carried no slot-2 rows since 2026-09-02 (cross-arb was never in the
  booted mask), so the practical blast radius is the reader's label only.
- Between XSD-S and XSD-3 an `EnableStrategy` for slot 2 is a counted refusal
  (previously a silent enable of an unconfigured, inert member).
- A `regime.toml` carrying `[labels.cross_arb]` refuses the boot — rename or
  drop the section (the live file carries none).

**Migration steps**

1. `cargo build --release -p cli`; the live `~/multivenue/strategy.conf`
   (`STRATEGY=ai+vrp`) needs no change — its mask is unchanged.
2. Nothing else: no operator file names `cross_arb` / `--groups`.

**Rollback**

- Revert the commit; no on-disk format moved.

## 2026-09-12 — harness fees per venue × INSTRUMENT CLASS; `fees.toml` v2; detail sidecar v7 (XSD-F)

**What changed**

- `ModelParams::fee_bps` is `[[(maker, taker); INSTRUMENT_CLASSES]; 7]` —
  indexed by `VenueId` AND `core_types::InstrumentClass` (`spot` 0 · `perp` 1 ·
  `dated` 2 · `option` 3 · `prediction` 4; new module
  `core_types::instrument_class`). It was one pair per venue.
- `--fee-bps <venue>[.<class>]:<maker>:<taker>` on `backtest` and `audit-pnl`:
  a bare `<venue>:` sets all five classes (the old flag, bit for bit);
  `<venue>.<class>:` sets one. Later flags win, so `bn:10:10 bn.perp:2:5` is
  "spot tier on Binance except perps".
- The class of a sym is its manifest descriptor's, through the DESCRIPTOR LAW
  `core_config::instrument_class::class_of_descriptor` (mirrored in
  `claude_worker.instrument_class`; both pinned by the shared fixture
  `claude-worker/tests/fixtures/fees/descriptor-classes.tsv`). A sym whose
  class is unknown (no manifest row, or a shape the law does not know — the
  `run-<epoch>/sym-<hex>` namespace of a manifest-less run) is charged the
  venue's DEAREST class and counted: `fee_class_unknown=` on the fills summary
  line (backtest) and the `ioc_fills=` line (audit-pnl), only when non-zero.
- `fees.toml` v2: the `[fees]` bare lines stay; an optional `[fees.<venue>]`
  table names classes (`"m:t"`), and `option_cap = "<index_bps>:<prem_bps>"`
  emits `--opt-fee <venue>:<index_bps>:<prem_bps>` (the venue's capped option
  law; a distinct key so a section-blind `key = "m:t"` reader never trips). `claude_worker.pnl_report.load_fee_flags` emits bare lines first,
  class lines after. `fees.toml.example` rewritten with every class per venue
  derived from the published schedules (sources + dates in the header);
  **D2-AMEND law L1 ("one slot per venue ⇒ the dearer class") is RETIRED**;
  L2 and L3 stand.
- Reports: the `--emit-detail` sidecar is `detail_version` **7** — the legacy
  per-venue `model.fee_bps` pair now prints the venue's DEAREST class (what the
  one-slot field meant under L1) and an additive `model.fee_classes` object
  carries the table; `fills.fee_class_unknown` added. The audit-pnl stdout JSON
  gains additive `fee_classes` + `fee_class_unknown_fills` (`audit_pnl_version`
  stays 1). The stderr `model:` line prints `fee_bps <venue>=m:t` when a
  venue's classes agree and `<venue>=spot:m:t,perp:m:t,…` when they differ.

**Why**

Statarb doc 08 §0.2 C7 / §4: the research charged the correct USDⓈ-M perp
taker (5 bps) while the harness charged every Binance perp leg the spot tier
(10 bps) because the table had one slot per venue and L1 put the dearer class
in it — every gate report on a perp member was understated by 5 bps on gross.
The `xsd` member (110 usdm perps as targets) cannot be gated under that model;
the operator ruled (R4, 2026-09-12) per-class fees with every class derived now.

**Ripple effects**

- A run under the LEGACY flags (`--fee-bps bn:10:10`, …) is bit-identical: a
  bare spec fills all five classes, so `fee_rate` returns the same pair for every
  sym, and the model summary token `bn=10:10` is unchanged.
- Under the v2 file, Binance/OKX/Bybit/Deribit PERP legs are charged 2:5 / 2:5
  / 2:6 / 2:4 instead of 10:10 / 8:10 / 2:6 / 2:5 — every perp member's tier
  number moves UP (less fee) by the difference; spot legs are unchanged.
- The `--opt-fee` flags now emitted from `fees.toml` for bn/okx/bybit activate
  those venues' capped option law in `ModelParams`; the fill model applies it
  only to syms whose underlying index the replay observed (Deribit today), so
  nothing changes for them until an options member on those venues exists.
- Manifest-less (pre-D3) roots: every sym is unclassed ⇒ charged the dearest
  class = the L1 number ⇒ identical to before; the `fee_class_unknown` counter
  says so.
- `instrument-manifest.tsv` is UNCHANGED (two columns; every reader is strict
  about that — a third column was considered and rejected for exactly that
  reason; the descriptor law needs nothing the manifest does not carry).

**Migration steps**

1. `cargo build --release -p cli` (the harness), rerun the worker pytest.
2. Copy `fees.toml.example` over `~/multivenue/fees.toml` (keep the old file as
   `fees.toml.bak-<ts>`); the nightly `pnl_report` picks it up at the next
   0020Z slot. Any script that reads `fees.toml` with a bare-line-only parser
   keeps working (the bare lines are unchanged).
3. Re-read every perp member's tier numbers from the first v2 day report — they
   are the first honest ones.

**Rollback**

- Restore `fees.toml.bak-<ts>` (the bare-line file): the new harness charges
  exactly the old numbers. Reverting the commit is not required for a fee
  rollback.

## 2026-09-11 — the VRP campaign exits at SETTLEMENT, not at E−ε (Y1)

**What changed**

- The E−ε unwind is gone. `maybe_exit` is replaced by `maybe_freeze_hedge`,
  which only stops NEW hedging inside ε; it places no order and does not end
  the campaign.
- `maybe_settle` at `E` is now the only terminal rung for a planned
  campaign: it books the option's intrinsic (ITM) or zeroes it (OTM),
  flattens the perp, folds the hold into the forecast and closes the
  campaign.
- `flatten()` survives for the **regime hard exit only** — the risk path,
  where crossing the spread to get out is the correct price.
- `flatten_pending` → `hedge_frozen`.

**Why**

The E−ε unwind bought the option back five minutes before expiry, crossing
the option spread a SECOND time. Lane plan §3.4 ruled that acceptable —
*"economically near-identical to cash settlement"* — and at the 0 % spread
it assumed, it was: −0.73 bps. V2a measured the real book the next day
(near-ATM 4–12 h calls, **median 25 % crossed**) and that gap became the
whole edge. Re-measured 2026-09-11 at the ATM rung, per expiry:

```
  settle    +3.658 bps  (t +1.67)
  fair_eps  −2.198 bps  (t −0.79)   <- what this rung did
```

Holding to expiry crosses the spread once. The member was built to a ruling
its own evidence had superseded, and it shipped the negative arm.

**Ripple effects**

- **`engine_vrp_exits_total` changes meaning.** It was "the planned E−ε
  unwind"; it is now "the regime RISK exit" and should read 0 in normal
  operation. A non-zero value is a regime slam, not a campaign ending.
  Campaign endings are `settlements` / `settled_itm` / `settled_otm`.
- The member now carries its option position THROUGH expiry, so the
  settlement rungs are exercised on every campaign rather than only when
  the unwind failed. `settled_unpriced` becoming non-zero is now a real
  operational signal (the contract rolled off the chain before settling).
- No on-disk format changes; `VRP_STATE_VERSION` stays 3.

**Migration steps**

1. Rebuild and restart. Re-baseline any alert on `engine_vrp_exits_total`.

**Rollback**

- Revert the commit. No format or config change is involved.

## 2026-09-11 — `vrp-state.tsv` v2 → v3: a decision is spent once (W6–W7)

**What changed**

- `VRP_STATE_VERSION` 2 → **3**. The `C` row gained an eighth field,
  `entry_done`. A v2 row (seven fields) still loads, with the flag derived
  from `opt_qty != 0` exactly as v2 meant it.
- `decide` now refuses a LATE decision: the band is `[E−τ, E−τ+selection]`,
  mirroring the selection band that ends at `E−τ`. A campaign reached past
  the deadline is SPENT — marked decided, counted, never retried.
- New counter and metric: `engine_vrp_decisions_late_total`.
- **W7:** `scripts/candles-cycle.sh` now cuts `vrp-seed.tsv` hourly, and
  `vrp_seed seed-out` gained `--vrp <path>` so the descriptor and tenor come
  from `vrp.toml` rather than a second copy in the cron.

**Why — this one traded**

`entry_done` was derived from the position, so a campaign that decided and
HELD left nothing to derive it from and the next boot decided it again. At
06:01Z on 2026-09-11 a restart re-reached the `BTC-11SEP26` campaign and
entered: short one $76,500 call, hedged long 0.975 BTC of perp, **1 h 56 m
before an 08:00Z expiry, gated and sized by an 8 h variance forecast.**

`τ` is the horizon, not a start line. `bounds` forecasts variance over a
τ-long hold and the edge was measured on one, so an entry taken hours late
is a different trade wearing the same gate. Both halves are needed: the
persisted flag stops a restart re-deciding, the deadline stops a boot that
first reaches the campaign late from entering at all.

This was called out as "minor, not harmful" when the re-decide was first
noticed on 2026-09-10. That was wrong, and it put on a live position.

**Ripple effects**

- A v2 binary refuses a v3 state file — the point of the bump. A v3 binary
  reads v1, v2 and v3.
- `engine_vrp_decisions_late_total > 0` means a restart straddled a decision
  instant and that day's campaign was skipped. Expected to stay 0 now that
  the day-boundary restart moved to 00:10.
- The hourly seed cut bounds seed staleness to an hour. It matters only when
  `vrp-state.tsv` is lost — with the state file intact the window carries
  itself across restarts — but in that case a stale seed boots the member
  WARM on old returns, and nothing refuses it.

**Migration steps**

1. Rebuild and install; restart. The next state write is v3.
2. No action for the seed — the hourly cycle now keeps it current.

**Rollback**

- Revert and delete `vrp-state.tsv` (a v2 binary refuses a v3 file). The
  member then boots cold and re-seeds from `vrp-seed.tsv`.

## 2026-09-11 — `vrp-state.tsv` v1 → v2 and `vrp-seed.tsv` v1 → v2 (W1–W5)

**What changed**

- `VRP_STATE_VERSION` 1 → **2**. New `R <min_ts_ms> <r_1e9>` rows carry the
  HAR's rolling minute window. A v2 reader accepts v1 (no `R` rows = a cold
  window, which is what v1 always meant) and REFUSES anything above 2.
- `vrp-seed.tsv` is now TAGGED and versioned: `V 2`, then `P <expiry_ts_ms>
  <x_1e9> <y_1e9>`, then `R <min_ts_ms> <r_1e9>`. A v1 seed — bare triples —
  still parses.
- `core-vol` gains `seed_return`, `on_minute_close_at`, `ret_chrono`,
  `n_returns`, `last_min_ts_ms`, `is_warm` and the public
  `HAR_WARM_MINUTES`.
- `cli::vrp_boot` reconciles the two window sources and hands the member one
  contiguous series; `VrpBoot` gains `window`, `window_from_seed`,
  `window_from_state`.
- **The member's state epoch now moves on every minute close.** It has to:
  otherwise the `R` rows only reach disk when a campaign happens to move the
  epoch — a few times a day — and the whole fix is inert. One ~40 KB
  tmp+rename per minute, on the observability cadence.

**Why — the member could never have traded**

`core_vol::har_1e9` returns `None` while `minutes < 1440`, `minutes` counted
from process start, and nothing persisted it. The restart lane fires five
times a UTC day (00:10 / 08:30 / 16:05 / 20:15 / 21:15) with a longest gap
of 7 h 35 m = **455 minutes**. 1440 was unreachable.

The first live campaign proved it. At 00:00:00Z on 2026-09-11 the member
selected the right instrument — `vrp-state.tsv` carried `C … 76500000000 0`,
the $76,500 call expiring 08:00Z — reached the decision with a fresh mark and
a fresh IV, and produced `decisions=1 entries=0 holds=0 **no_bounds=1**`.
Everything worked except the one thing that had never been able to work.

The seed gave the member its fitted LINE (90 pairs) but never its current
**x**, because `x = ln(har_now)` and `har_now` needs the 24 h window.

Same defect class as V8a, where kill criterion 3 could never arm for the same
reason. V8a fixed the pairs and the QLIKE ring and did not fix this.

**Ripple effects**

- **A v1 binary refuses a v2 seed** (`parse_seed_row` wants exactly three
  fields). That is the point of the bump, and it dictates the deploy order:
  **binary first, then re-cut the seed.** Doing it the other way round takes
  the engine down on its next restart.
- A v1 `vrp-state.tsv` upgrades silently on the first write.
- `docs/research/vrp/vrp-warmup-plan.md` (vault) carries the full design and the one known
  limitation left in place (a multi-minute gap in the perp tape publishes one
  close, not several, so `minutes` counts observed rolls rather than wall
  minutes — predates this change and alters the measured edge's input if
  touched).

**Migration steps**

1. Rebuild and install the binary.
2. Re-cut the seed: `python -m claude_worker.vrp_seed seed-out --db
   ~/multivenue/worker/candles.db --descriptor deribit:BTC-PERPETUAL --out
   ~/multivenue/vrp-seed.tsv`. It now reports window minutes and holes, and
   warns when the window is under 1440.
3. Restart. The boot tell is `vrp: forecast WARM minutes=… from_seed=…
   from_state=…`, or `vrp: forecast COLD … short_by=…`.

**Rollback**

- Revert the commit AND re-cut the seed with the old cutter — a v2 seed left
  in place will refuse a v1 binary's boot. `vrp-state.tsv` can be deleted
  instead; the member then boots cold, which is where it was before.

## 2026-09-10 — audit-pnl SETTLES expired options instead of marking them out (VX-A)

**What changed**

- `backtest::fill::FillEngine` gains `set_opt_settle(sym, value_1e6)` and an
  expiry settlement rung. When the replay clock reaches a settleable
  contract's expiry the mark is PINNED to its European cash value and stops
  moving; whatever position is still open at the end of the replay is closed
  at that value, charged the venue's SETTLEMENT rate (`fee_for` already
  switched on the instant — that part shipped with VX).
- The D-7 assumed half-spread is not charged on a settled sym. A cash
  settlement crosses no book.
- `audit-pnl` computes the value from `options-manifest.tsv` (strike, right)
  and the last underlying the instrument printed **at or before its own
  expiry**, wall-stamped through the same `run.epoch_ns + (raw_ts −
  ts_first)` rebase the merge uses.
- `ModelOutcome` and the audit's per-strategy JSON gain `opt_settled`.

**Why**

`audit-pnl` replays intents and never closed a position on its own, so a
contract that reached expiry still held stayed OPEN and marked out at the
last mid the tape carried. Deribit removes an expired instrument from the
chain, so that mid is a live option's price, not a dead one's.

For the VRP member's ordinary outcome — a short call that expires
worthless — that booked the whole premium as a loss it never took. The
member zeroes such a position in its own book with NO order, deliberately
(`maybe_settle`'s OTM branch: a zero-priced order is a fiction and a
mark-priced one would book value that expired), so nothing in the intent
log could ever have told the harness. `settled_otm` and
`settled_unpriced` were the only tells, and they are counters on the
engine, not inputs to the report.

**Ripple effects**

- Any window containing an option expiry with a position open at that
  instant now reports a DIFFERENT, correct net. Windows with no such expiry
  are byte-identical: the rung is inert without a settlement value, and no
  value is supplied for a contract whose expiry the window never reaches.
- The per-strategy JSON gains a key. Readers that match on an exact key set
  need updating; `opt_settled > 0` obliges the reader to look, because it
  means the intents did not close a contract that expired.
- The settlement table is PRINTED with the index it used and how many
  seconds before expiry that index was — the same obligation the D-7 mark
  law carries. Measured on the 2026-09-10 00:01→08:31Z run: 32 contracts
  reached expiry in-window, index lag **20–21 s**.

**Migration steps**

1. Rebuild. A report regenerated over a window containing an expiry will
   differ from one produced before this commit; that is the fix.

**Rollback**

- Revert the commit. No on-disk format, config key or wire format is
  involved — `engine-orders.pmlr`, `options-manifest.tsv` and
  `vrp-state.tsv` are untouched — and the engine loop never loads this
  code, so a running engine is unaffected either way.

## 2026-09-10 — `engine_vrp_no_selection_total` counts expiries, not records

**What changed**

- `strategy-vrp`'s selection law now evaluates the TIME window before the
  currency and right filters, and increments `no_selection` only when an
  expiry was inside the window and nothing in the chain was tradeable for
  this member.
- Previously every option record seen while no expiry was due incremented
  the counter.

**Why — the metric contradicted its own definition**

The counter is documented as "expiries where no instrument passed the
selection law". For ~23 h 50 m of every day no expiry is inside the
selection window at all, which is the member working, not the member
failing. On the live engine it reached **1537 in the first 20 s after
boot** — a rate set by the Deribit summary feed, not by anything about
the chain. An operator watching that has to learn to ignore it, and
ignoring it is the same as not having it: the one case the counter
exists to surface — the chain rolled without our currency, or carries no
calls at that expiry — would have been invisible inside the noise.

**Ripple effects**

- `engine_vrp_no_selection_total` is a COUNTER whose meaning changed
  between builds. Anything comparing across the 2026-09-10 boundary sees
  it drop to 0 and stay there; that is the fix, not a stall. Treat
  pre-fix samples as a different series.
- Nothing else reads it — no gate, no kill criterion, no state file.
- Selection behaviour is byte-identical: the same instrument is chosen by
  the same tie-breaks. Only the ORDER of the filters moved, and the time
  filter is a total predicate on `expiry_ns`/`lead`, so no row's verdict
  changes.

**Migration steps**

1. Rebuild and restart the engine (`pkill -TERM -f "multivenue-engine run"`;
   launchd `KeepAlive` relaunches it).
2. Re-baseline any alert on `engine_vrp_no_selection_total`. The expected
   steady-state value is `0`.

**Rollback**

- Revert the commit; the counter returns to per-record counting. No
  on-disk format, config key or wire format is involved, and
  `vrp-state.tsv` is untouched (`VRP_STATE_VERSION` stays `1`).

## 2026-09-10 — the VRP chain is restricted to ONE currency (V8b)

**What changed**
- `cli::vrp_boot::build_registry` now takes the hedge DESCRIPTOR and
  admits only options of that instrument's currency; every other row is
  skipped and counted in the existing `chain_rows_refused` boot tell.
- `strategy-vrp`'s selection law re-checks it: a row whose
  `underlying_sym` is not the member's own hedge leg is never a
  candidate.

**Why — this was a live defect, not a hardening**
`~/multivenue/universe.toml` has
`[deribit] options_underlyings = ["BTC", "ETH"]`, so boot discovery
hands the engine BOTH ladders. The selection law's first tie-break is
nearest expiry, and its second is nearest strike to the underlying — so
an ETH call expiring sooner than the BTC one would have been selected
and then delta-hedged with `deribit:BTC-PERPETUAL`. That is a
cross-asset naked position, and **nothing else in the member would have
caught it**: every other check is about size, staleness or timing, none
about what the instrument is. Deribit's BTC and ETH dailies happen to
share an 08:00 UTC expiry today, which would have masked it — the law
must not depend on that coincidence.

It also matters mechanically: `OptRegistry` holds 128 rows, and a
two-currency chain can exceed that, silently dropping instruments.

**Ripple effects**
- The evidence covers BTC only (edge spec §7: no ETH), so this is also
  where that restriction is enforced rather than assumed. Trading the
  ETH chain means pointing `vrp.toml`'s two descriptors at ETH — one
  edit, and the registry follows.
- A `vrp.toml` whose hedge descriptor has no option chain now fails the
  boot with the currency named, instead of building an empty table.
- Boot tell `chain_rows_refused` now includes the other currency's rows;
  a BTC+ETH ladder reports roughly half the chain refused, which is
  correct and expected.

**Operator action**
None.

## 2026-09-10 — `vrp-state.tsv`: the VRP member's state survives a restart (V8a)

**What changed**
- New engine-written file `~/multivenue/vrp-state.tsv`, read at boot and
  rewritten whenever the member's state epoch moves. It carries three
  things that outlive a process: the fitted `(x, y)` pairs with the
  expiry each came from, the **QLIKE window** kill criterion 3 is
  measured over, and any **open campaign**.
- `core-vol` gains `seed_pair_at`, `seed_qlike`, `pair_at`, `qlike_at`,
  `n_qlike` and `arm_hold_at`; pairs now carry an expiry stamp. The
  parity fixture and `vol_ref.py` are untouched — `seed_pair` and
  `arm_hold` still exist and stamp `0`.
- `strategy-vrp` gains `render_state` / `restore_state` / `state_epoch`,
  and holds the selected option's strike and right itself.
- `strategy-core` gains `StrategyCounters::{vrp_state_epoch,
  render_vrp_state}` (defaulted), forwarded by `strategy-set`. The cli
  writes the file on the same 5 s cadence as the metrics mirror, and
  ONLY when the epoch moved.

**Why**
Two gaps, both real:
- **Kill criterion 3 could never arm.** It is measured over sixty
  settled expiries — twenty days at an 8 h campaign — and the window
  started empty at every boot, against five scheduled restarts a day.
  It now accumulates across them, and an armed halt is no longer
  cleared by a restart (which `docs/risk-policy.md` forbids as
  auto-resume).
- **A restart inside a hold orphaned the position.** The next boot knew
  nothing about it: never hedged it again, never settled it.

**Ripple effects**
- **The campaign is keyed by `(expiry_ns, strike, right)`, never by
  `SymbolId`.** Deribit option ordinals reshuffle at every boot, so a
  persisted symbol would name a different instrument tomorrow.
- **Two files, one writer each.** `vrp-seed.tsv` is the WORKER's
  bootstrap cut and the engine only reads it. `vrp-state.tsv` is the
  ENGINE's own history. When both exist the state file wins.
- A state file that is present and malformed **refuses the boot** — a
  state file the engine cannot read exactly is a position nobody is
  tracking. An absent one is a normal first boot.
- New counter `engine_vrp_settled_unpriced_total`. **Non-zero is a
  reconciliation item, not a routine counter:** an in-the-money expiry
  whose contract had already rolled off the boot chain, so the member
  closed the position out of its own book without being able to price
  it. There is no symbol to submit against and the persisted one names
  a different instrument on the new boot, so booking it would be worse
  than counting it.
- Writes are atomic (temp file + rename) and a failed write is logged,
  never fatal: taking a running engine down over a full disk while it
  holds a position is worse than losing the ability to restore one.

**Operator action**
- Nothing to install — the engine creates the file.
- A boot logging `vrp: kill criterion 3 was ARMED before this restart`
  means the member will not enter. Investigate before deleting the state
  file; deleting it is what clears the halt, and it also discards the
  QLIKE window that armed it.

## 2026-09-10 — Deribit position caps are in COINS (operator amendment)

**What changed**
- New shared table `strategy_core::{VenueCaps, CAPS_BASE, CAPS_DERIBIT,
  caps_for_venue, caps_for_sym}`. Deribit: **1 whole coin** per order and
  per symbol, **$250 000** book total. Every other venue keeps the base
  tier ($10k / $20k / $100k), unchanged.
- `strategy-vrp` and `strategy-icdp` both read that table.
  `strategy_icdp::{CAP_LEG_1E6, CAP_SYM_1E6, CAP_TABLE_1E6}` are now
  re-exports of `CAPS_BASE`'s fields, so the two members cannot drift.
- `vrp.toml.example` size returns to **one contract** (`qty_1e6 =
  1000000`) with `band_qty_1e6 = 50000` — the edge spec's measured
  configuration, now exactly on the cap rather than eight times over it.

**Why a different unit for one venue**
A Deribit inverse contract IS one whole coin: its size is a coin count
and its dollar value moves with the index. A dollar cap therefore shrinks
the permitted size as the coin rises, and the member starts refusing at a
price nobody chose. A coin cap is the venue's own unit and holds at any
index.

**Ripple effects**
- `0` in a unit means "this member cannot express this venue's cap",
  never "unlimited". `strategy-icdp` sizes in dollars, so it now REFUSES
  a Deribit instrument at `configure` instead of reading Deribit's
  `leg_usd_1e6 == 0` as no limit. No behaviour change today — `icdp.toml`
  carries no Deribit instrument — but a future one is a boot refusal
  with a message, not a silent unbounded order.
- **The VM clamp and the ruleset validator are deliberately NOT
  amended.** `strategy_vm::POLICY_SINGLE_ORDER_CAP_1E6` and
  `ingress_ai::RULE_*_MAX_RISK_1E6` keep $10k / $20k / $100k; they are an
  independent tighten-only layer over AI-pushed rows, with hash-pinned
  rulesets and a proptest depending on those numbers. An AI-pushed VM row
  on a Deribit option is still capped at $10 000.
- Reports and gates over non-Deribit venues are byte-identical.

**Operator action**
None beyond the `vrp.toml` you install at V8 — the example already
carries the amended size.

## 2026-09-10 — European cash settlement at expiry (VRP VX, ruling O-D4)

**What changed**
- `strategy-vrp` gains the settle rung. On the first callback — tick OR
  option summary — with `wall_ns >= expiry_ns` while still holding, the
  member books the European payoff `max(0, S − K)` (call; mirrored for a
  put) at the freshest index it has, flattens the hedge, folds the hold
  into the forecast and ends the campaign. New counters `settled_itm` /
  `settled_otm` (`engine_vrp_settled_itm_total`, `_otm_total`).
- **An OTM expiry emits no order at all.** The option is worth nothing
  and the venue charges nothing for it, so there is no fill to price: a
  zero-priced order would be a fiction and a mark-priced one would book
  value that expired.
- `ModelParams::opt_fee` gains `settle_index_tenth_bps` (Deribit 15 =
  0.00015 = 1.5 bps). `FillEngine` gains `set_opt_expiry`, and a fill on
  an option sym AT OR AFTER its expiry is priced as a SETTLEMENT:
  `min(0.00015 × index, 0.125 × settlement value)` instead of the trade
  law's `min(0.0003 × index, 0.125 × premium)`. Both harness loaders
  populate the expiry map from the run's own registry — `backtest` keyed
  on the remapped sym (a chain that rolled across boots is one
  instrument), `audit-pnl` on the interned dense sym.
- New `fee_ceil_tenth_bps_1e12`, because the venue's settlement rate is
  exactly half its trade rate and half of 3 bps is not an integer number
  of bps. Rounding it to 1 or 2 would mis-state every expiry by a third.

**Why**
Operator ruling O-D4. The member's normal exit is E − ε, and V0(b)
measured that choice; VX is not an alternative exit but the last word on
a position that is still open at expiry — a submit ring that stayed
full, a data gap that swallowed the ε instant, an option lane that went
quiet. At expiry the instrument stops being tradeable and becomes cash,
so the member books cash.

**Ripple effects**
- **The settlement classifier is derived, not flagged.** No new wire
  field: the venue's own rule is "an option stops trading at expiry", so
  a fill's timestamp against the registry's `expiry_ns` IS the
  classification. A sym with no expiry recorded is never a settlement —
  the trade rate, never a guess.
- The settlement crossover sits at `value = index × 15/12500` = 0.12 %
  of the index, exactly half the trade crossover's 0.24 %.
- `--opt-fee <venue>:<index_bps>:<prem_bps>` carries the settlement leg
  with it (`settle_index_tenth_bps = index_bps × 5`, the venue's half
  relationship), so a fee sweep cannot leave the two disagreeing.
- Reports over roots with no option records are unchanged: `fee_for`
  cannot reach the settlement branch without an index AND an expiry.
- The member picks the FRESHER of its two index sources (the option
  record's forward, the underlying tick's mid) rather than preferring
  one. The option lane can go quiet for hours while the perp keeps
  printing, and settling against an eight-hour-old forward would book a
  payoff the option did not have. With neither, the settlement is
  DEFERRED to the next record and counted — a settlement priced off an
  invented number is worse than a late one.

**Operator action**
None.

## 2026-09-10 — **strategy slot 1 changes meaning: `ev` → `vrp`** (VRP V7)

**What changed**
- `crates/strategy-set` drops `strategy-ev` and composes
  `strategy-vrp` at slot 1. The slot NUMBER is unchanged and wire-stable:
  `Order.strategy_id` 1, `AiCmd::strategy_id` 1 and the enable bit
  `1 << 1 = 2` all mean exactly what they meant. **What changed is the
  member behind the number.**
- `mask_for_name`: `"ev"` is GONE (returns `None`); `"vrp"` = 2 and
  `"ai+vrp"` = 50 are new. `"all"` is unchanged at 127 and now composes
  vrp. `SLOT_VRP` / `BIT_VRP` are the primary constant names;
  `SLOT_EV` / `BIT_EV` remain as identical-valued aliases for readers of
  pre-boundary captures.
- New flag `--vrp <path>` on `run`. An ABSENT `~/multivenue/vrp.toml`
  leaves the member unconfigured and its bit unset — the `icdp.toml` law.
  `scripts/engine-wrapper.sh` accepts `STRATEGY=ai+vrp` and
  `STRATEGY=vrp` (still `--paper`, never `--live`).
- New metric family `engine_vrp_*` (18 counters + 4 gauges as of P0;
  the line below described the family at V6, when it was 11 + 4). The gauge
  `engine_strategy_ev_active` is **renamed** `engine_strategy_vrp_active`.
- `regime.toml`: the coded-member label key is now `[labels.vrp]`.
  **`[labels.ev]` refuses the boot** ("unknown coded member") rather than
  silently gating a different strategy with a label that was evidenced
  for the old one.
- Slot-1 display names follow: `audit-pnl`'s `strategy_label(1)` and
  `engine_snapshot::SLOT_NAMES[1]` both read `vrp`.
- **`crates/strategy-ev` stays**: workspace member, source, tests, and
  the standalone `--strategy ev` engine loop. Only the SET stopped
  referencing it.

**Why**
Operator ruling O-D2. The EV member was never enabled in the composed
mask and the VRP lane needed a slot; reusing the number keeps every wire
value and capture format stable while the member behind it changes.

**Ripple effects — the boundary**
- **A capture taken BEFORE 2026-09-10 carries EV rows under slot 1; one
  taken after carries VRP rows.** There is no way to tell from a row
  itself — the `strategy_id` byte is the same. That is exactly why this
  entry exists. `audit-pnl` labels every slot-1 row `vrp`, so a
  pre-boundary root's report names a member that did not produce it.
- Any dashboard panel or alert on `engine_strategy_ev_active` must move
  to `engine_strategy_vrp_active`.
- Any `~/multivenue/regime.toml` carrying `[labels.ev]` must be renamed
  to `[labels.vrp]` before the next boot, or the boot refuses. Under
  `[labels] require = 1` slot 1 still needs a label.

**Risk policy**
`docs/risk-policy.md` is updated in this same change: the three notional
caps now name `strategy-vrp` as an enforcement site (it gates on the
worst-case Δ = 1 hedge, refusing the entry rather than clamping the
hedge), and **kill criterion 3 is added to the kill-switch triggers** as
a member-scoped sticky halt — the VRP member stops entering for good
once a full trailing-60 window shows its forecast no longer beating
implied vol. The shipped `vrp.toml` size is 0.1 contracts, not the edge
spec's nominal 1.0: one contract's worst-case hedge is eight times the
$10 000 single-order cap.

**Operator action**
- Before the next boot with the VRP lane on: rename `[labels.ev]` →
  `[labels.vrp]` in `~/multivenue/regime.toml` if it exists.
- Repoint any `engine_strategy_ev_active` dashboard panel.
- Nothing else. Slot 1 has never been enabled in a live mask, so no
  running configuration changes until the V8 operator gate.

## 2026-09-10 — new crate `strategy-vrp` (VRP V6)

**What changed**
- New workspace member `crates/strategy-vrp`: the delta-hedged
  variance-risk-premium member. One short-dated Deribit option per daily
  expiry, entered only when the venue's implied vol leaves the band
  `core-vol`'s forecast opens around it, delta-hedged on the hour against
  the venue's own BS delta, exited at `E − ε`.
- `strategy-core` gains `VrpCounters` and a defaulted
  `StrategyCounters::vrp_counters()` accessor — defined there, like
  `IcdpCounters`, so the cli never names a member crate.
- The coin→USD denomination law moved from `cli::backtest::opt` to
  `opt_registry::coin_to_usd_1e6`, the crate that owns contract size.
  `cli::backtest::opt::coin_mark_to_usd_1e6` now delegates to it, so the
  live member and the harness cannot drift about what a premium is worth.
- New alloc gate `vrp_member_tick_and_opt_summary_are_zero_alloc`; the
  gate count rises 44 → 45.

**Why**
Written from the `Strategy` trait up rather than adapted from
`strategy-ev` (operator ruling O-D2): the two strategies share a slot
number and nothing else, and a copied file carries its previous
assumptions invisibly.

**The four doctrine clauses, verbatim in the crate header**
1. Paper has no fills — SUBMIT is the position event, and only V8's
   shadow reconciliation will ever check that assumption.
2. The book is USD-denominated, so the plain unadjusted BS delta is the
   correct hedge ratio; Deribit's account-level `DeltaTotal` carries an
   inverse-contract adjustment that must NOT be applied here.
3. Deribit option marks are coin-denominated; every USD number in the
   crate goes through `opt_registry::coin_to_usd_1e6`.
4. E1 is the mechanism and E2 the monetisation, so the QLIKE comparison
   is computed beside the bounds and exposed as a LIVE counter
   (`qlike_har_beats_iv`) — kill criterion 3 must be visible on a
   dashboard, not discoverable in a later report.

**Ripple effects**
- **Nothing is composed.** `strategy-vrp` is not referenced by
  `strategy-set`; a boot cannot be disturbed by this commit. V7 does the
  binding.
- The member's public state (`side`, positions, selected sym, counters)
  is read-only from outside — the cli reads counters through the
  `StrategyCounters` accessor.

**Operator action**
None. The member is unreachable until V7 binds it and V8 enables it.

## 2026-09-10 — `vrp-seed.tsv` + `--vrp-seed` (VRP V5)

**What changed**
- New worker module `claude_worker.vrp_seed`. Lane
  `python -m claude_worker.vrp_seed seed-out --db <candles.db>
  --descriptor <d> --out <vrp-seed.tsv>` cuts `expiry_ts_ms\tx_1e9\ty_1e9`
  rows for settled daily expiries out of `candles.db`, using the SAME
  integer law `core_vol` runs (`claude_worker.vol_ref`).
- `window_root.cut_run` gains an optional `vrp=<vrp.toml>` argument. When
  given alongside the existing `seed=(regime.toml, candles.db)` pair it
  writes the window's own `vrp-seed.tsv` beside `regime-seed.tsv` and
  `funding-seed.tsv` — seeded **as of the window's first instant**, never
  the wall clock.
- New engine module `cli::vrp_boot` and a `--vrp-seed <path>` flag on
  `run`, `backtest` and `audit-pnl`. `core_config::vrp` gains
  `parse_seed` / `load_seed` / `default_seed_path`.
- Boot tell: `vrp: seed applied pairs=N decisive=<bool> from <path>`, or
  `vrp: seed absent — the member holds until it has 60 pairs`.

**Why**
The forecast needs 60 settled `(x, y)` pairs before it will produce a
bound, and the engine restarts about three times a day. Without a seed
the member would be blind for two months after every restart.

**The two laws this file exists to enforce**
- **A cold boot is legal.** An absent default seed never refuses a boot;
  the member holds and the tell says so. A seed that is PRESENT and wrong
  — a float, a short row, a repeat, rows out of order — does refuse it: a
  seed the engine cannot read exactly is a fit nobody measured.
- **No lookahead.** A pair is `(x formed from the 1440 minutes BEFORE the
  entry instant, y realised over the hold that followed)`. A pair whose
  hold has not settled is never emitted, and `x` is formed by feeding a
  fresh engine only the minutes strictly before entry — the two windows
  are disjoint by construction, so no minute of a hold can reach its own
  regressor. Both are tested
  (`test_an_unsettled_hold_is_never_emitted`,
  `test_a_pair_is_cut_from_two_disjoint_windows`).

**Ripple effects**
- `vrp-seed.tsv` lives OUTSIDE git, beside `icdp.toml`, exactly like
  `regime-seed.tsv`. Fitted numbers go in the vault, never in the tree.
- Nothing consumes the loaded rows yet — the member arrives in V6 and is
  composed in V7. `--vrp-seed` on `run` today reads, validates and
  reports; it changes no behaviour.
- Schema-1, `AUDIT_PNL_VERSION` and `detail_version` are untouched.

**Operator action**
None yet. At V8, cut the live file with
`python -m claude_worker.vrp_seed seed-out --db ~/multivenue/candles.db
--descriptor deribit:BTC-PERPETUAL --out ~/multivenue/vrp-seed.tsv`.

## 2026-09-10 — new crate `core-vol` + `~/multivenue/vrp.toml` (VRP V4)

**What changed**
- New workspace member `crates/core-vol`: the integer volatility forecast.
  A rolling HAR over 1-minute returns (`[i64; 1536]` ring, three window
  sums rolled in O(1)), an OLS fit in log space over a 128-pair ring, and
  `bounds(tau_ns, theta_1e9)` returning the two `i64` an annualised
  `mark_iv_1e9` is compared against. `core_vol::fx` carries integer
  `log2/exp2/ln/exp` on 257-entry Q32 tables. No floats outside
  `#[cfg(test)]`; nothing allocates after `VolEngine::new`.
- New config namespace: `core_config::vrp` parses `~/multivenue/vrp.toml`
  (template: `vrp.toml.example`) — the same hand-written TOML SUBSET as
  `icdp.toml`, integers only, unknown/missing/duplicate keys fatal. Keys:
  `theta_1e9`, `tau_ns`, `epsilon_ns`, `selection_ns`, `rebalance_ns`,
  `qty_1e6`, `band_qty_1e6`, `underlying_descriptor`,
  `hedge_descriptor`. It also carries `parse_seed_row` for V5's
  `vrp-seed.tsv`.
- `core-config` gains a dependency on `core-vol`, for one reason:
  `tau_ns` is validated by `core_vol::tenor_of`, the same function the
  forecast consults. **Only 4 h and 8 h exist.** E1 — the fact the lane
  monetises — is measured at 4 h and 8 h and is absent by 12 h, so kill
  criterion 4 forbids the longer cells, and there is exactly one place in
  the tree that knows it.
- New Python mirror `claude_worker.vol_ref` implementing the identical
  integer law, and a shared parity fixture
  `claude-worker/tests/fixtures/vol/parity-1.{input,expected}.tsv`. The
  expected file is written by the RUST side
  (`CORE_VOL_PARITY_WRITE=1 cargo nextest run -p core-vol --test parity`)
  and asserted by both `crates/core-vol/tests/parity.rs` and
  `claude-worker/tests/test_vol_ref.py`.
- New alloc gate `vol_engine_minute_and_bounds_are_zero_alloc`; the gate
  count rises 43 → 44.

**Why**
The seed the engine boots from (V5) is cut by the Python; the pairs the
engine forms afterwards continue the same series. If the two
implementations disagree by one unit anywhere, the line the engine trades
on is not the line the research measured — and nothing else in the lane
would notice. Integers across both languages plus a fixture written by one
and asserted by both is the only arrangement where that cannot happen
quietly.

**Ripple effects**
- Nothing is wired into the engine yet. `core-vol` is a leaf crate with
  one consumer (`core-config`, for the tenor check); no strategy composes
  it and no boot path reads `vrp.toml` until V7.
- `docs/local-setup.md` gains nothing yet — `vrp.toml` is only required
  once the member is composed (V8, operator-gated).

**Operator action**
None yet. When the lane reaches V8, copy `vrp.toml.example` to
`~/multivenue/vrp.toml` beside `universe.toml` and `icdp.toml`.

## 2026-09-10 — `detail_version: 6` — the assumed option spread is a flag (VRP V3)

**What changed**
- New flag `--option-spread-frac <ppm>` on `backtest` and `audit-pnl`
  (not on `run`). It is the ASSUMED **crossed** option spread in
  parts-per-million of premium — `50000` = 5 % — and half of it is
  charged on each side of the D-7 synthetic option tick:
  `bid = mark − h`, `ask = mark + h`. The parser rejects anything above
  `1000000` (100 %).
- `crates/cli/src/backtest/fill.rs` gains `opt_half_spread_1e6`, which
  takes the **larger** of the old D-7 floor (`max(0.5 % of mark, 1
  tick)`) and `mark × frac / 2`, ceil-rounded. The flag can therefore
  only ever WIDEN: `--option-spread-frac 0` (the default) reproduces
  every pre-V3 number bit for bit, and the 0 rung of the ladder is an
  **upper bound** on the edge, not a middle estimate of it.
- Both reports now print ONE identical assumption sentence, rendered by
  the shared `backtest::opt::render_opt_mark_law`, naming the rung the
  run executed at. It is printed whenever any option sym was registered
  under the D-7 law — registration, not fills, is what makes the
  assumption able to shape a number.
- `--emit-detail` sidecar `detail_version` **5 → 6**:
  `model.opt_spread_frac_1e6` and a new `options` block
  (`mark_syms`, `mark_fills`, `spread_frac_1e6`, `law`).
- `audit-pnl` stdout gains an `options` object with the same three
  values, emitted **only** when option syms were registered, so
  `audit_pnl_version` stays `1` and an option-free root renders
  byte-identically.

**Why**
No options book exists anywhere in the capture — Deribit's TAIL rows
carry a top-of-book quote and a mark, never a ladder — so every option
fill in either report is a MODEL fill at `mark ± half-spread`. That
half-spread was a hard-coded 0.5 % with no way to ask what the cell is
worth if the real spread is 2 %, 5 % or 10 %. The edge spec's headline
number is only meaningful next to that ladder, and a number whose
assumption cannot be varied cannot be falsified.

**Ripple effects**
- Default behaviour is unchanged: schema-1 stdout, `AUDIT_PNL_VERSION`,
  the `run` surface and the frozen worker argv are all untouched.
- The sidecar changes for EVERY root, option-carrying or not — that is
  what the version bump is for. The `options` block on an option-free
  root reads `mark_syms: 0, mark_fills: 0`.
- First-order magnitude, for reading the ladder: a round trip pays
  exactly `2h` per contract, so the option leg moves down by
  `premium × frac` per round trip and `premium × frac/2` per crossing.

**Operator action**
None. To reproduce a pre-V3 number, omit the flag (or pass `0`).

## 2026-09-10 — Deribit option trade fees are now CAPPED (VRP V2b)

**What changed**
- `crates/cli/src/backtest/fill.rs` gains `FillEngine::fee_for`, which sits
  between `book_fill` and `fee_ceil_1e12`. A sym that has been given an
  index price (`FillEngine::set_opt_index`) and whose venue has an ACTIVE
  `ModelParams::opt_fee` row pays
  `min(index_bps × index × qty, prem_bps × premium × qty)`, each leg
  ceil-rounded in `i128`. Every other sym takes the flat `fee_bps` path
  it always took, byte for byte.
- New `ModelParams::opt_fee: [OptFee; 7]`, indexed by venue byte.
  **Deribit is ACTIVE by default** at its published schedule —
  `index_bps = 3` (0.03 % of the index) capped at `prem_bps = 1250`
  (12.5 % of the premium). Every other venue is `OFF`.
- New flag `--opt-fee <venue>:<index_bps>:<prem_bps>` (and
  `<venue>:off`) on `backtest` and `audit-pnl` **only**. It is
  deliberately NOT on `run`: the live engine does not price options.
- Both loaders now carry the per-sym index. `backtest` populates it in the
  replay pre-pass; `audit-pnl` threads an `opt_index_1e6` map out of
  `load_run_events`. The index is `OptSummary.underlying_px_1e9 / 1_000`
  — the same forward Deribit itself charges against.

**Why**
Deribit's option fee is `min(0.0003 × index, 0.125 × premium)`, not a flat
bps of notional. At these strikes the two legs cross at
`premium = index × 3 / 1250` = **0.24 % of the index** — $189.60 at an index
of $79,000. Below that premium the 12.5 % cap binds; above it the index leg
binds. The measured median premium on the captured chain is $219.07, i.e.
just above the crossover, so BOTH regimes occur inside a single window and
a flat-bps model is wrong in both directions. With the default
`fee_bps = (0, 0)`, option fills previously paid **nothing at all**.

**Ripple effects**
- A backtest or audit-pnl over a root that CAPTURED options now charges the
  option leg. Roots with no option records are unaffected — `fee_for`
  cannot fire without an index, and only option syms are given one.
- The `run` surface, schema-1 stdout keys, `AUDIT_PNL_VERSION` and
  `detail_version` are all unchanged: this adds a cost, not a field.
- Settlement fees (`min(0.00015 × index, 0.125 × value)`, OTM expiry free)
  are NOT here — they belong to the expiry path (VX), not the trade path.

**Operator action**
None. Options were never traded live. To reproduce a pre-V2b number over an
option-carrying root, pass `--opt-fee deribit:off`.

## 2026-09-10 — `detail_version: 5` — harness option marks are USD, not coin (VRP V2a)

**What changed**
- The two option mark-tick synthesis sites — `crates/cli/src/backtest.rs`
  (`load_run`) and `crates/cli/src/audit_pnl.rs` (`load_run_events`) — no
  longer rescale a captured `OptSummary.mark_px_1e9` with `/ 1_000`. They
  call one shared helper, `crates/cli/src/backtest/opt.rs`
  `synth_mark_usd_1e6`, which applies the denomination law
  `premium_usd = mark_coin × underlying_px × contract_size` in `i128`.
- `--emit-detail` sidecar `detail_version` **4 → 5**. Schema-1 (stdout) is
  unchanged and `AUDIT_PNL_VERSION` stays `1` — no key was removed and none
  changed type; the bump exists because the MEANING of a price on an option
  sym changed.
- New crate dependency for `cli`: `opt-registry` (VRP V1). Each run builds
  its own `OptRegistry` from that run's `instrument-manifest.tsv`.
- **The option QUOTE tick lane is converted too, and it is the lane that
  actually runs.** Deribit TAIL rows subscribe to `quote` AND `ticker`
  (`ingress-deribit/src/run_loop.rs:815-820`): the ticker feeds
  `OptSummary`, the quote feeds a real `Tick` whose bid/ask are equally
  coin-denominated. Because every option sym therefore HAS a tick lane,
  `tick_syms` suppresses the D-7 synthesis for all of them and
  `opt_synth_ticks` is 0 on any real capture — so converting only the two
  synthesis sites would have fixed nothing that executes. Both loaders now
  build an `UnderlyingBook` (a per-sym timeline of `underlying_px_1e9`
  from that run's own summaries) and convert every real option quote tick
  after loading. A quote with no underlying known at or before its instant
  is DROPPED rather than guessed. Additive counters
  `opt_quotes_converted` / `opt_quotes_dropped`, emitted only when
  non-zero.
- The DEPTH lane needs no equivalent: measured, `deribit-depth.pmlr`
  carries only the 9 static instruments and no options.

**Why**
Deribit options are inverse: quoted, margined and settled in the base coin
with a 1-coin multiplier, so a mark of `0.00038 BTC` at BTC = $79,000 is a
**$30** premium. `FillEngine` books `notional = px × qty` as USD by
assertion (`backtest/fill.rs:53`). The old rescale therefore understated the
option leg by the underlying price — about **79,000×** for BTC — while the
perp hedge leg was already USD. A delta-hedged run would have reported
essentially pure hedge P&L with the option contributing nothing, and nothing
in the output would have said so.

**Ripple effects**
- **A `Price` on a Deribit option sym is now USD ×1e6.** `FeatId::MarkPx`
  (`strategy-vm/src/features.rs:794`) is UNCHANGED and still returns the raw
  coin `mark_px_1e9` — it is a wire value and rulesets that reference it are
  hash-pinned. The two now mean different things on the same instrument;
  that is deliberate.
- Option marks are produced for **Deribit only**. A marked option record
  from any other venue is skipped and counted, never guessed at:
  `underlying_px_1e9` is venue-inconsistent by construction (OKX puts
  `fwdPx` there and supplies no mark at all; Binance-eapi quotes in USDT).
- New additive counter `opts_unconverted` on both reports — marked option
  records that could not be denominated (foreign venue, a sym the run's
  manifest never named, a missing underlying, an unrepresentable premium).
  A venue that sends no mark is NOT counted. Both reports emit the line
  **only when it is non-zero**, so a root carrying no option records renders
  byte-identically to before this change.
- Contract size offline is the venue constant `1.0` coin
  (`DERIBIT_OPT_CONTRACT_SIZE_1E9`), because `instrument-manifest.tsv`
  carries only the instrument name. What makes that safe is that
  `opt-registry`'s parser refuses Deribit's USDC-LINEAR chains
  (`BTC_USDC-…`), which do not carry that size. The live boot path (V7)
  will use `DeribitInstrumentRow::contract_size_1e9` instead.
- Registries are built **per run**: option ordinals reshuffle at every boot
  by design (chain roll — `crates/cli/src/options_manifest.rs:8-11`).

**Operator action**
None. Options were never traded live, so no existing report's traded P&L
moves. Reports over roots that CAPTURED options will show different option
prices — those rows previously held a coin number labelled as dollars.

Verified on `run-1788984954632200000` (0.98 h): the option-free root is
byte-identical before/after on all four output surfaces, while the root
with options reports `quote_ticks_usd=56562 quote_ticks_dropped=196` from
`backtest` and the identical pair from `audit-pnl` — two independently
written loaders in different symbol spaces agreeing, and both matching a
from-scratch reconstruction off the raw capture (64 quotes preceding their
sym's first underlying print + 132 one-sided books = 196).

## 2026-09-05 — `archive_manifest_version: 1` (object-storage archive)

**What changed**
- A new on-disk/on-bucket schema: `_manifest.json` under each archived run
  prefix, and a byte-identical copy at `v1/index/<host_id>/<run>.json`.
  Top-level keys: `archive_manifest_version, run, epoch_ns, host_id,
  uploaded_at_ns, tool_version, stored_encoding, zstd_level, files, totals,
  span_ns, pmlr_version, windows_2h_complete, catalog, catalog_note`
  (+ `catalog_error` when the catalog could not be produced).
- New optional config namespace `MULTIVENUE_S3_*`, read ONLY by
  `claude_worker.archive_config`, from `~/multivenue/s3.env` (template:
  `s3.env.example`). Not in `.env`: `engine-wrapper.sh` sources that with
  `set -a`, and the archive credential has no business in the engine's
  environment.
- `~/multivenue/retention.conf` gains `ARCHIVE_MODE=s3` and
  `S3_CYCLE_BUDGET_S`.
- `claude-worker/pyproject.toml` gains a SECOND console script,
  `multivenue-archive`. `dependencies` is unchanged — the S3 client is
  handwritten SigV4 over `hmac`/`hashlib`/`httpx`, so no cloud SDK enters
  either dependency graph.

**Why**
- Capture was single-copy on one Mac, and `retention.sh` deleted the oldest
  runs under disk pressure with nothing ever reading the tarballs it made.
  Every research gate is stated in ≤ 2 h windows that already exist, so
  deleting capture deletes the only input those gates have.

**Impact**
- **No engine change of any kind.** No Rust file, no `Cargo.toml`, no new
  engine flag or env key; `target/release/multivenue-engine` is not relinked
  and needs no restart. The PMLR wire format is untouched.
- **Absent config = absent behaviour.** With `MULTIVENUE_S3_ENABLED` unset,
  no HTTP client is constructed, the resolver reports every run as local or
  absent, `retention.sh` uses `compress` exactly as before, and the nightly
  report has no `data_source` key.
- Pulled runs are byte-identical to the originals (file names preserved;
  `.zst` is only the storage encoding), so `discover_runs`, `run_dirs`,
  `select_runs` and `window_root.cut_run` behave identically on them.
- Rollback is one line in `retention.conf` (`ARCHIVE_MODE=compress`) or
  `MULTIVENUE_S3_ENABLED=0`; effective at the next tick, no restart, no data
  movement. Bucket objects are never deleted by this subsystem.

**Layout versioning**
- The bucket prefix `v1/` IS the layout version. A layout change is a new
  prefix, never an in-place rewrite. `epoch_ns` is the partition key so that
  lexicographic `ListObjectsV2` order is chronological order — do not insert
  `YYYY/MM/DD` levels, which would break it.

**Amended 2026-09-07 — went live; defaults re-derived by measurement**
- `MULTIVENUE_S3_PART_SIZE_MIB` 8 → **32** and `MULTIVENUE_S3_TIMEOUT_S`
  300 → **900**. Multipart costs ~3× a single PUT *per part*, so larger parts
  recover most of that; a part must still finish inside the timeout on a bad
  window, and this link has been seen at both 0.1 and 7 MiB/s.
- New key `S3_NICE_NETWORK` in `retention.conf` (default 0). It is the ONE
  lever for the uploader's network class. **Do not put `ProcessType=Background`
  back in the launchd plist**: it applies background QoS to the whole job and
  silently overrides this key, which throttles sustained transfers ~13× and
  starves the cycle until retention can no longer verify-and-delete.
- `S3_CYCLE_BUDGET_S` default 600 → **3600**.
- `gc --max-gib` now takes **-1** to mean "use the configured cap"; `0` means
  "empty the cache" and is honoured (it used to be swallowed as falsy).
- `pnl_report --day` resolves an archived day through the object store, but
  only as a FALLBACK — when the day has no local run at all. A locally-present
  day costs zero HTTP requests.
- **Retention policy change (operator ruling, supersedes D3):** `PROTECT_DAYS`
  5 → 1 with `MIN/TARGET_FREE_GIB` set unreachably high, so the sweep runs
  nightly rather than only under pressure and old runs are DELETED (after
  verification) rather than tarred. The effective local window is **24–72 h**:
  `age_days` is integer, so anything under 48 h is protected, and the sweep
  runs once a day.
- Legacy `~/multivenue/archive/*.tar.gz` are pushed verbatim and pull back via
  `pull` (download → verify sha256 → `tar -xzf`), so the pre-2026-08-29 history
  is retrievable through the same verb as everything else.

## Template

```
## <YYYY-MM-DD> — <short headline>

**What changed**
- ...

**Why**
- ...

**Impact**
- On-disk formats: ...
- Config keys: ...
- Wire formats: ...

**Migration steps**
1. ...
2. ...

**Rollback**
- ...
```

## 2026-09-07 — `regime.toml` hysteresis keys: per-profile `confirm_min` + TREND/VOL/STRETCH exit bands (RG7 fix)

**What changed**

- `[profile.<name>]` gains four OPTIONAL integer keys — `confirm_min`
  (this profile's consecutive-minute confirm; 0 = `[hysteresis]
  confirm_min`), `trend_exit_bps_1e9` (a committed BULL/BEAR holds while
  `|ret| > exit`; breadth is an entry condition), `rv_exit_frac_1e9`
  (LOW holds while `rv < p30·(1+f)`, HIGH while `rv > p70·(1−f)`),
  `stretch_exit_k_1e9` (an EXT holds while `|s| > exit`). Bands must lie
  inside their entry thresholds (`RegimeErr::Bands`, boot refused).
  `core_regime::ProfileParams` carries them (`with_hysteresis`;
  `RegimeParams::{confirm_of, max_confirm_min}`); `judge_trend` /
  `judge_vol` / `judge_stretch` take the committed state like
  `judge_shape`; the seed replay warms `2·max confirm`. The worker
  mirror (`claude_worker.regime`) parses and judges identically; the
  parity harness now runs TWO fixtures (`parity-1` = the RG1 law,
  unchanged bit for bit; `parity-2` = the same tape under the keys).
- `regime.toml.example`: the fast profile carries `confirm_min = 10`,
  `trend_exit_bps_1e9 = 20000000000`, `rv_exit_frac_1e9 = 100000000`,
  `stretch_exit_k_1e9 = 1500000000` and a wider SHAPE band
  (`er_lo_exit_1e9 = 400000000`, `er_hi_exit_1e9 = 500000000`); the
  slow profile is unchanged.

**Why**

- The first RG7 soak (2026-09-07) FAILED: 7 of 9 windows over the ≤ 2
  flips bound, all on the FAST profile (`shape` 3–7, `trend` 3–6 per
  2 h). RG1 had implemented the §3.5 bands for SHAPE only, and a global
  `confirm_min = 3` is thin for a 60-min horizon judged in 5-min steps.
  Offline replay over the failed windows: pre-fix 8/9, confirm 10 alone
  2/9, bands alone 6/9, both 1/9.

**Impact**

- On-disk formats: none. Wire formats: none. Config: four optional keys
  per profile; every existing `regime.toml` boots and judges unchanged
  (keys absent ⇒ 0 ⇒ the RG1 law). The live file needs the keys for the
  fix to apply; the boot reads them (restart-applied, as every regime
  parameter).
- `RegimeState` layout: `ProfileParams` grew (the `_pad0` byte + 3
  trailing `i64`) — a boot-boxed struct, no wire or snapshot exposure.

**Migration steps**

1. `cargo build --release -p cli`; add the keys to `~/multivenue/
   regime.toml` `[profile.fast]` (values as the example); restart.
2. The RG7 soak restarts from zero: only windows AFTER the restart count.

**Rollback**

- Delete the keys (or set them to 0) and restart — the RG1 law, bit for bit.

## 2026-09-05 — The harness funding seed: `funding-seed.tsv` per window, `backtest --funding-seed` (RG4 carry blocker)

**What changed**

- `backtest` gains `--funding-seed <path>` (`cli::backtest::funding`):
  a `descriptor \t ts_ms \t rate_1e9` file (`#` comments; malformed =
  fatal) becomes synthesized `AiCmdKind::FundingSeed` commands applied
  through the vm's live `on_ai` path — dedup law included — BEFORE the
  first replayed record. Default = the first run directory's own
  `funding-seed.tsv` when it exists, else none. With a seed applied the
  V5 warm-up law drops the `apr24`/`apr72` 24 h / 72 h requirement
  (the seed IS the prints' history; a seed shorter than a feature's
  window leaves that feature ABSENT by the feature law). Summary gains
  `funding: seed_prints=… dropped=… deduped=… warmup=seeded|table`
  plus a `funding: seed <path> …` build tell. `audit-pnl` is untouched
  (it audits what happened; no vm warm-up is involved).
- Worker: `window_root.cut_run` writes the window's `funding-seed.tsv`
  whenever `seed=(…, candles.db)` names a store with a funding table —
  the window's manifest × the funding table under the boot seed lane's
  own law (`claude_worker.seeds.funding_seed_rows`: 73 h before the
  window's first instant, exclusive; newest 640 per sym; rate ×1e9
  RAW). `pool_ensure` back-fills the file on reused cuts (no re-cut).
  Neither file is in git (a window root is research data).

**Why**

- Under the ≤ 2 h law a funding-carry member (`apr24`/`apr72`) could
  never be evidenced — the table-global 24 h warm-up swallowed every
  2 h window (0 orders). Live, the same row is warm from its first
  minute because the boot seed lane pushes 73 h of prints; the replay
  now has the same warm-up, so the library / composer can evidence
  carry members on the standing pool (RG4's "≥ 2 members" exit tell).

**Impact**

- On-disk formats: a new optional per-window file `funding-seed.tsv`
  beside the manifests. Schema-1 stdout unchanged; `--emit-detail`
  unchanged (`detail_version` 4).
- Config keys: none. Wire formats: none (kind 10 frames, unchanged).
- Behaviour: NONE without a seed file (`warmup=table` = the V5 law);
  a root whose first run carries `funding-seed.tsv` replays seeded —
  the frozen worker argv picks it up by default, which is the point.

**Migration steps**

1. `cargo build --release -p cli` (pitfall 18 — the harness is the
   release binary).
2. Existing pool windows gain the file on the next `pool_ensure`
   (composer run) with `candles.db`; single windows: cut again with
   `seed=(regime.toml, candles.db)`, or pass `--funding-seed`.

**Rollback**

- Delete the per-window files (or pass `--funding-seed /dev/null` —
  an empty seed = `warmup=table`).

## 2026-09-05 — Ruleset grammar v2.1 regime keys, the vm row gate, the regime-aware harness (RG3)

**What changed**

- Ruleset JSON grammar v2.1: three optional v2 row keys — `regimes`
  (string array, the §3.3 label grammar with `fast:`/`slow:` prefixes
  and `rel:` terms), `regime_off` (`soft`|`hard`), `rel` (sugar for one
  `rel:` term) — parsed by the ingress-ai byte scanner into the
  `RuleRowV2` regime tail RG0 reserved (`regime_fast`/`regime_slow`/
  `regime_off`/`regime_rel`). Validator **rule 11** (`RulesetReject::Regime`)
  and the **rule-8 amendment** (identity-tuple duplicates only when the
  regime regions intersect — disjoint variants of one signal admit).
  `RegimeLabel::LABELLED_ANY` (core-types) = the fill of a REL-only
  profile. Rows without the keys are bit-identical (tail zero) — every
  existing artifact hashes and validates unchanged.
- `strategy-vm`: per-row gate byte `row_gate` judged on every
  `set_regime_view` (the new set→vm seam: `core_regime::RegimeView`,
  `RegimeState::view()`, pushed by `StrategySet` on configure / seed /
  every minute roll / effective change / declaration) and on every
  flip; the entry path skips closed rows (`regime_blocked`), the exit
  path flattens HARD-closed position rows at once (`regime_hard_exits`;
  age-out first, min-hold bypassed); soft-closed rows drain by their
  own law. `StrategyCounters::vm_regime_{blocked,hard_exits}`,
  `/metrics` `engine_vm_regime_{blocked,hard_exits}_total`.
- Harness: `backtest` / `audit-pnl` gain `--regime <path>|off` and
  `--regime-seed <path>` (`cli::backtest::regime`): the engine's own
  `RegimeState` replays over the window's ticks, funding prints and the
  `SetRegime` frames of `ai-cmds.pmlr` (pre-anchor frames clamped with
  their TTL shortened; expired ones dropped + counted); the vm receives
  the view exactly as live; `off` strips every tail (the on/off delta);
  absent flag = the default artifact when it exists and resolves on the
  root (members absent from the root are DROPPED, refs must resolve),
  else regime-blind with a stderr tell — the frozen worker argv can
  never fail on a default artifact. `--emit-detail` is `detail_version`
  4 (additive `regime` block); `audit-pnl` JSON gains the additive
  `regime` section (per profile: minutes per effective word, per
  `(word, strategy)` fill-model replays with the fee ladder;
  `audit_pnl_version` stays 1). `cli::regime_boot` is the ONE resolver
  the engine boot and the harness share.
- Worker: `window_root.cut_run` carries the pre-window `SetRegime`
  declaration still in force at the cut (latest frame per profile
  decides) in front of the `ai-cmds.pmlr` slice, and writes the window's
  own `regime-seed.tsv` from `candles.db` when `seed=(regime.toml,
  candles.db)` is given (`pnl_report` day mode passes the defaults);
  `strategist` prompt **v3** (`STRATEGIST_PROMPT_VERSION =
  "strategist-v3"`) teaches the keys, the gate law and regime variants
  and asks for `regimes` on every row; `parse_proposal` accepts the keys
  structurally (`regime_term_ok` / `regime_rel_ok` vocabulary mirrors).
- Bench alloc gate 42 (`vm_regime_gate_and_view_rejudge_are_zero_alloc`).

**Why**

- `docs/arch/regime-and-dashboard-plan.md` RG3: rows gate themselves on the
  regime (D2 — no table flip on a regime change), and every backtest /
  nightly report shows the per-regime P&L and the on/off delta.

**Impact**

- On-disk formats: none new. `--emit-detail` sidecar `detail_version`
  3 → 4 (additive). `audit-pnl` stdout: additive `regime` key.
  Windowed roots may now carry `regime-seed.tsv` and a carried
  pre-window `SetRegime` frame in `ai-cmds.pmlr` (ts before the first
  tick — the harness clamps it).
- Config keys: none. Ruleset artifacts: the three optional row keys.
- Wire formats: none (the tail bytes were reserved at RG0; kind 12
  unchanged).
- Behaviour: NONE for every existing artifact (tail zero ⇒ open under
  every view). A LABELLED row fails closed until the engine's detector
  knows the regime (`~/multivenue/regime.toml` + seed) — and in the
  harness until `--regime` (or the default artifact) resolves.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before any backtest /
   audit-pnl that matters or the next boot.
2. Nothing else: labelled rows only exist once a ruleset carries the
   keys.

**Rollback**

- Revert the commit. Labelled artifacts then reject as rule 2
  (unknown key) at stage — nothing else changes.

## 2026-09-03 — Regime detector in the engine: `regime.toml`, `regime-seed.tsv`, `--regime`, the `engine_regime_*` family (RG1–RG2)

**What changed**

- New crate `core-regime` (the measured regime: minute-close rings,
  the integer judge law, confirm hysteresis, the declared-over-measured
  effective law) and `core-config::regime` (the `regime.toml` parser +
  the seed reader). `ret_bps_1e9` / `isqrt_i128` moved to
  `core_regime::math` (`strategy-icdp` re-exports; `strategy-vm` imports).
- `Strategy` trait: `regime_label` / `set_regime_label` / `on_regime`
  (all defaulted — no existing strategy changes behaviour);
  `StrategyCounters::regime_counters`; `IcdpCounters` gains
  `regime_blocked` / `regime_exits` (additive).
- `StrategySet` owns the detector: a 1 s timer is armed when a detector
  is configured (`REGIME_TIMER_NS`; members' timers were `u64::MAX`);
  `AiCmdKind::SetRegime` is consumed at set level; labelled members
  receive edge-triggered `on_regime` calls. The ai-exec refuses intents
  while its gate is closed; the icdp blocks decisions while closed and
  exits open positions on a HARD close.
- Boot surface: `--regime <path>` (default `~/multivenue/regime.toml`;
  ABSENT default = detector unconfigured, today's behaviour; explicit
  path or present-but-invalid file = boot refused), `--regime-seed
  <path>` (default `~/multivenue/regime-seed.tsv`; absent = warm live).
  `scripts/engine-wrapper.sh` exports the seed
  (`python -m claude_worker.regime seed-out`) before every boot when
  the artifact exists.
- `/metrics`: the `engine_regime_*` family (≈ 50 names) +
  `engine_icdp_regime_{blocked,exits}_total`. Registry headroom holds
  (256 counters / 384 gauges).

**Why**

- `docs/arch/regime-and-dashboard-plan.md` RG1–RG2 (operator decisions
  D1–D4): the engine measures the regime itself, the AI declares, the
  effective word gates members without a table flip.

**Impact**

- On-disk formats: two new DATA files under `~/multivenue/`
  (`regime.toml` — operator-authored from `regime.toml.example`;
  `regime-seed.tsv` — worker-generated, atomic write). Neither is in git.
- Config keys: `regime.toml` grammar per `regime.toml.example`
  (integer-only TOML subset; unknown/missing/duplicate keys refuse the
  boot).
- Wire formats: none beyond RG0's kind 12.
- Behaviour: NONE until `~/multivenue/regime.toml` exists. With it and
  no `[labels.*]`, every coded member stays unconstrained (ANY) — the
  detector only measures and publishes.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before the next boot.
2. Optional: `cp regime.toml.example ~/multivenue/regime.toml` and edit
   `[breadth] members` to descriptors present in `universe.toml`.
3. Restart the standing engine; check the boot tells and
   `engine_regime_configured`.

**Rollback**

- Remove `~/multivenue/regime.toml` (detector unconfigured, no
  behaviour change) or revert the commit. The seed file is inert
  without an artifact.

## 2026-09-03 — `AiCmdKind::SetRegime = 12` + `RuleRowV2` regime tail (RG0 — regime wire freeze)

**What changed**

- `core_types::regime` (new module): `RegimeWord` (one byte per
  dimension, one-hot; bit 7 of each market byte = the per-dimension
  UNKNOWN mark; byte map in `docs/wire-format.md`), `RegimeLabel`
  (any subset per byte; `0` = unconstrained; gate `label == 0 || (word &
  label) == word`; omitted dimensions fill with the legal mask incl.
  the unknown mark, explicit lists exclude it — fail-closed per
  dimension), `RegimeRel` (per-symbol RELATIVE nibbles),
  `RegimeTerm` / `RegimeLabelSet` (≤ 4 product terms, ∃-semantics, for
  coded members), the rule-8 `intersects` law, and the text grammar
  `[fast:|slow:]dim:(*|!v|v1|v2…)` (`parse_label_term`,
  `RegimeLabelBuilder`). `REGIME_PROFILES = 2` is a layout constant.
- `AiCmdKind` gains **`SetRegime = 12`** (append-only ABI; the first
  unassigned byte is now 13) with its `validate_shape` arm: `param_id`
  = profile `< 2`, `px` = declared word (SOURCE byte empty), `qty` = 0
  or a state word, `ttl_ns > 0` ENFORCED, `strategy_id = 0xFF`, sym/side
  none, flags bit 0 legal. `frames.py` mirrors it (`KIND_SET_REGIME`,
  `regime_word()` helpers); the shared golden fixture gains vectors for
  kinds 10, 11 and 12 (it stopped at 9 before).
- `RuleRowV2` spends 18 of its 40 reserved tail bytes: `regime_fast:
  u64` @88, `regime_slow: u64` @96, `regime_off: u8` @104, `regime_rel:
  u8` @105; `_pad3` shrinks to `[u8; 22]` @106. Still 128 B; `ver`
  stays 2; `RuleRowV2::new` keeps its 23-argument shape (tail zero);
  `with_regime(term, off)` sets the tail; `regime_fields_well_formed()`
  is the rule-11 body (RG3 wires it into the validator).
- `audit-replay`'s AI-command table is sized by the last kind (13
  labels) — it indexed out of bounds on any captured seed (kinds 10/11)
  before.

**Why**

- `docs/arch/regime-and-dashboard-plan.md` (operator decisions D1–D4): the
  regime is a gate; VM rows carry their own per-profile masks so a
  regime change never flips a table; the AI declares a word per
  profile through the existing AI plane.

**Impact**

- On-disk formats: none (`RuleRowV2` never crosses a process boundary;
  PMLR `slot_kind = 4` records keep their 64 B shape — kind 12 is a new
  value in an existing byte).
- Config keys: none yet (`regime.toml` arrives with RG2; the example
  file is committed now).
- Wire formats: `AiCmd` kind byte 12 admitted; a pre-RG0 engine counts
  it as malformed (`engine_ingress_ai_malformed_total`) and drops it —
  no crash, no ring push.
- Every existing ruleset artifact validates to bit-identical rows
  (both masks 0); nothing is gated until a row carries a `regimes` key
  (RG3 grammar).

**Migration steps**

1. `cargo build --release -p cli` before any live boot (G0 relink law).
2. Nothing else — no artifact, DB, or capture changes.

**Rollback**

- Revert the commit; the ABI is append-only so no captured file needs
  rewriting. A worker that sends kind 12 to a reverted engine sees the
  frame counted as malformed.

## 2026-09-03 — `Order.ttl_ns` (wire-additive) + the IoC taker fill law + fee ladder (ICDP I1)

**What changed**

- `core_types::Order` spends 8 bytes of its explicit zero padding:
  `ttl_ns: u64` at offset 48 (`_pad1` shrinks to 42..48; `_pad2`
  unchanged). Still 64 B, `repr(C, align(64))`, every byte explicit.
  `Order::new` keeps its 8-argument shape (ttl 0);
  `Order::with_ttl_ns(ttl)` sets it.
- `backtest::fill` (shared verbatim by `backtest` and `audit-pnl`):
  `Order.kind == 1` (IoC) is now MODELED — judged once at the first
  fresh two-sided tick of its sym at/after `t_emit + Δ_venue`: a BID
  fills at that tick's `ask_px` iff `ask_px ≤ P` (an ASK at `bid_px`
  iff `bid_px ≥ P`), qty capped by the displayed opposite size under
  the same shared FIFO budget as makers, the remainder cancels (never
  rests), fee = the venue's TAKER column, rounded up. Any order with
  `ttl_ns > 0` is canceled at the first record of its sym at/after
  `t_emit + ttl_ns` (stale and one-sided records included — expiry is
  a clock fact), before any fill evidence of that record is read.
  `kind ≥ 2` is unroutable (counted, dropped; `debug_assert!` in debug).
- Every fill also accrues the §4.3 **fee ladder** (the same notional
  at flat 0 / 1 / 2 bps per side); reports print OOS net at the ladder
  beside the CLI tier. New counters `ioc_fills`, `ioc_canceled`,
  `ttl_expired`.
- Surfaces: backtest stderr `fills:` line gains `ioc= ioc_canceled=
  ttl_expired=` and a new `fee ladder (oos net, flat bps/side): 0= 1=
  2= tier=` line; **`--emit-detail` is `detail_version` 3**
  (`fills.ioc`, `fills.ioc_canceled`, `fills.ttl_expired`,
  `oos.fee_ladder_net_usd[3]`); audit-pnl per-strategy stderr gains an
  `ioc_fills=… | fee ladder …` row and the JSON (still
  `audit_pnl_version` 1 — additive keys, get-based readers) gains
  `ioc_fills`, `ioc_canceled`, `ttl_expired`, `fee_ladder_net_usd[3]`.
  **Schema-1 stdout is unchanged (frozen).**

**Why**

- ICDP is a taker edge (research note §5.6: resting entries are
  anti-selected); the post-only law scored it negative by construction.
  The vault's merged ICDP×VT plan, D1 (offline IoC model + paper IoC
  intents are pre-Stage-3) and D6 (G1 gate lifted, I1–I7 in order).

**Impact**

- On-disk formats: `engine-orders.pmlr` slots may now carry a non-zero
  `ttl_ns`; every older file reads 0 (never expires). `pmlr.py`'s
  `OrderRec` decodes the 42-byte prefix and is untouched.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Order` table + `kind` semantics.

**Migration steps**

1. Nothing for existing captures or goldens (the VM emits kind 0, ttl 0
   — every existing number is byte-identical).
2. Sidecar readers: accept `detail_version` 3 (additive keys).

**Rollback**

- Revert the commit; IoC intents already captured would replay under
  the maker law again (negative by construction) — the reason for I1.

## 2026-09-03 — Staleness is live: v3 captures start, the harness re-judges, `--emit-detail` v2 (VT2–VT6 close)

**What changed**

- The standing engine writes PMLR v3 since `run-1788417289611943000`
  (relink + reboot 06:34Z); every earlier run dir is v2.
- Every ingress stamps `venue_time_ms` and judges staleness live
  (`core_time::FeedClock`, per-venue `stale_after_ms` defaults pm 1000 /
  bn 1000 / okx 400 / deribit 600 / hl 700 / bybit 500; `run
  --stale-after-ms <venue>:<ms>`); Binance spot inherits the `aggTrade`
  sentinel's stamp + verdict with `TICK_FLAG_VENUE_TIME_SENTINEL`, and
  spot prints are captured as `ChannelId::Trade` event rows.
- strategy-vm: `Mid/Bid/Ask` are ABSENT while the last tick is stale.
- `backtest` / `audit-pnl`: `--stale-after-ms <venue>:<ms>` (both);
  v3 ticks are RE-JUDGED from the stamp per (venue, sym) per run in file
  order (`cli::backtest::stale`), with the **sentinel latch law**: a
  repeated inherited stamp keeps its print's verdict (re-judging it on
  the book update's own `ts_ns` flagged quiet seconds — 3.3 % false stale
  on `binance:btcusdt`, found by the VT2 live smoke). Stale ticks
  neither fill nor mark. stderr gains one `stale:` line per run
  (`stale-blind(v2)` on v2 files). **`--emit-detail` sidecar is
  `detail_version` 2**: `model.stale_after_ms` + a `stale` block
  (`ticks_skipped`, per-run per-lane `{ticks, stale_ticks,
  stale_time_bps, stale_blind}`). Schema-1 stdout is unchanged (frozen).
- `capture-catalog`: per-lane `stale_captured` (the ingress's live
  verdict count; `null` on v2 lanes) in JSON + summary.
- Metrics: `engine_ingress_<venue>_stale_ticks_total`,
  `engine_ingress_<venue>_feed_delay_ema_ms`. `IngressStatus` slot is
  exactly 128 B with zero slack.
- `claude_worker.features.collect_marks` skips stale ticks on v3 files.

**Why**

- `docs/arch/venue-time-capture-plan.md` §1 — stale-blind captures book
  mid-to-mid gains against books the engine could not see. Measured on
  the first v3 run: the VM row's one round trip is +$1.07 stale-blind vs
  −$4.87 judged (vault note).

**Impact**

- On-disk formats: v3 run dirs; `detail_version` 2 sidecars (a v1
  reader must tolerate the new keys — the worker never reads the
  sidecar).
- Config keys: none (`--stale-after-ms` is a flag; the wrapper passes
  none — venue defaults apply).
- Wire formats: none beyond VT1's entry below.

**Migration steps**

1. Research on staleness needs v3 roots: cut ≤ 2 h windows by `ts_ns`
   (the events file cut too) from runs ≥ `run-1788417289611943000`.
2. Treat every `stale-blind(v2)` number as an upper bound (CLAUDE.md
   pitfall 17).
3. Thresholds are re-derived per deployment with the engine-side table
   in `docs/venue-latency.md` §5 — a change is a replay, not a recapture.

**Rollback**

- `--stale-after-ms <venue>:0` on every venue restores the stale-blind
  law offline without a rebuild; the engine flag does the same live
  (ticks still carry the stamp). Reverting the crates restores v2
  writing; v3 files remain readable by the v3 reader only.

## 2026-09-03 — PMLR v3: `Tick.flags` + `Tick.venue_time_ms` (VT1, venue-time capture)

**What changed**

- `core_types::Tick` spends its v2 tail pad: `flags: u8` at offset 49
  (`TICK_FLAG_STALE = 1`, `TICK_FLAG_VENUE_TIME_SENTINEL = 2`) and
  `venue_time_ms: u64` at offset 56; `_pad` shrinks to 6 bytes (50..56).
  Still 64 B, still `repr(C, align(64))`, every byte explicit.
- `Tick::new` keeps its signature and now means "venue time unknown,
  flags 0" (the v2 shape); `Tick::new_stamped(…, venue_time_ms, flags)`
  is the v3 constructor the VT2 ingress parsers will use; `Tick::is_stale`.
- `core_io::VERSION` 2 → 3 for every slot kind (one header number).
  `PmlrReader` accepts ≤ 3 and gains `has_venue_time()` (version ≥ 3).
- `crates/cli` capture acceptance (backtest, audit-pnl, capture-catalog)
  moves from `== 2` to `MIN_PMLR_VERSION ..= core_io::VERSION` through one
  `pmlr_version_accepted` law; v3 ticks replay under the v2 law (never
  stale) until VT4 lands the harness stale rule.
- `claude_worker.pmlr`: `VERSION_MAX = 3`, `TickRec` gains `flags` +
  `venue_time_ms` (+ `is_stale()`), `Reader.has_venue_time`,
  `TICK_FLAG_*` mirrors; golden fixture `ticks_v3.pmlr` (Rust writer);
  `ticks_v2`/`fills_v2`/`ticks_v1` regenerated byte-identical.

**Why**

- `docs/arch/venue-time-capture-plan.md` §1: the capture cannot tell a stale
  Binance book (8.9 % of messages > 500 ms stale) from a current one;
  VT1 is the wire-format prerequisite for the per-venue venue-time
  extraction (VT2) and the staleness gate (VT3/VT4).

**Impact**

- On-disk formats: new captures carry header version 3. Every v2 (and
  v1) file stays readable; both new fields decode as 0 from v2 ("venue
  time unknown, never stale") and as garbage from v1 — consumers gate on
  `has_venue_time`.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Tick` table + replay-log header
  version row. Rings unchanged (same 64 B `Tick`); raw tap unchanged.

**Migration steps**

1. Nothing for existing captures.
2. Tooling that hard-codes `version == 2` (none in-tree after this
   entry; research one-shots read `≤ 3`) must accept 3.
3. The release binary is relinked on the operator's next authorized
   boot (G0 law) — until then the standing engine keeps writing v2.

**Rollback**

- Revert the commit; v3 files written meanwhile are refused by a v2
  reader (`UnsupportedVersion(3)`) — keep them or drop the run dirs.

## 2026-08-30 — VM2 V6: worker seed lane, depth digest, coverage audit, channel map, per-host candle budgets

**What changed**

- `claude_worker.pmlr` learned the kind-7 `DepthTopK` decode
  (`DepthReader`, 192-byte stride — the FIRST kind-determined slot
  size; the 64-B `Reader` keeps refusing kind 7 by design) and the
  kind-3 `Order` decode (`Reader.order/orders`, engine-orders.pmlr).
- `claude_worker.frames`: `KIND_FUNDING_SEED = 10`,
  `KIND_POSITION_SEED = 11` (core-types AiCmdKind mirror).
- NEW module `claude_worker.seeds` — the D-1/D-2 seed lane: kind-10
  frames from the candles.db `funding` table (RAW venue prints ×1e9;
  the ENGINE owns the deribit ÷8 law) + kind-11 restores
  reconstructed from the previous run's engine-orders.pmlr (slot-5
  FIFO fold, (sym,ref)-unique-row ambiguity law, sym RE-RESOLVED
  through the CURRENT manifest, qty = age seconds, ttl 0).
- NEW module `claude_worker.depth_digest` — hourly
  imbalance/spread/near-notional stats per (venue, descriptor) into
  the NEW `depth_digest` table beside candles (iv_digest pattern;
  STALE and empty-side snapshots skipped + counted).
- NEW module `claude_worker.coverage_audit` — per-class expected-vs-
  present audit of candles/funding/iv/depth over the newest manifest.
- NEW module `claude_worker.channel_map` — generated per-instrument
  channel TSV; `caps_of_descriptor` python mirror pinned CROSS-
  LANGUAGE against the new Rust `caps_of_descriptor_law` test.
- `claude_worker.candles.run_cycle`: REST budgets are now per HOST
  (`budget_key`; only the two bybit categories pool) and DEMAND-SIZED
  `max(env floor, 2 × tfs × targets)` per host.
- `scripts/candles-cycle.sh` runs `claude_worker.depth_digest` after
  the IV digest (same serialized window; exec bit preserved).

**Why**

- V6 of docs/vm2-plan.md: warm VM restarts (funding windows + open
  positions survive the daily restart without crons) and D-8 research
  reach (the agent sees depth, knows every instrument's channels, and
  the audit names data holes instead of leaving them silent).
- The budget change fixes a LIVE starvation found by the new audit
  (2026-08-30): binance spot and usdm shared one per-venue 30-call
  budget although they hit different hosts — the 22-target usdm lane
  got ZERO pages every cycle and 12 M5/BST symbols had no candles at
  all.

**Impact**

- On-disk formats: NEW `depth_digest` table in candles.db (additive);
  no existing table/file changes. `~/multivenue/worker/channel-map.tsv`
  is a new generated file.
- Config keys: none new (depth digest honors
  `CLAUDE_WORKER_DEPTH_DIGEST_WINDOW_H`, default 26). The meaning of
  `CLAUDE_WORKER_CANDLES_BUDGET_PER_H` narrows from per-venue pool to
  PER-HOST FLOOR.
- Wire formats: none (kinds 10/11 landed in V1; this is the worker
  mirror).

**Migration steps**

1. Nothing mandatory — tables and modules are additive; the next
   hourly candles cycle backfills the starved usdm symbols under the
   new budgets and starts writing depth digests.
2. Optional one-shots: `python -m claude_worker.depth_digest
   --backfill` (done 2026-08-30, 108 buckets), `python -m
   claude_worker.channel_map`.

**Rollback**

- Revert the commit; drop the `depth_digest` table if desired
  (nothing reads it engine-side). Seeds are push-only and the engine
  refuses/expires malformed ones by design.

## 2026-08-30 — VM2 V5: multi-channel backtest, warmup, D-7 options mark-fill law, D-3 report/gate amendment

**What changed**

- The backtest merge carries funding/ctx `ChannelEvent`s, `DepthTopK`
  and `OptSummary` records beside ticks (same §3.2/§3.3 total order
  and VIRT rebase; lane ordinals extend the lord space), replayed
  through the vm's REAL callbacks — every §1.1 feature evaluates in
  replay exactly as live (§1.5).
- Per-run sym REBIND (the §6 law's replay half): each run's manifest
  joins by DESCRIPTOR to the newest run's, and every record's sym is
  rewritten at load — options ordinals that reshuffle across boots
  evaluate as ONE instrument.
- WARMUP (§1.5, refined — recorded in vm2-plan §8): the longest
  window the TABLE references (Roll windows; Apr24 ⇒ 24 h; Apr72 ⇒
  72 h), 0 when none — features fill, no entries, split math
  unchanged, `warmup_end_virt_ns` reported.
- D-7 options mark-fill law in `backtest::fill` (shared by
  audit-pnl): mark-bearing OptSummary records synthesize zero-spread
  mark ticks for option syms without a tick lane; registered syms
  execute IMMEDIATELY at `mark ± max(0.5%, 1 tick)` with TAKER fees
  and value at mark; `mark_fills` counts and the assumption is
  PRINTED wherever it shaped numbers. okx's markless summaries stay
  feature-only (honestly unpriceable).
- D-3: schema-1 gains ADDITIVE keys — `oos.round_trips`, `oos.legs`,
  top-level `position_rows` (goldens updated; schema_version stays
  1). The worker gate counts LEGS toward `min_trades` and folds the
  position-ruleset floor `round_trips >= MIN_ROUND_TRIPS (10)` into
  the same verdict — GateThresholds/GateResult keep their frozen
  shapes; pre-V5 reports gate byte-identically (pinned by
  `tests/test_backtest_d3.py`, ruling cited).

**Why**

- vm2-plan §4-V5: funding/IV/depth strategies must be honestly
  backtestable through the frozen argv before V7's real backtests.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: schema-1
  additive keys documented in the harness goldens; worker report
  gains mirrored additive keys.

**Migration steps**

1. None — pre-V5 captures and reports replay/gate unchanged.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V4: validator v2 (descriptor resolution, rules 9–10), the §6 handoff flips to v2, v1 `RuleTable` retired

**What changed**

- The §4.2 validator grew the v2 grammar arm (docs/wire-format.md
  "Ruleset JSON grammar v2"): descriptor-addressed rows (D-6 —
  stage-time resolution against the bin's DescriptorTable, built
  from the SAME allocation truth as `instrument-manifest.tsv`;
  unresolvable ⇒ the new `Descriptor` reject), rule 9 (`Position`)
  and rule 10 (`Feature`: channel capabilities + window law + the
  rolling-bind budget), signed 9-decimal thresholds, and the
  KEYWORD_CAP 16→24 growth. v1 rows keep validating byte-exactly
  (the compat arm builds them THROUGH `RuleRowV2::from_v1`); both
  shapes may share one artifact.
- The §6 table-handoff ring is v2-typed (`RuleTableSlot =
  RuleTableV2`, 32 832 B slots; `Strategy::on_ruleset_table` takes
  `&RuleTableV2`); the v1 `RuleTable` struct retired (`RuleRow`
  stays as the v1-grammar record through the compat window).
- The backtest harness resolves v2 descriptors against the NEWEST
  run's `instrument-manifest.tsv` (offline capability law =
  `caps_of_descriptor`, deliberately permissive where the string
  under-determines — wrong grants only yield absent-data-holds);
  manifest-less (pre-D3) captures refuse v2 rows honestly.
- Fuzz: `ruleset_json` covers both arms (fixture descriptor table
  wired into the target; corpus seeded with v2/mixed artifacts).
  Bench gate 34 validates 255 v1 + 1 v2 rows with live resolution
  inside the measured window.

**Why**

- vm2-plan §4-V4/D-6: artifacts must be portable across restarts and
  universe edits, and refusals must be loud and specific before the
  agent authors against the grammar.

**Impact**

- On-disk formats: none (artifacts stay JSON; identity unchanged).
- Config keys: none.
- Wire formats: ruleset JSON grammar v2 documented; v1 table section
  retired in docs/wire-format.md.

**Migration steps**

1. None for operators: existing v1 artifacts (raw syms) stage and
   commit unchanged through the compat arm — one release, per D-6.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V3: the v2 grammar evaluator + position layer live in strategy-vm

**What changed**

- `VmStrategy` evaluates the GENERAL v2 grammar (vm2-plan §1.2–§1.3)
  over the V2 feature engine: two-operand signals, confirm gates,
  the position state machine (Flat→Entered→Flat), group exclusivity,
  two-leg emits, min/max-hold, the universal exit law
  `signal × entry_sign ≤ exit_1e9`, and `PositionSeed` (D-2)
  restore. v1 `RuleTable`s arriving through the UNCHANGED trait seam
  map row-for-row onto v2 sugar rows (`RuleRowV2::from_v1` — the
  byte-exact v1 semantics law); the §6 handoff ring stays v1-typed
  until V4 flips the validator.
- SEMANTIC DELTA (deliberate, vm2-plan §1.2): rows now evaluate when
  EITHER leg's sym ticks (two-legged signal freshness) — v1
  evaluated on action-sym ticks only. Fires move to the FIRST tick
  that satisfies them (fresher data); condition/emit laws unchanged.
  The golden harness pins the new eval counts.
- The vm's book generic retired (`VmStrategy<N>` → `VmStrategy`) —
  mids live in the feature engine (`FEAT_SYM_SLOTS` grew 1024→4096,
  absorbing the old `BACKTEST_VM_SLOTS` law); `SET_VM_SLOTS` retired
  with it.
- Sizing law hardened (caps-proptest catch): a qty whose NOTIONAL
  floors to zero is clamped away (the §11 zero-notional invariant
  now lives in `sized_qty_1e6` itself).

**Why**

- vm2-plan §4-V3: the general grammar must execute engine-side
  before the validator (V4) can accept it from artifacts.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: none (the
  evaluator is in-process; V1's formats stand).

**Migration steps**

1. None — v1 artifacts behave identically through the sugar mapping
   (two-legged evaluation moves WHEN a fire lands, never whether).

**Rollback**

- Revert the commit.

## 2026-08-29 — VM2 V2: feature engine, OptSummary engine lanes, Deribit Funding `v1` = funding_8h, HL event lane

**What changed**

- `strategy-vm` gains the feature engine (`strategy_vm::features`,
  vm2-plan §1.1): ONE boxed ~12 MiB zeroed-at-boot state holding
  per-sym latest values, per-(sym, window) rolling minute rings
  (lazy-recompute stats), funding-print rings with the per-venue
  settled-print laws, mark/IV and depth-derived features, and the
  venue-derived wall-clock offset. Fed exclusively through the vm's
  `Strategy` callbacks — the backtest replays the same records
  through the same code (§1.5 parity). Zero alloc after boot
  (release gate 39, `vm_feature_engine_paths_are_zero_alloc`).
- OptSummary (kind 6) enters the engine for the first time:
  `Strategy::on_opt_summary` (defaulted), three opt lanes
  (`OPT_RING_SIZE` 4096; `engine::opt_lane_of` okx/deribit/bn — the
  BN lane venue-dark until the eapi heal), pushes at every venue
  emit site AFTER capture (§6.5 law), `opt_ring_drops_total` per
  ingress. Capture files unchanged.
- Deribit Funding events: `v1` now carries `funding_8h` ×1e9 (was a
  constant 0) — additive; the parser gained the optional field
  (ticker scratch frame grew to 128 B, in-process only). Pre-V2
  captures replay via the `current_funding` (`v0`) fallback.
- Hyperliquid gained its venue-event lane (it had none): funding
  rides AssetCtx rows, spawn mask
  `EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX`.
- `AiCmdKind::FundingSeed` is consumed: the vm folds seeds into the
  same funding windows live events feed (dedup within half the
  venue print period).

**Why**

- vm2-plan §4-V2: every §1.1 feature must evaluate engine-side (and
  identically in replay) before the V3 grammar evaluator lands.

**Impact**

- On-disk formats: none (no capture layout changed; deribit Funding
  `v1` is a value-semantics addition inside an existing field).
- Config keys: none.
- Wire formats: docs/wire-format.md — OptSummary lane note, Funding
  `v1` note, HL AssetCtx lane note.

**Migration steps**

1. None — replay of old captures works (fallbacks documented).

**Rollback**

- Revert the commit; no persisted state depends on the new lanes.

## 2026-08-29 — VM2 V1: RuleRowV2/RuleTableV2 (table version 2), AiCmd kinds 10–11 (vm2-plan D-1…D-8)

**What changed**

- `core-types` gains the VM2 general-grammar types (vm2-plan §1/§3,
  design LOCKED 2026-08-29): `RuleRowV2` (128 B, two cache lines —
  feature/combine grammar, position mode, groups, confirm, min/max
  hold) and `RuleTableV2` (256 rows, 32 832 B), ADDITIVE beside the
  v1 types. The vm evaluator flips to v2 in V3, the validator + the
  §6 table-handoff ring in V4; the v1 `RuleRow`/`RuleTable` retire
  then (no unused code stays). v1 JSON artifacts keep committing
  through a compat arm for one release (D-6) — sugar maps onto v2
  rows with byte-exact v1 semantics, so H6-era artifacts and
  `cvfc-basis-kill` stay valid.
- `AiCmdKind` appends `FundingSeed = 10` (D-1: one historical funding
  print — sym, rate ×1e9 in `px`, venue print ms in `qty`) and
  `PositionSeed = 11` (D-2 as ruled: positions RESTORE at boot — row
  index in `param_id`, entered side, entry px in `px`, position age
  SECONDS in `qty`, `ttl_ns` 0-enforced: the drain site expires any
  nonzero ttl, and entry qty re-derives from the row's sizing law so
  restores respect current caps). Shape rules enforced by
  `AiCmd::validate_shape`; byte
  meanings pinned in `docs/wire-format.md`. Both are engine-directed
  (`venue = Ai`, `strategy_id = 5`). Capture-compatible: the 64 B
  AiCmd layout is unchanged, `ai-cmds.pmlr` readers see two new kind
  bytes.
- The funding cadence law gets its single home:
  `core_types::funding_print_divisor` (Deribit ÷8 — hourly samples of
  `interest_8h`) + `funding_period_s` (clock-feature fallback). The
  worker mirror (`claude_worker.carry_signal.apr_from_prints`) gains
  a pin test in V6.
- Refinement over the D-1 sketch (recorded in vm2-plan §8):
  FundingSeed carries RAW PRINTS with venue timestamps, not
  per-window aggregates — windows recompute engine-side through the
  same path live events take, keeping the cadence law in one place.

**Why**

- vm2-plan §0: the two-word v1 rule language cannot express the M5
  strategy families; v2 is the general, cron-free replacement. D-5
  ruled 128 B rows (the grammar does not fit 64 B).

**Impact**

- On-disk formats: none yet (RuleRow/Table never captured; AiCmd
  layout unchanged — only new kind bytes appear in `ai-cmds.pmlr`
  once V6 pushes seeds).
- Config keys: none.
- Wire formats: AiCmd kind table extended; RuleRowV2/RuleTableV2
  documented in `docs/wire-format.md`.

**Migration steps**

1. None until V3/V4 flip the evaluator/validator — this commit is
   type-additive and inert at runtime.

**Rollback**

- Revert the commit; no persisted state references the new types or
  kinds until V6.

## 2026-08-29 — SlotKind 7 = DepthTopK, first non-64-byte PMLR slot (WS10-B)

**What changed**

- PMLR slot size is now KIND-determined: kinds 0–6 keep 64 B; the new
  kind 7 (`DepthTopK`, 192 B = three cache lines) carries the WS10-B
  top-K depth snapshots in `<venue>-depth.pmlr` (opened for EVERY
  venue by the uniform-file-set law; header-only without a depth
  subscription). `PmlrReader` decodes the kind from the header FIRST
  and validates the caller's type/stride against it. Container
  version stays 2 — no pre-WS10 file changed shape.
- The engine gains two depth lanes (`Ring<DepthTopK, 4096>`, OKX +
  Deribit, `engine::depth_lane_of`) and the defaulted
  `Strategy::on_depth`; emission is change-gated in the ingress
  (`book_builder::ladder`, 64 levels/side); a seq-chain break emits a
  `flags = STALE` snapshot after clearing the ladder.
- WS10-A (same commit series, no wire change): venue-event lanes
  carry funding `ChannelEvent`s in-process (`EVENT_RING_SIZE` 1024,
  spawn-time `event_mask`, funding-only in v1) — the capture record
  IS the carrier, so nothing here migrates.

**Why**

- gaps-doc §1 / ws10-engine-plumbing-design.md, operator-approved
  D-A1..D-B3: funding and L2 depth reach `Strategy` without a second
  wire type; the 192 B slot keeps the top-5-per-side snapshot in one
  POD instead of splitting rows across 64 B records.

**Impact**

- On-disk: new `<venue>-depth.pmlr` files appear in every run dir
  from the first WS10 boot. Readers that assumed a flat 64 B stride
  must consult `SlotKind::slot_size` (in-tree readers updated;
  `claude-worker/pmlr.py` opens only tick/opt-summary files and is
  unaffected).
- audit-replay renders a per-venue `depth` stream section + totals
  (snapshots / syms / stale count) when records exist.

**Migration steps**

1. None for existing files. New binaries read old runs unchanged.

**Rollback**

- Pre-WS10 binaries ignore unknown kind 7 files (open fails with
  `UnknownSlotKind`; nothing else reads them).

## 2026-08-29 — VenueId 6 = Bybit + tick lane 6 (WS9, the sixth venue)

**What changed**

- `VenueId` gains `Bybit = 6` (append-only; `Ai = 5` keeps its
  discriminant — the lane↔venue identity is broken past lane 4 and
  `engine::tick_lane_of` is the mapping: Bybit rides TICK LANE 5).
- `NUM_TICK_LANES` 5 → 6; `TRADEABLE_VENUES` 5 → 6 with the new
  `tradeable_venue_byte` predicate (bytes 0..=4 and 6; Ai excluded);
  `ModelParams` tables widen to 7 slots (slot 5 = Ai, DEAD).
- New capture label `bybit` (`bybit-ticks/-events/...pmlr`),
  `bybit:` / `bybit-linear:` descriptor namespaces, `[bybit]`
  universe section (spot/linear; linear ordinal base 512), Config
  hosts `BYBIT_WS_HOST`/`BYBIT_REST_HOST`, metrics family
  `engine_ingress_bybit_*` + coverage gauge, TUI health bit 7.
- Worker: `VENUE_BYBIT = 6`, map seeding for `[bybit]`, candle lanes
  `bybit`/`bybit-linear` (kline REST), refdata tickers lane.

**Why**

- stage2-finish-plan WS9 / gaps-doc §1 — the sixth venue.

**Impact**

- On-disk formats: new per-venue capture files under the existing
  container version; ticks may carry venue byte 6. Pre-WS9
  `audit-replay` binaries skip the unknown label's files and treat
  venue byte 6 as corruption — decode with a post-WS9 binary.
- Config keys: `[bybit] spot/linear` (additive); two new optional
  env hosts.
- Wire formats: append-only enum growth on `VenueId`.

**Migration steps**

1. None until a `[bybit]` section is configured; the venue is
   entirely opt-in.

**Rollback**

- Safe while `[bybit]` stays empty (old binaries reject the section
  as an unknown-section parse error — remove it before rolling
  back).

## 2026-08-29 — ChannelId 12 = VolIndex (WS6 Deribit DVOL)

**What changed**

- `ChannelId` gains `VolIndex = 12` (append-only): the Deribit DVOL
  capture series. Field semantics in `docs/wire-format.md`: `sym` =
  `SYMBOL_ID_NONE` (venue-global), `v0` = volatility points ×1e9,
  `v1` = ordinal into the configured `[deribit] options_underlyings`
  list (DVOL subscriptions derive from that list — `BTC` →
  `btc_usd`; no new config key).
- DVOL channels ride the batched subscribe but sit OUTSIDE the
  subscribe-verification mask (its u128 is fully allocated) — an
  index the venue does not serve is a missing capture series, never
  a session verdict.

**Why**

- stage2-finish-plan WS6 / gaps-doc §1 "New data series": DVOL was
  absent from the repo entirely.

**Impact**

- On-disk formats: event logs may carry `channel = 12` rows. PMLR
  container version unchanged.
- Config keys: none (derived from options underlyings).
- Wire formats: pre-WS6 readers skip id 12 as a corrupt byte —
  additive, same posture as id 11.

**Migration steps**

1. None — decode with a post-WS6 binary.

**Rollback**

- Binary rollback safe: old readers skip id 12; old writers never
  emit it.

## 2026-08-29 — ChannelId 11 = SubDrop (WS2 non-fatal subscribe drops)

**What changed**

- `ChannelId` gains `SubDrop = 11` (append-only; nothing renumbered):
  the §6.6-paired evidence event for WS2's non-fatal subscribe drops
  on OKX/Deribit reconnect sessions. Field semantics in
  `docs/wire-format.md` (event-slot `channel` row): `sym` = dropped
  instrument or `SYMBOL_ID_NONE`, `v0` = venue error code (0 =
  missing-from-echo), `v1` = venue-local channel discriminant (−1 =
  unknown/folded).
- `IngressStatus` gains `sub_drops_total` (slot stays 128 B; mirrored
  as `engine_ingress_<venue>_sub_drops_total`).

**Why**

- Capture-continuity outage 2026-08-27 §5.2: venue errors / missing
  echo channels on reconnect killed whole sessions for six days.
  WS2 makes them per-instrument drops; every drop must stay visible
  offline (counter ↔ event pairing, the TradeGap/BookGap precedent).

**Impact**

- On-disk formats: event logs written by post-WS2 binaries may carry
  `channel = 11` rows. PMLR container version unchanged.
- Config keys: none.
- Wire formats: pre-WS2 readers (`ChannelId::from_u8`) treat 11 as a
  corrupt byte and skip the row — old `audit-replay` binaries
  under-report only the new event class, nothing else.

**Migration steps**

1. None operationally — the id is additive. Decode with a post-WS2
   binary; the worker's `pmlr.py` channel map picks the id up in its
   WS11 fold-in.

**Rollback**

- Binary rollback is safe: old readers skip id 11; old writers never
  emit it.

## 2026-08-23 — Order slot: `strategy_id` at offset 41 + the `engine-orders.pmlr` intent log (M4.1)

**What changed**

- `Order` (64 B ring/capture slot) claims ONE byte of `_pad1`:
  offset 41 = `strategy_id: u8` (strategy-set slot ids; `0xFF` =
  unattributed), `_pad1` shrinks 15 → 14 B. Stamped by the set's
  `StampCtx` adapter around every member callback; bare
  single-strategy boots leave `0xFF`.
- NEW engine-side capture file `engine-orders.pmlr`
  (`slot_kind = 3`, `core_io::SlotCapture<Order>`): every order the
  dispatcher ACCEPTED via `ctx.submit`, staged on the engine thread
  next to `engine-fills.pmlr`. Dispatcher refusals remain counters
  only.

**Why**

- mvp-plan §4-M4 (shadow-P&L): the M4.1 audit found paper mode
  captured NEITHER intents nor fills (`PaperDispatcher` counts and
  drops; `SlotKind::Order` was defined but wired to nothing) — the
  per-strategy intent log is the enabling substrate for `audit-pnl`
  (§9.9 "logged intents"). Per-ruleset attribution deliberately
  rides the existing ai-cmds `RulesetCommit` timeline (hash128 in
  px/qty), NOT a wider Order slot.

**Impact**

- On-disk formats: a NEW per-run file; PMLR container version stays
  2 (append-only kind usage). No historical Order file exists —
  the layout amendment is PRE-FIRST-CAPTURE and has zero
  reader-compat surface. Old binaries reading a new run dir simply
  never open the file; `audit-replay`/catalog treat it as another
  size-visible capture file.
- Config keys: none.
- Wire formats: `Order` table amended in `docs/wire-format.md`
  (offset 41). In-process ring consumers are unaffected (field was
  explicit zero padding; `Order::new` initializes `0xFF`).

**Migration steps**

1. Nothing operator-side: the file appears on the first boot of a
   binary carrying M4.1; older run dirs simply lack it (audit-pnl
   reports them intent-less).

**Rollback**

- Revert the commit; run dirs written meanwhile carry an extra
  `.pmlr` file old code never opens. Harmless.

## 2026-08-22 — SlotKind 6 (OptSummary): the options analytics capture channel (M2.3)

**What changed**

- New PMLR `slot_kind = 6` — `OptSummary` (64 B, layout pinned in
  `docs/wire-format.md`): mark px / mark IV / BS greeks / open
  interest / underlying px per option instrument, fed by Deribit
  option `ticker.{instr}.100ms` and OKX `opt-summary` (BN eapi at
  M2.4).
- `core_io::PmlrCapture` opens a FOURTH per-venue file,
  `<venue>-opt-summary.pmlr`, for EVERY venue (header-only where no
  options lane exists — the same uniform-file-set law as
  `<venue>-signals.pmlr`).
- PMLR **version stays 2** — this is an append-only SlotKind addition;
  no existing slot layout changed.

**Why**

- mvp-plan §4-M2.3/§9.8: one new capture record on the append-only
  raw-store doctrine; the strategist digest and audit read it offline.

**Impact**

- On-disk formats: run dirs gain `<venue>-opt-summary.pmlr` per venue.
  READER COMPAT: pre-M2.3 readers never open the new file (separate
  name) and are unaffected; `SlotKind::from_u8(6)` decodes only in
  M2.3+ binaries — an OLD binary reading a NEW file's header reports
  unknown-kind corruption, which is correct-and-loud, and never
  happens through the shipped tools (they open files by name/kind).
  Old run dirs (no opt-summary files) audit exactly as before —
  audit-replay treats the file as absent.
- Config keys: none (the options lanes were M2.1/M2.2 config).
- Wire formats: new `OptSummary` section in `docs/wire-format.md`;
  Deribit option rows now subscribe `ticker` in addition to `quote`
  (subscribe-verification folds both into one per-row bit); OKX
  gains the family-keyed `opt-summary` subscription (2 args).

**Migration steps**

1. None — capture stays append-only; new files appear on the first
   M2.3 boot.

**Rollback**

- Boot the previous binary: new files stop being written; existing
  ones remain readable by M2.3+ tools and ignorable garbage-by-name
  to older tools.

## 2026-08-15 — SlotKind 5 (ChannelEvent), capture files, raw tap (Phase 8e)

**What changed**

- New PMLR `slot_kind = 5` — `ChannelEvent` (64 B, layout in
  `docs/wire-format.md`): non-tick channel capture. `slot_kind = 4`
  is **reserved** for Stage-2 `AiCmd` (plan §8.4) and still decodes as
  invalid.
- PMLR replay capture is now actually wired into the shipped `run`
  path (the 8e defect fix): each ingress thread writes
  `<venue>-ticks.pmlr` / `<venue>-events.pmlr` / `<venue>-signals.pmlr`
  into `<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`.
- New sidecar format `<venue>-raw.tap` (`b"PMRT"` v1) — bounded raw
  payload capture behind `--raw-tap`, off in production.
- `MULTIVENUE_LOG_DIR` now tilde-expands a leading `~/` at boot.

**Why**

- Plan §6.5: the replay logs are the 8h backtest dataset and the
  `audit-replay` input; §6.6 G1 soaks are judged from them.

**Impact**

- On-disk formats: existing v2 Tick/Signal/Fill/Order logs unchanged
  and fully readable. Binaries at or before 8d.1 refuse `slot_kind=5`
  files (unknown kind = corruption by their rules) — expected.
- Config keys: `MULTIVENUE_LOG_DIR` semantics extended (tilde
  expansion); no new keys for capture itself. Tap is flag-driven.
- Wire formats: ring slots untouched — `ChannelEvent` exists only in
  PMLR files, never in rings.

**Migration steps**

1. Nothing for existing logs.
2. Tooling that globs `*.pmlr` should route on the header `slot_kind`
   byte (0/1/2/3/5) rather than assuming Tick.

**Rollback**

- Delete `run-<epoch_ns>/` capture directories; pre-8e binaries never
  read them.

## 2026-08-14 — PMLR v2: venue bytes + explicit padding (Phase 8a)

**What changed**

- `PMLR VERSION` bumped 1 → 2.
- `Tick` gains `venue: u8` at offset 48 (was implicit padding);
  `Order` gains `venue: u8` at offset 40 (was `_pad1[0]`). Values are
  `VenueId`: Polymarket=0, Binance=1, Okx=2, Deribit=3, Hyperliquid=4,
  Ai=5.
- All padding in all four slots is now explicit and zeroed. v1 writers
  emitted 8 undefined tail-padding bytes per slot (D9).
- `SymbolId` is venue-namespaced: bits 31..24 = venue, bits 23..0 =
  per-venue ordinal.

**Why**

- Phase 8 multivenue expansion needs venue identity on every tick and
  order; the `AsBytes` zero-copy log contract requires fully
  initialized slots.

**Impact**

- On-disk formats: v1 logs remain readable (`PmlrReader` accepts
  version ≤ 2, exposes `version()`). v1 files are **venue-less**: the
  byte at Tick offset 48 / Order offset 40 is undefined garbage in v1
  and must be ignored when `version() == 1`. Venue cannot be inferred
  from v1 slots (slot kind + sym do not disambiguate Polymarket vs
  Binance ticks).
- Config keys: none in this entry (per-venue symbol flags land with
  the venue ingress phases).
- Wire formats: ring slots and PMLR slots share the new layout;
  `docs/wire-format.md` is the byte-level source of truth.

**Migration steps**

1. Nothing to do for live capture — new logs are v2 automatically.
2. Backtests mixing v1 + v2 logs must branch on `PmlrReader::version()`
   and treat v1 venue bytes as absent.

**Rollback**

- Binaries at or before Phase 7 refuse v2 logs (`version > 1`); keep
  v1 archives if a rollback below Phase 8a is contemplated.

## 2026-04-19 — Phase 0 scaffold initial wire format

**What changed**

- Introduced the Phase 0 wire format documented in `docs/wire-format.md`:
  `Tick`, `Signal`, `Fill`, and `Order` are all 64-byte cache-aligned POD
  structs. Replay-log header version pinned at `1`.

**Why**

- First commit — establishes the baseline that every subsequent migration
  will bump against.

**Impact**

- On-disk formats: replay log header magic `b"PMLR"`, version `1`.
- Config keys: see `config.example.toml`.
- Wire formats: as documented in `docs/wire-format.md`.

**Migration steps**

1. None — fresh install.

**Rollback**

- Remove `~/multivenue/replay/` and `~/multivenue/artifacts/`.
