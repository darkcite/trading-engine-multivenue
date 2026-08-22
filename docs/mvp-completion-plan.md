# MVP Completion Plan — Stage 2.5 (pre–Stage 3)

Status: PROPOSED (operator review pending) · Authored 2026-08-22 at H6a close
· Authority: operator direction ("plan what we need to finish this MVP
before going to stage 3") + four scoping rulings taken live (decision log,
§8). Stage-3 boundary: **nothing in this plan executes an order anywhere.**
The executor / risk / dispatcher / live-ramp lane (8i+) stays untouched
until the operator's explicit go after the MVP sign-off (§7). The Stage-2
completion notification duty (CLAUDE.md hard requirement) still fires at
8h/H6b close — which is phase M0 of this plan.

---

## 1. The target MVP, formalized

One always-on local paper engine that:

1. **Ingests the full universe** — Binance (multi-symbol spot + USDS-M
   futures + options), OKX (spot + swaps + options), Deribit (perpetuals +
   options), Hyperliquid (perps + spot pairs + HIP-4 outcome markets),
   Polymarket (N markets per boot, YES/NO pairs) — into the single tick
   lane.
2. **Owns a persistent, reusable market-data store** — initialized from
   nothing on first run, appended forever by the live streams; coverage
   queryable; consumed by replay, backtest, features, and the research
   loop. (The init-if-empty + additive-refresh semantics already proven for
   market-map/features/state.db extend to capture continuity + cataloging.)
3. **Runs the research loop over that store — SEMI-MANUAL is the
   Stage-2.5 operating mode** (ruling #5: no `ANTHROPIC_API_KEY` until
   Stage 3): Claude-in-session as the Fable-5 strategist through the
   ai-session §4 verbs — fetch → author → REAL backtest → gates → verb
   promotion → manual walk-forward checks. The autonomous `serve` lane
   (§8.1 auto-promotion + §8.3 monitor) stays code-complete and
   E2E-proven but PARKED; its live proof is the Stage-3 entry gate (§7).
4. **Logs every strategy-generated order intent instead of executing it**,
   with per-strategy attribution, and provides an offline analyzer that
   reports the hypothetical P&L those orders would have generated (modeled
   fills via the §4 backtest fill model, beside the engine's paper-fill
   view).

---

## 2. What is DONE today (inventory, with evidence)

- **Six ingress lanes, live-proven.** polymarket / binance / okx / deribit /
  hyperliquid / rpc crates; all five market venues ticked in the G1-soak
  capture (run[2]: okx 365,779 · deribit 281,579 · hl 221,030 ticks) and
  PM+BN ran live tonight (H6a). Per-venue boot REST discovery (8e) with
  coverage audit. HIP-4: `#<enc>` outcome coins are ordinary HL items;
  `hip4_pairs` netting exists in the worker map + positions.
- **Capture + integrity.** PMLR v2 per-run capture (per-venue ticks /
  events / signals + engine-fills + ai-cmds + raw tap), `audit-replay`
  integrity rendering (seq regressions / holes / chain breaks — all zero on
  tonight's 99.5k-tick run), wire format documented (docs/wire-format.md),
  migration log exists.
- **Replay/backtest substrate, REAL.** Multi-run k-way merge, VIRT_T0
  rebase, capture-derived universe, strict-cross maker fill model + fees +
  latency + fixed-point accounting, frozen §5.1 gates, schema-1 stdout,
  determinism by construction. Exercised live tonight: refused a real
  ruleset (exit 3), passed a legitimate one (65 OOS trades, 2 days).
- **Worker data plane.** `fetch`: capture-derived features (per-sym OHLCV)
  + four keyless REST consumers (PM Gamma live-proven tonight; OKX/Deribit/
  HL candles MockTransport-proven) under RestBudget; market-map
  **bootstrap-if-empty + additive refresh + conflict reporting** —
  live-proven tonight (added=1 → seeded → added=2, conflicts=0).
- **Research loop, code-complete.** Strategist (prompt cache, strict output
  contract, budget ledger, candidates archive), §8.1 auto-promotion through
  the frozen stage/commit pair with attribution, §8.3 walk-forward monitor +
  §8.5 rollback (disable-5 → restage-prior, no-prior arm, dark guard) —
  1081 nextest + 354 pytest green, E2E-tested end to end. Live proof of the
  auto lane + rollback = H6b (owed).
- **Semi-manual lane, live-proven.** Tonight: Fable-5-authored ruleset
  gates-passed, installed, staged (seq=16), committed (seq=18) onto the
  live engine — vm_rows_active 2, table_epoch 1, vm_fires 1, zero rejects;
  audit-replay renders the frames from capture.
- **Paper engine + P&L views.** `--paper` submits nothing; per-strategy
  order counters on /metrics; `positions` renders positions / exposure /
  realized+unrealized P&L (HIP-4 netted) from capture.
- **Ops substrate.** Metrics on 127.0.0.1:9191, TUI, .env-only secrets,
  zero-alloc/single-writer discipline enforced by tests (36 alloc gates).

## 3. What is MISSING (gap → target mapping)

- **G1 Breadth.** Binance CLI exposes ONE spot symbol (crate already
  speaks spot + USDS-M futures); no options anywhere (Deribit/OKX options
  ride existing WS stacks — new discovery + subscriptions; **Binance
  options is a NEW half-ingress**: separate `eapi` REST + its own WS host);
  Polymarket is one market per boot; the boot universe is per-boot CLI
  flags — no standing universe config.
- **G2 Storage continuity.** Capture is per-boot run dirs with dark gaps
  between boots. H6a standing finding: the virtual clock preserves wall
  gaps, so today's real capture CANNOT satisfy `min_trading_days=2` — the
  research loop cannot promote on real data until multi-day coverage
  exists. No capture catalog (what's on disk, spans, per-venue coverage,
  gaps, backtestable windows), no retention policy, no supervisor.
- **G3 Loop live proof.** §8.1 auto-promotion and §8.5 rollback are
  E2E-test-proven but not yet observed live (H6b owed; needs
  `ANTHROPIC_API_KEY`).
- **G4 Shadow P&L.** Order intents are captured and counted per strategy,
  but there is no per-strategy/per-ruleset hypothetical-P&L analyzer:
  `positions` nets the whole book from paper fills; nothing replays the
  LOGGED intents through the §4 fill model to answer "what P&L would these
  strategies have generated". Per-strategy attribution on capture records
  must be verified field-by-field (wire-format check) before the analyzer.
- **G5 Options analytics fields.** The 64 B Tick is BBO-shaped — options
  BBO rides it unchanged, but mark px / mark IV / greeks / OI need the
  options-plan §3 capture channel (wire-format migration + fuzz per house
  rule).

---

## 4. The plan — phases M0…M6

Numbering deliberately avoids 8i+ (Stage 3). Every phase lands with the
full gate set green on the Mac (nextest / alloc 36+ / pytest stay-green /
fuzz), per-crate tests per house rules (every new ingress parser ships
proptest + fuzz target), and a progress-log entry.

### M0 — Close 8h (H6b-SEMI) — operator-amended
Ruling #5 makes the keyed serve cycle impossible in Stage 2/2.5, so the
§12 exit criteria are AMENDED by operator: the §8.1 auto-promotion live
proof and the §8.3 monitor-triggered rollback live proof are **deferred to
the Stage-3 entry gate** (§7); Stage 2 closes on the code-complete loop +
its full E2E proofs + the semi-manual live demonstrations (promotion:
H6a-done; rollback: this session). Session shape: gates → paper boot (HMAC
key now permanent in .env, ruling #6) → ensure the d8aea5f4… prior binds
(supersede re-stage from `~/multivenue/worker/demo-h6a/demo/` if /tmp
cleared) → author + promote a sacrificial ruleset S over capA via the
frozen verbs → the MANUAL §8.5-shaped rollback (ai-session §4 step 10):
Disable-5 observed (mask 49→17), restage+commit the prior (counters +
table_epoch), operator-act re-enable (mask →49, prior live) → audit-replay
renders the full frame sequence → phase-closing entry with the amended-§12
checklist + the Stage-2 closure statement + **the Stage-2 completion
notification (per the amended criteria)**. The in-file "H6b kickoff"
(keyed-serve version) at the end of docs/phase-8h-progress.md is
SUPERSEDED by the operator-issued H6b-SEMI prompt. **Est: 0.5 d.**

### M1 — Universe config + venue breadth (no new protocols)
- Boot **universe config file** (TOML via core-config; CLI flags remain as
  overrides): per-venue instrument lists, PM asset-id list, feature flags.
- **Binance multi-symbol spot + USDS-M futures**: N streams (one
  connection per instrument, existing crate capability), SymbolId
  allocation mirroring the flag-order convention, discovery audit.
- **Polymarket multi-market**: N asset ids per boot, symbol map for all
  (YES/NO pair ids into the map's pair machinery), fetch/market-map
  seeding for each; boot still refuses venue-blind.
- OKX/Deribit/HL lists move into the config (mechanics exist today).
- Exit: ONE boot command runs the full non-options universe; audit-replay
  shows every venue ticking; market-map resolves every observed sym;
  gates green. **Est: 3–4 d.**

### M2 — Options ingestion: Deribit → OKX → IV channel → Binance
Ruling: options from **Deribit, Binance AND OKX**. Ordered by
marginal-cost-on-existing-stacks:
1. **Deribit options** (2–3 d): discovery of the strike×expiry chain with a
   **capped universe policy** (default: nearest E expiries × K strikes
   around ATM per underlying, config-tunable — chains churn daily; boot
   re-discovery per the 8e snapshot pattern); `quote`-channel BBO into the
   existing tick lane (an option book is a book — zero wire changes).
2. **OKX options** (2–3 d): same v5 WS we already speak — discovery
   `instType=OPTION`, `bbo-tbt` subscriptions under the same capped
   universe policy.
3. **Options mark/IV capture channel** (3–4 d): the options-plan §3 field
   deltas — one new capture record (mark px, mark IV, greeks, OI,
   underlying px) fed by Deribit `ticker` + OKX `opt-summary`;
   wire-format.md + migration.md entries; new parsers arrive with proptest
   + fuzz targets (§21.3/§21.4 — non-negotiable); audit-replay gains the
   channel's coverage/cadence row.
4. **Binance options half-ingress** (4–5 d): NEW `eapi` stack (REST
   discovery + dedicated options WS host) built to the house ingress
   doctrine (mio, handwritten byte scanner, zero-alloc, single-writer,
   capture from day one, raw-tap support, proptest + fuzz); BBO first,
   mark/IV stream into the M2.3 channel.
- Live-smoke doctrine applies to every step (pitfall #11: fixtures lie,
  wires drift — each parser gets a live boot + raw-tap before "done").
- Exit: options ticks (3 venues) + mark/IV records (3 venues) in capture,
  integrity green, fuzz corpus running clean, full-universe boot includes a
  capped options chain. **Est: ~2 wk total.**

### M3 — Continuous data ops (the storage answer)
- **launchd always-on**: KeepAlive'd paper engine on the full universe;
  daily graceful restart (SIGTERM drain) → one run dir per UTC day —
  gap-free days by construction, satisfying `min_trading_days` on real
  capture (closes the H6a standing finding); `caffeinate`/power-settings
  runbook so the Mac never sleeps the engine.
- **capture-catalog** (offline CLI subcommand, audit-replay doctrine —
  allocations allowed): walks a replay root → per-run spans, per-venue
  tick coverage, UTC days, gap map, "backtestable window" report (the
  harness's own stats + the monitor's span logic, surfaced).
- **Retention policy**: age/size-based archival of run dirs (config;
  default keep-all until disk pressure), documented.
- **Init-if-empty, restated end-to-end**: first boot on an empty
  MULTIVENUE_LOG_DIR / worker dir bootstraps everything (already true for
  map/features/state.db; catalog makes the capture side visible).
- Exit: N≥3 consecutive gap-free days on disk; catalog reports them; a
  REAL-capture backtest passes the days gate. **Est: 2–3 d + calendar.**

### M4 — Shadow-P&L attribution (the "log orders, analyze later" answer)
- **Verify per-strategy attribution** on captured order/fill records
  (wire-format audit; if the strategy slot is missing on any record the
  fix rides a documented migration).
- **`audit-pnl`** (new offline CLI subcommand, audit-replay doctrine):
  replays a run root, feeds the LOGGED intents of EVERY strategy through
  the §4 strict-cross fill model (+fees, +latency) → per-strategy /
  per-ruleset-hash modeled P&L (net, trades, DD, per-day buckets), beside
  the engine paper-fill P&L for the same window (two views, one report:
  modeled vs paper). JSON + human summary, deterministic.
- **Worker surface**: `positions --by-strategy` or a thin `pnl` verb
  reading audit-pnl output; daily report file under the worker dir; the
  §7.1 digest performance seam extends from "active ruleset only" to the
  full strategy set.
- Exit: one command → per-strategy hypothetical-P&L report over any
  window; nightly report lands automatically (launchd timer). **Est: 3–4 d.**

### M5 — Research loop on the full universe, on real data — SEMI-MANUAL
- Feature/digest inputs widen to the full universe (options syms +
  futures: per-sym OHLCV everywhere + an IV summary from the M2.3
  channel); market-map completeness for every observed sym. Zero Anthropic
  spend (ruling #5) — the strategist is Claude-in-session.
- One **semi-manual promotion on REAL multi-day capture** (now possible
  post-M3) through the frozen verbs, end-to-end.
- **Manual walk-forward runbook**: the monitor's own arithmetic driven by
  hand — `backtest --split 0/100` over the trailing window vs the §8.3
  thresholds (net ≤ −$100 / dd ≥ $200), with the §4 step-10 rollback verbs
  as the action; documented as the operating procedure until serve
  unparks at Stage 3.
- Exit: a Fable-5(-in-session) ruleset promoted on real capture, trading
  paper on the full universe, walk-forward checked by runbook, shadow-P&L
  reported. **Est: 2–3 d.**

### M6 — MVP soak + sign-off
- 7-day full-universe soak: engine always-on, research loop cycling,
  nightly shadow-P&L reports, catalog clean, gates green throughout.
- Docs refresh (CLAUDE.md CURRENT STATE, architecture one-pager) + MVP
  exit checklist review with the operator.
- **Then and only then: the Stage-3 go/no-go conversation.**
  **Est: 1 d work + 7 d calendar.**

---

## 5. Order + estimate summary

| Phase | Content | Est (work) |
|---|---|---|
| M0 | 8h/H6b close: keyed serve cycle + rollback live | 0.5 d |
| M1 | Universe config; BN multi spot+futures; PM multi-market | 3–4 d |
| M2 | Options: Deribit → OKX → IV/mark channel → Binance eapi | ~10 d |
| M3 | launchd always-on; capture-catalog; retention | 2–3 d |
| M4 | Shadow-P&L: attribution audit + audit-pnl + reports | 3–4 d |
| M5 | Research loop on full universe, real-capture promotion | 2–3 d |
| M6 | 7-day soak + sign-off | 1 d + 7 d cal |

Serial worst case ≈ 22–25 working days; M3 can start in parallel with M2
(it touches no ingress code), pulling the calendar to ~4–5 weeks including
the soak. M0 is independent, first, and immediately runnable — no API key
needed anywhere in Stage 2.5 (ruling #5).

## 6. Risks + mitigations

- **Options chain churn / rate limits** (options-plan §2): capped universe
  policy (E expiries × K ATM strikes), boot re-discovery, discovery parsers
  fuzzed. Daily restart (M3) doubles as chain-roll refresh.
- **Binance eapi unknowns**: it is a genuinely new stack — budgeted as the
  largest single item; live-smoke + raw-tap early; falls back cleanly (MVP
  can sign off with Deribit+OKX options live and BN options in-flight if
  the operator rules so).
- **Wire-format migration** (M2.3, possibly M4): every change through
  docs/migration.md with reader-compat notes; new parsers = proptest +
  fuzz, no exceptions; capture stays append-only.
- **Disk growth**: full universe + options multiplies tick volume —
  catalog reports size; retention policy before the soak; raw-tap stays
  off by default.
- **Mac sleep/network flaps**: launchd KeepAlive + caffeinate runbook;
  ingress reconnect paths already exist (soak-proven for the current
  venues); flaps show as catalog gaps, not silent data loss.
- **SymbolId stability across boots** (plan §14 caveat, surfaced in H3):
  the catalog and shadow-P&L key by venue+descriptor, never by bare
  SymbolId, so config-order changes can't silently mix streams.
- **Anthropic spend: ZERO across Stage 2.5** (ruling #5 — no key until
  Stage 3). The serve/auto lane stays parked; its E2E suite keeps it green
  so it unparks at Stage 3 without drift. Budget law re-enters with the
  key at the Stage-3 entry gate.
- **Zero-alloc / single-writer invariants**: all new ingress work under the
  standing doctrine (no tokio, no serde_json, preallocated buffers,
  `#[repr(C)]`/`align(64)` where hot); alloc-gate additions only with
  progress-log justification (§11 discipline).

## 7. Explicit non-goals (the Stage-3 line)

No order submission on any venue (paper only, everywhere). No dispatcher
work beyond capture (the 8j dispatcher/signing lane untouched). No
risk-gate enforcement changes (8i untouched). No live ramp, no capital.
**No `claude-worker serve`, no Anthropic API calls** (ruling #5) — every
research action rides the ai-session §4 verbs. The moment any phase seems
to want one of these, it stops and the operator decides — that IS the
Stage-3 gate.

**The Stage-3 ENTRY GATE inherits the deferred live proofs** (M0
amendment): key provisioning → one keyed Fable-5 serve cycle with §8.1
auto-promotion observed live + one §8.3 monitor-triggered §8.5 rollback
observed live — BEFORE any executor/risk/dispatcher work begins.

## 8. Decision log (operator rulings, 2026-08-22)

1. Options scope: **Deribit + Binance + OKX all supported** (overrides the
   "Deribit-first-only" minimal slice; drives M2's four-step ladder).
2. Binance breadth: **multi-symbol spot + USDS-M futures**.
3. Polymarket breadth: **multi-market per boot**.
4. Data ops: **launchd always-on** with rotation + catalog + retention.
5. (2026-08-22, post-planning) **`ANTHROPIC_API_KEY` will NOT be
   provisioned until Stage 3 — everything runs SEMI-MANUAL** (ai-session
   §4 verbs; Claude-in-session is the strategist). Rescopes M0 → H6b-SEMI
   and M5; the §12 exit criteria are amended accordingly; the deferred
   live proofs move to the Stage-3 entry gate (§7).
6. (2026-08-22, post-planning) **`AI_INGRESS_HMAC_KEY` generated and made
   permanent in `.env`** (operator-directed append; 600 perms,
   gitignored, value never displayed) — engine dotenvy + worker
   BaseConfig read it; no per-invocation env prefixes anymore.

Open questions (answer during M1/M2 design, none blocking M0):
- Options universe policy defaults: how many expiries × strikes per
  underlying? (proposal: E=2, K=8 ATM±4 — config-tunable)
- Retention default: keep-all vs 30-day archive?
- PM market selection source: operator-picked list vs top-N Gamma
  liquidity auto-refresh?
- Shadow-P&L report cadence: nightly (proposed) vs per-run?

---

## 9. Data storage design (BINDING for M2.3, M3, M5)

Operator-reviewed 2026-08-22. Implementing sessions inherit this section
verbatim; deviations go through the operator.

### 9.1 Principle

Websocket ticks are the canonical, lossless truth; candles are DERIVED
data. Aggregation flows one way only (ticks → candles, exactly
computable; the reverse is impossible), so raw ticks are persisted
forever-ish and every candle is reproducible. None of this touches the
hot path: capture is an append-only sink on the ingress threads, candle
work is worker-side offline Python, the engine loop sees only live ticks
from the rings.

### 9.2 Websocket lane (EXISTS — M3 adds continuity only)

Every ingress thread parses venue wire bytes into the normalized 64 B
`Tick` slot (ts, sym, seq, bid px/qty, ask px/qty, venue) → engine ring;
`PmlrCapture` appends the SAME record to `<venue>-ticks.pmlr` in the run
dir (+ events / signals / engine-fills / ai-cmds files). PMLR v2 = fixed
header + fixed 64 B slots: O(1) slot-indexed access, append-only,
torn-tail tolerant, byte layout pinned in docs/wire-format.md. One run
dir per boot; M3's launchd daily graceful restart makes that one gap-free
run per UTC day, with capture-catalog + retention on top. Scale anchors
(H6a measured): BN btcusdt ~145 Hz ≈ 800 MB/day/instrument; a quiet PM
market ~0.16 Hz ≈ 1 MB/day. Consumers: backtest harness (k-way merge +
VIRT_T0), audit-replay (integrity), worker reader (features / positions /
monitor spans).

### 9.3 REST candle lane (EXISTS — deliberately ephemeral today)

Current fetchers request 1m × 60 bars (one hour) per sym per fetch
(`CANDLE_INTERVAL="1m"`, `CANDLE_WINDOW_MS=3_600_000`; Deribit
resolution "1", OKX limit 60) into per-run `<sym>-ohlcv.json` feature
files — a strategist warm-up window, NOT storage. PM: meta only.

### 9.4 `candles.db` (NEW, M3): the persistent candle store

Worker-owned SQLite (WAL; SQLite is already in the stack and this is
offline data — no new formats, no new deps). One table, PK
`(venue, descriptor, tf, open_ts)`:

- `descriptor` is the venue instrument string (e.g. `binance:btcusdt`,
  `deribit:BTC-27MAR26-100000-C`, a PM token id) — NEVER bare SymbolId
  (ids can reshuffle across boots; plan §6 / PLAN §14 caveat).
- columns: `o, h, l, c, v`, `source` ∈ `rest` | `derived` | `capture`,
  `fetched_ts`.
- Init-if-empty = create + bounded backfill (§9.6); append = the
  gap-fill upsert. The still-OPEN bar is upserted until it closes; CLOSED
  bars are immutable — a re-fetch disagreeing with a stored closed bar is
  LOGGED as a conflict (market-map conflict-report pattern), never
  silently overwritten.

### 9.5 Timeframe policy: fetch ONE base per horizon, derive the rest

- Fetched: **1m** for the rolling recent window (24–48 h per active
  sym); **1h** for medium history (30–90 d); **1d** for listing-lifetime
  history where the venue makes it cheap.
- NEVER fetched: 5m / 15m / 4h — derived EXACTLY from the finer base
  (O = first, H = max, L = min, C = last, V = sum), stored back with
  `source=derived`, computed on demand and cached.
- Rationale: RestBudget is 60 req/h/venue with per-call row caps
  (OKX 60–300); a fetched 15m bar is strictly less information than the
  fifteen 1m bars we already hold.

### 9.6 Gap-fill / backfill semantics ("initialize if empty, then append")

Each fetch cycle: `SELECT max(open_ts)` per (sym, tf) → request ONLY the
missing window, paginated under RestBudget → upsert. Empty store ⇒
bounded backfill, config-tunable defaults: 48 h @ 1m, 90 d @ 1h,
listing-lifetime @ 1d. Budget exhaustion mid-backfill is fine — the next
cycle resumes from `max(open_ts)` by construction.

### 9.7 Capture-derived candles (M3, beside the catalog)

An offline aggregator walks PMLR ticks → 1m **mid-price** OHLC +
tick-count, `source=capture`, volume NULL — we capture BBO, not prints;
volume is never fabricated (fills in later only where trade channels are
captured). Two jobs: (1) the candle source for venues without a usable
candle endpoint — Polymarket in particular (its book IS the price; the
CLOB prices-history REST is an optional later add); (2) a drift
detector — REST candles cross-checked against what our own sockets saw.

### 9.8 Options analytics records (M2.3)

Mark px / mark IV / greeks / OI arrive over websocket (Deribit `ticker`,
OKX `opt-summary`, BN eapi stream) into a NEW PMLR channel — same
append-only raw-store doctrine, parsers with proptest + fuzz (house
rule). Aggregated IV snapshots (per sym, 1m/1h) land in a table beside
`candles.db` for the strategist digest.

### 9.9 Consumer matrix (who reads what)

- Strategist digest (semi-manual or, at Stage 3, serve): `candles.db` +
  feature files.
- Shadow-P&L (`audit-pnl`, M4): PMLR ticks + logged intents + fills.
- **Backtest harness: PMLR ONLY, never candles** — the strict-cross fill
  model needs real book crossings; candle-fed fills would be fabricated.
- Engine hot path: NOTHING here — rings only, zero change.
