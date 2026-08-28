# Capture-continuity remediation & M5-gate readiness plan — 2026-08-28

**Status:** DRAFT for operator review · nothing in this plan has been executed ·
no git operation was performed (this file and the outage doc stage independently
when the operator authorizes a commit).

**Inputs:** `docs/capture-continuity-outage-2026-08-27.md` (Defects A/B/C, §7
tiers, §8 sequence — this plan implements its recommendations), the C6 closing
procedure in `docs/m3-progress.md`, `docs/mvp-completion-plan.md` §4-M5 + §8
rulings #5–#7, CLAUDE.md laws, and live diagnostics run 2026-08-28 ~19:41Z
(recorded in §R0.1 below).

**One-line summary:** revive the dead restart lane tonight → close C6 →
Tier-2 restarts (config-only coverage fix) → Tier-1 diagnosability → C6+
data-plane catch-up → M5-prep items on operator go → re-baseline before M6.

---

## 0. Findings inventory

| id | finding | source | fixed in |
|---|---|---|---|
| F0 | **`com.multivenue.daily-restart` dead**: last exit 78 EX_CONFIG, launchd penalty box, stamp `20260827`; the Aug-28 00:00Z turn never fired; failure window opens at the 2026-08-27 15:26Z licence-pass script rewrite (documented exec-bit strip) | live check 2026-08-28 | R0 |
| F1 | Defect A: OKX/Deribit sessions die permanently at the 08:00Z options settlement (frozen boot chain A-1 · fatal-on-reconnect A-2 · backoff disarmed + establishment wedge A-3 · chronic OKX churn A-4) | outage §5.1–5.4 | T2 (coverage), T1 (diagnosis), Tier 3 deferred |
| F2 | Defect B: PM dark from daily resolution (~16:14Z) until the next restart — currently unbounded because F0 removed the only re-arm (PM dark since Aug-27 ~22:32Z) | outage §5.6 + live | R0 (tonight), T2 (daily) |
| F3 | Defect C: every `RunResult::Error` discards its cause; no per-venue `last_tick_age`; six days of outage produced zero signals | outage §5.5 | T1 |
| F4 | C6/M3 close pending though arithmetic is met (trailing streak **5** gap-free days ending 2026-08-27 ≥ N=3; 30/30 harness-ok; `whole_root_backtestable=true`) | live catalog | C6 |
| F5 | `market-map.json` stale (2026-08-22; zero options descriptors; no fetch since) | outage App. A | D1 |
| F6 | `candles.db` breadth: 12 descriptors vs 204-instrument universe | outage App. A | D4 (operator decision) |
| F7 | `iv_digest` stale/partial; cadence never wired (ruled a C6+ item) | outage App. A | D3 |
| F8 | `pnl` nightly timer still deferred (only `pnl-2026-08-23.*` exists; M4 D2) | outage App. A | D2 |
| F9 | Ruling #7a: digest POSITIONS + PER-STRATEGY P&L sections unimplemented | outage App. A | M5P |
| F10 | Ruling #7b: post-restart ruleset re-commit unimplemented — live `engine_vm_fires_total 0`; every restart silently un-commits the AI ruleset | outage App. A | M5P |
| F11 | Disk: 21 GiB free < 25 GiB retention floor — next successful turn archives more runs | live check | watch item (R0 note) |
| F12 | Meta: the restart lane failed silently for >28 h — nothing monitors the monitor | this session | T1(c) |

**Explicitly out of scope:** Tier 3 (§9), the pre-existing `cargo fmt`/`clippy`
backlog (own commits per CLAUDE.md), the Binance eapi WS activation lever
(operator-ruled: revisit at/before the M6 soak), anything Stage-3.

---

## 1. Binding constraints (inherited, not new)

- **One engine ever**; the standing launchd instance is THE engine. R0 works
  through launchd runtime state only; no file edits, no git.
- **Ownership:** T2/D phases edit M3-owned paths ⇒ they run **after** the C6
  close opens the M3 C6+ window. T1 edits M2-owned run-loops + shared
  `paper.rs` ⇒ **explicit operator-authorized window** (M2 is CLOSED).
- **Commits are operator-authorized**, explicit paths only, lane prefixes;
  **no push** (pushes are the operator's, manual).
- **Stay-greens:** nextest ≥1240 · alloc 38/38 0 B/op (`--test-threads=1`,
  fresh-`Compiling bench` guard) · pytest 439. Mac only (sandbox cargo =
  false greens). Worker verbs + pytest globally serialized (`pgrep` first).
- **G0 relink law:** `cargo build --release -p cli` is a deliberate act;
  a relink takes effect at the next (slot) restart.
- **Zero-alloc doctrine:** every T1 edit lives on the session-teardown /
  reconnect path, not the tick path. No new allocation in tick paths; the
  alloc gate must stay green.
- **SPDX + `make license-check`** on every new/edited `.sh`/`.rs`/`.py`;
  no new dependencies expected ⇒ no `make license-deps` run.
- **`.env` is sourced, never read/printed** (H6b wrapper pattern).
- **After any script rewrite: `git diff --summary` must show no mode
  changes** — the exact failure class that caused F0.

---

## R0 — TONIGHT: revive the restart lane (operator, ~10 min, before 00:00Z)

### R0.1 Live diagnostics this plan rests on (2026-08-28 ~19:41Z)

- Engine up: pid 16002, boot 2026-08-27 00:00:44Z (43.7 h run — the Aug-28
  midnight turn never happened). BN + HL flowing; orders emitting.
- `launchctl print gui/$UID/com.multivenue.daily-restart`: `last exit code =
  78: EX_CONFIG`, **penalty box**, runs=6976, program path correct.
  `restart.log`: last drain line `20260827`; stamp file `20260827`.
- Scripts: all five `scripts/*.sh` currently `rwx--x--x`, mtime Aug 27 15:26
  (licence-pass rewrite); `zsh -n daily-restart.sh` clean. The script itself
  has no exit-78 path ⇒ the 78 is launchd's **spawn-level** EX_CONFIG,
  consistent with the documented exec-bit strip + inode replacement
  (`> tmp && mv`), with the penalty box outliving the repair.
  (Disambiguation: `engine-wrapper.sh` line 21 uses `exit 78` for a failed
  `cd` — an unrelated, coincidental use of the same code. Do not conflate.)
- Venue darkness now: OKX last tick 2026-08-27 07:59:31Z; Deribit ~08:02Z
  (Defect A, ~36 h); PM ~22:32Z Aug-27 (Defect B, unbounded due to F0).
  The whole OptSummary/IV surface is dark with OKX+Deribit.
- Catalog: trailing streak **5** gap-free days (2026-08-23…27);
  `harness_ok_runs 30/30`; `whole_root_backtestable true`;
  `days_gate_coverage_sufficient true`; `monitor_view.would_run true`.
  The two pre-M3 aborted-boot dirs were already archived by the Aug-27
  retention pass (visible in `restart.log`) — C6 step 2 is satisfied by
  side effect.
- Disk: 21 GiB free (< 25 GiB retention floor).

### R0.2 Actions (operator terminal; no file edits)

```sh
# 1. kick the job out of the penalty box
launchctl kickstart -k gui/$(id -u)/com.multivenue.daily-restart

# 2. if `last exit code` returns to 78 within a couple of minutes,
#    fully reload the JOB (never touch com.multivenue.engine):
launchctl bootout   gui/$(id -u)/com.multivenue.daily-restart
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.multivenue.daily-restart.plist
```

**Expected immediate effect:** the first successful run sees stamp
`20260827` ≠ today ⇒ it SIGTERM-drains the engine at once (M1d-proven clean
drain), launchd KeepAlive relaunches through `engine-wrapper.sh` → universe
refresh → fresh boot discovery → **PM, OKX and Deribit revive** (~40 s dark,
inside the 300 s catalog tolerance). Retention will also run and, given F11,
archive more old runs — expected, not a fault. Tonight's 00:00Z turn then
fires normally (stamp flips to `20260829`).

### R0.3 Verification checklist

1. `launchctl print …daily-restart` → `last exit code = 0`, runs
   incrementing, penalty box gone.
2. `restart.log` gains a `20260828` drain line; stamp file reads `20260828`.
3. New `run-<ns>` dir; new engine pid; boot log shows PM 4/4 resolved and two
   `discovery: options chain … selected=32` groups; `options-manifest.tsv`
   ≈192 rows, `instrument-manifest.tsv` ≈204.
4. Within minutes: OKX/Deribit/PM tick counters advancing (catalog or
   5 s-summary log lines); `opt_summaries` growing again.
5. After 00:05Z: a second drain line (`20260829`) — the lane is autonomous
   again.

**Interim manual revive lever (until T2 lands):** with the job healthy, writing
yesterday's date into `~/multivenue/state/last-restart-utc-day` forces a
drain+reboot within 60 s. This is the sanctioned mid-day revive after the
08:00Z settlement kill, at the operator's discretion.

**Expectation until T2:** Defects A/B remain — each day OKX+Deribit die at
08:00Z (≈16 h dark) and PM at ~16:14Z (≈8 h dark) until the next midnight
turn. R0 restores the *daily* self-heal, not intraday coverage.

---

## C6 — close M3 (first session in an operator-authorized window)

Execute the standing procedure (`docs/m3-progress.md` "C6 closing procedure")
— its evidence is already in hand:

1. Catalog exit tell: `continuity.trailing_streak = 5 ≥ 3`, dates
   2026-08-23…27 consecutive. (Step 2 of the procedure — archiving the two
   header-only dirs — was already done by the Aug-27 retention pass;
   `whole_root_backtestable=true` confirms.)
2. Real-capture backtest through the FROZEN argv over `~/multivenue/logs`
   with a ruleset that fills (H6a prior from `~/multivenue/worker/demo-h6a/demo/`,
   or a sacrificial in-session ruleset); PASS = `oos.trading_days >= 2`.
3. Close: exit entry in `docs/m3-progress.md` + CLAUDE.md CURRENT STATE
   refresh (M3 CLOSED; note F0 incident + this plan) + operator-authorized
   `M3:` commit, explicit paths.
4. Same window, separate commit(s): add
   `docs/capture-continuity-outage-2026-08-27.md` and this plan file to the
   repo (they are currently untracked).

C6 measures whole-run wall-clock continuity — the outage doc (§2) is explicit
that Defect A/B does **not** touch its arithmetic. Closing C6 is what opens
the M3 C6+ ownership window that T2 and the D-phase need.

---

## T2 — Tier-2 restarts: coverage with zero engine-code change (C6+ window)

**Design.** Generalize `scripts/daily-restart.sh` from one stamp to UTC
**slots `{00:00, 08:30, 16:05}`** (rationale unchanged from the outage doc §7:
08:30 clears Deribit's 9–19 min post-settlement removal lag; 16:05 picks up
the PM daily that went live at 16:00Z; `StartCalendarInterval` stays ruled out
because launchd calendars fire in LOCAL time). Keep the 60 s `StartInterval`
poll; the plist is untouched.

```zsh
# sketch (one stamp file per slot, e.g. last-restart-utc-<slot>)
now=$(date -u +%H%M); today=$(date -u +%Y%m%d); due=0
for slot in 0000 0830 1605; do
  [ "$now" -ge "$slot" ] && [ "$(cat $STAMP.$slot 2>/dev/null)" != "$today" ] \
    && { echo "$today" > "$STAMP.$slot"; due=1; }
done
# ONE drain even if several slots are due (wake/catch-up guard);
# retention runs on the 0000 slot only.
[ "$due" = 1 ] && drain
```

**Precondition (outage §5.6, verify before trusting the 16:05Z slot):**
confirm the tokens written by the boot-time universe refresh belong to the
market that is live *at that moment*. `engine-wrapper.sh` runs
`claude_worker.universe_refresh` on **every** boot (verified in source), so
the empirical tell is simply: **PM ticks flow after the 16:05Z boot.** If they
don't, the refresher is selecting the expiring market — fix it (M3-owned,
worker module) before relying on this slot.

**Verification:** the next full UTC day carries 3 run dirs and still scores
`gap_free=true`; per-venue coverage from the catalog rises from ~8 h
(OKX/Deribit) and ~16 h (PM) to ≈23.5–24 h; `opt_summaries` present in all
three windows. Cost: two extra ~40 s dark windows per day (tolerance 300 s;
Aug-23 had 3 runs and scored gap-free — precedent).

**Files:** `scripts/daily-restart.sh` (M3-owned; SPDX header preserved;
`make license-check`; verify no mode changes) + a runbook paragraph in
`docs/local-setup.md` + `docs/m3-progress.md` entry. No engine rebuild, no
gate re-run required (config/script only). **Side effect for M5:** restart
count 1→3/day multiplies the F10 re-commit points — carried into M5P.

---

## T1 — Tier-1 diagnosability (operator-authorized window; M2-owned + shared paths)

Three small edits, one window, per outage §7 tier 1. All sit on the
session-teardown/reconnect path — **not** the tick hot path; the tick-path
zero-alloc law is untouched and the alloc gate must stay 38/38.

**(a) Name the failure.** Carry the discarded cause through the twelve
`RunResult::Error` sites (`ingress-okx/src/run_loop.rs` :1088/:1097/:1106/
:1116/:1147/:1158; `ingress-deribit` mirrors) and the parsed OKX error code
(`OkxMsgKind::Error(u32)` — parsed today, discarded at the boundary) into the
`run-loop returned` log lines in `crates/cli/src/paper.rs` (:862, :1059).
Result: the next 08:00Z settlement and the chronic pre-08:00Z OKX churn
(§5.4) become self-describing, closing all three §6 unknowns at their next
occurrence.

**(b) Re-arm the backoff.** Replace the `msgs_total()`-based reset
(`paper.rs` :868-870, :1065-1067) with a **ticks-moved** predicate: reset the
capped-exponential schedule only when the session advanced the venue tick
counter, not when it merely received the venue's rejection. Restores the D8
intent and stops the ~1 Hz hammering that plausibly earns the Deribit ban.
(If the loop-visible status struct lacks a per-venue tick counter, add a plain
`u64` field — additive, POD, no allocation.)

**(c) Make dead lanes visible.** Per-venue `last_tick_age_seconds` gauge on
the `127.0.0.1:9191 /metrics` page, plus a `restart_stamp_age_seconds` gauge
(age of the newest slot stamp) so the F0 failure class can never again run
silent for 28 h. Gauges only — no new endpoint, no observability stack.

**Deliberately NOT in T1** (Tier-3 material, §9): making per-arg subscribe
failures non-fatal, adding connect/session-establishment timeouts, mid-run
re-discovery. T1 changes what we *know*, T2 changes what we *get*; neither
touches session-lifecycle semantics — that is what keeps this window small
and safe.

**Tests & gates:** unit tests for the backoff predicate (rejection-only
session ⇒ escalates; ticking session ⇒ resets) and the gauge plumbing;
happy+failure path per touched public fn. Full stay-greens on the Mac:
nextest (≥1240, may grow) · alloc 38/38 (`--test-threads=1`, fresh
`Compiling bench`) · pytest 439 untouched. No new untrusted-bytes parser ⇒
no new fuzz target (cause propagation re-labels existing errors). G0: relink
release; the standing engine picks it up at the next slot restart. Live
smoke: observe a **named** error line within the hour (OKX churn), and the
settlement path names itself at the next 08:00Z.

**Files:** `crates/ingress-okx/src/run_loop.rs`, `crates/ingress-deribit/src/run_loop.rs`
(M2-owned — explicit operator authorization required, M2 is CLOSED),
`crates/cli/src/paper.rs` (shared), metrics module, their tests. Commit
prefix per operator instruction, explicit paths.

---

## D — C6+ data-plane catch-up (M3/worker ownership window; verbs serialized)

Order within D is flexible; D1 first (cheapest, unblocks map completeness).

- **D1 — fetch / map refresh (F5).** After T2's first full-coverage day:
  source `.env` (H6b wrapper pattern), `pgrep -f claude-worker` guard, run
  `claude-worker fetch` once. Done-tell: `unresolved=0`; map gains §9.4
  options descriptors (resolved through the per-run
  `options-manifest.tsv`/`instrument-manifest.tsv` sidecars). Prune any stale
  PM entries the conflict report names.
- **D2 — nightly pnl timer (F8; the M4 D2 deferral landing here as ruled).**
  Wire `python -m claude_worker.pnl_report` into the nightly cadence — either
  a fourth launchd agent or a post-00:00Z-slot step; serialized behind the
  worker-verb law (`pgrep` first); writes `pnl-<day>.json` + `.summary.txt`.
- **D3 — iv_digest cadence (F7).** Hook `python -m claude_worker.iv_digest`
  (rolling window) into `scripts/candles-cycle.sh` after the candle pass —
  inherits the hourly agent's serialization and top-of-hour avoidance. Note:
  its usable input becomes near-full-day only after T2.
- **D4 — candles.db breadth (F6) — operator decision required.** §4-M5 says
  "per-sym OHLCV everywhere + an IV summary". Two readings:
  (i) **recommended:** OHLCV for spot/futures/perp syms + PM, IV surface via
  `iv_digest` for options (REST budgets stay sane; options are keyed by
  stable descriptor either way); (ii) literal: REST OHLCV for all ~192
  option instruments too — heavy against free-tier budgets and mostly
  duplicative of the OptSummary channel. Decide before M5 design entry.
- **D5 — optional tidies flagged at M2 close:** fold `SLOT_KIND_OPT_SUMMARY=6`
  decode + the `_run_anchor_ns` mirror into `claude_worker/pmlr.py`;
  a catalog row for the manifest sidecars if ever wanted.

Each D item: additive worker files where possible, frozen surfaces untouched
(the 8-verb pin stays; D2/D3 are modules/scripts, not verbs), pytest grows
additively, SPDX on new files, license-check per commit.

---

## M5P — M5-prep implementation (starts ONLY on explicit operator go; design entry first)

The two ruling-#7 items are §4-M5 scope; they appear here because the outage
assessment surfaced them as gate blockers.

- **F9 / #7a — digest inventory sections.** `build_digest`
  (`claude-worker/src/claude_worker/strategist.py`) gains POSITIONS (from the
  `positions` netting) and PER-STRATEGY P&L (from `pnl-<day>.json`), capped,
  with honest empty-section rendering when sources are absent.
- **F10 / #7b — post-restart re-commit.** After **each** of the (now 3)
  daily boots, the active registry ruleset must be re-staged + re-committed
  from its bound paths or the M5 exit criterion silently lapses. Mechanism:
  a small script (or M5 runbook step) using the EXISTING frozen verbs —
  wait for `ai.sock`, `stage-ruleset`, `commit-ruleset`, then verify
  `mask/epoch/vm_rows` on `/metrics`. The 8g gating pin stands: the vm
  member must be enabled for commit. No verb-surface change (the D1-unfrozen
  `pnl` verb remains the ONE amendment).
- Then the M5 proper work per `docs/mvp-completion-plan.md` §4-M5 (full
  universe digest widening, walk-forward runbook, promotion on real capture)
  — outside this plan's scope, on operator go, design doc first.

---

## Re-baseline & M6 readiness

Per outage §8.4: **post-fix day 0** = the first full UTC day with T2 (and
ideally T1) live and verified. The 7-day M6 soak starts no earlier than
post-fix day 0. Pre-fix days (Aug 23–27) remain valid evidence for
engine-uptime/C6 but NOT for full-universe capture claims.

### "M5 gate open" checklist

| # | condition | tell |
|---|---|---|
| 1 | Restart lane autonomous (F0) | two consecutive unattended 00:00Z turns post-R0 |
| 2 | M3/C6 CLOSED (F4) | close entry + commit; CLAUDE.md refreshed |
| 3 | Intraday coverage restored (F1/F2) | catalog: per-venue coverage ≈24 h for a full day; 3 run dirs/day, gap-free |
| 4 | Failures named + visible (F3/F12) | named `run-loop returned` lines; `last_tick_age` + stamp-age gauges live |
| 5 | Map/candles/IV/pnl current (F5–F8) | `unresolved=0`; candle cadence per D4 ruling; hourly iv_digest rows; nightly pnl files |
| 6 | #7a/#7b implemented (F9/F10) | digest carries inventory; `vm_fires > 0` survives a midnight turn |
| 7 | Operator go for M5 | explicit; design entry first |

---

## Deferred (recorded, not scheduled)

- **Tier 3** (outage §7): non-fatal per-arg subscribe drops (fail-fast
  narrows to boot), connect/establishment timeouts not `Steady`-gated,
  mid-run chain re-discovery — which REQUIRES manifest epoch/append
  semantics first (per-run manifest law; M2 close / M4 D3). M-phase sized,
  operator-gated; restarts are sufficient for the MVP.
- `cargo fmt` (~88 files) / `clippy -D warnings` (~40 lints) — own commits.
- Binance eapi WS lever (`BINANCE_EAPI_WS_HOST`) — revisit at/before M6.
- F11 disk: retention handles it; if free space keeps trending down after
  the T2 3-runs/day regime, revisit the floor/archive policy in the C6+
  window.

## Risk register

| risk | mitigation |
|---|---|
| R0 kickstart triggers an immediate drain | intended — it is the venue revival; ~40 s dark, within tolerance |
| `bootout` aimed at the wrong label | commands above name `daily-restart` only; **never** bootout `com.multivenue.engine` |
| 08:30Z slot picks an instrument Deribit removes late | slot chosen after the observed 9–19 min removal lag (outage §7); if a kill recurs at ~08:3x, shift to 08:45Z |
| 16:05Z slot doesn't fix PM | §5.6 check in T2; fix `universe_refresh` selection before relying on it |
| script edits strip exec bits again | `git diff --summary` check is now a hard rule; verify after every `> tmp && mv` |
| concurrent verbs/pytest collide | serialization law: `pgrep -f claude-worker` / `pgrep -f pytest` first |
| sandbox cargo false-greens | all builds/tests on the Mac only (pitfall #10) |
| T1 regresses zero-alloc | edits confined to teardown paths; alloc gate 38/38 re-run in-window |
| re-commit automation drifts the frozen surfaces | uses existing verbs only; the 202-test frozen pin + 8-verb amendment stay byte-green |

## Sequence & effort

```
R0  tonight, before 00:00Z          ~10 min   operator terminal only
C6  next authorized window          ~½ day    close M3 + commit docs
T2  immediately after C6            ~½–1 day  script + runbook + observe a full day
T1  next authorized window          ~1–2 days edits + gates + live smoke
D   C6+ window, interleaved         ~1–2 days D1→D2→D3 (+D4 ruling, D5 optional)
M5P on explicit M5 go               inside M5's 2–3 day estimate
     → post-fix day 0 → M6 soak eligibility
```

Total new calendar to a defensible M6 start: roughly **4–6 working days**
after tonight's R0, dominated by wanting one clean full-coverage day after
T2 and the T1 gates window.

---

## 12. IMPLEMENTATION STATUS — coding phase COMPLETE (2026-08-28 session)

Operator directive: ALL coding first; runs/gates/long tests follow as a
separate phase. **Everything below is WRITTEN and syntax-checked
(py_compile + `zsh -n` + no-mode-change verified) but NOT compiled,
NOT gate-run, NOT live-proven.** The run phase is scripted in
`docs/prompts/remediation-run-phase.md` (the handover doc — read it
first in any fresh session).

| phase | state | files |
|---|---|---|
| R0 | operator terminal action, still PENDING (lane was dead at 20:00Z) | none (runtime only; procedure updated — see delta 1) |
| T1 | CODE-COMPLETE | `crates/core-metrics/src/{ingress_status.rs,lib.rs}` · `crates/ingress-{polymarket,binance,okx,deribit,hyperliquid,rpc}/src/run_loop.rs` · `crates/cli/src/paper.rs` (+7 new Rust tests) |
| T2 + D2 | CODE-COMPLETE | `scripts/daily-restart.sh` (slots 0000/0830/1605 + 0020 pnl action slot) |
| D3 | CODE-COMPLETE (goes live at the candles agent's next hourly fire) | `scripts/candles-cycle.sh` |
| M5P #7b | CODE-COMPLETE (arms at the next engine boot — operator may disarm, see delta 4) | `scripts/engine-wrapper.sh` · `scripts/recommit-ruleset.sh` (new) · `claude-worker/src/claude_worker/recommit.py` (new) · `claude-worker/tests/test_recommit.py` (new) |
| M5P #7a | CODE-COMPLETE | `claude-worker/src/claude_worker/{strategist.py,daemon.py}` · `claude-worker/tests/test_strategist_inventory.py` (new) |
| C6 / D1 / gates | RUN PHASE | per the handover doc |
| D4 | **operator ruling still open** — note: candles.db's 12 descriptors = exactly the non-options universe, so reading (i) closes F6 with ZERO code | — |
| D5 | deliberately NOT implemented (optional tidy; unchanged) | — |

Deltas from the plan as written (decided while coding; all recorded in
the touched files' comments too):

1. **T2 migration seed changes R0.** A missing slot stamp seeds to
   today WITHOUT firing (deploy-safety: rewriting a minutely-executed
   script must never trigger a surprise drain). Consequence: after the
   launchd kickstart the old "stamp mismatch ⇒ immediate drain"
   no longer happens — the sanctioned revive lever is
   `echo 19700101 > ~/multivenue/state/last-restart-utc-0000`
   (fires within 60 s). Tonight's 00:00Z still fires normally.
2. **D2 rides the restart poller** as an 0020 ACTION slot (no fourth
   launchd agent, no plist changes): `claude_worker.pnl_report` runs
   with no args (its own day resolution), deferred minute-by-minute
   while any worker invocation is live.
3. **T1 scope refinements:** diag triple (`err_site` / `io_kind` /
   `venue_code`) lives in `IngressStatus` (same-thread write/read;
   first-error-wins; slot still 128 B) — full diag wired for
   okx+deribit (the outage pair), `add_ticks` data-arm accounting
   wired for ALL SIX venues (the ticks-based backoff predicate is
   loop-uniform; venue-quiet IdleTimeout/Stale trips also reset).
   The Binance MULTI lane keeps its in-lane per-slot pacing
   (untouched — eapi retry behavior unchanged). Deribit's
   subscribe-missing site reports the COUNT of missing channels
   (u128 masks don't fit the u32 diag slot).
4. **#7b arms itself at the next boot** through the wrapper's
   backgrounded child. The registry holds the H6b prior ⇒ the next
   boot will genuinely re-stage + re-commit it (gate binding
   re-verifies; drift refuses; epoch bumps — the H6b-proven shape).
   To disarm until gates pass: comment the `recommit-ruleset.sh` line
   in `engine-wrapper.sh`. The handover doc puts this decision FIRST.
5. **#7a daemon wiring passes no HIP-4 pairs** (the map file's pairs
   stay a cli concern; per-sym netting + totals carry the signal).
   The semi-manual M5 lane calls the same helpers with the map's
   pairs — an M5-design item, not a gap in this pass.

Expected gate deltas (run phase verifies): nextest 1240 → **≥1247**
(+3 core-metrics, +1 okx, +1 deribit, +2 cli) · alloc **38/38
unchanged-green** (every new hot-side op is a relaxed atomic; the
teardown/publish paths are the sanctioned cold class) · pytest 439 →
**≈454** (+6 recommit, +9 inventory; frozen 202 byte-untouched;
`backtest.py`/`cli.py` untouched) · `make license-check` green (SPDX
on all four new files; no new dependencies ⇒ no `license-deps` run).

