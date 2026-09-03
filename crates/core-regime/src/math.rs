// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integer primitives shared by the regime law, the ICDP feature law
//! and the VM feature engine (RG1 relocation — one home, no
//! duplicates). Every function is allocation-free, panic-free in
//! release, and bit-exact against the Python reference
//! (`claude_worker.regime`).

/// Floor division on `i128` (toward −∞), the Python `//` law.
#[inline(always)]
pub const fn floor_div(n: i128, d: i128) -> i128 {
    n.div_euclid(d)
}

/// Return of `to` over `from` in bps ×1e9: `((to − from) × 1e13) / from`,
/// floored. `from` must be > 0 (debug-asserted; release returns 0 so
/// a corrupt sample can never divide by zero).
#[inline(always)]
pub fn ret_bps_1e9(from_1e6: i64, to_1e6: i64) -> i64 {
    debug_assert!(from_1e6 > 0);
    if from_1e6 <= 0 {
        return 0;
    }
    floor_div(
        (to_1e6 as i128 - from_1e6 as i128) * 10_000_000_000_000,
        from_1e6 as i128,
    ) as i64
}

/// Integer square root (floor) of a non-negative `i128`, returned as
/// `i64` (saturating — inputs are sums of squared bps×1e9 returns,
/// whose root fits easily). Newton's method from `v / 2 + 1`, which
/// converges to the exact floor from any start ≥ the root — so
/// Python's `math.isqrt` is bit-identical without sharing the
/// iteration.
#[inline]
pub const fn isqrt_i128(v: i128) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut y = (x >> 1) + 1;
    while y < x {
        x = y;
        y = (x + v / x) >> 1;
    }
    if x > i64::MAX as i128 {
        i64::MAX
    } else {
        x as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ret_bps_is_floored_and_zero_safe() {
        assert_eq!(ret_bps_1e9(100_000_000, 101_000_000), 100_000_000_000); // +1 % = 100 bps
        assert_eq!(ret_bps_1e9(100_000_000, 99_000_000), -100_000_000_000);
        assert_eq!(ret_bps_1e9(100_000_000, 100_000_000), 0);
        // Floor toward −∞ (Python `//`): −1/3 bps ×1e9 rounds down.
        assert_eq!(ret_bps_1e9(300_000_000, 299_999_999), -33_334);
        assert_eq!(ret_bps_1e9(300_000_000, 300_000_001), 33_333);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn ret_bps_release_never_divides_by_zero() {
        assert_eq!(ret_bps_1e9(0, 5), 0);
    }

    #[test]
    fn isqrt_matches_exact_squares_and_floors_between() {
        assert_eq!(isqrt_i128(0), 0);
        assert_eq!(isqrt_i128(-7), 0);
        assert_eq!(isqrt_i128(1), 1);
        assert_eq!(isqrt_i128(15), 3);
        assert_eq!(isqrt_i128(16), 4);
        assert_eq!(isqrt_i128(17), 4);
        let big: i128 = (1i128 << 100) + 12345;
        let r = isqrt_i128(big) as i128;
        assert!(r * r <= big && (r + 1) * (r + 1) > big);
        assert_eq!(isqrt_i128(i128::MAX), i64::MAX);
    }
}
