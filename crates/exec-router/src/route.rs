// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The boot-fixed route table: strategy slot -> execution mode.
//!
//! ## Why the mode lives on `strategy_id`
//!
//! `core_types::Order` already carries `strategy_id` (the slot) and
//! `venue` on every order, and `strategy_set`'s `StampCtx` already
//! stamps the slot around every member callback. So per-strategy
//! routing needs **no new wire field, no capture-format bump and no
//! change to any member** — just a lookup on a byte the order is
//! already carrying. That is the whole reason E1 is cheap.
//!
//! ## The fail-closed law
//!
//! Every lookup that cannot answer with certainty answers
//! [`ExecMode::Paper`]:
//!
//! * `strategy_id` out of range (including `STRATEGY_ID_NONE = 0xFF`,
//!   which every un-stamped order carries) -> Paper;
//! * a mode byte that is not a known discriminant -> Paper;
//! * an absent artifact -> the table is never built, and
//!   [`ExecRoute::all_paper`] is what the engine runs -> Paper.
//!
//! There is no input — no typo, no torn byte, no un-stamped order —
//! that turns into `Live` by accident. `Live` is reachable only by an
//! artifact naming a slot AND `--arm-live` naming the same slot.
//!
//! ## Layout
//!
//! `#[repr(C, align(64))]`, 128 bytes = exactly two cache lines, with
//! **both hot arrays in the first 16 bytes** so the per-submit lookup
//! touches one line and never pointer-chases.

use crate::mode::ExecMode;

/// Strategy slots the table covers. Matches `strategy_set`'s slot
/// count (0 latency-arb · 1 vrp · 2 xsd · 3 bin15 · 4 ai-exec ·
/// 5 vm · 6 icdp · 7 reserved) and is a power of two so the
/// range mask is a single `&`.
pub const EXEC_SLOTS: usize = 8;

/// Every hot-path bounds argument in this module — and in
/// [`crate::counters`] — rests on `EXEC_SLOTS` being a power of two,
/// because that is what makes `& (EXEC_SLOTS - 1)` a total function
/// from `u8` into range and the `get_unchecked` loads sound. Assert it
/// at COMPILE time rather than leaving the invariant to a layout test
/// that a later edit could delete.
const _: () = assert!(EXEC_SLOTS.is_power_of_two());
const _: () = assert!(EXEC_SLOTS <= u8::MAX as usize + 1);

/// Venues a `venue_mask` byte can express — `core_types::VenueId` is
/// 0..=6 today (Polymarket · Binance · Okx · Deribit · Hyperliquid ·
/// Ai · Bybit), so one `u8` covers the domain with a bit to spare.
/// A `venue` byte at or above this fails closed (not allowed).
pub const EXEC_VENUES: u8 = 8;

/// Why a slot could not be written into the table. Boot-time only.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExecRouteErr {
    /// `slot >= EXEC_SLOTS`.
    SlotOutOfRange(usize),
    /// A venue byte at or above [`EXEC_VENUES`].
    VenueOutOfRange(u8),
}

impl core::fmt::Display for ExecRouteErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExecRouteErr::SlotOutOfRange(s) => {
                write!(f, "slot {s} is out of range (0..{EXEC_SLOTS})")
            }
            ExecRouteErr::VenueOutOfRange(v) => {
                write!(f, "venue id {v} is out of range (0..{EXEC_VENUES})")
            }
        }
    }
}

impl std::error::Error for ExecRouteErr {}

/// The per-slot routing decision, fixed at boot and never mutated
/// afterwards (E6's kill switches flip counters and a halt flag, not
/// this table — a halted slot refuses, it does not silently re-route).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ExecRoute {
    /// Index = `strategy_id`. Value = `ExecMode as u8`. **Hot.**
    modes: [u8; EXEC_SLOTS],
    /// Index = `strategy_id`. Bit `i` set = the slot may trade
    /// `VenueId(i)`. **Hot.** Only consulted on the `Live` arm: a
    /// paper slot's venue is the paper matcher's business.
    venue_mask: [u8; EXEC_SLOTS],
    /// Per-slot single-order notional clamp, USD x1e6. Carried by E1
    /// for the boot tell and `/state`; **enforced in E6** (the risk
    /// gate), which is the phase that owns clamping.
    max_order_usd_1e6: [i64; EXEC_SLOTS],
    /// Per-slot open-order clamp. Same status as above.
    max_open_orders: [u32; EXEC_SLOTS],
    /// Pad to exactly two cache lines (128 B). Reserved for E6's
    /// clamps; keeping the size fixed now means adding them later
    /// does not move the hot arrays off line 0.
    _pad: [u8; 16],
}

impl Default for ExecRoute {
    #[inline]
    fn default() -> Self {
        Self::all_paper()
    }
}

impl ExecRoute {
    /// The table the engine runs when there is **no `--exec`**: every
    /// slot paper, every cap zero (unused on the paper arm), no venue
    /// bit set anywhere. Bit for bit, today's behaviour.
    #[inline]
    #[must_use]
    pub const fn all_paper() -> Self {
        Self {
            modes: [ExecMode::Paper as u8; EXEC_SLOTS],
            venue_mask: [0u8; EXEC_SLOTS],
            max_order_usd_1e6: [0i64; EXEC_SLOTS],
            max_open_orders: [0u32; EXEC_SLOTS],
            _pad: [0u8; 16],
        }
    }

    /// Write one slot's decision. **Boot-only, cold, bounds-checked** —
    /// the hot path never mutates the table.
    ///
    /// `venues` is the list of venue ids the slot may reach; an id at
    /// or above [`EXEC_VENUES`] is a hard error rather than a silently
    /// dropped bit, because a dropped bit reads as "this slot may not
    /// trade that venue" and would surface later as a routing refusal
    /// nobody could explain.
    pub fn set_slot(
        &mut self,
        slot: usize,
        mode: ExecMode,
        venues: &[u8],
        max_order_usd_1e6: i64,
        max_open_orders: u32,
    ) -> Result<(), ExecRouteErr> {
        if slot >= EXEC_SLOTS {
            return Err(ExecRouteErr::SlotOutOfRange(slot));
        }
        let mut mask = 0u8;
        let mut i = 0usize;
        while i < venues.len() {
            let v = venues[i];
            if v >= EXEC_VENUES {
                return Err(ExecRouteErr::VenueOutOfRange(v));
            }
            mask |= 1u8 << v;
            i += 1;
        }
        self.modes[slot] = mode.as_u8();
        self.venue_mask[slot] = mask;
        self.max_order_usd_1e6[slot] = max_order_usd_1e6;
        self.max_open_orders[slot] = max_open_orders;
        Ok(())
    }

    /// The slot's mode. **Hot path: one masked load, no branch on the
    /// index, no bounds check.**
    ///
    /// Out-of-range ids (every id >= [`EXEC_SLOTS`], which includes
    /// `core_types::STRATEGY_ID_NONE` = 0xFF) resolve to
    /// [`ExecMode::Paper`] by arithmetic rather than by branch: the
    /// in-range flag multiplies the loaded byte to zero.
    #[inline(always)]
    #[must_use]
    pub fn mode(&self, strategy_id: u8) -> ExecMode {
        let idx = (strategy_id as usize) & (EXEC_SLOTS - 1);
        let in_range = ((strategy_id as usize) < EXEC_SLOTS) as u8;
        // SAFETY: `idx` is masked with `EXEC_SLOTS - 1` and
        // `EXEC_SLOTS` is a power of two, so `idx < EXEC_SLOTS` holds
        // for every possible `u8`. The array is exactly EXEC_SLOTS long.
        let raw = unsafe { *self.modes.get_unchecked(idx) };
        // Out of range -> 0 -> Paper. `wrapping_mul` so a future
        // writer storing a large byte can never panic in debug; the
        // decode maps anything unknown to Paper anyway.
        ExecMode::from_u8(raw.wrapping_mul(in_range))
    }

    /// May this slot reach this venue? **Hot path, branchless.**
    ///
    /// Answers `false` for an out-of-range slot and for a venue byte
    /// at or above [`EXEC_VENUES`] — both fail closed, and the caller
    /// turns a `false` into `DispatchError::NoLiveRoute`, never into a
    /// paper fallback (LAW E-1).
    #[inline(always)]
    #[must_use]
    pub fn venue_allowed(&self, strategy_id: u8, venue: u8) -> bool {
        let idx = (strategy_id as usize) & (EXEC_SLOTS - 1);
        let slot_ok = ((strategy_id as usize) < EXEC_SLOTS) as u8;
        let venue_ok = (venue < EXEC_VENUES) as u8;
        // SAFETY: as in `mode` — `idx` is masked into range.
        let mask = unsafe { *self.venue_mask.get_unchecked(idx) };
        // Masked shift: `venue & 7` is always a legal shift distance
        // for u8, so no UB and no debug panic on a wild venue byte.
        let bit = (mask >> (venue & (EXEC_VENUES - 1))) & 1;
        (bit & slot_ok & venue_ok) == 1
    }

    /// The slot's mode, bounds-checked. Cold: boot tells, `/state`,
    /// the `--arm-live` interlock.
    #[inline]
    #[must_use]
    pub fn mode_at(&self, slot: usize) -> Option<ExecMode> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(ExecMode::from_u8(self.modes[slot]))
    }

    /// Bit `i` set = slot `i` is [`ExecMode::Live`]. Cold; this is
    /// what the `--arm-live` interlock compares against, so it is the
    /// single definition of "the set of slots the artifact arms".
    #[inline]
    #[must_use]
    pub fn live_mask(&self) -> u8 {
        let mut m = 0u8;
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            m |= ((self.modes[i] == ExecMode::Live as u8) as u8) << i;
            i += 1;
        }
        m
    }

    /// Bit `i` set = slot `i` is [`ExecMode::Off`]. Cold; boot tell.
    #[inline]
    #[must_use]
    pub fn off_mask(&self) -> u8 {
        let mut m = 0u8;
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            m |= ((self.modes[i] == ExecMode::Off as u8) as u8) << i;
            i += 1;
        }
        m
    }

    /// The slot's venue bitmask. Cold; boot tell + `/state`.
    #[inline]
    #[must_use]
    pub fn venue_mask_at(&self, slot: usize) -> Option<u8> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.venue_mask[slot])
    }

    /// The slot's single-order notional clamp (USD x1e6). Cold.
    #[inline]
    #[must_use]
    pub fn max_order_usd_1e6_at(&self, slot: usize) -> Option<i64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.max_order_usd_1e6[slot])
    }

    /// The slot's open-order clamp. Cold.
    #[inline]
    #[must_use]
    pub fn max_open_orders_at(&self, slot: usize) -> Option<u32> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.max_open_orders[slot])
    }

    /// Is any slot live? Cold; decides whether the engine needs a live
    /// arm at all.
    #[inline]
    #[must_use]
    pub fn any_live(&self) -> bool {
        self.live_mask() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{STRATEGY_ID_NONE, STRATEGY_SLOT_BIN15};

    #[test]
    fn layout_is_two_cache_lines_with_the_hot_arrays_first() {
        assert_eq!(core::mem::size_of::<ExecRoute>(), 128, "two cache lines");
        assert_eq!(core::mem::align_of::<ExecRoute>(), 64);
        // The hot arrays must sit inside the FIRST cache line, or the
        // per-submit lookup costs two lines instead of one.
        let r = ExecRoute::all_paper();
        let base = core::ptr::addr_of!(r) as usize;
        let modes = core::ptr::addr_of!(r.modes) as usize - base;
        let venues = core::ptr::addr_of!(r.venue_mask) as usize - base;
        assert!(modes + EXEC_SLOTS <= 64, "modes at {modes} spills line 0");
        assert!(venues + EXEC_SLOTS <= 64, "venue_mask at {venues} spills line 0");
    }

    #[test]
    fn all_paper_is_paper_everywhere_including_out_of_range() {
        let r = ExecRoute::all_paper();
        for id in 0u8..=255 {
            assert_eq!(r.mode(id), ExecMode::Paper, "id {id}");
        }
        assert_eq!(r.live_mask(), 0);
        assert_eq!(r.off_mask(), 0);
        assert!(!r.any_live());
    }

    #[test]
    fn strategy_id_none_fails_closed_to_paper_even_when_slot_7_is_live() {
        // 0xFF & 7 == 7. If the range check were missing, an
        // un-stamped order would inherit slot 7's mode — this is the
        // exact aliasing the `in_range` multiply exists to kill.
        let mut r = ExecRoute::all_paper();
        r.set_slot(7, ExecMode::Live, &[4], 0, 0).unwrap();
        assert_eq!(r.mode(7), ExecMode::Live);
        assert_eq!(r.mode(STRATEGY_ID_NONE), ExecMode::Paper);
        assert!(!r.venue_allowed(STRATEGY_ID_NONE, 4));
    }

    #[test]
    fn every_out_of_range_id_is_paper_whatever_the_table_says() {
        let mut r = ExecRoute::all_paper();
        for s in 0..EXEC_SLOTS {
            r.set_slot(s, ExecMode::Live, &[0, 1, 2, 3, 4, 5, 6], i64::MAX, u32::MAX)
                .unwrap();
        }
        for id in 0u8..=255 {
            let want = if (id as usize) < EXEC_SLOTS {
                ExecMode::Live
            } else {
                ExecMode::Paper
            };
            assert_eq!(r.mode(id), want, "id {id}");
        }
    }

    #[test]
    fn bin15_is_slot_three_and_routes_to_hyperliquid_only() {
        let mut r = ExecRoute::all_paper();
        let slot = STRATEGY_SLOT_BIN15 as usize;
        assert_eq!(slot, 3, "the plan's slot map");
        r.set_slot(slot, ExecMode::Live, &[4], 100_000_000, 64).unwrap();

        assert_eq!(r.mode(3), ExecMode::Live);
        assert!(r.venue_allowed(3, 4), "hyperliquid");
        for v in 0u8..EXEC_VENUES {
            if v != 4 {
                assert!(!r.venue_allowed(3, v), "venue {v} must be refused");
            }
        }
        // Every other slot untouched.
        for s in 0u8..8 {
            if s != 3 {
                assert_eq!(r.mode(s), ExecMode::Paper, "slot {s}");
            }
        }
        assert_eq!(r.live_mask(), 0b0000_1000);
        assert!(r.any_live());
    }

    #[test]
    fn a_wild_venue_byte_never_panics_and_never_allows() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(3, ExecMode::Live, &[4], 0, 0).unwrap();
        // Bit 4 is set; without the `venue_ok` term, venue 12
        // (12 & 7 == 4) would alias onto it.
        for v in EXEC_VENUES..=255 {
            assert!(!r.venue_allowed(3, v), "venue byte {v} must fail closed");
        }
    }

    #[test]
    fn off_is_distinct_from_paper() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], 0, 0).unwrap();
        assert_eq!(r.mode(6), ExecMode::Off);
        assert_eq!(r.off_mask(), 0b0100_0000);
        assert_eq!(r.live_mask(), 0);
        assert!(!r.any_live());
    }

    #[test]
    fn out_of_range_writes_are_refused_not_wrapped() {
        let mut r = ExecRoute::all_paper();
        assert_eq!(
            r.set_slot(EXEC_SLOTS, ExecMode::Live, &[4], 0, 0),
            Err(ExecRouteErr::SlotOutOfRange(EXEC_SLOTS))
        );
        assert_eq!(
            r.set_slot(3, ExecMode::Live, &[EXEC_VENUES], 0, 0),
            Err(ExecRouteErr::VenueOutOfRange(EXEC_VENUES))
        );
        // A refused write leaves the table untouched.
        assert_eq!(r, ExecRoute::all_paper());
    }

    #[test]
    fn caps_and_masks_round_trip_through_the_cold_accessors() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(3, ExecMode::Live, &[4, 6], 100_000_000, 64).unwrap();
        assert_eq!(r.mode_at(3), Some(ExecMode::Live));
        assert_eq!(r.venue_mask_at(3), Some(0b0101_0000));
        assert_eq!(r.max_order_usd_1e6_at(3), Some(100_000_000));
        assert_eq!(r.max_open_orders_at(3), Some(64));
        assert_eq!(r.mode_at(EXEC_SLOTS), None);
        assert_eq!(r.venue_mask_at(EXEC_SLOTS), None);
        assert_eq!(r.max_order_usd_1e6_at(EXEC_SLOTS), None);
        assert_eq!(r.max_open_orders_at(EXEC_SLOTS), None);
    }
}
