// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Polymarket CLOB **V2** (PM15-6a — sign-only, no network): the Exchange
//! V2 `Order`, the L1 `ClobAuth`, and the Deposit Wallet's (POLY_1271)
//! ERC-7739 `TypedDataSign` wrapper.
//!
//! The authority on every byte is the official SDKs' known-answer vectors
//! (`py-clob-client-v2` f11b2c0, `clob-client-v2` 27633b3,
//! `rs-clob-client-v2` 561830b — project doc
//! `claude/pm15-clob-v2-spec-2026-10-17.md` §C, §J), reproduced here bit
//! for bit by the tests, plus vectors an independent stdlib
//! re-implementation computed after it had reproduced every SDK one.
//! The crate root's V1 order (domain `"1"`) is the pre-cutover form the
//! venue no longer accepts; nothing here touches it.
//!
//! ## The order, per signature type
//!
//! | type | `maker` | `signer` | signature |
//! |---|---|---|---|
//! | 0 EOA | EOA | EOA | 65 B `r‖s‖v` over [`order_v2_digest`] |
//! | 1 POLY_PROXY | proxy wallet | owner EOA | 65 B over [`order_v2_digest`] |
//! | 2 POLY_GNOSIS_SAFE | Safe | owner EOA | 65 B over [`order_v2_digest`] |
//! | 3 POLY_1271 | deposit wallet | deposit wallet | [`poly1271_wrap`] of a 65 B signature over [`poly1271_digest`] |
//!
//! The `orderID` the venue answers with is [`order_v2_digest`] for every
//! type (the wrapper changes only the signature).
//!
//! ## Cost
//!
//! The separators are computed once at boot ([`exchange_separator`],
//! [`clob_auth_separator`]); the typehashes are constants pinned against
//! the keccak of their type strings. Per order: one 12-word struct hash,
//! one 2-block digest (two for type 3), one secp256k1 signature. Zero
//! allocation: every word is a stack array.

use crate::{
    eip712_digest, enc_address, enc_u64, keccak256, keccak256_parts, Eip712Domain, SignError,
};

/// The Exchange V2 order type string (186 bytes; no `expiration`,
/// `nonce`, `feeRateBps` or `taker` — those left at the V2 cutover).
pub const ORDER_V2_TYPE: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";

/// ERC-7739's `TypedDataSign` over an [`ORDER_V2_TYPE`] — the Deposit
/// Wallet's (Solady) nested-EIP-712 type.
pub const TYPED_DATA_SIGN_TYPE: &str = concat!(
    "TypedDataSign(Order contents,string name,string version,uint256 chainId,address verifyingContract,bytes32 salt)",
    "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)"
);

/// The L1 auth type string (API-key creation and derivation only).
pub const CLOB_AUTH_TYPE: &str =
    "ClobAuth(address address,string timestamp,uint256 nonce,string message)";

/// The L1 auth domain's type: three fields, no `verifyingContract`.
pub const DOMAIN3_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId)";

/// The L1 auth's fixed `message` field.
pub const CLOB_AUTH_MESSAGE: &str = "This message attests that I control the given wallet";

/// The Exchange domain's `name`.
pub const EXCHANGE_DOMAIN_NAME: &str = "Polymarket CTF Exchange";

/// Polygon mainnet.
pub const CHAIN_POLYGON: u64 = 137;

/// `side`: buy.
pub const SIDE_BUY: u8 = 0;
/// `side`: sell.
pub const SIDE_SELL: u8 = 1;

/// `signatureType` 0: an EOA signs for itself.
pub const SIG_EOA: u8 = 0;
/// `signatureType` 1: an EOA signs for its Polymarket proxy wallet.
pub const SIG_POLY_PROXY: u8 = 1;
/// `signatureType` 2: an EOA signs for its Gnosis Safe.
pub const SIG_POLY_GNOSIS_SAFE: u8 = 2;
/// `signatureType` 3: EIP-1271 — the Deposit Wallet (every polymarket.com
/// account deployed on or after 2026-05-04).
pub const SIG_POLY_1271: u8 = 3;

/// A POLY_1271 signature's length: 65 + 32 + 32 + 186 + 2.
pub const POLY_1271_SIG_LEN: usize = 65 + 32 + 32 + ORDER_V2_TYPE.len() + 2;

/// `keccak256(`[`ORDER_V2_TYPE`]`)` (pinned by the tests).
pub const ORDER_V2_TYPEHASH: [u8; 32] = [
    0xbb, 0x86, 0x31, 0x8a, 0x21, 0x38, 0xf5, 0xfa, 0x8a, 0xe3, 0x2f, 0xbe, 0x8e, 0x65, 0x9f, 0x8f,
    0xcf, 0x13, 0xcc, 0x6a, 0xe4, 0x01, 0x4a, 0x70, 0x78, 0x93, 0x05, 0x54, 0x33, 0x81, 0x85, 0x89,
];

/// `keccak256(`[`TYPED_DATA_SIGN_TYPE`]`)` (pinned by the tests).
pub const TYPED_DATA_SIGN_TYPEHASH: [u8; 32] = [
    0x6b, 0xa0, 0x28, 0x56, 0x5c, 0xb3, 0x24, 0xc2, 0xaa, 0x02, 0xbb, 0x71, 0x4b, 0x98, 0x16, 0xd0,
    0xbd, 0xdd, 0x55, 0x7a, 0x2f, 0x33, 0xbb, 0x36, 0xcf, 0x13, 0x27, 0x2a, 0x42, 0x56, 0xbd, 0x42,
];

/// `keccak256(`[`CLOB_AUTH_TYPE`]`)` (pinned by the tests).
pub const CLOB_AUTH_TYPEHASH: [u8; 32] = [
    0x52, 0x57, 0x8c, 0x5c, 0x72, 0x5a, 0x28, 0xa8, 0x4f, 0xed, 0xc8, 0xc2, 0x2a, 0xa4, 0x79, 0x47,
    0x82, 0x29, 0x42, 0xf3, 0x5b, 0x4d, 0xc3, 0x50, 0xdb, 0x02, 0x8e, 0x45, 0x32, 0x0e, 0x03, 0x5c,
];

/// `keccak256(`[`DOMAIN3_TYPE`]`)` — the three-field domain of `ClobAuth`.
pub const DOMAIN3_TYPEHASH: [u8; 32] = [
    0xc2, 0xf8, 0x78, 0x71, 0x76, 0xb8, 0xac, 0x6b, 0xf7, 0x21, 0x5b, 0x4a, 0xdc, 0xc1, 0xe0, 0x69,
    0xbf, 0x4a, 0xb8, 0x2d, 0x9a, 0xb1, 0xdf, 0x05, 0xa5, 0x7a, 0x91, 0xd4, 0x25, 0x93, 0x5b, 0x6e,
];

/// `keccak256("DepositWallet")`.
pub const DEPOSIT_WALLET_NAME_HASH: [u8; 32] = [
    0xd6, 0x82, 0xb5, 0x29, 0xa1, 0x7c, 0xda, 0x19, 0xaa, 0x27, 0x5f, 0x3a, 0x05, 0x06, 0x08, 0xf9,
    0xe9, 0x40, 0x1f, 0xad, 0xd1, 0xb0, 0xd2, 0x33, 0xd8, 0x15, 0x19, 0x97, 0x22, 0x95, 0x82, 0x8b,
];

/// `keccak256("1")`.
pub const VERSION_1_HASH: [u8; 32] = [
    0xc8, 0x9e, 0xfd, 0xaa, 0x54, 0xc0, 0xf2, 0x0c, 0x7a, 0xdf, 0x61, 0x28, 0x82, 0xdf, 0x09, 0x50,
    0xf5, 0xa9, 0x51, 0x63, 0x7e, 0x03, 0x07, 0xcd, 0xcb, 0x4c, 0x67, 0x2f, 0x29, 0x8b, 0x8b, 0xc6,
];

/// `keccak256("ClobAuthDomain")`.
pub const CLOB_AUTH_NAME_HASH: [u8; 32] = [
    0x64, 0xfe, 0xab, 0xfd, 0x0e, 0x91, 0xbe, 0xd1, 0x7f, 0x42, 0x70, 0x58, 0x55, 0x53, 0xca, 0x84,
    0x56, 0x29, 0x8e, 0x8f, 0x50, 0x73, 0x63, 0xa4, 0xb7, 0x25, 0xbe, 0x4e, 0x4d, 0xa2, 0x98, 0x10,
];

/// `keccak256(`[`CLOB_AUTH_MESSAGE`]`)`.
pub const CLOB_AUTH_MESSAGE_HASH: [u8; 32] = [
    0x47, 0x49, 0x24, 0xbb, 0x27, 0x02, 0x25, 0x42, 0x02, 0x00, 0x23, 0xb4, 0xa5, 0x2f, 0xb8, 0x34,
    0xca, 0x5e, 0x3d, 0xd7, 0x0b, 0x9a, 0x4d, 0x7e, 0x7f, 0xe9, 0xca, 0x13, 0x22, 0x8c, 0xbd, 0xa3,
];

/// CTF Exchange V2 (`negRisk:false`, market `version:"v1"`, domain `"2"`): `0xE111180000d2663C0091e4f400237545B87B996B`.
pub const EXCHANGE_V2: [u8; 20] = [
    0xe1, 0x11, 0x18, 0x00, 0x00, 0xd2, 0x66, 0x3c, 0x00, 0x91, 0xe4, 0xf4, 0x00, 0x23, 0x75, 0x45,
    0xb8, 0x7b, 0x99, 0x6b,
];

/// Neg Risk CTF Exchange V2 (domain `"2"`): `0xe2222d279d744050d28e00520010520000310F59`.
pub const NEG_RISK_EXCHANGE_V2: [u8; 20] = [
    0xe2, 0x22, 0x2d, 0x27, 0x9d, 0x74, 0x40, 0x50, 0xd2, 0x8e, 0x00, 0x52, 0x00, 0x10, 0x52, 0x00,
    0x00, 0x31, 0x0f, 0x59,
];

/// ExchangeV3 (Protocol V2 markets, `version:"v2"`, domain `"3"`): `0xe3333700cA9d93003F00f0F71f8515005F6c00Aa`.
pub const EXCHANGE_V3: [u8; 20] = [
    0xe3, 0x33, 0x37, 0x00, 0xca, 0x9d, 0x93, 0x00, 0x3f, 0x00, 0xf0, 0xf7, 0x1f, 0x85, 0x15, 0x00,
    0x5f, 0x6c, 0x00, 0xaa,
];

/// pUSD, the collateral (6 decimals): `0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB`.
pub const PUSD: [u8; 20] = [
    0xc0, 0x11, 0xa7, 0xe1, 0x2a, 0x19, 0xf7, 0xb1, 0xf6, 0x70, 0xd4, 0x6f, 0x03, 0xb0, 0x3f, 0x33,
    0x42, 0xe8, 0x2d, 0xfb,
];

/// The Conditional Tokens Framework (ERC-1155): `0x4D97DCd97eC945f40cF65F87097ACe5EA0476045`.
pub const CONDITIONAL_TOKENS: [u8; 20] = [
    0x4d, 0x97, 0xdc, 0xd9, 0x7e, 0xc9, 0x45, 0xf4, 0x0c, 0xf6, 0x5f, 0x87, 0x09, 0x7a, 0xce, 0x5e,
    0xa0, 0x47, 0x60, 0x45,
];

/// CtfCollateralAdapter (redeems a standard CTF market to pUSD): `0xAdA100Db00Ca00073811820692005400218FcE1f`.
pub const CTF_COLLATERAL_ADAPTER: [u8; 20] = [
    0xad, 0xa1, 0x00, 0xdb, 0x00, 0xca, 0x00, 0x07, 0x38, 0x11, 0x82, 0x06, 0x92, 0x00, 0x54, 0x00,
    0x21, 0x8f, 0xce, 0x1f,
];

/// The Deposit Wallet factory (the relayer `WALLET` batch's `to`): `0x00000000000Fb5C9ADea0298D729A0CB3823Cc07`.
pub const DEPOSIT_WALLET_FACTORY: [u8; 20] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x0f, 0xb5, 0xc9, 0xad, 0xea, 0x02, 0x98, 0xd7, 0x29, 0xa0, 0xcb,
    0x38, 0x23, 0xcc, 0x07,
];

/// One Exchange V2 order, as the struct hash reads it.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderV2 {
    /// Random per order, ≤ 2^53 − 1 (it rides the JSON as a number).
    pub salt: u64,
    /// The funds' holder (see the module table).
    pub maker: [u8; 20],
    /// The signing address (see the module table).
    pub signer: [u8; 20],
    /// The outcome token id, a big-endian `uint256` ([`u256_from_decimal`]).
    pub token_id: [u8; 32],
    /// What the maker gives, 6-decimal base units (a BUY: pUSD).
    pub maker_amount: u64,
    /// What the maker receives, 6-decimal base units (a BUY: shares).
    pub taker_amount: u64,
    /// The order's creation, Unix ms (uniqueness, not an expiry).
    pub timestamp_ms: u64,
    /// Zero unless attached.
    pub metadata: [u8; 32],
    /// The builder code; zero unless attached.
    pub builder: [u8; 32],
    /// [`SIDE_BUY`] or [`SIDE_SELL`].
    pub side: u8,
    /// [`SIG_EOA`] … [`SIG_POLY_1271`].
    pub signature_type: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
}

/// The Exchange domain of `version` (`"2"`; `"3"` for ExchangeV3) on
/// `chain_id` at `contract`. Boot-time.
#[must_use]
pub fn exchange_domain(version: &[u8], chain_id: u64, contract: [u8; 20]) -> Eip712Domain {
    Eip712Domain::new(EXCHANGE_DOMAIN_NAME.as_bytes(), version, chain_id, contract)
}

/// The Exchange domain separator (boot-time, once per exchange).
#[must_use]
pub fn exchange_separator(version: &[u8], chain_id: u64, contract: [u8; 20]) -> [u8; 32] {
    crate::domain_separator_of(&exchange_domain(version, chain_id, contract))
}

/// `hashStruct(Order)` — the twelve words absorbed in place.
#[inline]
#[must_use]
pub fn order_v2_struct_hash(o: &OrderV2) -> [u8; 32] {
    keccak256_parts(&[
        &ORDER_V2_TYPEHASH,
        &enc_u64(o.salt),
        &enc_address(&o.maker),
        &enc_address(&o.signer),
        &o.token_id,
        &enc_u64(o.maker_amount),
        &enc_u64(o.taker_amount),
        &enc_u64(u64::from(o.side)),
        &enc_u64(u64::from(o.signature_type)),
        &enc_u64(o.timestamp_ms),
        &o.metadata,
        &o.builder,
    ])
}

/// The order's EIP-712 digest under the exchange separator `sep` — what
/// types 0–2 sign, and the `orderID` the venue answers with for every type.
#[inline]
#[must_use]
pub fn order_v2_digest(sep: &[u8; 32], contents: &[u8; 32]) -> [u8; 32] {
    eip712_digest(sep, contents)
}

/// The digest a Deposit Wallet's owner signs for an order (type 3):
/// `TypedDataSign(contents, "DepositWallet", "1", chainId, wallet, 0)`
/// under the EXCHANGE separator `sep`.
#[inline]
#[must_use]
pub fn poly1271_digest(
    sep: &[u8; 32],
    chain_id: u64,
    wallet: &[u8; 20],
    contents: &[u8; 32],
) -> [u8; 32] {
    let tds = keccak256_parts(&[
        &TYPED_DATA_SIGN_TYPEHASH,
        contents,
        &DEPOSIT_WALLET_NAME_HASH,
        &VERSION_1_HASH,
        &enc_u64(chain_id),
        &enc_address(wallet),
        &[0u8; 32],
    ]);
    eip712_digest(sep, &tds)
}

/// The type-3 signature: the owner's 65 B `inner` ‖ `sep` ‖ `contents` ‖
/// [`ORDER_V2_TYPE`] ‖ its length as a big-endian `u16`, written into
/// `out` (the wire's bytes before hex).
#[inline]
pub fn poly1271_wrap(
    inner: &[u8; 65],
    sep: &[u8; 32],
    contents: &[u8; 32],
    out: &mut [u8; POLY_1271_SIG_LEN],
) {
    // COPY: the four parts of the wrapper into the one signature buffer
    // the body's hex is rendered from — the wrapper IS a concatenation,
    // 317 B per type-3 order, stack only.
    out[..65].copy_from_slice(inner);
    out[65..97].copy_from_slice(sep);
    out[97..129].copy_from_slice(contents);
    let t = ORDER_V2_TYPE.as_bytes();
    out[129..129 + t.len()].copy_from_slice(t);
    let n = t.len() as u16;
    out[129 + t.len()] = (n >> 8) as u8;
    out[130 + t.len()] = n as u8;
}

/// The L1 `ClobAuthDomain` separator on `chain_id` (boot-time).
#[must_use]
pub fn clob_auth_separator(chain_id: u64) -> [u8; 32] {
    keccak256_parts(&[
        &DOMAIN3_TYPEHASH,
        &CLOB_AUTH_NAME_HASH,
        &VERSION_1_HASH,
        &enc_u64(chain_id),
    ])
}

/// The L1 auth digest: `ClobAuth(address, timestamp, nonce, message)`,
/// `timestamp` the Unix SECONDS as the decimal bytes the
/// `POLY_TIMESTAMP` header carries.
#[must_use]
pub fn clob_auth_digest(
    sep: &[u8; 32],
    address: &[u8; 20],
    timestamp_dec: &[u8],
    nonce: u64,
) -> [u8; 32] {
    let st = keccak256_parts(&[
        &CLOB_AUTH_TYPEHASH,
        &enc_address(address),
        &keccak256(timestamp_dec),
        &enc_u64(nonce),
        &CLOB_AUTH_MESSAGE_HASH,
    ]);
    eip712_digest(sep, &st)
}

/// A decimal `uint256` (Gamma's `clobTokenIds` spelling) as 32 big-endian
/// bytes; `None` for an empty string, a non-digit, or a value ≥ 2^256.
/// Off the order path (discovery, once per token).
#[must_use]
pub fn u256_from_decimal(dec: &[u8]) -> Option<[u8; 32]> {
    if dec.is_empty() || dec.len() > 78 {
        return None;
    }
    // Four little-endian 64-bit limbs; × 10 + digit with the carry in u128.
    let mut limb = [0u64; 4];
    let mut i = 0usize;
    while i < dec.len() {
        let c = dec[i];
        if !c.is_ascii_digit() {
            return None;
        }
        let mut carry = u128::from(c - b'0');
        let mut k = 0usize;
        while k < 4 {
            let v = u128::from(limb[k]) * 10 + carry;
            limb[k] = v as u64;
            carry = v >> 64;
            k += 1;
        }
        if carry != 0 {
            return None;
        }
        i += 1;
    }
    let mut out = [0u8; 32];
    let mut k = 0usize;
    while k < 4 {
        // COPY: each limb's 8 big-endian bytes into its quarter of the
        // word — the encoding is the placement; once per token.
        out[(3 - k) * 8..(4 - k) * 8].copy_from_slice(&limb[k].to_be_bytes());
        k += 1;
    }
    Some(out)
}

/// `addr` as its EIP-55 checksummed `0x…` text (42 bytes). Boot-time:
/// the order body carries the configured maker and signer this way.
#[must_use]
pub fn eip55(addr: &[u8; 20]) -> [u8; 42] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut lower = [0u8; 40];
    let mut i = 0usize;
    while i < 20 {
        lower[2 * i] = HEX[(addr[i] >> 4) as usize];
        lower[2 * i + 1] = HEX[(addr[i] & 0x0f) as usize];
        i += 1;
    }
    let h = keccak256(&lower);
    let mut out = [0u8; 42];
    out[0] = b'0';
    out[1] = b'x';
    let mut j = 0usize;
    while j < 40 {
        let nib = (h[j / 2] >> (4 * (1 - (j % 2)))) & 0x0f;
        let c = lower[j];
        out[2 + j] = if c >= b'a' && nib >= 8 { c - 32 } else { c };
        j += 1;
    }
    out
}

/// Sign `digest` with `sk` (a thin re-export of the crate's signer, so
/// the arm needs one import): 65 B `r‖s‖v`, `v` ∈ {27, 28}, low-s,
/// RFC 6979.
///
/// # Errors
/// Never in practice ([`SignError`] is the parse-time error type).
#[inline]
pub fn sign(sk: &crate::SecretKey, digest: &[u8; 32]) -> Result<[u8; 65], SignError> {
    crate::sign_digest_with_key(sk, digest)
}

#[cfg(test)]
mod tests;
