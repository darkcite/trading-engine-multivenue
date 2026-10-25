// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **T2T T1-c — signing in about a microsecond, from precomputed and
//! guarded ECDSA nonces.**
//!
//! An ECDSA signature over a digest `z` with key `d` is `r = (k·G).x` and
//! `s = k⁻¹·(z + r·d) mod n`. Everything but `z` is known before the
//! digest exists. The pool holds, per future signature, `k⁻¹`, `r`, `r·d`
//! and the recovery id. Signing then costs one mod-n add, one mod-n
//! multiply and the low-s rule. `k·G` and the inversion — almost all of the
//! ~19 µs an RFC 6979 signature costs on the Mac — move to the gateway's
//! idle turns.
//!
//! **The laws** (plan `t2t-plan-2026-10-25` T1-c; the risk policy's "T2T
//! T1-c" entry):
//!
//! 1. **A k is never reused.** One owner consumes the pool. Each entry is
//!    popped once and wiped as it is popped, whether or not it signed.
//! 2. **k is random and hedged.** `k = keccak256(tag ‖ d ‖ counter ‖ rand)`,
//!    with `rand` 32 bytes from the OS CSPRNG (`getrandom`) and the counter
//!    strictly increasing. A weak RNG cannot repeat a k under a fixed key,
//!    and a broken counter cannot either while the RNG works. (Keccak is a
//!    sponge: a secret prefix makes it a PRF, with no length extension.)
//! 3. **No field code of our own.** Every operation on a secret scalar is
//!    libsecp256k1's constant-time scalar arithmetic, the library that
//!    signs every order today (`seckey_tweak_add`, `seckey_tweak_mul`,
//!    `seckey_negate`, `ec_pubkey_create`). The inversion is Fermat's
//!    `k^(n−2)` by square-and-multiply over the public exponent.
//! 4. **The secrets sit in one `mlock`'d page,** wiped on drop: the key,
//!    every entry's `k⁻¹` and `r·d` (with `r` public, `r·d` is as secret as
//!    `d`).
//! 5. **Every boot self-tests:** one pooled signature must recover to the
//!    key's public key and verify, or the signer refuses to build.
//! 6. **Every pooled signature is checked after the fact.** It goes on a
//!    ring, and [`PrenonceSigner::check_one`] — run in an idle turn, never
//!    on the order path — verifies it and recovers its key. A mismatch
//!    disables the pool for the process (every later signature is RFC
//!    6979) and is reported to the caller, which halts.
//! 7. **An empty pool, or any edge the pool cannot sign** (a digest ≥ n,
//!    a zero `s`; each ≈ 2⁻¹²⁸), signs by RFC 6979 instead. That is
//!    today's path, unchanged.
//!
//! Signatures are low-s with `v = 27 + recid`, exactly the RFC 6979
//! path's shape ([`crate::sign_digest_with_key`]).

use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, Scalar, SecretKey};
use zeroize::Zeroize;

use crate::{all_ctx, keccak256_parts, sign_digest_with_key, signing_ctx, SignError};

/// Pooled signatures held ready.
pub const PRENONCE_POOL: usize = 64;
/// Pooled signatures awaiting their after-the-fact check. Past it the
/// oldest unchecked one is dropped and counted.
const CHECK_RING: usize = 16;

/// `n − 2` for secp256k1 (big-endian): the exponent of Fermat's inverse.
const N_MINUS_2: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE,
    0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x3F,
];
/// `⌊n / 2⌋` (big-endian): an `s` above it is high.
const HALF_N: [u8; 32] = [
    0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
];
/// The hedge's domain tag (law 2).
const HEDGE_TAG: &[u8] = b"multivenue/prenonce/v1";
/// The boot self-test's digest (law 5).
const SELF_TEST_TAG: &[u8] = b"multivenue/prenonce/self-test";

/// One future signature: everything but the digest. 128 B.
#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    /// `k⁻¹ mod n`.
    kinv: [u8; 32],
    /// `r·d mod n`.
    rd: [u8; 32],
    /// `r = (k·G).x`, public.
    r: [u8; 32],
    /// The recovery id before the low-s rule: `(k·G).y`'s parity.
    recid: u8,
    _pad: [u8; 31],
}

const EMPTY: Entry = Entry {
    kinv: [0; 32],
    rd: [0; 32],
    r: [0; 32],
    recid: 0,
    _pad: [0; 31],
};

impl Entry {
    fn wipe(&mut self) {
        self.kinv.zeroize();
        self.rd.zeroize();
        self.r.zeroize();
        self.recid.zeroize();
    }
}

/// The `mlock`'d secrets (law 4): the key, the pool, and the keep-hot
/// entry of a throwaway key. Page-aligned and page-sized (16 KiB: Apple
/// silicon's page, four of Linux's) so it shares no page with another
/// allocation — no `munlock` of a neighbour can unlock it, and its own
/// `munlock` unlocks nothing else (the risk policy's BX4 rule: `mlock` does
/// not stack on Linux).
#[repr(C, align(16384))]
struct Page {
    key: [u8; 32],
    dry_key: [u8; 32],
    entries: [Entry; PRENONCE_POOL],
    dry: Entry,
}

const _: () = assert!(core::mem::size_of::<Page>() == 16_384);
const _: () = assert!(core::mem::size_of::<Entry>() == 128);

/// A pooled signature awaiting its check (public data).
#[derive(Clone, Copy)]
struct Check {
    digest: [u8; 32],
    sig: [u8; 65],
}

/// What the signer did since boot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PrenonceCounters {
    /// Signatures from the pool.
    pub pooled: u64,
    /// Signatures by RFC 6979 (pool empty or disabled, or an edge).
    pub fallback: u64,
    /// Entries made.
    pub made: u64,
    /// Pooled signatures checked after the fact.
    pub checked: u64,
    /// Checks that failed. Any is fatal: the pool is off from then on.
    pub mismatched: u64,
    /// Pooled signatures that left the check ring unchecked.
    pub unchecked: u64,
}

/// **The precomputed-nonce signer** for one key. Single-owner: the
/// gateway thread signs, refills ([`Self::refill_one`]) and checks
/// ([`Self::check_one`]). See the module docs for its laws.
pub struct PrenonceSigner {
    page: Box<Page>,
    locked: bool,
    pubkey: PublicKey,
    /// The next entry to consume, and how many are ready from it.
    head: usize,
    len: usize,
    /// The hedge's counter (law 2).
    ctr: u64,
    checks: Box<[Check; CHECK_RING]>,
    check_head: usize,
    check_len: usize,
    enabled: bool,
    counters: PrenonceCounters,
}

impl PrenonceSigner {
    /// Build the signer for `sk`: lock the page, fill the pool, self-test
    /// (law 5). Boot-only: ~5 ms.
    ///
    /// # Errors
    /// [`SignError::Mlock`] when the page cannot be locked,
    /// [`SignError::Entropy`] when the OS CSPRNG fails,
    /// [`SignError::SelfTest`] when the self-test signature does not
    /// recover to `sk`'s key.
    pub fn new(sk: &SecretKey) -> Result<Self, SignError> {
        // COPY: boot, once — the 16 KiB page built zeroed and moved to the
        // heap; the key's 32 B into it. Rejected: an unsafe in-place
        // initialisation for a one-time move.
        let page = Box::new(Page {
            key: sk.secret_bytes(),
            dry_key: [0; 32],
            entries: [EMPTY; PRENONCE_POOL],
            dry: EMPTY,
        });
        let locked = lock(&page)?;
        let mut s = Self {
            pubkey: PublicKey::from_secret_key(signing_ctx(), sk),
            page,
            locked,
            head: 0,
            len: 0,
            ctr: 0,
            checks: Box::new(
                [Check {
                    digest: [0; 32],
                    sig: [0; 65],
                }; CHECK_RING],
            ),
            check_head: 0,
            check_len: 0,
            enabled: true,
            counters: PrenonceCounters::default(),
        };
        s.make_dry()?;
        while s.len < PRENONCE_POOL {
            s.make_one()?;
        }
        let digest = crate::keccak256(SELF_TEST_TAG);
        let sig = s.sign_pooled(&digest).ok_or(SignError::SelfTest)?;
        if !s.verifies(&digest, &sig) {
            return Err(SignError::SelfTest);
        }
        s.make_one()?;
        Ok(s)
    }

    /// Sign `digest` (`r ‖ s ‖ v`): from the pool when it holds an entry,
    /// by RFC 6979 otherwise (law 7).
    ///
    /// # Errors
    /// [`SignError::InvalidKey`] only if the page's key no longer parses,
    /// which cannot happen to a page this type built.
    #[inline]
    pub fn sign(&mut self, digest: &[u8; 32]) -> Result<[u8; 65], SignError> {
        if self.enabled && self.len > 0 {
            if let Some(sig) = self.sign_pooled(digest) {
                self.counters.pooled += 1;
                self.note_check(digest, &sig);
                return Ok(sig);
            }
        }
        self.counters.fallback += 1;
        let sk = SecretKey::from_slice(&self.page.key).map_err(|_| SignError::InvalidKey)?;
        sign_digest_with_key(&sk, digest)
    }

    /// Make one entry if the pool has room (an idle turn's work: ~60 µs).
    /// `false` when full, disabled, or the CSPRNG failed this time.
    pub fn refill_one(&mut self) -> bool {
        if !self.enabled || self.len == PRENONCE_POOL {
            return false;
        }
        self.make_one().is_ok()
    }

    /// Check the oldest pooled signature not yet checked (law 6; an idle
    /// turn's work: ~40 µs). `None` when none waits; `Some(false)` on a
    /// mismatch, after which the pool is off for good.
    pub fn check_one(&mut self) -> Option<bool> {
        if self.check_len == 0 {
            return None;
        }
        let c = self.checks[self.check_head];
        self.check_head = (self.check_head + 1) % CHECK_RING;
        self.check_len -= 1;
        self.counters.checked += 1;
        if self.verifies(&c.digest, &c.sig) {
            return Some(true);
        }
        self.counters.mismatched += 1;
        self.enabled = false;
        self.wipe_pool();
        Some(false)
    }

    /// The keep-hot dry run (T1-f): the pooled path's arithmetic on a
    /// throwaway key's fixed entry — nothing consumed, nothing kept. Its
    /// result is public and worthless.
    #[inline]
    pub fn dry_sign(&self, digest: &[u8; 32]) -> Option<[u8; 65]> {
        sign_with(&self.page.dry, digest)
    }

    /// Entries ready.
    #[must_use]
    pub const fn ready(&self) -> usize {
        self.len
    }

    /// Whether the pool still signs (no check has failed).
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the page is `mlock`'d (always on unix; the build refuses
    /// otherwise).
    #[must_use]
    pub const fn locked(&self) -> bool {
        self.locked
    }

    /// Counters since boot.
    #[must_use]
    pub const fn counters(&self) -> PrenonceCounters {
        self.counters
    }

    /// The key's public key.
    #[must_use]
    pub const fn public_key(&self) -> &PublicKey {
        &self.pubkey
    }

    /// Pop the next entry, sign with it and wipe it (law 1). `None` on an
    /// edge the pool does not sign.
    #[inline]
    fn sign_pooled(&mut self, digest: &[u8; 32]) -> Option<[u8; 65]> {
        debug_assert!(self.len > 0);
        let i = self.head;
        self.head = (i + 1) % PRENONCE_POOL;
        self.len -= 1;
        let e = &mut self.page.entries[i];
        let out = sign_with(e, digest);
        e.wipe();
        out
    }

    /// Queue a pooled signature for its check (law 6).
    #[inline]
    fn note_check(&mut self, digest: &[u8; 32], sig: &[u8; 65]) {
        if self.check_len == CHECK_RING {
            self.check_head = (self.check_head + 1) % CHECK_RING;
            self.check_len -= 1;
            self.counters.unchecked += 1;
        }
        let at = (self.check_head + self.check_len) % CHECK_RING;
        // COPY: 32 B + 65 B, the public digest and signature, onto the
        // check ring — the check runs later, off the order path.
        self.checks[at] = Check {
            digest: *digest,
            sig: *sig,
        };
        self.check_len += 1;
    }

    /// Whether `sig` over `digest` verifies (libsecp256k1 refuses a high
    /// `s`) and recovers, through its `v`, to this key.
    fn verifies(&self, digest: &[u8; 32], sig: &[u8; 65]) -> bool {
        let msg = Message::from_digest(*digest);
        let Some(recid) = sig[64]
            .checked_sub(27)
            .and_then(|v| RecoveryId::from_i32(i32::from(v)).ok())
        else {
            return false;
        };
        let Ok(rs) = RecoverableSignature::from_compact(&sig[..64], recid) else {
            return false;
        };
        let ctx = all_ctx();
        ctx.verify_ecdsa(&msg, &rs.to_standard(), &self.pubkey)
            .is_ok()
            && ctx
                .recover_ecdsa(&msg, &rs)
                .is_ok_and(|pk| pk == self.pubkey)
    }

    /// Make the next entry (laws 2, 3).
    fn make_one(&mut self) -> Result<(), SignError> {
        debug_assert!(self.len < PRENONCE_POOL);
        let at = (self.head + self.len) % PRENONCE_POOL;
        self.ctr += 1;
        let e = make_entry(&self.page.key, self.ctr)?;
        self.page.entries[at] = e;
        self.len += 1;
        self.counters.made += 1;
        Ok(())
    }

    /// The keep-hot entry, from a throwaway key (never the signing key).
    fn make_dry(&mut self) -> Result<(), SignError> {
        let mut k = [0u8; 32];
        loop {
            getrandom::getrandom(&mut k).map_err(|_| SignError::Entropy)?;
            if SecretKey::from_slice(&k).is_ok() {
                break;
            }
        }
        self.page.dry_key = k;
        k.zeroize();
        self.page.dry = make_entry(&self.page.dry_key, 1)?;
        Ok(())
    }

    fn wipe_pool(&mut self) {
        let mut i = 0usize;
        while i < PRENONCE_POOL {
            self.page.entries[i].wipe();
            i += 1;
        }
        self.len = 0;
    }
}

impl Drop for PrenonceSigner {
    fn drop(&mut self) {
        self.wipe_pool();
        self.page.key.zeroize();
        self.page.dry_key.zeroize();
        self.page.dry.wipe();
        if self.locked {
            unlock(&self.page);
        }
    }
}

/// One entry for key bytes `key` and hedge counter `ctr` (laws 2, 3): a
/// fresh k, R = k·G (retried on the ≈ 2⁻¹²⁸ cases where k or R.x is not a
/// scalar), k⁻¹ and r·d.
fn make_entry(key: &[u8; 32], ctr: u64) -> Result<Entry, SignError> {
    let d = SecretKey::from_slice(key).map_err(|_| SignError::InvalidKey)?;
    let mut tries = 0u32;
    loop {
        tries += 1;
        if tries > 8 {
            return Err(SignError::Entropy);
        }
        let mut rand = [0u8; 32];
        getrandom::getrandom(&mut rand).map_err(|_| SignError::Entropy)?;
        let mut kb = keccak256_parts(&[
            HEDGE_TAG,
            key,
            &ctr.to_be_bytes(),
            &tries.to_be_bytes(),
            &rand,
        ]);
        rand.zeroize();
        let k = SecretKey::from_slice(&kb);
        kb.zeroize();
        let Ok(k) = k else { continue };
        let big_r = PublicKey::from_secret_key(signing_ctx(), &k).serialize_uncompressed();
        let mut x = [0u8; 32];
        // COPY: R's 32 B x-coordinate out of its 65 B serialisation — the
        // library serialises by value; boot / idle work, off the order path.
        x.copy_from_slice(&big_r[1..33]);
        let Ok(r) = Scalar::from_be_bytes(x) else {
            continue;
        };
        let Some(kinv) = invert(k) else { continue };
        let Ok(rd) = d.mul_tweak(&r) else { continue };
        let mut e = EMPTY;
        e.kinv = kinv.secret_bytes();
        e.rd = rd.secret_bytes();
        e.r = x;
        e.recid = big_r[64] & 1;
        return Ok(e);
    }
}

/// `s = k⁻¹·(z + r·d)`, the low-s rule, `r ‖ s ‖ v`. `None` on a digest
/// that is not a scalar or a zero `s` (law 7's edges).
#[inline]
fn sign_with(e: &Entry, digest: &[u8; 32]) -> Option<[u8; 65]> {
    let z = Scalar::from_be_bytes(*digest).ok()?;
    let kinv = Scalar::from_be_bytes(e.kinv).ok()?;
    let rd = SecretKey::from_slice(&e.rd).ok()?;
    let s = rd.add_tweak(&z).ok()?.mul_tweak(&kinv).ok()?;
    let mut sb = s.secret_bytes();
    let mut recid = e.recid;
    if gt_be(&sb, &HALF_N) {
        sb = s.negate().secret_bytes();
        recid ^= 1;
    }
    let mut out = [0u8; 65];
    // COPY: r and s, 64 B, into the 65 B wire layout that adds v — per
    // signature, as the RFC 6979 path does.
    out[..32].copy_from_slice(&e.r);
    out[32..64].copy_from_slice(&sb);
    out[64] = 27 + recid;
    Some(out)
}

/// `k^(n−2) mod n` by square-and-multiply, the exponent public (law 3).
fn invert(k: SecretKey) -> Option<SecretKey> {
    let base = Scalar::from(k);
    let mut acc: Option<SecretKey> = None;
    let mut i = 0usize;
    while i < 32 {
        let mut bit = 8u32;
        while bit > 0 {
            bit -= 1;
            if let Some(a) = acc {
                acc = Some(a.mul_tweak(&Scalar::from(a)).ok()?);
            }
            if (N_MINUS_2[i] >> bit) & 1 == 1 {
                acc = Some(match acc {
                    None => k,
                    Some(a) => a.mul_tweak(&base).ok()?,
                });
            }
        }
        i += 1;
    }
    acc
}

/// `a > b`, both big-endian (public values only: `s` is in the signature).
#[inline]
fn gt_be(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut i = 0usize;
    while i < 32 {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
        i += 1;
    }
    false
}

/// `mlock` the page (law 4). Unix only; elsewhere the build refuses.
#[cfg(unix)]
fn lock(page: &Page) -> Result<bool, SignError> {
    // SAFETY: `page` is a live, aligned allocation of `size_of::<Page>()`
    // bytes; `mlock` reads no memory, it pins the pages that hold it.
    let rc = unsafe {
        libc::mlock(
            (page as *const Page).cast::<libc::c_void>(),
            core::mem::size_of::<Page>(),
        )
    };
    if rc != 0 {
        return Err(SignError::Mlock(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ));
    }
    Ok(true)
}

#[cfg(not(unix))]
fn lock(_page: &Page) -> Result<bool, SignError> {
    Err(SignError::Mlock(0))
}

#[cfg(unix)]
fn unlock(page: &Page) {
    // SAFETY: as `lock`: the same live allocation and length.
    let _ = unsafe {
        libc::munlock(
            (page as *const Page).cast::<libc::c_void>(),
            core::mem::size_of::<Page>(),
        )
    };
}

#[cfg(not(unix))]
fn unlock(_page: &Page) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SecretKey {
        let mut b = crate::keccak256(&[seed, 0x5A]);
        b[0] &= 0x7F;
        SecretKey::from_slice(&b).expect("a valid key")
    }

    #[test]
    fn the_constants_are_n_minus_two_and_half_n() {
        // n = FFFFFFFF FFFFFFFF FFFFFFFF FFFFFFFE BAAEDCE6 AF48A03B BFD25E8C D0364141
        let mut n = N_MINUS_2;
        n[31] += 2;
        assert_eq!(n[28..], [0xD0, 0x36, 0x41, 0x41]);
        // 2·⌊n/2⌋ + 1 = n (n is odd).
        let mut twice = [0u8; 32];
        let mut carry = 1u16;
        let mut i = 32usize;
        while i > 0 {
            i -= 1;
            let v = u16::from(HALF_N[i]) * 2 + carry;
            twice[i] = v as u8;
            carry = v >> 8;
        }
        assert_eq!(twice, n);
        // n − 1 parses as a key (the largest), n does not.
        let mut nm1 = n;
        nm1[31] -= 1;
        assert!(SecretKey::from_slice(&nm1).is_ok());
        assert!(SecretKey::from_slice(&n).is_err());
    }

    #[test]
    fn the_inverse_times_k_is_one() {
        let mut i = 1u8;
        while i < 40 {
            let k = key(i);
            let kinv = invert(k).expect("invertible");
            let one = kinv.mul_tweak(&Scalar::from(k)).expect("non-zero");
            let mut want = [0u8; 32];
            want[31] = 1;
            assert_eq!(one.secret_bytes(), want, "k·k⁻¹ = 1 for seed {i}");
            i += 1;
        }
    }

    /// Law 5 and the shape: every pooled signature verifies under
    /// libsecp256k1, recovers to the key through its v, and is low-s.
    #[test]
    fn pooled_signatures_verify_recover_and_are_low_s() {
        let sk = key(7);
        let mut s = PrenonceSigner::new(&sk).expect("builds");
        assert_eq!(s.ready(), PRENONCE_POOL, "full after boot");
        let pk = PublicKey::from_secret_key(signing_ctx(), &sk);
        assert_eq!(s.public_key(), &pk);
        let mut n = 0u32;
        let mut flips = [0u32; 2];
        while n < 2_000 {
            let digest = crate::keccak256(&n.to_be_bytes());
            let sig = s.sign(&digest).expect("signs");
            assert!(s.verifies(&digest, &sig), "signature {n}");
            assert!(!gt_be(sig[32..64].try_into().unwrap(), &HALF_N), "low-s");
            assert!(sig[64] == 27 || sig[64] == 28);
            flips[usize::from(sig[64] - 27)] += 1;
            if s.ready() < PRENONCE_POOL / 2 {
                while s.refill_one() {}
            }
            n += 1;
        }
        let c = s.counters();
        assert_eq!(c.pooled, 2_000, "every one from the pool");
        assert_eq!(c.fallback, 0);
        assert!(flips[0] > 800 && flips[1] > 800, "both v values: {flips:?}");
        // The after-the-fact checks pass, and the ring kept the last 16.
        let mut checked = 0;
        while let Some(ok) = s.check_one() {
            assert!(ok);
            checked += 1;
        }
        assert_eq!(checked, CHECK_RING);
        assert_eq!(s.counters().unchecked, 2_000 - CHECK_RING as u64);
    }

    /// Law 1: an entry is wiped as it is popped, and no two signatures in
    /// a long run share an r (a repeated k would).
    #[test]
    fn a_k_is_never_reused() {
        let mut s = PrenonceSigner::new(&key(9)).expect("builds");
        let mut seen: std::collections::BTreeSet<[u8; 32]> = std::collections::BTreeSet::new();
        let mut n = 0u32;
        while n < 3_000 {
            let i = s.head;
            let digest = crate::keccak256(&n.to_le_bytes());
            let sig = s.sign(&digest).expect("signs");
            assert_eq!(s.page.entries[i].kinv, [0u8; 32], "kinv wiped");
            assert_eq!(s.page.entries[i].rd, [0u8; 32], "rd wiped");
            let r: [u8; 32] = sig[..32].try_into().unwrap();
            assert!(seen.insert(r), "an r repeated at {n}");
            while s.refill_one() {}
            n += 1;
        }
    }

    /// Law 7: an empty pool signs by RFC 6979 — the same r ‖ s as the
    /// plain path, since RFC 6979 is deterministic.
    #[test]
    fn an_empty_pool_falls_back_to_rfc_6979() {
        let sk = key(11);
        let mut s = PrenonceSigner::new(&sk).expect("builds");
        let mut i = 0usize;
        while i < PRENONCE_POOL {
            s.sign(&[1u8; 32]).expect("signs");
            i += 1;
        }
        assert_eq!(s.ready(), 0);
        let digest = [0x42u8; 32];
        let a = s.sign(&digest).expect("falls back");
        let b = sign_digest_with_key(&sk, &digest).expect("plain");
        assert_eq!(a, b, "the fallback is today's signature");
        assert_eq!(s.counters().fallback, 1);
        assert!(s.verifies(&digest, &a));
    }

    /// Law 6: a corrupted pooled signature fails its check, turns the pool
    /// off for good, and every later signature is RFC 6979.
    #[test]
    fn a_failed_check_turns_the_pool_off() {
        let sk = key(13);
        let mut s = PrenonceSigner::new(&sk).expect("builds");
        let digest = [0x24u8; 32];
        s.sign(&digest).expect("signs");
        let at = (s.check_head + s.check_len - 1) % CHECK_RING;
        s.checks[at].sig[40] ^= 1;
        assert_eq!(s.check_one(), Some(false));
        assert!(!s.enabled());
        assert_eq!(s.ready(), 0, "the pool wiped");
        assert!(!s.refill_one(), "never refilled");
        let a = s.sign(&digest).expect("signs");
        assert_eq!(a, sign_digest_with_key(&sk, &digest).expect("plain"));
        assert_eq!(s.counters().mismatched, 1);
    }

    #[test]
    fn a_digest_past_n_falls_back() {
        let sk = key(15);
        let mut s = PrenonceSigner::new(&sk).expect("builds");
        let digest = [0xFFu8; 32];
        let a = s.sign(&digest).expect("signs");
        assert_eq!(a, sign_digest_with_key(&sk, &digest).expect("plain"));
        assert_eq!(s.counters().fallback, 1);
    }

    #[test]
    fn the_dry_run_signs_with_a_throwaway_key_and_consumes_nothing() {
        let sk = key(17);
        let s = PrenonceSigner::new(&sk).expect("builds");
        let ready = s.ready();
        let digest = [0x33u8; 32];
        let a = s.dry_sign(&digest).expect("dry");
        assert_eq!(s.ready(), ready, "nothing consumed");
        assert!(
            !s.verifies(&digest, &a),
            "never the signing key's signature"
        );
        assert_eq!(s.dry_sign(&digest), Some(a), "a fixed entry");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]
        /// Law 5's property: for any key and digest, the pooled signature
        /// verifies under libsecp256k1, recovers to the key and is low-s —
        /// the validity rules of the RFC 6979 path.
        #[test]
        fn any_key_and_digest_sign_verifiably(
            kb in proptest::array::uniform32(proptest::num::u8::ANY),
            d in proptest::array::uniform32(proptest::num::u8::ANY),
        ) {
            if let Ok(sk) = SecretKey::from_slice(&kb) {
                let mut s = PrenonceSigner::new(&sk).expect("builds");
                let sig = s.sign(&d).expect("signs");
                proptest::prop_assert!(s.verifies(&d, &sig));
                proptest::prop_assert!(!gt_be(sig[32..64].try_into().unwrap(), &HALF_N));
                let plain = sign_digest_with_key(&sk, &d).expect("plain");
                proptest::prop_assert!(s.verifies(&d, &plain), "both paths verify alike");
            }
        }
    }

    #[test]
    fn the_page_is_locked() {
        let s = PrenonceSigner::new(&key(19)).expect("builds");
        assert!(s.locked());
    }
}
