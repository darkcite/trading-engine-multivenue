// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-proto` — the shared wire between `multivenue-terminal` and `term-ui`
//! (TERM plan v2.1 §4.11.2). `#![no_std]`, zero dependencies, no `unsafe`.
//!
//! A frame is an 18-byte header (`kind u8 · ver u8 · len u32 · seq u32 ·
//! ts_ns u64`) and a fixed-layout little-endian body of `len` bytes, never
//! more than 64 KiB in total. Decoding is a borrowed view over the bytes:
//! fixed fields are read with explicit endianness, arrays stay in place and
//! are read element by element. Every kind has one parser, and `decode`
//! runs it, so a frame that decodes is a frame whose every field is valid.
//!
//! No free-form text is ever interpreted. The only strings are bounded,
//! length-prefixed printable-ASCII fields (`Ascii`), checked here and
//! checked again against an allow-list or an exact value by the server.
#![no_std]
#![forbid(unsafe_code)]

pub mod chunk;
pub mod ctl;
pub mod dec;
pub mod exec;
pub mod md;

/// Wire version. A replay spool of another version is refused (§4.6).
pub const VER: u8 = 1;
/// A frame never exceeds one WS slot.
pub const MAX_FRAME: usize = 64 * 1024;
pub const HDR_LEN: usize = 18;
pub const MAX_BODY: usize = MAX_FRAME - HDR_LEN;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The buffer ends before the frame or field does.
    Short,
    /// The frame would exceed 64 KiB, or an output buffer is too small.
    TooLong,
    Version,
    UnknownKind,
    /// The body length does not match the kind's layout.
    BadLen,
    /// A field holds a value outside its domain.
    BadValue,
    /// A chunk arrived out of order: resync.
    Gap,
    /// The kind is not one the UI may send.
    Direction,
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    BookSnap = 1,
    BookDelta = 2,
    TradeBatch = 3,
    Bar = 4,
    FootprintCells = 5,
    VapArray = 6,
    FlowPoint = 7,
    HeatCol = 8,
    IndicatorPoint = 9,
    Marker = 10,
    OrderView = 11,
    PositionView = 12,
    FillView = 13,
    EngineStatus = 14,
    Intent = 15,
    Control = 16,
    Heartbeat = 17,
    Echo = 18,
    Layout = 19,
    Drawing = 20,
    KeyMap = 21,
    LiveConfirm = 22,
    InstrDef = 23,
    /// Server → UI: the verdict on one intent (additive in wire v1).
    IntentAck = 24,
}

impl Kind {
    pub const ALL: [Kind; 24] = [
        Kind::BookSnap,
        Kind::BookDelta,
        Kind::TradeBatch,
        Kind::Bar,
        Kind::FootprintCells,
        Kind::VapArray,
        Kind::FlowPoint,
        Kind::HeatCol,
        Kind::IndicatorPoint,
        Kind::Marker,
        Kind::OrderView,
        Kind::PositionView,
        Kind::FillView,
        Kind::EngineStatus,
        Kind::Intent,
        Kind::Control,
        Kind::Heartbeat,
        Kind::Echo,
        Kind::Layout,
        Kind::Drawing,
        Kind::KeyMap,
        Kind::LiveConfirm,
        Kind::InstrDef,
        Kind::IntentAck,
    ];

    pub const fn from_u8(v: u8) -> Option<Kind> {
        if v >= 1 && v <= 24 {
            Some(Kind::ALL[(v - 1) as usize])
        } else {
            None
        }
    }

    /// Kinds the server accepts from a browser. Everything else is
    /// server → UI only, and a UI that sends it is cut off.
    pub const fn ui_may_send(self) -> bool {
        matches!(
            self,
            Kind::Intent | Kind::Control | Kind::Echo | Kind::Layout | Kind::Drawing | Kind::KeyMap | Kind::LiveConfirm
        )
    }
}

// ---- cursors -------------------------------------------------------------

/// A bounds-checked little-endian writer over a caller-owned buffer.
pub struct Wr<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl<'a> Wr<'a> {
    pub fn new(b: &'a mut [u8]) -> Wr<'a> {
        Wr { b, n: 0 }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.n
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    #[inline]
    pub fn bytes(&mut self, v: &[u8]) -> Result<()> {
        let end = self.n.checked_add(v.len()).ok_or(Error::TooLong)?;
        if end > self.b.len() {
            return Err(Error::TooLong);
        }
        self.b[self.n..end].copy_from_slice(v);
        self.n = end;
        Ok(())
    }
    #[inline]
    pub fn u8(&mut self, v: u8) -> Result<()> {
        self.bytes(&[v])
    }
    #[inline]
    pub fn u16(&mut self, v: u16) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    #[inline]
    pub fn u32(&mut self, v: u32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    #[inline]
    pub fn u64(&mut self, v: u64) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    #[inline]
    pub fn i32(&mut self, v: i32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    #[inline]
    pub fn i64(&mut self, v: i64) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
}

/// A bounds-checked little-endian reader over a borrowed body.
#[derive(Copy, Clone)]
pub struct Rd<'a> {
    b: &'a [u8],
    n: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, n: 0 }
    }
    #[inline]
    pub fn bytes(&mut self, k: usize) -> Result<&'a [u8]> {
        let end = self.n.checked_add(k).ok_or(Error::BadLen)?;
        if end > self.b.len() {
            return Err(Error::BadLen);
        }
        let s = &self.b[self.n..end];
        self.n = end;
        Ok(s)
    }
    #[inline]
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }
    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }
    #[inline]
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    #[inline]
    pub fn cloid(&mut self) -> Result<[u8; 16]> {
        self.arr()
    }
    /// The body must be consumed exactly.
    #[inline]
    pub fn end(&self) -> Result<()> {
        if self.n == self.b.len() {
            Ok(())
        } else {
            Err(Error::BadLen)
        }
    }
    #[inline]
    pub fn rest(&self) -> usize {
        self.b.len() - self.n
    }
}

/// Read a little-endian i64 at `o` of a slice already length-checked.
#[inline]
pub(crate) fn le_i64(b: &[u8], o: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    i64::from_le_bytes(a)
}
#[inline]
pub(crate) fn le_u64(b: &[u8], o: usize) -> u64 {
    le_i64(b, o) as u64
}

// ---- shared field types --------------------------------------------------

/// Side of a book level, a print's aggressor, or an order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Buy = 1,
    Sell = 2,
}

impl Side {
    pub const fn from_u8(v: u8) -> Result<Side> {
        match v {
            1 => Ok(Side::Buy),
            2 => Ok(Side::Sell),
            _ => Err(Error::BadValue),
        }
    }
}

/// A bounded printable-ASCII field: `len u8 · N bytes`, zero-padded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ascii<const N: usize> {
    len: u8,
    b: [u8; N],
}

impl<const N: usize> Ascii<N> {
    pub const WIRE: usize = 1 + N;

    /// Refuses anything longer than `N` or outside 0x20..=0x7e.
    pub fn new(s: &[u8]) -> Result<Ascii<N>> {
        if s.len() > N || N > 255 || !s.iter().all(|c| (0x20..=0x7e).contains(c)) {
            return Err(Error::BadValue);
        }
        let mut b = [0u8; N];
        b[..s.len()].copy_from_slice(s);
        Ok(Ascii { len: s.len() as u8, b })
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..self.len as usize]
    }
    pub fn as_str(&self) -> &str {
        // Printable ASCII is valid UTF-8 by construction.
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
    pub fn put(&self, w: &mut Wr<'_>) -> Result<()> {
        w.u8(self.len)?;
        w.bytes(&self.b)
    }
    pub fn get(r: &mut Rd<'_>) -> Result<Ascii<N>> {
        let len = r.u8()? as usize;
        let raw = r.bytes(N)?;
        if len > N || raw[len..].iter().any(|&c| c != 0) {
            return Err(Error::BadValue);
        }
        Ascii::new(&raw[..len])
    }
}

/// The header of a chunked payload: `total u32 · offset u32 · count u16 ·
/// last u8`, in elements. The client assembles chunks in order into its
/// preallocated array and applies the result only on `last` (§4.11.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ChunkHdr {
    pub total: u32,
    pub offset: u32,
    pub count: u16,
    pub last: bool,
}

impl ChunkHdr {
    pub const WIRE: usize = 11;

    pub fn put(&self, w: &mut Wr<'_>) -> Result<()> {
        w.u32(self.total)?;
        w.u32(self.offset)?;
        w.u16(self.count)?;
        w.u8(self.last as u8)
    }

    /// Checks `offset + count ≤ total ≤ max` and that `last` ⇔ the chunk
    /// reaches `total`.
    pub fn get(r: &mut Rd<'_>, max: u32) -> Result<ChunkHdr> {
        let total = r.u32()?;
        let offset = r.u32()?;
        let count = r.u16()?;
        let last = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(Error::BadValue),
        };
        let end = offset.checked_add(count as u32).ok_or(Error::BadValue)?;
        if total > max || end > total || last != (end == total) {
            return Err(Error::BadValue);
        }
        Ok(ChunkHdr { total, offset, count, last })
    }
}

// ---- frames --------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub len: u32,
    pub seq: u32,
    pub ts_ns: u64,
}

/// A validated frame: the header plus a borrowed body. Never copies.
#[derive(Copy, Clone, Debug)]
pub struct Frame<'a> {
    pub hdr: Header,
    pub body: &'a [u8],
}

/// Encode one frame: the closure writes the body after the header slot,
/// then the header is filled in with the body's length.
pub fn encode<F>(kind: Kind, seq: u32, ts_ns: u64, out: &mut [u8], body: F) -> Result<usize>
where
    F: FnOnce(&mut Wr<'_>) -> Result<()>,
{
    if out.len() < HDR_LEN {
        return Err(Error::TooLong);
    }
    let cap = out.len().min(MAX_FRAME);
    let (h, rest) = out[..cap].split_at_mut(HDR_LEN);
    let mut w = Wr::new(rest);
    body(&mut w)?;
    let len = w.len();
    h[0] = kind as u8;
    h[1] = VER;
    h[2..6].copy_from_slice(&(len as u32).to_le_bytes());
    h[6..10].copy_from_slice(&seq.to_le_bytes());
    h[10..18].copy_from_slice(&ts_ns.to_le_bytes());
    Ok(HDR_LEN + len)
}

/// Validate one frame at the start of `buf`: the header, the length, and
/// every field of the body. Returns the frame and its total length.
pub fn decode(buf: &[u8]) -> Result<(Frame<'_>, usize)> {
    if buf.len() < HDR_LEN {
        return Err(Error::Short);
    }
    let kind = Kind::from_u8(buf[0]).ok_or(Error::UnknownKind)?;
    if buf[1] != VER {
        return Err(Error::Version);
    }
    let len = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let total = (len as usize).checked_add(HDR_LEN).ok_or(Error::TooLong)?;
    if total > MAX_FRAME {
        return Err(Error::TooLong);
    }
    if buf.len() < total {
        return Err(Error::Short);
    }
    let body = &buf[HDR_LEN..total];
    validate(kind, body)?;
    let seq = u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]);
    let ts_ns = le_u64(buf, 10);
    Ok((Frame { hdr: Header { kind, len, seq, ts_ns }, body }, total))
}

/// The server's entry point for browser bytes: `decode` plus the
/// direction check (§3: the UI may send only intents and its own records).
pub fn decode_from_ui(buf: &[u8]) -> Result<(Frame<'_>, usize)> {
    if buf.len() >= HDR_LEN {
        match Kind::from_u8(buf[0]) {
            Some(k) if !k.ui_may_send() => return Err(Error::Direction),
            _ => {}
        }
    }
    decode(buf)
}

/// Run the kind's parser over the body, discarding the result.
pub fn validate(kind: Kind, body: &[u8]) -> Result<()> {
    match kind {
        Kind::BookSnap => md::BookSnap::parse(body).map(drop),
        Kind::BookDelta => md::BookDelta::parse(body).map(drop),
        Kind::TradeBatch => md::TradeBatch::parse(body).map(drop),
        Kind::Bar => md::Bar::parse(body).map(drop),
        Kind::FootprintCells => md::Footprint::parse(body).map(drop),
        Kind::VapArray => md::Vap::parse(body).map(drop),
        Kind::FlowPoint => md::FlowPoint::parse(body).map(drop),
        Kind::HeatCol => md::HeatCol::parse(body).map(drop),
        Kind::IndicatorPoint => md::IndicatorPoint::parse(body).map(drop),
        Kind::Marker => md::Marker::parse(body).map(drop),
        Kind::OrderView => exec::OrderView::parse(body).map(drop),
        Kind::PositionView => exec::PositionView::parse(body).map(drop),
        Kind::FillView => exec::FillView::parse(body).map(drop),
        Kind::EngineStatus => exec::EngineStatus::parse(body).map(drop),
        Kind::Intent => exec::Intent::parse(body).map(drop),
        Kind::LiveConfirm => exec::LiveConfirm::parse(body).map(drop),
        Kind::Control => ctl::Control::parse(body).map(drop),
        Kind::Heartbeat | Kind::Echo => ctl::Beat::parse(body).map(drop),
        Kind::Layout => ctl::Layout::parse(body).map(drop),
        Kind::Drawing => ctl::Drawing::parse(body).map(drop),
        Kind::KeyMap => ctl::KeyMap::parse(body).map(drop),
        Kind::InstrDef => ctl::InstrDef::parse(body).map(drop),
        Kind::IntentAck => exec::IntentAck::parse(body).map(drop),
    }
}
