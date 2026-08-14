//! Integration test: Polymarket CLOB ingress against a real
//! `127.0.0.1` TLS server with a self-signed cert.
//!
//! Boots a `rustls::ServerConnection` on an ephemeral port, scripts
//! the RFC 6455 opening handshake reply plus one canned CLOB book
//! frame, drives `ingress_polymarket::run_loop` against the real
//! `TlsTransport`, and asserts one [`Tick`] popped from the ring
//! with the expected bid/ask prices.
//!
//! This closes the gap left open by Phase 1b §2.5 and hardens the
//! `TlsTransport` → run-loop boundary against any subtle mio
//! readiness or rustls plaintext-buffering issues that the in-memory
//! `TestTransport` can't catch.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_net::{expected_accept, TlsTransport};
use core_ring::Ring;
use core_types::{SymbolId, Tick};
use ingress_polymarket::run_loop::{
    drive_one, note_transport_ready, Driver, State, SymbolMap, DEFAULT_TICK_RING_CAP,
};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

const ASSET_ID: &[u8] = b"0xABC";
const ASSET_SYM: SymbolId = 42;

/// A minimal self-signed cert + key for the loopback server.
struct LoopbackCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> LoopbackCert {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .expect("private key DER");
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

/// Build a canned RFC 6455 101 reply for the given client key.
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

/// Build an *unmasked* WebSocket text frame (server-side framing).
fn build_unmasked_text_frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 125, "short-form only for this test");
    let mut out = Vec::with_capacity(payload.len() + 2);
    out.push(0x81); // FIN | Text
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
    out
}

/// Find `Sec-WebSocket-Key: <24-byte>` in the buffered client handshake.
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

#[test]
fn polymarket_tls_loopback_yields_expected_tick() {
    // 1. Generate cert + bind listener.
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    // 2. Server thread: accept, drive TLS handshake, drive HTTP +
    //    one frame, then wait on signal-driven sync (no
    //    thread::sleep) before shutting down.
    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_server = client_done.clone();
    let server = thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept");
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        // Read client handshake.
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

        // Reply with 101 + Sec-WebSocket-Accept derived from the key.
        let client_key = extract_client_key(&buf[..total]);
        let reply = build_101_reply(&client_key);
        stream.write_all(&reply).expect("write 101");

        // Send one CLOB book frame (unmasked, server → client).
        let body = br#"{"event_type":"book","asset_id":"0xABC","timestamp":"1713000000000","bids":[["0.518","100"]],"asks":[["0.520","50"]]}"#;
        let frame = build_unmasked_text_frame(body);
        stream.write_all(&frame).expect("write frame");
        stream.flush().expect("flush stream");

        // Wait on signal-driven sync or fall through on a 5 s
        // deadline. The client thread flips `client_done` once it
        // has popped the expected Tick.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client_done_server.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let _ = sock.shutdown(Shutdown::Both);
    });

    // 3. Client side: real TlsTransport against our self-signed
    //    server.
    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("TlsTransport::connect");

    let mut driver = Driver::new(0xDEADBEEF);
    let symbol_map =
        SymbolMap::from_pairs(std::iter::once((ASSET_ID.to_vec(), ASSET_SYM)));
    let ring: Arc<Ring<Tick, DEFAULT_TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Drive the run-loop via mio until we either see Steady + one
    // tick or hit the deadline.
    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let token = mio::Token(0);
    use core_net::Transport as _;
    transport.register(poll.registry(), token).expect("register");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got: Option<Tick> = None;
    while got.is_none() && Instant::now() < deadline {
        poll.poll(&mut events, Some(Duration::from_millis(50)))
            .expect("poll");
        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            let status = transport.pump(ev).expect("pump");
            note_transport_ready(&mut driver, status);
        }
        drive_one(
            &mut transport,
            &mut driver,
            b"localhost",
            b"/",
            &mut prod,
            &symbol_map,
        )
        .expect("drive_one");
        got = cons.try_pop();
        transport
            .reregister(poll.registry(), token)
            .expect("reregister");
    }
    client_done.store(true, Ordering::Release);
    server.join().expect("server thread");

    let tick = got.expect("tick must arrive within 5 s");
    assert_eq!(driver.state(), State::Steady);
    assert_eq!(tick.sym, ASSET_SYM);
    // Prices are scaled 1e6 — 0.518 → 518_000; 0.520 → 520_000.
    assert_eq!(tick.bid_px.raw(), 518_000);
    assert_eq!(tick.ask_px.raw(), 520_000);
}
