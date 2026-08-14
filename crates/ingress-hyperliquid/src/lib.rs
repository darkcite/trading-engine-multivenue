//! # ingress-hyperliquid
//!
//! Hyperliquid **public** WebSocket ingress (Phase 8d). Channels per
//! `docs/phase-8-plan.md` §4.3/§4.4 (venue facts verified 2026-08-14):
//!
//! * `bbo {coin}`     — pushed **only on BBO change** → [`core_types::Tick`]
//! * `l2Book {coin}`  — **full snapshot every block, ≥ 0.5 s cadence,
//!   ≤ 20 levels/side — no diffs, no seq**; consumed for capture +
//!   integrity (§4.5)
//! * `trades {coin}`  — batched rows per push
//! * `activeAssetCtx {coin}` — funding / oracle / mark / OI (perp coins
//!   only — see *coin gating* below)
//! * `allMids`        — cheap whole-venue mid sweep (slow-lane capture)
//! * `outcomeMetaUpdates` — HIP-4 lifecycle (`outcomeCreated` /
//!   `outcomeSettled` / `questionUpdated` / `questionSettled`),
//!   slow-lane capture
//!
//! `fastAssetCtxs` is deliberately **skipped in v1** — it is
//! DEFLATE-compressed and decompression in the hot path is avoidable
//! complexity (plan §4.3).
//!
//! ## HIP-4 outcome coins (§4.4)
//!
//! Outcome markets ride the **ordinary** market-data surface: coin
//! string `#<enc>` with `enc = 10*outcome + side` (side 0 = Yes,
//! 1 = No). `bbo` / `l2Book` / `trades` subscriptions work on `#<enc>`
//! unchanged — the coin string flows through [`HlCoinTable`] like any
//! other, no special code path. `outcomeMetaUpdates` is captured on
//! the slow lane so new outcome markets are observed as they appear.
//!
//! ## Integrity (§6.2 row: Hyperliquid)
//!
//! Snapshots are **stateless** — there is no sequence chain to check
//! and nothing to resubscribe: missed data is recovered by the next
//! snapshot *by construction*. The monitor is pure staleness
//! ([`HlStaleness`]): per subscribed coin, the `l2Book` venue `time`
//! must strictly advance within the configured budget (default
//! **2 s** = 2× block cadence) or the session is flagged and
//! reconnected. A staleness trip counts into `gaps_total` — the §6.4
//! counter set has no dedicated stale counter; the pairing
//! (gap increment + `RunResult::Stale` reconnect) is the documented
//! signature of a staleness event.
//!
//! ## Subscribe acks
//!
//! Every `{"method":"subscribe",...}` is answered by a
//! `{"channel":"subscriptionResponse",...}` frame echoing the
//! subscription. Acks are verified **per subscription** through an
//! expected/found bitmask ([`MaskBits`], one bit per configured
//! subscription); any `{"channel":"error",...}` frame fails the
//! session (fail-fast doctrine). The run loop enforces an ack
//! deadline: all expected bits must be found within the configured
//! budget of entering `Steady`.
//!
//! ## Keepalive
//!
//! Client sends [`PING_PAYLOAD`] (`{"method":"ping"}`) every 50 s;
//! the venue cuts connections idle for 60 s and answers with
//! `{"channel":"pong"}`. Scheduling comes from `core_net::Keepalive`
//! in the run loop.
//!
//! ## Decisions documented (crate-header policy, mirrors 8b/8c)
//!
//! * **`Tick.venue_seq` = `time` (ms) truncated to `u32`.** `bbo`
//!   pushes carry no sequence number, only the venue event time in
//!   milliseconds. Same policy as Deribit quotes: monotonic across
//!   reconnects, wraps every ~49.7 days; same-ms updates collapse at
//!   `TopOfBook`. Full-width times live in the staleness monitor —
//!   truncation happens only at the `Tick` boundary.
//! * **Units:** prices are USD (outcome coins: collateral units in
//!   \[0, 1\]) ×1e6; sizes are **base-coin units** ×1e6 (unlike
//!   Deribit's USD notionals). Funding is ×1e9 — 1e6 would truncate
//!   typical rates (~1e-5) to noise.
//! * **REST discovery deferred to 8e** (`POST /info`: `meta`,
//!   `spotMeta`, `perpDexs`, `outcomeMeta`) with the boot coverage
//!   audit, its consumer — same disposition as OKX/Deribit. Until
//!   then coins come from the `--hl-coins` flag, ordinals from flag
//!   order.
//! * **Coin gating:** `activeAssetCtx` is subscribed only for perp
//!   coins — skipped for `#<enc>` (outcome) and `@<idx>` (spot)
//!   coins, whose context rides different channels/shapes. HIP-3
//!   builder-dex coins (`dex:COIN`) are perps and are not skipped.
//!
//! ## Zero-copy note (house doctrine)
//!
//! All parsing is in-place over `&[u8]` in the rx buffer. The one
//! unavoidable copy per event is the 64-byte parsed POD moved into
//! the SPSC ring by `try_push` (ownership transfer) — same as every
//! ingress. Subscribe/ping frames render into fixed stack scratch.

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

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, RX_BUF_SIZE,
    TX_BUF_SIZE,
};

use core_net::SubId;
use core_parse::{find_field, scan_price_1e6, scan_price_1e9, scan_u64, skip_byte};
use core_types::{NsTs, SymbolId};

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Longest coin string we accept. Native perps are short (`BTC`);
/// HIP-3 builder-dex coins are `dex:COIN`; HIP-4 outcome coins are
/// `#<enc>` (`enc` ≤ 10 digits); spot pairs are `@<idx>`.
pub const HL_COIN_MAX: usize = 24;

/// Maximum number of configured coins per connection. Fixed-cap
/// tables everywhere; boot fails fast beyond this.
pub const HL_MAX_COINS: usize = 16;

/// Client keepalive probe — Hyperliquid wants the JSON text frame
/// `{"method":"ping"}` (venue cuts at 60 s idle; cli sends at 50 s).
pub const PING_PAYLOAD: &[u8] = b"{\"method\":\"ping\"}";

/// Per-coin channels tracked in the ack mask: bbo, l2Book, trades,
/// activeAssetCtx.
pub const CHANNELS_PER_COIN: usize = 4;

// ---------------------------------------------------------------
// Channels + message classification
// ---------------------------------------------------------------

/// Public channels this ingress speaks. `#[repr(u8)]` so the value
/// can ride in PODs and metrics labels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HlChannel {
    /// `bbo` — best bid/offer, pushed only on change.
    Bbo = 0,
    /// `l2Book` — full snapshot per block, ≤ 20 levels/side.
    L2Book = 1,
    /// `trades`.
    Trades = 2,
    /// `activeAssetCtx` — funding/oracle/mark/OI (perps).
    ActiveAssetCtx = 3,
    /// `allMids` — whole-venue mid map (no coin arg).
    AllMids = 4,
    /// `outcomeMetaUpdates` — HIP-4 lifecycle (no coin arg).
    OutcomeMetaUpdates = 5,
}

impl HlChannel {
    /// The wire name Hyperliquid uses in `subscription.type` and
    /// push `channel` fields.
    #[inline]
    pub const fn wire_name(self) -> &'static [u8] {
        match self {
            HlChannel::Bbo => b"bbo",
            HlChannel::L2Book => b"l2Book",
            HlChannel::Trades => b"trades",
            HlChannel::ActiveAssetCtx => b"activeAssetCtx",
            HlChannel::AllMids => b"allMids",
            HlChannel::OutcomeMetaUpdates => b"outcomeMetaUpdates",
        }
    }

    /// Whether this channel takes a `coin` argument.
    #[inline]
    pub const fn per_coin(self) -> bool {
        !matches!(self, HlChannel::AllMids | HlChannel::OutcomeMetaUpdates)
    }
}

/// Coarse classification of one inbound text frame. Cheap byte
/// scans only — full parsing happens per-channel afterwards. Channel
/// names are matched **with** their closing quote so `bbo` can never
/// alias a longer name and `activeAssetCtx` can never alias
/// `activeSpotAssetCtx`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HlMsgKind {
    /// `{"channel":"pong"}` answering our keepalive probe.
    Pong,
    /// `{"channel":"subscriptionResponse","data":{...}}`.
    SubResponse,
    /// `{"channel":"error","data":"..."}` — fatal (fail-fast).
    Error,
    /// Data push for one channel.
    Data(HlChannel),
    /// Anything else — counted as a parse rejection by the caller.
    Unknown,
}

/// Classify one inbound payload. Zero-alloc; key-matched so field
/// order never matters.
#[inline]
pub fn classify(payload: &[u8]) -> HlMsgKind {
    if memchr::memmem::find(payload, b"\"channel\":\"pong\"").is_some() {
        return HlMsgKind::Pong;
    }
    if memchr::memmem::find(payload, b"\"channel\":\"subscriptionResponse\"").is_some() {
        return HlMsgKind::SubResponse;
    }
    if memchr::memmem::find(payload, b"\"channel\":\"error\"").is_some() {
        return HlMsgKind::Error;
    }
    if memchr::memmem::find(payload, b"\"channel\":\"bbo\"").is_some() {
        return HlMsgKind::Data(HlChannel::Bbo);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"l2Book\"").is_some() {
        return HlMsgKind::Data(HlChannel::L2Book);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"trades\"").is_some() {
        return HlMsgKind::Data(HlChannel::Trades);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"activeAssetCtx\"").is_some() {
        return HlMsgKind::Data(HlChannel::ActiveAssetCtx);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"allMids\"").is_some() {
        return HlMsgKind::Data(HlChannel::AllMids);
    }
    if memchr::memmem::find(payload, b"\"channel\":\"outcomeMetaUpdates\"").is_some() {
        return HlMsgKind::Data(HlChannel::OutcomeMetaUpdates);
    }
    HlMsgKind::Unknown
}

/// Extract the first `coin` value bytes from a payload (data pushes
/// carry it inside `data`; subscription echoes inside
/// `subscription`). Returns a subslice of `payload`; no copy. HIP-4
/// `#<enc>` and spot `@<idx>` coins pass through unchanged.
#[inline]
pub fn extract_coin(payload: &[u8]) -> Option<&[u8]> {
    let start = find_field(payload, b"\"coin\":")?;
    let start = skip_byte(payload, start, b'"');
    let rel_end = memchr::memchr(b'"', payload.get(start..)?)?;
    payload.get(start..start + rel_end)
}

/// Whether `activeAssetCtx` applies to this coin — perps only:
/// outcome (`#`) and spot (`@`) coins are skipped (crate-header
/// *coin gating* note).
#[inline]
pub fn coin_wants_asset_ctx(coin: &[u8]) -> bool {
    !matches!(coin.first(), Some(b'#') | Some(b'@'))
}

// ---------------------------------------------------------------
// Frame PODs — one cache line each, explicit padding
// ---------------------------------------------------------------

/// Parsed `bbo` push. A missing/one-sided level is px = 0, qty = 0
/// (`null` entry on the wire).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HlBboFrame {
    /// Venue event time (`time`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Best bid price ×1e6 (0 = side empty).
    pub bid_px_1e6: i64,
    /// Best bid size ×1e6 (base-coin units).
    pub bid_qty_1e6: i64,
    /// Best ask price ×1e6 (0 = side empty).
    pub ask_px_1e6: i64,
    /// Best ask size ×1e6 (base-coin units).
    pub ask_qty_1e6: i64,
    /// Resolved symbol (venue-namespaced, bits 31..24 = Hyperliquid).
    pub sym: SymbolId,
    // Explicit tail padding — keeps the slot exactly 64 B.
    _pad: [u8; 20],
}

/// Parsed `l2Book` snapshot **header** — §4.5: depth is consumed for
/// capture + integrity, so only the event time, level counts and the
/// touch are lifted; levels stay in the rx buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HlL2BookFrame {
    /// Venue event time (`time`, ms) converted to ns — the staleness
    /// monitor's clock.
    pub ts_ns: NsTs,
    /// Best bid price ×1e6 (0 = side empty).
    pub best_bid_px_1e6: i64,
    /// Best ask price ×1e6 (0 = side empty).
    pub best_ask_px_1e6: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// Bid levels in this snapshot (venue caps at 20).
    pub n_bids: u16,
    /// Ask levels in this snapshot (venue caps at 20).
    pub n_asks: u16,
    // Explicit tail padding.
    _pad: [u8; 32],
}

/// Parsed `trades` row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HlTradeFrame {
    /// Venue trade id (`tid`, unquoted decimal).
    pub tid: u64,
    /// Venue event time (`time`, ms) converted to ns.
    pub ts_ns: NsTs,
    /// Trade price ×1e6.
    pub px_1e6: i64,
    /// Trade size ×1e6 (base-coin units).
    pub qty_1e6: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    /// Aggressor side: 0 = buy (wire `"B"`), 1 = sell (wire `"A"`).
    pub side: u8,
    // Explicit tail padding.
    _pad: [u8; 27],
}

/// Parsed `activeAssetCtx` push (funding/oracle/mark/OI). The ctx
/// carries no venue timestamp — capture is slow-lane and the run
/// loop's local clock suffices for accounting.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HlAssetCtxFrame {
    /// Funding rate ×1e9 (signed — crate-header units note).
    pub funding_1e9: i64,
    /// Mark price ×1e6.
    pub mark_px_1e6: i64,
    /// Oracle price ×1e6.
    pub oracle_px_1e6: i64,
    /// Open interest ×1e6 (base-coin units).
    pub oi_1e6: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    // Explicit tail padding.
    _pad: [u8; 28],
}

/// HIP-4 lifecycle event kinds (`outcomeMetaUpdates`).
pub const OUTCOME_CREATED: u8 = 0;
/// `outcomeSettled`.
pub const OUTCOME_SETTLED: u8 = 1;
/// `questionUpdated`.
pub const QUESTION_UPDATED: u8 = 2;
/// `questionSettled`.
pub const QUESTION_SETTLED: u8 = 3;

/// Sentinel for [`HlOutcomeMetaFrame::enc`] when the update carries
/// no `#<enc>` coin.
pub const OUTCOME_ENC_NONE: u32 = u32::MAX;

/// Parsed `outcomeMetaUpdates` push (slow-lane capture). Robust by
/// design: kind is required, coin encoding and time are optional —
/// the HIP-4 update shape may grow fields as permissionless
/// deployment leaves testnet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(64))]
pub struct HlOutcomeMetaFrame {
    /// Venue event time (`time`, ms) converted to ns; 0 when absent.
    pub ts_ns: NsTs,
    /// Outcome encoding from a `#<enc>` coin;
    /// [`OUTCOME_ENC_NONE`] when absent.
    pub enc: u32,
    /// Lifecycle kind: [`OUTCOME_CREATED`] / [`OUTCOME_SETTLED`] /
    /// [`QUESTION_UPDATED`] / [`QUESTION_SETTLED`].
    pub kind: u8,
    // Explicit tail padding.
    _pad: [u8; 51],
}

const _SIZE_CHECKS: () = {
    assert!(::core::mem::size_of::<HlBboFrame>() == 64);
    assert!(::core::mem::size_of::<HlL2BookFrame>() == 64);
    assert!(::core::mem::size_of::<HlTradeFrame>() == 64);
    assert!(::core::mem::size_of::<HlAssetCtxFrame>() == 64);
    assert!(::core::mem::size_of::<HlOutcomeMetaFrame>() == 64);
};

// ---------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------

/// Parse an **unquoted** millisecond timestamp field (Hyperliquid
/// sends `"time":1708622398623` as a bare number, unlike OKX's
/// quoted strings) into nanoseconds.
#[inline]
fn scan_bare_ms_to_ns(buf: &[u8], key: &[u8]) -> Option<u64> {
    let pos = find_field(buf, key)?;
    let (ms, _) = scan_u64(buf, pos)?;
    Some(ms.saturating_mul(1_000_000))
}

/// Parse one level object `{"px":"...","sz":"...","n":N}` or the
/// literal `null` at `pos`. Returns `(px_1e6, sz_1e6, end)` where
/// `end` is one past the object. Field order is normative on this
/// venue (`px`, `sz`, `n`).
#[inline]
fn scan_level_obj(buf: &[u8], pos: usize) -> Option<(i64, i64, usize)> {
    if buf.get(pos..pos + 4)? == b"null" {
        return Some((0, 0, pos + 4));
    }
    if buf.get(pos..pos + 7)? != b"{\"px\":\"" {
        return None;
    }
    let (px, px_end) = scan_price_1e6(buf, pos + 7)?;
    if buf.get(px_end..px_end + 8)? != b"\",\"sz\":\"" {
        return None;
    }
    let (sz, sz_end) = scan_price_1e6(buf, px_end + 8)?;
    let rel = memchr::memchr(b'}', buf.get(sz_end..)?)?;
    Some((px, sz, sz_end + rel + 1))
}

/// Walk one side's level array `[{..},{..}]` at `pos` (the `[`).
/// Returns `(level_count, best_px_1e6, best_sz_1e6, end)`; an empty
/// side yields `(0, 0, 0, end)`. Every level is validated — ≤ 20 on
/// this venue, so the strict walk stays cheap.
#[inline]
fn scan_side_levels(buf: &[u8], pos: usize) -> Option<(u16, i64, i64, usize)> {
    if *buf.get(pos)? != b'[' {
        return None;
    }
    if *buf.get(pos + 1)? == b']' {
        return Some((0, 0, 0, pos + 2));
    }
    let (best_px, best_sz, mut at) = scan_level_obj(buf, pos + 1)?;
    let mut n: u16 = 1;
    loop {
        match *buf.get(at)? {
            b',' => {
                let (_px, _sz, e) = scan_level_obj(buf, at + 1)?;
                n = n.saturating_add(1);
                at = e;
            }
            b']' => return Some((n, best_px, best_sz, at + 1)),
            _ => return None,
        }
    }
}

// ---------------------------------------------------------------
// Channel parsers
// ---------------------------------------------------------------

/// Parse a `bbo` push into an [`HlBboFrame`]. `sym` is the
/// caller-resolved symbol (from [`extract_coin`] + [`HlCoinTable`]).
/// Returns `None` on malformed input — caller counts it.
#[inline]
pub fn parse_bbo(payload: &[u8], sym: SymbolId) -> Option<HlBboFrame> {
    let pos = find_field(payload, b"\"bbo\":")?;
    if *payload.get(pos)? != b'[' {
        return None;
    }
    let (bid_px_1e6, bid_qty_1e6, bid_end) = scan_level_obj(payload, pos + 1)?;
    if *payload.get(bid_end)? != b',' {
        return None;
    }
    let (ask_px_1e6, ask_qty_1e6, _ask_end) = scan_level_obj(payload, bid_end + 1)?;
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":")?;
    // A frame with both sides null carries no information.
    if bid_px_1e6 == 0 && ask_px_1e6 == 0 {
        return None;
    }
    Some(HlBboFrame {
        ts_ns,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        sym,
        _pad: [0; 20],
    })
}

/// Parse an `l2Book` snapshot into its [`HlL2BookFrame`] header.
/// `levels` is `[bids, asks]`, each best-first; levels themselves
/// stay in the rx buffer (§4.5).
#[inline]
pub fn parse_l2book_header(payload: &[u8], sym: SymbolId) -> Option<HlL2BookFrame> {
    let pos = find_field(payload, b"\"levels\":")?;
    if *payload.get(pos)? != b'[' {
        return None;
    }
    let (n_bids, best_bid_px_1e6, _bsz, bids_end) = scan_side_levels(payload, pos + 1)?;
    if *payload.get(bids_end)? != b',' {
        return None;
    }
    let (n_asks, best_ask_px_1e6, _asz, _asks_end) = scan_side_levels(payload, bids_end + 1)?;
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":")?;
    Some(HlL2BookFrame {
        ts_ns,
        best_bid_px_1e6,
        best_ask_px_1e6,
        sym,
        n_bids,
        n_asks,
        _pad: [0; 32],
    })
}

/// Parse one `trades` row into an [`HlTradeFrame`]. Hyperliquid
/// batches rows per push; the run loop walks rows by re-slicing the
/// payload at successive `"coin":"` markers.
#[inline]
pub fn parse_trade(payload: &[u8], sym: SymbolId) -> Option<HlTradeFrame> {
    // side: "side":"B" (buy) | "side":"A" (sell) — closing quote in
    // the pattern so a coin named B/A can never alias.
    let side = if memchr::memmem::find(payload, b"\"side\":\"B\"").is_some() {
        0u8
    } else if memchr::memmem::find(payload, b"\"side\":\"A\"").is_some() {
        1u8
    } else {
        return None;
    };
    let pos = find_field(payload, b"\"px\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (px_1e6, _) = scan_price_1e6(payload, pos)?;
    let pos = find_field(payload, b"\"sz\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (qty_1e6, _) = scan_price_1e6(payload, pos)?;
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":")?;
    // tid: unquoted decimal.
    let pos = find_field(payload, b"\"tid\":")?;
    let (tid, _) = scan_u64(payload, pos)?;
    Some(HlTradeFrame {
        tid,
        ts_ns,
        px_1e6,
        qty_1e6,
        sym,
        side,
        _pad: [0; 27],
    })
}

/// Parse an `activeAssetCtx` push into an [`HlAssetCtxFrame`]. All
/// four fields are required — a ctx without them is malformed for
/// the perp coins we subscribe.
#[inline]
pub fn parse_active_asset_ctx(payload: &[u8], sym: SymbolId) -> Option<HlAssetCtxFrame> {
    let pos = find_field(payload, b"\"funding\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (funding_1e9, _) = scan_price_1e9(payload, pos)?;
    let pos = find_field(payload, b"\"markPx\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (mark_px_1e6, _) = scan_price_1e6(payload, pos)?;
    let pos = find_field(payload, b"\"oraclePx\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (oracle_px_1e6, _) = scan_price_1e6(payload, pos)?;
    let pos = find_field(payload, b"\"openInterest\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let (oi_1e6, _) = scan_price_1e6(payload, pos)?;
    Some(HlAssetCtxFrame {
        funding_1e9,
        mark_px_1e6,
        oracle_px_1e6,
        oi_1e6,
        sym,
        _pad: [0; 28],
    })
}

/// Parse an `allMids` push: returns the number of mid entries (each
/// is `"COIN":"px"`, contributing exactly one `":"` byte triple).
/// Slow-lane capture — the count feeds coverage sanity, values stay
/// in the buffer.
#[inline]
pub fn parse_all_mids(payload: &[u8]) -> Option<u32> {
    let pos = find_field(payload, b"\"mids\":")?;
    if *payload.get(pos)? != b'{' {
        return None;
    }
    let n = memchr::memmem::find_iter(&payload[pos..], b"\":\"").count();
    Some(n as u32)
}

/// Parse an `outcomeMetaUpdates` push into an
/// [`HlOutcomeMetaFrame`]. Kind is required; `#<enc>` coin and time
/// are optional (see the frame doc).
#[inline]
pub fn parse_outcome_meta(payload: &[u8]) -> Option<HlOutcomeMetaFrame> {
    let kind = if memchr::memmem::find(payload, b"\"outcomeCreated\"").is_some() {
        OUTCOME_CREATED
    } else if memchr::memmem::find(payload, b"\"outcomeSettled\"").is_some() {
        OUTCOME_SETTLED
    } else if memchr::memmem::find(payload, b"\"questionUpdated\"").is_some() {
        QUESTION_UPDATED
    } else if memchr::memmem::find(payload, b"\"questionSettled\"").is_some() {
        QUESTION_SETTLED
    } else {
        return None;
    };
    let enc = match find_field(payload, b"\"coin\":") {
        Some(p) => {
            let p = skip_byte(payload, p, b'"');
            if payload.get(p) == Some(&b'#') {
                match scan_u64(payload, p + 1) {
                    Some((v, _)) if v <= u32::MAX as u64 => v as u32,
                    _ => OUTCOME_ENC_NONE,
                }
            } else {
                OUTCOME_ENC_NONE
            }
        }
        None => OUTCOME_ENC_NONE,
    };
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":").unwrap_or(0);
    Some(HlOutcomeMetaFrame {
        ts_ns,
        enc,
        kind,
        _pad: [0; 51],
    })
}

/// Parse a `subscriptionResponse` echo: returns the acknowledged
/// channel and (for per-coin channels) the echoed coin bytes.
/// `None` when the echo is not a `subscribe` ack (e.g. unsubscribe)
/// or names no known channel — caller treats that as quiet/reject.
#[inline]
pub fn parse_sub_response(payload: &[u8]) -> Option<(HlChannel, Option<&[u8]>)> {
    if memchr::memmem::find(payload, b"\"method\":\"subscribe\"").is_none() {
        return None;
    }
    let pos = find_field(payload, b"\"type\":")?;
    let pos = skip_byte(payload, pos, b'"');
    let rest = payload.get(pos..)?;
    let channel = if rest.starts_with(b"bbo\"") {
        HlChannel::Bbo
    } else if rest.starts_with(b"l2Book\"") {
        HlChannel::L2Book
    } else if rest.starts_with(b"trades\"") {
        HlChannel::Trades
    } else if rest.starts_with(b"activeAssetCtx\"") {
        HlChannel::ActiveAssetCtx
    } else if rest.starts_with(b"allMids\"") {
        HlChannel::AllMids
    } else if rest.starts_with(b"outcomeMetaUpdates\"") {
        HlChannel::OutcomeMetaUpdates
    } else {
        return None;
    };
    if channel.per_coin() {
        Some((channel, Some(extract_coin(payload)?)))
    } else {
        Some((channel, None))
    }
}

// ---------------------------------------------------------------
// Coin table — coin ⇄ SymbolId, fixed capacity, boot-built
// ---------------------------------------------------------------

/// Why an [`HlCoinTable::insert`] failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoinTableErr {
    /// All [`HL_MAX_COINS`] rows in use (boot misconfiguration).
    Full,
    /// Coin longer than [`HL_COIN_MAX`].
    TooLong,
    /// Coin empty.
    Empty,
}

/// Fixed-capacity `coin → SymbolId` map. Linear scan (N ≤ 16).
/// Single-owner: built at boot, read by the ingress thread. HIP-4
/// `#<enc>`, spot `@<idx>` and HIP-3 `dex:COIN` strings are ordinary
/// rows — no special surface.
pub struct HlCoinTable {
    rows: [(u8, [u8; HL_COIN_MAX], SymbolId); HL_MAX_COINS],
    len: usize,
}

impl HlCoinTable {
    /// Empty table.
    pub const fn new() -> Self {
        Self {
            rows: [(0, [0; HL_COIN_MAX], 0); HL_MAX_COINS],
            len: 0,
        }
    }

    /// Register `coin → sym`. Boot-time only.
    pub fn insert(&mut self, coin: &[u8], sym: SymbolId) -> Result<(), CoinTableErr> {
        if coin.is_empty() {
            return Err(CoinTableErr::Empty);
        }
        if coin.len() > HL_COIN_MAX {
            return Err(CoinTableErr::TooLong);
        }
        if self.len >= HL_MAX_COINS {
            return Err(CoinTableErr::Full);
        }
        let row = &mut self.rows[self.len];
        row.0 = coin.len() as u8;
        row.1[..coin.len()].copy_from_slice(coin);
        row.2 = sym;
        self.len += 1;
        Ok(())
    }

    /// Resolve a coin to its symbol. Hot path: length gate first,
    /// then bytewise compare.
    #[inline]
    pub fn lookup(&self, coin: &[u8]) -> Option<SymbolId> {
        let n = coin.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == coin {
                return Some(row.2);
            }
            i += 1;
        }
        None
    }

    /// Row accessor for subscribe building: `(coin, sym)`.
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

    /// Index of a coin string in insertion order (ack-mask bit
    /// derivation from `subscriptionResponse` echoes).
    #[inline]
    pub fn index_of_coin(&self, coin: &[u8]) -> Option<usize> {
        let n = coin.len();
        let mut i = 0;
        while i < self.len {
            let row = &self.rows[i];
            if row.0 as usize == n && &row.1[..n] == coin {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Number of configured coins.
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

impl Default for HlCoinTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Ack-verification mask (§ Subscribe acks, module doc)
// ---------------------------------------------------------------

/// Expected/found subscription bitmask. Per-coin channels occupy
/// bits `coin_idx * CHANNELS_PER_COIN + channel`; the two global
/// channels sit above [`ALL_MIDS_BIT`] / [`OUTCOME_META_BIT`].
/// 16 coins × 4 + 2 = 66 bits ⇒ `u128`.
pub type MaskBits = u128;

/// Mask bit for the global `allMids` subscription.
pub const ALL_MIDS_BIT: MaskBits = 1u128 << (HL_MAX_COINS * CHANNELS_PER_COIN);
/// Mask bit for the global `outcomeMetaUpdates` subscription.
pub const OUTCOME_META_BIT: MaskBits = 1u128 << (HL_MAX_COINS * CHANNELS_PER_COIN + 1);

/// Mask bit for `(coin_idx, per-coin channel)`. Debug-asserts the
/// channel is per-coin and the index in range.
#[inline]
pub fn bit_of(coin_idx: usize, channel: HlChannel) -> MaskBits {
    debug_assert!(coin_idx < HL_MAX_COINS);
    debug_assert!(channel.per_coin());
    1u128 << (coin_idx * CHANNELS_PER_COIN + channel as usize)
}

/// Expected-ack mask for a configured table: bbo + l2Book + trades
/// per coin, activeAssetCtx per perp coin ([`coin_wants_asset_ctx`]),
/// plus the two global channels.
pub fn expected_mask(coins: &HlCoinTable) -> MaskBits {
    let mut m: MaskBits = ALL_MIDS_BIT | OUTCOME_META_BIT;
    let mut i = 0;
    while let Some((coin, _sym)) = coins.get(i) {
        m |= bit_of(i, HlChannel::Bbo);
        m |= bit_of(i, HlChannel::L2Book);
        m |= bit_of(i, HlChannel::Trades);
        if coin_wants_asset_ctx(coin) {
            m |= bit_of(i, HlChannel::ActiveAssetCtx);
        }
        i += 1;
    }
    m
}

// ---------------------------------------------------------------
// Staleness monitor (§6.2 row: Hyperliquid)
// ---------------------------------------------------------------

/// Default staleness budget: 2 s = 2× block cadence (plan §4.3).
pub const HL_STALENESS_BUDGET_NS: u64 = 2_000_000_000;

/// Per-coin staleness monitor over `l2Book` snapshots. Stateless
/// snapshots have no chain — the only integrity signal is *the
/// venue's clock advancing per coin*. Armed once all subscriptions
/// verify; a coin is stale when no snapshot with a **strictly
/// greater** venue time has arrived within the budget (local
/// monotonic clock), which catches both silent sub death and frozen
/// block production.
pub struct HlStaleness {
    budget_ns: u64,
    n: usize,
    armed: bool,
    last_venue_ts_ns: [u64; HL_MAX_COINS],
    last_advance_ns: [u64; HL_MAX_COINS],
}

impl HlStaleness {
    /// New, disarmed monitor with the given budget.
    pub const fn new(budget_ns: u64) -> Self {
        Self {
            budget_ns,
            n: 0,
            armed: false,
            last_venue_ts_ns: [0; HL_MAX_COINS],
            last_advance_ns: [0; HL_MAX_COINS],
        }
    }

    /// Arm for `n_coins` coins with `now_ns` as every coin's
    /// baseline (all subscriptions just verified).
    pub fn arm(&mut self, now_ns: u64, n_coins: usize) {
        debug_assert!(n_coins <= HL_MAX_COINS);
        self.n = n_coins.min(HL_MAX_COINS);
        self.armed = true;
        let mut i = 0;
        while i < self.n {
            self.last_venue_ts_ns[i] = 0;
            self.last_advance_ns[i] = now_ns;
            i += 1;
        }
    }

    /// Disarm (reconnect teardown).
    pub fn disarm(&mut self) {
        self.armed = false;
        self.n = 0;
    }

    /// Whether the monitor is armed.
    #[inline]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Record one `l2Book` snapshot for `coin_idx`. Only a strictly
    /// advancing venue time refreshes the deadline.
    #[inline]
    pub fn on_l2book(&mut self, coin_idx: usize, venue_ts_ns: u64, now_ns: u64) {
        if coin_idx >= self.n {
            debug_assert!(coin_idx < HL_MAX_COINS, "coin_idx out of table range");
            return;
        }
        if venue_ts_ns > self.last_venue_ts_ns[coin_idx] {
            self.last_venue_ts_ns[coin_idx] = venue_ts_ns;
            self.last_advance_ns[coin_idx] = now_ns;
        }
    }

    /// First stale coin index, if any coin's deadline has passed.
    #[inline]
    pub fn first_stale(&self, now_ns: u64) -> Option<usize> {
        if !self.armed {
            return None;
        }
        let mut i = 0;
        while i < self.n {
            if now_ns.saturating_sub(self.last_advance_ns[i]) > self.budget_ns {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

// ---------------------------------------------------------------
// Subscribe writer + SubId derivation
// ---------------------------------------------------------------

#[inline]
fn push_bytes(dst: &mut [u8], at: usize, src: &[u8]) -> Option<usize> {
    let end = at.checked_add(src.len())?;
    dst.get_mut(at..end)?.copy_from_slice(src);
    Some(end)
}

/// Serialize one `{"method":"subscribe","subscription":{...}}` frame
/// into `dst`. Hyperliquid takes **one subscription per message** —
/// there is no batch form; the run loop queues one frame per
/// configured pair (well inside the 2000 client msgs/min budget).
/// Returns the byte length, `None` if `dst` is too small or a
/// per-coin channel is missing its coin.
#[inline]
pub fn write_subscribe(dst: &mut [u8], channel: HlChannel, coin: Option<&[u8]>) -> Option<usize> {
    if channel.per_coin() != coin.is_some() {
        return None;
    }
    let mut n = 0;
    n = push_bytes(dst, n, b"{\"method\":\"subscribe\",\"subscription\":{\"type\":\"")?;
    n = push_bytes(dst, n, channel.wire_name())?;
    if let Some(c) = coin {
        n = push_bytes(dst, n, b"\",\"coin\":\"")?;
        n = push_bytes(dst, n, c)?;
    }
    n = push_bytes(dst, n, b"\"}}")?;
    Some(n)
}

/// FNV-1a 64-bit over the channel tag byte + coin bytes — a stable
/// [`SubId`] for the `core_net::SubTable`. Global channels hash the
/// empty coin. Never returns `SubId::NONE`.
#[inline]
pub fn sub_id_of(channel: HlChannel, coin: &[u8]) -> SubId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    h ^= channel as u64;
    h = h.wrapping_mul(FNV_PRIME);
    let mut i = 0;
    while i < coin.len() {
        h ^= coin[i] as u64;
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

    const BBO: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    const BBO_ONE_SIDED: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398624,"bbo":[null,{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    const L2BOOK: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"19900.0","sz":"1.0","n":1},{"px":"19899.0","sz":"2.5","n":2}],[{"px":"20100.0","sz":"1.0","n":1}]]}}"#;
    const TRADES: &[u8] = br#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"19900.5","sz":"0.5","hash":"0xabc","time":1677700000000,"tid":118906512037719,"users":["0x1","0x2"]}]}"#;
    const CTX: &[u8] = br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"dayNtlVlm":"1169046.29406","funding":"0.0000125","impactPxs":["14.3047","14.3444"],"markPx":"14.3161","midPx":"14.314","openInterest":"688.11","oraclePx":"14.32","premium":"0.00031774","prevDayPx":"14.155"}}}"#;
    const ALLMIDS: &[u8] =
        br#"{"channel":"allMids","data":{"mids":{"BTC":"29792.0","ETH":"1891.4","SOL":"25.1"}}}"#;
    const SUBRESP_BBO: &[u8] = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#;
    const SUBRESP_ALLMIDS: &[u8] = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"allMids"}}}"#;
    const ERR: &[u8] = br#"{"channel":"error","data":"Already subscribed: {\"type\":\"bbo\",\"coin\":\"BTC\"}"}"#;
    const PONG: &[u8] = br#"{"channel":"pong"}"#;
    const OUTCOME: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"kind":"outcomeCreated","coin":"#330","time":1723600000000}]}"##;

    // ---- classify -------------------------------------------------

    #[test]
    fn classify_recognizes_every_kind() {
        assert_eq!(classify(PONG), HlMsgKind::Pong);
        assert_eq!(classify(SUBRESP_BBO), HlMsgKind::SubResponse);
        assert_eq!(classify(ERR), HlMsgKind::Error);
        assert_eq!(classify(BBO), HlMsgKind::Data(HlChannel::Bbo));
        assert_eq!(classify(L2BOOK), HlMsgKind::Data(HlChannel::L2Book));
        assert_eq!(classify(TRADES), HlMsgKind::Data(HlChannel::Trades));
        assert_eq!(classify(CTX), HlMsgKind::Data(HlChannel::ActiveAssetCtx));
        assert_eq!(classify(ALLMIDS), HlMsgKind::Data(HlChannel::AllMids));
        assert_eq!(classify(OUTCOME), HlMsgKind::Data(HlChannel::OutcomeMetaUpdates));
        assert_eq!(classify(b"{\"nonsense\":true}"), HlMsgKind::Unknown);
    }

    #[test]
    fn classify_does_not_alias_spot_asset_ctx() {
        let spot = br#"{"channel":"activeSpotAssetCtx","data":{"coin":"@1","ctx":{}}}"#;
        assert_eq!(classify(spot), HlMsgKind::Unknown);
    }

    // ---- extract_coin / gating -----------------------------------

    #[test]
    fn extract_coin_plain_and_hip4() {
        assert_eq!(extract_coin(BBO), Some(&b"BTC"[..]));
        let hip4 = br##"{"channel":"bbo","data":{"coin":"#330","time":1,"bbo":[null,null]}}"##;
        assert_eq!(extract_coin(hip4), Some(&b"#330"[..]));
        assert_eq!(extract_coin(b"{\"channel\":\"pong\"}"), None);
    }

    #[test]
    fn asset_ctx_gating_skips_outcome_and_spot_coins() {
        assert!(coin_wants_asset_ctx(b"BTC"));
        assert!(coin_wants_asset_ctx(b"test:ABC"), "HIP-3 dex coins are perps");
        assert!(!coin_wants_asset_ctx(b"#330"));
        assert!(!coin_wants_asset_ctx(b"@1"));
    }

    // ---- parse_bbo ------------------------------------------------

    #[test]
    fn parse_bbo_extracts_both_sides() {
        let f = parse_bbo(BBO, 7).unwrap();
        assert_eq!(f.sym, 7);
        assert_eq!(f.bid_px_1e6, 64_437_000_000);
        assert_eq!(f.bid_qty_1e6, 1_449_100);
        assert_eq!(f.ask_px_1e6, 64_438_000_000);
        assert_eq!(f.ask_qty_1e6, 541_000);
        assert_eq!(f.ts_ns, 1_708_622_398_623 * 1_000_000);
    }

    #[test]
    fn parse_bbo_null_side_yields_zeroes() {
        let f = parse_bbo(BBO_ONE_SIDED, 1).unwrap();
        assert_eq!(f.bid_px_1e6, 0);
        assert_eq!(f.bid_qty_1e6, 0);
        assert_eq!(f.ask_px_1e6, 64_438_000_000);
    }

    #[test]
    fn parse_bbo_rejects_missing_time_and_double_null() {
        let no_time = br#"{"bbo":[{"px":"1.0","sz":"1.0","n":1},{"px":"2.0","sz":"1.0","n":1}]}"#;
        assert!(parse_bbo(no_time, 0).is_none());
        let both_null = br#"{"time":1000,"bbo":[null,null]}"#;
        assert!(parse_bbo(both_null, 0).is_none());
    }

    // ---- parse_l2book_header -------------------------------------

    #[test]
    fn parse_l2book_counts_levels_and_lifts_touch() {
        let f = parse_l2book_header(L2BOOK, 3).unwrap();
        assert_eq!(f.sym, 3);
        assert_eq!(f.n_bids, 2);
        assert_eq!(f.n_asks, 1);
        assert_eq!(f.best_bid_px_1e6, 19_900_000_000);
        assert_eq!(f.best_ask_px_1e6, 20_100_000_000);
        assert_eq!(f.ts_ns, 1_677_700_000_000 * 1_000_000);
    }

    #[test]
    fn parse_l2book_empty_side_and_rejects() {
        let empty_asks = br#"{"time":1000,"levels":[[{"px":"1.0","sz":"1.0","n":1}],[]]}"#;
        let f = parse_l2book_header(empty_asks, 0).unwrap();
        assert_eq!(f.n_bids, 1);
        assert_eq!(f.n_asks, 0);
        assert_eq!(f.best_ask_px_1e6, 0);
        assert!(parse_l2book_header(b"{}", 0).is_none());
        let bad_level = br#"{"time":1000,"levels":[[{"sz":"1.0"}],[]]}"#;
        assert!(parse_l2book_header(bad_level, 0).is_none());
    }

    // ---- parse_trade ---------------------------------------------

    #[test]
    fn parse_trade_extracts_fields() {
        let t = parse_trade(TRADES, 5).unwrap();
        assert_eq!(t.sym, 5);
        assert_eq!(t.tid, 118_906_512_037_719);
        assert_eq!(t.px_1e6, 19_900_500_000);
        assert_eq!(t.qty_1e6, 500_000);
        assert_eq!(t.side, 0);
        assert_eq!(t.ts_ns, 1_677_700_000_000 * 1_000_000);
    }

    #[test]
    fn parse_trade_sell_side_and_missing_side() {
        let sell = br#"{"coin":"X","side":"A","px":"1.0","sz":"1.0","time":1000,"tid":7}"#;
        assert_eq!(parse_trade(sell, 0).unwrap().side, 1);
        let bad = br#"{"coin":"X","px":"1.0","sz":"1.0","time":1000,"tid":7}"#;
        assert!(parse_trade(bad, 0).is_none());
    }

    // ---- parse_active_asset_ctx ----------------------------------

    #[test]
    fn parse_ctx_keeps_1e9_funding_precision() {
        let f = parse_active_asset_ctx(CTX, 9).unwrap();
        assert_eq!(f.sym, 9);
        assert_eq!(f.funding_1e9, 12_500);
        assert_eq!(f.mark_px_1e6, 14_316_100);
        assert_eq!(f.oracle_px_1e6, 14_320_000);
        assert_eq!(f.oi_1e6, 688_110_000);
    }

    #[test]
    fn parse_ctx_negative_funding_and_rejects_missing() {
        let neg = br#"{"ctx":{"funding":"-0.0000125","markPx":"1.0","oraclePx":"1.0","openInterest":"2.0"}}"#;
        assert_eq!(parse_active_asset_ctx(neg, 0).unwrap().funding_1e9, -12_500);
        let missing = br#"{"ctx":{"funding":"0.0000125","markPx":"1.0"}}"#;
        assert!(parse_active_asset_ctx(missing, 0).is_none());
    }

    // ---- parse_all_mids ------------------------------------------

    #[test]
    fn parse_all_mids_counts_entries() {
        assert_eq!(parse_all_mids(ALLMIDS), Some(3));
        assert_eq!(parse_all_mids(br#"{"channel":"allMids","data":{"mids":{}}}"#), Some(0));
        assert_eq!(parse_all_mids(b"{}"), None);
    }

    // ---- parse_outcome_meta --------------------------------------

    #[test]
    fn parse_outcome_meta_kinds_and_enc() {
        let f = parse_outcome_meta(OUTCOME).unwrap();
        assert_eq!(f.kind, OUTCOME_CREATED);
        assert_eq!(f.enc, 330);
        assert_eq!(f.ts_ns, 1_723_600_000_000 * 1_000_000);
        let settled = br#"{"channel":"outcomeMetaUpdates","data":[{"kind":"questionSettled"}]}"#;
        let f = parse_outcome_meta(settled).unwrap();
        assert_eq!(f.kind, QUESTION_SETTLED);
        assert_eq!(f.enc, OUTCOME_ENC_NONE);
        assert_eq!(f.ts_ns, 0);
    }

    #[test]
    fn parse_outcome_meta_rejects_unknown_kind() {
        assert!(parse_outcome_meta(br#"{"channel":"outcomeMetaUpdates","data":[{"kind":"other"}]}"#).is_none());
    }

    // ---- parse_sub_response --------------------------------------

    #[test]
    fn sub_response_roundtrips_channel_and_coin() {
        assert_eq!(
            parse_sub_response(SUBRESP_BBO),
            Some((HlChannel::Bbo, Some(&b"BTC"[..])))
        );
        assert_eq!(parse_sub_response(SUBRESP_ALLMIDS), Some((HlChannel::AllMids, None)));
        let hip4 = br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}}"##;
        assert_eq!(
            parse_sub_response(hip4),
            Some((HlChannel::L2Book, Some(&b"#330"[..])))
        );
    }

    #[test]
    fn sub_response_rejects_unsubscribe_and_unknown_type() {
        let unsub = br#"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#;
        assert!(parse_sub_response(unsub).is_none());
        let unknown = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"candle","coin":"BTC"}}}"#;
        assert!(parse_sub_response(unknown).is_none());
        let missing_coin = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo"}}}"#;
        assert!(parse_sub_response(missing_coin).is_none());
    }

    // ---- coin table ----------------------------------------------

    #[test]
    fn coin_table_roundtrip_including_hip4() {
        let mut t = HlCoinTable::new();
        t.insert(b"BTC", 0x0400_0001).unwrap();
        t.insert(b"#330", 0x0400_0002).unwrap();
        assert_eq!(t.lookup(b"BTC"), Some(0x0400_0001));
        assert_eq!(t.lookup(b"#330"), Some(0x0400_0002));
        assert_eq!(t.lookup(b"ETH"), None);
        assert_eq!(t.index_of(0x0400_0002), Some(1));
        assert_eq!(t.index_of_coin(b"#330"), Some(1));
        assert_eq!(t.index_of_coin(b"DOGE"), None);
        assert_eq!(t.get(0).unwrap().0, b"BTC");
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn coin_table_rejects_bad_input() {
        let mut t = HlCoinTable::new();
        assert_eq!(t.insert(b"", 1), Err(CoinTableErr::Empty));
        assert_eq!(t.insert(&[b'A'; HL_COIN_MAX + 1], 1), Err(CoinTableErr::TooLong));
        let mut i = 0u32;
        while (i as usize) < HL_MAX_COINS {
            t.insert(format!("C{i}").as_bytes(), i).unwrap();
            i += 1;
        }
        assert_eq!(t.insert(b"OVER", 99), Err(CoinTableErr::Full));
    }

    // ---- masks ----------------------------------------------------

    #[test]
    fn expected_mask_gates_asset_ctx_per_coin() {
        let mut t = HlCoinTable::new();
        t.insert(b"BTC", 1).unwrap();
        t.insert(b"#330", 2).unwrap();
        let m = expected_mask(&t);
        assert_ne!(m & bit_of(0, HlChannel::Bbo), 0);
        assert_ne!(m & bit_of(0, HlChannel::L2Book), 0);
        assert_ne!(m & bit_of(0, HlChannel::Trades), 0);
        assert_ne!(m & bit_of(0, HlChannel::ActiveAssetCtx), 0);
        assert_ne!(m & bit_of(1, HlChannel::Bbo), 0);
        assert_eq!(m & bit_of(1, HlChannel::ActiveAssetCtx), 0, "outcome coin: no ctx");
        assert_ne!(m & ALL_MIDS_BIT, 0);
        assert_ne!(m & OUTCOME_META_BIT, 0);
        // Exactly 4 + 3 + 2 bits set.
        assert_eq!(m.count_ones(), 9);
    }

    // ---- staleness monitor ---------------------------------------

    #[test]
    fn staleness_fires_only_when_armed_and_budget_exceeded() {
        let mut s = HlStaleness::new(1_000);
        assert_eq!(s.first_stale(u64::MAX), None, "disarmed never fires");
        s.arm(10_000, 2);
        assert!(s.is_armed());
        assert_eq!(s.first_stale(10_500), None, "inside budget");
        assert_eq!(s.first_stale(11_001), Some(0), "budget exceeded");
        // Coin 0 advances; coin 1 does not.
        s.on_l2book(0, 1_000_000, 11_000);
        assert_eq!(s.first_stale(11_500), Some(1));
        s.on_l2book(1, 1_000_000, 11_400);
        assert_eq!(s.first_stale(11_500), None);
    }

    #[test]
    fn staleness_ignores_non_advancing_venue_time() {
        let mut s = HlStaleness::new(1_000);
        s.arm(0, 1);
        s.on_l2book(0, 5_000, 500);
        // Same venue time again much later — deadline must NOT refresh.
        s.on_l2book(0, 5_000, 900);
        assert_eq!(s.first_stale(1_600), Some(0), "frozen venue clock is stale");
        // Strictly advancing time refreshes.
        s.on_l2book(0, 5_001, 1_550);
        assert_eq!(s.first_stale(1_600), None);
        // Out-of-range index is ignored (debug asserts in range).
        s.on_l2book(HL_MAX_COINS - 1, 1, 1);
        s.disarm();
        assert_eq!(s.first_stale(u64::MAX), None);
    }

    // ---- subscribe writer ----------------------------------------

    #[test]
    fn write_subscribe_exact_bytes() {
        let mut dst = [0u8; 160];
        let n = write_subscribe(&mut dst, HlChannel::Bbo, Some(b"BTC")).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}"# as &[u8]
        );
        let n = write_subscribe(&mut dst, HlChannel::AllMids, None).unwrap();
        assert_eq!(
            &dst[..n],
            br#"{"method":"subscribe","subscription":{"type":"allMids"}}"# as &[u8]
        );
        let n = write_subscribe(&mut dst, HlChannel::L2Book, Some(b"#330")).unwrap();
        assert_eq!(
            &dst[..n],
            br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}"## as &[u8]
        );
    }

    #[test]
    fn write_subscribe_rejects_coin_mismatch_and_tiny_dst() {
        let mut dst = [0u8; 160];
        assert!(write_subscribe(&mut dst, HlChannel::Bbo, None).is_none());
        assert!(write_subscribe(&mut dst, HlChannel::AllMids, Some(b"BTC")).is_none());
        let mut tiny = [0u8; 8];
        assert!(write_subscribe(&mut tiny, HlChannel::Bbo, Some(b"BTC")).is_none());
    }

    // ---- sub ids --------------------------------------------------

    #[test]
    fn sub_ids_are_nonzero_and_distinct() {
        let a = sub_id_of(HlChannel::Bbo, b"BTC");
        let b = sub_id_of(HlChannel::Trades, b"BTC");
        let c = sub_id_of(HlChannel::Bbo, b"#330");
        let d = sub_id_of(HlChannel::AllMids, b"");
        assert_ne!(a.0, 0);
        assert_ne!(a, b, "channel must differentiate");
        assert_ne!(a, c, "coin must differentiate");
        assert_ne!(d.0, 0, "global channels hash the empty coin");
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
            bp in 1u32..999_999u32,
            bq in 0u32..999_999u32,
            ap in 1u32..999_999u32,
            aq in 0u32..999_999u32,
            ts in 1u64..2_000_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(256);
            use std::fmt::Write;
            write!(
                &mut buf,
                r#"{{"channel":"bbo","data":{{"coin":"X","time":{ts},"bbo":[{{"px":"0.{bp:06}","sz":"0.{bq:06}","n":1}},{{"px":"0.{ap:06}","sz":"0.{aq:06}","n":1}}]}}}}"#,
            ).unwrap();
            let f = parse_bbo(buf.as_bytes(), 5).unwrap();
            prop_assert_eq!(f.sym, 5);
            prop_assert_eq!(f.bid_px_1e6, bp as i64);
            prop_assert_eq!(f.bid_qty_1e6, bq as i64);
            prop_assert_eq!(f.ask_px_1e6, ap as i64);
            prop_assert_eq!(f.ask_qty_1e6, aq as i64);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
        }

        #[test]
        fn l2book_level_counts_roundtrip(
            n_bids in 0usize..20,
            n_asks in 0usize..20,
            ts in 1u64..2_000_000_000_000u64,
        ) {
            let mut buf = String::with_capacity(4096);
            use std::fmt::Write;
            write!(&mut buf, r#"{{"channel":"l2Book","data":{{"coin":"X","time":{ts},"levels":[["#).unwrap();
            let mut i = 0;
            while i < n_bids {
                if i > 0 { buf.push(','); }
                write!(&mut buf, r#"{{"px":"{}.0","sz":"1.0","n":1}}"#, 1000 - i).unwrap();
                i += 1;
            }
            buf.push_str("],[");
            let mut i = 0;
            while i < n_asks {
                if i > 0 { buf.push(','); }
                write!(&mut buf, r#"{{"px":"{}.0","sz":"1.0","n":1}}"#, 2000 + i).unwrap();
                i += 1;
            }
            buf.push_str("]]}}");
            let f = parse_l2book_header(buf.as_bytes(), 1).unwrap();
            prop_assert_eq!(f.n_bids as usize, n_bids);
            prop_assert_eq!(f.n_asks as usize, n_asks);
            prop_assert_eq!(f.ts_ns, ts * 1_000_000);
            if n_bids > 0 { prop_assert_eq!(f.best_bid_px_1e6, 1_000_000_000); }
            if n_asks > 0 { prop_assert_eq!(f.best_ask_px_1e6, 2_000_000_000); }
        }

        #[test]
        fn staleness_never_fires_while_venue_clock_advances_in_budget(
            steps in 1u64..50,
            step_ns in 1u64..1_000_000u64,
        ) {
            let budget = step_ns * 2;
            let mut s = HlStaleness::new(budget);
            s.arm(0, 1);
            let mut now = 0u64;
            let mut venue = 0u64;
            let mut k = 0;
            while k < steps {
                now += step_ns;
                venue += 1;
                s.on_l2book(0, venue, now);
                prop_assert_eq!(s.first_stale(now), None);
                k += 1;
            }
            // And once the clock freezes past the budget, it fires.
            prop_assert_eq!(s.first_stale(now + budget + 1), Some(0));
        }

        #[test]
        fn no_parser_panics_on_arbitrary_bytes(
            buf in proptest::collection::vec(any::<u8>(), 0..=400)
        ) {
            let _ = classify(&buf);
            let _ = extract_coin(&buf);
            let _ = parse_bbo(&buf, 0);
            let _ = parse_l2book_header(&buf, 0);
            let _ = parse_trade(&buf, 0);
            let _ = parse_active_asset_ctx(&buf, 0);
            let _ = parse_all_mids(&buf);
            let _ = parse_outcome_meta(&buf);
            let _ = parse_sub_response(&buf);
        }
    }
}
