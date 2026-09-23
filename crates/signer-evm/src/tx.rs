// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! EIP-1559 (type `0x02`) transactions: signing digest, signature, hash,
//! and the `0x`-hex render into the caller's final request buffer.
//!
//! `0x02 ‖ rlp([chain_id, nonce, max_priority_fee_per_gas,
//! max_fee_per_gas, gas_limit, to, value, data, access_list])` is the
//! signing pre-image; the signed form appends `y_parity, r, s` to the
//! list. `access_list` is always EMPTY — the struct has no field for
//! one, so a non-empty list cannot be expressed.

use crate::rlp::{address, list_header, string_header, uint, word, Enc};
use signer_eip712::{keccak256_parts, sign_digest_with_key, SecretKey};

/// EIP-2718 type byte of a dynamic-fee transaction.
const TX_TYPE: u8 = 0x02;
/// RLP of the empty access list.
const EMPTY_LIST: [u8; 1] = [0xc0];
const HEX: &[u8; 16] = b"0123456789abcdef";

/// One EIP-1559 transaction. `data` is BORROWED — it is hashed and
/// rendered straight from the caller's calldata buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Eip1559Tx<'a> {
    /// EIP-155 chain id (HyperEVM: 999 mainnet, 998 testnet).
    pub chain_id: u64,
    /// Sender nonce.
    pub nonce: u64,
    /// Priority fee (tip) per gas, wei.
    pub max_priority_fee_per_gas: u128,
    /// Fee cap per gas, wei.
    pub max_fee_per_gas: u128,
    /// Gas limit.
    pub gas_limit: u64,
    /// Recipient (always a call — contract creation is not expressible).
    pub to: [u8; 20],
    /// Value, wei.
    pub value: u128,
    /// Calldata.
    pub data: &'a [u8],
}

/// Why a transaction could not be signed or rendered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EvmTxErr {
    /// The destination buffer cannot hold the render.
    BufferTooSmall,
    /// `v` was not 27 or 28 — refused, never masked to a parity.
    BadSignature,
    /// The secp256k1 signer refused.
    Sign,
}

impl core::fmt::Display for EvmTxErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::BufferTooSmall => "destination buffer too small for the transaction",
            Self::BadSignature => "signature v is not 27/28",
            Self::Sign => "secp256k1 signing failed",
        })
    }
}

impl std::error::Error for EvmTxErr {}

/// The encoded list items that precede `data`, `data`'s header, and the
/// payload length of the UNSIGNED list.
struct Fields {
    head: [Enc; 6], // chain_id, nonce, prio, max_fee, gas, to
    value: Enc,
    data_hdr: Enc,
    unsigned_payload: usize,
}

#[inline]
fn fields(tx: &Eip1559Tx<'_>) -> Fields {
    let head = [
        uint(tx.chain_id as u128),
        uint(tx.nonce as u128),
        uint(tx.max_priority_fee_per_gas),
        uint(tx.max_fee_per_gas),
        uint(tx.gas_limit as u128),
        address(&tx.to),
    ];
    let value = uint(tx.value);
    let first = if tx.data.is_empty() { 0 } else { tx.data[0] };
    let data_hdr = string_header(tx.data.len(), first);
    let mut payload = 0usize;
    let mut i = 0;
    while i < head.len() {
        payload += head[i].len();
        i += 1;
    }
    payload += value.len() + data_hdr.len() + tx.data.len() + EMPTY_LIST.len();
    Fields {
        head,
        value,
        data_hdr,
        unsigned_payload: payload,
    }
}

/// `(y_parity, r, s)` encoded, from a `r ‖ s ‖ v` signature.
#[inline]
fn sig_fields(sig: &[u8; 65]) -> Result<[Enc; 3], EvmTxErr> {
    let v = sig[64];
    if v != 27 && v != 28 {
        return Err(EvmTxErr::BadSignature);
    }
    let (rs, _) = sig.split_at(64);
    let (r, s) = rs.split_at(32);
    Ok([uint(crate::y_parity_from_v(v) as u128), word(r), word(s)])
}

/// keccak256 of the signing pre-image — hashed IN PLACE from stack
/// encodings and the borrowed calldata. No pre-image buffer exists.
#[must_use]
pub fn tx_signing_digest(tx: &Eip1559Tx<'_>) -> [u8; 32] {
    let f = fields(tx);
    let lh = list_header(f.unsigned_payload);
    keccak256_parts(&[
        &[TX_TYPE],
        lh.as_slice(),
        f.head[0].as_slice(),
        f.head[1].as_slice(),
        f.head[2].as_slice(),
        f.head[3].as_slice(),
        f.head[4].as_slice(),
        f.head[5].as_slice(),
        f.value.as_slice(),
        f.data_hdr.as_slice(),
        tx.data,
        &EMPTY_LIST,
    ])
}

/// Sign the transaction with a pre-parsed key: `r ‖ s ‖ v`, `v = recid + 27`
/// (feed it to [`tx_encode_signed_hex`] / [`tx_hash`], which convert `v`).
pub fn tx_sign(tx: &Eip1559Tx<'_>, sk: &SecretKey) -> Result<[u8; 65], EvmTxErr> {
    sign_digest_with_key(sk, &tx_signing_digest(tx)).map_err(|_| EvmTxErr::Sign)
}

/// The transaction hash (`keccak256` of the signed envelope) — what a
/// node returns from `eth_sendRawTransaction` and what the receipt is
/// looked up by. Hashed in place, like the digest.
pub fn tx_hash(tx: &Eip1559Tx<'_>, sig: &[u8; 65]) -> Result<[u8; 32], EvmTxErr> {
    let f = fields(tx);
    let s = sig_fields(sig)?;
    let lh = list_header(f.unsigned_payload + s[0].len() + s[1].len() + s[2].len());
    Ok(keccak256_parts(&[
        &[TX_TYPE],
        lh.as_slice(),
        f.head[0].as_slice(),
        f.head[1].as_slice(),
        f.head[2].as_slice(),
        f.head[3].as_slice(),
        f.head[4].as_slice(),
        f.head[5].as_slice(),
        f.value.as_slice(),
        f.data_hdr.as_slice(),
        tx.data,
        &EMPTY_LIST,
        s[0].as_slice(),
        s[1].as_slice(),
        s[2].as_slice(),
    ]))
}

/// Length in bytes of the `0x`-hex render of the signed transaction.
pub fn signed_hex_len(tx: &Eip1559Tx<'_>, sig: &[u8; 65]) -> Result<usize, EvmTxErr> {
    let f = fields(tx);
    let s = sig_fields(sig)?;
    let payload = f.unsigned_payload + s[0].len() + s[1].len() + s[2].len();
    Ok(2 + 2 * (1 + list_header(payload).len() + payload))
}

/// Writes `0x`-prefixed lowercase hex into a caller buffer.
struct HexOut<'b> {
    dst: &'b mut [u8],
    n: usize,
}

impl HexOut<'_> {
    #[inline(always)]
    fn put(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            self.dst[self.n] = HEX[(b >> 4) as usize];
            self.dst[self.n + 1] = HEX[(b & 0x0f) as usize];
            self.n += 2;
            i += 1;
        }
    }
}

/// Render the SIGNED transaction as `0x`-prefixed lowercase hex straight
/// into `dst` — the `params[0]` string of an `eth_sendRawTransaction`
/// body the caller is assembling. Returns the bytes written. Nothing is
/// written unless all of it fits.
///
/// This is the render into the FINAL wire buffer: the calldata is read
/// once from the caller's slice and hex-encoded in place; no binary
/// transaction buffer exists to copy from.
pub fn tx_encode_signed_hex(
    tx: &Eip1559Tx<'_>,
    sig: &[u8; 65],
    dst: &mut [u8],
) -> Result<usize, EvmTxErr> {
    let f = fields(tx);
    let s = sig_fields(sig)?;
    let payload = f.unsigned_payload + s[0].len() + s[1].len() + s[2].len();
    let lh = list_header(payload);
    let total = 2 + 2 * (1 + lh.len() + payload);
    if dst.len() < total {
        return Err(EvmTxErr::BufferTooSmall);
    }
    dst[0] = b'0';
    dst[1] = b'x';
    let mut w = HexOut { dst, n: 2 };
    w.put(&[TX_TYPE]);
    w.put(lh.as_slice());
    let mut i = 0;
    while i < f.head.len() {
        w.put(f.head[i].as_slice());
        i += 1;
    }
    w.put(f.value.as_slice());
    w.put(f.data_hdr.as_slice());
    w.put(tx.data);
    w.put(&EMPTY_LIST);
    w.put(s[0].as_slice());
    w.put(s[1].as_slice());
    w.put(s[2].as_slice());
    debug_assert_eq!(w.n, total);
    Ok(total)
}
