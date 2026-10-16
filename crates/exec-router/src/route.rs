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
//! `#[repr(C, align(64))]`, 1 664 bytes = twenty-six cache lines, with
//! **both hot arrays in the first 48 bytes** (`modes` 16 B at 0,
//! `venue_mask` 32 B at 16) so the per-submit lookup touches one line
//! and never pointer-chases; the E6 clamps fill the rest of lines 0–7,
//! the halt thresholds sit on lines 8–21 and XH4e's perp limits on
//! lines 22–25, none of which a paper submit ever reads. (512 B until
//! E7's session bound grew `HaltLimits` 32 → 48 B, 640 B until XH4d's
//! ALO threshold grew it to 56 B, 704 B until XH4e's `PerpLimits`, 832 B
//! at eight slots until S16-3 doubled every array; the halt table is
//! read on the halt poll and the perp limits on a live order only, so
//! the extra lines cost nothing hot.) Sixteen slots is the most the
//! one-line lookup holds: 16 + 2 × 16 = 48 B, and 32 slots would be 96.

use crate::mode::ExecMode;
use core_types::SlotMask;

/// Strategy slots the table covers: `core_types::MAX_STRATEGY_SLOTS`
/// (16 since S16-3; 0 hyparb · 1 vrp · 2 xsd · 3 bin15 · 4 ai-exec ·
/// 5 vm · 6 xmm · 7 hcv · 8–15 reserved), derived rather than restated,
/// and a power of two so the range mask is a single `&`.
pub const EXEC_SLOTS: usize = core_types::MAX_STRATEGY_SLOTS as usize;

/// Every hot-path bounds argument in this module — and in
/// [`crate::counters`] — rests on `EXEC_SLOTS` being a power of two,
/// because that is what makes `& (EXEC_SLOTS - 1)` a total function
/// from `u8` into range and the `get_unchecked` loads sound. Assert it
/// at COMPILE time rather than leaving the invariant to a layout test
/// that a later edit could delete.
const _: () = assert!(EXEC_SLOTS.is_power_of_two());
const _: () = assert!(EXEC_SLOTS <= u8::MAX as usize + 1);
// The live and off masks are `SlotMask`s: one bit per slot.
const _: () = assert!(EXEC_SLOTS <= core_types::SlotMask::BITS as usize);
// `split::NO_SLOT` (0xFE) and `STRATEGY_ID_NONE` (0xFF) must stay out
// of range: the `in_range` multiply in `mode`/`venue_allowed` is what
// keeps `0xFF & (EXEC_SLOTS - 1)` (= 15) from reading slot 15's mode.
const _: () = assert!(EXEC_SLOTS <= 0xFE);

/// Venues a `venue_mask` word can express. `core_types::VenueId` is
/// 0..=8 since HYPARB (O-H11: … · Bybit · Mexc · HyperEvm), which
/// overflowed the original `u8` mask — `mask >> (venue & 7)` would have
/// aliased venue 8 onto 0 (Polymarket). The mask is a `u16` and this is
/// 16: a power of two, so the masked shift below stays a single `&`,
/// with seven venues of headroom. A `venue` byte at or above this fails
/// closed (not allowed).
pub const EXEC_VENUES: u8 = 16;
const _: () = assert!((EXEC_VENUES as u32).is_power_of_two() && EXEC_VENUES as u32 <= u16::BITS);
const _: () = assert!(core_types::VENUE_COUNT <= EXEC_VENUES as usize);

/// Why a slot could not be written into the table. Boot-time only.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExecRouteErr {
    /// `slot >= EXEC_SLOTS`.
    SlotOutOfRange(usize),
    /// A venue byte at or above [`EXEC_VENUES`].
    VenueOutOfRange(u8),
    /// XH4e: a slot's perp limits set one clamp and not the other, or a
    /// negative one — a perp slot is judged by both, or it is none.
    PerpLimitsPartial(usize),
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
            ExecRouteErr::PerpLimitsPartial(s) => {
                write!(f, "slot {s}'s perp limits must set both clamps or neither")
            }
        }
    }
}

impl std::error::Error for ExecRouteErr {}

/// One slot's operator-set clamps, as a named group.
///
/// A struct rather than four more positional arguments to
/// [`ExecRoute::set_slot`]. Three of the four are `i64` USD ×1e6 and
/// the call sites sit in config plumbing where the values are read out
/// of a parsed table in whatever order the struct happens to list
/// them: transposing `cap_day` and `cap_instance` at a call site would
/// compile, and the resulting engine would clamp a whole day's
/// turnover at one instance's number without a single test noticing.
/// Named fields make that transposition unwriteable.
///
/// `0` means UNSET for every field, which is what `exec.toml` means by
/// omitting the key, and what an all-paper table carries. **Unset is
/// not "unlimited"** — see [`ExecRoute::max_order_usd_1e6_at`] and the
/// refusal it drives: a live slot with no cap refuses, because a cap
/// the operator never wrote is not permission.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotCaps {
    /// Single-order notional clamp, USD ×1e6.
    pub max_order_usd_1e6: i64,
    /// **E6** — per-instance clamp, USD ×1e6, measured as NET
    /// EXPOSURE (`|yes − no|` × $1 a contract), not as turnover.
    pub cap_instance_usd_1e6: i64,
    /// **E6** — day clamp, USD ×1e6, measured as filled BUY TURNOVER
    /// and reset at 00:00Z. Cumulative within the day: unlike the
    /// instance cap it does not fall as positions net off, because a
    /// day cap that a member could churn under is not a day cap.
    pub cap_day_usd_1e6: i64,
    /// Open-order clamp, orders.
    pub max_open_orders: u32,
    /// Explicit tail padding, so the struct is 32 B and a future
    /// `u32` clamp costs nothing.
    _pad: u32,
}

impl SlotCaps {
    /// Every clamp unset. Same as `Default`, but usable in `const`.
    #[inline]
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_order_usd_1e6: 0,
            cap_instance_usd_1e6: 0,
            cap_day_usd_1e6: 0,
            max_open_orders: 0,
            _pad: 0,
        }
    }

    /// Build a set of clamps. `const` so a boot table can be a
    /// constant and a test can spell one inline.
    #[inline]
    #[must_use]
    pub const fn new(
        max_order_usd_1e6: i64,
        cap_instance_usd_1e6: i64,
        cap_day_usd_1e6: i64,
        max_open_orders: u32,
    ) -> Self {
        Self {
            max_order_usd_1e6,
            cap_instance_usd_1e6,
            cap_day_usd_1e6,
            max_open_orders,
            _pad: 0,
        }
    }
}

/// **E6 — one slot's halt thresholds.**
///
/// A struct beside [`SlotCaps`], for the same reason: four numbers,
/// three of them the same type, read out of a parsed table.
///
/// `0` means UNSET for every field, and `core_config::exec` refuses a
/// live slot that leaves any of them at zero — a halt trigger with no
/// threshold is a sensor wired to nothing, which an operator would
/// only discover by the halt never firing.
///
/// **The sensors are VENUE-WIDE; the thresholds are PER-SLOT.** A
/// reject streak, a WS gap and a reconciliation drift are properties
/// of the arm, not of any one member. Each live slot compares the
/// arm's signal against its own numbers, so a slot with a tighter
/// threshold halts first and a slot on a different venue is
/// untouched.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HaltLimits {
    /// Reconciliation drift, USD ×1e6, that trips a halt.
    pub recon_drift_usd_1e6: i64,
    /// Milliseconds without the venue's user-event stream.
    pub ws_gap_ms: i64,
    /// Milliseconds since the last reconciliation that AGREED. A
    /// reconciler that stops answering — or stops agreeing — is the
    /// safety net going dark, and the first cut could not see it.
    pub recon_stale_ms: i64,
    /// Consecutive venue rejections.
    pub reject_streak: u32,
    /// Consecutive LAW E-4 refusals (an order naming a rolled
    /// instance).
    pub asset_refusal_streak: u32,
    /// E7 session bound: spot-USDC gain over the anchor, USD ×1e6, at
    /// which the slot halts (judged flat only). `0` = no bound.
    pub pnl_gain_usd_1e6: i64,
    /// E7 session bound: spot-USDC loss under the anchor, USD ×1e6,
    /// at which the slot halts (judged flat only). `0` = no bound.
    pub pnl_loss_usd_1e6: i64,
    /// XH4d, XH-7: post-only crosses among the arm's last ALO answers,
    /// permille, at which the slot halts. `0` = off (XH4f maps
    /// `halt_on_alo_reject_pct` here, ×10).
    pub alo_reject_permille: u16,
    /// Explicit padding.
    _pad: [u8; 6],
}

const _: () = assert!(core::mem::size_of::<HaltLimits>() == 56);

impl HaltLimits {
    /// Every threshold unset. Usable in `const`.
    #[inline]
    #[must_use]
    pub const fn none() -> Self {
        Self {
            recon_drift_usd_1e6: 0,
            ws_gap_ms: 0,
            recon_stale_ms: 0,
            reject_streak: 0,
            asset_refusal_streak: 0,
            pnl_gain_usd_1e6: 0,
            pnl_loss_usd_1e6: 0,
            alo_reject_permille: 0,
            _pad: [0; 6],
        }
    }

    /// Build a set of fault thresholds; the session bound stays off
    /// (see [`Self::with_pnl_bound`]).
    #[inline]
    #[must_use]
    pub const fn new(
        reject_streak: u32,
        recon_drift_usd_1e6: i64,
        ws_gap_ms: i64,
        asset_refusal_streak: u32,
        recon_stale_ms: i64,
    ) -> Self {
        Self {
            recon_drift_usd_1e6,
            ws_gap_ms,
            recon_stale_ms,
            reject_streak,
            asset_refusal_streak,
            pnl_gain_usd_1e6: 0,
            pnl_loss_usd_1e6: 0,
            alo_reject_permille: 0,
            _pad: [0; 6],
        }
    }

    /// E7: the operator's session bound — halt at `gain` over or
    /// `loss` under the spot-USDC anchor, each `0` = that side off.
    #[inline]
    #[must_use]
    pub const fn with_pnl_bound(mut self, gain_usd_1e6: i64, loss_usd_1e6: i64) -> Self {
        self.pnl_gain_usd_1e6 = gain_usd_1e6;
        self.pnl_loss_usd_1e6 = loss_usd_1e6;
        self
    }

    /// XH4d, XH-7: halt at `permille` post-only crosses among the arm's
    /// last ALO answers; `0` = off.
    #[inline]
    #[must_use]
    pub const fn with_alo_reject_permille(mut self, permille: u16) -> Self {
        self.alo_reject_permille = permille;
        self
    }
}

/// **XH4e — a perp slot's two extra clamps** (plan §8.8), USD ×1e6,
/// `0` = unset.
///
/// A slot that sets both is a **perp slot** ([`ExecRoute::set_perp_limits`]
/// refuses one without the other): every order must be on a bound linear
/// instrument row (never the HIP-4 family clamps), and while any linear
/// row holds a position of the slot that no mark prices, only exits pass
/// (XH2-C). A slot that sets neither — every slot but a maker on perps —
/// is judged exactly as before. 16 B; the table's sixteen are its last
/// four lines.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PerpLimits {
    /// Per-perp inventory: the position plus every order working on the
    /// order's side plus the order, at the higher of the limit and the
    /// mark (`Ledger::probe_perp`).
    pub cap_perp_usd_1e6: i64,
    /// The slot's resting notional: Σ remaining × limit over its working
    /// orders (`Ledger::resting_usd_1e6`).
    pub max_resting_usd_1e6: i64,
}

const _: () = assert!(core::mem::size_of::<PerpLimits>() == 16);

impl PerpLimits {
    /// Both clamps unset: not a perp slot. Usable in `const`.
    #[inline]
    #[must_use]
    pub const fn none() -> Self {
        Self {
            cap_perp_usd_1e6: 0,
            max_resting_usd_1e6: 0,
        }
    }

    /// A perp slot's two clamps.
    #[inline]
    #[must_use]
    pub const fn new(cap_perp_usd_1e6: i64, max_resting_usd_1e6: i64) -> Self {
        Self {
            cap_perp_usd_1e6,
            max_resting_usd_1e6,
        }
    }

    /// Whether the slot is a perp slot: either clamp set.
    #[inline]
    #[must_use]
    pub const fn is_set(&self) -> bool {
        self.cap_perp_usd_1e6 > 0 || self.max_resting_usd_1e6 > 0
    }
}

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
    venue_mask: [u16; EXEC_SLOTS],
    /// Per-slot single-order notional clamp, USD x1e6. Carried by E1
    /// for the boot tell and `/state`; **enforced in E6** (the risk
    /// gate), which is the phase that owns clamping.
    max_order_usd_1e6: [i64; EXEC_SLOTS],
    /// Per-slot open-order clamp. Same status as above — **enforced
    /// in E6** against the router's own resting count.
    max_open_orders: [u32; EXEC_SLOTS],
    /// **E6** — per-slot per-instance clamp, USD x1e6, net exposure.
    cap_instance_usd_1e6: [i64; EXEC_SLOTS],
    /// **E6** — per-slot day clamp, USD x1e6, filled buy turnover.
    cap_day_usd_1e6: [i64; EXEC_SLOTS],
    /// Pad the hot arrays + clamp fields — 31 B a slot, 496 B at
    /// sixteen slots (S16-3; 248 B at eight, 240 B before HYPARB widened
    /// `venue_mask` to `u16`) — to a whole number of lines (512 B, eight
    /// of them), so the halt table that follows starts on a line
    /// boundary — which this field only achieves by sitting BEFORE
    /// `halts`; an earlier layout declared it after and the comment was
    /// describing tail padding. E1 reserved 16 B here "for E6's clamps";
    /// E6 needed 128 B (at eight slots), so the struct grew. What the
    /// reservation actually bought is what it was for: the hot arrays
    /// did not move. `modes` and `venue_mask` are at offsets 0 and 16
    /// (0 and 8 at eight slots), both inside line 0, so the per-submit
    /// lookup still costs ONE line, and the E6 clamps — read only on the
    /// live arm, after the mode branch has already resolved — sit after
    /// them where a paper boot never reads them.
    _pad: [u8; 16],
    /// **E6 commit 3** — per-slot halt thresholds. 56 B each (32 B
    /// before E7's session bound, 48 B before XH4d's ALO threshold),
    /// sixteen of them: 896 B at offset 512, lines 8–21.
    halts: [HaltLimits; EXEC_SLOTS],
    /// **XH4e** — per-slot perp limits, 16 B each: 256 B at offset
    /// 1 408, lines 22–25.
    perp: [PerpLimits; EXEC_SLOTS],
}

// S16-3 re-pin (832 → 1 664 B): every per-slot array doubled. The pins
// carry the one-line claim: `modes` (16 B) at 0 and `venue_mask` (32 B)
// at 16 end at 48, inside line 0, so a submit's mode + venue lookup is
// one line; the halt table starts on a line (that is `_pad`'s job).
const _: () = assert!(core::mem::size_of::<ExecRoute>() == 1664);
const _: () = assert!(core::mem::offset_of!(ExecRoute, modes) == 0);
const _: () = assert!(core::mem::offset_of!(ExecRoute, venue_mask) == 16);
const _: () = assert!(core::mem::offset_of!(ExecRoute, venue_mask) + 2 * EXEC_SLOTS <= 64);
const _: () = assert!(core::mem::offset_of!(ExecRoute, halts) == 512);
const _: () = assert!(core::mem::offset_of!(ExecRoute, perp) == 1408);

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
            venue_mask: [0u16; EXEC_SLOTS],
            max_order_usd_1e6: [0i64; EXEC_SLOTS],
            max_open_orders: [0u32; EXEC_SLOTS],
            cap_instance_usd_1e6: [0i64; EXEC_SLOTS],
            cap_day_usd_1e6: [0i64; EXEC_SLOTS],
            halts: [HaltLimits::none(); EXEC_SLOTS],
            _pad: [0u8; 16],
            perp: [PerpLimits::none(); EXEC_SLOTS],
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
        caps: SlotCaps,
        halts: HaltLimits,
    ) -> Result<(), ExecRouteErr> {
        if slot >= EXEC_SLOTS {
            return Err(ExecRouteErr::SlotOutOfRange(slot));
        }
        let mut mask = 0u16;
        let mut i = 0usize;
        while i < venues.len() {
            let v = venues[i];
            if v >= EXEC_VENUES {
                return Err(ExecRouteErr::VenueOutOfRange(v));
            }
            mask |= 1u16 << v;
            i += 1;
        }
        self.modes[slot] = mode.as_u8();
        self.venue_mask[slot] = mask;
        self.max_order_usd_1e6[slot] = caps.max_order_usd_1e6;
        self.max_open_orders[slot] = caps.max_open_orders;
        self.cap_instance_usd_1e6[slot] = caps.cap_instance_usd_1e6;
        self.cap_day_usd_1e6[slot] = caps.cap_day_usd_1e6;
        self.halts[slot] = halts;
        Ok(())
    }

    /// The slot's mode. **Hot path: one masked load, no branch on the
    /// index, no bounds check.**
    ///
    /// Out-of-range ids (every id >= [`EXEC_SLOTS`], which includes
    /// `core_types::STRATEGY_ID_NONE` = 0xFF and `split::NO_SLOT` =
    /// 0xFE — masked, slots 15 and 14) resolve to [`ExecMode::Paper`]
    /// by arithmetic rather than by branch: the in-range flag
    /// multiplies the loaded byte to zero.
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
        // Masked shift: `venue & 15` is always a legal shift distance
        // for u16, so no UB and no debug panic on a wild venue byte.
        let bit = ((mask >> (venue & (EXEC_VENUES - 1))) & 1) as u8;
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
    ///
    /// A [`SlotMask`] (S16-3): `i < EXEC_SLOTS <= SlotMask::BITS`, so
    /// the shift is exact — the `u8` this was at eight slots would, in a
    /// release build (`overflow-checks = false`), have put a live slot
    /// 11 on bit 3, BIN15's, and the interlock would have agreed.
    #[inline]
    #[must_use]
    pub fn live_mask(&self) -> SlotMask {
        self.mode_mask(ExecMode::Live)
    }

    /// Bit `i` set = slot `i` is [`ExecMode::Off`]. Cold; boot tell.
    #[inline]
    #[must_use]
    pub fn off_mask(&self) -> SlotMask {
        self.mode_mask(ExecMode::Off)
    }

    /// Bit `i` set = slot `i` is in `mode`. The one shift both masks use.
    #[inline]
    fn mode_mask(&self, mode: ExecMode) -> SlotMask {
        let mut m: SlotMask = 0;
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            m |= SlotMask::from(self.modes[i] == mode as u8) << i;
            i += 1;
        }
        m
    }

    /// Is any LIVE slot routed to `venue`? Cold; the boot decides
    /// from this whether to construct a real arm for that venue.
    #[inline]
    #[must_use]
    pub fn venue_live(&self, venue: u8) -> bool {
        if venue >= EXEC_VENUES {
            return false;
        }
        let mut i = 0usize;
        while i < EXEC_SLOTS {
            if self.modes[i] == ExecMode::Live as u8 && self.venue_mask[i] & (1u16 << venue) != 0 {
                return true;
            }
            i += 1;
        }
        false
    }

    /// The slot's venue bitmask. Cold; boot tell + `/state`.
    #[inline]
    #[must_use]
    pub fn venue_mask_at(&self, slot: usize) -> Option<u16> {
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

    /// **E6** — the slot's per-instance NET EXPOSURE clamp (USD x1e6).
    /// Cold: read once per live submit, on the refusal path's side of
    /// the mode branch.
    #[inline]
    #[must_use]
    pub fn cap_instance_usd_1e6_at(&self, slot: usize) -> Option<i64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.cap_instance_usd_1e6[slot])
    }

    /// **E6** — the slot's day TURNOVER clamp (USD x1e6). Cold.
    #[inline]
    #[must_use]
    pub fn cap_day_usd_1e6_at(&self, slot: usize) -> Option<i64> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.cap_day_usd_1e6[slot])
    }

    /// All four of a slot's clamps at once. Cold; the boot tell and
    /// `/state` want them together.
    #[inline]
    #[must_use]
    pub fn caps_at(&self, slot: usize) -> Option<SlotCaps> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(SlotCaps {
            max_order_usd_1e6: self.max_order_usd_1e6[slot],
            cap_instance_usd_1e6: self.cap_instance_usd_1e6[slot],
            cap_day_usd_1e6: self.cap_day_usd_1e6[slot],
            max_open_orders: self.max_open_orders[slot],
            _pad: 0,
        })
    }

    /// **E6 commit 3** — the slot's halt thresholds. Cold.
    #[inline]
    #[must_use]
    pub fn halts_at(&self, slot: usize) -> Option<HaltLimits> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.halts[slot])
    }

    /// **XH4e** — write one slot's perp limits. Boot-only, cold,
    /// bounds-checked, like [`Self::set_slot`]. Both clamps or neither: a
    /// perp slot with one clamp silently off is refused.
    ///
    /// # Errors
    /// [`ExecRouteErr::SlotOutOfRange`], [`ExecRouteErr::PerpLimitsPartial`].
    pub fn set_perp_limits(&mut self, slot: usize, lim: PerpLimits) -> Result<(), ExecRouteErr> {
        if slot >= EXEC_SLOTS {
            return Err(ExecRouteErr::SlotOutOfRange(slot));
        }
        let none = lim.cap_perp_usd_1e6 == 0 && lim.max_resting_usd_1e6 == 0;
        let both = lim.cap_perp_usd_1e6 > 0 && lim.max_resting_usd_1e6 > 0;
        if !none && !both {
            return Err(ExecRouteErr::PerpLimitsPartial(slot));
        }
        self.perp[slot] = lim;
        Ok(())
    }

    /// **XH4e** — the slot's perp limits. Read on a live order.
    #[inline]
    #[must_use]
    pub fn perp_at(&self, slot: usize) -> Option<PerpLimits> {
        if slot >= EXEC_SLOTS {
            return None;
        }
        Some(self.perp[slot])
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

    /// XH4e: a perp slot is one that sets either clamp; the default table
    /// has none, and an out-of-range slot is refused, never dropped.
    #[test]
    fn perp_limits_are_per_slot_and_unset_by_default() {
        let mut r = ExecRoute::all_paper();
        let mut s = 0usize;
        while s < EXEC_SLOTS {
            assert!(!r.perp_at(s).unwrap().is_set());
            s += 1;
        }
        r.set_perp_limits(6, PerpLimits::new(150_000_000, 300_000_000))
            .unwrap();
        assert!(r.perp_at(6).unwrap().is_set());
        assert!(!r.perp_at(5).unwrap().is_set());
        assert!(PerpLimits::new(0, 1).is_set() && PerpLimits::new(1, 0).is_set());
        assert!(
            !PerpLimits::new(-5, 0).is_set(),
            "a negative number is no clamp"
        );
        assert_eq!(
            r.set_perp_limits(EXEC_SLOTS, PerpLimits::none()),
            Err(ExecRouteErr::SlotOutOfRange(EXEC_SLOTS))
        );
        assert_eq!(r.perp_at(EXEC_SLOTS), None);
        // Both clamps or neither.
        assert_eq!(
            r.set_perp_limits(5, PerpLimits::new(150_000_000, 0)),
            Err(ExecRouteErr::PerpLimitsPartial(5))
        );
        assert_eq!(
            r.set_perp_limits(5, PerpLimits::new(-1, 300_000_000)),
            Err(ExecRouteErr::PerpLimitsPartial(5))
        );
        assert!(!r.perp_at(5).unwrap().is_set(), "a refusal writes nothing");
        assert_eq!(r.set_perp_limits(6, PerpLimits::none()), Ok(()));
        assert!(!r.perp_at(6).unwrap().is_set());
    }

    #[test]
    fn layout_is_twenty_six_cache_lines_with_the_hot_arrays_first() {
        assert_eq!(
            core::mem::size_of::<ExecRoute>(),
            1664,
            "twenty-six cache lines (S16-3: sixteen slots)"
        );
        assert_eq!(
            core::mem::size_of::<PerpLimits>(),
            16,
            "PerpLimits is 16 B (XH4e)"
        );
        assert_eq!(
            core::mem::size_of::<HaltLimits>(),
            56,
            "HaltLimits is 56 B (XH4d)"
        );
        assert_eq!(core::mem::size_of::<SlotCaps>(), 32, "SlotCaps is 32 B");
        assert_eq!(core::mem::align_of::<ExecRoute>(), 64);
        assert_eq!(
            core::mem::offset_of!(ExecRoute, halts),
            512,
            "the halt table starts on a line boundary — that is what `_pad` is for"
        );
        assert_eq!(core::mem::offset_of!(ExecRoute, perp), 1408);
        // The hot arrays must sit inside the FIRST cache line, or the
        // per-submit lookup costs two lines instead of one.
        let r = ExecRoute::all_paper();
        let base = core::ptr::addr_of!(r) as usize;
        let modes = core::ptr::addr_of!(r.modes) as usize - base;
        let venues = core::ptr::addr_of!(r.venue_mask) as usize - base;
        assert!(modes + EXEC_SLOTS <= 64, "modes at {modes} spills line 0");
        assert!(
            venues + 2 * EXEC_SLOTS <= 64,
            "venue_mask at {venues} spills line 0"
        );
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
    fn strategy_id_none_fails_closed_to_paper_even_when_slot_15_is_live() {
        // 0xFF & 15 == 15 (S16-3; `& 7` == 7 at eight slots). If the
        // range check were missing, an un-stamped order would inherit
        // slot 15's mode — this is the exact aliasing the `in_range`
        // multiply exists to kill.
        let mut r = ExecRoute::all_paper();
        r.set_slot(15, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        assert_eq!(r.mode(15), ExecMode::Live);
        assert!(r.venue_allowed(15, 4));
        assert_eq!(r.mode(STRATEGY_ID_NONE), ExecMode::Paper);
        assert!(!r.venue_allowed(STRATEGY_ID_NONE, 4));
    }

    #[test]
    fn no_slot_0xfe_fails_closed_to_paper_even_when_slot_14_is_live() {
        // `split::NO_SLOT` = 0xFE masks onto slot 14.
        let mut r = ExecRoute::all_paper();
        r.set_slot(14, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        assert_eq!(r.mode(14), ExecMode::Live);
        assert_eq!(r.mode(0xFE), ExecMode::Paper);
        assert!(!r.venue_allowed(0xFE, 4));
        assert_eq!(r.mode(16), ExecMode::Paper, "the first id past the table");
        assert_eq!(r.mode(16 + 3), ExecMode::Paper, "19 masks onto BIN15's slot 3");
    }

    /// S16-3: the masks are `SlotMask`s. A live slot 11 is bit 11 and
    /// never bit 3 (BIN15's), an off slot 15 is bit 15 and never bit 7 —
    /// the `u8` masks of eight slots would have aliased both in a
    /// release build and panicked here in a debug one.
    #[test]
    fn slots_11_and_15_are_their_own_bits_in_the_live_and_off_masks() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(11, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        r.set_slot(15, ExecMode::Off, &[], SlotCaps::none(), HaltLimits::none()).unwrap();
        assert_eq!(r.live_mask(), 1 << 11);
        assert_eq!(r.off_mask(), 1 << 15);
        assert_eq!(r.mode(11), ExecMode::Live);
        assert_eq!(r.mode(3), ExecMode::Paper, "a live slot 11 does not make slot 3 live");
        assert_eq!(r.mode(15), ExecMode::Off);
        assert_eq!(r.mode(7), ExecMode::Paper);
        assert!(r.venue_allowed(11, 4));
        assert!(!r.venue_allowed(3, 4), "slot 3 has no venue");
        assert!(r.venue_live(4));
        assert!(r.any_live());
        // Both directions: a live slot 3 is bit 3 alone.
        let mut b = ExecRoute::all_paper();
        b.set_slot(3, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        assert_eq!(b.live_mask(), 1 << 3);
        assert_eq!(b.mode(11), ExecMode::Paper);
        // Every slot live: the mask is every bit, no more.
        let mut all = ExecRoute::all_paper();
        for s in 0..EXEC_SLOTS {
            all.set_slot(s, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        }
        assert_eq!(all.live_mask(), SlotMask::MAX);
        assert_eq!(all.off_mask(), 0);
    }

    #[test]
    fn slot_11_and_15_caps_round_trip_and_slot_16_is_refused() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            11,
            ExecMode::Live,
            &[4, 6],
            SlotCaps::new(1, 2, 3, 64),
            HaltLimits::new(5, 6, 7, 8, 9),
        )
        .unwrap();
        r.set_perp_limits(15, PerpLimits::new(10, 20)).unwrap();
        assert_eq!(r.caps_at(11), Some(SlotCaps::new(1, 2, 3, 64)));
        assert_eq!(r.halts_at(11), Some(HaltLimits::new(5, 6, 7, 8, 9)));
        assert_eq!(r.venue_mask_at(11), Some(0b0101_0000));
        assert_eq!(r.caps_at(3), Some(SlotCaps::none()), "slot 3 untouched");
        assert_eq!(r.perp_at(15), Some(PerpLimits::new(10, 20)));
        assert!(!r.perp_at(7).unwrap().is_set(), "slot 7 untouched");
        assert_eq!(
            r.set_slot(16, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()),
            Err(ExecRouteErr::SlotOutOfRange(16))
        );
    }

    #[test]
    fn every_out_of_range_id_is_paper_whatever_the_table_says() {
        let mut r = ExecRoute::all_paper();
        for s in 0..EXEC_SLOTS {
            r.set_slot(
                s,
                ExecMode::Live,
                &[0, 1, 2, 3, 4, 5, 6],
                SlotCaps::new(i64::MAX, i64::MAX, i64::MAX, u32::MAX),
                HaltLimits::none(),
            )
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
        r.set_slot(
            slot,
            ExecMode::Live,
            &[4],
            SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
            HaltLimits::none(),
        )
        .unwrap();

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
        r.set_slot(3, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()).unwrap();
        // Bit 4 is set; without the `venue_ok` term, venue 20
        // (20 & 15 == 4) would alias onto it.
        for v in EXEC_VENUES..=255 {
            assert!(!r.venue_allowed(3, v), "venue byte {v} must fail closed");
        }
    }

    /// HYPARB H9 pin: venue 8 (`HyperEvm`, O-H11) is its OWN bit. Under
    /// the old `u8` mask `mask >> (8 & 7)` read bit 0 — Polymarket's —
    /// so a slot live on Polymarket would have "allowed" HyperEVM and a
    /// slot live on HyperEVM would have allowed Polymarket.
    #[test]
    fn venue_8_is_its_own_bit_and_never_aliases_onto_venue_0() {
        let hyperevm = core_types::VenueId::HyperEvm as u8;
        assert_eq!(hyperevm, 8);
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            3,
            ExecMode::Live,
            &[0],
            SlotCaps::none(),
            HaltLimits::none(),
        )
        .unwrap();
        assert!(r.venue_allowed(3, 0));
        assert!(
            !r.venue_allowed(3, hyperevm),
            "venue 0 live does not allow venue 8"
        );
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            3,
            ExecMode::Live,
            &[hyperevm],
            SlotCaps::none(),
            HaltLimits::none(),
        )
        .unwrap();
        assert!(r.venue_allowed(3, hyperevm));
        assert!(
            !r.venue_allowed(3, 0),
            "venue 8 live does not allow venue 0"
        );
        assert_eq!(r.venue_mask_at(3), Some(1 << 8));
    }

    #[test]
    fn off_is_distinct_from_paper() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(6, ExecMode::Off, &[], SlotCaps::none(), HaltLimits::none()).unwrap();
        assert_eq!(r.mode(6), ExecMode::Off);
        assert_eq!(r.off_mask(), 0b0100_0000);
        assert_eq!(r.live_mask(), 0);
        assert!(!r.any_live());
    }

    #[test]
    fn out_of_range_writes_are_refused_not_wrapped() {
        let mut r = ExecRoute::all_paper();
        assert_eq!(
            r.set_slot(EXEC_SLOTS, ExecMode::Live, &[4], SlotCaps::none(), HaltLimits::none()),
            Err(ExecRouteErr::SlotOutOfRange(EXEC_SLOTS))
        );
        assert_eq!(
            r.set_slot(3, ExecMode::Live, &[EXEC_VENUES], SlotCaps::none(), HaltLimits::none()),
            Err(ExecRouteErr::VenueOutOfRange(EXEC_VENUES))
        );
        // A refused write leaves the table untouched.
        assert_eq!(r, ExecRoute::all_paper());
    }

    #[test]
    fn caps_and_masks_round_trip_through_the_cold_accessors() {
        let mut r = ExecRoute::all_paper();
        r.set_slot(
            3,
            ExecMode::Live,
            &[4, 6],
            SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
            HaltLimits::none(),
        )
        .unwrap();
        assert_eq!(r.mode_at(3), Some(ExecMode::Live));
        assert_eq!(r.venue_mask_at(3), Some(0b0101_0000));
        assert_eq!(r.max_order_usd_1e6_at(3), Some(100_000_000));
        assert_eq!(r.max_open_orders_at(3), Some(64));
        // E6's two, which E1 parsed and carried nowhere.
        assert_eq!(r.cap_instance_usd_1e6_at(3), Some(1_000_000_000));
        assert_eq!(r.cap_day_usd_1e6_at(3), Some(30_000_000_000));
        // And all four together, in the order `SlotCaps` names them —
        // the point of the struct is that this line cannot silently
        // transpose the two `i64` caps.
        assert_eq!(
            r.caps_at(3),
            Some(SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64))
        );
        assert_eq!(r.cap_instance_usd_1e6_at(EXEC_SLOTS), None);
        assert_eq!(r.cap_day_usd_1e6_at(EXEC_SLOTS), None);
        assert_eq!(r.caps_at(EXEC_SLOTS), None);
        assert_eq!(r.mode_at(EXEC_SLOTS), None);
        assert_eq!(r.venue_mask_at(EXEC_SLOTS), None);
        assert_eq!(r.max_order_usd_1e6_at(EXEC_SLOTS), None);
        assert_eq!(r.max_open_orders_at(EXEC_SLOTS), None);
    }
}
