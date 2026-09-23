// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The hedge side: Hyperliquid touches, the venue selector (plan §8.5,
//! O-H10) and the fee-folded leg a decision assumes.
//!
//! ## The cost model (bps × 1e6, signed, relative to the venue's mid)
//!
//! ```text
//! spot = half_spread + taker_spot + depth_penalty
//! perp = half_spread + taker_perp + depth_penalty + |perp − spot| − funding
//! ```
//!
//! `half_spread` is what an IoC at the touch pays against the mid — the
//! plan's formula is mid-relative and silent on it, but the two books'
//! spreads differ and the IoC pays exactly that. `depth_penalty` charges
//! the notional beyond the displayed touch one more full spread (the
//! next level is at least a tick away; with `depth_cap_enabled` the size
//! never exceeds the touch and the term is zero). `|perp − spot|` is the
//! perp's basis to the spot book (0 without a usable spot). `funding` is
//! the hourly rate over `funding_window_ns`, signed by our side: a short
//! perp EARNS positive funding, so it lowers a sell's cost and raises a
//! buy's.

use core_types::{NsTs, Tick, SYMBOL_ID_NONE};

use crate::{mul_div_i64, HedgeLeg, HyparbStrategy, COIN_USD, E6, ONE_BPS_1E6};

/// A hedge touch older than this is not traded against: a feed that dies
/// never marks its last tick stale, so age is the member's own guard.
pub(crate) const TOUCH_MAX_AGE_NS: u64 = 10_000_000_000;
/// ns per hour — the funding rate's period.
const HOUR_NS: i128 = 3_600_000_000_000;

/// One Hyperliquid book's top of book as the member keeps it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoinTouch {
    /// Best bid, USD × 1e6.
    pub bid_px_1e6: i64,
    /// Size at the best bid, coin units × 1e6.
    pub bid_qty_1e6: i64,
    /// Best ask, USD × 1e6.
    pub ask_px_1e6: i64,
    /// Size at the best ask, coin units × 1e6.
    pub ask_qty_1e6: i64,
    /// When it arrived (the engine's clock).
    pub ts_ns: NsTs,
    /// The ingress judged the tick stale (VT4): recorded, never traded.
    pub stale: bool,
}

impl CoinTouch {
    /// No book yet.
    pub const EMPTY: Self = Self {
        bid_px_1e6: 0,
        bid_qty_1e6: 0,
        ask_px_1e6: 0,
        ask_qty_1e6: 0,
        ts_ns: 0,
        stale: true,
    };

    /// The touch a BBO tick carries.
    #[inline]
    #[must_use]
    pub const fn of(tick: &Tick) -> Self {
        Self {
            bid_px_1e6: tick.bid_px.raw(),
            bid_qty_1e6: tick.bid_qty.raw(),
            ask_px_1e6: tick.ask_px.raw(),
            ask_qty_1e6: tick.ask_qty.raw(),
            ts_ns: tick.ts_ns,
            stale: tick.is_stale(),
        }
    }

    /// Same prices, sizes and staleness (the arrival time aside): a tick
    /// that changes nothing re-evaluates nothing.
    #[inline]
    #[must_use]
    pub const fn same_quote(&self, other: &Self) -> bool {
        self.bid_px_1e6 == other.bid_px_1e6
            && self.bid_qty_1e6 == other.bid_qty_1e6
            && self.ask_px_1e6 == other.ask_px_1e6
            && self.ask_qty_1e6 == other.ask_qty_1e6
            && self.stale == other.stale
    }

    /// Two-sided, positive, uncrossed and not stale.
    #[inline]
    #[must_use]
    pub const fn is_sound(&self) -> bool {
        !self.stale
            && self.bid_px_1e6 > 0
            && self.ask_px_1e6 > self.bid_px_1e6
            && self.bid_qty_1e6 > 0
            && self.ask_qty_1e6 > 0
    }

    /// Sound and no older than [`TOUCH_MAX_AGE_NS`] at `now`.
    #[inline]
    #[must_use]
    pub const fn is_usable(&self, now: NsTs) -> bool {
        self.is_sound() && now.saturating_sub(self.ts_ns) <= TOUCH_MAX_AGE_NS
    }

    /// Mid, USD × 1e6 — `None` unless [`Self::is_sound`].
    #[inline]
    #[must_use]
    pub const fn mid_1e6(&self) -> Option<i64> {
        if self.is_sound() {
            Some(self.bid_px_1e6 + (self.ask_px_1e6 - self.bid_px_1e6) / 2)
        } else {
            None
        }
    }
}

/// Which hedge venue the operator allows (`hedge_venue` in `hyparb.toml`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HedgeMode {
    /// The cheaper book, under hysteresis.
    Auto = 0,
    /// The perp only (an A/B leg).
    Perp = 1,
    /// The spot pair only (an A/B leg).
    Spot = 2,
}

/// The venue a hedge goes to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HedgeVenue {
    /// The coin's perp.
    Perp = 0,
    /// The coin's spot pair.
    Spot = 1,
    /// No hedge (the USD side, or no usable book).
    None = 255,
}

/// One venue's candidacy: its total cost and the leg it would trade.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VenueCost {
    /// The venue.
    pub venue: HedgeVenue,
    /// Total cost, bps × 1e6, signed (funding can pay us).
    pub cost_bps_1e6: i64,
}

/// The selector (plan §8.5): `mode` forces a venue; `Auto` takes the
/// cheaper, but keeps `last` unless the other is cheaper by MORE than
/// `hysteresis_bps_1e6` (the choice does not flap tick to tick). A tie
/// with no previous choice goes to the perp. `None` when the allowed
/// venue has no candidate.
#[inline]
#[must_use]
pub fn choose_venue(
    mode: HedgeMode,
    perp: Option<VenueCost>,
    spot: Option<VenueCost>,
    last: HedgeVenue,
    hysteresis_bps_1e6: i64,
) -> Option<VenueCost> {
    match mode {
        HedgeMode::Perp => perp,
        HedgeMode::Spot => spot,
        HedgeMode::Auto => match (perp, spot) {
            (Some(p), Some(s)) => {
                let (keep, other) = match last {
                    HedgeVenue::Perp => (p, s),
                    HedgeVenue::Spot => (s, p),
                    HedgeVenue::None => {
                        return Some(if s.cost_bps_1e6 < p.cost_bps_1e6 {
                            s
                        } else {
                            p
                        });
                    }
                };
                if other.cost_bps_1e6 < keep.cost_bps_1e6.saturating_sub(hysteresis_bps_1e6) {
                    Some(other)
                } else {
                    Some(keep)
                }
            }
            (Some(p), None) => Some(p),
            (None, s) => s,
        },
    }
}

/// `a × b / d`, rounded UP, for non-negative operands; saturating.
#[inline]
fn mul_div_up_i64(a: i64, b: i64, d: i64) -> i64 {
    if d <= 0 {
        return 0;
    }
    let n = (a as i128) * (b as i128);
    let v = (n + (d as i128) - 1) / (d as i128);
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// The leg one touch offers a side: the fee-folded price, the touch
/// price, its displayed depth in USD and its cost before the perp-only
/// terms. `None` unless the touch is usable.
#[inline]
fn leg_on(
    t: &CoinTouch,
    now: NsTs,
    sell: bool,
    taker_bps_1e6: i64,
    notional_usd_1e6: i64,
) -> Option<(HedgeLeg, i64)> {
    if !t.is_usable(now) {
        return None;
    }
    let mid = t.mid_1e6()?;
    let (px, qty) = if sell {
        (t.bid_px_1e6, t.bid_qty_1e6)
    } else {
        (t.ask_px_1e6, t.ask_qty_1e6)
    };
    // A sale receives less than the touch, a purchase pays more: the fee
    // is folded against us, and rounds against us.
    let eff = if sell {
        mul_div_i64(px, ONE_BPS_1E6 - taker_bps_1e6, ONE_BPS_1E6)
    } else {
        mul_div_up_i64(px, ONE_BPS_1E6 + taker_bps_1e6, ONE_BPS_1E6)
    };
    let depth = mul_div_i64(qty, px, E6);
    let spread = t.ask_px_1e6 - t.bid_px_1e6;
    let half_spread_bps = mul_div_up_i64(spread, ONE_BPS_1E6, 2 * mid);
    let penalty = if notional_usd_1e6 > depth && notional_usd_1e6 > 0 {
        mul_div_up_i64(
            notional_usd_1e6 - depth,
            2 * half_spread_bps,
            notional_usd_1e6,
        )
    } else {
        0
    };
    if eff <= 0 {
        return None;
    }
    let leg = HedgeLeg {
        eff_px_1e6: eff,
        px_1e6: px,
        venue: HedgeVenue::None,
        depth_usd_1e6: depth,
    };
    Some((leg, half_spread_bps + taker_bps_1e6 + penalty))
}

impl HedgeLeg {
    /// The USD side of a pool: worth exactly one, nothing to trade, no
    /// depth bound.
    pub(crate) const USD: Self = Self {
        eff_px_1e6: E6,
        px_1e6: 0,
        venue: HedgeVenue::None,
        depth_usd_1e6: i64::MAX,
    };
}

impl HyparbStrategy {
    /// Expected funding over the hold, bps × 1e6, signed for OUR side of
    /// the perp: selling (short) earns a positive rate.
    #[inline]
    fn funding_bps_1e6(&self, c: usize, sell: bool) -> i64 {
        // rate × 1e9 per hour → bps × 1e6 over the window:
        // r/1e9 · (w / hour) · 1e4 · 1e6 = r · w · 10 / hour.
        let f = (self.coins[c].funding_1e9 as i128)
            .saturating_mul(self.params.funding_window_ns as i128)
            .saturating_mul(10)
            / HOUR_NS;
        let f = f.clamp(-(ONE_BPS_1E6 as i128), ONE_BPS_1E6 as i128) as i64;
        if sell {
            f
        } else {
            -f
        }
    }

    /// The hedge leg for coin `coin` on side `sell` (we SELL the coin when
    /// `true`) sized `notional_usd_1e6`, on the venue the selector picks.
    /// The USD numéraire needs no leg ([`HedgeLeg::USD`]). `None` when no
    /// allowed book is usable. Records the choice for the hysteresis.
    pub(crate) fn hedge_side(
        &mut self,
        coin: u8,
        sell: bool,
        notional_usd_1e6: i64,
        now: NsTs,
    ) -> Option<HedgeLeg> {
        if coin == COIN_USD {
            return Some(HedgeLeg::USD);
        }
        let c = coin as usize;
        if c >= self.params.n_coins {
            return None;
        }
        let k = self.params.coins[c];
        let run = self.coins[c];
        let spot = if k.spot_sym == SYMBOL_ID_NONE {
            None
        } else {
            leg_on(
                &run.spot,
                now,
                sell,
                self.params.spot_taker_bps_1e6,
                notional_usd_1e6,
            )
        };
        let perp = if k.perp_sym == SYMBOL_ID_NONE {
            None
        } else {
            match leg_on(
                &run.perp,
                now,
                sell,
                self.params.perp_taker_bps_1e6,
                notional_usd_1e6,
            ) {
                Some((leg, cost)) => {
                    // The perp's basis to the spot book, when there is one.
                    let basis = match (run.perp.mid_1e6(), run.spot.mid_1e6()) {
                        (Some(pm), Some(sm)) if run.spot.is_usable(now) => {
                            mul_div_i64((pm - sm).abs(), ONE_BPS_1E6, sm)
                        }
                        _ => 0,
                    };
                    Some((leg, cost + basis - self.funding_bps_1e6(c, sell)))
                }
                None => None,
            }
        };
        let perp_cost = perp.map(|(_, cost)| VenueCost {
            venue: HedgeVenue::Perp,
            cost_bps_1e6: cost,
        });
        let spot_cost = spot.map(|(_, cost)| VenueCost {
            venue: HedgeVenue::Spot,
            cost_bps_1e6: cost,
        });
        let side = usize::from(!sell);
        let pick = choose_venue(
            self.params.hedge_mode,
            perp_cost,
            spot_cost,
            run.last_venue[side],
            self.params.hedge_switch_hysteresis_bps_1e6,
        )?;
        self.coins[c].last_venue[side] = pick.venue;
        let (mut leg, _) = match pick.venue {
            HedgeVenue::Perp => perp?,
            HedgeVenue::Spot => spot?,
            HedgeVenue::None => return None,
        };
        leg.venue = pick.venue;
        Some(leg)
    }
}
