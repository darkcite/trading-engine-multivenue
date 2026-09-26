// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The XH4b order gateway against a scripted Hyperliquid on `127.0.0.1`
//! (`exec_hyperliquid::testvenue`, feature `testvenue`): a rustls server
//! that upgrades `/ws`, acknowledges the two subscribes, sends the
//! `userFills` snapshot, answers the application ping, answers every post
//! by the test's policy (on the socket it came on), pushes stream frames
//! to every live socket — as the venue does to two subscribed
//! connections during a swap — and serves `POST /exchange` for the REST
//! fallback.
//!
//! | test | asserts |
//! |---|---|
//! | coalesced | the first snapshot is history; three ALO places are ONE signed post; its statuses map item by item; a cancel is a cancelByCloid post; a live fill is FILL + FILL_META (fee, crossed); tid and orderUpdates dedupe |
//! | not ready | before the first snapshot, growth is refused unsent and a cancel still goes; a foreign slot and a Gtc place are refused |
//! | classes | 40 ALO places split 39 + 1; an IoC and a modify go in their own posts; batches post in the order they opened |
//! | ordering | a cancel never overtakes the place it names |
//! | silent | an unanswered post is UNANSWERED after its deadline, never REJECTED |
//! | dropped | a socket that dies strands its posts (UNANSWERED), the gateway redials and reports the gap |
//! | rest | with no socket left a post goes to `/exchange` — the envelope alone — and is answered |
//! | swap | make-before-break: the old socket's in-flight answer still lands, new posts use the new socket |
//! | halted | a halted gateway refuses growth unsent; a cancel still goes |
//! | refusals | a malformed or miscounted answer and the API layer's error are UNANSWERED; a refused action rejects every item |
//!
//! Offline path: this file MAY allocate.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_types::{ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_MARGIN};
use exec_gateway::stats::HALT_VENUE_BASE;
use exec_gateway::Idle;
use exec_hyperliquid::cloid;
use exec_hyperliquid::cmd::{HlCmd, CMD_CANCEL, CMD_PLACE};
use exec_hyperliquid::event::{
    ExecEvent, EVF_BUY, EVF_CROSSED, EVF_RESTING, EV_ACK, EV_FILL, EV_FILLED, EV_FILL_META, EV_GAP,
    EV_REJECTED, EV_SNAPSHOT, EV_UNANSWERED, REASON_HALTED, REASON_LOCAL, REASON_MALFORMED,
    REASON_NOT_READY, REASON_RATE_LIMIT, REASON_TRANSPORT,
};
use exec_hyperliquid::gateway::{hl_duplex, CoinMap, GatewayCfg, HlEngineEnd, HlGateway};
use exec_hyperliquid::testvenue::{items_in, resting, Answer, Venue};
use exec_hyperliquid::{Network, Tif};

const SLOT: u8 = 6;
const ETH: u32 = 1;
const CANCEL_OK: &str =
    r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;

fn gateway(v: &Venue, post_timeout_ns: u64) -> (HlGateway, HlEngineEnd) {
    let (eng, gw_end) = hl_duplex();
    let mut coins = CoinMap::new();
    assert!(coins.insert(b"ETH", ETH));
    let cfg = GatewayCfg {
        host: "localhost".to_string(),
        port: v.port,
        tls: Arc::clone(&v.tls),
        network: Network::Testnet,
        master: [0xA5; 20],
        slots: 1 << SLOT,
        post_timeout_ns,
        idle: Idle::new(0, 1_000_000),
    };
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("key");
    let gw = HlGateway::new(cfg, gw_end, sk, coins).expect("gateway");
    (gw, eng)
}

/// Turn the gateway, collecting events, until `done` or `secs` pass.
fn drive(
    gw: &mut HlGateway,
    eng: &mut HlEngineEnd,
    evs: &mut Vec<ExecEvent>,
    secs: u64,
    mut done: impl FnMut(&HlGateway, &[ExecEvent]) -> bool,
) -> bool {
    let t0 = Instant::now();
    loop {
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            evs.push(*e);
        }
        if done(gw, evs) {
            return true;
        }
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
    }
}

fn count(evs: &[ExecEvent], kind: u8) -> usize {
    evs.iter().filter(|e| e.kind == kind).count()
}

/// Up and trading: a live socket and the first snapshot absorbed.
fn live(gw: &mut HlGateway, eng: &mut HlEngineEnd, evs: &mut Vec<ExecEvent>) {
    assert!(
        drive(gw, eng, evs, 10, |g, e| g.ws_live()
            && count(e, EV_SNAPSHOT) >= 1),
        "the socket never came up with its snapshot"
    );
}

fn cloid_hex(slot: u8, client_oid: u64) -> String {
    let mut out = [0u8; 34];
    cloid::to_hex(&cloid::encode(slot, client_oid), &mut out);
    String::from_utf8(out.to_vec()).unwrap()
}

fn alo(client_oid: u64) -> HlCmd {
    HlCmd::place(
        SLOT,
        client_oid,
        ETH,
        true,
        250_000_000_000,
        10_000_000,
        Tif::Alo,
    )
}

fn fill_row(tid: u64, fee: &str, crossed: bool) -> String {
    format!(
        r#"{{"coin":"ETH","px":"2500.5","sz":"0.1","side":"B","time":1700000000000,"startPosition":"0","dir":"Open Long","closedPnl":"0","hash":"0x0","oid":101,"crossed":{crossed},"fee":"{fee}","tid":{tid},"cloid":"{}","feeToken":"USDC"}}"#,
        cloid_hex(SLOT, 1)
    )
}

#[test]
fn coalesced_places_are_one_post_answered_item_by_item_and_the_streams_route() {
    let v = Venue::start();
    v.snapshot(Some(&format!("[{}]", fill_row(900, "0.1", true))));
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            Answer::Action(CANCEL_OK.into())
        } else {
            Answer::Action(r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":101}},{"resting":{"oid":102}},{"error":"Post only order would have immediately matched, bbo was 1@2. asset=1"}]}}}"#.into())
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let snap = evs.iter().find(|e| e.kind == EV_SNAPSHOT).unwrap();
    assert_eq!((snap.aux, snap.venue_ms), (1, 1_700_000_000_000));
    assert_eq!(
        count(&evs, EV_FILL),
        0,
        "the boot snapshot is not new trading"
    );

    for oid in 1..=3u64 {
        assert!(eng.cmd.push(&alo(oid)));
    }
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) + count(
        e,
        EV_REJECTED
    ) >= 3));
    let posts = v.posts();
    assert_eq!(posts.len(), 1, "three ALO places are ONE post");
    let body = &posts[0].body;
    assert!(body.starts_with(r#"{"method":"post","id":"#), "{body}");
    assert!(body.contains(r#""type":"order""#) && body.contains(r#""grouping":"na""#));
    assert_eq!(body.matches(r#""tif":"Alo""#).count(), 3);
    assert!(body.contains(r#""signature":{"r":"0x"#) && body.contains(r#""nonce":"#));
    assert!(body.contains(&cloid_hex(SLOT, 2)));
    let acks: Vec<&ExecEvent> = evs.iter().filter(|e| e.kind == EV_ACK).collect();
    assert_eq!((acks[0].client_oid, acks[0].oid), (1, 101));
    assert_eq!((acks[1].client_oid, acks[1].oid), (2, 102));
    assert!(acks[0].flags & EVF_RESTING != 0 && acks[0].flags & EVF_BUY != 0);
    assert_eq!(
        (acks[0].slot, acks[0].asset, acks[0].cmd_kind()),
        (SLOT, ETH, CMD_PLACE)
    );
    let rej = evs.iter().find(|e| e.kind == EV_REJECTED).unwrap();
    assert_eq!(
        (rej.client_oid, rej.reason),
        (3, ORDER_EVENT_REASON_BAD_ALO_PX)
    );
    assert_eq!(rej.aux, acks[0].aux, "every item answers the same post");

    // A cancel is its own cancelByCloid post.
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 1, ETH)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| e
        .iter()
        .any(|x| x.kind == EV_ACK && x.cmd_kind() == CMD_CANCEL)));
    assert!(v.posts()[1].body.contains(r#""type":"cancelByCloid""#));

    // A live fill is FILL + FILL_META; the snapshot's tid is not re-booked.
    v.push_all(&format!(
        r#"{{"channel":"userFills","data":{{"user":"0x","fills":[{},{}]}}}}"#,
        fill_row(901, "-0.0025", false),
        fill_row(900, "0.1", true)
    ));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_FILL_META
    ) >= 1));
    let f = evs.iter().find(|e| e.kind == EV_FILL).unwrap();
    assert_eq!(
        (f.aux, f.client_oid, f.slot, f.oid, f.asset),
        (901, 1, SLOT, 101, ETH)
    );
    assert_eq!(
        (f.px_1e8, f.sz_1e8, f.venue_ms),
        (250_050_000_000, 10_000_000, 1_700_000_000_000)
    );
    let m = evs.iter().find(|e| e.kind == EV_FILL_META).unwrap();
    assert_eq!(
        (m.aux, m.px_1e8),
        (901, -250_000),
        "the fee, signed: a rebate"
    );
    assert_eq!(m.flags & EVF_CROSSED, 0);
    assert_eq!(
        count(&evs, EV_FILL),
        1,
        "tid 900 was absorbed with the snapshot"
    );

    // orderUpdates: one event per status change, duplicates dropped.
    let upd = format!(
        r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"ETH","side":"B","limitPx":"2500.5","sz":"0.0","oid":101,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"filled","statusTimestamp":1700000000001}}]}}"#,
        cloid_hex(SLOT, 1)
    );
    v.push_all(&upd);
    v.push_all(&upd);
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_FILLED
    ) >= 1));
    let _ = drive(&mut gw, &mut eng, &mut evs, 1, |_, _| false);
    assert_eq!(
        count(&evs, EV_FILLED),
        1,
        "a duplicate orderUpdates row is dropped"
    );
    let filled = evs.iter().find(|e| e.kind == EV_FILLED).unwrap();
    assert_eq!((filled.client_oid, filled.oid, filled.asset), (1, 101, ETH));
    assert_eq!(gw.counters().fills, 1);
}

#[test]
fn before_the_first_snapshot_growth_is_refused_and_foreign_slots_never_post() {
    let v = Venue::start();
    v.snapshot(None);
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            Answer::Action(CANCEL_OK.into())
        } else {
            Answer::Action(resting(items_in(&s.body), 60))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    assert!(drive(&mut gw, &mut eng, &mut evs, 10, |g, _| g.ws_live()));
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 1, ETH)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 1));
    let rej = evs.iter().find(|e| e.kind == EV_REJECTED).unwrap();
    assert_eq!((rej.client_oid, rej.reason), (1, REASON_NOT_READY));
    assert_eq!(v.posts().len(), 1, "only the cancel left the host");

    v.push_all(r#"{"channel":"userFills","data":{"isSnapshot":true,"user":"0x","fills":[]}}"#);
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_SNAPSHOT
    ) >= 1));
    let before = count(&evs, EV_REJECTED);
    assert!(eng.cmd.push(&HlCmd::place(3, 5, ETH, true, 1, 1, Tif::Alo)));
    assert!(eng
        .cmd
        .push(&HlCmd::place(SLOT, 6, ETH, true, 1, 1, Tif::Gtc)));
    assert!(eng.cmd.push(&alo(7)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 2));
    let refused: Vec<&ExecEvent> = evs
        .iter()
        .filter(|e| e.kind == EV_REJECTED)
        .skip(before)
        .collect();
    assert_eq!(refused.len(), 2);
    assert!(
        refused.iter().all(|e| e.reason == REASON_LOCAL),
        "a foreign slot and a Gtc place"
    );
    assert_eq!(
        evs.iter().rfind(|e| e.kind == EV_ACK).unwrap().oid,
        60,
        "growth flows once the snapshot is absorbed"
    );
    assert_eq!(v.posts().len(), 2);
}

#[test]
fn classes_split_at_39_and_never_mix() {
    let v = Venue::start();
    v.policy(|s| Answer::Action(resting(items_in(&s.body), 1)));
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    for oid in 1..=40u64 {
        assert!(eng.cmd.push(&alo(oid)));
    }
    let ioc = HlCmd::place(SLOT, 41, ETH, false, 240_000_000_000, 10_000_000, Tif::Ioc);
    assert!(eng.cmd.push(&ioc));
    assert!(eng.cmd.push(&alo(42).replacing(1)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 42));
    let posts = v.posts();
    assert_eq!(posts.len(), 4, "39 + 1 ALO, one IoC, one modify");
    assert_eq!(items_in(&posts[0].body), 39);
    assert_eq!(posts[0].body.matches(r#""tif":"Alo""#).count(), 39);
    assert_eq!(items_in(&posts[1].body), 1);
    assert!(posts[2].body.contains(r#""tif":"Ioc""#) && !posts[2].body.contains("Alo"));
    assert!(posts[3].body.contains(r#""type":"batchModify""#));
    assert!(posts[3]
        .body
        .contains(&format!(r#""oid":"{}""#, cloid_hex(SLOT, 1))));
    assert_eq!(count(&evs, EV_REJECTED) + count(&evs, EV_UNANSWERED), 0);
}

#[test]
fn a_cancel_never_overtakes_the_place_it_names() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            Answer::Action(CANCEL_OK.into())
        } else {
            Answer::Action(resting(items_in(&s.body), 7))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 99, ETH)));
    assert!(eng.cmd.push(&alo(7)));
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 7, ETH)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 3));
    let posts = v.posts();
    let place = posts
        .iter()
        .position(|p| p.body.contains(r#""type":"order""#))
        .unwrap();
    let last_cancel = posts
        .iter()
        .rposition(|p| p.body.contains(&cloid_hex(SLOT, 7)) && p.body.contains("cancelByCloid"))
        .unwrap();
    assert!(
        place < last_cancel,
        "the cancel of 7 went out before 7 was placed"
    );
}

#[test]
fn a_silent_venue_leaves_posts_unanswered_not_rejected() {
    let v = Venue::start();
    v.policy(|_| Answer::Silent);
    let (mut gw, mut eng) = gateway(&v, 300_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&alo(2)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_UNANSWERED
    ) >= 2));
    assert!(evs
        .iter()
        .filter(|e| e.kind == EV_UNANSWERED)
        .all(|e| e.reason == REASON_TRANSPORT));
    assert_eq!(
        count(&evs, EV_REJECTED),
        0,
        "silence is never a rejection (E-5)"
    );
    assert_eq!(eng.stats.io()[3], 1, "one post timed out");
}

#[test]
fn a_dropped_socket_strands_its_posts_and_the_gateway_redials() {
    let v = Venue::start();
    let first = Arc::new(AtomicBool::new(true));
    let f2 = Arc::clone(&first);
    v.policy(move |s| {
        if f2.swap(false, Ordering::SeqCst) {
            Answer::Drop
        } else {
            Answer::Action(resting(items_in(&s.body), 500))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&alo(2)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_UNANSWERED
    ) >= 2));
    assert!(evs
        .iter()
        .all(|e| e.kind != EV_UNANSWERED || e.reason == REASON_TRANSPORT));
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 15, |g, e| g.ws_live()
            && count(e, EV_GAP) >= 1),
        "no redial, or no gap reported"
    );
    assert!(eng.cmd.push(&alo(3)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 1));
    let posts = v.posts();
    assert_eq!(
        posts.last().unwrap().conn,
        1,
        "the new post went on the new socket"
    );
    assert!(eng.stats.io()[5] >= 1, "the reconnect is counted");
    assert!(gw.counters().conn_drops >= 1);
}

#[test]
fn with_no_socket_left_a_post_goes_rest_as_the_envelope_alone() {
    let v = Venue::start();
    v.policy(|s| {
        if s.conn == usize::MAX {
            Answer::Action(resting(items_in(&s.body), 55))
        } else {
            Answer::Drop
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    // The socket dies on the first post and no upgrade is taken again.
    v.refuse_ws(true);
    assert!(eng.cmd.push(&alo(1)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |g, e| !g.ws_live()
        && count(e, EV_UNANSWERED) >= 1));
    assert!(eng.cmd.push(&alo(2)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 10, |_, e| count(
        e, EV_ACK
    ) >= 1));
    let ack = evs.iter().find(|e| e.kind == EV_ACK).unwrap();
    assert_eq!((ack.client_oid, ack.oid), (2, 55));
    let posts = v.posts();
    let rest = posts
        .iter()
        .find(|p| p.conn == usize::MAX)
        .expect("a REST post");
    assert!(
        rest.body.starts_with(r#"{"action":{"type":"order""#),
        "{}",
        rest.body
    );
    assert!(
        !rest.body.contains("method"),
        "the REST body is the envelope, not the WS wrapper"
    );
    assert!(eng.stats.io()[4] >= 1, "the fallback is counted");
}

#[test]
fn make_before_break_lands_the_old_sockets_answer_and_moves_new_posts() {
    let v = Venue::start();
    v.policy(|s| {
        if s.conn == 0 {
            Answer::Later(Duration::from_millis(400), resting(items_in(&s.body), 10))
        } else {
            Answer::Action(resting(items_in(&s.body), 20))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, _| v
        .posts()
        .len()
        == 1));
    gw.request_replace();
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 10, |g, _| g.swaps() == 1),
        "no swap"
    );
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 1));
    assert_eq!(
        evs.iter().find(|e| e.kind == EV_ACK).unwrap().oid,
        10,
        "the draining socket's answer landed"
    );
    assert!(eng.cmd.push(&alo(2)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 2));
    assert_eq!(v.posts().last().unwrap().conn, 1);
    assert_eq!(
        count(&evs, EV_UNANSWERED) + count(&evs, EV_GAP),
        0,
        "nothing was stranded or lost by the swap"
    );
}

#[test]
fn a_halted_gateway_refuses_growth_unsent_but_still_cancels() {
    let v = Venue::start();
    v.policy(|_| Answer::Action(CANCEL_OK.into()));
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    eng.stats.halt(HALT_VENUE_BASE);
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 1, ETH)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 1));
    let rej = evs.iter().find(|e| e.kind == EV_REJECTED).unwrap();
    assert_eq!((rej.client_oid, rej.reason), (1, REASON_HALTED));
    let posts = v.posts();
    assert_eq!(posts.len(), 1, "only the cancel left the host");
    assert!(posts[0].body.contains("cancelByCloid"));
}

#[test]
fn unreadable_answers_and_api_errors_are_in_doubt_and_refusals_reject_every_item() {
    let v = Venue::start();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    v.policy(move |_| match n2.fetch_add(1, Ordering::SeqCst) {
        0 => Answer::Action(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[]}}}"#.into(),
        ),
        1 => Answer::Action(resting(2, 1)),
        2 => Answer::Error("Too many cumulative requests sent (1301 > 1200)".into()),
        _ => Answer::Action(
            r#"{"status":"err","response":"Insufficient margin to place order. asset=1"}"#.into(),
        ),
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let mut step = |oids: &[u64], evs: &mut Vec<ExecEvent>| {
        let before = evs.len();
        for o in oids {
            assert!(eng.cmd.push(&alo(*o)));
        }
        assert!(drive(&mut gw, &mut eng, evs, 5, |_, e| e.len()
            >= before + oids.len()));
        evs[before..].to_vec()
    };
    let e = step(&[1], &mut evs);
    assert!(e
        .iter()
        .all(|x| x.kind == EV_UNANSWERED && x.reason == REASON_MALFORMED));
    let e = step(&[2], &mut evs);
    assert!(
        e.iter()
            .all(|x| x.kind == EV_UNANSWERED && x.reason == REASON_MALFORMED),
        "{e:?}"
    );
    let e = step(&[3, 4], &mut evs);
    assert!(
        e.iter()
            .all(|x| x.kind == EV_UNANSWERED && x.reason == REASON_RATE_LIMIT),
        "the API layer's error is in doubt, its reason carried: {e:?}"
    );
    let e = step(&[5, 6], &mut evs);
    assert!(e
        .iter()
        .all(|x| x.kind == EV_REJECTED && x.reason == ORDER_EVENT_REASON_MARGIN));
    assert_eq!(gw.counters().answers_mismatched, 1);
}
