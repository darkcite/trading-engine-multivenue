// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: OKX v5 public WS ingress against a real
//! `127.0.0.1` TLS server with a self-signed cert. Three scripted
//! sessions per PLAN §11 ("every ingress" row): the happy path
//! (batched subscribe → ack + one `bbo-tbt` push → Tick on the ring →
//! server close), a `books` `seqId` chain break (gap + resubscribe
//! observed server-side), and the keepalive idle timeout (literal
//! `ping` text frame, then `RunResult::IdleTimeout`).
//!
//! The client side is the top-level [`run`] loop over the real
//! `TlsTransport`. Sessions end with an in-band WS Close frame — a
//! raced TCP FIN could mark the transport closed before the scripted
//! frames are drained; an in-band Close is always processed after
//! them.

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
use ingress_okx::run_loop::{run, Driver, RunResult, StopFlag, TICK_RING_CAP};

/// WS10-A: a throwaway venue-event lane per `run` call (consumer
/// dropped — pushes vanish; the loopback asserts the tick path).
fn event_lane() -> core_ring::Producer<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }> {
    Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new()
        .split()
        .0
}

/// WS10-B: a throwaway depth lane per `run` call (consumer dropped).
fn depth_lane() -> core_ring::Producer<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }> {
    Ring::<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>::new()
        .split()
        .0
}

use ingress_okx::{OkxInstType, OkxSymbolTable};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// Venue-namespaced test symbol (venue byte 2 = Okx, ordinal 1).
const SYM_BTC: SymbolId = (2 << 24) | 1;

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
/// inline handshake block — factored because three sessions share it.
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

fn test_symbols() -> OkxSymbolTable {
    let mut t = OkxSymbolTable::new();
    t.insert(b"BTC-USDT", SYM_BTC, OkxInstType::Spot)
        .expect("insert symbol");
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

#[test]
fn okx_tls_loopback_yields_expected_tick() {
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

        // The client's one batched subscribe op (masked text frame).
        let subscribe = read_client_text_frame(&mut stream);

        // Subscribe ack, one bbo-tbt push, then a server-side WS
        // Close frame — the in-band Close (not a raced FIN) is what
        // ends the session, so the push is always drained first.
        let ack = br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT"}}"#;
        stream
            .write_all(&build_unmasked_text_frame(ack))
            .expect("write ack");
        let push = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
        stream
            .write_all(&build_unmasked_text_frame(push))
            .expect("write push");
        stream.write_all(&[0x88, 0x00]).expect("write close");
        stream.flush().expect("flush stream");

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        subscribe
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0x0C5E, test_symbols(), false, &[]);
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
        b"/ws/v5/public",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING,
        &mut depth_lane(),
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let subscribe = server.join().expect("server thread");

    assert_eq!(res, RunResult::Disconnected);
    // The batched subscribe op reached the server intact.
    assert!(contains(&subscribe, br#""op":"subscribe""#));
    assert!(contains(&subscribe, b"bbo-tbt"));
    // Phase-8a observability through the real TLS path: the upgrade
    // published Up (D7); ack + push were both counted (D5).
    assert_eq!(status.state(), IngressState::Up);
    assert!(status.last_activity_ns() > 0);
    assert!(status.msgs_total() >= 2);
    assert!(status.bytes_total() > 0);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    let tick = cons.try_pop().expect("tick must be on the ring");
    assert_eq!(tick.venue, VenueId::Okx as u8);
    assert_eq!(tick.sym, SYM_BTC);
    // Prices are scaled 1e6 — 111.05 → 111_050_000; 111.06 → 111_060_000.
    assert_eq!(tick.bid_px.raw(), 111_050_000);
    assert_eq!(tick.ask_px.raw(), 111_060_000);
}

#[test]
fn okx_tls_loopback_books_gap_triggers_resubscribe() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        let subscribe = read_client_text_frame(&mut stream);

        // Ack, then a books snapshot (chain init: prevSeqId -1, seqId
        // 10) and an update whose prevSeqId 99 breaks the chain.
        let ack = br#"{"event":"subscribe","arg":{"channel":"books","instId":"BTC-USDT"}}"#;
        stream
            .write_all(&build_unmasked_text_frame(ack))
            .expect("write ack");
        let snap = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["111.06","5","0","2"]],"bids":[["111.05","7","0","2"]],"ts":"1000","checksum":0,"prevSeqId":-1,"seqId":10}]}"#;
        stream
            .write_all(&build_unmasked_text_frame(snap))
            .expect("write snapshot");
        let broken = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"update","data":[{"asks":[["111.07","1","0","1"]],"bids":[],"ts":"2000","checksum":0,"prevSeqId":99,"seqId":100}]}"#;
        stream
            .write_all(&build_unmasked_text_frame(broken))
            .expect("write update");
        stream.flush().expect("flush stream");

        // §6.2 resync: the client answers the break with an
        // unsubscribe op then a fresh subscribe op for (books, inst).
        let unsub = read_client_text_frame(&mut stream);
        let resub = read_client_text_frame(&mut stream);

        stream.write_all(&[0x88, 0x00]).expect("write close");
        stream.flush().expect("flush stream");

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (subscribe, unsub, resub)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    // depth_enabled: books rides in the subscribe batch.
    let mut driver = Driver::new(0xB00C, test_symbols(), true, &[]);
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
        b"/ws/v5/public",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING,
        &mut depth_lane(),
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let (subscribe, unsub, resub) = server.join().expect("server thread");

    assert_eq!(res, RunResult::Disconnected);
    assert!(contains(&subscribe, br#""op":"subscribe""#));
    assert!(contains(&subscribe, br#""channel":"books""#));
    // One chain break: one gap, one resubscribe cycle (§6.2).
    assert_eq!(status.gaps_total(), 1);
    assert_eq!(status.resubscribes_total(), 1);
    assert!(contains(&unsub, br#""op":"unsubscribe""#));
    assert!(contains(
        &unsub,
        br#"{"channel":"books","instId":"BTC-USDT"}"#
    ));
    assert!(contains(&resub, br#""op":"subscribe""#));
    assert!(contains(
        &resub,
        br#"{"channel":"books","instId":"BTC-USDT"}"#
    ));
    // Ack + snapshot + update all counted; nothing rejected.
    assert_eq!(status.msgs_total(), 3);
    assert_eq!(status.parse_errors_total(), 0);
}

#[test]
fn okx_tls_loopback_idle_timeout_sends_literal_ping() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> (Vec<u8>, Vec<u8>) {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        let subscribe = read_client_text_frame(&mut stream);

        // Send nothing further: the client must probe with the
        // literal `ping` text frame, then give up on the idle budget.
        let ping = read_client_text_frame(&mut stream);

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (subscribe, ping)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0x1D1E, test_symbols(), false, &[]);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, _cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // Ping due after 20 ms of silence; session dead by policy at 80 ms.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 20_000_000,
        idle_timeout_ns: 80_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws/v5/public",
        &mut prod,
        &mut event_lane(),
        core_types::EVENT_LANE_FUNDING,
        &mut depth_lane(),
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let (subscribe, ping) = server.join().expect("server thread");

    assert_eq!(res, RunResult::IdleTimeout);
    // The probe followed the subscribe batch and is the literal text
    // `ping` — OKX's venue-specific keepalive, not a WS Ping frame.
    assert!(contains(&subscribe, br#""op":"subscribe""#));
    assert_eq!(ping, b"ping");
}
