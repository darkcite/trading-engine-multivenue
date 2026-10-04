// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX3 (O-BX19) — the live composition, with a REAL arm in it.**
//!
//! `RoutedDispatcher<PaperDispatcher, SlotSplit<VenueSplit<HlExchange<64>,
//! ScriptedBn>, SpyArm>>` — the shape the engine boots once Binance has
//! an arm (O-BX17) — driven through the `OrderDispatch` trait, with the
//! real Hyperliquid arm in front of a scripted TLS venue on loopback
//! (O-BX20: `HlConfig::with_port`, the test-only `loopback` feature).
//! Unit tests prove each layer forwards; this proves the layers compose:
//! every verb reaches the arm that trades its route venue, leaves the
//! host when that arm is real, and the router's ledger follows it.
//!
//! | step | asserts |
//! |---|---|
//! | submit, Hyperliquid slot | reaches `HlExchange`, leaves the host as an `order` action, books one resting row |
//! | modify, Hyperliquid slot | leaves the host as a `batchModify` naming the resting cloid, and RENAMES the router's row |
//! | cancel of the replacement | leaves the host as a `cancelByCloid` and releases the renamed row — the rename, observed |
//! | submit + modify, Binance slot | reach the scripted Binance arm and never the Hyperliquid one |
//! | the legacy anchor (flat id 7, venue byte 0) | reaches the Binance arm through the ROUTER's alias table, handed down to the split — the only copy |
//! | slot 0 | reaches the hyparb arm (`SlotSplit`) |
//! | idle | the Binance arm's `Retired` releases the renamed row and is counted by `why` through `exec_counters`; the Hyperliquid arm's user stream is refused cleanly |
//!
//! Break-and-watch (recorded in the BX3 record): drop the router's
//! `ledger.on_modify` → the cancel of the replacement leaves slot 3's
//! row resting, and the retirement leaves slot 2's; route
//! `VenueSplit::modify` to `a` unconditionally → the Binance modify
//! reaches `HlExchange`, which refuses the unbound symbol (LAW E-4).
//!
//! **XMM XH4f** ([`slot_six`]): slot 6 outermost on its own arm — the
//! Hyperliquid gateway's engine half — through the same router: the
//! queued contract (a landed modify renames the row, a cancel releases it
//! on the arm's retirement, a refused modify's hold goes with its
//! replacement's), XH2-C and the venue's day spend (S7-L1).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clob_dispatcher::{
    DispatchError, DispatchStats, HaltSignal, OrderDispatch, PaperDispatcher, Retired,
    RETIRED_CANCELED_TTL,
};
use core_types::{
    make_symbol_id, CancelReq, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId, VenueId,
};
use exec_hyperliquid::config::{HlConfig, Scope};
use exec_hyperliquid::{AddressBudget, HlExchange};
use exec_router::{
    ExecMode, ExecRoute, HaltLimits, InstrumentSpec, RoutedDispatcher, SlotCaps, SlotSplit,
    VenueSplit, LAW_LINEAR, LAW_SPOT,
};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig, ServerConnection, Stream};

const KEY: [u8; 32] = [0x5a; 32];
const ADDR: [u8; 20] = [0x6b; 20];

const HL_SLOT: u8 = 3;
const BN_SLOT: u8 = 2;
const HYPARB_SLOT: u8 = 0;

const PLACED: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424242}}]}}}"#;
const MODIFIED: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424243}}]}}}"#;
const CANCELLED: &[u8] =
    br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;

/// The scripted Hyperliquid venue: every POST takes the next body; a
/// WebSocket upgrade (the arm's user stream) is refused by closing.
struct Venue {
    port: u16,
    client: Arc<ClientConfig>,
    /// The request bodies answered, in order.
    posts: Arc<Mutex<Vec<String>>>,
    /// User-stream upgrades refused.
    ws_refused: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
}

impl Drop for Venue {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

fn venue(bodies: &'static [&'static [u8]]) -> Venue {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der: CertificateDer<'static> = c.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");
    let server = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("trust anchor");
    let client = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let script = Arc::new(Mutex::new(bodies.iter().copied().collect::<VecDeque<_>>()));
    let posts = Arc::new(Mutex::new(Vec::new()));
    let ws_refused = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (posts_srv, ws_srv, done_srv) = (posts.clone(), ws_refused.clone(), done.clone());
    // Bounded: the loop ends with the test (the `Venue` drop) or after
    // 20 s, whichever is first, so no thread outlives a failed test.
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done_srv.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((sock, _)) => {
                    let (server, script, posts, ws) = (
                        server.clone(),
                        script.clone(),
                        posts_srv.clone(),
                        ws_srv.clone(),
                    );
                    thread::spawn(move || serve(sock, &server, &script, &posts, &ws));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    Venue {
        port,
        client,
        posts,
        ws_refused,
        done,
    }
}

/// One connection: requests in turn, until the peer or the script ends.
fn serve(
    mut sock: TcpStream,
    server: &Arc<ServerConfig>,
    script: &Mutex<VecDeque<&'static [u8]>>,
    posts: &Mutex<Vec<String>>,
    ws_refused: &AtomicUsize,
) {
    let _ = sock.set_nonblocking(false);
    let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));
    let mut conn = ServerConnection::new(server.clone()).expect("conn");
    let mut stream = Stream::new(&mut conn, &mut sock);
    loop {
        let Some((head, body)) = read_request(&mut stream) else {
            return;
        };
        if head.starts_with("GET ") {
            // The user stream's upgrade: refused by hanging up, which the
            // arm counts as a connect failure and backs off from.
            ws_refused.fetch_add(1, Ordering::SeqCst);
            return;
        }
        let Some(reply) = script.lock().expect("script").pop_front() else {
            return;
        };
        // Recorded BEFORE the reply goes out, so the client cannot read
        // it, finish and assert before the record exists.
        posts.lock().expect("posts").push(body);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            reply.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(reply).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

/// One whole request — the head, then `Content-Length` bytes of body.
fn read_request(stream: &mut Stream<'_, ServerConnection, TcpStream>) -> Option<(String, String)> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0usize;
    let (head_end, want) = loop {
        let n = stream.read(&mut buf[total..]).ok()?;
        if n == 0 {
            return None;
        }
        total += n;
        if let Some(i) = (0..total.saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
            let len = head
                .split("content-length:")
                .nth(1)
                .and_then(|t| t.split("\r\n").next())
                .and_then(|t| t.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while total < head_end + want {
        let n = stream.read(&mut buf[total..]).ok()?;
        if n == 0 {
            return None;
        }
        total += n;
    }
    Some((
        String::from_utf8_lossy(&buf[..head_end]).into_owned(),
        String::from_utf8_lossy(&buf[head_end..head_end + want]).into_owned(),
    ))
}

/// A scripted Binance arm: accepts every verb, keeps the venue's view
/// of what is working, and TTL-cancels all of it at its next idle
/// moment — each one handed to the router as a `Retired`.
#[derive(Default)]
struct ScriptedBn {
    verbs: Vec<&'static str>,
    working: Vec<(u64, u8)>,
    retire: VecDeque<Retired>,
}

impl OrderDispatch for ScriptedBn {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.verbs.push("submit");
        self.working.push((order.client_oid, order.strategy_id));
        Ok(())
    }

    fn cancel(&mut self, req: &CancelReq) -> Result<(), DispatchError> {
        self.verbs.push("cancel");
        self.working.retain(|w| w.0 != req.client_oid);
        Ok(())
    }

    fn modify(&mut self, req: &ModifyReq) -> Result<(), DispatchError> {
        self.verbs.push("modify");
        let prev = req.prev_client_oid();
        let Some(w) = self.working.iter_mut().find(|w| w.0 == prev) else {
            return Err(DispatchError::NoLiveRoute);
        };
        w.0 = req.order().client_oid;
        Ok(())
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    fn try_next_retired(&mut self) -> Option<Retired> {
        self.retire.pop_front()
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats::default()
    }

    fn on_idle(&mut self) -> bool {
        let worked = !self.working.is_empty();
        for (oid, slot) in self.working.drain(..) {
            self.retire
                .push_back(Retired::new(oid, slot, RETIRED_CANCELED_TTL));
        }
        worked
    }

    fn halt_signal(&self) -> HaltSignal {
        HaltSignal::new(0, 0, 0, 0, false, true, 1_000_000)
    }
}

/// Slot 0's arm: records what reached it.
#[derive(Default)]
struct SpyArm {
    seen: Vec<u64>,
}

impl OrderDispatch for SpyArm {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.seen.push(order.client_oid);
        Ok(())
    }

    fn cancel(&mut self, _req: &CancelReq) -> Result<(), DispatchError> {
        Ok(())
    }

    fn modify(&mut self, _req: &ModifyReq) -> Result<(), DispatchError> {
        Ok(())
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    fn stats(&self) -> DispatchStats {
        DispatchStats::default()
    }

    fn halt_signal(&self) -> HaltSignal {
        HaltSignal::new(0, 0, 0, 0, false, true, 1_000_000)
    }
}

fn order(slot: u8, venue: VenueId, sym: SymbolId, px: i64, oid: u64) -> Order {
    let mut o = Order::new(
        core_time::now_ns(),
        venue,
        sym,
        Side::Bid,
        core_fill::ORDER_KIND_MAKER,
        Price::from_raw(px),
        Qty::from_raw(1_000_000),
        oid,
    );
    o.strategy_id = slot;
    o
}

/// The cloid LAW E-9 puts on the wire for `(slot, client_oid)`.
fn cloid(slot: u8, client_oid: u64) -> String {
    format!("\"0x4d56{slot:02x}0000000000{client_oid:016x}\"")
}

type Composed =
    RoutedDispatcher<PaperDispatcher, SlotSplit<VenueSplit<HlExchange<64>, ScriptedBn>, SpyArm>>;

#[test]
fn every_verb_reaches_the_arm_of_its_route_venue_and_the_ledger_follows() {
    let hl = VenueId::Hyperliquid as u8;
    let bn = VenueId::Binance as u8;
    let hl_sym = make_symbol_id(VenueId::Hyperliquid, 5);
    let bn_sym = make_symbol_id(VenueId::Binance, 600);

    let v = venue(&[PLACED, MODIFIED, CANCELLED]);

    // The arm's durable files, in a directory of this test's own; the
    // budget restored with headroom so the governor lets a submit out.
    let dir = std::env::temp_dir().join(format!("bx3-composition-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("state dir");
    let budget_path = dir.join("exec-budget.state");
    exec_hyperliquid::budget::store(&budget_path, &AddressBudget::restored(ADDR, 100, 0, 0))
        .expect("budget file");
    let cfg = HlConfig::new(Scope::Testnet, "localhost", 'b', KEY, ADDR)
        .expect("cfg")
        .with_port(v.port)
        .expect("a local double takes any port");
    let (fills, _fills_rx) = core_ring::Ring::<Fill, 64>::new().split();
    let mut hlx =
        HlExchange::<64>::new(&cfg, v.client.clone(), fills, budget_path, 100).expect("arm");
    // LAW E-4: bound by a roll in production; instance 7 is what every
    // client id below names in its low bits.
    hlx.assets_mut().bind(hl_sym, 3, 7, b"#5").expect("bind");

    let limits = HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000);
    let caps = SlotCaps::new(1_000_000_000, i64::MAX, i64::MAX, 64);
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(HYPARB_SLOT as usize, ExecMode::Live, &[hl], caps, limits)
        .expect("slot 0");
    route
        .set_slot(BN_SLOT as usize, ExecMode::Live, &[bn], caps, limits)
        .expect("slot 2");
    route
        .set_slot(HL_SLOT as usize, ExecMode::Live, &[hl], caps, limits)
        .expect("slot 3");
    let arms = SlotSplit::new(
        HYPARB_SLOT,
        VenueSplit::new(bn, hlx, ScriptedBn::default(), &route),
        SpyArm::default(),
    );
    let mut d: Composed = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        arms,
        core_time::WallAnchor::now(),
    );
    // The boot's alias table (`cli::exec_boot::route_aliases`), set on the
    // router ONCE — which hands it down to the split.
    let anchor = core_config::universe::LEGACY_BN_ANCHOR_SYM;
    d.set_route_aliases(cli::exec_boot::route_aliases(Some(anchor)).expect("aliases"));
    assert_eq!(
        d.live().a().aliases(),
        d.route_aliases(),
        "one table, not two"
    );
    d.bind_instrument(&InstrumentSpec::new(bn_sym, LAW_LINEAR, 0, 0, 0))
        .expect("row");
    d.bind_instrument(&InstrumentSpec::new(anchor, LAW_SPOT, 0, 0, 0))
        .expect("anchor row");
    d.mark_ledger_seeded();

    // --- Hyperliquid: submit, modify, cancel -- all through the real arm.
    let a1 = (1u64 << 32) | 7;
    let a2 = (2u64 << 32) | 7;
    assert_eq!(
        d.submit(&order(HL_SLOT, VenueId::Hyperliquid, hl_sym, 470_000, a1)),
        Ok(())
    );
    assert_eq!(d.ledger().slot_resting(HL_SLOT as usize), 1);
    let repl = order(HL_SLOT, VenueId::Hyperliquid, hl_sym, 471_000, a2);
    assert_eq!(d.modify(&ModifyReq::new(a1, repl)), Ok(()));
    assert_eq!(
        d.ledger().slot_resting(HL_SLOT as usize),
        1,
        "a modify does not add a row"
    );
    let t = core_time::now_ns();
    assert_eq!(d.cancel(&CancelReq::of(&repl, t)), Ok(()));
    assert_eq!(
        d.ledger().slot_resting(HL_SLOT as usize),
        0,
        "the cancel of the replacement released the row — so the modify renamed it"
    );
    {
        let posts = v.posts.lock().expect("posts");
        assert_eq!(
            posts.len(),
            3,
            "exactly the three verbs left the host: {posts:?}"
        );
        assert!(posts[0].contains("\"type\":\"order\""), "{}", posts[0]);
        assert!(posts[0].contains(&cloid(HL_SLOT, a1)), "{}", posts[0]);
        assert!(
            posts[1].contains("\"type\":\"batchModify\""),
            "{}",
            posts[1]
        );
        assert!(
            posts[1].contains(&format!("\"oid\":{}", cloid(HL_SLOT, a1))),
            "{}",
            posts[1]
        );
        assert!(
            posts[1].contains(&format!("\"c\":{}", cloid(HL_SLOT, a2))),
            "{}",
            posts[1]
        );
        assert!(
            posts[2].contains("\"type\":\"cancelByCloid\""),
            "{}",
            posts[2]
        );
        assert!(posts[2].contains(&cloid(HL_SLOT, a2)), "{}", posts[2]);
    }
    let hc = *d.live().a().a().counters();
    assert_eq!((hc.submitted, hc.modifies_sent, hc.cancels_sent), (1, 1, 1));

    // --- Binance: submit and modify reach the scripted arm, never HL.
    let b1 = 11u64;
    let b2 = 12u64;
    assert_eq!(
        d.submit(&order(BN_SLOT, VenueId::Binance, bn_sym, 100_000_000, b1)),
        Ok(())
    );
    let bn_repl = order(BN_SLOT, VenueId::Binance, bn_sym, 101_000_000, b2);
    assert_eq!(d.modify(&ModifyReq::new(b1, bn_repl)), Ok(()));
    assert_eq!(d.live().a().b().verbs, ["submit", "modify"]);
    assert_eq!(d.live().a().b().working, [(b2, BN_SLOT)]);
    assert_eq!(d.ledger().slot_resting(BN_SLOT as usize), 1);
    // The anchor stamps Polymarket's venue byte; the alias routes it to
    // the Binance arm.
    let b3 = 13u64;
    assert_eq!(
        d.submit(&order(
            BN_SLOT,
            VenueId::Polymarket,
            anchor,
            100_000_000,
            b3
        )),
        Ok(())
    );
    assert_eq!(d.live().a().b().verbs, ["submit", "modify", "submit"]);
    assert_eq!(d.live().a().b().working, [(b2, BN_SLOT), (b3, BN_SLOT)]);
    assert_eq!(d.ledger().slot_resting(BN_SLOT as usize), 2);
    let hc = *d.live().a().a().counters();
    assert_eq!(
        (hc.submitted, hc.modifies_sent),
        (1, 1),
        "Hyperliquid saw no Binance verb"
    );

    // --- Slot 0: the hyparb arm.
    assert_eq!(
        d.submit(&order(
            HYPARB_SLOT,
            VenueId::Hyperliquid,
            hl_sym,
            470_000,
            21
        )),
        Ok(())
    );
    assert_eq!(d.live().b().seen, [21]);
    assert_eq!(d.live().a().a().counters().submitted, 1);

    // --- Idle: the Binance venue TTL-cancels both of its orders. The
    // renamed one's retirement names the REPLACEMENT's id, so it releases
    // the row only because the router renamed it on the modify.
    d.on_idle();
    assert_eq!(d.ledger().slot_resting(BN_SLOT as usize), 0);
    let e = d.exec_counters();
    assert_eq!(e.retired[RETIRED_CANCELED_TTL as usize], 2);
    assert_eq!(e.retired.iter().sum::<u64>(), 2);
    assert_eq!(e.refused_risk, 0);
    assert_eq!(e.live_submits, 4);
    // The Hyperliquid arm dialled its user stream on the same idle
    // moment and was refused cleanly: counted, backed off, no halt.
    assert_eq!(v.ws_refused.load(Ordering::SeqCst), 1);
    assert_eq!(d.live().a().a().counters().ws_connect_failures, 1);
    for slot in [HYPARB_SLOT, BN_SLOT, HL_SLOT] {
        assert!(
            !d.halt().reason(slot as usize).is_halted(),
            "slot {slot} halted"
        );
    }
    assert_eq!(
        v.posts.lock().expect("posts").len(),
        3,
        "idle sent nothing to /exchange"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// **XMM XH5a (R-XH4-1) — a halt sweeps the halted slot's orders alone,
/// through the REAL arm.** Slot 6's shape on the operator's arm
/// (`SlotSplit(6, HlExchange, …)`), slots 3 and 4 live on one account, one
/// order each resting on one leg. Slot 3 halts: the arm's sweep reads the
/// venue's open orders — both slots' cloids on that leg — and cancels slot
/// 3's oid alone; the router releases slot 3's row only once the sweep has
/// confirmed, and slot 4's row and order stay.
#[test]
fn a_halt_sweeps_the_halted_slot_s_orders_alone_through_the_real_arm() {
    const PLACED_4: &[u8] =
        br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424244}}]}}}"#;
    let hl = VenueId::Hyperliquid as u8;
    let hl_sym = make_symbol_id(VenueId::Hyperliquid, 5);
    let (a3, a4) = ((1u64 << 32) | 7, (2u64 << 32) | 7);
    // The sweep's read: slot 3's and slot 4's cloids, both resting on `#5`.
    let listing: &'static [u8] = Box::leak(
        format!(
            r##"[{{"coin":"#5","oid":424242,"cloid":{}}},{{"coin":"#5","oid":424244,"cloid":{}}}]"##,
            cloid(3, a3),
            cloid(4, a4)
        )
        .into_bytes()
        .into_boxed_slice(),
    );
    let script: &'static [&'static [u8]] =
        Box::leak(vec![PLACED, PLACED_4, listing, CANCELLED].into_boxed_slice());
    let v = venue(script);

    let dir = std::env::temp_dir().join(format!("xh5a-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("state dir");
    let budget_path = dir.join("exec-budget.state");
    exec_hyperliquid::budget::store(&budget_path, &AddressBudget::restored(ADDR, 100, 0, 0))
        .expect("budget file");
    let cfg = HlConfig::new(Scope::Testnet, "localhost", 'b', KEY, ADDR)
        .expect("cfg")
        .with_port(v.port)
        .expect("a local double takes any port");
    let (fills, _fills_rx) = core_ring::Ring::<Fill, 64>::new().split();
    let mut hlx =
        HlExchange::<64>::new(&cfg, v.client.clone(), fills, budget_path, 100).expect("arm");
    hlx.assets_mut().bind(hl_sym, 3, 7, b"#5").expect("bind");

    let limits = HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000);
    let caps = SlotCaps::new(1_000_000_000, i64::MAX, i64::MAX, 64);
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(3, ExecMode::Live, &[hl], caps, limits)
        .expect("slot 3");
    route
        .set_slot(4, ExecMode::Live, &[hl], caps, limits)
        .expect("slot 4");
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        SlotSplit::new(6, hlx, exec_router::NullLiveDispatcher::new()),
        core_time::WallAnchor::now(),
    );
    d.mark_ledger_seeded();
    let o3 = order(3, VenueId::Hyperliquid, hl_sym, 470_000, a3);
    assert_eq!(d.submit(&o3), Ok(()));
    let o4 = order(4, VenueId::Hyperliquid, hl_sym, 469_000, a4);
    assert_eq!(d.submit(&o4), Ok(()));
    assert_eq!(
        (d.ledger().slot_resting(3), d.ledger().slot_resting(4)),
        (1, 1)
    );

    // Slot 3 halts: its sweep is queued for slot 3 alone; nothing is
    // confirmed yet, so its row stays booked.
    assert!(d.halt_slot(3, exec_router::HaltReason::Operator));
    assert!(d.halt().cancel_outstanding());
    let working = clob_dispatcher::CancelAllState::Working;
    let clear = clob_dispatcher::CancelAllState::Clear;
    assert_eq!(d.live().a().cancel_slot_state(3), working);
    assert_eq!(
        d.live().a().cancel_slot_state(4),
        clear,
        "4 was not asked for"
    );
    assert_eq!(
        d.ledger().slot_resting(3),
        1,
        "released only on the confirmation"
    );

    // The idle path: the read, slot 3's cancel, the confirmation.
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.halt().cancel_outstanding() && Instant::now() < deadline {
        d.on_idle();
        thread::sleep(Duration::from_millis(2));
    }
    assert!(!d.halt().cancel_outstanding(), "slot 3's sweep confirmed");
    assert_eq!(
        (d.ledger().slot_resting(3), d.ledger().slot_resting(4)),
        (0, 1),
        "slot 3's row released; slot 4's order and row stay"
    );
    assert!(!d.halt().is_halted(4));
    {
        let posts = v.posts.lock().expect("posts");
        assert_eq!(
            posts.len(),
            4,
            "two places, one read, ONE cancel: {posts:?}"
        );
        assert!(posts[2].contains("frontendOpenOrders"), "{}", posts[2]);
        assert!(
            posts[3].contains("\"type\":\"cancel\"") && posts[3].contains("\"o\":424242"),
            "{}",
            posts[3]
        );
        assert!(
            !posts[3].contains("424244"),
            "slot 4's oid was never cancelled: {}",
            posts[3]
        );
    }
    let c = *d.live().a().counters();
    assert_eq!((c.sweeps_run, c.sweep_cancelled), (1, 1));
    assert_eq!((c.sweep_left, c.cancel_all_unqueued), (0, 0));

    std::fs::remove_dir_all(&dir).ok();
}

/// **XMM XH4f — slot 6 outermost, on its own arm.** The Hyperliquid
/// gateway's engine half (`HlGatewayArm`) behind `SlotSplit(6,
/// NullLiveDispatcher, arm)` — the shape the engine boots for slot 6
/// alone — with the gateway's end scripted in-process (the gateway's own
/// turn is its loopback tests' and gate 96b's), driven through the router
/// as the engine drives it: `on_idle`, the verbs, fills taken and booked.
mod slot_six {
    use super::*;
    use core_types::{
        ChannelEvent, ChannelId, EXEC_BLOCKED_UNSEEDED, ORDER_EVENT_REASON_BAD_ALO_PX,
        ORDER_EVENT_REASON_OTHER,
    };
    use exec_gateway::stats::SWEEP_CLEAR;
    use exec_hyperliquid::arm::{ArmCfg, HlGatewayArm, PerpBind, ARM_PERPS};
    use exec_hyperliquid::cmd::{
        CMD_CANCEL, CMD_MODIFY, CMD_PLACE, CMD_READ_DAY, CMD_READ_LIMIT, CMD_READ_ORDERS,
        CMD_READ_STATE, CMD_SCHEDULE_CANCEL,
    };
    use exec_hyperliquid::deadman::DeadManCfg;
    use exec_hyperliquid::event::{
        EVF_BUY, EVF_CMD_SHIFT, EVF_POST_ITEM, EVF_RESTING, EV_ACK, EV_CANCELED, EV_FILL,
        EV_READ_DONE, EV_REJECTED, EV_SNAPSHOT, READ_DAY, READ_LIMIT, READ_ORDERS, READ_STATE,
    };
    use exec_hyperliquid::gateway::{hl_duplex, HlGatewayEnd};
    use exec_hyperliquid::rules::PerpRules;
    use exec_hyperliquid::{ExecEvent, HlCmd};
    use exec_router::{NullLiveDispatcher, PerpLimits};

    const SLOT: u8 = 6;
    const ASSET: u32 = 3;
    const E6: i64 = 1_000_000;
    const E8: i64 = 100_000_000;
    const DAY_MS: u64 = 86_400_000;

    type Slot6 = RoutedDispatcher<PaperDispatcher, SlotSplit<NullLiveDispatcher, HlGatewayArm>>;

    /// The gateway's end, scripted: commands pulled into a list the test
    /// takes from, events injected as the gateway builds them.
    struct Gw {
        end: HlGatewayEnd,
        cmds: Vec<HlCmd>,
    }

    impl Gw {
        /// The first command of `kind` the arm pushed, taken.
        fn take(&mut self, kind: u8) -> HlCmd {
            while let Some(c) = self.end.cmd.pop() {
                self.cmds.push(*c);
            }
            let Some(i) = self.cmds.iter().position(|c| c.kind == kind) else {
                let seen: Vec<u8> = self.cmds.iter().map(|c| c.kind).collect();
                panic!("no command {kind} among {seen:?}");
            };
            self.cmds.remove(i)
        }

        fn send(&mut self, e: ExecEvent) {
            assert!(self.end.evt.emit(&e), "event ring full");
        }

        /// A post item's verdict.
        fn item(&mut self, c: &HlCmd, kind: u8, reason: u8, flags: u8) {
            let bits = if c.kind <= CMD_CANCEL { c.kind } else { 0 };
            self.send(ExecEvent {
                client_oid: c.client_oid,
                asset: c.asset,
                kind,
                reason,
                slot: c.slot,
                flags: EVF_POST_ITEM | (bits << EVF_CMD_SHIFT) | flags,
                ..ExecEvent::default()
            });
        }

        /// A read's end: the sheet's account value or the day's sum in
        /// `px_1e8`, the budget's requests used or the day's rows in `aux`.
        fn read_end(&mut self, c: &HlCmd, px_1e8: i64, aux: u64, venue_ms: u64) {
            let read = match c.kind {
                CMD_READ_ORDERS => READ_ORDERS,
                CMD_READ_LIMIT => READ_LIMIT,
                CMD_READ_DAY => READ_DAY,
                _ => READ_STATE,
            };
            self.send(ExecEvent {
                client_oid: c.client_oid,
                venue_ms,
                px_1e8,
                aux,
                kind: EV_READ_DONE,
                reason: read,
                slot: c.slot,
                ..ExecEvent::default()
            });
        }

        /// An `orderUpdates` row.
        fn row(&mut self, kind: u8, cid: u64, reason: u8) {
            self.send(ExecEvent {
                client_oid: cid,
                asset: ASSET,
                kind,
                reason,
                slot: SLOT,
                ..ExecEvent::default()
            });
        }

        /// A `userFills` row, a buy.
        fn bought(&mut self, cid: u64, px_1e8: i64, sz_1e8: i64, venue_ms: u64) {
            self.send(ExecEvent {
                client_oid: cid,
                venue_ms,
                px_1e8,
                sz_1e8,
                asset: ASSET,
                kind: EV_FILL,
                slot: SLOT,
                flags: EVF_BUY,
                ..ExecEvent::default()
            });
        }
    }

    fn quote(cid: u64, sym: SymbolId, px_1e6: i64, qty_1e6: i64) -> Order {
        let mut o = Order::new(
            core_time::now_ns(),
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            core_fill::ORDER_KIND_MAKER,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            cid,
        )
        .with_post_only();
        o.strategy_id = SLOT;
        o
    }

    /// The engine's side of the fills: taken, then booked back.
    fn book_fills(d: &mut Slot6) -> usize {
        let mut n = 0;
        while let Some(f) = d.try_next_fill() {
            d.on_fill_booked(&f);
            n += 1;
        }
        n
    }

    fn wall_ms() -> u64 {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970");
        u64::try_from(t.as_millis()).expect("ms")
    }

    /// | step | asserts |
    /// |---|---|
    /// | boot | the arm boots through the router's idle (the dead man's clear, the snapshot, the sweep, the sheet, the orders, the budget, the day); no order before |
    /// | the day (S7-L1) | until it is read the slot is unseeded — its health says so, a place is refused; the venue's day spend then seeds the router's ledger through the split — slot 6's alone |
    /// | place | reaches the arm, rests, books the router's row |
    /// | modify | a landed modify RENAMES the row (the queued contract) |
    /// | cancel | the replacement's cancel releases the row only on the arm's retirement — the rename, observed |
    /// | refused modify | its asked size is held while unanswered; the hold goes with its replacement's retirement, the original rests on |
    /// | XH2-C | a fill leaves a position no mark prices: growth refused; a mark lets it through |
    /// | a replacement ending with its landing | the landing and the end in one pump: the rename drains before the retirement — no phantom row |
    #[test]
    fn slot_six_rides_its_gateway_arm_outermost_and_the_ledger_keeps_the_queued_contract() {
        let hl = VenueId::Hyperliquid as u8;
        let sym = make_symbol_id(VenueId::Hyperliquid, 950);
        let bind = PerpBind {
            sym,
            asset: ASSET,
            rules: PerpRules::new(4).expect("rules"),
        };
        let cfg = ArmCfg {
            slot: SLOT,
            perps: [bind; ARM_PERPS],
            n_perps: 1,
            expires_after_ms: 5_000,
            post_timeout_ms: 5_000,
            skew_ms: 2_000,
            recon_ms: 5_000,
            join_ms: 1_000,
            gone_ms: 3_000,
            shutdown_ms: 300,
            own_account: false,
            anchor_1e6: 0,
            deadman: DeadManCfg::DEFAULT,
            budget_floor: 2_000,
            limit_ms: 600_000,
            leverage: 0,
        };
        let (eng, end) = hl_duplex();
        let arm = HlGatewayArm::new(&cfg, eng).expect("arm");
        let mut gw = Gw {
            end,
            cmds: Vec::new(),
        };
        // Slot 6 live: order $100, gross $1 000, day $5 000, 16 orders;
        // per perp $150, resting $200 (deviation 19's pair).
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[hl],
                SlotCaps::new(100 * E6, 1_000 * E6, 5_000 * E6, 16),
                HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000),
            )
            .expect("slot 6");
        route
            .set_perp_limits(SLOT as usize, PerpLimits::new(150 * E6, 200 * E6))
            .expect("the perp pair");
        let mut d: Slot6 = RoutedDispatcher::new(
            route,
            PaperDispatcher::new(),
            SlotSplit::new(SLOT, NullLiveDispatcher::new(), arm),
            core_time::WallAnchor::now(),
        );
        // `finish_xmm_boot`: the member's perp as a LAW_LINEAR row.
        d.bind_instrument(&InstrumentSpec::new(sym, LAW_LINEAR, 0, 0, 0))
            .expect("row");
        let slot = SLOT as usize;

        // --- The boot, through the router's idle.
        gw.end.stats.set_gauges(0, true);
        gw.end.stats.beat(core_time::now_ns());
        d.on_idle();
        let clear = gw.take(CMD_SCHEDULE_CANCEL);
        gw.item(&clear, EV_ACK, 0, 0);
        assert_eq!(
            d.submit(&quote(99, sym, 100 * E6, E6 / 2)),
            Err(DispatchError::RiskRefused),
            "no order before the boot: the unseeded ledger refuses it before the arm is asked"
        );
        gw.send(ExecEvent {
            kind: EV_SNAPSHOT,
            ..ExecEvent::default()
        });
        d.on_idle();
        let asked = gw.end.stats.sweep_requested();
        gw.end.stats.set_sweep(asked, SWEEP_CLEAR);
        d.on_idle();
        let sheet = gw.take(CMD_READ_STATE);
        gw.read_end(&sheet, 1_000 * E8, 0, 1);
        d.on_idle();
        let orders = gw.take(CMD_READ_ORDERS);
        gw.read_end(&orders, 0, 0, 0);
        d.on_idle();
        let budget = gw.take(CMD_READ_LIMIT);
        gw.read_end(&budget, 0, 100, 0);
        d.on_idle();

        // --- S7-L1: the day's spend — one page, two rows, $300, ending
        // where it began — reaches the router through the split.
        let day = gw.take(CMD_READ_DAY);
        assert_eq!(d.live().venue_day_bought(slot), None, "not before its read");
        // S7-L1, end to end: until the day is read the router's ledger is
        // unseeded for slot 6 — its health says so, and a place is refused.
        assert!(!d.ledger().is_slot_seeded(slot), "unseeded before the day");
        assert_ne!(
            d.exec_health(SLOT, core_time::now_ns()).blocked & EXEC_BLOCKED_UNSEEDED,
            0
        );
        assert_eq!(
            d.submit(&quote(98, sym, 100 * E6, E6 / 2)),
            Err(DispatchError::RiskRefused),
            "an unseeded slot places nothing"
        );
        gw.read_end(&day, 300 * E8, 2, day.time_ms);
        d.on_idle();
        assert!(
            d.ledger().is_slot_seeded(slot),
            "seeded once the day is read"
        );
        assert_eq!(
            d.live().venue_day_bought(slot),
            Some((day.time_ms / DAY_MS, 300 * E6))
        );
        assert_eq!(
            d.live().venue_day_bought(3),
            None,
            "another slot's day is its own"
        );
        assert_eq!(d.ledger().slot_day_turnover_1e6(slot), 300 * E6, "adopted");

        // --- A place rests: the router's row.
        assert_eq!(d.submit(&quote(1, sym, 100 * E6, E6 / 2)), Ok(()));
        let place = gw.take(CMD_PLACE);
        assert_eq!(
            (place.slot, place.asset, place.client_oid),
            (SLOT, ASSET, 1)
        );
        gw.item(&place, EV_ACK, 0, EVF_RESTING);
        d.on_idle();
        assert_eq!(d.ledger().slot_resting(slot), 1);
        assert_eq!(d.ledger().resting_usd_1e6(slot), 50 * E6);

        // --- A landed modify renames the row (1 → 2) ...
        let q2 = quote(2, sym, 99 * E6, E6 / 2);
        assert_eq!(d.modify(&ModifyReq::new(1, q2)), Ok(()));
        let m = gw.take(CMD_MODIFY);
        assert_eq!((m.client_oid, m.prev_client_oid), (2, 1));
        gw.item(&m, EV_ACK, 0, EVF_RESTING);
        d.on_idle();
        assert_eq!(d.ledger().slot_resting(slot), 1, "renamed, not added");
        // ... and the replacement's cancel releases it on the arm's
        // retirement only — which names 2, so the rename is observed.
        assert_eq!(d.cancel(&CancelReq::of(&q2, core_time::now_ns())), Ok(()));
        let c = gw.take(CMD_CANCEL);
        assert_eq!(c.client_oid, 2);
        assert_eq!(
            d.ledger().slot_resting(slot),
            1,
            "a queued arm's cancel waits for its end"
        );
        gw.item(&c, EV_ACK, 0, 0);
        gw.row(EV_CANCELED, 2, ORDER_EVENT_REASON_OTHER);
        d.on_idle();
        d.on_idle();
        assert_eq!(
            d.ledger().slot_resting(slot),
            0,
            "released by the retirement of 2"
        );

        // --- A refused modify: the asked size held while unanswered, the
        // hold gone with the replacement's retirement.
        assert_eq!(d.submit(&quote(3, sym, 100 * E6, E6 / 2)), Ok(()));
        let place = gw.take(CMD_PLACE);
        gw.item(&place, EV_ACK, 0, EVF_RESTING);
        d.on_idle();
        assert_eq!(
            d.modify(&ModifyReq::new(3, quote(4, sym, 100 * E6, E6))),
            Ok(())
        );
        assert_eq!(
            d.ledger().resting_usd_1e6(slot),
            100 * E6,
            "held at the asked size"
        );
        let m = gw.take(CMD_MODIFY);
        gw.item(&m, EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0);
        d.on_idle();
        d.on_idle();
        assert_eq!(d.ledger().slot_resting(slot), 1, "the original rests on");
        assert_eq!(
            d.ledger().resting_usd_1e6(slot),
            50 * E6,
            "the hold went with 4's retirement"
        );

        // --- XH2-C: a fill leaves a position no mark prices — growth is
        // refused until a mark arrives.
        gw.bought(3, 100 * E8, E8 / 4, wall_ms());
        d.on_idle();
        assert_eq!(book_fills(&mut d), 1);
        let grow = quote(5, sym, 100 * E6, E6 / 10);
        assert_eq!(d.submit(&grow), Err(DispatchError::RiskRefused));
        assert_eq!(d.counters().refused_unpriced, 1);
        d.on_venue_event(&ChannelEvent::new(
            core_time::now_ns(),
            VenueId::Hyperliquid,
            ChannelId::Mark,
            sym,
            0,
            0,
            100 * E6,
            100 * E6,
        ));
        assert_eq!(d.submit(&grow), Ok(()));
        assert_eq!(gw.take(CMD_PLACE).client_oid, 5);

        // --- A replacement that ends with its landing, both in one pump
        // before the next idle (risk r1): the rename drains before the
        // retirement — no phantom row.
        assert_eq!(d.ledger().slot_resting(slot), 2, "3 and 5");
        assert_eq!(
            d.modify(&ModifyReq::new(3, quote(6, sym, 99 * E6, E6 / 2))),
            Ok(())
        );
        let m = gw.take(CMD_MODIFY);
        gw.item(&m, EV_ACK, 0, EVF_RESTING);
        gw.row(EV_CANCELED, 6, ORDER_EVENT_REASON_OTHER);
        assert_eq!(
            book_fills(&mut d),
            0,
            "the engine's fill drain pumps the arm"
        );
        d.on_idle();
        d.on_idle();
        assert_eq!(d.ledger().slot_resting(slot), 1, "5 alone — no phantom 6");
        for s in 0..8usize {
            assert!(!d.halt().reason(s).is_halted(), "slot {s} halted");
        }
    }
}
