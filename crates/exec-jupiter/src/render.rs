// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # render — the Prediction API's requests, written into their final buffers
//! (PMJ J3)
//!
//! Each renderer writes the request body (or path) straight into the
//! caller's buffer — the HTTPS request's own body window — and returns its
//! length; `None` when the buffer cannot hold it (a programmer's size error:
//! every caller's buffer is sized by the `*_MAX` constants here).
//!
//! | request | body |
//! |---|---|
//! | `POST /orders` | `{"ownerPubkey":"…","marketId":"POLY-<id>-<o>","isYes":true,"isBuy":true,"depositAmount":"<micro>","depositMint":"…"}` |
//! | `POST /execute` | `{"signedTransaction":"<base64>","context":<the build's object>,"requestId":"<oid>"}` |
//! | `POST /positions/{pk}/claim` | `{"ownerPubkey":"…"}` |
//!
//! **The market id** (to verify at J0): Jupiter names Polymarket's binary
//! up/down outcome `o` of Gamma market `id` as `POLY-<id>-<o>`, `o` the
//! index into the market's `clobTokenIds` (0 = Up, 1 = Down, the PM15 law's
//! order), and a buy of that outcome is `isYes: true`.
//!
//! The base64 of the signed transaction is encoded straight into the body
//! (`core_crypto::base64_encode`); the context object is the one bounded
//! span copy the API forces (it must travel back verbatim).

use core_crypto::{base64_encoded_len, base64_encode};

use crate::tx::TX_MAX;

/// The longest base58 key (a 32-byte key is at most 44 characters).
pub const B58_KEY_MAX: usize = 44;
/// The longest `execution.context` object carried back.
pub const CONTEXT_MAX: usize = 2_048;
/// A `POST /orders` body's bound.
pub const ORDER_BODY_MAX: usize = 256;
/// A `POST /execute` body's bound.
pub const EXECUTE_BODY_MAX: usize = 64 + base64_encoded_len(TX_MAX) + CONTEXT_MAX + 32;
/// A claim body's bound.
pub const CLAIM_BODY_MAX: usize = 32 + B58_KEY_MAX;
/// A request path's bound.
pub const PATH_MAX: usize = 96;

/// The outcome Jupiter's market id names (module docs).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `clobTokenIds[0]`, Up.
    Up = 0,
    /// `clobTokenIds[1]`, Down.
    Down = 1,
}

struct W<'a> {
    dst: &'a mut [u8],
    n: usize,
    ok: bool,
}

impl W<'_> {
    #[inline]
    fn put(&mut self, b: &[u8]) {
        if !self.ok || self.n + b.len() > self.dst.len() {
            self.ok = false;
            return;
        }
        // COPY: a request part into the request's final body buffer (the
        // render itself; parts ≤ the context bound) — rejected: none, this
        // is the one write each wire byte gets.
        self.dst[self.n..self.n + b.len()].copy_from_slice(b);
        self.n += b.len();
    }

    #[inline]
    fn dec(&mut self, v: u64) {
        let mut d = [0u8; 20];
        let mut x = v;
        let mut k = 0usize;
        loop {
            d[19 - k] = b'0' + (x % 10) as u8;
            k += 1;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        self.put(&d[20 - k..]);
    }

    fn done(self) -> Option<usize> {
        if self.ok {
            Some(self.n)
        } else {
            None
        }
    }
}

/// The `POST /orders` body for a BUY of `outcome` of Gamma market
/// `market_id`, depositing `deposit_micro` units of `mint_b58` from
/// `owner_b58`'s account.
#[must_use]
pub fn order_body(
    dst: &mut [u8],
    owner_b58: &[u8],
    market_id: u64,
    outcome: Outcome,
    deposit_micro: u64,
    mint_b58: &[u8],
) -> Option<usize> {
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"ownerPubkey\":\"");
    w.put(owner_b58);
    w.put(b"\",\"marketId\":\"POLY-");
    w.dec(market_id);
    w.put(if outcome == Outcome::Up { b"-0" } else { b"-1" });
    w.put(b"\",\"isYes\":true,\"isBuy\":true,\"depositAmount\":\"");
    w.dec(deposit_micro);
    w.put(b"\",\"depositMint\":\"");
    w.put(mint_b58);
    w.put(b"\"}");
    w.done()
}

/// The `POST /execute` body: the signed transaction `signed` encoded as
/// base64 straight into the body, the build's `context` object verbatim,
/// and the order's client id as the correlation id.
#[must_use]
pub fn execute_body(dst: &mut [u8], signed: &[u8], context: &[u8], client_oid: u64) -> Option<usize> {
    if signed.len() > TX_MAX || context.len() > CONTEXT_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"signedTransaction\":\"");
    let need = base64_encoded_len(signed.len());
    if w.ok && w.n + need <= w.dst.len() {
        let n = w.n;
        w.n += base64_encode(signed, &mut w.dst[n..n + need]);
    } else {
        w.ok = false;
    }
    w.put(b"\",\"context\":");
    w.put(context);
    w.put(b",\"requestId\":\"");
    w.dec(client_oid);
    w.put(b"\"}");
    w.done()
}

/// The claim body for `owner_b58`.
#[must_use]
pub fn claim_body(dst: &mut [u8], owner_b58: &[u8]) -> Option<usize> {
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"{\"ownerPubkey\":\"");
    w.put(owner_b58);
    w.put(b"\"}");
    w.done()
}

/// `/prediction/v1/orders/status/<order pubkey>`.
#[must_use]
pub fn status_path(dst: &mut [u8], order_b58: &[u8]) -> Option<usize> {
    if order_b58.is_empty() || order_b58.len() > B58_KEY_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"/prediction/v1/orders/status/");
    w.put(order_b58);
    w.done()
}

/// `/prediction/v1/positions/<position pubkey>/claim`.
#[must_use]
pub fn claim_path(dst: &mut [u8], position_b58: &[u8]) -> Option<usize> {
    if position_b58.is_empty() || position_b58.len() > B58_KEY_MAX {
        return None;
    }
    let mut w = W { dst, n: 0, ok: true };
    w.put(b"/prediction/v1/positions/");
    w.put(position_b58);
    w.put(b"/claim");
    w.done()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{scan_build, BuildAnswer};

    #[test]
    fn the_order_body_is_the_probes_request_byte_for_byte() {
        let mut d = [0u8; ORDER_BODY_MAX];
        let n = order_body(
            &mut d,
            b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z",
            5_798_888,
            Outcome::Down,
            5_000_000,
            b"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        )
        .expect("fits");
        assert_eq!(
            &d[..n],
            br#"{"ownerPubkey":"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z","marketId":"POLY-5798888-1","isYes":true,"isBuy":true,"depositAmount":"5000000","depositMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"}"#
        );
        let mut small = [0u8; 40];
        assert_eq!(order_body(&mut small, b"x", 1, Outcome::Up, 1, b"m"), None);
    }

    #[test]
    fn the_execute_body_carries_the_transaction_and_the_context_verbatim() {
        let signed = [0xFFu8, 0x00, 0x01, 0x02];
        let ctx = br#"{"type":"create_order","orderPubkey":"Ord1111"}"#;
        let mut d = vec![0u8; EXECUTE_BODY_MAX];
        let n = execute_body(&mut d, &signed, ctx, 42).expect("fits");
        assert_eq!(
            &d[..n],
            br#"{"signedTransaction":"/wABAg==","context":{"type":"create_order","orderPubkey":"Ord1111"},"requestId":"42"}"#
        );
        // The body round-trips through the build scanner's context reader:
        // what comes back is what went in.
        let answer = format!(
            r#"{{"transaction":"AQ==","execution":{{"endpoint":"/x","context":{}}},"order":{{"maxBuyPriceUsd":"1","orderCostUsd":"1"}}}}"#,
            std::str::from_utf8(ctx).unwrap()
        );
        let mut a = BuildAnswer::default();
        scan_build(answer.as_bytes(), &mut a).expect("reads");
        assert_eq!(a.context.of(answer.as_bytes()), ctx.as_slice());
        let too_big = vec![b'x'; CONTEXT_MAX + 1];
        assert_eq!(execute_body(&mut d, &signed, &too_big, 1), None);
        let mut tiny = [0u8; 30];
        assert_eq!(execute_body(&mut tiny, &signed, ctx, 1), None);
        // A full-size transaction fits the bound.
        let full = vec![7u8; TX_MAX];
        assert!(execute_body(&mut d, &full, &[b'{'; CONTEXT_MAX], u64::MAX).is_some());
    }

    #[test]
    fn claim_body_and_paths_render_and_refuse_bad_keys() {
        let mut d = [0u8; CLAIM_BODY_MAX];
        let n = claim_body(&mut d, b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z").expect("fits");
        assert_eq!(&d[..n], br#"{"ownerPubkey":"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z"}"#);
        let mut p = [0u8; PATH_MAX];
        let n = status_path(&mut p, b"Ord1111").expect("fits");
        assert_eq!(&p[..n], b"/prediction/v1/orders/status/Ord1111");
        let n = claim_path(&mut p, b"Pos2222").expect("fits");
        assert_eq!(&p[..n], b"/prediction/v1/positions/Pos2222/claim");
        assert_eq!(status_path(&mut p, b""), None);
        assert_eq!(claim_path(&mut p, &[b'1'; B58_KEY_MAX + 1]), None);
    }
}
