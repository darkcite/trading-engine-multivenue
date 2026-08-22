# MVP progress — Stage 2.5 M-phases (docs/mvp-completion-plan.md)

Session log for M1…M6. M0 (= 8h/H6b-SEMI close) lives in
`docs/phase-8h-progress.md` — that file is CLOSED history. Authority:
`docs/mvp-completion-plan.md` (§9 BINDING) → the latest entry here →
CLAUDE.md. Standing rulings: no `ANTHROPIC_API_KEY`/`serve` until
Stage 3; `.env` never read/printed (verbs source it — H6b wrapper
pattern); paper only; git ops only with operator ask; push anomaly
record-only; Mac-only cargo/pytest.

---

## 2026-08-22 — Session M1a (M1 OPEN): design + operator rulings + core-config universe module

**Operator GO for M1 recorded this session** (explicit, after the
Stage-2 closure notification). Scope: mvp-plan §4-M1 — universe config
file + Binance multi-symbol spot/USDS-M futures + Polymarket
multi-market + OKX/Deribit/HL lists into config. No new protocols; no
candles.db (M3); no options (M2); nothing Stage-3.

### Operator rulings taken this session (M1-R*)

- **M1-R1 (PM universe content):** Polymarket universe = **crypto
  binary up/down markets ONLY** ("if bitcoin will go up or down").
  Selection mechanism: **operator-curated list in the universe config**
  (boot stays deterministic and REST-free for PM selection; the top-N
  Gamma auto-refresh option was not chosen). Because crypto up/down
  markets are SHORT-DATED (daily), the curated list needs a refresh
  cadence: until M3's daily-restart automation, the recipe is
  semi-manual — resolve the current day's market token ids through the
  worker's existing Gamma lane (fetch consumer / ai-session §5 flow)
  and update `universe.toml`. M3 formalizes this beside the daily
  graceful restart. *(Flagged for veto: if the operator prefers
  boot-time slug→today's-ids resolution instead, that is a NEW Gamma
  REST parser in engine discovery — deferred unless he asks.)*
- **M1-R2 (config location):** default path
  **`~/multivenue/universe.toml`**; `--universe <path>` overrides.
  Absent file + no flag = legacy flag-driven boot, byte-identical.

### Design (binding for the M1 slices)

**1. Universe config file — core-config, new module `universe.rs`.**
Handwritten **TOML-subset** parser (boot-time; offline doctrine header
— allocations allowed; no serde, no external toml crate). Grammar:
`# comments`, `[section]` headers, `key = value` with value ∈ string |
bool | integer | array-of-strings (single-line or multiline arrays).
No inline tables, no dotted keys, no floats, no dates, no nesting.
**Unknown section or key = fatal parse error** (fail-fast beats a
silently ignored typo'd venue). Schema:

```toml
[polymarket]
# M1-R1: crypto up/down binaries only. Entry forms:
#   "<yes-id>:<no-id>"  — one market, YES/NO token pair
#   "<token-id>"        — one market, single token
markets = [
  "57748138085022719760345772310040703848567377822400132842014290209986511882046",
]

[binance]
spot = ["btcusdt", "ethusdt"]
usdm = ["btcusdt"]          # USDS-M futures (fstream host)

[okx]
instruments = ["BTC-USDT", "ETH-USDT-SWAP"]
depth = false               # §4.5 books channel (capture-only)

[deribit]
instruments = ["BTC-PERPETUAL"]
depth = false

[hyperliquid]
coins = ["BTC", "ETH"]

[pairs]
# latency-arb pairs, "P:B" = markets[P] × binance.spot[B] (0-based,
# config order; a YES/NO market entry resolves to its YES token).
map = ["0:0"]
```

Load rule: `--universe <path>` explicit, else the default path if the
file exists, else legacy flags. **Per-venue CLI flags OVERRIDE that
venue's config section when explicitly passed** (mvp-plan: flags remain
overrides). Venue-blind refusal preserved and extended: effective PM
list empty ⇒ refuse; M1 additionally requires ≥1 Binance spot symbol
(the latency-arb pair anchor — relaxing that is deliberately deferred).
Tests: happy/failure units per house rule + proptest (grammar
round-trip, rejection shapes) + **new fuzz target `universe_toml`**
(§21.3/§21.4 — it is a parser over file bytes; the rule is cheap to
honor).

**2. SymbolId allocation law (config-order ordinals, legacy anchors).**
`make_symbol_id(venue, ordinal)` = `venue<<24 | ordinal` (core-types).
- PM tokens flatten in list order (a pair entry contributes YES then
  NO). Token[0] → **42** (legacy anchor — keeps the H6 demo lineage,
  worker-map seeds, and latency-arb defaults coherent); token[i≥1] →
  `make_symbol_id(Polymarket, i+1)`.
- Binance `spot[0]` → **7** (legacy anchor, the clap-default mirror the
  worker knows); `spot[i≥1]` → `make_symbol_id(Binance, i+1)`;
  `usdm[j]` → `make_symbol_id(Binance, 512 + j + 1)` (**USDM ordinal
  base 512** — spot and futures ordinals disjoint by construction;
  venue byte stays Binance; ≤500 per list is far beyond need).
- OKX/Deribit/HL unchanged: `make_symbol_id(venue, i+1)`, list order.
- Boot builds the full universe table and **fails fast on any duplicate
  id** (the flat legacy anchors could only collide at absurd list
  sizes; the check makes it impossible silently).
- **Descriptors (§9.4 law — nothing persistent keys by bare SymbolId):**
  PM = token id; BN spot = `binance:<sym>`; BN futures =
  `binance-usdm:<sym>`; OKX/Deribit/HL = existing `venue:<instrument>`.

**3. Binance multi-symbol: ONE thread, N connections.** The venue
thread owns N `(Transport, Driver, host, path)` tuples — **one
connection per instrument** (mvp-plan) so the single-stream
`/ws/<sym>@bookTicker` parser stays byte-untouched — driven by a new
`run_multi` in `ingress-binance::run_loop`: one mio `Poll`,
per-connection tokens, per-connection keepalive + reconnect/backoff
mirroring `run()`'s semantics, `drive_one` per readiness event, **one
producer** (single-writer law holds: one thread owns the lane) + one
`"bn"` capture. Spot connections use `cfg.binance_ws_host`; USDS-M use
new `cfg.binance_fut_ws_host` (env `BINANCE_FUT_WS_HOST`, default
`fstream.binance.com`; `.env.example` gains the line). Discovery
(mvp-plan "discovery audit"): new BN arm in `boot_discovery` —
spot `/api/v3/exchangeInfo` + futures `/fapi/v1/exchangeInfo` symbol
validation, byte-scanner parser **with proptest + fuzz target** (house
rule, no exceptions), coverage gauge added.

**4. Polymarket multi-market: one connection, N-id subscribe.**
`write_market_subscribe` grows a multi-id form (the wire format is
already an array — today it carries one element); the run-loop `Driver`
holds the configured id list (preallocated, boot cap **64 markets**);
`SymbolMap::from_pairs` is already N-capable; `boot_discovery::run_all`
takes the id slice (existing per-id Gamma validation in a loop — no new
parser); `spawn_polymarket` takes the list. Capture/audit unchanged
(per-sym streams already render).

**5. OKX/Deribit/HL:** instrument lists move to config (flag override
per venue); zero crate changes.

**6. Engine pairs:** `EngineConfig.pairs` built from `[pairs] map`
(P:B ordinal refs → resolved SymbolIds); default `["0:0"]` when both
lists are non-empty; empty pair set = fatal in M1 (latency-arb anchor
required). `build_ai_universe` signature widens from two scalars to the
full id sets.

**7. Worker seeding (fetch seam; every frozen surface untouched).**
New env `CLAUDE_WORKER_UNIVERSE_FILE` read **at the fetch seam** (H3
precedent — the BaseConfig field tuple is frozen; no new field). When
set and the file exists: `fetch` seeds map names for every configured
PM token id (additive, conflict-reporting — the H6a-proven token-id
seed lane) and, once both sides of a YES/NO pair resolve to observed
syms, records the pair into the map's pair machinery (no fabrication;
unresolved = reported). Python: `import tomllib` (stdlib, full-import
rule). Tests additive; the frozen 202 and the 7-verb surface untouched.

**8. Explicitly NOT in M1:** candles.db / capture-catalog (M3), options
(M2), any serve/auto-lane work (Stage-3 gate), any executor/dispatcher
work (Stage 3), Binance combined-stream envelope parsing (rejected —
one-connection-per-instrument keeps the parser frozen).

### Slice plan

- **M1a (this session):** design (this entry) + `core-config`
  `universe.rs` (parser + schema + allocation law + tests + fuzz
  target) + `universe.toml.example`.
- **M1b:** cli plumbing — `--universe` flag, per-venue override
  semantics, refusal law, universe table + dup check, pairs,
  `build_ai_universe` widening; PM multi-market (subscribe writer,
  driver list, discovery loop, spawn).
- **M1c:** BN `run_multi` + futures host + BN discovery arm + spawn;
  coverage gauges.
- **M1d:** worker fetch-seam seeding + full-universe live smoke
  (pitfall #11) + gates + M1 close entry.

Exit (mvp-plan M1): ONE boot command runs the full non-options
universe; audit-replay shows every venue ticking; market-map resolves
every observed sym; gates green (nextest ≥1081 / alloc 36+ corrected
guard / pytest ≥354 / fuzz incl. new targets).

### M1a + M1b results (landed this session, all gates green)

**M1a — `core-config::universe` (NEW module).** TOML-subset parser
exactly per the design grammar (quote-aware comments — HIP-4 `"#330"`
literal pinned; multiline arrays; trailing commas; unknown
section/key fatal; no escapes; strings never span lines), typed model
(`Universe`, `PmMarket::Single|YesNo`), caps (64 PM markets / 500 per
venue list), within-list + cross-entry duplicate rejection, pair
range/dup validation. Allocation law implemented as designed:
`allocate` (+ `allocate_with_anchors` for the legacy `*-sym-id`
override lane), PM token[0]→42 / BN spot[0]→7 anchors, namespaced
ordinals elsewhere, USDM base 512, universe-wide id-collision
fail-fast (the 42-token anchor-collision case test-pinned), default
pair injection, `assert_bootable` refusal law. Tests: 29 new (27
units + 2 proptests: generated-config round-trip + never-panics).
**NEW fuzz target `universe_toml`** (parse + allocate on every
successful parse): built + run 60 s — **34.9 M execs, zero
findings**. `universe.toml.example` committed at repo root.

**M1b — PM multi-market + cli plumbing.**
- `ingress-polymarket`: `write_market_subscribe_multi` (N-id array
  frame; single-id writer now delegates — byte-identical output
  pinned), `Driver::new_multi` (inline id table
  `[[u8;80];128]`, reconnect-preserving), multi subscribe queued as
  ONE frame (unmasked-payload exact-bytes test; cap-boundary test),
  `TX_BUF_SIZE` 4→16 KiB (worst-case multi subscribe ~10.7 KiB
  payload). `Driver::new` untouched for every existing caller —
  loopback + alloc-assertion tests unmodified; **alloc 36/36 0 B/op
  re-verified after the change**.
- `cli::universe_boot` (NEW): pure `resolve_boot_universe` precedence
  law — legacy mode byte-identical (flags, anchors 42/7, btcusdt
  default, venue-blind refusal moved to a message naming both the
  flag and the config path), config mode with per-venue flag
  overrides (PM/BN single-value override replaces the venue list and
  drops config `[pairs]` for the default pair; bare `*-sym-id` with
  active config = error; depth flags OR-combine; OKX/Deribit/HL lists
  join to the EXISTING comma-spec machinery — their discovery/table/
  spawn paths byte-untouched, same id arithmetic, one owner). 11
  tests. `read_universe_source`: explicit `--universe` must exist;
  default path optional.
- `boot_discovery::run_pm` loops the proven single-id Gamma
  validation per configured id (150 ms spacing); `run_all` takes the
  id slice; coverage aggregates.
- bin: `--universe` flag; `--polymarket-asset-id`/`--binance-symbol`/
  `--*-sym-id` now optional (legacy defaults preserved through the
  resolver); PM symbol map + subscribe list from ALL tokens;
  `EngineConfig.pairs` from the resolved pairs; `build_ai_universe`
  widened to slices (spawn-aligned: all PM tokens + BN spot[0] until
  M1c); loud interim warn when config lists >1 BN symbol (M1c).
- Gates after M1b: workspace nextest **1127/1127** (+1 ignored) —
  1081 baseline + 46 new; release alloc **36/36 0 B/op** corrected
  guard; release cli relinked. Worker pytest deliberately deferred to
  M1d (no worker-side change yet).

**Interim state / next:** M1c = BN `run_multi` (one thread, N
connections, per-connection keepalive+reconnect, one producer),
`BINANCE_FUT_WS_HOST` (+ `.env.example`), BN discovery arm
(exchangeInfo spot+fapi, byte-scanner + proptest + fuzz per house
rule), spawn integration + ai-universe widening to the full BN set.
M1d = worker fetch-seam seeding (`CLAUDE_WORKER_UNIVERSE_FILE`, no
BaseConfig field — H3 seam precedent) + PM multi-market LIVE smoke
(pitfall #11 — the multi-id subscribe has NOT yet seen the real
venue) + full-universe boot + audit-replay + gates + M1 close entry.
