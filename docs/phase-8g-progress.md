# Phase 8g — Ruleset Engine (`strategy-vm`): progress log

Session log for Phase 8g ONLY (8f is closed as of `7ca91be`; its log is
`docs/phase-8f-progress.md` and receives no further entries). Authority
chain: `docs/phase-8g-design.md` (§13 LOCKED 2026-08-16) + the latest
entry here supersede the committed plan where they conflict.

---

## 2026-08-16 — Session G0 (Phase 0: design; 8f exit-criteria closeout) — OPENING ENTRY

### 8f live AI-cmd demo — RUN on explicit operator go; PASSED

The 8f closing entry left one exit criterion deliberately open ("AI cmd
→ strategy toggle observed live"). Operator gave the go this session;
runbook executed on the Mac:

| runbook step | result |
|---|---|
| `pgrep -fl multivenue-engine` clean | ✓ (after the stale-binary incident below) |
| paper boot, `AI_INGRESS_HMAC_KEY` set | ✓ HEAD (`7ca91be`) binary, `--strategy all`, ephemeral 64-hex session key passed via environment — **`.env` untouched** |
| §6 push verbs from `claude-worker` | ✓ over UDS+HMAC: `disable --strategy 0`, `enable --strategy 5` (refusal probe), `enable --strategy 0`; worker seq 3–8 (each verb = implicit heartbeat + cmd) |
| `engine_ai_*` counters | ✓ `cmds_total 6`, `enable_refused_total 1` (reserved slot-5 probe), `hmac_fail 0`, `expired 0`, `seq_gap 0`, heartbeat age live |
| strategy-mask flip observed | ✓ behaviorally: paper order flow 34/15 s → **0/15 s** after disable → 32/15 s after re-enable (see finding 3 — no direct mask observable exists yet) |
| SIGINT | ✓ clean exit, twice (boot #2 and #3) |
| `audit-replay` the run's `ai-cmds.pmlr` | ⚠ tool ran clean on venue captures but **has no `slot_kind = 4` section** (finding 4); capture verified by direct byte decode instead: PMLR v2, slot_kind 4, exactly 6 slots — `HB, Disable(0), HB, Enable(5), HB, Enable(0)`, seq 3–8 contiguous |

Demo market: the same live PM market prior live runs used — "Strait of
Hormuz traffic returns to normal by August 31?" YES token
(`1054…7332`, two-sided book), resolved via Gamma at session time; run
dir `~/multivenue/logs/run-1786846027026989000`.

**The 8f §1/§12 exit-criteria checklist is now fully closed.**

### G0 findings (all folded into the 8g design doc)

1. **Stale release binary trap.** `target/release/multivenue-engine`
   dated Aug 15 14:43 — PRE-`d9da2b1`: first boot ran an `ingress-rss`
   thread and no AI lane. Item-17's gates (nextest, alloc assertions)
   build test binaries and rlibs but never relink the release bin.
   **Runbook law from now on: `cargo build --release -p cli` before any
   live boot.** (Design §12 records it.)
2. **Stale-binary shutdown hang, diagnosed then killed by PID.** After
   SIGINT the old binary's engine loop + venue threads exited cleanly
   but the process hung: `sample` showed main in
   `cli::paper::join_reverse` → `pthread_join` on thread `ingress-rss`
   spinning in `clock_gettime` (feed-less RSS loop never observed
   shutdown). A bug class item 16 deleted — HEAD SIGINTs cleanly
   (proven twice). `kill -9 <pid>` of our own demo process after
   diagnosis, per hygiene rules.
3. **`--strategy latency-arb` cannot express AI toggles.** Bare
   `Engine<LatencyArb>` accepts frames (`cmds_total` grew) but
   set-level Enable/Disable have no set to act on, and the
   `engine_strategy_*_active` gauges are which-kind-runs flags
   ("exactly one is 1"), NOT a runtime mask. AI-cmd work needs
   `--strategy all` (or `ai-exec`/`vm`). Additionally
   `StrategySet::enabled_mask()` has **no runtime observable** — no
   gauge, no log line, no TUI row; the demo proved the flip via
   order-flow deltas. Design §9 adds `engine_strategy_enabled_mask`.
4. **`audit-replay` lacks AiCmd support.** `audit_replay.rs` contains
   zero `slot_kind = 4` handling — the 8f runbook's final step was
   unsatisfiable as written. Design §9 puts the AiCmd section in 8g
   scope.
5. **Smaller runbook facts:** CLAUDE.md's quick-ref `run --config …`
   line is stale (`--config` does not exist; `--polymarket-asset-id` is
   REQUIRED since D1/8e); worker verbs require `CLAUDE_WORKER_REPLAY_DIR`
   set even for `push`; engine metrics live on `127.0.0.1:9191`
   (default `METRICS_BIND`). CLAUDE.md fixes ride the D4 tidy commit.
6. **Demo-probe semantics change ahead (design §8):** the demo used
   `enable --strategy 5` as the reserved-slot refusal probe. Once the
   vm member lands, that exact probe SUCCEEDS — refusal tests and any
   future runbook probe migrate to slot 6.

### Design deliverable

`docs/phase-8g-design.md` written and reviewed this session; §13 put to
the operator and **LOCKED**: D1 (a) Stage-time table ring + in-stream
Commit flip; D2 (a) `cross_deviation` + `level_breach`, AMENDED — legs
are venue-explicit via namespaced SymbolIds and BOTH legs may be any
asset on any boot-universe venue (operator corrected a first-draft
PM-only action-leg pin: `Order` has no venue field — the namespaced
`sym` is the venue targeting, `ctx.submit` is venue-agnostic, and live
emission is 8i/8j phase-gated, not grammar-gated); D3 (a) committed
table persists through worker silence; D4 (a) tidy list taken into G1
as one docs-only commit. Pinned-unless-objected list in §13 stands
unobjected.

### Hygiene / anomalies

- No git mutations this session (no commits, no pushes, no fetches, no
  branches; read-only `git log`/`status` only). The two new docs
  (`phase-8g-design.md`, this file) are **uncommitted working-tree
  files** — committing them is the operator's call.
- Push anomaly (S4–S7): not re-examined (no fetches); local ref still
  `origin/main = 38e599b` per the 8f closing entry. Record, never act.
- `.env` untouched (verified: demo key was ephemeral, generated to
  `/tmp`, exported per-process, shredded after SIGINT). `~/multivenue/
  run/ai.sock` left behind by the listener — harmless, recreated per
  boot.
- Sandbox used for greps only; all cargo/process work on the Mac via
  RustRover MCP (`executeInShell=true`, ≤45 s, nohup+poll for the
  build). One release relink: `cargo build --release -p cli` (19.8 s —
  rlibs were already post-`d9da2b1` from the item-17 gates; no
  stale-rmeta, no false-green ambiguity: binary behavior diffed).

### Resume point

Phase 8g design is LOCKED; no 8g code exists. Next session is G1 on
explicit operator go — prompt below.

---

## G1 kickoff prompt

```
Stage 2 — 8g RULESET ENGINE, SESSION G1 (checklist items 1–3), MAIN
CHECKOUT /Users/darkcite/trading-engine-multivenue.
8f is CLOSED (7ca91be); 8g design is LOCKED — docs/phase-8g-design.md
§13 (D1a stage-ring + in-stream flip, D2a deviation+level with
venue-explicit legs [BOTH legs = any asset on any boot-universe venue;
live emission stays 8i/8j phase-gated], D3a table persists, D4a tidy
commit) + the G0 entry in docs/phase-8g-progress.md supersede the
committed plan. NO push, NO rebase, NO history rewrite,
NO new branches, NO git ops without operator ask (NOTE: the two G0
docs may still be uncommitted working-tree files — ask the operator
whether to fold them into commit 1). Do NOT touch .env. 8g notes go
ONLY to docs/phase-8g-progress.md.
Verify get_project_modules against the main checkout FIRST; if the MCP
won't attach, stop.
REQUIRED READING, in order:
1. docs/phase-8g-design.md — §3 (POD layout), §4 (grammar + validator
   rules 1–8), §5 (side-path delta), §12 (checklist), §13 (locked)
2. docs/phase-8g-progress.md — G0 entry (demo findings 1–6, esp. the
   release-relink law and the slot-6 probe migration)
3. crates/ingress-ai/src/ruleset.rs (stub being grown) +
   crates/ingress-ai/src/listener.rs §4.4 step-8 seam
4. docs/wire-format.md AiCmd rows; docs/risk-policy.md caps
5. CLAUDE.md pitfalls #10/#11
G1 SCOPE (design §12, each step green on the Mac before the next):
item 1 — D4 docs-only tidy commit, ONE commit: S7 deviation-2 list
  (core-net incl. NetworkSource::Rss retired-ABI wording, core-parse
  module doc, core-types Slow-class/Signal docs, tui bit-3 + label
  array note, strategy-latency-arb on_signal doc, ingress-polymarket
  fnv comment) + CLAUDE.md quick-ref (drop --config, add REQUIRED
  --polymarket-asset-id, note --strategy all for AI-cmd work, add the
  release-relink law to the run recipe). Zero code motion — docs/
  comments only; grep-verify no token drift.
item 2 — core-types: RuleRow/RuleTable/RuleTableSlot per design §3
  EXACTLY (repr(C, align(64)), Copy, 64 B row / 16 KiB+64 table,
  compile-time size asserts), wire-format.md §3 gains the rows;
  happy+failure unit tests per house rule.
item 3 — ingress-ai validator per design §4.2 rules 1–8 (byte scanner,
  NO serde_json; scratch RuleTable owned by the side path, reused;
  fs::read alloc stays documented op-cadence), universe snapshot arg
  (Arc<[u32]> sorted; BOTH legs universe-membership only, NO venue
  restriction — D2 as amended), stub tests extended ({"rows":[]} fixtures become
  minimal VALID rulesets — empty rows is now a §4.2-rule-4 REJECT),
  one unit test per validator rule, plus alloc gate 34
  ruleset_validator_is_zero_alloc (baseline 33 → 34, 0 B/op,
  --test-threads=1).
Ring wiring (item 4) and beyond are G2+; do NOT start them.
Cargo on the Mac ONLY (pitfall #10; sandbox = greps only). Stale-rmeta
playbook (S4 addendum: clean ALL workspace-local touched crates on
impossible errors); S7 false-green guard (RustRover background check
keeps caches warm — cargo clean -p <touched> before trusting a <1 s
green); G0 law: cargo build --release -p cli before ANY live boot
(none is planned for G1 — no live venues, no engine run).
Test hygiene: sockets via tests/conftest.short_sock_dir()
(/tmp/cw-ai-<pid>-*/); METRICS_BIND test-local (NEVER 9191);
MULTIVENUE_LOG_DIR test-local (NEVER ~/multivenue/logs); NEVER run
multivenue-engine run or connect live venues (no operator gate exists
in G1); no kill/pkill by name (by-PID of own test processes only,
after diagnosis).
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long runs via
nohup > /tmp/8g-*.log & then poll; projectPath =
/Users/darkcite/trading-engine-multivenue. macOS landmines: AF_UNIX
sun_path cap (short_sock_dir), SO_RCVTIMEO EINVAL on peer-closed UDS,
std::thread::scope panic hangs without StopOnDrop, sample <pid> for
hang diagnosis. Push anomaly is KNOWN (S4–S7: origin/main local ref
38e599b): record, never act. One-line status after each commit; ask
before anything ambiguous. If context runs short: write interim state
+ exact resume point + relaunch prompt into
docs/phase-8g-progress.md, then tell me.
```

---

## 2026-08-16 — Session G1 (checklist items 1–3) — CLOSED

Three commits, each gated green on the Mac before the next
(§12 discipline):

| item | commit | scope |
|---|---|---|
| 1 (D4a) | `4690c06` | docs-only tidy: S7 deviation-2 six-file list + CLAUDE.md run quick-ref (stale `--config` dies; REQUIRED `--polymarket-asset-id`; `--strategy all` note; release-relink law) + **both G0 docs folded in** (operator answer this session). Zero code motion — grep-verified every changed `crates/` line is a comment. |
| 2 | `f21ac14` | `core-types`: `RuleRow`/`RuleTable`/`RuleTableSlot` per §3 + `fnv1a_64` (pub const fn, pins `name_h` next to the type) + `RULE_TABLE_ROWS` + `RuleRow` trigger/side consts + `ZERO`/`EMPTY` + size AND offset compile-time asserts + 8 unit tests; wire-format.md gains both offset tables (between ChannelEvent and Capture files, marked in-process-only / never captured). |
| 3 | `600a213` | `ingress-ai`: `validate_ruleset` (§4.2 rules 1–8, flat byte scanner, no serde_json, zero auxiliary storage — cross-row rules recomputed against the admitted prefix), `RulesetReject` per-rule enum, side path gains sorted `Arc<[u32]>` universe + `Box<RuleTable>` scratch (one documented boot alloc) + epoch stamp + `staged_table()` diagnostic; stub tests upgraded (`{"rows":[]}` = canonical rule-4 reject) + one test per rule; **gate 34** `ruleset_validator_is_zero_alloc` (baseline 33 → 34). |

### Operator decisions taken in-session

1. **G0 docs → commit 1** (fold, not separate/parked).
2. **§3 `RuleRow._pad` amended `[u8; 13]` → `[u8; 21]`.** Declared
   fields sum to 43 B; 13 left 8 *implicit* compiler tail bytes —
   size assert alone can't catch it (align(64) pads to 64 either
   way), the explicit-zeroed-padding doctrine and the
   fully-explicit-layout test can. Recorded in the struct doc,
   wire-format.md, and the item-2 commit message.

### Documented interpretations (validator, item 3 — flag on G2 review)

- **"Trailing bytes ⇒ reject" = trailing NON-whitespace.** ASCII
  ws after the closing brace is tolerated: JSON-insignificant, and
  the hash already pins content byte-exactly (rejecting a trailing
  newline buys no integrity). Non-ws trailing byte ⇒ Grammar.
- **"First failure wins" = streaming.** Rules apply in §4.2 order at
  each scan position; across positions the earliest-position failure
  wins (single pass, no row storage beyond the table — the
  whole-file phase model would need multi-pass for zero gain).
  Rule 4 lower fires after grammar completes (order 2 < 4); upper
  fires the moment a 257th row opens.
- **`max_risk_usd` must be > 0** — classified rule 3 (a non-positive
  notional cap is meaningless); the ≤ $100 check stays rule 7.
- **`ref`-present-on-`level_breach` ⇒ Symbol (rule 6)** per the
  design's own wording; `level`-present-on-`cross_deviation` ⇒
  Grammar (§4.1 shape). Asymmetric, design-literal.
- **`scan_u64` wrap closed** with a 10-digit literal cap on u32
  fields (wrapping scanner is fine for venue feeds, not for
  untrusted config).
- **Failed restage clears the parked scratch** (discard-on-reject
  wipes `len`) while `staged()` keeps the prior hash — pinned in
  test + `staged_table` docs; harmless because the item-4 ring copy
  at stage time is the durable handoff. The parked scratch is a G1
  diagnostic only.
- **`fnv1a_64` lives in `core-types`** (not ingress-ai): §3 defines
  `name_h` as FNV-1a 64, so the hash sits next to the type; also
  makes the item-1-tidied ingress-polymarket comment ("same hash
  core-types uses") true — it was aspirational for one commit.

### Wiring state after G1 (for item 4)

- `RulesetSidePath::new` now takes `universe: Arc<[u32]>`;
  `cli/paper.rs::spawn_ai` passes an **empty placeholder** —
  fail-closed: every row-bearing ruleset rejects at rule 6 until
  item 4 threads the real sorted discovery snapshot through
  `spawn_ai` alongside the `Ring<RuleTableSlot, 2>` producer. The
  `try_push` seam is marked in `stage()` ("Item 4 (G2) … lands
  HERE").
- Live-boot behavior delta (no live boots ran in G1): the 8f stub
  STAGED any hash-matching artifact; HEAD rejects invalid content
  and — until item 4 — rejects all row-bearing content (empty
  universe). Correct-conservative mid-phase.

### Gates at close (all Mac)

- workspace `cargo nextest run`: **963/963** (942 at 8f close).
- release alloc assertions `--test-threads=1`: **34/34, 0 B/op**
  (gate 34 confirmed by name in the run log).
- `cargo check --workspace` clean at every step; item-2 check
  verified as a REAL green (22 dependent crates rechecked, not a
  warm-fingerprint no-op).
- ingress-ai suite 56/56 (uds_loopback seam tests use a counting
  closure — routing-only, unaffected by validator semantics).

### Hygiene / anomalies

- Git: the three scope commits + this log commit + one follow-up
  (`747c1a8`: Cargo.lock for the item-3 dep additions, missed in
  `600a213` — committed separately, NOT amended, per the
  no-history-rewrite rule); no push, no fetch, no branch, no
  history ops. Push anomaly unchanged (origin/main local ref
  `38e599b`): recorded, not acted on.
- `.env` untouched. No engine run, no live venues, no sockets
  beyond the existing test suites' own fixtures.
- Sandbox: greps only; all cargo/git on the Mac via RustRover MCP
  (nohup + poll for every long run).

### Resume point

G1 items 1–3 CLOSED. Next session is **G2 = item 4** (design §12):
bin builds `Ring<RuleTableSlot, 2>`; `spawn_ai` gains producer +
real universe (replace the paper.rs empty placeholder); side-path
push per §5 (push-full ⇒ reject, counted;
`engine_ai_table_push_fail_total`); gate 35
`ruleset_table_handoff_is_zero_alloc` (push/pop halves; baseline
34 → 35). Items 5+ (strategy-vm crate, proptest + fuzz) stay G3+.

---

## 2026-08-16 — Session G2 (checklist item 4 ONLY) — CLOSED

One scope commit, green on the Mac before committing (§12
discipline), plus this log commit:

| item | commit | scope |
|---|---|---|
| 4 | `0e2f68c` | Table-handoff ring wiring (§5/§6, D1a push half): `core-types` `RULE_TABLE_RING_SLOTS = 2` (pow2 const-asserted, next to `AI_RING_SIZE`); `RulesetSidePath` gains the `Ring<RuleTableSlot, 2>` producer and `stage()` runs the §5 flow (validate → candidate-epoch stamp → `try_push` scratch = documented 16 KiB copy #1 → ok ⇒ staged/committed-cleared/inc staged; push-full ⇒ reject + NEW `AiIngressStatus::table_push_fail` counter); `cli::build_ai_universe` builds the REAL §4.3 snapshot (PM/BN pair flags + discovery-gated okx/deribit/hl table ids, sorted strict-ascending, deduped) once in the bin after discovery, before spawns — the G1 empty-universe fail-closed placeholder is GONE; `Rings.ruleset_tables` + bin split, consumer parked; gate 35 (34 → 35). |

### Documented interpretations (item 4 — flag on G3 review)

- **Candidate epoch ⇒ gapless consumer epochs.** "Stamp epoch →
  try_push" is kept literally (`scratch.epoch` is stamped before the
  push), but `self.epoch` commits only when the push lands — a
  push-full reject burns nothing, so consumer-visible epochs run
  1, 2, 3, … with no gap (the §3 "successful-stage counter" reading;
  pinned in `epoch_is_gapless_monotonic_across_push_full_rejects`).
- **Push-full also discards the scratch** (`len = 0`) — G1's
  discard-on-reject contract extended to the push reject: a
  never-staged table must not linger in the diagnostic surface, while
  `staged()`/`committed()` keep their prior values per §5 (pinned:
  only a SUCCESSFUL Stage supersedes a Commit —
  `push_full_reject_does_not_supersede_commit`).
- **`table_push_fail` increments IN ADDITION to `ruleset_rejected`**
  (§5 says "inc rejected"; the dedicated counter isolates the cause).
  Total-vs-cause pairing documented on the field; §9 /metrics
  registration stays item 8.
- **`RULE_TABLE_RING_SLOTS` lives in `core-types`** next to
  `AI_RING_SIZE` (shared by ingress-ai, cli, bench; the slot type
  `RuleTableSlot` already lives there).
- **Universe builder is `cli::build_ai_universe`, a pub fn** (not
  bin-inline) so the sorted/strict-ascending/deduped contract is
  unit-tested against the real venue tables; the bin calls it once.
  RPC contributes nothing (streams block headers — no instrument
  universe, per its 8e discovery note). Ids enter verbatim as wired:
  raw `--polymarket-sym-id`/`--binance-sym-id` + namespaced
  `make_symbol_id` venue-table ids.
- **Byte-identical handoff is proven by raw-byte compare** of the
  popped slot vs the parked scratch — well-defined precisely because
  of the G1 §3 pad amendment (`repr(C)` + all padding explicit and
  zeroed).

### Wiring state after G2 (for items 5+/7)

- Ring: `Rings.ruleset_tables` (`Arc<Ring<RuleTableSlot, 2>>`),
  split in the bin next to the AI lane. Producer half → `spawn_ai` →
  `RulesetSidePath` (ingress-ai thread). Consumer half **parked** in
  the bin as `_ruleset_table_cons` — alive for the process lifetime,
  never popped; item 7 hands it to the engine's pre-AI-drain pop.
  Key-unset boots drop the producer (the `ai`-lane unspawned shape).
- Live-boot behavior delta: none observable yet — staged tables now
  accumulate in the ring (≤ 2) instead of parking only in the
  scratch; nothing consumes them until item 7. Two live stages
  without an engine drain now reject the third
  (`table_push_fail`) — correct-conservative mid-phase, impossible
  at operator cadence once the drain exists.
- `spawn_ai` signature: `+ table_producer: Producer<RuleTableSlot,
  RULE_TABLE_RING_SLOTS>`, `+ universe: Arc<[u32]>` (from
  `build_ai_universe`, logged at boot as
  "ai: ruleset boot-universe snapshot built").

### Gates at close (all Mac)

- workspace `cargo nextest run`: **971/971** (963 at G1 close; +8 =
  4 ingress-ai side-path tests, 2 cli universe tests, 1 core-types
  ring-slots lock, gate 35).
- release alloc assertions `--test-threads=1`: **35/35, 0 B/op**
  (gate 35 `ruleset_table_handoff_is_zero_alloc` confirmed by name —
  push + pop halves + push-full reject path; the Commit-flip third
  joins with the vm member per the §12 item-4 parenthetical).
- `cargo check --workspace` clean — real green per the false-green
  guard (23 "Checking" lines from `core-types` up, zero warnings).
- No Cargo.lock delta (no dependency changes; the G1 747c1a8 lesson
  checked explicitly).

### Hygiene / anomalies

- Git: one scope commit (`0e2f68c`) + this log commit; no push, no
  fetch, no branch, no history ops. Push anomaly unchanged
  (origin/main local ref `38e599b`): recorded, not acted on.
- `.env` untouched. No live boots, no engine runs, no live venues
  (none planned in G2); test sockets only via the existing suites'
  fixtures.
- Sandbox: greps only; all cargo/git on the Mac via RustRover MCP
  (`executeInShell=true`, nohup + poll for every long run).

### Resume point

G2 (item 4) CLOSED. Next session is **G3 = item 5** (design §12):
`crates/strategy-vm` — eval (§7: cross_deviation/level_breach
trigger math, cooldown re-arm via lazy stamps, emit-time cap clamp,
StrategyCounters kind="vm") + gate 36
`vm_on_tick_steady_state_is_zero_alloc` (baseline 35 → 36) +
proptest + fuzz target (`fuzz/fuzz_targets/ruleset_json.rs`) land
WITH the crate, before set integration. Items 6+ (set slot 5 +
refusal-test migration to slot 6, engine flip, §9 observability,
final gates) stay G4+.

