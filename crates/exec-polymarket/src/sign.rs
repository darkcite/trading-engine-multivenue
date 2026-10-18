// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! One order's signature and `orderID` (spec §C): the struct hash once,
//! then — per signature type — the plain digest signed (0, 1, 2) or the
//! Deposit Wallet's `TypedDataSign` digest signed and wrapped (3).

use signer_eip712::pmv2::{
    order_v2_digest, order_v2_struct_hash, poly1271_digest, poly1271_wrap, OrderV2,
    POLY_1271_SIG_LEN, SIG_POLY_1271,
};
use signer_eip712::{SecretKey, SignError};

/// The longest signature an order carries (type 3's wrapper).
pub const SIG_MAX: usize = POLY_1271_SIG_LEN;

/// A signed order's facts: the signature's length in `sig` and the
/// `orderID` the venue will answer with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    /// Bytes of the signature written (65, or 317 for type 3).
    pub sig_len: usize,
    /// The order's EIP-712 digest — its `orderID`.
    pub order_id: [u8; 32],
}

/// Sign `o` under the exchange separator `sep` on `chain_id` with the
/// owner key `sk`, the signature into `sig`.
///
/// # Errors
/// [`SignError`] — never for a key that parsed at boot.
#[inline]
pub fn sign_order(
    sk: &SecretKey,
    sep: &[u8; 32],
    chain_id: u64,
    o: &OrderV2,
    sig: &mut [u8; SIG_MAX],
) -> Result<Signed, SignError> {
    let contents = order_v2_struct_hash(o);
    let order_id = order_v2_digest(sep, &contents);
    if o.signature_type == SIG_POLY_1271 {
        let inner =
            signer_eip712::pmv2::sign(sk, &poly1271_digest(sep, chain_id, &o.signer, &contents))?;
        poly1271_wrap(&inner, sep, &contents, sig);
        return Ok(Signed {
            sig_len: POLY_1271_SIG_LEN,
            order_id,
        });
    }
    let plain = signer_eip712::pmv2::sign(sk, &order_id)?;
    // COPY: the 65 B signature secp256k1 returns by value into the
    // order's signature buffer, the one the body's hex is rendered from.
    sig[..65].copy_from_slice(&plain);
    Ok(Signed {
        sig_len: 65,
        order_id,
    })
}
