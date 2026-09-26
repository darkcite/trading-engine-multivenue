// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! RFC 8032 §7.1 known answers and the boot self-test (LAW BX-3).
//!
//! The vectors are the RFC's own bytes: TEST 1–3 as they also open the
//! reference implementation's `sign.input`, and TEST SHA(abc), whose
//! message is SHA-512("abc"). Decoded from hex at COMPILE time: a non-hex
//! character or a wrong length fails the build; a wrong but valid digit
//! fails the self-test.
//!
//! Each vector also carries its signature as the two strings the wire
//! carries — base64 (WS API) and percent-encoded base64 (REST) — as
//! LITERALS computed outside this crate (Python's `base64` and
//! `urllib.parse.quote` over the RFC bytes). The self-test compares the
//! signer's renders with those literals, so an encoder that is
//! consistently wrong cannot pass it; a unit test ties each literal back to
//! the RFC bytes.

use core::fmt;

use crate::{Ed25519Signer, SignerErr, PUBLIC_KEY_LEN, SIG_B64_LEN, SIG_B64_PCT_MAX, SIG_LEN};

/// One known answer: a seed, the public key it must derive, a message and
/// the signature it must produce — raw, and as its two wire renders.
/// `Copy` so a test can corrupt one field of a copy.
#[derive(Copy, Clone)]
pub struct KnownAnswer {
    /// The 32-byte secret seed.
    pub seed: [u8; 32],
    /// The public key the seed derives.
    pub public: [u8; PUBLIC_KEY_LEN],
    /// The message.
    pub msg: &'static [u8],
    /// The signature of `msg` under `seed`, as the RFC prints it.
    pub sig: [u8; SIG_LEN],
    /// `sig` in base64 — a literal, not this crate's encoder.
    pub sig_b64: &'static str,
    /// `sig` in percent-encoded base64 — a literal, not this crate's encoder.
    pub sig_b64_pct: &'static str,
}

/// RFC 8032 §7.1: TEST 1 (empty message), TEST 2 (1 byte), TEST 3
/// (2 bytes) and TEST SHA(abc) (64 bytes).
pub const RFC8032: [KnownAnswer; 4] = [
    KnownAnswer {
        seed: hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"),
        public: hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"),
        msg: &[],
        sig: hex(concat!(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155",
            "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        )),
        sig_b64: "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw==",
        sig_b64_pct: "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc%2BbRr0lv18FlbviRlUUFDjnoQCw%3D%3D",
    },
    KnownAnswer {
        seed: hex("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb"),
        public: hex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"),
        msg: &[0x72],
        sig: hex(concat!(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da",
            "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
        )),
        sig_b64: "kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA==",
        sig_b64_pct: "kqAJqfDUyrhyDoILX2QlQKKye1QWUD%2BPs3YiI%2BvbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA%3D%3D",
    },
    KnownAnswer {
        seed: hex("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7"),
        public: hex("fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025"),
        msg: &[0xaf, 0x82],
        sig: hex(concat!(
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac",
            "18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"
        )),
        sig_b64: "YpHWV97sJAJIJ+acOr4BowzlSKKEdDpEXjaA19taw6wY/5tTjRbykK5n92CYTcZZSnwV6XFu0o3AJ77O6h7ECg==",
        sig_b64_pct: "YpHWV97sJAJIJ%2BacOr4BowzlSKKEdDpEXjaA19taw6wY%2F5tTjRbykK5n92CYTcZZSnwV6XFu0o3AJ77O6h7ECg%3D%3D",
    },
    KnownAnswer {
        seed: hex("833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42"),
        public: hex("ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf"),
        msg: &SHA512_ABC,
        sig: hex(concat!(
            "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b589",
            "09351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704"
        )),
        sig_b64: "3CpEWec2ljOlKxvyd4OaACAQCaPvvz7Lab6iGGwmtYkJNR/JrJCz7P37x8ZkMeAwPcoXnBOKwXrZvvEXczGnBA==",
        sig_b64_pct: "3CpEWec2ljOlKxvyd4OaACAQCaPvvz7Lab6iGGwmtYkJNR%2FJrJCz7P37x8ZkMeAwPcoXnBOKwXrZvvEXczGnBA%3D%3D",
    },
];

/// TEST SHA(abc)'s message: SHA-512("abc").
const SHA512_ABC: [u8; 64] = hex(concat!(
    "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a",
    "2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
));

/// Which known answer failed, and how. Each variant refuses the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SelfTestErr {
    /// The table was empty: a test that tests nothing is a refusal.
    Empty,
    /// Vector `i`'s seed could not be made into a signer.
    Signer(usize, SignerErr),
    /// Vector `i`'s seed derived a different public key.
    PublicKey(usize),
    /// Vector `i` signed to a different base64 signature.
    Signature(usize),
    /// Vector `i` signed to a different percent-encoded signature.
    SignaturePct(usize),
}

impl fmt::Display for SelfTestErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelfTestErr::Empty => f.write_str("ed25519 self-test: no known answers to check"),
            SelfTestErr::Signer(i, e) => write!(f, "ed25519 self-test: vector {i}: {e}"),
            SelfTestErr::PublicKey(i) => {
                write!(f, "ed25519 self-test: vector {i} derived a different public key")
            }
            SelfTestErr::Signature(i) => {
                write!(f, "ed25519 self-test: vector {i} produced a different signature")
            }
            SelfTestErr::SignaturePct(i) => write!(
                f,
                "ed25519 self-test: vector {i} produced a different percent-encoded signature"
            ),
        }
    }
}

impl std::error::Error for SelfTestErr {}

/// The boot self-test: every RFC 8032 known answer through
/// [`Ed25519Signer`] — page, placement, signature, and both renders
/// against literals. BX6's boot runs it before the real signer is built
/// and refuses the boot on `Err`.
pub fn self_test() -> Result<(), SelfTestErr> {
    self_test_with(&RFC8032)
}

/// [`self_test`] over any table; the tests use it to prove that one
/// corrupted byte anywhere refuses, and that an empty table refuses.
pub fn self_test_with(vectors: &[KnownAnswer]) -> Result<(), SelfTestErr> {
    if vectors.is_empty() {
        return Err(SelfTestErr::Empty);
    }
    let mut b64 = [0u8; SIG_B64_LEN];
    let mut pct = [0u8; SIG_B64_PCT_MAX];
    let mut i = 0usize;
    while i < vectors.len() {
        let v = &vectors[i];
        let signer = match Ed25519Signer::from_seed(&v.seed) {
            Ok(s) => s,
            Err(e) => return Err(SelfTestErr::Signer(i, e)),
        };
        if signer.public_key() != v.public.as_slice() {
            return Err(SelfTestErr::PublicKey(i));
        }
        let n = signer.sign_b64(v.msg, &mut b64);
        if &b64[..n] != v.sig_b64.as_bytes() {
            return Err(SelfTestErr::Signature(i));
        }
        let n = signer.sign_b64_pct(v.msg, &mut pct);
        if &pct[..n] != v.sig_b64_pct.as_bytes() {
            return Err(SelfTestErr::SignaturePct(i));
        }
        i += 1;
    }
    Ok(())
}

/// Lower-case or upper-case hex → `[u8; N]`, at compile time. Any
/// length or digit error is a `const` evaluation failure (a build error).
const fn hex<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    assert!(b.len() == 2 * N, "hex: wrong length");
    let mut out = [0u8; N];
    let mut i = 0usize;
    while i < N {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("hex: not a digit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc8032_known_answers_pass() {
        assert_eq!(self_test(), Ok(()));
    }

    #[test]
    fn an_empty_table_refuses() {
        assert_eq!(self_test_with(&[]), Err(SelfTestErr::Empty));
    }

    #[test]
    fn the_literals_are_the_rfc_signatures() {
        // Ties each independent literal back to the RFC bytes — and, in
        // doing so, tests core-crypto's two encoders against them.
        let mut i = 0;
        while i < RFC8032.len() {
            let v = &RFC8032[i];
            let mut b64 = [0u8; SIG_B64_LEN];
            let n = core_crypto::base64_encode(&v.sig, &mut b64);
            assert_eq!(&b64[..n], v.sig_b64.as_bytes(), "vector {i}");
            let mut pct = [0u8; SIG_B64_PCT_MAX];
            let n = core_crypto::base64_encode_pct(&v.sig, &mut pct);
            assert_eq!(&pct[..n], v.sig_b64_pct.as_bytes(), "vector {i}");
            i += 1;
        }
    }

    /// A copy of `s` with one byte changed, leaked for a `'static` field.
    fn corrupt(s: &'static str, at: usize) -> &'static str {
        let mut b = s.as_bytes().to_vec();
        b[at] = if b[at] == b'A' { b'B' } else { b'A' };
        Box::leak(String::from_utf8(b).expect("ascii").into_boxed_str())
    }

    #[test]
    fn one_corrupted_byte_anywhere_refuses() {
        // Every byte of every field of every vector: the seed and the
        // public key trip `PublicKey`, a render literal trips its own
        // variant, the message trips `Signature`.
        let mut v = 0usize;
        while v < RFC8032.len() {
            let mut b = 0usize;
            while b < 32 {
                let mut t = RFC8032;
                t[v].seed[b] ^= 0x01;
                assert_eq!(self_test_with(&t), Err(SelfTestErr::PublicKey(v)), "seed {v}:{b}");
                let mut t = RFC8032;
                t[v].public[b] ^= 0x80;
                assert_eq!(self_test_with(&t), Err(SelfTestErr::PublicKey(v)), "public {v}:{b}");
                b += 1;
            }
            let mut b = 0usize;
            while b < RFC8032[v].sig_b64.len() {
                let mut t = RFC8032;
                t[v].sig_b64 = corrupt(RFC8032[v].sig_b64, b);
                assert_eq!(self_test_with(&t), Err(SelfTestErr::Signature(v)), "b64 {v}:{b}");
                b += 1;
            }
            let mut b = 0usize;
            while b < RFC8032[v].sig_b64_pct.len() {
                let mut t = RFC8032;
                t[v].sig_b64_pct = corrupt(RFC8032[v].sig_b64_pct, b);
                assert_eq!(self_test_with(&t), Err(SelfTestErr::SignaturePct(v)), "pct {v}:{b}");
                b += 1;
            }
            v += 1;
        }
        // A different message under a correct key.
        let mut t = RFC8032;
        t[1].msg = &[0x73];
        assert_eq!(self_test_with(&t), Err(SelfTestErr::Signature(1)));
    }

    #[test]
    fn const_hex_reads_both_cases() {
        assert_eq!(hex::<2>("a0B1"), [0xa0, 0xb1]);
    }
}
