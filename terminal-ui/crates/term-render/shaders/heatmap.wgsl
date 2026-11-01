// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
// Pipeline 4: the heatmap texture ring. R8Unorm cols × rows, one new column
// per write, the read offset wraps. Each column has its own base tick, held in
// an R32Sint cols × 1 texture (WebGL2 has no 1-D textures). Integer row lookup
// with textureLoad, so the GPU never sees an absolute price.
struct Heat {
  w: f32, h: f32,                // pane size in px
  col_px: f32,                   // px per column
  ticks_per_px: f32,
  top_rel: f32,                  // top edge in ticks, relative to origin_tick (frac included)
  contrast: f32, gamma: f32, p0: f32,
  origin_tick: i32,              // view origin, in the session tick frame
  head: i32,                     // ring index of the newest column
  filled: i32,                   // columns written so far (≤ cols)
  tick_scale: i32,               // ticks per row
};
@group(0) @binding(0) var<uniform> u: Heat;
@group(0) @binding(1) var heat: texture_2d<f32>;
@group(0) @binding(2) var base: texture_2d<i32>;
@group(0) @binding(3) var lut: texture_2d<f32>;

struct Out { @builtin(position) pos: vec4<f32>, @location(0) px: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> Out {
  let c = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
  var o: Out;
  o.pos = vec4<f32>(c.x * 2.0 - 1.0, 1.0 - c.y * 2.0, 0.0, 1.0);
  o.px = c * vec2<f32>(u.w, u.h);
  return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
  let dims = textureDimensions(heat);
  let cols = i32(dims.x);
  let rows = i32(dims.y);
  let age = i32(floor((u.w - i.px.x) / u.col_px));
  if (age < 0 || age >= u.filled) { return vec4<f32>(0.0); }
  let col = ((u.head - age) % cols + cols) % cols;
  let tick_rel = floor(u.top_rel - i.px.y * u.ticks_per_px);
  let tick = u.origin_tick + i32(tick_rel);
  let b = textureLoad(base, vec2<i32>(col, 0), 0).r;
  let d = tick - b;
  if (d < 0) { return vec4<f32>(0.0); }
  let row = d / u.tick_scale;
  if (row >= rows) { return vec4<f32>(0.0); }
  let raw = textureLoad(heat, vec2<i32>(col, row), 0).r;
  let x = clamp(pow(raw * u.contrast, u.gamma), 0.0, 1.0);
  let c = textureLoad(lut, vec2<i32>(i32(x * 255.0), 0), 0);
  return vec4<f32>(c.rgb * c.a, c.a);
}
