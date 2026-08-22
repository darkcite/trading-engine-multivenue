# Options support — deferred plan (PROPOSED, NOT SCHEDULED)

Status: **Phase 9+ candidate, P&L-gated exactly like paid APIs** (plan §13
precedent: the OKX `EVENTS` class row). Authored session H0 (2026-08-16) as
Phase-8h deliverable D. **Nothing in this document lands in 8h**; the 8h
design's only obligation to it is negative — close no doors (§7 of this doc
cross-checks that). Entry gate: §9.

## 1. Venue inventory — who actually lists options

| venue | options? | shape |
|---|---|---|
| **Deribit** | **YES — flagship.** | European cash-settled crypto options (BTC/ETH families, further underlyings as listed), the deepest books in the class. Same JSON-RPC WS we already speak (8c ingress); instruments like `BTC-27MAR26-60000-C`; `ticker` channel carries mark px, mark IV, greeks, open interest, underlying/index px. |
| **OKX** | YES. | `instType = OPTION` on the v5 API we already speak (8b); European, coin-margined; `opt-summary` channel for greeks/IV; discovery via the same `/instruments` surface 8e already fuzzes (`okx_instruments`). |
| **Binance** | YES — but a **separate stack.** | European options (USDT-settled) live on a dedicated endpoint family (`eapi` REST + its own WS host), NOT the spot WS `ingress-binance` speaks today. Treat as a new half-ingress, not a flag on the old one. |
| **Hyperliquid** | **NO.** | HIP-4 outcome markets are digital-option-LIKE payoffs (binary settle at 0/1) but are NOT an options instrument class: no strike×expiry chain, no premium/greeks surface, no exercise semantics. Say it explicitly so nobody "adds HL options" by mistake — HL work stays in the HIP-4 lane that exists. |
| **Polymarket** | **NO.** | Binary event contracts; same statement as HL. PM's option-LIKE structure is already the engine's home turf and needs nothing from this plan. |

Rollout consequence: **Deribit first** (one venue, richest data, ingress
already built), OKX second (same-API increment), Binance last (new endpoint
stack = the largest cost for the least marginal signal).

## 2. Discovery: strike×expiry universe vs the SymbolId space

- **Capacity is a non-issue by construction:** SymbolId is venue byte +
  24-bit ordinal — 16.7 M instruments per venue. A full Deribit chain is
  order-of-thousands of instruments; OKX similar; Binance smaller. Three
  orders of magnitude of headroom. No identity-scheme change needed.
- **The real costs are elsewhere:** boot discovery time/rate limits
  (thousands of instrument rows per venue), per-instrument book/ticker
  subscription fan-out (venue WS subscription caps), `MultiBook`/lane sizing,
  and capture volume. Therefore discovery MUST ship with a **universe filter
  policy**: moneyness band (e.g. ±N strikes around ATM) × nearest-M expiries,
  configured per venue, applied at boot exactly where 8e's per-venue
  discovery (`ingress-*/src/discovery.rs`) already builds the instrument
  table. Filtered-out instruments are never allocated ordinals — the
  engine's working set stays bounded and explicit.
- Expiry churn: chains roll daily/weekly. Boot-snapshot universes (the 8e
  doctrine) stay correct — a new expiry enters at the next boot. Intraday
  chain-append is deliberately NOT proposed (violates the static-universe
  simplicity that everything from the validator to the vm leans on).
- Discovery parsers: the existing fuzz-target family extends
  (`okx_instruments` already exists; Deribit/Binance options discovery gets
  the same §21.4 treatment — every new byte scanner arrives with proptest +
  fuzz. Non-negotiable, unchanged.)

## 3. Tick-lane field deltas: mark / IV / greeks / underlying

The 64 B `Tick` is BBO-shaped (bid/ask px+qty) and stays so — options BBO
flows through it unchanged (an option book is a book). The options-specific
surface (mark px, mark IV, greeks, OI, underlying px) does NOT fit `Tick`,
and three routes were weighed:

1. **`ChannelEvent` rides (RECOMMENDED v1):** append new `ChannelId`s
   (`MarkIv`, `Greeks`, `Underlying`, `OpenInterest` — appended after 10,
   per the append-only ABI doctrine that added 9/10 in G1) carrying pairs in
   `v0`/`v1` (e.g. mark px ×1e6 / IV ×1e9). Zero new PODs, capture + replay
   + audit-replay inherit for free, cadence bands per channel slot in the
   existing per-venue audit. Cost: greeks arrive as events, not as hot-lane
   state — fine while triggers are IV-level/skew class (§4), which read at
   tick cadence, not per-greek-update.
2. Dedicated `OptionTick` POD + parallel lane: only if an option strategy
   ever needs greeks IN the hot path at tick latency. New SlotKind, new
   lane, new alloc gates — the expensive road; explicitly deferred until a
   strategy demands it with numbers.
3. Evolving `Tick` itself (spending its 15 pad bytes): REJECTED — v2 files
   pin "padding explicit and zeroed"; readers key semantics on version, and
   a half-spent pad breaks the one-slot-fits-all lane machinery for every
   non-options venue. The pad stays reserved.

## 4. Trigger-family candidates (strategy layer, in v1-grammar spirit)

All expressible as fixed-size row predicates over per-sym scalar state —
the `strategy-vm` shape survives:

- **IV level breach** — `mark_iv` vs threshold (the options `level_breach`).
- **IV skew** — IV(strike A) − IV(strike B) vs band, A/B two option syms:
  EXACTLY a `cross_deviation`-shaped two-leg rule; the D2 venue-explicit
  leg machinery already carries it.
- **Term structure** — IV(near expiry) − IV(far expiry): same two-leg shape.
- **Basis vs underlying** — option-implied forward vs venue spot (leg on
  another venue — the multivenue point of this engine).
- **Delta-hedged multileg** — option leg + underlying hedge leg sized by
  delta: this is THE flagship consumer of multileg v2
  (`docs/phase-8h-design.md` §16 — `ratio_1e6` on `ActionLeg` was shaped
  with a delta ratio in mind). Options trading beyond naked directional
  entries is gated on multileg v2 landing first. Dependency stated plainly:
  **options v1 = single-leg IV/level triggers; options v2 = after multileg.**

## 5. Risk-policy treatment (docs/risk-policy.md gains a section BEFORE any live option order)

- **Long options:** cap by PREMIUM PAID — premium is the max loss, so the
  existing notional-cap machinery applies with "notional := premium".
  Per-order / per-sym / total premium caps mirror the $100/$250/$1 000
  structure (tighten-only, phased-loosening procedure unchanged).
- **Short options:** max loss ≠ premium. v1 policy: **naked shorts are
  DISALLOWED** (validator-refusable the same way caps are); defined-risk
  structures (spreads) only AFTER multileg v2 can express them, capped by
  max structural loss (strike distance × size). This is the conservative
  gate that lets short-vol wait until the engine can even represent a hedge.
- **Settlement/exercise:** all three venues are European cash-settled — no
  early-assignment state machine needed. Expiry = a settlement fill at the
  venue's settlement price; position book must handle an instrument
  REACHING expiry mid-session (settle + remove, kill-switch on missing
  settlement data). No physical delivery anywhere.
- **Kill-switch interplay:** unrealized-loss-per-sym on marks includes IV
  moves by construction (mark px moves); the $200/day realized line and
  sticky halt apply unchanged. Greeks-based portfolio limits (net delta/vega
  caps) are named here as the risk-policy upgrade that MUST accompany any
  multi-leg option book — numbers to be proposed in the risk note that
  unlocks the phase (§9).

## 6. Dispatcher + signing deltas (8j interplay)

- **Deribit:** the 8j `deribit-dispatcher` (JSON-RPC private,
  `client_credentials`) covers options as-is — same order methods, same
  auth, instrument name is just a string. Marginal cost ≈ zero.
- **OKX:** the 8j `okx-dispatcher` (private WS, HMAC login) covers
  `instType OPTION` orders on the same order channel. Marginal cost ≈ small
  (tick-size/lot rules per instrument from discovery metadata).
- **Binance options:** separate `eapi` REST/WS with its own HMAC signing
  path ⇒ a NEW dispatcher crate, not an extension. This is the single
  biggest line item in the whole plan and the reason Binance goes last.
- Rate-limit governors, golden signing vectors, testnet-first: the 8j
  doctrine applies unchanged to every one of these.

## 7. Capture / replay / backtest impact (and the 8h cross-check)

- Capture: new `ChannelId`s flow into the existing per-venue
  `*-events.pmlr` — no format change, version stays, `audit-replay` gains
  cadence bands per new channel. Options BBO rides `*-ticks.pmlr` untouched.
- Replay/backtest: the 8h harness design is already venue-agnostic and
  merge-based (`phase-8h-design.md` §3–§4, §16.3 door-closers) — an option
  sym is just another namespaced SymbolId with a book. TWO extensions are
  needed when this plan activates, both localized in the fill/accounting
  module: (a) expiry settlement events in the equity curve; (b) fee tables
  per venue's options schedule (premium-relative fees). Neither requires
  schema-1 changes (aggregate USD stays aggregate USD).
- **8h closes no doors — checked:** capture-observed universes (harness)
  admit option syms automatically; `ChannelEvent` consumption is already
  declared non-consumed-v1 with the extension point named; market-map's
  `<venue>:<instrument>` naming covers option names verbatim.

## 8. What this plan does NOT propose

No IV surface fitting, no smile models, no vol marketplace-making, no
American exercise, no physical delivery, no cross-margin optimization, no
paid options-data vendors (the venues' own WS/REST are free and sufficient
for the trigger families in §4). Every one of those is a separate proposal
with its own P&L justification if it ever comes.

## 9. Rollout order + the explicit entry gate

Order, each step gated on the previous one's evidence:

1. **Deribit options capture soak** (ingress: discovery filter + new
   channels; paper, no strategies) — proves data completeness §6.6-style.
2. **IV trigger family in `strategy-vm` grammar** (single-leg, paper) —
   backtested by the 8h harness over the soak capture.
3. **Deribit paper → live tiny caps** (premium-cap policy from §5 in
   risk-policy.md first — the risk-reviewer blocks otherwise).
4. **OKX increment**, same ladder.
5. **Delta-hedged multileg** — only after multileg v2 ships (post-8j).
6. **Binance options** — only if steps 1–4 P&L justifies a new endpoint
   stack.

**Entry gate for step 1 (nothing starts before ALL of these):** Stage 3
complete (8i risk engine live, 8j dispatchers proven on testnets, 8k live
ramp underway); a written P&L report from the linear-instrument phases
showing the strategy family is profitable AND capacity-constrained enough
that options add expressible edge (the same evidentiary bar as the Phase-6
paid-API gate); operator sign-off per the risk-policy phased-loosening
procedure. Until then this document stays exactly what it is: a parked,
costed map.
