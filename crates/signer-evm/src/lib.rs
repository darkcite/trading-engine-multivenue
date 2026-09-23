// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `signer-evm` — EIP-1559 transactions, hashed and rendered in place
//!
//! HYPARB H7 (O-H7). Chain-agnostic: nothing here knows it is on
//! HyperEVM, so chain #2 reuses it unchanged. The secp256k1 primitive and
//! keccak come from `signer-eip712`, which this crate does NOT modify.
//!
//! ## Zero-copy (the doctrine is binding here)
//!
//! * **The signer hashes in place.** Every RLP field is encoded into a
//!   few bytes on the stack; the calldata is BORROWED; the digest is
//!   `keccak256_parts` over those slices. No pre-image buffer exists.
//! * **The encoder renders into the FINAL wire buffer.** JSON-RPC carries
//!   a transaction as `0x`-hex, so [`tx_encode_signed_hex`] writes the
//!   hex straight into the request body the caller is building — there
//!   is no binary transaction buffer to copy from, because nothing on
//!   the wire needs one. The transaction hash (for receipt tracking) is
//!   likewise a parts-hash, [`tx_hash`].
//!
//! ## The signature trap
//!
//! `signer_eip712::sign_digest_with_key` returns `r ‖ s ‖ v` with
//! **`v = recid + 27`** (legacy Ethereum). A type-2 transaction carries
//! **`y_parity ∈ {0, 1}`**. Getting that wrong yields well-formed
//! transactions that every node rejects — a silent 100 % failure.
//! [`y_parity_from_v`] is the one conversion; a signature whose `v` is
//! not 27 or 28 is refused, never masked.

#![forbid(unsafe_code)]
#![deny(missing_docs, unused_imports, unused_must_use, unreachable_pub)]

mod rlp;
mod tx;

pub use tx::{
    create_address, create_encode_signed_hex, create_hash, create_sign, create_signing_digest,
    signed_hex_len, tx_encode_signed_hex, tx_hash, tx_sign, tx_signing_digest, Eip1559Create,
    Eip1559Tx, EvmTxErr,
};

/// `v = recid + 27` (EIP-712 / legacy) → `y_parity ∈ {0, 1}` (EIP-1559).
#[inline]
#[must_use]
pub const fn y_parity_from_v(v: u8) -> u8 {
    v.wrapping_sub(27) & 1
}
