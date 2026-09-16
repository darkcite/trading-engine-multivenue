# Risk policy

This document is the source-of-truth for every position cap, loss threshold,
and kill-switch trigger in the engine. Any change to it **requires** a
corresponding change to the code in `crates/strategy-*`, `crates/engine`,
or `crates/clob-dispatcher` — and vice versa. The `risk-reviewer` subagent
under `.claude/agents/` will block merges that update one without the other.

## Absolute caps — $50k-book RESEARCH tier (operator ruling 2026-08-29)

Strategies assume a **$50,000 book** in M5 paper research; the DD line
is the operator's 15%-of-book ruling. Enforced in: the §4.2 ruleset
validator rule 7 (`ingress-ai::ruleset`), the VM emit-time clamp
(`strategy-vm::POLICY_SINGLE_ORDER_CAP_1E6`), the backtest/audit-pnl
fill law (`cli::backtest::fill` — open-order caps + caps-rejections),
and the worker gate thresholds (`claude_worker.backtest.GateThresholds`,
frozen-surface amendment with this ruling cited in the pin tests).

| cap                          | value     | enforced in                          |
| ---------------------------- | --------- | ------------------------------------ |
| max open orders per symbol   | 8         | `cli::backtest::fill` (paper model)  |
| max total open orders        | 64        | `cli::backtest::fill` (paper model)  |
| max net notional per symbol  | $20 000   | rule 7 + gates (RiskGate at 8i)      |
| max net notional total       | $100 000  | rule 7 + gates (2× book gross)       |
| max single-order notional    | $10 000   | rule 7 + VM clamp + gates            |
| max OOS drawdown (gate)      | $7 500    | `GateThresholds` (15% of book)       |

### Deribit is capped in COINS, not dollars (operator amendment, 2026-09-10)

| cap                        | Deribit   | every other venue |
| -------------------------- | --------- | ----------------- |
| max single-order size      | **1 coin**| n/a               |
| max net per-symbol size    | **1 coin**| n/a               |
| max single-order notional  | n/a       | $10 000           |
| max net notional per symbol| n/a       | $20 000           |
| max net notional total     | **$250 000** | $100 000       |

**Why a different unit.** A Deribit inverse contract IS one whole coin:
its size is a coin count and its dollar value moves with the index.
Capping it in dollars means the permitted size shrinks as the coin
rises, so the member starts refusing at a price nobody chose. Capping it
in coins is the venue's own unit and does not move.

**The one table.** Both figures live in `strategy_core::{CAPS_BASE,
CAPS_DERIBIT}` and are read through `caps_for_venue` / `caps_for_sym`.
`strategy-vrp` and `strategy-icdp` both read that table — a second
order-submission path that runs to its own numbers is a hole in this
document, not a new strategy. `0` in a unit means **"this member cannot
express this venue's cap"**, never "unlimited": `strategy-icdp` sizes
every position in dollars, so it REFUSES a Deribit instrument at
`configure` rather than reading Deribit's `leg_usd_1e6 == 0` as no
limit. Giving icdp a Deribit lane means giving it the index and
converting.

**Deliberately NOT amended: the VM clamp and the ruleset validator.**
`strategy_vm::POLICY_SINGLE_ORDER_CAP_1E6` and
`ingress_ai::{RULE_ROW_MAX_RISK_1E6, RULE_SYM_MAX_RISK_1E6,
RULE_TABLE_MAX_RISK_1E6}` keep the base $10k / $20k / $100k lines. They
are an independent, tighten-only defence layer over AI-PUSHED rows —
hash-pinned rulesets and a proptest depend on those numbers — and the
amendment was asked for on the coded VRP lane. **An AI-pushed VM row on
a Deribit option is therefore still capped at $10 000.** Raising that is
a separate ruling with its own re-pinning.

**How `strategy-vrp` applies it.** Two things are specific to the member:

* **It gates on the WORST case, not the current one.** The member's
  hedge is sized off the option's delta, and delta walks to 1 as the
  option goes in the money — so an entry is authorised only if the
  position it commits to is still legal at Δ = 1, where the hedge is
  exactly `qty_1e6` coins. Refusing at the decision is the only
  fail-closed choice: entering and clamping the hedge later would leave
  a naked option position wearing a hedged one's name. Refusals are
  counted (`engine_vrp_caps_rejected_total`).
* **The shipped size sits precisely ON the line.** `vrp.toml`'s
  `qty_1e6 = 1000000` is one contract = one coin = the per-order cap, so
  the edge spec's measured configuration runs unmodified.
  `core_config::vrp` refuses anything above one contract at boot; the
  runtime gate, which knows the venue and the side, refuses the rest.

A cap never blocks an EXIT: the member's unwind path and any hedge move
that REDUCES the position are exempt, the same exemption
`strategy-icdp`'s `exit_position` has. A cap that can stop a position
being closed is not a risk control.

Paper mode treats every fill as real for P&L accounting so the caps are
exercised on the same code path that live mode will use. Statistical
gates (OOS > 0, ≥ 50 trades, ≥ 1 trading day) are scale-independent.
**The trading-day floor moved 2 → 1** under the MVP-tempo operator
ruling of 2026-08-30 (`GateThresholds.min_trading_days`, D1-pattern
frozen-surface amendment, cited in the pin tests) so a ~12 h capture age
suffices for staging; the accepted trade-off is that an OOS verdict can
come from a single day's regime — the old floor was the
single-regime-overfit guard. **Superseded 2026-09-05 by the ≤ 2 h
law + the regime lane:** evidence is now a COUNT of disjoint complete
≤ 2 h windows pooled and judged leave-one-window-out (never days —
`docs/regime-and-dashboard-plan.md` §7.1), and the single-regime
overfit is guarded by the regime GATE itself: a labelled row trades
only in the words it was evidenced in, UNKNOWN fails it closed, and
its label must earn the `--regime off` delta.

## Regime gate (RG0–RG7, 2026-09-03 →) — a gate, never a signal

`docs/regime-and-dashboard-plan.md` §2 is the doctrine; the risk-relevant
laws, enforced in `crates/core-regime` (the detector), `crates/strategy-set`
(per-member gates), `crates/strategy-vm` (per-row gate bytes) and
`ingress-ai::ruleset` (validator rule 11 + the rule-8 amendment):

- **Entries only.** A closed gate blocks ENTRIES; it never blocks an
  exit and never flips a table. `off = soft` lets the position drain by
  its own exit law; `off = hard` flattens on the flip
  (`engine_vm_regime_hard_exits_total`, `engine_icdp_regime_exits_total`).
- **Fail closed.** UNKNOWN words (warm-up, a declaration that expired,
  a venue-dark FUND dimension) close every LABELLED row/member; an
  unlabelled row is bit-identical to pre-RG0 behaviour.
- **Declarations are bounded.** A `SetRegime` frame carries a TTL
  (`ttl_ns = 0` refused at the shape check); after it expires the
  engine's own measurement rules. Declarations never bypass caps.
- **No flicker.** Hysteresis bands + `confirm_min = 3`; the RG7 soak
  bounds flips to ≤ 2 per profile × dimension per ≤ 2 h window from the
  engine's own counters (`python -m claude_worker.regime soak`).
- **Read-only observability.** `/state` (9191), the TUI and the
  dashboard page (9292) carry no control into the AI plane —
  enable/disable/declare/halt stay verbs and frames under the
  single-writer seq law.

**Superseded demo tier (Phase 0 → 2026-08-29):** 4/sym · 32 total ·
$250/sym · $1 000 total · $100/order · $200 DD — the numbers every
pre-M5 backtest report and audit was measured against; historical
reports keep meaning under the tier that produced them. Any LIVE
(Stage-3) tier is a separate future ruling — these are research caps.

## Kill-switch triggers

The engine unconditionally flips to **halt** (no new orders, cancel all
open, stop ingesting rules) on any of the following:

1. Realized-loss cap breached in a rolling 24 h window.
2. Unrealized-loss cap breached per symbol.
3. A fill arrives for a `client_oid` the engine does not have on record.
4. Two consecutive WS reconnect failures to the Polymarket CLOB within 60 s.
5. `core-alloc::CountingAllocator` reports any allocation in a tick-loop
   iteration. (debug builds only; release aborts via `panic = "abort"`.)
6. A `debug_assert!` fails in a strategy's `on_tick`/`on_signal`/`on_fill`.
7. **(E6, NOT YET ENFORCED)** `halt_on_reject_streak` consecutive venue
   rejections on a live slot ⇒ sticky per-slot halt.
8. **(E6, NOT YET ENFORCED)** reconciliation drift between our position and
   the venue's above `halt_on_recon_drift_usd_1e6` ⇒ sticky per-slot halt.

Triggers 7 and 8 are PARSED from `exec.toml` today (E1) and enforced by
nothing. E6 adds the enforcement, cancel-all-on-halt and the
operator-restart requirement; until then neither number is a control.

Halt is **sticky**: it requires a manual engine restart. No "auto-resume"
logic is permitted — a halted engine means a human investigates.

### Member-scoped halt: VRP kill criterion 3 (2026-09-10)

`strategy-vrp` (slot 1) carries its own sticky halt, scoped to the
member rather than the engine, because what dies is one mechanism and
not the process:

7. **The VRP forecast stops beating implied vol.** Over a FULL trailing
   60 settled expiries, the member's own QLIKE is no longer below the
   venue's implied-vol QLIKE ⇒ the member halts: no new entries, ever,
   until a restart. Open campaigns still unwind through their normal
   exit law — a halt must never strand a position.

This is pre-registered kill criterion 3 of the edge spec: E1 (short-dated
implied vol is beaten by a plain fitted HAR) is the entire mechanism the
lane monetises, and a member that keeps selling premium without it is
not running the strategy that was measured. Enforced in
`strategy_vrp::VrpStrategy::refresh_qlike`; visible as
`engine_vrp_killed` (gauge, 1 = halted) beside
`engine_vrp_qlike_iv_1e6` / `engine_vrp_qlike_har_1e6`. The halt is
armed only on a full window — a half-filled comparison is not evidence
that a mechanism has died.

**The halt survives a restart (V8a, 2026-09-10).** The QLIKE window and
the armed flag are written to `~/multivenue/vrp-state.tsv` by the engine
and replayed at boot, so the sixty-expiry window accumulates across the
five scheduled restarts a day it takes twenty days to fill, and an armed
halt is NOT cleared by one. `vrp: kill criterion 3 was ARMED before this
restart` is logged at every boot that comes up halted. Clearing it is an
operator act — delete the state file — which is the manual step the
section above requires, not an auto-resume.

> **One item remains before V8.** `engine_vrp_killed` needs an
> operator-facing ALERT, not just a gauge: a halt nobody is paged for is
> a halt discovered in a weekly report. Until that alert exists, reading
> `engine_vrp_killed`, `engine_vrp_qlike_iv_1e6` and
> `engine_vrp_qlike_har_1e6` is an operator obligation on top of the
> engine control.

### VRP member alerts (2026-09-12)

Three VRP counters are ALERTS, not observability. Each names a state the
engine cannot fix for itself, and each is printed on the TUI's `vrp:`
line only when it is non-zero.

| counter | what it means | what to do |
|---|---|---|
| `engine_vrp_hedge_abandoned_total` | a hedge target was given up on after `HEDGE_RETRIES_MAX` — **the book is not at its delta target and no order is chasing it** | flatten or hedge the slot 1 perp position by hand, then read `engine_vrp_hedge_unfilled_total` and the venue's book depth to see whether the size is simply too large for the touch |
| `engine_vrp_entries_unfilled_total` | an option entry met no fill by its deadline. ONE is routine at a maker entry (`entry_mode = maker`); a run of them means the member is resting where nobody trades | compare with `engine_vrp_entry_maker_submitted_total` and `engine_vrp_entry_crossed_total`. All three moving together is the fallback working; `entries_unfilled` alone climbing with `entry_fallback = abandon` is a campaign lost every day |
| `engine_vrp_settled_unpriced_total` | an ITM expiry settled with no symbol to price it — **the value was NOT recorded** | reconcile that expiry by hand from the state-file backup's `C` row (see the F26 entry in `docs/local-setup.md`) |

Two more are diagnostics rather than alerts, and are worth a weekly
read: `engine_vrp_holds_cost_total` (HOLDs that θ alone would have
traded — the fee load, measured) and
`engine_vrp_settle_index_fallback_total` (settlements priced off the
last print because the 30-minute delivery window was thin).

## A venue outage must not take the engine down (operator ruling 2026-09-15)

A Deribit `system_maintenance` window on 2026-09-15 took the ENTIRE engine
down for roughly ten minutes, and would have kept it down indefinitely. Two
distinct faults, one ruling each.

### 1. The VRP member is DROPPED when the venue supplies no chain

`crates/cli/src/vrp_boot.rs` used to return one error type for two very
different things. It now returns two:

| cause | meaning | consequence |
| --- | --- | --- |
| `VrpBootError::Refused` | `vrp.toml` is missing where it was named, unreadable, or internally wrong | **REFUSES the boot.** F19 stands: an operator asked for a member and the engine cannot guess what they meant. |
| `VrpBootError::VenueChainEmpty` | the artifact is good; the VENUE published no options chain | **DROPS slot 1 and boots everything else.** |

**This is a deliberate, operator-ruled departure from the F19
requested-but-absent law, and it is narrow on purpose.** F19 exists so nobody
watches a member that was never there — a human error the human must fix.
A venue in maintenance is not that: nothing a human did, nothing a human can
fix, and it clears by itself. Refusing there takes down xsd, bin15, ai and vm
— four healthy members and all six venues' capture — because a fifth member's
exchange is offline.

**The drop is never silent**, which is the only reason it is safe:

* an **ERROR**-level tell naming the reason in full, what is given up (slot 1
  will not trade for the life of the run — the chain is discovered once, at
  boot) and what brings it back (the next restart, if the venue has recovered);
* `/state` shows it machine-readably: **`requested_mask` keeps the vrp bit**
  (it IS what was asked for) while **`enabled_mask` does not**. Since the only
  other cause of an absent vrp aborts the boot outright, that divergence is
  unambiguous.

The distinction is carried by a TYPE, not by matching on a message string, so
it cannot be inverted by a typo; `From<String>` defaults to `Refused`, so a
future bare-string error inside that module refuses rather than silently
becoming a drop.

**This ruling covers VRP only.** No other member may drop itself on a venue
condition without its own entry here.

### 2. A boot abort must reach exit

`join_reverse` joined threads that had never been asked to stop. The ~30
boot-abort sites hit a refusal, called it, and blocked forever: the process sat
at 15–21 % CPU for **seven minutes** and never exited, so launchd KeepAlive
could not relaunch it and the engine stayed down long after the venue had
recovered. `/state` was already serving (the metrics thread starts before the
engine loop), so a monitor saw a live-looking engine reporting a zeroed boot
block — `pid 0`, `paper 0`.

`join_reverse` now signals `SHUTDOWN` before joining — once, in the one place
the 31st abort site cannot forget — and arms a watchdog that force-exits with
`EXIT_JOIN_TIMEOUT` (75) after `JOIN_GRACE` (20 s) if a thread ignores the flag.

**The process MUST reach exit. A supervisor can only restart a process that
dies.** Fail-fast beats a graceful wait that never ends — and because KeepAlive
relaunches on a clean exit, a full venue outage now self-heals: the engine
retries every boot cycle until the venue is back, with no degradation and no
policy change.

## Per-strategy execution routing (E1, 2026-09-15) — DECLARED, not ENFORCED

Execution mode moved from one process-wide `--paper` / `--live` flag to a
**per-slot table keyed on `Order.strategy_id`** (`crates/exec-router`,
artifact `~/multivenue/exec.toml`, grammar in `exec.toml.example`). Three
modes: `paper` (the default for every unanswerable question), `live`,
`off`.

**Nothing in E1 can place a real order.** `cli::exec_boot::LIVE_ARM_VENUES`
is empty and a compile-time assertion holds it empty, so any artifact
marking a slot `live` REFUSES the boot and names the phase that will
supply the arm.

*(Amended 2026-09-15, E4: this paragraph used to say "no live
execution arm is compiled for any venue". That is no longer true —
`exec_hyperliquid::HlExchange` IS compiled into the binary. What keeps
it unreachable is that nothing constructs it, plus the empty const.
The barrier holds, but it is now ONE barrier where it used to be two,
which is exactly what `exec_boot.rs` warned would happen.)*
Arming needs two switches that must name the same set exactly,
`exec.toml` and `--arm-live`, plus — in the managed fleet — a third edit
to `scripts/engine-wrapper.sh`, which passes `--paper` and never
`--exec`. `--exec` has no default path, deliberately.

**LAW E-1 — a live slot never falls back to paper.** A live slot whose
order names a venue it has no route to is refused
(`DispatchError::NoLiveRoute`) and counted, never handed to the paper
matcher. A modelled fill wearing live semantics is a trade that never
happened entering the P&L.

**LAW E-2 — `matcher_counters()` and `open_paper_orders()` report the
PAPER arm only**, so `/metrics` can never suggest the matcher is
modelling something a venue is really doing.

**Fail-closed.** An absent artifact, an absent slot, an out-of-range
`strategy_id`, `STRATEGY_ID_NONE` (`0xFF`) and any unknown mode byte all
resolve to `paper`. An absent artifact is every slot paper, which is the
engine's behaviour before E1, bit for bit.

### The E1 caps are a DECLARATION. Nothing clamps to them.

`exec.toml` carries per-slot `max_order_usd_1e6`, `max_open_orders`,
`cap_day_usd_1e6`, `cap_instance_usd_1e6`, `request_budget_floor`,
`halt_on_reject_streak` and `halt_on_recon_drift_usd_1e6`. In E1 they are
PARSED, bounds-checked at boot and published in the boot tell — and
**enforced by nothing**. Enforcement lands in E6 (the risk gate), as
clamps inside `RoutedDispatcher::submit` computed from VENUE fills rather
than from the member's own position view.

Until E6 ships, the only limits in force on any slot are the **member's
own**, from its own artifact (`bin15.toml`). The boot tell prints
`caps-DECLARED-NOT-ENFORCED` and a following WARNING line for exactly
this reason; when E6 lands the token becomes `caps-ENFORCED` and the
warning is deleted.

**These caps are per SLOT, not global.** `max_open_orders = 64` on a slot
is not the global "max total open orders 64" above — eight slots at 64 is
512. The global line still binds, and E6 must enforce both.

**`0` means UNSET, and unset is NEVER "unlimited"** — the same ruling this
document already made for the Deribit coin caps. A live slot with a zero
`max_order_usd_1e6` or `cap_instance_usd_1e6` is refused at parse.

**Two cap tables now carry the same names.** `bin15.toml`'s are enforced
by the member; `exec.toml`'s are the independent second opinion E6 will
enforce over it. If they ever disagree, that disagreement is itself the
alarm — E6 refuses the boot on a mismatch.

### Operator ruling O-E4 (2026-09-15) — the live-ramp caps

Per-slot caps for slot 3 (bin15, Hyperliquid), as shipped in
`exec.toml.example`:

| cap | value | status in E1 |
| --- | --- | --- |
| max single-order notional | $100 | declared |
| max open orders (this slot) | 64 | declared |
| day turnover | $30,000 | declared |
| per-instance | $1,000 | declared |
| request-budget floor | 2,000 requests | declared (E4) |
| halt on consecutive rejects | 5 | declared (E6) |
| halt on reconciliation drift | $5 | declared (E6) |

Empirical basis: the caps are **UNCHANGED** from the paper configuration
the member was measured under — `bin15.toml` carries the identical
`cap_instance_usd_1e6` and `cap_day_usd_1e6`. Ruling O-E4 is that the
live ramp throttles by WHICH ARMS ARE ENABLED, never by re-cutting a cap,
so no cap is widened by going live. A day cap is a TURNOVER limit, not a
funding requirement.

### `off` is a STOP, and it stops exits too

`mode = "off"` refuses EVERY order from the slot, entries and **exits**
alike: `Order` carries no reduce-only bit today, so the dispatcher cannot
tell them apart. It also does not cancel orders already resting.

This is a deliberate, documented **exception** to the standing law above
that a cap never blocks an exit. `off` is not a cap — it is an operator
stop, and it is the only control in this file that can strand a position.
Members clear their logical position when they submit an exit regardless
of the result, so flipping a holding slot to `off` silently
desynchronises the member from the book.

**Use `off` only on a flat slot.** To stop a slot that holds a position,
set it to `paper` (or stop the engine) and let the member's own exit law
unwind. The reduce-only exemption lands with E6.

### The legacy `--live` flag is outside the interlock

`--live` still reaches the real signer and the real Polymarket CLOB
through `boot_queued_live` on ONE switch, while the new path requires
two. `--live` now `conflicts_with = "exec"`, so the two can never be
combined, but the asymmetry is recorded here deliberately rather than
left implicit: what keeps the legacy path unreachable in the managed
fleet is `scripts/engine-wrapper.sh`'s strategy allow-list and its
hard-coded `--paper`, not the flag's own design.

## Signing-key handling

- The EIP-712 signing key is loaded from the process environment, which
  the wrapper populates from `.env` (project root, or
  `~/multivenue/.env` for the managed fleet). **No code opens that file
  to read a key** — code that did would be one refactor away from
  logging it.
  *(Amended 2026-09-15, E3: this bullet used to say "project-root
  `.env` only". The managed fleet has sourced `~/multivenue/.env` since
  the launchd lane existed, so the old wording described a setup that
  had not been true for some time.)*
- Boot-time: the key is `mlock`'d into its own page (see
  `crates/core-config::SecretKeyBytes`).
- Drop: the key page is zeroized and `munlock`'d.
- Debug: the `Secrets` struct has a custom `Debug` impl that redacts the
  key. Any code that formats or logs the key without redaction fails the
  `risk-reviewer` subagent check.
- **There is more than one signing key.** Since E3 the Hyperliquid
  agent wallet lives in `exec_hyperliquid::HlConfig`, and it uses the
  SAME `core_config::SecretKeyBytes` — the same `mlock`, the same
  zeroize-on-drop — rather than a second implementation. Two
  implementations of a key's memory handling is one more than can be
  audited, and E4 puts a MAINNET key in that struct.
- The intermediate hex `String` the environment hands us is zeroized
  after parsing. Without that, the key sits in freed heap for the life
  of the process.
- **No error message echoes the VALUE of a variable** — only its name,
  and for the source variable its length. The likeliest operator
  mistake is pasting a key into the wrong variable, and an error that
  helpfully quoted it would put the key in the launchd log.
- The limit of all of this, stated so nobody over-reads it: the value
  is also in the process `environ` block, which we do not own and
  cannot erase. Zeroizing our copies is defence in depth, not a claim
  that the key is unreachable from a core dump.

## Phased loosening

The caps above are **Phase 0** (paper, local). They are deliberately tight
— we run thousands of ticks through the engine at these levels before
widening anything. Each subsequent phase (see `PLAN.md`) requires:

1. A P&L report from the prior phase showing the caps were not the bottleneck.
2. A written risk note in `docs/risk-policy.md` (this file) describing the
   new caps and the empirical basis for them.
3. Sign-off from the operator (the human, not the engine).

No exceptions. A cap change without the above is a kill-switch-trigger bug
waiting to happen.

The E1 execution-routing phase (2026-09-15) satisfies precondition 2 by the
"Per-strategy execution routing" section above; preconditions 1 and 3 are
satisfied by ruling O-E4, which holds the caps at their measured paper
values rather than widening them. **No subsequent E-phase may arm a venue
without its own entry here first.**

The E3 exchange-arm phase (2026-09-15) satisfies precondition 2 by the
"Hyperliquid exchange arm" section below. It widens nothing, so
preconditions 1 and 3 do not arise: ruling O-E4's caps are untouched,
`exec.toml.example` is unchanged, and kill-switch triggers 7 and 8
remain PARSED-not-ENFORCED.

## The Hyperliquid exchange arm (E3, 2026-09-15) — BUILT, not ARMED

E3 builds the thing that can send an order and does not connect it to
anything that would. It adds a mio + rustls `POST /exchange` client, the
JSON request bodies, a fail-closed scanner for the venue's answers, the
host/source interlock, and a testnet gate that runs before every armed
restart.

**No slot can be armed.** `cli::exec_boot::LIVE_ARM_VENUES` is still
empty, and the compile-time assertion that holds it empty is still
there — E3 did **not** delete it. Deleting that assertion remains the
reviewable moment, and it belongs to E4.

### The five mechanisms that keep this off mainnet

Named individually, so that a future edit to any one of them is
visibly a policy change and not a refactor:

1. **The guard.** `exec_hyperliquid::smoke::run` refuses unless
   `HlConfig::is_testnet()`, which requires BOTH the testnet host and
   the testnet `source` — two independent fields. It is the first
   statement in the function, before the key is parsed or a socket is
   opened.
2. **Disjoint credentials.** The gate reads `HYPERLIQUID_TESTNET_*`,
   which the live arm never reads, and vice versa. This is not
   tidiness: if the gate shared `HYPERLIQUID_EXCHANGE_HOST` with the
   live arm, then the day a slot is armed — and that host becomes
   mainnet — the gate would begin refusing itself on its own guard and
   block every restart, at exactly the moment it first carries weight.
   An operator facing a permanently red gate removes the gate.
3. **The host/source interlock.** `HlConfig::new` refuses a mainnet
   host with a testnet source and a testnet host with a mainnet source.
   The loud failure it prevents is a 100 % rejection rate; the quiet
   one, which is worse, is a testnet `source` pointed at the mainnet
   host.
4. **The empty `LIVE_ARM_VENUES`**, above.
5. **The signature is computed over the testnet `source` byte.** Even a
   redirected socket would carry a signature that recovers to a
   different address on mainnet, so the probe is not replayable to
   production. This is the strongest of the five and the only one that
   holds without trusting any host string.

### The smoke is not an order path

It signs exactly one action type: a `cancel`. **A cancel can only
reduce exposure, never open a position.** It needs no balance, no
faucet and no registered agent.

Note what the justification is NOT: the probe names order id 1, and
order ids are a global sequence, so id 1 did exist at genesis. "The oid
cannot exist" would be a false argument. The true one is the paragraph
above, plus the fact that the account is a separate testnet account.

### What the gate proves, and what it does not

The gate has two halves and they cover different things.

- **Offline** (`exec-smoke.sh --offline`, no network, no credentials):
  the binary reproduces all 25 known-answer vectors the official
  `hyperliquid-python-sdk` generated, and rebuilds one action per TYPE
  from inputs — `order`, `cancel`, `cancelByCloid`, `batchModify` —
  demanding the SDK's exact msgpack. **This is the half that covers LAW
  E-3 for orders.**
- **Venue** (two signed POSTs): the venue verifies a good signature and
  rejects a corrupted one.

The distinction matters because msgpack key order is **per action
type**. The venue half signs a cancel, so it structurally cannot see a
reordering of `OrderWire`'s keys — a rebuild that broke every order
this engine sends would leave the network probe perfectly green. That
is why the vectors are embedded in the binary (`exec_hyperliquid::
selftest`) rather than left to `cargo nextest`: the release artifact
certifies itself, rather than being certified by a test suite that may
not have been run against it.

**The gate vets an ARTIFACT, not a moment.** "The release binary
passed" is a claim about a FILE, so the file is what is recorded — its
sha256, in `state/exec-gate-vetted-sha256`. Two things follow:

- **A rebuild invalidates the pass immediately.** The digest no longer
  matches, so the next fired slot re-proves the NEW artifact against
  the venue rather than inheriting a verdict earned by the old one.
- **An unchanged binary is not re-probed.** Five restart slots a day
  were spending ten signed POSTs re-proving the same bytes; now they
  spend two, once, and a 24-hour TTL re-proves it daily regardless.
  The venue's request budget is address-based and finite, so a gate
  that asks the same question ten times a day is spending the budget
  it depends on.

What this still does not close: between the drain and the wrapper's
`exec` there are seconds in which a build could land. Closing that
means the WRAPPER verifying this digest before exec, and the wrapper is
the file that arms the fleet — E4's change, not one to make by
inference now. The digest is recorded so E4 has something to check
against.

### A new operator control that can stop the production engine

`scripts/daily-restart.sh` will **defer the restart** when
`~/multivenue/exec.toml` names a live slot and the gate does not pass.
This is the second control in this file, after `mode = "off"`, that can
have a consequence nobody asked for, so it gets the same explicit
treatment.

- **What triggers it:** any non-zero exit from the gate — a missing or
  stale binary, absent credentials, a failed offline self-test, a
  phase-A or phase-B failure, **or an unreachable venue**. It fails
  closed on purpose: an unproven binary is unproven whatever the
  reason, and the engine already running keeps trading under the binary
  that was vetted when it booted.
- **What it does NOT stop:** retention and the xsd table rotation. They
  have nothing to do with signing, and a gate that silently stopped the
  disk-pressure sweep would turn a signing problem into a full disk on
  a laptop-hosted engine.
- **The restart is deferred, never consumed.** The fired slots stay
  stamped — so the once-a-day laws hold — and the owed restart is
  recorded in `state/exec-gate-pending` and taken the minute the gate
  goes green.
- **Backoff is mandatory, not politeness.** Hyperliquid's request
  budget is ADDRESS-based, and an exhausted address drops to one
  request per ten seconds, which is below what the gate needs to pass.
  A gate retrying every 60 s would spend ~2,880 requests a day and
  within days exhaust the very budget it needs to go green — a
  transient failure repairing itself into a permanent one. So: 1, 5, 15
  then 60 minutes, and the free offline half still runs on every
  attempt.
- **There is no paging lane.** After three consecutive failures the
  gate writes `state/exec-gate-RED`, which is a file somebody has to
  look at. This file's own standard — *a halt nobody is paged for is a
  halt discovered in a weekly report* — is not met here, and that is a
  known gap rather than a solved problem.
- **The trigger errs toward running the gate.** Arming is
  `--exec <path>` plus `--arm-live` on the engine's command line, and
  `--exec` has **no default path by design** — an artifact that
  auto-loads from a well-known path is one filesystem accident away
  from arming a slot nobody meant to arm. Which file arms the fleet is
  therefore not knowable from the restart lane, so the gate does not
  guess: it scans **every `~/multivenue/exec*.toml`** and arms on any
  of them, and an existing-but-unreadable one arms it too, because
  "cannot be shown inert" is not "is inert".
  The asymmetry is deliberate and is the whole design: a false positive
  costs one four-second probe against testnet, a false negative puts an
  unvetted signer on the fleet. The residue is that a stale
  `exec*.toml` marked live defers restarts for a fleet that is entirely
  paper — which is the safe direction, and is why `enabled = 0` exists
  as a one-line way to say so. *(Operator ruling 2026-09-15: fix now
  rather than defer to E4.)*

### Credentials the gate requires

`HYPERLIQUID_TESTNET_AGENT_KEY` and `HYPERLIQUID_TESTNET_MASTER_ADDR`
are required; `HYPERLIQUID_TESTNET_EXCHANGE_HOST` and
`HYPERLIQUID_TESTNET_SOURCE` default to the testnet host and `"b"`.
They are documented in `.env.example` and `docs/local-setup.md`,
because an undocumented credential whose absence defers every restart
is the wedge described above waiting to happen.

### Phase C — the order lifecycle (plan §5.1)

The plan's E3 exit gate asks for a signed order ACCEPTED with its oid
echoed back, and modify + cancel-by-cloid round-tripping. That is
**phase C**, and it is BUILT but NOT YET RUN against a funded account.

It is deliberately **not** part of the pre-restart gate. It costs
balance, it creates state on the account, and it needs a registered
agent wallet — none of which the restart lane may depend on. It is
opt-in (`exec-smoke --lifecycle`) and run by a person.

Four things keep it from costing anything real:

1. The same testnet guard as phases A and B, **checked again** in
   `lifecycle::run_on` rather than assumed from the caller.
2. **Post-only (ALO).** A post-only order cannot take liquidity: a
   price that would cross is REFUSED by the venue, not filled. So the
   failure mode of a badly chosen price is a refusal, not a position.
   A fill is treated as a hard stop — if the post-only order traded,
   nothing measured after it means anything.
3. **The operator states the market and both prices.** Nothing is
   derived: LAW E-4 forbids deriving an asset id, and a price this code
   guessed would be the one number capable of turning a test into a
   trade. Because those inputs are four numbers typed on a command
   line — where a transposed price or a size off by a decimal is a
   plausible and expensive mistake — `--dry-run` prints the three
   actions it would send, with every number rendered **the way the
   venue will read it** rather than the way it was typed, and reaches
   no network. It needs no key and no account, so it can be run before
   either exists.
4. **It cleans up after itself.** Any failure after the place attempts
   a cancel before returning, and if that cancel also fails it says in
   so many words that an order may still be resting and must be
   cancelled by hand. This path is exercised by
   `tests/hl_lifecycle_loopback.rs` against a scripted server, because
   "we call cancel in the error branch" is a claim about source code
   until something has watched the cancel go out.

The cancel is by **cloid**, not by oid, and that is the point of the
step: a modify may issue a NEW oid, so a client that tracked only the
oid it was given at placement could not cancel what it placed. The
final stage then cancels the same cloid a second time and requires a
REFUSAL — without which "cancel returned ok" is a claim about a
response body rather than about the book.

### E3's exit gate is MET IN FULL (2026-09-15)

Phase C ran green against a funded testnet account with a registered
agent wallet:

```
{"placed_oid":60196753221,"modified_oid":60196753574,
 "cancelled":true,"verified_gone":true,"passed":true}
```

Two things that run told us, neither of which was knowable from the
documentation:

**1. The modify issued a NEW oid.** `placed_oid` and `modified_oid`
differ. Cancelling by `oid` would have chased an id the venue no
longer held, and the order would have been stranded on the book. The
decision to cancel by CLOID was load-bearing, not stylistic, and this
is the evidence.

**2. The venue enforces an 80% price band.** The first attempt placed
a post-only bid at $10,000 against a $76,935 mark — 87% away — and was
refused with *"Order price cannot be more than 80% away from the
reference price"*. Nothing was placed, so the cleanup path was not
exercised.

That band matters beyond this probe: **a resting order far from the
market is not a way to test safely**, because past 80% the venue
refuses it outright. Any future probe has to sit inside the band and
rely on post-only for its safety instead of on distance. The passing
run used $40,000, 48% below the mark.

E3's exit gate is therefore met in full: the signature half (phases A
and B) and the lifecycle half (phase C).

## The live fill lane (E4, 2026-09-15) — BUILT, not ARMED

**`cli::exec_boot::LIVE_ARM_VENUES` is still empty and the
compile-time assertion holding it empty is untouched.** E4 did not
widen it. That edit is the moment this engine can move real money, and
it is not made by inference — it needs an explicit operator decision,
recorded here, and it has not been taken.

What is built: the cloid law, the request-budget governor, the
user-event scanner AND its socket, the reconciliation parser, and
`HlExchange: OrderDispatch` — the live arm itself.

**The live arm is now COMPILED INTO the binary.** What keeps it
unreachable is that nothing constructs it, plus the empty
`LIVE_ARM_VENUES` behind its compile-time assertion. That is a real
reduction in defence depth against the earlier state, where no arm
existed to construct, and it is recorded here rather than glossed —
`exec_boot.rs` predicted exactly this ("this const becomes the WHOLE
barrier") and the prediction has come true.

What is NOT built, and is required before any mainnet order:

1. ~~**The roll handler.**~~ **BUILT** — see "The roll handler" below.
   The table now binds from a real `InstrumentRoll` event.

   It still binds nothing in production, for a different and narrower
   reason than before: **nothing constructs `HlExchange`**. That is
   E7's arming-path change (item 2). The handler is correct and
   unreachable, which is the state every other piece of this arm is in.

2. **The dispatcher-worker wiring on the `--exec` path.** `on_idle`
   exists and `RoutedDispatcher` forwards it, but that path hands its
   dispatcher straight to the engine loop with no `DispatcherWorker`,
   so the hook reaches nothing there. Until it is wired, a live arm's
   user-event socket would never be pumped — and `submit`'s blocking
   HTTPS POST would run on the engine tick loop, against §6.2's "the
   engine data path makes no syscalls". Wiring it is an arming-path
   change and belongs to E7.
3. ~~**The reconciliation timer** (§6.2).~~ **BUILT** — see
   "Reconciliation" above. What is NOT built is the HALT: drift is
   counted and never acted on, because `halt_on_recon_drift_usd_1e6`
   is an arming-path decision — and note the units: the counter is a
   contract QUANTITY at 1e6, the threshold is named `..._usd_1e6`, so
   E6 must convert. **USDC is also not compared**, though plan §6.2
   asks for it, so a wrong-price/right-quantity fill reconciles clean.
   And the check is blind to everything downstream of `try_push`, so a
   stale MEMBER view — which plan §6.2's version would have caught — is
   not covered. `net_exposure_1e8` also still has no
   caller — the netting rule (equal Yes+No on one outcome is riskless
   collateral) belongs with the risk gate, which reads positions this
   arm does not own.
4. **The tape write for a foreign fill.** LAW E-9 requires one; today
   it is counted (`fills_foreign`) and discarded. A count says a
   stranger's fill happened but not what it was.
5. **`/metrics` for the live arm.** `HlExchange::stats()` returns
   zeros, so `HlExecCounters` reaches no gauge. A live arm today would
   report zero submits and zero fills forever.
6. The worker-side `origin` split of §6.4.
7. **A settled instance stays ORDER-authorising** until the member
   zeroes its own instance — see the settled-roll residue above. The
   table has no notion of "this instance is over", and a dropped
   settled roll leaves the member and the binder agreeing on a dead
   one.
8. ~~**Settlement booking.**~~ **BUILT** — see "Settlement arrives as
   a FILL" above. One residue stays open: the `dir: "Settlement"`
   single-string threat model.

   The `unknown_fills` dilution is **also closed**, and the reason it
   was deferred turned out to be wrong. Operator ruling 2026-09-16: a
   SEPARATE FLAG on `Fill`, not a third `origin` value. `Fill::_pad0`
   — one byte of explicit zero at offset 15 since the struct was
   written — became `flags`, carrying `FILL_FLAG_SETTLEMENT`.
   **`origin` is untouched**, so the contract two languages test
   directly is intact, and the Python reader's `_FILL` format string
   skips byte 15 as padding and always did. No version bump: every
   capture in existence reads as no flags. `book_fill` now returns on
   the flag BEFORE the oid scan, counting `settlement_fills` — which
   also removes a reachable collision, where a settlement's venue oid
   matching a live `client_oid` could have applied a spurious sell to
   a pending leg. It was an operator decision, not an
   inference.
9. **The `Fill::order_id` convention.** FIXED 2026-09-15, recorded
   here because of what it implies about the class of defect. Engine
   wide, `order_id` is the MEMBER'S `client_oid`: the paper matcher
   stamps `o.client_oid`, the backtest stamps `f.client_oid`, and
   `strategy_bin15::PendingLeg::oid` is documented as "the
   `client_oid` submitted" and is what `book_fill` matches on. The
   Hyperliquid arm stamped the VENUE oid, while the real `client_oid`
   sat decoded in `Owner::Ours` and was discarded. Every live fill
   would have missed every pending leg, so the member would have
   reported zero inventory while the venue held real size — the paper
   arm and the live arm describing different worlds, which is the
   shape LAW E-1 exists to forbid. **It was invisible to every gate**:
   the paper path was untouched and correct, and nothing exercised the
   live path end to end.

**The fill path allocated, and the gate that should have said so was
measuring something else.** The first revision of `pump_user_events`
staged every WS frame into a heap `Vec` before routing it, because
`route_fills` took `&mut self` and the payload borrows the socket.
That allocated on every frame — pings and `orderUpdates` included,
since the channel test happens inside the router — on the one thread
that also signs and submits orders. It survived review and it survived
"56/56 at 0 B/op", because no allocation gate touched `HlExchange` at
all: gate 55 measures the fill lane's *pieces*, and a gate that
measures a lookalike reports on code nobody runs.

The fix is `HlExchange::route_frame`, which takes the five fields it
needs as disjoint borrows instead of `&mut self`, so the scan reads the
socket's own receive buffer in place — zero copy, per §"all networking
is zero-copy", and no allocation. It is **`pub`**, so the gate measures
the real function rather than a copy of it; that widening adds no
capability, because every ingredient it uses was already public and
every `HlExchange` field is private, which makes it a NARROWING wrapper
over `Producer::try_push` (it adds the channel test, the strict parse,
the tid dedupe, the cloid containment and the zero-quantity refusal).
**Gate 56**
(`hl_exchange_route_frame_is_zero_alloc`) measures that function
directly over a 400-row snapshot-sized frame, and was verified to FAIL
(200 allocations, 8.3 MB) when a single `payload.to_vec()` is
reintroduced — a gate whose failure has never been observed is a claim,
not a check.

### The fill path's LAW E-4 — `AssetTable::sym_of_coin`

The venue echoes a coin name in every `userFills` row; the engine
routes on its own `SymbolId`. Resolving one to the other was the last
thing standing between the live arm and a booked fill, and there were
two ways to do it.

The tempting one is arithmetic. The plan states the encoding twice
(§"Asset id for a HIP-4 leg", and §6.2's "outcome legs appear as
`+<enc>` coins"), `ingress-hyperliquid` already implements
`100_000_000 + enc`, and parsing `+<enc>` back out is four lines. **It
is not what this does.** A fill booked against the wrong symbol moves a
position the member never took, silently, in the tape, forever — and
reconciliation, the backstop that catches a *missing* fill inside a
minute, catches a misattributed one never. An encoding we have not yet
seen on a real outcome position is not a foundation to put that on.

So the coin name is **bound, not derived**, by exactly the discipline
LAW E-4 applies to asset ids in the other direction: the roll event
that supplies the asset id supplies the venue's name for the leg,
`AssetTable` stores it, and `sym_of_coin` answers by **comparing
bytes**. A coin no roll bound resolves to nothing and its fill is
counted (`fills_unresolved`), never booked. `AssetTable::outcome_coin`
exists so a roll handler has one place to compute `+<enc>` — the mirror
of `asset_id`, and like it, deliberately unreachable from the fill
path. **If the venue spells an outcome leg differently from what the
plan records, the failure is a counted non-booking and one edit in the
roll handler, not a misattributed position.**

*One deliberate asymmetry.* `sym_of_coin` remembers the previous
generation of each slot's name; `lookup` — which authorises ORDERS —
does not, and still refuses a stale instance outright. A fill arriving
just after the quarter-hour roll is a position the member really took
and must book; an order placed on a rolled instance is the catastrophe
LAW E-4 exists for. Two generations back is forgotten, so the memory
cannot grow into a way to trade a settled market.

### The venue has TWO coin namespaces, and the plan recorded one

**Measured against the live testnet venue, 2026-09-15**, on one account
holding one outcome leg — both spellings observed for the same `enc`:

| endpoint                             | spelling for `enc = 112410` |
|--------------------------------------|------------------------------|
| `l2Book`, `userFills`                | `#112410`                    |
| `spotClearinghouseState`             | `+112410`                    |

The plan records only the `+` form (§6.2, "outcome legs appear as
`+<enc>` coins"), which is correct **for balances** and wrong for
fills. Every hand-written `userFills` fixture in `exec-hyperliquid`
had inherited it — including one named `a_real_frame_scans_...`, which
was not real. They agreed with a sentence in a document and with
nothing else.

**This is the case the "bind, don't derive" design was built for, and
it is worth being precise about what it bought.** Had the fill path
parsed `+<enc>` directly, every venue fill would have failed to resolve
— silently, counted as `fills_unresolved`, with `fills_booked` sitting
at zero and looking exactly like a quiet market. Instead the cost was
one edit in one helper (`outcome_coin` renders `#`,
`outcome_balance_coin` renders `+` for reconciliation) and a fixture
sweep. Nothing was ever misbooked, because nothing could be: the table
answers only for names a roll bound.

The fixtures are now **captured venue output**, not invention —
`userws::tests::the_real_venue_shape_scans` carries three verbatim rows
from the testnet API, and `recon.rs` keeps `+<enc>` because balances
really are spelled that way.

### The roll handler — where a binding comes from

`AssetTable::bind` had no production caller, so the fill path could
resolve a coin in principle and never in fact. The binding needs
`outcome_id → SymbolId`, which is engine-side knowledge, while §6.1
forbids `exec-hyperliquid` depending on `ingress-hyperliquid`.

**The wire already carries both.** `InstrumentRoll` sets `sym` to the
family's YES leg and packs the outcome id into `venue_seq` bits 0..32;
the NO leg is the next ordinal by the boot ordinal law. So nothing has
to be derived or looked up — the event states the pair `bind` wants.

**And the two ends are already on one thread.** `Engine` owns
`D: OrderDispatch`, and on the `--exec` path `RoutedDispatcher` runs
INLINE on the engine thread. A ring would have been machinery for a
boundary that does not exist. So: `OrderDispatch::on_venue_event`,
defaulted to a no-op like `on_idle`, called from the engine's event
drain, forwarded by the router, implemented by the live arm.

**Ordering is load-bearing.** The dispatcher is told BEFORE the
strategy. A member handed a roll may submit into the new instance in
the same call — `on_venue_event` gives it a ctx with `submit` — so
strategy-first would refuse the order against a table the dispatcher
was about to fill in. `engine::a_roll_binds_before_the_strategy_can_act`
holds it, and was verified to fail with the two lines swapped.

A **settled** roll binds nothing and unbinds nothing. `unbind` clears
`prev_coin`, which is exactly the one-generation memory that lets a
fill still in flight across the roll resolve; the venue itself keeps a
settled instance's coins subscribed, and the successor's `bind`
overwrites in place and carries the old name forward. Dropping the
binding would throw away a real fill to tidy a table.

**The residue, stated rather than left implicit.** That reasoning is
about FILLS; the same binding also stays ORDER-authorising. Between a
settlement and the successor's created roll, `lookup` will happily
answer for the settled instance — so what actually refuses an order
into a dead market is **the member zeroing its own instance**
(`strategy_bin15::clear_instance` clears `live.outcome`, after which
`next_oid` names instance 0 and the table returns `StaleInstance`).
That is a member-side guarantee, not a table-side one, and it has two
consequences worth naming:

- A future Hyperliquid member that does **not** zero its instance on
  settlement reopens the window. Nothing in `exec-hyperliquid` would
  catch it.
- If the settled `InstrumentRoll` is dropped from the event ring
  (`inc_event_ring_drops`) or masked off, **both** the member and the
  binder miss it. The member keeps naming the settled outcome, the
  table keeps answering for it, and the two agree on a dead instance
  until the successor's created roll rebinds.

Neither is solved here. Both go on the pre-arming list rather than
being argued away — the fill-side reasoning for keeping the binding is
sound, and it is not a reason to claim the order side is covered.

#### The trap it had to clear: `instance = 0`

`submit` looked the asset up with `lookup(order.sym, 0)` — the
instance hardcoded. That was harmless only while the table was empty,
and it was not a small thing: **a hardcoded 0 compares 0 to 0 forever,
so LAW E-4's staleness check could never fire.** The law's whole
purpose — refusing an order whose asset id was bound for an instance
that has since rolled, because that is a real order on someone else's
market — was inert.

Binding with `instance = 0` would have preserved the inertness. Instead
the instance now comes from the order: the member already states which
one it believes it is trading, in `client_oid`'s low bits. That
convention moved to `core_types::OID_INSTANCE_MASK`, beside the field
itself, because the two crates that must agree about it **cannot see
each other** — and a convention that drifts does not fail a test, it
refuses every order or, worse, accepts one against an instance that has
rolled. `an_order_naming_a_rolled_instance_is_refused` is the test that
was impossible to write before.

#### Fees: measured, and the row does not move

The same phase-D fill reported `"fee": "0.0"` on a CROSSING (taker)
leg. HIP-4 fees really are zero. `fees.toml` already said `0:0` —
corrected from the docs on 2026-09-14, superseding a `2:5` pair
inferred from the perp schedule — so **the delta on every BIN15 replay
is exactly nothing**. What changed is the row's standing: §6.5 says
measure rather than read, and until this fill nothing in the repo could
measure a fee at all, because every BIN15 fill in existence is paper.
The caveat stays: this is testnet, and a mainnet HIP-4 fill has still
never been measured here.

### Phase D — the round trip LAW E-9 rests on, finally measured

`exec-smoke --fill` places IoCs that are **meant to trade** — one by
default, up to `--fill-repeat 32`. It is
separate from `--lifecycle` rather than a flag on it because the two
are opposite in intent: `--lifecycle` places a POST-ONLY order and
treats a fill as a failure, which tests the lifecycle without trading.
Phase D trades on purpose, for one reason — **nothing else proves that
our own cloid survives the round trip**, and LAW E-9's whole
attribution model rests on it.

An IoC either trades or is gone, so unlike the post-only probe this one
should not be able to strand a resting order — and it therefore carries
no cleanup path. **That is a property of the order type, not a
guarantee this code enforces**: `FillReport::any_resting` exists
precisely because a venue that rested an IoC would have done what the
order type forbids, and if that ever happens the order is on the book
with nothing behind it. The CLI exits nonzero and says so; recovery is
a cancel-by-cloid across the batch's whole id RANGE, which is possible
because every cloid is deterministic from the two numbers that placed
it — `cloid::encode(slot, client_oid + i)` for `i` in
`0..attempted`, slot and first id defaulting to 3 and 1. Both ends are
printed by the dry run and again by the recovery message. A loopback
test drives that branch.

**A batch makes that reporting load-bearing, so `Err` was narrowed to
mean one thing: nothing left the process.** Anything that goes wrong
once a request has gone out comes back as `Ok(FillRun)` with
`stopped: Some(e)` and the report of everything already done. The first
shape of `--fill-repeat` did the opposite — it accumulated counts into
a local and returned them only on the happy path — so a batch that had
an IoC rested at order 4 and was refused at order 5 reported the
refusal and **nothing about the resting order**, on the one path here
with no cleanup behind it. The CLI now prints the machine-readable line
before any verdict, and names both ends of the cloid range whenever
something may be on the book. `FillReport::attempted` counts requests
that LEFT, not requests that were ACKED, because a send that fails on
the way back was still read by the venue: recovery sweeps the attempted
range. Two loopback cases drive it — `[FILLED, REJECTED, …]` must still
report the fill, and `[FILLED, PLACED, REJECTED]` must still report the
rest.

**`attempted != sent` is not an alarm, and conflating the two nearly
cost the alarm its meaning.** A non-crossing IoC is REFUSED by the
venue — it arrives as `stopped`, carrying the venue's own words, not as
a quiet zero-fill — and that is the most common outcome of this
command. The venue answering "I placed nothing" is the opposite of
doubt, so a warning keyed on `attempted != sent` would have printed
*an order may be ON THE BOOK* on every ordinary retry, and the only
alarm guarding the only unrecoverable state on this path would be one
nobody reads by the twentieth run. `FillReport::in_doubt` carries the
actionable bit instead, set at the single site that knows which
refusals are answers: a transport failure or an unparseable answer is
doubt; a venue refusal, a signing failure and an encode overflow are
not, and any variant added later defaults to doubt. The cancel-by-cloid
alarm reads `any_resting || in_doubt`; the separate advice to advance
`--fill-cloid` past ids the venue has already seen is a `warn!` on its
own trigger, because it is true of the benign refusal too. Two loopback
cases pin the bit in both directions.

Four guards, because this is the only path in the repo that executes
by design:

- **A dry run**, `--fill --dry-run`. `--lifecycle` has one, with the
  rationale that "a transposed price or a size off by a decimal is a
  plausible mistake and an expensive one" — which was written for the
  case where post-only makes a wrong price a REFUSAL. On this path the
  same typo trades, so the rationale applies with more force, and the
  preview prints the **notional**, which is the number a misplaced
  decimal corrupts.
- **A notional ceiling**, `MAX_FILL_NOTIONAL_1E8` = $100, checked
  before anything is signed and shared with the dry run so the
  rehearsal refuses what the send refuses. It is a **typo limit, not a
  risk limit**: every cap that matters lives in `strategy-*` and the
  ruleset validator, and this path traverses none of them. An
  unbounded `i64` on the only executing path is not defensible even on
  testnet, because the shape is what a mainnet variant would copy.
- **A batch bound**, `MAX_FILL_REPEAT` = 32, also a typo limit. The
  notional ceiling stays **per order** — twenty small fills are meant
  to stay twenty small fills, which is the whole point of the flag —
  so the ceiling on one *invocation* is the product: **$100 × 32 =
  $3,200**. That is the number one command can spend, written down
  here because the per-order figure no longer answers the question.
  The dry run prints both the per-order and the batch notional. The
  same check refuses a `client_oid` range that would wrap `u64`: the
  add is unchecked in release (`overflow-checks = false`), and the low
  32 bits of a client id are the roll instance `OID_INSTANCE_MASK`
  names.
- **Tests on the guards themselves.** Mainnet refusal on both
  `run_fill` and the `run_fill_on` seam, the oversized-notional
  refusal (including that `i64::MAX × i64::MAX` cannot wrap past the
  ceiling), and the loopback cases above. The repo's own standard is
  that an unexercised guard is a claim about source code.

The operator states the market, the crossing price and the size; this
code does not read the book and will not guess a price.

### Phase E — the safety net, finally run against a socket

§6.2 calls reconciliation "the single most valuable safety net in the
plan". Until `exec-smoke --recon` existed it could only run inside a
live `HlExchange`, which E7 gates — so the check meant to catch a lost
fill, a double-counted fill, a wrong asset id and a stale position view
had never once run against a venue. It was a unit test and a claim.

Phase E **reads only**: no order, no signature, no balance spent. It
takes the `userFills` snapshot, rebuilds this arm's ledger from OUR OWN
cloids, asks for `spotClearinghouseState`, and compares. It is the one
phase here that needs no A/B probe first, because it signs nothing.

Three properties make it worth trusting:

- **The ledger is rebuilt from the venue's echo of our cloids**, never
  from anything a member believes. A reconciler that asked the member
  would be agreeing with itself.
- **Two passes, not one.** `userFills` arrives newest-first and a
  settlement carries no cloid, so it can only be attributed by a symbol
  an earlier fill bound. A single forward pass would meet a settlement
  before the trades whose position it settles, count it unowned, and
  leave the ledger claiming a position the venue has already paid out.
  Binding is its own pass so the ORDER OF THE SNAPSHOT cannot decide
  the answer. `a_settlement_ahead_of_its_own_trades_is_still_booked`
  fails against the single-pass version.
- **Nothing is dropped.** `ours + foreign + settlements == rows`, and a
  row that cannot be placed is `refused` — which exits nonzero on its
  own, because a reconciliation missing rows agrees by having less to
  disagree with. A `userFills` frame that does not scan, and a snapshot
  that never arrives, are both errors rather than empty ledgers, for
  the same reason.
- **An agreement over nothing is not an agreement.** `drift_legs` is
  accumulated *inside* `for_each_live`, which does not run when no leg
  is bound — so zero legs produce zero drift, and a verdict of
  `drift_legs == 0` alone reads "nothing disagreed" as "the things
  agreed". Those are the same sentence only over a non-empty set, and
  the empty one is reachable: a wrong master address, an untraded
  account, a snapshot of nothing but payouts. `agreed()` therefore
  requires `legs > 0`, an empty snapshot is refused exactly like a
  missing one, and `ours == 0` gets its own message. **Measured, not
  argued**: the same binary against the same venue with one env var
  changed prints `agreed:false` and exits nonzero (see the two runs
  below).

**Phase E's reach, and the number that measures it.** Its comparison is
the *intersection* of the `userFills` snapshot window and our own fills:
`compare_booked` walks OUR legs, and a leg is bound only from rows
present in the snapshot — so a position we still hold whose trades are
older than the venue's snapshot depth would never be bound, never
compared, and would read as agreement **by absence**. The asymmetry is
not small: the runs below compared 2 legs against 18 balance-sheet rows.

`recon::unreconciled_venue_legs` is that blind spot measured from the
side that can see it — every `+<enc>` row the venue reports with a
NON-ZERO holding that matched no bound leg. Non-zero because a leg the
venue holds nothing of has nothing to reconcile, and counting settled
husks would bury the real ones (the venue's own sheet volunteers
thirteen zeroes for an account holding one coin). `agreed()` requires it
to be zero, so **a run that agreed on every leg it looked at, while the
account holds a leg it never looked at, is not a pass** — E4's gate is
about the account, not about the subset that fitted in a snapshot.

A leg that aged out and then **settled** was always harmless (a HIP-4
binary resolves the whole position, so the venue holds nothing either);
it is the leg that aged out and is **still open** that this catches.
`legs_nonzero` is the companion number: how much of an agreement is
carrying weight rather than netting to zero.

The drift is reported **twice**: as a contract quantity, and in USD
through `recon::drift_qty_to_usd_1e6`. The two are not the same number
by accident — `recon_drift_max_qty_1e6` is contracts and
`halt_on_recon_drift_usd_1e6` is money, and they share a scale suffix,
so E6's halt rule comparing the raw counter would have been comparing
contracts to dollars. The factor is the **settle ceiling, 1 USDC per
contract**, because a HIP-4 leg resolves to exactly 0 or 1. Marking at
the last trade would be more accurate on average and wrong in the only
direction that matters: a 40-contract drift on a leg trading at 0.02
marks to 0.80 USDC and clears a 5 USDC halt, while the position it
failed to account for is worth 40 if that leg settles YES. At the
ceiling the conversion is the identity and it can only ever halt EARLY.

**Run 2026-09-16, testnet, immediately after the 20-fill batch — a
LIVE position:**

```
{"rows":23,"ours":21,"foreign":0,"settlements":2,
 "settlements_unowned":0,"legs":2,"refused":0,"balances":18,
 "drift_legs":0,"worst_qty_1e6":0,"worst_usd_1e6":0,"agreed":true}
```

Agreed over 21 of our own fills across two legs, against 18 rows of the
venue's balance sheet. At that moment the venue held `+195641` total
`40.0` and our ledger said the same 40 — so the agreement was on a real
open position, not on arithmetic.

**And then the market settled, mid-session.** `#195641` resolved YES:
one `dir:"Settlement"` row, px `1.0`, sz `40.0`, `closedPnl 8.4` (40
bought at 0.79 = 31.60, paid out at 1.00 = 40.00). USDC 968.04 →
1008.01. The next run:

```
{"rows":23,"ours":21,"foreign":0,"settlements":2,
 "settlements_unowned":0,"legs":2,"refused":0,"balances":18,
 "drift_legs":0,"legs_nonzero":0,"worst_qty_1e6":0,
 "worst_usd_1e6":0,"agreed":true}
```

Still agreed — and `legs_nonzero: 0` says exactly what changed. Both
legs now net to zero on our side (+40 −40, +2 −2) and the venue holds
neither, so this agreement is arithmetic where the earlier one was a
position. **That is the number doing its job**: without it the two runs
are indistinguishable, and a reader would take the weaker evidence for
the stronger. It also means the settlement-books ruling is measured end
to end on two separate legs: the venue paid out, `build_ledger`
attributed the payout by symbol with no cloid to go on, and the two
sides still met at zero.

and the falsification, same binary, same venue, one env var changed:

```
HYPERLIQUID_TESTNET_MASTER_ADDR=0x…dead
{"rows":3,"ours":0,"foreign":3,"settlements":0,"settlements_unowned":0,
 "legs":0,"refused":0,"balances":25,"drift_legs":0,"legs_nonzero":0,
 "worst_qty_1e6":0,"worst_usd_1e6":0,"agreed":false}
ERROR the snapshot carried NO fills of ours. Nothing was compared…
```

Before `agreed()` required `legs > 0`, that second run printed
`agreed:true` and exited SUCCESS. A gate whose failure has never been
observed is a claim; this one has now been observed against the venue.

**Agreed on every leg**, over 21 of our own fills across two legs,
against 18 rows of the venue's balance sheet. The two settlement rows
were attributed by symbol and booked — `settlements_unowned` is zero —
so the settlement-books ruling is now measured end to end rather than
only in a fixture: the leg that settled nets to zero in our ledger and
the venue agrees it holds nothing.

This closes E4's exit gate as written.

**Run 2026-09-15, testnet, bought 2 of a live MLB outcome leg at 0.68.**
The results, which are now the crate's fixtures rather than invention:

- The cloid we signed — `0x4d560300000000000000000000000001` — came
  back down `userFills` **byte for byte**, and decodes to
  `Owner::Ours { strategy_id: 3, client_oid: 1 }`. LAW E-9 is measured,
  not asserted. `userws::tests::our_own_cloid_survives_the_venue_round_trip`
  carries the row verbatim.
- **Both namespaces, one account, one moment**: the fill says
  `#194180`; `spotClearinghouseState` for the same leg says `+194180`,
  total `2.0`, `entryNtl 1.36`. That is the table above, confirmed on
  our own balance rather than a stranger's.
- USDC went 999.00 → 997.64. Exactly 2 × 0.68.

Two venue constraints fell out of it:

- **Minimum order value is 1 USDC.** The first attempt (size 1 at 0.68
  = $0.68) was REFUSED with "Order must have minimum value of 1 USDC".
  This is a real sizing constraint, not a testnet quirk to ignore: a
  clip priced near the bottom of a binary's range can breach it, and
  the refusal costs a round trip and a nonce. **Size the clip against
  the price, not against the unit count.**
- **`fee: "0.0"`** on a crossing (taker) fill. HIP-4 fees really are
  zero on the wire, which is the fact `fees.toml`'s 2:5 contradicts
  (see the BIN15 intraday note). Measured, per plan §6.5 — not read
  from a doc.

### LAW E-8 — the roll takes its own quotes back

On a roll and on a settle, every order this engine placed on the leg
that just ended has to come off the book. `release_pendings` releases
cap reservations only; in live mode the venue has to be told.

**The list of what to cancel comes from the VENUE.** A table of open
orders kept by the arm can disagree with the venue — and it disagrees
*invisibly*, which is exactly how a restart or a missed ACK leaves a
quote on a dead instance. Same principle as reconciliation: believe the
venue, not our own record. One `frontendOpenOrders` round trip per
roll, ~768/day across eight families against a budget of 10,000+.

`frontendOpenOrders` rather than plain `openOrders` because **only that
variant echoes the cloid**, and without it the sweep cannot tell an
order this engine placed from one it did not. Cancelling a stranger's
order would be the mirror image of booking a stranger's fill, so the
selection (`recon::ours_on_leg`) takes two filters and needs both: the
coin must match the ended leg exactly, and the cloid must decode as
OURS (LAW E-9). Deleting either filter fails a test.

**The sweep runs on the IDLE path, not in the roll handler.**
`on_venue_event` runs inline on the engine thread, and a sweep is one
info round trip plus a cancel per resting order — blocking it at roll
time is blocking it at the exact moment the member wants to quote the
successor. The roll records what it RETIRES (before the rebind
overwrites the slot, after which the ended leg is unnameable) and the
idle path drains one entry per call, which is also where `reconcile`
lives and for the same reason.

**The sweep's cancels are exempt from the submit budget**, and that is
a risk-policy fact rather than an implementation detail. `AddressBudget`
has always said so — `may_cancel` returns `true` unconditionally, with
the note that a halted engine must be able to flatten — but that method
had **no callers** until E5, because `submit` was the only verb and a
submit is a submit. Giving three verbs one shared tail is exactly where
that rule could have been inverted, and for a while it was: cancels
answered to `may_submit`, so at the budget floor the engine's cancel
verb stopped working and the sweep converted a transient squeeze into
`sweep_left` — "orders left resting on a dead instance", the number E6's
kill switch reads — while the real cause was our own governor, dropping
the entry for good. `Spend::{Submit, Cancel}` now names which rule an
action answers to at each call site. Cancels still COUNT against the
address; they are never REFUSED by it.

**A modify answers to the SUBMIT rule.** LAW E-7 makes it the requote
path, not a way around the governor — and the budget matters *because*
Arm B reprices ~333 times per instance, so a modify routed to
`may_cancel` would let a member reprice past the floor indefinitely,
which is the address-budget exhaustion the governor exists to prevent.
**One case this gets wrong, deliberately**: a modify that REDUCES
exposure — smaller size, or a price further from the market — is
arguably an exit and is refused at the floor anyway. `Order` carries no
reduce-only bit, which is the same limitation already recorded for
`mode = "off"`, so treating every modify as a submit is the fail-closed
choice with an existing precedent. **When the reduce-only bit lands
with E6, this is one of the sites to revisit.**

**A repeated roll retires nothing.** The venue re-sends
`outcomeCreated` on a reconnect snapshot and a replayed ring entry
carries it too, so the handler compares the bound asset against the one
it is about to bind. Without that the queued asset is the very one the
bind re-establishes as live, and the next idle would enumerate the
account and cancel every quote on a LIVE leg — at the moment the member
is quoting it.

**It does not halt.** `sweep_left` counts what could not be taken off
the book once the retries are spent, and E6 decides what that is worth
— a halt inferred here would be a policy this file invented, the same
reasoning that keeps `reconcile` from halting. "Retry on the next idle"
is bounded at `SWEEP_TRIES` = 8, because retrying forever is a leg that
burns the address budget forever. Queue overflow is counted as
`sweep_left` too: a leg nobody swept and nobody counted is precisely
the stranded quote this law exists to prevent.

The counters grew `HlExecCounters` from three cache lines to four. That
is a decision, not a surprise — the pin assertion exists to make it one.

**Run 2026-09-16, testnet, `exec-smoke --sweep` on outcome 15417:**

```
{"cloid":"0x4d5603000000000000000000000001f5","placed_oid":60250817976,
 "coin":"#154170","listed":true,"selected":true,"cancelled":true,
 "gone_after":true,"unswept":false,"stopped":false,"passed":true}
```

The venue lists our order **with its cloid**, `recon::ours_on_leg` —
**the arm's own selection, not a copy of it** — picks it out of the
whole account, the cancel takes it off, and a **second enumerate**
confirms it is gone. That last step is the point: *acked* and *gone*
are different claims, and a venue that accepted a cancel and left the
order resting would otherwise read as a pass. The leg's name `#154170`
came from the venue's own row rather than being derived from the asset
id — that derivation is the same class of guess LAW E-4 refuses in the
other direction.

### Phase F — a requote is a MODIFY, and it changes the cloid

LAW E-7 makes a live requote a modify rather than a cancel plus a
place: two requests instead of one, and at ~333 reprices per instance
that is the difference between fitting inside the address budget and
not. The operator ruling of 2026-09-16 added the second half — the
replacement carries a **fresh** client id, so every `userFills` row maps
to exactly one quote instead of to a cloid that has meant several
different prices.

**Neither half had been measured.** Phase C's modify targets the resting
order by its VENUE oid and hands the replacement the SAME cloid, so
nothing in this repo had ever asked the venue the question the ruling
depends on. If the answer were no, the ruling could not be implemented
as written — and that is a thing to learn from one testnet probe rather
than from a live requote lane running at 333 per instance.

`exec-smoke --requote` is four requests, post-only throughout, and **the
last two are the assertion**:

```text
  place  cloid A, post-only     -> rests
  modify BY cloid A -> cloid B  -> ok
  cancel cloid A                -> must be REFUSED  (A is gone)
  cancel cloid B                -> must SUCCEED     (B is real)
```

The asymmetry is the evidence and neither half carries it alone: a
refusal on A is equally explained by a modify that killed A and created
nothing, and a success on B is equally explained by a modify that left
**both** resting — which is a leak, not a requote. Only the pair says
the order MOVED. Each half is pinned by its own loopback case, and
reducing `passed()` to either one makes the other case fail.

The probe is self-cleaning because the verification IS the cleanup —
**and nothing past the place uses `?`**. The first version did, on the
first cancel, which left the NEW id unswept and unnamed on a transport
failure: the id the probe's own hypothesis says is resting. Phase C
gets away with the same shape only because its `?` sits AFTER its
successful cancel; phase F is the first probe in this lane to hold two
ids at once, and it swept one. `Err` from `run_requote_on` now means
**nothing has been sent** — testnet, the spec, the two encodes, and
nothing else. Every failure from the first request onwards, **the place
included**, comes back as `Ok` with `stopped` set, `unswept_old` /
`unswept_new` saying which cancel went unanswered, and **both client
ids printed**. The place was the last stage still returning `Err` after
a send, and it had two ways to strand an order: a lost answer, and a
venue reply saying RESTING with no oid echoed back. The second used to
share a message with "not resting" — opposite situations, since one
strands nothing and the other is on the book. Both now fall through to
the sweep, which needs no oid because it cancels by cloid. That last part is the
whole recovery path: the ids are a marker plus a millisecond nobody
typed, so an order left under one of them is otherwise findable only by
listing open orders in the venue UI.

The field names say what they MEASURE. `old_cancel_refused` is not
`old_cloid_gone`: a per-item refusal for some reason other than the
order's non-existence would read the same. That case is narrow rather
than absent — an envelope-level failure (a rate limit, a rejected
signature) becomes `stopped` whatever the stage, so what remains is a
per-item refusal of a well-formed cancel for a cloid this probe built
itself. Narrow is not impossible, and the honest name is what keeps the
difference visible.

Three details that are easy to get wrong and are written down rather
than rediscovered:

- **The two probe cloids are DERIVED, not drawn twice.** `fresh_cloid`
  is a marker plus a millisecond timestamp and the two calls are
  microseconds apart, so drawing again would collide most of the time —
  and a probe whose two ids are equal proves the opposite of what it
  claims while looking like a pass.
- **Byte 15 is that timestamp's LSB**, so two runs exactly one
  millisecond apart can produce swapped pairs — run 1's B is run 2's A.
  Harmless for a four-round-trip probe that sweeps both ids, and
  recorded rather than rediscovered.
- **Whether a per-item error is a failure or the answer is now a
  PARAMETER**, not a stage-name string compare. `post` used to gate on
  `stage != "verify" && stage != "cleanup"`, with no compile-time link
  to any caller — and phase F doubled the number of callers depending
  on those exact spellings. A stage renamed to `verify-old` would have
  the venue's "already canceled", the precise answer this probe exists
  to obtain, come back as an error. `ItemErrors::{AreFailures, AreData}`
  makes the choice explicit at each call site.

**Run 2026-09-16, testnet, outcome 15417 (asset 100154170), a resting
bid at 0.30 moved to 0.31:**

```
{"old_cloid":"0xe35c000000000000000001a0a8cbcf62",
 "new_cloid":"0xe35c000000000000000001a0a8cbcf63",
 "placed_oid":60246216459,"modified_oid":60246217044,
 "old_cancel_refused":true,"new_cancel_succeeded":true,
 "unswept_old":false,"unswept_new":false,"stopped":false,"passed":true}
```

**The venue accepts it.** LAW E-7 with a fresh cloid per requote is
implementable. One thing fell out that the probe was not looking for:
the modify issued a **new oid** (…459 → …044), which is precisely why
cancel-by-cloid is the durable handle and an oid captured at placement
is not — a client that tracked only the oid could not cancel what it
had just requoted.

### PAPER and VENUE are two numbers, never one

Plan §6.4. The moment one fill carries `origin = VENUE`, every reader of
`strategy 3 (bin15) net=` is reading a number whose meaning changed
underneath it. A total that mixes a modelled fill with a real one is not
approximately right — it is meaningless, because the two answer
different questions and nothing in the figure says which.

`Fill.origin` is the byte (`core_types::FILL_ORIGIN_VENUE` = 0,
`FILL_ORIGIN_PAPER` = 1; `claude_worker.fill_origin` is the one Python
mirror of those two integers). The split is enforced in three places,
and in each of them it is **structural rather than a discipline asked of
a caller**:

- **The merge key.** `bin15_accrue.merge_entries` dedupes by
  `(outcome, origin)`, not by `outcome`. One instance can legitimately
  carry a PAPER entry — what the model would have done, from a replay of
  that day — and a VENUE entry, what the account actually did. Two facts
  about one market. Keyed on the outcome alone, whichever merged second
  would silently replace the other.
  `pnl_report.merge_reports` does the same with `(strategy_id, origin)`.
- **The renderer refuses.** `bin15_accrue.render` raises on a mixed
  sequence rather than averaging it. Every figure it produces — hit
  rate, cost, payout, the Wilson interval — is a sum over what it was
  handed, and a modelled fill averaged with a real one produces a number
  that looks exactly like the ones that are true. `by_origin` is how a
  caller splits a store; the report prints one block per accounting,
  headed by the word.
- **The nightly line carries the word.** `strategy 3 (bin15) PAPER
  net=…` and `strategy 3 (bin15) VENUE net=…`, one line per accounting.
  **There is no combined line**, because the combined number is the one
  §6.4 forbids. This breaks any reader matching the old exact shape,
  deliberately.
- **Every OTHER dollar aggregation in the same file, too.** The first
  version of this split fixed `merge_reports` and left two neighbours
  summing the same dollars: `_merge_regime` (the §5.1 per-regime P&L —
  the same figures broken down by regime word instead of merged flat)
  and `vm_by_ruleset` (merged by ruleset hash across runs). Both now key
  on `(…, origin)` and both producers stamp the byte. Worth recording
  *how* that was missed: `bin15_accrue` got a GUARD as well as a split
  key, and a guard fails loudly wherever it is reached, while a key only
  protects the dictionary it is the key of. Counting aggregations is not
  optional when the protection is structural.

**Producers stamp; readers refuse.** `Bin15EntryRow::origin`, the
`audit-pnl` strategy row and `bartest.to_audit_pnl` all write the byte
explicitly, and all three are `PAPER` today for the same reason: a
REPLAY models every fill, so there is no venue number in them and there
never can be. That is precisely why it is written down. A reader that
DEFAULTED an absent field would inherit "paper" in silence on the first
live day — so `entries_from_sidecar` and `merge_reports` both raise on a
missing `origin` and name the fix (rebuild the binary). The failure mode
being designed against is not a wrong number; it is a right-looking one.

**`audit_pnl_version` moved to 2, and the reason is not "the shape
changed".** `regime` and `binary_fills` both rode at version 1 because
they are emitted CONDITIONALLY and tolerated when absent. `origin` is
unconditional and the reader REFUSES a row without it, so a v1 document
would pass the version gate and fail several frames deeper with a
different error — a version field sitting directly in front of a shape
change, saying nothing. Archived nightly JSONs embed raw per-run objects
(`runs_detail`), so both shapes exist on disk under the old number.

**The store migrates rather than restarts.** An 11-column entries row
predates the split and reads as PAPER — not as a default, but because
the harness models every fill and no live path had written there when
those rows were produced. 11 and 12 are the only widths accepted; a
reader that guesses at a column count produces a number nobody can
defend.

**The lane's reading law extends**: never quote a BIN15 dollar figure
without saying which accounting it is, without the window t-stat, and
from now on without saying **PAPER or VENUE**.

### Reconciliation — the one check that believes nothing

§6.2 calls this "the single most valuable safety net in the plan", and
the reason is what it is independent OF. **One claim in that sentence
does not survive this implementation, and it is named here rather than
inherited.**

What it catches: a lost fill (the socket never delivered it), a fill
whose coin did not resolve, a refused conversion, a dedupe
double-book, a wrong asset id (both legs move), and a ring overflow.
Everything, that is, between the venue and `try_push`.

What it is BLIND to: everything downstream of `try_push`. A fill this
arm booked correctly into a lane nobody drained — or one the member
discards — leaves the ledger and the venue in perfect agreement. That
is not hypothetical: after `clear_instance()` every late fill lands in
`unknown_fills` and is dropped, as recorded below, and reconciliation
does not backstop it.

So the plan's "catches a **stale position view**" is the claim this
design gives up. Plan §6.2 compares against "the member's position for
that instance"; this compares against what the arm itself wrote to the
lane. **That is a deliberate change, not an implementation detail**: a
reconciler that asked the member would be agreeing with itself, and the
independence is worth more than the coverage — but the coverage is
genuinely lost and a future reader must not assume otherwise.

**So the comparison is against what this arm BOOKED**, not against the
member's position. `AssetTable` carries a per-leg ledger fed only when
a fill actually enters lane 3 — a dropped, refused or foreign fill is
not a position — and the roll ZEROES it, because a new instance is a
new position and carrying the old quantity forward would have the
reconciler comparing a settled position against a fresh balance
forever. A reconciler that asked the member would be agreeing with
itself.

Once a minute on the idle path (weight 2, negligible): `POST /info
{"type":"spotClearinghouseState"}`, rendered zero-alloc from the
config's raw 20 address bytes, then every live leg compared against the
venue's own sheet. A leg the venue does not mention reads as zero —
which is the right reading, and is itself a drift if we booked
something.

**USDC is NOT compared.** Plan §6.2 asks for two comparisons — the
USDC spot balance against the engine's cash view, and each leg against
its position — and only the second is built. The consequence is
concrete: a fill at the WRONG PRICE with the right quantity reconciles
clean, because nothing reads cash. On the pre-arming list.

**The two namespaces meet here and nowhere else.** The sheet spells a
leg `+<enc>` while the table holds `#<enc>`, so `recon::same_leg`
compares the bytes AFTER the prefix, having checked both prefixes are
the two known ones. It does not parse either side into a number: that
would make `+032530` and `#32530` compare equal, and a leg is not
identified by the value of its digits.

`/info` shares the connection and the response buffer with
`/exchange` — `HlHttp::resp` holds only the last answer — so a
reconciliation must consume its reply before the next order goes out.
Both callers live on one thread and this one runs between order
batches, so they are serialised by construction; that is a property of
the caller and it is written down rather than assumed.

**It does NOT halt.** `halt_on_recon_drift_usd_1e6` is an arming-path
decision and nothing is armed, so a halt inferred here would be a
policy this file invented. Drift is counted per leg
(`recon_drift_legs`) with the worst magnitude kept
(`recon_drift_max_qty_1e6`), alongside `recon_ok` and `recon_failed` — a
venue that is unreachable or answers with something unparseable is
counted and retried at the next cadence, never in a tight loop. Like
every counter here they reach no gauge until `stats()` is wired
(pre-arming item 5).

**Units, because the name of the cap it will feed does not match —
and the conversion now exists.** `recon_drift_max_qty_1e6` is a CONTRACT
QUANTITY at 1e6, while E6's threshold is `halt_on_recon_drift_usd_1e6`,
which is money. The field carries `qty` in its name for that reason,
and `recon::drift_qty_to_usd_1e6` is the conversion E6 must call — the
two numbers share a scale suffix, so a halt rule reading the raw
counter would be comparing contracts to dollars. The factor is the
settle ceiling of 1 USDC per contract; see Phase E above for why a
ceiling rather than a mark.

Two things the comparison had to get right, both of which the first
version got wrong and both of which now have a test verified to fail
against the old behaviour:

- **Total, not free.** `free = total - hold`, and `hold` is what a
  RESTING order has committed — on spot an ask holds the base token.
  The ledger is a pure position and knows nothing about encumbrance, so
  comparing against `free` reported drift equal to the resting size for
  as long as a quote was live: continuously, for a maker, and in the
  direction that looks like a double-counted fill. It passed because
  every fixture written for it set `"hold":"0.0"`.
- **A late fill does not credit its successor.** `sym_of_coin` keeps
  one generation so a fill from the instance that just ended still
  resolves — and it must still reach the lane and the tape. But `bind`
  zeroes the ledger on a roll, so crediting that fill to the successor
  leaves a phantom quantity the venue's sheet will never contain:
  permanent drift, in a high-water mark that never clears.
  `sym_of_coin_gen` reports WHICH generation matched, and the ledger
  takes only the current one.

### Settlement arrives as a FILL

The same capture turned up something the plan does not mention:
**`userFills` carries settlement**, `dir: "Settlement"`, at px `1.0`
for the winning side and `0.0` for the loser, both as `side: "A"` —
the position sold back.

**Operator ruling 2026-09-15: book it like any other fill** — the
venue is the truth, and a binary payout is exactly a sale at 1.0 or
0.0. **IMPLEMENTED**, by resolving the slot from the SYMBOL.

#### The mechanism

A settlement row carries no cloid — the venue generated the order, not
us — so `to_fill` has nothing to attribute from and the row would take
the foreign arm forever. The slot therefore comes from the asset-table
binding:

- `AssetTable::note_owner(sym, strategy_id)` runs in the **accepted**
  arm of `submit`, never at intent. An order the budget, the scale
  guards, the signer, the transport or the venue itself refused never
  traded, and a leg we have not traded must not absorb a settlement.
- `AssetTable::owner_of_sym(sym)` reads it back in `route_frame`.
- `userws::to_fill_as` converts with that explicitly supplied slot.

#### Why this does not punch a hole in LAW E-9

It is a SECOND attribution path, and LAW E-9 exists to forbid exactly
that, so the gate is narrow and every neighbouring case keeps the old
behaviour. The gate is `is_settlement && cloid.is_none()`:

| row | outcome |
|---|---|
| settlement, leg we trade | attributed to the owner, booked |
| settlement, leg we do not | `fills_unowned`, booked nowhere |
| cloid-less row that is NOT a settlement | foreign, as before |
| `dir` absent entirely | reads as not-a-settlement — fails closed |
| settlement that DOES carry a cloid | attributed from the cloid, which is strictly better |

`to_fill_as` is the only function anywhere that takes a caller-supplied
slot, and it **refuses at runtime in every profile** — not under
`debug_assert!`, which release compiles out — both a non-settlement row
(`ConvertErr::NotSettlement`) and `STRATEGY_ID_NONE`
(`ConvertErr::NoSlot`, the fan-out sentinel).

Ownership's lifecycle: learned only on acceptance; **survives a roll by
design**, because the member trading the Yes leg of one instance is the
member trading the Yes leg of its successor; cleared only by `unbind`.
Two members trading one symbol marks the slot CONTESTED — sticky, never
un-contested, counted `owner_contested` — and `owner_of_sym` answers
`None` thereafter, so its settlements are counted and never booked. That
is the same rule `sym_of_coin` applies to an ambiguous coin: a guess
between two claimants is the misattribution this module exists to
prevent. One symbol belongs to one member; a contest is a configuration
error, not a market event.

#### The threat model, stated rather than left implicit

The gate rests on one venue-controlled byte-compare against
`dir: "Settlement"`.

- If the venue **renamed** the string, the row falls back to foreign —
  the safe direction, and exactly the pre-ruling behaviour.
- If the venue sent `dir: "Settlement"` on a cloid-less row that was
  really a trade, it would be booked to the owning slot. **"No cloid"
  is not a second independent check**: the venue omits the cloid for
  every order we did not place, so the two halves of the gate are not
  independent. That is the residue of accepting the ruling, and it is
  recorded rather than argued away.

#### What booking achieves, and what it does not

This is the load-bearing justification, and it is written down so the
next reader does not re-derive it.

**It achieves the TAPE.** The engine captures every lane-3 fill to
`engine-fills.pmlr` *before* the strategy sees it, so lane arrival alone
is enough. Pre-ruling the settlement was `Routed::TapeOnly`, which is
never pushed into the lane, so it reached the capture not at all.
Downstream, `audit_pnl`'s fold reads only `px`, `qty`, `side` and `sym`:
a settlement at side Ask credits `px × qty` and drives `paper_qty[sym]`
to zero. **The mark-out is the sharper half** — without the settlement
fill, `paper_qty[sym]` stays at N and is marked at a stale last mid on a
market that no longer exists. The settlement is what retires that
phantom position.

**It does NOT close the member's in-memory position, deliberately.**
`to_fill_as` stamps the VENUE's oid — a settlement matches no pending
leg of the member's, and pretending otherwise would collide with a real
`client_oid` — and flags the fill `FILL_FLAG_SETTLEMENT`.
`strategy_bin15::book_fill` tests that flag BEFORE the oid scan,
counts `settlement_fills`, and returns. The member is flattened instead
by `clear_instance()` on the roll, which zeroes
`pos_yes_1e6`/`pos_no_1e6`; applying the payout as well would close the
same position twice. **All three orderings end flat** — settlement
first, roll first, or the settled roll DROPPED (the successor's created
roll clears it). The outcome is right by every path and the mechanisms
are different, and "book it like any other fill" without this paragraph
would be a claim the code does not support.

The flag also removed a hazard rather than only a counter's ambiguity:
before it, a settlement whose venue oid happened to equal a live
`client_oid` would have matched a pending leg and applied a spurious
sell. The flag test now precedes the scan, so that cannot happen.

`settlement_fills` DOES reach a gauge —
`engine_bin15_settlement_fills_total`, registered beside
`engine_bin15_unknown_fills_total` on purpose, because settlements used
to be counted there and leaving the new one unpublished would have
moved them from a visible series to a field only a unit test can see: a
regression dressed as a fix. The exec arm's own counters are the
exception: `fills_unowned` and `owner_contested` join `fills_settlement`
as counters that reach no gauge until `stats()` is wired (pre-arming
item 5) — visible to a test and not to
an operator.

`UserFill::is_settlement` records the flag so a settlement is never
*inferred* from a price of 1.0, which a genuine trade can also print.
`fills_settlement` counts every settlement row seen, booked or not, so
it is **not** comparable with `fills_booked`.

**What actually happens today, checked rather than assumed.** The
earlier draft of this section claimed the hazard was a short sale
against zero inventory. It is not, in either event order.
`strategy_bin15::book_fill` matches `fill.order_id` against
`pend_take.oid` / `pend_quote[].oid`; a settlement matches no pending
leg and, since the flag landed, does not even reach that scan —
`apply_position` is never reached either way, so nothing underflows and
the never-sells-short assertion never fires. A GENUINE late trade
arriving for a cleared instance is still **silently dropped**, which is
the real failure mode and a quieter one.

Note the second-order effect, now HALF resolved. `clear_instance()`
wipes `pend_take` and `pend_quote`, so after a roll a late fill for
that instance matches nothing. A SETTLEMENT is no longer among them —
the flag routes it out before the scan — but **a genuine late trade
still lands in `unknown_fills` and moves no position**. That defeats
the one-generation memory `sym_of_coin` was built for: the exec layer
resolves the late fill correctly and the member then forgets the order
it belonged to. The remaining half stays on the pre-arming list.
**Resolve it before a slot is armed**, not after.

*The bug the second review caught.* The first version of
`sym_of_coin` tested the current and previous names at equal
precedence inside one scan, so a **dead** previous-generation name on a
lower-index slot outranked a **live** current name on a higher one:
with slot 0 as `{sym: 42, prev_coin: "A"}` and slot 1 as
`{sym: 99, coin: "A"}`, a fill for `A` resolved to 42. Not `None` and
not the right symbol — confidently wrong, on the fill path, which is
the single failure this design exists to prevent, and the one case
where byte comparison was *not* safer than the arithmetic it replaced.
It is now two passes: every slot's current generation first, then one
generation back only if nothing live owns the name. A coin claimed by
two live slots is ambiguous and resolves to `None`. Both properties are
held by tests that were verified to FAIL against the single-pass form,
returning exactly `Some(42)` and `Some(1)` where the fix returns
`Some(99)` and `None`. The gap that let it through was narrow and worth
naming: every test bound exactly ONE symbol, so the only configuration
in which the function can be wrong was never constructed.

Three smaller things fell out of writing it. `outcome_coin` first
computed `10 * outcome_id + side` unchecked; its own test caught the
overflow, and it now refuses an id past `OUTCOME_ID_MAX` by writing no
name at all. `asset_id` had the same latent wrap, and it now refuses at
**runtime in every profile** rather than under `debug_assert!` — the
release profile sets `overflow-checks = false`, so a debug-only guard
is a no-op in the artifact that actually trades, and the wrap does not
land somewhere harmless: it lands in the low `u32` range, the PERP
asset-id space, where a nonsense outcome id becomes a valid BTC or ETH
asset id. A name that fails to render is harmless; an order placed on
someone else's market is the thing LAW E-4 exists for. And `bind` now
refuses an EMPTY coin name, because binding an asset id with no name
authorises orders for a leg whose fills can never resolve — a roll
handler written as `bind(.., &out[..n.unwrap_or(0)])` would land
exactly there, silently.

Gate 56 now measures the **whole** book path —
`to_fill`, the cloid attribution, `try_push` into the lane and
`on_venue_fill` against the budget — which was unreachable while the
resolver was a `None` stub; 800 fills book inside the guard at 0 B/op.

**`fills_bad_ts`** is new alongside it. The venue's millisecond stamp
was converted with `saturating_mul`, which clamps a corrupt value to
`u64::MAX` ns rather than refusing it — a wrong answer that looks like
an answer, and one that would place the fill around the year 2554 in a
tape read in time order. It is now `checked_mul`, and a stamp that will not
convert books the fill against the LOCAL receive clock and counts it:
the position is real whatever the venue says the time was, and a fill
the engine never hears about is the worse of the two failures. The
conversion sits ABOVE the `resolve_sym` guard deliberately — a corrupt
stamp is a venue data-quality fact, not something to learn only once
symbols are bound. Note the honest limit: like every other counter
here, `fills_bad_ts` reaches no gauge until `stats()` is wired (item 5
above), so today it is visible to a test and not to an operator.

### LAW E-9 — the cloid encodes the slot

The venue echoes a client id and nothing else we chose, and the
engine's fill fan-out routes by `strategy_id`. So the slot travels
inside the cloid:

```
byte  0    1    2      3..7            8..15
     0x4D 0x56  slot   reserved 0      client_oid, big-endian
```

Recovery is a shift and a mask — no table, no allocation, no lock on
the fill path. It also gives cross-slot uniqueness the moment a second
slot arms, and makes engine-origin orders recognisable on the venue's
own books during an incident.

**A cloid without the `MV` marker is NOT ours**, and the obvious
handling of that is a trap worth recording, because the review pass
found it in the first draft of this very section.

Stamping `STRATEGY_ID_NONE` and pushing the fill into lane 3 reads
like containment. It is the opposite: in `strategy_set::on_fill` that
sentinel is the **fan-out** branch, delivered to *every* enabled
member. The one value that looks like "belongs to nobody" hands a
stranger's fill to all seven slots at once.

So the containment is in the type, not in a comment.
`exec_hyperliquid::userws::to_fill` returns `Routed`, and only
`Routed::Slot` carries a fill the worker may push into lane 3.
`Routed::TapeOnly` is a foreign fill: **counted, written to the tape,
and never admitted to the lane.** Both the unit tests and the fuzz
target assert that directly, because a fill the engine did not order is
precisely the evidence reconciliation exists to catch, and both
attributing it and fanning it out destroy that evidence at the moment
it appears.

*(The counter exists — `HlExecCounters::fills_foreign`. **The tape
write does not**, and is listed above as required before any mainnet
order. What E4 guarantees today is the narrower thing: the worker
cannot reach the lane with a foreign fill by accident.)*

The reserved bytes must be zero for the same reason: our encoder never
writes a dirty reserve, so neither did we.

### LAW E-5 — the HTTP response is the ACK, the WS stream is the FILL

The exchange reply to a place carries `resting` / `filled` / `error`;
it binds `cloid → oid` and surfaces reject reasons. **It never books a
fill.** Two sources for one fill is double-counting, and the tape is
the record.

The dedupe key is the venue's own `tid`, and **the ring that holds it
must outlive the socket**. Hyperliquid answers every fresh
subscription with a snapshot of recent fills, so a dedupe that reset on
reconnect would double-book at exactly the moment the engine had just
lost and regained its view of the account.

**The ring must also be at least as large as the venue's snapshot
bound** — a correctness requirement, not a tuning knob. That snapshot
runs to ~2,000 fills; a smaller ring evicts its own earliest rows while
still reading the same snapshot, and the next reconnect re-admits and
re-books them. `SNAPSHOT_RING` (4,096) is the production size.

### The scale conversion, and the fill that must not be booked

The venue quotes 1e8; the engine's `Price` is 1e-6 USDC and its `Qty`
is contracts × 1e6. A fill whose size is non-zero on the wire but
rounds to zero at the engine's scale is **refused**, not booked — a
zero-quantity fill is a trade that reports as having happened and
moved nothing, and it would sit in the tape forever looking like one.

### The request-budget governor

Hyperliquid meters L1 actions **per address**: `10,000 + one per USDC
of lifetime volume`, and an exhausted address drops to one request
every ten seconds — below what this engine needs to place, cancel or
reconcile. That is a cliff, not a rate limit, and it does not clear on
reconnect.

- **Cancels are exempt.** A halted engine must always be able to
  flatten; a governor that refused a cancel would strand the very
  position it was protecting.
- **Volume is counted from VENUE FILLS ONLY**, never from what the
  engine believes it traded. The number exists to predict the venue's
  accounting, and the engine's beliefs are what reconciliation doubts.
- **A cold boot assumes the worst.** No state file, or one written for
  a different address, starts at `spent = initial_buffer` — zero
  headroom until venue fills accumulate. Assuming the full grant on
  the strength of a missing file is how a budget gets spent twice.
  `dailyUserVlm` is a DAILY figure and the venue exposes no lifetime
  query, so the allowance cannot be recomputed at boot — it has to be
  remembered, durably, which is why this owns a state file at all.
- **Cold is an absorbing state, and the ramp needs a seeding step.**
  Headroom rises only with venue fills, and venue fills require
  submits — so a genuine first boot with no state file can never
  permit one. That is fail-closed and therefore safe, but it is not
  self-clearing: E7's first step must seed `exec-budget.state` with the
  address's real figures, or the first armed boot will refuse every
  order and look like a bug.
- **`spent_requests` is `u64` and must be clamped, not cast.** A cast
  to `i64` wraps negative above `i64::MAX`, turning the subtraction
  into an addition: a maximally-spent address then reports ~10,000
  requests of headroom. That was a real defect in the first draft, and
  the test that covered it called `remaining()` without asserting on
  the answer.

### Reconciliation is fail-closed, and "empty" is not a safe default

An unparseable balance sheet is an ERROR, never an empty one. The
failure that motivates this: when the engine also holds nothing, an
empty-on-junk scanner reports zero drift having read nothing at all —
loudest exactly when it was working, silent exactly when it was
broken. A balance list longer than the caller's buffer is an error for
the same reason: a position nobody saw is a position that reconciles by
not being there.

**HIP-4 netting: exposure is `|yes − no|`,** because equal legs are
riskless collateral. The reconciler and the risk gate must compute it
the same way or they will disagree with the venue — and a disagreement
between two of our own components, about a number the venue is the
authority on, is the worst possible place to find a bug. E6 must use
`exec_hyperliquid::recon::net_exposure_1e8` rather than restate it.

### One shared implementation, not two

`write_atomic` (temp file → `sync_all` → rename) moved from `cli` down
to `core-io` so the budget's state file uses the same one the VRP and
xsd state files use. The xsd path did NOT use it until this change:
`cli::xsd_boot::write_state` was a second copy with **no `sync_all`**,
live on the path that persists xsd positions. It survived because
nothing made the two copies share code. That function's entire point is a `sync_all` that
was once missing, and a second copy is how that bug comes back. Its
temp name now APPENDS `.tmp` rather than replacing the extension —
the old form was right for `*.tsv` and silently wrong for anything
else.

### E5 — the engine learns two verbs (2026-09-16)

The engine could submit and nothing else. Every strategy that wanted
to move a quote had to let it expire, and every strategy that wanted
to pull one had to wait for the venue. E5 gives `OrderDispatch` and
`Ctx` a `cancel` and a `modify`, and makes the PAPER arm implement
both — a trait method with only a default body is untestable.

#### `Ok` means "this call did it", never "it is not there"

The whole reason these return a `Result` is the difference between
those two facts. A cancel that finds no resting order returns
`NoSuchOrder`, not `Ok`: the order is indeed gone, but a *fill* is
what removed it, and a strategy told `Ok` will go on believing it
pulled a quote whose fill it has already booked. `no_such_order` is
therefore an **expected, non-zero counter** — it counts races lost to
a fill or a TTL, not errors.

`IdentityMismatch` is the opposite: a resting order *does* carry that
client id, but the request describes a different order.
`identity_mismatch` **must stay 0**, because nothing in normal
operation changes an order's identity — any non-zero value names a
caller that built the wrong request.

#### A `client_oid` is unique only inside its own slot

This nearly shipped wrong. Every member allocates `client_oid` from
its own counter starting at 1, `strategy-xsd` resets its counter on a
path that does not clear the open table, and `strategy-bin15` rides a
14-bit sequence — while **all enabled members share ONE
`PaperMatcher`**. So id 1 belongs to as many resting orders as there
are members quoting.

The lookup is therefore keyed on `(client_oid, strategy_id)`. The
slot is the NAMESPACE, not an assertion the caller is making, which
is why it belongs in the key while `sym` and `venue` are checked
*after* the lookup. Keyed on the id alone, a member's correct cancel
would find another member's order, be reported as `IdentityMismatch`
— a "not retryable, operator decision" answer that makes the member
abandon a quote that is still resting — and `identity_mismatch` could
never have held its must-stay-0 contract, because ordinary
multi-member operation trips it.

A member that reuses an id while its own first order still rests gets
`AmbiguousOrder`, not first-wins. Taking back one of two quotes that
answer to the same name and reporting success leaves the caller
believing both are gone. `ambiguous_order` must stay 0 too.

#### What a MODIFY may change

Price, size and client id. Nothing else. `core_types::OrderIdentity`
is the single statement of what "the same order" means — venue, slot,
sym, side, kind — and it exists as one comparable POD rather than a
five-way `&&` at each site so that a sixth identity field added later
updates every arm at once and no arm can quietly compare four of the
five. A modify that changes any of them is refused, not performed.

#### A reprice does not extend a quote's life

The modified order inherits the ORIGINAL `expiry_ns`, and the
replacement's own `ttl_ns` is read for nothing. Without this, an Arm
B repricing every 333 ms would hold a quote forever past the TTL its
ruleset set — the TTL would stop being a bound.

#### …but the replacement's `ts_ns` IS read, and is load-bearing

An earlier draft of this section said `ts_ns` and `ttl_ns` were both
"read for nothing". Only `ttl_ns` is. `ts_ns` is the modify's
decision clock — `PaperDispatcher::modify` passes it as `now_ns`,
exactly as `submit` passes `order.ts_ns` — and the activation delta
below is re-armed from it. **A replacement carrying a stale or zero
`ts_ns` lands with its activation already in the past and is fillable
at the NEW price on the very next tick**, which is the fabricated
fill the next paragraph exists to prevent. Stamp it from
`ctx.now_ns()`.

This is the same contract `submit` has always had; it is written down
here because the first caller will be a requote loop, which is
exactly where a reused or forgotten timestamp is easy to write.

#### A reprice re-arms the activation delta

**This is a deliberate departure from "preserve the original
`ts_ns`".** Δ_venue is the measured time an instruction takes to reach
the venue, and it is why an order cannot fill on a tick that arrived
before it. A new price is an instruction like any other: it is not at
the venue for Δ. Preserving `t_active_ns` across a modify would let
the paper matcher fill at a price the venue had not yet been told
about — a fabricated fill, the one class of error the matcher exists
to prevent.

The cost is that the OLD price, which really is still resting during
the flight window, cannot fill either, so the model under-fills a
modify by at most one Δ. Under-filling is recoverable. Inventing a
fill is not. If E6 or later wants the exact model, it is "the old
order until `now + Δ`, the new one after" — more machinery than this
commit should carry, and strictly harder to get right than the
conservative version.

#### A cancel has NO flight model, and that is a known optimism

`PaperMatcher::cancel` removes the order instantly, while `modify`
re-arms Δ. In reality the quote rests for another Δ after the cancel
is sent and can be picked off in that window, so the paper model
suppresses fills a live boot would take — the optimistic direction,
and the "paper looks better than live" hazard this file exists to
name. It is a deliberate omission, not an oversight: E5 has no live
cancel path to compare against, and a half-modelled flight window
would be a second number to reconcile. **E6 must revisit it once a
live cancel exists**, and until then a paper boot's absence of
cancel-window fills is a modelling artefact, not evidence.

#### A modify can RAISE an order's size

`remaining_1e6` is taken from the replacement, so a modify is a
resize in both directions. The E6 risk clamps are specified above as
landing "inside `RoutedDispatcher::submit`" — **a clamp on `submit`
alone leaves a size-raising path uncapped.** Whatever E6 gates on
must gate `modify` too.

#### LAW E-1 applies to a cancel, and harder

`RoutedDispatcher` routes a cancel and a modify through the same
three-way branch as a submit, on the request's own `strategy_id` and
`venue`. A live slot's verb that names an unrouted venue is refused
and counted — **never handed to the paper matcher**.

A mis-routed submit invents a fill. A mis-routed cancel invents the
ABSENCE of one: the matcher would remove a modelled order and report
success while the real quote stays resting at the venue, and nothing
downstream can detect the difference. `StampCtx` stamps a cancel's
`strategy_id` exactly as it stamps an order's, because an unstamped
cancel would route by slot `0xFF & 7` — a member able to pull another
member's quote.

#### THE OPEN GAP: `engine-orders.pmlr` cannot express either verb

The intent capture has one record type, `Order`, and no way to say
"and then I pulled that one" or "and then I moved it to 0.47". The
moment a boot performs a cancel or a modify, **the capture stops
describing it**: an offline replay models an order the engine had
already taken back, or fills the old price of one it had moved.

Appending the replacement Order would be worse, not better — a replay
would then see two submits and model two resting orders where the
engine had one that moved. So nothing is appended, and the gap is a
number instead: `engine::LifecycleCounters::capture_is_incomplete()`,
mirrored as `engine_lifecycle_cancels_ok_total` and
`engine_lifecycle_modifies_ok_total`. It reads the `_ok` fields and
not the `_err` fields, because a refused verb changed nothing and
leaves the capture correct.

**The gate is manual today.** `capture_is_incomplete()` has no
non-test caller: what actually reaches an operator is the two
metrics, and no replay, backtest or audit path consults either. The
Arm B commit owes it a real consumer.

**The offline harness is the same gap from the other side.**
`cli::backtest::BacktestCtx` implements neither verb, so it takes the
`Unsupported` default: the same member that cancels successfully in
the live paper engine gets `Refused` → `orders_dropped` in the
harness. Fail-closed, and therefore safe today — but every gate, OOS
verdict and pin runs through that harness, so the Arm B commit owes
it the verbs as well as the capture.

**Nothing in the tree emits either verb yet.** E5 commit 3 is
plumbing behind trait defaults. The commit that makes these counters
non-zero — bin15's Arm B requote — owes the capture a lifecycle
record type first, or owes an explicit decision that its boots are
not replay sources.

#### The parity gate

`crates/clob-dispatcher/tests/paper_replay_parity.rs` fingerprints the
raw bytes of every `Fill` a scripted stream produces, plus the seven
pre-E5 matcher counters. The constant was derived by running the same
script against the tree at `7f30752` — **before** the change — not
regenerated from the post-change tree, which would pin the new
behaviour to itself and prove nothing. It is accompanied by a
non-vacuity test, because a fingerprint over an empty fill stream
passes forever: the same "agreement over an empty set" that made E4's
reconciliation report green against a wallet it had never looked at.

#### `SubmitErr` has four names now

`RingFull` used to be the only one, and the engine mapped every
dispatcher error onto it — a slot the operator switched off arrived at
a strategy as back-pressure. Harmless while every caller dropped the
order either way; not harmless the moment a strategy must tell "retry
later" from "that order is already gone". The mapping is now an
exhaustive match with no `_` arm, so a new `DispatchError` variant is
a compile error rather than a silent re-merge.
