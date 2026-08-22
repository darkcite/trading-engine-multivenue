# 8h kickoff prompt — SESSION H0 (paste verbatim into a fresh session)

Stage 2 — 8h RESEARCH LOOP, SESSION H0 (Phase 0: DESIGN ONLY — no code),
MAIN CHECKOUT /Users/darkcite/trading-engine-multivenue.
HEAD 39e6542; PHASE 8g CLOSED (39e6542 = G7 closing log commit; 8f closed
7ca91be). Stage-2 status: 8f CLOSED, 8g CLOSED, 8h = LAST Stage-2 phase
(phase-8-plan §12 row: data_fetcher + strategist (Fable 5) + REAL
`cli backtest` harness + gates + rollback; exit criteria =
Fable-5-authored ruleset auto-promoted after passing backtest, trading
in paper, AND a forced-underperformance rollback demonstrated). Stage 3
(8i risk / 8j dispatchers / 8k live ramp) stays OUT.
Baselines at 8g close: alloc gates 36/36 0 B/op (`--test-threads=1`),
workspace nextest 1029/1029, worker pytest 202, fuzz `ruleset_json`
72.3M runs / 301 s clean (cargo-fuzz v0.13.2 IS installed now; runs
`+nightly`; in-repo `cargo install` trips the 1.88.0 toolchain pin —
install `+stable` from `$HOME` if ever needed again).
NO push, NO rebase, NO history rewrite, NO new branches, NO git ops
without operator ask. Do NOT touch .env. 8h notes go ONLY to
docs/phase-8h-progress.md (CREATE it this session, G0-style: H0 opening
entry + the H1 kickoff prompt; the new docs stay working-tree files —
ask the operator whether to commit; 8g-G0 precedent was
fold-into-commit-1 at H1).
Verify get_project_modules against the main checkout FIRST; if the MCP
won't attach, stop.

REQUIRED READING, in order:
1. docs/phase-8-plan.md — §8.7 ("Autonomous research loop (8h): fetch →
   analyze (Fable 5) → backtest → push", incl. §8.7.3 cli backtest),
   §8.2/§8.2.1 (worker daemon + verbs; data_fetcher and strategist
   component sketches; MODEL_STRATEGIST = "claude-fable-5"), §8.1
   (RSS→worker, done in 8f), §12 stage table (8h row + exit criteria),
   §13 risks (Anthropic budget / prompt-cache note — strategist cost
   control is a DESIGN SECTION, not an afterthought).
2. docs/phase-8g-design.md §15 + docs/phase-8g-progress.md G7 CLOSING
   entry — what 8h inherits: the §5.1 SHIM SEAM the G7 smoke used
   (fake `multivenue-engine` on PATH emitting schema-1 stdout) is
   RETIRED BY the real harness; mask-49 compose-if-configured boot
   fact; slot-6 refusal-probe doctrine; worker seq namespace continues
   across boots (state.db); ~/multivenue/worker/market-map.json is
   ABSENT — 8h name resolution must own creating/maintaining it.
3. claude-worker/src/claude_worker/backtest.py — THE FROZEN CONTRACT
   the harness must satisfy: ENGINE_BINARY = "multivenue-engine", argv
   `backtest --ruleset R --replay-dir D --split 70/30`, schema-1 JSON
   stdout (ruleset_hash = full sha256 of the file bytes;
   oos{net_pnl_usd, trades, trading_days, max_drawdown_usd};
   bounds{max_order/symbol/total_notional_usd}), GateThresholds
   numbers (risk-policy caps). The HARNESS conforms to the WORKER (202
   tests pin the contract), never vice versa.
4. claude-worker daemon.py + commander.py + llm.py + state.py — the
   serve loop the strategist rides; stage_ruleset/commit_ruleset are
   the ONLY promote path (gates in code, no override flag exists).
5. docs/prompts/ai-session.md §4 — the manual loop 8h automates
   verb-for-verb; docs/wire-format.md (PMLR formats the harness
   replays); docs/risk-policy.md (the cap/DD numbers the gates
   enforce).
6. crates/cli/src/audit_replay.rs + crates/core-io (PmlrReader,
   SlotCapture) — the existing replay read surface the harness builds
   on (per-venue ticks + engine fills).

H0 SCOPE (design only — NO code, NO cargo builds; read-only greps and
doc/diagram work; G0 precedent). Deliverables A–E, in order:

A. WRITE docs/phase-8h-design.md (the 8h plan, .md, house format:
   scope/non-goals, component table, semantics §§, observability §,
   alloc-gate plan, test plan, ordered checklist, decisions table,
   comment-tidy triage if any). Must cover at minimum:
   - data_fetcher completion incl. the venue REST consumers deferred
     from 8g §15 (rate-budgeted, boot/cold only — never hot path);
   - strategist serve loop on Fable 5: prompt-cache budget plan,
     cadence guardrails, artifact/feature inputs, Tier-1-only output
     (rulesets; Tier-2 crates stay operator-gated per §8.6);
   - the REAL `multivenue-engine backtest` subcommand: replay-driven
     eval of a candidate ruleset over PMLR capture, deterministic
     (seeded, capture-ordered), fill model + fee + latency penalty
     (put the model to the operator as a decision), 70/30 split
     semantics, schema-1 stdout EXACTLY per backtest.py;
   - gates + auto-promotion + ROLLBACK: define the
     forced-underperformance trigger precisely (metric, window,
     threshold, action = disable-5 / restage-prior-hash) and how the
     demo forces it;
   - hot-path impact statement: backtest is an OFFLINE cli path —
     expected hot-path delta ZERO; baseline stays 36 gates unless a
     genuinely hot seam appears (justify any append);
   - test plan per PLAN §21.3/§21.4: proptest + fuzz for ANY new
     untrusted-bytes parser (harness reads capture files — reuse the
     hardened core-io readers, do not hand-roll new ones without
     fuzz); worker fake-harness tests stay green UNTOUCHED (202) —
     add real-harness integration coverage alongside, never instead;
   - non-goals restated: 8i/8j/8k, paid APIs (Phase-6 P&L gate),
     options (deferred plan = deliverable D), live emission.

B. ACTUALIZE docs/phase-8-architecture.svg (dated Aug 15 — pre-8f/8g
   close). First bring it to 8g-close truth: AI lane (UDS listener +
   HMAC + AiCmd ring), ruleset side path (validator + scratch +
   Ring<RuleTableSlot,2>) → engine pre-AI-drain pop → strategy-vm
   slot-5 member + in-stream Commit flip, §9 metrics surface,
   ai-cmds.pmlr capture + audit-replay ai section, worker verbs/serve
   split. THEN overlay the 8h components (data_fetcher, strategist,
   backtest harness, promotion/rollback arrows) visually marked as
   8h-new. One SVG, renderable (verify it opens), staged next to the
   design doc.

C. MULTILEG READINESS section in the design doc (MANDATORY): prove or
   amend the evolution path for multileg strategies across ALL
   venues. Audit what exists TODAY: D2-amended venue-explicit legs
   (namespaced SymbolIds; venue-agnostic ctx.submit; both legs any
   boot-universe venue), cross-arb's MarketGroup precedent, RuleRow
   64 B fixed layout (2 legs max: sym + ref, SINGLE action leg).
   Specify the v2 shape WITHOUT building it: multi-action-leg rows
   (leg count, per-leg side/ratio/cap), atomicity + partial-fill
   policy (paper semantics now; 8i/8j make it real), 64 B row fit vs
   leg-table indirection (256-row table budget), validator rule
   deltas, per-sym/table cap composition across legs. PIN what 8h
   must NOT do that would close the door — in particular the backtest
   harness MUST replay MULTI-VENUE capture with cross-venue
   simultaneous fill modeling from day one; a single-venue replay
   design would make multileg backtests impossible later. Put v2
   timing (design-only vs first slice in 8h) to the operator.

D. DEFERRED OPTIONS-SUPPORT PLAN — NEW docs/options-support-plan.md,
   proposed NOT scheduled (Phase 9+ candidate, P&L-gated exactly like
   paid APIs). Venues WITH listed options today: Deribit (flagship),
   OKX, Binance (European options, separate WS endpoint). HL and PM
   have NONE (HIP-4 binaries are digital-option-LIKE payoffs, not an
   options instrument class — say so explicitly). The plan must
   cover, per venue: discovery of strike×expiry chains (universe
   explosion vs the existing 8e per-venue discovery + SymbolId
   venue-table capacity — venue byte + 24-bit table space), tick-lane
   field deltas (mark/IV/greeks/underlying vs the fixed Tick POD —
   parallel lane vs POD evolution, ABI append-only doctrine),
   trigger-family candidates (IV level/skew/term-structure triggers;
   delta-hedged multileg interplay with deliverable C), risk-policy
   treatment (premium-paid vs notional-at-risk caps; assignment/
   exercise semantics where applicable), dispatcher + signing deltas
   (8j interplay; Deribit/OKX/Binance auth models), capture/replay
   format impact, and a rollout order with an explicit entry gate
   (which P&L/soak evidence unlocks it). Nothing from this document
   lands in 8h.

E. docs/phase-8h-progress.md — H0 opening entry (decisions LOCKED
   table, diagram delta summary, multileg verdict, options-plan
   pointer, hygiene/anomalies) + the H1 kickoff prompt in the house
   format (this document's shape: authority line, baselines, required
   reading, scope items with green-gates, landmines, session facts).

Decisions to PUT TO OPERATOR in the design §13-equivalent (draft
recommended-first, LOCK in-session; do not silently decide):
strategist cadence/trigger + API budget guardrails; backtest fill/fee/
latency model + determinism guarantees; promotion policy (full
auto-commit vs stage-only + operator commit — reconcile with the
"auto-promoted" exit criterion) + the forced-underperformance rollback
trigger definition; data_fetcher venue-REST scope + rate budget;
multileg v2 timing (C); market-map.json ownership/creation; extent of
real-harness integration tests vs the untouched 202.

Cargo on the Mac ONLY if anything at all runs (pitfall #10; sandbox =
greps/file-reads only) — H0 is design-only, so no builds, no tests, no
live boots (no operator gate exists in H0; G0 ran a demo ONLY because
8f left an exit criterion open — 8g left NONE). Stale-rmeta playbook +
false-green guard apply to any read-only check you do run.
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long runs via nohup >
/tmp/8h-*.log & then poll; projectPath =
/Users/darkcite/trading-engine-multivenue. zsh eats bare === in echo
chains. macOS landmines: AF_UNIX sun_path cap, SO_RCVTIMEO EINVAL on
peer-closed UDS, std::thread::scope panic hangs without StopOnDrop,
sample <pid> for hang diagnosis. Push anomaly is KNOWN (origin/main
local ref 38e599b): record, never act. One-line status after each
commit (if the operator authorizes any); ask before anything
ambiguous. If context runs short: write interim state + exact resume
point + relaunch prompt into docs/phase-8h-progress.md, then tell me.
