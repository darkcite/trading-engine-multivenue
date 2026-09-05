# Migration notes

This document tracks **format and schema migrations** — places where a bump
to a wire-format version, an on-disk file layout, or a config key has
ripple effects the operator needs to know about.

Each entry is atomic: one version bump per section. Do not batch.

## Template

```
## <YYYY-MM-DD> — <short headline>

**What changed**
- ...

**Why**
- ...

**Impact**
- On-disk formats: ...
- Config keys: ...
- Wire formats: ...

**Migration steps**
1. ...
2. ...

**Rollback**
- ...
```

## 2026-09-05 — The harness funding seed: `funding-seed.tsv` per window, `backtest --funding-seed` (RG4 carry blocker)

**What changed**

- `backtest` gains `--funding-seed <path>` (`cli::backtest::funding`):
  a `descriptor \t ts_ms \t rate_1e9` file (`#` comments; malformed =
  fatal) becomes synthesized `AiCmdKind::FundingSeed` commands applied
  through the vm's live `on_ai` path — dedup law included — BEFORE the
  first replayed record. Default = the first run directory's own
  `funding-seed.tsv` when it exists, else none. With a seed applied the
  V5 warm-up law drops the `apr24`/`apr72` 24 h / 72 h requirement
  (the seed IS the prints' history; a seed shorter than a feature's
  window leaves that feature ABSENT by the feature law). Summary gains
  `funding: seed_prints=… dropped=… deduped=… warmup=seeded|table`
  plus a `funding: seed <path> …` build tell. `audit-pnl` is untouched
  (it audits what happened; no vm warm-up is involved).
- Worker: `window_root.cut_run` writes the window's `funding-seed.tsv`
  whenever `seed=(…, candles.db)` names a store with a funding table —
  the window's manifest × the funding table under the boot seed lane's
  own law (`claude_worker.seeds.funding_seed_rows`: 73 h before the
  window's first instant, exclusive; newest 640 per sym; rate ×1e9
  RAW). `pool_ensure` back-fills the file on reused cuts (no re-cut).
  Neither file is in git (a window root is research data).

**Why**

- Under the ≤ 2 h law a funding-carry member (`apr24`/`apr72`) could
  never be evidenced — the table-global 24 h warm-up swallowed every
  2 h window (0 orders). Live, the same row is warm from its first
  minute because the boot seed lane pushes 73 h of prints; the replay
  now has the same warm-up, so the library / composer can evidence
  carry members on the standing pool (RG4's "≥ 2 members" exit tell).

**Impact**

- On-disk formats: a new optional per-window file `funding-seed.tsv`
  beside the manifests. Schema-1 stdout unchanged; `--emit-detail`
  unchanged (`detail_version` 4).
- Config keys: none. Wire formats: none (kind 10 frames, unchanged).
- Behaviour: NONE without a seed file (`warmup=table` = the V5 law);
  a root whose first run carries `funding-seed.tsv` replays seeded —
  the frozen worker argv picks it up by default, which is the point.

**Migration steps**

1. `cargo build --release -p cli` (pitfall 18 — the harness is the
   release binary).
2. Existing pool windows gain the file on the next `pool_ensure`
   (composer run) with `candles.db`; single windows: cut again with
   `seed=(regime.toml, candles.db)`, or pass `--funding-seed`.

**Rollback**

- Delete the per-window files (or pass `--funding-seed /dev/null` —
  an empty seed = `warmup=table`).

## 2026-09-05 — Ruleset grammar v2.1 regime keys, the vm row gate, the regime-aware harness (RG3)

**What changed**

- Ruleset JSON grammar v2.1: three optional v2 row keys — `regimes`
  (string array, the §3.3 label grammar with `fast:`/`slow:` prefixes
  and `rel:` terms), `regime_off` (`soft`|`hard`), `rel` (sugar for one
  `rel:` term) — parsed by the ingress-ai byte scanner into the
  `RuleRowV2` regime tail RG0 reserved (`regime_fast`/`regime_slow`/
  `regime_off`/`regime_rel`). Validator **rule 11** (`RulesetReject::Regime`)
  and the **rule-8 amendment** (identity-tuple duplicates only when the
  regime regions intersect — disjoint variants of one signal admit).
  `RegimeLabel::LABELLED_ANY` (core-types) = the fill of a REL-only
  profile. Rows without the keys are bit-identical (tail zero) — every
  existing artifact hashes and validates unchanged.
- `strategy-vm`: per-row gate byte `row_gate` judged on every
  `set_regime_view` (the new set→vm seam: `core_regime::RegimeView`,
  `RegimeState::view()`, pushed by `StrategySet` on configure / seed /
  every minute roll / effective change / declaration) and on every
  flip; the entry path skips closed rows (`regime_blocked`), the exit
  path flattens HARD-closed position rows at once (`regime_hard_exits`;
  age-out first, min-hold bypassed); soft-closed rows drain by their
  own law. `StrategyCounters::vm_regime_{blocked,hard_exits}`,
  `/metrics` `engine_vm_regime_{blocked,hard_exits}_total`.
- Harness: `backtest` / `audit-pnl` gain `--regime <path>|off` and
  `--regime-seed <path>` (`cli::backtest::regime`): the engine's own
  `RegimeState` replays over the window's ticks, funding prints and the
  `SetRegime` frames of `ai-cmds.pmlr` (pre-anchor frames clamped with
  their TTL shortened; expired ones dropped + counted); the vm receives
  the view exactly as live; `off` strips every tail (the on/off delta);
  absent flag = the default artifact when it exists and resolves on the
  root (members absent from the root are DROPPED, refs must resolve),
  else regime-blind with a stderr tell — the frozen worker argv can
  never fail on a default artifact. `--emit-detail` is `detail_version`
  4 (additive `regime` block); `audit-pnl` JSON gains the additive
  `regime` section (per profile: minutes per effective word, per
  `(word, strategy)` fill-model replays with the fee ladder;
  `audit_pnl_version` stays 1). `cli::regime_boot` is the ONE resolver
  the engine boot and the harness share.
- Worker: `window_root.cut_run` carries the pre-window `SetRegime`
  declaration still in force at the cut (latest frame per profile
  decides) in front of the `ai-cmds.pmlr` slice, and writes the window's
  own `regime-seed.tsv` from `candles.db` when `seed=(regime.toml,
  candles.db)` is given (`pnl_report` day mode passes the defaults);
  `strategist` prompt **v3** (`STRATEGIST_PROMPT_VERSION =
  "strategist-v3"`) teaches the keys, the gate law and regime variants
  and asks for `regimes` on every row; `parse_proposal` accepts the keys
  structurally (`regime_term_ok` / `regime_rel_ok` vocabulary mirrors).
- Bench alloc gate 42 (`vm_regime_gate_and_view_rejudge_are_zero_alloc`).

**Why**

- `docs/regime-and-dashboard-plan.md` RG3: rows gate themselves on the
  regime (D2 — no table flip on a regime change), and every backtest /
  nightly report shows the per-regime P&L and the on/off delta.

**Impact**

- On-disk formats: none new. `--emit-detail` sidecar `detail_version`
  3 → 4 (additive). `audit-pnl` stdout: additive `regime` key.
  Windowed roots may now carry `regime-seed.tsv` and a carried
  pre-window `SetRegime` frame in `ai-cmds.pmlr` (ts before the first
  tick — the harness clamps it).
- Config keys: none. Ruleset artifacts: the three optional row keys.
- Wire formats: none (the tail bytes were reserved at RG0; kind 12
  unchanged).
- Behaviour: NONE for every existing artifact (tail zero ⇒ open under
  every view). A LABELLED row fails closed until the engine's detector
  knows the regime (`~/multivenue/regime.toml` + seed) — and in the
  harness until `--regime` (or the default artifact) resolves.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before any backtest /
   audit-pnl that matters or the next boot.
2. Nothing else: labelled rows only exist once a ruleset carries the
   keys.

**Rollback**

- Revert the commit. Labelled artifacts then reject as rule 2
  (unknown key) at stage — nothing else changes.

## 2026-09-03 — Regime detector in the engine: `regime.toml`, `regime-seed.tsv`, `--regime`, the `engine_regime_*` family (RG1–RG2)

**What changed**

- New crate `core-regime` (the measured regime: minute-close rings,
  the integer judge law, confirm hysteresis, the declared-over-measured
  effective law) and `core-config::regime` (the `regime.toml` parser +
  the seed reader). `ret_bps_1e9` / `isqrt_i128` moved to
  `core_regime::math` (`strategy-icdp` re-exports; `strategy-vm` imports).
- `Strategy` trait: `regime_label` / `set_regime_label` / `on_regime`
  (all defaulted — no existing strategy changes behaviour);
  `StrategyCounters::regime_counters`; `IcdpCounters` gains
  `regime_blocked` / `regime_exits` (additive).
- `StrategySet` owns the detector: a 1 s timer is armed when a detector
  is configured (`REGIME_TIMER_NS`; members' timers were `u64::MAX`);
  `AiCmdKind::SetRegime` is consumed at set level; labelled members
  receive edge-triggered `on_regime` calls. The ai-exec refuses intents
  while its gate is closed; the icdp blocks decisions while closed and
  exits open positions on a HARD close.
- Boot surface: `--regime <path>` (default `~/multivenue/regime.toml`;
  ABSENT default = detector unconfigured, today's behaviour; explicit
  path or present-but-invalid file = boot refused), `--regime-seed
  <path>` (default `~/multivenue/regime-seed.tsv`; absent = warm live).
  `scripts/engine-wrapper.sh` exports the seed
  (`python -m claude_worker.regime seed-out`) before every boot when
  the artifact exists.
- `/metrics`: the `engine_regime_*` family (≈ 50 names) +
  `engine_icdp_regime_{blocked,exits}_total`. Registry headroom holds
  (256 counters / 384 gauges).

**Why**

- `docs/regime-and-dashboard-plan.md` RG1–RG2 (operator decisions
  D1–D4): the engine measures the regime itself, the AI declares, the
  effective word gates members without a table flip.

**Impact**

- On-disk formats: two new DATA files under `~/multivenue/`
  (`regime.toml` — operator-authored from `regime.toml.example`;
  `regime-seed.tsv` — worker-generated, atomic write). Neither is in git.
- Config keys: `regime.toml` grammar per `regime.toml.example`
  (integer-only TOML subset; unknown/missing/duplicate keys refuse the
  boot).
- Wire formats: none beyond RG0's kind 12.
- Behaviour: NONE until `~/multivenue/regime.toml` exists. With it and
  no `[labels.*]`, every coded member stays unconstrained (ANY) — the
  detector only measures and publishes.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before the next boot.
2. Optional: `cp regime.toml.example ~/multivenue/regime.toml` and edit
   `[breadth] members` to descriptors present in `universe.toml`.
3. Restart the standing engine; check the boot tells and
   `engine_regime_configured`.

**Rollback**

- Remove `~/multivenue/regime.toml` (detector unconfigured, no
  behaviour change) or revert the commit. The seed file is inert
  without an artifact.

## 2026-09-03 — `AiCmdKind::SetRegime = 12` + `RuleRowV2` regime tail (RG0 — regime wire freeze)

**What changed**

- `core_types::regime` (new module): `RegimeWord` (one byte per
  dimension, one-hot; bit 7 of each market byte = the per-dimension
  UNKNOWN mark; byte map in `docs/wire-format.md`), `RegimeLabel`
  (any subset per byte; `0` = unconstrained; gate `label == 0 || (word &
  label) == word`; omitted dimensions fill with the legal mask incl.
  the unknown mark, explicit lists exclude it — fail-closed per
  dimension), `RegimeRel` (per-symbol RELATIVE nibbles),
  `RegimeTerm` / `RegimeLabelSet` (≤ 4 product terms, ∃-semantics, for
  coded members), the rule-8 `intersects` law, and the text grammar
  `[fast:|slow:]dim:(*|!v|v1|v2…)` (`parse_label_term`,
  `RegimeLabelBuilder`). `REGIME_PROFILES = 2` is a layout constant.
- `AiCmdKind` gains **`SetRegime = 12`** (append-only ABI; the first
  unassigned byte is now 13) with its `validate_shape` arm: `param_id`
  = profile `< 2`, `px` = declared word (SOURCE byte empty), `qty` = 0
  or a state word, `ttl_ns > 0` ENFORCED, `strategy_id = 0xFF`, sym/side
  none, flags bit 0 legal. `frames.py` mirrors it (`KIND_SET_REGIME`,
  `regime_word()` helpers); the shared golden fixture gains vectors for
  kinds 10, 11 and 12 (it stopped at 9 before).
- `RuleRowV2` spends 18 of its 40 reserved tail bytes: `regime_fast:
  u64` @88, `regime_slow: u64` @96, `regime_off: u8` @104, `regime_rel:
  u8` @105; `_pad3` shrinks to `[u8; 22]` @106. Still 128 B; `ver`
  stays 2; `RuleRowV2::new` keeps its 23-argument shape (tail zero);
  `with_regime(term, off)` sets the tail; `regime_fields_well_formed()`
  is the rule-11 body (RG3 wires it into the validator).
- `audit-replay`'s AI-command table is sized by the last kind (13
  labels) — it indexed out of bounds on any captured seed (kinds 10/11)
  before.

**Why**

- `docs/regime-and-dashboard-plan.md` (operator decisions D1–D4): the
  regime is a gate; VM rows carry their own per-profile masks so a
  regime change never flips a table; the AI declares a word per
  profile through the existing AI plane.

**Impact**

- On-disk formats: none (`RuleRowV2` never crosses a process boundary;
  PMLR `slot_kind = 4` records keep their 64 B shape — kind 12 is a new
  value in an existing byte).
- Config keys: none yet (`regime.toml` arrives with RG2; the example
  file is committed now).
- Wire formats: `AiCmd` kind byte 12 admitted; a pre-RG0 engine counts
  it as malformed (`engine_ingress_ai_malformed_total`) and drops it —
  no crash, no ring push.
- Every existing ruleset artifact validates to bit-identical rows
  (both masks 0); nothing is gated until a row carries a `regimes` key
  (RG3 grammar).

**Migration steps**

1. `cargo build --release -p cli` before any live boot (G0 relink law).
2. Nothing else — no artifact, DB, or capture changes.

**Rollback**

- Revert the commit; the ABI is append-only so no captured file needs
  rewriting. A worker that sends kind 12 to a reverted engine sees the
  frame counted as malformed.

## 2026-09-03 — `Order.ttl_ns` (wire-additive) + the IoC taker fill law + fee ladder (ICDP I1)

**What changed**

- `core_types::Order` spends 8 bytes of its explicit zero padding:
  `ttl_ns: u64` at offset 48 (`_pad1` shrinks to 42..48; `_pad2`
  unchanged). Still 64 B, `repr(C, align(64))`, every byte explicit.
  `Order::new` keeps its 8-argument shape (ttl 0);
  `Order::with_ttl_ns(ttl)` sets it.
- `backtest::fill` (shared verbatim by `backtest` and `audit-pnl`):
  `Order.kind == 1` (IoC) is now MODELED — judged once at the first
  fresh two-sided tick of its sym at/after `t_emit + Δ_venue`: a BID
  fills at that tick's `ask_px` iff `ask_px ≤ P` (an ASK at `bid_px`
  iff `bid_px ≥ P`), qty capped by the displayed opposite size under
  the same shared FIFO budget as makers, the remainder cancels (never
  rests), fee = the venue's TAKER column, rounded up. Any order with
  `ttl_ns > 0` is canceled at the first record of its sym at/after
  `t_emit + ttl_ns` (stale and one-sided records included — expiry is
  a clock fact), before any fill evidence of that record is read.
  `kind ≥ 2` is unroutable (counted, dropped; `debug_assert!` in debug).
- Every fill also accrues the §4.3 **fee ladder** (the same notional
  at flat 0 / 1 / 2 bps per side); reports print OOS net at the ladder
  beside the CLI tier. New counters `ioc_fills`, `ioc_canceled`,
  `ttl_expired`.
- Surfaces: backtest stderr `fills:` line gains `ioc= ioc_canceled=
  ttl_expired=` and a new `fee ladder (oos net, flat bps/side): 0= 1=
  2= tier=` line; **`--emit-detail` is `detail_version` 3**
  (`fills.ioc`, `fills.ioc_canceled`, `fills.ttl_expired`,
  `oos.fee_ladder_net_usd[3]`); audit-pnl per-strategy stderr gains an
  `ioc_fills=… | fee ladder …` row and the JSON (still
  `audit_pnl_version` 1 — additive keys, get-based readers) gains
  `ioc_fills`, `ioc_canceled`, `ttl_expired`, `fee_ladder_net_usd[3]`.
  **Schema-1 stdout is unchanged (frozen).**

**Why**

- ICDP is a taker edge (research note §5.6: resting entries are
  anti-selected); the post-only law scored it negative by construction.
  The vault's merged ICDP×VT plan, D1 (offline IoC model + paper IoC
  intents are pre-Stage-3) and D6 (G1 gate lifted, I1–I7 in order).

**Impact**

- On-disk formats: `engine-orders.pmlr` slots may now carry a non-zero
  `ttl_ns`; every older file reads 0 (never expires). `pmlr.py`'s
  `OrderRec` decodes the 42-byte prefix and is untouched.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Order` table + `kind` semantics.

**Migration steps**

1. Nothing for existing captures or goldens (the VM emits kind 0, ttl 0
   — every existing number is byte-identical).
2. Sidecar readers: accept `detail_version` 3 (additive keys).

**Rollback**

- Revert the commit; IoC intents already captured would replay under
  the maker law again (negative by construction) — the reason for I1.

## 2026-09-03 — Staleness is live: v3 captures start, the harness re-judges, `--emit-detail` v2 (VT2–VT6 close)

**What changed**

- The standing engine writes PMLR v3 since `run-1788417289611943000`
  (relink + reboot 06:34Z); every earlier run dir is v2.
- Every ingress stamps `venue_time_ms` and judges staleness live
  (`core_time::FeedClock`, per-venue `stale_after_ms` defaults pm 1000 /
  bn 1000 / okx 400 / deribit 600 / hl 700 / bybit 500; `run
  --stale-after-ms <venue>:<ms>`); Binance spot inherits the `aggTrade`
  sentinel's stamp + verdict with `TICK_FLAG_VENUE_TIME_SENTINEL`, and
  spot prints are captured as `ChannelId::Trade` event rows.
- strategy-vm: `Mid/Bid/Ask` are ABSENT while the last tick is stale.
- `backtest` / `audit-pnl`: `--stale-after-ms <venue>:<ms>` (both);
  v3 ticks are RE-JUDGED from the stamp per (venue, sym) per run in file
  order (`cli::backtest::stale`), with the **sentinel latch law**: a
  repeated inherited stamp keeps its print's verdict (re-judging it on
  the book update's own `ts_ns` flagged quiet seconds — 3.3 % false stale
  on `binance:btcusdt`, found by the VT2 live smoke). Stale ticks
  neither fill nor mark. stderr gains one `stale:` line per run
  (`stale-blind(v2)` on v2 files). **`--emit-detail` sidecar is
  `detail_version` 2**: `model.stale_after_ms` + a `stale` block
  (`ticks_skipped`, per-run per-lane `{ticks, stale_ticks,
  stale_time_bps, stale_blind}`). Schema-1 stdout is unchanged (frozen).
- `capture-catalog`: per-lane `stale_captured` (the ingress's live
  verdict count; `null` on v2 lanes) in JSON + summary.
- Metrics: `engine_ingress_<venue>_stale_ticks_total`,
  `engine_ingress_<venue>_feed_delay_ema_ms`. `IngressStatus` slot is
  exactly 128 B with zero slack.
- `claude_worker.features.collect_marks` skips stale ticks on v3 files.

**Why**

- `docs/venue-time-capture-plan.md` §1 — stale-blind captures book
  mid-to-mid gains against books the engine could not see. Measured on
  the first v3 run: the VM row's one round trip is +$1.07 stale-blind vs
  −$4.87 judged (vault note).

**Impact**

- On-disk formats: v3 run dirs; `detail_version` 2 sidecars (a v1
  reader must tolerate the new keys — the worker never reads the
  sidecar).
- Config keys: none (`--stale-after-ms` is a flag; the wrapper passes
  none — venue defaults apply).
- Wire formats: none beyond VT1's entry below.

**Migration steps**

1. Research on staleness needs v3 roots: cut ≤ 2 h windows by `ts_ns`
   (the events file cut too) from runs ≥ `run-1788417289611943000`.
2. Treat every `stale-blind(v2)` number as an upper bound (CLAUDE.md
   pitfall 17).
3. Thresholds are re-derived per deployment with the engine-side table
   in `docs/venue-latency.md` §5 — a change is a replay, not a recapture.

**Rollback**

- `--stale-after-ms <venue>:0` on every venue restores the stale-blind
  law offline without a rebuild; the engine flag does the same live
  (ticks still carry the stamp). Reverting the crates restores v2
  writing; v3 files remain readable by the v3 reader only.

## 2026-09-03 — PMLR v3: `Tick.flags` + `Tick.venue_time_ms` (VT1, venue-time capture)

**What changed**

- `core_types::Tick` spends its v2 tail pad: `flags: u8` at offset 49
  (`TICK_FLAG_STALE = 1`, `TICK_FLAG_VENUE_TIME_SENTINEL = 2`) and
  `venue_time_ms: u64` at offset 56; `_pad` shrinks to 6 bytes (50..56).
  Still 64 B, still `repr(C, align(64))`, every byte explicit.
- `Tick::new` keeps its signature and now means "venue time unknown,
  flags 0" (the v2 shape); `Tick::new_stamped(…, venue_time_ms, flags)`
  is the v3 constructor the VT2 ingress parsers will use; `Tick::is_stale`.
- `core_io::VERSION` 2 → 3 for every slot kind (one header number).
  `PmlrReader` accepts ≤ 3 and gains `has_venue_time()` (version ≥ 3).
- `crates/cli` capture acceptance (backtest, audit-pnl, capture-catalog)
  moves from `== 2` to `MIN_PMLR_VERSION ..= core_io::VERSION` through one
  `pmlr_version_accepted` law; v3 ticks replay under the v2 law (never
  stale) until VT4 lands the harness stale rule.
- `claude_worker.pmlr`: `VERSION_MAX = 3`, `TickRec` gains `flags` +
  `venue_time_ms` (+ `is_stale()`), `Reader.has_venue_time`,
  `TICK_FLAG_*` mirrors; golden fixture `ticks_v3.pmlr` (Rust writer);
  `ticks_v2`/`fills_v2`/`ticks_v1` regenerated byte-identical.

**Why**

- `docs/venue-time-capture-plan.md` §1: the capture cannot tell a stale
  Binance book (8.9 % of messages > 500 ms stale) from a current one;
  VT1 is the wire-format prerequisite for the per-venue venue-time
  extraction (VT2) and the staleness gate (VT3/VT4).

**Impact**

- On-disk formats: new captures carry header version 3. Every v2 (and
  v1) file stays readable; both new fields decode as 0 from v2 ("venue
  time unknown, never stale") and as garbage from v1 — consumers gate on
  `has_venue_time`.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Tick` table + replay-log header
  version row. Rings unchanged (same 64 B `Tick`); raw tap unchanged.

**Migration steps**

1. Nothing for existing captures.
2. Tooling that hard-codes `version == 2` (none in-tree after this
   entry; research one-shots read `≤ 3`) must accept 3.
3. The release binary is relinked on the operator's next authorized
   boot (G0 law) — until then the standing engine keeps writing v2.

**Rollback**

- Revert the commit; v3 files written meanwhile are refused by a v2
  reader (`UnsupportedVersion(3)`) — keep them or drop the run dirs.

## 2026-08-30 — VM2 V6: worker seed lane, depth digest, coverage audit, channel map, per-host candle budgets

**What changed**

- `claude_worker.pmlr` learned the kind-7 `DepthTopK` decode
  (`DepthReader`, 192-byte stride — the FIRST kind-determined slot
  size; the 64-B `Reader` keeps refusing kind 7 by design) and the
  kind-3 `Order` decode (`Reader.order/orders`, engine-orders.pmlr).
- `claude_worker.frames`: `KIND_FUNDING_SEED = 10`,
  `KIND_POSITION_SEED = 11` (core-types AiCmdKind mirror).
- NEW module `claude_worker.seeds` — the D-1/D-2 seed lane: kind-10
  frames from the candles.db `funding` table (RAW venue prints ×1e9;
  the ENGINE owns the deribit ÷8 law) + kind-11 restores
  reconstructed from the previous run's engine-orders.pmlr (slot-5
  FIFO fold, (sym,ref)-unique-row ambiguity law, sym RE-RESOLVED
  through the CURRENT manifest, qty = age seconds, ttl 0).
- NEW module `claude_worker.depth_digest` — hourly
  imbalance/spread/near-notional stats per (venue, descriptor) into
  the NEW `depth_digest` table beside candles (iv_digest pattern;
  STALE and empty-side snapshots skipped + counted).
- NEW module `claude_worker.coverage_audit` — per-class expected-vs-
  present audit of candles/funding/iv/depth over the newest manifest.
- NEW module `claude_worker.channel_map` — generated per-instrument
  channel TSV; `caps_of_descriptor` python mirror pinned CROSS-
  LANGUAGE against the new Rust `caps_of_descriptor_law` test.
- `claude_worker.candles.run_cycle`: REST budgets are now per HOST
  (`budget_key`; only the two bybit categories pool) and DEMAND-SIZED
  `max(env floor, 2 × tfs × targets)` per host.
- `scripts/candles-cycle.sh` runs `claude_worker.depth_digest` after
  the IV digest (same serialized window; exec bit preserved).

**Why**

- V6 of docs/vm2-plan.md: warm VM restarts (funding windows + open
  positions survive the daily restart without crons) and D-8 research
  reach (the agent sees depth, knows every instrument's channels, and
  the audit names data holes instead of leaving them silent).
- The budget change fixes a LIVE starvation found by the new audit
  (2026-08-30): binance spot and usdm shared one per-venue 30-call
  budget although they hit different hosts — the 22-target usdm lane
  got ZERO pages every cycle and 12 M5/BST symbols had no candles at
  all.

**Impact**

- On-disk formats: NEW `depth_digest` table in candles.db (additive);
  no existing table/file changes. `~/multivenue/worker/channel-map.tsv`
  is a new generated file.
- Config keys: none new (depth digest honors
  `CLAUDE_WORKER_DEPTH_DIGEST_WINDOW_H`, default 26). The meaning of
  `CLAUDE_WORKER_CANDLES_BUDGET_PER_H` narrows from per-venue pool to
  PER-HOST FLOOR.
- Wire formats: none (kinds 10/11 landed in V1; this is the worker
  mirror).

**Migration steps**

1. Nothing mandatory — tables and modules are additive; the next
   hourly candles cycle backfills the starved usdm symbols under the
   new budgets and starts writing depth digests.
2. Optional one-shots: `python -m claude_worker.depth_digest
   --backfill` (done 2026-08-30, 108 buckets), `python -m
   claude_worker.channel_map`.

**Rollback**

- Revert the commit; drop the `depth_digest` table if desired
  (nothing reads it engine-side). Seeds are push-only and the engine
  refuses/expires malformed ones by design.

## 2026-08-30 — VM2 V5: multi-channel backtest, warmup, D-7 options mark-fill law, D-3 report/gate amendment

**What changed**

- The backtest merge carries funding/ctx `ChannelEvent`s, `DepthTopK`
  and `OptSummary` records beside ticks (same §3.2/§3.3 total order
  and VIRT rebase; lane ordinals extend the lord space), replayed
  through the vm's REAL callbacks — every §1.1 feature evaluates in
  replay exactly as live (§1.5).
- Per-run sym REBIND (the §6 law's replay half): each run's manifest
  joins by DESCRIPTOR to the newest run's, and every record's sym is
  rewritten at load — options ordinals that reshuffle across boots
  evaluate as ONE instrument.
- WARMUP (§1.5, refined — recorded in vm2-plan §8): the longest
  window the TABLE references (Roll windows; Apr24 ⇒ 24 h; Apr72 ⇒
  72 h), 0 when none — features fill, no entries, split math
  unchanged, `warmup_end_virt_ns` reported.
- D-7 options mark-fill law in `backtest::fill` (shared by
  audit-pnl): mark-bearing OptSummary records synthesize zero-spread
  mark ticks for option syms without a tick lane; registered syms
  execute IMMEDIATELY at `mark ± max(0.5%, 1 tick)` with TAKER fees
  and value at mark; `mark_fills` counts and the assumption is
  PRINTED wherever it shaped numbers. okx's markless summaries stay
  feature-only (honestly unpriceable).
- D-3: schema-1 gains ADDITIVE keys — `oos.round_trips`, `oos.legs`,
  top-level `position_rows` (goldens updated; schema_version stays
  1). The worker gate counts LEGS toward `min_trades` and folds the
  position-ruleset floor `round_trips >= MIN_ROUND_TRIPS (10)` into
  the same verdict — GateThresholds/GateResult keep their frozen
  shapes; pre-V5 reports gate byte-identically (pinned by
  `tests/test_backtest_d3.py`, ruling cited).

**Why**

- vm2-plan §4-V5: funding/IV/depth strategies must be honestly
  backtestable through the frozen argv before V7's real backtests.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: schema-1
  additive keys documented in the harness goldens; worker report
  gains mirrored additive keys.

**Migration steps**

1. None — pre-V5 captures and reports replay/gate unchanged.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V4: validator v2 (descriptor resolution, rules 9–10), the §6 handoff flips to v2, v1 `RuleTable` retired

**What changed**

- The §4.2 validator grew the v2 grammar arm (docs/wire-format.md
  "Ruleset JSON grammar v2"): descriptor-addressed rows (D-6 —
  stage-time resolution against the bin's DescriptorTable, built
  from the SAME allocation truth as `instrument-manifest.tsv`;
  unresolvable ⇒ the new `Descriptor` reject), rule 9 (`Position`)
  and rule 10 (`Feature`: channel capabilities + window law + the
  rolling-bind budget), signed 9-decimal thresholds, and the
  KEYWORD_CAP 16→24 growth. v1 rows keep validating byte-exactly
  (the compat arm builds them THROUGH `RuleRowV2::from_v1`); both
  shapes may share one artifact.
- The §6 table-handoff ring is v2-typed (`RuleTableSlot =
  RuleTableV2`, 32 832 B slots; `Strategy::on_ruleset_table` takes
  `&RuleTableV2`); the v1 `RuleTable` struct retired (`RuleRow`
  stays as the v1-grammar record through the compat window).
- The backtest harness resolves v2 descriptors against the NEWEST
  run's `instrument-manifest.tsv` (offline capability law =
  `caps_of_descriptor`, deliberately permissive where the string
  under-determines — wrong grants only yield absent-data-holds);
  manifest-less (pre-D3) captures refuse v2 rows honestly.
- Fuzz: `ruleset_json` covers both arms (fixture descriptor table
  wired into the target; corpus seeded with v2/mixed artifacts).
  Bench gate 34 validates 255 v1 + 1 v2 rows with live resolution
  inside the measured window.

**Why**

- vm2-plan §4-V4/D-6: artifacts must be portable across restarts and
  universe edits, and refusals must be loud and specific before the
  agent authors against the grammar.

**Impact**

- On-disk formats: none (artifacts stay JSON; identity unchanged).
- Config keys: none.
- Wire formats: ruleset JSON grammar v2 documented; v1 table section
  retired in docs/wire-format.md.

**Migration steps**

1. None for operators: existing v1 artifacts (raw syms) stage and
   commit unchanged through the compat arm — one release, per D-6.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V3: the v2 grammar evaluator + position layer live in strategy-vm

**What changed**

- `VmStrategy` evaluates the GENERAL v2 grammar (vm2-plan §1.2–§1.3)
  over the V2 feature engine: two-operand signals, confirm gates,
  the position state machine (Flat→Entered→Flat), group exclusivity,
  two-leg emits, min/max-hold, the universal exit law
  `signal × entry_sign ≤ exit_1e9`, and `PositionSeed` (D-2)
  restore. v1 `RuleTable`s arriving through the UNCHANGED trait seam
  map row-for-row onto v2 sugar rows (`RuleRowV2::from_v1` — the
  byte-exact v1 semantics law); the §6 handoff ring stays v1-typed
  until V4 flips the validator.
- SEMANTIC DELTA (deliberate, vm2-plan §1.2): rows now evaluate when
  EITHER leg's sym ticks (two-legged signal freshness) — v1
  evaluated on action-sym ticks only. Fires move to the FIRST tick
  that satisfies them (fresher data); condition/emit laws unchanged.
  The golden harness pins the new eval counts.
- The vm's book generic retired (`VmStrategy<N>` → `VmStrategy`) —
  mids live in the feature engine (`FEAT_SYM_SLOTS` grew 1024→4096,
  absorbing the old `BACKTEST_VM_SLOTS` law); `SET_VM_SLOTS` retired
  with it.
- Sizing law hardened (caps-proptest catch): a qty whose NOTIONAL
  floors to zero is clamped away (the §11 zero-notional invariant
  now lives in `sized_qty_1e6` itself).

**Why**

- vm2-plan §4-V3: the general grammar must execute engine-side
  before the validator (V4) can accept it from artifacts.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: none (the
  evaluator is in-process; V1's formats stand).

**Migration steps**

1. None — v1 artifacts behave identically through the sugar mapping
   (two-legged evaluation moves WHEN a fire lands, never whether).

**Rollback**

- Revert the commit.

## 2026-08-29 — VM2 V2: feature engine, OptSummary engine lanes, Deribit Funding `v1` = funding_8h, HL event lane

**What changed**

- `strategy-vm` gains the feature engine (`strategy_vm::features`,
  vm2-plan §1.1): ONE boxed ~12 MiB zeroed-at-boot state holding
  per-sym latest values, per-(sym, window) rolling minute rings
  (lazy-recompute stats), funding-print rings with the per-venue
  settled-print laws, mark/IV and depth-derived features, and the
  venue-derived wall-clock offset. Fed exclusively through the vm's
  `Strategy` callbacks — the backtest replays the same records
  through the same code (§1.5 parity). Zero alloc after boot
  (release gate 39, `vm_feature_engine_paths_are_zero_alloc`).
- OptSummary (kind 6) enters the engine for the first time:
  `Strategy::on_opt_summary` (defaulted), three opt lanes
  (`OPT_RING_SIZE` 4096; `engine::opt_lane_of` okx/deribit/bn — the
  BN lane venue-dark until the eapi heal), pushes at every venue
  emit site AFTER capture (§6.5 law), `opt_ring_drops_total` per
  ingress. Capture files unchanged.
- Deribit Funding events: `v1` now carries `funding_8h` ×1e9 (was a
  constant 0) — additive; the parser gained the optional field
  (ticker scratch frame grew to 128 B, in-process only). Pre-V2
  captures replay via the `current_funding` (`v0`) fallback.
- Hyperliquid gained its venue-event lane (it had none): funding
  rides AssetCtx rows, spawn mask
  `EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX`.
- `AiCmdKind::FundingSeed` is consumed: the vm folds seeds into the
  same funding windows live events feed (dedup within half the
  venue print period).

**Why**

- vm2-plan §4-V2: every §1.1 feature must evaluate engine-side (and
  identically in replay) before the V3 grammar evaluator lands.

**Impact**

- On-disk formats: none (no capture layout changed; deribit Funding
  `v1` is a value-semantics addition inside an existing field).
- Config keys: none.
- Wire formats: docs/wire-format.md — OptSummary lane note, Funding
  `v1` note, HL AssetCtx lane note.

**Migration steps**

1. None — replay of old captures works (fallbacks documented).

**Rollback**

- Revert the commit; no persisted state depends on the new lanes.

## 2026-08-29 — VM2 V1: RuleRowV2/RuleTableV2 (table version 2), AiCmd kinds 10–11 (vm2-plan D-1…D-8)

**What changed**

- `core-types` gains the VM2 general-grammar types (vm2-plan §1/§3,
  design LOCKED 2026-08-29): `RuleRowV2` (128 B, two cache lines —
  feature/combine grammar, position mode, groups, confirm, min/max
  hold) and `RuleTableV2` (256 rows, 32 832 B), ADDITIVE beside the
  v1 types. The vm evaluator flips to v2 in V3, the validator + the
  §6 table-handoff ring in V4; the v1 `RuleRow`/`RuleTable` retire
  then (no unused code stays). v1 JSON artifacts keep committing
  through a compat arm for one release (D-6) — sugar maps onto v2
  rows with byte-exact v1 semantics, so H6-era artifacts and
  `cvfc-basis-kill` stay valid.
- `AiCmdKind` appends `FundingSeed = 10` (D-1: one historical funding
  print — sym, rate ×1e9 in `px`, venue print ms in `qty`) and
  `PositionSeed = 11` (D-2 as ruled: positions RESTORE at boot — row
  index in `param_id`, entered side, entry px in `px`, position age
  SECONDS in `qty`, `ttl_ns` 0-enforced: the drain site expires any
  nonzero ttl, and entry qty re-derives from the row's sizing law so
  restores respect current caps). Shape rules enforced by
  `AiCmd::validate_shape`; byte
  meanings pinned in `docs/wire-format.md`. Both are engine-directed
  (`venue = Ai`, `strategy_id = 5`). Capture-compatible: the 64 B
  AiCmd layout is unchanged, `ai-cmds.pmlr` readers see two new kind
  bytes.
- The funding cadence law gets its single home:
  `core_types::funding_print_divisor` (Deribit ÷8 — hourly samples of
  `interest_8h`) + `funding_period_s` (clock-feature fallback). The
  worker mirror (`claude_worker.carry_signal.apr_from_prints`) gains
  a pin test in V6.
- Refinement over the D-1 sketch (recorded in vm2-plan §8):
  FundingSeed carries RAW PRINTS with venue timestamps, not
  per-window aggregates — windows recompute engine-side through the
  same path live events take, keeping the cadence law in one place.

**Why**

- vm2-plan §0: the two-word v1 rule language cannot express the M5
  strategy families; v2 is the general, cron-free replacement. D-5
  ruled 128 B rows (the grammar does not fit 64 B).

**Impact**

- On-disk formats: none yet (RuleRow/Table never captured; AiCmd
  layout unchanged — only new kind bytes appear in `ai-cmds.pmlr`
  once V6 pushes seeds).
- Config keys: none.
- Wire formats: AiCmd kind table extended; RuleRowV2/RuleTableV2
  documented in `docs/wire-format.md`.

**Migration steps**

1. None until V3/V4 flip the evaluator/validator — this commit is
   type-additive and inert at runtime.

**Rollback**

- Revert the commit; no persisted state references the new types or
  kinds until V6.

## 2026-08-29 — SlotKind 7 = DepthTopK, first non-64-byte PMLR slot (WS10-B)

**What changed**

- PMLR slot size is now KIND-determined: kinds 0–6 keep 64 B; the new
  kind 7 (`DepthTopK`, 192 B = three cache lines) carries the WS10-B
  top-K depth snapshots in `<venue>-depth.pmlr` (opened for EVERY
  venue by the uniform-file-set law; header-only without a depth
  subscription). `PmlrReader` decodes the kind from the header FIRST
  and validates the caller's type/stride against it. Container
  version stays 2 — no pre-WS10 file changed shape.
- The engine gains two depth lanes (`Ring<DepthTopK, 4096>`, OKX +
  Deribit, `engine::depth_lane_of`) and the defaulted
  `Strategy::on_depth`; emission is change-gated in the ingress
  (`book_builder::ladder`, 64 levels/side); a seq-chain break emits a
  `flags = STALE` snapshot after clearing the ladder.
- WS10-A (same commit series, no wire change): venue-event lanes
  carry funding `ChannelEvent`s in-process (`EVENT_RING_SIZE` 1024,
  spawn-time `event_mask`, funding-only in v1) — the capture record
  IS the carrier, so nothing here migrates.

**Why**

- gaps-doc §1 / ws10-engine-plumbing-design.md, operator-approved
  D-A1..D-B3: funding and L2 depth reach `Strategy` without a second
  wire type; the 192 B slot keeps the top-5-per-side snapshot in one
  POD instead of splitting rows across 64 B records.

**Impact**

- On-disk: new `<venue>-depth.pmlr` files appear in every run dir
  from the first WS10 boot. Readers that assumed a flat 64 B stride
  must consult `SlotKind::slot_size` (in-tree readers updated;
  `claude-worker/pmlr.py` opens only tick/opt-summary files and is
  unaffected).
- audit-replay renders a per-venue `depth` stream section + totals
  (snapshots / syms / stale count) when records exist.

**Migration steps**

1. None for existing files. New binaries read old runs unchanged.

**Rollback**

- Pre-WS10 binaries ignore unknown kind 7 files (open fails with
  `UnknownSlotKind`; nothing else reads them).

## 2026-08-29 — VenueId 6 = Bybit + tick lane 6 (WS9, the sixth venue)

**What changed**

- `VenueId` gains `Bybit = 6` (append-only; `Ai = 5` keeps its
  discriminant — the lane↔venue identity is broken past lane 4 and
  `engine::tick_lane_of` is the mapping: Bybit rides TICK LANE 5).
- `NUM_TICK_LANES` 5 → 6; `TRADEABLE_VENUES` 5 → 6 with the new
  `tradeable_venue_byte` predicate (bytes 0..=4 and 6; Ai excluded);
  `ModelParams` tables widen to 7 slots (slot 5 = Ai, DEAD).
- New capture label `bybit` (`bybit-ticks/-events/...pmlr`),
  `bybit:` / `bybit-linear:` descriptor namespaces, `[bybit]`
  universe section (spot/linear; linear ordinal base 512), Config
  hosts `BYBIT_WS_HOST`/`BYBIT_REST_HOST`, metrics family
  `engine_ingress_bybit_*` + coverage gauge, TUI health bit 7.
- Worker: `VENUE_BYBIT = 6`, map seeding for `[bybit]`, candle lanes
  `bybit`/`bybit-linear` (kline REST), refdata tickers lane.

**Why**

- stage2-finish-plan WS9 / gaps-doc §1 — the sixth venue.

**Impact**

- On-disk formats: new per-venue capture files under the existing
  container version; ticks may carry venue byte 6. Pre-WS9
  `audit-replay` binaries skip the unknown label's files and treat
  venue byte 6 as corruption — decode with a post-WS9 binary.
- Config keys: `[bybit] spot/linear` (additive); two new optional
  env hosts.
- Wire formats: append-only enum growth on `VenueId`.

**Migration steps**

1. None until a `[bybit]` section is configured; the venue is
   entirely opt-in.

**Rollback**

- Safe while `[bybit]` stays empty (old binaries reject the section
  as an unknown-section parse error — remove it before rolling
  back).

## 2026-08-29 — ChannelId 12 = VolIndex (WS6 Deribit DVOL)

**What changed**

- `ChannelId` gains `VolIndex = 12` (append-only): the Deribit DVOL
  capture series. Field semantics in `docs/wire-format.md`: `sym` =
  `SYMBOL_ID_NONE` (venue-global), `v0` = volatility points ×1e9,
  `v1` = ordinal into the configured `[deribit] options_underlyings`
  list (DVOL subscriptions derive from that list — `BTC` →
  `btc_usd`; no new config key).
- DVOL channels ride the batched subscribe but sit OUTSIDE the
  subscribe-verification mask (its u128 is fully allocated) — an
  index the venue does not serve is a missing capture series, never
  a session verdict.

**Why**

- stage2-finish-plan WS6 / gaps-doc §1 "New data series": DVOL was
  absent from the repo entirely.

**Impact**

- On-disk formats: event logs may carry `channel = 12` rows. PMLR
  container version unchanged.
- Config keys: none (derived from options underlyings).
- Wire formats: pre-WS6 readers skip id 12 as a corrupt byte —
  additive, same posture as id 11.

**Migration steps**

1. None — decode with a post-WS6 binary.

**Rollback**

- Binary rollback safe: old readers skip id 12; old writers never
  emit it.

## 2026-08-29 — ChannelId 11 = SubDrop (WS2 non-fatal subscribe drops)

**What changed**

- `ChannelId` gains `SubDrop = 11` (append-only; nothing renumbered):
  the §6.6-paired evidence event for WS2's non-fatal subscribe drops
  on OKX/Deribit reconnect sessions. Field semantics in
  `docs/wire-format.md` (event-slot `channel` row): `sym` = dropped
  instrument or `SYMBOL_ID_NONE`, `v0` = venue error code (0 =
  missing-from-echo), `v1` = venue-local channel discriminant (−1 =
  unknown/folded).
- `IngressStatus` gains `sub_drops_total` (slot stays 128 B; mirrored
  as `engine_ingress_<venue>_sub_drops_total`).

**Why**

- Capture-continuity outage 2026-08-27 §5.2: venue errors / missing
  echo channels on reconnect killed whole sessions for six days.
  WS2 makes them per-instrument drops; every drop must stay visible
  offline (counter ↔ event pairing, the TradeGap/BookGap precedent).

**Impact**

- On-disk formats: event logs written by post-WS2 binaries may carry
  `channel = 11` rows. PMLR container version unchanged.
- Config keys: none.
- Wire formats: pre-WS2 readers (`ChannelId::from_u8`) treat 11 as a
  corrupt byte and skip the row — old `audit-replay` binaries
  under-report only the new event class, nothing else.

**Migration steps**

1. None operationally — the id is additive. Decode with a post-WS2
   binary; the worker's `pmlr.py` channel map picks the id up in its
   WS11 fold-in.

**Rollback**

- Binary rollback is safe: old readers skip id 11; old writers never
  emit it.

## 2026-08-23 — Order slot: `strategy_id` at offset 41 + the `engine-orders.pmlr` intent log (M4.1)

**What changed**

- `Order` (64 B ring/capture slot) claims ONE byte of `_pad1`:
  offset 41 = `strategy_id: u8` (strategy-set slot ids; `0xFF` =
  unattributed), `_pad1` shrinks 15 → 14 B. Stamped by the set's
  `StampCtx` adapter around every member callback; bare
  single-strategy boots leave `0xFF`.
- NEW engine-side capture file `engine-orders.pmlr`
  (`slot_kind = 3`, `core_io::SlotCapture<Order>`): every order the
  dispatcher ACCEPTED via `ctx.submit`, staged on the engine thread
  next to `engine-fills.pmlr`. Dispatcher refusals remain counters
  only.

**Why**

- mvp-plan §4-M4 (shadow-P&L): the M4.1 audit found paper mode
  captured NEITHER intents nor fills (`PaperDispatcher` counts and
  drops; `SlotKind::Order` was defined but wired to nothing) — the
  per-strategy intent log is the enabling substrate for `audit-pnl`
  (§9.9 "logged intents"). Per-ruleset attribution deliberately
  rides the existing ai-cmds `RulesetCommit` timeline (hash128 in
  px/qty), NOT a wider Order slot.

**Impact**

- On-disk formats: a NEW per-run file; PMLR container version stays
  2 (append-only kind usage). No historical Order file exists —
  the layout amendment is PRE-FIRST-CAPTURE and has zero
  reader-compat surface. Old binaries reading a new run dir simply
  never open the file; `audit-replay`/catalog treat it as another
  size-visible capture file.
- Config keys: none.
- Wire formats: `Order` table amended in `docs/wire-format.md`
  (offset 41). In-process ring consumers are unaffected (field was
  explicit zero padding; `Order::new` initializes `0xFF`).

**Migration steps**

1. Nothing operator-side: the file appears on the first boot of a
   binary carrying M4.1; older run dirs simply lack it (audit-pnl
   reports them intent-less).

**Rollback**

- Revert the commit; run dirs written meanwhile carry an extra
  `.pmlr` file old code never opens. Harmless.

## 2026-08-22 — SlotKind 6 (OptSummary): the options analytics capture channel (M2.3)

**What changed**

- New PMLR `slot_kind = 6` — `OptSummary` (64 B, layout pinned in
  `docs/wire-format.md`): mark px / mark IV / BS greeks / open
  interest / underlying px per option instrument, fed by Deribit
  option `ticker.{instr}.100ms` and OKX `opt-summary` (BN eapi at
  M2.4).
- `core_io::PmlrCapture` opens a FOURTH per-venue file,
  `<venue>-opt-summary.pmlr`, for EVERY venue (header-only where no
  options lane exists — the same uniform-file-set law as
  `<venue>-signals.pmlr`).
- PMLR **version stays 2** — this is an append-only SlotKind addition;
  no existing slot layout changed.

**Why**

- mvp-plan §4-M2.3/§9.8: one new capture record on the append-only
  raw-store doctrine; the strategist digest and audit read it offline.

**Impact**

- On-disk formats: run dirs gain `<venue>-opt-summary.pmlr` per venue.
  READER COMPAT: pre-M2.3 readers never open the new file (separate
  name) and are unaffected; `SlotKind::from_u8(6)` decodes only in
  M2.3+ binaries — an OLD binary reading a NEW file's header reports
  unknown-kind corruption, which is correct-and-loud, and never
  happens through the shipped tools (they open files by name/kind).
  Old run dirs (no opt-summary files) audit exactly as before —
  audit-replay treats the file as absent.
- Config keys: none (the options lanes were M2.1/M2.2 config).
- Wire formats: new `OptSummary` section in `docs/wire-format.md`;
  Deribit option rows now subscribe `ticker` in addition to `quote`
  (subscribe-verification folds both into one per-row bit); OKX
  gains the family-keyed `opt-summary` subscription (2 args).

**Migration steps**

1. None — capture stays append-only; new files appear on the first
   M2.3 boot.

**Rollback**

- Boot the previous binary: new files stop being written; existing
  ones remain readable by M2.3+ tools and ignorable garbage-by-name
  to older tools.

## 2026-08-15 — SlotKind 5 (ChannelEvent), capture files, raw tap (Phase 8e)

**What changed**

- New PMLR `slot_kind = 5` — `ChannelEvent` (64 B, layout in
  `docs/wire-format.md`): non-tick channel capture. `slot_kind = 4`
  is **reserved** for Stage-2 `AiCmd` (plan §8.4) and still decodes as
  invalid.
- PMLR replay capture is now actually wired into the shipped `run`
  path (the 8e defect fix): each ingress thread writes
  `<venue>-ticks.pmlr` / `<venue>-events.pmlr` / `<venue>-signals.pmlr`
  into `<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`.
- New sidecar format `<venue>-raw.tap` (`b"PMRT"` v1) — bounded raw
  payload capture behind `--raw-tap`, off in production.
- `MULTIVENUE_LOG_DIR` now tilde-expands a leading `~/` at boot.

**Why**

- Plan §6.5: the replay logs are the 8h backtest dataset and the
  `audit-replay` input; §6.6 G1 soaks are judged from them.

**Impact**

- On-disk formats: existing v2 Tick/Signal/Fill/Order logs unchanged
  and fully readable. Binaries at or before 8d.1 refuse `slot_kind=5`
  files (unknown kind = corruption by their rules) — expected.
- Config keys: `MULTIVENUE_LOG_DIR` semantics extended (tilde
  expansion); no new keys for capture itself. Tap is flag-driven.
- Wire formats: ring slots untouched — `ChannelEvent` exists only in
  PMLR files, never in rings.

**Migration steps**

1. Nothing for existing logs.
2. Tooling that globs `*.pmlr` should route on the header `slot_kind`
   byte (0/1/2/3/5) rather than assuming Tick.

**Rollback**

- Delete `run-<epoch_ns>/` capture directories; pre-8e binaries never
  read them.

## 2026-08-14 — PMLR v2: venue bytes + explicit padding (Phase 8a)

**What changed**

- `PMLR VERSION` bumped 1 → 2.
- `Tick` gains `venue: u8` at offset 48 (was implicit padding);
  `Order` gains `venue: u8` at offset 40 (was `_pad1[0]`). Values are
  `VenueId`: Polymarket=0, Binance=1, Okx=2, Deribit=3, Hyperliquid=4,
  Ai=5.
- All padding in all four slots is now explicit and zeroed. v1 writers
  emitted 8 undefined tail-padding bytes per slot (D9).
- `SymbolId` is venue-namespaced: bits 31..24 = venue, bits 23..0 =
  per-venue ordinal.

**Why**

- Phase 8 multivenue expansion needs venue identity on every tick and
  order; the `AsBytes` zero-copy log contract requires fully
  initialized slots.

**Impact**

- On-disk formats: v1 logs remain readable (`PmlrReader` accepts
  version ≤ 2, exposes `version()`). v1 files are **venue-less**: the
  byte at Tick offset 48 / Order offset 40 is undefined garbage in v1
  and must be ignored when `version() == 1`. Venue cannot be inferred
  from v1 slots (slot kind + sym do not disambiguate Polymarket vs
  Binance ticks).
- Config keys: none in this entry (per-venue symbol flags land with
  the venue ingress phases).
- Wire formats: ring slots and PMLR slots share the new layout;
  `docs/wire-format.md` is the byte-level source of truth.

**Migration steps**

1. Nothing to do for live capture — new logs are v2 automatically.
2. Backtests mixing v1 + v2 logs must branch on `PmlrReader::version()`
   and treat v1 venue bytes as absent.

**Rollback**

- Binaries at or before Phase 7 refuse v2 logs (`version > 1`); keep
  v1 archives if a rollback below Phase 8a is contemplated.

## 2026-04-19 — Phase 0 scaffold initial wire format

**What changed**

- Introduced the Phase 0 wire format documented in `docs/wire-format.md`:
  `Tick`, `Signal`, `Fill`, and `Order` are all 64-byte cache-aligned POD
  structs. Replay-log header version pinned at `1`.

**Why**

- First commit — establishes the baseline that every subsequent migration
  will bump against.

**Impact**

- On-disk formats: replay log header magic `b"PMLR"`, version `1`.
- Config keys: see `config.example.toml`.
- Wire formats: as documented in `docs/wire-format.md`.

**Migration steps**

1. None — fresh install.

**Rollback**

- Remove `~/multivenue/replay/` and `~/multivenue/artifacts/`.
