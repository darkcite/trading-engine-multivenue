# Phase 8h — Autonomous research loop: Design (Phase 0, no code)

`data_fetcher` completion + strategist (Fable 5) + the REAL `multivenue-engine
backtest` harness + gates + auto-promotion + rollback. Authored session H0
(2026-08-16), HEAD `39e6542` (8g closed). Authority: `docs/prompts/8h-kickoff.md`;
parent plan `docs/phase-8-plan.md` §8.7/§8.2/§12/§13. Decisions §14 were put to
the operator and LOCKED in-session (H0). Baselines at 8g close: alloc gates
36/36 0 B/op, workspace nextest 1029/1029, worker pytest 202, fuzz
`ruleset_json` 72.3M runs clean.

**The frozen contract:** `claude-worker/src/claude_worker/backtest.py` — argv
`multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`, schema-1
JSON stdout, GateThresholds numbers. 202 worker tests pin it. **The harness
conforms to the worker, never vice versa.** The §5.1 fake-`multivenue-engine`
shim seam the G7 smoke used is RETIRED by this phase's real subcommand.

---

## 1. Scope and non-goals

**In scope (8h):**

- The REAL `backtest` subcommand on the `multivenue-engine` binary (§3–§5):
  deterministic replay-driven evaluation of a candidate ruleset over PMLR
  capture through the real `strategy-vm` evaluator, multi-venue merged from
  day one (§16 door-closer), strict-cross fill model + fees + latency penalty
  (H-D1), 70/30 split, schema-1 stdout byte-conformant to `backtest.py`.
- `data_fetcher` completion (§6): the venue REST consumers deferred from 8g
  §15 (PM Gamma, OKX candles, Deribit chart data, HL candleSnapshot — H-D5),
  `RestBudget` finally wired, `--no-rest` made real, and ownership of
  `~/multivenue/worker/market-map.json` (bootstrap + additive refresh, H-D7).
- `strategist.py` (§7): serve-only Fable-5 proposal loop on a 6 h cadence with
  hard API-budget guardrails and Anthropic prompt caching (H-D4). Tier-1
  output only (rulesets); Tier-2 crate drafts stay operator-gated per plan
  §8.6 and are NOT automated here.
- Auto-promotion (§8.1–§8.2, H-D2): gates pass ⇒ serve installs the artifact
  into `$AI_RULESET_DIR`, then stage → commit through the existing (frozen)
  `backtest.stage_ruleset`/`commit_ruleset` functions. Paper only.
- Rollback (§8.3–§8.4, H-D3): walk-forward re-backtest monitor with a precise
  forced-underperformance trigger; action = `disable --strategy 5` +
  restage/commit of the prior gates-passed hash. Demo per §8.5.

**Out of scope (unchanged gates):** `crates/risk` (8i); venue dispatchers /
live fill producers (8j); live ramp (8k); paid APIs (Phase-6 P&L gate);
options (deferred plan: `docs/options-support-plan.md`, Phase 9+ candidate,
P&L-gated); multileg v2 *code* (design-only §16, H-D6); live emission of any
kind; TUI panes; **new worker CLI verbs** (the 7-verb surface is frozen by
`test_verb_surface_is_exactly_section_6` — the strategist lives inside
`serve`); a `PaperDispatcher` fill model (noted for 8i, §8.3 rationale).

**Exit criteria (plan §12, 8h row):** a Fable-5-authored ruleset is
auto-promoted after passing the real backtest, trading in paper, AND a
forced-underperformance rollback is demonstrated.

---

## 2. Component breakdown

| component | crate / module | change |
|---|---|---|
| `backtest` subcommand | `crates/cli` (`Cmd::Backtest` + new `src/backtest.rs`) | NEW — offline path, `audit_replay.rs` doctrine (allocates; never loaded by the engine loop) |
| Capture discovery + k-way merge | `cli::backtest` over `core_io::PmlrReader` | NEW — the cross-venue merged timeline `audit-replay` never needed (§3.2) |
| Virtual clock rebase | `cli::backtest` | NEW — VIRT_T0 continuous timeline across runs (§3.3) |
| Candidate validation | `ingress_ai::validate_ruleset` | REUSED verbatim — universe = capture-observed syms (§3.5); fuzz-hardened (72.3M) |
| Evaluator | `strategy_vm::VmStrategy<N>` | REUSED verbatim — real `receive_table` + `on_ai(Commit)` flip path; N monomorphized large for backtest (§3.6) |
| Fill/fee/latency model + accounting | `cli::backtest::fill` | NEW (§4) — strict-cross maker, per-venue Δ + fee tables, fixed-point equity/DD |
| schema-1 stdout | `cli::backtest::report` | NEW (§5) — exact `backtest.py` contract |
| REST consumers | `claude-worker` `features.py` + new `fetchers.py` | NEW consumers on the pinned `RestBudget` (§6.1) |
| market-map ownership | `claude-worker` fetcher path | NEW — bootstrap + additive refresh (§6.2) |
| `strategist.py` | `claude-worker` | NEW module, serve-only (§7); `MODEL_STRATEGIST` gets its first consumer |
| `llm.py` | `claude-worker` | prompt caching (`cache_control`), usage accounting, strategist token budget (§7.2/§7.5) |
| serve composition | `daemon.py` | research cycle: fetch → strategist → backtest → promote → monitor (§9) |
| `state.py` | `claude-worker` | additive: attribution params (`model`, `thesis` — columns pre-provisioned in the S5 schema, no migration), budget/rollback events (§7.5/§8.4) |

Hot-path additions: **none** (§11).

---

## 3. The harness: replay substrate

### 3.1 Inputs and capture discovery

`--replay-dir D` accepts EITHER a single `run-<epoch_ns>/` directory OR a log
root containing `run-*` children (the worker's default passes
`CLAUDE_WORKER_REPLAY_DIR`, which is the engine's `MULTIVENUE_LOG_DIR` root —
so multi-run is the common case, and it is what makes the ≥ 2
`trading_days` gate satisfiable at all). Runs are ordered by the `epoch_ns`
parsed from the directory name and cross-checked against each file's PMLR
header `epoch_ns`.

Per run, the harness opens what exists of
`{pm,bn,okx,rpc,deribit,hl}-ticks.pmlr` via `PmlrReader<Tick>` (zero-copy
mmap `records()`). Missing venue files are tolerated (a run captures only
spawned venues). `*-events.pmlr`, `*-signals.pmlr`, `ai-cmds.pmlr` and
`engine-fills.pmlr` are NOT consumed in v1: the vm evaluates ticks only; the
candidate table is injected directly (§3.6), and the harness synthesizes its
own fills (`engine-fills.pmlr` is header-only in paper today —
`PaperDispatcher::try_next_fill() -> None`). Documented non-consumption, not
an oversight.

### 3.2 Deterministic k-way merge (multi-venue from day one)

Within a run, all files share one monotonic clock (one `epoch_ns` threaded
into every `PmlrCapture::open`), so `ts_ns` is comparable across venues
inside a run. The merge is a k-way heap over the per-venue tick slices with
the total order:

```
(ts_ns, venue byte, per-file record index)
```

`venue_seq` is deliberately NOT an ordering key (audit-replay doctrine: it is
venue-specific and informational — BN gaps are legitimate, Deribit's is a ms
timestamp). The tie-break makes the order total and stable ⇒ bit-identical
merges. Cross-run records are NEVER interleaved: runs are disjoint time
windows replayed in `epoch_ns` order.

**Multileg door-closer (§16.3, pinned):** the merge, the book state, and the
fill engine are venue-agnostic from day one — open orders are keyed by
namespaced `SymbolId` (venue rides bits 31..24), never by "the Polymarket
order". A single-venue replay design is forbidden; it would make multileg
backtests impossible later.

### 3.3 Virtual clock rebase (VIRT_T0)

`ts_ns` is monotonic-clock, not wall — its base is arbitrary per boot, so (a)
cross-run values are incomparable and (b) small-uptime captures would break
`strategy-vm`'s first-window cooldown arithmetic (`now − 0 ≥ horizon` needs
`now ≥ horizon`; a 24 h horizon is 8.64e13 ns — the G3 lesson, house rule
`VM_T0 = 1e17`). The harness therefore rebases every record onto one
continuous virtual timeline:

```
virt(run i, rec) = VIRT_T0 + (epoch_ns_i − epoch_ns_0) + (ts_ns − ts_ns_first_of_run_i)
```

with `VIRT_T0 = 100_000_000_000_000_000` (the existing house base). Intra-run
deltas are preserved exactly; inter-run gaps equal the wall-clock gap between
run opens; magnitudes are production-like. `Ctx::now_ns` returns the current
record's virtual ts. Wall-clock mapping for `trading_days` (§5) is
`epoch_ns_0 + (virt − VIRT_T0)`.

### 3.4 Split semantics (`--split 70/30`)

Strict parse: `N/M`, integers, `N + M == 100`, both ≥ 10 — with ONE carved
degenerate form: `0/100` (all-OOS; the §8.3 walk-forward monitor's scoring
mode — `run_backtest`'s `split` parameter is a passthrough string, so the
monitor reaches it with zero worker-code change). `0 < N < 10`, `70/40`,
garbage: usage error (nonzero exit — the worker maps it to
`BacktestError`). The split
point is the wall-time boundary at N% of the merged capture's total time span
(first record → last record, across runs). Replay is CONTINUOUS through the
boundary — books stay warm, cooldown stamps carry — but accounting is
bucketed: only orders emitted with `virt ts` in the OOS window (and their
fills/marks) count toward the `oos` section. The IS window exists to warm
state and (later, operator-side) to compare IS-vs-OOS; schema-1 reports OOS
only (§5).

### 3.5 Candidate validation — the same validator, a capture-derived universe

The harness reads the ruleset file bytes once, computes the full SHA-256
(this exact value is `ruleset_hash` in stdout), takes `hash128 =
digest[..16]`, and calls `ingress_ai::validate_ruleset(bytes, &hash128,
&universe, &mut table)` — the identical fuzz-hardened byte scanner the engine
side path runs, with all eight rule families (hash binding, grammar,
numbers, row count, names, symbol legs, caps, duplicates). No second parser
exists to drift.

The boot-universe stand-in is the **capture-observed universe**: the sorted,
deduplicated set of `sym` values present across all merged tick files. A
ruleset leg absent from capture ⇒ `RulesetReject::Symbol` ⇒ nonzero exit
("you cannot evaluate what you did not capture" — and the worker correctly
treats that as an untrustworthy-report condition, not a gate fail).

### 3.6 Evaluator drive — the real `strategy-vm`, real flip path

The harness instantiates `VmStrategy<BACKTEST_VM_SLOTS>` directly (not the
whole `StrategySet` — slots 0–4 are irrelevant to scoring a ruleset, and the
vm member is what the committed table will run in). `BACKTEST_VM_SLOTS =
4096` — the same generic code monomorphized with a larger `MultiBook`
capacity, because a multi-run capture can carry more syms than the engine's
`SET_VM_SLOTS = 512`; offline memory is free. Injection uses the REAL paths,
not a shortcut:

1. `vm.receive_table(&table)` — the inherent copy-#2 seam. (NOT the trait's
   `on_ruleset_table`: on a bare `VmStrategy` that is the defaulted no-op —
   only `StrategySet` forwards it to the slot-5 member. The harness drives
   the bare vm, so it calls the inherent method the forwarder would.)
2. A synthesized `AiCmd{kind: RulesetCommit, px/qty = hash128 LE halves}`
   through `vm.on_ai(...)` — the real hash-checked index flip, cooldown
   re-arm included.
3. Then, per merged record: `vm.on_tick(&tick, &mut ctx)` with a
   `BacktestCtx` implementing `strategy_core::Ctx` (`submit` captures orders
   into the fill engine; `now_ns` returns the virtual clock).
4. Synthesized fills are fed back via `vm.on_fill` for engine-fidelity (the
   engine's D3 pump would), even though the vm currently ignores them.

Emit-time semantics are therefore EXACTLY production: post-only at mid,
row-cap ∧ `POLICY_SINGLE_ORDER_CAP_1E6` re-clamp, one-emit-per-row-per-tick,
cooldown stamped only on accepted submit.

---

## 4. Fill / fee / latency model (H-D1: strict-cross maker; LOCKED)

Plain doctrine: **the backtest may under-promise; it must never over-promise.**

### 4.1 Order intake and lifetime

`BacktestCtx::submit` appends to a preallocated open-order table (fixed
arrays, no allocation after harness init). Risk-policy structural caps are
modeled even though `Engine::on_new_order` does not exist yet (plan §10):
max 4 open orders per symbol, 32 total; an emit beyond a cap is counted
`orders_rejected_caps` and dropped (conservative vs today's paper engine,
which has no such wall — divergence documented here). An order rests from
`t_active = t_emit + Δ_venue` until filled or replay end; unfilled remainders
are canceled at end with zero P&L effect.

### 4.2 The maker fill rule (strict-cross)

For a resting BID at price P (sym s, venue v = venue byte of s): at each
merged tick of s with `virt ts ≥ t_active`, the order fills iff
`ask_px(s) < P` — the opposite best trades STRICTLY THROUGH the level, not
merely touches it. Fill price = P (maker never improves), fill qty =
`min(remaining, ask_qty at that tick)`. Mirror rule for asks
(`bid_px(s) > P`). Partial fills rest on. No queue-position credit exists to
be optimistic about: if price only touches our level, queue-ahead is assumed
infinite. Zero RNG anywhere in the model — determinism is by construction,
not by seed (the "seeded" requirement is satisfied vacuously and stated in
the report’s reproducibility line).

### 4.3 Fees

Per-venue `(maker_bps, taker_bps)` const table, charged on fill notional.
Defaults: Polymarket 0/0 (current CLOB fee schedule; the table cites the doc
and the flag exists precisely so a fee-schedule change never needs a code
change), BN/OKX/Deribit/HL 0/0 until those venues can execute (8j).
Override: `--fee-bps <venue>:<maker>:<taker>` (repeatable). The worker never
passes it — defaults ARE the contract.

### 4.4 Latency penalty Δ

Per-venue const defaults, deliberately conservative: PM 200 ms, BN/OKX/
Deribit 100 ms, HL 600 ms (per-block push cadence ~0.5 s+, plan §13).
Override: `--latency-ns N` (global, per the plan §8.7.3 sketch) or
`--latency-ns-venue <venue>:<ns>` (repeatable). Applied at order activation
(§4.1) — the conservative reading for a maker model: our order cannot rest
(and thus cannot fill) until the venue would plausibly have booked it.

### 4.5 P&L accounting (all fixed-point i64 ×1e6; floats only at render)

Per-sym position book: signed qty, average-cost basis; realized P&L on
reducing fills (average-cost); fees subtracted at fill. Equity curve =
realized + Σ unrealized (position × current mid − basis), updated on every
fill and on every tick of a held sym; `oos.max_drawdown_usd` = max
peak-to-trough of the OOS equity curve. At replay end, open positions are
marked out at last mid and the mark-out enters `net_pnl_usd` (standard
liquidation-at-mark convention; stated in the report's reproducibility
line). `oos.trades` = OOS fill count (a partial fill is one trade);
`oos.trading_days` = count of distinct UTC days (from the §3.3 wall mapping)
containing ≥ 1 OOS trade.

### 4.6 `bounds` — observed maxima, not declared caps

The validator already enforces DECLARED caps statically (rule 7). The
`bounds` section reports what the replay actually DID, full-window (IS+OOS —
a breach anywhere is disqualifying): `max_order_notional_usd` = max over
emitted orders of px×qty; `max_symbol_notional_usd` = peak per-sym
|position|×mark; `max_total_notional_usd` = peak Σ|position|×mark. Observed ≤
declared holds by the emit-clamp construction; reporting observed keeps the
gate honest if that construction ever regresses.

### 4.7 Cross-venue simultaneity (multileg-ready, pinned)

Open orders on ALL venues are evaluated against the one merged timeline;
each order fills against its own sym's book at its own venue Δ. Two legs on
two venues emitted in the same evaluation pass fill independently and
correctly interleaved — the cross-venue simultaneous fill modeling §16
requires is a property of this design, not a future feature.

---

## 5. schema-1 stdout — the frozen contract, field for field

Exactly the fields `backtest.py::parse_harness_report` reads; **no extra
keys** (an `is` section or per-symbol breakdown would be silently dropped by
the worker's report writer and become drift bait — pinned OUT of stdout):

```json
{
  "schema_version": 1,
  "ruleset_hash": "<full sha256 hex of the ruleset file bytes>",
  "split": "70/30",
  "oos": {
    "net_pnl_usd": -3.25,
    "trades": 61,
    "trading_days": 3,
    "max_drawdown_usd": 12.5
  },
  "bounds": {
    "max_order_notional_usd": 5.0,
    "max_symbol_notional_usd": 9.75,
    "max_total_notional_usd": 9.75
  }
}
```

Contract details the worker's strict parsers pin: the two `oos` counts are
REAL integers (`_strict_int` rejects bools); `schema_version` is
equality-checked against `1` (the harness emits the integer literal `1`);
USD values are JSON numbers rendered from i64 ×1e6 by a deterministic
fixed-point formatter (no float round-trip wobble — bit-identical reruns per
the plan §11 determinism row); `ruleset_hash` must equal the worker's own
recomputation or `BacktestError`. `split` echoes the argv value verbatim.

Per-symbol breakdown / IS metrics / equity curve (plan §8.7.3's richer
report) go to the OPTIONAL `--emit-detail <path>` sidecar (JSON, versioned
separately) and a human summary on stderr — operator/session surfaces, never
parsed by the worker.

**Exit codes:** `0` = a trustworthy report was printed (a money-losing
ruleset still exits 0 — the VERDICT belongs to the worker's gates); nonzero =
no trustworthy report is possible: bad args/split, unreadable or corrupt
capture (`PmlrReadErr`), validator reject (§3.5), empty merged stream, or an
OOS window containing zero ticks. The worker maps nonzero to `BacktestError`
→ verb exit 2 ("harness output untrusted") — matching `ai-session.md` §3.

---

## 6. `data_fetcher` completion (H-D5 + H-D7; LOCKED)

All Python, all fetch-time (one-shot `fetch` verb + serve's research cycle).
The engine is never involved; nothing here is within a mile of a hot path.
`features.py` keeps its injected-`get_fn` seam — the new consumers live in a
new `fetchers.py` and are handed the client, preserving the "never imports an
HTTP client" module doctrine and the existing test pattern.

### 6.1 Venue REST consumers (the full plan-§8.2 secondary set)

| consumer | endpoint (public, keyless) | output |
|---|---|---|
| PM Gamma markets | Gamma REST, by token id / slug | names + metadata → market-map (§6.2) + feature headers |
| OKX candles | `/api/v5/market/candles` | per-sym OHLCV feature files (warm-up windows) |
| Deribit chart | `/api/v2/public/get_tradingview_chart_data` | same |
| HL candleSnapshot | `POST /info {type: candleSnapshot}` | same |

Every consumer: strict field-checked parser (the `labeling.py` strictness
precedent — malformed ⇒ logged skip, never a crash), output as feature files
beside the replay-derived ones, and **`RestBudget`-gated**: default 60
requests per venue per hour (env override `CLAUDE_WORKER_REST_BUDGET_PER_H`),
fixed-window `try_acquire`, `skipped_total` surfaced in fetch output.
`--no-rest` finally does what it says (skips all four). The 8g-era
`cli.py:295-300` "no venue URL consumers exist until 8h" deviation note is
retired by this section.

### 6.2 market-map.json — bootstrap + additive refresh

The file is ABSENT on the real box; 8h owns it (kickoff mandate). On every
fetch: derive the observed universe from the latest run's tick capture
(distinct syms), resolve names — PM ordinals via Gamma (question/slug), CEX
syms as `<venue>:<instrument>` from the instrument metadata already flowing
through discovery-shaped REST (e.g. `okx:BTC-USDT`), HIP-4 `(yes,no)` pairs
from HL outcome metadata when present (none live today). Then:

- **Bootstrap:** file missing ⇒ write `{"markets": {...}, "hip4_pairs":
  [...]}` complete.
- **Additive refresh:** file present ⇒ add missing names only. Operator
  entries are NEVER deleted or overwritten; a conflict (existing name now
  resolving to a different sym) is REPORTED in fetch output and left alone —
  the operator edit wins.
- **Atomic write:** temp file + `os.replace` in the same directory; a
  half-written map can never exist.

The reader side (`cli.load_market_map`) is untouched — same path, same shape,
same strictness; the 202 tests that pin it stay green by construction.

**SymbolId stability caveat (recorded):** ordinals are allocated at boot from
discovery order, so the map is only as stable as the boot universe. The map
regenerates from the LATEST capture; a name whose sym drifted across boots
surfaces as a §6.2 conflict report, which is exactly the visibility the
operator needs. Durable cross-boot identity is a Phase-9-class item; the 8h
loop (rulesets authored against the latest capture, backtested against the
same capture) is internally consistent without it.

---

## 7. `strategist.py` — Fable 5, serve-only (H-D4; LOCKED)

The verb surface is frozen (7 verbs); the strategist is a `serve`-internal
collaborator, exactly as plan §8.2 sketched. In semi-manual mode the
operator's session IS the strategist (`ai-session.md` §4) — unchanged, same
gates.

### 7.1 Inputs (all files, no sockets)

Feature files (replay-derived + §6.1 REST), the news NDJSON digest,
the market map, the ACTIVE ruleset's latest walk-forward report (§8.4), and
the static ruleset grammar + caps contract. The strategist never touches the
UDS — its output is a FILE; only the frozen stage/commit path sends frames.

### 7.2 Prompt architecture + Anthropic prompt caching

Two blocks. STATIC system block: the §4.1 ruleset grammar, validator rules
(row/sym/table caps: ≤ $100 / ≤ $250 / ≤ $1 000, tighten-only), the output
contract, worked examples — marked `cache_control: {type: "ephemeral"}` so
every call after the first in a cache window pays ~10% for the bulk of the
prompt. DYNAMIC user block: a token-capped digest of features/news/
performance (cap constant `STRATEGIST_INPUT_CAP` chars, `TEXT_CAP`
precedent). `MODEL_STRATEGIST = "claude-fable-5"` gets its first consumer;
`STRATEGIST_MAX_TOKENS = 4096` (the `llm.py:22` comment — "the strategist
sets its own" — comes due). `llm.complete` grows an optional
system/cache/usage-return surface; existing triage/label callers unchanged.

### 7.3 Output contract (strict, like everything else)

One JSON object: `{"thesis": str, "rows": [...]}` — rows in EXACTLY the §4.1
artifact grammar. Strict parse (`labeling.py` discipline): malformed ⇒
archived to the candidates dir with a `.rejected` marker + state.db event,
cycle over. Valid ⇒ written to `~/multivenue/worker/candidates/
<utc-ts>-<hash128>.json` and handed to the REAL backtest verb path
(`backtest.run_backtest` — the frozen function, real binary).

### 7.4 Cycle state machine (one research cycle, every 6 h)

```
fetch → strategist call #1 → validate/parse → backtest
  → gates PASS → promote (§8.1) → arm monitor (§8.4)
  → gates FAIL → strategist call #2 (revision; gate summary + report appended)
      → backtest → PASS ⇒ promote / FAIL ⇒ archive with report, cycle over
```

≤ 2 Fable-5 calls per cycle, hard daily ceiling 12 calls. A cycle with no
fresh capture since the last one is SKIPPED entirely (no call, event logged).
Content-hash dedupe via the existing SQLite `prompt_cache` (identical inputs
⇒ replayed response, zero API cost).

### 7.5 Budget ledger + guards

Every call writes a state.db `events` row (`kind='strategist_call'`, detail =
model, input/output tokens from `message.usage`, cache-read flag). The daily
counter is a query over that ledger. Breach of the ceiling ⇒ skip + 
`kind='strategist_budget_skip'` event. New env keys (`.env.example` only —
`.env` itself is never touched by Claude):
`CLAUDE_WORKER_STRATEGIST_INTERVAL_S` (default 21600),
`CLAUDE_WORKER_STRATEGIST_DAILY_CAP` (default 12),
`CLAUDE_WORKER_REST_BUDGET_PER_H` (default 60). Cost control is a design
section, not an afterthought (plan §13 Anthropic-budget risk row).

### 7.6 Threading — the slow-call seam

A Fable-5 call is tens of seconds; `daemon.py`'s loop owes 5 s heartbeats on
`TICK_S = 0.2`. The LLM call runs on a single background worker thread
(`concurrent.futures.ThreadPoolExecutor(max_workers=1)`); the serve loop
polls the future each tick. FRAMES REMAIN SINGLE-WRITER: the background
thread computes and writes files only; every UDS send (heartbeat, stage,
commit, disable) happens on the serve-loop thread. SQLite from the
background thread uses its own connection, prompt_cache table only. The
backtest subprocess is fast relative to cadence and runs inline (it is
read-only and touches no socket).

---

## 8. Promotion + rollback (H-D2 + H-D3; LOCKED)

### 8.1 Auto-promotion — closing the manual-install gap

G7's smoke installed the artifact by hand; nothing in `src/` writes into
`$AI_RULESET_DIR`. serve's promote step, on gates PASS: copy the candidate
to `$AI_RULESET_DIR/<hash128-hex>.json` (atomic, §6.2 style), then call the
FROZEN pair — `backtest.stage_ruleset(state, client, ruleset, report,
author_mode="auto")` → `backtest.commit_ruleset(state, client, full_hash)`.
Gates bind in code inside `stage_ruleset` exactly as today (`backtest.py:307`
already names this caller: "serve's commander path (8h strategist) calls this
same function"). No new frame path, no override, paper only (live emission
is 8i/8j/8k-gated at the deployment layer, not here).

### 8.2 Attribution

`state.stage_ruleset` gains OPTIONAL `model=`/`thesis=` params (default
`None` — every existing call site and all 202 tests unchanged) writing the
pre-provisioned `rulesets.model`/`rulesets.thesis` columns. The registry
finally answers "who wrote the live table and why".

### 8.3 Rollback trigger — precise definition (H-D3)

Fact forcing the design: paper mode produces ZERO fills
(`PaperDispatcher::try_next_fill() -> None`, all four fill-ring producers
dropped at boot), so a "live paper realized P&L" trigger would never fire
without engine-side work — which would be a hot-path change (D3 pump, alloc
gates) and 8j scope-creep. LOCKED instead: **walk-forward re-backtest**, the
honest paper-performance proxy (identical evaluator, identical fill model,
freshest data the engine actually saw).

| element | definition |
|---|---|
| metric | ACTIVE ruleset's `net_pnl_usd` and `max_drawdown_usd` from a real-harness run over the trailing window, invoked with `--split 0/100` (the §3.4 carved all-OOS form: the whole trailing window IS the OOS bucket; plain passthrough via `run_backtest(split="0/100")`, no worker change) |
| window | trailing 24 h of capture (target), floor 6 h — below the floor the monitor SKIPS (insufficient-data event), it does not guess |
| threshold | `net_pnl_usd ≤ −$100` (½ the risk-policy $200/day realized-loss kill line — the AI lane rolls back before the engine-level kill would) OR `max_drawdown_usd ≥ $200` |
| action | (1) `push --kind disable --strategy 5` equivalent through the commander path; (2) restage + commit the PRIOR gates-passed committed hash from the registry (artifact still installed by construction); if no prior exists — disable only. Both frames ride the normal UDS path ⇒ PMLR ai-cmds trail ⇒ `audit-replay` renders the whole demotion |
| cadence | every research cycle (§7.4) + once shortly after every promotion (arm check) |

D3a semantics respected: disable flips the mask bit, the table persists —
restaging the prior hash then commit-flips back to known-good rows.

### 8.4 Events

state.db `events` rows: `promotion`, `rollback_triggered` (with metric
values), `rollback_no_prior`, `monitor_skip_insufficient_data`. The engine
side needs NOTHING new: mask flip + staged/committed counters + ai-cmds
capture already observe every action (§10).

### 8.5 The forced-underperformance demo (exit criterion)

The trigger definition above stays production-true; the demo forces the
INPUT, never bypasses a gate: author a sacrificial ruleset that legitimately
passes gates on capture window A (regime-fit `level_breach` rows), promote it
through the REAL auto path, then run the monitor with its trailing window
pointed at capture window B where the regime inverts (operator-selected
run dirs; worst case, a crafted synthetic capture written with `PmlrWriter` —
the golden-fixture machinery doubles as the demo generator). Monitor computes
net ≤ −$100 ⇒ disable-5 + restage-prior observed live, `audit-replay` shows
the Disable/Stage/Commit sequence, state.db shows `rollback_triggered`.

---

## 9. serve composition (daemon.py)

The 0.2 s cooperative loop and its collaborators are untouched; one new
collaborator joins: `research_cycle` (owns §7.4 + §8), due every
`CLAUDE_WORKER_STRATEGIST_INTERVAL_S`, checked once per tick like the
watcher. Heartbeats, news_watcher, label emission: unchanged. The SDK client
construction site stays `daemon.py` and nowhere else; `strategist.py`
receives `complete_fn` injected (same seam as feeds/labeling — FakeClient
testing works identically). SIGTERM drains: an in-flight background LLM call
is abandoned (thread daemonized; its file-write is atomic-or-absent), an
in-flight promote finishes its current frame before close.

---

## 10. Observability

- **Harness:** schema-1 stdout (machine), human summary on stderr (fills,
  per-venue tick counts, window boundaries, universe size), optional
  `--emit-detail` sidecar (§5). Exit codes per §5.
- **Worker:** state.db events are the ledger (§7.5, §8.4); fetch output
  lists REST budget consumption + market-map conflicts (§6).
- **Engine:** ZERO new metrics. The existing §9 family already renders every
  8h-visible effect: `engine_ai_ruleset_{staged,committed,rejected}_total`,
  `engine_vm_{rows_active,table_epoch,fires,orders_*,commit_dropped}`,
  `engine_strategy_enabled_mask` (49→17 on disable-5 — the compose-if-
  configured mask-49 boot fact from G7 is the runbook baseline).
- **audit-replay:** unchanged; the ai section renders promotion and rollback
  frames by construction. Slot-6 refusal-probe doctrine holds for any live
  session probing (slot 6 refuses, slot 5 is the vm).
- No TUI changes (metrics suffice; 8g precedent).

---

## 11. Hot-path impact statement + alloc-gate plan

**Expected hot-path delta: ZERO.** `backtest` is an offline CLI path under
the `audit_replay.rs` doctrine ("this module ALLOCATES … never loaded by the
engine loop"); everything else in 8h is Python. No engine, strategy-vm,
ingress, ring, or dispatcher line changes. Baseline stays **36 gates, 0 B/op,
`--test-threads=1`** — no appends planned; if implementation surfaces a
genuinely hot seam (none is foreseen), the append must be justified in the
progress log against this section. Every H-session still runs the full gate
set (small changes have regressed gates before — pitfall #9), with the
false-green guard (`cargo clean -p bench` on warm-looking runs) and the
stale-rmeta playbook in force. Cargo on the Mac only (pitfall #10).

---

## 12. Test plan (PLAN §21.3/§21.4; plan §11 harness row; H-D8 LOCKED)

**No new untrusted-bytes parser exists** — pinned by construction: capture is
read via the hardened `core-io` readers (`PmlrReader` header/size validation,
existing since 8e), the candidate ruleset via `ingress_ai::validate_ruleset`
(fuzz target `ruleset_json`, 72.3M runs clean, UNTOUCHED). argv/`--split`
parsing is clap + strict integer checks — unit-tested, not a wire surface.
Therefore no new fuzz target is REQUIRED; the fuzz suite runs untouched.
If implementation ever hand-rolls a byte scanner instead (it must not), that
scanner arrives with proptest + fuzz per §21.3/§21.4 — non-negotiable.

**Rust (crates/cli/tests/ + in-module):**

- Golden replay fixture: synthetic multi-venue, multi-run capture written
  with `PmlrWriter` in-test; hand-computed P&L/DD/bounds asserted EXACTLY
  (plan §11: "golden replay fixture with known P&L").
- Determinism: two invocations over the same fixture ⇒ byte-identical stdout
  (plan §11: "same log ⇒ bit-identical report").
- Proptest: merge order is total, stable, and sorted for arbitrary per-venue
  streams; fill-model invariants (fill px never better than the crossing
  book; qty ≤ displayed; fees ≥ 0; cash+position conservation identity).
- Unit: fee table + overrides, latency activation, split parse (accepts
  `70/30` and the carved `0/100` monitor form; rejects `5/95`, `70/40`,
  garbage), split-boundary bucketing, VIRT_T0 rebase
  (cross-run monotonicity), multi-run stitching order, open-order caps,
  end-of-window liquidation, empty-OOS ⇒ nonzero exit, capture-universe
  derivation, validator-reject ⇒ nonzero exit.
- Happy-path + failure-mode per public fn (house rule).

**Python (additive; the 202 and their fake-harness shim UNTOUCHED):**

- `strategist`: prompt build (static/dynamic split, cache_control present),
  strict output parse (good/malformed/oversized), revision-call cap, daily
  ceiling, dedupe hit, budget ledger rows, background-thread seam
  (FakeClient; no live API — house rule).
- `fetchers`: per-venue consumers against injected `get_fn` fixtures,
  RestBudget acquire/skip, `--no-rest` real, malformed-response skip.
- market-map: bootstrap, additive refresh, conflict report, operator-entry
  preservation, atomic write (tmp+rename observed).
- promotion: auto-install + stage/commit against `FakeUdsServer`; attribution
  columns written; gates-fail ⇒ no install, no frames.
- monitor: threshold arithmetic (both arms), window floor skip, action
  ordering (disable BEFORE restage), no-prior fallback, event rows.
- **Real-harness integration** (the H-D8 centerpiece):
  `tests/test_backtest_real.py`, `skipif` the release binary is absent —
  drives the REAL `multivenue-engine backtest` over a committed golden
  fixture through `run_backtest`, proving the frozen argv + schema-1 contract
  against the real implementation. Runs on the Mac; auto-skips anywhere the
  binary is missing, so pytest is green everywhere.

**Live doctrine (pitfall #11):** probe fixtures are not proof. The harness
gets a live-capture smoke (real run dir from a real boot) in an H session
before being declared done; the H6 demo (§8.5) is the phase-level proof.

---

## 13. Ordered implementation checklist (H1…H6; each lands only with all
gates green on the Mac: workspace nextest, release alloc 36/36
`--test-threads=1`, worker pytest additive-green, fuzz untouched)

1. **H1 — harness substrate:** `Cmd::Backtest` + args; capture discovery
   (run dir + root); PMLR open + k-way merge + VIRT_T0 rebase;
   capture-universe derivation; `validate_ruleset` reuse; `VmStrategy`
   drive via real receive/commit paths; `BacktestCtx`; hold-only accounting
   stub; schema-1 stdout EXACT (zeros where the model is not yet in);
   golden-fixture skeleton + determinism test + merge proptest.
2. **H2 — the model:** §4 in full (open-order table, strict-cross fills,
   fees, Δ, fixed-point equity/DD/bounds, liquidation, split bucketing);
   golden P&L asserted exactly; fill proptests; `--emit-detail`;
   `test_backtest_real.py`. **The shim seam is retired here** (G7 §5.1
   pattern remains only inside the frozen 202 as the mock).
3. **H3 — data_fetcher:** §6 in full (four REST consumers, RestBudget
   wiring, `--no-rest` real, market-map bootstrap/refresh/conflicts) +
   tests; `.env.example` gains the §7.5 keys.
4. **H4 — strategist:** §7 in full (module, prompt cache, budget ledger,
   candidates dir, thread seam, serve research-cycle) + §8.1/§8.2 promotion
   + tests.
5. **H5 — rollback:** §8.3/§8.4 monitor + trigger + restage-prior + events +
   tests.
6. **H6 — close:** final gates; operator-gated LIVE demo: real capture, one
   real Fable-5 serve cycle (budget-capped), auto-promotion observed on a
   live paper boot, §8.5 forced-underperformance rollback demonstrated,
   `audit-replay` verification; progress-log closing entry.

Session slicing follows the 8g precedent (one checklist item per session;
an item may split if it runs long — the progress log records the seam).

---

## 14. Design decisions — RESOLVED by operator 2026-08-16 (H0)

| # | decision | options (recommended first) | status |
|---|---|---|---|
| H-D1 | Backtest fill/fee/latency model | (a) strict-cross maker + per-venue Δ + fee table, zero RNG / (b) touch-fill maker / (c) taker conversion | **LOCKED (a)** — §4; determinism by construction |
| H-D2 | Promotion policy | (a) full auto in serve (install → stage → commit), paper only / (b) auto-stage + operator commit / (c) auto with veto window | **LOCKED (a)** — §8.1; matches plan §8.7.5 + §12 exit criterion literally |
| H-D3 | Rollback trigger | (a) walk-forward re-backtest: trailing 24 h (floor 6 h), net ≤ −$100 OR DD ≥ $200 ⇒ disable-5 + restage-prior / (b) PaperDispatcher fill model now / (c) fires-divergence | **LOCKED (a)** — §8.3; zero hot-path delta; paper fills noted for 8i |
| H-D4 | Strategist cadence + budget | (a) 6 h cycle, ≤2 calls/cycle, 12/day, prompt caching, bg-thread, ledger / (b) event-driven + min spacing / (c) semi-manual only | **LOCKED (a)** — §7 |
| H-D5 | data_fetcher REST scope | (a) full §8.2 set: PM Gamma + OKX candles + Deribit chart + HL candleSnapshot, ≤60 req/venue/h / (b) Gamma only / (c) PM+Binance | **LOCKED (a)** — §6.1 |
| H-D6 | Multileg v2 timing | (a) design-only in 8h; harness multi-venue day one regardless / (b) first slice (v2 PODs + grammar flag) | **LOCKED (a)** — §16 |
| H-D7 | market-map ownership | (a) fetcher bootstraps + additive refresh, atomic, operator wins / (b) bootstrap-if-absent only / (c) generated+overlay files | **LOCKED (a)** — §6.2 |
| H-D8 | Real-harness test extent | (a) both sides, additive; 202 untouched / (b) Rust only / (c) full e2e in pytest | **LOCKED (a)** — §12 |

Pinned unless the operator objects (folded from §3–§10): schema-1 stdout
carries EXACTLY the contract fields (extras go to `--emit-detail`); exit-code
mapping (§5); universe = capture-observed syms; `trades` = fills;
`trading_days` = distinct UTC days with ≥1 OOS trade; open-order caps 4/32
modeled; Δ defaults PM 200 ms / BN·OKX·Deribit 100 ms / HL 600 ms; fee
defaults all 0 with the PM-schedule pointer; `VIRT_T0 = 1e17`;
`BACKTEST_VM_SLOTS = 4096`; candidates dir `~/multivenue/worker/candidates/`;
`state.stage_ruleset` extended additively; monitor window 24 h/6 h floor; no
`default_run_fn` code change (PATH resolution stays; absolute-path option is
documented in `.env.example` commentary only).

---

## 15. Comment-tidy triage (G0-style; docs-only, ride natural commits)

1. `docs/arch/phase-8g-design.md` §4.1: "`Order` carries no venue field" is
   factually stale — `Order.venue` exists at offset 40 (`core-types`,
   wire-format row). The MECHANISM described (venue-agnostic `ctx.submit`,
   venue derived from the namespaced sym at emit) is correct; one clause
   needs rewording. Docs-only fix, ride any H commit.
2. `core-types`: `RuleRow` has size/offset asserts but no align-64 unit
   assert (Tick/Signal/Fill/Order have them, `lib.rs:1448-1469`). One-line
   test append WHEN core-types is next touched — 8h does not touch it.
3. `backtest.py:39` comment ("absolute in prod .env wiring arrives with the
   8h harness"): resolved WITHOUT code motion — PATH resolution stays the
   contract (§14 pinned); `docs/local-setup.md` gains the "release binary on
   PATH" runbook line in H2.

---

## 16. MULTILEG READINESS (mandatory audit + v2 specification; NO v2 code in 8h — H-D6)

### 16.1 What exists TODAY (audited at HEAD 39e6542)

- **Venue-explicit legs (D2 as amended):** both `RuleRow` legs are
  namespaced SymbolIds — venue byte in bits 31..24 — and BOTH may be any
  asset on any boot-universe venue (`ruleset.rs:579-594` rule 6; test
  `ruleset.rs:1697` "Cross-venue legs are legal"). `ctx.submit` is
  venue-agnostic (`strategy-core/src/lib.rs:63-72`); `Order.venue` is
  DERIVED from the action sym at emit (`strategy-vm/src/lib.rs:427-446`).
  The plumbing for "leg on venue V" is done and live-proven.
- **Cross-venue TRIGGERING is done:** `cross_deviation` with a PM action leg
  and a BN reference leg is precisely a 2-venue rule; G7 ran the boot-default
  pairing (sym 42 vs ref 7).
- **`MarketGroup` precedent:** `strategy-cross-arb`'s
  `MarketGroup<M>{members: [SymbolId; M], count}` + boot-only
  `register_group` (dup/full/reserved checks) is the in-repo pattern for
  "N symbols form one economic unit" — the natural shape for a v2 leg set.
- **The hard wall — ONE action leg:** `RuleRow` is 64 B fully consumed (43
  declared + 21 explicit pad); `sym` is the ONLY order-emitting leg
  (`ref_sym` never trades). The single-action-leg assumption is baked into
  five places (the door-closer audit): the emit path (`strategy-vm`
  `:437-446`), the one-emit-per-row-per-tick invariant that makes the cap
  composition argument sound (`:56-66` + caps proptest), the `sym`-filter
  row scan (`:321-324`), rule-8 duplicate identity `(sym, trigger, side,
  ref/level)` (`ruleset.rs:616-631`), and rule-7's per-sym Σ walk keyed on
  `rows[j].sym` (`:601-608`). `MultiBook` sizing arithmetic assumes 2 legs:
  `SET_VM_SLOTS = 512 = 256 rows × 2` (`strategy-set/src/lib.rs:137-140`).

Verdict: **the venue dimension is solved; the leg-count dimension is not.**
Multileg across all venues is an evolution of the row/validator/emit layer
only — no ring, engine-loop, capture, or worker-transport change is implied.

### 16.2 The v2 shape (specified now, built later)

**Layout — leg-table indirection over fat rows.** Two candidates were
weighed: (a) `RuleRow` stays 64 B and spends 4 of its 21 pad bytes on
`{leg_table_idx: u16, leg_count: u8, leg_flags: u8}`, pointing into a new
per-table leg region; (b) a 128 B `RuleRowV2` with inline legs.
**(a) is the specified shape:** the hot row scan keeps its one-line-per-row
cache behavior (legs are touched only on FIRE — cold by definition), v1 rows
lift losslessly (leg_count 0 ⇒ implicit single leg = `sym`, bit-compatible),
and the budget stays explicit:

```
RuleRow v2 pad spend (bytes 43..47, repr(C)-clean — no implicit pad):
  leg_count u8 @43 · leg_table_idx u16 @44 · leg_flags u8 @46 · _pad [u8;17] @47
ActionLeg (32 B, #[repr(C)], Copy): sym u32 · side u8 · _pad0 [u8;3] ·
  ratio_1e6 i64 (signed size ratio vs leg 0) · max_risk_1e6 i64 · _pad1 [u8;8]
LegTable: [ActionLeg; 512] = 16 KiB — a SHARED budget (avg 2 legs × 256 rows;
  Σ leg_count over all rows ≤ 512 is validator rule 9 below — an
  all-256-rows-×-4-legs table is deliberately NOT representable)
RuleTableV2 = rows (16 KiB) + legs (16 KiB) + meta line ≈ 32 832 B
Ring<RuleTableSlotV2, 2> ≈ 65 664 B — trivially affordable at operator cadence
```

**Grammar v2:** a row MAY carry `"legs": [{sym, side, ratio, max_risk_usd},
…]` (1–4 entries); `sym`/`side` at row level remain and define leg 0 (v1
compatibility); the artifact gains no version field — the validator infers
v2 by key presence, and rule 2's unknown-key strictness means v1 engines
REJECT v2 artifacts cleanly (fail-closed forward compatibility).

**Validator deltas (extending, not replacing, the eight families):** per-leg
universe membership (rule 6 per leg); leg_count ∈ [1,4]; rule 9: Σ
leg_count ≤ 512 (leg-table capacity, shared budget); ratio domain
(nonzero, |ratio| ≤ 1e6×cap); per-leg cap ≤ single-order cap; Σ legs of a row
≤ the row's `max_risk_1e6`; the rule-7 per-sym Σ walks EVERY leg of every row
(a sym's exposure = Σ over all legs naming it, any row) ≤ $250; table Σ ≤
$1 000; rule-8 identity extends to the leg-set (order-independent FNV over
legs). Cap composition proof obligation: one evaluation pass emits ≤ Σ
per-leg caps per row ≤ row cap; the caps proptest gains legged cases.

**Atomicity + partial-fill policy (paper semantics NOW, 8i/8j make real):**
legs of a fired row are emitted in one evaluation pass, same `ts`, fixed leg
order; there is NO cross-venue atomicity (does not exist in the world).
Policy knobs specified: on `SubmitErr::RingFull` at leg k — remaining legs
are NOT emitted, the cooldown is NOT stamped, `vm_leg_partial_emit_total`
counts (paper-visible residual-exposure signal); the unwind-vs-carry decision
for a leg-1-filled/leg-2-unfilled LIVE position is explicitly 8i RiskGate
domain (it needs fill feedback) — v2 paper semantics carry the residual and
surface it, nothing more.

### 16.3 What 8h must NOT do (door-closers, PINNED)

1. The harness replays MULTI-VENUE merged capture with per-venue independent
   fill clocks from day one (§3.2/§4.7) — a single-venue replay would make
   multileg backtests structurally impossible later. **This is build-order
   law for H1, not an option.**
2. The harness fill engine keys everything by namespaced SymbolId (venue
   byte inside) — no "the PM order" fields, no per-venue special cases in
   accounting; per-sym/total bounds compose across venues unchanged.
3. schema-1 stays leg-agnostic (aggregate USD) — v2 needs NO stdout schema
   change; leg detail extends the `--emit-detail` sidecar only.
4. `RuleRow._pad` stays untouched in 8h — bytes 43..47 are RESERVED for the
   v2 `{leg_table_idx, leg_count, leg_flags}` spend.
5. market-map resolution stays name → namespaced SymbolId with the venue
   inside the id — no PM-only assumptions (§6.2 already writes
   `<venue>:<instrument>` names).
6. `BacktestCtx` accepts orders for ANY venue byte (it already must — vm
   emission is venue-derived), so a v2 evaluator drops into the same harness
   with zero fill-engine changes.

### 16.4 Timing

**LOCKED (H-D6): design-only in 8h.** v2 implementation is a Phase-9
candidate gated behind 8i (RiskGate legged netting) + 8j (real venue fills) —
the two phases that give partial-fill policy teeth. This section is the v2
design seed; nothing in the 8h checklist builds it, and §16.3 guarantees 8h
closes no doors.

---

## 17. Non-goals, restated once more

`crates/risk` state machine — **8i**. Venue execution dispatchers + live
fill producers (and the `PaperDispatcher` synthetic-fill model) — **8j**
(fills note: 8i per §8.3). Live ramp — **8k**. Paid APIs (X, Benzinga,
Blocknative) — **Phase-6 P&L gate**. Options support — **proposed, not
scheduled**: `docs/options-support-plan.md` (Phase 9+ candidate, P&L-gated
exactly like paid APIs; HL/PM have no options instrument class — HIP-4
binaries are digital-option-LIKE payoffs, not options). Multileg v2 code —
**post-8j** (§16.4). New trigger families, TUI panes, new worker verbs,
`dlopen` anything, cloud anything — no.

---

*Design complete; §14 locked in-session (H0). Implementation starts only on
explicit operator go per house convention — the H1 kickoff prompt lives in
`docs/phase-8h-progress.md`.*


