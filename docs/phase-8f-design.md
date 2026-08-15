# Phase 8f — AI-Ingress: Design (Phase 0, no code)

Status: DRAFT for operator review. No implementation before explicit go.
Branch: `stage2/8f-ai-ingress` (worktree), branch point `b931c59` (8e commit).
Authority: the 2026-08-15 kickoff directive (rewrite + dual-mode amendments,
plan §8.2.1 / §8.7-attribution / §9 ANTHROPIC_API_KEY note / §12 "both modes
proven") supersedes the committed plan where they differ. Python is **3.14**
everywhere the committed docs say 3.12.

---

## 1. Scope and non-goals

**In scope (8f):** `AiCmd` POD + PMLR `slot_kind = 4`; `ingress-ai` crate
(UDS → HMAC → SPSC ring); engine AI lane + `Strategy::on_ai`; `StrategySet`
runtime enable/disable; `strategy-ai-exec`; **complete rewrite** of
`claude-worker` (daemon-first, dual-mode, Python 3.14); operator verb CLI;
`docs/prompts/ai-session.md`; RSS removal (sequenced LAST); docs sweep.

**Out of scope (deferred):** `strategy-vm` ruleset *evaluation* (8g — but the
Stage/Commit command plumbing and gate binding ship now), `cli backtest`
harness (8h — the worker's backtest verb ships now against a subprocess seam,
proven with canned reports per plan §11), `crates/risk` (8i; OrderIntent is
paper-only and clamped when 8i lands), live-venue demo (only on operator go,
after the soak ends, `pgrep` clean).

**Exit criteria (§12 + amendment):** AI cmd → strategy toggle observed live
(operator-gated), RSS fully deleted, heartbeat/staleness proven, **both modes
proven** — full-auto serve loop and scripted semi-manual verb session, each
against the fake UDS server in tests.

---

## 2. Component breakdown

### Rust (all in the existing workspace)

| Component | Crate | Work |
|---|---|---|
| `AiCmd` POD + kinds + consts | `core-types` | new 64-B `#[repr(C)]` Copy struct, `AiCmdKind`, `AI_RING_SIZE = 1024`, static size/offset asserts |
| PMLR kind 4 | `core-io` | `SlotKind::AiCmd = 4` decode path (today: reserved, refuses), wire-format.md table row |
| HMAC tag helper | `core-crypto` | reuse `hmac_sha256`; add truncated-16-B tag + constant-time compare if not present |
| UDS listener + frame parser + ring producer | **`ingress-ai`** (new) | mio-driven, single client, peer-cred check, in-place parse, HMAC verify, seq policy, PMLR capture, metrics; also hosts the ruleset stage/commit side-path (validation stub in 8f, table fill in 8g) |
| AI lane + dispatch | `engine` | `ai_cons: Consumer<AiCmd, AI_RING_SIZE>` drained in `Engine::tick()` with budget; TTL-expired-on-pop dropped + counter; `Strategy::on_ai` defaulted method (monomorphized) |
| Runtime composition | `strategy-set` (new crate or module in `strategy-core` — see §13 Q3) | bitmask fan-out over statically composed members; Enable/Disable semantics |
| AI-driven strategy | **`strategy-ai-exec`** (new) | fair-value table (fixed, TTL), deviation quoting, OrderIntent honor, staleness fail-safe |
| Run-path wiring | `cli` | spawn `ingress-ai` (core 4 after RSS removal), `--strategy` → initial mask, env keys |
| Fills capture | `cli` run path | engine-thread `PmlrCapture` → `engine-fills.pmlr` (`SlotKind::Fill = 2`): paper fills now, venue fills ride the same path post-8j — the positions/P&L feed for the research loop (§5.1); design answer to "how does the AI see our open positions" — via replay, not a new IPC channel |
| Metrics | `core-metrics` consumers | `engine_ingress_ai_*` counters/gauges (names below; exposition carries `_total`) |

### Python — `claude-worker/` rewritten from scratch (§8.2 + dual-mode)

One package, **library core + two frontends**. Old code is read-only
reference; nothing ports wholesale (§9 of this doc lists what carries over).

```
claude-worker/
  pyproject.toml            # >=3.14, hatchling, ruff/mypy/pytest carried config style
  .python-version           # 3.14
  src/claude_worker/
    __init__.py
    config.py               # BaseConfig (all modes) + ServeConfig (adds ANTHROPIC_API_KEY)
    llm.py                  # THE SDK seam: make_client()/complete(); constructed by serve ONLY
    frames.py               # AiCmd pack/unpack, kinds, HMAC tag, seq alloc (via state.py)
    uds.py                  # UDS client: connect, heartbeat, send_frame; single-writer
    state.py                # SQLite: dedupe, seq, prompt cache, ruleset registry, event log
    pmlr.py                 # read-only PMLR v1/v2 reader (mmap + struct.unpack_from)
    features.py             # data_fetcher: replay logs (+ rate-budgeted venue REST) → feature files
    feeds.py                # news_watcher: httpx RSS/Atom poll, dedupe, triage→escalate (LLM injected)
    labeling.py             # Sonnet labeling prompt + strict parse (schema carried from old rule_parser style)
    strategist.py           # Fable 5 proposal generation (serve only)
    backtest.py             # subprocess seam → `multivenue-engine backtest`; report parse; GATES
    commander.py            # policy → AiCmd emission; heartbeat cadence
    daemon.py               # `serve` composition loop; signals; cadences
    cli.py                  # typer app: serve + operator verbs (thin frontends only)
  tests/                    # all fresh; SDK mocked; fake UDS server; scripted session test
docs/prompts/ai-session.md  # 8f deliverable, part of the test surface
```

**Dual-mode invariant (in code, not prose):** verbs and `serve` call the
*same* library functions. `stage-ruleset` gate logic lives in
`backtest.py`/`state.py` and is the only path to a Stage frame — no override
flag exists anywhere in the CLI surface (asserted by a test that parses
`--help` output). `llm.py` clients are constructed inside `daemon.py` only;
verbs never import-and-construct (asserted by test: invoking any verb with
`ANTHROPIC_API_KEY` unset succeeds).

---

## 3. `AiCmd` — 64-byte POD (`core-types`)

Layout exactly per plan §8.4; `#[repr(C)]`, `#[derive(Copy, Clone)]`, all
padding explicit and zeroed (AsBytes contract), one cache line:

```
offset  field        type      notes
0..8    ts_ns        u64       worker send time (worker clock; engine never trusts it for TTL base — see below)
8..12   seq          u32       strictly increasing per session; gap ⇒ counter, never fatal
12..16  sym          u32       venue-namespaced SymbolId or SYMBOL_ID_NONE
16..24  px           i64       fixed-point 1e6 (fair value / intent px / param value / hash lo)
24..32  qty          i64       fixed-point 1e6 (qty / hash hi)
32..40  ttl_ns       u64       expiry relative to *engine receive time*; 0 = no expiry
40      kind         u8        AiCmdKind (below)
41      venue        u8        VenueId (Ai = 5 for engine-directed cmds; target venue for intents)
42      strategy_id  u8        StrategySet slot index
43      side         u8        Side or 0xFF
44..46  param_id     u16       SetParam selector
46..48  flags        u16       bit0: expire_on_silence
48..64  _pad         [u8; 16]  explicit, zeroed
```

`static_assert_size!(AiCmd, 64)` + per-field offset asserts (same pattern as
`Tick`). TTL decision (design deviation, documented): plan says "expiry
relative to ts_ns"; worker and engine clocks are different domains
(`core-time::now_ns` is monotonic, worker sends wall-ish ns). Trusting
`ts_ns` would make TTL depend on cross-process clock agreement. **Rule:
engine stamps `recv_ns` at pop; expiry = `recv_ns_at_pop > push_recv_ns +
ttl_ns` is not storable in the slot, so the engine checks TTL at the drain
site against ring-residency: `now_ns - ingress_recv_ns > ttl_ns ⇒ drop +
`engine_ingress_ai_expired_total``. Implementation: `ingress-ai` rewrites
`ts_ns` to its own `now_ns` at accept time (after HMAC verify, before ring
push) — the original worker send time is preserved in the PMLR capture record
only. This keeps the slot self-contained and clock-coherent.* If the operator
prefers the plan's literal reading (trust worker ts), flip one line; flagged
as review question Q1.

**Kinds:** `Heartbeat=0, EnableStrategy=1, DisableStrategy=2, SetFairValue=3,
SetBias=4, SetParam=5, OrderIntent=6, RulesetStage=7, RulesetCommit=8,
HaltRequest=9`. **No Resume exists** — sticky halt requires manual restart
(risk-policy); the command deliberately cannot be expressed.

Per-kind field semantics (unused fields MUST be zero / `SYMBOL_ID_NONE` /
`0xFF`; engine rejects violations at the drain site — `malformed_total`):

| kind | sym | px | qty | ttl_ns | strategy_id | side | param_id |
|---|---|---|---|---|---|---|---|
| Heartbeat | NONE | 0 | 0 | 0 | 0xFF→n/a | 0xFF | 0 |
| Enable/DisableStrategy | NONE | 0 | 0 | 0 | slot idx | 0xFF | 0 |
| SetFairValue | sym | fair 1e6 | 0 | required >0 | 0xFF | 0xFF | 0 |
| SetBias | sym | bias 1e6 (signed) | 0 | required >0 | 0xFF | 0xFF | 0 |
| SetParam | NONE or sym | value 1e6 | 0 | 0 | slot idx | 0xFF | param |
| OrderIntent | sym | px 1e6 | qty 1e6 | required >0 | 4 (ai-exec) | Bid/Ask | 0 |
| RulesetStage/Commit | NONE | hash[0..8] LE | hash[8..16] LE | 0 | 5 (vm) | 0xFF | 0 |
| HaltRequest | NONE | 0 | 0 | 0 | 0xFF | 0xFF | 0 |

Ruleset identity: `hash128` = first 16 bytes of SHA-256 over the canonical
ruleset file bytes, carried in `px`+`qty`. The full 32-B hash lives in the
report/registry; 128 bits is collision-safe for an operator-scale registry
and keeps Stage/Commit single-frame atomic.

---

## 4. Transport: UDS at the process boundary, SPSC ring inside (§8.3)

### 4.1 Wire frame

```
[len: u16 LE = 80] [AiCmd: 64 B] [tag: 16 B]
total frame = 82 B; tag = HMAC-SHA256(AI_INGRESS_HMAC_KEY, cmd_bytes[0..64])[0..16]
```

Key: `AI_INGRESS_HMAC_KEY` in `.env`, 64 hex chars → 32 B (loaded once,
never logged). Tag comparison is constant-time (`core-crypto`). `len != 80`
⇒ protocol error ⇒ connection dropped (fail-fast; client reconnects).
Multiple commands = back-to-back frames on the stream.

### 4.2 Socket lifecycle & authn

- Path `AI_INGRESS_SOCK` (default `~/multivenue/run/ai.sock`); parent dir
  0700, socket 0600, stale socket unlinked at bind.
- **Single client.** Second connect is accepted-then-closed immediately with
  a counter increment (`rejected_conns_total`). This is also the dual-mode
  interlock: while `serve` holds the socket, operator verbs fail with exit 4
  ("daemon holds the socket") — modes cannot interleave by construction.
- Peer credentials verified at accept: `LOCAL_PEERCRED` (macOS `xucred`) /
  `SO_PEERCRED` (Linux `ucred`); peer euid must equal process euid.
- HMAC per frame on top of peer-cred (defense in depth + audit integrity).

### 4.3 Ownership model (single-writer, bounded, cache-aligned)

```
[claude-worker process]                      [engine process]
 uds.py — sole frame writer                   ingress-ai thread (core 4)
        │ 82-B frames over UDS                 │ owns: listener, conn, 4 KiB rx buf (preallocated),
        └──────────────────────────────────────┤        Producer<AiCmd,1024>, PmlrCapture(ai),
                                               │        ruleset stage/commit side-path
                                               │ parse IN PLACE from rx buf → verify → capture → try_push
                                               ▼
                                     Ring<AiCmd, 1024>  (#[repr(align(64))], SPSC, lock-free)
                                               │ try_pop (budgeted) in Engine::tick()
                                               ▼
                                     engine thread — sole consumer → StrategySet::on_ai fan-out
```

- The mio thread is the ring's **only** producer; the engine thread the
  **only** consumer — same `core-ring` machinery as every ingress
  (`Ring::new() → split()`).
- Ring full ⇒ drop + `ring_drops_total` (engine never blocks on AI; AI never
  blocks the engine).
- No cross-thread shared state beyond the ring: heartbeat/staleness is
  derived by the *consumer* from popped frames (§7), so no extra atomics.
- **Zero-copy accounting (doctrine):** the frame is parsed in place from the
  rx buffer (`&[u8]` view; no intermediate buffer, no allocation). The one
  unavoidable copy is the 64-byte slot memcpy into the ring on `try_push` —
  ownership transfer, identical to every other ingress. Documented here per
  house rule. Python side reuses one preallocated 82-B `bytearray` per
  connection for frame construction (`struct.pack_into`).
- Cross-process mmap ring remains **rejected** (CPython atomics; ~1 cmd/s is
  7 orders below UDS capacity — plan §8.3). Revisit only if rate matters.

### 4.4 Accept-path policy (in `ingress-ai`, order matters)

1. read ≥ 2 B → `len == 80` else drop conn (`protocol_err_total`)
2. read full 82-B frame (mio readiness loop; partial reads buffered)
3. HMAC verify → fail ⇒ drop conn + `hmac_fail_total` (no partial trust)
4. kind-range + per-kind field-shape check (§3 table) → fail ⇒
   `malformed_total`, frame discarded, conn kept
5. seq check vs last: `seq <= last` ⇒ `seq_regress_total`, discard;
   gap > 1 ⇒ `seq_gap_total`, accept (§6 monitors apply to the AI feed too)
6. `ts_ns := now_ns()` rewrite (§3); PMLR capture (`SlotKind::AiCmd = 4`) —
   capture BEFORE push so ring-dropped commands remain auditable (8e pattern)
7. `try_push` → full ⇒ `ring_drops_total`
8. Stage/Commit additionally routed to the side-path validator (§8)

Metrics (exposition names get `_total` where counters):
`engine_ingress_ai_{cmds,hmac_fail,protocol_err,malformed,seq_gap,seq_regress,ring_drops,expired,rejected_conns}_total`,
`engine_ingress_ai_last_heartbeat_age_ns` (gauge),
plus the standard capture pair `engine_ingress_ai_capture_{io_errors,records}`.

---

## 5. Worker daemon design (§8.2, Python 3.14, rewritten)

### 5.1 Subsystems (library core — mode-agnostic)

- **news_watcher** (`feeds.py`): httpx polling of the `RSS_FEEDS` allowlist
  (env moves here from engine config), 15–60 s per-feed cadence with jitter;
  dedupe by `(feed, guid)` in SQLite; triage via injected `complete_fn`
  (Haiku) → escalate to labeling (Sonnet): market-mapped, direction,
  confidence, half-life. Malformed model output degrades to no-op (never
  crashes the daemon loop; counted).
- **commander** (`commander.py`): labeled events + operator policy →
  `AiCmd` frames via `frames.py`/`uds.py`; TTL from label half-life;
  Heartbeat every 5 s (serve mode).
- **data_fetcher** (`features.py`): PMLR replay logs primary
  (`CLAUDE_WORKER_REPLAY_DIR` → engine's `MULTIVENUE_LOG_DIR`), venue REST
  secondary and rate-budgeted; emits compact per-symbol feature files
  (`CLAUDE_WORKER_FEATURES_DIR`). **Positions/P&L**: reconstructed from the
  engine's own `engine-fills.pmlr` (capture added in 8f, §2) + tick marks —
  exposure, open positions, realized/unrealized P&L per symbol, HIP-4
  netting (|yes−no|) mirrored per risk-policy (authoritative enforcement
  stays engine-side, 8i). 8f emits the fill data and the `fetch` output's
  position summary; the position-aware strategist features and the
  live-vs-backtest monitor consume it in 8h.
- **strategist** (`strategist.py`): `MODEL_STRATEGIST = "claude-fable-5"`
  (replaces the never-used `MODEL_HARD`), 6 h default cadence; proposes
  Tier-1 ruleset JSON artifacts (thesis, symbols, expected edge, caps ≤
  risk-policy); never touches the socket — output goes through gates.
- **backtester** (`backtest.py`): drives `multivenue-engine backtest`
  (subprocess seam; binary ships in 8h — seam is mockable now, canned
  reports in tests per plan §11); parses the machine-readable report;
  evaluates **gates** (thresholds in worker config, not prompts): OOS net
  P&L > 0 after fees+latency; ≥ 50 trades over ≥ 2 trading days; max DD ≤
  cap; bounds ≤ risk-policy. Fail ⇒ archived with report, never pushed.

### 5.2 Two frontends over one code path (§8.2.1)

- **FULL-AUTO** — `claude-worker serve` (the only daemon mode):
  `daemon.py` composes the subsystems on cadences, constructs the SDK client
  (`llm.py.make_client`) — **the only place `ANTHROPIC_API_KEY` is read**
  (`ServeConfig`). Single-threaded cooperative loop (poll cadences off a
  monotonic clock); SIGTERM/SIGINT → flush SQLite, close UDS, exit 0.
  No asyncio (offline soft path; synchronous is simpler to test — carried
  decision from the old wrapper).
- **SEMI-MANUAL** — no daemon. A Claude session (primed by
  `docs/prompts/ai-session.md`) does the reasoning itself and drives the
  pipeline with the verbs below. The verb layer handles HMAC+UDS; a
  Heartbeat frame precedes every verb-initiated push (same connection);
  `llm.py` client is never constructed.
- Mode is chosen by invocation; no mode flag; no divergent logic. The §4.2
  single-client rule makes concurrent modes physically impossible.

### 5.3 SQLite state (`state.py`; WAL; `CLAUDE_WORKER_DB`, default `~/multivenue/worker/state.db`)

```
seen_items    (feed TEXT, guid TEXT, first_ts INTEGER, PRIMARY KEY(feed,guid))
prompt_cache  (model TEXT, prompt_version_hash TEXT, content_hash TEXT,
               response TEXT, created_ts INTEGER,
               PRIMARY KEY(model,prompt_version_hash,content_hash))   -- PLAN §10.2, finally built
ai_seq        (id INTEGER PRIMARY KEY CHECK(id=1), next_seq INTEGER)  -- one namespace across BOTH modes
rulesets      (hash TEXT PRIMARY KEY, path TEXT, report_path TEXT, gates_passed INTEGER,
               author_mode TEXT CHECK(author_mode IN ('auto','session')),  -- §8.7 attribution
               model TEXT, thesis TEXT, staged_ts INTEGER, committed_ts INTEGER)
events        (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, kind TEXT, detail TEXT)
```

Seq allocation is transactional (`UPDATE … RETURNING`), so verb processes
and a later serve run share one strictly-increasing namespace; engine-side
gaps are counters, not faults.

### 5.4 Heartbeat / staleness semantics (both sides)

- serve: Heartbeat every 5 s from commander.
- verbs: exactly one Heartbeat immediately before the payload frame(s) of
  each invocation — then the connection closes. Between manual invocations
  the engine sees silence; that is *correct*: fail-safe means AI-derived
  state TTLs out rather than freezing in (§7). `ai-session.md` teaches this.
- engine (`strategy-ai-exec`): staleness = `now - last_accepted_frame_ns >
  15 s` ⇒ pull AI quotes, refuse intents; recovers on next valid frame.

### 5.5 Python 3.14 toolchain

`uv python install 3.14`; `.python-version` = `3.14`; `requires-python =
">=3.14"`; ruff `target-version` and mypy `python_version` = 3.14 (verified
in checklist item 1 — if ruff/mypy lag 3.14 support, pin the newest they
accept and note it). Item 1 also import-checks `anthropic`, `httpx`,
`typer`, `structlog`, `pytest` under 3.14 in a scratch venv — **no live
calls**. Old suite is never run; it dies with the old worker.

---

## 6. Operator verb CLI (exact surface)

All verbs: read `BaseConfig` (never the API key), open SQLite, connect UDS,
send Heartbeat, do their one job, close. Global exit codes:

```
0 OK        2 usage/validation (bad args, bad file, schema)   3 GATE REFUSED
4 transport (socket absent/busy/HMAC/protocol)                5 state (SQLite/seq)
1 unexpected exception (fail-fast; traceback to stderr)
```

```
claude-worker serve
    the only daemon mode; requires ANTHROPIC_API_KEY

claude-worker fetch [--replay-dir D] [--symbols CSV] [--no-rest] [--news]
    data_fetcher one-shot: replay logs (+budgeted REST) → feature files; prints paths.
    --news additionally runs one news_watcher poll cycle MINUS the LLM steps:
    fetch allowlist feeds, dedupe via the same SQLite (feed,guid), write
    items NDJSON {id, feed, ts, title, link, text} for the session to triage/
    label itself — in semi-manual the session IS the triage/labeling brain

claude-worker backtest --ruleset R.json [--replay-dir D] [--split 70/30]
    runs the harness via the subprocess seam, writes report JSON next to R,
    prints report path + gate verdict; exit 0 pass / 3 fail (report still written)

claude-worker push --kind heartbeat|enable|disable|set-fair-value|set-bias|set-param|order-intent|halt
    [--sym N|--symbol STR] [--venue V] [--strategy IDX] [--side bid|ask]
    [--px F] [--qty F] [--ttl-s F] [--param-id N] [--expire-on-silence]
    one frame (after the implicit Heartbeat); per-kind required-arg validation
    mirrors the §3 table; floats scaled 1e6 at pack time

claude-worker positions [--run-dir D|latest] [--json]
    lightweight live-state view for semi-manual triggers: tail engine-fills.pmlr
    + latest tick marks from the CURRENT run dir (capture flushes ≤1 s, so the
    view is ≤~1 s stale), print positions / exposure / realized+unrealized P&L
    per symbol (HIP-4 netting applied). Read-only: no UDS, no heartbeat, no
    seq consumed. This is THE answer to "query live data from the running
    engine" — the engine is a producer, never a server; its append-only
    capture is the query surface (metrics endpoint complements for coarse
    instantaneous state; post-8i the RiskGate book cross-checks this view)

claude-worker stage-ruleset --ruleset R.json --report REP.json
    GATE BINDING SITE: recompute sha256(R); require REP.ruleset_hash == it,
    REP.gates.all_passed == true, and REP produced by our backtest schema
    version; record in `rulesets` (author_mode from --by, default 'session'
    when invoked as a verb, 'auto' from serve); send RulesetStage{hash128}.
    Any mismatch ⇒ exit 3. NO OVERRIDE FLAG EXISTS.

claude-worker commit-ruleset --hash HEX32|--ruleset R.json
    require a staged, gates_passed row matching the hash; send RulesetCommit.
    Unstaged/failed hash ⇒ exit 3.
```

Notes: `serve`'s commander path calls the *same* `stage_ruleset()` /
`commit_ruleset()` functions — gates bind in code in both modes. UDS is
one-way (no request/response in v1 — keeps the engine side allocation-free
and simple); verbs report "sent", and application is observed out-of-band
via engine metrics/log (`engine_ai_ruleset_{staged,committed,rejected}_total`)
— `ai-session.md` documents the check. `tag-topics`/`parse-rules` one-shots
are **not** re-implemented: triage/labeling live inside news_watcher (auto)
or in the session's own reasoning (semi-manual); the *artifact file formats*
survive (§9) because engine boot loading (`research-artifacts`) is untouched
in 8f.

---

## 7. StrategySet + `strategy-ai-exec` runtime semantics (§8.5)

```rust
pub struct StrategySet {
    latency_arb: LatencyArb<64>, ev: EvStrategy<8>, cross_arb: CrossArb,
    rule_tree: RuleTree<8>, ai_exec: AiExec<64>,   // slot 5 = strategy-vm, reserved for 8g
    enabled: u8,
}
```

- Slots: 0 latency-arb, 1 ev, 2 cross-arb, 3 rule-tree, 4 ai-exec,
  5 vm (bit reserved now, member lands 8g — no dead member in 8f, per the
  unused-code rule).
- `impl Strategy for StrategySet`: static fan-out, one predictable branch
  per member per event (`on_start`/`on_tick`/`on_signal`/`on_fill`/timer/
  `on_ai`); `StrategyCounters` aggregates members (`strategy_kind` =
  `"set"`; per-member counters keep their own kinds for metrics breakdown).
- `on_ai` routing: Enable/Disable/HaltRequest/SetParam(set-level) are
  consumed by the set itself; everything else fans out to enabled members
  (ai-exec is the primary consumer; others get the default no-op).
- **Enable refused while halted; Disable always honored.** 8f "halted" =
  the engine's existing kill-switch/halt signal if present at integration
  time, else a set-local sticky halt flag driven by `HaltRequest` (8i
  replaces it with the real risk state machine). Refusals increment
  `engine_ai_enable_refused_total`.
- `--strategy` becomes the initial mask (single name = single bit,
  back-compatible; `all` = all built members).
- Memory: each member keeps its own book state in v1 (plan-accepted cost).
- `strategy-ai-exec`: fixed `[FairEntry; 64]` keyed by sym (open-addressed,
  no hashing in hot path — linear probe over 64), entries carry
  `{px_1e6, set_ns, ttl_ns, bias_1e6}` from SetFairValue/SetBias; quotes/
  takes when venue book deviates beyond edge param; honors OrderIntent
  (paper; RiskGate clamp when 8i lands); staleness per §5.4 pulls quotes +
  refuses intents; `expire_on_silence` flag ties an entry to heartbeat
  liveness in addition to its TTL.

### 8f ruleset side-path (stub scope)

`ingress-ai`'s side thread accepts Stage/Commit kinds, resolves
`AI_RULESET_DIR/<hash>.json` (default `~/multivenue/artifacts/rulesets/`),
recomputes sha256, bounds-checks the JSON shape (fixed caps: ≤ 256 rows,
symbols exist, caps ≤ risk-policy table), and records staged/committed
state + metrics. The double-buffered table flip into a live evaluator is
8g (`strategy-vm`); in 8f a Commit for a validated hash flips a state flag
observable in metrics — the full plumbing is proven end-to-end without an
evaluator behind it.

---

## 8. `docs/prompts/ai-session.md` — draft outline (deliverable + test surface)

1. **Role & authority** — you are the strategist/triage brain in semi-manual
   mode; the engine trusts frames, not prose; gates are in code; there is no
   override and no Resume.
2. **Environment map** — worktree paths; env keys the verbs read; where
   features / rulesets / reports / replay dirs live; how to read engine
   metrics to confirm application (names carry `_total`); where current
   positions/exposure/P&L appear (`claude-worker positions` — ≤~1 s-stale
   view from the running engine's capture; per-strategy order counters on
   the metrics endpoint) and that they MUST be consulted before pushing
   intents or new rulesets.
3. **Verb cookbook** — one entry per verb: exact invocation, required args
   per kind (§3 table), expected stdout, exit-code meanings, common
   failures (4 = engine down or serve holds socket; 3 = gate refusal is
   FINAL — fix the ruleset, don't fight the gate).
4. **Standard workflow** — fetch → inspect feature files → author ruleset
   JSON (schema + caps inline) → backtest → read report → stage → commit →
   verify via metrics → monitor; rollback = `push --kind disable
   --strategy 5` or stage/commit of the prior hash.
5. **News/labeling recipe** — read fetched items, reason, emit
   `set-fair-value`/`set-bias` with explicit `--ttl-s` sized to the event
   half-life; heartbeat semantics: your state TTLs out between pushes — that
   is by design; re-push to refresh.
6. **Safety rails** — never construct frames by hand; never touch the
   socket directly; halted engine refuses Enable (expect exit 0 send +
   `enable_refused` metric — check before retrying); paper-first; caps
   tighten-only.
7. **Session hygiene** — one action per verb, verify, log your reasoning to
   the session transcript; SQLite carries seq — don't parallelize verb
   invocations.

The scripted test drives this doc's workflow verbatim against the fake UDS
server (§11) — the doc and the code cannot drift silently.

---

## 9. Carry-over from the old worker (REFERENCE ONLY) and deletion blast radius

### 9.1 Mined for reuse (patterns re-implemented, not ported)

- **SDK seam**: module-level `make_client()` + `complete(client, req)`
  wrapper; monkeypatch point → becomes `llm.py` (same seam, now
  serve-only construction).
- **conftest FakeClient / _FakeMessages** programmed-responses pattern.
- **`test_imports_are_full.py`** AST walk — carried forward verbatim in
  spirit (full `import x` only; `__future__` exempt) + the dir-exists guard.
- **Prompt formats**: tagger vocab (`family ∈ {crypto, politics, sports,
  macro, other}`, `impact ∈ {low, med, high}`, reason ≤ 120) and the
  rule-extractor JSON schema (`name/family/trigger/edge_bps/horizon_ms/
  max_risk_usd`, strict bounds, bool-rejecting int coercion) — reused by
  labeling + as the artifact schemas `research-artifacts` still boots from.
- **Model constants** pattern → `MODEL_BULK` (Haiku 4.5), `MODEL_REASONING`
  (Sonnet 4.6), **`MODEL_STRATEGIST` (`claude-fable-5`)** replacing unused
  `MODEL_HARD`; CLAUDE.md model table updates accordingly.
- **Config style**: frozen slots dataclass, env-dict injection for tests,
  fail-fast `assert_complete`, `sk-ant-` shape check (serve config only).
- **pyproject skeleton**: hatchling, ruff single-line/full-import
  enforcement, strict pytest/mypy — regenerated for 3.14.
- Stale defaults NOT carried: `~/polymarket/...` paths (old config) →
  `~/multivenue/...`.

### 9.2 Deletion blast radius (`claude-worker/` is deleted wholesale in item 2)

Path and toolchain commands are preserved by the rewrite (same directory,
still uv+pytest+ruff), so these refs stay **valid unchanged**:
`Cargo.toml` workspace `exclude = ["fuzz", "claude-worker"]`; `Makefile`
`py-test`/`py-lint`; `.claude/hooks/post-edit-fmt.sh` claude-worker case;
`.claude/commands/test.md` step 5.

Refs requiring **edit in 8f** (worktree copies):
- `docs/local-setup.md` — worker section: serve daemon, new env keys, 3.14,
  `~/multivenue/worker/` state dir.
- `docs/architecture.md` — AI plane: boot-files-only → boot files (tags/
  rules, unchanged) **+ live UDS AiCmd path**.
- `CLAUDE.md` — directory guide (`Python 3.12` → `3.14, daemon + verbs`),
  model table (+ Fable 5 strategist), build/test lines unchanged.
- `docs/phase-8-architecture.svg` — "Python 3.12" text node → 3.14.
- `PLAN.md` — the two `3.12` claude-worker mentions (§12.2 vicinity, §
  worker process note); minimal touch only.
- `.env.example` — `+AI_INGRESS_SOCK`, `AI_INGRESS_HMAC_KEY`,
  `AI_RULESET_DIR`, `CLAUDE_WORKER_{REPLAY_DIR,DB,FEATURES_DIR}`;
  `RSS_FEEDS` annotated "worker-only" (engine key removed in the RSS
  commit).
- launchd: **no plist files exist in-repo** (PLAN prose only); PLAN §12.2's
  worker line gets the serve invocation. No launchd deliverable in 8f.

NOT touched (historical records): `docs/phase-5-plan.md`,
`docs/phase-6-plan.md`, progress logs. Rust crates whose comments mention
claude-worker (`research-artifacts`, `strategy-ev`, `strategy-rule-tree`,
`core-types`, `paper.rs`) stay true after the rewrite — no edits.

### 9.3 RSS removal blast radius (§8.1 — same treatment, sequenced LAST)

`crates/ingress-rss/` (delete), `crates/strategy-news/` (corpse, delete),
`fuzz/fuzz_targets/rss_item.rs` + its `fuzz/Cargo.toml` entry, 2 bench
alloc-assertions, `paper.rs` (~76 refs: `Rings.rss_signal`,
`Consumers.rss_signal`, `spawn_rss`, `RssFeed`, `RSS_FEEDS`,
`engine_rss_*` metrics), `core-config` `rss_feeds_csv`/`RSS_FEEDS` (+its
doc comment), `.env.example` RSS_FEEDS relocation, CPU core 4 → `ingress-ai`,
`.claude/agents/parser-property-tester.md` scope (`ingress-rss` → `ingress-ai`),
`SignalSource::Rss = 1` **stays reserved** (already marked retired in
wire-format.md). `core-net::http1` is NOT orphaned (8e made it the
discovery client). **Why last:** the remediation session is editing `cli`
against the soak; a 76-reference `paper.rs` sweep is the single biggest
merge-friction surface in 8f — doing it as the final, isolated, one-commit
change makes the eventual post-G1 merge conflict exactly one commit wide.

---

## 10. Configuration delta

New/changed env keys (all mirrored in `.env.example`):

```
AI_INGRESS_SOCK           default ~/multivenue/run/ai.sock      (engine + worker)
AI_INGRESS_HMAC_KEY       64 hex chars, required for ingress-ai (engine + worker)
AI_RULESET_DIR            default ~/multivenue/artifacts/rulesets (engine + worker)
CLAUDE_WORKER_REPLAY_DIR  points at engine MULTIVENUE_LOG_DIR    (worker)
CLAUDE_WORKER_DB          default ~/multivenue/worker/state.db   (worker)
CLAUDE_WORKER_FEATURES_DIR default ~/multivenue/worker/features  (worker)
RSS_FEEDS                 moves: engine key deleted in RSS commit; worker reads it
ANTHROPIC_API_KEY         read by `claude-worker serve` ONLY (§9 amendment)
```

Stage-2 session hygiene (isolation, binding rules): all tests override
`AI_INGRESS_SOCK=/tmp/stage2-ai-<pid>.sock`, `METRICS_BIND` to a
stage2-local port (never 9191), `MULTIVENUE_LOG_DIR` to a stage2-local dir;
nothing writes `~/multivenue/logs` or `/tmp/soak*`.

---

## 11. Test plan (house standard + §11 + dual-mode)

### Rust

| Surface | Tests |
|---|---|
| `AiCmd` POD | size/offset static asserts; per-kind shape validator unit tests (happy + failure per public fn) |
| frame parser | property test (arbitrary bytes never panic; round-trip pack→parse); **fuzz target `ai_cmd_frame`** registered in `fuzz/Cargo.toml` |
| `ingress-ai` | UDS loopback integration test (analog of the venue TLS-loopback standard): scripted client sends good frame, bad HMAC, short/torn frame, oversize len, seq regress/gap, second-connection reject, heartbeat cadence; asserts every counter |
| PMLR kind 4 | round-trip write/read; v≤2 reader accepts; audit-replay smoke over an ai capture |
| engine drain | TTL-expiry-on-pop, budget respected, malformed rejected |
| `StrategySet` | mask fan-out correctness; Enable-while-halted refused; Disable always; initial-mask from `--strategy` |
| `strategy-ai-exec` | fair-table TTL, staleness pull (15 s), `expire_on_silence`, OrderIntent paper flow |
| Alloc gate | new entries in `bench/tests/alloc_assertions.rs`: ingress-ai frame path (accept→verify→capture→push), StrategySet fan-out steady state, ai-exec on_tick/on_ai — release, `--test-threads=1`, **0 B/op** |

### Python (uv run pytest under 3.14; SDK mocked at `llm.py` seam — no live calls)

- `frames.py` golden vectors **shared with Rust**: a checked-in fixture of
  frame bytes + HMAC (test key) consumed by both suites — the two packers
  cannot drift.
- **Fake UDS server fixture** (accept, length-check, HMAC-verify with test
  key, record frames): commander tests, every verb test, heartbeat-precedes-
  push assertion, single-client interlock (exit 4).
- Gate binding: stage-ruleset refuses (report missing / hash mismatch /
  gates failed / schema version wrong) → exit 3; **CLI-surface test asserts
  no override flag parses**; verbs run with `ANTHROPIC_API_KEY` unset.
- serve loop: one composed iteration with canned feeds + mocked LLM +
  fake UDS: dedupe honored, cache hit on second identical prompt, heartbeat
  emitted, SIGTERM clean shutdown.
- `pmlr.py`: golden fixture produced by the Rust writer (committed bytes),
  v1 + v2.
- backtest gates: canned pass/fail reports (plan §11 pattern).
- `fetch --news`: canned feed XML served by a local httpx-mock transport;
  asserts dedupe against pre-seeded SQLite rows and the items-NDJSON schema
  (no LLM step involved — pure mechanics).
- `positions`: golden fills fixture (Rust-writer bytes incl. a HIP-4
  yes/no pair) → known positions/exposure/P&L; `latest` run-dir resolution;
  torn-final-record tolerance (reader stops cleanly mid-flush).
- **Scripted semi-manual session test (8f amendment):** executes the
  `ai-session.md` §4 workflow verb-by-verb as subprocesses against the fake
  UDS server + canned backtest report: fetch → backtest (pass) → stage →
  commit → push disable; plus the refusal path (failing report → stage
  exit 3, commit of unstaged hash exit 3). Proves SEMI-MANUAL end-to-end
  with zero SDK construction.
- Carried: imports-are-full test; config tests (base vs serve key split).

### System (operator-gated)

The 8f live demo (AI cmd → strategy toggle observed on the running engine)
runs ONLY on explicit operator go, after the soak ends and `pgrep` shows no
other engine. Until then, everything above runs stage2-local.

---

## 12. Ordered implementation checklist (each step: code + tests green on the Mac before the next)

| # | Item | Size |
|---|---|---|
| 1 | **Python 3.14 toolchain**: `uv python install 3.14`; scratch venv; import-check anthropic/httpx/typer/structlog/pytest; record exact versions in phase-8f-progress.md | S |
| 2 | **Delete `claude-worker/` entirely; scaffold fresh**: pyproject (>=3.14), `.python-version`, ruff/mypy/pytest config, `__init__`, `config.py` (Base/Serve split), imports-full + config tests green | M |
| 3 | `core-types::AiCmd` (+kinds, consts, asserts, shape validators) + `core-io` SlotKind 4 decode + wire-format.md row | M |
| 4 | `core-crypto`: truncated-tag helper + constant-time compare (if absent) + NIST/known-answer vectors | S |
| 5 | **`ingress-ai` crate**: listener, peer-cred, in-place parser, HMAC, seq policy, capture, metrics, ring producer; UDS loopback suite; `ai_cmd_frame` fuzz + proptest; alloc assertion | L |
| 6 | Engine: `ai_cons` lane + budgeted drain + TTL-on-pop + `Strategy::on_ai` + cli spawn wiring (core 4 note; env keys) + engine-thread fills capture (`engine-fills.pmlr`, kind 2, alloc-asserted) | M |
| 7 | `StrategySet` crate/module: fan-out, mask semantics, halt refusal, `--strategy` mask; alloc assertion; unit suite | L |
| 8 | `strategy-ai-exec`: fair table, staleness, intents (paper); unit + alloc | L |
| 9 | Worker core I: `frames.py`, `uds.py`, `state.py` (+ fake UDS fixture, golden frame vectors shared with Rust) | M |
| 10 | Worker core II: `pmlr.py`, `features.py`, `feeds.py`, `labeling.py`, `backtest.py` (gates), `commander.py` — all mocked-LLM tests | L |
| 11 | `daemon.py` + `serve` (SDK seam lives here; heartbeat 5 s; signals) + serve-loop test | M |
| 12 | Operator verbs in `cli.py` + exit codes + heartbeat-precedes-push + gate binding; no-override CLI test | M |
| 13 | **`docs/prompts/ai-session.md`** + scripted semi-manual session test | M |
| 14 | Engine-side ruleset stage/commit side-path stub (§7) + metrics | M |
| 15 | Docs sweep (§9.2 list) + `.env.example` + config.example.toml reference block | S |
| 16 | **RSS removal — LAST, one commit** (§9.3 sweep); workspace green after | L |
| 17 | Final gates on the Mac: `cargo nextest run --workspace`; release alloc assertions `--test-threads=1` 0 B/op; fuzz targets `cargo check`; `uv run pytest` (3.14); phase-8f-progress.md closing entry | M |

Commit granularity: one well-messaged commit per numbered item (some L items
may split, e.g. 5a listener / 5b policy+tests). All notes go to
`docs/phase-8f-progress.md` — never the other session's file.

### 12.1 Session plan (estimate: 6–8 sessions)

Sizing driver is context burn from Rust compile/test loops (45 s-capped MCP
terminal calls, nohup+poll, worktree cargo contention with the soak/fix
session), not code volume. Python items are cheap per context (pytest
seconds vs cargo minutes).

| Session | Items | Content |
|---|---|---|
| S1 | 1–4 | 3.14 toolchain · delete+scaffold worker · `AiCmd` POD + SlotKind 4 · core-crypto helper |
| S2 | 5 | `ingress-ai` alone (listener, peer-cred, parser, loopback suite, fuzz, alloc assert) — biggest single Rust item |
| S3 | 6–7 | engine AI lane · `strategy-set` |
| S4 | 8–9 | `strategy-ai-exec` · Python frames/uds/state + fake UDS fixture + shared golden vectors |
| S5 | 10–12 | worker library II · daemon/serve · operator verbs |
| S6 | 13–15 | ai-session.md + scripted session test · ruleset side-path stub · docs sweep |
| S7 | 16–17 | RSS removal (isolated one-commit sweep) · final gates + closing progress entry |

Variance: −1 session if S3/S4 pack well; +1–2 if `ingress-ai` or the alloc
gates fight back, or worktree cargo contention makes compile loops expensive.

### 12.2 Session handoff protocol (mandatory)

Every session ends by appending to `docs/phase-8f-progress.md`:

1. **Status** — checklist items done (with commit hashes), item in flight,
   gates last run and their results.
2. **Interim state** — any deviation from this design (what + why), open
   defects, stale-rmeta / toolchain landmines hit.
3. **Exact resume point** — file + next action, precise enough to resume
   without re-deriving anything.
4. **Kickoff prompt for the next session** — a ready-to-paste prompt block
   containing: worktree path + branch + isolation rules (unchanged),
   required reading delta (this design doc §§ relevant to the next items,
   plus the latest progress entry — NOT the full Stage-1 reading list),
   the next session's checklist items, session facts (MCP
   `executeInShell=true` ≤45 s, nohup builds, cargo Mac-only, stage2-local
   UDS/env overrides), and any amendments the operator issued mid-session.
   The prompt must state that this design doc + latest progress entry are
   authoritative over the committed plan.

Context-shortage rule (kickoff directive) folds into this: if context runs
short mid-item, write §§1–4 immediately at the current boundary and tell the
operator to relaunch with the generated prompt.

---

## 13. Design decisions — RESOLVED by operator 2026-08-15

1. **AiCmd TTL clock base**: `ingress-ai` rewrites `ts_ns` to engine
   monotonic at accept (after HMAC verify, before ring push); TTL is
   clock-coherent; worker send time preserved in PMLR capture only (§3).
2. **`push --kind order-intent`**: available in semi-manual from day one,
   paper-only; RiskGate clamp arrives with 8i. No config allowlist.
3. **StrategySet home**: new crate `strategy-set`; `strategy-core` stays
   dependency-clean.
4. **CLI library**: Typer stays (verbs + serve); 3.14 compat verified in
   checklist item 1; argparse is the documented fallback if it fails.
5. **Ruleset hash**: hash128 (first 16 B of SHA-256) in `px`+`qty`,
   single-frame atomic Stage/Commit; full hash in report + registry;
   engine side-path recomputes full SHA-256 from the file.
6. **Heartbeat/staleness**: 5 s / 15 s compile-time constants, env-tunable
   in tests only — staleness is not loosenable in prod.

---

*Design complete; decisions above locked. Awaiting operator review of this
document before any implementation (checklist item 1 starts only on
explicit go).*
