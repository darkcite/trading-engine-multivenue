// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # ingress-hyperliquid
//!
//! Hyperliquid **public** WebSocket ingress (Phase 8d). Channels per
//! `docs/phase-8-plan.md` §4.3/§4.4 (venue facts verified 2026-08-14):
//!
//! * `bbo {coin}`     — pushed **only on BBO change** → [`core_types::Tick`]
//! * `l2Book {coin}`  — **full snapshot on a venue TIMER, ~5.3 s per
//!   coin (measured on mainnet 2026-09-19: median 5.33 s, p10 5.04,
//!   p90 5.48, the same for perps and outcome legs, ~41 % of pushes
//!   move the outcome touch), ≤ 20 levels/side — no diffs, no seq**;
//!   consumed for capture + integrity (§4.5), and, for HIP-4 outcome
//!   legs, the top-K depth capture (WS10-B, 2026-09-19) and the only
//!   two-sided touch (BIN15 O8: their `bbo` still carries `null` for
//!   the ask — 35 of 35 pushes on 2026-09-19 — while its bid side is
//!   pushed on change within ~0.1 s)
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
//! **10 s** — live-measured push cadence is ~3.3 s per coin, see
//! [`HL_STALENESS_BUDGET_NS`]) or the session is flagged and
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

pub mod discovery;
pub mod family;
pub mod run_loop;

pub use run_loop::{
    drive_one, note_transport_ready, run, Driver, RunResult, State, StopFlag, RX_BUF_SIZE,
    TX_BUF_SIZE,
};

use core_net::SubId;
use core_parse::{find_field, scan_price_1e6, scan_price_1e9, scan_u64, skip_byte, skip_ws};
use core_types::{DepthLevel, DepthTopK, NsTs, SymbolId, VenueId};

// ---------------------------------------------------------------
// Constants
// ---------------------------------------------------------------

/// Longest coin string we accept. Native perps are short (`BTC`);
/// HIP-3 builder-dex coins are `dex:COIN`; HIP-4 outcome coins are
/// `#<enc>` (`enc` ≤ 10 digits); spot pairs are `@<idx>`.
pub const HL_COIN_MAX: usize = 24;

/// Maximum number of configured coins per connection. Fixed-cap
/// tables everywhere; boot fails fast beyond this.
///
/// **16 → 32 (BIN15 O2.)** A rolling HIP-4 family costs TWO rows
/// (its Yes and No legs), and the eight families ruled for BIN15
/// would alone exhaust the old cap before a single perp was
/// configured. 32 × [`CHANNELS_PER_COIN`] = 128 fills [`MaskBits`]
/// exactly, which is why the two venue-global ack bits had to move
/// out of it into [`GlobalBits`].
pub const HL_MAX_COINS: usize = 32;

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

/// Whether `coin` names a HIP-4 outcome leg (`#<enc>`).
///
/// BIN15 O8: these are the coins whose `bbo` the venue publishes
/// ONE-SIDED — see [`parse_l2book_header`]'s note and the run loop's
/// `Bbo`/`L2Book` arms.
#[inline]
#[must_use]
pub fn is_outcome_coin(coin: &[u8]) -> bool {
    matches!(coin.first(), Some(b'#'))
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

impl HlBboFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        ts_ns: 0,
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        sym: 0,
        _pad: [0; 20],
    };
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
    /// BIN15 O8: best bid size ×1e6 (0 = side empty).
    pub best_bid_sz_1e6: i64,
    /// BIN15 O8: best ask size ×1e6 (0 = side empty).
    pub best_ask_sz_1e6: i64,
    // Explicit tail padding.
    _pad: [u8; 16],
}

impl HlL2BookFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        ts_ns: 0,
        best_bid_px_1e6: 0,
        best_ask_px_1e6: 0,
        sym: 0,
        n_bids: 0,
        n_asks: 0,
        best_bid_sz_1e6: 0,
        best_ask_sz_1e6: 0,
        _pad: [0; 16],
    };
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

impl HlTradeFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        tid: 0,
        ts_ns: 0,
        px_1e6: 0,
        qty_1e6: 0,
        sym: 0,
        side: 0,
        _pad: [0; 27],
    };
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
    /// WS3 (gaps §2.5): `premium` ×1e9 (signed — the mark-vs-oracle
    /// basis fraction; same 1e9 scaling as funding). 0 when the wire
    /// omits the field (optional: perp ctxs carry it today; the ctx
    /// shape drifts — pitfall 11).
    pub premium_1e9: i64,
    /// Resolved symbol.
    pub sym: SymbolId,
    // Explicit tail padding.
    _pad: [u8; 20],
}

impl HlAssetCtxFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        funding_1e9: 0,
        mark_px_1e6: 0,
        oracle_px_1e6: 0,
        oi_1e6: 0,
        premium_1e9: 0,
        sym: 0,
        _pad: [0; 20],
    };
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
    /// Outcome encoding — `10 * outcome_id`, i.e. the **Yes** side
    /// (`#<enc>`; the No side is `enc + 1`). Read from the live
    /// shape's `"outcome"` / `"outcomeSettled"` id, falling back to a
    /// legacy `"coin":"#<enc>"`. [`OUTCOME_ENC_NONE`] when the push
    /// names no outcome at all (the `question*` kinds).
    pub enc: u32,
    /// Lifecycle kind: [`OUTCOME_CREATED`] / [`OUTCOME_SETTLED`] /
    /// [`QUESTION_UPDATED`] / [`QUESTION_SETTLED`].
    pub kind: u8,
    // Explicit tail padding.
    _pad: [u8; 51],
}

impl HlOutcomeMetaFrame {
    /// The all-zero frame — the in-place parse's starting slot.
    pub const ZERO: Self = Self {
        ts_ns: 0,
        enc: 0,
        kind: 0,
        _pad: [0; 51],
    };
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
/// Walk one side's `[{px,sz,n},…]` array: returns `(count, best_px,
/// best_sz, end)` and fills `out` with the first `out.len()` levels in
/// venue order (best-first) on the way past — the same walk serves the
/// header (`out` empty) and the WS10-B depth snapshot (`out` = the
/// top-K), so there is one level scanner and not two.
fn scan_side_levels(
    buf: &[u8],
    pos: usize,
    out: &mut [DepthLevel],
) -> Option<(u16, i64, i64, usize)> {
    if *buf.get(pos)? != b'[' {
        return None;
    }
    if *buf.get(pos + 1)? == b']' {
        return Some((0, 0, 0, pos + 2));
    }
    let (best_px, best_sz, mut at) = scan_level_obj(buf, pos + 1)?;
    if let Some(slot) = out.first_mut() {
        *slot = DepthLevel {
            px_1e6: best_px,
            qty_1e6: best_sz,
        };
    }
    let mut n: u16 = 1;
    loop {
        match *buf.get(at)? {
            b',' => {
                let (px, sz, e) = scan_level_obj(buf, at + 1)?;
                if let Some(slot) = out.get_mut(usize::from(n)) {
                    *slot = DepthLevel {
                        px_1e6: px,
                        qty_1e6: sz,
                    };
                }
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
/// Returns `false` on malformed input — caller counts it.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_bbo(payload: &[u8], sym: SymbolId, out: &mut HlBboFrame) -> bool {
    parse_bbo_fill(payload, sym, out).is_some()
}

/// [`parse_bbo`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_bbo_fill(payload: &[u8], sym: SymbolId, out: &mut HlBboFrame) -> Option<()> {
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
    *out = HlBboFrame {
        ts_ns,
        bid_px_1e6,
        bid_qty_1e6,
        ask_px_1e6,
        ask_qty_1e6,
        sym,
        _pad: [0; 20],
    };
    Some(())
}

/// Parse an `l2Book` snapshot into its [`HlL2BookFrame`] header.
/// `levels` is `[bids, asks]`, each best-first; levels themselves
/// stay in the rx buffer (§4.5).
///
/// BIN15 O8: the best price AND SIZE of each side are carried out,
/// because for a HIP-4 outcome leg this snapshot is the only
/// two-sided touch the venue publishes — its `bbo` sends the ask as
/// `null`. Probed live 2026-09-12 on `#27760`: `bbo` gave
/// `[{"px":"0.5",...}, null]` while `l2Book` on the same coin in the
/// same second had six asks, best `0.69`.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_l2book_header(payload: &[u8], sym: SymbolId, out: &mut HlL2BookFrame) -> bool {
    parse_l2book(payload, sym, &mut [], &mut [], out)
}

/// [`parse_l2book_header`] that also lifts the first `bids.len()` /
/// `asks.len()` levels of each side into the caller's slices, in venue
/// (best-first) order; levels beyond the book's real depth are left as
/// the caller set them (`DepthLevel::EMPTY`). The WS10-B depth capture
/// for a HIP-4 outcome leg ([`parse_l2book_depth`]) hands it its
/// carrier's own level arrays — one walk of the snapshot serves the
/// touch, the level counts and the top-K (2026-09-19; the venue pushes
/// `l2Book` on a 5.3 s timer per coin, measured on mainnet, so the
/// snapshot IS the only full view of an outcome book and the capture
/// keeps every change of its top five levels).
///
/// The header is written IN PLACE into `out`, once, after the whole
/// snapshot has parsed; the level slices fill as the walk goes, so a
/// `false` can leave some of them written.
#[inline]
#[must_use = "on `false` the frame was not written"]
fn parse_l2book(
    payload: &[u8],
    sym: SymbolId,
    bids: &mut [DepthLevel],
    asks: &mut [DepthLevel],
    out: &mut HlL2BookFrame,
) -> bool {
    parse_l2book_fill(payload, sym, bids, asks, out).is_some()
}

/// [`parse_l2book`]'s body: `?` short-circuits, and `out` is written once,
/// at the end, only after every field has parsed.
#[inline(always)]
fn parse_l2book_fill(
    payload: &[u8],
    sym: SymbolId,
    bids: &mut [DepthLevel],
    asks: &mut [DepthLevel],
    out: &mut HlL2BookFrame,
) -> Option<()> {
    let pos = find_field(payload, b"\"levels\":")?;
    if *payload.get(pos)? != b'[' {
        return None;
    }
    let (n_bids, best_bid_px_1e6, best_bid_sz_1e6, bids_end) =
        scan_side_levels(payload, pos + 1, bids)?;
    if *payload.get(bids_end)? != b',' {
        return None;
    }
    let (n_asks, best_ask_px_1e6, best_ask_sz_1e6, _asks_end) =
        scan_side_levels(payload, bids_end + 1, asks)?;
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":")?;
    *out = HlL2BookFrame {
        ts_ns,
        best_bid_px_1e6,
        best_ask_px_1e6,
        sym,
        n_bids,
        n_asks,
        best_bid_sz_1e6,
        best_ask_sz_1e6,
        _pad: [0; 16],
    };
    Some(())
}

/// The WS10-B depth snapshot of a HIP-4 outcome leg from one `l2Book`
/// push: the top [`core_types::DEPTH_K`] of each side, best-first, `EMPTY` beyond
/// the book's depth, plus the snapshot's [`HlL2BookFrame`] header from
/// the same walk — an outcome leg needs both, and one walk yields both.
/// `false` on a malformed frame (the caller counts it, exactly as for
/// [`parse_l2book_header`]).
///
/// Parsed IN PLACE (`out` is 192 B; its `Option` was 256 B by value):
/// the top-K levels are lifted straight into `out.bids` / `out.asks`,
/// never staged on the stack. `header` follows the fixed-size frames'
/// rule — written once, untouched on `false`. `out` does NOT: the walk
/// may have filled some levels, so it is never read on `false`.
#[inline]
#[must_use = "on `false` the frame is not a snapshot"]
pub fn parse_l2book_depth(
    payload: &[u8],
    sym: SymbolId,
    now_ns: NsTs,
    out: &mut DepthTopK,
    header: &mut HlL2BookFrame,
) -> bool {
    *out = DepthTopK::EMPTY;
    if !parse_l2book(payload, sym, &mut out.bids, &mut out.asks, header) {
        return false;
    }
    out.ts_ns = now_ns;
    out.venue = VenueId::Hyperliquid as u8;
    out.sym = sym;
    true
}

/// Parse one `trades` row into an [`HlTradeFrame`]. Hyperliquid
/// batches rows per push; the run loop walks rows by re-slicing the
/// payload at successive `"coin":"` markers.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_trade(payload: &[u8], sym: SymbolId, out: &mut HlTradeFrame) -> bool {
    parse_trade_fill(payload, sym, out).is_some()
}

/// [`parse_trade`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_trade_fill(payload: &[u8], sym: SymbolId, out: &mut HlTradeFrame) -> Option<()> {
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
    *out = HlTradeFrame {
        tid,
        ts_ns,
        px_1e6,
        qty_1e6,
        sym,
        side,
        _pad: [0; 27],
    };
    Some(())
}

/// Parse an `activeAssetCtx` push into an [`HlAssetCtxFrame`]. The
/// four original fields are required — a ctx without them is
/// malformed for the perp coins we subscribe. `premium` (WS3) is
/// optional: absent ⇒ 0.
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_active_asset_ctx(payload: &[u8], sym: SymbolId, out: &mut HlAssetCtxFrame) -> bool {
    parse_active_asset_ctx_fill(payload, sym, out).is_some()
}

/// [`parse_active_asset_ctx`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_active_asset_ctx_fill(payload: &[u8], sym: SymbolId, out: &mut HlAssetCtxFrame) -> Option<()> {
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
    // WS3: `premium` — optional, quoted like the other ctx numbers.
    let premium_1e9 = match find_field(payload, b"\"premium\":") {
        Some(pos) => {
            let pos = skip_byte(payload, pos, b'"');
            let (v, _) = scan_price_1e9(payload, pos)?;
            v
        }
        None => 0,
    };
    *out = HlAssetCtxFrame {
        funding_1e9,
        mark_px_1e6,
        oracle_px_1e6,
        oi_1e6,
        premium_1e9,
        sym,
        _pad: [0; 20],
    };
    Some(())
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
///
/// Parsed IN PLACE into `out`: the frame inside an `Option` would cross
/// the call by value, past the 64 B bound. `out` is written once, only
/// after every field has parsed; on `false` it is untouched.
#[inline]
#[must_use = "on `false` the frame was not written"]
pub fn parse_outcome_meta(payload: &[u8], out: &mut HlOutcomeMetaFrame) -> bool {
    parse_outcome_meta_fill(payload, out).is_some()
}

/// [`parse_outcome_meta`]'s body: `?` short-circuits, and `out` is written once, at
/// the end, only after every field has parsed.
#[inline(always)]
fn parse_outcome_meta_fill(payload: &[u8], out: &mut HlOutcomeMetaFrame) -> Option<()> {
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
    let enc = outcome_enc(payload, kind);
    let ts_ns = scan_bare_ms_to_ns(payload, b"\"time\":").unwrap_or(0);
    *out = HlOutcomeMetaFrame {
        ts_ns,
        enc,
        kind,
        _pad: [0; 51],
    };
    Some(())
}

/// The outcome id an `outcomeMetaUpdates` element names.
///
/// The live shape (venue-probed 2026-09-12) carries it as
/// `"outcome":N` inside the `outcomeCreated` object and as the bare
/// integer after `"outcomeSettled":`. The kind-specific key is tried
/// first, then the generic `"outcome"` key — so a future settled push
/// that wraps the id in an object is read correctly too. `"outcome":`
/// cannot false-match `"outcomeCreated":` or `"outcomeMetaUpdates"`:
/// the needle ends in `":` and those keys continue with a letter.
#[inline]
fn outcome_id(payload: &[u8], kind: u8) -> Option<u32> {
    let specific: &[u8] = match kind {
        OUTCOME_SETTLED => b"\"outcomeSettled\":",
        _ => b"\"outcome\":",
    };
    for needle in [specific, b"\"outcome\":".as_slice()] {
        if let Some(pos) = find_field(payload, needle) {
            if let Some((v, _)) = scan_u64(payload, skip_ws(payload, pos)) {
                if let Ok(id) = u32::try_from(v) {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// `enc` for an `outcomeMetaUpdates` push: `10 * id` from the live
/// shape, else the legacy `"coin":"#<enc>"` scan (the pre-2026-09-12
/// fixture shape carries no outcome id at all), else
/// [`OUTCOME_ENC_NONE`].
#[inline]
fn outcome_enc(payload: &[u8], kind: u8) -> u32 {
    if let Some(id) = outcome_id(payload, kind) {
        if let Some(enc) = id.checked_mul(10) {
            debug_assert_ne!(enc, OUTCOME_ENC_NONE);
            return enc;
        }
    }
    let Some(p) = find_field(payload, b"\"coin\":") else {
        return OUTCOME_ENC_NONE;
    };
    let p = skip_byte(payload, p, b'"');
    if payload.get(p) != Some(&b'#') {
        return OUTCOME_ENC_NONE;
    }
    match scan_u64(payload, p + 1) {
        Some((v, _)) if v <= u32::MAX as u64 => v as u32,
        _ => OUTCOME_ENC_NONE,
    }
}

/// The outcome id and the raw `description` VALUE of an
/// `outcomeCreated` push, **borrowed from the rx buffer**.
///
/// Zero copy by design: the returned slice points into `payload`, so
/// the caller must hand it to
/// [`discovery::parse_outcome_spec`](crate::discovery::parse_outcome_spec)
/// — which copies out only the fixed-size fields it needs — before
/// the connection reads over that buffer.
///
/// `None` for every other lifecycle kind, and for a created push
/// whose description is absent, not a string, or unterminated.
#[inline]
#[must_use]
pub fn outcome_meta_description(payload: &[u8]) -> Option<(u32, &[u8])> {
    // Created pushes only — the same shape `parse_sub_response` uses.
    memchr::memmem::find(payload, b"\"outcomeCreated\"")?;
    let id = outcome_id(payload, OUTCOME_CREATED)?;
    let pos = find_field(payload, b"\"description\":")?;
    let q = skip_ws(payload, pos);
    if payload.get(q) != Some(&b'"') {
        return None;
    }
    let start = q + 1;
    let mut i = start;
    while i < payload.len() {
        match payload[i] {
            // A JSON escape consumes the next byte, so an escaped
            // quote cannot terminate the value.
            b'\\' => i += 2,
            b'"' => return Some((id, &payload[start..i])),
            _ => i += 1,
        }
    }
    None
}

/// Parse a `subscriptionResponse` echo: returns the acknowledged
/// channel and (for per-coin channels) the echoed coin bytes.
/// `None` when the echo is not a `subscribe` ack (e.g. unsubscribe)
/// or names no known channel — caller treats that as quiet/reject.
#[inline]
pub fn parse_sub_response(payload: &[u8]) -> Option<(HlChannel, Option<&[u8]>)> {
    memchr::memmem::find(payload, b"\"method\":\"subscribe\"")?;
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
    /// BIN15 O2: [`HlCoinTable::rebind`] named a row that does not
    /// exist. A programming error, not operator input — the family
    /// table hands back the indices [`HlCoinTable::reserve`] returned.
    NoSuchRow,
}

/// Fixed-capacity `coin → SymbolId` map. Linear scan (N ≤ 32).
/// HIP-4 `#<enc>`, spot `@<idx>` and HIP-3 `dex:COIN` strings are
/// ordinary rows — no special surface.
///
/// A row may be **EMPTY** (`len` byte 0) since BIN15 O2: a RESERVED
/// slot whose `SymbolId` is fixed for the life of the process while
/// the venue instrument it names comes and goes. That is what makes
/// a rolling family expressible — the strategy and every capture
/// consumer address a stable sym, and the coin under it is rebound
/// as instances are created and settle.
///
/// Ownership: [`Self::insert`] and [`Self::reserve`] are BOOT-time;
/// [`Self::rebind`] is **ingress-thread only** (it runs inside the
/// `outcomeMetaUpdates` arm). Everything else is a read.
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

    /// Reserve an EMPTY row bound to `sym` and return its index.
    ///
    /// BIN15 O2: the slot exists from boot so that `SymbolId` is
    /// stable, but names no venue instrument until [`Self::rebind`]
    /// writes one. A reserved row is skipped by [`Self::lookup`],
    /// [`expected_mask`] and the run loop's subscribe sweep, so a
    /// dormant family costs nothing on the wire. Boot-time only.
    pub fn reserve(&mut self, sym: SymbolId) -> Result<usize, CoinTableErr> {
        if self.len >= HL_MAX_COINS {
            return Err(CoinTableErr::Full);
        }
        let idx = self.len;
        let row = &mut self.rows[idx];
        row.0 = 0;
        row.1 = [0; HL_COIN_MAX];
        row.2 = sym;
        self.len += 1;
        Ok(idx)
    }

    /// Point row `idx` at `coin`, keeping its `SymbolId`.
    ///
    /// An EMPTY `coin` unbinds the row (the slot stays reserved).
    /// **Ingress-thread only** — this is the roll. Allocation-free.
    pub fn rebind(&mut self, idx: usize, coin: &[u8]) -> Result<(), CoinTableErr> {
        if idx >= self.len {
            return Err(CoinTableErr::NoSuchRow);
        }
        if coin.len() > HL_COIN_MAX {
            return Err(CoinTableErr::TooLong);
        }
        let row = &mut self.rows[idx];
        row.0 = coin.len() as u8;
        row.1 = [0; HL_COIN_MAX];
        let mut k = 0usize;
        while k < coin.len() {
            row.1[k] = coin[k];
            k += 1;
        }
        Ok(())
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
    /// then bytewise compare. EMPTY (reserved) rows never match —
    /// including against an empty needle.
    #[inline]
    pub fn lookup(&self, coin: &[u8]) -> Option<SymbolId> {
        let n = coin.len();
        if n == 0 {
            return None;
        }
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

    /// Row accessor for subscribe building: `(coin, sym)`. The coin
    /// is EMPTY for a reserved row — callers must skip those.
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
        if n == 0 {
            return None;
        }
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

    /// Whether row `idx` NAMES a venue instrument.
    ///
    /// A row that [`Self::reserve`] created for a dormant rolling
    /// family names nothing until [`Self::rebind`] writes one, and is
    /// skipped by [`Self::lookup`], [`expected_mask`] and the run
    /// loop's subscribe sweep for exactly that reason — it is not on
    /// the wire, so nothing about it can ever arrive.
    #[inline]
    #[must_use]
    pub fn is_named(&self, idx: usize) -> bool {
        idx < self.len && self.rows[idx].0 != 0
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

/// Expected/found bitmask for the **per-coin** subscriptions: bit
/// `coin_idx * CHANNELS_PER_COIN + channel`.
///
/// BIN15 O2 raised [`HL_MAX_COINS`] to 32, so 32 × 4 = 128 bits fill
/// a `u128` EXACTLY and the two venue-global channels no longer fit
/// above them. They moved to their own [`GlobalBits`] byte rather
/// than this widening to a hand-rolled 256-bit word: the per-coin
/// mask is read on every subscribe ack, and one machine word is what
/// keeps that free.
pub type MaskBits = u128;

/// Expected/found bitmask for the two venue-GLOBAL subscriptions
/// (`allMids`, `outcomeMetaUpdates`), which resolve no coin. Split
/// out of [`MaskBits`] by BIN15 O2 — see that type's doc.
pub type GlobalBits = u8;

/// Mask bit for the global `allMids` subscription.
pub const ALL_MIDS_BIT: GlobalBits = 1;
/// Mask bit for the global `outcomeMetaUpdates` subscription.
pub const OUTCOME_META_BIT: GlobalBits = 2;

/// Mask bit for `(coin_idx, per-coin channel)`. Debug-asserts the
/// channel is per-coin and the index in range.
#[inline]
pub fn bit_of(coin_idx: usize, channel: HlChannel) -> MaskBits {
    debug_assert!(coin_idx < HL_MAX_COINS);
    debug_assert!(channel.per_coin());
    1u128 << (coin_idx * CHANNELS_PER_COIN + channel as usize)
}

/// Expected-ack masks for a configured table: bbo + l2Book + trades
/// per BOUND coin, activeAssetCtx per perp coin
/// ([`coin_wants_asset_ctx`]), plus the two global channels.
///
/// Returns `(per-coin, global)`. EMPTY (reserved) rows contribute
/// nothing — a dormant family is not waited on, which is what lets a
/// session verify while a family has no live instance.
pub fn expected_mask(coins: &HlCoinTable) -> (MaskBits, GlobalBits) {
    let mut m: MaskBits = 0;
    let g: GlobalBits = ALL_MIDS_BIT | OUTCOME_META_BIT;
    let mut i = 0;
    while let Some((coin, _sym)) = coins.get(i) {
        if coin.is_empty() {
            i += 1;
            continue;
        }
        m |= bit_of(i, HlChannel::Bbo);
        m |= bit_of(i, HlChannel::L2Book);
        m |= bit_of(i, HlChannel::Trades);
        if coin_wants_asset_ctx(coin) {
            m |= bit_of(i, HlChannel::ActiveAssetCtx);
        }
        i += 1;
    }
    (m, g)
}

// ---------------------------------------------------------------
// Staleness monitor (§6.2 row: Hyperliquid)
// ---------------------------------------------------------------

/// Default staleness budget: **10 s**.
///
/// Plan §4.3 assumed "full snapshot every block, ≥ 0.5 s cadence"
/// and budgeted 2× block cadence (2 s). Live probe 2026-08-14
/// (14-sub connection, coins BTC/ETH/SOL): `l2Book` pushes are
/// **timer-paced per subscription at ~1 push / 3.3 s per coin** —
/// uniform across coins regardless of book activity, so the 2 s
/// budget tripped every session by construction. Re-measured
/// 2026-09-19 on mainnet (7 subscriptions, BTC + two outcome legs):
/// **5.33 s median, 4.4–6.0 s range**, again uniform across coins.
/// 10 s is ~2× that period; still fast enough that a dead
/// subscription is caught well inside the venue's own 60 s idle
/// cutoff.
pub const HL_STALENESS_BUDGET_NS: u64 = 10_000_000_000;

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
    /// BIN15 O7: bit `i` set ⇔ row `i` NAMES an instrument and is
    /// therefore judged. See [`Self::arm`].
    watched: u32,
    last_venue_ts_ns: [u64; HL_MAX_COINS],
    last_advance_ns: [u64; HL_MAX_COINS],
}

// `watched` is a u32 bitmask over the coin table.
const _: () = assert!(
    HL_MAX_COINS <= 32,
    "HlStaleness::watched is a u32 — widen it with HL_MAX_COINS"
);

impl HlStaleness {
    /// New, disarmed monitor with the given budget.
    pub const fn new(budget_ns: u64) -> Self {
        Self {
            budget_ns,
            n: 0,
            armed: false,
            watched: 0,
            last_venue_ts_ns: [0; HL_MAX_COINS],
            last_advance_ns: [0; HL_MAX_COINS],
        }
    }

    /// Arm over `coins` with `now_ns` as every coin's baseline (all
    /// subscriptions just verified).
    ///
    /// BIN15 O7 — **only rows that NAME an instrument are judged.**
    /// A row reserved for a dormant rolling family is not subscribed
    /// (`HlCoinTable::reserve`'s own contract), so no `l2Book` can
    /// ever arrive for it and its deadline passes at `arm + budget`
    /// by construction, every time, for as long as the family stays
    /// dormant. Watching one cost the 2026-09-12 BIN15 go-live: the
    /// eight rolling families reserved sixteen slots, three families
    /// had no live instance, and the six empty rows tripped the WHOLE
    /// connection on a **11.5 s metronome** (the 10 s budget plus the
    /// reconnect) — 279 trips in one hour against 1-5 per DAY before
    /// them, cascading into venue-side closes. The monitor exists to
    /// catch a subscription that died, and a row that was never on
    /// the wire cannot have died. A row joins the watch set when
    /// [`Self::reset`] marks it rebound.
    pub fn arm(&mut self, now_ns: u64, coins: &HlCoinTable) {
        let n = coins.len().min(HL_MAX_COINS);
        debug_assert!(coins.len() <= HL_MAX_COINS);
        self.n = n;
        self.armed = true;
        self.watched = 0;
        let mut i = 0;
        while i < n {
            self.last_venue_ts_ns[i] = 0;
            self.last_advance_ns[i] = now_ns;
            if coins.is_named(i) {
                self.watched |= 1u32 << i;
            }
            i += 1;
        }
    }

    /// Re-baseline ONE coin's cadence at `now_ns`.
    ///
    /// BIN15 O2: when a rolling family rebinds a slot, the new
    /// instrument inherits the old one's staleness stamps. The old
    /// coin stopped publishing at its expiry, so without this the
    /// dead instance's silence condemns the fresh one and kills the
    /// session at the first roll. Out-of-range or disarmed is a
    /// no-op.
    pub fn reset(&mut self, coin_idx: usize, now_ns: u64) {
        if !self.armed || coin_idx >= self.n {
            return;
        }
        self.last_venue_ts_ns[coin_idx] = 0;
        self.last_advance_ns[coin_idx] = now_ns;
        // BIN15 O7: a reset follows a rebind, so the row names an
        // instrument now even if it was reserved and empty at `arm`.
        // This is how a family that was dormant at boot starts being
        // judged the moment it goes live.
        self.watched |= 1u32 << coin_idx;
    }

    /// BIN15 O7: stop judging one row without disturbing its stamps.
    ///
    /// For a SETTLED rolling instance. It stops publishing but keeps
    /// its coins bound and subscribed — unsubscribing would open a
    /// window with no subscription for no gain — so from the
    /// monitor's side it is indistinguishable from a dead feed.
    /// Judging it would condemn the session for the whole interval
    /// between settlement and the successor's `outcomeCreated`, and
    /// forever when no successor comes, which is how a family ends.
    /// [`Self::reset`] puts the row back under watch when the
    /// successor binds. Out-of-range or disarmed is a no-op.
    pub fn unwatch(&mut self, coin_idx: usize) {
        if !self.armed || coin_idx >= self.n {
            return;
        }
        self.watched &= !(1u32 << coin_idx);
    }

    /// Disarm (reconnect teardown).
    pub fn disarm(&mut self) {
        self.armed = false;
        self.n = 0;
        self.watched = 0;
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
            // BIN15 O7: an unwatched row names no instrument.
            if self.watched & (1u32 << i) != 0
                && now_ns.saturating_sub(self.last_advance_ns[i]) > self.budget_ns
            {
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
    n = push_bytes(
        dst,
        n,
        b"{\"method\":\"subscribe\",\"subscription\":{\"type\":\"",
    )?;
    n = push_bytes(dst, n, channel.wire_name())?;
    if let Some(c) = coin {
        n = push_bytes(dst, n, b"\",\"coin\":\"")?;
        n = push_bytes(dst, n, c)?;
    }
    n = push_bytes(dst, n, b"\"}}")?;
    Some(n)
}

/// Render `{"method":"unsubscribe","subscription":{…}}` into `dst`.
///
/// BIN15 O2: when a rolling family's instance settles, its two coins
/// must stop consuming a subscription slot on the venue side before
/// the next instance's are opened. The frame is [`write_subscribe`]'s
/// with one verb changed, and the venue's echo is deliberately
/// IGNORED — `parse_sub_response` matches only `"method":"subscribe"`,
/// so an unsubscribe ack cannot disturb the ack mask and none is
/// awaited.
#[inline]
pub fn write_unsubscribe(dst: &mut [u8], channel: HlChannel, coin: Option<&[u8]>) -> Option<usize> {
    if channel.per_coin() != coin.is_some() {
        return None;
    }
    let mut n = 0;
    n = push_bytes(
        dst,
        n,
        b"{\"method\":\"unsubscribe\",\"subscription\":{\"type\":\"",
    )?;
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

/// Test views in the old by-value shape, shared by the unit and the
/// property tests: each wraps one in-place parser and hands back its
/// frame's `Option`, so assertions read naturally. Cold — production
/// callers parse in place.
#[cfg(test)]
mod views {
    // COPY: `Option<HlBboFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_bbo_view(payload: &[u8], sym: core_types::SymbolId) -> Option<crate::HlBboFrame> {
        let mut f = crate::HlBboFrame::ZERO;
        crate::parse_bbo(payload, sym, &mut f).then_some(f)
    }
    // COPY: `Option<HlTradeFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_trade_view(payload: &[u8], sym: core_types::SymbolId) -> Option<crate::HlTradeFrame> {
        let mut f = crate::HlTradeFrame::ZERO;
        crate::parse_trade(payload, sym, &mut f).then_some(f)
    }
    // COPY: `Option<HlAssetCtxFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_active_asset_ctx_view(payload: &[u8], sym: core_types::SymbolId) -> Option<crate::HlAssetCtxFrame> {
        let mut f = crate::HlAssetCtxFrame::ZERO;
        crate::parse_active_asset_ctx(payload, sym, &mut f).then_some(f)
    }
    // COPY: `Option<HlOutcomeMetaFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_outcome_meta_view(payload: &[u8]) -> Option<crate::HlOutcomeMetaFrame> {
        let mut f = crate::HlOutcomeMetaFrame::ZERO;
        crate::parse_outcome_meta(payload, &mut f).then_some(f)
    }
    // COPY: `Option<HlL2BookFrame>` 128 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_l2book_header_view(payload: &[u8], sym: core_types::SymbolId) -> Option<crate::HlL2BookFrame> {
        let mut f = crate::HlL2BookFrame::ZERO;
        crate::parse_l2book_header(payload, sym, &mut f).then_some(f)
    }
    // COPY: `Option<DepthTopK>` 256 B by value — a cold test
    // view, so assertions read as `Option` — rejected: a scratch
    // frame and a `bool` check at every assertion site.
    pub(super) fn parse_l2book_depth_view(
        payload: &[u8],
        sym: core_types::SymbolId,
        now_ns: core_types::NsTs,
    ) -> Option<core_types::DepthTopK> {
        let mut d = core_types::DepthTopK::EMPTY;
        let mut header = crate::HlL2BookFrame::ZERO;
        crate::parse_l2book_depth(payload, sym, now_ns, &mut d, &mut header).then_some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::DEPTH_K;
    use super::views::*;

    const BBO: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    const BBO_ONE_SIDED: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398624,"bbo":[null,{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    const L2BOOK: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"19900.0","sz":"1.0","n":1},{"px":"19899.0","sz":"2.5","n":2}],[{"px":"20100.0","sz":"1.0","n":1}]]}}"#;
    const TRADES: &[u8] = br#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"19900.5","sz":"0.5","hash":"0xabc","time":1677700000000,"tid":118906512037719,"users":["0x1","0x2"]}]}"#;
    const CTX: &[u8] = br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"dayNtlVlm":"1169046.29406","funding":"0.0000125","impactPxs":["14.3047","14.3444"],"markPx":"14.3161","midPx":"14.314","openInterest":"688.11","oraclePx":"14.32","premium":"0.00031774","prevDayPx":"14.155"}}}"#;
    const ALLMIDS: &[u8] =
        br#"{"channel":"allMids","data":{"mids":{"BTC":"29792.0","ETH":"1891.4","SOL":"25.1"}}}"#;
    const SUBRESP_BBO: &[u8] = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#;
    const SUBRESP_ALLMIDS: &[u8] = br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"allMids"}}}"#;
    const ERR: &[u8] =
        br#"{"channel":"error","data":"Already subscribed: {\"type\":\"bbo\",\"coin\":\"BTC\"}"}"#;
    const PONG: &[u8] = br#"{"channel":"pong"}"#;
    const OUTCOME: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"kind":"outcomeCreated","coin":"#330","time":1723600000000}]}"##;
    /// The LIVE `outcomeMetaUpdates` shapes, captured verbatim from
    /// the venue 2026-09-12: `data` is an array of
    /// `{"outcomeCreated":{…}}` / `{"outcomeSettled":<id>}` elements
    /// with NO `coin` key and NO top-level `time`.
    const OUTCOME_LIVE_CREATED: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeCreated":{"outcome":2649,"name":"template:binaryPrice","description":"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630","sideSpecs":[{"name":"template:Yes"},{"name":"template:No"}],"quoteToken":"USDC","venue":"out","deployerFeeScale":"1.0"}}]}"##;
    const OUTCOME_LIVE_SETTLED: &[u8] =
        br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeSettled":2638}]}"##;

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
        assert_eq!(
            classify(OUTCOME),
            HlMsgKind::Data(HlChannel::OutcomeMetaUpdates)
        );
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
        assert!(
            coin_wants_asset_ctx(b"test:ABC"),
            "HIP-3 dex coins are perps"
        );
        assert!(!coin_wants_asset_ctx(b"#330"));
        assert!(!coin_wants_asset_ctx(b"@1"));
    }

    // ---- parse_bbo ------------------------------------------------

    #[test]
    fn parse_bbo_extracts_both_sides() {
        let f = parse_bbo_view(BBO, 7).unwrap();
        assert_eq!(f.sym, 7);
        assert_eq!(f.bid_px_1e6, 64_437_000_000);
        assert_eq!(f.bid_qty_1e6, 1_449_100);
        assert_eq!(f.ask_px_1e6, 64_438_000_000);
        assert_eq!(f.ask_qty_1e6, 541_000);
        assert_eq!(f.ts_ns, 1_708_622_398_623 * 1_000_000);
    }

    #[test]
    fn parse_bbo_null_side_yields_zeroes() {
        let f = parse_bbo_view(BBO_ONE_SIDED, 1).unwrap();
        assert_eq!(f.bid_px_1e6, 0);
        assert_eq!(f.bid_qty_1e6, 0);
        assert_eq!(f.ask_px_1e6, 64_438_000_000);
    }

    #[test]
    fn parse_bbo_rejects_missing_time_and_double_null() {
        let no_time = br#"{"bbo":[{"px":"1.0","sz":"1.0","n":1},{"px":"2.0","sz":"1.0","n":1}]}"#;
        assert!(parse_bbo_view(no_time, 0).is_none());
        let both_null = br#"{"time":1000,"bbo":[null,null]}"#;
        assert!(parse_bbo_view(both_null, 0).is_none());
    }

    // ---- parse_l2book_header -------------------------------------

    #[test]
    fn parse_l2book_counts_levels_and_lifts_touch() {
        let f = parse_l2book_header_view(L2BOOK, 3).unwrap();
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
        let f = parse_l2book_header_view(empty_asks, 0).unwrap();
        assert_eq!(f.n_bids, 1);
        assert_eq!(f.n_asks, 0);
        assert_eq!(f.best_ask_px_1e6, 0);
        assert!(parse_l2book_header_view(b"{}", 0).is_none());
        let bad_level = br#"{"time":1000,"levels":[[{"sz":"1.0"}],[]]}"#;
        assert!(parse_l2book_header_view(bad_level, 0).is_none());
    }

    // ---- parse_trade ---------------------------------------------

    #[test]
    fn parse_trade_extracts_fields() {
        let t = parse_trade_view(TRADES, 5).unwrap();
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
        assert_eq!(parse_trade_view(sell, 0).unwrap().side, 1);
        let bad = br#"{"coin":"X","px":"1.0","sz":"1.0","time":1000,"tid":7}"#;
        assert!(parse_trade_view(bad, 0).is_none());
    }

    // ---- parse_active_asset_ctx ----------------------------------

    #[test]
    fn parse_ctx_keeps_1e9_funding_precision() {
        let f = parse_active_asset_ctx_view(CTX, 9).unwrap();
        assert_eq!(f.sym, 9);
        assert_eq!(f.funding_1e9, 12_500);
        assert_eq!(f.mark_px_1e6, 14_316_100);
        assert_eq!(f.oracle_px_1e6, 14_320_000);
        assert_eq!(f.oi_1e6, 688_110_000);
        // WS3: `premium` ×1e9 ("0.00031774" — the fixture value).
        assert_eq!(f.premium_1e9, 317_740);
    }

    #[test]
    fn parse_ctx_negative_funding_and_rejects_missing() {
        let neg = br#"{"ctx":{"funding":"-0.0000125","markPx":"1.0","oraclePx":"1.0","openInterest":"2.0"}}"#;
        let f = parse_active_asset_ctx_view(neg, 0).unwrap();
        assert_eq!(f.funding_1e9, -12_500);
        assert_eq!(f.premium_1e9, 0, "absent premium parses as 0 (optional)");
        let missing = br#"{"ctx":{"funding":"0.0000125","markPx":"1.0"}}"#;
        assert!(parse_active_asset_ctx_view(missing, 0).is_none());
    }

    #[test]
    fn parse_ctx_negative_premium() {
        // WS3: a discount (mark below oracle) is a signed premium.
        let neg = br#"{"ctx":{"funding":"0.0000125","markPx":"1.0","oraclePx":"1.0","openInterest":"2.0","premium":"-0.00031774"}}"#;
        assert_eq!(
            parse_active_asset_ctx_view(neg, 0).unwrap().premium_1e9,
            -317_740
        );
    }

    // ---- parse_all_mids ------------------------------------------

    #[test]
    fn parse_all_mids_counts_entries() {
        assert_eq!(parse_all_mids(ALLMIDS), Some(3));
        assert_eq!(
            parse_all_mids(br#"{"channel":"allMids","data":{"mids":{}}}"#),
            Some(0)
        );
        assert_eq!(parse_all_mids(b"{}"), None);
    }

    // ---- parse_outcome_meta --------------------------------------

    #[test]
    fn parse_outcome_meta_kinds_and_enc() {
        let f = parse_outcome_meta_view(OUTCOME).unwrap();
        assert_eq!(f.kind, OUTCOME_CREATED);
        assert_eq!(f.enc, 330);
        assert_eq!(f.ts_ns, 1_723_600_000_000 * 1_000_000);
        let settled = br#"{"channel":"outcomeMetaUpdates","data":[{"kind":"questionSettled"}]}"#;
        let f = parse_outcome_meta_view(settled).unwrap();
        assert_eq!(f.kind, QUESTION_SETTLED);
        assert_eq!(f.enc, OUTCOME_ENC_NONE);
        assert_eq!(f.ts_ns, 0);
    }

    #[test]
    fn parse_outcome_meta_rejects_unknown_kind() {
        assert!(parse_outcome_meta_view(
            br#"{"channel":"outcomeMetaUpdates","data":[{"kind":"other"}]}"#
        )
        .is_none());
    }

    #[test]
    fn parse_outcome_meta_reads_the_live_shape() {
        // The live created push: the id lives INSIDE the created
        // object and `enc` is its Yes side, `10 * 2649`.
        let f = parse_outcome_meta_view(OUTCOME_LIVE_CREATED).unwrap();
        assert_eq!(f.kind, OUTCOME_CREATED);
        assert_eq!(f.enc, 26_490);
        // The live shape carries no top-level `time`.
        assert_eq!(f.ts_ns, 0);

        let f = parse_outcome_meta_view(OUTCOME_LIVE_SETTLED).unwrap();
        assert_eq!(f.kind, OUTCOME_SETTLED);
        assert_eq!(f.enc, 26_380);
        assert_eq!(f.ts_ns, 0);

        // A settled push that wraps the id in an object still reads.
        let f = parse_outcome_meta_view(
            br#"{"channel":"outcomeMetaUpdates","data":[{"outcomeSettled":{"outcome":2638}}]}"#,
        )
        .unwrap();
        assert_eq!(f.enc, 26_380);
    }

    #[test]
    fn outcome_meta_description_is_borrowed_and_created_only() {
        let (id, desc) = outcome_meta_description(OUTCOME_LIVE_CREATED).unwrap();
        assert_eq!(id, 2649);
        assert_eq!(
            desc,
            b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630"
        );
        // Borrowed, not copied: the slice points into the payload.
        let base = OUTCOME_LIVE_CREATED.as_ptr() as usize;
        let at = desc.as_ptr() as usize;
        assert!(at > base && at < base + OUTCOME_LIVE_CREATED.len());

        // The grammar parser consumes exactly these bytes.
        let spec = crate::discovery::parse_outcome_spec(id, desc);
        assert_eq!(spec.grammar, crate::discovery::HlOutcomeGrammar::OutBinaryPrice);
        assert_eq!(spec.strike_1e6, 77_177_000_000);
        assert_eq!(spec.twap_s, 60);

        // Every other kind, and the legacy shape (no description).
        assert!(outcome_meta_description(OUTCOME_LIVE_SETTLED).is_none());
        assert!(outcome_meta_description(OUTCOME).is_none());
        // Unterminated / non-string descriptions are refused.
        assert!(outcome_meta_description(
            br#"{"data":[{"outcomeCreated":{"outcome":1,"description":"unterminated}}]}"#
        )
        .is_none());
        assert!(outcome_meta_description(
            br#"{"data":[{"outcomeCreated":{"outcome":1,"description":null}}]}"#
        )
        .is_none());
    }

    // ---- parse_sub_response --------------------------------------

    #[test]
    fn sub_response_roundtrips_channel_and_coin() {
        assert_eq!(
            parse_sub_response(SUBRESP_BBO),
            Some((HlChannel::Bbo, Some(&b"BTC"[..])))
        );
        assert_eq!(
            parse_sub_response(SUBRESP_ALLMIDS),
            Some((HlChannel::AllMids, None))
        );
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
        assert_eq!(
            t.insert(&[b'A'; HL_COIN_MAX + 1], 1),
            Err(CoinTableErr::TooLong)
        );
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
        let (m, g) = expected_mask(&t);
        assert_ne!(m & bit_of(0, HlChannel::Bbo), 0);
        assert_ne!(m & bit_of(0, HlChannel::L2Book), 0);
        assert_ne!(m & bit_of(0, HlChannel::Trades), 0);
        assert_ne!(m & bit_of(0, HlChannel::ActiveAssetCtx), 0);
        assert_ne!(m & bit_of(1, HlChannel::Bbo), 0);
        assert_eq!(
            m & bit_of(1, HlChannel::ActiveAssetCtx),
            0,
            "outcome coin: no ctx"
        );
        assert_ne!(g & ALL_MIDS_BIT, 0);
        assert_ne!(g & OUTCOME_META_BIT, 0);
        // Exactly 4 + 3 per-coin bits; the two globals are their own
        // byte since BIN15 O2.
        assert_eq!(m.count_ones(), 7);
        assert_eq!(g.count_ones(), 2);
    }

    #[test]
    fn thirty_two_coins_times_four_channels_fill_the_mask_exactly() {
        // The reason the global bits had to move: bit_of(31, Trades)
        // is the TOP bit of the u128, so nothing else fits in it.
        assert_eq!(HL_MAX_COINS * CHANNELS_PER_COIN, 128);
        assert_eq!(
            bit_of(HL_MAX_COINS - 1, HlChannel::ActiveAssetCtx),
            1u128 << 127
        );
        assert_eq!(bit_of(0, HlChannel::Bbo), 1u128);
        // And the globals are disjoint one-hot bits of their own byte.
        assert_eq!(ALL_MIDS_BIT & OUTCOME_META_BIT, 0);
        assert_eq!((ALL_MIDS_BIT | OUTCOME_META_BIT).count_ones(), 2);
    }

    // ---- reserved slots + rebinding (BIN15 O2) --------------------

    #[test]
    fn a_reserved_row_is_invisible_until_it_is_rebound() {
        let mut t = HlCoinTable::new();
        t.insert(b"BTC", 7).unwrap();
        let slot = t.reserve(4096).unwrap();
        assert_eq!(slot, 1);
        assert_eq!(t.len(), 2);
        // Reserved: no coin resolves to it, not even the empty one.
        assert_eq!(t.lookup(b""), None);
        assert_eq!(t.index_of_coin(b""), None);
        assert!(t.get(slot).unwrap().0.is_empty());
        assert_eq!(t.get(slot).unwrap().1, 4096);
        // ... and it is not waited on.
        let (m, _g) = expected_mask(&t);
        assert_eq!(m & bit_of(slot, HlChannel::Bbo), 0);
        assert_eq!(m.count_ones(), 4, "BTC's four channels only");

        // Bound: it resolves, keeps its sym, and joins the mask.
        t.rebind(slot, b"#26490").unwrap();
        assert_eq!(t.lookup(b"#26490"), Some(4096));
        assert_eq!(t.index_of_coin(b"#26490"), Some(slot));
        let (m, _g) = expected_mask(&t);
        assert_ne!(m & bit_of(slot, HlChannel::Bbo), 0);
        // An outcome coin takes no activeAssetCtx.
        assert_eq!(m & bit_of(slot, HlChannel::ActiveAssetCtx), 0);
        assert_eq!(m.count_ones(), 7);

        // Rebound again: the OLD coin stops resolving, the sym holds.
        t.rebind(slot, b"#26500").unwrap();
        assert_eq!(t.lookup(b"#26490"), None);
        assert_eq!(t.lookup(b"#26500"), Some(4096));
        // Unbound by an empty coin; the slot survives.
        t.rebind(slot, b"").unwrap();
        assert_eq!(t.lookup(b"#26500"), None);
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(slot).unwrap().1, 4096);
        // Bad input.
        assert_eq!(t.rebind(9, b"BTC"), Err(CoinTableErr::NoSuchRow));
        assert_eq!(
            t.rebind(slot, &[b'A'; HL_COIN_MAX + 1]),
            Err(CoinTableErr::TooLong)
        );
    }

    /// `n` NAMED coins — what the monitor is meant to judge.
    pub(crate) fn named_coins(n: usize) -> HlCoinTable {
        let mut t = HlCoinTable::new();
        let mut i = 0usize;
        while i < n {
            let name = [b'C', b'0' + (i as u8)];
            t.insert(&name, 100 + i as SymbolId).expect("insert");
            i += 1;
        }
        t
    }

    /// BIN15 O7: the go-live regression, as a test.
    ///
    /// A dormant rolling family's row is RESERVED — bound to a
    /// `SymbolId` so the sym is stable, naming no venue instrument,
    /// and deliberately absent from the subscribe sweep. Nothing can
    /// ever arrive for it, so judging it means tripping the whole
    /// connection at `arm + budget` on a metronome. That is what
    /// happened on 2026-09-12: six reserved rows behind three dormant
    /// families trip-looped Hyperliquid every 11.5 s.
    /// BIN15 O8: the header carries each side's best SIZE, not just
    /// its price — an outcome leg's touch is built from it.
    #[test]
    fn l2book_header_carries_both_sides_price_and_size() {
        let p = br##"{"channel":"l2Book","data":{"coin":"#330","time":1789252941096,"levels":[[{"px":"0.5","sz":"64.0","n":1},{"px":"0.49","sz":"64.0","n":1}],[{"px":"0.69","sz":"69.0","n":1}]]}}"##;
        let f = parse_l2book_header_view(p, 7).expect("header");
        assert_eq!(f.best_bid_px_1e6, 500_000);
        assert_eq!(f.best_bid_sz_1e6, 64_000_000);
        assert_eq!(f.best_ask_px_1e6, 690_000);
        assert_eq!(f.best_ask_sz_1e6, 69_000_000);
        assert_eq!(f.n_bids, 2);
        assert_eq!(f.n_asks, 1);
        assert!(is_outcome_coin(b"#330"));
        assert!(!is_outcome_coin(b"BTC"));
        assert!(!is_outcome_coin(b"@1"), "a spot pair is not an outcome leg");
    }

    /// WS10-B for outcome legs (2026-09-19): the same walk lifts the
    /// top-K levels, best-first, `EMPTY` past the book's depth — and
    /// a book deeper than K keeps only the first K.
    #[test]
    fn l2book_depth_lifts_the_top_k_levels_in_venue_order() {
        let p = br##"{"channel":"l2Book","data":{"coin":"#330","time":1789252941096,"levels":[[{"px":"0.5","sz":"64.0","n":1},{"px":"0.49","sz":"64.0","n":1}],[{"px":"0.69","sz":"69.0","n":1}]]}}"##;
        let d = parse_l2book_depth_view(p, 7, 123).expect("depth");
        assert_eq!(d.ts_ns, 123);
        assert_eq!(d.sym, 7);
        assert_eq!(d.venue, VenueId::Hyperliquid as u8);
        assert_eq!(d.k, DEPTH_K as u8);
        assert_eq!(d.bids[0], DepthLevel { px_1e6: 500_000, qty_1e6: 64_000_000 });
        assert_eq!(d.bids[1], DepthLevel { px_1e6: 490_000, qty_1e6: 64_000_000 });
        assert_eq!(d.bids[2], DepthLevel::EMPTY);
        assert_eq!(d.asks[0], DepthLevel { px_1e6: 690_000, qty_1e6: 69_000_000 });
        assert_eq!(d.asks[1], DepthLevel::EMPTY);
        // The header read of the same frame is unchanged by the arrays.
        let f = parse_l2book_header_view(p, 7).expect("header");
        assert_eq!((f.n_bids, f.n_asks, f.best_ask_px_1e6), (2, 1, 690_000));
        // One walk yields both: the depth walk's header is the header walk's.
        let mut snap = DepthTopK::EMPTY;
        let mut header = HlL2BookFrame::ZERO;
        assert!(parse_l2book_depth(p, 7, 123, &mut snap, &mut header));
        assert_eq!(header, f, "the depth walk's header differs from the header walk's");

        // Seven bids, three asks: the top five bids, all three asks.
        let mut buf = String::with_capacity(1024);
        use std::fmt::Write;
        write!(&mut buf, r##"{{"channel":"l2Book","data":{{"coin":"#330","time":1,"levels":[["##).unwrap();
        for i in 0..7 {
            write!(&mut buf, r#"{}{{"px":"0.{}","sz":"{}.0","n":1}}"#, if i == 0 { "" } else { "," }, 90 - i, 10 + i).unwrap();
        }
        write!(&mut buf, r#"],["#).unwrap();
        for i in 0..3 {
            write!(&mut buf, r#"{}{{"px":"0.{}","sz":"{}.0","n":1}}"#, if i == 0 { "" } else { "," }, 91 + i, 20 + i).unwrap();
        }
        write!(&mut buf, r#"]]}}}}"#).unwrap();
        let d = parse_l2book_depth_view(buf.as_bytes(), 7, 1).expect("depth");
        assert_eq!(d.bids[4], DepthLevel { px_1e6: 860_000, qty_1e6: 14_000_000 }, "fifth-best bid");
        assert_eq!(d.asks[2], DepthLevel { px_1e6: 930_000, qty_1e6: 22_000_000 });
        assert_eq!(d.asks[3], DepthLevel::EMPTY);
        let f = parse_l2book_header_view(buf.as_bytes(), 7).expect("header");
        assert_eq!((f.n_bids, f.n_asks), (7, 3), "the count still walks the whole side");
        // A truncated frame is refused, not half-filled into a snapshot.
        assert!(parse_l2book_depth_view(&buf.as_bytes()[..buf.len() - 8], 7, 1).is_none());
    }

    #[test]
    fn a_reserved_row_is_never_stale_until_it_is_rebound() {
        let mut coins = named_coins(1);
        let reserved = coins.reserve(4242).expect("reserve");
        assert!(coins.is_named(0), "the perp names an instrument");
        assert!(!coins.is_named(reserved), "the dormant slot names none");

        let mut s = HlStaleness::new(1_000);
        s.arm(10_000, &coins);
        // Far past the budget with NOTHING delivered: the named coin
        // is stale, the reserved one is not — and must not be, or it
        // alone would condemn the session forever.
        assert_eq!(s.first_stale(99_999), Some(0), "the named coin is judged");
        s.on_l2book(0, 1, 99_999);
        assert_eq!(
            s.first_stale(100_100),
            None,
            "a reserved row must never trip the connection"
        );

        // The family goes live: rebind + reset brings it under watch.
        coins.rebind(reserved, b"@2750").expect("rebind");
        s.reset(reserved, 100_100);
        assert_eq!(s.first_stale(100_500), None, "inside budget");
        // Keep the named coin fresh so the next assertion can only be
        // about the row that just went live.
        s.on_l2book(0, 2, 100_900);
        assert_eq!(
            s.first_stale(101_200),
            Some(reserved),
            "once live it is judged like any other coin"
        );
    }

    #[test]
    fn staleness_reset_rebaselines_one_coin_only() {
        let mut s = HlStaleness::new(1_000);
        s.arm(10_000, &named_coins(2));
        s.on_l2book(0, 1, 10_000);
        s.on_l2book(1, 1, 10_000);
        // Both coins go stale at the same instant.
        assert_eq!(s.first_stale(11_500), Some(0));
        // Re-baselining coin 0 leaves coin 1 stale — the roll must not
        // excuse the coins it did not touch.
        s.reset(0, 11_500);
        assert_eq!(s.first_stale(11_500), Some(1));
        s.reset(1, 11_500);
        assert_eq!(s.first_stale(11_500), None);
        // Out of range and disarmed are no-ops.
        s.reset(99, 12_000);
        let mut d = HlStaleness::new(1_000);
        d.reset(0, 1);
        assert_eq!(d.first_stale(u64::MAX), None);
    }

    #[test]
    fn unsubscribe_is_the_subscribe_frame_with_one_verb_changed() {
        let mut sub_buf = [0u8; 160];
        let mut unsub_buf = [0u8; 160];
        let ns = write_subscribe(&mut sub_buf, HlChannel::Bbo, Some(b"#26490")).unwrap();
        let nu = write_unsubscribe(&mut unsub_buf, HlChannel::Bbo, Some(b"#26490")).unwrap();
        assert_eq!(
            &unsub_buf[..nu],
            br##"{"method":"unsubscribe","subscription":{"type":"bbo","coin":"#26490"}}"##
        );
        // Identical but for the verb.
        let a = core::str::from_utf8(&sub_buf[..ns]).unwrap();
        let b = core::str::from_utf8(&unsub_buf[..nu]).unwrap();
        assert_eq!(a.replace("\"subscribe\"", "\"unsubscribe\""), b);
        // The global form takes no coin, and the arity rule holds.
        let n = write_unsubscribe(&mut unsub_buf, HlChannel::AllMids, None).unwrap();
        assert_eq!(
            &unsub_buf[..n],
            br#"{"method":"unsubscribe","subscription":{"type":"allMids"}}"#
        );
        assert!(write_unsubscribe(&mut unsub_buf, HlChannel::Bbo, None).is_none());
        assert!(write_unsubscribe(&mut unsub_buf, HlChannel::AllMids, Some(b"BTC")).is_none());
        // An unsubscribe echo is NOT an ack (the parser gates on the verb).
        assert!(parse_sub_response(&unsub_buf[..n]).is_none());
    }

    // ---- staleness monitor ---------------------------------------

    #[test]
    fn staleness_fires_only_when_armed_and_budget_exceeded() {
        let mut s = HlStaleness::new(1_000);
        assert_eq!(s.first_stale(u64::MAX), None, "disarmed never fires");
        s.arm(10_000, &named_coins(2));
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
        s.arm(0, &named_coins(1));
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
            br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}"##
                as &[u8]
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
    use super::views::*;

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
            let f = parse_bbo_view(buf.as_bytes(), 5).unwrap();
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
            let f = parse_l2book_header_view(buf.as_bytes(), 1).unwrap();
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
            s.arm(0, &crate::tests::named_coins(1));
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
            let _ = parse_bbo_view(&buf, 0);
            let _ = parse_l2book_header_view(&buf, 0);
            let _ = parse_trade_view(&buf, 0);
            let _ = parse_active_asset_ctx_view(&buf, 0);
            let _ = parse_all_mids(&buf);
            let _ = parse_outcome_meta_view(&buf);
            let _ = parse_sub_response(&buf);
        }
    }
}
