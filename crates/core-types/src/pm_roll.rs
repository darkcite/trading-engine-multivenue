// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PM15: Polymarket's ROLLING 15-minute crypto up/down families.
//!
//! Polymarket lists one binary market per asset per 15-minute window,
//! `<asset>-updown-15m-<open epoch>`, with two outcome tokens (Up, Down)
//! settled on a Chainlink 60-second TWAP ending at the window end. A new
//! token pair every 15 minutes cannot each get a boot-time `SymbolId` (the
//! static `[polymarket] markets` law), so — exactly as HIP-4's families on
//! Hyperliquid (`ingress_hyperliquid::family`) — a family owns fixed SLOT
//! syms whose venue tokens are rebound at every roll, and the roll is a
//! [`crate::ChannelId::InstrumentRoll`] event.
//!
//! ## The slot pool
//!
//! Family `f` (0-based, in the boot `[polymarket] rolling` order) owns
//! [`PM_ROLLING_STRIDE`] consecutive ordinals of venue byte 0 from
//! [`PM_ROLLING_ORDINAL_BASE`]:
//!
//! | offset | slot | means |
//! |---|---|---|
//! | 0, 1 | `[up0]`, `[down0]` | the tokens of the family's EVEN window (pair 0) |
//! | 2, 3 | `[up1]`, `[down1]` | the tokens of its ODD window (pair 1) |
//! | 4 | `[mark]` | the asset's Chainlink reference price |
//!
//! **Two pairs, by window parity**, because a Polymarket window does not
//! end the way a HIP-4 instance does: the market keeps trading after its
//! window ends and resolves about two minutes later (`market_resolved`,
//! measured 2026-10-17: +116 s), while the next window has already opened.
//! One pair would rebind a slot that still holds an unsettled position.
//! With two, window `w` lives on pair `w & 1` and the pair it vacates is
//! not needed again until `w + 2` opens, 15 minutes after `w` ended.
//!
//! The pool sits far above every flat static-token ordinal (`flat + 1 ≤
//! 2·PM_MARKETS_MAX`) and above the legacy anchors 7 and 42, so a slot
//! can never alias a static market. That matters because Polymarket is
//! venue byte 0: a LOW Polymarket ordinal reads as a legacy anchor id.
//!
//! ## The instance id and the roll family
//!
//! A window index ([`pm_window_of_open`]) is the same number for every
//! asset — BTC's and ETH's 06:00Z windows are both `1 991 352` — so it
//! cannot be a roll's outcome id: a consumer that settles by outcome
//! (`exec_router::Ledger::settle`, `strategy_bin15::on_roll`) would
//! settle the wrong asset. The instance id is [`pm_instance_id`]`(window,
//! family) = window · PM_ROLLING_MAX + family`, unique per market. The
//! roll's FAMILY byte is the slot pair, [`pm_roll_family`]`(family,
//! pair) = 2·family + pair` (< 16), so a ledger keyed by family keeps
//! the two pairs of one family on two rows.
//!
//! ## The events (venue byte Polymarket)
//!
//! **Roll** — [`crate::ChannelId::InstrumentRoll`]:
//!
//! | field | value |
//! |---|---|
//! | `sym` | the pair's Up slot ([`pm_rolling_sym`]`(f, pair, PM_LEG_UP)`; Down is the next ordinal) |
//! | `venue_seq` | [`crate::pack_roll_seq`]`(pm_instance_id(window, f), PM_TWAP_S, pm_roll_family(f, pair), settled)` |
//! | `v1` | the window END, Unix ns ([`pm_window_end_ns`]) |
//! | `v0` | created: 0 — the strike (`priceToBeat`) is not published before resolution, so the consumer computes it from the `[mark]` stream; settled: [`PM_SETTLED_UP`] / [`PM_SETTLED_DOWN`] / [`PM_SETTLED_UNKNOWN`] |
//! | `venue_time_ms` | created: 0; settled: the `market_resolved` stamp (0 when the ingress retired an unresolved window, [`PM_SETTLED_UNKNOWN`]) |
//!
//! **Order law** — [`crate::ChannelId::OutcomeMeta`], one per leg, at the
//! window's creation and at every `tick_size_change`:
//!
//! | field | value |
//! |---|---|
//! | `sym` | the leg's slot |
//! | `venue_seq` | the instance id |
//! | `v0` | the price tick ×1e6 (`orderPriceMinTickSize`; 10 000 or 1 000) |
//! | `v1` | the minimum order size, shares ×1e6 (`orderMinSize`) |
//! | `venue_time_ms` | 0 at creation; the venue stamp of a `tick_size_change` |
//!
//! **Reference price** — [`crate::ChannelId::Mark`] on the `[mark]` slot,
//! one per Chainlink report (1 Hz): `venue_time_ms` = the report's second
//! (ms), `venue_seq` = that second, `v0` = the price ×1e6 and `v1` = ×1e12
//! (both rounded half up from the 18-decimal `full_accuracy_value`).
//!
//! ## PMJ J1b: the 5-minute families (ruling R-PMJ-S1, 2026-10-18)
//!
//! Polymarket also lists `<asset>-updown-5m-<open epoch>`: the same law
//! (the Chainlink 60-s TWAP, the same fee schedule, a 5-share minimum)
//! on a 300 s window. Two pairs do not fit it: the pair window `w`
//! vacates is needed again by `w + 2`, which opens only 300 s after `w`
//! ends, while resolution takes +116…218 s (measured on 15 m) and an
//! unresolved window must wait for its retirement. So the 5-minute
//! families have a pool of their OWN — the 15-minute pool, its syms, its
//! instance ids and its roll families are untouched, bit for bit:
//!
//! | item | 15 m | 5 m |
//! |---|---|---|
//! | ordinal base | [`PM_ROLLING_ORDINAL_BASE`] 4096 | [`PM5_ROLLING_ORDINAL_BASE`] 4160 |
//! | stride | 5: `[up0][down0][up1][down1][mark]` | 7: `[up0][down0][up1][down1][up2][down2][mark]` |
//! | pairs | 2, window parity | 3, window mod 3 — a pair is needed again 600 s after its window ends |
//! | families | [`PM_ROLLING_MAX`] 8 | [`PM5_ROLLING_MAX`] 8 |
//! | roll family byte | `2·f + pair` (0..16) | `16 + 3·g + pair` (16..40) |
//! | instance id | `window · 8 + f` | `PM5_INSTANCE_FLAG \| (window · 8 + g)` |
//!
//! The 5-minute instance id carries bit 31 ([`PM5_INSTANCE_FLAG`]): a
//! 15-minute window index stays below 2²⁸ for some 7 000 years, so the two
//! families' ids can never be equal, by construction rather than by the
//! calendar. The events are those of the 15-minute families, on the 5 m
//! pool's syms; `v1` of a roll is the 5 m window's end.
//!
//! The ingress is the only writer. Pure `const fn`s; no allocation.

use crate::{make_symbol_id, symbol_ordinal, symbol_venue_byte, SymbolId, VenueId};

/// Cap on `[polymarket] rolling` families (the seven 15-minute crypto
/// assets fit with one to spare).
pub const PM_ROLLING_MAX: usize = 8;

/// Ordinal base of the rolling slot pool (venue byte 0).
pub const PM_ROLLING_ORDINAL_BASE: u32 = 4096;

/// Ordinals per family: two token pairs and the mark.
pub const PM_ROLLING_STRIDE: u32 = 5;

/// Token pairs per family (window parity).
pub const PM_PAIRS: usize = 2;

/// Leg of the Up outcome token within a pair.
pub const PM_LEG_UP: u32 = 0;
/// Leg of the Down outcome token within a pair.
pub const PM_LEG_DOWN: u32 = 1;

/// Offset of the mark slot within a family.
pub const PM_OFF_MARK: u32 = 4;

/// The manifest tag of each family offset, in offset order.
pub const PM_SLOT_TAGS: [&str; PM_ROLLING_STRIDE as usize] =
    ["up0", "down0", "up1", "down1", "mark"];

/// Window length in seconds (the 15-minute family).
pub const PM_WINDOW_S: u64 = 900;

/// Settlement TWAP length in seconds (`cryptoMarketConfig.twapLookbackSeconds`).
pub const PM_TWAP_S: u32 = 60;

/// Settled roll `v0`: the Up token won.
pub const PM_SETTLED_UP: i64 = 1;
/// Settled roll `v0`: the Down token won.
pub const PM_SETTLED_DOWN: i64 = -1;
/// Settled roll `v0`: no winner known — the venue named a token that is
/// neither leg, or the ingress retired a window the venue never resolved.
pub const PM_SETTLED_UNKNOWN: i64 = 0;

const NS_PER_S: u64 = 1_000_000_000;

const _: () = assert!(PM_OFF_MARK == 2 * PM_PAIRS as u32);
const _: () = assert!(PM_OFF_MARK + 1 == PM_ROLLING_STRIDE);
// The whole pool fits the 24-bit ordinal and stays clear of the flat
// static-token ordinals (≤ 2 × 64 + 1) and the legacy anchors (7, 42).
const _: () = assert!(
    PM_ROLLING_ORDINAL_BASE + PM_ROLLING_MAX as u32 * PM_ROLLING_STRIDE
        <= crate::SYMBOL_ORDINAL_MASK
);
const _: () = assert!(PM_ROLLING_ORDINAL_BASE > 2 * 64 + 1);
// Every roll family fits the 8-bit family field of `pack_roll_seq`.
const _: () = assert!(PM_ROLLING_MAX * PM_PAIRS <= 256);

/// The sym at `offset` (0..[`PM_ROLLING_STRIDE`]) of family `family`.
#[inline(always)]
#[must_use]
pub const fn pm_slot_sym(family: usize, offset: u32) -> SymbolId {
    debug_assert!(family < PM_ROLLING_MAX && offset < PM_ROLLING_STRIDE);
    make_symbol_id(
        VenueId::Polymarket,
        PM_ROLLING_ORDINAL_BASE + family as u32 * PM_ROLLING_STRIDE + offset,
    )
}

/// The token slot of `leg` in `pair` of family `family`.
#[inline(always)]
#[must_use]
pub const fn pm_rolling_sym(family: usize, pair: usize, leg: u32) -> SymbolId {
    debug_assert!(pair < PM_PAIRS && leg <= PM_LEG_DOWN);
    pm_slot_sym(family, pair as u32 * 2 + leg)
}

/// The mark slot of family `family`.
#[inline(always)]
#[must_use]
pub const fn pm_mark_sym(family: usize) -> SymbolId {
    pm_slot_sym(family, PM_OFF_MARK)
}

/// `(family, offset)` of a rolling slot sym; `None` for every other sym.
#[inline(always)]
#[must_use]
pub const fn pm_rolling_of(sym: SymbolId) -> Option<(usize, u32)> {
    if symbol_venue_byte(sym) != VenueId::Polymarket as u8 {
        return None;
    }
    let o = symbol_ordinal(sym);
    if o < PM_ROLLING_ORDINAL_BASE {
        return None;
    }
    let rel = o - PM_ROLLING_ORDINAL_BASE;
    let family = (rel / PM_ROLLING_STRIDE) as usize;
    if family >= PM_ROLLING_MAX {
        return None;
    }
    Some((family, rel % PM_ROLLING_STRIDE))
}

/// The pair window `window` lives on: its parity.
#[inline(always)]
#[must_use]
pub const fn pm_pair_of_window(window: u32) -> usize {
    (window & 1) as usize
}

/// The roll's family byte for `pair` of family `family` (module docs).
#[inline(always)]
#[must_use]
pub const fn pm_roll_family(family: usize, pair: usize) -> usize {
    family * PM_PAIRS + pair
}

/// The instance id of family `family`'s market for window `window`;
/// `None` past `u32` (window ≥ 2²⁹, the year 17 000).
#[inline(always)]
#[must_use]
pub const fn pm_instance_id(window: u32, family: usize) -> Option<u32> {
    if family >= PM_ROLLING_MAX || window > u32::MAX / PM_ROLLING_MAX as u32 {
        return None;
    }
    Some(window * PM_ROLLING_MAX as u32 + family as u32)
}

/// `(window, family)` of an instance id — the inverse of [`pm_instance_id`].
#[inline(always)]
#[must_use]
pub const fn pm_instance_parts(id: u32) -> (u32, usize) {
    (
        id / PM_ROLLING_MAX as u32,
        (id % PM_ROLLING_MAX as u32) as usize,
    )
}

/// The window index of the window that OPENS at `open_s` (Unix s);
/// `None` off the 900 s grid or past `u32`.
#[inline(always)]
#[must_use]
pub const fn pm_window_of_open(open_s: u64) -> Option<u32> {
    if open_s % PM_WINDOW_S != 0 {
        return None;
    }
    let w = open_s / PM_WINDOW_S;
    if w > u32::MAX as u64 {
        return None;
    }
    Some(w as u32)
}

/// The window that CONTAINS Unix second `t_s` (open ≤ t < end).
#[inline(always)]
#[must_use]
pub const fn pm_window_at(t_s: u64) -> u32 {
    (t_s / PM_WINDOW_S) as u32
}

/// The open of window `window`, Unix s — also the slug's epoch.
#[inline(always)]
#[must_use]
pub const fn pm_window_open_s(window: u32) -> u64 {
    window as u64 * PM_WINDOW_S
}

/// The open of window `window`, Unix ns.
#[inline(always)]
#[must_use]
pub const fn pm_window_open_ns(window: u32) -> u64 {
    window as u64 * PM_WINDOW_S * NS_PER_S
}

/// The end of window `window`, Unix ns — a roll's `v1`.
#[inline(always)]
#[must_use]
pub const fn pm_window_end_ns(window: u32) -> u64 {
    (window as u64 + 1) * PM_WINDOW_S * NS_PER_S
}

// ---------------------------------------------------------------
// PMJ J1b: the 5-minute families (module docs; ruling R-PMJ-S1)
// ---------------------------------------------------------------

/// Cap on 5-minute `[polymarket] rolling` families (the seven crypto
/// assets fit with one to spare).
pub const PM5_ROLLING_MAX: usize = 8;
/// Ordinal base of the 5-minute slot pool (venue byte 0), above the
/// whole 15-minute pool.
pub const PM5_ROLLING_ORDINAL_BASE: u32 = 4160;
/// Token pairs per 5-minute family (window mod 3).
pub const PM5_PAIRS: usize = 3;
/// Offset of the mark slot within a 5-minute family.
pub const PM5_OFF_MARK: u32 = 6;
/// Ordinals per 5-minute family: three token pairs and the mark.
pub const PM5_ROLLING_STRIDE: u32 = 7;
/// The manifest tag of each 5-minute family offset, in offset order.
pub const PM5_SLOT_TAGS: [&str; PM5_ROLLING_STRIDE as usize] =
    ["up0", "down0", "up1", "down1", "up2", "down2", "mark"];
/// Window length in seconds (the 5-minute family).
pub const PM5_WINDOW_S: u64 = 300;
/// The first 5-minute roll family byte: past every 15-minute one.
pub const PM5_ROLL_FAMILY_BASE: usize = PM_ROLLING_MAX * PM_PAIRS;
/// Bit 31 of every 5-minute instance id (module docs).
pub const PM5_INSTANCE_FLAG: u32 = 1 << 31;
/// Roll family bytes in use by both pools: `0..PM_ROLL_FAMILIES`. The
/// router ledger keys a row by (venue, family) and refuses a family at or
/// above its row count, so it must hold at least this many.
pub const PM_ROLL_FAMILIES: usize = PM5_ROLL_FAMILY_BASE + PM5_ROLLING_MAX * PM5_PAIRS;

const _: () = assert!(PM5_OFF_MARK == 2 * PM5_PAIRS as u32);
const _: () = assert!(PM5_OFF_MARK + 1 == PM5_ROLLING_STRIDE);
// The 5 m pool sits wholly above the 15 m pool and inside the ordinal.
const _: () = assert!(
    PM5_ROLLING_ORDINAL_BASE >= PM_ROLLING_ORDINAL_BASE + PM_ROLLING_MAX as u32 * PM_ROLLING_STRIDE
);
const _: () = assert!(
    PM5_ROLLING_ORDINAL_BASE + PM5_ROLLING_MAX as u32 * PM5_ROLLING_STRIDE
        <= crate::SYMBOL_ORDINAL_MASK
);
// Every roll family fits the 8-bit family field of `pack_roll_seq`.
const _: () = assert!(PM_ROLL_FAMILIES <= 256);
// The flag bit is clear of every 5 m `window · 8 + g` this module builds.
const _: () = assert!(PM5_ROLLING_MAX as u32 <= 8);

/// The sym at `offset` (0..[`PM5_ROLLING_STRIDE`]) of 5-minute family
/// `family`.
#[inline(always)]
#[must_use]
pub const fn pm5_slot_sym(family: usize, offset: u32) -> SymbolId {
    debug_assert!(family < PM5_ROLLING_MAX && offset < PM5_ROLLING_STRIDE);
    make_symbol_id(
        VenueId::Polymarket,
        PM5_ROLLING_ORDINAL_BASE + family as u32 * PM5_ROLLING_STRIDE + offset,
    )
}

/// The token slot of `leg` in `pair` of 5-minute family `family`.
#[inline(always)]
#[must_use]
pub const fn pm5_rolling_sym(family: usize, pair: usize, leg: u32) -> SymbolId {
    debug_assert!(pair < PM5_PAIRS && leg <= PM_LEG_DOWN);
    pm5_slot_sym(family, pair as u32 * 2 + leg)
}

/// The mark slot of 5-minute family `family`.
#[inline(always)]
#[must_use]
pub const fn pm5_mark_sym(family: usize) -> SymbolId {
    pm5_slot_sym(family, PM5_OFF_MARK)
}

/// `(family, offset)` of a 5-minute slot sym; `None` for every other sym
/// (the 15-minute pool included).
#[inline(always)]
#[must_use]
pub const fn pm5_rolling_of(sym: SymbolId) -> Option<(usize, u32)> {
    if symbol_venue_byte(sym) != VenueId::Polymarket as u8 {
        return None;
    }
    let o = symbol_ordinal(sym);
    if o < PM5_ROLLING_ORDINAL_BASE {
        return None;
    }
    let rel = o - PM5_ROLLING_ORDINAL_BASE;
    let family = (rel / PM5_ROLLING_STRIDE) as usize;
    if family >= PM5_ROLLING_MAX {
        return None;
    }
    Some((family, rel % PM5_ROLLING_STRIDE))
}

/// The pair 5-minute window `window` lives on: window mod 3.
#[inline(always)]
#[must_use]
pub const fn pm5_pair_of_window(window: u32) -> usize {
    (window % PM5_PAIRS as u32) as usize
}

/// The roll's family byte for `pair` of 5-minute family `family`:
/// `16 + 3·family + pair` (module docs).
#[inline(always)]
#[must_use]
pub const fn pm5_roll_family(family: usize, pair: usize) -> usize {
    PM5_ROLL_FAMILY_BASE + family * PM5_PAIRS + pair
}

/// The instance id of 5-minute family `family`'s market for window
/// `window`: [`PM5_INSTANCE_FLAG`] `| (window · 8 + family)`; `None` past
/// the 31 bits (window ≥ 2²⁸, the year 4 500).
#[inline(always)]
#[must_use]
pub const fn pm5_instance_id(window: u32, family: usize) -> Option<u32> {
    if family >= PM5_ROLLING_MAX || window > (PM5_INSTANCE_FLAG - 1) / PM5_ROLLING_MAX as u32 {
        return None;
    }
    Some(PM5_INSTANCE_FLAG | (window * PM5_ROLLING_MAX as u32 + family as u32))
}

/// `(window, family)` of a 5-minute instance id — the inverse of
/// [`pm5_instance_id`]; `None` for an id without [`PM5_INSTANCE_FLAG`]
/// (a 15-minute one).
#[inline(always)]
#[must_use]
pub const fn pm5_instance_parts(id: u32) -> Option<(u32, usize)> {
    if id & PM5_INSTANCE_FLAG == 0 {
        return None;
    }
    let rel = id & !PM5_INSTANCE_FLAG;
    Some((
        rel / PM5_ROLLING_MAX as u32,
        (rel % PM5_ROLLING_MAX as u32) as usize,
    ))
}

/// The 5-minute window index of the window that OPENS at `open_s`
/// (Unix s); `None` off the 300 s grid or past `u32`.
#[inline(always)]
#[must_use]
pub const fn pm5_window_of_open(open_s: u64) -> Option<u32> {
    if open_s % PM5_WINDOW_S != 0 {
        return None;
    }
    let w = open_s / PM5_WINDOW_S;
    if w > u32::MAX as u64 {
        return None;
    }
    Some(w as u32)
}

/// The 5-minute window that CONTAINS Unix second `t_s`.
#[inline(always)]
#[must_use]
pub const fn pm5_window_at(t_s: u64) -> u32 {
    (t_s / PM5_WINDOW_S) as u32
}

/// The open of 5-minute window `window`, Unix s — also the slug's epoch.
#[inline(always)]
#[must_use]
pub const fn pm5_window_open_s(window: u32) -> u64 {
    window as u64 * PM5_WINDOW_S
}

/// The open of 5-minute window `window`, Unix ns.
#[inline(always)]
#[must_use]
pub const fn pm5_window_open_ns(window: u32) -> u64 {
    window as u64 * PM5_WINDOW_S * NS_PER_S
}

/// The end of 5-minute window `window`, Unix ns — a roll's `v1`.
#[inline(always)]
#[must_use]
pub const fn pm5_window_end_ns(window: u32) -> u64 {
    (window as u64 + 1) * PM5_WINDOW_S * NS_PER_S
}

// ---------------------------------------------------------------
// PMJ J1b: the period, for code that serves both pools
// ---------------------------------------------------------------

/// A rolling family's period: which pool, which window grid. The rolling
/// ingress and its discovery are built once per period (one pipeline per
/// pool, each on its own lane); every answer of a [`PmPeriod::M15`] call
/// is the 15-minute free function's own, so slot 9's pipeline is
/// unchanged bit for bit.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PmPeriod {
    /// The 15-minute families (slot 9).
    M15 = 0,
    /// The 5-minute families (PMJ, slot 11).
    M5 = 1,
}

/// The most token pairs either period keeps per family.
pub const PM_PAIRS_MAX: usize = PM5_PAIRS;
/// The most families either period carries.
pub const PM_FAMILIES_MAX: usize = PM_ROLLING_MAX;
const _: () = assert!(PM_PAIRS_MAX >= PM_PAIRS && PM_FAMILIES_MAX >= PM5_ROLLING_MAX);

impl PmPeriod {
    /// The window length, s.
    #[inline(always)]
    #[must_use]
    pub const fn window_s(self) -> u64 {
        match self {
            Self::M15 => PM_WINDOW_S,
            Self::M5 => PM5_WINDOW_S,
        }
    }

    /// Token pairs per family.
    #[inline(always)]
    #[must_use]
    pub const fn pairs(self) -> usize {
        match self {
            Self::M15 => PM_PAIRS,
            Self::M5 => PM5_PAIRS,
        }
    }

    /// The family cap.
    #[inline(always)]
    #[must_use]
    pub const fn max_families(self) -> usize {
        match self {
            Self::M15 => PM_ROLLING_MAX,
            Self::M5 => PM5_ROLLING_MAX,
        }
    }

    /// The slug's fixed middle: `-updown-15m-` / `-updown-5m-`.
    #[inline(always)]
    #[must_use]
    pub const fn slug_mid(self) -> &'static [u8] {
        match self {
            Self::M15 => b"-updown-15m-",
            Self::M5 => b"-updown-5m-",
        }
    }

    /// `cryptoMarketConfig.duration` on the venue: `15m` / `5m`.
    #[inline(always)]
    #[must_use]
    pub const fn duration(self) -> &'static [u8] {
        match self {
            Self::M15 => b"15m",
            Self::M5 => b"5m",
        }
    }

    /// The token slot of `leg` in `pair` of family `family`.
    #[inline(always)]
    #[must_use]
    pub const fn rolling_sym(self, family: usize, pair: usize, leg: u32) -> SymbolId {
        match self {
            Self::M15 => pm_rolling_sym(family, pair, leg),
            Self::M5 => pm5_rolling_sym(family, pair, leg),
        }
    }

    /// The mark slot of family `family`.
    #[inline(always)]
    #[must_use]
    pub const fn mark_sym(self, family: usize) -> SymbolId {
        match self {
            Self::M15 => pm_mark_sym(family),
            Self::M5 => pm5_mark_sym(family),
        }
    }

    /// `(family, offset)` of a slot sym of THIS period's pool.
    #[inline(always)]
    #[must_use]
    pub const fn rolling_of(self, sym: SymbolId) -> Option<(usize, u32)> {
        match self {
            Self::M15 => pm_rolling_of(sym),
            Self::M5 => pm5_rolling_of(sym),
        }
    }

    /// The pair window `window` lives on.
    #[inline(always)]
    #[must_use]
    pub const fn pair_of_window(self, window: u32) -> usize {
        match self {
            Self::M15 => pm_pair_of_window(window),
            Self::M5 => pm5_pair_of_window(window),
        }
    }

    /// The first roll family byte of this period: the family byte of
    /// local slot `s = family · pairs + pair` is `roll_family_base() + s`.
    #[inline(always)]
    #[must_use]
    pub const fn roll_family_base(self) -> usize {
        match self {
            Self::M15 => 0,
            Self::M5 => PM5_ROLL_FAMILY_BASE,
        }
    }

    /// The roll's family byte for `pair` of family `family`.
    #[inline(always)]
    #[must_use]
    pub const fn roll_family(self, family: usize, pair: usize) -> usize {
        match self {
            Self::M15 => pm_roll_family(family, pair),
            Self::M5 => pm5_roll_family(family, pair),
        }
    }

    /// The instance id of family `family`'s market for window `window`.
    #[inline(always)]
    #[must_use]
    pub const fn instance_id(self, window: u32, family: usize) -> Option<u32> {
        match self {
            Self::M15 => pm_instance_id(window, family),
            Self::M5 => pm5_instance_id(window, family),
        }
    }

    /// The window index of the window that OPENS at `open_s`.
    #[inline(always)]
    #[must_use]
    pub const fn window_of_open(self, open_s: u64) -> Option<u32> {
        match self {
            Self::M15 => pm_window_of_open(open_s),
            Self::M5 => pm5_window_of_open(open_s),
        }
    }

    /// The window that CONTAINS Unix second `t_s`.
    #[inline(always)]
    #[must_use]
    pub const fn window_at(self, t_s: u64) -> u32 {
        match self {
            Self::M15 => pm_window_at(t_s),
            Self::M5 => pm5_window_at(t_s),
        }
    }

    /// The open of window `window`, Unix s.
    #[inline(always)]
    #[must_use]
    pub const fn window_open_s(self, window: u32) -> u64 {
        match self {
            Self::M15 => pm_window_open_s(window),
            Self::M5 => pm5_window_open_s(window),
        }
    }

    /// The open of window `window`, Unix ns.
    #[inline(always)]
    #[must_use]
    pub const fn window_open_ns(self, window: u32) -> u64 {
        match self {
            Self::M15 => pm_window_open_ns(window),
            Self::M5 => pm5_window_open_ns(window),
        }
    }

    /// The end of window `window`, Unix ns.
    #[inline(always)]
    #[must_use]
    pub const fn window_end_ns(self, window: u32) -> u64 {
        match self {
            Self::M15 => pm_window_end_ns(window),
            Self::M5 => pm5_window_end_ns(window),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_round_trips_and_nothing_else_is_a_slot() {
        let mut f = 0usize;
        while f < PM_ROLLING_MAX {
            let mut off = 0u32;
            while off < PM_ROLLING_STRIDE {
                let s = pm_slot_sym(f, off);
                assert_eq!(pm_rolling_of(s), Some((f, off)));
                assert_eq!(symbol_venue_byte(s), VenueId::Polymarket as u8);
                off += 1;
            }
            // The legs of a pair are adjacent, Up first (the ledger's
            // `sym_no = sym_yes + 1`), and the mark is the last offset.
            let mut pair = 0usize;
            while pair < PM_PAIRS {
                let up = pm_rolling_sym(f, pair, PM_LEG_UP);
                assert_eq!(pm_rolling_sym(f, pair, PM_LEG_DOWN), up + 1);
                pair += 1;
            }
            assert_eq!(pm_rolling_of(pm_mark_sym(f)), Some((f, PM_OFF_MARK)));
            f += 1;
        }
        // Below the pool, above it, another venue, the sentinel.
        assert_eq!(pm_rolling_of(make_symbol_id(VenueId::Polymarket, 42)), None);
        assert_eq!(pm_rolling_of(make_symbol_id(VenueId::Polymarket, 7)), None);
        assert_eq!(
            pm_rolling_of(make_symbol_id(
                VenueId::Polymarket,
                PM_ROLLING_ORDINAL_BASE - 1
            )),
            None
        );
        assert_eq!(
            pm_rolling_of(make_symbol_id(
                VenueId::Polymarket,
                PM_ROLLING_ORDINAL_BASE + PM_ROLLING_MAX as u32 * PM_ROLLING_STRIDE
            )),
            None
        );
        assert_eq!(
            pm_rolling_of(make_symbol_id(
                VenueId::Hyperliquid,
                PM_ROLLING_ORDINAL_BASE
            )),
            None
        );
        assert_eq!(pm_rolling_of(crate::SYMBOL_ID_NONE), None);
    }

    #[test]
    fn window_math_matches_the_slug_epochs() {
        // btc-updown-15m-1792216800: opens 2026-10-17 06:00:00Z.
        let w = pm_window_of_open(1_792_216_800).expect("on the grid");
        assert_eq!(pm_window_open_s(w), 1_792_216_800);
        assert_eq!(pm_window_open_ns(w), 1_792_216_800 * NS_PER_S);
        assert_eq!(pm_window_end_ns(w), 1_792_217_700 * NS_PER_S);
        assert_eq!(pm_window_at(1_792_216_800), w);
        assert_eq!(pm_window_at(1_792_217_699), w);
        assert_eq!(pm_window_at(1_792_217_700), w + 1);
        assert_eq!(pm_window_of_open(1_792_216_801), None);
        // Consecutive windows alternate pairs.
        assert_ne!(pm_pair_of_window(w), pm_pair_of_window(w + 1));
        assert_eq!(pm_pair_of_window(w), pm_pair_of_window(w + 2));
    }

    #[test]
    fn instance_ids_are_unique_per_market_and_invert() {
        let w = pm_window_of_open(1_792_216_800).expect("on the grid");
        let btc = pm_instance_id(w, 0).expect("in range");
        let eth = pm_instance_id(w, 1).expect("in range");
        assert_ne!(btc, eth, "one window, two assets, two instances");
        assert_eq!(pm_instance_parts(btc), (w, 0));
        assert_eq!(pm_instance_parts(eth), (w, 1));
        assert_ne!(pm_instance_id(w + 1, 0), Some(btc));
        assert_eq!(pm_instance_id(w, PM_ROLLING_MAX), None);
        assert_eq!(pm_instance_id(u32::MAX / 8 + 1, 0), None);
        let top = pm_instance_id(u32::MAX / 8, PM_ROLLING_MAX - 1).expect("the last one fits");
        assert_eq!(pm_instance_parts(top), (u32::MAX / 8, PM_ROLLING_MAX - 1));
        // Roll families: two per family, all distinct and < 16.
        assert_eq!(pm_roll_family(0, 0), 0);
        assert_eq!(pm_roll_family(0, 1), 1);
        assert_eq!(pm_roll_family(3, 1), 7);
        assert_eq!(pm_roll_family(PM_ROLLING_MAX - 1, 1), 15);
    }

    #[test]
    fn every_5m_slot_round_trips_and_the_two_pools_are_disjoint() {
        let mut g = 0usize;
        while g < PM5_ROLLING_MAX {
            let mut off = 0u32;
            while off < PM5_ROLLING_STRIDE {
                let s = pm5_slot_sym(g, off);
                assert_eq!(pm5_rolling_of(s), Some((g, off)));
                assert_eq!(pm_rolling_of(s), None, "a 5 m slot is no 15 m slot");
                assert_eq!(symbol_venue_byte(s), VenueId::Polymarket as u8);
                off += 1;
            }
            let mut pair = 0usize;
            while pair < PM5_PAIRS {
                let up = pm5_rolling_sym(g, pair, PM_LEG_UP);
                assert_eq!(pm5_rolling_sym(g, pair, PM_LEG_DOWN), up + 1);
                pair += 1;
            }
            assert_eq!(pm5_rolling_of(pm5_mark_sym(g)), Some((g, PM5_OFF_MARK)));
            g += 1;
        }
        // Every 15 m slot is no 5 m slot.
        let mut f = 0usize;
        while f < PM_ROLLING_MAX {
            let mut off = 0u32;
            while off < PM_ROLLING_STRIDE {
                assert_eq!(pm5_rolling_of(pm_slot_sym(f, off)), None);
                off += 1;
            }
            f += 1;
        }
        // The gap between the pools, past the 5 m pool, another venue.
        let gap = PM_ROLLING_ORDINAL_BASE + PM_ROLLING_MAX as u32 * PM_ROLLING_STRIDE;
        assert_eq!(pm5_rolling_of(make_symbol_id(VenueId::Polymarket, gap)), None);
        assert_eq!(pm_rolling_of(make_symbol_id(VenueId::Polymarket, gap)), None);
        assert_eq!(
            pm5_rolling_of(make_symbol_id(
                VenueId::Polymarket,
                PM5_ROLLING_ORDINAL_BASE - 1
            )),
            None
        );
        assert_eq!(
            pm5_rolling_of(make_symbol_id(
                VenueId::Polymarket,
                PM5_ROLLING_ORDINAL_BASE + PM5_ROLLING_MAX as u32 * PM5_ROLLING_STRIDE
            )),
            None
        );
        assert_eq!(
            pm5_rolling_of(make_symbol_id(
                VenueId::Hyperliquid,
                PM5_ROLLING_ORDINAL_BASE
            )),
            None
        );
        assert_eq!(pm5_rolling_of(crate::SYMBOL_ID_NONE), None);
        assert_eq!(PM5_SLOT_TAGS[PM5_OFF_MARK as usize], "mark");
    }

    #[test]
    fn five_minute_window_math_matches_the_slug_epochs() {
        // btc-updown-5m-1792308000: opens 2026-10-18 07:20:00Z.
        let w = pm5_window_of_open(1_792_308_000).expect("on the grid");
        assert_eq!(w, 5_974_360);
        assert_eq!(pm5_window_open_s(w), 1_792_308_000);
        assert_eq!(pm5_window_open_ns(w), 1_792_308_000 * NS_PER_S);
        assert_eq!(pm5_window_end_ns(w), 1_792_308_300 * NS_PER_S);
        assert_eq!(pm5_window_at(1_792_308_299), w);
        assert_eq!(pm5_window_at(1_792_308_300), w + 1);
        assert_eq!(pm5_window_of_open(1_792_308_060), None);
        // Three pairs in turn: a pair is reused three windows on.
        assert_ne!(pm5_pair_of_window(w), pm5_pair_of_window(w + 1));
        assert_ne!(pm5_pair_of_window(w), pm5_pair_of_window(w + 2));
        assert_ne!(pm5_pair_of_window(w + 1), pm5_pair_of_window(w + 2));
        assert_eq!(pm5_pair_of_window(w), pm5_pair_of_window(w + 3));
        // A 15 m open is also on the 5 m grid; the indices differ.
        assert_eq!(
            pm5_window_of_open(1_792_216_800),
            Some(pm_window_of_open(1_792_216_800).expect("15 m") * 3)
        );
    }

    #[test]
    fn five_minute_instance_ids_carry_the_flag_invert_and_never_meet_a_15m_one() {
        let w = pm5_window_of_open(1_792_308_000).expect("on the grid");
        let btc = pm5_instance_id(w, 0).expect("in range");
        let eth = pm5_instance_id(w, 1).expect("in range");
        assert_ne!(btc, eth);
        assert_ne!(btc & PM5_INSTANCE_FLAG, 0);
        assert_eq!(pm5_instance_parts(btc), Some((w, 0)));
        assert_eq!(pm5_instance_parts(eth), Some((w, 1)));
        // The same numbers as a 15 m (window, family) are another id.
        let same = pm_instance_id(w, 0).expect("in range");
        assert_ne!(same, btc);
        assert_eq!(pm5_instance_parts(same), None, "a 15 m id is not a 5 m one");
        assert_eq!(pm5_instance_id(w, PM5_ROLLING_MAX), None);
        let last = (PM5_INSTANCE_FLAG - 1) / PM5_ROLLING_MAX as u32;
        assert_eq!(pm5_instance_id(last + 1, 0), None);
        let top = pm5_instance_id(last, PM5_ROLLING_MAX - 1).expect("the last one fits");
        assert_eq!(pm5_instance_parts(top), Some((last, PM5_ROLLING_MAX - 1)));
        // Roll families: three per family, 16..40, disjoint from 15 m's.
        assert_eq!(pm5_roll_family(0, 0), 16);
        assert_eq!(pm5_roll_family(0, 2), 18);
        assert_eq!(pm5_roll_family(1, 0), 19);
        assert_eq!(pm5_roll_family(PM5_ROLLING_MAX - 1, 2), PM_ROLL_FAMILIES - 1);
        assert_eq!(PM_ROLL_FAMILIES, 40);
        assert!(pm5_roll_family(0, 0) > pm_roll_family(PM_ROLLING_MAX - 1, 1));
    }

    #[test]
    fn the_15m_period_is_the_15m_free_functions_bit_for_bit() {
        let p = PmPeriod::M15;
        let w = pm_window_of_open(1_792_216_800).expect("on the grid");
        assert_eq!(p.window_of_open(1_792_216_800), Some(w));
        assert_eq!(p.window_at(1_792_217_699), pm_window_at(1_792_217_699));
        assert_eq!(p.window_open_s(w), pm_window_open_s(w));
        assert_eq!(p.window_open_ns(w), pm_window_open_ns(w));
        assert_eq!(p.window_end_ns(w), pm_window_end_ns(w));
        assert_eq!(p.pairs(), PM_PAIRS);
        assert_eq!(p.window_s(), PM_WINDOW_S);
        assert_eq!(p.slug_mid(), b"-updown-15m-");
        let mut f = 0usize;
        while f < PM_ROLLING_MAX {
            assert_eq!(p.mark_sym(f), pm_mark_sym(f));
            let mut pair = 0usize;
            while pair < PM_PAIRS {
                assert_eq!(p.roll_family(f, pair), pm_roll_family(f, pair));
                assert_eq!(p.rolling_sym(f, pair, PM_LEG_DOWN), pm_rolling_sym(f, pair, PM_LEG_DOWN));
                pair += 1;
            }
            assert_eq!(p.instance_id(w, f), pm_instance_id(w, f));
            f += 1;
        }
        assert_eq!(p.pair_of_window(w + 1), pm_pair_of_window(w + 1));
        // And the 5 m period is the 5 m pool.
        let q = PmPeriod::M5;
        let w5 = q.window_of_open(1_792_308_000).expect("on the grid");
        assert_eq!(q.instance_id(w5, 2), pm5_instance_id(w5, 2));
        assert_eq!(q.rolling_of(q.mark_sym(3)), Some((3, PM5_OFF_MARK)));
        assert_eq!(q.rolling_of(p.mark_sym(3)), None);
        assert_eq!(q.slug_mid(), b"-updown-5m-");
        assert_eq!(q.duration(), b"5m");
        assert_eq!(q.pairs(), 3);
        // The local slot's family byte is the pool's roll family.
        let (mut f, mut pair) = (0usize, 0usize);
        while f < PM5_ROLLING_MAX {
            while pair < PM5_PAIRS {
                assert_eq!(q.roll_family_base() + f * q.pairs() + pair, q.roll_family(f, pair));
                assert_eq!(p.roll_family_base() + (f * p.pairs() + pair % 2), p.roll_family(f, pair % 2));
                pair += 1;
            }
            pair = 0;
            f += 1;
        }
    }
}
