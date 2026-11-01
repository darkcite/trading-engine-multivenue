// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Round-trip proptests for every frame kind, the refusals that guard the
//! server's door, and chunk reassembly (TERM plan §4.11.2 "Testing").

use proptest::prelude::*;
use term_proto::chunk::Assembler;
use term_proto::ctl::*;
use term_proto::exec::*;
use term_proto::md::*;
use term_proto::*;

fn rt(buf: &[u8], kind: Kind) -> Frame<'_> {
    let (f, used) = decode(buf).expect("decodes");
    assert_eq!(used, buf.len());
    assert_eq!(f.hdr.kind, kind);
    f
}

fn px() -> impl Strategy<Value = i64> {
    1i64..=1_000_000_000_000_000
}
fn qty() -> impl Strategy<Value = i64> {
    0i64..=1_000_000_000_000
}
fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}
fn cloid() -> impl Strategy<Value = [u8; 16]> {
    any::<[u8; 16]>().prop_filter("nonzero", |c| *c != [0; 16])
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn book_snap(instr in any::<u16>(), s in side(), lv in prop::collection::vec((px(), qty()), 0..64), seq in any::<u32>(), ts in any::<u64>()) {
        let levels: Vec<Level> = lv.iter().map(|&(px, qty)| Level { px, qty }).collect();
        let n = levels.len() as u32;
        let ch = ChunkHdr { total: n, offset: 0, count: n as u16, last: true };
        let mut buf = vec![0u8; MAX_FRAME];
        let k = BookSnap::encode(instr, s, ch, &levels, seq, ts, &mut buf).unwrap();
        let f = rt(&buf[..k], Kind::BookSnap);
        prop_assert_eq!((f.hdr.seq, f.hdr.ts_ns), (seq, ts));
        let v = BookSnap::parse(f.body).unwrap();
        prop_assert_eq!((v.instr, v.side, v.chunk), (instr, s, ch));
        for (i, l) in levels.iter().enumerate() { prop_assert_eq!(v.level(i), *l); }
    }

    #[test]
    fn book_delta(instr in any::<u16>(), d in prop::collection::vec((side(), px(), qty()), 1..=MAX_DELTAS)) {
        let ds: Vec<Delta> = d.iter().map(|&(side, px, qty)| Delta { side, px, qty }).collect();
        let mut buf = vec![0u8; MAX_FRAME];
        let k = BookDelta::encode(instr, &ds, 1, 2, &mut buf).unwrap();
        let v = BookDelta::parse(rt(&buf[..k], Kind::BookDelta).body).unwrap();
        prop_assert_eq!(v.instr, instr);
        for (i, x) in ds.iter().enumerate() { prop_assert_eq!(v.get(i), *x); }
    }

    #[test]
    fn trades(instr in any::<u16>(), t in prop::collection::vec((any::<u64>(), px(), 1i64..1_000_000_000, side()), 1..=MAX_TRADES)) {
        let ts: Vec<Trade> = t.iter().map(|&(ts_ns, px, qty, aggressor)| Trade { ts_ns, px, qty, aggressor }).collect();
        let mut buf = vec![0u8; MAX_FRAME];
        let k = TradeBatch::encode(instr, &ts, 1, 2, &mut buf).unwrap();
        let v = TradeBatch::parse(rt(&buf[..k], Kind::TradeBatch).body).unwrap();
        for (i, x) in ts.iter().enumerate() { prop_assert_eq!(v.get(i), *x); }
    }

    #[test]
    fn bar(instr in any::<u16>(), tf in 1u32..86_400, bar in any::<i64>(), l in px(), span in 0i64..1_000_000_000, fo in 0.0f64..1.0, fc in 0.0f64..1.0, vol in qty(), fd in -1.0f64..1.0) {
        let h = l + span;
        let o = l + (span as f64 * fo) as i64;
        let c = l + (span as f64 * fc) as i64;
        let delta = (vol as f64 * fd) as i64;
        let b = Bar { instr, tf_s: tf, bar, o, h, l, c, vol, delta };
        let mut buf = [0u8; 128];
        let k = b.encode(3, 4, &mut buf).unwrap();
        prop_assert_eq!(Bar::parse(rt(&buf[..k], Kind::Bar).body).unwrap(), b);
    }

    #[test]
    fn footprint(instr in any::<u16>(), bar in any::<i64>(), cells in prop::collection::vec((px(), qty(), qty()), 0..200)) {
        let cs: Vec<FpCell> = cells.iter().map(|&(px, bid_vol, ask_vol)| FpCell { px, bid_vol, ask_vol }).collect();
        let n = cs.len() as u32;
        let ch = ChunkHdr { total: n, offset: 0, count: n as u16, last: true };
        let mut buf = vec![0u8; MAX_FRAME];
        let k = Footprint::encode(instr, 60, bar, ch, &cs, 1, 1, &mut buf).unwrap();
        let v = Footprint::parse(rt(&buf[..k], Kind::FootprintCells).body).unwrap();
        for (i, c) in cs.iter().enumerate() { prop_assert_eq!(v.cell(i), *c); }
    }

    #[test]
    fn flow(instr in any::<u16>(), cvd in any::<i64>(), o1 in any::<i64>(), ok in any::<i64>(), imb in -1_000_000i64..=1_000_000, mp in px()) {
        let f = FlowPoint { instr, cvd, ofi_l1: o1, ofi_k: ok, imb_1e6: imb, microprice: mp };
        let mut buf = [0u8; 128];
        let k = f.encode(1, 1, &mut buf).unwrap();
        prop_assert_eq!(FlowPoint::parse(rt(&buf[..k], Kind::FlowPoint).body).unwrap(), f);
    }

    #[test]
    fn heat(instr in any::<u16>(), col in any::<u32>(), base in any::<i32>(), sc in 1u16.., cells in prop::collection::vec(any::<u8>(), 1..=MAX_HEAT_ROWS)) {
        let h = HeatCol { instr, col, base_tick: base, tick_scale: sc, cells: &cells };
        let mut buf = vec![0u8; MAX_FRAME];
        let k = h.encode(1, 1, &mut buf).unwrap();
        let v = HeatCol::parse(rt(&buf[..k], Kind::HeatCol).body).unwrap();
        prop_assert_eq!((v.instr, v.col, v.base_tick, v.tick_scale, v.cells), (instr, col, base, sc, &cells[..]));
    }

    #[test]
    fn order_view(slot in 0u8..=MAX_SLOT, c in cloid(), s in side(), p in px(), q in 1i64..1_000_000_000, ff in 0.0f64..=1.0, qa in -1i64..1_000_000) {
        let o = OrderView { slot, instr: 7, cloid: c, side: s, kind: OrderKind::Limit, status: OrderStatus::Partial, px: p, qty: q, filled: (q as f64 * ff) as i64, queue_ahead: qa };
        let mut buf = [0u8; 128];
        let k = o.encode(1, 1, &mut buf).unwrap();
        prop_assert_eq!(OrderView::parse(rt(&buf[..k], Kind::OrderView).body).unwrap(), o);
    }

    #[test]
    fn fill_and_position(slot in 0u8..=MAX_SLOT, c in cloid(), s in side(), p in px(), q in 1i64..1_000_000_000, fee in any::<i64>(), ts in any::<u64>(), pq in any::<i64>()) {
        let f = FillView { slot, instr: 1, cloid: c, side: s, px: p, qty: q, fee, ts_ns: ts };
        let mut buf = [0u8; 128];
        let k = f.encode(1, 1, &mut buf).unwrap();
        prop_assert_eq!(FillView::parse(rt(&buf[..k], Kind::FillView).body).unwrap(), f);
        let pos = PositionView { slot, instr: 1, qty: pq, avg_px: p, upnl: fee, liq_px: 0 };
        let k = pos.encode(1, 1, &mut buf).unwrap();
        prop_assert_eq!(PositionView::parse(rt(&buf[..k], Kind::PositionView).body).unwrap(), pos);
    }

    #[test]
    fn intent_place(nonce in 1u64.., slot in 0u8..=MAX_SLOT, s in side(), ioc in any::<bool>(), ro in any::<bool>(), p in px(), q in 1i64..1_000_000_000) {
        let mut i = Intent::blank(nonce, slot, 3, Op::Place);
        i.side = s as u8;
        i.kind = if ioc { 2 } else { 1 };
        i.flags = if ro { FLAG_REDUCE_ONLY } else { 0 };
        i.px = p;
        i.qty = q;
        let mut buf = [0u8; 128];
        let k = i.encode(1, 1, &mut buf).unwrap();
        prop_assert_eq!(Intent::parse(rt(&buf[..k], Kind::Intent).body).unwrap(), i);
        prop_assert!(decode_from_ui(&buf[..k]).is_ok());
    }

    #[test]
    fn decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512), kind in 1u8..=24) {
        let mut b = bytes;
        if b.len() >= HDR_LEN {
            b[0] = kind;
            b[1] = VER;
            let len = (b.len() - HDR_LEN) as u32;
            b[2..6].copy_from_slice(&len.to_le_bytes());
        }
        let _ = decode(&b);
        let _ = decode_from_ui(&b);
    }

    #[test]
    fn chunks_reassemble(n in 0u32..3000, step in 1u32..700) {
        let cells: Vec<VapCell> = (0..n as i64).map(|i| VapCell { bid_vol: i, ask_vol: 2 * i }).collect();
        let mut store = vec![0u8; 4096 * Vap::STRIDE];
        let mut asm = Assembler::new(&mut store, Vap::STRIDE);
        let mut buf = vec![0u8; MAX_FRAME];
        let mut off = 0u32;
        let done;
        loop {
            let cnt = step.min(n - off);
            let last = off + cnt == n;
            let ch = ChunkHdr { total: n, offset: off, count: cnt as u16, last };
            let k = Vap::encode(1, 1_000_000, 100_000, ch, &cells[off as usize..(off + cnt) as usize], off, 0, &mut buf).unwrap();
            let v = Vap::parse(rt(&buf[..k], Kind::VapArray).body).unwrap();
            let r = asm.feed(v.chunk, v.raw()).unwrap();
            off += cnt;
            if last { done = r; break; }
            prop_assert!(r.is_none());
        }
        let len = done.unwrap();
        prop_assert_eq!(len, n as usize * Vap::STRIDE);
        let out = asm.assembled(len);
        for i in 0..n as usize {
            let b = i64::from_le_bytes(out[i * 16..i * 16 + 8].try_into().unwrap());
            prop_assert_eq!(b, i as i64);
        }
    }
}

#[test]
fn chunk_gap_resets() {
    let mut store = vec![0u8; 64 * 16];
    let mut asm = Assembler::new(&mut store, 16);
    let raw = [0u8; 32];
    assert_eq!(asm.feed(ChunkHdr { total: 6, offset: 0, count: 2, last: false }, &raw), Ok(None));
    assert_eq!(asm.feed(ChunkHdr { total: 6, offset: 4, count: 2, last: true }, &raw), Err(Error::Gap));
    assert_eq!(asm.feed(ChunkHdr { total: 6, offset: 2, count: 2, last: false }, &raw), Err(Error::Gap));
}

#[test]
fn direction_is_enforced() {
    let b = Bar { instr: 1, tf_s: 60, bar: 1, o: 5, h: 9, l: 1, c: 2, vol: 10, delta: 3 };
    let mut buf = [0u8; 128];
    let k = b.encode(1, 1, &mut buf).unwrap();
    assert_eq!(decode_from_ui(&buf[..k]).unwrap_err(), Error::Direction);
    let k = Control::op(CtlOp::Hidden).encode(1, 1, &mut buf).unwrap();
    assert!(decode_from_ui(&buf[..k]).is_ok());
}

#[test]
fn intent_shapes_are_strict() {
    let mut buf = [0u8; 128];
    // Place without a price.
    let mut i = Intent::blank(1, 10, 1, Op::Place);
    i.side = 1;
    i.kind = 1;
    i.qty = 5;
    assert_eq!(i.encode(1, 1, &mut buf), Err(Error::BadValue));
    // IoC cannot be post-only.
    i.px = 100;
    i.kind = 2;
    i.flags = FLAG_POST_ONLY;
    assert_eq!(i.encode(1, 1, &mut buf), Err(Error::BadValue));
    // Cancel needs a target and nothing else.
    let mut c = Intent::blank(2, 10, 1, Op::Cancel);
    assert_eq!(c.encode(1, 1, &mut buf), Err(Error::BadValue));
    c.target = [9; 16];
    assert!(c.encode(1, 1, &mut buf).is_ok());
    c.qty = 1;
    assert_eq!(c.encode(1, 1, &mut buf), Err(Error::BadValue));
    // Slot out of range, nonce zero.
    let h = Intent::blank(3, 16, 1, Op::Halt);
    let k = term_proto::encode(Kind::Intent, 1, 1, &mut buf, |w| {
        w.u64(h.nonce)?; w.u8(h.slot)?; w.u16(h.instr)?; w.u8(8)?;
        w.bytes(&[0u8; 3 + 16 + 16 + 1 + 4 + 2 + 8])
    }).unwrap();
    assert_eq!(decode(&buf[..k]).unwrap_err(), Error::BadValue);
    assert_eq!(Intent::blank(0, 1, 1, Op::Halt).encode(1, 1, &mut buf), Err(Error::BadValue));
    // TWAP needs a duration and slices.
    let mut t = Intent::blank(4, 10, 1, Op::AlgoStart);
    t.side = 2; t.kind = 1; t.px = 10; t.qty = 10; t.algo = Algo::Twap;
    assert_eq!(t.encode(1, 1, &mut buf), Err(Error::BadValue));
    t.dur_s = 60; t.slices = 6;
    assert!(t.encode(1, 1, &mut buf).is_ok());
}

#[test]
fn ascii_and_records() {
    assert!(Ascii::<8>::new(b"BTC-PERP").is_ok());
    assert!(Ascii::<8>::new(b"BTC-PERP1").is_err());
    assert!(Ascii::<8>::new(b"bad\n").is_err());
    assert!(Ascii::<8>::new("€".as_bytes()).is_err());
    let mut buf = [0u8; 256];
    let d = InstrDef { instr: 4, venue: Venue::Hyperliquid, tick_1e6: 100_000, lot_1e6: 10, flags: INSTR_IOC_ONLY, label: Ascii::new(b"BTC").unwrap() };
    let k = d.encode(1, 1, &mut buf).unwrap();
    assert_eq!(InstrDef::parse(rt(&buf[..k], Kind::InstrDef).body).unwrap(), d);
    // Garbage after the label's length is refused (no hidden bytes).
    let mut bad = buf;
    bad[HDR_LEN + 2 + 1 + 8 + 8 + 1 + 1 + 5] = b'X';
    assert_eq!(decode(&bad[..k]).unwrap_err(), Error::BadValue);
    let mut nodes = [LayoutNode::default(); MAX_LAYOUT_NODES];
    nodes[0] = LayoutNode { kind: PanelKind::SplitH, parent: 0, share_permille: 1000 };
    nodes[1] = LayoutNode { kind: PanelKind::Chart, parent: 0, share_permille: 700 };
    nodes[2] = LayoutNode { kind: PanelKind::Ladder, parent: 0, share_permille: 300 };
    let l = Layout { name: Ascii::new(b"default").unwrap(), n: 3, nodes };
    let k = l.encode(1, 1, &mut buf).unwrap();
    assert_eq!(Layout::parse(rt(&buf[..k], Kind::Layout).body).unwrap(), l);
    let mut bad_l = l;
    bad_l.nodes[2].parent = 1; // a chart cannot be a parent
    let k = bad_l.encode(1, 1, &mut buf).unwrap();
    assert!(decode(&buf[..k]).is_err());
    let mut binds = [KeyBind { key: 0, mods: 0, action: KeyAction::Size1 }; MAX_KEYS];
    binds[0] = KeyBind { key: 70, mods: MOD_CTRL | MOD_SHIFT, action: KeyAction::Flatten };
    binds[1] = KeyBind { key: 75, mods: MOD_SHIFT, action: KeyAction::Kill };
    let km = KeyMap { n: 2, binds };
    let k = km.encode(1, 1, &mut buf).unwrap();
    assert!(decode(&buf[..k]).is_err(), "Kill needs ⌃⇧");
    let s = EngineStatus { boot_epoch: 9, git_sha: Ascii::new(b"6bf205d6").unwrap(), engine_up: true, n: 1, slots: [exec::SlotState { slot: 3, mode: SlotMode::Live, halted: false }; 16] };
    let k = s.encode(1, 1, &mut buf).unwrap();
    assert_eq!(EngineStatus::parse(rt(&buf[..k], Kind::EngineStatus).body).unwrap().slots[0].mode, SlotMode::Live);
    let lc = LiveConfirm { slot: 10, nonce: 77, phrase: Ascii::new(b"LIVE term-hl").unwrap() };
    let k = lc.encode(1, 1, &mut buf).unwrap();
    assert_eq!(LiveConfirm::parse(rt(&buf[..k], Kind::LiveConfirm).body).unwrap(), lc);
    let bt = Beat { n: 5, server_ns: 6 };
    let k = bt.encode(true, 1, 1, &mut buf).unwrap();
    assert_eq!(rt(&buf[..k], Kind::Echo).hdr.kind, Kind::Echo);
    let dr = Drawing { instr: 1, id: 3, kind: DrawKind::HLine, rgba: 0xffffffff, pts: [ChartPt { bar: 5, px: 7 }, ChartPt::default()] };
    let k = dr.encode(1, 1, &mut buf).unwrap();
    assert_eq!(Drawing::parse(rt(&buf[..k], Kind::Drawing).body).unwrap(), dr);
}

#[test]
fn intent_acks() {
    let mut buf = [0u8; 64];
    let a = IntentAck { nonce: 9, slot: 12, accepted: false, source: AckSource::Engine, reason: 7 };
    let k = a.encode(1, 1, &mut buf).unwrap();
    assert_eq!(IntentAck::parse(rt(&buf[..k], Kind::IntentAck).body).unwrap(), a);
    assert_eq!(decode_from_ui(&buf[..k]).unwrap_err(), Error::Direction, "server → UI only");
    let bad = IntentAck { accepted: true, ..a };
    assert_eq!(bad.encode(1, 1, &mut buf), Err(Error::BadValue), "an acceptance has no reason");
}

#[test]
fn header_refusals() {
    let mut buf = [0u8; 64];
    let k = Control::op(CtlOp::Visible).encode(1, 1, &mut buf).unwrap();
    assert_eq!(decode(&buf[..k - 1]).unwrap_err(), Error::Short);
    let mut v = buf;
    v[1] = 0;
    assert_eq!(decode(&v[..k]).unwrap_err(), Error::Version);
    let mut kd = buf;
    kd[0] = 0;
    assert_eq!(decode(&kd[..k]).unwrap_err(), Error::UnknownKind);
    let mut l = buf;
    l[2..6].copy_from_slice(&(MAX_FRAME as u32).to_le_bytes());
    assert_eq!(decode(&l[..k]).unwrap_err(), Error::TooLong);
}
