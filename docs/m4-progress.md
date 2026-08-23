# M4 — Shadow-P&L attribution: progress log

Phase authority: `docs/mvp-completion-plan.md` §4-M4 + §6 risks + §8
open question 4 + §9.9 (consumer matrix, BINDING) → this log's latest
entry → CLAUDE.md. Operator go recorded 2026-08-22 (M2-close session;
design entry reviewed before implementation). Commits are
operator-authorized, `M4:` prefix, explicit paths only. Post-M2
single-lane session — the CLAUDE.md protocol laws (one engine,
serialized worker invocations, explicit-path staging, M3-owned paths
untouched until C6) remain in force.

---

## 2026-08-22 — DESIGN ENTRY (operator review requested before code)

### Goal (mvp-plan §4-M4, restated)

One offline command answering "what P&L would the LOGGED strategy
intents have generated", per strategy and per ruleset hash, modeled
through the SAME §4 strict-cross fill law the backtest harness uses —
rendered BESIDE the engine's paper-fill view for the same window (two
views, one report), deterministic, JSON + human summary; a nightly
report lands automatically. Nothing here executes an order anywhere
(mvp-plan §7).

### Ground truth at design time (grep-level; M4.1 audits field-by-field)

- `engine-fills.pmlr` EXISTS (engine thread, `SlotKind::Fill`; the
  8f positions/P&L feed). The worker's `FillRec` decodes
  `ts/sym/side/px/qty/order_id` — NO strategy field on the wire
  today; the Fill slot carries a 24 B explicit pad.
- `SlotKind::Order = 3` is DEFINED (enum + writer roundtrip) but NO
  capture path writes Order slots — there is no `engine-orders.pmlr`.
  The §2-inventory phrase "order intents are captured and counted"
  = per-strategy /metrics counters + paper fills, not an intent log.
- Per-run `options-manifest.tsv` (M2 close) resolves option syms →
  descriptors; non-options have no per-run name sidecar yet.

### M4.1 — Attribution wire audit (read-only slice, lands first)

Field-by-field answers, written into this log + docs/wire-format.md
notes where they differ from assumption:

1. What exactly does the paper lane write per fill (who stamps
   `order_id`, is it strategy-recoverable through any existing chain
   — e.g. AiCmd seq / vm row / strategy slot in `Order` the moment it
   is captured)?
2. Is a fill 1:1 with an intent in paper mode, or can intents exist
   that never paper-fill (cooldown refusals, caps)? The answer
   decides whether `engine-fills` alone can seed the modeled lane or
   an intent log is REQUIRED for fidelity.
3. Migration proposal (expected outcome, to be confirmed): extend the
   Fill slot INSIDE its 24 B pad — `strategy_id: u8` +
   `ruleset_hash128: [u8;16]` (zero for non-vm strategies) + 7 B pad
   — layout-compatible (old readers skip pad; new readers gate on a
   flags/presence rule), documented in docs/migration.md; AND/OR
   start writing `engine-orders.pmlr` (`SlotKind::Order`, the intent
   log §4-M4 names) from the same engine-thread capture seam with the
   same attribution fields. Fuzz/proptest per house rule wherever a
   parser changes; capture stays append-only; PMLR version stays 2 if
   pad-additive, else bumps with a migration entry.

### M4.2 — `audit-pnl` (new offline cli subcommand; audit-replay doctrine — allocates freely, doctrine header)

- **Inputs (§9.9 BINDING)**: PMLR ticks + logged intents + fills from
  a run root or single run dir (window flags mirror capture-catalog's
  `--dir`; run selection/k-way merge/VIRT_T0/wall law REUSED from
  `backtest.rs` — one merge law, `pub(crate)` lending exactly like
  the catalog did; the harness itself stays PMLR-ticks-only and
  byte-untouched).
- **Shadow-fill semantics (THE design pin)**: an intent becomes a
  RESTING maker order at `intent_ts + latency` (the §4 model's
  latency constant) at its logged px/qty/side; it fills when the
  opposite side STRICTLY CROSSES its price (the harness's
  strict-cross law — trade-through, not touch), fees applied at the
  §4 rates, fixed-point accounting throughout; TTL honored where the
  intent carries one; unfilled residue expires silently. The fill
  LAW is the harness's own code path reused in-crate — audit-pnl
  duplicates NOTHING of the model (no twin to drift).
- **Two views, one report**: modeled fills for EVERY strategy's
  intents (including intents the paper lane refused/never filled, if
  M4.1 finds such exist) rendered beside the engine paper-fill P&L
  for the same window (from `engine-fills`), keyed per strategy and
  per ruleset hash128.
- **Keying law (§6 risk)**: cross-run aggregation keys are
  per-strategy / per-ruleset-hash ONLY. Per-instrument breakdowns
  key by venue+descriptor where a run's manifest resolves the sym;
  syms with no descriptor lane aggregate per-run only (bare SymbolId
  NEVER crosses a run boundary). See D3 below — generalizing the
  manifest removes the residual class entirely.
- **Output**: schema-versioned one-line JSON on stdout (catalog
  pattern; stderr-pinned tracing, deterministic byte-identical
  reruns) + human summary on stderr: per strategy — net, trades,
  fills, win days, max drawdown, per-UTC-day buckets; modeled-vs-
  paper deltas for the paper-filled subset.
- **Tests**: golden fixture runs via the real `PmlrWriter`
  (crafted intents + ticks with hand-computable strict-cross
  outcomes), determinism (byte-identical rerun), refusal shapes
  (overlap/empty/no-intents), per-strategy split pins, descriptor
  keying through a crafted manifest; nextest floor grows from 1227.

### M4.3 — Reports lane (worker-side, additive files only until C6)

- NEW module `claude_worker.pnl_report` (`python -m …`; the
  candles/iv_digest precedent — NEVER a verb): invokes
  `multivenue-engine audit-pnl` by PATH-resolved name (the pinned
  §14 contract), writes `~/multivenue/worker/reports/pnl-<utcday>.json`
  + a rendered text summary; idempotent per day; serialized like
  every worker invocation (pgrep-first, off top-of-hour).
- The §7.1 digest performance seam extension ("active ruleset only" →
  full strategy set) rides the report file — the strategist digest
  reads it at M5.
- pytest additions in a NEW test file; 429 stays the floor.

### Operator decisions requested (D1–D3; implementation blocked only on these)

- **D1 — worker surface**: §4-M4's literal menu (`positions
  --by-strategy` flag or a thin `pnl` verb) conflicts with the FROZEN
  7-verb surface. PROPOSAL (recommended): the `claude_worker.pnl_report`
  MODULE above — zero verb-surface change, cli.py stays byte-frozen,
  the frozen-202 stay untouched. Alternative: operator unfreezes the
  surface for a `pnl` verb (not recommended before Stage 3).
- **D2 — cadence (§8 OQ4)**: PROPOSAL nightly — the report module
  runs once per UTC day shortly after the 00:00Z restart closes the
  day's run dir (exact trigger: M3-owned launchd; see D2b). Per-run
  reports remain available on demand via the subcommand. D2b: the
  nightly TIMER is a `~/Library/LaunchAgents` plist — M3-owned
  path; M2-close precedent says coordinate, don't touch: either M3
  wires it at its C6+ window (coordination note in their log, like
  the digest cadence), or the operator sanctions a single M4-owned
  plist file now. PROPOSAL: coordinate at C6+ (manual nightly
  invocation documented until then); mvp-plan §4-M4's "nightly
  report lands automatically (launchd timer)" then closes at the C6
  window rather than inside M4 — flagged honestly as a deferred
  exit-line item.
- **D3 — manifest generalization**: extend the per-run manifest to
  EVERY allocated instrument (all venues: PM token ids, BN
  spot/usdm, OKX, Deribit, HL — the bin already holds every
  name+sym at boot; ~small render() extension, file stays
  `options-manifest.tsv`? → PROPOSAL: new name
  `instrument-manifest.tsv` with the options file kept one release
  for reader compat, wire-format.md updated). This makes EVERY
  per-instrument key descriptor-clean across runs (audit-pnl, M5
  worker naming, catalog). RECOMMENDED: yes, inside M4.2.

### Estimates (mvp-plan: 3–4 d total)

M4.1 0.5 d (audit + migration design entry) · M4.2 2 d (subcommand +
fill-model reuse + tests + live proof on the real root) · M4.3 1 d
(worker module + report + docs). Gates every slice: nextest ≥ 1227 /
alloc 37 corrected-guard / pytest ≥ 429 / fuzz on any changed parser
≥ 300 s; live smoke per pitfall #11 wherever capture format moves.

**Resume point if context dies here:** design entry complete,
AWAITING operator rulings D1–D3 (+ review of the M4.1 migration
direction); no M4 code exists yet; M2 close landed as `255f7ca`.

### OPERATOR RULINGS D1–D3 (2026-08-22/23, recorded verbatim-in-effect)

- **D1 = UNFREEZE for a `pnl` verb.** The operator lifts the 7-verb
  freeze for EXACTLY ONE additive verb: `pnl` (thin — reads/invokes
  `audit-pnl` output; no research-lane semantics). Scope of the
  unfreeze, pinned here: `cli.py` gains the verb ADDITIVELY;
  `backtest.py` argv/schema-1 stays FROZEN; the frozen-202 pytest
  surface MUST STAY GREEN unchanged (if any frozen test literally
  pins the verb LIST, STOP and put the conflict to the operator —
  never edit a frozen test); `docs/prompts/ai-session.md` is
  test-pinned (test_session_scripted.py) and stays byte-untouched —
  `pnl` is an ops verb, not a research verb, so no drift there.
- **D2 = NIGHTLY, timer wired at C6+** (as proposed): manual nightly
  invocation documented until M3's C6 window lands the launchd
  timer; §4-M4's "lands automatically" exit line transfers to the C6
  window (recorded as a deferred exit item, operator-sanctioned).
- **D3 = GENERALIZE the manifest in M4.2**: per-run
  `instrument-manifest.tsv` covering EVERY allocated instrument on
  every venue; `options-manifest.tsv` kept one release for reader
  compat (digest reader follows to the new file with fallback);
  wire-format.md updated.

Implementation UNBLOCKED. Slices proceed M4.1 → M4.2 → M4.3.

---

## 2026-08-23 — M4.1 AUDIT FINDINGS (read-only pass complete) + the ratified migration shape

### Findings (code-cited)

1. **Paper mode captures NEITHER intents NOR fills.**
   `PaperDispatcher::submit` counts `stats.accepted` and DROPS the
   order (`_order`, never stored); `try_next_fill` is always `None`
   (clob-dispatcher/src/lib.rs 283–296). The engine's fill capture
   (engine/src/lib.rs 316–374: venue fill lanes + the D3 dispatcher
   pump, staged to `engine-fills.pmlr` BEFORE the strategy callback)
   therefore never fires in `--paper` — the file is header-only in
   every paper run. The mvp-plan §2 sentence "order intents are
   captured and counted per strategy" is half-true: they are COUNTED
   (per-member `orders_emitted` aggregation in strategy-set →
   /metrics), never CAPTURED. G4's analyzer has NOTHING to read
   today — the intent log is the enabling work, not a verification.
2. **`SlotKind::Order = 3` is defined, writer-roundtrip-tested, and
   wired to NOTHING** (core-io/src/pmlr.rs 258/374; no capture site
   writes it). `engine-fills.pmlr` is the only engine-side capture
   (`ENGINE_FILLS_FILE`, engine/src/lib.rs 43).
3. **`Order` carries NO attribution**: ts/sym/side/kind/px/qty/
   client_oid/venue + 23 B explicit pad (offsets 41..64). `Fill`
   likewise (24 B pad). The strategy identity exists only INSIDE
   strategy-set's dispatch (members are called synchronously; the
   set aggregates per-member `orders_emitted` counters), and never
   reaches the Order bytes — `ctx.submit` is called by the member on
   the engine-owned `EngineCtx` with no member tag.
4. **Ruleset identity is already on capture**: vm intents can be
   attributed to a ruleset hash128 WITHOUT touching the Order slot —
   the ai-cmds capture carries the RulesetCommit timeline (`AiCmd`
   kind 8; hash128 rides px/qty as hash[0..8]/hash[8..16] LE,
   wire-format AiCmd table) — a ts-ordered join reconstructs "active
   hash at intent time" offline.
5. **No historical Order files exist** ⇒ the Order slot's CAPTURED
   layout is being defined for the first time — a layout amendment
   has ZERO reader-compat surface (documented in migration.md as
   pre-first-capture; PMLR version stays 2).

### Ratified migration (within the reviewed design-entry direction)

- **M-a `engine-orders.pmlr`**: `SlotCapture<Order>` on the engine
  thread (the exact `fill_capture` pattern: staged, zero-alloc,
  flush-interval law), appended in `EngineCtx::submit` AFTER
  `disp.submit` returns Ok — capture-what-was-accepted; refusals
  stay counters. Uniform file set: the file exists whenever fills
  capture exists (header-only when no strategy fires). bench: the
  SlotCapture alloc assertion extends to `<Order>` (gate stays 37 —
  same assertion body, or grows to 38 if a new fn is cleaner;
  recorded either way).
- **M-b Order layout amendment** (wire-format.md table + core-types
  static asserts + migration.md entry): offset 41 `strategy_id: u8`
  (`0xFF` = none/unattributed; strategy-set slot ids otherwise),
  `_pad1` shrinks to 14 B, `_pad2` unchanged. ONE byte claimed;
  hash128 deliberately NOT embedded (finding 4 — the timeline join
  is capture-truth already).
- **M-c attribution stamp**: a monomorphic `StampCtx<'_, C: Ctx>`
  adapter INSIDE strategy-set wrapping the engine ctx per member
  callback — `submit()` stamps `order.strategy_id = member_slot`
  then forwards; members stay byte-untouched; no `dyn`, no hot-path
  cost beyond one register write. Bare single-strategy boots submit
  through the engine ctx unwrapped ⇒ `0xFF` (audit-pnl labels the
  boot's sole strategy from run context; recorded semantics).
- **M-d joins in audit-pnl** (M4.2): per-strategy = Order.strategy_id;
  per-hash = vm orders (slot 5) joined against the ai-cmds commit
  timeline; fills (live-mode, Stage 3+) join orders on
  `Fill.order_id == Order.client_oid` where the paper pump / venue
  supplies it — NO Fill layout change in M4.

Next: implement M-a..M-c + tests (proptest/fuzz untouched — no new
untrusted-bytes parser; Order bytes are engine-authored), then gates,
then the M4.2 subcommand.

---

## 2026-08-23 — M4.1 IMPLEMENTED (M-a/M-b/M-c landed; gates + smoke below)

### What landed, by crate

- **core-types (M-b)**: `Order.strategy_id: u8` at offset 41
  (`STRATEGY_ID_NONE = 0xFF` const; `Order::new` initializes it;
  `_pad1` 15 → 14). Layout tests amended: the documented-offsets
  byte test pins offset 41 = `0xFF` default; the fully-explicit sum
  comment carries the new field.
- **engine (M-a)**: `ENGINE_ORDERS_FILE = "engine-orders.pmlr"`;
  `order_capture: Option<SlotCapture<Order>>` beside the fills sink;
  ALL NINE `EngineCtx` construction sites (tick / signal / fill-lane
  / D3 pump / AI ×2 / timer / start / stop) now carry
  `order_capture: self.order_capture.as_mut()`;
  `EngineCtx::submit` stages the order AFTER `disp.submit` returns
  Ok (capture-what-was-accepted; refusals stay counters);
  `set_order_capture` / `maybe_flush_order_capture` /
  `order_capture_records` / `order_capture_io_errors`; `stop()`
  drains both sinks. Hot-path delta: one `Option` reborrow per ctx
  build + one staged 64 B copy per ACCEPTED submit — the proven
  SlotCapture pattern, zero-alloc.
- **strategy-set (M-c)**: `pub StampCtx<'a, C: Ctx>` (monomorphic
  adapter; `submit` stamps `order.strategy_id = slot`, forwards;
  `now_ns` passthrough). ALL member callback forwards
  (on_start/on_tick/on_signal/on_fill/on_ai/on_timer/on_stop × 6
  members) wrap through it with the member's `SLOT_*`. Members
  byte-untouched.
- **cli**: `open_orders_capture` (+ crate-root re-export) beside
  `open_fills_capture`; `Observability.orders_capture` +
  `with_orders_capture`; engine_loop takes ownership + flushes on
  the 5 s report tick; NEW gauges
  `engine_orders_capture_{records,io_errors}` (registered + mirrored
  centrally like the fills pair); bin opens the file after the fills
  capture with the same fatal-on-open-failure stance.
- **bench**: NEW `engine_orders_capture_and_stamp_are_zero_alloc`
  (SlotCapture<Order> append/flush + StampCtx stamp, 10k iters,
  0 B/op) — the alloc gate grows **37 → 38**.
- **docs**: wire-format.md Order table (offset 41 + the
  engine-side-files paragraph now lists fills/orders/ai-cmds),
  migration.md dated entry (pre-first-capture amendment, zero
  reader-compat surface).

### Tests added

engine: `orders_capture_logs_accepted_intents` (submit → staged →
PMLR roundtrip; bare-strategy `strategy_id == STRATEGY_ID_NONE`
pinned), `orders_capture_skips_refused_submits` (refuse-all
dispatcher ⇒ header-only file), `orders_capture_getters_zero_without_capture`.
strategy-set: `stamp_ctx_attributes_member_orders` (direct adapter
law + through-the-set: the latency-arb trigger pair emits ONE order
stamped `SLOT_LATENCY_ARB`; everything else on the Order untouched).
Fuzz: NOTHING owed — no new untrusted-bytes parser on either side
(Order bytes are engine-authored; the worker gains no new parser in
this slice).

### Gates (all on the Mac)

- workspace nextest **1232/1232** (+1 skipped) = 1227 + 3 engine + 1
  strategy-set + 1 bench. New stay-green floor **1232**.
- release alloc **38/38** 0 B/op (corrected clean, fresh `Compiling
  bench` grep-count 1, `--test-threads=1`) — the NEW
  `engine_orders_capture_and_stamp_are_zero_alloc` holds 0 B/op.
  **Alloc gate floor is 38 now** (documented growth, §11 discipline).
- worker pytest **429/429** (Python untouched; serialized).
- fuzz: standing (nothing owed, rationale above).

### Live smoke (pitfall #11 — capture format moved ⇒ real boot)

G0 relink → bootout standing lane → foreign boot on the operator
universe (`run --paper --strategy all --metrics`, ~100 s) → SIGTERM
drain → verify → `launchctl bootstrap` back (pid verified, fresh run
dir).

- run-1787419437232124000: **`engine-orders.pmlr` = 503 records**
  (latency-arb fired live through the set), `engine-fills.pmlr`
  header-only — EXACTLY the audit's paper-mode finding, now with the
  intent side populated. ZERO error lines.
- Byte-level verification (fresh-terminal python over the raw file):
  header magic `PMLR` kind 3; **all 503 records
  `strategy_id = 0 = SLOT_LATENCY_ARB`** — the StampCtx attribution
  law working END-TO-END on the live engine (not `0xFF`: the stamp
  really lands through `--strategy all`); first record fully sane
  (sym 42 = the PM anchor, side Bid, kind limit, px 575_000 = 0.575,
  qty 10_000_000, client_oid 1 monotonic, venue 0 = Polymarket).
- Standing lane restored on the M4.1 binary — it now writes the
  intent log continuously (additive file; older tooling ignores it).

### Slice checkpoint — COMMIT ASK (pending operator authorization)

`M4:`-prefixed, EXPLICIT paths only:

- `crates/core-types/src/lib.rs`
- `crates/engine/src/lib.rs`
- `crates/strategy-set/src/lib.rs`
- `crates/cli/src/paper.rs`
- `crates/cli/src/lib.rs`
- `crates/cli/src/bin/multivenue-engine.rs`
- `crates/bench/tests/alloc_assertions.rs`
- `docs/wire-format.md`
- `docs/migration.md`
- `docs/m4-progress.md` (new)

NOT staged: `docs/m3-progress.md` (M3's WIP + the M2-close
coordination note — rides M3's next commit); `.env`; `~/multivenue/*`.

**Resume point if context dies here:** M4.1 code-complete +
gates-green (1232/38/429) + live-proven (503 stamped intents);
commit ask pending; NEXT = M4.2 (audit-pnl subcommand + D3
instrument-manifest generalization; design pinned in the entry
above — fill law REUSED from backtest.rs, never twinned).

(M4.1 COMMITTED `89d9348`, operator-authorized, 10 explicit paths;
post-commit tree carried only M3's dirty log.)

---

## 2026-08-23 — M4.2 IMPLEMENTED: `audit-pnl` + the D3 instrument manifest (gates + live proof below; commit ask at end)

### (D3) `instrument-manifest.tsv` — the generalized descriptor lane

- `options_manifest.rs`: NEW `INSTRUMENT_MANIFEST_FILE` +
  `render_instruments()` — EVERY allocated instrument,
  `<sym_u32>\t<descriptor>` per line, FINAL §9.4 strings: the static
  lanes' descriptors come PRE-BAKED from `core-config::universe`
  (`Instrument.descriptor` — the M1d allocation law; PM tokens =
  bare token ids), options composed here
  (`deribit:`/`okx:`/`binance-opt:` + name). Written by the bin on
  EVERY boot (fatal on write failure, the capture-is-the-product
  stance); `options-manifest.tsv` unchanged, kept ONE release for
  pre-D3 readers (wire-format.md updated with the new section).
- `claude_worker.iv_digest` follows: `read_manifest` PREFERS the
   2-col instrument manifest (venue = the SymbolId namespace byte),
  falls back to the legacy options file; skip line names both files.
  (Both touched files are THIS lane's own M2-close additions —
  the M3 additive-files law holds: no pre-existing worker file
  touched.) +2 pytest (prefer/strict + legacy-fallback lanes).

### `audit-pnl` — the §4-M4 analyzer (`crates/cli/src/audit_pnl.rs` + bin arm)

- **Fill law REUSED, never twinned**: one
  `backtest::fill::FillEngine` per attribution key with
  `boundary_virt_ns = 0` — the OOS book IS the whole-window book, so
  `oos_*` outcomes are the window stats. Same `ModelParams` knobs as
  the backtest (`--fee-bps` / `--latency-ns` / `--latency-ns-venue`
  via the SAME `parse_model_params`). Three additive promotions, all
  behavior-free: `discover_runs`/`RunDir` → `pub(crate)` (one
  discovery law; the catalog precedent) and
  `FillEngine::oos_equity_1e12()` (per-day snapshots).
- **Merge law inherited verbatim** (§3.2/§3.3): per-run total order
  extended over event classes — at equal ts, TICKS sort before
  ORDERS (the engine's "fill pass precedes emit" law) and
  RulesetCommits before orders (8g §6 in-stream flip); VIRT_T0
  rebase; the run anchor = min tick ts; orders/fills/ai-cmds share
  the engine clock and rebase with the SAME anchor; wall-overlapping
  runs refused. DELIBERATE divergence from the harness, documented
  in-module: a tick-less run (aborted boot) is SKIPPED with a report
  line, not fatal — audit tooling reports, only the harness gates.
- **§6 keying — the reshuffle wall**: every sym is rewritten to a
  root-scoped DENSE id through its run's manifest descriptor
  (instrument manifest → options fallback); a manifest-less run
  resolves into a PER-RUN namespace (`run-<epoch>/sym-0x…`) so its
  instruments can NEVER silently merge across boots. Dense ids
  preserve the venue byte (the model's Δ/fee lookup untouched).
  Books, marks, and per-sym report rows all live in descriptor
  space.
- **Attribution**: per `Order.strategy_id` row (labels =
  strategy-set slots; 255 = unattributed); vm intents additionally
  bucket per ruleset hash ACTIVE at emit (ai-cmds `RulesetCommit`
  timeline via `AiCmd::ruleset_hash128` — the shared helper; per-hash
  books are INDEPENDENT replays beside the slot-5 aggregate;
  pre-commit vm intents counted `vm_orders_no_hash`).
- **Two views, one report**: modeled rows beside the paper view
  (`engine-fills` folded to signed cash + mark-out at last mids, no
  fees in paper — honestly zero today). Per-UTC-day net buckets =
  day-boundary equity snapshots (§4-M4's per-day requirement).
  Output: one deterministic hand-rendered JSON line on stdout
  (`audit_pnl_version` 1, `fmt_usd_1e6` rendering) + human summary
  incl. per-descriptor rows on stderr; stderr-pinned tracing; exit
  0 iff a report was produced.
- **Tests: +8** — 1 manifest-render unit (all lanes, final
  descriptors) and 7 integration goldens via the real `PmlrWriter`
  (hand-computed strict-cross fill + markout through a nonzero
  anchor and the PM Δ; vm hash timeline pre/post-commit; THE
  reshuffle test — same descriptor on different ordinals across two
  runs stays ONE continuous book while a re-used ordinal lands in
  its new descriptor; manifest-less namespace arm; paper fold;
  byte-identical rerun; tick-less-root refusal). All passed FIRST
  RUN.

### Gates (all on the Mac)

- workspace nextest **1240/1240** (+1 skipped; 1 leaky =
  informational on a pre-existing real-binary harness test) = 1232 +
  8. New stay-green floor **1240**.
- release alloc **38/38** 0 B/op (corrected clean, fresh `Compiling
  bench` ×1, `--test-threads=1`).
- worker pytest **431/431** (429 + 2 manifest lanes; serialized,
  off top-of-hour). New stay-green **431**.
- fuzz: NOTHING owed — the new Rust parsers read OWN capture files +
  own manifests (engine-authored trust class, same as every PMLR
  read; no venue wire bytes).

### Live proof (real root, real intents, G0-relinked binary)

`audit-pnl --dir ~/multivenue/logs` over the WHOLE standing root —
**35 runs, 10 UTC days, exit 0, 558-byte JSON**:

- **strategy 0 (latency-arb): 217 accepted intents → 203 modeled
  fills/trades over 2 trading days; net −$32.55 (all markout —
  positions open at window end), max DD $165.03; per-day day8
  +$47.70 / day9 −$80.25.** Paper view fills=0 net=$0 — the audited
  paper-mode reality, printed beside the modeled view exactly as
  §4-M4 demands.
- **REAL FINDING the tool exists to surface: `caps_rejected=211,840`**
  — the standing latency-arb re-emits while its 4-per-sym/32-total
  §4.1 modeled slots are full (~212k intents logged since the M4.1
  binary went live; the model's conservative wall takes 217). The
  strategy's emit cadence vs open-order lifecycle is now VISIBLE and
  quantified — M5 digest material.
- vm rows empty + `vm_orders_no_hash=0` — correct: every vm-era run
  predates the M4.1 intent log.
- Manifest-less pre-D3 runs reported + per-run-namespaced (per-run
  lines on stderr); no cross-run mixing possible.
- Ops note: today's M4.1/M4.2 smoke windows (runbook dance) cost
  ~30–40 s of dark time — well inside M3's 300 s/day gap tolerance;
  2026-08-23 REMAINS a C6 candidate day.

### Slice checkpoint — COMMIT ASK (pending operator authorization)

`M4:`-prefixed, EXPLICIT paths only:

- `crates/cli/src/audit_pnl.rs` (new)
- `crates/cli/tests/audit_pnl.rs` (new)
- `crates/cli/src/options_manifest.rs`
- `crates/cli/src/backtest.rs` (pub(crate) promotions)
- `crates/cli/src/backtest/fill.rs` (equity accessor)
- `crates/cli/src/lib.rs`
- `crates/cli/src/bin/multivenue-engine.rs`
- `claude-worker/src/claude_worker/iv_digest.py`
- `claude-worker/tests/test_iv_digest.py`
- `docs/wire-format.md`
- `docs/m4-progress.md`

NOT staged: `docs/m3-progress.md` (M3's WIP + coordination note);
`.env`; `~/multivenue/*`.

**Resume point if context dies here:** M4.2 code-complete +
gates-green (1240/38/431) + live-proven (35-run root, real modeled
P&L, the caps finding); commit ask pending; REMAINING M4 = M4.3
(the D1-unfrozen additive `pnl` verb + `claude_worker.pnl_report`
module + nightly-at-C6 coordination; design + rulings pinned in the
entries above).

(M4.2 COMMITTED `c107157`, operator-authorized, 11 explicit paths.)

---

## 2026-08-23 — M4.3 IMPLEMENTED: `pnl_report` module + the D1 `pnl` verb; M4 CLOSES (commit ask below)

### `claude_worker.pnl_report` (NEW module — the report writer)

`python -m claude_worker.pnl_report`: spawns
`multivenue-engine audit-pnl --dir <replay>` by PATH-resolved name —
`backtest.ENGINE_BINARY` REUSED, the same pinned §14 spawn contract,
runner injectable for tests — validates the stdout
(`audit_pnl_version` 1, JSON, non-empty; anything else = LOUD nonzero
— a silently skipped nightly would read as a flat day), and writes
the day pair under `CLAUDE_WORKER_REPORTS_DIR`
(default `~/multivenue/worker/reports`; documented in `.env.example`,
`.env` untouched): `pnl-<YYYY-MM-DD>.json` (verbatim stdout) +
`pnl-<YYYY-MM-DD>.summary.txt` (verbatim stderr summary). Idempotent
per UTC day (a re-run refreshes — derived cache; PMLR stays the
truth). Serialized like every worker invocation. **D2 stands: the
nightly launchd timer is M3's C6+ window; manual invocation until
then** (this entry is the runbook line — `docs/local-setup.md` gains
it at the C6 window, M3-owned).

### The `pnl` verb (cli.py — the D1-unfrozen ONE additive verb)

THIN by construction: finds the newest (or `--date`) report pair and
prints the summary (`--json` for the raw report); no socket, no
engine spawn, no BaseConfig read; exit 2 with a
run-the-module hint when nothing exists. cli.py changes: the module
import + the command + the docstring surface note — nothing else.

### The frozen-pin conflict — fired and resolved EXACTLY per D1

`test_cli.py::test_verb_surface_is_exactly_section_6` pins the verb
set to the §6 seven — the D1 stop-condition. STOPPED, put to the
operator; **operator ruling: amend the pin to 8** (the one sanctioned
edit to a frozen test, comment in-test citing this ruling + date).
The pin keeps guarding: an unsanctioned 9th verb still fails. The
no-override-flag §11 test passes untouched (`pnl` carries no
override-shaped option). Every other frozen test byte-untouched.

### Tests (+8, all first-run green)

`tests/test_pnl_report.py`: module — argv/spawn-contract pin, report
pair written, per-day idempotence+refresh, loud failure on nonzero /
empty / non-JSON / wrong-version stdout; verb — latest-with-JSON-
fallback, dated summary + `--json`, missing ⇒ exit 2; and a
PATH-gated REAL-BINARY end-to-end (crafted run: ticks + hand-packed
Order slots + manifest → module spawns the real `audit-pnl` → report
pair verified, fills=1) riding the `test_backtest_real` skip law.

### Gates

- worker pytest **439/439** (431 + 8; incl. the amended pin). New
  stay-green **439**.
- Rust BYTE-UNTOUCHED in M4.3 ⇒ nextest **1240** / alloc **38** stand
  (same tree as the M4.2 gate run). fuzz: nothing owed.

### Live proof (real root, serialized, off top-of-hour)

- `python -m claude_worker.pnl_report` against the standing root:
  exit 0 — `pnl-2026-08-23.json` + summary written
  (strategies=1 runs=35; the audit-pnl numbers had GROWN since the
  M4.2 proof — orders 217→324, fills 203→317 — the standing lane
  logs intents continuously; the report is the living daily
  snapshot, exactly the design).
- `claude-worker pnl` prints the summary (per-descriptor rows
  visible); `claude-worker pnl --json` prints the schema-1 line.
- Recorded expectation: today's intent-bearing runs predate the
  instrument manifest (M4.1-era binary) ⇒ their rows show the
  per-run-namespace form (`run-…/sym-0x…`) — the §6-correct honest
  arm. Descriptor continuity begins at the standing lane's NEXT
  00:00Z restart (first boot of the M4.2+ binary writes
  `instrument-manifest.tsv` every boot). No action needed.

### M4 CLOSED — exit vs mvp-plan §4-M4

- per-strategy attribution on capture: **BUILT + verified**
  (M4.1 migration; wire-format + migration.md entries) ✓
- `audit-pnl`: one command → per-strategy / per-ruleset-hash modeled
  P&L over any window, beside the paper view, JSON + human,
  deterministic ✓ (live-proven twice)
- worker surface + daily report: `pnl_report` module + `pnl` verb ✓
- "nightly report lands automatically (launchd timer)": **DEFERRED
  to M3's C6+ window by operator ruling D2** — recorded as the one
  open exit line, owned by the C6 procedure.
- Estimates: plan said 3–4 d; landed in one session (M4.1+M4.2+M4.3)
  on the back of the M2-close manifest work.

### COMMIT ASK (pending operator authorization)

`M4:`-prefixed, EXPLICIT paths only:

- `claude-worker/src/claude_worker/pnl_report.py` (new)
- `claude-worker/tests/test_pnl_report.py` (new)
- `claude-worker/src/claude_worker/cli.py` (the D1 additive verb)
- `claude-worker/tests/test_cli.py` (the ONE sanctioned pin
  amendment)
- `.env.example`
- `CLAUDE.md` (CURRENT STATE → M4 CLOSED + baselines 1240/38/439;
  M5 next on operator go)
- `docs/m4-progress.md`

NOT staged: `docs/m3-progress.md`; `.env`; `~/multivenue/*`.

**Resume point if context dies here:** M4 is functionally COMPLETE
and live-proven end-to-end; only this commit is pending. After it:
M5 on explicit operator go (mvp-plan §4-M5 — full-universe
digest inputs incl. the iv_digest table, one semi-manual promotion
on REAL multi-day capture post-C6, the manual walk-forward runbook);
the C6+ window owes M3 the nightly pnl timer + the digest cadence
hookups (coordination notes in docs/m3-progress.md).
