// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Chart scales (plan v2.1 §4.11.4 "Precision", §4.11.5).
//!
//! The GPU never sees an absolute price or time. A view keeps, on the CPU,
//! an origin in whole bars and whole ticks (i64). Vertices carry f32 offsets
//! from that origin in whole bars / whole ticks, exact while |offset| < 2^24.
//! The view's sub-unit position lives in the uniform (`ViewUniform`), and the
//! origin re-bases once the view drifts more than `REBASE` units from it, so
//! offsets stay small and sub-tick panning never jitters.

/// Re-base when the view's edge is more than 2^16 units from the origin.
pub const REBASE: i64 = 1 << 16;

/// A position in whole units plus a fraction in [0, 1).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Pos {
    pub whole: i64,
    pub frac: f32,
}

impl Pos {
    pub const fn new(whole: i64) -> Pos {
        Pos { whole, frac: 0.0 }
    }
    /// Add a (small) f32 delta, renormalising the fraction into [0, 1).
    pub fn shift(self, d: f32) -> Pos {
        let t = self.frac + d;
        let fl = t.floor();
        let mut frac = t - fl;
        let mut whole = self.whole + fl as i64;
        if frac >= 1.0 {
            // f32 rounding of t - floor(t) can land exactly on 1.0.
            frac = 0.0;
            whole += 1;
        }
        Pos { whole, frac }
    }
    /// Offset of this position from an integer origin, as f32 (small by
    /// construction after re-basing).
    #[inline]
    pub fn rel(self, origin: i64) -> f32 {
        (self.whole - origin) as f32 + self.frac
    }
}

/// The NDC mapping handed to every pipeline: ndc = off * a + b.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ViewUniform {
    pub ax: f32,
    pub bx: f32,
    pub ay: f32,
    pub by: f32,
    /// Pane size in physical pixels (for pixel-sized widths).
    pub w_px: f32,
    pub h_px: f32,
    pub _pad: [f32; 2],
}

/// One pane's time + price scale.
#[derive(Copy, Clone, Debug)]
pub struct Viewport {
    /// Integer origin for vertex offsets.
    pub origin_bar: i64,
    pub origin_tick: i64,
    /// Left edge of the pane, in bars.
    pub left: Pos,
    /// Top edge of the pane, in ticks.
    pub top: Pos,
    pub px_per_bar: f32,
    pub px_per_tick: f32,
}

impl Viewport {
    pub fn new(left_bar: i64, top_tick: i64, px_per_bar: f32, px_per_tick: f32) -> Viewport {
        Viewport {
            origin_bar: left_bar,
            origin_tick: top_tick,
            left: Pos::new(left_bar),
            top: Pos::new(top_tick),
            px_per_bar,
            px_per_tick,
        }
    }

    /// Re-base the origin onto the current edges if they drifted too far.
    /// Returns true when the origin moved (vertex offsets must be rebuilt).
    pub fn rebase_if_needed(&mut self) -> bool {
        let dx = (self.left.whole - self.origin_bar).abs();
        let dy = (self.top.whole - self.origin_tick).abs();
        if dx > REBASE || dy > REBASE {
            self.origin_bar = self.left.whole;
            self.origin_tick = self.top.whole;
            true
        } else {
            false
        }
    }

    /// Pan by pixels (positive dx moves the content right = earlier bars).
    pub fn pan_px(&mut self, dx: f32, dy: f32) {
        self.left = self.left.shift(-dx / self.px_per_bar);
        self.top = self.top.shift(dy / self.px_per_tick);
    }

    /// Zoom the time axis by `k` keeping the bar under `anchor_px` fixed.
    pub fn zoom_x_at(&mut self, anchor_px: f32, k: f32) {
        let k = k.clamp(0.05, 20.0);
        let new_ppb = (self.px_per_bar * k).clamp(0.25, 400.0);
        let bars_before = anchor_px / self.px_per_bar;
        let bars_after = anchor_px / new_ppb;
        self.left = self.left.shift(bars_before - bars_after);
        self.px_per_bar = new_ppb;
    }

    /// Zoom the price axis by `k` keeping the tick under `anchor_px` fixed.
    pub fn zoom_y_at(&mut self, anchor_px: f32, k: f32) {
        let k = k.clamp(0.05, 20.0);
        let new_ppt = (self.px_per_tick * k).clamp(1e-4, 400.0);
        let t_before = anchor_px / self.px_per_tick;
        let t_after = anchor_px / new_ppt;
        self.top = self.top.shift(t_after - t_before);
        self.px_per_tick = new_ppt;
    }

    /// Fit the price scale so ticks [lo, hi] fill `h_px` with a margin.
    pub fn fit_y(&mut self, lo: i64, hi: i64, h_px: f32, margin: f32) {
        let span = (hi - lo).max(1) as f32;
        let pad = span * margin;
        self.px_per_tick = h_px / (span + 2.0 * pad);
        self.top = Pos::new(hi).shift(pad);
    }

    pub fn uniform(&self, w_px: f32, h_px: f32) -> ViewUniform {
        // ndc_x = ((off - left_rel) * ppb) / w * 2 - 1
        let ax = 2.0 * self.px_per_bar / w_px;
        let bx = -1.0 - self.left.rel(self.origin_bar) * ax;
        // ndc_y = 1 - ((top_rel - off) * ppt) / h * 2
        let ay = 2.0 * self.px_per_tick / h_px;
        let by = 1.0 - self.top.rel(self.origin_tick) * ay;
        ViewUniform { ax, bx, ay, by, w_px, h_px, _pad: [0.0; 2] }
    }

    /// Vertex offsets for an absolute bar / tick.
    #[inline]
    pub fn off_bar(&self, bar: i64) -> f32 {
        (bar - self.origin_bar) as f32
    }
    #[inline]
    pub fn off_tick(&self, tick: i64) -> f32 {
        (tick - self.origin_tick) as f32
    }

    /// Visible bar range [first, last] for a pane `w_px` wide.
    pub fn visible_bars(&self, w_px: f32) -> (i64, i64) {
        let first = self.left.whole;
        let last = self.left.shift(w_px / self.px_per_bar).whole + 1;
        (first, last)
    }
    /// Visible tick range [lo, hi] for a pane `h_px` tall.
    pub fn visible_ticks(&self, h_px: f32) -> (i64, i64) {
        let hi = self.top.whole + 1;
        let lo = self.top.shift(-h_px / self.px_per_tick).whole;
        (lo, hi)
    }

    /// Pixel y (from the pane's top) of an absolute tick, for labels.
    pub fn y_px(&self, tick: i64) -> f32 {
        (self.top.rel(self.origin_tick) - self.off_tick(tick)) * self.px_per_tick
    }
    /// Pixel x (from the pane's left) of a bar's centre, for labels.
    pub fn x_px(&self, bar: i64) -> f32 {
        (self.off_bar(bar) + 0.5 - self.left.rel(self.origin_bar)) * self.px_per_bar
    }
}

/// A 1-2-5 × 10^k step (in ticks) giving roughly `target` lines over `span`.
pub fn nice_step(span_ticks: i64, target: i64) -> i64 {
    let raw = (span_ticks.max(1) / target.max(1)).max(1);
    let mut p = 1i64;
    while p.saturating_mul(10) <= raw {
        p = p.saturating_mul(10);
    }
    for m in [1i64, 2, 5, 10] {
        let s = p.saturating_mul(m);
        if s >= raw {
            return s;
        }
    }
    p.saturating_mul(10)
}

/// Grid lines (multiples of `step`) inside [lo, hi], written into `out`.
/// Returns the count; never allocates.
pub fn grid(lo: i64, hi: i64, step: i64, out: &mut [i64]) -> usize {
    let step = step.max(1);
    let mut t = lo.div_euclid(step) * step;
    if t < lo {
        t += step;
    }
    let mut n = 0;
    while t <= hi && n < out.len() {
        out[n] = t;
        n += 1;
        t += step;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_add_normalises() {
        let p = Pos::new(10).shift(2.75).shift(-0.5);
        assert_eq!(p.whole, 12);
        assert!((p.frac - 0.25).abs() < 1e-6);
        let q = Pos::new(0).shift(-0.25);
        assert_eq!(q.whole, -1);
        assert!((q.frac - 0.75).abs() < 1e-6);
        for d in [1e-9f32, -1e-9, 0.999_999_9, -0.999_999_9] {
            let r = Pos::new(5).shift(d);
            assert!((0.0..1.0).contains(&r.frac), "{d}: {r:?}");
        }
    }

    /// A BTC ×1e6 price at a 0.1 tick is ~1e6 ticks; a ns time is ~1.8e18.
    /// Neither may reach the GPU: only small offsets do, and they are exact.
    #[test]
    fn offsets_exact_far_from_zero() {
        let big_tick = 67_123_400_000_000i64 / 100_000; // 67 123.4 at a 0.1 tick
        let mut v = Viewport::new(1_000_000_000, big_tick, 8.0, 2.0);
        for i in 0..2000 {
            v.pan_px(37.3, -11.1);
            v.rebase_if_needed();
            let off = v.off_tick(v.top.whole);
            assert!(off.abs() <= (REBASE + 1) as f32, "iter {i}");
            assert_eq!(off as i64 + v.origin_tick, v.top.whole);
        }
    }

    #[test]
    fn uniform_maps_edges_to_ndc() {
        let mut v = Viewport::new(500, 9000, 10.0, 4.0);
        v.pan_px(-33.0, 17.0);
        let u = v.uniform(800.0, 400.0);
        let left_off = v.left.rel(v.origin_bar);
        let top_off = v.top.rel(v.origin_tick);
        assert!((left_off * u.ax + u.bx + 1.0).abs() < 1e-5);
        assert!((top_off * u.ay + u.by - 1.0).abs() < 1e-5);
        let right_off = left_off + 800.0 / v.px_per_bar;
        assert!((right_off * u.ax + u.bx - 1.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_keeps_anchor() {
        let mut v = Viewport::new(0, 0, 10.0, 1.0);
        let anchor = 400.0;
        let bar_at = |v: &Viewport| v.left.rel(0) + anchor / v.px_per_bar;
        let b0 = bar_at(&v);
        v.zoom_x_at(anchor, 1.7);
        assert!((bar_at(&v) - b0).abs() < 1e-3);
        let tick_at = |v: &Viewport| v.top.rel(0) - 300.0 / v.px_per_tick;
        let t0 = tick_at(&v);
        v.zoom_y_at(300.0, 0.6);
        assert!((tick_at(&v) - t0).abs() < 1e-3);
    }

    #[test]
    fn nice_steps_and_grid() {
        assert_eq!(nice_step(1000, 10), 100);
        assert_eq!(nice_step(1300, 10), 200);
        assert_eq!(nice_step(4000, 10), 500);
        assert_eq!(nice_step(3, 10), 1);
        let mut g = [0i64; 16];
        let n = grid(-7, 23, 10, &mut g);
        assert_eq!(&g[..n], &[0, 10, 20]);
        let n = grid(0, 1_000_000, 1, &mut g);
        assert_eq!(n, 16, "bounded by the output slice");
    }
}
