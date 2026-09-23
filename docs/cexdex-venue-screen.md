# CEX-DEX Venue Screen — method, target inventory, decision rule

**Status: DRAFT — authored 2026-09-19, TON added and probed 2026-09-20, on
operator request. UNCOMMITTED, UNSCHEDULED.**

A repeatable procedure for deciding whether an AMM-versus-orderbook
arbitrage is worth building on a given chain, before any crate is written.
Generalised from the HYPEREVM↔HL study; the build plan it feeds is
`docs/hyparb-build-plan.md`.

> **Where the numbers live.** This document carries *venue and method* facts —
> block times, fee tiers, endpoint behaviour, listing status, the screening
> statistic, the traps checklist. Every *strategy* finding — measured edge,
> pie sizing, backtest P&L, per-target verdicts — stays in the git-excluded
> vault under `docs/research/hyparb/` per the operator law of 2026-09-02,
> and is CITED here, never restated.

---

---

## 0. What generalises

HyperEVM was one instance of a family: **an AMM whose price only updates on a
block boundary, quoted against a continuously-updating orderbook venue whose
tape we already capture.** The method that settled it transfers whole:

* the **realised-flow screen** — classify on-chain swaps by whether they move
  price toward a reference venue, then price them against our own tape. No
  assumption about who wins the race, because those trades happened. Cheap:
  logs + our existing capture.
* the **depth-capped simulation** — walk the real tick map, carry own market
  impact, cap size at the hedge venue's top of book.
* the **traps checklist** (§5), each item of which silently fabricates P&L.

We already capture the orderbook side for **Binance spot + 122 usdm perps,
OKX, Deribit, Bybit and Hyperliquid**. For any EVM chain, the only new work
is the DEX side, and `core-amm` (H1 of the implementation plan) is the AMM
math for all of them.

## 1. The screening statistic — and why "faster chain" is the wrong instinct

The edge is the pool price being stale for one block while the CEX moves. Per
block of length *T*, the dislocation is ~*σ√T*. Round-trip cost *c* is the
pool fee + CEX taker fee + gas.

> **profit per day ≈ (86400 / T) × size × E[max(0, kσ√T − c)]**

Two regimes fall out, and they explain the HyperEVM result exactly:

* **σ√T ≫ c** — cost is irrelevant, and *shorter* blocks win because you get
  more of them. This is the regime CBB traded in 2025.
* **σ√T ≈ c** — the max() truncates most blocks to zero and the opportunity
  collapses **super-linearly**. This is not theoretical: HyperEVM went
  2 s → 0.983 s, shrinking σ√T by ~30 % against a fixed round-trip cost floor,
  and the measured opportunity collapsed with it (magnitude: vault verdict
  §4.1).

So the screen does not rank on block time. It ranks on **σ√T versus c**, which
makes two things decisive that are easy to overlook:

1. **The lowest pool fee tier available on that chain.** A 1 bp tier instead
   of 5 bp moves *c* more than any latency work ever will. This is the single
   highest-leverage variable in the whole screen.
2. **Matched CEX top-of-book depth**, because `size` is capped by it. This was
   the single largest correction in the HyperEVM work (magnitude: vault
   verdict §4.3), and it is measurable before writing any code.

A third variable decides whether *we* can compete at all:

3. **The ordering mechanism.** A priority-gas auction (HyperEVM: p50 0.34
   gwei, p99 258.9 gwei — 2,600× base) is a **capital** race and we lose it.
   A first-come-first-served sequencer is a **latency** race, which is this
   engine's actual comparative advantage. This must be *measured* per chain
   from the receipt distribution, never assumed from documentation.

A fourth variable does not vary across the EVM chains but does across the
target set, and it changes the RISK CLASS rather than the edge:

4. **Atomicity.** On an EVM chain a failed arb costs gas and nothing else —
   the transaction reverts whole. On an asynchronous, message-passing chain
   (TON) a swap is a CHAIN of messages across several blocks with **no atomic
   revert**: a partially-executed arb leaves you holding the wrong asset
   mid-flight, unhedged, with no way to undo it. That is a different failure
   mode from "we lost the race", and it cannot be modelled by the same fill
   law. Any non-atomic chain needs its own screen design, not a port of R2.

## 2. The method, per target (≈1 day each)

Identical to the parent study, so the tooling is already written:

1. Chain-wide `Swap`-topic sweep → live pools → resolve tokens/fees/decimals.
2. Filter to pools whose both legs price against a CEX market we capture.
3. Six **disjoint ≤2 h windows** (the ≤2 h law), spread across sessions.
4. Pull `Swap` logs (they carry post-swap price/liquidity/tick), exact block
   timestamps, per-window seed state and tick maps from a **verified archive**.
5. Run both measurements: realised flow, and the depth-capped simulation.
6. Sample ~500 receipts → gas distribution, sender concentration, ordering
   mechanism.
7. Report the r0→r3 ladder (upper bound → depth → latency → basis+gas).

**Required per target:** an archive-capable endpoint that passes the honesty
probe, a CEX tape we already hold, and AMM math `core-amm` supports.

## 3. Target inventory — R1 probe already run (2026-09-19)

Measured from the container; several public endpoints are blocked by this
environment's egress, which is itself a screen criterion (the Mac has
different egress and reached HyperEVM fine).

| chain | id | block time | gas limit | base fee | V3 swaps / 1000 blk | status |
|---|---|---|---|---|---|---|
| **Optimism** | 10 | **2.000 s** | 40 M | ~0 | **5,570** | reachable — *best measured candidate* |
| HyperEVM | 999 | 0.983 s | 3 M | 0.100 gwei | 1,711 | the parent study — NO-GO |
| BNB Chain | 56 | **0.450 s** | 70 M | ~0 | n/a (endpoint refused logs) | reachable; very short *T* |
| Base | 8453 | — | — | — | — | endpoint 403 here |
| Arbitrum | 42161 | — | — | — | — | endpoint 403 here |
| Polygon | 137 | — | — | — | — | endpoint 401 here |
| Unichain | 130 | — | — | — | — | endpoint 403 here |
| **Solana** | — | **0.263–0.271 s/slot** | — | — | ~265k tx / 60 s | reachable |
| **TON** | — | **0.404–0.433 s/blk** (masterchain, from `gen_utime`) | n/a | n/a | n/a (not EVM logs) | reachable — **but see §3.1: no CEX leg** |

Two readings jump out and both are worth stating before anyone builds:

* **Optimism has 2× HyperEVM's staleness window and 3.3× its V3 swap
  density**, at effectively zero base fee. On the §1 statistic it is the
  strongest EVM candidate we can currently see. It is also an old, well-farmed
  chain — competition density is the thing to measure first, not the gap.
* **BNB (0.45 s) and Solana (0.27 s) are in the wrong regime** for this
  strategy on the §1 argument: *σ√T* is smaller than HyperEVM's, against
  costs that are not smaller. They are worth screening precisely *because*
  the argument predicts they fail — a cheap falsification test of §1 itself.

### Target classes (per the operator's scope ruling)

**Class A — EVM chains reusing our existing CEX ingress.** Optimism, Base,
Arbitrum, BNB, Polygon, Unichain. AMMs: Uniswap V3/V4, Aerodrome (Base),
Velodrome (OP), PancakeSwap V3 (BNB), Camelot (Arb). Cheapest per target:
only the DEX side is new and `core-amm` covers the math.

**Class B — Solana AMMs.** Raydium CLMM, Orca Whirlpools, Meteora DLMM.
Largest AMM volume by far, but a wholly new stack: non-EVM RPC, different
account model, Jito bundle auctions rather than a gas auction, and DLMM's bin
model is **not** the V3 tick map — `core-amm` does not cover it. Screen it
with Python tooling only; do not plan Rust work until a screen justifies it.

**Class C — orderbook perp DEXs as the *CEX* leg.** Aster, Lighter, Paradex,
dYdX v4, Vertex. Each needs new ingress (the expensive part), but their maker
fee schedules and thinner competition may beat the HL ecosystem. Screen the
**fee schedule and top-of-book depth first** — both are free to obtain via
public API, and depth is what caps size.

### 3.1 TON — probed 2026-09-20, and it fails R1 on the hedge leg

Added at operator request. The on-chain side is healthy; the problem is the
other leg.

**Measured on-chain (all four endpoints reachable from the container:
`toncenter.com/api/v2`, `tonapi.io/v2`, `api.ston.fi/v1`, `api.dedust.io/v2`):**

| | |
|---|---|
| masterchain block time | **0.404 / 0.400 / 0.433 s** across three intervals, from block `gen_utime` (not seqno polling) |
| DeDust fee tiers | **52,290 of 52,330 pools at 0.25 %**; 18 at 0.05 %, 16 at 1 %, 4 at 0.5 %, 2 at 0.1 % |
| STON.fi fee tiers | **47,468 of 48,475 pools at `lp_fee = 20`** (0.20 %), + `protocol_fee = 10` |
| best TON/USDT pool | DeDust, **0.1 % fee**, reserves ≈211,880 TON / 303,557 USDT ≈ **$600 k TVL** — same order as the HyperEVM pool |
| in our `universe.toml` | **no** |

**Measured CEX leg — this is the finding:**

| venue (all ones we already capture) | TON status, 2026-09-20 |
|---|---|
| Hyperliquid | perp exists, **`isDelisted = true`**, `midPx = null`, 24 h volume **$0** |
| OKX | **no TON instrument at all** — spot or swap |
| Bybit | **no TON instrument at all** — spot or linear |
| Binance spot | `TONUSDT` **`status = BREAK`** (halted); bookTicker returns all zeros |
| Binance USD-M | `TONUSDT` **`status = SETTLING`** |
| Deribit | not listed |

**There is no tradeable hedge venue for TON on any venue in our tape.** Four
independent venues agree, and HL's flag is an explicit delisting rather than
a transient state, so this is a withdrawal in progress and not a snapshot
glitch. (`BREAK` alone would be ambiguous; `BREAK` + `SETTLING` + two venues
carrying nothing + an explicit `isDelisted` is not.)

**Verdict: NO-GO at R1, on the hedge leg, before any screen is run.** Not
because the chain is uninteresting — it is live, fast and has real AMM TVL —
but because a CEX/DEX arb needs a CEX, and ours do not have one.

**On the §1 statistic it would also have failed anyway.** *T* is **0.41 s**,
*less than half* HyperEVM's 0.983 s, so σ√T is ≈0.65× — a **smaller**
staleness window. And *c* is worse: the cheapest real TON/USDT pool is 10 bps
against HyperEVM's dominant 5 bps, with the ecosystem default at 20–25 bps.
Both decisive variables point the wrong way, from a chain already measured to
be at the collapse point (vault verdict §4).

**What would reopen it** (in order of likelihood, none cheap):

1. **A venue we capture relists TON.** Then it becomes an ordinary Class D
   screen. Watch it in the monthly R1 re-run (§9) — it is one API call.
2. **Class C first.** A liquid TON book exists on venues we do *not* capture
   (Gate, MEXC, KuCoin, Bitget). Reaching it means new CEX ingress, so TON
   becomes a combined **Class B+C** target: a new non-EVM chain *and* a new
   CEX venue — the most expensive combination in this plan, for a target
   whose §1 statistic already reads worse than the one we just closed.
3. A 1 bp TON pool tier reaching real TVL, which would move *c* enough to
   matter — but only if (1) or (2) has already solved the hedge leg.

### 3.2 Class D — non-EVM, asynchronous (TON), if it is ever reopened

Nothing here is `core-amm`-reusable except the constant-product arm, and
nothing is R0-harness-reusable at all. Budget accordingly.

* **No `eth_getLogs`.** Swap detection means parsing transactions and
  messages against pool contracts via `toncenter` / `tonapi`, or a
  liteserver (ADNL over UDP — binary, not JSON-RPC). The realised-flow screen
  has to be rewritten, not parameterised.
* **Asynchronous, non-atomic** (§1 variable 4). The fill law is not
  `judge_amm`: a swap can land partially. Model the message chain, or do not
  model it at all.
* **Sharded.** The masterchain block time measured above is the *coordination*
  chain. A pool contract lives in a basechain shard, and **the shard's block
  time is the one that governs pool staleness** — measure it directly, do not
  inherit the masterchain number.
* **Jettons, not ERC-20** (TEP-74): balances live in per-owner jetton wallet
  contracts, so "read the pool's token balance" is a different operation.
* AMM shapes: DeDust `volatile` and STON.fi v1 are constant-product →
  `core_amm::AMM_KIND_V2` covers the math. DeDust `stable` and anything
  concentrated is new math.
* Rate limits measured: toncenter free tier throttles at roughly 1 rps and
  returns **HTTP 429** — the screen needs pacing or a key, and `tonapi.io`
  took the load better.

## 4. Phases

### R0 — Turn the one-shots into a screen harness (2–3 d)
Generalise the parent study's scripts into a parameterised research
one-shot (git-excluded — see `docs/arch/research-tools-exclusion-plan.md`
for where such tools live and how they are referred to). Inputs: chain endpoint set, `Swap` topic set, CEX
tape selector, window list. Outputs: the standard report.
Python convention: **full `import x` only, never `from x import y`.**

### R1 — Endpoint + feasibility matrix (1–2 d)
Finish the table in §3 from the Mac (different egress), and for each chain
find and **verify** an archive endpoint with the honesty probe (§5.1). Score
every target on the §1 variables. Kill anything with no honest archive — the
study is not possible without one.

### R2 — Class A screen, ranked order (1 d per chain)
Optimism first, then whichever of Base/Arbitrum/Polygon/Unichain has a working
archive, then BNB as the §1 falsification test. Reuse the six-window method
verbatim so results are comparable across chains and against HyperEVM.

### R3 — Class C fee/depth desk check (1–2 d total, no build)
For each orderbook perp DEX: maker/taker schedule, rebate tiers, top-of-book
depth on the coins that have deep AMM pairs, and API/WS quality. A venue that
offers a genuine **maker rebate** is the interesting case — the parent study
found maker-vs-taker to be the single largest lever in the whole model
(magnitude: vault verdict §4.3). Only then decide whether any deserves ingress work.

### R4 — Class B Solana screen (3–4 d)
Non-EVM stack, so budget more. Screen Raydium CLMM and Orca Whirlpools
(tick-map-like, closest to reusable) before Meteora DLMM (bin model, new
math). Measure the Jito bundle auction the way we measured HyperEVM's gas
auction: it decides capital-race vs latency-race.

### R4b — Class D (TON), ONLY if R1 reopens it (5–7 d)
Do not start unless the hedge leg exists (§3.1). New RPC stack, new swap
detection, new non-atomic fill model, new AMM shapes. Everything in §3.2 is
new work; the only reuse is `core_amm`'s constant-product arm.

### R5 — Cross-target synthesis (2 d)
One table: for each target, the r0→r3 ladder, competition concentration,
ordering mechanism, and the §1 statistic. The deliverable is a **ranked
go/no-go**, in the same form as the HyperEVM verdict — including the
expectation that most or all come back NO-GO, which is a valid and cheap
result.

### R6 — Promote at most one target into the engine
Only if R5 clears a pre-committed bar (§7). Reuses `core-amm` and the
`ingress-hyperevm` shape from the implementation plan; a Class A chain should
be a config file plus one ingress impl.

## 5. The traps checklist — mandatory on every target

Each of these silently fabricates P&L. All five were hit in the parent study.

1. **Verify the archive endpoint.** `rpc.hyperliquid.xyz/evm` and
   `rpc.hypurrscan.io` answer historical `eth_call` with **latest** state and
   return `OK`, not an error. Probe: read a known-changing slot at a block
   ≥1,000 back and at head; if equal, the endpoint is lying. **No screen
   starts until its endpoint passes.**
2. **Sign extension.** `liquidityNet` is `int128` inside a 256-bit ABI word.
   Extending from 128 turns negatives into ~2^256; the corrupted map gave
   fabricates P&L by orders of magnitude, silently, with nothing in the
   output looking wrong (vault verdict §3).
3. **Never extrapolate liquidity.** Walk the real tick map and stop at its
   edge. Constant-liquidity sizing invents size that is not there.
4. **Carry your own market impact.** Without it a standing gap is re-harvested
   every block on stale pools.
5. **Perp is not spot.** If the hedge tape is perps and the AMM trades spot,
   de-mean each pool against its own prevailing basis. Before de-basing,
   simulated flow was **3.6 : 1 one-sided**; after, balanced. The skew was
   basis booked as profit.

Plus two from the adversarial pass:

6. **Cost both measurements symmetrically.** The realised-flow screen
   over-counts (share in the vault verdict §4.2) and charges no fees. Costing it on
   the simulation's basis flips its sign (vault verdict §4.2). Never quote
   one as corroborating the other without the same cost basis.
7. **Validate the simulator against reality.** Replay real on-chain swaps;
   median amount error must come out at the pool fee (0.05 % for a 5 bp pool).
   Anything else is a bug, not noise.

## 6. Reuse vs new, per class

| component | Class A (EVM) | Class B (Solana) | Class C (new CEX leg) |
|---|---|---|---|
| CEX tape | **already captured** | already captured | **new ingress** |
| AMM math | **`core-amm` (V3/V2)** | new (CLMM/DLMM) | n/a |
| screen harness | **R0, unchanged** | adapted | unchanged |
| log/state pull | **R0, unchanged** | new (accounts, not logs) | n/a |
| traps checklist | **unchanged** | items 1–2 differ | items 5–7 apply |

Class A is the cheap direction and should be exhausted first.

## 7. Pre-committed decision rule

Set before the numbers arrive, so the bar cannot move to fit them. A target
advances to R6 only if **all** hold, measured on the depth-capped, latency-
and basis-corrected ladder (r3), not the upper bound:

0. **A hedge venue exists on a tape we already capture.** TON failed here
   on 2026-09-20 (§3.1) before any screen ran. This is the cheapest test in
   the list — run it first, on every target, always.
1. **r3 pie ≥ the bar set in the vault verdict §5** — derived as a multiple
   of the HyperEVM taker pie, scaled by a plausible new-entrant share (the
   range is in the verdict too). The bar is stated there in dollars; it is deliberately NOT
   restated here.
2. **Top-5 sender concentration ≤ 60 %**, i.e. not already owned. Measured
   from a receipt sample, never from a block explorer's league table.
3. **Ordering mechanism is latency-decided, not capital-decided**, or the gas
   p99/p50 ratio is under ~10× (HyperEVM's is ~760×).
4. **Median leg is profitable after fees and gas.** HyperEVM's was
   negative — that finding is what closed it (vault verdict §4.5).
5. **An honest archive endpoint exists** and the simulator passes the 0.05 %
   replay gate on that chain.

Fewer than all six ⇒ NO-GO, written up in the vault in the parent study's
format and closed. **A cheap, well-argued negative is the expected and
acceptable outcome** — the parent study cost about a day and saved a
multi-month build.

## 8. Sequencing and effort

```
R0 (harness 2-3d) ──► R1 (matrix 1-2d) ──┬─► R2 Class A  (1d / chain)
                                         ├─► R3 Class C  (1-2d desk)
                                         └─► R4 Class B  (3-4d)
                                                   └────► R5 (2d) ──► R6
```

~2 weeks of screening covers every Class A chain plus the Class C desk check.
Class B is a separate decision. **R3 is the highest expected value per day**:
it is desk work with no build, and it targets the largest measured lever.

## 9. What would make this whole family worth revisiting

* A materially higher vol regime anywhere in the set — the pie scales with σ.
* **A 1 bp pool tier** reaching real TVL on a 2 s chain. This changes *c* more
  than any engineering, and on the §1 statistic it is the single highest-
  leverage thing to watch for.
* An orderbook venue with a real maker rebate and adequate top-of-book depth
  on AMM-matched coins (Class C).
* Any chain moving to a first-come-first-served sequencer, which converts the
  race from capital to latency.

* **A venue we capture relisting an asset whose chain we already screened** —
  TON is the live example: everything on-chain is in place and only the hedge
  leg is missing (§3.1).

Cheap to monitor: a monthly re-run of R1's probe plus a fee-tier scan, plus a
listing check for the assets of any chain parked on the hedge leg, is a
scheduled task, not a project.
