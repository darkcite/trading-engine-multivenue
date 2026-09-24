// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The non-zero start of the in-place parser contract. A failed parse
//! must leave its frame untouched; a target that starts from `ZERO`
//! cannot tell "untouched" from "zeroed on the way out", one that starts
//! from a `0xA5`-filled frame can.

/// Plain-integer POD: every bit pattern is a valid `Self`.
///
/// # Safety
///
/// Implement only for a `repr(C)` struct whose every field is an
/// integer, an integer alias or newtype (`NsTs`, `SymbolId`, `Price`,
/// `Qty`) or an integer array — no `bool`, enum, reference, pointer,
/// `NonZero*`, cell or safety invariant. `Copy` rules out drop glue.
pub unsafe trait AnyBits: Copy {}

// SAFETY: each frame below was checked field by field against its
// definition (2026-09-23): `repr(C)`, integers and `[u8; N]` only.
// `DeribitVolIndexFrame` re-checked 2026-09-24 after its index name
// became a payload span: `ts_ns: NsTs`, `vol_1e9: i64`,
// `index_name_off: u32`, `index_name_len: u8`, `_pad: [u8; 43]`.
unsafe impl AnyBits for core_types::Tick {}
unsafe impl AnyBits for ingress_okx::OkxBboFrame {}
unsafe impl AnyBits for ingress_okx::OkxTradeFrame {}
unsafe impl AnyBits for ingress_okx::OkxMarkPriceFrame {}
unsafe impl AnyBits for ingress_okx::OkxFundingFrame {}
unsafe impl AnyBits for ingress_okx::OkxBookFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitQuoteFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitOptTickerFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitVolIndexFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitTickerFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitTradeFrame {}
unsafe impl AnyBits for ingress_deribit::DeribitBookFrame {}
unsafe impl AnyBits for ingress_hyperliquid::HlBboFrame {}
unsafe impl AnyBits for ingress_hyperliquid::HlL2BookFrame {}
unsafe impl AnyBits for ingress_hyperliquid::HlTradeFrame {}
unsafe impl AnyBits for ingress_hyperliquid::HlAssetCtxFrame {}
unsafe impl AnyBits for ingress_hyperliquid::HlOutcomeMetaFrame {}
unsafe impl AnyBits for ingress_mexc::spot::MexcBookTicker {}
unsafe impl AnyBits for ingress_mexc::spot::MexcSpotAck {}
unsafe impl AnyBits for ingress_mexc::MexcDeal {}
unsafe impl AnyBits for ingress_mexc::futures::MexcDepthFrame {}
unsafe impl AnyBits for ingress_mexc::futures::MexcTickerFrame {}
unsafe impl AnyBits for ingress_bybit::BybitBookFrame {}
unsafe impl AnyBits for ingress_bybit::BybitTradeFrame {}
unsafe impl AnyBits for ingress_bybit::BybitTickerFrame {}
unsafe impl AnyBits for ingress_rpc::NewHead {}
unsafe impl AnyBits for ingress_binance::BookTickerFrame {}
unsafe impl AnyBits for ingress_binance::BnMarkPriceFrame {}

/// A `T` with every byte `0xA5`.
pub fn poisoned<T: AnyBits>() -> T {
    // Tripwire, not the proof (that is the `AnyBits` contract): a field
    // the compiler knows an invalid value for gives `T` a niche, and
    // `Option<T>` then fits in `T`'s own size.
    const {
        assert!(
            core::mem::size_of::<Option<T>>() > core::mem::size_of::<T>(),
            "poisoned::<T>: T has a niche, so it is not plain-integer POD"
        )
    };
    let mut v = core::mem::MaybeUninit::<T>::uninit();
    // SAFETY: `T: AnyBits` — every bit pattern is a valid `T`, so the
    // filled bytes form one.
    unsafe {
        core::ptr::write_bytes(v.as_mut_ptr(), 0xA5, 1);
        v.assume_init()
    }
}
