// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hypercall's EIP-712 actions (HC8 — sign-only, no network).
//!
//! Every write to the venue is a JSON body plus an EIP-712 signature over
//! a typed struct in the domain `{name: "Hypercall", version: "1",
//! chainId: 999 (998 testnet), verifyingContract: 0x0}`. The types are
//! the official TS SDK's (`@hypercallxyz/sdk` 0.1.0, `esm/signing/mod.js`)
//! plus the route-less legacy `PlaceOrder` of the venue's auth docs; the
//! known-answer vectors in `tests/fixtures/hypercall/vectors.tsv` were
//! produced by that SDK and ethers, and they — not the docs — are the
//! authority on every byte here (LAW E-3).
//!
//! ## Sign the bytes you send (D7)
//!
//! A `string` field is encoded as the keccak of its bytes, so `"100.0"`
//! and `"100"` are different orders. Every struct hash here therefore
//! takes the field values as BORROWED SPANS — the caller passes the spans
//! of the request body it rendered, and the hash is taken over those very
//! bytes: no second rendering, no staging copy, and a signed string can
//! never diverge from the one on the wire. Addresses are 20 raw bytes (the
//! SDK lower-cases the hex; the encoding is case-free).
//!
//! ## Cost
//!
//! The domain separator is computed once at boot ([`hc_domain_separator`])
//! and passed in; the typehashes are compile-time constants (pinned against
//! `keccak256(type string)` by the tests); each struct hash absorbs its
//! words in place ([`crate::keccak256_parts`]). Zero allocation: every word
//! is a stack array.
//!
//! ## Not decided here
//!
//! * How the body's `rfq_id` / `quote_id` strings map onto the `bytes32`
//!   the RFQ types sign is not in the SDK 0.1.0; the exec arm (HC9) takes
//!   it from the venue before it signs one.
//! * `SetMmpConfig` & co. are named in the docs but not defined by the SDK
//!   0.1.0 (plan §1.5): not here until their field order is known.

use crate::{
    enc_address, enc_bool, enc_i256, enc_string, enc_u64, keccak256_parts, Eip712Domain,
    SignError,
};

/// EIP-712 domain name.
pub const HC_DOMAIN_NAME: &str = "Hypercall";
/// EIP-712 domain version.
pub const HC_DOMAIN_VERSION: &str = "1";
/// HyperEVM mainnet — the venue's chain.
pub const HC_CHAIN_ID_MAINNET: u64 = 999;
/// HyperEVM testnet (the venue's testnet is disabled; kept for the
/// vectors, which pin that the chain id reaches every digest).
pub const HC_CHAIN_ID_TESTNET: u64 = 998;
/// EIP-712 verifying contract: the zero address.
pub const HC_VERIFYING_CONTRACT: [u8; 20] = [0u8; 20];

/// The venue's domain on `chain_id`.
#[must_use]
pub fn hc_domain(chain_id: u64) -> Eip712Domain {
    Eip712Domain::new(
        HC_DOMAIN_NAME.as_bytes(),
        HC_DOMAIN_VERSION.as_bytes(),
        chain_id,
        HC_VERIFYING_CONTRACT,
    )
}

/// The domain separator on `chain_id` — boot-time; the exec arm caches it.
#[must_use]
pub fn hc_domain_separator(chain_id: u64) -> [u8; 32] {
    crate::domain_separator_of(&hc_domain(chain_id))
}

// -----------------------------------------------------------------
// Type strings and their typehashes
// -----------------------------------------------------------------

/// `PlaceOrder`, the live type (with `route`).
pub const PLACE_ORDER_TYPE: &str = "PlaceOrder(address wallet,string symbol,string side,string size,string price,string tif,string route,string clientId,uint64 nonce)";
/// `PlaceOrder`, the legacy type WITHOUT `route` (the venue's auth docs).
pub const PLACE_ORDER_LEGACY_TYPE: &str = "PlaceOrder(address wallet,string symbol,string side,string size,string price,string tif,string clientId,uint64 nonce)";
/// `PlaceOrderReduceOnly` (`reduce_only: true` bodies must use it).
pub const PLACE_ORDER_REDUCE_ONLY_TYPE: &str = "PlaceOrderReduceOnly(address wallet,string symbol,string side,string size,string price,string tif,string route,string clientId,bool reduceOnly,uint64 nonce)";
/// `ReplaceOrder` (an atomic cancel + place).
pub const REPLACE_ORDER_TYPE: &str = "ReplaceOrder(address wallet,string orderId,string symbol,string side,string size,string price,string tif,string clientId,uint64 nonce)";
/// `ReplaceOrderReduceOnly` — the exit path of a reduce-only replace.
pub const REPLACE_ORDER_REDUCE_ONLY_TYPE: &str = "ReplaceOrderReduceOnly(address wallet,string orderId,string symbol,string side,string size,string price,string tif,string clientId,bool reduceOnly,uint64 nonce)";
/// `CancelOrder` (by the venue's order id).
pub const CANCEL_ORDER_TYPE: &str = "CancelOrder(address wallet,string orderId,uint64 nonce)";
/// `CancelOrderByClientId`.
pub const CANCEL_ORDER_BY_CLIENT_ID_TYPE: &str =
    "CancelOrderByClientId(address wallet,string clientId,uint64 nonce)";
/// `ApproveAgent` (signed by the owner).
pub const APPROVE_AGENT_TYPE: &str = "ApproveAgent(address agent,uint64 nonce)";
/// `RevokeAgent`.
pub const REVOKE_AGENT_TYPE: &str = "RevokeAgent(address agent,uint64 nonce)";
/// `RevokeAllAgents`.
pub const REVOKE_ALL_AGENTS_TYPE: &str = "RevokeAllAgents(uint64 nonce)";
/// `SubmitRFQ`.
pub const SUBMIT_RFQ_TYPE: &str =
    "SubmitRFQ(bytes32 rfqId,bytes32 legsHash,address wallet,uint64 nonce)";
/// `SubmitAutoExecuteRfq`.
pub const SUBMIT_AUTO_EXECUTE_RFQ_TYPE: &str = "SubmitAutoExecuteRfq(bytes32 rfqId,bytes32 legsHash,int256 limitPrice,address wallet,uint64 nonce)";
/// `AcceptRFQQuote`.
pub const ACCEPT_RFQ_QUOTE_TYPE: &str = "AcceptRFQQuote(bytes32 rfqId,bytes32 quoteId,int256 netPremium,address wallet,uint64 nonce)";
/// `SetMarginMode`.
pub const SET_MARGIN_MODE_TYPE: &str = "SetMarginMode(address wallet,string marginMode,uint64 nonce)";
/// `WithdrawUsdc`.
pub const WITHDRAW_USDC_TYPE: &str = "WithdrawUsdc(address wallet,address account,address destination,string amount,uint64 nonce)";
/// `SetSettlementPayoutSeen`.
pub const SET_SETTLEMENT_PAYOUT_SEEN_TYPE: &str =
    "SetSettlementPayoutSeen(address wallet,string payoutIds,bool seen,uint64 nonce)";
/// `CreateReferralCode`.
pub const CREATE_REFERRAL_CODE_TYPE: &str =
    "CreateReferralCode(address wallet,string code,uint64 nonce)";
/// `SetReferrer`.
pub const SET_REFERRER_TYPE: &str = "SetReferrer(address wallet,address referrer,uint64 nonce)";
/// `StandardMarginLiquidationOrder` (a liquidation-auction bid).
pub const STANDARD_MARGIN_LIQUIDATION_ORDER_TYPE: &str = "StandardMarginLiquidationOrder(address wallet,address liquidatedWallet,string requestId,string auctionId,string bidUsdc,string portfolioHash,string auctionTermsHash,string bidIntentHash,uint64 auctionVersion,uint64 nonce)";

/// 64 hex digits → 32 bytes, at compile time (a bad digit fails the
/// build).
const fn hex32(s: &[u8; 64]) -> [u8; 32] {
    const fn nib(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("hex32: not a lower-case hex digit"),
        }
    }
    let mut out = [0u8; 32];
    let mut i = 0usize;
    while i < 32 {
        out[i] = (nib(s[2 * i]) << 4) | nib(s[2 * i + 1]);
        i += 1;
    }
    out
}

/// `keccak256(PLACE_ORDER_TYPE)`.
pub const PLACE_ORDER_TYPEHASH: [u8; 32] =
    hex32(b"67b2d8cea8c1dacf00ab377841dc28ad9d4bd6da70c11e29f41915946425d4a8");
/// `keccak256(PLACE_ORDER_LEGACY_TYPE)`.
pub const PLACE_ORDER_LEGACY_TYPEHASH: [u8; 32] =
    hex32(b"eb051615d85124dc102e6b50a89ee7fc080ccb0e98b135239d75821e7a546843");
/// `keccak256(PLACE_ORDER_REDUCE_ONLY_TYPE)`.
pub const PLACE_ORDER_REDUCE_ONLY_TYPEHASH: [u8; 32] =
    hex32(b"893377b3a40665304ce854fb754dc68edd0ffb55999427f5ef89a3c918ab8c40");
/// `keccak256(REPLACE_ORDER_TYPE)`.
pub const REPLACE_ORDER_TYPEHASH: [u8; 32] =
    hex32(b"15ac21dd10f259042a85274c7d7efdf5aedd6c40d41450a0a630efdac6e5959a");
/// `keccak256(REPLACE_ORDER_REDUCE_ONLY_TYPE)`.
pub const REPLACE_ORDER_REDUCE_ONLY_TYPEHASH: [u8; 32] =
    hex32(b"1eb6a6233e182a9c6225a1723a76552f6dea3ea96b3694914cbeeee77f9c5143");
/// `keccak256(CANCEL_ORDER_TYPE)`.
pub const CANCEL_ORDER_TYPEHASH: [u8; 32] =
    hex32(b"b78b489db0405733815361ceeb0fd4cf14ec6684bca70570cc6904c88a5446a8");
/// `keccak256(CANCEL_ORDER_BY_CLIENT_ID_TYPE)`.
pub const CANCEL_ORDER_BY_CLIENT_ID_TYPEHASH: [u8; 32] =
    hex32(b"90bacd0d18a36a0e28a4bf2e7964e294566bf430833323aa796a68a586fe5866");
/// `keccak256(APPROVE_AGENT_TYPE)`.
pub const APPROVE_AGENT_TYPEHASH: [u8; 32] =
    hex32(b"ebecfb614a9ccffbf0a0ecfa93286461f5ee3ff700eb72546a0e969d9f86ee5b");
/// `keccak256(REVOKE_AGENT_TYPE)`.
pub const REVOKE_AGENT_TYPEHASH: [u8; 32] =
    hex32(b"1f87f6c85cee2e118ffce50ceceda8a1936a0453f76eb4bdcfe947a5399c04b5");
/// `keccak256(REVOKE_ALL_AGENTS_TYPE)`.
pub const REVOKE_ALL_AGENTS_TYPEHASH: [u8; 32] =
    hex32(b"e3f8bb1cf6eaa85a54d8ab1b21244f57b146f0c898d729fa3848a11248257dce");
/// `keccak256(SUBMIT_RFQ_TYPE)`.
pub const SUBMIT_RFQ_TYPEHASH: [u8; 32] =
    hex32(b"18439cdb0f792814c1a518e126f051cde9eb34302a1b9ef1d132ea4ab947612c");
/// `keccak256(SUBMIT_AUTO_EXECUTE_RFQ_TYPE)`.
pub const SUBMIT_AUTO_EXECUTE_RFQ_TYPEHASH: [u8; 32] =
    hex32(b"75b9b4582a3c20502dbafde57cf7c28a752d39bba3a5cef1da79b745052e3c65");
/// `keccak256(ACCEPT_RFQ_QUOTE_TYPE)`.
pub const ACCEPT_RFQ_QUOTE_TYPEHASH: [u8; 32] =
    hex32(b"d8cde7b055554a2f7b9cc3e675e4c63f4735085becaa31bda0836c2d54a4d17c");
/// `keccak256(SET_MARGIN_MODE_TYPE)`.
pub const SET_MARGIN_MODE_TYPEHASH: [u8; 32] =
    hex32(b"5938b9ef87c5e43e37f61b2d0b19533a8a29be6c6e3c2103ab4a1169cc6d58d8");
/// `keccak256(WITHDRAW_USDC_TYPE)`.
pub const WITHDRAW_USDC_TYPEHASH: [u8; 32] =
    hex32(b"b76f1e3cf4083010f94c95758f1300453b52490ce203567b8c462c3dc76109df");
/// `keccak256(SET_SETTLEMENT_PAYOUT_SEEN_TYPE)`.
pub const SET_SETTLEMENT_PAYOUT_SEEN_TYPEHASH: [u8; 32] =
    hex32(b"a016874ba2f37bd9c6dda1d4f0ae99d2badceee1bd90a9924e67fc68d750b639");
/// `keccak256(CREATE_REFERRAL_CODE_TYPE)`.
pub const CREATE_REFERRAL_CODE_TYPEHASH: [u8; 32] =
    hex32(b"9e71d0261efa2e28a602e0164f32e6ddd69e94a34bbc8b0936c156aa6e932f99");
/// `keccak256(SET_REFERRER_TYPE)`.
pub const SET_REFERRER_TYPEHASH: [u8; 32] =
    hex32(b"b5d87af41e32afca45d43a278ca2bde063b64c92da1f12107560bf9d63c43e3b");
/// `keccak256(STANDARD_MARGIN_LIQUIDATION_ORDER_TYPE)`.
pub const STANDARD_MARGIN_LIQUIDATION_ORDER_TYPEHASH: [u8; 32] =
    hex32(b"461f2e9cb2230b0f1a0acb9b14079c5c10bed538e1bb0fe1df6e9cb4d6e5aab4");

// -----------------------------------------------------------------
// The views: the request body's spans
// -----------------------------------------------------------------

/// An order placement's fields, as the request body carries them (D7).
/// `route` is ignored by the legacy type.
#[derive(Copy, Clone, Debug)]
pub struct HcPlaceView<'a> {
    /// The signing wallet.
    pub wallet: &'a [u8; 20],
    /// `<UND>-<YYYYMMDD>-<STRIKE>-<C|P>`.
    pub symbol: &'a [u8],
    /// `Buy` | `Sell`.
    pub side: &'a [u8],
    /// Decimal string, exactly as sent.
    pub size: &'a [u8],
    /// Decimal string, exactly as sent.
    pub price: &'a [u8],
    /// `gtc` | `ioc` | `fok`.
    pub tif: &'a [u8],
    /// `best_execution` | `book_only` | `rfq_only`.
    pub route: &'a [u8],
    /// The client order id (empty when none).
    pub client_id: &'a [u8],
    /// Unique per wallet.
    pub nonce: u64,
}

/// A replace's fields, as the request body carries them (D7).
#[derive(Copy, Clone, Debug)]
pub struct HcReplaceView<'a> {
    /// The signing wallet.
    pub wallet: &'a [u8; 20],
    /// The venue's order id, as a decimal string.
    pub order_id: &'a [u8],
    /// The instrument.
    pub symbol: &'a [u8],
    /// `Buy` | `Sell`.
    pub side: &'a [u8],
    /// Decimal string, exactly as sent.
    pub size: &'a [u8],
    /// Decimal string, exactly as sent.
    pub price: &'a [u8],
    /// `gtc` | `ioc` | `fok`.
    pub tif: &'a [u8],
    /// The client order id (empty when none).
    pub client_id: &'a [u8],
    /// Unique per wallet.
    pub nonce: u64,
}

/// A liquidation-auction bid's fields, as the request body carries them.
#[derive(Copy, Clone, Debug)]
pub struct HcLiquidationView<'a> {
    /// The bidding wallet.
    pub wallet: &'a [u8; 20],
    /// The wallet being liquidated.
    pub liquidated_wallet: &'a [u8; 20],
    /// The request id.
    pub request_id: &'a [u8],
    /// The auction id.
    pub auction_id: &'a [u8],
    /// The bid, a decimal USDC string.
    pub bid_usdc: &'a [u8],
    /// The portfolio hash, as a string.
    pub portfolio_hash: &'a [u8],
    /// The auction-terms hash, as a string.
    pub auction_terms_hash: &'a [u8],
    /// The bid-intent hash, as a string.
    pub bid_intent_hash: &'a [u8],
    /// The auction version.
    pub auction_version: u64,
    /// Unique per wallet.
    pub nonce: u64,
}

// -----------------------------------------------------------------
// Struct hashes — each word absorbed in place
// -----------------------------------------------------------------

/// `hashStruct(PlaceOrder)`, the live type.
#[must_use]
pub fn place_order_struct_hash(v: &HcPlaceView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &PLACE_ORDER_TYPEHASH,
        &enc_address(v.wallet),
        &enc_string(v.symbol),
        &enc_string(v.side),
        &enc_string(v.size),
        &enc_string(v.price),
        &enc_string(v.tif),
        &enc_string(v.route),
        &enc_string(v.client_id),
        &enc_u64(v.nonce),
    ])
}

/// `hashStruct(PlaceOrder)`, the legacy route-less type (`v.route` is
/// not signed).
#[must_use]
pub fn place_order_legacy_struct_hash(v: &HcPlaceView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &PLACE_ORDER_LEGACY_TYPEHASH,
        &enc_address(v.wallet),
        &enc_string(v.symbol),
        &enc_string(v.side),
        &enc_string(v.size),
        &enc_string(v.price),
        &enc_string(v.tif),
        &enc_string(v.client_id),
        &enc_u64(v.nonce),
    ])
}

/// `hashStruct(PlaceOrderReduceOnly)`: the type IS the flag, so
/// `reduceOnly` is signed `true`, as the SDK always does.
#[must_use]
pub fn place_order_reduce_only_struct_hash(v: &HcPlaceView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &PLACE_ORDER_REDUCE_ONLY_TYPEHASH,
        &enc_address(v.wallet),
        &enc_string(v.symbol),
        &enc_string(v.side),
        &enc_string(v.size),
        &enc_string(v.price),
        &enc_string(v.tif),
        &enc_string(v.route),
        &enc_string(v.client_id),
        &enc_bool(true),
        &enc_u64(v.nonce),
    ])
}

/// `hashStruct(ReplaceOrder)`.
#[must_use]
pub fn replace_order_struct_hash(v: &HcReplaceView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &REPLACE_ORDER_TYPEHASH,
        &enc_address(v.wallet),
        &enc_string(v.order_id),
        &enc_string(v.symbol),
        &enc_string(v.side),
        &enc_string(v.size),
        &enc_string(v.price),
        &enc_string(v.tif),
        &enc_string(v.client_id),
        &enc_u64(v.nonce),
    ])
}

/// `hashStruct(ReplaceOrderReduceOnly)`: `reduceOnly` signed `true`.
#[must_use]
pub fn replace_order_reduce_only_struct_hash(v: &HcReplaceView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &REPLACE_ORDER_REDUCE_ONLY_TYPEHASH,
        &enc_address(v.wallet),
        &enc_string(v.order_id),
        &enc_string(v.symbol),
        &enc_string(v.side),
        &enc_string(v.size),
        &enc_string(v.price),
        &enc_string(v.tif),
        &enc_string(v.client_id),
        &enc_bool(true),
        &enc_u64(v.nonce),
    ])
}

/// `hashStruct(CancelOrder)`.
#[must_use]
pub fn cancel_order_struct_hash(wallet: &[u8; 20], order_id: &[u8], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[
        &CANCEL_ORDER_TYPEHASH,
        &enc_address(wallet),
        &enc_string(order_id),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(CancelOrderByClientId)`.
#[must_use]
pub fn cancel_order_by_client_id_struct_hash(
    wallet: &[u8; 20],
    client_id: &[u8],
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &CANCEL_ORDER_BY_CLIENT_ID_TYPEHASH,
        &enc_address(wallet),
        &enc_string(client_id),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(ApproveAgent)` — signed by the OWNER, never the agent.
#[must_use]
pub fn approve_agent_struct_hash(agent: &[u8; 20], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[&APPROVE_AGENT_TYPEHASH, &enc_address(agent), &enc_u64(nonce)])
}

/// `hashStruct(RevokeAgent)`.
#[must_use]
pub fn revoke_agent_struct_hash(agent: &[u8; 20], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[&REVOKE_AGENT_TYPEHASH, &enc_address(agent), &enc_u64(nonce)])
}

/// `hashStruct(RevokeAllAgents)`.
#[must_use]
pub fn revoke_all_agents_struct_hash(nonce: u64) -> [u8; 32] {
    keccak256_parts(&[&REVOKE_ALL_AGENTS_TYPEHASH, &enc_u64(nonce)])
}

/// `hashStruct(SubmitRFQ)`; the `bytes32` words are absorbed verbatim.
#[must_use]
pub fn submit_rfq_struct_hash(
    rfq_id: &[u8; 32],
    legs_hash: &[u8; 32],
    wallet: &[u8; 20],
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &SUBMIT_RFQ_TYPEHASH,
        rfq_id,
        legs_hash,
        &enc_address(wallet),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(SubmitAutoExecuteRfq)`; `limit_price` sign-extended.
#[must_use]
pub fn submit_auto_execute_rfq_struct_hash(
    rfq_id: &[u8; 32],
    legs_hash: &[u8; 32],
    limit_price: i128,
    wallet: &[u8; 20],
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &SUBMIT_AUTO_EXECUTE_RFQ_TYPEHASH,
        rfq_id,
        legs_hash,
        &enc_i256(limit_price),
        &enc_address(wallet),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(AcceptRFQQuote)`; `net_premium` sign-extended.
#[must_use]
pub fn accept_rfq_quote_struct_hash(
    rfq_id: &[u8; 32],
    quote_id: &[u8; 32],
    net_premium: i128,
    wallet: &[u8; 20],
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &ACCEPT_RFQ_QUOTE_TYPEHASH,
        rfq_id,
        quote_id,
        &enc_i256(net_premium),
        &enc_address(wallet),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(SetMarginMode)` (`standard` | `portfolio`).
#[must_use]
pub fn set_margin_mode_struct_hash(wallet: &[u8; 20], margin_mode: &[u8], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[
        &SET_MARGIN_MODE_TYPEHASH,
        &enc_address(wallet),
        &enc_string(margin_mode),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(WithdrawUsdc)`; `amount` a decimal string.
#[must_use]
pub fn withdraw_usdc_struct_hash(
    wallet: &[u8; 20],
    account: &[u8; 20],
    destination: &[u8; 20],
    amount: &[u8],
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &WITHDRAW_USDC_TYPEHASH,
        &enc_address(wallet),
        &enc_address(account),
        &enc_address(destination),
        &enc_string(amount),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(SetSettlementPayoutSeen)`; `payout_ids` is the SDK's
/// canonical form, the ids in decimal joined by `,`.
#[must_use]
pub fn set_settlement_payout_seen_struct_hash(
    wallet: &[u8; 20],
    payout_ids: &[u8],
    seen: bool,
    nonce: u64,
) -> [u8; 32] {
    keccak256_parts(&[
        &SET_SETTLEMENT_PAYOUT_SEEN_TYPEHASH,
        &enc_address(wallet),
        &enc_string(payout_ids),
        &enc_bool(seen),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(CreateReferralCode)`; `code` exactly as sent (the venue
/// authenticates this string before it normalises the stored code).
#[must_use]
pub fn create_referral_code_struct_hash(wallet: &[u8; 20], code: &[u8], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[
        &CREATE_REFERRAL_CODE_TYPEHASH,
        &enc_address(wallet),
        &enc_string(code),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(SetReferrer)`.
#[must_use]
pub fn set_referrer_struct_hash(wallet: &[u8; 20], referrer: &[u8; 20], nonce: u64) -> [u8; 32] {
    keccak256_parts(&[
        &SET_REFERRER_TYPEHASH,
        &enc_address(wallet),
        &enc_address(referrer),
        &enc_u64(nonce),
    ])
}

/// `hashStruct(StandardMarginLiquidationOrder)`.
#[must_use]
pub fn standard_margin_liquidation_order_struct_hash(v: &HcLiquidationView<'_>) -> [u8; 32] {
    keccak256_parts(&[
        &STANDARD_MARGIN_LIQUIDATION_ORDER_TYPEHASH,
        &enc_address(v.wallet),
        &enc_address(v.liquidated_wallet),
        &enc_string(v.request_id),
        &enc_string(v.auction_id),
        &enc_string(v.bid_usdc),
        &enc_string(v.portfolio_hash),
        &enc_string(v.auction_terms_hash),
        &enc_string(v.bid_intent_hash),
        &enc_u64(v.auction_version),
        &enc_u64(v.nonce),
    ])
}

// -----------------------------------------------------------------
// Digest and signature
// -----------------------------------------------------------------

/// The digest a Hypercall signature covers:
/// `keccak256(0x19 0x01 ‖ domain_separator ‖ struct_hash)`.
#[inline]
#[must_use]
pub fn hc_eip712_digest(domain_separator: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    crate::eip712_digest(domain_separator, struct_hash)
}

/// Sign any struct hash above: the 65-byte `r‖s‖v` over its digest. The
/// caller holds the parsed key and the cached separator from boot.
///
/// # Errors
///
/// None in practice: a parsed [`secp256k1::SecretKey`] always signs; the
/// `Result` is the crate's signing signature.
#[inline]
pub fn sign_hc_with_key(
    sk: &secp256k1::SecretKey,
    domain_separator: &[u8; 32],
    struct_hash: &[u8; 32],
) -> Result<[u8; 65], SignError> {
    crate::sign_digest_with_key(sk, &hc_eip712_digest(domain_separator, struct_hash))
}

/// The hot path's one-shot: a live `PlaceOrder`'s signature.
///
/// # Errors
///
/// As [`sign_hc_with_key`].
#[inline]
pub fn sign_place_order_with_key(
    sk: &secp256k1::SecretKey,
    domain_separator: &[u8; 32],
    v: &HcPlaceView<'_>,
) -> Result<[u8; 65], SignError> {
    sign_hc_with_key(sk, domain_separator, &place_order_struct_hash(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keccak256;

    /// Every const typehash IS the keccak of its type string — the
    /// compile-time constants cannot drift from the strings they name.
    #[test]
    fn every_typehash_is_the_keccak_of_its_type_string() {
        let pairs: [(&str, [u8; 32]); 19] = [
            (PLACE_ORDER_TYPE, PLACE_ORDER_TYPEHASH),
            (PLACE_ORDER_LEGACY_TYPE, PLACE_ORDER_LEGACY_TYPEHASH),
            (PLACE_ORDER_REDUCE_ONLY_TYPE, PLACE_ORDER_REDUCE_ONLY_TYPEHASH),
            (REPLACE_ORDER_TYPE, REPLACE_ORDER_TYPEHASH),
            (REPLACE_ORDER_REDUCE_ONLY_TYPE, REPLACE_ORDER_REDUCE_ONLY_TYPEHASH),
            (CANCEL_ORDER_TYPE, CANCEL_ORDER_TYPEHASH),
            (CANCEL_ORDER_BY_CLIENT_ID_TYPE, CANCEL_ORDER_BY_CLIENT_ID_TYPEHASH),
            (APPROVE_AGENT_TYPE, APPROVE_AGENT_TYPEHASH),
            (REVOKE_AGENT_TYPE, REVOKE_AGENT_TYPEHASH),
            (REVOKE_ALL_AGENTS_TYPE, REVOKE_ALL_AGENTS_TYPEHASH),
            (SUBMIT_RFQ_TYPE, SUBMIT_RFQ_TYPEHASH),
            (SUBMIT_AUTO_EXECUTE_RFQ_TYPE, SUBMIT_AUTO_EXECUTE_RFQ_TYPEHASH),
            (ACCEPT_RFQ_QUOTE_TYPE, ACCEPT_RFQ_QUOTE_TYPEHASH),
            (SET_MARGIN_MODE_TYPE, SET_MARGIN_MODE_TYPEHASH),
            (WITHDRAW_USDC_TYPE, WITHDRAW_USDC_TYPEHASH),
            (SET_SETTLEMENT_PAYOUT_SEEN_TYPE, SET_SETTLEMENT_PAYOUT_SEEN_TYPEHASH),
            (CREATE_REFERRAL_CODE_TYPE, CREATE_REFERRAL_CODE_TYPEHASH),
            (SET_REFERRER_TYPE, SET_REFERRER_TYPEHASH),
            (STANDARD_MARGIN_LIQUIDATION_ORDER_TYPE, STANDARD_MARGIN_LIQUIDATION_ORDER_TYPEHASH),
        ];
        let mut i = 0usize;
        while i < pairs.len() {
            assert_eq!(keccak256(pairs[i].0.as_bytes()), pairs[i].1, "{}", pairs[i].0);
            let mut j = i + 1;
            while j < pairs.len() {
                assert_ne!(pairs[i].1, pairs[j].1, "two types share a hash");
                j += 1;
            }
            i += 1;
        }
    }

    /// The venue's separators, pinned — the chain id reaches them, and
    /// they are neither Polymarket's nor Hyperliquid's.
    #[test]
    fn the_domain_is_the_venues_on_each_chain() {
        let main = hc_domain_separator(HC_CHAIN_ID_MAINNET);
        assert_eq!(main, hex32(b"fe7c43fd5c8de2e81ca734eeebe8bfd393d8bd29da7d4f2993a02b03a78c0588"));
        assert_eq!(
            hc_domain_separator(HC_CHAIN_ID_TESTNET),
            hex32(b"0b9459ea657d0024f9b92b5eaa58782ea66e1b8773f9b847edbfcd893d169316")
        );
        assert_ne!(main, crate::domain_separator());
        assert_ne!(main, crate::hyperliquid::hl_domain_separator());
    }

    /// D7: a string is signed as its bytes — `"100.0"` and `"100"` are
    /// different orders, and every field of the view reaches the hash.
    #[test]
    fn the_signed_strings_are_the_bytes_sent() {
        let w = [7u8; 20];
        let base = HcPlaceView {
            wallet: &w,
            symbol: b"SP500-20261002-7730-P",
            side: b"Sell",
            size: b"1",
            price: b"100.0",
            tif: b"gtc",
            route: b"best_execution",
            client_id: b"c1",
            nonce: 42,
        };
        let h = place_order_struct_hash(&base);
        let other = HcPlaceView { price: b"100", ..base };
        assert_ne!(h, place_order_struct_hash(&other));
        let routed = HcPlaceView { route: b"book_only", ..base };
        assert_ne!(h, place_order_struct_hash(&routed), "route is signed");
        assert_eq!(
            place_order_legacy_struct_hash(&base),
            place_order_legacy_struct_hash(&routed),
            "the legacy type does not sign the route"
        );
        assert_ne!(h, place_order_reduce_only_struct_hash(&base));
    }
}
