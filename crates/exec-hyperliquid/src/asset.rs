// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The asset-id table — the sharpest edge in the whole lane.
//!
//! **LAW E-4 — an order whose asset id was not bound by an
//! `outcomeCreated` roll event for the instance it names is REFUSED.
//! Never derived, never guessed.**
//!
//! Why this is not paranoia: a HIP-4 leg's asset id is
//! `100_000_000 + 10 * outcome_id + side`, and the 15-minute instance
//! changes every quarter hour. Boot discovery is therefore NOT a valid
//! source of the id at submit time — it is stale within minutes. An id
//! that is merely *derivable* is an id that can be derived from the
//! wrong instance, and **a stale asset id is a real order on someone
//! else's market**: correctly signed, correctly sized, and placed on a
//! question nobody asked.
//!
//! So the table is written only from roll events, each slot is stamped
//! with the instance identity the member believes it is trading, and a
//! lookup that disagrees is a refusal plus a counter — never a guess.

/// Slots the table holds: one per live outcome leg. Eight families ×
/// two sides, with room to spare.
pub const ASSET_SLOTS: usize = 32;

/// The HIP-4 asset-id base. `asset = ASSET_BASE + 10 * outcome_id + side`.
pub const ASSET_BASE: u32 = 100_000_000;

/// The largest outcome id whose asset id fits in `u32`.
///
/// Mirrors `ingress_hyperliquid`'s own bound (`discovery.rs:103`),
/// duplicated rather than imported because `exec-hyperliquid` must not
/// depend on the market-data crate (§6.1 ownership ruling). A test
/// holds the two in agreement by re-deriving it.
pub const OUTCOME_ID_MAX: u32 = (u32::MAX - ASSET_BASE - 9) / 10;

/// Longest venue coin name a slot can hold, in bytes.
///
/// The venue echoes outcome legs as `+<enc>` (§6.2 of the plan) and
/// perps by name (`BTC`). Twenty bytes covers an eleven-digit `+<enc>`
/// with room over; a longer name is REFUSED at bind time rather than
/// truncated, because a truncated name is a name that can collide.
pub const COIN_MAX: usize = 20;

/// Why a lookup did not produce an asset id.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AssetError {
    /// No slot holds this symbol. The member asked for an instrument
    /// no roll has bound.
    Unbound,
    /// A slot holds the symbol, but under a DIFFERENT instance than the
    /// caller named — the instance rolled and the caller is stale.
    /// This is the one that would have traded someone else's market.
    StaleInstance {
        /// What the table has.
        bound: u64,
        /// What the caller asked for.
        asked: u64,
    },
    /// The table is full. Boot-time only; a refusal, never an eviction,
    /// because evicting a live leg would unbind an order in flight.
    Full,
    /// The venue coin name does not fit [`COIN_MAX`]. Refused rather
    /// than truncated: a truncated name can collide with another leg,
    /// and a collision here books a fill against the wrong symbol.
    CoinTooLong,
    /// No venue coin name was supplied. Refused because the binding
    /// would authorise orders for a leg whose fills could never
    /// resolve — see [`AssetTable::bind`].
    CoinEmpty,
}

impl core::fmt::Display for AssetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AssetError::Unbound => write!(f, "asset: no roll has bound this instrument"),
            AssetError::StaleInstance { bound, asked } => write!(
                f,
                "asset: instance mismatch — the table holds {bound}, the caller named {asked}"
            ),
            AssetError::Full => write!(f, "asset: table full ({ASSET_SLOTS} slots)"),
            AssetError::CoinTooLong => {
                write!(f, "asset: venue coin name longer than {COIN_MAX} bytes")
            }
            AssetError::CoinEmpty => write!(f, "asset: no venue coin name supplied"),
        }
    }
}

impl std::error::Error for AssetError {}

/// One bound leg. Exactly one cache line; a test holds that.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct AssetSlot {
    /// Engine symbol id.
    sym: u32,
    /// Venue asset id.
    asset: u32,
    /// The instance this binding belongs to — the identity the roll
    /// carried. A lookup must name the same one.
    instance: u64,
    /// The venue's OWN name for this leg, as it appears in a
    /// `userFills` frame. Written by the roll, never parsed out of a
    /// fill — see [`AssetTable::sym_of_coin`].
    coin: [u8; COIN_MAX],
    /// The name this slot held BEFORE the last roll. One generation,
    /// and only for the reverse (fill) direction — see
    /// [`AssetTable::sym_of_coin`].
    prev_coin: [u8; COIN_MAX],
    /// Whether the slot holds anything.
    live: bool,
    coin_len: u8,
    prev_coin_len: u8,
    _pad: [u8; 5],
}

impl Default for AssetSlot {
    #[inline]
    fn default() -> Self {
        Self::EMPTY
    }
}

impl AssetSlot {
    const EMPTY: Self = Self {
        sym: 0,
        asset: 0,
        instance: 0,
        coin: [0; COIN_MAX],
        prev_coin: [0; COIN_MAX],
        live: false,
        coin_len: 0,
        prev_coin_len: 0,
        _pad: [0; 5],
    };
}

/// Symbol → asset id, bound from roll events only.
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct AssetTable {
    slots: [AssetSlot; ASSET_SLOTS],
    len: usize,
}

impl Default for AssetTable {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl AssetTable {
    /// An empty table. Until a roll binds something, EVERY lookup
    /// fails — which is the correct state for a member that has not
    /// yet seen the market it wants to trade.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [AssetSlot::EMPTY; ASSET_SLOTS],
            len: 0,
        }
    }

    /// Compute the venue asset id for an outcome leg. Pure arithmetic —
    /// exposed so a roll handler can call it, and deliberately NOT
    /// reachable from the submit path, where deriving is the bug.
    /// `None` for an `outcome_id` past [`OUTCOME_ID_MAX`] or a `side`
    /// past 9, where `ASSET_BASE + enc` does not fit in `u32`.
    ///
    /// **A real refusal, not a `debug_assert!`.** The release profile
    /// sets `overflow-checks = false`, so a debug-only guard is a
    /// no-op in the artifact that actually trades — and the wrap does
    /// not land somewhere harmless: it lands in the LOW `u32` range,
    /// which is the perp asset-id space. A nonsense outcome id would
    /// silently become a valid BTC or ETH perp asset id. That is
    /// literally "a real order on someone else's market", the one
    /// thing this module exists to prevent, so it is refused at
    /// runtime in every profile. The mirror, [`Self::outcome_coin`],
    /// refuses the same inputs — and it only produces a NAME, which is
    /// harmless; this produces an ORDER.
    #[inline(always)]
    #[must_use]
    pub const fn asset_id(outcome_id: u32, side: u8) -> Option<u32> {
        if outcome_id > OUTCOME_ID_MAX || side > 9 {
            return None;
        }
        Some(ASSET_BASE + 10 * outcome_id + side as u32)
    }

    /// The venue's coin name for an outcome leg, rendered into
    /// `out`, returning the written length.
    ///
    /// The mirror of [`Self::asset_id`] and held to the same rule: it
    /// exists so a ROLL HANDLER has one place to compute the name, and
    /// it is deliberately NOT reachable from the fill path. Deriving a
    /// name while booking a fill is the same class of mistake as
    /// deriving an asset id while placing an order — see
    /// [`Self::sym_of_coin`].
    ///
    /// Returns `None` — writing nothing — for an outcome id past
    /// [`OUTCOME_ID_MAX`], where `ASSET_BASE + enc` would not fit in
    /// `u32`. Fail-closed: a roll carrying a nonsense id produces no
    /// name, so `bind` gets nothing to bind and no fill can ever
    /// resolve to it. (This is not hypothetical — the first version of
    /// this function computed `10 * outcome_id + side` unchecked and
    /// its own test caught the overflow.)
    #[must_use]
    pub fn outcome_coin(outcome_id: u32, side: u8, out: &mut [u8; COIN_MAX]) -> Option<usize> {
        if outcome_id > OUTCOME_ID_MAX || side > 9 {
            return None;
        }
        let enc = 10 * outcome_id + side as u32;
        out[0] = b'+';
        let mut digits = [0u8; 10];
        let mut n = 0usize;
        let mut v = enc;
        loop {
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
            if v == 0 {
                break;
            }
        }
        let mut i = 0usize;
        while i < n {
            out[1 + i] = digits[n - 1 - i];
            i += 1;
        }
        Some(1 + n)
    }

    /// Bind (or rebind) a symbol from a roll event.
    ///
    /// `coin` is the venue's OWN name for the leg — what a `userFills`
    /// frame will echo. It is carried here rather than derived at fill
    /// time so that the table stays the single authority for what this
    /// member trades: a fill naming a coin no roll bound resolves to
    /// nothing and is counted, never booked.
    ///
    /// Rebinding the same symbol to a NEW instance is the normal
    /// quarter-hour roll and overwrites in place, keeping the previous
    /// name for one generation.
    pub fn bind(
        &mut self,
        sym: u32,
        asset: u32,
        instance: u64,
        coin: &[u8],
    ) -> Result<(), AssetError> {
        if coin.is_empty() {
            // Binding an asset id with no name authorises ORDERS while
            // leaving every fill for that leg permanently unresolvable
            // — `sym_of_coin` refuses empty input. A roll handler
            // written as `bind(.., &out[..n.unwrap_or(0)])` lands
            // exactly here, so the refusal belongs at this end too:
            // `outcome_coin`'s fail-closed `None` is only fail-closed
            // if `bind` will not accept its absence.
            return Err(AssetError::CoinEmpty);
        }
        if coin.len() > COIN_MAX {
            return Err(AssetError::CoinTooLong);
        }
        for s in self.slots.iter_mut() {
            if s.live && s.sym == sym {
                // The roll: the name this slot held becomes the
                // previous generation, so a fill still in flight from
                // the instance that just ended can still be resolved.
                s.prev_coin = s.coin;
                s.prev_coin_len = s.coin_len;
                s.coin = [0; COIN_MAX];
                s.coin[..coin.len()].copy_from_slice(coin);
                s.coin_len = coin.len() as u8;
                s.asset = asset;
                s.instance = instance;
                return Ok(());
            }
        }
        for s in self.slots.iter_mut() {
            if !s.live {
                *s = AssetSlot::EMPTY;
                s.sym = sym;
                s.asset = asset;
                s.instance = instance;
                s.coin[..coin.len()].copy_from_slice(coin);
                s.coin_len = coin.len() as u8;
                s.live = true;
                self.len += 1;
                return Ok(());
            }
        }
        Err(AssetError::Full)
    }

    /// Release a symbol — a settled instance whose leg is gone.
    pub fn unbind(&mut self, sym: u32) -> bool {
        for s in self.slots.iter_mut() {
            if s.live && s.sym == sym {
                *s = AssetSlot::EMPTY;
                self.len -= 1;
                return true;
            }
        }
        false
    }

    /// The engine symbol a venue coin name belongs to, or `None`.
    ///
    /// **The fill path's LAW E-4.** The venue echoes a coin name in
    /// every `userFills` row; the engine routes on its own `SymbolId`.
    /// This resolves one to the other by COMPARING BYTES against what
    /// a roll bound — it never parses `+<enc>` back into an asset id,
    /// because a fill booked against the wrong symbol moves a position
    /// the member never took, silently, in the tape, forever. A
    /// missing fill is caught by reconciliation inside a minute; a
    /// misattributed one is caught by nobody. So an unrecognised coin
    /// resolves to `None` and its fill is counted, not booked.
    ///
    /// One generation of memory, and ONLY here: a fill from the
    /// instance that just rolled still resolves, because it is a
    /// position the member really took. The forward direction
    /// ([`Self::lookup`], which authorises ORDERS) has no such memory
    /// and still refuses a stale instance outright — placing an order
    /// on a rolled instance is the catastrophe LAW E-4 exists for,
    /// while booking a late fill from an instance we genuinely traded
    /// is simply correct. The asymmetry is deliberate.
    ///
    /// **TWO PASSES, and that is load-bearing.** A live current name
    /// must outrank a dead previous-generation one wherever each
    /// happens to sit in the array. The first version tested both at
    /// equal precedence inside one scan, so with slot 0 holding
    /// `{sym: 42, prev_coin: "+A"}` and slot 1 holding
    /// `{sym: 99, coin: "+A"}`, a fill for `+A` resolved to 42 — a
    /// *dead* name on a lower slot beating the *live* owner. Not
    /// `None`, not the right symbol: confidently wrong, on the fill
    /// path, which is the one failure this whole design exists to
    /// prevent. Two passes over 32 cache-line slots is not a cost
    /// worth trading for that.
    ///
    /// **An ambiguous match is `None`.** If two live slots claim the
    /// same current name the table is in a state no roll should
    /// produce, and a guess between them is exactly the misattribution
    /// being avoided. Counted, not booked.
    #[inline]
    #[must_use]
    pub fn sym_of_coin(&self, coin: &[u8]) -> Option<u32> {
        if coin.is_empty() || coin.len() > COIN_MAX {
            return None;
        }
        let n = coin.len();

        // Pass 1 — the CURRENT generation, every slot.
        let mut hit: Option<u32> = None;
        let mut i = 0usize;
        while i < ASSET_SLOTS {
            // SAFETY: `i` is bounded by the loop condition and the
            // array is exactly ASSET_SLOTS long.
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.live && usize::from(s.coin_len) == n && &s.coin[..n] == coin {
                if hit.is_some() {
                    return None;
                }
                hit = Some(s.sym);
            }
            i += 1;
        }
        if hit.is_some() {
            return hit;
        }

        // Pass 2 — one generation back, and only because pass 1 found
        // nothing live that owns this name.
        let mut i = 0usize;
        while i < ASSET_SLOTS {
            // SAFETY: as above.
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.live && usize::from(s.prev_coin_len) == n && &s.prev_coin[..n] == coin {
                if hit.is_some() {
                    return None;
                }
                hit = Some(s.sym);
            }
            i += 1;
        }
        hit
    }

    /// The asset id for `sym`, **only if** it is bound to `instance`.
    ///
    /// Hot path, and the enforcement point for LAW E-4.
    #[inline(always)]
    pub fn lookup(&self, sym: u32, instance: u64) -> Result<u32, AssetError> {
        let mut i = 0usize;
        while i < ASSET_SLOTS {
            // SAFETY: `i` is bounded by the loop condition and the
            // array is exactly ASSET_SLOTS long.
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.live && s.sym == sym {
                if s.instance == instance {
                    return Ok(s.asset);
                }
                return Err(AssetError::StaleInstance {
                    bound: s.instance,
                    asked: instance,
                });
            }
            i += 1;
        }
        Err(AssetError::Unbound)
    }

    /// How many legs are bound. Cold; boot tell and `/state`.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Nothing bound.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formula_matches_the_venues() {
        // outcome 3253, YES side 0 / NO side 1 — the ids in the vectors.
        assert_eq!(AssetTable::asset_id(3253, 0).expect("in range"), 100_032_530);
        assert_eq!(AssetTable::asset_id(3253, 1).expect("in range"), 100_032_531);
        assert_eq!(AssetTable::asset_id(0, 0).expect("in range"), 100_000_000);
    }

    #[test]
    fn an_empty_table_refuses_everything() {
        let t = AssetTable::new();
        assert!(t.is_empty());
        assert_eq!(t.lookup(42, 1), Err(AssetError::Unbound));
    }

    /// LAW E-4, the load-bearing test: the id is right, the instance
    /// has rolled, and the answer is a REFUSAL rather than the id.
    #[test]
    fn a_stale_instance_is_refused_even_though_the_symbol_is_bound() {
        let mut t = AssetTable::new();
        t.bind(42, AssetTable::asset_id(3253, 0).expect("in range"), 1_000, b"+32530").unwrap();
        assert_eq!(t.lookup(42, 1_000), Ok(100_032_530));
        // The quarter rolled. The caller still believes in the old one.
        assert_eq!(
            t.lookup(42, 999),
            Err(AssetError::StaleInstance {
                bound: 1_000,
                asked: 999
            }),
            "a stale asset id is a real order on someone else's market"
        );
    }

    #[test]
    fn a_roll_rebinds_in_place() {
        let mut t = AssetTable::new();
        t.bind(42, AssetTable::asset_id(3253, 0).expect("in range"), 1_000, b"+32530").unwrap();
        assert_eq!(t.len(), 1);
        t.bind(42, AssetTable::asset_id(3254, 0).expect("in range"), 1_001, b"+32540").unwrap();
        assert_eq!(t.len(), 1, "a roll must not consume a second slot");
        assert_eq!(t.lookup(42, 1_001), Ok(100_032_540));
        assert!(t.lookup(42, 1_000).is_err(), "the old instance is gone");
    }

    #[test]
    fn unbind_frees_the_slot() {
        let mut t = AssetTable::new();
        t.bind(7, 100_000_001, 1, b"+1").unwrap();
        assert!(t.unbind(7));
        assert!(t.is_empty());
        assert_eq!(t.lookup(7, 1), Err(AssetError::Unbound));
        assert!(!t.unbind(7), "unbinding twice is not an error, just false");
    }

    #[test]
    fn a_full_table_refuses_rather_than_evicting() {
        let mut t = AssetTable::new();
        for i in 0..ASSET_SLOTS as u32 {
            t.bind(i, 100_000_000 + i, 1, b"+0").unwrap();
        }
        // Evicting here would unbind a leg that may have an order in
        // flight against it.
        assert_eq!(t.bind(999, 1, 1, b"+9"), Err(AssetError::Full));
        // And everything already bound still resolves.
        assert_eq!(t.lookup(0, 1), Ok(100_000_000));
    }

    #[test]
    fn the_table_is_cache_line_aligned() {
        assert_eq!(core::mem::align_of::<AssetTable>(), 64);
    }

    #[test]
    fn a_slot_is_exactly_one_cache_line() {
        assert_eq!(
            core::mem::size_of::<AssetSlot>(),
            64,
            "a slot that straddles a cache line costs a second fetch \
             on every step of the reverse scan"
        );
    }

    #[test]
    fn the_coin_name_matches_the_asset_formula() {
        let mut out = [0u8; COIN_MAX];
        let n = AssetTable::outcome_coin(3253, 0, &mut out).expect("in range");
        assert_eq!(&out[..n], b"+32530");
        assert_eq!(AssetTable::asset_id(3253, 0).expect("in range"), ASSET_BASE + 32_530);
        let n = AssetTable::outcome_coin(0, 0, &mut out).expect("in range");
        assert_eq!(&out[..n], b"+0");

        // The bound itself, and one past it.
        let n = AssetTable::outcome_coin(OUTCOME_ID_MAX, 9, &mut out).expect("at the bound");
        assert!(n <= COIN_MAX);
        assert_eq!(
            AssetTable::asset_id(OUTCOME_ID_MAX, 9).expect("in range"),
            ASSET_BASE + 10 * OUTCOME_ID_MAX + 9,
            "the bound is exactly where the u32 sum still fits"
        );
        assert_eq!(
            AssetTable::outcome_coin(OUTCOME_ID_MAX + 1, 0, &mut out),
            None,
            "an out-of-range outcome id must produce NO name, not a wrapped one"
        );
        // The ORDER-producing half refuses the same inputs, and at
        // RUNTIME: release sets overflow-checks = false, and the wrap
        // lands in the perp asset-id space.
        assert_eq!(AssetTable::asset_id(OUTCOME_ID_MAX + 1, 0), None);
        assert_eq!(AssetTable::asset_id(1, 10), None);
        assert!(
            AssetTable::asset_id(OUTCOME_ID_MAX, 9).is_some(),
            "the bound itself must still be usable"
        );
        assert_eq!(AssetTable::outcome_coin(1, 10, &mut out), None, "side is one digit");

        // Agreement with ingest's own bound (discovery.rs:103), which
        // this crate deliberately does not import.
        assert_eq!(
            u64::from(OUTCOME_ID_MAX),
            ((u32::MAX - 100_000_000 - 9) / 10) as u64
        );
    }

    /// The whole point of the reverse direction: a coin no roll bound
    /// resolves to NOTHING, so its fill is counted and never booked.
    #[test]
    fn an_unbound_coin_resolves_to_nothing() {
        let mut t = AssetTable::new();
        t.bind(42, 100_032_530, 1, b"+32530").unwrap();
        assert_eq!(t.sym_of_coin(b"+32530"), Some(42));
        assert_eq!(t.sym_of_coin(b"+99999"), None, "never bound");
        assert_eq!(t.sym_of_coin(b"BTC"), None, "a perp we do not trade");
        assert_eq!(t.sym_of_coin(b""), None);
        // A PREFIX must not match: truncation is how a name collides.
        assert_eq!(t.sym_of_coin(b"+3253"), None);
        assert_eq!(t.sym_of_coin(b"+325300"), None);
    }

    /// A fill still in flight when the quarter rolls is a position the
    /// member really took. The reverse direction remembers one
    /// generation; the forward direction (which authorises ORDERS)
    /// deliberately does not.
    #[test]
    fn a_fill_from_the_instance_that_just_rolled_still_resolves() {
        let mut t = AssetTable::new();
        t.bind(42, AssetTable::asset_id(3253, 0).expect("in range"), 1_000, b"+32530")
            .unwrap();
        t.bind(42, AssetTable::asset_id(3254, 0).expect("in range"), 1_001, b"+32540")
            .unwrap();
        assert_eq!(t.sym_of_coin(b"+32540"), Some(42), "the live leg");
        assert_eq!(
            t.sym_of_coin(b"+32530"),
            Some(42),
            "a late fill from the instance that just ended is OURS"
        );
        // But an ORDER naming the old instance is still refused.
        assert!(
            t.lookup(42, 1_000).is_err(),
            "the forward direction must NOT have grown a memory"
        );
        // Two generations back is gone.
        t.bind(42, AssetTable::asset_id(3255, 0).expect("in range"), 1_002, b"+32550")
            .unwrap();
        assert_eq!(t.sym_of_coin(b"+32530"), None, "only ONE generation");
    }

    /// **The bug this two-pass scan exists for.** A dead
    /// previous-generation name on a LOWER slot must not outrank a
    /// live current name on a higher one. The first version tested
    /// both at equal precedence in a single scan and returned 42 here
    /// — not `None`, not 99: confidently wrong, on the fill path.
    #[test]
    fn a_live_owner_outranks_another_slots_previous_generation() {
        let mut t = AssetTable::new();
        // Slot 0: sym 42 rolls off "+A", which becomes its prev.
        t.bind(42, 100_000_010, 1, b"+A").unwrap();
        t.bind(42, 100_000_020, 2, b"+B").unwrap();
        // Slot 1: "+A" is now LIVE for a different symbol.
        t.bind(99, 100_000_030, 3, b"+A").unwrap();
        assert_eq!(
            t.sym_of_coin(b"+A"),
            Some(99),
            "a dead prev-generation name beat the live owner"
        );
        assert_eq!(t.sym_of_coin(b"+B"), Some(42));
    }

    /// The mirror: slot order must be irrelevant, so binding the same
    /// pair in the opposite order gives the same answer.
    #[test]
    fn slot_order_does_not_decide_which_symbol_a_coin_belongs_to() {
        let mut t = AssetTable::new();
        // Slot 0 holds the LIVE "+A" this time.
        t.bind(99, 100_000_030, 3, b"+A").unwrap();
        t.bind(42, 100_000_010, 1, b"+Z").unwrap();
        t.bind(42, 100_000_020, 2, b"+B").unwrap();
        assert_eq!(t.sym_of_coin(b"+A"), Some(99));
        assert_eq!(t.sym_of_coin(b"+Z"), Some(42), "one generation back");
    }

    /// Two live slots claiming the same current name is a state no
    /// roll should produce. Guessing between them is the
    /// misattribution this module exists to avoid, so it is `None`.
    #[test]
    fn an_ambiguous_current_name_resolves_to_nothing() {
        let mut t = AssetTable::new();
        t.bind(1, 100_000_010, 1, b"+DUP").unwrap();
        t.bind(2, 100_000_020, 1, b"+DUP").unwrap();
        assert_eq!(
            t.sym_of_coin(b"+DUP"),
            None,
            "a guess between two live claimants is exactly what must not happen"
        );
    }

    /// The second pass has its own ambiguity branch, and nothing
    /// constructed it. Two symbols that each rolled OFF the same name
    /// leave it as both their previous generations — with no live
    /// claimant, so pass 1 misses and pass 2 finds two. A guess there
    /// is the same misattribution as a guess in pass 1.
    #[test]
    fn a_name_two_symbols_both_rolled_off_resolves_to_nothing() {
        let mut t = AssetTable::new();
        t.bind(1, 100_000_010, 1, b"+A").unwrap();
        t.bind(1, 100_000_020, 2, b"+B").unwrap();
        t.bind(2, 100_000_030, 1, b"+A").unwrap();
        t.bind(2, 100_000_040, 2, b"+C").unwrap();
        // "+A" is now nobody's CURRENT name and two symbols' previous.
        assert_eq!(t.sym_of_coin(b"+B"), Some(1));
        assert_eq!(t.sym_of_coin(b"+C"), Some(2));
        assert_eq!(
            t.sym_of_coin(b"+A"),
            None,
            "two prev-generation claimants must not be guessed between"
        );
    }

    /// A binding with no name would authorise ORDERS for a leg whose
    /// fills can never resolve — `sym_of_coin` refuses empty input.
    #[test]
    fn a_binding_with_no_coin_name_is_refused() {
        let mut t = AssetTable::new();
        assert_eq!(t.bind(1, 100_000_010, 1, b""), Err(AssetError::CoinEmpty));
        assert!(t.is_empty(), "a refused bind must consume no slot");
        assert!(
            t.lookup(1, 1).is_err(),
            "and must not have authorised an order either"
        );
    }

    #[test]
    fn a_coin_name_that_does_not_fit_is_refused_not_truncated() {
        let mut t = AssetTable::new();
        let long = [b'x'; COIN_MAX + 1];
        assert_eq!(t.bind(1, 1, 1, &long), Err(AssetError::CoinTooLong));
        assert!(t.is_empty(), "a refused bind must consume no slot");
    }

    #[test]
    fn unbind_forgets_the_coin_too() {
        let mut t = AssetTable::new();
        t.bind(7, 100_000_001, 1, b"+1").unwrap();
        assert_eq!(t.sym_of_coin(b"+1"), Some(7));
        assert!(t.unbind(7));
        assert_eq!(t.sym_of_coin(b"+1"), None, "a settled leg is GONE");
    }
}
