// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Minimal 256-bit unsigned integer for the V3 swap math.
//!
//! Exactly what `TickMath`, `SqrtPriceMath`, `SwapMath` and
//! `FullMath.mulDiv` need, and nothing else: compare, add/sub with an
//! overflow report, shifts, a widening `u128 × u128`, a 256×256→512
//! product and a 512÷256 division (Knuth D on 64-bit limbs).
//!
//! Doctrine: fixed-size arrays only, no allocation, no floats, no
//! panics. Overflow is REPORTED (`Option`), never wrapped silently,
//! except in the two functions whose names say `wrapping` — those exist
//! for `tick_at_sqrt`'s two's-complement `int256` arithmetic, where
//! wrapping IS the specified semantics.

/// Unsigned 256-bit integer. Field order `hi, lo` makes the derived
/// `Ord` numeric.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct U256 {
    pub(crate) hi: u128,
    pub(crate) lo: u128,
}

/// 2^64 as `u128`, the Knuth-D radix.
const RADIX: u128 = 1 << 64;

impl U256 {
    pub(crate) const ZERO: Self = Self { hi: 0, lo: 0 };
    pub(crate) const ONE: Self = Self { hi: 0, lo: 1 };
    pub(crate) const MAX: Self = Self {
        hi: u128::MAX,
        lo: u128::MAX,
    };

    #[inline(always)]
    pub(crate) const fn from_u128(x: u128) -> Self {
        Self { hi: 0, lo: x }
    }

    /// `2^n` for `n < 256`.
    #[inline(always)]
    pub(crate) const fn pow2(n: u32) -> Self {
        debug_assert!(n < 256);
        if n < 128 {
            Self {
                hi: 0,
                lo: 1u128 << n,
            }
        } else {
            Self {
                hi: 1u128 << (n - 128),
                lo: 0,
            }
        }
    }

    /// `self < rhs`, usable in `const fn` (the derived `Ord` is not).
    #[inline(always)]
    pub(crate) const fn lt(self, rhs: Self) -> bool {
        self.hi < rhs.hi || (self.hi == rhs.hi && self.lo < rhs.lo)
    }

    /// `self <= rhs`, usable in `const fn`.
    #[inline(always)]
    pub(crate) const fn le(self, rhs: Self) -> bool {
        !rhs.lt(self)
    }

    /// `self == rhs`, usable in `const fn` (the derived `PartialEq` is not).
    #[inline(always)]
    pub(crate) const fn same_as(self, rhs: Self) -> bool {
        self.hi == rhs.hi && self.lo == rhs.lo
    }

    #[inline(always)]
    pub(crate) const fn is_zero(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    /// The value clamped into `u128`.
    #[inline(always)]
    pub(crate) const fn saturating_u128(self) -> u128 {
        if self.hi == 0 {
            self.lo
        } else {
            u128::MAX
        }
    }

    /// A `uint160` carried as `(low 128 bits, high 32 bits)`.
    #[inline(always)]
    pub(crate) const fn from_u160(lo: u128, hi: u32) -> Self {
        Self { hi: hi as u128, lo }
    }

    /// `Some((lo, hi))` when the value fits `uint160`.
    #[inline(always)]
    pub(crate) const fn to_u160(self) -> Option<(u128, u32)> {
        if self.hi <= u32::MAX as u128 {
            Some((self.lo, self.hi as u32))
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) const fn leading_zeros(self) -> u32 {
        if self.hi != 0 {
            self.hi.leading_zeros()
        } else {
            128 + self.lo.leading_zeros()
        }
    }

    /// Index of the most significant set bit. Undefined (0) for zero.
    #[inline(always)]
    pub(crate) const fn msb(self) -> u32 {
        255 - self.leading_zeros()
    }

    #[inline(always)]
    pub(crate) const fn overflowing_add(self, rhs: Self) -> (Self, bool) {
        let (lo, c) = self.lo.overflowing_add(rhs.lo);
        let (hi1, o1) = self.hi.overflowing_add(rhs.hi);
        let (hi, o2) = hi1.overflowing_add(c as u128);
        (Self { hi, lo }, o1 | o2)
    }

    #[inline(always)]
    pub(crate) const fn overflowing_sub(self, rhs: Self) -> (Self, bool) {
        let (lo, b) = self.lo.overflowing_sub(rhs.lo);
        let (hi1, o1) = self.hi.overflowing_sub(rhs.hi);
        let (hi, o2) = hi1.overflowing_sub(b as u128);
        (Self { hi, lo }, o1 | o2)
    }

    #[inline(always)]
    pub(crate) const fn checked_add(self, rhs: Self) -> Option<Self> {
        let (v, o) = self.overflowing_add(rhs);
        if o {
            None
        } else {
            Some(v)
        }
    }

    #[inline(always)]
    pub(crate) const fn checked_sub(self, rhs: Self) -> Option<Self> {
        let (v, o) = self.overflowing_sub(rhs);
        if o {
            None
        } else {
            Some(v)
        }
    }

    /// Two's-complement addition modulo 2^256 (`int256` semantics).
    #[inline(always)]
    pub(crate) const fn wrapping_add(self, rhs: Self) -> Self {
        self.overflowing_add(rhs).0
    }

    /// Two's-complement subtraction modulo 2^256 (`int256` semantics).
    #[inline(always)]
    pub(crate) const fn wrapping_sub(self, rhs: Self) -> Self {
        self.overflowing_sub(rhs).0
    }

    /// Two's-complement negation modulo 2^256.
    #[inline(always)]
    pub(crate) const fn wrapping_neg(self) -> Self {
        Self::ZERO.wrapping_sub(self)
    }

    /// Logical shift left; bits shifted past 255 are discarded.
    #[inline(always)]
    pub(crate) const fn shl(self, n: u32) -> Self {
        if n == 0 {
            self
        } else if n >= 256 {
            Self::ZERO
        } else if n >= 128 {
            Self {
                hi: self.lo << (n - 128),
                lo: 0,
            }
        } else {
            Self {
                hi: (self.hi << n) | (self.lo >> (128 - n)),
                lo: self.lo << n,
            }
        }
    }

    /// Logical shift right.
    #[inline(always)]
    pub(crate) const fn shr(self, n: u32) -> Self {
        if n == 0 {
            self
        } else if n >= 256 {
            Self::ZERO
        } else if n >= 128 {
            Self {
                hi: 0,
                lo: self.hi >> (n - 128),
            }
        } else {
            Self {
                hi: self.hi >> n,
                lo: (self.lo >> n) | (self.hi << (128 - n)),
            }
        }
    }

    #[inline(always)]
    const fn limbs(self) -> [u64; 4] {
        [
            self.lo as u64,
            (self.lo >> 64) as u64,
            self.hi as u64,
            (self.hi >> 64) as u64,
        ]
    }

    #[inline(always)]
    const fn from_limbs(l0: u64, l1: u64, l2: u64, l3: u64) -> Self {
        Self {
            hi: ((l3 as u128) << 64) | l2 as u128,
            lo: ((l1 as u128) << 64) | l0 as u128,
        }
    }

    /// The full 256-bit product of two `u128`s. Never overflows.
    #[inline(always)]
    pub(crate) const fn mul_u128(a: u128, b: u128) -> Self {
        let a0 = a as u64 as u128;
        let a1 = a >> 64;
        let b0 = b as u64 as u128;
        let b1 = b >> 64;
        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;
        // middle = high(p00) + low(p01) + low(p10) < 3·2^64 — no overflow.
        let mid = (p00 >> 64) + (p01 as u64 as u128) + (p10 as u64 as u128);
        let lo = (p00 as u64 as u128) | (mid << 64);
        let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
        Self { hi, lo }
    }

    /// `Some(self * rhs)` when the product fits 256 bits.
    #[inline]
    pub(crate) const fn checked_mul(self, rhs: Self) -> Option<Self> {
        let p = full_mul(self.limbs(), rhs.limbs());
        if p[4] | p[5] | p[6] | p[7] != 0 {
            None
        } else {
            Some(Self::from_limbs(p[0], p[1], p[2], p[3]))
        }
    }

    /// `Some((self / d, self % d))`; `None` when `d == 0`.
    #[inline]
    pub(crate) const fn checked_div_rem(self, d: Self) -> Option<(Self, Self)> {
        if d.is_zero() {
            return None;
        }
        let l = self.limbs();
        let u = [l[0], l[1], l[2], l[3], 0, 0, 0, 0];
        let (q, r) = divmod_512_by_256(u, d.limbs());
        Some((
            Self::from_limbs(q[0], q[1], q[2], q[3]),
            Self::from_limbs(r[0], r[1], r[2], r[3]),
        ))
    }

    /// Floor square root (Newton, monotone from above).
    pub(crate) const fn isqrt(self) -> Self {
        if self.hi == 0 && self.lo < 2 {
            return self;
        }
        // x0 = 2^ceil(bits/2) >= sqrt(self)
        let bits = 256 - self.leading_zeros();
        let mut x = Self::pow2(bits.div_ceil(2));
        loop {
            let (q, _) = match self.checked_div_rem(x) {
                Some(v) => v,
                None => return Self::ZERO, // x is never zero here
            };
            let (s, o) = x.overflowing_add(q);
            // (x + q) / 2 with the carry bit folded back in
            let y = if o {
                s.shr(1).wrapping_add(Self::pow2(255))
            } else {
                s.shr(1)
            };
            if x.le(y) {
                return x;
            }
            x = y;
        }
    }
}

/// `floor(a · b / d)`; `None` when `d == 0` or the quotient needs more
/// than 256 bits. The Uniswap `FullMath.mulDiv` contract.
#[inline]
pub(crate) const fn mul_div(a: U256, b: U256, d: U256) -> Option<U256> {
    match mul_div_rem(a, b, d) {
        Some((q, _)) => Some(q),
        None => None,
    }
}

/// `ceil(a · b / d)`; `None` on `d == 0` or overflow.
/// The Uniswap `FullMath.mulDivRoundingUp` contract.
#[inline]
pub(crate) const fn mul_div_rounding_up(a: U256, b: U256, d: U256) -> Option<U256> {
    match mul_div_rem(a, b, d) {
        Some((q, r)) => {
            if r.is_zero() {
                Some(q)
            } else {
                q.checked_add(U256::ONE)
            }
        }
        None => None,
    }
}

/// `ceil(a / b)`; `None` when `b == 0`. `UnsafeMath.divRoundingUp`.
#[inline]
pub(crate) const fn div_rounding_up(a: U256, b: U256) -> Option<U256> {
    match a.checked_div_rem(b) {
        Some((q, r)) => {
            if r.is_zero() {
                Some(q)
            } else {
                q.checked_add(U256::ONE)
            }
        }
        None => None,
    }
}

#[inline]
const fn mul_div_rem(a: U256, b: U256, d: U256) -> Option<(U256, U256)> {
    if d.is_zero() {
        return None;
    }
    let p = full_mul(a.limbs(), b.limbs());
    let (q, r) = divmod_512_by_256(p, d.limbs());
    if q[4] | q[5] | q[6] | q[7] != 0 {
        return None;
    }
    Some((
        U256::from_limbs(q[0], q[1], q[2], q[3]),
        U256::from_limbs(r[0], r[1], r[2], r[3]),
    ))
}

/// Schoolbook 4×4-limb product. Each partial `a·b + r + carry` is at
/// most `(2^64−1)^2 + 2·(2^64−1) = 2^128 − 1`, so `u128` never overflows.
#[inline]
const fn full_mul(a: [u64; 4], b: [u64; 4]) -> [u64; 8] {
    let mut r = [0u64; 8];
    let mut i = 0;
    while i < 4 {
        let mut carry: u128 = 0;
        let mut j = 0;
        while j < 4 {
            let t = (a[i] as u128) * (b[j] as u128) + (r[i + j] as u128) + carry;
            r[i + j] = t as u64;
            carry = t >> 64;
            j += 1;
        }
        r[i + 4] = carry as u64;
        i += 1;
    }
    r
}

/// Knuth algorithm D (TAOCP 4.3.1; Hacker's Delight `divmnu64`) for an
/// up-to-8-limb dividend and a non-zero up-to-4-limb divisor.
///
/// `qhat · vn[n−2]` is only formed after `qhat < 2^64` has been checked,
/// and `rhat` stays below `2^64` whenever it is shifted, so every `u128`
/// intermediate is in range.
const fn divmod_512_by_256(u: [u64; 8], v: [u64; 4]) -> ([u64; 8], [u64; 4]) {
    let mut q = [0u64; 8];
    let mut n = 4;
    while n > 0 && v[n - 1] == 0 {
        n -= 1;
    }
    // n == 0 is excluded by every caller (checked is_zero first).
    if n == 0 {
        return (q, [0; 4]);
    }
    let mut m = 8;
    while m > 0 && u[m - 1] == 0 {
        m -= 1;
    }
    if m < n {
        // u < v by limb count.
        return (q, [u[0], u[1], u[2], u[3]]);
    }
    if n == 1 {
        let d = v[0] as u128;
        let mut rem: u128 = 0;
        let mut i = m;
        while i > 0 {
            i -= 1;
            let cur = (rem << 64) | u[i] as u128;
            q[i] = (cur / d) as u64;
            rem = cur % d;
        }
        return (q, [rem as u64, 0, 0, 0]);
    }
    // Normalise so the divisor's top limb has its high bit set.
    let s = v[n - 1].leading_zeros();
    let mut vn = [0u64; 4];
    let mut i = n - 1;
    while i > 0 {
        vn[i] = if s == 0 {
            v[i]
        } else {
            (v[i] << s) | (v[i - 1] >> (64 - s))
        };
        i -= 1;
    }
    vn[0] = v[0] << s;
    let mut un = [0u64; 9];
    un[m] = if s == 0 { 0 } else { u[m - 1] >> (64 - s) };
    let mut i = m - 1;
    while i > 0 {
        un[i] = if s == 0 {
            u[i]
        } else {
            (u[i] << s) | (u[i - 1] >> (64 - s))
        };
        i -= 1;
    }
    un[0] = u[0] << s;

    let top = vn[n - 1] as u128;
    let next = vn[n - 2] as u128;
    let mut j = m - n + 1;
    while j > 0 {
        j -= 1;
        let num = ((un[j + n] as u128) << 64) | un[j + n - 1] as u128;
        let mut qhat = num / top;
        let mut rhat = num % top;
        while qhat >= RADIX || qhat * next > ((rhat << 64) | un[j + n - 2] as u128) {
            qhat -= 1;
            rhat += top;
            if rhat >= RADIX {
                break;
            }
        }
        // Multiply and subtract: un[j..=j+n] -= qhat · vn.
        let mut borrow: i128 = 0;
        let mut carry: u128 = 0;
        let mut k = 0;
        while k < n {
            let p = qhat * (vn[k] as u128) + carry;
            carry = p >> 64;
            let t = (un[k + j] as i128) - borrow - ((p as u64) as i128);
            un[k + j] = t as u64;
            borrow = if t < 0 { 1 } else { 0 };
            k += 1;
        }
        let t = (un[j + n] as i128) - borrow - (carry as i128);
        un[j + n] = t as u64;
        if t < 0 {
            // qhat was one too large: add the divisor back.
            qhat -= 1;
            let mut c: u128 = 0;
            let mut k = 0;
            while k < n {
                let s2 = (un[k + j] as u128) + (vn[k] as u128) + c;
                un[k + j] = s2 as u64;
                c = s2 >> 64;
                k += 1;
            }
            un[j + n] = un[j + n].wrapping_add(c as u64);
        }
        q[j] = qhat as u64;
    }
    // Remainder = un[0..n] >> s.
    let mut r = [0u64; 4];
    let mut k = 0;
    while k < n {
        r[k] = if s == 0 {
            un[k]
        } else {
            (un[k] >> s) | (un[k + 1] << (64 - s))
        };
        k += 1;
    }
    (q, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn u(hi: u128, lo: u128) -> U256 {
        U256 { hi, lo }
    }

    /// Reference 512-bit value as 8 limbs, for `q·d + r == a·b`.
    fn add512(a: [u64; 8], b: [u64; 8]) -> ([u64; 8], bool) {
        let mut r = [0u64; 8];
        let mut c = 0u128;
        let mut i = 0;
        while i < 8 {
            let s = a[i] as u128 + b[i] as u128 + c;
            r[i] = s as u64;
            c = s >> 64;
            i += 1;
        }
        (r, c != 0)
    }

    fn arb_u256() -> impl Strategy<Value = U256> {
        prop_oneof![
            (any::<u128>(), any::<u128>()).prop_map(|(h, l)| u(h, l)),
            any::<u128>().prop_map(U256::from_u128),
            (any::<u64>(), any::<u128>()).prop_map(|(h, l)| u(h as u128, l)),
            (0u32..256).prop_map(U256::pow2),
            (0u32..256, any::<u128>())
                .prop_map(|(n, x)| U256::pow2(n).wrapping_sub(U256::from_u128(x % 7))),
        ]
    }

    #[test]
    fn mul_u128_matches_full_mul() {
        let a = u128::MAX;
        let p = U256::mul_u128(a, a);
        // (2^128-1)^2 = 2^256 - 2^129 + 1
        assert_eq!(
            p,
            U256::MAX
                .wrapping_sub(U256::pow2(129))
                .wrapping_add(U256::from_u128(2))
        );
    }

    #[test]
    fn division_edge_cases() {
        assert_eq!(
            U256::MAX.checked_div_rem(U256::ONE),
            Some((U256::MAX, U256::ZERO))
        );
        assert_eq!(
            U256::MAX.checked_div_rem(U256::MAX),
            Some((U256::ONE, U256::ZERO))
        );
        assert_eq!(U256::ONE.checked_div_rem(U256::ZERO), None);
        assert_eq!(mul_div(U256::MAX, U256::MAX, U256::MAX), Some(U256::MAX));
        assert_eq!(mul_div(U256::MAX, U256::from_u128(2), U256::ONE), None);
        assert_eq!(
            mul_div_rounding_up(U256::from_u128(7), U256::from_u128(3), U256::from_u128(2)),
            Some(U256::from_u128(11))
        );
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::ONE, U256::ONE),
            Some(U256::MAX)
        );
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::from_u128(2), U256::from_u128(2)),
            Some(U256::MAX)
        );
        assert_eq!(
            div_rounding_up(U256::from_u128(10), U256::from_u128(4)),
            Some(U256::from_u128(3))
        );
    }

    #[test]
    fn isqrt_small_and_extremes() {
        let mut x = 0u128;
        while x < 10_000 {
            let r = U256::from_u128(x).isqrt().lo;
            assert!(r * r <= x && (r + 1) * (r + 1) > x, "isqrt({x}) = {r}");
            x += 1;
        }
        assert_eq!(U256::MAX.isqrt(), U256::from_u128(u128::MAX));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20_000))]

        /// q·d + r == a·b and r < d — correctness without an oracle.
        #[test]
        fn mul_div_is_exact(a in arb_u256(), b in arb_u256(), d in arb_u256()) {
            prop_assume!(!d.is_zero());
            let p = full_mul(a.limbs(), b.limbs());
            let (q, r) = divmod_512_by_256(p, d.limbs());
            let qd = {
                // q (8 limbs) · d (4 limbs), truncated to 8 limbs — exact when q·d ≤ p.
                let mut acc = [0u64; 8];
                let mut i = 0;
                while i < 8 {
                    let mut carry = 0u128;
                    let dl = d.limbs();
                    let mut j = 0;
                    while j < 4 && i + j < 8 {
                        let t = q[i] as u128 * dl[j] as u128 + acc[i + j] as u128 + carry;
                        acc[i + j] = t as u64;
                        carry = t >> 64;
                        j += 1;
                    }
                    if i + 4 < 8 { acc[i + 4] = acc[i + 4].wrapping_add(carry as u64); }
                    i += 1;
                }
                acc
            };
            let (sum, of) = add512(qd, [r[0], r[1], r[2], r[3], 0, 0, 0, 0]);
            prop_assert!(!of);
            prop_assert_eq!(sum, p);
            let rr = U256::from_limbs(r[0], r[1], r[2], r[3]);
            prop_assert!(rr < d);
        }

        #[test]
        fn small_values_match_u128(a in any::<u64>(), b in any::<u64>(), d in 1u128..) {
            let want = (a as u128 * b as u128) / d;
            prop_assert_eq!(mul_div(U256::from_u128(a as u128), U256::from_u128(b as u128), U256::from_u128(d)), Some(U256::from_u128(want)));
        }

        #[test]
        fn div_rem_is_exact(a in arb_u256(), d in arb_u256()) {
            prop_assume!(!d.is_zero());
            let (q, r) = a.checked_div_rem(d).unwrap();
            prop_assert!(r < d);
            let back = q.checked_mul(d).and_then(|x| x.checked_add(r));
            prop_assert_eq!(back, Some(a));
        }

        /// A left shift that loses no bits is undone by the right shift,
        /// and truncating the low bits never increases the value.
        #[test]
        fn shifts_round_trip(a in arb_u256(), n in 0u32..256) {
            if a.leading_zeros() >= n { prop_assert_eq!(a.shl(n).shr(n), a); }
            prop_assert!(a.shr(n).shl(n) <= a);
            prop_assert_eq!(a.shr(n), a.checked_div_rem(U256::pow2(n)).unwrap().0);
        }

        #[test]
        fn isqrt_is_floor(a in arb_u256()) {
            let r = a.isqrt();
            prop_assert!(r.checked_mul(r).is_some_and(|sq| sq <= a));
            let r1 = r.wrapping_add(U256::ONE);
            prop_assert!(r1.checked_mul(r1).is_none_or(|sq| sq > a));
        }
    }
}
