// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hyperliquid's L1-action signature — the `Agent` EIP-712 envelope.
//!
//! The crate name is venue-neutral and layers 1–2 are already exactly
//! what Hyperliquid needs: [`crate::keccak256`] for both the action
//! hash and the EIP-712 digest, and [`crate::sign_digest_with_key`]
//! for the 65-byte `r‖s‖v` over a cached `Secp256k1<SignOnly>`. Only
//! the domain and the one struct are new, so this is a module beside
//! the Polymarket constants rather than a fork of them. **Nothing in
//! the Polymarket path is touched.**
//!
//! ## What Hyperliquid actually signs
//!
//! ```text
//! connection_id = keccak256(
//!       msgpack(action)                    // canonical key order — LAW E-3
//!    ‖  nonce                  as BE u64
//!    ‖  0x00                                // no vault
//!    [‖ 0x01 ‖ vault_address[20]]           // with vault
//!    [‖ 0x00 ‖ expires_after   as BE u64]   // only when set
//! )
//!
//! digest = keccak256(0x19 0x01 ‖ domainSeparator ‖ hashStruct(Agent))
//! ```
//!
//! where `Agent` is `{source: string, connectionId: bytes32}` and
//! `source` is `"a"` on mainnet, `"b"` on testnet.
//!
//! **Getting `source` wrong is a 100 % rejection rate, not a subtle
//! bug** — the signature verifies against a different digest, so the
//! venue sees a valid signature from the wrong address and refuses
//! every order. It is a parameter and never a default.
//!
//! The `expires_after` tail is appended AFTER the vault byte, and only
//! when set. That ordering is not guessable; it is mirrored from the
//! SDK and pinned by the vectors in `crates/exec-hyperliquid`.

use crate::{keccak256, keccak256_parts, SignError};

/// EIP-712 domain name. Not Hyperliquid's brand — the literal string
/// the venue's own signer uses.
pub const HL_DOMAIN_NAME: &str = "Exchange";
/// EIP-712 domain version.
pub const HL_DOMAIN_VERSION: &str = "1";
/// EIP-712 domain chain id. **1337 regardless of mainnet or testnet** —
/// the network is selected by `source`, not by the chain id.
pub const HL_CHAIN_ID: u64 = 1337;
/// EIP-712 verifying contract: the zero address.
pub const HL_VERIFYING_CONTRACT: [u8; 20] = [0u8; 20];
/// The `Agent` struct's EIP-712 type string.
pub const HL_AGENT_TYPE: &str = "Agent(string source,bytes32 connectionId)";
/// `source` on mainnet.
pub const HL_SOURCE_MAINNET: &[u8] = b"a";
/// `source` on testnet.
pub const HL_SOURCE_TESTNET: &[u8] = b"b";

/// The EIP-712 domain type string (identical text to Polymarket's, but
/// spelled out here so this module reads without cross-referencing).
const HL_DOMAIN_TYPE: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// The domain separator. All inputs are compile-time constants, so the
/// keccak is too — cached once, exactly as the Polymarket separator is.
///
/// Absorbed as PARTS (`keccak256_parts`): the five 32-byte words go
/// into the sponge straight from where they already are, so there is
/// no 160-byte preimage buffer to assemble first. Once per process
/// either way; the shape is the point, because `agent_struct_hash`
/// and `agent_eip712_hash` below are per-signature and use it too.
#[inline]
pub fn hl_domain_separator() -> [u8; 32] {
    static DS: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *DS.get_or_init(|| {
        // uint256 chainId, big-endian right-aligned in 32 bytes;
        // address, right-aligned in 32 bytes (the low 20 of the 32).
        // COPY: 8 B + 20 B into two zero-padded 32 B EIP-712 words —
        // the encoding IS the padding; once per process (cached).
        let mut chain = [0u8; 32];
        chain[24..32].copy_from_slice(&HL_CHAIN_ID.to_be_bytes());
        let mut contract = [0u8; 32];
        contract[12..32].copy_from_slice(&HL_VERIFYING_CONTRACT);
        keccak256_parts(&[
            &keccak256(HL_DOMAIN_TYPE.as_bytes()),
            &keccak256(HL_DOMAIN_NAME.as_bytes()),
            &keccak256(HL_DOMAIN_VERSION.as_bytes()),
            &chain,
            &contract,
        ])
    })
}

/// `keccak(HL_AGENT_TYPE)` — a constant, hashed once. The first cut
/// recomputed it on EVERY signature (one full keccak permutation per
/// order for a value that never changes).
#[inline]
fn hl_agent_typehash() -> &'static [u8; 32] {
    static T: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    T.get_or_init(|| keccak256(HL_AGENT_TYPE.as_bytes()))
}

/// `keccak(source)` for the two sources that exist, hashed once each
/// — the same per-signature saving as [`hl_agent_typehash`]. Any other
/// `source` (tests) is hashed on the spot, so the function stays total
/// and the vectors stay the authority on what a digest is.
#[inline]
fn hl_source_hash(source: &[u8]) -> [u8; 32] {
    static A: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    static B: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    if source == HL_SOURCE_MAINNET {
        *A.get_or_init(|| keccak256(HL_SOURCE_MAINNET))
    } else if source == HL_SOURCE_TESTNET {
        *B.get_or_init(|| keccak256(HL_SOURCE_TESTNET))
    } else {
        keccak256(source)
    }
}

/// `hashStruct(Agent)` = keccak(typehash ‖ keccak(source) ‖ connectionId).
///
/// Per signature. Zero-copy: the three words are absorbed from where
/// they live (a cached static, a cached static, the caller's array) —
/// no 96-byte preimage is assembled.
#[inline]
pub fn agent_struct_hash(source: &[u8], connection_id: &[u8; 32]) -> [u8; 32] {
    // `string` is encoded as the keccak of its bytes; `bytes32`
    // verbatim.
    keccak256_parts(&[hl_agent_typehash(), &hl_source_hash(source), connection_id])
}

/// The digest Hyperliquid expects a signature over:
/// `keccak(0x19 0x01 ‖ domainSeparator ‖ hashStruct(Agent))`.
///
/// `source` is [`HL_SOURCE_MAINNET`] or [`HL_SOURCE_TESTNET`] — a
/// parameter, never a default, because the wrong one rejects every
/// order.
///
/// Per signature, zero-copy as [`agent_struct_hash`]: the 2-byte
/// prefix is a literal and the two hashes are absorbed in place.
#[inline]
pub fn agent_eip712_hash(source: &[u8], connection_id: &[u8; 32]) -> [u8; 32] {
    let ds = hl_domain_separator();
    let sh = agent_struct_hash(source, connection_id);
    keccak256_parts(&[&[0x19, 0x01], &ds, &sh])
}

/// One-shot: digest + sign, returning the 65-byte `r‖s‖v`.
///
/// Hot path — the caller holds a parsed [`secp256k1::SecretKey`] from
/// boot so no key parsing happens per order.
#[inline]
pub fn sign_agent_with_key(
    sk: &secp256k1::SecretKey,
    source: &[u8],
    connection_id: &[u8; 32],
) -> Result<[u8; 65], SignError> {
    let digest = agent_eip712_hash(source, connection_id);
    crate::sign_digest_with_key(sk, &digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_domain_is_the_venues_not_polymarkets() {
        // A silent collision with the Polymarket separator would sign
        // Hyperliquid actions under the wrong domain and every order
        // would be rejected — with a VALID signature, from the wrong
        // address, which is the hardest kind of failure to read.
        assert_ne!(hl_domain_separator(), crate::domain_separator());
        assert_eq!(HL_CHAIN_ID, 1337);
        assert_eq!(HL_VERIFYING_CONTRACT, [0u8; 20]);
        assert_eq!(HL_DOMAIN_NAME, "Exchange");
    }

    #[test]
    fn the_separator_is_cached_and_stable() {
        let a = hl_domain_separator();
        let b = hl_domain_separator();
        assert_eq!(a, b);
    }

    #[test]
    fn mainnet_and_testnet_digests_differ() {
        // The `source` byte is the ONLY thing separating the two
        // networks — same action, same nonce, same key.
        let cid = [7u8; 32];
        let a = agent_eip712_hash(HL_SOURCE_MAINNET, &cid);
        let b = agent_eip712_hash(HL_SOURCE_TESTNET, &cid);
        assert_ne!(a, b, "source must reach the digest");
    }

    #[test]
    fn the_connection_id_reaches_the_digest() {
        let x = agent_eip712_hash(HL_SOURCE_MAINNET, &[1u8; 32]);
        let y = agent_eip712_hash(HL_SOURCE_MAINNET, &[2u8; 32]);
        assert_ne!(x, y);
    }
}
