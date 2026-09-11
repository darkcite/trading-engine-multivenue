<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright 2026 Anton (darkcite) -->

# VRP X1–X5 — the member has to actually get filled

**Status:** specced 2026-09-11, nothing implemented.
**Trigger:** the first live campaign did not execute in the model.

## 1. What the replay said

`audit-pnl` over the campaign's own run:

```
strategy 1 (vrp): orders=4 fills=1 trades=1 net=162.63
                  (realized=0.0 fees=0.0 markout=162.63)
                  max_dd=275.04 canceled_end=1 ioc_canceled=1 ttl_expired=1
  deribit:BTC-PERPETUAL: fills=1 pos=975320 realized=0.0
```

One fill, and it is the perp. **No row for the option at all.** The
$162.63 is mark-out on a long 0.975 BTC hedge that was opened and never
closed — a hedge with nothing to hedge. `realized` is zero because
nothing round-tripped.

The member submits IoC **at the mark** (`px = self.last_mark.px_usd_1e6`)
for the entry and the unwind alike. This run registered NO D-7 mark-fill
syms — 52,272 option quote ticks were converted, so every option sym had
a real tick lane and was judged by strict-cross. A mid-priced IoC does
not cross a two-sided book, so it cancelled at its activation touch, and
the second one expired without ever seeing fill evidence.

This is doctrine clause 1 being tested for the first time, and failing:
*"a divergence between this member's position and the harness's is a real
defect wearing the shape of a rounding difference."* It is not a rounding
difference. The member believes it is short a call and hedged. The
harness says what actually went on was an unhedged long BTC.

D-7 reads "no options book exists in the capture, so every option fill is
a model fill at mark ± half-spread". That premise was true when Deribit
options arrived as summaries only. **It is no longer true**, and the
member is the last thing still relying on it.

## 2. What the book actually costs (measured, not assumed)

`claude-worker/tools_opt_spread.py` over `run-1789085487143512000`
(00:11→03:10Z, the 11SEP chain at ~7 h to expiry — close to the designed
`E−τ` entry), spot ≈ $77,000:

| contract | two-sided quotes | crossed spread | mid | cross cost/side |
|---|---|---|---|---|
| BTC-11SEP26-75000-C | 405 | 10.8 % | 0.02400 | $96.25 |
| BTC-11SEP26-76500-C | 1808 | 24.0 % | 0.00575 | $57.75 |
| **BTC-11SEP26-77000-C (ATM)** | **3996** | **18.9 %** | **0.00235** | **$19.25** |
| BTC-11SEP26-77500-C | 3658 | 40.0 % | 0.00075 | $11.55 |
| BTC-11SEP26-78000-C | 2195 | 66.7 % | 0.00030 | $7.70 |

**The number that matters is 18.9 %** — the selection law picks nearest
strike to spot, so ATM is what we trade. Crossing costs ~9.5 % of premium
per side.

Against a ~$181 ATM premium, a round trip is roughly:

* spread, both sides: **~$38.50**
* capped Deribit fee, both sides: `min(0.0003 × 77,000, 0.125 × 181)` =
  **~$22.63 each**, ~$45 the pair
* **total ≈ $84 on $181 of premium — 46 %.**

The wide relative spreads on cheap strikes are a tick-size artifact (a
0.0002 mid on a 0.0001 tick is 50 % by construction); the ATM figure is
the real one and it is not an artifact.

### 2.1 This is off the top of the ladder the edge was validated against

`--option-spread-frac` exists precisely to ask "does the edge survive a
crossed spread", and the edge spec's ladder runs 0 / 2 / 5 / 10 %. The
measured ATM book is **18.9 %** — roughly double the worst rung ever
tested. Every backtest number the lane has rests on an assumption the
live book does not honour.

**So X0 comes before everything else in this document.**

## 3. X0 — does the edge survive the real spread? (BLOCKING)

Re-run the edge measurement at `--option-spread-frac 189000` (18.9 %
crossed) over the existing evidence windows, beside the 0 rung.

* If the VRP still clears frictions: proceed to X1–X4.
* If it does not: **the strategy does not work as specified** and the
  question becomes a different one — a wider `θ` so only richer expiries
  qualify, a strike rule that trades spread against moneyness rather than
  taking ATM by default, a longer tenor where premium is larger relative
  to a fixed tick, or dropping the lane.

Nothing below is worth building until X0 answers. Writing crossing logic
for a strategy whose edge the crossing destroys is how you get a system
that loses money reliably instead of not trading at all.

### 3.1 X0 RAN, 2026-09-11 — it does not clear

Full numbers in the vault: `docs/research/vrp-x0-spread-findings-2026-09-11.md`.

The ladder's `spread_frac` is MID-TO-TOUCH, so 18.9 % crossed = the
**0.0945** rung, and the published 0.10 rung was already a 20 %-crossed
book. Re-running `tools_vrp_v0_exit.py` with that rung added (BTC 8 h,
maker hedge, n = 71):

| exit | @0 % | t | @9.45 % | t |
|---|--:|--:|--:|--:|
| `settle` | +9.048 | +3.47 | **+3.658** | **+1.67** |
| `fair_eps` — **what the member does** | +8.318 | +3.25 | **−2.198** | −0.79 |

And θ does not rescue it: swept 0.10 → 0.40, the mean rises and `n` falls
in step, so **t stays 1.5–1.9 and never reaches 2**, while `fair_eps`
stays negative at every θ.

**Verdict.** Two separable conclusions:

1. **The E−ε unwind is wrong for this book and must go**, independent of
   everything else. It crosses the option spread twice where `settle`
   crosses once; at a 19 % book the second crossing costs ~6 bps of spot,
   more than the whole VRP. The member already has the rung
   (`maybe_settle`) — make it the primary exit and flatten only the perp
   at E−ε. This is unambiguous and cheap.
2. **The lane is NOT validated at the real spread.** Even with the right
   exit: +3.658 bps, t +1.67, median −0.49, hit rate 46 % — a positive
   mean carried by a tail, on n = 71. Not distinguishable from zero.

So X1–X4 below are **not authorised by X0**. The next probe worth running
is **strike selection** — the cost that kills this varies 2× across
strikes (10.8 % at 75000-C against 18.9 % ATM) and the selection law takes
ATM by reflex. That is the only lever found with real headroom. θ is not
one, and more execution engineering is not one either.

## 4. X1 — the member cannot retry what it cannot observe

Anton's question was: *if the order is not filled, do we resend with an
amended price until it fills?*

**Not until the member can see fills.** Doctrine clause 1 is
"SUBMIT is the position event" — `on_fill` is a documented no-op because
`PaperDispatcher::try_next_fill` always returns `None`. A retry loop on
top of that assumption is worse than the bug it fixes: the member would
count every resend as a fill and believe it is short N contracts when it
is short at most one. The book would diverge without limit.

So the enabling change is fill feedback:

* Give the paper path a **model fill** that uses THE SAME law the harness
  uses — `backtest::fill`'s strict-cross against the live book, not a
  second implementation. One law, both sides, so the member and the
  replay cannot disagree again.
* `VrpStrategy::on_fill` stops being a no-op; positions come from fills.
* Doctrine clause 1 is retired and the header says so.

This is the change that makes the whole lane honest, and it is a
precondition for X2 and X3.

## 5. X2 — price per leg, by urgency not by habit

One price rule for every order is the actual defect. The three legs have
different economics:

| leg | economics | rule |
|---|---|---|
| **entry** | DISCRETIONARY. We trade only because `iv > hi`. Every tick given away is edge given away, and there is a price at which the trade is no longer worth doing. A missed entry costs nothing. | Limit at a price that still clears `θ` after frictions. Do not chase. Abandon at the decision deadline. |
| **hedge** | RISK-REDUCING. The perp is tight and deep; carrying unhedged delta costs far more than a tick. | Marketable from the start — cross the touch. |
| **unwind** | OBLIGATORY. The position must be flat before expiry; we are not price-sensitive, we are deadline-sensitive. | Escalate toward and through the touch as `E` approaches. |

The entry rule is the one worth stating precisely: **the limit is derived
from the edge, not from the touch.** We know `hi` (the forecast + θ). The
worst price at which the trade still clears the gate is a function of it.
Sell there or not at all — that is what "only trade when it is rich"
actually means once frictions exist.

## 6. X3 — the retry ladder (only after X1)

For the legs where retrying is right (hedge, unwind), and never for the
entry beyond its edge-derived limit:

1. Attempt at the leg's rule price.
2. On a cancel/no-fill, re-price toward the touch by one step and resend,
   at most `N` times, with a floor/ceiling that is the leg's worst
   acceptable price.
3. Count every rung: `engine_vrp_retries_total`,
   `engine_vrp_unfilled_total`. An unfilled entry is a HOLD, not a
   failure; an unfilled unwind at `E−ε` is an ALERT — that is a position
   going into settlement unhedged.
4. Never resend while an attempt is in flight. With IoC there is no
   resting order, so "in flight" means "submitted and not yet judged",
   which X1's fill feedback is what makes knowable.

## 7. X4 — what the harness must stop assuming

* `mark_fill_syms` is empty whenever options carry a tick lane, so the
  D-7 path is now dead code in production runs. Keep it for summary-only
  captures, but the report must say which law applied — today it printed
  nothing and the silence was the only clue.
* `audit-pnl` should surface `fills=0` on a strategy that submitted
  orders as something louder than a number in a line. A member that
  submits four orders and fills one is the headline, not a detail.

## 8. Files

| file | change |
|---|---|
| `crates/clob-dispatcher/src/lib.rs` | X1: a paper dispatcher that models fills instead of returning `None`. |
| `crates/cli/src/backtest/fill.rs` | X1: the fill law shared with the live paper path rather than only the replay. |
| `crates/strategy-vrp/src/lib.rs` | X1 `on_fill` real; X2 per-leg pricing; X3 the ladder; header doctrine rewritten. |
| `crates/strategy-core/src/lib.rs` | X3 counters. |
| `crates/cli/src/paper.rs` | X3 metrics. |
| `crates/cli/src/audit_pnl.rs` | X4 reporting. |
| `docs/risk-policy.md` | the unfilled-unwind alert. |

## 9. Acceptance

* A replay of a campaign shows the member's position and the harness's
  agreeing leg for leg — the V8 shadow reconciliation, which has never
  yet passed and which this is really about.
* `audit-pnl` shows the option leg filling, with `realized` non-zero.
* X0's answer is recorded here, whichever way it goes.
