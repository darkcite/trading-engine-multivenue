// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `--synthetic <seed>` (TERM plan §4.6): a deterministic market written as
//! code, not data — books, prints, bars, flow, heat — for two instruments,
//! plus a stub `EngineStatus`. With it the UI and the cloud browser smoke
//! need neither the network nor a capture nor the engine.

use crate::source::Source;
use term_proto::ctl::{InstrDef, Venue};
use term_proto::exec::{EngineStatus, SlotMode, SlotState};
use term_proto::md::*;
use term_proto::{Ascii, ChunkHdr, Side};

const ROWS: usize = 600;
const DEPTH: usize = 50;
const HEAT_ROWS: usize = 256;
const BAR_HISTORY: i64 = 300;
const TF_S: u32 = 60;
pub const TICK_1E6: i64 = 100_000; // 0.1

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.unit() * (hi - lo + 1) as f64) as i64
    }
}

struct Instr {
    def: InstrDef,
    /// Size per absolute tick row `anchor + i`, ×1e6.
    rows: Vec<i64>,
    anchor: i64,
    mid: i64,
    bar: Bar,
    cvd: i64,
    prev_l1: (i64, i64, i64, i64),
    col: u32,
}

pub struct Synthetic {
    rng: Rng,
    ins: Vec<Instr>,
    seq: u32,
    last_t: u64,
    epoch_bar0: i64,
    scratch: Vec<u8>,
    deltas: Vec<Delta>,
    trades: Vec<Trade>,
}

impl Synthetic {
    pub fn new(seed: u64) -> Synthetic {
        let mut rng = Rng(seed | 1);
        let mk = |instr: u16, venue, label: &[u8], mid: i64, rng: &mut Rng| {
            let anchor = mid - ROWS as i64 / 2;
            let rows = (0..ROWS).map(|_| (rng.unit().powi(2) * 8e6) as i64 + 10_000).collect();
            Instr {
                def: InstrDef { instr, venue, tick_1e6: TICK_1E6, lot_1e6: 1_000, flags: 0, label: Ascii::new(label).unwrap_or(Ascii::new(b"?").unwrap()) },
                rows,
                anchor,
                mid,
                bar: Bar::default(),
                cvd: 0,
                prev_l1: (0, 0, 0, 0),
                col: 0,
            }
        };
        let ins = vec![mk(0, Venue::Hyperliquid, b"BTC-PERP HL", 670_000, &mut rng), mk(1, Venue::Binance, b"BTCUSDT BN-UM", 670_012, &mut rng)];
        let mut s = Synthetic { rng, ins, seq: 0, last_t: 0, epoch_bar0: 29_500_000, scratch: vec![0u8; term_proto::MAX_FRAME], deltas: Vec::with_capacity(256), trades: Vec::with_capacity(64) };
        for i in 0..s.ins.len() {
            s.open_bar(i, s.epoch_bar0);
        }
        s
    }

    fn seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn open_bar(&mut self, i: usize, bar: i64) {
        let px = self.ins[i].mid * TICK_1E6;
        self.ins[i].bar = Bar { instr: i as u16, tf_s: TF_S, bar, o: px, h: px, l: px, c: px, vol: 0, delta: 0 };
    }

    fn size_at(&self, i: usize, tick: i64) -> i64 {
        let k = tick - self.ins[i].anchor;
        if (0..ROWS as i64).contains(&k) {
            self.ins[i].rows[k as usize]
        } else {
            0
        }
    }

    fn push(&mut self, out: &mut Vec<u8>, n: usize) {
        out.extend_from_slice(&self.scratch[..n]);
    }

    fn book_snap(&mut self, i: usize, out: &mut Vec<u8>) {
        let mid = self.ins[i].mid;
        for side in [Side::Buy, Side::Sell] {
            let mut lv = [Level::default(); DEPTH];
            for (d, l) in lv.iter_mut().enumerate() {
                let t = if side == Side::Buy { mid - 1 - d as i64 } else { mid + 1 + d as i64 };
                *l = Level { px: t * TICK_1E6, qty: self.size_at(i, t) };
            }
            let ch = ChunkHdr { total: DEPTH as u32, offset: 0, count: DEPTH as u16, last: true };
            let seq = self.seq();
            if let Ok(n) = BookSnap::encode(i as u16, side, ch, &lv, seq, self.last_t, &mut self.scratch) {
                self.push(out, n);
            }
        }
    }

    fn step(&mut self, i: usize, t: u64, out: &mut Vec<u8>) {
        let old_mid = self.ins[i].mid;
        let mv = self.rng.range(-2, 2);
        let mid = old_mid + mv;
        // Keep the book array around the mid; re-anchor with a snapshot.
        let k = mid - self.ins[i].anchor;
        let reanchor = !(DEPTH as i64 + 5..ROWS as i64 - DEPTH as i64 - 5).contains(&k);
        if reanchor {
            self.ins[i].anchor = mid - ROWS as i64 / 2;
            for r in self.ins[i].rows.iter_mut() {
                *r = (self.rng.unit().powi(2) * 8e6) as i64 + 10_000;
            }
        }
        // Random size changes near the touch.
        self.deltas.clear();
        for _ in 0..12 {
            let d = self.rng.range(1, DEPTH as i64);
            let side = if self.rng.unit() < 0.5 { Side::Buy } else { Side::Sell };
            let tick = if side == Side::Buy { mid - d } else { mid + d };
            let row = (tick - self.ins[i].anchor) as usize;
            let q = (self.rng.unit().powi(2) * 8e6) as i64 + 10_000;
            self.ins[i].rows[row] = q;
            self.deltas.push(Delta { side, px: tick * TICK_1E6, qty: q });
        }
        // Levels the mid crossed change side: remove on the old side.
        if mv != 0 && !reanchor {
            let (lo, hi) = (old_mid.min(mid), old_mid.max(mid));
            for tick in lo - 1..=hi + 1 {
                let side_old = if tick < old_mid { Side::Buy } else { Side::Sell };
                let side_new = if tick < mid { Some(Side::Buy) } else if tick > mid { Some(Side::Sell) } else { None };
                if Some(side_old) != side_new && tick != old_mid {
                    self.deltas.push(Delta { side: side_old, px: tick * TICK_1E6, qty: 0 });
                }
                if let Some(s) = side_new {
                    if Some(side_old) != side_new || tick == old_mid {
                        let q = self.size_at(i, tick);
                        self.deltas.push(Delta { side: s, px: tick * TICK_1E6, qty: q });
                    }
                }
            }
        }
        // Remove the far edge beyond DEPTH so the client book stays bounded.
        if mv > 0 {
            for d in 0..mv {
                self.deltas.push(Delta { side: Side::Buy, px: (old_mid - 1 - DEPTH as i64 + 1 + d) * TICK_1E6, qty: 0 });
            }
        } else if mv < 0 {
            for d in 0..-mv {
                self.deltas.push(Delta { side: Side::Sell, px: (old_mid + 1 + DEPTH as i64 - 1 - d) * TICK_1E6, qty: 0 });
            }
        }
        self.ins[i].mid = mid;
        if reanchor {
            self.book_snap(i, out);
        } else if !self.deltas.is_empty() {
            self.deltas.truncate(MAX_DELTAS);
            let seq = self.seq();
            if let Ok(n) = BookDelta::encode(i as u16, &self.deltas, seq, t, &mut self.scratch) {
                self.push(out, n);
            }
        }
        // Prints at the touch.
        self.trades.clear();
        let nt = self.rng.range(0, 6);
        for _ in 0..nt {
            let buy = self.rng.unit() < 0.5 + 0.1 * mv as f64;
            let tick = if buy { mid + 1 } else { mid - 1 };
            let qty = (self.rng.unit().powi(4) * 5e6) as i64 + 1_000;
            self.trades.push(Trade { ts_ns: t, px: tick * TICK_1E6, qty, aggressor: if buy { Side::Buy } else { Side::Sell } });
            let b = &mut self.ins[i].bar;
            let px = tick * TICK_1E6;
            b.h = b.h.max(px);
            b.l = b.l.min(px);
            b.c = px;
            b.vol += qty;
            b.delta += if buy { qty } else { -qty };
            self.ins[i].cvd += if buy { qty } else { -qty };
        }
        if !self.trades.is_empty() {
            let seq = self.seq();
            if let Ok(n) = TradeBatch::encode(i as u16, &self.trades, seq, t, &mut self.scratch) {
                self.push(out, n);
            }
        }
        // Flow: L1 OFI (Cont–Kukanov–Stoikov), top-5 imbalance, microprice.
        let (bp, bq, ap, aq) = ((mid - 1) * TICK_1E6, self.size_at(i, mid - 1), (mid + 1) * TICK_1E6, self.size_at(i, mid + 1));
        let (pbp, pbq, pap, paq) = self.ins[i].prev_l1;
        let e_b = if bp > pbp { bq } else if bp == pbp { bq - pbq } else { -pbq };
        let e_a = if ap < pap { aq } else if ap == pap { aq - paq } else { -paq };
        self.ins[i].prev_l1 = (bp, bq, ap, aq);
        let (mut sb, mut sa) = (0i64, 0i64);
        for d in 1..=5 {
            sb += self.size_at(i, mid - d);
            sa += self.size_at(i, mid + d);
        }
        let imb = if sb + sa > 0 { ((sb - sa) as i128 * 1_000_000 / (sb + sa) as i128) as i64 } else { 0 };
        let micro = if bq + aq > 0 { ((ap as i128 * bq as i128 + bp as i128 * aq as i128) / (bq + aq) as i128) as i64 } else { (bp + ap) / 2 };
        let fp = FlowPoint { instr: i as u16, cvd: self.ins[i].cvd, ofi_l1: e_b - e_a, ofi_k: e_b - e_a, imb_1e6: imb, microprice: micro };
        let seq = self.seq();
        if let Ok(n) = fp.encode(seq, t, &mut self.scratch) {
            self.push(out, n);
        }
        // Heat column around the mid.
        let mut cells = [0u8; HEAT_ROWS];
        let base = mid - HEAT_ROWS as i64 / 2;
        for (r, c) in cells.iter_mut().enumerate() {
            let q = self.size_at(i, base + r as i64) as f64 / 1e6;
            *c = ((1.0 + q).ln() / (9.0f64).ln() * 255.0).clamp(0.0, 255.0) as u8;
        }
        let col = self.ins[i].col;
        self.ins[i].col = col.wrapping_add(1);
        let hc = HeatCol { instr: i as u16, col, base_tick: base as i32, tick_scale: 1, cells: &cells };
        let seq = self.seq();
        if let Ok(n) = hc.encode(seq, t, &mut self.scratch) {
            self.push(out, n);
        }
        // The live bar; roll on the minute.
        let bar_now = self.epoch_bar0 + (t / 1_000_000_000 / TF_S as u64) as i64;
        if bar_now != self.ins[i].bar.bar {
            self.open_bar(i, bar_now);
        }
        let b = self.ins[i].bar;
        let seq = self.seq();
        if let Ok(n) = b.encode(seq, t, &mut self.scratch) {
            self.push(out, n);
        }
    }
}

impl Source for Synthetic {
    fn hello(&mut self, out: &mut Vec<u8>) {
        for i in 0..self.ins.len() {
            let d = self.ins[i].def;
            let seq = self.seq();
            if let Ok(n) = d.encode(seq, self.last_t, &mut self.scratch) {
                self.push(out, n);
            }
        }
        let mut slots = [SlotState::default(); 16];
        for (k, s) in [10u8, 11, 12, 13].iter().enumerate() {
            slots[k] = SlotState { slot: *s, mode: SlotMode::Paper, halted: false };
        }
        let st = EngineStatus { boot_epoch: 0, git_sha: Ascii::new(b"0000000").unwrap_or(Ascii::new(b"0").unwrap()), engine_up: false, n: 4, slots };
        let seq = self.seq();
        if let Ok(n) = st.encode(seq, self.last_t, &mut self.scratch) {
            self.push(out, n);
        }
    }

    fn snapshot(&mut self, instr: u16, out: &mut Vec<u8>) {
        let i = instr as usize;
        if i >= self.ins.len() {
            return;
        }
        self.book_snap(i, out);
        // Bar history: a deterministic walk ending at the live bar's open.
        let mut r = Rng(0x5EED ^ instr as u64);
        let live = self.ins[i].bar;
        let mut px = live.o;
        let mut hist = [Bar::default(); BAR_HISTORY as usize];
        for k in (0..BAR_HISTORY as usize).rev() {
            let c = px;
            let o = c - r.range(-30, 30) * TICK_1E6;
            let h = o.max(c) + r.range(0, 15) * TICK_1E6;
            let l = o.min(c) - r.range(0, 15) * TICK_1E6;
            let vol = r.range(1, 400) * 100_000;
            let delta = (vol as f64 * (r.unit() - 0.5)) as i64;
            hist[k] = Bar { instr, tf_s: TF_S, bar: live.bar - (BAR_HISTORY - k as i64), o, h, l, c, vol, delta };
            px = o;
        }
        for b in hist {
            let seq = self.seq();
            if let Ok(n) = b.encode(seq, self.last_t, &mut self.scratch) {
                self.push(out, n);
            }
        }
    }

    fn tick(&mut self, t_ns: u64, out: &mut Vec<u8>) {
        self.last_t = t_ns;
        for i in 0..self.ins.len() {
            self.step(i, t_ns, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::frames;

    #[test]
    fn every_frame_validates_and_the_book_stays_consistent() {
        let mut s = Synthetic::new(7);
        let mut out = Vec::new();
        s.hello(&mut out);
        s.snapshot(0, &mut out);
        // Rebuild instrument 0's book from snapshots and deltas.
        let mut bids = std::collections::BTreeMap::new();
        let mut asks = std::collections::BTreeMap::new();
        for t in 0..3000u64 {
            s.tick(t * 100_000_000, &mut out);
        }
        for f in frames(&out) {
            let f = f.expect("valid frame");
            let (fr, _) = term_proto::decode(f).unwrap();
            match fr.hdr.kind {
                term_proto::Kind::BookSnap => {
                    let v = BookSnap::parse(fr.body).unwrap();
                    if v.instr != 0 {
                        continue;
                    }
                    let m = if v.side == Side::Buy { &mut bids } else { &mut asks };
                    m.clear();
                    for k in 0..v.len() {
                        let l = v.level(k);
                        m.insert(l.px, l.qty);
                    }
                }
                term_proto::Kind::BookDelta => {
                    let v = BookDelta::parse(fr.body).unwrap();
                    if v.instr != 0 {
                        continue;
                    }
                    for k in 0..v.len() {
                        let d = v.get(k);
                        let m = if d.side == Side::Buy { &mut bids } else { &mut asks };
                        if d.qty == 0 {
                            m.remove(&d.px);
                        } else {
                            m.insert(d.px, d.qty);
                        }
                    }
                    let bb = bids.keys().next_back().copied().unwrap_or(0);
                    let ba = asks.keys().next().copied().unwrap_or(i64::MAX);
                    assert!(bb < ba, "crossed book {bb} ≥ {ba}");
                }
                _ => {}
            }
        }
        assert!(bids.len() <= 80 && asks.len() <= 80, "book stays bounded: {} / {}", bids.len(), asks.len());
    }
}
