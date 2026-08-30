# The AI-strategy pipeline

**Status:** current as of 2026-08-30 (VM2 V8 opening act live, xv-v2 `bfbc5349…` committed).
**Companion diagram:** `docs/ai-strategy-pipeline.svg`.
**Authority above this file:** `docs/vm2-plan.md` §8/§9 → `docs/stage2-finish-plan.md` → `docs/mvp-completion-plan.md` (§7 gate, §9 data law) → `CLAUDE.md`.
This file is descriptive, not normative — where it disagrees with those, they win.

---

## 0. The one-paragraph version

Six venues stream into the engine, which writes every normalized record to append-only PMLR
capture. Offline Python folds that capture (plus budgeted public REST) into a candle/funding/IV/depth
store and per-run feature files. A **strategist** — today Claude-in-session, at Stage 3 the keyed
`serve` daemon — reads those files and authors a **ruleset**: a ≤256-row table in a fixed grammar of
17 features and 4 combines. The ruleset is scored by the **real engine VM replaying the real
capture** through a frozen subprocess contract, and either passes five hard gates or is refused with
exit 3 and no override. A passing artifact is installed by content hash, then **staged** and
**committed** over an 82-byte HMAC'd UDS frame; the engine validates it a second time with the *same*
validator, ping-pongs it into the VM, and flips it live on the Commit. From then on it trades on
every tick with **no AI, no worker, and no cron in the loop**. Its orders are stamped with strategy
slot 5 into `engine-orders.pmlr`, which is what `audit-pnl`, `parity` and `audit-replay` read to
close the loop back into the next authoring round.

The AI is a *compiler*, not a *runtime*. Nothing Claude produces ever executes as code — it produces
data that a hand-written validator either admits or rejects.

---

## 1. The two strategy carriers

Everything below distinguishes these two, and the whole VM2 V8 workstream is about migrating the
second onto the first. (`docs/research-universe.md` §1 is the binding statement.)

| | **Ruleset VM — slot 5 (`s5`)** | **Signal cron + Intent lane — slot 4 (`s4`)** |
|---|---|---|
| Lives in | the engine, Rust, on the tick path | the worker, Python module + launchd |
| Needs a cron running? | **No** — evaluates every tick with no AI, no worker | **Yes** — entries and exits only happen when the cron fires |
| Backtestable by the frozen verb? | Yes, natively | No |
| Expresses | the fixed v2 grammar (§4) | arbitrary Python (REST at decision time, cross-coin ranking, dynamic pair selection) |
| Today carries | `xv-v2` cross-venue reversion | CVFC-1 carry, S1 funding-spread pilot, the `hl↔bn-usdm` xv pair |
| Emits as | `Order.strategy_id = 5` | `Order.strategy_id = 4` (`OrderIntent`, AiCmd kind 6) |

Both are **paper-only** until the operator opens the Stage-3 gate. Both obey the same $50k research
tier: **≤$10k/order, ≤$20k/symbol, ≤$100k table total, DD ≤$7,500**. Both land in the same
`engine-orders.pmlr` and are audited by the same `audit-pnl`. That shared ground truth is exactly
what makes the **parity window** (§10) possible.

Slot 0 (`latency-arb`) runs beside them and is not AI-authored at all.

---

## 2. Stage A — CAPTURE (engine, continuous, no AI)

**Actor:** `multivenue-engine run --paper --strategy all`, under launchd `com.multivenue.engine`,
KeepAlive, with graceful restarts at 00:00 / 08:30 / 16:05Z. One run directory per boot; the daily
restart makes one gap-free run per UTC day.

Every ingress thread parses venue bytes → a normalized 64-byte `Tick` → the engine ring, and the
*same* record is appended by `PmlrCapture` to disk. Capture is not a debug feature — §6.5 makes it
the product, and a capture-open failure is a fatal boot error.

**`<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`** — PMLR v2, 64-byte header + fixed-width slots, O(1)
slot-indexed, append-only, torn-tail tolerant:

| file | `SlotKind` | slot size | content |
|---|---|---|---|
| `<venue>-ticks.pmlr` | 0 `Tick` | 64 B | BBO |
| `<venue>-signals.pmlr` | 1 `Signal` | 64 B | strategy signals |
| `engine-fills.pmlr` | 2 `Fill` | 64 B | paper fills |
| `engine-orders.pmlr` | 3 `Order` | 64 B | **accepted intents — the attribution log (M4.1)** |
| `ai-cmds.pmlr` | 4 `AiCmd` | 64 B | **every accepted AI command, ts-rewritten** |
| `<venue>-events.pmlr` | 5 `ChannelEvent` | 64 B | funding / sub-drop / establishment |
| `<venue>-opt-summary.pmlr` | 6 `OptSummary` | 64 B | option mark / IV / OI |
| `<venue>-depth.pmlr` | 7 `DepthTopK` | **192 B** | L2 top-5 ladder (WS10-B) |
| `<venue>-raw.tap` | — | var | optional `PMRT` payload tap |

Sidecars, rewritten every boot:

- **`instrument-manifest.tsv`** — `<sym_u32>\t<descriptor>` for **every** allocated instrument.
- `options-manifest.tsv` — the pre-D3 options-only form, kept one release.

**Why the manifests are load-bearing:** option and PM-daily ordinals are allocated in selection order
and **reshuffle every boot by design**. A bare `SymbolId` is therefore meaningless across runs.
Every offline consumer joins by **descriptor string** (`okx:BTC-USDT`, `binance-usdm:cotiusdt`,
`deribit:BTC-30AUG26-77500-C`, a bare PM token id) and resolves through the run's own manifest.
Skipping this is what produced a phantom $248.8M per-symbol bound in V7.

Venue labels, in venue-byte order: `pm bn okx rpc deribit hl bybit`.

---

## 3. Stage B — DERIVE and FETCH (worker, offline, serialized)

Two distinct outputs feed the strategist. Neither ever touches the hot path, and every worker
invocation is **globally serialized** (`pgrep -f 'claude[-_]worke[r]'` first — one SQLite seq
namespace).

### 3.1 `candles.db` — the research data plane

`~/multivenue/worker/candles.db` (SQLite, WAL). Written by *modules*, never verbs, on launchd cadence:

| module | table |
|---|---|
| `claude_worker.candles` | `candles`, `candle_conflicts` |
| `claude_worker.funding` | `funding` (per-print rates, 5 venues) |
| `claude_worker.iv_digest` | `iv_digest` (1m/1h IV from kind-6 slots) |
| `claude_worker.depth_digest` | `depth_digest` (hourly imbalance-OHLC, spread bps, near notional) |
| `claude_worker.refdata` | `refdata` (24 h quote volume, open interest) |
| `claude_worker.channel_map` | `channel-map.tsv` (per-descriptor feature capability) |
| `claude_worker.coverage_audit` | hollow-lane report — expected vs present, per class |

**§9 data law, binding:**

- Every table is PK'd on `(venue, descriptor, …)`. **Never a bare SymbolId.**
- Ticks are canonical and lossless; candles are derived. Aggregation flows one way only.
- **Fetch one base per horizon, derive the rest.** Fetched: 1m (48 h), 1h (90 d), 1d (lifetime).
  Never fetched: 5m / 15m / 4h — derived exactly (O=first, H=max, L=min, C=last, V=sum),
  `source='derived'`.
- Open bars upsert until they close; **closed bars are immutable** — a disagreeing re-fetch is
  recorded in `candle_conflicts`, never silently overwritten.
- Capture-derived candles carry `source='capture'` and **NULL volume** — we capture BBO, not prints,
  and volume is never fabricated.
- Budgets are **per REST host** (`budget_key`), demand-sized `max(floor, 2 × tfs × targets)`. The
  earlier per-*venue* pooling starved `binance-usdm` to zero pages for weeks; the coverage audit
  caught it on its first run.

### 3.2 `claude-worker fetch` — the per-run feature files

```
claude-worker fetch [--replay-dir D] [--symbols CSV] [--no-rest] [--news]
```

Writes `$CLAUDE_WORKER_FEATURES_DIR/<run>/<sym>.json` (BBO / mid / spread / tick-rate),
`<sym>-ohlcv.json`, `<sym>-meta.json`, and `news/items-<ns>.ndjson`.

It also **owns `market-map.json`** (`{"markets": {name: sym}, "hip4_pairs": [[yes, no]]}`), and the
ownership rule matters: refresh is **additive only**. A name that now resolves to a different sym is
reported as a *conflict* and the operator's entry is left untouched. Writes are `tmp` + `os.replace`.

REST consumers are all keyless and public — PM Gamma, OKX candles, Deribit TradingView chart, HL
`candleSnapshot` — behind a 60 req/venue/h `RestBudget`. `--news` is a **mechanical** pull and dedupe;
no LLM runs inside `fetch`.

The `CLAUDE_WORKER_UNIVERSE_FILE` seam lets `fetch` replicate the Rust `core-config::universe`
allocation law (PM token[0] → 42, BN spot[0] → 7, otherwise `venue_byte << 24 | ordinal`, USDM base
512) and propose map names for syms the capture actually observed.

---

## 4. Stage C — AUTHOR: the ruleset grammar

### 4.1 Who authors, today and later

**Today (semi-manual, standing operator ruling #5):** `ANTHROPIC_API_KEY` is not provisioned, so
`claude-worker serve` never runs and no Anthropic API call is made from this stack at all. The
strategist **is Claude-in-session**, working the verb-by-verb cookbook in `docs/prompts/ai-session.md`
§4 — which is pinned by `claude-worker/tests/test_session_scripted.py`, so the doc and the code cannot
drift silently. Orientation material for that session is `docs/research-universe.md`.

**At Stage 3 (code-complete, never keyed):** `daemon.ResearchCycle` inside `serve` drives
`strategist.py` against `MODEL_STRATEGIST = "claude-fable-5"` on a 6 h cadence
(`CLAUDE_WORKER_STRATEGIST_INTERVAL_S = 21600`), ≤2 calls per cycle (one proposal, one revision), hard
daily ceiling 12. A cycle with no fresh capture is skipped entirely; identical prompts hit a SQLite
`prompt_cache` and cost nothing.

### 4.2 The artifact

A JSON file, `{"rows": [ … ]}`, 1–256 rows. Its **content hash is its identity**: the full SHA-256 of
the file bytes; the first 16 bytes (32 hex chars) are the `hash128` that travels on the wire and names
the file on disk.

Every row is one statement of the same shape:

```
signal = combine( feat_a(instrument, window_a),  feat_b(ref, window_b) )
```

**Signal domain law:** every feature and combine output is an `i64` in **×1e9 of its natural unit**
(prices ×1e9, APR/IV/imbalance fractions ×1e9, bps ×1e9, notional USD ×1e9, clock seconds ×1e9).
Thresholds live in the same domain, which is why `enter`/`exit`/`confirm` are parsed at 9 decimals —
6 is too coarse for a funding rate.

**17 features** (`FeatId`, wire-stable, never renumbered):

| class | features |
|---|---|
| price | `mid` `bid` `ask` |
| rolling (require `window_min` ∈ 1…4320) | `roll_mean` `roll_ema` `roll_min` `roll_max` `roll_std` |
| funding | `apr24` `apr72` `clock_to_funding` |
| options (kind 6) | `mark_px` `mark_iv` |
| depth (kind 7) | `depth_imb` `depth_spread_bps` `depth_notional` |
| clock | `clock_utc_sod` |

**4 combines** (`CombineOp`), but only **three JSON tokens**: `diff` (natural units — APR and IV
spreads) · `diff_bps` (relative price deviation) · `ratio` (the `Ratio1e9` variant). The fourth,
`LhsOnly`, has **no token at all** — it is what the engine infers for a single-leg row. That falls out
of the combine law: `combine` is **required with `ref` and forbidden without it**.

**Direction law:** a positive signal means ASK the instrument (sell the rich leg / short the
higher-funding venue), negative means BID. Refire rows may pin a `side` filter instead.

**Positions:** `exit` present ⇔ position row. One universal exit law — **`signal × entry_sign ≤ exit`**
— covers both decay and sign flip. `min_hold_s` gates exits; `max_hold_s` is an unconditional age-out.
Pair rows hedge both legs at equal notional. `group` (0–254) enforces **at most one position per
group, first qualifying row in table order wins** — the MAX_POSITIONS pattern.

**Sizing:** `max_risk_usd` is **per leg**. Validator rule 7 statically sums *every* row × its legs
against 10k/20k/100k and is deliberately **group-blind**, so a wide table forces smaller legs. That
arithmetic is why the merged artifact is the hard one.

**Absences worth knowing:** there is no `Last` feature — trade prints are not captured anywhere in the
stack, and neither is open interest as a live channel. An empty feature window is **ABSENT**, not
zero: rows hold rather than assume.

**v1 sugar** (`trigger.type = cross_deviation | level_breach`) maps fully onto v2 at build time, so
there is exactly one evaluator path and no v1 branch.

**What the grammar cannot express — and therefore stays on s4:** dynamic best-pair venue selection per
coin (approximable as one row per pair sharing a group), global position-count caps beyond groups,
anything needing REST or history at decision time, and cross-coin breadth/rank statistics.

---

## 5. Stage D — GATE: the backtest

```
claude-worker backtest --ruleset R.json [--replay-dir D] [--split 70/30]
```

This is the **frozen contract**. The Python side spawns exactly:

```
multivenue-engine backtest --ruleset R --replay-dir D --split 70/30
```

and parses schema-1 JSON off stdout. **The harness conforms to the worker, never the reverse.**
`backtest.py` and `cli.py` have been byte-untouched across five sessions.

### 5.1 What the harness actually does

1. **k-way merge** across runs; total order `(ts_ns, venue byte, per-file record index)`. Records from
   different runs never interleave. Timestamps are rebased onto a virtual clock at
   `VIRT_T0 = 100_000_000_000_000_000`.
2. **The same validator** — `ingress_ai::validate_ruleset`. No second parser exists to drift.
3. **The real evaluator** — an actual `VmStrategy`, fed by `receive_table_v2` plus a synthesized
   `AiCmd{kind: RulesetCommit}` through the real `on_ai`. If `commits_applied != 1` it is an internal
   error, not a result.
4. **Warmup is table-global**: the longest window any row references. One `apr72` row zeroes the whole
   backtest on a root younger than 72 h. This is the merged-artifact sequencing trap.
5. **Manifest rebind + dead-descriptor drop**: syms are rejoined by descriptor against the newest
   run's manifest; a descriptor the newest manifest no longer carries has *all* its records dropped.
6. **Fill model** (`crates/cli/src/backtest/fill.rs`, doctrine: *the backtest may under-promise; it
   must never over-promise*):
   - **Strict-cross maker.** A resting BID at `P` fills only when `ask_px < P` — strictly through,
     never a touch. Fill price is always `P`. Displayed opposite size is a **shared budget** across all
     our resting orders on that sym, consumed FIFO. Zero RNG.
   - **Activation latency** `t_emit + Δ`: PM 200 ms, BN/OKX/Deribit/Bybit 100 ms, **HL 600 ms**. An
     order can never fill on its own emitting tick.
   - **Open-order caps** 8 per sym, 64 total; beyond-cap emits are counted and dropped.
   - **D-7 options mark-fill:** option syms have no book, so they fill in full at
     `mark ± max(0.5% of mark, 1 tick)` with **taker** fees — and any report that used it must print
     the assumption.
   - Fees round **up**; net rounds **down**; drawdown and bounds round **up**. Nothing rounds in our
     favour. Fixed point throughout — px/qty ×1e6, money ×1e12, no floats.
7. **Split semantics:** `70/30` on the virtual clock. Every order is tagged at *intake*
   `oos = emit_virt_ts >= boundary`. **Two books run over the same fill stream** — the full book
   (IS+OOS) produces the observed `bounds` maxima, and a breach *anywhere* disqualifies; the OOS book
   produces net / trades / days / drawdown. An IS-emitted order whose fill lands after the boundary
   counts in the full book but never in the OOS verdict. The degenerate `0/100` form is carved out for
   the all-OOS monitor.
8. Backtest reads **PMLR only, never candles** — candle-fed fills would be fabricated.

### 5.2 The five gates

`GateThresholds` in `claude-worker/src/claude_worker/backtest.py`:

| gate | threshold | note |
|---|---|---|
| `pnl_positive` | OOS net **> $0.00** | strict |
| `min_trades` | **≥ 50 legs** | counts *legs* (fills) since D-3; **plus ≥ 10 round trips iff `position_rows > 0`** |
| `min_days` | **≥ 1 OOS trading day** | amended 2 → 1 on 2026-08-30 |
| `max_drawdown` | **≤ $7,500** | 15% of the $50k research book |
| `bounds` | **≤ $10k order / $20k symbol / $100k table** | *observed* maxima over IS+OOS, not declared |

**Exit codes:** `0` gates passed · **`3` gates refused — final, there is no override flag anywhere on
the CLI surface** (a test scans every verb's `--help` for `--override/--force/--skip-gates/…`) · `2`
report untrusted (hash mismatch, wrong schema, harness failure) · `4` transport · `5` state.

The report is written **either way**, next to the ruleset as `R.report.json`.

**On the `min_trading_days` 2 → 1 amendment:** the recorded rationale is MVP tempo — a ~12 h capture
age should suffice to stage. The recorded trade-off is that a floor of 1 lets the OOS verdict come
from a single day's regime, which is precisely what the old floor guarded. It proved its own risk the
same session: the first post-amendment rerun failed honestly (OOS −$5.81, DD $131, symbol bound
$59.6k), per-pair probes isolated the damage entirely to `hl↔bn-usdm` (unfilled exits stacking on a
thin overnight book at the model's 600 ms HL latency), and the fix was to retune to okx-only rather
than to loosen anything. Revisit at the M6 soak.

---

## 6. Stage E — PROMOTE: install → stage → commit

Three steps, and every one of them can only tighten.

### 6.1 Install by content hash

Copy `R.json` → `$AI_RULESET_DIR/<hash128>.json` (32 lowercase hex + `.json`, default dir
`~/multivenue/artifacts/rulesets`). Without this the engine's side path counts a reject when the Stage
frame arrives — the frame carries **only the hash**, never the bytes.

### 6.2 `stage-ruleset` — the gate binding site

```
claude-worker stage-ruleset --ruleset R.json --report R.report.json [--by session|auto]
```

Recompute SHA-256 of the file → require the report's `schema_version == 1`, its `ruleset_hash` to
match, and `gates.all_passed is True` → **else `GateRefused`, exit 3**. Then write the registry row in
`state.db`, *then* send. Record-then-send.

### 6.3 The wire

One-way, single-client `AF_UNIX` at `$AI_INGRESS_SOCK` (`~/multivenue/run/ai.sock`), parent dir 0700,
socket 0600, peer-euid checked. **82 bytes:**

```
[len u16 LE = 80] [AiCmd 64 B] [tag 16 B]
tag = HMAC-SHA256(AI_INGRESS_HMAC_KEY, cmd_bytes[0..64])[0..16]
```

`AiCmd` is 64 B `repr(C)`: `ts_ns, seq, sym, px, qty, ttl_ns, kind, venue, strategy_id, side,
param_id, flags, [16 zero pad]`. The ruleset `hash128` rides in `px` (bytes 0–7) and `qty` (bytes
8–15). Kinds: **7 = `RulesetStage`, 8 = `RulesetCommit`**, with `strategy_id` pinned to 5.

**There is no response frame.** The worker only `sendall`s; the engine never writes back. Outcomes are
observed through `/metrics` counters and `ai-cmds.pmlr`. Every verb sends an implicit Heartbeat first,
which is why live evidence reads *stage seq 48 / commit seq 50* — heartbeat 47, stage 48, heartbeat
49, commit 50, all drawn from one monotonic SQLite allocator.

**Never parallelize verb invocations.** The seq allocator is the single frame namespace.

### 6.4 Accept path (`ingress-ai`, §4.4 — the order is load-bearing)

`len == 80` → HMAC (constant-time) → `validate_shape()` → seq policy → **`ts_ns` rewritten to engine
monotonic** → **append to `ai-cmds.pmlr` *before* the ring push**, so even a ring-dropped command stays
auditable → `try_push` → Stage/Commit additionally routed to the side-path seam. Failures at steps 1–2
drop the connection; step 3 discards the frame and keeps the connection.

### 6.5 Side path — the second validation

`RulesetSidePath::stage(hash128)` reads `$AI_RULESET_DIR/<hex>.json` (documented copy #0 — this path is
deliberately *off* hot path and may allocate) and runs `validate_ruleset` into a `Box<RuleTableV2>`
scratch allocated once at boot. Rule 1 is the hash check and strictly precedes any parsing. Rules 2–10
cover grammar, numbers, row count, names, symbols, static caps, duplicate identity, positions,
features/bind budget, and descriptor resolution.

**Descriptors resolve at stage time**, against a `DescriptorTable` built from the same allocation
truth that writes `instrument-manifest.tsv`. Unresolvable ⇒ `Descriptor` refuse. This is exactly why
the post-boot re-commit (§8) is what makes options tradeable across ordinal reshuffles.

On success the table is stamped `epoch = self.epoch + 1` and pushed by value (copy #1, 32,832 B) onto
a 2-slot ring. A successful Stage sets `committed = None` — **a new Stage supersedes an old Commit.**

### 6.6 Engine side — staged, then the flip

Inside one `Engine::tick()`, the table lane is drained **immediately before** the AI-command lane, so a
Stage and Commit arriving in the same batch always land in the right order:

```
table lane:  try_pop → StrategySet::on_ruleset_table → vm.receive_table_v2   (copy #2)
             ⇒ tables[(active&1)^1] = table;  staged_valid = true
             ⇒ NOT mask-gated

AI lane:     drain (budget 8/iter) → TTL check → re-validate shape → StrategySet::on_ai
             ⇒ RulesetCommit fans out to enabled members ⇒ MASK-GATED by BIT_VM
             VmStrategy::on_ai: staged_valid && staged.hash128 == cmd.hash128
                ⇒ active ^= 1          ← the flip is an index swap, zero copy
                  staged_valid = false; commits_applied++
                  on_table_flipped(): rebind ≤4 feature legs per row,
                                      all 256 last_fire_ns = 0, all 256 positions FLAT
                ⇒ else commits_dropped++
```

Rolling-window history is deliberately discarded on flip so new windows warm honestly.

**The gating asymmetry is the single most surprising fact in this pipeline, and it is standing
procedure:** `on_ruleset_table` is *not* mask-gated, `on_ai` *is*. With slot 5 disabled you can still
stage — the table survives the disabled window — but the Commit is silently swallowed. Recovery after
a rollback is therefore **enable, then re-commit**, never enable alone and never restage-first.

Within one 5 s metrics period a successful promotion shows:
`engine_vm_table_epoch 0 → 1`, `engine_vm_rows_active 0 → len`,
`engine_ai_ruleset_{staged,committed}_total` each +1, `rejected_total` unchanged,
`engine_strategy_enabled_mask` still 49.

---

## 7. Stage F — EXECUTE (live, no AI in the loop)

`StrategySet` is a statically composed struct — no `dyn`, no allocation after boot — fanning each tick
to enabled members in slot order:

| slot | bit | member |
|---|---|---|
| 0 | 1 | `latency-arb` |
| 1 | 2 | `ev` |
| 2 | 4 | `cross-arb` |
| 3 | 8 | `rule-tree` |
| 4 | 16 | `ai-exec` — the s4 Intent lane |
| 5 | 32 | `vm` — the ruleset VM |

`--strategy all` requests 63, but slots 1/2/3 need `--artifacts-path` / `--groups` / `--rules-path`,
which the launchd wrapper does not pass. So the live mask is **`63 & 49 = 49`** (`latency_arb=true
ai_exec=true vm=true`). Disabling slot 5 gives **17**; re-enabling gives 49 back.

Every member callback is wrapped in `StampCtx`, which writes the slot into `Order.strategy_id` (byte
41 of the 64-byte `Order`) on submit. That one register write is the whole attribution mechanism —
per-*ruleset* attribution is deliberately not embedded, and is reconstructed offline by joining vm
orders against the `RulesetCommit` timeline in `ai-cmds.pmlr`.

VM evaluation per tick: features update for **every** tick regardless of table state; an empty table
takes the inert branch and returns. Otherwise a linear scan over active rows — exit path first
(`max_hold_s` age-out unconditional, then `min_hold_s` gate, then the universal exit law), then the
refire/entry path behind the `horizon_ms` cooldown, then `confirm`, then group occupancy, then sizing
clamped by a second independent per-order cap that mirrors the validator's.

---

## 8. Stage G — RESTART CONTINUITY

**Committed tables are in-memory only.** The engine reads nothing ruleset-related at boot: the VM boots
inert, `rows_active 0`, `epoch 0`, hash all-zero. That is normal, not an error. Two backgrounded hooks
in `scripts/engine-wrapper.sh` restore state:

1. **`recommit-ruleset.sh` (operator ruling #7b)** → `python -m claude_worker.recommit
   --wait-sock-seconds 180`. Re-stages and re-commits the registry's active ruleset. Because
   descriptors resolve at stage time, this is also what **re-binds option and PM-daily instruments to
   this boot's ordinals**. Fail-safe by design: it refuses rather than guesses.
2. **`seed-push.sh`, after a 45 s grace** so the re-commit lands first (a seed against an inert VM is
   refused) → `python -m claude_worker.seeds`:
   - **`FundingSeed` (AiCmd kind 10)** — 73 h of raw venue funding prints from the `funding` table,
     manifest-resolved, ≤640/sym newest-kept, so `apr24`/`apr72` are warm immediately instead of after
     three days.
   - **`PositionSeed` (kind 11)** — restores open positions from the previous run's slot-5 FIFO fold
     in `engine-orders.pmlr`; sym re-resolved through the *current* manifest, `px` = surviving-basket
     VWAP, `qty` = **age in seconds**, `ttl_ns` forced 0.

Proven live 2026-08-30: 1,669 frames, 52 funding descriptors, `cmds_total` 1674, rejected 0.

`MULTIVENUE_SEED_RULESET` in `.env` is the operator lever — unset gives funding seeds only.

---

## 9. Stage H — OBSERVE, and the edge that closes the loop

| tool | invocation | reads | produces |
|---|---|---|---|
| `audit-pnl` | `multivenue-engine audit-pnl --dir <root\|run>` | `engine-orders.pmlr`, ticks, opt-summary, `ai-cmds.pmlr`, manifests | modeled + paper P&L, per strategy slot **and per ruleset hash** |
| `pnl_report` | `python -m claude_worker.pnl_report` (nightly 00:20Z) | spawns `audit-pnl` | `reports/pnl-<day>.json` + `.summary.txt` |
| `pnl` verb | `claude-worker pnl [--date] [--json]` | those files | the operator's read (thin; no socket, no spawn) |
| `parity` | `python -m claude_worker.parity --window-h 48` | `engine-orders.pmlr` only | s4-vs-s5 verdict (§10) |
| `audit-replay` | `multivenue-engine audit-replay --dir run-<ns>` | the whole run dir | cadence/integrity verdicts + the full AI-command chain |
| `/metrics` | `127.0.0.1:<port>/metrics` | live counters | `engine_vm_*`, `engine_ai_ruleset_*`, `engine_ingress_ai_*` |

`audit-pnl` reuses `backtest::fill` **verbatim** with `boundary_virt_ns = 0`, so the shadow-P&L and the
gate speak the same economics. It reuses the same fill model, the same rounding, the same latency
table.

The feedback edge is real: these reports become the `ACTIVE RULESET WALK-FORWARD`, `POSITIONS` and
`PER-STRATEGY SHADOW P&L` sections of the next authoring digest.

---

## 10. The parity window (VM2 V8, open now)

The migration proof. One ground truth for both sides — `engine-orders.pmlr` — because the cron and the
VM write to the same file, on the same clock, through the same manifests. **No cron state file is
trusted.**

- **Event law:** chronological net-fold per `(slot, descriptor)`. |net| increase = entry, decrease to 0
  = exit, sign flip = both. Every **cron** event needs a matching VM event on the same descriptor,
  same kind, same direction, within the family tolerance — **xv 600 s, carry 7200 s**. Unmatched cron
  events are **misses**; unmatched VM events are **extras** (informational — the VM legitimately
  trades rows the crons never carried).
- **Position law:** end-of-window net **sign** agreement per descriptor. Sizes are deliberately not
  compared — the group-blind cap arithmetic forces different leg sizes.
- **GREEN = 0 misses AND 0 position disagreements**, judged **per family**, not globally.
- P&L is not recomputed here; `audit-pnl` owns economics.

**Window:** ≥48 h, opened ~2026-08-30 08:55Z with `xv-v2` `bfbc5349…` (rows 1, epoch 1). Earliest GREEN
close ~Sep-1 09:00Z.

**Known standing miss:** the xv cron still carries the `hl↔bn-usdm` pair that the retune dropped, so
`parity[xv]` will report its entries as misses until the operator rules its fate. The carry family
stays RED-by-absence until `cvfc-v2` / `s1-v2` stage.

**Cron bootout is operator-only, explicit, per family, after that family's parity is green:**

```sh
launchctl bootout gui/$(id -u)/com.multivenue.xv      # after xv parity GREEN
launchctl bootout gui/$(id -u)/com.multivenue.carry   # after cvfc + s1 parity GREEN
```

The **data** agents (funding, candles, iv, depth) stay regardless — data agents are not strategy
carriers. Plists archive to `~/multivenue/archive/launchd/`; they are not deleted.

---

## 11. Rollback

Live-demonstrated twice (8h H6a, H6b-SEMI) and shaped exactly like the deferred §8.5 automation:

```
1.  claude-worker push --kind disable --strategy 5     # mask 49 → 17
        rows_active and epoch HOLD — the mask gates evaluation, not storage
        fires freeze, even past a row's horizon
2.  restage + commit the prior gates-passed hash from its bound paths
        stage lands (not mask-gated); commit is SWALLOWED (mask-gated)
3.  operator act: push --kind enable --strategy 5      # mask 17 → 49
4.  re-commit the prior                                 # epoch bumps, prior goes live
```

Step 3 is a **human** act by standing ruling — there is no auto re-enable, and `HaltRequest` is sticky
with **no Resume on the wire at all**. `audit-replay` renders the whole chain with zero integrity
violations, which is how the rollback was proven rather than asserted.

---

## 12. Live today vs deferred to Stage 3

**Live (2026-08-30):** capture on 6 venues · every offline derive module · the `fetch` verb ·
Claude-in-session authoring · the full backtest harness and all five gates · install / stage / commit
through the frozen verbs · the VM executing `xv-v2` on the tick path · the seed lane · `audit-pnl`,
`pnl_report`, `parity`, `audit-replay` · semi-manual promotion **and** the §8.5-shaped rollback, both
demonstrated live.

**Deferred to the Stage-3 entry gate (`docs/mvp-completion-plan.md` §7):**

- `ANTHROPIC_API_KEY` provisioning — and therefore `serve`, `strategist.py`, and Fable-5 authoring.
- **§8.1 auto-promotion** — `serve` installing into `$AI_RULESET_DIR` then calling
  `stage_ruleset(..., by="auto")` → `commit_ruleset` with no operator verb. E2E-proven; *live* proof
  deferred.
- **§8.3 monitor + §8.5 automatic rollback.** Its shape is fully specified: re-run the **active**
  ruleset over a trailing 24 h of capture with `--split 0/100` (floor 6 h — below it the monitor
  *skips*, it never guesses), trigger on `net ≤ −$100` **or** `DD ≥ $200`, act by Disable-5 then
  restage-and-commit the prior. Note it is a *walk-forward* trigger rather than a live-P&L one for a
  structural reason: paper mode produces zero fills, so a realized-P&L trigger could never fire.

**The gate, exactly:** key provisioning → one keyed Fable-5 `serve` cycle with §8.1 auto-promotion
observed live → one §8.3 monitor-triggered §8.5 rollback observed live — **before** any executor, risk
or dispatcher work begins. It is the operator's to open.

---

## 13. Why this is safe — the invariants

1. **One validator.** Engine and harness call the same `validate_ruleset`. No second parser can drift.
2. **The gate is bound to the artifact by hash.** `stage-ruleset` recomputes SHA-256 and refuses a
   report that does not match. You cannot stage a ruleset that was scored as a different file.
3. **No override exists.** Exit 3 is final; a test enforces the absence of a bypass flag on every verb.
4. **Caps are enforced three times** — statically by validator rule 7, again at emit by the VM's own
   `POLICY_SINGLE_ORDER_CAP`, and again as *observed* bounds in the gate. Only tightening is possible.
5. **The AI never executes code.** It emits data into a fixed grammar. Malformed output is a rejected
   candidate, not a crash.
6. **Every accepted command is captured before it is acted on**, and `ts_ns` is rewritten to engine
   monotonic — so the audit chain is the engine's own clock, not the sender's claim.
7. **The transport is one-way, single-client, HMAC'd, euid-gated, 0600.** A verb reports "sent"; only
   `/metrics` and the capture confirm application.
8. **Fail-safe everywhere.** Feature pool exhaustion leaves a feature ABSENT and rows hold. Capture I/O
   errors sticky-disable the sink rather than kill the session. A seed against an inert VM is refused.
   The monitor skips rather than guesses.
9. **The hot path stays clean.** The frame pump is 0 B/op and alloc-asserted; the ruleset side path is
   deliberately off hot path and its allocations are enumerated (copies #0–#2).

---

## 14. Strategist drift — found, and fixed 2026-08-30

None of this affected the live semi-manual lane. All of it would have hit the automated one the moment
a key was provisioned: the strategist was the only component still speaking the pre-VM2 world.

**Fixed (`claude-worker/src/claude_worker/strategist.py`, `daemon.py`, `tests/test_strategist.py`):**

1. **The static system prompt taught the superseded tier** — "≥2 OOS trading days, DD ≤ $200,
   ≤$100/row, ≤$250/sym, ≤$1000 table" — against live thresholds of 1 day, $7,500 and $10k/$20k/$100k.
   Rewritten to the numbers `GateThresholds` actually enforces, including the ≥10-round-trip rider on
   position tables and the table-global warmup trap. A new test pins the prompt to the gate module and
   fails on the stale strings, so this cannot drift silently again.
2. **`ROW_MAX_RISK_USD` was 100.0** — the strict parser would have rejected every $1,400–$9,900 row the
   operator lane actually authors. Now 10,000.0, mirrored from `ingress_ai::RULE_ROW_MAX_RISK_1E6`, with
   `SYM_MAX_RISK_USD` / `TABLE_MAX_RISK_USD` published beside it so the prompt can state the whole
   tier. The per-sym and per-table Σ walks stay rule 7 in Rust — they need resolved symbols.
3. **The parser emitted v1 rows only.** It now carries the full v2 arm: 17 features, the 3 combine
   tokens, descriptor strings, positions, groups, holds and confirms, with v1 XOR v2 arm selection
   mirroring `parse_and_admit_row`. Structural checks only, per the no-second-deep-parser doctrine —
   descriptor resolution, name uniqueness, the cap Σ, row identity and the channel/bind budget stay
   engine-side. **Verified against production:** all seven real V7/V8 artifacts — `xv-v2` (live), the
   18-row `merged-v2`, `cvfc-v2`, `s1-v2` and the three generality proofs — now round-trip through the
   mirror; under the old code every one of them was refused.
4. **The digest had no instrument vocabulary.** v2 names instruments by descriptor string, so a keyed
   `serve` literally could not author a valid v2 row. `build_digest` gained an `INSTRUMENTS` section
   fed by `instruments_digest_text`, which reads the newest run's `instrument-manifest.tsv` — the same
   file the engine's `DescriptorTable` is built from — and labels each descriptor's channels through
   the pinned `caps_of_descriptor` mirror. `daemon.py` passes it.
5. `STRATEGIST_PROMPT_VERSION` bumped `strategist-v1` → `strategist-v2`, as the module's own rule
   requires: every v1-era `prompt_cache` entry is stale by construction.
6. **`docs/vm2-plan.md` §9's runbook cited the pre-retune `merged-v2` `4d5dbe65…` / 19 rows / $99.4k.**
   Corrected to `79eaceec…` / 18 rows / $85.6k. The §8 log entries above it are dated history and were
   deliberately left alone — a log records what was true when.

**Deliberately left:**

- `docs/arch/phase-8h-design.md` §7.2 carries the same stale cap block. `docs/arch/` is CLOSED history
  under the archive law — never written to, read only for archaeology. It is superseded, not wrong.
- `docs/prompts/ai-session.md` does not mention the `pnl` verb. Cosmetic, and the doc is pinned by
  `test_session_scripted.py`, so changing it means changing the pin.

**Gate after the change:** worker pytest **595 passed** on the Mac (was 553), no regressions.

---

## 15. Quick reference

**Environment seams** — `AI_INGRESS_SOCK` · `AI_INGRESS_HMAC_KEY` (64 hex, permanent in `.env`, never
read or printed by a session) · `AI_RULESET_DIR` · `MULTIVENUE_LOG_DIR` = `CLAUDE_WORKER_REPLAY_DIR` ·
`CLAUDE_WORKER_DB` · `CLAUDE_WORKER_FEATURES_DIR` · `CLAUDE_WORKER_MARKET_MAP` ·
`CLAUDE_WORKER_CANDLES_DB` · `CLAUDE_WORKER_REPORTS_DIR` · `MULTIVENUE_SEED_RULESET`.

**Verb surface (frozen 7 + the one D1-sanctioned addition)** — `serve` · `fetch` · `backtest` · `push`
· `positions` · `stage-ruleset` · `commit-ruleset` · `pnl`.

**AiCmd kinds** — 0 Heartbeat · 1 Enable · 2 Disable · 3 SetFairValue · 4 SetBias · 5 SetParam ·
6 OrderIntent (slot 4) · **7 RulesetStage** · **8 RulesetCommit** · 9 Halt (sticky, no Resume) ·
10 FundingSeed · 11 PositionSeed.

**Two SQLite stores, never mixed** — `state.db` is the control plane (seq allocator, dedupe, prompt
cache, ruleset registry, events ledger); `candles.db` is the research data plane.

**Session laws** — one engine ever · worker verbs globally serialized · never parallelize verbs ·
explicit-path git staging · no push/rebase/branch without the operator · never touch `.env` ·
compile and test on the Mac, never in the sandbox.

---

## 16. Read next

`docs/vm2-plan.md` (§8 log, §9 runbook) → `docs/research-universe.md` (grammar §6, the authoring
catalog) → `docs/prompts/ai-session.md` (the verb-by-verb cookbook) → `docs/mvp-completion-plan.md`
(§7 gate, §8 automation, §9 data law) → `docs/wire-format.md` (slot and row layouts) →
`docs/risk-policy.md` (kill-switch and caps).
