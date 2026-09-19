# Deribit ingress outage — a Coinbase-routed spot pair's trades channel refused every session

**Date found:** 2026-09-19 11:35Z · **Dark since:** 2026-09-17 16:07:02Z (the 16:05Z
restart) · **Status:** ROOT CAUSE PROVEN and FIXED (`ingress-deribit::row_wants_channel`)
· **Author:** the VRP investigation session (read-only on the host; the fix is one
law change plus its tests).

## What was observed

`/state.ingress[deribit]`: `state 3` (backoff), `ticks 0`, `msgs 3078`,
`reconnects 1539` over 10,616 s of uptime — one session every ~7 s, two messages
each. `/state.vrp`: every counter 0, including `records_ignored` (which grows
constantly on a healthy lane) and `select_scans`. Slot 1 (vrp) was blind for two
days across seven scheduled restarts; the standing daily 09:15Z read-only VRP check
did not flag it, and the dashboard's Ingress row showed `backoff` with the reconnect
count climbing — visible, unread.

`engine.out.log`, every session since 2026-09-17T16:07:02Z:
`deribit: run-loop returned res=Error err_site="subscribe-missing" io_kind="invalid-data" venue_code=1`
— `venue_code` is the COUNT of configured channels missing from the subscribe echo:
**one**. 22,480 such lines by the time of the finding. `chain_rows=32
chain_rows_refused=32` on the vrp boot line is NOT this defect: it is the 32 ETH
rows the BTC-currency filter refuses by design (the registry held its 32 BTC rows).

## Reproduction from the host (before any change)

A plain `websockets` client subscribed the engine's exact 163-channel batch (the 73
instruments from the run's `instrument-manifest.tsv`: 9 static, 64 options) in one
`public/subscribe`. Echo: 162 channels. Missing: `trades.BTC_USDC.100ms`. Probing
each spot channel alone:

| channel | echo |
|---|---|
| `trades.BTC_USDC.100ms` | `[]` (accepted, not subscribed) |
| `trades.BTC_USDC.agg2` | `[]` |
| `trades.BTC_USDC.raw` | `13778 raw_subscriptions_not_available_for_unauthorized` |
| `quote.BTC_USDC` · `book.BTC_USDC.100ms` · `ticker.BTC_USDC.100ms` | echoed |
| `trades.ETH_USDC.100ms` | `[]` |
| `trades.BTC_USDT.100ms` | echoed |

REST `public/get_last_trades_by_instrument?instrument_name=BTC_USDC` →
`11060 not_supported_for_coinbase_routed_spot`. `public/get_instrument` still says
`is_active true, kind spot`. So Deribit now routes its USDC spot pairs through
Coinbase and publishes no trade prints for them; the subscription is accepted and
silently dropped from the echo. The nine configured Deribit instruments were all
active; the option chain was fresh at every boot. The venue's wire drifted, not our
configuration — caught live, as CLAUDE.md pitfall 7 predicts.

## Why one absent name took the whole venue down

`run_loop.rs`, `Dispatch::SubscribeResult`: the first-ever echo of a session that
lacks a configured channel is a BOOT fail-fast — "misconfiguration; refuse to run
venue-blind" — and the session errors into backoff. That doctrine is right for every
channel a member depends on (an option row's quote/ticker, the perp's book). It was
also applied to a spot row's trades channel that nothing consumes: the spot row
exists for its BBO capture (WS6), and no strategy, no capture consumer and no
worker module reads a Deribit spot print.

## The fix

`row_wants_channel` (the single per-row channel-policy law, used by the batch
builder, the verification mask, registration and drop emission alike): a static
SPOT row wants `quote` + `book` (with depth) only — no `ticker` (WS6, unchanged)
and, from this change, no `trades`. The verifier is untouched: the fail-fast stays
exact for every channel that is asked for. Tests re-pinned:
`subscribe_all_spot_combo_and_dvol_channel_policy` (the batch carries no
`trades.BTC_USDC`), `spot_row_verification_and_registration_skip_ticker_and_trades`
(the exact echo shape Deribit now sends passes FULL verification and arms the WS2
discriminator; `sub_count` 5 → 4).

Rejected alternatives: (a) dropping `BTC_USDC` from `universe.toml` — a mid-list
removal shifts the seven USDC-perp `SymbolId`s (append-only law); (b) teaching the
verifier an "optional channel" class — more mask law for a channel nobody wants.

Capture consequence: `deribit-events.pmlr` carries no Trade rows for the spot row
from the first boot on the fixed binary (there were none to carry since 09-17
anyway). Recorded in `docs/migration.md`.

## What should have caught it earlier

- A member enabled whose ONLY venue is down is a red state, not a yellow one:
  `slots[vrp].enabled = 1` with `ingress[deribit].state != UP` for more than one
  reconnect budget. The dashboard should paint the slot, not just the venue.
- `engine_vrp_records_ignored_total` flat at 0 for a whole selection window is the
  member-level tell (it grows constantly on a healthy lane — the P6 record says so).
- The daily VRP check reads `/state.vrp` counters; it should read the venue state
  beside them.
