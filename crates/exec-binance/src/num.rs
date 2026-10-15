// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The numeric law (BX-5, plan §3.6).**
//!
//! Every price and quantity is an integer ×1e6. A row's tick and step are
//! exact multiples of 1e-6 (a row where they are not is refused at boot,
//! never rounded), so quantization is integer arithmetic:
//!
//! * a BUY price FLOORS to the tick, a SELL price CEILS to it — never a
//!   worse price than the member asked for;
//! * a quantity FLOORS to the step — never more than the member asked for.
//!
//! **No hardware divide on the submit path.** Each row carries a
//! [`Magic`] reciprocal per divisor: `⌊x / d⌋ = (x · m) >> (63 + l)` with
//! `l = ⌈log₂ d⌉` and `m = ⌈2^(63+l) / d⌉`, exact for every `0 ≤ x < 2^63`
//! (the error term `x·(m·d − 2^k) / (d·2^k)` stays below `1/d`). The
//! property test pins it against `/` over the whole domain.
//!
//! **The renderer** writes a non-negative ×1e6 value with a fixed number of
//! decimals straight into the request window — two digits per step from a
//! 200-byte table, no intermediate string. Its length is known before it
//! is written ([`u64_len`], [`fixed_len`]), so the frame's header goes
//! first and the digits are rendered in the frame itself
//! ([`crate::wsapi::Part`]).

/// A magic reciprocal: exact `⌊x / d⌋` for `0 ≤ x < 2^63`, `1 ≤ d ≤ 2^32 − 1`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Magic {
    m: u64,
    sh: u8,
}

impl Magic {
    /// The reciprocal of `d` (≥ 1). Boot-only; `d = 0` is refused by the
    /// row builder before this runs.
    #[must_use]
    pub const fn new(d: u32) -> Self {
        debug_assert!(d >= 1);
        // l = ⌈log₂ d⌉ (0 for d = 1).
        let l = if d <= 1 {
            0
        } else {
            32 - (d - 1).leading_zeros()
        };
        let k = 63 + l;
        let d128 = d as u128;
        let m = (1u128 << k).div_ceil(d128);
        debug_assert!(m <= u64::MAX as u128);
        Self {
            m: m as u64,
            sh: l as u8,
        }
    }

    /// The multiplier (a table stores it split from the shift, to pack a
    /// row into one cache line).
    #[inline(always)]
    #[must_use]
    pub const fn m(self) -> u64 {
        self.m
    }

    /// The shift past 63.
    #[inline(always)]
    #[must_use]
    pub const fn sh(self) -> u8 {
        self.sh
    }

    /// Rebuild from [`Magic::m`] and [`Magic::sh`] of a [`Magic::new`].
    #[inline(always)]
    #[must_use]
    pub const fn from_parts(m: u64, sh: u8) -> Self {
        Self { m, sh }
    }

    /// `⌊x / d⌋` for `0 ≤ x < 2^63`.
    #[inline(always)]
    #[must_use]
    pub const fn div(self, x: u64) -> u64 {
        debug_assert!(x < (1u64 << 63));
        ((x as u128 * self.m as u128) >> (63 + self.sh as u32)) as u64
    }
}

/// `x` floored to a multiple of `unit` (`x ≥ 0`; `magic` is `unit`'s).
#[inline(always)]
#[must_use]
pub const fn floor_to(x: i64, unit: i64, magic: Magic) -> i64 {
    debug_assert!(x >= 0 && unit > 0);
    magic.div(x as u64) as i64 * unit
}

/// `x` ceiled to a multiple of `unit` (`0 ≤ x ≤ i64::MAX − unit`).
#[inline(always)]
#[must_use]
pub const fn ceil_to(x: i64, unit: i64, magic: Magic) -> i64 {
    debug_assert!(x >= 0 && unit > 0 && x <= i64::MAX - unit);
    magic.div((x + unit - 1) as u64) as i64 * unit
}

/// Decimals needed to write `unit_1e6` (a tick or a step ×1e6) exactly:
/// 6 minus the trailing decimal zeros, capped at 6.
#[must_use]
pub const fn decimals_of(unit_1e6: u32) -> u8 {
    let mut u = unit_1e6;
    let mut dec = 6u8;
    while dec > 0 && u % 10 == 0 && u != 0 {
        u /= 10;
        dec -= 1;
    }
    dec
}

const fn digits2() -> [u8; 200] {
    let mut t = [0u8; 200];
    let mut i = 0;
    while i < 100 {
        t[2 * i] = b'0' + (i / 10) as u8;
        t[2 * i + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    t
}

/// "00" "01" … "99".
static DIGITS2: [u8; 200] = digits2();

/// The widest rendering: 19 integer digits, a point and 6 decimals.
pub const RENDER_MAX: usize = 26;

/// The digits [`render_u64`] writes for `v`.
#[inline(always)]
#[must_use]
pub const fn u64_len(v: u64) -> usize {
    match v.checked_ilog10() {
        Some(l) => l as usize + 1,
        None => 1,
    }
}

/// The bytes [`render_fixed`] writes for `v_1e6` (≥ 0) with `dec`
/// decimals.
#[inline(always)]
#[must_use]
pub const fn fixed_len(v_1e6: i64, dec: u8) -> usize {
    u64_len(v_1e6 as u64 / 1_000_000) + if dec == 0 { 0 } else { 1 + dec as usize }
}

/// Write `v` (decimal digits only) into `out`, returning the length.
/// `out` must hold [`u64_len`]`(v)` bytes (the frame passes exactly that).
#[inline]
pub fn render_u64(v: u64, out: &mut [u8]) -> usize {
    let n = u64_len(v);
    debug_assert!(out.len() >= n);
    let mut i = n;
    let mut x = v;
    while x >= 100 {
        let r = (x % 100) as usize;
        x /= 100;
        out[i - 1] = DIGITS2[2 * r + 1];
        out[i - 2] = DIGITS2[2 * r];
        i -= 2;
    }
    if x >= 10 {
        let r = x as usize;
        out[i - 1] = DIGITS2[2 * r + 1];
        out[i - 2] = DIGITS2[2 * r];
    } else {
        out[i - 1] = b'0' + x as u8;
    }
    n
}

/// Write the ×1e6 value `v_1e6` (≥ 0) with exactly `dec` decimals (≤ 6)
/// into `out` (≥ [`fixed_len`] bytes; the frame passes exactly that),
/// returning the length. `dec = 0` writes no point. The digits past `dec`
/// must be zero — the value is on its tick or step — and are asserted so
/// in debug. Every digit is written straight into `out`: nothing staged.
#[inline]
pub fn render_fixed(v_1e6: i64, dec: u8, out: &mut [u8]) -> usize {
    debug_assert!(v_1e6 >= 0 && dec <= 6);
    let v = v_1e6 as u64;
    let int = v / 1_000_000;
    let frac = v % 1_000_000;
    let mut n = render_u64(int, out);
    if dec == 0 {
        debug_assert!(frac == 0, "a value off its unit");
        return n;
    }
    out[n] = b'.';
    n += 1;
    // The fraction's first `dec` digits, most significant first, two per
    // table entry (the pair indices live in registers, not the digits).
    let d = dec as usize;
    debug_assert!(frac % POW10[6 - d] == 0, "a value off its unit");
    let pairs = [(frac / 10_000) as usize, (frac / 100 % 100) as usize, (frac % 100) as usize];
    let mut j = 0;
    while j < d {
        out[n + j] = DIGITS2[2 * pairs[j >> 1] + (j & 1)];
        j += 1;
    }
    n + d
}

/// Powers of ten to 1e6 (the debug unit check).
const POW10: [u64; 7] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000];

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn magic_edges() {
        for d in [1u32, 2, 3, 7, 10, 100, 1_000_000, 4_294_967_295, 2_147_483_648] {
            let g = Magic::new(d);
            for x in [0u64, 1, d as u64 - 1, d as u64, d as u64 + 1, (1u64 << 63) - 1] {
                assert_eq!(g.div(x), x / d as u64, "x={x} d={d}");
            }
        }
    }

    #[test]
    fn quantize_is_the_side_safe_rounding() {
        let tick = 100_000; // 0.1
        let m = Magic::new(tick as u32);
        assert_eq!(floor_to(65_000_150_000, tick, m), 65_000_100_000);
        assert_eq!(ceil_to(65_000_150_000, tick, m), 65_000_200_000);
        assert_eq!(ceil_to(65_000_100_000, tick, m), 65_000_100_000);
        assert_eq!(floor_to(0, tick, m), 0);
    }

    #[test]
    fn decimals_follow_the_unit() {
        assert_eq!(decimals_of(100_000), 1);
        assert_eq!(decimals_of(1), 6);
        assert_eq!(decimals_of(1_000_000), 0);
        assert_eq!(decimals_of(10_000_000), 0);
        assert_eq!(decimals_of(10), 5);
    }

    #[test]
    fn render_examples() {
        let mut b = [0u8; RENDER_MAX];
        let n = render_fixed(65_000_100_000, 1, &mut b);
        assert_eq!(&b[..n], b"65000.1");
        let n = render_fixed(1_000, 3, &mut b);
        assert_eq!(&b[..n], b"0.001");
        let n = render_fixed(5_000_000, 0, &mut b);
        assert_eq!(&b[..n], b"5");
        let n = render_fixed(i64::MAX / 1_000_000 * 1_000_000, 0, &mut b);
        assert_eq!(&b[..n], b"9223372036854");
        let n = render_u64(u64::MAX, &mut b);
        assert_eq!(&b[..n], b"18446744073709551615");
    }

    proptest! {
        #[test]
        fn magic_is_division(d in 1u32.., x in 0u64..(1u64 << 63)) {
            prop_assert_eq!(Magic::new(d).div(x), x / d as u64);
        }

        #[test]
        fn magic_small_divisors(d in 1u32..=4_096, x in 0u64..(1u64 << 63)) {
            prop_assert_eq!(Magic::new(d).div(x), x / d as u64);
        }

        #[test]
        fn floor_and_ceil_bracket(x in 0i64..(1i64 << 60), unit in 1i64..=4_294_967_295) {
            let m = Magic::new(unit as u32);
            let f = floor_to(x, unit, m);
            let c = ceil_to(x, unit, m);
            prop_assert!(f <= x && x <= c);
            prop_assert_eq!(f % unit, 0);
            prop_assert_eq!(c % unit, 0);
            prop_assert!(c - f == 0 || c - f == unit);
            prop_assert_eq!(c == f, x % unit == 0);
        }

        #[test]
        fn render_fixed_matches_the_formatter(q in 0i64..(1i64 << 55), dec in 0u8..=6) {
            // A value on the 10^-dec grid.
            let unit = 10i64.pow(6 - dec as u32);
            let v = q / unit * unit;
            let mut b = [0u8; RENDER_MAX];
            let n = render_fixed(v, dec, &mut b);
            let want = if dec == 0 {
                std::format!("{}", v / 1_000_000)
            } else {
                let frac = std::format!("{:06}", v % 1_000_000);
                std::format!("{}.{}", v / 1_000_000, &frac[..dec as usize])
            };
            prop_assert_eq!(core::str::from_utf8(&b[..n]).unwrap(), want.as_str());
            // The frame's header is written from this before the digits.
            prop_assert_eq!(n, fixed_len(v, dec));
        }

        #[test]
        fn render_u64_matches_the_formatter(v in any::<u64>()) {
            let mut b = [0u8; 20];
            let n = render_u64(v, &mut b);
            let want = std::format!("{v}");
            prop_assert_eq!(core::str::from_utf8(&b[..n]).unwrap(), want.as_str());
            prop_assert_eq!(n, u64_len(v));
        }
    }
}
