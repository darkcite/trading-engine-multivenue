// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: Hyperliquid public WS ingress against a real
//! `127.0.0.1` TLS server with a self-signed cert. Four scripted
//! sessions per PLAN §11 ("every ingress" row): the happy path
//! (nine per-subscription subscribe frames — HIP-4 outcome coin
//! `#330` included — → nine `subscriptionResponse` echoes + one
//! `bbo` push per coin → Ticks on the ring → server close), the
//! §6.2 staleness trip (one `l2Book` snapshot then silence →
//! `RunResult::Stale` + one gap), the keepalive idle timeout
//! (venue-specific `{"method":"ping"}` text frame observed, then
//! `RunResult::IdleTimeout`), and the missed-ack deadline
//! (`RunResult::Error`).
//!
//! The client side is the top-level [`run`] loop over the real
//! `TlsTransport`. The happy path syncs on the client's own JSON
//! keepalive ping before closing: a ping fires only after
//! `ping_interval_ns` of inbound silence, and the acks refresh the
//! activity clock when they are *drained* — the same loop iteration
//! whose `session_health` pass verifies the ack mask. Holding a ping
//! therefore proves `driver.is_verified()` flipped, with no sleeps.
//! Sessions end with an in-band WS Close frame — a raced TCP FIN
//! could mark the transport closed before the scripted frames are
//! drained; an in-band Close is always processed after them.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_metrics::{IngressState, IngressStatus};
use core_net::{expected_accept, Keepalive, KeepaliveCfg, TlsTransport};
use core_ring::Ring;
use core_types::{NullCapture, SymbolId, Tick, VenueId};

/// VM2 V2: a throwaway venue-event lane per `run` call (consumer
/// dropped).
fn event_lane() -> core_ring::Producer<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }> {
    Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new()
        .split()
        .0
}
use ingress_hyperliquid::run_loop::{run, Driver, RunResult, StopFlag, TICK_RING_CAP};
use ingress_hyperliquid::{HlCoinTable, PING_PAYLOAD};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// Venue-namespaced test symbols (venue byte 4 = Hyperliquid).
const SYM_BTC: SymbolId = (4 << 24) | 1;
/// HIP-4 outcome coin `#330` — ordinary market-data surface.
const SYM_HIP4: SymbolId = (4 << 24) | 2;

/// Generous budget for whichever deadline a scenario is *not*
/// exercising (monotonic now-since-boot never reaches this).
const GENEROUS_NS: u64 = u64::MAX / 4;

/// Exact subscribe payloads for a {BTC, #330} table, in the driver's
/// deterministic queue order: per coin bbo → l2Book → trades
/// (→ activeAssetCtx, perp coins only — gated off for `#330`), then
/// the two global channels.
const EXP_SUBS_BTC_HIP4: [&[u8]; 9] = [
    br#"{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#330"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#330"}}"##,
    br#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"outcomeMetaUpdates"}}"#,
];

/// Exact subscribe payloads for a BTC-only table (perp coin: all
/// four per-coin channels + the two globals = six frames).
const EXP_SUBS_BTC_ONLY: [&[u8]; 6] = [
    br#"{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"outcomeMetaUpdates"}}"#,
];

struct LoopbackCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> LoopbackCert {
    let cert =
        generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-signed cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("private key DER");
    LoopbackCert { cert_der, key_der }
}

fn build_server_config(cert: &LoopbackCert) -> Arc<ServerConfig> {
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
        .expect("server config");
    Arc::new(cfg)
}

fn build_client_config(cert: &LoopbackCert) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("add trust anchor");
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

fn build_101_reply(client_key: &[u8; 24]) -> Vec<u8> {
    let accept = expected_accept(client_key);
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
    out.extend_from_slice(b"Upgrade: websocket\r\n");
    out.extend_from_slice(b"Connection: Upgrade\r\n");
    out.extend_from_slice(b"Sec-WebSocket-Accept: ");
    out.extend_from_slice(&accept);
    out.extend_from_slice(b"\r\n\r\n");
    out
}

/// Build an *unmasked* server-side text frame (RFC 6455 server
/// frames are never masked). Handles both short and medium length.
fn build_unmasked_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    if payload.len() <= 125 {
        out.push(0x81);
        out.push(payload.len() as u8);
    } else {
        out.push(0x81);
        out.push(126);
        let be = (payload.len() as u16).to_be_bytes();
        out.push(be[0]);
        out.push(be[1]);
    }
    out.extend_from_slice(payload);
    out
}

fn extract_client_key(req: &[u8]) -> [u8; 24] {
    let needle = b"Sec-WebSocket-Key: ";
    let pos = req
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("client must send Sec-WebSocket-Key");
    let start = pos + needle.len();
    let mut out = [0u8; 24];
    out.copy_from_slice(&req[start..start + 24]);
    out
}

/// `needle` occurs somewhere in `haystack` (windows scan — same
/// style as [`extract_client_key`]).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Read the client's HTTP upgrade request and put a valid `101
/// Switching Protocols` on the wire. Identical to the template's
/// inline handshake block — factored because four sessions share it.
fn serve_ws_upgrade(stream: &mut Stream<'_, ServerConnection, TcpStream>) {
    let mut buf = [0u8; 4096];
    let mut total = 0;
    loop {
        let n = stream.read(&mut buf[total..]).expect("server read");
        if n == 0 {
            panic!("client closed before handshake complete");
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if total == buf.len() {
            panic!("oversized client handshake");
        }
    }

    let client_key = extract_client_key(&buf[..total]);
    let reply = build_101_reply(&client_key);
    stream.write_all(&reply).expect("write 101");
}

/// Read one masked client text frame off the TLS stream and return
/// its unmasked payload. Client frames are always `FIN | Text` and
/// masked (RFC 6455 client rule); short and medium lengths only.
fn read_client_text_frame(stream: &mut Stream<'_, ServerConnection, TcpStream>) -> Vec<u8> {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).expect("frame header");
    assert_eq!(hdr[0], 0x81, "client data frames are FIN | Text");
    assert!(hdr[1] & 0x80 != 0, "client frames must be masked");
    let mut len = (hdr[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).expect("extended length");
        len = u16::from_be_bytes(ext) as usize;
    }
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).expect("mask key");
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).expect("frame payload");
    for i in 0..len {
        payload[i] ^= mask[i & 3];
    }
    payload
}

/// Read `expected.len()` subscribe frames — one WS text frame per
/// subscription, no batch form on this venue — and assert each
/// decoded payload byte-for-byte (order is the driver's
/// deterministic queue order). Server-side asserts: a mismatch
/// panics the server thread and fails the join.
fn read_and_check_subscribes(
    stream: &mut Stream<'_, ServerConnection, TcpStream>,
    expected: &[&[u8]],
) -> Vec<Vec<u8>> {
    let mut frames = Vec::with_capacity(expected.len());
    for (i, want) in expected.iter().enumerate() {
        let got = read_client_text_frame(stream);
        assert_eq!(got.as_slice(), *want, "subscribe frame {i} mismatch");
        frames.push(got);
    }
    frames
}

/// Ack one subscription by echoing the request's
/// `{"method":"subscribe","subscription":{...}}` object as the
/// `data` of a `subscriptionResponse` frame — the venue's documented
/// echo shape.
fn write_sub_ack(stream: &mut Stream<'_, ServerConnection, TcpStream>, subscribe_payload: &[u8]) {
    let mut ack = Vec::with_capacity(subscribe_payload.len() + 48);
    ack.extend_from_slice(br#"{"channel":"subscriptionResponse","data":"#);
    ack.extend_from_slice(subscribe_payload);
    ack.push(b'}');
    stream
        .write_all(&build_unmasked_text_frame(&ack))
        .expect("write ack");
}

fn coins_btc_and_hip4() -> HlCoinTable {
    let mut t = HlCoinTable::new();
    t.insert(b"BTC", SYM_BTC).expect("insert BTC");
    t.insert(b"#330", SYM_HIP4).expect("insert #330");
    t
}

fn coins_btc_only() -> HlCoinTable {
    let mut t = HlCoinTable::new();
    t.insert(b"BTC", SYM_BTC).expect("insert BTC");
    t
}

/// Generous but bounded keepalive: no probe interferes with the
/// script, and a wedged session ends the test via `IdleTimeout`
/// instead of hanging forever.
fn generous_keepalive() -> Keepalive {
    Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 2_000_000_000,
        idle_timeout_ns: 10_000_000_000,
    })
}

/// Happy path with a HIP-4 outcome coin riding the ordinary surface:
/// all nine subscribe frames observed and byte-checked server-side
/// (`activeAssetCtx` gated off for `#330`), all nine acks verified
/// (`is_verified`), one `bbo` Tick per coin on the ring, in-band
/// Close → `Disconnected`.
#[test]
fn happy_path_hip4_coin_roundtrip() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> (Vec<Vec<u8>>, Vec<u8>) {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        // Exactly nine subscribe frames: BTC (perp) gets all four
        // per-coin channels; #330 (HIP-4 outcome) gets three — no
        // activeAssetCtx; plus allMids + outcomeMetaUpdates.
        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_HIP4);

        // One subscriptionResponse echo per subscription, then one
        // bbo push per coin (outcome prices live in [0, 1]).
        for sub in &subscribes {
            write_sub_ack(&mut stream, sub);
        }
        let bbo_btc = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
        stream
            .write_all(&build_unmasked_text_frame(bbo_btc))
            .expect("write BTC bbo");
        let bbo_hip4 = br##"{"channel":"bbo","data":{"coin":"#330","time":1723600000001,"bbo":[{"px":"0.4","sz":"100.0","n":1},{"px":"0.6","sz":"50.0","n":1}]}}"##;
        stream
            .write_all(&build_unmasked_text_frame(bbo_hip4))
            .expect("write #330 bbo");
        stream.flush().expect("flush stream");

        // Deterministic verification sync (file doc): the JSON
        // keepalive ping fires only ping_interval after the last
        // inbound frame was drained, and session_health verified the
        // ack mask at the end of that drain's iteration — holding
        // the ping proves `is_verified()` without sleeping.
        let ping = read_client_text_frame(&mut stream);

        stream.write_all(&[0x88, 0x00]).expect("write close");
        stream.flush().expect("flush stream");

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (subscribes, ping)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0x8D01, coins_btc_and_hip4(), GENEROUS_NS, GENEROUS_NS);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // Sync ping due 400 ms after the last inbound frame — far past
    // the ack/push burst even on a loaded box; idle budget bounds a
    // wedged session.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 400_000_000,
        idle_timeout_ns: 10_000_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let (subscribes, ping) = server.join().expect("server thread");

    assert_eq!(res, RunResult::Disconnected);
    // Payloads were byte-checked server-side; re-assert the HIP-4
    // gating invariant here where a failure reads best.
    assert_eq!(subscribes.len(), 9);
    assert!(
        !subscribes
            .iter()
            .any(|f| contains(f, b"activeAssetCtx") && contains(f, b"#330")),
        "outcome coins must not subscribe activeAssetCtx"
    );
    // The sync probe is the venue-specific JSON ping, verbatim.
    assert_eq!(ping, PING_PAYLOAD);
    // Every expected ack was found — the mask verified and the
    // staleness monitor armed (generous budget: no trip).
    assert!(driver.is_verified());
    assert_eq!(driver.sub_count(), 9);
    // Phase-8a observability through the real TLS path: the upgrade
    // published Up (D7); 9 acks + 2 bbo pushes counted (D5).
    assert_eq!(status.state(), IngressState::Up);
    assert!(status.last_activity_ns() > 0);
    assert_eq!(status.msgs_total(), 11);
    assert!(status.bytes_total() > 0);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(status.gaps_total(), 0);
    // FIFO ring: BTC bbo first, then the HIP-4 coin.
    let tick = cons.try_pop().expect("BTC tick must be on the ring");
    assert_eq!(tick.venue, VenueId::Hyperliquid as u8);
    assert_eq!(tick.sym, SYM_BTC);
    // venue_seq = time (ms) truncated to u32 (crate-header policy).
    assert_eq!(tick.venue_seq, 1_708_622_398_623u64 as u32);
    // Prices ×1e6 — 64437.0 → 64_437_000_000; 64438.0 → 64_438_000_000.
    assert_eq!(tick.bid_px.raw(), 64_437_000_000);
    assert_eq!(tick.ask_px.raw(), 64_438_000_000);
    // Sizes are base-coin units ×1e6.
    assert_eq!(tick.bid_qty.raw(), 1_449_100);
    assert_eq!(tick.ask_qty.raw(), 541_000);
    let tick = cons.try_pop().expect("HIP-4 tick must flow the same path");
    assert_eq!(tick.venue, VenueId::Hyperliquid as u8);
    assert_eq!(tick.sym, SYM_HIP4);
    assert_eq!(tick.venue_seq, 1_723_600_000_001u64 as u32);
    // Outcome prices in [0, 1] collateral units ×1e6.
    assert_eq!(tick.bid_px.raw(), 400_000);
    assert_eq!(tick.ask_px.raw(), 600_000);
    assert!(cons.try_pop().is_none(), "exactly two ticks were pushed");
}

/// §6.2 integrity: stateless snapshots have no chain — the only
/// signal is the venue clock advancing per coin. One `l2Book`
/// snapshot then silence must trip the tiny staleness budget:
/// `gaps_total` increments and the session ends `Stale`.
#[test]
fn staleness_trips_reconnect() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> Vec<Vec<u8>> {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        // BTC alone: four per-coin frames + two globals = six.
        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_ONLY);

        // All six acks (the mask verifies and the staleness monitor
        // arms), one l2Book snapshot, then silence with the socket
        // held open: the venue time never advances again and the
        // per-coin budget must trip client-side.
        for sub in &subscribes {
            write_sub_ack(&mut stream, sub);
        }
        let l2book = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"64437.0","sz":"1.0","n":1}],[{"px":"64438.0","sz":"1.0","n":1}]]}}"#;
        stream
            .write_all(&build_unmasked_text_frame(l2book))
            .expect("write l2Book snapshot");
        stream.flush().expect("flush stream");

        // Silence — the wait loop keeps the socket open (no FIN, no
        // frames) while the client trips Stale on its own clock.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        subscribes
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    // Tiny staleness budget (50 ms), generous ack budget: the only
    // deadline in play is the §6.2 per-coin one.
    let mut driver = Driver::new(0x57A1, coins_btc_only(), 50_000_000, GENEROUS_NS);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    let mut keepalive = generous_keepalive();

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let subscribes = server.join().expect("server thread");

    // Staleness, not a disconnect and not idle: the socket stayed
    // open and the generous keepalive never fired.
    assert_eq!(res, RunResult::Stale);
    assert_eq!(subscribes.len(), 6);
    // §6.2 signature: staleness counts into gaps_total (no dedicated
    // stale counter) paired with the Stale reconnect result.
    assert_eq!(status.gaps_total(), 1);
    // The session verified before it went stale.
    assert!(driver.is_verified());
    assert_eq!(driver.sub_count(), 6);
    // Six acks + one l2Book snapshot; nothing rejected, no ticks
    // (l2Book feeds the monitor, not the Tick lane).
    assert_eq!(status.msgs_total(), 7);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert!(cons.try_pop().is_none());
}

/// Keepalive: after the acks the server goes silent; the client must
/// probe with the literal `{"method":"ping"}` text frame and, with
/// the pong never coming, give up on the idle budget.
#[test]
fn ping_emitted_then_idle_timeout() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> Vec<u8> {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_ONLY);

        // Ack everything up front so the ack deadline can never fire
        // — the idle budget is the only deadline in play.
        for sub in &subscribes {
            write_sub_ack(&mut stream, sub);
        }
        stream.flush().expect("flush stream");

        // Send nothing further: the client must probe with the
        // venue-specific JSON ping, then give up on the idle budget.
        let ping = read_client_text_frame(&mut stream);

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        ping
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0x1D1E, coins_btc_only(), GENEROUS_NS, GENEROUS_NS);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, _cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // Ping due after 50 ms of silence; session dead by policy at 400 ms.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 50_000_000,
        idle_timeout_ns: 400_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let ping = server.join().expect("server thread");

    assert_eq!(res, RunResult::IdleTimeout);
    // The probe is the venue-specific JSON ping text frame, verbatim
    // — not a WS Ping control frame.
    assert_eq!(ping, PING_PAYLOAD);
    // The acks landed long before the idle budget: the mask verified.
    assert!(driver.is_verified());
    assert_eq!(status.msgs_total(), 6);
    assert_eq!(status.parse_errors_total(), 0);
}

/// Fail-fast on missed acks: the server swallows every subscribe and
/// never echoes; a tiny ack budget must end the session with
/// `RunResult::Error` (module doc: a venue timing condition, not a
/// code invariant — no debug assert fires).
#[test]
fn missed_acks_fail_session() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> Vec<Vec<u8>> {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        // Accept the subscribes, never ack, keep the socket open:
        // the client's ack deadline does the rest.
        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_ONLY);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        subscribes
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    // Tiny ack budget (50 ms), generous staleness: the only deadline
    // in play is the subscribe-ack one.
    let mut driver = Driver::new(0xACC5, coins_btc_only(), GENEROUS_NS, 50_000_000);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, _cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    let mut keepalive = generous_keepalive();

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let subscribes = server.join().expect("server thread");

    // Unverified past the ack budget fails the session (fail-fast).
    assert_eq!(res, RunResult::Error);
    assert_eq!(subscribes.len(), 6);
    assert!(!driver.is_verified());
    assert_eq!(driver.sub_count(), 0);
    assert_eq!(status.msgs_total(), 0);
    assert_eq!(status.parse_errors_total(), 0);
}

// ---------------------------------------------------------------
// BIN15 O2 — the rolling-instrument family
// ---------------------------------------------------------------

/// A capture that keeps the `ChannelEvent`s it is handed. The roll's
/// whole purpose is an OFFLINE record, so the only honest way to test
/// it is to read what capture received.
#[derive(Default)]
struct EventRecorder {
    events: Vec<core_types::ChannelEvent>,
}

impl core_types::Capture for EventRecorder {
    fn event(&mut self, e: &core_types::ChannelEvent) {
        self.events.push(*e);
    }
}

/// The live description of outcome 2649 (venue-probed 2026-09-12) and
/// the same grammar one roll later.
const DESC_2649: &[u8] =
    b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630";
const DESC_2650: &[u8] =
    b"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77201|time:20260912-0645";

/// The LIVE lifecycle frames: an array of kind-keyed objects, no
/// `coin`, no top-level `time`.
fn created_frame(outcome: u32, desc: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(br#"{"channel":"outcomeMetaUpdates","data":[{"outcomeCreated":{"outcome":"#);
    v.extend_from_slice(outcome.to_string().as_bytes());
    v.extend_from_slice(br#","name":"template:binaryPrice","description":""#);
    v.extend_from_slice(desc);
    v.extend_from_slice(
        br#"","sideSpecs":[{"name":"template:Yes"},{"name":"template:No"}],"quoteToken":"USDC","venue":"out","deployerFeeScale":"1.0"}}]}"#,
    );
    v
}

fn settled_frame(outcome: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(br#"{"channel":"outcomeMetaUpdates","data":[{"outcomeSettled":"#);
    v.extend_from_slice(outcome.to_string().as_bytes());
    v.extend_from_slice(b"}]}");
    v
}

/// BTC plus one `out:BTC:15m` family bound to outcome 2649 — the
/// shape a boot leaves behind after `bind_live`.
/// 2649's expiry, as the grammar parses it: 2026-09-12T06:30:00Z.
const EXPIRY_2649_NS: u64 = 1_789_194_600_000_000_000;

/// An anchor that maps the CURRENT monotonic instant to one minute
/// before 2649 expires — so the scripted lifecycle frames land inside
/// the family's window deterministically, whatever the wall clock of
/// the machine running the test says.
fn wall_anchor_one_minute_before_expiry() -> core_time::WallAnchor {
    core_time::WallAnchor::new(
        core_time::now_ns(),
        EXPIRY_2649_NS - 60_000_000_000,
    )
}

fn coins_and_family_bound_to_2649() -> (HlCoinTable, ingress_hyperliquid::family::HlFamilyTable) {
    use ingress_hyperliquid::discovery::parse_outcome_spec;
    use ingress_hyperliquid::family::{rolling_sym, HlFamilyTable};
    let mut coins = HlCoinTable::new();
    coins.insert(b"BTC", SYM_BTC).expect("BTC");
    let yes = coins.reserve(rolling_sym(0, 0)).expect("reserve yes");
    let no = coins.reserve(rolling_sym(0, 1)).expect("reserve no");
    let mut families = HlFamilyTable::new();
    let (kind, und, period) = HlFamilyTable::parse_key(b"out:BTC:15m").expect("key");
    families
        .push(
            kind,
            und,
            period,
            [yes as u8, no as u8],
            [rolling_sym(0, 0), rolling_sym(0, 1)],
        )
        .expect("push");
    let spec = parse_outcome_spec(2649, DESC_2649);
    assert_eq!(spec.expiry_ns, EXPIRY_2649_NS);
    // One minute before 2649 expires — inside its window.
    assert_eq!(
        families.bind_live(&[spec], EXPIRY_2649_NS - 60_000_000_000, &mut coins),
        1
    );
    (coins, families)
}

/// BTC (4) + the family's two bound coins (3 each) + the two globals.
const EXP_SUBS_BTC_FAMILY: [&[u8]; 12] = [
    br#"{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}"#,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26491"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26491"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26491"}}"##,
    br#"{"method":"subscribe","subscription":{"type":"allMids"}}"#,
    br#"{"method":"subscribe","subscription":{"type":"outcomeMetaUpdates"}}"#,
];

/// The roll's own twelve client frames, in order: the outgoing
/// instance's six unsubscribes, then the incoming one's six
/// subscribes.
const EXP_ROLL_FRAMES: [&[u8]; 12] = [
    br##"{"method":"unsubscribe","subscription":{"type":"bbo","coin":"#26490"}}"##,
    br##"{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"#26490"}}"##,
    br##"{"method":"unsubscribe","subscription":{"type":"trades","coin":"#26490"}}"##,
    br##"{"method":"unsubscribe","subscription":{"type":"bbo","coin":"#26491"}}"##,
    br##"{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"#26491"}}"##,
    br##"{"method":"unsubscribe","subscription":{"type":"trades","coin":"#26491"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26500"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26500"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26500"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26501"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26501"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26501"}}"##,
];

/// A family's instance settles and its successor is created, mid
/// session: the outgoing coins are unsubscribed, the incoming ones
/// bound and subscribed, both transitions recorded as
/// `InstrumentRoll` events, and a push for the NEW coin resolves to
/// the family's stable slot sym.
#[test]
fn a_roll_rebinds_unsubscribes_subscribes_and_emits_the_event() {
    use ingress_hyperliquid::family::{rolling_sym, unpack_roll_seq};

    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);
        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_FAMILY);
        for sub in &subscribes {
            write_sub_ack(&mut stream, sub);
        }
        stream.flush().expect("flush acks");

        // The venue's own order at a quarter-hour: the old instance
        // settles, then the new one is created in the same burst.
        stream
            .write_all(&build_unmasked_text_frame(&settled_frame(2649)))
            .expect("write settled");
        stream
            .write_all(&build_unmasked_text_frame(&created_frame(2650, DESC_2650)))
            .expect("write created");
        stream.flush().expect("flush lifecycle");

        // The roll's twelve frames come back on the wire.
        let roll_frames = read_and_check_subscribes(&mut stream, &EXP_ROLL_FRAMES);

        // A push for the INCOMING coin must reach the family's slot.
        let bbo_new = br##"{"channel":"bbo","data":{"coin":"#26500","time":1789195000000,"bbo":[{"px":"0.41","sz":"120.0","n":2},{"px":"0.44","sz":"80.0","n":1}]}}"##;
        stream
            .write_all(&build_unmasked_text_frame(bbo_new))
            .expect("write new bbo");
        stream.flush().expect("flush bbo");

        // The roll frames above are this test's verification sync:
        // the client can only have sent them after draining the
        // lifecycle pushes. No ping probe — a keepalive firing between
        // the acks and the lifecycle burst would land in the middle of
        // the frames under test.
        stream.write_all(&[0x88, 0x00]).expect("write close");
        stream.flush().expect("flush close");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (subscribes, roll_frames)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let (coins, families) = coins_and_family_bound_to_2649();
    let roll_status = Arc::new(ingress_hyperliquid::family::HlRollStatus::new());
    let mut driver = Driver::new(0x8D02, coins, GENEROUS_NS, GENEROUS_NS);
    driver.set_families(
        families,
        roll_status.clone(),
        wall_anchor_one_minute_before_expiry(),
    );

    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    let mut recorder = EventRecorder::default();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // No ping inside the scripted window (the roll frames are the
    // sync); the idle budget still bounds a wedged session.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 30_000_000_000,
        idle_timeout_ns: 60_000_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut recorder,
    );
    client_done.store(true, Ordering::Release);
    let (subscribes, roll_frames) = server.join().expect("server thread");

    assert_eq!(res, RunResult::Disconnected);
    // The boot binding put the family's two coins on the wire beside
    // the configured perp — 12 subscriptions, not 6.
    assert_eq!(subscribes.len(), 12);
    assert!(driver.is_verified(), "a bound family verifies like any coin");
    // The roll: six unsubscribes for the outgoing instance, then six
    // subscribes for the incoming one, in that order (byte-checked
    // server-side; re-asserted here where a failure reads best).
    assert_eq!(roll_frames.len(), 12);
    assert!(
        roll_frames[..6]
            .iter()
            .all(|f| contains(f, b"unsubscribe")),
        "the outgoing instance is dropped first"
    );
    assert!(
        roll_frames[6..]
            .iter()
            .all(|f| !contains(f, b"unsubscribe")),
        "then the incoming one is subscribed"
    );

    // Three InstrumentRoll events with the exact payload packing: the
    // BOOT announcement of the instance `bind_live` bound (E7 R0,
    // 2026-09-19 — a created roll for 2649 at the first Steady, so the
    // member and the exec arm learn the slot without waiting for the
    // venue's next push), then the venue's own settled 2649 and
    // created 2650.
    let rolls: Vec<_> = recorder
        .events
        .iter()
        .filter(|e| e.channel == core_types::ChannelId::InstrumentRoll as u8)
        .collect();
    assert_eq!(rolls.len(), 3, "boot-created, then settled, then created");

    let (id, twap, fam, settled) = unpack_roll_seq(rolls[0].venue_seq);
    assert_eq!((id, twap, fam, settled), (2649, 60, 0, false), "the boot announcement");
    assert_eq!(rolls[0].sym, rolling_sym(0, 0), "the Yes slot names the family");
    assert_eq!(rolls[0].v0, 77_177_000_000, "strike x1e6");
    assert_eq!(rolls[0].v1, 1_789_194_600_000_000_000, "expiry ns");

    let (id, twap, fam, settled) = unpack_roll_seq(rolls[1].venue_seq);
    assert_eq!((id, twap, fam, settled), (2649, 60, 0, true));
    assert_eq!(rolls[1].sym, rolling_sym(0, 0), "the Yes slot names the family");
    assert_eq!(rolls[1].v0, 77_177_000_000, "strike x1e6");
    assert_eq!(rolls[1].v1, 1_789_194_600_000_000_000, "expiry ns");
    assert_eq!(rolls[1].venue_time_ms, 0, "the venue push carries no time");

    let (id, twap, fam, settled) = unpack_roll_seq(rolls[2].venue_seq);
    assert_eq!((id, twap, fam, settled), (2650, 60, 0, false));
    assert_eq!(rolls[2].sym, rolling_sym(0, 0));
    assert_eq!(rolls[2].v0, 77_201_000_000);
    // One 15-minute period later than 2649.
    assert_eq!(rolls[2].v1 - rolls[1].v1, 900 * 1_000_000_000);

    // The push for the NEW coin arrived on the family's stable slot.
    let tick = cons.try_pop().expect("a tick for the rolled slot");
    assert_eq!(tick.sym, rolling_sym(0, 0));
    assert_eq!(tick.bid_px.raw(), 410_000);
    assert_eq!(tick.ask_px.raw(), 440_000);

    // Counters: one roll, nothing ignored (every push matched), no
    // ack timeout (the budget is generous), nothing dormant.
    assert_eq!(roll_status.rolls_total(), 1);
    assert_eq!(roll_status.rolls_ignored_unmatched(), 0);
    assert_eq!(roll_status.family_ack_timeouts(), 0);
    assert_eq!(roll_status.families_dormant(), 0);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.gaps_total(), 0);
}

/// A family with no live instance subscribes NOTHING — its reserved
/// rows are invisible to the subscribe sweep and to the ack mask, so
/// the session verifies on the configured coins alone. And a repeated
/// `outcomeCreated` for the instance already held is idempotent: the
/// venue re-pushes on reconnect, and a second rebind would churn the
/// wire for no change.
#[test]
fn a_dormant_family_subscribes_nothing_and_repeats_are_idempotent() {
    use ingress_hyperliquid::family::{rolling_sym, HlFamilyTable};

    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> Vec<Vec<u8>> {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);
        // A dormant family adds nothing: BTC's four + the two globals.
        let subscribes = read_and_check_subscribes(&mut stream, &EXP_SUBS_BTC_ONLY);
        for sub in &subscribes {
            write_sub_ack(&mut stream, sub);
        }
        // A created push for a family we do not configure (SOL), then
        // one for BTC — the first is ignored, the second rolls.
        stream
            .write_all(&build_unmasked_text_frame(&created_frame(
                9001,
                b"perp:SOL|priceDescription:x|seconds:60|threshold:200|time:20260912-0630",
            )))
            .expect("write foreign");
        stream
            .write_all(&build_unmasked_text_frame(&created_frame(2649, DESC_2649)))
            .expect("write created");
        // And the SAME push again: idempotent, no second roll.
        stream
            .write_all(&build_unmasked_text_frame(&created_frame(2649, DESC_2649)))
            .expect("write repeat");
        stream.flush().expect("flush");

        let after = read_and_check_subscribes(&mut stream, &EXP_ROLL_SUBS_2649);

        // The roll frames above are this test's verification sync:
        // the client can only have sent them after draining the
        // lifecycle pushes. No ping probe — a keepalive firing between
        // the acks and the lifecycle burst would land in the middle of
        // the frames under test.
        stream.write_all(&[0x88, 0x00]).expect("write close");
        stream.flush().expect("flush close");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        after
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    // Reserved but UNBOUND — the dormant boot.
    let mut coins = HlCoinTable::new();
    coins.insert(b"BTC", SYM_BTC).expect("BTC");
    let yes = coins.reserve(rolling_sym(0, 0)).expect("yes");
    let no = coins.reserve(rolling_sym(0, 1)).expect("no");
    let mut families = HlFamilyTable::new();
    let (kind, und, period) = HlFamilyTable::parse_key(b"out:BTC:15m").expect("key");
    families
        .push(
            kind,
            und,
            period,
            [yes as u8, no as u8],
            [rolling_sym(0, 0), rolling_sym(0, 1)],
        )
        .expect("push");
    assert_eq!(families.dormant_count(), 1);

    let roll_status = Arc::new(ingress_hyperliquid::family::HlRollStatus::new());
    let mut driver = Driver::new(0x8D03, coins, GENEROUS_NS, GENEROUS_NS);
    driver.set_families(
        families,
        roll_status.clone(),
        wall_anchor_one_minute_before_expiry(),
    );
    assert_eq!(roll_status.families_dormant(), 1, "published at attach");

    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, _cons) = ring.split();
    let mut recorder = EventRecorder::default();
    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // No ping inside the scripted window (the roll frames are the
    // sync); the idle budget still bounds a wedged session.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 30_000_000_000,
        idle_timeout_ns: 60_000_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut recorder,
    );
    client_done.store(true, Ordering::Release);
    let after = server.join().expect("server thread");

    assert_eq!(res, RunResult::Disconnected);
    // The dormant family cost nothing at boot, and verification did
    // not wait on it.
    assert!(driver.is_verified());
    // Exactly ONE roll's worth of subscribes — six, not twelve: the
    // first push was another deployer's and the third was a repeat.
    assert_eq!(after.len(), 6);
    assert_eq!(roll_status.rolls_total(), 1, "one roll, not two");
    assert_eq!(roll_status.rolls_ignored_unmatched(), 1, "the SOL market");
    assert_eq!(roll_status.families_dormant(), 0, "bound by the roll");
    let rolls = recorder
        .events
        .iter()
        .filter(|e| e.channel == core_types::ChannelId::InstrumentRoll as u8)
        .count();
    assert_eq!(rolls, 1);
    assert_eq!(status.parse_errors_total(), 0);
}

/// The incoming instance's six subscribes, for the dormant-boot case.
const EXP_ROLL_SUBS_2649: [&[u8]; 6] = [
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26490"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"bbo","coin":"#26491"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"l2Book","coin":"#26491"}}"##,
    br##"{"method":"subscribe","subscription":{"type":"trades","coin":"#26491"}}"##,
];
