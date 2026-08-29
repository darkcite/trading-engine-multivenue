// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-okx
//!
//! OKX v5 **public** WebSocket ingress (Phase 8b). Channels per
//! `docs/phase-8-plan.md` §4.1 (venue facts verified 2026-08-14):
//!
//! * `bbo-tbt`   — 1-level book, 10 ms cadence, free tier → [`core_types::Tick`]
//! * `trades`    — per-trade pushes, `seqId`-monotonic
//! * `mark-price` — 200 ms cadence
//! * `funding-rate` — 30–90 s push cadence; interval 1h–8h (read `fundingTime`)
//! * `books`     — 400-level diffs, 100 ms, behind `--okx-depth`;
//!   consumed for **capture + integrity** only (§4.5)
//!
//! ## Integrity (§6.2)
//!
//! The book `checksum` field is **deprecated (always 0) — no CRC32
//! here, deliberately.** Continuity is the `seqId`/`prevSeqId` chain,
//! implemented by [`OkxSeqChain`]: snapshot has `prevSeqId == -1`;
//! each update's `prevSeqId` must equal the prior `seqId`; idle
//! heartbeats (~60 s) repeat `prevSeqId == seqId`; maintenance may
//! legitimately *reset* (`seqId < prevSeqId`, chain intact). Any true
//! break ⇒ resubscribe + `gaps_total`. `trades` carries a
//! per-instrument monotonic `seqId` checked by [`TradeSeqMonitor`].
//!
//! ## Keepalive
//!
//! OKX cuts connections silent for 30 s; the client sends the literal
//! text frame `ping` and the venue answers the literal `pong`
//! ([`PING_PAYLOAD`]/[`PONG_PAYLOAD`]). Scheduling comes from
//! `core_net::Keepalive` (25 s interval) in the run loop.
//!
//! ## Zero-copy note (house doctrine)
//!
//! All parsing is in-place over `&[u8]` in the rx buffer. The one
//! unavoidable copy per event is the 64-byte parsed POD moved into
//! the SPSC ring by `try_push` (ownership transfer) — same as every
//! ingress. Subscribe/ping frames are serialized into the tx buffer
//! through fixed scratch arrays; no heap after construction.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod discovery;
pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, RX_BUF_SIZE,
    TX_BUF_SIZE,
};

use core_net::SubId;
use core_parse::{
    find_field, scan_i64, scan_number_sci_1e9, scan_price_1e6, scan_price_1e9, scan_u64, skip_byte,
};
use core_types::{NsTs, SymbolId};

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Longest OKX `instId` we accept. Live 2026-08-15: pre-market
/// futures like `MOODENG-USD_UM_XPERP-310815` run to 27 bytes (the
/// old cap of 24 rejected the FUTURES discovery page); 32 leaves
/// margin. Table rows are fixed at this width.
pub const OKX_INST_ID_MAX: usize = 32;

/// Maximum CONFIGURED (static) instruments per connection — the
/// pre-M2 law, unchanged: these subscribe the full type-gated channel
/// set. Fixed-cap tables everywhere; boot fails fast beyond this.
pub const OKX_STATIC_MAX: usize = 16;

/// Maximum boot-DISCOVERED capped-chain OPTION instruments per
/// connection (M2.2, docs/m2-progress.md design entry). Options
/// subscribe `bbo-tbt` ONLY (`opt-summary` mark/IV arrives at M2.3).
/// Sized so the default policy (2 underlyings × E2 × K8 × C/P = 64)
/// fits exactly; a larger configured policy fails fast at table build
/// with an actionable message.
pub const OKX_OPT_MAX: usize = 64;

/// Total symbol-table capacity: the static block + the options block.
/// The single sizing constant for per-row state arrays (book chains /
/// trade seqs — options rows never use them, but row-indexed arrays
/// stay uniform).
pub const OKX_MAX_SYMBOLS: usize = OKX_STATIC_MAX + OKX_OPT_MAX;

/// OKX instrument class, as discovered from the REST instruments
/// endpoint at boot (8e). Drives WS channel gating: `mark-price`
/// applies to derivatives (`Swap` | `Futures`); `funding-rate` to
/// `Swap` only. Replaces the retired `-SWAP` instId-suffix hack.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OkxInstType {
    /// `instType == "SPOT"`.
    Spot = 0,
    /// `instType == "SWAP"` — perpetual swaps (funding applies).
    Swap = 1,
    /// `instType == "FUTURES"` — dated futures (mark, no funding).
    Futures = 2,
    /// `instType == "OPTION"` — M2.2 capped-chain options
    /// (`bbo-tbt`-only subscription; `opt-summary` arrives at M2.3).
    Option = 3,
}

impl OkxInstType {
    /// Decode the venue's `instType` string. The legacy pages accept
    /// SPOT/SWAP/FUTURES; OPTION decodes for the M2.2 options pages
    /// (per-page contract enforced by the discovery `RowMode` — an
    /// OPTION row on a legacy page is still a violation); anything
    /// else (MARGIN, EVENTS) is a contract violation at this layer.
    #[inline]
    pub fn from_bytes(s: &[u8]) -> Option<Self> {
        match s {
            b"SPOT" => Some(Self::Spot),
            b"SWAP" => Some(Self::Swap),
            b"FUTURES" => Some(Self::Futures),
            b"OPTION" => Some(Self::Option),
            _ => None,
        }
    }

    /// Log-friendly name.
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "SPOT",
            Self::Swap => "SWAP",
            Self::Futures => "FUTURES",
            Self::Option => "OPTION",
        }
    }
}

/// Client keepalive probe — OKX wants the **literal text frame**
/// `ping`, not a WS protocol ping.
pub const PING_PAYLOAD: &[u8] = b"ping";

/// The venue's answer to [`PING_PAYLOAD`].
pub const PONG_PAYLOAD: &[u8] = b"pong";

// ---------------------------------------------------------------
// Channels + message classification
// ---------------------------------------------------------------

/// Public channels this ingress speaks. `#[repr(u8)]` so the value
/// can ride in PODs and metrics labels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OkxChannel {
    /// `bbo-tbt` — best bid/offer, tick-by-tick (10 ms).
    BboTbt = 0,
    /// `trades`.
    Trades = 1,
    /// `mark-price`.
    MarkPrice = 2,
    /// `funding-rate`.
    FundingRate = 3,
    /// `books` — 400-level diffs (behind `--okx-depth`).
    Books = 4,
    /// `opt-summary` — per-FAMILY options mark-IV/greeks stream
    /// (M2.3; subscribed with `instFamily`, not `instId` — the
    /// subscribe writer keys on the channel).
    OptSummary = 5,
}

impl OkxChannel {
    /// The wire name OKX uses in `arg.channel`.
    #[inline]
    pub const fn wire_name(self) -> &'static [u8] {
        match self {
            OkxChannel::BboTbt => b"bbo-tbt",
            OkxChannel::Trades => b"trades",
            OkxChannel::MarkPrice => b"mark-price",
            OkxChannel::FundingRate => b"funding-rate",
            OkxChannel::Books => b"books",
            OkxChannel::OptSummary => b"opt-summary",
        }
    }
}

/// Coarse classification of one inbound text frame. Cheap byte
/// scans only — full parsing happens per-channel afterwards.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OkxMsgKind {
    /// Literal `pong` answering our keepalive probe.
    Pong,
    /// `{"event":"subscribe","arg":{...}}` — one arg acknowledged.
    SubAck,
    /// `{"event":"unsubscribe",...}` — books-resync unsubscribe ack.
    UnsubAck,
    /// `{"event":"error","code":"NNNNN",...}`. Carries the numeric
    /// code (OKX sends it as a quoted string).
    Error(u32),
    /// `{"arg":{"channel":X,...},"data":[...]}` push.
    Data(OkxChannel),
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// Classify one inbound payload. Zero-alloc; key-matched so field
/// order never matters. Channel names are matched **with** their
/// closing quote so `books` can never alias `books5-l2-tbt`.
#[inline]
pub fn classify(payload: &[u8]) -> OkxMsgKind {
    if payload == PONG_PAYLOAD {
        return OkxMsgKind::Pong;
    }
    if memchr::memmem::find(payload, b"\"event\":\"subscribe\"").is_some() {
        return OkxMsgKind::SubAck;
    }
    if memchr::memmem::find(payload, b"\"event\":\"unsubscribe\"").is_some() {
        return OkxMsgKind::UnsubAck;
    }
    if memchr::memmem::find(payload, b"\"event\":\"error\"").is_some() {
        // "code":"60012" — quoted decimal.
        let code = find_field(payload, b"\"code\":")
            .map(|p| skip_byte(payload, p, b'"'))
            .and_then(|p| scan_u64(payload, p))
            .map(|(v, _)| v as u32)
            .unwrap_or(0);
        return OkxMsgKind::Error(code);
    }
    if memchr::memmem::find(payload, b"\"data\":[").is_some() {
        if memchr::memmem::find(payload, b"\"channel\":\"bbo-tbt\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::BboTbt);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"trades\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::Trades);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"mark-price\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::MarkPrice);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"funding-rate\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::FundingRate);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"books\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::Books);
        }
        if memchr::memmem::find(payload, b"\"channel\":\"opt-summary\"").is_some() {
            return OkxMsgKind::Data(OkxChannel::OptSummary);
        }
    }
    OkxMsgKind::Unknown
}

/// Extract the `instId` value bytes from the **arg object** (the
/// first `instId` in the payload — OKX places `arg` before `data`).
/// Returns a subslice of `payload`; no copy.
#[inline]
pub fn extract_inst_id(payload: &[u8]) -> Option<&[u8]> {
    let start = find_field(payload, b"\"instId\":")?;
    let start = skip_byte(payload, start, b'"');
    let rel_end = memchr::memchr(b'"', payload.get(start..)?)?;
    payload.get(start..start + rel_end)
}

/// Extract the FIRST `"instFamily":"…"` value — the `opt-summary`
/// subscribe-ack arg key (M2.3; family-keyed, unlike every other
/// channel's `instId`).
#[inline]
pub fn extract_inst_family(payload: &[u8]) -> Option<&[u8]> {
    let start = find_field(payload, b"\"instFamily\":")?;
    let start = skip_byte(payload, start, b'"');
    let rel_end = memchr::memchr(b'"', payload.get(start..)?)?;
    payload.get(start..start + rel_end)
}

/// WS2 (outage 2026-08-27 §5.2): best-effort extraction of the
/// failing instrument named INSIDE a venue error event's `msg` TEXT —
/// e.g. `"msg":"Wrong URL or channel:bbo-tbt,instId:BTC-USD-260828-
/// 45000-C doesn't exist. …"` (the 60018 post-settlement class). The
/// key appears UNQUOTED inside the message string, so this scans for
/// the literal `instId:` and takes the maximal `[A-Za-z0-9-]` run
/// after it (OKX instIds are uppercase alphanumerics + `-`).
/// `None` when the error names no instrument (60012-class) — the
/// caller then attributes the drop to `SYMBOL_ID_NONE`.
#[inline]
pub fn extract_error_inst_id(payload: &[u8]) -> Option<&[u8]> {
    let at = memchr::memmem::find(payload, b"instId:")?;
    let start = at + b"instId:".len();
    let rest = payload.get(start..)?;
    let mut end = 0usize;
    while end < rest.len() {
        let b = rest[end];
        let word = b.is_ascii_alphanumeric() || b == b'-';
        if !word {
            break;
        }
        end += 1;
    }
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// One parsed `opt-summary` DATA ROW (M2.3) — the OKX side of the
/// `OptSummary` capture record. Scaling matches the record: IV
/// fraction ×1e9 (`markVol` is already a fraction on this venue),
/// forward px ×1e9 (`fwdPx` → the record's underlying slot), BS
/// greeks (`*BS` fields) delta/gamma ×1e9, vega/theta ×1e6. OKX
/// `opt-summary` carries NO mark price and NO open interest — the
/// record's flags stay 0 for both (docs/wire-format.md).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct OkxOptSummaryFrame {
    /// `markVol` fraction ×1e9.
    pub mark_iv_1e9: i64,
    /// `fwdPx` ×1e9.
    pub fwd_px_1e9: i64,
    /// `deltaBS` ×1e9.
    pub delta_1e9: i64,
    /// `gammaBS` ×1e9.
    pub gamma_1e9: i64,
    /// `vegaBS` ×1e6.
    pub vega_1e6: i64,
    /// `thetaBS` ×1e6.
    pub theta_1e6: i64,
}

/// Parse ONE `opt-summary` row slice (M2.3). All captured values are
/// quoted decimal strings on this venue; sign + scientific notation
/// accepted; an empty or malformed required field ⇒ `None`. Row
/// slicing (one object per `"instId":"` marker) is the run-loop
/// scanner's job — this parses within one row.
#[inline]
pub fn parse_opt_summary_row(row: &[u8]) -> Option<OkxOptSummaryFrame> {
    #[inline]
    fn quoted_1e9(row: &[u8], key: &[u8]) -> Option<i64> {
        let start = find_field(row, key)?;
        let start = skip_byte(row, start, b'"');
        let rel_end = memchr::memchr(b'"', row.get(start..)?)?;
        let span = row.get(start..start + rel_end)?;
        if span.is_empty() {
            return None;
        }
        let (v, used) = scan_number_sci_1e9(span, 0)?;
        if used != span.len() {
            return None;
        }
        Some(v)
    }
    Some(OkxOptSummaryFrame {
        mark_iv_1e9: quoted_1e9(row, b"\"markVol\":")?,
        fwd_px_1e9: quoted_1e9(row, b"\"fwdPx\":")?,
        delta_1e9: quoted_1e9(row, b"\"deltaBS\":")?,
        gamma_1e9: quoted_1e9(row, b"\"gammaBS\":")?,
        vega_1e6: quoted_1e9(row, b"\"vegaBS\":")? / 1000,
        theta_1e6: quoted_1e9(row, b"\"thetaBS\":")? / 1000,
    })
}

// ---------------------------------------------------------------
// Frame PODs — one cache line each, explicit padding
// ---------------------------------------------------------------

/// Parsed `bbo-tbt` push (one level per side; a missing side is
/// px = 0, qty = 0 — legitimate for one-sided books).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct OkxBboFrame {
    /// `seqId` from the data row (0 when the venue omits it).
    pub seq_id: i64,
    /// Venue event time (`ts`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Best bid price ×1e6 (0 = side empty).
    pub bid_px_1e6: i64,
    /// Best bid size ×1e6.
    pub bid_qty_1e6: i64,
    /// Best ask price ×1e6 (0 = side empty).
    pub ask_px_1e6: i64,
    /// Best ask size ×1e6.
    pub ask_qty_1e6: i64,
    /// Resolved symbol (venue-namespaced, bits 31..24 = Okx).
    pub sym: SymbolId,
    // Explicit tail padding — keeps the slot exactly 64 B.
    _pad: [u8; 12],
}

/// Parsed `trades` row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct OkxTradeFrame {
    /// Venue `tradeId` (decimal string on the wire).
    pub trade_id: u64,
    /// Venue event time (`ts`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Trade price ×1e6.
    pub px_1e6: i64,
    /// Trade size ×1e6.
    pub qty_1e6: i64,
    /// `seqId` from the row (0 when omitted).
    pub seq_id: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// Taker direction: 0 = buy, 1 = sell (wire `side`).
    pub side: u8,
    // Explicit tail padding.
    _pad: [u8; 15],
}

/// Parsed `mark-price` push.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct OkxMarkPriceFrame {
    /// Venue event time (`ts`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Mark price ×1e6.
    pub mark_px_1e6: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    // Explicit tail padding.
    _pad: [u8; 44],
}

/// Parsed `funding-rate` push. Rates are stored ×1e9 — funding
/// resolution (e.g. `0.0000593`) exceeds the 1e6 price scale.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct OkxFundingFrame {
    /// Venue event time (`ts`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Funding rate ×1e9 (signed).
    pub funding_rate_1e9: i64,
    /// Next settlement time (`fundingTime`, ms) converted to ns.
    pub funding_time_ns: u64,
    /// Resolved symbol.
    pub sym: SymbolId,
    // Explicit tail padding.
    _pad: [u8; 36],
}

/// Parsed `books` push **header** — §4.5: depth is consumed for
/// capture + integrity, so only the chain fields and event time are
/// lifted; levels stay in the rx buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct OkxBookFrame {
    /// `seqId` of this message.
    pub seq_id: i64,
    /// `prevSeqId` of this message (`-1` on snapshots).
    pub prev_seq_id: i64,
    /// Venue event time (`ts`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// 0 = snapshot, 1 = update (wire `action`).
    pub action: u8,
    // Explicit tail padding.
    _pad: [u8; 35],
}

/// `action` value for a books snapshot.
pub const BOOK_ACTION_SNAPSHOT: u8 = 0;
/// `action` value for a books incremental update.
pub const BOOK_ACTION_UPDATE: u8 = 1;

const _SIZE_CHECKS: () = {
    assert!(::core::mem::size_of::<OkxBboFrame>() == 64);
    assert!(::core::mem::size_of::<OkxTradeFrame>() == 64);
    assert!(::core::mem::size_of::<OkxMarkPriceFrame>() == 64);
    assert!(::core::mem::size_of::<OkxFundingFrame>() == 64);
    assert!(::core::mem::size_of::<OkxBookFrame>() == 64);
};

// ---------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------

/// Parse an OKX quoted millisecond timestamp field (e.g.
/// `"ts":"1670324386802"`) into nanoseconds.
#[inline]
fn scan_quoted_ms_to_ns(buf: &[u8], key: &[u8]) -> Option<u64> {
    let pos = find_field(buf, key)?;
    let pos = skip_byte(buf, pos, b'"');
    let (ms, _) = scan_u64(buf, pos)?;
    Some(ms.saturating_mul(1_000_000))
}

/// Parse the bare-number `seqId`-family field (`"seqId":363996337`,
/// `"prevSeqId":-1`). Returns `None` when the key is absent.
#[inline]
fn scan_seq_field(buf: &[u8], key: &[u8]) -> Option<i64> {
    let pos = find_field(buf, key)?;
    let (v, _) = scan_i64(buf, pos)?;
    Some(v)
}

/// Parse the first level of a `bbo-tbt` side array: `[["px","sz",..`.
/// An empty side (`[]`) yields `(0, 0)`. `pos` must point at the
/// side's outer `[` (i.e. the byte returned by `find_field`).
#[inline]
fn scan_bbo_side(buf: &[u8], pos: usize) -> Option<(i64, i64)> {
    // Outer '['.
    if *buf.get(pos)? != b'[' {
        return None;
    }
    match *buf.get(pos + 1)? {
        b']' => Some((0, 0)),
        b'[' => {
            // Inner ["px","sz",...]: opening quote then digits.
            if *buf.get(pos + 2)? != b'"' {
                return None;
            }
            let (px, px_end) = scan_price_1e6(buf, pos + 3)?;
            // `","` between px and sz.
            if buf.get(px_end..px_end + 3)? != b"\",\"" {
                return None;
            }
            let (qty, _) = scan_price_1e6(buf, px_end + 3)?;
            Some((px, qty))
        }
        _ => None,
    }
}

/// WS10-B: apply every level of one `books` side array onto a ladder
/// side. `pos` points at the side's outer `[` (the byte returned by
/// `find_field`). Rows are `["px","sz","liqOrders","numOrders"]`;
/// `sz == 0` deletes the level (venue delta semantics). Returns the
/// applied level count; `None` on malformed input — the caller
/// counts a parse error (the seq-chain monitor stays the resync
/// law).
fn walk_book_side(
    buf: &[u8],
    pos: usize,
    side: &mut book_builder::ladder::LadderSide,
) -> Option<u32> {
    if *buf.get(pos)? != b'[' {
        return None;
    }
    let mut at = pos + 1;
    let mut applied = 0u32;
    loop {
        match *buf.get(at)? {
            b']' => return Some(applied),
            b',' => {
                at += 1;
            }
            b'[' => {
                if *buf.get(at + 1)? != b'"' {
                    return None;
                }
                let (px, px_end) = scan_price_1e6(buf, at + 2)?;
                if buf.get(px_end..px_end + 3)? != b"\",\"" {
                    return None;
                }
                let (qty, qty_end) = scan_price_1e6(buf, px_end + 3)?;
                side.set(px, qty);
                applied += 1;
                // Skip the row's remaining fields to its closing `]`.
                let close = memchr::memchr(b']', buf.get(qty_end..)?)? + qty_end;
                at = close + 1;
            }
            _ => return None,
        }
    }
}

/// WS10-B: apply one `books` push's level arrays onto an instrument
/// ladder (both sides). The caller has already chain-verified the
/// frame ([`parse_book_header`]) and cleared the ladder on
/// `action:"snapshot"`. Returns total applied levels; `None` on
/// malformed input.
pub fn walk_book_levels(
    payload: &[u8],
    ladder: &mut book_builder::ladder::DepthLadder,
) -> Option<u32> {
    let asks_pos = find_field(payload, b"\"asks\":")?;
    let a = walk_book_side(payload, asks_pos, &mut ladder.asks)?;
    let bids_pos = find_field(payload, b"\"bids\":")?;
    let b = walk_book_side(payload, bids_pos, &mut ladder.bids)?;
    Some(a + b)
}

#[cfg(test)]
mod book_walk_tests {
    use super::*;
    use book_builder::ladder::DepthLadder;
    use core_types::VenueId;

    #[test]
    fn walk_applies_snapshot_and_delta() {
        let mut l = DepthLadder::new();
        // Snapshot: 2 asks + 2 bids (real books row shape).
        let snap = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["8476.98","415","0","13"],["8477","7","0","2"]],"bids":[["8476.97","256","0","12"],["8475.55","101","0","1"]],"ts":"1597026383085","seqId":123456}]}"#;
        assert_eq!(walk_book_levels(snap, &mut l), Some(4));
        assert_eq!(l.asks.len(), 2);
        assert_eq!(l.bids.len(), 2);
        let s = l.snapshot(1, VenueId::Okx, 7, 0);
        assert_eq!(s.asks[0].px_1e6, 8_476_980_000);
        assert_eq!(s.bids[0].px_1e6, 8_476_970_000);
        // Delta: delete the best ask (sz 0), add a better bid.
        let delta = br#"{"action":"update","data":[{"asks":[["8476.98","0","0","0"]],"bids":[["8476.99","5","0","1"]],"ts":"1597026383086","seqId":123457,"prevSeqId":123456}]}"#;
        assert_eq!(walk_book_levels(delta, &mut l), Some(2));
        assert_eq!(l.asks.len(), 1);
        let s = l.snapshot(2, VenueId::Okx, 7, 0);
        assert_eq!(s.asks[0].px_1e6, 8_477_000_000);
        assert_eq!(s.bids[0].px_1e6, 8_476_990_000, "new best bid");
    }

    #[test]
    fn walk_rejects_malformed_rows() {
        let mut l = DepthLadder::new();
        // Unquoted price inside a row.
        assert_eq!(
            walk_book_levels(br#"{"asks":[[8476.98,"415"]],"bids":[]}"#, &mut l),
            None
        );
        // Missing bids field entirely.
        assert_eq!(walk_book_levels(br#"{"asks":[]}"#, &mut l), None);
        // Truncated mid-row.
        assert_eq!(walk_book_levels(br#"{"asks":[["8476.98","#, &mut l), None);
        // Empty sides parse as zero levels.
        let mut l2 = DepthLadder::new();
        assert_eq!(
            walk_book_levels(br#"{"asks":[],"bids":[]}"#, &mut l2),
            Some(0)
        );
    }
}

// ---------------------------------------------------------------
// Channel parsers
// ---------------------------------------------------------------

/// Parse a `bbo-tbt` push into an [`OkxBboFrame`]. `sym` is the
/// caller-resolved symbol (from [`extract_inst_id`] + the symbol
/// table). Returns `None` on malformed input — caller counts it.
#[inline]
pub fn parse_bbo(payload: &[u8], sym: SymbolId) -> Option<OkxBboFrame> {
    let asks_pos = find_field(payload, b"\"asks\":")?;
    let (ask_px_1e6, ask_qty_1e6) = scan_bbo_side(payload, asks_pos)?;
    let bids_pos = find_field(payload, b"\"bids\":")?;
    let (bid_px_1e6, bid_qty_1e6) = scan_bbo_side(payload, bids_pos)?;
    let ts_ns = scan_quoted_ms_to_ns(payload, b"\"ts\":")?;
    let seq_id = scan_seq_field(payload, b"\"seqId\":").unwrap_or(0);
    // A frame with both sides empty carries no information.
    if ask_px_1e6 == 0 && bid_px_1e6 == 0 {
        return None;
    }
    Some(OkxBboFrame {
        seq_id,
        ts_ns,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        sym,
        _pad: [0; 12],
    })
}

/// Parse the **first** `trades` row of a push into an
/// [`OkxTradeFrame`]. OKX batches rows per push; the run loop walks
/// subsequent rows by re-slicing the payload (see `run_loop`).
#[inline]
pub fn parse_trade(payload: &[u8], sym: SymbolId) -> Option<OkxTradeFrame> {
    // px: "px":"42219.9"
    let pos = find_field(payload, b"\"px\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (px_1e6, _) = scan_price_1e6(payload, pos)?;
    // sz: "sz":"0.12060306"
    let pos = find_field(payload, b"\"sz\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (qty_1e6, _) = scan_price_1e6(payload, pos)?;
    // side: "side":"buy" | "sell"
    let side = if memchr::memmem::find(payload, b"\"side\":\"buy\"").is_some() {
        0u8
    } else if memchr::memmem::find(payload, b"\"side\":\"sell\"").is_some() {
        1u8
    } else {
        return None;
    };
    // tradeId: "tradeId":"130639474" — quoted decimal.
    let pos = find_field(payload, b"\"tradeId\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (trade_id, _) = scan_u64(payload, pos)?;
    let ts_ns = scan_quoted_ms_to_ns(payload, b"\"ts\":")?;
    let seq_id = scan_seq_field(payload, b"\"seqId\":").unwrap_or(0);
    Some(OkxTradeFrame {
        trade_id,
        ts_ns,
        px_1e6,
        qty_1e6,
        seq_id,
        sym,
        side,
        _pad: [0; 15],
    })
}

/// Parse a `mark-price` push into an [`OkxMarkPriceFrame`].
#[inline]
pub fn parse_mark_price(payload: &[u8], sym: SymbolId) -> Option<OkxMarkPriceFrame> {
    let pos = find_field(payload, b"\"markPx\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (mark_px_1e6, _) = scan_price_1e6(payload, pos)?;
    let ts_ns = scan_quoted_ms_to_ns(payload, b"\"ts\":")?;
    Some(OkxMarkPriceFrame {
        ts_ns,
        mark_px_1e6,
        sym,
        _pad: [0; 44],
    })
}

/// Parse a `funding-rate` push into an [`OkxFundingFrame`]. The rate
/// is scaled ×1e9 (see the struct doc).
#[inline]
pub fn parse_funding_rate(payload: &[u8], sym: SymbolId) -> Option<OkxFundingFrame> {
    let pos = find_field(payload, b"\"fundingRate\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (funding_rate_1e9, _) = scan_price_1e9(payload, pos)?;
    let funding_time_ns = scan_quoted_ms_to_ns(payload, b"\"fundingTime\":")?;
    // `ts` is optional on some funding pushes — fall back to
    // fundingTime so the frame always carries an event time.
    let ts_ns = scan_quoted_ms_to_ns(payload, b"\"ts\":").unwrap_or(funding_time_ns);
    Some(OkxFundingFrame {
        ts_ns,
        funding_rate_1e9,
        funding_time_ns,
        sym,
        _pad: [0; 36],
    })
}

/// Parse a `books` push **header** (chain fields + action). Levels
/// are deliberately not lifted (§4.5).
#[inline]
pub fn parse_book_header(payload: &[u8], sym: SymbolId) -> Option<OkxBookFrame> {
    let action = if memchr::memmem::find(payload, b"\"action\":\"snapshot\"").is_some() {
        BOOK_ACTION_SNAPSHOT
    } else if memchr::memmem::find(payload, b"\"action\":\"update\"").is_some() {
        BOOK_ACTION_UPDATE
    } else {
        return None;
    };
    let seq_id = scan_seq_field(payload, b"\"seqId\":")?;
    let prev_seq_id = scan_seq_field(payload, b"\"prevSeqId\":")?;
    let ts_ns = scan_quoted_ms_to_ns(payload, b"\"ts\":")?;
    Some(OkxBookFrame {
        seq_id,
        prev_seq_id,
        ts_ns,
        sym,
        action,
        _pad: [0; 35],
    })
}

// ---------------------------------------------------------------
// Symbol table — instId ⇄ SymbolId, fixed capacity, boot-built
// ---------------------------------------------------------------

/// Why an [`OkxSymbolTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbolTableErr {
    /// All [`OKX_MAX_SYMBOLS`] rows in use (boot misconfiguration).
    Full,
    /// `instId` longer than [`OKX_INST_ID_MAX`].
    TooLong,
    /// `instId` empty.
    Empty,
}

/// Fixed-capacity `instId → SymbolId` map. Linear scan (N ≤ 16 —
/// two cache lines of ids). Single-owner: built at boot, read by the
/// ingress thread.
pub struct OkxSymbolTable {
    rows: [(u8, [u8; OKX_INST_ID_MAX], SymbolId, OkxInstType); OKX_MAX_SYMBOLS],
    len: usize,
}

impl OkxSymbolTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; OKX_INST_ID_MAX], 0, OkxInstType::Spot); OKX_MAX_SYMBOLS],
            len: 0,
        }
    }

    /// Register `inst_id → sym` with its discovered instrument class.
    /// Boot-time only; `inst_type` comes from REST discovery (8e) and
    /// drives per-instrument channel gating.
    pub fn insert(
        &mut self,
        inst_id: &[u8],
        sym: SymbolId,
        inst_type: OkxInstType,
    ) -> Result<(), SymbolTableErr> {
        if inst_id.is_empty() {
            return Err(SymbolTableErr::Empty);
        }
        if inst_id.len() > OKX_INST_ID_MAX {
            return Err(SymbolTableErr::TooLong);
        }
        if self.len >= OKX_MAX_SYMBOLS {
            return Err(SymbolTableErr::Full);
        }
        let row = &mut self.rows[self.len];
        row.0 = inst_id.len() as u8;
        row.1[..inst_id.len()].copy_from_slice(inst_id);
        row.2 = sym;
        row.3 = inst_type;
        self.len += 1;
        Ok(())
    }

    /// Resolve an `instId` to its symbol. Hot path: length gate first,
    /// then bytewise compare.
    #[inline]
    pub fn lookup(&self, inst_id: &[u8]) -> Option<SymbolId> {
        let n = inst_id.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == inst_id {
                return Some(row.2);
            }
            i += 1;
        }
        None
    }

    /// Row accessor for subscribe-batch building:
    /// `(inst_id, sym, inst_type)`.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<(&[u8], SymbolId, OkxInstType)> {
        if idx >= self.len {
            return None;
        }
        let row = &self.rows[idx];
        Some((&row.1[..row.0 as usize], row.2, row.3))
    }

    /// Index of `sym` in insertion order (chain-monitor slot index).
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

impl Default for OkxSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Integrity monitors (§6.2)
// ---------------------------------------------------------------

/// Outcome of one [`OkxSeqChain::apply`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainOutcome {
    /// First message of a session/resync — snapshot accepted.
    Init,
    /// `prevSeqId` chained to the prior `seqId`.
    Chained,
    /// Idle heartbeat (`prevSeqId == seqId == last`) — chain intact.
    IdleHeartbeat,
    /// Venue maintenance reset (`seqId < prevSeqId`, chain intact) —
    /// accepted, **not** a gap.
    Reset,
    /// Chain broken — caller must resubscribe this channel and count
    /// `gaps_total`. The monitor re-arms for a fresh snapshot.
    Gap,
}

/// Sentinel: awaiting the first (snapshot) message.
const CHAIN_AWAITING: i64 = i64::MIN;

/// Per-instrument `seqId`/`prevSeqId` chain monitor for the `books`
/// channel, implementing the §4.1 rules (snapshot `prevSeqId == -1`,
/// idle heartbeats, maintenance resets).
#[derive(Copy, Clone, Debug)]
pub struct OkxSeqChain {
    last_seq_id: i64,
}

impl OkxSeqChain {
    /// New monitor, awaiting a snapshot.
    pub const fn new() -> Self {
        Self {
            last_seq_id: CHAIN_AWAITING,
        }
    }

    /// Re-arm after a resubscribe: the next message must be a
    /// snapshot again.
    #[inline]
    pub fn reset_await_snapshot(&mut self) {
        self.last_seq_id = CHAIN_AWAITING;
    }

    /// Advance the chain with one message's `(prevSeqId, seqId)`.
    #[inline]
    pub fn apply(&mut self, prev_seq_id: i64, seq_id: i64) -> ChainOutcome {
        if self.last_seq_id == CHAIN_AWAITING {
            return if prev_seq_id == -1 {
                self.last_seq_id = seq_id;
                ChainOutcome::Init
            } else {
                // We joined mid-stream (missed the snapshot).
                ChainOutcome::Gap
            };
        }
        if prev_seq_id == seq_id {
            // Idle heartbeat repeats the last chain point.
            return if seq_id == self.last_seq_id {
                ChainOutcome::IdleHeartbeat
            } else {
                self.reset_await_snapshot();
                ChainOutcome::Gap
            };
        }
        if prev_seq_id == self.last_seq_id {
            self.last_seq_id = seq_id;
            return if seq_id < prev_seq_id {
                // Maintenance may legitimately move seqId backwards
                // while the chain stays intact.
                ChainOutcome::Reset
            } else {
                ChainOutcome::Chained
            };
        }
        self.reset_await_snapshot();
        ChainOutcome::Gap
    }
}

impl Default for OkxSeqChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of one [`TradeSeqMonitor::apply`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TradeSeqOutcome {
    /// Sequence advanced (or first observation).
    Ok,
    /// Sequence went backwards — counted, monitor resyncs to the
    /// observed value.
    Regression,
}

/// Per-instrument monotonic `seqId` monitor for `trades` (equal ids
/// are legal — one push may batch several rows under one seq).
#[derive(Copy, Clone, Debug)]
pub struct TradeSeqMonitor {
    last_seq_id: i64,
}

impl TradeSeqMonitor {
    /// New monitor.
    pub const fn new() -> Self {
        Self {
            last_seq_id: CHAIN_AWAITING,
        }
    }

    /// Advance with one row's `seqId`.
    #[inline]
    pub fn apply(&mut self, seq_id: i64) -> TradeSeqOutcome {
        if self.last_seq_id != CHAIN_AWAITING && seq_id < self.last_seq_id {
            self.last_seq_id = seq_id;
            return TradeSeqOutcome::Regression;
        }
        self.last_seq_id = seq_id;
        TradeSeqOutcome::Ok
    }
}

impl Default for TradeSeqMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Subscribe / unsubscribe writers + SubId derivation
// ---------------------------------------------------------------

/// One `(channel, instId)` pair in a subscribe batch.
#[derive(Copy, Clone, Debug)]
pub struct SubArg<'a> {
    /// Channel to (un)subscribe.
    pub channel: OkxChannel,
    /// Instrument id bytes.
    pub inst_id: &'a [u8],
}

#[inline]
fn push_bytes(dst: &mut [u8], at: usize, src: &[u8]) -> Option<usize> {
    let end = at.checked_add(src.len())?;
    dst.get_mut(at..end)?.copy_from_slice(src);
    Some(end)
}

#[inline]
fn write_op(dst: &mut [u8], op: &[u8], args: &[SubArg<'_>]) -> Option<usize> {
    let mut n = 0;
    n = push_bytes(dst, n, b"{\"op\":\"")?;
    n = push_bytes(dst, n, op)?;
    n = push_bytes(dst, n, b"\",\"args\":[")?;
    let mut i = 0;
    while i < args.len() {
        if i > 0 {
            n = push_bytes(dst, n, b",")?;
        }
        n = push_bytes(dst, n, b"{\"channel\":\"")?;
        n = push_bytes(dst, n, args[i].channel.wire_name())?;
        // M2.3: `opt-summary` is the one FAMILY-keyed channel — its
        // arg key is `instFamily` (the SubArg's inst_id bytes carry
        // the family string for it).
        if args[i].channel == OkxChannel::OptSummary {
            n = push_bytes(dst, n, b"\",\"instFamily\":\"")?;
        } else {
            n = push_bytes(dst, n, b"\",\"instId\":\"")?;
        }
        n = push_bytes(dst, n, args[i].inst_id)?;
        n = push_bytes(dst, n, b"\"}")?;
        i += 1;
    }
    n = push_bytes(dst, n, b"]}")?;
    Some(n)
}

/// Serialize one batched `{"op":"subscribe","args":[...]}` request
/// into `dst`. Returns the byte length, `None` if `dst` is too small.
/// OKX budgets 480 sub/unsub **operations per hour** — batch all args
/// into one op (§4.1).
#[inline]
pub fn write_subscribe_batch(dst: &mut [u8], args: &[SubArg<'_>]) -> Option<usize> {
    write_op(dst, b"subscribe", args)
}

/// Serialize one `{"op":"unsubscribe","args":[...]}` request —
/// used by the books resync (unsubscribe + subscribe ⇒ fresh
/// snapshot).
#[inline]
pub fn write_unsubscribe_batch(dst: &mut [u8], args: &[SubArg<'_>]) -> Option<usize> {
    write_op(dst, b"unsubscribe", args)
}

/// FNV-1a 64-bit over the channel tag byte + `instId` bytes — a
/// stable [`SubId`] for the `core_net::SubTable`. Never returns
/// [`SubId::NONE`].
#[inline]
pub fn sub_id_of(channel: OkxChannel, inst_id: &[u8]) -> SubId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    h ^= channel as u64;
    h = h.wrapping_mul(FNV_PRIME);
    let mut i = 0;
    while i < inst_id.len() {
        h ^= inst_id[i] as u64;
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

    const BBO: &[u8] = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","55154","0","2"]],"bids":[["111.05","57745","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
    const TRADE_BUY: &[u8] = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"130639474","px":"42219.9","sz":"0.12060306","side":"buy","ts":"1630048897897","count":"3","seqId":123456}]}"#;
    const MARK: &[u8] = br#"{"arg":{"channel":"mark-price","instId":"BTC-USD-SWAP"},"data":[{"instType":"SWAP","instId":"BTC-USD-SWAP","markPx":"42310.6","ts":"1630049455539"}]}"#;
    const FUNDING: &[u8] = br#"{"arg":{"channel":"funding-rate","instId":"BTC-USD-SWAP"},"data":[{"fundingRate":"0.0000593","fundingTime":"1630051200000","instId":"BTC-USD-SWAP","instType":"SWAP","ts":"1630048897897"}]}"#;
    const BOOK_SNAP: &[u8] = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["8476.98","415","0","13"]],"bids":[["8476.97","256","0","12"]],"ts":"1597026383085","checksum":0,"prevSeqId":-1,"seqId":123456}]}"#;
    const BOOK_UPD: &[u8] = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[["8476.98","415","0","13"]],"bids":[],"ts":"1597026383217","checksum":0,"prevSeqId":123456,"seqId":123457}]}"#;
    const SUB_ACK: &[u8] = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"connId":"a4d3ae55"}"#;
    const SUB_ERR: &[u8] =
        br#"{"event":"error","code":"60012","msg":"Invalid request","connId":"a4d3ae55"}"#;

    // ---- classify -------------------------------------------------

    #[test]
    fn classify_recognizes_every_kind() {
        assert_eq!(classify(b"pong"), OkxMsgKind::Pong);
        assert_eq!(classify(SUB_ACK), OkxMsgKind::SubAck);
        assert_eq!(classify(SUB_ERR), OkxMsgKind::Error(60012));
        assert_eq!(classify(BBO), OkxMsgKind::Data(OkxChannel::BboTbt));
        assert_eq!(classify(TRADE_BUY), OkxMsgKind::Data(OkxChannel::Trades));
        assert_eq!(classify(MARK), OkxMsgKind::Data(OkxChannel::MarkPrice));
        assert_eq!(classify(FUNDING), OkxMsgKind::Data(OkxChannel::FundingRate));
        assert_eq!(classify(BOOK_SNAP), OkxMsgKind::Data(OkxChannel::Books));
        assert_eq!(classify(b"{\"nonsense\":true}"), OkxMsgKind::Unknown);
    }

    #[test]
    fn classify_books_does_not_alias_l2_tbt_channels() {
        let b = br#"{"arg":{"channel":"books5-l2-tbt","instId":"X"},"data":[{}]}"#;
        assert_eq!(classify(b), OkxMsgKind::Unknown);
    }

    #[test]
    fn classify_unsubscribe_ack() {
        let b = br#"{"event":"unsubscribe","arg":{"channel":"books","instId":"BTC-USDT"}}"#;
        assert_eq!(classify(b), OkxMsgKind::UnsubAck);
    }

    // ---- extract_inst_id -----------------------------------------

    #[test]
    fn extract_inst_id_takes_arg_occurrence() {
        assert_eq!(extract_inst_id(BBO), Some(&b"BTC-USDT"[..]));
        assert_eq!(extract_inst_id(MARK), Some(&b"BTC-USD-SWAP"[..]));
    }

    #[test]
    fn extract_inst_id_none_when_absent() {
        assert_eq!(extract_inst_id(b"{\"event\":\"error\"}"), None);
    }

    #[test]
    fn extract_error_inst_id_reads_msg_text_form() {
        // WS2: the 60018 post-settlement shape — instId appears
        // UNQUOTED inside the msg string, terminated by a space.
        let e = br#"{"event":"error","code":"60018","msg":"Wrong URL or channel:bbo-tbt,instId:BTC-USD-260828-45000-C doesn't exist. Please use the correct URL, channel and parameters referring to API document.","connId":"a4d3ae55"}"#;
        assert_eq!(
            extract_error_inst_id(e),
            Some(&b"BTC-USD-260828-45000-C"[..])
        );
        // Quote-terminated (instId at the end of the msg string).
        let e2 = br#"{"event":"error","code":"60018","msg":"channel:trades,instId:ETH-USDT-SWAP","connId":"x"}"#;
        assert_eq!(extract_error_inst_id(e2), Some(&b"ETH-USDT-SWAP"[..]));
    }

    #[test]
    fn extract_error_inst_id_none_when_error_names_no_instrument() {
        // 60012-class errors carry no instId → None (drop attributes
        // to SYMBOL_ID_NONE).
        let e = br#"{"event":"error","code":"60012","msg":"Invalid request: {\"op\":\"subscribe\"}","connId":"x"}"#;
        assert_eq!(extract_error_inst_id(e), None);
        // Degenerate: key present but no word bytes after it.
        assert_eq!(extract_error_inst_id(b"msg instId: "), None);
        assert_eq!(extract_error_inst_id(b"no key at all"), None);
    }

    // ---- parse_bbo ------------------------------------------------

    #[test]
    fn parse_bbo_extracts_both_sides() {
        let f = parse_bbo(BBO, 7).unwrap();
        assert_eq!(f.sym, 7);
        assert_eq!(f.ask_px_1e6, 111_060_000);
        assert_eq!(f.ask_qty_1e6, 55_154_000_000);
        assert_eq!(f.bid_px_1e6, 111_050_000);
        assert_eq!(f.bid_qty_1e6, 57_745_000_000);
        assert_eq!(f.ts_ns, 1_670_324_386_802 * 1_000_000);
        assert_eq!(f.seq_id, 363_996_337);
    }

    #[test]
    fn parse_bbo_handles_empty_ask_side() {
        let b = br#"{"arg":{"channel":"bbo-tbt","instId":"X"},"data":[{"asks":[],"bids":[["1.5","2","0","1"]],"ts":"1000","seqId":9}]}"#;
        let f = parse_bbo(b, 1).unwrap();
        assert_eq!(f.ask_px_1e6, 0);
        assert_eq!(f.ask_qty_1e6, 0);
        assert_eq!(f.bid_px_1e6, 1_500_000);
    }

    #[test]
    fn parse_bbo_rejects_missing_ts_and_double_empty() {
        let no_ts = br#"{"asks":[["1","1","0","1"]],"bids":[["1","1","0","1"]],"seqId":1}"#;
        assert!(parse_bbo(no_ts, 0).is_none());
        let both_empty = br#"{"asks":[],"bids":[],"ts":"1000","seqId":1}"#;
        assert!(parse_bbo(both_empty, 0).is_none());
    }

    #[test]
    fn parse_bbo_defaults_seq_to_zero_when_absent() {
        let b = br#"{"asks":[["2","1","0","1"]],"bids":[["1","1","0","1"]],"ts":"1000"}"#;
        assert_eq!(parse_bbo(b, 0).unwrap().seq_id, 0);
    }

    // ---- parse_trade ---------------------------------------------

    #[test]
    fn parse_trade_extracts_fields() {
        let t = parse_trade(TRADE_BUY, 3).unwrap();
        assert_eq!(t.sym, 3);
        assert_eq!(t.trade_id, 130_639_474);
        assert_eq!(t.px_1e6, 42_219_900_000);
        assert_eq!(t.qty_1e6, 120_603);
        assert_eq!(t.side, 0);
        assert_eq!(t.ts_ns, 1_630_048_897_897 * 1_000_000);
        assert_eq!(t.seq_id, 123_456);
    }

    #[test]
    fn parse_trade_sell_side_and_missing_side() {
        let sell = br#"{"tradeId":"1","px":"1.0","sz":"1.0","side":"sell","ts":"1000"}"#;
        assert_eq!(parse_trade(sell, 0).unwrap().side, 1);
        let bad = br#"{"tradeId":"1","px":"1.0","sz":"1.0","ts":"1000"}"#;
        assert!(parse_trade(bad, 0).is_none());
    }

    // ---- parse_mark_price ----------------------------------------

    #[test]
    fn parse_mark_price_extracts_fields() {
        let m = parse_mark_price(MARK, 9).unwrap();
        assert_eq!(m.sym, 9);
        assert_eq!(m.mark_px_1e6, 42_310_600_000);
        assert_eq!(m.ts_ns, 1_630_049_455_539 * 1_000_000);
    }

    #[test]
    fn parse_mark_price_rejects_garbage() {
        assert!(parse_mark_price(b"{}", 0).is_none());
    }

    // ---- parse_funding_rate --------------------------------------

    #[test]
    fn parse_funding_rate_keeps_1e9_precision() {
        let f = parse_funding_rate(FUNDING, 4).unwrap();
        assert_eq!(f.funding_rate_1e9, 59_300);
        assert_eq!(f.funding_time_ns, 1_630_051_200_000 * 1_000_000);
        assert_eq!(f.ts_ns, 1_630_048_897_897 * 1_000_000);
    }

    #[test]
    fn parse_funding_rate_negative_and_ts_fallback() {
        let b = br#"{"fundingRate":"-0.000375","fundingTime":"2000"}"#;
        let f = parse_funding_rate(b, 0).unwrap();
        assert_eq!(f.funding_rate_1e9, -375_000);
        // No ts → falls back to fundingTime.
        assert_eq!(f.ts_ns, 2_000 * 1_000_000);
        assert_eq!(f.ts_ns, f.funding_time_ns);
    }

    #[test]
    fn parse_funding_rate_rejects_missing_funding_time() {
        assert!(parse_funding_rate(br#"{"fundingRate":"0.0001"}"#, 0).is_none());
    }

    // ---- parse_book_header ---------------------------------------

    #[test]
    fn parse_book_header_snapshot_and_update() {
        let s = parse_book_header(BOOK_SNAP, 2).unwrap();
        assert_eq!(s.action, BOOK_ACTION_SNAPSHOT);
        assert_eq!(s.prev_seq_id, -1);
        assert_eq!(s.seq_id, 123_456);
        let u = parse_book_header(BOOK_UPD, 2).unwrap();
        assert_eq!(u.action, BOOK_ACTION_UPDATE);
        assert_eq!(u.prev_seq_id, 123_456);
        assert_eq!(u.seq_id, 123_457);
    }

    #[test]
    fn parse_book_header_rejects_missing_action() {
        let b = br#"{"data":[{"prevSeqId":1,"seqId":2,"ts":"1000"}]}"#;
        assert!(parse_book_header(b, 0).is_none());
    }

    // ---- symbol table --------------------------------------------

    #[test]
    fn symbol_table_roundtrip() {
        let mut t = OkxSymbolTable::new();
        t.insert(b"BTC-USDT", 0x0200_0001, OkxInstType::Spot)
            .unwrap();
        t.insert(b"ETH-USDT", 0x0200_0002, OkxInstType::Spot)
            .unwrap();
        assert_eq!(t.lookup(b"BTC-USDT"), Some(0x0200_0001));
        assert_eq!(t.lookup(b"ETH-USDT"), Some(0x0200_0002));
        assert_eq!(t.lookup(b"XRP-USDT"), None);
        assert_eq!(t.index_of(0x0200_0002), Some(1));
        assert_eq!(t.get(0).unwrap().0, b"BTC-USDT");
        assert_eq!(t.get(0).unwrap().2, OkxInstType::Spot);
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn symbol_table_carries_inst_type_per_row() {
        let mut t = OkxSymbolTable::new();
        t.insert(b"BTC-USDT", 1, OkxInstType::Spot).unwrap();
        t.insert(b"BTC-USDT-SWAP", 2, OkxInstType::Swap).unwrap();
        t.insert(b"BTC-USD-260821", 3, OkxInstType::Futures)
            .unwrap();
        assert_eq!(t.get(0).unwrap().2, OkxInstType::Spot);
        assert_eq!(t.get(1).unwrap().2, OkxInstType::Swap);
        assert_eq!(t.get(2).unwrap().2, OkxInstType::Futures);
    }

    #[test]
    fn opt_summary_row_parses_and_rejects() {
        // Live wire shape: quoted decimals, *BS greeks, negative
        // theta, sci-notation gamma, empty realVol skipped.
        let row = br#"{"instType":"OPTION","instId":"BTC-USD-260327-100000-C","uly":"BTC-USD","delta":"0.0000064","gamma":"0.0000000121","theta":"-0.000001","vega":"0.0000029","deltaBS":"0.512","gammaBS":"1.234e-5","thetaBS":"-85.3","vegaBS":"152.3","realVol":"","bidVol":"0.62","askVol":"0.68","markVol":"0.6543","lever":"12.3","fwdPx":"77300.12","ts":"1774598400123"}"#;
        let f = parse_opt_summary_row(row).expect("parses");
        assert_eq!(f.mark_iv_1e9, 654_300_000); // fraction already
        assert_eq!(f.fwd_px_1e9, 77_300_120_000_000);
        assert_eq!(f.delta_1e9, 512_000_000); // deltaBS, not delta
        assert_eq!(f.gamma_1e9, 12_340);
        assert_eq!(f.vega_1e6, 152_300_000);
        assert_eq!(f.theta_1e6, -85_300_000);
        // Empty markVol (pre-listing rows) rejects.
        let empty = br#"{"instId":"X","markVol":"","fwdPx":"1","deltaBS":"0","gammaBS":"0","thetaBS":"0","vegaBS":"0"}"#;
        assert!(parse_opt_summary_row(empty).is_none());
        // Missing any BS greek rejects.
        let no_vega = br#"{"instId":"X","markVol":"0.5","fwdPx":"1","deltaBS":"0","gammaBS":"0","thetaBS":"0"}"#;
        assert!(parse_opt_summary_row(no_vega).is_none());
        // Bare (unquoted) number = contract change → reject.
        let bare = br#"{"instId":"X","markVol":0.5,"fwdPx":"1","deltaBS":"0","gammaBS":"0","thetaBS":"0","vegaBS":"0"}"#;
        assert!(parse_opt_summary_row(bare).is_none());
    }

    #[test]
    fn inst_family_extracts_and_subscribe_renders_family_key() {
        let ack = br#"{"event":"subscribe","arg":{"channel":"opt-summary","instFamily":"BTC-USD"},"connId":"x"}"#;
        assert_eq!(extract_inst_family(ack), Some(&b"BTC-USD"[..]));
        assert_eq!(extract_inst_family(b"{}"), None);
        // write_op renders instFamily for the opt-summary channel and
        // instId for everything else, in one batch.
        let args = [
            SubArg {
                channel: OkxChannel::OptSummary,
                inst_id: b"BTC-USD",
            },
            SubArg {
                channel: OkxChannel::BboTbt,
                inst_id: b"BTC-USD-260327-100000-C",
            },
        ];
        let mut buf = [0u8; 512];
        let n = write_subscribe_batch(&mut buf, &args).expect("fits");
        let s = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            s.contains(r#"{"channel":"opt-summary","instFamily":"BTC-USD"}"#),
            "{s}"
        );
        assert!(
            s.contains(r#"{"channel":"bbo-tbt","instId":"BTC-USD-260327-100000-C"}"#),
            "{s}"
        );
        // classify sees the data push.
        let push =
            br#"{"arg":{"channel":"opt-summary","instFamily":"BTC-USD"},"data":[{"instId":"X"}]}"#;
        assert_eq!(classify(push), OkxMsgKind::Data(OkxChannel::OptSummary));
    }

    #[test]
    fn inst_type_decodes_wire_strings() {
        assert_eq!(OkxInstType::from_bytes(b"SPOT"), Some(OkxInstType::Spot));
        assert_eq!(OkxInstType::from_bytes(b"SWAP"), Some(OkxInstType::Swap));
        assert_eq!(
            OkxInstType::from_bytes(b"FUTURES"),
            Some(OkxInstType::Futures)
        );
        // M2.2: OPTION decodes (the per-page contract lives in the
        // discovery RowMode — legacy pages still reject option rows).
        assert_eq!(
            OkxInstType::from_bytes(b"OPTION"),
            Some(OkxInstType::Option)
        );
        assert_eq!(OkxInstType::from_bytes(b"MARGIN"), None);
        assert_eq!(OkxInstType::from_bytes(b""), None);
        assert_eq!(OkxInstType::Swap.as_str(), "SWAP");
        assert_eq!(OkxInstType::Option.as_str(), "OPTION");
    }

    #[test]
    fn symbol_table_rejects_bad_input() {
        let mut t = OkxSymbolTable::new();
        assert_eq!(
            t.insert(b"", 1, OkxInstType::Spot),
            Err(SymbolTableErr::Empty)
        );
        assert_eq!(
            t.insert(&[b'A'; OKX_INST_ID_MAX + 1], 1, OkxInstType::Spot),
            Err(SymbolTableErr::TooLong)
        );
        let mut i = 0u32;
        while (i as usize) < OKX_MAX_SYMBOLS {
            t.insert(format!("S{i}").as_bytes(), i, OkxInstType::Spot)
                .unwrap();
            i += 1;
        }
        assert_eq!(
            t.insert(b"OVER", 99, OkxInstType::Spot),
            Err(SymbolTableErr::Full)
        );
    }

    // ---- seq chain -----------------------------------------------

    #[test]
    fn chain_init_chain_heartbeat_reset() {
        let mut c = OkxSeqChain::new();
        // Snapshot.
        assert_eq!(c.apply(-1, 10), ChainOutcome::Init);
        // Normal chain.
        assert_eq!(c.apply(10, 11), ChainOutcome::Chained);
        // Idle heartbeat repeats the chain point.
        assert_eq!(c.apply(11, 11), ChainOutcome::IdleHeartbeat);
        // Chain continues after heartbeat.
        assert_eq!(c.apply(11, 12), ChainOutcome::Chained);
        // Maintenance reset: chain intact, seq goes backwards.
        assert_eq!(c.apply(12, 5), ChainOutcome::Reset);
        assert_eq!(c.apply(5, 6), ChainOutcome::Chained);
    }

    #[test]
    fn chain_gap_paths_rearm_for_snapshot() {
        // Joined mid-stream: first message is no snapshot.
        let mut c = OkxSeqChain::new();
        assert_eq!(c.apply(7, 8), ChainOutcome::Gap);
        // Still awaiting: a snapshot then inits.
        assert_eq!(c.apply(-1, 20), ChainOutcome::Init);
        // Broken chain: prev doesn't match last.
        assert_eq!(c.apply(99, 100), ChainOutcome::Gap);
        // Re-armed: next must be a snapshot again.
        assert_eq!(c.apply(100, 101), ChainOutcome::Gap);
        assert_eq!(c.apply(-1, 200), ChainOutcome::Init);
        // Heartbeat that repeats the WRONG seq is also a gap.
        assert_eq!(c.apply(150, 150), ChainOutcome::Gap);
    }

    // ---- trade seq -----------------------------------------------

    #[test]
    fn trade_seq_monotonic_rules() {
        let mut m = TradeSeqMonitor::new();
        assert_eq!(m.apply(5), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(5), TradeSeqOutcome::Ok, "equal ids are legal");
        assert_eq!(m.apply(9), TradeSeqOutcome::Ok);
        assert_eq!(m.apply(3), TradeSeqOutcome::Regression);
        // Resynced to the observed value.
        assert_eq!(m.apply(4), TradeSeqOutcome::Ok);
    }

    // ---- subscribe writers ---------------------------------------

    #[test]
    fn write_subscribe_batch_exact_bytes() {
        let mut dst = [0u8; 256];
        let args = [
            SubArg {
                channel: OkxChannel::BboTbt,
                inst_id: b"BTC-USDT",
            },
            SubArg {
                channel: OkxChannel::Trades,
                inst_id: b"ETH-USDT",
            },
        ];
        let n = write_subscribe_batch(&mut dst, &args).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"op":"subscribe","args":[{"channel":"bbo-tbt","instId":"BTC-USDT"},{"channel":"trades","instId":"ETH-USDT"}]}"#
                as &[u8]
        );
    }

    #[test]
    fn write_unsubscribe_batch_and_tiny_dst() {
        let mut dst = [0u8; 256];
        let args = [SubArg {
            channel: OkxChannel::Books,
            inst_id: b"BTC-USDT",
        }];
        let n = write_unsubscribe_batch(&mut dst, &args).unwrap();
        assert!(n > 0);
        assert!(dst[..n].starts_with(br#"{"op":"unsubscribe""#));
        let mut tiny = [0u8; 8];
        assert!(write_subscribe_batch(&mut tiny, &args).is_none());
    }

    // ---- sub ids --------------------------------------------------

    #[test]
    fn sub_ids_are_nonzero_and_distinct() {
        let a = sub_id_of(OkxChannel::BboTbt, b"BTC-USDT");
        let b = sub_id_of(OkxChannel::Trades, b"BTC-USDT");
        let c = sub_id_of(OkxChannel::BboTbt, b"ETH-USDT");
        assert_ne!(a.0, 0);
        assert_ne!(a, b, "channel must differentiate");
        assert_ne!(a, c, "instId must differentiate");
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
        fn bbo_roundtrips(
            ap in 1u32..999_999u32,
            aq in 0u32..999_999u32,
            bp in 1u32..999_999u32,
            bq in 0u32..999_999u32,
            ts in 1u64..2_000_000_000_000u64,
            seq in 0i64..i64::MAX / 2,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"arg":{{"channel":"bbo-tbt","instId":"X"}},"data":[{{"asks":[["0.{ap:06}","0.{aq:06}","0","1"]],"bids":[["0.{bp:06}","0.{bq:06}","0","1"]],"ts":"{ts}","seqId":{seq}}}]}}"#,
            ).unwrap();
            let f = parse_bbo(buf.as_bytes(), 5).unwrap();
            prop_assert_eq!(f.sym, 5);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, aq as i64);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, bq as i64);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
            prop_assert_eq!(f.seq_id, seq);
        }

        #[test]
        fn funding_roundtrips_1e9(
            rate_nano in -3_000_000i64..3_000_000i64,
            ft in 1u64..2_000_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(160);
            use std::fmt::Write;
            let sign = if rate_nano < 0 { "-" } else { "" };
            write!(
                &mut buf,
                r#"{{"fundingRate":"{sign}0.{:09}","fundingTime":"{ft}"}}"#,
                rate_nano.unsigned_abs(),
            ).unwrap();
            let f = parse_funding_rate(buf.as_bytes(), 1).unwrap();
            prop_assert_eq!(f.funding_rate_1e9, rate_nano);
            prop_assert_eq!(f.funding_time_ns, ft * 1_000_000);
        }

        #[test]
        fn no_parser_panics_on_arbitrary_bytes(
            buf in proptest::collection::vec(any::<u8>(), 0..=400)
        ) {
            let _ = classify(&buf);
            let _ = extract_inst_id(&buf);
            let _ = parse_bbo(&buf, 0);
            let _ = parse_trade(&buf, 0);
            let _ = parse_mark_price(&buf, 0);
            let _ = parse_funding_rate(&buf, 0);
            let _ = parse_book_header(&buf, 0);
        }
    }
}
