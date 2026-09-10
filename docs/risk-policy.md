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

> **Not yet enforceable — a V8 precondition, stated plainly.** The QLIKE
> window lives in `core_vol::VolEngine` and is zeroed at construction;
> the V5 boot seed restores the fitted `(x, y)` pairs but NOT the
> window. With the standing restart cadence (`scripts/daily-restart.sh`,
> five slots a day) and an 8 h campaign, sixty settlements cannot
> accumulate inside one process, so trigger 7 **cannot arm as deployed
> today** and its halt would in any case be cleared by the next
> scheduled restart — which is the "auto-resume" the section above
> forbids. Two things must land before the lane is enabled at V8: the
> QLIKE window must persist across restarts (alongside the pair seed),
> and `engine_vrp_killed` must carry an operator-facing alert so a halt
> is seen rather than erased. The member-side code is written and tested
> so that closing this is a seed change, not a strategy change. **Until
> both land, kill criterion 3 is an operator obligation — read the two
> QLIKE gauges — not an engine control.**

## Signing-key handling

- The EIP-712 signing key is loaded from the project-root `.env` file only.
- Boot-time: the key is `mlock`'d into its own page (see
  `crates/core-config::SecretKeyBytes`).
- Drop: the key page is zeroized and `munlock`'d.
- Debug: the `Secrets` struct has a custom `Debug` impl that redacts the
  key. Any code that formats or logs the key without redaction fails the
  `risk-reviewer` subagent check.

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
