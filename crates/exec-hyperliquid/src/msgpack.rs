// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A zero-allocation MessagePack writer, sized for exactly what
//! Hyperliquid's L1 actions contain and nothing more.
//!
//! ## Why hand-written
//!
//! The same reason every wire format in this tree is hand-written: a
//! general encoder would need a map type, and a map type would need an
//! allocation and an iteration order. **LAW E-3 — msgpack key order is
//! part of the signature.** A different order is a different hash is a
//! rejected order. So there is no map here at all: the action encoders
//! call `map_header(n)` and then write exactly `n` keys in a fixed
//! compile-time order. The order is a property of the CODE, checked by
//! the known-answer vectors, and it cannot drift at runtime.
//!
//! ## Encoding rules, matched to the Python `msgpack` the SDK uses
//!
//! Integers are packed MINIMALLY — the smallest encoding that holds
//! the value — because that is what `msgpack.packb` does, and a
//! non-minimal encoding of the same number is different bytes and
//! therefore a different signature.
//!
//! | value | encoding |
//! |---|---|
//! | `0 ..= 127` | positive fixint, one byte |
//! | `..= 0xFF` | `0xcc` + u8 |
//! | `..= 0xFFFF` | `0xcd` + BE u16 |
//! | `..= 0xFFFF_FFFF` | `0xce` + BE u32 |
//! | else | `0xcf` + BE u64 |
//!
//! Strings, arrays and maps follow the same minimal-width rule.
//!
//! Every writer returns [`MsgPackErr::Overflow`] rather than panicking
//! or truncating: a half-written action must never reach a signer.

/// The writer ran out of room.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MsgPackErr {
    /// The destination buffer could not hold the next token. The
    /// buffer's contents are UNDEFINED after this and must be
    /// discarded, never signed.
    Overflow,
}

impl core::fmt::Display for MsgPackErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "msgpack: buffer overflow")
    }
}

impl std::error::Error for MsgPackErr {}

/// A cursor over a caller-owned buffer. Holds no allocation of its own
/// and never grows: the caller sizes the buffer at boot.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    /// Wrap a buffer. The cursor starts at zero.
    #[inline(always)]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Bytes written so far.
    #[inline(always)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Nothing written yet.
    #[inline(always)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What has been written.
    #[inline(always)]
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    #[inline(always)]
    fn put(&mut self, b: u8) -> Result<(), MsgPackErr> {
        if self.len >= self.buf.len() {
            return Err(MsgPackErr::Overflow);
        }
        // SAFETY: bounds checked immediately above.
        unsafe { *self.buf.get_unchecked_mut(self.len) = b };
        self.len += 1;
        Ok(())
    }

    #[inline(always)]
    fn put_all(&mut self, bytes: &[u8]) -> Result<(), MsgPackErr> {
        let end = self
            .len
            .checked_add(bytes.len())
            .ok_or(MsgPackErr::Overflow)?;
        if end > self.buf.len() {
            return Err(MsgPackErr::Overflow);
        }
        // COPY: the serialiser's own write — caller literals (keys,
        // ≤ COIN_MAX coin names, 32 B cloid hex) into the boot-owned
        // action buffer that is keccak'd and sent. The wire bytes have
        // to exist contiguously somewhere; this is that place, written
        // once. There is no second buffer downstream of it.
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// Map header for `n` key/value pairs.
    #[inline(always)]
    pub fn map_header(&mut self, n: usize) -> Result<(), MsgPackErr> {
        if n < 16 {
            self.put(0x80 | (n as u8))
        } else if n <= u16::MAX as usize {
            self.put(0xde)?;
            self.put_all(&(n as u16).to_be_bytes())
        } else {
            self.put(0xdf)?;
            self.put_all(&(n as u32).to_be_bytes())
        }
    }

    /// Array header for `n` elements.
    #[inline(always)]
    pub fn array_header(&mut self, n: usize) -> Result<(), MsgPackErr> {
        if n < 16 {
            self.put(0x90 | (n as u8))
        } else if n <= u16::MAX as usize {
            self.put(0xdc)?;
            self.put_all(&(n as u16).to_be_bytes())
        } else {
            self.put(0xdd)?;
            self.put_all(&(n as u32).to_be_bytes())
        }
    }

    /// A UTF-8 string, as raw bytes the caller has already validated.
    #[inline(always)]
    pub fn str_bytes(&mut self, s: &[u8]) -> Result<(), MsgPackErr> {
        let n = s.len();
        if n < 32 {
            self.put(0xa0 | (n as u8))?;
        } else if n <= u8::MAX as usize {
            self.put(0xd9)?;
            self.put(n as u8)?;
        } else if n <= u16::MAX as usize {
            self.put(0xda)?;
            self.put_all(&(n as u16).to_be_bytes())?;
        } else {
            self.put(0xdb)?;
            self.put_all(&(n as u32).to_be_bytes())?;
        }
        self.put_all(s)
    }

    /// An unsigned integer, MINIMALLY encoded.
    #[inline(always)]
    pub fn uint(&mut self, v: u64) -> Result<(), MsgPackErr> {
        if v < 128 {
            self.put(v as u8)
        } else if v <= u8::MAX as u64 {
            self.put(0xcc)?;
            self.put(v as u8)
        } else if v <= u16::MAX as u64 {
            self.put(0xcd)?;
            self.put_all(&(v as u16).to_be_bytes())
        } else if v <= u32::MAX as u64 {
            self.put(0xce)?;
            self.put_all(&(v as u32).to_be_bytes())
        } else {
            self.put(0xcf)?;
            self.put_all(&v.to_be_bytes())
        }
    }

    /// A boolean.
    #[inline(always)]
    pub fn bool(&mut self, v: bool) -> Result<(), MsgPackErr> {
        self.put(if v { 0xc3 } else { 0xc2 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(f: impl FnOnce(&mut Writer<'_>) -> Result<(), MsgPackErr>) -> Vec<u8> {
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        f(&mut w).expect("fits");
        w.as_slice().to_vec()
    }

    /// Pinned against real `msgpack.packb` output. A non-minimal
    /// encoding of the same number is different BYTES and therefore a
    /// different signature — this is not a style preference.
    #[test]
    fn integers_are_minimally_encoded() {
        assert_eq!(enc(|w| w.uint(0)), vec![0x00]);
        assert_eq!(enc(|w| w.uint(1)), vec![0x01]);
        assert_eq!(enc(|w| w.uint(127)), vec![0x7f]);
        assert_eq!(enc(|w| w.uint(128)), vec![0xcc, 0x80]);
        assert_eq!(enc(|w| w.uint(255)), vec![0xcc, 0xff]);
        assert_eq!(enc(|w| w.uint(256)), vec![0xcd, 0x01, 0x00]);
        assert_eq!(enc(|w| w.uint(65_535)), vec![0xcd, 0xff, 0xff]);
        assert_eq!(enc(|w| w.uint(65_536)), vec![0xce, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(
            enc(|w| w.uint(4_294_967_295)),
            vec![0xce, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            enc(|w| w.uint(4_294_967_296)),
            vec![0xcf, 0, 0, 0, 1, 0, 0, 0, 0]
        );
        // The HIP-4 asset id range: 100_000_000 + enc lands in u32.
        assert_eq!(enc(|w| w.uint(100_032_530)), vec![0xce, 0x05, 0xf6, 0x60, 0x12]);
    }

    #[test]
    fn strings_match_the_observed_sdk_bytes() {
        // Straight out of the committed vectors: "type" and "order".
        assert_eq!(enc(|w| w.str_bytes(b"type")), vec![0xa4, b't', b'y', b'p', b'e']);
        assert_eq!(
            enc(|w| w.str_bytes(b"order")),
            vec![0xa5, b'o', b'r', b'd', b'e', b'r']
        );
        assert_eq!(enc(|w| w.str_bytes(b"")), vec![0xa0]);
        // 31 bytes is the last fixstr; 32 crosses to str8.
        let s31 = vec![b'x'; 31];
        let s32 = vec![b'x'; 32];
        assert_eq!(enc(|w| w.str_bytes(&s31))[0], 0xbf);
        assert_eq!(&enc(|w| w.str_bytes(&s32))[..2], &[0xd9, 32]);
    }

    #[test]
    fn map_and_array_headers_match() {
        assert_eq!(enc(|w| w.map_header(3)), vec![0x83]);
        assert_eq!(enc(|w| w.map_header(6)), vec![0x86]);
        assert_eq!(enc(|w| w.array_header(1)), vec![0x91]);
        assert_eq!(enc(|w| w.map_header(15)), vec![0x8f]);
        assert_eq!(enc(|w| w.map_header(16)), vec![0xde, 0x00, 0x10]);
        assert_eq!(enc(|w| w.array_header(15)), vec![0x9f]);
        assert_eq!(enc(|w| w.array_header(16)), vec![0xdc, 0x00, 0x10]);
    }

    #[test]
    fn booleans_match() {
        assert_eq!(enc(|w| w.bool(true)), vec![0xc3]);
        assert_eq!(enc(|w| w.bool(false)), vec![0xc2]);
    }

    /// An overflow must be an ERROR, never a truncation — a
    /// half-written action that reached a signer would be a valid
    /// signature over the wrong thing.
    #[test]
    fn overflow_is_refused_not_truncated() {
        let mut buf = [0u8; 4];
        let mut w = Writer::new(&mut buf);
        assert!(w.str_bytes(b"type").is_err(), "5 bytes into a 4-byte buffer");

        let mut buf = [0u8; 2];
        let mut w = Writer::new(&mut buf);
        assert_eq!(w.uint(1), Ok(()));
        assert_eq!(w.uint(1), Ok(()));
        assert_eq!(w.uint(1), Err(MsgPackErr::Overflow));

        // A zero-length buffer refuses everything.
        let mut buf = [0u8; 0];
        let mut w = Writer::new(&mut buf);
        assert_eq!(w.bool(true), Err(MsgPackErr::Overflow));
        assert!(w.is_empty());
    }

    #[test]
    fn the_cursor_reports_what_it_wrote() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        assert!(w.is_empty());
        w.map_header(1).unwrap();
        w.str_bytes(b"a").unwrap();
        w.uint(0).unwrap();
        assert_eq!(w.len(), 4);
        assert_eq!(w.as_slice(), &[0x81, 0xa1, b'a', 0x00]);
    }
}
