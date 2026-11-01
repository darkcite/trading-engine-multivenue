// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
// Pipeline 2: polylines as instanced segments. The point buffer is bound four
// times at offsets i-1, i, i+1, i+2 with an instance step; the vertex shader
// builds the miter join from the neighbours. No storage buffer (WebGL2).
struct Line { ax: f32, bx: f32, ay: f32, by: f32, w: f32, h: f32, width_px: f32, p1: f32, color: vec4<f32> };
@group(0) @binding(0) var<uniform> u: Line;

struct Out { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };

fn to_px(p: vec2<f32>) -> vec2<f32> {
  let ndc = vec2<f32>(p.x * u.ax + u.bx, p.y * u.ay + u.by);
  return ndc * vec2<f32>(0.5 * u.w, 0.5 * u.h);
}
fn safe_dir(a: vec2<f32>, b: vec2<f32>, fallback: vec2<f32>) -> vec2<f32> {
  let d = b - a;
  let l = length(d);
  if (l < 1e-4) { return fallback; }
  return d / l;
}
// Offset (in px) of the joint at `p` between incoming `din` and outgoing `dout`.
fn miter(din: vec2<f32>, dout: vec2<f32>, seg_n: vec2<f32>, hw: f32) -> vec2<f32> {
  let t = normalize(din + dout + vec2<f32>(1e-6, 0.0));
  let m = vec2<f32>(-t.y, t.x);
  let k = dot(m, seg_n);
  let len = hw / max(abs(k), 0.25); // miter limit 4x → bevel-like clamp
  return m * len * sign(k + 1e-6);
}

@vertex
fn vs(@builtin(vertex_index) vi: u32,
      @location(0) a0: vec2<f32>, @location(1) a1: vec2<f32>,
      @location(2) a2: vec2<f32>, @location(3) a3: vec2<f32>) -> Out {
  let p0 = to_px(a0);
  let p1 = to_px(a1);
  let p2 = to_px(a2);
  let p3 = to_px(a3);
  let d = safe_dir(p1, p2, vec2<f32>(1.0, 0.0));
  let din = safe_dir(p0, p1, d);
  let dout = safe_dir(p2, p3, d);
  let n = vec2<f32>(-d.y, d.x);
  let hw = 0.5 * u.width_px;
  let end = (vi >> 1u) & 1u;      // 0 at p1, 1 at p2
  let side = select(-1.0, 1.0, (vi & 1u) == 1u);
  var base = p1;
  var off = miter(din, d, n, hw);
  if (end == 1u) { base = p2; off = miter(d, dout, n, hw); }
  let px = base + side * off;
  var o: Out;
  o.pos = vec4<f32>(px / vec2<f32>(0.5 * u.w, 0.5 * u.h), 0.0, 1.0);
  o.color = u.color;
  return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
  return vec4<f32>(i.color.rgb * i.color.a, i.color.a);
}
