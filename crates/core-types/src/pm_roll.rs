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
//! **The slot pool.** Family `f` (0-based, in the boot `[polymarket]
//! rolling` order) owns [`PM_ROLLING_STRIDE`] consecutive ordinals of venue
//! byte 0 from [`PM_ROLLING_ORDINAL_BASE`]: `[up]`, `[down]` and `[mark]` —
//! the asset's Chainlink reference price (RTDS `crypto_prices_chainlink`,
//! or PolyBolt `price.crypto`), delivered as `ChannelId::Mark` events on
//! that sym. The pool sits far above every flat static-token ordinal
//! (`flat + 1 ≤ 2·PM_MARKETS_MAX`) and above the legacy anchors 7 and 42,
//! so a slot can never alias a static market. That matters because
//! Polymarket is venue byte 0: a LOW Polymarket ordinal reads as a legacy
//! anchor id.
//!
//! **The roll event** (venue byte Polymarket):
//!
//! | field | value |
//! |---|---|
//! | `sym` | the family's `[up]` slot ([`PM_LEG_UP`]) |
//! | `venue_seq` | [`crate::pack_roll_seq`]`(window, PM_TWAP_S, family, settled)` — `window` = open epoch / 900 |
//! | `v1` | the window END, Unix ns ([`pm_window_end_ns`]) |
//! | `v0` | created: 0 — the strike (`priceToBeat`) is not published before resolution, so the consumer computes it from the `[mark]` stream; settled: [`PM_SETTLED_UP`] / [`PM_SETTLED_DOWN`] / [`PM_SETTLED_UNKNOWN`] |
//! | `venue_time_ms` | 0 |
//!
//! The ingress is the only writer. Pure `const fn`s; no allocation.

use crate::{make_symbol_id, symbol_ordinal, symbol_venue_byte, SymbolId, VenueId};

/// Cap on `[polymarket] rolling` families (the seven 15-minute crypto
/// assets fit with one to spare).
pub const PM_ROLLING_MAX: usize = 8;

/// Ordinal base of the rolling slot pool (venue byte 0).
pub const PM_ROLLING_ORDINAL_BASE: u32 = 4096;

/// Ordinals per family: `[up]`, `[down]`, `[mark]`.
pub const PM_ROLLING_STRIDE: u32 = 3;

/// Leg of the Up outcome token.
pub const PM_LEG_UP: u32 = 0;
/// Leg of the Down outcome token.
pub const PM_LEG_DOWN: u32 = 1;
/// Leg of the asset's Chainlink reference price.
pub const PM_LEG_MARK: u32 = 2;

/// Window length in seconds (the 15-minute family).
pub const PM_WINDOW_S: u64 = 900;

/// Settlement TWAP length in seconds (`cryptoMarketConfig.twapLookbackSeconds`).
pub const PM_TWAP_S: u32 = 60;

/// Settled roll `v0`: the Up token won.
pub const PM_SETTLED_UP: i64 = 1;
/// Settled roll `v0`: the Down token won.
pub const PM_SETTLED_DOWN: i64 = -1;
/// Settled roll `v0`: the venue's resolution named no winner we know.
pub const PM_SETTLED_UNKNOWN: i64 = 0;

const NS_PER_S: u64 = 1_000_000_000;

const _: () = assert!(PM_LEG_UP < PM_ROLLING_STRIDE);
const _: () = assert!(PM_LEG_DOWN < PM_ROLLING_STRIDE);
const _: () = assert!(PM_LEG_MARK < PM_ROLLING_STRIDE);
// The whole pool fits the 24-bit ordinal and stays clear of the flat
// static-token ordinals (≤ 2 × 64 + 1) and the legacy anchors (7, 42).
const _: () = assert!(
    PM_ROLLING_ORDINAL_BASE + PM_ROLLING_MAX as u32 * PM_ROLLING_STRIDE
        <= crate::SYMBOL_ORDINAL_MASK
);
const _: () = assert!(PM_ROLLING_ORDINAL_BASE > 2 * 64 + 1);

/// The slot sym of `leg` in family `family`.
#[inline(always)]
#[must_use]
pub const fn pm_rolling_sym(family: usize, leg: u32) -> SymbolId {
    debug_assert!(family < PM_ROLLING_MAX && leg < PM_ROLLING_STRIDE);
    make_symbol_id(
        VenueId::Polymarket,
        PM_ROLLING_ORDINAL_BASE + family as u32 * PM_ROLLING_STRIDE + leg,
    )
}

/// `(family, leg)` of a rolling slot sym; `None` for every other sym.
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
            let mut leg = 0u32;
            while leg < PM_ROLLING_STRIDE {
                let s = pm_rolling_sym(f, leg);
                assert_eq!(pm_rolling_of(s), Some((f, leg)));
                assert_eq!(symbol_venue_byte(s), VenueId::Polymarket as u8);
                leg += 1;
            }
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
        assert_eq!(pm_window_end_ns(w), 1_792_217_700 * NS_PER_S);
        assert_eq!(pm_window_at(1_792_216_800), w);
        assert_eq!(pm_window_at(1_792_217_699), w);
        assert_eq!(pm_window_at(1_792_217_700), w + 1);
        assert_eq!(pm_window_of_open(1_792_216_801), None);
    }
}
