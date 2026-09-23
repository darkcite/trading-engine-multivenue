// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz the HyperEVM ingress decoders and the snapshot state machine
//! (HYPARB H3, `ingress-hyperevm`).
//!
//! Every byte here comes from a node over the network. Failure modes
//! that matter are silent: a log decoded from a corrupted frame into a
//! plausible pool event, a snapshot whose ticks are out of order or
//! outside the coverage it claims, a payload the member cannot decode.
//!
//! Mode by the first byte:
//!
//! * `0` — the bytes are a log push: `parse_log` never panics; anything
//!   it accepts publishes payloads that decode.
//! * `1` — the bytes are an envelope: the head / subscription-id / id
//!   scanners never panic.
//! * `2` — the bytes are a node's replies to a two-pool snapshot (one V3,
//!   one Algebra): each read gets the next chunk as its `result` (or an
//!   error reply). No panic; if the snapshot completes, every emitted
//!   payload decodes, every pool's `TICK`s are strictly ascending inside
//!   the `[lo, hi]` its `SNAPSHOT` claims, their count matches, and the
//!   price sits inside the coverage.

#![no_main]

use libfuzzer_sys::fuzz_target;

use core_amm::payload::{decode, PoolEvent};
use ingress_hyperevm::rpc::{parse_head, push_sub_id, response_id, subscribe_result};
use ingress_hyperevm::{
    parse_log, payloads, PoolEntry, PoolFamily, PoolTable, SnapState, Snapshotter,
};

fuzz_target!(|d: &[u8]| {
    if d.is_empty() {
        return;
    }
    let body = &d[1..];
    match d[0] % 3 {
        0 => {
            if let Ok((meta, log)) = parse_log(body) {
                if let Some((ps, n)) = payloads(&meta, &log) {
                    let mut i = 0;
                    while i < n {
                        assert!(
                            decode(&ps[i]).is_some(),
                            "an accepted log published an undecodable payload"
                        );
                        i += 1;
                    }
                }
            }
        }
        1 => {
            let _ = parse_head(body);
            let _ = push_sub_id(body);
            let _ = subscribe_result(body);
            let _ = response_id(body);
        }
        _ => {
            let t = PoolTable::new(&[
                PoolEntry {
                    address: [1; 20],
                    sym: 1,
                    family: PoolFamily::UniswapV3,
                },
                PoolEntry {
                    address: [2; 20],
                    sym: 2,
                    family: PoolFamily::Algebra,
                },
            ])
            .expect("fuzz pools");
            let mut s = Snapshotter::new(&t, 200);
            s.begin(46_650_000);
            let mut i = 0usize;
            let mut guard = 0;
            while guard < 4_096 {
                guard += 1;
                let Some(c) = s.next_call() else { break };
                if i >= body.len() {
                    break;
                }
                let len = (body[i] as usize) * 3 % 400;
                i += 1;
                let end = (i + len).min(body.len());
                let chunk = &body[i..end];
                i = end;
                if !chunk.is_empty() && chunk[0] == 0xff {
                    s.on_result(c, None);
                } else {
                    s.on_result(c, Some(chunk));
                }
            }
            if s.state() != SnapState::Ready {
                return;
            }
            let mut open: Option<(u32, i32, i32, u16, u16)> = None; // sym, lo, hi, want, got
            let mut last_tick = i32::MIN;
            while let Some((sym, p)) = s.next_signal() {
                match decode(&p).expect("snapshot payloads decode") {
                    PoolEvent::Snapshot { lo, hi, nodes, .. } => {
                        assert!(open.is_none());
                        open = Some((sym, lo, hi, nodes, 0));
                        last_tick = i32::MIN;
                    }
                    PoolEvent::Tick { tick, .. } => {
                        let o = open.as_mut().expect("TICK inside a snapshot");
                        assert_eq!(o.0, sym);
                        assert!(
                            tick > last_tick && tick >= o.1 && tick <= o.2,
                            "tick outside its coverage or out of order"
                        );
                        last_tick = tick;
                        o.4 += 1;
                    }
                    PoolEvent::State { tick, snapshot, .. } => {
                        let o = open.take().expect("STATE closes a snapshot");
                        assert!(snapshot && o.0 == sym && o.3 == o.4, "node count mismatch");
                        assert!(tick >= o.1 && tick <= o.2, "price outside the coverage");
                    }
                    other => panic!("unexpected snapshot payload {other:?}"),
                }
            }
            assert!(open.is_none());
        }
    }
});
