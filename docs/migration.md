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
