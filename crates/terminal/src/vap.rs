// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Volume at price, computed by the server from the prints it routes
//! (source-agnostic: live, synthetic and replay alike) and sent as
//! `VapArray` on `CH_VAP` (TERM plan §4.9: the server computes, the WASM
//! renders). Buckets are ≈ 0.1 bp of the first price, rounded to 1-2-5, over
//! a fixed window centred there; prints outside it are counted, not stored.

use term_proto::md::{TradeBatch, Vap, VapCell};
use term_proto::{ChunkHdr, Side};

pub const BUCKETS: usize = 2048;

pub struct VapBook {
    pub base_px: i64,
    pub step_px: i64,
    pub cells: Vec<VapCell>,
    pub outside: u64,
    dirty: bool,
}

/// A 1-2-5 × 10^k step near `px / 100_000` (≈ 0.1 bp), ×1e6.
pub fn step_for(px: i64) -> i64 {
    let raw = (px / 100_000).max(1);
    let mut p = 1i64;
    while p.saturating_mul(10) <= raw {
        p *= 10;
    }
    [1, 2, 5, 10].iter().map(|m| p * m).find(|s| *s >= raw).unwrap_or(p * 10)
}

impl VapBook {
    pub fn new(first_px: i64) -> VapBook {
        let step_px = step_for(first_px);
        let base_px = (first_px / step_px - (BUCKETS as i64) / 2) * step_px;
        VapBook { base_px, step_px, cells: vec![VapCell::default(); BUCKETS], outside: 0, dirty: false }
    }

    pub fn add(&mut self, px: i64, qty: i64, aggressor: Side) {
        let k = (px - self.base_px).div_euclid(self.step_px);
        if !(0..BUCKETS as i64).contains(&k) {
            self.outside += 1;
            return;
        }
        let c = &mut self.cells[k as usize];
        // A buy aggressor lifts the ask; a sell hits the bid.
        if aggressor == Side::Buy {
            c.ask_vol += qty;
        } else {
            c.bid_vol += qty;
        }
        self.dirty = true;
    }

    /// The whole profile as one VapArray frame (2048 × 16 B fits a frame).
    pub fn frame(&mut self, instr: u16, ts: u64, out: &mut Vec<u8>) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let ch = ChunkHdr { total: BUCKETS as u32, offset: 0, count: BUCKETS as u16, last: true };
        let mut b = vec![0u8; term_proto::MAX_FRAME];
        if let Ok(n) = Vap::encode(instr, self.base_px, self.step_px, ch, &self.cells, 0, ts, &mut b) {
            out.extend_from_slice(&b[..n]);
        }
    }
}

/// Feed every print of a TradeBatch body into the instrument's book.
pub fn feed(books: &mut [Option<VapBook>], body: &[u8]) {
    let Ok(t) = TradeBatch::parse(body) else { return };
    let Some(slot) = books.get_mut(t.instr as usize) else { return };
    for k in 0..t.len() {
        let p = t.get(k);
        slot.get_or_insert_with(|| VapBook::new(p.px)).add(p.px, p.qty, p.aggressor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_buckets() {
        assert_eq!(step_for(78_000_000_000), 1_000_000, "≈ 0.1 bp of 78k → 1");
        assert_eq!(step_for(2_269_000_000), 50_000, "2269 → 0.05");
        let mut v = VapBook::new(78_000_000_000);
        v.add(78_000_000_000, 5, Side::Buy);
        v.add(78_000_400_000, 7, Side::Sell);
        let k = ((78_000_000_000 - v.base_px) / v.step_px) as usize;
        assert_eq!((v.cells[k].ask_vol, v.cells[k].bid_vol), (5, 7), "same 1-wide bucket");
        v.add(1, 1, Side::Buy);
        assert_eq!(v.outside, 1);
        let mut out = Vec::new();
        v.frame(3, 0, &mut out);
        let (f, _) = term_proto::decode(&out).unwrap();
        let p = Vap::parse(f.body).unwrap();
        assert_eq!((p.instr, p.len(), p.cell(k).ask_vol), (3, BUCKETS, 5));
        let mut again = Vec::new();
        v.frame(3, 0, &mut again);
        assert!(again.is_empty(), "nothing new, nothing sent");
    }
}
