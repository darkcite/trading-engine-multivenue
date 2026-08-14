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

### `Tick` — 64 bytes

| offset | bytes | field      | type             | notes                                  |
| -----: | ----: | ---------- | ---------------- | -------------------------------------- |
|      0 |     8 | ts_ns      | `u64` NsTs       | monotonic ns from `core-time::now_ns`  |
|      8 |     4 | sym        | `u32` SymbolId   | `SYMBOL_ID_NONE = u32::MAX` is invalid |
|     12 |     4 | venue_seq  | `u32`            | venue-provided sequence, monotonic     |
|     16 |     8 | bid_px     | `i64` Price      | fixed-point ×1e6                       |
|     24 |     8 | bid_qty    | `i64` Qty        | fixed-point ×1e6                       |
|     32 |     8 | ask_px     | `i64` Price      | fixed-point ×1e6                       |
|     40 |     8 | ask_qty    | `i64` Qty        | fixed-point ×1e6                       |
|     48 |    16 | _pad       | `[u8; 16]`       | alignment to 64 B                      |

### `Signal` — 64 bytes

| offset | bytes | field    | type           | notes                                  |
| -----: | ----: | -------- | -------------- | -------------------------------------- |
|      0 |     8 | ts_ns    | `u64` NsTs     | monotonic ns                           |
|      8 |     4 | sym      | `u32` SymbolId |                                        |
|     12 |     1 | class    | `u8`           | `LatencyClass` (Hot=0, Warm=1, Slow=2) |
|     13 |     1 | source   | `u8`           | enum per-ingress source identifier     |
|     14 |     2 | _pad0    | `[u8; 2]`      |                                        |
|     16 |    40 | payload  | `[u8; 40]`     | opaque; interpretation by source       |
|     56 |     8 | _pad1    | `[u8; 8]`      |                                        |

### `Fill` — 64 bytes

| offset | bytes | field     | type           | notes                           |
| -----: | ----: | --------- | -------------- | ------------------------------- |
|      0 |     8 | ts_ns     | `u64` NsTs     |                                 |
|      8 |     4 | sym       | `u32` SymbolId |                                 |
|     12 |     1 | side      | `u8`           | `Side` (Bid=0, Ask=1)           |
|     13 |     3 | _pad0     | `[u8; 3]`      |                                 |
|     16 |     8 | px        | `i64` Price    | fixed-point ×1e6                |
|     24 |     8 | qty       | `i64` Qty      | fixed-point ×1e6                |
|     32 |     8 | order_id  | `u64`          | engine-assigned client oid      |
|     40 |    16 | _pad1     | `[u8; 16]`     |                                 |
|     56 |     8 | _pad2     | `[u8; 8]`      |                                 |

### `Order` — 64 bytes

| offset | bytes | field       | type           | notes                           |
| -----: | ----: | ----------- | -------------- | ------------------------------- |
|      0 |     8 | ts_ns       | `u64` NsTs     | client creation ts              |
|      8 |     4 | sym         | `u32` SymbolId |                                 |
|     12 |     1 | side        | `u8`           | `Side`                          |
|     13 |     1 | kind        | `u8`           | 0=Limit, 1=IoC, 2=Market (rsv.) |
|     14 |     2 | _pad0       | `[u8; 2]`      |                                 |
|     16 |     8 | px          | `i64` Price    |                                 |
|     24 |     8 | qty         | `i64` Qty      |                                 |
|     32 |     8 | client_oid  | `u64`          | engine-assigned, monotonic      |
|     40 |    16 | _pad1       | `[u8; 16]`     |                                 |
|     56 |     8 | _pad2       | `[u8; 8]`      |                                 |

## Replay log

**TODO** — pinned here in Phase 1. Provisional shape:

```
[header: 64 B] [slot: 64 B] [slot: 64 B] ...
```

Header:

| offset | bytes | field        | notes                                    |
| -----: | ----: | ------------ | ---------------------------------------- |
|      0 |     4 | magic        | `b"PMLR"`                                |
|      4 |     2 | version      | `u16` — bumped on any slot change        |
|      6 |     1 | slot_kind    | `u8` — 0=Tick, 1=Signal, 2=Fill, 3=Order |
|      7 |     1 | _pad0        |                                          |
|      8 |     8 | epoch_ns     | wall-clock ns at file open               |
|     16 |    48 | _reserved    |                                          |

No framing bytes between slots; the file size modulo 64 is a corruption
check. Readers mmap the file and index by slot number.
