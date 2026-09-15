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
        }
    }
}

impl std::error::Error for AssetError {}

/// One bound leg.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
struct AssetSlot {
    /// Engine symbol id.
    sym: u32,
    /// Venue asset id.
    asset: u32,
    /// The instance this binding belongs to — the identity the roll
    /// carried. A lookup must name the same one.
    instance: u64,
    /// Whether the slot holds anything.
    live: bool,
    _pad: [u8; 7],
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
            slots: [AssetSlot {
                sym: 0,
                asset: 0,
                instance: 0,
                live: false,
                _pad: [0; 7],
            }; ASSET_SLOTS],
            len: 0,
        }
    }

    /// Compute the venue asset id for an outcome leg. Pure arithmetic —
    /// exposed so a roll handler can call it, and deliberately NOT
    /// reachable from the submit path, where deriving is the bug.
    #[inline(always)]
    #[must_use]
    pub const fn asset_id(outcome_id: u32, side: u8) -> u32 {
        ASSET_BASE + 10 * outcome_id + side as u32
    }

    /// Bind (or rebind) a symbol from a roll event.
    ///
    /// Rebinding the same symbol to a NEW instance is the normal
    /// quarter-hour roll and overwrites in place.
    pub fn bind(&mut self, sym: u32, asset: u32, instance: u64) -> Result<(), AssetError> {
        for s in self.slots.iter_mut() {
            if s.live && s.sym == sym {
                s.asset = asset;
                s.instance = instance;
                return Ok(());
            }
        }
        for s in self.slots.iter_mut() {
            if !s.live {
                *s = AssetSlot {
                    sym,
                    asset,
                    instance,
                    live: true,
                    _pad: [0; 7],
                };
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
                *s = AssetSlot {
                    sym: 0,
                    asset: 0,
                    instance: 0,
                    live: false,
                    _pad: [0; 7],
                };
                self.len -= 1;
                return true;
            }
        }
        false
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
        assert_eq!(AssetTable::asset_id(3253, 0), 100_032_530);
        assert_eq!(AssetTable::asset_id(3253, 1), 100_032_531);
        assert_eq!(AssetTable::asset_id(0, 0), 100_000_000);
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
        t.bind(42, AssetTable::asset_id(3253, 0), 1_000).unwrap();
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
        t.bind(42, AssetTable::asset_id(3253, 0), 1_000).unwrap();
        assert_eq!(t.len(), 1);
        t.bind(42, AssetTable::asset_id(3254, 0), 1_001).unwrap();
        assert_eq!(t.len(), 1, "a roll must not consume a second slot");
        assert_eq!(t.lookup(42, 1_001), Ok(100_032_540));
        assert!(t.lookup(42, 1_000).is_err(), "the old instance is gone");
    }

    #[test]
    fn unbind_frees_the_slot() {
        let mut t = AssetTable::new();
        t.bind(7, 100_000_001, 1).unwrap();
        assert!(t.unbind(7));
        assert!(t.is_empty());
        assert_eq!(t.lookup(7, 1), Err(AssetError::Unbound));
        assert!(!t.unbind(7), "unbinding twice is not an error, just false");
    }

    #[test]
    fn a_full_table_refuses_rather_than_evicting() {
        let mut t = AssetTable::new();
        for i in 0..ASSET_SLOTS as u32 {
            t.bind(i, 100_000_000 + i, 1).unwrap();
        }
        // Evicting here would unbind a leg that may have an order in
        // flight against it.
        assert_eq!(t.bind(999, 1, 1), Err(AssetError::Full));
        // And everything already bound still resolves.
        assert_eq!(t.lookup(0, 1), Ok(100_000_000));
    }

    #[test]
    fn the_table_is_cache_line_aligned() {
        assert_eq!(core::mem::align_of::<AssetTable>(), 64);
    }
}
