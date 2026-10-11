// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The WS API session (plan §3.3, §3.8; BX-6, BX-10).** Gateway thread.
//!
//! The USDⓈ-M WS API (`ws-fapi`) is order entry for UM × classic: one
//! socket, authenticated ONCE by `session.logon` (Ed25519 over the sorted
//! parameters `apiKey=…&timestamp=…`), after which every request carries
//! only its own parameters, a `timestamp` and the `recvWindow`.
//!
//! **Rendering.** Each request is written as ONE text frame straight into
//! the socket's send window ([`FrameSink::queue_parts`]): the static bytes
//! are literals; per order only the request id, the 32-character client
//! id, the price and quantity digits and the timestamp change, and each is
//! RENDERED IN THE FRAME ([`Part`]: its length is known first, so the
//! header goes first) — nothing is staged on the stack and copied in; the
//! frame is then masked in place. Every order requests
//! `newOrderRespType=ACK` (BX-2: the answer is the ACK, the user stream
//! the FILL) and an explicit `selfTradePreventionMode` (UM refuses
//! `NONE`).
//!
//! **The answer** ([`scan_answer`]) is read in place: `id`, `status`, the
//! `result` object's `orderId` / `status` / `executedQty` / `avgPrice` /
//! `clientOrderId`, or the `error` object's `code`. Fail closed: a frame
//! with no `id` or no `status` is an `Err` (BX-15), never a guess.
//!
//! **Maybe-sent** (BX5 carried): a request is in flight from the socket's
//! `flush` onward; a connection that dies with requests in flight leaves
//! them IN DOUBT, resolved by `order.status`, never resent (BX-11).
//!
//! **Spot (BX8)** speaks the same requests on `ws-api/v3` — the place
//! without `reduceOnly` (spot has none; the arm refuses one), the account
//! read `account.status` with `omitZeroBalances` — and its session
//! subscribes to the user data after the logon
//! ([`queue_subscribe`]): the events then share the socket with the
//! answers, each framed `{"subscriptionId":N,"event":{…}}`, which
//! [`scan_answer`] tells from an answer and hands on whole
//! ([`WsAnswer::user`], [`WsAnswer::event`]). A spot order's answer has no
//! `avgPrice`: its average is `cummulativeQuoteQty / executedQty`.

use core_net::{WsConn, WsErr, WsPart};

use crate::cid::{CidPrefix, CID_LEN};
use crate::inst::{PRODUCT_COINM, PRODUCT_SPOT};
use crate::json::{dec_1e6, i64_of, is_null, str_of, u64_of, Pairs};
use crate::num::{fixed_len, render_fixed, render_u64, u64_len};
use crate::userstream::{status_of, ScanErr, Span};

/// `session.logon`.
pub const REQ_LOGON: u8 = 1;
/// `order.place`.
pub const REQ_PLACE: u8 = 2;
/// `order.cancel`.
pub const REQ_CANCEL: u8 = 3;
/// `order.modify`.
pub const REQ_MODIFY: u8 = 4;
/// `order.status` (an in-doubt order, BX-11).
pub const REQ_STATUS: u8 = 5;
/// `v2/account.status` (recon and margin).
pub const REQ_ACCOUNT: u8 = 6;
/// `userDataStream.subscribe` (BX8, spot): the session's user data, after
/// its logon.
pub const REQ_SUBSCRIBE: u8 = 7;

/// What a WS API request id stands for: the verb, the open-order slot, the
/// slot's generation and the placement id of the order it was sent for —
/// so an answer arriving after its order ended is never applied to the
/// slot's next order, even one placed under the same id.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WsReq {
    /// The order's placement id (0 for requests not about an order).
    pub oid: u64,
    /// The slot's generation when the request was sent.
    pub gen: u32,
    /// The open-order-table slot (verbs on an order).
    pub ix: u16,
    /// `REQ_*`.
    pub verb: u8,
}

impl WsReq {
    /// A request of `verb` for the order `oid` in slot `ix` of generation
    /// `gen`.
    #[inline(always)]
    #[must_use]
    pub const fn on(verb: u8, ix: u16, gen: u32, oid: u64) -> Self {
        Self { oid, gen, ix, verb }
    }
}

/// **One piece of a request**, rendered straight into the frame
/// ([`WsPart`]): a literal is the serialiser's own write; a number's
/// digits and our client id are rendered in place — nothing is staged.
#[derive(Copy, Clone)]
pub enum Part<'a> {
    /// Bytes as they are (a literal, the key, a signature, a venue's id).
    Lit(&'a [u8]),
    /// An unsigned integer's digits.
    U64(u64),
    /// A ×1e6 value (≥ 0, on its unit) with this many decimals.
    Fixed(i64, u8),
    /// The client id of (slot, member id) under this boot's prefix.
    Cid(&'a CidPrefix, u8, u64),
}

impl WsPart for Part<'_> {
    #[inline(always)]
    fn part_len(&self) -> usize {
        match *self {
            Part::Lit(b) => b.len(),
            Part::U64(v) => u64_len(v),
            Part::Fixed(v, dec) => fixed_len(v, dec),
            Part::Cid(..) => CID_LEN,
        }
    }

    #[inline(always)]
    fn write_to(&self, dst: &mut [u8]) -> usize {
        match *self {
            Part::Lit(b) => {
                // COPY: a literal, the key, a signature or a venue's id
                // (≤ the part; ≤ the frame's payload in all) into its
                // destination — the frame (the serialiser's own write,
                // masked in place after), the REST order route's request
                // window (BX7: `QueryWriter::parts`, the final bytes,
                // signed in place) or the logon's signer input
                // (`render_parts`, cold) — rejected: none, the destination
                // is where the bytes go.
                dst.copy_from_slice(b);
                b.len()
            }
            Part::U64(v) => render_u64(v, dst),
            Part::Fixed(v, dec) => render_fixed(v, dec, dst),
            Part::Cid(prefix, slot, oid) => {
                // `dst` is exactly `CID_LEN` long (the serialiser's law).
                if let Ok(out) = <&mut [u8; CID_LEN]>::try_from(dst) {
                    prefix.render(slot, oid, out);
                    CID_LEN
                } else {
                    debug_assert!(false, "a client id's window is not CID_LEN");
                    0
                }
            }
        }
    }
}

/// Where a rendered frame goes: the socket's send window (or a test's
/// buffer). Monomorphized, never `dyn`.
pub trait FrameSink {
    /// Render `parts` into one text frame.
    fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), WsErr>;
}

impl FrameSink for WsConn {
    #[inline(always)]
    fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), WsErr> {
        self.queue_text_with(parts)
    }
}

/// Render `parts` as plain text into `dst` — the REST order route's request
/// window (BX7, through `QueryWriter::parts`: the final bytes, signed in
/// place), the logon's signer input, a test's or the boot vectors' buffer:
/// what the frame carries, unmasked. The length, or `Overflow` — refused
/// whole, never truncated.
pub fn render_parts(parts: &[Part<'_>], dst: &mut [u8]) -> Result<usize, WsErr> {
    let mut n = 0;
    let mut i = 0;
    while i < parts.len() {
        let k = parts[i].part_len();
        if n + k > dst.len() {
            return Err(WsErr::Overflow);
        }
        let wrote = parts[i].write_to(&mut dst[n..n + k]);
        debug_assert!(wrote == k, "a part wrote other than its length");
        n += k;
        i += 1;
    }
    Ok(n)
}

/// The boot-fixed request parameters.
#[derive(Copy, Clone)]
pub struct WsStatic {
    /// `selfTradePreventionMode` — empty where the endpoint takes none
    /// (papi's COIN-M order, BX7): the parameter is then not rendered.
    pub stp: &'static [u8],
    /// `recvWindow`, ms.
    pub recv_window_ms: u64,
}

impl WsStatic {
    /// From the `exec.toml` words.
    #[must_use]
    pub fn new(stp_mode: &str, recv_window_ms: u64) -> Self {
        let stp: &'static [u8] = match stp_mode {
            "EXPIRE_MAKER" => b"EXPIRE_MAKER",
            "EXPIRE_BOTH" => b"EXPIRE_BOTH",
            _ => b"EXPIRE_TAKER",
        };
        Self { stp, recv_window_ms }
    }

    /// The same, rendering no STP mode (papi's COIN-M order takes none).
    #[must_use]
    pub const fn without_stp(self) -> Self {
        Self {
            stp: b"",
            recv_window_ms: self.recv_window_ms,
        }
    }
}

/// `BUY` / `SELL` (the WS API's and the REST route's word).
#[inline(always)]
pub(crate) const fn side_word(side: u8) -> &'static [u8] {
    if side == core_types::Side::Ask as u8 {
        b"SELL"
    } else {
        b"BUY"
    }
}

/// **`session.logon`** — signed once per connection (BX4 carried: the
/// signer is the process's one, built at boot).
pub fn queue_logon<S: FrameSink>(
    s: &mut S,
    id: u64,
    api_key: &[u8],
    ts_ms: u64,
    signer: &signer_ed25519::Ed25519Signer,
) -> Result<(), WsErr> {
    // COPY: the sorted payload `apiKey=<key>&timestamp=<ts>` (≤ 192 B) onto
    // the stack — signer input, cold (once per connection, BX4 carried);
    // the frame is JSON and the payload a query: one cannot be the other.
    let mut payload = [0u8; 192];
    let n = render_parts(&[Part::Lit(b"apiKey="), Part::Lit(api_key), Part::Lit(b"&timestamp="), Part::U64(ts_ms)], &mut payload)?;
    // COPY: the signature (88 B of base64) is signed onto the stack, then
    // written into the frame by its `Part::Lit` — cold, once per
    // connection — rejected: a signing part (the one signer's API writes
    // into a caller's buffer; a second path for one call).
    let mut sig = [0u8; signer_ed25519::SIG_B64_LEN];
    let sn = signer.sign_b64(&payload[..n], &mut sig);
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"session.logon\",\"params\":{\"apiKey\":\""),
        Part::Lit(api_key),
        Part::Lit(b"\",\"signature\":\""),
        Part::Lit(&sig[..sn]),
        Part::Lit(b"\",\"timestamp\":"),
        Part::U64(ts_ms),
        Part::Lit(b"}}"),
    ])
}

/// One order to place.
#[derive(Copy, Clone)]
pub struct Place<'a> {
    /// The wire symbol.
    pub symbol: &'a [u8],
    /// `Side` as its `u8`.
    pub side: u8,
    /// A maker (`GTX`, post-only) or an IoC.
    pub maker: bool,
    /// `reduceOnly=true` (BX7: `ORDER_FLAG_REDUCE_ONLY`; one-way mode is
    /// asserted at boot, so the venue accepts it). Not rendered by a modify,
    /// which keeps the order's own.
    pub reduce_only: bool,
    /// Quantity ×1e6 and its decimals.
    pub qty_1e6: i64,
    /// The step's decimals.
    pub qty_dec: u8,
    /// Price ×1e6.
    pub px_1e6: i64,
    /// The tick's decimals.
    pub px_dec: u8,
    /// The client order id ([`Part::Cid`]).
    pub cid: Part<'a>,
    /// The venue timestamp, ms.
    pub ts_ms: u64,
}

/// **`order.place`** (plan §3.8: UM `LIMIT` + `GTX` for a maker, `LIMIT` +
/// `IOC` otherwise; `newOrderRespType=ACK`; explicit STP).
#[inline]
pub fn queue_place<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, o: &Place<'_>) -> Result<(), WsErr> {
    let tif: &[u8] = if o.maker { b"GTX" } else { b"IOC" };
    let ro: &[u8] = if o.reduce_only { b"\",\"reduceOnly\":\"true" } else { b"" };
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"order.place\",\"params\":{\"symbol\":\""),
        Part::Lit(o.symbol),
        Part::Lit(b"\",\"side\":\""),
        Part::Lit(side_word(o.side)),
        Part::Lit(b"\",\"type\":\"LIMIT\",\"timeInForce\":\""),
        Part::Lit(tif),
        Part::Lit(ro),
        Part::Lit(b"\",\"quantity\":\""),
        Part::Fixed(o.qty_1e6, o.qty_dec),
        Part::Lit(b"\",\"price\":\""),
        Part::Fixed(o.px_1e6, o.px_dec),
        Part::Lit(b"\",\"newClientOrderId\":\""),
        o.cid,
        Part::Lit(b"\",\"newOrderRespType\":\"ACK"),
        Part::Lit(if st.stp.is_empty() { b"" } else { b"\",\"selfTradePreventionMode\":\"" }),
        Part::Lit(st.stp),
        Part::Lit(b"\",\"recvWindow\":"),
        Part::U64(st.recv_window_ms),
        Part::Lit(b",\"timestamp\":"),
        Part::U64(o.ts_ms),
        Part::Lit(b"}}"),
    ])
}

/// **`order.cancel`** by the client id the venue holds ([`Part::Cid`] for
/// ours, [`Part::Lit`] for an id the venue listed).
#[inline]
pub fn queue_cancel<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, symbol: &[u8], cid: Part<'_>, ts_ms: u64) -> Result<(), WsErr> {
    queue_by_cid(s, id, st, b"order.cancel", symbol, cid, ts_ms)
}

/// **`order.status`** — the in-doubt query (BX-11).
#[inline]
pub fn queue_status<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, symbol: &[u8], cid: Part<'_>, ts_ms: u64) -> Result<(), WsErr> {
    queue_by_cid(s, id, st, b"order.status", symbol, cid, ts_ms)
}

fn queue_by_cid<S: FrameSink>(
    s: &mut S,
    id: u64,
    st: &WsStatic,
    method: &[u8],
    symbol: &[u8],
    cid: Part<'_>,
    ts_ms: u64,
) -> Result<(), WsErr> {
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\""),
        Part::Lit(method),
        Part::Lit(b"\",\"params\":{\"symbol\":\""),
        Part::Lit(symbol),
        Part::Lit(b"\",\"origClientOrderId\":\""),
        cid,
        Part::Lit(b"\",\"recvWindow\":"),
        Part::U64(st.recv_window_ms),
        Part::Lit(b",\"timestamp\":"),
        Part::U64(ts_ms),
        Part::Lit(b"}}"),
    ])
}

/// **`order.modify`** (UM: price and quantity together; the venue keeps
/// the order's client id; a quantity at or under the executed one, or a
/// GTX price that would cross, CANCELS the order — plan §3.8). `o.maker`
/// is not rendered: a modify keeps the order's time in force.
#[inline]
pub fn queue_modify<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, o: &Place<'_>) -> Result<(), WsErr> {
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"order.modify\",\"params\":{\"symbol\":\""),
        Part::Lit(o.symbol),
        Part::Lit(b"\",\"side\":\""),
        Part::Lit(side_word(o.side)),
        Part::Lit(b"\",\"origClientOrderId\":\""),
        o.cid,
        Part::Lit(b"\",\"quantity\":\""),
        Part::Fixed(o.qty_1e6, o.qty_dec),
        Part::Lit(b"\",\"price\":\""),
        Part::Fixed(o.px_1e6, o.px_dec),
        Part::Lit(b"\",\"recvWindow\":"),
        Part::U64(st.recv_window_ms),
        Part::Lit(b",\"timestamp\":"),
        Part::U64(o.ts_ms),
        Part::Lit(b"}}"),
    ])
}

/// The account read's method: USDⓈ-M's `v2/account.status`, COIN-M's
/// `account.status` (BX7: per-asset margins, positions in contracts),
/// spot's `account.status` (BX8: the balances, the permissions).
#[inline(always)]
#[must_use]
pub const fn account_method(product: u8) -> &'static [u8] {
    if product == PRODUCT_COINM || product == PRODUCT_SPOT {
        b"account.status"
    } else {
        b"v2/account.status"
    }
}

/// The account read's parameters before `recvWindow`: spot's leave the
/// zero balances out (BX8: a Demo account lists hundreds).
#[inline(always)]
const fn account_params(product: u8) -> &'static [u8] {
    if product == PRODUCT_SPOT {
        b"\",\"params\":{\"omitZeroBalances\":true,\"recvWindow\":"
    } else {
        b"\",\"params\":{\"recvWindow\":"
    }
}

/// **The account read** ([`account_method`]) — positions, balances and the
/// margin figures (recon, BX-20).
pub fn queue_account<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, product: u8, ts_ms: u64) -> Result<(), WsErr> {
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\""),
        Part::Lit(account_method(product)),
        Part::Lit(account_params(product)),
        Part::U64(st.recv_window_ms),
        Part::Lit(b",\"timestamp\":"),
        Part::U64(ts_ms),
        Part::Lit(b"}}"),
    ])
}

/// **The subscription's answer** (BX8, spot): its `result` names a
/// `subscriptionId` — once, an unsigned integer. A 200 without one is not
/// the subscription it answers: the session never goes live on it.
#[must_use]
pub fn scan_subscribed(f: &[u8], result: Span) -> bool {
    let Ok(mut r) = Pairs::new(f, result.at as usize) else {
        return false;
    };
    let mut id = None;
    loop {
        match r.next() {
            Ok(Some((b"subscriptionId", a, z))) => {
                if id.is_some() {
                    return false;
                }
                id = u64_of(f, a, z).ok();
                if id.is_none() {
                    return false;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return id.is_some(),
            Err(()) => return false,
        }
    }
}

/// **`userDataStream.subscribe`** (BX8, spot): the logged-on session's
/// user data on the session itself — no parameters (the logon names the
/// account).
pub fn queue_subscribe<S: FrameSink>(s: &mut S, id: u64) -> Result<(), WsErr> {
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"userDataStream.subscribe\"}"),
    ])
}

// -------------------------------------------------------------------------
// The answer
// -------------------------------------------------------------------------

/// One scanned WS API answer.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct WsAnswer {
    /// The request id (0 for `null`).
    pub id: u64,
    /// `result.orderId` (0 if none).
    pub venue_oid: u64,
    /// `result.executedQty` ×1e6.
    pub executed_1e6: i64,
    /// `result.avgPrice` ×1e6.
    pub avg_px_1e6: i64,
    /// `result.origQty` ×1e6.
    pub qty_1e6: i64,
    /// `result.price` ×1e6.
    pub px_1e6: i64,
    /// The `result` value's span (the account answer is read from it).
    pub result: Span,
    /// `result.clientOrderId`.
    pub cid: Span,
    /// `error.code` (0 = none).
    pub code: i32,
    /// `status` (HTTP-like).
    pub status: u16,
    /// `result.status` as `S_*` (0 if none).
    pub order_status: u8,
    /// A `serverShutdown` event: move to a new connection now.
    pub shutdown: bool,
    /// `error.data.retryAfter`: venue ms until which a budget or lock
    /// answer asks for silence (0 = not said).
    pub retry_after_ms: u64,
    /// A spot user event's frame (BX8): `subscriptionId` with an `event`,
    /// no `id`, no `status` — [`Self::event`] is the event object.
    pub user: bool,
    /// The `event` object's span (a user event's, or `serverShutdown`'s).
    pub event: Span,
}

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// The keys a WS API answer is read by, a bit each (BX7, the one-reading
/// law: a second copy is refused — a `status` 400 after a 200 would retire
/// an order the venue holds; a second `result` would mix two orders).
const A_ID: u8 = 1 << 0;
const A_STATUS: u8 = 1 << 1;
const A_RESULT: u8 = 1 << 2;
const A_ERROR: u8 = 1 << 3;
const A_EVENT: u8 = 1 << 4;
const A_SUB: u8 = 1 << 5;

/// **Scan one WS API frame** (module docs).
pub fn scan_answer(f: &[u8], out: &mut WsAnswer) -> Result<(), ScanErr> {
    *out = WsAnswer::default();
    let mut w = Pairs::new(f, 0).map_err(malformed)?;
    let mut seen = 0u8;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        let bit = match k {
            b"id" => {
                out.id = if is_null(f, a, z) { 0 } else { u64_of(f, a, z).map_err(bad)? };
                A_ID
            }
            b"status" => {
                let s = u64_of(f, a, z).map_err(bad)?;
                if s > 999 {
                    return Err(ScanErr::BadField);
                }
                out.status = s as u16;
                A_STATUS
            }
            b"result" => {
                out.result = Span {
                    at: a as u32,
                    len: (z - a) as u32,
                };
                if f.get(a) == Some(&b'{') {
                    result_fields(f, a, out)?;
                }
                A_RESULT
            }
            b"error" => {
                let mut e = Pairs::new(f, a).map_err(malformed)?;
                let mut coded = false;
                while let Some((ek, ea, ez)) = e.next().map_err(malformed)? {
                    if ek == b"code" {
                        // Read once: a second code is refused.
                        if coded {
                            return Err(ScanErr::Malformed);
                        }
                        coded = true;
                        let c = i64_of(f, ea, ez).map_err(bad)?;
                        out.code = c.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    } else if ek == b"data" && f.get(ea) == Some(&b'{') {
                        // Advisory: a `retryAfter` it cannot read leaves the
                        // default quiet in force (never a shorter one), and
                        // of two copies the later end stands.
                        let mut d = Pairs::new(f, ea).map_err(malformed)?;
                        while let Some((dk, da, dz)) = d.next().map_err(malformed)? {
                            if dk == b"retryAfter" {
                                out.retry_after_ms = out.retry_after_ms.max(u64_of(f, da, dz).unwrap_or(0));
                            }
                        }
                    }
                }
                A_ERROR
            }
            // The object is read below, once the frame says whose it is.
            b"event" => {
                if f.get(a) != Some(&b'{') {
                    return Err(ScanErr::BadField);
                }
                out.event = Span {
                    at: a as u32,
                    len: (z - a) as u32,
                };
                A_EVENT
            }
            // Spot's user data (BX8): the subscription the event is for.
            b"subscriptionId" => {
                let _ = u64_of(f, a, z).map_err(bad)?;
                A_SUB
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    // One object, whole (BX8, the parser review): bytes after it — a second
    // frame run on — are no shape of the venue's, never left unread.
    if !crate::rest::ends_whole(f, w.end()) {
        return Err(ScanErr::Malformed);
    }
    if seen & A_SUB != 0 {
        // A spot user event (BX8): its event and nothing of an answer's —
        // the event is the caller's to read ([`crate::userstream::scan_spot_event`]),
        // once.
        if seen & (A_ID | A_STATUS | A_RESULT | A_ERROR) != 0 || seen & A_EVENT == 0 {
            return Err(ScanErr::Malformed);
        }
        out.user = true;
        return Ok(());
    }
    if seen & A_EVENT != 0 {
        // A venue event outside a subscription: its name, read once — a
        // second is refused.
        let mut e = Pairs::new(f, out.event.at as usize).map_err(malformed)?;
        let mut named = false;
        while let Some((ek, ea, ez)) = e.next().map_err(malformed)? {
            if ek == b"e" {
                if named {
                    return Err(ScanErr::Malformed);
                }
                named = true;
                out.shutdown = str_of(f, ea, ez).map_err(bad)? == b"serverShutdown";
            }
        }
    }
    // An order's result AND a refusal in one answer is no shape of the
    // venue's: neither is read as the other's (BX7).
    if seen & (A_RESULT | A_ERROR) == A_RESULT | A_ERROR {
        return Err(ScanErr::Malformed);
    }
    if out.shutdown {
        return Ok(());
    }
    if seen & (A_ID | A_STATUS) != A_ID | A_STATUS {
        return Err(ScanErr::Missing);
    }
    if out.status != 200 && out.code == 0 {
        // Anything but a 200 must say why (BX7: a code-less 204 or 3xx read
        // as a refusal would retire an order the venue may hold); one that
        // does not is not a shape we know.
        return Err(ScanErr::Missing);
    }
    Ok(())
}

/// **A REST order-route answer read as a WS API one** (BX7, C5): the same
/// judges take both. A 200's body IS the order (or the account) — the
/// `result` — and is ONE JSON object, the whole body (whitespace after it
/// allowed): bytes past it are refused, never read past; any other
/// status's body is the venue's `{"code","msg"}`, which a 4xx must carry
/// (a refusal says why) and a 5xx may not (its status alone says
/// "unknown", BX-11). A 418's or 429's ban end rides `retry_after_ms`. The
/// request id is the caller's.
pub fn scan_rest_answer(status: u16, body: &[u8], out: &mut WsAnswer) -> Result<(), ScanErr> {
    *out = WsAnswer::default();
    out.status = status;
    if status == 200 {
        let end = core_parse::skip_json_value(body, 0).ok_or(ScanErr::Malformed)?;
        if body.first() != Some(&b'{') || core_parse::skip_ws(body, end) != body.len() {
            return Err(ScanErr::Malformed);
        }
        out.result = Span {
            at: 0,
            len: body.len() as u32,
        };
        return result_fields(body, 0, out);
    }
    out.code = crate::rest::scan_error(body);
    if status == 418 || status == 429 {
        out.retry_after_ms = crate::rest::scan_ban_until(body);
    }
    if out.code == 0 && status < 500 {
        return Err(ScanErr::Missing);
    }
    Ok(())
}

/// An order's fields in a `result` (or the REST route's 200 body), each
/// key read at most once (BX7: a second copy is refused).
fn result_fields(f: &[u8], at: usize, out: &mut WsAnswer) -> Result<(), ScanErr> {
    let mut r = Pairs::new(f, at).map_err(malformed)?;
    let mut seen = 0u8;
    // Spot's cumulative quote (BX8): its average's numerator; negative
    // when the venue does not have it.
    let mut quote_1e6 = 0i64;
    while let Some((k, a, z)) = r.next().map_err(malformed)? {
        let bit: u8 = match k {
            b"orderId" => {
                out.venue_oid = u64_of(f, a, z).map_err(bad)?;
                1 << 0
            }
            b"status" => {
                out.order_status = status_of(str_of(f, a, z).map_err(bad)?);
                1 << 1
            }
            b"executedQty" => {
                out.executed_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                1 << 2
            }
            b"avgPrice" => {
                out.avg_px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                1 << 3
            }
            // eapi names it `quantity` (BX9): one or the other, read once.
            b"origQty" | b"quantity" => {
                out.qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                1 << 4
            }
            b"price" => {
                out.px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                1 << 5
            }
            b"clientOrderId" => {
                out.cid = Span::of(f, str_of(f, a, z).map_err(bad)?);
                1 << 6
            }
            b"cummulativeQuoteQty" => {
                quote_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                1 << 7
            }
            _ => 0,
        };
        if seen & bit != 0 {
            return Err(ScanErr::Malformed);
        }
        seen |= bit;
    }
    // Spot (BX8): no `avgPrice` — the average is the cumulative quote over
    // the executed quantity, when the venue has both.
    if seen & (1 << 3) == 0 && quote_1e6 > 0 && out.executed_1e6 > 0 {
        out.avg_px_1e6 = (quote_1e6 as i128 * 1_000_000 / out.executed_1e6 as i128).min(i64::MAX as i128) as i64;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A sink that keeps the frame's plain text.
    #[derive(Default)]
    pub(crate) struct Text(pub std::vec::Vec<u8>);

    impl FrameSink for Text {
        fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), WsErr> {
            let mut b = [0u8; 1_024];
            let n = render_parts(parts, &mut b)?;
            self.0.extend_from_slice(&b[..n]);
            Ok(())
        }
    }

    const PREFIX: CidPrefix = CidPrefix::new(0x6512_ab0f);

    fn st() -> WsStatic {
        WsStatic::new("EXPIRE_TAKER", 1_000)
    }

    /// BX7 (C5): a REST answer reads as the WS API's — the order fields of
    /// a 200, the code of a refusal, the ban of a 418; a 4xx without a code
    /// is not a shape we know, a 5xx without one is "unknown" (BX-11).
    #[test]
    fn a_rest_answer_reads_as_a_ws_one() {
        let mut a = WsAnswer::default();
        let ok = br#"{"orderId":8001,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.0","avgPrice":"0.00","origQty":"0.002","executedQty":"0"}"#;
        scan_rest_answer(200, ok, &mut a).unwrap();
        assert_eq!((a.status, a.venue_oid, a.qty_1e6, a.code), (200, 8_001, 2_000, 0));
        assert_eq!(a.cid.get(ok), b"mv6512ab0f2000000000000006500000");
        assert_eq!(a.result.get(ok), &ok[..]);
        scan_rest_answer(400, br#"{"code":-2019,"msg":"Margin is insufficient."}"#, &mut a).unwrap();
        assert_eq!((a.status, a.code), (400, -2019));
        scan_rest_answer(418, br#"{"code":-1003,"msg":"IP banned until 1790000600000."}"#, &mut a).unwrap();
        assert_eq!((a.code, a.retry_after_ms), (-1003, 1_790_000_600_000));
        assert_eq!(scan_rest_answer(400, b"oops", &mut a), Err(ScanErr::Missing));
        scan_rest_answer(502, b"<html>bad gateway</html>", &mut a).unwrap();
        assert_eq!((a.status, a.code), (502, 0));
        assert_eq!(scan_rest_answer(200, b"[]", &mut a), Err(ScanErr::Malformed));
    }

    #[test]
    fn place_renders_the_planned_request() {
        let mut t = Text::default();
        let o = Place {
            symbol: b"BTCUSDT",
            side: core_types::Side::Bid as u8,
            maker: true,
            qty_1e6: 12_000,
            qty_dec: 3,
            px_1e6: 65_000_100_000,
            px_dec: 1,
            cid: Part::Cid(&PREFIX, 3, 0x0123_4567_89ab_cdef),
            ts_ms: 1_790_000_000_000,
            reduce_only: false,
        };
        queue_place(&mut t, 42, &st(), &o).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":42,"method":"order.place","params":{"symbol":"BTCUSDT","side":"BUY","type":"LIMIT","timeInForce":"GTX","quantity":"0.012","price":"65000.1","newClientOrderId":"mv6512ab0f30123456789abcdef00000","newOrderRespType":"ACK","selfTradePreventionMode":"EXPIRE_TAKER","recvWindow":1000,"timestamp":1790000000000}}"#
        );
        // BX7: a reduce-only IoC says so.
        let mut t = Text::default();
        let ro = Place { maker: false, reduce_only: true, ..o };
        queue_place(&mut t, 43, &st(), &ro).unwrap();
        assert!(std::str::from_utf8(&t.0).unwrap().contains(r#""timeInForce":"IOC","reduceOnly":"true","quantity":"0.012""#));
    }

    #[test]
    fn cancel_modify_status_account_render() {
        let mut t = Text::default();
        queue_cancel(&mut t, 7, &st(), b"BTCUSDT", Part::Lit(b"CID"), 5).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":7,"method":"order.cancel","params":{"symbol":"BTCUSDT","origClientOrderId":"CID","recvWindow":1000,"timestamp":5}}"#
        );
        let mut t = Text::default();
        let m = Place {
            symbol: b"BTCUSDT",
            side: core_types::Side::Ask as u8,
            maker: true,
            qty_1e6: 1_000,
            qty_dec: 3,
            px_1e6: 65_000_000_000,
            px_dec: 1,
            cid: Part::Lit(b"CID"),
            ts_ms: 6,
            reduce_only: true,
        };
        queue_modify(&mut t, 8, &st(), &m).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":8,"method":"order.modify","params":{"symbol":"BTCUSDT","side":"SELL","origClientOrderId":"CID","quantity":"0.001","price":"65000.0","recvWindow":1000,"timestamp":6}}"#
        );
        let mut t = Text::default();
        queue_status(&mut t, 9, &st(), b"BTCUSDT", Part::Cid(&PREFIX, 3, 0x0123_4567_89ab_cdef), 7).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":9,"method":"order.status","params":{"symbol":"BTCUSDT","origClientOrderId":"mv6512ab0f30123456789abcdef00000","recvWindow":1000,"timestamp":7}}"#
        );
        let mut t = Text::default();
        queue_account(&mut t, 10, &st(), crate::inst::PRODUCT_USDM, 8).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":10,"method":"v2/account.status","params":{"recvWindow":1000,"timestamp":8}}"#
        );
        let mut t = Text::default();
        queue_account(&mut t, 11, &st(), PRODUCT_COINM, 8).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":11,"method":"account.status","params":{"recvWindow":1000,"timestamp":8}}"#
        );
        // BX8: spot's account read leaves its zero balances out; its
        // session subscribes to the user data with no parameters.
        let mut t = Text::default();
        queue_account(&mut t, 12, &st(), PRODUCT_SPOT, 8).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":12,"method":"account.status","params":{"omitZeroBalances":true,"recvWindow":1000,"timestamp":8}}"#
        );
        let mut t = Text::default();
        queue_subscribe(&mut t, 13).unwrap();
        assert_eq!(std::str::from_utf8(&t.0).unwrap(), r#"{"id":13,"method":"userDataStream.subscribe"}"#);
    }

    /// BX8: a spot user event shares the order socket — read as one by its
    /// `subscriptionId`, its event handed on whole and unread; never mixed
    /// with an answer's keys. A spot order's answer has no `avgPrice`: the
    /// average is its cumulative quote over its executed quantity.
    #[test]
    fn spot_user_events_are_told_from_answers() {
        let mut a = WsAnswer::default();
        let ev = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1,"s":"BTCUSDT"}}"#;
        scan_answer(ev, &mut a).unwrap();
        assert!(a.user && !a.shutdown);
        assert_eq!(a.event.get(ev), &br#"{"e":"executionReport","E":1,"s":"BTCUSDT"}"#[..]);
        // Key order is not relied on.
        let rev = br#"{"event":{"e":"balanceUpdate"},"subscriptionId":3}"#;
        scan_answer(rev, &mut a).unwrap();
        assert!(a.user);
        for bad in [
            &br#"{"subscriptionId":0,"event":{"e":"x"},"id":7,"status":200}"#[..],
            &br#"{"subscriptionId":0,"id":7,"status":200,"result":{}}"#[..],
            &br#"{"subscriptionId":0,"subscriptionId":1,"event":{"e":"x"}}"#[..],
            &br#"{"subscriptionId":0,"event":{"e":"x"},"event":{"e":"y"}}"#[..],
            &br#"{"subscriptionId":0,"event":[1]}"#[..],
            &br#"{"subscriptionId":"zero","event":{"e":"x"}}"#[..],
        ] {
            assert!(scan_answer(bad, &mut a).is_err(), "{}", std::str::from_utf8(bad).unwrap());
        }
        // The shutdown event (no subscription) is still the WS API's.
        scan_answer(br#"{"event":{"e":"serverShutdown","E":1}}"#, &mut a).unwrap();
        assert!(a.shutdown && !a.user);
        let ok = br#"{"id":5,"status":200,"result":{"symbol":"BTCUSDT","orderId":4293153,"orderListId":-1,"clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.01000000","origQty":"0.00200000","executedQty":"0.00100000","cummulativeQuoteQty":"65.00000000","status":"EXPIRED","timeInForce":"IOC","type":"LIMIT","side":"BUY"}}"#;
        scan_answer(ok, &mut a).unwrap();
        assert_eq!((a.executed_1e6, a.avg_px_1e6, a.order_status), (1_000, 65_000_000_000, crate::userstream::S_EXPIRED));
        let unknown = std::string::String::from_utf8(ok.to_vec()).unwrap().replace("\"65.00000000\"", "\"-1.00000000\"");
        scan_answer(unknown.as_bytes(), &mut a).unwrap();
        assert_eq!(a.avg_px_1e6, 0, "a negative quote is unavailable");
        let twice = std::string::String::from_utf8(ok.to_vec()).unwrap().replace(r#""status":"EXPIRED","#, r#""status":"EXPIRED","cummulativeQuoteQty":"1","#);
        assert_eq!(scan_answer(twice.as_bytes(), &mut a), Err(ScanErr::Malformed));
        // BX8 parser review: a frame is one object, whole.
        let run_on = br#"{"subscriptionId":0,"event":{"e":"x"}}{"subscriptionId":0,"event":{"e":"y"}}"#;
        assert_eq!(scan_answer(run_on, &mut a), Err(ScanErr::Malformed));
        assert_eq!(scan_answer(br#"{"id":1,"status":200,"result":{}} x"#, &mut a), Err(ScanErr::Malformed));
        scan_answer(b"{\"id\":1,\"status\":200,\"result\":{}}\n", &mut a).unwrap();
        // The subscription's answer names its id, once.
        let sub = br#"{"id":2,"status":200,"result":{"subscriptionId":0}}"#;
        scan_answer(sub, &mut a).unwrap();
        assert!(scan_subscribed(sub, a.result));
        for bad in [
            &br#"{"id":2,"status":200,"result":{}}"#[..],
            &br#"{"id":2,"status":200,"result":{"subscriptionId":0,"subscriptionId":1}}"#[..],
            &br#"{"id":2,"status":200,"result":{"subscriptionId":"x"}}"#[..],
            &br#"{"id":2,"status":200,"result":[]}"#[..],
        ] {
            scan_answer(bad, &mut a).unwrap();
            assert!(!scan_subscribed(bad, a.result), "{}", std::str::from_utf8(bad).unwrap());
        }
        // The ACK (spot's has no status) names the order.
        let ack = br#"{"id":6,"status":200,"result":{"symbol":"BTCUSDT","orderId":4293153,"orderListId":-1,"clientOrderId":"mv6512ab0f2000000000000006500000","transactTime":1790000000099}}"#;
        scan_answer(ack, &mut a).unwrap();
        assert_eq!((a.venue_oid, a.order_status), (4_293_153, 0));
        assert_eq!(a.cid.get(ack), &b"mv6512ab0f2000000000000006500000"[..]);
    }

    #[test]
    fn logon_signs_the_sorted_payload() {
        let seed = [9u8; 32];
        let signer = signer_ed25519::Ed25519Signer::from_seed(&seed).unwrap();
        let mut t = Text::default();
        queue_logon(&mut t, 1, b"KEY123", 1_790_000_000_000, &signer).unwrap();
        let s = std::str::from_utf8(&t.0).unwrap();
        assert!(s.starts_with(r#"{"id":1,"method":"session.logon","params":{"apiKey":"KEY123","signature":""#));
        assert!(s.ends_with(r#"","timestamp":1790000000000}}"#));
        // The signature is over exactly the sorted query.
        let mut want = [0u8; signer_ed25519::SIG_B64_LEN];
        let n = signer.sign_b64(b"apiKey=KEY123&timestamp=1790000000000", &mut want);
        assert!(s.contains(std::str::from_utf8(&want[..n]).unwrap()));
    }

    #[test]
    fn answers() {
        let mut a = WsAnswer::default();
        let ok = br#"{"id":42,"status":200,"result":{"orderId":325078477,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mvABC","executedQty":"0","avgPrice":"0.00"},"rateLimits":[{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":300,"count":1}]}"#;
        scan_answer(ok, &mut a).unwrap();
        assert_eq!((a.id, a.status, a.code, a.venue_oid), (42, 200, 0, 325_078_477));
        assert_eq!(a.order_status, crate::userstream::S_NEW);
        assert_eq!(a.cid.get(ok), b"mvABC");
        let err = br#"{"id":43,"status":400,"error":{"code":-2010,"msg":"Account has insufficient balance."}}"#;
        scan_answer(err, &mut a).unwrap();
        assert_eq!((a.id, a.status, a.code, a.retry_after_ms), (43, 400, -2010, 0));
        let ban = br#"{"id":44,"status":418,"error":{"code":-1003,"msg":"Way too much request weight used; IP banned until 1790000600000.","data":{"serverTime":1790000000000,"retryAfter":1790000600000}}}"#;
        scan_answer(ban, &mut a).unwrap();
        assert_eq!((a.status, a.code, a.retry_after_ms), (418, -1003, 1_790_000_600_000));
        let shut = br#"{"event":{"e":"serverShutdown","E":1}}"#;
        scan_answer(shut, &mut a).unwrap();
        assert!(a.shutdown);
        assert_eq!(scan_answer(br#"{"status":200}"#, &mut a), Err(ScanErr::Missing));
        assert_eq!(scan_answer(br#"{"id":1,"status":400}"#, &mut a), Err(ScanErr::Missing));
        assert_eq!(scan_answer(br#"{"id":null,"status":400,"error":{"code":-1102}}"#, &mut a), Ok(()));
        assert_eq!(a.id, 0);
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut a = WsAnswer::default();
            if scan_answer(&bytes, &mut a).is_ok() {
                let _ = (a.cid.get(&bytes), a.result.get(&bytes));
                // BX8 (P2-6c): a subscribe's answer judged on any result.
                let _ = scan_subscribed(&bytes, a.result);
                // BX8: a user frame's event lies in the frame, and reads on.
                prop_assert!(a.event.at as usize + a.event.len as usize <= bytes.len());
                if a.user {
                    let mut e = crate::userstream::UserEvent::default();
                    if crate::userstream::scan_spot_event(&bytes, a.event.at as usize, &mut e).is_ok() {
                        prop_assert!(e.symbol.at as usize + e.symbol.len as usize <= bytes.len());
                        prop_assert!(e.cid.at as usize + e.cid.len as usize <= bytes.len());
                    }
                }
            }
        }

        #[test]
        fn a_rendered_order_is_valid_json_with_our_fields(qty in 1i64..1_000_000_000, px in 1i64..(1i64 << 40), id in any::<u32>()) {
            let q = qty / 1_000 * 1_000 + 1_000;
            let p = px / 100_000 * 100_000 + 100_000;
            let mut t = Text::default();
            let o = Place { symbol: b"BTCUSDT", side: 1, maker: false, qty_1e6: q, qty_dec: 3, px_1e6: p, px_dec: 1, cid: Part::Cid(&PREFIX, 3, id as u64), ts_ms: 1, reduce_only: id & 1 == 1 };
            queue_place(&mut t, id as u64, &st(), &o).unwrap();
            let mut w = Pairs::new(&t.0, 0).unwrap();
            let mut seen = 0;
            while let Some((k, a, z)) = w.next().unwrap() {
                if k == b"params" {
                    let mut pw = Pairs::new(&t.0, a).unwrap();
                    while let Some((pk, pa, pz)) = pw.next().unwrap() {
                        if pk == b"quantity" { prop_assert_eq!(dec_1e6(&t.0, pa, pz).unwrap(), q); seen += 1; }
                        if pk == b"price" { prop_assert_eq!(dec_1e6(&t.0, pa, pz).unwrap(), p); seen += 1; }
                    }
                }
                if k == b"id" { prop_assert_eq!(u64_of(&t.0, a, z).unwrap(), id as u64); seen += 1; }
            }
            prop_assert_eq!(seen, 3);
        }
    }
}
