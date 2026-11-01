// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! A deterministic synthetic market (the UI-side twin of the server's
//! `--synthetic <seed>`, plan v2.1 §4.6), so the spike runs with no server,
//! network or capture. Code, not data. Fixed-capacity outputs, no allocation
//! after `new`.

use term_proto::Bar;

/// xorshift64*: deterministic, tiny, good enough for a picture.
#[derive(Copy, Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Rng {
        Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    #[inline]
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Roughly N(0, 1) (Irwin–Hall, 4 terms).
    #[inline]
    pub fn gauss(&mut self) -> f32 {
        (self.unit() + self.unit() + self.unit() + self.unit() - 2.0) * 1.732
    }
}

/// Tick size ×1e6 for the synthetic instrument (0.1).
pub const TICK_1E6: i64 = 100_000;

pub struct Market {
    rng: Rng,
    pub mid_tick: i64,
    pub bar: i64,
    vol_ticks: f32,
}

/// One print for a bubble: tick, size ×1e6, aggressor buy?
#[derive(Copy, Clone, Debug, Default)]
pub struct Print {
    pub bar: i64,
    pub tick: i64,
    pub size_1e6: i64,
    pub buy: bool,
}

impl Market {
    /// Starts near 67 000.0 at a 0.1 tick, on a bar index far from zero.
    pub fn new(seed: u64) -> Market {
        Market { rng: Rng::new(seed), mid_tick: 670_000, bar: 29_500_000, vol_ticks: 18.0 }
    }

    /// Next 1-minute bar (×1e6 prices on the tick grid).
    pub fn next_bar(&mut self) -> Bar {
        let o = self.mid_tick;
        let mut hi = o;
        let mut lo = o;
        let mut p = o;
        for _ in 0..8 {
            p += (self.rng.gauss() * self.vol_ticks * 0.35) as i64;
            hi = hi.max(p);
            lo = lo.min(p);
        }
        // Slow regime drift in volatility.
        self.vol_ticks = (self.vol_ticks * (1.0 + 0.05 * self.rng.gauss())).clamp(4.0, 80.0);
        self.mid_tick = p;
        let vol = ((0.2 + self.rng.unit() * 3.0) * 1e6) as i64 * (hi - lo + 1);
        let delta = (p - o).signum() * vol / 3 + ((self.rng.gauss() * 0.1) * vol as f32) as i64;
        let b = Bar {
            bar: self.bar,
            tf_s: 60,
            o: o * TICK_1E6,
            h: hi * TICK_1E6,
            l: lo * TICK_1E6,
            c: p * TICK_1E6,
            vol,
            delta,
        };
        self.bar += 1;
        b
    }

    /// One heat column: log-quantized depth over `out.len()` rows around the
    /// mid, row 0 = `base_tick`. Returns the column's base tick.
    pub fn heat_col(&mut self, rows_tick_scale: i64, out: &mut [u8]) -> i64 {
        let rows = out.len() as i64;
        self.mid_tick += (self.rng.gauss() * 2.0) as i64;
        let base = self.mid_tick - rows / 2 * rows_tick_scale;
        let walls = [
            (self.mid_tick + 37, 0.9f32),
            (self.mid_tick - 52, 0.85),
            (self.mid_tick + 140, 0.7),
            (self.mid_tick - 211, 0.75),
        ];
        for (r, cell) in out.iter_mut().enumerate() {
            let t = base + r as i64 * rows_tick_scale;
            let d = (t - self.mid_tick).abs() as f32;
            let mut v = 0.55 * (-d / 140.0).exp() + 0.08 * self.rng.unit();
            for (wt, w) in walls {
                let dd = (t - wt).abs() as f32;
                if dd < 3.0 {
                    v = v.max(w * (1.0 - dd / 3.0));
                }
            }
            if d < 1.0 {
                v = 0.0; // the spread
            }
            *cell = (v.clamp(0.0, 1.0) * 255.0) as u8;
        }
        base
    }

    /// Prints for one bar between its low and high.
    pub fn prints(&mut self, b: &Bar, out: &mut [Print]) -> usize {
        let lo = b.l / TICK_1E6;
        let hi = b.h / TICK_1E6;
        let n = 1 + (self.rng.unit() * out.len() as f32) as usize;
        let n = n.min(out.len());
        for p in out.iter_mut().take(n) {
            let t = lo + ((hi - lo + 1) as f32 * self.rng.unit()) as i64;
            let size = (self.rng.unit().powi(4) * 25.0e6) as i64 + 10_000;
            *p = Print { bar: b.bar, tick: t, size_1e6: size, buy: self.rng.unit() < 0.5 };
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Market::new(7);
        let mut b = Market::new(7);
        for _ in 0..500 {
            assert_eq!(a.next_bar(), b.next_bar());
        }
    }

    #[test]
    fn bars_are_well_formed_on_the_grid() {
        let mut m = Market::new(42);
        for _ in 0..2000 {
            let b = m.next_bar();
            assert!(b.l <= b.o && b.o <= b.h && b.l <= b.c && b.c <= b.h);
            for v in [b.o, b.h, b.l, b.c] {
                assert_eq!(v % TICK_1E6, 0);
            }
            assert!(b.vol > 0);
        }
    }

    #[test]
    fn heat_col_has_a_spread_and_walls() {
        let mut m = Market::new(3);
        let mut col = [0u8; 1024];
        let base = m.heat_col(1, &mut col);
        let mid_row = (m.mid_tick - base) as usize;
        assert_eq!(col[mid_row], 0);
        assert!(col.iter().any(|&c| c > 200));
    }
}
