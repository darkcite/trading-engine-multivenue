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

use core_net::{ReqKind, WsConn, WsErr, WsPart};

use crate::cid::{CidPrefix, CID_LEN};
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
const REQ_FREE: u8 = 0xFF;

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

impl ReqKind for WsReq {
    const FREE: Self = Self {
        oid: 0,
        gen: 0,
        ix: 0,
        verb: REQ_FREE,
    };
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
                // masked in place after) or the logon's signer input
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

/// Render `parts` as plain text into `dst` (a test's or the boot
/// vectors' buffer: what the frame carries, unmasked). The length, or
/// `Overflow` — refused whole, never truncated.
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
    /// `selfTradePreventionMode`.
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
}

#[inline(always)]
const fn side_word(side: u8) -> &'static [u8] {
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
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"order.place\",\"params\":{\"symbol\":\""),
        Part::Lit(o.symbol),
        Part::Lit(b"\",\"side\":\""),
        Part::Lit(side_word(o.side)),
        Part::Lit(b"\",\"type\":\"LIMIT\",\"timeInForce\":\""),
        Part::Lit(tif),
        Part::Lit(b"\",\"quantity\":\""),
        Part::Fixed(o.qty_1e6, o.qty_dec),
        Part::Lit(b"\",\"price\":\""),
        Part::Fixed(o.px_1e6, o.px_dec),
        Part::Lit(b"\",\"newClientOrderId\":\""),
        o.cid,
        Part::Lit(b"\",\"newOrderRespType\":\"ACK\",\"selfTradePreventionMode\":\""),
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

/// **`v2/account.status`** — positions, balances and the margin figures
/// (recon, BX-20).
pub fn queue_account<S: FrameSink>(s: &mut S, id: u64, st: &WsStatic, ts_ms: u64) -> Result<(), WsErr> {
    s.queue_parts(&[
        Part::Lit(b"{\"id\":"),
        Part::U64(id),
        Part::Lit(b",\"method\":\"v2/account.status\",\"params\":{\"recvWindow\":"),
        Part::U64(st.recv_window_ms),
        Part::Lit(b",\"timestamp\":"),
        Part::U64(ts_ms),
        Part::Lit(b"}}"),
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
}

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// **Scan one WS API frame** (module docs).
pub fn scan_answer(f: &[u8], out: &mut WsAnswer) -> Result<(), ScanErr> {
    *out = WsAnswer::default();
    let mut w = Pairs::new(f, 0).map_err(malformed)?;
    let (mut have_id, mut have_status) = (false, false);
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"id" => {
                out.id = if is_null(f, a, z) { 0 } else { u64_of(f, a, z).map_err(bad)? };
                have_id = true;
            }
            b"status" => {
                let s = u64_of(f, a, z).map_err(bad)?;
                if s > 999 {
                    return Err(ScanErr::BadField);
                }
                out.status = s as u16;
                have_status = true;
            }
            b"result" => {
                out.result = Span {
                    at: a as u32,
                    len: (z - a) as u32,
                };
                if f.get(a) == Some(&b'{') {
                    result_fields(f, a, out)?;
                }
            }
            b"error" => {
                let mut e = Pairs::new(f, a).map_err(malformed)?;
                while let Some((ek, ea, ez)) = e.next().map_err(malformed)? {
                    if ek == b"code" {
                        let c = i64_of(f, ea, ez).map_err(bad)?;
                        out.code = c.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                    } else if ek == b"data" && f.get(ea) == Some(&b'{') {
                        // Advisory: a `retryAfter` it cannot read leaves the
                        // default quiet in force (never a shorter one).
                        let mut d = Pairs::new(f, ea).map_err(malformed)?;
                        while let Some((dk, da, dz)) = d.next().map_err(malformed)? {
                            if dk == b"retryAfter" {
                                out.retry_after_ms = u64_of(f, da, dz).unwrap_or(0);
                            }
                        }
                    }
                }
            }
            b"event" => {
                let mut e = Pairs::new(f, a).map_err(malformed)?;
                while let Some((ek, ea, ez)) = e.next().map_err(malformed)? {
                    if ek == b"e" && str_of(f, ea, ez).map_err(bad)? == b"serverShutdown" {
                        out.shutdown = true;
                    }
                }
            }
            _ => {}
        }
    }
    if out.shutdown {
        return Ok(());
    }
    if !have_id || !have_status {
        return Err(ScanErr::Missing);
    }
    if out.status >= 400 && out.code == 0 {
        // A refusal must say why; one that does not is not a shape we know.
        return Err(ScanErr::Missing);
    }
    Ok(())
}

fn result_fields(f: &[u8], at: usize, out: &mut WsAnswer) -> Result<(), ScanErr> {
    let mut r = Pairs::new(f, at).map_err(malformed)?;
    while let Some((k, a, z)) = r.next().map_err(malformed)? {
        match k {
            b"orderId" => out.venue_oid = u64_of(f, a, z).map_err(bad)?,
            b"status" => out.order_status = status_of(str_of(f, a, z).map_err(bad)?),
            b"executedQty" => out.executed_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"avgPrice" => out.avg_px_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"origQty" => out.qty_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"price" => out.px_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"clientOrderId" => out.cid = Span::of(f, str_of(f, a, z).map_err(bad)?),
            _ => {}
        }
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
        };
        queue_place(&mut t, 42, &st(), &o).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":42,"method":"order.place","params":{"symbol":"BTCUSDT","side":"BUY","type":"LIMIT","timeInForce":"GTX","quantity":"0.012","price":"65000.1","newClientOrderId":"mv6512ab0f30123456789abcdef00000","newOrderRespType":"ACK","selfTradePreventionMode":"EXPIRE_TAKER","recvWindow":1000,"timestamp":1790000000000}}"#
        );
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
        queue_account(&mut t, 10, &st(), 8).unwrap();
        assert_eq!(
            std::str::from_utf8(&t.0).unwrap(),
            r#"{"id":10,"method":"v2/account.status","params":{"recvWindow":1000,"timestamp":8}}"#
        );
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
            }
        }

        #[test]
        fn a_rendered_order_is_valid_json_with_our_fields(qty in 1i64..1_000_000_000, px in 1i64..(1i64 << 40), id in any::<u32>()) {
            let q = qty / 1_000 * 1_000 + 1_000;
            let p = px / 100_000 * 100_000 + 100_000;
            let mut t = Text::default();
            let o = Place { symbol: b"BTCUSDT", side: 1, maker: false, qty_1e6: q, qty_dec: 3, px_1e6: p, px_dec: 1, cid: Part::Cid(&PREFIX, 3, id as u64), ts_ms: 1 };
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
