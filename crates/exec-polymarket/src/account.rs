// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The account the arm trades (PM15-6b): the signing key, the three
//! addresses the signature type implies (spec §C.3), the CLOB API
//! credentials (§B.1) and the exchange separator — built once at boot from
//! values the wrapper sources (a session never reads `.env`).
//!
//! | type | `maker` | `signer` | `POLY_ADDRESS` |
//! |---|---|---|---|
//! | 0 EOA | the EOA | the EOA | the EOA |
//! | 1 proxy, 2 Safe | the wallet | the EOA | the EOA |
//! | 3 Deposit Wallet | the wallet | the wallet | the EOA |

use signer_eip712::pmv2::{
    eip55, exchange_separator, CHAIN_POLYGON, EXCHANGE_V2, SIG_EOA, SIG_POLY_1271,
};
use signer_eip712::SecretKey;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::l2::L2Secret;
use crate::render::{is_plain_token, API_KEY_MAX, PASSPHRASE_MAX};

/// The longest API secret text kept for the user channel's subscribe
/// (base64 of ≤ 64 bytes).
pub const SECRET_TEXT_MAX: usize = 96;

/// Why an account did not build.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccountErr {
    /// A signature type outside 0..=3.
    SigType,
    /// Type 0 with a wallet, or types 1–3 without one.
    Wallet,
    /// The API key or passphrase is empty, too long, or not plain ASCII.
    Credential,
    /// The secret does not decode, or its text is too long.
    Secret,
}

/// The API secret's text, wiped on drop (it rides the user channel's
/// subscribe raw, spec §F.1).
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretText {
    b: [u8; SECRET_TEXT_MAX],
    len: usize,
}

impl SecretText {
    /// The text.
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..self.len]
    }
}

/// One account (module docs). Boot-built, then read only.
pub struct PmAccount {
    /// The signature type (`pmv2::SIG_*`).
    pub sig_type: u8,
    /// The order's `maker`.
    pub maker: [u8; 20],
    /// The order's `signer`.
    pub signer: [u8; 20],
    /// `maker`, EIP-55.
    pub maker_text: [u8; 42],
    /// `signer`, EIP-55.
    pub signer_text: [u8; 42],
    /// `POLY_ADDRESS` (the owner EOA), EIP-55.
    pub poly_address_text: [u8; 42],
    api_key: [u8; API_KEY_MAX],
    api_key_len: usize,
    passphrase: [u8; PASSPHRASE_MAX],
    passphrase_len: usize,
    /// The decoded HMAC key.
    pub l2: L2Secret,
    /// The secret's text (the user channel).
    pub secret_text: SecretText,
    /// The owner EOA's key.
    pub sk: SecretKey,
    /// The CTF Exchange V2 separator (domain `"2"`, `negRisk:false`).
    pub exchange_sep: [u8; 32],
    /// The chain the separator names.
    pub chain_id: u64,
}

impl PmAccount {
    /// Build the account: `owner` is the EOA `sk` controls; `wallet` the
    /// proxy, Safe or Deposit Wallet (`None` for type 0).
    ///
    /// # Errors
    /// [`AccountErr`] — the arm does not boot.
    pub fn new(
        sig_type: u8,
        owner: [u8; 20],
        wallet: Option<[u8; 20]>,
        api_key: &[u8],
        secret_b64: &[u8],
        passphrase: &[u8],
        sk: SecretKey,
    ) -> Result<Self, AccountErr> {
        if sig_type > SIG_POLY_1271 {
            return Err(AccountErr::SigType);
        }
        let (maker, signer) = match (sig_type, wallet) {
            (SIG_EOA, None) => (owner, owner),
            (SIG_POLY_1271, Some(w)) => (w, w),
            (_, Some(w)) if sig_type != SIG_EOA => (w, owner),
            _ => return Err(AccountErr::Wallet),
        };
        if !is_plain_token(api_key)
            || api_key.len() > API_KEY_MAX
            || !is_plain_token(passphrase)
            || passphrase.len() > PASSPHRASE_MAX
        {
            return Err(AccountErr::Credential);
        }
        if !is_plain_token(secret_b64) || secret_b64.len() > SECRET_TEXT_MAX {
            return Err(AccountErr::Secret);
        }
        let l2 = L2Secret::from_base64(secret_b64).map_err(|_| AccountErr::Secret)?;
        let mut a = Self {
            sig_type,
            maker,
            signer,
            maker_text: eip55(&maker),
            signer_text: eip55(&signer),
            poly_address_text: eip55(&owner),
            api_key: [0; API_KEY_MAX],
            api_key_len: api_key.len(),
            passphrase: [0; PASSPHRASE_MAX],
            passphrase_len: passphrase.len(),
            l2,
            secret_text: SecretText {
                b: [0; SECRET_TEXT_MAX],
                len: secret_b64.len(),
            },
            sk,
            exchange_sep: exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2),
            chain_id: CHAIN_POLYGON,
        };
        // COPY: the credentials' bytes into the account's fixed fields —
        // boot, once.
        a.api_key[..api_key.len()].copy_from_slice(api_key);
        a.passphrase[..passphrase.len()].copy_from_slice(passphrase);
        a.secret_text.b[..secret_b64.len()].copy_from_slice(secret_b64);
        Ok(a)
    }

    /// The API key (`POLY_API_KEY`, the body's `owner`).
    #[inline]
    #[must_use]
    pub fn api_key(&self) -> &[u8] {
        &self.api_key[..self.api_key_len]
    }

    /// The passphrase (`POLY_PASSPHRASE`).
    #[inline]
    #[must_use]
    pub fn passphrase(&self) -> &[u8] {
        &self.passphrase[..self.passphrase_len]
    }
}
