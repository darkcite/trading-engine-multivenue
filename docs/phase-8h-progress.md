# Phase 8h — Autonomous research loop: progress log

## 2026-08-16 — Session H0 (Phase 0: design ONLY; no code) — OPENING ENTRY

Authority: `docs/prompts/8h-kickoff.md`. HEAD `39e6542` (8g G7 closing log
commit) verified against the main checkout — RustRover MCP attached FIRST
(`get_project_modules` OK), read-only `git rev-parse/status/log` via the MCP
terminal. Tree clean except untracked `docs/prompts/8h-kickoff.md` (present
at session start). Design-only held: zero cargo invocations, zero builds,
zero boots, zero live sockets — G0's demo exception did not apply (8g left
no open exit criterion). Sandbox used for greps/file-reads and SVG raster
verification only.

Baselines carried (8g close, unchanged this session — nothing ran): alloc
gates 36/36 0 B/op (`--test-threads=1`), workspace nextest 1029/1029, worker
pytest 202, fuzz `ruleset_json` 72.3M/301 s clean.

### Decisions LOCKED (operator, in-session; design §14)

| # | decision | LOCKED |
|---|---|---|
| H-D1 | fill/fee/latency model | strict-cross maker (trade-THROUGH, not touch), per-venue Δ (PM 200 ms / BN·OKX·Deribit 100 ms / HL 600 ms) + fee table (all 0, PM per docs), zero RNG — determinism by construction |
| H-D2 | promotion | FULL AUTO in serve: gates pass ⇒ auto-install `$AI_RULESET_DIR/<hash128>.json` ⇒ stage ⇒ commit (frozen fns), paper only |
| H-D3 | rollback trigger | walk-forward re-backtest of the ACTIVE ruleset each cycle, trailing 24 h (floor 6 h): net ≤ −$100 OR DD ≥ $200 ⇒ disable-5 + restage/commit prior gates-passed hash; paper-fill monitor explicitly rejected for 8h (hot-path/8j creep; `PaperDispatcher` emits no fills) |
| H-D4 | strategist cadence/budget | 6 h cycle, ≤2 Fable-5 calls/cycle, 12/day ceiling, Anthropic prompt caching + SQLite dedupe, usage ledger in state.db, LLM on a bg thread (frames stay single-writer), serve-only (verb surface frozen) |
| H-D5 | data_fetcher REST scope | full plan-§8.2 set: PM Gamma + OKX candles + Deribit chart + HL candleSnapshot, RestBudget ≤60 req/venue/h, fetch-time only |
| H-D6 | multileg v2 timing | design-only (design §16); harness multi-venue + multi-run from day one is LAW regardless |
| H-D7 | market-map ownership | data_fetcher bootstraps + ADDITIVE refresh (operator entries win, never deleted), atomic tmp+rename |
| H-D8 | integration-test extent | both sides, additive: Rust golden/determinism/proptests + pytest real-harness module (skip-if-no-binary); the 202 untouched |
| — | docs commit | LEAVE working-tree; fold into H1 commit 1 (8g-G0 precedent) — operator-confirmed |

Pinned-unless-objection list (schema-1 exact fields only, exit codes,
capture-observed universe, trades=fills, VIRT_T0=1e17 rebase,
BACKTEST_VM_SLOTS=4096, caps 4/32 modeled, candidates dir, additive
`state.stage_ruleset` params, monitor window, no `default_run_fn` change):
design §14 tail.

### Deliverables (all working-tree, this session)

- **A+C** `docs/phase-8h-design.md` — 17 sections, house format; §16 is the
  mandatory MULTILEG READINESS audit+spec (verdict below).
- **B** `docs/phase-8-architecture.svg` — actualized (delta below);
  XML-validated + raster-rendered (cairosvg) to verify it opens.
- **D** `docs/options-support-plan.md` — proposed-NOT-scheduled, Phase 9+,
  P&L-gated; Deribit-first; HL/PM have no options class (HIP-4 =
  digital-option-LIKE payoff, stated explicitly).
- **E** this file.

### Diagram delta (B) — 8g-close truth first, then 8h overlay

Brought to truth: ingress-ai box now carries the §4.2 validator + scratch →
staged-table side path; the ai ring cell carries BOTH rings (AiCmd ⟨1024⟩ +
RuleTable ⟨2⟩ 16 KiB staged→commit); engine header line gains "table-ring
pop PRE-AI-drain → vm staged buffer · on_ai Commit flips in-stream · mask
compose-if-configured (49 observed live) · fills captured pre-on_fill";
slot-6 refusal-probe note added under the strategy chips; slots renumbered
visibly (s0–s5); 8f/8g chips restyled BUILT; core-metrics box lists the §9
family (enabled_mask, vm rows/epoch/fires/orders, ai
staged/committed/rejected, push_fail); capture box gains `ai-cmds.pmlr` +
`engine-fills.pmlr` (header-only-in-paper honesty) + audit-replay ai
section; clob-dispatcher notes paper = count-only/no fills (8j); worker
plane retitled "serve daemon + 7 FROZEN verbs", news_watcher/commander
marked 8f BUILT, state.db strip added (cross-boot seq, registry, events).
8h overlay (amber dashed + legend entry): data_fetcher REST + market-map
line, strategist box (cadence/caps), backtester+monitor (gates + rollback
line), commander auto-promote tag, the `multivenue-engine backtest` harness
box (merge/VIRT_T0/strict-cross/schema-1, "retires G7 shim"), amber
run/report arrows, UDS margin note "promotion + rollback frames ride this
path", usage-ledger + candidates/market-map file strip, status footer
(mask-49, seq-contiguity, hot-path delta ZERO). Renderer-portability fix
recorded: mixed-font-size tspans inside `text-anchor:middle` texts
mis-measure in cairo-class renderers — flattened to single-run texts.

### Multileg verdict (C, design §16)

**Venue dimension SOLVED, leg-count dimension NOT.** D2-amended
venue-explicit legs + venue-agnostic `ctx.submit` + venue-derived
`Order.venue` are live-proven; cross-venue TRIGGERING already works (G7 ran
sym 42 vs ref 7). The wall is single-action-leg: RuleRow's 64 B fully
consumed (43+21 pad), five code sites bake the assumption (emit path, cap
composition invariant, sym-filter scan, rule-8 identity, rule-7 Σ walk),
`SET_VM_SLOTS = 512 = 256×2` sizing. v2 SPECIFIED (not built): leg-table
indirection — 4 pad bytes → `{leg_table_idx u16, leg_count u8, leg_flags
u8}`, `ActionLeg` 32 B × 512 legs (+16 KiB region), grammar `"legs":[…]`
with fail-closed v1 rejection, validator deltas incl. per-sym Σ across ALL
legs, paper partial-emit policy (no cooldown stamp, counted), unwind policy
deferred to 8i RiskGate. Six door-closers PINNED on 8h (design §16.3) — #1
is law: the harness replays MULTI-VENUE merged capture with per-venue
independent fill clocks from day one; #4 reserves `RuleRow._pad` bytes
43..47.

### Hygiene / anomalies

- Git: ZERO git operations beyond read-only verify (no commit, no push, no
  fetch, no branch). **Push-anomaly delta recorded:** the kickoff records
  origin/main local ref `38e599b`; observed this session `f2b3742`. The
  local ref moved between the kickoff's drafting and H0 (not by this
  session — no fetch/push was run here). Recorded, not acted on, per
  standing instruction.
- Working-tree at close: three NEW untracked docs (phase-8h-design.md,
  options-support-plan.md, this file) + the MODIFIED tracked
  phase-8-architecture.svg + the pre-existing untracked
  docs/prompts/8h-kickoff.md — the five files the H1 fold covers
  (fold at H1 — operator-locked above).
- `.env` untouched. No installs on the Mac. Sandbox: file reads, greps,
  `cairosvg` render check only (cairosvg pip-installed IN THE SANDBOX, not
  on the Mac).
- Contract-fidelity checks done against source, not memory: schema-1 field
  list + strict int/float rules re-read from `backtest.py` (`:146-178`);
  gate numbers from `GateThresholds` (`:64-76`); validator caps/bounds from
  `ingress-ai/src/ruleset.rs` (`:89-105`); risk caps from
  `docs/risk-policy.md`. Fuzz inventory re-verified: 22 targets, no PMLR
  target exists (harness reuses hardened readers — no new parser, no new
  fuzz REQUIRED; design §12 pins the if-hand-rolled-then-fuzz rule).
- H0 verification pass: fresh-eyes subagent audit of all four deliverables
  vs sources + the kickoff checklist — verdict PASS-with-minors, 8 findings,
  ALL patched in-session: (1) bare-vm injection must use inherent
  `receive_table`, not the defaulted trait hook; (2) monitor scoring needed
  a mechanism — `0/100` all-OOS form carved into the §3.4 split grammar
  (§8.3/§12 aligned; plain `split` passthrough, no worker change); (3) v2
  leg-table is a SHARED 512 budget — validator rule 9 (Σ leg_count ≤ 512)
  added; (4) hygiene file count corrected (3 new + 1 modified + kickoff);
  (5) v2 pad spend reordered repr(C)-clean (`u8@43, u16@44, u8@46`); (6)
  `schema_version` is equality-checked, not `_strict_int` (harness emits
  literal 1); (7) eight-families enumeration was listing seven (hash
  binding added); (8) H1 hold-stub `trading_days` = 0 per §4.5 (span count
  → stderr only). SVG separately XML-validated + raster-rendered twice
  (cairosvg; mixed-size-tspan portability fix recorded above).
- Facts inherited into the design from G7 verbatim: mask-49
  compose-if-configured, slot-6 refusal probe, worker seq continuity
  (state.db), market-map ABSENT (8h owns it — now H-D7).

### Resume point

Design phase complete; all §14 decisions locked; no code exists yet.
Next session H1 = design §13 checklist item 1 (harness substrate), on
explicit operator go, with the kickoff below. H2–H6 slicing per §13.

---

## H1 kickoff prompt (paste verbatim into a fresh session)

8h implementation — SESSION H1 (design §13 checklist ITEM 1 ONLY: harness
substrate), MAIN CHECKOUT /Users/darkcite/trading-engine-multivenue. HEAD
39e6542 + FIVE working-tree docs from H0 (phase-8h-design.md,
phase-8-architecture.svg, options-support-plan.md, phase-8h-progress.md,
prompts/8h-kickoff.md) — FOLD ALL FIVE into this session's commit 1
(operator-locked at H0; 8g-G0 precedent). Stage-2 status: 8f/8g CLOSED, 8h
design LOCKED (phase-8h-design.md §14: H-D1..H-D8 all option (a); pinned
list §14 tail). Baselines: alloc 36/36 0 B/op (`--test-threads=1`), nextest
1029/1029, worker pytest 202 (UNTOUCHABLE — additive only), fuzz
`ruleset_json` 72.3M clean (no fuzz run due this session: item 1 adds NO
untrusted-bytes parser — design §12; if you find yourself hand-rolling one,
STOP and re-read §12). NO push, NO rebase, NO history rewrite, NO branches,
NO git ops without operator ask (commit 1 above IS operator-authorized;
one-line status after it). Do NOT touch .env. Notes go ONLY to
docs/phase-8h-progress.md (H1 entry: interpretations flagged-for-H2-review,
wiring state, gates at close, hygiene, resume point + H2 kickoff prompt).
Verify get_project_modules against the main checkout FIRST; if the MCP won't
attach, stop.
REQUIRED READING, in order: (1) docs/phase-8h-design.md §1–§5 + §11–§14 +
§16.3 (the six door-closers — #1 multi-venue merge and #4 RuleRow._pad are
law for THIS session); (2) claude-worker/src/claude_worker/backtest.py
(FROZEN: argv `multivenue-engine backtest --ruleset R --replay-dir D
--split 70/30`; schema-1 stdout, `_strict_int` rejects bools; harness
conforms to worker, never vice versa); (3) crates/core-io src/pmlr.rs +
pmlr_reader.rs (+ tests/pmlr_replay.rs — the replay-drive pattern to
generalize); (4) crates/cli/src/bin/multivenue-engine.rs (Cmd enum + the
12-line audit-replay handler = the shim pattern for the backtest handler)
+ crates/cli/src/audit_replay.rs module doctrine header (offline path MAY
allocate); (5) crates/strategy-vm/src/lib.rs (receive_table `:212`, on_ai
flip `:474`, on_tick emit path `:404-460`) + crates/strategy-core Ctx; (6)
crates/ingress-ai/src/ruleset.rs `validate_ruleset` signature `:659` (+
universe_contains — the harness's universe is capture-observed, sorted);
(7) docs/wire-format.md (PMLR header: version ≤ 2, slot kinds; Tick layout).
H1 SCOPE (item 1, nothing more — the FILL MODEL IS H2; do not start it):
`Cmd::Backtest` + `BacktestArgs` (--ruleset, --replay-dir, --split
"70/30" strict N/M parse both ≥10 sum 100; optional flags may be DECLARED
but MUST have design-§4 defaults and may stub); capture discovery
(run-dir OR log root, runs ordered by epoch_ns, cross-checked vs PMLR
headers); k-way merge over per-venue `PmlrReader<Tick>` slices keyed
(ts_ns, venue byte, per-file index), runs never interleaved; VIRT_T0=1e17
rebase (design §3.3 formula — intra-run deltas exact, inter-run gaps =
epoch deltas); capture-observed universe (sorted dedup syms);
`validate_ruleset` reuse (reject ⇒ nonzero exit + stderr, NO schema-1
output); `VmStrategy<BACKTEST_VM_SLOTS=4096>` driven via the inherent
`receive_table` (NOT the trait's `on_ruleset_table` — that is a defaulted
no-op on a bare vm; only StrategySet forwards it) + synthesized
`AiCmd{RulesetCommit, hash128 LE halves}` through `on_ai`, then `on_tick`
per merged record; `BacktestCtx: Ctx`
(submit captures into a preallocated order log — hold-only this session;
now_ns = virtual clock); schema-1 stdout EXACT with hold-model zeros
(net_pnl 0.0, trades 0, trading_days 0 — §4.5 pins it as distinct UTC days
with ≥1 OOS trade, and the hold model trades zero; the capture-span day
count goes to the stderr summary only — DD 0.0, bounds max_order from
emitted orders / 0.0 holds — state precisely in the H1 entry what is real
vs stub) — deterministic fixed-point rendering (i64
1e6, no float wobble); golden-fixture test (synthetic multi-venue multi-run
capture via PmlrWriter in-test), byte-identical determinism test, merge
proptest (total+stable+sorted). GREEN GATES to close: workspace nextest
(old + new), release alloc 36/36 `--test-threads=1` with the false-green
guard (fresh `Compiling bench` in-log or clean -p bench and rerun), worker
pytest 202 untouched-green, `cargo build --release -p cli` links. LANDMINES:
compile/test on the Mac ONLY (sandbox = false greens, pitfall #10);
stale-rmeta playbook (`cargo clean -p <touched>` on impossible import
errors); RustRover MCP execute_terminal_command executeInShell=true ≤45 s —
long runs nohup > /tmp/8h-h1-*.log & then poll; zsh eats bare === in echo;
clap large-enum-variant allow already on Cmd; `#[global_allocator]`
CountingAllocator is process-global — keep new cli tests off the bench
crate; engine/strategy-vm/ingress/core crates are READ-ONLY this session
(cli additions only; if a seam genuinely needs a pub item elsewhere, flag
in the log and take the smallest additive route); schema-1 ints must be
JSON ints (no bools, no floats for counts); do NOT consume events/signals/
ai-cmds/engine-fills files (declared non-consumed v1, design §3.1); do NOT
spend RuleRow._pad; multi-venue merge is LAW (door-closer 16.3.1 — a
single-venue shortcut is a design violation, not a simplification).
SESSION FACTS: projectPath /Users/darkcite/trading-engine-multivenue;
macOS: AF_UNIX sun_path cap, SO_RCVTIMEO EINVAL on peer-closed UDS,
std::thread::scope panic hangs without StopOnDrop, sample <pid> for hangs;
push anomaly KNOWN and MOVED (kickoff recorded origin/main local 38e599b;
H0 observed f2b3742): record, never act; market-map.json ABSENT (H3
scope — irrelevant to H1); mask-49 compose-if-configured + slot-6 refusal
probe are runbook facts (no boots expected in H1; G0 law `cargo build
--release -p cli` applies before ANY boot if one ever runs). If context
runs short: write interim state + exact resume point + relaunch prompt into
docs/phase-8h-progress.md, then tell me.

---

## 2026-08-22 — Session H1 (design §13 item 1: harness substrate) — CLOSED

Authority: the H1 kickoff above + design (LOCKED). RustRover MCP attached
FIRST (`get_project_modules` OK against the main checkout). **Precondition
drift, recorded:** the kickoff assumed HEAD `39e6542` + five working-tree H0
docs to fold; the actual session-start HEAD was `f052da0` ("+ Docks
organized") — the operator had already committed ALL FIVE H0 docs plus the
2026-08-22 doc-reorg (CLAUDE.md rewrite, `docs/arch/` moves, AGENTS/README
touches, a 4-line `core-types` doc tweak) and the tree was CLEAN. The fold
is therefore moot; **commit 1 of this session is code + this log entry
only.** No git op was taken beyond read-only verify until that commit.

### What was built (cli-only; engine/strategy-vm/ingress/core untouched)

- `crates/cli/src/backtest.rs` (NEW, `audit_replay.rs` offline doctrine —
  allocates freely, never engine-loaded): §3.1 capture discovery (run dir OR
  log root; runs epoch-ordered; dir-name epoch cross-checked against every
  PMLR header), §3.2 merge, §3.3 VIRT_T0 rebase, §3.5 capture-observed
  universe (sorted dedup, `SYMBOL_ID_NONE` skipped) + `validate_ruleset`
  REUSE, §3.6 evaluator drive, §5 schema-1 render + stderr summary, §4.3/
  §4.4 model-flag parsing (declared, defaults pinned, explicitly UNUSED).
- `crates/cli/src/bin/multivenue-engine.rs`: `Cmd::Backtest` +
  `BacktestArgs` (`--ruleset/--replay-dir/--split` mandatory = the frozen
  worker argv; `--fee-bps/--latency-ns/--latency-ns-venue/--emit-detail`
  declared per §4/§5). **Backtest-arm tracing is pinned to stderr**
  (parse-then-init split in `main`): the default fmt layer writes to
  stdout, and one stray log line would corrupt the schema-1 contract; the
  Run/PrintConfig/AuditReplay arms keep their historical stdout logging
  byte-for-byte.
- `crates/cli/Cargo.toml`: + `strategy-vm`, + `core-crypto` (moved up from
  dev-deps — the harness hashes the artifact the way the worker does), +
  `proptest` (dev).
- Evaluator drive is the REAL path end to end: `Box<VmStrategy<4096>>` →
  `on_start` → inherent `receive_table` (copy-#2 seam; NOT the defaulted
  trait hook) → synthesized shape-valid `AiCmd{RulesetCommit, px/qty =
  hash128 LE halves}` through `on_ai` — flip VERIFIED (`commits_applied ==
  1` or Internal error, nonzero exit) → `on_tick` per merged record with
  `BacktestCtx` (`now_ns` = record virt ts; `submit` logs orders +
  running max notional, i128-exact). Cooldown stamps carry across run
  boundaries (continuous replay); emit-time semantics are production
  (post-only at mid, row-cap ∧ policy re-clamp — all inside the vm).

### Interpretations — FLAGGED FOR H2 REVIEW (uphold or amend, explicitly)

1. **Merge key carries a file-ordinal backstop:** implemented total key =
   `(ts_ns, venue byte, file ordinal, per-file idx)` with the fixed label
   order pm/bn/okx/rpc/deribit/hl. With one tick file per venue per run the
   ordinal never differs from venue-byte ranking (the §3.2 triple), but it
   keeps the order total on degenerate fixtures. Implemented as a total-key
   sort — equivalent to the §3.2 k-way heap on per-file-monotonic inputs
   and still total/deterministic if a file were ever non-monotonic.
2. **PMLR v2 REQUIRED on consumed tick files:** v1 leaves the venue byte
   undefined and the merge keys on it ⇒ v1 file = Capture error, nonzero.
   (Reader itself accepts ≤2; the harness is stricter. Design was silent.)
3. **Overlapping runs are refused:** if a run's virtual base (VIRT_T0 +
   Δepoch) precedes the previous run's last virt tick, two captures overlap
   in wall time (two writers on one root) — §3.2 "disjoint time windows"
   is ENFORCED (Capture error), not assumed.
4. **OOS window = `[boundary, last]`, boundary inclusive;** boundary =
   `first_virt + span·N/100` (u128 mul, integer floor). Under this
   definition empty-OOS is structurally unreachable while the merge is
   non-empty; the §5 "zero OOS ticks" exit is kept as a tripwire for H2
   bucketing changes.
5. **Run-content rules:** a run dir with NO `*-ticks.pmlr` at all ⇒
   Capture error; all-header-only runs are tolerated (contribute nothing);
   a merged UNION that is empty ⇒ "merged capture stream is empty", nonzero.
   Non-consumption of events/signals/ai-cmds/engine-fills upheld (§3.1).
6. **Split grammar strictness:** ASCII digits only, ≤3 per part, no signs,
   no whitespace, no leading zeros (echo is verbatim ⇒ canonical spellings
   only); `N+M==100 ∧ both ≥10`, plus the carved `0/100`. `100/0` rejected.
7. **Epoch-tied run dirs** order by `(epoch_ns, name)` — deterministic;
   the overlap guard then rejects any real collision.
8. **Table epoch stays 0** as `validate_ruleset` leaves it (production
   epochs are the side path's to stamp; the flip keys on hash128 only).
9. **clap usage errors exit 2** (clap default), harness errors exit 1 —
   both nonzero, both mapped by the worker to `BacktestError`. §5 needs
   only "nonzero"; noted for precision.

### schema-1 wiring state — real vs stub (kickoff mandate)

REAL now: `schema_version` (literal 1), `ruleset_hash` (core-crypto SHA-256
of the file bytes, full hex), `split` (argv echo, validated before embed),
`bounds.max_order_notional_usd` (max px·qty/1e6 over ALL emitted orders —
§4.6 full-window — deterministic fixed-point i64×1e6 renderer, zero float
round-trips anywhere). REAL ZEROS BY VACUITY (not placeholders): `oos.trades`
/ `trading_days` / `net_pnl_usd` / `max_drawdown_usd` and the two position
bounds — the hold model synthesizes no fills, so 0 is the true §4.5 value;
the capture-span UTC-day count goes to stderr only. STUB (H2 scope): the
whole §4 model — open-order table + 4/32 caps, strict-cross fills, fees, Δ
activation, fixed-point equity/DD, end-of-window liquidation, OOS
accounting bucketing, `--emit-detail` sidecar (flag parsed, stderr notes
"deferred"), and `on_fill` feedback (§3.6.4 — nothing to feed yet).

### Tests added (27; §12 H1 slice)

In-module (14): split grammar (contract + carved form + 19 reject shapes),
model-flag defaults/layering/malformed, run-dir name parse, fixed-point
renderer (incl. i64::MAX), §3.2 order unit + **merge proptest
(total/stable/sorted over arbitrary per-venue streams, adversarial input
order)**, §3.3 rebase arithmetic (cross-run monotonicity), universe
derivation, ctx notional/log. Integration `crates/cli/tests/
backtest_harness.rs` (13): **golden two-run two-venue fixture** (PmlrWriter
in-test; bn mid 0.56 / pm mid 0.50, 80 bps row ⇒ hand-computed: 5 merged,
universe {7,42}, virt window/boundary exact, evals 3 / fires 2 / emits 2 /
$50.0 max-order, 1 UTC day), single-run-dir form, carved 0/100 echo,
**byte-identical determinism** (schema-1 AND summary), **REAL binary on the
frozen worker argv** (stdout = the schema-1 line ALONE + nonempty stderr),
reject/bad-split via binary ⇒ nonzero + EMPTY stdout, `Reject(Symbol)` via
lib, empty root, header-only run, epoch cross-check mismatch, crafted
v1-header refusal, unreadable ruleset.

### Gates at close (all on the Mac; MCP terminal, nohup+poll)

- workspace nextest **1056/1056** (baseline 1029 + 27 new), 0 skipped.
- release alloc **36/36** 0 B/op `--test-threads=1`, false-green guard
  applied (`cargo clean -p bench`; exactly one fresh `Compiling bench` in
  `/tmp/8h-h1-alloc.log`).
- worker pytest **202 untouched-green** (zero worker files changed).
- `cargo build --release -p cli` links (backtest arm included).
- Fuzz untouched by design §12: no new untrusted-bytes parser exists —
  capture via hardened `PmlrReader`, candidate via `validate_ruleset`,
  argv/split via clap + strict integer checks (unit-tested).

### Hygiene

- Cargo on the Mac ONLY (pitfall #10); zero sandbox cargo. No boots, no
  live sockets, no `.env` read or write, no fuzz run (none due).
- Diff scope is exactly: `crates/cli/{Cargo.toml, src/lib.rs,
  src/backtest.rs (NEW), src/bin/multivenue-engine.rs,
  tests/backtest_harness.rs (NEW)}` + this file. No seam was needed in any
  read-only crate (kickoff escape hatch unused). New tests live in cli —
  none in bench (CountingAllocator isolation upheld).
- `RuleRow._pad` untouched (door-closer §16.3.4). Multi-venue merge is the
  implementation, not an option (§16.3.1): the golden fixture is two-venue
  from day one, and `BacktestCtx` accepts any venue byte (§16.3.6).
- Push anomaly: unchanged posture — recorded, not acted on; no fetch/push
  this session. CLAUDE.md deliberately NOT edited (notes go only here; its
  "NEXT SESSION = H1" line is superseded by this entry per the authority
  chain).
- Live doctrine (pitfall #11): the harness has NOT yet seen a live-capture
  run dir — the real-capture smoke stays OWED and lands with H2+ per §12
  tail ("probe fixtures are not proof").

### Resume point

Item 1 CLOSED (this commit). Next session H2 = design §13 item 2: the §4
model in full. The H2 kickoff prompt below is ready to paste; it opens with
the mandatory review of the nine H1 interpretations above.

---

## H2 kickoff prompt (paste verbatim into a fresh session)

8h implementation — SESSION H2 (design §13 checklist ITEM 2 ONLY: the §4
fill/fee/latency model + accounting), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue. Stage-2 status: 8f/8g CLOSED; 8h
H0 design LOCKED, H1 (harness substrate) CLOSED — HEAD = the H1 commit
(cli::backtest exists, schema-1 skeleton live, hold-model zeros). Baselines
NOW: workspace nextest 1056/1056, release alloc 36/36 0 B/op
(`--test-threads=1`), worker pytest 202 (UNTOUCHABLE — additive only), fuzz
`ruleset_json` 72.3M clean (no fuzz run due UNLESS you hand-roll a parser —
design §12 says do not). NO push, NO rebase, NO history rewrite, NO
branches, NO git ops without operator ask (ONE closing commit IS
authorized; one-line status after). Do NOT touch .env. Notes go ONLY to
docs/phase-8h-progress.md (H2 entry: interpretation-review verdicts, model
math decisions, gates, hygiene, resume point + H3 kickoff prompt). Verify
get_project_modules against the main checkout FIRST; stop if no attach.
FIRST TASK, before any code: review the NINE H1 interpretations in the H1
entry of docs/phase-8h-progress.md — uphold or amend each, explicitly, in
the H2 entry (amendments must not break the frozen worker contract or the
H1 tests without stating why the test changes).
REQUIRED READING, in order: (1) docs/phase-8h-design.md §4 ENTIRE + §5 +
§12 + §16.3 (door-closers 2/6: fill engine keyed by namespaced SymbolId,
any venue byte); (2) docs/phase-8h-progress.md H1 entry (wiring state:
what is real vs stub — you are replacing the stubs); (3)
crates/cli/src/backtest.rs (the substrate you extend: BacktestCtx submit/
order-log seam, HoldReport, boundary_virt_ns, ModelParams already parsed
and UNUSED); (4) claude-worker/src/claude_worker/backtest.py (FROZEN;
GateThresholds numbers are what the report must be judged against;
run_backtest/parse_harness_report/write_report for the pytest side); (5)
docs/risk-policy.md (caps the §4.1 open-order model mirrors); (6)
docs/local-setup.md (gains the "release binary on PATH" runbook line —
design §15.3). H2 SCOPE (item 2, nothing more — data_fetcher is H3): §4 IN
FULL — preallocated open-order table (max 4/sym, 32 total; beyond-cap emit
= counted + dropped, conservative divergence documented), per-venue Δ
activation (t_active = t_emit + Δ_venue; §4.4 defaults already in
ModelParams), strict-cross maker fills (BID fills iff ask_px < P at a tick
of s with virt ts ≥ t_active; fill px = P; qty = min(remaining, displayed);
mirror for asks; partials rest; NO touch-fill, NO queue credit, zero RNG),
§4.3 fee charge on fill notional, §4.5 fixed-point i64×1e6 accounting
(per-sym signed position + average-cost basis, realized on reducing fills,
fees at fill, equity = realized + Σ unrealized on every fill AND every tick
of a held sym, OOS max peak-to-trough DD, end-of-replay mark-out at last
mid into net_pnl, trades = OOS fill count, trading_days = distinct UTC days
with ≥1 OOS trade via the §3.3 wall mapping), §4.6 bounds observed maxima
(order/symbol/total — full window), §3.4 split bucketing (replay
continuous, accounting bucketed on boundary_virt_ns), on_fill feedback into
the vm per §3.6.4, --emit-detail sidecar (versioned separately, operator
surface) + refreshed stderr summary, schema-1 gains REAL oos numbers
(fixed-point renderer already exists — reuse it, no floats). TESTS: golden
fixture EXTENDED to known-P&L (hand-computed net/DD/trades/days asserted
EXACTLY — plan §11), fill proptests (§12: fill px never better than the
crossing book, qty ≤ displayed, fees ≥ 0, cash+position conservation),
unit tests for Δ activation, fee charge, split-boundary bucketing,
open-order caps, liquidation, partial fills; determinism test stays
byte-identical; PYTHON side (additive, 202 untouched):
claude-worker/tests/test_backtest_real.py per design §12 — drives the REAL
release binary through backtest.run_backtest over a committed golden
fixture, skipif binary absent; plus docs/local-setup.md runbook line and
the §15.1 stale-comment docs fix may ride this commit. THE G7 §5.1 SHIM
SEAM IS RETIRED THIS SESSION (design §13.2) — the fake-binary pattern
survives only inside the frozen 202 as a mock. GREEN GATES to close:
workspace nextest (1056 + new), release alloc 36/36 `--test-threads=1`
with the false-green guard, worker pytest 202 + new real-harness module
green (or cleanly skipped where the binary is absent), `cargo build
--release -p cli` links, and the OWED pitfall-#11 live-capture smoke: run
the release harness once over a REAL run dir from the box
(~/multivenue/logs) and record the stderr summary + exit code in the H2
entry. LANDMINES: Mac-only cargo (pitfall #10); stale-rmeta playbook;
RustRover MCP ≤45 s window — nohup > /tmp/8h-h2-*.log & then poll; zsh
eats bare === ; engine/strategy-vm/ingress/core crates READ-ONLY (cli +
claude-worker/tests + docs only; flag any seam need in the log, smallest
additive route); keep new Rust tests out of bench; schema-1 counts are
JSON ints (no bools/floats); fixed-point everywhere — floats only at
render, and only via the existing renderer; do NOT consume events/signals/
ai-cmds/engine-fills; do NOT spend RuleRow._pad; per-venue independent
fill clocks (§4.7) are LAW — no cross-venue serialization of fills.
SESSION FACTS: projectPath /Users/darkcite/trading-engine-multivenue;
macOS: AF_UNIX sun_path cap, SO_RCVTIMEO EINVAL on peer-closed UDS,
std::thread::scope panic hangs without StopOnDrop, sample <pid> for hangs;
push anomaly KNOWN (38e599b → f2b3742 across H0): record, never act;
market-map.json ABSENT (H3); no boots expected beyond the smoke — G0 law
(`cargo build --release -p cli` before ANY boot) applies to it. If context
runs short: write interim state + exact resume point + relaunch prompt
into docs/phase-8h-progress.md, then tell me.
