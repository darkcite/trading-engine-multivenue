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

### `Order` — 64 bytes (layout amended M4.1, pre-first-capture — see docs/migration.md)

Captured to `engine-orders.pmlr` (`slot_kind = 3`) since M4.1 — the
per-run strategy-intent log (`audit-pnl` input, §9.9). Before M4.1 no
Order slot was ever persisted, so the `strategy_id` addition has zero
reader-compat surface.

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
|     41 |     1 | strategy_id | `u8`           | M4.1: emitting strategy-set slot (0=latency-arb 1=ev 2=cross-arb 3=rule-tree 4=ai-exec 5=vm), stamped by the set's `StampCtx`; `0xFF` = unattributed (bare boots). Per-ruleset attribution is NOT embedded — join vm orders (slot 5) against the ai-cmds `RulesetCommit` timeline |
|     42 |    14 | _pad1       | `[u8; 14]`     | explicit, zeroed                |
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
`docs/arch/phase-8f-design.md` §3 and enforced by
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
|     13 |     1 | channel       | `u8` ChannelId | 0=Trade 1=Book 2=Mark 3=Funding 4=Ticker 5=AssetCtx 6=AllMids 7=OutcomeMeta 8=PriceChange 9=TradeGap 10=BookGap (appended 2026-08-15, G1 remediation — gap-monitor pairing events: TradeGap `v0`=expected seq `v1`=observed seq; BookGap `v0`=expected prev\_change\_id (`i64::MIN` = awaiting snapshot) `v1`=observed prev\_change\_id. Emitted 1:1 with every runtime `gaps_total` increment so §6.6's pairing letter is checkable offline) 11=SubDrop (appended 2026-08-29, WS2 — non-fatal subscribe drop on a reconnect session: `sym`=dropped instrument (`SYMBOL_ID_NONE` when the venue error names none), `venue_seq`=0, `venue_time_ms`=0, `v0`=venue numeric error code (0 = missing-from-echo), `v1`=venue-local channel discriminant (−1 = unknown / folded Deribit option row; Deribit static rows carry the CHANNEL\_ORDER index 0=quote 1=ticker 2=trades 3=book). Emitted 1:1 with every `sub_drops_total` increment — same §6.6 pairing contract as the gap events) 12=VolIndex (appended 2026-08-29, WS6 — Deribit DVOL `deribit_volatility_index.{index}`: venue-GLOBAL series, `sym`=`SYMBOL_ID_NONE`, `venue_time_ms`=venue ts, `v0`=volatility POINTS ×1e9 (59.18 → 59\_180\_000\_000), `v1`=0-based ordinal of the index in the boot-configured `[deribit] options_underlyings` list — the boot log + universe file are the ordinal's resolution) |
|     14 |     2 | _pad0         | `[u8; 2]`      | explicit, zeroed                        |
|     16 |     8 | venue_seq     | `u64`          | full-width venue seq; 0 where none. WS3 (2026-08-29) exception: HL AssetCtx rows (channel 5) carry `premium` ×1e9 BIT-CAST `i64`→`u64` here (the ctx has no venue seq; pre-WS3 rows are a constant 0 — the M4 hash128-in-px/qty packing precedent) |
|     24 |     8 | venue_time_ms | `u64`          | venue timestamp ms; 0 where absent      |
|     32 |     8 | v0            | `i64`          | channel-dependent (px ×1e6, counts, …). Funding rows (channel 3): rate ×1e9 on OKX **and** (WS3) Deribit — the Deribit perp ticker's `current_funding` now emits a paired Funding row (`v1` = 0: continuous funding, no next-funding time; OKX keeps `v1` = next-funding ms) |
|     40 |     8 | v1            | `i64`          | channel-dependent (qty ×1e6, rate ×1e9, …). Mark rows (channel 2): WS5 — Binance `@markPrice` rows carry the INDEX price ×1e6 here (OKX Mark rows keep 0); Funding rows: next-funding ms on OKX **and** Binance, 0 on Deribit |
|     48 |    16 | _pad1         | `[u8; 16]`     | explicit, zeroed                        |

### `OptSummary` — 64 bytes (M2.3; PMLR `slot_kind = 6` only, never rings)

Options analytics record (mvp-plan §4-M2.3/§9.8): one record per venue
push, appended by the owning ingress thread to its per-venue
`<venue>-opt-summary.pmlr`. CAPTURE-ONLY — never enters the engine
ring; consumers are offline (audit-replay, the strategist digest).
Fed by Deribit option `ticker.{instr}.100ms` and OKX `opt-summary`
(BN eapi joins at M2.4). Values are RAW VENUE UNITS fixed-point
(Deribit option mark px is coin-denominated); IV is a FRACTION ×1e9
(Deribit's percent wire value normalized /100); greeks are
Black-Scholes-style (Deribit `greeks.*`, OKX `*BS`) with SATURATING
i32 conversion — a value equal to the i32 bound means saturation.
`flags` records venue-optional fields: bit0 = mark_px supplied,
bit1 = open_interest supplied (OKX `opt-summary` carries neither in
M2.3 — both 0 with flags 0; its `fwdPx` fills `underlying_px_1e9`).

| offset | bytes | field             | type           | notes                                    |
| -----: | ----: | ----------------- | -------------- | ---------------------------------------- |
|      0 |     8 | ts_ns             | `u64` NsTs     | ingress parse-complete time              |
|      8 |     4 | sym               | `u32` SymbolId | option sym (base-512 options block)      |
|     12 |     1 | venue             | `u8` VenueId   |                                          |
|     13 |     1 | flags             | `u8`           | bit0 mark_px, bit1 open_interest         |
|     14 |     2 | _pad0             | `[u8; 2]`      | explicit, zeroed                         |
|     16 |     8 | mark_px_1e9       | `i64`          | raw venue units ×1e9; 0 if flag absent   |
|     24 |     8 | mark_iv_1e9       | `i64`          | IV fraction ×1e9                         |
|     32 |     8 | underlying_px_1e9 | `i64`          | Deribit `underlying_price`; OKX `fwdPx`  |
|     40 |     8 | open_interest_1e6 | `i64`          | raw venue units ×1e6; 0 if flag absent   |
|     48 |     4 | delta_1e9         | `i32`          | BS delta ×1e9 (exact; \|δ\| ≤ 1)         |
|     52 |     4 | gamma_1e9         | `i32`          | BS gamma ×1e9, saturating                |
|     56 |     4 | vega_1e6          | `i32`          | BS vega ×1e6, saturating                 |
|     60 |     4 | theta_1e6         | `i32`          | BS theta ×1e6, saturating                |

### `DepthTopK` — 192 bytes (WS10-B; ring + PMLR `slot_kind = 7`)

Top-K L2 depth snapshot: the in-ingress bounded ladder (OKX `books`,
Deribit `book.100ms`; `book_builder::ladder`, cap 64 levels/side)
emits one on every book update whose top-K actually CHANGED
(byte-compare gate — levels + flags, not timestamps), onto the
per-venue depth ring (`Ring<DepthTopK, 4096>`, engine `on_depth`) AND
into `<venue>-depth.pmlr`. **First non-64-byte PMLR kind: slot size
is KIND-determined since WS10-B — kinds 0–6 stay 64 B, kind 7 is
192 B (three cache lines; still a 64-multiple, so mmap'd access
stays aligned).** On a venue seq-chain break the ladder clears and a
`flags = 1` (STALE) snapshot always emits — a strategy must never
trade a known-broken book; the first post-resync snapshot arrives
with flags 0. `bids` best-first descending, `asks` best-first
ascending; slots past the book's real depth are all-zero.

| offset | bytes | field | type                | notes                                |
| -----: | ----: | ----- | ------------------- | ------------------------------------ |
|      0 |     8 | ts_ns | `u64` NsTs          | ingress apply-complete time          |
|      8 |     4 | sym   | `u32` SymbolId      |                                      |
|     12 |     1 | venue | `u8` VenueId        | Okx / Deribit in v1                  |
|     13 |     1 | k     | `u8`                | always 5 (`DEPTH_K`)                 |
|     14 |     1 | flags | `u8`                | bit0 = STALE (book resyncing)        |
|     15 |     1 | _pad0 | `u8`                | explicit, zeroed                     |
|     16 |    80 | bids  | `[(i64,i64); 5]`    | (px ×1e6, qty ×1e6) best-first desc  |
|     96 |    80 | asks  | `[(i64,i64); 5]`    | (px ×1e6, qty ×1e6) best-first asc   |
|    176 |    16 | _pad1 | `[u8; 16]`          | explicit, zeroed                     |

### `RuleRow` — 64 bytes (8g; row of `RuleTable`, in-process only)

One validated rule of an operator-committed ruleset (8g design §3).
**Built** by the `ingress-ai` validator from the JSON artifact — never
parsed from wire bytes, never captured to PMLR (no `AsBytes`; the JSON
artifact is the durable form, identity is the table's `hash128`).
Offsets are compile-time asserted in `core-types`. The §3 pad count
was amended 13 → 21 in G1 (operator-confirmed): declared fields sum to
43 B and padding must be fully explicit.

| offset | bytes | field        | type           | notes                                        |
| -----: | ----: | ------------ | -------------- | -------------------------------------------- |
|      0 |     4 | sym          | `u32` SymbolId | action leg; validated against boot universe  |
|      4 |     4 | ref_sym      | `u32` SymbolId | `cross_deviation` reference leg; `SYMBOL_ID_NONE` for `level_breach`. D2 as amended: either leg = any asset on any boot-universe venue |
|      8 |     4 | edge_bps     | `u32`          | trigger threshold, bps; ≤ 10 000             |
|     12 |     4 | horizon_ms   | `u32`          | re-arm cooldown, ms; \[10, 86 400 000\]      |
|     16 |     8 | level_1e6    | `i64`          | `level_breach` px ×1e6, \[0, 1 000 000\]; 0 for `cross_deviation` |
|     24 |     8 | max_risk_1e6 | `i64`          | per-row notional cap ×1e6; ≤ risk-policy single-order cap (tighten-only) |
|     32 |     8 | name_h       | `u64`          | FNV-1a 64 of the row name (`core_types::fnv1a_64`); names live only in the artifact + worker registry |
|     40 |     1 | trigger      | `u8`           | 0 = cross_deviation, 1 = level_breach        |
|     41 |     1 | side         | `u8`           | `Side` (0/1) or `0xFF` = both                |
|     42 |     1 | family       | `u8`           | `MarketFamily` byte — reporting only         |
|     43 |    21 | _pad         | `[u8; 21]`     | explicit, zeroed                             |

### `RuleTable` / `RuleTableSlot` — 16 448 bytes (8g; `Ring<RuleTableSlot, 2>` only, never captured)

The engine-facing table: 256 rows (16 KiB) + one trailing metadata
cache line. `#[repr(C, align(64))]`, Copy. Ferried ingress→engine by
value at operator cadence (the two documented 16 KiB copies, design
§6); all fields native-endian in-process POD — the table never
crosses a process or byte-order boundary, so no serialization is
defined and none is captured.

| offset | bytes | field   | type              | notes                                  |
| -----: | ----: | ------- | ----------------- | -------------------------------------- |
|      0 | 16384 | rows    | `[RuleRow; 256]`  | only `rows[..len]` is meaningful       |
|  16384 |     4 | len     | `u32`             | validated row count, \[1, 256\] staged (0 = `EMPTY` boot value) |
|  16388 |     4 | epoch   | `u32`             | side-path monotonic stage counter      |
|  16392 |    16 | hash128 | `[u8; 16]`        | identity — first 16 B of the artifact's full SHA-256 (d5 filename convention) |
|  16408 |    40 | _pad    | `[u8; 40]`        | explicit, zeroed                       |

## Capture files (8e)

Per-run capture directory `<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`,
per-venue files written by the owning ingress thread
(`core_io::PmlrCapture`): `<venue>-ticks.pmlr` (kind 0),
`<venue>-events.pmlr` (kind 5), `<venue>-signals.pmlr` (kind 1;
header-only on venues that emit none),
`<venue>-opt-summary.pmlr` (kind 6, M2.3; header-only on venues
without an options lane — the uniform-file-set law),
`<venue>-depth.pmlr` (kind 7, WS10-B, 192 B slots; header-only
without a depth subscription), and optionally
`<venue>-raw.tap`. Staged writes flush at least every 1 s
(`CAPTURE_FLUSH_INTERVAL_NS`). Venue labels: `pm`, `bn`, `okx`,
`rpc`, `deribit`, `hl`, and — WS9, appended 2026-08-29 — `bybit`
(VenueId 6; spot + linear share one label the way `bn` covers
spot + usdm; Bybit trades carry `venue_seq` 0 — UUID trade ids, no
venue sequence, so the §6.2 chain law does not apply to this venue).
Bybit `Ticker` event rows carry `v0` = 0, `v1` = open interest ×1e6
(venue base/contract units) — unlike Deribit's mark+OI pairing.

Engine-side single-file sinks (`core_io::SlotCapture`) in the same
run dir: `engine-fills.pmlr` (kind 2, Phase 8f — every fill
dispatched to `Strategy::on_fill`, venue lanes + dispatcher pump),
`engine-orders.pmlr` (kind 3, M4.1 — every order ACCEPTED by the
dispatcher via `ctx.submit`; refusals are counters only), and
`ai-cmds.pmlr` (kind 4, written by `ingress-ai`).

### Options manifest — `options-manifest.tsv` (M2 close)

Per-run sidecar written ONCE by the bin at boot, after discovery,
mapping the boot's option SymbolIds to venue instrument names —
option ordinals are allocated per boot in selection order and
reshuffle across boots by design (chain roll), and options are
boot-discovered, so no universe-file lane can name them. Offline
venue+descriptor consumers (§9.4 law: the §9.8 IV digest, M4
shadow-P&L) resolve option syms through this file. UTF-8 text, one
line per selected instrument,
`<venue_label>\t<sym_u32_decimal>\t<instrument_name>\n`, where
`venue_label` is the venue's capture-file prefix (`deribit`, `okx`,
`bn`); no header line; present only when the boot selected ≥ 1
option instrument (absence = options-less or pre-M2-close run).
Readers parse strictly and skip-and-count malformed lines.

### Instrument manifest — `instrument-manifest.tsv` (M4.2, ruling D3)

The options manifest generalized to EVERY allocated instrument:
written once by the bin on EVERY boot (a boot always carries ≥ 1
instrument), `<sym_u32_decimal>\t<descriptor>\n` per line, where
`descriptor` is the FINAL §9.4 worker map-name string (PM token ids
bare; `binance:` / `binance-usdm:` / `okx:` / `deribit:` /
`hyperliquid:` for the static lanes — baked by
`core-config::universe` at allocation; options
`deribit:`/`okx:`/`binance-opt:` + instrument name). Emission order =
allocation order. This is the sym→descriptor resolution lane for
every offline venue+descriptor consumer (`audit-pnl`, the §9.8 IV
digest, M5 naming); `options-manifest.tsv` is kept ONE release for
pre-D3 readers and then retires. Absence = a pre-D3 run. Readers
parse strictly and skip-and-count malformed lines.

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
|      6 |     1 | slot_kind    | `u8` — 0=Tick, 1=Signal, 2=Fill, 3=Order, 4=AiCmd (8f §8.4), 5=ChannelEvent (8e), 6=OptSummary (M2.3), 7=DepthTopK (WS10-B) |
|      7 |     1 | _pad0        |                                          |
|      8 |     8 | epoch_ns     | wall-clock ns at file open               |
|     16 |    48 | _reserved    |                                          |

Version history: v1 = Phase-1 layouts (no venue byte; tail padding
implicit — those bytes are undefined in v1 files). v2 = Phase-8a
layouts above. Migration notes: `docs/migration.md`.

No framing bytes between slots. **Slot size is KIND-determined since
WS10-B** (kinds 0–6: 64 B; kind 7: 192 B — always a 64-multiple);
the file size modulo the kind's slot size is a corruption check.
Readers decode `slot_kind` from the header first, then mmap-index by
slot number at that stride. Files written before WS10-B are
unaffected (no kind changed size).
