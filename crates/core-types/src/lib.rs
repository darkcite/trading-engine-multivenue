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
    /// Seconds-to-minutes (RSS, Claude artifacts).
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

/// External signal (Binance price move, RSS headline, mempool tx, ...).
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
    /// Source identifier (0=binance, 1=rss, 2=rpc, ...). See `SignalSource`.
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
    /// RSS feeds.
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
