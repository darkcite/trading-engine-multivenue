// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Control, liveness and typed records: `Control`, `Heartbeat`/`Echo`,
//! `Layout`, `Drawing`, `KeyMap`, `InstrDef`. Every record is typed binary;
//! the only strings are bounded `Ascii` fields.

use crate::md::MAX_SLOT;
use crate::{encode, Ascii, Error, Kind, Rd, Result};

// ---- Control ---------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CtlOp {
    /// Visibility (§4.11.3): the server moves a non-visible client to
    /// snapshot mode until `Visible` or `Resumed`.
    Hidden = 1,
    Visible = 2,
    Frozen = 3,
    Resumed = 4,
    /// The client's ring overflowed: resend snapshots.
    Stale = 5,
    /// `a` = instr, `b` = channel mask.
    Subscribe = 6,
    Unsubscribe = 7,
    /// Trading is off at login; these flip the session's switch (§4.11.6).
    TradingEnable = 8,
    TradingDisable = 9,
    /// Server → UI: the nonce a live confirm must quote (`c`), for slot `a`.
    ConfirmNonce = 10,
}

pub const CH_BOOK: u32 = 1;
pub const CH_TRADES: u32 = 2;
pub const CH_BARS: u32 = 4;
pub const CH_FOOTPRINT: u32 = 8;
pub const CH_VAP: u32 = 16;
pub const CH_FLOW: u32 = 32;
pub const CH_HEAT: u32 = 64;
pub const CH_IND: u32 = 128;
pub const CH_ALL: u32 = 255;

/// `op u8 · 0 u8 · a u16 · b u32 · c u64` — 16 bytes, fields an op does not
/// use must be zero.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Control {
    pub op: CtlOp,
    pub a: u16,
    pub b: u32,
    pub c: u64,
}

impl Control {
    pub const LEN: usize = 16;

    pub const fn op(op: CtlOp) -> Control {
        Control { op, a: 0, b: 0, c: 0 }
    }

    pub fn parse(body: &[u8]) -> Result<Control> {
        let mut r = Rd::new(body);
        let op = match r.u8()? {
            1 => CtlOp::Hidden,
            2 => CtlOp::Visible,
            3 => CtlOp::Frozen,
            4 => CtlOp::Resumed,
            5 => CtlOp::Stale,
            6 => CtlOp::Subscribe,
            7 => CtlOp::Unsubscribe,
            8 => CtlOp::TradingEnable,
            9 => CtlOp::TradingDisable,
            10 => CtlOp::ConfirmNonce,
            _ => return Err(Error::BadValue),
        };
        if r.u8()? != 0 {
            return Err(Error::BadValue);
        }
        let c = Control { op, a: r.u16()?, b: r.u32()?, c: r.u64()? };
        r.end()?;
        c.check()?;
        Ok(c)
    }

    fn check(&self) -> Result<()> {
        let ok = match self.op {
            CtlOp::Subscribe | CtlOp::Unsubscribe => self.b != 0 && self.b & !CH_ALL == 0 && self.c == 0,
            CtlOp::ConfirmNonce => self.a <= MAX_SLOT as u16 && self.b == 0 && self.c != 0,
            _ => self.a == 0 && self.b == 0 && self.c == 0,
        };
        if ok {
            Ok(())
        } else {
            Err(Error::BadValue)
        }
    }

    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        self.check()?;
        encode(Kind::Control, seq, ts, out, |w| {
            w.u8(self.op as u8)?;
            w.u8(0)?;
            w.u16(self.a)?;
            w.u32(self.b)?;
            w.u64(self.c)
        })
    }
}

// ---- Heartbeat / Echo -------------------------------------------------------

/// Server → UI `Heartbeat` every second; the UI answers with an `Echo` of
/// the same `n` from its socket handler. Three missed echoes or a closed
/// socket is "UI lost" (§4.6, R-T15).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Beat {
    pub n: u64,
    pub server_ns: u64,
}

impl Beat {
    pub fn parse(body: &[u8]) -> Result<Beat> {
        let mut r = Rd::new(body);
        let b = Beat { n: r.u64()?, server_ns: r.u64()? };
        r.end()?;
        Ok(b)
    }
    pub fn encode(&self, echo: bool, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        let k = if echo { Kind::Echo } else { Kind::Heartbeat };
        encode(k, seq, ts, out, |w| {
            w.u64(self.n)?;
            w.u64(self.server_ns)
        })
    }
}

// ---- Layout ------------------------------------------------------------------

pub const MAX_LAYOUT_NODES: usize = 32;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PanelKind {
    #[default]
    SplitH = 1,
    SplitV = 2,
    Chart = 3,
    Ladder = 4,
    Ticket = 5,
    Watchlist = 6,
    Blotter = 7,
    Tape = 8,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct LayoutNode {
    pub kind: PanelKind,
    /// Index of the parent node; the root's parent is itself (0).
    pub parent: u8,
    /// Share of the parent split, ‰ (1..=1000).
    pub share_permille: u16,
}

/// A layout preset: a panel tree in pre-order, saved on the server.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: Ascii<32>,
    pub n: u8,
    pub nodes: [LayoutNode; MAX_LAYOUT_NODES],
}

impl Layout {
    pub fn parse(body: &[u8]) -> Result<Layout> {
        let mut r = Rd::new(body);
        let name = Ascii::<32>::get(&mut r)?;
        let n = r.u8()?;
        if n == 0 || n as usize > MAX_LAYOUT_NODES || name.as_bytes().is_empty() {
            return Err(Error::BadValue);
        }
        let mut nodes = [LayoutNode::default(); MAX_LAYOUT_NODES];
        for (i, nd) in nodes.iter_mut().enumerate().take(n as usize) {
            let kind = match r.u8()? {
                1 => PanelKind::SplitH,
                2 => PanelKind::SplitV,
                3 => PanelKind::Chart,
                4 => PanelKind::Ladder,
                5 => PanelKind::Ticket,
                6 => PanelKind::Watchlist,
                7 => PanelKind::Blotter,
                8 => PanelKind::Tape,
                _ => return Err(Error::BadValue),
            };
            let parent = r.u8()?;
            let share_permille = r.u16()?;
            // Pre-order: every non-root parent comes earlier and is a split.
            let parent_ok = if i == 0 { parent == 0 } else { (parent as usize) < i };
            if !parent_ok || share_permille == 0 || share_permille > 1000 {
                return Err(Error::BadValue);
            }
            *nd = LayoutNode { kind, parent, share_permille };
        }
        r.end()?;
        for i in 1..n as usize {
            let p = nodes[nodes[i].parent as usize].kind;
            if !matches!(p, PanelKind::SplitH | PanelKind::SplitV) {
                return Err(Error::BadValue);
            }
        }
        Ok(Layout { name, n, nodes })
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::Layout, seq, ts, out, |w| {
            self.name.put(w)?;
            w.u8(self.n)?;
            for nd in &self.nodes[..(self.n as usize).min(MAX_LAYOUT_NODES)] {
                w.u8(nd.kind as u8)?;
                w.u8(nd.parent)?;
                w.u16(nd.share_permille)?;
            }
            Ok(())
        })
    }
}

// ---- Drawing -------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DrawKind {
    HLine = 1,
    Ray = 2,
    Trend = 3,
    Rect = 4,
    Measure = 5,
}

impl DrawKind {
    /// Points each kind is defined by.
    pub const fn points(self) -> u8 {
        match self {
            DrawKind::HLine => 1,
            DrawKind::Ray | DrawKind::Trend | DrawKind::Rect | DrawKind::Measure => 2,
        }
    }
}

/// A point on the chart: bar index and ×1e6 price — never screen space.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ChartPt {
    pub bar: i64,
    pub px: i64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Drawing {
    pub instr: u16,
    pub id: u32,
    pub kind: DrawKind,
    pub rgba: u32,
    pub pts: [ChartPt; 2],
}

impl Drawing {
    pub fn parse(body: &[u8]) -> Result<Drawing> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let id = r.u32()?;
        let kind = match r.u8()? {
            1 => DrawKind::HLine,
            2 => DrawKind::Ray,
            3 => DrawKind::Trend,
            4 => DrawKind::Rect,
            5 => DrawKind::Measure,
            _ => return Err(Error::BadValue),
        };
        let rgba = r.u32()?;
        let n = r.u8()?;
        if n != kind.points() || id == 0 {
            return Err(Error::BadValue);
        }
        let mut pts = [ChartPt::default(); 2];
        for p in pts.iter_mut().take(n as usize) {
            *p = ChartPt { bar: r.i64()?, px: r.i64()? };
            if p.px <= 0 {
                return Err(Error::BadValue);
            }
        }
        r.end()?;
        Ok(Drawing { instr, id, kind, rgba, pts })
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        let n = self.kind.points();
        encode(Kind::Drawing, seq, ts, out, |w| {
            w.u16(self.instr)?;
            w.u32(self.id)?;
            w.u8(self.kind as u8)?;
            w.u32(self.rgba)?;
            w.u8(n)?;
            for p in &self.pts[..n as usize] {
                w.i64(p.bar)?;
                w.i64(p.px)?;
            }
            Ok(())
        })
    }
}

// ---- KeyMap ----------------------------------------------------------------------

pub const MAX_KEYS: usize = 64;
pub const MOD_CTRL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_META: u8 = 8;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyAction {
    Size1 = 1,
    Size2 = 2,
    Size3 = 3,
    Size4 = 4,
    Size5 = 5,
    BuyIoc = 6,
    SellIoc = 7,
    CancelAll = 8,
    Flatten = 9,
    Kill = 10,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeyBind {
    /// egui key code.
    pub key: u16,
    pub mods: u8,
    pub action: KeyAction,
}

/// Hotkey bindings (`keys.toml` on the server). ⌘ chords are refused: the
/// browser reserves ⌘W/T/N/Q/L (§4.11.3).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeyMap {
    pub n: u8,
    pub binds: [KeyBind; MAX_KEYS],
}

impl KeyMap {
    const EMPTY: KeyBind = KeyBind { key: 0, mods: 0, action: KeyAction::Size1 };

    pub fn parse(body: &[u8]) -> Result<KeyMap> {
        let mut r = Rd::new(body);
        let n = r.u8()?;
        if n as usize > MAX_KEYS {
            return Err(Error::BadValue);
        }
        let mut binds = [Self::EMPTY; MAX_KEYS];
        for i in 0..n as usize {
            let key = r.u16()?;
            let mods = r.u8()?;
            let action = match r.u8()? {
                1 => KeyAction::Size1,
                2 => KeyAction::Size2,
                3 => KeyAction::Size3,
                4 => KeyAction::Size4,
                5 => KeyAction::Size5,
                6 => KeyAction::BuyIoc,
                7 => KeyAction::SellIoc,
                8 => KeyAction::CancelAll,
                9 => KeyAction::Flatten,
                10 => KeyAction::Kill,
                _ => return Err(Error::BadValue),
            };
            if mods & !(MOD_CTRL | MOD_SHIFT | MOD_ALT | MOD_META) != 0 || mods & MOD_META != 0 {
                return Err(Error::BadValue);
            }
            // Flatten and Kill need a two-modifier chord (⌃⇧).
            if matches!(action, KeyAction::Flatten | KeyAction::Kill) && mods & (MOD_CTRL | MOD_SHIFT) != (MOD_CTRL | MOD_SHIFT) {
                return Err(Error::BadValue);
            }
            if binds[..i].iter().any(|b| b.key == key && b.mods == mods) {
                return Err(Error::BadValue);
            }
            binds[i] = KeyBind { key, mods, action };
        }
        r.end()?;
        Ok(KeyMap { n, binds })
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::KeyMap, seq, ts, out, |w| {
            w.u8(self.n)?;
            for b in &self.binds[..(self.n as usize).min(MAX_KEYS)] {
                w.u16(b.key)?;
                w.u8(b.mods)?;
                w.u8(b.action as u8)?;
            }
            Ok(())
        })
    }
}

// ---- InstrDef --------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Venue {
    Hyperliquid = 1,
    Binance = 2,
    Polymarket = 3,
    Hypercall = 4,
}

pub const INSTR_IOC_ONLY: u8 = 1;
pub const INSTR_CHART_ONLY: u8 = 2;

/// One allow-listed instrument (R-T9): the index every other frame uses, its
/// venue, tick and lot (×1e6) and a display label.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InstrDef {
    pub instr: u16,
    pub venue: Venue,
    pub tick_1e6: i64,
    pub lot_1e6: i64,
    pub flags: u8,
    pub label: Ascii<24>,
}

impl InstrDef {
    pub fn parse(body: &[u8]) -> Result<InstrDef> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let venue = match r.u8()? {
            1 => Venue::Hyperliquid,
            2 => Venue::Binance,
            3 => Venue::Polymarket,
            4 => Venue::Hypercall,
            _ => return Err(Error::BadValue),
        };
        let d = InstrDef { instr, venue, tick_1e6: r.i64()?, lot_1e6: r.i64()?, flags: r.u8()?, label: Ascii::<24>::get(&mut r)? };
        r.end()?;
        // T-L8: an instrument below 1e-6 is refused.
        if d.tick_1e6 <= 0 || d.lot_1e6 <= 0 || d.flags & !(INSTR_IOC_ONLY | INSTR_CHART_ONLY) != 0 || d.label.as_bytes().is_empty() {
            return Err(Error::BadValue);
        }
        Ok(d)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::InstrDef, seq, ts, out, |w| {
            w.u16(self.instr)?;
            w.u8(self.venue as u8)?;
            w.i64(self.tick_1e6)?;
            w.i64(self.lot_1e6)?;
            w.u8(self.flags)?;
            self.label.put(w)
        })
    }
}
