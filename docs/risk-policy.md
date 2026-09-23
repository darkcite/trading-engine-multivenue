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
`docs/arch/regime-and-dashboard-plan.md` §7.1), and the single-regime
overfit is guarded by the regime GATE itself: a labelled row trades
only in the words it was evidenced in, UNKNOWN fails it closed, and
its label must earn the `--regime off` delta.

## Regime gate (RG0–RG7, 2026-09-03 →) — a gate, never a signal

`docs/arch/regime-and-dashboard-plan.md` §2 is the doctrine; the risk-relevant
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
- **HYPARB (H8) adds the HyperEVM testnet wallets** — the same
  `SecretKeyBytes`, read through `from_hex_env`; see "HYPARB — slot 0"
  below for which variable and why it may be the E3 testnet key.
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

#### The capture gap — opened by commit 3, CLOSED by commit 4a (2026-09-19)

E5 commit 3 left a real hole: the intent capture had one record type,
`Order`, and no way to say "and then I pulled that one" or "and then I
moved it to 0.47". A boot that performed either verb produced a
capture that no longer described it, and an offline replay of that
capture modelled an order the engine had already taken back.

**`Order.verb` and `Order.prev_client_oid` close it**, both taken out
of the slot's own explicit zeroed padding, so every Order ever
captured reads as `verb = PLACE` / `prev_client_oid = 0` — which is
what every one of them was. `EngineCtx::{cancel, modify}` append the
record on success, under the same capture-what-was-accepted law a
submit obeys: a refused verb changed nothing and records nothing.

**Why the verb rides the `Order` slot** rather than a second file: a
capture is a log of intents in the order they happened, and a cancel
is meaningless apart from the place it refers to. Two streams would
let a replay apply a cancel before its own place; one stream cannot.
That is the identical argument §7 of the exec plan gives for the
`ExecCmd` union ring, and the field meanings are deliberately the
same — `prev_client_oid` is the TARGET (0 for a place), `client_oid`
is the id the record's own order carries (0 for a cancel, which
creates no order).

**A modify is never recorded as a second place.** A replay that saw
two places would model two resting orders where the engine had one
that moved — worse than seeing nothing at all.

**A reader that does not know a verb byte DROPS the record.** It does
not fall back to "place". Applying a record whose meaning you do not
have is how a newer engine's capture quietly becomes a different
backtest; `FillEngine`'s `lifecycle.unknown_verb` counts them.

#### The offline harness replays them too

`cli::backtest::BacktestCtx` implemented neither verb, so it took the
`Unsupported` default: the same member that repriced successfully in
the live paper engine had every reprice refused there and counted as
a dropped order. Every gate, OOS verdict and pin runs through that
harness, so the divergence would have been measured as strategy
behaviour.

It now records both into the SAME ordered stream as its places, and
`FillEngine::intake` dispatches on the verb **inside itself** — there
are eight `intake` call sites across `backtest`, `backtest::member`
and `audit_pnl`, and a replay that applied a cancel at seven of them
would be silently wrong at the eighth.

`FillEngine`'s open table gained `strategy_id` for the same reason the
paper matcher's lookup is slot-scoped: `audit-pnl` replays ONE capture
holding every member's intents through ONE table, and every member
counts its `client_oid` from 1. `backtest --member` runs a single
member and would never have noticed.

`LifecycleReplay` is deliberately NOT in `ModelOutcome` and therefore
not in the frozen schema-1 line: it is a property of the CAPTURE, not
of the strategy's economics, and the schema is a contract with the
worker.

**Still true: nothing in the tree emits either verb yet.** Commits 3
and 4a are plumbing; bin15's Arm B requote is what will make these
numbers move.

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

### E5 commit 4b — Arm B moves and pulls its own quotes (2026-09-19)

bin15's maker had no cancel path, and its own code said so: *"this
member has no cancel path (Stage-3), so a live quote can only be
replaced by letting it expire. `requote_ttl_ns` is therefore the
replace cadence, not a nicety."* LAW E-7 lifts that gate.

#### A reprice is one MODIFY, not a wait and not a cancel plus a place

A live quote whose centre has travelled `requote_thr_1e6` now moves
with one request. At 333 reprices per instance the difference between
one request and two is the difference between fitting inside the
address budget and not (§7).

The re-quote-loop gate stays exactly as it was: a quote that reached
its TTL unfilled told us that price does not fill in this book, and
re-offering the same price a second later adds no information. Nothing
moves unless the fair value moved.

#### A partially-filled quote is left alone — unless it is an oversized ask

A modify replaces the venue's remaining size wholesale, so moving one
means the member's `filled_1e6`/`qty_1e6` pair and the venue's
remainder have to agree across a race — which is where double-count
bugs live. A quote that is already working does not need the help; its
TTL will end it. Counted as `skipped_partial`.

**The oversize pull outranks it.** A partially-filled ask that offers
more than the holding cannot be safely resized, so it is CANCELLED
rather than left: the gate that protects the bookkeeping must not
protect a short the venue will refuse.

#### The previous client id is remembered for ONE generation

A reprice gives the replacement a fresh cloid (ruling O-E5b), so a
fill the venue was already answering under the old id arrives after
the member has stopped listening for it. Without the memory it lands
in `unknown_fills`, moves no position, and the member believes it
holds less than it does — **the F7 shape again: absence read as
evidence.** `quotes_raced` counts them.

One generation and not a list, because the window is one round trip: a
second reprice means the first replacement was acknowledged, so
anything still outstanding under the id before that is a venue that
has lost two messages — a reconciliation problem, not a bookkeeping
one.

#### A refused modify changes NOTHING

The old quote is still resting at its old price and still reserving
its cap room. The reservation is credited and the replacement booked
only **after** the venue takes the modify: crediting first and then
being refused would free room for an order that never moved. The
replacement is sized with `cap_room_excluding_1e6`, because sizing it
against the caps as they stand would refuse room the quote itself is
holding and every reprice would shrink until it hit zero.

#### A reprice does not extend a quote's life

The modified leg inherits the ORIGINAL `deadline_ns`. The dispatcher
enforces the same rule on its own open table; this is the member's
book agreeing with it rather than trusting it.

#### A raced fill belongs to the RETIRED order, not the replacement

The first cut of this credited a `prev_oid`-matched fill to the
replacement's `filled_1e6`, and two independent reviews caught it.
`(qty_1e6, filled_1e6)` on a quote leg is the member's statement of
what the VENUE is resting, and it is the whole of the short rule —
`arm_take` sizes a closing take as `pos − (ask.qty − ask.filled)` and
`ask_free_1e6` reads the same pair. Crediting a raced fill there moves
both sides of that subtraction by the same amount, so the oversize it
creates becomes **algebraically invisible**: the resting ask and a
closing take would size against the same contracts, which is exactly
the "two sells of the same holding" BIN15 P4b (F3) exists to prevent.

It was also how the replacement got closed by someone else's fill. A
reprice may SHRINK a quote — the oversized-ask pull does exactly that
— so a raced quantity between the new size and the old one would mark
a still-resting order full, clear it, and send every later fill of
that order into `unknown_fills`. The F7 shape, reopened by the fix for
the F7 shape.

So a raced fill moves the POSITION and nothing else about the
replacement. It does re-book cap room: the old leg's whole reservation
was credited when the modify was taken, because nothing had filled at
that moment, and this fill says part of it was in fact spent. A BUY
re-books at the FILL's price, because that is the cash that left.

#### LAW E-8's member half — a lapsed quote is CANCELLED

The venue has no server-side TTL on a Gtc/Alo order, so a quote nobody
cancels rests forever. Before E5 the member simply forgot a lapsed
leg: correct against a paper matcher running its own TTL law, and a
stranded quote live.

**Two call sites, on purpose.** The requote pass usually gets there
first because it runs on every mark and every touch; the 1 s timer
sweep is the backstop, and it runs on a clock, so a family whose book
has gone quiet — exactly when a stale quote is most dangerous — still
has its quote taken back.

**The leg is cleared and its room credited either way**, as it always
has been: the member's own deadline governs its book, and keeping a
pending alive because a cancel failed would leak one forever. A
refusal that is not `NoSuchOrder` is counted as
`quotes_cancel_refused` — that is a quote the member has stopped
tracking and the venue may still hold, which is what E6's
reconciliation exists to find. `NoSuchOrder` is excluded deliberately:
there the order really is gone, which is what the member wanted.

#### The oversized ask, and the only way into it

Arm B's ask may only offer inventory we can prove we hold — a HIP-4
position cannot go negative and the venue refuses the sell outright.
An ask larger than the holding is an order the live venue will not
fill and the paper matcher takes happily: one of the few places the
model is OPTIMISTIC against live, which is the direction that matters.

**The reachable trigger is a raced fill on the ASK**, and only that.
A closing take cannot create the state — `arm_take` already subtracts
the resting ask's remaining before sizing — and a settlement cannot
either, because `clear_instance` zeroes the position and the quotes
together. What can is a sell the venue answered under the id the ask
carried BEFORE a reprice: the holding drops and the replacement keeps
its size. (An earlier draft of this section named the closing take and
the settlement; both were wrong, and the test that went with them
reached the state through a private method no engine path can drive.)

The ask is modified down to the free amount, and the trigger is the
inventory rather than the price — it fires with the centre standing
still. **Where the replacement cannot be sent** — nothing free to
offer, a size under the venue's lot or minimum notional, a partial
fill, or a refused modify — the quote is CANCELLED instead. Now that
the member has a cancel verb, an ask offering what we do not hold must
not survive to its TTL on any path.

#### What this commit changes about the standing engine

bin15 is slot 3 of the live `--paper` boot, so from the next release
build this member emits MODIFY and CANCEL into `PaperDispatcher`, and
`engine-orders.pmlr` starts carrying verb records (commit 4a is what
made that legible). `backtest --member bin15` will likewise produce
different numbers than before — the maker no longer waits out a TTL to
move — so **bin15 gate numbers measured before this commit are not
comparable with ones measured after it.**

### E5 — the budget counts what LEFT the host (2026-09-19)

Carried forward from commit 2 and closed here. `send_action` called
`on_action_sent()` **after** the `?` on `http.post`, so a request the
venue received and answered unreadably — a stalled server, a
connection that died mid-response — was never counted at all. The
governor drifted OPTIMISTIC, which `budget.rs` names as the wrong
direction: under-counting means exceeding the venue's real address
limit and then reading the rate-limit answer as a transport problem.

#### The fact lives where it is known, and the caller must look at it

`HlHttp::post` now returns `PostErr { err, left_host }`. `left_host`
cannot be derived from the `HttpErr` variant — `Disconnected` is both
"the connect failed" (nothing left) and "the peer went away
mid-response" (everything left), and `Timeout` covers the whole cycle.
A classifier over the variants would have been a name describing a
stronger property than its condition tests. So it is recorded inside
the HTTP cycle at the moment the write is attempted, and returned in a
struct every one of the fifteen call sites must destructure.

#### The live arm's stale keep-alive (H9c, operator ask, 2026-09-24)

Found by the HYPARB H9 review on `core_net::HttpsPost`, which copied
`HlHttp`'s shape. The venue closes an idle keep-alive connection on its
own schedule (and may announce `Connection: close`). `HlHttp` reused
such a connection: the next ORDER's bytes went into a socket the peer
had already closed, the local write succeeded, the read found EOF, and
the order came back `Disconnected` with `left_host == true` — counted
`sent_unanswered`, charged to the address budget, left to
reconciliation, although no byte of it reached the venue. An entry lost
exactly when the book was worth entering.

Now an answer that announces `Connection: close` (or arrives with the
FIN) retires the connection WITH the answer (the answer is kept), and a
one-byte non-blocking read before each reuse retires a connection the
venue closed while idle; the order dials fresh with `left_host ==
false`. Cost: one `read(2)` per order on a healthy connection, a TLS
handshake (50–150 ms) only when the venue had closed it — which the
order would otherwise have lost. `HlHttp::dials()` counts handshakes.
Pinned by `hl_exchange_tls_loopback::an_idle_close_is_noticed_before_the_next_order_is_written`
and `…::an_announced_close_retires_the_connection_with_the_answer`
(both RED with the fix disabled). It reaches the armed engine only
through a merge to `main`, a release build and an
`exec-smoke`-gated restart.

#### A torn write COUNTS

`left_host` is set before the write is attempted rather than after it
returns. We cannot tell a write that died on its first byte from one
that died on its last, and of the two ways to be wrong, sending fewer
actions than the venue allows is the recoverable one. **This half is a
documented choice rather than a tested one**: the TLS loopback cannot
reliably produce a torn write — a small request fits in the kernel
buffer and "succeeds" even into a dead socket — and a flaky gate is
worse than a stated assumption.

The two post-write failures that CAN be produced are tested:
`a_mid_body_disconnect_…` and `a_stalled_server_…` both assert
`left_host`.

#### `sent_unanswered`

New counter, and the most direct reason to run E6's reconciliation: an
action reached the wire and nothing in this process knows what the
venue did with it. There may be an order resting under an id no local
book holds. Before this commit the same failure was invisible — it
silently under-counted the budget instead. `HlExecCounters` grew from
four cache lines to five to carry it.

### E5 §7.1 — the two exit-gate items (2026-09-19)

#### The alloc gate: `hl_exchange_requote_path_is_zero_alloc`

Gate 60 drives the arm's own requote verb (`HlExchange::modify_by_cloid`,
since BX0-F3 through the `OrderDispatch::modify` the router calls), not
the raw encoders — with the budget floor at `u64::MAX`, so `send_action`
refuses at its barrier before any network work and only the encode
half runs. `seal` (the nonce/signature/envelope half of `send_action`,
split out for this) is driven beside it. Between them that is every
instruction a requote executes on the engine thread; the socket is the
only thing not covered, and gate 59 split `compare` for exactly the
same reason.

**`cancel_by_cloid` is deliberately not driven there.** It spends
`Spend::Cancel`, and `may_cancel()` is unconditionally true — a halted
engine must be able to flatten — so no budget setting can bar it. The
first cut of this gate did drive it, and would have opened two
thousand sockets to the live testnet; it was caught by the run
hanging. The gate now asserts `modifies_sent == 0` and
`refused_local >= 2000`, so the barrier holding is a fact the test
states rather than one it assumes.

#### Phase F asks the venue what is resting

§7.1 asks for the resting order's final state from `orderUpdates`.
Nothing parses those frames (the engine receives and skips them), so
the evidence comes from `frontendOpenOrders` instead — the endpoint
that echoes the cloid, which plain `openOrders` does not, and
therefore the only one that can tell our order from a stranger's. It
is read between the modify and the cancels, because a cancel destroys
the thing being observed.

**`confirmed_by_venue()` is a separate verdict from `passed()`**, and
deliberately so. `passed()` is the LAW E-7 asymmetry as the two
cancels show it, and a scripted loopback can produce those answers.
The readback needs a book containing our own timestamp-derived cloid,
which only a real run has. One predicate covering both would have had
to be satisfiable offline, and would have stopped meaning what its
name says. `exec-smoke --requote` requires both.

`new_resting` is required as a POSITIVE sighting. `!old_resting` alone
would also hold for a readback that found nothing at all — the
"agreement over an empty set" this lane shipped once already in E4's
reconciliation, and the loopback now pins it with an empty book.

#### MEASURED, testnet, 2026-09-19 02:12Z — §7.1 is met

`exec-smoke --requote --asset 100201820 --px 30000000 --px2 31000000
--sz 4000000000`. A post-only BUY of 40 contracts at 0.30 on the Yes
leg of outcome 20182, moved to 0.31, in a book quoting 0.499 / 0.539 —
far enough away that a post-only order rests rather than being refused
for crossing.

```
old 0xe35c000000000000000001a0b76f96ee
new 0xe35c000000000000000001a0b76f96ef
placed 60483073670 -> modified 60483074057
old_cancel_refused true   new_cancel_succeeded true
new_resting true   old_resting false   readback_failed false
unswept_old false   unswept_new false
```

**The venue's own book confirmed it**, which is the half §7.1 asked for
and the half the two cancel outcomes could only infer:
`frontendOpenOrders`, read between the modify and the cancels, showed
exactly one order resting and it carried the NEW cloid.

The modify issued a NEW oid again (`…3670` → `…4057`), as it did at
commit 1 — the third independent observation that cancel-by-cloid is
the durable handle and an oid captured at placement is not.

Run under the standing engine's own laws: `cargo build --release -p
cli` first (G0), the launchd instance booted out for the window and
bootstrapped back after, nothing left resting.

### BX0-F3 — the router's cancel and modify reach the arm (2026-09-23)

**The defect.** `HlExchange` implemented both lifecycle verbs as
INHERENT methods — `cancel_by_cloid` and `modify(prev, &Order)` — but
its `impl OrderDispatch` overrode `submit` alone. `RoutedDispatcher` is
generic over its live arm (`L: OrderDispatch`), and generic code can
call only trait methods, so every live slot's cancel and modify that
reached the router got the trait default, `Err(Unsupported)`, and never
left the host. Phase F's §7.1 measurement (above) drove the inherent
verbs from `exec-smoke`, not through the router, so it could not see
this: E5's "the engine learns two verbs" was true of the arm and of the
router separately, never of the pair. It stayed invisible because
bin15 has run with `maker_enabled = 0` — IoC entries only, nothing to
requote or pull. Had the maker gone live, a member could neither
requote nor take back its own quote; only the E6 halt's sweep could.

**The repair.** The inherent `modify` is renamed `modify_by_cloid`, so
no inherent method shares a name with a trait method (the shadowing
that hid the gap). The trait's `cancel` / `modify` delegate to
`cancel_by_cloid(req.sym, req.strategy_id, req.client_oid)` /
`modify_by_cloid(req.prev_client_oid(), req.order())`. Spend classes
unchanged: a cancel is an exit that no budget floor bars, a modify a
submit (the router has already risk-checked it as a `Replace`).

**The proof.** `exchange::tests::the_trait_cancel_and_modify_reach_the_cloid_verbs`
(the arm's own LAW E-4 refusal comes back through the trait and its
counters move) and
`…::the_trait_modify_names_the_resting_order_and_carries_the_replacement`
(the rendered action addresses the resting order by its cloid as `oid`
and carries the replacement's as `c` — a transposed pair fails it).
Break-and-watch: without the overrides both read `Unsupported`. Gate 60
now drives the TRAIT modify, the path the router calls, at 0 B/op. The
router→ledger half (`Ok` → `on_cancel` / `on_modify`) was already pinned
with a spy arm. The operator accepted this split (2026-09-23) in place of
the planned composed test — `RoutedDispatcher<Paper, HlExchange>` against
a loopback venue — which moves to BX3.

**Not re-measured on a venue.** The wire — cancel-by-cloid, and a
`batchModify` addressing the resting order by cloid — is Phase F's,
unchanged; BX0 changed only which caller reaches it. The bar before
bin15's maker is switched on live: `exec-smoke --requote` plus a
router-driven cancel/modify on testnet (plan §5 BX0 F3) — and, since a
happy-path run exercises none of them, a ruling or a test on each of
these paths, which the router could not reach before BX0 and now can
(risk-reviewer, 2026-09-23; UNVERIFIED until then):

1. **An uncertain modify.** The router renames the ledger row on `Ok`
   and keeps the old one on `Err`; a modify whose reply timed out or
   could not be read, but which the venue applied, leaves the
   replacement unbooked and the caps under-counting. It needs the
   E-5 "sent, unanswered" treatment: resolved by a readback, booked
   conservatively meanwhile.
2. **A modify the venue turns into a cancel** (a post-only replacement
   that would cross): the old row stays in the ledger for an order that
   no longer rests, and its later cancel returns `Err`.
3. **A modify across an instance roll** (`instance_of(prev)` ≠
   `instance_of(order.client_oid)`) is not refused by `stage_modify`,
   which looks up the replacement's instance only.
4. **Ledger edges:** a late partial fill of the old id after
   `on_modify`; `on_modify` for an old id the ledger does not hold; a
   modify that flips side (the HIP-4 ledger has no shorts).
5. **A refused requote** (risk gate or budget floor) leaves the old
   quote resting at the old price — the member must pull it.
6. **Two members quoting one leg from one address** (`note_owner`
   counts the contest and nothing more): self-trade prevention by the
   venue would cancel without a fill, which a fill-fed ledger never
   sees.

### BX0 — `ingress-binance` joins the zero-copy gate (2026-09-23)

On the operator's word (plan O-BX12d): every pre-existing zero-copy
finding in `ingress-binance` fixed, and the crate put in
`make copy-audit`'s scope.

**The gate had a blind spot.** `scripts/copy-audit.sh` stopped reading a
file at its first `#[cfg(test)]` line, on the convention that test
modules sit at the end. Three files break the convention with a
test-only METHOD mid-file, so everything after it went unaudited:
`exec-router/src/routed.rs` from line 358 (≈ 920 lines of non-test
code), `exec-hyperliquid/src/exchange.rs` from line 898 (≈ 1 570) and
`ingress-binance/src/run_loop.rs` from line 436 (≈ 1 010). The sweep now
skips a `#[cfg(test)]` ITEM to its own end and resumes: a brace-less
line's `;`, a braced one-liner, or a block's closing brace at the
attribute's indentation. It refuses to guess: `#[cfg(test)]` on a field,
a variant, an arm or a multi-line expression, or an item that never
closes, fails the sweep (exit 3). A `COPY:` marker inside a test item no
longer covers the live copy below it. `scripts/copy-audit-selftest.sh`
proves all of that on fixtures (14 live copies found and none inside a
test item; 6 undelimitable constructs refused), and `make copy-audit`
runs it first. A probe copy injected at the end of each formerly blind
region was flagged in all three files. The exec lane's formerly blind
code is clean: no new hit.

**What the pass removed** (copies gone, not commented):

* `parse_book_ticker` / `parse_mark_price` returned a 64 B `align(64)`
  frame inside an `Option` — 128 B by value, on every bookTicker and
  markPrice push. Both now fill the caller's frame in place
  (`&mut Frame` → `bool`) and write it only once every field has
  parsed: a failed parse leaves it untouched (a unit test, and both
  fuzz targets assert it on every input). `parse_trade` keeps its
  `Option` — under 64 B, asserted at compile time.
* The spot sentinel's SUBSCRIBE was re-assembled through a stack
  scratch on every (re)connect. Its stream symbol is now read from the
  slot's own endpoint path, and the request's four parts go straight
  into the masked frame through the new
  `core_net::ws_write_text_frame_parts` (core-net's one frame
  serialiser now writes a payload from parts; the single-payload
  writers call it with one). Nothing is composed or stored: the
  Driver's 32 B sentinel stream buffer is gone.
* The Ping echo went rx → stack scratch → tx; it goes rx → tx.
* `MultiConn` copied its host and path into two `Vec`s per connection;
  it borrows them from the boot's endpoint list.
* `parse_filters` copied each `filterType` into a 32 B buffer to
  compare it; it compares the borrowed span, so an over-long
  `filterType` no longer rejects its row (tested).
* The discovery rows (`BnSymbolRow`; `EapiOptionRow`, 72 B) crossed a
  return by value together with their end offset; they are parsed in
  place into their table slot.
* `options-select` sizes its output once (`with_capacity`), so a
  selected row is copied in exactly once, never again by a growing Vec.

**What the pass commented** (designed copies, each with its `// COPY:`):
the ≤ 32 B symbol and ≤ 16 B underlying copied into each discovery row
at boot (the row outlives the REST body it was scanned from, and the
fetch buffer is reused by the next request); the options selection
law's by-value rows (72 B, ≤ 64, once at boot — references into the
table would change the three-venue law for ≤ 4.6 KB); the ≈ 2.9 KB
Driver moving into its slot 2–4 times per connection at boot (every
slot's `StreamLane` is sized for the 2 568 B options table it carries
inline); the transport moving into its slot once per reconnect; and the
test views that return a frame's `Option` by value.

**The auditor on the pass** (`zero-copy-auditor`, Opus): PASS — no hot
copy left in `ingress-binance`; RX = 4 copies against the target of 3
on every Binance lane, the extra one in core-net (below). Its cold
findings were all acted on. The first sentinel fix carried a `// COPY:`
whose reason was false ("the driver cannot borrow the boot strings" —
the symbol is in the slot's own path), so that copy was removed rather
than re-worded; the Driver, transport and test-view moves got honest
markers; the options table's marker now states its real size and its
second move. Its review of the first version of the new skip logic
found shapes that would still have swallowed live code silently (a
braced `use`, an empty-bodied method, a field or a variant, a struct
literal, a multi-line array); the reader above delimits or refuses
each, and the self-test pins them.

**Escalated, not changed:** core-net's RX path keeps a fourth copy —
rustls' plaintext into the caller's rx buffer — and its `// COPY:`
rejection ("rustls has no in-place plaintext borrow") is out of date for
the `unbuffered` API of rustls 0.23 (the pinned 0.23.38). The auditor
also flagged that the buffered API may stage each received record's
plaintext through a heap `Vec` — an allocation per record that the
alloc gate cannot see, because it drives a test transport. UNVERIFIED:
it needs an allocation count over a real TLS loopback before anyone
moves the engine's TLS reader.

**Measured since the HYPARB merge (2026-09-23).** HYPARB H9's bench
gate 72 is that count: `HttpsPost` against a rustls loopback node, in a
child process, allocates EXACTLY 2 per request — one per record sealed,
one per application-data record decrypted (rustls 0.23's buffered API)
— once `TlsTransport::read`'s `WouldBlock` stopped allocating (3 more
per drain loop until then). Record: "HYPARB — slot 0" → "What H9 fixed
on the write path". The move to the unbuffered API stays the
operator's transport decision.

Gate after the pass: `hits=32 baselined=32 new=0 paid=0` over the exec
lane, `core-net` and `ingress-binance`. The baseline shrank by one (the
old single-payload copy in `ws_frame.rs`, now the marked parts write)
and did not grow.

### BX0 — the other ingress crates parse in place too (2026-09-23)

On the operator's word ("the okx, deribit, hyperliquid and mexc parsers
return frames by value too — fix it as far as you've found it"): the
pattern the BX0 pass removed from `ingress-binance` was measured across
every other ingress crate and removed there as well — `ingress-okx`,
`-deribit`, `-hyperliquid`, `-mexc`, `-bybit`, `-polymarket` and `-rpc`.

**What the pass removed** (copies gone, not commented):

* **30 parsers** returned a 64 B `align(64)` frame inside an `Option` —
  128 B by value (192 B for `DeribitTickerFrame`, 256 B for HL's
  `DepthTopK`) on every push they parse. Each now fills the caller's
  frame in place (`&mut Frame` → `bool`, `#[must_use]`) and writes it
  once, only after every field has parsed, so a failed parse leaves it
  untouched. The one exception is `parse_l2book_depth`, whose level
  carrier fills as the walk goes and is never read on `false` (its
  header obeys the rule).
* **Frames inside the dispatch value.** The two-phase run loops carried
  the parsed `Tick` / frame / head inside `Dispatch` from phase 1 to
  phase 2 — every dispatch value ≥ 128 B (a 64 B `align(64)` payload
  plus its tag), and mexc's crossed two function returns by value. The frame now lives in a phase-1 scratch the arm
  writes in place; the variant only says it was written. Each run
  loop's `Dispatch` is const-asserted ≤ 64 B (HL's 64 B roll spec is
  parked in the driver to fit).
* **OKX's 144 B `TradeScan`** staged sixteen seq ids for phase 2. The
  seq monitor is a driver field disjoint from the rx borrow, so the walk
  chain-checks the same first 16 rows as it reads them; the scan is
  12 B.
* **The top-K change gate** kept the last snapshot by copying the new
  one over it (192 B per changed push, OKX / Deribit / HL). The new
  `core_types::DepthPair` holds two rows: the snapshot is written in
  place into the spare one and becomes the last by an index flip.
  `DepthLadder::snapshot_into` / `top_k_into` replace the 192 B / 80 B
  by-value `snapshot` / `top_k`.
* **HL walked an outcome leg's `l2Book` twice** (header, then depth):
  `parse_l2book_depth` now yields the header from the same walk.

**What the pass commented** (designed copies, each with its `// COPY:`):
the 192 B `DepthTopK` pushed into the depth ring (OKX, Deribit) —
core-ring has only a by-value `try_push`; the boot-time discovery rows
(Deribit, MEXC ×2, HL, Polymarket ×2, OKX; 80–264 B, once per row);
`DepthPair::new` at boot; the HL test recorder; and one marker per test
view (the views are now one `#[cfg(test)] mod views` per file). Two
by-value returns at the bound are pinned by compile-time asserts:
`Option<MexcSpotFrame>` (64 B only through `MexcChannel`'s niche) and
Deribit's 56 B `TradeScan`.

**The fuzz contract is checked against non-zero bytes.** Starting a
target from `ZERO` could not tell "untouched" from "zeroed on the way
out". Thirteen targets now start from a `0xA5`-filled frame
(`fuzz/fuzz_targets/common/poison.rs`; `poisoned::<T>()` is bounded on
the `unsafe trait AnyBits`, implemented only for the 28 frames checked
field by field as integer-only `repr(C)`). `parse_price_change_row` and
`parse_l2book_depth` are now fuzzed and alloc-gated; `hl_l2book` —
broken since `HlStaleness::arm` took the coin table — compiles again
and runs a header-vs-depth differential.

**The auditor on the pass** (`zero-copy-auditor`): PASS — no hot hidden
copy left on these lanes; RX = 4 against the target of 3, the extra one
core-net's rustls copy (above). Its findings were acted on: the poison
helper's first soundness argument (a niche check) was not a proof, so
the proof moved into the `AnyBits` contract and the niche check stayed
as a tripwire; three more cold discovery returns got markers; two
at-bound returns got size pins; stale docs were corrected.

**Open, not in this pass:**

1. **core-ring moves by value.** `try_push(T)` and `try_pop() ->
   Option<T>` — every consumer pops a 128 B `Option<Tick>` per tick and
   a 256 B `Option<DepthTopK>` per snapshot. An in-slot claim/commit
   push and a `pop_into(&mut T) -> bool` is its own change, across
   every consumer.
2. **These seven crates are not in `make copy-audit`.** A sweep with
   the script's own reader finds 51 unmarked copy verbs, all older than
   this pass. The hot one is the WS Ping echo through a 125 B stack
   scratch in OKX, Deribit, HL, Bybit, RPC and Polymarket (BX0 removed
   it from Binance; MEXC marks it); the rest are cold (subscribe,
   resync and log renders, boot symbol copies, Bybit's boot `to_vec`).
   Whether they join the gate is the operator's call.

Gates after the pass: clippy clean; nextest 2743 passed (3 skipped; one
earlier run hit the known parallel-load flake
`hl_userws_loopback::a_frame_larger_than_the_buffer_is_refused_not_grown`,
green alone and on the rerun); alloc 64/64 at 0 B/op; `make copy-audit`
new=0; license-check OK; fourteen fuzz targets 60 s each, no crash
(1.1 M–31 M runs); live smokes 60 s — MEXC 31 002 messages, Binance
59 819 — both with 0 parse errors and 0 reconnects.

## E6 — the risk gate and the kill switches

### E6 commit 1 — the per-order clamp (2026-09-19)

`exec.toml`'s `max_order_usd_1e6` has been parsed, carried into
`ExecRoute` and printed at boot since E1, and **read by nothing**. The
risk-reviewer called the E1 caps "declared-not-enforced". They are
enforced now, in `RoutedDispatcher`, before dispatch.

#### It is a SECOND OPINION, not a copy

bin15 has its own `cap_instance`/`cap_day` ledger and sizes every order
against it. This clamp is the operator's number, checked on the
dispatch path rather than the sizing path, computed from the request in
front of it rather than from anything the member believes. **A non-zero
`refused_risk` means the two disagreed** — a member asked for something
its own caps should already have stopped — and that disagreement is the
alarm, which is why it is a counter and a `/metrics` row
(`engine_exec_refused_risk_total`) rather than a silent clamp.

#### Both verbs, because a modify can RAISE size

A clamp on `submit` alone leaves the cap reachable by repricing upward.
The E5 commit-4b review named that hole while the modify path was being
built; it is closed here, and the replacement is measured exactly as a
fresh order is.

#### The live arm only

A paper slot is modelling, and the offline harness replays the same
intents through no such gate. Refusing a paper order here would make
the engine and the harness disagree for a reason that has nothing to do
with the strategy. The clamp exists to stop real money leaving.

#### `i128`, and why it is not fussiness

`px × qty` leaves `i64` at about 9.2e18 — a $4 m price and three
contracts reaches it, which is inside the range a fat-fingered
`exec.toml` could ask for. A wrapped product is NEGATIVE and sails
straight past a `>` test; a saturating one is positive but is a number
nobody computed. The multiply is done in `i128`, and the wrap case is
pinned by a test that asserts its own premise (`checked_mul` really
does return `None` for those inputs).

The boundary is `>`, not `>=`: an order exactly AT the cap is what an
operator who wrote that number asked for.

#### NOT in this commit: `max_open_orders`

It is in the same `exec.toml` block and equally unenforced, but it
cannot be done honestly yet. The router sees submits, cancels and
modifies — it does not see FILLS or TTL expiries, so a count kept from
dispatches alone drifts upward and would eventually refuse everything
for ever. The paper arm can answer exactly (its open table now carries
`strategy_id`); the live arm cannot answer per-submit, because a
partial fill does not retire a resting order and only
`frontendOpenOrders` knows the truth — which is the reconciler's
cadence, not the dispatch path's.

So `max_open_orders` lands with the venue-fill ledger, where there is
something real to count. A clamp that refuses everything after the
sixty-fourth order of a boot would be worse than no clamp: it would be
a check whose name says "too many open" while its condition says
"sixty-four have been sent", which is the defect shape this lane keeps
finding.

### E6 commit 2 — the venue-fill exposure ledger (2026-09-19)

Commit 1 could enforce `max_order_usd` because a single order's
notional is entirely contained in the request: nothing has to be
remembered to judge it. The other three clamps are not like that.
`cap_instance_usd`, `cap_day_usd` and `max_open_orders` all ask a
question about the PAST, and the router had no memory of one.

`exec_router::ledger` is that memory, and this commit gives all three
a number to check against.

#### The router could not see a live fill at all

This is the fact the whole commit turns on, and it was not obvious.
`OrderDispatch::on_venue_event` carries a `ChannelEvent` — market
data, never a fill. `OrderDispatch::try_next_fill` carries the PAPER
arm's fills only: `RoutedDispatcher` forwards it to `self.paper` and
nothing else, because the live arm pushes into the engine's own fill
lane 3 and the engine drains that lane directly (the "Where live fills
come from" note in `exec_router::routed`).

So the component that refuses orders against an exposure cap sat
downstream of nothing that could tell it a position had changed. A new
trait hook, `OrderDispatch::on_fill_booked`, closes that: the engine
calls it from the fill-lane drain and from the dispatcher fill pump,
in both cases **before** the strategy sees the same fill.

That ordering is load-bearing, exactly as the roll handler's is. A
member handed a fill may submit in the same call, and a ledger that
had not yet booked it would size the refusal against the position as
it was BEFORE the trade — which is precisely the order the cap exists
to stop. `engine::tests::a_fill_is_booked_before_the_strategy_can_act_on_it`
and its fill-pump twin pin it.

#### A row is a FAMILY, not an outcome — and that was nearly wrong

The first cut of this ledger keyed its rows on the **outcome id**. It
would have failed open inside half an hour of live running, and the
review that caught it raised the point as a *question* rather than a
finding: "please state as a fact whether the outcome id is stable
across instances; the whole sizing argument rests on it."

It is not. `ingress_hyperliquid::run_loop::perform_roll` rebinds a
**family** to a brand-new `HlOutcomeSpec` on every roll — new outcome
id, new coins, new symbols. A BIN15 family rolls every fifteen
minutes. So a table keyed on outcome id consumes a fresh row each
roll and, with eight families and sixteen rows, is full after two of
them. Then every bind is refused, every fill lands as
`fills_unbound`, and `cap_instance` silently stops seeing the
position it exists to bound.

Keyed on the family index — which is the ingress's, shared across
members, and is exactly what `strategy_bin15::on_roll` keys on — a
roll REPLACES the row it already owns and the table never grows.
`successive_instances_of_a_family_reuse_one_row` drives a hundred
rolls of eight families and asserts `binds_refused == 0`.

A **settled** roll is different from a created one here: it drops the
resting orders (LAW E-8) and marks the row settled, but **keeps the
binding and leaves the position alone**.

The binding is kept because the ingress keeps the coins bound and
subscribed on a settle for the same reason, and the venue reports
SETTLEMENT down `userFills` like any other fill — a freed row would
leave that fill counting as `fills_unbound`, a counter meaning "a
position the router is not tracking", on the one event guaranteed to
end every instance.

The position is left alone because the contracts ARE still held until
the settlement cash arrives, so zeroing would report a flat book over
a real one for the length of that window. It would also make every
settlement fill land on a zero leg and trip `sells_below_zero` —
eight families rolling four times an hour is on the order of 768 a
day — burying the one signal that counter was added to carry. The
settlement fill reduces the position naturally; the successor's
CREATED roll clears any residue if that fill never arrives.

`book_fill` reads the `settled` flag for two more things: a floored
sell on a settled row is not counted as a disagreement with the
venue, and a settled row never charges the day cap, because a
settlement is the venue paying out rather than the member committing
capital.

A settle frame naming **outcome 0** is REFUSED into
`settles_unmatched` rather than falling back to the family key. That
fallback would clear whatever the family currently holds, which after
a reordered frame is the successor's live position — the very thing
the outcome-first search exists to prevent. A duplicate settle is a
no-op.

The row's key is **(venue, family)**, not the family byte alone. A
family index is only unique within the ingress that issued it, so two
venues' family 0 would otherwise land on one row and the second bind
would retire the first venue's live position — a fail-open from a key
collision that no number of rows fixes.

#### Three ledgers, deliberately not one

| ledger | question | fed by | reset |
|---|---|---|---|
| exposure | how much is at stake right now | fills | the instance's roll |
| turnover | how much has been bought today | fills | 00:00Z |
| resting | how many orders are working | submit/cancel/modify **and** fills | the instance's roll |

**The first two are computed from fills ALONE and never consult the
resting table.** That separation is load-bearing rather than tidy: the
resting table is the only part that has to match orders to fills by
`client_oid`, which is the one place a key can be ambiguous. Keeping
exposure and turnover off that path means a defect in open-order
tracking degrades `max_open_orders` loudly — into
`resting_ambiguous` — and cannot silently corrupt the number the money
caps are judged against.
`an_ambiguous_key_does_not_stop_the_money_ledgers` asserts it.

#### Two clocks, one day epoch — the other near miss

`core_time::now_ns` is `CLOCK_MONOTONIC_RAW`: nanoseconds since an
arbitrary origin, **not** since the Unix epoch. That is what stamps an
`Order`. A VENUE `Fill` is stamped by the Hyperliquid arm from
`SystemTime` and is Unix time. The two differ by decades.

The first cut fed both to one `wall_ns / DAY_NS` field. Every
alternation between an order and a fill would have looked like a
midnight crossing and zeroed the day's turnover — `cap_day` disabled
outright, failing open, with nothing to show for it but a climbing
`day_rollovers`.

`strategy_bin15` already solves this with `core_time::WallAnchor`
(`self.bar.anchor.wall_of(now_ns)`), so the ledger uses the same
mechanism rather than a second one: `Ledger::new` takes the boot
anchor, `observe_mono_clock` converts, and `roll_day` only ever sees
wall nanoseconds. `Ledger` has **no `Default`** any more — a zeroed
anchor maps every engine timestamp to 1970, which is one fixed epoch
and looks exactly like a day cap that never rolls, so forgetting the
anchor is now a compile error.

No test could have caught it: every one of them built both stamps
from a single constant. `the_two_clocks_do_not_thrash_the_day_epoch`
builds the anchor the way boot does, with an eleven-day monotonic
origin, and alternates the two sources across sixty-four minutes.

#### An unreconciled ledger refuses — the restart hole

A fresh `Ledger` reads zero exposure, zero turnover and zero resting
orders. After a **restart** that is not the truth: the venue still
holds whatever the previous boot left. All three ledger-fed clamps
would fail OPEN — a whole `cap_instance` addable on top of an
existing position, a fresh `cap_day` on top of the day's real spend,
and `max_open_orders` more orders on top of the ones already working.
CLAUDE.md documents a scheduled daily restart, so this is routine,
not exotic.

So a live PLACE is refused until something has reconciled the ledger
against the venue. **Nothing does yet** — the reconciler wiring is
commit 3 — which means a live slot armed on this commit alone
refuses every order it tries to place. That is the intended reading:
a risk gate with no memory of the venue must not pass orders, and a
slot that refuses loudly in its first second is a better failure than
a cap that was never really there.

**Both verbs.** The first cut exempted a MODIFY, reasoning that
refusing one would strand a quote at the venue with no way to move or
shrink it. That reasoning was simply false: `cancel` is never
risk-checked at all, so a member always has a way to take a quote
back, and waiting for the reconciler is not being stranded. What the
exemption actually bought was the one thing the interlock exists to
stop — a modify RAISES price and size, the venue holds pre-boot
orders across our restarts, and the exempted verb would have been
judged against a ledger reading zero. Commit 1's own note names that
hole in those words: "a clamp on `submit` alone leaves the cap
reachable by repricing upward."

A cancel stays open, and that is the escape hatch.

The refusal has **its own counter**, `refused_unseeded`, rather than a
share of `refused_cap_instance`. Nothing calls `mark_ledger_seeded` in
production on this commit, so on a live boot *every* refusal is this
one; folded into the exposure counter it would send an operator to
look at a `cap_instance_usd` number that has nothing to do with why
their orders are being refused — the same defect as `refused_risk`'s
changed meaning, made twice.

#### The keying: `(client_oid, slot)`, never the oid alone

Every member counts its own `client_oid`s from 1, so two slots share
oid 11 routinely. Keying on the oid alone is the exact defect the E5
commit-3 review found in the paper matcher's `find_resting`, where a
member's correct cancel found another member's order. The same lesson,
applied before it could be made twice.

#### What each cap MEANS here, and where it differs from bin15's

bin15's own `cap_instance`/`cap_day` count **reserved entry
notional**: booked at submit, credited back on the unfilled part,
buys only. The router's count **what the venue actually filled**.
Those are different quantities, deliberately:

* `cap_day_usd` — filled BUY turnover, cumulative within the day,
  reset at 00:00Z on the same `DAY_NS` epoch bin15 uses so the two
  roll at the same instant. Sells are not counted: the day cap asks
  how much was committed, and selling a position back does not
  un-commit it. A ledger that netted sells off would let a member
  round-trip an unbounded notional under a fixed cap.
* `cap_instance_usd` — NET EXPOSURE, `|yes − no|` per outcome, summed
  across outcomes. Money actually at stake rather than money spent.

So **a gap between the router's `cap_instance` and bin15's is not
automatically an alarm** the way a `max_order_usd` refusal is. A
member can sit well inside its own turnover cap while holding a
one-sided position this clamp refuses to add to, and that is the clamp
working. `refused_max_order` is the field that means "the two ledgers
disagreed"; the other three mean "the operator's ceiling was reached".
The `refused_risk` aggregate commit 1 shipped is kept as their sum, so
a dashboard built against it keeps parsing. **Its MEANING changed
while its name did not**, which is the more dangerous half: in commit
1 a non-zero value was an alarm, and three of its four contributors
now mean "the operator's ceiling was reached", which is the clamp
working. An alert wired to `engine_exec_refused_risk_total` should be
re-pointed at `refused_max_order`, the one field that still carries
the commit-1 meaning. **A non-zero `refused_cap_instance`,
`refused_cap_day` or `refused_open_orders` is not an alarm.**

#### Netting is per outcome, and then SUMMED

HIP-4 pays $1 to one leg of an outcome and $0 to the other, so equal
Yes and No is riskless collateral. That is
`core_types::net_exposure_1e8` — the reconciler's own rule, and now
the only copy of it (see below).

Two DIFFERENT outcomes do not net. They settle on independent events,
and a member long Yes on one and long No on another is exposed to
both. So the slot's number is `Σ over outcomes |yes − no|`, never
`|Σ yes − Σ no|` — the latter would report a flat book over real risk.
`two_outcomes_do_not_net_against_each_other` holds it, and breaking
the sum is one of this commit's break-and-watch runs.

#### The clamp PROJECTS, and it never blocks a reduction

A slot flat at zero passes any test on its *current* exposure, so a
cap read off the position would never refuse a slot's first order
however large — it would start biting only on the second, which is one
order too late.

So `cap_instance` is checked against what the order would LEAVE, and
the projection is exact wherever the leg is bound: buying the shorter
leg of an outcome *reduces* `|yes − no|`.

The condition is two tests, and the second is not redundant:

```rust
if projected > cap && projected > current { refuse }
```

**Over the cap is not enough — the order must also INCREASE exposure.**
A slot can be over its cap without having asked to be (the operator
lowered the number, or fills landed past what any projection could
have known), and the only way out of a position is to send an order. A
clamp testing `projected > cap` alone refuses exactly that order and
TRAPS the member inside the exposure the cap exists to bound, with no
path back but an operator cancelling by hand.

This was written wrong first. The test
`the_instance_cap_never_refuses_an_order_that_does_not_raise_exposure`
was written to state the property and failed against the code, which
is the only reason the trap door is not in this commit. There is no
way to nibble upward: any increase at all fails the second test once
the first is failing.

`cap_day` needs no such guard (a sell adds no turnover, so selling is
always possible) and neither does `max_open_orders` (cancels and
modifies always pass).

#### `max_open_orders`, finally honest

Commit 1 left it out because the router saw dispatches but not
retirements. It now sees all four events that change the count:

| event | effect |
|---|---|
| accepted submit | +1 |
| accepted cancel | −1 |
| accepted modify | 0 — LAW E-7 replaces in place; the id and size move |
| fill taking remaining to ≤0 | −1 |
| roll of the instance | drop every order on its two legs |

**Only ACCEPTANCE counts.** An order the arm refused is not working at
the venue, and counting it would leak the count upward until the clamp
refused a slot holding nothing.

Dropping a rolled instance's orders is not a guess about the venue:
LAW E-8 says the roll sends a real cancel-all, so an order on a
retired leg is gone. Keeping them would leak one instance's quotes
every roll — the exact failure that kept this clamp out of commit 1.

A MODIFY is exempt from the clamp itself. Testing it would refuse the
requote of a slot sitting exactly at its cap, which is the slot that
most needs to be able to move its quotes.

**The blind spots, stated in full** — and "in full" is the point: an
earlier draft named only the first of these and read as if the list
were complete.

| blind spot | direction | where it goes |
|---|---|---|
| a venue-side reject AFTER the HTTP ACK | CLOSED, count one high | a halt trigger in commit 3 |
| a restart with live state at the venue | was OPEN | closed by the seeding interlock above |
| the shared resting table filling | was OPEN | closed both ways — see below |
| a fill on a leg whose bind was refused | OPEN for exposure, CLOSED for turnover | `fills_unbound`, a halt trigger in commit 3 |

#### The shared table can no longer switch the clamp off

`max_open_orders` is counted out of one 512-row array shared by every
live slot. When that array filled, the first cut bumped
`resting_full` and returned **without incrementing the slot's
count** — so the count froze and every later order passed. A clamp
switched off by the table it depends on, failing open, for the rest
of the boot.

Closed from both ends:

* `on_submit` now increments the count even when no row was taken.
  The order IS working at the venue — the same reading
  `on_modify`'s unmatched branch takes — so the slot ratchets toward
  refusing everything instead of toward passing everything. Fail
  closed.
* `core_config::exec` refuses at boot any live slot whose
  `max_open_orders` exceeds its equal share of the table
  (`LEDGER_RESTING / EXEC_SLOTS` = 64, which is what the shipped
  template asks for). Equal shares rather than a sum, because slots
  are validated independently and a sum rule would make one slot's
  legality depend on a later section of the file.

That boot check bounds the SUBMIT path. It does **not** make the
fail-closed path unreachable, and an earlier draft claimed it did: a
modify is exempt from `max_open_orders` under LAW E-7, and an
unmatched modify tracks its replacement, so the count is not bounded
by the clamp. Reaching the table's end needs a stream of modifies
naming orders the ledger never saw — a disagreement with the venue,
which `resting_full` is now the tell for, not a configuration error.

`core_config` has to restate `LEDGER_RESTING` (`exec-router` depends
on it, so the dependency cannot run the other way). An earlier draft
of that comment said `exec_router::route::tests` asserted the two
agree. **It did not** — which is worse than no check, because a false
claim of coverage stops the next reader adding one. `cli::exec_boot`,
the one crate that depends on both, now const-asserts it.

#### A modify of an order the ledger is not holding

`on_modify` tracks the replacement — it IS working at the venue — but
the first cut recorded it with `SYMBOL_ID_NONE`. A row with no symbol
matches no leg, so `drop_resting_on` could never release it and no
roll, no LAW E-8 cancel-all, would ever retire it: one place leaked
out of the shared table per unmatched modify, for the life of the
boot. The condition that makes the table fill, caused by the branch
that handles the table being unreliable. The replacement's real `sym`
is carried now.

The same branch also fired on an AMBIGUOUS key, because it asked a
function returning `Option` a question with three answers. Two rows
under one key are already a guess; adding a third under the same key
is the opposite of the ruling `consume_resting` follows and is proud
of. `find` returns `One`/`None`/`Many` now, and `Many` leaves the
table alone and counts.

#### Fixed tables, and they refuse rather than evict

`LEDGER_ROWS = 16` (twice one ingress's `HL_MAX_FAMILIES`, with the
venue byte in the key so a second venue's family table cannot evict
the first's) and
`LEDGER_RESTING = 512` (`EXEC_SLOTS × max_open_orders` at the shipped
64 — sized TO the clamp, so filling up is not reachable while the
clamp is doing its job). Past either, the request is refused into
`binds_refused` / `resting_full` rather than overwriting a live row.
An overwritten binding books a real position against the wrong
outcome; if `resting_full` is ever non-zero, either the clamps are
unset or something is not retiring, and both are operator-visible
facts rather than silent truncation.

#### What `cap_instance` does NOT bound

It is computed from FILLS. A slot's own resting orders are not in the
projection, so N orders in flight are each judged against the same
unchanged position and all N can pass. **The worst case with every
quote working is `cap_instance_usd + max_open_orders × max_order_usd`,
not `cap_instance_usd`.** The three numbers multiply, and an operator
setting them needs to know that.

This is a stated bound rather than an oversight: projecting resting
notional too would mean assuming both sides of a two-sided quote
fill, which cannot happen, and it would strangle the maker. bin15's
own ledger reserves notional at submit and self-limits in the healthy
case; this gate's job is the unhealthy one, and there it bounds
exposure at that product.

Two related shapes, both by design and both worth an operator
knowing:

* **Alternating legs.** `buy yes 10`, `buy no 20`, `buy yes 20`, …
  pins `|yes − no|` at the cap while gross position grows without
  bound. `cap_instance` is a NET measure; `cap_day` is what bounds
  capital committed.
* **Flip-ratcheting.** From over the cap, an order that reduces net
  exposure passes even if it lands far on the other side. Net
  exposure never rises — the "no way to nibble upward" claim holds
  literally — but a slot over its cap is never forced down and can
  churn gross indefinitely.

#### `0` means UNSET for all four now, and the boot says so

`core_config::exec` already refused a live slot that left
`max_order_usd_1e6` or `cap_instance_usd_1e6` at zero — "`0` means
UNSET here, never `unlimited`". `cap_day_usd_1e6` and
`max_open_orders` join that rule in this commit, because they stopped
being decoration. A live slot that omitted one would otherwise boot
and then refuse every order it placed: safe, but discovered at the
wrong end of the day. Refused at boot instead, where an operator is
present. No `exec.toml` exists on the engine host today (the standing
instance runs all-paper, no `--exec`), so nothing in service changes.

#### One copy of the two shared primitives

The risk review's E5 ruling was that E6 must **use**
`net_exposure_1e8` rather than restate it. `exec_router` cannot depend
on `exec_hyperliquid` — that would drag rustls, mio and secp256k1 into
the routing crate — so the honest reading of the ruling was to move
the function somewhere both can reach.

`core_types` has no dependencies and every arm already depends on it.
It now owns `net_exposure_1e8` and the `InstrumentRoll` `venue_seq`
codec; `exec_hyperliquid::recon`, `ingress_hyperliquid::family` and
`cli::backtest::binary` re-export theirs, so no caller changed and the
copies are gone. The roll codec had **four** writers, each carrying a
comment explaining why it could not import the others — every one of
those reasons is a reason to live in the zero-dependency crate.

**A divergence those four copies hid.** Three masked the kind byte to
its low bit; `strategy_bin15::on_roll` compares the WHOLE byte against
1. Over everything the packer can produce they agree, which is why
nothing ever caught fire. On a top byte no packer of ours writes they
part company: the masking reading calls `0x03` "settled", the
whole-byte reading calls it "created". Neither is right — the frame is
malformed. `core_types::roll_kind` returns the raw byte so a caller
can refuse it, and
`the_two_readings_of_the_kind_byte_agree_only_where_the_packer_writes`
states the divergence as a tested fact.

`exec_hyperliquid`'s reading is bit-identical to what it did before
the move. **Widening it to refuse a malformed kind byte is a live-arm
behaviour change and is deliberately not smuggled into a commit about
the exposure ledger**; the same goes for bin15's. Named here for the
risk review to rule on.

#### Two flaky gates, found and fixed

Neither is this commit's doing; both surfaced because a bigger test
suite changes scheduling, and both are the same shape — a gate that
teaches an operator to re-run until green.

**`exec_hyperliquid`'s loopback harness** bumped its request counter
AFTER flushing the response, so the client could read the body,
finish the test and assert on the counter before the server thread
incremented it. `the_whole_round_trip_passes_and_takes_exactly_four_requests`
read 3 while simultaneously asserting `verified_gone`, which can only
be true if the fourth response had already arrived. Counted before
the write now — the request is fully read by then, which is what
"served" means.

**`core_config`'s** `phase_8e_host_fields_use_defaults_when_unset` REMOVES
`OKX_WS_PUBLIC_HOST` while `..._honor_env_overrides` SETS it, on
process-global `std::env`, under `cargo test`'s thread pool. The pair
has always raced; adding two validation tests to a sibling module
changed the schedule and surfaced it. An `ENV_LOCK` now serialises every env-mutating test in that module, not
just the two that happened to collide.

#### Gates

`cargo test --workspace --exclude bench --no-fail-fast`: **2434
passed, 0 failed**. **80 tests added by this commit, none removed** —
47 in the new `exec_router::ledger`, 33 across the existing suites.
**61/61 alloc assertions**: gate 61 drives the ledger's fill,
lifecycle, roll and read paths against a FULL binding table and
asserts every outcome actually fired, so the zero is not agreement
over an empty set. Clippy clean under `-D warnings`. Licence OK (391
source files — `ledger.rs` is the one new file and carries its
two-line SPDX header).

**Seventeen break-and-watch runs**, each caught by the tests that name
the mechanism and no others. Five on the first cut:

| break | caught by |
|---|---|
| drop the "does not raise exposure" guard | `the_instance_cap_never_refuses_an_order_that_does_not_raise_exposure` |
| sum exposure as `\|Σ yes − Σ no\|` | `two_outcomes_do_not_net_against_each_other` |
| a roll stops dropping the instance's resting orders | `a_roll_drops_the_orders_resting_on_the_instance_that_ended`, `a_roll_leaves_another_outcomes_orders_alone` |
| the engine books the fill AFTER the member acts | `a_fill_is_booked_before_the_strategy_can_act_on_it` |
| the ledger stops filtering paper fills | `a_paper_fill_never_reaches_a_live_ledger`, `a_paper_fill_does_not_move_a_live_slots_ledger` |

And six more, on the behaviours the review repaired — those are the
ones that were wrong once already, so they get the tightest proof:

| break | caught by |
|---|---|
| rows keyed on the instance, not the family | `successive_instances_of_a_family_reuse_one_row`, `a_new_instance_of_a_family_does_not_inherit_the_old_position` |
| the modify exemption from the seeding interlock restored | `a_live_place_is_refused_until_the_ledger_has_been_reconciled` |
| the unseeded refusal counted as a `cap_instance` breach | `a_live_place_is_refused_until_the_ledger_has_been_reconciled` |
| `settle` zeroes the position again | `a_settle_drops_the_resting_orders_and_leaves_the_position_to_the_fill`, `a_settle_and_then_its_successor_clear_the_instance_through_the_same_path` |
| the day cap stops reading `settled` | `a_settlement_never_charges_the_day_cap` |
| the sell floor stops reporting on a settled row | `a_settlement_that_sells_more_than_we_booked_is_reported` |
| the venue byte dropped from the family key | `two_venues_do_not_share_a_family_row` |
| the unstamped-clock guard removed | `an_unstamped_order_clock_does_not_roll_the_day` |
| `settle`'s outcome-0 fallback to the family key restored | `a_settle_naming_no_outcome_is_refused_rather_than_clearing_a_successor` |
| the duplicate-settle gate removed | `a_duplicate_settle_is_a_no_op` |
| the mirrored resting-table size drifts | `cli::exec_boot`'s const assertion (compile error) |
| the order's monotonic stamp fed straight to the wall epoch | `the_two_clocks_do_not_thrash_the_day_epoch` |
| the unreconciled-ledger interlock removed | `a_live_place_is_refused_until_the_ledger_has_been_reconciled` |
| a full resting table stops counting | `a_full_resting_table_still_counts_the_order_that_did_not_fit`, `the_resting_table_refuses_rather_than_overwriting_when_full` |
| a tracked replacement recorded with no symbol | `a_tracked_replacement_is_still_droppable_by_a_roll` |
| the roll kind byte read with the permissive mask | `a_malformed_roll_kind_byte_is_refused_rather_than_read_as_settled` |

#### What the SECOND review round changed

The corrected diff went back to the same reviewer. It blocked again,
and again it was right. Three blocks and six smaller findings, and
the reviewer named the pattern better than I can:

> "of the three blocks here, two are prose asserting a guarantee the
> code does not implement … The code is careful; the claims about it
> are running ahead of it."

| finding | disposition |
|---|---|
| the seeding interlock exempted MODIFY on a premise (`cancel` would strand a quote) that is simply false | exemption dropped; both verbs refused, cancel is the escape hatch |
| `settle` zeroed the position, so every settlement tripped `sells_below_zero` ~768×/day | `settle` no longer zeroes; the settlement fill reduces naturally |
| `core_config`'s mirror constant claimed an agreement test that did not exist | `cli::exec_boot` const-asserts it; both comments corrected |
| the unseeded refusal was counted as `refused_cap_instance` | its own `RiskRefusal::Unseeded` / `refused_unseeded` |
| two venues collide on one family row, and the doc claimed they could not | the key is `(venue, family)` |
| `observe_mono_clock` lost `roll_day`'s zero guard — `wall_of(0)` is a plausible wall time in another epoch | guarded on the input |
| the `resting_full` unreachability claim was false | corrected; modify is exempt from the clamp, so the bound does not hold |
| `settle`'s `outcome == 0` fallback re-opened the reordering hazard | refused into `settles_unmatched` |
| a duplicate settle double-counted `instances_cleared` | gated on `settled == 0` |
| `settled` was write-only | read on both the fill path and the settle path |

One more came from break-and-watch rather than from either review.
The reviewer asked for the settlement floor to stop counting
`sells_below_zero`, on the premise that `settle` zeroed the position.
Not zeroing was the better of the two fixes and **both were
applied** — so the suppression had nothing left to suppress.
Break-and-watch removed it and **not one test failed**, which is how
an unreachable guard announces itself. It was also actively harmful:
a settlement selling MORE than the ledger booked is precisely the
router disagreeing with the venue, and the guard would have swallowed
exactly that. Removed, with a test
(`a_settlement_that_sells_more_than_we_booked_is_reported`).

#### What the FIRST review round changed

Both project review agents ran. `alloc-auditor` returned **PASS**.
`risk-reviewer` returned **BLOCK**, and it was right to: four of its
findings were real fail-open defects, and one of its *questions* —
whether outcome ids are stable across instances — turned out to be
the most serious defect in the commit. Everything it verified
mechanically (the netting, the per-outcome sum, the exact projection,
the `i128` notional, the paper-fill filter, the fill-before-strategy
ordering, the acceptance-only counting, the bit-identity of the
`core_types` move) it confirmed correct; the objection was entirely
to **the set of states the ledger could not represent**, which is the
harder thing to see from inside the change.

| finding | disposition |
|---|---|
| outcome ids are not stable across instances (raised as a question) | rows re-keyed on family |
| two clocks in one day epoch (raised as non-blocking) | `WallAnchor`; `Default` removed |
| a restart fails all three clamps open | seeding interlock; live PLACE refused until reconciled |
| the shared table switches `max_open_orders` off | fail-closed count **and** a boot bound |
| `on_modify` creates undroppable phantom rows, and fires on the ambiguous case | carries `sym`; `find` is three-valued |
| `cap_instance` bounds filled-only exposure | stated, with the multiplying bound |
| the new roll reading inherited the permissive kind-byte mask | uses `roll_kind`, refuses a malformed byte |
| `refused_risk`'s meaning changed under its name | documented, with the alert to re-point |
| the sell-to-zero floor absorbs errors silently | `sells_below_zero` counter |
| a settle for an unheld family is silent | `settles_unmatched` counter |

Two of those — the family keying and the two clocks — were defects
**no test in this commit could have caught**, because the tests built
the world the code assumed. That is the same shape this lane keeps
finding, one level up: not a check whose condition is weaker than its
name, but a test whose fixture is narrower than its claim.

#### Still ahead in E6

Commit 3 — the sticky halt state machine (N consecutive venue
rejects, budget floor breached, reconciliation drift, WS user-stream
gap, asset-id refusal streak, day cap reached; halt ⇒ cancel-all ⇒
refuse every submit ⇒ operator restart, never self-clearing,
per-slot refusal with venue-wide cancel). Commit 4 — the `exec.HALT`
file switch polled in `HlExchange::on_idle`, the
`engine_exec_halted{slot}` gauge, the `/state` field and the loud boot
tell.

### E6 commit 3a — the live arm finally has a thread (2026-09-19)

Commit 2 built a risk gate fed by venue fills. Commit 3 is meant to
build a halt machine fed by the venue relationship. Both assume
something is driving the live arm. **Nothing was.**

`OrderDispatch::on_idle` is what a live dispatcher uses to pump the
venue's user-event socket, run the reconciliation timer and persist
the budget's state file. `DispatcherWorker::run` calls it — and
`DispatcherWorker` is constructed in exactly one production place, the
legacy Polymarket `--live` path. The `--exec` path hands its
`RoutedDispatcher` straight to the engine loop with no worker behind
it, so on **the one path that can arm Hyperliquid** the hook was never
called. `exec-hyperliquid`'s own module docs say so in as many words,
and the trait's do too; the plan filed the wiring under E7 as "an
arming-path change".

Three consequences, all of which commit 3 would have inherited:

* the user-event socket never pumps, so **no venue fill ever reaches
  fill lane 3** — and the exposure ledger commit 2 built is fed by
  exactly that;
* the reconciler never runs, so nothing can call `mark_ledger_seeded`
  and a live slot refuses every order for ever;
* three of E6's six halt triggers — budget floor, reconciliation
  drift, WS user-stream gap — have no source of truth at all.

A halt machine wired to sensors nothing reads is the
declared-not-enforced shape this phase exists to remove, so the
wiring comes first, as its own commit, reviewable on its own.

#### Which loop, and how many drivers

`run_engine_loop` is the one loop every `engine_loop_*` entry point
shares, so the driver lands there and reaches the `--exec` path
(`engine_loop_set_full` → `run_engine_loop`). `RoutedDispatcher`
already forwards `on_idle` to BOTH arms, so the live arm is reached.

The legacy Polymarket `--live` path goes through the same loop, and it
is worth being explicit that this does not double-drive anything: it
holds a `QueuedDispatcher`, which does **not** override `on_idle` — it
takes the trait's no-op default. The real `LiveDispatcher` sits behind
`DispatcherWorker` on its own thread, and that worker is still its
only driver. One driver per boot mode, verified rather than assumed.

#### It runs on the engine thread, and that is a real cost

`Engine::drive_dispatcher_idle` forwards to the dispatcher; the cli
loop calls it. **It blocks.** The socket work does not — mio,
edge-triggered, drained to `WouldBlock` — but the reconciler is one
HTTPS round trip a minute and a pending LAW E-8 sweep is another, on
the thread that also runs every member's callbacks, stalling the
paper slots along with the live one.

The alternative was moving the live arm behind `QueuedDispatcher` and
a worker thread, which is the design the codebase already has for
this. It was rejected because it takes the HTTP response off the
calling thread, and **the HTTP response being the ACK is LAW E-5** —
along with E5's whole synchronous cancel/modify contract.

**"A bounded stall" needs a number, and the first draft of this
section did not have one.** The bounds, measured from the code rather
than asserted:

| step | bound | why |
|---|---|---|
| `pump_user_events` | none needed | mio, edge-triggered, drained to `WouldBlock`; `PUMP_BUDGET` 20 ms |
| a WS reconnect | **blocking DNS — `std::net::ToSocketAddrs`, no timeout parameter; ~40 s on glibc defaults (5 s × 2 attempts × 3 nameservers) and potentially worse on macOS** — then a non-blocking connect | `WS_BACKOFF` bounds the FREQUENCY, not the duration |
| `persist_budget` | a file write + rename | guarded by `PERSIST_EVERY` (5 s) |
| one HTTPS request | **5 s** — `http::REQ_DEADLINE`, enforced by a mio poll loop with a 50 ms poll timeout over a NON-BLOCKING `mio::net::TcpStream::connect` | a blackholed host cannot cost the ~75 s a blocking connect would |
| `reconcile` | 1–2 requests, guarded by `RECON_EVERY` (60 s) | ≤ 10 s an hour |
| `sweep_one_pending` | **this was the unbounded one** | see below |

`ours_on_leg` can select up to `MAX_OPEN_ORDERS` (256) oids and the
sweep cancelled every one of them in a single call — up to ~21
minutes of deadline in the pathological case and ~13 s at a realistic
50 ms a round trip. On the engine thread that means no ring drains,
`shutdown_requested()` is never reached, and **E6's halt machine
cannot run on the very thread a dead venue is blocking** — a dead
venue disabling the machine built for a dead venue.

`SWEEP_CANCELS_PER_IDLE` caps it at 8 a call: ~40 s of deadline and
~0.4 s in practice. The remainder is not dropped — the entry stays
pending and the next idle moment continues it, and idle moments come
round every 2 ms, so a 256-order sweep finishes in 32 of them rather
than one.

**The rows bound each step; the engine thread pays whatever
co-occurs in one call.** Worst case for a single `on_idle`: a DNS
stall (~40 s) plus 8 sweep cancels at their deadline (40 s) plus a
reconcile (10 s) ≈ **90 seconds**, and that is the number that
belongs beside "`shutdown_requested()` is never reached". The DNS row
is the one term still bounded by the platform rather than by this
code; resolving at boot and caching would remove it, and is not in
this commit.

Budget exhaustion is counted as `sweep_deferred` and does **not**
spend a retry. Folding it into the failure path would have burned one
of `SWEEP_TRIES` (8) per idle moment, so a 256-order sweep at 8 a call
would be abandoned after 64 with the rest reported as `sweep_left`:
quotes left resting on a retired instance, which is the one thing LAW
E-8 exists to prevent.

**And `truncated` had to stop meaning failure for the same reason** —
a regression the budget itself created, caught only in review.
`truncated` is `k == MAX_OPEN_ORDERS`, the selection filling the
buffer. Before the budget one call cancelled all 256, so a leg of N
orders was truncated for about `N / 256` calls. After it, progress is
8 a call, so the selection sits AT the ceiling for roughly
`(N − 256) / 8` calls — and classing each as `Retry` spends one of
eight tries every time. **Past N > 312 the entry is abandoned with the
remainder reported as `sweep_left`**, and commit 2's boot rule allows
64 open orders on each of 8 slots, so a 512-order leg is a valid
configuration. Truncation now defers: still never `Done`, because we
could not see the end of the selection, but continued rather than
counted against.

**The invariant deferral rests on, named because nothing here
enforces it.** `sweep_one_pending` re-asks the VENUE every call — it
POSTs `frontendOpenOrders` and re-runs `ours_on_leg` over the fresh
answer — so an order cancelled last call is no longer listed and the
selection shrinks. That is a property of re-fetching, not of this
file. If it ever failed (a venue that keeps listing a cancelled order
AND accepts the re-cancel, so nothing fails and nothing completes),
deferral would never spend a retry and the entry would issue 8 HTTPS
round trips every 2 ms for the life of the boot. `SWEEP_MAX_DEFERS`
is the guard, and its SIZE is the argument: counting only consecutive
non-shrinking calls does not work, because the selection cannot
reveal progress while the leg exceeds the buffer — a 312-order leg
reports 256 selected on each of its first seven calls while eight
orders really are being cancelled each time. So it is a total count,
sized at 256: four times the 64 deferrals the largest valid
configuration can need, and still terminating a genuinely stuck entry
inside about half a second of idle moments. `sweep_stalled` counts it.

A PAPER boot reaches the same call and gets the trait's default: a
`false` return and nothing else. No syscall, nothing measurable.

**What it does to the paper slots is more than a delay.** VT3/VT4's
stale law judges a tick against `core_time::FeedClock` per connection;
a multi-second engine-thread stall makes every tick drained afterwards
stale by venue time, so paper members stop marking and quoting across
the backlog, `engine_ingress_<venue>_stale_ticks_total` spikes, and
the effect outlasts the stall. That is a live/backtest divergence the
harness cannot reproduce — which is the strongest reason the sweep
budget above is not optional.

#### "When the rings are empty" would have starved it

The natural gate is a tick that drained nothing, and that is the
right default — the venue work blocks and market data must not queue
behind it. But `tick` drains up to `DRAIN_BATCH` (256) per ring per
lane, and a busy market across six venues can keep every tick
non-empty indefinitely. Gated on emptiness ALONE, the only path that
pumps the socket, reconciles and persists the budget would stop
running **exactly when there is most trading to reconcile**.

So the condition is "drained nothing, OR the gap has reached 2 ms".

**And the gap must be stamped from a clock read taken AFTER the
call.** The first cut stamped the pre-call reading, which collapses
the ceiling in exactly the case it exists for: one HTTPS round trip
always exceeds 2 ms, so the next iteration's clock is already past the
gap, the gate fires again immediately, and the engine thread sits
inside `on_idle` continuously. That is not a 2 ms cadence; it is a
synchronous HTTP loop with the engine attached.
`a_blocking_call_does_not_turn_the_ceiling_into_every_iteration`
simulates a second of 50 ms calls and asserts about twenty of them,
not thousands.

`now` is re-read at the same point, because everything downstream —
the 1 s `/state` publish, the 5 s report, the per-venue tick ages —
would otherwise compute staleness against a clock read before the
stall and **under-report it by exactly the stall's duration**, hiding
the pause that caused it.

The decision, the call and the stamping all live in `IdlePacer`,
because the loop is a two-thousand-line function no test constructs
and the stamping is where the first cut went wrong. Extracting only
the predicate was not enough: the test then re-implemented the
stamping itself, so it asserted its own simulation, and a
break-and-watch run that reinstated the pre-call stamp **passed**.

Measuring it took a third attempt too. Counting DRIVES PER SECOND
cannot see the defect — when a 50 ms call dominates the clock,
"fires every iteration" and "fires every 2 ms" both give about twenty
a second. What differs is how much TICK WORK the engine gets between
venue calls: ~2000 loop iterations when the ceiling holds, one when
it does not. `a_blocking_call_leaves_the_engine_time_to_work_between_calls`
asserts that ratio, and it is the only assertion in the set that
fails when the stamp goes back.

`Engine::tick` now returns how many items it CONSUMED. It returned
`()`, so the loop had no way to tell a quiet iteration from a busy
one. Every existing call site ignores the value and is unaffected.

Consumed, not dispatched, and the difference is the whole point. The
first cut differenced the `*_dispatched` counters — what reached a
MEMBER. That summed five of the eight lanes, missed depth, opt and
the ruleset table entirely, and could not see the two AI outcomes
(expired, malformed) that have no counter by construction. Any of
those reading zero on a busy iteration drives the blocking venue work
every time round the loop, which is the same degeneration the 2 ms
ceiling exists to prevent, reached from the other side. One `usize`
on the stack, incremented at each ring pop, answers the question that
is actually being asked; `tick_reports_every_lane_it_drains` pushes
one item into every lane and holds it complete.

#### What this commit newly puts on the wire

It cannot cause a live submit or modify that would not have been
sent: `risk_check` is untouched and the seeding interlock still
refuses everything, because nothing calls `mark_ledger_seeded` yet.
But it is not read-only. An `--exec` boot now emits **LAW E-8 cancel
traffic** from `sweep_one_pending` and the reconciler's `info`
requests, neither of which it has ever sent before. The direction is
safe — cancels reduce risk and are not risk-checked — but "what this
commit changes about arming" is the question the E6-versus-E7
decision turned on, and the answer is not "nothing".

#### A third flaky gate, and this one was a real test defect

`core_metrics`'s `scrape_hammer_all_succeed_without_conn_errors`
asserts the metrics server's error sink stays silent under 50 rapid
scrapes. It failed once in a full-workspace run and passed four times
alone — the signature of load sensitivity, which is what I first took
it for.

It is not. The test's own **readiness probe** connected and dropped
the socket without sending a request:

```rust
if TcpStream::connect_timeout(&addr, …).is_ok() { break; }
```

The server accepts that connection, reads EOF, and reports a
connection event — which the sink counts. Whether it lands before or
after the final assertion is a pure race, and under load it lands.
The probe poisons the counter it is about to assert is zero.

**The first fix was wrong and the break-and-watch run said so.**
Zeroing the counter after the probe compensates for the effect rather
than removing the cause, and removing it only brings a *race* back —
so nothing failed when it was deliberately broken. Synchronising
instead (wait for the probe's event, then zero) was worse: the event
is not guaranteed to arrive at all, and two of three runs then hung
to the timeout.

The cause is removed instead. The probe now sends a complete request
and reads the reply, so it is an ordinary served scrape that produces
no event to race with, and the test asserts the count is zero
**immediately after the probe** rather than only at the end — if the
probe ever goes back to aborting, that fires where the defect is
rather than intermittently three seconds later. 5/5 clean since.

#### Gates (3a), and what the `cli` number is worth

`cargo test --workspace --exclude bench --no-fail-fast`: 2452 passed,
0 failed. 61/61 alloc assertions. Clippy clean under `-D warnings`.
Licence OK (391 files).

**That number is not evidence for the `cli` crate.** See below: its
test binary aborts about one run in three without naming a test, so a
green run cannot be distinguished from a run that died before
reaching what would have failed. Every assertion this commit adds to
`cli` — `should_drive_idle`, `IdlePacer`, the whole
`dispatcher_idle_tests` module — sits in that crate, and those were
also exercised individually and through break-and-watch, which is the
evidence that does hold.

#### And a FOURTH, which is a crash and is NOT fixed here

`cargo test -p cli --lib` aborts mid-run with **exit status 75** and
no failing test named, roughly one run in three. All 268 tests report
`ok` and the process then dies partway through the listing.

Measured, not guessed:

* **It predates this lane.** With every one of this commit's files
  stashed, HEAD fails **2 runs out of 6**.
* **It does not reproduce running the test binary directly** — 8
  consecutive clean runs of `target/debug/deps/cli-*`. It happens only
  under `cargo test`.

So it is not an assertion that races; it is the harness process
terminating abnormally, which makes the whole `cli` suite an
unreliable gate. Every full-workspace run reported in this document
is a run where it did not fire, and that is luck rather than
evidence.

**A concrete lead for whoever picks it up.** Exit 75 is
`EX_TEMPFAIL` in `sysexits.h`. This host already has an fd-exhaustion
incident on record — a launchd agent born with a 256-descriptor soft
limit, fixed by adding `ulimit -S -n 8192` to `engine-wrapper.sh`.
`cargo test` holds extra pipes per test binary that a direct run does
not, and `cli` is the largest suite, which fits both "only under
`cargo test`" and "only sometimes". Comparing `ulimit -n` between the
two environments is a cheap next step and turns this from "flaky"
into something falsifiable.

**Left open deliberately.** Diagnosing a mid-run abort in a 268-test
binary is its own investigation, and three flaky gates have already
been absorbed into this lane. Recorded here so the next person does
not spend an hour discovering it from scratch, and so that a green
`cli` run is not read as stronger evidence than it is.

**Three flaky gates in two commits**, none of them this lane's doing,
all surfaced because a growing suite keeps changing the schedule.
That is worth naming as a pattern rather than fixing three times in
silence: this repository has tests that assert properties which only
hold on a quiet machine, and each one that survives teaches an
operator to re-run until green.

### E6 commit 3 — the sticky halt state machine (2026-09-19)

E6 commit 1 gave the router a clamp that refuses **one order**. This
commit gives it the thing that refuses **every future order**: five
triggers, one latch per slot, a venue-wide cancel on the edge, and a
file on disk so a halt is not forgotten by the next restart.

The distinction matters more than it sounds. A clamp answers "is this
order too big?" A halt answers "is this arm still trustworthy?" — and
once the answer is no, the size of the next order is beside the point.

#### The five triggers, and what each one actually observes

| trigger | fires when | threshold key |
|---|---|---|
| `RejectStreak` | consecutive venue rejects ≥ N | `halt_on_reject_streak` |
| `AssetRefusals` | consecutive asset-id refusals ≥ N | `halt_on_asset_refusal_streak` |
| `BudgetFloor` | the arm reports its allowance spent | *(none — see below)* |
| `ReconDrift` | reconciler disagreement ≥ $X | `halt_on_recon_drift_usd_1e6` |
| `WsGap` | user stream silent ≥ N ms | `halt_on_ws_gap_ms` |

`trigger_for` evaluates them in that order and returns the FIRST one
that fires.

**The order only breaks ties inside one poll.** `on_idle` runs every
2 ms and the latch is sticky, so whichever condition crosses its
threshold first *in time* is the one recorded, whatever the order
says. Within a single poll the order puts symptoms
(`RejectStreak`, `AssetRefusals`) ahead of causes (`BudgetFloor`,
`WsGap`) — a dead socket that has also produced a reject streak
reports as `reject-streak`. That is a real wart and it is recorded
here rather than dressed up: an earlier draft of this section claimed
the order reported "the one an operator most needs to see", which the
code does not do and was never written to do.

**The arm reports observations; the router owns the thresholds.**
`HaltSignal` carries raw numbers — a gap in nanoseconds, a streak
count, a drift in dollars — and no opinion about whether any of them
is too much. That split is deliberate: the thresholds are per SLOT and
live in the operator's artifact, and an arm that compared against them
itself could not serve two slots with different appetites. The test
`two_slots_with_different_thresholds_reach_different_conclusions` pins
exactly that.

**`0` means UNSET, never "unlimited".** A threshold left at zero
disables its trigger, which is why every one of the four is now
*required non-zero* on a live slot — see below. The alternative
reading, where `0` means "halt on the first reject", would make a
missing key the most aggressive possible setting; the reading where it
means "never halt" makes a missing key the most dangerous one. Neither
is acceptable as a silent default, so the boot refuses instead.

**`ws_gap_ns == 0` is "no observation", not "no gap".** An arm that
has never connected has no last-good timestamp to subtract from. If
zero were read as a gap of zero the trigger would never fire before
the first connect (harmless); if it were read as an infinite gap the
engine would be unstartable (not harmless). The check requires
`sig.ws_gap_ns > 0` before comparing, and says so in a comment,
because this is the third time in this lane a sentinel has been the
whole bug.

#### The budget floor has no per-slot threshold, on purpose

It is the one trigger a slot cannot tune, because it is not about the
slot. `request_budget_floor` is a property of the ADDRESS: an account
with no allowance left cannot place for anybody, and a slot permitted
to set its own floor could keep submitting into an arm that has
already stopped being able to sign. The arm compares and reports a
flag; the router latches it unconditionally. Pinned by
`the_budget_floor_fires_even_with_every_threshold_unset`.

#### It runs on the IDLE path, and that is the entire point

A halt evaluated on the dispatch path would fire last or never. The
condition that trips these triggers — a dead venue, a silent stream,
an address out of allowance — is *exactly* the condition under which
the member stops submitting, so waiting for the next order to
re-evaluate means waiting for an order that is not coming.

Commit 3a gave this hook a driver: `on_idle` runs on the engine thread
every 2 ms whether or not anything is trading.
`a_dead_venue_halts_with_no_order_flow_at_all` asserts the whole
sequence with `live().seen` empty — nothing was ever submitted, and
the slot halted anyway.

#### Cancel-only: the halt refuses submit and modify, and allows cancel

A halted slot must be able to GET OUT. Refusing its cancels would
leave the member holding orders it has decided it does not want, with
no path to give them back, which is a worse state than the one that
tripped the halt.

So `submit` and `modify` are refused with `RiskRefusal::Halted`, and
`cancel` is not refused because **it never reaches the risk gate at
all** — `RoutedDispatcher::cancel` does not call `risk_check`. There
is no "cancel" verb to exempt: `RiskVerb` is `{Place, Replace}`. The
escape hatch is structural rather than a permission, which is the
stronger form.

A modify is refused rather than allowed-if-smaller because LAW E-7
makes a live requote a MODIFY: permitting it would let a halted slot
keep quoting indefinitely at ever-smaller sizes, which is trading.

#### The venue-wide cancel: a REQUEST and a CONFIRMATION, not one call

On the halt edge every working order for the arm is taken back —
venue-wide, not per instance, because a halt is not a statement about
one market.

**This is where the commit's worst defect lived, and it is worth
setting out in full**, because it is the fourth appearance of this
lane's recurring shape: *a name that describes a stronger property
than the thing it is attached to tests.*

`OrderDispatch::cancel_all` looks like it cancels. On Hyperliquid it
does not. It walks the live legs and calls `queue_sweep` for each,
and **returns `Ok(())` as soon as every leg is queued.** Nothing has
been sent to the venue at that point. The sweep drains later, one
entry per idle moment, asking the venue what is resting and cancelling
by oid, eight at a time.

The first version of this commit read that `Ok(())` as confirmation:

* `cancel_cleared()` zeroed the pending flags, so **the retry the
  section promised never ran again.** If the sweep then burned its
  eight tries and was dropped, the orders stayed working at the venue
  and nothing re-queued them.
* `ledger.clear_resting()` zeroed the resting count for **every**
  slot while none of those orders had been touched — handing the
  HEALTHY slots a permissive `max_open_orders` at the exact moment a
  sibling slot had halted, and letting them stack a second full book
  on top of the one still resting. A clamp that gets weaker when the
  venue is misbehaving is precisely backwards, and this is the second
  time in this commit that exact hole was opened.

The fix is to stop pretending one call answers two questions.

| method | asks | answers |
|---|---|---|
| `cancel_all` | *take everything back* | the request was accepted |
| `cancel_all_state` | *is the venue clear?* | `Clear` / `Working` / `Stranded` |

`CancelAllState` is three-valued rather than a `bool` for the same
reason the ledger's lookup is `Found::{One,None,Many}`: the caller's
response to "still working" and to "gave up" are opposite — wait,
versus ask again — and a `bool` would force one of them to be
guessed.

The arm can answer honestly because it already knows the difference. A
sweep entry is dropped two ways: `sweep_one_pending` drops it when the
venue reports **nothing of ours resting on that leg**, and `spend_try`
drops it — bumping `sweep_left` — when the retries run out. So an
empty sweep table means "clear" only when `sweep_left` has not moved
since the request. That is the whole of `cancel_all_state`.

**One request and one confirmation per poll, and confirming can never
loop back into asking** — the re-request lives in `try_cancel_all`
rather than inside the `Stranded` arm, so the shape is flat by
construction rather than by a comment saying it does not recurse.

`latch` marks the slot `WANTED`; the first poll asks and moves it to
`ASKED`; later polls confirm. `WANTED` beats `ASKED` across slots, so
a slot that halts while another slot's sweep is draining gets its own
request — its legs were not in that sweep. And two slots halting on
the *same* poll share one request, because cancel-all is venue-wide.

#### A failed cancel halts anyway, and nothing is cleared until the venue says so

Waiting for a successful cancel before refusing would keep submitting
into the condition that tripped the halt. So the latch is immediate
and unconditional; only the *clearing* waits.

Two counters, because they are two different facts and an earlier
draft had them as one:

* **`cancel_all_failures`** — requests the arm would not accept.
* **`cancel_all_stranded`** — polls on which the arm reported it had
  given up with the venue unconfirmed, and the router asked again.
  This is the number an operator wants after an incident: requests
  that *landed* and still left orders working.

#### How long "clear" actually takes

The refusal latches in microseconds. The venue being clear is minutes
away, and the section should say so rather than leave "retries until
it lands" to imply otherwise.

From `SWEEP_CANCELS_PER_IDLE = 8`, one entry drained per `on_idle`,
`REQ_DEADLINE` 5 s per round trip, `SWEEP_TRIES = 8`:

* one leg of 256 resting orders — 32 idle moments, so **≈14 s** at a
  50 ms round trip, and **up to ≈24 min** if every request runs to
  the deadline;
* 16 queued legs drained serially — up to 512 calls, so **≈4 min**
  realistically and **hours** at the deadline.

That gap is exactly why the request and the confirmation had to be
separated: for minutes at a time the honest answer to "are we clear?"
is *no, still working*, and the old contract had no way to say it.

#### Sticky, and nothing in the process clears it

`latch` keeps the FIRST reason a slot halted for. Later triggers
describe the same incident, and the first one is what an operator
needs. Nothing — not a recovered stream, not a successful
reconciliation, not a hundred idle polls — un-halts a slot;
`nothing_clears_a_halt` asserts it directly. Clearing a halt is an
operator action, by restart, and commit 4 makes even that deliberate.

#### `exec.HALT`, so a halt survives the daily restart

Without a file, a halt lives only in this process — and the scheduled
restart at 00:10Z would clear it and resume trading into whatever
tripped it, unattended, at an hour when nobody is watching. So the
edge writes `exec.HALT` with the slot and the reason.

**Wired at the `--exec` boot**, beside the artifact that armed the
slots: `halt_file_path` puts it in the artifact's own directory, so an
operator running two engines from two artifacts gets two halt files
rather than one they share. A first draft of this section described
the writer as delivered while `set_halt_path` had no caller outside
its own tests — the file was never written on any real boot, and the
restart hole the section exists to close was still wide open.

Best effort, deliberately: the write happens AFTER the latch, and a
failed write does not un-halt the slot. Refusing to halt because a
file could not be written would be the wrong direction, and
`no_halt_path_means_no_file_and_no_refusal_to_halt` pins that a router
with no configured path still halts.

The writer uses a fixed 64-byte buffer and no `format!` — it runs on
the engine thread — and renders two digits rather than one. With
`EXEC_SLOTS == 8` one digit is correct today and would silently
mod-10 wrap the moment the table grew, in the one file an operator
reads after an incident.

#### `--halt-slot`, the operator's switch

`halt_slot` needed a caller that was not a test, and this is it:
`--halt-slot 3` (or `3,5`) boots with those slots already halted.

Deliberately NOT half of a two-switch interlock, unlike `--arm-live`.
Arming needs two agreeing edits because it reaches real money;
halting needs one, because it is the direction that cannot. For the
same reason it does not require `--exec` — but naming slots without an
artifact logs a warning saying plainly that the flag halted nothing,
rather than refusing a boot because someone asked for more safety, or
staying silent and letting them believe a switch fired.

#### The day cap refuses; it does not halt

`cap_day` reaching its limit is the system working as designed, not a
fault: the slot has spent what it was given. It refuses further buys
and keeps running — sells still pass, so the member can still reduce.
`spending_the_day_cap_refuses_but_does_not_halt` asserts the
distinction, which is worth pinning precisely because the other four
money-adjacent conditions all do halt.

#### Two new required keys, and a boot that refuses without them

`halt_on_ws_gap_ms` and `halt_on_asset_refusal_streak` join
`halt_on_reject_streak` and `halt_on_recon_drift_usd_1e6` as keys a
live slot MUST set non-zero. All four are now enforced at parse time
with an error that says what `0` means, so an operator who omits one
gets a refused boot naming the key rather than a trigger that silently
never fires.

`request_budget_floor` joins them. It is not a threshold the router
compares against — the arm owns it and reports a flag — but at `0` the
budget trigger fires only at TOTAL exhaustion, which is a kill switch
that waits until the address is already bricked.

This broke ten existing fixture tests, which is the correct outcome: a
fixture that armed a live slot without a kill switch was describing a
configuration the engine should never have accepted.

**And it broke the shipped template, which nothing caught.**
`exec.toml.example` tells the operator to change `mode` to `"live"`,
and after this change that edit produced a refused boot naming a key
the operator had never heard of. The fixtures were updated; the file
the operator actually starts from was missed, and the only sign would
have been the refusal itself. There is now a test that parses
`exec.toml.example` with that one edit applied and asserts every
required key is present and non-zero — the thing that would have said
so.

#### The restart interlock is now switched off by a real reconciliation

Commit 2 left the ledger UNSEEDED at boot, so every live PLACE was
refused until something proved the ledger's numbers described the
venue rather than an empty world. Commit 3 supplies that something:
`on_idle` reads `halt_signal().reconciled` and calls `mark_seeded`
the first time the arm reports a completed reconciliation.

This is the most safety-relevant behaviour change in the commit — it
is the line that lets a live slot trade at all — and the first draft
of this section did not mention it anywhere. The gate itself is
sound: `reconciled` is set only after the arm's `scan_spot_state`
succeeded and `compare` ran, so it cannot be reported by an arm that
merely connected.

#### A FIFTH flaky gate, and this one was ours

`exec_boot::tests::an_all_paper_artifact_resolves_to_an_all_paper_table`
failed roughly one run in six with `resolve()` reporting "no such
file".

The cause: `tmp()` built its directory name from the pid and the WALL
CLOCK in nanoseconds. `cargo test` runs these in parallel, two threads
landing in the same clock tick got the same directory, and whichever
finished first removed the other's artifact out from under it. Every
other temp-dir helper in the repository disambiguates with a per-test
tag; this one did not.

Fixed by counting instead of clocking — a process-wide
`AtomicU64` cannot tie. Eight consecutive clean runs of `cli --lib`
after the change, where the flake reproduced within three before it.

That is **four flaky gates fixed** across this lane and one
(the exit-75 abort, §E6 3a) still open. The pattern named there holds:
this repository has tests that assert properties which only hold on a
quiet machine.

#### Break-and-watch

Ten deliberate breaks, each restored by file copy, each caught by a
test whose name describes what was broken:

| broken | caught by |
|---|---|
| each of the five `trigger_for` conditions | its own `*_halts_*` test, 5/5 |
| `latch` stickiness | `a_halt_keeps_its_first_reason`, `nothing_clears_a_halt` |
| `latch` marking the cancel pending | 10 tests |
| `try_cancel_all`'s outstanding guard | `a_halt_edge_fires_cancel_all_once_not_once_per_poll` |
| clearing resting on a FAILED cancel | `a_failed_cancel_all_leaves_the_resting_count_alone` |
| the operator halt's immediate cancel | `an_operator_halt_is_exactly_as_sticky_as_a_triggered_one` |
| a required key missing from the template | `the_shipped_template_arms_a_live_slot_without_further_edits` |

One honourable mention: the first attempt at the tenth break inserted
a dead `if false { … }` and left the live call below it, so the suite
passed and it looked for a moment like a test gap. The break script
was the bug. Worth recording because a break-and-watch that does not
actually break anything reports the same "ok" as a missing test.

**And the thing break-and-watch did NOT find.** Every break above was
caught, and the suite was green, while `cancel_all` still reported a
queued sweep as a cleared venue. Break-and-watch tests that the
guards fire; it cannot tell you that a guard is reading a value which
does not mean what its name says. That took a reviewer reading
`HlExchange::cancel_all` against the prose — which is the argument for
running the review agents on the DOC and the code together, not the
code alone.

#### What the review round changed

Both agents ran against the working tree with the risk-policy section
already written, which is what made the prose auditable.

The alloc audit passed: no forbidden pattern, no `dyn`, every size
assertion recomputed and correct (`HaltLimits` 24 B, `HaltSignal`
32 B, `ExecRoute` 448 B), 61/61 at 0 B/op, clippy clean. It flagged
four borderline items, of which two were taken here — `HaltState` had
no size assertion (it is one cache line; now asserted) and the halt
file rendered one digit for the slot — and two are recorded for
commit 4: there is no alloc gate driving `on_idle` itself, and
`write_halt_file`'s `core_io::write_atomic` allocates on the engine
thread on the halt edge.

The risk review returned `BLOCK`, correctly, and found:

1. **The cancel contract** (above) — the real one.
2. **`set_halt_path` had no production caller**, so `exec.HALT` was
   never written on any real boot while this section said it was.
3. **`halt_slot` had no production caller**, so the "operator halt"
   the section described did not exist.
4. **`exec.toml.example` was left unbootable** by the new required
   keys.
5. Six further prose-vs-code mismatches, all corrected above: the
   claim that the trigger order ranks by operator value, a
   `RiskVerb::Cancel` that does not exist, "taken back" for a queued
   sweep, and the entirely unmentioned auto-seed.

Also taken from it: `HaltState::refused_halted` was a second field of
the same name and the same value as `RouteCounters::refused_halted`
that nothing ever read — deleted, because two counters for one fact
is how they drift — and `request_budget_floor` joined the required
keys.

Left for commit 4, deliberately: `refused_halted` and the halt state
are still not exported through `ExecCounters`, so `/metrics` sees a
halt only folded into `refused_risk`. That is commit 4's subject.
`exec.HALT` also records only the LAST slot to halt, which matters
once the read-back lands and not before.

#### Gates

* `cargo test --workspace --exclude bench --no-fail-fast` — **2493
  passed, 0 failed**
* `cargo test --release --test alloc_assertions -- --test-threads=1` —
  **61/61**
* `cargo clippy --workspace --all-targets -- -D warnings` — clean
* `make license-check` — OK, 391 source files

#### Still ahead in E6

Commit 4: the operator-written `exec.HALT` read back at boot and
polled at runtime, an `engine_exec_halted{slot}` gauge, `refused_halted`
and the halt state exported through `ExecCounters`, the halt state and
`is_seeded` in `/state`, and a loud boot tell when the engine starts
with a halt file already present. With the read-back comes the fix for
`exec.HALT` recording only the last slot to halt, and an alloc gate
that drives `on_idle` under `AllocGuard`.

### E6 commit 4 — the halt file, read as well as written (2026-09-19)

Commit 3 built the kill switches and wrote `exec.HALT`. Nothing read
it. This commit closes that loop and puts the halt state where an
operator can see it.

#### The file names EVERY halted slot, not the last one

The writer took a slot and a reason and rendered one line. With two
slots halted the file named one — so the read-back this commit adds
would have resumed the other, silently, which is the precise failure
the file exists to prevent.

`render_halt_file` now takes the whole `[HaltReason; EXEC_SLOTS]` and
writes a line per halted slot, and `on_halt_edge` takes **no slot
argument at all**. That is deliberate: everything the edge does is a
function of the whole halt state — the file names every halted slot,
the cancel is venue-wide — so a per-slot argument would be a standing
invitation to write per-slot behaviour that is wrong by construction.

The format is engine-written and hand-writable:

```
# exec.HALT - engine-written. Delete it and restart to clear.
slot=3 reason=ws-gap
slot=5 reason=operator
```

`parse_halt_file` also accepts a bare number, because an operator
reaching for a kill switch in a hurry types `echo 3 > exec.HALT` and
that has to mean something. Three readings are deliberate:

* **An unknown reason still halts.** A word this binary does not
  recognise parses as `Operator`, never `None` — "I cannot tell why"
  reads as *stay stopped*.
* **A bad line does not discard the good ones.** One unparseable line
  must not throw away the halts that parsed.
* **A corrupt file is not a boot refusal.** Refusing to start would
  make a mangled file a denial of service on an engine that might be
  needed to flatten a position.

And the fourth reading, which the first draft of this commit got
badly wrong — see *The file is an INPUT* below.

#### The file is an INPUT, and the first draft did not treat it as one

`exec.HALT` is now read from the engine thread — the thread that
pumps the live arm's socket, drains the cancel-all sweep and
evaluates the triggers every 2 ms. The first draft read it with
`std::fs::read_to_string`, which is three hazards at once:

1. **It allocates.** A `String`, on the 2 ms thread, every time the
   file changes.
2. **It is unbounded.** A multi-gigabyte file is a multi-gigabyte
   `String`.
3. **It blocks.** `open` on a FIFO waits for a writer — for ever,
   with no timeout and no log. Put a FIFO where `exec.HALT` goes and
   the engine stops, silently, at boot or mid-session.

(3) is the one that matters. This section already said *"a corrupt
file is not a boot refusal… refusing to start would make a mangled
file a denial of service on an engine that might be needed to flatten
a position."* A FIFO **is** that denial of service, and worse than a
refusal, because a refusal at least prints something.

`read_halt_file` replaces it:

* `stat` first and refuse anything that is not a **regular file** —
  a directory, a device, a socket, a FIFO, and a symlink to any of
  them, since `metadata` follows;
* refuse anything larger than a file we would have written
  (`HALT_FILE_MAX`, 512 B);
* open with **`O_NONBLOCK`**, so even a FIFO swapped in between the
  `stat` and the `open` returns instead of hanging;
* read into the caller's fixed buffer — **zero allocation**, since
  std takes a stack fast path for short paths in both `stat` and
  `open`.

Each of those is pinned by a test. The FIFO one is worth naming: its
break-and-watch does not fail, it **hangs** — the run had to be killed
after 60 s. A test that hangs on the broken code is the right test for
a bug whose symptom is an engine that stops answering.

#### And a halt file that halts NOTHING

A mistyped slot number, a line this binary cannot parse, or a slot
that is not live: the file is there, and no slot is halted.

Silence here is the inverse of the loud boot tell below, and strictly
worse — an operator who asked for a halt, got a clean boot log, and an
engine that is trading. So `halt_file_present` is tracked separately
from `halt_file_adopted`, the boot logs at error level when a file was
present and adopted nothing, and the runtime path counts
`halt_file_inert`.

**`latch_all` is LIVE slots only**, exactly like the trigger loop. A
paper or off slot cannot reach a venue, so halting one changes nothing
it does — but `on_halt_edge` fires a VENUE-WIDE cancel, so
`echo 1 > exec.HALT` with slot 1 on paper would have pulled the live
arm's real quotes off the book for nothing. It is now an inert read
instead, which is what the operator needs to be told.

#### Boot: adopt, and say so in a way nobody can miss

`adopt_halt_file` runs once at boot and latches every slot the file
names, with the reason the *last* run recorded. The boot then logs at
**error** level, not warning, and names each halted slot.

The failure being guarded against is not subtle: an operator reads a
clean boot log, assumes the halt cleared, and waits for quotes that
are never coming.

#### Runtime: the operator's kill switch, without a restart

`poll_halt_file` runs from `on_idle`. An operator writes the file and
the slot stops inside a second — no restart, no control socket, no
new listener to secure.

Two costs are held down deliberately, and each has its own counter
because one number could not have caught both:

| counter | what it counts | what holds it down |
|---|---|---|
| `halt_file_polls` | `stat` calls | the 1 s cadence |
| `halt_file_reads` | `read` calls | the mtime check |

Both are internal instrumentation, pinned by a unit test and by alloc
gate 62 — not an operator surface. An earlier draft of this section
presented them as numbers an operator watches, which they are not; the
operator surfaces are the boot tell and `/state`'s `exec` object.

Without the cadence the engine thread would `stat` five hundred times
a second; without the mtime check it would *read* once a second for
ever. **The read count would not have noticed the cadence breaking**
— removing the cadence leaves reads at 1 while syscalls go to 500/s —
which is exactly why they are two numbers and not one.

The router's own writes refresh the mtime baseline, so a halt edge
does not make the next poll re-read what it just wrote.

**One direction only.** A halt found in the file is adopted; a halt
*absent* from it is not cleared. Deleting the file un-halts nothing in
a running process — it only stops the next boot adopting it. A halt is
sticky, and clearing one is a restart-level decision.

#### Observability: `/metrics` and `/state`

`ExecCounters` — which already crosses the trait boundary — gained
`refused_halted`, `refused_unseeded`, `halts`,
`cancel_all_failures`, `cancel_all_stranded`, `seeded` and a per-slot
`halted` array. `refused_halted` had been counted since commit 3 and
exported nowhere, so a halt reached `/metrics` only folded into
`refused_risk`.

New rows, live slots only as before:

```
engine_exec_refused_halted_total
engine_exec_refused_unseeded_total
engine_exec_halts_total
engine_exec_cancel_all_failures_total
engine_exec_cancel_all_stranded_total
engine_exec_seeded                    (gauge)
engine_exec_slot<N>_halted            (gauge: HaltReason as u8)
```

`engine_exec_slot<N>_halted` is a gauge rather than a counter because
the operator's question is *"is it halted now, and why"*, not *"how
many times"*.

`/state` gained an `exec` object — including `adopted_from_file` and
`halt_file_present`, which took a review round to get right: the first
draft declared and rendered `adopted_from_file` and never assigned it,
so the surface built to answer *"did this engine start halted?"*
answered `0` always. Two fields now carry it through `ExecCounters`,
and the distinction between them is the point: `present` without
`adopted` is the typo case above. It spells the reason as a word,
because `"slot 3 stopped on ws-gap"` is the whole answer and a number
would send the reader to a table. The word table has to be duplicated
in `engine-snapshot` — the dependency runs the other way — so `cli`,
which sees both, pins them together. A silent divergence there would
make `/state` name the wrong reason for a halt, which is the one field
it would be read for.

#### Gate 62: the busiest path in this lane had no alloc gate

`on_idle` runs on the engine thread every 2 ms whether or not anything
is trading. Gates 53 and 61 drive `submit`, `cancel`, `modify` and the
ledger — neither drives `on_idle`. So the hook this lane added, and
then added a *file poll* to, was the one path with no zero-allocation
gate at all.

Gate 62 drives 500 idle moments — one second of real pacing — then
the seeding edge, the five triggers, the halt edge, and
`cancel_all_state` polling through `Working` to `Clear`.

The halt file exists for the whole window **and is modified just
before it**, so the one poll the cadence lets through takes the READ
branch rather than the mtime early return. The gate asserts
`halt_file_polls() == 1` and `halt_file_reads() == 1` — equalities,
not upper bounds, because `reads <= 1` is satisfied by a poll that
never ran at all.

**Two windows, not one.** The steady state and the halt EDGE are
measured separately, because the edge writes `exec.HALT` and
`core_io::write_atomic` allocates a `.tmp` sibling path. That
allocation is real and deliberate; what must never happen is it
recurring per poll. Splitting the windows enforces the distinction
instead of asserting it in a comment.

**And the gate nearly measured nothing — twice.**

First it did not create a halt file at all, so every poll failed at
the first `stat` and returned early; breaking the mtime check changed
nothing and the gate still passed.

Then, with a file, it still never took the read branch, because
`adopt_halt_file` ran before the guard and left the mtime baseline
matching — and the assertion was `reads <= 1`, which cannot tell one
read from none. So the gate reported `ok` over the one new allocating
call in the commit. Both review agents found this independently, and
the fix — touch the file, assert equalities — makes the gate fail by
exactly 1 allocation and 23 bytes when `read_to_string` is put back.

A gate whose subject never runs reports the same `ok` as a gate that
holds. **Three times in this lane now**, which is enough to state the
rule: a new gate is not finished until the thing it measures has been
broken and seen to fail.

#### The exit-75 flake: one lead down

§E6 3a recorded a lead — exit 75 is `EX_TEMPFAIL`, this host has an
fd-exhaustion incident on record, so compare `ulimit -n`.

**Measured: `ulimit -n` is 1,048,576 soft, unlimited hard.** The fd
hypothesis is dead. The flake still reproduces at roughly 1 run in 6
of `cargo test -p cli --lib`, and one further observation is worth
recording: when it fires, **no `test result` line is printed at all**
— the process dies before the harness prints its summary, rather than
after. Still open, still not this lane's doing.

#### Gates

* `cargo test --workspace --exclude bench --no-fail-fast` — **2510
  passed, 0 failed**
* `cargo test --release --test alloc_assertions -- --test-threads=1` —
  **62/62** (gate 62 is new)
* `cargo clippy --workspace --all-targets -- -D warnings` — clean
* `make license-check` — OK, 392 source files

#### What the review round changed

Both agents ran against the working tree with this section already
written, which is what made the prose auditable. The alloc audit
passed; the risk review returned `BLOCK`, correctly.

Taken and fixed here:

1. **The uncapped, untyped, blocking read** — the FIFO hang above.
   Both agents reached it from different directions: the risk review
   by treating the file as an attacker-controlled input, the alloc
   audit by tracing what allocates on the 2 ms thread.
2. **Gate 62 did not measure the read** it claimed to, and `<= 1`
   hid it.
3. **`/state`'s `adopted_from_file` was hard-wired to `0`** — a field
   declared, documented and rendered, and never assigned. The third
   instance in two commits of *an accessor documented as feeding a
   surface, with no caller*.
4. **A present-but-inert halt file booted silently.**
5. **`latch_all` had no live-slot filter**, so a mistyped slot number
   would fire a venue-wide cancel for nothing.
6. Prose: the two counters are not operator surfaces; the render's
   truncation bound is now a compile-time assertion rather than a
   happens-to-fit; a misplaced comment in `on_idle`.

Recorded and NOT fixed here: `core-io/src/state_file.rs`'s doctrine
header still says `write_atomic` runs "never on the tick path", which
has been stale since commit 3 put it on the halt edge. It is a comment
on another crate's invariant and belongs in its own change.

#### E6 is complete

Commit 1 the per-order clamp · commit 2 the venue-fill ledger ·
commit 3a the idle thread · commit 3 the kill switches and the
request/confirm cancel · commit 4 the halt file, read as well as
written.

Known and deliberately open: `exec.HALT` records the halt REASON per
slot but the boot read-back cannot distinguish a halt the engine
wrote from one an operator wrote, so an adopted halt always reports
the recorded reason rather than "adopted". `request_budget_floor` is
required non-zero but is the arm's number, not a router threshold.
And the exit-75 flake above.

## E7 — the E1–E6 review, the zero-copy law, and the TESTNET ramp (2026-09-19)

### Operator rulings (2026-09-19, verbatim intent)

1. **Review everything Opus 5 wrote in E1–E6 before E7**, "for being
   proper written", and **retest everything**.
2. **E7 trades on TESTNET, not mainnet.** Outcome (HIP-4) exists there
   too. The mainnet ramp of plan §9 is deferred to a separate, later
   ruling; every bar in §9 is re-stated below as a testnet bar.
3. **THE ZERO-COPY RULE:** everything that can be done zero-copy is
   done zero-copy; a copy that cannot be avoided carries a comment.
   Enforced by a new `zero-copy-auditor` agent, the same way
   `alloc-auditor` / `risk-reviewer` are enforced — and **all three
   review agents run on Opus 5** (`model: claude-opus-5`). *Amended
   2026-09-23 (ruling O-8): all three now pin `model: claude-opus-5-5`
   (Opus 5.5); `parser-property-tester` stays on Sonnet.*

### What the review was

Four Opus-5 review agents, each with the full E1–E6 diff (`94d3eaf..49d155d`,
41 k lines) and a brief that demanded file:line citations and a
failure scenario per finding. A: the wire layer (encode/sign/send/scan,
signer, core-net transport). B: the arm's lifecycle (fills,
reconciliation, sweep, budget, WS). C: the router, ledger, halt machine
and boot. D: the engine loop, E5 verbs, capture, alloc gates. Their
reports are in the vault (`Claude outputs/review-e1e6/{A,B,C,D}-*.md`,
git-excluded). All four returned **BLOCK** on risk. The top findings
were re-verified by hand against the code before anything was changed;
none was a false positive.

### The blocking findings, and what closed each

| # | where | the hole | the fix |
|---|---|---|---|
| 1 | `http.rs` read_response | a chunked or close-delimited `/exchange` answer was returned as soon as ANY bytes arrived — a truncated `{"status":"ok","response":{"type":"order","data":{"statu` reached the scanner | `Chunked ⇒ Err(BadHttp)`; `CloseDelimited` only once `peer_closed`; `ContentLength` waits for `body_end` |
| 2 | `response.rs` scan | the no-`"statuses"` branch ACCEPTED on the mere absence of the token (fail-open, against the module's own doctrine) | accepted only when `"type":"default"` is present, else `Malformed`; fuzz invariant strengthened (`hl_exchange_response.rs` now requires `statuses` or `type:default` on any accept) |
| 3 | `exchange.rs` send_action | the live submit accepted on `ok.accepted()` alone — no resting/filled outcome required, `oid == 0` bound the slot to a leg the sweep could not see; the stricter predicate lived only in the operator probe | acceptance is `Spend`-aware: Submit ⇒ `any_resting ‖ any_filled`, Cancel ⇒ `any_success`; anything else takes the reject-streak branch |
| 4 | `http.rs` post | polled 50 ms BEFORE flushing rustls, so every request waited a slice before its ciphertext left the host | `Transport::flush()` (new trait method; `TlsTransport` drains `write_tls` to `WouldBlock`) + `reregister` before `read_response`; `write_segments` `Ok(0)`/`WouldBlock` ⇒ `Overflow`, no sleep |
| 5 | `exchange.rs` reconcile | `reconciled = true` on the PARSE succeeding — an empty asset table "agreed" with a venue holding 40 contracts and unlocked the seeding interlock through the interlock | `reconciled` (and the new `last_recon_ok`) only when `drift_legs == 0 && unreconciled_venue_legs == 0` (`recon::unreconciled_venue_legs`, already written for the probe, now wired) |
| 6 | `userws_conn.rs` | no client keepalive on the fill socket: the venue cuts a 60 s-idle `/ws`, so a quiet account was redialled every ~61 s, each redial re-delivering the snapshot on the engine thread with up to 30 s of fresh handshake deadlines, and `ws_gap` never fired | `core_net::Keepalive` 50 s ping / 75 s idle (the ingress crate's law), ONE `HANDSHAKE_DEADLINE` through connect + both subscribes, re-resolve every 3rd connect failure |
| 7 | `exchange.rs` cancel_all_state | `Clear` reachable over legs `cancel_all` never queued (`MAX_PENDING_SWEEPS` 16 < 2×8 families) — the router stopped retrying and every healthy slot got its `max_open_orders` back | `cancel_all_unqueued_mark` beside `cancel_all_mark`; `Stranded` while `cancel_all_unqueued != mark` |
| 8 | `exchange.rs` + `halt.rs` | nothing halted on reconciliation going STALE: `recon_failed` climbed, `reconciled` stayed latched, the arm traded with the safety net silently disabled | `HaltSignal.recon_age_ns` (from `last_recon_ok`), `HaltReason::ReconStale = 7`, `HaltLimits.recon_stale_ms`, **`halt_on_recon_stale_ms` is a REQUIRED live key** (`exec.toml.example` 300000) |
| 9 | `exchange.rs` route_frame | the address budget was credited from a `userFills` row BEFORE its sign/scale were validated — one hostile `px` manufactured request headroom until the next cold boot | credited only when `px_1e8 >= 0 && sz_1e8 > 0`, i.e. only from a row the arm would book |
| 10 | `exchange.rs` post | the budget was charged after the `?` — a request the venue answered unreadably was never counted (governor drifting OPTIMISTIC) | `post_counted` charges on `left_host` regardless of the answer; `on_action_sent(items)` charges a batch per item (§2.2) |
| 11 | `routed.rs` halt file | an unreadable `exec.HALT` (FIFO, directory, non-UTF-8) booted SILENTLY; `poll_halt_file` moved the mtime baseline even on a failed read | `HaltFileRead::{Absent,Refused,Read}`; Refused ⇒ present + inert (the boot tell says so); baseline moves only after a successful read |
| 12 | `routed.rs` halt_slot | `--halt-slot` had no live-slot filter — a typo fired a venue-wide cancel for nothing | `halt_slot() -> bool`, live-only, per-slot warn |
| 13 | `exec_boot.rs` | the live boot tell still printed `caps-DECLARED-NOT-ENFORCED` after E6 enforced them | tell rewritten (`caps-ENFORCED … worst case with every quote working = instance + open × order`) + the HALTS line, pinned |
| 14 | `ledger.rs` | `LedgerCounters` reached no surface; `bind` on a repeated CREATED frame flattened a live position | exported through `ExecCounters`, mirrored to `engine_exec_ledger_*` + `/state`; repeat-frame guard |
| 15 | `strategy-bin15` + `exchange.rs` + `routed.rs` | THREE readers of the roll kind byte, three answers for `0x03` (masked ⇒ settled; `== 1` ⇒ created; refused) | one `core_types::roll_kind_strict` ⇒ `Option<bool>`; all three refuse an unknown kind |
| 16 | `routed.rs` cancel on `Off` | an `Off` slot refused CANCELS (an exit) | cancels pass to the live arm when the venue is allowed (`cancel_on_off` counted); submit/modify still refused |
| 17 | `cli/src/bin` | fill lane 3's producer was DROPPED at boot — the E6 exposure ledger and `on_fill_booked` were reachable only from tests | the producer is handed to `HlExchange::new` on the `--exec` path; a real arm is built when a slot is live on Hyperliquid |
| 18 | `cli/src/paper.rs` | `HlExecCounters` (24 arm counters) reached no gauge — `ws_reconnects`, `recon_failed`, `sweep_left` were invisible | `LiveArmCounters` crosses the trait by value (cold, 1 Hz), mirrored to `engine_exec_hl_*_total` + `engine_exec_hl_budget_remaining` + `/state` `exec.arm_*`; `MAX_COUNTERS` 256 → 512 |
| 19 | `userws.rs` TidRing | O(N) dedupe over a 2,000-row snapshot on the engine thread | open-addressed index (2N slots, Fibonacci hash, backward-shift delete), O(1), 1 M-admit parity test vs a FIFO reference |
| 20 | `userws_conn.rs` | NO loopback test of any kind on the fill socket | `tests/hl_userws_loopback.rs`: 101+first-frame in one segment, split frame, two frames in one read, Ping echo, Close, keepalive ping + idle reconnect, refused upgrade, oversize frame |

Smaller items closed in the same pass: `libc` literal dep in
`exec-router` — **NOT a finding**: `libc = "0.2"` per crate is the
repo's existing convention (cli, core-config, core-io, core-time all do
it; there is no workspace `libc`); `gen_vectors.py` now drives the SDK's
own `order_request_to_order_wire` / `order_wires_to_order_action` and
records the SDK version; `selftest` pins the vector count (25);
`MAX_ORDERS` enforced in all four batch encoders; `BudgetGauge` removed;
`cancel_by_cloid` / `modify_by_cloid` render into the boot-owned `mp` / `req`
buffers (no 16 KiB stack arrays on the engine thread); `POLL_SLICE`
removed from the steady-state pump (it parked the single-writer thread
50 ms out of every 52); `Order.verb` doc corrected (`paper.rs` said the
tape was "no longer replayable" after a cancel — false since E5 4a);
`BacktestCtx::modify` now moves `max_order_notional` (the live gate
clamps modifies; the harness measured only places); `ModifyReq` layout
doc; `core-io::state_file` doctrine header; bench gate labels (the E3
gate was a duplicate "54" — now 58, so 53..62 is complete).

### The zero-copy pass

The rule, as now written into `CLAUDE.md`'s hard rules and enforced by
`make copy-audit` (`scripts/copy-audit.sh` + `scripts/copy-audit-baseline.txt`)
and the `zero-copy-auditor` agent:

> Everything that can be done zero-copy is done zero-copy. A copy that
> cannot be avoided carries, within the eight lines above it,
> `// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>`,
> the way an `unsafe` block carries `// SAFETY:`.

What the pass CHANGED (copies removed, not commented):

* the request body is rendered **in place**: `envelope_open` writes the
  head into the arm's `req`, the action JSON is rendered directly
  behind it, `envelope_close` appends nonce/signature — the old
  `envelope()` copied a ≤ 16 KiB action JSON out of a scratch buffer on
  every order (kept only as a helper, byte-identical by test);
* the connection id is `keccak256_parts(&[action, &tail[..n]])` — the
  old path copied the whole msgpack action (≤ 4 KiB) into a stack
  buffer just to append a ≤ 38 B tail;
* the Agent EIP-712 digest is absorbed as parts (`keccak256_parts`),
  with the typehash and the two `keccak(source)` values cached — the
  old path assembled a 96 B and a 66 B preimage AND recomputed two
  constant keccaks per signature;
* the sweep probe's open-order rows are read in place from the
  response buffer; the selection buffer (`oids`) is boot-owned.

What the pass COMMENTED (designed copies, each with its `// COPY:`):
kernel↔user reads and rustls' plaintext window (`core-net`), the
rx-tail compaction after each frame, the first frame packed into the
101's segment, the ≤ 125 B Ping echo, the serialisers' own writes into
the final wire buffer (`msgpack::put_all`, `request::Json::put`), the
`/info` request renders, ≤ 64 B host `String`s at boot, the 8 B/20 B
POD word assemblies (cloid, connection-id tail, EIP-712 padding), the
64 B `r‖s` secp256k1 hands back by value, and the `Fill` POD into the
lane-3 ring slot (the designed ring copy). Three operator/boot modules
carry a `COPY-DOCTRINE:` header instead (`smoke`, `lifecycle`,
`selftest` — never reachable from the engine loop).

The baseline: **33 legacy sites** (pre-E1: the Polymarket signer's
EIP-712 assembly, the PM dispatcher, `core-net`), listed in
`scripts/copy-audit-baseline.txt`. The script is a ratchet — a NEW
unmarked copy fails, a paid entry is dropped with `--update-baseline`,
and only the operator grows it. Gate at the close: `hits=33 baselined=33
new=0 paid=0` on the exec lane + core-net.
Since BX0 the gate also covers `ingress-binance`, and a `#[cfg(test)]`
ITEM no longer ends the scan of its file ("BX0 — `ingress-binance` joins
the zero-copy gate", above).

### The auditor's own verdict on the pass

The new `zero-copy-auditor` (Opus 5) was then run over the pass itself
— the honest test of an agent is whether it catches its author. It did:
**FAIL**, one hot finding. `UserWs::pump_inner` compacted the unread
tail after EVERY frame (`copy_within`, ≤ 1 MiB), and because `fill_rx`
drains to `WouldBlock` one poll can leave k frames in the buffer — so
a 50-frame burst moved ≈ 1.2 MB on the engine thread, O(k²), while the
`// COPY:` line I had written called it "amortised to one frame's
remainder" and rejected the wrong alternative (a ring) instead of the
right one (`core_net::IoBuf`'s head cursor, already in a crate the
file imports). Fixed: `rx_head` cursor; a drained buffer resets for
free; the ONE compaction happens only when the buffer is full with a
partial frame behind consumed bytes; the handshake's first-frame copy
is gone too (the cursor simply starts after the 101's header block).
RX copies: kernel + TLS + ring slot = 3, the target. TX: TLS + kernel =
2, the target, plus the serialiser's single tolerated write. Also
taken from the report: `#[inline]` on `to_fill`/`to_fill_as` (128 B
`Result<Routed>` by value — structural, not hopeful), the two cold
by-value counter structs got their `// COPY:` lines, and the
`ExecCounters` bound in its comment was corrected (≈ 504 B, not ~440).
Left as the auditor recorded it: `crates/cli` and `crates/engine` are
outside the script's default remit — whether they join the ratchet
with their own baseline is the operator's call.

### Rulings this pass made that the operator may want to reverse

* **Cancels pass on an `Off` slot** (they were refused). Matches the
  halt law — an exit is never blocked. `exec.toml.example` says so.
* **`halt_on_recon_stale_ms` is REQUIRED non-zero on every live slot**
  (new key; example 300000). An `exec.toml` that predates this refuses
  to boot a live slot until the key is added — deliberate.
* **`reconciled` requires `drift_legs == 0 && unreconciled_venue_legs == 0`**
  — a flat account seeds immediately; an account still holding a leg of
  a RETIRED instance (unbound after the roll) waits until it settles.
  That is the safe reading; the alternative was the hole in row 5.
* **`MAX_COUNTERS` 256 → 512** in `core-metrics` (the arm's 24 counters
  + 6 ledger counters would have overflowed the fixed registry).
* **An unreadable `exec.HALT` is present + inert, not absent.**

### E7 — the TESTNET ramp (plan §9 restated; the mainnet ramp is deferred)

The network interlock (LAW E-4's precondition, new in `cli/src/bin`):
`HYPERLIQUID_WS_HOST` (market data — where the asset ids come from) and
`HYPERLIQUID_EXCHANGE_HOST` (the arm) must be on the SAME network or
the boot refuses. A testnet id is a mainnet stranger's market.

Env shape for E7 (operator's `.env`, never read by a session):
`HYPERLIQUID_WS_HOST=api.hyperliquid-testnet.xyz`,
`HYPERLIQUID_API_HOST=api.hyperliquid-testnet.xyz`,
`HYPERLIQUID_EXCHANGE_HOST=api.hyperliquid-testnet.xyz`,
`HYPERLIQUID_SOURCE=b`, the testnet agent key + master address in
`HYPERLIQUID_AGENT_KEY` / `HYPERLIQUID_MASTER_ADDR` (the `Scope::Live`
variables — "live" here means "the arm", not "mainnet"; the smoke's
disjoint `HYPERLIQUID_TESTNET_*` set is unchanged).
`~/multivenue/exec.toml`: slot 3 `mode = "live"`, `venues = ["hyperliquid"]`,
every cap and every `halt_on_*` key present (incl. `halt_on_recon_stale_ms`),
then `--exec ~/multivenue/exec.toml --arm-live 3` on the run line.
Boot tells to expect: `exec: hyperliquid arm ARMED network=testnet …`,
`caps-ENFORCED …`, `HALTS … recon_stale_ms=300000 …`,
`running strategy-set with LIVE slots — real orders will be submitted`
(testnet USDC, but the code path is the real one).

Plan §2.3 still stands as a precondition: confirm from `/state` that
the 15-minute family binds live instances ON TESTNET (E5 §7.1 used
testnet outcome 20182, so the family existed there on 2026-09-19).

Bars — the same shape as §9, on testnet:

* **R0 (plumbing)** — `maker_enabled = 0`, `e_take_1e6 = 900000`,
  `entry_usd_1e6 = 12000000` (chosen 2026-09-19 under the then-believed
  $10 venue floor; the floor is **1 USDC** — see "E4 phase D" above and
  the 2026-09-19 migration entry — so `$2` is the smallest entry that
  survives whole-contract flooring at every ask ≥ 0.50), and, per
  the operator's 2026-09-19 ruling for the testnet account (1,000 USDC,
  trade within 500, as many winning bids as possible even if small):
  `cap_day_usd_1e6 = 500000000`, `cap_instance_usd_1e6 = 20000000`,
  `entry_min_px_1e6 = 500000` (the new optional price floor;
  `700000` is the higher-win-rate alternative), mirrored in
  `exec.toml` (`max_order_usd_1e6 = 20000000`, `max_open_orders = 4`,
  the same two caps). O-E4's "caps stay as paper" is superseded for the
  TESTNET ramp by that ruling; the mainnet caps are a later ruling. Bar: ≥ 96 submitted entries, ≥ 1 venue
  fill, `engine_exec_hl_recon_ok_total` climbing with
  `recon_drift_legs = 0` and `recon_unseen_legs = 0`,
  `unknown_fills = 0`, `refused_no_route = 0`, `fills_unowned = 0`,
  `ws_reconnects` flat over the window, the nightly VENUE line agreeing
  with the venue's own fill list order for order. Proves the loop.
* **R1 (Arm A)** — `e_take_1e6 = 30000`. Bar: ≥ 300 IoCs, the LIVE
  fill rate with a binomial interval, `engine_exec_hl_budget_remaining`
  never at the floor. Testnet liquidity is not mainnet liquidity, so
  this number is a MECHANISM check, not the plan's "number the lane has
  been waiting for" — that one still needs mainnet.
* **R2 (Arm B)** — `maker_enabled = 1`. Bar: requests/day under
  `budget_growth/day`; every requote a MODIFY (`modifies_sent` ≫
  `cancels_sent`); LAW E-8 sweeps clean at every roll (`sweep_left = 0`).
* **R3** — the eight families, only if they have live testnet
  instances.

Gates stay in orders/instances, never hours (the ≤ 2 h window law
governs any replay used to judge). Mainnet is a NEW operator ruling
after R2 on testnet, with the §11 shopping list done.

### E7 — MAINNET R0, ARMED 2026-09-19 13:04:39Z (operator ruling "lets go mainnet")

Testnet R0 could not run — the venue has no rolling 15-minute BTC
family there (vault docs 21/22) — so the operator ruled mainnet
directly after the testnet exec battery (vault doc 23, 12/12) with the
bankroll that was there: **9.8 USDC spot**. Shape (`~/multivenue/`):
`bin15.toml` entry $2 at ask ≥ 0.70 and ≤ p̂ − 2c, `cap_instance $2`,
`cap_day $8`, maker 0, `e_take 0.9`; `exec.toml` slot 3 live, order $2,
open 2, day $8, instance $2, drift halt $2, floor 2000, the other halts
as tested; `strategy.conf` + `EXEC_TOML`/`ARM_LIVE=3`. Credentials: the
four `Scope::Live` keys in the REPO `.env` (the launchd engine's file);
agent `bin15` approved on mainnet **until 2026-10-19** (30 days).

The first hour, as it happened:

* **13:00:40Z — sticky halt `budget-floor` before the first order.**
  `exec-hyperliquid::budget` started an address with no state file
  COLD (remaining 0 < floor 2000) "until the engine has watched itself
  trade" — which it cannot, because the floor refuses the submits that
  would earn it. The premise ("the venue exposes no lifetime figure")
  was wrong: `/info userRateLimit` answers `nRequestsUsed` /
  `nRequestsCap` / `cumVlm`. Fixed by hand for the second boot (state
  file seeded with the venue's figures, `exec.HALT` removed) and in code
  as **E7-F1**: `HlExchange::seed_budget_from_venue` at boot, the cold
  budget now the fallback for a venue that does not answer; the ARMED
  tell prints `budget_source` and `budget_remaining`.
* **13:07:05Z / 13:07:06Z — two IoC entries missed the book**
  (`iocCancelRejected`, "Order could not immediately match against any
  resting orders.") and were counted as VENUE REJECTIONS into
  `halt_on_reject_streak` (5). Five misses in a row are routine for a
  1 s IoC against a 6 s book (vault, the fill model), so the arm would
  have halted sticky on orders that were never refused. **E7-F2**: the
  scanner classifies the venue's miss wording (`HlOk::ioc_misses`,
  `missed()`), the arm counts `ioc_missed` (`/metrics
  engine_exec_hl_ioc_missed_total`) and moves NEITHER streak; the caller
  still sees the error and the member still retries. A batch with one
  genuine refusal is still a refusal; a cancel is never a "miss".
* **13:08:59Z — the first fill: BUY 2 @ 0.89 `#42681`** (the No leg of
  the 13:00 instance), $1.78, `fee 0.0`, cloid slot 3 / id 7, on
  `userFills` within the second, six reconciliations agreeing, drift 0.
* **13:15:09Z — settlement at 1.0 × 2 = $2.00 with `fee 0.002688`
  USDC.** HIP-4's fee is charged on SETTLEMENT, not on the trade:
  0.1344 % of the payout on this row — 14 bps before the account's 4 %
  referral discount, and the 13:30:06Z LOSING settlement (payout 0)
  charged 0. The 2026-09-15 "fees are zero on the wire" finding was
  true of the TRADE row only. MEASURED, per the fee law — and carried
  the same day: `fees.toml` `[fees.hl] prediction_settle = "14:14"` →
  `--fee-bps hl.prediction.settle:14:14` → `FillEngine::settle_binary`
  charges it on the payout (migration entry of 2026-09-19).
* 13:17:19Z — the next entry, BUY 2 @ 0.75 `#42700` (Yes, 13:15
  instance), $1.50. USDC 8.517312 = 9.8 − 1.78 + 2.00 − 0.002688 − 1.50
  to the cent.
* **15:32:22Z — sticky halt `recon-drift` in the SAME SECOND as an
  entry fill** (BUY 2 @ 0.70 `#42810`, venue time 15:32:22.543Z,
  booked from `userFills` and later settled at 1.0 — the position was
  never wrong). The reconciler's 60 s cycle came due during the
  submit's round trip; the venue's `spotClearinghouseState` already
  carried the leg, the `userFills` push had not reached the arm, and
  2 contracts against 0 booked is $2 at the outcome ceiling — exactly
  `halt_on_recon_drift_usd_1e6` — so `recon_drift_max_qty_1e6`, a
  high-water mark, latched it for good. The 16:05Z daily restart then
  came up STARTED HALTED from `exec.HALT`; nothing traded from 15:32Z
  until the operator's next restart. **E7-F3**: a disagreement reaches
  the high-water mark only when the NEXT reconciliation sees one too
  (`HlExchange::note_drift`, the minimum of two consecutive worsts);
  `recon_drift_legs` stays the instantaneous level, so the race is
  still visible. A lost fill survives 60 s; a race does not. The halt's
  sensitivity to a genuine drift is delayed by one cycle, which the
  per-order cap bounds.

The interlock, the caps, the seeding, the sweep on a real roll, the fill
on the stream, the reconciliation and the settlement booking all did
what E1–E7 said they would; the two findings are both in the arm's
bookkeeping, and both are fixed above. R0's bar (≥ 96 entries, ≥ 1
fill, recon agreeing, `unknown_fills = 0`) is being measured on
mainnet; R1/R2 remain operator rulings.

### E7 — the SESSION BOUND (operator ruling 2026-09-19: "run until it either earns +15 USDC or loses 5 USDC")

The operator's stopping rule for the mainnet ramp, as a control the
engine enforces rather than a number a human watches: two OPTIONAL
`exec.toml` keys on the live slot, `halt_on_gain_usd_1e6` and
`halt_on_loss_usd_1e6` (USD ×1e6; `0` or absent = no bound on that
side; the five fault halts stay REQUIRED whatever these say), judged by
the same sticky halt machine as every kill switch — reasons
`pnl-gain` (8) and `pnl-loss` (9), cancel-all on the edge, `exec.HALT`
written, refused submits until an operator restart. A gain halt is a
halt: "run until" means until.

**What is measured, and from where.** The account's SPOT USDC as the
reconciler already reads it (`spotClearinghouseState`, every 60 s on the
idle path), against an ANCHOR: the balance at the first reconciliation
of the session that found the account FLAT — no outcome leg with a
non-zero holding (`recon::account_view`). The anchor is written once to
`exec-pnl-anchor.state` beside the budget file (`<master>\t<usdc ×1e6>\t<unix s>`,
another address's line is not an anchor) and restored on every boot, so
the SESSION outlives the process: the launchd `KeepAlive` relaunches the
engine within a minute of any exit, and a bound that re-anchored per
boot would be a bound on nothing. An operator starts a new session by
deleting the file before a restart — the manual step every sticky halt
already requires, not an auto-resume.

**Judged only while flat.** An outcome leg is worth anything from 0 to
1 USDC until the venue settles it, so an account holding one has no
P&L to read: the premium it paid is not a loss, the payout it may get
is not a gain. `HaltSignal::pnl_flat` (anchored AND no leg held AND a
balance read this process) gates the comparison; `pnl_delta_usd_1e6`
is spot USDC minus the anchor. Fees are in the balance, so the bound is
on the venue's own net figure — nothing the ledger believes enters it.
The bound is checked LAST in `trigger_for`: when a fault and the bound
coincide, the fault is the reason recorded.

**Tells.** ARMED: `pnl_anchor_usd_1e6=<n>` (0 = not anchored yet) and
`pnl_anchor_state=<path>`; HALTS: `session_bound=+$15/-$5`; `/state`
`exec.arm_pnl_anchor_usd_1e6` / `exec.arm_session_pnl_usd_1e6`;
`/metrics` `engine_exec_hl_pnl_anchor_usd_1e6` /
`engine_exec_hl_session_pnl_usd_1e6` (gauges); a failed anchor write
counts `HlExecCounters::anchor_persist_failed` (in-memory anchor stands
for the process, the next boot re-anchors). Cadence: the halt can lag
the crossing by up to one reconciliation (60 s) plus one halt poll;
that lag is bounded by the per-order cap, which is exactly the point of
having both.

### Gates run at the close of the pass (2026-09-19, Mac)

* `cargo check --workspace --all-targets` — clean
* `cargo clippy --workspace --all-targets -- -D warnings` — clean
* `cargo nextest run --workspace` — **2585 passed, 1 skipped** (the
  first full run was 2580/2581: the one red was the FIFO test's old
  expectation, updated to the present-and-inert ruling; the four
  `hl_userws_loopback` scripts are new and green on first run)
* `make alloc-assert` — **62/62 at 0 B/op**, fresh `Compiling bench`
  (gate 60 re-cut over `stage_modify` + `seal` in the arm's own
  buffers; gate 58 is E3's, relabelled from its duplicate "54")
* `make copy-audit` — `hits=33 baselined=33 new=0 paid=0`
* `make license-check` — OK (392 tracked source files; the three new
  files carry the header and join the count at commit)
* `cargo build --release -p cli` — relinked 16:23 local; the running
  engine picks it up at its next restart (G0)
* fuzz `hl_exchange_response` / `hl_msgpack_encode` / `hl_user_events`
  — 300 s each (`cargo +nightly fuzz run`), 25.5 M / 24.4 M / 24.1 M
  runs, no crash
* `cd claude-worker && uv run pytest -q` — 1153 passed, 3 skipped
* the whole battery was re-run after the cursor fix — the session log
  §4b carries the second set of numbers

### Known and deliberately open after this pass

* `THIRD-PARTY-NOTICES.md` — no dependency changed (the `libc` item was
  not a change), so `make license-deps` was not required; `cargo-about`
  / `cargo-deny` are not installed on this host in any case.
* The chunked `/exchange` answer is REFUSED, not dechunked. If the venue
  ever moves behind an edge that chunks, `core_net::http1::dechunk_in_place`
  exists; the refusal is the fail-fast reading of `lib.rs`'s doctrine.
* The foreign-fill tape write and the settlement attribution (E4's
  "deliberately not here") are unchanged.
* `.claude/settings.json` still names `claude-opus-4-6` as the SESSION
  model; only the three review agents were pinned to `claude-opus-5`
  (the ruling was about the agents) — since ruling O-8 (2026-09-23)
  to `claude-opus-5-5`. Operator's call.

## E8 — the BIN15 settlement law (S1, 2026-09-24)

### LAW E-11 — a binary settles on the window the venue's rules text names

> **A HIP-4 binary settles on the window the venue's rules text names —
> today the 60-second TWAP of the underlying's perp MARK ENDING at the
> expiry, `TWAP[T − 60 s, T] ≥ strike`. The harness, the audit and (from
> BIN15 S3) the member share ONE implementation of that window; none
> restates it.**

The outcome.xyz rules text for the rolling BTC 15-minute market:
*"Settlement is according to the 60-second TWAP of BTC-USDC perp mark
price ENDING at <expiry> UTC."* Until 2026-09-24 the harness
(`cli::backtest::binary::settle_value`), the audit (which reaches the
same function) and the lane's restart law read the minute AFTER the
expiry — wrong on ~8 % of instances. The corrected law was confirmed on
the account's OWN venue settlements before any code moved: all 15
`userFills` rows with `dir = "Settlement"` of the 2026-09-19 mainnet
session agree with `TWAP[T − 60, T]`; the old window agreed with 14.

What shares the law (E-10 is reserved for the doc-24 bankroll law):

* `core_types::binary_settle_open_ns` / `binary_twap_segment` — the
  window and the TIME-weighted piece (the mark in force counts for as
  long as it was the mark, the last one carried forward, `dt` clipped at
  the window's edges, `i128`) — in the one crate every consumer already
  depends on.
* `cli::backtest::binary::settle_reference_1e6` / `settle_value` — the
  harness (`backtest`, `--member`) and `audit-pnl` both settle through
  it; `>=` settles in the money. **The evidence is strict**: a mark in
  force at the window's open, no piece longer than
  `SETTLE_MARK_GAP_MAX_NS` (10 s — the venue marks every 1–3 s, so a
  longer hole is a capture gap) anywhere across the window including
  the carry to the expiry, and at least `SETTLE_MIN_MARKS` (3) marks
  inside it. Anything less is UNSETTLED — counted, never guessed, never
  averaged over part of the minute.
* The value is knowable AT the expiry, so the harness settles a slot at
  the expiry and the successor trades from `T`; an order still resting
  on the slot is cancelled at that instant (the venue clears the book at
  `T`) and counted in `settled_sym_orders_canceled`.
* **The live member does not share it YET.** Until BIN15 S3 lands, the
  member's pricer still builds its horizon for a window AFTER the expiry
  (`τ + twap/3`); S3 moves it onto this window. A harness number scored
  on E-11 is therefore scoring a member that priced the old window.

**The venue-published cross-check.** The venue prints each settlement
price as the SUCCESSOR's strike (rounded to the strike grid, ~9.5 s after
the expiry), so `next_strike > strike` is the venue's own label. Both
harness surfaces carry it (`BinaryInstance::next_strike_1e6`) and print
`settle_disagree_next_strike` beside `next_strike_checked` (ties
excluded). **A non-zero disagreement is a finding** — a mark tape that
misses the venue's marks, a clock error, or a law change — and is never
absorbed into the P&L silently. The sidecar carries the venue label per
row (`y_next_strike`, `-1` unknown or a tie) beside `y`.

**The settlement-sensitive minute is the one BEFORE each quarter-hour.**
The engine is down 75–90 s across a restart, and an instance whose
settlement minute is missing from the tape is unsettleable offline
(counted, and lost to the accrual). So no restart window may cover
`[T − 60 s, T]` of a live instance: manual restarts avoid hh:14–15,
hh:29–30, hh:44–45 and hh:59–00 (ruling O-6), and the routine 00:10 /
08:30 / 16:05Z slots are clear. (The ingress re-announces every bound
family at its first Steady, so the member binds the live instance as
soon as the boot completes.)

## HYPARB — slot 0: paper-first, TESTNET-only EVM writes (H0–H9, 2026-09-23)

Slot 0 is `hyparb`, the HyperEVM AMM ↔ Hyperliquid Core arb
(`crates/strategy-hyparb`). It trades **PAPER**; the mask flip that puts
it in `strategy.conf` is an operator action after H9 (O-H8). Its only
real submission path is the HyperEVM **TESTNET** shadow (chain 998).
Build record: `docs/hyparb-build-plan.md` §16.

### Slot 0 is never armed live (H9)

`exec_boot::NEVER_LIVE_SLOTS` holds slot 0: a `mode = "live"` slot 0
refuses the boot even when both arming switches agree and the venue it
names has a live arm. The reason is structural, not cautious: the
member's AMM leg has NO live arm (HyperEVM writes are testnet-only),
while its hedge legs name Hyperliquid, which DOES — arming slot 0 would
send real hedges against paper swaps, a one-legged arb building real
inventory. Pinned by `exec_boot::tests::slot_0_hyparb_is_never_armed_live`.

### The member's caps (`hyparb.toml`, paper)

| cap | key | shipped value |
|---|---|---|
| one arb | `max_order_usd_1e6` | $100 |
| one pool (a `[[pool]]` may set its own) | `cap_instance_usd_1e6` | $100 |
| one UTC day of AMM notional | `cap_day_usd_1e6` | $2,000 |
| unhedged inventory, all coins | `inventory_cap_usd_1e6` | $300 |

The inventory cap is an **entry gate**: a breach halts NEW arbs; hedges
and the flattening timer keep running (they are how the exposure comes
down), and the halt lifts only under half the cap. H9 closed a hole in
it: a coin whose hedge books had gone unusable was valued at **$0**, so
a dead book could lift the halt with the exposure still on. Now a coin
is valued at its last usable mid; inventory in a coin that never had a
mid cannot be valued at all, and unvalued inventory both SETS the halt
and never lifts it — an exposure of unknown size is under no cap.

### The EVM write path's interlock

1. **Compiled:** `exec_hyperevm::EVM_ARM_CHAIN_IDS = [998]`,
   compile-time asserted; `Network` has no mainnet variant, so an arm
   for chain 999 cannot be constructed.
2. **Signed:** every transaction is EIP-155 — chain id 998 inside the
   signed payload, so no signature this engine makes is valid on 999
   (the node answers "invalid chain ID"; the arm HALTS on that refusal).
3. **At the wire, at boot:** the write endpoint's `eth_chainId` must be
   998 (the arm halts otherwise), and `check_chains(read, write,
   --evm-hybrid)` allows same-chain, or exactly 999 reads → 998 writes
   with the O-H12 switch — never the inverse.
4. **Switches:** `mode = "testnet"` in `hyparb.toml` AND `--evm-testnet`
   (+ `--evm-hybrid` for mainnet reads); either alone refuses.
5. **Per-process halt:** a node that answers a different hash than the
   one signed, a receipt from another sender or to another target, or a
   node refusing the chain HALTS the arm for the life of the process —
   nothing more is sent; only a restart re-arms it.

**Refuse vs dark (H9).** A misconfiguration or a VERIFIED interlock
failure refuses the boot: a wrong chain, a 999 read without the switch,
a missing or malformed key or URL, an executor wallet 0 does not own or
an executor address with no contract. An endpoint that cannot be used
right now — DNS, transport, a non-200, the public endpoint's `-32005`
throttle (a JSON-RPC error inside an HTTP 200, so it is classified by
its message: `ArmErr::RateLimited`), an unreadable answer to a routine
read — or an unfunded wallet 0 leaves the shadow **DARK**: logged at
ERROR, `engine_hyparb_evm_dark = 1`, nothing sent, the paper member
running. Dark bypasses no interlock: an interlock that could not be
verified is still not passed, because nothing is sent. (A mistyped HOST
is a DNS failure and therefore dark — it looks like an outage from
here; the ERROR line names it.)

### The shadow is a submission path OUTSIDE the risk gate

The E6 risk gate sits in `RoutedDispatcher`, on `Order`s. The shadow
does not go through it: the `evm-shadow` thread sends testnet swaps
straight from the member's decision log. Its bounds are therefore its
own, and they are these:

* **Testnet only** (the interlock above) — no swap it sends can move
  mainnet value.
* **One swap in flight**, from **wallet 0 only**: the executor accepts
  its immutable owner alone (`NotOwner`), boot reads `owner()` and
  refuses an executor wallet 0 does not own. While a swap is in flight
  a burst of decisions collapses to its newest (`superseded`, counted).
  Wallets 1..7 carry the battery's nonce and ordering probes (0-value
  self-transfers), never a swap.
* **Fixed size:** every swap is `[testnet] amount_raw` exact input,
  whatever the decision's notional.
* **Gas:** G2 — a quarter of the decision's net edge over the swap's gas
  limit, never above `gas_p99_usd_1e6` — so a shadow swap never bids
  more than the artifact's p99 gas.
* **Nonces:** one transaction in flight per wallet, `Ready` only when
  `latest == pending`; a refusal that proves the nonce was not taken
  returns it, anything else quarantines the wallet until a settled sync.

A MAINNET write path would need an executor allow-list or one executor
per wallet (a contract change under its own review) and a place inside
the risk gate — neither exists, and the chain allow-list keeps it so.

### The executor answers three callback names (H9d, 2026-09-24)

Found live by the H8 battery: its first swap reverted on chain because
the testnet pool is Hyperswap V3, whose pools call
`hyperswapV3SwapCallback` — a name the executor did not have (mainnet
Hyperswap runs the same pool code). The contract now answers it; it
routes to the same `_pay` as `uniswapV3SwapCallback` and
`algebraSwapCallback`, which pays only the pool the current call is
swapping and only inside that swap (transient storage), so a third name
adds no trust. An executor deployed before H9d must be redeployed
(`evm-testnet deploy`, then `[testnet] executor`); boot's `owner()` check
does not tell the two apart, the first swap's revert does. The cost of
the miss was gas only — `BelowMinOut` and a callback revert both move no
inventory.

### Signing keys

`HYPEREVM_TESTNET_KEY`, else — by the 2026-09-23 operator ruling ("reuse
the one we used for HL") — `HYPERLIQUID_TESTNET_AGENT_KEY`, which makes
this lane a NEW reader of the E3 gate's testnet key; never the mainnet
agent key (pinned by a test). Read through
`core_config::SecretKeyBytes::from_hex_env` (the one env-hex-key reader:
mlock'd page, the env string zeroized, errors name the variable, never
the value). Wallets 1..7 are derived from it, each in its own mlock'd
page. The engine never opens `.env`; `scripts/evm-testnet.sh` sources it
exactly as `exec-smoke.sh` does.

### What H9 fixed on the write path (review, 2026-09-23)

* **R4 (critical)** — the shadow picked wallets round-robin; the
  executor accepts its owner (wallet 0) only, so every swap from wallets
  1.. would have reverted and burnt gas. Fixed as above.
* **R3** — every boot failure refused the engine; transport, throttle
  and funding failures now leave the shadow dark (the second review
  caught the throttle: it arrives as an HTTP 200 and first scanned as
  "unreadable" — a refusal).
* **A false "may have left the host"** — a kept-alive connection the
  server had since closed took the next request's bytes into a dead
  socket, and the arm booked a `MaybeSent` (a wallet lost to the receipt
  timeout). `HttpsPost` now retires a connection the answer closes or
  announces closing, and probes one before reusing it.
* **Allocations on every TLS ingress thread** — `TlsTransport::read`
  built its `WouldBlock` with `io::Error::new(kind, "…")`: three heap
  allocations at the end of EVERY drain loop, on every TLS socket in the
  engine, since 2026-08-14. Now `io::Error::from(kind)`. Found by a
  measurement the alloc gates could not make (they drive
  `TestTransport`); bench gate 72 now pins the HTTPS cycle.
* **A chunked answer could abort the engine** (found by the H9 fuzz
  run, `http1_response`; present since the initial commit): a chunk
  size near `usize::MAX` wrapped the chunk's end below its start, the
  framing check read the size line's own CRLF and passed, and the copy
  pass panicked on the inverted range — `panic = "abort"` in release,
  so one malformed chunked answer ended the process. Reachable from
  `boot_http` (every venue's boot REST) and `HttpsPost`. The walk now
  uses checked arithmetic: such a size is `Malformed`.
* **The shadow's steady state left the opt-out.** `evm_testnet.rs`
  carries a file-level `COPY-DOCTRINE:` header, which the copy audit
  honours for the whole file — and the engine thread's `drain` lived
  there. The tap and the worker are now `crates/cli/src/evm_shadow.rs`
  (no opt-out, in the audit's default sweep), and `drain` walks the
  member's decision log in place instead of staging 64 × 48 B per
  report.
* **Residue, recorded not fixed:** rustls 0.23's buffered API allocates
  one `Vec` per TLS record sealed and one per application-data record
  decrypted — on every TLS socket, the E-lane's `HlHttp` included (it
  writes head and body as two records: two allocations where one would
  do; its stale-keep-alive shape WAS fixed — H9c, "The live arm's stale
  keep-alive"). Gate 72 pins it at exactly 2 per `HttpsPost` request. Removing it
  is rustls' unbuffered API (`UnbufferedClientConnection`) — a core-net
  transport decision for the operator, not a HYPARB-lane change.
