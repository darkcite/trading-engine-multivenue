// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The terminal app (TERM plan v2.1 §4.11.6, T1b first cut): the server's
//! frames drawn with `term-render` — candles and volume from `Bar`, the
//! heatmap from `HeatCol`, a ladder from `BookSnap`/`BookDelta`, the tape
//! from `TradeBatch`, flow from `FlowPoint`. The server computes; the WASM
//! renders (O-T13). Prices stay ×1e6 integers until a label is formatted
//! with integer arithmetic (T-L8).

use std::collections::BTreeMap;

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use egui_wgpu::RenderState;
use term_proto::ctl::{Beat, Control, CtlOp, InstrDef, CH_ALL};
use term_proto::dec::format_1e6;
use term_proto::ctl::Venue;
use term_proto::exec::{EngineStatus, FillView, Intent, IntentAck, Op, OrderKind, OrderStatus, OrderView, PositionView, AckSource, FLAG_REDUCE_ONLY};
use term_proto::md::*;
use term_proto::{Kind, Side};
use term_render::{CircleInst, HeatU, PaneDraw, RectInst, TermGpu, ViewU, HEAT_COLS, HEAT_ROWS};

use crate::chart::{grid, nice_step, Pos, Viewport};
use crate::spike::BG;
use crate::{Link, LinkState, Platform};

const PANE: Color32 = Color32::from_rgb(0x16, 0x1E, 0x27);
const TEXT: Color32 = Color32::from_rgb(0xC6, 0xD0, 0xD8);
const MUTED: Color32 = Color32::from_rgb(0x6B, 0x7C, 0x8A);
const UP: [u8; 4] = [0x4F, 0xB7, 0xA0, 0xFF];
const DOWN: [u8; 4] = [0xE0, 0x46, 0x6E, 0xFF];
const GRID: [u8; 4] = [0x24, 0x30, 0x3B, 0xFF];
const MAX_BARS: usize = 2000;
const TAPE: usize = 64;
const DRAIN_PER_PAINT: usize = 4000;

#[derive(Default)]
struct Inst {
    def: Option<InstrDef>,
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
    bars: BTreeMap<i64, Bar>,
    tape: Vec<(i64, i64, Side)>,
    flow: FlowPoint,
    heat_base: Vec<i32>,
    heat_head: i64,
    heat_scale: u16,
    view: Option<Viewport>,
    follow: bool,
}

pub struct App<P: Platform, L: Link> {
    plat: P,
    link: L,
    gpu_ready: bool,
    ins: Vec<Inst>,
    sel: usize,
    status: Option<EngineStatus>,
    subscribed: Vec<bool>,
    frames: u64,
    bad: u64,
    dropped: u64,
    token: String,
    out: [u8; 256],
    rects: Vec<RectInst>,
    circles: Vec<CircleInst>,
    heat_cells: Vec<u8>,
    grid_ticks: [i64; 64],
    fmt: [u8; 28],
    last_log: f64,
    // Order entry (T2 first cut): off at login (§4.11.6).
    trading: bool,
    nonce: u64,
    px_text: String,
    qty_text: String,
    reduce_only: bool,
    entry_err: Option<&'static str>,
    orders: BTreeMap<u64, OrderView>,
    positions: BTreeMap<(u8, u16), PositionView>,
    fills: Vec<FillView>,
    acks: Vec<IntentAck>,
}

/// The TERM slot an instrument trades on (O-T17: HL 12, Binance 13,
/// Polymarket 14, Hypercall 15).
pub fn term_slot(v: Venue) -> u8 {
    match v {
        Venue::Hyperliquid => 12,
        Venue::Binance => 13,
        Venue::Polymarket => 14,
        Venue::Hypercall => 15,
    }
}

/// A readable reason for an ack (server codes from `term_proto::exec`,
/// engine codes are `ingress_term::Reason`).
pub fn ack_reason(a: &IntentAck) -> &'static str {
    if a.accepted {
        return "accepted";
    }
    match (a.source, a.reason) {
        (AckSource::Server, 1) => "order entry is off (replay or no door)",
        (AckSource::Server, 2) => "trading is off for this session",
        (AckSource::Server, 3) => "no door to the engine",
        (AckSource::Server, 4) => "no route for this instrument, or an op not carried yet",
        (AckSource::Server, 5) => "the door socket failed",
        (AckSource::Engine, 1) => "malformed",
        (AckSource::Engine, 2) => "out of sequence",
        (AckSource::Engine, 3) => "another engine boot",
        (AckSource::Engine, 4) => "too old",
        (AckSource::Engine, 5) => "the engine is busy (ring full)",
        (AckSource::Engine, 6) => "the slot is halted",
        (AckSource::Engine, 7) => "outside the price band",
        (AckSource::Engine, 8) => "below the venue minimum",
        (AckSource::Engine, 9) => "over the order notional cap",
        (AckSource::Engine, 10) => "over the position cap",
        (AckSource::Engine, 11) => "rate limited",
        (AckSource::Engine, 12) => "no fresh engine price",
        (AckSource::Engine, 13) => "unknown order",
        (AckSource::Engine, 14) => "the venue path refused it",
        (AckSource::Engine, 15) => "instrument not allowed",
        _ => "refused",
    }
}

impl<P: Platform, L: Link> App<P, L> {
    pub fn new(plat: P, link: L) -> App<P, L> {
        App {
            plat,
            link,
            gpu_ready: false,
            ins: Vec::new(),
            sel: 0,
            status: None,
            subscribed: Vec::new(),
            frames: 0,
            bad: 0,
            dropped: 0,
            token: String::new(),
            out: [0; 256],
            rects: Vec::with_capacity(term_render::MAX_RECTS),
            circles: Vec::with_capacity(term_render::MAX_CIRCLES),
            heat_cells: vec![0; HEAT_ROWS as usize],
            grid_ticks: [0; 64],
            fmt: [0; 28],
            last_log: 0.0,
            trading: false,
            nonce: 0,
            px_text: String::new(),
            qty_text: String::new(),
            reduce_only: false,
            entry_err: None,
            orders: BTreeMap::new(),
            positions: BTreeMap::new(),
            fills: Vec::with_capacity(64),
            acks: Vec::with_capacity(32),
        }
    }

    fn inst(&mut self, k: u16) -> &mut Inst {
        let k = k as usize;
        while self.ins.len() <= k {
            self.ins.push(Inst { follow: true, heat_base: vec![i32::MIN; HEAT_COLS as usize], ..Inst::default() });
        }
        &mut self.ins[k]
    }

    fn subscribe(&mut self, k: u16) {
        while self.subscribed.len() <= k as usize {
            self.subscribed.push(false);
        }
        if self.subscribed[k as usize] {
            return;
        }
        self.subscribed[k as usize] = true;
        if let Ok(n) = (Control { op: CtlOp::Subscribe, a: k, b: CH_ALL, c: 0 }).encode(0, 0, &mut self.out) {
            self.link.send(&self.out[..n]);
        }
    }

    /// Apply one server frame. Unknown or invalid frames are counted, never
    /// trusted.
    fn apply(&mut self, raw: &[u8], rs: Option<&RenderState>) {
        let Ok((f, _)) = term_proto::decode(raw) else {
            self.bad += 1;
            return;
        };
        self.frames += 1;
        match f.hdr.kind {
            Kind::InstrDef => {
                if let Ok(d) = InstrDef::parse(f.body) {
                    self.inst(d.instr).def = Some(d);
                    self.subscribe(d.instr);
                }
            }
            Kind::EngineStatus => self.status = EngineStatus::parse(f.body).ok(),
            Kind::BookSnap => {
                if let Ok(v) = BookSnap::parse(f.body) {
                    let i = self.inst(v.instr);
                    let m = if v.side == Side::Buy { &mut i.bids } else { &mut i.asks };
                    if v.chunk.offset == 0 {
                        m.clear();
                    }
                    for k in 0..v.len() {
                        let l = v.level(k);
                        m.insert(l.px, l.qty);
                    }
                }
            }
            Kind::BookDelta => {
                if let Ok(v) = BookDelta::parse(f.body) {
                    let i = self.inst(v.instr);
                    for k in 0..v.len() {
                        let d = v.get(k);
                        let m = if d.side == Side::Buy { &mut i.bids } else { &mut i.asks };
                        if d.qty == 0 {
                            m.remove(&d.px);
                        } else {
                            m.insert(d.px, d.qty);
                        }
                    }
                }
            }
            Kind::TradeBatch => {
                if let Ok(v) = TradeBatch::parse(f.body) {
                    let i = self.inst(v.instr);
                    for k in 0..v.len() {
                        let t = v.get(k);
                        if i.tape.len() == TAPE {
                            i.tape.remove(0);
                        }
                        i.tape.push((t.px, t.qty, t.aggressor));
                    }
                }
            }
            Kind::Bar => {
                if let Ok(b) = Bar::parse(f.body) {
                    let i = self.inst(b.instr);
                    i.bars.insert(b.bar, b);
                    while i.bars.len() > MAX_BARS {
                        i.bars.pop_first();
                    }
                }
            }
            Kind::FlowPoint => {
                if let Ok(p) = FlowPoint::parse(f.body) {
                    self.inst(p.instr).flow = p;
                }
            }
            Kind::HeatCol => {
                if let Ok(h) = HeatCol::parse(f.body) {
                    // Only the selected instrument owns the GPU ring.
                    if h.instr as usize == self.sel {
                        let col = (h.col % HEAT_COLS) as usize;
                        self.heat_cells.fill(0);
                        let n = h.cells.len().min(HEAT_ROWS as usize);
                        self.heat_cells[..n].copy_from_slice(&h.cells[..n]);
                        let base = h.base_tick;
                        let i = self.inst(h.instr);
                        i.heat_base[col] = base;
                        i.heat_head = h.col as i64;
                        i.heat_scale = h.tick_scale;
                        if let Some(rs) = rs {
                            let g = rs.renderer.read();
                            if let Some(gpu) = g.callback_resources.get::<TermGpu>() {
                                gpu.push_heat_col(&rs.queue, col as u32, base, &self.heat_cells);
                            }
                        }
                    }
                }
            }
            Kind::OrderView => {
                if let Ok(o) = OrderView::parse(f.body) {
                    let oid = u64::from_le_bytes([o.cloid[0], o.cloid[1], o.cloid[2], o.cloid[3], o.cloid[4], o.cloid[5], o.cloid[6], o.cloid[7]]);
                    if matches!(o.status, OrderStatus::Open | OrderStatus::Partial | OrderStatus::Pending) {
                        self.orders.insert(oid, o);
                    } else {
                        self.orders.remove(&oid);
                    }
                }
            }
            Kind::PositionView => {
                if let Ok(p) = PositionView::parse(f.body) {
                    self.positions.insert((p.slot, p.instr), p);
                }
            }
            Kind::FillView => {
                if let Ok(x) = FillView::parse(f.body) {
                    if self.fills.len() == 64 {
                        self.fills.remove(0);
                    }
                    self.fills.push(x);
                }
            }
            Kind::IntentAck => {
                if let Ok(a) = IntentAck::parse(f.body) {
                    if self.acks.len() == 32 {
                        self.acks.remove(0);
                    }
                    self.acks.push(a);
                }
            }
            Kind::Heartbeat => {
                // The web link echoes from its socket handler already; this
                // is only for a link that does not (native, tests).
                let _ = Beat::parse(f.body);
            }
            _ => {}
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, rs: Option<&RenderState>) {
        if let (Some(rs), false) = (rs, self.gpu_ready) {
            // The terminal is dark whatever the browser prefers: its panes,
            // heatmap and palette are designed for it.
            ctx.set_visuals(egui::Visuals::dark());
            let gpu = TermGpu::new(&rs.device, &rs.queue, rs.target_format, &term_render::heat_lut());
            rs.renderer.write().callback_resources.insert(gpu);
            self.gpu_ready = true;
        }
        let mut batch: Vec<Vec<u8>> = Vec::new();
        self.link.drain(DRAIN_PER_PAINT, |f| batch.push(f.to_vec()));
        for f in &batch {
            self.apply(f, rs);
        }
        let d = self.link.take_dropped();
        if d > 0 {
            self.dropped += d as u64;
            // The ring overflowed: ask for fresh snapshots.
            if let Ok(n) = Control::op(CtlOp::Stale).encode(0, 0, &mut self.out) {
                self.link.send(&self.out[..n]);
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
        let now = self.plat.now_ms();
        if now - self.last_log > 5000.0 {
            self.last_log = now;
            let books: Vec<String> = self.ins.iter().map(|i| format!("[{},{},{}]", i.bids.len(), i.asks.len(), i.bars.len())).collect();
            self.plat.log(&format!(
                "TERMAPP {{\"state\":\"{:?}\",\"frames\":{},\"bad\":{},\"dropped\":{},\"instruments\":{},\"books_bids_asks_bars\":[{}]}}",
                self.link.state(), self.frames, self.bad, self.dropped, self.ins.len(), books.join(",")
            ));
        }

        match self.link.state() {
            LinkState::NeedsLogin | LinkState::LoginFailed => return self.login_form(ctx),
            _ => {}
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ctx, |ui| {
            let full = ui.available_rect_before_wrap();
            self.top_bar(ui, Rect::from_min_size(full.min, Vec2::new(full.width(), 24.0)));
            let body = Rect::from_min_max(full.min + Vec2::new(0.0, 28.0), full.max);
            if self.ins.is_empty() {
                ui.painter().text(body.center(), Align2::CENTER_CENTER, "Waiting for the server's instruments…", FontId::proportional(14.0), MUTED);
                return;
            }
            let side_w = 280.0f32.min(body.width() * 0.3);
            let left = Rect::from_min_max(body.min, Pos2::new(body.max.x - side_w - 6.0, body.max.y));
            let right = Rect::from_min_max(Pos2::new(body.max.x - side_w, body.min.y), body.max);
            let s1 = left.min.y + left.height() * 0.52;
            let s2 = left.min.y + left.height() * 0.76;
            let chart = Rect::from_min_max(left.min, Pos2::new(left.max.x, s1 - 3.0));
            let heat = Rect::from_min_max(Pos2::new(left.min.x, s1 + 3.0), Pos2::new(left.max.x, s2 - 3.0));
            let blotter = Rect::from_min_max(Pos2::new(left.min.x, s2 + 3.0), left.max);
            let ticket = Rect::from_min_max(right.min, Pos2::new(right.max.x, right.min.y + 196.0));
            let lsplit = ticket.max.y + 6.0 + (right.max.y - ticket.max.y) * 0.62;
            let ladder = Rect::from_min_max(Pos2::new(right.min.x, ticket.max.y + 6.0), Pos2::new(right.max.x, lsplit - 3.0));
            let tape = Rect::from_min_max(Pos2::new(right.min.x, lsplit + 3.0), right.max);
            self.ticket(ui, ticket);
            self.blotter(ui, blotter);
            if let Some(rs) = rs {
                self.chart(ui, rs, chart);
                self.heat(ui, rs, heat);
            }
            self.ladder(ui, ladder);
            self.tape(ui, tape);
        });
    }

    fn login_form(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG)).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(egui::RichText::new("Sign in to TERM").size(20.0).color(TEXT));
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Paste the login token multivenue-terminal printed when it started.").color(MUTED));
                if self.link.state() == LinkState::LoginFailed {
                    ui.label(egui::RichText::new("That token was refused. Tokens work once; restart the server for a new one.").color(Color32::from_rgb(DOWN[0], DOWN[1], DOWN[2])));
                }
                ui.add_space(8.0);
                let te = ui.add(egui::TextEdit::singleline(&mut self.token).password(true).desired_width(320.0).hint_text("32-character token"));
                let go = ui.button("Sign in").clicked() || (te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go && self.token.len() == 32 {
                    let t = std::mem::take(&mut self.token);
                    self.link.login(&t);
                }
            });
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, r: Rect) {
        let p = ui.painter_at(r);
        let mut x = r.min.x + 8.0;
        for k in 0..self.ins.len() {
            let Some(d) = self.ins[k].def else { continue };
            let label = d.label.as_str().to_owned();
            let g = p.layout_no_wrap(label, FontId::proportional(13.0), if k == self.sel { TEXT } else { MUTED });
            let br = Rect::from_min_size(Pos2::new(x, r.min.y + 2.0), g.size() + Vec2::new(16.0, 6.0));
            let resp = ui.interact(br, ui.id().with(("tab", k)), Sense::click());
            if k == self.sel {
                p.rect_filled(br, 3.0, PANE);
            }
            p.galley(br.min + Vec2::new(8.0, 3.0), g, TEXT);
            if resp.clicked() && k != self.sel {
                self.sel = k;
                self.ins[k].heat_base.fill(i32::MIN);
            }
            x = br.max.x + 6.0;
        }
        let link = match self.link.state() {
            LinkState::Open => "live",
            LinkState::Connecting => "connecting",
            LinkState::Closed => "disconnected",
            _ => "signed out",
        };
        let eng = match &self.status {
            Some(s) if s.engine_up => "engine up",
            Some(_) => "engine down: order entry off",
            None => "no engine status",
        };
        let s = format!("{link} · {eng} · {} frames · {} dropped · {} bad", self.frames, self.dropped, self.bad);
        p.text(Pos2::new(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, s, FontId::monospace(11.0), MUTED);
    }

    fn chart(&mut self, ui: &mut egui::Ui, rs: &RenderState, rect: Rect) {
        let sel = self.sel;
        let (w, h) = (rect.width(), rect.height());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        let resp = ui.interact(rect, ui.id().with("chart"), Sense::click_and_drag());
        let Some(tick) = self.ins[sel].def.map(|d| d.tick_1e6) else { return };
        let (Some((&first, _)), Some((&last, _))) = (self.ins[sel].bars.first_key_value(), self.ins[sel].bars.last_key_value()) else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "No bars yet", FontId::proportional(13.0), MUTED);
            return;
        };
        let i = &mut self.ins[sel];
        let v = i.view.get_or_insert_with(|| Viewport::new(last - 120, 0, 8.0, 1.0));
        let d = resp.drag_delta();
        if d != Vec2::ZERO {
            v.pan_px(d.x, 0.0);
            i.follow = false;
        }
        if let Some(hp) = resp.hover_pos() {
            let z = ui.input(|x| x.smooth_scroll_delta.y);
            if z != 0.0 {
                v.zoom_x_at(hp.x - rect.min.x, (1.0 + z * 0.002).clamp(0.8, 1.25));
            }
        }
        if resp.double_clicked() {
            i.follow = true;
        }
        if i.follow {
            v.left = Pos::new(last + 4).shift(-w / v.px_per_bar);
        }
        let (b0, b1) = v.visible_bars(w);
        let (b0, b1) = (b0.max(first), b1.min(last));
        let (mut lo, mut hi) = (i64::MAX, i64::MIN);
        for (_, b) in i.bars.range(b0..=b1) {
            lo = lo.min(b.l / tick);
            hi = hi.max(b.h / tick);
        }
        if lo > hi {
            return;
        }
        v.fit_y(lo, hi, h * 0.8, 0.05);
        v.top = v.top.shift(h * 0.02 / v.px_per_tick);
        v.rebase_if_needed();
        let u = v.uniform(w, h);
        let vu = ViewU { ax: u.ax, bx: u.bx, ay: u.ay, by: u.by, w_px: w, h_px: h, _pad: [0.0; 2] };
        let v = *v;
        self.rects.clear();
        self.circles.clear();
        let (t_lo, t_hi) = v.visible_ticks(h);
        let n_grid = grid(t_lo, t_hi, nice_step(t_hi - t_lo, 8), &mut self.grid_ticks);
        let xmid = v.left.rel(v.origin_bar) + 0.5 * w / v.px_per_bar;
        for &t in &self.grid_ticks[..n_grid] {
            let y = v.off_tick(t);
            self.rects.push(RectInst { x: xmid, hw: 0.5 * w / v.px_per_bar + 2.0, y0: y, y1: y, min_px: 0.0, rgba: GRID });
        }
        let vmax = i.bars.range(b0..=b1).map(|(_, b)| b.vol).max().unwrap_or(1).max(1);
        let floor = v.off_tick(t_lo);
        let span = (t_hi - t_lo) as f32;
        for (&k, b) in i.bars.range(b0..=b1) {
            if self.rects.len() + 3 > term_render::MAX_RECTS {
                break;
            }
            let x = v.off_bar(k) + 0.5;
            let col = if b.c >= b.o { UP } else { DOWN };
            let mut vc = col;
            vc[3] = 0x50;
            self.rects.push(RectInst { x, hw: 0.36, y0: floor, y1: floor + span * 0.15 * (b.vol as f32 / vmax as f32), min_px: 0.0, rgba: vc });
            self.rects.push(RectInst { x, hw: 0.0, y0: v.off_tick(b.l / tick), y1: v.off_tick(b.h / tick), min_px: 0.5, rgba: col });
            self.rects.push(RectInst { x, hw: 0.36, y0: v.off_tick(b.o / tick), y1: v.off_tick(b.c / tick), min_px: 0.5, rgba: col });
        }
        let g = rs.renderer.read();
        if let Some(gpu) = g.callback_resources.get::<TermGpu>() {
            gpu.upload_pane(&rs.queue, 0, &vu, &self.rects, &self.circles);
        }
        drop(g);
        painter.add(egui_wgpu::Callback::new_paint_callback(rect, PaneDraw { pane: 0, n_rects: self.rects.len() as u32, ..PaneDraw::default() }));
        for &t in &self.grid_ticks[..n_grid] {
            let n = format_1e6(t * tick, decimals(tick), &mut self.fmt);
            let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
            painter.text(Pos2::new(rect.max.x - 6.0, rect.min.y + v.y_px(t) - 1.0), Align2::RIGHT_BOTTOM, s, FontId::monospace(10.5), MUTED);
        }
        if let Some(b) = i.bars.get(&last) {
            let n = format_1e6(b.c, decimals(tick), &mut self.fmt);
            let s = std::str::from_utf8(&self.fmt[..n]).unwrap_or("");
            let y = rect.min.y + v.y_px(b.c / tick);
            let tag = Rect::from_min_size(Pos2::new(rect.max.x - 72.0, y - 8.0), Vec2::new(72.0, 16.0));
            let c = if b.c >= b.o { UP } else { DOWN };
            painter.rect_filled(tag, 2.0, Color32::from_rgb(c[0], c[1], c[2]));
            painter.text(tag.center(), Align2::CENTER_CENTER, s, FontId::monospace(10.5), BG);
        }
    }

    fn heat(&mut self, ui: &mut egui::Ui, rs: &RenderState, rect: Rect) {
        let sel = self.sel;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x0C, 0x11, 0x17));
        let i = &self.ins[sel];
        let (Some(def), Some((&bb, _)), Some((&ba, _))) = (i.def, i.bids.last_key_value(), i.asks.first_key_value()) else { return };
        let tick = def.tick_1e6;
        let (w, h) = (rect.width(), rect.height());
        let col_px = 2.0f32;
        let rows = 256.0 * i.heat_scale.max(1) as f32;
        let ppt = h / rows;
        let mid = (bb + ba) / 2 / tick;
        let filled = i.heat_base.iter().filter(|b| **b != i32::MIN).count() as i32;
        let hu = HeatU {
            w_px: w,
            h_px: h,
            col_px,
            ticks_per_px: 1.0 / ppt,
            top_rel: rows / 2.0,
            contrast: 1.2,
            gamma: 0.8,
            _p0: 0.0,
            origin_tick: mid as i32,
            head: (i.heat_head.rem_euclid(HEAT_COLS as i64)) as i32,
            filled,
            tick_scale: i.heat_scale.max(1) as i32,
        };
        let g = rs.renderer.read();
        if let Some(gpu) = g.callback_resources.get::<TermGpu>() {
            gpu.upload_heat(&rs.queue, &hu);
        }
        drop(g);
        painter.add(egui_wgpu::Callback::new_paint_callback(rect, PaneDraw { pane: 1, heat: true, ..PaneDraw::default() }));
        painter.text(rect.min + Vec2::new(8.0, 6.0), Align2::LEFT_TOP, "depth heatmap", FontId::monospace(10.5), MUTED);
    }

    fn ladder(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        let i = &self.ins[self.sel];
        let Some(def) = i.def else { return };
        let dec = decimals(def.tick_1e6);
        let rh = 15.0f32;
        let rows = ((rect.height() / rh) as usize / 2).max(1);
        let maxq = i.bids.values().rev().take(rows).chain(i.asks.values().take(rows)).copied().max().unwrap_or(1).max(1);
        let font = FontId::monospace(10.5);
        let row = |k: usize, px: i64, q: i64, side: Side, fmt: &mut [u8; 28]| {
            let y = rect.min.y + k as f32 * rh;
            let c = if side == Side::Buy { UP } else { DOWN };
            let wbar = (q as f32 / maxq as f32) * rect.width() * 0.45;
            painter.rect_filled(Rect::from_min_size(Pos2::new(rect.max.x - wbar, y + 1.0), Vec2::new(wbar, rh - 2.0)), 0.0, Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 0x38));
            let n = format_1e6(px, dec, fmt);
            painter.text(Pos2::new(rect.min.x + 8.0, y + 1.0), Align2::LEFT_TOP, std::str::from_utf8(&fmt[..n]).unwrap_or(""), font.clone(), Color32::from_rgb(c[0], c[1], c[2]));
            let n = format_1e6(q, 4, fmt);
            painter.text(Pos2::new(rect.max.x - 8.0, y + 1.0), Align2::RIGHT_TOP, std::str::from_utf8(&fmt[..n]).unwrap_or(""), font.clone(), TEXT);
        };
        let asks: Vec<(i64, i64)> = i.asks.iter().take(rows).map(|(p, q)| (*p, *q)).collect();
        for (k, (p, q)) in asks.iter().rev().enumerate() {
            row(k, *p, *q, Side::Sell, &mut self.fmt);
        }
        for (k, (p, q)) in i.bids.iter().rev().take(rows).enumerate() {
            row(rows + k, *p, *q, Side::Buy, &mut self.fmt);
        }
        let f = i.flow;
        let s = format!("imb {:+.2}  ofi {:+}", f.imb_1e6 as f64 / 1e6, f.ofi_l1 / 1_000);
        painter.text(Pos2::new(rect.min.x + 8.0, rect.max.y - 4.0), Align2::LEFT_BOTTOM, s, FontId::monospace(10.0), MUTED);
    }

    fn tape(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, PANE);
        let i = &self.ins[self.sel];
        let dec = i.def.map(|d| decimals(d.tick_1e6)).unwrap_or(2);
        let rh = 14.0f32;
        for (k, (px, q, side)) in i.tape.iter().rev().take((rect.height() / rh) as usize).enumerate() {
            let y = rect.min.y + 4.0 + k as f32 * rh;
            let c = if *side == Side::Buy { UP } else { DOWN };
            let n = format_1e6(*px, dec, &mut self.fmt);
            painter.text(Pos2::new(rect.min.x + 8.0, y), Align2::LEFT_TOP, std::str::from_utf8(&self.fmt[..n]).unwrap_or(""), FontId::monospace(10.5), Color32::from_rgb(c[0], c[1], c[2]));
            let n = format_1e6(*q, 4, &mut self.fmt);
            painter.text(Pos2::new(rect.max.x - 8.0, y), Align2::RIGHT_TOP, std::str::from_utf8(&self.fmt[..n]).unwrap_or(""), FontId::monospace(10.5), TEXT);
        }
    }
}

impl<P: Platform, L: Link> App<P, L> {
    fn send_intent(&mut self, mut i: Intent) {
        self.nonce += 1;
        i.nonce = (self.plat.now_ms() as u64).wrapping_mul(1000).wrapping_add(self.nonce) | 1;
        match i.encode(0, 0, &mut self.out) {
            Ok(n) => {
                self.link.send(&self.out[..n]);
                self.entry_err = None;
            }
            Err(_) => self.entry_err = Some("that order is malformed (price and size must be above zero)"),
        }
    }

    fn place(&mut self, side: Side, ioc: bool) {
        let Some(def) = self.ins.get(self.sel).and_then(|i| i.def) else { return };
        let (Ok(px), Ok(qty)) = (term_proto::dec::parse_1e6(self.px_text.trim().as_bytes()), term_proto::dec::parse_1e6(self.qty_text.trim().as_bytes())) else {
            self.entry_err = Some("price and size: plain decimals, at most 6 places");
            return;
        };
        let mut i = Intent::blank(1, term_slot(def.venue), def.instr, Op::Place);
        i.side = side as u8;
        i.kind = if ioc { OrderKind::Ioc as u8 } else { OrderKind::Limit as u8 };
        i.flags = if self.reduce_only { FLAG_REDUCE_ONLY } else { 0 };
        i.px = px;
        i.qty = qty;
        self.send_intent(i);
    }

    fn simple(&mut self, op: Op) {
        let Some(def) = self.ins.get(self.sel).and_then(|i| i.def) else { return };
        self.send_intent(Intent::blank(1, term_slot(def.venue), def.instr, op));
    }

    fn ticket(&mut self, ui: &mut egui::Ui, rect: Rect) {
        ui.painter_at(rect).rect_filled(rect, 0.0, PANE);
        let inner = rect.shrink(8.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.horizontal(|ui| {
                let label = if self.trading { "Trading ON" } else { "Trading OFF" };
                let col = if self.trading { Color32::from_rgb(UP[0], UP[1], UP[2]) } else { MUTED };
                if ui.add(egui::Button::new(egui::RichText::new(label).color(col))).clicked() {
                    self.trading = !self.trading;
                    let op = if self.trading { CtlOp::TradingEnable } else { CtlOp::TradingDisable };
                    if let Ok(n) = Control::op(op).encode(0, 0, &mut self.out) {
                        self.link.send(&self.out[..n]);
                    }
                }
                ui.checkbox(&mut self.reduce_only, "reduce-only");
            });
            ui.horizontal(|ui| {
                ui.label("px");
                ui.add(egui::TextEdit::singleline(&mut self.px_text).desired_width(90.0));
                ui.label("qty");
                ui.add(egui::TextEdit::singleline(&mut self.qty_text).desired_width(70.0));
            });
            ui.add_enabled_ui(self.trading, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Buy limit").clicked() {
                        self.place(Side::Buy, false);
                    }
                    if ui.button("Sell limit").clicked() {
                        self.place(Side::Sell, false);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Buy IoC").clicked() {
                        self.place(Side::Buy, true);
                    }
                    if ui.button("Sell IoC").clicked() {
                        self.place(Side::Sell, true);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Cancel all").clicked() {
                        self.simple(Op::CancelAll);
                    }
                    if ui.button("Flatten").clicked() {
                        self.simple(Op::Flatten);
                    }
                    if ui.button(egui::RichText::new("Halt slot").color(Color32::from_rgb(DOWN[0], DOWN[1], DOWN[2]))).clicked() {
                        self.simple(Op::Halt);
                    }
                });
            });
            if let Some(e) = self.entry_err {
                ui.label(egui::RichText::new(e).color(Color32::from_rgb(DOWN[0], DOWN[1], DOWN[2])).size(11.0));
            }
        });
    }

    fn blotter(&mut self, ui: &mut egui::Ui, rect: Rect) {
        ui.painter_at(rect).rect_filled(rect, 0.0, PANE);
        let inner = rect.shrink(6.0);
        let mut cancel: Option<(u8, u16, [u8; 16])> = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
            ui.columns(3, |cols| {
                cols[0].label(egui::RichText::new("Orders").color(MUTED).size(11.0));
                for o in self.orders.values() {
                    cols[0].horizontal(|ui| {
                        let n = format_1e6(o.px, 2, &mut self.fmt);
                        let px = std::str::from_utf8(&self.fmt[..n]).unwrap_or("").to_owned();
                        let side = if o.side == Side::Buy { "B" } else { "S" };
                        ui.label(egui::RichText::new(format!("{side} {px} ×{} s{}", o.qty, o.slot)).size(11.0).monospace());
                        if ui.small_button("✕").clicked() {
                            cancel = Some((o.slot, o.instr, o.cloid));
                        }
                    });
                }
                cols[1].label(egui::RichText::new("Positions · fills").color(MUTED).size(11.0));
                for p in self.positions.values().filter(|p| p.qty != 0) {
                    cols[1].label(egui::RichText::new(format!("s{} #{} {:+} @ {}", p.slot, p.instr, p.qty, p.avg_px / 1_000_000)).size(11.0).monospace());
                }
                for f in self.fills.iter().rev().take(6) {
                    let side = if f.side == Side::Buy { "B" } else { "S" };
                    cols[1].label(egui::RichText::new(format!("fill {side} {} ×{}", f.px / 1_000_000, f.qty)).size(11.0).monospace().color(MUTED));
                }
                cols[2].label(egui::RichText::new("Acks").color(MUTED).size(11.0));
                for a in self.acks.iter().rev().take(8) {
                    let col = if a.accepted { Color32::from_rgb(UP[0], UP[1], UP[2]) } else { Color32::from_rgb(DOWN[0], DOWN[1], DOWN[2]) };
                    cols[2].label(egui::RichText::new(format!("#{} {}", a.nonce % 100_000, ack_reason(a))).size(11.0).color(col));
                }
            });
        });
        if let Some((slot, instr, cloid)) = cancel {
            let mut i = Intent::blank(1, slot, instr, Op::Cancel);
            i.target = cloid;
            self.send_intent(i);
        }
    }
}

/// Decimals to show for a tick (×1e6): 1.0 → 0, 0.1 → 1, 0.01 → 2 …
pub fn decimals(tick_1e6: i64) -> u32 {
    let mut d = 6;
    let mut t = tick_1e6.max(1);
    while t % 10 == 0 && d > 0 {
        t /= 10;
        d -= 1;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_and_reasons() {
        assert_eq!(term_slot(Venue::Hyperliquid), 12);
        assert_eq!(term_slot(Venue::Hypercall), 15);
        let a = IntentAck { nonce: 1, slot: 12, accepted: false, source: AckSource::Engine, reason: 7 };
        assert_eq!(ack_reason(&a), "outside the price band");
        let a = IntentAck { nonce: 1, slot: 12, accepted: false, source: AckSource::Server, reason: 2 };
        assert_eq!(ack_reason(&a), "trading is off for this session");
    }

    #[test]
    fn decimals_follow_the_tick() {
        assert_eq!(decimals(1_000_000), 0);
        assert_eq!(decimals(100_000), 1);
        assert_eq!(decimals(10_000), 2);
        assert_eq!(decimals(1), 6);
        assert_eq!(decimals(250_000), 2);
    }
}
