// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The FAK BUY amounts law (plan D-3; spec §A.2, §A.6).
//!
//! A FOK / FAK BUY **targets collateral**: `makerAmount` is the pUSD the
//! order may spend and `takerAmount` the fewest shares it accepts, so its
//! limit is `makerAmount / takerAmount`. A price improvement buys more
//! shares for the same dollars — the dollar caps upstream hold whatever
//! the book does. The SDKs' market-order rule, which the venue's rounding
//! checks follow, is:
//!
//! * `makerAmount` = the dollars rounded **down to cents** (2 dp);
//! * `takerAmount` = `makerAmount / price` **truncated** to the tick's
//!   amount precision (4 dp at tick 0.01, 5 dp at tick 0.001, …).
//!
//! In 6-decimal base units both are exact integer operations here: the
//! SDK's float `round_up(·, dp + 4)` before its `round_down(·, dp)` only
//! cleans float noise, because `maker / price` never lies within
//! `10^-(dp+4)` below a `10^-dp` boundary for an on-tick price (the gap
//! is at least `10^-dp / 999`). The tests pin the SDKs' vectors.

/// One whole unit (pUSD or a share) in base units.
pub const UNIT: u64 = 1_000_000;

/// One cent in base units — `makerAmount`'s grain on a FAK BUY.
pub const CENT: u64 = 10_000;

/// The venue's floor on a marketable BUY's dollars, applied as a
/// precaution (spec §A.4: reported by a third party, unconfirmed; the
/// arm's orders are far above it).
pub const MIN_MARKETABLE_BUY: u64 = UNIT;

/// The amounts of one order, base units.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Amounts {
    /// What the maker gives: pUSD for a BUY.
    pub maker: u64,
    /// What the maker receives at least: shares for a BUY.
    pub taker: u64,
}

/// Why no amounts exist for an order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AmountErr {
    /// A tick the rounding table does not name.
    Tick,
    /// A price off the tick, or outside `[tick, 1 − tick]`.
    Price,
    /// Less than a cent to spend.
    Empty,
}

/// The base-unit grain of `takerAmount` at `tick_1e6` (the SDKs'
/// `ROUNDING_CONFIG` `amount` digits): `None` for an unknown tick.
#[inline]
#[must_use]
pub fn amount_grain(tick_1e6: u64) -> Option<u64> {
    match tick_1e6 {
        100_000 => Some(1_000),
        10_000 => Some(100),
        5_000 | 1_000 => Some(10),
        2_500 | 100 => Some(1),
        _ => None,
    }
}

/// A FAK BUY of up to `usd` (base units) at the limit `px_1e6` on the
/// tick `tick_1e6`.
///
/// # Errors
/// [`AmountErr`] — the order is refused unsent.
#[inline]
pub fn fak_buy(usd: u64, px_1e6: u64, tick_1e6: u64) -> Result<Amounts, AmountErr> {
    let grain = match amount_grain(tick_1e6) {
        Some(g) => g,
        None => return Err(AmountErr::Tick),
    };
    if px_1e6 % tick_1e6 != 0 || px_1e6 < tick_1e6 || px_1e6 > UNIT - tick_1e6 {
        return Err(AmountErr::Price);
    }
    let maker = usd - usd % CENT;
    if maker == 0 {
        return Err(AmountErr::Empty);
    }
    let exact = (u128::from(maker) * u128::from(UNIT) / u128::from(px_1e6)) as u64;
    Ok(Amounts {
        maker,
        taker: exact - exact % grain,
    })
}

/// The dollars `qty_1e6` shares cost at `px_1e6`, rounded down to the
/// cent — what a member's `(qty, px)` order may spend.
#[inline]
#[must_use]
pub fn usd_of(qty_1e6: u64, px_1e6: u64) -> u64 {
    let usd = (u128::from(qty_1e6) * u128::from(px_1e6) / u128::from(UNIT)) as u64;
    usd - usd % CENT
}

/// Whether `a` clears the venue's floors: at least `min_shares_1e6`
/// shares (the market's `min_order_size`) and [`MIN_MARKETABLE_BUY`].
#[inline]
#[must_use]
pub fn clears_floors(a: &Amounts, min_shares_1e6: u64) -> bool {
    (a.taker >= min_shares_1e6) & (a.maker >= MIN_MARKETABLE_BUY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDK (py `builder.py` market rule; rs `order_builder.rs:637-641`):
    /// 10 pUSD at 0.52 on tick 0.01 → 10 000 000 / 19 230 700.
    #[test]
    fn sdk_market_buy_vector() {
        assert_eq!(
            fak_buy(10 * UNIT, 520_000, 10_000),
            Ok(Amounts {
                maker: 10_000_000,
                taker: 19_230_700
            })
        );
    }

    #[test]
    fn the_grain_follows_the_tick() {
        // $250 at 0.957 on tick 0.001: 250 / 0.957 = 261.23301985… → 5 dp.
        assert_eq!(
            fak_buy(250 * UNIT, 957_000, 1_000),
            Ok(Amounts {
                maker: 250_000_000,
                taker: 261_233_010
            })
        );
        // The same at 0.96 on tick 0.01: 260.41666… → 4 dp.
        assert_eq!(
            fak_buy(250 * UNIT, 960_000, 10_000),
            Ok(Amounts {
                maker: 250_000_000,
                taker: 260_416_600
            })
        );
        assert_eq!(amount_grain(100_000), Some(1_000));
        assert_eq!(amount_grain(2_500), Some(1));
        assert_eq!(amount_grain(20_000), None);
    }

    #[test]
    fn dollars_round_down_to_the_cent_and_the_price_is_judged() {
        assert_eq!(
            fak_buy(12_345_678, 500_000, 10_000).unwrap().maker,
            12_340_000
        );
        assert_eq!(fak_buy(9_999, 500_000, 10_000), Err(AmountErr::Empty));
        assert_eq!(fak_buy(UNIT, 505_000, 10_000), Err(AmountErr::Price));
        assert_eq!(fak_buy(UNIT, 0, 10_000), Err(AmountErr::Price));
        assert_eq!(fak_buy(UNIT, 1_000_000, 10_000), Err(AmountErr::Price));
        assert_eq!(fak_buy(UNIT, 990_000, 10_000).map(|a| a.maker), Ok(UNIT));
        assert_eq!(fak_buy(UNIT, 500_000, 7_000), Err(AmountErr::Tick));
    }

    /// The limit the venue reads, `maker / taker`, never sits below the
    /// member's price (truncating shares only raises it), and the
    /// shares never exceed what the dollars buy at the price.
    #[test]
    fn the_implied_limit_is_at_or_just_above_the_price_on_every_tick_price() {
        for tick in [10_000u64, 1_000] {
            let mut px = tick;
            while px < UNIT {
                for usd in [UNIT, 7 * UNIT + 30_000, 250 * UNIT] {
                    let a = fak_buy(usd, px, tick).unwrap();
                    assert!(
                        u128::from(a.taker) * u128::from(px)
                            <= u128::from(a.maker) * u128::from(UNIT)
                    );
                    let grain = amount_grain(tick).unwrap();
                    assert!(
                        u128::from(a.taker + grain) * u128::from(px)
                            > u128::from(a.maker) * u128::from(UNIT),
                        "taker is the largest on-grain value at {px}"
                    );
                }
                px += tick;
            }
        }
    }

    #[test]
    fn usd_of_and_the_floors() {
        assert_eq!(usd_of(260_416_600, 960_000), 249_990_000);
        assert_eq!(usd_of(5 * UNIT, 950_000), 4_750_000);
        let a = fak_buy(usd_of(5 * UNIT, 950_000), 950_000, 10_000).unwrap();
        assert_eq!(a.taker, 5 * UNIT);
        assert!(clears_floors(&a, 5 * UNIT));
        // 4.99 / 0.99 = 5.0404 shares: under a 5.1-share minimum.
        let b = fak_buy(4_990_000, 990_000, 10_000).unwrap();
        assert_eq!(b.taker, 5_040_400);
        assert!(!clears_floors(&b, 5_100_000));
        let c = fak_buy(990_000, 10_000, 10_000).unwrap();
        assert!(!clears_floors(&c, 5 * UNIT), "under the $1 floor");
    }
}
