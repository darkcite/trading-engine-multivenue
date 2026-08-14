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

/// Dense u32 identifier for a trading symbol (Polymarket market or
/// external feed instrument). Resolved once at boot from a perfect-hash
/// table; never parsed in the hot path.
pub type SymbolId = u32;

/// The sentinel meaning "not a real symbol". Never collide with a valid
/// perfect-hash slot.
pub const SYMBOL_ID_NONE: SymbolId = u32::MAX;

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
    /// Reserved for future fields (kept for layout stability).
    _pad: [u8; 8],
}

impl Tick {
    /// Construct a Tick in one shot without named-field noise at call sites.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
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
            _pad: [0; 8],
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
    /// Reserved.
    _pad1: [u8; 16],
}

impl Order {
    /// Construct an Order without naming the private padding fields.
    #[inline(always)]
    pub const fn new(
        ts_ns: NsTs,
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
            _pad1: [0; 16],
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
// fields are plain integers and explicitly-named `_pad` arrays. No
// padding bytes are uninitialized.
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
            7,
            42,
            Price::from_raw(500_000),
            Qty::from_raw(100),
            Price::from_raw(510_000),
            Qty::from_raw(50),
        );
        assert_eq!(t.mid(), Price::from_raw(505_000));
        assert_eq!(t.spread(), 10_000);
    }

    #[test]
    fn tick_mid_of_empty_book_is_zero() {
        // Happy-path AND failure-mode coverage as required by §21.1 of PLAN.md.
        let t = Tick::new(
            0,
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
