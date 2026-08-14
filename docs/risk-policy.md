# Risk policy

This document is the source-of-truth for every position cap, loss threshold,
and kill-switch trigger in the engine. Any change to it **requires** a
corresponding change to the code in `crates/strategy-*`, `crates/engine`,
or `crates/clob-dispatcher` — and vice versa. The `risk-reviewer` subagent
under `.claude/agents/` will block merges that update one without the other.

## Absolute caps (Phase 0 — paper mode)

| cap                          | value    | enforced in                         |
| ---------------------------- | -------- | ----------------------------------- |
| max open orders per symbol   | 4        | `engine::Engine::on_new_order`      |
| max total open orders        | 32       | `engine::Engine::on_new_order`      |
| max net notional per symbol  | $250     | `strategy-core::RiskGate`           |
| max net notional total       | $1 000   | `strategy-core::RiskGate`           |
| max single-order notional    | $100     | `strategy-core::RiskGate`           |
| max realized loss per day    | $200     | `engine::Engine::kill_if_exceeded`  |
| max unrealized loss per sym  | $100     | `engine::Engine::kill_if_exceeded`  |

Paper mode treats every fill as real for P&L accounting so the caps are
exercised on the same code path that live mode will use.

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
