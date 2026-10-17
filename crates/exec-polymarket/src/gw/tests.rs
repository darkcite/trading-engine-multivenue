// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! pm-gw on the rig: a scripted CLOB that records every request and
//! answers from a script.
//!
//! COPY-DOCTRINE: a test-only module (`#[cfg(test)] mod tests;` in
//! `gw.rs`): every copy here builds a fixture or records the rig's wire.

use super::*;
use crate::cmd::EVT_FILL;
use core_net::PostErrKind;
use core_ring::Ring;
use core_types::pm_roll::{pm_instance_id, pm_window_of_open};
use exec_gateway::duplex;
use ingress_polymarket::gamma15::PmTokenId;
use signer_eip712::{address_from_private_key, parse_secret_key};
use std::collections::VecDeque;

const HARDHAT: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];
const KEY: &[u8] = b"00000000-0000-0000-0000-000000000000";
const SECRET: &[u8] = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const PASS: &[u8] = b"pass-phrase";
const OPEN: u64 = 1_792_216_800;
const COND: &[u8] = b"0x53efa9a4a5bddfd12a4d154653e337cedf2ec6667842dca165036ac21f6765b7";
const WALL: u64 = 1_792_217_690_123_000_000;

#[derive(Debug, Clone)]
struct Sent {
    method: Method,
    target: Vec<u8>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    body: Vec<u8>,
}

#[derive(Default)]
struct Rig {
    body: Vec<u8>,
    resp: Vec<u8>,
    script: VecDeque<Result<(u16, Vec<u8>), PostErr>>,
    sent: Vec<Sent>,
}

impl PmClob for Rig {
    fn body_mut(&mut self) -> &mut [u8] {
        &mut self.body
    }
    fn request(
        &mut self,
        method: Method,
        target: &[u8],
        extra: &[Header<'_>],
        body_len: usize,
    ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
        self.sent.push(Sent {
            method,
            target: target.to_vec(),
            headers: extra.iter().map(|(n, v)| (n.to_vec(), v.to_vec())).collect(),
            body: self.body[..body_len].to_vec(),
        });
        match self.script.pop_front().expect("the script has an answer") {
            Ok((http, b)) => {
                self.resp = b;
                Ok((http, 0..self.resp.len()))
            }
            Err(e) => Err(e),
        }
    }
    fn resp(&self) -> &[u8] {
        &self.resp
    }
}

fn account() -> PmAccount {
    let sk = parse_secret_key(&HARDHAT).unwrap();
    let me = address_from_private_key(&HARDHAT).unwrap();
    PmAccount::new(0, me, None, KEY, SECRET, PASS, sk).expect("a valid EOA account")
}

fn token(s: &[u8]) -> PmTokenId {
    let mut t = PmTokenId::NONE;
    t.b[..s.len()].copy_from_slice(s);
    t.len = s.len() as u8;
    t
}

fn spec() -> PmWindowSpec {
    let mut s = PmWindowSpec::EMPTY;
    s.family = 0;
    s.window = pm_window_of_open(OPEN).unwrap();
    s.tick_1e6 = 10_000;
    s.min_size_1e6 = 5_000_000;
    s.up = token(b"1234");
    s.down = token(b"5678");
    s.condition_id[..66].copy_from_slice(COND);
    s.condition_id_len = 66;
    s
}

struct Bench {
    gw: PmGateway<Rig>,
    eng: crate::arm::PmArmEnd,
    specs_tx: core_ring::Producer<PmWindowSpec, PM15_SPEC_RING>,
}

fn bench() -> Bench {
    let (eng, end) = duplex::<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>();
    let (specs_tx, specs_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let rig = Rig {
        body: vec![0; BODY_MIN],
        ..Rig::default()
    };
    let mut b = Bench {
        gw: PmGateway::new(account(), rig, end, specs_rx, 7),
        eng,
        specs_tx,
    };
    assert!(b.specs_tx.try_push_ref(&spec()));
    b.gw.step(1, WALL);
    assert_eq!(b.gw.counters().specs, 1);
    b
}

fn instance() -> u32 {
    pm_instance_id(pm_window_of_open(OPEN).unwrap(), 0).unwrap()
}

fn submit(cid: u64) -> PmCmd {
    let mut c = PmCmd::new(CMD_SUBMIT, cid);
    c.px_1e6 = 930_000;
    c.usd_1e6 = 249_990_000;
    c.instance = instance();
    c.tick_1e6 = 10_000;
    c.leg = 0;
    c
}

fn events(b: &mut Bench) -> Vec<PmEvt> {
    let mut v = Vec::new();
    while let Some(e) = b.eng.evt.pop() {
        v.push(*e);
    }
    v
}

fn hex(id: &[u8; 32]) -> String {
    let mut s = String::from("0x");
    for b in id {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn accepted(id: &[u8; 32], trades: &str) -> Vec<u8> {
    format!(
        r#"{{"success":true,"errorMsg":"","orderID":"{}","status":"matched","makingAmount":"249.99","takingAmount":"268.8","transactionsHashes":[],"tradeIDs":{trades}}}"#,
        hex(id)
    )
    .into_bytes()
}

/// The digest pm-gw will sign for the next submit: run it once on a
/// throwaway rig with the same seed and read its SENT.
fn digest_of_next(cid: u64) -> [u8; 32] {
    let mut b = bench();
    b.gw.clob_mut()
        .script
        .push_back(Ok((400, br#"{"error":"not enough balance / allowance"}"#.to_vec())));
    assert!(b.eng.cmd.push(&submit(cid)));
    b.gw.step(2, WALL);
    let ev = events(&mut b);
    assert_eq!(ev[0].kind, EVT_SENT);
    ev[0].order_id
}

#[test]
fn a_submit_is_signed_rendered_in_place_maced_sent_then_acked() {
    let id = digest_of_next(41);
    let mut b = bench();
    b.gw.clob_mut().script.push_back(Ok((200, accepted(&id, r#"["t-1"]"#))));
    assert!(b.eng.cmd.push(&submit(41)));
    assert!(b.gw.step(2, WALL));
    let ev = events(&mut b);
    assert_eq!(ev.len(), 2, "{ev:?}");
    assert_eq!((ev[0].kind, ev[0].a, ev[0].order_id), (EVT_SENT, 41, id));
    assert_eq!((ev[1].kind, ev[1].a, ev[1].b), (EVT_ACK, 41, 1));
    let s = &b.gw.clob_mut().sent[0];
    assert_eq!((s.method, s.target.as_slice()), (Method::Post, &b"/order"[..]));
    let body = std::str::from_utf8(&s.body).unwrap();
    assert!(body.starts_with(r#"{"order":{"maker":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","signer":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","tokenId":"1234","side":"BUY""#), "{body}");
    // $249.99 at 0.93 on tick 0.01: 268.8064… shares, truncated to the
    // 4-decimal grain.
    assert!(body.contains(r#""makerAmount":"249990000","takerAmount":"268806400""#), "{body}");
    assert!(body.ends_with(r#""owner":"00000000-0000-0000-0000-000000000000","orderType":"FAK","deferExec":false,"postOnly":false}"#));
    // The MAC covers ts ‖ POST ‖ /order ‖ the exact body sent.
    let h = |n: &[u8]| s.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap();
    assert_eq!(h(b"POLY_TIMESTAMP"), b"1792217690");
    assert_eq!(h(b"POLY_ADDRESS"), b"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    assert_eq!(h(b"POLY_API_KEY"), KEY);
    assert_eq!(h(b"POLY_PASSPHRASE"), PASS);
    let want = crate::l2::L2Secret::from_base64(SECRET)
        .unwrap()
        .sign(&[b"1792217690", b"POST", b"/order", &s.body]);
    assert_eq!(h(b"POLY_SIGNATURE"), want);
    assert_eq!(b.gw.counters().posts, 1);
}

#[test]
fn a_zero_fill_fak_and_each_halting_class_are_refusals() {
    let mut b = bench();
    b.gw.clob_mut().script.push_back(Ok((
        400,
        br#"{"error":"no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found."}"#.to_vec(),
    )));
    assert!(b.eng.cmd.push(&submit(1)));
    b.gw.step(2, WALL);
    let ev = events(&mut b);
    assert_eq!((ev[1].kind, ev[1].why, ev[1].b), (EVT_REFUSED, Refusal::NoMatch as u8, 400));
    assert!(!b.gw.halted());
    // 401: the account is refused — halt; the next submit never leaves.
    b.gw.clob_mut()
        .script
        .push_back(Ok((401, br#"{"error":"Unauthorized/Invalid api key"}"#.to_vec())));
    assert!(b.eng.cmd.push(&submit(2)));
    b.gw.step(3, WALL);
    let ev = events(&mut b);
    assert_eq!((ev[1].kind, ev[1].why), (EVT_REFUSED, Refusal::Auth as u8));
    assert!(b.gw.halted());
    assert_eq!(b.eng.stats.halt_reason(), HALT_REFUSAL);
    assert!(b.eng.cmd.push(&submit(3)));
    b.gw.step(4, WALL);
    let ev = events(&mut b);
    assert_eq!((ev.len(), ev[0].kind, ev[0].why), (1, EVT_REFUSED, LOCAL_REQUEST));
    assert_eq!(b.gw.clob_mut().sent.len(), 2, "nothing more on the wire");
}

#[test]
fn the_retry_law_resends_the_same_bytes_once_then_doubts() {
    let id = digest_of_next(9);
    let mut b = bench();
    let lost = PostErr {
        err: PostErrKind::Disconnected,
        left_host: false,
    };
    b.gw.clob_mut().script.push_back(Err(lost));
    b.gw.clob_mut().script.push_back(Ok((200, accepted(&id, "[]"))));
    assert!(b.eng.cmd.push(&submit(9)));
    b.gw.step(2, WALL);
    let ev = events(&mut b);
    assert_eq!((ev.len(), ev[1].kind), (2, EVT_ACK), "{ev:?}");
    let sent = &b.gw.clob_mut().sent;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].body, sent[1].body, "the same salt and timestamp");
    assert_eq!(b.gw.counters().resends, 1);
    // `order timed out` (rejected before the book), then the venue says
    // the copy is a duplicate: the first one's fate stands — in doubt.
    b.gw.clob_mut().script.push_back(Ok((500, br#"{"error":"order timed out"}"#.to_vec())));
    b.gw.clob_mut().script.push_back(Ok((
        400,
        br#"{"error":"order 0x12 is invalid. Duplicated."}"#.to_vec(),
    )));
    assert!(b.eng.cmd.push(&submit(10)));
    b.gw.step(3, WALL);
    let ev = events(&mut b);
    assert_eq!(ev[1].kind, EVT_IN_DOUBT);
    assert_eq!(ev[1].order_id, ev[0].order_id);
    // Two transport failures: in doubt.
    let gone = PostErr {
        err: PostErrKind::Disconnected,
        left_host: true,
    };
    b.gw.clob_mut().script.push_back(Err(gone));
    b.gw.clob_mut().script.push_back(Err(gone));
    assert!(b.eng.cmd.push(&submit(11)));
    b.gw.step(4, WALL);
    let ev = events(&mut b);
    assert_eq!(ev[1].kind, EVT_IN_DOUBT);
    assert_eq!(b.gw.counters().in_doubt, 2);
}

#[test]
fn an_answer_naming_another_order_id_halts_and_doubts() {
    let mut b = bench();
    b.gw.clob_mut().script.push_back(Ok((200, accepted(&[7; 32], "[]"))));
    assert!(b.eng.cmd.push(&submit(5)));
    b.gw.step(2, WALL);
    let ev = events(&mut b);
    assert_eq!(ev[1].kind, EVT_IN_DOUBT);
    assert!(b.gw.halted());
    assert_eq!(b.eng.stats.halt_reason(), HALT_ORDER_ID);
}

#[test]
fn local_refusals_never_reach_the_wire() {
    let mut b = bench();
    let mut c = submit(1);
    c.instance += 8; // the next window: not in the table
    assert!(b.eng.cmd.push(&c));
    b.gw.step(2, WALL);
    let mut c = submit(2);
    c.px_1e6 = 935_000; // off the 0.01 tick
    assert!(b.eng.cmd.push(&c));
    b.gw.step(3, WALL);
    let mut c = submit(3);
    c.usd_1e6 = 2_000_000; // 2.15 shares: under the 5-share floor
    assert!(b.eng.cmd.push(&c));
    b.gw.step(4, WALL);
    let ev = events(&mut b);
    let why: Vec<u8> = ev.iter().map(|e| e.why).collect();
    assert_eq!(why, vec![LOCAL_NO_TOKEN, LOCAL_AMOUNTS, LOCAL_FLOOR]);
    assert!(b.gw.clob_mut().sent.is_empty());
    assert_eq!(b.gw.counters().refused_local, 3);
}

#[test]
fn recon_reads_the_accounts_trades_and_emits_the_orders_own_once_paced() {
    let mut b = bench();
    let ours = [0x5a; 32];
    let page1 = format!(
        r#"{{"limit":100,"next_cursor":"MTAw","count":2,"data":[
          {{"id":"t-1","taker_order_id":"{}","market":"{}","asset_id":"1234","side":"BUY","size":"100.5","price":"0.93","status":"TRADE_STATUS_MATCHED","trader_side":"TAKER","maker_orders":[{{"price":"0.91"}}]}},
          {{"id":"t-x","taker_order_id":"0x{}","market":"{}","asset_id":"1234","side":"BUY","size":"9","price":"0.5","status":"MATCHED","trader_side":"TAKER"}}]}}"#,
        hex(&ours),
        std::str::from_utf8(COND).unwrap(),
        "11".repeat(32),
        std::str::from_utf8(COND).unwrap(),
    );
    let page2 = format!(
        r#"{{"next_cursor":"LTE=","data":[{{"id":"t-2","taker_order_id":"{}","asset_id":"1234","side":"BUY","size":"10","price":"0.92","status":"CONFIRMED","trader_side":"TAKER"}}]}}"#,
        hex(&ours)
    );
    // The first read fails: queued again, paced.
    b.gw.clob_mut().script.push_back(Ok((502, b"bad gateway".to_vec())));
    b.gw.clob_mut().script.push_back(Ok((200, page1.into_bytes())));
    b.gw.clob_mut().script.push_back(Ok((200, page2.into_bytes())));
    let mut c = PmCmd::new(CMD_RECON, 77);
    c.order_id = ours;
    c.instance = instance();
    assert!(b.eng.cmd.push(&c));
    b.gw.step(10, WALL);
    assert!(events(&mut b).is_empty());
    assert_eq!(b.gw.counters().recon_failures, 1);
    b.gw.step(10 + RECON_GAP_NS - 1, WALL);
    assert_eq!(b.gw.clob_mut().sent.len(), 1, "paced");
    b.gw.step(10 + RECON_GAP_NS, WALL);
    let ev = events(&mut b);
    assert_eq!(ev.len(), 3, "{ev:?}");
    assert_eq!((ev[0].kind, ev[0].order_id, ev[0].b, ev[0].c), (EVT_FILL, ours, 930_000, 100_500_000));
    assert_eq!(ev[0].a, crate::userws::fnv1a(b"t-1"));
    assert_eq!((ev[1].kind, ev[1].b, ev[1].why), (EVT_FILL, 920_000, crate::userws::TS_CONFIRMED));
    assert_eq!((ev[2].kind, ev[2].a, ev[2].b), (EVT_DONE, 77, 2));
    let sent = &b.gw.clob_mut().sent;
    assert_eq!(sent[1].method, Method::Get);
    let t1 = std::str::from_utf8(&sent[1].target).unwrap();
    assert_eq!(t1, format!("/data/trades?market={}&after=1792217390", std::str::from_utf8(COND).unwrap()));
    assert!(std::str::from_utf8(&sent[2].target).unwrap().ends_with("&next_cursor=MTAw"));
    let h = sent[1].headers.iter().find(|(k, _)| k == b"POLY_SIGNATURE").unwrap();
    let want = crate::l2::L2Secret::from_base64(SECRET)
        .unwrap()
        .sign(&[b"1792217690", b"GET", b"/data/trades", b""]);
    assert_eq!(h.1, want, "the path is signed, the query is not");
}

#[test]
fn recon_gives_up_after_its_tries_and_halts() {
    let mut b = bench();
    let mut c = PmCmd::new(CMD_RECON, 3);
    c.instance = instance();
    c.tries = RECON_TRIES_MAX - 1;
    b.gw.clob_mut().script.push_back(Ok((500, Vec::new())));
    assert!(b.eng.cmd.push(&c));
    b.gw.step(10, WALL);
    let ev = events(&mut b);
    assert_eq!((ev[0].kind, ev[0].a, ev[0].b), (EVT_DONE, 3, -1));
    assert_eq!(b.eng.stats.halt_reason(), HALT_RECON);
}
