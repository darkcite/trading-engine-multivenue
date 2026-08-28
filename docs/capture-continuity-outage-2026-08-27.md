# Capture-continuity outage — expired instruments kill whole venue sessions

**Date:** 2026-08-27 · **Status:** ROOT CAUSE PROVEN, unremediated · **Author:** investigation session (read-only; no code, config or git state was changed)

**Scope.** Two independent capture-continuity defects found while assessing the
M5 entry gate. Both are invisible to every existing health signal and both have
been silently degrading the standing lane since the 2026-08-23 full-scope flip.

1. **Defect A (severe):** OKX and Deribit sessions die permanently at ~08:00Z
   every day when the boot-selected option chain settles. ~16 dark hours per
   24-hour day, on two of five venues.
2. **Defect B (moderate, adjacent):** Polymarket goes dark at ~16:14Z every day
   when the daily up/down market resolves, and is not re-armed until the
   00:00Z restart. ~8 dark hours per day.

Net: **only 2 of 5 venues (Binance, Hyperliquid) actually have 24-hour
coverage.** Nothing in the catalog, the `/metrics` gauges, or the
`audit-replay` integrity totals reports this, because every one of them is
blind to a lane that stops existing.

Authority: this document is a finding, not a plan. Remediation is the
operator's call; §7 lays out the options. No M-phase is opened by it.

---

## 1. Summary

The reconnect loop re-subscribes to a **frozen boot-time instrument table**.
Option instruments are not permanent — the front expiry settles daily at
08:00 UTC on both Deribit and OKX. After settlement the venue answers our
subscribe with "that instrument does not exist"; the ingress classifies that
answer as a **fatal session error** (correct doctrine for a boot
misconfiguration, wrong for a reconnect), tears the session down, reconnects,
re-sends the identical dead subscribe, and fails again — roughly once per
second, forever.

Because one connection carries the entire venue (66 instruments on OKX, 65 on
Deribit), **32 expired options destroy the perpetual and spot streams with
them.** The lane cannot recover; only the 00:00Z process restart, which re-runs
boot discovery and picks a fresh chain, brings it back.

---

## 2. Impact, quantified

Per-venue coverage inside each 24-hour standing run (from
`capture-catalog` per-venue `first_ts_ns` / `last_ts_ns`):

| Venue | Coverage per day | Dark per day | Cause |
|---|---|---|---|
| Binance | 24h00m | — | healthy |
| Hyperliquid | 24h00m | — | healthy |
| **OKX** | **~7h59m** | **~16h01m** | Defect A |
| **Deribit** | **~8h10m** | **~15h50m** | Defect A |
| **Polymarket** | **~16h13m** | **~7h47m** | Defect B |

Observed vs. extrapolated ticks, Aug 24–26 (three full days):

| Venue | Captured | Extrapolated at observed rate | Approx. lost |
|---|---|---|---|
| OKX | 4,967,652 | ~14.9M | **~10M** |
| Deribit | 3,558,062 | ~10.7M | **~7.1M** |

(Order-of-magnitude only — intraday rate is not uniform across sessions.)

The **options mark/IV channel (`SlotKind::OptSummary`) is carried exclusively by
OKX and Deribit** — the Binance eapi lane is dark by operator ruling (M2.4,
`BINANCE_EAPI_WS_HOST` lever). So the entire §9.8 IV surface dies with them:
2,471,990 / 2,503,715 / 1,960,981 opt-summary records on Aug 24/25/26 all
stop at 08:00Z.

**Consequence for M5 and M6.** The M5 exit criterion is a promotion on real
capture "trading paper on the **full universe**". The capture is not
full-universe: for two-thirds of every day it is a Binance+Hyperliquid feed. A
7-day M6 soak started on this substrate would bake the hole into the sign-off
evidence. The M3/C6 gate itself is unaffected — its arithmetic is *whole-run*
wall-clock continuity, which is genuinely gap-free (trailing streak 4, ending
2026-08-26); C6 measures that the engine was up, not that the venues were.

---

## 3. Evidence

### 3.1 The failure is aligned to the UTC clock, not to elapsed runtime

Last tick per venue, converted to wall clock from each run's anchor:

| Run start (UTC) | OKX last tick | Deribit last tick | Polymarket last tick |
|---|---|---|---|
| Aug 24 00:00:28Z | **07:59:47Z** | 08:19:01Z | 16:13:58Z |
| Aug 25 00:00:12Z | **07:59:48Z** | 08:17:51Z | 16:14:58Z |
| Aug 26 00:00:58Z | **07:59:37Z** | 08:09:19Z | 16:13:34Z |

Three consecutive days, same wall-clock minute, on runs that started at three
different offsets. No elapsed-time mechanism (buffer growth, leak, fd
exhaustion, memory pressure) can produce that signature. 08:00 UTC is the daily
options settlement on both venues; Deribit's 9–19 minute lag is its
post-settlement instrument removal, not the expiry instant.

### 3.2 The error volume starts exactly at the options flip

`run-loop returned res=Error` counts per UTC day, per venue
(`~/multivenue/logs/launchd/engine.out.log`):

| Day | OKX | Deribit | Hyperliquid | Polymarket |
|---|---|---|---|---|
| Aug 22 (options **OFF**) | 37 | **1** | 31 | 184 |
| Aug 23 (flip 06:14Z) | 68,501 | 36,809 | 144 | 338 |
| Aug 24 | 82,457 | 46,192 | 151 | 219 |
| Aug 25 | 80,945 | 44,966 | 163 | 474 |
| Aug 26 | 81,512 | 17,232 | 275 | 146 |
| Aug 27 (to 03:25Z) | 9,153 | 40 | 4 | 8 |

The two venues that carry option chains go from single-digit daily errors to
tens of thousands, on the day options were enabled. The two that do not are
unchanged. Total across the log: **469,962** run-loop returns, **469,891** of
them `Error`.

### 3.3 The Aug-26 hourly profile shows both the kill and the wedge

Errors per hour, 2026-08-26:

```
hour  00   01   02   03   04   05   06   07 │ 08   09   10   11   12   13 │ 14 … 23
okx  2199 2220 2459 2526 2514 2474 2549 2729│3738 3966 3933 3967 3888 3701│3782 … 4015
drbt  170  115   44   31    5   25   12   49│2439 2951 2934 2937 2904 2621│  0 …    0
```

- **OKX** churns from boot (~2,500/h) — see §5.4 — and steps up to ~3,900/h at
  08Z, at which point ticks stop entirely and never resume. The lane spins at
  ~1 Hz for the remaining 16 hours.
- **Deribit** is near-quiet until 08Z, explodes to ~2,900/h, and then **stops
  logging altogether after `2026-08-26T13:58:29.674317Z`** — 17,237 Deribit
  lines that day, none in the last ten hours. The thread did not exit (it
  cannot; see §5.3) — it wedged.

### 3.4 What is ruled out

- **Not the machine, network or process.** Binance and Hyperliquid ride the same
  process, the same NIC and the same 24-hour window with full coverage. The
  engine PID is continuous; `caffeinate` is loaded.
- **Not a clean venue disconnect.** `RunResult::Disconnected` is a distinct
  variant reached via peer-close (`fill_rx` → `Ok(0)` → `State::Closed`,
  `ingress-okx/src/run_loop.rs:1118-1120`). The log says `Error`, which is the
  protocol/fatal path.
- **Not shutdown.** `RunResult::Stopped` requires the SHUTDOWN flag; the process
  ran on for another 10 hours.
- **Not the capture layer.** `harness_ok` is true for all 30 runs and
  `whole_root_backtestable` is true; the files are clean, they simply contain
  nothing after 08:00Z for these venues.

---

## 4. Reproduction / verification

Read-only, no engine interaction:

```sh
# per-venue first/last tick per run (the §3.1 table)
./target/release/multivenue-engine capture-catalog --dir ~/multivenue/logs

# the §3.2 table — note the ANSI strip, without it every grep silently
# returns zero (the log is colourised; 'res=Error' is split by escapes)
cd ~/multivenue/logs/launchd
grep 'run-loop returned' engine.out.log \
  | sed $'s/\x1b\\[[0-9;]*m//g' \
  | awk '{print substr($1,1,10), $3}' | sort | uniq -c

# the §3.3 hourly profile for one day
grep 'run-loop returned' engine.out.log | sed $'s/\x1b\\[[0-9;]*m//g' \
  | grep '^2026-08-26' | awk '{print substr($1,12,2), $3}' | sort | uniq -c
```

**Session fact worth keeping:** `engine.out.log` is ANSI-colourised, and the
escapes fall *inside* field values (`res` ESC `=` ESC `Error`). Any grep for a
`key=value` pair returns 0 matches unless the escapes are stripped first. This
cost one false "no such string" conclusion during the investigation.

---

## 5. Root cause

### 5.1 Defect A-1 — the boot-selected chain is frozen for the process lifetime

`Driver::new(...)` is constructed **once, above** the reconnect loop:

- `crates/cli/src/paper.rs:823` — `owl::Driver::new(now_ns(), symbols, depth_enabled, &fam_refs)`
- `crates/cli/src/paper.rs:1020` — `dwl::Driver::new(now_ns(), symbols, depth_enabled)`

The `while !shutdown_requested()` loop that follows only calls
`driver.reset_for_reconnect(now_ns())`, which resets **protocol** state. The
symbol table — including the 32 option instruments per underlying selected by
boot discovery — is immutable until the process exits. Every reconnect re-sends
`queue_subscribe_all` against that same table
(`ingress-okx/src/run_loop.rs:343-348`).

Boot discovery, which *would* select a live chain, runs exactly once per
process: the log carries five `discovery: options chain venue=… selected=32`
groups for five boots (Aug 23–27), none in between.

### 5.2 Defect A-2 — "instrument does not exist" is treated as fatal on reconnect

**OKX** — `crates/ingress-okx/src/run_loop.rs:959-969`:

```rust
Dispatch::VenueError { code } => {
    // Fail-fast doctrine: a venue error event means our
    // subscribe (or framing) is wrong. Crash loudly in debug,
    // surface a session error in release — the reconnect
    // path applies backoff and the operator sees it.
    debug_assert!(false, "okx venue error event, code={code}");
    return Err(io::Error::new(io::ErrorKind::InvalidData, "okx venue error event"));
}
```

The venue's numeric code *is* parsed (`classify` →
`OkxMsgKind::Error(u32)`, `ingress-okx/src/lib.rs:220-228`) and then discarded
at this boundary.

**Deribit** — `crates/ingress-deribit/src/run_loop.rs:21-24` (module law) and
the `RunResult::Error` doc at `:246-248`:

> The subscribe **result** echoes the successfully-subscribed channel list; any
> expected channel missing ⇒ misconfiguration ⇒ session error (fail-fast).
> Venue `error` responses are equally fatal.

This doctrine is **right at boot** — a subscribe we cannot fulfil means the
config is wrong and we must not run venue-blind. It is **wrong on reconnect**:
the config did not change, the instrument universe did. And because a single
connection carries every instrument for the venue, expired options take
`BTC-USDT`, `ETH-USDT-SWAP` and `BTC-PERPETUAL` down with them.

`Err(...)` from `drive_one` becomes `RunResult::Error`
(`ingress-okx/src/run_loop.rs:1115-1117`), which the spawn loop logs and
retries — into the same wall, forever.

### 5.3 Defect A-3 — the retry escalation is disarmed, and then the thread wedges

**The backoff never escalates.** `crates/cli/src/paper.rs:868-870` (OKX) and
`:1065-1067` (Deribit):

```rust
if status.msgs_total() > msgs_before {
    backoff.reset();
}
```

A failing session still *receives* the venue's subscribe reply before dying, so
`msgs_total` advances and the capped-exponential schedule resets to its minimum
on every cycle. The intent (D8: "a session that moved data restarts the
schedule; a flapping endpoint keeps escalating") is defeated because *receiving
the rejection counts as moving data*. Result: ~1 reconnect/second indefinitely —
81,512 OKX and 17,232 Deribit cycles on Aug 26 alone.

**And then it wedges.** `connect_tls` (`paper.rs:3461-3467`) calls
`TlsTransport::connect`, which uses a **non-blocking** `mio::net::TcpStream`
(`core-net/src/transport.rs:119-131`; `tcp_connected: false`, completion is
poll-driven). It therefore returns `Ok` instantly even when the peer never
answers. If the SYN is blackholed — the natural consequence of six hours of
once-a-second reconnects into a venue that rate-limits connection abuse — the
driver sits in `State::Connecting`, never reaches `State::Steady`, and:

- the keepalive / idle timeout is **gated on `Steady`**
  (`ingress-okx/src/run_loop.rs:1133`, `ingress-deribit/src/run_loop.rs:1527`),
  so `KeepaliveAction::Reconnect` can never fire;
- `poll` returns every 50 ms with no events; `drive_one` does nothing and
  returns `Ok`;
- the loop spins silently, forever.

**There is no connect timeout and no session-establishment timeout anywhere in
the loop.** The only timeout in the design presumes the session already
succeeded. This is what Deribit's ten hours of silence after 13:58:29Z are.

### 5.4 Defect A-4 — the chronic pre-08:00Z churn (open)

OKX errors at ~2,500/h **from boot**, before any settlement, while ticks flow
normally. This is a second, milder instance of the same fail-fast path (some arg
in the boot subscribe draws an error event on most connections), but the exact
trigger is **not proven** — see §6. Its cost is real but bounded: sessions last
~1.6 s, and enough bbo-tbt frames arrive per session that daily tick volume
still looks plausible. It is almost certainly why the M2.3 live smoke saw "103
silent reconnects" and was read as load starvation.

### 5.5 Defect C — the outage is unnamed by construction

Every `RunResult::Error` site discards its cause. In `ingress-okx/src/run_loop.rs`
the six sites are `:1088`, `:1097`, `:1106` (`Err(_e) => return RunResult::Error`
— the error is explicitly dropped), `:1116`, `:1147`, `:1158`; Deribit mirrors
them at `:1481`, `:1490`, `:1499`, `:1509`, `:1538`, `:1549`. The caller logs
`tracing::info!(?res, "okx: run-loop returned")` (`paper.rs:862`, `:1059`) with
nothing attached.

Six days of total venue outage produced 469,891 log lines that say only
`res=Error`. Meanwhile `engine_ingress_okx_state` reads `2` on a lane that has
delivered nothing for hours — `IngressState::Up` is published at the
upgrade→Steady edge (`ingress-deribit/src/run_loop.rs:519-521`) and a 1 Hz churn
means a sampler nearly always catches a lane mid-cycle. **No existing signal —
catalog, gauge, or audit-replay integrity — would have raised a hand.** The
catalog is the closest: it *has* the per-venue `last_ts_ns` that proves the
outage, but nothing derives a per-venue coverage figure from it.

### 5.6 Defect B — Polymarket's daily roll is never re-armed

Independent of A, and by market design rather than protocol error: the PM
crypto up/down dailies **resolve at 16:00Z** (already documented in
`CLAUDE.md` and the M1 runbook). The launchd wrapper refreshes
`universe.toml` via the Gamma lane and restarts **only at 00:00Z**, so the
engine streams the current daily from 00:00Z until it resolves at ~16:14Z, then
has no live PM market until the next midnight boot. The next day's market has
in fact been live since 16:00Z — we simply are not subscribed to it.

Evidence: PM last tick at 16:13:58Z / 16:14:58Z / 16:13:34Z on Aug 24/25/26,
and `universe-refresh: 2 market(s) for 2026-08-2N` appearing once per day in
`engine.err.log`. **One check remains** to close it fully: confirm the token ids
written at the 00:00Z refresh belong to the market expiring at 16:00Z that same
day (rather than the one starting then). §7 tier 2 assumes this reading.

---

## 6. What is proven, and what is not

**Proven:**

- The 08:00Z alignment (§3.1) and its exclusivity to the two option-carrying
  venues (§3.2).
- That the instrument table is frozen across reconnects (§5.1, code).
- That a venue error / missing subscribe confirmation is fatal to the whole
  session (§5.2, code + module law).
- That the backoff cannot escalate and no establishment timeout exists
  (§5.3, code).
- That the failure carries no diagnostic payload (§5.5, code).

**Not proven — and the same one-line change closes all three:**

- The exact OKX error code returned post-settlement (expected 60018-class,
  "instId doesn't exist"). Never logged.
- Whether Deribit's terminal wedge is a rate-limit ban (§5.3) or a different
  stall in `Connecting`/`AwaitingWsUpgrade`.
- The trigger for the chronic pre-08:00Z OKX churn (§5.4).

Logging the discarded `io::Error` and the OKX code on the `run-loop returned`
line names all three at their next occurrence — which for OKX is within the
hour, and for the settlement path is the next 08:00Z.

---

## 7. Remediation options

Presented cheapest-first. **The obvious repair — re-run discovery on reconnect —
is not the cheap one**, because it collides with a standing law: option ordinals
are allocated at boot in selection order and `options-manifest.tsv` /
`instrument-manifest.tsv` are written **once per run** (M2 close; M4 D3;
`docs/wire-format.md`). Re-selecting a chain mid-run would silently re-map
SymbolIds inside a run that has exactly one manifest — which corrupts every
offline descriptor consumer (`audit-pnl`, `iv_digest`, the worker map) far more
seriously than the outage does. Mid-run re-discovery therefore requires the
manifest to gain epochs or append semantics **first**.

### Tier 1 — name the failure (small, safe, do regardless)

- Carry the discarded `io::Error` (and the parsed OKX code) into the
  `run-loop returned` log line.
- Do not reset the backoff for a session that produced **no ticks** — distinguish
  "moved data" from "received a rejection". This alone stops the 1 Hz hammering
  that plausibly earns the Deribit ban.
- Optional, same edit: add a per-venue `last_tick_age` gauge so a dead lane is
  visible on `/metrics`.

Touches `crates/cli/src/paper.rs` and the two ingress run-loops. Both are
M2-owned paths; M2 is CLOSED, so this needs an operator-authorised window.
Requires a gates re-run (1240 / 38 / 439) and a live smoke.

### Tier 2 — restore coverage with zero code change (recommended interim)

Add two more launchd restarts to the existing daily wrapper:

| Time | Closes | Note |
|---|---|---|
| 00:00Z | (existing) | day boundary, PM refresh |
| **~08:30Z** | Defect A | **after** Deribit's removal lag — a restart at 08:05Z risks selecting an instrument Deribit removes at ~08:19Z, reproducing the failure 15 minutes later |
| **~16:05Z** | Defect B | picks up the daily that went live at 16:00Z |

Each restart re-runs boot discovery and selects a live chain, which then
survives until the next settlement. Cost: two extra dark windows of ~40 s
(observed restart cost: 10–39 s), against a catalog gap tolerance of 300 s.
Days would carry 3–4 run dirs instead of 1 — already normal (Aug 23 had 3 runs
and scored GAP-FREE). Recovers roughly 16 h/day on two venues and 8 h/day on a
third.

This is not merely cheaper than tier 3 — it is **more consistent with the
current design**, because a restart is exactly the event the per-run manifest
law is built around.

### Tier 3 — the real fix (M-phase sized, operator-gated)

1. Per-arg subscribe failures become non-fatal drops (with a counter and a loud
   log) instead of session kills; the fail-fast doctrine narrows to **boot**.
2. Connect and session-establishment timeouts that are **not** `Steady`-gated,
   so a lane that never establishes is torn down and retried rather than wedging.
3. Only then: mid-run chain re-discovery, once the manifest carries epochs.

---

## 8. Recommended sequence

1. **Close C6 / M3 first.** Its gate is genuinely met (trailing streak 4, ending
   2026-08-26; 30/30 harness-clean; `whole_root_backtestable: true`) and this
   finding does not touch that arithmetic.
2. **Tier 2 now** — it is config-only, needs no gate re-run, and makes the
   capture honest before any M5 promotion is authored on it.
3. **Tier 1 next** — in the same operator window, so the next occurrence is
   self-describing and §6's three unknowns close themselves.
4. **Re-baseline before M6.** The 7-day soak should start on post-fix capture;
   pre-fix days are legitimate for engine-uptime evidence but not for
   full-universe evidence.
5. **Tier 3** only if the operator wants continuous in-process resilience before
   Stage 3. Restarts are sufficient for the MVP.

---

## Appendix A — adjacent M5-gate findings (not caused by this outage)

Recorded here so the assessment that surfaced the outage is not lost. These are
independent of §5 and each is separately actionable.

- **`market-map.json` is stale** — last written 2026-08-22 19:57, i.e. before the
  Aug-23 flip. Holds the Aug-22 PM dailies plus 12 core descriptors and **zero
  options descriptors**. `fetch` has not run since (`state.db` also Aug 22). M5
  requires map completeness for every observed sym.
- **`candles.db` breadth** — fresh (hourly agent current to 02:27Z on Aug 27) but
  only **12 descriptors**, against a 204-instrument universe. M5 wants per-sym
  OHLCV across the universe.
- **`iv_digest` is stale and partial** — 571 rows / 128 descriptors, OKX and
  Deribit only, `max(open_ts)` 2026-08-22 19:11Z. Cadence was never wired (a
  C6+ item); it has only ever run manually. Note the interaction with §2: even
  once wired, it can only ever digest the ~8 h/day that survives Defect A.
- **`pnl` reports** — only `pnl-2026-08-23.*` exists; the M4 D2 nightly timer is
  still deferred to the C6+ window.
- **Digest inventory sections missing** — `build_digest`
  (`claude-worker/src/claude_worker/strategist.py:260`) emits MARKET MAP,
  OBSERVED CAPTURE UNIVERSE, FEATURES, ACTIVE RULESET WALK-FORWARD and NEWS.
  Ruling #7(a)'s POSITIONS and PER-STRATEGY P&L sections are unimplemented.
- **Ruling #7(b) post-restart re-commit is unimplemented and already biting** —
  live `/metrics` reads `engine_vm_fires_total 0` and
  `engine_vm_orders_emitted_total 0`: the committed ruleset table is in-memory
  and every 00:00Z restart clears it. Latency-arb is emitting normally
  (`engine_orders_emitted_total 71506` this run), so the paper lane is alive,
  but nothing AI-authored is trading. Note that tier 2 above **adds two more
  restarts per day**, which multiplies the number of re-commit points the M5
  runbook must cover.

## Appendix B — state at time of writing

- **Now:** 2026-08-27 ~03:25Z. Engine live under launchd (`com.multivenue.engine`,
  pid 16002); `caffeinate`, `daily-restart` and `candles` agents loaded.
- **Current run:** `run-1787788844280759000`, started 00:00:44Z, 3h23m in,
  6,594,302 ticks. OKX already churning (9,153 errors); Deribit still healthy
  (40) — its 08:00Z kill is ~4.5 h away at time of writing.
- **Catalog:** 30 runs · 246,994,986 ticks · 16,196 MiB · 6 UTC days ·
  `trailing_streak 4` ending 2026-08-26 · `whole_root_backtestable: true` ·
  `monitor_view.would_run: true`.
- **Repo:** HEAD `7676cbe`. Working tree dirty with pre-existing off-plan items
  (`.gitignore` modified; untracked `EXTERNAL STRATEGIES TO ONBOARD/` and three
  `docs/*.md` research files from Aug 23–24). This document is a new file and
  stages independently; no git operation was performed.
