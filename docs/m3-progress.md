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
