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

**Nothing in E1 can place a real order.** No live execution arm is
compiled for any venue — `cli::exec_boot::LIVE_ARM_VENUES` is empty and a
test asserts it — so any artifact marking a slot `live` REFUSES the boot
and names the phase (E2 signing, E3 HTTP) that will supply the arm.
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

**Until phase C has been run green against a funded testnet account,
E3's exit gate is met in its signature half only and must not be read
as met in full.**

## The live fill lane (E4, 2026-09-15) — BUILT, not ARMED

**`cli::exec_boot::LIVE_ARM_VENUES` is still empty and the
compile-time assertion holding it empty is untouched.** E4 did not
widen it. That edit is the moment this engine can move real money, and
it is not made by inference — it needs an explicit operator decision,
recorded here, and it has not been taken.

What is built: the cloid law, the request-budget governor, the
user-event scanner that turns venue fills into engine fills, and the
reconciliation parser. What is NOT built, and is required before any
mainnet order: the user-event WS transport and the dispatcher worker
that owns it, `HlExchange: OrderDispatch`, and the worker-side
`origin` split of §6.4.

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

*(The counter and the tape write themselves belong to the worker,
which E4 does not build. What E4 guarantees is that the worker cannot
reach the lane with a foreign fill by accident.)*

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
