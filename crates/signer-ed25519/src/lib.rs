// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `signer-ed25519` — the Binance arm's Ed25519 signatures (BX4)
//!
//! Ruling O-BX5: Ed25519 comes from `ring`. Binance authenticates a WS API
//! session with ONE Ed25519 signature — `session.logon`, for spot, USDⓈ-M
//! and COIN-M order entry — and a REST request with one signature over its
//! query (options, Portfolio Margin, Binance Stocks, the cold fapi / dapi /
//! sapi calls). This crate is that primitive and nothing more: the payloads
//! are rendered by `exec-binance` (BX6), in their final buffers.
//!
//! ## The key
//!
//! The 32-byte seed arrives from `core_config::SecretKeyBytes`
//! (`from_hex_env("BINANCE_ED25519_SEED")`: mlock'd, zeroized on drop).
//! `ring` expands it into an `Ed25519KeyPair` — the secret scalar, the
//! nonce prefix and the public key. [`Ed25519Signer::from_seed`] builds
//! that expansion ONCE, at boot, into a page of its own (page-aligned,
//! page-sized, `mlock`'d), and dropping the signer overwrites the page
//! with volatile zero writes before it is `munlock`'d and freed. `ring`'s
//! type has no destructor and zeroizes nothing; the page gives it both.
//!
//! The limit, stated so nobody over-reads it: `ring` expands the seed and
//! signs inside its own stack frames (its SHA-512 buffers the seed there),
//! which this crate cannot reach. The NAMED temporary the expansion is
//! returned into is wiped; a return slot the compiler does not elide, or a
//! register spill, is out of reach too. This is defence in depth, the same
//! limit `docs/risk-policy.md` states for the process environment.
//!
//! ## Zero-copy
//!
//! A signature goes straight into the caller's FINAL buffer — base64 for a
//! WS API `signature` string ([`Ed25519Signer::sign_b64`]), percent-encoded
//! base64 for a REST query's `signature=` value
//! ([`Ed25519Signer::sign_b64_pct`]). No base64 string exists anywhere
//! else. The one copy the API forces is `ring`'s `Signature`, returned by
//! value because `ring` 0.17 has no sign-into-a-slice call; it is marked
//! at its site.
//!
//! ## Known answers (LAW BX-3)
//!
//! [`self_test`] runs RFC 8032 §7.1 — TEST 1, 2, 3 and SHA(abc) — through
//! [`Ed25519Signer`] itself: the page, the placement, the signing, and both
//! renders, each compared with a literal that does not come from this
//! crate's encoders. Any byte that differs refuses, and so does an empty
//! table. **BX6's boot must call it before building the real signer and
//! refuse the boot on `Err`** — never warn; nothing calls it at boot yet.
//!
//! ## PMJ J2: Solana (ruling R-PMJ-S3)
//!
//! Jupiter Predict hands back an unsigned Solana transaction; the wallet's
//! signature goes RAW into its slot of the transaction's own buffer
//! ([`Ed25519Signer::sign_raw`]). A Solana wallet exports its key as 64
//! bytes, seed ‖ public key: [`Ed25519Signer::from_solana_keypair`] builds
//! the signer from the seed and refuses a key whose public half is not
//! the one the seed derives. [`verify`] checks a signature against a public
//! key (the boot self-test, and a signed transaction before it is sent).
//!
//! ## Doctrine
//!
//! Boot allocates one page per signer; signing allocates nothing (bench
//! gate 86 — the renders, and since PMJ J2 the raw signature and its
//! verification). No `dyn`, no
//! iterators, no closures on the signing path.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

mod page;
mod vectors;

pub use vectors::{self_test, self_test_with, KnownAnswer, SelfTestErr, RFC8032};

use core::fmt;
use core::mem::{align_of, needs_drop, size_of, MaybeUninit};

use ring::error::KeyRejected;
use ring::signature::{Ed25519KeyPair, KeyPair, Signature, UnparsedPublicKey, ED25519};

/// Length of a raw Ed25519 signature.
pub const SIG_LEN: usize = 64;

/// Length of a raw Ed25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;

/// Length of a signature in base64: 88 bytes (two of them `=`).
pub const SIG_B64_LEN: usize = core_crypto::base64_encoded_len(SIG_LEN);

/// Upper bound of a signature in percent-encoded base64 (every byte
/// escaped). The real length is 88 plus 2 per `+`, `/` or `=`.
pub const SIG_B64_PCT_MAX: usize = core_crypto::base64_pct_encoded_max_len(SIG_LEN);

/// DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410 §4):
/// SEQUENCE { SEQUENCE { OID 1.3.101.112 } BIT STRING (33 bytes) }.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// Length of the public key's SPKI in base64: 60 bytes, the body of the
/// PEM block Binance's API-management page shows for a registered key.
pub const SPKI_B64_LEN: usize = core_crypto::base64_encoded_len(SPKI_PREFIX.len() + PUBLIC_KEY_LEN);

// A 12-byte prefix is whole base64 groups, so base64(prefix ‖ key) is
// base64(prefix) ‖ base64(key): the SPKI renders without assembling it.
const _: () = assert!(SPKI_PREFIX.len() % 3 == 0);
// The page holds the keypair by a bitwise move and never runs a
// destructor on it; both are sound only while `ring`'s type has none.
const _: () = assert!(!needs_drop::<Ed25519KeyPair>());
const _: () = assert!(!needs_drop::<Result<Ed25519KeyPair, KeyRejected>>());
const _: () = assert!(size_of::<Ed25519KeyPair>() <= page::MIN_PAGE);
const _: () = assert!(align_of::<Ed25519KeyPair>() <= page::MIN_PAGE);
// The sizes the `// COPY:` lines state, pinned: a `ring` bump that moves
// them fails the build instead of making a comment false.
const _: () = assert!(size_of::<Ed25519KeyPair>() == 96);
const _: () = assert!(size_of::<Result<Ed25519KeyPair, KeyRejected>>() == 104);
const _: () = assert!(size_of::<Signature>() == 120);
// `unsafe impl Send for Ed25519Signer` rests on this.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<Ed25519KeyPair>();
};

/// Why a signer could not be built. Boot-time only; every variant refuses
/// the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SignerErr {
    /// `ring` refused the seed.
    KeyRejected,
    /// The key's page could not be allocated.
    PageAlloc,
    /// `mlock` failed; the errno (on macOS and Linux, `RLIMIT_MEMLOCK` is
    /// the usual cause).
    Mlock(i32),
    /// PMJ J2: a 64-byte Solana keypair whose public half is not the key
    /// its seed derives — a corrupt export, or two wallets' halves.
    KeypairMismatch,
}

impl fmt::Display for SignerErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignerErr::KeyRejected => f.write_str("ed25519: the seed was rejected"),
            SignerErr::PageAlloc => f.write_str("ed25519: the key page could not be allocated"),
            SignerErr::Mlock(e) => write!(f, "ed25519: mlock of the key page failed (errno {e})"),
            SignerErr::KeypairMismatch => {
                f.write_str("ed25519: the keypair's public half is not its seed's key")
            }
        }
    }
}

impl std::error::Error for SignerErr {}

/// An Ed25519 signing key, expanded once into its own locked page.
///
/// Built at boot ([`Ed25519Signer::from_seed`]); signs with `&self` and
/// never allocates. Moves to the thread that signs (it is `Send`); never
/// shared.
pub struct Ed25519Signer {
    /// Holds the `ring` keypair at offset 0, placed by `from_seed`.
    page: page::LockedPage,
}

// SAFETY: the signer exclusively owns its page (no alias to it exists
// outside `self`), and the `Ed25519KeyPair` in it is `Send` + `Sync`.
// Moving the signer moves the only handle to the page.
unsafe impl Send for Ed25519Signer {}

impl Ed25519Signer {
    /// Expand `seed` into a new locked page. Boot only, and ONCE per key:
    /// a WS API re-logon reuses the signer, it never rebuilds it.
    ///
    /// This crate copies the caller's seed nowhere; `ring` hashes it once,
    /// inside its own frames.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, SignerErr> {
        let mut page = page::LockedPage::new()?;

        // `ring` returns the expanded key BY VALUE: give that value a home
        // this function can wipe, rather than an anonymous temporary.
        let mut tmp: MaybeUninit<Result<Ed25519KeyPair, KeyRejected>> = MaybeUninit::uninit();
        // COPY: ring's `Result<Ed25519KeyPair, KeyRejected>`, 104 B, by value,
        // once per boot — ring's only constructor returns by value — an
        // anonymous temporary rejected: it could not be wiped.
        tmp.write(Ed25519KeyPair::from_seed_unchecked(seed));
        // SAFETY: written on the line above.
        let placed = match unsafe { tmp.assume_init_ref() } {
            Ok(kp) => {
                // SAFETY: `kp` is a valid keypair; the page is aligned and
                // sized for it (const-asserted against MIN_PAGE); the two
                // regions are disjoint; the type has no drop glue
                // (const-asserted), so this bitwise move leaves exactly one
                // live value — the page's — and `tmp` is only wiped.
                unsafe {
                    // COPY: the expanded keypair, 96 B (ring 0.17.14), once
                    // per boot — `ring` has no constructor that writes in
                    // place — leaving it in `tmp` rejected: that is unlocked
                    // stack, wiped below.
                    core::ptr::copy_nonoverlapping(
                        kp as *const Ed25519KeyPair,
                        page.as_mut_ptr().cast::<Ed25519KeyPair>(),
                        1,
                    );
                }
                true
            }
            Err(_) => false,
        };
        // SAFETY: `tmp` is a live local of exactly this size, and its type
        // has no drop glue (const-asserted), so zero bytes are never read
        // back as a value.
        unsafe {
            page::wipe(
                tmp.as_mut_ptr().cast::<u8>(),
                size_of::<Result<Ed25519KeyPair, KeyRejected>>(),
            );
        }
        if !placed {
            return Err(SignerErr::KeyRejected);
        }
        Ok(Self { page })
    }

    /// PMJ J2: the signer of a Solana wallet's 64-byte secret key (seed ‖
    /// public key, the `solana-keygen` / wallet export). Boot only. The
    /// seed half is expanded as by [`Self::from_seed`]; the public half must
    /// be the key that seed derives, compared in constant time.
    ///
    /// # Errors
    /// [`SignerErr::KeypairMismatch`] for a public half that differs, or
    /// any [`Self::from_seed`] refusal. The caller wipes `keypair`.
    pub fn from_solana_keypair(keypair: &[u8; 64]) -> Result<Self, SignerErr> {
        let mut seed = [0u8; 32];
        // COPY: the 32-byte seed half into a local the expansion takes by
        // reference, once per boot — `from_seed` takes `&[u8; 32]` and a
        // sub-array reference of the caller's 64 bytes needs this copy or
        // an unsafe cast — the cast rejected (no unsafe for a boot path);
        // the local is wiped below.
        seed.copy_from_slice(&keypair[..32]);
        let built = Self::from_seed(&seed);
        // SAFETY: `seed` is a live local of exactly 32 bytes, never read
        // again.
        unsafe {
            page::wipe(seed.as_mut_ptr(), seed.len());
        }
        let signer = built?;
        if !core_crypto::ct_eq(signer.public_key(), &keypair[32..]) {
            return Err(SignerErr::KeypairMismatch);
        }
        Ok(signer)
    }

    #[inline(always)]
    fn keypair(&self) -> &Ed25519KeyPair {
        // SAFETY: `from_seed` placed a valid keypair at the page's start
        // and nothing writes the page again until `LockedPage::drop`.
        unsafe { &*self.page.as_ptr().cast::<Ed25519KeyPair>() }
    }

    /// The raw public key ([`PUBLIC_KEY_LEN`] bytes), borrowed from the
    /// page — not secret.
    #[inline]
    #[must_use]
    pub fn public_key(&self) -> &[u8] {
        self.keypair().public_key().as_ref()
    }

    /// Sign `msg` and write the signature's base64 into `dst[..88]` —
    /// the WS API form (`"signature":"…"`; the alphabet needs no JSON
    /// escaping). Returns [`SIG_B64_LEN`]. Zero-alloc.
    ///
    /// `dst` shorter than 88 bytes is a programmer error (debug assert,
    /// release bounds-check abort).
    #[inline]
    pub fn sign_b64(&self, msg: &[u8], dst: &mut [u8]) -> usize {
        // COPY: ring's `Signature`, 120 B (its largest-signature array and a
        // length) by value — ring 0.17 signs only into its return value —
        // ed25519-dalek's 64 B return rejected: ruling O-BX5 names ring.
        let sig = self.keypair().sign(msg);
        core_crypto::base64_encode(sig.as_ref(), dst)
    }

    /// Sign `msg` and write the signature as percent-encoded base64 into
    /// `dst` — the REST form (`&signature=…`). Returns the bytes written
    /// (88 plus 2 per escaped `+`, `/` or `=`). Zero-alloc.
    ///
    /// `dst` must hold [`SIG_B64_PCT_MAX`] bytes (programmer error
    /// otherwise, as for [`Ed25519Signer::sign_b64`]).
    #[inline]
    pub fn sign_b64_pct(&self, msg: &[u8], dst: &mut [u8]) -> usize {
        // COPY: ring's `Signature`, 120 B by value — as in `sign_b64`.
        let sig = self.keypair().sign(msg);
        core_crypto::base64_encode_pct(sig.as_ref(), dst)
    }

    /// PMJ J2: sign `msg` and write the raw 64-byte signature into `dst` —
    /// a Solana transaction's signature slot, in the transaction's own
    /// buffer. Zero-alloc.
    #[inline]
    pub fn sign_raw(&self, msg: &[u8], dst: &mut [u8; SIG_LEN]) {
        // COPY: ring's `Signature`, 120 B by value — as in `sign_b64`.
        let sig = self.keypair().sign(msg);
        // COPY: the 64 signature bytes into the caller's slot — ring 0.17
        // has no sign-into-a-slice call — rejected: a second buffer.
        dst.copy_from_slice(sig.as_ref());
    }

    /// Write the public key's `SubjectPublicKeyInfo` in base64 into
    /// `dst[..60]` — exactly the body of the PEM block Binance shows for
    /// the registered key, so the boot tell can be compared with it by
    /// eye. Returns [`SPKI_B64_LEN`]. Cold.
    pub fn public_key_spki_b64(&self, dst: &mut [u8]) -> usize {
        debug_assert!(dst.len() >= SPKI_B64_LEN);
        let n = core_crypto::base64_encode(&SPKI_PREFIX, dst);
        n + core_crypto::base64_encode(self.public_key(), &mut dst[n..])
    }
}

/// PMJ J2: whether `sig` is `public`'s Ed25519 signature of `msg` (RFC
/// 8032, `ring`'s strict verification). Zero-alloc.
#[inline]
#[must_use]
pub fn verify(public: &[u8; PUBLIC_KEY_LEN], msg: &[u8], sig: &[u8; SIG_LEN]) -> bool {
    UnparsedPublicKey::new(&ED25519, public).verify(msg, sig).is_ok()
}

impl fmt::Debug for Ed25519Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ed25519Signer { key: <redacted> }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signer_moves_to_its_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Ed25519Signer>();
    }

    #[test]
    fn debug_never_prints_the_key() {
        let s = Ed25519Signer::from_seed(&[7u8; 32]).expect("signer");
        let text = format!("{s:?}");
        assert_eq!(text, "Ed25519Signer { key: <redacted> }");
    }

    #[test]
    fn the_page_holds_the_expanded_key_and_nothing_past_it() {
        // The keypair was placed (the page is not all zeros) and only
        // within its own size (every byte past it is still the
        // allocator's zero).
        let s = Ed25519Signer::from_seed(&[9u8; 32]).expect("signer");
        let n = size_of::<Ed25519KeyPair>();
        // SAFETY: the page is live for `len` bytes while `s` lives.
        let bytes = unsafe { core::slice::from_raw_parts(s.page.as_ptr(), s.page.len()) };
        assert!(bytes[..n].iter().any(|&b| b != 0));
        assert!(bytes[n..].iter().all(|&b| b == 0));
    }

    #[test]
    fn errors_name_the_failure_and_never_a_value() {
        assert!(SignerErr::KeyRejected.to_string().contains("rejected"));
        assert!(SignerErr::PageAlloc.to_string().contains("page"));
        assert!(SignerErr::Mlock(12).to_string().contains("errno 12"));
        assert!(SignerErr::KeypairMismatch.to_string().contains("public half"));
    }

    /// PMJ J2: the Solana export of RFC 8032 TEST 1 (seed ‖ public key)
    /// builds TEST 1's signer; a keypair whose halves disagree refuses.
    #[test]
    fn a_solana_keypair_builds_its_signer_and_mismatched_halves_refuse() {
        let v = &crate::RFC8032[0];
        let mut kp = [0u8; 64];
        kp[..32].copy_from_slice(&v.seed);
        kp[32..].copy_from_slice(&v.public);
        let s = Ed25519Signer::from_solana_keypair(&kp).expect("TEST 1's keypair");
        assert_eq!(s.public_key(), v.public.as_slice());
        let mut sig = [0u8; SIG_LEN];
        s.sign_raw(v.msg, &mut sig);
        assert_eq!(sig, v.sig);
        let mut bad = kp;
        bad[63] ^= 1;
        assert_eq!(
            Ed25519Signer::from_solana_keypair(&bad).err(),
            Some(SignerErr::KeypairMismatch)
        );
        let mut other = kp;
        other[32..].copy_from_slice(&crate::RFC8032[1].public);
        assert_eq!(
            Ed25519Signer::from_solana_keypair(&other).err(),
            Some(SignerErr::KeypairMismatch)
        );
    }

    /// PMJ J2: verification accepts exactly the signature, the key and the
    /// message, and nothing one bit away from any of them.
    #[test]
    fn verify_accepts_only_the_signature_of_that_key_and_message() {
        let mut i = 0usize;
        while i < crate::RFC8032.len() {
            let v = &crate::RFC8032[i];
            assert!(verify(&v.public, v.msg, &v.sig), "vector {i}");
            let mut b = 0usize;
            while b < SIG_LEN {
                let mut s = v.sig;
                s[b] ^= 0x01;
                assert!(!verify(&v.public, v.msg, &s), "vector {i} sig bit {b}");
                b += 1;
            }
            let mut p = v.public;
            p[0] ^= 0x01;
            assert!(!verify(&p, v.msg, &v.sig), "vector {i} key");
            let mut m = [0u8; 65];
            m[..v.msg.len()].copy_from_slice(v.msg);
            m[v.msg.len()] = 0x01;
            assert!(!verify(&v.public, &m[..v.msg.len() + 1], &v.sig), "vector {i} msg");
            i += 1;
        }
    }
}
