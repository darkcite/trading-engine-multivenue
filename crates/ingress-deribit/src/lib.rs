//! # ingress-deribit
//!
//! Deribit **public** market-data ingress over JSON-RPC 2.0 / WebSocket
//! (Phase 8c). Endpoint `wss://www.deribit.com/ws/api/v2` (test:
//! `test.deribit.com`). Channels per `docs/phase-8-plan.md` §4.2
//! (venue facts verified 2026-08-14):
//!
//! * `quote.{instr}`         — BBO pushes → [`core_types::Tick`] lane
//!   (`VenueId::Deribit = 3`)
//! * `ticker.{instr}.100ms`  — mark, index, `current_funding`,
//!   price limits, open interest
//! * `trades.{instr}.100ms`  — batched trade rows, `trade_seq`-sequential
//! * `book.{instr}.100ms`    — `change_id` chain, behind `--deribit-depth`;
//!   consumed for **capture + integrity** only (§4.5)
//!
//! ## Subscribe batching (credit budget)
//!
//! One `public/subscribe` call costs **3000 credits of a 30 000 pool**
//! (~3.3 calls/s refill) — the run loop batches **all** configured
//! `(channel × instrument)` pairs into a single call
//! ([`write_subscribe_all`]). The subscribe *result* echoes the list of
//! successfully-subscribed channels; any expected channel missing from
//! the result is a misconfiguration and fails the session (fail-fast).
//!
//! ## Integrity (§6.2)
//!
//! * **Books** chain `change_id` → `prev_change_id`
//!   ([`DeribitBookChain`]). The first notification after subscribe is
//!   a full **snapshot of unbounded depth** — parsing caps the level
//!   walk at [`DEPTH_CAP`] = 64 per side, **excess is counted, not
//!   stored** ([`DeribitBookFrame::excess_bids`]/`excess_asks`).
//!   `prev_change_id` mismatch ⇒ resubscribe that book channel (official
//!   guidance) + `gaps_total`.
//! * **Trades** carry a per-instrument, strictly-sequential
//!   `trade_seq` ([`DeribitTradeSeq`]): `last + 1` chains; a jump is a
//!   [`TradeSeqOutcome::Gap`] (missed trades — counted; **no**
//!   resubscribe: Deribit does not replay missed trade notifications);
//!   a regression is counted and resyncs.
//!
//! ## Heartbeat protocol (no WS-level ping)
//!
//! Deribit has no client ping. After connect the run loop calls
//! `public/set_heartbeat {"interval": 15}`; the venue then emits
//! `test_request` heartbeats that **must** be answered with
//! `public/test` or the venue closes the socket. Request/response
//! correlation runs on `core_net::subs::PendingTable` (JSON-RPC ids,
//! monotonic from 1). Proactive `public/test` doubles as the idle
//! probe (`KeepaliveAction::SendPing`); the idle budget is ~2× the
//! heartbeat interval.
//!
//! ## Sequence numbers on the Tick lane
//!
//! `quote` pushes carry **no venue sequence number**. Decision (8c):
//! `Tick.venue_seq = timestamp_ms as u32` — venue-provided, monotonic
//! across reconnects, wraps every ~49.7 days. Two quotes inside the
//! same millisecond collapse at the `TopOfBook` boundary (equal seq =
//! stale) — acceptable at BBO granularity. Books use full-width
//! `change_id` and trades full-width `trade_seq` inside the monitors;
//! u32 truncation only ever happens at the Tick boundary (same policy
//! as OKX).
//!
//! ## Amount normalization
//!
//! Deribit amounts for futures/perpetuals are **USD notional** (not
//! contracts, not coin). `Qty(1e6)` therefore carries USD × 1e6 for
//! this venue. `tick_size_steps` (price-banded tick sizes) and
//! `contract_size` arrive with REST instrument discovery, which is
//! deferred to the 8e boot-coverage audit like OKX — until then the
//! ingress publishes raw venue prices ×1e6 and performs no
//! step-rounding (market-data capture does not quantize).
//!
//! ## Zero-copy note (house doctrine)
//!
//! All parsing is in-place over `&[u8]` in the rx buffer. The one
//! unavoidable copy per event is the 64-byte parsed POD moved into the
//! SPSC ring by `try_push` (ownership transfer) — same as every
//! ingress. Requests render into fixed stack scratch; no heap after
//! construction.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod run_loop;
pub mod discovery;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, PENDING_CAP,
    RX_BUF_SIZE, TX_BUF_SIZE,
};

use core_net::SubId;
use core_parse::{find_field, scan_i64, scan_number_sci_1e6, scan_number_sci_1e9, scan_u64, skip_byte};
use core_types::{NsTs, SymbolId};

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Longest Deribit `instrument_name` we accept
/// (`BTC_USDC-PERPETUAL` = 18, dated futures ≤ 15; margin for
/// combo-style names). Table rows are fixed at this width.
pub const DERIBIT_INSTR_MAX: usize = 32;

/// Maximum CONFIGURED (static) instruments per connection — the
/// pre-M2 law, unchanged: these subscribe the full channel set
/// (quote + ticker + trades [+ book]). Fixed-cap tables everywhere;
/// boot fails fast beyond this.
pub const DERIBIT_STATIC_MAX: usize = 16;

/// Maximum boot-DISCOVERED capped-chain OPTION instruments per
/// connection (M2.1, docs/m2-progress.md design entry). Options
/// subscribe QUOTE ONLY (mark/IV `ticker` arrives at M2.3). Sized so
/// the default policy (2 underlyings × E2 × K8 × C/P = 64) fits
/// exactly; a larger configured policy fails fast at table build with
/// an actionable message.
pub const DERIBIT_OPT_MAX: usize = 64;

/// Total symbol-table capacity: the static block + the options block.
/// Kept as the single sizing constant for per-row state arrays
/// (book chains / trade seqs — options rows never use them, but
/// row-indexed arrays stay uniform).
pub const DERIBIT_MAX_SYMBOLS: usize = DERIBIT_STATIC_MAX + DERIBIT_OPT_MAX;

/// Book snapshot level cap per side — levels beyond this are counted
/// ([`DeribitBookFrame::excess_bids`]/`excess_asks`), not stored.
pub const DEPTH_CAP: usize = 64;

/// `public/set_heartbeat` interval we request (venue minimum is 10 s).
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;

/// Channel-name suffix for the 100 ms non-auth cadence.
pub const SUFFIX_100MS: &[u8] = b".100ms";

// ---------------------------------------------------------------
// Channels + message classification
// ---------------------------------------------------------------

/// Public channels this ingress speaks. `#[repr(u8)]` so the value
/// can ride in PODs and subscribe-verification masks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeribitChannel {
    /// `quote.{instr}` — BBO.
    Quote = 0,
    /// `ticker.{instr}.100ms`.
    Ticker = 1,
    /// `trades.{instr}.100ms`.
    Trades = 2,
    /// `book.{instr}.100ms` (behind `--deribit-depth`).
    Book = 3,
}

impl DeribitChannel {
    /// Wire prefix up to and including the dot before the instrument.
    #[inline]
    pub const fn wire_prefix(self) -> &'static [u8] {
        match self {
            DeribitChannel::Quote => b"quote.",
            DeribitChannel::Ticker => b"ticker.",
            DeribitChannel::Trades => b"trades.",
            DeribitChannel::Book => b"book.",
        }
    }

    /// Wire suffix after the instrument (`.100ms` cadence for all
    /// non-quote channels; `quote` has no interval variant).
    #[inline]
    pub const fn wire_suffix(self) -> &'static [u8] {
        match self {
            DeribitChannel::Quote => b"",
            _ => SUFFIX_100MS,
        }
    }
}

/// Coarse classification of one inbound text frame. Cheap byte scans
/// only — full parsing happens per-channel afterwards. Order of
/// checks follows hot-path frequency: subscription pushes first.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeribitMsgKind {
    /// `{"method":"subscription","params":{"channel":"<ch>.<instr>...`.
    Notification(DeribitChannel),
    /// `{"method":"heartbeat","params":{"type":"test_request"}}` —
    /// **must** be answered with `public/test`.
    TestRequest,
    /// `{"method":"heartbeat","params":{"type":"heartbeat"}}` —
    /// venue liveness beat, activity only.
    Heartbeat,
    /// `{"id":N,"result":...}` — response to one of our requests.
    RpcResult(u64),
    /// `{"id":N,"error":{"code":C,...}}`. Fatal for the session.
    RpcError {
        /// Echoed request id (0 when absent/unparseable).
        id: u64,
        /// Venue error code.
        code: i32,
    },
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// Classify one inbound payload. Zero-alloc; key-matched so field
/// order never matters.
#[inline]
pub fn classify(payload: &[u8]) -> DeribitMsgKind {
    if memchr::memmem::find(payload, b"\"method\":\"subscription\"").is_some() {
        // Channel prefixes end at the dot — no aliasing possible
        // among quote./ticker./trades./book. names.
        if memchr::memmem::find(payload, b"\"channel\":\"quote.").is_some() {
            return DeribitMsgKind::Notification(DeribitChannel::Quote);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"ticker.").is_some() {
            return DeribitMsgKind::Notification(DeribitChannel::Ticker);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"trades.").is_some() {
            return DeribitMsgKind::Notification(DeribitChannel::Trades);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"book.").is_some() {
            return DeribitMsgKind::Notification(DeribitChannel::Book);
        }
        return DeribitMsgKind::Unknown;
    }
    if memchr::memmem::find(payload, b"\"method\":\"heartbeat\"").is_some() {
        if memchr::memmem::find(payload, b"\"type\":\"test_request\"").is_some() {
            return DeribitMsgKind::TestRequest;
        }
        return DeribitMsgKind::Heartbeat;
    }
    if memchr::memmem::find(payload, b"\"error\":").is_some() {
        let id = find_field(payload, b"\"id\":")
            .and_then(|p| scan_u64(payload, p))
            .map(|(v, _)| v)
            .unwrap_or(0);
        let code = find_field(payload, b"\"code\":")
            .and_then(|p| scan_i64(payload, p))
            .map(|(v, _)| v as i32)
            .unwrap_or(0);
        return DeribitMsgKind::RpcError { id, code };
    }
    if memchr::memmem::find(payload, b"\"result\"").is_some() {
        if let Some((id, _)) =
            find_field(payload, b"\"id\":").and_then(|p| scan_u64(payload, p))
        {
            return DeribitMsgKind::RpcResult(id);
        }
    }
    DeribitMsgKind::Unknown
}

/// Extract the instrument bytes from the notification's
/// `"channel":"<prefix><instr>[.100ms]"` value. Returns a subslice of
/// `payload`; no copy. Instrument names contain no dots (documented
/// invariant: perps `BTC-PERPETUAL`, dated futures `BTC-27MAR26`,
/// spot `BTC_USDC` — options are out of v1 scope), so the instrument
/// ends at the next `.` or `"`.
#[inline]
pub fn extract_instrument(payload: &[u8], channel: DeribitChannel) -> Option<&[u8]> {
    let key_pos = find_field(payload, b"\"channel\":")?;
    let val = skip_byte(payload, key_pos, b'"');
    let prefix = channel.wire_prefix();
    let rest = payload.get(val..)?;
    if !rest.starts_with(prefix) {
        return None;
    }
    let instr_start = val + prefix.len();
    let tail = payload.get(instr_start..)?;
    let mut i = 0;
    while i < tail.len() {
        let b = tail[i];
        if b == b'.' || b == b'"' {
            return payload.get(instr_start..instr_start + i);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------
// Frame PODs — one cache line each, explicit padding
// ---------------------------------------------------------------

/// Parsed `quote.{instr}` push. A one-sided book reports the missing
/// side as px = 0, qty = 0 (Deribit sends JSON `null`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct DeribitQuoteFrame {
    /// Venue event time (`timestamp`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Venue `timestamp` in raw milliseconds (source of
    /// `Tick.venue_seq` — see crate header).
    pub ts_ms: u64,
    /// Best bid price ×1e6 (0 = side empty).
    pub bid_px_1e6: i64,
    /// Best bid amount ×1e6 (USD notional for perps/futures).
    pub bid_qty_1e6: i64,
    /// Best ask price ×1e6 (0 = side empty).
    pub ask_px_1e6: i64,
    /// Best ask amount ×1e6.
    pub ask_qty_1e6: i64,
    /// Resolved symbol (venue-namespaced, bits 31..24 = Deribit).
    pub sym: SymbolId,
    // Explicit tail padding — keeps the slot exactly 64 B.
    _pad: [u8; 12],
}

/// Parsed `ticker.{instr}.100ms` push — mark/index/funding/limits/OI
/// per plan §4.2. All seven payload fields fit one cache line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct DeribitTickerFrame {
    /// Venue event time (`timestamp`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// `mark_price` ×1e6.
    pub mark_px_1e6: i64,
    /// `index_price` ×1e6.
    pub index_px_1e6: i64,
    /// `current_funding` ×1e9 (funding resolution exceeds 1e6).
    pub current_funding_1e9: i64,
    /// `open_interest` ×1e6 (USD notional for perps/futures).
    pub open_interest_1e6: i64,
    /// `min_price` (lower price limit) ×1e6.
    pub min_px_1e6: i64,
    /// `max_price` (upper price limit) ×1e6.
    pub max_px_1e6: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    // Explicit tail padding.
    _pad: [u8; 4],
}

/// Parsed `trades.{instr}.100ms` row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct DeribitTradeFrame {
    /// Per-instrument sequential `trade_seq` (full width — see
    /// [`DeribitTradeSeq`]).
    pub trade_seq: i64,
    /// Venue event time (`timestamp`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Trade price ×1e6.
    pub px_1e6: i64,
    /// Trade amount ×1e6 (USD notional for perps/futures).
    pub qty_1e6: i64,
    /// Numeric `trade_id` (0 when the venue string is non-numeric —
    /// identity is not consumed in 8c; `trade_seq` is the integrity
    /// key).
    pub trade_id: u64,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// Taker direction: 0 = buy, 1 = sell (wire `direction`).
    pub side: u8,
    // Explicit tail padding.
    _pad: [u8; 15],
}

/// Parsed `book.{instr}.100ms` push **header** — §4.5: depth is
/// consumed for capture + integrity, so only the chain fields, event
/// time and level counts are lifted; levels stay in the rx buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct DeribitBookFrame {
    /// `change_id` of this message.
    pub change_id: i64,
    /// `prev_change_id` (`-1` on snapshots — field absent on wire).
    pub prev_change_id: i64,
    /// Venue event time (`timestamp`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// Bid levels seen, capped at [`DEPTH_CAP`].
    pub n_bids: u16,
    /// Ask levels seen, capped at [`DEPTH_CAP`].
    pub n_asks: u16,
    /// Bid levels beyond the cap — counted, not stored.
    pub excess_bids: u16,
    /// Ask levels beyond the cap — counted, not stored.
    pub excess_asks: u16,
    /// 0 = snapshot, 1 = change (wire `type`).
    pub action: u8,
    // Explicit tail padding.
    _pad: [u8; 27],
}

/// `action` value for a book snapshot (`"type":"snapshot"`).
pub const BOOK_ACTION_SNAPSHOT: u8 = 0;
/// `action` value for a book incremental (`"type":"change"`).
pub const BOOK_ACTION_CHANGE: u8 = 1;

const _SIZE_CHECKS: () = {
    assert!(::core::mem::size_of::<DeribitQuoteFrame>() == 64);
    assert!(::core::mem::size_of::<DeribitTickerFrame>() == 64);
    assert!(::core::mem::size_of::<DeribitTradeFrame>() == 64);
    assert!(::core::mem::size_of::<DeribitBookFrame>() == 64);
};

// ---------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------

/// Parse a bare millisecond timestamp field (`"timestamp":1550658624149`)
/// into `(ns, raw_ms)`. Deribit numbers are unquoted.
#[inline]
fn scan_ms_field(buf: &[u8], key: &[u8]) -> Option<(u64, u64)> {
    let pos = find_field(buf, key)?;
    let (ms, _) = scan_u64(buf, pos)?;
    Some((ms.saturating_mul(1_000_000), ms))
}

/// Parse an unquoted decimal price field ×1e6. `null` (empty side on
/// `quote`) yields 0. Scientific notation is accepted: the first live
/// raw-tap (2026-08-15) showed Deribit's starbase engine rendering
/// round floats as exponent forms (`"amount": 1.0e3`) — the strict
/// decimal scanner rejected ~1.3 % of messages.
#[inline]
fn scan_px_field_1e6(buf: &[u8], key: &[u8]) -> Option<i64> {
    let pos = find_field(buf, key)?;
    if buf.get(pos..pos + 4) == Some(b"null") {
        return Some(0);
    }
    let (v, _) = scan_number_sci_1e6(buf, pos)?;
    Some(v)
}

// ---------------------------------------------------------------
// Channel parsers
// ---------------------------------------------------------------

/// Parse a `quote` push into a [`DeribitQuoteFrame`]. `sym` is the
/// caller-resolved symbol (from [`extract_instrument`] + the symbol
/// table). Returns `None` on malformed input — caller counts it.
#[inline]
pub fn parse_quote(payload: &[u8], sym: SymbolId) -> Option<DeribitQuoteFrame> {
    let bid_px_1e6 = scan_px_field_1e6(payload, b"\"best_bid_price\":")?;
    let bid_qty_1e6 = scan_px_field_1e6(payload, b"\"best_bid_amount\":")?;
    let ask_px_1e6 = scan_px_field_1e6(payload, b"\"best_ask_price\":")?;
    let ask_qty_1e6 = scan_px_field_1e6(payload, b"\"best_ask_amount\":")?;
    let (ts_ns, ts_ms) = scan_ms_field(payload, b"\"timestamp\":")?;
    // A frame with both sides empty carries no information.
    if bid_px_1e6 == 0 && ask_px_1e6 == 0 {
        return None;
    }
    Some(DeribitQuoteFrame {
        ts_ns,
        ts_ms,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        sym,
        _pad: [0; 12],
    })
}

/// Parse a `ticker.{instr}.100ms` push into a [`DeribitTickerFrame`].
#[inline]
pub fn parse_ticker(payload: &[u8], sym: SymbolId) -> Option<DeribitTickerFrame> {
    // `"mark_price":`/`"index_price":` cannot false-match inside
    // `"settlement_price":` etc. — the leading quote anchors the key.
    let mark_px_1e6 = scan_px_field_1e6(payload, b"\"mark_price\":")?;
    let index_px_1e6 = scan_px_field_1e6(payload, b"\"index_price\":")?;
    let pos = find_field(payload, b"\"current_funding\":")?;
    let (current_funding_1e9, _) = scan_number_sci_1e9(payload, pos)?;
    let open_interest_1e6 = scan_px_field_1e6(payload, b"\"open_interest\":")?;
    let min_px_1e6 = scan_px_field_1e6(payload, b"\"min_price\":")?;
    let max_px_1e6 = scan_px_field_1e6(payload, b"\"max_price\":")?;
    let (ts_ns, _) = scan_ms_field(payload, b"\"timestamp\":")?;
    Some(DeribitTickerFrame {
        ts_ns,
        mark_px_1e6,
        index_px_1e6,
        current_funding_1e9,
        open_interest_1e6,
        min_px_1e6,
        max_px_1e6,
        sym,
        _pad: [0; 4],
    })
}

/// Parse one `trades` **row slice** into a [`DeribitTradeFrame`]. The
/// run loop slices rows at successive `"trade_seq":` markers; every
/// key here is matched inside the row slice only.
#[inline]
pub fn parse_trade(row: &[u8], sym: SymbolId) -> Option<DeribitTradeFrame> {
    let pos = find_field(row, b"\"trade_seq\":")?;
    let (trade_seq, _) = scan_i64(row, pos)?;
    let pos = find_field(row, b"\"price\":")?;
    let (px_1e6, _) = scan_number_sci_1e6(row, pos)?;
    let pos = find_field(row, b"\"amount\":")?;
    let (qty_1e6, _) = scan_number_sci_1e6(row, pos)?;
    let side = if memchr::memmem::find(row, b"\"direction\":\"buy\"").is_some() {
        0u8
    } else if memchr::memmem::find(row, b"\"direction\":\"sell\"").is_some() {
        1u8
    } else {
        return None;
    };
    let (ts_ns, _) = scan_ms_field(row, b"\"timestamp\":")?;
    // `trade_id` is a decimal string for futures/perps; non-numeric
    // forms degrade to 0 (identity unused in 8c).
    let trade_id = find_field(row, b"\"trade_id\":")
        .map(|p| skip_byte(row, p, b'"'))
        .and_then(|p| scan_u64(row, p))
        .map(|(v, _)| v)
        .unwrap_or(0);
    Some(DeribitTradeFrame {
        trade_seq,
        ts_ns,
        px_1e6,
        qty_1e6,
        trade_id,
        sym,
        side,
        _pad: [0; 15],
    })
}

/// Count the level entries of one side array (`"bids":[["new",px,amt],…]`)
/// without storing them. `pos` must point at the outer `[`. Returns
/// `(levels_within_cap, excess)` — the walk itself never stops early
/// (the chain fields may follow the array), only *storage* is capped.
/// Level strings (`new`/`change`/`delete`) contain no brackets, so a
/// plain bracket-depth walk is exact.
#[inline]
fn count_side_levels(buf: &[u8], pos: usize) -> Option<(u16, u16)> {
    if *buf.get(pos)? != b'[' {
        return None;
    }
    let mut depth = 1usize;
    let mut i = pos + 1;
    let mut n: u32 = 0;
    while depth > 0 {
        let b = *buf.get(i)?;
        if b == b'[' {
            if depth == 1 {
                n += 1;
            }
            depth += 1;
        } else if b == b']' {
            depth -= 1;
        }
        i += 1;
    }
    let capped = if n > DEPTH_CAP as u32 { DEPTH_CAP as u32 } else { n };
    let excess = n - capped;
    // u16 saturation: a >65k-level side is beyond any real book; cap
    // rather than wrap (counts feed metrics, not indexing).
    Some((capped as u16, if excess > u16::MAX as u32 { u16::MAX } else { excess as u16 }))
}

/// Parse a `book.{instr}.100ms` push **header** (chain fields, event
/// time, capped level counts). Levels are deliberately not lifted
/// (§4.5). Snapshots have no `prev_change_id` on the wire → `-1`.
#[inline]
pub fn parse_book_header(payload: &[u8], sym: SymbolId) -> Option<DeribitBookFrame> {
    let action = if memchr::memmem::find(payload, b"\"type\":\"snapshot\"").is_some() {
        BOOK_ACTION_SNAPSHOT
    } else if memchr::memmem::find(payload, b"\"type\":\"change\"").is_some() {
        BOOK_ACTION_CHANGE
    } else {
        return None;
    };
    let pos = find_field(payload, b"\"change_id\":")?;
    let (change_id, _) = scan_i64(payload, pos)?;
    let prev_change_id = find_field(payload, b"\"prev_change_id\":")
        .and_then(|p| scan_i64(payload, p))
        .map(|(v, _)| v)
        .unwrap_or(-1);
    // A change without prev_change_id is malformed (only snapshots
    // omit it) — surface as unparseable rather than inventing a chain.
    if action == BOOK_ACTION_CHANGE && prev_change_id == -1 {
        return None;
    }
    let (ts_ns, _) = scan_ms_field(payload, b"\"timestamp\":")?;
    let bids_pos = find_field(payload, b"\"bids\":")?;
    let (n_bids, excess_bids) = count_side_levels(payload, bids_pos)?;
    let asks_pos = find_field(payload, b"\"asks\":")?;
    let (n_asks, excess_asks) = count_side_levels(payload, asks_pos)?;
    Some(DeribitBookFrame {
        change_id,
        prev_change_id,
        ts_ns,
        sym,
        n_bids,
        n_asks,
        excess_bids,
        excess_asks,
        action,
        _pad: [0; 27],
    })
}

// ---------------------------------------------------------------
// Symbol table — instrument_name ⇄ SymbolId, fixed capacity
// ---------------------------------------------------------------

/// Why a [`DeribitSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolTableErr {
    /// The addressed block is full ([`DERIBIT_STATIC_MAX`] static rows
    /// / [`DERIBIT_OPT_MAX`] option rows) — boot misconfiguration.
    Full,
    /// Instrument longer than [`DERIBIT_INSTR_MAX`].
    TooLong,
    /// Instrument empty.
    Empty,
    /// Instrument contains `.` — impossible on Deribit and would
    /// corrupt channel-name parsing ([`extract_instrument`]).
    HasDot,
    /// A static `insert` after the first `insert_option` (M2.1): the
    /// table is partitioned `[static | options]`; build order is
    /// static first, options after.
    StaticAfterOptions,
}

/// Fixed-capacity `instrument_name → SymbolId` map, PARTITIONED
/// (M2.1): rows `[0..static_len)` are configured instruments (full
/// channel set), rows `[static_len..len)` are discovered capped-chain
/// options (quote-only). Linear scan (static N ≤ 16; with a full
/// options block N ≤ 80 — measured trivial at Deribit's per-message
/// cadence, noted in docs/hot-path-latency.md at the M2.1 slice).
/// Single-owner: built at boot, read by the ingress thread.
pub struct DeribitSymbolTable {
    rows: [(u8, [u8; DERIBIT_INSTR_MAX], SymbolId); DERIBIT_MAX_SYMBOLS],
    len: usize,
    /// First options row (== `len` while no option was inserted).
    static_len: usize,
}

impl DeribitSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; DERIBIT_INSTR_MAX], 0); DERIBIT_MAX_SYMBOLS],
            len: 0,
            static_len: 0,
        }
    }

    fn validate(instrument: &[u8]) -> Result<(), SymbolTableErr> {
        if instrument.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if instrument.len() > DERIBIT_INSTR_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if memchr::memchr(b'.', instrument).is_some() {
            return Err(SymbolTableErr::HasDot);
        }
        Ok(())
    }

    fn push_row(&mut self, instrument: &[u8], sym: SymbolId) {
        let row = &mut self.rows[self.len];
        row.0 = instrument.len() as u8;
        row.1[..instrument.len()].copy_from_slice(instrument);
        row.2 = sym;
        self.len += 1;
    }

    /// Register a CONFIGURED `instrument → sym` (full channel set).
    /// Boot-time only; must precede every [`Self::insert_option`].
    pub fn insert(&mut self, instrument: &[u8], sym: SymbolId) -> Result<(), SymbolTableErr> {
        Self::validate(instrument)?;
        if self.static_len != self.len {
            return Err(SymbolTableErr::StaticAfterOptions);
        }
        if self.static_len >= DERIBIT_STATIC_MAX {
            return Err(SymbolTableErr::Full);
        }
        self.push_row(instrument, sym);
        self.static_len += 1;
        Ok(())
    }

    /// Register a DISCOVERED capped-chain option `instrument → sym`
    /// (quote-only subscription; M2.1). Boot-time only.
    pub fn insert_option(
        &mut self,
        instrument: &[u8],
        sym: SymbolId,
    ) -> Result<(), SymbolTableErr> {
        Self::validate(instrument)?;
        if self.len - self.static_len >= DERIBIT_OPT_MAX {
            return Err(SymbolTableErr::Full);
        }
        debug_assert!(self.len < DERIBIT_MAX_SYMBOLS, "blocks sum to capacity");
        self.push_row(instrument, sym);
        Ok(())
    }

    /// Number of static (configured, full-channel-set) rows.
    #[inline]
    pub fn static_len(&self) -> usize {
        self.static_len
    }

    /// Number of option (quote-only) rows.
    #[inline]
    pub fn n_options(&self) -> usize {
        self.len - self.static_len
    }

    /// True when row `idx` is an option row (quote-only).
    #[inline]
    pub fn is_option_row(&self, idx: usize) -> bool {
        idx >= self.static_len && idx < self.len
    }

    /// Resolve an instrument to its symbol. Hot path: length gate
    /// first, then bytewise compare.
    #[inline]
    pub fn lookup(&self, instrument: &[u8]) -> Option<SymbolId> {
        let n = instrument.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == instrument {
                return Some(row.2);
            }
            i += 1;
        }
        None
    }

    /// Row accessor for subscribe-batch building: `(instrument, sym)`.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<(&[u8], SymbolId)> {
        if idx >= self.len {
            return None;
        }
        let row = &self.rows[idx];
        Some((&row.1[..row.0 as usize], row.2))
    }

    /// Index of `sym` in insertion order (monitor slot index).
    #[inline]
    pub fn index_of(&self, sym: SymbolId) -> Option<usize> {
        let mut i = 0;
        while i < self.len {
            if self.rows[i].2 == sym {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Number of configured instruments.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the table is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for DeribitSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Integrity monitors (§6.2)
// ---------------------------------------------------------------

/// Sentinel: awaiting the first (snapshot) message / first trade.
const AWAITING: i64 = i64::MIN;

/// Outcome of one [`DeribitBookChain::apply`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainOutcome {
    /// Snapshot accepted — chain (re)rooted at its `change_id`.
    Init,
    /// `prev_change_id` chained to the prior `change_id`.
    Chained,
    /// Chain broken — caller must resubscribe this book channel
    /// (official guidance) and count `gaps_total`. The monitor
    /// re-arms for a fresh snapshot.
    Gap,
}

/// Per-instrument `change_id`/`prev_change_id` chain monitor for
/// `book.{instr}.100ms`. Unlike OKX there is no idle-heartbeat and no
/// maintenance-reset rule: a snapshot roots the chain, every change
/// must link exactly, anything else is a gap.
///
/// Sentinel note: the internal awaiting-snapshot marker is
/// `i64::MIN`; a snapshot whose `change_id` equals it would silently
/// un-root the monitor. Deribit change ids are non-negative on the
/// wire, so the collision is unreachable — recorded here (and
/// normalized away in the `deribit_book` fuzz model) rather than
/// spent a branch on.
#[derive(Copy, Clone, Debug)]
pub struct DeribitBookChain {
    last_change_id: i64,
}

impl DeribitBookChain {
    /// New monitor, awaiting a snapshot.
    pub const fn new() -> Self {
        Self {
            last_change_id: AWAITING,
        }
    }

    /// Re-arm after a resubscribe: the next message must be a
    /// snapshot again.
    #[inline]
    pub fn reset_await_snapshot(&mut self) {
        self.last_change_id = AWAITING;
    }

    /// The chain's current tail (`i64::MIN` = awaiting a snapshot).
    /// Read by the run loop *before* [`Self::apply`] so a Gap verdict
    /// can pair its `gaps_total` increment with a `ChannelId::BookGap`
    /// event carrying expected vs observed `prev_change_id`.
    #[inline]
    pub const fn last_change_id(&self) -> i64 {
        self.last_change_id
    }

    /// Advance the chain with one message's
    /// `(action, prev_change_id, change_id)`.
    #[inline]
    pub fn apply(&mut self, action: u8, prev_change_id: i64, change_id: i64) -> ChainOutcome {
        if action == BOOK_ACTION_SNAPSHOT {
            // A snapshot always (re)roots the chain — the venue sends
            // one after every (re)subscribe.
            self.last_change_id = change_id;
            return ChainOutcome::Init;
        }
        if self.last_change_id == AWAITING {
            // Change before any snapshot: joined mid-stream.
            return ChainOutcome::Gap;
        }
        if prev_change_id == self.last_change_id {
            self.last_change_id = change_id;
            return ChainOutcome::Chained;
        }
        self.reset_await_snapshot();
        ChainOutcome::Gap
    }
}

impl Default for DeribitBookChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of one [`DeribitTradeSeq::apply`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TradeSeqOutcome {
    /// First observation or exact `last + 1` successor.
    Ok,
    /// Jumped forward by more than 1 — trades were missed. Counted;
    /// no resubscribe (Deribit does not replay). Monitor resyncs.
    Gap,
    /// Went backwards or repeated — counted, monitor resyncs.
    Regression,
}

/// Per-instrument strictly-sequential `trade_seq` monitor
/// (`trades.{instr}.100ms`): consecutive trades increment by exactly 1.
#[derive(Copy, Clone, Debug)]
pub struct DeribitTradeSeq {
    last_seq: i64,
}

impl DeribitTradeSeq {
    /// New monitor.
    pub const fn new() -> Self {
        Self { last_seq: AWAITING }
    }

    /// The next `trade_seq` this monitor expects (`last + 1`).
    /// Meaningless before the first observation (sentinel + 1) — the
    /// caller only reads it when [`Self::apply_frame`] returned a
    /// non-`Ok` outcome, which the first observation never does.
    #[inline]
    pub const fn next_expected(&self) -> i64 {
        self.last_seq.wrapping_add(1)
    }

    /// Advance with one row's `trade_seq`.
    #[inline]
    pub fn apply(&mut self, seq: i64) -> TradeSeqOutcome {
        let last = self.last_seq;
        self.last_seq = seq;
        if last == AWAITING || seq == last.wrapping_add(1) {
            return TradeSeqOutcome::Ok;
        }
        if seq > last {
            return TradeSeqOutcome::Gap;
        }
        TradeSeqOutcome::Regression
    }

    /// Advance with one whole `trades` push: `first`/`last` are the
    /// frame's first and last row seqs. Equivalent to row-by-row
    /// [`Self::apply`] over the frame edge (`first` vs the previous
    /// frame's true tail) with the interior rows checked by the
    /// caller's intra-frame walk — the monitor adopts `last` as its
    /// new tail unconditionally.
    ///
    /// This replaced per-row applies capped at 16 rows/frame on
    /// 2026-08-15: burst-coalesced frames (59 rows observed live)
    /// left rows 17+ unchecked, so every oversized frame produced one
    /// phantom Gap on the next frame's first row — the first 6 h
    /// soak's 67 false positives, exactly (41 BTC + 26 ETH oversized
    /// frames re-derived from capture).
    #[inline]
    pub fn apply_frame(&mut self, first: i64, last: i64) -> TradeSeqOutcome {
        let out = self.apply(first);
        self.last_seq = last;
        out
    }
}

impl Default for DeribitTradeSeq {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// JSON-RPC request writers + SubId derivation
// ---------------------------------------------------------------

#[inline]
pub(crate) fn push_bytes(dst: &mut [u8], at: usize, src: &[u8]) -> Option<usize> {
    let end = at.checked_add(src.len())?;
    dst.get_mut(at..end)?.copy_from_slice(src);
    Some(end)
}

/// Render `v` as decimal ASCII into `scratch`, returning the digit
/// slice (right-aligned internally; no allocation).
#[inline]
pub(crate) fn fmt_u64(v: u64, scratch: &mut [u8; 20]) -> &[u8] {
    let mut i = scratch.len();
    let mut v = v;
    loop {
        i -= 1;
        scratch[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &scratch[i..]
}

/// `{"jsonrpc":"2.0","id":<id>,"method":"<method>","params":` — the
/// shared request head. Caller appends the params body and `}`.
#[inline]
fn write_req_head(dst: &mut [u8], id: u64, method: &[u8]) -> Option<usize> {
    let mut n = 0;
    n = push_bytes(dst, n, b"{\"jsonrpc\":\"2.0\",\"id\":")?;
    let mut digits = [0u8; 20];
    n = push_bytes(dst, n, fmt_u64(id, &mut digits))?;
    n = push_bytes(dst, n, b",\"method\":\"")?;
    n = push_bytes(dst, n, method)?;
    n = push_bytes(dst, n, b"\",\"params\":")?;
    Some(n)
}

#[inline]
fn write_channel_name(
    dst: &mut [u8],
    at: usize,
    channel: DeribitChannel,
    instrument: &[u8],
) -> Option<usize> {
    let mut n = at;
    n = push_bytes(dst, n, b"\"")?;
    n = push_bytes(dst, n, channel.wire_prefix())?;
    n = push_bytes(dst, n, instrument)?;
    n = push_bytes(dst, n, channel.wire_suffix())?;
    n = push_bytes(dst, n, b"\"")?;
    Some(n)
}

/// Serialize the single batched `public/subscribe` covering every
/// configured `(channel × instrument)` pair: `quote` + `ticker.100ms`
/// + `trades.100ms` per STATIC instrument, `book.100ms` when
/// `depth_enabled`; OPTION rows (M2.1 capped chain) subscribe `quote`
/// ONLY — an option book is a book, the BBO rides the existing tick
/// lane; the mark/IV `ticker` stream arrives at M2.3. **One call** —
/// subscribe costs 3000 of 30 000 credits (§4.2), so batching is
/// mandatory. Returns the byte length, `None` if `dst` is too small.
#[inline]
pub fn write_subscribe_all(
    dst: &mut [u8],
    id: u64,
    symbols: &DeribitSymbolTable,
    depth_enabled: bool,
) -> Option<usize> {
    let mut n = write_req_head(dst, id, b"public/subscribe")?;
    n = push_bytes(dst, n, b"{\"channels\":[")?;
    let mut first = true;
    let mut i = 0;
    while let Some((instr, _sym)) = symbols.get(i) {
        let channels = [
            DeribitChannel::Quote,
            DeribitChannel::Ticker,
            DeribitChannel::Trades,
            DeribitChannel::Book,
        ];
        let n_ch = if symbols.is_option_row(i) {
            1 // quote only (M2.1)
        } else if depth_enabled {
            4
        } else {
            3
        };
        let mut c = 0;
        while c < n_ch {
            if !first {
                n = push_bytes(dst, n, b",")?;
            }
            first = false;
            n = write_channel_name(dst, n, channels[c], instr)?;
            c += 1;
        }
        i += 1;
    }
    n = push_bytes(dst, n, b"]}}")?;
    Some(n)
}

/// Serialize a one-channel `public/subscribe` or `public/unsubscribe`
/// for `book.{instr}.100ms` — the §6.2 resync action after a chain
/// break (unsubscribe + subscribe ⇒ fresh snapshot).
#[inline]
pub fn write_book_op(
    dst: &mut [u8],
    id: u64,
    method: &[u8],
    instrument: &[u8],
) -> Option<usize> {
    let mut n = write_req_head(dst, id, method)?;
    n = push_bytes(dst, n, b"{\"channels\":[")?;
    n = write_channel_name(dst, n, DeribitChannel::Book, instrument)?;
    n = push_bytes(dst, n, b"]}}")?;
    Some(n)
}

/// Serialize `public/set_heartbeat {"interval": <secs>}` — queued
/// once per connection, immediately after the WS upgrade.
#[inline]
pub fn write_set_heartbeat(dst: &mut [u8], id: u64, interval_secs: u64) -> Option<usize> {
    let mut n = write_req_head(dst, id, b"public/set_heartbeat")?;
    n = push_bytes(dst, n, b"{\"interval\":")?;
    let mut digits = [0u8; 20];
    n = push_bytes(dst, n, fmt_u64(interval_secs, &mut digits))?;
    n = push_bytes(dst, n, b"}}")?;
    Some(n)
}

/// Serialize `public/test {}` — the mandatory `test_request` answer
/// and our proactive idle probe.
#[inline]
pub fn write_test(dst: &mut [u8], id: u64) -> Option<usize> {
    let mut n = write_req_head(dst, id, b"public/test")?;
    n = push_bytes(dst, n, b"{}}")?;
    Some(n)
}

/// FNV-1a 64-bit over the channel tag byte + instrument bytes — a
/// stable [`SubId`] for the `core_net::SubTable`. Never returns
/// [`SubId::NONE`].
#[inline]
pub fn sub_id_of(channel: DeribitChannel, instrument: &[u8]) -> SubId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    h ^= channel as u64;
    h = h.wrapping_mul(FNV_PRIME);
    let mut i = 0;
    while i < instrument.len() {
        h ^= instrument[i] as u64;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    // SubId(0) is reserved by the table.
    SubId(h | 1)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const QUOTE: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
    const TICKER: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"state":"open","settlement_price":3925.85,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"last_price":3906.0,"instrument_name":"BTC-PERPETUAL","index_price":3931.73,"funding_8h":0.00655,"current_funding":0.00042,"best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
    const TRADES: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[{"trade_seq":30289442,"trade_id":"48079269","timestamp":1590484512188,"tick_direction":2,"price":8950.0,"mark_price":8948.9,"instrument_name":"BTC-PERPETUAL","index_price":8955.88,"direction":"sell","amount":10.0}]}}"#;
    const BOOK_SNAP: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1554373962454,"instrument_name":"BTC-PERPETUAL","change_id":297217105,"bids":[["new",5042.34,30.0],["new",5041.94,20.0]],"asks":[["new",5042.64,40.0]],"type":"snapshot"}}}"#;
    const BOOK_CHANGE: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1554373963454,"instrument_name":"BTC-PERPETUAL","change_id":297217107,"prev_change_id":297217105,"bids":[["delete",5041.94,0.0]],"asks":[],"type":"change"}}}"#;
    const TEST_REQ: &[u8] = br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
    const HEARTBEAT: &[u8] = br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"heartbeat"}}"#;
    const RPC_OK: &[u8] = br#"{"jsonrpc":"2.0","id":42,"result":["quote.BTC-PERPETUAL"],"usIn":1,"usOut":2,"usDiff":1,"testnet":false}"#;
    const RPC_ERR: &[u8] = br#"{"jsonrpc":"2.0","id":7,"error":{"code":10028,"message":"too_many_requests"},"testnet":false}"#;

    // ---- classify -------------------------------------------------

    #[test]
    fn classify_recognizes_every_kind() {
        assert_eq!(classify(QUOTE), DeribitMsgKind::Notification(DeribitChannel::Quote));
        assert_eq!(classify(TICKER), DeribitMsgKind::Notification(DeribitChannel::Ticker));
        assert_eq!(classify(TRADES), DeribitMsgKind::Notification(DeribitChannel::Trades));
        assert_eq!(classify(BOOK_SNAP), DeribitMsgKind::Notification(DeribitChannel::Book));
        assert_eq!(classify(TEST_REQ), DeribitMsgKind::TestRequest);
        assert_eq!(classify(HEARTBEAT), DeribitMsgKind::Heartbeat);
        assert_eq!(classify(RPC_OK), DeribitMsgKind::RpcResult(42));
        assert_eq!(classify(RPC_ERR), DeribitMsgKind::RpcError { id: 7, code: 10028 });
        assert_eq!(classify(b"{\"nonsense\":true}"), DeribitMsgKind::Unknown);
    }

    #[test]
    fn classify_unknown_subscription_channel_and_negative_error_code() {
        let odd = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"platform_state","data":{}}}"#;
        assert_eq!(classify(odd), DeribitMsgKind::Unknown);
        let neg = br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"invalid params"}}"#;
        assert_eq!(classify(neg), DeribitMsgKind::RpcError { id: 3, code: -32602 });
    }

    // ---- extract_instrument --------------------------------------

    #[test]
    fn extract_instrument_strips_prefix_and_suffix() {
        assert_eq!(
            extract_instrument(QUOTE, DeribitChannel::Quote),
            Some(&b"BTC-PERPETUAL"[..])
        );
        assert_eq!(
            extract_instrument(TICKER, DeribitChannel::Ticker),
            Some(&b"BTC-PERPETUAL"[..])
        );
        assert_eq!(
            extract_instrument(TRADES, DeribitChannel::Trades),
            Some(&b"BTC-PERPETUAL"[..])
        );
        assert_eq!(
            extract_instrument(BOOK_SNAP, DeribitChannel::Book),
            Some(&b"BTC-PERPETUAL"[..])
        );
    }

    #[test]
    fn extract_instrument_rejects_wrong_prefix_and_absence() {
        assert_eq!(extract_instrument(QUOTE, DeribitChannel::Ticker), None);
        assert_eq!(extract_instrument(b"{}", DeribitChannel::Quote), None);
        // Unterminated channel value.
        assert_eq!(
            extract_instrument(br#"{"channel":"quote.BTC-PERP"#, DeribitChannel::Quote),
            None
        );
    }

    // ---- parse_quote ---------------------------------------------

    #[test]
    fn parse_quote_extracts_both_sides() {
        let f = parse_quote(QUOTE, 7).unwrap();
        assert_eq!(f.sym, 7);
        assert_eq!(f.bid_px_1e6, 3_914_970_000);
        assert_eq!(f.bid_qty_1e6, 40_000_000);
        assert_eq!(f.ask_px_1e6, 3_996_610_000);
        assert_eq!(f.ask_qty_1e6, 50_000_000);
        assert_eq!(f.ts_ns, 1_550_658_624_149 * 1_000_000);
        assert_eq!(f.ts_ms, 1_550_658_624_149);
    }

    #[test]
    fn parse_quote_null_side_and_double_null() {
        let one_sided = br#"{"timestamp":1000,"best_bid_price":null,"best_bid_amount":null,"best_ask_price":1.5,"best_ask_amount":2.0}"#;
        let f = parse_quote(one_sided, 1).unwrap();
        assert_eq!(f.bid_px_1e6, 0);
        assert_eq!(f.bid_qty_1e6, 0);
        assert_eq!(f.ask_px_1e6, 1_500_000);
        let empty = br#"{"timestamp":1000,"best_bid_price":null,"best_bid_amount":null,"best_ask_price":null,"best_ask_amount":null}"#;
        assert!(parse_quote(empty, 1).is_none());
    }

    #[test]
    fn parse_quote_rejects_missing_timestamp() {
        let b = br#"{"best_bid_price":1.0,"best_bid_amount":1.0,"best_ask_price":2.0,"best_ask_amount":1.0}"#;
        assert!(parse_quote(b, 0).is_none());
    }

    // ---- parse_ticker --------------------------------------------

    #[test]
    fn parse_ticker_extracts_all_seven_fields() {
        let f = parse_ticker(TICKER, 9).unwrap();
        assert_eq!(f.sym, 9);
        assert_eq!(f.mark_px_1e6, 3_940_060_000);
        assert_eq!(f.index_px_1e6, 3_931_730_000);
        assert_eq!(f.current_funding_1e9, 420_000);
        assert_eq!(f.open_interest_1e6, 18_918_470_000_000);
        assert_eq!(f.min_px_1e6, 3_943_210_000);
        assert_eq!(f.max_px_1e6, 3_982_840_000);
        assert_eq!(f.ts_ns, 1_550_652_954_406 * 1_000_000);
    }

    #[test]
    fn parse_ticker_negative_funding_and_rejects_garbage() {
        let b = br#"{"timestamp":2000,"mark_price":1.0,"index_price":1.0,"current_funding":-0.000375,"open_interest":5,"min_price":0.9,"max_price":1.1}"#;
        let f = parse_ticker(b, 0).unwrap();
        assert_eq!(f.current_funding_1e9, -375_000);
        assert!(parse_ticker(b"{}", 0).is_none());
    }

    // ---- parse_trade ---------------------------------------------

    #[test]
    fn parse_trade_extracts_fields() {
        // Row slice as the run loop cuts it (from "trade_seq" on).
        let row = br#""trade_seq":30289442,"trade_id":"48079269","timestamp":1590484512188,"tick_direction":2,"price":8950.0,"mark_price":8948.9,"instrument_name":"BTC-PERPETUAL","index_price":8955.88,"direction":"sell","amount":10.0}"#;
        let t = parse_trade(row, 3).unwrap();
        assert_eq!(t.sym, 3);
        assert_eq!(t.trade_seq, 30_289_442);
        assert_eq!(t.trade_id, 48_079_269);
        assert_eq!(t.px_1e6, 8_950_000_000);
        assert_eq!(t.qty_1e6, 10_000_000);
        assert_eq!(t.side, 1);
        assert_eq!(t.ts_ns, 1_590_484_512_188 * 1_000_000);
    }

    #[test]
    fn parse_trade_buy_side_missing_direction_and_nonnumeric_id() {
        let buy = br#""trade_seq":1,"trade_id":"9","timestamp":1000,"price":1.0,"direction":"buy","amount":2.0}"#;
        assert_eq!(parse_trade(buy, 0).unwrap().side, 0);
        let no_dir = br#""trade_seq":1,"trade_id":"9","timestamp":1000,"price":1.0,"amount":2.0}"#;
        assert!(parse_trade(no_dir, 0).is_none());
        let odd_id = br#""trade_seq":2,"trade_id":"ETH-88","timestamp":1000,"price":1.0,"direction":"buy","amount":2.0}"#;
        assert_eq!(parse_trade(odd_id, 0).unwrap().trade_id, 0);
    }

    // ---- parse_book_header ---------------------------------------

    #[test]
    fn parse_book_header_snapshot_and_change() {
        let s = parse_book_header(BOOK_SNAP, 2).unwrap();
        assert_eq!(s.action, BOOK_ACTION_SNAPSHOT);
        assert_eq!(s.change_id, 297_217_105);
        assert_eq!(s.prev_change_id, -1);
        assert_eq!(s.n_bids, 2);
        assert_eq!(s.n_asks, 1);
        assert_eq!(s.excess_bids, 0);
        assert_eq!(s.excess_asks, 0);
        let c = parse_book_header(BOOK_CHANGE, 2).unwrap();
        assert_eq!(c.action, BOOK_ACTION_CHANGE);
        assert_eq!(c.change_id, 297_217_107);
        assert_eq!(c.prev_change_id, 297_217_105);
        assert_eq!(c.n_bids, 1);
        assert_eq!(c.n_asks, 0);
    }

    #[test]
    fn parse_book_header_caps_depth_and_counts_excess() {
        let mut b = Vec::with_capacity(16 * 1024);
        b.extend_from_slice(br#"{"timestamp":1000,"change_id":5,"type":"snapshot","bids":["#);
        let mut i = 0;
        while i < DEPTH_CAP + 10 {
            if i > 0 {
                b.push(b',');
            }
            b.extend_from_slice(br#"["new",1.0,2.0]"#);
            i += 1;
        }
        b.extend_from_slice(br#"],"asks":[]}"#);
        let f = parse_book_header(&b, 0).unwrap();
        assert_eq!(f.n_bids, DEPTH_CAP as u16);
        assert_eq!(f.excess_bids, 10);
        assert_eq!(f.n_asks, 0);
        assert_eq!(f.excess_asks, 0);
    }

    #[test]
    fn parse_book_header_rejects_change_without_prev_and_missing_type() {
        let no_prev = br#"{"timestamp":1000,"change_id":5,"type":"change","bids":[],"asks":[]}"#;
        assert!(parse_book_header(no_prev, 0).is_none());
        let no_type = br#"{"timestamp":1000,"change_id":5,"bids":[],"asks":[]}"#;
        assert!(parse_book_header(no_type, 0).is_none());
        // Truncated side array never panics, just fails.
        let trunc = br#"{"timestamp":1000,"change_id":5,"type":"snapshot","bids":[["new",1.0"#;
        assert!(parse_book_header(trunc, 0).is_none());
    }

    // ---- symbol table --------------------------------------------

    #[test]
    fn symbol_table_roundtrip() {
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", 0x0300_0001).unwrap();
        t.insert(b"ETH-PERPETUAL", 0x0300_0002).unwrap();
        assert_eq!(t.lookup(b"BTC-PERPETUAL"), Some(0x0300_0001));
        assert_eq!(t.lookup(b"ETH-PERPETUAL"), Some(0x0300_0002));
        assert_eq!(t.lookup(b"SOL-PERPETUAL"), None);
        assert_eq!(t.index_of(0x0300_0002), Some(1));
        assert_eq!(t.get(0).unwrap().0, b"BTC-PERPETUAL");
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn symbol_table_rejects_bad_input() {
        let mut t = DeribitSymbolTable::new();
        assert_eq!(t.insert(b"", 1), Err(SymbolTableErr::Empty));
        assert_eq!(
            t.insert(&[b'A'; DERIBIT_INSTR_MAX + 1], 1),
            Err(SymbolTableErr::TooLong)
        );
        assert_eq!(t.insert(b"BTC.WEIRD", 1), Err(SymbolTableErr::HasDot));
        let mut i = 0u32;
        while (i as usize) < DERIBIT_STATIC_MAX {
            t.insert(format!("S{i}").as_bytes(), i).unwrap();
            i += 1;
        }
        assert_eq!(t.insert(b"OVER", 99), Err(SymbolTableErr::Full));
    }

    #[test]
    fn symbol_table_options_partition_law() {
        // Static rows first, options after; both blocks fixed-cap;
        // static-after-options is a build-order violation (M2.1).
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", 1).unwrap();
        t.insert_option(b"BTC-27MAR26-100000-C", 513).unwrap();
        t.insert_option(b"BTC-27MAR26-100000-P", 514).unwrap();
        assert_eq!(t.static_len(), 1);
        assert_eq!(t.n_options(), 2);
        assert_eq!(t.len(), 3);
        assert!(!t.is_option_row(0));
        assert!(t.is_option_row(1) && t.is_option_row(2));
        assert!(!t.is_option_row(3)); // out of range
        assert_eq!(
            t.insert(b"ETH-PERPETUAL", 2),
            Err(SymbolTableErr::StaticAfterOptions)
        );
        // Lookup spans both blocks.
        assert_eq!(t.lookup(b"BTC-27MAR26-100000-P"), Some(514));
        assert_eq!(t.lookup(b"BTC-PERPETUAL"), Some(1));
        // Options block cap.
        let mut i = 2u32;
        while (i as usize) < DERIBIT_OPT_MAX {
            t.insert_option(format!("O{i}").as_bytes(), 512 + i).unwrap();
            i += 1;
        }
        assert_eq!(t.insert_option(b"OVER", 999), Err(SymbolTableErr::Full));
        // Validation applies to option inserts too.
        assert_eq!(t.insert_option(b"", 1), Err(SymbolTableErr::Empty));
        assert_eq!(t.insert_option(b"BTC.X", 1), Err(SymbolTableErr::HasDot));
    }

    #[test]
    fn subscribe_all_options_rows_are_quote_only() {
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", 1).unwrap();
        t.insert_option(b"BTC-27MAR26-100000-C", 513).unwrap();
        let mut buf = [0u8; 4096];
        let n = write_subscribe_all(&mut buf, 5, &t, false).expect("fits");
        let s = core::str::from_utf8(&buf[..n]).unwrap();
        // Static row: full non-depth set.
        assert!(s.contains("\"quote.BTC-PERPETUAL\""));
        assert!(s.contains("\"ticker.BTC-PERPETUAL.100ms\""));
        assert!(s.contains("\"trades.BTC-PERPETUAL.100ms\""));
        // Option row: quote only — no ticker/trades/book.
        assert!(s.contains("\"quote.BTC-27MAR26-100000-C\""));
        assert!(!s.contains("ticker.BTC-27MAR26-100000-C"));
        assert!(!s.contains("trades.BTC-27MAR26-100000-C"));
        assert!(!s.contains("book.BTC-27MAR26-100000-C"));
        // Depth on: static gains book, option still quote-only.
        let n2 = write_subscribe_all(&mut buf, 6, &t, true).expect("fits");
        let s2 = core::str::from_utf8(&buf[..n2]).unwrap();
        assert!(s2.contains("\"book.BTC-PERPETUAL.100ms\""));
        assert!(!s2.contains("book.BTC-27MAR26-100000-C"));
    }

    // ---- book chain ----------------------------------------------

    #[test]
    fn book_chain_init_chain_and_reroot() {
        let mut c = DeribitBookChain::new();
        assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, 100), ChainOutcome::Init);
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 100, 101), ChainOutcome::Chained);
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 101, 105), ChainOutcome::Chained);
        // Unsolicited snapshot re-roots (venue behaviour after any
        // resubscribe) — not a gap.
        assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, 200), ChainOutcome::Init);
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 200, 201), ChainOutcome::Chained);
    }

    #[test]
    fn book_chain_gap_paths_rearm_for_snapshot() {
        // Change before any snapshot: mid-stream join.
        let mut c = DeribitBookChain::new();
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 7, 8), ChainOutcome::Gap);
        assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, 20), ChainOutcome::Init);
        // Broken chain: prev doesn't match last.
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 99, 100), ChainOutcome::Gap);
        // Re-armed: a follow-up change (even self-consistent) is
        // still a gap until a snapshot arrives.
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 100, 101), ChainOutcome::Gap);
        assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, 300), ChainOutcome::Init);
    }

    // ---- trade seq -----------------------------------------------

    #[test]
    fn trade_seq_sequential_rules() {
        let mut m = DeribitTradeSeq::new();
        assert_eq!(m.apply(5), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(6), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(6), TradeSeqOutcome::Regression, "repeat is a regression");
        // Resynced to 6 → 7 chains.
        assert_eq!(m.apply(7), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(10), TradeSeqOutcome::Gap, "jump of +3 missed trades");
        // Resynced to 10 → 11 chains.
        assert_eq!(m.apply(11), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(3), TradeSeqOutcome::Regression);
        assert_eq!(m.apply(4), TradeSeqOutcome::Ok);
    }

    #[test]
    fn trade_seq_apply_frame_checks_edge_and_adopts_tail() {
        let mut m = DeribitTradeSeq::new();
        // First frame: no prior state — Ok regardless of size.
        assert_eq!(m.apply_frame(100, 158), TradeSeqOutcome::Ok, "59-row frame");
        assert_eq!(m.next_expected(), 159);
        // Contiguous next frame chains off the TRUE tail (the
        // pre-2026-08-15 16-row sample would have flagged this).
        assert_eq!(m.apply_frame(159, 159), TradeSeqOutcome::Ok);
        // Single-row frame ≡ apply.
        assert_eq!(m.apply_frame(160, 160), TradeSeqOutcome::Ok);
    }

    #[test]
    fn trade_seq_apply_frame_flags_edge_gap_and_regression() {
        let mut m = DeribitTradeSeq::new();
        assert_eq!(m.apply_frame(10, 12), TradeSeqOutcome::Ok);
        // Edge jump 12 → 14: a real hole.
        assert_eq!(m.next_expected(), 13);
        assert_eq!(m.apply_frame(14, 15), TradeSeqOutcome::Gap);
        // Edge regression 15 → 9; tail adoption still moves to the
        // frame's last row so the following frame is judged off it.
        assert_eq!(m.apply_frame(9, 20), TradeSeqOutcome::Regression);
        assert_eq!(m.apply_frame(21, 21), TradeSeqOutcome::Ok, "tail = 20 adopted");
    }

    #[test]
    fn book_chain_exposes_last_change_id_for_pairing() {
        let mut c = DeribitBookChain::new();
        assert_eq!(c.last_change_id(), i64::MIN, "awaiting-snapshot sentinel");
        assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, 42), ChainOutcome::Init);
        assert_eq!(c.last_change_id(), 42);
        assert_eq!(c.apply(BOOK_ACTION_CHANGE, 42, 43), ChainOutcome::Chained);
        assert_eq!(c.last_change_id(), 43, "reads as the expected prev for the next change");
    }

    // ---- request writers -----------------------------------------

    #[test]
    fn write_subscribe_all_exact_bytes() {
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", 1).unwrap();
        t.insert(b"ETH-PERPETUAL", 2).unwrap();
        let mut dst = [0u8; 1024];
        let n = write_subscribe_all(&mut dst, 5, &t, false).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":5,"method":"public/subscribe","params":{"channels":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","quote.ETH-PERPETUAL","ticker.ETH-PERPETUAL.100ms","trades.ETH-PERPETUAL.100ms"]}}"#
                as &[u8]
        );
        // Depth adds book.100ms per instrument; still one call.
        let n = write_subscribe_all(&mut dst, 6, &t, true).unwrap();
        assert!(memchr::memmem::find(&dst[..n], b"\"book.BTC-PERPETUAL.100ms\"").is_some());
        assert!(memchr::memmem::find(&dst[..n], b"\"book.ETH-PERPETUAL.100ms\"").is_some());
        assert_eq!(
            memchr::memmem::find_iter(&dst[..n], b"public/subscribe").count(),
            1
        );
    }

    #[test]
    fn write_subscribe_all_tiny_dst_fails() {
        let mut t = DeribitSymbolTable::new();
        t.insert(b"BTC-PERPETUAL", 1).unwrap();
        let mut tiny = [0u8; 16];
        assert!(write_subscribe_all(&mut tiny, 1, &t, false).is_none());
    }

    #[test]
    fn write_book_op_heartbeat_and_test_exact_bytes() {
        let mut dst = [0u8; 512];
        let n = write_book_op(&mut dst, 9, b"public/unsubscribe", b"BTC-PERPETUAL").unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":9,"method":"public/unsubscribe","params":{"channels":["book.BTC-PERPETUAL.100ms"]}}"#
                as &[u8]
        );
        let n = write_set_heartbeat(&mut dst, 1, HEARTBEAT_INTERVAL_SECS).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":1,"method":"public/set_heartbeat","params":{"interval":15}}"#
                as &[u8]
        );
        let n = write_test(&mut dst, 12345).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"jsonrpc":"2.0","id":12345,"method":"public/test","params":{}}"#
                as &[u8]
        );
        let mut tiny = [0u8; 8];
        assert!(write_test(&mut tiny, 1).is_none());
    }

    // ---- sub ids --------------------------------------------------

    #[test]
    fn sub_ids_are_nonzero_and_distinct() {
        let a = sub_id_of(DeribitChannel::Quote, b"BTC-PERPETUAL");
        let b = sub_id_of(DeribitChannel::Trades, b"BTC-PERPETUAL");
        let c = sub_id_of(DeribitChannel::Quote, b"ETH-PERPETUAL");
        assert_ne!(a.0, 0);
        assert_ne!(a, b, "channel must differentiate");
        assert_ne!(a, c, "instrument must differentiate");
    }
}

// ---------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn quote_roundtrips(
            bp in 1u32..999_999u32,
            bq in 1u64..9_999_999u64,
            ap in 1u32..999_999u32,
            aq in 1u64..9_999_999u64,
            ts in 1u64..2_000_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"timestamp":{ts},"best_bid_price":0.{bp:06},"best_bid_amount":{bq},"best_ask_price":0.{ap:06},"best_ask_amount":{aq}}}"#,
            ).unwrap();
            let f = parse_quote(buf.as_bytes(), 5).unwrap();
            prop_assert_eq!(f.sym, 5);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, (bq as i64) * 1_000_000);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, (aq as i64) * 1_000_000);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
            prop_assert_eq!(f.ts_ms, ts);
        }

        #[test]
        fn book_chain_never_chains_across_a_break(
            root in 1i64..1_000_000i64,
            steps in proptest::collection::vec(1i64..100i64, 1..40),
            break_at in 0usize..40usize,
        ) {
            let mut c = DeribitBookChain::new();
            prop_assert_eq!(c.apply(BOOK_ACTION_SNAPSHOT, -1, root), ChainOutcome::Init);
            let mut last = root;
            for (i, step) in steps.iter().enumerate() {
                let next = last + step;
                if i == break_at {
                    // Wrong prev: must be Gap, then only a snapshot recovers.
                    prop_assert_eq!(
                        c.apply(BOOK_ACTION_CHANGE, last + 1_000_000_000, next),
                        ChainOutcome::Gap
                    );
                    prop_assert_eq!(
                        c.apply(BOOK_ACTION_CHANGE, next, next + 1),
                        ChainOutcome::Gap
                    );
                    return Ok(());
                }
                prop_assert_eq!(c.apply(BOOK_ACTION_CHANGE, last, next), ChainOutcome::Chained);
                last = next;
            }
        }

        #[test]
        fn no_parser_panics_on_arbitrary_bytes(
            buf in proptest::collection::vec(any::<u8>(), 0..=400)
        ) {
            let _ = classify(&buf);
            let _ = extract_instrument(&buf, DeribitChannel::Quote);
            let _ = extract_instrument(&buf, DeribitChannel::Book);
            let _ = parse_quote(&buf, 0);
            let _ = parse_ticker(&buf, 0);
            let _ = parse_trade(&buf, 0);
            let _ = parse_book_header(&buf, 0);
        }
    }
}
