// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The venue's perp price and size rules, applied LOCALLY (XH4c, plan
//! §8.2): an order that breaks one is refused or rounded before it is
//! signed — never sent for the venue to reject (a reject costs address
//! budget and, for a post-only quote, reads like a market move).
//!
//! Hyperliquid, perps (the venue's docs, as `ingress_hyperliquid::discovery`
//! records them):
//! - **price:** an integer price is always valid; otherwise at most 5
//!   significant figures AND at most `6 − szDecimals` decimal places;
//! - **size:** a multiple of the lot `10^−szDecimals`;
//! - **notional:** price × size ≥ $10.
//!
//! In the house's 1e8 fixed point the price rule is one number: the tick
//! at a price level, `10^(8 − d)` with `d = min(6 − szDecimals, 4 − e)`
//! clamped at 0, where `e = ⌊log10 px⌋`. A price is valid iff it is a
//! multiple of the tick at its own level; rounding to that tick is
//! always valid (rounding down keeps `e` or lowers it; rounding up lands
//! on the same level or exactly on the next power of ten).
//!
//! Pure integer arithmetic, `const` where it can be, no allocation.

/// 10^0 … 10^18.
const POW10: [i64; 19] = {
    let mut t = [1i64; 19];
    let mut i = 1usize;
    while i < 19 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// $10 in 1e8 × 1e8 units (price 1e8 × size 1e8).
pub const MIN_NOTIONAL_1E16: i128 = 10 * 10_000_000_000_000_000;

/// Which way a price is rounded to its tick.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Round {
    /// Toward zero — a maker's bid (away from the touch).
    Down,
    /// Away from zero — a maker's ask (away from the touch).
    Up,
}

/// Why an order breaks a rule.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RuleErr {
    /// Price or size is zero or negative.
    NonPositive,
    /// The price is finer than its tick (too many significant figures or
    /// decimals).
    Price,
    /// The size is not a multiple of the lot.
    Lot,
    /// Price × size is below $10.
    MinNotional,
}

/// One perp's rules: its `szDecimals`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PerpRules {
    sz_decimals: u8,
}

impl PerpRules {
    /// The rules for a perp with `sz_decimals`; `None` above 6 (the
    /// price's decimal budget `6 − szDecimals` would be negative — no
    /// perp has one).
    #[must_use]
    pub const fn new(sz_decimals: u8) -> Option<Self> {
        if sz_decimals > 6 {
            None
        } else {
            Some(Self { sz_decimals })
        }
    }

    /// `szDecimals`.
    #[inline(always)]
    #[must_use]
    pub const fn sz_decimals(&self) -> u8 {
        self.sz_decimals
    }

    /// The price tick (1e8) at `px_1e8`'s level; `px_1e8` > 0.
    #[inline]
    #[must_use]
    pub const fn tick_1e8(&self, px_1e8: i64) -> i64 {
        debug_assert!(px_1e8 > 0, "a tick is for a positive price");
        // ⌊log10 px⌋ = ⌊log10 px_1e8⌋ − 8.
        let lg = match px_1e8.checked_ilog10() {
            Some(v) => v as i64,
            None => 0,
        };
        let e = lg - 8;
        let by_sig = 4 - e;
        let by_dec = 6 - self.sz_decimals as i64;
        let d = if by_sig < by_dec { by_sig } else { by_dec };
        let d = if d < 0 { 0 } else { d };
        POW10[(8 - d) as usize]
    }

    /// `px_1e8` rounded to its tick in `dir`; `None` when it is not
    /// positive or rounds to zero.
    #[inline]
    #[must_use]
    pub const fn round_px(&self, px_1e8: i64, dir: Round) -> Option<i64> {
        if px_1e8 <= 0 {
            return None;
        }
        let tick = self.tick_1e8(px_1e8);
        let rem = px_1e8 % tick;
        let r = match dir {
            Round::Down => px_1e8 - rem,
            Round::Up => {
                if rem == 0 {
                    px_1e8
                } else {
                    match (px_1e8 - rem).checked_add(tick) {
                        Some(v) => v,
                        None => return None,
                    }
                }
            }
        };
        if r <= 0 {
            None
        } else {
            Some(r)
        }
    }

    /// The size lot (1e8).
    #[inline(always)]
    #[must_use]
    pub const fn lot_1e8(&self) -> i64 {
        POW10[(8 - self.sz_decimals) as usize]
    }

    /// `sz_1e8` rounded down to the lot (never up: a size grows no
    /// position the member did not ask for).
    #[inline(always)]
    #[must_use]
    pub const fn round_sz_down(&self, sz_1e8: i64) -> i64 {
        sz_1e8 - sz_1e8 % self.lot_1e8()
    }

    /// Whether the order may be sent as is.
    ///
    /// # Errors
    /// The first rule it breaks.
    pub const fn check(&self, px_1e8: i64, sz_1e8: i64) -> Result<(), RuleErr> {
        if px_1e8 <= 0 || sz_1e8 <= 0 {
            return Err(RuleErr::NonPositive);
        }
        if px_1e8 % self.tick_1e8(px_1e8) != 0 {
            return Err(RuleErr::Price);
        }
        if sz_1e8 % self.lot_1e8() != 0 {
            return Err(RuleErr::Lot);
        }
        if (px_1e8 as i128) * (sz_1e8 as i128) < MIN_NOTIONAL_1E16 {
            return Err(RuleErr::MinNotional);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireNum;

    const E8: i64 = 100_000_000;

    fn render(px_1e8: i64) -> String {
        String::from_utf8(WireNum::from_1e8(px_1e8).as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn btc_like_prices_are_integers_above_five_digits() {
        // BTC: szDecimals 5 → at most 1 decimal, and 5 significant figures.
        let r = PerpRules::new(5).unwrap();
        assert_eq!(
            r.tick_1e8(65_432 * E8 + E8 / 2),
            E8,
            "6 figures: whole dollars"
        );
        assert_eq!(
            render(r.round_px(65_432 * E8 + E8 / 2, Round::Down).unwrap()),
            "65432"
        );
        assert_eq!(
            render(r.round_px(65_432 * E8 + E8 / 2, Round::Up).unwrap()),
            "65433"
        );
        assert_eq!(
            r.check(123_456 * E8, 100_000),
            Ok(()),
            "an integer is always valid"
        );
        assert_eq!(r.check(65_432 * E8 + E8 / 2, 100_000), Err(RuleErr::Price));
        assert_eq!(
            r.tick_1e8(9_999 * E8),
            E8 / 10,
            "4 integer digits: 1 decimal left"
        );
        assert_eq!(r.check(9_999 * E8 + E8 / 10, 1_000_000), Ok(()), "$99.99");
    }

    #[test]
    fn small_prices_are_bounded_by_decimals_and_figures() {
        // szDecimals 0 → 6 decimals; 0.012345 has 5 figures and 6 decimals.
        let r = PerpRules::new(0).unwrap();
        assert_eq!(r.check(1_234_500, E8 * 1_000), Ok(()));
        assert_eq!(
            r.check(1_234_560, E8 * 1_000),
            Err(RuleErr::Price),
            "7 decimals"
        );
        // 1.23456 has 6 figures: 4 decimals at most.
        assert_eq!(r.round_px(123_456_000, Round::Down), Some(123_450_000));
        assert_eq!(r.round_px(123_456_000, Round::Up), Some(123_460_000));
        // szDecimals 4 → 2 decimals even where figures would allow more.
        let r4 = PerpRules::new(4).unwrap();
        assert_eq!(r4.round_px(123_456_000, Round::Down), Some(123_000_000));
        assert_eq!(render(r4.round_px(123_456_000, Round::Up).unwrap()), "1.24");
    }

    #[test]
    fn rounding_up_across_a_power_of_ten_stays_valid() {
        let r = PerpRules::new(1).unwrap();
        let px = 999_995 * (E8 / 100); // 9999.95
        let up = r.round_px(px, Round::Up).unwrap();
        assert_eq!(up, 10_000 * E8);
        assert_eq!(r.check(up, 10 * E8 / 10), Ok(()));
        assert_eq!(r.round_px(0, Round::Up), None);
        assert_eq!(r.round_px(-5, Round::Down), None);
        let tiny = PerpRules::new(6).unwrap();
        assert_eq!(
            tiny.round_px(1, Round::Down),
            None,
            "below the smallest tick"
        );
    }

    #[test]
    fn every_rounded_price_passes_the_check() {
        let mut sz = 0u8;
        while sz <= 6 {
            let r = PerpRules::new(sz).unwrap();
            let mut px = 7i64;
            while px < 5_000_000 * E8 {
                let mut k = 0;
                while k < 2 {
                    let dir = if k == 0 { Round::Down } else { Round::Up };
                    if let Some(p) = r.round_px(px, dir) {
                        assert_eq!(
                            r.check(p, r.lot_1e8() * 1_000_000_000),
                            Ok(()),
                            "sz {sz} px {px} {dir:?} -> {p}"
                        );
                        assert!(if k == 0 { p <= px } else { p >= px });
                    }
                    k += 1;
                }
                px = px * 3 + 1;
            }
            sz += 1;
        }
    }

    #[test]
    fn sizes_lots_and_notional() {
        let r = PerpRules::new(3).unwrap();
        assert_eq!(r.lot_1e8(), 100_000, "0.001");
        assert_eq!(r.round_sz_down(123_456), 100_000);
        assert_eq!(r.check(2_000 * E8, 123_456), Err(RuleErr::Lot));
        assert_eq!(
            r.check(2_000 * E8, 100_000),
            Err(RuleErr::MinNotional),
            "$2"
        );
        assert_eq!(r.check(2_000 * E8, 500_000), Ok(()), "$10 exactly");
        assert_eq!(r.check(2_000 * E8, 0), Err(RuleErr::NonPositive));
        assert_eq!(PerpRules::new(7), None);
    }
}
