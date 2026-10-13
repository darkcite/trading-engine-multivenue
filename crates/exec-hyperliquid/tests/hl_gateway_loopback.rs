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
//! | control | leverage, scheduleCancel and noop post alone, in the order they came between batches; `{"type":"default"}` is the ACK, `status:err` the refusal |
//! | halted | a halted gateway refuses growth (a place, a leverage change) unsent; a cancel and a scheduleCancel still go |
//! | refusals | a malformed or miscounted answer and the API layer's error are UNANSWERED; a refused action rejects every item |
//! | reads | a read posts alone as `{"type":"info"}`; `frontendOpenOrders` rows (ours and a stranger's) and a `clearinghouseState` position come back as events, then the read's end (rows, account value, the sheet's time); an answer that does not scan is `EV_READ_FAILED` |
//! | read, no socket | a read never goes REST: with no socket it is refused `TRANSPORT`, unsent |
//! | sweep | the cancel-all word: a read, a cancelByCloid of this gateway's slots only (never another slot's, never a stranger's), a settle, a read with none left — `SWEEP_CLEAR`; the sweep's reads emit nothing, its cancels' answers reach the arm |
//! | stranded | a sweep that never reads clear stops after 8 rounds `SWEEP_STRANDED`; a new request starts over |
//! | rates | XH4e: the budget and day reads post as `info` (the day's start in place) and answer one end each; past 80 % of the IP share growth is refused `THROTTLED` unsent while a cancel still goes, and the congestion word says `THROTTLED` |
//! | full page | XH4e: a full day page — 2 000 real-length rows, ≈ 0.8 MB in one frame — fits the receive buffer (`MAX_WS_BUF`) and is summed in place; the connection holds |
//! | ttl | XH4f (D9): an order's TTL ends at the gateway — an untimed `cancelByCloid` at the next beat, its success read `EXPIRED`; a replacement inherits its original's deadline; the arm's own successful cancel leaves nothing to fire |
//! | not found | XH4f (D9): a not-found ends no TTL — the arm's own cancel's leaves the order's TTL to fire; the TTL's own leaves its row, and a late landing fires it again |
//! | ttl races | XH4f (D9): an order seen resting while its TTL cancel is out waits for that cancel's answer (one cancel); a place left in doubt whose TTL cancel found nothing is cancelled when it lands late |
//! | ttl ends | XH4f (D9): an end while the TTL cancel is out — a plain `canceled` reads `EXPIRED`, the dead man's `scheduledCancel` keeps its reason |
//! | outcome sweep | XH4i-1: on an outcome map an order of ours on a `#<enc>` leg is cancelled by cloid with `ASSET_BASE + enc` — another slot's and a stranger's on the same leg never — and the sweep reads clear; on a perps-only map it is unmapped and never cancelled |
//! | outcome streams | XH4i-1: on an outcome map a `#<enc>` fill, `orderUpdates` row and open-order row carry `ASSET_BASE + enc`; on a perps-only map `ASSET_UNKNOWN`, as before |
//! | fin | HK2: an answer that arrives in the same read as the socket's FIN is ACKed, not stranded; the gateway then redials |
//!
//! `Answer::Later` defers a post's answer without pausing the socket: the
//! TTL races above see their stream rows while the cancel is still out.
//!
//! Offline path: this file MAY allocate.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_types::{
    ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED,
    ORDER_EVENT_REASON_MARGIN, ORDER_EVENT_REASON_SCHEDULED_CANCEL,
};
use exec_gateway::stats::{
    CONGESTION_THROTTLED, HALT_VENUE_BASE, SWEEP_CLEAR, SWEEP_STRANDED, SWEEP_WORKING,
};
use exec_gateway::Idle;
use exec_hyperliquid::asset::ASSET_BASE;
use exec_hyperliquid::cloid;
use exec_hyperliquid::cmd::{HlCmd, CMD_CANCEL, CMD_PLACE};
use exec_hyperliquid::event::{
    ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CROSSED, EVF_RESTING, EV_ACK, EV_FILL, EV_FILLED,
    EV_FILL_META, EV_GAP, EV_OPEN_ORDER, EV_POSITION, EV_READ_DONE, EV_READ_FAILED, EV_REJECTED,
    EV_SNAPSHOT, EV_UNANSWERED, READ_DAY, READ_LIMIT, READ_ORDERS, READ_STATE, REASON_HALTED,
    REASON_LOCAL, REASON_MALFORMED, REASON_NOT_FOUND, REASON_NOT_READY, REASON_RATE_LIMIT,
    REASON_THROTTLED, REASON_TRANSPORT, SLOT_FOREIGN,
};
use exec_hyperliquid::gateway::{
    hl_duplex, pw, CoinMap, GatewayCfg, HlEngineEnd, HlGateway, IP_WEIGHT_SHARE, WS_MSG_SHARE,
};
use exec_hyperliquid::testvenue::{items_in, resting, Answer, Venue};
use exec_hyperliquid::{Network, Tif};

const SLOT: u8 = 6;
const ETH: u32 = 1;
/// XH4i-1: an outcome leg's `enc` (outcome 194, side 1).
const ENC: u32 = 1941;
const CANCEL_OK: &str =
    r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
const DEFAULT_OK: &str = r#"{"status":"ok","response":{"type":"default"}}"#;

fn gateway(v: &Venue, post_timeout_ns: u64) -> (HlGateway, HlEngineEnd) {
    gateway_with(v, post_timeout_ns, IP_WEIGHT_SHARE)
}

/// A gateway with an IP-weight share of `ip` a minute (XH4e).
fn gateway_with(v: &Venue, post_timeout_ns: u64, ip: u32) -> (HlGateway, HlEngineEnd) {
    gateway_on(v, post_timeout_ns, ip, CoinMap::new())
}

/// A gateway over `coins`, ETH bound in it (XH4i: a perps-only map, or
/// one that names outcome legs).
fn gateway_on(
    v: &Venue,
    post_timeout_ns: u64,
    ip: u32,
    mut coins: CoinMap,
) -> (HlGateway, HlEngineEnd) {
    let (eng, gw_end) = hl_duplex();
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
        ip_weight_per_min: ip,
        ws_msgs_per_min: WS_MSG_SHARE,
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

/// A `frontendOpenOrders` row on ETH; ours when `cloid` names a slot.
fn open_row(oid: u64, cloid: Option<(u8, u64)>) -> String {
    let c = match cloid {
        Some((slot, cid)) => format!(r#","cloid":"{}""#, cloid_hex(slot, cid)),
        None => String::new(),
    };
    format!(
        r#"{{"coin":"ETH","limitPx":"2500.0","oid":{oid},"origSz":"0.1","side":"B","sz":"0.1","timestamp":1790000000000,"isTrigger":false{c}}}"#
    )
}

/// XH4i-1: a `frontendOpenOrders` row on the outcome leg `#<ENC>` — the
/// venue's spelling there, measured on testnet (2026-09-19) — ours when
/// `cloid` names a slot.
fn outcome_row(oid: u64, cloid: Option<(u8, u64)>) -> String {
    let c = match cloid {
        Some((slot, cid)) => format!(r#","cloid":"{}""#, cloid_hex(slot, cid)),
        None => String::new(),
    };
    format!(
        r##"{{"coin":"#{ENC}","limitPx":"0.41","oid":{oid},"origSz":"25.0","side":"B","sz":"25.0","timestamp":1790000000000,"isTrigger":false{c}}}"##
    )
}

/// A read's answer: `frontendOpenOrders` with `rows`.
fn open_orders(rows: &[String]) -> String {
    format!(
        r#"{{"type":"frontendOpenOrders","data":[{}]}}"#,
        rows.join(",")
    )
}

/// A read's answer: `clearinghouseState` — 1000.5 USD, 0.1 ETH worth 250.
const SHEET: &str = r#"{"type":"clearinghouseState","data":{"marginSummary":{"accountValue":"1000.5","totalNtlPos":"250.0","totalRawUsd":"750.5","totalMarginUsed":"25.0"},"crossMarginSummary":{"accountValue":"1000.5","totalNtlPos":"250.0","totalRawUsd":"750.5","totalMarginUsed":"25.0"},"crossMaintenanceMarginUsed":"5.0","withdrawable":"975.5","assetPositions":[{"type":"oneWay","position":{"coin":"ETH","entryPx":"2500.0","szi":"0.1","positionValue":"250.0","unrealizedPnl":"0.0"}}],"time":1790000000000}}"#;

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
    // XH4f: the answered posts' worst round trip went out with the
    // heartbeat — measured, and inside the post timeout.
    let w = eng.stats.rtt_worst_ns();
    assert!(w > 0 && w < 5_000_000_000, "{w}");
    // XH4f (D11): and its own counters, for the arm's report — as of that
    // heartbeat.
    let dials = eng.stats.published(pw::DIALS);
    let fills = eng.stats.published(pw::FILLS);
    assert!(
        dials >= 1 && fills <= gw.counters().fills,
        "{dials} {fills}"
    );
}

/// XH4f (D9): an order's TTL ends at the gateway — an untimed
/// `cancelByCloid` goes on its own at the next beat and its success reads
/// `EXPIRED`, charged with the sweeps'; a replacement inherits its
/// original's deadline (the original's TTL ends as the modify lands); the
/// arm's own cancel before the TTL leaves nothing to fire; an end row
/// after the TTL cancel's answer reads as the venue said.
#[test]
fn a_ttl_ends_at_the_gateway_and_its_cancel_reads_expired() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            // One success per cancel: two TTLs may end in one beat.
            let n = s.body.matches(r#""cloid":"#).count();
            let st = vec![r#""success""#; n].join(",");
            Answer::Action(format!(
                r#"{{"status":"ok","response":{{"type":"cancel","data":{{"statuses":[{st}]}}}}}}"#
            ))
        } else {
            Answer::Action(resting(items_in(&s.body), 300))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let ttl = |mut c: HlCmd, ms: u64| {
        c.time_ms = ms;
        c
    };
    let acks = |e: &[ExecEvent]| count(e, EV_ACK);
    // 1: its TTL fires. 2: the arm cancels it first. 3 → 4: a modify's
    // replacement fires at 3's deadline.
    for (i, cmd) in [
        ttl(alo(1), 300),
        ttl(alo(2), 300),
        ttl(alo(3), 300),
        alo(4).replacing(3),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(eng.cmd.push(&cmd));
        assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| acks(e) > i));
    }
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 2, ETH)));
    let expired = |e: &ExecEvent| {
        e.kind == EV_ACK && e.cmd_kind() == CMD_CANCEL && e.reason == ORDER_EVENT_REASON_EXPIRED
    };
    let fired_both = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| {
        e.iter().filter(|x| expired(x)).count() >= 2
    });
    let seen: Vec<(u8, u8, u8, u64)> = evs
        .iter()
        .map(|x| (x.kind, x.cmd_kind(), x.reason, x.client_oid))
        .collect();
    assert!(fired_both, "{:?} {seen:?}", gw.counters());
    let _ = drive(&mut gw, &mut eng, &mut evs, 1, |_, _| false);
    let mut fired: Vec<u64> = evs
        .iter()
        .filter(|x| expired(x))
        .map(|x| x.client_oid)
        .collect();
    fired.sort_unstable();
    assert_eq!(
        fired,
        [1, 4],
        "2 was cancelled by its arm first, 3 replaced"
    );
    let posts = v.posts();
    let ttl_posts: Vec<&String> = posts
        .iter()
        .map(|p| &p.body)
        .filter(|b| b.contains("cancelByCloid") && !b.contains(&cloid_hex(SLOT, 2)))
        .collect();
    assert!(!ttl_posts.is_empty());
    assert!(
        ttl_posts.iter().all(|b| !b.contains("expiresAfter")),
        "a TTL cancel is untimed (XG-3)"
    );
    assert_eq!(gw.counters().ttl_cancels, 2);
    assert_eq!(eng.stats.sweep_cancels(), 2, "charged with the sweeps'");
    // The end rows: the TTL's read EXPIRED; the arm's own, as the venue said.
    let upd = |cid: u64| {
        format!(
            r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"ETH","side":"B","limitPx":"2500.0","sz":"0.1","oid":{},"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"canceled","statusTimestamp":1700000000002}}]}}"#,
            300 + cid,
            cloid_hex(SLOT, cid)
        )
    };
    v.push_all(&upd(2));
    v.push_all(&upd(1));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| e
        .iter()
        .filter(|x| x.kind == exec_hyperliquid::event::EV_CANCELED)
        .count()
        >= 2));
    let end = |cid: u64| {
        evs.iter()
            .find(|x| x.kind == exec_hyperliquid::event::EV_CANCELED && x.client_oid == cid)
            .map(|x| x.reason)
    };
    assert_eq!(end(2), Some(ORDER_EVENT_REASON_CANCEL_REQUESTED));
    assert_eq!(
        end(1),
        Some(ORDER_EVENT_REASON_CANCEL_REQUESTED),
        "its TTL row went with the ACK"
    );
}

/// XH4f (D9, risk r2): a not-found ends no TTL. The arm's own cancel
/// answered not-found (its order still in flight on another socket, say)
/// leaves the order's TTL to fire; the TTL's own cancel answered not-found
/// leaves its row, and the order seen resting after it — a late landing —
/// has the cancel go again at the next beat.
#[test]
fn a_not_found_ends_no_ttl_and_a_late_landing_fires_it_again() {
    const NOT_FOUND: &str = r#"{"error":"Order was never placed, already canceled, or filled."}"#;
    let v = Venue::start();
    let (c1, c2) = (cloid_hex(SLOT, 1), cloid_hex(SLOT, 2));
    let (first1, first2) = (AtomicBool::new(true), AtomicBool::new(true));
    v.policy(move |s| {
        if s.body.contains("cancelByCloid") {
            // Each order's first cancel finds nothing — 1's is its TTL's
            // (not landed yet), 2's the arm's own; the rest succeed.
            let st: Vec<&str> = s
                .body
                .split(r#""cloid":""#)
                .skip(1)
                .map(|item| {
                    let gone = if item.starts_with(&c1) {
                        first1.swap(false, Ordering::SeqCst)
                    } else {
                        item.starts_with(&c2) && first2.swap(false, Ordering::SeqCst)
                    };
                    if gone {
                        NOT_FOUND
                    } else {
                        r#""success""#
                    }
                })
                .collect();
            Answer::Action(format!(
                r#"{{"status":"ok","response":{{"type":"cancel","data":{{"statuses":[{}]}}}}}}"#,
                st.join(",")
            ))
        } else {
            Answer::Action(resting(items_in(&s.body), 300))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let ttl = |mut c: HlCmd, ms: u64| {
        c.time_ms = ms;
        c
    };
    let acks = |e: &[ExecEvent]| count(e, EV_ACK);
    // 1's TTL ends at 300 ms, 2's at 900 ms: their cancels go apart.
    for (i, cmd) in [ttl(alo(1), 300), ttl(alo(2), 900)].into_iter().enumerate() {
        assert!(eng.cmd.push(&cmd));
        assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| acks(e) > i));
    }
    let not_found = |e: &[ExecEvent], cid: u64| {
        e.iter()
            .any(|x| x.kind == EV_REJECTED && x.reason == REASON_NOT_FOUND && x.client_oid == cid)
    };
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 2, ETH)));
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 5, |_, e| not_found(e, 2)),
        "the arm's own cancel of 2: not found"
    );
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 5, |_, e| not_found(e, 1)),
        "1's TTL cancel: not found"
    );
    // 1 lands late: the venue's open row for it.
    v.push_all(&format!(
        r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"ETH","side":"B","limitPx":"2500.0","sz":"0.1","oid":301,"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"open","statusTimestamp":1700000000001}}]}}"#,
        cloid_hex(SLOT, 1)
    ));
    let expired = |x: &ExecEvent| {
        x.kind == EV_ACK && x.cmd_kind() == CMD_CANCEL && x.reason == ORDER_EVENT_REASON_EXPIRED
    };
    let both = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| {
        e.iter().filter(|x| expired(x)).count() >= 2
    });
    assert!(both, "{:?}", gw.counters());
    let mut fired: Vec<u64> = evs
        .iter()
        .filter(|x| expired(x))
        .map(|x| x.client_oid)
        .collect();
    fired.sort_unstable();
    assert_eq!(
        fired,
        [1, 2],
        "2's TTL outlived its arm's not-found; 1's went again on its landing"
    );
    assert_eq!(gw.counters().ttl_cancels, 3, "1's twice, 2's once");
}

/// XH4f (D9, risk r3): the TTL cancel's two races. An order seen resting
/// while its TTL cancel is out waits for that cancel's answer — a success
/// ends it with ONE cancel, none at the next beat meanwhile. A place left in
/// doubt (never answered) whose TTL cancel found nothing is cancelled again
/// when it lands late, its end read `EXPIRED`.
#[test]
fn a_ttl_cancel_waits_out_its_answer_and_a_doubtful_place_landing_late_is_cancelled() {
    const NOT_FOUND: &str = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled."}]}}}"#;
    let v = Venue::start();
    let (c3, c4) = (cloid_hex(SLOT, 3), cloid_hex(SLOT, 4));
    let first3 = AtomicBool::new(true);
    v.policy(move |s| {
        if s.body.contains("cancelByCloid") {
            if s.body.contains(&c4) {
                // 4's TTL cancel: a success, answered late.
                Answer::Later(Duration::from_millis(400), CANCEL_OK.into())
            } else if s.body.contains(&c3) && first3.swap(false, Ordering::SeqCst) {
                // 3's first TTL cancel finds nothing: its place is in doubt.
                Answer::Action(NOT_FOUND.into())
            } else {
                Answer::Action(CANCEL_OK.into())
            }
        } else if s.body.contains(&c3) {
            // 3's place: never answered — it lands later.
            Answer::Silent
        } else {
            Answer::Action(resting(items_in(&s.body), 300))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 1_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let ttl = |mut c: HlCmd, ms: u64| {
        c.time_ms = ms;
        c
    };
    let open = |cid: u64| {
        format!(
            r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"ETH","side":"B","limitPx":"2500.0","sz":"0.1","oid":{},"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"open","statusTimestamp":{}}}]}}"#,
            300 + cid,
            cloid_hex(SLOT, cid),
            1_700_000_000_000 + cid
        )
    };
    let expired = |e: &[ExecEvent], cid: u64| {
        e.iter().any(|x| {
            x.kind == EV_ACK
                && x.cmd_kind() == CMD_CANCEL
                && x.reason == ORDER_EVENT_REASON_EXPIRED
                && x.client_oid == cid
        })
    };
    let cancels_of = |c: &str| {
        v.posts()
            .iter()
            .filter(|p| p.body.contains("cancelByCloid") && p.body.contains(c))
            .count()
    };

    // 4 rests; its TTL cancel goes at 300 ms, answered 400 ms later; its
    // open row arrives while the cancel is out.
    let c4 = cloid_hex(SLOT, 4);
    let acked = |e: &[ExecEvent]| count(e, EV_ACK) >= 1;
    let sent4 = || cancels_of(&c4) >= 1;
    assert!(eng.cmd.push(&ttl(alo(4), 300)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| acked(e)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, _| sent4()));
    v.push_all(&open(4));
    let done4 = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| expired(e, 4));
    assert!(done4, "4: its TTL cancel's success {:?}", gw.counters());
    let _ = drive(&mut gw, &mut eng, &mut evs, 1, |_, _| false);
    assert_eq!(cancels_of(&c4), 1, "the open row waited for the answer");

    // 3's place is never answered (in doubt); its TTL cancel at 1.5 s finds
    // nothing; then it lands — the cancel goes again.
    assert!(eng.cmd.push(&ttl(alo(3), 1_500)));
    let doubt = |e: &[ExecEvent]| {
        e.iter()
            .any(|x| x.kind == EV_UNANSWERED && x.client_oid == 3)
    };
    let in_doubt = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| doubt(e));
    assert!(in_doubt, "3: in doubt");
    let missed = |e: &[ExecEvent]| {
        e.iter()
            .any(|x| x.kind == EV_REJECTED && x.reason == REASON_NOT_FOUND && x.client_oid == 3)
    };
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 5, |_, e| missed(e)),
        "3's TTL cancel: not found"
    );
    v.push_all(&open(3));
    let done3 = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| expired(e, 3));
    assert!(done3, "3 landed late: cancelled again {:?}", gw.counters());
    assert_eq!(gw.counters().ttl_cancels, 3, "4's once, 3's twice");
}

/// XH4f (D9, risk r5): an end row while the order's TTL cancel is out —
/// a plain `canceled` reads `EXPIRED` (the TTL's), a venue-specific one
/// (here the dead man's `scheduledCancel`) keeps its reason.
#[test]
fn an_end_while_the_ttl_cancel_is_out_is_the_ttl_s_only_when_plain() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            // Both TTL cancels, answered late: the end rows come first.
            let n = s.body.matches(r#""cloid":"#).count();
            let st = vec![r#""success""#; n].join(",");
            Answer::Later(
                Duration::from_millis(600),
                format!(
                    r#"{{"status":"ok","response":{{"type":"cancel","data":{{"statuses":[{st}]}}}}}}"#
                ),
            )
        } else {
            Answer::Action(resting(items_in(&s.body), 300))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let ttl = |mut c: HlCmd| {
        c.time_ms = 300;
        c
    };
    assert!(eng.cmd.push(&ttl(alo(5))));
    assert!(eng.cmd.push(&ttl(alo(6))));
    let acked = |e: &[ExecEvent]| count(e, EV_ACK) >= 2;
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| acked(e)));
    let out = || v.posts().iter().any(|p| p.body.contains("cancelByCloid"));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, _| out()));
    let row = |cid: u64, status: &str| {
        format!(
            r#"{{"order":{{"coin":"ETH","side":"B","limitPx":"2500.0","sz":"0.1","oid":{},"timestamp":1,"origSz":"0.1","cloid":"{}"}},"status":"{status}","statusTimestamp":{}}}"#,
            300 + cid,
            cloid_hex(SLOT, cid),
            1_700_000_000_000 + cid
        )
    };
    v.push_all(&format!(
        r#"{{"channel":"orderUpdates","data":[{},{}]}}"#,
        row(5, "canceled"),
        row(6, "scheduledCancel")
    ));
    let ended = |e: &[ExecEvent]| count(e, exec_hyperliquid::event::EV_CANCELED) >= 2;
    let both = drive(&mut gw, &mut eng, &mut evs, 5, |_, e| ended(e));
    assert!(both, "{:?}", gw.counters());
    let end = |cid: u64| {
        evs.iter()
            .find(|x| x.kind == exec_hyperliquid::event::EV_CANCELED && x.client_oid == cid)
            .map(|x| x.reason)
    };
    assert_eq!(
        end(5),
        Some(ORDER_EVENT_REASON_EXPIRED),
        "a plain end: the TTL's"
    );
    assert_eq!(
        end(6),
        Some(ORDER_EVENT_REASON_SCHEDULED_CANCEL),
        "the dead man's end keeps its reason"
    );
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

/// XMM XH5d: an action carries one grouping — ALO places bidding the same
/// priority are one post, `"grouping":{"p":N}`; a place bidding another
/// (or none) posts the open batch first.
#[test]
fn alo_places_batch_by_the_priority_they_bid() {
    let v = Venue::start();
    v.policy(|s| Answer::Action(resting(items_in(&s.body), 1)));
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1).with_priority(10_000)));
    assert!(eng.cmd.push(&alo(2).with_priority(10_000)));
    assert!(eng.cmd.push(&alo(3)));
    assert!(eng.cmd.push(&alo(4).with_priority(20_000)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 4));
    let posts = v.posts();
    assert_eq!(posts.len(), 3, "p 10 000 twice, none, p 20 000");
    assert_eq!(items_in(&posts[0].body), 2);
    assert!(
        posts[0].body.contains(r#""grouping":{"p":10000}"#),
        "{}",
        posts[0].body
    );
    assert_eq!(items_in(&posts[1].body), 1);
    assert!(
        posts[1].body.contains(r#""grouping":"na""#),
        "{}",
        posts[1].body
    );
    assert!(
        posts[2].body.contains(r#""grouping":{"p":20000}"#),
        "{}",
        posts[2].body
    );
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

/// HK2: an answer that arrives in the same read as the socket's FIN is the
/// stream's too: the post is ACKed, not stranded as unanswered, and the
/// gateway then redials. Before, the EOF closed the connection before the
/// frames in rx were parsed.
#[test]
fn an_answer_that_arrives_with_the_fin_is_delivered_before_the_close() {
    let v = Venue::start();
    let first = Arc::new(AtomicBool::new(true));
    let f2 = Arc::clone(&first);
    v.policy(move |s| {
        if f2.swap(false, Ordering::SeqCst) {
            Answer::ActionThenDrop(resting(items_in(&s.body), 500))
        } else {
            Answer::Action(resting(items_in(&s.body), 600))
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, _| !v.posts().is_empty()));
    // The answer and the FIN both land before the gateway reads again.
    std::thread::sleep(Duration::from_millis(300));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(e, EV_ACK)
        + count(e, EV_UNANSWERED)
        >= 1));
    assert_eq!(count(&evs, EV_ACK), 1, "the answer behind the FIN is delivered");
    assert_eq!(count(&evs, EV_UNANSWERED), 0);
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 15, |g, e| g.ws_live()
            && count(e, EV_GAP) >= 1),
        "no redial, or no gap reported"
    );
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
    // XMM XH5c-3: the REST round trip is its own series, and with no
    // socket Ready the congestion word holds the member (with the
    // heartbeat, ≤ 100 ms).
    assert!(eng.stats.rest_rtt.count() >= 1, "its own series");
    let t = std::time::Instant::now();
    while eng.stats.congestion() != CONGESTION_THROTTLED
        && t.elapsed() < std::time::Duration::from_secs(2)
    {
        gw.turn();
    }
    assert_eq!(eng.stats.congestion(), CONGESTION_THROTTLED);
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
fn control_actions_post_alone_in_the_order_they_came() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains(r#""type":"order""#) {
            Answer::Action(resting(items_in(&s.body), 50))
        } else if s.body.contains(r#""type":"scheduleCancel""#) {
            Answer::Action(
                r#"{"status":"err","response":"Cannot set scheduled cancel time until enough volume traded. Required: $1000000. Traded: $12.5."}"#
                    .into(),
            )
        } else {
            Answer::Action(DEFAULT_OK.into())
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&HlCmd::leverage(SLOT, 2, ETH, true, 3)));
    assert!(eng.cmd.push(&alo(3)));
    assert!(eng
        .cmd
        .push(&HlCmd::schedule_cancel(SLOT, 4, 1_789_000_005_000).expiring_after(5_000)));
    assert!(eng.cmd.push(&HlCmd::noop(SLOT, 5)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) + count(
        e,
        EV_REJECTED
    ) >= 5));
    let posts = v.posts();
    assert_eq!(
        posts.len(),
        5,
        "[ALO 1] [leverage] [ALO 3] [scheduleCancel] [noop]"
    );
    assert!(posts[0].body.contains(&cloid_hex(SLOT, 1)) && items_in(&posts[0].body) == 1);
    assert!(posts[1]
        .body
        .contains(r#""action":{"type":"updateLeverage","asset":1,"isCross":true,"leverage":3}"#));
    assert!(posts[2].body.contains(&cloid_hex(SLOT, 3)) && items_in(&posts[2].body) == 1);
    assert!(posts[3]
        .body
        .contains(r#""action":{"type":"scheduleCancel","time":1789000005000}"#));
    assert!(
        posts[3].body.contains(r#""expiresAfter":"#) && !posts[1].body.contains("expiresAfter"),
        "a control post carries its command's own expiresAfter"
    );
    assert!(posts[4].body.contains(r#""action":{"type":"noop"}"#));
    let of = |oid: u64| *evs.iter().rfind(|e| e.client_oid == oid).unwrap();
    let lev = of(2);
    assert_eq!(
        (lev.kind, lev.cmd_kind(), lev.slot, lev.asset),
        (EV_ACK, 0, SLOT, ETH)
    );
    assert_eq!(
        of(4).kind,
        EV_REJECTED,
        "the venue's refusal of a first send is final"
    );
    assert_eq!(of(5).kind, EV_ACK);
    assert_eq!(
        of(3).oid,
        50,
        "the place after a control post answered on its own"
    );
    assert_eq!(count(&evs, EV_UNANSWERED), 0);
}

#[test]
fn a_halted_gateway_refuses_growth_unsent_but_still_cancels() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("cancelByCloid") {
            Answer::Action(CANCEL_OK.into())
        } else {
            Answer::Action(DEFAULT_OK.into())
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    eng.stats.halt(HALT_VENUE_BASE);
    assert!(eng.cmd.push(&alo(1)));
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 1, ETH)));
    assert!(eng.cmd.push(&HlCmd::leverage(SLOT, 2, ETH, false, 5)));
    assert!(eng.cmd.push(&HlCmd::schedule_cancel(SLOT, 3, 0)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e, EV_ACK
    ) >= 2));
    let refused: Vec<(u64, u8)> = evs
        .iter()
        .filter(|e| e.kind == EV_REJECTED)
        .map(|e| (e.client_oid, e.reason))
        .collect();
    assert_eq!(
        refused,
        [(1, REASON_HALTED), (2, REASON_HALTED)],
        "a place and a leverage change grow"
    );
    let posts = v.posts();
    assert_eq!(
        posts.len(),
        2,
        "only the cancel and the dead-man clear left the host"
    );
    assert!(posts[0].body.contains("cancelByCloid"));
    assert!(posts[1]
        .body
        .contains(r#""action":{"type":"scheduleCancel"}"#));
}

#[test]
fn reads_answer_their_rows_then_their_end_and_an_unreadable_one_fails() {
    let v = Venue::start();
    let mut orders = 0usize;
    v.policy(move |s| {
        if s.body.contains("frontendOpenOrders") {
            orders += 1;
            if orders == 1 {
                Answer::Info(open_orders(&[
                    open_row(101, Some((SLOT, 7))),
                    open_row(102, None),
                ]))
            } else {
                Answer::Info(r#"{"type":"frontendOpenOrders","data":{"not":"a list"}}"#.into())
            }
        } else if s.body.contains("clearinghouseState") {
            Answer::Info(SHEET.into())
        } else {
            Answer::Silent
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&HlCmd::read_orders(SLOT, 1)));
    assert!(eng.cmd.push(&HlCmd::read_state(SLOT, 2)));
    assert!(eng.cmd.push(&HlCmd::read_orders(SLOT, 3)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_READ_DONE
    ) + count(
        e,
        EV_READ_FAILED
    ) >= 3));
    let posts = v.posts();
    assert_eq!(posts.len(), 3, "a read posts alone");
    assert!(
        posts[0].body.contains(r#""type":"info""#) && posts[0].body.contains("0xa5a5a5a5"),
        "{}",
        posts[0].body
    );
    let rows: Vec<&ExecEvent> = evs.iter().filter(|e| e.kind == EV_OPEN_ORDER).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (
            rows[0].slot,
            rows[0].client_oid,
            rows[0].oid,
            rows[0].aux,
            rows[0].asset
        ),
        (SLOT, 7, 101, 1, ETH)
    );
    assert_eq!((rows[1].slot, rows[1].oid), (SLOT_FOREIGN, 102));
    let end = |cid: u64| {
        *evs.iter()
            .find(|e| e.client_oid == cid && (e.kind == EV_READ_DONE || e.kind == EV_READ_FAILED))
            .unwrap()
    };
    let o = end(1);
    assert_eq!(
        (o.kind, o.reason, o.aux, o.slot),
        (EV_READ_DONE, READ_ORDERS, 2, SLOT)
    );
    let pos = evs
        .iter()
        .find(|e| e.kind == EV_POSITION)
        .expect("the position");
    assert_eq!(
        (pos.client_oid, pos.slot, pos.asset, pos.sz_1e8, pos.aux),
        (2, SLOT, ETH, 10_000_000, 25_000_000_000)
    );
    let st = end(2);
    assert_eq!(
        (st.kind, st.reason, st.px_1e8, st.venue_ms),
        (EV_READ_DONE, READ_STATE, 100_050_000_000, 1_790_000_000_000)
    );
    assert_eq!(end(3).kind, EV_READ_FAILED, "an answer that does not scan");
    assert_eq!(
        (count(&evs, EV_UNANSWERED), count(&evs, EV_REJECTED)),
        (0, 0)
    );
    assert_eq!((gw.counters().reads, gw.counters().reads_failed), (2, 1));
}

/// XH4e: the budget and day reads post as `info` — the day's start
/// rendered in place — and answer one end each; two reads (weight 40) put
/// the gateway past 80 % of an IP share of 25, so growth is refused
/// `THROTTLED` unsent while a cancel still goes, and the congestion word
/// says so.
#[test]
fn over_its_rate_share_growth_is_throttled_and_the_new_reads_answer() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("userRateLimit") {
            Answer::Info(
                r#"{"type":"userRateLimit","data":{"cumVlm":"1500.5","nRequestsUsed":2890,"nRequestsCap":11500,"nRequestsSurplus":7}}"#
                    .into(),
            )
        } else if s.body.contains("userFillsByTime") {
            Answer::Info(format!(
                r#"{{"type":"userFillsByTime","data":[{},{}]}}"#,
                fill_row(1, "0.01", false),
                fill_row(2, "0.01", true)
            ))
        } else if s.body.contains("cancelByCloid") {
            Answer::Action(CANCEL_OK.into())
        } else {
            Answer::Silent
        }
    });
    let (mut gw, mut eng) = gateway_with(&v, 5_000_000_000, 25);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&HlCmd::read_limit(SLOT, 1)));
    assert!(eng.cmd.push(&HlCmd::read_day(
        SLOT,
        2,
        1_790_000_000_000,
        1_790_000_600_000
    )));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_READ_DONE
    ) >= 2));
    let posts = v.posts();
    assert!(
        posts[1]
            .body
            .contains(r#""startTime":1790000000000,"endTime":1790000600000}"#),
        "{}",
        posts[1].body
    );
    let end = |cid: u64| {
        *evs.iter()
            .find(|e| e.client_oid == cid && e.kind == EV_READ_DONE)
            .unwrap()
    };
    let l = end(1);
    assert_eq!(
        (l.reason, l.aux, l.px_1e8, l.sz_1e8),
        (READ_LIMIT, 2_890, 150_050_000_000, 7)
    );
    let d = end(2);
    assert_eq!(
        (d.reason, d.aux, d.px_1e8, d.venue_ms),
        (READ_DAY, 2, 50_010_000_000, 1_700_000_000_000),
        "two rows of 0.1 at 2 500.5"
    );
    assert!(eng.cmd.push(&alo(3)));
    assert!(eng.cmd.push(&HlCmd::cancel(SLOT, 4, ETH)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| e
        .iter()
        .any(|x| x.client_oid == 4 && x.kind == EV_ACK)));
    let refused: Vec<(u64, u8)> = evs
        .iter()
        .filter(|e| e.kind == EV_REJECTED)
        .map(|e| (e.client_oid, e.reason))
        .collect();
    assert_eq!(refused, [(3, REASON_THROTTLED)]);
    assert_eq!(gw.counters().throttled, 1);
    // The word goes with the heartbeat (≤ 100 ms).
    let t = std::time::Instant::now();
    while eng.stats.congestion() != CONGESTION_THROTTLED
        && t.elapsed() < std::time::Duration::from_secs(2)
    {
        gw.turn();
    }
    assert_eq!(eng.stats.congestion(), CONGESTION_THROTTLED);
    assert_eq!(v.posts().len(), 3, "the place never left the host");
}

/// XH4e: a full page of the day's fills — the venue's 2 000 rows, each as
/// long as a real one with a cloid, a hash and a builder fee (≈ 0.4 KiB:
/// ≈ 0.8 MB in one frame) — fits the post connection's receive buffer and
/// is summed in place: one end, 2 000 rows, the latest time, the
/// connection kept. XH4g measures the venue's real page against the same
/// bound.
#[test]
fn a_full_day_page_is_one_frame_summed_in_place() {
    let hash = format!("0x{}", "ab".repeat(32));
    let ours = cloid_hex(SLOT, 7);
    let mut rows = String::with_capacity(2_000 * 440);
    let mut k = 0u64;
    while k < 2_000 {
        if k > 0 {
            rows.push(',');
        }
        rows.push_str(&format!(
            r#"{{"coin":"ETH","px":"109663.25","sz":"0.00013","side":"B","time":{},"startPosition":"-0.00261","dir":"Close Short","closedPnl":"0.004836","hash":"{hash}","oid":{},"crossed":false,"fee":"0.006357","tid":{},"cloid":"{ours}","builderFee":"0.001234","feeToken":"USDC","twapId":null}}"#,
            1_790_000_000_000 + k,
            110_939_869_658 + k,
            1_063_627_418_003_425 + k
        ));
        k += 1;
    }
    let page = format!(r#"{{"type":"userFillsByTime","data":[{rows}]}}"#);
    assert!(
        page.len() > 780_000 && page.len() < exec_hyperliquid::MAX_WS_BUF,
        "a full page of real-length rows: {} B",
        page.len()
    );
    let v = Venue::start();
    v.policy(move |s| {
        if s.body.contains("userFillsByTime") {
            Answer::Info(page.clone())
        } else {
            Answer::Silent
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    assert!(eng.cmd.push(&HlCmd::read_day(
        SLOT,
        1,
        1_790_000_000_000,
        1_790_000_600_000
    )));
    assert!(drive(&mut gw, &mut eng, &mut evs, 10, |_, e| e.iter().any(
        |x| x.client_oid == 1 && (x.kind == EV_READ_DONE || x.kind == EV_READ_FAILED)
    )));
    let d = *evs.iter().find(|e| e.client_oid == 1).unwrap();
    assert_eq!(
        (d.kind, d.reason, d.aux, d.sz_1e8, d.venue_ms),
        (EV_READ_DONE, READ_DAY, 2_000, 0, 1_790_000_001_999),
        "one end: every row, in time order, the latest time"
    );
    assert!(d.px_1e8 > 0, "our rows summed");
    assert!(gw.ws_live() && gw.swaps() == 0, "the connection held");
    assert_eq!(count(&evs, EV_GAP), 0);
}

#[test]
fn a_read_with_no_socket_is_refused_never_sent_by_rest() {
    let v = Venue::start();
    v.policy(|_| Answer::Drop);
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    // The socket dies on the first post and no upgrade is taken again.
    v.refuse_ws(true);
    assert!(eng.cmd.push(&HlCmd::read_orders(SLOT, 1)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |g, e| !g.ws_live()
        && count(e, EV_UNANSWERED) >= 1));
    assert!(eng.cmd.push(&HlCmd::read_orders(SLOT, 2)));
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(
        e,
        EV_REJECTED
    ) >= 1));
    let r = evs.iter().find(|e| e.kind == EV_REJECTED).unwrap();
    assert_eq!(
        (r.client_oid, r.reason, r.cmd_kind()),
        (2, REASON_TRANSPORT, 0)
    );
    assert!(
        v.posts().iter().all(|p| p.conn != usize::MAX),
        "a read never goes REST"
    );
}

#[test]
fn the_sweep_cancels_this_gateway_s_slots_only_and_reads_until_none_rest() {
    let v = Venue::start();
    let mut reads = 0usize;
    v.policy(move |s| {
        if s.body.contains("frontendOpenOrders") {
            reads += 1;
            // Another slot's order and a stranger's never leave.
            let mut rows = vec![open_row(300, Some((3, 9))), open_row(301, None)];
            if reads == 1 {
                rows.push(open_row(101, Some((SLOT, 7))));
            }
            Answer::Info(open_orders(&rows))
        } else {
            Answer::Action(CANCEL_OK.into())
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let stats = Arc::clone(&eng.stats);
    let asked = stats.request_sweep();
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 10, |_, _| stats.sweep()
            == (asked, SWEEP_CLEAR)),
        "the sweep never read clear"
    );
    let posts = v.posts();
    assert_eq!(posts.len(), 3, "[read] [cancel ours] [read]");
    assert!(posts[0].body.contains("frontendOpenOrders"));
    assert!(
        posts[1].body.contains(&cloid_hex(SLOT, 7)) && !posts[1].body.contains(&cloid_hex(3, 9)),
        "{}",
        posts[1].body
    );
    assert!(posts[2].body.contains("frontendOpenOrders"));
    let ack = evs
        .iter()
        .find(|e| e.kind == EV_ACK)
        .expect("the cancel's answer reaches the arm");
    assert_eq!(
        (ack.client_oid, ack.slot, ack.cmd_kind()),
        (7, SLOT, CMD_CANCEL)
    );
    assert_eq!(
        count(&evs, EV_OPEN_ORDER) + count(&evs, EV_READ_DONE),
        0,
        "the sweep's reads are its own"
    );
    assert_eq!(
        (gw.counters().sweep_rounds, gw.counters().sweep_cancels),
        (2, 1)
    );
}

#[test]
fn a_sweep_that_never_reads_clear_is_stranded_and_a_new_request_starts_over() {
    let v = Venue::start();
    v.policy(|s| {
        if s.body.contains("frontendOpenOrders") {
            Answer::Info(open_orders(&[open_row(101, Some((SLOT, 7)))]))
        } else {
            Answer::Action(CANCEL_OK.into())
        }
    });
    let (mut gw, mut eng) = gateway(&v, 5_000_000_000);
    let mut evs = Vec::new();
    live(&mut gw, &mut eng, &mut evs);
    let stats = Arc::clone(&eng.stats);
    let asked = stats.request_sweep();
    assert!(
        drive(&mut gw, &mut eng, &mut evs, 20, |_, _| stats.sweep()
            == (asked, SWEEP_STRANDED)),
        "the sweep never gave up"
    );
    assert_eq!(gw.counters().sweep_rounds, 8);
    let again = stats.request_sweep();
    assert!(drive(&mut gw, &mut eng, &mut evs, 5, |_, _| stats.sweep()
        == (again, SWEEP_WORKING)));
}

/// XH4i-1: an order of ours on an outcome leg is swept, not stranded —
/// on a map that names outcome legs its cancel names `ASSET_BASE + enc`,
/// read off the venue's own `#<enc>`, and the sweep reads clear; another
/// slot's order and a stranger's on the same leg are never cancelled. On
/// a perps-only map (slot 6's) ours is unmapped and never cancelled, as
/// before. The venue keeps an order until a cancel names it.
#[test]
fn an_outcome_order_of_ours_is_swept_when_the_map_names_outcome_legs() {
    for outcomes in [true, false] {
        let v = Venue::start();
        let mut cancelled = false;
        v.policy(move |s| {
            if s.body.contains("frontendOpenOrders") {
                // Another slot's order and a stranger's never leave.
                let mut rows = vec![outcome_row(300, Some((3, 9))), outcome_row(301, None)];
                if !cancelled {
                    rows.push(outcome_row(201, Some((SLOT, 8))));
                }
                Answer::Info(open_orders(&rows))
            } else {
                cancelled |= s.body.contains(&cloid_hex(SLOT, 8));
                Answer::Action(CANCEL_OK.into())
            }
        });
        let coins = if outcomes {
            CoinMap::with_outcomes()
        } else {
            CoinMap::new()
        };
        let (mut gw, mut eng) = gateway_on(&v, 5_000_000_000, IP_WEIGHT_SHARE, coins);
        let mut evs = Vec::new();
        live(&mut gw, &mut eng, &mut evs);
        let stats = Arc::clone(&eng.stats);
        let asked = stats.request_sweep();
        if outcomes {
            assert!(
                drive(&mut gw, &mut eng, &mut evs, 10, |_, _| stats.sweep()
                    == (asked, SWEEP_CLEAR)),
                "the outcome order was never swept: {:?} {:?}",
                stats.sweep(),
                gw.counters()
            );
            let posts = v.posts();
            assert_eq!(posts.len(), 3, "[read] [cancel ours] [read]");
            let want = format!(r#"{{"asset":{},"cloid":"#, ASSET_BASE + ENC);
            assert!(
                posts[1].body.contains(r#""type":"cancelByCloid""#)
                    && posts[1].body.contains(&want)
                    && posts[1].body.contains(&cloid_hex(SLOT, 8))
                    && !posts[1].body.contains(&cloid_hex(3, 9)),
                "{}",
                posts[1].body
            );
            assert_eq!(
                posts[1].body.matches(r#"{"asset":"#).count(),
                1,
                "ours alone: {}",
                posts[1].body
            );
            assert_eq!(
                (gw.counters().sweep_cancels, gw.counters().sweep_unmapped),
                (1, 0)
            );
        } else {
            assert!(
                drive(&mut gw, &mut eng, &mut evs, 10, |g, _| g
                    .counters()
                    .sweep_rounds
                    >= 2),
                "the second round never read"
            );
            assert!(
                v.posts().iter().all(|p| !p.body.contains("cancelByCloid")),
                "a perps-only map names no outcome leg"
            );
            assert_eq!(gw.counters().sweep_cancels, 0);
            assert!(gw.counters().sweep_unmapped >= 1);
        }
    }
}

/// XH4i-1: on an outcome map a leg's `#<enc>` names its asset in every
/// stream — a fill, an `orderUpdates` row (the shape measured on testnet,
/// 2026-09-19), a read's open-order row; on a perps-only map (slot 6's)
/// the same rows carry `ASSET_UNKNOWN`, as before.
#[test]
fn outcome_legs_name_their_asset_in_every_stream_on_an_outcome_map() {
    for outcomes in [true, false] {
        let want = if outcomes {
            ASSET_BASE + ENC
        } else {
            ASSET_UNKNOWN
        };
        let v = Venue::start();
        v.policy(|s| {
            if s.body.contains("frontendOpenOrders") {
                Answer::Info(open_orders(&[outcome_row(201, Some((SLOT, 8)))]))
            } else {
                Answer::Silent
            }
        });
        let coins = if outcomes {
            CoinMap::with_outcomes()
        } else {
            CoinMap::new()
        };
        let (mut gw, mut eng) = gateway_on(&v, 5_000_000_000, IP_WEIGHT_SHARE, coins);
        let mut evs = Vec::new();
        live(&mut gw, &mut eng, &mut evs);
        v.push_all(&format!(
            r##"{{"channel":"userFills","data":{{"user":"0x","fills":[{{"coin":"#{ENC}","px":"0.41","sz":"25.0","side":"B","time":1700000000000,"startPosition":"0","dir":"Buy","closedPnl":"0","hash":"0x0","oid":201,"crossed":false,"fee":"0.0","tid":902,"cloid":"{}","feeToken":"USDC"}}]}}}}"##,
            cloid_hex(SLOT, 8)
        ));
        v.push_all(&format!(
            r##"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"#{ENC}","side":"B","limitPx":"0.41","sz":"0.0","oid":201,"timestamp":1,"origSz":"25.0","cloid":"{}"}},"status":"filled","statusTimestamp":1700000000001}}]}}"##,
            cloid_hex(SLOT, 8)
        ));
        assert!(eng.cmd.push(&HlCmd::read_orders(SLOT, 1)));
        assert!(
            drive(&mut gw, &mut eng, &mut evs, 5, |_, e| count(e, EV_FILL)
                >= 1
                && count(e, EV_FILLED) >= 1
                && count(e, EV_READ_DONE) >= 1),
            "outcomes {outcomes}: the streams never arrived"
        );
        let f = evs.iter().find(|e| e.kind == EV_FILL).unwrap();
        assert_eq!((f.asset, f.client_oid, f.slot, f.oid), (want, 8, SLOT, 201));
        let u = evs.iter().find(|e| e.kind == EV_FILLED).unwrap();
        assert_eq!((u.asset, u.client_oid, u.oid), (want, 8, 201));
        let r = evs.iter().find(|e| e.kind == EV_OPEN_ORDER).unwrap();
        assert_eq!((r.asset, r.client_oid, r.oid), (want, 8, 201));
    }
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
