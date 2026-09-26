// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Binance-shaped payloads through the public API, against answers from an
//! INDEPENDENT implementation (OpenSSL through Python `cryptography` 46,
//! 2026-09-26), plus property tests that check every signature with
//! `ring`'s verifier.
//!
//! The spot WS API docs' Ed25519 example payload is used verbatim; the
//! docs do not publish their private key, so it is signed here under
//! RFC 8032 TEST 1's seed and TEST 2's.

use proptest::prelude::*;
use ring::signature::{UnparsedPublicKey, ED25519};
use signer_ed25519::{
    self_test, Ed25519Signer, RFC8032, SIG_B64_LEN, SIG_B64_PCT_MAX, SIG_LEN, SPKI_B64_LEN,
};

/// The spot WS API docs' Ed25519 signing example: every param but
/// `signature`, sorted by name, `name=value` joined by `&`.
const WSAPI_DOCS_PAYLOAD: &[u8] = b"apiKey=4yNzx3yWC5bS6YTwEkSRaC0nRmSQIIStAUOh1b6kqaBrTLIhjCpI5lJH8q8R8WNO&price=52000.00&quantity=0.01000000&recvWindow=100&side=SELL&symbol=BTCUSDT&timeInForce=GTC&timestamp=1645423376532&type=LIMIT";

/// A REST order query in the shape BX6 renders (32-character cid, §13.4).
const REST_QUERY: &[u8] = b"symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=IOC&quantity=0.00100&price=60000.00&newClientOrderId=mv65f0a1b23000000000000002a00000&newOrderRespType=ACK&recvWindow=1000&timestamp=1790000000000";

#[test]
fn the_ws_api_docs_payload_signs_as_openssl_signs_it() {
    let s = Ed25519Signer::from_seed(&RFC8032[0].seed).expect("signer");
    let mut b64 = [0u8; SIG_B64_LEN];
    assert_eq!(s.sign_b64(WSAPI_DOCS_PAYLOAD, &mut b64), SIG_B64_LEN);
    assert_eq!(
        &b64[..],
        b"Ws+5m/CMnpkko0uBFxGTZ2+fjqqBXsUjRiaz173fPhXTkhoDBYNZ6wcYNeWItdrGn1pvG7vkwx2fhmJdAZ3KDQ=="
    );
    let mut pct = [0u8; SIG_B64_PCT_MAX];
    let n = s.sign_b64_pct(WSAPI_DOCS_PAYLOAD, &mut pct);
    assert_eq!(
        &pct[..n],
        b"Ws%2B5m%2FCMnpkko0uBFxGTZ2%2BfjqqBXsUjRiaz173fPhXTkhoDBYNZ6wcYNeWItdrGn1pvG7vkwx2fhmJdAZ3KDQ%3D%3D"
    );
}

#[test]
fn a_rest_query_signs_as_openssl_signs_it() {
    let s = Ed25519Signer::from_seed(&RFC8032[1].seed).expect("signer");
    let mut pct = [0u8; SIG_B64_PCT_MAX];
    let n = s.sign_b64_pct(REST_QUERY, &mut pct);
    assert_eq!(
        &pct[..n],
        b"0HKIt1M93IYYJE5r3fn0nJe4gQnoEzqVwvJnPbGRH7pJwBQf39WQrwOWEOrMXZp9bDonGUMKQLLyuJykETeGDQ%3D%3D"
    );
}

#[test]
fn the_spki_is_the_pem_body_binance_shows() {
    let s = Ed25519Signer::from_seed(&RFC8032[0].seed).expect("signer");
    let mut out = [0u8; SPKI_B64_LEN];
    assert_eq!(s.public_key_spki_b64(&mut out), SPKI_B64_LEN);
    assert_eq!(&out[..], b"MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=");
}

#[test]
fn the_boot_self_test_passes() {
    assert_eq!(self_test(), Ok(()));
}

/// Test-only base64 decoder (the engine never decodes base64).
fn b64_decode(s: &[u8], out: &mut [u8; SIG_LEN]) -> bool {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    if s.len() != SIG_B64_LEN || &s[86..] != b"==" {
        return false;
    }
    let mut o = 0usize;
    let mut i = 0usize;
    while i + 4 <= 84 {
        let mut n = 0u32;
        for k in 0..4 {
            match val(s[i + k]) {
                Some(v) => n = (n << 6) | v,
                None => return false,
            }
        }
        out[o] = (n >> 16) as u8;
        out[o + 1] = (n >> 8) as u8;
        out[o + 2] = n as u8;
        o += 3;
        i += 4;
    }
    // Last group: two data chars + "==" → one byte.
    match (val(s[84]), val(s[85])) {
        (Some(a), Some(b)) => out[o] = ((a << 2) | (b >> 4)) as u8,
        _ => return false,
    }
    o + 1 == SIG_LEN
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Any seed, any message: the base64 decodes to a signature `ring`'s
    /// verifier accepts under the signer's own public key, and the
    /// percent-encoded form is exactly that base64 with `+ / =` escaped.
    #[test]
    fn every_signature_verifies_and_both_forms_agree(
        seed in proptest::array::uniform32(any::<u8>()),
        msg in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let s = Ed25519Signer::from_seed(&seed).expect("signer");
        let mut b64 = [0u8; SIG_B64_LEN];
        prop_assert_eq!(s.sign_b64(&msg, &mut b64), SIG_B64_LEN);
        let mut raw = [0u8; SIG_LEN];
        prop_assert!(b64_decode(&b64, &mut raw));
        prop_assert!(UnparsedPublicKey::new(&ED25519, s.public_key()).verify(&msg, &raw).is_ok());

        let mut pct = [0u8; SIG_B64_PCT_MAX];
        let n = s.sign_b64_pct(&msg, &mut pct);
        let mut back = Vec::with_capacity(SIG_B64_LEN);
        let mut i = 0;
        while i < n {
            if pct[i] == b'%' {
                back.push(match &pct[i + 1..i + 3] {
                    b"2B" => b'+',
                    b"2F" => b'/',
                    b"3D" => b'=',
                    other => return Err(TestCaseError::fail(format!("escape {other:?}"))),
                });
                i += 3;
            } else {
                prop_assert!(pct[i].is_ascii_alphanumeric());
                back.push(pct[i]);
                i += 1;
            }
        }
        prop_assert_eq!(&back[..], &b64[..]);
    }

    /// A different message never verifies under the same signature.
    #[test]
    fn a_changed_message_does_not_verify(
        seed in proptest::array::uniform32(any::<u8>()),
        msg in proptest::collection::vec(any::<u8>(), 1..256),
        flip in any::<usize>(),
    ) {
        let s = Ed25519Signer::from_seed(&seed).expect("signer");
        let mut b64 = [0u8; SIG_B64_LEN];
        s.sign_b64(&msg, &mut b64);
        let mut raw = [0u8; SIG_LEN];
        prop_assert!(b64_decode(&b64, &mut raw));
        let mut other = msg.clone();
        let at = flip % other.len();
        other[at] ^= 0x01;
        prop_assert!(UnparsedPublicKey::new(&ED25519, s.public_key()).verify(&other, &raw).is_err());
    }
}
