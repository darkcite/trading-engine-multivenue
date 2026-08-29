# Stage-2 FINISH plan — the single numbered list

**Operator ruling (2026-08-29 local / 2026-08-28Z):** all remaining
work is CODED FIRST — the outage fixes AND the full
`docs/venue-instrument-support-gaps.md` §1 add-list — then ONE
combined long run validates everything at once; when that run is
green, Stage 2 is considered FINISHED. This file is the ONE numbering
that supersedes the phase-letter soup for sequencing; the detail docs
stay as references. Gaps-doc §7 still holds: order submission,
dispatchers, risk/8i, keys — Stage 3, NOT here.

**Status legend:** ✅ done · 🔧 to code · 🎛 operator decision ·
▶ run phase (the one long run at the end).

**Old-label map:** R0/T1/T2/D1-D5/M5P/F0-F12 (remediation plan) and
the gaps-doc checkboxes all fold into WS0–WS13 below. When an old doc
says "Tier 3", that is WS2. When it says "#7a/#7b", that is WS1.

---

## WS0 — Operations, runnable ANY time (not coding, not the long run)

The standing lane keeps accumulating data while we code — these keep
it honest. Operator terminal, 5 minutes:

1. Revive the restart lane: `launchctl kickstart -k
   gui/$(id -u)/com.multivenue.daily-restart` (if exit 78 recurs:
   bootout + bootstrap the JOB — never `com.multivenue.engine`).
2. First run of the new script SEEDS the slot stamps (no drain). Then
   revive the three dark venues NOW with the lever:
   `echo 19700101 > ~/multivenue/state/last-restart-utc-0000`
   (drain within 60 s → fresh discovery → PM/OKX/Deribit back).
3. 🎛 #7b is ARMED: the next boot re-stages/re-commits the H6b prior
   via the new (untested) recommit child. Leave armed, or disarm by
   commenting the `recommit-ruleset.sh` line in `engine-wrapper.sh`.
4. Note: the revival boot runs the OLD binary until the long run's
   relink — T1 gauges/named errors go live only after WS13's rebuild.
   (Optional early: `cargo build --release -p cli` before the lever —
   a build is not a test — so the revival boot carries T1 already.)

## WS1 — ✅ DONE and COMMITTED (2026-08-28Z session)

`24d545a` T1 diagnosability (named session errors, ticks-based
backoff, last-tick-age + stamp-age gauges) · `9b062c1` T2 slot
restarts 0000/0830/1605 + 0020 nightly-pnl slot · `f3bd448` D3
iv_digest hourly · `0626cef` #7b post-boot re-commit ·
`09a7bbb` #7a digest POSITIONS + per-strategy P&L. All UNTESTED until
WS13. Detail: `docs/capture-remediation-plan-2026-08-28.md` §12.

## WS2 — 🔧→CODED (2026-08-29 session; untested until WS13) — Kill the OKX/Deribit failure class (was "Tier 3" items 1+2)

**CODED, compile-checkpointed green (`cargo check --all-targets` on the
6 touched crates), NOT test-run.** What landed:
- `ChannelId::SubDrop = 11` (core-types; wire-format.md row +
  migration.md entry) · `IngressStatus.sub_drops_total` +
  `ERR_SITE_ESTABLISH` (core-metrics; root re-export extended; slot
  stays 128 B) · `engine_ingress_<venue>_sub_drops_total` mirror (cli).
- Venue-error policy: fatal ONLY until first-ever subscribe evidence
  (OKX: first applied SubAck; Deribit: first FULL verification —
  process-lifetime flags surviving `reset_for_reconnect`); after that,
  non-fatal per-arg/per-channel drops: counter + 1:1 SubDrop capture
  event (§6.6 pairing) + rate-limited stderr WARN. OKX resolves the
  failing instId out of the error msg text (`extract_error_inst_id`);
  Deribit registers only the echoed subset
  (`register_confirmed_subs(found)`), emits per-missing-bit drops, and
  completes the pending id on RPC errors.
- Establishment budget (shared pattern in
  `core_net::{ESTABLISH_BUDGET_NS, establishment_expired}`, 30 s
  default, per-driver override for tests): both run loops return the
  new `RunResult::EstablishTimeout` when zero subscriptions confirm in
  budget — NOT gated on `Steady`, kills the §5.3 Connecting wedge AND
  zero-sub pong-alive sessions. paper.rs needs no change (matches! are
  non-exhaustive; EstablishTimeout ≠ IdleTimeout ⇒ backoff escalates).
- Tests written (run at WS13): OKX +6 (boot-fatal kept, post-ack drop,
  expired-instrument reconnect keeps spot flowing, sym-none drop,
  establish-timeout fires/disarmed) · Deribit +6 (boot-fatal kept ×2,
  reconnect missing-channels drop + statics flow, folded option row,
  discriminator arming, rpc-error nonfatal, establish ×2) ·
  core-net +2 · core-metrics +2 · core-types roundtrip extended.
- STILL OPEN in WS2 (deliberately): §5.4 chronic-churn named-cause fix
  — gated on the first live hour on the T1 binary (▶ WS13 names the
  code; the non-fatal drop path likely absorbs the class meanwhile).

The 08:00Z settlement still kills both venues today; WS1 only shrinks
the darkness to ~30 min and names the error. WS2 removes the class:

- Per-arg subscribe failures become NON-FATAL DROPS: a venue
  error/missing-channel on reconnect drops that instrument from the
  session's subscribe set (loud log + counter + §6.6 capture event);
  fail-fast narrows to BOOT (first-ever subscribe of a config must
  still refuse venue-blind boots). Files: `ingress-okx` /
  `ingress-deribit` run loops + driver sub-tables; tests: expired-
  instrument reconnect keeps spot/perp flowing.
- Connect + session-establishment timeouts NOT gated on `Steady`
  (outage §5.3 wedge): a session stuck in Connecting/AwaitingUpgrade
  past a budget tears down and retries. Shared pattern in
  `crates/cli/src/paper.rs` connect path + the two run loops.
- §5.4 chronic OKX churn: once the first live hour on the T1 binary
  names its code (▶), fix the named cause here.
- STILL DEFERRED (Stage-3-adjacent, needs manifest epochs): mid-run
  chain RE-DISCOVERY. The 0830/1605 slots remain the chain-refresh
  mechanism; WS2 makes the sessions survive in between.

## WS3 — 🔧→CODED (2026-08-29 session; untested until WS13) — Small venue-data fixes (gaps §1, parsed-but-dropped class)

- ✅ Deribit funding emit: perp tickers now emit a paired
  `ChannelId::Funding` event (v0 = `current_funding` ×1e9, v1 = 0 —
  continuous funding) beside the Ticker event.
- ✅ settlement_period gating, two halves: (wire) `parse_ticker` makes
  `current_funding` OPTIONAL with a `has_funding` frame flag — fixes
  the latent bug where every DATED-future ticker was rejected
  wholesale — and the Funding emit gates on it; (boot) `run_deribit`
  discovery audit now counts + names configured dated futures
  (`dated=` on the coverage line; `row.perpetual` finally used).
- ✅ HL `premium`: parsed (optional, signed, ×1e9) into
  `HlAssetCtxFrame.premium_1e9`; rides the AssetCtx event's
  `venue_seq` slot bit-cast i64→u64 (slot was constant 0; M4
  hash128-packing precedent; wire-format.md documents it).
- Tests: deribit lib +1 / run_loop +1 (+2 updated counts for the
  double emit) · hl lib +1 extended +1 new / run_loop +1.
  `cargo check --all-targets` green on the 4 touched crates.

## WS4 — 🔧→CODED (2026-08-29 session; untested until WS13) — Reference-data REST, existing five venues (gaps §1/§5)

**Placement law implemented as written:** static metadata → boot
discovery; periodic series → the NEW worker lane.

- ✅ Worker lane `claude_worker.refdata` (a MODULE, never a verb —
  candles/iv_digest precedent): hourly-bucketed snapshots in a new
  `refdata` table BESIDE candles in candles.db, PK (venue, descriptor,
  kind, hour_ts), kinds `vol24h_quote`/`oi`, RAW venue units,
  per-venue RestBudget, universe lanes REUSED from candles, injectable
  Http (no live calls in tests). Implements: BN spot vol24h · BN USDM
  vol24h + OI (`/fapi/v1/openInterest`) · Deribit
  `get_book_summary_by_instrument` (ONE call → vol_usd + OI). OKX and
  HL lanes report "deferred to WS7/WS8" loudly. +12 pytest
  (`test_refdata.py`); py_compile clean.
- ✅ BN static tick/lot: `BnSymbolRow` grows `tick_size_1e9` /
  `lot_step_1e9` (0 = absent — old fixtures parse), parsed from
  `filters[]` PRICE_FILTER.tickSize / LOT_SIZE.stepSize (order-
  independent, structural skip for foreign filters, strict quoted-
  decimal → ×1e9 with sub-1e-9 rejection). +3 discovery tests.
  NOTE for WS13: the `binance_exchange_info` fuzz target covers a
  MODIFIED parser — re-run it ≥300 s with the new-target batch.
- HL tick/lot + vol → WS8; OKX vol/OI → WS7 (venue workstreams).

## WS5 — 🔧→CODED (2026-08-29 session; untested until WS13) — Binance expansion (gaps §2.1)

- ✅ `@markPrice` lane: `BnMarkPriceFrame` + `parse_mark_price` (new
  fns — the frozen bookTicker parser bytes untouched; required
  `"e":"markPriceUpdate"` tag; `"r":""` on dated contracts ⇒
  `has_funding=0`, the WS3 convention) · `StreamLane::MarkPrice` +
  `Driver::new_mark_price` · capture events: Mark (v0 = mark ×1e6,
  v1 = INDEX ×1e6 — BN-only, documented in wire-format.md) + Funding
  (v0 = rate ×1e9, v1 = next-funding ms) gated on wire truth ·
  DEFAULT-ON: every usdm + dated instrument gets one extra
  `@markPrice` conn in the multi lane (conn counts grow at the next
  boot — expected in WS13's smoke).
- ✅ Dated futures named as a class: `[binance] usdm_dated`
  (`<base>_<yymmdd>`, underscore REQUIRED — provably disjoint from
  `usdm` by alphabet), ordinals from `BN_DATED_ORDINAL_BASE = 2048`,
  descriptor stays `binance-usdm:<sym>` (one fapi lane offline);
  discovery parses `contractType`/`deliveryDate` into `BnSymbolRow`
  (`BnContractType` enum, unknown ⇒ Other, never fatal) and the boot
  audit REFUSES a non-dated symbol in `usdm_dated` (`not_dated`) ·
  bin wires dated bookTicker + markPrice conns; ai-universe snapshot
  includes the dated block; legacy flag boots byte-identical.
- ✅ Worker: `usdm_dated` seeds map names (base-2048 law in
  fetchers.py) and joins the binance-usdm candles lane (klines serve
  delivery symbols) — refdata inherits automatically.
- Tests: bn lib +4 (markPrice parse) + 2 proptests (roundtrip +
  never-panics) · run_loop +3 (event pinning, dated, foreign-reject) ·
  discovery +3+2 (filters, contract class) · core-config +1 big
  (parse/allocate/validators) · NEW fuzz target `binance_mark_price`
  (registered in fuzz/Cargo.toml — WS13 runs it ≥300 s with the
  bybit batch; `binance_exchange_info` re-run per the WS4 note).
  `cargo check --all-targets` green ×3 crates; py_compile clean.

## WS6 — 🔧→CODED (2026-08-29 session; untested until WS13) — Deribit expansion (gaps §2.3)

**Design pivot recorded here:** all three items route through ONE new
policy fn `row_wants_channel` (lib) that now drives subscribe-building
AND the WS2 verification-mask/registration/drop sites — the two can
never drift again. Row classes: static future (q/t/tr[+b]) · static
SPOT (no ticker — name-shape law: no `-` ⇒ spot, `BTC_USDC`) ·
option (q+t) · combo (quote-only).

- ✅ Spot lane: `is_spot_row` (derived, no config change — spot names
  go straight into `[deribit] instruments`); ticker never subscribed
  (spot has no funding/OI/mark analytics; also avoids the
  parse-reject noise); discovery gains `kind=spot` pages
  (`ingest_spot_body`; `settlement_period`/`contract_size` optional
  for spot rows, defaults false/1.0), fetched ONLY when a configured
  name is spot-shaped; the WS3 dated log excludes spot. ⚠ pitfall-11:
  the spot page row shape (`state` field presence) is NOT live-proven
  — WS13's smoke should include one spot instrument.
- ✅ DVOL: derives from `[deribit] options_underlyings` (BTC →
  `btc_usd`; no new key) · `ChannelId::VolIndex = 12` (wire-format
  row + own migration entry; v0 = points ×1e9, v1 = underlying
  ordinal, sym = SYMBOL_ID_NONE) · `parse_vol_index` +
  `DeribitMsgKind::VolIndexPush` + capture emit · Driver
  `new_with_dvol` (additive; `new` delegates) · DVOL channels are
  OUTSIDE the verification mask (u128 fully allocated) — absent echo
  = missing series, never a verdict · NEW fuzz target
  `deribit_vol_index` + 2 proptests.
- ✅ Combos: `[deribit] combos` list (explicit names; cap 64;
  cross-list dup check vs instruments) → allocation base
  `DERIBIT_COMBO_ORDINAL_BASE = 1024` → table tail via
  `insert_combo` (options+combos SHARE the 64-row/64-bit tail block —
  partition law static→options→combos, new `OptionAfterCombos` err) ·
  quote-only BBO → Tick capture · NO boot REST validation BY DESIGN:
  the subscribe echo is the validator (misspelled combo ⇒ first-ever
  verification fails ⇒ boot fail-fast) · `--deribit-symbols`
  override drops combos (section-replacement law, warned).
- Tests: deribit lib +4 big (policy end-to-end, partition/capacity,
  DVOL parse/classify, subscribe rendering) + run_loop +3 (DVOL
  event, spot verification/registration/flow, combo
  verification/flow) + core-config +cross-dup + core-types roundtrip.
  `cargo check --all-targets` green ×4 crates.

## WS7 — 🔧→CODED (2026-08-29 session; untested until WS13) — OKX expansion (gaps §2.4)

- ✅ REST twin chosen (the WS4 placement law: periodic series → the
  worker lane, never the engine): refdata okx lane —
  `market/ticker` → `volCcy24h` → vol24h_quote per instId; OI
  (`public/open-interest` → `oi`, contract units) fetched for
  DERIVATIVE instIds only (≥3 hyphen segments; the venue errors OI on
  spot). +2 parser tests +1 cycle test.

## WS8 — 🔧→CODED (2026-08-29 session; untested until WS13) — Hyperliquid expansion (gaps §2.5)

- ✅ 24h volume: refdata hl lane — ONE `metaAndAssetCtxs` body per
  cycle covers the whole perp universe (`dayNtlVlm` → vol24h_quote,
  USD notional); `@spot`/`#outcome` coins skipped (no ctx on this
  endpoint — the `coin_wants_asset_ctx` law). `activeSpotAssetCtx`
  stays deliberately unsubscribed. +1 parser +1 cycle test.
- ✅ Tick/lot: discovery already captured `szDecimals` (the gaps doc
  aged); the missing derivation now exists —
  `HlAssetInfo::max_price_decimals()` (perp price-tick rule:
  ≤ 6−szDecimals decimals, ≤5 sig figs; lot step = 10^-szDecimals)
  + the hl audit row logs `sz_decimals`/`max_price_decimals`.
  +1 discovery test.

## WS9 — 🔧→CODED (2026-08-29 session; untested until WS13) — Bybit, the sixth venue (gaps §1)

**Design deltas from the sketch, recorded:** BBO comes from
`orderbook.1.<SYM>` on BOTH classes (spot `tickers` has no bid/ask;
uniform parser; per-symbol snapshot/delta BBO state, one-sided books
never emit) — there is no `--bybit-depth` flag (depth-1 IS the BBO
lane; deeper books are a later want). `tickers.<SYM>` rides LINEAR
conns only (mark v0 + index v1 = the WS5 shape · funding v0 + next
ms v1 · OI in a Bybit-mapped `Ticker` event v0=0 v1=OI — wire-format
notes). Trades carry venue_seq 0 (UUID ids — NO §6.2 chain law on
this venue, documented). Subscribe acks are ALL-OR-NOTHING on this
venue ⇒ WS2 semantics at request granularity (boot refusal fatal;
post-first-success refusal = drop + establishment-budget reap).
Spot+linear = 2 conns, ONE thread, one producer (`run_multi`, the BN
M1 shape, WS2 establishment budget built in).

- ✅ `VenueId::Bybit = 6` (Ai keeps 5 ⇒ lane↔venue identity broken:
  NEW `engine::tick_lane_of`, Bybit = TICK LANE 5, `NUM_TICK_LANES`
  = 6, alloc-gate pin updated 5→6) · `crates/ingress-bybit` (lib +
  run_loop + discovery; ~2.2k lines; 8 lib tests + 4 proptests + 9
  run-loop tests + 4 discovery tests + 1 discovery proptest) · fuzz
  `bybit_ws_frame` + `bybit_instruments` registered.
- ✅ Discovery: paged `instruments-info` (nextPageCursor walk),
  liveness + tick/lot (`priceFilter.tickSize`,
  `lotSizeFilter.qtyStep`/`basePrecision`) — the WS4 parity line;
  boot audit `run_bybit` per configured category.
- ✅ Constants ripple: VENUE_LABELS ×2 → 7 (+`bybit`),
  `TRADEABLE_VENUES` = 6 with the NEW `tradeable_venue_byte`
  predicate (Ai = 5 sits INSIDE the byte range — a plain bound would
  have made the command feed "tradeable"), `ModelParams` [7] with a
  DEAD Ai slot 5 (bybit latency default 100 ms), Rings/Consumers/
  bench/wiring-test lane arrays ×6, IngressStatusSet + full metrics
  family (`engine_ingress_bybit_*`, capture + coverage gauges,
  last-tick-age [7], TUI health bit 7, raw-tap label `bybit`),
  `[bybit] spot/linear` grammar (UPPERCASE [A-Z0-9]; linear base
  512), Config hosts BYBIT_WS_HOST/BYBIT_REST_HOST, bin spawn on
  core 8, wire-format + migration entries.
- ✅ Worker: `VENUE_BYBIT=6` · map seeding (`bybit:`/`bybit-linear:`,
  base-512 law) · candle lanes 6+7 (`parse_bybit_kline` —
  newest-first wire normalized; 1d floor 2018, untested vs start=0 —
  pitfall-11 note for WS13) · refdata tickers lane (one call: vol +
  linear OI). +2 candle tests +1 refdata test; py_compile clean.
- No new external deps (same stack) — license-deps not needed;
  license-check green (new files carry SPDX; the tracked-file count
  grows at `git add`).
- WS13 NOTE: fuzz the two bybit targets ≥300 s; live smoke must
  include ≥1 spot + ≥1 linear symbol; the all-or-nothing-ack
  assumption (subscribe) and orderbook.1 zero-size-clear semantics
  are wire-UNPROVEN (pitfall 11) — the smoke names them.

## WS10 — 🔧→CODED + GATED (2026-08-29; operator approved D-A1…D-B3 as recommended, then A+B were coded, gated and committed the same day)

**Review outcome (operator, 2026-08-29, via AskUserQuestion): D-A1
ChannelEvent-as-carrier ✓ · D-A2 EVENT_RING_SIZE=1024 ✓ · D-B1
slot_kind 7 + `<venue>-depth.pmlr` capture ✓ · D-B2 K=5 / ladder 64 /
depth ring 4096 ✓ · D-B3 both now, A then B, gates once over both ✓.**

**Landed (commit series `M-WS10a` ×3 + `M-WS10b` ×3 + docs):**
A = venue-event lanes exactly per the design (core-types
EVENT_RING_SIZE/event_lane_bit/EVENT_LANE_FUNDING; engine
NUM_EVENT_LANES=6 drain + events_dispatched; defaulted
`on_venue_event` + set fan-out; gated try_push at the four funding
capture sites, capture-first; event_ring_drops counter+mirror; cli
rings/spawns/capture-only drain; bench engine test pushes+drains one
funding event/iteration at 0 B/op; per-venue lane tests incl. mask-0
and ring-full). B = depth per the design + ONE discovered format
amendment: **PMLR slot size became KIND-determined (kinds 0–6 = 64 B,
kind 7 = 192 B)** — the 64 B pin was load-bearing in
PmlrWriter/PmlrReader and the design's "container version unchanged"
survived as: version stays 2, readers decode kind first
(migration.md entry). book_builder::ladder (SoA sides, evict-worst at
cap, beyond_cap counter, proptests vs BTreeMap incl. never-panic);
okx/deribit level walkers (+ parser tests: snapshot/delta/delete,
sci-notation, malformed rejects; fuzz targets okx_book_levels +
deribit_book_levels); Book dispatch arms restructured (chain apply +
BookGap event moved to phase 1 where the payload is in scope; resync
queueing stays phase 2 — behavior-identical, pinned by the standing
tests); change-gated emission + STALE-on-gap; engine
NUM_DEPTH_LANES=2/depth_lane_of/on_depth; depth_ring_drops
counter+mirror; audit-replay depth stream+totals; cli plumbing.

**Gates over A+B (2026-08-29): nextest 1345/1345 (+29) · alloc 38/38
0 B/op (engine test now proves event+depth push/drain in-window) ·
pytest 477 (untouched) · make lint green · fuzz okx_book_levels +
deribit_book_levels 301 s clean each · license-check green. The
stay-greens move to 1345/38/477.**

Original design summary (approved): Summary: (A) funding carrier =
per-venue `ChannelEvent` lanes (the §6.5 POD reused; ring 1024×64 B
×6; ingress pushes at the four existing funding capture sites;
`Strategy::on_venue_event` defaulted no-op; `event_lane_mask` gates
the lane to Funding-only in v1) — land first, ~1 day. (B) L2 depth =
in-ingress bounded ladder (64 levels/side, in-place deltas) emitting
change-gated `DepthTopK` PODs (192 B, top-5/side, slot_kind 7 +
`<venue>-depth.pmlr` capture; `Strategy::on_depth`) — own slice,
1.5–2 days. Five operator decisions (D-A1..D-B3) listed in the doc.
WS11/WS12 do not depend on WS10 and proceeded.

## WS11 — 🔧→CODED (2026-08-29 session; untested until WS13) — Worker offline lanes + leftovers

- ✅ Funding history: NEW `claude_worker.funding` module (a MODULE,
  never a verb) — `funding` table beside candles, PK (venue,
  descriptor, ts_ms), INSERT-OR-IGNORE idempotence (history is
  immutable); lanes select ONLY funding-bearing instruments by the
  engine's class laws (usdm minus dated · OKX `*-SWAP` · Deribit
  PERPETUAL names · HL perp coins · bybit linear); 5 wire parsers;
  depth = one NEWEST page per instrument per cycle (BN ≈333 d ·
  OKX ≈33 d · Deribit 30 d range · HL 33 d · Bybit ≈66 d — deeper
  pagination is a recorded extension, not v1). +5 pytest
  (`test_funding.py`).
- ✅ D5 fold-ins LANDED: `pmlr.py` now owns
  `SLOT_KIND_OPT_SUMMARY = 6`, `OptSummaryRec`/`OptSummaryReader`
  and `run_anchor_ns`; iv_digest's local reader + BOTH
  `_run_anchor_ns` copies (candles too) retired to aliases — zero
  test churn by construction.
- ✅ D4 ruling recorded as (i) — ZERO code (documented in
  `docs/m5-runbook-notes.md` §3; reading (ii) stays un-built unless
  overruled).
- ✅ M5 runbook snippet: `docs/m5-runbook-notes.md` — #7a
  `gather_positions_payload` with the map's `hip4_pairs` (serialized
  invocation, `.env` seam), plus the five offline-module lanes and
  the candles.db table inventory. The pinned ai-session prompt is
  untouched.

## WS12 — 🔧→CODED (2026-08-29 session; untested until WS13) Mechanical hygiene, LAST before the run

`cargo fmt` (~88 files) + `clippy -D warnings` (~40 lints) ⇒
`make lint` green. Sequenced last so the churn never collides with
WS2–WS11 diffs. Own commits, no logic changes.

**Landed 2026-08-29 (same session as WS2–WS11; uncommitted):**

- `cargo fmt` across the workspace — 106 files reformatted, whitespace
  only; `git diff --summary` shows ZERO mode changes (the 2026-08-27
  exec-bit pitfall audited explicitly).
- clippy `-D warnings`, three `--keep-going` waves (~67 sites):
  - **Doctrine-conflicting lints got targeted `#[allow]` + comment,
    NEVER the suggested rewrite** (existing `too_many_arguments`
    precedent): `needless_range_loop` on the binance/bybit `run_multi`
    hot poll loops (`i` is the mio Token identity), on
    `ingress-polymarket` `queue_market_subscribe` + the
    `write_market_subscribe_multi` serializer (raw-index doctrine;
    PM parser internals left byte-untouched);
    `large_enum_variant` on binance `StreamLane` (Eapi payload
    deliberately inline — `Box` forbidden, one slot per conn,
    preallocated at boot).
  - **Mechanical equivalents everywhere else** (identical codegen, no
    logic change): `len() == 0`→`is_empty()` in every venue `flush_tx`
    + the 3 rx-stall guards + audit-replay; proptest
    `match {Ok…, Err(_)=>{}}`→`if let Ok` in all 5 discovery crates;
    `?`-operator for the two `memmem::find` guards (hl
    `parse_sub_response`, bn `parse_mark_price` — the latter is
    WS5-new code, not a frozen parser); `Some(span) if
    span.is_empty()`→`None | Some([])` (eapi); `.iter().copied()
    .collect()`→`.to_vec()`; `.get(&k).is_none()`→`!contains_key`
    (fill tests); `.iter().any(|r| *r == x)`→`.contains(&x)`
    (audit-replay); inline format args ×6; `push_str("…")`→
    `push('…')`; needless `&PathBuf`→`&Path` ×3 (test helpers);
    elidable lifetime (audit-replay `stat_for`); `u64 as u64` cast
    dropped; `explicit_counter_loop` ×2 in cli boot parsers rewritten
    with `enumerate()` (okx legacy table: ordinal now derived
    `(n_seen+1)` — lockstep proven, missing rows still burn ordinals;
    raw-tap label parser); `assert!(const)` ×2 in strategy-set
    promoted to `const _: () = assert!(…)` compile-time pins;
    `doc_lazy_continuation` ×5 (doc lines starting with `+ ` parse as
    markdown list bullets — reworded: okx discovery, paper.rs
    CaptureGaugeIds, bench ×2, audit-replay) + one blockquote-lazy
    (`>6 fractional digits` reworded, ingress-ai).
- Final state, all run on the Mac: **`cargo fmt --check` clean ·
  `make lint` GREEN (clippy `-D warnings`, all targets, zero errors) ·
  `make license-check` OK (198 files) · zero mode changes.**
- NOT touched: `backtest.py`/`cli.py` (frozen), any `.py` (fmt/clippy
  are Rust-only), wire formats, test semantics.

**WS12 exit state: WS2–WS12 ALL CODED ⇒ the coding phase of this plan
is COMPLETE. Remaining: operator commit authorization (explicit paths,
`M-` prefixes, license-check before each), the WS10 design review
(`docs/ws10-engine-plumbing-design.md` — code only after approval),
then WS13.**

## WS13 — ▶ THE ONE LONG RUN (everything validated at once)

**LIVE PHASE RUN 2026-08-29 (operator ruling: soak amended 7 days →
1 HOUR — "we don't have 7 days"). Timeline (all UTC, all this
session):**

- 07:17 revive-lever reboot onto the WS10 binary → full universe
  amendment (okx/deribit `depth = true`, Deribit `BTC_USDC` spot,
  `[bybit]` BTCUSDT spot+linear) → **live catch #1 (pitfall 11):
  Bybit spot discovery BadRow boot-loop — live `basePrecision` runs
  to 1e-13 (BTTUSDT/BABYDOGEUSDT) and the WS4-inherited sub-1e-9
  reject killed the page. Fix: Bybit `quoted_1e9` truncates past 9
  fraction digits (floor → 0 = absent sentinel), regression test on
  the exact live rows** (`98d4db2`).
- **Live catch #2: GaugedCapture never forwarded `Capture::depth` —
  the trait default silently swallowed every snapshot (zero depth
  records while Book events flowed on both venues). Forward + a
  test pinning EVERY per-record hook** (same commit). Verified live
  within 2 min: okx-depth 242 / deribit-depth 258 snapshots.
- 07:33:53 soak boot (run-1787988833603587000). T+32 min: okx-depth
  5 735 · deribit-depth 4 933 snapshots · bybit 41.9k ticks + 35.2k
  events (mark/funding/OI) · bn 409k · pm 27.3k · hl 16.8k ticks ·
  9 271 stamped order intents.
- **08:00Z SETTLEMENT CROSSED ALIVE — the WS2 proof.** Deribit and
  OKX tick files grew straight through the hour that produced the
  6-day outage; okx carried **1 441 `SubDrop` events** (non-fatal
  per-arg drops with §6.6 evidence) instead of dying; deribit
  crossed with zero drops. Settlement-adjacent churn on deribit
  (~07:59) stayed non-fatal.
- **§5.4 NAMED (the residual's precondition met):** okx chronic
  churn = `err_site=pump io_kind=other venue_code=0`, ~1/s,
  508 200 log lines over Aug 25–29 — PRE-EXISTING (present on the
  old binary for days), unaffected by depth on/off. The fix is its
  own slice as planned, now with the named code.
- **BN markPrice: venue-side unreachable from this network** (the
  M2.4 eapi-WS precedent): hand-rolled WSS probe — `@bookTicker`
  pushes instantly, `@markPrice`/`@markPrice@1s`/combined-stream
  upgrade then stay silent on fstream from here. Code correct; the
  lane idles harmlessly; BN funding evidence deferred to the venue.
- **#7b recommit proven live in fail-safe form:** the boot waiter
  armed at every boot and REFUSED with the named reason (bound
  paths in cleared /tmp) — the designed refuse-don't-guess posture.
  Re-binding the prior to durable paths = the M5 runbook re-commit
  step.
- **D1 fetch:** every REST lane clean, conflicts=0, unresolved=0 in
  the mapped universe (the 128 flagged are per-boot okx/deribit
  OPTION ordinals, which resolve via options-manifest.tsv by the
  M2-close law — map names never apply to them).
- **C6:** step 1 **PASSED — trailing gap-free streak 6 (Aug 23–28)
  vs the ≥3 tell**; step 2 done (7 boot-loop empties archived; the
  two pre-M3 aborted dirs were already gone). Step 3 executed
  through the FROZEN argv over a 27-run/19M-tick multi-day subset
  (validator + merge + fill model + schema-1 all proven; 657
  in-sample fires) but the `oos.trading_days >= 2` sub-gate is
  STRUCTURALLY blocked today: the full-root merge (~27 GB of
  MergeKeyed) exceeds this Mac's 24 GiB, and every RAM-feasible
  subset's OOS tail lands on Aug-28 — a PM-dark legacy day (dailies
  expired 16:00Z Aug-27 under the pre-T2 single-restart regime).
  With T2's three daily refreshes now live, two PM-healthy days
  accumulate by Aug-31 — the bounded rerun then satisfies the
  sub-gate. OPERATOR CALL: bless the close on streak+machinery or
  order the Aug-31 rerun.

**SOAK VERDICT (run-1787988833603587000, 07:33:53→08:30:53Z = 57 min
+ the post-0830 session continuing; the 0830 T2 slot fired on the
minute and closed the run — its own live proof):**

- **audit-replay integrity: pm/bn/okx/hl/bybit ALL ZERO**
  (regressions / trade holes / missing ids / chain breaks); deribit
  6 trade holes / 68 missing ids (venue-side feed holes across its
  churny sessions, chain_breaks=0 — the §6.6-paired class).
- **Depth (WS10-B live): okx 9 678 snapshots / 2 syms / STALE=0 ·
  deribit 8 687 / 2 / STALE=0** — change-gated emission at ~2.8/s
  per book, zero broken-book emissions.
- Bybit sixth venue: clean integrity, spot+linear ticks + the
  mark/funding/OI event stream throughout.
- Options analytics: deribit 102 178 opt-summaries (mark+OI on
  every record) · okx 13 651.
- The audit's sub_drop streams hand §5.4 its shape: each ~1 s okx
  session's re-subscribe takes per-arg refusals on the OPTION block
  (~0.64/s per sym ×20+ syms + 7.63/s venue-global) before dying at
  pump/other — the named lead for the §5.4 fix slice.
- **Soak = GREEN under the operator's 1-hour amendment.** Remaining
  before Stage 3 (unchanged set, now all named): §5.4 okx churn fix
  (evidence above) · C6 fill-days sub-gate (operator bless or
  Aug-31 rerun) · M5 on go · the §7 entry gate.

**GATES PHASE RUN 2026-08-29 (operator-ordered "test everything"; live
phase still pending).** Results + the regression findings it existed to
catch:

- **Found + fixed — the WS9 seventh-venue test-world desyncs** (latent
  because the no-tests-until-WS13 ruling deferred all suites; every fix
  below is test-or-bug-scoped, no design change):
  1. `paper.rs` raw-tap capacity test fed 6-era labels + 1 — now all
     seven + an eighth (`bybit` in the list; stale "six venues"
     docstring corrected).
  2. `core-types` VenueId: round-trip list gained `Bybit`;
     rejects-test boundary 6 → 7 (6 IS Bybit now).
  3. **REAL BUG:** `backtest.rs` `RunSummary.venue_records` was still
     `[u64; 6]` while `VENUE_LABELS` grew to 7 ⇒ every render_summary
     (incl. the REAL BINARY on any capture dir) panicked OOB. Fixed +
     future-proofed: `[u64; VENUE_LABELS.len()]`.
  4. Same class in `capture_catalog.rs` (M3-owned; bugfix-scoped
     touch): 5 sites `[…; 6]` → `VENUE_LABELS.len()`-tied.
  5. `ingress-binance` discovery `parse_filters`: end-of-buffer at the
     key's `:` returned `BadRow` — against the parser's own
     convention; now `Truncated` (code fix, the WS5 test was right).
  6. `iv_digest._OPT` alias restored beside OptRec/OptReader (D5
     fold-in had dropped the struct alias the fixture-packing tests
     use).
- **Gate results:** nextest **1316/1316** (+1 known skip; baseline
  1240 → +76 new tests) · alloc **38/38 0 B/op** (fresh `Compiling
  bench` ×2 confirmed; release binary relinked WITH all WS2–WS12 +
  the fixes above) · pytest **477/477** (baseline 439 → +38; frozen
  202 untouched inside) · fuzz **5/5 CLEAN, 301 s each, ZERO crash
  artifacts**: binance_mark_price 271.2M execs cov 97 ·
  deribit_vol_index 80.1M cov 139 · bybit_ws_frame 17.4M cov 207 ·
  bybit_instruments 18.7M cov 121 · binance_exchange_info re-run
  14.8M cov 380 (re-fuzzed BECAUSE finding 5 touched its parser).
  **New stay-greens: 1316 / 38 / 477.**
- Post-fix hygiene re-verified at the end of the session: `cargo fmt
  --check` + `make lint` + `make license-check`.
- **Live phase (steps 3+ below) NOT run:** it stops the standing
  capture engine mid-C6-window — operator's call, per this plan.
  NOTE: the standing lane picks up the freshly-relinked release binary
  at its next 00:00Z restart regardless (established M4-close
  pattern).

**§5.4 CHURN — ROOT-CAUSED + FIXED + GATED (2026-08-29 session, after
the docs-archive commit `7d6518a`; live proof pending the next engine
restart):**

- **The venue was innocent and the named lead was a passenger.** The
  current standing run (post-08:30Z boot, fresh 1400/1400-live chain)
  carried **ZERO SubDrops, zero gaps, zero resubscribes** — every
  subscribe arg accepted — yet 2 699 reconnects at the ~1.4 s
  `pump/other` cadence. The per-arg OPTION refusals seen in the soak
  hour were the post-settlement expired-instrument class riding the
  churn, not causing it. A hand-rolled WSS probe replicating the
  engine's EXACT batched subscribe (66 instruments, 4 064 bytes —
  32 under OKX's 4 096 cap, `_UM` rows included) from this Mac
  survived 90 s / 6 000+ msgs / ping-pong clean.
- **ROOT CAUSE (ours, `core-net::TlsTransport`):** rustls 0.23 hard-
  caps buffered received plaintext at **16 KiB**
  (`DEFAULT_RECEIVED_PLAINTEXT_LIMIT`, `common_state.rs:1055`, NOT
  configurable — `set_buffer_limit` is send-side only) and its
  `read_tls()` signals BACKPRESSURE with `ErrorKind::Other`
  ("received plaintext buffer full", `conn.rs:761`; the rustls doc
  says exactly this). Our `drive_tls` looped `read_tls` +
  `process_new_packets` WITHOUT draining plaintext between
  iterations and treated every error as fatal ⇒ ANY poll wake with
  >16 KiB decryptable queued = session death at `err_site=pump
  io_kind="other" venue_code=0`. OKX is the venue whose NORMAL
  bursts qualify — `books` 400-level snapshots ≈25 KiB in ONE frame,
  `opt-summary` family pushes ≈600 KiB, post-subscribe burst = MBs.
  **Aug-25 onset = OKX listing the `BTC-USD_UM`/`ETH-USD_UM` option
  families** (manifest shows the `_UM` rows interleaved in our
  capped chain): venue-side burst growth, zero repo change — matches
  every observed property (pre-existing, depth-toggle-indifferent,
  all-day cadence). Deribit's "churny sessions" in the soak = the
  same bug at lower rate through the shared transport.
- **FIX (`crates/core-net/src/transport.rs`, both sides of the drain
  protocol):** (a) `drive_tls` treats the `ErrorKind::Other`
  backpressure signal as break-not-die (kind-match is exact: raw OS
  errors surface as specific kinds/`Uncategorized`, never `Other`;
  TLS protocol failures keep surfacing via `process_new_packets` as
  fatal `InvalidData`); (b) `Transport::read` gained a pull-through:
  when plaintext runs empty it decrypts the next wave from the
  queued ciphertext (`read_tls` + `process_new_packets`, with a
  no-progress guard), so the existing fill-until-WouldBlock loops
  drain an arbitrarily large burst within ONE poll iteration —
  edge-triggered-safe (kqueue/mio re-fires only on NEW bytes), same
  buffers, same copy count, zero allocation. Heals all six WSS
  venues at once.
- **Red→green proof:** NEW `crates/core-net/tests/tls_burst_loopback.rs`
  (rcgen/rustls loopback, transport-only): a 256 KiB single-write
  burst — 16× the cap — failed PRE-fix with the exact production
  error (`Custom { kind: Other, "received plaintext buffer full" }`
  at the pump) and passes POST-fix with every byte intact; a
  small-trickle fast-path guard passes both sides.
- **Gates re-run this session: nextest 1349/1349** (+2 = the new
  loopback pins; +1 known skip) · **alloc 38/38 0 B/op** (fresh
  `Compiling bench` confirmed) · **pytest 477** (worker untouched) ·
  `cargo fmt --check` + `make lint` + `make license-check` green ·
  fuzz: NO new target — no untrusted-bytes parser changed (§21.4
  scope; the deterministic loopback covers the transport seam).
  **Stay-greens now 1349 / 38 / 477.** Release binary RELINKED with
  the fix (G0) — the standing lane boots it at its next restart
  (T2 slot or revive lever); expected live tell: okx
  `run-loop returned` churn stops, sessions go long-lived,
  `reconnects_total` flatlines, opt-summary capture rate jumps to
  full-family cadence.
- **LIVE-PROVEN same session (commit `00c13bf`, operator-ordered
  revive at 10:28Z → run-1787999341626410000 boot 10:29Z):** every
  predicted tell landed — okx `run-loop returned` lines since boot
  **0** (old binary: ~40/min) · okx **reconnects_total 0** across
  the first 8 min on ONE unbroken Steady session (46 891 ticks,
  sub_drops 0, last_tick_age 0) · **deribit reconnects_total 0**
  (the same-bug healing) · okx-opt-summary.pmlr **1.26 MB in 8 min
  vs ~874 KB in the ENTIRE 57-min soak hour** — the ~600 KiB
  full-family pushes that used to kill the session are now consumed
  whole (~9× capture rate). §5.4 is CLOSED; the 508 200-line churn
  class is dead on all venues.

1. Full build (`cargo build --release --workspace`; G0 relink).
2. Gates: `cargo nextest run --workspace` (baseline grows well past
   1247) · alloc assertions 0 B/op (`--test-threads=1`, fresh
   `Compiling bench`) · `uv run pytest` (frozen 202 untouched) ·
   every NEW fuzz target ≥300 s (bybit ×2 + any new parser targets) ·
   `make license-check` (+ `license-deps` only if a dep changed).
3. Reboot standing lane on the new binary (drain lever). Live smoke
   ALL venues incl. Bybit: discovery counts, manifests, audit-replay
   integrity zero, `/metrics` gauges live, recommit.log evidence.
4. Named-error + revival verification across one settlement cycle:
   08:00Z survives (WS2) or self-names + 0830 revival; 16:05Z PM
   check (§5.6 tell); next-day catalog: gap-free, per-venue coverage
   ≈24 h.
5. C6/M3 close (arithmetic already met: streak 5 ending 2026-08-27) +
   D1 `fetch` (`unresolved=0`).
6. The 7-day soak on post-fix, full-scope capture (M6's shape) with
   the nightly pnl + hourly candles/iv lanes running.
7. Soak green ⇒ **declare Stage-2 FINISHED**: close entries, CLAUDE.md
   CURRENT STATE rewrite, operator commits, operator push.

**Discipline during WS2–WS12:** compile checkpoints (`cargo check` /
`py_compile`) after each workstream are sanctioned (a build is not a
test); NO test suites, NO live boots before WS13 except WS0. Commits
at each workstream boundary on operator authorization, explicit
paths, license-check each time. If context runs short mid-workstream:
write resume state into THIS file under the workstream heading and
tell the operator.

**Rough coding effort:** WS2 1–1.5 d · WS3 0.5 d · WS4 1–1.5 d ·
WS5 1 d · WS6 1 d · WS7+WS8 0.5–1 d · WS9 2–3 d · WS10 1.5–2 d
(incl. design docs) · WS11 1 d · WS12 0.5 d ⇒ ≈ 10–13 working days of
coding, then WS13's 1–2 days + 7-day soak calendar.

---

## KICKOFF PROMPT for the next session (paste verbatim)

> Read, in order: `CLAUDE.md` CURRENT STATE, then
> `docs/stage2-finish-plan.md` (THE authority — single WS numbering),
> then for background the §12 status section of
> `docs/capture-remediation-plan-2026-08-28.md` and
> `docs/venue-instrument-support-gaps.md` (WS2–WS11's source
> inventory). Standing rulings: ALL CODING FIRST (WS2 → WS12, in
> order unless I say otherwise), compile checkpoints allowed, NO test
> suites / live boots / launchctl until WS13 except the WS0
> operational steps which I run myself; commits only when I authorize
> them, explicit paths, `M-` style prefixes as in WS1, license-check
> before each, NO push, never read `.env`, Mac-only cargo/pytest,
> one-engine + serialized-worker laws, frozen surfaces untouchable
> (`backtest.py`, `cli.py` 8-verb surface, the 202 pytest pins,
> PM/BN parser bytes). Stage-3 items (gaps-doc §7) stay out. Start
> with WS2 (kill the OKX/Deribit failure class): read the outage doc
> §5 for the exact sites, design the non-fatal-drop + establishment-
> timeout change, code it with tests, then continue WS3 onward. If
> context runs short: write resume state into `stage2-finish-plan.md`
> under the current WS heading, tell me, and stop.
