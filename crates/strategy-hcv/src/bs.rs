// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Black–Scholes with zero rates, its greeks, and the implied vol —
//! the member's option math (f64; no allocation; a few hundred ns).
//!
//! Hypercall options are European, cash-settled, USD premium per
//! 1-unit contract; funding and carry are ignored at the 1–40 day tenors
//! traded (r = q = 0 — the HAR forecast is a pure diffusive σ̂ and the
//! gap it is compared with must be too).
//!
//! The cumulative normal is Hart's double-precision algorithm in West's
//! form ("Better approximations to cumulative normal functions", 2005):
//! absolute error below 1e-14 everywhere — far under a premium's last
//! quoted digit.

/// `1 / √(2π)`.
const INV_SQRT_2PI: f64 = 0.398_942_280_401_432_7;

/// The standard normal density.
#[inline]
#[must_use]
pub fn npdf(x: f64) -> f64 {
    INV_SQRT_2PI * (-0.5 * x * x).exp()
}

/// The standard normal cumulative distribution (Hart / West).
#[must_use]
pub fn ncdf(x: f64) -> f64 {
    let xa = x.abs();
    let c = if xa > 37.0 {
        0.0
    } else {
        let e = (-xa * xa / 2.0).exp();
        if xa < 7.071_067_811_865_475 {
            let mut b = 3.526_249_659_989_11e-2 * xa + 0.700_383_064_443_688;
            b = b * xa + 6.373_962_203_531_65;
            b = b * xa + 33.912_866_078_383;
            b = b * xa + 112.079_291_497_871;
            b = b * xa + 221.213_596_169_931;
            b = b * xa + 220.206_867_912_376;
            let mut d = 8.838_834_764_831_84e-2 * xa + 1.755_667_163_182_64;
            d = d * xa + 16.064_177_579_207;
            d = d * xa + 86.780_732_202_946_1;
            d = d * xa + 296.564_248_779_674;
            d = d * xa + 637.333_633_378_831;
            d = d * xa + 793.826_512_519_948;
            d = d * xa + 440.413_735_824_752;
            e * b / d
        } else {
            let mut b = xa + 0.65;
            b = xa + 4.0 / b;
            b = xa + 3.0 / b;
            b = xa + 2.0 / b;
            b = xa + 1.0 / b;
            e / b / 2.506_628_274_631
        }
    };
    if x > 0.0 {
        1.0 - c
    } else {
        c
    }
}

/// One option's price and greeks at `(s, k, tau_y, sigma)`.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Greeks {
    /// The premium, USD per contract.
    pub price: f64,
    /// ∂price/∂S (a put's is negative).
    pub delta: f64,
    /// ∂price/∂σ per ONE vol point (0.01), USD per contract.
    pub vega_pt: f64,
}

/// Black–Scholes (r = 0). `None` off the domain (`s, k, tau, sigma` must
/// be finite and positive).
#[must_use]
pub fn price(call: bool, s: f64, k: f64, tau_y: f64, sigma: f64) -> Option<Greeks> {
    if !(s > 0.0 && k > 0.0 && tau_y > 0.0 && sigma > 0.0)
        || !(s.is_finite() && k.is_finite() && tau_y.is_finite() && sigma.is_finite())
    {
        return None;
    }
    let sq = tau_y.sqrt();
    let v = sigma * sq;
    let d1 = ((s / k).ln() + 0.5 * v * v) / v;
    let d2 = d1 - v;
    let (p, delta) = if call {
        (s * ncdf(d1) - k * ncdf(d2), ncdf(d1))
    } else {
        (k * ncdf(-d2) - s * ncdf(-d1), ncdf(d1) - 1.0)
    };
    Some(Greeks {
        price: p.max(0.0),
        delta,
        vega_pt: s * npdf(d1) * sq * 0.01,
    })
}

/// The implied vol of `premium`: bracketed Newton in `[0.005, 5.0]`,
/// premium tolerance 1e-12 × S (a far wing's vega is tiny: a looser
/// premium tolerance is a loose vol there), or the bracket narrowed to
/// 1e-12. `None` when the premium is outside the no-arbitrage band (below
/// intrinsic, above the forward/strike bound) or no root is found.
#[must_use]
pub fn implied_vol(call: bool, s: f64, k: f64, tau_y: f64, premium: f64) -> Option<f64> {
    if !(s > 0.0 && k > 0.0 && tau_y > 0.0 && premium > 0.0) {
        return None;
    }
    let intrinsic = if call { (s - k).max(0.0) } else { (k - s).max(0.0) };
    let upper = if call { s } else { k };
    if premium <= intrinsic || premium >= upper {
        return None;
    }
    let tol = 1e-12 * s;
    let (mut lo, mut hi) = (0.005f64, 5.0f64);
    let plo = price(call, s, k, tau_y, lo)?.price;
    let phi = price(call, s, k, tau_y, hi)?.price;
    if premium < plo || premium > phi {
        return None;
    }
    let mut sig = 0.5f64.clamp(lo, hi);
    let mut i = 0u32;
    while i < 100 {
        let g = price(call, s, k, tau_y, sig)?;
        let diff = g.price - premium;
        if diff.abs() <= tol {
            return Some(sig);
        }
        if diff > 0.0 {
            hi = sig;
        } else {
            lo = sig;
        }
        let vega = g.vega_pt * 100.0;
        let newton = sig - diff / vega;
        sig = if vega > 1e-12 && newton > lo && newton < hi {
            newton
        } else {
            0.5 * (lo + hi)
        };
        if hi - lo < 1e-12 {
            return Some(sig);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_normal_matches_its_known_values() {
        assert!((ncdf(0.0) - 0.5).abs() < 1e-15);
        assert!((ncdf(1.0) - 0.841_344_746_068_542_9).abs() < 1e-14);
        assert!((ncdf(-1.959_963_984_540_054) - 0.025).abs() < 1e-14);
        assert!((ncdf(8.0) - 0.999_999_999_999_999_4).abs() < 1e-14);
        assert!(ncdf(-40.0) == 0.0 && ncdf(40.0) == 1.0);
        let mut x = -10.0f64;
        while x < 10.0 {
            assert!((ncdf(x) + ncdf(-x) - 1.0).abs() < 1e-14, "{x}");
            x += 0.37;
        }
    }

    #[test]
    fn the_textbook_price_and_parity_hold() {
        // S = K = 100, 1 y, 20 %: the call is 7.9656 (r = 0).
        let c = price(true, 100.0, 100.0, 1.0, 0.2).unwrap();
        let p = price(false, 100.0, 100.0, 1.0, 0.2).unwrap();
        assert!((c.price - 7.965_567_455_405_804).abs() < 1e-9, "{}", c.price);
        assert!((c.price - p.price).abs() < 1e-9, "put–call parity at the money, r = 0");
        assert!((c.delta - p.delta - 1.0).abs() < 1e-12);
        assert!((c.vega_pt - 0.396_952_547_477_011_8).abs() < 1e-9, "{}", c.vega_pt);
        assert!(price(true, 0.0, 1.0, 1.0, 0.2).is_none());
        assert!(price(true, 1.0, 1.0, 1.0, f64::NAN).is_none());
    }

    #[test]
    fn implied_vol_round_trips_across_the_traded_surface() {
        let mut k = 90.0f64;
        while k <= 110.0 {
            for tau_d in [1.0f64, 3.0, 7.0, 21.0, 40.0] {
                for sig in [0.08f64, 0.25, 0.6, 1.4] {
                    for call in [true, false] {
                        let t = tau_d / 365.0;
                        let px = price(call, 100.0, k, t, sig).unwrap().price;
                        let intrinsic = if call { (100.0 - k).max(0.0) } else { (k - 100.0).max(0.0) };
                        if px - intrinsic < 1e-6 {
                            continue;
                        }
                        let iv = implied_vol(call, 100.0, k, t, px).expect("a root");
                        assert!((iv - sig).abs() < 1e-5, "k {k} tau {tau_d} sig {sig} call {call}: {iv}");
                    }
                }
            }
            k += 2.5;
        }
    }

    #[test]
    fn premiums_outside_the_band_have_no_vol() {
        assert!(implied_vol(true, 100.0, 90.0, 0.1, 9.99).is_none(), "below intrinsic");
        assert!(implied_vol(true, 100.0, 90.0, 0.1, 100.0).is_none(), "at the stock");
        assert!(implied_vol(false, 100.0, 110.0, 0.1, 110.0).is_none(), "at the strike");
        assert!(implied_vol(true, 100.0, 100.0, 0.0, 1.0).is_none());
    }
}
