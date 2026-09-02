# VM v2 plan — general engine-resident strategy execution, cron-free (V0–V9)

**Status: ACTIVE — authored 2026-08-29, GENERALIZED same day on operator
order: v2 must support cron-free execution of ALL strategy types across
ALL venues and instruments in the universe — the three M5 families are
validation cases, NOT the scope. D-1…D-8 RULED 2026-08-29; §1/§3
LOCKED (§8 V0 entry = the rulings record).**
Authority position: below `docs/mvp-completion-plan.md`; executes as
**M5.5** (after the current M5 paper phase, before the M6 soak — so M6
soaks the VM-resident strategies) and does NOT touch the §7 Stage-3
entry gate. Progress appends to §8.

## §0 The gap this closes (why crons exist today)

The 8g ruleset VM already delivers the operator's target shape — hard
format, stage/commit push, sub-second tick-path execution, artifact
persistence with #7b re-commit at restart, per-hash audit-pnl — but
its rule language has exactly two words (`level_breach`,
`cross_deviation` over live prices), no memory, no position awareness,
and price is its only input despite the engine capturing five data
channels. Concretely:

| strategy class | blocked by |
|---|---|
| xv cross-venue reversion | position blindness (2026-08-29 bounds finding: refires accumulated $68k vs the $20k cap) |
| CVFC-1 / S1 funding carry & spread | no funding input, no windows, no pair groups, no min-hold/exit |
| spot↔perp basis | position blindness + no funding input (carry cost) |
| options IV level / IV spread / IV-vs-RV | OptSummary (kind 6) not a VM input; no rolling stats |
| depth imbalance / liquidity | DepthTopK (kind 7) not a VM input |
| funding-print timing | no clock feature |
| momentum / z-score | no rolling mean/std |

The carry/xv **strategy crons** exist solely to supply the first two
lines from outside. v2 does not add per-strategy triggers to close
them — it adds a GENERAL feature-and-condition grammar that closes the
whole table, so any future ruleset in these classes runs engine-side
with no new engine code. The crons retire. (The hourly **data** crons
— candles, funding-history REST, universe refresh — are data plumbing,
not strategy execution, and remain; see §2 D-4.)

Out of grammar by design (per-strategy native Rust members or Stage-3+,
see §7): >2-leg structures, path-dependent logic beyond the position
machine, portfolio-level optimization, text/news inputs.

## §1 Capabilities

### §1.1 Feature engine (the VM's inputs — all channels, all syms)

Per-sym engine-maintained state, preallocated at table commit for the
FULL boot universe (budget: ≤1024 syms × ≤256 B ≈ 256 KiB — trivial),
`#[repr(C)]` PODs, align(64) blocks, zero alloc, no drop:

- **price** — bid / ask / mid / last (v1's input, unchanged).
- **rolling stats** — mean / EMA / min / max / std of mid over a
  per-row window (u16 minutes, ≤ 4320 = 3 d), maintained as fixed
  minute-bucket rings. Raw units; no annualization in-engine (a
  threshold owns its scale).
- **funding APR** — 24 h and 72 h windows over funding prints (WS10-A
  `ChannelEvent`s via `on_venue_event`, today okx/deribit/hl/bybit
  live + seeds per D-1/D-4). The per-venue cadence law (deribit
  hourly-sampled `interest_8h` ⇒ ÷8) lives in ONE `core-types` const
  fn used by VM, harness, and (via pin tests) the worker — the R4-§9
  unit law gets a single home. Empty window = feature ABSENT: rows
  referencing it hold, never assume zero (the carry_signal law).
- **mark / IV** — from OptSummary (kind 6) for options instruments.
- **depth** — from DepthTopK (kind 7): top-K imbalance (bid−ask
  notional / total), spread_bps, near-depth notional. STALE gap law
  inherited: stale book ⇒ feature ABSENT.
- **clock** — seconds to the sym's next funding print
  (venue-cadence aware) and UTC seconds-of-day. Enables pre/post-print
  and session-timing strategies.

### §1.2 General condition rows (grammar, not named triggers)

A v2 row is a two-operand comparison, not a hardcoded trigger:

```
signal = combine( feat_a(sym, win_a), feat_b(ref | CONST, win_b) )
combine ∈ { DIFF, DIFF_BPS, RATIO_1E9, LHS_ONLY }
entry:   signal ⋛ enter_1e9   (cmp per side/flags, abs-mode flag)
exit:    signal ⋛ exit_1e9    (evaluated on the HELD position)
confirm: optional second condition (feat_c/win_c ⋛ confirm_1e9)
```

This one shape expresses every class in the §0 table: level breach
(mid vs CONST), cross-deviation (mid DIFF_BPS mid), funding spread
(apr24 DIFF apr24, confirm on apr72), basis (perp-mid DIFF_BPS
spot-mid, confirm on funding), IV spread (iv DIFF iv), IV-vs-RV (iv
DIFF rolling-std), imbalance (imb vs CONST), z-score/momentum (mid
DIFF rolling-mean vs std-scaled threshold via confirm), print timing
(clock vs CONST as confirm). v1's `level_breach`/`cross_deviation`
remain as JSON sugar mapping onto the grammar — existing artifacts
stay valid (§3).

### §1.3 Position layer (8i's paper-grade precursor)

Per-row state machine `Flat → Entered(side, qty, entry_px, entry_ts)
→ Flat`; entry emits BOTH legs when `ref` is a real sym (sym + ref,
opposite sides, equal notional; single-leg when CONST), exit emits
the closers. `min_hold_s` honored; caps pre-emit: per-position
`max_risk` ≤ $10k/order tier, net per-sym ≤ $20k and table ≤ $100k
**by construction** (positions exclusive per row; `group` byte makes
rows sharing a group hold AT MOST ONE position — argmax entry =
first/best qualifying row while the group is flat, deterministic row
order). A row with `exit == 0` and no position flags keeps v1
horizon-refire semantics. Paper law: positions advance on ACCEPTED
SUBMIT (paper has no fills); 8i upgrades to fill-confirmed without
touching the grammar — documented in the module header.

### §1.4 Universe-wide, descriptor-addressed binding

Rows in the JSON artifact reference instruments by **§9.4 descriptor
string** (`okx:BTC-USDT`, `deribit:BTC-…-C`, `binance-usdm:temusdt`,
`polymarket:<slug>`…), NEVER raw SymbolId: commit resolves descriptors
against the LIVE boot universe (the engine owns that map — it writes
the manifests). Unresolvable descriptor ⇒ commit REFUSES (fail-fast,
reported). This makes artifacts portable across restarts and universe
edits and — critically — makes OPTIONS tradeable by ruleset despite
per-boot ordinal reshuffle (manifest law). #7b re-commit re-resolves
every boot. Any venue, any instrument class in the universe is
addressable: PM binaries, spot, perps (TradFi incl.), dated futures,
options, bStocks — the emit path is already venue-generic (intent
carries the venue; notional math is px×qty everywhere).

### §1.5 Native backtests (all channels replayed)

The Rust harness k-way merge grows from ticks-only to ticks +
`*-events.pmlr` (funding → `on_venue_event`) + depth + OptSummary
streams, so every feature evaluates in replay exactly as live. Fill
law tracks the same positions and reports ROUND-TRIP realized P&L.
The first 24 h (or longest referenced window, if greater) of a replay
is WARMUP — features fill, no entries; split math unchanged, warmup
stated in the report. Funding/IV/depth strategies become backtestable
through the frozen argv — the session-2 gap closes.

### §1.6 Research-side generality (the agent's reach must equal the VM's)

Execution generality is worthless if the research agent cannot SEE an
instrument class offline. Chain audit (fetch → mine → author →
backtest → commit → audit-pnl) yields four v2 obligations and two
recorded absences:

- **Options fill & valuation law.** Options have NO BBO tick stream —
  only OptSummary mark/IV (kind 6). `backtest::fill` and audit-pnl
  fill/value from tick books, so options rulesets would be authorable
  but never honestly backtestable or auditable. v2 adds a MARK-BASED
  law in BOTH: fill at mark ± an assumed half-spread (per D-7), value
  positions at mark; the assumption is printed in every report that
  used it.
- **Depth research digest.** DepthTopK exists only in raw PMLR;
  mining depth strategies must not require multi-GB scans. A worker
  `depth_digest` module (hourly imbalance/spread/near-notional stats
  per sym, beside candles in candles.db) makes the channel minable —
  same pattern as `iv_digest`.
- **Offline coverage audit.** Verify (and fix where hollow) that the
  candle + funding fetch families actually cover EVERY universe
  class: bybit spot+linear, bStocks, TradFi perps, PM price history,
  options underlyings. Any hole silently blinds the agent to that
  class.
- **Machine-readable channel map.** The agent must know per
  instrument which features exist (depth? funding? IV?) BEFORE
  authoring, or rule 10 refusals become its discovery mechanism. A
  generated catalog (instrument descriptor → channel/feature columns)
  lands beside `research-universe.md`'s grammar section.
- **Recorded absences (future data slices, NOT v2):** open interest
  and trade-prints are not captured anywhere in the stack — volume
  research is candle-volume only. The feature enum is extensible;
  they slot in as new kinds when captured.
- **PM breadth prerequisite (outside v2):** the ≤6-token M1 cap
  (id-7 collision) is the one hard "all instruments" blocker on PM —
  the allocation-base slice is its own operator-ruled work item; v2
  neither fixes nor waits on it.

## §2 Design decisions (operator sign-off at kickoff)

- **D-1 Funding seed carrier.** After a restart (and for BN, whose
  funding WS is venue-dark), APR windows need seeding. RECOMMEND: one
  new AiCmd kind `FundingSeed` (sym, rate ×1e9, window slot) — pushed
  by the EXISTING hourly funding agent right after boot (#7b-style
  waiter extension) and on each BN 8 h print. Clean per-kind audit in
  ai-cmds.pmlr. Alternative: overload SetParam (no wire change, murky
  audit). Wire note either way in `docs/wire-format.md`.
- **D-2 Position restore on restart.** RECOMMEND: positions reset to
  Flat at boot; signal-persistent strategies re-enter automatically on
  the first post-seed evaluation — honest and stateless. Min-hold
  hysteresis across a restart is lost; if that matters, a
  `PositionSeed` AiCmd (same D-1 mechanism) is the lever — decide
  now, implement only if wanted.
- **D-3 Gate arithmetic for position strategies.** Round-trips are
  the honest `trades` unit for position rows, but slow strategies do
  FEW round-trips — the frozen `min_trades: 50` would refuse honest
  CVFC-class backtests forever. RECOMMEND: the report gains additive
  fields (`round_trips`, `legs`) and the worker gate counts LEGS
  toward `min_trades` while requiring `round_trips ≥ 10` for
  position rulesets — a D1-pattern frozen amendment, ruling cited in
  the pins. Schema stays version 1 with additive keys (verify the
  worker's parser tolerates additions; if strict, this is a schema-2
  bump — flagged, not assumed).
- **D-4 BN funding stays REST-fed** (venue-dark posture, re-probed
  2026-08-29): the hourly funding agent keeps fetching BN prints and
  forwards them as `FundingSeed`s. The DECISION loop is engine-only;
  this is a data feed for one dark venue and retires itself the day
  the venue heals. Confirm this satisfies "without cron" intent.
- **D-5 Row/table size.** The general grammar does not fit 64 B.
  RECOMMEND: 128-B v2 rows, 256 rows, 32 KiB table (stage/commit
  buffers grow accordingly; wire-format v2 note). Alternative — two
  64-B row halves — rejected: pointer-chasing and torn semantics.
- **D-6 Descriptor addressing (§1.4).** Changes the artifact JSON
  shape (descriptors, not syms) and adds commit-time resolution.
  RECOMMEND yes — it is what makes "all venues, all instruments,
  restart-stable" true. v1 artifacts (raw syms) stay commit-able
  through a compat arm for one release.
- **D-7 Options fill assumption (§1.6).** Mark-based fills need a
  half-spread number. RECOMMEND: fill at mark ± max(0.5% of mark,
  1 IV-tick equivalent), flat per venue, printed in every report
  that used it; revisit when real options quotes are captured.
  Alternative: refuse options fills entirely (keeps options
  research-only) — honest but blocks the class.
- **D-8 Research-side deliverables in scope (§1.6).** Confirm the
  `depth_digest` module, the offline coverage audit (+ fixes for
  any hollow class), and the generated channel map are IN v2 —
  they are worker/offline additions (allocate freely, no engine
  cost) but they are what makes the agent's reach equal the VM's.

## §3 Wire & format changes

- `RuleRow` v2 (128 B, D-5): `ver(1) flags(1) side(1) group(1)
  feat_a(1) feat_b(1) feat_c(1) combine(1) sym(4) ref(4) win_a(2)
  win_b(2) win_c(2) cmp_bits(1) pad(1) enter_1e9(8) exit_1e9(8)
  confirm_1e9(8) min_hold_s(4) horizon_ms(4) edge_bps(4) pad(4)
  max_risk_1e6(8) reserved → 128 B`. Table version 1→2; the engine
  accepts BOTH (v1 64-B rows map onto v2 with zeroed feature fields —
  H6-era artifacts and `cvfc-basis-kill` stay valid).
- Ruleset JSON grammar v2: `instrument`/`ref` take DESCRIPTOR strings
  (D-6); new optional keys (`feature`, `ref_feature`, `combine`,
  `window_min`, `ref_window_min`, `confirm_*`, `group`, `enter`,
  `exit`, `min_hold_s`) — absent keys = v1 semantics; `level_breach`
  / `cross_deviation` names remain as sugar. The §4.2 validator grows
  arms (handwritten scanner — NO serde_json — KEYWORD_CAP grows),
  rule 7 unchanged ($50k-tier numbers), new rule 9: position rows
  require exit semantics (no un-exitable positions), new rule 10:
  referenced features must exist for the resolved instrument's
  channels (no depth rows on a sym without depth).
- New AiCmd kind(s) per D-1/D-2, appended to the kind table,
  capture-compatible (64 B AiCmd unchanged). `docs/wire-format.md` +
  `docs/migration.md` entries for: table v2, descriptor resolution,
  new kinds, harness multi-channel replay.

## §4 Workstreams

- **V0 — design freeze.** Operator answers D-1…D-8; §1/§3 then
  LOCKED (WS10 pattern).
- **V1 — core-types.** RuleRow/RuleTable v2, feature/combine enums,
  the funding cadence const fn (single home), AiCmd kind(s). Unit +
  roundtrip tests; wire-format/migration docs.
- **V2 — feature engine.** Rolling-stat rings, funding windows
  (`on_venue_event`), IV (kind 6) + depth (kind 7) hooks, clock
  features, seed handling; alloc assertions grow (every feature path
  0 B/op); proptests vs naive reference implementations.
- **V3 — position layer + grammar eval.** Condition evaluator,
  state machine, group exclusivity, two-leg emit, pre-emit caps,
  v1-sugar mapping. Proptest invariants: never two positions per
  group; net sym exposure ≤ cap under arbitrary interleavings;
  absent-feature rows never fire.
- **V4 — validator v2 + fuzz.** Grammar arms, descriptor fields,
  rules 9–10, KEYWORD_CAP; commit-time descriptor resolution +
  refusal; `ruleset` fuzz target corpus extended, ≥300 s clean;
  alloc-assertion validator fixture gains v2 rows.
- **V5 — backtest harness + audit-pnl.** Multi-channel k-way merge
  (events, depth, OptSummary; VIRT anchoring identical), feature
  warmup, position-aware fill law, round-trip accounting, the D-7
  mark-based options fill/valuation law in BOTH `backtest::fill`
  and audit-pnl (assumption printed), report additive fields;
  synthetic-capture fixture with hand-computed round-trip P&L
  (exact-match test). Worker gate amendment per D-3 (pins cited).
- **V6 — worker seed lane + research-side reach (D-8).**
  `FundingSeed` pushes in the hourly funding agent + post-boot
  waiter; BN prints forwarded. `depth_digest` module (hourly stats
  beside candles, iv_digest pattern). Offline coverage audit:
  candles+funding fetch verified (fixed where hollow) for bybit
  spot+linear, bStocks, TradFi perps, PM history, options
  underlyings. Generated per-instrument channel map. Pytest.
- **V7 — validation rulesets: 3 migrations + ≥3 generality proofs.**
  Migrations: `xv-v2` (position-mode cross-deviation), `cvfc-v2`
  (groups, min-hold 96 h, enter 20 APR pts), `s1-v2` (7 pilot pairs,
  |spread| 50 %/30 % confirm). Generality proofs, one per NEW input
  class, backtested on the multi-day root (committed only on merit):
  spot↔perp basis (BTC bn-spot vs bn-usdm, funding confirm), IV
  spread (deribit vs okx BTC ATM), depth imbalance (okx BTC). All
  through the frozen §4 verbs.
- **V8 — parity window + cron retirement.** VM and crons run in
  PARALLEL ≥ 48 h; an offline comparator (audit doctrine) checks
  every cron entry/exit has a VM counterpart within one evaluation
  cadence, positions agree, per-tag P&L within tolerance. A full
  engine restart mid-window proves seed + re-resolve + re-enter +
  #7b recommit end-to-end. Then, on operator order: `launchctl
  bootout` com.multivenue.carry + com.multivenue.xv (plists archived;
  the FUNDING data agent remains per D-4). The engine executes every
  family alone.
- **V9 — gates + docs.** nextest/alloc/pytest/fuzz stay-greens;
  wire-format/migration/risk-policy cross-checks; CLAUDE.md CURRENT
  STATE; `docs/research-universe.md` gains the definitive "what the
  VM grammar can express" section (the loop agent's menu — it now
  researches over the FULL grammar, not two triggers);
  m5-runbook close entry.

## §5 Doctrine compliance (restated, binding)

Zero alloc after boot on every new path (feature blocks and position
arrays preallocated at table commit from the boot universe; seeds
mutate in place); `#[repr(C)]` + `Copy` PODs, align(64) blocks; no
serde_json (validator stays a handwritten scanner); no dyn; no tokio;
branch-lean feature eval (feature kinds dispatch via match on POD
enums — monomorphic, no fn pointers); every new parser input fuzzed;
every public fn happy+failure tested; offline paths (harness,
comparator) allocate freely with doctrine headers; SPDX on every new
file; license-check per commit; frozen surfaces amended ONLY per D-3
with rulings cited. Paper-only nature unchanged — v2 emits the same
paper orders through the same dispatcher, stamped s5, audited by the
same audit-pnl (per-hash rows now show round-trips).

## §6 Effort & sequencing

V0 ½ session · V1+V2 1–1½ sessions · V3+V4 1–1½ sessions · V5 1–1½
sessions (options law) · V6 ½–1 session (seeds + D-8 reach) · V7 ½–1
session · V8 48 h calendar (passive) + ½ session · V9 ½ session ⇒
**~6–7 working sessions + a 2-day parity window**, all before
M6/Stage-3. Checkpoint commits per workstream (`VM2:` prefix),
operator-authorized, explicit paths, no push.

## §7 Explicit non-goals

No live execution, no venue dispatchers, no RiskGate (the position
layer is its paper-grade precursor and says so); no serve/LLM
dependency anywhere in the loop; no change to the frozen backtest
argv or the 8-verb surface (only the D-3 gate amendment, if
approved); no >2-leg structures, no trailing/path-dependent state
beyond the position machine, no portfolio optimizer, no in-engine
annualization, no text/news features (Stage-3+ or native members);
no PM allocation-base work (the ≤6-token cap is its own slice); no
new venues.

## §8 Progress log

- 2026-08-29 — plan drafted on operator order.
- 2026-08-29 — GENERALIZED on operator order: scope = all strategy
  types, all venues, all instruments; feature-grammar design (§1.1–
  §1.4), D-5/D-6 added, V7 gains generality-proof rulesets.
- 2026-08-29 — research-side audit on operator challenge ("anything
  missed for the agent to research everything?"): §1.6 added —
  options mark-fill/valuation law (D-7), depth_digest + coverage
  audit + channel map (D-8), OI/trade-prints recorded absent, PM
  6-token cap flagged as the external breadth blocker. V5/V6
  extended accordingly. Awaiting D-1…D-8 + the go.
- 2026-08-29 — **V0 DESIGN FREEZE: D-1…D-8 RULED by operator
  (AskUserQuestion, this session); §1/§3 LOCKED (WS10 pattern).**
  - **D-1 = FundingSeed AiCmd** (new kind: sym, rate ×1e9, window
    slot; pushed by the hourly funding agent post-boot #7b-style +
    on each BN 8 h print; per-kind audit in ai-cmds.pmlr;
    wire-format note owed in V1).
  - **D-2 = PositionSeed AiCmd — the ALTERNATIVE chosen: positions
    RESTORE at boot**, not Flat-reset. Same D-1 carrier mechanism;
    seed carries side/qty/entry_px/entry_ts so min-hold hysteresis
    survives restarts. Wire kind lands in V1; push lane + post-boot
    waiter in V6. A row with no seed at boot starts Flat and
    re-enters on first post-seed evaluation (the recommended
    behavior remains the fallback, so restore is additive, never
    load-bearing for correctness).
  - **D-3 = amend (recommended):** report gains additive
    `round_trips` + `legs`; worker gate counts LEGS toward
    `min_trades` and requires `round_trips ≥ 10` for position
    rulesets. D1-pattern frozen amendment, ruling cited in the
    pins; schema stays 1 with additive keys unless the worker
    parser proves strict (then flagged schema-2, not assumed).
  - **D-4 = CONFIRMED:** BN funding stays REST-fed, forwarded as
    FundingSeeds; satisfies the cron-free intent — data agents
    (candles/funding/universe) remain, only the strategy crons
    (carry, xv) retire.
  - **D-5 = 128-B rows × 256 (32 KiB table);** table version 1→2,
    engine accepts both (v1 64-B rows map with zeroed feature
    fields). Two-half alternative rejected.
  - **D-6 = descriptor addressing APPROVED;** v1 raw-sym artifacts
    stay commit-able through a compat arm for one release.
  - **D-7 = mark-based options fills:** mark ± max(0.5 % of mark,
    1 IV-tick equivalent), flat per venue, assumption PRINTED in
    every report that used it; revisit when real options quotes
    are captured.
  - **D-8 = all three research-side deliverables IN scope**
    (depth_digest, offline coverage audit + fixes, generated
    channel map).
  Next: plan commit on operator authorization (`VM2:` prefix,
  explicit path), then V1.
- 2026-08-29 — plan committed `a6e38b8` (operator-authorized).
  Environment finding, recorded in memory + here: git write-ops
  through the Cowork sandbox mount leave stale `.git` locks (mount
  forbids unlink) — cleaned on the Mac; ALL git write-ops run on the
  Mac lane from now on.
- 2026-08-29 — **V1 CODED (core-types + docs; additive, workspace
  green).** `RuleRowV2` 128 B / `RuleTableV2` 32 832 B beside the v1
  types (v1 retires at V4 — every checkpoint stays compilable);
  `FeatId` (17 features; no `Last` — trade prints are the §1.6
  recorded absence) + `CombineOp` + `cmp_bits`/`flags` bit law;
  `AiCmdKind::FundingSeed=10` / `PositionSeed=11` + shape arms;
  `funding_print_divisor` (deribit ÷8) + `funding_period_s` single
  home. Wire-format + migration entries written. Stay-green: nextest
  workspace 1367/1367 (baseline 1351 + 16 new), core-types 68/68.
  **Three design refinements recorded (all within the locked
  skeleton, operator-visible):**
  1. FundingSeed carries RAW PRINTS (rate ×1e9 + venue print ms),
     not the D-1 sketch's per-window aggregates — windows recompute
     engine-side through the same path live funding events take, so
     the cadence law stays in its one home.
  2. PositionSeed: age rides in `qty` as SECONDS and `ttl_ns` is
     0-enforced — the engine drain expires ANY kind with nonzero
     `ttl_ns` (age-in-ttl would be dropped at drain); entry qty is
     NOT carried — the vm re-derives it from the committed row's
     sizing law at the seeded px (restores respect current caps).
  3. `max_hold_s` u32 allocated from the §3 reserved space — the S1
     age-out exit (>240 h) is otherwise inexpressible; 0 = no
     age-out.
  Universal exit law pinned: `signal × entry_sign ≤ exit_1e9`
  (covers xv |dev|-decay AND sign-flip, CVFC spread<0 after
  min-hold, S1 directional<10%). v1 sugar maps FULLY onto the
  grammar (bid `level_breach` → `LhsOnly(Ask) ≤ level`, ask →
  `LhsOnly(Bid) ≥ level`, `cross_deviation` →
  `|DiffBps(Mid,Mid)| ≥ edge` + side filter) — ONE evaluator path,
  no v1 branch. Alloc gate deferred to V2 (V1 links into no hot
  path); pytest untouched (499 stands). Committed `6ad3a86`.
- 2026-08-29 — **V2 CODED (feature engine + engine plumbing;
  workspace green).** `strategy_vm::features` = the §1.1 engine: ONE
  boxed ~12 MiB zeroed-at-boot state (heap-direct — a stack
  round-trip would overflow), per-sym latest values (open-addressed
  1024 slots), per-(sym,window) rolling minute rings (256-entry
  pool, ≤8 windows/sym, LAZY stats recompute once per minute per
  entry — deterministic in replay), funding blocks (256 × 640
  prints) with the per-venue SETTLED-print laws, mark/IV + depth
  features, clock features over a VENUE-DERIVED wall offset (no
  syscalls; replay-identical). Fed only through Strategy callbacks ⇒
  V5 replay parity is structural. Engine side: OptSummary enters the
  engine (kind 6's first ring) — `on_opt_summary` hook, 3 opt lanes
  (`opt_lane_of` okx/deribit/bn — BN venue-dark, `.env`-heal-ready),
  capture-before-push at all emit sites, `opt_ring_drops_total`.
  Venue-truth work: deribit ticker parser gains `funding_8h`
  (Funding `v1`, was 0 — the worker-REST-parity series; ÷8 law
  applies to ONE series now); HL gained its venue-event lane
  (funding rides AssetCtx; mask FUNDING|ASSET_CTX); OKX/Bybit/BN
  use the next-funding-ADVANCE settled-print law. FundingSeed
  consumed by the vm (same path as live events; dedup within half
  the venue period). **A REAL BUG caught by the new proptest and
  fixed pre-commit:** the rolling ring's advance-clearing cap
  (`min(gap,w)−1`) left one stale slot on ring wrap — a phantom
  sample resurfaced under sparse ticking; law corrected to
  `min(gap−1, w)` cleared slots, red→green pinned by
  `features_proptest::rolling_stats_match_naive`. Gates: workspace
  nextest 1388/1388 + 2 proptests (roll-stats + funding-APR vs the
  transcribed carry_signal law) · **release alloc 39/39** (new gate
  39 = every feature ingest+read path 0 B/op, seeds/dedup/lazy
  recomputes included) · license-check green. PositionSeed consume +
  grammar eval = V3. Committed `781ee1a`.
- 2026-08-30 — **V3 CODED (grammar evaluator + position layer;
  workspace green).** `VmStrategy` rewritten around the v2 grammar:
  signal = combine(feat_a, feat_b) ×1e9, confirm gates ENTRY only,
  direction law (LhsOnly rows emit `side`; signal-signed rows
  mean-revert with `side` as filter), the §1.3 state machine
  (Flat→Entered→Flat, group exclusivity first-qualifying-row,
  two-leg equal-notional emits with per-leg clamp, min-hold gate,
  max-hold UNCONDITIONAL age-out, universal exit
  `signal × entry_sign ≤ exit_1e9`, paper law = advance on accepted
  sym-leg submit, ref-leg refusals counted `leg_drops`), and
  `PositionSeed` (D-2) restore with min-hold memory + refusal
  counting. v1 tables map at receive (`RuleRowV2::from_v1` +
  `map_v1`) — the 20 pre-V3 vm tests pass THROUGH the sugar path
  (v1-semantics regression suite); the both-sides `level_breach`
  lives as the documented sugar arm. **Deliberate semantic delta:**
  rows evaluate on EITHER leg's tick (two-legged freshness) — the
  golden harness's run-1 fire moved 200 ns earlier onto its ref
  tick, fires/emits/bounds byte-identical; migration.md documents
  it. Book generic retired (VmStrategy<N> → VmStrategy;
  FEAT_SYM_SLOTS 1024→4096 absorbs BACKTEST_VM_SLOTS; SET_VM_SLOTS
  gone). **caps-proptest CATCH: zero-notional micro-cap orders**
  (cap $1e-6 at ~\$1 px → qty 1, notional 0) — the clamp moved into
  `sized_qty_1e6` itself, pinned by
  `micro_cap_zero_notional_is_clamped_away`. New V3 tests: pair
  enter/exit both legs, sign-flip exit, re-entry cooldown, group
  exclusivity, min/max-hold, absent-holds (funding row with no
  data), confirm gate (wall-absent ⇒ hold), seed
  restore-with-min-hold-memory, seed refusal battery, flip resets
  positions, ref-leg ring-full accounting. Gates: nextest
  1404/1404 · alloc 39/39 release (fresh Compiling) · pytest 499 ·
  license green. Committed `e9c6d43`.
- 2026-08-30 — **V4 CODED (validator v2 + descriptor resolution +
  ring flip; workspace green, fuzz running).** The §4.2 validator
  gained the v2 grammar arm (wire-format.md "Ruleset JSON grammar
  v2"): descriptor-addressed rows resolved at STAGE time against the
  bin's `DescriptorTable` (built beside `instrument-manifest.tsv`
  from the SAME allocation truth, with per-lane channel-capability
  bits; unresolvable ⇒ the new `Descriptor` REFUSE — D-6 fail-fast;
  #7b re-stages ⇒ re-resolves every boot); rule 9 (`Position`: exit
  ⇔ position row, holds/groups require it, max>min sanity); rule 10
  (`Feature`: channel capabilities per resolved leg, the rule-3
  window law, the rolling-bind budget ≤8/sym ≤256 pairs); signed
  9-DECIMAL thresholds (funding rates survive — pinned by
  `v2_signal_scanner_keeps_nine_decimals`); KEYWORD_CAP 16→24;
  rule 7 charges BOTH legs of two-leg position rows; rule 8 v2
  identity includes features/windows/cmp-bits (a coarser draft
  collided with the bind-budget fixture — caught in-session).
  v1 rows validate byte-exactly through the compat arm (built VIA
  `RuleRowV2::from_v1`); both shapes coexist per artifact (pinned).
  The §6 handoff ring flipped to v2 (`RuleTableSlot = RuleTableV2`,
  32 832 B slots; `on_ruleset_table(&RuleTableV2)`); the v1
  `RuleTable` RETIRED (RuleRow stays as the v1-grammar record
  through the compat window). Backtest resolves v2 descriptors from
  the NEWEST run's manifest (offline `caps_of_descriptor` string
  law, documented-permissive; pre-D3 captures refuse v2 rows
  honestly; cross-run option-ordinal rebind = a documented V5
  concern). Fuzz target covers both arms (fixture descriptor table
  in-target; corpus seeded v2/mixed); bench gate 34 = 255 v1 + 1 v2
  rows with live resolution measured. Gates: nextest 1412/1412 ·
  alloc 39/39 release (fresh Compiling) · pytest 499 · license
  green · **fuzz `ruleset_json` 311 s / 34.05M runs CLEAN** (corpus
  is machine-local by repo convention; the v2 seeds live there).
  Committed `e181ee9`.
- 2026-08-30 — **V5 CODED (multi-channel backtest + audit-pnl D-7 +
  D-3 gate; workspace green).** Harness: the merge carries
  funding/ctx events + depth + OptSummary beside ticks (lord-space
  extension of the §3.2 key; VIRT anchoring identical) through the
  vm's REAL callbacks — §1.5 parity is structural; per-run manifest
  REBIND (descriptor join to the newest run) unifies reshuffled
  ordinals (pinned: `v5_cross_run_manifest_rebind_unifies_syms`);
  WARMUP = the longest TABLE-referenced window (Roll wins, Apr24
  24 h, Apr72 72 h; 0 when none — REFINEMENT of the locked "first
  24 h" text: a flat floor would zero every short-capture v1
  backtest while warming nothing; features-only feed via the public
  `feats` API). **D-7 options law in `backtest::fill` (ONE home,
  audit-pnl reuses it):** mark-bearing OptSummary ⇒ synthetic
  zero-spread mark ticks for tickless option syms; registered syms
  fill IMMEDIATELY at `mark ± max(0.5%, 1 tick)` with TAKER fees
  (strict-cross can never cross its own synthetic book — the
  mark-fill CLASS is the honest reading of the ruling), valued at
  mark, `mark_fills` counted, assumption PRINTED in harness stderr
  AND audit-pnl (pinned in both). okx's markless summaries stay
  honestly unpriceable; live okx-option execution remains blocked
  (recorded divergence — backtest models D-7, live waits on real
  quotes). **D-3:** schema-1 additive keys `oos.round_trips` /
  `oos.legs` / `position_rows` (schema stays 1; goldens updated);
  the worker gate counts LEGS and folds `round_trips ≥ 10` for
  position rulesets into the FROZEN-SHAPED `min_trades` verdict
  (GateThresholds/GateResult untouched; `MIN_ROUND_TRIPS = 10`
  cited to the ruling; `tests/test_backtest_d3.py` pins pre-V5
  reports gating byte-identically). **The V5 golden:**
  `v5_position_round_trip_hand_computed_exact` — a v2 position pair
  through entry→reversion→exit, every fill at its resting px, net
  EXACTLY $3 234, dd 0, legs 4, round_trips 1, byte-exact schema
  line (a fixture-size catch en route: the shared 10-unit book
  partial-filled the $9.9k legs — deep-book v5_tick pinned it).
  Gates: nextest 1417/1417 · cli 151/151 · alloc/pytest/license
  this entry's close.

- **2026-08-30 — V6 CODED + REAL-ROOT SMOKED (worker seed lane +
  D-8 research reach).** pmlr.py grew the kind-7 `DepthReader`
  (192-B stride — the container's first kind-determined slot size;
  the 64-B `Reader` still refuses kind 7 by design, cross-pinned)
  and the kind-3 `OrderRec` decode; frames.py kinds 10/11. FOUR new
  modules (module-surface law, never verbs, serialized): **seeds**
  (D-1 kind-10 pushes: 73 h of RAW venue prints from the funding
  table, manifest-resolved, engine owns the ÷8 law, ≤640/sym
  newest-kept; D-2 kind-11 restores: previous-run engine-orders
  slot-5 FIFO fold under the (sym,ref)-unique-row ambiguity law,
  sym RE-RESOLVED through the CURRENT manifest, px = surviving-
  basket VWAP 1e6, qty = age s, ttl 0; `--dry-run` keyless;
  heartbeat-first §5.4 push path), **depth_digest** (hourly
  imb-OHLC/spread-bps-avg/near-notional-avg per (venue, descriptor)
  beside candles, iv_digest pattern, STALE + empty-side skipped),
  **coverage_audit** (expected-vs-present per class ×
  candles/funding/iv/depth; expectation law from caps),
  **channel_map** (generated TSV; `caps_of_descriptor` python
  mirror pinned CROSS-LANGUAGE against the NEW Rust
  `caps_of_descriptor_law` test — 14 fixture rows, change either
  side only with the other). candles-cycle.sh runs the depth digest
  in the same serialized window (D3 precedent). **Real-root
  smokes:** depth_digest folded 725 943 live snapshots → 108 hourly
  buckets, unresolved 0 (BTC-PERP 0.06 bps vs ADA 7.9 bps — sane
  microstructure; backfilled into the REAL candles.db); seeds
  --dry-run framed 1 634 prints across 52 funding descriptors;
  channel_map rendered the live manifest. **The audit EARNED ITS
  KEEP on first run (hollow-lanes-total=103):** (1) binance-usdm
  candles 12/22 missing ⇒ ROOT-CAUSED per-VENUE budget pooling —
  spot+usdm shared ONE 30-call budget across DIFFERENT hosts, spot
  ran first, usdm starved to ZERO pages every cycle since the M5/BST
  additions ⇒ FIXED: `run_cycle` budgets per HOST (`budget_key`;
  bybit categories pool) and demand-sized `max(floor, 2×tfs×targets)`
  (~2 calls/min worst case, free-tier-trivial; old shared-budget test
  REWRITTEN into the per-host pin + a can't-starve regression);
  (2) polymarket candles 4/6 missing = STALE market-map (fetch not
  run since the 16:00Z universe refresh) — operational: run `fetch`
  post-refresh, wiring it into the BST3 refresh family is an
  operator call; (3) binance-opt iv 64/64 missing = the venue-dark
  eapi lane (standing .env lever, recorded); (4) okx/opt iv 52/64 —
  the 12 `_UM`-family instruments carry no opt-summary in-window
  (venue-side observation, recorded); (5) deribit+okx depth hollow =
  the digest not yet landed — closed by this workstream's backfill.
  Tests: +36 worker pins (layout pins for Depth/Order at documented
  offsets, torn-slot law, measure/fold/upsert, raw-rate + window +
  640-cap laws, artifact exit-key law, FIFO/flip folds, ambiguity +
  re-resolution + age, dry-run E2E, ÷8 mirror pin citing
  funding_print_divisor, caps law table, audit expectation law) +
  the candle-budget pins (old shared-venue pin rewritten per-host +
  a can't-starve regression) + 1 Rust caps pin. **Gates at entry
  close: nextest 1418/1418 · release alloc 39/39 (fresh `Compiling
  bench`) · pytest 547 (release binary on PATH) · license-check OK
  (222 files) — new stay-greens 1418/39/547.** Budget fix
  LIVE-PROVEN in one manual cycle: all 12 starved symbols
  backfilled 2 880 1m bars each, ZERO BUDGET lines. (A debug
  parallel-nextest one-off on `rpc_block_number_is_zero_alloc`
  under REST-cycle machine load passed solo + on the quiet full
  rerun — the V2-noted bench-crate noise class; the release gate is
  authoritative.)

- **2026-08-30 — V7 CODED + REAL-BACKTESTED (6 artifacts through
  the frozen verb; 2 harness fixes found by the proofs; staging =
  root-age-gated, operator ruling pending).** Authored via a
  git-excluded repo-side one-shot (that class, and its worked example:
  `docs/research-tools-exclusion-plan.md` — the only doc that may name
  one; the reproducible record is the sha256-named artifact itself in
  ~/multivenue/artifacts/rulesets, under the stage verb's recompute
  law, plus its stage/commit seqs in the audit-replay chain):
  **xv-v2** `33e91345…` (2 position pairs, mid diff_bps ABS 4.0 →
  exit 1.0, $4,950/leg), **cvfc-v2** `f7d79ce5…` (5 coins × 2
  addressable venue pairs, apr24 diff ABS 0.20 → exit 0.0, group per
  coin = the MAX_POSITIONS-5 law, min-hold 96 h, $4,950/leg ⇒ table
  Σ $99k inside the GROUP-BLIND rule-7 cap — leg size is the
  documented delta vs the cron's $9.9k; the deribit↔hl cross pair
  dropped), **s1-v2** `a56350ce…` (7 usdm perps, apr24 LhsOnly ABS
  0.50, apr72 confirm ABS 0.30, exit 0.10, max-hold 240 h; the
  cron's global 4-cap not reproduced — documented), **basis-proof**
  `dfaaef31…`, **iv-spread-proof** `e98e7413…` (deribit vs okx BTC
  30AUG26-77500-C mark_iv diff), **depth-imb-proof** `2f0dbd91…`.
  Grammar lessons: `family` is the CLOSED enum (crypto/politics/
  sports/macro/other). All six ran through the FROZEN
  `claude-worker backtest` verb on `~/multivenue/backtest-roots/
  v7-root` (symlinks to the 7 depth/funding-era runs; captured span
  at run time = Aug-29 07:33Z → ~20:00Z — the harness's own truth:
  virt span 44,739 s ≈ **12.4 h, 1 UTC day** [correction 2026-08-30:
  earlier drafts said "36 h", a wall-clock slip off the operator's
  LOCAL Aug-30 date while UTC was still Aug-29 evening]; ≈2.5 GB,
  34.9M merged ticks).
  **Harness fixes the proofs forced (both pinned):** (1) the
  §6-law completion — a run-manifest sym whose descriptor the
  binding manifest no longer carries is DEAD and its records DROP
  (`dropped_foreign`, printed per run + in the new `channels:`
  stderr line): pass-through ordinals were interleaving expired
  options/rotated PM dailies into current instruments — the 7-run
  iv backtest carried **747,640 foreign records and a phantom
  $248.8M per-sym bound**, sane ($3.2k) after the fix; pin =
  `v7_dead_descriptor_records_drop_instead_of_colliding`. (2) the
  D-7 mark-fill exactly-once pin
  (`mark_fill_fills_exactly_once_across_many_marks`) + `mark=` on
  the fills stderr line. Finding recorded, non-gating: okx OPTIONS
  emit a real BBO tick lane live (2,846 legit partial cross-fills
  against the real thin book) — `caps_of_descriptor`'s CAP_OPT-only
  okx-option row UNDERSTATES the wire; a rule-10 refinement
  candidate for a later phase, cross-language pin to move with it.
  **Final OOS table (70/30, this root):** xv-v2 **+$11.73, 44 legs,
  11 round-trips, dd $17.52, bounds ALL GREEN** — fails ONLY
  legs 44<50 + trading_days 1<2, both pure root-age; cvfc-v2 valid,
  0 trades (24 h apr warmup + 96 h min-hold ⇒ no round trip can
  exist inside the 12.4 h root — REACHING rt≥10 needs a multi-WEEK
  root at 96 h holds); s1-v2 valid, 0 trades (72 h apr72 confirm warmup >
  root span); basis honest-zero (no |3 bps| spot↔perp crossing with
  live-funding confirm in OOS); iv-spread exercised the WHOLE
  options path in-sample (entries on real IV spreads, D-7 mark
  fills, real okx option book cross-fills) with an honestly empty
  OOS (the 30AUG26 options EXPIRE before the OOS window); depth-imb
  machinery-proven and NEGATIVE on merit (−$58.10 OOS) — per the
  committed-only-on-merit law, not staged. **Staging/commit of the
  migrations is structurally gated**: the frozen stage verb demands
  gates.all_passed and NO OVERRIDE EXISTS — xv-v2 is expected to
  clear as the funding-era root reaches ≥2 OOS UTC days (~Sep-1/2
  rerun); cvfc/s1 cannot clear D-3's rt≥10 on any near-term root at
  their hold laws — sequencing put to the operator at this entry.
  **Gates at entry close: nextest 1420/1420 · release alloc 39/39
  (fresh `Compiling bench`) · pytest 547 · license-check OK — new
  stay-greens 1420/39/547.**
  **OPERATOR RULING (2026-08-30, V7 exit):** stage on ROOT-AGE —
  xv-v2 reruns through the frozen verb ~Sep-1/2 (OOS ≥ 2 UTC days)
  and, gates-passing, stages+commits as V8's OPENING ACT; cvfc-v2 /
  s1-v2 join the parity window as their gates clear naturally; the
  crons keep carrying every not-yet-staged family, and V8's cron
  bootout is PER-FAMILY on operator order. No frozen-surface
  amendment for the slow-carry families at this time.

- **2026-08-30 — V8 PREP CODED (operator-authorized same day):
  comparator + runbook + seed wiring; S1 pair correction; the
  one-table/merged law.** `claude_worker.parity` (module, never a
  verb): ONE ground truth for both sides = `engine-orders.pmlr`
  (cron families = slot-4 ai-exec intents, VM = slot-5; same file,
  clock, manifests — no cron state files trusted). EVENT law
  (chronological net-fold per (slot, descriptor): |net|-increase =
  entry, decrease-to-0 = exit, sign-flip = both; every cron event
  needs a same-type/same-direction VM event on the descriptor within
  the family tolerance — xv 600 s, carry 7200 s), POSITION law
  (end-of-window net SIGN agreement; sizes deliberately differ),
  P&L stays audit-pnl's. VM extras are informational. 6 pytest pins
  + REAL-ROOT smoke: the honest pre-staging RED — cron entries live
  right now (xv hl:BTC; carry COTI + 1000RATS S1 pairs, ADA cvfc
  pair) all MISS with vm-events 0. **The smoke exposed the V7 S1
  authoring error: the live cron trades bn-usdm↔bybit-linear
  funding-SPREAD pairs (sp24/sp3 = cross-venue spreads), not
  single-leg apr — s1-v2 re-authored as pair rows with confirm_pair
  apr72 (`0cf7433e…`, old `a56350ce…` retired); re-backtested valid
  (structural 0 at the 72 h warmup).** NEW `merged-v2` artifact
  `4d5dbe65…` (19 rows, $99.4k static sum) for the ONE-TABLE law +
  the discovered sequencing consequence: TABLE-GLOBAL warmup means
  the merged table 0-trades on short roots even where xv-v2 alone
  trades — xv commits alone first; merged waits for root depth
  (per-row warmup recorded as a refinement candidate).
  `scripts/seed-push.sh` committed UNWIRED (runbook §9 carries the
  one-line wrapper hookup + `MULTIVENUE_SEED_RULESET`, applied on
  operator order). §9 runbook below = the full execute path (rerun →
  stage → commit → 48 h window → restart drill → per-family
  bootout). Gates: pytest 550 (+6) · license-check OK (231).

- **2026-08-30 — V8 EXECUTION RESUME POINT (the Sep-1/2 session
  starts HERE).** State: V0–V7 committed (`…`, `70bc59b` V5,
  `342eccc` V6, `6cc1ba5` V7, `2a283a5` V8-prep); stay-greens
  nextest 1420 / release alloc 39 / pytest 550 (553 with the release
  binary on PATH) / license-check 231 files. The engine runs
  unattended; crons carry all families; the VM is inert (no
  committed table); seeds/parity/depth-digest/coverage/channel-map
  modules all landed and real-root-smoked. NEXT SESSION, in order:
  (1) runbook §9 step 1 — SUPERSEDED 2026-08-30 by the MVP-tempo
  ruling entry below: min_trading_days 2 → 1, xv-v2 retuned to
  `bfbc5349…` (okx-only, 3.0/1.0, $3,000/leg) and ALREADY
  gates-PASSED (exit 0). Start at step 2 = stage + commit. (2) On PASS: stage-ruleset +
  commit-ruleset (engine live, §9 step 1 commands). (3) Operator
  applies §9 step 2 (wrapper hookup + MULTIVENUE_SEED_RULESET).
  (4) Open the ≥ 48 h parity window (§9 step 3, daily
  `python -m claude_worker.parity --window-h 48`; xv family GREEN =
  misses 0 + position sign agreement; carry family stays RED-by-
  absence until cvfc/s1 stage — per-family law). (5) Mid-window
  restart drill (§9 step 4). (6) Per-family bootout ONLY on explicit
  operator order (§9 step 5). cvfc-v2 `f7d79ce5…` / s1-v2
  `0cf7433e…` reruns ride the same verb as their gate horizons
  arrive; switch to merged-v2 `4d5dbe65…` only when the MERGED
  report itself passes (one-table + table-global-warmup laws, §9).
  Then V9 (gates + docs + CLAUDE.md CURRENT STATE + closure).

- **2026-08-30 — OPERATOR RULING (MVP tempo) + xv-v2 RETUNE ⇒ ALL
  GATES PASS; the V8 opening act unlocked TODAY.** Ruling: the
  operator cannot afford the ~2-day wait — `GateThresholds.
  min_trading_days` 2 → 1 (the D1-pattern frozen-surface amendment;
  citation at the constant + in the amended pin; trade-off on
  record: at floor 1 the OOS verdict can come from a single day's
  regime — revisit at the M6 soak). Worker suite 553 green under
  the amendment. **The immediate rerun then FAILED honestly** on
  the aged 25 h root (OOS −$5.81, dd $131, sym bound $59.6k —
  min_days/min_trades now green, pnl/bounds red): the overnight
  hours flipped it, exactly the variance the old floor guarded.
  **Per-pair probes isolated the damage:** okx↔bn-spot CLEAN
  (+$8.75, dd $5.89, bounds green @ 4.0 bps; +$14.73 / 92 legs @
  3.0 bps), hl↔bn-usdm carried ALL of it (−$0.12, dd $129, $59.6k
  stacking — unfilled exits piling on the thin overnight book at
  the model's 600 ms hl latency). **Retune (one iteration, recorded
  as such):** xv-v2 = okx↔bn-spot ONLY, enter 3.0 / exit 1.0 bps,
  $3,000/leg (the deterministic 6-leg in-flight stack × $3,000 =
  $18.1k ≤ the $20k per-sym bound). New artifact **`bfbc5349…`**
  (old `33e91345…` retired); merged-v2 → **`79eaceec…`** ($85.6k
  static sum). **FROZEN-VERB RESULT: exit 0, ALL GATES PASS — OOS
  +$8.93, 83 legs, 17 round trips, dd $16.50, bounds
  $3,002/$18,056/$24,073.** Standing notes: (a) the cron keeps
  carrying the hl pair — parity[xv] will show its entries as
  KNOWN standing misses until the operator rules its fate at
  bootout (§9 step 5: booting out com.multivenue.xv drops BOTH
  pairs — keep the cron for hl, migrate a retuned hl row later, or
  accept dropping hl); (b) probe artifacts were /tmp diagnostics,
  not registry artifacts.

- **2026-08-30 ~08:55Z — V8 OPENING ACT EXECUTED: xv-v2 LIVE on the
  standing engine; the ≥48 h parity window is OPEN.** Repo commit
  `928fc99` (gate amendment + retune). Then through the frozen
  verbs on the live engine: **stage seq=48 · commit seq=50** for
  `bfbc5349…`; engine confirms staged 1 / committed 1 / rejected 0,
  **vm_rows_active 1, vm_table_epoch 1**, vm_fires 0 (awaiting the
  first ≥3 bps deviation). **Seed lane WIRED live (operator-
  authorized):** engine-wrapper.sh gained the backgrounded
  seed-push line (45 s grace so #7b recommits FIRST — a seed
  against an inert VM is refused by design; same reparented +
  interpreter-invoked laws), `.env` gained
  `MULTIVENUE_SEED_RULESET` → the committed artifact (appended,
  never read). **Proven E2E immediately: 1,669 frames SENT live**
  (52 funding descriptors; position lane honestly flat=1/seeded=0 —
  no slot-5 history yet); `engine_ingress_ai_cmds_total` 1674,
  rejected 0. **Parity window T0 ≈ 2026-08-30 08:55Z** — daily
  `python -m claude_worker.parity --window-h 48`; xv-family GREEN
  criterion per §9 step 3 with the KNOWN standing hl-pair misses
  (cron-only, operator rules at bootout). **The 16:05Z T2 restart
  today doubles as the §9 step-4 drill for free**: expect #7b
  restage+recommit in recommit.log, seed-push.log frames sent, and
  any open xv position restored with continuous age
  (`position_seeds_applied ≥ 1` iff a position is open at 16:05Z).
  48 h earliest GREEN close: ~Sep-1 09:00Z, then per-family bootout
  on explicit operator order only.

- **2026-08-30 — V9 PREP (everything not gated on the parity
  outcome), run while the window ages.** Health at ~09:40Z: VM
  armed (rows 1, fires 0 — no ≥3 bps deviation yet), both carriers
  quiet in the trailing 2 h, parity trivially GREEN.
  **research-universe.md actualized + gained §6 "What the ruleset
  grammar expresses"** (the D-8 deliverable: features/combines/
  entry/confirm/position laws, sizing arithmetic, descriptor
  identity + seed continuity, the CANNOT-express list, the
  author→backtest→stage→commit→parity path; also fixed stale v1
  vocabulary, demo-tier caps, pre-amendment gate numbers, and the
  live inventory). CLAUDE.md VM2 bullet actualized to the LIVE
  state. **Battery: nextest 1420/1420 · release alloc 39/39 (fresh
  `Compiling bench`) · pytest 553 · fuzz ruleset_json 311 s /
  36.33M runs CLEAN (the V7-touched target re-cleared) —
  stay-greens 1420/39/553.** Remaining for V9 close (after the
  window): parity GREEN record, restart-drill record (16:05Z today,
  free), bootout record on operator order, closure entries +
  m5-runbook note.

- **2026-09-02 — V8 OUTAGE ROOT-CAUSED + FIXED + REVIVED (the
  parity window RESTARTS; supersedes the "window open since Aug-30
  08:55Z" reading).** What actually ran: xv-v2 traded ONE window,
  2026-08-30 08:55Z → 16:05Z (~7 h 10 m). The interim audit
  (2026-08-31 session, audit-pnl strategy-5 bucket): **net +$32.71
  realized, 156 orders / 187 modeled fills, max_dd $247.88, both
  legs flat at window end** (binance:btcusdt +$34.79 / 92 fills;
  okx:BTC-USDT −$2.08 / 95 fills). Then FOUR consecutive boots
  (Aug-30 16:05 / 20:15 / 21:15Z, Aug-31 00:00Z — and every boot
  through Sep-2 08:30Z) left the VM INERT. **Root cause (worker
  side, latent since #7b landed):** `claude_worker.recommit.
  wait_for_sock` tests `sock_path.exists()` only — the STALE
  ai.sock inode from the PREVIOUS boot satisfies it instantly,
  `connect()` then gets `[Errno 61] Connection refused` (the new
  engine hasn't bound yet) and main ABORTED with no retry; the
  `--wait-sock-seconds 180` budget never engaged. Latent before
  Aug-30 because earlier boots refused EARLIER (retired H6 demo
  bound-paths) — the first real recommit attempt was the first
  crash. **Fix (this session):** `main` now RETRIES transport
  failures against the same budget (2 s cadence, one final attempt
  at the deadline; gate/state refusals stay immediate — only
  transport is a race); pins
  `test_stale_socket_retries_until_engine_binds` (the outage
  red→green: stale inode + late-binding FakeUdsServer → EXIT_OK,
  ≥3 frames) and `test_stale_socket_exhausts_budget_as_transport`.
  Also: seed-push.sh's serialization skip became a bounded WAIT
  (5 min, 30 s poll — two boots had lost their seed push to
  transient collisions). Worker suite **600 green** (interim
  sessions grew it; +2 here). **REVIVE executed ~13:05Z:** the
  fixed recommit re-staged (seq 18618) + re-committed (seq 18619)
  `bfbc5349…`; engine staged 1 / committed 1 / rejected 0,
  **vm_rows_active 1 / epoch 1**; seeds re-pushed (1,625 funding
  frames; position lane honestly flat). **Parity window T0-2 =
  2026-09-02 ~13:05Z. OPERATOR RULING (same session, MVP tempo):
  the window duration is 2 HOURS, not ≥48 h — GREEN check ~15:05Z
  today.** Caveat on record: 2 quiet hours can yield a VACUOUS
  green (zero events both sides — the comparator reports it
  honestly as cron-events=0); the operator rules bootout with that
  in front of him. The 16:05Z T2 restart lands AFTER the window and
  serves as the fixed recommit's first live boot proof
  (non-gating). The Aug-30 7 h window stands as evidence, not as
  window time. Ops note: a metrics scrape immediately after commit
  can race the drain — staged/committed read 0 for ~a second before
  flipping.

- **2026-09-02 ~13:10–13:35Z — DISK-FULL EMERGENCY (found while
  building the operator's all-strategies P&L report) + RECOVERY +
  the recommit fix's LIVE PROOF.** The Data volume hit 100 %
  (418/460 GB; ~360 GB non-project data; logs 49 GB at ~5-8 GB/day)
  — **all six venue capture lanes + the orders log took ENOSPC and
  the writers WEDGED** (bn-ticks frozen ~13:48 local; writers do
  not retry after ENOSPC — engine restart is the recovery, recorded
  ops fact). Operator approved: (1) scratch/cache cleanup (~4 GB:
  fuzz/target, uv cache, cargo debug profile), (2) retention
  PROTECT_DAYS 7 → 5 via `~/multivenue/retention.conf` (the conf,
  not the script — the designed knob) + one manual retention pass
  (Aug-26/27 runs archived-compressed; 11 GB free after), (3)
  engine restart. **Restart 13:33:19Z (run-1788356599313696000
  ≈13:33) = the stale-sock fix's FIRST REAL BOOT: recommit.log
  shows the exact designed sequence — two "Connection refused —
  engine not bound yet, retrying" lines then re-staged seq=20247 +
  re-committed seq=20248 — vm_rows_active 1 / epoch 1 with ZERO
  manual help; seed-push sent 1,625 frames; capture_io_errors 0
  all lanes.** The outage class is dead. **Parity window T0-3 =
  2026-09-02 13:33Z; the operator's 2-hour ruling ⇒ GREEN check
  ~15:33Z.** The 13:05Z revive window (T0-2) was voided by the
  capture wedge inside it. Report deliverables (untracked, in
  `pnl-reports/` + `vm2-window1-report.md`): all-strategies
  totals/per-day/trades — s0 modeled −$1,408.59 net (5,009 fills;
  3.71M of 3.72M intents caps-rejected by the model), s4 −$15,471.25
  WITH the run-boundary caveat (per-run modeling splits 96 h cron
  positions into naked legs; the honest chained whole-root
  audit-pnl OOMs at this scale — recorded fix-shape: streaming
  mode), s5 −$152.30 (the one 7 h window; strict-cross taker-floor
  caveat). Ops findings also recorded: the 00:20Z nightly pnl
  timer has been DEAD since Aug-23 (one report ever) — needs its
  own lane revival; pre-Aug-26 P&L unrecoverable (retention).

- **2026-09-02 ~14:20–15:20Z — OPERATOR-ORDERED BOOTOUT EXECUTED (V8
  CLOSES).** Operator ruled: stop the parity run now, validate,
  decommission the crons, keep everything engine-only. Parity
  comparator on the truncated window (13:33Z boot → ~14:5xZ):
  **GREEN** — 0 misses / 0 position disagreements (16 VM orders vs 0
  cron events; cron-side quiet ⇒ semi-vacuous, caveat standing).
  Positions verb + seed lane both flat (the COTI carry position had
  closed) ⇒ bootout positionally clean. **com.multivenue.carry +
  com.multivenue.xv booted out AND plists deleted.** The engine-only
  continuation: xv lives on the VM (`bfbc5349…` active, rows 1).
  **Carry is DARK by operator order:** the merged-table attempt
  (retuned xv + cvfc-v2 + s1-v2) was built (`fe2f5aab…`, then
  `b9883c1a…` after a Rule-7 leg-counted table-cap rejection at
  $174,300 > $100k — carry legs rescaled 4950→2750) but its gate
  run produced OOS 0-trades (bounded root's OOS tail = the outage
  window; the healthy-root rerun was operator-killed as taking too
  long) and the frozen §6 stage law (gates.all_passed required, NO
  OVERRIDE) refuses a failing report — so no merged commit. Carry
  revisit shape when wanted: healthy 3-day root (Aug-29→31 links
  survive in logs/), b9883c1a as candidate. Ops learnings recorded:
  worker-verb detached runs need launchd submit + `set -a` .env
  export + release-dir PATH (the H6b wrapper pattern; `python -m
  claude_worker.cli` is a silent no-op — no `__main__`); the
  backtest harness OOMs (SIGKILL) on the ~44 GB full root — bounded
  roots are the working shape (17 GB ≈ 8 GB RSS). Stay-green battery
  deferred to the V9 gates pass (engine + VM verified live instead).

## §9 V8 parity runbook (prepared 2026-08-30; execute on the root-age ruling's schedule)

Every step below runs on the Mac; worker invocations serialized
(`pgrep -f 'claude[-_]worke[r]'` first); one-engine law for anything
touching the standing instance.

**1. Root-age rerun (~Sep-1/2, then stage + commit xv-v2):**

```sh
cd ~/trading-engine-multivenue/claude-worker
# refresh the root: add the newest run symlinks beside the V7 seven
for r in ~/multivenue/logs/run-*; do ln -sfn "$r" ~/multivenue/backtest-roots/v7-root/$(basename "$r"); done
set -a; . ../.env; set +a
PATH=~/trading-engine-multivenue/target/release:$PATH uv run claude-worker backtest \
  --ruleset ~/multivenue/artifacts/rulesets/bfbc53491f0da7463970260f398daa62.json \
  --replay-dir ~/multivenue/backtest-roots/v7-root --split 70/30
# gates PASS ⇒ stage + commit through the frozen verbs (engine must be live):
uv run claude-worker stage-ruleset \
  --ruleset ~/multivenue/artifacts/rulesets/bfbc53491f0da7463970260f398daa62.json \
  --report  ~/multivenue/artifacts/rulesets/bfbc53491f0da7463970260f398daa62.report.json
uv run claude-worker commit-ruleset --hash bfbc53491f0da7463970260f398daa62
```

(cvfc-v2 `f7d79ce5…` / s1-v2 `0cf7433e…` — the V8-prep S1
CORRECTION: the live cron trades bn-usdm↔bybit-linear funding-spread
PAIRS (sp24/sp3 are cross-venue spreads; verified against slot-4
capture), so s1-v2 is pair rows with `confirm_pair` apr72 — same
sequence whenever their reruns pass, expected weeks out at their
hold/warmup laws.)

**ONE-TABLE LAW:** the VM holds a single active table — families run
TOGETHER only via the MERGED artifact `79eaceec…` (18 rows: xv
$3,000 + cvfc $3,000 + s1 $1,400 legs = $85.6k static rule-7 sum;
the leg-size deltas vs the crons are the group-blind cap's price).
(Corrected 2026-08-30: this runbook still named the pre-retune
`4d5dbe65…` / 19 rows / $99.4k after the §8 log had moved to
`79eaceec…`. The §8 entries above are dated history and stay as
written — the log records what was true when; §9 must not.)
Sequencing consequence of the TABLE-GLOBAL warmup law (V5): the
merged table references apr72 ⇒ 72 h warmup gates EVERY row
(verified: merged backtests 0-trade on the 12.4 h root while xv-v2
alone trades) — commit xv-v2 ALONE at Sep-1/2; switch to merged
only once the root lets the merged REPORT itself pass gates
(≈ a week of funding-era capture, realistically when cvfc clears).
Per-row warmup is a recorded harness-refinement candidate, not V8
scope.

**2. Arm the seed lane** (operator applies; one line + one env):
`scripts/seed-push.sh` is committed and NOT yet wired. Hookup =
append to `scripts/engine-wrapper.sh` after the engine launch line:

```sh
( /bin/zsh "$REPO/scripts/seed-push.sh" & )  # VM2 V8: post-boot seeds
```

and set `MULTIVENUE_SEED_RULESET=~/multivenue/artifacts/rulesets/bfbc53491f0da7463970260f398daa62.json`
in `.env` once xv-v2 is COMMITTED (funding-only seeding runs safely
without it). Verify at next restart: `seeds: sent N frames` in the
wrapper log; engine-side `funding_seeds_applied > 0`.

**3. The parity window** (≥48 h as designed; **operator-ruled 2 HOURS
on 2026-09-02, MVP tempo — see that §8 entry**): VM + crons run in
parallel (nothing to start — the crons already run; the VM trades
once committed). At window end (and daily if longer):

```sh
cd ~/trading-engine-multivenue/claude-worker
uv run python -m claude_worker.parity --window-h 48
```

GREEN = misses-total 0 AND position-disagreements 0 (vm-extras are
informational by design — the VM trades rows the crons never
carried). Run `python -m claude_worker.pnl_report` beside it for the
per-strategy P&L buckets (slot-4 vs slot-5).

**4. Mid-window restart drill (proves seed + re-resolve + re-enter
+ #7b):** pick a moment with an OPEN xv VM position;
`launchctl kickstart -k gui/$(id -u)/com.multivenue.engine` (or wait
for a T2 slot); then verify: (a) #7b re-staged + re-committed the
ruleset (engine log), (b) seed-push ran (wrapper log), (c)
`position_seeds_applied ≥ 1` + the position ages CONTINUOUSLY (no
fresh entry emit for the seeded row), (d) `parity` stays GREEN over
the restart boundary.

**5. Per-family cron bootout — ONLY on explicit operator order,
family by family after ITS parity is green:**

```sh
launchctl bootout gui/$(id -u)/com.multivenue.xv     # after xv parity GREEN
launchctl bootout gui/$(id -u)/com.multivenue.carry  # after cvfc+s1 parity GREEN
# plists archive to ~/multivenue/archive/launchd/ — do not delete.
```

The FUNDING data agent and candles/iv/depth cycles REMAIN (D-4/D-8:
data agents are not strategy carriers).
