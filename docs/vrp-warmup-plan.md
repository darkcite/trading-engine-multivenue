<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright 2026 Anton (darkcite) -->

# VRP W1–W5 — the HAR window must survive a restart

**Status:** planned 2026-09-11, operator-ruled the same day.
**Trigger:** the first live campaign did not trade.

## 1. What happened

At 00:00:00Z on 2026-09-11 the member made its one decision of the day
for the `BTC-11SEP26` 08:00Z expiry and produced nothing:

```
engine_vrp_decisions_total 1
engine_vrp_entries_total   0
engine_vrp_holds_total     0
engine_vrp_no_bounds_total 1
```

Everything upstream of the forecast worked. `vrp-state.tsv` carries the
selection it made —

```
C  1789113600000000000  76500000000  0  0  0  0  0
```

— the **$76,500 call expiring 08:00Z on the 11th**, which is the right
instrument. `decide` reached `no_bounds`, and that arm sits *after* the
staleness gate, so the option mark and its IV were both fresh at the
decision instant. Selection, the currency filter, the denomination law,
the mark pipeline and the decision rung are all live and correct.

## 2. Why it produced nothing

`core_vol::VolEngine::har_1e9` opens with

```rust
let longest = HAR_WINDOWS[HAR_WINDOWS.len() - 1] as u64;   // 1440
if self.minutes < longest { return None; }
```

`minutes` counts minute rolls **since construction**. It is not in
`vrp-state.tsv`, which carries only `V`, `P` (fitted pairs), `Q` (QLIKE)
`K` and `C`. Neither is the `ret_1e9` ring or `sum_sq`.

The restart lane fires **five times a UTC day** — 00:10 / 08:30 / 16:05
/ 20:15 / 21:15 — and the longest gap between slots is 08:30→16:05,
**7 h 35 m = 455 minutes**. 1440 is unreachable. The member booted at
21:16:44Z and had 163 minutes at the decision.

**The member could never have traded.** Not on this expiry — on any.

The seed gives the member its fitted LINE (90 pairs, ≥ `MIN_PAIRS` 60)
but never its current **x**. `x = ln(har_now)`, and `har_now` needs the
24 h rolling window that nothing preserves.

This is the same defect class as V8a, where kill criterion 3 could never
arm because its window was process-local against the same restart
cadence. V8a fixed the pairs and the QLIKE ring. It did not fix the
HAR's own rolling returns, and nothing tested the difference because no
test restarts an engine mid-warm-up.

## 3. Operator rulings (2026-09-11)

1. **Both sources, freshest wins** — seed the window from `candles.db`
   AND persist it across restarts.
2. **Bump the version** — `VRP_STATE_VERSION` 1 → 2, and refuse a file
   the running binary does not fully understand.
3. **Fix this, then sweep** — land the warm-up fix first, then audit
   every member for state that assumes an uptime longer than the gap
   between restart slots.

## 4. What gets stored, and why it is RETURNS

`on_minute_close(px)` derives a return and pushes it. Persisting the
derived **returns** — which `ret_1e9` already holds — means:

* no new array in a hot `#[repr(C, align(64))]` struct; the only new
  field is `last_min_ts_ms: u64`;
* `sum_sq` is never read from a file. Restore replays each return
  through the same push the live path uses, so the three accumulators,
  the ring and `minutes` cannot disagree with each other by
  construction. A corrupt file can make the window wrong; it cannot make
  it *inconsistent*.

The worker computes its returns with `core_regime::math::ret_bps_1e9`'s
integer law, which `claude_worker.vol_ref` already mirrors and
`tests/fixtures/vol/parity-1` already pins. Same law, both sides.

## 5. The merge

Each source yields chronological `(ts_ms, r_1e9)`.

1. Union by `ts_ms`. On collision **state wins** — it is the engine's
   own observation of the live tape; the candle is a REST-derived
   aggregate of the same minute.
2. Sort ascending, take the longest suffix contiguous at 60 000 ms,
   cap at `MINUTE_RING`, replay.
3. Report it. `warm minutes=N from_seed=A from_state=B short_by=M`.

Honest note, to be carried in the code: the two sources are different
derivations of the same quantity — capture mid-roll versus REST candle
close — so a union splices two series. A handful of splice points move
a 1440-minute `Σ r²` by far less than a cold engine that cannot trade at
all, and the boot tell names the split so the operator can see it.

Neither source alone is enough, which is why the ruling is both: state
covers since the last boot (as little as 20 minutes), the seed covers
the day up to the worker's last hourly refresh (~1 h stale). Together
they cover 24 h.

## 6. Work items

| # | Crate / file | What |
|---|---|---|
| **W1** | `core-vol` | Split `on_minute_close` into derive + `push_return`; add `pub fn seed_return(r_1e9)`, `last_min_ts_ms` (+ getter/setter), `ret_chrono(i)`, `warm_minutes()`. Tests: a seeded ring equals a replayed one exactly. |
| **W2** | `strategy-vrp` | `R <ts_ms> <r_1e9>` rows in `render_state`/`restore_state`; `VRP_STATE_VERSION = 2` (accept ≤ 2, refuse > 2); the §5 merge; the warm-up boot tell. Tests: restart mid-warm-up stays warm; a v1 file still loads; a v3 file is refused. |
| **W3** | `core-config::vrp`, `cli::vrp_boot` | Seed file gains a `V 2` line and tagged rows; parse `R` rows; carry them into the member; boot tell. |
| **W4** | `claude-worker` | `vrp_seed.py` cuts the trailing ≥ 1441 `deribit:BTC-PERPETUAL` 1m closes from `candles.db` into `R` rows. `candles.db` holds 264,513 such rows, 2026-03-11 → now. |
| **W5** | deploy | Rebuild, re-cut the seed, restart. **Binary first, then the seed** — an old binary refuses a new seed, which is the stale-writer property ruling 2 asked for. |

## 7. Acceptance

* A restart at any point leaves `engine_vrp_no_bounds_total` flat.
* The boot tell names the warm-up state on every boot. Today the member
  booted cold five times in a row and said nothing; `no_bounds` was the
  only tell, and it is indistinguishable from every other cause.
* The 00:00Z decision on the 12th produces an entry or an honest hold —
  `engine_vrp_holds_total`, not `no_bounds`.

## 8. Known limitation, deliberately not fixed here

`on_tick` publishes a close on the minute ROLL, so a multi-minute gap in
the perp tape produces ONE close, not the several the wall clock passed.
`minutes` is therefore a count of observed rolls, not of wall minutes.
That predates this plan and changes the measured edge's input if
touched, so it stays — recorded here so the next reader does not mistake
it for part of the fix.
