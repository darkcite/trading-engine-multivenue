# Wire format — ring slots and replay log

This document is the source-of-truth for **every byte** that flows through
the engine's in-process rings and its on-disk replay log. Any code that
produces or consumes these formats must cite this file.

The formats are deliberately:

- **POD**: `#[repr(C)]`, `#[derive(Copy, Clone)]` — no heap fields.
- **64-byte aligned**: one slot per cache line, no false sharing.
- **Fixed-width**: zero parsing branches at the consumer.

All multi-byte fields are **native-endian** (little-endian on every platform
we target — aarch64 Apple Silicon and x86_64 Linux). The replay log is
expected to be read on the same machine class that produced it; no
cross-endianness translation is attempted.

## Ring slot layouts

Defined in `crates/core-types/src/lib.rs`. A static size assert at the
bottom of that file pins every struct at exactly 64 bytes.

### Venue identity (Phase 8, PMLR v2)

`VenueId` (`#[repr(u8)]`, wire-stable, never renumbered):
`Polymarket=0, Binance=1, Okx=2, Deribit=3, Hyperliquid=4, Ai=5`.
`255` is reserved (venue byte of `SYMBOL_ID_NONE`).

`SymbolId` is venue-namespaced: bits 31..24 = venue byte, bits 23..0 =
per-venue ordinal allocated at boot from venue discovery. Staleness
bucketing mixes the venue byte: `bucket = (sym ^ (sym >> 24)) & 63`.

`Tick` and `Order` additionally carry the venue as an explicit byte so
log consumers and lane audits never decode `sym`. All padding in every
slot is **explicit and zeroed** — this is the `AsBytes` contract; v1
files predate it (see `docs/migration.md`).

### `Tick` — 64 bytes

| offset | bytes | field      | type             | notes                                  |
| -----: | ----: | ---------- | ---------------- | -------------------------------------- |
|      0 |     8 | ts_ns      | `u64` NsTs       | monotonic ns from `core-time::now_ns`  |
|      8 |     4 | sym        | `u32` SymbolId   | venue-namespaced; `SYMBOL_ID_NONE = u32::MAX` invalid |
|     12 |     4 | venue_seq  | `u32`            | venue-provided sequence, monotonic     |
|     16 |     8 | bid_px     | `i64` Price      | fixed-point ×1e6                       |
|     24 |     8 | bid_qty    | `i64` Qty        | fixed-point ×1e6                       |
|     32 |     8 | ask_px     | `i64` Price      | fixed-point ×1e6                       |
|     40 |     8 | ask_qty    | `i64` Qty        | fixed-point ×1e6                       |
|     48 |     1 | venue      | `u8` VenueId     | producing venue (v2+; garbage in v1)   |
|     49 |    15 | _pad       | `[u8; 15]`       | explicit, zeroed                       |

### `Signal` — 64 bytes

| offset | bytes | field    | type           | notes                                  |
| -----: | ----: | -------- | -------------- | -------------------------------------- |
|      0 |     8 | ts_ns    | `u64` NsTs     | monotonic ns                           |
|      8 |     4 | sym      | `u32` SymbolId |                                        |
|     12 |     1 | class    | `u8`           | `LatencyClass` (Hot=0, Warm=1, Slow=2) |
|     13 |     1 | source   | `u8`           | `SignalSource` (Rss=1 retired, reserved) |
|     14 |     2 | _pad0    | `[u8; 2]`      | explicit, zeroed                       |
|     16 |    40 | payload  | `[u8; 40]`     | opaque; interpretation by source       |
|     56 |     8 | _pad1    | `[u8; 8]`      | explicit, zeroed (v2+; garbage in v1)  |

### `Fill` — 64 bytes

| offset | bytes | field     | type           | notes                           |
| -----: | ----: | --------- | -------------- | ------------------------------- |
|      0 |     8 | ts_ns     | `u64` NsTs     |                                 |
|      8 |     4 | sym       | `u32` SymbolId | venue-namespaced (venue in sym) |
|     12 |     1 | side      | `u8`           | `Side` (Bid=0, Ask=1)           |
|     13 |     3 | _pad0     | `[u8; 3]`      | explicit, zeroed                |
|     16 |     8 | px        | `i64` Price    | fixed-point ×1e6                |
|     24 |     8 | qty       | `i64` Qty      | fixed-point ×1e6                |
|     32 |     8 | order_id  | `u64`          | engine-assigned client oid      |
|     40 |    16 | _pad1     | `[u8; 16]`     | explicit, zeroed                |
|     56 |     8 | _pad2     | `[u8; 8]`      | explicit, zeroed (v2+; garbage in v1) |

### `Order` — 64 bytes

| offset | bytes | field       | type           | notes                           |
| -----: | ----: | ----------- | -------------- | ------------------------------- |
|      0 |     8 | ts_ns       | `u64` NsTs     | client creation ts              |
|      8 |     4 | sym         | `u32` SymbolId | venue-namespaced                |
|     12 |     1 | side        | `u8`           | `Side`                          |
|     13 |     1 | kind        | `u8`           | 0=Limit, 1=IoC, 2=Market (rsv.) |
|     14 |     2 | _pad0       | `[u8; 2]`      | explicit, zeroed                |
|     16 |     8 | px          | `i64` Price    |                                 |
|     24 |     8 | qty         | `i64` Qty      |                                 |
|     32 |     8 | client_oid  | `u64`          | engine-assigned, monotonic      |
|     40 |     1 | venue       | `u8` VenueId   | routing target (v2+; garbage in v1) |
|     41 |    15 | _pad1       | `[u8; 15]`     | explicit, zeroed                |
|     56 |     8 | _pad2       | `[u8; 8]`      | explicit, zeroed                |

### `AiCmd` — 64 bytes (8f; `ingress-ai` ring + PMLR `slot_kind = 4`)

AI-ingress command (plan §8.4). Packed by `claude-worker` (`frames.py`)
as the payload of an 82-byte HMAC-tagged UDS frame
(`[len u16 = 80][AiCmd 64 B][tag 16 B]`), materialized and shape-checked
by `ingress-ai`, captured to PMLR, pushed onto `Ring<AiCmd, 1024>`.
`ts_ns` is rewritten to engine-monotonic time at accept (after HMAC
verify, before ring push) — the capture record carries the rewritten
slot, byte-identical to what the ring consumer sees; the worker's send
time survives only in the optional raw tap (design §3/§13.1 as amended
2026-08-15). Per-kind field shapes ("unused fields
MUST be zero / `SYMBOL_ID_NONE` / `0xFF`") are pinned in
`docs/phase-8f-design.md` §3 and enforced by
`core_types::AiCmd::validate_shape`.

| offset | bytes | field       | type           | notes                                   |
| -----: | ----: | ----------- | -------------- | --------------------------------------- |
|      0 |     8 | ts_ns       | `u64` NsTs     | engine accept time (rewritten; see above) |
|      8 |     4 | seq         | `u32`          | strictly increasing per session; gap = counter, regress = discard |
|     12 |     4 | sym         | `u32` SymbolId | venue-namespaced or `SYMBOL_ID_NONE`    |
|     16 |     8 | px          | `i64`          | ×1e6: fair value / intent px / param value / ruleset hash\[0..8\] LE |
|     24 |     8 | qty         | `i64`          | ×1e6: intent qty / ruleset hash\[8..16\] LE |
|     32 |     8 | ttl_ns      | `u64`          | expiry relative to `ts_ns`; 0 = none    |
|     40 |     1 | kind        | `u8` AiCmdKind | 0=Heartbeat 1=EnableStrategy 2=DisableStrategy 3=SetFairValue 4=SetBias 5=SetParam 6=OrderIntent 7=RulesetStage 8=RulesetCommit 9=HaltRequest — **no Resume exists** (halt is sticky) |
|     41 |     1 | venue       | `u8` VenueId   | `Ai=5` for engine-directed cmds; target venue for intents |
|     42 |     1 | strategy_id | `u8`           | strategy-set slot; `0xFF` = none; intents pin 4 (ai-exec), ruleset cmds pin 5 (vm) |
|     43 |     1 | side        | `u8`           | `Side` (Bid=0, Ask=1) or `0xFF`         |
|     44 |     2 | param_id    | `u16`          | `SetParam` selector; else 0             |
|     46 |     2 | flags       | `u16`          | bit0 = expire_on_silence (SetFairValue/SetBias only); others must be 0 |
|     48 |    16 | _pad        | `[u8; 16]`     | explicit, zeroed (enforced by shape check) |

### `ChannelEvent` — 64 bytes (8e; PMLR `slot_kind = 5` only, never rings)

Non-tick channel capture written by each ingress thread into its
per-venue event log (plan §6.5). BBO has no `ChannelId` — BBO flows as
`Tick` into the tick log.

| offset | bytes | field         | type           | notes                                   |
| -----: | ----: | ------------- | -------------- | --------------------------------------- |
|      0 |     8 | ts_ns         | `u64` NsTs     | ingress parse-complete time             |
|      8 |     4 | sym           | `u32` SymbolId | `SYMBOL_ID_NONE` for venue-global channels |
|     12 |     1 | venue         | `u8` VenueId   |                                         |
|     13 |     1 | channel       | `u8` ChannelId | 0=Trade 1=Book 2=Mark 3=Funding 4=Ticker 5=AssetCtx 6=AllMids 7=OutcomeMeta 8=PriceChange 9=TradeGap 10=BookGap (appended 2026-08-15, G1 remediation — gap-monitor pairing events: TradeGap `v0`=expected seq `v1`=observed seq; BookGap `v0`=expected prev\_change\_id (`i64::MIN` = awaiting snapshot) `v1`=observed prev\_change\_id. Emitted 1:1 with every runtime `gaps_total` increment so §6.6's pairing letter is checkable offline) |
|     14 |     2 | _pad0         | `[u8; 2]`      | explicit, zeroed                        |
|     16 |     8 | venue_seq     | `u64`          | full-width venue seq; 0 where none      |
|     24 |     8 | venue_time_ms | `u64`          | venue timestamp ms; 0 where absent      |
|     32 |     8 | v0            | `i64`          | channel-dependent (px ×1e6, counts, …)  |
|     40 |     8 | v1            | `i64`          | channel-dependent (qty ×1e6, rate ×1e9, …) |
|     48 |    16 | _pad1         | `[u8; 16]`     | explicit, zeroed                        |

## Capture files (8e)

Per-run capture directory `<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`,
per-venue files written by the owning ingress thread
(`core_io::PmlrCapture`): `<venue>-ticks.pmlr` (kind 0),
`<venue>-events.pmlr` (kind 5), `<venue>-signals.pmlr` (kind 1;
header-only on venues that emit none), and optionally
`<venue>-raw.tap`. Staged writes flush at least every 1 s
(`CAPTURE_FLUSH_INTERVAL_NS`).

### Raw tap — `<venue>-raw.tap`

Bounded byte-exact inbound-payload capture for parser-vs-wire
differential audits (`--raw-tap`; off in production). Header, 64 B:

| offset | bytes | field    | notes                          |
| -----: | ----: | -------- | ------------------------------ |
|      0 |     4 | magic    | `b"PMRT"`                      |
|      4 |     2 | version  | `u16` — current = 1            |
|      6 |     1 | venue    | `u8` VenueId; 0xFF = unset     |
|      7 |     1 | _pad     |                                |
|      8 |     8 | epoch_ns | wall-clock ns at file open     |
|     16 |    48 | _reserved|                                |

Records, back-to-back, variable length:
`[ts_ns u64][len u32][flags u32][payload len B]` — `flags` bit 0 set ⇒
parser rejected the payload. The file is budget-bounded at capture
time; a torn final record (crash mid-write) is detected by readers and
terminates iteration.

## Replay log

**TODO** — pinned here in Phase 1. Provisional shape:

```
[header: 64 B] [slot: 64 B] [slot: 64 B] ...
```

Header:

| offset | bytes | field        | notes                                    |
| -----: | ----: | ------------ | ---------------------------------------- |
|      0 |     4 | magic        | `b"PMLR"`                                |
|      4 |     2 | version      | `u16` — current = **2**; readers accept ≤ 2 |
|      6 |     1 | slot_kind    | `u8` — 0=Tick, 1=Signal, 2=Fill, 3=Order, 4=AiCmd (8f §8.4), 5=ChannelEvent (8e) |
|      7 |     1 | _pad0        |                                          |
|      8 |     8 | epoch_ns     | wall-clock ns at file open               |
|     16 |    48 | _reserved    |                                          |

Version history: v1 = Phase-1 layouts (no venue byte; tail padding
implicit — those bytes are undefined in v1 files). v2 = Phase-8a
layouts above. Migration notes: `docs/migration.md`.

No framing bytes between slots; the file size modulo 64 is a corruption
check. Readers mmap the file and index by slot number.
