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

## WS2 — 🔧 Kill the OKX/Deribit failure class (was "Tier 3" items 1+2)

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

## WS3 — 🔧 Small venue-data fixes (gaps §1, parsed-but-dropped class)

- Deribit: emit `current_funding_1e9` into the capture event (parser
  already fills it; lib.rs:531).
- Deribit: use `settlement_period` to gate perp-vs-dated channel
  treatment (discovery.rs:489 parses it today, unused).
- Hyperliquid: parse the `premium` wire field into the asset-ctx
  event.

## WS4 — 🔧 Reference-data REST, existing five venues (gaps §1/§5)

- 24h quote volume: BN spot `/api/v3/ticker/24hr`, BN USDM
  `/fapi/v1/ticker/24hr`, OKX `tickers`, Deribit, HL.
- Open interest REST: BN `/fapi/v1/openInterest`, OKX. (Deribit/HL
  already carry OI on WS.)
- Tick/lot/contract size: BN (`BnSymbolRow` grows fields), HL meta.
- Placement law: static metadata → boot discovery (8e pattern, audit
  row); periodic series (24h vol, OI) → WORKER fetch lane (serialized,
  budgeted, stored beside candles keyed venue+descriptor) — the
  engine's hot path never does REST.

## WS5 — 🔧 Binance expansion (gaps §2.1)

- USDM `<sym>@markPrice` channel: mark, index, funding, next-funding
  → capture events (+ the WS10 carrier when it lands).
- Dated futures: parse `contractType`/`deliveryDate` in discovery,
  name the class in the universe grammar, dated-future BBO lane.
- Parser/proptest/fuzz updates per new frame shapes.

## WS6 — 🔧 Deribit expansion (gaps §2.3)

- Spot lane (`kind=spot` discovery page + subscribe).
- DVOL volatility index (WS channel + descriptor + capture series).
- Option COMBO INSTRUMENTS: discovery + BBO capture only (combo
  ORDERS stay Stage-3).

## WS7 — 🔧 OKX expansion (gaps §2.4)

- `tickers` channel (24h vol) or its REST twin, wired into the same
  reference-data placement law as WS4; OI fetch.

## WS8 — 🔧 Hyperliquid expansion (gaps §2.5)

- Tick/lot metadata via meta REST; 24h volume. (`activeSpotAssetCtx`
  stays deliberately unsubscribed unless the operator flips it.)

## WS9 — 🔧 Bybit, the sixth venue (gaps §1 — the biggest single item)

- `VenueId::Bybit = 6` + `from_u8`; `crates/ingress-bybit` v5 public
  WS (`tickers`, `publicTrade`, `orderbook` behind `--bybit-depth`);
  REST discovery (`instruments-info`, `tickers`); handwritten byte
  scanners + property tests + `bybit_ws_frame` / `bybit_instruments`
  fuzz targets; `bybit-ticks/-events.pmlr` + `bybit:` manifest
  prefix; `[bybit]` universe grammar; worker candle lane #6.
- Constants ripple (gaps §1 last block): `VENUE_LABELS` ×2 mirrors,
  `TRADEABLE_VENUES=6`, `MODEL_VENUE_LABELS`/`ModelParams` arrays +
  their hand-rendered JSON/stderr lines, audit-replay coverage
  matrix, `docs/wire-format.md` + `docs/migration.md`.
- No new external deps expected (same hyper/rustls/mio stack) — if
  one appears anyway: `make license-deps` + commit the regenerated
  notices with it.

## WS10 — 🔧 Engine plumbing (gaps §1) — DESIGN DOC FIRST, then code

The two Strategy-surface changes; both get a short design doc for
operator review BEFORE code (they touch `strategy-core`, rings, and
the wire format — the highest-blast-radius items in the whole list):

- Funding carrier ingress → `Strategy` (today funding is capture-only
  events; no engine ring type carries it).
- L2 depth to `Strategy` (today book channels are header-only
  capture; book-builder is top-of-book).
- Zero-alloc laws apply in full: preallocated lanes, `#[repr(C)]`
  Copy types, no new hot-path branches beyond the lane drain.

## WS11 — 🔧 Worker offline lanes + leftovers

- Funding-history backfill (per-venue REST, new table beside candles,
  §9 keying; "funding" currently has zero occurrences in the worker).
- D5 fold-ins: `SLOT_KIND_OPT_SUMMARY=6` decode + `_run_anchor_ns`
  mirror into `pmlr.py` (iv_digest's local reader retires).
- 🎛 D4 ruling: recommendation (i) — candles.db's 12 descriptors
  already = the full non-options universe; options ride iv_digest ⇒
  closes with ZERO code. Reading (ii) = REST OHLCV for ~192 option
  instruments (new budgeted lane). Default (i) unless overruled.
- M5 semi-manual runbook snippet wiring #7a helpers with the map's
  HIP-4 pairs (the daemon passes none by design).

## WS12 — 🔧 Mechanical hygiene, LAST before the run

`cargo fmt` (~88 files) + `clippy -D warnings` (~40 lints) ⇒
`make lint` green. Sequenced last so the churn never collides with
WS2–WS11 diffs. Own commits, no logic changes.

## WS13 — ▶ THE ONE LONG RUN (everything validated at once)

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
