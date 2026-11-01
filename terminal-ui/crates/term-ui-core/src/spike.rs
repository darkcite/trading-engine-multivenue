// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The T0 render spike (plan v2.1 §6 T0): one pipeline of each kind inside
//! `CallbackTrait` and a timed frame with 5 000 egui labels, driven by the
//! synthetic market. It runs a scripted sequence of phases, logs per-phase
//! frame statistics as `TERMSPIKE {json}` lines, then stays interactive.
//!
//! Hot-ish path discipline (a cold UI process, so "loosely"): every staging
//! buffer is allocated once in `new` and refilled in place; egui's own
//! per-frame shape lists are egui's.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use egui::{Align2, Color32, FontId, Galley, Pos2, Rect, Sense, Vec2};
use egui_wgpu::RenderState;
use term_proto::dec::format_1e6;
use term_proto::md::Bar;
use term_render::{
    CircleInst, HeatU, LineStyle, PaneDraw, RectInst, TermGpu, ViewU, HEAT_COLS, HEAT_ROWS, MAX_CIRCLES, MAX_RECTS,
};

use crate::chart::{grid, nice_step, Viewport, ViewUniform};
use crate::synth::{Market, Print, TICK_1E6};
use crate::Platform;

// ---- palette (dark is functional here: charts and heat read at night) ----
pub const BG: Color32 = Color32::from_rgb(0x12, 0x18, 0x1F);
const PANE: Color32 = Color32::from_rgb(0x16, 0x1E, 0x27);
const TEXT: Color32 = Color32::from_rgb(0xC6, 0xD0, 0xD8);
const MUTED: Color32 = Color32::from_rgb(0x6B, 0x7C, 0x8A);
const UP: [u8; 4] = [0x4F, 0xB7, 0xA0, 0xFF];
const DOWN: [u8; 4] = [0xE0, 0x46, 0x6E, 0xFF];
const GRID: [u8; 4] = [0x24, 0x30, 0x3B, 0xFF];
const VOL_UP: [u8; 4] = [0x4F, 0xB7, 0xA0, 0x55];
const VOL_DOWN: [u8; 4] = [0xE0, 0x46, 0x6E, 0x55];

const NB: usize = 4096; // bar ring
const PPB: usize = 6; // prints kept per bar
const N_LABELS: usize = 5000;
const MAX_SAMPLES: usize = 512;
const N_LINES: usize = 3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    Warm,
    Chart,
    Heat,
    TextPaint,
    TextCached,
    All,
    Done,
}

impl Phase {
    fn frames(self) -> u32 {
        match self {
            Phase::Warm => 30,
            Phase::Chart | Phase::Heat | Phase::All => 240,
            Phase::TextPaint | Phase::TextCached => 120,
            Phase::Done => u32::MAX,
        }
    }
    fn next(self) -> Phase {
        match self {
            Phase::Warm => Phase::Chart,
            Phase::Chart => Phase::Heat,
            Phase::Heat => Phase::TextPaint,
            Phase::TextPaint => Phase::TextCached,
            Phase::TextCached => Phase::All,
            Phase::All | Phase::Done => Phase::Done,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Phase::Warm => "warm",
            Phase::Chart => "chart",
            Phase::Heat => "heat",
            Phase::TextPaint => "labels_painter_text",
            Phase::TextCached => "labels_cached_galleys",
            Phase::All => "all",
            Phase::Done => "interactive",
        }
    }
}

struct Samples {
    v: [f32; MAX_SAMPLES],
    n: usize,
}

impl Samples {
    const fn new() -> Samples {
        Samples { v: [0.0; MAX_SAMPLES], n: 0 }
    }
    fn push(&mut self, x: f32) {
        if self.n < MAX_SAMPLES && x.is_finite() {
            self.v[self.n] = x;
            self.n += 1;
        }
    }
    fn pct(&self, q: f32) -> f32 {
        if self.n == 0 {
            return f32::NAN;
        }
        let mut s = self.v;
        let s = &mut s[..self.n];
        s.sort_unstable_by(f32::total_cmp);
        s[((self.n - 1) as f32 * q).round() as usize]
    }
    fn mean(&self) -> f32 {
        if self.n == 0 {
            return f32::NAN;
        }
        self.v[..self.n].iter().sum::<f32>() / self.n as f32
    }
}

struct PhaseStats {
    dt: Samples,    // interval between frames (≈ rAF cadence), ms
    cpu: Samples,   // eframe's cpu_usage of the previous frame, ms
    build: Samples, // our own ui() time, ms
}

impl PhaseStats {
    const fn new() -> PhaseStats {
        PhaseStats { dt: Samples::new(), cpu: Samples::new(), build: Samples::new() }
    }
}

/// The spike app. Generic over the platform seam (no `dyn`).
pub struct Spike<P: Platform> {
    plat: P,
    gpu_ready: bool,
    backend: String,
    phase: Phase,
    phase_frame: u32,
    frame: u64,
    last_ms: f64,
    stats: PhaseStats,
    summary: String,

    mkt: Market,
    bars: Box<[Bar; NB]>,
    prints: Box<[Print; NB * PPB]>,
    nprints: Box<[u8; NB]>,
    ema20: Box<[f64; NB]>,
    ema50: Box<[f64; NB]>,
    vwap: Box<[f64; NB]>,
    vwap_num: f64,
    vwap_den: f64,
    first_bar: i64,
    n_bars: usize,
    view: Viewport,

    heat_mkt: Market,
    heat_cells: Box<[u8; HEAT_ROWS as usize]>,
    heat_col: i64,
    heat_mid: Box<[i64; HEAT_COLS as usize]>,
    heat_view: Viewport,

    rects: Vec<RectInst>,
    circles: Vec<CircleInst>,
    lines: [Vec<[f32; 2]>; N_LINES],
    grid_ticks: [i64; 64],
    label_cache: HashMap<i64, Arc<Galley>>,
    fmt: [u8; 28],
}

impl<P: Platform> Spike<P> {
    pub fn new(plat: P) -> Spike<P> {
        let mut s = Spike {
            last_ms: plat.now_ms(),
            plat,
            gpu_ready: false,
            backend: String::new(),
            phase: Phase::Warm,
            phase_frame: 0,
            frame: 0,
            stats: PhaseStats::new(),
            summary: String::with_capacity(4096),
            mkt: Market::new(0x7E57),
            bars: Box::new([Bar::default(); NB]),
            prints: Box::new([Print::default(); NB * PPB]),
            nprints: Box::new([0; NB]),
            ema20: Box::new([0.0; NB]),
            ema50: Box::new([0.0; NB]),
            vwap: Box::new([0.0; NB]),
            vwap_num: 0.0,
            vwap_den: 0.0,
            first_bar: 0,
            n_bars: 0,
            view: Viewport::new(0, 0, 7.0, 1.0),
            heat_mkt: Market::new(0xBEEF),
            heat_cells: Box::new([0; HEAT_ROWS as usize]),
            heat_col: 0,
            heat_mid: Box::new([0; HEAT_COLS as usize]),
            heat_view: Viewport::new(0, 0, 1.0, 1.0),
            rects: Vec::with_capacity(MAX_RECTS),
            circles: Vec::with_capacity(MAX_CIRCLES),
            lines: core::array::from_fn(|_| Vec::with_capacity(term_render::MAX_LINE_PTS + 2)),
            grid_ticks: [0; 64],
            label_cache: HashMap::with_capacity(N_LABELS * 2),
            fmt: [0; 28],
        };
        s.first_bar = s.mkt.bar;
        for _ in 0..3000 {
            s.push_bar();
        }
        let last = s.first_bar + s.n_bars as i64 - 1;
        let close = s.bars[(last as usize) % NB].c / TICK_1E6;
        s.view = Viewport::new(last - 150, close + 300, 7.0, 1.0);
        s
    }

    fn slot(b: i64) -> usize {
        b.rem_euclid(NB as i64) as usize
    }

    fn push_bar(&mut self) {
        let b = self.mkt.next_bar();
        let i = Self::slot(b.bar);
        let c = (b.c / TICK_1E6) as f64;
        let prev = if self.n_bars == 0 { c } else { self.ema20[Self::slot(b.bar - 1)] };
        let prev50 = if self.n_bars == 0 { c } else { self.ema50[Self::slot(b.bar - 1)] };
        self.ema20[i] = prev + (c - prev) * (2.0 / 21.0);
        self.ema50[i] = prev50 + (c - prev50) * (2.0 / 51.0);
        let typ = ((b.h + b.l + b.c) / 3 / TICK_1E6) as f64;
        let v = b.vol as f64 / 1e6;
        self.vwap_num = self.vwap_num * 0.995 + typ * v;
        self.vwap_den = self.vwap_den * 0.995 + v;
        self.vwap[i] = self.vwap_num / self.vwap_den.max(1e-9);
        let mut tmp = [Print::default(); PPB];
        let n = self.mkt.prints(&b, &mut tmp);
        self.prints[i * PPB..i * PPB + PPB].copy_from_slice(&tmp);
        self.nprints[i] = n as u8;
        self.bars[i] = b;
        if self.n_bars < NB {
            self.n_bars += 1;
        } else {
            self.first_bar += 1;
        }
    }

    fn last_bar(&self) -> i64 {
        self.first_bar + self.n_bars as i64 - 1
    }

    fn init_gpu(&mut self, rs: &RenderState) {
        let info = rs.adapter.get_info();
        let lim = rs.device.limits();
        let webgl2 = egui_wgpu::wgpu::Limits::downlevel_webgl2_defaults();
        // Plan §4.11.4: the pipelines live inside the WebGL2 subset on every
        // backend; the device was created with these limits (term-ui-web).
        let within = lim.max_storage_buffers_per_shader_stage == 0
            && lim.max_uniform_buffer_binding_size <= webgl2.max_uniform_buffer_binding_size.max(16 * 1024)
            && lim.max_vertex_buffers >= 4;
        self.backend = format!("{:?}", info.backend);
        let gpu = TermGpu::new(&rs.device, &rs.queue, rs.target_format, &term_render::heat_lut());
        rs.renderer.write().callback_resources.insert(gpu);
        self.gpu_ready = true;
        self.plat.log(&format!(
            "TERMSPIKE {{\"event\":\"init\",\"backend\":\"{:?}\",\"adapter\":\"{}\",\"driver\":\"{}\",\"format\":\"{:?}\",\"storage_buffers\":{},\"max_uniform_binding\":{},\"max_vertex_buffers\":{},\"max_tex_2d\":{},\"webgl2_subset\":{}}}",
            info.backend,
            info.name.replace('"', "'"),
            info.driver.replace('"', "'"),
            rs.target_format,
            lim.max_storage_buffers_per_shader_stage,
            lim.max_uniform_buffer_binding_size,
            lim.max_vertex_buffers,
            lim.max_texture_dimension_2d,
            within
        ));
    }

    /// One frame. `cpu_last_ms` is eframe's CPU time for the previous frame.
    pub fn ui(&mut self, ctx: &egui::Context, rs: Option<&RenderState>, cpu_last_ms: Option<f32>) {
        let t0 = self.plat.now_ms();
        let Some(rs) = rs else {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("No wgpu render state: the WebGPU/WebGL2 device failed to start.");
            });
            return;
        };
        if !self.gpu_ready {
            self.init_gpu(rs);
        }
        self.frame += 1;
        ctx.request_repaint();

        let shows_chart = matches!(self.phase, Phase::Warm | Phase::Chart | Phase::All | Phase::Done);
        let shows_heat = matches!(self.phase, Phase::Heat | Phase::All | Phase::Done);
        if shows_chart && self.frame % 8 == 0 && self.phase != Phase::Done {
            self.push_bar();
        }
        if shows_heat {
            self.push_heat(rs);
        }

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ctx, |ui| {
            let full = ui.available_rect_before_wrap();
            self.hud(ui, full);
            let body = Rect::from_min_max(full.min + Vec2::new(0.0, 22.0), full.max);
            match self.phase {
                Phase::Warm | Phase::Chart => self.chart_pane(ui, rs, body, 0),
                Phase::Heat => self.heat_pane(ui, rs, body, 1),
                Phase::TextPaint => self.labels(ui, body, false),
                Phase::TextCached => self.labels(ui, body, true),
                Phase::All | Phase::Done => {
                    let ladder_w = 260.0f32.min(body.width() * 0.3);
                    let left = Rect::from_min_max(body.min, Pos2::new(body.max.x - ladder_w - 6.0, body.max.y));
                    let split = left.min.y + left.height() * 0.6;
                    let top = Rect::from_min_max(left.min, Pos2::new(left.max.x, split - 3.0));
                    let bot = Rect::from_min_max(Pos2::new(left.min.x, split + 3.0), left.max);
                    let ladder = Rect::from_min_max(Pos2::new(body.max.x - ladder_w, body.min.y), body.max);
                    self.chart_pane(ui, rs, top, 0);
                    self.heat_pane(ui, rs, bot, 1);
                    self.ladder(ui, ladder);
                }
            }
        });

        // Stats (the warm phase is measured but not reported).
        let now = self.plat.now_ms();
        let dt = (now - self.last_ms) as f32;
        self.last_ms = now;
        if self.phase_frame > 2 {
            self.stats.dt.push(dt);
            if let Some(c) = cpu_last_ms {
                self.stats.cpu.push(c);
            }
            self.stats.build.push((now - t0) as f32);
        }
        self.phase_frame += 1;
        if self.phase != Phase::Done && self.phase_frame >= self.phase.frames() {
            self.end_phase();
        }
    }

    fn end_phase(&mut self) {
        let s = &self.stats;
        let line = format!(
            "{{\"event\":\"phase\",\"phase\":\"{}\",\"backend\":\"{}\",\"frames\":{},\"dt_ms\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2},\"mean\":{:.2}}},\"cpu_ms\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}},\"build_ms\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}},\"labels\":{}}}",
            self.phase.name(),
            self.backend,
            s.dt.n,
            s.dt.pct(0.5),
            s.dt.pct(0.95),
            s.dt.pct(0.99),
            s.dt.pct(1.0),
            s.dt.mean(),
            s.cpu.pct(0.5),
            s.cpu.pct(0.95),
            s.cpu.pct(0.99),
            s.cpu.pct(1.0),
            s.build.pct(0.5),
            s.build.pct(0.95),
            s.build.pct(0.99),
            s.build.pct(1.0),
            match self.phase {
                Phase::TextPaint | Phase::TextCached => N_LABELS,
                Phase::All => 600,
                _ => 0,
            }
        );
        if self.phase != Phase::Warm {
            self.plat.log(&format!("TERMSPIKE {line}"));
            let _ = writeln!(
                self.summary,
                "{:<22} dt p50 {:>6.1} p99 {:>6.1} ms · cpu p50 {:>5.1} ms",
                self.phase.name(),
                s.dt.pct(0.5),
                s.dt.pct(0.99),
                s.cpu.pct(0.5)
            );
        }
        self.phase = self.phase.next();
        self.phase_frame = 0;
        self.stats = PhaseStats::new();
        if self.phase == Phase::Done {
            self.plat.log("TERMSPIKE {\"event\":\"done\"}");
        }
    }

    fn hud(&self, ui: &egui::Ui, full: Rect) {
        let p = ui.painter();
        let txt = format!(
            "TERM T0 spike · {} · {} · frame {} · {} bars",
            self.backend,
            self.phase.name(),
            self.frame,
            self.n_bars
        );
        p.text(full.min + Vec2::new(8.0, 4.0), Align2::LEFT_TOP, txt, FontId::monospace(12.0), MUTED);
        if self.phase == Phase::Done && !self.summary.is_empty() {
            let g = p.layout_no_wrap(self.summary.clone(), FontId::monospace(11.0), TEXT);
            let r = Rect::from_min_size(Pos2::new(full.max.x - g.size().x - 18.0, full.min.y + 26.0), g.size() + Vec2::splat(12.0));
            p.rect_filled(r, 4.0, Color32::from_rgba_unmultiplied(0x12, 0x18, 0x1F, 0xE0));
            p.galley(r.min + Vec2::splat(6.0), g, TEXT);
        }
    }

    // ---- chart pane: rects (candles, volume, grid), lines, circles ------

    fn chart_pane(&mut self, ui: &mut egui::Ui, rs: &RenderState, rect: Rect, pane: usize) {
        let resp = ui.interact(rect, ui.id().with(("chart", pane)), Sense::click_and_drag());
        let (w, h) = (rect.width(), rect.height());
        if self.phase == Phase::Done {
            let d = resp.drag_delta();
            if d != Vec2::ZERO {
                self.view.pan_px(d.x, d.y);
            }
            if let Some(hp) = resp.hover_pos() {
                let z = ui.input(|i| i.smooth_scroll_delta.y);
                if z != 0.0 {
                    self.view.zoom_x_at(hp.x - rect.min.x, (1.0 + z * 0.002).clamp(0.8, 1.25));
                }
            }
            if resp.double_clicked() {
                self.view.px_per_bar = 7.0;
            }
        } else {
            // Scripted: follow the last bar, breathe the zoom, slow drift.
            let ppb = 7.0 + 3.5 * ((self.frame as f32) * 0.03).sin();
            self.view.px_per_bar = ppb;
            let right = self.last_bar() + 6;
            self.view.left = crate::chart::Pos::new(right).shift(-w / ppb);
        }
        let (b0, b1) = self.view.visible_bars(w);
        let b0 = b0.max(self.first_bar);
        let b1 = b1.min(self.last_bar());
        if b1 <= b0 {
            return;
        }
        // Auto-fit price to the visible bars (TradingView default).
        let (mut lo, mut hi) = (i64::MAX, i64::MIN);
        for b in b0..=b1 {
            let bar = &self.bars[Self::slot(b)];
            lo = lo.min(bar.l / TICK_1E6);
            hi = hi.max(bar.h / TICK_1E6);
        }
        if self.phase != Phase::Done || !resp.dragged() {
            self.view.fit_y(lo, hi, h * 0.78, 0.06);
            self.view.top = self.view.top.shift((h * 0.02) / self.view.px_per_tick);
        }
        self.view.rebase_if_needed();
        let u: ViewUniform = self.view.uniform(w, h);
        let vu = ViewU { ax: u.ax, bx: u.bx, ay: u.ay, by: u.by, w_px: u.w_px, h_px: u.h_px, _pad: [0.0; 2] };

        // Grid.
        self.rects.clear();
        self.circles.clear();
        let (t_lo, t_hi) = self.view.visible_ticks(h);
        let step = nice_step(t_hi - t_lo, 8);
        let n_grid = grid(t_lo, t_hi, step, &mut self.grid_ticks);
        let xmid = self.view.left.rel(self.view.origin_bar) + 0.5 * w / self.view.px_per_bar;
        let half = 0.5 * w / self.view.px_per_bar + 2.0;
        for &t in &self.grid_ticks[..n_grid] {
            let y = self.view.off_tick(t);
            self.rects.push(RectInst { x: xmid, hw: half, y0: y, y1: y, min_px: 0.0, rgba: GRID });
        }
        // Volume (bottom 18 % of the pane), then wicks and bodies.
        let span_t = (t_hi - t_lo) as f32;
        let floor_t = self.view.off_tick(t_lo);
        let mut vmax = 1i64;
        for b in b0..=b1 {
            vmax = vmax.max(self.bars[Self::slot(b)].vol);
        }
        for b in b0..=b1 {
            let bar = self.bars[Self::slot(b)];
            let x = self.view.off_bar(b) + 0.5;
            let up = bar.c >= bar.o;
            let vh = span_t * 0.16 * (bar.vol as f32 / vmax as f32);
            self.rects.push(RectInst { x, hw: 0.36, y0: floor_t, y1: floor_t + vh, min_px: 0.0, rgba: if up { VOL_UP } else { VOL_DOWN } });
        }
        for b in b0..=b1 {
            let bar = self.bars[Self::slot(b)];
            let x = self.view.off_bar(b) + 0.5;
            let col = if bar.c >= bar.o { UP } else { DOWN };
            let (o, c, hh, l) = (bar.o / TICK_1E6, bar.c / TICK_1E6, bar.h / TICK_1E6, bar.l / TICK_1E6);
            self.rects.push(RectInst { x, hw: 0.0, y0: self.view.off_tick(l), y1: self.view.off_tick(hh), min_px: 0.5, rgba: col });
            self.rects.push(RectInst { x, hw: 0.36, y0: self.view.off_tick(o), y1: self.view.off_tick(c), min_px: 0.5, rgba: col });
            // Bubbles: area ∝ size.
            let i = Self::slot(b);
            for k in 0..self.nprints[i] as usize {
                let p = self.prints[i * PPB + k];
                if self.circles.len() >= MAX_CIRCLES {
                    break;
                }
                let r = 1.5 + (p.size_1e6 as f32 / 1e6).sqrt() * 2.2;
                let ring = if p.size_1e6 > 15_000_000 { 0.28 } else { 0.0 };
                let mut c = if p.buy { UP } else { DOWN };
                c[3] = 0x90;
                self.circles.push(CircleInst { x, y: self.view.off_tick(p.tick), r_px: r, ring, rgba: c });
            }
        }
        // Lines: EMA20, EMA50, VWAP — padded with duplicated ends.
        let series: [(&[f64; NB], LineStyle); N_LINES] = [
            (&self.ema20, LineStyle { width_px: 1.6, color: [0.96, 0.78, 0.36, 1.0] }),
            (&self.ema50, LineStyle { width_px: 1.6, color: [0.55, 0.62, 0.98, 1.0] }),
            (&self.vwap, LineStyle { width_px: 2.2, color: [0.85, 0.85, 0.88, 0.55] }),
        ];
        let mut line_pts = [0u32; term_render::MAX_LINES];
        let gpu_guard = rs.renderer.read();
        let Some(gpu) = gpu_guard.callback_resources.get::<TermGpu>() else { return };
        for (k, (src, style)) in series.iter().enumerate() {
            let out = &mut self.lines[k];
            out.clear();
            for b in b0..=b1 {
                let y = (src[Self::slot(b)] - self.view.origin_tick as f64) as f32;
                let p = [self.view.off_bar(b) + 0.5, y];
                if out.is_empty() {
                    out.push(p);
                }
                out.push(p);
            }
            if let Some(&last) = out.last() {
                out.push(last);
            }
            line_pts[k] = (out.len() - 2) as u32;
            gpu.upload_line(&rs.queue, pane, k, &vu, *style, out);
        }
        gpu.upload_pane(&rs.queue, pane, &vu, &self.rects, &self.circles);
        drop(gpu_guard);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            PaneDraw {
                pane: pane as u8,
                heat: false,
                n_rects: self.rects.len() as u32,
                n_circles: self.circles.len() as u32,
                n_lines: N_LINES as u8,
                line_pts,
            },
        ));
        // Price axis labels from i64 with integer formatting (no f64 Display).
        for &t in &self.grid_ticks[..n_grid] {
            let y = rect.min.y + self.view.y_px(t);
            let n = format_1e6(t * TICK_1E6, 1, &mut self.fmt);
            let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
            painter.text(Pos2::new(rect.max.x - 6.0, y - 1.0), Align2::RIGHT_BOTTOM, s, FontId::monospace(10.5), MUTED);
        }
        let last = self.bars[Self::slot(self.last_bar())].c;
        let n = format_1e6(last, 1, &mut self.fmt);
        let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
        let y = rect.min.y + self.view.y_px(last / TICK_1E6);
        let tag = Rect::from_min_size(Pos2::new(rect.max.x - 64.0, y - 8.0), Vec2::new(64.0, 16.0));
        painter.rect_filled(tag, 2.0, Color32::from_rgb(UP[0], UP[1], UP[2]));
        painter.text(tag.center(), Align2::CENTER_CENTER, s, FontId::monospace(10.5), BG);
    }

    // ---- heat pane: the texture ring + bubbles --------------------------

    fn push_heat(&mut self, rs: &RenderState) {
        let base = self.heat_mkt.heat_col(1, &mut self.heat_cells[..]);
        let col = self.heat_col.rem_euclid(HEAT_COLS as i64) as u32;
        self.heat_mid[col as usize] = self.heat_mkt.mid_tick;
        let guard = rs.renderer.read();
        if let Some(gpu) = guard.callback_resources.get::<TermGpu>() {
            gpu.push_heat_col(&rs.queue, col, base as i32, &self.heat_cells[..]);
        }
        self.heat_col += 1;
    }

    fn heat_pane(&mut self, ui: &mut egui::Ui, rs: &RenderState, rect: Rect, pane: usize) {
        let (w, h) = (rect.width(), rect.height());
        let col_px = 2.0f32;
        let ppt = h / 420.0; // ~420 ticks tall
        let mid = self.heat_mkt.mid_tick;
        let hv = &mut self.heat_view;
        hv.px_per_bar = col_px;
        hv.px_per_tick = ppt;
        hv.left = crate::chart::Pos::new(self.heat_col).shift(-w / col_px);
        hv.top = crate::chart::Pos::new(mid).shift(210.0);
        if hv.origin_tick == 0 || hv.rebase_if_needed() {
            hv.origin_tick = mid;
            hv.origin_bar = self.heat_col;
        }
        let u = hv.uniform(w, h);
        let vu = ViewU { ax: u.ax, bx: u.bx, ay: u.ay, by: u.by, w_px: w, h_px: h, _pad: [0.0; 2] };
        let filled = self.heat_col.min(HEAT_COLS as i64) as i32;
        let head = (self.heat_col - 1).rem_euclid(HEAT_COLS as i64) as i32;
        let hu = HeatU {
            w_px: w,
            h_px: h,
            col_px,
            ticks_per_px: 1.0 / ppt,
            top_rel: hv.top.rel(hv.origin_tick),
            contrast: 1.15,
            gamma: 0.85,
            _p0: 0.0,
            origin_tick: hv.origin_tick as i32,
            head,
            filled,
            tick_scale: 1,
        };
        // Bubbles on recent columns.
        self.rects.clear();
        self.circles.clear();
        let visible = ((w / col_px) as i64).min(self.heat_col);
        let mut rng = crate::synth::Rng::new(0xC0FFEE);
        for age in 0..visible {
            let c = self.heat_col - 1 - age;
            let r = rng.unit();
            if r < 0.06 && self.circles.len() < MAX_CIRCLES {
                let m = self.heat_mid[c.rem_euclid(HEAT_COLS as i64) as usize];
                let tick = m + ((rng.unit() - 0.5) * 6.0) as i64;
                let size = rng.unit().powi(3) * 30.0;
                let buy = rng.unit() < 0.5;
                let mut col = if buy { UP } else { DOWN };
                col[3] = 0xB0;
                self.circles.push(CircleInst {
                    x: hv.off_bar(c) + 0.5,
                    y: hv.off_tick(tick),
                    r_px: 2.0 + size.sqrt() * 2.6,
                    ring: if size > 18.0 { 0.3 } else { 0.0 },
                    rgba: col,
                });
            }
        }
        let guard = rs.renderer.read();
        if let Some(gpu) = guard.callback_resources.get::<TermGpu>() {
            gpu.upload_heat(&rs.queue, &hu);
            gpu.upload_pane(&rs.queue, pane, &vu, &self.rects, &self.circles);
        }
        drop(guard);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x0C, 0x11, 0x17));
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            PaneDraw { pane: pane as u8, heat: true, n_rects: 0, n_circles: self.circles.len() as u32, n_lines: 0, line_pts: [0; 8] },
        ));
        painter.text(rect.min + Vec2::new(8.0, 6.0), Align2::LEFT_TOP, "depth heatmap · synthetic", FontId::monospace(10.5), MUTED);
    }

    // ---- 5 000 labels: the go/no-go for egui's text path -----------------

    fn labels(&mut self, ui: &mut egui::Ui, rect: Rect, cached: bool) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        let cols = 50usize;
        let rows = N_LABELS / cols;
        let cw = rect.width() / cols as f32;
        let rh = rect.height() / rows as f32;
        let font = FontId::monospace(9.5);
        let f = self.frame as i64;
        for i in 0..N_LABELS {
            let (cx, ry) = (i % cols, i / cols);
            let pos = Pos2::new(rect.min.x + cx as f32 * cw + 2.0, rect.min.y + ry as f32 * rh);
            if cached {
                // 5 000 distinct prices, shuffled positions: a ladder that
                // scrolls re-shows the same values. Cached per value.
                let v = 67_000_000_000 + (((i as i64) + f * 37) % N_LABELS as i64) * TICK_1E6;
                let g = match self.label_cache.get(&v) {
                    Some(g) => g.clone(),
                    None => {
                        let n = format_1e6(v, 1, &mut self.fmt);
                        let s = String::from_utf8_lossy(&self.fmt[..n]).into_owned();
                        let g = painter.layout_no_wrap(s, font.clone(), TEXT);
                        self.label_cache.insert(v, g.clone());
                        g
                    }
                };
                painter.galley(pos, g, TEXT);
            } else {
                // Worst case: every label's value changes every frame.
                let v = 67_000_000_000 + ((i as i64 * 7919 + f * 13) % 1_000_000_000);
                let n = format_1e6(v, 2, &mut self.fmt);
                let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
                painter.text(pos, Align2::LEFT_TOP, s, font.clone(), TEXT);
            }
        }
    }

    // ---- a ladder-shaped column (600 labels) for the combined phase ------

    fn ladder(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        let rows = 120usize;
        let rh = rect.height() / rows as f32;
        let mid = self.heat_mkt.mid_tick;
        let font = FontId::monospace(9.5);
        let mut rng = crate::synth::Rng::new(self.frame);
        for r in 0..rows {
            let tick = mid + 60 - r as i64;
            let y = rect.min.y + r as f32 * rh;
            let bid = tick < mid;
            let size = (rng.unit() * 40.0 * 1e6) as i64;
            let bar_w = (size as f32 / 40e6) * rect.width() * 0.3;
            let c = if bid { UP } else { DOWN };
            let fill = Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 0x30);
            let bx = if bid { rect.min.x + rect.width() * 0.35 - bar_w } else { rect.min.x + rect.width() * 0.65 };
            painter.rect_filled(Rect::from_min_size(Pos2::new(bx, y), Vec2::new(bar_w, rh - 1.0)), 0.0, fill);
            let cells: [(f32, i64, u32, Align2); 5] = [
                (0.05, if bid { size / 3 } else { 0 }, 2, Align2::LEFT_TOP),
                (0.33, if bid { size } else { 0 }, 2, Align2::RIGHT_TOP),
                (0.50, tick * TICK_1E6, 1, Align2::CENTER_TOP),
                (0.67, if bid { 0 } else { size }, 2, Align2::LEFT_TOP),
                (0.95, if bid { 0 } else { size / 3 }, 2, Align2::RIGHT_TOP),
            ];
            for (fx, v, d, al) in cells {
                if v == 0 && fx != 0.50 {
                    continue;
                }
                let n = format_1e6(v, d, &mut self.fmt);
                let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
                let col = if fx == 0.50 { TEXT } else { MUTED };
                painter.text(Pos2::new(rect.min.x + rect.width() * fx, y), al, s, font.clone(), col);
            }
        }
    }
}
