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

### `Tick` — 64 bytes (v3 since VT1, 2026-09-03 — `docs/venue-time-capture-plan.md` §3)

| offset | bytes | field         | type             | notes                                  |
| -----: | ----: | ------------- | ---------------- | -------------------------------------- |
|      0 |     8 | ts_ns         | `u64` NsTs       | monotonic ns from `core-time::now_ns` — the ordering key everywhere; venue time never re-times a record |
|      8 |     4 | sym           | `u32` SymbolId   | venue-namespaced; `SYMBOL_ID_NONE = u32::MAX` invalid |
|     12 |     4 | venue_seq     | `u32`            | venue-provided sequence, monotonic     |
|     16 |     8 | bid_px        | `i64` Price      | fixed-point ×1e6                       |
|     24 |     8 | bid_qty       | `i64` Qty        | fixed-point ×1e6                       |
|     32 |     8 | ask_px        | `i64` Price      | fixed-point ×1e6                       |
|     40 |     8 | ask_qty       | `i64` Qty        | fixed-point ×1e6                       |
|     48 |     1 | venue         | `u8` VenueId     | producing venue (v2+; garbage in v1)   |
|     49 |     1 | flags         | `u8`             | v3: bit0 `TICK_FLAG_STALE` (the ingress judged the quote stale against the venue's `stale_after_ms` — captured, but never a signal, a fill, or a mark), bit1 `TICK_FLAG_VENUE_TIME_SENTINEL` (venue time inherited from the connection's sentinel stream — Binance spot `aggTrade` — not this message). v2: zeroed pad; v1: garbage |
|     50 |     6 | _pad          | `[u8; 6]`        | explicit, zeroed                       |
|     56 |     8 | venue_time_ms | `u64`            | v3: venue timestamp ms (venue clock); 0 = unknown (VT2 lists the per-venue source field). v2: zeroed pad; v1: garbage |

Constructors: `Tick::new` (venue time unknown, flags 0 — the v2 shape,
used by tests/replay/synthetic ticks) and `Tick::new_stamped` (VT2
ingress parsers). Readers gate every staleness judgement on the file's
header version (`PmlrReader::has_venue_time`, `pmlr.Reader.has_venue_time`):
a v2 file replays under the v2 law — never stale.

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
|     42 |     6 | _pad1       | `[u8; 6]`      | explicit, zeroed                |
|     48 |     8 | ttl_ns      | `u64`          | I1 (2026-09-03): time-to-live relative to `ts_ns`; 0 = none. A MODEL field — the offline fill law (`backtest::fill`) cancels the order at the first record of its sym at/after `ts_ns + ttl_ns` (an IoC that meets no fresh tick before its emitting bar closes is a cancel); no engine cancel path reads it (Stage-3). Wire-additive: every Order persisted before I1 carries 0 here (the bytes were explicit zeroed padding) |
|     56 |     8 | _pad2       | `[u8; 8]`      | explicit, zeroed                |

`kind` semantics in the offline fill model (I1): 0 = post-only maker
(strict-cross fill at P, maker fee); 1 = IoC taker (judged once at the
first fresh two-sided tick at/after `t_emit + Δ_venue`: fills at that
tick's touch iff marketable, capped by displayed size, remainder
cancels, taker fee); 2 = reserved (unroutable in the model).

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
|     40 |     1 | kind        | `u8` AiCmdKind | 0=Heartbeat 1=EnableStrategy 2=DisableStrategy 3=SetFairValue 4=SetBias 5=SetParam 6=OrderIntent 7=RulesetStage 8=RulesetCommit 9=HaltRequest — **no Resume exists** (halt is sticky) — 10=FundingSeed 11=PositionSeed (appended 2026-08-29, VM2 V1 per D-1/D-2: FundingSeed = one historical funding PRINT — `sym`=instrument, `px`=raw per-print rate ×1e9 signed (NOT ×1e6 — the hash128-in-px precedent for kind-specific field meaning), `qty`=venue print time ms > 0, `strategy_id`=5 (vm), everything else zero/`0xFF`; the engine folds it into the SAME per-sym funding windows the live Funding-event path feeds, so the `funding_print_divisor` cadence law applies in one place. Seeds carry raw prints, not window aggregates — strictly more general than the plan's "window slot" sketch, refinement noted in vm2-plan §8. PositionSeed = restore one v2 row's position after restart — `param_id`=row index < 256, `sym`=the row's action sym (consume-time cross-check against the committed row; mismatch refuses), `side`=entered side (0/1 required), `px`=entry px ×1e6 > 0, `qty`=position AGE in SECONDS ≥ 0 (engine derives entry = now − age·1e9; no wall-clock crossing), `ttl_ns`=0 ENFORCED — the engine drain expires ANY kind with `ttl_ns ≠ 0`, so age can never ride there; entry QTY is not carried — the vm re-derives it from the committed row's sizing law at the seeded px, so restores respect the CURRENT caps. `strategy_id`=5. The worker's post-boot waiter sends PositionSeeds only after verifying the #7b re-commit landed the expected hash — the seed itself carries no table identity by design) |
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
|     13 |     1 | channel       | `u8` ChannelId | 0=Trade (VT2, 2026-09-03: Binance SPOT prints now emitted by the aggTrade sentinel — `venue_seq`=aggregate id, `venue_time_ms`=`T`, `v0`=px ×1e6, `v1`=qty ×1e6 negated when `m:true` (aggressor sold), the cross-venue sign convention; capture only) 1=Book 2=Mark 3=Funding 4=Ticker 5=AssetCtx 6=AllMids 7=OutcomeMeta 8=PriceChange 9=TradeGap 10=BookGap (appended 2026-08-15, G1 remediation — gap-monitor pairing events: TradeGap `v0`=expected seq `v1`=observed seq; BookGap `v0`=expected prev\_change\_id (`i64::MIN` = awaiting snapshot) `v1`=observed prev\_change\_id. Emitted 1:1 with every runtime `gaps_total` increment so §6.6's pairing letter is checkable offline) 11=SubDrop (appended 2026-08-29, WS2 — non-fatal subscribe drop on a reconnect session: `sym`=dropped instrument (`SYMBOL_ID_NONE` when the venue error names none), `venue_seq`=0, `venue_time_ms`=0, `v0`=venue numeric error code (0 = missing-from-echo), `v1`=venue-local channel discriminant (−1 = unknown / folded Deribit option row; Deribit static rows carry the CHANNEL\_ORDER index 0=quote 1=ticker 2=trades 3=book). Emitted 1:1 with every `sub_drops_total` increment — same §6.6 pairing contract as the gap events) 12=VolIndex (appended 2026-08-29, WS6 — Deribit DVOL `deribit_volatility_index.{index}`: venue-GLOBAL series, `sym`=`SYMBOL_ID_NONE`, `venue_time_ms`=venue ts, `v0`=volatility POINTS ×1e9 (59.18 → 59\_180\_000\_000), `v1`=0-based ordinal of the index in the boot-configured `[deribit] options_underlyings` list — the boot log + universe file are the ordinal's resolution) |
|     14 |     2 | _pad0         | `[u8; 2]`      | explicit, zeroed                        |
|     16 |     8 | venue_seq     | `u64`          | full-width venue seq; 0 where none. WS3 (2026-08-29) exception: HL AssetCtx rows (channel 5) carry `premium` ×1e9 BIT-CAST `i64`→`u64` here (the ctx has no venue seq; pre-WS3 rows are a constant 0 — the M4 hash128-in-px/qty packing precedent) |
|     24 |     8 | venue_time_ms | `u64`          | venue timestamp ms; 0 where absent      |
|     32 |     8 | v0            | `i64`          | channel-dependent (px ×1e6, counts, …). Funding rows (channel 3): rate ×1e9 on OKX **and** (WS3) Deribit — the Deribit perp ticker's `current_funding` now emits a paired Funding row (OKX keeps `v1` = next-funding ms) |
|     40 |     8 | v1            | `i64`          | channel-dependent (qty ×1e6, rate ×1e9, …). Mark rows (channel 2): WS5 — Binance `@markPrice` rows carry the INDEX price ×1e6 here (OKX Mark rows keep 0); Funding rows: next-funding ms on OKX **and** Binance; Deribit — VM2 V2 (was a constant 0): `funding_8h` ×1e9 from the same ticker frame, the SAME 8-hour rolling series the worker's REST lane samples hourly (`interest_8h`), so the vm's hourly deribit funding sample and `carry_signal`'s ÷8-law windows accumulate one series; 0 when the frame lacked the field (pre-V2 captures replay with the `current_funding` fallback). VM2 V2 lane note: Hyperliquid FUNDING rides its AssetCtx rows (channel 5, `v0` = rate ×1e9 — there is no HL Funding channel), so the HL ingress gained its venue-event lane with spawn mask `EVENT_LANE_FUNDING \| EVENT_LANE_ASSET_CTX` |
|     48 |    16 | _pad1         | `[u8; 16]`     | explicit, zeroed                        |

### `OptSummary` — 64 bytes (M2.3; PMLR `slot_kind = 6` + VM2 V2 opt lanes)

Options analytics record (mvp-plan §4-M2.3/§9.8): one record per venue
push, appended by the owning ingress thread to its per-venue
`<venue>-opt-summary.pmlr`. Capture-only through M5; **VM2 V2: the
record now ALSO rides a per-venue options-summary SPSC lane into the
engine** (`Ring<OptSummary, OPT_RING_SIZE = 4096>`,
`engine::opt_lane_of`: OKX 0, Deribit 1, Binance-eapi 2 — the BN lane
exists venue-dark so the `.env` heal activates it with no engine
change) → `Strategy::on_opt_summary` → the vm feature engine's
mark/IV features. Capture stays FIRST at every emit site (§6.5
capture-before-push law); full-ring pushes count
`opt_ring_drops_total`. Offline consumers unchanged.
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

### `RuleRowV2` — 128 bytes (VM2 V1, table version 2; in-process only)

One validated rule of the VM2 general grammar (vm2-plan §1/§3,
D-1…D-8 ruled + LOCKED 2026-08-29). Same contract as the v1 row:
**built** by the v2 validator from the JSON artifact, never parsed
from wire bytes, never captured; identity is the table's `hash128`.
Two cache lines. v1 sugar maps FULLY onto this shape at build time
with byte-exact v1 semantics (`level_breach` bid → `LhsOnly(Ask) ≤
level`, ask → `LhsOnly(Bid) ≥ level`; `cross_deviation` →
`|DiffBps(Mid, Mid)| ≥ edge` with the mean-reverting direction law
and `side` as filter; both horizon-refire mode) — the evaluator has
ONE path, no v1 branch.

**Signal domain law**: every feature/combine output is an `i64` in
×1e9 of its natural unit — prices px ×1e9, APR/IV/imbalance fractions
×1e9, bps ×1e9, notional USD ×1e9, clock seconds ×1e9. Thresholds
live in the same domain. `FeatId`: 0=Mid 1=Bid 2=Ask 3=RollMean
4=RollEma 5=RollMin 6=RollMax 7=RollStd 8=Apr24 9=Apr72 10=MarkPx
11=MarkIv 12=DepthImb 13=DepthSpreadBps 14=DepthNearNotional
15=ClockToFunding 16=ClockUtcSod; `0xFF` = none (feat_c). `CombineOp`:
0=Diff 1=DiffBps 2=Ratio1e9 3=LhsOnly (the CONST-operand form —
`ref_sym` = `SYMBOL_ID_NONE`). `cmp_bits`: bit0 entry ≤ (clear = ≥),
bit1 entry abs, bit2 confirm ≤, bit3 confirm abs, bit4 confirm-pair
(confirm = the row's combine over `feat_c` across both legs; clear =
`feat_c(sym)` alone). `flags`: bit0 = position mode (Flat→Entered
state machine, two-leg emit when `ref_sym` real, universal exit law
`signal × entry_sign ≤ exit_1e9`, min/max-hold honored); clear = v1
horizon-refire (then `exit_1e9`/`min_hold_s`/`max_hold_s`/`group`
must be 0 — rule 9). `group`: rows sharing a byte hold at most ONE
position (first qualifying row in table order enters); `0xFF` =
ungrouped. Funding accumulation applies
`core_types::funding_print_divisor` (Deribit ÷8 — the R4-§9 law's
single home).

| offset | bytes | field        | type           | notes                                        |
| -----: | ----: | ------------ | -------------- | -------------------------------------------- |
|      0 |     1 | ver          | `u8`           | always 2 (`RULE_ROW_VER_2`); 0 = inert filler |
|      1 |     1 | flags        | `u8`           | bit0 = position mode (`ROW_FLAGS_MASK`)      |
|      2 |     1 | side         | `u8`           | `Side` (0/1) or `0xFF` both — emitted side for LhsOnly rows, direction filter for signal-signed rows |
|      3 |     1 | group        | `u8`           | exclusivity group; `0xFF` = ungrouped        |
|      4 |     1 | feat_a       | `u8` FeatId    | action-sym operand                           |
|      5 |     1 | feat_b       | `u8` FeatId    | reference operand; 0 (unused) for LhsOnly    |
|      6 |     1 | feat_c       | `u8` FeatId    | confirm feature or `0xFF`                    |
|      7 |     1 | combine      | `u8` CombineOp |                                              |
|      8 |     4 | sym          | `u32` SymbolId | action leg (descriptor-resolved at commit, D-6) |
|     12 |     4 | ref_sym      | `u32` SymbolId | reference leg or `SYMBOL_ID_NONE`            |
|     16 |     2 | win_a        | `u16`          | minutes, \[1, 4320\] iff feat_a is a Roll*, else 0 |
|     18 |     2 | win_b        | `u16`          | same law for feat_b                          |
|     20 |     2 | win_c        | `u16`          | same law for feat_c                          |
|     22 |     1 | cmp_bits     | `u8`           | `CMP_BITS_MASK` = 0x1F                       |
|     23 |     1 | _pad0        | `u8`           | explicit, zeroed                             |
|     24 |     8 | enter_1e9    | `i64`          | entry threshold, signal domain               |
|     32 |     8 | exit_1e9     | `i64`          | position-mode exit threshold (universal law) |
|     40 |     8 | confirm_1e9  | `i64`          | confirm threshold; 0 when feat_c = `0xFF`    |
|     48 |     4 | min_hold_s   | `u32`          | position mode: exit evaluates only after     |
|     52 |     4 | horizon_ms   | `u32`          | refire cooldown (v1 law) / entry re-arm      |
|     56 |     4 | edge_bps     | `u32`          | v1-sugar diagnostic mirror; 0 native rows    |
|     60 |     4 | _pad1        | `u32`          | explicit, zeroed                             |
|     64 |     8 | max_risk_1e6 | `i64`          | per-row (per-LEG in position mode) notional cap ×1e6 |
|     72 |     8 | name_h       | `u64`          | FNV-1a 64 of the row name                    |
|     80 |     4 | max_hold_s   | `u32`          | position age-out exit (S1 law); 0 = none     |
|     84 |     1 | family       | `u8`           | `MarketFamily` — reporting only              |
|     85 |     3 | _pad2        | `[u8; 3]`      | explicit, zeroed                             |
|     88 |    40 | _pad3        | `[u8; 40]`     | explicit, zeroed (reserved)                  |

### `RuleTableV2` / `RuleTableSlot` — 32 832 bytes (VM2; the §6 handoff ring's slot since V4, never captured)

256 × 128 B rows (32 KiB) + one trailing metadata cache line —
identical metadata layout to the retired v1 table at the 32 KiB
base. The engine accepts BOTH artifact grammar versions (v1 JSON
maps onto v2 rows at build); in-memory there is ONE row format. The
§6 documented copies grew to 32 832 B each at operator cadence.

### Ruleset JSON grammar v2 (VM2 V4; `~/multivenue/artifacts/rulesets/<hash128>.json`)

A `rows` array whose rows are v1-shaped (the 8g sugar — raw `sym`
ints + `trigger` objects, validated against the boot sym universe;
commit-able through the D-6 one-release compat arm) or v2-shaped —
the two shapes may coexist in one artifact but never mix in one row.
A v2 row:

- `name` (required; rule 5), `family` (optional, default `other`),
  `side` (`bid`|`ask`|`both`, default `both`).
- `instrument` (required) and `ref` (optional): **§9.4 DESCRIPTOR
  strings** (`okx:BTC-USDT-SWAP`, `binance-usdm:btcusdt`,
  `deribit:BTC-…-C`, bare PM token ids) resolved at STAGE time
  against the LIVE boot universe (the bin's DescriptorTable, built
  from the same allocation truth as `instrument-manifest.tsv`);
  unresolvable ⇒ REFUSE (`Descriptor`). #7b re-commit re-resolves
  every boot — this is what makes options tradeable across ordinal
  reshuffles.
- `feature` (required) / `ref_feature` (default = `feature`):
  `mid bid ask roll_mean roll_ema roll_min roll_max roll_std apr24
  apr72 mark_px mark_iv depth_imb depth_spread_bps depth_notional
  clock_to_funding clock_utc_sod`. `window_min`/`ref_window_min`/
  `confirm_window_min` ∈ [1, 4320], REQUIRED for `roll_*` features
  and FORBIDDEN otherwise (rule 3's window law; `Feature` reject).
- `combine` (`diff`|`diff_bps`|`ratio`): required WITH `ref`,
  forbidden without (a ref-less row is `LhsOnly` — the CONST form).
- `enter` (required), `exit`, `confirm`: decimals in NATURAL units
  parsed at 9-decimal precision (funding rates survive); `cmp`
  (`ge`|`le`, default `ge`) + `abs` (default false) shape the entry;
  `confirm_cmp`/`confirm_abs`/`confirm_pair` shape the confirm
  (`confirm_pair` = the row's combine over `confirm_feature` across
  BOTH legs — the S1 72 h-spread shape; requires `ref`).
- **Rule 9 (`Position` reject)**: `exit` present ⇔ the row is a
  POSITION row; `group` (0–254), `min_hold_s`, `max_hold_s` require
  it; `max_hold_s > min_hold_s` when both set. Ref-full position
  rows emit two legs (`max_risk_usd` is PER LEG; rule 7 charges both
  legs' syms and the table total accordingly).
- **Rule 10 (`Feature` reject)**: every referenced feature must
  exist for its resolved instrument's channels (capability bits from
  the bin's lane truth — no depth rows on a sym without depth, no
  funding features on spot, no IV off options); the table's distinct
  rolling (sym, window) pairs must fit the feature engine's bind
  budget (≤ 8 windows/sym, ≤ 256 pairs).
- `horizon_ms` [10, 86 400 000] (required): refire cooldown on
  refire rows, re-entry cooldown after position exits.
- Rule 8 (v2 identity): duplicate `(instrument, ref, features,
  windows, combine, cmp bits, mode, group, enter)` rejects.

| offset | bytes | field   | type               | notes                             |
| -----: | ----: | ------- | ------------------ | --------------------------------- |
|      0 | 32768 | rows    | `[RuleRowV2; 256]` | only `rows[..len]` is meaningful  |
|  32768 |     4 | len     | `u32`              | validated row count               |
|  32772 |     4 | epoch   | `u32`              | side-path monotonic stage counter |
|  32776 |    16 | hash128 | `[u8; 16]`         | artifact identity                 |
|  32792 |    40 | _pad    | `[u8; 40]`         | explicit, zeroed                  |

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
|      4 |     2 | version      | `u16` — current = **3** (VT1); readers accept ≤ 3 |
|      6 |     1 | slot_kind    | `u8` — 0=Tick, 1=Signal, 2=Fill, 3=Order, 4=AiCmd (8f §8.4), 5=ChannelEvent (8e), 6=OptSummary (M2.3), 7=DepthTopK (WS10-B) |
|      7 |     1 | _pad0        |                                          |
|      8 |     8 | epoch_ns     | wall-clock ns at file open               |
|     16 |    48 | _reserved    |                                          |

Version history: v1 = Phase-1 layouts (no venue byte; tail padding
implicit — those bytes are undefined in v1 files). v2 = Phase-8a
layouts above. v3 = VT1 (2026-09-03): `Tick.flags` at 49 and
`Tick.venue_time_ms` at 56 spend the v2 tail pad; every other kind is
byte-identical to v2 (the header version is one number for all kinds,
so it bumps once). Migration notes: `docs/migration.md`.

No framing bytes between slots. **Slot size is KIND-determined since
WS10-B** (kinds 0–6: 64 B; kind 7: 192 B — always a 64-multiple);
the file size modulo the kind's slot size is a corruption check.
Readers decode `slot_kind` from the header first, then mmap-index by
slot number at that stride. Files written before WS10-B are
unaffected (no kind changed size).
