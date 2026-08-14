//! Zero-alloc JSON encoder for signed Polymarket orders.
//!
//! Polymarket CLOB POST `/order` body (Phase 3 v1 shape):
//!
//! ```json
//! {
//!   "order": {
//!     "salt": "12345",
//!     "maker": "0xabcdef0123456789...",
//!     "signer": "0xabcdef0123456789...",
//!     "taker": "0x0000000000000000000000000000000000000000",
//!     "tokenId": "0xabc...",
//!     "makerAmount": "10000000",
//!     "takerAmount": "5000000",
//!     "expiration": "0",
//!     "nonce": "0",
//!     "feeRateBps": "0",
//!     "side": 0,
//!     "signatureType": 0,
//!     "signature": "0xabcdef0123456789..."
//!   },
//!   "owner": "0xabcdef0123456789...",
//!   "orderType": "GTC"
//! }
//! ```
//!
//! All numeric fields except `side` and `signatureType` are sent as
//! decimal strings (per Polymarket's spec; their verifier accepts
//! either but their docs use strings for arbitrary-precision
//! ints). Addresses + token id are 0x-prefixed hex.
//!
//! No `serde`, no heap; one pass over the input.

use signer_eip712::OrderToSign;

/// Why an encode call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum JsonEncodeErr {
    /// The destination buffer is too small for the encoded body.
    BufferTooSmall,
}

/// Order-type tag wired into the JSON envelope. Phase 3 emits only
/// `"GTC"` (good-till-cancelled); other types are future work.
pub const ORDER_TYPE_GTC: &[u8] = b"GTC";

/// Encode a Polymarket-shaped POST body into `dst`. Returns bytes
/// written.
///
/// * `order` — already-signed order parameters.
/// * `signature` — 65-byte `r || s || v` from
///   `signer_eip712::sign_order`.
/// * `owner` — Polymarket account id (typically the same 20-byte
///   address as `order.maker`).
/// * `order_type` — pass `ORDER_TYPE_GTC` for v1.
pub fn encode_signed_order(
    dst: &mut [u8],
    order: &OrderToSign,
    signature: &[u8; 65],
    owner: &[u8; 20],
    order_type: &[u8],
) -> Result<usize, JsonEncodeErr> {
    let mut c = Cursor::new(dst);
    c.put(b"{\"order\":{")?;

    c.put(b"\"salt\":\"")?;
    c.put_u128_dec(order.salt as u128)?;
    c.put(b"\",")?;

    c.put(b"\"maker\":\"")?;
    c.put_address_hex(&order.maker)?;
    c.put(b"\",")?;

    c.put(b"\"signer\":\"")?;
    c.put_address_hex(&order.signer)?;
    c.put(b"\",")?;

    c.put(b"\"taker\":\"")?;
    c.put_address_hex(&order.taker)?;
    c.put(b"\",")?;

    c.put(b"\"tokenId\":\"")?;
    c.put_b32_hex(&order.token_id)?;
    c.put(b"\",")?;

    c.put(b"\"makerAmount\":\"")?;
    c.put_u128_dec(order.maker_amount)?;
    c.put(b"\",")?;

    c.put(b"\"takerAmount\":\"")?;
    c.put_u128_dec(order.taker_amount)?;
    c.put(b"\",")?;

    c.put(b"\"expiration\":\"")?;
    c.put_u128_dec(order.expiration as u128)?;
    c.put(b"\",")?;

    c.put(b"\"nonce\":\"")?;
    c.put_u128_dec(order.nonce as u128)?;
    c.put(b"\",")?;

    c.put(b"\"feeRateBps\":\"")?;
    c.put_u128_dec(order.fee_rate_bps as u128)?;
    c.put(b"\",")?;

    c.put(b"\"side\":")?;
    c.put_u128_dec(order.side as u128)?;
    c.put(b",")?;

    c.put(b"\"signatureType\":")?;
    c.put_u128_dec(order.signature_type as u128)?;
    c.put(b",")?;

    c.put(b"\"signature\":\"0x")?;
    c.put_hex_lower(signature)?;
    c.put(b"\"")?;

    c.put(b"},\"owner\":\"")?;
    c.put_address_hex(owner)?;
    c.put(b"\",\"orderType\":\"")?;
    c.put(order_type)?;
    c.put(b"\"}")?;

    Ok(c.pos)
}

// -----------------------------------------------------------------
// Cursor — bounded byte writer
// -----------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    fn put(&mut self, src: &[u8]) -> Result<(), JsonEncodeErr> {
        let end = self
            .pos
            .checked_add(src.len())
            .ok_or(JsonEncodeErr::BufferTooSmall)?;
        if end > self.buf.len() {
            return Err(JsonEncodeErr::BufferTooSmall);
        }
        self.buf[self.pos..end].copy_from_slice(src);
        self.pos = end;
        Ok(())
    }

    /// Decimal u128. Max length 39 digits.
    #[inline]
    fn put_u128_dec(&mut self, mut v: u128) -> Result<(), JsonEncodeErr> {
        if v == 0 {
            return self.put(b"0");
        }
        // 39 digits is the max for u128::MAX.
        let mut tmp = [0u8; 39];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.put(&tmp[i..])
    }

    /// 20-byte address as lowercase 0x-prefixed hex.
    #[inline]
    fn put_address_hex(&mut self, addr: &[u8; 20]) -> Result<(), JsonEncodeErr> {
        self.put(b"0x")?;
        self.put_hex_lower(addr)
    }

    /// 32-byte bytes32 as lowercase 0x-prefixed hex.
    #[inline]
    fn put_b32_hex(&mut self, b32: &[u8; 32]) -> Result<(), JsonEncodeErr> {
        self.put(b"0x")?;
        self.put_hex_lower(b32)
    }

    /// Lowercase hex of arbitrary bytes (no `0x` prefix).
    #[inline]
    fn put_hex_lower(&mut self, bytes: &[u8]) -> Result<(), JsonEncodeErr> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        // Two ASCII chars per byte. Pre-bounds check.
        let need = bytes.len() * 2;
        let end = self
            .pos
            .checked_add(need)
            .ok_or(JsonEncodeErr::BufferTooSmall)?;
        if end > self.buf.len() {
            return Err(JsonEncodeErr::BufferTooSmall);
        }
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            self.buf[self.pos] = HEX[(b >> 4) as usize];
            self.buf[self.pos + 1] = HEX[(b & 0x0f) as usize];
            self.pos += 2;
            i += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canned_order() -> OrderToSign {
        OrderToSign::new(
            42,
            [0xAAu8; 20],
            [0xAAu8; 20],
            [0u8; 20],
            [0x7au8; 32],
            10_000_000,
            5_000_000,
            0,
            0,
            0,
            0,
            0,
        )
    }

    #[test]
    fn encode_returns_byte_count() {
        let mut buf = [0u8; 4096];
        let sig = [0x12u8; 65];
        let owner = [0xAAu8; 20];
        let n = encode_signed_order(&mut buf, &canned_order(), &sig, &owner, ORDER_TYPE_GTC)
            .unwrap();
        assert!(n > 0);
        assert!(n <= buf.len());
    }

    #[test]
    fn encode_emits_valid_json_shape() {
        let mut buf = [0u8; 4096];
        let sig = [0x12u8; 65];
        let owner = [0xAAu8; 20];
        let n = encode_signed_order(&mut buf, &canned_order(), &sig, &owner, ORDER_TYPE_GTC)
            .unwrap();
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        // Top-level keys are present.
        assert!(s.starts_with("{\"order\":{"));
        assert!(s.ends_with("\"GTC\"}"));
        assert!(s.contains("\"salt\":\"42\""));
        assert!(s.contains("\"makerAmount\":\"10000000\""));
        assert!(s.contains("\"side\":0"));
        assert!(s.contains("\"signatureType\":0"));
        // Address + bytes32 + signature are 0x-prefixed lowercase hex.
        assert!(s.contains("\"maker\":\"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""));
        assert!(s.contains("\"taker\":\"0x0000000000000000000000000000000000000000\""));
        assert!(s.contains(
            "\"tokenId\":\"0x7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a\""
        ));
        // 65-byte sig → 130 hex chars after the 0x.
        let sig_marker = "\"signature\":\"0x";
        let off = s.find(sig_marker).expect("signature field present");
        let after = &s[off + sig_marker.len()..];
        let end = after.find('"').expect("signature field closes");
        assert_eq!(end, 130, "65-byte sig should be 130 hex digits");
    }

    #[test]
    fn encode_returns_overflow_on_tiny_buffer() {
        let mut buf = [0u8; 8];
        let sig = [0x12u8; 65];
        let owner = [0xAAu8; 20];
        let err = encode_signed_order(&mut buf, &canned_order(), &sig, &owner, ORDER_TYPE_GTC)
            .unwrap_err();
        assert_eq!(err, JsonEncodeErr::BufferTooSmall);
    }

    #[test]
    fn encode_handles_large_u128_amounts() {
        let mut buf = [0u8; 4096];
        let sig = [0u8; 65];
        let owner = [0xAAu8; 20];
        let mut o = canned_order();
        o.maker_amount = u128::MAX;
        o.taker_amount = 1;
        let n =
            encode_signed_order(&mut buf, &o, &sig, &owner, ORDER_TYPE_GTC).expect("encode");
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains(&format!("\"makerAmount\":\"{}\"", u128::MAX)));
        assert!(s.contains("\"takerAmount\":\"1\""));
    }

    #[test]
    fn encode_handles_zero_amount() {
        let mut buf = [0u8; 4096];
        let sig = [0u8; 65];
        let owner = [0xAAu8; 20];
        let mut o = canned_order();
        o.maker_amount = 0;
        let n =
            encode_signed_order(&mut buf, &o, &sig, &owner, ORDER_TYPE_GTC).expect("encode");
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("\"makerAmount\":\"0\""));
    }
}
