// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The action hash, and the signature over it.
//!
//! ```text
//! connection_id = keccak256(
//!       msgpack(action)
//!    ‖  nonce                as BE u64
//!    ‖  0x00                              // no vault
//!    [‖ 0x01 ‖ vault[20]]                 // with vault
//!    [‖ 0x00 ‖ expires_after as BE u64]   // only when set
//! )
//! ```
//!
//! Two things about that tail are worth stating because neither is
//! guessable and both are silent when wrong:
//!
//! 1. The vault byte is **always present** — `0x00` when there is no
//!    vault. It is not omitted.
//! 2. `expires_after` appends `0x00` and then the value, AFTER the
//!    vault byte. The leading `0x00` there is NOT the no-vault marker;
//!    it is part of the expiry tail, and both appear when a vaulted
//!    action also expires.
//!
//! Getting either wrong produces a perfectly valid signature over a
//! digest the venue did not compute, which rejects every order with no
//! useful error.

use signer_eip712::hyperliquid as hl;
use signer_eip712::{keccak256_parts, SignError};

/// Whether an action carries a vault, and which.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Vault {
    /// No vault — the hash carries the bare `0x00` marker.
    None,
    /// Trade on behalf of this vault address.
    Address([u8; 20]),
}

/// Build the connection id for an already-encoded action.
///
/// `action` is the msgpack bytes from an `encode_*` in
/// [`crate::action`]. Nothing here inspects them — the encoder owns
/// LAW E-3, this owns the envelope.
///
/// ZERO COPY: the preimage `action ‖ nonce ‖ vault ‖ [expiry]` is
/// absorbed part by part; only the ≤ 30-byte tail is built on the
/// stack. The first cut staged the whole action into a 4 KiB buffer
/// to hash one slice.
///
/// # Errors
/// Never, today: the tail is fixed-size and the action is taken as
/// is. Kept as a `Result` so the signature stays the one every caller
/// already handles.
#[inline]
pub fn connection_id(
    action: &[u8],
    nonce: u64,
    vault: Vault,
    expires_after: Option<u64>,
) -> Result<[u8; 32], SignError> {
    // nonce (8) ‖ vault marker (1 | 21) ‖ expiry (0 | 9)
    let mut tail = [0u8; 8 + 21 + 9];
    let mut n = 0usize;

    // nonce, big-endian u64
    // COPY: 8 B nonce into the ≤ 38 B stack tail that keccak absorbs
    // right after the action (`keccak256_parts`) — the hash input is
    // action ‖ tail and the tail does not exist anywhere else; the
    // action itself is NOT copied.
    tail[n..n + 8].copy_from_slice(&nonce.to_be_bytes());
    n += 8;

    // vault marker — ALWAYS written
    match vault {
        Vault::None => {
            tail[n] = 0x00;
            n += 1;
        }
        Vault::Address(addr) => {
            tail[n] = 0x01;
            n += 1;
            // COPY: 20 B vault address into the same stack tail.
            tail[n..n + 20].copy_from_slice(&addr);
            n += 20;
        }
    }

    // expiry tail — only when set, and AFTER the vault byte
    if let Some(exp) = expires_after {
        tail[n] = 0x00;
        n += 1;
        // COPY: 8 B expiry into the same stack tail.
        tail[n..n + 8].copy_from_slice(&exp.to_be_bytes());
        n += 8;
    }

    Ok(keccak256_parts(&[action, &tail[..n]]))
}

/// Which network the signature is for. **A parameter, never a
/// default** — the wrong one rejects every order with a valid
/// signature from the wrong address.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// `api.hyperliquid.xyz`, source `"a"`.
    Mainnet,
    /// `api.hyperliquid-testnet.xyz`, source `"b"`.
    Testnet,
}

impl Network {
    /// The `source` string this network signs under.
    #[inline(always)]
    #[must_use]
    pub const fn source(self) -> &'static [u8] {
        match self {
            Network::Mainnet => hl::HL_SOURCE_MAINNET,
            Network::Testnet => hl::HL_SOURCE_TESTNET,
        }
    }
}

/// The full path: encoded action → connection id → `Agent` digest →
/// 65-byte `r‖s‖v`.
///
/// Hot path. The caller holds a parsed key from boot.
#[inline]
pub fn sign_action(
    sk: &secp256k1::SecretKey,
    action: &[u8],
    nonce: u64,
    vault: Vault,
    expires_after: Option<u64>,
    network: Network,
) -> Result<[u8; 65], SignError> {
    let cid = connection_id(action, nonce, vault, expires_after)?;
    hl::sign_agent_with_key(sk, network.source(), &cid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vault_marker_is_always_written() {
        // Same action and nonce, no vault vs a vault: different hash.
        let a = connection_id(b"\x80", 1, Vault::None, None).unwrap();
        let b = connection_id(b"\x80", 1, Vault::Address([0u8; 20]), None).unwrap();
        assert_ne!(a, b, "a zero vault address is not the same as no vault");
    }

    #[test]
    fn the_expiry_tail_changes_the_hash() {
        let a = connection_id(b"\x80", 1, Vault::None, None).unwrap();
        let b = connection_id(b"\x80", 1, Vault::None, Some(0)).unwrap();
        assert_ne!(a, b, "an expiry of ZERO is not the same as no expiry");
    }

    #[test]
    fn the_nonce_reaches_the_hash() {
        let a = connection_id(b"\x80", 1, Vault::None, None).unwrap();
        let b = connection_id(b"\x80", 2, Vault::None, None).unwrap();
        assert_ne!(a, b);
    }

    /// The preimage is hashed part by part; it must equal the one-slice
    /// hash of the concatenation, for every tail shape.
    #[test]
    fn the_incremental_hash_equals_the_concatenated_one() {
        let action = [0x83u8, 0xa4, b't', b'y', b'p', b'e', 0xa5, 1, 2, 3, 4, 5];
        let nonce = 0x0102_0304_0506_0708u64;
        let vault = [0x77u8; 20];
        for (v, exp) in [
            (Vault::None, None),
            (Vault::Address(vault), None),
            (Vault::None, Some(9u64)),
            (Vault::Address(vault), Some(0u64)),
        ] {
            let mut flat = Vec::new();
            flat.extend_from_slice(&action);
            flat.extend_from_slice(&nonce.to_be_bytes());
            match v {
                Vault::None => flat.push(0),
                Vault::Address(a) => {
                    flat.push(1);
                    flat.extend_from_slice(&a);
                }
            }
            if let Some(e) = exp {
                flat.push(0);
                flat.extend_from_slice(&e.to_be_bytes());
            }
            assert_eq!(
                connection_id(&action, nonce, v, exp).unwrap(),
                signer_eip712::keccak256(&flat)
            );
        }
    }

    #[test]
    fn the_networks_sources_are_the_venues() {
        assert_eq!(Network::Mainnet.source(), b"a");
        assert_eq!(Network::Testnet.source(), b"b");
    }
}
