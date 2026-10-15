// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The gas bid — spec-gap default **G2**: a priority fee equal to a
//! fixed fraction of the expected net edge, with the attempt's WORST-CASE
//! fee (`max_fee_per_gas × gas_limit`) never above the artifact's
//! `gas_p99_usd_1e6`. A pure function: the arm never decides a price.
//!
//! **HyperEVM burns priority fees** (plan §16.3 finding 22): the tip buys
//! ordering inside a block, not a proposer's favour. What a losing bid
//! looks like on the chain is measured in H8, not assumed here.
//!
//! Units: USD × 1e6 (the engine's fixed point), HYPE at `hype_usd_1e6`
//! per whole HYPE, wei = 1e-18 HYPE.

/// G2's fixed fraction of the expected net edge bid as the tip, × 1e6
/// (250 000 = a quarter of the edge).
pub const BID_EDGE_FRACTION_1E6: i64 = 250_000;
/// Gas limit of one executor swap: a V3 / Algebra swap, the callback's
/// pool check and one token transfer each way — about 2× what the fork
/// tests spend on the deepest-crossing case, well under the 3M block.
pub const SWAP_GAS_LIMIT: u64 = 400_000;
/// Base-fee headroom: the fee cap covers this many times the next
/// block's base fee (EIP-1559 lets it rise 12.5 % per full block).
pub const BASE_FEE_HEADROOM: u128 = 2;
/// Wei per HYPE.
pub const WEI_PER_HYPE: u128 = 1_000_000_000_000_000_000;

/// A dynamic-fee bid, per gas.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GasBid {
    /// Tip per gas, wei.
    pub max_priority_fee_per_gas: u128,
    /// Fee cap per gas, wei (`headroom × base + tip`).
    pub max_fee_per_gas: u128,
}

/// Why no bid was produced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GasRefusal {
    /// No HYPE price, no cap, or a zero gas limit.
    NoPrice,
    /// The base fee alone (with its headroom) already exceeds what the
    /// p99 cap allows per gas: the chain is too expensive to attempt.
    BaseFeeOverCap {
        /// The next block's base fee, wei.
        base_fee_wei: u128,
        /// What the cap allows per gas, wei.
        cap_per_gas_wei: u128,
    },
}

/// USD × 1e6 → wei at `hype_usd_1e6` per HYPE (floor). Non-positive
/// inputs are zero.
#[inline]
#[must_use]
pub const fn usd_to_wei(usd_1e6: i64, hype_usd_1e6: i64) -> u128 {
    if usd_1e6 <= 0 || hype_usd_1e6 <= 0 {
        return 0;
    }
    (usd_1e6 as u128).saturating_mul(WEI_PER_HYPE) / hype_usd_1e6 as u128
}

/// Wei → USD × 1e6 at `hype_usd_1e6` per HYPE (floor, saturating).
#[inline]
#[must_use]
pub const fn wei_to_usd_1e6(wei: u128, hype_usd_1e6: i64) -> i64 {
    if hype_usd_1e6 <= 0 {
        return 0;
    }
    let v = wei.saturating_mul(hype_usd_1e6 as u128) / WEI_PER_HYPE;
    if v > i64::MAX as u128 {
        i64::MAX
    } else {
        v as i64
    }
}

/// The G2 bid for one attempt.
///
/// * `edge_usd_1e6` — the decision's expected net edge (the solver's
///   P&L after fees and p50 gas); non-positive bids no tip;
/// * `base_fee_wei` — the NEXT block's base fee (`eth_feeHistory`);
/// * `gas_limit` — the attempt's limit ([`SWAP_GAS_LIMIT`] for a swap);
/// * `cap_usd_1e6` — `gas_p99_usd_1e6`: the worst-case fee cap.
pub const fn bid(
    edge_usd_1e6: i64,
    base_fee_wei: u128,
    gas_limit: u64,
    hype_usd_1e6: i64,
    cap_usd_1e6: i64,
) -> Result<GasBid, GasRefusal> {
    if hype_usd_1e6 <= 0 || cap_usd_1e6 <= 0 || gas_limit == 0 {
        return Err(GasRefusal::NoPrice);
    }
    let cap_per_gas = usd_to_wei(cap_usd_1e6, hype_usd_1e6) / gas_limit as u128;
    let headroom = base_fee_wei.saturating_mul(BASE_FEE_HEADROOM);
    if headroom >= cap_per_gas {
        return Err(GasRefusal::BaseFeeOverCap {
            base_fee_wei,
            cap_per_gas_wei: cap_per_gas,
        });
    }
    let tip_usd = if edge_usd_1e6 > 0 {
        (edge_usd_1e6 as i128 * BID_EDGE_FRACTION_1E6 as i128 / 1_000_000) as i64
    } else {
        0
    };
    let tip = usd_to_wei(tip_usd, hype_usd_1e6) / gas_limit as u128;
    let room = cap_per_gas - headroom;
    let prio = if tip < room { tip } else { room };
    Ok(GasBid {
        max_priority_fee_per_gas: prio,
        max_fee_per_gas: headroom + prio,
    })
}

/// What a mined attempt paid, wei (`gasUsed × effectiveGasPrice`).
#[inline]
#[must_use]
pub const fn fee_paid_wei(gas_used: u64, effective_gas_price: u128) -> u128 {
    (gas_used as u128).saturating_mul(effective_gas_price)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HYPE: i64 = 40_000_000; // $40
    const GWEI: u128 = 1_000_000_000;

    #[test]
    fn usd_and_wei_convert_both_ways() {
        assert_eq!(
            usd_to_wei(40_000_000, HYPE),
            WEI_PER_HYPE,
            "$40 is one HYPE"
        );
        assert_eq!(usd_to_wei(0, HYPE), 0);
        assert_eq!(usd_to_wei(-5, HYPE), 0);
        assert_eq!(usd_to_wei(5, 0), 0);
        assert_eq!(wei_to_usd_1e6(WEI_PER_HYPE, HYPE), 40_000_000);
        assert_eq!(wei_to_usd_1e6(u128::MAX, HYPE), i64::MAX, "saturates");
        assert_eq!(fee_paid_wei(21_000, GWEI), 21_000 * GWEI);
    }

    #[test]
    fn the_tip_is_a_quarter_of_the_edge_spread_over_the_gas() {
        // Edge $1 → tip $0.25 over 400k gas at $40: 0.00625 HYPE / 4e5.
        let b = bid(1_000_000, GWEI / 10, SWAP_GAS_LIMIT, HYPE, 3_910_000).unwrap();
        let tip_wei_total = usd_to_wei(250_000, HYPE);
        assert_eq!(b.max_priority_fee_per_gas, tip_wei_total / 400_000);
        assert_eq!(
            b.max_fee_per_gas,
            2 * (GWEI / 10) + b.max_priority_fee_per_gas
        );
        // The worst case stays under the cap.
        let worst = b.max_fee_per_gas * SWAP_GAS_LIMIT as u128;
        assert!(worst <= usd_to_wei(3_910_000, HYPE));
    }

    #[test]
    fn a_big_edge_is_capped_at_p99_never_above() {
        let cap = 3_910_000;
        let b = bid(1_000_000_000_000, GWEI / 10, SWAP_GAS_LIMIT, HYPE, cap).unwrap();
        let cap_per_gas = usd_to_wei(cap, HYPE) / SWAP_GAS_LIMIT as u128;
        assert_eq!(b.max_fee_per_gas, cap_per_gas, "the fee cap IS the p99 cap");
        assert!(b.max_fee_per_gas * SWAP_GAS_LIMIT as u128 <= usd_to_wei(cap, HYPE));
    }

    #[test]
    fn no_edge_bids_the_base_fee_only() {
        let b = bid(-5, GWEI, SWAP_GAS_LIMIT, HYPE, 3_910_000).unwrap();
        assert_eq!(b.max_priority_fee_per_gas, 0);
        assert_eq!(b.max_fee_per_gas, 2 * GWEI);
    }

    #[test]
    fn a_base_fee_above_the_cap_refuses() {
        // $0.01 cap over 400k gas at $40 = 625 gwei per gas... use a
        // base fee far above it.
        let e = bid(1_000_000, 1_000_000 * GWEI, SWAP_GAS_LIMIT, HYPE, 10_000).unwrap_err();
        assert!(matches!(e, GasRefusal::BaseFeeOverCap { .. }), "{e:?}");
        assert_eq!(bid(1, GWEI, SWAP_GAS_LIMIT, 0, 1), Err(GasRefusal::NoPrice));
        assert_eq!(bid(1, GWEI, 0, HYPE, 1), Err(GasRefusal::NoPrice));
        assert_eq!(
            bid(1, GWEI, SWAP_GAS_LIMIT, HYPE, 0),
            Err(GasRefusal::NoPrice)
        );
    }
}
