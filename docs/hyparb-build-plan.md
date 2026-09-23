# HYPARB Build Plan (HZ–H9) — slot 0, paper, dual-book hedge, testnet EVM writes

**Status: IN PROGRESS — branch `hyparb` (worktree `~/trading-engine-multivenue-hyparb`,
O-H17). HZ part A done; H1 `core-amm` and H7a `signer-evm` landed — see §16.** Adds a NINTH venue (`VenueId::HyperEvm = 8` per O-H11 — MEXC
takes 7; the first non-orderbook one), a reusable AMM math crate, an AMM fill law, and a
testnet-only EVM write path. Slot 0 changes hands from `strategy-latency-arb`
to `strategy-hyparb`.

Reference precedents throughout: **VRP / O-D1,O-D2** (slot hand-over, crate
unlinked-not-deleted), **BIN15 / O4b** (member shape, artifact-hash boot
tell, the F19 requested-but-absent law), **E6/E7** (the two-switch arming
interlock), and **MEXC / MX0–MX9** (`docs/mexc-ingress-plan.md`) for new-venue
mechanics.

> **Where the numbers live.** This plan carries *venue and engineering* facts
> — endpoints, block times, chain ids, RPC limits, fee schedules, wire
> layouts, signatures. Every *strategy* finding it rests on — measured edge,
> pie sizing, backtest P&L, the go/no-go verdict — stays in the git-excluded
> vault under `docs/research/hyparb/` (vault) per the operator law of 2026-09-02, and is CITED
> here, never restated. The lane was ruled **NO-GO as a standalone lane**;
> see the vault verdict for why, and §0 here for why it is being built anyway.

---

---

## 0. How to use this spec

* Phases run **HZ → H9**. **HZ runs FIRST**, before any crate is written —
  it can close the lane in a day (§3).
* Each phase has **Files**, **Exact work**, **Tests**, **DONE**.
* Anything marked *law* is not negotiable and carries a recorded reason.
* Tree beats spec: where they disagree, stop and report.
* **Never run `cargo` in the Cowork sandbox** — stale fingerprints give false
  greens. Compile and test on the Mac.
* **Never do a git write-op through the Cowork mount** — stale `.git` locks.
  Git write-ops run on the Mac, and only when the operator asks.

---

## 1. Fixed decisions

| id | decision | date |
|---|---|---|
| **O-H1** | `strategy-latency-arb` is UNLINKED from `strategy-set`; the crate STAYS in the repo and workspace. New member written FROM SCRATCH. (Precedent: `strategy-ev`, `strategy-cross-arb`, `strategy-rule-tree`.) | 09-19 |
| **O-H2** | The member takes **slot 0**. Name `hyparb`. | 09-19 |
| **O-H3** | All arbable pools CAPTURED; a configurable subset TRADED. | 09-19 |
| **O-H4** | **Archive-endpoint law.** Historical EVM state only from an endpoint passing the boot archive probe. Failure REFUSES the boot. | 09-19 |
| **O-H5** | **Testnet-only law.** The EVM write path refuses unless `--evm-testnet` AND chain id **998**. Chain id 999 is a boot refusal, enforced in code. | 09-19 |
| **O-H6** | No branches, commits or pushes without an explicit operator ask. | 09-19 |
| **O-H7** | RLP + the EIP-1559 envelope live in a NEW **`crates/signer-evm`**, depending on `signer-eip712` for the secp256k1 primitive. Chain-agnostic, so chain #2 reuses it. `signer-eip712` is NOT renamed and NOT modified. | 09-20 |
| **O-H8** | **Land dark.** hyparb ships in the binary but never enters the live engine's mask until HZ/H8 are green. `~/multivenue/strategy.conf` keeps `STRATEGY=ai+vrp+xsd+bin15`. | 09-20 |
| **O-H9** | The AMM fill law lands in **`core-fill`** and is mirrored in the **offline harness** in the same pass, with `paper_replay_parity` green. Full X1 parity, first pass. | 09-20 |
| **O-H10** | **Dual hedge books.** Ingest HL spot AND perp; the member selects the hedge venue per trade on total cost (fee + prevailing basis − expected funding). | 09-20 |
| **O-H2 re-confirmed** | Slot **0**, re-confirmed after a "slot 1" ask. VRP keeps slot 1 untouched. | 09-23 |
| **O-H11** | **`VenueId::Mexc = 7`, `VenueId::HyperEvm = 8`.** *(Amended same day: MEXC is being built IN PARALLEL, so there is NO reservation — MEXC's own MX2 lands `Mexc = 7` and widens the venue tables to 8; HYPARB merges `main` after MX2 and widens 8 → 9.)* Consequences: `ACTIVATION_NS_DEFAULT` → `[u64; 9]`; every venue-count table (`ModelParams`, `stale_after_ms_defaults`, snapshot, `paper.rs` arrays, `VENUE_LABELS`) → 9; `tradeable_venue_byte` gains **8**; **`exec-router`'s venue mask widens `u8` → `u16` and `EXEC_VENUES` → 16**, because `mask >> (venue & 7)` would silently alias venue 8 onto 0 (Polymarket). Everywhere below that says venue byte 7 for HyperEVM now means **8**. | 09-23 |
| **O-H12** | **H8 hybrid switch.** A third flag permits EXACTLY reads on chain 999 + writes on chain 998 (never the inverse), shouted in caps in the ARMED tell. Without it, §11.4 layer 5 (chain ids must match) stays strict. Testnet sends are a **shadow** of each paper AMM decision; the paper book remains the P&L source. | 09-23 |
| **O-H13** | **HZ gates the mask flip only.** HZ still runs first and is recorded, but H0–H9 are built regardless (supersedes §3's "record that and stop"). As with O-H8, hyparb enters the live mask only after HZ and H8 are green. | 09-23 |
| **O-H14** | **Git:** one commit per phase on branch **`hyparb`** (O-H17), prefix `HYPARB:`, explicit-path staging, gates green first, from the Mac terminal, never pushed. This plan and `docs/cexdex-venue-screen.md` go in with the first HYPARB commit, after a sanitisation grep (no strategy numbers). **Landing on `main` is a plain `git merge hyparb` (never a rebase), run only when the operator names a quiet point** — the MEXC session's checkout must be clean, because the merge rewrites files under it. | 09-23 |
| **O-H15** | **Endpoint:** `rpc.purroofgroup.com` is the sole WS (`eth_subscribe` newHeads + logs) AND archive source for v1, on the free tier. Measured 09-23: it is the only candidate answering a WS upgrade (101); `rpc.hyperliquid.xyz/evm`, `rpc.hypurrscan.io` and the testnet endpoint all return 405. **A probe failure or outage disables the hyparb MEMBER only** (unconfigured, loud tell, counter) and never takes down the rest of the set. This is a deliberate exception to the F19 requested-but-absent law, for this member. | 09-23 |
| **O-H16** | **Ops hands:** the implementing session stops and restarts the launchd engine via `launchctl` for the H3 raw-tap and H8 smokes, announced before and after, verifying `vm_rows_active ≥ 1`. It writes `~/multivenue/universe.toml` / `hyparb.toml` with `.bak` copies. It **never** touches `.env` or `strategy.conf`. | 09-23 |
| **O-H17** | **Isolation from the parallel MEXC lane.** HYPARB is built in a separate worktree `~/trading-engine-multivenue-hyparb` on branch `hyparb` (cut from `90c9c39`), with its own `target/` — never in the main checkout, never touching its index. **Sequencing:** phases disjoint from MEXC's files run first (HZ, H1 `core-amm`, H7 `signer-evm` + `exec-hyperevm` unwired); H0 and H2–H6 start only after MX2 has landed on `main` and been merged into `hyparb`. Conflicts are resolved on `hyparb`, never on `main`. Before any launchd stop/restart for a smoke, check that MEXC is not mid-smoke; in `~/multivenue/universe.toml` touch only `[hyperliquid]`. | 09-23 |
| **O-H18** | **Executor contract.** H7/H8 transactions call a minimal executor contract of OUR OWN, not a pool (an EOA cannot satisfy the V3 swap callback) and not a DEX router: owner-only `swap(pool, zeroForOne, amountSpecified, sqrtPriceLimitX96, minOut)`, the pool callbacks (`uniswapV3SwapCallback`, plus `algebraSwapCallback` per O-H19) paying from the contract's own balance and accepting calls only from the pool it invoked, and a revert when the output is below `minOut` — an unprofitable race costs gas, never inventory. Source + compiled bytecode live in the repo with a reproducibility check; deployment is H8, TESTNET ONLY (O-H5 binds the deployer too). Supersedes spec-gap default G1. | 09-23 |
| **O-H19** | **The 20 pools without a Uniswap `slot0()` are SUPPORTED in H3**, each family behind its own decoder AND its own replay gate like H1's. Measured 09-23: **8 pools** (one factory, `0x32b9…24c2`) answer a **6-word `slot0`** (Slipstream-style: no `feeProtocol`; the price/tick words are Uniswap's); **12 pools** (two factories, `0xf77b…b1f3` ×7, `0x5f95…61a7` ×5) are **Algebra-Integral-style** — `globalState()` (6 words), `plugin()`, `safelyGetStateOfAMM()`, no `slot0`. Algebra walks its tick TREE (no bitmap-word boundaries) and its plugin can set the fee per swap, so `core-amm` gains an Algebra step mode, proven by that family's own bit-exact replay before any of its pools is traded. | 09-23 |

**Spec-gap defaults (09-23, proposed and not objected to):** (G1) SUPERSEDED by
O-H18 — an EOA cannot call a pool's `swap()`; (G2) `gas.rs` bids a priority fee
equal to a fixed fraction of the expected net edge, capped by a p99 key in
`hyparb.toml`; (G3) each pool is registered as a `SymbolId`, so `Order.sym`
carries the pool and audit/per-sym P&L stay sound — `sym` is never overloaded
with a venue-local index.

### Why O-H10, in numbers (measured 2026-09-20)

| | perp | spot |
|---|---|---|
| taker rate (base tier) | `userCrossRate` **0.00045** | `userSpotCrossRate` **0.0007** (1.56×) |
| at the backtest's platinum-tier-4 assumption | ~2 bps | ~3.1 bps |
| funding | **earned** (material — see vault verdict §2) | none |
| basis carry | yes (mean-reverting; magnitude in the vault verdict §4.3) | none |
| our tape | 12 h+ | **none yet** |
| top-of-book (HYPE, snapshot) | $102 / $15,021 | $17,049 / $158 |
| spread (snapshot) | 0.2 bps | 0.1 bps |

The spot/perp fee gap is a material fraction of the edge this lane is
chasing (magnitude in the vault verdict §4.3), so the hedge venue is not a
detail and hard-coding either one bakes in an assumption. Selecting per
trade makes it a **measured output** and adds a sixth disputed quantity
(§10).

**Note on the HL spot API:** `spotMetaAndAssetCtxs` returns `universe` len
329 against `ctxs` len 868 — **they are NOT index-aligned.** Reading them
zipped gives nonsense (HYPE/USDC "mid" 0.082128, `dayNtlVlm` 0.0). Resolve
spot state from `l2Book` per pair, or align by `universe[i].name`.

---

## 2. Identifier registry — everything that must agree

| identifier | value | where |
|---|---|---|
| `VenueId::HyperEvm` | **7** | `core-types` — next free (0 Polymarket … 6 Bybit; 255 reserved). Append only. |
| `EXEC_VENUES` | 8 (unchanged) | `exec-router/src/route.rs:61` — venue 7 fits. **7 EXHAUSTS the byte** (`mask >> (venue & 7)`); a ninth venue needs the type widened. |
| `ACTIVATION_NS_DEFAULT` | `[u64; 7]` → **`[u64; 8]`** | `core-fill/src/lib.rs:89`. **The matcher rejects any venue byte ≥ len as `unroutable`**, so venue 7 is unreachable until this is widened. Pin test `the_activation_table_is_the_measured_one` (:488) asserts the literal — update both. |
| `tradeable_venue_byte` | `venue <= 4 \|\| venue == 6` → add 7 | `cli/src/backtest/fill.rs:115` |
| `ORDER_KIND_AMM_SWAP` | **2** | `core-fill` — next free (0 MAKER, 1 IOC) |
| `SLOT_HYPARB` / `BIT_HYPARB` | `0` / `1 << 0` | `strategy-set` (replace `SLOT_LATENCY_ARB` / `BIT_LATENCY_ARB`) |
| `BUILT_MASK` | unchanged `127` | `BIT_HYPARB` replaces `BIT_LATENCY_ARB` in the expression |
| mask names | `hyparb`, `ai+hyparb`, `ai+vrp+xsd+bin15+hyparb` | `MASK_TABLE` **and** `STRATEGY_SET_NAMES` **and** the wrapper `case` — all three or the boot refuses |
| `SignalSource::HyperEvm` | **5** | `core-types` — next free. Append only. |
| slot-0 display name | `"hyparb"` | `exec_boot.rs` `SLOT_NAMES[0]` · `engine-snapshot/src/snapshot.rs:48` · `audit_pnl.rs:137` |
| ingress metric prefix | `engine_ingress_hyperevm_*` | `register_ingress_counters(&mut reg, "hyperevm")` |
| member metric prefix | `engine_hyparb_*` | `register_hyparb_metrics` |
| capture label | `"hyperevm"` | `PmlrCapture::open(run_dir, "hyperevm", …)` |
| config artifact | `~/multivenue/hyparb.toml` | contract: `hyparb.toml.example` at repo root |
| chain id testnet / mainnet | **998** / 999 (refused) | `signer-evm`, `exec-hyperevm` |

### HL spot symbols to append (O-H10)

`universe.toml [hyperliquid] coins` — **APPEND ONLY.** A `SymbolId` is a
file-order ordinal; reordering silently repoints every historical sym.

| pair | HL name | tokens | current spread | note |
|---|---|---|---|---|
| HYPE/USDC | `@107` | `[150, 0]` | 0.1 bps | hedges WHYPE |
| UBTC/USDC | `@142` | `[197, 0]` | 0.1 bps | hedges UBTC |
| UETH/USDC | `@151` | `[221, 0]` | 0.4 bps | hedges UETH |
| USOL/USDC | `@156` | `[254, 0]` | 0.9 bps | hedges USOL |

**Coin-table budget (verified):** `HL_MAX_COINS = 32`,
`CHANNELS_PER_COIN = 4`, `MAX_SUBS = 4×32+2 = 130`. Current usage is
8 coins + 2×8 families = **24 of 32**. Four spot appends → **28 of 32**,
four spare. Spot rows subscribe `bbo` + `l2Book` + `trades` (no
`activeAssetCtx` — spot has no funding/premium), so +12 subscriptions.
HL is one connection, so there is no fd-budget interaction.

---

## 3. HZ — the latency budget. RUN THIS FIRST.

Before any crate. One day, a throwaway script, not engine code.

> From `newHeads` for block *N* arriving, to our signed transaction ACKed by
> the RPC — **p50 and p99** — against the **0.983 s** block budget.

Per the standing law, venue latency is **measured per host AND per location**
(`python -m claude_worker.latency_probe` → `docs/venue-latency.md` → the
harness Δ table). Measure from the MacBook first; if marginal, repeat from a
colocated box. Treat colocation as a precondition, not an optimisation.

**If p99 does not fit inside one block from a plausible deployment, the lane
is closed on mechanics alone.** Record that and stop. Everything below is
wasted otherwise.

---

## 4. H0 — unlink slot 0, reserve the name, land dark

**Law (O-H1).** Unlink, do not delete.

### 4.1 `crates/strategy-set/src/lib.rs` — 29 non-test sites
Rename `latency_arb`/`LatencyArb`/`SLOT_LATENCY_ARB`/`BIT_LATENCY_ARB`/
`SET_LATENCY_ARB_SLOTS` → the `hyparb` equivalents at:
module-doc slot table (≈19) · `use` (≈105) · `SLOT_*` (≈113) · `BIT_*`
(≈160) · `BUILT_MASK` (≈179) · `SET_*_SLOTS` (≈182) · `MASK_TABLE` (≈198) ·
struct field (≈266) · `new()` init (≈317) · `set_regime_label` (≈373) ·
`pull_regime_labels` (≈414) · `deliver_gate` (≈484) · `*_mut()` (≈528) ·
`route_fill_to_slot` (≈538) · `enable_slot` (≈645) · `orders_emitted` (≈686)
· `orders_dropped` (≈696) · `slot_counters` (≈867) · `on_start` (≈978) ·
`on_tick` (≈1011) · `on_signal` (≈1043) · `on_venue_event` (≈1088) ·
`on_depth` (≈1122) · `on_opt_summary` (≈1156) · `on_fill` (≈1213) · `on_ai`
(≈1291) · `on_timer` (≈1343) · `timer_period_ns` (≈1384) · `on_stop` (≈1419).
Plus `Cargo.toml:20` and tests ≈1472–2201.

### 4.2 THE ASYMMETRY — slot 0 is unlike slots 1/2/3
`latency-arb` is the **only** mask name with its own non-set boot arm. The
cross-arb/rule-tree precedent does not cover this.

* `multivenue-engine.rs:702` — `#[arg(long, default_value = "latency-arb")]`
  → **change the default to `"ai"`**, or a bare `run` refuses.
* `:3455` `("latency-arb", true)` and `:3463` `("latency-arb", false)` —
  **delete both.** `hyparb` is a set member and gets no standalone arm.
* `:4018` `const EXEMPT: &[&str] = &["latency-arb"];` → **`&[]`**, and add
  the three new names to `STRATEGY_SET_NAMES` (`:43-71`). The pin tests at
  `:4028`, `:4044`, `:4059-4094` then enforce `STRATEGY_SET_NAMES ⇄
  MASK_TABLE` equality for free.

### 4.3 Out-of-crate slot-0 name sites
`cli/src/exec_boot.rs:104` · `engine-snapshot/src/snapshot.rs:48` ·
`cli/src/audit_pnl.rs:137` · `cli/src/paper.rs` ≈65, 2560, 2595, 2618
(`configure_latency_arb`), 2793, 2816, 3173, 3224, 3306, 3516, 3785, 6471,
6728 (`engine_strategy_latency_arb_active` gauge), 9969 ·
`cli/Cargo.toml:37` · `bench/Cargo.toml:46` · `core-io/Cargo.toml:22`.
Tests: `bench/tests/alloc_assertions.rs:730-819` and `:2693-2859` ·
`bench/benches/hot_path.rs:29,107,180,197-229,279` ·
`core-io/tests/pmlr_replay.rs:8,20,23,108,124` ·
`cli/tests/audit_pnl.rs:191,333`. Add a row to `docs/migration.md`.

### 4.4 Land dark (O-H8)
The wrapper allow-list gains the three names **so they CAN be set later**,
but `~/multivenue/strategy.conf` is **not** edited in this phase and stays
`STRATEGY=ai+vrp+xsd+bin15`. The live engine — armed on mainnet slot 3 —
never loads hyparb until HZ and H8 are green. Nothing in H0–H7 restarts it
beyond the normal daily-restart cadence.

**DONE(H0):** workspace builds; nextest green with a stub `HyparbStrategy`;
`--strategy hyparb` resolves; `--strategy latency-arb` refused;
`strategy-latency-arb` still builds standalone; live `strategy.conf`
unchanged.

---

## 5. H1 — `crates/core-amm`, the reusable AMM engine

Pure math. No network, no config, no I/O. This is what makes chain #2 cheap.

### 5.1 Cargo.toml
```toml
[package]
name = "core-amm"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true
authors.workspace = true
description = "Concentrated- and constant-product AMM swap simulation: tick-crossing exact-input walk, arb sizing solve. Zero-alloc, no dyn, integer-first."

[lib]
path = "src/lib.rs"

[dependencies]
core-types = { workspace = true }

[dev-dependencies]
proptest = { workspace = true }
```
Standard lint header:
```rust
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs, unused_imports, unused_must_use, unreachable_pub,
        clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]
```
Add to `[workspace] members` under `# --- core primitives ---` and to
`[workspace.dependencies]`.

### 5.2 Types
```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct PoolState {
    pub sqrt_price_lo: u128,   // sqrt(P)*2^96, low 128 bits
    pub liquidity: u128,
    pub sqrt_price_hi: u32,    // uint160 high word; 0 in practice, carried so a
                               // venue change cannot silently truncate
    pub tick: i32,
    pub block: u64,
    pub log_index: u32,
    pub pool: u16,
    pub flags: u8,             // POOL_FLAG_STALE
    _pad0: [u8; 1],
}
const _: () = assert!(core::mem::size_of::<PoolState>() == 64);
const _: () = assert!(core::mem::align_of::<PoolState>() == 64);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct PoolMeta {
    pub token0: [u8; 20], pub token1: [u8; 20], pub address: [u8; 20],
    pub fee_pips: u32,          // 500 = 0.05 %
    pub tick_spacing: i32,
    pub dec0: u8, pub dec1: u8,
    pub sym0: u16, pub sym1: u16,   // HL symbol table index, u16::MAX = USD stable
    pub kind: u8,                   // AMM_KIND_V3 | AMM_KIND_V2
    _pad: [u8; 3],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(C)]
pub struct TickNode { pub tick: i32, _pad: [u8; 4], pub liquidity_net: i128 }

#[derive(Debug)]
#[repr(C, align(64))]
pub struct TickMap<const N: usize> {
    nodes: [TickNode; N], len: u16,
    pub lo_tick: i32, pub hi_tick: i32,   // the walk STOPS at these
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AmmError { TickMapFull, TickMapUnsorted, TickMapDuplicate, LiquidityNetOverflow, BadState }

pub const AMM_KIND_V3: u8 = 0;
pub const AMM_KIND_V2: u8 = 1;
pub const POOL_FLAG_STALE: u8 = 1;

#[derive(Copy, Clone, Debug, PartialEq, Eq)] #[repr(u8)]
pub enum ArbSide { None = 0, BuyToken0 = 1, SellToken0 = 2 }

#[derive(Copy, Clone, Debug, PartialEq, Eq)] #[repr(C)]
pub struct ArbQuote {
    pub side: ArbSide, _pad: [u8; 3],
    pub gross_bps_1e6: i64,
    pub token0_raw: u128, pub token1_raw: u128,
    pub pnl_usd_1e6: i64, pub notional_usd_1e6: i64,
    pub after: PoolState,      // our own impact — the caller MUST carry this
}
```

### 5.3 Functions
```rust
/// Walk the tick map from `state` toward the target, exact-input.
/// Returns (token0_raw, token1_raw, state_after) PRE-fee. Upward: token0 is
/// OUTPUT, token1 INPUT. Downward: the reverse.
///
/// **Law — never extrapolate.** Clamps the target to the fetched map's
/// [lo_tick, hi_tick] and stops. Liquidity beyond the map is unknown and
/// guessing it manufactures size that is not on chain.
pub fn swap_to_target<const N: usize>(
    state: &PoolState, meta: &PoolMeta, map: &TickMap<N>,
    sqrt_target_lo: u128, sqrt_target_hi: u32,
    max_token0_raw: u128, up: bool,
) -> (u128, u128, PoolState);

/// Profit-maximising single-block arb. `eff_bid`/`eff_ask` price token0 in
/// token1 ×1e18 with the HEDGE venue's taker fees ALREADY folded in by the
/// caller. Only the pool fee is applied here. `max_notional_usd_1e6` is the
/// caller's cap AFTER the hedge venue's top-of-book has been applied.
pub fn solve_arb<const N: usize>(
    state: &PoolState, meta: &PoolMeta, map: &TickMap<N>,
    eff_bid_1e18: u128, eff_ask_1e18: u128,
    px0_usd_1e6: i64, max_notional_usd_1e6: i64, gas_usd_1e6: i64,
) -> ArbQuote;

/// Exact-input swap WITHIN the active tick range only (constant L).
/// Used by the paper matcher, which has no tick map (§6.3).
pub fn swap_in_range(
    state: &PoolState, meta: &PoolMeta,
    sqrt_target_lo: u128, sqrt_target_hi: u32,
    max_token0_raw: u128, up: bool,
) -> (u128, u128, PoolState);

#[must_use] pub fn sqrt_at_tick(tick: i32) -> (u128, u32);
#[must_use] pub fn tick_at_sqrt(lo: u128, hi: u32) -> i32;
#[must_use] pub fn price_1e18_from_sqrt(lo: u128, hi: u32, dec0: u8, dec1: u8) -> u128;
#[must_use] pub fn sqrt_from_price_1e18(px_1e18: u128, dec0: u8, dec1: u8) -> (u128, u32);
#[must_use] pub fn range_bounds(tick: i32, tick_spacing: i32) -> (i32, i32);
```

### 5.4 Implementation laws
1. **`liquidityNet` sign-extends from 256 bits, not 128.** It is `int128`
   ABI-encoded into a 256-bit word. Extending from 128 turns every negative
   tick into ≈2^256. The error is silent and the magnitude is
   catastrophic (quantified in the vault verdict §3). **52.8 % of real ticks
   are negative**, so it is not an edge case.
   Decoding is H3's job; the invariant is asserted here — `TickMap::load`
   returns `LiquidityNetOverflow` on `|liquidity_net| > 2^127`.
2. **No iterators on the walk.** `while` + raw index; `get_unchecked` only
   inside a safe wrapper carrying `// SAFETY:`.
3. **No floats.** `sqrt_at_tick` is the integer tick→sqrt ladder (the 19
   magic constants), not `exp()`. A float path will not reproduce on-chain
   amounts to the 0.05 % bar.
4. Clamp liquidity at zero when a downward cross would take it negative.
5. Saturating arithmetic on every USD conversion; `i128` intermediate,
   narrowed with a saturating cast (the `exec-router` precedent).

### 5.5 Tests
* `tests/replay.rs` — **THE GATE.** Replay ≥5,000 real mainnet Swap events
  (fixture TSV under `tests/data/`, cut from the research tape:
  `pre_sqrt, pre_liq, pre_tick, post_sqrt, amount0, amount1, fee_pips`).
  Feed each swap's own post-price back as the target; compare amounts.
  **Median relative error ≤ 0.05 %** — exactly the pool fee, because
  `swap_to_target` returns pre-fee amounts. The Python reference hit exactly
  0.050 % on this fixture; a Rust port that does worse has a bug.
* `tests/proptest.rs` — up-then-down returns within one tick; amounts
  monotone in target distance; a clamped target never yields more than an
  unclamped one; `solve_arb` never returns positive pnl with `side == None`;
  `swap_in_range` never exceeds `swap_to_target` for the same input.
* `fuzz/fuzz_targets/amm_tick_walk.rs` — never panics, never returns absurd
  amounts.
* Zero-alloc: add a `solve_arb` case to `bench/tests/alloc_assertions.rs` at
  **0 B/op**. `--test-threads=1` (the allocator is process-global); the log
  must show a fresh `Compiling bench` or `cargo clean -p bench --release`.

**DONE(H1):** replay gate ≤0.05 % median · proptest + fuzz 300 s clean ·
alloc 0 B/op · `make lint` · `make copy-audit` new=0 · SPDX + `license.workspace`.

---

## 6. H2 — the AMM fill law: `core-fill` + paper matcher + harness (O-H9)

The largest new phase, and the one with the most existing invariants to
respect. Read §6.1 before writing anything.

### 6.1 What the survey found (all verified 2026-09-19/20)

1. **`PaperMatcher` keeps NO per-symbol state.** `observe_tick` judges
   against the tick it is handed and discards it. There is no `[Tick; N]`
   cache anywhere. An AMM fill is deterministic *given pool state*, so pool
   state needs a home that does not exist yet.
2. **`PaperMatcher::new()` and `PaperDispatcher::new()` are both `const fn`**
   and the matcher is inline (~8 KiB) inside the dispatcher. Any new store
   must be const-constructible and small.
3. **`Pending` is size-asserted at exactly 64 bytes** with `_pad: [u8; 7]`.
   A per-order pool handle must fit in ≤7 bytes.
4. **`Touch` is four `i64`s** and cannot express a curve. The AMM judge needs
   its own POD.
5. **`ACTIVATION_NS_DEFAULT` is `[u64; 7]`** and `PaperMatcher::submit`
   rejects `venue as usize >= ACTIVATION_NS_DEFAULT.len()` as `unroutable`.
   **Venue 7 is unreachable today.**
6. **X1 parity**: the law must also land in `cli/src/backtest/fill.rs`
   (≈1336–1397), pinned by `clob-dispatcher/tests/paper_replay_parity.rs`.
7. **No fees exist on any paper fill.** `core_types::Fill` has no fee field;
   `core-fill`'s own doc says it "knows nothing about fees". Fees live only
   in the harness (`ModelParams`, `parse_model_params`, `--fee-bps`), read
   from `fees.toml` by the nightly Python lane, never by Rust.

### 6.2 Decisions that follow from §6.1 (do not re-litigate)

* **The pool fee rides in the fill PRICE.** An AMM's fee is part of the
  execution price on chain, not a charge on top — so folding it into
  `px_1e6` is both correct and the only option that leaves `Fill` and the
  wire format untouched.
* **The HL hedge leg's fees are applied OFFLINE** by the nightly
  `pnl_report` from `fees.toml`, exactly as BIN15 does. `fees.toml` gains a
  `hyperevm` venue block and HL `spot` rows. No change to `Fill`.
* **The matcher does NOT carry a tick map.** 128 pools × a 1024-node map is
  ~2 MB — it cannot live inline in a `const fn`-constructed matcher. The
  matcher judges with **`core_amm::swap_in_range`** (constant liquidity
  within the active tick range). The member does the full map walk when it
  sizes. This is **conservative by construction**: a fill that would cross a
  tick is capped at the range boundary, so the matcher can only ever fill
  LESS than the chain would, never more. Measured pool response: a $20 k swap moves the
  deepest HYPE pool ~5.4 bps, about half a tick spacing, so the common case
  is exact and the approximation only bites on the rare large trade — in the
  safe direction. **State this in the fn's doc comment.**

### 6.3 `crates/core-fill` additions

`Cargo.toml` gains `core-amm = { workspace = true }`. No cycle: `core-amm`
depends only on `core-types`. `core-fill` keeps `#![forbid(unsafe_code)]` —
it only *calls* `core-amm`.

```rust
/// Order kinds. 0 maker, 1 IoC, 2 AMM swap.
pub const ORDER_KIND_AMM_SWAP: u8 = 2;

/// The AMM-side market state one swap is judged against.
///
/// `Touch` carries four `i64`s and cannot express a curve, so the AMM judge
/// takes its own POD. Exactly 64 bytes, one cache line.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolTouch {
    pub sqrt_price_lo: u128,
    pub liquidity: u128,
    pub sqrt_price_hi: u32,
    pub tick: i32,
    pub fee_pips: u32,
    pub tick_spacing: i32,
    pub pool: u16,
    pub dec0: u8,
    pub dec1: u8,
    pub flags: u8,          // POOL_FLAG_STALE
    _pad: [u8; 3],
}
const _: () = assert!(core::mem::size_of::<PoolTouch>() == 64);

impl PoolTouch {
    #[inline] pub const fn is_stale(&self) -> bool;
    #[inline] pub const fn is_live(&self) -> bool;   // liquidity > 0 && sqrt > 0 && !stale
}

/// Judge one AMM swap against current pool state.
///
/// Deterministic given the pool: an exact-input swap executes at the curve's
/// price or not at all. The POOL FEE IS PRICED INTO the returned `px_1e6` —
/// `Fill` has no fee field and this crate knows nothing about fees, and an
/// AMM's fee is part of the execution price on chain anyway.
///
/// Walks the ACTIVE TICK RANGE ONLY (constant liquidity). A swap that would
/// cross a tick is capped at the range boundary, so this can only ever fill
/// LESS than the chain would — never more. The full tick-map walk lives in
/// `core_amm::swap_to_target` and is used for sizing, not for judging.
pub fn judge_amm(
    side: Side,
    px_limit_1e6: i64,
    remaining_1e6: i64,
    pool: PoolTouch,
) -> Verdict;
```
`judge_amm` returns `Verdict::Fill { px_1e6, qty_1e6 }` when the curve's
average execution price for `remaining_1e6` is at or better than
`px_limit_1e6`, partial-filled at the range boundary if the full size would
cross a tick; `Verdict::Cancel` otherwise (an AMM swap never rests, so it
**never returns `Wait`** — the same contract `judge_ioc` has).

Widen the activation table and its pin test:
```rust
pub const ACTIVATION_NS_DEFAULT: [u64; 8] = [
    200 * MS,  // pm
    130 * MS,  // bn
    130 * MS,  // okx
    220 * MS,  // deribit
    340 * MS,  // hl
    0,         // ai (dead)
    60 * MS,   // bybit
    1_000 * MS, // hyperevm — ONE BLOCK. Measured 0.983 s, rounded UP.
];
```
**Law:** the HyperEVM Δ is one block, not a network RTT. A transaction
cannot land sooner than the next block however fast the wire is, and
modelling it as an RTT is the single easiest way to invent edge. Update
`the_activation_table_is_the_measured_one` (≈:488) to the 8-element literal.

### 6.4 `crates/clob-dispatcher` — the matcher arm

**New defaulted trait method** on `OrderDispatch`, mirroring `observe_tick`'s
shape and its ordering contract (the dispatcher sees it before the strategy):
```rust
/// Pool state for an AMM venue. Defaulted to nothing, because a live
/// dispatcher learns about fills from the chain and has no business
/// inventing them from a curve. Only `PaperDispatcher` overrides it.
fn observe_pool(&mut self, _pool: &core_fill::PoolTouch, _now_ns: NsTs) {}
```
`RoutedDispatcher` forwards it to the paper arm only, exactly as it does
`observe_tick` (`routed.rs:1065`).

`PaperMatcher` gains:
```rust
pools: [core_fill::PoolTouch; MAX_POOLS],   // MAX_POOLS = 64; 4 KiB, const-constructible
```
`Pending` uses two of its seven spare bytes for `pool: u16` (index into
`pools`, `u16::MAX` = not an AMM order). The 64-byte assertion must still
hold — re-run it.

`observe_pool(pool, now)` stores by `pool.pool` index, then judges every open
order carrying that pool index via `judge_amm`, in emit order, exactly as
`observe_tick` does for a sym. TTL sweep first (expiry is a clock fact),
then the fill pass. Fills go through the existing `push_fill`, so
`FILL_ORIGIN_PAPER` and attribution are unchanged.

`PaperMatcher::submit` gate: accept `ORDER_KIND_AMM_SWAP` alongside
MAKER/IOC, and accept venue byte 7 now that the activation table is 8 long.

New counters on `MatcherCounters`: `amm_fills`, `amm_canceled`,
`amm_range_capped`, `amm_stale_pool`.

### 6.5 The harness mirror (X1 parity — the half that is easy to forget)

`cli/src/backtest/fill.rs`:
* `tradeable_venue_byte(venue) = venue <= 4 || venue == 6` → **add 7**.
* Mirror the `judge_amm` arm into `FillEngine` beside the IoC/maker arms
  (≈1336–1397), routing through the **same** `core_fill::judge_amm` — the
  crate's doctrine is "the law lives here once, both consumers call it".
* `ModelParams` fee tables are `[…; 7]` venue-dimensioned → **widen to 8**
  (`fee_bps`, `fee_open_bps`, `fee_settle_bps`, `latency_ns`,
  `stale_after_ms`, `opt_fee`). `parse_model_params` gains the `hyperevm`
  venue label.
* Pool state must reach the harness from the captured tape — it replays the
  `Signal`s the ingress wrote (§7.5), decoding the same payload.
* Extend `clob-dispatcher/tests/paper_replay_parity.rs` to cover an AMM
  order: engine and harness must produce byte-identical fills.

### 6.6 `crates/engine` — the inbound path
In the signal drain (beside the tick drain at `engine/src/lib.rs:451`), a
`Signal` with `source == SignalSource::HyperEvm` is decoded into a
`PoolTouch` and handed to `self.disp.observe_pool(&pt, now)` **before**
`strategy.on_signal(...)`, mirroring the tick ordering. Pin it with a test,
as the tick ordering is pinned.

**DONE(H2):** `judge_amm` unit tests (fill / partial at range boundary /
cancel on limit / refuse on stale pool) · activation pin test updated ·
`Pending` still 64 bytes · `paper_replay_parity` green including an AMM
order · alloc 0 B/op on `observe_pool` · harness replays an AMM fill
identically to the engine.

---

## 7. H3 — `crates/ingress-hyperevm` + the HL spot appends

### 7.1 Crate
Template: **`crates/ingress-rpc`** — copy its shape exactly. Deps identical
to `ingress-rpc`'s set plus `core-amm`. Files: `lib.rs` (re-exports + hex
scanners + Swap decode), `run_loop.rs`, `discovery.rs`. Add to
`[workspace] members` under the ingress-adapters comment.

`run_loop.rs` mirrors `ingress-rpc` exactly: same `State`, `RunResult`,
`StopFlag`, `Driver` (keep `_not_sync: PhantomData<UnsafeCell<()>>`),
`drive_one`, `run` (12 args), `note_transport_ready`. Constants:
```rust
pub const RX_BUF_SIZE: usize = 256 * 1024;   // logs frames dwarf newHeads
pub const TX_BUF_SIZE: usize = 8 * 1024;     // the subscribe frame carries N addresses
pub const DEFAULT_POOL_RING_CAP: usize = 4096;
pub const PENDING_CAP: usize = 64;           // power of two
pub const SUB_CAP: usize = 4;
pub const RPC_POLL_NS: u64 = 2_000_000_000;

#[repr(u8)] pub enum RpcKind { BlockNumber = 0, SubscribeNewHeads = 1, SubscribeLogs = 2, None = 255 }
#[repr(u8)] pub enum SubKind { NewHeads = 0, Logs = 1, None = 255 }
```
**Copy the two-phase borrow in `handle_json_frame` verbatim** — phase 1 takes
an immutable borrow of `drv.rx.filled()[range]`, calls
`capture.raw_frame(now_ns(), payload)`, classifies and pre-parses into a
`Copy` enum; phase 2 drops the borrow and mutates. `Range<usize>` is not
`Copy`; clone any range needed in phase 2 up front. This is what makes the
decode zero-copy.

### 7.2 Subscribe serializer (new, `ingress-rpc` style)
```rust
/// {"jsonrpc":"2.0","id":N,"method":"eth_subscribe","params":["logs",
///  {"address":["0x…",…],"topics":["0xc42079…"]}]}
/// Rendered in place. Addresses are pre-formatted 42-byte `0x…` ASCII
/// literals held in the pool table — formatted ONCE at boot, never per frame.
pub fn write_request_subscribe_logs(
    dst: &mut [u8], id: u64, addresses: &[[u8; 42]], topic0: &[u8; 66],
) -> Result<usize, RpcWriteErr>;
```
V3 Swap topic0:
`0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67`.

### 7.3 Hex scanners (new — nothing in the tree covers this)
**`core-parse` has NO hex helper.** The only public `0x`-hex scanner is
`ingress_rpc::parse_hex_u64`, capped at 16 digits (u64) — useless for a
32-byte word. Add to `ingress-hyperevm/src/lib.rs` (lift into `core-parse`
when chain #2 wants them):
```rust
#[inline] pub fn hex_bytes<const N: usize>(buf: &[u8], pos: Pos) -> Option<([u8; N], Pos)>;
#[inline] pub fn word_u128(buf: &[u8], word_off: Pos) -> Option<u128>;
/// A 32-byte word as i128, TWO'S-COMPLEMENT FROM 256 BITS.
/// See §5.4 law 1 — extending from 128 fabricates P&L silently.
#[inline] pub fn word_i128(buf: &[u8], word_off: Pos) -> Option<i128>;
#[inline] pub fn word_u160(buf: &[u8], word_off: Pos) -> Option<(u128, u32)>;
```
Swap `data` is one `0x`-prefixed string of 5×64 hex chars:
`amount0 int256 | amount1 int256 | sqrtPriceX96 uint160 | liquidity uint128 |
tick int24`. Slice by word offset; decode only the low bytes each field needs.

### 7.4 `discovery.rs` + the archive probe
Model on `ingress-hyperliquid/src/discovery.rs`: **no network code in the
module** — the cli fetches via `core_net::boot_http::https_post` and hands
bodies in. Boot-time allocation is permitted; the table is dropped before
the engine loop starts.
```rust
pub const HYPEREVM_MAX_POOLS: usize = 128;
pub const HYPEREVM_TICKS_PER_POOL: usize = 1024;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HyperEvmDiscoveryErr { Envelope, BadRow, Truncated, TooMany, ArchiveDishonest }

impl HyperEvmDiscovery {
    pub fn new() -> Self;
    pub fn ingest_pool_meta(&mut self, body: &[u8]) -> Result<u32, HyperEvmDiscoveryErr>;
    pub fn ingest_slot0(&mut self, pool: usize, body: &[u8]) -> Result<(), HyperEvmDiscoveryErr>;
    pub fn ingest_tick_bitmap(&mut self, pool: usize, word: i16, body: &[u8]) -> Result<u32, HyperEvmDiscoveryErr>;
    pub fn ingest_tick(&mut self, pool: usize, tick: i32, body: &[u8]) -> Result<(), HyperEvmDiscoveryErr>;

    /// **O-H4 — a BOOT GATE, not a warning.** Read one known-changing slot
    /// at `head` and at `head - 1000`. Equal ⇒ the endpoint answers
    /// historical `eth_call` with LATEST state and every seeded tick map is
    /// silently wrong.
    ///
    /// Measured 2026-09-19: `rpc.hyperliquid.xyz/evm` and
    /// `rpc.hypurrscan.io` BOTH fail this and return `OK`, not an error.
    /// Only `rpc.purroofgroup.com` passed. Logs and headers are fine on all.
    pub fn archive_probe(head_body: &[u8], old_body: &[u8]) -> Result<(), HyperEvmDiscoveryErr>;
}
```
Selectors: `slot0 0x3850c7bd` · `liquidity 0x1a686502` ·
`tickSpacing 0xd0c93a7c` · `fee 0xddca3f43` · `token0 0x0dfe1681` ·
`token1 0xd21220a7` · `tickBitmap 0x5339c296` · `ticks 0xf30dba93` ·
`decimals 0x313ce567`.

**Measured RPC limits — encode as constants:** JSON-RPC batch cap **20** ·
`eth_getLogs` block range cap **1000** · address arrays and topic OR-arrays
both accepted.

### 7.5 Publishing to the engine
**Do not add a PMLR slot kind.** `SlotKind::Swap = 8` costs seven separate
edits across `core-types`, `core-io`, the `Capture` trait, `PmlrCapture`,
`GaugedCapture`, `docs/wire-format.md` and five readers including Python.
Instead emit **`Signal`** (kind 1) with `source = SignalSource::HyperEvm`
(new variant, value 5 — append only) and `sym = SYMBOL_ID_NONE`, packing the
pool state into the opaque 40-byte payload exactly as `ingress-rpc` packs
`NewHead`:

`pool u16 @0 · flags u8 @2 · dec0 u8 @3 · block u32 @4 · sqrt_lo u128 @8 ·
sqrt_hi u32 @24 · tick i32 @28 · liquidity_lo u64 @32`

The high 64 bits of `liquidity` are 0 for every pool in the universe —
**assert it and count a `liquidity_truncated` refusal** if a venue change
ever breaks that. Document the layout under the existing Signal payload
table in `docs/wire-format.md`, not as a new slot-kind section.

**Capture BEFORE push** (the §6.5 law):
```rust
capture.signal(&sig);
if producer.try_push(sig).is_err() { status.inc_ring_drops(); }
```
**Capture volume:** publish per SWAP (event-driven), never per block. Per-swap
is ~1.2/s across the universe ≈ 7 MB/day. Per-block across 128 pools would be
~708 MB/day against a Data volume already at 94 %.

### 7.6 Wiring
`paper.rs`: `use ingress_hyperevm::run_loop as ewl;` and a `spawn_hyperevm`
that is `spawn_rpc` (≈:1975-2069) with label `"hyperevm"` — plus, unlike
rpc, a tap venue byte, because this source HAS a `VenueId`:
```rust
if tap_cfg.mode != TapMode::Off {
    capture.set_tap_venue_byte(run_dir, "hyperevm", VenueId::HyperEvm.to_u8())?;
}
```
Rings: `pub hyperevm_signal: Arc<Ring<Signal, SIGNAL_RING_SIZE>>` (≈:529) +
`Ring::new()` (≈:577); a `Consumer` in `Consumers` (≈:2425) drained beside
`cons.rpc_signal.try_pop()` (≈:2518); `.split()` in `multivenue-engine.rs`
(≈:2585). **If not spawned, `drop(prod)`** so the lane stays a
permanently-empty ring — see the Bybit `else` arm.
Metrics: `let ingress_hyperevm = register_ingress_counters(&mut reg, "hyperevm")?;`

### 7.7 HL spot appends (O-H10)
`~/multivenue/universe.toml` `[hyperliquid] coins` — **APPEND ONLY**, after
the existing 8 perps and before nothing:
```toml
coins = ["BTC", "ETH", "SOL", "XRP", "DOGE", "ADA", "LTC", "HYPE",
         "@107", "@142", "@151", "@156"]
```
`@107` HYPE/USDC · `@142` UBTC/USDC · `@151` UETH/USDC · `@156` USOL/USDC.
Budget: 8 + 4 + 2×8 families = **28 of 32**. Spot rows carry no
`activeAssetCtx`, so +12 subscriptions of 130.

**Law:** a `SymbolId` is a file-order ordinal. Appending is safe; reordering
silently repoints every historical sym in the tape. After the restart run
`claude-worker fetch` once; `unresolved=0` is the done-tell.

`fees.toml` gains HL `spot` rows and a `hyperevm` venue block (data only —
no Rust reader; the nightly lane and `audit-pnl --fee-bps` consume it).

### 7.8 Tests
Proptest AND a cargo-fuzz target for the Swap decoder (house law: every
ingress parser has both). Loopback WS integration test using
`core_net::TestTransport` (the `hl_userws_loopback` family is the model).
A live `--raw-tap` smoke before trusting the parser — venue wire drifts have
only ever been caught live.

**DONE(H3):** archive probe refuses a dishonest endpoint in a unit test ·
decoder proptest + fuzz 300 s clean · loopback green · live `--raw-tap`
shows real pool states · spot symbols resolve with `unresolved=0` ·
alloc 0 B/op steady state · copy-audit new=0.

---

## 8. H4 — `crates/strategy-hyparb`, the slot-0 member

Reference member: **`crates/strategy-bin15`**. Match its shape.

### 8.1 Skeleton (land a stub in H0 so the set compiles)
```toml
[dependencies]
core-amm      = { workspace = true }
core-fill     = { workspace = true }
core-time     = { workspace = true }
core-types    = { workspace = true }
strategy-core = { workspace = true }
```
```rust
#![forbid(unsafe_code)]     // stricter than the workspace default — bin15 does this
#![deny(missing_docs)]
```
**Law — no `core-config` dependency.** The cli parses `hyparb.toml`,
validates every bound, and passes a params struct. A member that parsed its
own TOML would be a second grammar to keep in step with the artifact hash
the operator reads at boot.

### 8.2 Struct
```rust
pub const HYPARB_MAX_POOLS: usize = 128;
pub const HYPARB_MAX_COINS: usize = 8;

#[repr(C, align(64))]
pub struct HyparbStrategy {
    params: HyparbParams,
    pools: [PoolSlot; HYPARB_MAX_POOLS],
    perp: [CoinTouch; HYPARB_MAX_COINS],     // HL perp BBO + funding + premium
    spot: [CoinTouch; HYPARB_MAX_COINS],     // HL spot BBO  (O-H10)
    basis: [BasisEma; HYPARB_MAX_POOLS],     // pool-vs-perp, rolling median/EMA
    maps: Box<[core_amm::TickMap<1024>; HYPARB_MAX_POOLS]>,   // the ONLY heap
    counters: HyparbCounters,
    cooldown: strategy_core::CooldownGate<HYPARB_MAX_POOLS>,
    inventory: [i64; HYPARB_MAX_COINS],      // unhedged, 1e6 — first-class
    day_notional_1e6: i64, day_epoch: u64, oid_seq: u64,
    orders_emitted: u64, orders_dropped: u64,
    configured: u8,
    regime_label: core_types::RegimeLabelSet,
}
impl HyparbStrategy {
    #[must_use] pub fn new() -> Self;   // boot only; takes the map box
    pub fn configure(&mut self, params: HyparbParams,
                     maps: Box<[core_amm::TickMap<1024>; HYPARB_MAX_POOLS]>,
                     metas: &[core_amm::PoolMeta],
                     anchor: core_time::WallAnchor) -> Result<(), StrategyError>;
    pub const fn counters(&self) -> HyparbCounters;
    pub fn pool(&self, idx: usize) -> Option<&PoolSlot>;
}
```

### 8.3 `impl Strategy`
Required: `on_start`, `on_tick`, `on_signal`, `on_fill`, `on_timer`,
`timer_period_ns`, `on_stop`. **Every callback opens with
`if self.configured == 0 { return; }`** (the bin15 convention).

* `on_start` — unconfigured ⇒ `Err(StrategyError::Config("hyparb: on_start before configure"))`.
* `on_tick` — an HL BBO updates `perp[i]` or `spot[i]` by symbol.
  **`tick.is_stale()` ⇒ record but never trade** (VT4).
* `on_venue_event` — `ChannelId::Mark` / funding carries the perp's funding
  rate and premium; feed the hedge selector (§8.5).
* `on_signal` — `source == SignalSource::HyperEvm` unpacks the 40-byte
  payload into a `PoolState`, updates `pools[idx]`, refreshes `basis[idx]`,
  and re-evaluates that pool alone.
* `on_fill` — book it. An HL-leg fill closes the hedge; an AMM-leg fill
  (venue 7, `FILL_ORIGIN_PAPER`) closes the pool leg. **Unhedged inventory
  is first-class**: track per coin, cap it, halt the member on breach.
* `on_timer` — 1 s: roll the day cap, age out stale pool states, TWAP out
  unhedged inventory, refresh the basis EMA and the funding estimate.
* `timer_period_ns` — `1_000_000_000` configured, `u64::MAX` otherwise.
* `on_regime` — empty. Carry the label, never consult it (the HORIZON law).

**Law — never stamp `strategy_id`.** `StampCtx` writes the slot; a member
that stamped itself would disagree the moment a slot moved.

**Law — enumerate `SubmitErr` arms, never `_`:**
```rust
Err(SubmitErr::RingFull | SubmitErr::Unsupported | SubmitErr::NoSuchOrder | SubmitErr::Refused) => {
    self.orders_dropped = self.orders_dropped.wrapping_add(1);
    false
}
```

### 8.4 The four corrections — each a config-visible term
These are what the backtest was wrong about. Each is a knob so it can be
turned off and re-measured — that is the point of the build.

1. **Depth cap.** Hedge notional clamped to the chosen venue's live
   top-of-book (both legs on a cross pool). **The single largest
   correction in the whole model** (magnitude: vault verdict §4.3). HYPE perp
   top-of-book is a measured median ≈$900 against a naive $20 k swing.
2. **Latency.** Decide at *t*, hedge priced at *t+lag*. A decision that turns
   negative under the lag counts `missed_on_lag` and is not taken. The AMM
   leg's own latency is the one-block Δ in `ACTIVATION_NS_DEFAULT[7]`.
3. **Basis.** De-mean each pool against its own prevailing pool-vs-perp
   basis (rolling median over `basis_window_ns`). The basis runs the same
   order of magnitude as the edge itself (percentiles: vault verdict §4.3),
   and leaving it uncontrolled produced a badly one-sided flow in
   simulation. **A persistent buy/sell side-skew in live paper means basis
   control is off or wrong** — that is the tell to watch.
4. **Gas.** Charged per ATTEMPT including losses, from the measured
   distribution — never the base fee. p50 0.34 gwei (3.4× base), p99 258.9
   gwei (2,600× base); USD/tx p50 $0.0098, p90 $0.26, p99 $3.91.

### 8.5 The hedge-venue selector (O-H10)
```rust
/// Total cost of hedging `notional_usd_1e6` of `coin` on each venue, and
/// the cheaper one. Costs are bps ×1e6, SIGNED — funding can make the perp
/// leg cheaper than free.
///
///   perp = taker_bps + |basis_dev_bps| + depth_penalty − expected_funding_bps
///   spot = taker_bps_spot + depth_penalty
///
/// `expected_funding_bps` is the funding rate over the expected hold,
/// signed by our side: short perp earns positive funding. It is a material
/// term in the incumbent's own accounting, not a rounding error.
fn choose_hedge(&self, coin: usize, side: Side, notional_usd_1e6: i64)
    -> (HedgeVenue, i64);   // (venue, total_cost_bps_1e6)

#[derive(Copy, Clone, Debug, PartialEq, Eq)] #[repr(u8)]
pub enum HedgeVenue { Perp = 0, Spot = 1, None = 255 }
```
Config gates: `hedge_venue = "auto" | "perp" | "spot"` so either can be
forced for an A/B, and `hedge_switch_hysteresis_bps_1e6` so the choice does
not flap tick to tick.

### 8.6 `StrategyCounters`
Add to `strategy-core`: a `#[repr(C)]` `HyparbCounters` POD
(`Copy + Clone + Debug + Default + PartialEq + Eq`), a defaulted
`fn hyparb_counters(&self) -> HyparbCounters { … }`, and
`fn hyparb_pools_view(&self, out: &mut [HyparbPoolView]) -> u32` — a
caller-owned slice, `min(out.len())`, while-index loop, never allocates.
`strategy_kind()` returns `"hyparb"`.

**DONE(H4):** member compiles into the set · `--strategy hyparb` boots in
paper and emits nothing without an artifact · alloc 0 B/op on
`on_tick`/`on_signal` · every public fn has a happy-path AND a failure test ·
`hedge_venue = "perp"` and `"spot"` both run.

---

## 9. H5 — config + boot

### 9.1 `crates/core-config/src/hyparb.rs`
**No `serde`, no `toml`** — the workspace has neither. Hand-roll a line
scanner. Reuse the `pub(crate)` primitives in `icdp.rs` (`strip_comment`,
`parse_int`, `parse_value`, `Value`) and, because `hyparb.toml` needs
per-pool blocks, the `[[table]]` state machine from `icdp::parse` rather
than `parse_single_section`.
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyparbError(pub String);
impl core::fmt::Display for HyparbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "hyparb.toml: {}", self.0)   // name the RIGHT file — see the VrpError defect
    }
}
impl std::error::Error for HyparbError {}
impl From<IcdpError> for HyparbError { fn from(e: IcdpError) -> Self { Self(e.0) } }

const HYPARB_KEYS: [&str; 17] = [
    "endpoint_kind", "lag_ns", "basis_window_ns", "basis_enabled",
    "depth_cap_enabled", "gas_model", "gas_p50_usd_1e6", "gas_p99_usd_1e6",
    "max_order_usd_1e6", "cap_instance_usd_1e6", "cap_day_usd_1e6",
    "min_net_bps_1e6", "inventory_cap_usd_1e6", "mode",
    "hedge_venue", "hedge_switch_hysteresis_bps_1e6", "funding_window_ns",
];
const HYPARB_POOL_KEYS: [&str; 6] =
    ["address", "coin0", "coin1", "trade", "max_notional_usd_1e6", "tick_cap"];

pub fn parse(src: &str) -> Result<HyparbFile, HyparbError>;
```
Inherited grammar laws: **integers only** (a float anywhere is fatal) ·
**an unknown key is a REFUSAL** (a typo'd knob that silently took a default
is a member trading an edge nobody chose) · duplicate key fatal · duplicate
section fatal · key before header fatal · **arrays are ONE LINE** (the shared
`parse_value` reads an array off a single trimmed line) · nothing defaults
silently except keys explicitly marked OPTIONAL, and an absent optional key
means exactly the behaviour the file had before that key existed.
Register the module beside `pub mod bin15;`.

### 9.2 `hyparb.toml.example` (repo root)
House style — a header explaining the artifact hash, the one-line-array law,
and every key's *reason*, not just its type. Copy the tone of
`vrp.toml.example`. **This file is the grammar contract**; parser and example
are reviewed together.

### 9.3 `crates/cli/src/hyparb_boot.rs`
```rust
pub struct HyparbBoot {
    pub params: strategy_hyparb::HyparbParams,
    pub metas: Vec<core_amm::PoolMeta>,
    pub maps: Box<[core_amm::TickMap<1024>; HYPARB_MAX_POOLS]>,
    pub hash: [u8; 32], pub path: PathBuf, pub resolved: usize,
}
pub fn hyparb_wanted(requested: u8) -> bool;
pub fn load_hyparb_boot(
    artifact: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    tls: &Arc<rustls::ClientConfig>,
    endpoint: &str,
) -> Result<Option<HyparbBoot>, String>;
pub fn render_boot_tell(boot: &HyparbBoot, dormant: usize) -> String;
```
Boot laws: **requested-but-absent REFUSES** (the icdp/F19 law) ·
**present-and-unreadable refuses too** · the engine hashes the exact artifact
bytes and prints them in the boot tell · **the archive probe runs here** and
a failure refuses, naming the endpoint. Add `pub mod hyparb_boot;` to
`cli/src/lib.rs`.

### 9.4 Call site — `multivenue-engine.rs`
Copy the bin15 block (≈:3696-3727) exactly: gate on the bit → `Err ⇒
join_reverse(handles); return ExitCode::from(1)` → a **separate** F19 check
for requested-but-`None`.
```rust
/// HYPARB: the slot-0 parameter artifact (`~/multivenue/hyparb.toml`).
/// Read only when the mask carries the hyparb bit; absent or unresolvable
/// with the bit set REFUSES the boot — never a silent no-op.
#[arg(long)] hyparb: Option<PathBuf>,
/// HYPARB O-H5: allow the EVM write path, TESTNET ONLY. The crate refuses
/// chain id 999 regardless; this flag is the second switch.
#[arg(long, default_value_t = false)] evm_testnet: bool,
/// HyperEVM JSON-RPC websocket path. Absent ⇒ the ingress is not started.
#[arg(long)] hyperevm_path: Option<String>,
```
`engine_loop_set_full` gains `hyparb: Option<&HyparbBoot>` and
`if hyparb.is_some() { configured |= strategy_set::BIT_HYPARB; }`.

### 9.5 `scripts/engine-wrapper.sh` — three edits
1. `case` arms: add `hyparb|ai+hyparb|ai+vrp+xsd+bin15+hyparb`;
2. the refusal message's `allowed:` list;
3. a `HYPARB_TOML` / `EVM_TESTNET` block mirroring `EXEC_TOML`/`ARM_LIVE` —
   both-or-neither, `exit 78` on exactly-one-set, appended as its own array
   to the final `exec` line.

**Per O-H8 this phase does NOT edit `~/multivenue/strategy.conf`.**

**DONE(H5):** a good artifact boots and prints its hash · a float, an unknown
key, a duplicate key and a multi-line array each refuse with `line N:` naming
`hyparb.toml` · requested-but-absent refuses · wrapper refuses a half-set
pair with exit 78 · live `strategy.conf` untouched.

---

## 10. H6 — metrics and verdict-validation instrumentation

`register_hyparb_metrics` beside `register_bin15_metrics` (≈:5422), same
`one(name)` closure idiom, names literal as `engine_hyparb_<thing>_total`.
One line in `Observability::build` (≈:3467 block). **Members keep plain
`u64` counters; the mirror fn computes `cur.saturating_sub(last)` and
`inc`s** — never `.inc()` from the member.

Budget: `MAX_COUNTERS = 512`, `MAX_GAUGES = 384`; the live engine sat at
≈243 at E7. `RegErr::Full` is a refused boot — add the family-size pin test
(`the_hyparb_family_is_N_counters_and_M_gauges`, modelled on
`the_bin15_family_is_31_counters_and_80_gauges` ≈:9026).

### The six disputed quantities — why the lane is being built

| # | the backtest claimed | the live counter that settles it |
|---|---|---|
| 1 | Top-of-book caps the hedge (vault §4.3) | `engine_hyparb_hedge_depth_usd_1e6` per coin per venue vs `…_size_capped_total` |
| 2 | Perp basis manufactured a one-sided flow (vault §4.3) | `engine_hyparb_basis_bps_1e6` per pool + `…_side_buy_total` / `…_side_sell_total` — **should stay ≈1:1** |
| 3 | HL maker materially beats taker (vault §4.3) | run `mode = "maker"` in paper alongside taker; diff the series |
| 4 | P&L was highly concentrated in one pool (vault §4.7) | capture all, trade the subset, rank by `engine_hyparb_pool_pnl_usd_1e6` |
| 5 | The vault's figures are a ×730 extrapolation from 12 h (vault §4.7) | continuous paper removes the extrapolation |
| **6** | **(new, O-H10) perp vs spot hedge** | `engine_hyparb_hedge_venue_perp_total` / `…_spot_total`, `…_hedge_cost_bps_1e6` per venue, `…_funding_earned_usd_1e6`. Settles whether the spot fee premium or the basis-plus-funding of the perp is the better trade. |

Nightly `pnl_report` gains a `hyparb` section diffing live paper against the
backtest ladder (r0 → r1 depth → r2 latency → r3 full; the ladder itself
is defined in the vault verdict §4.3).
**Python: full `import x` only, never `from x import y`** — ruff, a pytest
and `.claude/hooks/no-forbidden-crates.sh` all enforce it.

**DONE(H6):** family-size pin test green · `/metrics` carries every counter ·
the nightly section renders against a day of paper.

---

## 11. H7 — `signer-evm` + `exec-hyperevm` (testnet-interlocked)

### 11.1 What exists (verified) — and the one trap
**Reusable:** `signer_eip712::{keccak256, keccak256_parts, sign_digest,
sign_digest_with_key, parse_secret_key, address_from_private_key}` ·
`core_config::SecretKeyBytes` (the mlock'd key page) · `exec-hyperliquid`'s
render-in-place discipline and mio+rustls arm · `ingress-rpc`'s JSON-RPC
request writers · `exec-router`'s halt/risk machinery.

**Absent from the entire repo:** any RLP encoder · any EIP-1559 typed-tx
builder · any `eth_sendRawTransaction` caller.

> **THE SIGNATURE TRAP.** `sign_digest_with_key` returns `[u8; 65]` as
> `r ‖ s ‖ v` with **`v = recid + 27`** (legacy Ethereum). **EIP-1559
> type-0x02 carries `signature_y_parity ∈ {0,1}`, not 27/28.** Subtract 27
> at the RLP-append site. Get this wrong and every transaction is
> well-formed and universally rejected — silent, 100 % failure.
> (`secp256k1` is pulled with `features = ["recovery", "lowmemory"]` and
> `sign_ecdsa_recoverable` already low-S-normalises, so EIP-2 is satisfied.)

### 11.2 `crates/signer-evm` (O-H7) — chain-agnostic
```
src/lib.rs   re-exports; the y_parity helper
src/rlp.rs   minimal RLP: byte-string + list, rendered INTO the final buffer
src/tx.rs    EIP-1559 typed tx: build → keccak → sign → serialise, in place
```
```toml
[dependencies]
signer-eip712 = { workspace = true }   # the secp256k1 primitive + keccak
core-types    = { workspace = true }
```
```rust
/// `v = recid + 27` (EIP-712 / legacy) → `y_parity ∈ {0,1}` (EIP-1559).
#[inline] #[must_use]
pub const fn y_parity_from_v(v: u8) -> u8 { v.wrapping_sub(27) & 1 }

#[derive(Copy, Clone, Debug)]
pub struct Eip1559Tx<'a> {
    pub chain_id: u64, pub nonce: u64,
    pub max_priority_fee_per_gas: u128, pub max_fee_per_gas: u128,
    pub gas_limit: u64, pub to: [u8; 20], pub value: u128,
    pub data: &'a [u8],   // access_list is always empty — assert it
}

/// Render the unsigned pre-image into `dst` and return its keccak digest.
/// Renders IN PLACE: no intermediate Vec, no concatenation.
pub fn tx_signing_digest(tx: &Eip1559Tx<'_>, dst: &mut [u8]) -> Result<([u8; 32], usize), EvmTxErr>;

/// Render the SIGNED transaction into `dst`. Returns the byte length.
pub fn tx_encode_signed(tx: &Eip1559Tx<'_>, sig: &[u8; 65], dst: &mut [u8]) -> Result<usize, EvmTxErr>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EvmTxErr { BufferTooSmall, AccessListUnsupported }
```

### 11.3 `crates/exec-hyperevm`
```
src/lib.rs    Network, config-from-env, the interlock assertions
src/nonce.rs  fixed-size multi-wallet nonce table, single-writer
src/gas.rs    the bidding policy — a pure fn of expected edge
src/arm.rs    the mio+rustls send arm + receipt tracking
```
```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Network { Testnet = 998 }      // Mainnet DELIBERATELY ABSENT in HZ-H9

/// **O-H5.** The compiled-capability allow-list, mirroring
/// `exec_boot::LIVE_ARM_VENUES`. E1 shipped that empty with a compile-time
/// assertion that it stayed empty; deleting the assertion was reviewed as
/// the moment the engine became able to trade real money. This one stays
/// testnet-only until a phase after H9 says otherwise, in writing.
pub const EVM_ARM_CHAIN_IDS: &[u64] = &[998];
const _: () = assert!(EVM_ARM_CHAIN_IDS.len() == 1 && EVM_ARM_CHAIN_IDS[0] == 998);
```

**Keys.** Per the standing secrets law, keys live in `.env` only, `chmod
600`, git-ignored, and are **never read, printed or edited by a session**.
The operator generates and funds the testnet wallets and supplies them as
`HYPEREVM_KEY_0..N`. **3–5 wallets is enough to prove nonce parallelism**;
CBB's 100+ was a mainnet throughput answer, not a correctness one. Load
through `core_config::SecretKeyBytes` into an mlock'd page; zeroize on drop.

### 11.4 The interlock — five layers, mirroring `exec_boot.rs`
1. **clap** — `--evm-testnet` (§9.4); pair with `requires` if a second
   switch is added.
2. **A `resolve`-style fn** refusing a half-armed command line *before*
   reading any artifact.
3. **Set equality, never subset**, between what the artifact marks and what
   the flag names, with BOTH sets printed in the refusal. A subset rule
   would let one edited file arm the lane — the single point of failure the
   second switch exists to remove.
4. **The compiled allow-list** above: a chain id outside
   `EVM_ARM_CHAIN_IDS` is refused in code, naming the phase that would
   supply it.
5. **Network agreement at boot**, mirroring the LAW E-4 precondition at
   `multivenue-engine.rs:3789-3809`: the market-data endpoint's chain id and
   the exchange endpoint's chain id must match, or
   `join_reverse(handles); return ExitCode::from(1)`. The ARMED tell prints
   the network and shouts the mainnet spelling in caps — keep that.

### 11.5 Zero-copy obligations
The doctrine is binding: **encoders render into the FINAL wire buffer,
signers hash in place.** `rlp.rs` and `tx.rs` write into the caller's
buffer, never an intermediate. `keccak256_parts` exists precisely so the
pre-image is hashed without concatenating it. Any unavoidable copy carries,
within the eight lines above it,
`// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>`.
`make copy-audit` is a ratchet against `scripts/copy-audit-baseline.txt`;
**only the operator grows the baseline.**

### 11.6 Tests
Known-answer vectors for RLP and for a full signed EIP-1559 transaction
(from the EIP-1559 spec and a real testnet tx). Proptest: any
`(nonce, gas, to, value, data)` round-trips encode→decode. Fuzz target on
the RLP writer. **A `y_parity` pin test that fails if the value ever leaves
`{0,1}`.**

**DONE(H7):** vectors pass · the crate refuses chain id 999 in a unit test ·
copy-audit new=0 · alloc 0 B/op on the render+sign path.

---

## 12. H8 — testnet smoke

**Measured 2026-09-19.** HyperEVM testnet is chain **998** (`0x3e6`), block
time **0.984 s**, gas limit **3,000,000** — identical to mainnet — with
**21 AMM pools and ≈450 Swap events per 3,000 blocks**. WHYPE exists at the
same address (`0x5555…5555`). **Hyperswap's factory does NOT exist there**
(`eth_getCode` = `0x`), so there is no like-for-like pool and no competing
arb flow.

Same shape as the E7 R0 finding ("testnet has no 15-minute family, so R0 ran
there only as the exec battery"). **Testnet is an exec battery, not a
market.** It proves construct → sign → submit → receipt end to end against a
real 1 s chain; multi-wallet nonce behaviour under concurrent sends; gas
bidding mechanics and what a losing bid looks like.

It **cannot** prove race-win rate.

Signal comes from **mainnet** HL + mainnet pool reads; only the write lands
on testnet. **Label every artefact this phase produces with that hybrid**,
so no number from H8 is ever quoted as a mainnet result.

**DONE(H8):** a signed transaction lands in a testnet block and its receipt
reconciles · three wallets send concurrently without nonce collision · a
deliberately-underbid transaction is observed losing.

---

## 13. H9 — gates, review, and the mask flip

```sh
cargo build --release --workspace
cargo nextest run --workspace
cargo test -p bench --test alloc_assertions --release -- --test-threads=1
cargo +nightly fuzz run amm_tick_walk        -- -max_total_time=300
cargo +nightly fuzz run hyperevm_swap_decode -- -max_total_time=300
cargo +nightly fuzz run evm_rlp              -- -max_total_time=300
make lint && make license-check && make copy-audit
make license-deps      # ONLY if a dependency changed
cd claude-worker && uv run pytest    # pgrep -f 'claude[-_]worke[r]' / pytes[t] FIRST
```
Baselines at HEAD: nextest **2585** (1 skipped) · alloc **62/62** at 0 B/op ·
clippy clean · license-check OK · copy-audit new=0 · worker pytest **1153**
(3 skipped). Known isolation-disproven flakes are listed in CLAUDE.md —
rerun in isolation before believing a red. `make py-lint` (ruff) is RED at
HEAD and has been for weeks; `make lint` means clippy.

Then the three Opus-5 review agents: **`alloc-auditor`**,
**`zero-copy-auditor`**, **`risk-reviewer`**. Record in
`docs/risk-policy.md` and a dated vault log.

**Only after HZ and H8 are green** does the operator flip
`~/multivenue/strategy.conf` to `STRATEGY=ai+vrp+xsd+bin15+hyparb` (O-H8).
That flip is an operator action, not a phase deliverable. Verify
`vm_rows_active ≥ 1` on `/state` after the restart, as after any restart,
and run `scripts/exec-smoke.sh` first because the engine is armed.

---

## 14. Appendix — traps, with their measured cost

1. **Dishonest archive endpoints.** `rpc.hyperliquid.xyz/evm` and
   `rpc.hypurrscan.io` answer historical `eth_call` with LATEST state and
   return `OK`. Only `rpc.purroofgroup.com` passed. Logs and headers are
   correct everywhere. → O-H4, the boot probe.
2. **`liquidityNet` sign extension.** `int128` in a 256-bit word; extending
   from 128 inflates every downstream number catastrophically and silently
   (vault verdict §3). 52.8 % of ticks are negative.
3. **Constant-liquidity sizing.** Invents depth. Walk the real map; stop at
   its edge. (The matcher's in-range approximation is the *opposite*
   direction — conservative — and is documented as such.)
4. **Re-harvesting.** Without carrying own market impact, a standing gap is
   re-taken every block on a stale pool.
5. **Perp ≠ spot.** De-mean against the pool's own basis or book the basis
   as profit. The tell is a persistent side-skew.
6. **Hedge depth.** The largest single correction once the top-of-book cap is applied (vault §4.3).
7. **`v` vs `y_parity`.** 27/28 from the signer; EIP-1559 wants 0/1.
8. **Venue byte 7 is unreachable** until `ACTIVATION_NS_DEFAULT` is widened
   to 8 — the matcher counts it `unroutable` and the order silently never
   fills.
9. **`spotMetaAndAssetCtxs` is not index-aligned** (329 vs 868). Zipping
   them yields nonsense. Use `l2Book` per pair, or align by name.
10. **Cost both measurements symmetrically.** The realised-flow screen
    over-counts (share in the vault verdict §4.2) and charges no fees; costing it like
    the simulation flips its sign (vault verdict §4.2).
11. **`--strategy` name drift.** `MASK_TABLE`, `STRATEGY_SET_NAMES` and the
    wrapper `case` must all carry a new name. They drifted once and took the
    whole set dark behind a healthy-looking capture (2026-09-12T20:04:42Z).
12. **X1 parity.** A fill law in the engine but not the harness (or vice
    versa) breaks replay silently. `paper_replay_parity` is the guard.
13. **SymbolId ordinals are file order.** Append to `universe.toml`, never
    reorder — a reorder silently repoints every historical sym.
14. **The Cowork sandbox.** `cargo` there gives false greens; git write-ops
    there leave stale `.git` locks. Both happen on the Mac.

## 15. Appendix — API quick reference

```rust
// strategy-core
trait Strategy: StrategyCounters {
    fn on_start<C: Ctx>(&mut self, ctx: &mut C) -> Result<(), StrategyError>;
    fn on_tick<C: Ctx>(&mut self, tick: &Tick, ctx: &mut C);
    fn on_signal<C: Ctx>(&mut self, signal: &Signal, ctx: &mut C);
    fn on_fill<C: Ctx>(&mut self, fill: &Fill, ctx: &mut C);
    fn on_timer<C: Ctx>(&mut self, now_ns: NsTs, ctx: &mut C);
    fn timer_period_ns(&self) -> u64;
    fn on_stop<C: Ctx>(&mut self, ctx: &mut C);
    // defaulted: on_ai, on_ruleset_table, on_venue_event, on_depth,
    //            on_opt_summary, regime_label, set_regime_label, on_regime
}
trait Ctx { fn submit(&mut self, o: Order) -> Result<(), SubmitErr>;
            fn cancel(&mut self, r: CancelReq) -> Result<(), SubmitErr>;
            fn modify(&mut self, prev_oid: u64, o: Order) -> Result<(), SubmitErr>;
            fn now_ns(&self) -> NsTs; }
// clob-dispatcher
trait OrderDispatch { submit, cancel, modify, try_next_fill, stats,
    observe_tick, observe_pool /* NEW */, matcher_counters, open_paper_orders,
    on_idle, on_venue_event, on_fill_booked, halt_signal, cancel_all,
    cancel_all_state, exec_counters, arm_counters }
// core-fill
pub const ORDER_KIND_MAKER: u8 = 0; ORDER_KIND_IOC: u8 = 1; ORDER_KIND_AMM_SWAP: u8 = 2;
judge_ioc(side, px, rem, Touch, &mut ask_budget, &mut bid_budget) -> Verdict
judge_maker(...) -> Verdict
judge_amm(side, px_limit_1e6, rem, PoolTouch) -> Verdict        // NEW
ACTIVATION_NS_DEFAULT: [u64; 8]                                  // widened
// core-types
Order::new(ts_ns, VenueId, sym, side, kind, Price, Qty, client_oid).with_ttl_ns(ns)
Tick { ts_ns, sym, venue_seq, bid_px, bid_qty, ask_px, ask_qty, venue, flags, venue_time_ms }
Signal::new(ts_ns, sym, LatencyClass, source_u8, payload: [u8; 40])
trait Capture { tick, opt_summary, event, depth, signal, raw_frame, parse_reject, maybe_flush }
// core-net
trait Transport { interest, register, reregister, pump, read, write, flush }
IoBuf::{with_capacity, filled, filled_mut, free_mut, advance, consume, clear}
ws_read_frame ; ws_unmask_in_place ; queue_masked_text_frame
boot_http::https_post(tls, host, port, path, ua, ctype, body, out, max, timeout)
// core-parse  (NO hex helper — see §7.3)
find_field(buf, needle_with_colon) ; skip_byte ; skip_ws ; skip_string ;
skip_json_value ; scan_u64 ; scan_i64 ; scan_price_1e6/1e8/1e9
// signer-eip712  (unchanged by this plan)
keccak256(&[u8]) -> [u8;32] ; keccak256_parts(&[&[u8]]) -> [u8;32]
sign_digest_with_key(&SecretKey, &[u8;32]) -> Result<[u8;65], SignError>  // v = recid+27
parse_secret_key ; address_from_private_key
// signer-evm  (NEW)
y_parity_from_v(v: u8) -> u8 ; tx_signing_digest(&Eip1559Tx, dst) ; tx_encode_signed(...)
```

---

## 16. Implementation record

Built on branch `hyparb` in the worktree `~/trading-engine-multivenue-hyparb`
(O-H17), one commit per phase (O-H14). Baselines at the branch point
`90c9c39`: nextest **2603** (1 skipped) · alloc **62/62** at 0 B/op.

### 16.1 HZ part A — done 2026-09-23 (numbers in the vault, `hz/`)

Measured from the MacBook: newHeads over purroof WS, and the ACK RTT of an
`eth_sendRawTransaction` fired at head arrival (throwaway UNFUNDED key —
every reply `-32003 insufficient funds`, nothing landed). Engineering
conclusions: **send through the official endpoint** (purroof is ~3× slower
on sends); **the feed's jitter, not the send, is the binding term**; Swap
logs ride with their head. Part B (inclusion N+1 vs N+2) needs the funded
testnet wallets. Per O-H13 this gates the mask flip, not the build.

### 16.2 H1 `core-amm` — LANDED

Deviations from §5, each deliberate:

* **Bit-exact, not "≤ 0.05 % median".** `TickMath`, `SqrtPriceMath`,
  `SwapMath` and the pool's swap loop are ported with their rounding AND
  their bitmap-word step decomposition (`nextInitializedTickWithinOneWord`
  emulated from the sorted node list), over a crate-private 256-bit
  integer (Knuth D for the 512÷256 `mulDiv`). The replay gate therefore
  demands the chain's own numbers to the wei — input, output, post-price,
  post-tick, post-liquidity — not a statistical bar. The spec's 0.05 %
  median is still reported (it is the 500-pip pool's fee, as predicted).
* `#![forbid(unsafe_code)]` — stricter than §5.1; the walk's indices are
  bounded by the branches that produce them.
* **No `core-types` dependency** (nothing in it was used).
* `solve_arb(state, meta, map, &ArbParams)` — the five hedge-side integers
  travel as one `#[repr(C)]` POD instead of five positional arguments.
* Additional public API, used by the gate and by H2: `swap_exact` (the
  contract's exact fee-inclusive swap), `swap_exact_in_range` (its
  constant-liquidity, map-less variant for the matcher), `SwapSpec`,
  `SwapResult`, `max_liquidity_per_tick`.
* `POOL_FLAG_EDGE = 2`: a state produced by the map-less judge ending ON a
  range edge (liquidity beyond unknown) is marked, and `is_live()` is
  false until a real update overwrites it.
* `ArbQuote.flags` (`ARB_FLAG_SIZE_CAPPED | MAP_EDGE | NOT_LIVE |
  BELOW_GAS | MATH`) — the counters §10 needs.
* `TickMap::load(nodes, lo, hi, spacing)` refuses a `|liquidityNet|` above
  the contract's `tickSpacingToMaxLiquidityPerTick` — the fingerprint of a
  sign-extension decode — as well as unsorted/duplicate/misaligned ticks.
* Property tests live in `tests/walk_props.rs` (a test target named
  `proptest` would shadow the crate).

**The replay fixture** (`tests/data/amm-replay.tsv`): 6,000 real mainnet
swaps from two DISJOINT ≤ 2 h windows (the first swap of each (pool, block),
evenly sampled), exact pre-state from the archive at `block − 1`, raw chain
data only. Built by the git-excluded fixture tools under
`docs/research/hyparb/fixture/`.

### 16.3 Findings from H1 that bind later phases

15. **Dynamic fees.** 9 of 34 active Uniswap-ABI pools charge an effective
    fee that `fee()` read at HEAD does not report (nominal 500–1,500 pips,
    effective 360–2,995). `fee()` at `block − 1` fixes most; a few pools
    move it swap by swap. Every walk is bit-exact under the right fee, so
    the fee is observable: **the member (H4) must estimate each pool's
    effective fee from its observed swaps** (the gate's fee-solve is the
    method) — a boot-time static fee misprices the edge by more than the
    edge.
16. **20 of the 57 universe "V3" pools do not expose `slot0()`** (a
    different CL ABI). H3's discovery covers the 37 that do; the other 20
    need a decoder of their own or are excluded — an H3 ruling.
17. **The official endpoint rate-limits `eth_getLogs`** (`-32005` within
    seconds). Historical logs come from the archive endpoint.
18. **An EOA cannot call a V3 pool's `swap()`** — the pool calls back into
    `msg.sender` to collect payment. H7/H8 calldata must target either the
    DEX's router (approvals + `exactInputSingle`) or an executor contract
    of our own. Supersedes spec-gap default G1; operator ruling pending.
19. The top pool shows a recurring same-size liquidity change INSIDE
    blocks (a position managed intra-block) — the 0.7 % of rows a
    first-swap-of-block fixture cannot reproduce.
20. **Slipstream's `fee()` is not always the fee charged**: 203 of 1,580
    swaps (13 %) needed the solved fee. Same law as finding 15 — the member
    estimates each pool's effective fee from its swaps.
21. Kittenswap (Algebra v1.2) emits an extra per-`Burn` event
    (`0x1a25098b…`, 2 topics, 1 word); it carries no state the member needs
    and is not subscribed.
22. **HyperEVM burns priority fees** (Cancun without blobs, HyperBFT): the
    G2 gas bid buys ordering, not a proposer tip — H7c's gas policy must be
    re-measured on testnet (HZ part B).

### 16.4 H7a `signer-evm` — LANDED (standalone; `exec-hyperevm` pending)

Deviations from §11.2:

* **No binary encoder and no scratch pre-image.** The signing digest and
  the transaction hash are `keccak256_parts` over stack encodings and the
  BORROWED calldata; the signed envelope is rendered as `0x`-hex straight
  into the JSON-RPC body (`tx_encode_signed_hex`) — JSON-RPC is the only
  transport, so a binary transaction buffer would exist only to be copied.
* API: `tx_signing_digest(tx)`, `tx_sign(tx, sk)`, `tx_hash(tx, sig)`,
  `signed_hex_len(tx, sig)`, `tx_encode_signed_hex(tx, sig, dst)`,
  `y_parity_from_v`. `EvmTxErr { BufferTooSmall, BadSignature, Sign }` — a
  `v` outside {27, 28} is refused, never masked; `AccessListUnsupported`
  is gone because the struct cannot express an access list.
* Known-answer vectors from an INDEPENDENT implementation (eth-account
  0.14): five transactions covering zero fields, a one-byte calldata below
  0x80, a 55-byte short-string edge, a 300-byte calldata (two-byte length)
  and 101/127-bit integers — digest, `r`, `s`, `y_parity`, raw hex and hash
  all byte-equal.
* Added to `scripts/copy-audit.sh`'s default crate list (exec lane).

### 16.5 O-H19 `core-amm` families — LANDED

* **Algebra Integral loop** (`AMM_KIND_ALGEBRA = 2`): same step arithmetic
  as V3 (`movePriceTowardsTarget` ≡ `computeSwapStep`), a different loop —
  every step targets the next INITIALISED tick of the linked list (no
  bitmap-word stops, no `MIN/MAX_TICK` clamp), and a step that moves the
  price without reaching its target ends the swap. Monomorphised
  (`walk::<LINKED>`), selected by `PoolMeta::kind`; Algebra maps load with
  spacing 1.
* **Replay gates, one per family, all against real HyperEVM swaps from
  three disjoint ≤ 2 h windows:**

  | family | rows | pools | bit-exact | note |
  |---|---|---|---|---|
  | Uniswap V3 ABI | 6,000 | 35 | 99.38 % | unchanged from H1 |
  | Slipstream (Hybra CL) | 1,580 | 8 | **100.00 %** | 203 rows needed the solved fee |
  | Algebra v1.0 + v1.2 (NEST, Kittenswap) | 5,538 | 12 | **100.00 %** | fee in force from `Fee` / `SwapFee`, never solved |

  The same Algebra rows walked with the V3 loop are 99.11 % exact — the 49
  misses are exactly the step-decomposition difference the Algebra loop
  removes.
* **Tick-map maintenance** — `TickMap::apply_position` (Mint/Burn:
  gross ± amount at both ends, net ± at lower/upper, de-initialise at
  gross 0, sorted insert) and `PoolState::apply_position` (in-range
  liquidity, the contract's `lower ≤ tick < upper`). All-or-nothing; an
  error means the map disagrees with the chain and must be resynced.
  `TickNode` now carries `liquidityGross` (u96, in what was padding — the
  node stays 32 B; above 2⁹⁶ is refused, never truncated).
* **`core_amm::payload`** — the 40-byte pool-event payload, one codec for
  both ends of the ring. Byte 0 = kind (low nibble) | sub (high nibble);
  the pool is the `Signal`'s `sym` (G3). Kinds: `STATE` (tick i24,
  sqrtPriceX96 **u160**, liquidity **u128** — full width, so §7.5's
  `liquidity_truncated` refusal is gone), `SWAP` (block u56, amounts
  i128), `FEE` (Algebra v1.0 / v1.2), `LIQUIDITY` (Mint/Burn), `HEAD`
  (block, timestamp, baseFeePerGas), `GAP`, `SNAPSHOT`, `TICK`. The
  decoder is canonical (anything it accepts re-encodes to the same bytes).
* Gates: property tests run every walk property in BOTH loops and pin the
  loops to each other (same end state, amounts within one wei per step);
  alloc gate **65** (Algebra walk + map mutation + payload codec, 0 B/op);
  fuzz `amm_tick_walk` (now both loops) and new `amm_map_payload`.

### 16.6 H3a `ingress-hyperevm` — LANDED (not wired)

Deviations from §7, each deliberate:

* **The ingress snapshots the pools, not the boot loader** (§7.4 / §8.2 /
  §9.3). A tick map must be rebuilt after every stream break at a block
  the stream continues from without a gap, AND it must be on the capture
  tape so a replay rebuilds the maps the live member walked (X1 parity —
  §6.5 already requires pool state to come from the tape). So the session
  subscribes `newHeads` + pool logs first, pins the next head as block
  `B`, reads every pool at `B` over the same WebSocket (pipelined
  `eth_call`s, up to 256 in flight), and publishes `SNAPSHOT` · `TICK`… ·
  `STATE` before any event after `B`. Events that stream in meanwhile are
  held (boot-allocated, 4,096) and flushed in order; those at or before
  `B` are dropped as covered. Consequences for H4/H5: `configure()` takes
  no maps (the member owns its boxed maps and fills them from signals);
  `HyparbBoot` loses `maps`; the cli does no HTTPS pool discovery.
* **The O-H4 archive probe runs inside every snapshot** — each pool's price
  is also read at `B − 1000`; if no pool differs, the snapshot is refused
  and the run ends with `RunResult::ArchiveDishonest` (O-H15: the caller
  disables the member, not the engine).
* **Three families** (O-H19): V3 (7-word `slot0`, bitmap + `ticks`),
  Slipstream (6-word `slot0`, 10-word `ticks`), Algebra (`globalState`,
  `prevTickGlobal`/`nextTickGlobal`, the linked list walked both ways;
  the `MIN/MAX_TICK` markers bound the coverage). Coverage: ±`radius`
  ticks (default 4,000), never more than 1,024 initialised ticks (narrowed
  around the price) or 64 bitmap words. A reply with the wrong ABI shape
  fails that pool, never guesses.
* **Resync without reconnect**: a ring drop in Live (the member lost an
  event it cannot recover) or a `removed: true` log → `GAP` and a fresh
  snapshot on the same connection; an event of one pool the payload
  cannot carry (an amount beyond `int128`) → `GAP` on THAT pool's symbol
  only. A reconnect announces the break with `GAP(last delivered block)`.
* **128-bit subscription ids** are folded (`hi ^ lo`) into `core_net`'s
  `SubId`; a fold collision between two live ids is refused.
* Decoders check each event's SHAPE (topic count, data-word count —
  measured on chain for all families) and decode signed words from all
  256 bits.
* `SIGNAL_SOURCE_HYPEREVM = 5` is named in the crate; the
  `SignalSource::HyperEvm` variant is H0's (after MX2).
* Gates: 26 unit/lifecycle tests (a model node answers the driver's own
  frames — snapshot, held events, Live, every resync path, the probe) + a
  400-case random-book property; alloc gate **66** (the whole session after
  the handshake — subscribes, snapshot, 1,000 live swaps — 0 B/op); fuzz
  `hyperevm_decode`.

### 16.7 H7b executor contract (O-H18) — LANDED (not deployed)

`contracts/hyparb-executor/`: `HyparbExecutor.sol` (solc 0.8.28, cancun —
HyperEVM runs Cancun without blobs), the committed creation / runtime
bytecode, and `scripts/hyparb-executor-repro.sh`, which fetches the pinned
solc (sha256-checked) and proves the committed bytes are what the source
compiles to. Owner-only `swap(pool, zeroForOne, amountSpecified,
sqrtPriceLimitX96, minOut)`; the pool is held in transient storage for the
call, so `uniswapV3SwapCallback` / `algebraSwapCallback` pay only that pool
and only inside the swap; `BelowMinOut` reverts an unprofitable race;
`sweep` is owner-only. Foundry tests: 9 unit (mock pools, both callbacks,
impostor callbacks, USDT-style and false-returning tokens) + 4 on a
HyperEVM MAINNET FORK — a real Uniswap-ABI, Slipstream, Algebra v1.0 and
Algebra v1.2 pool each swapped exactly (local fork only; nothing sent).
Deployment is H8, testnet only (O-H5).

### 16.8 H0 — slot 0 is hyparb, HyperEvm = 8 — LANDED (dark)

Done as §4 says, with these decisions recorded:

* **The venue sweep went through one constant.** `core_types::VENUE_COUNT
  = 9` (const-asserted against `VenueId::from_u8`) now sizes every
  venue-indexed table (§2); the next venue is one edit plus its arms, not
  a sweep of literal `8`s. HyperEvm's stale default is 2 500 ms (HZ head
  p99 2 281 ms) and its activation Δ one block (1 000 ms); it rides no
  tick / depth / option / fill lane. `SignalSource::HyperEvm = 5`.
  `exec-router`: `EXEC_VENUES` 16, per-slot mask `u16`, table still
  inside its 64-byte bound. Harness labels gain `hyperevm` (the rendered
  fee table grows a trailing `hyperevm` entry). `SNAPSHOT_VENUES` /
  capture labels wait for H3b, `TRADEABLE_VENUES` for H2.
* **The standalone arm went with the name.** `engine_loop` /
  `engine_loop_with` / `engine_loop_full` (the LatencyArb-only loops) and
  `configure_latency_arb` are deleted; `engine_loop_set_full` loses its
  `EngineConfig` argument (only latency-arb read it); the ev / rule-tree
  defaults (`threshold`, `qty`, `cooldown`) are cli constants now. The
  name-pin tests lost their exemption: `STRATEGY_SET_NAMES` ⇄
  `MASK_TABLE` is an equality.
* **Dark means NOT configured.** At H0 slot 0 is never in the set
  builder's configured mask, so `--strategy hyparb` refuses ("no
  requested member is configured") and the two composite names boot
  with bit 0 cleared — an inert stub never boots under a healthy name.
  H5 makes it configured iff its artifact resolves.
* **The probe moved.** The strategy-set fan-out / mask / halt tests used
  the latency-arb trigger pair; they now use the ai-exec member
  (Heartbeat + one OrderIntent), and a new test pins the dark slot
  (`hyparb_slot_is_dark_and_inert_at_h0`).
* **Unlinked, not deleted — kept exercised.** `bench` (standalone alloc
  gate + criterion bench) and `core-io` (replay parity test) keep their
  dev-dependency on `strategy-latency-arb`, exactly as they do for the
  earlier unlinked members (ev, cross-arb, rule-tree). Only the cli graph
  dropped it. The set alloc gate measures slot 0 as the hyparb stub.
* **Every slot-0 label follows the ev / cross_arb precedent:** `/state`
  slot name, `audit-pnl` label, `exec_boot` names, the dashboard,
  `regime.toml [labels.hyparb]` (`[labels.latency_arb]` refused at the
  grammar), gauge `engine_strategy_hyparb_active`. `docs/migration.md`
  carries the two entries (slot 0; VenueId 8).

### 16.9 H2 — the AMM fill law — LANDED

As §6 says, with these decisions recorded:

* **The pool state lives in a book, not in a `PoolTouch` the engine
  decodes.** `core_fill::AmmBook` (128 pools × 128 B, const-constructed)
  takes the raw pool-event signals — `observe(sym, payload)` — and holds
  what the judge needs: price / tick / liquidity, the fee in force, the
  last fee a swap was SEEN to pay, spacing, family, decimals. The engine
  forwards every `SignalSource::HyperEvm` signal to
  `OrderDispatch::observe_amm` (defaulted no-op; `PaperDispatcher` and
  `RoutedDispatcher`'s paper arm override) BEFORE `on_signal` — pinned by
  `a_pool_event_reaches_the_dispatcher_before_the_strategy`. The harness
  feeds its own `AmmBook` the same signals (`FillEngine::on_amm_signal`).
  `Pending` is unchanged: the pool index is the sym's ordinal.
* **`SNAPSHOT` carries both tokens' decimals** (`dec0 @23 · dec1 @24`,
  ≤ 36; the fee must be < 100 %), so a replay prices a pool from the tape
  alone. `ingress_hyperevm::PoolEntry` gains `dec0/dec1` (boot-supplied;
  H3b verifies them against `decimals()` on chain).
* **When a swap is judged:** once, at the first `HEAD` at or after
  `emit + one block`, against the pool as the tape left it — i.e. after
  every swap of the block it raced; we fill as if LAST in that block. A
  chain-wide `GAP` cancels every open swap.
* **How:** `core_amm::fill_in_range` (new, beside the walk) — exact fee
  arithmetic, ACTIVE RANGE only, the limit placed on the MARGINAL price
  with the fee folded in (so the average cannot breach it), quantity
  floored, price rounded against us against the reported quantity, a
  breaching dust result refused. Our impact is carried until the chain's
  next `STATE` for that pool.
* **Fee:** the worse of the fee in force and the last one OBSERVED —
  `core_amm::observed_fee_pips` solves it in range from `SWAP` + `STATE`
  (findings 15/20), and an Algebra v1.2 `SwapFee` reports
  `(override ≠ 0 ? override : lastFee) + pluginFee` directly. The member
  (H4) can call the same function.
* **Venue law:** HyperEVM (venue 8) takes `ORDER_KIND_AMM_SWAP = 2` on a
  pool slot and nothing else; no other venue takes a swap — in the
  matcher and in the harness (`tradeable_venue_byte` += 8,
  `TRADEABLE_VENUES` 7). An AMM fill books at 0 bps (the fee is in the
  price). **Gas is not charged by the harness yet** — H6 adds the gas
  ledger (every attempt, filled or reverted).
* Counters: `MatcherCounters.amm_{fills,canceled,partial,not_live}`;
  harness `AmmReplay` (NOT in the frozen schema-1 line, like
  `LifecycleReplay`). `/metrics` mirrors them in H6.
* Gates: `core_amm::fill` 8 unit tests; `core_fill::amm` 10; matcher 8;
  the X1 AMM parity gate `cli/tests/amm_parity.rs` (engine vs harness,
  fills byte-identical + a non-vacuity pin); `paper_replay_parity`'s
  pre-E5 hash unchanged (H2 is additive for every CLOB order); alloc gate
  **67** (the matcher's observe → submit → HEAD judge → pump, 0 B/op);
  fuzz `amm_book` (new — no panic, never `Wait`, a fill within size and
  limit, never on a pool that is not live) and the two codec targets
  re-run after the `SNAPSHOT` change.

### 16.10 H3b — the HyperEVM ingress wired — LANDED

* **Where the pools come from:** `universe.toml [hyperevm] pools =
  ["0x<addr>:<v3|slipstream|algebra>:<dec0>:<dec1>", …]` (≤ 128, append
  only; `pools[i]` = `make_symbol_id(HyperEvm, i+1)` = the `AmmBook`
  slot; descriptor `hyperevm:0x<addr>`, class `Spot` in the Rust + Python
  descriptor law). The universe is the one place symbols are allocated,
  and the ingress captures whether or not the member runs. **The live
  `main` binary refuses an unknown section — `[hyperevm]` never enters the
  live `universe.toml` before the merge; hyparb smokes boot with
  `--universe <copy>`.**
* **Decimals are verified on chain** (the §16.9 promise): every snapshot
  reads `token0()` / `token1()` with the headers and then `decimals()` on
  each TOKEN (the only reads not addressed to the pool); a mismatch fails
  that pool (`SnapCounters::dec_mismatch`). A pool is done when its map is
  AND both decimals agree.
* **Engine:** a dedicated pool lane (`engine::POOL_RING_SIZE` 4,096 —
  snapshots are bursts the ingress flow-controls against it), attached by
  `Engine::set_pool_lane` and drained through the same per-signal path as
  the RPC lane (`dispatch_signal`: latency sample → `observe_amm` for
  HyperEvm sources → `on_signal`).
* **cli:** `spawn_hyperevm` (`spawn_rpc`'s shape + a tap venue byte + an
  `ArchiveDishonest` exit that does not reconnect — O-H15: the member goes
  dark, the engine runs on), `hyperevm_pool_table`, `--hyperevm-path` +
  `HYPEREVM_WS_HOST` (default `rpc.purroofgroup.com`), both-or-nothing
  (else the producer is dropped — the unspawned-venue shape). Capture label
  `hyperevm`; `/state` venue 9; the `engine_ingress_hyperevm_*` family;
  `--raw-tap hyperevm`. Bounds const-asserted across crates (pools 128,
  decimals 36).
* **Live smoke without the engine:** `cli/tests/hyperevm_live_smoke.rs`
  (`#[ignore]`, the MX9 shape — never stops launchd, separate target dir):
  `spawn_hyperevm` against the real chain, the ring drained into an
  `AmmBook`; asserts heads arrive, the book refuses nothing, and every
  pool completes a snapshot (archive probe + decimals).
* **What the first live smoke caught (2026-09-23):** the WHYPE/USDC pool
  has nearly every tick of its 10-grid initialised (~700 `ticks()` reads
  per snapshot). The driver queued up to 248 reads at once; the archive
  endpoint answered them OUT OF ORDER in ~20–36-reply rounds, and the
  first new id that folded onto a still-unanswered id's pending slot
  ended the session ("pending-request slot collision") — 29 reconnects in
  90 s, no snapshot ever completed, nothing published. Fix: at most
  `MAX_READS_IN_FLIGHT` = 20 reads out (the measured batch cap); request
  ids skip a slot a straggler still holds (`alloc_id`, via the new
  `core_net::PendingTable::is_free`) — the straggler completes whenever it
  is answered; a snapshot whose reads go unanswered for `SNAP_STALL_NS`
  (20 s) ends the session. Pinned by a model-node test that answers
  newest-first around a held straggler past a full id wrap. Re-smoke,
  120 s: 0 reconnects, the snapshot live at 12.4 s, 120 heads, 799
  signals, 21 fees observed, the book refused nothing.
* **HL spot appends (§7.7) are DEFERRED to the go-live step**: the live
  engine is armed on mainnet (slot 3, HL) and the appended coins change
  its HL subscription set at the next daily restart; that edit is made
  with its verification (spotMeta indices, the 32-subscription budget,
  `claude-worker fetch` `unresolved=0`) when hyparb enters the live mask.
