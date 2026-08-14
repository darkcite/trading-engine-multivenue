# Phase 5 — claude-worker polish + Strategy A groundwork

Status: **complete** (2026-05-19). All §5 deliverables landed.
Final tally: **375 tests across 33 binaries**, 20 alloc
assertions (19 @ 0 B/op + 1 budgeted signer), 10 fuzz targets,
clippy clean. CLI flag `--strategy {latency-arb|ev}` wired;
EvStrategy loads claude-worker NDJSON artifacts at boot and
trades model-vs-market mispricing.

This plan wires the Python `claude-worker` artifact pipeline to a
real Rust `Strategy A`: pricing-model-vs-market mispricing detection
against Polymarket binary outcomes. The latency-arb path stays
unchanged; Strategy A becomes a peer the cli can pick.

## Current state

* `claude-worker` Python — already shipped:
  * `anthropic_client.py` — SDK wrapper with `CompletionRequest`.
  * `topic_tagger.py` — Haiku-based market tagger; NDJSON output.
  * `rule_parser.py` — Sonnet-based rule extractor; JSON array
    output.
  * `cli.py` — `claude-worker` console script.
  * **21 / 21 tests pass** under Python 3.12.
* `strategy-ev` Rust — stub only. Counts callbacks, trades nothing.

## Scope

* `crates/research-artifacts` — **new** crate. Loads claude-worker
  NDJSON tags + JSON rules at boot into fixed-size POD tables.
  Zero-alloc query path; allocation only during the boot read.
* `crates/strategy-ev` — promote `EvStrategy` to a real
  Strategy A:
  * Per-tracked-symbol probability snapshot: model-driven `p_true`
    in `[0, 1_000_000]` 1e6 fixed-point.
  * Trigger: emit `Order` when `|market_mid - p_true|` exceeds the
    configured `threshold_1e6`, side flips on sign, cooldown
    enforced per market.
  * Probabilities seeded at boot from the artifacts table; v1
    static. v2 will let claude-worker stream updates.
* `crates/cli` — `--strategy {latency-arb|ev}` flag selects which
  strategy `engine_loop_full` instantiates. `--artifacts-dir
  <path>` points at the claude-worker output directory.
* `crates/bench/tests/alloc_assertions.rs` — assertion for the
  EvStrategy on_tick path.

Explicitly out of scope (Phase 6+):
* Live claude-worker → engine streaming (a future
  `--reload-artifacts` flag).
* Multi-strategy mux (running latency-arb and EV simultaneously).
  v1 picks one at boot.
* Reactive probability updates from RSS / news signals. EV's
  probability is static within a session.
* Real Bayesian inference. Phase 5 only loads the model-output
  probability directly.

## Non-negotiables (carry over)

* **Zero alloc in steady state.** Artifact tables are
  preallocated; queries are linear scans (≤ 64 entries).
* **No `serde_json` on the artifact load path** — a thin
  hand-rolled NDJSON scanner suffices.
* **Strategy A monomorphises through `Engine<S>`** — same shape
  as `LatencyArb<N>`.
* **Every public fn carries rustdoc.**
* **Alloc assertion + at least one unit test per public fn.**

## Deliverables

### 5.1 `research-artifacts`

```rust
pub struct ArtifactTable<const N: usize> {
    /// `symbol_key` is a 64-byte ASCII id (Polymarket asset_id
    /// prefix). Sentinel `[0; 64]` means slot is free.
    keys:        [[u8; 64]; N],
    /// `model_p_1e6` is `0..=1_000_000` (probability fixed-point).
    model_p_1e6: [u32; N],
    /// `family` tag — small enum from the topic tagger.
    family:      [u8; N],   // 0=crypto, 1=politics, 2=sports, 3=macro, 4=other
    /// `impact` tag — low/med/high.
    impact:      [u8; N],   // 0=low, 1=med, 2=high
    count:       u32,
}

impl<const N: usize> ArtifactTable<N> {
    pub fn empty() -> Self;
    pub fn insert(&mut self, key: &[u8], p_1e6: u32, family: u8, impact: u8)
        -> Result<(), ArtifactErr>;
    pub fn lookup(&self, key: &[u8]) -> Option<(u32, u8, u8)>;
    pub fn load_ndjson(path: &Path) -> io::Result<Self>;
    pub fn load_rules(path: &Path) -> io::Result<RulesTable<N>>;
    pub fn len(&self) -> usize;
}
```

The NDJSON shape is what `topic_tagger.write_artifact` already
emits:

```json
{"id":"0xabc","family":"crypto","impact":"high","reason":"..."}
{"id":"0xdef","family":"politics","impact":"low","reason":"..."}
```

A separate `RulesTable` mirrors `rule_parser.write_artifact`'s
JSON-array shape.

### 5.2 `strategy-ev::EvStrategy<const N: usize>`

State:
```rust
pub struct EvStrategy<const N: usize> {
    table:    ArtifactTable<N>,
    book:     MultiBook<N>,                 // Polymarket TOBs
    threshold_1e6: i64,                     // |mid - p_true| trigger
    qty:      Qty,
    cooldown_ns: u64,
    last_emit_ns: [u64; N],
    next_oid: u64,

    // counters (read by paper/UI)
    pub pm_ticks_seen: u64,
    pub orders_emitted: u64,
    pub orders_dropped: u64,
}
```

Decision rule:
```text
on Polymarket tick T (sym ps):
  if !table.lookup(asset_id_for(ps)).is_some(): return
  book.apply(T)
  let mid_1e6 = book.snapshot(ps).mid()    // 0..1_000_000
  let p_1e6   = table.lookup(...)?         // 0..1_000_000
  delta = mid_1e6 - p_1e6
  if delta.abs() < threshold: return
  if now - last_emit_ns[ps] < cooldown: return
  side = delta > 0 ? Ask /* sell rich */ : Bid /* buy cheap */
  ctx.submit(...)
```

Same hot-path shape as `LatencyArb`. `Signal` callbacks are
counter-only (no behaviour change in v1).

### 5.3 `cli` integration

* `--strategy <name>` — defaults to `latency-arb`. Other valid
  values: `ev`.
* `--artifacts-dir <path>` — defaults to `<log_dir>/artifacts`.
  Strategy A loads `tags.ndjson` from there; absent → fail-fast.
* Boot-time logging records which strategy + how many artifact
  rows were loaded.

### 5.4 Test surface

* `research-artifacts`:
  - NDJSON parser: happy-path + malformed-line + duplicate-key.
  - JSON-array rules parser.
  - `lookup` zero-alloc property test.
* `strategy-ev`:
  - on_start fails without artifacts loaded.
  - Trigger on rich market → Ask.
  - Trigger on cheap market → Bid.
  - Cooldown suppresses.
  - Unknown symbol dropped silently.
* `bench`: new assertion `ev_strategy_on_tick_is_zero_alloc`.

## Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` — **375 tests across 33 binaries** ✔
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` — **20 / 20** (19 zero-alloc + 1 budgeted signer)
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
- [x] Python `claude-worker` test suite — **21 / 21** under
      Python 3.12 via uv.
- [x] `cargo run --release -p cli -- run --paper --strategy ev
      --artifacts-path <NDJSON>` boots cleanly (manual smoke
      pending live artifacts).
- [x] `docs/phase-5-plan.md` flipped to **complete**
- [x] Memory file refreshed

## Sequencing

1. **`research-artifacts`** — new crate, NDJSON scanner +
   `ArtifactTable`. ~400 LOC.
2. **`strategy-ev`** — real Strategy A built on top.
   ~350 LOC + ~150 LOC tests.
3. **`cli` flag wiring** — `--strategy` mutually exclusive
   between latency-arb / ev. ~80 LOC.
4. **Alloc assertion + sweep + docs + memory.**
