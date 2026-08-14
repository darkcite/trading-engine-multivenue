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

### `ChannelEvent` — 64 bytes (8e; PMLR `slot_kind = 5` only, never rings)

Non-tick channel capture written by each ingress thread into its
per-venue event log (plan §6.5). BBO has no `ChannelId` — BBO flows as
`Tick` into the tick log.

| offset | bytes | field         | type           | notes                                   |
| -----: | ----: | ------------- | -------------- | --------------------------------------- |
|      0 |     8 | ts_ns         | `u64` NsTs     | ingress parse-complete time             |
|      8 |     4 | sym           | `u32` SymbolId | `SYMBOL_ID_NONE` for venue-global channels |
|     12 |     1 | venue         | `u8` VenueId   |                                         |
|     13 |     1 | channel       | `u8` ChannelId | 0=Trade 1=Book 2=Mark 3=Funding 4=Ticker 5=AssetCtx 6=AllMids 7=OutcomeMeta 8=PriceChange |
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
|      6 |     1 | slot_kind    | `u8` — 0=Tick, 1=Signal, 2=Fill, 3=Order, 4=**reserved** (AiCmd, Stage 2 §8.4), 5=ChannelEvent (8e) |
|      7 |     1 | _pad0        |                                          |
|      8 |     8 | epoch_ns     | wall-clock ns at file open               |
|     16 |    48 | _reserved    |                                          |

Version history: v1 = Phase-1 layouts (no venue byte; tail padding
implicit — those bytes are undefined in v1 files). v2 = Phase-8a
layouts above. Migration notes: `docs/migration.md`.

No framing bytes between slots; the file size modulo 64 is a corruption
check. Readers mmap the file and index by slot number.
