// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The `POST /order` body, rendered as **two spans**:
//!
//! ```text
//! [prefix]  per token, at the roll  {"order":{"maker":…,"signer":…,"tokenId":…,"side":"BUY",…,"salt":
//! [tail]    per order               <salt>,"makerAmount":…,"takerAmount":…,"timestamp":…,"signature":…},"owner":…,"orderType":"FAK",…}
//! ```
//!
//! PM15-6b: the gateway lays them side by side in `core_net::HttpsReq`'s
//! body window (the prefix copied, the tail rendered in place after it),
//! and the client renders the head — the `POLY_*` lines ride its extra
//! headers — flush against them: one contiguous write, one TLS record.
//!
//! The venue parses the body as JSON, so the key order is ours (the three
//! official SDKs use three different orders, spec §A.1): ours puts every
//! per-token field first, so a token's prefix is rendered once at its roll
//! and an order renders only its tail. The body's bytes are `prefix ‖
//! tail`; the L2 MAC absorbs exactly those spans ([`crate::l2`]).

use crate::amounts::Amounts;
use crate::num::{put, put_hex, put_u64};

/// A token prefix's capacity (the longest is 422 B: below).
pub const PREFIX_MAX: usize = 448;

/// An order tail's capacity (the longest is 906 B: below).
pub const TAIL_MAX: usize = 1024;

/// The longest `owner` / `POLY_API_KEY` (a UUID is 36 B).
pub const API_KEY_MAX: usize = 64;

/// The longest `POLY_PASSPHRASE`.
pub const PASSPHRASE_MAX: usize = 128;

const ZERO32_HEX: &[u8; 66] = b"0x0000000000000000000000000000000000000000000000000000000000000000";

/// Whether `v` may stand in a header value and a JSON string unescaped:
/// non-empty printable ASCII, no `"` and no `\` (boot-time check of the
/// API key and passphrase).
#[must_use]
pub fn is_plain_token(v: &[u8]) -> bool {
    if v.is_empty() {
        return false;
    }
    let mut i = 0usize;
    while i < v.len() {
        let c = v[i];
        if !(0x21..=0x7e).contains(&c) || c == b'"' || c == b'\\' {
            return false;
        }
        i += 1;
    }
    true
}

/// Unix seconds as the `POLY_TIMESTAMP` digits, into `out`; returns
/// their length.
#[inline]
pub fn render_ts(ts_s: u64, out: &mut [u8; 20]) -> usize {
    put_u64(out, 0, ts_s)
}

/// A token's body prefix: every field fixed for (account, token, BUY).
/// Rendered once per token, when its market rolls in.
pub fn render_token_prefix(
    maker_text: &[u8; 42],
    signer_text: &[u8; 42],
    token_dec: &[u8],
    signature_type: u8,
    out: &mut [u8; PREFIX_MAX],
) -> usize {
    debug_assert!(!token_dec.is_empty() && token_dec.len() <= 78);
    let mut at = put(out, 0, br#"{"order":{"maker":""#);
    at = put(out, at, maker_text);
    at = put(out, at, br#"","signer":""#);
    at = put(out, at, signer_text);
    at = put(out, at, br#"","tokenId":""#);
    at = put(out, at, token_dec);
    at = put(
        out,
        at,
        br#"","side":"BUY","expiration":"0","signatureType":"#,
    );
    at = put_u64(out, at, u64::from(signature_type));
    at = put(out, at, br#","metadata":""#);
    at = put(out, at, ZERO32_HEX);
    at = put(out, at, br#"","builder":""#);
    at = put(out, at, ZERO32_HEX);
    put(out, at, br#"","salt":"#)
}

/// One order's tail: its salt, amounts, timestamp and signature, then the
/// envelope (`owner` = the API key; `FAK`; no deferral; not post-only).
pub fn render_order_tail(
    salt: u64,
    a: &Amounts,
    ts_ms: u64,
    signature: &[u8],
    api_key: &[u8],
    out: &mut [u8; TAIL_MAX],
) -> usize {
    debug_assert!(salt < (1u64 << 53), "salt rides the JSON as a number");
    let mut at = put_u64(out, 0, salt);
    at = put(out, at, br#","makerAmount":""#);
    at = put_u64(out, at, a.maker);
    at = put(out, at, br#"","takerAmount":""#);
    at = put_u64(out, at, a.taker);
    at = put(out, at, br#"","timestamp":""#);
    at = put_u64(out, at, ts_ms);
    at = put(out, at, br#"","signature":"0x"#);
    at = put_hex(out, at, signature);
    at = put(out, at, br#""},"owner":""#);
    at = put(out, at, api_key);
    at = put(
        out,
        at,
        br#"","orderType":"FAK","deferExec":false,"postOnly":false}"#,
    );
    at
}

// Sizing, const-asserted. The longest prefix: two 42 B addresses, a
// 78-digit token id, two 66 B zero words, one type digit and 127 B of
// fragments — 422 B. The longest tail: a 16-digit salt, two 20-digit
// amounts, a 20-digit timestamp, type 3's 634 hex digits, a 64 B key and
// 132 B of fragments — 906 B.
const _: () = assert!(2 * 42 + 78 + 2 * 66 + 1 + 127 <= PREFIX_MAX);
const _: () = assert!(16 + 2 * 20 + 20 + 2 * crate::sign::SIG_MAX + API_KEY_MAX + 132 <= TAIL_MAX);

#[cfg(test)]
mod tests;
