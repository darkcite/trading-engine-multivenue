// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! pm-user on the rig: the subscribe it sends and what each frame becomes.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `user.rs`): every copy here builds a fixture or records the fake wire.

use super::*;
use crate::cmd::{EVT_FILL, EVT_GAP};
use crate::userws::{TS_MATCHED, TS_MATCHED_NOT_BROADCASTED};
use core_ring::{Consumer, Ring};
use signer_eip712::{address_from_private_key, parse_secret_key};

const HARDHAT: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

#[derive(Default)]
struct Wire {
    frames: Vec<Vec<u8>>,
}

impl Pm15Wire for Wire {
    fn is_open(&self) -> bool {
        true
    }
    fn queue(&mut self, parts: &[&[u8]]) -> bool {
        self.frames.push(parts.concat());
        true
    }
}

fn driver() -> (
    PmUserDriver,
    Consumer<PmEvt, PM_USER_RING>,
    Arc<GatewayStats>,
) {
    let sk = parse_secret_key(&HARDHAT).unwrap();
    let me = address_from_private_key(&HARDHAT).unwrap();
    let acct = PmAccount::new(0, me, None, b"key-1", b"c2VjcmV0LXNlY3JldA==", b"pp", sk).unwrap();
    let (tx, rx) = Ring::<PmEvt, PM_USER_RING>::new().split();
    // The arm's shared block, as the duplex makes it.
    let (eng, _gw) = exec_gateway::duplex::<
        crate::cmd::PmCmd,
        PmEvt,
        { crate::cmd::PM_CMD_RING },
        { crate::cmd::PM_EVT_RING },
    >();
    let stats = Arc::clone(&eng.stats);
    (PmUserDriver::new(&acct, tx, Arc::clone(&stats)), rx, stats)
}

fn drain(rx: &mut Consumer<PmEvt, PM_USER_RING>) -> Vec<PmEvt> {
    let mut v = Vec::new();
    while let Some(e) = rx.try_pop_ref() {
        v.push(*e);
    }
    v
}

const OURS: &str = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn trade(id: &str, side: &str, status: &str) -> String {
    format!(
        r#"{{"event_type":"trade","type":"TRADE","id":"{id}","taker_order_id":"{OURS}","market":"0x1","asset_id":"1234","side":"BUY","size":"10","fee_rate_bps":"0","price":"0.93","status":"{status}","owner":"key-1","trader_side":"{side}","timestamp":"1792217690123","maker_orders":[{{"price":"0.91","side":"SELL"}}]}}"#
    )
}

#[test]
fn the_subscribe_carries_the_raw_secret_and_trades_become_fills() {
    let (mut d, mut rx, stats) = driver();
    let mut w = Wire::default();
    assert!(d.on_open(&mut w));
    assert_eq!(
        w.frames[0],
        br#"{"auth":{"apiKey":"key-1","secret":"c2VjcmV0LXNlY3JldA==","passphrase":"pp"},"type":"user"}"#
    );
    d.on_text(b"PONG", 5);
    let frame = format!(
        "[{},{},{}]",
        trade("t-1", "TAKER", "MATCHED"),
        trade("t-1", "TAKER", "TRADE_STATUS_MATCHED_NOT_BROADCASTED"),
        trade("t-2", "MAKER", "MATCHED"),
    );
    d.on_text(frame.as_bytes(), 6);
    let ev = drain(&mut rx);
    assert_eq!(ev.len(), 2, "the maker trade is not ours");
    assert_eq!(ev[0].kind, EVT_FILL);
    assert_eq!(&ev[0].order_id[..2], &[0x5a, 0x5a]);
    assert_eq!(
        (ev[0].a, ev[0].b, ev[0].c, ev[0].why),
        (
            crate::userws::fnv1a(b"t-1"),
            930_000,
            10_000_000,
            TS_MATCHED
        )
    );
    assert_eq!(ev[1].why, TS_MATCHED_NOT_BROADCASTED);
    let c = d.counters();
    assert_eq!((c.frames, c.pongs, c.fills, c.maker_trades), (2, 1, 2, 1));
    assert_eq!(stats.published(PUB_USER_RX_NS), 6, "each frame stamped");
    d.on_down();
    assert_eq!(drain(&mut rx)[0].kind, EVT_GAP);
    assert_eq!(stats.halt_reason(), exec_gateway::HALT_NONE);
    // A frame that does not scan halts the arm.
    d.on_text(br#"{"event_type":"trade","id":"x"}"#, 7);
    assert_eq!(stats.halt_reason(), HALT_USER_SCAN);
    assert_eq!(d.counters().scan_errors, 1);
}
