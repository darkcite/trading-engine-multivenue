// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Footprint cells, computed by the server from the prints it routes: per
//! instrument, the live bar's volume at each traded price split by
//! aggressor (bid = sells hitting the bid, ask = buys lifting the offer).
//! The bar index follows the `Bar` frames for that instrument. Sent as
//! `FootprintCells` on `CH_FOOTPRINT`: the live bar about once a second,
//! and a bar's final cells when the next one opens.

use std::collections::BTreeMap;

use term_proto::md::{Bar, FpCell, Footprint, TradeBatch, MAX_FOOTPRINT_CELLS};
use term_proto::{ChunkHdr, Side};

#[derive(Default)]
pub struct FpBook {
    pub bar: i64,
    pub tf_s: u32,
    pub cells: BTreeMap<i64, (i64, i64)>,
    dirty: bool,
    /// A finished bar waiting to be sent: (bar, tf, cells).
    closed: Option<(i64, u32, Vec<FpCell>)>,
}

fn to_cells(m: &BTreeMap<i64, (i64, i64)>) -> Vec<FpCell> {
    m.iter().map(|(px, (b, a))| FpCell { px: *px, bid_vol: *b, ask_vol: *a }).collect()
}

impl FpBook {
    pub fn on_bar(&mut self, b: &Bar) {
        if b.bar > self.bar {
            if self.tf_s != 0 && !self.cells.is_empty() {
                self.closed = Some((self.bar, self.tf_s, to_cells(&self.cells)));
            }
            self.cells.clear();
            self.bar = b.bar;
            self.tf_s = b.tf_s;
            self.dirty = false;
        }
    }

    pub fn on_trade(&mut self, px: i64, qty: i64, aggressor: Side) {
        if self.tf_s == 0 || self.cells.len() >= MAX_FOOTPRINT_CELLS as usize && !self.cells.contains_key(&px) {
            return; // no bar yet, or a pathological bar: drop, never grow
        }
        let c = self.cells.entry(px).or_insert((0, 0));
        if aggressor == Side::Buy {
            c.1 += qty;
        } else {
            c.0 += qty;
        }
        self.dirty = true;
    }

    fn put(instr: u16, bar: i64, tf_s: u32, cells: &[FpCell], ts: u64, out: &mut Vec<u8>) {
        let mut b = vec![0u8; term_proto::MAX_FRAME];
        // ≤ 2600 cells of 24 B fit one 64 KiB frame; chunk the rest.
        let per = 2600usize;
        let total = cells.len() as u32;
        if total == 0 {
            return;
        }
        let mut off = 0usize;
        while off < cells.len() {
            let n = per.min(cells.len() - off);
            let ch = ChunkHdr { total, offset: off as u32, count: n as u16, last: off + n == cells.len() };
            if let Ok(k) = Footprint::encode(instr, tf_s, bar, ch, &cells[off..off + n], 0, ts, &mut b) {
                out.extend_from_slice(&b[..k]);
            }
            off += n;
        }
    }

    /// The closed bar's final cells (once), then the live bar's if changed.
    pub fn frames(&mut self, instr: u16, ts: u64, out: &mut Vec<u8>) {
        if let Some((bar, tf, cells)) = self.closed.take() {
            Self::put(instr, bar, tf, &cells, ts, out);
        }
        if self.dirty {
            self.dirty = false;
            Self::put(instr, self.bar, self.tf_s, &to_cells(&self.cells), ts, out);
        }
    }
}

/// Feed one routed md frame (Bar or TradeBatch) into the books.
pub fn feed(books: &mut [FpBook], frame: &[u8]) {
    let Some(&k) = frame.first() else { return };
    let body = &frame[term_proto::HDR_LEN.min(frame.len())..];
    if k == term_proto::Kind::Bar as u8 {
        if let Ok(b) = Bar::parse(body) {
            if let Some(fp) = books.get_mut(b.instr as usize) {
                fp.on_bar(&b);
            }
        }
    } else if k == term_proto::Kind::TradeBatch as u8 {
        if let Ok(t) = TradeBatch::parse(body) {
            if let Some(fp) = books.get_mut(t.instr as usize) {
                for i in 0..t.len() {
                    let p = t.get(i);
                    fp.on_trade(p.px, p.qty, p.aggressor);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_accumulates_then_closes() {
        let mut fp = FpBook::default();
        fp.on_trade(100, 1, Side::Buy);
        assert!(fp.cells.is_empty(), "no bar yet: nothing kept");
        let bar = |n| Bar { instr: 0, tf_s: 60, bar: n, o: 100, h: 100, l: 100, c: 100, vol: 0, delta: 0 };
        fp.on_bar(&bar(10));
        fp.on_trade(100, 3, Side::Buy);
        fp.on_trade(100, 2, Side::Sell);
        fp.on_trade(101, 5, Side::Sell);
        assert_eq!(fp.cells[&100], (2, 3));
        let mut out = Vec::new();
        fp.frames(0, 0, &mut out);
        let (f, _) = term_proto::decode(&out).unwrap();
        let v = Footprint::parse(f.body).unwrap();
        assert_eq!((v.bar, v.len(), v.cell(1).px, v.cell(1).bid_vol), (10, 2, 101, 5));
        fp.on_bar(&bar(11));
        out.clear();
        fp.frames(0, 0, &mut out);
        let (f, _) = term_proto::decode(&out).unwrap();
        assert_eq!(Footprint::parse(f.body).unwrap().bar, 10, "the closed bar's final cells");
        assert!(fp.cells.is_empty());
    }
}
