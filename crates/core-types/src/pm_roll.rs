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
}
