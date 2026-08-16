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
