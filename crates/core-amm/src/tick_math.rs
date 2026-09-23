// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Tick ↔ `sqrtPriceX96`, bit-exact with Uniswap V3 `TickMath`.
//!
//! **No floats (law 3).** `sqrt_at_tick` is the integer ladder of 20
//! 128.128 fixed-point constants, `tick_at_sqrt` the 14-iteration
//! binary logarithm. An `exp()`/`log()` path cannot reproduce on-chain
//! amounts to the replay gate's tolerance; this one reproduces them to
//! the wei.

use crate::u256::U256;

/// The minimum tick a V3 pool can reach (`log_1.0001(2^-128)`).
pub const MIN_TICK: i32 = -887_272;
/// The maximum tick a V3 pool can reach (`log_1.0001(2^128)`).
pub const MAX_TICK: i32 = 887_272;

/// `sqrt_at_tick(MIN_TICK)`, low 128 bits (the high word is 0).
pub const MIN_SQRT_LO: u128 = 4_295_128_739;
/// `sqrt_at_tick(MAX_TICK)` = `0xfffd8963efd1fc6a506488495d951d5263988d26`,
/// low 128 bits.
pub const MAX_SQRT_LO: u128 = 318_775_800_626_314_356_294_205_765_087_544_249_638;
/// `sqrt_at_tick(MAX_TICK)`, high 32 bits.
pub const MAX_SQRT_HI: u32 = 4_294_805_859;

pub(crate) const MIN_SQRT: U256 = U256::from_u128(MIN_SQRT_LO);
pub(crate) const MAX_SQRT: U256 = U256::from_u160(MAX_SQRT_LO, MAX_SQRT_HI);

/// `(a · c) >> 128` for `a, c < 2^128` — the high half of the product.
#[inline(always)]
const fn mul_shr128(a: u128, c: u128) -> u128 {
    U256::mul_u128(a, c).hi
}

/// `sqrt(1.0001^tick) · 2^96` as a `U256`, exactly `TickMath.getSqrtRatioAtTick`.
/// `tick` is clamped into `[MIN_TICK, MAX_TICK]` (Solidity reverts; the
/// caller never passes one outside, and a debug build says so).
pub(crate) const fn sqrt_ratio_at_tick(tick: i32) -> U256 {
    debug_assert!(tick >= MIN_TICK && tick <= MAX_TICK);
    let t = if tick < MIN_TICK {
        MIN_TICK
    } else if tick > MAX_TICK {
        MAX_TICK
    } else {
        tick
    };
    let abs = t.unsigned_abs();
    // `one` marks the exact 2^128 start, which does not fit u128: its
    // first multiplication by `c` yields `c` itself.
    let mut one = abs & 0x1 == 0;
    let mut r: u128 = 0xfffc_b933_bd6f_ad37_aa2d_162d_1a59_4001;
    macro_rules! step {
        ($mask:expr, $c:expr) => {
            if abs & $mask != 0 {
                r = if one { $c } else { mul_shr128(r, $c) };
                one = false;
            }
        };
    }
    step!(0x2, 0xfff9_7272_373d_4132_59a4_6990_580e_213a);
    step!(0x4, 0xfff2_e50f_5f65_6932_ef12_357c_f3c7_fdcc);
    step!(0x8, 0xffe5_caca_7e10_e4e6_1c36_24ea_a094_1cd0);
    step!(0x10, 0xffcb_9843_d60f_6159_c9db_5883_5c92_6644);
    step!(0x20, 0xff97_3b41_fa98_c081_472e_6896_dfb2_54c0);
    step!(0x40, 0xff2e_a164_66c9_6a38_43ec_78b3_26b5_2861);
    step!(0x80, 0xfe5d_ee04_6a99_a2a8_11c4_61f1_969c_3053);
    step!(0x100, 0xfcbe_86c7_900a_88ae_dcff_c83b_479a_a3a4);
    step!(0x200, 0xf987_a725_3ac4_1317_6f2b_074c_f781_5e54);
    step!(0x400, 0xf339_2b08_22b7_0005_940c_7a39_8e4b_70f3);
    step!(0x800, 0xe715_9475_a2c2_9b74_43b2_9c7f_a6e8_89d9);
    step!(0x1000, 0xd097_f3bd_fd20_22b8_845a_d8f7_92aa_5825);
    step!(0x2000, 0xa9f7_4646_2d87_0fdf_8a65_dc1f_90e0_61e5);
    step!(0x4000, 0x70d8_69a1_56d2_a1b8_90bb_3df6_2baf_32f7);
    step!(0x8000, 0x31be_135f_97d0_8fd9_8123_1505_542f_cfa6);
    step!(0x10000, 0x09aa_508b_5b7a_84e1_c677_de54_f3e9_9bc9);
    step!(0x20000, 0x5d_6af8_dedb_8119_6699_c329_225e_e604);
    step!(0x40000, 0x2216_e584_f5fa_1ea9_2604_1bed_fe98);
    step!(0x80000, 0x48a_1703_91f7_dc42_444e_8fa2);
    let ratio = if one { U256::pow2(128) } else { U256::from_u128(r) };
    let ratio = if t > 0 {
        match U256::MAX.checked_div_rem(ratio) {
            Some((q, _)) => q,
            None => U256::ZERO, // ratio is never zero
        }
    } else {
        ratio
    };
    // Round UP to 160 bits so the result is never below the true ratio.
    let s = ratio.shr(32);
    if ratio.lo as u32 == 0 {
        s
    } else {
        s.wrapping_add(U256::ONE)
    }
}

/// `255738958999603826347141` — `2^64 / log2(sqrt(1.0001))` in 128.128.
const LOG_SQRT_10001: u128 = 255_738_958_999_603_826_347_141;
/// Lower error bound of the log estimate, 128.128.
const TICK_LOW_ERR: u128 = 3_402_992_956_809_132_418_596_140_100_660_247_210;
/// Upper error bound of the log estimate, 128.128.
const TICK_HI_ERR: u128 = 291_339_464_771_989_622_907_027_621_153_398_088_495;

/// Greatest tick whose sqrt ratio is `<= sqrt_price`, exactly
/// `TickMath.getTickAtSqrtRatio`. Out-of-domain input (Solidity reverts)
/// saturates to the nearest valid tick.
pub(crate) const fn tick_at_sqrt_ratio(sqrt_price: U256) -> i32 {
    if sqrt_price.lt(MIN_SQRT) {
        return MIN_TICK;
    }
    if MAX_SQRT.le(sqrt_price) {
        return MAX_TICK - 1;
    }
    let ratio = sqrt_price.shl(32);
    let msb = ratio.msb();
    // Normalise r into [2^127, 2^128).
    let r0 = if msb >= 128 { ratio.shr(msb - 127) } else { ratio.shl(127 - msb) };
    let mut r: u128 = r0.lo;
    let mut log_2: i128 = ((msb as i128) - 128) << 64;
    let mut bit: u32 = 63;
    while bit >= 50 {
        // r := (r·r) >> 127, which is < 2^129; f is its bit 128.
        let sq = U256::mul_u128(r, r).shr(127);
        let f = sq.hi as u32; // 0 or 1
        log_2 |= (f as i128) << bit;
        r = if f == 1 { sq.shr(1).lo } else { sq.lo };
        bit -= 1;
    }
    // log_sqrt10001 = log_2 · K as a two's-complement int256.
    let mag = U256::mul_u128(log_2.unsigned_abs(), LOG_SQRT_10001);
    let log_sqrt = if log_2 < 0 { mag.wrapping_neg() } else { mag };
    // Arithmetic `>> 128` of an int256 is its high 128 bits read as i128.
    let tick_low = log_sqrt.wrapping_sub(U256::from_u128(TICK_LOW_ERR)).hi as i128 as i32;
    let tick_hi = log_sqrt.wrapping_add(U256::from_u128(TICK_HI_ERR)).hi as i128 as i32;
    if tick_low == tick_hi {
        tick_low
    } else {
        let s = sqrt_ratio_at_tick(tick_hi);
        if s.le(sqrt_price) {
            tick_hi
        } else {
            tick_low
        }
    }
}

/// `sqrt(1.0001^tick) · 2^96` as `(low 128 bits, high 32 bits)` of the
/// `uint160` — bit-exact with `TickMath.getSqrtRatioAtTick`. A tick
/// outside `[MIN_TICK, MAX_TICK]` is clamped.
#[must_use]
pub fn sqrt_at_tick(tick: i32) -> (u128, u32) {
    let s = sqrt_ratio_at_tick(tick);
    (s.lo, s.hi as u32)
}

/// The greatest tick whose sqrt ratio is `<=` the given `uint160`
/// `(lo, hi)` — bit-exact with `TickMath.getTickAtSqrtRatio`. Below
/// `MIN_SQRT` returns `MIN_TICK`; at or above `MAX_SQRT` returns
/// `MAX_TICK − 1` (Solidity reverts on both).
#[must_use]
pub fn tick_at_sqrt(lo: u128, hi: u32) -> i32 {
    tick_at_sqrt_ratio(U256::from_u160(lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn le(a: U256, b: U256) -> bool {
        a <= b
    }

    #[test]
    fn endpoints_are_the_published_constants() {
        assert_eq!(sqrt_ratio_at_tick(MIN_TICK), MIN_SQRT);
        assert_eq!(sqrt_ratio_at_tick(MAX_TICK), MAX_SQRT);
        assert_eq!(sqrt_ratio_at_tick(0), U256::pow2(96));
        assert_eq!(tick_at_sqrt_ratio(MIN_SQRT), MIN_TICK);
        assert_eq!(tick_at_sqrt_ratio(MAX_SQRT.wrapping_sub(U256::ONE)), MAX_TICK - 1);
        assert_eq!(tick_at_sqrt_ratio(U256::pow2(96)), 0);
    }

    /// Published Uniswap V3 test vector (`TickMath.spec.ts`), and the
    /// true value 4295343489.19… rounded UP, as the contract promises.
    #[test]
    fn published_vector_min_plus_one() {
        assert_eq!(sqrt_ratio_at_tick(MIN_TICK + 1), U256::from_u128(4_295_343_490));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50_000))]

        /// The two functions are mutual inverses at every tick boundary.
        #[test]
        fn tick_sqrt_round_trip(t in MIN_TICK..MAX_TICK) {
            let s = sqrt_ratio_at_tick(t);
            prop_assert_eq!(tick_at_sqrt_ratio(s), t);
            if t > MIN_TICK {
                prop_assert_eq!(tick_at_sqrt_ratio(s.wrapping_sub(U256::ONE)), t - 1);
            }
            prop_assert!(le(s, sqrt_ratio_at_tick(t + 1)));
        }

        /// Monotone and strictly increasing.
        #[test]
        fn strictly_increasing(t in MIN_TICK..MAX_TICK) {
            prop_assert!(sqrt_ratio_at_tick(t) < sqrt_ratio_at_tick(t + 1));
        }

        /// Any price maps to a tick whose bracket contains it.
        #[test]
        fn bracket(lo in any::<u128>(), hi in 0u32..=MAX_SQRT_HI) {
            let p = U256::from_u160(lo, hi);
            prop_assume!(p >= MIN_SQRT && p < MAX_SQRT);
            let t = tick_at_sqrt_ratio(p);
            prop_assert!(sqrt_ratio_at_tick(t) <= p);
            prop_assert!(p < sqrt_ratio_at_tick(t + 1));
        }
    }
}
