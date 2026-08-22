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

---

## 2026-08-22 — Session H2 (design §13 item 2: the §4 fill/fee/latency model + accounting) — CLOSED

Authority: the H2 kickoff above + design (LOCKED). RustRover MCP attached
FIRST (`get_project_modules` OK). Session-start HEAD `3ad40a9` (the H1
commit), tree clean. Diff scope is exactly: `crates/cli/src/backtest.rs`,
`crates/cli/src/backtest/fill.rs` (NEW), `crates/cli/tests/
backtest_harness.rs`, `claude-worker/tests/test_backtest_real.py` (NEW),
`claude-worker/tests/fixtures/backtest-real/` (NEW, committed golden
capture), `docs/local-setup.md`, `docs/arch/phase-8g-design.md` (§15.1
ride), this file. Read-only crates untouched (engine/strategy-vm/ingress/
core — kickoff escape hatch unused). No bin changes needed: every §4 flag
was already H1-declared and wired.

### H1 interpretation review — all NINE UPHELD (kickoff first task)

1. **Merge-key file-ordinal backstop — UPHELD.** H2 consumes the merged
   stream as-is; nothing in §4 touches merge order.
2. **PMLR v2 required — UPHELD.** Unchanged; the fill engine additionally
   keys Δ/fees off the ORDER venue byte (sym-derived), not the tick's.
3. **Overlapping runs refused — UPHELD.** Activation (`virt ≥ t_active`)
   depends on cross-run virtual monotonicity; overlap would corrupt it.
4. **OOS = `[boundary, last]`, boundary inclusive — UPHELD**, and §3.4
   got its teeth: accounting buckets ORDERS by emit virt ts ≥
   `boundary_virt_ns` (fills/marks follow their order). The zero-OOS-ticks
   tripwire stays live.
5. **Run-content rules — UPHELD.** Non-consumption of events/signals/
   ai-cmds/engine-fills held (§3.1).
6. **Split grammar strictness — UPHELD** (untouched).
7. **Epoch-tied run-dir order — UPHELD** (untouched).
8. **Table epoch stays 0 — UPHELD** (flip keys on hash128 only).
9. **clap exit 2 / harness exit 1 — UPHELD.** One new exit-1 Usage path:
   an unwritable `--emit-detail` (sidecar written BEFORE stdout, so
   failure ⇒ nonzero with EMPTY stdout — "exit 0 ⇔ trustworthy report
   printed" preserved).

No H1 test changed semantically. One mechanical rename inside
`backtest.rs` tests: the private `HoldReport` became `ReportValues`
(same fields, real numbers) — the schema-1 shape test renamed its local
accordingly. The H1 golden fixture now runs THROUGH the live model and
its schema-1 stays byte-identical (its resting bids at 0.50 never see an
ask strictly below — the zeros are now "no cross ever happened" zeros).

### What was built (design §4 IN FULL; `cli::backtest::fill`, NEW module)

Open-order table `[OpenOrder; 32]` (§4.1: 4/sym + 32 caps, risk-policy
mirror; preallocated, emit-ordered, compaction preserves FIFO) →
strict-cross maker matcher (§4.2) → per-venue Δ activation (§4.4) →
maker-fee charge (§4.3) → two fixed-point books + DD/bounds trackers
(§4.5/§4.6) → `on_fill` feedback (§3.6.4) → `--emit-detail` sidecar +
refreshed stderr summary (§5/§10) → REAL `oos` numbers in schema-1 via
the existing renderer. Zero floats anywhere; zero RNG anywhere.

### Model math decisions (recorded; conservative doctrine throughout)

1. **Per-record order:** mark update → fill pass → `vm.on_tick` →
   `vm.on_fill` (per design §3.6's 3→4 listing) → order intake after
   every vm callback. An order can NEVER fill on its emitting tick, even
   with `--latency-ns 0` (its record's fill pass already ran).
2. **Two-sided ticks only, for marks AND fills** (8e preopen lesson):
   `ask_px = 0` must not read as "ask below our bid"; every fill needs
   the mark its record just wrote. Fewer fills = conservative.
3. **Shared displayed budget per side per tick, FIFO by emit seq:** two
   of our resting orders can never double-claim one printed size
   (conservative refinement of §4.2's per-order `min`; Σ fills per side
   per tick ≤ displayed, proptest-pinned).
4. **Cap-dropped emits:** counted (`rejected 31+0` in the live smoke —
   see below) and refused fills, but the vm still sees `Ok` from
   `submit` — production-faithful: today's engine has no cap wall, so
   cooldown stamping must match production. Per-sym cap checked before
   total (risk-policy table order). Divergence documented in module docs.
5. **Unroutable venue byte** (≥ 5 — no Δ/fee row, e.g. Ai): counted +
   dropped; a venue that cannot execute must not fill.
6. **Two books:** FULL (every fill; §4.6 bounds — breach anywhere
   disqualifies) and OOS (fills of OOS-emitted orders only; the schema-1
   verdict — the P&L of a strategy starting flat at the boundary).
   IS-emitted orders warm the vm and the full book but never leak into
   `oos` even when their fills land after the boundary (unit-pinned).
7. **Cost basis:** signed `cost i128×1e12`; average never materialized.
   The truncated average-cost `removed` quantum leaves `cost` and enters
   `realized` as the SAME value ⇒ `realized + unrealized` is EXACTLY
   cash-conserving regardless of truncation — the §12 conservation
   proptest asserts the identity with zero tolerance.
8. **Fees:** maker bps, `ceil` (a fee never rounds in our favor); taker
   column parsed and reserved (maker-only model has no taker fills).
9. **Render directions (×1e12 → ×1e6, once, at the edge):** net FLOOR
   (toward −∞ — profit never overstated), drawdown + symbol/total bounds
   CEIL (risk never understated); `max_order` keeps the H1 emit-time
   floor (pinned by the shipped golden; divergence < 1e-6 USD).
10. **Equity sampling:** OOS book sampled after each OOS fill and on each
    two-sided tick of a held sym, at THAT tick's mid — the mark precedes
    the fill pass, so a fill against a falling book is booked at the
    fallen mid (adverse-selection-honest; the golden's −$4.375 trough
    exists precisely because of this ordering).
11. **DD peak seeds at 0** (the OOS book starts flat at the boundary; a
    pure loss is its own drawdown).
12. **Fill identity:** `Fill.order_id = Order.client_oid`, fill ts = the
    record's virt ts. Per-venue independent fill clocks upheld (§4.7):
    activation compares each order's own `t_active` against the one
    merged timeline — no cross-venue serialization anywhere
    (door-closers §16.3.2/§16.3.6 hold; `bounds` composes across
    namespaced syms venue-agnostically, unit-pinned).

### Golden known-P&L fixture (plan §11) + the committed Python copy

New two-run/two-venue fixture (`build_pnl_capture`): two `level_breach`
rows (buy ≤ 0.42 / sell ≥ 0.60, $50 caps) over a scripted pm-42 path;
bn-7 rides as merge ballast. Run 0 all-IS (warms vm + full book: an IS
buy 125 @ 0.40, an IS partial sell 30 @ 0.625 ⇒ full-book realized
+$6.75, sidecar-asserted); run 1 all-OOS, starting 1970-01-02 23:59:59.2
so its two OOS fills straddle midnight (trades 2, trading_days 2).
Hand-computed and asserted EXACTLY: **net +5.0, DD 4.375 (deeper-buy
trough), bounds 50.0 / 96.8 / 96.8** (peak = 220 held × 0.44 last mark),
canceled_end 1, peak_open 2. A fee-override variant (`pm:50:50`) asserts
net 4.75 / DD 4.625 / fees 0.25 exactly (ceil math). Byte-exact copies
live at `claude-worker/tests/fixtures/backtest-real/` (5 files, ≤ 333 B
each), regenerated ONLY via the `#[ignore]`d
`regenerate_committed_python_fixture` test and drift-guarded by
`committed_python_fixture_matches_the_generator_byte_for_byte`
(PmlrWriter output is deterministic).

### Tests added (+25 Rust, +2 Python)

- `fill.rs` in-module (19): conversion floor/ceil, fee-ceil, Δ-activation
  boundary (±1 ns), strict-cross vs touch, one-sided refusals, partial
  fill (2 trades), shared-budget FIFO, per-sym + total caps, unroutable,
  maker-fee equity, §3.4 bucketing, average-cost reduce + cross-zero,
  liquidation mark-out, DD tracker, deeper-buy trough, cross-venue
  bounds composition, and TWO §12 proptests: fill invariants (px == P,
  strict-through only, Σ ≤ displayed per side, no fill before
  activation, fees ≥ 0) and the EXACT cash+position conservation
  identity (incremental unrealized == from-scratch, equity == cash −
  fees + Σ pos×mark).
- `backtest_harness.rs` integration (+6, +1 ignored): known-P&L exact,
  fee override exact, sidecar (versioned, deterministic, full-realized
  6.75, schema-1 gains NO keys), P&L determinism byte-identical, real
  binary over the frozen argv, committed-fixture drift guard (+ regen).
- Python `tests/test_backtest_real.py` (+2, module-level skipif when the
  binary is off PATH — skip reason names the runbook): the real release
  binary through the FROZEN `run_backtest` over the committed fixture —
  all eight harness numbers exact, gate matrix over real numbers
  (pnl ✓ / trades ✗ 2<50 / days ✓ 2≥2 / dd ✓ / bounds ✓ ⇒ all_passed
  False, report still written), and nonzero-exit ⇒ `BacktestError`.
  **The G7 §5.1 shim seam is RETIRED** (design §13.2): the real binary
  now proves the frozen argv + schema-1 contract end to end; the
  fake-binary pattern survives only inside the frozen 202 as a mock.

### Gates at close (all on the Mac; MCP terminal, nohup+poll)

- workspace nextest **1081/1081** (baseline 1056 + 25), 1 skipped (the
  `#[ignore]` regen). nextest flagged `bench::alloc_assertions
  cross_arb_on_tick_is_zero_alloc` LEAK (passing) — bench is UNTOUCHED
  by H2; recorded as pre-existing/environmental, not acted on.
- release alloc **36/36** 0 B/op `--test-threads=1` — with a REAL
  false-green caught and killed: `cargo clean -p bench` (no `--release`)
  removed 61 files yet the release test bin survived (`Finished` in
  0.13 s, NO `Compiling bench` line). Guard escalation that works on
  this toolchain: **`cargo clean -p bench --release`**, then the rerun
  showed `Removed 15 files` + a fresh `Compiling bench` + 36/36. The
  CLAUDE.md guard text still says plain `-p bench` — folded here per
  notes-go-only-here; fix CLAUDE.md at the next phase-boundary edit.
- worker pytest **204** green with the release binary on PATH (202
  frozen untouched + 2 real-harness); binary-absent path verified
  separately: 2 skipped cleanly, exit 0.
- `cargo build --release -p cli` links (fresh `Compiling cli`, 19.87 s)
  — G0 law satisfied BEFORE the smoke.
- Fuzz untouched (design §12): no new untrusted-bytes parser — the
  sidecar is a writer, capture stays on hardened `PmlrReader`, candidate
  on `validate_ruleset`, argv on clap + strict ints.

### Pitfall-#11 live-capture smoke (OWED from H1 — PAID)

`./target/release/multivenue-engine backtest --ruleset
claude-worker/tests/fixtures/backtest-real/golden-ruleset.json
--replay-dir ~/multivenue/logs/run-1786874540193462000 --split 70/30`
⇒ **exit 0**, schema-1 alone on stdout (`oos` zeros, max_order 50.0).
stderr summary (verbatim numbers): capture 1 run, **4856 merged ticks**
(pm=82 bn=4774), universe 2 syms, 1 UTC day; window virt
[100000000000000000, 100000194978925417] boundary 100000136485247791
(1012 OOS ticks); vm evals=164 fires=35 orders_emitted=35; orders
accepted is=4 oos=0, **rejected_caps=31+0**, canceled_end=4, peak_open=4
(per-sym 4); fills 0. The committed ruleset VALIDATED against the live
universe (pm sym 42 present on the box), the vm fired 35× on real data,
and the §4.1 per-sym cap visibly enforced the conservative wall (the
0.42/0.60 levels never strictly crossed ⇒ zero fills — correct under
strict-cross). Real PMLR bytes, real venue mix, real sizes: the harness
is live-proven for item 2.

### Hygiene

- Cargo/pytest on the Mac ONLY (pitfall #10); the Linux sandbox was not
  used at all this session. No engine boots (the smoke is an offline
  binary run; G0 relink preceded it). No `.env` read or write. No git op
  until the closing commit (authorized). Push anomaly: unchanged
  posture; no fetch/push.
- Read-only crates untouched — `git status` at close lists ONLY the
  files in the diff-scope line above. New Rust tests live in cli, none
  in bench (CountingAllocator isolation upheld). `RuleRow._pad`
  untouched (§16.3.4).
- Docs rides landed per kickoff: `docs/local-setup.md` gained the
  "Release binary on PATH (8h backtest harness)" runbook section
  (design §15.3 — PATH resolution stays the contract); `docs/arch/
  phase-8g-design.md` §4.1 stale clause reworded per design §15.1
  (mechanism was always right; wording fixed, edit annotated in place).
  CLAUDE.md deliberately not edited (H1 precedent; authority chain
  points here).

### Resume point

Item 2 CLOSED (this commit). Next session H3 = design §13 item 3:
`data_fetcher` completion (§6 in full — Python only). The H3 kickoff
prompt below is ready to paste.

---

## H3 kickoff prompt (paste verbatim into a fresh session)

8h implementation — SESSION H3 (design §13 checklist ITEM 3 ONLY:
data_fetcher completion, §6 IN FULL), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue. Stage-2 status: 8f/8g CLOSED;
8h H0 design LOCKED, H1 CLOSED, H2 CLOSED — HEAD = the H2 commit
(cli::backtest::fill live: strict-cross model + accounting, REAL oos
numbers in schema-1, --emit-detail sidecar, committed golden fixture
claude-worker/tests/fixtures/backtest-real/ + test_backtest_real.py; the
G7 fake-binary shim seam RETIRED). Baselines NOW: workspace nextest
1081/1081 (+1 ignored fixture-regen; nextest may flag
bench::alloc_assertions cross_arb_on_tick_is_zero_alloc LEAK —
pre-existing, passing, recorded H2), release alloc 36/36 0 B/op
(`--test-threads=1`; false-green guard CORRECTED in H2: `cargo clean -p
bench --release` — plain `-p bench` does NOT remove the release test bin
on this toolchain; always confirm a fresh `Compiling bench` in-log),
worker pytest 204 (202 frozen UNTOUCHABLE + 2 real-harness; 204 is the
new stay-green), fuzz `ruleset_json` 72.3M clean (no fuzz run due UNLESS
you hand-roll a Rust untrusted-bytes parser — §12; strict field-checked
JSON parsing of REST responses in PYTHON follows the labeling.py
precedent and implies NO fuzz). NO push, NO rebase, NO history rewrite,
NO branches, NO git ops without operator ask (ONE closing commit IS
authorized; one-line status after). Do NOT touch .env — `.env.example`
IS in scope (§7.5 keys land here). Notes go ONLY to
docs/phase-8h-progress.md (H3 entry: decisions, tests, gates, hygiene,
resume point + H4 kickoff prompt). Verify get_project_modules against
the main checkout FIRST; stop if no attach.
REQUIRED READING, in order: (1) docs/phase-8h-design.md §6 ENTIRE + §7.5
(the three env keys: CLAUDE_WORKER_STRATEGIST_INTERVAL_S=21600,
CLAUDE_WORKER_STRATEGIST_DAILY_CAP=12, CLAUDE_WORKER_REST_BUDGET_PER_H=
60) + §14 pinned tail (market-map SymbolId-stability caveat is RECORDED
design — do not "fix" it); (2) docs/phase-8h-progress.md H2 entry
(baselines + landmine deltas); (3) claude-worker/src/claude_worker/
features.py (the injected-get_fn seam the new fetchers.py must preserve
— the module doctrine "never imports an HTTP client"), cli.py (fetch
verb wiring; the cli.py "no venue URL consumers exist until 8h"
deviation note RETIRES this session; load_market_map reader UNTOUCHED —
its shape is the write-side contract), labeling.py (strict-parse
precedent: malformed ⇒ logged skip, never a crash), state.py (events
surface), and the pinned RestBudget (grep it — wired for REAL this
session: 60 req/venue/h default, fixed-window try_acquire,
skipped_total surfaced in fetch output); (4) docs/local-setup.md worker
env-keys paragraph. H3 SCOPE (item 3, nothing more — strategist is H4):
§6.1 the FOUR public keyless REST consumers in a NEW fetchers.py handed
the client (PM Gamma markets by token id/slug; OKX
/api/v5/market/candles; Deribit /api/v2/public/
get_tradingview_chart_data; HL POST /info {type:candleSnapshot}), each
a strict field-checked parser writing per-sym OHLCV/metadata feature
files beside the replay-derived ones, every call RestBudget-gated,
`--no-rest` finally REAL (skips all four); §6.2 market-map.json
ownership: derive the observed universe from the LATEST run's tick
capture, resolve names (PM ordinals via Gamma question/slug, CEX as
<venue>:<instrument>, HIP-4 (yes,no) pairs when present — none live
today), BOOTSTRAP when absent ({"markets":{...},"hip4_pairs":[...]}
complete), ADDITIVE refresh when present (add missing names only;
operator entries NEVER deleted or overwritten; a conflict is REPORTED in
fetch output and left alone), atomic tmp + os.replace in the same dir;
`.env.example` gains the three §7.5 keys (+ the absolute-binary-path
commentary option per §14 tail if absent). TESTS (additive; the 202
frozen, 204 stays green): design §12 fetchers rows — per-venue consumers
against injected get_fn fixtures (happy + malformed-skip +
budget-exhausted), RestBudget acquire/skip arithmetic, --no-rest real,
market-map bootstrap / additive refresh / conflict report /
operator-entry preservation / atomic write (tmp+rename observed); NO
live API calls in tests (house rule — mock at the seam). GREEN GATES to
close: worker pytest 204 + new ALL green, workspace nextest 1081/1081
untouched-green, release alloc 36/36 `--test-threads=1` with the
corrected guard, `cargo build --release -p cli` links (no Rust changes
expected — the gates are the pitfall-#9 regression net). LANDMINES:
Mac-only cargo/pytest (pitfall #10); RustRover MCP ≤45 s window — nohup
> /tmp/8h-h3-*.log & then poll, and remember a `sleep N` INSIDE the
polled command counts against the window; zsh eats bare ===; full
`import x` only (no `from x import y`); engine/strategy-vm/ingress/
core/cli crates READ-ONLY (claude-worker + docs only); the 7-verb
surface is FROZEN (fetch grows internals only, no new verbs); the
Anthropic SDK stays constructed inside serve only (H3 needs NO SDK);
do NOT touch the committed backtest-real fixture (byte-pinned by the
Rust drift-guard; regen only via the ignored test). SESSION FACTS:
projectPath /Users/darkcite/trading-engine-multivenue; macOS: AF_UNIX
sun_path cap, SO_RCVTIMEO EINVAL on peer-closed UDS, std::thread::scope
panic hangs without StopOnDrop, sample <pid> for hangs; push anomaly
KNOWN (38e599b → f2b3742 across H0): record, never act; market-map.json
ABSENT on the box — H3 owns creating it (fixtures drive the tests; a
live one-shot `fetch` smoke against the public endpoints is
operator-gated, budget-capped, at operator discretion); no engine boots
expected (G0 law applies if one ever runs). If context runs short: write
interim state + exact resume point + relaunch prompt into
docs/phase-8h-progress.md, then tell me.

---

## 2026-08-22 — Session H3 (design §13 item 3: data_fetcher completion, §6 in full) — CLOSED

Authority: the H3 kickoff above + design (LOCKED). RustRover MCP attached
FIRST (`get_project_modules` OK). Session-start HEAD `1ed6017` (the H2
commit), tree clean. Diff scope is exactly:
`claude-worker/src/claude_worker/fetchers.py` (NEW),
`claude-worker/src/claude_worker/cli.py` (fetch internals + HTTP wrappers
+ module-docstring map-ownership clause; verb surface UNTOUCHED),
`claude-worker/tests/test_fetchers.py` (NEW), `.env.example` (the three
§7.5 keys + §14 PATH commentary), `docs/local-setup.md` (env-keys
paragraph ride), this file. Python + docs only — engine/strategy-vm/
ingress/core/cli **crates** untouched (read-only law held); the
committed backtest-real fixture untouched; `.env` untouched.

### The descriptor question — the one §6 gap that needed an
### interpretation, and how it was closed

§6.2 says "resolve names — PM ordinals via Gamma (question/slug), CEX
syms as `<venue>:<instrument>` from the instrument metadata already
flowing through discovery-shaped REST" — but the capture carries NO
instrument identity: a run dir is pmlr ticks + raw taps only (verified
live on `run-1786874540193462000`), engine ordinals are allocated from
BOOT FLAGS (`paper.rs`: "ordinals follow flag order", 1-based;
`--binance-sym-id 7`, `--polymarket-sym-id 42` are direct ids), and boot
flags are invisible to the worker. Interpretation LOCKED this session
(H4 review welcome, H1/H2 precedent):

**The market-map names themselves are the descriptors.** A PM entry
whose NAME is an all-digit Gamma token id (10..80 digits — the Rust
`PM_TOKEN_RUN_MIN`/`PM_TOKEN_MAX` mirror) or a slug seeds the Gamma
consumer for its sym; `okx:<instId>` / `deribit:<instrument>` /
`hyperliquid:<coin>` names drive the candle consumers. Unseeded observed
syms are REPORTED unresolved in fetch output (with the exact
name-format hint), never guessed — with ONE exception: the clap-default
mirror `binance:btcusdt` ↔ (VENUE_BINANCE, sym 7), resolved with zero
REST. A non-default boot then surfaces as a §6.2 conflict report —
exactly the §14 SymbolId-stability caveat's visibility (recorded design,
not fixed). Operator workflow: seed the token id (or slug) as a map name
once; fetch resolves and ADDS the question + slug names additively (the
slug keeps future fetches seeded — Gamma cannot be queried by question);
the seed and every operator entry survive verbatim.

### Numbered H3 interpretation calls (all pinned by tests)

1. **Descriptors = map names** (above). `derive_targets` = map names ∩
   observed universe of the LATEST run, name-sorted (deterministic
   request order), one Gamma seed per sym (first sorted wins).
2. **`CLAUDE_WORKER_REST_BUDGET_PER_H` reads at the fetch seam, NOT
   `BaseConfig`**: the frozen 202 construct `ServeConfig(...)` directly
   (`test_llm.py:47`, `test_daemon.py:57`), so the config dataclass
   field tuple is itself a frozen surface (a defaulted BaseConfig field
   would also be a dataclass TypeError under ServeConfig's non-default
   key). Strict parse: malformed/negative ⇒ ValueError ⇒ exit 2.
3. **Venue REST hosts reuse the engine's existing `.env` keys**
   (`POLYMARKET_GAMMA_HOST`, `OKX_REST_HOST`, `DERIBIT_REST_HOST`,
   `HYPERLIQUID_API_HOST`) with the same defaults — zero new keys
   beyond §7.5's three.
4. **`--no-rest` is REAL and scoped to REST**: all four consumers skip
   (injected fns never touched); §6.2 map ownership still runs on every
   fetch (zero-REST resolutions only). Bootstrap "complete" = the
   complete `{"markets":{},"hip4_pairs":[]}` SHAPE; unresolvable syms
   stay OUT of `markets` (reported), no placeholder names.
5. **Lazy client construction**: the httpx client is built on the FIRST
   actual request, so zero targets ⇒ `_make_http_client` never invoked.
   This is the invariant that keeps the frozen fetch tests hermetic —
   including `test_session_scripted`'s SUBPROCESS fetch, where
   monkeypatching cannot reach — and it is pinned by
   `test_run_secondary_zero_targets_never_touches_client` +
   `test_cli_fetch_no_descriptors_zero_rest_and_bootstrap`.
6. **BN has no candle consumer** — H-D5's locked set is PM/OKX/Deribit/
   HL; binance syms get name resolution only.
7. **HIP-4 pairs: preserved, never fabricated** — none derivable live
   (design §6.2's own record); operator pairs round-trip verbatim; the
   HL outcome-metadata derivation lands when outcome coins exist.
8. **Strictness split per response shape**: Gamma/OKX/HL = per-row skip
   + count (rows have identity); Deribit's columnar body = any bad cell
   rejects the whole body. All bool-rejecting numeric coercion
   (labeling.py), all logged-skip-never-crash. Token-seed match mirrors
   Rust `find_by_token` (token ∈ `clobTokenIds`, double-encoding
   handled); slug match is exact; no match = counted `failed`, not
   `malformed`. All-digit names below the token threshold are
   token-or-nothing (never a slug query).
9. **Feature-file names**: `<sym>-ohlcv.json` (candles oldest-first,
   normalized from OKX's newest-first) and `<sym>-meta.json` (Gamma
   header) beside the replay-derived `<sym>.json` — same dir, per §6.1
   "beside". Frozen glob-equality asserts are safe because the frozen
   env has zero targets (invariant #5).
10. **`--symbols` does NOT restrict the §6.2 universe** — its
    documented contract only limits WRITTEN replay feature files; map
    ownership always sees the full observed universe.
11. **Malformed operator map halts fetch BEFORE any write** — the
    UNTOUCHED `cli.load_market_map` loads first (exit 2), so a
    half-readable map is never "repaired" by the writer
    (`test_cli_fetch_malformed_map_is_exit_2_and_never_overwritten`).
12. **Candle window/interval constants**: 1m bars, one-hour window
    (OKX `limit=60`; Deribit/HL windowed from injectable `now_ms` —
    deterministic in tests), `resolution=1` for Deribit's TradingView
    grammar.

### What was built

`fetchers.py` (NEW, ~640 lines): strict parsers
(`parse_gamma_markets`, `parse_okx_candles`, `parse_deribit_chart`,
`parse_hl_candles`), the four §6.1 consumers (`fetch_pm_gamma`,
`fetch_venue_candles` dispatching OKX-GET/Deribit-GET/HL-POST) — every
call `RestBudget.try_acquire`-gated (per-venue instances via
`venue_budgets`, default 60/h fixed window, `skipped_total` surfaced in
the per-venue stats line), feature-file writers, §6.2
`observed_universe` + `refresh_market_map` (bootstrap + additive
refresh + conflict report + atomic same-dir tmp + `os.replace`), and
the `run_secondary` orchestrator returning files + human summary lines.
Module doctrine held: fetchers.py never imports an HTTP client —
`get_fn`/`post_fn` injected; httpx lives in `cli.py` (`_http_get`/
`_http_post` beside the existing `_make_http_client` test seam, the
`feeds.fetch_feed` None-on-failure pattern). The 8g `cli.py` "no venue
URL consumers exist until 8h" deviation note is RETIRED. The 7-verb
surface is unchanged; fetch grew internals only.

### Tests added (+43 Python; frozen 202 untouched; NO live API calls)

`tests/test_fetchers.py`: budget env parse (default/override/malformed)
+ window-reset arithmetic + per-venue instance isolation; observed
universe over the committed `ticks_v2.pmlr` (= {7:pm, 67119674:hl,
67119675:hl}); all four parsers happy + malformed-skip + unusable-body;
seed classification; target derivation (universe restriction, one seed
per sym, binance-never-a-candle-target); per-consumer happy /
budget-exhausted (fns provably uncalled) / failed / malformed / URL +
POST-body exactness (windows from injected `now_ms`); candle + meta
file content and placement; engine-default mirror; gamma question+slug
naming; map bootstrap (reader-loadable by the UNTOUCHED
`cli.load_market_map`), additive refresh with operator preservation,
conflict reported-and-left-alone, pairs never fabricated, atomic write
observed via an `os.replace` spy (same-dir `.tmp`, tmp gone after);
`run_secondary` --no-rest-real / zero-targets-client-invariant / full
pass (budget cap 1 ⇒ second HL target skipped, `skipped_total`
surfaced); cli-level: --no-rest never constructs the client (raising
seam), no-descriptors bootstrap keeps the frozen glob exactly
[67119674.json, 67119675.json, 7.json], MockTransport end-to-end
(gamma + HL, files written + printed, map gains question/slug), HTTP
500 counted-not-fatal, malformed-map exit 2 with file byte-preserved.

### Gates at close (all on the Mac; MCP terminal, nohup+poll)

- worker pytest **247** green (204 baseline + 43; release binary on
  PATH so the 2 real-harness tests RAN, 0 skipped). 247 is the new
  stay-green.
- workspace nextest **1081/1081**, 1 skipped (the `#[ignore]` regen) —
  untouched-green; no LEAK flag appeared this run (H2's annotation was
  environmental).
- release alloc **36/36** 0 B/op `--test-threads=1` with the CORRECTED
  guard: `cargo clean -p bench --release` (`Removed 15 files`) + a
  fresh `Compiling bench` line verified in-log.
- `cargo build --release -p cli` Finished (0.08 s — no Rust changes, as
  the kickoff expected; the gates were the pitfall-#9 net).
- Fuzz untouched (kickoff law): all new parsing is strict field-checked
  JSON in PYTHON (labeling.py precedent) — no hand-rolled Rust
  untrusted-bytes parser exists.

### Hygiene

- Cargo/pytest on the Mac ONLY (pitfall #10; the Linux sandbox was used
  for read-only file inspection, never for gates). No engine boots. No
  `.env` read or write (`.env.example` only). No git op until the
  authorized closing commit. Push anomaly: unchanged posture.
- The pitfall-#11 live smoke for §6.1 (a one-shot `fetch` against the
  real public endpoints, budget-capped) is OPERATOR-GATED per the
  kickoff and was NOT run; the operator can run it any time with:
  seed the map (e.g. the boot market's token id as a name for sym 42),
  then `uv run claude-worker fetch`. Recorded as OWED-at-operator-
  discretion, not blocking item 3 (fixtures drove every §12 row).
- market-map.json on the real box: still absent (creating it live is
  part of the operator smoke above — the code path is
  bootstrap-on-first-fetch by construction).

### Resume point

Item 3 CLOSED (this commit). Next session H4 = design §13 item 4:
strategist (§7 in full + §8.1/§8.2 promotion). The H4 kickoff prompt
below is ready to paste.

---

## H4 kickoff prompt (paste verbatim into a fresh session)

8h implementation — SESSION H4 (design §13 checklist ITEM 4 ONLY:
strategist §7 IN FULL + §8.1/§8.2 auto-promotion; rollback/§8.3–§8.5 is
H5 — do NOT start it), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue. Stage-2 status: 8f/8g CLOSED;
8h H0 design LOCKED, H1 CLOSED, H2 CLOSED, H3 CLOSED — HEAD = the H3
commit (fetchers.py live: four §6.1 REST consumers on injected
get_fn/post_fn, RestBudget wired real 60/venue/h, --no-rest real,
market-map bootstrap/additive-refresh/conflict-report with atomic
writes; descriptors = map names, engine-default mirror binance:btcusdt;
.env.example carries the three §7.5 keys). Baselines NOW: workspace
nextest 1081/1081 (+1 ignored fixture-regen), release alloc 36/36
0 B/op (`--test-threads=1`; guard: `cargo clean -p bench --release` +
fresh `Compiling bench` in-log), worker pytest **247** (202 frozen
UNTOUCHABLE + 2 real-harness + 43 H3 fetchers; 247 is the stay-green),
fuzz `ruleset_json` 72.3M clean (untouched unless you hand-roll a Rust
untrusted-bytes parser — you must not). NO push, NO rebase, NO history
rewrite, NO branches, NO git ops without operator ask (ONE closing
commit IS authorized; one-line status after). Do NOT touch `.env`.
Notes go ONLY to docs/phase-8h-progress.md (H4 entry: decisions, tests,
gates, hygiene, resume point + H5 kickoff prompt). Verify
get_project_modules against the main checkout FIRST; stop if no attach.
REQUIRED READING, in order: (1) docs/phase-8h-design.md §7 ENTIRE
(prompt architecture + cache_control, §7.3 output contract, §7.4 cycle
state machine ≤2 calls/cycle + 12/day, §7.5 budget ledger + the env
keys NOW CONSUMED: CLAUDE_WORKER_STRATEGIST_INTERVAL_S=21600,
CLAUDE_WORKER_STRATEGIST_DAILY_CAP=12, §7.6 threading — LLM call on a
1-worker ThreadPoolExecutor, FRAMES SINGLE-WRITER on the serve loop,
background thread writes files only, own SQLite connection
prompt_cache-only) + §8.1/§8.2 (promote = atomic §6.2-style install to
$AI_RULESET_DIR/<hash128>.json then the FROZEN
backtest.stage_ruleset(author_mode="auto")/commit_ruleset pair;
attribution model=/thesis= additive OPTIONAL params on
state.stage_ruleset writing the pre-provisioned rulesets.model/thesis
columns — every existing call site unchanged) + §9 (serve composition:
research_cycle collaborator, SDK client construction stays daemon.py
and nowhere else, strategist receives complete_fn injected, SIGTERM
drain semantics) + §12 strategist/promotion test rows; (2)
docs/phase-8h-progress.md H3 entry (baselines + the 12 interpretation
calls — review them, uphold or flag, H2 precedent); (3)
claude-worker/src/claude_worker/ llm.py (grows the optional
system/cache_control/usage-return surface; MODEL_STRATEGIST +
STRATEGIST_MAX_TOKENS=4096 come due; existing triage/label callers
UNCHANGED), daemon.py (0.2 s loop, heartbeat cadence, watcher
composition pattern to mirror for research_cycle), state.py
(stage_ruleset additive params + events surface + prompt_cache/
cached_complete dedupe), backtest.py (the FROZEN stage/commit pair —
backtest.py:307 already names serve's commander path as a caller;
run_backtest is the promotion gate), labeling.py (strict-parse
precedent for the §7.3 {"thesis", "rows"} contract: malformed ⇒
.rejected archive + state event, cycle over), config.py (ServeConfig —
NOTE H3 interpretation #2: the frozen 202 construct ServeConfig
directly, so strategist env keys read at the seam, NOT new BaseConfig
fields), fetchers.py (feature files + market map the strategist digests
— read-only consumer); (4) docs/prompts/ai-session.md §4 (the
semi-manual strategist the auto path must mirror gate-for-gate). H4
SCOPE (item 4, nothing more): NEW strategist.py (inputs = feature files
+ news NDJSON + market map + grammar/caps contract; STATIC system block
with cache_control ephemeral + DYNAMIC token-capped digest
STRATEGIST_INPUT_CAP; strict §7.3 parse; candidates dir
~/multivenue/worker/candidates/<utc-ts>-<hash128>.json; ≤2 Fable-5
calls/cycle, revision call carries gate summary + report; daily
ceiling via the state.db events ledger kind='strategist_call' with
usage tokens + cache-read flag, breach ⇒ 'strategist_budget_skip'
event; no-fresh-capture cycle ⇒ SKIP, event logged;
prompt_cache dedupe via cached_complete), llm.py surface growth,
daemon.py research_cycle (interval from env, checked once per tick;
ThreadPoolExecutor(max_workers=1); UDS sends ONLY on the serve-loop
thread; backtest subprocess inline), state.stage_ruleset additive
model=/thesis= params, §8.1 promote step (gates PASS ⇒ atomic install ⇒
frozen stage/commit, author_mode="auto", paper only; gates FAIL ⇒ no
install, no frames). TESTS (additive; 247 stays green; design §12
strategist + promotion rows): prompt build (static/dynamic split,
cache_control present), strict output parse good/malformed/oversized,
revision-call cap, daily ceiling, dedupe hit (zero API cost), budget
ledger rows, background-thread seam with FakeClient (NO live SDK —
house rule; SDK constructed inside serve only), promotion auto-install
+ stage/commit against FakeUdsServer, attribution columns written,
gates-fail ⇒ no install no frames, existing stage_ruleset call sites
byte-unchanged. GREEN GATES to close: worker pytest 247 + new ALL
green, workspace nextest 1081/1081 untouched-green, release alloc 36/36
`--test-threads=1` corrected guard, `cargo build --release -p cli`
links (no Rust changes expected). LANDMINES: Mac-only cargo/pytest
(pitfall #10); RustRover MCP ≤45 s window — nohup > /tmp/8h-h4-*.log &
then poll, `sleep N` inside the polled command counts against the
window; zsh eats bare ===; full `import x` only; engine/strategy-vm/
ingress/core/cli crates READ-ONLY (claude-worker + docs only); the
7-verb surface FROZEN (the strategist lives INSIDE serve); do NOT touch
the committed backtest-real fixture; the H1 golden fixture + 202
untouchable; ANTHROPIC_API_KEY never read outside serve (the Base/Serve
split tests pin it). SESSION FACTS: projectPath
/Users/darkcite/trading-engine-multivenue; macOS: AF_UNIX sun_path cap,
SO_RCVTIMEO EINVAL on peer-closed UDS, std::thread::scope panic hangs
without StopOnDrop, sample <pid> for hangs; push anomaly KNOWN (38e599b
→ f2b3742 across H0): record, never act; the H3 §6.1 live fetch smoke
is OWED at operator discretion (not blocking); market-map.json still
absent on the box (bootstraps on first live fetch). If context runs
short: write interim state + exact resume point + relaunch prompt into
docs/phase-8h-progress.md, then tell me.

---

## 2026-08-22 — Session H4 (design §13 item 4: strategist §7 in full + §8.1/§8.2 auto-promotion) — CLOSED

Authority: the H4 kickoff above + design (LOCKED). RustRover MCP attached
FIRST (`get_project_modules` OK). Session-start HEAD `76680db` (the H3
commit), tree clean. Diff scope is exactly:
`claude-worker/src/claude_worker/strategist.py` (NEW),
`claude-worker/src/claude_worker/llm.py` (§7.2 surface growth),
`claude-worker/src/claude_worker/daemon.py` (`ResearchCycle` + serve
composition), `claude-worker/src/claude_worker/state.py`
(`stage_ruleset` additive `model=`/`thesis=` + new
`ruleset_attribution`), `claude-worker/tests/test_strategist.py` (NEW),
`claude-worker/tests/test_research_cycle.py` (NEW), `CLAUDE.md`
(operator-directed edit, below), this file. **`backtest.py` and
`cli.py` are byte-untouched**; engine/strategy-vm/ingress/core/cli
crates untouched (read-only law held); `conftest.py` and every frozen
test file untouched; the committed backtest-real fixture untouched;
`.env` untouched (`.env.example` already carried the §7.5 keys from H3
— nothing to add).

### NEW HARD OPERATOR REQUIREMENT (given in the H4 kickoff message)

**When Stage 2 (8f + 8g + 8h) is FULLY implemented — at 8h/H6 close
with the §12 exit criteria demonstrated — Claude must explicitly notify
the operator that Stage 2 is complete, and must NOT start ANY Stage-3
work (executor, risk/8i+, venue dispatchers, live ramp — no code, no
plans, no designs) without his explicit confirmation.** Recorded here,
in CLAUDE.md's CURRENT STATE, and in Claude's persistent memory; the H5
and H6 kickoff prompts carry it forward.

### H3 interpretation review — all TWELVE UPHELD (kickoff first task)

The twelve H3 calls concern fetchers/map ownership; H4 consumes them
read-only. Specifically upheld by use: #1 (descriptors = map names —
the strategist digest lists map names verbatim), #2 (config-seam env
reads — the two strategist keys follow the same seam pattern, NOT
BaseConfig fields), #5 (lazy client — untouched), #9 (feature-file
names — the digest globs `<run>/*.json`, which picks up `-ohlcv`/`-meta`
beside replay-derived files by construction). None amended.

### What was built

- **`llm.py` (§7.2 growth, additive):** `Completion` NamedTuple (text +
  `message.usage` token fields), `complete_message(client, model,
  prompt, *, max_tokens, system=None)` — `system` content blocks are
  passed to the SDK ONLY when provided, so every pre-8h call shape (and
  the conftest FakeClient's keyword-only `create(model=, max_tokens=,
  messages=)`) is untouched by construction; absent/bool/negative usage
  fields read as 0 (labeling.py numeric discipline). `complete` now
  delegates (same behavior); `STRATEGIST_MAX_TOKENS = 4096` lands (the
  llm.py "sets its own" note comes due). Frozen `test_llm` pins hold.
- **`strategist.py` (NEW, §7 in full):** §7.5 env readers (strict parse;
  interval ≥ 1, cap ≥ 0 with 0 = legal kill switch); the STATIC system
  block (grammar §4.1 exact — the 8-key row shape with `sym`/`side` and
  structured `trigger`, families/sides enums, validator-rule mirrors,
  tighten-only caps 100/250/1000, the backtest gates, output contract,
  worked example) marked `cache_control: {type: ephemeral}`; DYNAMIC
  digest builder (market map + observed universe + `<run>/*.json`
  feature files + news NDJSON tail oldest→newest + H5 performance seam),
  char-capped at `STRATEGIST_INPUT_CAP = 24_000` (TEXT_CAP precedent),
  deterministic for identical files (the dedupe key rides on it);
  proposal + revision user-prompt builders (revision carries prior rows
  + gate summary + report text per §7.4); §7.3 STRICT structural parse
  (exact key sets, bool-rejecting types, enums, published domain
  bounds, trigger-shape rules incl. the rule-6 mirrors `ref != sym` /
  no-`ref`-on-`level_breach`; >256 rows = oversized = malformed);
  canonical artifact writer (`{"rows":[...]}` compact, fixed key order
  — thesis stays OUT: the engine validator is unknown-key-strict);
  candidates-dir writers (atomic tmp+`os.replace`; `<utc-ts>-<hash128>
  .json` / `<utc-ts>.rejected.json`); §8.1 `install_candidate` (atomic
  `$AI_RULESET_DIR/<hash128>.json`); §7.5 ledger arithmetic
  (`utc_day_start_ns`, `calls_today` over `kind='strategist_call'`,
  `call_detail` JSON with model/usage/cache-read flag/purpose);
  `call_with_cache` — the ONE background-thread entry point: own State
  handle, `prompt_cache` table only, through the pinned
  `cached_complete` seam verbatim.
- **`state.py` (§8.2, additive):** `stage_ruleset` gains keyword-only
  `model=None`/`thesis=None` writing the pre-provisioned columns with
  COALESCE preservation (a restage WITHOUT attribution — e.g. H5's
  restage-prior through the frozen pair — never erases it; explicit
  values overwrite). `ruleset_row` stays the pinned 7-tuple; the new
  `ruleset_attribution(hash)` reads the pair. Every existing call site
  byte-unchanged (signature-pinned by test).
- **`daemon.py` (§7.4/§7.6/§8.1/§9):** `ResearchCycle` collaborator —
  idle→fetch→call→(revision)→promote state machine spanning 0.2 s
  ticks; due-check once per tick in idle only (cycles never overlap ⇒
  ≤1 in-flight background job); `ThreadPoolExecutor(max_workers=1)`
  created in serve (threads spawn lazily — a serve run whose cycle
  never comes due starts zero threads); the bg worker runs ONLY the
  fetch subprocess and `call_with_cache`; frames + events-ledger rows
  stay on the serve-loop thread; backtest subprocess inline on the
  serve thread; promote = install (at gates-pass time) → FROZEN
  `backtest.stage_ruleset(..., "auto")` → `state.stage_ruleset`
  attribution upsert → FROZEN `commit_ruleset`, then the `promotion`
  event; `serve()` composes it beside the watcher with additive
  `research_*` test-seam kwargs; shutdown adds
  `executor.shutdown(wait=False, cancel_futures=True)` (§9 drain).
  `MODEL_STRATEGIST` gets its first consumer (the serve-built
  completion seam binds it + `STRATEGIST_MAX_TOKENS`).

### Numbered H4 interpretation calls (H5 review welcome; all pinned by tests)

1. **Attribution route keeps `backtest.py` byte-frozen:** design §8.1
   spells the frozen pair WITHOUT attribution params and §8.2 gives
   them to `state.stage_ruleset` — so promote calls the frozen
   `backtest.stage_ruleset` (frame + registry row), then
   `state.stage_ruleset` AGAIN with `model=`/`thesis=` (attribution
   upsert, no frame, refreshes `staged_ts` before the commit — order:
   stage → attribute → commit), then the frozen `commit_ruleset`. The
   alternative (additive optional params on `backtest.stage_ruleset`)
   was rejected to keep the frozen surface literally byte-identical.
2. **COALESCE attribution semantics** (state.py above): None preserves,
   value overwrites — H5's restage-prior keeps the original author.
3. **`ruleset_row` stays a 7-tuple** (pinned by the frozen lifecycle
   test); attribution reads via the NEW `ruleset_attribution`.
4. **Cadence primes at serve start: first cycle due at t0 + interval.**
   Keeps every frozen daemon test hermetic (clock 0 / short runs never
   fire) and makes serve restarts budget-safe; the SQLite dedupe
   additionally makes any repeated inputs zero-API-cost. Interval + cap
   env keys are read ONCE at composition (strict parse fails the boot,
   the ServeConfig fail-fast pattern) — "checked once per tick" is the
   DUE check, watcher-style.
5. **The §7.4 in-cycle fetch = `claude-worker fetch --news` as a
   SUBPROCESS on the bg worker** (entry script beside the interpreter —
   the `test_session_scripted` invocation pattern; 180 s timeout).
   Rationale: the verb surface is frozen, `fetch` never touches the
   socket (no exit-4 collision with serve), cli.py↔daemon.py stay
   acyclic, and WAL already legalizes cross-process DB use. Fetch
   failure = counted degradation, cycle proceeds on existing files
   (REST is best-effort enrichment, §6.1 doctrine). Injectable seam
   (`research_fetch_fn`).
6. **Budget check is serve-side, BEFORE submit** (the events ledger is
   serve-thread-only under §7.6). Consequence: an at-cap cycle skips
   without consulting the dedupe cache — conservative, recorded.
7. **A SQLite-dedupe hit writes NO `strategist_call` row** (zero API
   cost ⇒ zero budget burn); the ledger's `cache_read` flag refers to
   ANTHROPIC prompt-cache reads (`usage.cache_read_input_tokens > 0`).
8. **Daily ceiling window = the current UTC calendar day** (consistent
   with the harness's UTC `trading_days`).
9. **Freshness = latest `run-*` NAME differs from the last cycle's**,
   held in memory for the serve lifetime; a restart re-runs one cycle
   and the dedupe absorbs it. Skip event kind:
   `strategist_capture_skip` (parallel to the design-named
   `strategist_budget_skip`).
10. **§7.3 parse is STRUCTURAL-only** (exact keys/types/enums/published
    bounds + the two cheap rule-6 mirrors); the semantic families
    (universe membership, duplicates, cap Σ walks) remain the harness
    validator's — no second deep parser to drift (§3.5 doctrine).
11. **`BacktestError` (untrusted report / validator reject) is
    candidate-fatal: NO revision call** — §7.4's revision carries "gate
    summary + report", which only a trustworthy gates-FAIL possesses.
    Event `strategist_candidate_rejected` reason `backtest_error`.
12. **Candidate artifact carries rows only**; the thesis persists via
    the registry column (and the raw response sits in `prompt_cache`) —
    recoverable, never validator-visible.
13. **Candidates dir = `CLAUDE_WORKER_DB` parent / `candidates`** (≡ the
    §14-pinned `~/multivenue/worker/candidates/` under defaults;
    test-local under a tmp db; no new env key).
14. **The §8.4 `promotion` event lands in H4** with the promote step it
    describes (H5 adds the rollback kinds).
15. **Promote waits for the engine:** gates-pass installs immediately;
    Stage/Commit wait for a live connection and retry on `UdsError`
    (Stage supersede semantics make retries idempotent). A pending
    promote blocks SUBSEQUENT cycles until delivered — single-candidate
    discipline, visible via stats/events.
16. **SIGTERM drain nuance vs §9's "thread daemonized":**
    `ThreadPoolExecutor` threads are not literally daemonic;
    `shutdown(wait=False, cancel_futures=True)` returns serve
    immediately and abandons the future — a truly in-flight SDK call
    bounds process exit by the SDK's own request timeout. Recorded as
    the §9-faithful mechanism on CPython 3.14.
17. **Additive event kind `strategist_call_failed`** for API/transport
    failures of the call itself (§5.1 no-crash doctrine: counted event,
    cycle over, serve survives).
18. **`ServeStats` untouched; research counters live in the new
    `ResearchStats`** (exposed via `serve(research_stats_out=)`).

### Tests added (+79 Python; frozen 202 + H2/H3 suites untouched; NO live SDK)

- `tests/test_strategist.py` (63): §7.5 env strict-parse matrices;
  system-block static/cache_control content pins; digest sections +
  determinism + char-cap + static/dynamic split; revision-prompt
  content; §7.3 parse — good 2-row (canonical key order), 12 top-level
  malformed shapes, 25 row-level malformed shapes (missing/unknown key,
  name len/ascii, enums, bool-sneaks, fractional `edge_bps`, domain
  bounds, trigger-shape rules, `ref == sym`), oversized 257 vs 256;
  candidate write (name = `<utc-ts>-<hash128>.json`, hash == the frozen
  `ruleset_hashes` recomputation, rows-only body, no `.tmp` residue);
  `.rejected` archive; §8.1 install (atomic, idempotent re-install);
  candidates-dir derivation; `utc_day_start_ns`/`calls_today` UTC-day
  arithmetic (other kinds excluded); `call_detail` field-exact incl.
  cache-read flag both ways; `call_with_cache` miss→hit (zero API cost,
  static blocks ride the call, second handle open = WAL legality) +
  version scoping (`strategist-v0` seed must miss); llm surface —
  usage returned, system passed-through when given and ABSENT when
  None, bool/negative usage zeroed, `STRATEGIST_MAX_TOKENS == 4096`;
  §8.2 — signature keyword-only defaults pinned, attribution
  written/preserved/overwritten with `ruleset_row` 7-tuple intact.
- `tests/test_research_cycle.py` (16): first-due-after-interval;
  capture-skip (no runs + stale run, event details exact, no call);
  budget cap 0 kill switch (event detail exact, zero calls);
  yesterday's ledger rows don't count / today's 12 do; restart-dedupe
  (fresh instance, identical inputs ⇒ replayed response, zero calls, no
  new ledger row, flow still parses); call-failure event (no crash, no
  frames); `BacktestError` ⇒ no revision, no install, no frames;
  fetch-failure counted + cycle proceeds + fetch provably on the bg
  thread; **revision-call cap** (exactly 2 calls, revision prompt
  carries FAILED + gate summary + report + prior rows, final archive
  event, both reports in the candidates dir, ledger purposes
  [proposal, revision], no install, no frames); **full promotion**
  against FakeUdsServer (bg-thread seam ident-checked, cache_control
  blocks on the call, install byte-equal at
  `$AI_RULESET_DIR/<hash128>.json`, frames = Heartbeat→Stage→Commit
  with px/qty == hash128 LE halves + VENUE_AI + slot-5, registry
  staged+committed `author_mode='auto'`, attribution ==
  (claude-fable-5, thesis), ledger + promotion event details exact);
  promote-waits-for-connection (install early, zero frames while down,
  delivery after connect); **serve-level composition** (monkeypatched
  system-capable fake at the llm seam, synthetic 0.5 s/tick clock,
  interval 1 s: one cycle, one Fable-5 call with system blocks + the
  4096 budget, Stage before Commit on the serve connection, install +
  attribution + committed registry + ledger row — §9's composition
  proven inside the real loop).

### Gates at close (all on the Mac; MCP terminal, nohup+poll)

- worker pytest **326** green, 0 skipped (202 frozen + 2 real-harness
  RAN with the release binary on PATH + 43 H3 fetchers + 79 new).
  **326 is the new stay-green.**
- workspace nextest **1081/1081**, 1 skipped (the `#[ignore]` regen) —
  untouched-green, zero Rust changes this session.
- release alloc **36/36** 0 B/op `--test-threads=1` with the corrected
  guard: `cargo clean -p bench --release` (`Removed 15 files`) + a
  fresh `Compiling bench` verified in `/tmp/8h-h4-alloc.log`.
- `cargo build --release -p cli` Finished 0.18 s (no Rust changes; the
  H3-built binary stands — G0 law satisfied for the pytest PATH run).
- Fuzz untouched (kickoff law): all new parsing is strict Python JSON
  over MODEL output (labeling.py precedent) — no Rust untrusted-bytes
  parser exists or was hand-rolled.

### Hygiene

- Cargo/pytest on the Mac ONLY (pitfall #10; the Linux sandbox ran
  read-only file inspection + `py_compile` syntax checks only — never a
  gate). No engine boots, no live sockets beyond the FakeUdsServer
  fixture, no live SDK calls anywhere. `ANTHROPIC_API_KEY` is still
  read nowhere outside serve (the strategist seam is built in
  `daemon.serve` from the one llm client; the Base/Serve split tests
  stayed untouched-green).
- No git op until the authorized closing commit. Push anomaly:
  unchanged posture; no fetch/push.
- **CLAUDE.md edited this session — deliberately, two causes:** (a) the
  NEW hard operator requirement above (operator-directed, belongs in
  front-loaded context); (b) the H2-noted stale alloc-guard text
  (`cargo clean -p bench` → `--release`) fixed as the rider H2's entry
  scheduled "at the next phase-boundary edit"; plus the CURRENT STATE
  block refreshed to H4-close truth (baselines, next session = H5).
  The authority chain (latest progress entry wins) is unchanged.
- The H3 §6.1 live fetch smoke remains OWED at operator discretion.
  The strategist's own live proof is H6's (one real budget-capped
  Fable-5 serve cycle, per design §13.6) — nothing live ran in H4, by
  scope.
- `market-map.json` still absent on the box (bootstraps on first live
  fetch — unchanged).

### Resume point

Item 4 CLOSED (this commit). Next session H5 = design §13 item 5:
rollback (§8.3 monitor + trigger + restage-prior + §8.4 events). The
H5 kickoff prompt below is ready to paste; it opens with the mandatory
review of the eighteen H4 interpretations above.

---

## H5 kickoff prompt (paste verbatim into a fresh session)

8h implementation — SESSION H5 (design §13 checklist ITEM 5 ONLY:
rollback — §8.3 walk-forward monitor + trigger + restage-prior + §8.4
events; H6 close/live-demo is NOT this session — do NOT start it),
MAIN CHECKOUT /Users/darkcite/trading-engine-multivenue. Stage-2
status: 8f/8g CLOSED; 8h H0 design LOCKED, H1 CLOSED, H2 CLOSED, H3
CLOSED, H4 CLOSED — HEAD = the H4 commit (strategist.py + serve
research_cycle live: 6 h Fable-5 cycle on a 1-worker bg executor,
prompt caching + SQLite dedupe via cached_complete, §7.5 budget ledger
kind='strategist_call' + strategist_budget_skip/strategist_capture_skip
/strategist_candidate_rejected/strategist_call_failed events,
candidates dir db-parent/candidates, §7.3 strict structural parse,
promote = atomic install → FROZEN backtest.stage_ruleset("auto") →
state.stage_ruleset attribution upsert (model=/thesis= COALESCE) →
FROZEN commit_ruleset → 'promotion' event; backtest.py + cli.py
byte-untouched; ruleset_row stays a 7-tuple, attribution reads via
state.ruleset_attribution). Baselines NOW: worker pytest **326** green
0 skipped with the release binary on PATH (202 frozen UNTOUCHABLE + 2
real-harness + 43 fetchers + 79 H4; 326 is the stay-green), workspace
nextest 1081/1081 (+1 ignored fixture-regen), release alloc 36/36
0 B/op (`--test-threads=1`; guard: `cargo clean -p bench --release` +
fresh `Compiling bench` in-log), fuzz `ruleset_json` 72.3M clean
(untouched unless you hand-roll a Rust untrusted-bytes parser — you
must not). NO push, NO rebase, NO history rewrite, NO branches, NO git
ops without operator ask (ONE closing commit IS authorized; one-line
status after). Do NOT touch `.env`. Notes go ONLY to
docs/phase-8h-progress.md (H5 entry: interpretation-review verdicts,
decisions, tests, gates, hygiene, resume point + H6 kickoff prompt).
Verify get_project_modules against the main checkout FIRST; stop if no
attach. **HARD OPERATOR REQUIREMENT (standing, recorded H4): when
Stage 2 (8f+8g+8h) is FULLY implemented — at H6 close with the §12
exit criteria demonstrated — explicitly notify the operator that
Stage 2 is complete, and do NOT start ANY Stage-3 work (executor,
risk/8i+, dispatchers, live ramp — code, plans, or designs) without
his explicit confirmation. H5 does not trigger the notification (H6
does), but carry the requirement into the H6 kickoff prompt.**
FIRST TASK, before any code: review the EIGHTEEN H4 interpretations in
the H4 entry of docs/phase-8h-progress.md — uphold or amend each,
explicitly, in the H5 entry (amendments must not break the frozen
worker contract or the H4 tests without stating why). REQUIRED
READING, in order: (1) docs/phase-8h-design.md §8.3 ENTIRE (the
trigger table: walk-forward re-backtest of the ACTIVE ruleset,
trailing 24 h of capture target with a 6 h FLOOR — below floor the
monitor SKIPS with monitor_skip_insufficient_data, it never guesses;
metric = net_pnl_usd ≤ −$100 OR max_drawdown_usd ≥ $200 from a
`--split 0/100` all-OOS run — plain run_backtest(split="0/100")
passthrough, ZERO worker-code change to backtest.py; action = (1)
disable --strategy 5 equivalent through the commander path, (2)
restage + commit the PRIOR gates-passed committed hash from the
registry — artifact still installed by construction; no prior ⇒
disable only + rollback_no_prior; cadence = every research cycle +
once shortly after every promotion as the arm check; D3a semantics:
disable flips the mask bit, the table persists, restaging the prior
then commit flips back to known-good rows) + §8.4 (event kinds:
rollback_triggered WITH metric values, rollback_no_prior,
monitor_skip_insufficient_data; 'promotion' already landed in H4) +
§8.5 (the forced-underperformance demo definition — H6 EXECUTES it,
but H5 must leave the seams the demo needs: operator-selectable
monitor window inputs) + §12 monitor test rows (threshold arithmetic
BOTH arms, window floor skip, action ordering disable BEFORE restage,
no-prior fallback, event rows); (2) docs/phase-8h-progress.md H4 entry
(the 18 interpretations + what ResearchCycle already owns — the
monitor is a new step INSIDE the existing cycle state machine, §8.3
cadence row); (3) claude-worker/src/claude_worker/ daemon.py
(ResearchCycle — the monitor slots in as a cycle step + a
post-promotion arm check; UDS sends ONLY on the serve-loop thread —
the disable/restage/commit frames ride exactly the promote path's
client discipline; note interpretation #15: a pending promote blocks
subsequent cycles — decide and record how the monitor interacts with
that), state.py (registry queries — you will likely need an additive
accessor for "latest committed hash" and "prior gates-passed committed
hash"; ruleset_row stays a 7-tuple, the frozen lifecycle test pins
it), backtest.py (READ-ONLY — run_backtest(split=) is a passthrough
string and §3.4 carved 0/100 exactly for this; stage/commit pair
frozen; you must NOT edit this file), strategist.py (event-kind
constants live here; add the three §8.4 kinds beside them),
frames.py (KIND_DISABLE_STRATEGY, STRATEGY_SLOT_VM = 5); (4)
docs/prompts/ai-session.md §4 step 10 (the manual rollback the auto
path mirrors); (5) docs/risk-policy.md (the −$100 = ½ the $200/day
kill line rationale — thresholds are code constants, never prompts).
H5 SCOPE (item 5, nothing more): monitor implementation inside
ResearchCycle (or a sibling collaborator it owns) — select the
trailing-window capture input for the ACTIVE ruleset's re-backtest
(design §8.3 window row: trailing 24 h target / 6 h floor of CAPTURE;
the harness merges runs under --replay-dir, so H5 must RESOLVE the
window-selection mechanism against the run-dir layout and RECORD the
interpretation — a floor-check over run epochs + a temp dir of
selected runs, or an equivalent that keeps backtest.py untouched),
run_backtest(active_ruleset_path, window_dir, split="0/100") inline,
threshold check (net ≤ −100.0 OR dd ≥ 200.0, constants in code),
trigger action ordering: disable-5 frame FIRST (through the serve
uds_client, KIND_DISABLE_STRATEGY strategy_id=5 sym NONE venue AI —
mirror the push verb's wire shape), THEN restage+commit the prior
gates-passed committed hash via the FROZEN pair (attribution untouched
— interpretation #2's COALESCE covers it), no-prior ⇒ disable only +
rollback_no_prior event; rollback_triggered event carries the metric
values; monitor_skip_insufficient_data on floor-breach; arm check once
shortly after every promotion (§8.3 cadence row); the §7.1 digest
"performance" seam may now feed the ACTIVE ruleset's latest
walk-forward report into the strategist digest (design §7.1 — the
parameter already exists); events + stats additive. TESTS (additive;
326 stays green; design §12 monitor rows): threshold arithmetic both
arms (boundary values exact), window floor skip + event, action
ordering (disable frame BEFORE the Stage frame — FakeUdsServer frame
order), no-prior fallback (disable only + event), rollback_triggered
event detail carries net/dd values, restage-prior preserves
attribution (#2), monitor frames on the serve-loop thread only,
post-promotion arm check fires, insufficient-capture floor math over
run epochs, and the existing H4 promotion tests stay byte-green.
GREEN GATES to close: worker pytest 326 + new ALL green (release
binary on PATH), workspace nextest 1081/1081 untouched-green, release
alloc 36/36 `--test-threads=1` corrected guard, `cargo build --release
-p cli` links (no Rust changes expected). LANDMINES: Mac-only
cargo/pytest (pitfall #10); RustRover MCP ≤45 s window — nohup >
/tmp/8h-h5-*.log & then poll, `sleep N` inside the polled command
counts against the window; zsh eats bare ===; full `import x` only;
engine/strategy-vm/ingress/core/cli crates READ-ONLY (claude-worker +
docs only); backtest.py + cli.py stay byte-untouched (H4 precedent —
attribution/monitor both live outside them); the 7-verb surface
FROZEN; do NOT touch the committed backtest-real fixture; conftest.py
+ every frozen test file untouchable; ANTHROPIC_API_KEY never read
outside serve. SESSION FACTS: projectPath
/Users/darkcite/trading-engine-multivenue; macOS: AF_UNIX sun_path
cap, SO_RCVTIMEO EINVAL on peer-closed UDS, std::thread::scope panic
hangs without StopOnDrop, sample <pid> for hangs; push anomaly KNOWN
(38e599b → f2b3742 across H0): record, never act; the H3 §6.1 live
fetch smoke is OWED at operator discretion (not blocking);
market-map.json still absent on the box; the strategist's live proof
(one real budget-capped Fable-5 serve cycle) is H6's demo, not H5's.
If context runs short: write interim state + exact resume point +
relaunch prompt into docs/phase-8h-progress.md, then tell me.
