// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX-3: the venue's own published vectors, in `tests/` (HK5).**
//!
//! Binance's API documentation publishes, for `POST /api/v3/order`, an HMAC
//! key pair with two signed requests: an ASCII symbol and a non-ASCII one
//! whose symbol is percent-encoded before signing. Both are pinned here byte
//! for byte along the arm's own signing path (`rest::sign_hmac_in_place`: the
//! signature appended in the request's window, the hash padded in place),
//! and the boot self-tests (`config::hmac_self_test`, `selftest::run`) are
//! run from outside the crate.
//!
//! Ed25519: the documentation's example publishes the API key, the payload
//! and the signature, but neither the private key it signed with nor its
//! public key — so the documentation gives no Ed25519 vector to check. The
//! signer's authority stays RFC 8032 §7.1 (pinned in `signer-ed25519`), and
//! the renders those signatures are made over stay `selftest`'s.

use exec_binance::config::{hmac_self_test, HmacSigner, HMAC_SIG_HEX};
use exec_binance::rest::sign_hmac_in_place;

/// The documentation's HMAC secret key (public, a published example).
const SECRET: [u8; 64] = *b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";

/// The documentation's two signed requests and their signatures.
const VECTORS: [(&[u8], &[u8; HMAC_SIG_HEX]); 2] = [
    (
        b"symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559",
        b"c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71",
    ),
    (
        b"symbol=%EF%BC%91%EF%BC%92%EF%BC%93%EF%BC%94%EF%BC%95%EF%BC%96&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559",
        b"e1353ec6b14d888f1164ae9af8228a3dbd508bc82eb867db8ab6046442f33ef3",
    ),
];

/// `&signature=` + 64 hex.
const SIG_SUFFIX: usize = 11 + HMAC_SIG_HEX;

#[test]
fn the_docs_hmac_vectors_hold_on_the_arms_signing_path() {
    let signer = HmacSigner::from_secret(SECRET).expect("the docs' secret");
    let mut v = 0usize;
    while v < VECTORS.len() {
        let (msg, want) = VECTORS[v];
        let mut window = [0u8; 512];
        window[..msg.len()].copy_from_slice(msg);
        let n = sign_hmac_in_place(&signer, &mut window, msg.len()).expect("room for the signature");
        assert_eq!(n, msg.len() + SIG_SUFFIX, "vector {v}");
        assert_eq!(&window[..msg.len()], msg, "vector {v}: the request is untouched");
        assert_eq!(&window[msg.len()..msg.len() + 11], b"&signature=", "vector {v}");
        assert_eq!(&window[msg.len() + 11..n], &want[..], "vector {v}: the docs' signature");
        v += 1;
    }
}

#[test]
fn the_boot_self_tests_pass_from_outside_the_crate() {
    assert!(hmac_self_test().is_ok(), "BX-3 for HMAC");
    assert!(exec_binance::selftest::run().is_ok(), "BX-3: the renders' vectors");
}
