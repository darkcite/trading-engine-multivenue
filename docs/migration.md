# Migration notes

This document tracks **format and schema migrations** — places where a bump
to a wire-format version, an on-disk file layout, or a config key has
ripple effects the operator needs to know about.

Each entry is atomic: one version bump per section. Do not batch.

## 2026-09-11 — the VRP campaign exits at SETTLEMENT, not at E−ε (Y1)

**What changed**

- The E−ε unwind is gone. `maybe_exit` is replaced by `maybe_freeze_hedge`,
  which only stops NEW hedging inside ε; it places no order and does not end
  the campaign.
- `maybe_settle` at `E` is now the only terminal rung for a planned
  campaign: it books the option's intrinsic (ITM) or zeroes it (OTM),
  flattens the perp, folds the hold into the forecast and closes the
  campaign.
- `flatten()` survives for the **regime hard exit only** — the risk path,
  where crossing the spread to get out is the correct price.
- `flatten_pending` → `hedge_frozen`.

**Why**

The E−ε unwind bought the option back five minutes before expiry, crossing
the option spread a SECOND time. Lane plan §3.4 ruled that acceptable —
*"economically near-identical to cash settlement"* — and at the 0 % spread
it assumed, it was: −0.73 bps. V2a measured the real book the next day
(near-ATM 4–12 h calls, **median 25 % crossed**) and that gap became the
whole edge. Re-measured 2026-09-11 at the ATM rung, per expiry:

```
  settle    +3.658 bps  (t +1.67)
  fair_eps  −2.198 bps  (t −0.79)   <- what this rung did
```

Holding to expiry crosses the spread once. The member was built to a ruling
its own evidence had superseded, and it shipped the negative arm.

**Ripple effects**

- **`engine_vrp_exits_total` changes meaning.** It was "the planned E−ε
  unwind"; it is now "the regime RISK exit" and should read 0 in normal
  operation. A non-zero value is a regime slam, not a campaign ending.
  Campaign endings are `settlements` / `settled_itm` / `settled_otm`.
- The member now carries its option position THROUGH expiry, so the
  settlement rungs are exercised on every campaign rather than only when
  the unwind failed. `settled_unpriced` becoming non-zero is now a real
  operational signal (the contract rolled off the chain before settling).
- No on-disk format changes; `VRP_STATE_VERSION` stays 3.

**Migration steps**

1. Rebuild and restart. Re-baseline any alert on `engine_vrp_exits_total`.

**Rollback**

- Revert the commit. No format or config change is involved.

## 2026-09-11 — `vrp-state.tsv` v2 → v3: a decision is spent once (W6–W7)

**What changed**

- `VRP_STATE_VERSION` 2 → **3**. The `C` row gained an eighth field,
  `entry_done`. A v2 row (seven fields) still loads, with the flag derived
  from `opt_qty != 0` exactly as v2 meant it.
- `decide` now refuses a LATE decision: the band is `[E−τ, E−τ+selection]`,
  mirroring the selection band that ends at `E−τ`. A campaign reached past
  the deadline is SPENT — marked decided, counted, never retried.
- New counter and metric: `engine_vrp_decisions_late_total`.
- **W7:** `scripts/candles-cycle.sh` now cuts `vrp-seed.tsv` hourly, and
  `vrp_seed seed-out` gained `--vrp <path>` so the descriptor and tenor come
  from `vrp.toml` rather than a second copy in the cron.

**Why — this one traded**

`entry_done` was derived from the position, so a campaign that decided and
HELD left nothing to derive it from and the next boot decided it again. At
06:01Z on 2026-09-11 a restart re-reached the `BTC-11SEP26` campaign and
entered: short one $76,500 call, hedged long 0.975 BTC of perp, **1 h 56 m
before an 08:00Z expiry, gated and sized by an 8 h variance forecast.**

`τ` is the horizon, not a start line. `bounds` forecasts variance over a
τ-long hold and the edge was measured on one, so an entry taken hours late
is a different trade wearing the same gate. Both halves are needed: the
persisted flag stops a restart re-deciding, the deadline stops a boot that
first reaches the campaign late from entering at all.

This was called out as "minor, not harmful" when the re-decide was first
noticed on 2026-09-10. That was wrong, and it put on a live position.

**Ripple effects**

- A v2 binary refuses a v3 state file — the point of the bump. A v3 binary
  reads v1, v2 and v3.
- `engine_vrp_decisions_late_total > 0` means a restart straddled a decision
  instant and that day's campaign was skipped. Expected to stay 0 now that
  the day-boundary restart moved to 00:10.
- The hourly seed cut bounds seed staleness to an hour. It matters only when
  `vrp-state.tsv` is lost — with the state file intact the window carries
  itself across restarts — but in that case a stale seed boots the member
  WARM on old returns, and nothing refuses it.

**Migration steps**

1. Rebuild and install; restart. The next state write is v3.
2. No action for the seed — the hourly cycle now keeps it current.

**Rollback**

- Revert and delete `vrp-state.tsv` (a v2 binary refuses a v3 file). The
  member then boots cold and re-seeds from `vrp-seed.tsv`.

## 2026-09-11 — `vrp-state.tsv` v1 → v2 and `vrp-seed.tsv` v1 → v2 (W1–W5)

**What changed**

- `VRP_STATE_VERSION` 1 → **2**. New `R <min_ts_ms> <r_1e9>` rows carry the
  HAR's rolling minute window. A v2 reader accepts v1 (no `R` rows = a cold
  window, which is what v1 always meant) and REFUSES anything above 2.
- `vrp-seed.tsv` is now TAGGED and versioned: `V 2`, then `P <expiry_ts_ms>
  <x_1e9> <y_1e9>`, then `R <min_ts_ms> <r_1e9>`. A v1 seed — bare triples —
  still parses.
- `core-vol` gains `seed_return`, `on_minute_close_at`, `ret_chrono`,
  `n_returns`, `last_min_ts_ms`, `is_warm` and the public
  `HAR_WARM_MINUTES`.
- `cli::vrp_boot` reconciles the two window sources and hands the member one
  contiguous series; `VrpBoot` gains `window`, `window_from_seed`,
  `window_from_state`.
- **The member's state epoch now moves on every minute close.** It has to:
  otherwise the `R` rows only reach disk when a campaign happens to move the
  epoch — a few times a day — and the whole fix is inert. One ~40 KB
  tmp+rename per minute, on the observability cadence.

**Why — the member could never have traded**

`core_vol::har_1e9` returns `None` while `minutes < 1440`, `minutes` counted
from process start, and nothing persisted it. The restart lane fires five
times a UTC day (00:10 / 08:30 / 16:05 / 20:15 / 21:15) with a longest gap
of 7 h 35 m = **455 minutes**. 1440 was unreachable.

The first live campaign proved it. At 00:00:00Z on 2026-09-11 the member
selected the right instrument — `vrp-state.tsv` carried `C … 76500000000 0`,
the $76,500 call expiring 08:00Z — reached the decision with a fresh mark and
a fresh IV, and produced `decisions=1 entries=0 holds=0 **no_bounds=1**`.
Everything worked except the one thing that had never been able to work.

The seed gave the member its fitted LINE (90 pairs) but never its current
**x**, because `x = ln(har_now)` and `har_now` needs the 24 h window.

Same defect class as V8a, where kill criterion 3 could never arm for the same
reason. V8a fixed the pairs and the QLIKE ring and did not fix this.

**Ripple effects**

- **A v1 binary refuses a v2 seed** (`parse_seed_row` wants exactly three
  fields). That is the point of the bump, and it dictates the deploy order:
  **binary first, then re-cut the seed.** Doing it the other way round takes
  the engine down on its next restart.
- A v1 `vrp-state.tsv` upgrades silently on the first write.
- `docs/vrp-warmup-plan.md` carries the full design and the one known
  limitation left in place (a multi-minute gap in the perp tape publishes one
  close, not several, so `minutes` counts observed rolls rather than wall
  minutes — predates this change and alters the measured edge's input if
  touched).

**Migration steps**

1. Rebuild and install the binary.
2. Re-cut the seed: `python -m claude_worker.vrp_seed seed-out --db
   ~/multivenue/worker/candles.db --descriptor deribit:BTC-PERPETUAL --out
   ~/multivenue/vrp-seed.tsv`. It now reports window minutes and holes, and
   warns when the window is under 1440.
3. Restart. The boot tell is `vrp: forecast WARM minutes=… from_seed=…
   from_state=…`, or `vrp: forecast COLD … short_by=…`.

**Rollback**

- Revert the commit AND re-cut the seed with the old cutter — a v2 seed left
  in place will refuse a v1 binary's boot. `vrp-state.tsv` can be deleted
  instead; the member then boots cold, which is where it was before.

## 2026-09-10 — audit-pnl SETTLES expired options instead of marking them out (VX-A)

**What changed**

- `backtest::fill::FillEngine` gains `set_opt_settle(sym, value_1e6)` and an
  expiry settlement rung. When the replay clock reaches a settleable
  contract's expiry the mark is PINNED to its European cash value and stops
  moving; whatever position is still open at the end of the replay is closed
  at that value, charged the venue's SETTLEMENT rate (`fee_for` already
  switched on the instant — that part shipped with VX).
- The D-7 assumed half-spread is not charged on a settled sym. A cash
  settlement crosses no book.
- `audit-pnl` computes the value from `options-manifest.tsv` (strike, right)
  and the last underlying the instrument printed **at or before its own
  expiry**, wall-stamped through the same `run.epoch_ns + (raw_ts −
  ts_first)` rebase the merge uses.
- `ModelOutcome` and the audit's per-strategy JSON gain `opt_settled`.

**Why**

`audit-pnl` replays intents and never closed a position on its own, so a
contract that reached expiry still held stayed OPEN and marked out at the
last mid the tape carried. Deribit removes an expired instrument from the
chain, so that mid is a live option's price, not a dead one's.

For the VRP member's ordinary outcome — a short call that expires
worthless — that booked the whole premium as a loss it never took. The
member zeroes such a position in its own book with NO order, deliberately
(`maybe_settle`'s OTM branch: a zero-priced order is a fiction and a
mark-priced one would book value that expired), so nothing in the intent
log could ever have told the harness. `settled_otm` and
`settled_unpriced` were the only tells, and they are counters on the
engine, not inputs to the report.

**Ripple effects**

- Any window containing an option expiry with a position open at that
  instant now reports a DIFFERENT, correct net. Windows with no such expiry
  are byte-identical: the rung is inert without a settlement value, and no
  value is supplied for a contract whose expiry the window never reaches.
- The per-strategy JSON gains a key. Readers that match on an exact key set
  need updating; `opt_settled > 0` obliges the reader to look, because it
  means the intents did not close a contract that expired.
- The settlement table is PRINTED with the index it used and how many
  seconds before expiry that index was — the same obligation the D-7 mark
  law carries. Measured on the 2026-09-10 00:01→08:31Z run: 32 contracts
  reached expiry in-window, index lag **20–21 s**.

**Migration steps**

1. Rebuild. A report regenerated over a window containing an expiry will
   differ from one produced before this commit; that is the fix.

**Rollback**

- Revert the commit. No on-disk format, config key or wire format is
  involved — `engine-orders.pmlr`, `options-manifest.tsv` and
  `vrp-state.tsv` are untouched — and the engine loop never loads this
  code, so a running engine is unaffected either way.

## 2026-09-10 — `engine_vrp_no_selection_total` counts expiries, not records

**What changed**

- `strategy-vrp`'s selection law now evaluates the TIME window before the
  currency and right filters, and increments `no_selection` only when an
  expiry was inside the window and nothing in the chain was tradeable for
  this member.
- Previously every option record seen while no expiry was due incremented
  the counter.

**Why — the metric contradicted its own definition**

The counter is documented as "expiries where no instrument passed the
selection law". For ~23 h 50 m of every day no expiry is inside the
selection window at all, which is the member working, not the member
failing. On the live engine it reached **1537 in the first 20 s after
boot** — a rate set by the Deribit summary feed, not by anything about
the chain. An operator watching that has to learn to ignore it, and
ignoring it is the same as not having it: the one case the counter
exists to surface — the chain rolled without our currency, or carries no
calls at that expiry — would have been invisible inside the noise.

**Ripple effects**

- `engine_vrp_no_selection_total` is a COUNTER whose meaning changed
  between builds. Anything comparing across the 2026-09-10 boundary sees
  it drop to 0 and stay there; that is the fix, not a stall. Treat
  pre-fix samples as a different series.
- Nothing else reads it — no gate, no kill criterion, no state file.
- Selection behaviour is byte-identical: the same instrument is chosen by
  the same tie-breaks. Only the ORDER of the filters moved, and the time
  filter is a total predicate on `expiry_ns`/`lead`, so no row's verdict
  changes.

**Migration steps**

1. Rebuild and restart the engine (`pkill -TERM -f "multivenue-engine run"`;
   launchd `KeepAlive` relaunches it).
2. Re-baseline any alert on `engine_vrp_no_selection_total`. The expected
   steady-state value is `0`.

**Rollback**

- Revert the commit; the counter returns to per-record counting. No
  on-disk format, config key or wire format is involved, and
  `vrp-state.tsv` is untouched (`VRP_STATE_VERSION` stays `1`).

## 2026-09-10 — the VRP chain is restricted to ONE currency (V8b)

**What changed**
- `cli::vrp_boot::build_registry` now takes the hedge DESCRIPTOR and
  admits only options of that instrument's currency; every other row is
  skipped and counted in the existing `chain_rows_refused` boot tell.
- `strategy-vrp`'s selection law re-checks it: a row whose
  `underlying_sym` is not the member's own hedge leg is never a
  candidate.

**Why — this was a live defect, not a hardening**
`~/multivenue/universe.toml` has
`[deribit] options_underlyings = ["BTC", "ETH"]`, so boot discovery
hands the engine BOTH ladders. The selection law's first tie-break is
nearest expiry, and its second is nearest strike to the underlying — so
an ETH call expiring sooner than the BTC one would have been selected
and then delta-hedged with `deribit:BTC-PERPETUAL`. That is a
cross-asset naked position, and **nothing else in the member would have
caught it**: every other check is about size, staleness or timing, none
about what the instrument is. Deribit's BTC and ETH dailies happen to
share an 08:00 UTC expiry today, which would have masked it — the law
must not depend on that coincidence.

It also matters mechanically: `OptRegistry` holds 128 rows, and a
two-currency chain can exceed that, silently dropping instruments.

**Ripple effects**
- The evidence covers BTC only (edge spec §7: no ETH), so this is also
  where that restriction is enforced rather than assumed. Trading the
  ETH chain means pointing `vrp.toml`'s two descriptors at ETH — one
  edit, and the registry follows.
- A `vrp.toml` whose hedge descriptor has no option chain now fails the
  boot with the currency named, instead of building an empty table.
- Boot tell `chain_rows_refused` now includes the other currency's rows;
  a BTC+ETH ladder reports roughly half the chain refused, which is
  correct and expected.

**Operator action**
None.

## 2026-09-10 — `vrp-state.tsv`: the VRP member's state survives a restart (V8a)

**What changed**
- New engine-written file `~/multivenue/vrp-state.tsv`, read at boot and
  rewritten whenever the member's state epoch moves. It carries three
  things that outlive a process: the fitted `(x, y)` pairs with the
  expiry each came from, the **QLIKE window** kill criterion 3 is
  measured over, and any **open campaign**.
- `core-vol` gains `seed_pair_at`, `seed_qlike`, `pair_at`, `qlike_at`,
  `n_qlike` and `arm_hold_at`; pairs now carry an expiry stamp. The
  parity fixture and `vol_ref.py` are untouched — `seed_pair` and
  `arm_hold` still exist and stamp `0`.
- `strategy-vrp` gains `render_state` / `restore_state` / `state_epoch`,
  and holds the selected option's strike and right itself.
- `strategy-core` gains `StrategyCounters::{vrp_state_epoch,
  render_vrp_state}` (defaulted), forwarded by `strategy-set`. The cli
  writes the file on the same 5 s cadence as the metrics mirror, and
  ONLY when the epoch moved.

**Why**
Two gaps, both real:
- **Kill criterion 3 could never arm.** It is measured over sixty
  settled expiries — twenty days at an 8 h campaign — and the window
  started empty at every boot, against five scheduled restarts a day.
  It now accumulates across them, and an armed halt is no longer
  cleared by a restart (which `docs/risk-policy.md` forbids as
  auto-resume).
- **A restart inside a hold orphaned the position.** The next boot knew
  nothing about it: never hedged it again, never settled it.

**Ripple effects**
- **The campaign is keyed by `(expiry_ns, strike, right)`, never by
  `SymbolId`.** Deribit option ordinals reshuffle at every boot, so a
  persisted symbol would name a different instrument tomorrow.
- **Two files, one writer each.** `vrp-seed.tsv` is the WORKER's
  bootstrap cut and the engine only reads it. `vrp-state.tsv` is the
  ENGINE's own history. When both exist the state file wins.
- A state file that is present and malformed **refuses the boot** — a
  state file the engine cannot read exactly is a position nobody is
  tracking. An absent one is a normal first boot.
- New counter `engine_vrp_settled_unpriced_total`. **Non-zero is a
  reconciliation item, not a routine counter:** an in-the-money expiry
  whose contract had already rolled off the boot chain, so the member
  closed the position out of its own book without being able to price
  it. There is no symbol to submit against and the persisted one names
  a different instrument on the new boot, so booking it would be worse
  than counting it.
- Writes are atomic (temp file + rename) and a failed write is logged,
  never fatal: taking a running engine down over a full disk while it
  holds a position is worse than losing the ability to restore one.

**Operator action**
- Nothing to install — the engine creates the file.
- A boot logging `vrp: kill criterion 3 was ARMED before this restart`
  means the member will not enter. Investigate before deleting the state
  file; deleting it is what clears the halt, and it also discards the
  QLIKE window that armed it.

## 2026-09-10 — Deribit position caps are in COINS (operator amendment)

**What changed**
- New shared table `strategy_core::{VenueCaps, CAPS_BASE, CAPS_DERIBIT,
  caps_for_venue, caps_for_sym}`. Deribit: **1 whole coin** per order and
  per symbol, **$250 000** book total. Every other venue keeps the base
  tier ($10k / $20k / $100k), unchanged.
- `strategy-vrp` and `strategy-icdp` both read that table.
  `strategy_icdp::{CAP_LEG_1E6, CAP_SYM_1E6, CAP_TABLE_1E6}` are now
  re-exports of `CAPS_BASE`'s fields, so the two members cannot drift.
- `vrp.toml.example` size returns to **one contract** (`qty_1e6 =
  1000000`) with `band_qty_1e6 = 50000` — the edge spec's measured
  configuration, now exactly on the cap rather than eight times over it.

**Why a different unit for one venue**
A Deribit inverse contract IS one whole coin: its size is a coin count
and its dollar value moves with the index. A dollar cap therefore shrinks
the permitted size as the coin rises, and the member starts refusing at a
price nobody chose. A coin cap is the venue's own unit and holds at any
index.

**Ripple effects**
- `0` in a unit means "this member cannot express this venue's cap",
  never "unlimited". `strategy-icdp` sizes in dollars, so it now REFUSES
  a Deribit instrument at `configure` instead of reading Deribit's
  `leg_usd_1e6 == 0` as no limit. No behaviour change today — `icdp.toml`
  carries no Deribit instrument — but a future one is a boot refusal
  with a message, not a silent unbounded order.
- **The VM clamp and the ruleset validator are deliberately NOT
  amended.** `strategy_vm::POLICY_SINGLE_ORDER_CAP_1E6` and
  `ingress_ai::RULE_*_MAX_RISK_1E6` keep $10k / $20k / $100k; they are an
  independent tighten-only layer over AI-pushed rows, with hash-pinned
  rulesets and a proptest depending on those numbers. An AI-pushed VM row
  on a Deribit option is still capped at $10 000.
- Reports and gates over non-Deribit venues are byte-identical.

**Operator action**
None beyond the `vrp.toml` you install at V8 — the example already
carries the amended size.

## 2026-09-10 — European cash settlement at expiry (VRP VX, ruling O-D4)

**What changed**
- `strategy-vrp` gains the settle rung. On the first callback — tick OR
  option summary — with `wall_ns >= expiry_ns` while still holding, the
  member books the European payoff `max(0, S − K)` (call; mirrored for a
  put) at the freshest index it has, flattens the hedge, folds the hold
  into the forecast and ends the campaign. New counters `settled_itm` /
  `settled_otm` (`engine_vrp_settled_itm_total`, `_otm_total`).
- **An OTM expiry emits no order at all.** The option is worth nothing
  and the venue charges nothing for it, so there is no fill to price: a
  zero-priced order would be a fiction and a mark-priced one would book
  value that expired.
- `ModelParams::opt_fee` gains `settle_index_tenth_bps` (Deribit 15 =
  0.00015 = 1.5 bps). `FillEngine` gains `set_opt_expiry`, and a fill on
  an option sym AT OR AFTER its expiry is priced as a SETTLEMENT:
  `min(0.00015 × index, 0.125 × settlement value)` instead of the trade
  law's `min(0.0003 × index, 0.125 × premium)`. Both harness loaders
  populate the expiry map from the run's own registry — `backtest` keyed
  on the remapped sym (a chain that rolled across boots is one
  instrument), `audit-pnl` on the interned dense sym.
- New `fee_ceil_tenth_bps_1e12`, because the venue's settlement rate is
  exactly half its trade rate and half of 3 bps is not an integer number
  of bps. Rounding it to 1 or 2 would mis-state every expiry by a third.

**Why**
Operator ruling O-D4. The member's normal exit is E − ε, and V0(b)
measured that choice; VX is not an alternative exit but the last word on
a position that is still open at expiry — a submit ring that stayed
full, a data gap that swallowed the ε instant, an option lane that went
quiet. At expiry the instrument stops being tradeable and becomes cash,
so the member books cash.

**Ripple effects**
- **The settlement classifier is derived, not flagged.** No new wire
  field: the venue's own rule is "an option stops trading at expiry", so
  a fill's timestamp against the registry's `expiry_ns` IS the
  classification. A sym with no expiry recorded is never a settlement —
  the trade rate, never a guess.
- The settlement crossover sits at `value = index × 15/12500` = 0.12 %
  of the index, exactly half the trade crossover's 0.24 %.
- `--opt-fee <venue>:<index_bps>:<prem_bps>` carries the settlement leg
  with it (`settle_index_tenth_bps = index_bps × 5`, the venue's half
  relationship), so a fee sweep cannot leave the two disagreeing.
- Reports over roots with no option records are unchanged: `fee_for`
  cannot reach the settlement branch without an index AND an expiry.
- The member picks the FRESHER of its two index sources (the option
  record's forward, the underlying tick's mid) rather than preferring
  one. The option lane can go quiet for hours while the perp keeps
  printing, and settling against an eight-hour-old forward would book a
  payoff the option did not have. With neither, the settlement is
  DEFERRED to the next record and counted — a settlement priced off an
  invented number is worse than a late one.

**Operator action**
None.

## 2026-09-10 — **strategy slot 1 changes meaning: `ev` → `vrp`** (VRP V7)

**What changed**
- `crates/strategy-set` drops `strategy-ev` and composes
  `strategy-vrp` at slot 1. The slot NUMBER is unchanged and wire-stable:
  `Order.strategy_id` 1, `AiCmd::strategy_id` 1 and the enable bit
  `1 << 1 = 2` all mean exactly what they meant. **What changed is the
  member behind the number.**
- `mask_for_name`: `"ev"` is GONE (returns `None`); `"vrp"` = 2 and
  `"ai+vrp"` = 50 are new. `"all"` is unchanged at 127 and now composes
  vrp. `SLOT_VRP` / `BIT_VRP` are the primary constant names;
  `SLOT_EV` / `BIT_EV` remain as identical-valued aliases for readers of
  pre-boundary captures.
- New flag `--vrp <path>` on `run`. An ABSENT `~/multivenue/vrp.toml`
  leaves the member unconfigured and its bit unset — the `icdp.toml` law.
  `scripts/engine-wrapper.sh` accepts `STRATEGY=ai+vrp` and
  `STRATEGY=vrp` (still `--paper`, never `--live`).
- New metric family `engine_vrp_*` (11 counters + 4 gauges). The gauge
  `engine_strategy_ev_active` is **renamed** `engine_strategy_vrp_active`.
- `regime.toml`: the coded-member label key is now `[labels.vrp]`.
  **`[labels.ev]` refuses the boot** ("unknown coded member") rather than
  silently gating a different strategy with a label that was evidenced
  for the old one.
- Slot-1 display names follow: `audit-pnl`'s `strategy_label(1)` and
  `engine_snapshot::SLOT_NAMES[1]` both read `vrp`.
- **`crates/strategy-ev` stays**: workspace member, source, tests, and
  the standalone `--strategy ev` engine loop. Only the SET stopped
  referencing it.

**Why**
Operator ruling O-D2. The EV member was never enabled in the composed
mask and the VRP lane needed a slot; reusing the number keeps every wire
value and capture format stable while the member behind it changes.

**Ripple effects — the boundary**
- **A capture taken BEFORE 2026-09-10 carries EV rows under slot 1; one
  taken after carries VRP rows.** There is no way to tell from a row
  itself — the `strategy_id` byte is the same. That is exactly why this
  entry exists. `audit-pnl` labels every slot-1 row `vrp`, so a
  pre-boundary root's report names a member that did not produce it.
- Any dashboard panel or alert on `engine_strategy_ev_active` must move
  to `engine_strategy_vrp_active`.
- Any `~/multivenue/regime.toml` carrying `[labels.ev]` must be renamed
  to `[labels.vrp]` before the next boot, or the boot refuses. Under
  `[labels] require = 1` slot 1 still needs a label.

**Risk policy**
`docs/risk-policy.md` is updated in this same change: the three notional
caps now name `strategy-vrp` as an enforcement site (it gates on the
worst-case Δ = 1 hedge, refusing the entry rather than clamping the
hedge), and **kill criterion 3 is added to the kill-switch triggers** as
a member-scoped sticky halt — the VRP member stops entering for good
once a full trailing-60 window shows its forecast no longer beating
implied vol. The shipped `vrp.toml` size is 0.1 contracts, not the edge
spec's nominal 1.0: one contract's worst-case hedge is eight times the
$10 000 single-order cap.

**Operator action**
- Before the next boot with the VRP lane on: rename `[labels.ev]` →
  `[labels.vrp]` in `~/multivenue/regime.toml` if it exists.
- Repoint any `engine_strategy_ev_active` dashboard panel.
- Nothing else. Slot 1 has never been enabled in a live mask, so no
  running configuration changes until the V8 operator gate.

## 2026-09-10 — new crate `strategy-vrp` (VRP V6)

**What changed**
- New workspace member `crates/strategy-vrp`: the delta-hedged
  variance-risk-premium member. One short-dated Deribit option per daily
  expiry, entered only when the venue's implied vol leaves the band
  `core-vol`'s forecast opens around it, delta-hedged on the hour against
  the venue's own BS delta, exited at `E − ε`.
- `strategy-core` gains `VrpCounters` and a defaulted
  `StrategyCounters::vrp_counters()` accessor — defined there, like
  `IcdpCounters`, so the cli never names a member crate.
- The coin→USD denomination law moved from `cli::backtest::opt` to
  `opt_registry::coin_to_usd_1e6`, the crate that owns contract size.
  `cli::backtest::opt::coin_mark_to_usd_1e6` now delegates to it, so the
  live member and the harness cannot drift about what a premium is worth.
- New alloc gate `vrp_member_tick_and_opt_summary_are_zero_alloc`; the
  gate count rises 44 → 45.

**Why**
Written from the `Strategy` trait up rather than adapted from
`strategy-ev` (operator ruling O-D2): the two strategies share a slot
number and nothing else, and a copied file carries its previous
assumptions invisibly.

**The four doctrine clauses, verbatim in the crate header**
1. Paper has no fills — SUBMIT is the position event, and only V8's
   shadow reconciliation will ever check that assumption.
2. The book is USD-denominated, so the plain unadjusted BS delta is the
   correct hedge ratio; Deribit's account-level `DeltaTotal` carries an
   inverse-contract adjustment that must NOT be applied here.
3. Deribit option marks are coin-denominated; every USD number in the
   crate goes through `opt_registry::coin_to_usd_1e6`.
4. E1 is the mechanism and E2 the monetisation, so the QLIKE comparison
   is computed beside the bounds and exposed as a LIVE counter
   (`qlike_har_beats_iv`) — kill criterion 3 must be visible on a
   dashboard, not discoverable in a later report.

**Ripple effects**
- **Nothing is composed.** `strategy-vrp` is not referenced by
  `strategy-set`; a boot cannot be disturbed by this commit. V7 does the
  binding.
- The member's public state (`side`, positions, selected sym, counters)
  is read-only from outside — the cli reads counters through the
  `StrategyCounters` accessor.

**Operator action**
None. The member is unreachable until V7 binds it and V8 enables it.

## 2026-09-10 — `vrp-seed.tsv` + `--vrp-seed` (VRP V5)

**What changed**
- New worker module `claude_worker.vrp_seed`. Lane
  `python -m claude_worker.vrp_seed seed-out --db <candles.db>
  --descriptor <d> --out <vrp-seed.tsv>` cuts `expiry_ts_ms\tx_1e9\ty_1e9`
  rows for settled daily expiries out of `candles.db`, using the SAME
  integer law `core_vol` runs (`claude_worker.vol_ref`).
- `window_root.cut_run` gains an optional `vrp=<vrp.toml>` argument. When
  given alongside the existing `seed=(regime.toml, candles.db)` pair it
  writes the window's own `vrp-seed.tsv` beside `regime-seed.tsv` and
  `funding-seed.tsv` — seeded **as of the window's first instant**, never
  the wall clock.
- New engine module `cli::vrp_boot` and a `--vrp-seed <path>` flag on
  `run`, `backtest` and `audit-pnl`. `core_config::vrp` gains
  `parse_seed` / `load_seed` / `default_seed_path`.
- Boot tell: `vrp: seed applied pairs=N decisive=<bool> from <path>`, or
  `vrp: seed absent — the member holds until it has 60 pairs`.

**Why**
The forecast needs 60 settled `(x, y)` pairs before it will produce a
bound, and the engine restarts about three times a day. Without a seed
the member would be blind for two months after every restart.

**The two laws this file exists to enforce**
- **A cold boot is legal.** An absent default seed never refuses a boot;
  the member holds and the tell says so. A seed that is PRESENT and wrong
  — a float, a short row, a repeat, rows out of order — does refuse it: a
  seed the engine cannot read exactly is a fit nobody measured.
- **No lookahead.** A pair is `(x formed from the 1440 minutes BEFORE the
  entry instant, y realised over the hold that followed)`. A pair whose
  hold has not settled is never emitted, and `x` is formed by feeding a
  fresh engine only the minutes strictly before entry — the two windows
  are disjoint by construction, so no minute of a hold can reach its own
  regressor. Both are tested
  (`test_an_unsettled_hold_is_never_emitted`,
  `test_a_pair_is_cut_from_two_disjoint_windows`).

**Ripple effects**
- `vrp-seed.tsv` lives OUTSIDE git, beside `icdp.toml`, exactly like
  `regime-seed.tsv`. Fitted numbers go in the vault, never in the tree.
- Nothing consumes the loaded rows yet — the member arrives in V6 and is
  composed in V7. `--vrp-seed` on `run` today reads, validates and
  reports; it changes no behaviour.
- Schema-1, `AUDIT_PNL_VERSION` and `detail_version` are untouched.

**Operator action**
None yet. At V8, cut the live file with
`python -m claude_worker.vrp_seed seed-out --db ~/multivenue/candles.db
--descriptor deribit:BTC-PERPETUAL --out ~/multivenue/vrp-seed.tsv`.

## 2026-09-10 — new crate `core-vol` + `~/multivenue/vrp.toml` (VRP V4)

**What changed**
- New workspace member `crates/core-vol`: the integer volatility forecast.
  A rolling HAR over 1-minute returns (`[i64; 1536]` ring, three window
  sums rolled in O(1)), an OLS fit in log space over a 128-pair ring, and
  `bounds(tau_ns, theta_1e9)` returning the two `i64` an annualised
  `mark_iv_1e9` is compared against. `core_vol::fx` carries integer
  `log2/exp2/ln/exp` on 257-entry Q32 tables. No floats outside
  `#[cfg(test)]`; nothing allocates after `VolEngine::new`.
- New config namespace: `core_config::vrp` parses `~/multivenue/vrp.toml`
  (template: `vrp.toml.example`) — the same hand-written TOML SUBSET as
  `icdp.toml`, integers only, unknown/missing/duplicate keys fatal. Keys:
  `theta_1e9`, `tau_ns`, `epsilon_ns`, `selection_ns`, `rebalance_ns`,
  `qty_1e6`, `band_qty_1e6`, `underlying_descriptor`,
  `hedge_descriptor`. It also carries `parse_seed_row` for V5's
  `vrp-seed.tsv`.
- `core-config` gains a dependency on `core-vol`, for one reason:
  `tau_ns` is validated by `core_vol::tenor_of`, the same function the
  forecast consults. **Only 4 h and 8 h exist.** E1 — the fact the lane
  monetises — is measured at 4 h and 8 h and is absent by 12 h, so kill
  criterion 4 forbids the longer cells, and there is exactly one place in
  the tree that knows it.
- New Python mirror `claude_worker.vol_ref` implementing the identical
  integer law, and a shared parity fixture
  `claude-worker/tests/fixtures/vol/parity-1.{input,expected}.tsv`. The
  expected file is written by the RUST side
  (`CORE_VOL_PARITY_WRITE=1 cargo nextest run -p core-vol --test parity`)
  and asserted by both `crates/core-vol/tests/parity.rs` and
  `claude-worker/tests/test_vol_ref.py`.
- New alloc gate `vol_engine_minute_and_bounds_are_zero_alloc`; the gate
  count rises 43 → 44.

**Why**
The seed the engine boots from (V5) is cut by the Python; the pairs the
engine forms afterwards continue the same series. If the two
implementations disagree by one unit anywhere, the line the engine trades
on is not the line the research measured — and nothing else in the lane
would notice. Integers across both languages plus a fixture written by one
and asserted by both is the only arrangement where that cannot happen
quietly.

**Ripple effects**
- Nothing is wired into the engine yet. `core-vol` is a leaf crate with
  one consumer (`core-config`, for the tenor check); no strategy composes
  it and no boot path reads `vrp.toml` until V7.
- `docs/local-setup.md` gains nothing yet — `vrp.toml` is only required
  once the member is composed (V8, operator-gated).

**Operator action**
None yet. When the lane reaches V8, copy `vrp.toml.example` to
`~/multivenue/vrp.toml` beside `universe.toml` and `icdp.toml`.

## 2026-09-10 — `detail_version: 6` — the assumed option spread is a flag (VRP V3)

**What changed**
- New flag `--option-spread-frac <ppm>` on `backtest` and `audit-pnl`
  (not on `run`). It is the ASSUMED **crossed** option spread in
  parts-per-million of premium — `50000` = 5 % — and half of it is
  charged on each side of the D-7 synthetic option tick:
  `bid = mark − h`, `ask = mark + h`. The parser rejects anything above
  `1000000` (100 %).
- `crates/cli/src/backtest/fill.rs` gains `opt_half_spread_1e6`, which
  takes the **larger** of the old D-7 floor (`max(0.5 % of mark, 1
  tick)`) and `mark × frac / 2`, ceil-rounded. The flag can therefore
  only ever WIDEN: `--option-spread-frac 0` (the default) reproduces
  every pre-V3 number bit for bit, and the 0 rung of the ladder is an
  **upper bound** on the edge, not a middle estimate of it.
- Both reports now print ONE identical assumption sentence, rendered by
  the shared `backtest::opt::render_opt_mark_law`, naming the rung the
  run executed at. It is printed whenever any option sym was registered
  under the D-7 law — registration, not fills, is what makes the
  assumption able to shape a number.
- `--emit-detail` sidecar `detail_version` **5 → 6**:
  `model.opt_spread_frac_1e6` and a new `options` block
  (`mark_syms`, `mark_fills`, `spread_frac_1e6`, `law`).
- `audit-pnl` stdout gains an `options` object with the same three
  values, emitted **only** when option syms were registered, so
  `audit_pnl_version` stays `1` and an option-free root renders
  byte-identically.

**Why**
No options book exists anywhere in the capture — Deribit's TAIL rows
carry a top-of-book quote and a mark, never a ladder — so every option
fill in either report is a MODEL fill at `mark ± half-spread`. That
half-spread was a hard-coded 0.5 % with no way to ask what the cell is
worth if the real spread is 2 %, 5 % or 10 %. The edge spec's headline
number is only meaningful next to that ladder, and a number whose
assumption cannot be varied cannot be falsified.

**Ripple effects**
- Default behaviour is unchanged: schema-1 stdout, `AUDIT_PNL_VERSION`,
  the `run` surface and the frozen worker argv are all untouched.
- The sidecar changes for EVERY root, option-carrying or not — that is
  what the version bump is for. The `options` block on an option-free
  root reads `mark_syms: 0, mark_fills: 0`.
- First-order magnitude, for reading the ladder: a round trip pays
  exactly `2h` per contract, so the option leg moves down by
  `premium × frac` per round trip and `premium × frac/2` per crossing.

**Operator action**
None. To reproduce a pre-V3 number, omit the flag (or pass `0`).

## 2026-09-10 — Deribit option trade fees are now CAPPED (VRP V2b)

**What changed**
- `crates/cli/src/backtest/fill.rs` gains `FillEngine::fee_for`, which sits
  between `book_fill` and `fee_ceil_1e12`. A sym that has been given an
  index price (`FillEngine::set_opt_index`) and whose venue has an ACTIVE
  `ModelParams::opt_fee` row pays
  `min(index_bps × index × qty, prem_bps × premium × qty)`, each leg
  ceil-rounded in `i128`. Every other sym takes the flat `fee_bps` path
  it always took, byte for byte.
- New `ModelParams::opt_fee: [OptFee; 7]`, indexed by venue byte.
  **Deribit is ACTIVE by default** at its published schedule —
  `index_bps = 3` (0.03 % of the index) capped at `prem_bps = 1250`
  (12.5 % of the premium). Every other venue is `OFF`.
- New flag `--opt-fee <venue>:<index_bps>:<prem_bps>` (and
  `<venue>:off`) on `backtest` and `audit-pnl` **only**. It is
  deliberately NOT on `run`: the live engine does not price options.
- Both loaders now carry the per-sym index. `backtest` populates it in the
  replay pre-pass; `audit-pnl` threads an `opt_index_1e6` map out of
  `load_run_events`. The index is `OptSummary.underlying_px_1e9 / 1_000`
  — the same forward Deribit itself charges against.

**Why**
Deribit's option fee is `min(0.0003 × index, 0.125 × premium)`, not a flat
bps of notional. At these strikes the two legs cross at
`premium = index × 3 / 1250` = **0.24 % of the index** — $189.60 at an index
of $79,000. Below that premium the 12.5 % cap binds; above it the index leg
binds. The measured median premium on the captured chain is $219.07, i.e.
just above the crossover, so BOTH regimes occur inside a single window and
a flat-bps model is wrong in both directions. With the default
`fee_bps = (0, 0)`, option fills previously paid **nothing at all**.

**Ripple effects**
- A backtest or audit-pnl over a root that CAPTURED options now charges the
  option leg. Roots with no option records are unaffected — `fee_for`
  cannot fire without an index, and only option syms are given one.
- The `run` surface, schema-1 stdout keys, `AUDIT_PNL_VERSION` and
  `detail_version` are all unchanged: this adds a cost, not a field.
- Settlement fees (`min(0.00015 × index, 0.125 × value)`, OTM expiry free)
  are NOT here — they belong to the expiry path (VX), not the trade path.

**Operator action**
None. Options were never traded live. To reproduce a pre-V2b number over an
option-carrying root, pass `--opt-fee deribit:off`.

## 2026-09-10 — `detail_version: 5` — harness option marks are USD, not coin (VRP V2a)

**What changed**
- The two option mark-tick synthesis sites — `crates/cli/src/backtest.rs`
  (`load_run`) and `crates/cli/src/audit_pnl.rs` (`load_run_events`) — no
  longer rescale a captured `OptSummary.mark_px_1e9` with `/ 1_000`. They
  call one shared helper, `crates/cli/src/backtest/opt.rs`
  `synth_mark_usd_1e6`, which applies the denomination law
  `premium_usd = mark_coin × underlying_px × contract_size` in `i128`.
- `--emit-detail` sidecar `detail_version` **4 → 5**. Schema-1 (stdout) is
  unchanged and `AUDIT_PNL_VERSION` stays `1` — no key was removed and none
  changed type; the bump exists because the MEANING of a price on an option
  sym changed.
- New crate dependency for `cli`: `opt-registry` (VRP V1). Each run builds
  its own `OptRegistry` from that run's `instrument-manifest.tsv`.
- **The option QUOTE tick lane is converted too, and it is the lane that
  actually runs.** Deribit TAIL rows subscribe to `quote` AND `ticker`
  (`ingress-deribit/src/run_loop.rs:815-820`): the ticker feeds
  `OptSummary`, the quote feeds a real `Tick` whose bid/ask are equally
  coin-denominated. Because every option sym therefore HAS a tick lane,
  `tick_syms` suppresses the D-7 synthesis for all of them and
  `opt_synth_ticks` is 0 on any real capture — so converting only the two
  synthesis sites would have fixed nothing that executes. Both loaders now
  build an `UnderlyingBook` (a per-sym timeline of `underlying_px_1e9`
  from that run's own summaries) and convert every real option quote tick
  after loading. A quote with no underlying known at or before its instant
  is DROPPED rather than guessed. Additive counters
  `opt_quotes_converted` / `opt_quotes_dropped`, emitted only when
  non-zero.
- The DEPTH lane needs no equivalent: measured, `deribit-depth.pmlr`
  carries only the 9 static instruments and no options.

**Why**
Deribit options are inverse: quoted, margined and settled in the base coin
with a 1-coin multiplier, so a mark of `0.00038 BTC` at BTC = $79,000 is a
**$30** premium. `FillEngine` books `notional = px × qty` as USD by
assertion (`backtest/fill.rs:53`). The old rescale therefore understated the
option leg by the underlying price — about **79,000×** for BTC — while the
perp hedge leg was already USD. A delta-hedged run would have reported
essentially pure hedge P&L with the option contributing nothing, and nothing
in the output would have said so.

**Ripple effects**
- **A `Price` on a Deribit option sym is now USD ×1e6.** `FeatId::MarkPx`
  (`strategy-vm/src/features.rs:794`) is UNCHANGED and still returns the raw
  coin `mark_px_1e9` — it is a wire value and rulesets that reference it are
  hash-pinned. The two now mean different things on the same instrument;
  that is deliberate.
- Option marks are produced for **Deribit only**. A marked option record
  from any other venue is skipped and counted, never guessed at:
  `underlying_px_1e9` is venue-inconsistent by construction (OKX puts
  `fwdPx` there and supplies no mark at all; Binance-eapi quotes in USDT).
- New additive counter `opts_unconverted` on both reports — marked option
  records that could not be denominated (foreign venue, a sym the run's
  manifest never named, a missing underlying, an unrepresentable premium).
  A venue that sends no mark is NOT counted. Both reports emit the line
  **only when it is non-zero**, so a root carrying no option records renders
  byte-identically to before this change.
- Contract size offline is the venue constant `1.0` coin
  (`DERIBIT_OPT_CONTRACT_SIZE_1E9`), because `instrument-manifest.tsv`
  carries only the instrument name. What makes that safe is that
  `opt-registry`'s parser refuses Deribit's USDC-LINEAR chains
  (`BTC_USDC-…`), which do not carry that size. The live boot path (V7)
  will use `DeribitInstrumentRow::contract_size_1e9` instead.
- Registries are built **per run**: option ordinals reshuffle at every boot
  by design (chain roll — `crates/cli/src/options_manifest.rs:8-11`).

**Operator action**
None. Options were never traded live, so no existing report's traded P&L
moves. Reports over roots that CAPTURED options will show different option
prices — those rows previously held a coin number labelled as dollars.

Verified on `run-1788984954632200000` (0.98 h): the option-free root is
byte-identical before/after on all four output surfaces, while the root
with options reports `quote_ticks_usd=56562 quote_ticks_dropped=196` from
`backtest` and the identical pair from `audit-pnl` — two independently
written loaders in different symbol spaces agreeing, and both matching a
from-scratch reconstruction off the raw capture (64 quotes preceding their
sym's first underlying print + 132 one-sided books = 196).

## 2026-09-05 — `archive_manifest_version: 1` (object-storage archive)

**What changed**
- A new on-disk/on-bucket schema: `_manifest.json` under each archived run
  prefix, and a byte-identical copy at `v1/index/<host_id>/<run>.json`.
  Top-level keys: `archive_manifest_version, run, epoch_ns, host_id,
  uploaded_at_ns, tool_version, stored_encoding, zstd_level, files, totals,
  span_ns, pmlr_version, windows_2h_complete, catalog, catalog_note`
  (+ `catalog_error` when the catalog could not be produced).
- New optional config namespace `MULTIVENUE_S3_*`, read ONLY by
  `claude_worker.archive_config`, from `~/multivenue/s3.env` (template:
  `s3.env.example`). Not in `.env`: `engine-wrapper.sh` sources that with
  `set -a`, and the archive credential has no business in the engine's
  environment.
- `~/multivenue/retention.conf` gains `ARCHIVE_MODE=s3` and
  `S3_CYCLE_BUDGET_S`.
- `claude-worker/pyproject.toml` gains a SECOND console script,
  `multivenue-archive`. `dependencies` is unchanged — the S3 client is
  handwritten SigV4 over `hmac`/`hashlib`/`httpx`, so no cloud SDK enters
  either dependency graph.

**Why**
- Capture was single-copy on one Mac, and `retention.sh` deleted the oldest
  runs under disk pressure with nothing ever reading the tarballs it made.
  Every research gate is stated in ≤ 2 h windows that already exist, so
  deleting capture deletes the only input those gates have.

**Impact**
- **No engine change of any kind.** No Rust file, no `Cargo.toml`, no new
  engine flag or env key; `target/release/multivenue-engine` is not relinked
  and needs no restart. The PMLR wire format is untouched.
- **Absent config = absent behaviour.** With `MULTIVENUE_S3_ENABLED` unset,
  no HTTP client is constructed, the resolver reports every run as local or
  absent, `retention.sh` uses `compress` exactly as before, and the nightly
  report has no `data_source` key.
- Pulled runs are byte-identical to the originals (file names preserved;
  `.zst` is only the storage encoding), so `discover_runs`, `run_dirs`,
  `select_runs` and `window_root.cut_run` behave identically on them.
- Rollback is one line in `retention.conf` (`ARCHIVE_MODE=compress`) or
  `MULTIVENUE_S3_ENABLED=0`; effective at the next tick, no restart, no data
  movement. Bucket objects are never deleted by this subsystem.

**Layout versioning**
- The bucket prefix `v1/` IS the layout version. A layout change is a new
  prefix, never an in-place rewrite. `epoch_ns` is the partition key so that
  lexicographic `ListObjectsV2` order is chronological order — do not insert
  `YYYY/MM/DD` levels, which would break it.

**Amended 2026-09-07 — went live; defaults re-derived by measurement**
- `MULTIVENUE_S3_PART_SIZE_MIB` 8 → **32** and `MULTIVENUE_S3_TIMEOUT_S`
  300 → **900**. Multipart costs ~3× a single PUT *per part*, so larger parts
  recover most of that; a part must still finish inside the timeout on a bad
  window, and this link has been seen at both 0.1 and 7 MiB/s.
- New key `S3_NICE_NETWORK` in `retention.conf` (default 0). It is the ONE
  lever for the uploader's network class. **Do not put `ProcessType=Background`
  back in the launchd plist**: it applies background QoS to the whole job and
  silently overrides this key, which throttles sustained transfers ~13× and
  starves the cycle until retention can no longer verify-and-delete.
- `S3_CYCLE_BUDGET_S` default 600 → **3600**.
- `gc --max-gib` now takes **-1** to mean "use the configured cap"; `0` means
  "empty the cache" and is honoured (it used to be swallowed as falsy).
- `pnl_report --day` resolves an archived day through the object store, but
  only as a FALLBACK — when the day has no local run at all. A locally-present
  day costs zero HTTP requests.
- **Retention policy change (operator ruling, supersedes D3):** `PROTECT_DAYS`
  5 → 1 with `MIN/TARGET_FREE_GIB` set unreachably high, so the sweep runs
  nightly rather than only under pressure and old runs are DELETED (after
  verification) rather than tarred. The effective local window is **24–72 h**:
  `age_days` is integer, so anything under 48 h is protected, and the sweep
  runs once a day.
- Legacy `~/multivenue/archive/*.tar.gz` are pushed verbatim and pull back via
  `pull` (download → verify sha256 → `tar -xzf`), so the pre-2026-08-29 history
  is retrievable through the same verb as everything else.

## Template

```
## <YYYY-MM-DD> — <short headline>

**What changed**
- ...

**Why**
- ...

**Impact**
- On-disk formats: ...
- Config keys: ...
- Wire formats: ...

**Migration steps**
1. ...
2. ...

**Rollback**
- ...
```

## 2026-09-07 — `regime.toml` hysteresis keys: per-profile `confirm_min` + TREND/VOL/STRETCH exit bands (RG7 fix)

**What changed**

- `[profile.<name>]` gains four OPTIONAL integer keys — `confirm_min`
  (this profile's consecutive-minute confirm; 0 = `[hysteresis]
  confirm_min`), `trend_exit_bps_1e9` (a committed BULL/BEAR holds while
  `|ret| > exit`; breadth is an entry condition), `rv_exit_frac_1e9`
  (LOW holds while `rv < p30·(1+f)`, HIGH while `rv > p70·(1−f)`),
  `stretch_exit_k_1e9` (an EXT holds while `|s| > exit`). Bands must lie
  inside their entry thresholds (`RegimeErr::Bands`, boot refused).
  `core_regime::ProfileParams` carries them (`with_hysteresis`;
  `RegimeParams::{confirm_of, max_confirm_min}`); `judge_trend` /
  `judge_vol` / `judge_stretch` take the committed state like
  `judge_shape`; the seed replay warms `2·max confirm`. The worker
  mirror (`claude_worker.regime`) parses and judges identically; the
  parity harness now runs TWO fixtures (`parity-1` = the RG1 law,
  unchanged bit for bit; `parity-2` = the same tape under the keys).
- `regime.toml.example`: the fast profile carries `confirm_min = 10`,
  `trend_exit_bps_1e9 = 20000000000`, `rv_exit_frac_1e9 = 100000000`,
  `stretch_exit_k_1e9 = 1500000000` and a wider SHAPE band
  (`er_lo_exit_1e9 = 400000000`, `er_hi_exit_1e9 = 500000000`); the
  slow profile is unchanged.

**Why**

- The first RG7 soak (2026-09-07) FAILED: 7 of 9 windows over the ≤ 2
  flips bound, all on the FAST profile (`shape` 3–7, `trend` 3–6 per
  2 h). RG1 had implemented the §3.5 bands for SHAPE only, and a global
  `confirm_min = 3` is thin for a 60-min horizon judged in 5-min steps.
  Offline replay over the failed windows: pre-fix 8/9, confirm 10 alone
  2/9, bands alone 6/9, both 1/9.

**Impact**

- On-disk formats: none. Wire formats: none. Config: four optional keys
  per profile; every existing `regime.toml` boots and judges unchanged
  (keys absent ⇒ 0 ⇒ the RG1 law). The live file needs the keys for the
  fix to apply; the boot reads them (restart-applied, as every regime
  parameter).
- `RegimeState` layout: `ProfileParams` grew (the `_pad0` byte + 3
  trailing `i64`) — a boot-boxed struct, no wire or snapshot exposure.

**Migration steps**

1. `cargo build --release -p cli`; add the keys to `~/multivenue/
   regime.toml` `[profile.fast]` (values as the example); restart.
2. The RG7 soak restarts from zero: only windows AFTER the restart count.

**Rollback**

- Delete the keys (or set them to 0) and restart — the RG1 law, bit for bit.

## 2026-09-05 — The harness funding seed: `funding-seed.tsv` per window, `backtest --funding-seed` (RG4 carry blocker)

**What changed**

- `backtest` gains `--funding-seed <path>` (`cli::backtest::funding`):
  a `descriptor \t ts_ms \t rate_1e9` file (`#` comments; malformed =
  fatal) becomes synthesized `AiCmdKind::FundingSeed` commands applied
  through the vm's live `on_ai` path — dedup law included — BEFORE the
  first replayed record. Default = the first run directory's own
  `funding-seed.tsv` when it exists, else none. With a seed applied the
  V5 warm-up law drops the `apr24`/`apr72` 24 h / 72 h requirement
  (the seed IS the prints' history; a seed shorter than a feature's
  window leaves that feature ABSENT by the feature law). Summary gains
  `funding: seed_prints=… dropped=… deduped=… warmup=seeded|table`
  plus a `funding: seed <path> …` build tell. `audit-pnl` is untouched
  (it audits what happened; no vm warm-up is involved).
- Worker: `window_root.cut_run` writes the window's `funding-seed.tsv`
  whenever `seed=(…, candles.db)` names a store with a funding table —
  the window's manifest × the funding table under the boot seed lane's
  own law (`claude_worker.seeds.funding_seed_rows`: 73 h before the
  window's first instant, exclusive; newest 640 per sym; rate ×1e9
  RAW). `pool_ensure` back-fills the file on reused cuts (no re-cut).
  Neither file is in git (a window root is research data).

**Why**

- Under the ≤ 2 h law a funding-carry member (`apr24`/`apr72`) could
  never be evidenced — the table-global 24 h warm-up swallowed every
  2 h window (0 orders). Live, the same row is warm from its first
  minute because the boot seed lane pushes 73 h of prints; the replay
  now has the same warm-up, so the library / composer can evidence
  carry members on the standing pool (RG4's "≥ 2 members" exit tell).

**Impact**

- On-disk formats: a new optional per-window file `funding-seed.tsv`
  beside the manifests. Schema-1 stdout unchanged; `--emit-detail`
  unchanged (`detail_version` 4).
- Config keys: none. Wire formats: none (kind 10 frames, unchanged).
- Behaviour: NONE without a seed file (`warmup=table` = the V5 law);
  a root whose first run carries `funding-seed.tsv` replays seeded —
  the frozen worker argv picks it up by default, which is the point.

**Migration steps**

1. `cargo build --release -p cli` (pitfall 18 — the harness is the
   release binary).
2. Existing pool windows gain the file on the next `pool_ensure`
   (composer run) with `candles.db`; single windows: cut again with
   `seed=(regime.toml, candles.db)`, or pass `--funding-seed`.

**Rollback**

- Delete the per-window files (or pass `--funding-seed /dev/null` —
  an empty seed = `warmup=table`).

## 2026-09-05 — Ruleset grammar v2.1 regime keys, the vm row gate, the regime-aware harness (RG3)

**What changed**

- Ruleset JSON grammar v2.1: three optional v2 row keys — `regimes`
  (string array, the §3.3 label grammar with `fast:`/`slow:` prefixes
  and `rel:` terms), `regime_off` (`soft`|`hard`), `rel` (sugar for one
  `rel:` term) — parsed by the ingress-ai byte scanner into the
  `RuleRowV2` regime tail RG0 reserved (`regime_fast`/`regime_slow`/
  `regime_off`/`regime_rel`). Validator **rule 11** (`RulesetReject::Regime`)
  and the **rule-8 amendment** (identity-tuple duplicates only when the
  regime regions intersect — disjoint variants of one signal admit).
  `RegimeLabel::LABELLED_ANY` (core-types) = the fill of a REL-only
  profile. Rows without the keys are bit-identical (tail zero) — every
  existing artifact hashes and validates unchanged.
- `strategy-vm`: per-row gate byte `row_gate` judged on every
  `set_regime_view` (the new set→vm seam: `core_regime::RegimeView`,
  `RegimeState::view()`, pushed by `StrategySet` on configure / seed /
  every minute roll / effective change / declaration) and on every
  flip; the entry path skips closed rows (`regime_blocked`), the exit
  path flattens HARD-closed position rows at once (`regime_hard_exits`;
  age-out first, min-hold bypassed); soft-closed rows drain by their
  own law. `StrategyCounters::vm_regime_{blocked,hard_exits}`,
  `/metrics` `engine_vm_regime_{blocked,hard_exits}_total`.
- Harness: `backtest` / `audit-pnl` gain `--regime <path>|off` and
  `--regime-seed <path>` (`cli::backtest::regime`): the engine's own
  `RegimeState` replays over the window's ticks, funding prints and the
  `SetRegime` frames of `ai-cmds.pmlr` (pre-anchor frames clamped with
  their TTL shortened; expired ones dropped + counted); the vm receives
  the view exactly as live; `off` strips every tail (the on/off delta);
  absent flag = the default artifact when it exists and resolves on the
  root (members absent from the root are DROPPED, refs must resolve),
  else regime-blind with a stderr tell — the frozen worker argv can
  never fail on a default artifact. `--emit-detail` is `detail_version`
  4 (additive `regime` block); `audit-pnl` JSON gains the additive
  `regime` section (per profile: minutes per effective word, per
  `(word, strategy)` fill-model replays with the fee ladder;
  `audit_pnl_version` stays 1). `cli::regime_boot` is the ONE resolver
  the engine boot and the harness share.
- Worker: `window_root.cut_run` carries the pre-window `SetRegime`
  declaration still in force at the cut (latest frame per profile
  decides) in front of the `ai-cmds.pmlr` slice, and writes the window's
  own `regime-seed.tsv` from `candles.db` when `seed=(regime.toml,
  candles.db)` is given (`pnl_report` day mode passes the defaults);
  `strategist` prompt **v3** (`STRATEGIST_PROMPT_VERSION =
  "strategist-v3"`) teaches the keys, the gate law and regime variants
  and asks for `regimes` on every row; `parse_proposal` accepts the keys
  structurally (`regime_term_ok` / `regime_rel_ok` vocabulary mirrors).
- Bench alloc gate 42 (`vm_regime_gate_and_view_rejudge_are_zero_alloc`).

**Why**

- `docs/regime-and-dashboard-plan.md` RG3: rows gate themselves on the
  regime (D2 — no table flip on a regime change), and every backtest /
  nightly report shows the per-regime P&L and the on/off delta.

**Impact**

- On-disk formats: none new. `--emit-detail` sidecar `detail_version`
  3 → 4 (additive). `audit-pnl` stdout: additive `regime` key.
  Windowed roots may now carry `regime-seed.tsv` and a carried
  pre-window `SetRegime` frame in `ai-cmds.pmlr` (ts before the first
  tick — the harness clamps it).
- Config keys: none. Ruleset artifacts: the three optional row keys.
- Wire formats: none (the tail bytes were reserved at RG0; kind 12
  unchanged).
- Behaviour: NONE for every existing artifact (tail zero ⇒ open under
  every view). A LABELLED row fails closed until the engine's detector
  knows the regime (`~/multivenue/regime.toml` + seed) — and in the
  harness until `--regime` (or the default artifact) resolves.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before any backtest /
   audit-pnl that matters or the next boot.
2. Nothing else: labelled rows only exist once a ruleset carries the
   keys.

**Rollback**

- Revert the commit. Labelled artifacts then reject as rule 2
  (unknown key) at stage — nothing else changes.

## 2026-09-03 — Regime detector in the engine: `regime.toml`, `regime-seed.tsv`, `--regime`, the `engine_regime_*` family (RG1–RG2)

**What changed**

- New crate `core-regime` (the measured regime: minute-close rings,
  the integer judge law, confirm hysteresis, the declared-over-measured
  effective law) and `core-config::regime` (the `regime.toml` parser +
  the seed reader). `ret_bps_1e9` / `isqrt_i128` moved to
  `core_regime::math` (`strategy-icdp` re-exports; `strategy-vm` imports).
- `Strategy` trait: `regime_label` / `set_regime_label` / `on_regime`
  (all defaulted — no existing strategy changes behaviour);
  `StrategyCounters::regime_counters`; `IcdpCounters` gains
  `regime_blocked` / `regime_exits` (additive).
- `StrategySet` owns the detector: a 1 s timer is armed when a detector
  is configured (`REGIME_TIMER_NS`; members' timers were `u64::MAX`);
  `AiCmdKind::SetRegime` is consumed at set level; labelled members
  receive edge-triggered `on_regime` calls. The ai-exec refuses intents
  while its gate is closed; the icdp blocks decisions while closed and
  exits open positions on a HARD close.
- Boot surface: `--regime <path>` (default `~/multivenue/regime.toml`;
  ABSENT default = detector unconfigured, today's behaviour; explicit
  path or present-but-invalid file = boot refused), `--regime-seed
  <path>` (default `~/multivenue/regime-seed.tsv`; absent = warm live).
  `scripts/engine-wrapper.sh` exports the seed
  (`python -m claude_worker.regime seed-out`) before every boot when
  the artifact exists.
- `/metrics`: the `engine_regime_*` family (≈ 50 names) +
  `engine_icdp_regime_{blocked,exits}_total`. Registry headroom holds
  (256 counters / 384 gauges).

**Why**

- `docs/regime-and-dashboard-plan.md` RG1–RG2 (operator decisions
  D1–D4): the engine measures the regime itself, the AI declares, the
  effective word gates members without a table flip.

**Impact**

- On-disk formats: two new DATA files under `~/multivenue/`
  (`regime.toml` — operator-authored from `regime.toml.example`;
  `regime-seed.tsv` — worker-generated, atomic write). Neither is in git.
- Config keys: `regime.toml` grammar per `regime.toml.example`
  (integer-only TOML subset; unknown/missing/duplicate keys refuse the
  boot).
- Wire formats: none beyond RG0's kind 12.
- Behaviour: NONE until `~/multivenue/regime.toml` exists. With it and
  no `[labels.*]`, every coded member stays unconstrained (ANY) — the
  detector only measures and publishes.

**Migration steps**

1. `cargo build --release -p cli` (G0 relink) before the next boot.
2. Optional: `cp regime.toml.example ~/multivenue/regime.toml` and edit
   `[breadth] members` to descriptors present in `universe.toml`.
3. Restart the standing engine; check the boot tells and
   `engine_regime_configured`.

**Rollback**

- Remove `~/multivenue/regime.toml` (detector unconfigured, no
  behaviour change) or revert the commit. The seed file is inert
  without an artifact.

## 2026-09-03 — `AiCmdKind::SetRegime = 12` + `RuleRowV2` regime tail (RG0 — regime wire freeze)

**What changed**

- `core_types::regime` (new module): `RegimeWord` (one byte per
  dimension, one-hot; bit 7 of each market byte = the per-dimension
  UNKNOWN mark; byte map in `docs/wire-format.md`), `RegimeLabel`
  (any subset per byte; `0` = unconstrained; gate `label == 0 || (word &
  label) == word`; omitted dimensions fill with the legal mask incl.
  the unknown mark, explicit lists exclude it — fail-closed per
  dimension), `RegimeRel` (per-symbol RELATIVE nibbles),
  `RegimeTerm` / `RegimeLabelSet` (≤ 4 product terms, ∃-semantics, for
  coded members), the rule-8 `intersects` law, and the text grammar
  `[fast:|slow:]dim:(*|!v|v1|v2…)` (`parse_label_term`,
  `RegimeLabelBuilder`). `REGIME_PROFILES = 2` is a layout constant.
- `AiCmdKind` gains **`SetRegime = 12`** (append-only ABI; the first
  unassigned byte is now 13) with its `validate_shape` arm: `param_id`
  = profile `< 2`, `px` = declared word (SOURCE byte empty), `qty` = 0
  or a state word, `ttl_ns > 0` ENFORCED, `strategy_id = 0xFF`, sym/side
  none, flags bit 0 legal. `frames.py` mirrors it (`KIND_SET_REGIME`,
  `regime_word()` helpers); the shared golden fixture gains vectors for
  kinds 10, 11 and 12 (it stopped at 9 before).
- `RuleRowV2` spends 18 of its 40 reserved tail bytes: `regime_fast:
  u64` @88, `regime_slow: u64` @96, `regime_off: u8` @104, `regime_rel:
  u8` @105; `_pad3` shrinks to `[u8; 22]` @106. Still 128 B; `ver`
  stays 2; `RuleRowV2::new` keeps its 23-argument shape (tail zero);
  `with_regime(term, off)` sets the tail; `regime_fields_well_formed()`
  is the rule-11 body (RG3 wires it into the validator).
- `audit-replay`'s AI-command table is sized by the last kind (13
  labels) — it indexed out of bounds on any captured seed (kinds 10/11)
  before.

**Why**

- `docs/regime-and-dashboard-plan.md` (operator decisions D1–D4): the
  regime is a gate; VM rows carry their own per-profile masks so a
  regime change never flips a table; the AI declares a word per
  profile through the existing AI plane.

**Impact**

- On-disk formats: none (`RuleRowV2` never crosses a process boundary;
  PMLR `slot_kind = 4` records keep their 64 B shape — kind 12 is a new
  value in an existing byte).
- Config keys: none yet (`regime.toml` arrives with RG2; the example
  file is committed now).
- Wire formats: `AiCmd` kind byte 12 admitted; a pre-RG0 engine counts
  it as malformed (`engine_ingress_ai_malformed_total`) and drops it —
  no crash, no ring push.
- Every existing ruleset artifact validates to bit-identical rows
  (both masks 0); nothing is gated until a row carries a `regimes` key
  (RG3 grammar).

**Migration steps**

1. `cargo build --release -p cli` before any live boot (G0 relink law).
2. Nothing else — no artifact, DB, or capture changes.

**Rollback**

- Revert the commit; the ABI is append-only so no captured file needs
  rewriting. A worker that sends kind 12 to a reverted engine sees the
  frame counted as malformed.

## 2026-09-03 — `Order.ttl_ns` (wire-additive) + the IoC taker fill law + fee ladder (ICDP I1)

**What changed**

- `core_types::Order` spends 8 bytes of its explicit zero padding:
  `ttl_ns: u64` at offset 48 (`_pad1` shrinks to 42..48; `_pad2`
  unchanged). Still 64 B, `repr(C, align(64))`, every byte explicit.
  `Order::new` keeps its 8-argument shape (ttl 0);
  `Order::with_ttl_ns(ttl)` sets it.
- `backtest::fill` (shared verbatim by `backtest` and `audit-pnl`):
  `Order.kind == 1` (IoC) is now MODELED — judged once at the first
  fresh two-sided tick of its sym at/after `t_emit + Δ_venue`: a BID
  fills at that tick's `ask_px` iff `ask_px ≤ P` (an ASK at `bid_px`
  iff `bid_px ≥ P`), qty capped by the displayed opposite size under
  the same shared FIFO budget as makers, the remainder cancels (never
  rests), fee = the venue's TAKER column, rounded up. Any order with
  `ttl_ns > 0` is canceled at the first record of its sym at/after
  `t_emit + ttl_ns` (stale and one-sided records included — expiry is
  a clock fact), before any fill evidence of that record is read.
  `kind ≥ 2` is unroutable (counted, dropped; `debug_assert!` in debug).
- Every fill also accrues the §4.3 **fee ladder** (the same notional
  at flat 0 / 1 / 2 bps per side); reports print OOS net at the ladder
  beside the CLI tier. New counters `ioc_fills`, `ioc_canceled`,
  `ttl_expired`.
- Surfaces: backtest stderr `fills:` line gains `ioc= ioc_canceled=
  ttl_expired=` and a new `fee ladder (oos net, flat bps/side): 0= 1=
  2= tier=` line; **`--emit-detail` is `detail_version` 3**
  (`fills.ioc`, `fills.ioc_canceled`, `fills.ttl_expired`,
  `oos.fee_ladder_net_usd[3]`); audit-pnl per-strategy stderr gains an
  `ioc_fills=… | fee ladder …` row and the JSON (still
  `audit_pnl_version` 1 — additive keys, get-based readers) gains
  `ioc_fills`, `ioc_canceled`, `ttl_expired`, `fee_ladder_net_usd[3]`.
  **Schema-1 stdout is unchanged (frozen).**

**Why**

- ICDP is a taker edge (research note §5.6: resting entries are
  anti-selected); the post-only law scored it negative by construction.
  The vault's merged ICDP×VT plan, D1 (offline IoC model + paper IoC
  intents are pre-Stage-3) and D6 (G1 gate lifted, I1–I7 in order).

**Impact**

- On-disk formats: `engine-orders.pmlr` slots may now carry a non-zero
  `ttl_ns`; every older file reads 0 (never expires). `pmlr.py`'s
  `OrderRec` decodes the 42-byte prefix and is untouched.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Order` table + `kind` semantics.

**Migration steps**

1. Nothing for existing captures or goldens (the VM emits kind 0, ttl 0
   — every existing number is byte-identical).
2. Sidecar readers: accept `detail_version` 3 (additive keys).

**Rollback**

- Revert the commit; IoC intents already captured would replay under
  the maker law again (negative by construction) — the reason for I1.

## 2026-09-03 — Staleness is live: v3 captures start, the harness re-judges, `--emit-detail` v2 (VT2–VT6 close)

**What changed**

- The standing engine writes PMLR v3 since `run-1788417289611943000`
  (relink + reboot 06:34Z); every earlier run dir is v2.
- Every ingress stamps `venue_time_ms` and judges staleness live
  (`core_time::FeedClock`, per-venue `stale_after_ms` defaults pm 1000 /
  bn 1000 / okx 400 / deribit 600 / hl 700 / bybit 500; `run
  --stale-after-ms <venue>:<ms>`); Binance spot inherits the `aggTrade`
  sentinel's stamp + verdict with `TICK_FLAG_VENUE_TIME_SENTINEL`, and
  spot prints are captured as `ChannelId::Trade` event rows.
- strategy-vm: `Mid/Bid/Ask` are ABSENT while the last tick is stale.
- `backtest` / `audit-pnl`: `--stale-after-ms <venue>:<ms>` (both);
  v3 ticks are RE-JUDGED from the stamp per (venue, sym) per run in file
  order (`cli::backtest::stale`), with the **sentinel latch law**: a
  repeated inherited stamp keeps its print's verdict (re-judging it on
  the book update's own `ts_ns` flagged quiet seconds — 3.3 % false stale
  on `binance:btcusdt`, found by the VT2 live smoke). Stale ticks
  neither fill nor mark. stderr gains one `stale:` line per run
  (`stale-blind(v2)` on v2 files). **`--emit-detail` sidecar is
  `detail_version` 2**: `model.stale_after_ms` + a `stale` block
  (`ticks_skipped`, per-run per-lane `{ticks, stale_ticks,
  stale_time_bps, stale_blind}`). Schema-1 stdout is unchanged (frozen).
- `capture-catalog`: per-lane `stale_captured` (the ingress's live
  verdict count; `null` on v2 lanes) in JSON + summary.
- Metrics: `engine_ingress_<venue>_stale_ticks_total`,
  `engine_ingress_<venue>_feed_delay_ema_ms`. `IngressStatus` slot is
  exactly 128 B with zero slack.
- `claude_worker.features.collect_marks` skips stale ticks on v3 files.

**Why**

- `docs/arch/venue-time-capture-plan.md` §1 — stale-blind captures book
  mid-to-mid gains against books the engine could not see. Measured on
  the first v3 run: the VM row's one round trip is +$1.07 stale-blind vs
  −$4.87 judged (vault note).

**Impact**

- On-disk formats: v3 run dirs; `detail_version` 2 sidecars (a v1
  reader must tolerate the new keys — the worker never reads the
  sidecar).
- Config keys: none (`--stale-after-ms` is a flag; the wrapper passes
  none — venue defaults apply).
- Wire formats: none beyond VT1's entry below.

**Migration steps**

1. Research on staleness needs v3 roots: cut ≤ 2 h windows by `ts_ns`
   (the events file cut too) from runs ≥ `run-1788417289611943000`.
2. Treat every `stale-blind(v2)` number as an upper bound (CLAUDE.md
   pitfall 17).
3. Thresholds are re-derived per deployment with the engine-side table
   in `docs/venue-latency.md` §5 — a change is a replay, not a recapture.

**Rollback**

- `--stale-after-ms <venue>:0` on every venue restores the stale-blind
  law offline without a rebuild; the engine flag does the same live
  (ticks still carry the stamp). Reverting the crates restores v2
  writing; v3 files remain readable by the v3 reader only.

## 2026-09-03 — PMLR v3: `Tick.flags` + `Tick.venue_time_ms` (VT1, venue-time capture)

**What changed**

- `core_types::Tick` spends its v2 tail pad: `flags: u8` at offset 49
  (`TICK_FLAG_STALE = 1`, `TICK_FLAG_VENUE_TIME_SENTINEL = 2`) and
  `venue_time_ms: u64` at offset 56; `_pad` shrinks to 6 bytes (50..56).
  Still 64 B, still `repr(C, align(64))`, every byte explicit.
- `Tick::new` keeps its signature and now means "venue time unknown,
  flags 0" (the v2 shape); `Tick::new_stamped(…, venue_time_ms, flags)`
  is the v3 constructor the VT2 ingress parsers will use; `Tick::is_stale`.
- `core_io::VERSION` 2 → 3 for every slot kind (one header number).
  `PmlrReader` accepts ≤ 3 and gains `has_venue_time()` (version ≥ 3).
- `crates/cli` capture acceptance (backtest, audit-pnl, capture-catalog)
  moves from `== 2` to `MIN_PMLR_VERSION ..= core_io::VERSION` through one
  `pmlr_version_accepted` law; v3 ticks replay under the v2 law (never
  stale) until VT4 lands the harness stale rule.
- `claude_worker.pmlr`: `VERSION_MAX = 3`, `TickRec` gains `flags` +
  `venue_time_ms` (+ `is_stale()`), `Reader.has_venue_time`,
  `TICK_FLAG_*` mirrors; golden fixture `ticks_v3.pmlr` (Rust writer);
  `ticks_v2`/`fills_v2`/`ticks_v1` regenerated byte-identical.

**Why**

- `docs/arch/venue-time-capture-plan.md` §1: the capture cannot tell a stale
  Binance book (8.9 % of messages > 500 ms stale) from a current one;
  VT1 is the wire-format prerequisite for the per-venue venue-time
  extraction (VT2) and the staleness gate (VT3/VT4).

**Impact**

- On-disk formats: new captures carry header version 3. Every v2 (and
  v1) file stays readable; both new fields decode as 0 from v2 ("venue
  time unknown, never stale") and as garbage from v1 — consumers gate on
  `has_venue_time`.
- Config keys: none.
- Wire formats: `docs/wire-format.md` `Tick` table + replay-log header
  version row. Rings unchanged (same 64 B `Tick`); raw tap unchanged.

**Migration steps**

1. Nothing for existing captures.
2. Tooling that hard-codes `version == 2` (none in-tree after this
   entry; research one-shots read `≤ 3`) must accept 3.
3. The release binary is relinked on the operator's next authorized
   boot (G0 law) — until then the standing engine keeps writing v2.

**Rollback**

- Revert the commit; v3 files written meanwhile are refused by a v2
  reader (`UnsupportedVersion(3)`) — keep them or drop the run dirs.

## 2026-08-30 — VM2 V6: worker seed lane, depth digest, coverage audit, channel map, per-host candle budgets

**What changed**

- `claude_worker.pmlr` learned the kind-7 `DepthTopK` decode
  (`DepthReader`, 192-byte stride — the FIRST kind-determined slot
  size; the 64-B `Reader` keeps refusing kind 7 by design) and the
  kind-3 `Order` decode (`Reader.order/orders`, engine-orders.pmlr).
- `claude_worker.frames`: `KIND_FUNDING_SEED = 10`,
  `KIND_POSITION_SEED = 11` (core-types AiCmdKind mirror).
- NEW module `claude_worker.seeds` — the D-1/D-2 seed lane: kind-10
  frames from the candles.db `funding` table (RAW venue prints ×1e9;
  the ENGINE owns the deribit ÷8 law) + kind-11 restores
  reconstructed from the previous run's engine-orders.pmlr (slot-5
  FIFO fold, (sym,ref)-unique-row ambiguity law, sym RE-RESOLVED
  through the CURRENT manifest, qty = age seconds, ttl 0).
- NEW module `claude_worker.depth_digest` — hourly
  imbalance/spread/near-notional stats per (venue, descriptor) into
  the NEW `depth_digest` table beside candles (iv_digest pattern;
  STALE and empty-side snapshots skipped + counted).
- NEW module `claude_worker.coverage_audit` — per-class expected-vs-
  present audit of candles/funding/iv/depth over the newest manifest.
- NEW module `claude_worker.channel_map` — generated per-instrument
  channel TSV; `caps_of_descriptor` python mirror pinned CROSS-
  LANGUAGE against the new Rust `caps_of_descriptor_law` test.
- `claude_worker.candles.run_cycle`: REST budgets are now per HOST
  (`budget_key`; only the two bybit categories pool) and DEMAND-SIZED
  `max(env floor, 2 × tfs × targets)` per host.
- `scripts/candles-cycle.sh` runs `claude_worker.depth_digest` after
  the IV digest (same serialized window; exec bit preserved).

**Why**

- V6 of docs/vm2-plan.md: warm VM restarts (funding windows + open
  positions survive the daily restart without crons) and D-8 research
  reach (the agent sees depth, knows every instrument's channels, and
  the audit names data holes instead of leaving them silent).
- The budget change fixes a LIVE starvation found by the new audit
  (2026-08-30): binance spot and usdm shared one per-venue 30-call
  budget although they hit different hosts — the 22-target usdm lane
  got ZERO pages every cycle and 12 M5/BST symbols had no candles at
  all.

**Impact**

- On-disk formats: NEW `depth_digest` table in candles.db (additive);
  no existing table/file changes. `~/multivenue/worker/channel-map.tsv`
  is a new generated file.
- Config keys: none new (depth digest honors
  `CLAUDE_WORKER_DEPTH_DIGEST_WINDOW_H`, default 26). The meaning of
  `CLAUDE_WORKER_CANDLES_BUDGET_PER_H` narrows from per-venue pool to
  PER-HOST FLOOR.
- Wire formats: none (kinds 10/11 landed in V1; this is the worker
  mirror).

**Migration steps**

1. Nothing mandatory — tables and modules are additive; the next
   hourly candles cycle backfills the starved usdm symbols under the
   new budgets and starts writing depth digests.
2. Optional one-shots: `python -m claude_worker.depth_digest
   --backfill` (done 2026-08-30, 108 buckets), `python -m
   claude_worker.channel_map`.

**Rollback**

- Revert the commit; drop the `depth_digest` table if desired
  (nothing reads it engine-side). Seeds are push-only and the engine
  refuses/expires malformed ones by design.

## 2026-08-30 — VM2 V5: multi-channel backtest, warmup, D-7 options mark-fill law, D-3 report/gate amendment

**What changed**

- The backtest merge carries funding/ctx `ChannelEvent`s, `DepthTopK`
  and `OptSummary` records beside ticks (same §3.2/§3.3 total order
  and VIRT rebase; lane ordinals extend the lord space), replayed
  through the vm's REAL callbacks — every §1.1 feature evaluates in
  replay exactly as live (§1.5).
- Per-run sym REBIND (the §6 law's replay half): each run's manifest
  joins by DESCRIPTOR to the newest run's, and every record's sym is
  rewritten at load — options ordinals that reshuffle across boots
  evaluate as ONE instrument.
- WARMUP (§1.5, refined — recorded in vm2-plan §8): the longest
  window the TABLE references (Roll windows; Apr24 ⇒ 24 h; Apr72 ⇒
  72 h), 0 when none — features fill, no entries, split math
  unchanged, `warmup_end_virt_ns` reported.
- D-7 options mark-fill law in `backtest::fill` (shared by
  audit-pnl): mark-bearing OptSummary records synthesize zero-spread
  mark ticks for option syms without a tick lane; registered syms
  execute IMMEDIATELY at `mark ± max(0.5%, 1 tick)` with TAKER fees
  and value at mark; `mark_fills` counts and the assumption is
  PRINTED wherever it shaped numbers. okx's markless summaries stay
  feature-only (honestly unpriceable).
- D-3: schema-1 gains ADDITIVE keys — `oos.round_trips`, `oos.legs`,
  top-level `position_rows` (goldens updated; schema_version stays
  1). The worker gate counts LEGS toward `min_trades` and folds the
  position-ruleset floor `round_trips >= MIN_ROUND_TRIPS (10)` into
  the same verdict — GateThresholds/GateResult keep their frozen
  shapes; pre-V5 reports gate byte-identically (pinned by
  `tests/test_backtest_d3.py`, ruling cited).

**Why**

- vm2-plan §4-V5: funding/IV/depth strategies must be honestly
  backtestable through the frozen argv before V7's real backtests.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: schema-1
  additive keys documented in the harness goldens; worker report
  gains mirrored additive keys.

**Migration steps**

1. None — pre-V5 captures and reports replay/gate unchanged.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V4: validator v2 (descriptor resolution, rules 9–10), the §6 handoff flips to v2, v1 `RuleTable` retired

**What changed**

- The §4.2 validator grew the v2 grammar arm (docs/wire-format.md
  "Ruleset JSON grammar v2"): descriptor-addressed rows (D-6 —
  stage-time resolution against the bin's DescriptorTable, built
  from the SAME allocation truth as `instrument-manifest.tsv`;
  unresolvable ⇒ the new `Descriptor` reject), rule 9 (`Position`)
  and rule 10 (`Feature`: channel capabilities + window law + the
  rolling-bind budget), signed 9-decimal thresholds, and the
  KEYWORD_CAP 16→24 growth. v1 rows keep validating byte-exactly
  (the compat arm builds them THROUGH `RuleRowV2::from_v1`); both
  shapes may share one artifact.
- The §6 table-handoff ring is v2-typed (`RuleTableSlot =
  RuleTableV2`, 32 832 B slots; `Strategy::on_ruleset_table` takes
  `&RuleTableV2`); the v1 `RuleTable` struct retired (`RuleRow`
  stays as the v1-grammar record through the compat window).
- The backtest harness resolves v2 descriptors against the NEWEST
  run's `instrument-manifest.tsv` (offline capability law =
  `caps_of_descriptor`, deliberately permissive where the string
  under-determines — wrong grants only yield absent-data-holds);
  manifest-less (pre-D3) captures refuse v2 rows honestly.
- Fuzz: `ruleset_json` covers both arms (fixture descriptor table
  wired into the target; corpus seeded with v2/mixed artifacts).
  Bench gate 34 validates 255 v1 + 1 v2 rows with live resolution
  inside the measured window.

**Why**

- vm2-plan §4-V4/D-6: artifacts must be portable across restarts and
  universe edits, and refusals must be loud and specific before the
  agent authors against the grammar.

**Impact**

- On-disk formats: none (artifacts stay JSON; identity unchanged).
- Config keys: none.
- Wire formats: ruleset JSON grammar v2 documented; v1 table section
  retired in docs/wire-format.md.

**Migration steps**

1. None for operators: existing v1 artifacts (raw syms) stage and
   commit unchanged through the compat arm — one release, per D-6.

**Rollback**

- Revert the commit.

## 2026-08-30 — VM2 V3: the v2 grammar evaluator + position layer live in strategy-vm

**What changed**

- `VmStrategy` evaluates the GENERAL v2 grammar (vm2-plan §1.2–§1.3)
  over the V2 feature engine: two-operand signals, confirm gates,
  the position state machine (Flat→Entered→Flat), group exclusivity,
  two-leg emits, min/max-hold, the universal exit law
  `signal × entry_sign ≤ exit_1e9`, and `PositionSeed` (D-2)
  restore. v1 `RuleTable`s arriving through the UNCHANGED trait seam
  map row-for-row onto v2 sugar rows (`RuleRowV2::from_v1` — the
  byte-exact v1 semantics law); the §6 handoff ring stays v1-typed
  until V4 flips the validator.
- SEMANTIC DELTA (deliberate, vm2-plan §1.2): rows now evaluate when
  EITHER leg's sym ticks (two-legged signal freshness) — v1
  evaluated on action-sym ticks only. Fires move to the FIRST tick
  that satisfies them (fresher data); condition/emit laws unchanged.
  The golden harness pins the new eval counts.
- The vm's book generic retired (`VmStrategy<N>` → `VmStrategy`) —
  mids live in the feature engine (`FEAT_SYM_SLOTS` grew 1024→4096,
  absorbing the old `BACKTEST_VM_SLOTS` law); `SET_VM_SLOTS` retired
  with it.
- Sizing law hardened (caps-proptest catch): a qty whose NOTIONAL
  floors to zero is clamped away (the §11 zero-notional invariant
  now lives in `sized_qty_1e6` itself).

**Why**

- vm2-plan §4-V3: the general grammar must execute engine-side
  before the validator (V4) can accept it from artifacts.

**Impact**

- On-disk formats: none. Config keys: none. Wire formats: none (the
  evaluator is in-process; V1's formats stand).

**Migration steps**

1. None — v1 artifacts behave identically through the sugar mapping
   (two-legged evaluation moves WHEN a fire lands, never whether).

**Rollback**

- Revert the commit.

## 2026-08-29 — VM2 V2: feature engine, OptSummary engine lanes, Deribit Funding `v1` = funding_8h, HL event lane

**What changed**

- `strategy-vm` gains the feature engine (`strategy_vm::features`,
  vm2-plan §1.1): ONE boxed ~12 MiB zeroed-at-boot state holding
  per-sym latest values, per-(sym, window) rolling minute rings
  (lazy-recompute stats), funding-print rings with the per-venue
  settled-print laws, mark/IV and depth-derived features, and the
  venue-derived wall-clock offset. Fed exclusively through the vm's
  `Strategy` callbacks — the backtest replays the same records
  through the same code (§1.5 parity). Zero alloc after boot
  (release gate 39, `vm_feature_engine_paths_are_zero_alloc`).
- OptSummary (kind 6) enters the engine for the first time:
  `Strategy::on_opt_summary` (defaulted), three opt lanes
  (`OPT_RING_SIZE` 4096; `engine::opt_lane_of` okx/deribit/bn — the
  BN lane venue-dark until the eapi heal), pushes at every venue
  emit site AFTER capture (§6.5 law), `opt_ring_drops_total` per
  ingress. Capture files unchanged.
- Deribit Funding events: `v1` now carries `funding_8h` ×1e9 (was a
  constant 0) — additive; the parser gained the optional field
  (ticker scratch frame grew to 128 B, in-process only). Pre-V2
  captures replay via the `current_funding` (`v0`) fallback.
- Hyperliquid gained its venue-event lane (it had none): funding
  rides AssetCtx rows, spawn mask
  `EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX`.
- `AiCmdKind::FundingSeed` is consumed: the vm folds seeds into the
  same funding windows live events feed (dedup within half the
  venue print period).

**Why**

- vm2-plan §4-V2: every §1.1 feature must evaluate engine-side (and
  identically in replay) before the V3 grammar evaluator lands.

**Impact**

- On-disk formats: none (no capture layout changed; deribit Funding
  `v1` is a value-semantics addition inside an existing field).
- Config keys: none.
- Wire formats: docs/wire-format.md — OptSummary lane note, Funding
  `v1` note, HL AssetCtx lane note.

**Migration steps**

1. None — replay of old captures works (fallbacks documented).

**Rollback**

- Revert the commit; no persisted state depends on the new lanes.

## 2026-08-29 — VM2 V1: RuleRowV2/RuleTableV2 (table version 2), AiCmd kinds 10–11 (vm2-plan D-1…D-8)

**What changed**

- `core-types` gains the VM2 general-grammar types (vm2-plan §1/§3,
  design LOCKED 2026-08-29): `RuleRowV2` (128 B, two cache lines —
  feature/combine grammar, position mode, groups, confirm, min/max
  hold) and `RuleTableV2` (256 rows, 32 832 B), ADDITIVE beside the
  v1 types. The vm evaluator flips to v2 in V3, the validator + the
  §6 table-handoff ring in V4; the v1 `RuleRow`/`RuleTable` retire
  then (no unused code stays). v1 JSON artifacts keep committing
  through a compat arm for one release (D-6) — sugar maps onto v2
  rows with byte-exact v1 semantics, so H6-era artifacts and
  `cvfc-basis-kill` stay valid.
- `AiCmdKind` appends `FundingSeed = 10` (D-1: one historical funding
  print — sym, rate ×1e9 in `px`, venue print ms in `qty`) and
  `PositionSeed = 11` (D-2 as ruled: positions RESTORE at boot — row
  index in `param_id`, entered side, entry px in `px`, position age
  SECONDS in `qty`, `ttl_ns` 0-enforced: the drain site expires any
  nonzero ttl, and entry qty re-derives from the row's sizing law so
  restores respect current caps). Shape rules enforced by
  `AiCmd::validate_shape`; byte
  meanings pinned in `docs/wire-format.md`. Both are engine-directed
  (`venue = Ai`, `strategy_id = 5`). Capture-compatible: the 64 B
  AiCmd layout is unchanged, `ai-cmds.pmlr` readers see two new kind
  bytes.
- The funding cadence law gets its single home:
  `core_types::funding_print_divisor` (Deribit ÷8 — hourly samples of
  `interest_8h`) + `funding_period_s` (clock-feature fallback). The
  worker mirror (`claude_worker.carry_signal.apr_from_prints`) gains
  a pin test in V6.
- Refinement over the D-1 sketch (recorded in vm2-plan §8):
  FundingSeed carries RAW PRINTS with venue timestamps, not
  per-window aggregates — windows recompute engine-side through the
  same path live events take, keeping the cadence law in one place.

**Why**

- vm2-plan §0: the two-word v1 rule language cannot express the M5
  strategy families; v2 is the general, cron-free replacement. D-5
  ruled 128 B rows (the grammar does not fit 64 B).

**Impact**

- On-disk formats: none yet (RuleRow/Table never captured; AiCmd
  layout unchanged — only new kind bytes appear in `ai-cmds.pmlr`
  once V6 pushes seeds).
- Config keys: none.
- Wire formats: AiCmd kind table extended; RuleRowV2/RuleTableV2
  documented in `docs/wire-format.md`.

**Migration steps**

1. None until V3/V4 flip the evaluator/validator — this commit is
   type-additive and inert at runtime.

**Rollback**

- Revert the commit; no persisted state references the new types or
  kinds until V6.

## 2026-08-29 — SlotKind 7 = DepthTopK, first non-64-byte PMLR slot (WS10-B)

**What changed**

- PMLR slot size is now KIND-determined: kinds 0–6 keep 64 B; the new
  kind 7 (`DepthTopK`, 192 B = three cache lines) carries the WS10-B
  top-K depth snapshots in `<venue>-depth.pmlr` (opened for EVERY
  venue by the uniform-file-set law; header-only without a depth
  subscription). `PmlrReader` decodes the kind from the header FIRST
  and validates the caller's type/stride against it. Container
  version stays 2 — no pre-WS10 file changed shape.
- The engine gains two depth lanes (`Ring<DepthTopK, 4096>`, OKX +
  Deribit, `engine::depth_lane_of`) and the defaulted
  `Strategy::on_depth`; emission is change-gated in the ingress
  (`book_builder::ladder`, 64 levels/side); a seq-chain break emits a
  `flags = STALE` snapshot after clearing the ladder.
- WS10-A (same commit series, no wire change): venue-event lanes
  carry funding `ChannelEvent`s in-process (`EVENT_RING_SIZE` 1024,
  spawn-time `event_mask`, funding-only in v1) — the capture record
  IS the carrier, so nothing here migrates.

**Why**

- gaps-doc §1 / ws10-engine-plumbing-design.md, operator-approved
  D-A1..D-B3: funding and L2 depth reach `Strategy` without a second
  wire type; the 192 B slot keeps the top-5-per-side snapshot in one
  POD instead of splitting rows across 64 B records.

**Impact**

- On-disk: new `<venue>-depth.pmlr` files appear in every run dir
  from the first WS10 boot. Readers that assumed a flat 64 B stride
  must consult `SlotKind::slot_size` (in-tree readers updated;
  `claude-worker/pmlr.py` opens only tick/opt-summary files and is
  unaffected).
- audit-replay renders a per-venue `depth` stream section + totals
  (snapshots / syms / stale count) when records exist.

**Migration steps**

1. None for existing files. New binaries read old runs unchanged.

**Rollback**

- Pre-WS10 binaries ignore unknown kind 7 files (open fails with
  `UnknownSlotKind`; nothing else reads them).

## 2026-08-29 — VenueId 6 = Bybit + tick lane 6 (WS9, the sixth venue)

**What changed**

- `VenueId` gains `Bybit = 6` (append-only; `Ai = 5` keeps its
  discriminant — the lane↔venue identity is broken past lane 4 and
  `engine::tick_lane_of` is the mapping: Bybit rides TICK LANE 5).
- `NUM_TICK_LANES` 5 → 6; `TRADEABLE_VENUES` 5 → 6 with the new
  `tradeable_venue_byte` predicate (bytes 0..=4 and 6; Ai excluded);
  `ModelParams` tables widen to 7 slots (slot 5 = Ai, DEAD).
- New capture label `bybit` (`bybit-ticks/-events/...pmlr`),
  `bybit:` / `bybit-linear:` descriptor namespaces, `[bybit]`
  universe section (spot/linear; linear ordinal base 512), Config
  hosts `BYBIT_WS_HOST`/`BYBIT_REST_HOST`, metrics family
  `engine_ingress_bybit_*` + coverage gauge, TUI health bit 7.
- Worker: `VENUE_BYBIT = 6`, map seeding for `[bybit]`, candle lanes
  `bybit`/`bybit-linear` (kline REST), refdata tickers lane.

**Why**

- stage2-finish-plan WS9 / gaps-doc §1 — the sixth venue.

**Impact**

- On-disk formats: new per-venue capture files under the existing
  container version; ticks may carry venue byte 6. Pre-WS9
  `audit-replay` binaries skip the unknown label's files and treat
  venue byte 6 as corruption — decode with a post-WS9 binary.
- Config keys: `[bybit] spot/linear` (additive); two new optional
  env hosts.
- Wire formats: append-only enum growth on `VenueId`.

**Migration steps**

1. None until a `[bybit]` section is configured; the venue is
   entirely opt-in.

**Rollback**

- Safe while `[bybit]` stays empty (old binaries reject the section
  as an unknown-section parse error — remove it before rolling
  back).

## 2026-08-29 — ChannelId 12 = VolIndex (WS6 Deribit DVOL)

**What changed**

- `ChannelId` gains `VolIndex = 12` (append-only): the Deribit DVOL
  capture series. Field semantics in `docs/wire-format.md`: `sym` =
  `SYMBOL_ID_NONE` (venue-global), `v0` = volatility points ×1e9,
  `v1` = ordinal into the configured `[deribit] options_underlyings`
  list (DVOL subscriptions derive from that list — `BTC` →
  `btc_usd`; no new config key).
- DVOL channels ride the batched subscribe but sit OUTSIDE the
  subscribe-verification mask (its u128 is fully allocated) — an
  index the venue does not serve is a missing capture series, never
  a session verdict.

**Why**

- stage2-finish-plan WS6 / gaps-doc §1 "New data series": DVOL was
  absent from the repo entirely.

**Impact**

- On-disk formats: event logs may carry `channel = 12` rows. PMLR
  container version unchanged.
- Config keys: none (derived from options underlyings).
- Wire formats: pre-WS6 readers skip id 12 as a corrupt byte —
  additive, same posture as id 11.

**Migration steps**

1. None — decode with a post-WS6 binary.

**Rollback**

- Binary rollback safe: old readers skip id 12; old writers never
  emit it.

## 2026-08-29 — ChannelId 11 = SubDrop (WS2 non-fatal subscribe drops)

**What changed**

- `ChannelId` gains `SubDrop = 11` (append-only; nothing renumbered):
  the §6.6-paired evidence event for WS2's non-fatal subscribe drops
  on OKX/Deribit reconnect sessions. Field semantics in
  `docs/wire-format.md` (event-slot `channel` row): `sym` = dropped
  instrument or `SYMBOL_ID_NONE`, `v0` = venue error code (0 =
  missing-from-echo), `v1` = venue-local channel discriminant (−1 =
  unknown/folded).
- `IngressStatus` gains `sub_drops_total` (slot stays 128 B; mirrored
  as `engine_ingress_<venue>_sub_drops_total`).

**Why**

- Capture-continuity outage 2026-08-27 §5.2: venue errors / missing
  echo channels on reconnect killed whole sessions for six days.
  WS2 makes them per-instrument drops; every drop must stay visible
  offline (counter ↔ event pairing, the TradeGap/BookGap precedent).

**Impact**

- On-disk formats: event logs written by post-WS2 binaries may carry
  `channel = 11` rows. PMLR container version unchanged.
- Config keys: none.
- Wire formats: pre-WS2 readers (`ChannelId::from_u8`) treat 11 as a
  corrupt byte and skip the row — old `audit-replay` binaries
  under-report only the new event class, nothing else.

**Migration steps**

1. None operationally — the id is additive. Decode with a post-WS2
   binary; the worker's `pmlr.py` channel map picks the id up in its
   WS11 fold-in.

**Rollback**

- Binary rollback is safe: old readers skip id 11; old writers never
  emit it.

## 2026-08-23 — Order slot: `strategy_id` at offset 41 + the `engine-orders.pmlr` intent log (M4.1)

**What changed**

- `Order` (64 B ring/capture slot) claims ONE byte of `_pad1`:
  offset 41 = `strategy_id: u8` (strategy-set slot ids; `0xFF` =
  unattributed), `_pad1` shrinks 15 → 14 B. Stamped by the set's
  `StampCtx` adapter around every member callback; bare
  single-strategy boots leave `0xFF`.
- NEW engine-side capture file `engine-orders.pmlr`
  (`slot_kind = 3`, `core_io::SlotCapture<Order>`): every order the
  dispatcher ACCEPTED via `ctx.submit`, staged on the engine thread
  next to `engine-fills.pmlr`. Dispatcher refusals remain counters
  only.

**Why**

- mvp-plan §4-M4 (shadow-P&L): the M4.1 audit found paper mode
  captured NEITHER intents nor fills (`PaperDispatcher` counts and
  drops; `SlotKind::Order` was defined but wired to nothing) — the
  per-strategy intent log is the enabling substrate for `audit-pnl`
  (§9.9 "logged intents"). Per-ruleset attribution deliberately
  rides the existing ai-cmds `RulesetCommit` timeline (hash128 in
  px/qty), NOT a wider Order slot.

**Impact**

- On-disk formats: a NEW per-run file; PMLR container version stays
  2 (append-only kind usage). No historical Order file exists —
  the layout amendment is PRE-FIRST-CAPTURE and has zero
  reader-compat surface. Old binaries reading a new run dir simply
  never open the file; `audit-replay`/catalog treat it as another
  size-visible capture file.
- Config keys: none.
- Wire formats: `Order` table amended in `docs/wire-format.md`
  (offset 41). In-process ring consumers are unaffected (field was
  explicit zero padding; `Order::new` initializes `0xFF`).

**Migration steps**

1. Nothing operator-side: the file appears on the first boot of a
   binary carrying M4.1; older run dirs simply lack it (audit-pnl
   reports them intent-less).

**Rollback**

- Revert the commit; run dirs written meanwhile carry an extra
  `.pmlr` file old code never opens. Harmless.

## 2026-08-22 — SlotKind 6 (OptSummary): the options analytics capture channel (M2.3)

**What changed**

- New PMLR `slot_kind = 6` — `OptSummary` (64 B, layout pinned in
  `docs/wire-format.md`): mark px / mark IV / BS greeks / open
  interest / underlying px per option instrument, fed by Deribit
  option `ticker.{instr}.100ms` and OKX `opt-summary` (BN eapi at
  M2.4).
- `core_io::PmlrCapture` opens a FOURTH per-venue file,
  `<venue>-opt-summary.pmlr`, for EVERY venue (header-only where no
  options lane exists — the same uniform-file-set law as
  `<venue>-signals.pmlr`).
- PMLR **version stays 2** — this is an append-only SlotKind addition;
  no existing slot layout changed.

**Why**

- mvp-plan §4-M2.3/§9.8: one new capture record on the append-only
  raw-store doctrine; the strategist digest and audit read it offline.

**Impact**

- On-disk formats: run dirs gain `<venue>-opt-summary.pmlr` per venue.
  READER COMPAT: pre-M2.3 readers never open the new file (separate
  name) and are unaffected; `SlotKind::from_u8(6)` decodes only in
  M2.3+ binaries — an OLD binary reading a NEW file's header reports
  unknown-kind corruption, which is correct-and-loud, and never
  happens through the shipped tools (they open files by name/kind).
  Old run dirs (no opt-summary files) audit exactly as before —
  audit-replay treats the file as absent.
- Config keys: none (the options lanes were M2.1/M2.2 config).
- Wire formats: new `OptSummary` section in `docs/wire-format.md`;
  Deribit option rows now subscribe `ticker` in addition to `quote`
  (subscribe-verification folds both into one per-row bit); OKX
  gains the family-keyed `opt-summary` subscription (2 args).

**Migration steps**

1. None — capture stays append-only; new files appear on the first
   M2.3 boot.

**Rollback**

- Boot the previous binary: new files stop being written; existing
  ones remain readable by M2.3+ tools and ignorable garbage-by-name
  to older tools.

## 2026-08-15 — SlotKind 5 (ChannelEvent), capture files, raw tap (Phase 8e)

**What changed**

- New PMLR `slot_kind = 5` — `ChannelEvent` (64 B, layout in
  `docs/wire-format.md`): non-tick channel capture. `slot_kind = 4`
  is **reserved** for Stage-2 `AiCmd` (plan §8.4) and still decodes as
  invalid.
- PMLR replay capture is now actually wired into the shipped `run`
  path (the 8e defect fix): each ingress thread writes
  `<venue>-ticks.pmlr` / `<venue>-events.pmlr` / `<venue>-signals.pmlr`
  into `<MULTIVENUE_LOG_DIR>/run-<epoch_ns>/`.
- New sidecar format `<venue>-raw.tap` (`b"PMRT"` v1) — bounded raw
  payload capture behind `--raw-tap`, off in production.
- `MULTIVENUE_LOG_DIR` now tilde-expands a leading `~/` at boot.

**Why**

- Plan §6.5: the replay logs are the 8h backtest dataset and the
  `audit-replay` input; §6.6 G1 soaks are judged from them.

**Impact**

- On-disk formats: existing v2 Tick/Signal/Fill/Order logs unchanged
  and fully readable. Binaries at or before 8d.1 refuse `slot_kind=5`
  files (unknown kind = corruption by their rules) — expected.
- Config keys: `MULTIVENUE_LOG_DIR` semantics extended (tilde
  expansion); no new keys for capture itself. Tap is flag-driven.
- Wire formats: ring slots untouched — `ChannelEvent` exists only in
  PMLR files, never in rings.

**Migration steps**

1. Nothing for existing logs.
2. Tooling that globs `*.pmlr` should route on the header `slot_kind`
   byte (0/1/2/3/5) rather than assuming Tick.

**Rollback**

- Delete `run-<epoch_ns>/` capture directories; pre-8e binaries never
  read them.

## 2026-08-14 — PMLR v2: venue bytes + explicit padding (Phase 8a)

**What changed**

- `PMLR VERSION` bumped 1 → 2.
- `Tick` gains `venue: u8` at offset 48 (was implicit padding);
  `Order` gains `venue: u8` at offset 40 (was `_pad1[0]`). Values are
  `VenueId`: Polymarket=0, Binance=1, Okx=2, Deribit=3, Hyperliquid=4,
  Ai=5.
- All padding in all four slots is now explicit and zeroed. v1 writers
  emitted 8 undefined tail-padding bytes per slot (D9).
- `SymbolId` is venue-namespaced: bits 31..24 = venue, bits 23..0 =
  per-venue ordinal.

**Why**

- Phase 8 multivenue expansion needs venue identity on every tick and
  order; the `AsBytes` zero-copy log contract requires fully
  initialized slots.

**Impact**

- On-disk formats: v1 logs remain readable (`PmlrReader` accepts
  version ≤ 2, exposes `version()`). v1 files are **venue-less**: the
  byte at Tick offset 48 / Order offset 40 is undefined garbage in v1
  and must be ignored when `version() == 1`. Venue cannot be inferred
  from v1 slots (slot kind + sym do not disambiguate Polymarket vs
  Binance ticks).
- Config keys: none in this entry (per-venue symbol flags land with
  the venue ingress phases).
- Wire formats: ring slots and PMLR slots share the new layout;
  `docs/wire-format.md` is the byte-level source of truth.

**Migration steps**

1. Nothing to do for live capture — new logs are v2 automatically.
2. Backtests mixing v1 + v2 logs must branch on `PmlrReader::version()`
   and treat v1 venue bytes as absent.

**Rollback**

- Binaries at or before Phase 7 refuse v2 logs (`version > 1`); keep
  v1 archives if a rollback below Phase 8a is contemplated.

## 2026-04-19 — Phase 0 scaffold initial wire format

**What changed**

- Introduced the Phase 0 wire format documented in `docs/wire-format.md`:
  `Tick`, `Signal`, `Fill`, and `Order` are all 64-byte cache-aligned POD
  structs. Replay-log header version pinned at `1`.

**Why**

- First commit — establishes the baseline that every subsequent migration
  will bump against.

**Impact**

- On-disk formats: replay log header magic `b"PMLR"`, version `1`.
- Config keys: see `config.example.toml`.
- Wire formats: as documented in `docs/wire-format.md`.

**Migration steps**

1. None — fresh install.

**Rollback**

- Remove `~/multivenue/replay/` and `~/multivenue/artifacts/`.
