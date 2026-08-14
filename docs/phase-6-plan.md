# Phase 6 — Strategy C + Strategy D

Status: **complete** (2026-05-19). All §6 deliverables landed.
Final tally: **400 tests across 35 binaries**, 22 alloc
assertions (21 @ 0 B/op + 1 budgeted signer), 10 fuzz targets,
clippy clean. CLI flag `--strategy {latency-arb|ev|cross-arb|rule-tree}`
all live.

This plan wires the remaining two strategies from the PLAN.md
roadmap:

* **Strategy C — Cross-market arbitrage.** When a set of
  mutually-exclusive Polymarket binaries (a "group") sums to a
  probability that deviates from 1.0 by more than a threshold,
  emit N orders (one per market in the group) that close the gap
  and lock in the implied edge.
* **Strategy D — Rule-tree exploitation.** Consume the
  `RulesTable` artifacts emitted by `claude-worker/rule_parser.py`,
  match incoming `Signal` payloads against per-rule triggers, and
  emit a position when a high-confidence rule fires and the market
  hasn't yet repriced.

Both strategies share the existing infrastructure: `Engine<S, D>`,
`MultiBook`, `ArtifactTable`, `Order` / `Signal` rings. v1 ships
them as alternatives — picked via `--strategy {cross-arb|rule-tree}`
— rather than as composable layers; multi-strategy mux lands in
Phase 7 if any of them prove out.

## Scope

* `crates/strategy-cross-arb` — **new** crate.
  * `MarketGroup<const M: usize>` — a fixed-size list of
    `SymbolId`s that partition a single event (e.g. all 7
    candidates for an election).
  * `CrossArb<const N: usize, const M: usize>` — state =
    `[MarketGroup<M>; N]` + per-group cooldown + threshold +
    `MultiBook` for the union of tracked symbols.
  * Decision rule (see §6.1).
* `crates/strategy-rule-tree` — **new** crate.
  * Consumes `research_artifacts::RulesTable<N>`.
  * Per-rule trigger matcher (`Signal.payload` fast-scan against
    per-rule keyword arrays).
  * Per-market firing record + cooldown.
* `crates/cli` — extend `--strategy` to accept `cross-arb` and
  `rule-tree`. Each gets its own `engine_loop_*_full` entry
  point.
* `crates/bench/tests/alloc_assertions.rs` — assertions for both
  strategies' hot paths.

Explicitly out of scope:
* Multi-strategy mux (running latency-arb + ev + cross-arb
  simultaneously). Phase 7.
* Real fill ingestion. Phase 7 alongside Polymarket WS `order`
  channel.
* Automatic group construction from `RulesTable.family`. v1 ships
  with statically-configured groups via CLI args.
* claude-worker artifact streaming. v1 reads the same NDJSON at
  boot.

## Non-negotiables (carry over)

* **Zero alloc in steady state** for both new strategies.
* **No `dyn Trait` in hot paths** — both strategies stay generic.
* **Strategy C never holds a partial fill.** v1 emits orders
  for every leg in the group at once or not at all; partial fill
  handling is Phase 7.
* **Strategy D gates on rule confidence.** Low-confidence rules
  (`edge_bps < 10`) never fire; v1 hard-codes that floor.
* **Every public fn has rustdoc; every parser has a unit test.**

## Deliverables

### 6.1 `strategy-cross-arb`

```rust
pub struct MarketGroup<const M: usize> {
    /// Symbols making up this partition. `SYMBOL_ID_NONE` marks
    /// unused slots.
    pub members: [SymbolId; M],
    /// Number of populated slots.
    pub count: u32,
}

pub struct CrossArb<const N: usize, const M: usize> {
    groups: [MarketGroup<M>; N],
    book: MultiBook<{ N * M }>,
    threshold_1e6: i64,       // |sum - 1_000_000| trigger
    qty: Qty,
    cooldown_ns: u64,
    last_emit_ns: [u64; N],
    next_oid: u64,
    // counters
    pub pm_ticks_seen: u64,
    pub orders_emitted: u64,
    pub orders_dropped: u64,
}
```

Decision rule:
```text
on Polymarket tick T (sym = ps):
  book.apply(T)
  find group g containing ps (linear scan, ≤ N groups)
  if !all_members_have_quotes(g): return       // wait for full state
  sum_p_1e6 = Σ book.snapshot(member).mid()
  delta = sum_p_1e6 - 1_000_000
  if abs(delta) < threshold_1e6: return
  if (now - last_emit_ns[g]) < cooldown_ns: return
  // delta > 0: group is overpriced (Σp > 1) → sell each member
  // delta < 0: group is underpriced (Σp < 1) → buy each member
  for member in g:
    let side = delta > 0 ? Ask : Bid
    ctx.submit(Order { sym=member, side, px=book.snapshot(member).mid(), qty=self.qty/M })
  last_emit_ns[g] = now
  orders_emitted += M
```

Constraint:
- `M ≤ 8` (linear scan + 8-element ladder; bigger groups have
  fewer arb opportunities and need book-builder work first).
- `N ≤ 16` (the cli can register 16 groups).

Boot-time helpers:
```rust
pub fn register_group(&mut self, members: &[SymbolId]) -> Result<GroupId, GroupErr>;
```

### 6.2 `strategy-rule-tree`

```rust
pub struct RuleTree<const N: usize> {
    rules: RulesTable<N>,
    book:  MultiBook<N>,
    /// Per-rule symbol mapping — which Polymarket market does
    /// this rule trade?
    rule_to_sym: [SymbolId; N],
    /// Per-rule edge requirement (basis points). Cached from the
    /// loaded rule.
    rule_edge_bps: [u32; N],
    /// Per-rule cooldown deadlines.
    last_emit_ns: [u64; N],
    /// Per-rule trigger keywords for fast Signal payload match.
    /// First 16 ASCII bytes of the keyword; arbitrary-length match
    /// promoted to "Signal.payload contains this prefix anywhere".
    rule_kw: [[u8; 16]; N],
    /// Min basis-point edge a rule must claim to be eligible.
    floor_edge_bps: u32,
    qty: Qty,
    cooldown_ns: u64,
    next_oid: u64,
    // counters
    pub signals_seen: u64,
    pub orders_emitted: u64,
    pub orders_dropped: u64,
}
```

Decision rule:
```text
on Signal s:
  for rule i in rules:
    if rule.edge_bps < floor: continue
    if !payload_contains(s.payload, rule_kw[i]): continue
    let sym = rule_to_sym[i]
    let mid = book.snapshot(sym).mid()  // or return if no quotes
    // The rule predicts a price *gap* of edge_bps; v1 fires a buy
    // if mid < 0.5 (the rule expects the binary to resolve YES)
    // or a sell if mid > 0.5.
    let side = if mid < 500_000 { Bid } else { Ask }
    if (now - last_emit_ns[i]) < cooldown: continue
    ctx.submit(Order { sym, side, px=mid, qty=self.qty })
    last_emit_ns[i] = now
```

Boot-time helpers:
```rust
pub fn add_rule(
    &mut self,
    rule: Rule,
    sym: SymbolId,
    keyword: &[u8],
) -> Result<(), RuleAddErr>;
```

### 6.3 `cli` integration

* `--strategy cross-arb`:
  * Requires `--groups <path>` pointing at a TOML/CSV file
    listing `(group_name, [sym1, sym2, …])` rows.
  * Optional `--threshold-1e6 N` (default 20_000 = 2 cents
    deviation from sum=1).
* `--strategy rule-tree`:
  * Requires `--artifacts-path <NDJSON>` (tags) and
    `--rules-path <JSON>` (rules from `rule_parser.py`).
  * Optional `--floor-edge-bps N` (default 10).

### 6.4 Test surface

* `strategy-cross-arb`:
  - Group register + duplicate rejection.
  - Sum at 1 exactly → no fire.
  - Sum > 1 + threshold → N Asks emitted.
  - Sum < 1 - threshold → N Bids emitted.
  - Partial book (one member missing quotes) → no fire.
  - Cooldown suppresses.
* `strategy-rule-tree`:
  - `add_rule` rejects oversized keyword.
  - Rule below floor_edge_bps → no fire.
  - Matching signal + cheap market → Bid.
  - Matching signal + rich market → Ask.
  - Cooldown suppresses.
  - Non-matching keyword → no-op.
* `bench/tests/alloc_assertions.rs` — 2 new assertions:
  - `cross_arb_on_tick_is_zero_alloc`
  - `rule_tree_on_signal_is_zero_alloc`
  Bringing total to **22 / 22**.

## Acceptance checklist

- [x] `cargo check --workspace --all-targets` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace --release --exclude bench` — **400 tests across 35 binaries** ✔
- [x] `cargo test -p bench --test alloc_assertions --release --
      --test-threads=1` — **22 / 22** (21 zero-alloc + 1 budgeted signer)
- [x] `cargo check --manifest-path fuzz/Cargo.toml` green
- [x] `cargo run --release -p cli -- run --paper --strategy
      cross-arb --groups "10,11,12"` boots (parses ";"-delimited
      groups of comma-separated SymbolIds).
- [x] `cargo run --release -p cli -- run --paper --strategy
      rule-tree --rules-path <JSON>` boots (loads via
      `RulesTable::load_json`).
- [x] `docs/phase-6-plan.md` flipped to **complete**
- [x] Memory file refreshed

## Sequencing

1. **`strategy-cross-arb`** — new crate, group table + sum-
   detection. ~400 LOC + tests.
2. **`strategy-rule-tree`** — new crate, RulesTable consumer +
   keyword matcher. ~400 LOC + tests.
3. **`cli` flag wiring** — extend `--strategy` to four values,
   add `--groups` / `--rules-path` flags. ~250 LOC change.
4. **Alloc assertions + sweep + docs + memory.**

## Risks / open questions

* **Group symbol registration.** v1 reads a static fixture file
  for the symbol set. Wrong symbol → no edge. Mitigation: log
  the registered groups at boot for operator review.
* **Rule keyword matching is naive.** v1 does plain substring
  match on the first 16 ASCII bytes of each rule's trigger. Phase
  7 may upgrade to a fixed-size aho-corasick over the union of
  keywords if false positives become a problem.
* **Cross-arb assumes equal-sized legs.** Real fills will be
  partial, leaving the operator with directional exposure. Phase
  7 wires the fill ring + position tracker to correct
  imbalances.
