# Phase 8g — Ruleset Engine (`strategy-vm`): Design (Phase 0, no code)

Status: **§13 LOCKED by operator, session G0 (2026-08-16).** Authority:
this file + the G0 entry in `docs/phase-8g-progress.md`. Supersedes nothing in the committed plan;
completes the 8f §1 deferral ("`strategy-vm` ruleset *evaluation*") and
the item-14 side-path deferrals recorded in the 8f closing entry.

Inputs read for this design: 8f closing entry (`docs/phase-8f-progress.md`
S7), `docs/phase-8f-design.md` §1/§7/§13, `crates/ingress-ai/src/ruleset.rs`
(the stub this phase replaces), `docs/wire-format.md` (AiCmd, SlotKind 4),
`docs/risk-policy.md`, `docs/prompts/ai-session.md` §4, and the live 8f
AI-cmd demo run this session (see the G0 progress entry — four of its
findings land in this doc's scope).

---

## 1. Scope and non-goals

**In scope (8g):**

- `crates/strategy-vm` — the slot-5 StrategySet member: zero-alloc
  evaluation of an operator-committed rule table in the engine hot path.
- Ruleset side-path completion (8f item-14 deferrals): JSON bounds-check
  in Rust (byte scanner, no `serde_json`), staged-table build, and the
  Commit-driven flip into the live evaluator. The d5 hash128 filename
  convention and the S6 registry semantics (restage supersedes commit)
  are KEPT exactly as shipped.
- `StrategySet` slot-5 activation: `vm` member lands, `BUILT_MASK` gains
  bit 5, `mask_for_name("vm")`, `--strategy all` composes it.
- Metrics/capture surface for evaluated rulesets, including two gaps the
  G0 live demo proved: no runtime `enabled_mask` observable, and no
  `slot_kind = 4` support in `audit-replay`.
- Alloc-gate APPEND (baseline at 8f close: **33 gates, 0 B/op**).
- Triage of the 8f S7 comment-tidy list (§14).

**Out of scope (unchanged gates):** `strategist.py` cadence, the real
`cli backtest` harness, venue REST consumers (all 8h); `crates/risk`
(8i); live fill producers/dispatchers (8j); paid APIs (Phase-6 P&L
gate). Also out: trigger families beyond §4's v1 grammar, TUI ruleset
pane (metrics suffice; snapshot-page field reserved), any change to the
worker's Stage/Commit verbs (frozen §6 surface — the engine catches up
to it, not vice versa).

**Exit criteria (8g):** a gates-passed ruleset authored per
`ai-session.md` §4 stages, commits, and *evaluates* live in paper mode —
rows trigger, orders clamp to risk-policy caps, `disable --strategy 5`
rolls it back — with all alloc gates green (33 + appended), proptest +
fuzz on the validator, and `audit-replay` rendering the run's
`ai-cmds.pmlr`.

---

## 2. Component breakdown

| component | crate | change |
|---|---|---|
| `RuleRow` / `RuleTable` / `RuleTableSlot` POD | `core-types` | NEW types (§3) — wire-format.md gains rows |
| Ruleset JSON validator | `ingress-ai` (`ruleset.rs` grows; scanner helpers in `core-parse` if reusable) | stub's "drop after hash" becomes parse+validate+stage (§4, §5) |
| Table handoff ring | `core-ring` (existing generic `Ring<T, N>`) | NEW instance `Ring<RuleTableSlot, 2>` (§6) |
| Evaluator | `crates/strategy-vm` | NEW crate (§7) |
| Set integration | `strategy-set` | slot 5 member + mask (§8) |
| Engine drain | `engine` | table-ring pop + Commit-flip (§6) |
| Boot wiring | `cli` (`paper.rs`, bin) | `spawn_ai` + engine ctor args; universe snapshot for the validator (§4.3) |
| Observability | `cli`, `audit_replay.rs`, `ingress-ai::status` | §9 |
| Gates | `bench/tests/alloc_assertions.rs` | +3 gates (§10) |

Hot-path additions are exactly: one SPSC-ring emptiness check per engine
iteration, the vm member's `on_tick`/`on_ai` work, and (on the ingress
thread) nothing — the side path stays at operator cadence.

---

## 3. Rule table representation (`core-types`)

Sized for the §4 caps: ≤ 256 rows, one row = 64 B (exactly one cache
line; no row straddles a line), table body = 16 KiB.

```rust
/// One validated rule. POD; every field fixed-width; strings never
/// reach the hot table (row names live only in the JSON artifact +
/// worker registry; `name_h` is an FNV-1a 64 of the name bytes for
/// offline correlation).
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct RuleRow {
    pub sym: u32,            // SymbolId, validated against boot universe
    pub ref_sym: u32,        // cross_deviation reference leg; SYMBOL_ID_NONE for level_breach
    pub edge_bps: u32,       // trigger threshold, basis points
    pub horizon_ms: u32,     // re-arm horizon (cooldown), clamped [10, 86_400_000]
    pub level_1e6: i64,      // level_breach threshold px ×1e6; 0 for cross_deviation
    pub max_risk_1e6: i64,   // per-row notional cap ×1e6 (≤ risk-policy single-order cap)
    pub name_h: u64,         // FNV-1a 64 of `name`
    pub trigger: u8,         // 0 = cross_deviation, 1 = level_breach (§4.2, D2)
    pub side: u8,            // Side (0/1) or 0xFF = both
    pub family: u8,          // 0 crypto, 1 politics, 2 sports, 3 macro, 4 other (reporting only)
    pub _pad: [u8; 13],      // explicit, zeroed; validator rejects nonzero? — no: built, not parsed
}

/// The engine-facing table. `#[repr(C, align(64))]`, Copy. 16 KiB rows
/// + one trailing line of metadata.
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct RuleTable {
    pub rows: [RuleRow; 256],
    pub len: u32,            // validated row count ≤ 256
    pub epoch: u32,          // side-path monotonic stage counter (diagnostics)
    pub hash128: [u8; 16],   // identity — the d5 truncated sha256
    pub _pad: [u8; 40],
}

/// Ring slot ferrying a staged table ingress→engine (§6).
pub type RuleTableSlot = RuleTable;
```

Size assertions (compile-time, house style): `size_of::<RuleRow>() == 64`,
`size_of::<RuleTable>() == 16 * 1024 + 64`. All fields little-endian
in-memory POD — the table never crosses a byte-order boundary (same
process), so no serialization is defined and none is captured (identity
is `hash128`; the JSON artifact is the durable form).

---

## 4. Ruleset JSON: grammar and validator (`ingress-ai`, byte scanner)

### 4.1 Grammar (v1 — freezes what `ai-session.md` §4 step 3 sketches)

```json
{
  "rows": [
    {
      "name": "btc-pm-lag",
      "family": "crypto",
      "trigger": {"type": "cross_deviation", "ref": 7},
      "sym": 42,
      "side": "bid",
      "edge_bps": 80,
      "horizon_ms": 1500,
      "max_risk_usd": 50.0
    },
    {
      "name": "hormuz-floor",
      "family": "politics",
      "trigger": {"type": "level_breach", "level": 0.012},
      "sym": 42,
      "side": "ask",
      "edge_bps": 0,
      "horizon_ms": 60000,
      "max_risk_usd": 25.0
    }
  ]
}
```

- `sym` / `ref` are numeric SymbolIds (the worker's market map already
  resolves names → ids on its side; the engine never sees names).
  SymbolIds are venue-namespaced (`core_types::symbol_venue_byte`), so
  every leg names "asset X **on venue V**" — D2 operator amendment
  makes this explicit: **both legs may be any symbol on any
  boot-universe venue**. The namespaced `sym` IS the venue targeting
  and `ctx.submit` is venue-agnostic (cross-arb precedent);
  `Order.venue` (offset 40, wire-format row) is DERIVED from the
  action sym's namespace byte at emit, never chosen independently.
  *(Clause reworded in 8h H2 per phase-8h-design §15.1 — the original
  "carries no venue field" wording was factually stale; the mechanism
  described was always correct.)* Live emission is a DEPLOYMENT
  gate, not a grammar rule: everything is paper until 8i, and per-venue
  live dispatchers are 8j — today's only live gateway
  (`clob-dispatcher`, EIP-712) happens to be Polymarket, which
  constrains nothing here. (The example `ref: 7` is Binance btcusdt,
  the boot default pairing.)
- `side`: `"bid"` | `"ask"` | `"both"`.
- v1 trigger types (D2): `cross_deviation` (PM leg `sym` deviates from
  the reference leg `ref` mid — that asset on that venue — by ≥
  `edge_bps`) and `level_breach` (best price crosses `level`). Both
  replayable from tick captures — the 8h backtest harness requirement.

### 4.2 Validator rules (reject ⇒ `engine_ai_ruleset_rejected_total`, stage refused)

Order matters; first failure wins. All checks over `&[u8]`, handwritten
scanner per house rule (no `serde_json` — this parses **untrusted
bytes**: the artifact file is operator-installed but the frame that
names it is network-adjacent, and the file can be swapped on disk).

1. Full sha256 of the raw bytes must match the frame's hash128 prefix
   (already the 8f stub behavior — kept, runs FIRST, before any parse).
2. Well-formed grammar per §4.1; strict: unknown key ⇒ reject; duplicate
   key ⇒ reject; trailing bytes ⇒ reject; depth is fixed (no recursion —
   the scanner is a flat state machine).
3. Numbers: decimal only, no exponents (Deribit sci-notation lesson —
   8e), no NaN/Inf tokens; `max_risk_usd` and `level` parsed via the
   `core-parse` 1e6 fixed-point scanner; integer fields reject
   fractional parts; range checks: `edge_bps ≤ 10_000`,
   `horizon_ms ∈ [10, 86_400_000]`, `level_1e6 ∈ [0, 1_000_000]`
   (Polymarket price domain).
4. `rows` count ∈ [1, 256].
5. `name` ASCII, len ∈ [1, 64] bytes, unique within the file (name_h
   collision ⇒ reject — FNV collision at ≤256 names is authoring error).
6. Symbols: `sym` (and `ref` where present) must exist in the boot
   universe snapshot (§4.3) — no venue restriction on either leg
   (D2 amendment: any asset on any boot-universe venue; live-emission
   gating is 8i/8j's job, not the validator's). `cross_deviation`
   additionally requires `ref != sym`. `level_breach` requires `ref`
   ABSENT.
7. Caps (tighten-only vs `docs/risk-policy.md`, in 1e6 fixed point):
   per-row `max_risk_1e6 ≤ $100`; Σ `max_risk_1e6` per sym ≤ $250;
   Σ over the table ≤ $1 000. A ruleset may not widen anything — there
   is no field that CAN express a widened cap; absence of such fields is
   the enforcement.
8. Exact-duplicate row (`sym`, `trigger`, `side`, `ref`/`level`) ⇒
   reject (authoring error).

The validator writes into a side-thread-owned scratch `RuleTable`
(preallocated at spawn; reused per Stage — steady-state 0 B/op gate,
§10). `std::fs::read` of the artifact allocates: **documented copy #0**,
operator cadence only, same dispensation the 8f stub already documents.

### 4.3 Boot universe snapshot

`spawn_ai` gains `universe: Arc<[u32]>` — the sorted SymbolId set the 8e
discovery produced (built once in the bin after discovery, before
threads spawn; binary-search per row check). No liveness coupling: a
symbol that later loses its feed still validates (the row just never
triggers) — universe membership is a boot-time fact, mirroring how every
other consumer treats SymbolMap.

---

## 5. Side-path completion (semantics kept, drop replaced)

The 8f stub machine is correct and stays: hash-verify → staged;
commit-for-staged-hash → committed; **restage supersedes commit**
(S6 registry semantics — `state.py` mirrors this; the golden pair of
state machines must not drift). The d5 filename convention
(`AI_RULESET_DIR/<hash128-hex>.json`, first 32 hex of full sha256) is
KEPT — `ai-session.md` §4 step 5 already teaches it.

8g changes inside `RulesetSidePath::stage` only:

```
8f: read → sha256 → prefix match → staged = Some(h); bytes DROPPED
8g: read → sha256 → prefix match → validate into scratch (§4.2)
      → try_push scratch table into Ring<RuleTableSlot, 2>   (§6)
      → push ok  ⇒ staged = Some(h); committed = None; inc staged
      → any fail ⇒ inc rejected (staged/committed unchanged)
```

`commit` is UNCHANGED in the side path (state flag + counter) — the
engine-side flip keys off the Commit **AiCmd in the command ring**, not
off side-path state (§6). Push-full (2 staged tables the engine never
drained) is a reject: impossible at operator cadence against a µs-drain
engine loop, counted honestly anyway.

---

## 6. Handoff and flip architecture (D1 — put to operator)

**Recommended (a): table ring at Stage + flip on the in-stream Commit.**

- Boot builds `Ring<RuleTableSlot, 2>` (SPSC, cache-aligned, like every
  other ring). Producer: ingress-ai thread (side path). Consumer: engine.
- Listener ordering already guarantees: Stage AiCmd `try_push` happens
  BEFORE the seam runs (`listener.rs` §4.4 step 8) — so in the command
  ring the Stage cmd precedes any Commit cmd, and the table push lands
  between them in real time.
- Engine loop, each iteration, BEFORE the AI-cmd drain: pop table ring
  (usually empty — one acquire-load); a popped table lands in the vm
  member's **staged buffer** (member-internal `[RuleTable; 2]`
  ping-pong; a later pop overwrites staged = restage-supersedes,
  engine-side mirror of §5).
- When the AI-cmd drain dispatches `RulesetCommit` to the set, the set
  routes it to the vm member (today it fans out as a no-op): vm compares
  `cmd` hash128 (px+qty reassembly, same helper as the side path)
  against `staged.hash128` — match ⇒ ping-pong index flip (staged
  becomes active; no copy), mismatch/no-staged ⇒ drop + counter (§9).
- Copies, documented per house rule: **(#1)** scratch → ring slot on
  `try_push` (16 KiB, by-value), **(#2)** ring slot → member staged
  buffer on pop (16 KiB). Both at operator cadence (per-Stage, not
  per-tick). Zero copies and zero atomics in the eval path itself; the
  flip is an index swap on the engine thread.
- Why in-stream flip: the Commit is **serialized against every other
  AiCmd** (enable/disable/halt) in the single command ring — replaying
  the capture reproduces the exact interleaving; no cross-ring ordering
  reasoning, no shared mutable table memory between threads, single
  writer everywhere.

**Alternative (b): shared double buffer + atomic index flip at the side
path.** Zero data movement (side path writes the inactive buffer in
place, `active.store(Release)` on Commit; engine `load(Acquire)` per
iteration). Costs: cross-thread shared mutable 32 KiB, a reader-ack
protocol to make rapid restage safe, one acquire load in the hot path
even when idle, and the flip is NOT ordered against the command stream
(a Commit can apply mid-iteration relative to enables). More invariants,
less auditability, saves two 16 KiB copies per operator action.

**Alternative (c): push the table at Commit time instead of Stage.**
Fewer moving parts (no engine staged buffer) but the flip then keys off
ring arrival rather than the command stream, losing the in-stream
ordering property of (a) for no cost saving.

---

## 7. `strategy-vm` evaluation semantics

### 7.1 Strategy-trait fit

`VmStrategy` implements `Strategy` (monomorphized member of
`StrategySet`, which the engine already drives as `Engine<StrategySet>`
— no `dyn` anywhere; G0 demo note: set-level commands REQUIRE the set,
`--strategy all|ai-exec|vm`).

- `on_start`: nothing to allocate (tables are inline fields); validates
  nothing (tables arrive later); returns Ok.
- `on_tick`: the hot path. Maintains per-sym best-px state (reuse the
  `MultiBook` pattern rule-tree uses) for the reference legs; evaluates
  active-table rows for `tick.sym`:
  - `cross_deviation`: |mid(sym) − mid(ref)| in bps ≥ `edge_bps` ⇒ fire.
  - `level_breach`: best px crosses `level_1e6` on the row's side ⇒ fire.
  - Fire = emit paper order via `ctx.submit` with qty sized so notional
    ≤ `max_risk_1e6` (and never above risk-policy caps — re-clamped at
    emit as defense in depth even though §4.2 validated it; 8i replaces
    the clamp with RiskGate).
  - Re-arm per row via a `CooldownGate<256>`-style stamp: a fired row
    sleeps `horizon_ms`.
  - Row scan: linear over `len` with a `sym` filter — ≤ 256 iterations
    of branch-light code over contiguous 64 B rows, `get_unchecked`
    inside a safe wrapper (index < len invariant), `// SAFETY:` comment
    per house rule. A per-sym bucket index is a measured-later
    optimization; not v1 (the scan is cheap and the table is one L2-warm
    16 KiB block).
- `on_signal` / `on_fill`: v1 no-op (fills matter when 8j lands real
  fill producers; paper fills feed counters only).
- `on_ai`: consumes `RulesetCommit` (§6 flip), ignores Stage (side-path
  concern), inherits nothing else — fair values/bias stay `ai-exec`'s
  domain (slot separation is deliberate; one member per state family).
- `on_timer`: expiry sweep for cooldown stamps is NOT needed (stamps
  compare against `now_ns` lazily); timer disabled (`u64::MAX`).
- `StrategyCounters`: `strategy_kind() == "vm"`, counters for
  rows_active, evals, fires, orders_emitted/dropped, commits_applied,
  commits_dropped.

### 7.2 TTL/staleness interplay with the AI lane (D3 — put to operator)

The §5.4 machinery (5 s heartbeat / 15 s staleness, TTL-on-pop for
AiCmds, `expire_on_silence` on fair-value entries) governs **AI-pushed
state that decays**. A committed ruleset is a different animal: it
passed offline backtest gates, an explicit two-step Stage/Commit, and is
observable in three counters and the registry.

**Recommended (a): the committed table persists through worker silence.**
Rationale: rows trigger on MARKET data, not on AI-pushed state; the
§5.4 fail-safe exists to decay *opinions* (fair values), not *vetted
policy* — the analogue is rule-tree's boot-loaded rules, which no one
expects to vanish when a Python process naps. Rollback remains explicit
and already documented (`ai-session.md` §4 step 10): `push --kind
disable --strategy 5`, or restage/commit the prior hash. `HaltRequest`
and the risk-policy kill switch clear the mask sticky, as everywhere.
Ruleset Stage/Commit frames themselves ride `ttl_ns = 0` (never expire
in-ring) — pinned, since a TTL'd Commit that dies in the ring while the
side path already counted `staged` would desynchronize the two state
machines for no benefit.

**Alternative (b): suspend-on-staleness.** vm stops firing while
`last_heartbeat_age > 15 s`, resumes on the next heartbeat, mask
untouched. Safety-conservative but introduces the codebase's only
self-resuming behavior (halt is sticky BY DESIGN; a silently on/off
strategy contradicts that doctrine), and makes backtest↔live behavior
diverge on a variable (worker liveness) the backtest cannot see.

### 7.3 Inert states

No table committed (or table len 0, or slot 5 disabled) ⇒ `on_tick`
falls through on one predictable branch (`active_len == 0`). `--strategy
all` composes vm from day one exactly like ai-exec — booting inert is
normal, not an error.

---

## 8. `StrategySet` integration

- `vm: VmStrategy` lands at slot 5; `BUILT_MASK |= 1 << 5`;
  `mask_for_name("vm")`; bin help text + `run` docs updated.
- The module-doc sentence "slot 5 exists ONLY as a reserved mask bit"
  dies; the Enable-refusal semantics now apply only to slots 6–7
  (still reserved).
- **Test migration:** every test that proves "Enable(5) is refused
  because reserved" flips meaning — G0's live demo itself used
  `enable --strategy 5` as the refusal probe (`enable_refused 1`).
  Post-8g the same probe SUCCEEDS. Tests move to slot 6, and the demo
  runbook note in `phase-8g-progress.md` records the semantics change.
- `on_ai` fan-out: RulesetCommit routes to vm (today: generic member
  fan-out no-op). Enable/Disable/Halt stay set-level. SetParam gains no
  vm ids in v1.

---

## 9. Metrics / capture / audit-replay surface

New (all registered in `paper.rs`, mirrored on the existing 5 s cadence
except where noted):

| name | kind | meaning |
|---|---|---|
| `engine_strategy_enabled_mask` | gauge | the set's live `enabled_mask()` — **G0 demo finding: this observable did not exist**; the demo proved the flip behaviorally via order-flow deltas. Mirrored every 5 s summary. |
| `engine_vm_rows_active` | gauge | active-table `len` (0 = inert) |
| `engine_vm_table_epoch` | gauge | active-table `epoch` (0 = none ever) |
| `engine_vm_fires_total` | counter | rows fired (pre-clamp) |
| `engine_vm_orders_emitted_total` / `_dropped_total` | counter | via StrategyCounters kind="vm" |
| `engine_vm_commit_dropped_total` | counter | in-stream Commit with no/mismatched staged table (§6) |
| `engine_ai_table_push_fail_total` | counter | side-path table-ring push full (§5) |

Existing `engine_ai_ruleset_{staged,committed,rejected}_total` keep
their exact 8f meanings (ingress-side state machine).

**Capture:** unchanged — `ai-cmds.pmlr` already records Stage/Commit
frames (identity in px/qty); the applied flip is recoverable from the
capture (Commit cmd) + metrics. No new capture file.

**`audit-replay` (G0 demo finding):** gains a `slot_kind = 4` section —
the G0 runbook step "audit-replay the run's ai-cmds.pmlr" was
unsatisfiable: `audit_replay.rs` never reads the file (G0 verified the
capture by direct byte decode instead). 8g adds: per-kind counts, seq
continuity (gaps/regressions), heartbeat cadence histogram, Stage/Commit
rows with hash128 hex, TTL'd-at-pop annotations. This closes the runbook
for 8h+ demos.

---

## 10. Alloc-gate plan (append; baseline 33 gates, 0 B/op)

| # | gate (in `bench/tests/alloc_assertions.rs`) | asserts |
|---|---|---|
| 34 | `ruleset_validator_is_zero_alloc` | §4.2 scan of a max-size (256-row) valid ruleset AND a battery of rejects, over `&[u8]` into a prewarmed scratch table: 0 B/op (the `fs::read` documented alloc sits OUTSIDE the validator seam) |
| 35 | `ruleset_table_handoff_is_zero_alloc` | ring push (copy #1) + engine pop (copy #2) + Commit flip: 0 B/op — copies move bytes, not heap |
| 36 | `vm_on_tick_steady_state_is_zero_alloc` | full-table (256 rows) tick storm incl. fires + cooldown re-arms + clamped submits into a placeholder ring: 0 B/op |

Run law unchanged: `--test-threads=1` (process-global CountingAllocator).
New baseline after 8g: **36 gates**. Numbers recorded in the closing
entry like 8f did (33 → 36).

---

## 11. Test plan (house standard: PLAN §21.3/§21.4)

- **Unit** (every public fn: ≥1 happy + ≥1 failure): validator per-rule
  rejects (one test per §4.2 clause), side-path stage/commit/supersede
  against the new push (8f stub tests extended, not replaced), vm
  trigger math (deviation bps, level crossing, both sides, `both`),
  cooldown re-arm, cap clamp at emit, commit-mismatch drop, set slot-5
  enable/disable, reserved-slot refusal MOVED to slot 6 (§8).
- **Proptest** (§21.3): (1) generator builds arbitrary VALID rulesets →
  serializer (test-only) → validator ⇒ accepted, table fields
  roundtrip; (2) arbitrary mutations/truncations of valid bytes ⇒
  never panic, never partially stage (scratch discarded on reject);
  (3) vm eval invariant: over arbitrary tick sequences, Σ emitted
  notional per sym ≤ table per-sym cap and per-order ≤ row cap.
- **Fuzz** (§21.4, the validator parses untrusted bytes):
  `fuzz/fuzz_targets/ruleset_json.rs` over the raw validator seam.
  CI default `-max_total_time=300` like the other targets;
  `.claude/agents/parser-property-tester.md` scope gains the file.
- **Integration** (per-crate `tests/`, fake UDS server per 8f harness,
  `tests/conftest.short_sock_dir()` hygiene): full path — install
  artifact → Stage frame → table pops → Commit frame → flip → ticks
  fire → orders clamp → `disable 5` rollback → restage supersedes.
  Plus: Stage push-full reject, Commit-before-table-pop same-iteration
  ordering (§6 race note), engine boot with vm in `--strategy all`
  inert.
- **Worker (`claude-worker`)**: NO code change (frozen §6 surface);
  existing 202-test suite must stay green untouched — any red here means
  8g leaked into the worker.
- **Live smoke** (pitfall #11; operator-gated like every live boot):
  one paper boot staging+committing a real 1-row ruleset against the
  demo market, `--raw-tap` on, before 8g is declared done.

---

## 12. Ordered implementation checklist (G1…; each step green on the Mac before the next)

1. **Docs-only tidy commit** (if D4 = take): §14 list + CLAUDE.md
   quick-ref fixes (G0 demo finding: `run --config` line is stale —
   `--polymarket-asset-id` is required, `--config` does not exist).
2. `core-types`: `RuleRow`/`RuleTable`/`RuleTableSlot` + size/layout
   compile-time asserts + wire-format.md §3 tables.
3. `ingress-ai`: validator (§4.2) + scratch + universe snapshot arg +
   gate 34; stub tests extended (stage now requires VALID content —
   `{"rows":[]}` fixtures become minimal valid rulesets).
4. Ring wiring: bin builds `Ring<RuleTableSlot, 2>`; `spawn_ai` gains
   producer + universe; side-path push (§5); gate 35 (push/pop halves).
5. `strategy-vm` crate: eval + cooldown + clamps + counters + gate 36;
   proptest + fuzz target land HERE (before set integration).
6. `strategy-set`: slot 5 member + mask + refusal-test migration (§8).
7. `engine`: table-ring consumer + pre-AI-drain pop + vm Commit flip;
   integration tests (§11).
8. Observability: §9 metrics + `audit-replay` slot_kind-4 section.
9. Final gates: full nextest, alloc assertions (36/36, 0 B/op,
   `--test-threads=1`), fuzz check, worker pytest untouched-green,
   operator-gated live smoke (§11), closing entry.

Cargo on the Mac ONLY (pitfall #10; the G0 stale-binary incident is the
newest exhibit — see progress entry: `target/release/multivenue-engine`
predated d9da2b1 because test gates never relink the release bin;
**rebuild `-p cli` before any live boot** joins the runbook).

---

## 13. Design decisions — RESOLVED by operator 2026-08-16 (G0)

| # | decision | options (recommended first) | status |
|---|---|---|---|
| D1 | Table handoff + flip | (a) Stage-time table ring + in-stream Commit flip (§6) / (b) shared double buffer + atomic flip / (c) Commit-time ring push | **LOCKED (a)** |
| D2 | v1 trigger grammar | (a) `cross_deviation` + `level_breach` / (b) `cross_deviation` only / (c) also keyword-on-signal triggers | **LOCKED (a), AMENDED**: legs are venue-explicit via namespaced SymbolIds — BOTH legs may be any asset on any boot-universe venue (operator correction G0; a first-draft PM-only action-leg pin was dropped — live emission is 8i/8j phase-gated, not grammar-gated) (§4.1/§4.2 rule 6) |
| D3 | Committed-table staleness | (a) persists through worker silence; rollback explicit / (b) suspend-on-staleness, resume on heartbeat | **LOCKED (a)** |
| D4 | 8f comment-tidy list | (a) take into G1 as one docs-only commit (+ CLAUDE.md runbook fixes) / (b) park entirely | **LOCKED (a)** |

Pinned unless the operator objects (folded from §4–§10): strict
validator (unknown key/dup key/trailing bytes/exponents/dup rows ⇒
reject); 64 B row / 256-row table; ruleset frames ride `ttl_ns = 0`;
`enabled_mask` gauge; `audit-replay` AiCmd section in-scope; TUI pane
deferred; slot-5 refusal tests migrate to slot 6; worker surface frozen.

---

## 14. 8f comment-tidy triage (S7 deviation 2 list)

Docs-only, zero code motion, one commit (D4a): `core-net` (http1/lib/
transport docs; `NetworkSource::Rss = 3` comment gets the same
"retired, append-only ABI" wording as `SignalSource`), `core-parse`
module doc, `core-types` Slow-class/Signal docs, `tui` (bit-3 doc +
per-bit label array note — bits never renumber; the retired `rss` row
rendering Down is honest and stays), `strategy-latency-arb` `on_signal`
doc, `ingress-polymarket` fnv comment. G0 adds from the live demo:
CLAUDE.md run-command quick-ref (stale `--config`, missing required
`--polymarket-asset-id`, missing `--strategy all` note for AI-cmd
work). If D4b (park): recorded here, untouched until a code commit
naturally visits each file.

---

## 15. Non-goals, restated once more

`strategist.py` cadence, `cli backtest` harness, venue REST consumers —
**8h**. `crates/risk` state machine — **8i** (until then: validator caps
+ emit-time clamp + sticky halt). Live fill producers/dispatchers —
**8j**. Paid APIs (X, Benzinga, Blocknative) — **Phase-6 P&L gate**.
No new trigger families, no TUI pane, no worker changes, no cloud
anything.

---

*Design complete; §13 locked in-session (G0). Implementation starts
only on explicit operator go per house §12.2 convention — the G1
kickoff prompt lives in `docs/phase-8g-progress.md`.*
