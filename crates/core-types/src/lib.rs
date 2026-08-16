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
            _ => None,
        }
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

/// Top-of-book tick snapshot. Produced by the Polymarket WS parser,
/// pushed onto the tick ring, consumed by the engine.
///
/// Layout is fixed-size (64 bytes) so it fills one cache line exactly —
/// no false sharing between the ingress thread and the engine thread
/// when they both touch adjacent slots in the ring.
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
    /// Explicit tail padding — [`AsBytes`] requires every byte of the
    /// 64 B slot to be initialized. Always zero.
    _pad: [u8; 15],
}

impl Tick {
    /// Construct a Tick in one shot without named-field noise at call sites.
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
        Self {
            ts_ns,
            sym,
            venue_seq,
            bid_px,
            bid_qty,
            ask_px,
            ask_qty,
            venue: venue as u8,
            _pad: [0; 15],
        }
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
    /// Reserved.
    _pad1: [u8; 15],
    /// Explicit tail padding (see [`AsBytes`]). Always zero.
    _pad2: [u8; 8],
}

impl Order {
    /// Construct an Order without naming the private padding fields.
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
            _pad1: [0; 15],
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
    /// Funding rate (OKX `funding-rate`).
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
            _ => None,
        }
    }
}

/// Capacity of the AI command SPSC ring (design §4.3). Power of two,
/// like every `core-ring` capacity.
pub const AI_RING_SIZE: usize = 1024;

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

/// Strategy-set slot reserved for `strategy-vm` (8g). `RulesetStage` /
/// `RulesetCommit` commands must target exactly this slot.
pub const STRATEGY_SLOT_VM: u8 = 5;

/// `AiCmd::side` sentinel meaning "no side" (every kind except
/// `OrderIntent`).
pub const AI_SIDE_NONE: u8 = 0xFF;

/// Why an [`AiCmd`] failed [`AiCmd::validate_shape`]. Each variant maps
/// to one violated row/column of the per-kind shape table in
/// `docs/phase-8f-design.md` §3; the accept path folds all of them into
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
    /// `docs/phase-8f-design.md` §3 ("unused fields MUST be zero /
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

    /// One parsed non-tick channel event.
    #[inline(always)]
    fn event(&mut self, _e: &ChannelEvent) {}

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
        ];
        let mut i = 0;
        while i < all.len() {
            assert_eq!(VenueId::from_u8(all[i].to_u8()), Some(all[i]));
            i += 1;
        }
    }

    #[test]
    fn venue_id_rejects_unknown_bytes() {
        assert_eq!(VenueId::from_u8(6), None);
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
        let tb = unsafe {
            core::slice::from_raw_parts((&t as *const Tick).cast::<u8>(), 64)
        };
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
        let ob = unsafe {
            core::slice::from_raw_parts((&o as *const Order).cast::<u8>(), 64)
        };
        assert_eq!(ob[40], VenueId::Okx.to_u8());
        let mut i = 41;
        while i < 64 {
            assert_eq!(ob[i], 0);
            i += 1;
        }
    }

    #[test]
    fn layout_is_fully_explicit() {
        // Sum of declared field widths must equal size_of — any
        // compiler-inserted padding would break the AsBytes contract.
        // Tick: 8+4+4+8+8+8+8+1+15 = 64.
        // Signal: 8+4+1+1+2+40+8 = 64.
        // Fill: 8+4+1+3+8+8+8+16+8 = 64.
        // Order: 8+4+1+1+2+8+8+8+1+15+8 = 64.
        assert_eq!(::core::mem::size_of::<Tick>(), 64);
        assert_eq!(::core::mem::size_of::<Signal>(), 64);
        assert_eq!(::core::mem::size_of::<Fill>(), 64);
        assert_eq!(::core::mem::size_of::<Order>(), 64);
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
        let b = unsafe {
            core::slice::from_raw_parts((&e as *const ChannelEvent).cast::<u8>(), 64)
        };
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
        ];
        let mut i = 0;
        while i < all.len() {
            let c = all[i];
            assert_eq!(ChannelId::from_u8(c as u8), Some(c));
            assert!(!c.as_str().is_empty());
            i += 1;
        }
        assert_eq!(ChannelId::from_u8(11), None);
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
        }
    }

    const ALL_KINDS: [AiCmdKind; 10] = [
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
    ];

    #[test]
    fn ai_cmd_size_is_one_cache_line() {
        assert_eq!(::core::mem::size_of::<AiCmd>(), 64);
        assert_eq!(::core::mem::align_of::<AiCmd>(), 64);
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
        assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), 0x1111_2222_3333_4444);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 0xAABB_CCDD);
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 0x0500_0007);
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
        // 10 would be `Resume` if it existed. It must not: halt is
        // sticky by design (risk-policy) — the wire cannot express it.
        assert_eq!(AiCmdKind::from_u8(10), None);
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
        let mut c = valid(AiCmdKind::Heartbeat);
        c.kind = 10;
        assert_eq!(c.validate_shape(), Err(AiCmdShapeError::UnknownKind(10)));
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
        assert_eq!(fv.validate_shape(), Err(AiCmdShapeError::BadSym(SYMBOL_ID_NONE)));
        let mut oi = valid(AiCmdKind::OrderIntent);
        oi.sym = SYMBOL_ID_NONE;
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadSym(SYMBOL_ID_NONE)));
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
        assert_eq!(fv.validate_shape(), Err(AiCmdShapeError::BadStrategySlot(0)));
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
        assert_eq!(oi.validate_shape(), Err(AiCmdShapeError::BadSide(AI_SIDE_NONE)));
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
}
