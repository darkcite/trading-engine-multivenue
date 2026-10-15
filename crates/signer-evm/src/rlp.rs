// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Minimal RLP: byte strings and list headers, each encoded into a small
//! stack array. Large payloads (calldata) are never copied here — only
//! their HEADER is encoded; the bytes stay borrowed.

/// An encoded RLP item or header, at most 33 bytes (a 32-byte string
/// plus its one-byte header), on the stack.
#[derive(Copy, Clone)]
pub(crate) struct Enc {
    buf: [u8; 33],
    len: u8,
}

impl Enc {
    const EMPTY: Self = Self {
        buf: [0; 33],
        len: 0,
    };

    #[inline(always)]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    #[inline(always)]
    pub(crate) const fn len(&self) -> usize {
        self.len as usize
    }
}

/// Number of bytes in the minimal big-endian form of `v` (0 for 0).
#[inline(always)]
const fn be_len(v: u128) -> usize {
    (128 - v.leading_zeros() as usize).div_ceil(8)
}

/// RLP of an unsigned integer: minimal big-endian bytes as a string.
/// `0` is the empty string `0x80`; `1..=0x7f` is the byte itself.
pub(crate) const fn uint(v: u128) -> Enc {
    let mut e = Enc::EMPTY;
    if v == 0 {
        e.buf[0] = 0x80;
        e.len = 1;
        return e;
    }
    if v < 0x80 {
        e.buf[0] = v as u8;
        e.len = 1;
        return e;
    }
    let n = be_len(v);
    e.buf[0] = 0x80 + n as u8;
    let mut i = 0;
    while i < n {
        e.buf[1 + i] = (v >> (8 * (n - 1 - i))) as u8;
        i += 1;
    }
    e.len = (1 + n) as u8;
    e
}

/// RLP of a 32-byte big-endian WORD read as an integer (`r`, `s`),
/// borrowed straight out of the signature: leading zero bytes are
/// stripped — an integer, not a fixed string.
pub(crate) const fn word(w: &[u8]) -> Enc {
    debug_assert!(w.len() == 32);
    let mut z = 0;
    while z < w.len() && w[z] == 0 {
        z += 1;
    }
    let mut e = Enc::EMPTY;
    let n = 32 - z;
    if n == 0 {
        e.buf[0] = 0x80;
        e.len = 1;
        return e;
    }
    if n == 1 && w[w.len() - 1] < 0x80 {
        e.buf[0] = w[w.len() - 1];
        e.len = 1;
        return e;
    }
    e.buf[0] = 0x80 + n as u8;
    let mut i = 0;
    while i < n {
        e.buf[1 + i] = w[z + i];
        i += 1;
    }
    e.len = (1 + n) as u8;
    e
}

/// RLP of a 20-byte address: always the 21-byte string `0x94 ‖ addr`.
pub(crate) const fn address(a: &[u8; 20]) -> Enc {
    let mut e = Enc::EMPTY;
    e.buf[0] = 0x94;
    let mut i = 0;
    while i < 20 {
        e.buf[1 + i] = a[i];
        i += 1;
    }
    e.len = 21;
    e
}

/// Header of a byte string of `len` bytes whose first byte is `first`
/// (ignored unless `len == 1`). A single byte below `0x80` IS its own
/// encoding, so its header is empty.
pub(crate) const fn string_header(len: usize, first: u8) -> Enc {
    let mut e = Enc::EMPTY;
    if len == 1 && first < 0x80 {
        return e;
    }
    if len <= 55 {
        e.buf[0] = 0x80 + len as u8;
        e.len = 1;
        return e;
    }
    let n = be_len(len as u128);
    e.buf[0] = 0xb7 + n as u8;
    let mut i = 0;
    while i < n {
        e.buf[1 + i] = (len >> (8 * (n - 1 - i))) as u8;
        i += 1;
    }
    e.len = (1 + n) as u8;
    e
}

/// Header of a list whose concatenated items take `payload` bytes.
pub(crate) const fn list_header(payload: usize) -> Enc {
    let mut e = Enc::EMPTY;
    if payload <= 55 {
        e.buf[0] = 0xc0 + payload as u8;
        e.len = 1;
        return e;
    }
    let n = be_len(payload as u128);
    e.buf[0] = 0xf7 + n as u8;
    let mut i = 0;
    while i < n {
        e.buf[1 + i] = (payload >> (8 * (n - 1 - i))) as u8;
        i += 1;
    }
    e.len = (1 + n) as u8;
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Ethereum wiki's RLP examples.
    #[test]
    fn wiki_vectors() {
        assert_eq!(uint(0).as_slice(), &[0x80]);
        assert_eq!(uint(15).as_slice(), &[0x0f]);
        assert_eq!(uint(1024).as_slice(), &[0x82, 0x04, 0x00]);
        assert_eq!(uint(0x7f).as_slice(), &[0x7f]);
        assert_eq!(uint(0x80).as_slice(), &[0x81, 0x80]);
        assert_eq!(uint(u128::MAX).len(), 17);
        let dog = b"dog";
        let mut v = string_header(dog.len(), dog[0]).as_slice().to_vec();
        v.extend_from_slice(dog);
        assert_eq!(v, [0x83, b'd', b'o', b'g']);
        // ["cat", "dog"]
        assert_eq!(list_header(8).as_slice(), &[0xc8]);
        assert_eq!(list_header(0).as_slice(), &[0xc0]);
        let lorem = b"Lorem ipsum dolor sit amet, consectetur adipisicing elit";
        assert_eq!(lorem.len(), 56);
        assert_eq!(
            string_header(lorem.len(), lorem[0]).as_slice(),
            &[0xb8, 0x38]
        );
        assert_eq!(string_header(1, 0x7f).len(), 0);
        assert_eq!(string_header(1, 0x80).as_slice(), &[0x81]);
        assert_eq!(string_header(0, 0).as_slice(), &[0x80]);
        assert_eq!(string_header(1024, 0).as_slice(), &[0xb9, 0x04, 0x00]);
        assert_eq!(list_header(1024).as_slice(), &[0xf9, 0x04, 0x00]);
    }

    #[test]
    fn word_strips_leading_zeros_as_an_integer() {
        let mut w = [0u8; 32];
        assert_eq!(word(&w).as_slice(), &[0x80]);
        w[31] = 0x05;
        assert_eq!(word(&w).as_slice(), &[0x05]);
        w[30] = 0x01;
        assert_eq!(word(&w).as_slice(), &[0x82, 0x01, 0x05]);
        let full = [0xffu8; 32];
        assert_eq!(word(&full).len(), 33);
    }
}
