// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The renderers every request is written with: decimal digits, lowercase
//! hex and literal fragments, each into `dst[at..]`, returning the new
//! end. A buffer too short is a sizing bug: the bounds check aborts
//! (fail-fast; every caller's buffer is a const-sized array).

/// The number of decimal digits of `v` (1 for 0).
#[inline]
#[must_use]
pub fn digits(v: u64) -> usize {
    let mut n = 1usize;
    let mut x = v;
    while x >= 10 {
        x /= 10;
        n += 1;
    }
    n
}

/// `v` in decimal at `dst[at..]`, written right to left in place.
#[inline]
pub fn put_u64(dst: &mut [u8], at: usize, v: u64) -> usize {
    let n = digits(v);
    let mut x = v;
    let mut i = at + n;
    while i > at {
        i -= 1;
        dst[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    at + n
}

/// The bytes of `s` at `dst[at..]`.
#[inline]
pub fn put(dst: &mut [u8], at: usize, s: &[u8]) -> usize {
    // COPY: a literal fragment or a boot-rendered field into the request
    // being rendered — rendering IS writing these bytes; the caller sends
    // the buffer itself (no second copy of ours).
    dst[at..at + s.len()].copy_from_slice(s);
    at + s.len()
}

/// `b` as lowercase hex (no `0x`) at `dst[at..]`.
#[inline]
pub fn put_hex(dst: &mut [u8], at: usize, b: &[u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut i = 0usize;
    let mut o = at;
    while i < b.len() {
        dst[o] = HEX[(b[i] >> 4) as usize];
        dst[o + 1] = HEX[(b[i] & 0x0f) as usize];
        o += 2;
        i += 1;
    }
    o
}

/// The 32 bytes of a `0x`-prefixed 64-hex-digit text (either case);
/// `None` otherwise.
#[must_use]
pub fn hex32(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 66 || text[0] != b'0' || (text[1] != b'x' && text[1] != b'X') {
        return None;
    }
    let mut out = [0u8; 32];
    let mut i = 0usize;
    while i < 32 {
        let hi = nibble(text[2 + 2 * i])?;
        let lo = nibble(text[3 + 2 * i])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

/// The 20 bytes of a `0x`-prefixed 40-hex-digit address (either case;
/// the checksum is not judged here — boot compares the rendered EIP-55).
#[must_use]
pub fn hex20(text: &[u8]) -> Option<[u8; 20]> {
    if text.len() != 42 || text[0] != b'0' || (text[1] != b'x' && text[1] != b'X') {
        return None;
    }
    let mut out = [0u8; 20];
    let mut i = 0usize;
    while i < 20 {
        let hi = nibble(text[2 + 2 * i])?;
        let lo = nibble(text[3 + 2 * i])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

#[inline]
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_hex_and_fragments_render_in_place() {
        let mut b = [0u8; 64];
        let mut at = put(&mut b, 0, b"n=");
        at = put_u64(&mut b, at, 0);
        at = put(&mut b, at, b",");
        at = put_u64(&mut b, at, 18_446_744_073_709_551_615);
        at = put(&mut b, at, b",");
        at = put_hex(&mut b, at, &[0x00, 0xab, 0xff]);
        assert_eq!(&b[..at], b"n=0,18446744073709551615,00abff");
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(1_792_217_690_123), 13);
    }

    #[test]
    fn hex32_and_hex20_read_either_case_and_refuse_the_rest() {
        let t = b"0x2AFCA94626db91b2d556c351584a4cd93e13da32e377dc48c7983e43afa5ab47";
        let v = hex32(t).unwrap();
        assert_eq!(v[0], 0x2a);
        assert_eq!(v[31], 0x47);
        assert!(hex32(&t[..65]).is_none());
        assert!(
            hex32(b"0x2afca94626db91b2d556c351584a4cd93e13da32e377dc48c7983e43afa5ab4g").is_none()
        );
        let a = hex20(b"0xE111180000d2663C0091e4f400237545B87B996B").unwrap();
        assert_eq!(a, signer_eip712::pmv2::EXCHANGE_V2);
        assert!(hex20(b"E111180000d2663C0091e4f400237545B87B996B00").is_none());
    }
}
