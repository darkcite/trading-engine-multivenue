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
