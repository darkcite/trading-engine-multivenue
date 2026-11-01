// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-render` — the terminal's wgpu pipelines (plan v2.1 §4.11.4).
//!
//! Four pipelines run inside egui through `egui_wgpu::CallbackTrait`:
//! instanced rects, instanced-segment polylines, SDF circles, and the heatmap
//! texture ring. Every buffer and texture is created once, sized to the view's
//! maximum, and updated in place with `write_buffer` / `write_texture`. No GPU
//! allocation happens per frame. Only the WebGL2 subset of wgpu is used: no
//! storage buffers, no storage textures, no compute, uniforms ≤ 16 KiB,
//! vertex strides ≤ 255 B, textures ≤ 8 192 a side.
//!
//! The GPU never sees an absolute price or time: instances carry whole-bar /
//! whole-tick f32 offsets from the view's i64 origin; the uniform holds the
//! sub-unit position (see `term-ui-core::chart`).

use bytemuck::{Pod, Zeroable};
use egui_wgpu::wgpu;
use egui_wgpu::{CallbackResources, CallbackTrait};

pub const MAX_PANES: usize = 4;
pub const MAX_RECTS: usize = 16_384;
pub const MAX_CIRCLES: usize = 4_096;
pub const MAX_LINES: usize = 8;
pub const MAX_LINE_PTS: usize = 8_192;
pub const HEAT_COLS: u32 = 3_000;
pub const HEAT_ROWS: u32 = 1_024;

// ---- instance / uniform layouts -----------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
pub struct ViewU {
    pub ax: f32,
    pub bx: f32,
    pub ay: f32,
    pub by: f32,
    pub w_px: f32,
    pub h_px: f32,
    pub _pad: [f32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
pub struct RectInst {
    pub x: f32,
    pub hw: f32,
    pub y0: f32,
    pub y1: f32,
    pub min_px: f32,
    pub rgba: [u8; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
pub struct CircleInst {
    pub x: f32,
    pub y: f32,
    pub r_px: f32,
    pub ring: f32,
    pub rgba: [u8; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
struct LineU {
    view: ViewU, // _pad[0] carries width_px
    color: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Pod, Zeroable)]
pub struct HeatU {
    pub w_px: f32,
    pub h_px: f32,
    pub col_px: f32,
    pub ticks_per_px: f32,
    pub top_rel: f32,
    pub contrast: f32,
    pub gamma: f32,
    pub _p0: f32,
    pub origin_tick: i32,
    pub head: i32,
    pub filled: i32,
    pub tick_scale: i32,
}

/// One series' style.
#[derive(Copy, Clone, Debug)]
pub struct LineStyle {
    pub width_px: f32,
    pub color: [f32; 4],
}

// Every layout must fit the WebGL2 downlevel limits.
const _: () = assert!(core::mem::size_of::<RectInst>() <= 255);
const _: () = assert!(core::mem::size_of::<CircleInst>() <= 255);
const _: () = assert!(core::mem::size_of::<LineU>() <= 16 * 1024);
const _: () = assert!(core::mem::size_of::<HeatU>() % 16 == 0);
const _: () = assert!(HEAT_COLS <= 8_192 && HEAT_ROWS <= 8_192);

// ---- GPU state -----------------------------------------------------------

struct LineGpu {
    pts: wgpu::Buffer,
    uni: wgpu::Buffer,
    bg: wgpu::BindGroup,
}

struct PaneGpu {
    view: wgpu::Buffer,
    view_bg: wgpu::BindGroup,
    rects: wgpu::Buffer,
    circles: wgpu::Buffer,
    lines: [LineGpu; MAX_LINES],
}

struct HeatGpu {
    heat: wgpu::Texture,
    base: wgpu::Texture,
    uni: wgpu::Buffer,
    bg: wgpu::BindGroup,
}

/// All pipelines and buffers; lives in egui's `CallbackResources`.
pub struct TermGpu {
    rect_pipe: wgpu::RenderPipeline,
    line_pipe: wgpu::RenderPipeline,
    circ_pipe: wgpu::RenderPipeline,
    heat_pipe: wgpu::RenderPipeline,
    panes: [PaneGpu; MAX_PANES],
    heat: HeatGpu,
}

fn buf(dev: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn uniform_entry() -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform_bgl(dev: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
    dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &[uniform_entry()] })
}

fn bind_uniform(dev: &wgpu::Device, bgl: &wgpu::BindGroupLayout, b: &wgpu::Buffer) -> wgpu::BindGroup {
    dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: bgl,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: b.as_entire_binding() }],
    })
}

fn tex_entry(binding: u32, sample: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: sample,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn pipeline(
    dev: &wgpu::Device,
    fmt: wgpu::TextureFormat,
    label: &str,
    src: &str,
    bgl: &wgpu::BindGroupLayout,
    buffers: &[wgpu::VertexBufferLayout<'_>],
) -> wgpu::RenderPipeline {
    let module = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[bgl],
        push_constant_ranges: &[],
    });
    dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers,
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: fmt,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    })
}

const RECT_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    0 => Float32, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Float32, 5 => Unorm8x4
];
const CIRC_ATTRS: [wgpu::VertexAttribute; 5] =
    wgpu::vertex_attr_array![0 => Float32, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Unorm8x4];
const PT_ATTRS: [[wgpu::VertexAttribute; 1]; 4] = [
    wgpu::vertex_attr_array![0 => Float32x2],
    wgpu::vertex_attr_array![1 => Float32x2],
    wgpu::vertex_attr_array![2 => Float32x2],
    wgpu::vertex_attr_array![3 => Float32x2],
];

fn make_tex(dev: &wgpu::Device, label: &str, w: u32, h: u32, f: wgpu::TextureFormat) -> wgpu::Texture {
    dev.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: f,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn copy_at(t: &wgpu::Texture, x: u32) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture: t,
        mip_level: 0,
        origin: wgpu::Origin3d { x, y: 0, z: 0 },
        aspect: wgpu::TextureAspect::All,
    }
}

impl TermGpu {
    /// Builds every pipeline, buffer and texture once.
    pub fn new(dev: &wgpu::Device, queue: &wgpu::Queue, fmt: wgpu::TextureFormat, lut: &[[u8; 4]; 256]) -> TermGpu {
        let view_bgl = uniform_bgl(dev, "term view");
        let line_bgl = uniform_bgl(dev, "term line");
        let heat_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("term heat"),
            entries: &[
                uniform_entry(),
                tex_entry(1, wgpu::TextureSampleType::Float { filterable: true }),
                tex_entry(2, wgpu::TextureSampleType::Sint),
                tex_entry(3, wgpu::TextureSampleType::Float { filterable: true }),
            ],
        });

        let rect_layout = [wgpu::VertexBufferLayout {
            array_stride: core::mem::size_of::<RectInst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &RECT_ATTRS,
        }];
        let circ_layout = [wgpu::VertexBufferLayout {
            array_stride: core::mem::size_of::<CircleInst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &CIRC_ATTRS,
        }];
        let pt_layout: [wgpu::VertexBufferLayout; 4] = core::array::from_fn(|k| wgpu::VertexBufferLayout {
            array_stride: 8,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &PT_ATTRS[k],
        });

        let rect_pipe = pipeline(dev, fmt, "term rects", include_str!("../shaders/rects.wgsl"), &view_bgl, &rect_layout);
        let line_pipe = pipeline(dev, fmt, "term polyline", include_str!("../shaders/polyline.wgsl"), &line_bgl, &pt_layout);
        let circ_pipe = pipeline(dev, fmt, "term circles", include_str!("../shaders/circles.wgsl"), &view_bgl, &circ_layout);
        let heat_pipe = pipeline(dev, fmt, "term heatmap", include_str!("../shaders/heatmap.wgsl"), &heat_bgl, &[]);

        let panes: [PaneGpu; MAX_PANES] = core::array::from_fn(|_| {
            let view = buf(dev, "pane view", core::mem::size_of::<ViewU>() as u64, wgpu::BufferUsages::UNIFORM);
            let view_bg = bind_uniform(dev, &view_bgl, &view);
            let lines = core::array::from_fn(|_| {
                let pts = buf(dev, "line pts", ((MAX_LINE_PTS + 2) * 8) as u64, wgpu::BufferUsages::VERTEX);
                let uni = buf(dev, "line uni", core::mem::size_of::<LineU>() as u64, wgpu::BufferUsages::UNIFORM);
                let bg = bind_uniform(dev, &line_bgl, &uni);
                LineGpu { pts, uni, bg }
            });
            PaneGpu {
                view,
                view_bg,
                rects: buf(dev, "rects", (MAX_RECTS * core::mem::size_of::<RectInst>()) as u64, wgpu::BufferUsages::VERTEX),
                circles: buf(
                    dev,
                    "circles",
                    (MAX_CIRCLES * core::mem::size_of::<CircleInst>()) as u64,
                    wgpu::BufferUsages::VERTEX,
                ),
                lines,
            }
        });

        let heat = make_tex(dev, "heat ring", HEAT_COLS, HEAT_ROWS, wgpu::TextureFormat::R8Unorm);
        let base = make_tex(dev, "heat base", HEAT_COLS, 1, wgpu::TextureFormat::R32Sint);
        let lut_t = make_tex(dev, "heat lut", 256, 1, wgpu::TextureFormat::Rgba8Unorm);
        queue.write_texture(
            copy_at(&lut_t, 0),
            bytemuck::cast_slice(lut.as_slice()),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256 * 4), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 256, height: 1, depth_or_array_layers: 1 },
        );
        let uni = buf(dev, "heat uni", core::mem::size_of::<HeatU>() as u64, wgpu::BufferUsages::UNIFORM);
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let (hv, bv, lv) = (view(&heat), view(&base), view(&lut_t));
        let bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("heat bg"),
            layout: &heat_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uni.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&hv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&bv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&lv) },
            ],
        });

        TermGpu { rect_pipe, line_pipe, circ_pipe, heat_pipe, panes, heat: HeatGpu { heat, base, uni, bg } }
    }

    /// Upload one pane's view, rects and circles. The caller sizes its
    /// staging to the caps; anything beyond is truncated (debug-asserted).
    pub fn upload_pane(&self, q: &wgpu::Queue, pane: usize, view: &ViewU, rects: &[RectInst], circles: &[CircleInst]) {
        debug_assert!(rects.len() <= MAX_RECTS && circles.len() <= MAX_CIRCLES);
        let p = &self.panes[pane];
        q.write_buffer(&p.view, 0, bytemuck::bytes_of(view));
        let r = &rects[..rects.len().min(MAX_RECTS)];
        let c = &circles[..circles.len().min(MAX_CIRCLES)];
        if !r.is_empty() {
            q.write_buffer(&p.rects, 0, bytemuck::cast_slice(r));
        }
        if !c.is_empty() {
            q.write_buffer(&p.circles, 0, bytemuck::cast_slice(c));
        }
    }

    /// Upload one series. `padded` is [first, p0 .. p(n-1), last]: the
    /// duplicated ends give the first and last segments their neighbours.
    pub fn upload_line(&self, q: &wgpu::Queue, pane: usize, slot: usize, view: &ViewU, style: LineStyle, padded: &[[f32; 2]]) {
        debug_assert!(padded.len() <= MAX_LINE_PTS + 2);
        let l = &self.panes[pane].lines[slot];
        let mut v = *view;
        v._pad[0] = style.width_px;
        q.write_buffer(&l.uni, 0, bytemuck::bytes_of(&LineU { view: v, color: style.color }));
        let n = padded.len().min(MAX_LINE_PTS + 2);
        if n > 0 {
            q.write_buffer(&l.pts, 0, bytemuck::cast_slice(&padded[..n]));
        }
    }

    /// Write one heat column at ring index `col` with its base tick.
    pub fn push_heat_col(&self, q: &wgpu::Queue, col: u32, base_tick: i32, cells: &[u8]) {
        debug_assert_eq!(cells.len(), HEAT_ROWS as usize);
        let col = col % HEAT_COLS;
        q.write_texture(
            copy_at(&self.heat.heat, col),
            cells,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: Some(HEAT_ROWS) },
            wgpu::Extent3d { width: 1, height: HEAT_ROWS, depth_or_array_layers: 1 },
        );
        q.write_texture(
            copy_at(&self.heat.base, col),
            &base_tick.to_le_bytes(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
    }

    pub fn upload_heat(&self, q: &wgpu::Queue, u: &HeatU) {
        q.write_buffer(&self.heat.uni, 0, bytemuck::bytes_of(u));
    }
}

// ---- paint callback (Copy scalars only) ---------------------------------

/// Draw one pane: heat (optional) → rects → lines → circles.
#[derive(Copy, Clone, Debug, Default)]
pub struct PaneDraw {
    pub pane: u8,
    pub heat: bool,
    pub n_rects: u32,
    pub n_circles: u32,
    pub n_lines: u8,
    /// Points per series (unpadded).
    pub line_pts: [u32; MAX_LINES],
}

impl CallbackTrait for PaneDraw {
    fn paint(&self, _info: egui::PaintCallbackInfo, rp: &mut wgpu::RenderPass<'static>, res: &CallbackResources) {
        let Some(g) = res.get::<TermGpu>() else { return };
        let p = &g.panes[(self.pane as usize).min(MAX_PANES - 1)];
        if self.heat {
            rp.set_pipeline(&g.heat_pipe);
            rp.set_bind_group(0, &g.heat.bg, &[]);
            rp.draw(0..4, 0..1);
        }
        if self.n_rects > 0 {
            rp.set_pipeline(&g.rect_pipe);
            rp.set_bind_group(0, &p.view_bg, &[]);
            rp.set_vertex_buffer(0, p.rects.slice(..));
            rp.draw(0..4, 0..self.n_rects.min(MAX_RECTS as u32));
        }
        let n_lines = (self.n_lines as usize).min(MAX_LINES);
        if n_lines > 0 {
            rp.set_pipeline(&g.line_pipe);
            for (l, &n) in p.lines.iter().zip(self.line_pts.iter()).take(n_lines) {
                let n = n.min(MAX_LINE_PTS as u32);
                if n < 2 {
                    continue;
                }
                rp.set_bind_group(0, &l.bg, &[]);
                for k in 0..4u32 {
                    rp.set_vertex_buffer(k, l.pts.slice(u64::from(k) * 8..));
                }
                rp.draw(0..4, 0..n - 1);
            }
        }
        if self.n_circles > 0 {
            rp.set_pipeline(&g.circ_pipe);
            rp.set_bind_group(0, &p.view_bg, &[]);
            rp.set_vertex_buffer(0, p.circles.slice(..));
            rp.draw(0..4, 0..self.n_circles.min(MAX_CIRCLES as u32));
        }
    }
}

/// A 256-entry colour LUT for the heatmap (transparent → blue → teal →
/// amber → white).
pub fn heat_lut() -> [[u8; 4]; 256] {
    let stops: [(f32, [f32; 4]); 5] = [
        (0.00, [0.02, 0.05, 0.12, 0.0]),
        (0.18, [0.05, 0.20, 0.45, 0.75]),
        (0.45, [0.10, 0.55, 0.70, 0.9]),
        (0.75, [0.95, 0.70, 0.20, 1.0]),
        (1.00, [1.00, 0.98, 0.92, 1.0]),
    ];
    core::array::from_fn(|i| {
        let x = i as f32 / 255.0;
        let mut k = 0;
        while k + 2 < stops.len() && x > stops[k + 1].0 {
            k += 1;
        }
        let (x0, c0) = stops[k];
        let (x1, c1) = stops[k + 1];
        let t = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
        core::array::from_fn(|j| ((c0[j] + (c1[j] - c0[j]) * t) * 255.0) as u8)
    })
}
