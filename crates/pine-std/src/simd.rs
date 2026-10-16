// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The reduction kernels of impl-04 §4.9 — scalar references only so far (the NEON / AVX2 twins
//! are not written: on every target the scalar path is the selected one, which §4.9 rule 1 allows
//! for "everything else"). The scalar reference is the definition: four accumulators `s0..s3` take
//! indices `≡ 0, 1, 2, 3 (mod 4)` in increasing order, combined as `(s0 + s1) + (s2 + s3)`, the
//! `n mod 4` tail folded into the combined total in order.

/// `Σ` and count of the non-NaN values of `a` (§4.9 `sum_f64_skipna`).
#[must_use]
pub fn sum_f64_skipna(a: &[f64]) -> (f64, u32) {
    sum_f64_skipna_scalar(a)
}

/// The scalar reference of [`sum_f64_skipna`].
#[must_use]
pub fn sum_f64_skipna_scalar(a: &[f64]) -> (f64, u32) {
    let mut s = [0.0f64; 4];
    let mut n = 0u32;
    let body = a.len() - a.len() % 4;
    let mut i = 0usize;
    while i < body {
        let mut k = 0usize;
        while k < 4 {
            let x = a[i + k];
            if !x.is_nan() {
                s[k] += x;
                n += 1;
            }
            k += 1;
        }
        i += 4;
    }
    let mut t = (s[0] + s[1]) + (s[2] + s[3]);
    while i < a.len() {
        let x = a[i];
        if !x.is_nan() {
            t += x;
            n += 1;
        }
        i += 1;
    }
    (t, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_skips_nan_and_follows_the_tree() {
        let a = [1.0, f64::NAN, 3.0, 4.0, 5.0, 6.0];
        let (s, n) = sum_f64_skipna(&a);
        assert_eq!(n, 5);
        assert_eq!(
            s.to_bits(),
            ((((1.0 + 0.0) + (3.0 + 4.0f64)) + 5.0) + 6.0).to_bits()
        );
        assert_eq!(sum_f64_skipna(&[f64::NAN; 3]), (0.0, 0));
    }
}
