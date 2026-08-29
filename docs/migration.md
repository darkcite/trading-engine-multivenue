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
