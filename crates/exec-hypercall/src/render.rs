// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The request bodies, rendered IN PLACE and signed over their own
//! bytes (D7 — the Hypercall form of LAW E-3).
//!
//! Each writer renders the JSON body straight into the caller's buffer
//! (the `HttpsReq` body window), recording the span of every signed
//! string as it goes; the EIP-712 struct hash is then taken over THOSE
//! spans (`signer_eip712::hypercall`, `keccak256_parts`), and the
//! 65-byte signature is written as hex into the body's last field. A
//! price, a size, a symbol or a client id is rendered exactly once, so
//! the string the venue verifies can never differ from the one it
//! received.
//!
//! Zero allocation: the buffer is the caller's; every scratch is a
//! stack array. The only string escaping needed is none — every value
//! written is checked against a plain-ASCII alphabet
//! ([`plain_ascii`]), and a symbol outside it is refused rather than
//! escaped (an escaped symbol would sign different bytes than the
//! venue compares).

use core::ops::Range;

use secp256k1::SecretKey;
use signer_eip712::hypercall::{
    cancel_order_by_client_id_struct_hash, place_order_struct_hash, replace_order_struct_hash,
    sign_hc_with_key, HcPlaceView, HcReplaceView,
};

use crate::cloid::CLOID_LEN;
use crate::config::write_hex20;
use crate::num::{render_1e6, NUM_MAX};

/// Time in force.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Tif {
    /// Good till cancelled.
    Gtc,
    /// Immediate or cancel.
    Ioc,
}

impl Tif {
    /// The venue's spelling.
    #[must_use]
    pub const fn wire(self) -> &'static [u8] {
        match self {
            Self::Gtc => b"gtc",
            Self::Ioc => b"ioc",
        }
    }
}

/// The routing preference (signed).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Route {
    /// Best execution: book and the providers' firm RPI (S1 takes
    /// liquidity this way).
    BestExecution,
    /// The central book only (the smoke's dust order; makers).
    BookOnly,
}

impl Route {
    /// The venue's spelling.
    #[must_use]
    pub const fn wire(self) -> &'static [u8] {
        match self {
            Self::BestExecution => b"best_execution",
            Self::BookOnly => b"book_only",
        }
    }
}

/// Why a body did not render. Nothing was signed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RenderErr {
    /// The body does not fit the buffer.
    Overflow,
    /// A price or size is not positive.
    NonPositive,
    /// A symbol outside the plain-ASCII alphabet (or empty).
    BadSymbol,
    /// The signer refused (never with a parsed key).
    Sign,
}

/// Bytes a place body needs at most (symbol ≤ 32).
pub const PLACE_BODY_MAX: usize = 420;

/// The characters a value may carry unescaped: `[A-Za-z0-9._-]`.
#[must_use]
pub fn plain_ascii(s: &[u8]) -> bool {
    !s.is_empty()
        && s.iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_')
}

/// A cursor over the output buffer that records spans.
struct W<'a> {
    out: &'a mut [u8],
    n: usize,
}

impl W<'_> {
    #[inline]
    fn put(&mut self, b: &[u8]) -> Result<Range<usize>, RenderErr> {
        let end = self.n.checked_add(b.len()).ok_or(RenderErr::Overflow)?;
        if end > self.out.len() {
            return Err(RenderErr::Overflow);
        }
        let start = self.n;
        // COPY: literal and value bytes into the request body, each
        // once — rendering the body IS its construction; the body
        // window is the final wire buffer (HttpsReq writes it as is).
        self.out[start..end].copy_from_slice(b);
        self.n = end;
        Ok(start..end)
    }

    #[inline]
    fn num(&mut self, v: i64) -> Result<Range<usize>, RenderErr> {
        let mut s = [0u8; NUM_MAX];
        let k = render_1e6(v, &mut s).ok_or(RenderErr::NonPositive)?;
        self.put(&s[..k])
    }

    #[inline]
    fn uint(&mut self, v: u64) -> Result<Range<usize>, RenderErr> {
        let mut s = [0u8; 20];
        let mut k = 20usize;
        let mut x = v;
        loop {
            k -= 1;
            s[k] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        self.put(&s[k..])
    }

    #[inline]
    fn wallet(&mut self, w: &[u8; 20]) -> Result<Range<usize>, RenderErr> {
        let mut h = [0u8; 42];
        write_hex20(w, &mut h);
        self.put(&h)
    }

    /// The signature over `struct_hash`, as `0x` + 130 hex, then `"}`.
    fn sign_close(
        &mut self,
        sk: &SecretKey,
        dom: &[u8; 32],
        struct_hash: &[u8; 32],
    ) -> Result<usize, RenderErr> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let sig = sign_hc_with_key(sk, dom, struct_hash).map_err(|_| RenderErr::Sign)?;
        let mut h = [0u8; 132];
        h[0] = b'0';
        h[1] = b'x';
        let mut i = 0usize;
        while i < 65 {
            h[2 + 2 * i] = HEX[(sig[i] >> 4) as usize];
            h[3 + 2 * i] = HEX[(sig[i] & 0x0f) as usize];
            i += 1;
        }
        self.put(&h)?;
        self.put(b"\"}")?;
        Ok(self.n)
    }
}

/// A placement, as the arm has it.
#[derive(Debug, Copy, Clone)]
pub struct Place<'a> {
    /// The owner wallet.
    pub wallet: &'a [u8; 20],
    /// The instrument, the venue's spelling.
    pub symbol: &'a [u8],
    /// Buy (else sell).
    pub buy: bool,
    /// Limit price, USD ×1e6.
    pub px_1e6: i64,
    /// Contracts ×1e6.
    pub qty_1e6: i64,
    /// Time in force.
    pub tif: Tif,
    /// Route.
    pub route: Route,
    /// Our client id ([`crate::cloid`]).
    pub client_id: &'a [u8; CLOID_LEN],
    /// The nonce.
    pub nonce: u64,
}

#[inline]
const fn side_wire(buy: bool) -> &'static [u8] {
    if buy {
        b"Buy"
    } else {
        b"Sell"
    }
}

/// `POST /order`'s body, signed (`PlaceOrder`, the live route-bearing
/// type). Returns the body length.
///
/// # Errors
///
/// [`RenderErr`]; nothing is signed on a refusal.
pub fn place(
    out: &mut [u8],
    p: &Place<'_>,
    sk: &SecretKey,
    dom: &[u8; 32],
) -> Result<usize, RenderErr> {
    if !plain_ascii(p.symbol) {
        return Err(RenderErr::BadSymbol);
    }
    if p.px_1e6 <= 0 || p.qty_1e6 <= 0 {
        return Err(RenderErr::NonPositive);
    }
    let mut w = W { out, n: 0 };
    w.put(b"{\"wallet\":\"")?;
    w.wallet(p.wallet)?;
    w.put(b"\",\"symbol\":\"")?;
    let sym = w.put(p.symbol)?;
    w.put(b"\",\"side\":\"")?;
    let side = w.put(side_wire(p.buy))?;
    w.put(b"\",\"price\":\"")?;
    let price = w.num(p.px_1e6)?;
    w.put(b"\",\"size\":\"")?;
    let size = w.num(p.qty_1e6)?;
    w.put(b"\",\"tif\":\"")?;
    let tif = w.put(p.tif.wire())?;
    w.put(b"\",\"route\":\"")?;
    let route = w.put(p.route.wire())?;
    w.put(b"\",\"client_id\":\"")?;
    let cid = w.put(p.client_id)?;
    w.put(b"\",\"nonce\":")?;
    w.uint(p.nonce)?;
    w.put(b",\"signature\":\"")?;
    // D7: the hash reads the spans just written.
    let body = &w.out[..w.n];
    let h = place_order_struct_hash(&HcPlaceView {
        wallet: p.wallet,
        symbol: &body[sym],
        side: &body[side],
        size: &body[size],
        price: &body[price],
        tif: &body[tif],
        route: &body[route],
        client_id: &body[cid],
        nonce: p.nonce,
    });
    w.sign_close(sk, dom, &h)
}

/// A replace (atomic cancel + place, `PUT /order`).
#[derive(Debug, Copy, Clone)]
pub struct Replace<'a> {
    /// The owner wallet.
    pub wallet: &'a [u8; 20],
    /// The venue's id of the resting order.
    pub order_id: u64,
    /// The new order's instrument.
    pub symbol: &'a [u8],
    /// Buy (else sell).
    pub buy: bool,
    /// New limit, USD ×1e6.
    pub px_1e6: i64,
    /// New size, contracts ×1e6.
    pub qty_1e6: i64,
    /// Time in force.
    pub tif: Tif,
    /// The replacement's client id.
    pub client_id: &'a [u8; CLOID_LEN],
    /// The nonce.
    pub nonce: u64,
}

/// `PUT /order`'s body, signed (`ReplaceOrder`: `orderId` is signed as
/// the decimal string the JSON integer is written as — the same bytes).
///
/// # Errors
///
/// [`RenderErr`].
pub fn replace(
    out: &mut [u8],
    p: &Replace<'_>,
    sk: &SecretKey,
    dom: &[u8; 32],
) -> Result<usize, RenderErr> {
    if !plain_ascii(p.symbol) {
        return Err(RenderErr::BadSymbol);
    }
    if p.px_1e6 <= 0 || p.qty_1e6 <= 0 {
        return Err(RenderErr::NonPositive);
    }
    let mut w = W { out, n: 0 };
    w.put(b"{\"wallet\":\"")?;
    w.wallet(p.wallet)?;
    w.put(b"\",\"order_id\":")?;
    let oid = w.uint(p.order_id)?;
    w.put(b",\"symbol\":\"")?;
    let sym = w.put(p.symbol)?;
    w.put(b"\",\"side\":\"")?;
    let side = w.put(side_wire(p.buy))?;
    w.put(b"\",\"price\":\"")?;
    let price = w.num(p.px_1e6)?;
    w.put(b"\",\"size\":\"")?;
    let size = w.num(p.qty_1e6)?;
    w.put(b"\",\"tif\":\"")?;
    let tif = w.put(p.tif.wire())?;
    w.put(b"\",\"client_id\":\"")?;
    let cid = w.put(p.client_id)?;
    w.put(b"\",\"nonce\":")?;
    w.uint(p.nonce)?;
    w.put(b",\"signature\":\"")?;
    let body = &w.out[..w.n];
    let h = replace_order_struct_hash(&HcReplaceView {
        wallet: p.wallet,
        order_id: &body[oid],
        symbol: &body[sym],
        side: &body[side],
        size: &body[size],
        price: &body[price],
        tif: &body[tif],
        client_id: &body[cid],
        nonce: p.nonce,
    });
    w.sign_close(sk, dom, &h)
}

/// `DELETE /order_cloid`'s body, signed (`CancelOrderByClientId`).
///
/// # Errors
///
/// [`RenderErr::Overflow`], [`RenderErr::Sign`].
pub fn cancel_cloid(
    out: &mut [u8],
    wallet: &[u8; 20],
    client_id: &[u8; CLOID_LEN],
    nonce: u64,
    sk: &SecretKey,
    dom: &[u8; 32],
) -> Result<usize, RenderErr> {
    let mut w = W { out, n: 0 };
    w.put(b"{\"wallet\":\"")?;
    w.wallet(wallet)?;
    w.put(b"\",\"client_id\":\"")?;
    let cid = w.put(client_id)?;
    w.put(b"\",\"nonce\":")?;
    w.uint(nonce)?;
    w.put(b",\"signature\":\"")?;
    let body = &w.out[..w.n];
    let h = cancel_order_by_client_id_struct_hash(wallet, &body[cid], nonce);
    w.sign_close(sk, dom, &h)
}

/// `POST /risk/simulate/orders`'s body (unsigned; never mutates): one
/// resting leg.
///
/// # Errors
///
/// [`RenderErr`].
pub fn simulate(
    out: &mut [u8],
    wallet: &[u8; 20],
    symbol: &[u8],
    buy: bool,
    px_1e6: i64,
    qty_1e6: i64,
) -> Result<usize, RenderErr> {
    if !plain_ascii(symbol) {
        return Err(RenderErr::BadSymbol);
    }
    let mut w = W { out, n: 0 };
    w.put(b"{\"wallet\":\"")?;
    w.wallet(wallet)?;
    w.put(b"\",\"execution_mode\":\"resting\",\"orders\":[{\"symbol\":\"")?;
    w.put(symbol)?;
    w.put(b"\",\"side\":\"")?;
    w.put(side_wire(buy))?;
    w.put(b"\",\"size\":\"")?;
    w.num(qty_1e6)?;
    w.put(b"\",\"price\":\"")?;
    w.num(px_1e6)?;
    w.put(b"\"}]}")?;
    Ok(w.n)
}

/// A GET target `<path>?wallet=0x…[<tail>]` into `out` (the recon reads).
///
/// # Errors
///
/// [`RenderErr::Overflow`].
pub fn wallet_target(
    out: &mut [u8],
    path: &[u8],
    wallet: &[u8; 20],
    tail: &[u8],
) -> Result<usize, RenderErr> {
    let mut w = W { out, n: 0 };
    w.put(path)?;
    w.put(b"?wallet=")?;
    w.wallet(wallet)?;
    w.put(tail)?;
    Ok(w.n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use signer_eip712::hypercall::{hc_domain_separator, HC_CHAIN_ID_MAINNET};

    const KEY: [u8; 32] = [0x42; 32];
    const WALLET: [u8; 20] = [0xab; 20];

    fn sk() -> SecretKey {
        signer_eip712::parse_secret_key(&KEY).unwrap()
    }

    fn recover(digest: &[u8; 32], sig_hex: &[u8]) -> [u8; 20] {
        let mut sig = [0u8; 65];
        for i in 0..65 {
            let h = |c: u8| -> u8 { (c as char).to_digit(16).unwrap() as u8 };
            sig[i] = (h(sig_hex[2 + 2 * i]) << 4) | h(sig_hex[3 + 2 * i]);
        }
        let secp = secp256k1::Secp256k1::new();
        let rid = secp256k1::ecdsa::RecoveryId::from_i32(i32::from(sig[64]) - 27).unwrap();
        let rs = secp256k1::ecdsa::RecoverableSignature::from_compact(&sig[..64], rid).unwrap();
        let msg = secp256k1::Message::from_digest_slice(digest).unwrap();
        let pk = secp.recover_ecdsa(&msg, &rs).unwrap();
        let h = signer_eip712::keccak256(&pk.serialize_uncompressed()[1..]);
        let mut a = [0u8; 20];
        a.copy_from_slice(&h[12..]);
        a
    }

    fn json_str<'a>(b: &'a [u8], key: &[u8]) -> &'a [u8] {
        crate::json::field(b, key).unwrap().bytes(b)
    }

    #[test]
    fn a_place_body_is_valid_json_and_its_signature_covers_its_own_strings() {
        let dom = hc_domain_separator(HC_CHAIN_ID_MAINNET);
        let cid = crate::cloid::encode(7, 99);
        let p = Place {
            wallet: &WALLET,
            symbol: b"BTC-20261002-100000-C",
            buy: true,
            px_1e6: 500,
            qty_1e6: 1,
            tif: Tif::Gtc,
            route: Route::BookOnly,
            client_id: &cid,
            nonce: 1_767_225_600_000_000,
        };
        let mut out = [0u8; PLACE_BODY_MAX];
        let n = place(&mut out, &p, &sk(), &dom).unwrap();
        let b = &out[..n];
        assert!(crate::json::root(b).is_some(), "{}", String::from_utf8_lossy(b));
        assert_eq!(json_str(b, b"price"), b"0.0005");
        assert_eq!(json_str(b, b"size"), b"0.000001");
        assert_eq!(json_str(b, b"side"), b"Buy");
        assert_eq!(json_str(b, b"route"), b"book_only");
        assert_eq!(json_str(b, b"wallet"), &crate::config::hex20(&WALLET).into_bytes()[..]);
        let nonce = crate::json::field(b, b"nonce").unwrap().as_u64(b);
        assert_eq!(nonce, Some(1_767_225_600_000_000));
        // Re-hash from the body's own values: the signature recovers
        // to the key's address.
        let h = place_order_struct_hash(&HcPlaceView {
            wallet: &WALLET,
            symbol: json_str(b, b"symbol"),
            side: json_str(b, b"side"),
            size: json_str(b, b"size"),
            price: json_str(b, b"price"),
            tif: json_str(b, b"tif"),
            route: json_str(b, b"route"),
            client_id: json_str(b, b"client_id"),
            nonce: 1_767_225_600_000_000,
        });
        let d = signer_eip712::hypercall::hc_eip712_digest(&dom, &h);
        let signer = signer_eip712::address_from_private_key(&KEY).unwrap();
        assert_eq!(recover(&d, json_str(b, b"signature")), signer);
        assert!(n <= PLACE_BODY_MAX);
    }

    #[test]
    fn a_replace_signs_the_order_id_as_the_digits_it_sends() {
        let dom = hc_domain_separator(HC_CHAIN_ID_MAINNET);
        let cid = crate::cloid::encode(7, 100);
        let r = Replace {
            wallet: &WALLET,
            order_id: 123_456_789,
            symbol: b"SP500-20261002-7730-P",
            buy: false,
            px_1e6: 52_300,
            qty_1e6: 250_000,
            tif: Tif::Gtc,
            client_id: &cid,
            nonce: 5,
        };
        let mut out = [0u8; PLACE_BODY_MAX];
        let n = replace(&mut out, &r, &sk(), &dom).unwrap();
        let b = &out[..n];
        let oid = crate::json::field(b, b"order_id").unwrap();
        assert_eq!(oid.bytes(b), b"123456789");
        let h = replace_order_struct_hash(&HcReplaceView {
            wallet: &WALLET,
            order_id: b"123456789",
            symbol: json_str(b, b"symbol"),
            side: b"Sell",
            size: b"0.25",
            price: b"0.0523",
            tif: b"gtc",
            client_id: &cid,
            nonce: 5,
        });
        let d = signer_eip712::hypercall::hc_eip712_digest(&dom, &h);
        let signer = signer_eip712::address_from_private_key(&KEY).unwrap();
        assert_eq!(recover(&d, json_str(b, b"signature")), signer);
    }

    #[test]
    fn a_cancel_by_client_id_recovers_and_refusals_sign_nothing() {
        let dom = hc_domain_separator(HC_CHAIN_ID_MAINNET);
        let cid = crate::cloid::encode(7, 1);
        let mut out = [0u8; 300];
        let n = cancel_cloid(&mut out, &WALLET, &cid, 9, &sk(), &dom).unwrap();
        let b = &out[..n];
        let h = cancel_order_by_client_id_struct_hash(&WALLET, &cid, 9);
        let d = signer_eip712::hypercall::hc_eip712_digest(&dom, &h);
        let signer = signer_eip712::address_from_private_key(&KEY).unwrap();
        assert_eq!(recover(&d, json_str(b, b"signature")), signer);

        let bad = Place {
            wallet: &WALLET,
            symbol: b"BTC\"x",
            buy: true,
            px_1e6: 1,
            qty_1e6: 1,
            tif: Tif::Ioc,
            route: Route::BestExecution,
            client_id: &cid,
            nonce: 1,
        };
        let mut o2 = [0u8; PLACE_BODY_MAX];
        assert_eq!(place(&mut o2, &bad, &sk(), &dom), Err(RenderErr::BadSymbol));
        let zero = Place { symbol: b"BTC-1", px_1e6: 0, ..bad };
        assert_eq!(place(&mut o2, &zero, &sk(), &dom), Err(RenderErr::NonPositive));
        let mut tiny = [0u8; 40];
        let ok = Place { symbol: b"BTC-1", ..bad };
        assert_eq!(place(&mut tiny, &ok, &sk(), &dom), Err(RenderErr::Overflow));
    }

    #[test]
    fn the_simulate_body_and_the_wallet_targets_render() {
        let mut out = [0u8; 256];
        let n = simulate(&mut out, &WALLET, b"BTC-20261002-100000-C", true, 500, 1).unwrap();
        let b = &out[..n];
        assert!(crate::json::root(b).is_some());
        assert_eq!(json_str(b, b"execution_mode"), b"resting");
        let mut t = [0u8; 128];
        let n = wallet_target(&mut t, b"/orders", &WALLET, b"&status=open").unwrap();
        assert_eq!(
            &t[..n],
            b"/orders?wallet=0xabababababababababababababababababababab&status=open"
        );
    }
}
