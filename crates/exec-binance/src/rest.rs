// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Signed REST (plan §3.3; BX4 and BX5 carried).** Gateway thread.
//!
//! The futures × classic paths need REST for what their WS API does not
//! carry: the listenKey, the dead-man (`countdownCancelAll`, BX-17), the
//! sweep's fallback while the order session is down (`DELETE
//! allOpenOrders`), the time, the open orders and the day's trades (recon,
//! obligation 8), and the boot assertions (BX-19). USDⓈ-M's paths are
//! fapi's, COIN-M's dapi's (BX7, C1), and in Portfolio Margin each
//! product's papi paths (BX7, C2): one template table, indexed alike
//! ([`fut_specs`], [`ord_specs`]); a slot a host has no endpoint for is
//! never started there.
//!
//! **Signing in place** (BX4 carried): the parameters are rendered
//! contiguously into the connection's own template window
//! ([`QueryWriter`]), the Ed25519 signature is computed over exactly those
//! bytes and appended straight after as `&signature=<percent-encoded
//! base64>` ([`sign_in_place`], one `split_at_mut`, no second buffer).
//! The REST order route (BX7, C5: `order_transport = "rest"`, the Demo
//! battery's fallback) renders the WS API's own parameters as a query
//! ([`render_place`] and its siblings) on its own connection
//! ([`ord_specs`]); every order verb — the place, the cancel, the modify —
//! is body-form, as is the sweep's `DELETE allOpenOrders` (core-net's law
//! for the order path: a query-form request stages its header tail behind
//! the parameters on every send). HMAC (R22) signs the same bytes
//! ([`QueryWriter::sign_with`]: `&signature=<hex>`).
//!
//! **Answers** are read in place, fail closed: [`scan_error`] (the
//! venue's `{"code":…,"msg":…}`), [`scan_listen_key`],
//! [`scan_server_time`], [`scan_countdown`], [`scan_orders`] (the open
//! orders) and [`scan_trades`] (the day's trades).

use core_net::{Method, Params, ReqSpec};

use crate::config::{hex_lower, HmacSigner, Signer, HMAC_SIG_HEX};
use crate::inst::PRODUCT_COINM;
use crate::json::{bool_of, dec_1e6, i64_of, str_of, u64_of, Elems, Pairs};
use crate::num::{render_fixed, render_u64, RENDER_MAX};
use crate::userstream::{side_of, status_of, ScanErr, Span};
use crate::wsapi::{render_parts, side_word, Part, Place, WsStatic};

/// The form content type.
pub const FORM: &str = "application/x-www-form-urlencoded";
/// The API key header.
pub const KEY_HEADER: &str = "X-MBX-APIKEY";

/// `POST …/v1/listenKey`.
pub const T_LISTEN_KEY_NEW: usize = 0;
/// `PUT …/v1/listenKey` (keep-alive).
pub const T_LISTEN_KEY_KEEP: usize = 1;
/// `POST …/v1/countdownCancelAll` (signed).
pub const T_COUNTDOWN: usize = 2;
/// `GET …/v1/time`.
pub const T_TIME: usize = 3;
/// `GET …/v1/openOrders` (signed).
pub const T_OPEN_ORDERS: usize = 4;
/// `GET …/v1/userTrades` (signed).
pub const T_USER_TRADES: usize = 5;
/// `DELETE …/v1/allOpenOrders` (signed, body form — BX7): the sweep's
/// fallback (B1).
pub const T_CANCEL_ALL: usize = 6;
/// `GET …/v1/positionSide/dual` (signed).
pub const T_DUAL: usize = 7;
/// `GET /fapi/v1/multiAssetsMargin` (signed). USDⓈ-M classic only: dapi
/// and papi have no multi-assets toggle, and the slot is never started
/// there.
pub const T_MULTI_ASSETS: usize = 8;
/// `GET …/account` (signed): started only in Portfolio Margin — the
/// unified account, `/papi/v1/account` (BX7, C2: the ratio, the equity,
/// the account status). Classic reads its account over the order route.
pub const T_ACCOUNT: usize = 9;
/// `GET /fapi/v1/symbolConfig?symbol=…` (signed; BX7, BX-19): an owned
/// row's margin type and leverage, asserted at boot. USDⓈ-M only — COIN-M's
/// account read carries both for every symbol, and Portfolio Margin is
/// cross by construction — so the slot is never started elsewhere.
pub const T_SYMBOL_CONFIG: usize = 10;
/// The futures jobs connection's templates.
pub const FUT_TEMPLATES: usize = 11;

/// papi's connectivity check: what a slot papi has no endpoint for names
/// (the countdown — K13: no dead-man in PM — the time, multi-assets); it
/// is never started.
const PAPI_NONE: &str = "/papi/v1/ping";

/// The jobs connection's paths by template, USDⓈ-M.
const FAPI_PATHS: [&str; FUT_TEMPLATES] = [
    "/fapi/v1/listenKey",
    "/fapi/v1/listenKey",
    "/fapi/v1/countdownCancelAll",
    "/fapi/v1/time",
    "/fapi/v1/openOrders",
    "/fapi/v1/userTrades",
    "/fapi/v1/allOpenOrders",
    "/fapi/v1/positionSide/dual",
    "/fapi/v1/multiAssetsMargin",
    "/fapi/v3/account",
    "/fapi/v1/symbolConfig",
];

/// The jobs connection's paths by template, COIN-M. Its multi-assets and
/// symbol-config slots name no dapi endpoint: never started, they would
/// only ever be refused.
const DAPI_PATHS: [&str; FUT_TEMPLATES] = [
    "/dapi/v1/listenKey",
    "/dapi/v1/listenKey",
    "/dapi/v1/countdownCancelAll",
    "/dapi/v1/time",
    "/dapi/v1/openOrders",
    "/dapi/v1/userTrades",
    "/dapi/v1/allOpenOrders",
    "/dapi/v1/positionSide/dual",
    "/dapi/v1/multiAssetsMargin",
    "/dapi/v1/account",
    "/dapi/v1/symbolConfig",
];

/// Portfolio Margin's paths by template, USDⓈ-M (papi `um/`).
const PAPI_UM_PATHS: [&str; FUT_TEMPLATES] = [
    "/papi/v1/listenKey",
    "/papi/v1/listenKey",
    PAPI_NONE,
    PAPI_NONE,
    "/papi/v1/um/openOrders",
    "/papi/v1/um/userTrades",
    "/papi/v1/um/allOpenOrders",
    "/papi/v1/um/positionSide/dual",
    PAPI_NONE,
    "/papi/v1/account",
    PAPI_NONE,
];

/// Portfolio Margin's paths by template, COIN-M (papi `cm/`).
const PAPI_CM_PATHS: [&str; FUT_TEMPLATES] = [
    "/papi/v1/listenKey",
    "/papi/v1/listenKey",
    PAPI_NONE,
    PAPI_NONE,
    "/papi/v1/cm/openOrders",
    "/papi/v1/cm/userTrades",
    "/papi/v1/cm/allOpenOrders",
    "/papi/v1/cm/positionSide/dual",
    PAPI_NONE,
    "/papi/v1/account",
    PAPI_NONE,
];

/// `GET /sapi/v1/account/apiRestrictions` (signed).
pub const S_API_RESTRICTIONS: usize = 0;
/// `GET /sapi/v1/portfolio/account` (signed).
pub const S_PORTFOLIO: usize = 1;
/// `GET /api/v3/time`: the clock in Portfolio Margin, whose papi has no
/// time endpoint (Binance's one server time; BX7, C2).
pub const S_TIME: usize = 2;
/// The sapi templates.
pub const SAPI_TEMPLATES: usize = 3;

/// The futures jobs connection's templates for `product` (`PRODUCT_COINM`:
/// dapi; otherwise fapi) — in Portfolio Margin (`pm`), the product's papi
/// paths — every one carrying the key header.
#[must_use]
pub fn fut_specs<'a>(product: u8, pm: bool, key: &'a [(&'a str, &'a str)]) -> [ReqSpec<'a>; FUT_TEMPLATES] {
    let p = match (pm, product == PRODUCT_COINM) {
        (false, false) => &FAPI_PATHS,
        (false, true) => &DAPI_PATHS,
        (true, false) => &PAPI_UM_PATHS,
        (true, true) => &PAPI_CM_PATHS,
    };
    let q = |method, t: usize| ReqSpec {
        method,
        path: p[t],
        params: Params::Query,
        headers: key,
    };
    let b = |method, t: usize| ReqSpec {
        method,
        path: p[t],
        params: Params::Body(FORM),
        headers: key,
    };
    [
        b(Method::Post, T_LISTEN_KEY_NEW),
        b(Method::Put, T_LISTEN_KEY_KEEP),
        b(Method::Post, T_COUNTDOWN),
        q(Method::Get, T_TIME),
        q(Method::Get, T_OPEN_ORDERS),
        q(Method::Get, T_USER_TRADES),
        b(Method::Delete, T_CANCEL_ALL),
        q(Method::Get, T_DUAL),
        q(Method::Get, T_MULTI_ASSETS),
        q(Method::Get, T_ACCOUNT),
        q(Method::Get, T_SYMBOL_CONFIG),
    ]
}

/// The REST order route's templates (BX7, C5), on its own keep-alive
/// connection — one request on the wire at a time.
/// `POST …/v1/order` (signed, body form).
pub const R_PLACE: usize = 0;
/// `DELETE …/v1/order` (signed, body form — BX7): one order's cancel by its
/// client id (or, a stray's, by the venue's id).
pub const R_CANCEL: usize = 1;
/// `PUT …/v1/order` (signed, body form): a modify.
pub const R_MODIFY: usize = 2;
/// `GET …/v1/order` (signed): the in-doubt query (BX-11).
pub const R_STATUS: usize = 3;
/// `GET /fapi/v3/account` · `GET /dapi/v1/account` (signed): recon and
/// margin — the WS API's `v2/account.status` · `account.status`, the same
/// shapes (fapi's v2 lists every symbol of the market and no `notional`:
/// BX7 moved the route to v3, the WS API's own). In
/// Portfolio Margin the product's positions: `GET /papi/v2/um/account` ·
/// `GET /papi/v1/cm/account` (the margin is the unified account's,
/// [`T_ACCOUNT`]).
pub const R_ACCOUNT: usize = 4;
/// The order route's templates.
pub const ORD_TEMPLATES: usize = 5;

/// The order route's templates for `product` (`PRODUCT_COINM`: dapi;
/// otherwise fapi; in Portfolio Margin, `pm`, the product's papi paths),
/// every one carrying the key header.
#[must_use]
pub fn ord_specs<'a>(product: u8, pm: bool, key: &'a [(&'a str, &'a str)]) -> [ReqSpec<'a>; ORD_TEMPLATES] {
    let (order, account) = match (pm, product == PRODUCT_COINM) {
        (false, false) => ("/fapi/v1/order", "/fapi/v3/account"),
        (false, true) => ("/dapi/v1/order", "/dapi/v1/account"),
        (true, false) => ("/papi/v1/um/order", "/papi/v2/um/account"),
        (true, true) => ("/papi/v1/cm/order", "/papi/v1/cm/account"),
    };
    let q = |method, path| ReqSpec {
        method,
        path,
        params: Params::Query,
        headers: key,
    };
    let b = |method, path| ReqSpec {
        method,
        path,
        params: Params::Body(FORM),
        headers: key,
    };
    [
        b(Method::Post, order),
        b(Method::Delete, order),
        b(Method::Put, order),
        q(Method::Get, order),
        q(Method::Get, account),
    ]
}

/// **`POST /fapi/v1/order`** — the REST route's place (BX7, C5): the WS
/// API's `order.place` parameters as a query, rendered in place.
pub fn render_place(w: &mut QueryWriter<'_>, st: &WsStatic, o: &Place<'_>) {
    let tif: &[u8] = if o.maker { b"GTX" } else { b"IOC" };
    let ro: &[u8] = if o.reduce_only { b"&reduceOnly=true" } else { b"" };
    w.parts(&[
        Part::Lit(b"symbol="),
        Part::Lit(o.symbol),
        Part::Lit(b"&side="),
        Part::Lit(side_word(o.side)),
        Part::Lit(b"&type=LIMIT&timeInForce="),
        Part::Lit(tif),
        Part::Lit(ro),
        Part::Lit(b"&quantity="),
        Part::Fixed(o.qty_1e6, o.qty_dec),
        Part::Lit(b"&price="),
        Part::Fixed(o.px_1e6, o.px_dec),
        Part::Lit(b"&newClientOrderId="),
        o.cid,
        Part::Lit(b"&newOrderRespType=ACK"),
        // papi's COIN-M order takes no STP mode (an unread parameter is
        // refused): its lane's static carries none.
        Part::Lit(if st.stp.is_empty() { b"" } else { b"&selfTradePreventionMode=" }),
        Part::Lit(st.stp),
        Part::Lit(b"&recvWindow="),
        Part::U64(st.recv_window_ms),
        Part::Lit(b"&timestamp="),
        Part::U64(o.ts_ms),
    ]);
}

/// **`PUT /fapi/v1/order`** — a modify: price and quantity together; the
/// venue keeps the order's client id, time in force and flags.
pub fn render_modify(w: &mut QueryWriter<'_>, st: &WsStatic, o: &Place<'_>) {
    w.parts(&[
        Part::Lit(b"symbol="),
        Part::Lit(o.symbol),
        Part::Lit(b"&side="),
        Part::Lit(side_word(o.side)),
        Part::Lit(b"&origClientOrderId="),
        o.cid,
        Part::Lit(b"&quantity="),
        Part::Fixed(o.qty_1e6, o.qty_dec),
        Part::Lit(b"&price="),
        Part::Fixed(o.px_1e6, o.px_dec),
        Part::Lit(b"&recvWindow="),
        Part::U64(st.recv_window_ms),
        Part::Lit(b"&timestamp="),
        Part::U64(o.ts_ms),
    ]);
}

/// **`DELETE` / `GET /fapi/v1/order`** — one order by its client id: a
/// cancel, or the in-doubt query.
pub fn render_by_cid(w: &mut QueryWriter<'_>, st: &WsStatic, symbol: &[u8], cid: Part<'_>, ts_ms: u64) {
    w.parts(&[
        Part::Lit(b"symbol="),
        Part::Lit(symbol),
        Part::Lit(b"&origClientOrderId="),
        cid,
        Part::Lit(b"&recvWindow="),
        Part::U64(st.recv_window_ms),
        Part::Lit(b"&timestamp="),
        Part::U64(ts_ms),
    ]);
}

/// **`DELETE /fapi/v1/order`** by the venue's order id — a stray the
/// REST route cancels (its client id lay in an answer since overwritten).
pub fn render_by_order_id(w: &mut QueryWriter<'_>, st: &WsStatic, symbol: &[u8], order_id: u64, ts_ms: u64) {
    w.parts(&[
        Part::Lit(b"symbol="),
        Part::Lit(symbol),
        Part::Lit(b"&orderId="),
        Part::U64(order_id),
        Part::Lit(b"&recvWindow="),
        Part::U64(st.recv_window_ms),
        Part::Lit(b"&timestamp="),
        Part::U64(ts_ms),
    ]);
}

/// **`GET /fapi/v3/account`** — recon and margin.
pub fn render_account(w: &mut QueryWriter<'_>, st: &WsStatic, ts_ms: u64) {
    w.parts(&[Part::Lit(b"recvWindow="), Part::U64(st.recv_window_ms), Part::Lit(b"&timestamp="), Part::U64(ts_ms)]);
}

/// The sapi connection's templates.
#[must_use]
pub fn sapi_specs<'a>(key: &'a [(&'a str, &'a str)]) -> [ReqSpec<'a>; SAPI_TEMPLATES] {
    let q = |path| ReqSpec {
        method: Method::Get,
        path,
        params: Params::Query,
        headers: key,
    };
    [q("/sapi/v1/account/apiRestrictions"), q("/sapi/v1/portfolio/account"), q("/api/v3/time")]
}

/// A render that did not fit its window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Overflow;

/// **Parameters rendered in place** into a template window.
pub struct QueryWriter<'a> {
    buf: &'a mut [u8],
    n: usize,
    ok: bool,
}

impl<'a> QueryWriter<'a> {
    /// Write from the start of `buf` (the template's window).
    #[must_use]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, n: 0, ok: true }
    }

    /// Append literal bytes.
    #[inline]
    pub fn put(&mut self, b: &[u8]) -> &mut Self {
        if self.ok && self.n + b.len() <= self.buf.len() {
            // COPY: a parameter name or value (≤ 64 B) into the request
            // window — this IS the render (cold REST, once per request); the
            // window is the one buffer the bytes are sent from.
            self.buf[self.n..self.n + b.len()].copy_from_slice(b);
            self.n += b.len();
        } else {
            self.ok = false;
        }
        self
    }

    /// Append an unsigned integer.
    #[inline]
    pub fn uint(&mut self, v: u64) -> &mut Self {
        if self.ok && self.n + 20 <= self.buf.len() {
            self.n += render_u64(v, &mut self.buf[self.n..]);
        } else {
            self.ok = false;
        }
        self
    }

    /// Append a ×1e6 value with `dec` decimals.
    #[inline]
    pub fn fixed(&mut self, v_1e6: i64, dec: u8) -> &mut Self {
        if self.ok && self.n + RENDER_MAX <= self.buf.len() {
            self.n += render_fixed(v_1e6, dec, &mut self.buf[self.n..]);
        } else {
            self.ok = false;
        }
        self
    }

    /// Append parts — a number's digits, our client id — rendered in
    /// place (the WS API's renderer, BX7).
    #[inline]
    pub fn parts(&mut self, parts: &[Part<'_>]) -> &mut Self {
        if self.ok {
            match render_parts(parts, &mut self.buf[self.n..]) {
                Ok(k) => self.n += k,
                Err(_) => self.ok = false,
            }
        }
        self
    }

    /// The rendered length, or `Overflow`.
    pub fn rendered(&self) -> Result<usize, Overflow> {
        if self.ok {
            Ok(self.n)
        } else {
            Err(Overflow)
        }
    }

    /// Sign what was rendered and append the signature (module docs).
    /// Returns the request's total length.
    pub fn sign(self, signer: &signer_ed25519::Ed25519Signer) -> Result<usize, Overflow> {
        let n = self.rendered()?;
        sign_in_place(signer, self.buf, n)
    }

    /// Sign with the arm's signer — Ed25519, or HMAC where the
    /// configuration says so (BX7, R22).
    pub fn sign_with(self, signer: &Signer) -> Result<usize, Overflow> {
        match signer {
            Signer::Ed25519(s) => self.sign(s),
            Signer::Hmac(h) => {
                let n = self.rendered()?;
                sign_hmac_in_place(h, self.buf, n)
            }
        }
    }
}

const SIG_KEY: &[u8] = b"&signature=";

/// Sign `buf[..n]` and append `&signature=<pct-b64>` right after it.
pub fn sign_in_place(signer: &signer_ed25519::Ed25519Signer, buf: &mut [u8], n: usize) -> Result<usize, Overflow> {
    let need = n + SIG_KEY.len() + signer_ed25519::SIG_B64_PCT_MAX;
    if n == 0 || need > buf.len() {
        return Err(Overflow);
    }
    let (msg, rest) = buf.split_at_mut(n);
    // COPY: the 11 B literal `&signature=` after the signed bytes — the
    // render of the separator itself; nothing precedes it anywhere else.
    rest[..SIG_KEY.len()].copy_from_slice(SIG_KEY);
    let m = signer.sign_b64_pct(msg, &mut rest[SIG_KEY.len()..]);
    Ok(n + SIG_KEY.len() + m)
}

/// HMAC (R22): sign `buf[..n]` and append `&signature=<64 hex>` right
/// after it. The inner hash pads in place, in the 75 B the signature then
/// takes (BX7: ≥ `SHA256_PAD_MAX`; no byte of the request is stashed).
pub fn sign_hmac_in_place(h: &HmacSigner, buf: &mut [u8], n: usize) -> Result<usize, Overflow> {
    const _: () = assert!(SIG_KEY.len() + HMAC_SIG_HEX >= core_crypto::SHA256_PAD_MAX);
    let need = n + SIG_KEY.len() + HMAC_SIG_HEX;
    if n == 0 || need > buf.len() {
        return Err(Overflow);
    }
    let tag = h.tag_in_place(&mut buf[..need], n).ok_or(Overflow)?;
    let rest = &mut buf[n..need];
    // COPY: the 11 B literal `&signature=` after the signed bytes — the
    // separator's render, as for Ed25519 (over the padding the tag left).
    rest[..SIG_KEY.len()].copy_from_slice(SIG_KEY);
    let Ok(sig) = <&mut [u8; HMAC_SIG_HEX]>::try_from(&mut rest[SIG_KEY.len()..]) else {
        return Err(Overflow);
    };
    hex_lower(&tag, sig);
    Ok(need)
}

// -------------------------------------------------------------------------
// Answers
// -------------------------------------------------------------------------

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// The venue's refusal code in a `{"code":…,"msg":…}` body; 0 if the body
/// is not one. BX7 (the one-reading law, as the WS API's `error` is read):
/// the body is ONE object, whole and well-formed — bytes after it, a pair
/// it cannot walk or `code` named twice is no code (0: "no code", which a
/// refusal under 500 may not be).
#[must_use]
pub fn scan_error(body: &[u8]) -> i32 {
    let whole = match core_parse::skip_json_value(body, 0) {
        Some(end) => core_parse::skip_ws(body, end) == body.len(),
        None => false,
    };
    if !whole {
        return 0;
    }
    let Ok(mut w) = Pairs::new(body, 0) else {
        return 0;
    };
    let (mut code, mut coded) = (0, false);
    loop {
        match w.next() {
            Ok(Some((k, a, z))) if k == b"code" => {
                if coded {
                    return 0;
                }
                coded = true;
                if let Ok(c) = i64_of(body, a, z) {
                    code = c.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return code,
            Err(()) => return 0,
        }
    }
}

/// The venue ms a 418 names in its message (`…IP banned until
/// 1790000600000…`); 0 if it names none — the latest, if it names several
/// (BX7: as the WS API's `retryAfter` copies are read). Advisory: it can
/// only lengthen the gateway's quiet, never shorten it.
#[must_use]
pub fn scan_ban_until(body: &[u8]) -> u64 {
    const AT: &[u8] = b"banned until ";
    let mut latest = 0u64;
    let mut i = 0;
    while i + AT.len() <= body.len() {
        if &body[i..i + AT.len()] == AT {
            let mut j = i + AT.len();
            let mut v = 0u64;
            let mut n = 0;
            while j < body.len() && body[j].is_ascii_digit() && n < 19 {
                v = v * 10 + (body[j] - b'0') as u64;
                j += 1;
                n += 1;
            }
            latest = latest.max(v);
            i = j;
            continue;
        }
        i += 1;
    }
    latest
}

/// `{"listenKey":"…"}` → the key.
pub fn scan_listen_key(body: &[u8]) -> Result<Span, ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        if k == b"listenKey" {
            return Ok(Span::of(body, str_of(body, a, z).map_err(bad)?));
        }
    }
    Err(ScanErr::Missing)
}

/// `{"serverTime":…}` → ms.
pub fn scan_server_time(body: &[u8]) -> Result<u64, ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        if k == b"serverTime" {
            return u64_of(body, a, z).map_err(bad);
        }
    }
    Err(ScanErr::Missing)
}

/// `{"symbol":"…","countdownTime":"…"}` — the dead-man is armed for that
/// symbol (anything else is a refusal).
pub fn scan_countdown(body: &[u8]) -> Result<Span, ScanErr> {
    let mut w = Pairs::new(body, 0).map_err(malformed)?;
    let (mut sym, mut ct) = (None, false);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"symbol" => sym = Some(Span::of(body, str_of(body, a, z).map_err(bad)?)),
            b"countdownTime" => {
                u64_of(body, a, z).map_err(bad)?;
                ct = true;
            }
            _ => {}
        }
    }
    match (sym, ct) {
        (Some(s), true) => Ok(s),
        _ => Err(ScanErr::Missing),
    }
}

/// One order from `openOrders`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OrderRow {
    /// `orderId`.
    pub venue_oid: u64,
    /// `origQty` ×1e6.
    pub qty_1e6: i64,
    /// `executedQty` ×1e6.
    pub executed_1e6: i64,
    /// `price` ×1e6.
    pub px_1e6: i64,
    /// `avgPrice` ×1e6.
    pub avg_px_1e6: i64,
    /// `cumQuote` ×1e6 (the filled notional).
    pub cum_quote_1e6: i64,
    /// `updateTime` ms.
    pub update_ms: u64,
    /// `symbol`.
    pub symbol: Span,
    /// `clientOrderId`.
    pub cid: Span,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `S_*`.
    pub status: u8,
    _r: [u8; 6],
}

/// Why an array answer was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArrErr {
    /// The body or an element did not scan.
    Scan(ScanErr),
    /// More elements than the buffer holds: refused, never truncated.
    Truncated,
}

impl From<ScanErr> for ArrErr {
    fn from(e: ScanErr) -> Self {
        Self::Scan(e)
    }
}

const O_SYM: u32 = 1 << 0;
const O_CID: u32 = 1 << 1;
const O_OID: u32 = 1 << 2;
const O_SIDE: u32 = 1 << 3;
const O_STATUS: u32 = 1 << 4;
const O_QTY: u32 = 1 << 5;
const O_EXEC: u32 = 1 << 6;
const O_NEED: u32 = O_SYM | O_CID | O_OID | O_SIDE | O_STATUS | O_QTY | O_EXEC;

/// An array of orders into `out`; the count. Fail closed: a missing field
/// or an element past `out` refuses the whole answer.
pub fn scan_orders(body: &[u8], out: &mut [OrderRow]) -> Result<usize, ArrErr> {
    let mut e = Elems::new(body, 0).map_err(malformed)?;
    let mut n = 0;
    while let Some((a, _)) = e.next().map_err(malformed)? {
        if n == out.len() {
            return Err(ArrErr::Truncated);
        }
        let r = &mut out[n];
        *r = OrderRow::default();
        let mut w = Pairs::new(body, a).map_err(malformed)?;
        let mut have = 0u32;
        while let Some((k, va, vz)) = w.next().map_err(malformed)? {
            match k {
                b"symbol" => {
                    r.symbol = Span::of(body, str_of(body, va, vz).map_err(bad)?);
                    have |= O_SYM;
                }
                b"clientOrderId" => {
                    r.cid = Span::of(body, str_of(body, va, vz).map_err(bad)?);
                    have |= O_CID;
                }
                b"orderId" => {
                    r.venue_oid = u64_of(body, va, vz).map_err(bad)?;
                    have |= O_OID;
                }
                b"side" => {
                    r.side = side_of(str_of(body, va, vz).map_err(bad)?)?;
                    have |= O_SIDE;
                }
                b"status" => {
                    r.status = status_of(str_of(body, va, vz).map_err(bad)?);
                    have |= O_STATUS;
                }
                b"origQty" => {
                    r.qty_1e6 = dec_1e6(body, va, vz).map_err(bad)?;
                    have |= O_QTY;
                }
                b"executedQty" => {
                    r.executed_1e6 = dec_1e6(body, va, vz).map_err(bad)?;
                    have |= O_EXEC;
                }
                b"price" => r.px_1e6 = dec_1e6(body, va, vz).map_err(bad)?,
                b"avgPrice" => r.avg_px_1e6 = dec_1e6(body, va, vz).map_err(bad)?,
                b"cumQuote" => r.cum_quote_1e6 = dec_1e6(body, va, vz).map_err(bad)?,
                b"updateTime" => r.update_ms = u64_of(body, va, vz).map_err(bad)?,
                _ => {}
            }
        }
        if have & O_NEED != O_NEED {
            return Err(ArrErr::Scan(ScanErr::Missing));
        }
        n += 1;
    }
    Ok(n)
}

/// One trade from `userTrades`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TradeRow {
    /// `orderId`.
    pub venue_oid: u64,
    /// `id` (the trade id).
    pub trade_id: u64,
    /// `price` ×1e6.
    pub px_1e6: i64,
    /// `qty` ×1e6.
    pub qty_1e6: i64,
    /// `time` ms.
    pub time_ms: u64,
    /// `symbol`.
    pub symbol: Span,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `maker`.
    pub maker: u8,
    _r: [u8; 6],
}

const R_OID: u32 = 1 << 0;
const R_ID: u32 = 1 << 1;
const R_PX: u32 = 1 << 2;
const R_QTY: u32 = 1 << 3;
const R_TIME: u32 = 1 << 4;
const R_SIDE: u32 = 1 << 5;
const R_SYM: u32 = 1 << 6;
const R_NEED: u32 = R_OID | R_ID | R_PX | R_QTY | R_TIME | R_SIDE | R_SYM;

/// An array of trades into `out`; the count (fail closed, as
/// [`scan_orders`]).
pub fn scan_trades(body: &[u8], out: &mut [TradeRow]) -> Result<usize, ArrErr> {
    let mut e = Elems::new(body, 0).map_err(malformed)?;
    let mut n = 0;
    while let Some((a, _)) = e.next().map_err(malformed)? {
        if n == out.len() {
            return Err(ArrErr::Truncated);
        }
        let r = &mut out[n];
        *r = TradeRow::default();
        let mut w = Pairs::new(body, a).map_err(malformed)?;
        let mut have = 0u32;
        while let Some((k, va, vz)) = w.next().map_err(malformed)? {
            match k {
                b"orderId" => {
                    r.venue_oid = u64_of(body, va, vz).map_err(bad)?;
                    have |= R_OID;
                }
                b"id" => {
                    r.trade_id = u64_of(body, va, vz).map_err(bad)?;
                    have |= R_ID;
                }
                b"price" => {
                    r.px_1e6 = dec_1e6(body, va, vz).map_err(bad)?;
                    have |= R_PX;
                }
                b"qty" => {
                    r.qty_1e6 = dec_1e6(body, va, vz).map_err(bad)?;
                    have |= R_QTY;
                }
                b"time" => {
                    r.time_ms = u64_of(body, va, vz).map_err(bad)?;
                    have |= R_TIME;
                }
                b"side" => {
                    r.side = side_of(str_of(body, va, vz).map_err(bad)?)?;
                    have |= R_SIDE;
                }
                b"symbol" => {
                    r.symbol = Span::of(body, str_of(body, va, vz).map_err(bad)?);
                    have |= R_SYM;
                }
                b"maker" => r.maker = bool_of(body, va, vz).map_err(bad)? as u8,
                _ => {}
            }
        }
        if have & R_NEED != R_NEED {
            return Err(ArrErr::Scan(ScanErr::Missing));
        }
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_ban_names_its_end() {
        let b = br#"{"code":-1003,"msg":"Way too much request weight used; IP banned until 1790000600000. Please use WebSocket Streams for live updates to avoid bans."}"#;
        assert_eq!(super::scan_ban_until(b), 1_790_000_600_000);
        assert_eq!(super::scan_ban_until(br#"{"code":-1003,"msg":"Too many requests."}"#), 0);
        assert_eq!(super::scan_ban_until(b"banned until "), 0, "no digits: none");
        assert_eq!(super::scan_ban_until(b"banned until 99999999999999999999999"), 9_999_999_999_999_999_999, "capped at 19 digits");
        // BX7: the latest end named (never a shorter quiet than the venue's).
        assert_eq!(super::scan_ban_until(b"IP banned until 1000. IP banned until 1790000600000."), 1_790_000_600_000);
        assert_eq!(super::scan_ban_until(b"IP banned until 1790000600000. IP banned until 1000."), 1_790_000_600_000);
    }

    use super::*;

    /// BX7 (F1): every order verb on the REST route — the place, the
    /// cancel, the modify — and the sweep's cancel-all are body-form, on
    /// every product and mode: core-net's law for the order path (a
    /// query-form request stages its header tail on every send).
    #[test]
    fn the_order_verbs_are_body_form() {
        let key = [(KEY_HEADER, "k")];
        for (product, pm) in [(crate::inst::PRODUCT_USDM, false), (PRODUCT_COINM, false), (crate::inst::PRODUCT_USDM, true), (PRODUCT_COINM, true)] {
            let o = ord_specs(product, pm, &key);
            for t in [R_PLACE, R_CANCEL, R_MODIFY] {
                assert!(matches!(o[t].params, Params::Body(FORM)), "template {t}");
            }
            assert!(matches!(o[R_STATUS].params, Params::Query), "a read stays a query");
            assert!(matches!(fut_specs(product, pm, &key)[T_CANCEL_ALL].params, Params::Body(FORM)));
        }
    }

    #[test]
    fn each_product_has_its_own_paths_by_the_same_template() {
        let key = [(KEY_HEADER, "k")];
        let um = fut_specs(crate::inst::PRODUCT_USDM, false, &key);
        let cm = fut_specs(PRODUCT_COINM, false, &key);
        let mut t = 0;
        while t < FUT_TEMPLATES {
            assert!(um[t].path.starts_with("/fapi/v"), "{}", um[t].path);
            assert!(cm[t].path.starts_with("/dapi/v1/"), "{}", cm[t].path);
            if t != T_ACCOUNT {
                assert_eq!(um[t].path[5..], cm[t].path[5..], "template {t} names one endpoint");
            }
            assert_eq!(um[t].method, cm[t].method);
            t += 1;
        }
        let (um, cm) = (ord_specs(crate::inst::PRODUCT_USDM, false, &key), ord_specs(PRODUCT_COINM, false, &key));
        assert_eq!((um[R_PLACE].path, um[R_ACCOUNT].path), ("/fapi/v1/order", "/fapi/v3/account"));
        assert_eq!((cm[R_PLACE].path, cm[R_ACCOUNT].path), ("/dapi/v1/order", "/dapi/v1/account"));
        // Portfolio Margin (BX7, C2): each product's papi paths; one
        // listenKey and one unified account for both.
        let (pu, pc) = (fut_specs(crate::inst::PRODUCT_USDM, true, &key), fut_specs(PRODUCT_COINM, true, &key));
        assert_eq!((pu[T_OPEN_ORDERS].path, pc[T_OPEN_ORDERS].path), ("/papi/v1/um/openOrders", "/papi/v1/cm/openOrders"));
        assert_eq!((pu[T_USER_TRADES].path, pc[T_DUAL].path), ("/papi/v1/um/userTrades", "/papi/v1/cm/positionSide/dual"));
        assert_eq!((pu[T_LISTEN_KEY_NEW].path, pc[T_LISTEN_KEY_NEW].path), ("/papi/v1/listenKey", "/papi/v1/listenKey"));
        assert_eq!((pu[T_ACCOUNT].path, pc[T_ACCOUNT].path), ("/papi/v1/account", "/papi/v1/account"));
        // BX7 (BX-19): the margin type is USDⓈ-M's to ask; papi never.
        let um = fut_specs(crate::inst::PRODUCT_USDM, false, &key);
        assert_eq!((um[T_SYMBOL_CONFIG].path, um[T_SYMBOL_CONFIG].method), ("/fapi/v1/symbolConfig", Method::Get));
        assert_eq!((pu[T_SYMBOL_CONFIG].path, pc[T_SYMBOL_CONFIG].path), (PAPI_NONE, PAPI_NONE));
        let (pu, pc) = (ord_specs(crate::inst::PRODUCT_USDM, true, &key), ord_specs(PRODUCT_COINM, true, &key));
        assert_eq!((pu[R_PLACE].path, pu[R_ACCOUNT].path), ("/papi/v1/um/order", "/papi/v2/um/account"));
        assert_eq!((pc[R_PLACE].path, pc[R_ACCOUNT].path), ("/papi/v1/cm/order", "/papi/v1/cm/account"));
        assert_eq!(sapi_specs(&key)[S_TIME].path, "/api/v3/time");
    }
    use proptest::prelude::*;

    #[test]
    fn a_signed_query_is_rendered_then_signed_in_place() {
        let signer = signer_ed25519::Ed25519Signer::from_seed(&[3u8; 32]).unwrap();
        let mut win = [0u8; 512];
        let mut w = QueryWriter::new(&mut win);
        w.put(b"symbol=").put(b"BTCUSDT").put(b"&countdownTime=").uint(30_000).put(b"&recvWindow=").uint(1_000).put(b"&timestamp=").uint(1_790_000_000_000);
        let n = w.rendered().unwrap();
        let total = w.sign(&signer).unwrap();
        let q = std::str::from_utf8(&win[..total]).unwrap();
        let (msg, sig) = q.split_once("&signature=").unwrap();
        assert_eq!(msg, "symbol=BTCUSDT&countdownTime=30000&recvWindow=1000&timestamp=1790000000000");
        assert_eq!(msg.len(), n);
        let mut want = [0u8; signer_ed25519::SIG_B64_PCT_MAX];
        let m = signer.sign_b64_pct(msg.as_bytes(), &mut want);
        assert_eq!(sig.as_bytes(), &want[..m]);
        // Too small a window refuses, never truncates.
        let mut tiny = [0u8; 40];
        let mut w = QueryWriter::new(&mut tiny);
        w.put(b"timestamp=").uint(1);
        assert_eq!(w.sign(&signer), Err(Overflow));
        let mut tiny = [0u8; 4];
        let mut w = QueryWriter::new(&mut tiny);
        w.put(b"timestamp=");
        assert_eq!(w.rendered(), Err(Overflow));
    }

    /// BX7 (C5, R22): the REST route renders the WS API's parameters as a
    /// query, and HMAC signs exactly those bytes. Break-and-watch: a modify
    /// that renders the time in force, or HMAC over anything but the
    /// rendered bytes, fails here.
    #[test]
    fn the_rest_order_route_renders_and_hmac_signs() {
        const PREFIX: crate::cid::CidPrefix = crate::cid::CidPrefix::new(0x6512_ab0f);
        let st = WsStatic::new("EXPIRE_MAKER", 1_000);
        let o = Place {
            symbol: b"BTCUSDT",
            side: core_types::Side::Ask as u8,
            maker: false,
            reduce_only: true,
            qty_1e6: 2_000,
            qty_dec: 3,
            px_1e6: 64_000_000_000,
            px_dec: 1,
            cid: Part::Cid(&PREFIX, 3, 0x0123_4567_89ab_cdef),
            ts_ms: 1_790_000_000_000,
        };
        let text = |f: &dyn Fn(&mut QueryWriter<'_>)| {
            let mut win = [0u8; 512];
            let mut w = QueryWriter::new(&mut win);
            f(&mut w);
            let n = w.rendered().unwrap();
            String::from_utf8(win[..n].to_vec()).unwrap()
        };
        assert_eq!(
            text(&|w| render_place(w, &st, &o)),
            "symbol=BTCUSDT&side=SELL&type=LIMIT&timeInForce=IOC&reduceOnly=true&quantity=0.002&price=64000.0&newClientOrderId=mv6512ab0f30123456789abcdef00000&newOrderRespType=ACK&selfTradePreventionMode=EXPIRE_MAKER&recvWindow=1000&timestamp=1790000000000"
        );
        assert_eq!(
            text(&|w| render_modify(w, &st, &o)),
            "symbol=BTCUSDT&side=SELL&origClientOrderId=mv6512ab0f30123456789abcdef00000&quantity=0.002&price=64000.0&recvWindow=1000&timestamp=1790000000000"
        );
        assert_eq!(
            text(&|w| render_by_cid(w, &st, b"BTCUSDT", Part::Lit(b"CID"), 5)),
            "symbol=BTCUSDT&origClientOrderId=CID&recvWindow=1000&timestamp=5"
        );
        assert_eq!(text(&|w| render_account(w, &st, 6)), "recvWindow=1000&timestamp=6");
        assert_eq!(
            text(&|w| render_by_order_id(w, &st, b"BTCUSDT", 7_001, 7)),
            "symbol=BTCUSDT&orderId=7001&recvWindow=1000&timestamp=7"
        );
        // HMAC over exactly the rendered bytes.
        let secret = *b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let h = Signer::Hmac(HmacSigner::from_secret(secret).unwrap());
        let mut win = [0u8; 512];
        let mut w = QueryWriter::new(&mut win);
        render_by_cid(&mut w, &st, b"BTCUSDT", Part::Lit(b"CID"), 5);
        let total = w.sign_with(&h).unwrap();
        let q = std::str::from_utf8(&win[..total]).unwrap();
        let (msg, sig) = q.split_once("&signature=").unwrap();
        let want = core_crypto::hmac_sha256(&secret, msg.as_bytes());
        let hex: String = want.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sig, hex);
        // BX7 (zero-copy N1): right at every length around the hash's block
        // and padding bounds — the inner hash pads in the room the
        // signature then takes — and nothing past the signature written.
        let hk = HmacSigner::from_secret(secret).unwrap();
        let mut n = 1;
        while n <= 200 {
            let mut win = [0xEEu8; 300];
            let mut i = 0;
            while i < n {
                win[i] = b'a' + (i % 26) as u8;
                i += 1;
            }
            assert_eq!(sign_hmac_in_place(&hk, &mut win[..n + 75], n), Ok(n + 75));
            assert_eq!(&win[n..n + 11], b"&signature=");
            let mut hex = [0u8; HMAC_SIG_HEX];
            hex_lower(&core_crypto::hmac_sha256(&secret, &win[..n]), &mut hex);
            assert_eq!(&win[n + 11..n + 75], &hex[..], "n {n}");
            assert!(win[n + 75..].iter().all(|b| *b == 0xEE), "n {n}");
            n += 1;
        }
        let mut room = [0u8; 80];
        assert_eq!(sign_hmac_in_place(&hk, &mut room, 6), Err(Overflow), "75 B of room are needed");
        // Too small a window refuses, never truncates.
        let mut tiny = [0u8; 70];
        let mut w = QueryWriter::new(&mut tiny);
        w.put(b"timestamp=").uint(1);
        assert_eq!(w.sign_with(&h), Err(Overflow));
    }

    #[test]
    fn small_answers() {
        assert_eq!(scan_error(br#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#), -1021);
        assert_eq!(scan_error(br#"{"listenKey":"x"}"#), 0);
        // BX7: a code named twice is no code (either order).
        assert_eq!(scan_error(br#"{"code":-2019,"code":0,"msg":"x"}"#), 0);
        assert_eq!(scan_error(br#"{"code":0,"code":-2019,"msg":"x"}"#), 0);
        // ...and a body that is not one whole, well-formed object: no code
        // (the parser review's round-3 R3-1; the WS API's reader refuses
        // each of these alike).
        for bad in [
            &br#"{"code":-2019,"code":}"#[..],
            br#"{"code":-2019,"code""#,
            br#"{"code":-2019,"msg":"x"}{"code":-1003}"#,
            br#"{"code":-2019,"msg":"x"} trailing"#,
            br#"{"code":-2019,"msg":}"#,
        ] {
            assert_eq!(scan_error(bad), 0, "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(scan_error(b"{\"code\":-2019,\"msg\":\"x\"}\r\n"), -2019, "whitespace after it is not bytes");
        assert_eq!(scan_error(b"<html>"), 0);
        let lk = br#"{"listenKey":"pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1"}"#;
        assert_eq!(scan_listen_key(lk).unwrap().get(lk).len(), 64);
        assert_eq!(scan_server_time(br#"{"serverTime":1499827319559}"#), Ok(1_499_827_319_559));
        let cd = br#"{"symbol":"BTCUSDT","countdownTime":"30000"}"#;
        assert_eq!(scan_countdown(cd).unwrap().get(cd), b"BTCUSDT");
        assert_eq!(scan_countdown(br#"{"code":-1102}"#), Err(ScanErr::Missing));
    }

    const ORDERS: &[u8] = br#"[{"avgPrice":"0.00000","clientOrderId":"mvA","cumQuote":"0","executedQty":"0","orderId":1917641,"origQty":"0.40","origType":"LIMIT","price":"0","reduceOnly":false,"side":"BUY","positionSide":"BOTH","status":"NEW","stopPrice":"9300","closePosition":false,"symbol":"BTCUSDT","time":1579276756075,"timeInForce":"GTC","type":"LIMIT","updateTime":1579276756075},{"avgPrice":"65000.1","clientOrderId":"web_x","cumQuote":"650.001","executedQty":"0.01","orderId":2,"origQty":"0.01","price":"65000.1","side":"SELL","status":"FILLED","symbol":"ETHUSDT","updateTime":1}]"#;

    #[test]
    fn orders() {
        let mut out = [OrderRow::default(); 4];
        let n = scan_orders(ORDERS, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!((out[0].venue_oid, out[0].qty_1e6, out[0].status), (1_917_641, 400_000, crate::userstream::S_NEW));
        assert_eq!(out[0].cid.get(ORDERS), b"mvA");
        assert_eq!((out[1].cum_quote_1e6, out[1].side), (650_001_000, core_types::Side::Ask as u8));
        let mut one = [OrderRow::default(); 1];
        assert_eq!(scan_orders(ORDERS, &mut one), Err(ArrErr::Truncated));
        assert_eq!(scan_orders(b"[]", &mut one), Ok(0));
        assert_eq!(scan_orders(br#"[{"symbol":"X"}]"#, &mut one), Err(ArrErr::Scan(ScanErr::Missing)));
    }

    #[test]
    fn trades() {
        let body = br#"[{"buyer":false,"commission":"-0.07819010","commissionAsset":"USDT","id":698759,"maker":false,"orderId":25851813,"price":"7819.01","qty":"0.002","quoteQty":"15.63802","realizedPnl":"-0.91539999","side":"SELL","positionSide":"SHORT","symbol":"BTCUSDT","time":1569514978020}]"#;
        let mut out = [TradeRow::default(); 2];
        assert_eq!(scan_trades(body, &mut out), Ok(1));
        assert_eq!((out[0].trade_id, out[0].venue_oid, out[0].px_1e6, out[0].qty_1e6), (698_759, 25_851_813, 7_819_010_000, 2_000));
        assert_eq!(out[0].symbol.get(body), b"BTCUSDT");
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = scan_error(&bytes);
            if let Ok(s) = scan_listen_key(&bytes) { let _ = s.get(&bytes); }
            let _ = scan_server_time(&bytes);
            let _ = scan_countdown(&bytes);
            let mut o = [OrderRow::default(); 8];
            if let Ok(n) = scan_orders(&bytes, &mut o) {
                for r in &o[..n] { let _ = (r.symbol.get(&bytes), r.cid.get(&bytes)); }
            }
            let mut t = [TradeRow::default(); 8];
            let _ = scan_trades(&bytes, &mut t);
        }
    }
}
