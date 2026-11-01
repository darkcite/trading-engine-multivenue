// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `term-proto` — the shared wire between `multivenue-terminal` and `term-ui`.
//!
//! T0 subset (plan v2.1 §4.11.2): the frame header, `Bar`, `HeatCol`,
//! `Control`, and the `dec` parser. `#![no_std]`, zero dependencies.
//! Layouts are little-endian with fixed offsets; decoding is a borrowed view
//! over the bytes with unaligned-safe accessors (no `transmute`).
#![no_std]
#![forbid(unsafe_code)]

pub mod dec;

/// Wire version. A replay spool of another version is refused (§4.6).
pub const VER: u8 = 0;
/// A frame never exceeds one WS slot.
pub const MAX_FRAME: usize = 64 * 1024;
/// kind u8 · ver u8 · len u32 · seq u32 · ts_ns u64.
pub const HDR_LEN: usize = 18;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Bar = 4,
    HeatCol = 8,
    Control = 15,
}

impl Kind {
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Kind> {
        match v {
            4 => Some(Kind::Bar),
            8 => Some(Kind::HeatCol),
            15 => Some(Kind::Control),
            _ => None,
        }
    }
    /// Fixed body length, or `None` for variable bodies (checked per kind).
    #[inline]
    const fn fixed_body(self) -> Option<usize> {
        match self {
            Kind::Bar => Some(BAR_LEN),
            Kind::Control => Some(CONTROL_LEN),
            Kind::HeatCol => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Short,
    TooLong,
    Version,
    UnknownKind,
    BadLen,
    BadValue,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub len: u32,
    pub seq: u32,
    pub ts_ns: u64,
}

#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
#[inline]
fn rd_i64(b: &[u8], o: usize) -> i64 {
    rd_u64(b, o) as i64
}
#[inline]
fn rd_i32(b: &[u8], o: usize) -> i32 {
    rd_u32(b, o) as i32
}

/// A validated frame: header plus a borrowed body. Never copies.
#[derive(Copy, Clone, Debug)]
pub struct Frame<'a> {
    pub hdr: Header,
    pub body: &'a [u8],
}

/// Validate one frame at the start of `buf`; returns it and its total length.
pub fn decode(buf: &[u8]) -> Result<(Frame<'_>, usize), Error> {
    if buf.len() < HDR_LEN {
        return Err(Error::Short);
    }
    if buf[1] != VER {
        return Err(Error::Version);
    }
    let kind = Kind::from_u8(buf[0]).ok_or(Error::UnknownKind)?;
    let len = rd_u32(buf, 2);
    let total = HDR_LEN.checked_add(len as usize).ok_or(Error::TooLong)?;
    if total > MAX_FRAME {
        return Err(Error::TooLong);
    }
    if buf.len() < total {
        return Err(Error::Short);
    }
    let body = &buf[HDR_LEN..total];
    match kind.fixed_body() {
        Some(n) if n != body.len() => return Err(Error::BadLen),
        Some(_) => {}
        None => HeatColView::check(body)?,
    }
    if kind == Kind::Control && Control::from_u8(body[0]).is_none() {
        return Err(Error::BadValue);
    }
    let hdr = Header { kind, len, seq: rd_u32(buf, 6), ts_ns: rd_u64(buf, 10) };
    Ok((Frame { hdr, body }, total))
}

fn put_hdr(out: &mut [u8], kind: Kind, body_len: usize, seq: u32, ts_ns: u64) {
    out[0] = kind as u8;
    out[1] = VER;
    out[2..6].copy_from_slice(&(body_len as u32).to_le_bytes());
    out[6..10].copy_from_slice(&seq.to_le_bytes());
    out[10..18].copy_from_slice(&ts_ns.to_le_bytes());
}

// ---- Bar ---------------------------------------------------------------

/// ×1e6 prices, ×1e6 volumes. `bar` is the bar index on the timeframe.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Bar {
    pub bar: i64,
    pub tf_s: u32,
    pub o: i64,
    pub h: i64,
    pub l: i64,
    pub c: i64,
    pub vol: i64,
    pub delta: i64,
}
pub const BAR_LEN: usize = 8 + 4 + 8 * 6;

impl Bar {
    pub fn encode(&self, seq: u32, ts_ns: u64, out: &mut [u8]) -> Result<usize, Error> {
        let n = HDR_LEN + BAR_LEN;
        if out.len() < n {
            return Err(Error::Short);
        }
        put_hdr(out, Kind::Bar, BAR_LEN, seq, ts_ns);
        let b = &mut out[HDR_LEN..n];
        b[0..8].copy_from_slice(&self.bar.to_le_bytes());
        b[8..12].copy_from_slice(&self.tf_s.to_le_bytes());
        let mut o = 12;
        for v in [self.o, self.h, self.l, self.c, self.vol, self.delta] {
            b[o..o + 8].copy_from_slice(&v.to_le_bytes());
            o += 8;
        }
        Ok(n)
    }
    pub fn view(body: &[u8]) -> Bar {
        debug_assert_eq!(body.len(), BAR_LEN);
        Bar {
            bar: rd_i64(body, 0),
            tf_s: rd_u32(body, 8),
            o: rd_i64(body, 12),
            h: rd_i64(body, 20),
            l: rd_i64(body, 28),
            c: rd_i64(body, 36),
            vol: rd_i64(body, 44),
            delta: rd_i64(body, 52),
        }
    }
}

// ---- HeatCol -----------------------------------------------------------

/// `col u32 · base_tick i32 · tick_scale u16 · rows u16 · rows × u8`.
/// Intensities are log-quantized u8 over `rows` price rows starting at
/// `base_tick`, each row `tick_scale` ticks tall.
pub const HEATCOL_FIXED: usize = 4 + 4 + 2 + 2;
pub const HEATCOL_MAX_ROWS: usize = 4096;

#[derive(Copy, Clone, Debug)]
pub struct HeatColView<'a> {
    pub col: u32,
    pub base_tick: i32,
    pub tick_scale: u16,
    pub cells: &'a [u8],
}

impl<'a> HeatColView<'a> {
    fn check(body: &[u8]) -> Result<(), Error> {
        if body.len() < HEATCOL_FIXED {
            return Err(Error::BadLen);
        }
        let rows = u16::from_le_bytes([body[10], body[11]]) as usize;
        if rows == 0 || rows > HEATCOL_MAX_ROWS || body.len() != HEATCOL_FIXED + rows {
            return Err(Error::BadLen);
        }
        if u16::from_le_bytes([body[8], body[9]]) == 0 {
            return Err(Error::BadValue);
        }
        Ok(())
    }
    /// Body must have passed `decode`.
    pub fn view(body: &'a [u8]) -> HeatColView<'a> {
        HeatColView {
            col: rd_u32(body, 0),
            base_tick: rd_i32(body, 4),
            tick_scale: u16::from_le_bytes([body[8], body[9]]),
            cells: &body[HEATCOL_FIXED..],
        }
    }
    pub fn encode(
        col: u32,
        base_tick: i32,
        tick_scale: u16,
        cells: &[u8],
        seq: u32,
        ts_ns: u64,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        if cells.is_empty() || cells.len() > HEATCOL_MAX_ROWS || tick_scale == 0 {
            return Err(Error::BadValue);
        }
        let body = HEATCOL_FIXED + cells.len();
        let n = HDR_LEN + body;
        if out.len() < n {
            return Err(Error::Short);
        }
        put_hdr(out, Kind::HeatCol, body, seq, ts_ns);
        let b = &mut out[HDR_LEN..n];
        b[0..4].copy_from_slice(&col.to_le_bytes());
        b[4..8].copy_from_slice(&base_tick.to_le_bytes());
        b[8..10].copy_from_slice(&tick_scale.to_le_bytes());
        b[10..12].copy_from_slice(&(cells.len() as u16).to_le_bytes());
        b[12..].copy_from_slice(cells);
        Ok(n)
    }
}

// ---- Control (visibility subset) ---------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Control {
    Hidden = 1,
    Visible = 2,
    Frozen = 3,
    Resumed = 4,
    Stale = 5,
}
pub const CONTROL_LEN: usize = 1;

impl Control {
    pub const fn from_u8(v: u8) -> Option<Control> {
        match v {
            1 => Some(Control::Hidden),
            2 => Some(Control::Visible),
            3 => Some(Control::Frozen),
            4 => Some(Control::Resumed),
            5 => Some(Control::Stale),
            _ => None,
        }
    }
    pub fn encode(self, seq: u32, ts_ns: u64, out: &mut [u8]) -> Result<usize, Error> {
        let n = HDR_LEN + CONTROL_LEN;
        if out.len() < n {
            return Err(Error::Short);
        }
        put_hdr(out, Kind::Control, CONTROL_LEN, seq, ts_ns);
        out[HDR_LEN] = self as u8;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_roundtrip() {
        let b = Bar { bar: 123, tf_s: 60, o: 1, h: 5, l: -3, c: 2, vol: 9, delta: -4 };
        let mut buf = [0u8; 128];
        let n = b.encode(7, 99, &mut buf).unwrap();
        let (f, used) = decode(&buf[..n]).unwrap();
        assert_eq!(used, n);
        assert_eq!(f.hdr.seq, 7);
        assert_eq!(f.hdr.ts_ns, 99);
        assert_eq!(Bar::view(f.body), b);
    }

    #[test]
    fn heatcol_roundtrip_and_refusals() {
        let cells = [3u8; 300];
        let mut buf = [0u8; 512];
        let n = HeatColView::encode(9, -42, 2, &cells, 1, 2, &mut buf).unwrap();
        let (f, _) = decode(&buf[..n]).unwrap();
        let v = HeatColView::view(f.body);
        assert_eq!((v.col, v.base_tick, v.tick_scale, v.cells.len()), (9, -42, 2, 300));
        // A body one byte short of its row count is refused.
        let mut bad = buf;
        bad[2..6].copy_from_slice(&((HEATCOL_FIXED + 299) as u32).to_le_bytes());
        assert_eq!(decode(&bad[..n - 1]).unwrap_err(), Error::BadLen);
    }

    #[test]
    fn header_refusals() {
        let mut buf = [0u8; 64];
        let n = Control::Hidden.encode(1, 1, &mut buf).unwrap();
        assert!(decode(&buf[..n]).is_ok());
        assert_eq!(decode(&buf[..n - 1]).unwrap_err(), Error::Short);
        let mut v = buf;
        v[1] = 9;
        assert_eq!(decode(&v[..n]).unwrap_err(), Error::Version);
        let mut k = buf;
        k[0] = 200;
        assert_eq!(decode(&k[..n]).unwrap_err(), Error::UnknownKind);
        let mut c = buf;
        c[HDR_LEN] = 0;
        assert_eq!(decode(&c[..n]).unwrap_err(), Error::BadValue);
        let mut l = buf;
        l[2..6].copy_from_slice(&(MAX_FRAME as u32).to_le_bytes());
        assert_eq!(decode(&l[..n]).unwrap_err(), Error::TooLong);
    }
}
