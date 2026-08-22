# M3 — Continuous data ops: progress log

Phase authority: `docs/mvp-completion-plan.md` §4-M3 + §9 (BINDING
VERBATIM) → this log's latest entry → CLAUDE.md. Operator go recorded
2026-08-22; M2 runs in a PARALLEL session — the CLAUDE.md "Parallel
M2/M3 session protocol" is LAW for every entry below. Commits are
operator-authorized, `M3:` prefix, explicit paths only.

---

## 2026-08-22 — Session 1: Session-0 baseline + C1 capture-catalog BUILT (gates green, live-smoked; commit ask pending)

### Session 0

- RustRover MCP attached (get_project_modules) against the main
  checkout first, per law.
- `git status` at session start: `docs/mvp-progress.md` dirty (M2
  lane's — untouched by M3), branch `main` ahead 11 of origin (the
  KNOWN push anomaly — record, never act). No engine, no worker
  running. Disk 51 Gi free (89 % used — retention (C3) will matter).
- Baseline gates re-verified on the Mac: workspace nextest
  **1139/1139** (+1 skipped fixture-regen), alloc **36/36** with a
  fresh `Compiling bench` in the log (false-green guard), worker
  pytest **363** (361 + the 2 `test_backtest_real` with the release
  binary on PATH — release cli relinked first). Matches the M1-close
  baseline exactly.

### C1 — capture-catalog (mvp-plan §4-M3 item 2)

**New offline subcommand `multivenue-engine capture-catalog --dir
<root|run-dir> [--gap-tolerance-ns N]`** (audit-replay doctrine —
allocates freely, doctrine header in the module). JSON
(`catalog_version` 1, hand-rendered one-liner) on stdout,
deterministic human summary on stderr; stderr-pinned tracing like the
backtest arm so stdout stays pure. An EMPTY root is a VALID zero-run
report (init-if-empty visibility, mvp-plan §4-M3 item 4).

What it reports, and whose law each piece surfaces:

- **Per-run**: wall span under the harness's §3.3 anchor
  (`wall = epoch_ns + (ts − run_first_ts)`, run-level min-first
  anchor exactly like `backtest::load_and_merge`), duration under the
  monitor's `RunSpan` law (`max(last_ts) − min(first_ts)` across tick
  files), per-venue tick counts/bytes/first/last, whole-dir byte
  sizes, `other_files` aggregate (events/signals/fills/ai-cmds/raw
  tap — and any FUTURE channel, e.g. M2.3 mark/IV, is size-visible
  with ZERO catalog change; a dedicated coverage row is the
  designated extension point).
- **Harness view** (`backtest::load_run` §3.1 acceptance, mirrored
  per file): dir-name parse, PMLR v2, `SlotKind::Tick`, header/dir
  epoch cross-check — deterministic per-file rejection notes
  (`unreadable-header`, `pmlr-vN-not-v2`, `slot-kind-not-tick`,
  `header-epoch-mismatch`); wall-overlap detection (the §3.3
  condition the harness refuses roots over);
  `whole_root_backtestable` = all runs clean ∧ no overlaps ∧ ≥1 tick.
  `capture_utc_days` is the harness's §4.5 arithmetic verbatim
  (distinct `wall_ns / 86_400_000_000_000` over every tick), and the
  days-gate line reports the `min_trading_days = 2` NECESSARY
  condition (the gate itself counts OOS fill days — stated, never
  overclaimed).
- **Monitor view** (`monitor.py` §8.3 arithmetic, constants mirrored
  with a divergence-is-a-doc-bug note): trailing 24 h anchored at the
  capture's own end, run-granular selection, duration-0 runs never
  selected, in-window coverage vs the 6 h floor → would RUN/SKIP.
- **Continuity** (the M3 exit tell): per-UTC-day covered/dark ns +
  per-venue per-day tick counts, inter-run dark-gap map, gap-free
  verdict (`dark ≤ tolerance ∧ ticks > 0`; default tolerance 300 s —
  sized for the daily-restart drain), longest + trailing consecutive
  gap-free-day streaks. The `N≥3 CONSECUTIVE gap-free days` exit
  reads directly off `continuity.trailing_streak`.

Reuse over duplication: `backtest.rs` lends `parse_run_dir_name`,
`VENUE_LABELS`, `REQUIRED_PMLR_VERSION` (now `pub(crate)`, doc'd) —
one name law, one acceptance law, no drift. Catalog discovery differs
from `discover_runs` ONLY in the empty-root semantics (valid report,
not error), stated in the module docs.

### Files touched (all M3-owned or SHARED-additive per protocol)

- `crates/cli/src/capture_catalog.rs` — NEW (module + unit tests).
- `crates/cli/tests/capture_catalog.rs` — NEW (6 fixture-driven
  integration tests via the real `PmlrWriter`: 3-full-days gap-free
  streak + monitor-RUN, byte-identical rerun determinism, partial-day
  gapped + monitor-SKIP + days-gate-insufficient, per-file harness
  rejection notes, single-run-dir resolution, overlap refusal).
- `crates/cli/src/lib.rs` — SHARED, additive: `pub mod
  capture_catalog;` (one line).
- `crates/cli/src/backtest.rs` — SHARED, additive: 3 items to
  `pub(crate)` + doc lines; zero behavior change.
- `crates/cli/src/bin/multivenue-engine.rs` — SHARED, additive: new
  `CaptureCatalog` variant/args/arm (stderr tracing, backtest-arm
  pattern).
- `docs/m3-progress.md` — NEW (this log).

### Gates (all on the Mac)

- workspace nextest **1151/1151** (+12 = 6 module unit + 6
  integration; 1 skipped fixture-regen unchanged).
- alloc **36/36** 0 B/op, fresh `Compiling bench` confirmed
  (`--test-threads=1`, corrected clean-guard).
- worker pytest **363** (Python untouched; re-run green with release
  binary on PATH).
- fuzz: untouched — no new untrusted-bytes parser (PMLR reading rides
  the existing `core-io` reader; catalog parses no venue wire bytes).

### Live smoke (pitfall #11 — real capture, real binary)

`target/release/multivenue-engine capture-catalog --dir
~/multivenue/logs` (fresh release relink): exit 0; **14 runs,
3,075,482 ticks, 257.4 MiB, 4 UTC days with ticks**. Cross-validation
against history: run[4] reports okx 365,779 · deribit 281,579 · hl
221,030 — the EXACT G1-soak run[2] numbers in mvp-plan §2. Two
header-only aborted-boot dirs correctly harness=REJECT ⇒ whole-root
replay REFUSED (true: 8h-era backtests always targeted subdir
captures); monitor view coverage 30m41s < 6 h floor ⇒ SKIP (true:
only short windows ran today); continuity 0 gap-free days — the
honest statement of the exact problem C2 (always-on lane) exists to
fix.

### Next

1. **Commit ask C1** (pending operator go): the 6 paths above,
   `M3:` prefix, explicit paths. On landing: the "CATALOG LANDED —
   M2.3 UNBLOCKED" entry line + operator notification (verbatim
   duty).
2. C2 launchd always-on lane (install EARLY — exit gate is calendar
   time): wrapper sourcing `.env`, KeepAlive plist, caffeinate/power
   runbook, daily SIGTERM restart + Gamma universe.toml refresh
   script.
3. C3 retention, C4/C5 candles.db per §9.4–§9.7.

**Resume point if context dies here:** C1 code complete + gates green
+ smoked; nothing committed; ask the operator to authorize the C1
commit of exactly the six paths listed above, then follow the
notification duty.

---

## 2026-08-22 — C1 LANDED

CATALOG LANDED — M2.3 UNBLOCKED (commit cf132ae)

Operator authorized; committed `M3:`-prefixed with the six explicit
paths only (`git status` after: only M2's `docs/mvp-progress.md`
remains dirty — untouched). Operator notified verbatim per the
kickoff duty. Next: C2 — launchd always-on lane (installed EARLY so
gap-free days start accumulating).

---

## 2026-08-22 — C2: launchd always-on lane BUILT + INSTALLED + LIVE-PROVEN (commit ask pending)

**The standing engine is UP** (installed 12:08:21Z; the M3 one-engine
law is now in force — this launchd instance IS the engine; M2 smoke
windows stop/start it per the new runbook section).

### Pieces

- **`scripts/engine-wrapper.sh`** (launchd target, every boot):
  one-engine pgrep guard (foreign engine ⇒ backoff-and-retry — the
  lane self-heals around manual smoke windows) → source `.env`
  (values never inlined in plists, never echoed; the H6b wrapper
  pattern) → best-effort `claude_worker.universe_refresh` → `exec
  release-binary run --paper --strategy all`. G0 relink law stated
  in-file: the wrapper NEVER builds.
- **`scripts/daily-restart.sh`** + 60 s `StartInterval` agent: on
  UTC-day change (stamp file), SIGTERM drain (M1d-proven) →
  KeepAlive relaunch → fresh universe + fresh run dir ⇒ **one run
  dir per UTC day, gap-free by construction**. `StartInterval`
  deliberately, not `StartCalendarInterval` (launchd calendars are
  LOCAL-time/DST; slept-through midnights fire on wake).
- **`launchd/com.multivenue.{engine,daily-restart,caffeinate}.plist`**
  templates (`@REPO@`/`@HOME@`) + **`scripts/install-launchd.sh`**
  (idempotent render+bootstrap; seeds the restart stamp so install
  never self-kills; seeds `~/multivenue/pm-dailies.toml` from the
  example). `caffeinate -s -i` holds sleep off on AC; pmset/clamshell
  operator notes in the runbook.
- **`claude_worker/universe_refresh.py`** — a MODULE, not a verb
  (7-verb surface untouched; `cli.py` stays byte-frozen): resolves
  each configured underlying's nearest unresolved up/down daily via
  the Gamma lane and rewrites ONLY the `[polymarket]` `markets` array
  of `universe.toml`, atomically, byte-preserving everything else
  ([pairs] untouched — `pm-dailies.toml` order law documented).
  **Date law** (live-verified): dailies resolve 16:00Z and list ~2
  days early ⇒ target today before 16:00Z, else tomorrow — the
  00:00Z restart always picks the day's markets. **Slug law**
  live-verified (`bitcoin-up-or-down-on-august-22-2026`); unpadded
  day, with a padded fallback until a single-digit day confirms.
  Best-effort law: ANY failure leaves the file byte-untouched, exit
  1, wrapper boots on the existing universe.
- Tests: `tests/test_universe_refresh.py`, 18 additive (mocked
  `get_fn`, real-shape Gamma fixtures incl. the double-encoded
  `clobTokenIds`/token-id length law; Down/Up swap; rewrite
  byte-preservation; end-to-end idempotence; fail-soft paths).
- Runbook: `docs/local-setup.md` new "Always-on standing engine"
  section (install/status/logs, M2 smoke-window dance, relink law —
  `pkill -TERM`, never `kickstart -k` (SIGKILL), uninstall, power).

### Live proofs (2026-08-22, this session)

1. **Install** → engine pid 73763 via the wrapper, fresh
   `run-1787400501923792000`, metrics 9191 up, caffeinate + restart
   poller loaded.
2. **First boot exposed a REAL bug**: the refresh module's stdlib
   `urllib` GET failed CA verification under uv-managed CPython
   (launchd env) while manual `curl` succeeded — root-caused and
   FIXED to the worker-standard httpx GET (`cli._http_get` pattern,
   `REST_TIMEOUT_S`); the wrapper's best-effort law held (booted on
   the existing file — availability never depended on the refresh).
3. **Refresh proven against ground truth**: live module run resolved
   2 markets for 2026-08-22 whose token ids are BYTE-IDENTICAL to the
   operator's hand-resolved M1 entries (diff shows comment lines
   only).
4. **Full-chain restart**: `pkill -TERM` → KeepAlive relaunch raced
   the drain → one-engine guard backed off (by design) → retry →
   **refresh SUCCESS through the wrapper** → engine pid 74176, fresh
   `run-1787400655038242000`, ticks flowing. Dark window ≈ 60–90 s ≪
   the 300 s gap tolerance.
5. First automated UTC-midnight turn happens tonight; from tomorrow
   the gap-free-day counter starts (catalog is the judge).

### Gates

- Rust byte-untouched since C1 ⇒ nextest 1151/1151 + alloc 36/36
  stand (C1 numbers, same tree).
- worker pytest **381** (363 stay-green + 18 additive), release
  binary on PATH (the 2 real-harness tests included).
- fuzz: untouched — no new untrusted-bytes parser (Gamma JSON rides
  the EXISTING `parse_gamma_markets`; the new module adds no parser).

### Commit ask C2 (pending operator go) — explicit paths

`scripts/engine-wrapper.sh` · `scripts/daily-restart.sh` ·
`scripts/install-launchd.sh` · `launchd/com.multivenue.engine.plist`
· `launchd/com.multivenue.daily-restart.plist` ·
`launchd/com.multivenue.caffeinate.plist` · `pm-dailies.toml.example`
· `claude-worker/src/claude_worker/universe_refresh.py` ·
`claude-worker/tests/test_universe_refresh.py` ·
`docs/local-setup.md` · `docs/m3-progress.md`

**Resume point if context dies here:** C2 installed + live-proven,
commit not yet asked/landed; C3 (retention) + C4/C5 (candles.db) not
started; the standing engine accumulates days meanwhile.

**C2 LANDED** — commit `2a89238` (operator-authorized, 11 explicit
paths; post-commit tree carries only the M2 lane's files).

---

## 2026-08-22 — C3: retention BUILT + smoke-proven (commit ask pending)

`scripts/retention.sh` + `retention.conf.example` +
`docs/local-setup.md` paragraph + a one-line daily-restart hook (runs
once per UTC day inside the day-flip branch).

Policy exactly as ruled (mvp-plan §4-M3 item 3 / decision-log open
question resolved toward the default): **KEEP-ALL until disk
pressure** — free < `MIN_FREE_GIB` (25) triggers compressing the
OLDEST run dirs into `~/multivenue/archive/` until `TARGET_FREE_GIB`
(40); the newest (live) run dir and anything ≤ `PROTECT_DAYS` (7) are
never touched; originals removed only after a verified archive;
archives never auto-deleted (operator prunes; catalog reports sizes).
`--root`/`--conf` flags for testing; config file sourced over
defaults.

**Live smoke found a real environment fact** (pitfall #11): macOS
bsdtar `--zstd` shells out to an uninstalled external `zstd` — the
fail-soft path held (original kept, partial archive removed, stop);
switched to bsdtar-INTERNAL gzip (`tar -cz`, zero new deps, house
doctrine), re-proven: 20d/10d/8d fixture dirs compressed + removed,
newest kept, protect law honored, archive list+extract round-trip
clean, keep-all no-op on the real root (free 50 GiB ≥ 25). Gates
untouched (shell + docs only; pytest 381 / nextest 1151 / alloc 36
stand).

Commit ask C3 paths: `scripts/retention.sh` ·
`retention.conf.example` · `scripts/daily-restart.sh` ·
`docs/local-setup.md` · `docs/m3-progress.md`.

**C3 LANDED** — commit `1d712e8`.

---

## 2026-08-22 — C4: candles.db BUILT + INSTALLED + LIVE-PROVEN (commit ask pending)

**`claude_worker/candles.py`** — a MODULE (`python -m
claude_worker.candles`), never a verb; `cli.py` stays byte-frozen.
§9.4–§9.6 implemented VERBATIM:

- **Store** (§9.4): SQLite WAL `~/multivenue/worker/candles.db`
  (`CLAUDE_WORKER_CANDLES_DB`), ONE `candles` table, PK
  `(venue, descriptor, tf, open_ts)` WITHOUT ROWID; descriptors in
  the worker map-name convention (`binance:btcusdt`,
  `binance-usdm:btcusdt`, `okx:BTC-USDT`, `deribit:BTC-PERPETUAL`,
  `hyperliquid:BTC`) — never bare SymbolId; `o,h,l,c,v`, `source` ∈
  rest|derived|capture, `fetched_ts`; `candle_conflicts` beside it
  (market-map conflict pattern, `first_seen_ts` preserved).
- **Timeframes** (§9.5): bases 1m (48 h) / 1h (90 d) / 1d (listing
  lifetime) fetched ONLY; 5m/15m/4h never fetched (C5 derives).
  Venue-cheapness adaptations, all live-probed: OKX 1d bounded 400 d
  (backward 100-row pages make lifetime expensive — the §9.5 "where
  cheap" carve-out); Deribit/HL 1d pre-launch floors (2016-01-01 /
  2020-01-01) because epoch-0 forward windows crawl the 1970s
  (Deribit no_data abort) and HL rejects startTime=0 — both venues
  clamp a pre-listing floor to their earliest bar = true lifetime.
- **Gap-fill** (§9.6): per (descriptor, tf) `max(open_ts)` frontier →
  request ONLY the missing window (frontier bar re-requested — the
  open-bar finalization lane) → paginate under a per-VENUE
  `RestBudget` (30/h default, deliberately under the fetch verb's
  60) → upsert. Bounded backfill on empty store; budget exhaustion
  resumes next cycle by construction. **Closed-stored law**: a row is
  immutable only if it had CLOSED when fetched
  (`open_ts + tf_ms <= fetched_ts`) — mid-life snapshots are
  finalized by later fetches; disagreement with a closed-stored bar
  is logged to `candle_conflicts`, never overwritten. A rest bar
  supersedes a capture bar on the same PK (capture fills only where
  no rest lane exists, §9.7).
- **Hole-avoidance**: forward lanes (BN spot `/api/v3/klines`, USDM
  `/fapi/v1/klines` — NEW strict parser `parse_binance_klines`,
  labeling.py discipline; Deribit chart; HL candleSnapshot) upsert
  page-by-page, monotone frontier. OKX pages BACKWARD
  (`history-candles` + `after`): the walk is BUFFERED and upserted
  only when it connects to the frontier — a budget-truncated walk is
  discarded whole (else `max(open_ts)` would leap a permanent hole).
- Glue: `scripts/candles-cycle.sh` (.env sourced, self-overlap
  guard) + `com.multivenue.candles` hourly agent (installed;
  install-launchd.sh covers it for fresh installs), `.env.example`
  M3 block (BN REST hosts — the only new host keys; db path, budget,
  backfill horizons), runbook paragraph.

**Live proofs (real venues, two cycles):** cycle 1 = 42,254 rows —
BN spot/usdm full §9.6 backfills incl. TRUE lifetime 1d (btcusdt
3,293 daily bars back to the 2017 listing; usdm 2,541 to 2019); OKX
1m backward walk 29 pages/2,881 bars then BUDGET — and the remaining
OKX lanes correctly deferred (§9.6 resume, observed); Deribit/HL
1m+1h clean. Cycle 1 exposed the two 1d edge cases above; floors
fixed; cycle 2 = **52,148 rows, conflict-rows 0**: Deribit 1d 2,931
bars (full perpetual lifetime), HL 1d 2,195 × 2, OKX 1d 400
(carve-out), BN 1d `pages=1 bars=1 open~1` — the frontier resume
re-fetching ONLY the open daily bar and finalizing it. Zero
conflicts across 52k rows.

**Gates:** worker pytest **401** expected (381 + 20 additive — full
suite re-run at commit); Rust untouched since C1 (nextest 1151 /
alloc 36 stand); fuzz untouched (parse_binance_klines is worker-side
Python under the labeling.py strict-parse discipline with
table-driven tests — not a Rust ingress parser; §21.3/§21.4 does not
attach).

Commit ask C4 paths:
`claude-worker/src/claude_worker/candles.py` ·
`claude-worker/tests/test_candles.py` · `scripts/candles-cycle.sh` ·
`launchd/com.multivenue.candles.plist` · `scripts/install-launchd.sh`
· `.env.example` · `docs/local-setup.md` · `docs/m3-progress.md`.

**C4 LANDED** — commit `e6b4de2`.

---

## 2026-08-22 — C5: derive + capture-derived + drift check BUILT + LIVE-PROVEN (commit ask pending)

Extends `claude_worker/candles.py` (still a module, `cli.py` frozen):

- **§9.5 derive**: 5m/15m from 1m, 4h from 1h — EXACT law (O=first,
  H=max, L=min, C=last, V=sum), `source=derived`, cached; only
  COMPLETE CLOSED windows (all k base bars present); derived rows
  refresh when a base finalization changes them (they are cache —
  immutability protects fetched rest bars only); NULL-volume
  poisoning (any NULL base ⇒ NULL — volume never fabricated).
- **§9.7 capture lane**: fold PMLR ticks (harness wall law,
  `epoch + (ts − run_anchor)`; one-sided ticks skipped) → per-minute
  mid-price OHLC + tick-count → stored for PM (`source=capture`,
  `v` NULL, count in the new nullable **`n` column** — the §9.7
  "tick-count" landing spot; §9.4's column list gains exactly this
  one nullable column, C4→C5 `ALTER` migration in `open_db`;
  OPERATOR NOTE: flagging since §9 is binding — say the word and it
  moves to a side table instead). Rest supersedes capture on a
  shared PK, never the reverse. Rolling 26 h window
  (`CLAUDE_WORKER_CANDLES_CAPTURE_WINDOW_H`);
  `--capture-backfill` = one-shot full-history fold.
- **§9.7 drift check**: capture-vs-REST close comparison for
  `binance:*` descriptors over a 6 h window, report-only, WARN over
  20 bps (env-tunable).
- **Rotation fix** (LIVE-observed convergence flaw): OKX
  ETH-USDT-SWAP's 29-page backward 1m walk was discarded EVERY cycle
  behind BTC's 3-page head start (27 < 29 remaining budget,
  forever). Targets now rotate by cycle hour — every target leads
  eventually, every backfill completes.

**Live proof (cycle 3, real venues + real capture):** incremental
§9.6 visible everywhere (`pages=1 bars=10 +9 open~1` per lane — ten
minutes since cycle 2, frontier + open-bar finalization);
**capture pm: minutes=166 +166** (today's real PM capture stored,
volume NULL); **drift binance:btcusdt mean 1.09 bps max 5.67 /
ethusdt 3.44/10.47 — our sockets and Binance's own candles agree to
basis points** (BBO-mid vs trade-close structural difference
included), no WARN; **derive +4,062/+1,348/+3,776** (5m/15m/4h).
rows=61,570, conflict-rows=0.

**Gates:** worker pytest **412** (401 + 11: 10 C5 + 1 rotation);
Rust untouched (nextest 1151 / alloc 36 stand); fuzz untouched (no
new parser — PMLR rides `claude_worker.pmlr`, map rides
`cli.load_market_map`). `tests/craft.py` gained the additive
`write_ticks_px` builder (explicit per-slot prices).

Commit ask C5 paths:
`claude-worker/src/claude_worker/candles.py` ·
`claude-worker/tests/test_candles.py` · `claude-worker/tests/craft.py`
· `.env.example` · `docs/local-setup.md` · `docs/m3-progress.md`.
