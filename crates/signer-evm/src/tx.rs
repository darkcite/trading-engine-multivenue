// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! EIP-1559 (type `0x02`) transactions: signing digest, signature, hash,
//! and the `0x`-hex render into the caller's final request buffer.
//!
//! `0x02 ‖ rlp([chain_id, nonce, max_priority_fee_per_gas,
//! max_fee_per_gas, gas_limit, to, value, data, access_list])` is the
//! signing pre-image; the signed form appends `y_parity, r, s` to the
//! list. `access_list` is always EMPTY — the struct has no field for
//! one, so a non-empty list cannot be expressed. A contract creation
//! ([`Eip1559Create`], HYPARB H7c — the executor's testnet deployer) is
//! the same list with `to` the empty string; both shapes share one
//! digest / hash / render core, so a creation cannot drift from a call.

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
    /// Recipient (always a call; a creation is [`Eip1559Create`]).
    pub to: [u8; 20],
    /// Value, wei.
    pub value: u128,
    /// Calldata.
    pub data: &'a [u8],
}

/// One EIP-1559 CONTRACT CREATION: no recipient, `init_code` as the
/// data. A separate type rather than an optional `to` on [`Eip1559Tx`],
/// so a call can never be turned into a creation by a zeroed field.
/// Cold path (a deployer); `init_code` is BORROWED like calldata.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Eip1559Create<'a> {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Sender nonce (the deployed address is a function of it —
    /// [`create_address`]).
    pub nonce: u64,
    /// Priority fee (tip) per gas, wei.
    pub max_priority_fee_per_gas: u128,
    /// Fee cap per gas, wei.
    pub max_fee_per_gas: u128,
    /// Gas limit.
    pub gas_limit: u64,
    /// Value endowed to the new contract, wei.
    pub value: u128,
    /// Creation bytecode (constructor + runtime).
    pub init_code: &'a [u8],
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

/// The fields of either transaction shape. `to` is the encoded
/// destination: a 21-byte address string for a call, the empty string
/// `0x80` for a contract creation.
#[inline]
#[allow(clippy::too_many_arguments)]
fn fields_of(
    chain_id: u64,
    nonce: u64,
    prio: u128,
    max_fee: u128,
    gas_limit: u64,
    to: Enc,
    value: u128,
    data: &[u8],
) -> Fields {
    let head = [
        uint(chain_id as u128),
        uint(nonce as u128),
        uint(prio),
        uint(max_fee),
        uint(gas_limit as u128),
        to,
    ];
    let value = uint(value);
    let first = if data.is_empty() { 0 } else { data[0] };
    let data_hdr = string_header(data.len(), first);
    let mut payload = 0usize;
    let mut i = 0;
    while i < head.len() {
        payload += head[i].len();
        i += 1;
    }
    payload += value.len() + data_hdr.len() + data.len() + EMPTY_LIST.len();
    Fields {
        head,
        value,
        data_hdr,
        unsigned_payload: payload,
    }
}

#[inline]
fn fields(tx: &Eip1559Tx<'_>) -> Fields {
    fields_of(
        tx.chain_id,
        tx.nonce,
        tx.max_priority_fee_per_gas,
        tx.max_fee_per_gas,
        tx.gas_limit,
        address(&tx.to),
        tx.value,
        tx.data,
    )
}

#[inline]
fn create_fields(tx: &Eip1559Create<'_>) -> Fields {
    fields_of(
        tx.chain_id,
        tx.nonce,
        tx.max_priority_fee_per_gas,
        tx.max_fee_per_gas,
        tx.gas_limit,
        uint(0), // the empty string: no recipient
        tx.value,
        tx.init_code,
    )
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

/// keccak256 of a signing pre-image, hashed in place.
#[inline]
fn digest_of(f: &Fields, data: &[u8]) -> [u8; 32] {
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
        data,
        &EMPTY_LIST,
    ])
}

/// keccak256 of a signed envelope, hashed in place.
#[inline]
fn hash_of(f: &Fields, data: &[u8], sig: &[u8; 65]) -> Result<[u8; 32], EvmTxErr> {
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
        data,
        &EMPTY_LIST,
        s[0].as_slice(),
        s[1].as_slice(),
        s[2].as_slice(),
    ]))
}

/// Length of the `0x`-hex render of a signed envelope.
#[inline]
fn hex_len_of(f: &Fields, sig: &[u8; 65]) -> Result<usize, EvmTxErr> {
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

/// Render a signed envelope as `0x`-hex into `dst`; nothing is written
/// unless all of it fits.
#[inline]
fn render_of(f: &Fields, data: &[u8], sig: &[u8; 65], dst: &mut [u8]) -> Result<usize, EvmTxErr> {
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
    w.put(data);
    w.put(&EMPTY_LIST);
    w.put(s[0].as_slice());
    w.put(s[1].as_slice());
    w.put(s[2].as_slice());
    debug_assert_eq!(w.n, total);
    Ok(total)
}

/// keccak256 of the signing pre-image — hashed IN PLACE from stack
/// encodings and the borrowed calldata. No pre-image buffer exists.
#[must_use]
pub fn tx_signing_digest(tx: &Eip1559Tx<'_>) -> [u8; 32] {
    digest_of(&fields(tx), tx.data)
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
    hash_of(&fields(tx), tx.data, sig)
}

/// Length in bytes of the `0x`-hex render of the signed transaction.
pub fn signed_hex_len(tx: &Eip1559Tx<'_>, sig: &[u8; 65]) -> Result<usize, EvmTxErr> {
    hex_len_of(&fields(tx), sig)
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
    render_of(&fields(tx), tx.data, sig, dst)
}

/// [`tx_signing_digest`] of a contract creation.
#[must_use]
pub fn create_signing_digest(tx: &Eip1559Create<'_>) -> [u8; 32] {
    digest_of(&create_fields(tx), tx.init_code)
}

/// [`tx_sign`] of a contract creation.
pub fn create_sign(tx: &Eip1559Create<'_>, sk: &SecretKey) -> Result<[u8; 65], EvmTxErr> {
    sign_digest_with_key(sk, &create_signing_digest(tx)).map_err(|_| EvmTxErr::Sign)
}

/// [`tx_hash`] of a contract creation.
pub fn create_hash(tx: &Eip1559Create<'_>, sig: &[u8; 65]) -> Result<[u8; 32], EvmTxErr> {
    hash_of(&create_fields(tx), tx.init_code, sig)
}

/// [`tx_encode_signed_hex`] of a contract creation.
pub fn create_encode_signed_hex(
    tx: &Eip1559Create<'_>,
    sig: &[u8; 65],
    dst: &mut [u8],
) -> Result<usize, EvmTxErr> {
    render_of(&create_fields(tx), tx.init_code, sig, dst)
}

/// The address a creation from `sender` at `nonce` deploys to:
/// `keccak256(rlp([sender, nonce]))[12..]` — hashed in place.
#[must_use]
pub fn create_address(sender: &[u8; 20], nonce: u64) -> [u8; 20] {
    let a = address(sender);
    let n = uint(nonce as u128);
    let lh = list_header(a.len() + n.len());
    let h = keccak256_parts(&[lh.as_slice(), a.as_slice(), n.as_slice()]);
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = h[12 + i];
        i += 1;
    }
    out
}
