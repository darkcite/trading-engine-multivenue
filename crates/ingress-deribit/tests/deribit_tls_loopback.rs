// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: Deribit JSON-RPC 2.0 public WS ingress against a
//! real `127.0.0.1` TLS server with a self-signed cert. Three scripted
//! sessions per PLAN §11 ("every ingress" row): the happy path
//! (`public/set_heartbeat` then ONE batched `public/subscribe` →
//! subscribe result echo + one `quote` push → Tick on the ring →
//! `test_request` answered with `public/test` — the 8c exit criterion
//! — → StopFlag shutdown), a `book` `change_id` chain break (gap +
//! unsubscribe→subscribe resync observed server-side), and the
//! keepalive idle timeout (proactive `public/test` probe, then
//! `RunResult::IdleTimeout`).
//!
//! The client side is the top-level [`run`] loop over the real
//! `TlsTransport`. The happy path ends via the shared `StopFlag`
//! (raised by the server thread once it holds the `public/test`
//! answer); the gap session ends with an in-band WS Close frame — a
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
use ingress_deribit::run_loop::{run, Driver, RunResult, StopFlag, TICK_RING_CAP};
use ingress_deribit::DeribitSymbolTable;

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// Venue-namespaced test symbol (venue byte 3 = Deribit, ordinal 1).
const SYM_BTC: SymbolId = (3 << 24) | 1;

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

fn test_symbols() -> DeribitSymbolTable {
    let mut t = DeribitSymbolTable::new();
    t.insert(b"BTC-PERPETUAL", SYM_BTC).expect("insert symbol");
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
fn deribit_tls_loopback_yields_tick_and_answers_test_request() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    // Shared StopFlag: the server thread raises it once it holds the
    // client's `public/test` answer — run() then returns Stopped.
    let stop = Arc::new(StopFlag::new(false));
    let stop_server = stop.clone();
    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        serve_ws_upgrade(&mut stream);

        // Two masked client frames, in order: set_heartbeat first
        // (the venue polices from that moment), then ONE batched
        // subscribe covering every configured channel.
        let set_heartbeat = read_client_text_frame(&mut stream);
        let subscribe = read_client_text_frame(&mut stream);

        // Results for both calls (the subscribe result must echo the
        // full channel list or the client fails the session), one
        // quote push, then the venue heartbeat test_request.
        let hb_result = br#"{"jsonrpc":"2.0","id":1,"result":"ok","testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(hb_result))
            .expect("write hb result");
        let sub_result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms"],"testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(sub_result))
            .expect("write subscribe result");
        let push = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
        stream
            .write_all(&build_unmasked_text_frame(push))
            .expect("write quote push");
        let test_req =
            br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
        stream
            .write_all(&build_unmasked_text_frame(test_req))
            .expect("write test_request");
        stream.flush().expect("flush stream");

        // 8c exit criterion, observed server-side: the test_request
        // must be answered with a masked `public/test` frame.
        let test_answer = read_client_text_frame(&mut stream);

        // Clean shutdown: raise the stop flag; run() exits Stopped.
        stop_server.store(true, Ordering::Release);

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (set_heartbeat, subscribe, test_answer)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0xD3B1, test_symbols(), false);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let mut keepalive = generous_keepalive();

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws/api/v2",
        &mut prod,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let (set_heartbeat, subscribe, test_answer) = server.join().expect("server thread");

    assert_eq!(res, RunResult::Stopped);
    // Frame 1: set_heartbeat armed before anything else (id 1).
    assert!(contains(
        &set_heartbeat,
        br#""method":"public/set_heartbeat""#
    ));
    assert!(contains(&set_heartbeat, br#""interval":15"#));
    assert!(!contains(&set_heartbeat, br#""method":"public/subscribe""#));
    // Frame 2: ONE batched subscribe with every configured channel.
    assert!(contains(&subscribe, br#""method":"public/subscribe""#));
    assert!(contains(&subscribe, br#""quote.BTC-PERPETUAL""#));
    assert!(contains(&subscribe, br#""ticker.BTC-PERPETUAL.100ms""#));
    assert!(contains(&subscribe, br#""trades.BTC-PERPETUAL.100ms""#));
    // Heartbeat proof: the answer is public/test and the JSON-RPC id
    // kept incrementing (set_heartbeat 1, subscribe 2, test 3).
    assert!(contains(&test_answer, br#""id":3,"method":"public/test""#));
    // Phase-8a observability through the real TLS path: the upgrade
    // published Up (D7); both results + push + test_request counted (D5).
    assert_eq!(status.state(), IngressState::Up);
    assert!(status.last_activity_ns() > 0);
    assert_eq!(status.msgs_total(), 4);
    assert!(status.bytes_total() > 0);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(status.gaps_total(), 0);
    assert_eq!(status.resubscribes_total(), 0);
    let tick = cons.try_pop().expect("tick must be on the ring");
    assert_eq!(tick.venue, VenueId::Deribit as u8);
    assert_eq!(tick.sym, SYM_BTC);
    // No seq on quotes: venue ms timestamp truncated to u32 (8c).
    assert_eq!(tick.venue_seq, 1_550_658_624_149u64 as u32);
    // Prices ×1e6 — 3914.97 → 3_914_970_000; 3996.61 → 3_996_610_000.
    assert_eq!(tick.bid_px.raw(), 3_914_970_000);
    assert_eq!(tick.ask_px.raw(), 3_996_610_000);
    // Amounts are USD notional ×1e6.
    assert_eq!(tick.bid_qty.raw(), 40_000_000);
    assert_eq!(tick.ask_qty.raw(), 50_000_000);
}

#[test]
fn deribit_tls_loopback_book_gap_triggers_resubscribe() {
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

        let _set_heartbeat = read_client_text_frame(&mut stream);
        let subscribe = read_client_text_frame(&mut stream);

        // Results (the subscribe result echoes all FOUR channels —
        // depth enabled), then a book snapshot rooting the chain at
        // change_id 10 and a change whose prev_change_id 99 breaks it.
        let hb_result = br#"{"jsonrpc":"2.0","id":1,"result":"ok","testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(hb_result))
            .expect("write hb result");
        let sub_result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","book.BTC-PERPETUAL.100ms"],"testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(sub_result))
            .expect("write subscribe result");
        let snap = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1000,"instrument_name":"BTC-PERPETUAL","change_id":10,"bids":[["new",1.0,1.0]],"asks":[],"type":"snapshot"}}}"#;
        stream
            .write_all(&build_unmasked_text_frame(snap))
            .expect("write snapshot");
        let broken = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":2000,"instrument_name":"BTC-PERPETUAL","change_id":100,"prev_change_id":99,"bids":[],"asks":[["delete",1.0,0.0]],"type":"change"}}}"#;
        stream
            .write_all(&build_unmasked_text_frame(broken))
            .expect("write broken change");
        stream.flush().expect("flush stream");

        // §6.2 resync: the client answers the break with a
        // `public/unsubscribe` then a fresh `public/subscribe` for
        // the one book channel (official venue guidance).
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

    // depth_enabled: book.*.100ms rides in the subscribe batch.
    let mut driver = Driver::new(0xB00C, test_symbols(), true);
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
        b"/ws/api/v2",
        &mut prod,
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
    assert!(contains(&subscribe, br#""method":"public/subscribe""#));
    assert!(contains(&subscribe, br#""quote.BTC-PERPETUAL""#));
    assert!(contains(&subscribe, br#""ticker.BTC-PERPETUAL.100ms""#));
    assert!(contains(&subscribe, br#""trades.BTC-PERPETUAL.100ms""#));
    assert!(contains(&subscribe, br#""book.BTC-PERPETUAL.100ms""#));
    // One chain break: one gap, one resubscribe cycle (§6.2).
    assert_eq!(status.gaps_total(), 1);
    assert_eq!(status.resubscribes_total(), 1);
    assert!(contains(&unsub, br#""method":"public/unsubscribe""#));
    assert!(contains(&unsub, br#""book.BTC-PERPETUAL.100ms""#));
    assert!(contains(&resub, br#""method":"public/subscribe""#));
    assert!(contains(&resub, br#""book.BTC-PERPETUAL.100ms""#));
    // Both results + snapshot + change all counted; nothing rejected.
    assert_eq!(status.msgs_total(), 4);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
}

#[test]
fn deribit_tls_loopback_idle_timeout_sends_public_test_probe() {
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

        let _set_heartbeat = read_client_text_frame(&mut stream);
        let subscribe = read_client_text_frame(&mut stream);

        // Answer both calls, then go silent: the client must probe
        // with a proactive `public/test` (Deribit has no WS-level
        // ping), then give up on the idle budget.
        let hb_result = br#"{"jsonrpc":"2.0","id":1,"result":"ok","testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(hb_result))
            .expect("write hb result");
        let sub_result = br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms"],"testnet":false}"#;
        stream
            .write_all(&build_unmasked_text_frame(sub_result))
            .expect("write subscribe result");
        stream.flush().expect("flush stream");

        let probe = read_client_text_frame(&mut stream);

        // Signal-driven sync — see binance_tls_loopback for shape.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
        (subscribe, probe)
    });

    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0x1D1E, test_symbols(), false);
    let status = IngressStatus::new();
    let ring: Arc<Ring<Tick, TICK_RING_CAP>> = Ring::new();
    let (mut prod, _cons) = ring.split();

    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    let stop = StopFlag::new(false);
    // Probe due after 50 ms of silence; session dead by policy at 300 ms.
    let mut keepalive = Keepalive::new(KeepaliveCfg {
        ping_interval_ns: 50_000_000,
        idle_timeout_ns: 300_000_000,
    });

    let res = run(
        &mut transport,
        &mut driver,
        b"localhost",
        b"/ws/api/v2",
        &mut prod,
        &mut poll,
        &mut events,
        token,
        &stop,
        &status,
        &mut keepalive,
        &mut NullCapture,
    );
    client_done.store(true, Ordering::Release);
    let (subscribe, probe) = server.join().expect("server thread");

    assert_eq!(res, RunResult::IdleTimeout);
    // The probe followed the subscribe batch and is a JSON-RPC
    // `public/test` call — Deribit's venue-specific keepalive, not a
    // WS Ping frame.
    assert!(contains(&subscribe, br#""method":"public/subscribe""#));
    assert!(contains(&probe, br#""method":"public/test""#));
}
