// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The user-data stream (plan §3.3; BX-2, BX-15).** Gateway thread.
//!
//! The FILL is the user stream's (BX-2): an order answer is only the ACK.
//! USDⓈ-M reports one trade twice — `TRADE_LITE` (lower latency, no
//! commission) and inside `ORDER_TRADE_UPDATE` — and the gateway books the
//! first and drops the second on (row, trade id) ([`crate::oot::TidRing`]).
//!
//! [`scan_user_event`] reads one frame in place into a [`UserEvent`] (spans
//! into the frame, integers ×1e6; no allocation). Fail closed (BX-15): a
//! frame whose event it knows but cannot read is an `Err`, which the
//! gateway turns into a halt observation — never a skip. An event it does
//! not act on (`ACCOUNT_CONFIG_UPDATE`, `STRATEGY_UPDATE`, …) scans as
//! [`UE_OTHER`] and is counted. Both the raw form (`/ws`, `/private/ws`)
//! and the combined form (`{"stream":…,"data":{…}}`) are read.
//!
//! [`UserStream`] holds the listenKey and the socket's lifecycle state;
//! the gateway drives it (the REST calls that create and keep the key are
//! the gateway's, the one REST caller).

use core_types::Side;

use crate::json::{dec_1e6, str_of, u64_of, Pairs};

/// `ORDER_TRADE_UPDATE`.
pub const UE_ORDER: u8 = 1;
/// `TRADE_LITE`.
pub const UE_TRADE_LITE: u8 = 2;
/// `ACCOUNT_UPDATE` (a margin re-read follows).
pub const UE_ACCOUNT: u8 = 3;
/// `MARGIN_CALL` (BX-20: `MarginRisk` on every Binance slot).
pub const UE_MARGIN_CALL: u8 = 4;
/// `listenKeyExpired` (re-create the key, reconnect).
pub const UE_KEY_EXPIRED: u8 = 5;
/// An event the arm does not act on.
pub const UE_OTHER: u8 = 6;

/// Execution type `x`: `NEW`.
pub const X_NEW: u8 = 1;
/// `CANCELED`.
pub const X_CANCELED: u8 = 2;
/// `CALCULATED` (a liquidation fill).
pub const X_CALCULATED: u8 = 3;
/// `EXPIRED`.
pub const X_EXPIRED: u8 = 4;
/// `TRADE`.
pub const X_TRADE: u8 = 5;
/// `AMENDMENT` (a modify applied).
pub const X_AMENDMENT: u8 = 6;
/// Any other execution type.
pub const X_OTHER: u8 = 7;

/// Order status `X`: `NEW`.
pub const S_NEW: u8 = 1;
/// `PARTIALLY_FILLED`.
pub const S_PARTIALLY_FILLED: u8 = 2;
/// `FILLED`.
pub const S_FILLED: u8 = 3;
/// `CANCELED`.
pub const S_CANCELED: u8 = 4;
/// `EXPIRED`.
pub const S_EXPIRED: u8 = 5;
/// `EXPIRED_IN_MATCH` (self-trade prevention).
pub const S_EXPIRED_IN_MATCH: u8 = 6;
/// Any other status.
pub const S_OTHER: u8 = 7;

/// Why a frame did not scan (BX-15).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanErr {
    /// Not a JSON object, or a malformed one.
    Malformed = 1,
    /// No `e` (event type).
    NoEvent = 2,
    /// A field the event needs is missing.
    Missing = 3,
    /// A field is present but unreadable.
    BadField = 4,
}

/// A byte span into the frame.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Start.
    pub at: u32,
    /// Length.
    pub len: u32,
}

impl Span {
    /// The span of `inner`, a sub-slice of `outer`.
    #[inline(always)]
    pub(crate) fn of(outer: &[u8], inner: &[u8]) -> Self {
        // `inner` is a sub-slice of `outer`: its offset is the address gap.
        let at = inner.as_ptr() as usize - outer.as_ptr() as usize;
        Self {
            at: at as u32,
            len: inner.len() as u32,
        }
    }

    /// The bytes it names in `frame`.
    #[inline(always)]
    #[must_use]
    pub fn get<'a>(&self, frame: &'a [u8]) -> &'a [u8] {
        let a = self.at as usize;
        frame.get(a..a + self.len as usize).unwrap_or(&[])
    }
}

/// One scanned event. 128 B.
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct UserEvent {
    /// `i`: the venue order id.
    pub venue_oid: u64,
    /// `t`: the trade id (0 when no trade).
    pub trade_id: u64,
    /// `l`: the last fill's quantity ×1e6.
    pub last_qty_1e6: i64,
    /// `L`: the last fill's price ×1e6.
    pub last_px_1e6: i64,
    /// `z`: the order's cumulative filled quantity ×1e6.
    pub cum_qty_1e6: i64,
    /// `q`: the order's quantity ×1e6.
    pub qty_1e6: i64,
    /// `p`: the order's price ×1e6.
    pub px_1e6: i64,
    /// `n`: the commission ×1e6 (0 on `TRADE_LITE`).
    pub commission_1e6: i64,
    /// `ap`: the order's average fill price ×1e6 (0 when none, and on
    /// `TRADE_LITE`).
    pub avg_px_1e6: i64,
    /// `E`: the event time, ms.
    pub event_ms: u64,
    /// `T`: the trade / transaction time, ms.
    pub trade_ms: u64,
    /// `s`: the symbol.
    pub symbol: Span,
    /// `c`: the client order id.
    pub cid: Span,
    /// `UE_*`.
    pub kind: u8,
    /// `Side` as its `u8`.
    pub side: u8,
    /// `X_*`.
    pub exec: u8,
    /// `S_*`.
    pub status: u8,
    /// `m`: the fill was a maker.
    pub maker: u8,
    _r: [u8; 3],
}

const _: () = assert!(core::mem::size_of::<UserEvent>() == 128);

fn exec_of(s: &[u8]) -> u8 {
    match s {
        b"NEW" => X_NEW,
        b"CANCELED" => X_CANCELED,
        b"CALCULATED" => X_CALCULATED,
        b"EXPIRED" => X_EXPIRED,
        b"TRADE" => X_TRADE,
        b"AMENDMENT" => X_AMENDMENT,
        _ => X_OTHER,
    }
}

/// An order status word (`X`, and the `status` of an order answer).
#[must_use]
pub fn status_of(s: &[u8]) -> u8 {
    match s {
        b"NEW" => S_NEW,
        b"PARTIALLY_FILLED" => S_PARTIALLY_FILLED,
        b"FILLED" => S_FILLED,
        b"CANCELED" => S_CANCELED,
        b"EXPIRED" => S_EXPIRED,
        b"EXPIRED_IN_MATCH" => S_EXPIRED_IN_MATCH,
        _ => S_OTHER,
    }
}

/// `BUY` / `SELL` → the side's `u8`.
pub fn side_of(s: &[u8]) -> Result<u8, ScanErr> {
    match s {
        b"BUY" => Ok(Side::Bid as u8),
        b"SELL" => Ok(Side::Ask as u8),
        _ => Err(ScanErr::BadField),
    }
}

// Field-presence bits for the fail-closed check.
const F_S: u32 = 1 << 0;
const F_C: u32 = 1 << 1;
const F_SIDE: u32 = 1 << 2;
const F_X: u32 = 1 << 3;
const F_ST: u32 = 1 << 4;
const F_I: u32 = 1 << 5;
const F_L: u32 = 1 << 6;
const F_LP: u32 = 1 << 7;
const F_Z: u32 = 1 << 8;
const F_T: u32 = 1 << 9;
const F_Q: u32 = 1 << 10;
const F_P: u32 = 1 << 11;
const NEED_ORDER: u32 = F_S | F_C | F_SIDE | F_X | F_ST | F_I | F_L | F_LP | F_Z | F_T | F_Q | F_P;
const NEED_LITE: u32 = F_S | F_C | F_SIDE | F_I | F_L | F_LP | F_T;

#[inline(always)]
fn malformed(_: ()) -> ScanErr {
    ScanErr::Malformed
}

#[inline(always)]
fn bad(_: ()) -> ScanErr {
    ScanErr::BadField
}

/// Read the order fields of `obj` (the `o` object of `ORDER_TRADE_UPDATE`,
/// or the `TRADE_LITE` event itself) into `out`; the presence bits.
fn order_fields(f: &[u8], obj: usize, out: &mut UserEvent) -> Result<u32, ScanErr> {
    let mut w = Pairs::new(f, obj).map_err(malformed)?;
    let mut have = 0u32;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"s" => {
                // An order event names its symbol: an empty one is refused
                // (fail closed), never looked up.
                let sym = str_of(f, a, z).map_err(bad)?;
                if sym.is_empty() {
                    return Err(ScanErr::BadField);
                }
                out.symbol = Span::of(f, sym);
                have |= F_S;
            }
            b"c" => {
                out.cid = Span::of(f, str_of(f, a, z).map_err(bad)?);
                have |= F_C;
            }
            b"S" => {
                out.side = side_of(str_of(f, a, z).map_err(bad)?)?;
                have |= F_SIDE;
            }
            b"x" => {
                out.exec = exec_of(str_of(f, a, z).map_err(bad)?);
                have |= F_X;
            }
            b"X" => {
                out.status = status_of(str_of(f, a, z).map_err(bad)?);
                have |= F_ST;
            }
            b"i" => {
                out.venue_oid = u64_of(f, a, z).map_err(bad)?;
                have |= F_I;
            }
            b"l" => {
                out.last_qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                have |= F_L;
            }
            b"L" => {
                out.last_px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                have |= F_LP;
            }
            b"z" => {
                out.cum_qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                have |= F_Z;
            }
            b"t" => {
                out.trade_id = u64_of(f, a, z).map_err(bad)?;
                have |= F_T;
            }
            b"q" => {
                out.qty_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                have |= F_Q;
            }
            b"p" => {
                out.px_1e6 = dec_1e6(f, a, z).map_err(bad)?;
                have |= F_P;
            }
            b"n" => out.commission_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"ap" => out.avg_px_1e6 = dec_1e6(f, a, z).map_err(bad)?,
            b"m" => out.maker = crate::json::bool_of(f, a, z).map_err(bad)? as u8,
            b"T" => out.trade_ms = u64_of(f, a, z).map_err(bad)?,
            _ => {}
        }
    }
    Ok(have)
}

/// **Scan one user-data frame** (module docs).
pub fn scan_user_event(frame: &[u8], out: &mut UserEvent) -> Result<(), ScanErr> {
    *out = UserEvent::default();
    let mut w = Pairs::new(frame, 0).map_err(malformed)?;
    let mut ev: &[u8] = &[];
    let mut inner = usize::MAX;
    let mut data = usize::MAX;
    while let Some((k, a, z)) = w.next().map_err(malformed)? {
        match k {
            b"e" => ev = str_of(frame, a, z).map_err(bad)?,
            b"E" => out.event_ms = u64_of(frame, a, z).map_err(bad)?,
            b"T" => out.trade_ms = u64_of(frame, a, z).map_err(bad)?,
            b"o" => inner = a,
            b"data" => data = a,
            _ => {}
        }
    }
    if ev.is_empty() {
        if data != usize::MAX {
            // The combined-stream wrapper: the event is `data`.
            return scan_data(frame, data, out);
        }
        return Err(ScanErr::NoEvent);
    }
    match ev {
        b"ORDER_TRADE_UPDATE" => {
            if inner == usize::MAX {
                return Err(ScanErr::Missing);
            }
            out.kind = UE_ORDER;
            let have = order_fields(frame, inner, out)?;
            if have & NEED_ORDER != NEED_ORDER {
                return Err(ScanErr::Missing);
            }
        }
        b"TRADE_LITE" => {
            out.kind = UE_TRADE_LITE;
            let have = order_fields(frame, 0, out)?;
            if have & NEED_LITE != NEED_LITE {
                return Err(ScanErr::Missing);
            }
            out.exec = X_TRADE;
        }
        b"ACCOUNT_UPDATE" => out.kind = UE_ACCOUNT,
        b"MARGIN_CALL" => out.kind = UE_MARGIN_CALL,
        b"listenKeyExpired" => out.kind = UE_KEY_EXPIRED,
        _ => out.kind = UE_OTHER,
    }
    Ok(())
}

/// The `data` object of a combined-stream frame, scanned as a raw event.
fn scan_data(frame: &[u8], at: usize, out: &mut UserEvent) -> Result<(), ScanErr> {
    let end = core_parse::skip_json_value(frame, at).ok_or(ScanErr::Malformed)?;
    let inner = frame.get(at..end).ok_or(ScanErr::Malformed)?;
    if inner.first() != Some(&b'{') {
        return Err(ScanErr::Malformed);
    }
    // One level only: a `data` inside `data` is not a venue shape. Scanned
    // straight into `out`, whose spans are then re-based onto the frame.
    let r = scan_user_event_flat(inner, out);
    out.symbol.at += at as u32;
    out.cid.at += at as u32;
    r
}

fn scan_user_event_flat(frame: &[u8], out: &mut UserEvent) -> Result<(), ScanErr> {
    let mut w = Pairs::new(frame, 0).map_err(malformed)?;
    while let Some((k, _, _)) = w.next().map_err(malformed)? {
        if k == b"data" {
            return Err(ScanErr::Malformed);
        }
    }
    scan_user_event(frame, out)
}

// -------------------------------------------------------------------------
// The listenKey and the socket's lifecycle
// -------------------------------------------------------------------------

/// The listenKey's capacity (the venue's keys are 64 characters).
pub const LISTEN_KEY_MAX: usize = 96;
/// A keep-alive every 30 minutes (the key lives 60).
pub const KEEPALIVE_NS: u64 = 30 * 60 * 1_000_000_000;

/// Where the stream is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UsPhase {
    /// No key: the gateway must `POST` one.
    NeedKey = 0,
    /// A `POST` is in flight.
    KeyPending = 1,
    /// A key is held; the socket may connect.
    Keyed = 2,
}

/// The key and its clocks (the socket is the gateway's `WsConn`).
#[derive(Clone, Debug)]
pub struct UserStream {
    key: [u8; LISTEN_KEY_MAX],
    key_len: usize,
    /// The phase.
    pub phase: UsPhase,
    /// Monotonic ns of the last successful create or keep-alive.
    pub kept_ns: u64,
    /// Monotonic ns of the last frame read.
    pub last_rx_ns: u64,
}

impl Default for UserStream {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStream {
    /// No key yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            key: [0; LISTEN_KEY_MAX],
            key_len: 0,
            phase: UsPhase::NeedKey,
            kept_ns: 0,
            last_rx_ns: 0,
        }
    }

    /// Adopt a key from a `POST` answer: `Some(changed)`, or `None` — the
    /// key is refused whole, never truncated — if it is empty, too long or
    /// not `[A-Za-z0-9]` (it goes into a URL path).
    pub fn set_key(&mut self, key: &[u8], now_ns: u64) -> Option<bool> {
        if key.is_empty() || key.len() > LISTEN_KEY_MAX || !key.iter().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let changed = self.key() != key;
        // COPY: the listenKey (≤ 96 B) into the stream's own buffer — once
        // per create, cold; the REST answer that carried it is overwritten
        // by the next request — keeping a span into it was rejected.
        self.key[..key.len()].copy_from_slice(key);
        self.key_len = key.len();
        self.phase = UsPhase::Keyed;
        self.kept_ns = now_ns;
        Some(changed)
    }

    /// The key.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key[..self.key_len]
    }

    /// The key expired (`listenKeyExpired`, or a keep-alive refused).
    pub fn expire(&mut self) {
        self.phase = UsPhase::NeedKey;
        self.key_len = 0;
    }

    /// A keep-alive is due.
    #[must_use]
    pub fn keepalive_due(&self, now_ns: u64) -> bool {
        self.phase == UsPhase::Keyed && now_ns.saturating_sub(self.kept_ns) >= KEEPALIVE_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(crate) const OTU: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000123,"T":1790000000120,"o":{"s":"BTCUSDT","c":"mv0000000720000000000000005000000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.012","p":"65000.1","ap":"65000.1","sp":"0","x":"TRADE","X":"FILLED","i":8886774,"l":"0.012","z":"0.012","L":"65000.1","N":"USDT","n":"0.312","T":1790000000119,"t":4242,"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_TAKER","pm":"NONE","gtd":0}}"#;
    const LITE: &[u8] = br#"{"e":"TRADE_LITE","E":1790000000100,"T":1790000000099,"s":"BTCUSDT","q":"0.012","p":"0.0","m":true,"c":"mvX","S":"SELL","L":"65000.2","l":"0.005","t":4243,"i":8886775}"#;

    #[test]
    fn an_order_trade_update() {
        let mut e = UserEvent::default();
        scan_user_event(OTU, &mut e).unwrap();
        assert_eq!((e.kind, e.exec, e.status, e.side), (UE_ORDER, X_TRADE, S_FILLED, Side::Bid as u8));
        assert_eq!((e.venue_oid, e.trade_id), (8_886_774, 4_242));
        assert_eq!((e.last_qty_1e6, e.last_px_1e6, e.cum_qty_1e6), (12_000, 65_000_100_000, 12_000));
        assert_eq!((e.qty_1e6, e.px_1e6, e.commission_1e6), (12_000, 65_000_100_000, 312_000));
        assert_eq!(e.symbol.get(OTU), b"BTCUSDT");
        assert_eq!(e.cid.get(OTU), b"mv0000000720000000000000005000000");
        assert_eq!((e.event_ms, e.trade_ms), (1_790_000_000_123, 1_790_000_000_119));
    }

    #[test]
    fn a_trade_lite() {
        let mut e = UserEvent::default();
        scan_user_event(LITE, &mut e).unwrap();
        assert_eq!((e.kind, e.exec, e.side, e.maker), (UE_TRADE_LITE, X_TRADE, Side::Ask as u8, 1));
        assert_eq!((e.last_qty_1e6, e.last_px_1e6, e.trade_id, e.venue_oid), (5_000, 65_000_200_000, 4_243, 8_886_775));
        assert_eq!(e.cid.get(LITE), b"mvX");
    }

    #[test]
    fn the_combined_form_rebases_its_spans() {
        let mut f = std::vec::Vec::new();
        f.extend_from_slice(br#"{"stream":"k","data":"#);
        f.extend_from_slice(OTU);
        f.extend_from_slice(b"}");
        let mut e = UserEvent::default();
        scan_user_event(&f, &mut e).unwrap();
        assert_eq!(e.symbol.get(&f), b"BTCUSDT");
        assert_eq!(e.kind, UE_ORDER);
    }

    #[test]
    fn a_known_event_missing_a_field_is_refused_never_skipped() {
        let cut = std::str::from_utf8(OTU).unwrap().replace(r#""t":4242,"#, "");
        let mut e = UserEvent::default();
        assert_eq!(scan_user_event(cut.as_bytes(), &mut e), Err(ScanErr::Missing));
        let bad_side = std::str::from_utf8(OTU).unwrap().replace(r#""S":"BUY""#, r#""S":"BOTH""#);
        assert_eq!(scan_user_event(bad_side.as_bytes(), &mut e), Err(ScanErr::BadField));
        assert_eq!(scan_user_event(br#"{"E":1}"#, &mut e), Err(ScanErr::NoEvent));
        assert_eq!(scan_user_event(br#"{"e":"ORDER_TRADE_UPDATE","E":1}"#, &mut e), Err(ScanErr::Missing));
        assert_eq!(scan_user_event(b"[1]", &mut e), Err(ScanErr::Malformed));
    }

    #[test]
    fn the_other_events() {
        let mut e = UserEvent::default();
        scan_user_event(br#"{"e":"MARGIN_CALL","E":1,"cw":"3.16","p":[]}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_MARGIN_CALL);
        scan_user_event(br#"{"e":"ACCOUNT_UPDATE","E":1,"T":1,"a":{"m":"ORDER","B":[],"P":[]}}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_ACCOUNT);
        scan_user_event(br#"{"e":"listenKeyExpired","E":1,"listenKey":"x"}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_KEY_EXPIRED);
        scan_user_event(br#"{"e":"STRATEGY_UPDATE","E":1}"#, &mut e).unwrap();
        assert_eq!(e.kind, UE_OTHER);
    }

    #[test]
    fn the_key_lifecycle() {
        let mut u = UserStream::new();
        assert_eq!(u.phase, UsPhase::NeedKey);
        let key: &[u8] = b"pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";
        assert_eq!(u.set_key(b"has space", 1), None);
        assert_eq!(u.set_key(&[b'k'; LISTEN_KEY_MAX + 1], 1), None, "refused whole, never truncated");
        assert_eq!(u.set_key(key, 10), Some(true));
        assert_eq!(u.phase, UsPhase::Keyed);
        assert_eq!(u.set_key(key, 10), Some(false), "the same key: no new socket");
        assert!(!u.keepalive_due(10 + KEEPALIVE_NS - 1));
        assert!(u.keepalive_due(10 + KEEPALIVE_NS));
        u.expire();
        assert_eq!((u.phase, u.key().len()), (UsPhase::NeedKey, 0));
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut e = UserEvent::default();
            if scan_user_event(&bytes, &mut e).is_ok() {
                let _ = (e.symbol.get(&bytes), e.cid.get(&bytes));
                prop_assert!(matches!(e.kind, UE_ORDER | UE_TRADE_LITE | UE_ACCOUNT | UE_MARGIN_CALL | UE_KEY_EXPIRED | UE_OTHER));
            }
        }

        /// A mutation of a real frame never panics and, when it still
        /// scans as an order event, carries every required field.
        #[test]
        fn mutations_of_a_real_frame(at in 0usize..600, byte in any::<u8>()) {
            let mut f = OTU.to_vec();
            let i = at % f.len();
            f[i] = byte;
            let mut e = UserEvent::default();
            if scan_user_event(&f, &mut e).is_ok() && e.kind == UE_ORDER {
                prop_assert!(!e.symbol.get(&f).is_empty() && !e.cid.get(&f).is_empty());
            }
        }
    }
}
