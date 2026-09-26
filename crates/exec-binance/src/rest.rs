// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Signed REST (plan §3.3; BX4 and BX5 carried).** Gateway thread.
//!
//! USDⓈ-M × classic needs REST for what its WS API does not carry: the
//! listenKey, the dead-man (`countdownCancelAll`, BX-17), the sweep's
//! fallback while the order session is down (`DELETE allOpenOrders`), the
//! time, the open orders and the day's trades (recon, obligation 8), and
//! the boot assertions (BX-19).
//!
//! **Signing in place** (BX4 carried): the parameters are rendered
//! contiguously into the connection's own template window
//! ([`QueryWriter`]), the Ed25519 signature is computed over exactly those
//! bytes and appended straight after as `&signature=<percent-encoded
//! base64>` ([`sign_in_place`], one `split_at_mut`, no second buffer).
//! Order-path verbs would use body-form templates; every call here is cold
//! (the query form's per-request header tail is a cold copy, BX5).
//!
//! **Answers** are read in place, fail closed: [`scan_error`] (the
//! venue's `{"code":…,"msg":…}`), [`scan_listen_key`],
//! [`scan_server_time`], [`scan_countdown`], [`scan_orders`] (the open
//! orders) and [`scan_trades`] (the day's trades).

use core_net::{Method, Params, ReqSpec};

use crate::json::{bool_of, dec_1e6, i64_of, str_of, u64_of, Elems, Pairs};
use crate::num::{render_fixed, render_u64, RENDER_MAX};
use crate::userstream::{side_of, status_of, ScanErr, Span};

/// The form content type.
pub const FORM: &str = "application/x-www-form-urlencoded";
/// The API key header.
pub const KEY_HEADER: &str = "X-MBX-APIKEY";

/// `POST /fapi/v1/listenKey`.
pub const T_LISTEN_KEY_NEW: usize = 0;
/// `PUT /fapi/v1/listenKey` (keep-alive).
pub const T_LISTEN_KEY_KEEP: usize = 1;
/// `POST /fapi/v1/countdownCancelAll` (signed).
pub const T_COUNTDOWN: usize = 2;
/// `GET /fapi/v1/time`.
pub const T_TIME: usize = 3;
/// `GET /fapi/v1/openOrders` (signed).
pub const T_OPEN_ORDERS: usize = 4;
/// `GET /fapi/v1/userTrades` (signed).
pub const T_USER_TRADES: usize = 5;
/// `DELETE /fapi/v1/allOpenOrders` (signed): the sweep's fallback (B1).
pub const T_CANCEL_ALL: usize = 6;
/// `GET /fapi/v1/positionSide/dual` (signed).
pub const T_DUAL: usize = 7;
/// `GET /fapi/v1/multiAssetsMargin` (signed).
pub const T_MULTI_ASSETS: usize = 8;
/// The fapi templates.
pub const FAPI_TEMPLATES: usize = 9;

/// `GET /sapi/v1/account/apiRestrictions` (signed).
pub const S_API_RESTRICTIONS: usize = 0;
/// `GET /sapi/v1/portfolio/account` (signed).
pub const S_PORTFOLIO: usize = 1;
/// The sapi templates.
pub const SAPI_TEMPLATES: usize = 2;

/// The fapi connection's templates, every one carrying the key header.
#[must_use]
pub fn fapi_specs<'a>(key: &'a [(&'a str, &'a str)]) -> [ReqSpec<'a>; FAPI_TEMPLATES] {
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
        b(Method::Post, "/fapi/v1/listenKey"),
        b(Method::Put, "/fapi/v1/listenKey"),
        b(Method::Post, "/fapi/v1/countdownCancelAll"),
        q(Method::Get, "/fapi/v1/time"),
        q(Method::Get, "/fapi/v1/openOrders"),
        q(Method::Get, "/fapi/v1/userTrades"),
        q(Method::Delete, "/fapi/v1/allOpenOrders"),
        q(Method::Get, "/fapi/v1/positionSide/dual"),
        q(Method::Get, "/fapi/v1/multiAssetsMargin"),
    ]
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
    [q("/sapi/v1/account/apiRestrictions"), q("/sapi/v1/portfolio/account")]
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
/// is not one.
#[must_use]
pub fn scan_error(body: &[u8]) -> i32 {
    let Ok(mut w) = Pairs::new(body, 0) else {
        return 0;
    };
    let mut code = 0;
    while let Ok(Some((k, a, z))) = w.next() {
        if k == b"code" {
            if let Ok(c) = i64_of(body, a, z) {
                code = c.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
        }
    }
    code
}

/// The venue ms a 418 names in its message (`…IP banned until
/// 1790000600000…`); 0 if it names none. Advisory: it can only lengthen
/// the gateway's quiet, never shorten it.
#[must_use]
pub fn scan_ban_until(body: &[u8]) -> u64 {
    const AT: &[u8] = b"banned until ";
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
            return v;
        }
        i += 1;
    }
    0
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
    }

    use super::*;
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

    #[test]
    fn small_answers() {
        assert_eq!(scan_error(br#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#), -1021);
        assert_eq!(scan_error(br#"{"listenKey":"x"}"#), 0);
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
