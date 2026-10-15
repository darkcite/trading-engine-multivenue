// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The private socket's messages (LAW E-5: `Fill` is the fill of
//! record).
//!
//! Every message is one flat object with a `type`:
//!
//! * `Fill` — `order_id`, `fill_id`, `symbol`, `side` (`buy`/`sell`),
//!   `price`, `size`, `timestamp` (ms), `fee`, …
//! * `OrderUpdate` — `order_id`, `status`, `filled_size` (cumulative),
//!   `reason`, …
//! * `Authenticated`, `Subscribed`, `Error` — the session's own.
//!
//! The venue may add fields at any time (its schema is additive); only
//! the ones named are read, by top-level key. A `Fill` whose price,
//! size or side does not read is `Bad` — counted, never guessed.

use core::ops::Range;

use crate::json::{self, Kind};
use crate::num::scan_1e6_exact;
use crate::response::Status;

/// One fill, as the socket carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillMsg {
    /// The venue's order id.
    pub order_id: u64,
    /// The venue's fill id (the dedupe key).
    pub fill_id: u64,
    /// The instrument's span in the payload.
    pub symbol: Range<usize>,
    /// We bought.
    pub buy: bool,
    /// USD per contract ×1e6.
    pub px_1e6: i64,
    /// Contracts ×1e6 (positive).
    pub qty_1e6: i64,
    /// Venue time, ms.
    pub ts_ms: u64,
    /// Fee paid, USD ×1e6 (0 when absent — MEASURED, never assumed).
    pub fee_1e6: i64,
}

/// One order update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMsg {
    /// The venue's order id.
    pub order_id: u64,
    /// Its status.
    pub status: Status,
    /// Cumulative contracts filled ×1e6.
    pub filled_1e6: i64,
}

/// What a payload was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// A fill.
    Fill(FillMsg),
    /// An order update.
    Update(UpdateMsg),
    /// `Authenticated`.
    Authenticated,
    /// `Subscribed`.
    Subscribed,
    /// `Error` (its `message`/`error` span, when present).
    Error(Option<Range<usize>>),
    /// Anything else the socket may carry (portfolio, pongs, …).
    Other,
    /// A `Fill` or `OrderUpdate` that did not read.
    Bad,
}

fn str_of<'a>(b: &'a [u8], obj: &json::Val, k: &[u8]) -> Option<&'a [u8]> {
    json::field_in(b, obj, k)
        .filter(|v| v.kind == Kind::Str)
        .map(|v| &b[v.span])
}

fn fill(b: &[u8], obj: &json::Val) -> Option<FillMsg> {
    let order_id = json::field_in(b, obj, b"order_id")?.as_u64(b)?;
    let fill_id = json::field_in(b, obj, b"fill_id")?.as_u64(b)?;
    let sym = json::field_in(b, obj, b"symbol").filter(|v| v.kind == Kind::Str)?;
    let buy = match str_of(b, obj, b"side")? {
        b"buy" | b"Buy" | b"BUY" => true,
        b"sell" | b"Sell" | b"SELL" => false,
        _ => return None,
    };
    let px_1e6 = scan_1e6_exact(str_of(b, obj, b"price")?)?;
    let qty_1e6 = scan_1e6_exact(str_of(b, obj, b"size")?)?;
    if qty_1e6 <= 0 || sym.span.is_empty() {
        return None;
    }
    let ts_ms = json::field_in(b, obj, b"timestamp")?.as_u64(b)?;
    let fee_1e6 = match str_of(b, obj, b"fee") {
        Some(f) => match f.first() {
            Some(b'-') => -scan_1e6_exact(&f[1..])?,
            _ => scan_1e6_exact(f)?,
        },
        None => 0,
    };
    Some(FillMsg {
        order_id,
        fill_id,
        symbol: sym.span,
        buy,
        px_1e6,
        qty_1e6,
        ts_ms,
        fee_1e6,
    })
}

fn update(b: &[u8], obj: &json::Val) -> Option<UpdateMsg> {
    let order_id = json::field_in(b, obj, b"order_id")?.as_u64(b)?;
    let status = Status::parse(str_of(b, obj, b"status")?)?;
    let filled_1e6 = match json::field_in(b, obj, b"filled_size") {
        Some(v) if v.kind == Kind::Str => scan_1e6_exact(v.bytes(b))?,
        Some(v) if v.is_null(b) => 0,
        None => 0,
        Some(_) => return None,
    };
    Some(UpdateMsg {
        order_id,
        status,
        filled_1e6,
    })
}

/// Classify one payload.
#[must_use]
pub fn parse(b: &[u8]) -> Msg {
    let Some(root) = json::root(b) else {
        return Msg::Other;
    };
    let Some(ty) = str_of(b, &root, b"type") else {
        // The venue's slow-consumer notice has no `type`; the socket
        // is closed after it anyway.
        return Msg::Other;
    };
    match ty {
        b"Fill" => fill(b, &root).map_or(Msg::Bad, Msg::Fill),
        b"OrderUpdate" => update(b, &root).map_or(Msg::Bad, Msg::Update),
        b"Authenticated" => Msg::Authenticated,
        b"Subscribed" => Msg::Subscribed,
        b"Error" => Msg::Error(
            json::field_in(b, &root, b"message")
                .or_else(|| json::field_in(b, &root, b"error"))
                .filter(|v| v.kind == Kind::Str)
                .map(|v| v.span),
        ),
        _ => Msg::Other,
    }
}

/// The last fill ids seen — a fill re-sent after a reconnect is booked
/// once. Fixed ring; outlives the socket (it lives in the arm).
pub struct FillIds {
    ids: [u64; FILL_IDS],
    head: usize,
    len: usize,
}

/// Fill ids remembered.
pub const FILL_IDS: usize = 512;

impl Default for FillIds {
    fn default() -> Self {
        Self::new()
    }
}

impl FillIds {
    /// Nothing seen.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ids: [0; FILL_IDS],
            head: 0,
            len: 0,
        }
    }

    /// Record `id`; `false` if it was already seen (do not book).
    pub fn first_time(&mut self, id: u64) -> bool {
        let mut i = 0usize;
        while i < self.len {
            if self.ids[i] == id {
                return false;
            }
            i += 1;
        }
        self.ids[self.head] = id;
        self.head = (self.head + 1) % FILL_IDS;
        if self.len < FILL_IDS {
            self.len += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILL: &[u8] = br#"{"type":"Fill","order_id":12345,"fill_id":67890,"symbol":"BTC-20260131-100000-C","side":"buy","price":"0.0523","size":"5.0","timestamp":1737331200000,"wallet_address":"0x1234","fee":"0","trade_id":99999,"is_taker":true}"#;

    #[test]
    fn the_documented_fill_reads() {
        let Msg::Fill(f) = parse(FILL) else {
            panic!("not a fill")
        };
        assert_eq!((f.order_id, f.fill_id, f.buy), (12345, 67890, true));
        assert_eq!((f.px_1e6, f.qty_1e6, f.ts_ms, f.fee_1e6), (52_300, 5_000_000, 1_737_331_200_000, 0));
        assert_eq!(&FILL[f.symbol], b"BTC-20260131-100000-C");
    }

    #[test]
    fn the_documented_update_reads() {
        let b = br#"{"type":"OrderUpdate","order_id":12345,"request":{"price":"0.0523","size":"10.0","symbol":"BTC-20260131-100000-C","side":"Buy","tif":"gtc"},"status":"PARTIALLY_FILLED","filled_size":"0.75","timestamp":1767225600000,"reason":null,"wallet_address":"0x1","instrument_type":"option"}"#;
        assert_eq!(
            parse(b),
            Msg::Update(UpdateMsg {
                order_id: 12345,
                status: Status::PartiallyFilled,
                filled_1e6: 750_000
            })
        );
    }

    #[test]
    fn session_messages_and_junk_classify() {
        assert_eq!(parse(br#"{"type":"Authenticated","wallet":"0x1"}"#), Msg::Authenticated);
        assert_eq!(parse(br#"{"type":"Subscribed","channel":"fills"}"#), Msg::Subscribed);
        let e = br#"{"type":"Error","message":"invalid wallet"}"#;
        let Msg::Error(Some(r)) = parse(e) else { panic!() };
        assert_eq!(&e[r], b"invalid wallet");
        assert_eq!(parse(br#"{"type":"PortfolioUpdate"}"#), Msg::Other);
        assert_eq!(parse(b"not json"), Msg::Other);
        assert_eq!(parse(br#"{"type":"Fill","order_id":1}"#), Msg::Bad);
        let bad_side = br#"{"type":"Fill","order_id":1,"fill_id":2,"symbol":"X","side":"up","price":"1","size":"1","timestamp":1}"#;
        assert_eq!(parse(bad_side), Msg::Bad);
        let zero = br#"{"type":"Fill","order_id":1,"fill_id":2,"symbol":"X","side":"sell","price":"1","size":"0","timestamp":1}"#;
        assert_eq!(parse(zero), Msg::Bad);
    }

    #[test]
    fn a_replayed_fill_is_booked_once() {
        let mut s = FillIds::new();
        assert!(s.first_time(7));
        assert!(!s.first_time(7));
        let mut i = 0u64;
        while i < FILL_IDS as u64 {
            assert!(s.first_time(1_000 + i));
            i += 1;
        }
        assert!(s.first_time(7), "evicted after a full ring");
    }
}
