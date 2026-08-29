# Remediation RUN PHASE — runbook + fresh-session handover

**Written 2026-08-28 by the coding session.** The capture-remediation
plan (`docs/capture-remediation-plan-2026-08-28.md`, §12) is
CODE-COMPLETE and UNTESTED: nothing was compiled, no gates were run, no
live boot was made. This doc is the exact, ordered run phase. A fresh
Claude session starts from the RESUME PROMPT at the bottom; the
operator can equally drive it by hand.

Authority chain: outage doc → remediation plan (§12 status table +
deltas) → this runbook. Laws in force: Mac-only cargo/pytest
(pitfall #10) · G0 relink · one-engine · serialized worker verbs +
pytest (`pgrep` first) · operator-authorized explicit-path commits ·
NO push · `.env` never read/printed.

---

## 0. FIRST DECISION (operator): #7b armed at next boot

`engine-wrapper.sh` now spawns `scripts/recommit-ruleset.sh` on EVERY
boot, and the registry holds the H6b prior — so the next engine boot
will re-stage + re-commit it through the frozen gate binding
(idempotent, paper, epoch bumps; drift/missing-report REFUSE). The
recommit module is untested until step 3.

- **Leave armed** (default): tonight's boot exercises #7b live;
  `~/multivenue/logs/launchd/recommit.log` carries the evidence.
- **Disarm until gates pass:** comment the
  `( zsh "${0:A:h}/recommit-ruleset.sh" … )` line in
  `scripts/engine-wrapper.sh`; re-enable after step 3.

Also note: the candles agent's NEXT hourly fire runs the new
`candles-cycle.sh` → candles + `iv_digest` (D3 live by automation;
harmless, serialized, skips pre-manifest runs).

## 1. Build FIRST, then revive (order matters)

The revival boot should run the T1 binary, so build before pulling the
revive lever. On the Mac:

```sh
cd ~/trading-engine-multivenue
cargo build --release --workspace        # includes the -p cli relink (G0)
```

If impossible-looking unresolved-import errors appear (stale rmeta):
`cargo clean -p core-metrics -p ingress-okx -p ingress-deribit -p cli`
and rebuild. RustRover terminal is ~45 s — use
`nohup cargo build --release --workspace > /tmp/rem-build.log 2>&1 &`
and poll.

## 2. R0 — revive the restart lane (operator terminal, ~5 min)

State at 2026-08-28 20:00Z: job dead (`last exit code = 78: EX_CONFIG`,
penalty box), stamp `20260827`, engine pid 16002 (Aug-27 boot),
PM/OKX/Deribit dark.

```sh
# 1. kick the JOB (never touch com.multivenue.engine)
launchctl kickstart -k gui/$(id -u)/com.multivenue.daily-restart
# 2. if exit 78 recurs within ~2 min, fully reload the job:
launchctl bootout   gui/$(id -u)/com.multivenue.daily-restart
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.multivenue.daily-restart.plist
# 3. first run of the NEW script SEEDS the four slot stamps (no drain).
#    Verify: ls ~/multivenue/state/   → last-restart-utc-{0000,0020,0830,1605}
# 4. revive the three dark venues NOW (sanctioned lever — one drain):
echo 19700101 > ~/multivenue/state/last-restart-utc-0000
```

Within ~60 s: drain line in `restart.log` → wrapper reboot → verify:
new `run-<ns>` dir; boot log shows PM 4/4 + two
`discovery: options chain … selected=32` groups; `launchctl print`
shows `last exit code = 0` and no penalty box; retention will archive
(free disk was 21 GiB < 25 GiB floor — expected). If #7b is armed,
`recommit.log` shows the restage+recommit (or a named refusal).
Tonight's 00:00Z then fires on its own (stamp date < tomorrow).

`/metrics` (127.0.0.1:9191) on the NEW binary now carries:
`engine_ingress_<venue>_last_tick_age_seconds` (−1 until first tick),
`engine_restart_stamp_age_seconds`, `engine_ingress_<venue>_ticks_total`,
and the okx/deribit `run-loop returned` lines carry
`err_site`/`io_kind`/`venue_code` (the chronic OKX churn should
self-name within the hour — outage §5.4/§6).

## 3. Gates (Mac; serialized; long-runners via nohup+poll)

```sh
# Rust — expect ≥1247 (was 1240; +3 core-metrics, +1 okx, +1 deribit, +2 cli)
cargo nextest run --workspace

# alloc gate — MUST stay 38/38 0 B/op; fresh `Compiling bench` guard
cargo test -p bench --test alloc_assertions --release -- --test-threads=1

# worker — pgrep -f claude-worker AND pgrep -f pytest first; off the
# top of the hour (candles agent). Expect ≈454 (was 439; +6 recommit,
# +9 inventory; frozen 202 untouched).
cd claude-worker && uv run pytest

# licence gate (new files carry SPDX; no new deps)
cd .. && make license-check
```

Known possible fallout, in likelihood order — fix additively, never
touch `backtest.py`/`cli.py`/the frozen 202:

1. **Daemon digest tests** that pin exact digest text: `_after_fetch`
   now appends POSITIONS + SHADOW-P&L sections (honest-empty in test
   envs). If an existing daemon/strategist test asserts full-digest
   equality, extend its expectation for the two sections (additive
   drift, sanctioned by ruling #7(a)).
2. **`fake_uds.frames` count assumptions** in test_recommit.py (3 =
   heartbeat+stage+commit; 2 on drift) — if the conftest fake counts
   heartbeats differently, reconcile against `FakeUdsServer` behavior,
   not by weakening the no-commit-after-drift assertion.
3. **`ErrorKind::Other` deprecation lint** in the core-metrics test —
   if clippy/deny complains, swap the test's `Other` for
   `ErrorKind::AddrInUse` (any unmapped kind).
4. Anything else: fail-fast, read the error, smallest fix, re-run.

## 4. Live verification (needs one settlement cycle)

- Next **08:00Z**: OKX/Deribit die — now with NAMED errors
  (`err_site=venue-error venue_code=…` / `subscribe-missing`); the
  **0830 slot** revives both; `last_tick_age` for both returns to ~0.
  §6's three unknowns close themselves from these lines — record the
  observed codes in the outage doc as an addendum.
- Next **16:05Z**: slot fires; **§5.6 tell: PM ticks flow after the
  16:05Z boot** (PM `last_tick_age` drops). If not, the refresher is
  selecting the expiring market — fix `claude_worker.universe_refresh`
  selection before trusting the slot.
- Next full UTC day: catalog shows 3–4 runs, `gap_free=true`,
  per-venue coverage ≈23.5–24 h (was 8/8/16 for okx/deribit/pm),
  `opt_summaries` present in all three windows.
- Backoff: rejection-spam cycles now ESCALATE (reconnect cadence for a
  dead lane stretches to the cap instead of ~1 Hz) — visible in the
  err.log line spacing.

## 5. C6 close + D1 + commits (operator-authorized window)

1. **C6**: procedure in `docs/m3-progress.md` (evidence already met:
   trailing streak 5 ending 2026-08-27 ≥ 3; step-2 archival already
   done by Aug-27 retention; step 3 = frozen-argv real-capture
   backtest with the H6a prior from `~/multivenue/worker/demo-h6a/demo/`,
   PASS = `oos.trading_days >= 2`). Close entry in m3-progress +
   CLAUDE.md CURRENT STATE refresh (M3 CLOSED; F0 incident; this
   remediation; new stay-green numbers from step 3).
2. **D1**: source `.env` (H6b wrapper pattern), `pgrep` guard, run
   `claude-worker fetch` once after a full-coverage boot; done-tell
   `unresolved=0`; prune stale PM map entries the conflict report
   names.
3. **D4 ruling**: recommendation (i) — candles.db's 12 descriptors
   already = the full non-options universe; IV surface via iv_digest
   covers options ⇒ F6 closes with zero code. Reading (ii) (REST
   OHLCV for 192 option instruments) = new fetch-lane scope, operator
   call.
4. **Commits** (suggested split; operator authorizes, explicit paths,
   `git add <paths>` never `-A`):
   - `outage-T1: name session errors; ticks-based backoff; dead-lane gauges`
     → `crates/core-metrics/src/ingress_status.rs crates/core-metrics/src/lib.rs
     crates/ingress-polymarket/src/run_loop.rs crates/ingress-binance/src/run_loop.rs
     crates/ingress-okx/src/run_loop.rs crates/ingress-deribit/src/run_loop.rs
     crates/ingress-hyperliquid/src/run_loop.rs crates/ingress-rpc/src/run_loop.rs
     crates/cli/src/paper.rs`
   - `outage-T2: UTC slot restarts (0000/0830/1605) + 0020 nightly pnl slot`
     → `scripts/daily-restart.sh`
   - `M3-D3: iv_digest rides the hourly candles cycle`
     → `scripts/candles-cycle.sh`
   - `M5P-7b: post-boot ruleset re-commit (module + wrapper child)`
     → `scripts/engine-wrapper.sh scripts/recommit-ruleset.sh
     claude-worker/src/claude_worker/recommit.py claude-worker/tests/test_recommit.py`
   - `M5P-7a: digest POSITIONS + per-strategy shadow-P&L sections`
     → `claude-worker/src/claude_worker/strategist.py
     claude-worker/src/claude_worker/daemon.py
     claude-worker/tests/test_strategist_inventory.py`
   - `docs: outage finding + remediation plan + run-phase handover`
     → `docs/capture-continuity-outage-2026-08-27.md
     docs/capture-remediation-plan-2026-08-28.md
     docs/prompts/remediation-run-phase.md`
   - the C6 close commit per its own procedure (`M3:` prefix).
   Run `make license-check` before each commit.

## 6. Re-baseline → M6

**Post-fix day 0** = first full UTC day with T2 slots + T1 binary live
and verified (§4 above all green). The 7-day M6 soak may start then —
never on pre-fix capture (outage §8.4). M5 proper (design entry first)
remains on explicit operator go; #7a/#7b landing here removes its two
gate blockers F9/F10.

## Still open / deferred (unchanged from the plan)

D4 ruling (above) · D5 optional pmlr.py fold-ins · Tier 3 (in-process
resilience; needs manifest epochs first) · fmt/clippy backlog (own
commits) · BN eapi WS lever (≤ M6) · daemon HIP-4 pairs plumbing (M5
design) · BN multi-lane per-slot backoff review (Tier-3 adjacent).

---

## RESUME PROMPT (paste into a fresh session)

> Read, in order: `CLAUDE.md` (CURRENT STATE), the last entry of
> `docs/capture-remediation-plan-2026-08-28.md` (§12 implementation
> status + deltas), and `docs/prompts/remediation-run-phase.md` (this
> file) in the `trading-engine-multivenue` repo. The remediation
> coding phase is COMPLETE and UNTESTED; the working tree carries the
> uncommitted changes inventoried in plan §12. Execute the RUN PHASE
> of `remediation-run-phase.md` from step 0, in order: confirm the
> operator's #7b arm/disarm choice, build release on the Mac, run R0
> with the operator, then gates (nextest ≥1247 / alloc 38 / pytest
> ≈454 / license-check), fix fallout additively (frozen surfaces
> untouchable: `backtest.py`, `cli.py` verb surface, the 202), then
> the live verification, C6 close, D1, and the commit map — commits
> and any launchctl/git action only on explicit operator go. Mac-only
> cargo/pytest; serialized worker invocations; no push; never read
> `.env`.

## Session facts worth carrying (this session's finds)

- launchd `StartInterval` jobs can sit in the **penalty box** with
  `last exit code = 78: EX_CONFIG` long after the underlying cause
  (exec-bit strip + inode swap) is repaired — `kickstart -k` or
  bootout/bootstrap of the JOB is the fix; the engine job is separate.
- `engine-wrapper.sh` line 21 uses `exit 78` for a failed `cd` — an
  unrelated, coincidental use of launchd's EX_CONFIG code. Don't
  conflate when debugging.
- `engine.out.log` is ANSI-colourised WITH escapes inside key=value
  pairs — strip with `sed $'s/\x1b\\[[0-9;]*m//g'` before any grep.
- The catalog's `capture_end` equals "now" for the live run — a
  same-second read is not a stall.
- The Mac's UTC day can lag the operator's local date — always
  `date -u` before slot arithmetic.
