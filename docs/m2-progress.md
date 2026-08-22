# M2 — Options ingestion (Deribit → OKX → mark/IV channel → Binance eapi): progress log

Phase authority: `docs/mvp-completion-plan.md` §4-M2 + §6 risks + §9
(BINDING, esp. §9.8) + `docs/options-support-plan.md` §2–§3 (field
deltas) → this log's latest entry → CLAUDE.md. Operator go recorded
2026-08-22; M3 runs in a PARALLEL session — the CLAUDE.md "Parallel
M2/M3 session protocol" is LAW for every entry below. Commits are
operator-authorized, `M2:` prefix, explicit paths only.

---

## 2026-08-22 — Session 0: attach + gates baseline + the capped-universe design entry (no code touched)

### Session 0 mechanics

- RustRover MCP attached (`get_project_modules`) against the main
  checkout first, per law.
- `git status` at session start: branch `main` ahead 12 of origin (the
  KNOWN push anomaly — record, never act); dirty `docs/m3-progress.md`
  (M3's log — theirs) and `docs/mvp-progress.md` (SHARED — the diff is
  the kickoff-prompt notification-duty wording both lanes' prompts
  quote; M3's log attributes it to the M2 lane, so this lane CARRIES it
  into its first commit ask). Mid-session M3's C2 WIP appeared
  untracked (`claude-worker/src/claude_worker/universe_refresh.py`,
  `claude-worker/tests/test_universe_refresh.py`, `scripts/`) — M3-owned
  paths, not touched by M2. No engine, no worker process running at
  baseline time (`pgrep` clean on both).
- **M2.3 pin status: CATALOG LANDED — M2.3 UNBLOCKED** confirmed by the
  verbatim line in `docs/m3-progress.md` AND `git log` (`cf132ae`, +12
  tests). The pin is satisfied before M2 wrote a line of code; ladder
  order is unchanged (Deribit → OKX → mark/IV → eapi). M2 lands second
  on the catalog ⇒ M2.3 extends M3's catalog with the options channel's
  dedicated coverage row (their C1 entry names it the designated
  extension point).

### Baseline gates (all on the Mac)

- workspace nextest **1151/1151** (+1 skipped fixture-regen) — the
  kickoff's 1139 SUPERSEDED by M3's C1 landing (+12 catalog tests);
  matches M3's post-C1 number exactly. 1151 is M2's stay-green floor.
- alloc **36/36** 0 B/op release, `--test-threads=1`, corrected
  clean-guard (`cargo clean -p bench --release`), fresh `Compiling
  bench` confirmed in the log (grep count 1).
- worker pytest: **every committed test green** — 378 passed, 3 failed,
  ALL 3 confined to M3's UNTRACKED in-flight `test_universe_refresh.py`
  (their C2 WIP, minutes old at run time). The committed surface = the
  363 baseline, intact; frozen 202 + conftest untouched. M2's stay-green
  is 363-committed (re-baseline against M3's number after their C2
  lands).
- fuzz sanity, M2-relevant targets, ALL CLEAN (61 s each, zero
  crashes/panics; full ≥300 s runs remain the per-slice gate for
  new/changed targets): `deribit_instruments` 2.82M ·
  `deribit_jsonrpc_frame` 17.0M · `deribit_book` 61.1M ·
  `okx_instruments` 1.76M · `okx_frame` 10.1M · `universe_toml` 35.2M
  execs. (24 targets exist; the untouched 18 stand on their M1-close /
  standing numbers — e.g. `ruleset_json` 72.3M.)
- Toolchain note (re-learned live): bare `cargo fuzz run` inside the
  repo trips the 1.88.0 toolchain pin ("1 nightly option were parsed" at
  the sanitizer flags) — `cargo +nightly fuzz run` is the working form,
  exactly as the CLAUDE.md note says.

### DESIGN ENTRY — capped options universe (ADOPTED; operator may override)

Adopts the mvp-plan §8 open-question-1 proposal, made concrete. This is
the §4-M2 "capped universe policy" and the options-plan §2 "universe
filter policy", with numbers and seams.

**Policy.** Per venue, per configured underlying: the nearest **E = 2
expiries** × the **K = 8 strikes nearest ATM** (4 at-or-below + 4 above
the boot-time reference price), **both calls and puts** at every
selected strike ⇒ hard cap **E × K × 2 = 32 instruments per
underlying** (Deribit BTC+ETH ⇒ ≤ 64; OKX same shape; Binance eapi
M2.4). Chains churn daily; the 8e boot-snapshot doctrine stands — a new
expiry enters at the next boot, and M3's daily launchd restart doubles
as the chain-roll refresh (mvp-plan §6 risk 1). Intraday chain-append
stays explicitly NOT proposed (options-plan §2).

**ATM centering.** The capped filter needs a reference price at
discovery time: ONE paced REST call per underlying on the venue's own
surface (Deribit `public/get_index_price`, OKX `index-tickers`;
Binance eapi equivalent at M2.4), handwritten byte scanner like every
discovery parser, result visible in the discovery audit line. Failure
is FATAL at boot like every other discovery transport/parse failure
(M1c precedent) — no silent options-less boot when options are
configured.

**Config keys** (`core-config::universe` — SHARED file, small additive
edit, sequential commit, noted here per protocol). The TOML-subset
parser today speaks string-arrays and booleans only ⇒ it gains ONE
grammar extension: bare non-negative **integer values** (`key = 2`),
unknown-key/section fatality unchanged, `universe.toml.example`
documents it, existing `universe_toml` fuzz target + proptests extend
(house rule — the grammar change rides the EXISTING fuzz target, ≥300 s
clean before the slice closes). New keys, all three venue sections,
same shape:

```toml
[deribit]
options_underlyings = []  # e.g. ["BTC", "ETH"]; empty/absent = options lane OFF
options_expiries    = 2   # E: nearest expiries, integer ≥ 1 (default 2)
options_strikes     = 8   # K: strikes nearest ATM, integer ≥ 2 (default 8 = ATM±4)

[okx]                     # uly families, e.g. ["BTC-USD"]; same three keys
[binance]                 # M2.4, eapi underlyings, e.g. ["BTCUSDT"]; same keys
```

Defaults E=2/K=8 apply when the integer keys are absent but
`options_underlyings` is non-empty. Backstop caps E ≤ 4, K ≤ 32
(config-reject above — the working set stays order-of-dozens by
construction). Per-venue CLI override lane: none for options in M2
(config-file only; the legacy flag boot stays options-less and
byte-identical — options are a from_config-only feature).

**SymbolId allocation** (the M1 law, extended by the proven BN-usdm
pattern). Discovered, post-filter options get ordinals in a DISJOINT
per-venue block: `OPT_ORDINAL_BASE = 512` for Deribit and OKX (their
static instrument lists cap at 500 < 512 — exactly the
`BN_USDM_ORDINAL_BASE` precedent); Binance eapi (M2.4) takes base
**1024** (spot ordinals < 512, usdm block ends < 1012). Within-boot
deterministic order: underlying → expiry asc → strike asc → call
before put. "Filtered-out instruments are never allocated ordinals"
(options-plan §2) — allocation strictly post-filter. Cross-boot
ordinal instability is ACCEPTED and already mitigated system-wide:
catalog, shadow-P&L and candles key by **venue+descriptor**
(`deribit:BTC-27MAR26-100000-C`), never bare SymbolId (mvp-plan §6
risk 6, §9.4); the worker map's descriptor proposals extend to the
options block at the fetch seam (M1d machinery, observed-syms-only).

**Wire/lane consequences.** M2.1 is ZERO wire changes by construction:
the Deribit ingress already speaks `quote.` with a multi-channel ×
multi-instrument subscribe writer, and the quote parser normalizes BBO
→ the 64 B `Tick` → the existing per-venue `deribit-ticks.pmlr`
capture (an option book is a book). M2.2 identical via OKX `bbo-tbt`.
Engine-side tables (symbol map, MultiBook/lane sizing, discovery
audit gauges) size from the resolved universe at boot — the slices
verify the +N options syms fit the existing preallocation law (boot
sizing, never hot-path growth). Discovery extends
`ingress-deribit/src/discovery.rs` from `kind=future` to also fetch
`kind=option` pages per configured underlying (the 1 req/s pacing lane
exists), and OKX discovery gains `instType=OPTION` — both under the
existing proptest + fuzz targets, extended.

**M2.3 transport reading** (recorded now, confirmed at slice kickoff):
the BINDING mvp-plan §4-M2.3 + §9.8 wording — "ONE new capture
record", "NEW PMLR channel", wire-format.md + migration.md entries —
SUPERSEDES options-plan §3 route-1 (ChannelEvent rides) on the
transport question; options-plan §3 remains authoritative for the
FIELD LIST (mark px, mark IV, greeks, OI, underlying px) and the
fixed-point spirit (px ×1e6, IV ×1e9). Shape at slice time: one new
fixed-size append-only PMLR slot kind, per-venue options-analytics
capture file, fed by Deribit `ticker.{opt}` + OKX `opt-summary`
(+ BN eapi stream at M2.4); audit-replay gains the channel's
coverage/cadence row; the capture-catalog gains the dedicated row (M2
extends, per the lands-second rule). The §9.8 aggregated-IV snapshot
table is worker-side BESIDE `candles.db` and waits for M3's candles.db
to exist — not an M2.3 blocker (capture channel first, digest table
after).

### Ladder (unchanged from the kickoff; estimates from mvp-plan §4-M2)

1. **M2.1 Deribit options** (2–3 d): capped discovery (`kind=option` +
   index px) + config keys + allocation block + `quote` BBO into the
   tick lane; discovery audit; live smoke `--raw-tap` (pitfall #11).
2. **M2.2 OKX options** (2–3 d): `instType=OPTION` discovery, `bbo-tbt`
   subs, same policy keys; live smoke.
3. **M2.3 mark/IV channel** (3–4 d): UNBLOCKED already (`cf132ae`); the
   record above; proptest + fuzz for every new parser (§21.3/§21.4);
   wire-format + migration entries; audit-replay + catalog rows.
4. **M2.4 Binance eapi half-ingress** (4–5 d): new REST discovery +
   options WS host, full house doctrine; BBO first, then mark/IV into
   the M2.3 channel. Clean fallback if the operator rules M2 done at
   Deribit+OKX live (mvp-plan §6 risk 2).

Exit (mvp-plan §4-M2): options ticks (3 venues) + mark/IV records (3
venues) in capture, integrity green, fuzz clean, full-universe boot
includes a capped options chain. Per-slice gates: nextest ≥ 1151 /
alloc 36+ 0 B/op corrected guard / pytest ≥ 363-committed / fuzz incl.
new-or-extended targets ≥ 300 s.

### Session-0 status

Code untouched; zero commits; no engine boots (none needed — no smoke
this session). Shared-file coordination state: `docs/mvp-progress.md`
dirty diff carried by this lane; `core-config` integer-grammar
extension announced here BEFORE the edit lands (M3 heads-up via this
log). NEXT = M2.1 Deribit slice: discovery `kind=option` pages +
capped filter + config keys + allocation + subscribe fan-in, tests
first, then the pitfall-11 live smoke (coordinate the boot window —
`pgrep -f multivenue-engine` first; if M3's launchd instance is
standing by then, `launchctl` stop → smoke → start; refresh
`universe.toml` up/down dailies via the Gamma lane if booting before
16:00Z expiry).

**Resume point if context dies here:** Session 0 complete (baseline +
design entry above); nothing staged; start M2.1
with the design entry as law; re-read CLAUDE.md parallel protocol
first.

---

## 2026-08-22 — M2.1 Deribit options: CODE-COMPLETE (same session; gates + live smoke below)

Operator go for the ladder recorded (this session). Everything the
design entry pinned, built; deltas from the entry are called out
explicitly.

### core-config (SHARED — announced in Session 0, single additive diff)

- TOML-subset grammar: bare non-negative decimal integers (no sign,
  no `_`, no leading zeros, ≤ 7 digits) — `universe.toml.example`
  documents; unknown-key fatality unchanged.
- `[deribit] options_underlyings / options_expiries / options_strikes`
  → `OptionsPolicy { underlyings, expiries, strikes }` on `Universe`
  (defaults E=2 / K=8; E 1..=4; K EVEN 2..=32; ≤ 16 underlyings,
  dup-checked; integer knobs with empty underlyings = FATAL
  fail-fast). Keys are **deribit-only in M2.1** — `[okx]`/`[binance]`
  reject them until their slices land (test-pinned).
- Constants: `OPT_ORDINAL_BASE = 512` (Deribit/OKX options block; the
  BN-usdm precedent; static lists ≤ 500 ⇒ disjoint by construction),
  `BN_OPT_ORDINAL_BASE = 1024` (M2.4), `OPT_*_DEFAULT/MAX`.
- `allocate()` untouched by policy (test-pinned byte-identical) — the
  cli allocates options POST-discovery.
- Tests: core-config 42 → **55** (13 new: defaults, ranges,
  even-K, literal forms incl. `007`/`2_000`/quoted/8-digit, dup keys,
  underlying dup+cap, deribit-only scope pin, type mismatches,
  allocation-untouched; round-trip proptest generates the options
  section too).

### ingress-deribit

- discovery.rs: `ingest_options_body` (kind=option contract:
  `option_type`/`strike`/`expiration_timestamp` REQUIRED; own cap
  `DERIBIT_OPT_DISCOVERY_ROWS_CAP = 4096` — live BTC chain is
  order-1k) sharing the futures row-walker (`RowKind` parametrized;
  futures behavior byte-identical, kind cross-tests both directions);
  row gains `is_option/is_call/strike_1e9/expiration_ts_ms`
  (`expiration_timestamp` via `scan_u64` — the ×1e9 scanners overflow
  on ms timestamps; captured on futures rows too where present).
- `parse_index_price` (ATM reference; envelope law = ingest;
  nonpositive rejected) + `index_name` ("BTC"→"btc_usd").
- `select_capped_chain`: nearest-E distinct future expiries asc; per
  expiry the K nearest-ATM strikes POSITION-BASED (last K/2
  at-or-below + first K/2 above; no distance ties, no cross-side
  backfill); C then P per strike; ≤ E×K×2 by construction;
  deterministic order = the allocation order.
- **Symbol-table partition (design delta, forced by the u64 mask):**
  `DERIBIT_STATIC_MAX = 16` (old law, full channel set) +
  `DERIBIT_OPT_MAX = 64` (quote-only; default policy 2×E2×K8×C/P =
  64 exactly) ⇒ `DERIBIT_MAX_SYMBOLS = 80`. `insert` (static,
  order-enforced: new `StaticAfterOptions` err variant) /
  `insert_option`; `static_len()/n_options()/is_option_row()`.
- Subscribe verification mask u64 → **u128** (static block bits 0..64
  = sym×4+ch; options bits 64..128 = one quote bit each) —
  connection-establishment state, NOT hot path. `MAX_CHANNELS
  64 → 128`, `TX_BUF_SIZE 8 → 16 KiB`, `SUBSCRIBE_SCRATCH 8 → 12 KiB`
  (≤ ~48 B/channel × 128 ≈ 6.1 KiB, ≥2× margin).
  `write_subscribe_all`: option rows emit `quote.{instr}` ONLY (the
  mark/IV `ticker` stream is M2.3); ack registration + masks
  row-aware. Hot-path deltas: NONE on the message path (same parsers,
  same Tick lane); the table linear scan grows ≤ 80 rows — trivial at
  Deribit cadence, note rides docs/hot-path-latency.md at slice
  close.
- Tests: ingress-deribit 60 → **82** (options page fixtures incl.
  banded tick steps + sci strike `5e-1`, chain-field missing/reject,
  cap-above-futures-cap, index-price happy/sci/error shapes,
  selection E2K2 exact-name determinism + one-sided + missing twin +
  dead/expired exclusion + ≤cap, partition law, quote-only subscribe
  bytes, u128 mask high-block law; +2 proptests: options walker/index
  never-panic, selection invariants incl. the order law).
- Fuzz: NEW target `deribit_index_price`; `deribit_instruments`
  extended (options walker + selection-cap invariant + index parser
  ride the same corpus).

### cli

- universe_boot: `BootUniverse.deribit_options` (+
  `deribit_options_dropped`) — config policy carried; explicit
  `--deribit-symbols` override replaces the WHOLE [deribit] section
  per the M1a law ⇒ policy dropped + bin WARNS; legacy boots carry
  the disabled default (byte-identical). 3 new tests.
- boot_discovery: `run_deribit_options` — per underlying (config
  order): 1050 ms pacing before EVERY fetch (venue 1 req/s law),
  `get_index_price` (fatal on fetch/parse failure — no silent
  options-less boot), `kind=option` page into a FRESH table, capped
  selection; EMPTY selection for an underlying = MISSING semantics
  (`any_missing`, reason `no_chain` — paper warns, live refuses);
  per-underlying info line (index px, chain total/live, selected).
  Ordinals allocated HERE in selection order:
  `make_symbol_id(Deribit, OPT_ORDINAL_BASE + k + 1)`.
  `Outcome.deribit_options: Vec<(String, SymbolId)>`; `run_all` takes
  the policy (one new param; legacy passes the disabled default).
- bin: deribit venue boots when static spec **or** discovered chain
  is non-empty (options-only [deribit] is a valid universe);
  `extend_deribit_table_with_options` appends the chain after the
  static block (dupes + cap fail-fast, actionable message); NEW gauge
  `engine_ingress_deribit_options_selected`; spawn/capture unchanged
  (options BBO rides `deribit-ticks.pmlr` — ZERO wire changes, as
  designed).
- cli tests 125 → **131**.

### Recorded consequences (by design — revisit markers, not bugs)

- **Worker map:** options syms are boot-discovered ⇒ the M1d
  universe-file seeding lane cannot propose their descriptors; worker
  `fetch` will report them UNRESOLVED until an options-aware naming
  lane exists (engine-emitted manifest vs worker chain mirror —
  decided at M2.3 with the analytics channel). Accepted for M2.1.
- **`SYM_BUCKETS = 64`** (engine last-tick buckets, hashed): ~75+
  total syms alias buckets more often — tick-age gauge granularity
  only, semantics unchanged.
- Strategy books (`MultiBook`) track lazily / by registration —
  option syms cost nothing engine-side until a strategy references
  them; no fixed all-syms table exists to overflow (checked).
- Policy caps (E4×K32×U16 = 4096 theoretical) exceed the
  per-connection `DERIBIT_OPT_MAX = 64` deliberately — boot
  fail-fasts with the shrink-your-policy message rather than
  silently truncating.
- **M3 refresh-lane coordination:** `universe_refresh.py` (M3 WIP)
  rewrites `universe.toml` daily — it MUST round-trip the new
  `[deribit] options_*` keys (tomllib reads them fine; the rewriter
  must preserve them). Flagged here for the M3 lane.

### Gates (all on the Mac)

- workspace nextest **1183/1183** (+1 skipped) = 1151 baseline + 32
  M2.1 (13 core-config + 22 ingress-deribit + 6 cli + …; M3's C1 rode
  the 1151). 1183 is the new stay-green floor for this lane.
- release alloc **36/36** 0 B/op, `--test-threads=1`, corrected
  clean-guard, fresh `Compiling bench` confirmed (grep count 1) —
  re-verified AFTER the ingress-deribit Driver/table growth.
- worker pytest **412/412** with the release binary on PATH (Python
  untouched by M2.1; count grew 363 → 412 via M3's C4/C5 landings).
  NOTE (cross-session): ONE run showed a single
  `test_cli.py::test_push_set_param_and_halt` failure while BOTH
  lanes ran pytest simultaneously; it passes alone and the full
  suite rerun is green — the worker-verb serialization law
  effectively extends to CONCURRENT PYTEST RUNS (shared socket/tmp
  fixtures). Logged for the protocol.
- fuzz ≥300 s each: `universe_toml` **204.2M** execs clean (the
  integer grammar rides the existing target), `deribit_instruments`
  **9.66M** clean (options walker + selection-cap invariant + index
  parser added to the corpus loop), `deribit_index_price` (NEW)
  **44.75M** execs clean; zero crashes/panics across all three.

### Live smoke (pitfall #11 — real venue, real binary, raw tap armed)

Procedure per the M3 runbook: `launchctl bootout` the standing lane →
`universe_refresh` module (fresh dailies; **my `[deribit] options_*`
keys survived the rewrite byte-preserved — M3's invariant holds with
the new keys, live-verified**) → `cargo build --release -p cli` (G0)
→ foreign boot `run --paper --strategy all --metrics --raw-tap
deribit` → ~2.5 min steady → SIGTERM drain → audit-replay → restore
pre-smoke universe.toml → `launchctl bootstrap` (standing lane back,
pid verified).

- **Discovery, live:** BTC chain **1038 rows parsed** (all live —
  vindicates `DERIBIT_OPT_DISCOVERY_ROWS_CAP=4096` > the 1024 futures
  cap), index 77,275.53; ETH chain **932 rows**, index 2,428.96;
  **selected 32 + 32 = 64** = the default policy exactly; per-
  underlying info lines as designed. ZERO error lines whole boot.
- **Subscribe:** `deribit: starting ingress thread instruments=65`
  (1 static + 64 options); state Steady; the u128 verification mask
  accepted the real venue's batched ack (a missing channel would have
  errored the session — none did).
- **Gauge:** `engine_ingress_deribit_options_selected 64` live on
  :9191.
- **Capture:** `deribit-ticks.pmlr` **4,145 ticks** in ~2.5 min
  across the chain; audit-replay renders per-stream lines for the
  full options block (`0x03000201+` ordinals = base-512 law live),
  EVERY stream `regr=0 holes=0 missing=0 chain_breaks=0`; venue
  integrity totals ALL ZERO; gap pairing zero.
- **Raw tap (rejects mode): `deribit-raw.tap` = 64 B header, ZERO
  records — not one deribit frame (option quotes included) failed to
  parse.** The pitfall-11 evidence.
- Post-smoke `claude-worker fetch` (serialized, `.env` sourced):
  `unresolved=0 conflicts=0` — the standing (options-off) run is the
  observation window, so the documented options-descriptor gap has
  not bitten yet; it will surface only when a fetch observes option
  syms (M2.3 decides the naming lane).
- Standing lane restored on the NEW binary (options lane inert
  without config keys); pre-smoke universe.toml back in place —
  options go always-on only when the operator says so.

### Slice checkpoint — COMMIT ASK (pending operator authorization)

`M2:`-prefixed, EXPLICIT paths only:

- `crates/core-config/src/universe.rs`
- `crates/ingress-deribit/src/discovery.rs`
- `crates/ingress-deribit/src/lib.rs`
- `crates/ingress-deribit/src/run_loop.rs`
- `crates/cli/src/universe_boot.rs`
- `crates/cli/src/paper.rs`
- `crates/cli/src/lib.rs`
- `crates/cli/src/bin/multivenue-engine.rs`
- `fuzz/Cargo.toml`
- `fuzz/fuzz_targets/deribit_instruments.rs`
- `fuzz/fuzz_targets/deribit_index_price.rs` (new)
- `universe.toml.example`
- `docs/m2-progress.md` (new)
- `docs/mvp-progress.md` (the carried kickoff-wording diff from
  Session 0 — SHARED, additive)

NOT staged: anything of M3's; `.env`; `~/multivenue/*` (operator
space).

**Resume point if context dies here:** M2.1 code-complete + gates
green + live-smoked (evidence above); NOTHING committed — put the
commit ask to the operator, then M2.2 (OKX options: same v5 stack,
`instType=OPTION` discovery, `bbo-tbt` subs, `[okx]` options keys in
core-config — grammar already speaks integers, so the M2.2
core-config diff is slot wiring only).

---

## 2026-08-22 — M2.1 COMMITTED `d0e14d4` (operator-authorized); M2.2 OKX options CODE-COMPLETE (same session)

M2.1 landed as `d0e14d4` (14 explicit paths, `M2:` prefix; only M3's
`docs/m3-progress.md` remained dirty after — clean handoff). Operator
go for M2.2 in the same message.

### core-config (SHARED, additive — second sequential touch)

- `[okx] options_underlyings / options_expiries / options_strikes` →
  `Universe.okx_options` (underlyings are OKX **uly families**, e.g.
  `"BTC-USD"`). The M2.1 validation block generalized into ONE
  venue-labeled `finalize_options_policy` helper (no duplicated law;
  deribit messages keep their M2.1 shape, okx errors say `OKX …`).
  Grammar untouched — the integer extension was M2.1's.
- Scope pin updated: `[deribit]` + `[okx]` accept the keys;
  `[binance]`/`[polymarket]`/`[hyperliquid]` still reject
  (test-pinned; `[binance]` flips at M2.4).
- Tests 55 → **58** (okx carry/validate/coexist + scope flip;
  `full_src` example carries okx options keys through allocation
  untouched).

### ingress-okx

- `OkxInstType` gains `Option` (`from_bytes` decodes `OPTION`; the
  per-page contract lives in the discovery `RowMode` — a legacy page
  still rejects option rows, both directions test-pinned).
- discovery: `ingest_options_body` (`RowMode::Option`: requires
  `stk`/`expTime`/`optType`; quoted-string numbers per this venue —
  `stk` via the quoted 1e9 scanner, `expTime` quoted-digit ms parse
  (×1e9 scanners overflow), `optType` `"C"|"P"`; empty forms of all
  three legal on NON-option rows and skipped); row gains
  `is_call/strike_1e9/exp_ms`; `rows()` accessor; shared
  `OKX_DISCOVERY_ROWS_CAP = 16 384` already covers a full uly chain —
  no separate options cap needed (unlike Deribit's 1024→4096).
- `parse_index_price` (`/api/v5/market/index-tickers?instId=<uly>`,
  quoted `idxPx`; the uly IS the index instId — no name mapping) —
  proptest + NEW fuzz target `okx_index_price`.
- `select_capped_chain` — the M2 selection LAW **twinned verbatim**
  from the Deribit original (doc header names the law source; the
  SAME proptest invariants pin both — cap, candidate filter,
  deterministic expiry→strike→C/P order). Unification into a shared
  home deferred to M2.4 when BN eapi becomes the third consumer
  (rule of three) — recorded as a deliberate twin, not drift.
- Sizing partition law (the Deribit pattern, but NO table partition
  field — the `OkxInstType::Option` tag IS the discriminator):
  `OKX_STATIC_MAX = 16` (old law) + `OKX_OPT_MAX = 64` ⇒
  `OKX_MAX_SYMBOLS = 80`; `MAX_SUB_ARGS = 5×16 + 64 = 144`
  (options are **bbo-tbt ONLY** — one arg each; `opt-summary` is
  M2.3); `TX_BUF 8→16 KiB`, `SUBSCRIBE_SCRATCH 8→12 KiB`.
  `build_sub_args`: Option rows short-circuit after the bbo-tbt arg
  (no trades/mark/funding/books, depth never touches them —
  test-pinned). OKX has per-arg SubAcks (no Deribit-style mask) —
  ack machinery unchanged, capacities follow MAX_SUB_ARGS.
- Tests 61 → **72** (option grid fixtures in the quoted wire shape,
  contract both directions, missing/bad chain fields, fractional
  strike `"0.5"`, index-price shapes incl. bare-number rejection,
  selection determinism/one-sided/missing-twin/cap, bbo-only
  sub-args; +2 proptests mirroring the Deribit invariants).
- Fuzz: `okx_instruments` extended (options walker + selection cap +
  index parser on the same corpus); NEW target `okx_index_price`.

### cli

- `build_okx_symbol_table` caps static at `OKX_STATIC_MAX` (table
  capacity is 80 now); NEW `extend_okx_table_with_options` (inserts
  with `OkxInstType::Option`; dup + `OKX_OPT_MAX` fail-fast — the
  existing-options count is DERIVED from the table so the cap holds
  across calls).
- boot_discovery: `run_okx_options` (per uly: index-tickers + OPTION
  page, 150 ms pacing — the OKX page cadence; fatal index; `no_chain`
  MISSING; ordinals `make_symbol_id(Okx, OPT_ORDINAL_BASE + k + 1)`
  in selection order). `run_all` okx arm: options-only `[okx]` boots
  the venue (empty static table + chain); the chain is appended to
  `okx_table` INSIDE run_all (unlike deribit, whose table builds
  bin-side); `Outcome.okx_options` carries the pairs for gauge +
  logging.
- universe_boot: `okx_options` (+`okx_options_dropped`; `--okx-symbols`
  override replaces the whole `[okx]` section, policy dropped + bin
  WARNS — same M1a law); legacy boots byte-identical.
- bin: second warn line; NEW gauge `engine_ingress_okx_options_selected`.
- cli tests → **all green** (261 across the three touched crates in
  the slice run; totals in the gates below).

### Gates (all on the Mac)

- workspace nextest **1197/1197** (+1 skipped) = 1183 + 14 M2.2
  (3 core-config + 11 ingress-okx + cli deltas net). New stay-green
  floor 1197.
- release alloc **36/36** 0 B/op, corrected clean-guard, fresh
  `Compiling bench` (grep count 1) — re-verified after the OKX
  Driver/table growth.
- worker pytest **412/412** (Python untouched). The cross-session
  concurrent-pytest flake bit AGAIN mid-gates (a DIFFERENT test this
  time — `test_stage_refusals_…`; passes alone, full rerun green with
  no second pytest running). TWICE-observed now: **proposed protocol
  addendum — treat pytest runs like worker verbs: `pgrep -f pytest`
  before running, don't overlap the lanes.**
- fuzz ≥300 s each, all clean, zero crashes/panics: `universe_toml`
  **173.4M** (now with okx keys), `okx_instruments` **5.5M**
  (options walker + selection cap + index parser riding),
  `okx_index_price` (NEW) **45.8M**.

### Live smoke (pitfall #11 — BOTH venues' chains in ONE boot)

Same runbook window (bootout → G0 relink → foreign boot `--paper
--strategy all --metrics --raw-tap okx,deribit` → ~2.5 min → SIGTERM
→ audit-replay → restore pre-smoke universe.toml → bootstrap; PM
dailies still live — no refresh needed this window). The smoke
universe carried BOTH `[okx]` and `[deribit]` options policies
(BTC-USD/ETH-USD + BTC/ETH, E2/K8) — the coexistence proof.

- **OKX discovery, live:** BTC-USD chain **1558 rows** parsed (the
  quoted-string wire incl. `stk`/`expTime`/`optType`), idx 77,202.10;
  ETH-USD **1270 rows**, idx 2,427.48; selected **32 + 32 = 64**.
  Deribit alongside: 1038 + 932 rows, 32 + 32 — **128 option
  instruments in one boot**. ZERO error lines.
- **Threads:** `okx … instruments=66` (2 static + 64 options),
  `deribit … instruments=65`; both Steady; per-arg OKX SubAcks
  accepted (no missing-channel session error).
- **Gauges:** `engine_ingress_okx_options_selected 64` +
  `engine_ingress_deribit_options_selected 64` live on :9191.
- **Capture:** okx-ticks **15,694 ticks** (~2.7 min — bbo-tbt's 10 ms
  cadence), deribit-ticks 3,987; audit-replay shows **all 64 OKX
  option streams** (`0x02000201+` = the base-512 law on the OKX venue
  byte) and every okx/deribit stream `regr=0 holes=0 missing=0
  chain_breaks=0`; both venues' integrity totals ALL ZERO. (One
  informational OUT-OF-BAND cadence-band note on a quiet option
  stream — the band display, not a violation.)
- **Raw taps (rejects mode): okx-raw.tap AND deribit-raw.tap both
  64 B header-only — ZERO parse rejects across both venues.**
- Standing lane restored + bootstrapped (pid verified); pre-smoke
  universe.toml back (options-off overnight until the operator rules
  otherwise). Note: the drain `pkill -f "multivenue-engine run"`
  pattern can match a POLLING shell that quotes the same string —
  cosmetic (exit 143 on the poll), use a `[e]`-bracketed pattern.

### Slice checkpoint — COMMIT ASK (pending operator authorization)

`M2:`-prefixed, EXPLICIT paths only:

- `crates/core-config/src/universe.rs`
- `crates/ingress-okx/src/discovery.rs`
- `crates/ingress-okx/src/lib.rs`
- `crates/ingress-okx/src/run_loop.rs`
- `crates/cli/src/universe_boot.rs`
- `crates/cli/src/paper.rs`
- `crates/cli/src/lib.rs`
- `crates/cli/src/bin/multivenue-engine.rs`
- `fuzz/Cargo.toml`
- `fuzz/fuzz_targets/okx_instruments.rs`
- `fuzz/fuzz_targets/okx_index_price.rs` (new)
- `universe.toml.example`
- `docs/m2-progress.md`

NOT staged: anything of M3's; `.env`; `~/multivenue/*`.

**Resume point if context dies here:** M2.2 code-complete + gates
green + live-smoked; commit ask pending; then M2.3 (mark/IV channel —
ALREADY UNBLOCKED by `cf132ae`): the BINDING one-new-PMLR-record
reading from the Session-0 design entry, Deribit `ticker.{opt}` + OKX
`opt-summary`, wire-format.md + migration.md entries, proptest+fuzz
per parser, audit-replay coverage/cadence row, catalog extension
(M2 lands second ⇒ extends).
