// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
// Pipeline 1: instanced rects (candle bodies, wicks, histograms, ladder bars).
// Instances carry whole-bar / whole-tick f32 offsets from the view's i64 origin.
struct View { ax: f32, bx: f32, ay: f32, by: f32, w: f32, h: f32, p0: f32, p1: f32 };
@group(0) @binding(0) var<uniform> v: View;

struct Inst {
  @location(0) x: f32,      // bar offset of the centre
  @location(1) hw: f32,     // half width in bars
  @location(2) y0: f32,     // tick offset
  @location(3) y1: f32,     // tick offset
  @location(4) min_px: f32, // minimum half width in px (wicks)
  @location(5) color: vec4<f32>,
};
struct Out { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
  let c = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
  let cx = i.x * v.ax + v.bx;
  let hw = max(abs(i.hw * v.ax), i.min_px * 2.0 / v.w);
  let ya = i.y0 * v.ay + v.by;
  let yb = i.y1 * v.ay + v.by;
  let ym = 0.5 * (ya + yb);
  let hh = max(0.5 * abs(yb - ya), 1.0 / v.h); // at least 1 px tall
  var o: Out;
  o.pos = vec4<f32>(mix(cx - hw, cx + hw, c.x), mix(ym - hh, ym + hh, c.y), 0.0, 1.0);
  o.color = i.color;
  return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
  return vec4<f32>(i.color.rgb * i.color.a, i.color.a);
}
