// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-mexc (MX3 + MX4 — the seventh venue)
//!
//! MEXC public market data, DATA-ONLY (operator ruling O-MX1: no exec
//! arm, no keys, nothing here can place an order). Two connection
//! classes share ONE thread, ONE tick producer and ONE venue-event
//! producer (single-writer law) — the Bybit spot/linear pattern — and
//! a [`MexcClass`] on every driver selects the parser, the subscribe
//! renderer, the keepalive payload and the confirmation law:
//!
//! * **SPOT** — `wss://wbs-api.mexc.com/ws`. Pushes are **Protocol
//!   Buffers** in WS BINARY frames ([`spot`]): the
//!   `PushDataV3ApiWrapper` is walked ORDER-AGNOSTICALLY with the
//!   `core_parse` PB primitives (plan §4 D5), and every price and
//!   quantity is an ASCII decimal string scanned IN PLACE off the rx
//!   buffer. Channels `spot@public.aggre.bookTicker.v3.api.pb@10ms@<SYM>`
//!   (the BBO) and `spot@public.aggre.deals.v3.api.pb@10ms@<SYM>`
//!   (prints). Incremental depth is `Blocked!` on the public tier
//!   (plan §1.1): there is no spot book to build, only a BBO.
//! * **FUTURES** — `wss://contract.mexc.com/edge`. Plain JSON text
//!   ([`futures`]): `push.depth.full` (limit 5, ~270 ms — the BBO
//!   source, plan §4 D2), `push.deal` (prints) and `push.ticker` (mark /
//!   funding / open interest). Every number is a BARE JSON number that
//!   may be an integer (`80489`) — the exponent-tolerant
//!   `scan_number_sci_*` scanners read them all.
//!
//! ## Channel → capture mapping (plan §4 D4)
//!
//! | source | emits | `v0` | `v1` |
//! |---|---|---|---|
//! | spot `aggre.bookTicker` | `Tick` | — | — |
//! | spot `aggre.deals` | `Trade` | px ×1e6 | qty ×1e6, NEGATED when the aggressor sold |
//! | futures `depth.full` | `Tick` (qty in venue contracts) | — | — |
//! | futures `deal` | `Trade` | px ×1e6 | vol ×1e6 (contracts), negated on `T == 2` |
//! | futures `ticker` | `Mark` | `fairPrice` ×1e6 | `indexPrice` ×1e6 |
//! | futures `ticker` | `Funding` | rate ×1e9 | next-settle ms (ruling Q-MX3, below) |
//! | futures `ticker` | `Ticker` | 0 | `holdVol` ×1e6, venue contract units |
//! | a refused subscription | `SubDrop` | [`SUB_DROP_REFUSED`] or the ack's code | [`MexcChannel::discriminant`] (−1 unknown) |
//!
//! ## A tick is a BBO CHANGE (measured live 2026-09-23, MX9)
//!
//! MEXC republishes UNCHANGED quotes: spot `aggre.bookTicker@10ms`
//! pushes every 10 ms per symbol whether or not the touch moved
//! (96.5–99.9 % of spot pushes were byte-identical to the previous one
//! over a 120 s live window), and futures `depth.full` pushes whenever
//! any of its 5 levels moves (26–79 % BBO-identical). The engine's
//! doctrine is tick = BBO change (Binance bookTicker, Bybit
//! `orderbook.1`, OKX `bbo-tbt` all push on change), so the driver
//! drops a push whose `(bid px, bid qty, ask px, ask qty)` equals the
//! last one EMITTED for that symbol: it counts as a msg, still teaches
//! the feed clock its stamp and still feeds the seq-regression check,
//! but takes no ring slot and writes no capture row. The first quote of
//! every session is emitted, and so is an unchanged quote whose VT2
//! stale verdict FLIPPED (the vm mirrors the latest tick's flag) or
//! whose previous emission the ring dropped.
//!
//! ## Client heartbeat (measured live 2026-09-23, MX9)
//!
//! MEXC futures closes a socket that has not received a client `ping`
//! for 60 s, however busy the feed (`rs.error: "more than 60 seconds
//! no response, close the channel"`), so both classes ping on a fixed
//! cadence from the last ping SENT — the first one a ping interval
//! after the session STARTS (`core_net::Keepalive::poll_client_heartbeat`),
//! never after inbound silence, which a busy feed never reaches.
//!
//! ## `venue_seq` (operator ruling Q-MX1)
//!
//! Carried everywhere: spot book `version` (ASCII digits in body field
//! 5), spot trade = the LEADING DIGITS of the `tradeId` string
//! ([`trade_id_seq`]), futures book `version`, futures deal `i`.
//! `Tick::venue_seq` is `u32` (truncated `v & 0xFFFF_FFFF`, the Bybit
//! law); `ChannelEvent::venue_seq` is the full `u64`. The §6.2 chain
//! law does NOT apply — these are sampled/snapshot streams that skip
//! versions by design — so no `TradeGap`/`BookGap` is ever emitted.
//! Instead the driver counts seq REGRESSIONS
//! (`IngressStatus::inc_seq_regressions`): a value strictly below the
//! last-seen full-width value of the same symbol × stream (book / trade);
//! 0 = absent and never counts. A trade push carrying several prints
//! is judged by its LARGEST id, so the venue's intra-push print order
//! cannot manufacture a regression.
//!
//! ## Subscribe / ack law (plan §4 D7)
//!
//! * Spot: ONE `{"method":"SUBSCRIPTION","params":[…]}` per connection;
//!   the venue answers ONE text ack that enumerates per-param outcomes —
//!   failed params inside `Not Subscribed successfully! [p1,p2]`
//!   (followed by `Reason： …`, a FULL-WIDTH colon). Every requested
//!   param NOT listed as failed is confirmed; `code != 0` refuses the
//!   whole request. A spot pair is also confirmed by its first data.
//! * Futures: ONE `{"method":"sub.<ch>","param":{…}}` frame per
//!   (symbol, channel); the `rs.sub.<ch>` acks carry NO symbol, so a
//!   pair is confirmed only by its FIRST DATA. A non-`success`
//!   `rs.sub.*` or an `rs.error` frame is a request refusal.
//! * A refused SYMBOL never blinds its socket (plan R10 — MEXC delists
//!   aggressively): the pairs a spot ack confirms are confirmed first,
//!   and a refused param is a non-fatal drop — `sub_drops` + one
//!   `SubDrop` event + a rate-limited WARN — unless the ack refuses
//!   EVERY pair of a driver that never confirmed anything (the boot
//!   fail-fast, venue-blind). A futures `rs.error` naming a contract
//!   (`"Contract [X] not exists"`, live 2026-09-23) is a per-symbol drop
//!   and never fatal; a non-`success` `rs.sub.*` names no symbol and
//!   keeps the never-confirmed fail-fast; any other `rs.error` (the
//!   heartbeat notice) is a session error, not a drop. The
//!   establishment budget reaps a session that confirms nothing.
//!
//! ## Funding `v1` (operator ruling Q-MX3)
//!
//! `push.ticker` carries no next-funding time. Each perp row is seeded
//! at boot from REST `GET /api/v1/contract/funding_rate/{SYM}`
//! ([`discovery::parse_funding_rate`] →
//! [`run_loop::Driver::set_funding_seed`]) and advanced by
//! `collectCycle × 1 h` exactly when an event's venue time REACHES the
//! latched instant ([`funding_next_settle_ms`] — arithmetic, never a
//! loop), so the strategy-vm settled-print law sees `v1` advance exactly
//! when a period settles. Unseeded or `collectCycle == 0` ⇒ `v1 = 0`.
//!
//! Everything after the handshake is zero-alloc and zero-copy: parsers
//! return spans into the rx buffer; the only copies are the sanctioned
//! ones marked `// COPY:` (subscribe render scratch, the WS ping echo,
//! the 64-byte PODs moved into their ring slots).

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod discovery;
pub mod futures;
pub mod run_loop;
pub mod spot;

pub use futures::{
    classify_futures, extract_fut_symbol, extract_fut_ts_ms, extract_refused_contract,
    parse_depth_full,
    parse_fut_deal_item, parse_ticker, write_fut_subscribe, MexcDepthFrame, MexcFutDealsWalk,
    MexcFutKind, MexcTickerFrame,
};
pub use run_loop::{
    drive_one, note_transport_ready, run_multi, Driver, MexcConn, RunResult, State, StopFlag,
    RX_BUF_SIZE, SUB_DROP_REFUSED, TICK_RING_CAP, TX_BUF_SIZE,
};
pub use spot::{
    classify_spot, extract_param_channel, extract_param_symbol, parse_book_ticker_body,
    parse_deal_item, parse_spot_wrapper, parse_sub_ack, trade_id_seq, write_spot_subscribe,
    MexcAckParams, MexcBookTicker, MexcDealsWalk, MexcSpotAck, MexcSpotFrame, MexcSpotKind,
};

use core_types::SymbolId;

// ---------------------------------------------------------------
// Connection sizing (MEASURED — plan §1.1 / §1.2 / §4 D1)
// ---------------------------------------------------------------

/// Symbol-table capacity of ONE connection (both classes).
pub const MEXC_MAX_SYMBOLS_PER_CONN: usize = 16;

/// Spot subscriptions one connection may hold — MEASURED: 32
/// requested, 30 ever pushed (plan §1.1).
pub const MEXC_SPOT_SUBS_PER_CONN: usize = 30;

/// Spot symbols per connection: 2 channels each at the measured
/// 30-subscription cap. The cli chunks the spot universe by this.
pub const MEXC_SPOT_SYMBOLS_PER_CONN: usize = 15;

/// Futures symbols per connection: 3 channels each (`depth.full` +
/// `deal` + `ticker` for EVERY perp, ruling Q-MX2) = 39 subscriptions,
/// under the measured ≥ 40 (plan §1.2). The cli chunks the perp
/// universe by this.
pub const MEXC_FUT_SYMBOLS_PER_CONN: usize = 13;

/// Longest venue symbol a connection table accepts (`AAPLSTOCK_USDT`
/// class; generous).
pub const MEXC_SYMBOL_MAX: usize = 24;

const _SIZING: () = {
    assert!(MEXC_SPOT_SYMBOLS_PER_CONN * 2 <= MEXC_SPOT_SUBS_PER_CONN);
    assert!(MEXC_SPOT_SYMBOLS_PER_CONN <= MEXC_MAX_SYMBOLS_PER_CONN);
    assert!(MEXC_FUT_SYMBOLS_PER_CONN * 3 < 40);
    assert!(MEXC_FUT_SYMBOLS_PER_CONN <= MEXC_MAX_SYMBOLS_PER_CONN);
    assert!(MEXC_SYMBOL_MAX <= u8::MAX as usize);
};

// ---------------------------------------------------------------
// Endpoints + keepalive payloads
// ---------------------------------------------------------------

/// Default spot WS host (the cli's `MEXC_WS_HOST` overrides it).
pub const SPOT_WS_HOST: &[u8] = b"wbs-api.mexc.com";
/// Spot WS path.
pub const SPOT_WS_PATH: &[u8] = b"/ws";
/// Default futures WS host (the cli's `MEXC_FUT_WS_HOST` overrides it).
pub const FUT_WS_HOST: &[u8] = b"contract.mexc.com";
/// Futures WS path.
pub const FUT_WS_PATH: &[u8] = b"/edge";
/// Spot keepalive probe; the venue answers
/// `{"id":0,"code":0,"msg":"PONG"}` (text).
pub const SPOT_PING_PAYLOAD: &[u8] = br#"{"method":"PING"}"#;
/// Futures keepalive probe; the venue answers
/// `{"channel":"pong","data":<ms>,"ts":<ms>}`.
pub const FUT_PING_PAYLOAD: &[u8] = br#"{"method":"ping"}"#;

// ---------------------------------------------------------------
// Connection class + channels
// ---------------------------------------------------------------

/// The two MEXC connection classes (one per driver).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MexcClass {
    /// Spot — protobuf pushes on `/ws`.
    Spot = 0,
    /// Perpetual futures — JSON pushes on `/edge`.
    Futures = 1,
}

impl MexcClass {
    /// Default WS host of the class.
    #[inline]
    pub const fn default_ws_host(self) -> &'static [u8] {
        match self {
            MexcClass::Spot => SPOT_WS_HOST,
            MexcClass::Futures => FUT_WS_HOST,
        }
    }

    /// WS path of the class.
    #[inline]
    pub const fn ws_path(self) -> &'static [u8] {
        match self {
            MexcClass::Spot => SPOT_WS_PATH,
            MexcClass::Futures => FUT_WS_PATH,
        }
    }

    /// Keepalive text payload of the class.
    #[inline]
    pub const fn ping_payload(self) -> &'static [u8] {
        match self {
            MexcClass::Spot => SPOT_PING_PAYLOAD,
            MexcClass::Futures => FUT_PING_PAYLOAD,
        }
    }

    /// Channels subscribed per symbol (spot 2, futures 3).
    #[inline]
    pub const fn channels_per_symbol(self) -> usize {
        match self {
            MexcClass::Spot => 2,
            MexcClass::Futures => 3,
        }
    }

    /// Symbols per connection (the cli's chunk size).
    #[inline]
    pub const fn symbols_per_conn(self) -> usize {
        match self {
            MexcClass::Spot => MEXC_SPOT_SYMBOLS_PER_CONN,
            MexcClass::Futures => MEXC_FUT_SYMBOLS_PER_CONN,
        }
    }

    /// Log label.
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            MexcClass::Spot => "spot",
            MexcClass::Futures => "futures",
        }
    }
}

/// Every channel this ingress subscribes. The discriminant is the
/// VENUE-LOCAL channel id carried in `SubDrop.v1` (wire-stable: never
/// renumber).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MexcChannel {
    /// Spot `aggre.bookTicker@10ms` — the spot BBO.
    SpotBookTicker = 0,
    /// Spot `aggre.deals@10ms` — spot prints.
    SpotDeals = 1,
    /// Futures `depth.full` (limit 5) — the futures BBO.
    FutDepthFull = 2,
    /// Futures `deal` — futures prints.
    FutDeal = 3,
    /// Futures `ticker` — mark / index / funding / open interest.
    FutTicker = 4,
}

impl MexcChannel {
    /// The class that carries this channel.
    #[inline]
    pub const fn class(self) -> MexcClass {
        match self {
            MexcChannel::SpotBookTicker | MexcChannel::SpotDeals => MexcClass::Spot,
            MexcChannel::FutDepthFull | MexcChannel::FutDeal | MexcChannel::FutTicker => {
                MexcClass::Futures
            }
        }
    }

    /// Per-class slot `0..channels_per_symbol` — the bit index of the
    /// driver's per-row confirmed mask and the subscribe order.
    #[inline]
    pub const fn slot(self) -> u8 {
        match self {
            MexcChannel::SpotBookTicker | MexcChannel::FutDepthFull => 0,
            MexcChannel::SpotDeals | MexcChannel::FutDeal => 1,
            MexcChannel::FutTicker => 2,
        }
    }

    /// Inverse of [`Self::slot`] within a class.
    #[inline]
    pub const fn from_slot(class: MexcClass, slot: u8) -> Option<Self> {
        match (class, slot) {
            (MexcClass::Spot, 0) => Some(MexcChannel::SpotBookTicker),
            (MexcClass::Spot, 1) => Some(MexcChannel::SpotDeals),
            (MexcClass::Futures, 0) => Some(MexcChannel::FutDepthFull),
            (MexcClass::Futures, 1) => Some(MexcChannel::FutDeal),
            (MexcClass::Futures, 2) => Some(MexcChannel::FutTicker),
            _ => None,
        }
    }

    /// Subscribe text: the spot param PREFIX (the symbol follows) or
    /// the futures `method`.
    #[inline]
    pub const fn topic(self) -> &'static [u8] {
        match self {
            MexcChannel::SpotBookTicker => b"spot@public.aggre.bookTicker.v3.api.pb@10ms@",
            MexcChannel::SpotDeals => b"spot@public.aggre.deals.v3.api.pb@10ms@",
            MexcChannel::FutDepthFull => b"sub.depth.full",
            MexcChannel::FutDeal => b"sub.deal",
            MexcChannel::FutTicker => b"sub.ticker",
        }
    }

    /// The `SubDrop.v1` value (venue-local discriminant).
    #[inline]
    pub const fn discriminant(self) -> i64 {
        self as u8 as i64
    }
}

// ---------------------------------------------------------------
// One print (both classes)
// ---------------------------------------------------------------

/// [`MexcDeal::side`]: the aggressor BOUGHT.
pub const DEAL_SIDE_BUY: u8 = 0;
/// [`MexcDeal::side`]: the aggressor SOLD.
pub const DEAL_SIDE_SELL: u8 = 1;

/// One parsed print (spot deals item or futures deal item). 64-byte
/// POD.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct MexcDeal {
    /// Price ×1e6.
    pub px_1e6: i64,
    /// Quantity ×1e6, unsigned (spot base units; futures contracts).
    pub qty_1e6: i64,
    /// Venue trade time ms (0 = the item carried none).
    pub time_ms: u64,
    /// Full-width venue trade seq (Q-MX1; 0 = absent).
    pub trade_seq: u64,
    /// [`DEAL_SIDE_BUY`] or [`DEAL_SIDE_SELL`].
    pub side: u8,
    // Explicit tail padding.
    _pad: [u8; 31],
}

impl MexcDeal {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        px_1e6: 0,
        qty_1e6: 0,
        time_ms: 0,
        trade_seq: 0,
        side: 0,
        _pad: [0; 31],
    };
}

impl MexcDeal {
    #[inline]
    pub(crate) const fn new(px_1e6: i64, qty_1e6: i64, time_ms: u64, trade_seq: u64, side: u8) -> Self {
        Self {
            px_1e6,
            qty_1e6,
            time_ms,
            trade_seq,
            side,
            _pad: [0; 31],
        }
    }

    /// The cross-venue signed quantity: negated when the aggressor
    /// sold (§6.5 `Trade.v1`).
    #[inline]
    pub const fn signed_qty_1e6(&self) -> i64 {
        if self.side == DEAL_SIDE_SELL {
            -self.qty_1e6
        } else {
            self.qty_1e6
        }
    }
}

const _POD_SIZES: () = {
    assert!(::core::mem::size_of::<MexcDeal>() == 64);
};

// ---------------------------------------------------------------
// Funding clock (ruling Q-MX3)
// ---------------------------------------------------------------

/// Milliseconds per hour (`collectCycle` is in hours).
pub const MS_PER_HOUR: u64 = 3_600_000;

/// The next-settle instant as of venue time `venue_ms`, given the
/// latched `next_settle_ms` and the period `cycle_ms`: unchanged while
/// `venue_ms < next_settle_ms`; once the venue time REACHES it, advanced
/// by the whole number of elapsed periods (computed, never looped) so
/// the result is strictly after `venue_ms`. Unseeded (`next_settle_ms
/// == 0`) or `cycle_ms == 0` ⇒ 0 ("unknown" — the vm ignores `v1 = 0`);
/// `venue_ms == 0` (unknown) never advances. Saturates, never wraps.
#[inline]
pub const fn funding_next_settle_ms(next_settle_ms: u64, cycle_ms: u64, venue_ms: u64) -> u64 {
    if next_settle_ms == 0 || cycle_ms == 0 {
        return 0;
    }
    if venue_ms < next_settle_ms {
        return next_settle_ms;
    }
    let periods = (venue_ms - next_settle_ms) / cycle_ms + 1;
    next_settle_ms.saturating_add(periods.saturating_mul(cycle_ms))
}

// ---------------------------------------------------------------
// Symbol table (per connection)
// ---------------------------------------------------------------

/// Why a [`MexcSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolTableErr {
    /// Table full ([`MEXC_MAX_SYMBOLS_PER_CONN`]).
    Full,
    /// Symbol longer than [`MEXC_SYMBOL_MAX`].
    TooLong,
    /// Symbol empty.
    Empty,
    /// Symbol already present (a duplicate would double-subscribe and
    /// alias the per-row state).
    Duplicate,
}

/// Fixed-capacity `SYMBOL → SymbolId` map for ONE connection. Spot
/// `BTCUSDT` and perp `BTC_USDT` are different instruments sharing one
/// venue — each connection owns its table. Linear scan (N ≤ 16).
pub struct MexcSymbolTable {
    rows: [(u8, [u8; MEXC_SYMBOL_MAX], SymbolId); MEXC_MAX_SYMBOLS_PER_CONN],
    len: usize,
}

impl MexcSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; MEXC_SYMBOL_MAX], 0); MEXC_MAX_SYMBOLS_PER_CONN],
            len: 0,
        }
    }

    /// Register `symbol → sym` (boot-time).
    pub fn insert(&mut self, symbol: &[u8], sym: SymbolId) -> Result<(), SymbolTableErr> {
        if symbol.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if symbol.len() > MEXC_SYMBOL_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if self.lookup(symbol).is_some() {
            return Err(SymbolTableErr::Duplicate);
        }
        let Some(row) = self.rows.get_mut(self.len) else {
            return Err(SymbolTableErr::Full);
        };
        row.0 = symbol.len() as u8;
        // COPY: venue symbol ≤ 24 B, once at boot — the table owns fixed
        // rows so the hot lookup compares in place with no pointer chase
        // — borrowing the config's String rejected: the driver moves to
        // the ingress thread and must not pin boot allocations.
        row.1[..symbol.len()].copy_from_slice(symbol);
        row.2 = sym;
        self.len += 1;
        Ok(())
    }

    /// Resolve a venue symbol to `(row index, sym)`. Hot path: length
    /// gate, then an in-place bytewise compare.
    #[inline]
    pub fn lookup(&self, symbol: &[u8]) -> Option<(usize, SymbolId)> {
        let rows = self.rows.get(..self.len)?;
        let n = symbol.len();
        let mut i = 0;
        while i < rows.len() {
            let row = &rows[i];
            if row.0 as usize == n && row.1.get(..n) == Some(symbol) {
                return Some((i, row.2));
            }
            i += 1;
        }
        None
    }

    /// Row accessor: `(symbol, sym)`.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<(&[u8], SymbolId)> {
        let row = self.rows.get(..self.len)?.get(idx)?;
        Some((row.1.get(..row.0 as usize)?, row.2))
    }

    /// Configured symbol count.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no symbol is configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for MexcSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Shared byte helpers
// ---------------------------------------------------------------

/// Append `src` at `at` in `dst` (subscribe rendering into stack
/// scratch). `None` when `dst` is too small.
#[inline]
pub(crate) fn push_bytes(dst: &mut [u8], at: usize, src: &[u8]) -> Option<usize> {
    let end = at.checked_add(src.len())?;
    let slot = dst.get_mut(at..end)?;
    // COPY: subscribe text ≤ the render scratch (4 KiB spot / 128 B
    // futures), once per session — the WS frame header needs the
    // payload length before the payload is masked into tx — rendering
    // straight into tx rejected: the header width (7/16-bit length) is
    // unknown until the render ends.
    slot.copy_from_slice(src);
    Some(end)
}

/// Checked ASCII-digit run at `pos` → `(value, end)`. `None` when no
/// digit is present or the run overflows `u64` (never wraps — unlike
/// `core_parse::scan_u64`, whose wrap would turn a garbage id into a
/// plausible one).
#[inline]
pub(crate) fn scan_u64_checked(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut i = pos;
    let mut v: u64 = 0;
    while i < buf.len() {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
        i += 1;
    }
    if i == pos {
        None
    } else {
        Some((v, i))
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_constants_and_chunking() {
        assert_eq!(MexcClass::Spot.default_ws_host(), b"wbs-api.mexc.com");
        assert_eq!(MexcClass::Spot.ws_path(), b"/ws");
        assert_eq!(MexcClass::Futures.default_ws_host(), b"contract.mexc.com");
        assert_eq!(MexcClass::Futures.ws_path(), b"/edge");
        assert_eq!(MexcClass::Spot.ping_payload(), br#"{"method":"PING"}"#);
        assert_eq!(MexcClass::Futures.ping_payload(), br#"{"method":"ping"}"#);
        assert_eq!(MexcClass::Spot.channels_per_symbol(), 2);
        assert_eq!(MexcClass::Futures.channels_per_symbol(), 3);
        // The measured caps: 30 spot subs, ≥ 40 futures subs.
        assert_eq!(MexcClass::Spot.symbols_per_conn() * 2, 30);
        assert_eq!(MexcClass::Futures.symbols_per_conn() * 3, 39);
        assert_eq!(MexcClass::Spot.label(), "spot");
        assert_eq!(MexcClass::Futures.label(), "futures");
    }

    #[test]
    fn channel_slots_roundtrip_and_reject_foreign_slots() {
        let all = [
            MexcChannel::SpotBookTicker,
            MexcChannel::SpotDeals,
            MexcChannel::FutDepthFull,
            MexcChannel::FutDeal,
            MexcChannel::FutTicker,
        ];
        let mut i = 0;
        while i < all.len() {
            let ch = all[i];
            assert_eq!(MexcChannel::from_slot(ch.class(), ch.slot()), Some(ch));
            assert_eq!(ch.discriminant(), i as i64, "wire-stable discriminants");
            i += 1;
        }
        assert_eq!(MexcChannel::from_slot(MexcClass::Spot, 2), None);
        assert_eq!(MexcChannel::from_slot(MexcClass::Futures, 3), None);
        assert_eq!(
            MexcChannel::SpotBookTicker.topic(),
            b"spot@public.aggre.bookTicker.v3.api.pb@10ms@"
        );
        assert_eq!(MexcChannel::FutDepthFull.topic(), b"sub.depth.full");
    }

    #[test]
    fn deal_signed_qty_follows_the_aggressor() {
        let buy = MexcDeal::new(1, 5, 0, 0, DEAL_SIDE_BUY);
        let sell = MexcDeal::new(1, 5, 0, 0, DEAL_SIDE_SELL);
        assert_eq!(buy.signed_qty_1e6(), 5);
        assert_eq!(sell.signed_qty_1e6(), -5);
    }

    #[test]
    fn funding_clock_advances_exactly_at_settlement() {
        const H8: u64 = 8 * MS_PER_HOUR;
        let t0 = 1_789_920_000_000u64; // plan §1.4 nextSettleTime
        // Before: unchanged.
        assert_eq!(funding_next_settle_ms(t0, H8, t0 - 1), t0);
        // AT the instant: the period settled — advance one cycle.
        assert_eq!(funding_next_settle_ms(t0, H8, t0), t0 + H8);
        // After, inside the next period: one cycle.
        assert_eq!(funding_next_settle_ms(t0, H8, t0 + 1), t0 + H8);
        // A long outage: whole periods computed, strictly after venue.
        let late = t0 + 5 * H8 + 7;
        let next = funding_next_settle_ms(t0, H8, late);
        assert_eq!(next, t0 + 6 * H8);
        assert!(next > late);
        // Exactly on a later boundary: that period settled too.
        assert_eq!(funding_next_settle_ms(t0, H8, t0 + 2 * H8), t0 + 3 * H8);
    }

    #[test]
    fn funding_clock_failure_modes() {
        assert_eq!(funding_next_settle_ms(0, 8 * MS_PER_HOUR, 5), 0, "unseeded");
        assert_eq!(funding_next_settle_ms(1_000, 0, 5_000), 0, "cycle 0");
        assert_eq!(funding_next_settle_ms(1_000, 10, 0), 1_000, "unknown time never advances");
        // Saturates instead of wrapping.
        assert_eq!(funding_next_settle_ms(u64::MAX - 1, u64::MAX / 2, u64::MAX), u64::MAX);
    }

    #[test]
    fn symbol_table_laws() {
        let mut t = MexcSymbolTable::new();
        assert!(t.is_empty());
        t.insert(b"BTCUSDT", 1).unwrap();
        t.insert(b"AAPLXUSDT", 2).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.lookup(b"BTCUSDT"), Some((0, 1)));
        assert_eq!(t.lookup(b"AAPLXUSDT"), Some((1, 2)));
        assert_eq!(t.lookup(b"BTCUSD"), None, "prefix is not a match");
        assert_eq!(t.lookup(b""), None);
        assert_eq!(t.get(0), Some((&b"BTCUSDT"[..], 1)));
        assert_eq!(t.get(2), None);
        assert_eq!(t.insert(b"", 3), Err(SymbolTableErr::Empty));
        assert_eq!(t.insert(&[b'A'; MEXC_SYMBOL_MAX + 1], 3), Err(SymbolTableErr::TooLong));
        assert_eq!(t.insert(b"BTCUSDT", 9), Err(SymbolTableErr::Duplicate));
        let mut full = MexcSymbolTable::default();
        let mut k = 0u32;
        while k < MEXC_MAX_SYMBOLS_PER_CONN as u32 {
            let name = [b'A' + (k % 26) as u8, b'A' + (k / 26) as u8];
            full.insert(&name, k + 1).unwrap();
            k += 1;
        }
        assert_eq!(full.insert(b"OVER", 999), Err(SymbolTableErr::Full));
    }

    #[test]
    fn checked_digit_scan_never_wraps() {
        assert_eq!(scan_u64_checked(b"123x", 0), Some((123, 3)));
        assert_eq!(scan_u64_checked(b"x", 0), None);
        assert_eq!(scan_u64_checked(b"", 0), None);
        assert_eq!(scan_u64_checked(b"18446744073709551615", 0), Some((u64::MAX, 20)));
        assert_eq!(scan_u64_checked(b"18446744073709551616", 0), None, "overflow");
    }

    #[test]
    fn push_bytes_bounds() {
        let mut d = [0u8; 4];
        assert_eq!(push_bytes(&mut d, 0, b"ab"), Some(2));
        assert_eq!(push_bytes(&mut d, 2, b"cd"), Some(4));
        assert_eq!(push_bytes(&mut d, 4, b"e"), None);
        assert_eq!(push_bytes(&mut d, usize::MAX, b"e"), None);
        assert_eq!(&d, b"abcd");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Q-MX3: the advanced instant is always a whole number of
        /// cycles after the seed and strictly after a reaching venue
        /// time; it never moves before the venue time reaches it.
        #[test]
        fn funding_clock_is_period_aligned(
            seed in 1u64..4_000_000_000_000u64,
            cycle_h in 1u64..=24u64,
            dt in 0u64..1_000_000_000u64,
            before in any::<bool>(),
        ) {
            let cycle = cycle_h * MS_PER_HOUR;
            // `before`: a venue time in [0, seed) (0 = unknown).
            let venue = if before { seed - 1 - dt % seed } else { seed + dt };
            let next = funding_next_settle_ms(seed, cycle, venue);
            if venue < seed {
                prop_assert_eq!(next, seed);
            } else {
                prop_assert!(next > venue);
                prop_assert_eq!((next - seed) % cycle, 0);
                prop_assert!(next - venue <= cycle);
            }
        }
    }
}
