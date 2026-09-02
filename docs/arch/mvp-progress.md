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

### M1c results (landed this session, all gates green)

**Binance multi-symbol spot + USDS-M futures — code-complete.**
- `ingress-binance::run_multi` + `MultiConn` (NEW): N single-stream
  connections on ONE thread with ONE producer (single-writer law) —
  per-slot Driver/keepalive/backoff, in-loop reconnects **due-time
  paced at ONE blocking dial per poll iteration** (a flapping
  endpoint cannot starve live slots; oldest-due first), per-slot
  interest cache (no redundant `epoll_ctl`/`kevent`), D8
  flap-vs-healthy backoff reset on activity, kill = drop socket +
  jittered retry. The loop exits only on the stop flag; per-slot
  failures never end the venue. Parser/`drive_one` byte-untouched.
  Tests: two-steady-slots-one-producer (syms 42+7 through one ring)
  + reconnect-pacing (exactly one dial per iteration, both slots
  scheduled).
- `cli::spawn_binance_multi` + `BinanceConnSpec` (NEW): resolves
  every endpoint up front (spot host vs `BINANCE_FUT_WS_HOST`,
  default `fstream.binance.com`; `.env.example` updated), builds the
  slots, dials via `connect_tls` in the run_multi callback. The bin
  picks the lane: **>1 BN instrument (or any usdm) ⇒ multi;
  one-symbol boots keep the soak-proven single-stream lane
  byte-identical** (both lanes live, no dead code; M3's always-on
  soak can unify later).
- **BN discovery arm** (mvp-plan "discovery audit"):
  `ingress-binance::discovery::BnDiscovery` (NEW) — byte-scanner over
  `exchangeInfo` bodies (spot `?symbol=` probes + the full
  `/fapi/v1/exchangeInfo` page share one `"symbols":[…]` walker;
  field-order-free, nested filters skipped structurally, escapes
  rejected, rows cap 8192), 8 units + never-panics proptest + **NEW
  fuzz target `binance_exchange_info` — 4.48 M execs / 60 s clean**.
  `boot_discovery::run_bn`: spot per-symbol probe (HTTP 400 ⇒
  MISSING `not_found`, not fatal — the venue 400s unknown symbols;
  all other failures fatal), usdm membership+TRADING check against
  the fapi page, 150 ms pacing; `run_all` gains the
  `binance: Option<(&[String],&[String])>` arg — **config boots get
  the audit, legacy flag boots keep their historical zero-REST BN
  behavior** (`None`). Coverage gauge `bn` registered + set;
  `engine_ingress_bn_coverage_configured` joins the §6.1 family.
  New REST hosts in core-config: `BINANCE_REST_HOST`
  (api.binance.com) / `BINANCE_FUT_REST_HOST` (fapi.binance.com).
- ai-universe now spawn-aligned to the FULL BN set (spot + usdm).
- Gates after M1c: workspace nextest **1139/1139** (+1 ignored);
  release alloc **36/36 0 B/op** corrected guard; release cli
  relinked; fuzz `binance_exchange_info` 4.48 M clean +
  `universe_toml` 34.9 M clean (both this session);
  `ruleset_json` 72.3 M standing untouched.

### M1d results — **M1 CLOSED** (live smoke + worker seeding, all exit criteria met)

**Worker universe-file seeding (fetch seam; frozen surfaces
untouched).** `fetchers.universe_file_proposals` (NEW): full Python
mirror of the `core-config::universe` allocation law — PM flat tokens
(anchor 42, ordinals), Binance spot (anchor 7) + usdm (base-512
block), OKX/Deribit/HL ordinals — proposing **§9.4 descriptor names**
(`token id`, `binance:<s>`, `binance-usdm:<s>`, `okx:<i>`,
`deribit:<i>`, `hyperliquid:<c>`) for observed syms only (venue-byte
guarded); YES/NO pair entries with both syms observed feed the map's
pair machinery via a new keyword-only `pair_proposals` on
`refresh_market_map` (operator pairs first + verbatim, additive,
deduped, idempotent). Read at the fetch seam
(`CLAUDE_WORKER_UNIVERSE_FILE`; no BaseConfig field — H3 precedent);
proposals ALSO join the same fetch's Gamma seed derivation, so ONE
fetch names the tokens AND resolves question/slug/meta. Best-effort
by design (missing/malformed file = one report line, never an error —
the engine is the config validator). `import tomllib` (stdlib).
Tests +9 (`tests/test_universe_file.py`); **pytest 363** (354
stay-green + 9); `backtest.py`/`cli.py` byte-untouched (cli.py now
SIX sessions), 7-verb surface + conftest + frozen 202 untouched.

**The pitfall-#11 LIVE smoke — full non-options universe, ONE boot
command, zero flags** (`run --paper --strategy all` off the default
`~/multivenue/universe.toml`). Config picks (operator-delegated
Gamma resolution, M1-R1): *Bitcoin Up or Down on August 22?*
(Up:Down pair, vol24h ≈ $253k) + *Ethereum Up or Down on August 22?*
(≈ $103k) — resolved live from the `*-up-or-down-on-<date>-2026`
event series; BN spot btcusdt+ethusdt & usdm btcusdt; OKX
BTC-USDT + ETH-USDT-SWAP; Deribit BTC-PERPETUAL; HL BTC+ETH; pairs
0:0 + 1:1. Boot evidence (PID 66715, run-1787396699882623000):
`universe resolved from_config=true pm_tokens=4 bn_spot=2 bn_usdm=1
pairs=2`; discovery coverage okx 2/2 (universe 2004), deribit 1/1
(106), hl 2/2 (574), **bn 3/3 (universe 746 — the NEW exchangeInfo
audit live: spot probes + fapi page)**, pm 4/4 (each token resolved
with sibling cross-link + tick/min-size metadata); PM symbol map
markets=4 first_sym=42; **`binance: M1 multi-connection lane conns=3
spot=2 usdm=1`**; okx/deribit/hl threads up; ingress-ai up (permanent
key). ~13 min live: engine consuming thousands of ticks/5 s,
orders flowing on both pairs, dropped=0; coverage gauges
pm4/okx2/deribit1/hl2/**bn3**.

**audit-replay (post-SIGTERM canonical, exit 0): every venue, every
instrument ticking; integrity totals ALL FIVE venues ZERO.**
pm 4 streams (2,700+2,700 BTC legs, 12,921+12,921 ETH legs — the
multi-id subscribe proven on the real venue); bn 3 streams —
btcusdt spot 28,320 @142/s, ethusdt spot 21,469 @108/s, **btcusdt
USDM 79,917 @401/s through the fstream host — the multi-connection
lane + futures host proven live**; okx ticks/mark/funding/trade;
deribit ticks+ticker; hl ticks/book/trade/asset_ctx/all_mids.
The `hl trade regr≈n/2` per-stream display note re-observed exactly
as documented at G1 (equal-timestamp trade batches; totals zero —
known behavior, not a violation).

**Live fetch through the seam:** `universe file: entries=2
proposals=12 pairs=2 skipped=0` → `market map refreshed: added=7
conflicts=0 **unresolved=0**` — every observed sym resolves. Gamma
4/4; **and the OKX (2/2), Deribit (1/1), Hyperliquid (2/2) candle
consumers ran LIVE for the first time** (closing the H6a scope note
— they had been MockTransport-proven only), zero
failed/malformed/budget-skipped.

**Final gates at M1 close:** workspace nextest **1139/1139** (+1
ignored); release alloc **36/36 0 B/op** corrected guard (fresh
`Compiling bench` in-log); worker pytest **363**; release cli
relinked (M1c build; no Rust change in M1d); fuzz: `universe_toml`
34.9 M + `binance_exchange_info` 4.48 M both clean this phase,
`ruleset_json` 72.3 M standing.

**M1 EXIT CRITERIA (mvp-plan §4-M1) — ALL MET:** ONE boot command
runs the full non-options universe ✓ (zero flags, default config
path); audit-replay shows every venue ticking ✓ (integrity zero);
market-map resolves every observed sym ✓ (unresolved=0); gates
green ✓ (incl. the two new fuzz targets). **M1 is CLOSED** —
commits `c477bb9` (M1a+M1b) + `bad65d6` (M1c) + the M1d close
commit.

**Standing operational notes:**
- The up/down dailies EXPIRE (16:00Z) — before the next boot,
  re-resolve the day's markets via the Gamma lane and update
  `~/multivenue/universe.toml` (the M1-R1 semi-manual recipe; M3's
  daily-restart automation absorbs it).
- Recommend adding `CLAUDE_WORKER_UNIVERSE_FILE=~/multivenue/universe.toml`
  to `.env` (operator's hand — the session sets it inline until then).
- Legacy flag boots remain byte-identical end to end (resolver law +
  single-stream BN lane + zero-REST BN discovery posture).

**NEXT = M2** (options ingestion: Deribit → OKX → mark/IV channel →
Binance eapi, per mvp-plan §4-M2 with the §9.8 records design) —
**only on explicit operator go**; nothing Stage-3 without his
confirmation (mvp-plan §7).

---

## 2026-08-22 — OPERATOR GO: M2 ∥ M3 in parallel

The operator confirmed the mvp-plan §5 parallelism ("M3 can start in
parallel with M2 — it touches no ingress code") and gave the go for
BOTH phases in concurrent sessions on this one checkout. The
**Parallel M2/M3 session protocol in CLAUDE.md is LAW** for both
(explicit-path staging with `M2:`/`M3:` prefixes, ownership map,
one-engine rule, verb serialization, cargo-lock patience, the
M2.3-after-catalog sequencing pin). Per-phase logs:
`docs/m2-progress.md` / `docs/m3-progress.md` (create on first
entry); this file stays the shared index. The `.env` was
operator-reviewed and refreshed this session (all M1 keys present;
`CLAUDE_WORKER_UNIVERSE_FILE` active; HMAC key preserved). The two
kickoff prompts below are the frozen session authorities.

---

## M2 kickoff prompt (paste verbatim into a fresh session)

M2 of docs/mvp-completion-plan.md — OPTIONS INGESTION (Deribit → OKX
→ mark/IV channel → Binance eapi), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue. OPERATOR GO recorded
2026-08-22; **M3 runs IN PARALLEL in another session — read and obey
the "Parallel M2/M3 session protocol" in CLAUDE.md before touching
anything** (owned paths; explicit-path git staging, `M2:` prefix;
one-engine rule; verb serialization; cargo-lock patience; the
M2.3-after-catalog pin). Authority: mvp-plan §4-M2 + §6 risks + §9
BINDING (esp. §9.8) + docs/options-support-plan.md §2–§3 (field
deltas) → CLAUDE.md → your log docs/m2-progress.md (create on first
entry; if context runs short: interim state + exact resume point +
relaunch prompt there, then tell the operator). SESSION 0: RustRover
attach FIRST (stop if no attach); gates baseline (nextest 1139 /
alloc 36 corrected clean+fresh-Compiling-bench guard / pytest 363 /
fuzz clean); design entry adopting the §8 capped-universe proposal
(E=2 expiries × K=8 strikes ATM±4 per underlying, config-tunable via
new options keys in core-config::universe — SHARED file, coordinate)
unless the operator overrides. THEN THE LADDER, IN ORDER: **(1)
DERIBIT OPTIONS (2–3d):** boot re-discovery of the strike×expiry
chain under the capped policy (8e snapshot pattern; chains churn
daily — M3's daily restart doubles as roll refresh); `quote`-channel
BBO into the EXISTING tick lane (an option book is a book — ZERO
wire changes this step); universe-config keys + discovery audit +
live smoke with --raw-tap (pitfall #11) before "done". **(2) OKX
OPTIONS (2–3d):** same v5 WS stack — discovery instType=OPTION,
bbo-tbt subscriptions, same policy keys, live smoke. **(3) MARK/IV
CHANNEL (3–4d; starts ONLY after M3's capture-catalog first commit —
the tell is the "CATALOG LANDED — M2.3 UNBLOCKED" line in
docs/m3-progress.md; the M3 session also notifies the operator, and
`git log --oneline | grep "M3:"` cross-checks):** ONE new PMLR capture record (mark px, mark IV,
greeks, OI, underlying px — options-plan §3 deltas) fed by Deribit
`ticker` + OKX `opt-summary`; docs/wire-format.md + docs/migration.md
entries (reader-compat notes; capture stays append-only); every new
parser ships proptest + a fuzz target (§21.3/§21.4 NON-NEGOTIABLE);
audit-replay gains the channel's coverage/cadence row; coordinate the
catalog extension with M3 (whoever lands second extends). The §9.8
aggregated-IV snapshot table lands only after M3's candles.db exists
(worker-side, beside it). **(4) BINANCE EAPI HALF-INGRESS (4–5d, the
largest single item; falls back cleanly if the operator rules M2 done
with Deribit+OKX live):** NEW eapi REST discovery + dedicated options
WS host, full house ingress doctrine (mio, handwritten byte scanner,
zero-alloc, single-writer, #[repr(C)]/align(64), capture from day
one, raw-tap, proptest + fuzz); BBO first, then mark/IV into the
M2.3 channel. EXIT (mvp-plan): options ticks (3 venues) + mark/IV
records (3 venues) in capture, integrity green, fuzz clean,
full-universe boot includes a capped options chain. GATES per slice:
nextest ≥1139 / alloc 36+ 0 B/op corrected guard / pytest ≥363 /
fuzz incl. new targets; commits operator-authorized at slice
checkpoints, `M2:` prefix, EXPLICIT paths only. LANDMINES: Mac-only
cargo/pytest (pitfall #10 — the Cowork sandbox false-greens);
RustRover terminal ≤45 s — nohup > /tmp/m2-*.log & then poll
(pitfall #12); zsh eats bare ===; fixtures lie, wires drift — live
smoke before "done" (pitfall #11: 8e caught preopen empties, 27-byte
XPERP ids, sci-notation floats LIVE); paper only, no --live; no
tokio/serde_json in ingress; full `import x` in worker; frozen
7-verb surface + backtest.py/cli.py byte-untouched; conftest +
frozen 202 untouchable; .env: read/print NOTHING; NOTHING Stage-3
(executor/risk/dispatcher/live — mvp-plan §7 is the operator's
gate); push anomaly record-only. SESSION FACTS: metrics
127.0.0.1:9191; ONE ENGINE EVER — `pgrep -f multivenue-engine`
before any boot and coordinate smoke windows with M3's standing
launchd instance (launchctl stop → smoke → start); universe.toml
up/down dailies expire 16:00Z (refresh via the Gamma lane before
smoke boots until M3's automation lands); AF_UNIX sun_path cap;
SO_RCVTIMEO EINVAL on peer-closed UDS; `sample <pid>` for hangs.

---

## M3 kickoff prompt (paste verbatim into a fresh session)

M3 of docs/mvp-completion-plan.md — CONTINUOUS DATA OPS
(capture-catalog + launchd always-on + retention + candles.db), MAIN
CHECKOUT /Users/darkcite/trading-engine-multivenue. OPERATOR GO
recorded 2026-08-22; **M2 runs IN PARALLEL in another session — read
and obey the "Parallel M2/M3 session protocol" in CLAUDE.md** (owned
paths; explicit-path git staging, `M3:` prefix; one-engine rule —
YOUR launchd instance becomes the standing engine; verb
serialization; cargo-lock patience; land the capture-catalog FIRST
commit early — M2.3's wire migration waits on it). Authority:
mvp-plan §4-M3 + **§9 BINDING VERBATIM** (§9.2 continuity, §9.4
candles.db, §9.5 timeframe policy, §9.6 gap-fill, §9.7
capture-derived, §9.9 consumer matrix) → CLAUDE.md → your log
docs/m3-progress.md (create on first entry; context-short procedure
as usual). SESSION 0: RustRover attach FIRST; gates baseline
(nextest 1139 / alloc 36 / pytest 363). SLICES, CATALOG-FIRST:
**(1) CAPTURE-CATALOG** (new offline CLI subcommand, audit-replay
doctrine — allocations allowed, doctrine header): walk a replay
root → per-run spans, per-venue tick coverage, UTC days spanned, gap
map, run-dir sizes, "backtestable window" report (surface the
harness's span/stats logic + the monitor's window arithmetic); JSON
+ human summary, deterministic; per-crate tests; PMLR v2 as-is
(M2.3 adds a channel later — whoever lands second extends). **LAND
THE FIRST COMMIT EARLY, and the moment it lands you have a VERBATIM
NOTIFICATION DUTY: write the line "CATALOG LANDED — M2.3 UNBLOCKED
(commit <hash>)" at the top of your next docs/m3-progress.md entry
AND tell the operator explicitly: "The capture-catalog first commit
is in — M2.3 is unblocked."** (That log line is the tell the M2
session greps for; the operator message is the human signal.) **(2) LAUNCHD ALWAYS-ON:** KeepAlive'd
paper engine on the full universe (~/Library/LaunchAgents plist +
docs/local-setup.md runbook; caffeinate/power settings so the Mac
never sleeps it); DAILY GRACEFUL RESTART (SIGTERM drain — proven
clean at M1d) → one run dir per UTC day = gap-free days BY
CONSTRUCTION (closes the H6a min_trading_days standing finding);
the restart step ALSO refreshes universe.toml's PM up/down dailies
via the Gamma lane (automating the M1-R1 recipe: resolve the day's
`*-up-or-down-on-<date>-2026` markets → rewrite [polymarket]
markets; wholesale PM replacement is the CLEAN path per the
CLAUDE.md universe runbook; a standalone script or worker module —
the 7-verb surface is FROZEN, no new verb); the plist must source
`.env` via a wrapper script (never inline values); ONE ENGINE EVER —
your instance is the standing one, M2's smoke windows stop/start it
via launchctl. **(3) RETENTION:** age/size-based archival of run
dirs (config; default keep-all until disk pressure), documented;
catalog reports sizes. **(4) CANDLES.DB (§9.4–§9.7 BINDING):**
worker-owned SQLite WAL; ONE table, PK (venue, descriptor, tf,
open_ts) — descriptor NEVER bare SymbolId; columns o/h/l/c/v,
source ∈ rest|derived|capture, fetched_ts; fetch bases 1m (24–48 h
rolling) / 1h (30–90 d) / 1d (listing lifetime) ONLY; derive
5m/15m/4h exactly (O=first H=max L=min C=last V=sum,
source=derived, cached); §9.6 gap-fill upsert (SELECT max(open_ts)
per (sym,tf) → request ONLY the missing window → paginate under
RestBudget; bounded backfill 48h/90d/lifetime; budget exhaustion
resumes next cycle by construction); the OPEN bar is upserted,
CLOSED bars are IMMUTABLE — a refetch disagreeing with a stored
closed bar is a LOGGED conflict (market-map pattern), never
overwritten; §9.7 capture-derived 1m mid-OHLC + tick-count (volume
NULL — we capture BBO; never fabricate) for PM especially, plus the
REST-vs-socket drift check; worker-side Python, full `import x`,
tests additive (frozen 202 + 7-verb surface untouchable — candle
work rides fetch/offline modules; any new operator surface is a
script/module, ask the operator before ANY CLI-surface change).
**(5) INIT-IF-EMPTY restated end-to-end:** first boot on an empty
MULTIVENUE_LOG_DIR/worker dir bootstraps everything; the catalog
makes the capture side visible. EXIT (mvp-plan): **N≥3 CONSECUTIVE
gap-free days on disk — CALENDAR TIME: install the always-on lane
EARLY and let it accumulate while you build**; the catalog reports
them; a REAL-capture backtest passes the days gate (the frozen
`multivenue-engine backtest` argv over the accumulated root). GATES
per slice: nextest ≥1139 / alloc 36+ corrected guard / pytest ≥363
(+ additive) / fuzz untouched unless a new parser (then
§21.3/§21.4). LANDMINES: Mac-only cargo/pytest (pitfall #10);
RustRover ≤45 s — nohup > /tmp/m3-*.log & then poll; zsh eats bare
===; paper only; .env read/print NOTHING; worker verbs globally
serialized vs M2 (`pgrep -f claude-worker` first); **the backtest
harness reads PMLR ONLY, never candles (§9.9 — candle-fed fills
would be fabricated)**; NOTHING Stage-3 (mvp-plan §7); push anomaly
record-only; commits operator-authorized, `M3:` prefix, EXPLICIT
paths only. SESSION FACTS: metrics 127.0.0.1:9191; SIGTERM = clean
drain (M1d-proven); universe.toml PM dailies expire 16:00Z — your
restart automation is the standing fix; audit-replay integrity-zero
is the health tell; `sample <pid>` for hangs.

---

## M6 — CLOSED by operator ruling, 2026-09-02

The operator ruled M6 **done** at the VM2-close boundary (the same
blessing pattern as C6 and the soak amendments: the 7-day calendar
requirement waived on the accumulated evidence — the launchd fleet ran
unattended since M3 with T2 restarts + retention + hourly candles/iv;
WS13's 1-hour blessed soak; the Aug-29→Sep-2 VM2 live windows; the
Sep-2 disk-full recovery proving the revive levers; gate batteries
green at every boundary, last = VM2 V9's 1420/39/600). Shadow-P&L
delivered as the operator's all-strategies report (2026-09-02;
per-run model + aggregation — the whole-root OOM + the dead 00:20Z
nightly timer are recorded ops debts, not gates). **MVP COMPLETE.
The M-phase plan (M0–M6) is closed end to end; the only gate left is
the Stage-3 ENTRY GATE (mvp-completion-plan §7, archived beside this
log — §7 and §9 remain FORWARD-BINDING from the archive).**
