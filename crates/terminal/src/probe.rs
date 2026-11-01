// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `--probe SECS`: run a source with no server and report what it produced —
//! frames per kind, decode failures, and the book rebuilt from snapshots and
//! deltas exactly as a client would (crossed books are counted).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use term_proto::md::{BookDelta, BookSnap};
use term_proto::{Kind, Side};

use crate::source::{frames, Source};

pub fn run<S: Source>(src: &mut S, secs: u64) {
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut books: BTreeMap<u16, (BTreeMap<i64, i64>, BTreeMap<i64, i64>)> = BTreeMap::new();
    let (mut bad, mut crossed, mut checks) = (0u64, 0u64, 0u64);
    let mut out = Vec::with_capacity(1 << 20);
    src.hello(&mut out);
    let t0 = Instant::now();
    let mut last_print = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        src.tick(t0.elapsed().as_nanos() as u64, &mut out);
        for f in frames(&out) {
            let Ok(f) = f else {
                bad += 1;
                continue;
            };
            let (fr, _) = match term_proto::decode(f) {
                Ok(x) => x,
                Err(_) => {
                    bad += 1;
                    continue;
                }
            };
            *counts.entry(kind_name(fr.hdr.kind)).or_default() += 1;
            match fr.hdr.kind {
                Kind::BookSnap => {
                    if let Ok(v) = BookSnap::parse(fr.body) {
                        let b = books.entry(v.instr).or_default();
                        let m = if v.side == Side::Buy { &mut b.0 } else { &mut b.1 };
                        m.clear();
                        for k in 0..v.len() {
                            let l = v.level(k);
                            m.insert(l.px, l.qty);
                        }
                    }
                }
                Kind::BookDelta => {
                    if let Ok(v) = BookDelta::parse(fr.body) {
                        let b = books.entry(v.instr).or_default();
                        for k in 0..v.len() {
                            let d = v.get(k);
                            let m = if d.side == Side::Buy { &mut b.0 } else { &mut b.1 };
                            if d.qty == 0 {
                                m.remove(&d.px);
                            } else {
                                m.insert(d.px, d.qty);
                            }
                        }
                        if let (Some(bb), Some(ba)) = (b.0.keys().next_back(), b.1.keys().next()) {
                            checks += 1;
                            if bb >= ba {
                                crossed += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out.clear();
        if last_print.elapsed() > Duration::from_secs(10) {
            last_print = Instant::now();
            eprintln!("probe {:>4.0}s: {counts:?}", t0.elapsed().as_secs_f32());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for (instr, (b, a)) in &books {
        let bb = b.keys().next_back().copied().unwrap_or(0);
        let ba = a.keys().next().copied().unwrap_or(0);
        eprintln!("probe instr {instr}: {} bids / {} asks, best {}.{:06} / {}.{:06}", b.len(), a.len(), bb / 1_000_000, bb % 1_000_000, ba / 1_000_000, ba % 1_000_000);
    }
    eprintln!("probe done: {counts:?}; undecodable {bad}; crossed {crossed}/{checks} delta checks");
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::BookSnap => "BookSnap",
        Kind::BookDelta => "BookDelta",
        Kind::TradeBatch => "TradeBatch",
        Kind::Bar => "Bar",
        Kind::FlowPoint => "FlowPoint",
        Kind::HeatCol => "HeatCol",
        Kind::InstrDef => "InstrDef",
        Kind::EngineStatus => "EngineStatus",
        _ => "other",
    }
}
