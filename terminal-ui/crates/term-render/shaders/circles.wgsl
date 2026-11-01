// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
// Pipeline 3: SDF circles and rings (trade bubbles, liquidation rings).
struct View { ax: f32, bx: f32, ay: f32, by: f32, w: f32, h: f32, p0: f32, p1: f32 };
@group(0) @binding(0) var<uniform> v: View;

struct Inst {
  @location(0) x: f32,      // bar offset
  @location(1) y: f32,      // tick offset
  @location(2) r_px: f32,   // radius in px (area ∝ size, computed on the CPU)
  @location(3) ring: f32,   // 0 = disc, >0 = ring thickness as a fraction of r
  @location(4) color: vec4<f32>,
};
struct Out {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
  @location(2) ring: f32,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
  let c = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u)) * 2.0 - 1.0;
  let ctr = vec2<f32>(i.x * v.ax + v.bx, i.y * v.ay + v.by);
  let r = (i.r_px + 1.0) * vec2<f32>(2.0 / v.w, 2.0 / v.h);
  var o: Out;
  o.pos = vec4<f32>(ctr + c * r, 0.0, 1.0);
  o.uv = c * (i.r_px + 1.0) / max(i.r_px, 0.5);
  o.color = i.color;
  o.ring = i.ring;
  return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
  let d = length(i.uv);
  let aa = max(fwidth(d), 1e-4);
  var a = 1.0 - smoothstep(1.0 - aa, 1.0 + aa, d);
  if (i.ring > 0.0) {
    a = a * smoothstep(1.0 - i.ring - aa, 1.0 - i.ring + aa, d);
  }
  let alpha = i.color.a * a;
  return vec4<f32>(i.color.rgb * alpha, alpha);
}
