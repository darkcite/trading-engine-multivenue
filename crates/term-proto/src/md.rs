// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Market-data frames (server → UI). Prices and sizes are ×1e6 integers
//! (T-L8); `instr` is the allow-list index announced by `InstrDef`.

use crate::{encode, le_i64, le_u64, ChunkHdr, Error, Kind, Rd, Result, Side, Wr};

/// Levels per side in a book snapshot (§4.11.2: K ≤ 512).
pub const MAX_BOOK_LEVELS: u32 = 512;
pub const MAX_DELTAS: usize = 256;
pub const MAX_TRADES: usize = 256;
pub const MAX_FOOTPRINT_CELLS: u32 = 4096;
/// 24 h of VAP at a fine bucket (≈ 30k) with headroom.
pub const MAX_VAP_BUCKETS: u32 = 1 << 20;
pub const MAX_HEAT_ROWS: usize = 4096;
pub const MAX_IND_VALUES: usize = 4;
pub const MAX_SLOT: u8 = 15;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Level {
    pub px: i64,
    pub qty: i64,
}

fn chunk_body_ok(r: &Rd<'_>, ch: &ChunkHdr, stride: usize) -> Result<()> {
    if ch.count as usize * stride != r.rest() {
        return Err(Error::BadLen);
    }
    Ok(())
}

// ---- BookSnap (chunked) --------------------------------------------------

#[derive(Copy, Clone, Debug)]
pub struct BookSnap<'a> {
    pub instr: u16,
    pub side: Side,
    pub chunk: ChunkHdr,
    raw: &'a [u8],
}

impl<'a> BookSnap<'a> {
    pub const STRIDE: usize = 16;

    pub fn parse(body: &'a [u8]) -> Result<BookSnap<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let side = Side::from_u8(r.u8()?)?;
        let chunk = ChunkHdr::get(&mut r, MAX_BOOK_LEVELS)?;
        chunk_body_ok(&r, &chunk, Self::STRIDE)?;
        let raw = r.bytes(r.rest())?;
        let s = BookSnap { instr, side, chunk, raw };
        for i in 0..s.len() {
            let l = s.level(i);
            if l.px <= 0 || l.qty < 0 {
                return Err(Error::BadValue);
            }
        }
        Ok(s)
    }
    pub fn len(&self) -> usize {
        self.chunk.count as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn level(&self, i: usize) -> Level {
        let o = i * Self::STRIDE;
        Level { px: le_i64(self.raw, o), qty: le_i64(self.raw, o + 8) }
    }
    pub fn encode(instr: u16, side: Side, chunk: ChunkHdr, levels: &[Level], seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if levels.len() != chunk.count as usize {
            return Err(Error::BadLen);
        }
        encode(Kind::BookSnap, seq, ts, out, |w| {
            w.u16(instr)?;
            w.u8(side as u8)?;
            chunk.put(w)?;
            for l in levels {
                w.i64(l.px)?;
                w.i64(l.qty)?;
            }
            Ok(())
        })
    }
}

// ---- BookDelta -----------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    pub side: Side,
    pub px: i64,
    /// New size at the level; 0 removes it.
    pub qty: i64,
}

#[derive(Copy, Clone, Debug)]
pub struct BookDelta<'a> {
    pub instr: u16,
    raw: &'a [u8],
}

impl<'a> BookDelta<'a> {
    pub const STRIDE: usize = 17;

    pub fn parse(body: &'a [u8]) -> Result<BookDelta<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let n = r.u16()? as usize;
        if n == 0 || n > MAX_DELTAS || r.rest() != n * Self::STRIDE {
            return Err(Error::BadLen);
        }
        let raw = r.bytes(r.rest())?;
        let s = BookDelta { instr, raw };
        for i in 0..n {
            Side::from_u8(raw[i * Self::STRIDE])?;
            let d = s.get(i);
            if d.px <= 0 || d.qty < 0 {
                return Err(Error::BadValue);
            }
        }
        Ok(s)
    }
    pub fn len(&self) -> usize {
        self.raw.len() / Self::STRIDE
    }
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
    pub fn get(&self, i: usize) -> Delta {
        let o = i * Self::STRIDE;
        let side = if self.raw[o] == 2 { Side::Sell } else { Side::Buy };
        Delta { side, px: le_i64(self.raw, o + 1), qty: le_i64(self.raw, o + 9) }
    }
    pub fn encode(instr: u16, d: &[Delta], seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if d.is_empty() || d.len() > MAX_DELTAS {
            return Err(Error::BadLen);
        }
        encode(Kind::BookDelta, seq, ts, out, |w| {
            w.u16(instr)?;
            w.u16(d.len() as u16)?;
            for x in d {
                w.u8(x.side as u8)?;
                w.i64(x.px)?;
                w.i64(x.qty)?;
            }
            Ok(())
        })
    }
}

// ---- TradeBatch ----------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub ts_ns: u64,
    pub px: i64,
    pub qty: i64,
    pub aggressor: Side,
}

#[derive(Copy, Clone, Debug)]
pub struct TradeBatch<'a> {
    pub instr: u16,
    raw: &'a [u8],
}

impl<'a> TradeBatch<'a> {
    pub const STRIDE: usize = 25;

    pub fn parse(body: &'a [u8]) -> Result<TradeBatch<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let n = r.u16()? as usize;
        if n == 0 || n > MAX_TRADES || r.rest() != n * Self::STRIDE {
            return Err(Error::BadLen);
        }
        let raw = r.bytes(r.rest())?;
        let s = TradeBatch { instr, raw };
        for i in 0..n {
            Side::from_u8(raw[i * Self::STRIDE + 24])?;
            let t = s.get(i);
            if t.px <= 0 || t.qty <= 0 {
                return Err(Error::BadValue);
            }
        }
        Ok(s)
    }
    pub fn len(&self) -> usize {
        self.raw.len() / Self::STRIDE
    }
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
    pub fn get(&self, i: usize) -> Trade {
        let o = i * Self::STRIDE;
        let aggressor = if self.raw[o + 24] == 2 { Side::Sell } else { Side::Buy };
        Trade { ts_ns: le_u64(self.raw, o), px: le_i64(self.raw, o + 8), qty: le_i64(self.raw, o + 16), aggressor }
    }
    pub fn encode(instr: u16, t: &[Trade], seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if t.is_empty() || t.len() > MAX_TRADES {
            return Err(Error::BadLen);
        }
        encode(Kind::TradeBatch, seq, ts, out, |w| {
            w.u16(instr)?;
            w.u16(t.len() as u16)?;
            for x in t {
                w.u64(x.ts_ns)?;
                w.i64(x.px)?;
                w.i64(x.qty)?;
                w.u8(x.aggressor as u8)?;
            }
            Ok(())
        })
    }
}

// ---- Bar -----------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Bar {
    pub instr: u16,
    pub tf_s: u32,
    /// Bar index on the timeframe (open time / tf).
    pub bar: i64,
    pub o: i64,
    pub h: i64,
    pub l: i64,
    pub c: i64,
    pub vol: i64,
    /// Buy minus sell aggressor volume.
    pub delta: i64,
}

impl Bar {
    pub const LEN: usize = 2 + 4 + 8 * 8;

    pub fn parse(body: &[u8]) -> Result<Bar> {
        let mut r = Rd::new(body);
        let b = Bar {
            instr: r.u16()?,
            tf_s: r.u32()?,
            bar: r.i64()?,
            o: r.i64()?,
            h: r.i64()?,
            l: r.i64()?,
            c: r.i64()?,
            vol: r.i64()?,
            delta: r.i64()?,
        };
        r.end()?;
        let ok = b.tf_s > 0
            && b.l <= b.h
            && (b.l..=b.h).contains(&b.o)
            && (b.l..=b.h).contains(&b.c)
            && b.vol >= 0
            && b.delta.checked_abs().is_some_and(|d| d <= b.vol);
        if ok {
            Ok(b)
        } else {
            Err(Error::BadValue)
        }
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::Bar, seq, ts, out, |w| {
            w.u16(self.instr)?;
            w.u32(self.tf_s)?;
            for v in [self.bar, self.o, self.h, self.l, self.c, self.vol, self.delta] {
                w.i64(v)?;
            }
            Ok(())
        })
    }
}

// ---- FootprintCells (chunked) -------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct FpCell {
    pub px: i64,
    pub bid_vol: i64,
    pub ask_vol: i64,
}

#[derive(Copy, Clone, Debug)]
pub struct Footprint<'a> {
    pub instr: u16,
    pub tf_s: u32,
    pub bar: i64,
    pub chunk: ChunkHdr,
    raw: &'a [u8],
}

impl<'a> Footprint<'a> {
    pub const STRIDE: usize = 24;

    pub fn parse(body: &'a [u8]) -> Result<Footprint<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let tf_s = r.u32()?;
        let bar = r.i64()?;
        let chunk = ChunkHdr::get(&mut r, MAX_FOOTPRINT_CELLS)?;
        chunk_body_ok(&r, &chunk, Self::STRIDE)?;
        if tf_s == 0 {
            return Err(Error::BadValue);
        }
        let raw = r.bytes(r.rest())?;
        let s = Footprint { instr, tf_s, bar, chunk, raw };
        for i in 0..s.len() {
            let c = s.cell(i);
            if c.px <= 0 || c.bid_vol < 0 || c.ask_vol < 0 {
                return Err(Error::BadValue);
            }
        }
        Ok(s)
    }
    pub fn len(&self) -> usize {
        self.chunk.count as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn cell(&self, i: usize) -> FpCell {
        let o = i * Self::STRIDE;
        FpCell { px: le_i64(self.raw, o), bid_vol: le_i64(self.raw, o + 8), ask_vol: le_i64(self.raw, o + 16) }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(instr: u16, tf_s: u32, bar: i64, chunk: ChunkHdr, cells: &[FpCell], seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if cells.len() != chunk.count as usize {
            return Err(Error::BadLen);
        }
        encode(Kind::FootprintCells, seq, ts, out, |w| {
            w.u16(instr)?;
            w.u32(tf_s)?;
            w.i64(bar)?;
            chunk.put(w)?;
            for c in cells {
                w.i64(c.px)?;
                w.i64(c.bid_vol)?;
                w.i64(c.ask_vol)?;
            }
            Ok(())
        })
    }
}

// ---- VapArray (chunked) -------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct VapCell {
    pub bid_vol: i64,
    pub ask_vol: i64,
}

#[derive(Copy, Clone, Debug)]
pub struct Vap<'a> {
    pub instr: u16,
    /// Price of bucket 0 and the bucket height, ×1e6.
    pub base_px: i64,
    pub step_px: i64,
    pub chunk: ChunkHdr,
    raw: &'a [u8],
}

impl<'a> Vap<'a> {
    pub const STRIDE: usize = 16;

    pub fn parse(body: &'a [u8]) -> Result<Vap<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let base_px = r.i64()?;
        let step_px = r.i64()?;
        let chunk = ChunkHdr::get(&mut r, MAX_VAP_BUCKETS)?;
        chunk_body_ok(&r, &chunk, Self::STRIDE)?;
        if step_px <= 0 {
            return Err(Error::BadValue);
        }
        let raw = r.bytes(r.rest())?;
        let s = Vap { instr, base_px, step_px, chunk, raw };
        for i in 0..s.len() {
            let c = s.cell(i);
            if c.bid_vol < 0 || c.ask_vol < 0 {
                return Err(Error::BadValue);
            }
        }
        Ok(s)
    }
    pub fn len(&self) -> usize {
        self.chunk.count as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn cell(&self, i: usize) -> VapCell {
        let o = i * Self::STRIDE;
        VapCell { bid_vol: le_i64(self.raw, o), ask_vol: le_i64(self.raw, o + 8) }
    }
    /// The chunk's raw cells, for in-order assembly (`chunk::Assembler`).
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(instr: u16, base_px: i64, step_px: i64, chunk: ChunkHdr, cells: &[VapCell], seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if cells.len() != chunk.count as usize {
            return Err(Error::BadLen);
        }
        encode(Kind::VapArray, seq, ts, out, |w| {
            w.u16(instr)?;
            w.i64(base_px)?;
            w.i64(step_px)?;
            chunk.put(w)?;
            for c in cells {
                w.i64(c.bid_vol)?;
                w.i64(c.ask_vol)?;
            }
            Ok(())
        })
    }
}

// ---- FlowPoint -----------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct FlowPoint {
    pub instr: u16,
    pub cvd: i64,
    pub ofi_l1: i64,
    pub ofi_k: i64,
    /// (B − A)/(B + A), ×1e6, within ±1e6.
    pub imb_1e6: i64,
    pub microprice: i64,
}

impl FlowPoint {
    pub fn parse(body: &[u8]) -> Result<FlowPoint> {
        let mut r = Rd::new(body);
        let f = FlowPoint { instr: r.u16()?, cvd: r.i64()?, ofi_l1: r.i64()?, ofi_k: r.i64()?, imb_1e6: r.i64()?, microprice: r.i64()? };
        r.end()?;
        if !(-1_000_000..=1_000_000).contains(&f.imb_1e6) || f.microprice < 0 {
            return Err(Error::BadValue);
        }
        Ok(f)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::FlowPoint, seq, ts, out, |w| {
            w.u16(self.instr)?;
            for v in [self.cvd, self.ofi_l1, self.ofi_k, self.imb_1e6, self.microprice] {
                w.i64(v)?;
            }
            Ok(())
        })
    }
}

// ---- HeatCol -------------------------------------------------------------

/// One heatmap column: u8 log-quantized depth over `rows` price rows from
/// `base_tick`, each `tick_scale` ticks tall.
#[derive(Copy, Clone, Debug)]
pub struct HeatCol<'a> {
    pub instr: u16,
    pub col: u32,
    pub base_tick: i32,
    pub tick_scale: u16,
    pub cells: &'a [u8],
}

impl<'a> HeatCol<'a> {
    pub fn parse(body: &'a [u8]) -> Result<HeatCol<'a>> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let col = r.u32()?;
        let base_tick = r.i32()?;
        let tick_scale = r.u16()?;
        let rows = r.u16()? as usize;
        if rows == 0 || rows > MAX_HEAT_ROWS || r.rest() != rows {
            return Err(Error::BadLen);
        }
        if tick_scale == 0 {
            return Err(Error::BadValue);
        }
        Ok(HeatCol { instr, col, base_tick, tick_scale, cells: r.bytes(rows)? })
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if self.cells.is_empty() || self.cells.len() > MAX_HEAT_ROWS || self.tick_scale == 0 {
            return Err(Error::BadValue);
        }
        encode(Kind::HeatCol, seq, ts, out, |w| {
            w.u16(self.instr)?;
            w.u32(self.col)?;
            w.i32(self.base_tick)?;
            w.u16(self.tick_scale)?;
            w.u16(self.cells.len() as u16)?;
            w.bytes(self.cells)
        })
    }
}

// ---- IndicatorPoint --------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct IndicatorPoint {
    pub instr: u16,
    pub ind: u16,
    pub bar: i64,
    pub n: u8,
    pub values: [i64; MAX_IND_VALUES],
}

impl IndicatorPoint {
    pub fn parse(body: &[u8]) -> Result<IndicatorPoint> {
        let mut r = Rd::new(body);
        let mut p = IndicatorPoint { instr: r.u16()?, ind: r.u16()?, bar: r.i64()?, n: r.u8()?, values: [0; MAX_IND_VALUES] };
        if p.n == 0 || p.n as usize > MAX_IND_VALUES {
            return Err(Error::BadValue);
        }
        for v in p.values.iter_mut().take(p.n as usize) {
            *v = r.i64()?;
        }
        r.end()?;
        Ok(p)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        if self.n == 0 || self.n as usize > MAX_IND_VALUES {
            return Err(Error::BadValue);
        }
        encode(Kind::IndicatorPoint, seq, ts, out, |w| {
            w.u16(self.instr)?;
            w.u16(self.ind)?;
            w.i64(self.bar)?;
            w.u8(self.n)?;
            for v in &self.values[..self.n as usize] {
                w.i64(*v)?;
            }
            Ok(())
        })
    }
}

// ---- Marker --------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MarkerKind {
    FillBuy = 1,
    FillSell = 2,
    Signal = 3,
    Liquidation = 4,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub instr: u16,
    pub slot: u8,
    pub kind: MarkerKind,
    pub bar: i64,
    pub px: i64,
    pub qty: i64,
}

impl Marker {
    pub fn parse(body: &[u8]) -> Result<Marker> {
        let mut r = Rd::new(body);
        let instr = r.u16()?;
        let slot = r.u8()?;
        let kind = match r.u8()? {
            1 => MarkerKind::FillBuy,
            2 => MarkerKind::FillSell,
            3 => MarkerKind::Signal,
            4 => MarkerKind::Liquidation,
            _ => return Err(Error::BadValue),
        };
        let m = Marker { instr, slot, kind, bar: r.i64()?, px: r.i64()?, qty: r.i64()? };
        r.end()?;
        if slot > MAX_SLOT || m.px <= 0 || m.qty < 0 {
            return Err(Error::BadValue);
        }
        Ok(m)
    }
    pub fn encode(&self, seq: u32, ts: u64, out: &mut [u8]) -> Result<usize> {
        encode(Kind::Marker, seq, ts, out, |w: &mut Wr<'_>| {
            w.u16(self.instr)?;
            w.u8(self.slot)?;
            w.u8(self.kind as u8)?;
            w.i64(self.bar)?;
            w.i64(self.px)?;
            w.i64(self.qty)
        })
    }
}
