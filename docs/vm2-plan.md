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
