// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-types
//!
//! POD value types shared across the workspace.
//!
//! ## Design rules
//!
//! * Every type exposed here is `#[repr(C)]` + `#[derive(Copy, Clone)]`.
//! * No `String`, no `Vec`, no `Box`, no heap fields anywhere.
//! * Prices are fixed-point integers (`i64` at `1e-6` USDC) to avoid
//!   floating-point rounding in the hot path.
//! * Symbols are `u32` IDs resolved at boot from a perfect-hash table.
//! * Structs that sit at ring-slot boundaries are `#[repr(align(64))]`
//!   to prevent false sharing between the producer and the consumer.
//!
//! The `hot_path` module contains only types that must survive compile-
//! time layout checks; see the `static_assert_layout!` block at the
//! bottom of this file.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

// ---------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------

/// Venue discriminator. One byte in every `Tick` / `Order` and the
/// namespace byte (bits 31..24) of every [`SymbolId`].
///
/// Discriminant values are wire-format-stable (PMLR v2, ring slots):
/// never renumber, only append. `255` is reserved — it is the venue
/// byte of [`SYMBOL_ID_NONE`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VenueId {
    /// Polymarket CLOB.
    Polymarket = 0,
    /// Binance spot WS.
    Binance = 1,
    /// OKX v5 public WS.
    Okx = 2,
    /// Deribit JSON-RPC WS.
    Deribit = 3,
    /// Hyperliquid WS (native perps, spot, HIP-4 outcomes).
    Hyperliquid = 4,
    /// AI-Ingress (claude-worker command feed; no market data).
    Ai = 5,
    /// WS9: Bybit v5 public WS (spot + linear perps).
    Bybit = 6,
}

impl VenueId {
    /// Raw byte value as stored in POD structs and SymbolIds.
    #[inline(always)]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a raw byte (e.g. from a PMLR v2 slot). `None` for
    /// unknown values — readers must treat that as corruption.
    #[inline(always)]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Polymarket),
            1 => Some(Self::Binance),
            2 => Some(Self::Okx),
            3 => Some(Self::Deribit),
            4 => Some(Self::Hyperliquid),
            5 => Some(Self::Ai),
            6 => Some(Self::Bybit),
            _ => None,
        }
    }

    /// VT2 default staleness threshold (ms) for this venue's tick feed
    /// — docs/venue-time-capture-plan.md §2 doctrine 4: the measured
    /// feed-delay p99 rounded up (docs/venue-latency.md 2026-09-03),
    /// with Binance capped at 1 000 ms on purpose (a 1 s-stale BTC book
    /// is unknown). Operator override: `--stale-after-ms <venue>:<ms>`.
    /// `Ai` carries no market data — 0, never judged.
    #[inline(always)]
    pub const fn default_stale_after_ms(self) -> u32 {
        match self {
            Self::Polymarket => 1_000,
            Self::Binance => 1_000,
            Self::Okx => 400,
            Self::Deribit => 600,
            Self::Hyperliquid => 700,
            Self::Ai => 0,
            Self::Bybit => 500,
        }
    }

    /// VT2/VT4: the whole default threshold table indexed by the venue
    /// byte — the ONE table the engine flag parser and the harness
    /// `ModelParams` share.
    pub const fn stale_after_ms_defaults() -> [u32; 7] {
        [
            Self::Polymarket.default_stale_after_ms(),
            Self::Binance.default_stale_after_ms(),
            Self::Okx.default_stale_after_ms(),
            Self::Deribit.default_stale_after_ms(),
            Self::Hyperliquid.default_stale_after_ms(),
            Self::Ai.default_stale_after_ms(),
            Self::Bybit.default_stale_after_ms(),
        ]
    }
}

/// Dense u32 identifier for a trading symbol, namespaced by venue:
/// bits 31..24 = [`VenueId`] byte, bits 23..0 = per-venue ordinal
/// (16.7 M instruments per venue). Ordinals are allocated once at
/// boot from venue REST discovery; never parsed in the hot path.
pub type SymbolId = u32;

/// The sentinel meaning "not a real symbol". Its venue byte is 255,
/// which no [`VenueId`] ever uses, so it can never collide with a
/// real namespaced id.
pub const SYMBOL_ID_NONE: SymbolId = u32::MAX;

/// Bit position of the venue byte inside a [`SymbolId`].
pub const SYMBOL_VENUE_SHIFT: u32 = 24;

/// Mask selecting the per-venue ordinal bits of a [`SymbolId`].
pub const SYMBOL_ORDINAL_MASK: u32 = 0x00FF_FFFF;

/// Compose a namespaced [`SymbolId`] from a venue and a per-venue
/// ordinal. Boot-time only in practice, but zero-cost anyway.
///
/// Debug builds assert the ordinal fits in 24 bits; release builds
/// mask silently (an oversized ordinal is a boot-time config bug the
/// discovery path must reject before it gets here).
#[inline(always)]
pub const fn make_symbol_id(venue: VenueId, ordinal: u32) -> SymbolId {
    debug_assert!(ordinal <= SYMBOL_ORDINAL_MASK);
    ((venue as u32) << SYMBOL_VENUE_SHIFT) | (ordinal & SYMBOL_ORDINAL_MASK)
}

/// Venue byte of a namespaced [`SymbolId`]. For [`SYMBOL_ID_NONE`]
/// this returns 255, which [`VenueId::from_u8`] rejects.
#[inline(always)]
pub const fn symbol_venue_byte(sym: SymbolId) -> u8 {
    (sym >> SYMBOL_VENUE_SHIFT) as u8
}

/// Per-venue ordinal of a namespaced [`SymbolId`].
#[inline(always)]
pub const fn symbol_ordinal(sym: SymbolId) -> u32 {
    sym & SYMBOL_ORDINAL_MASK
}

/// Mix the venue byte into the low bits for staleness bucketing.
/// Callers mask the result down to their bucket count (e.g. `& 63`).
/// Replaces the old `sym & 63` scheme so two venues' ordinal-0
/// symbols do not collide on low bits.
#[inline(always)]
pub const fn symbol_bucket_mix(sym: SymbolId) -> u32 {
    sym ^ (sym >> SYMBOL_VENUE_SHIFT)
}

/// Monotonic nanosecond timestamp produced by `core_time::now_ns`. We
/// deliberately do NOT use `std::time::Instant` in POD types — its
/// representation is platform-private and not `#[repr(C)]`.
pub type NsTs = u64;

// ---------------------------------------------------------------
// Fixed-point price / quantity
// ---------------------------------------------------------------

/// Fixed-point price in units of `1e-6` USDC. A Polymarket contract
/// trades between `0` and `1_000_000` (probability 0..1 scaled by 1e6).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Price(pub i64);

impl Price {
    /// Wrap a raw fixed-point integer.
    #[inline(always)]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    /// Unwrap to the raw fixed-point integer.
    #[inline(always)]
    pub const fn raw(self) -> i64 {
        self.0
    }
}

/// Fixed-point quantity in contract units (1 contract = 1 YES/NO share).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Qty(pub i64);

impl Qty {
    /// Wrap a raw fixed-point integer.
    #[inline(always)]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    /// Unwrap.
    #[inline(always)]
    pub const fn raw(self) -> i64 {
        self.0
    }
}

// ---------------------------------------------------------------
// Enums — `#[repr(u8)]` so they're a single byte in POD structs.
// ---------------------------------------------------------------

/// Bid or ask side.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Side {
    /// Buy / YES-long / bid.
    Bid = 0,
    /// Sell / NO-long / ask.
    Ask = 1,
}

/// How latency-sensitive a Signal is. Determines which strategies
/// react and how much drift we tolerate between `ts_emitted` and
/// `ts_consumed`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LatencyClass {
    /// Microsecond-class (Binance WS, Polymarket WS).
    Hot = 0,
    /// Millisecond-class (RPC event feeds).
    Warm = 1,
    /// Seconds-to-minutes (Claude artifacts, operator-cadence sources).
    Slow = 2,
}

/// Kind of market on Polymarket. Used to route to the right probability
/// model (`strategy-ev`) and to decide whether a news event can move the
/// probability at all (`strategy-rule-tree`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MarketFamily {
    /// BTC/ETH/SOL price-threshold markets. Binance-linked.
    Crypto = 0,
    /// Elections, referenda, appointments.
    Politics = 1,
    /// Match / series / championship outcomes.
    Sports = 2,
    /// Covid/economic/fed / misc macro.
    Macro = 3,
    /// Anything else. Default family.
    Other = 4,
}

// ---------------------------------------------------------------
// Hot-path POD structs
// ---------------------------------------------------------------

/// `Tick::flags` bit 0 (VT1, Tick v3): the producing ingress judged this
/// quote STALE — its venue time trails the connection's least-delayed
/// message by more than the venue's `stale_after_ms`. A stale tick is
/// still captured (it is what the engine saw) but MUST NOT feed a
/// strategy signal, fill a modeled order, or move the last-known-good
/// mark (docs/venue-time-capture-plan.md §2 doctrine 3).
pub const TICK_FLAG_STALE: u8 = 1;
/// `Tick::flags` bit 1 (VT1, Tick v3): `venue_time_ms` (and the
/// staleness judgement) came from the connection's SENTINEL stream
/// (Binance spot `aggTrade` — `bookTicker` carries no venue timestamp),
/// not from this message itself. Research can separate the inferred
/// case from the direct one.
pub const TICK_FLAG_VENUE_TIME_SENTINEL: u8 = 2;

/// Top-of-book tick snapshot. Produced by the Polymarket WS parser,
/// pushed onto the tick ring, consumed by the engine.
///
/// Layout is fixed-size (64 bytes) so it fills one cache line exactly —
/// no false sharing between the ingress thread and the engine thread
/// when they both touch adjacent slots in the ring.
///
/// Tick v3 (VT1, 2026-09-03) spends the v2 tail pad on `flags` (offset
/// 49) and `venue_time_ms` (offset 56, naturally aligned `u64`). Both
/// are zero in every v2 capture — "venue time unknown, never stale" —
/// so v2 files keep replaying under the v2 law unchanged. Venue time
/// is DATA, not a clock: `ts_ns` stays the ordering key everywhere.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct Tick {
    /// When the ingress thread finished parsing the frame.
    pub ts_ns: NsTs,
    /// Resolved symbol ID (not a string).
    pub sym: SymbolId,
    /// Sequence number emitted by the venue.
    pub venue_seq: u32,
    /// Best bid price (fixed-point).
    pub bid_px: Price,
    /// Size at best bid.
    pub bid_qty: Qty,
    /// Best ask price.
    pub ask_px: Price,
    /// Size at best ask.
    pub ask_qty: Qty,
    /// Producing venue ([`VenueId`] as raw byte). Redundant with the
    /// venue byte inside `sym` by construction; carried explicitly so
    /// PMLR consumers and lane audits never need to decode `sym`.
    pub venue: u8,
    /// [`TICK_FLAG_STALE`] | [`TICK_FLAG_VENUE_TIME_SENTINEL`]; 0 in
    /// v2 captures.
    pub flags: u8,
    /// Explicit padding — [`AsBytes`] requires every byte of the 64 B
    /// slot to be initialized. Always zero.
    _pad: [u8; 6],
    /// Venue timestamp of the quote in ms (venue clock); 0 = unknown
    /// (v2 captures, and venues whose stream carries no stamp).
    pub venue_time_ms: u64,
}

impl Tick {
    /// Construct a Tick whose venue time is UNKNOWN (`venue_time_ms = 0`,
    /// `flags = 0` — the v2 shape). Ingress parsers that carry a venue
    /// stamp use [`Tick::new_stamped`]; everything else (tests, replay
    /// fixtures, synthetic ticks) uses this.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        venue: VenueId,
        sym: SymbolId,
        venue_seq: u32,
        bid_px: Price,
        bid_qty: Qty,
        ask_px: Price,
        ask_qty: Qty,
    ) -> Self {
        Self::new_stamped(ts_ns, venue, sym, venue_seq, bid_px, bid_qty, ask_px, ask_qty, 0, 0)
    }

    /// Construct a Tick v3 with its venue time and flags (VT1). The
    /// ingress owns the staleness judgement: it sets
    /// [`TICK_FLAG_STALE`] from its per-connection offset estimator and
    /// [`TICK_FLAG_VENUE_TIME_SENTINEL`] when the stamp was inherited
    /// from the connection's sentinel stream.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new_stamped(
        ts_ns: NsTs,
        venue: VenueId,
        sym: SymbolId,
        venue_seq: u32,
        bid_px: Price,
        bid_qty: Qty,
        ask_px: Price,
        ask_qty: Qty,
        venue_time_ms: u64,
        flags: u8,
    ) -> Self {
        Self {
            ts_ns,
            sym,
            venue_seq,
            bid_px,
            bid_qty,
            ask_px,
            ask_qty,
            venue: venue as u8,
            flags,
            _pad: [0; 6],
            venue_time_ms,
        }
    }

    /// True when the producing ingress flagged this quote stale
    /// (`flags & TICK_FLAG_STALE`). Branch-free callers can use the
    /// mask directly; this is the readable form for consumers.
    #[inline(always)]
    pub const fn is_stale(self) -> bool {
        self.flags & TICK_FLAG_STALE != 0
    }

    /// Mid price (integer arithmetic; rounds toward zero).
    #[inline(always)]
    pub const fn mid(self) -> Price {
        Price((self.bid_px.0 + self.ask_px.0) / 2)
    }

    /// Spread in fixed-point units.
    #[inline(always)]
    pub const fn spread(self) -> i64 {
        self.ask_px.0 - self.bid_px.0
    }
}

/// Options analytics record (M2.3, mvp-plan §4-M2.3/§9.8): mark px,
/// mark IV, greeks, open interest, underlying px — ONE record per
/// venue push, appended to the per-venue `<venue>-opt-summary.pmlr`
/// capture channel. CAPTURE-ONLY: never enters the engine ring; the
/// strategist digest reads it offline.
///
/// Layout is fixed-size (64 bytes = one PMLR slot / cache line).
/// Field conventions (docs/wire-format.md is the pinned law):
///
/// * prices/IV/OI are captured in RAW VENUE UNITS scaled fixed-point
///   (Deribit option mark px is in BTC/ETH; OKX has no mark px in
///   `opt-summary` — see `flags`). IV is a FRACTION ×1e9 (0.6543 →
///   654_300_000; Deribit's percent wire value is normalized /100).
/// * greeks are Black-Scholes-style (Deribit `greeks`, OKX `*BS`
///   fields), i32 fixed-point with SATURATING conversion — delta and
///   gamma ×1e9 (|delta| ≤ 1 exact; gamma > ~2.1 saturates), vega
///   and theta ×1e6 (±2147 units — extreme near-expiry theta
///   saturates; a saturated value equals the type bound, detectable
///   downstream).
/// * `flags` records which OPTIONAL fields the venue supplied.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct OptSummary {
    /// When the ingress thread finished parsing the frame.
    pub ts_ns: NsTs,
    /// Resolved option symbol (base-512 options block).
    pub sym: SymbolId,
    /// Producing venue ([`VenueId`] as raw byte).
    pub venue: u8,
    /// [`OPT_SUMMARY_FLAG_MARK_PX`] | [`OPT_SUMMARY_FLAG_OI`].
    pub flags: u8,
    /// Explicit padding — always zero.
    _pad0: [u8; 2],
    /// Mark price ×1e9, raw venue units (0 when flag absent).
    pub mark_px_1e9: i64,
    /// Mark implied volatility, FRACTION ×1e9.
    pub mark_iv_1e9: i64,
    /// Underlying reference px ×1e9 (Deribit `underlying_price`;
    /// OKX `fwdPx` — the family forward).
    pub underlying_px_1e9: i64,
    /// Open interest ×1e6, raw venue units (0 when flag absent).
    pub open_interest_1e6: i64,
    /// BS delta ×1e9 (|delta| ≤ 1 ⇒ exact).
    pub delta_1e9: i32,
    /// BS gamma ×1e9 (saturating).
    pub gamma_1e9: i32,
    /// BS vega ×1e6 (saturating).
    pub vega_1e6: i32,
    /// BS theta ×1e6 (saturating).
    pub theta_1e6: i32,
}

/// [`OptSummary::flags`] bit: the venue supplied `mark_px_1e9`.
pub const OPT_SUMMARY_FLAG_MARK_PX: u8 = 1;
/// [`OptSummary::flags`] bit: the venue supplied `open_interest_1e6`.
pub const OPT_SUMMARY_FLAG_OI: u8 = 2;

impl OptSummary {
    /// Construct in one shot; saturates the i32 greek conversions.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        venue: VenueId,
        sym: SymbolId,
        flags: u8,
        mark_px_1e9: i64,
        mark_iv_1e9: i64,
        underlying_px_1e9: i64,
        open_interest_1e6: i64,
        delta_1e9: i64,
        gamma_1e9: i64,
        vega_1e6: i64,
        theta_1e6: i64,
    ) -> Self {
        Self {
            ts_ns,
            sym,
            venue: venue as u8,
            flags,
            _pad0: [0; 2],
            mark_px_1e9,
            mark_iv_1e9,
            underlying_px_1e9,
            open_interest_1e6,
            delta_1e9: sat_i32(delta_1e9),
            gamma_1e9: sat_i32(gamma_1e9),
            vega_1e6: sat_i32(vega_1e6),
            theta_1e6: sat_i32(theta_1e6),
        }
    }
}

/// Saturating i64 → i32 (const-friendly; greek fixed-point law).
#[inline(always)]
pub const fn sat_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

/// External signal (Binance price move, mempool tx, ...).
///
/// `payload` is a caller-chosen 32-byte inline blob — typically a
/// fixed-point price delta, or an indexed reference into a preallocated
/// news table. Never a pointer.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct Signal {
    /// When the ingress thread emitted this signal.
    pub ts_ns: NsTs,
    /// Which symbol (or `SYMBOL_ID_NONE` if cross-market).
    pub sym: SymbolId,
    /// Latency budget tag — routes slow-path news differently from hot-path price moves.
    pub class: LatencyClass,
    /// Source identifier (0=binance, 2=rpc, ...; 1 = retired RSS,
    /// reserved). See `SignalSource`.
    pub source: u8,
    /// Reserved.
    _pad0: [u8; 2],
    /// Opaque inline payload (no heap).
    pub payload: [u8; 40],
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad1: [u8; 8],
}

impl Signal {
    /// Construct a Signal without naming the private padding field.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        sym: SymbolId,
        class: LatencyClass,
        source: u8,
        payload: [u8; 40],
    ) -> Self {
        Self {
            ts_ns,
            sym,
            class,
            source,
            _pad0: [0; 2],
            payload,
            _pad1: [0; 8],
        }
    }
}

/// Well-known signal source tags. Stored as a plain `u8` in `Signal`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SignalSource {
    /// Binance WS.
    Binance = 0,
    /// Retired 8f (`ingress-rss` deleted). Wire value reserved —
    /// append-only ABI: never renumbered, never reused.
    Rss = 1,
    /// Polygon RPC event feed (Alchemy / QuickNode).
    Rpc = 2,
    /// Offline Claude worker artifact (topic tags, news label, etc.).
    ClaudeWorker = 3,
    /// Internal heartbeat / liveness ticker.
    Heartbeat = 4,
}

/// A fill report coming back from the CLOB. Pushed onto the fill ring
/// by `clob-dispatcher`, consumed by the engine.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct Fill {
    /// When the dispatcher parsed the fill.
    pub ts_ns: NsTs,
    /// Market filled.
    pub sym: SymbolId,
    /// Bid or ask.
    pub side: Side,
    /// Padding for alignment.
    _pad0: [u8; 3],
    /// Fill price.
    pub px: Price,
    /// Fill quantity.
    pub qty: Qty,
    /// Venue-assigned order ID (u64 for Polymarket CLOB).
    pub order_id: u64,
    /// Reserved.
    _pad1: [u8; 16],
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad2: [u8; 8],
}

impl Fill {
    /// Construct a Fill without naming the private padding fields.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        sym: SymbolId,
        side: Side,
        px: Price,
        qty: Qty,
        order_id: u64,
    ) -> Self {
        Self {
            ts_ns,
            sym,
            side,
            _pad0: [0; 3],
            px,
            qty,
            order_id,
            _pad1: [0; 16],
            _pad2: [0; 8],
        }
    }
}

/// An order request from a strategy, handed off to `clob-dispatcher`.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct Order {
    /// When the strategy decided.
    pub ts_ns: NsTs,
    /// Market.
    pub sym: SymbolId,
    /// Side.
    pub side: Side,
    /// Order-type tag (0=post-only limit; extend over time).
    pub kind: u8,
    /// Reserved.
    _pad0: [u8; 2],
    /// Limit price.
    pub px: Price,
    /// Quantity.
    pub qty: Qty,
    /// Client-assigned idempotency key.
    pub client_oid: u64,
    /// Target venue ([`VenueId`] as raw byte). The engine's
    /// `VenueRouter` dispatches on this byte (Phase 8j).
    pub venue: u8,
    /// Emitting strategy-set slot (M4.1 M-b; docs/wire-format.md):
    /// stamped by the set's `StampCtx` adapter around each member
    /// callback; [`STRATEGY_ID_NONE`] (`0xFF`) = unattributed (bare
    /// single-strategy boots, tests). Wire-additive pre-first-capture
    /// — no Order slot was ever persisted before `engine-orders.pmlr`.
    pub strategy_id: u8,
    /// Reserved.
    _pad1: [u8; 14],
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad2: [u8; 8],
}

/// `Order.strategy_id` value meaning "no strategy attribution".
pub const STRATEGY_ID_NONE: u8 = 0xFF;

impl Order {
    /// Construct an Order without naming the private padding fields.
    /// `strategy_id` starts [`STRATEGY_ID_NONE`]; the strategy-set's
    /// stamping ctx overwrites it per member (M4.1 M-c).
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        venue: VenueId,
        sym: SymbolId,
        side: Side,
        kind: u8,
        px: Price,
        qty: Qty,
        client_oid: u64,
    ) -> Self {
        Self {
            ts_ns,
            sym,
            side,
            kind,
            _pad0: [0; 2],
            px,
            qty,
            client_oid,
            venue: venue as u8,
            strategy_id: STRATEGY_ID_NONE,
            _pad1: [0; 14],
            _pad2: [0; 8],
        }
    }
}

// ---------------------------------------------------------------
// AsBytes — unsafe marker for zero-copy serialization.
// ---------------------------------------------------------------

/// Unsafe marker trait opting a type in to raw byte-level serialization.
///
/// Implementors promise:
///
/// * `Self: Copy` (already required by this trait bound).
/// * `Self` has a stable, fully-initialized representation — no
///   uninitialized padding bytes, no indirection, no
///   platform-sensitive layout.
///
/// Given these guarantees, a caller may reinterpret `&Self` as
/// `&[u8; size_of::<Self>()]` without invoking UB.
///
/// This trait intentionally has no methods: the crate uses it as a
/// tag that the writer layer checks.
///
/// # Safety
///
/// An impl that violates the invariants above will read uninitialized
/// memory and corrupt any consumer that mmaps the replay log.
pub unsafe trait AsBytes: Copy {}

// SAFETY: Tick is `#[repr(C, align(64))]`, `#[derive(Copy)]`, all
// fields are plain integers and explicitly-named `_pad` arrays that
// sum to exactly 64 bytes (checked by `layout_is_fully_explicit` in
// tests) — there is no compiler-inserted padding, so every byte is
// initialized.
unsafe impl AsBytes for Tick {}

// SAFETY: Signal has the same guarantees as Tick.
unsafe impl AsBytes for Signal {}

// SAFETY: Fill has the same guarantees as Tick.
unsafe impl AsBytes for Fill {}

// SAFETY: Order has the same guarantees as Tick.
unsafe impl AsBytes for Order {}

// SAFETY: ChannelEvent is `#[repr(C, align(64))]`, `#[derive(Copy)]`,
// all fields plain integers + explicit `_pad` arrays summing to exactly
// 64 bytes (checked by `channel_event_layout_is_fully_explicit`); no
// compiler-inserted padding, every byte initialized.
unsafe impl AsBytes for ChannelEvent {}

// SAFETY: AiCmd is `#[repr(C, align(64))]`, `#[derive(Copy)]`, all
// fields plain integers + an explicit `_pad` array summing to exactly
// 64 bytes (checked by the compile-time offset asserts below and
// `ai_cmd_layout_is_fully_explicit`); no compiler-inserted padding,
// every byte initialized.
unsafe impl AsBytes for AiCmd {}

// SAFETY: OptSummary is `#[repr(C, align(64))]`, `#[derive(Copy)]`,
// all fields plain integers + an explicit `_pad0` array summing to
// exactly 64 bytes (checked by `opt_summary_layout_is_fully_explicit`
// in tests); no compiler-inserted padding, every byte initialized.
unsafe impl AsBytes for OptSummary {}

const _: () = assert!(core::mem::size_of::<OptSummary>() == 64);

// SAFETY: DepthTopK is `#[repr(C, align(64))]`, `#[derive(Copy)]`,
// all fields plain integers / `#[repr(C)]` integer-pair levels +
// explicit `_pad` bytes summing to exactly 192 bytes (checked by
// `depth_top_k_layout_is_fully_explicit` in tests); no
// compiler-inserted padding, every byte initialized.
unsafe impl AsBytes for DepthTopK {}

const _: () = assert!(core::mem::size_of::<DepthTopK>() == 192);
const _: () = assert!(core::mem::align_of::<DepthTopK>() == 64);

// ---------------------------------------------------------------
// ChannelEvent — non-tick channel capture slot (Phase 8e, §6.5)
// ---------------------------------------------------------------

/// Venue-agnostic channel tag carried by [`ChannelEvent`]. Wire-stable
/// (PMLR event logs) — never renumber. The BBO channels have no entry
/// here deliberately: BBO flows as [`Tick`] into the per-venue tick
/// log; events cover everything else so the §6.5 venue × channel
/// coverage matrix is reconstructable offline.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelId {
    /// Trade prints (OKX `trades`, Deribit `trades.100ms`, HL `trades`).
    Trade = 0,
    /// Depth/book updates — header-level capture (OKX `books`, Deribit
    /// `book.100ms`, HL `l2Book`).
    Book = 1,
    /// Mark price (OKX `mark-price`).
    Mark = 2,
    /// Funding rate (OKX `funding-rate`; WS3: Deribit perp tickers'
    /// `current_funding` — `v0` = rate ×1e9 on both venues, `v1` =
    /// next-funding time ms on OKX / 0 on Deribit (continuous
    /// funding, no discrete next time)).
    Funding = 3,
    /// Composite ticker (Deribit `ticker.100ms`).
    Ticker = 4,
    /// Per-asset context (HL `activeAssetCtx`).
    AssetCtx = 5,
    /// Whole-venue mid sweep (HL `allMids`).
    AllMids = 6,
    /// HIP-4 outcome lifecycle (HL `outcomeMetaUpdates`).
    OutcomeMeta = 7,
    /// Polymarket `price_change` rows that did not move the touch (the
    /// touch-moving ones become Ticks).
    PriceChange = 8,
    /// Runtime trade-seq monitor increment (G1 remediation, 2026-08-15):
    /// emitted 1:1 with every trades-channel `gaps_total` increment so
    /// §6.6's "every increment paired with a logged venue event" is
    /// mechanically checkable offline. `venue_seq` = observed seq,
    /// `v0` = expected seq, `v1` = observed seq.
    TradeGap = 9,
    /// Runtime book-chain monitor increment, same pairing contract as
    /// [`ChannelId::TradeGap`]. `venue_seq` = the message `change_id`,
    /// `v0` = expected `prev_change_id` (the chain's last; `i64::MIN`
    /// = monitor was awaiting a snapshot), `v1` = observed
    /// `prev_change_id`.
    BookGap = 10,
    /// Runtime NON-FATAL subscribe drop (WS2, capture-continuity
    /// outage 2026-08-27 §5.2 remediation): a venue refused one
    /// subscribe arg — or omitted one expected channel from its
    /// subscribe echo — on a RECONNECT session, and the ingress
    /// dropped that instrument/channel from the session's subscribe
    /// set instead of failing the session. Emitted 1:1 with every
    /// `sub_drops_total` increment (the §6.6 pairing contract).
    /// `sym` = the dropped instrument (`SYMBOL_ID_NONE` when the
    /// venue's error names no resolvable instrument), `venue_seq` = 0,
    /// `v0` = the venue's numeric error code (0 = missing-from-echo,
    /// no code), `v1` = venue-local channel discriminant (−1 =
    /// unknown, or a folded option row).
    SubDrop = 11,
    /// WS6: venue volatility-index series (Deribit DVOL,
    /// `deribit_volatility_index.{index}`). Venue-GLOBAL — `sym` =
    /// `SYMBOL_ID_NONE`; `v0` = volatility POINTS ×1e9 (the venue's
    /// percent-points figure, e.g. 59.18 → 59_180_000_000), `v1` =
    /// 0-based ordinal of the index in the boot-configured
    /// options-underlyings list (the offline identity — the boot log
    /// + universe file resolve it), `venue_time_ms` = venue ts.
    VolIndex = 12,
}

impl ChannelId {
    /// Decode a raw byte from a PMLR event slot. `None` = corrupt log.
    #[inline]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Trade),
            1 => Some(Self::Book),
            2 => Some(Self::Mark),
            3 => Some(Self::Funding),
            4 => Some(Self::Ticker),
            5 => Some(Self::AssetCtx),
            6 => Some(Self::AllMids),
            7 => Some(Self::OutcomeMeta),
            8 => Some(Self::PriceChange),
            9 => Some(Self::TradeGap),
            10 => Some(Self::BookGap),
            11 => Some(Self::SubDrop),
            12 => Some(Self::VolIndex),
            _ => None,
        }
    }

    /// Log/report-friendly name.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trade => "trade",
            Self::Book => "book",
            Self::Mark => "mark",
            Self::Funding => "funding",
            Self::Ticker => "ticker",
            Self::AssetCtx => "asset_ctx",
            Self::AllMids => "all_mids",
            Self::OutcomeMeta => "outcome_meta",
            Self::PriceChange => "price_change",
            Self::TradeGap => "trade_gap",
            Self::BookGap => "book_gap",
            Self::SubDrop => "sub_drop",
            Self::VolIndex => "vol_index",
        }
    }
}

/// One captured non-tick channel event (Phase 8e, plan §6.5). Written
/// by each ingress thread into its per-venue PMLR event log so the
/// offline audit (`cli audit-replay`) can compute per-channel message
/// rates, inter-arrival histograms and gap totals without re-parsing
/// raw venue frames.
///
/// `v0`/`v1` are channel-dependent payloads (documented per
/// [`ChannelId`] at the capture site): typically price ×1e6 and
/// qty ×1e6 for `Trade`, level counts for `Book`, rate ×1e9 for
/// `Funding`. The audit tool primarily consumes
/// `ts_ns`/`venue`/`channel`/`sym`/`venue_seq`/`venue_time_ms`.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct ChannelEvent {
    /// When the ingress thread finished parsing the event.
    pub ts_ns: NsTs,
    /// Venue-namespaced symbol, or `SYMBOL_ID_NONE` for venue-global
    /// channels (`AllMids`, `OutcomeMeta`).
    pub sym: SymbolId,
    /// Producing venue ([`VenueId`] as raw byte).
    pub venue: u8,
    /// Channel tag ([`ChannelId`] as raw byte).
    pub channel: u8,
    /// Reserved. Always zero.
    _pad0: [u8; 2],
    /// Venue-provided sequence (full width — OKX `seqId`, Deribit
    /// `change_id`/`trade_seq`); 0 where the channel carries none.
    pub venue_seq: u64,
    /// Venue-provided timestamp in ms; 0 where absent.
    pub venue_time_ms: u64,
    /// Channel-dependent payload 0 (see struct docs).
    pub v0: i64,
    /// Channel-dependent payload 1 (see struct docs).
    pub v1: i64,
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad1: [u8; 16],
}

impl ChannelEvent {
    /// Construct a ChannelEvent without naming the private padding.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        ts_ns: NsTs,
        venue: VenueId,
        channel: ChannelId,
        sym: SymbolId,
        venue_seq: u64,
        venue_time_ms: u64,
        v0: i64,
        v1: i64,
    ) -> Self {
        Self {
            ts_ns,
            sym,
            venue: venue as u8,
            channel: channel as u8,
            _pad0: [0; 2],
            venue_seq,
            venue_time_ms,
            v0,
            v1,
            _pad1: [0; 16],
        }
    }
}

// ---------------------------------------------------------------
// DepthTopK — L2 top-of-book depth carrier (WS10-B)
// ---------------------------------------------------------------

/// Levels per side carried by [`DepthTopK`] (D-B2). The in-ingress
/// ladder maintains up to `book_builder::DEPTH_LADDER_CAP` levels;
/// K is the CARRIER bound — what strategies and the depth capture
/// see.
pub const DEPTH_K: usize = 5;

/// Capacity of every depth SPSC ring (WS10-B). Book channels run
/// 10–20 Hz/instrument and emission is change-gated, so 4096 slots
/// is generous headroom. Power of two, like every `core-ring`
/// capacity.
pub const DEPTH_RING_SIZE: usize = 4096;

/// [`DepthTopK::flags`] bit 0: the book behind this snapshot broke
/// its venue seq chain and is resyncing — a strategy must never
/// trade a known-broken book. Set on the ladder-clearing emit; the
/// first post-resync snapshot arrives with flags = 0.
pub const DEPTH_FLAG_STALE: u8 = 1;

/// One price level inside [`DepthTopK`]. `qty_1e6 == 0` marks an
/// unpopulated slot (a book with fewer than K levels on that side).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct DepthLevel {
    /// Price ×1e6.
    pub px_1e6: i64,
    /// Quantity ×1e6 (venue-native units, per the venue's tick
    /// convention documented in docs/wire-format.md).
    pub qty_1e6: i64,
}

impl DepthLevel {
    /// The unpopulated-slot sentinel.
    pub const EMPTY: Self = Self {
        px_1e6: 0,
        qty_1e6: 0,
    };
}

/// Top-K L2 depth snapshot (WS10-B, D-B1/D-B2): the carrier the
/// in-ingress ladder emits onto the per-venue depth ring AND into
/// the `<venue>-depth.pmlr` capture (slot kind 7) whenever the top-K
/// actually changed (byte-compare emission gate). 192 bytes, three
/// cache lines.
///
/// `bids` are best-first (descending price), `asks` best-first
/// (ascending price); slots beyond the book's real depth are
/// [`DepthLevel::EMPTY`].
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct DepthTopK {
    /// When the ingress thread finished applying the update that
    /// produced this snapshot.
    pub ts_ns: NsTs,
    /// Venue-namespaced symbol.
    pub sym: SymbolId,
    /// Producing venue ([`VenueId`] as raw byte).
    pub venue: u8,
    /// Populated level count hint: `min(K, real bid levels)` in the
    /// low nibble is deliberately NOT packed — `k` carries
    /// [`DEPTH_K`] for forward-compat readers.
    pub k: u8,
    /// [`DEPTH_FLAG_STALE`] et al.
    pub flags: u8,
    /// Reserved. Always zero.
    _pad0: u8,
    /// Best-first bid levels.
    pub bids: [DepthLevel; DEPTH_K],
    /// Best-first ask levels.
    pub asks: [DepthLevel; DEPTH_K],
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad1: [u8; 16],
}

impl DepthTopK {
    /// Construct with zeroed padding.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
        venue: VenueId,
        sym: SymbolId,
        flags: u8,
        bids: [DepthLevel; DEPTH_K],
        asks: [DepthLevel; DEPTH_K],
    ) -> Self {
        Self {
            ts_ns,
            sym,
            venue: venue as u8,
            k: DEPTH_K as u8,
            flags,
            _pad0: 0,
            bids,
            asks,
            _pad1: [0; 16],
        }
    }

    /// The all-empty snapshot (boot value for last-emitted compare
    /// state; also the base of a STALE emit).
    pub const EMPTY: Self = Self::new(
        0,
        VenueId::Polymarket,
        0,
        0,
        [DepthLevel::EMPTY; DEPTH_K],
        [DepthLevel::EMPTY; DEPTH_K],
    );
}

// ---------------------------------------------------------------
// AiCmd — AI-ingress command slot (Phase 8f, plan §8.4)
// ---------------------------------------------------------------

// The AiCmd wire layout equals its in-memory layout only on
// little-endian targets — the only targets this workspace supports
// (docs/wire-format.md pins native-endian == LE).
#[cfg(not(target_endian = "little"))]
compile_error!("AiCmd wire format assumes little-endian (see docs/wire-format.md)");

/// Command kind carried by [`AiCmd`]. Wire-stable (UDS frames, PMLR
/// `slot_kind = 4`) — never renumber, only append.
///
/// There is deliberately **no `Resume` kind**: halt is sticky and
/// requires a manual engine restart (docs/risk-policy.md), so the
/// command cannot even be expressed on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AiCmdKind {
    /// Liveness beacon (serve: every 5 s; verbs: one before payload).
    Heartbeat = 0,
    /// Set a strategy-set slot enable bit. Refused while halted.
    EnableStrategy = 1,
    /// Clear a strategy-slot enable bit. Always honored.
    DisableStrategy = 2,
    /// Publish a fair value for `sym` into the ai-exec table (TTL'd).
    SetFairValue = 3,
    /// Publish a signed bias for `sym` into the ai-exec table (TTL'd).
    SetBias = 4,
    /// Set a per-strategy numeric parameter (`param_id` selects).
    SetParam = 5,
    /// Paper-only order intent for the ai-exec strategy (8i clamps).
    OrderIntent = 6,
    /// Stage a validated ruleset by content hash (gates bound upstream).
    RulesetStage = 7,
    /// Commit a previously staged ruleset hash.
    RulesetCommit = 8,
    /// Request a sticky engine halt. No wire-expressible resume.
    HaltRequest = 9,
    /// VM2 D-1 (docs/vm2-plan.md §8 V0 entry): one historical funding
    /// PRINT for a perp instrument — `sym` = instrument, `px` = raw
    /// per-print rate ×1e9 (signed), `qty` = venue print time ms.
    /// Pushed by the hourly funding data agent right after boot
    /// (window seeding) and on each venue-dark-BN 8 h print. The
    /// engine folds it into the same per-sym funding windows the live
    /// `on_venue_event` Funding path feeds — the cadence law
    /// ([`funding_print_divisor`]) applies in that ONE place for both.
    FundingSeed = 10,
    /// VM2 D-2 (operator ruling 2026-08-29: positions RESTORE at
    /// boot): re-enter one v2 table row's position after a restart.
    /// `param_id` = row index, `sym` = the row's action sym
    /// (consume-time cross-check against the committed row — a
    /// mismatch refuses the seed), `side` = entered side, `px` =
    /// entry px ×1e6, `qty` = position AGE in SECONDS at send time
    /// (≥ 0; engine derives entry_ts = now − age·1e9 — no wall-clock
    /// crossing, the §13-decision-1 pattern), `ttl_ns` = 0 ENFORCED —
    /// the drain site expires any kind with `ttl_ns ≠ 0`, so age can
    /// never ride there. Entry QTY is deliberately not carried: the
    /// vm re-derives it from the committed row's own sizing law at
    /// the seeded entry px (min(row cap, policy cap) / px), so a
    /// restored position always respects the CURRENT caps. Sent by
    /// the worker's post-boot waiter AFTER it verifies the #7b
    /// re-commit landed the expected hash.
    PositionSeed = 11,
}

impl AiCmdKind {
    /// Raw byte value as stored in [`AiCmd::kind`].
    #[inline(always)]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a raw byte from a UDS frame or PMLR slot. `None` for
    /// unknown values — the accept path counts these as malformed.
    #[inline(always)]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Heartbeat),
            1 => Some(Self::EnableStrategy),
            2 => Some(Self::DisableStrategy),
            3 => Some(Self::SetFairValue),
            4 => Some(Self::SetBias),
            5 => Some(Self::SetParam),
            6 => Some(Self::OrderIntent),
            7 => Some(Self::RulesetStage),
            8 => Some(Self::RulesetCommit),
            9 => Some(Self::HaltRequest),
            10 => Some(Self::FundingSeed),
            11 => Some(Self::PositionSeed),
            _ => None,
        }
    }
}

/// Capacity of the AI command SPSC ring (design §4.3). Power of two,
/// like every `core-ring` capacity.
pub const AI_RING_SIZE: usize = 1024;

/// Capacity of every venue-event SPSC ring (WS10-A, D-A2). One ring
/// per tick lane; [`ChannelEvent`] slots. Funding runs 1–10 Hz per
/// venue, so 1024 slots is hours of headroom; 1024 × 64 B × 6 lanes
/// = 384 KiB boot-time. Power of two, like every `core-ring`
/// capacity.
pub const EVENT_RING_SIZE: usize = 1024;

/// Bit for `channel` in a venue-event lane mask (WS10-A gating knob:
/// an ingress pushes a [`ChannelEvent`] onto its lane only when the
/// channel's bit is set in the spawn-time `event_mask`). `ChannelId`
/// discriminants are ≤ 12, so `u16` covers the whole enum.
#[inline]
pub const fn event_lane_bit(ch: ChannelId) -> u16 {
    1u16 << (ch as u16)
}

/// The v1 venue-event lane mask: ONLY funding flows (WS10-A ships
/// deliberately narrow — Mark/OI/DVOL flip on later by widening the
/// spawn-time mask, no new plumbing).
pub const EVENT_LANE_FUNDING: u16 = event_lane_bit(ChannelId::Funding);

/// VM2 V2: the AssetCtx lane bit — Hyperliquid's funding rides its
/// `activeAssetCtx` events (`v0` = rate ×1e9; there is no HL Funding
/// channel), so the HL ingress spawns with
/// `EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX` and the vm feature
/// engine treats HL AssetCtx events as funding samples.
pub const EVENT_LANE_ASSET_CTX: u16 = event_lane_bit(ChannelId::AssetCtx);

/// Capacity of every OptSummary SPSC lane (VM2 V2 — the kind-6
/// channel enters the engine for the first time; capture-only
/// before). OKX `opt-summary` pushes whole-family bursts (the capped
/// E2/K8 chains ≈ 200 instruments per push) and Deribit option
/// tickers run at 100 ms cadence, so 4096 slots (256 KiB/lane)
/// absorbs a full burst with an order of magnitude of headroom.
/// Power of two, like every `core-ring` capacity.
pub const OPT_RING_SIZE: usize = 4096;

/// `AiCmd::flags` bit 0: the ai-exec fair-table entry this command
/// creates additionally expires when worker heartbeats go stale,
/// not only when its own TTL lapses. Valid on `SetFairValue` /
/// `SetBias` only.
pub const AI_CMD_FLAG_EXPIRE_ON_SILENCE: u16 = 1 << 0;

/// `AiCmd::strategy_id` sentinel meaning "no strategy slot".
pub const STRATEGY_SLOT_NONE: u8 = 0xFF;

/// Hard ceiling on strategy-set slots. The runtime enable mask is a
/// `u8` bitmask (design §7), so slot indices are wire-bounded to 0..8
/// forever.
pub const MAX_STRATEGY_SLOTS: u8 = 8;

/// Strategy-set slot of `strategy-ai-exec` (design §7). `OrderIntent`
/// commands must target exactly this slot.
pub const STRATEGY_SLOT_AI_EXEC: u8 = 4;

/// Strategy-set slot of `strategy-vm` (built in 8g item 6).
/// `RulesetStage` / `RulesetCommit` commands must target exactly
/// this slot.
pub const STRATEGY_SLOT_VM: u8 = 5;

/// `AiCmd::side` sentinel meaning "no side" (every kind except
/// `OrderIntent`).
pub const AI_SIDE_NONE: u8 = 0xFF;

/// Why an [`AiCmd`] failed [`AiCmd::validate_shape`]. Each variant maps
/// to one violated row/column of the per-kind shape table in
/// `docs/arch/phase-8f-design.md` §3; the accept path folds all of them into
/// `engine_ingress_ai_malformed_total` and keeps the frame in the PMLR
/// capture for offline audit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AiCmdShapeError {
    /// `kind` byte is not a known [`AiCmdKind`].
    UnknownKind(u8),
    /// Explicit tail padding contains a non-zero byte.
    NonZeroPad,
    /// `venue` byte is wrong for this kind (engine-directed kinds
    /// require `VenueId::Ai`; `OrderIntent` requires a real market
    /// venue).
    BadVenue(u8),
    /// `sym` violates the kind's required/forbidden rule.
    BadSym(SymbolId),
    /// `px` violates the kind's rule (must-be-zero, or sign/range).
    BadPx(i64),
    /// `qty` violates the kind's rule.
    BadQty(i64),
    /// `ttl_ns` violates the kind's required(>0)/must-be-zero rule.
    BadTtl(u64),
    /// `strategy_id` violates the kind's slot rule.
    BadStrategySlot(u8),
    /// `side` violates the kind's rule (`Side` byte or `0xFF`).
    BadSide(u8),
    /// `param_id` must be zero for every kind except `SetParam`.
    BadParamId(u16),
    /// `flags` carries bits the kind does not define.
    BadFlags(u16),
}

/// AI-ingress command — 64 bytes, one cache line (Phase 8f, plan §8.4;
/// byte layout pinned in `docs/wire-format.md`). Produced by
/// `claude-worker` as the payload of an 82-byte HMAC-tagged UDS frame,
/// materialized by `ingress-ai`, captured to PMLR (`slot_kind = 4`) and
/// pushed onto the `Ring<AiCmd, AI_RING_SIZE>` consumed in
/// `Engine::tick()`.
///
/// `ts_ns` is rewritten by `ingress-ai` to engine-monotonic time at
/// accept (after HMAC verify, before ring push) so TTL arithmetic never
/// crosses clock domains — design §13 decision 1. The PMLR capture
/// record carries the rewritten slot (byte-identical to what the ring
/// consumer sees — operator decision 2026-08-15); the worker's original
/// send time survives only in the optional raw tap.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct AiCmd {
    /// Engine-monotonic accept time (see struct docs; worker send time
    /// pre-rewrite).
    pub ts_ns: NsTs,
    /// Strictly increasing per worker session. Gaps are counted, never
    /// fatal; regressions are discarded.
    pub seq: u32,
    /// Venue-namespaced [`SymbolId`], or [`SYMBOL_ID_NONE`] where the
    /// kind carries no symbol.
    pub sym: SymbolId,
    /// Fixed-point ×1e6: fair value / intent price / param value /
    /// ruleset hash bytes 0..8 LE.
    pub px: i64,
    /// Fixed-point ×1e6: intent quantity / ruleset hash bytes 8..16 LE.
    pub qty: i64,
    /// Expiry relative to `ts_ns` (engine clock after rewrite); 0 = no
    /// expiry where the kind's shape allows it.
    pub ttl_ns: u64,
    /// [`AiCmdKind`] as raw byte.
    pub kind: u8,
    /// [`VenueId`] as raw byte: `Ai` for engine-directed commands, the
    /// target market venue for `OrderIntent`.
    pub venue: u8,
    /// Strategy-set slot index, or [`STRATEGY_SLOT_NONE`].
    pub strategy_id: u8,
    /// [`Side`] as raw byte, or [`AI_SIDE_NONE`].
    pub side: u8,
    /// `SetParam` selector; 0 for every other kind.
    pub param_id: u16,
    /// Bit 0 = [`AI_CMD_FLAG_EXPIRE_ON_SILENCE`]; all other bits must
    /// be zero.
    pub flags: u16,
    /// Explicit tail padding — [`AsBytes`] requires every byte of the
    /// 64 B slot to be initialized. Always zero; enforced by
    /// [`Self::validate_shape`] so captured frames stay canonical.
    _pad: [u8; 16],
}

impl AiCmd {
    /// Construct an `AiCmd` without naming the private padding field.
    /// Production Rust only *parses* commands ([`Self::read_le`]); this
    /// constructor serves tests and loopback clients.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        ts_ns: NsTs,
        seq: u32,
        sym: SymbolId,
        px: i64,
        qty: i64,
        ttl_ns: u64,
        kind: AiCmdKind,
        venue: VenueId,
        strategy_id: u8,
        side: u8,
        param_id: u16,
        flags: u16,
    ) -> Self {
        Self {
            ts_ns,
            seq,
            sym,
            px,
            qty,
            ttl_ns,
            kind: kind.to_u8(),
            venue: venue.to_u8(),
            strategy_id,
            side,
            param_id,
            flags,
            _pad: [0; 16],
        }
    }

    /// Decoded [`AiCmdKind`], or `None` for an unknown `kind` byte.
    #[inline(always)]
    pub const fn kind(&self) -> Option<AiCmdKind> {
        AiCmdKind::from_u8(self.kind)
    }

    /// Reassemble the ruleset identity hash128 carried by
    /// `RulesetStage` / `RulesetCommit` frames from the `px` + `qty`
    /// halves (`px` = bytes 0..8 LE, `qty` = bytes 8..16 LE — the
    /// field-doc convention above). THE shared helper of 8g §6: the
    /// ingress-ai side path and the `strategy-vm` Commit flip must
    /// reassemble identically or the two state machines desynchronize,
    /// so the pairing lives here, next to the fields that define it.
    /// Meaningless for other kinds (there `px`/`qty` are prices).
    #[inline(always)]
    pub const fn ruleset_hash128(&self) -> [u8; 16] {
        let px = self.px.to_le_bytes();
        let qty = self.qty.to_le_bytes();
        let mut h = [0u8; 16];
        let mut i = 0;
        while i < 8 {
            h[i] = px[i];
            h[8 + i] = qty[i];
            i += 1;
        }
        h
    }

    /// Materialize an `AiCmd` from 64 wire bytes (little-endian, i.e.
    /// native — compile-guarded above).
    ///
    /// Zero-copy accounting (doctrine): UDS frames sit at arbitrary
    /// offsets in the rx buffer, so a 64-alignment-free view is
    /// impossible; this is the **one documented copy** that materializes
    /// the slot onto the stack (a handful of vector moves). The
    /// subsequent ring `try_push` copy is ownership transfer, identical
    /// to every other ingress.
    #[inline(always)]
    pub fn read_le(bytes: &[u8; 64]) -> Self {
        // SAFETY: the source is a valid, initialized 64-byte buffer;
        // `AiCmd` is `#[repr(C)]`, `Copy`, exactly 64 bytes (static
        // asserts below), and every field is a plain integer type, so
        // any bit pattern is a valid `AiCmd`. `read_unaligned` imposes
        // no alignment requirement on the source.
        unsafe { bytes.as_ptr().cast::<AiCmd>().read_unaligned() }
    }

    /// Validate kind range and the full per-kind field-shape table of
    /// `docs/arch/phase-8f-design.md` §3 ("unused fields MUST be zero /
    /// `SYMBOL_ID_NONE` / `0xFF`"). Run by `ingress-ai` at accept
    /// (§4.4 step 4) and by the engine drain site; failures increment
    /// `engine_ingress_ai_malformed_total`.
    pub fn validate_shape(&self) -> Result<(), AiCmdShapeError> {
        let kind = match AiCmdKind::from_u8(self.kind) {
            Some(k) => k,
            None => return Err(AiCmdShapeError::UnknownKind(self.kind)),
        };

        // Canonical-bytes rule: explicit padding must be zero.
        let mut i = 0;
        while i < self._pad.len() {
            if self._pad[i] != 0 {
                return Err(AiCmdShapeError::NonZeroPad);
            }
            i += 1;
        }

        // Venue column: `Ai` for engine-directed kinds, a real market
        // venue for intents.
        match kind {
            AiCmdKind::OrderIntent => match VenueId::from_u8(self.venue) {
                Some(VenueId::Ai) | None => return Err(AiCmdShapeError::BadVenue(self.venue)),
                Some(_) => {}
            },
            _ => {
                if self.venue != VenueId::Ai.to_u8() {
                    return Err(AiCmdShapeError::BadVenue(self.venue));
                }
            }
        }

        // Remaining columns, one arm per row of the §3 table.
        match kind {
            AiCmdKind::Heartbeat | AiCmdKind::HaltRequest => {
                if self.sym != SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.px != 0 {
                    return Err(AiCmdShapeError::BadPx(self.px));
                }
                if self.qty != 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_NONE {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::EnableStrategy | AiCmdKind::DisableStrategy => {
                if self.sym != SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.px != 0 {
                    return Err(AiCmdShapeError::BadPx(self.px));
                }
                if self.qty != 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id >= MAX_STRATEGY_SLOTS {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::SetFairValue | AiCmdKind::SetBias => {
                if self.sym == SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                // Fair values are prices (non-negative); biases are
                // signed by design.
                if matches!(kind, AiCmdKind::SetFairValue) && self.px < 0 {
                    return Err(AiCmdShapeError::BadPx(self.px));
                }
                if self.qty != 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns == 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_NONE {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags & !AI_CMD_FLAG_EXPIRE_ON_SILENCE != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::SetParam => {
                // `sym` may be SYMBOL_ID_NONE (set-level) or a real
                // symbol; `px` is the raw parameter value — both
                // unconstrained here.
                if self.qty != 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id >= MAX_STRATEGY_SLOTS {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::OrderIntent => {
                if self.sym == SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.px <= 0 {
                    return Err(AiCmdShapeError::BadPx(self.px));
                }
                if self.qty <= 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns == 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_AI_EXEC {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != Side::Bid as u8 && self.side != Side::Ask as u8 {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::RulesetStage | AiCmdKind::RulesetCommit => {
                // `px`/`qty` carry hash128 halves — any bit pattern.
                if self.sym != SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_VM {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::FundingSeed => {
                // VM2 D-1: `px` = rate ×1e9 (any sign, 0 legal — a
                // zero print is a venue fact), `qty` = venue print
                // time ms (> 0: a print without a time is meaningless).
                if self.sym == SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.qty <= 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_VM {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != AI_SIDE_NONE {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id != 0 {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
            AiCmdKind::PositionSeed => {
                // VM2 D-2: `param_id` = v2 row index (< table rows),
                // `side` = the entered side (required), `px` = entry
                // px ×1e6 (> 0), `qty` = position age SECONDS (≥ 0;
                // 0 = just entered), `ttl_ns` MUST be 0 — the engine
                // drain expires any kind with a nonzero ttl, and a
                // seed must never expire (kind docs).
                if self.sym == SYMBOL_ID_NONE {
                    return Err(AiCmdShapeError::BadSym(self.sym));
                }
                if self.px <= 0 {
                    return Err(AiCmdShapeError::BadPx(self.px));
                }
                if self.qty < 0 {
                    return Err(AiCmdShapeError::BadQty(self.qty));
                }
                if self.ttl_ns != 0 {
                    return Err(AiCmdShapeError::BadTtl(self.ttl_ns));
                }
                if self.strategy_id != STRATEGY_SLOT_VM {
                    return Err(AiCmdShapeError::BadStrategySlot(self.strategy_id));
                }
                if self.side != Side::Bid as u8 && self.side != Side::Ask as u8 {
                    return Err(AiCmdShapeError::BadSide(self.side));
                }
                if self.param_id as usize >= RULE_TABLE_ROWS {
                    return Err(AiCmdShapeError::BadParamId(self.param_id));
                }
                if self.flags != 0 {
                    return Err(AiCmdShapeError::BadFlags(self.flags));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------
// Capture — per-ingress replay/tap sink (Phase 8e, §6.5)
// ---------------------------------------------------------------

/// Sink for everything an ingress thread parses, threaded through the
/// run loops as a monomorphized generic (`C: Capture` — no `dyn`, per
/// doctrine). The production impl is `core-io`'s `PmlrCapture`
/// (per-venue PMLR logs + optional bounded raw tap); tests and
/// capture-off paths use [`NullCapture`], whose no-op defaults compile
/// away entirely.
///
/// All methods are infallible by design: capture failure must degrade
/// capture, never the market-data session (the impl owns its error
/// policy and surfaces loss through counters).
pub trait Capture {
    /// One parsed BBO tick (called before the ring `try_push`, so
    /// ring-dropped ticks are still captured — the offline audit
    /// compares capture counts against `ring_drops_total`).
    #[inline(always)]
    fn tick(&mut self, _t: &Tick) {}

    /// One parsed options analytics record (M2.3, mvp-plan §9.8 —
    /// Deribit option `ticker`, OKX `opt-summary`, BN eapi at M2.4).
    /// Capture-only: never pushed to the engine ring.
    #[inline(always)]
    fn opt_summary(&mut self, _o: &OptSummary) {}

    /// One parsed non-tick channel event.
    #[inline(always)]
    fn event(&mut self, _e: &ChannelEvent) {}

    /// One emitted top-K depth snapshot (WS10-B, D-B1 — slot kind 7,
    /// `<venue>-depth.pmlr`). Called at the change-gated emission
    /// site, BEFORE the depth-ring push (capture-before-push law).
    #[inline(always)]
    fn depth(&mut self, _d: &DepthTopK) {}

    /// One parsed signal (RPC ingress).
    #[inline(always)]
    fn signal(&mut self, _s: &Signal) {}

    /// One raw inbound WS/HTTP payload, pre-parse. Only observed when
    /// the impl's tap mode is `All` — bounded, off in production.
    #[inline(always)]
    fn raw_frame(&mut self, _ts_ns: NsTs, _payload: &[u8]) {}

    /// One payload the parser rejected, at the site that increments
    /// `parse_errors_total`. Only observed in tap modes `Rejects`/`All`.
    #[inline(always)]
    fn parse_reject(&mut self, _ts_ns: NsTs, _payload: &[u8]) {}

    /// Time-based flush hook; run loops call this once per outer poll
    /// iteration so staged bytes reach disk within the flush interval
    /// even on quiet feeds.
    #[inline(always)]
    fn maybe_flush(&mut self, _now_ns: NsTs) {}
}

/// The do-nothing [`Capture`]: every hook is the trait's empty default,
/// monomorphizing to nothing at the call sites.
#[derive(Copy, Clone, Debug, Default)]
pub struct NullCapture;

impl Capture for NullCapture {}

// ---------------------------------------------------------------
// RuleRow / RuleTable — operator-committed ruleset (Phase 8g, §3)
// ---------------------------------------------------------------

/// FNV-1a 64 over raw bytes. Pins the [`RuleRow::name_h`] identity
/// (8g design §3): offline correlation between hot-table rows and the
/// row names that live only in the JSON artifact + the worker
/// registry. `const` so fixtures can hash at compile time; two lines
/// by design — no external crate.
#[inline]
pub const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
        i += 1;
    }
    h
}

/// Row capacity of a [`RuleTableV2`] — the §4.2 rule-4 upper cap.
pub const RULE_TABLE_ROWS: usize = 256;

/// One validated rule — 64 bytes, exactly one cache line; no row
/// straddles a line (8g design §3; byte layout pinned in
/// `docs/wire-format.md`).
///
/// POD; every field fixed-width. Strings never reach the hot table:
/// row names live only in the JSON artifact + the worker registry;
/// [`Self::name_h`] carries the FNV-1a 64 of the name bytes for
/// offline correlation. Rows are **built** by the `ingress-ai`
/// validator (§4.2), never parsed from wire bytes — hence no
/// `AsBytes` impl, no serialization, no shape validator: the builder
/// upholds the invariants and the table never crosses a process
/// boundary (identity is the table's `hash128`; the JSON artifact is
/// the durable form).
#[derive(Copy, Clone)]
#[repr(C, align(64))]
pub struct RuleRow {
    /// Action-leg [`SymbolId`], validated against the boot universe
    /// snapshot (§4.3).
    pub sym: SymbolId,
    /// `cross_deviation` reference leg; [`SYMBOL_ID_NONE`] for
    /// `level_breach`. D2 as amended: any asset on any boot-universe
    /// venue — no venue restriction on either leg.
    pub ref_sym: SymbolId,
    /// Trigger threshold, basis points (§4.2 rule 3: ≤ 10 000).
    pub edge_bps: u32,
    /// Re-arm horizon (cooldown), ms (§4.2 rule 3: clamped
    /// `[10, 86_400_000]`).
    pub horizon_ms: u32,
    /// `level_breach` threshold px ×1e6; 0 for `cross_deviation`
    /// (§4.2 rule 3: `[0, 1_000_000]` — Polymarket price domain).
    pub level_1e6: i64,
    /// Per-row notional cap ×1e6 (§4.2 rule 7: ≤ the risk-policy
    /// single-order cap; tighten-only).
    pub max_risk_1e6: i64,
    /// FNV-1a 64 of the row's `name` bytes ([`fnv1a_64`]).
    pub name_h: u64,
    /// [`Self::TRIGGER_CROSS_DEVIATION`] or
    /// [`Self::TRIGGER_LEVEL_BREACH`] (§4.2, D2).
    pub trigger: u8,
    /// [`Side`] as raw byte (0/1) or [`Self::SIDE_BOTH`].
    pub side: u8,
    /// [`MarketFamily`] as raw byte — reporting only, never gates
    /// evaluation.
    pub family: u8,
    /// Explicit tail padding — always zero. Design §3's `[u8; 13]`
    /// was amended to 21 in G1 (operator-confirmed): declared fields
    /// sum to 43 B, and house doctrine forbids implicit compiler
    /// padding in pinned layouts.
    _pad: [u8; 21],
}

impl RuleRow {
    /// `trigger` byte — fire when |mid(sym) − mid(ref_sym)| in bps
    /// ≥ `edge_bps`.
    pub const TRIGGER_CROSS_DEVIATION: u8 = 0;
    /// `trigger` byte — fire when best px crosses `level_1e6` on the
    /// row's side.
    pub const TRIGGER_LEVEL_BREACH: u8 = 1;
    /// `side` byte meaning "both sides".
    pub const SIDE_BOTH: u8 = 0xFF;

    /// The all-zero row — inert filler for `rows[len..]`; never
    /// evaluated (the vm scan stops at `len`).
    pub const ZERO: Self = Self {
        sym: 0,
        ref_sym: 0,
        edge_bps: 0,
        horizon_ms: 0,
        level_1e6: 0,
        max_risk_1e6: 0,
        name_h: 0,
        trigger: 0,
        side: 0,
        family: 0,
        _pad: [0; 21],
    };

    /// Construct a row without naming the private padding field. The
    /// §4.2 validator is the only production builder; tests build
    /// fixtures through it too.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sym: SymbolId,
        ref_sym: SymbolId,
        edge_bps: u32,
        horizon_ms: u32,
        level_1e6: i64,
        max_risk_1e6: i64,
        name_h: u64,
        trigger: u8,
        side: u8,
        family: u8,
    ) -> Self {
        Self {
            sym,
            ref_sym,
            edge_bps,
            horizon_ms,
            level_1e6,
            max_risk_1e6,
            name_h,
            trigger,
            side,
            family,
            _pad: [0; 21],
        }
    }
}

// (VM2 V4: the v1 `RuleTable` retired — the §6 handoff carries
// [`RuleTableV2`] now and the validator builds v2 rows directly.
// [`RuleRow`] stays as the v1 JSON grammar's record through the D-6
// one-release compat window; `RuleRowV2::from_v1` is the sugar law.)

/// Capacity (slots) of the §6 table-handoff ring
/// `Ring<RuleTableSlot, RULE_TABLE_RING_SLOTS>` — SPSC, ingress-ai
/// side path → engine, one push per validated Stage (operator
/// cadence). 2 slots: one staged table in flight plus one
/// restage-supersede; a third undrained stage is a §5 push-full
/// REJECT at the side path (counted — impossible at operator cadence
/// against a µs-drain engine loop, counted honestly anyway). Power
/// of two per the `core-ring` contract.
pub const RULE_TABLE_RING_SLOTS: usize = 2;

// ---------------------------------------------------------------
// RuleRowV2 / RuleTableV2 — the VM2 general grammar (vm2-plan §1/§3,
// D-1…D-8 ruled + LOCKED 2026-08-29; byte layout pinned in
// docs/wire-format.md)
// ---------------------------------------------------------------

/// v2 feature selector (vm2-plan §1.1). One byte in [`RuleRowV2`];
/// wire-stable within the v2 table format — never renumber, only
/// append. `0xFF` ([`FEAT_NONE`]) = no feature (a row's `feat_c` when
/// no confirm condition exists).
///
/// Signal domain law (vm2-plan §1.2, pinned in docs/wire-format.md):
/// every evaluated feature/combine output is an `i64` in **×1e9 of
/// its natural unit** — prices in px ×1e9 (the ×1e6 tick domain
/// ×1e3), APR/IV/imbalance as fractions ×1e9, bps values as bps ×1e9,
/// notional as USD ×1e9, clock features as seconds ×1e9. Thresholds
/// (`enter_1e9`/`exit_1e9`/`confirm_1e9`) live in the same domain.
///
/// There is deliberately no `Last` (last-trade) feature: the BBO
/// carrier has no trade price and trade prints are a recorded absence
/// (vm2-plan §1.6) — it slots in as an appended feature when a trade
/// channel is captured.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FeatId {
    /// Mid price of the operand sym's top-of-book (px ×1e9).
    Mid = 0,
    /// Best bid px (×1e9).
    Bid = 1,
    /// Best ask px (×1e9).
    Ask = 2,
    /// Rolling arithmetic mean of mid over the operand window (px ×1e9).
    RollMean = 3,
    /// Rolling EMA of mid over the operand window (px ×1e9).
    RollEma = 4,
    /// Rolling minimum of mid (px ×1e9).
    RollMin = 5,
    /// Rolling maximum of mid (px ×1e9).
    RollMax = 6,
    /// Rolling population std-dev of mid (px ×1e9).
    RollStd = 7,
    /// Annualized funding APR over the trailing 24 h of prints
    /// (fraction ×1e9; the [`funding_print_divisor`] cadence law
    /// applied at accumulation). Empty window ⇒ feature ABSENT.
    Apr24 = 8,
    /// Annualized funding APR over the trailing 72 h (fraction ×1e9).
    Apr72 = 9,
    /// Options mark price from OptSummary kind 6 (raw venue units
    /// ×1e9, the OptSummary convention). Absent until first record,
    /// or when the venue supplies no mark px (flags bit0 clear).
    MarkPx = 10,
    /// Options mark IV from OptSummary (fraction ×1e9).
    MarkIv = 11,
    /// Top-K depth imbalance: (Σ bid notional − Σ ask notional) /
    /// (Σ bid + Σ ask), fraction ×1e9 in \[−1e9, 1e9\]. STALE book ⇒
    /// ABSENT (WS10-B gap law).
    DepthImb = 12,
    /// Top-of-depth spread in bps of mid (bps ×1e9), from DepthTopK.
    DepthSpreadBps = 13,
    /// Near-depth notional: Σ (px×qty) over both sides' top-K, USD
    /// ×1e9. STALE ⇒ ABSENT.
    DepthNearNotional = 14,
    /// Seconds to the sym's next funding print, ×1e9
    /// (venue-cadence-aware: venue-supplied next-funding time when
    /// the funding channel carries one, else derived from
    /// [`funding_period_s`]). ABSENT on continuous-funding venues
    /// (period 0) and before the first funding observation.
    ClockToFunding = 15,
    /// UTC seconds-of-day ×1e9 (always present).
    ClockUtcSod = 16,
}

/// [`RuleRowV2::feat_c`] value meaning "no confirm condition".
pub const FEAT_NONE: u8 = 0xFF;

impl FeatId {
    /// Decode a raw byte. `None` for unknown values — the §4.2 v2
    /// validator rejects them (rule 2).
    #[inline]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Mid),
            1 => Some(Self::Bid),
            2 => Some(Self::Ask),
            3 => Some(Self::RollMean),
            4 => Some(Self::RollEma),
            5 => Some(Self::RollMin),
            6 => Some(Self::RollMax),
            7 => Some(Self::RollStd),
            8 => Some(Self::Apr24),
            9 => Some(Self::Apr72),
            10 => Some(Self::MarkPx),
            11 => Some(Self::MarkIv),
            12 => Some(Self::DepthImb),
            13 => Some(Self::DepthSpreadBps),
            14 => Some(Self::DepthNearNotional),
            15 => Some(Self::ClockToFunding),
            16 => Some(Self::ClockUtcSod),
            _ => None,
        }
    }

    /// The feature reads the sym's BBO tick stream (books / rolling
    /// stats). Rule-10 channel arithmetic (vm2-plan §3): every
    /// universe instrument with a tick lane satisfies this.
    #[inline]
    pub const fn requires_price(self) -> bool {
        matches!(
            self,
            Self::Mid
                | Self::Bid
                | Self::Ask
                | Self::RollMean
                | Self::RollEma
                | Self::RollMin
                | Self::RollMax
                | Self::RollStd
        )
    }

    /// The feature reads the sym's funding channel (venue-event lane
    /// prints + FundingSeeds).
    #[inline]
    pub const fn requires_funding(self) -> bool {
        matches!(self, Self::Apr24 | Self::Apr72 | Self::ClockToFunding)
    }

    /// The feature reads the sym's OptSummary (kind 6) channel.
    #[inline]
    pub const fn requires_opt_summary(self) -> bool {
        matches!(self, Self::MarkPx | Self::MarkIv)
    }

    /// The feature reads the sym's DepthTopK (kind 7) channel.
    #[inline]
    pub const fn requires_depth(self) -> bool {
        matches!(
            self,
            Self::DepthImb | Self::DepthSpreadBps | Self::DepthNearNotional
        )
    }

    /// The feature takes a per-row rolling window: its operand window
    /// field must be in `[1, ROLL_WINDOW_MAX_MIN]`; every other
    /// feature's window field must be 0 (the v2 validator's rule-3
    /// window law).
    #[inline]
    pub const fn requires_window(self) -> bool {
        matches!(
            self,
            Self::RollMean | Self::RollEma | Self::RollMin | Self::RollMax | Self::RollStd
        )
    }
}

/// v2 combine operator (vm2-plan §1.2): how `feat_a(sym)` and
/// `feat_b(ref | CONST)` produce the row's signal. One byte in
/// [`RuleRowV2`]; never renumber.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CombineOp {
    /// `a − b` (same-unit operands; the signal keeps their unit ×1e9).
    Diff = 0,
    /// `(a − b) / b × 1e4`, in bps ×1e9 (i128 intermediate).
    DiffBps = 1,
    /// `a / b` as a ratio ×1e9 (i128 intermediate; b = 0 ⇒ ABSENT).
    Ratio1e9 = 2,
    /// `a` alone — the CONST-operand form: the row compares `feat_a`
    /// directly against its thresholds; `ref` must be
    /// [`SYMBOL_ID_NONE`] and `feat_b` unused.
    LhsOnly = 3,
}

impl CombineOp {
    /// Decode a raw byte. `None` = validator rule-2 reject.
    #[inline]
    pub const fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Diff),
            1 => Some(Self::DiffBps),
            2 => Some(Self::Ratio1e9),
            3 => Some(Self::LhsOnly),
            _ => None,
        }
    }
}

/// [`RuleRowV2::cmp_bits`] bit 0: entry fires on `signal ≤ enter_1e9`
/// (clear = `signal ≥ enter_1e9`).
pub const CMP_ENTRY_LE: u8 = 1 << 0;
/// [`RuleRowV2::cmp_bits`] bit 1: entry compares `|signal|` (abs
/// mode — the sym-rich/sym-cheap two-direction entry; the position
/// records the raw signal's sign as `entry_sign`).
pub const CMP_ENTRY_ABS: u8 = 1 << 1;
/// [`RuleRowV2::cmp_bits`] bit 2: confirm fires on
/// `confirm_signal ≤ confirm_1e9` (clear = `≥`).
pub const CMP_CONFIRM_LE: u8 = 1 << 2;
/// [`RuleRowV2::cmp_bits`] bit 3: confirm compares `|confirm_signal|`.
pub const CMP_CONFIRM_ABS: u8 = 1 << 3;
/// [`RuleRowV2::cmp_bits`] bit 4: the confirm signal is the row's
/// combine applied to `feat_c` across BOTH legs (`feat_c(sym) ⊕
/// feat_c(ref)` — the S1 72 h-spread confirm shape). Clear = confirm
/// evaluates `feat_c(sym)` alone (the basis funding confirm / clock
/// timing shape).
pub const CMP_CONFIRM_PAIR: u8 = 1 << 4;
/// Mask of the defined [`RuleRowV2::cmp_bits`] bits — the validator
/// rejects any set bit outside it.
pub const CMP_BITS_MASK: u8 =
    CMP_ENTRY_LE | CMP_ENTRY_ABS | CMP_CONFIRM_LE | CMP_CONFIRM_ABS | CMP_CONFIRM_PAIR;

/// [`RuleRowV2::flags`] bit 0: position mode — the row runs the
/// Flat→Entered state machine (two-leg emit when `ref` is a real sym,
/// exit law `signal × entry_sign ≤ exit_1e9`, min/max-hold honored).
/// Clear = v1 horizon-refire semantics (fire, sleep `horizon_ms`,
/// re-arm; `exit_1e9`/`min_hold_s`/`max_hold_s`/`group` must be 0 —
/// validator rule 9).
pub const ROW_FLAG_POSITION: u8 = 1 << 0;
/// Mask of the defined [`RuleRowV2::flags`] bits.
pub const ROW_FLAGS_MASK: u8 = ROW_FLAG_POSITION;

/// [`RuleRowV2::group`] value meaning "no group" (the row's position
/// is exclusive to itself alone).
pub const GROUP_NONE: u8 = 0xFF;

/// [`RuleRowV2::ver`] value of every row this workspace builds.
pub const RULE_ROW_VER_2: u8 = 2;

/// Upper bound on rolling-stat windows, minutes (3 days; vm2-plan
/// §1.1). Also the longest window the backtest warmup must cover.
pub const ROLL_WINDOW_MAX_MIN: u16 = 4320;

/// One validated v2 rule — 128 bytes, exactly two cache lines
/// (vm2-plan §3, D-5; byte layout pinned in docs/wire-format.md).
///
/// POD; **built** by the v2 §4.2 validator from the JSON artifact —
/// never parsed from wire bytes, never captured (no `AsBytes`; the
/// JSON artifact is the durable form, identity is the table's
/// `hash128`). v1 sugar (`level_breach` / `cross_deviation`) maps
/// FULLY onto this shape at build time with byte-exact v1 semantics:
///
/// * `level_breach` bid row → `LhsOnly(Ask) ≤ level` (watch the
///   transact price), ask row → `LhsOnly(Bid) ≥ level`, `both` →
///   two-arm check bid-leg-first; horizon-refire mode.
/// * `cross_deviation` → `|DiffBps(Mid, Mid)| ≥ edge_bps` with the
///   mean-reverting direction law and `side` as filter;
///   horizon-refire mode.
///
/// The `edge_bps` field is a diagnostic mirror for sugar rows (the
/// live threshold is `enter_1e9`); 0 on native-grammar rows.
#[derive(Copy, Clone)]
#[repr(C, align(64))]
pub struct RuleRowV2 {
    /// Row format version — always [`RULE_ROW_VER_2`].
    pub ver: u8,
    /// [`ROW_FLAG_POSITION`] et al ([`ROW_FLAGS_MASK`]).
    pub flags: u8,
    /// [`Side`] byte (0/1) or `0xFF` = both. For `LhsOnly` rows the
    /// EMITTED side (v1 law); for signal-signed rows a direction
    /// FILTER (v1 cross-deviation law).
    pub side: u8,
    /// Exclusivity group: rows sharing a group hold AT MOST ONE
    /// position (first qualifying row in table order enters while the
    /// group is flat). [`GROUP_NONE`] = ungrouped.
    pub group: u8,
    /// [`FeatId`] of the action-sym operand.
    pub feat_a: u8,
    /// [`FeatId`] of the reference operand (`ref`); unused (0) for
    /// [`CombineOp::LhsOnly`].
    pub feat_b: u8,
    /// [`FeatId`] of the confirm condition, or [`FEAT_NONE`].
    pub feat_c: u8,
    /// [`CombineOp`] byte.
    pub combine: u8,
    /// Action-leg [`SymbolId`] (resolved from the artifact's
    /// descriptor at commit — vm2-plan §1.4/D-6).
    pub sym: SymbolId,
    /// Reference-leg [`SymbolId`], or [`SYMBOL_ID_NONE`] for
    /// `LhsOnly` (CONST-operand) rows.
    pub ref_sym: SymbolId,
    /// Rolling window of `feat_a`, minutes (`[1, 4320]` when
    /// `feat_a.requires_window()`, else 0).
    pub win_a: u16,
    /// Rolling window of `feat_b`, minutes (same law).
    pub win_b: u16,
    /// Rolling window of `feat_c`, minutes (same law).
    pub win_c: u16,
    /// [`CMP_ENTRY_LE`] … [`CMP_CONFIRM_PAIR`] ([`CMP_BITS_MASK`]).
    pub cmp_bits: u8,
    /// Explicit padding — always zero.
    _pad0: u8,
    /// Entry threshold, signal-domain ×1e9 (see [`FeatId`] docs).
    pub enter_1e9: i64,
    /// Exit threshold (position mode): exit fires when
    /// `signal × entry_sign ≤ exit_1e9` on the HELD position — the
    /// universal reversion law (covers |signal| decay AND sign flip).
    /// 0 with flags 0 = v1 refire semantics (rule 9).
    pub exit_1e9: i64,
    /// Confirm threshold, signal-domain ×1e9; 0 when `feat_c` is
    /// [`FEAT_NONE`].
    pub confirm_1e9: i64,
    /// Minimum hold seconds before exit evaluates (position mode).
    pub min_hold_s: u32,
    /// Re-arm horizon ms (refire mode: v1 law; position mode:
    /// cooldown between an exit and the row's next entry).
    pub horizon_ms: u32,
    /// v1-sugar diagnostic mirror of the bps threshold; 0 on
    /// native-grammar rows (module docs).
    pub edge_bps: u32,
    /// Explicit padding — always zero.
    _pad1: u32,
    /// Per-row notional cap ×1e6 (§4.2 rule 7; per LEG in position
    /// mode — a two-leg entry emits `max_risk` per leg, opposite
    /// sides, equal notional).
    pub max_risk_1e6: i64,
    /// FNV-1a 64 of the row's `name` bytes ([`fnv1a_64`]).
    pub name_h: u64,
    /// Maximum hold seconds (position mode): the age-out exit — the
    /// S1 `age > 10 d` law (vm2-plan V0 freeze: allocated from the §3
    /// reserved space). 0 = no age-out.
    pub max_hold_s: u32,
    /// [`MarketFamily`] byte — reporting only.
    pub family: u8,
    /// Explicit padding — always zero.
    _pad2: [u8; 3],
    /// Explicit tail padding / reserved — always zero.
    _pad3: [u8; 40],
}

impl RuleRowV2 {
    /// The all-zero row — inert filler for `rows[len..]`; never
    /// evaluated (`ver` 0 marks it non-built).
    pub const ZERO: Self = Self {
        ver: 0,
        flags: 0,
        side: 0,
        group: 0,
        feat_a: 0,
        feat_b: 0,
        feat_c: 0,
        combine: 0,
        sym: 0,
        ref_sym: 0,
        win_a: 0,
        win_b: 0,
        win_c: 0,
        cmp_bits: 0,
        _pad0: 0,
        enter_1e9: 0,
        exit_1e9: 0,
        confirm_1e9: 0,
        min_hold_s: 0,
        horizon_ms: 0,
        edge_bps: 0,
        _pad1: 0,
        max_risk_1e6: 0,
        name_h: 0,
        max_hold_s: 0,
        family: 0,
        _pad2: [0; 3],
        _pad3: [0; 40],
    };

    /// Map one v1 row onto the grammar with byte-exact v1 semantics
    /// (the struct docs' sugar law). V3 uses this at the vm's v1
    /// table-receive seam; V4 moves the call into the validator's
    /// compat arm and retires the v1 types.
    ///
    /// * `level_breach` → `LhsOnly` refire row: the threshold is
    ///   `level ×1e3` (px 1e6 → signal 1e9 domain). Bid rows watch
    ///   the ASK (`feat_a = Ask`, `≤`), Ask rows the BID (`≥`); a
    ///   SIDE_BOTH row keeps `feat_a = Ask` and the evaluator's
    ///   documented both-leg arm checks bid-leg-first (v1 order).
    /// * `cross_deviation` → `|DiffBps(Mid, Mid)| ≥ edge_bps` with
    ///   the mean-reverting direction law; `side` stays the filter.
    pub const fn from_v1(r: &RuleRow) -> Self {
        if r.trigger == RuleRow::TRIGGER_LEVEL_BREACH {
            let (feat_a, cmp) = if r.side == Side::Ask as u8 {
                (FeatId::Bid, 0u8) // sell at/above: bid ≥ level
            } else {
                // Bid rows AND SIDE_BOTH (both-leg arm keys off
                // side byte + LhsOnly; bid leg first = v1 order).
                (FeatId::Ask, CMP_ENTRY_LE) // buy at/below: ask ≤ level
            };
            Self::new(
                0, // refire mode
                r.side,
                GROUP_NONE,
                feat_a,
                FeatId::Mid, // unused for LhsOnly
                FEAT_NONE,
                CombineOp::LhsOnly,
                r.sym,
                SYMBOL_ID_NONE,
                0,
                0,
                0,
                cmp,
                r.level_1e6 * 1_000,
                0,
                0,
                0,
                r.horizon_ms,
                r.edge_bps,
                r.max_risk_1e6,
                r.name_h,
                0,
                r.family,
            )
        } else {
            Self::new(
                0, // refire mode
                r.side,
                GROUP_NONE,
                FeatId::Mid,
                FeatId::Mid,
                FEAT_NONE,
                CombineOp::DiffBps,
                r.sym,
                r.ref_sym,
                0,
                0,
                0,
                CMP_ENTRY_ABS, // |dev| ≥ edge, direction from sign
                (r.edge_bps as i64) * 1_000_000_000,
                0,
                0,
                0,
                r.horizon_ms,
                r.edge_bps,
                r.max_risk_1e6,
                r.name_h,
                0,
                r.family,
            )
        }
    }

    /// Construct a row without naming the private padding fields. The
    /// v2 §4.2 validator is the only production builder; tests build
    /// fixtures through it too. `ver` is stamped [`RULE_ROW_VER_2`].
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        flags: u8,
        side: u8,
        group: u8,
        feat_a: FeatId,
        feat_b: FeatId,
        feat_c: u8,
        combine: CombineOp,
        sym: SymbolId,
        ref_sym: SymbolId,
        win_a: u16,
        win_b: u16,
        win_c: u16,
        cmp_bits: u8,
        enter_1e9: i64,
        exit_1e9: i64,
        confirm_1e9: i64,
        min_hold_s: u32,
        horizon_ms: u32,
        edge_bps: u32,
        max_risk_1e6: i64,
        name_h: u64,
        max_hold_s: u32,
        family: u8,
    ) -> Self {
        Self {
            ver: RULE_ROW_VER_2,
            flags,
            side,
            group,
            feat_a: feat_a as u8,
            feat_b: feat_b as u8,
            feat_c,
            combine: combine as u8,
            sym,
            ref_sym,
            win_a,
            win_b,
            win_c,
            cmp_bits,
            _pad0: 0,
            enter_1e9,
            exit_1e9,
            confirm_1e9,
            min_hold_s,
            horizon_ms,
            edge_bps,
            _pad1: 0,
            max_risk_1e6,
            name_h,
            max_hold_s,
            family,
            _pad2: [0; 3],
            _pad3: [0; 40],
        }
    }
}

/// The v2 engine-facing rule table — 32 KiB of rows plus one trailing
/// metadata cache line, 32 832 B total (vm2-plan §3, D-5; layout
/// pinned in docs/wire-format.md). In-memory POD only, identical
/// contract to the retired v1 table: never crosses a process
/// boundary, identity is `hash128`, ferried ingress→engine by value
/// via `Ring<RuleTableV2Slot, RULE_TABLE_RING_SLOTS>` (the two
/// documented copies grow to 32 KiB + 64 each — operator cadence).
#[derive(Copy, Clone)]
#[repr(C, align(64))]
pub struct RuleTableV2 {
    /// Row storage; only `rows[..len as usize]` is meaningful.
    pub rows: [RuleRowV2; RULE_TABLE_ROWS],
    /// Validated row count ≤ [`RULE_TABLE_ROWS`].
    pub len: u32,
    /// Side-path monotonic stage counter (diagnostics).
    pub epoch: u32,
    /// Identity — first 16 bytes of the full SHA-256 of the JSON
    /// artifact bytes.
    pub hash128: [u8; 16],
    /// Explicit tail padding — always zero.
    _pad: [u8; 40],
}

impl RuleTableV2 {
    /// The empty table: no rows, epoch 0, zero hash. Boot/scratch
    /// preallocation value — built once, reused forever.
    pub const EMPTY: Self = Self {
        rows: [RuleRowV2::ZERO; RULE_TABLE_ROWS],
        len: 0,
        epoch: 0,
        hash128: [0; 16],
        _pad: [0; 40],
    };

}

/// Ring slot ferrying a staged table ingress→engine (§6; VM2 V4:
/// v2-typed — 32 832 B by value at operator cadence).
pub type RuleTableSlot = RuleTableV2;

// ---------------------------------------------------------------
// Funding cadence law — the single home (vm2-plan §1.1, R4-§9)
// ---------------------------------------------------------------

/// Divisor applied to every funding print when accumulating a window
/// sum, per venue — THE single home of the R4-§9 unit law: Deribit's
/// funding rows are HOURLY SAMPLES of an 8-hour rolling
/// `interest_8h`, so summing them over-counts 8× and every Deribit
/// print divides by 8; every other venue's prints are settled
/// per-print rates that sum directly. Used by the VM feature engine,
/// the backtest harness, and (via pin tests) mirrored by
/// `claude_worker.carry_signal.apr_from_prints`.
#[inline(always)]
pub const fn funding_print_divisor(venue: VenueId) -> i64 {
    match venue {
        VenueId::Deribit => 8,
        _ => 1,
    }
}

/// Nominal funding-print period per venue, seconds — the
/// [`FeatId::ClockToFunding`] fallback when the venue's funding
/// channel supplies no explicit next-funding time (OKX and Binance
/// supply one; Bybit's per-symbol interval can differ from the 8 h
/// default, so the venue-supplied time wins whenever present).
/// 0 = continuous funding (Deribit — no discrete print; the clock
/// feature is ABSENT) or no funding at all (PM, AI).
#[inline(always)]
pub const fn funding_period_s(venue: VenueId) -> u32 {
    match venue {
        VenueId::Binance | VenueId::Okx | VenueId::Bybit => 28_800,
        VenueId::Hyperliquid => 3_600,
        VenueId::Polymarket | VenueId::Deribit | VenueId::Ai => 0,
    }
}

/// The [`FeatId::Apr24`] trailing window, minutes.
pub const FUNDING_WINDOW_24H_MIN: u16 = 1_440;
/// The [`FeatId::Apr72`] trailing window, minutes.
pub const FUNDING_WINDOW_72H_MIN: u16 = 4_320;

// ---------------------------------------------------------------
// Static layout assertions — if these ever fire, revisit the
// cache-line story in PLAN.md §7.
// ---------------------------------------------------------------

/// Compile-time size check. Invoking this macro on `(T, N)` aborts the
/// build if `size_of::<T>() != N`.
#[macro_export]
macro_rules! static_assert_size {
    ($t:ty, $n:expr) => {
        const _: [(); $n] = [(); ::core::mem::size_of::<$t>()];
    };
}

static_assert_size!(Tick, 64);
static_assert_size!(Signal, 64);
static_assert_size!(Fill, 64);
static_assert_size!(Order, 64);
static_assert_size!(ChannelEvent, 64);
static_assert_size!(AiCmd, 64);
static_assert_size!(RuleRow, 64);

// AiCmd byte layout is a cross-process wire contract (Python packs it,
// docs/wire-format.md pins it) — every field offset is asserted at
// compile time, same spirit as the `Tick` offset checks in tests but
// build-breaking.
const _: () = {
    assert!(::core::mem::offset_of!(AiCmd, ts_ns) == 0);
    assert!(::core::mem::offset_of!(AiCmd, seq) == 8);
    assert!(::core::mem::offset_of!(AiCmd, sym) == 12);
    assert!(::core::mem::offset_of!(AiCmd, px) == 16);
    assert!(::core::mem::offset_of!(AiCmd, qty) == 24);
    assert!(::core::mem::offset_of!(AiCmd, ttl_ns) == 32);
    assert!(::core::mem::offset_of!(AiCmd, kind) == 40);
    assert!(::core::mem::offset_of!(AiCmd, venue) == 41);
    assert!(::core::mem::offset_of!(AiCmd, strategy_id) == 42);
    assert!(::core::mem::offset_of!(AiCmd, side) == 43);
    assert!(::core::mem::offset_of!(AiCmd, param_id) == 44);
    assert!(::core::mem::offset_of!(AiCmd, flags) == 46);
    assert!(::core::mem::offset_of!(AiCmd, _pad) == 48);
    assert!(AI_RING_SIZE.is_power_of_two());
};

// RuleRow / RuleTable layout is pinned in `docs/wire-format.md` (8g
// §3) — offsets asserted build-breaking, same spirit as AiCmd above
// (in-process only, but the vm evaluator and the wire-format doc both
// cite these offsets).
const _: () = {
    assert!(::core::mem::offset_of!(RuleRow, sym) == 0);
    assert!(::core::mem::offset_of!(RuleRow, ref_sym) == 4);
    assert!(::core::mem::offset_of!(RuleRow, edge_bps) == 8);
    assert!(::core::mem::offset_of!(RuleRow, horizon_ms) == 12);
    assert!(::core::mem::offset_of!(RuleRow, level_1e6) == 16);
    assert!(::core::mem::offset_of!(RuleRow, max_risk_1e6) == 24);
    assert!(::core::mem::offset_of!(RuleRow, name_h) == 32);
    assert!(::core::mem::offset_of!(RuleRow, trigger) == 40);
    assert!(::core::mem::offset_of!(RuleRow, side) == 41);
    assert!(::core::mem::offset_of!(RuleRow, family) == 42);
    assert!(::core::mem::offset_of!(RuleRow, _pad) == 43);
    assert!(RULE_TABLE_RING_SLOTS.is_power_of_two());
};

// RuleRowV2 / RuleTableV2 layout is pinned in docs/wire-format.md
// (vm2-plan §3, D-5) — offsets asserted build-breaking, same spirit
// as the v1 block above.
static_assert_size!(RuleRowV2, 128);
static_assert_size!(RuleTableV2, 32 * 1024 + 64);
const _: () = {
    assert!(::core::mem::align_of::<RuleRowV2>() == 64);
    assert!(::core::mem::offset_of!(RuleRowV2, ver) == 0);
    assert!(::core::mem::offset_of!(RuleRowV2, flags) == 1);
    assert!(::core::mem::offset_of!(RuleRowV2, side) == 2);
    assert!(::core::mem::offset_of!(RuleRowV2, group) == 3);
    assert!(::core::mem::offset_of!(RuleRowV2, feat_a) == 4);
    assert!(::core::mem::offset_of!(RuleRowV2, feat_b) == 5);
    assert!(::core::mem::offset_of!(RuleRowV2, feat_c) == 6);
    assert!(::core::mem::offset_of!(RuleRowV2, combine) == 7);
    assert!(::core::mem::offset_of!(RuleRowV2, sym) == 8);
    assert!(::core::mem::offset_of!(RuleRowV2, ref_sym) == 12);
    assert!(::core::mem::offset_of!(RuleRowV2, win_a) == 16);
    assert!(::core::mem::offset_of!(RuleRowV2, win_b) == 18);
    assert!(::core::mem::offset_of!(RuleRowV2, win_c) == 20);
    assert!(::core::mem::offset_of!(RuleRowV2, cmp_bits) == 22);
    assert!(::core::mem::offset_of!(RuleRowV2, _pad0) == 23);
    assert!(::core::mem::offset_of!(RuleRowV2, enter_1e9) == 24);
    assert!(::core::mem::offset_of!(RuleRowV2, exit_1e9) == 32);
    assert!(::core::mem::offset_of!(RuleRowV2, confirm_1e9) == 40);
    assert!(::core::mem::offset_of!(RuleRowV2, min_hold_s) == 48);
    assert!(::core::mem::offset_of!(RuleRowV2, horizon_ms) == 52);
    assert!(::core::mem::offset_of!(RuleRowV2, edge_bps) == 56);
    assert!(::core::mem::offset_of!(RuleRowV2, _pad1) == 60);
    assert!(::core::mem::offset_of!(RuleRowV2, max_risk_1e6) == 64);
    assert!(::core::mem::offset_of!(RuleRowV2, name_h) == 72);
    assert!(::core::mem::offset_of!(RuleRowV2, max_hold_s) == 80);
    assert!(::core::mem::offset_of!(RuleRowV2, family) == 84);
    assert!(::core::mem::offset_of!(RuleRowV2, _pad2) == 85);
    assert!(::core::mem::offset_of!(RuleRowV2, _pad3) == 88);
    assert!(::core::mem::offset_of!(RuleTableV2, rows) == 0);
    assert!(::core::mem::offset_of!(RuleTableV2, len) == 32 * 1024);
    assert!(::core::mem::offset_of!(RuleTableV2, epoch) == 32 * 1024 + 4);
    assert!(::core::mem::offset_of!(RuleTableV2, hash128) == 32 * 1024 + 8);
    assert!(::core::mem::offset_of!(RuleTableV2, _pad) == 32 * 1024 + 24);
    // The FundingSeed/PositionSeed row-index bound and the v2 window
    // laws are wire facts, not tunables.
    assert!(FUNDING_WINDOW_72H_MIN == ROLL_WINDOW_MAX_MIN);
    assert!(CMP_BITS_MASK == 0b0001_1111);
    assert!(ROW_FLAGS_MASK == 0b0000_0001);
};

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<Tick>(), 64);
        assert_eq!(::core::mem::align_of::<Tick>(), 64);
    }

    #[test]
    fn signal_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<Signal>(), 64);
        assert_eq!(::core::mem::align_of::<Signal>(), 64);
    }

    #[test]
    fn fill_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<Fill>(), 64);
        assert_eq!(::core::mem::align_of::<Fill>(), 64);
    }

    #[test]
    fn order_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<Order>(), 64);
        assert_eq!(::core::mem::align_of::<Order>(), 64);
    }

    #[test]
    fn tick_mid_and_spread_are_correct() {
        let t = Tick::new(
            1_000,
            VenueId::Polymarket,
            7,
            42,
            Price::from_raw(500_000),
            Qty::from_raw(100),
            Price::from_raw(510_000),
            Qty::from_raw(50),
        );
        assert_eq!(t.mid(), Price::from_raw(505_000));
        assert_eq!(t.spread(), 10_000);
        assert_eq!(t.venue, VenueId::Polymarket.to_u8());
    }

    #[test]
    fn tick_mid_of_empty_book_is_zero() {
        // Happy-path AND failure-mode coverage as required by §21.1 of PLAN.md.
        let t = Tick::new(
            0,
            VenueId::Polymarket,
            SYMBOL_ID_NONE,
            0,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        );
        assert_eq!(t.mid(), Price::from_raw(0));
        assert_eq!(t.spread(), 0);
    }

    // ---- Phase 8: VenueId + SymbolId namespacing ----

    #[test]
    fn venue_id_roundtrips_through_u8() {
        let all = [
            VenueId::Polymarket,
            VenueId::Binance,
            VenueId::Okx,
            VenueId::Deribit,
            VenueId::Hyperliquid,
            VenueId::Ai,
            VenueId::Bybit,
        ];
        let mut i = 0;
        while i < all.len() {
            assert_eq!(VenueId::from_u8(all[i].to_u8()), Some(all[i]));
            i += 1;
        }
    }

    #[test]
    fn venue_id_rejects_unknown_bytes() {
        // 6 became Bybit at WS9 — the first unassigned byte is now 7.
        assert_eq!(VenueId::from_u8(7), None);
        assert_eq!(VenueId::from_u8(254), None);
        // 255 is the venue byte of SYMBOL_ID_NONE — must never decode.
        assert_eq!(VenueId::from_u8(255), None);
        assert_eq!(VenueId::from_u8(symbol_venue_byte(SYMBOL_ID_NONE)), None);
    }

    #[test]
    fn symbol_id_namespacing_roundtrips() {
        let sym = make_symbol_id(VenueId::Deribit, 0x00AB_CDEF);
        assert_eq!(symbol_venue_byte(sym), VenueId::Deribit.to_u8());
        assert_eq!(symbol_ordinal(sym), 0x00AB_CDEF);
        // Ordinal 0 on venue 0 is a valid, non-sentinel id.
        let zero = make_symbol_id(VenueId::Polymarket, 0);
        assert_eq!(zero, 0);
        assert_ne!(zero, SYMBOL_ID_NONE);
    }

    #[test]
    fn symbol_bucket_mix_separates_ordinal_zero_across_venues() {
        // The old `sym & 63` scheme put every venue's ordinal-0
        // symbol in bucket 0. The mixed scheme must not.
        let pm = make_symbol_id(VenueId::Polymarket, 0);
        let okx = make_symbol_id(VenueId::Okx, 0);
        let hl = make_symbol_id(VenueId::Hyperliquid, 0);
        let b_pm = symbol_bucket_mix(pm) & 63;
        let b_okx = symbol_bucket_mix(okx) & 63;
        let b_hl = symbol_bucket_mix(hl) & 63;
        assert_ne!(b_pm, b_okx);
        assert_ne!(b_pm, b_hl);
        assert_ne!(b_okx, b_hl);
    }

    #[test]
    fn venue_bytes_sit_at_documented_offsets() {
        // docs/wire-format.md pins Tick.venue at offset 48 and
        // Order.venue at offset 40. Byte-level check through AsBytes.
        let t = Tick::new(
            0,
            VenueId::Hyperliquid,
            make_symbol_id(VenueId::Hyperliquid, 9),
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
        );
        // SAFETY: Tick is AsBytes (repr(C), Copy, fully-initialized);
        // read-only byte view of a live stack value.
        let tb = unsafe { core::slice::from_raw_parts((&t as *const Tick).cast::<u8>(), 64) };
        assert_eq!(tb[48], VenueId::Hyperliquid.to_u8());
        // Explicit tail padding must be zero.
        let mut i = 49;
        while i < 64 {
            assert_eq!(tb[i], 0);
            i += 1;
        }

        let o = Order::new(
            0,
            VenueId::Okx,
            make_symbol_id(VenueId::Okx, 3),
            Side::Ask,
            0,
            Price::from_raw(1),
            Qty::from_raw(1),
            7,
        );
        // SAFETY: Order is AsBytes; same argument as above.
        let ob = unsafe { core::slice::from_raw_parts((&o as *const Order).cast::<u8>(), 64) };
        assert_eq!(ob[40], VenueId::Okx.to_u8());
        // M4.1: offset 41 = strategy_id, STRATEGY_ID_NONE by default.
        assert_eq!(ob[41], STRATEGY_ID_NONE);
        let mut i = 42;
        while i < 64 {
            assert_eq!(ob[i], 0);
            i += 1;
        }
    }

    #[test]
    fn layout_is_fully_explicit() {
        // Sum of declared field widths must equal size_of — any
        // compiler-inserted padding would break the AsBytes contract.
        // Tick: 8+4+4+8+8+8+8+1+1+6+8 = 64 (VT1: +flags, +venue_time_ms).
        // Signal: 8+4+1+1+2+40+8 = 64.
        // Fill: 8+4+1+3+8+8+8+16+8 = 64.
        // Order: 8+4+1+1+2+8+8+8+1+1+14+8 = 64 (M4.1: +strategy_id).
        assert_eq!(::core::mem::size_of::<Tick>(), 64);
        assert_eq!(::core::mem::size_of::<Signal>(), 64);
        assert_eq!(::core::mem::size_of::<Fill>(), 64);
        assert_eq!(::core::mem::size_of::<Order>(), 64);
    }

    #[test]
    fn opt_summary_layout_is_fully_explicit() {
        // OptSummary: 8+4+1+1+2+8+8+8+8+4+4+4+4 = 64 (M2.3).
        assert_eq!(::core::mem::size_of::<OptSummary>(), 64);
        // Field offsets are the docs/wire-format.md pinned law.
        let o = OptSummary::new(
            1,
            VenueId::Deribit,
            2,
            OPT_SUMMARY_FLAG_MARK_PX | OPT_SUMMARY_FLAG_OI,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
        );
        let base = &o as *const OptSummary as usize;
        assert_eq!(&o.ts_ns as *const _ as usize - base, 0);
        assert_eq!(&o.sym as *const _ as usize - base, 8);
        assert_eq!(&o.venue as *const _ as usize - base, 12);
        assert_eq!(&o.flags as *const _ as usize - base, 13);
        assert_eq!(&o.mark_px_1e9 as *const _ as usize - base, 16);
        assert_eq!(&o.mark_iv_1e9 as *const _ as usize - base, 24);
        assert_eq!(&o.underlying_px_1e9 as *const _ as usize - base, 32);
        assert_eq!(&o.open_interest_1e6 as *const _ as usize - base, 40);
        assert_eq!(&o.delta_1e9 as *const _ as usize - base, 48);
        assert_eq!(&o.gamma_1e9 as *const _ as usize - base, 52);
        assert_eq!(&o.vega_1e6 as *const _ as usize - base, 56);
        assert_eq!(&o.theta_1e6 as *const _ as usize - base, 60);
    }

    #[test]
    fn tick_v3_layout_offsets_are_the_wire_format_law() {
        // docs/wire-format.md `Tick` v3: flags at 49, pad 50..56,
        // venue_time_ms at 56 (naturally aligned u64), 64 B total.
        let t = Tick::new_stamped(
            1,
            VenueId::Okx,
            2,
            3,
            Price::from_raw(4),
            Qty::from_raw(5),
            Price::from_raw(6),
            Qty::from_raw(7),
            1_700_000_000_123,
            TICK_FLAG_STALE | TICK_FLAG_VENUE_TIME_SENTINEL,
        );
        let base = &t as *const Tick as usize;
        assert_eq!(&t.ts_ns as *const _ as usize - base, 0);
        assert_eq!(&t.sym as *const _ as usize - base, 8);
        assert_eq!(&t.venue_seq as *const _ as usize - base, 12);
        assert_eq!(&t.bid_px as *const _ as usize - base, 16);
        assert_eq!(&t.bid_qty as *const _ as usize - base, 24);
        assert_eq!(&t.ask_px as *const _ as usize - base, 32);
        assert_eq!(&t.ask_qty as *const _ as usize - base, 40);
        assert_eq!(&t.venue as *const _ as usize - base, 48);
        assert_eq!(&t.flags as *const _ as usize - base, 49);
        assert_eq!(&t.venue_time_ms as *const _ as usize - base, 56);
        assert_eq!(t.venue_time_ms, 1_700_000_000_123);
        assert_eq!(t.flags, 3);
        assert!(t.is_stale());
        // The v2 bytes 50..56 stay zero so a v3 slot written by this
        // constructor is byte-stable (AsBytes contract).
        // SAFETY: Tick is AsBytes — repr(C, align(64)), 64 B, every byte
        // initialized (the `layout_is_fully_explicit` law) — so viewing
        // it as `[u8; 64]` is exactly what the PMLR writer does.
        let bytes: [u8; 64] = unsafe { ::core::mem::transmute(t) };
        assert_eq!(&bytes[50..56], &[0u8; 6]);
        assert_eq!(&bytes[56..64], &1_700_000_000_123u64.to_le_bytes());
        assert_eq!(bytes[49], 3);
    }

    #[test]
    fn tick_new_is_the_v2_shape_venue_time_unknown_and_fresh() {
        let t = Tick::new(
            9,
            VenueId::Binance,
            7,
            1,
            Price::from_raw(100),
            Qty::from_raw(1),
            Price::from_raw(101),
            Qty::from_raw(1),
        );
        assert_eq!(t.venue_time_ms, 0);
        assert_eq!(t.flags, 0);
        assert!(!t.is_stale());
        // Tail bytes 49..64 all zero — identical to the v2 encoding.
        // SAFETY: same AsBytes argument as
        // `tick_v3_layout_offsets_are_the_wire_format_law`.
        let bytes: [u8; 64] = unsafe { ::core::mem::transmute(t) };
        assert_eq!(&bytes[49..64], &[0u8; 15]);
    }

    #[test]
    fn default_stale_after_ms_is_the_measured_table_with_the_binance_cap() {
        assert_eq!(VenueId::Binance.default_stale_after_ms(), 1_000);
        assert_eq!(VenueId::Okx.default_stale_after_ms(), 400);
        assert_eq!(VenueId::Bybit.default_stale_after_ms(), 500);
        assert_eq!(VenueId::Deribit.default_stale_after_ms(), 600);
        assert_eq!(VenueId::Hyperliquid.default_stale_after_ms(), 700);
        assert_eq!(VenueId::Polymarket.default_stale_after_ms(), 1_000);
        assert_eq!(VenueId::Ai.default_stale_after_ms(), 0);
    }

    #[test]
    fn stale_after_ms_defaults_is_indexed_by_the_venue_byte() {
        let table = VenueId::stale_after_ms_defaults();
        let mut b = 0u8;
        while (b as usize) < table.len() {
            let venue = VenueId::from_u8(b).expect("every index is a venue");
            assert_eq!(table[b as usize], venue.default_stale_after_ms(), "byte {b}");
            b += 1;
        }
        // One past the table is not a venue: the table is exactly the
        // venue-byte space, nothing more.
        assert!(VenueId::from_u8(table.len() as u8).is_none());
    }

    #[test]
    fn tick_is_stale_reads_only_bit0() {
        let t = Tick::new_stamped(
            1,
            VenueId::Bybit,
            2,
            3,
            Price::from_raw(4),
            Qty::from_raw(5),
            Price::from_raw(6),
            Qty::from_raw(7),
            42,
            TICK_FLAG_VENUE_TIME_SENTINEL,
        );
        assert!(!t.is_stale());
        assert_eq!(t.flags & TICK_FLAG_VENUE_TIME_SENTINEL, TICK_FLAG_VENUE_TIME_SENTINEL);
    }

    #[test]
    fn opt_summary_greeks_saturate() {
        assert_eq!(sat_i32(0), 0);
        assert_eq!(sat_i32(-1_000_000_000), -1_000_000_000);
        assert_eq!(sat_i32(i64::MAX), i32::MAX);
        assert_eq!(sat_i32(i64::MIN), i32::MIN);
        let o = OptSummary::new(
            1,
            VenueId::Okx,
            2,
            0,
            0,
            650_000_000,
            0,
            0,
            10_000_000_000, // delta overflow → saturates
            5,
            5,
            -10_000_000_000,
        );
        assert_eq!(o.delta_1e9, i32::MAX);
        assert_eq!(o.theta_1e6, i32::MIN);
        assert_eq!(o.flags, 0);
    }

    #[test]
    fn price_and_qty_are_transparent_i64() {
        assert_eq!(::core::mem::size_of::<Price>(), 8);
        assert_eq!(::core::mem::size_of::<Qty>(), 8);
    }

    #[test]
    fn side_and_class_are_one_byte() {
        assert_eq!(::core::mem::size_of::<Side>(), 1);
        assert_eq!(::core::mem::size_of::<LatencyClass>(), 1);
        assert_eq!(::core::mem::size_of::<MarketFamily>(), 1);
    }
}

#[cfg(test)]
mod channel_event_tests {
    use super::*;

    #[test]
    fn channel_event_layout_is_fully_explicit() {
        // ChannelEvent: 8+4+1+1+2+8+8+8+8+16 = 64 — any compiler-
        // inserted padding would break the AsBytes contract.
        assert_eq!(::core::mem::size_of::<ChannelEvent>(), 64);
        assert_eq!(::core::mem::align_of::<ChannelEvent>(), 64);
    }

    #[test]
    fn depth_top_k_layout_is_fully_explicit() {
        // WS10-B: 8+4+1+1+1+1 + 5×16 + 5×16 + 16 = 192 — any
        // compiler-inserted padding would break the AsBytes contract.
        assert_eq!(::core::mem::size_of::<DepthTopK>(), 192);
        assert_eq!(::core::mem::align_of::<DepthTopK>(), 64);
        assert_eq!(::core::mem::size_of::<DepthLevel>(), 16);
    }

    #[test]
    fn depth_top_k_new_and_empty_shape() {
        let mut bids = [DepthLevel::EMPTY; DEPTH_K];
        bids[0] = DepthLevel {
            px_1e6: 65_000_010_000,
            qty_1e6: 1_234_000,
        };
        let d = DepthTopK::new(
            7,
            VenueId::Okx,
            make_symbol_id(VenueId::Okx, 1),
            0,
            bids,
            [DepthLevel::EMPTY; DEPTH_K],
        );
        assert_eq!(d.k, DEPTH_K as u8);
        assert_eq!(d.venue, VenueId::Okx as u8);
        assert_eq!(d.bids[0].px_1e6, 65_000_010_000);
        assert_eq!(d.bids[1], DepthLevel::EMPTY);
        assert_eq!(d.flags, 0);
        // Failure-mode shape: EMPTY is all-zero and byte-comparable.
        let e = DepthTopK::EMPTY;
        assert_eq!(e.ts_ns, 0);
        assert_eq!(e.bids[DEPTH_K - 1], DepthLevel::EMPTY);
    }

    #[test]
    fn channel_event_bytes_sit_at_documented_offsets() {
        // docs/wire-format.md pins venue at offset 12 and channel at
        // offset 13. Byte-level check through AsBytes.
        let e = ChannelEvent::new(
            0x1111_2222_3333_4444,
            VenueId::Deribit,
            ChannelId::Ticker,
            make_symbol_id(VenueId::Deribit, 7),
            0xAABB_CCDD_EEFF_0011,
            1_700_000_000_000,
            42,
            -42,
        );
        // SAFETY: ChannelEvent is AsBytes (repr(C), Copy, fully
        // initialized); read-only byte view of a live stack value.
        let b =
            unsafe { core::slice::from_raw_parts((&e as *const ChannelEvent).cast::<u8>(), 64) };
        assert_eq!(b[12], VenueId::Deribit.to_u8());
        assert_eq!(b[13], ChannelId::Ticker as u8);
        // Reserved + tail padding must be zero.
        assert_eq!(b[14], 0);
        assert_eq!(b[15], 0);
        let mut i = 48;
        while i < 64 {
            assert_eq!(b[i], 0);
            i += 1;
        }
        // venue_seq at offset 16, little-endian.
        let mut seq = [0u8; 8];
        seq.copy_from_slice(&b[16..24]);
        assert_eq!(u64::from_le_bytes(seq), 0xAABB_CCDD_EEFF_0011);
    }

    #[test]
    fn channel_id_roundtrips_and_rejects_unknown() {
        let all = [
            ChannelId::Trade,
            ChannelId::Book,
            ChannelId::Mark,
            ChannelId::Funding,
            ChannelId::Ticker,
            ChannelId::AssetCtx,
            ChannelId::AllMids,
            ChannelId::OutcomeMeta,
            ChannelId::PriceChange,
            ChannelId::TradeGap,
            ChannelId::BookGap,
            ChannelId::SubDrop,
            ChannelId::VolIndex,
        ];
        let mut i = 0;
        while i < all.len() {
            let c = all[i];
            assert_eq!(ChannelId::from_u8(c as u8), Some(c));
            assert!(!c.as_str().is_empty());
            i += 1;
        }
        assert_eq!(ChannelId::from_u8(13), None);
        assert_eq!(ChannelId::from_u8(255), None);
    }

    #[test]
    fn null_capture_accepts_everything_and_does_nothing() {
        // Happy-path exercise of every default hook — compiles to
        // nothing, but pins the trait surface so a signature change
        // breaks loudly here first.
        let mut c = NullCapture;
        let t = Tick::new(
            1,
            VenueId::Okx,
            make_symbol_id(VenueId::Okx, 1),
            1,
            Price::from_raw(1),
            Qty::from_raw(1),
            Price::from_raw(2),
            Qty::from_raw(1),
        );
        let e = ChannelEvent::new(1, VenueId::Okx, ChannelId::Trade, 0, 0, 0, 0, 0);
        let s = Signal::new(1, 0, LatencyClass::Hot, SignalSource::Rpc as u8, [0; 40]);
        Capture::tick(&mut c, &t);
        Capture::event(&mut c, &e);
        Capture::signal(&mut c, &s);
        Capture::raw_frame(&mut c, 1, b"payload");
        Capture::parse_reject(&mut c, 1, b"bad");
        Capture::maybe_flush(&mut c, 2);
    }
}

#[cfg(test)]
mod ai_cmd_tests {
    use super::*;

    /// Canonical valid command per kind — the §3 table's happy rows.
    /// Failure tests mutate one field at a time off these.
    fn valid(kind: AiCmdKind) -> AiCmd {
        let sym = make_symbol_id(VenueId::Polymarket, 7);
        match kind {
            AiCmdKind::Heartbeat | AiCmdKind::HaltRequest => AiCmd::new(
                1,
                1,
                SYMBOL_ID_NONE,
                0,
                0,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                0,
                0,
            ),
            AiCmdKind::EnableStrategy | AiCmdKind::DisableStrategy => AiCmd::new(
                1,
                1,
                SYMBOL_ID_NONE,
                0,
                0,
                0,
                kind,
                VenueId::Ai,
                2,
                AI_SIDE_NONE,
                0,
                0,
            ),
            AiCmdKind::SetFairValue => AiCmd::new(
                1,
                1,
                sym,
                500_000,
                0,
                5_000_000_000,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                0,
                0,
            ),
            AiCmdKind::SetBias => AiCmd::new(
                1,
                1,
                sym,
                -25_000,
                0,
                5_000_000_000,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                0,
                0,
            ),
            AiCmdKind::SetParam => AiCmd::new(
                1,
                1,
                SYMBOL_ID_NONE,
                42_000_000,
                0,
                0,
                kind,
                VenueId::Ai,
                1,
                AI_SIDE_NONE,
                7,
                0,
            ),
            AiCmdKind::OrderIntent => AiCmd::new(
                1,
                1,
                sym,
                480_000,
                10_000_000,
                2_000_000_000,
                kind,
                VenueId::Polymarket,
                STRATEGY_SLOT_AI_EXEC,
                Side::Bid as u8,
                0,
                0,
            ),
            AiCmdKind::RulesetStage | AiCmdKind::RulesetCommit => AiCmd::new(
                1,
                1,
                SYMBOL_ID_NONE,
                0x0123_4567_89AB_CDEFu64 as i64,
                -42,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_VM,
                AI_SIDE_NONE,
                0,
                0,
            ),
            // VM2 V1 (D-1): one funding print — rate ×1e9 in px,
            // venue print ms in qty.
            AiCmdKind::FundingSeed => AiCmd::new(
                1,
                1,
                make_symbol_id(VenueId::Okx, 3),
                125_000_000,
                1_756_400_000_000,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_VM,
                AI_SIDE_NONE,
                0,
                0,
            ),
            // VM2 V1 (D-2): restore row 17's position — entry px
            // ×1e6, age SECONDS in qty, ttl 0 (enforced).
            AiCmdKind::PositionSeed => AiCmd::new(
                1,
                1,
                make_symbol_id(VenueId::Binance, 9),
                65_000_000_000,
                3_600,
                0,
                kind,
                VenueId::Ai,
                STRATEGY_SLOT_VM,
                Side::Ask as u8,
                17,
                0,
            ),
        }
    }

    const ALL_KINDS: [AiCmdKind; 12] = [
        AiCmdKind::Heartbeat,
        AiCmdKind::EnableStrategy,
        AiCmdKind::DisableStrategy,
        AiCmdKind::SetFairValue,
        AiCmdKind::SetBias,
        AiCmdKind::SetParam,
        AiCmdKind::OrderIntent,
        AiCmdKind::RulesetStage,
        AiCmdKind::RulesetCommit,
        AiCmdKind::HaltRequest,
        AiCmdKind::FundingSeed,
        AiCmdKind::PositionSeed,
    ];

    #[test]
    fn ai_cmd_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<AiCmd>(), 64);
        assert_eq!(::core::mem::align_of::<AiCmd>(), 64);
    }

    #[test]
    fn ruleset_hash128_reassembles_px_qty_halves() {
        // Happy path: the golden pairing — px = bytes 0..8 LE,
        // qty = bytes 8..16 LE. Vector chosen with distinct bytes so
        // any half-swap or endianness slip fails.
        let h: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let px = i64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]);
        let qty = i64::from_le_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
        let cmd = valid(AiCmdKind::RulesetCommit);
        let mut cmd = cmd;
        cmd.px = px;
        cmd.qty = qty;
        assert_eq!(cmd.ruleset_hash128(), h);
    }

    #[test]
    fn ruleset_hash128_differs_when_either_half_differs() {
        // Failure mode: perturbing either half must change the
        // reassembled identity (no half is ignored).
        let base = valid(AiCmdKind::RulesetStage);
        let h0 = base.ruleset_hash128();
        let mut px_flip = base;
        px_flip.px ^= 1;
        assert_ne!(px_flip.ruleset_hash128(), h0);
        let mut qty_flip = base;
        qty_flip.qty ^= 1;
        assert_ne!(qty_flip.ruleset_hash128(), h0);
    }

    #[test]
    fn ai_cmd_layout_is_fully_explicit() {
        // AiCmd: 8+4+4+8+8+8+1+1+1+1+2+2+16 = 64 — any compiler-
        // inserted padding would break the AsBytes contract.
        assert_eq!(::core::mem::size_of::<AiCmd>(), 64);
    }

    #[test]
    fn ai_cmd_bytes_sit_at_documented_offsets() {
        // docs/wire-format.md pins the AiCmd layout; the Python packer
        // (claude-worker frames.py) depends on these exact positions.
        let c = AiCmd::new(
            0x1111_2222_3333_4444,
            0xAABB_CCDD,
            0x0500_0007,
            0x0102_0304_0506_0708,
            -1,
            0x2222_0000_1111_0000,
            AiCmdKind::SetParam,
            VenueId::Ai,
            1,
            AI_SIDE_NONE,
            0xBEEF,
            0,
        );
        // SAFETY: AiCmd is AsBytes (repr(C), Copy, fully initialized);
        // read-only byte view of a live stack value.
        let b = unsafe { core::slice::from_raw_parts((&c as *const AiCmd).cast::<u8>(), 64) };
        assert_eq!(
            u64::from_le_bytes(b[0..8].try_into().unwrap()),
            0x1111_2222_3333_4444
        );
        assert_eq!(
            u32::from_le_bytes(b[8..12].try_into().unwrap()),
            0xAABB_CCDD
        );
        assert_eq!(
            u32::from_le_bytes(b[12..16].try_into().unwrap()),
            0x0500_0007
        );
        assert_eq!(
            i64::from_le_bytes(b[16..24].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(i64::from_le_bytes(b[24..32].try_into().unwrap()), -1);
        assert_eq!(
            u64::from_le_bytes(b[32..40].try_into().unwrap()),
            0x2222_0000_1111_0000
        );
        assert_eq!(b[40], AiCmdKind::SetParam.to_u8());
        assert_eq!(b[41], VenueId::Ai.to_u8());
        assert_eq!(b[42], 1);
        assert_eq!(b[43], AI_SIDE_NONE);
        assert_eq!(u16::from_le_bytes([b[44], b[45]]), 0xBEEF);
        assert_eq!(u16::from_le_bytes([b[46], b[47]]), 0);
        // Explicit tail padding must be zero.
        let mut i = 48;
        while i < 64 {
            assert_eq!(b[i], 0);
            i += 1;
        }
    }

    #[test]
    fn ai_cmd_kind_roundtrips_and_rejects_unknown() {
        let mut i = 0;
        while i < ALL_KINDS.len() {
            let k = ALL_KINDS[i];
            assert_eq!(AiCmdKind::from_u8(k.to_u8()), Some(k));
            i += 1;
        }
        // No `Resume` kind exists ANYWHERE in the table: halt is
        // sticky by design (risk-policy) — the wire cannot express
        // it. VM2 appended FundingSeed=10/PositionSeed=11 (append-only
        // ABI); the first unassigned byte is now 12.
        assert_eq!(AiCmdKind::from_u8(12), None);
        assert_eq!(AiCmdKind::from_u8(0xFF), None);
    }

    #[test]
    fn ai_cmd_read_le_roundtrips() {
        let src = valid(AiCmdKind::OrderIntent);
        // SAFETY: AiCmd is AsBytes; read-only byte view of a live value.
        let b = unsafe { core::slice::from_raw_parts((&src as *const AiCmd).cast::<u8>(), 64) };
        let arr: &[u8; 64] = b.try_into().unwrap();
        let got = AiCmd::read_le(arr);
        assert_eq!(got.ts_ns, src.ts_ns);
        assert_eq!(got.seq, src.seq);
        assert_eq!(got.sym, src.sym);
        assert_eq!(got.px, src.px);
        assert_eq!(got.qty, src.qty);
        assert_eq!(got.ttl_ns, src.ttl_ns);
        assert_eq!(got.kind, src.kind);
        assert_eq!(got.venue, src.venue);
        assert_eq!(got.strategy_id, src.strategy_id);
        assert_eq!(got.side, src.side);
        assert_eq!(got.param_id, src.param_id);
        assert_eq!(got.flags, src.flags);
        assert!(got.validate_shape().is_ok());
    }

    #[test]
    fn ai_cmd_read_le_handles_unaligned_source() {
        // UDS frames sit at arbitrary rx-buffer offsets; read_le must
        // not require alignment. Stage the bytes at offset 1.
        let src = valid(AiCmdKind::SetFairValue);
        // SAFETY: AiCmd is AsBytes; read-only byte view of a live value.
        let b = unsafe { core::slice::from_raw_parts((&src as *const AiCmd).cast::<u8>(), 64) };
        let mut staged = [0u8; 80];
        staged[1..65].copy_from_slice(b);
        let arr: &[u8; 64] = (&staged[1..65]).try_into().unwrap();
        let got = AiCmd::read_le(arr);
        assert_eq!(got.sym, src.sym);
        assert_eq!(got.px, src.px);
        assert_eq!(got.ttl_ns, src.ttl_ns);
        assert!(got.validate_shape().is_ok());
    }

    #[test]
    fn every_kind_has_a_valid_shape() {
        let mut i = 0;
        while i < ALL_KINDS.len() {
            let c = valid(ALL_KINDS[i]);
            assert_eq!(c.validate_shape(), Ok(()), "kind {:?}", ALL_KINDS[i]);
            assert_eq!(c.kind(), Some(ALL_KINDS[i]));
            i += 1;
        }
    }

    #[test]
    fn expire_on_silence_flag_is_legal_on_fair_value_and_bias_only() {
        let mut fv = valid(AiCmdKind::SetFairValue);
        fv.flags = AI_CMD_FLAG_EXPIRE_ON_SILENCE;
        assert_eq!(fv.validate_shape(), Ok(()));

        let mut bias = valid(AiCmdKind::SetBias);
        bias.flags = AI_CMD_FLAG_EXPIRE_ON_SILENCE;
        assert_eq!(bias.validate_shape(), Ok(()));

        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.flags = AI_CMD_FLAG_EXPIRE_ON_SILENCE;
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadFlags(1)));

        // Undefined bit — rejected even where bit 0 is legal.
        fv.flags = 0b10;
        assert_eq!(fv.validate_shape(), Err(AiCmdShapeError::BadFlags(0b10)));
    }

    #[test]
    fn validate_rejects_unknown_kind_byte() {
        // 12 = first unassigned kind byte after VM2's 10/11.
        let mut c = valid(AiCmdKind::Heartbeat);
        c.kind = 12;
        assert_eq!(c.validate_shape(), Err(AiCmdShapeError::UnknownKind(12)));
    }

    #[test]
    fn validate_rejects_nonzero_padding() {
        let mut c = valid(AiCmdKind::Heartbeat);
        c._pad[3] = 1;
        assert_eq!(c.validate_shape(), Err(AiCmdShapeError::NonZeroPad));
    }

    #[test]
    fn validate_rejects_wrong_venue() {
        // Engine-directed kinds must carry VenueId::Ai.
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.venue = VenueId::Binance.to_u8();
        assert_eq!(
            hb.validate_shape(),
            Err(AiCmdShapeError::BadVenue(VenueId::Binance.to_u8()))
        );
        // Intents must carry a real market venue — never Ai...
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.venue = VenueId::Ai.to_u8();
        assert_eq!(
            oi.validate_shape(),
            Err(AiCmdShapeError::BadVenue(VenueId::Ai.to_u8()))
        );
        // ...and never an undecodable byte.
        oi.venue = 200;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadVenue(200)));
    }

    #[test]
    fn validate_rejects_bad_sym() {
        // Symbol forbidden on heartbeat.
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.sym = make_symbol_id(VenueId::Polymarket, 1);
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadSym(hb.sym)));
        // Symbol required on fair value / intents.
        let mut fv = valid(AiCmdKind::SetFairValue);
        fv.sym = SYMBOL_ID_NONE;
        assert_eq!(
            fv.validate_shape(),
            Err(AiCmdShapeError::BadSym(SYMBOL_ID_NONE))
        );
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.sym = SYMBOL_ID_NONE;
        assert_eq!(
            oi.validate_shape(),
            Err(AiCmdShapeError::BadSym(SYMBOL_ID_NONE))
        );
    }

    #[test]
    fn validate_rejects_bad_px() {
        // px must be zero where unused.
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.px = 1;
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadPx(1)));
        // Fair values are prices — negative is malformed (bias is the
        // signed channel).
        let mut fv = valid(AiCmdKind::SetFairValue);
        fv.px = -1;
        assert_eq!(fv.validate_shape(), Err(AiCmdShapeError::BadPx(-1)));
        // Intent price must be strictly positive.
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.px = 0;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadPx(0)));
    }

    #[test]
    fn validate_rejects_bad_qty() {
        let mut en = valid(AiCmdKind::EnableStrategy);
        en.qty = 1;
        assert_eq!(en.validate_shape(), Err(AiCmdShapeError::BadQty(1)));
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.qty = 0;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadQty(0)));
        oi.qty = -5;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadQty(-5)));
    }

    #[test]
    fn validate_rejects_bad_ttl() {
        // TTL required (>0) on fair value / bias / intents.
        let mut fv = valid(AiCmdKind::SetFairValue);
        fv.ttl_ns = 0;
        assert_eq!(fv.validate_shape(), Err(AiCmdShapeError::BadTtl(0)));
        // TTL forbidden elsewhere.
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.ttl_ns = 1;
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadTtl(1)));
        let mut st = valid(AiCmdKind::RulesetStage);
        st.ttl_ns = 1;
        assert_eq!(st.validate_shape(), Err(AiCmdShapeError::BadTtl(1)));
    }

    #[test]
    fn validate_rejects_bad_strategy_slot() {
        // Enable/Disable/SetParam: slot must be < MAX_STRATEGY_SLOTS.
        let mut en = valid(AiCmdKind::EnableStrategy);
        en.strategy_id = MAX_STRATEGY_SLOTS;
        assert_eq!(
            en.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(MAX_STRATEGY_SLOTS))
        );
        en.strategy_id = STRATEGY_SLOT_NONE;
        assert_eq!(
            en.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(STRATEGY_SLOT_NONE))
        );
        // Kinds with no slot must carry the sentinel.
        let mut fv = valid(AiCmdKind::SetFairValue);
        fv.strategy_id = 0;
        assert_eq!(
            fv.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(0))
        );
        // Intents are pinned to the ai-exec slot...
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.strategy_id = STRATEGY_SLOT_VM;
        assert_eq!(
            oi.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(STRATEGY_SLOT_VM))
        );
        // ...and ruleset commands to the vm slot.
        let mut st = valid(AiCmdKind::RulesetCommit);
        st.strategy_id = STRATEGY_SLOT_AI_EXEC;
        assert_eq!(
            st.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(STRATEGY_SLOT_AI_EXEC))
        );
    }

    #[test]
    fn validate_rejects_bad_side() {
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.side = Side::Bid as u8;
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadSide(0)));
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.side = 2;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadSide(2)));
        oi.side = AI_SIDE_NONE;
        assert_eq!(
            oi.validate_shape(),
            Err(AiCmdShapeError::BadSide(AI_SIDE_NONE))
        );
    }

    #[test]
    fn validate_rejects_bad_param_id() {
        let mut hb = valid(AiCmdKind::Heartbeat);
        hb.param_id = 1;
        assert_eq!(hb.validate_shape(), Err(AiCmdShapeError::BadParamId(1)));
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.param_id = 3;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadParamId(3)));
    }

    #[test]
    fn ai_ring_size_is_locked() {
        assert_eq!(AI_RING_SIZE, 1024);
        assert!(AI_RING_SIZE.is_power_of_two());
    }

    #[test]
    fn rule_table_ring_slots_is_locked() {
        // §6 (D1a): one staged table in flight + one restage-
        // supersede; a third undrained stage is a §5 push-full reject.
        assert_eq!(RULE_TABLE_RING_SLOTS, 2);
        assert!(RULE_TABLE_RING_SLOTS.is_power_of_two());
    }

    // -----------------------------------------------------------
    // RuleRow / RuleTable (Phase 8g §3)
    // -----------------------------------------------------------

    #[test]
    fn rule_row_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<RuleRow>(), 64);
        assert_eq!(::core::mem::align_of::<RuleRow>(), 64);
    }


    #[test]
    fn rule_row_layout_is_fully_explicit() {
        // Sum of declared field widths must equal size_of — any
        // implicit compiler padding breaks this (§3 pad amended
        // 13 → 21 in G1, operator-confirmed).
        let declared = 4 + 4 + 4 + 4 + 8 + 8 + 8 + 1 + 1 + 1 + 21;
        assert_eq!(declared, ::core::mem::size_of::<RuleRow>());
    }


    #[test]
    fn rule_row_new_roundtrips_and_zeroes_padding() {
        let r = RuleRow::new(
            make_symbol_id(VenueId::Polymarket, 42),
            make_symbol_id(VenueId::Binance, 7),
            80,
            1_500,
            0,
            50_000_000,
            fnv1a_64(b"btc-pm-lag"),
            RuleRow::TRIGGER_CROSS_DEVIATION,
            Side::Bid as u8,
            MarketFamily::Crypto as u8,
        );
        assert_eq!(r.sym, make_symbol_id(VenueId::Polymarket, 42));
        assert_eq!(r.ref_sym, make_symbol_id(VenueId::Binance, 7));
        assert_eq!(r.edge_bps, 80);
        assert_eq!(r.horizon_ms, 1_500);
        assert_eq!(r.level_1e6, 0);
        assert_eq!(r.max_risk_1e6, 50_000_000);
        assert_eq!(r.name_h, fnv1a_64(b"btc-pm-lag"));
        assert_eq!(r.trigger, RuleRow::TRIGGER_CROSS_DEVIATION);
        assert_eq!(r.side, Side::Bid as u8);
        assert_eq!(r.family, MarketFamily::Crypto as u8);
        let mut i = 0;
        while i < r._pad.len() {
            assert_eq!(r._pad[i], 0);
            i += 1;
        }
    }


    #[test]
    fn fnv1a_64_matches_published_vectors() {
        // Offset basis — the hash of the empty input.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn fnv1a_64_distinct_names_hash_apart() {
        // The §4.2 rule-5 uniqueness reject leans on name_h
        // inequality for distinct names (failure-mode guard: a
        // degenerate hash would collapse these).
        assert_ne!(fnv1a_64(b"btc-pm-lag"), fnv1a_64(b"hormuz-floor"));
        assert_ne!(fnv1a_64(b"a"), fnv1a_64(b"b"));
        assert_ne!(fnv1a_64(b""), fnv1a_64(b"\0"));
    }
}

#[cfg(test)]
mod vm2_v1_tests {
    use super::*;

    // ---------------- AiCmd kinds 10/11 ----------------

    #[test]
    fn new_kinds_roundtrip_and_next_byte_rejects() {
        assert_eq!(AiCmdKind::from_u8(10), Some(AiCmdKind::FundingSeed));
        assert_eq!(AiCmdKind::from_u8(11), Some(AiCmdKind::PositionSeed));
        assert_eq!(AiCmdKind::FundingSeed.to_u8(), 10);
        assert_eq!(AiCmdKind::PositionSeed.to_u8(), 11);
        // 12 is the first unassigned kind byte after VM2.
        assert_eq!(AiCmdKind::from_u8(12), None);
    }

    fn funding_seed() -> AiCmd {
        AiCmd::new(
            1,
            1,
            make_symbol_id(VenueId::Okx, 3),
            125_000_000, // rate ×1e9
            1_756_400_000_000, // print time ms
            0,
            AiCmdKind::FundingSeed,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn funding_seed_shape_happy_path() {
        assert_eq!(funding_seed().validate_shape(), Ok(()));
        // Negative and zero rates are venue facts, both legal.
        let mut c = funding_seed();
        c.px = -3_000_000;
        assert_eq!(c.validate_shape(), Ok(()));
        c.px = 0;
        assert_eq!(c.validate_shape(), Ok(()));
    }

    #[test]
    fn funding_seed_shape_rejects() {
        let mut c = funding_seed();
        c.sym = SYMBOL_ID_NONE;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadSym(_))
        ));
        let mut c = funding_seed();
        c.qty = 0; // a print without a time
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadQty(0))
        ));
        let mut c = funding_seed();
        c.ttl_ns = 5;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadTtl(5))
        ));
        let mut c = funding_seed();
        c.strategy_id = STRATEGY_SLOT_AI_EXEC;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(_))
        ));
        let mut c = funding_seed();
        c.side = Side::Bid as u8;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadSide(_))
        ));
        let mut c = funding_seed();
        c.param_id = 1;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadParamId(1))
        ));
        let mut c = funding_seed();
        c.flags = 1;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadFlags(1))
        ));
        let mut c = funding_seed();
        c.venue = VenueId::Okx.to_u8(); // engine-directed kinds pin Ai
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadVenue(_))
        ));
    }

    fn position_seed() -> AiCmd {
        AiCmd::new(
            1,
            1,
            make_symbol_id(VenueId::Binance, 9),
            65_000_000_000, // entry px ×1e6
            3_600,          // age SECONDS (1 h)
            0,              // ttl MUST be 0 (drain-expiry law)
            AiCmdKind::PositionSeed,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            Side::Ask as u8,
            17, // row index
            0,
        )
    }

    #[test]
    fn position_seed_shape_happy_path() {
        assert_eq!(position_seed().validate_shape(), Ok(()));
        // Age 0 = just entered; row 0 and the last row are both legal.
        let mut c = position_seed();
        c.qty = 0;
        c.param_id = 0;
        assert_eq!(c.validate_shape(), Ok(()));
        c.param_id = (RULE_TABLE_ROWS - 1) as u16;
        assert_eq!(c.validate_shape(), Ok(()));
    }

    #[test]
    fn position_seed_shape_rejects() {
        let mut c = position_seed();
        c.param_id = RULE_TABLE_ROWS as u16; // first out-of-range row
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadParamId(_))
        ));
        let mut c = position_seed();
        c.px = 0;
        assert!(matches!(c.validate_shape(), Err(AiCmdShapeError::BadPx(0))));
        let mut c = position_seed();
        c.qty = -1; // negative age
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadQty(-1))
        ));
        let mut c = position_seed();
        c.ttl_ns = 1; // a seed must never expire at the drain site
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadTtl(1))
        ));
        let mut c = position_seed();
        c.side = AI_SIDE_NONE; // the entered side is required
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadSide(_))
        ));
        let mut c = position_seed();
        c.sym = SYMBOL_ID_NONE;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadSym(_))
        ));
        let mut c = position_seed();
        c.strategy_id = STRATEGY_SLOT_NONE;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadStrategySlot(_))
        ));
        let mut c = position_seed();
        c.flags = 2;
        assert!(matches!(
            c.validate_shape(),
            Err(AiCmdShapeError::BadFlags(2))
        ));
    }

    // ---------------- FeatId / CombineOp ----------------

    #[test]
    fn feat_id_roundtrips_and_rejects() {
        let all = [
            FeatId::Mid,
            FeatId::Bid,
            FeatId::Ask,
            FeatId::RollMean,
            FeatId::RollEma,
            FeatId::RollMin,
            FeatId::RollMax,
            FeatId::RollStd,
            FeatId::Apr24,
            FeatId::Apr72,
            FeatId::MarkPx,
            FeatId::MarkIv,
            FeatId::DepthImb,
            FeatId::DepthSpreadBps,
            FeatId::DepthNearNotional,
            FeatId::ClockToFunding,
            FeatId::ClockUtcSod,
        ];
        let mut i = 0;
        while i < all.len() {
            assert_eq!(FeatId::from_u8(all[i] as u8), Some(all[i]));
            i += 1;
        }
        // First unassigned byte, and the FEAT_NONE sentinel, reject.
        assert_eq!(FeatId::from_u8(17), None);
        assert_eq!(FeatId::from_u8(FEAT_NONE), None);
    }

    #[test]
    fn feat_id_channel_classification_is_a_partition() {
        // Every feature needs exactly one of price/funding/opt/depth —
        // or none (pure clock) — never two: rule 10 leans on this.
        let all = [
            FeatId::Mid,
            FeatId::Bid,
            FeatId::Ask,
            FeatId::RollMean,
            FeatId::RollEma,
            FeatId::RollMin,
            FeatId::RollMax,
            FeatId::RollStd,
            FeatId::Apr24,
            FeatId::Apr72,
            FeatId::MarkPx,
            FeatId::MarkIv,
            FeatId::DepthImb,
            FeatId::DepthSpreadBps,
            FeatId::DepthNearNotional,
            FeatId::ClockToFunding,
            FeatId::ClockUtcSod,
        ];
        let mut i = 0;
        while i < all.len() {
            let f = all[i];
            let n = f.requires_price() as u8
                + f.requires_funding() as u8
                + f.requires_opt_summary() as u8
                + f.requires_depth() as u8;
            assert!(n <= 1, "feature must not claim two channels");
            i += 1;
        }
        assert!(FeatId::Mid.requires_price());
        assert!(FeatId::RollStd.requires_price());
        assert!(FeatId::Apr24.requires_funding());
        assert!(FeatId::ClockToFunding.requires_funding());
        assert!(FeatId::MarkIv.requires_opt_summary());
        assert!(FeatId::DepthImb.requires_depth());
        // ClockUtcSod needs nothing — always present.
        let c = FeatId::ClockUtcSod;
        assert!(
            !c.requires_price()
                && !c.requires_funding()
                && !c.requires_opt_summary()
                && !c.requires_depth()
        );
    }

    #[test]
    fn feat_id_window_law() {
        assert!(FeatId::RollMean.requires_window());
        assert!(FeatId::RollStd.requires_window());
        assert!(!FeatId::Mid.requires_window());
        assert!(!FeatId::Apr24.requires_window(), "APR windows are fixed");
        assert!(!FeatId::ClockUtcSod.requires_window());
    }

    #[test]
    fn combine_op_roundtrips_and_rejects() {
        let all = [
            CombineOp::Diff,
            CombineOp::DiffBps,
            CombineOp::Ratio1e9,
            CombineOp::LhsOnly,
        ];
        let mut i = 0;
        while i < all.len() {
            assert_eq!(CombineOp::from_u8(all[i] as u8), Some(all[i]));
            i += 1;
        }
        assert_eq!(CombineOp::from_u8(4), None);
    }

    #[test]
    fn cmp_and_flag_bits_are_distinct() {
        assert_eq!(CMP_ENTRY_LE & CMP_ENTRY_ABS, 0);
        assert_eq!(CMP_CONFIRM_LE & CMP_CONFIRM_ABS, 0);
        assert_eq!(CMP_CONFIRM_PAIR & (CMP_ENTRY_LE | CMP_ENTRY_ABS), 0);
        assert_eq!(
            CMP_ENTRY_LE | CMP_ENTRY_ABS | CMP_CONFIRM_LE | CMP_CONFIRM_ABS | CMP_CONFIRM_PAIR,
            CMP_BITS_MASK
        );
        assert_eq!(ROW_FLAG_POSITION, ROW_FLAGS_MASK);
    }

    // ---------------- RuleRowV2 / RuleTableV2 layout ----------------

    #[test]
    fn rule_row_v2_layout_is_fully_explicit() {
        // Declared field widths sum to size_of — no implicit padding:
        // 1×8 + 4+4 + 2+2+2 + 1+1 + 8+8+8 + 4+4+4+4 + 8+8 + 4+1+3+40
        // = 8+8+6+2+24+16+16+48 = 128.
        assert_eq!(::core::mem::size_of::<RuleRowV2>(), 128);
        assert_eq!(::core::mem::align_of::<RuleRowV2>(), 64);
        assert_eq!(::core::mem::size_of::<RuleTableV2>(), 32 * 1024 + 64);
        assert_eq!(::core::mem::align_of::<RuleTableV2>(), 64);
    }

    #[test]
    fn rule_row_v2_zero_is_all_zero_and_new_stamps_ver() {
        let z = RuleRowV2::ZERO;
        assert_eq!(z.ver, 0, "ZERO is non-built filler");
        assert_eq!(z.sym, 0);
        assert_eq!(z.enter_1e9, 0);
        // Byte-level: the whole 128 B slot is zero.
        // SAFETY: RuleRowV2 is #[repr(C)] Copy with fully explicit
        // padding; read-only byte view of a live stack value.
        let zb = unsafe {
            core::slice::from_raw_parts((&z as *const RuleRowV2).cast::<u8>(), 128)
        };
        let mut i = 0;
        while i < 128 {
            assert_eq!(zb[i], 0);
            i += 1;
        }

        let r = RuleRowV2::new(
            ROW_FLAG_POSITION,
            RuleRow::SIDE_BOTH,
            2,
            FeatId::Apr24,
            FeatId::Apr24,
            FeatId::Apr72 as u8,
            CombineOp::Diff,
            make_symbol_id(VenueId::Hyperliquid, 1),
            make_symbol_id(VenueId::Okx, 4),
            0,
            0,
            0,
            CMP_CONFIRM_PAIR | CMP_CONFIRM_ABS,
            200_000_000,
            0,
            300_000_000,
            345_600,
            60_000,
            0,
            9_900_000_000,
            fnv1a_64(b"cvfc-doge"),
            0,
            0,
        );
        assert_eq!(r.ver, RULE_ROW_VER_2);
        assert_eq!(r.flags, ROW_FLAG_POSITION);
        assert_eq!(r.feat_a, FeatId::Apr24 as u8);
        assert_eq!(r.feat_c, FeatId::Apr72 as u8);
        assert_eq!(r.combine, CombineOp::Diff as u8);
        assert_eq!(r.min_hold_s, 345_600);
        assert_eq!(r.name_h, fnv1a_64(b"cvfc-doge"));
    }

    #[test]
    fn rule_table_v2_empty_is_inert() {
        let t = RuleTableV2::EMPTY;
        assert_eq!(t.len, 0);
        assert_eq!(t.epoch, 0);
        assert_eq!(t.hash128, [0u8; 16]);
        assert_eq!(t.rows[0].ver, 0);
        assert_eq!(t.rows[RULE_TABLE_ROWS - 1].ver, 0);
    }

    // ---------------- v1 → v2 sugar mapping ----------------

    #[test]
    fn from_v1_level_breach_maps_transact_price_law() {
        // Bid row: buy at/below ⇒ watches the ASK with ≤.
        let bid = RuleRow::new(
            42,
            SYMBOL_ID_NONE,
            0,
            1_500,
            480_000,
            3_000_000,
            fnv1a_64(b"lb-bid"),
            RuleRow::TRIGGER_LEVEL_BREACH,
            Side::Bid as u8,
            2,
        );
        let v2 = RuleRowV2::from_v1(&bid);
        assert_eq!(v2.ver, RULE_ROW_VER_2);
        assert_eq!(v2.flags, 0, "sugar rows keep v1 refire semantics");
        assert_eq!(v2.feat_a, FeatId::Ask as u8);
        assert_eq!(v2.combine, CombineOp::LhsOnly as u8);
        assert_eq!(v2.cmp_bits, CMP_ENTRY_LE);
        assert_eq!(v2.enter_1e9, 480_000_000, "px ×1e6 → signal ×1e9");
        assert_eq!(v2.ref_sym, SYMBOL_ID_NONE);
        assert_eq!(v2.group, GROUP_NONE);
        assert_eq!(v2.exit_1e9, 0);
        assert_eq!(v2.horizon_ms, 1_500);
        assert_eq!(v2.name_h, fnv1a_64(b"lb-bid"));
        assert_eq!(v2.family, 2);
        // Ask row: sell at/above ⇒ watches the BID with ≥.
        let mut ask = bid;
        ask.side = Side::Ask as u8;
        let v2a = RuleRowV2::from_v1(&ask);
        assert_eq!(v2a.feat_a, FeatId::Bid as u8);
        assert_eq!(v2a.cmp_bits, 0, "GE");
        // SIDE_BOTH keeps Ask/LE (the evaluator's both-leg arm keys
        // off the side byte — bid leg first, the v1 order).
        let mut both = bid;
        both.side = RuleRow::SIDE_BOTH;
        let v2b = RuleRowV2::from_v1(&both);
        assert_eq!(v2b.feat_a, FeatId::Ask as u8);
        assert_eq!(v2b.side, RuleRow::SIDE_BOTH);
    }

    #[test]
    fn from_v1_cross_deviation_maps_abs_diff_bps() {
        let cd = RuleRow::new(
            42,
            7,
            80,
            1_000,
            0,
            3_000_000,
            fnv1a_64(b"cd"),
            RuleRow::TRIGGER_CROSS_DEVIATION,
            RuleRow::SIDE_BOTH,
            0,
        );
        let v2 = RuleRowV2::from_v1(&cd);
        assert_eq!(v2.feat_a, FeatId::Mid as u8);
        assert_eq!(v2.feat_b, FeatId::Mid as u8);
        assert_eq!(v2.combine, CombineOp::DiffBps as u8);
        assert_eq!(v2.cmp_bits, CMP_ENTRY_ABS);
        assert_eq!(v2.enter_1e9, 80_000_000_000, "80 bps ×1e9");
        assert_eq!(v2.ref_sym, 7);
        assert_eq!(v2.side, RuleRow::SIDE_BOTH, "side stays the filter");
        assert_eq!(v2.edge_bps, 80, "diagnostic mirror");
    }


    // ---------------- funding cadence law ----------------

    #[test]
    fn funding_print_divisor_is_the_deribit_law() {
        // Exhaustive over venues: ONLY Deribit divides (hourly samples
        // of interest_8h — the R4-§9 unit law; the worker mirror is
        // claude_worker.carry_signal.apr_from_prints).
        assert_eq!(funding_print_divisor(VenueId::Deribit), 8);
        assert_eq!(funding_print_divisor(VenueId::Polymarket), 1);
        assert_eq!(funding_print_divisor(VenueId::Binance), 1);
        assert_eq!(funding_print_divisor(VenueId::Okx), 1);
        assert_eq!(funding_print_divisor(VenueId::Hyperliquid), 1);
        assert_eq!(funding_print_divisor(VenueId::Ai), 1);
        assert_eq!(funding_print_divisor(VenueId::Bybit), 1);
    }

    #[test]
    fn funding_period_matches_venue_cadence() {
        assert_eq!(funding_period_s(VenueId::Binance), 28_800);
        assert_eq!(funding_period_s(VenueId::Okx), 28_800);
        assert_eq!(funding_period_s(VenueId::Bybit), 28_800);
        assert_eq!(funding_period_s(VenueId::Hyperliquid), 3_600);
        // Continuous funding / no funding: the clock feature is ABSENT.
        assert_eq!(funding_period_s(VenueId::Deribit), 0);
        assert_eq!(funding_period_s(VenueId::Polymarket), 0);
        assert_eq!(funding_period_s(VenueId::Ai), 0);
    }

    #[test]
    fn funding_windows_are_the_carry_law_windows() {
        assert_eq!(FUNDING_WINDOW_24H_MIN, 1_440);
        assert_eq!(FUNDING_WINDOW_72H_MIN, 4_320);
        assert_eq!(ROLL_WINDOW_MAX_MIN, 4_320);
    }
}
