// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! L2 auth (spec §B.1): `POLY_SIGNATURE` = URL-safe, `=`-padded base64 of
//! `HMAC-SHA256(base64url_decode(secret), ts ‖ METHOD ‖ path ‖ body)`.
//!
//! * `ts` — Unix **seconds**, the `POLY_TIMESTAMP` header's own digits;
//! * `METHOD` — uppercase;
//! * `path` — the path **without** its query (the query is not signed);
//! * `body` — the exact bytes sent, absent for a request without one.
//!
//! The MAC absorbs the request's own spans ([`L2Secret::sign`] takes
//! them as parts): the message is never staged.

use zeroize::{Zeroize, ZeroizeOnDrop};

/// `POLY_SIGNATURE`'s length: base64 of a 32 B tag, padded.
pub const L2_SIG_LEN: usize = 44;

/// The largest decoded secret held (the venue's are 32 B).
pub const SECRET_MAX: usize = 64;

/// Why a secret does not load.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum L2Err {
    /// Not base64 in either alphabet.
    Base64,
    /// Empty, or longer than [`SECRET_MAX`] decoded.
    Length,
}

/// The decoded API secret — wiped on drop, never printed.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct L2Secret {
    key: [u8; SECRET_MAX],
    len: usize,
}

impl L2Secret {
    /// Decode the API secret's base64 text (either alphabet, padding
    /// optional — the SDKs accept both). Boot-time.
    ///
    /// # Errors
    /// [`L2Err`] — the arm does not boot.
    pub fn from_base64(text: &[u8]) -> Result<Self, L2Err> {
        if text.is_empty() || core_crypto::base64_decoded_max_len(text.len()) > SECRET_MAX {
            return Err(L2Err::Length);
        }
        let mut s = Self {
            key: [0; SECRET_MAX],
            len: 0,
        };
        match core_crypto::base64_decode(text, &mut s.key) {
            Ok(n) if n > 0 => {
                s.len = n;
                Ok(s)
            }
            Ok(_) => Err(L2Err::Length),
            Err(_) => Err(L2Err::Base64),
        }
    }

    /// `POLY_SIGNATURE` over the concatenation of `parts` (`ts`,
    /// `METHOD`, `path`, then the body's spans).
    #[inline]
    #[must_use]
    pub fn sign(&self, parts: &[&[u8]]) -> [u8; L2_SIG_LEN] {
        let tag = core_crypto::hmac_sha256_parts(&self.key[..self.len], parts);
        let mut out = [0u8; L2_SIG_LEN];
        let n = core_crypto::base64url_encode(&tag, &mut out);
        debug_assert_eq!(n, L2_SIG_LEN);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDK_SECRET: &[u8] = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    /// SDK: py tests/signing/test_hmac.py:9-23, ts tests/signing/hmac.test.ts:5-17
    /// (the body WITH a space), rs src/auth.rs:392-415 (without) and
    /// rs src/auth.rs:335-373 (no body).
    #[test]
    fn sdk_hmac_vectors() {
        let s = L2Secret::from_base64(SDK_SECRET).unwrap();
        assert_eq!(
            &s.sign(&[
                b"1000000",
                b"test-sign",
                b"/orders",
                br#"{"hash": "0x123"}"#
            ]),
            b"ZwAdJKvoYRlEKDkNMwd5BuwNNtg93kNaR_oU2HrfVvc="
        );
        assert_eq!(
            &s.sign(&[b"1000000", b"test-sign", b"/orders", br#"{"hash":"0x123"}"#]),
            b"4gJVbox-R6XlDK4nlaicig0_ANVL1qdcahiL8CXfXLM="
        );
        assert_eq!(
            &s.sign(&[b"1", b"GET", b"/"]),
            b"eHaylCwqRSOa2LFD77Nt_SaTpbsxzN8eTEI3LryhEj4="
        );
    }

    /// VER: the MAC over a whole order body, split anywhere, is one MAC.
    #[test]
    fn ver_hmac_over_an_order_body_in_two_spans() {
        let body: &[u8] = br#"{"order":{"salt":479249096354,"maker":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","signer":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","tokenId":"1234","makerAmount":"5200000","takerAmount":"10000000","side":"BUY","expiration":"0","signatureType":0,"timestamp":"1780449126930","metadata":"0x0000000000000000000000000000000000000000000000000000000000000000","builder":"0x0000000000000000000000000000000000000000000000000000000000000000","signature":"0xSIG"},"owner":"00000000-0000-0000-0000-000000000000","orderType":"GTC","deferExec":false,"postOnly":false}"#;
        let s = L2Secret::from_base64(SDK_SECRET).unwrap();
        let want: &[u8; 44] = b"fsWxxxgNmOGHMGBHXU5bnY1vEoT82vOHZWF8txkUfjU=";
        assert_eq!(&s.sign(&[b"1780449126", b"POST", b"/order", body]), want);
        let mut cut = 0usize;
        while cut <= body.len() {
            assert_eq!(
                &s.sign(&[
                    b"1780449126",
                    b"POST",
                    b"/order",
                    &body[..cut],
                    &body[cut..]
                ]),
                want
            );
            cut += 37;
        }
    }

    #[test]
    fn a_secret_in_either_alphabet_loads_and_garbage_does_not() {
        // 0xfb 0xff …: `+/` and `-_` spell the same key.
        let a = L2Secret::from_base64(b"-_8AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        let b = L2Secret::from_base64(b"+/8AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert_eq!(a.sign(&[b"1", b"GET", b"/"]), b.sign(&[b"1", b"GET", b"/"]));
        assert_eq!(L2Secret::from_base64(b"").err(), Some(L2Err::Length));
        assert_eq!(L2Secret::from_base64(b"!!!!").err(), Some(L2Err::Base64));
        assert_eq!(
            L2Secret::from_base64(&[b'A'; 120]).err(),
            Some(L2Err::Length)
        );
    }
}
