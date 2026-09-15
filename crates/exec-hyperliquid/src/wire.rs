// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Prices and sizes on the wire.
//!
//! Hyperliquid takes both as JSON **strings**, and the exact spelling
//! of that string is inside the signature — so this is not formatting,
//! it is part of the wire protocol. The SDK's rule is:
//!
//! ```python
//! def float_to_wire(x):
//!     rounded = f"{x:.8f}"            # exactly 8 decimal places
//!     ...
//!     normalized = Decimal(rounded).normalize()
//!     return f"{normalized:f}"        # trailing zeros stripped
//! ```
//!
//! So `0.5` is `"0.5"` and never `"0.50000000"`; `10.0` is `"10"` and
//! never `"10.0"`. Get that wrong and every order is rejected with a
//! perfectly valid signature over the wrong bytes.
//!
//! We never see a float. The member carries fixed-point integers, and
//! this renders a `1e8`-scaled `i64` straight to bytes — no `format!`,
//! no `String`, no float arithmetic and therefore no rounding question
//! to answer. That is also why the SDK's `ValueError` branch has no
//! analogue here: an integer cannot fail to be representable in 8
//! decimal places.

/// The fixed-point scale: 8 decimal places, matching `f"{x:.8f}"`.
pub const WIRE_SCALE: i64 = 100_000_000;

/// Longest possible rendering: `-92233720368.54775808` is 21 bytes.
/// Round up to 24 so the scratch buffer is a tidy size.
pub const WIRE_MAX: usize = 24;

/// A rendered price or size. `Copy`, stack-only, no allocation.
#[derive(Copy, Clone)]
pub struct WireNum {
    buf: [u8; WIRE_MAX],
    len: u8,
}

impl WireNum {
    /// Render a `1e8`-scaled fixed-point integer.
    #[inline(always)]
    #[must_use]
    pub fn from_1e8(v: i64) -> Self {
        let mut buf = [0u8; WIRE_MAX];
        let mut n = 0usize;

        // `-0` renders as `0` (the SDK special-cases this), which falls
        // out naturally: zero takes the early path and never sees the
        // sign.
        if v == 0 {
            buf[0] = b'0';
            return Self { buf, len: 1 };
        }
        let neg = v < 0;
        // `unsigned_abs` so `i64::MIN` cannot overflow.
        let mag = v.unsigned_abs();
        if neg {
            buf[n] = b'-';
            n += 1;
        }

        let scale = WIRE_SCALE as u64;
        let int_part = mag / scale;
        let frac_part = mag % scale;

        // Integer part, most significant digit first.
        let mut tmp = [0u8; 20];
        let mut t = 0usize;
        let mut q = int_part;
        if q == 0 {
            tmp[0] = b'0';
            t = 1;
        } else {
            while q > 0 {
                tmp[t] = b'0' + (q % 10) as u8;
                q /= 10;
                t += 1;
            }
        }
        let mut i = t;
        while i > 0 {
            i -= 1;
            buf[n] = tmp[i];
            n += 1;
        }

        // Fraction, trailing zeros stripped. A zero fraction writes
        // nothing at all — no point, no digits.
        if frac_part != 0 {
            buf[n] = b'.';
            n += 1;
            // Eight digits, most significant first.
            let mut digits = [0u8; 8];
            let mut f = frac_part;
            let mut k = 8usize;
            while k > 0 {
                k -= 1;
                digits[k] = b'0' + (f % 10) as u8;
                f /= 10;
            }
            // Strip trailing zeros: `Decimal.normalize()`.
            let mut last = 8usize;
            while last > 0 && digits[last - 1] == b'0' {
                last -= 1;
            }
            let mut j = 0usize;
            while j < last {
                buf[n] = digits[j];
                n += 1;
                j += 1;
            }
        }

        Self {
            buf,
            len: n as u8,
        }
    }

    /// The rendered bytes.
    #[inline(always)]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl core::fmt::Debug for WireNum {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(self.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: i64) -> String {
        String::from_utf8(WireNum::from_1e8(v).as_bytes().to_vec()).unwrap()
    }

    /// Every one of these is a value that appears in the committed
    /// vectors, rendered by the SDK itself.
    #[test]
    fn matches_the_sdk_on_the_values_the_vectors_use() {
        assert_eq!(s(50_000_000), "0.5"); // 0.5
        assert_eq!(s(1_000_000_000), "10"); // 10.0 -> "10", NOT "10.0"
        assert_eq!(s(45_670_000), "0.4567"); // 0.4567
        assert_eq!(s(99_900_000), "0.999"); // 0.999
        assert_eq!(s(100_000), "0.001"); // 0.001
        assert_eq!(s(10_000_000_000), "100"); // 100.0
        assert_eq!(s(100_000_000), "1"); // 1.0
        assert_eq!(s(250_000_000), "2.5"); // 2.5
        assert_eq!(s(150_000_000), "1.5"); // 1.5
        assert_eq!(s(2_500_000_000), "25"); // 25.0
        assert_eq!(s(40_000_000), "0.4"); // 0.4
        assert_eq!(s(60_000_000), "0.6"); // 0.6
        assert_eq!(s(25_000_000), "0.25"); // 0.25
        assert_eq!(s(48_000_000), "0.48"); // 0.48
    }

    /// The whole point: trailing zeros are STRIPPED. `0.5000` on the
    /// wire is `"0.5"`, and a vector exercises exactly that.
    #[test]
    fn trailing_zeros_are_stripped() {
        assert_eq!(s(50_000_000), "0.5");
        assert_eq!(s(50_000_001), "0.50000001", "a real 8th digit survives");
        assert_eq!(s(50_100_000), "0.501");
        assert_eq!(s(1), "0.00000001", "the smallest representable");
        assert_eq!(s(10), "0.0000001");
    }

    #[test]
    fn zero_and_sign() {
        assert_eq!(s(0), "0");
        assert_eq!(s(-50_000_000), "-0.5");
        assert_eq!(s(-100_000_000), "-1");
        // A negative zero cannot arise from an integer, so the SDK's
        // "-0" special case has nothing to fire on.
        assert_eq!(s(-0), "0");
    }

    #[test]
    fn the_extremes_do_not_overflow_the_buffer() {
        // i64::MIN is the one value `abs()` would panic on.
        let a = s(i64::MIN);
        assert!(a.starts_with("-92233720368.5477"), "{a}");
        assert!(a.len() <= WIRE_MAX);
        let b = s(i64::MAX);
        assert!(b.starts_with("92233720368.5477"), "{b}");
        assert!(b.len() <= WIRE_MAX);
    }

    /// The gate's property test: every value in the HIP-4 range
    /// round-trips to the same fixed-point integer it came from.
    #[test]
    fn hip4_range_round_trips_to_the_same_integer() {
        // [0.001, 0.999] at the 0.0001 tick — every tick on the book.
        let mut v = 100_000i64; // 0.001
        while v <= 99_900_000 {
            let rendered = s(v);
            let back = parse_1e8(&rendered);
            assert_eq!(back, v, "{rendered} did not round-trip from {v}");
            v += 10_000; // one 0.0001 tick
        }
    }

    /// Parse a rendered decimal back to 1e8 fixed point — test-only,
    /// and deliberately independent of the writer so it cannot share a
    /// bug with it.
    fn parse_1e8(s: &str) -> i64 {
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int_s, frac_s) = match body.split_once('.') {
            Some((a, b)) => (a, b),
            None => (body, ""),
        };
        assert!(frac_s.len() <= 8, "more than 8 decimals: {s}");
        let int_v: i64 = int_s.parse().unwrap();
        let mut frac_v: i64 = if frac_s.is_empty() {
            0
        } else {
            frac_s.parse().unwrap()
        };
        for _ in frac_s.len()..8 {
            frac_v *= 10;
        }
        let m = int_v * WIRE_SCALE + frac_v;
        if neg {
            -m
        } else {
            m
        }
    }
}
