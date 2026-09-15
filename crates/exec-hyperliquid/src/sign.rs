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
use signer_eip712::{keccak256, SignError};

/// Scratch for `msgpack ‖ nonce ‖ vault ‖ expiry`. The action itself is
/// capped at [`crate::action::MAX_ACTION`]; the tail is at most
/// 8 + 1 + 20 + 1 + 8 = 38 bytes.
pub const MAX_HASH_INPUT: usize = crate::action::MAX_ACTION + 64;

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
#[inline]
pub fn connection_id(
    action: &[u8],
    nonce: u64,
    vault: Vault,
    expires_after: Option<u64>,
) -> Result<[u8; 32], SignError> {
    let mut buf = [0u8; MAX_HASH_INPUT];
    let mut n = 0usize;

    if action.len() > buf.len() {
        return Err(SignError::InvalidKey);
    }
    buf[..action.len()].copy_from_slice(action);
    n += action.len();

    // nonce, big-endian u64
    if n + 8 > buf.len() {
        return Err(SignError::InvalidKey);
    }
    buf[n..n + 8].copy_from_slice(&nonce.to_be_bytes());
    n += 8;

    // vault marker — ALWAYS written
    match vault {
        Vault::None => {
            if n + 1 > buf.len() {
                return Err(SignError::InvalidKey);
            }
            buf[n] = 0x00;
            n += 1;
        }
        Vault::Address(addr) => {
            if n + 21 > buf.len() {
                return Err(SignError::InvalidKey);
            }
            buf[n] = 0x01;
            n += 1;
            buf[n..n + 20].copy_from_slice(&addr);
            n += 20;
        }
    }

    // expiry tail — only when set, and AFTER the vault byte
    if let Some(exp) = expires_after {
        if n + 9 > buf.len() {
            return Err(SignError::InvalidKey);
        }
        buf[n] = 0x00;
        n += 1;
        buf[n..n + 8].copy_from_slice(&exp.to_be_bytes());
        n += 8;
    }

    Ok(keccak256(&buf[..n]))
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

    #[test]
    fn an_oversized_action_is_refused_not_truncated() {
        let huge = vec![0u8; MAX_HASH_INPUT + 1];
        assert!(connection_id(&huge, 1, Vault::None, None).is_err());
    }

    #[test]
    fn the_networks_sources_are_the_venues() {
        assert_eq!(Network::Mainnet.source(), b"a");
        assert_eq!(Network::Testnet.source(), b"b");
    }
}
