// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `0x`-hex scanners over the JSON bytes in the rx buffer.
//!
//! Nothing here decodes a hex string into a second buffer: an ABI word
//! is read straight out of its 64 ASCII digits, and only the bytes a
//! field needs are folded into an integer. Every function is total —
//! `None` on anything malformed or out of range, never a panic — and
//! zero-alloc.
//!
//! **Law — signed words are decoded from ALL 256 bits.** An `int256`
//! that is narrowed to `i128` must have its upper 128 bits equal to the
//! sign extension of the lower 128; anything else is refused. Reading
//! the low 16 bytes alone turns a large negative amount into a large
//! positive one — P&L fabricated silently.

use core_parse::Pos;

/// Hex digits in one ABI word.
pub const WORD_HEX: usize = 64;

/// Value of one ASCII hex digit, `0xff` if not a digit.
#[inline(always)]
const fn nib(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0xff,
    }
}

/// `true` when `buf[pos..pos+2]` is `0x` / `0X`.
#[inline(always)]
fn has_prefix(buf: &[u8], pos: Pos) -> bool {
    pos + 2 <= buf.len() && buf[pos] == b'0' && (buf[pos + 1] == b'x' || buf[pos + 1] == b'X')
}

/// Fold `digits` (all valid hex, at most 32) into a `u128`.
#[inline(always)]
fn fold_u128(digits: &[u8]) -> Option<u128> {
    debug_assert!(digits.len() <= 32);
    let mut v: u128 = 0;
    let mut i = 0;
    while i < digits.len() {
        let n = nib(digits[i]);
        if n == 0xff {
            return None;
        }
        v = (v << 4) | n as u128;
        i += 1;
    }
    Some(v)
}

/// `true` when every byte of `digits` is `c` (ASCII, either case for
/// `f`).
#[inline(always)]
fn all_digit(digits: &[u8], c: u8) -> bool {
    let mut i = 0;
    while i < digits.len() {
        let d = digits[i] | 0x20; // ASCII lower-case; '0' is unchanged
        if d != c {
            return false;
        }
        i += 1;
    }
    true
}

/// A `0x`-prefixed quantity of 1..=32 hex digits as `u128` (JSON-RPC
/// `QUANTITY`: block numbers, `baseFeePerGas`, subscription ids).
/// Returns `(value, position after the last digit)`.
#[inline]
pub fn hex_quantity_u128(buf: &[u8], pos: Pos) -> Option<(u128, Pos)> {
    if !has_prefix(buf, pos) {
        return None;
    }
    let start = pos + 2;
    let mut end = start;
    while end < buf.len() && nib(buf[end]) != 0xff {
        end += 1;
    }
    let n = end - start;
    if n == 0 || n > 32 {
        return None;
    }
    Some((fold_u128(&buf[start..end])?, end))
}

/// [`hex_quantity_u128`] bounded to `u64`.
#[inline]
pub fn hex_quantity_u64(buf: &[u8], pos: Pos) -> Option<(u64, Pos)> {
    let (v, end) = hex_quantity_u128(buf, pos)?;
    if v > u64::MAX as u128 {
        return None;
    }
    Some((v as u64, end))
}

/// Exactly `2·N` hex digits after `0x`, and NOT followed by another
/// digit, as `N` big-endian bytes (addresses, topic hashes).
#[inline]
pub fn hex_fixed<const N: usize>(buf: &[u8], pos: Pos) -> Option<([u8; N], Pos)> {
    if !has_prefix(buf, pos) {
        return None;
    }
    let start = pos + 2;
    let end = start + 2 * N;
    if end > buf.len() || (end < buf.len() && nib(buf[end]) != 0xff) {
        return None;
    }
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        let hi = nib(buf[start + 2 * i]);
        let lo = nib(buf[start + 2 * i + 1]);
        if hi == 0xff || lo == 0xff {
            return None;
        }
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some((out, end))
}

/// An ABI `data` string: `0x` followed by a whole number of 64-digit
/// words, every digit hex. Returns `(first digit, word count, position
/// after the last digit)`. The words stay where they are.
#[inline]
pub fn data_words(buf: &[u8], pos: Pos) -> Option<(Pos, usize, Pos)> {
    if !has_prefix(buf, pos) {
        return None;
    }
    let start = pos + 2;
    let mut end = start;
    while end < buf.len() && nib(buf[end]) != 0xff {
        end += 1;
    }
    let n = end - start;
    if n % WORD_HEX != 0 {
        return None;
    }
    Some((start, n / WORD_HEX, end))
}

/// The 64 digits of word `i` of a [`data_words`] span.
#[inline(always)]
pub fn word(buf: &[u8], start: Pos, i: usize) -> &[u8] {
    &buf[start + i * WORD_HEX..start + (i + 1) * WORD_HEX]
}

/// A word as `uint128`: the upper 128 bits must be zero.
#[inline]
pub fn word_u128(w: &[u8]) -> Option<u128> {
    if w.len() != WORD_HEX || !all_digit(&w[..32], b'0') {
        return None;
    }
    fold_u128(&w[32..])
}

/// A word as `uint160` → `(low 128 bits, high 32 bits)`.
#[inline]
pub fn word_u160(w: &[u8]) -> Option<(u128, u32)> {
    if w.len() != WORD_HEX || !all_digit(&w[..24], b'0') {
        return None;
    }
    let hi = fold_u128(&w[24..32])? as u32;
    Some((fold_u128(&w[32..])?, hi))
}

/// A word as an ADDRESS, rendered `0x` + 40 lowercase digits into `dst`
/// straight from the word's own digits (H9: `token0()` used to parse
/// the word to integers, copy them into a byte array and render that
/// back to hex). The upper 96 bits must be zero and the address not
/// zero; `false` otherwise, `dst` then unspecified.
#[inline]
pub fn word_addr_hex(w: &[u8], dst: &mut [u8; 42]) -> bool {
    if w.len() != WORD_HEX || !all_digit(&w[..24], b'0') {
        return false;
    }
    dst[0] = b'0';
    dst[1] = b'x';
    let mut any = false;
    let mut i = 0;
    while i < 40 {
        let c = w[24 + i];
        if nib(c) == 0xff {
            return false;
        }
        any |= c != b'0';
        // ASCII case fold: `A`-`F` → `a`-`f`; digits and `a`-`f` keep.
        dst[2 + i] = if c.is_ascii_uppercase() { c | 0x20 } else { c };
        i += 1;
    }
    any
}

/// A word as `uint32` (fees): the upper 224 bits must be zero.
#[inline]
pub fn word_u32(w: &[u8]) -> Option<u32> {
    if w.len() != WORD_HEX || !all_digit(&w[..56], b'0') {
        return None;
    }
    Some(fold_u128(&w[56..])? as u32)
}

/// A word as `int256` narrowed to `i128`, decoded from ALL 256 bits: the
/// upper 128 bits must be the sign extension of the lower 128.
#[inline]
pub fn word_i128(w: &[u8]) -> Option<i128> {
    if w.len() != WORD_HEX {
        return None;
    }
    let low = fold_u128(&w[32..])?;
    let neg = low >> 127 == 1;
    if !all_digit(&w[..32], if neg { b'f' } else { b'0' }) {
        return None;
    }
    Some(low as i128)
}

/// A word as a sign-extended `int24` (ticks), decoded from all 256 bits.
#[inline]
pub fn word_i24(w: &[u8]) -> Option<i32> {
    if w.len() != WORD_HEX {
        return None;
    }
    let low = fold_u128(&w[56..])? as u32 as i32; // the low 32 bits
    let neg = low < 0;
    if !all_digit(&w[..56], if neg { b'f' } else { b'0' }) {
        return None;
    }
    if !(-(1 << 23)..(1 << 23)).contains(&low) {
        return None;
    }
    Some(low)
}

/// Render `bytes` as `0x` + lowercase hex into `dst`; returns the length.
/// `None` if `dst` is too small.
#[inline]
pub fn render_hex(dst: &mut [u8], bytes: &[u8]) -> Option<usize> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let n = 2 + 2 * bytes.len();
    if dst.len() < n {
        return None;
    }
    dst[0] = b'0';
    dst[1] = b'x';
    let mut i = 0;
    while i < bytes.len() {
        dst[2 + 2 * i] = HEX[(bytes[i] >> 4) as usize];
        dst[3 + 2 * i] = HEX[(bytes[i] & 0x0f) as usize];
        i += 1;
    }
    Some(n)
}

/// Render a `QUANTITY` (`0x` + minimal lowercase hex, `0x0` for zero).
#[inline]
pub fn render_quantity(dst: &mut [u8], v: u64) -> Option<usize> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digits = if v == 0 {
        1
    } else {
        (16 - v.leading_zeros() as usize / 4).max(1)
    };
    let n = 2 + digits;
    if dst.len() < n {
        return None;
    }
    dst[0] = b'0';
    dst[1] = b'x';
    let mut i = 0;
    while i < digits {
        let shift = 4 * (digits - 1 - i);
        dst[2 + i] = HEX[((v >> shift) & 0xf) as usize];
        i += 1;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u8> {
        assert_eq!(s.len(), 64);
        s.as_bytes().to_vec()
    }

    #[test]
    fn an_address_word_renders_lowercase_from_its_own_digits() {
        let mut d = [0u8; 42];
        let a = w("000000000000000000000000AbCdEf0123456789abcdef0123456789ABCDEF01");
        assert!(word_addr_hex(&a, &mut d));
        assert_eq!(&d[..], b"0xabcdef0123456789abcdef0123456789abcdef01");
        let high = w("000000000000000000000001abcdef0123456789abcdef0123456789abcdef01");
        assert!(!word_addr_hex(&high, &mut d), "a set bit above 160");
        let zero = w("0000000000000000000000000000000000000000000000000000000000000000");
        assert!(!word_addr_hex(&zero, &mut d), "the zero address");
        let junk = w("000000000000000000000000abcdef0123456789abcdef0123456789abcdefzz");
        assert!(!word_addr_hex(&junk, &mut d), "a non-hex digit");
        assert!(!word_addr_hex(&a[..63], &mut d), "not a whole word");
    }

    #[test]
    fn signed_words_are_decoded_from_all_256_bits() {
        let minus_one = w("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert_eq!(word_i128(&minus_one), Some(-1));
        let big_pos = w("000000000000000000000000000000007fffffffffffffffffffffffffffffff");
        assert_eq!(word_i128(&big_pos), Some(i128::MAX));
        // Low 128 bits look negative but the upper half is zero: a
        // uint256 above i128::MAX — refused, never reinterpreted.
        let fake_neg = w("00000000000000000000000000000000ffffffffffffffffffffffffffffffff");
        assert_eq!(word_i128(&fake_neg), None);
        // Upper half ones with a positive low half: refused too.
        let fake_pos = w("ffffffffffffffffffffffffffffffff00000000000000000000000000000001");
        assert_eq!(word_i128(&fake_pos), None);
        assert_eq!(word_i24(&minus_one), Some(-1));
        let t = w("fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffb7618");
        assert_eq!(word_i24(&t), Some(-297_448));
        let over = w("0000000000000000000000000000000000000000000000000000000000800000");
        assert_eq!(word_i24(&over), None);
    }

    #[test]
    fn unsigned_words_refuse_high_bits() {
        let v = format!("{}00000001{:032x}", "0".repeat(24), 2);
        let v = w(&v);
        assert_eq!(word_u160(&v), Some((2, 1)));
        assert_eq!(word_u128(&v), None);
        let over = format!("{}{}{}", "0".repeat(23), "1", "0".repeat(40));
        assert_eq!(word_u160(&w(&over)), None, "bit 160 set");
        let v = w("0000000000000000000000000000000000000000000000000000000000000bb8");
        assert_eq!(word_u32(&v), Some(3000));
    }

    #[test]
    fn quantities_and_fixed_widths() {
        assert_eq!(hex_quantity_u128(b"0x2c7e1a0\"", 0), Some((0x2c7e1a0, 9)));
        assert_eq!(hex_quantity_u128(b"0x\"", 0), None);
        let id = b"0xd5a6e2f0c7e44b3b9a1e0f3a1c2b3d4e";
        assert_eq!(
            hex_quantity_u128(id, 0).map(|x| x.0),
            Some(0xd5a6e2f0c7e44b3b9a1e0f3a1c2b3d4e)
        );
        assert_eq!(
            hex_quantity_u128(b"0x100000000000000000000000000000000", 0),
            None,
            "33 digits"
        );
        assert_eq!(hex_quantity_u64(b"0x10000000000000000", 0), None);
        assert_eq!(hex_fixed::<2>(b"0xABcd\"", 0), Some(([0xab, 0xcd], 6)));
        assert_eq!(hex_fixed::<2>(b"0xabcde", 0), None, "longer than the width");
        assert_eq!(hex_fixed::<2>(b"0xabc", 0), None, "shorter than the width");
        assert_eq!(data_words(b"0x\"", 0), Some((2, 0, 2)));
        assert_eq!(data_words(b"0x0\"", 0), None);
    }

    #[test]
    fn renders() {
        let mut b = [0u8; 18];
        let n = render_quantity(&mut b, 0).unwrap();
        assert_eq!(&b[..n], b"0x0");
        let n = render_quantity(&mut b, 0x2c7e1a0).unwrap();
        assert_eq!(&b[..n], b"0x2c7e1a0");
        let n = render_quantity(&mut b, u64::MAX).unwrap();
        assert_eq!(&b[..n], b"0xffffffffffffffff");
        assert_eq!(render_quantity(&mut b[..17], u64::MAX), None);
        let n = render_hex(&mut b, &[0xde, 0xad]).unwrap();
        assert_eq!(&b[..n], b"0xdead");
        assert_eq!(render_hex(&mut b[..3], &[0xde, 0xad]), None);
    }
}
