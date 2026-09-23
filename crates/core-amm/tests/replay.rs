// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **THE H1 / O-H19 GATES.** Replay real HyperEVM mainnet swaps through
//! the walk and require the chain's own numbers back — one gate per pool
//! family, each against its own fixture:
//!
//! | gate | fixture | loop |
//! |---|---|---|
//! | Uniswap V3 ABI (H1) | `tests/data/amm-replay.tsv` | V3 |
//! | Slipstream (Hybra CL) | `tests/data/slipstream-replay.tsv` | V3 |
//! | Algebra Integral v1.0 + v1.2 (NEST, Kittenswap) | `tests/data/algebra-replay.tsv` | Algebra |
//!
//! Each row is the FIRST swap of a (pool, block) with no earlier
//! Swap/Mint/Burn of that pool in the block, across disjoint ≤ 2 h
//! windows, with the EXACT pre-state read from an honest archive at
//! `block − 1`, the event's post-state and amounts, and every initialised
//! tick over the traversed interval (liquidityNet decoded from 256 bits;
//! for Algebra, the coverage is bracketed by initialised ticks walked off
//! the pool's linked list, and the fee is the one in force for that swap
//! — its `Fee` / `SwapFee` event, else `lastFee`). Built by the vault
//! scripts under `docs/research/hyparb/fixture/`.
//!
//! What is required, per row:
//!
//! 1. `tick_at_sqrt(pre_sqrt)` agrees with the pool's own `slot0.tick`
//!    (or sits exactly on a boundary the pool crossed downward).
//! 2. **Bit-exact replay.** The swap re-executed from the pre-state —
//!    exact-input with the event's input, else exact-output with its
//!    output, else to the event's own post-price — reproduces the
//!    event's input, output, post-price, post-liquidity and post-tick
//!    EXACTLY, under the fixture's fee (`fee()` at `block − 1`; for
//!    Algebra, the fee in force).
//!
//!    Some HyperEVM pools charge a DYNAMIC fee that `fee()` does not
//!    report (measured 2026-09-23: 9 of 34 Uniswap-ABI pools). For those
//!    rows the effective fee is solved from the swap itself — the pre-fee
//!    input to the event's post-price against the event's gross input —
//!    and the replay must then be bit-exact under a fee within ±3 pips
//!    of that solve. One integer that makes five independent quantities
//!    agree to the wei is not a coincidence.
//!
//!    Required: exact (either way) for ≥ 99 % of rows. The remainder is
//!    the intra-block state a first-swap-of-block row cannot see (a mint
//!    or burn earlier in the same block); it is counted and printed.
//! 3. **The pre-fee walk** (`swap_to_target` to the event's post-price)
//!    returns the event's output (to per-step rounding dust) on every
//!    exactly-replayed row,
//!    and an input that differs from the event's by no more than the
//!    pool fee — the spec's ≤ 0.05 % median bar, generalised to every
//!    fee tier in the fixture.

use core_amm::{
    sqrt_at_tick, swap_exact, swap_to_target, tick_at_sqrt, PoolMeta, PoolState, SwapSpec, TickMap,
    TickNode, AMM_KIND_ALGEBRA, AMM_KIND_V3, MAX_SQRT_HI, MAX_SQRT_LO, MIN_SQRT_LO,
};

const UNISWAP: &str = include_str!("data/amm-replay.tsv");
const SLIPSTREAM: &str = include_str!("data/slipstream-replay.tsv");
const ALGEBRA: &str = include_str!("data/algebra-replay.tsv");

/// Decimal string → `uint160` as `(lo, hi)`. Test-only parsing.
fn u160(s: &str) -> (u128, u32) {
    let mut limbs = [0u64; 3];
    for b in s.bytes() {
        assert!(b.is_ascii_digit(), "bad digit in {s}");
        let mut carry = (b - b'0') as u128;
        for l in limbs.iter_mut() {
            let v = (*l as u128) * 10 + carry;
            *l = v as u64;
            carry = v >> 64;
        }
        assert_eq!(carry, 0, "{s} overflows 192 bits");
    }
    let lo = (limbs[0] as u128) | ((limbs[1] as u128) << 64);
    assert!(limbs[2] <= u32::MAX as u64, "{s} exceeds uint160");
    (lo, limbs[2] as u32)
}

struct Row {
    block: u64,
    pool: u32,
    fee: u32,
    spacing: i32,
    pre: PoolState,
    post: (u128, u32, i32, u128),
    a0: i128,
    a1: i128,
    lo: i32,
    hi: i32,
    ticks: Vec<TickNode>,
}

fn rows(fixture: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for line in fixture.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 16, "bad row: {line}");
        let (plo, phi) = u160(f[5]);
        let (qlo, qhi) = u160(f[8]);
        let ticks = if f[15] == "-" {
            Vec::new()
        } else {
            f[15]
                .split(',')
                .map(|p| {
                    let (t, n) = p.split_once(':').unwrap();
                    TickNode::new(t.parse().unwrap(), n.parse().unwrap())
                })
                .collect()
        };
        out.push(Row {
            pool: f[0].parse().unwrap(),
            block: f[1].parse().unwrap(),
            fee: f[3].parse().unwrap(),
            spacing: f[4].parse().unwrap(),
            pre: PoolState::new(plo, phi, f[6].parse().unwrap(), f[7].parse().unwrap()),
            post: (qlo, qhi, f[9].parse().unwrap(), f[10].parse().unwrap()),
            a0: f[11].parse().unwrap(),
            a1: f[12].parse().unwrap(),
            lo: f[13].parse().unwrap(),
            hi: f[14].parse().unwrap(),
            ticks,
        });
    }
    out
}

fn state_matches(after: &PoolState, post: &(u128, u32, i32, u128)) -> bool {
    after.sqrt_price_lo == post.0
        && after.sqrt_price_hi == post.1
        && after.tick == post.2
        && after.liquidity == post.3
}

/// The first of `tries` that reproduces the event bit-exactly, if any.
fn replay(
    pre: &PoolState,
    meta: &PoolMeta,
    map: &TickMap<1024>,
    tries: &[SwapSpec; 3],
    amt_in: u128,
    amt_out: u128,
    post: &(u128, u32, i32, u128),
) -> Option<usize> {
    let mut k = 0;
    while k < tries.len() {
        let res = swap_exact(pre, meta, map, &tries[k]);
        if res.amount_in == amt_in && res.amount_out == amt_out && state_matches(&res.after, post) {
            return Some(k);
        }
        k += 1;
    }
    None
}

#[test]
fn replay_gate_uniswap_v3() {
    gate("uniswap-v3", UNISWAP, AMM_KIND_V3, 5_000);
}

#[test]
fn replay_gate_slipstream() {
    gate("slipstream", SLIPSTREAM, AMM_KIND_V3, 1_000);
}

#[test]
fn replay_gate_algebra() {
    gate("algebra", ALGEBRA, AMM_KIND_ALGEBRA, 1_000);
}

/// One family's gate: `min_rows` real swaps, ≥ 99 % bit-exact.
fn gate(family: &str, fixture: &str, kind: u8, min_rows: usize) {
    let rows = rows(fixture);
    assert!(
        rows.len() >= min_rows,
        "{family}: the gate needs >= {min_rows} real swaps, fixture has {}",
        rows.len()
    );
    let mut map = Box::new(TickMap::<1024>::EMPTY);
    let mut exact = 0usize;
    let mut by_mode = [0usize; 3];
    let mut crossed_rows = 0usize;
    let mut dynamic_fee = 0usize;
    let mut misses: Vec<String> = Vec::new();
    let mut prefee_gap_ppb: Vec<u128> = Vec::new(); // fee paid / input, parts per billion
    let mut pools = std::collections::BTreeSet::new();
    for r in &rows {
        pools.insert(r.pool);
        // (1) tick ↔ price agreement with the pool's own slot0.
        let t = tick_at_sqrt(r.pre.sqrt_price_lo, r.pre.sqrt_price_hi);
        let on_boundary =
            t == r.pre.tick + 1 && sqrt_at_tick(t) == (r.pre.sqrt_price_lo, r.pre.sqrt_price_hi);
        assert!(
            t == r.pre.tick || on_boundary,
            "block {} pool {}: tick_at_sqrt {} vs slot0 {}",
            r.block,
            r.pool,
            t,
            r.pre.tick
        );

        map.load(&r.ticks, r.lo, r.hi, r.spacing)
            .unwrap_or_else(|e| panic!("block {} pool {}: map {e}", r.block, r.pool));
        let mut meta = PoolMeta::ZERO;
        meta.kind = kind;
        meta.tick_spacing = r.spacing;
        meta.fee_pips = r.fee;
        let zero_for_one = r.a0 > 0;
        let (amt_in, amt_out) = if zero_for_one {
            (r.a0, -r.a1)
        } else {
            (r.a1, -r.a0)
        };
        if amt_in <= 0 || amt_out < 0 {
            misses.push(format!(
                "block {} pool {}: degenerate amounts {} {}",
                r.block, r.pool, r.a0, r.a1
            ));
            continue;
        }
        if r.pre.liquidity != r.post.3 {
            crossed_rows += 1;
        }
        let (amt_in, amt_out) = (amt_in as u128, amt_out as u128);
        let (lim_lo, lim_hi) = if zero_for_one {
            (MIN_SQRT_LO + 1, 0)
        } else {
            (MAX_SQRT_LO - 1, MAX_SQRT_HI)
        };
        // (2) bit-exact replay, three ways the router could have asked.
        let tries = [
            SwapSpec {
                amount: amt_in,
                limit_lo: lim_lo,
                limit_hi: lim_hi,
                fee_pips: r.fee,
                zero_for_one,
                exact_in: true,
            },
            SwapSpec {
                amount: amt_out,
                limit_lo: lim_lo,
                limit_hi: lim_hi,
                fee_pips: r.fee,
                zero_for_one,
                exact_in: false,
            },
            SwapSpec {
                amount: amt_in,
                limit_lo: r.post.0,
                limit_hi: r.post.1,
                fee_pips: r.fee,
                zero_for_one,
                exact_in: true,
            },
        ];
        let mut hit = replay(&r.pre, &meta, &map, &tries, amt_in, amt_out, &r.post);
        if hit.is_none() {
            // Dynamic-fee pool: solve the effective fee from the swap itself.
            let (t0, t1, _) = swap_to_target(
                &r.pre,
                &meta,
                &map,
                r.post.0,
                r.post.1,
                u128::MAX,
                !zero_for_one,
            );
            let pf_in = if zero_for_one { t0 } else { t1 };
            if pf_in > 0 && pf_in <= amt_in {
                let f0 = ((amt_in - pf_in).saturating_mul(1_000_000) / amt_in) as i64;
                let mut f = (f0 - 3).max(0);
                while f <= f0 + 3 && f < 1_000_000 {
                    let mut alt = tries;
                    let mut j = 0;
                    while j < alt.len() {
                        alt[j].fee_pips = f as u32;
                        j += 1;
                    }
                    if let Some(k) = replay(&r.pre, &meta, &map, &alt, amt_in, amt_out, &r.post) {
                        hit = Some(k);
                        dynamic_fee += 1;
                        meta.fee_pips = f as u32;
                        break;
                    }
                    f += 1;
                }
            }
        }
        match hit {
            Some(k) => {
                exact += 1;
                by_mode[k] += 1;
                // (3) the pre-fee walk to the same post-price.
                let (t0, t1, after) = swap_to_target(
                    &r.pre,
                    &meta,
                    &map,
                    r.post.0,
                    r.post.1,
                    u128::MAX,
                    !zero_for_one,
                );
                let (pf_in, pf_out) = if zero_for_one { (t0, t1) } else { (t1, t0) };
                assert_eq!(
                    (after.sqrt_price_lo, after.sqrt_price_hi),
                    (r.post.0, r.post.1),
                    "block {}: pre-fee walk missed the post price",
                    r.block
                );
                // Exact-output swaps cap the last step's output at the request, so
                // the uncapped pre-fee walk may exceed it by per-step rounding dust.
                assert!(
                    pf_out >= amt_out && pf_out - amt_out <= 16,
                    "block {} pool {}: pre-fee output {} vs event {}",
                    r.block,
                    r.pool,
                    pf_out,
                    amt_out
                );
                assert!(
                    pf_in <= amt_in,
                    "block {}: pre-fee input exceeds the event's",
                    r.block
                );
                // fee-paid fraction must not exceed the pool fee (+1 wei per step of rounding)
                let paid = amt_in - pf_in;
                let bound = amt_in * meta.fee_pips as u128 / 1_000_000 + 1 + 16;
                assert!(
                    paid <= bound,
                    "block {} pool {}: pre-fee gap {} > fee bound {}",
                    r.block,
                    r.pool,
                    paid,
                    bound
                );
                prefee_gap_ppb.push(paid.saturating_mul(1_000_000_000) / amt_in);
            }
            None => {
                let res = swap_exact(&r.pre, &meta, &map, &tries[0]);
                misses.push(format!(
                    "block {} pool {} fee {} z4o {}: event in {} out {} post ({},{},{}) | replay in {} out {} post ({},{},{}) flags {}",
                    r.block, r.pool, r.fee, zero_for_one, amt_in, amt_out, r.post.0, r.post.2, r.post.3,
                    res.amount_in, res.amount_out, res.after.sqrt_price_lo, res.after.tick, res.after.liquidity, res.flags
                ));
            }
        }
    }
    prefee_gap_ppb.sort_unstable();
    let med_ppb = prefee_gap_ppb
        .get(prefee_gap_ppb.len() / 2)
        .copied()
        .unwrap_or(0);
    let rate_bp = exact * 10_000 / rows.len();
    println!(
        "replay {family}: rows {} pools {} exact {} ({}.{:02} %) [exact-in {} exact-out {} to-price {}; under a solved dynamic fee {}] rows-with-liquidity-change {} misses {} | pre-fee input gap median {} ppb",
        rows.len(), pools.len(), exact, rate_bp / 100, rate_bp % 100, by_mode[0], by_mode[1], by_mode[2], dynamic_fee, crossed_rows, misses.len(), med_ppb
    );
    for m in misses.iter().take(20) {
        println!("  MISS {family} {m}");
    }
    assert!(
        exact * 100 >= rows.len() * 99,
        "{family}: bit-exact replay rate {}.{:02} % < 99 %",
        rate_bp / 100,
        rate_bp % 100
    );
}
