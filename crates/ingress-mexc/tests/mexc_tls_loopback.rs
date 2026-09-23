// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: the MEXC ingress against a real `127.0.0.1` TLS
//! server with a self-signed cert — the binance/okx loopback
//! scaffolding, but driven through [`run_multi`] with TWO real
//! `TlsTransport`s on one thread and one producer: a SPOT connection
//! (`/ws`) and a FUTURES connection (`/edge`). The server checks the
//! exact subscribe bytes each class sends, then answers each with one
//! push: a protobuf `aggre.bookTicker` in a BINARY frame (spot) and a
//! JSON `push.depth.full` in a TEXT frame (futures). Both must arrive
//! as ticks. No network beyond loopback.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_net::{expected_accept, TlsTransport};
use core_ring::Ring;
use core_types::{ChannelEvent, NullCapture, Tick, VenueId, EVENT_RING_SIZE};
use ingress_mexc::run_loop::{run_multi, Driver, MexcConn, RunResult, StopFlag, TICK_RING_CAP};
use ingress_mexc::{MexcClass, MexcSymbolTable};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

const SYM_SPOT: u32 = (7 << 24) | 1;
const SYM_PERP: u32 = (7 << 24) | 513;

const SPOT_SUB: &[u8] = br#"{"method":"SUBSCRIPTION","params":["spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT","spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT"]}"#;
const FUT_SUBS: [&[u8]; 3] = [
    br#"{"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}"#,
    br#"{"method":"sub.deal","param":{"symbol":"BTC_USDT"}}"#,
    br#"{"method":"sub.ticker","param":{"symbol":"BTC_USDT"}}"#,
];
const FUT_DEPTH: &[u8] = br#"{"symbol":"BTC_USDT","data":{"cts":1789897581009,"asks":[[80468.7,3446,2]],"bids":[[80468.6,31288,7]],"version":41925002140},"channel":"push.depth.full","ts":1789897581013}"#;

// ---- a minimal PB encoder (test-only) ------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn len_field(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
    varint(out, ((field_no as u64) << 3) | 2);
    varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// The plan §1.1 bookTicker push for BTCUSDT.
fn spot_book_push() -> Vec<u8> {
    let mut body = Vec::new();
    len_field(&mut body, 1, b"80535.88");
    len_field(&mut body, 2, b"0.380497");
    len_field(&mut body, 3, b"80535.89");
    len_field(&mut body, 4, b"0.33336356");
    len_field(&mut body, 5, b"81721676217");
    let mut f = Vec::new();
    len_field(&mut f, 1, b"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT");
    len_field(&mut f, 3, b"BTCUSDT");
    varint(&mut f, 6 << 3);
    varint(&mut f, 1_789_897_517_479);
    len_field(&mut f, 315, &body);
    f
}

// ---- TLS + WS scaffolding (the binance loopback shape) ----------------

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

/// Unmasked server→client frame (`first` = 0x81 text / 0x82 binary).
fn server_frame(first: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(first);
    if payload.len() <= 125 {
        out.push(payload.len() as u8);
    } else {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn extract_client_key(req: &[u8]) -> [u8; 24] {
    let needle = b"Sec-WebSocket-Key: ";
    let start = find(req, needle).expect("client must send Sec-WebSocket-Key") + needle.len();
    let mut out = [0u8; 24];
    out.copy_from_slice(&req[start..start + 24]);
    out
}

/// Read one masked client frame → (opcode, unmasked payload).
fn read_client_frame<R: Read>(r: &mut R) -> (u8, Vec<u8>) {
    let mut h = [0u8; 2];
    r.read_exact(&mut h).expect("frame header");
    assert!(h[1] & 0x80 != 0, "client frames are masked");
    let mut len = (h[1] & 0x7F) as usize;
    if len == 126 {
        let mut e = [0u8; 2];
        r.read_exact(&mut e).expect("ext len");
        len = u16::from_be_bytes(e) as usize;
    }
    let mut mask = [0u8; 4];
    r.read_exact(&mut mask).expect("mask");
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).expect("payload");
    for (i, b) in body.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
    (h[0] & 0x0F, body)
}

/// What one server connection saw: the request path + the client's
/// subscribe frames.
type Seen = Arc<Mutex<Vec<(String, Vec<(u8, Vec<u8>)>)>>>;

/// Serve one connection: handshake, read the class's subscribe frames,
/// answer with the class's push, hold until the client is done.
fn serve_one(mut sock: TcpStream, cfg: Arc<ServerConfig>, done: Arc<AtomicBool>, seen: Seen) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
    let mut conn = ServerConnection::new(cfg).expect("server conn");
    {
        let mut stream = Stream::new(&mut conn, &mut sock);
        let mut buf = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = stream.read(&mut buf[total..]).expect("server read");
            assert!(n > 0, "client closed before handshake complete");
            total += n;
            if find(&buf[..total], b"\r\n\r\n").is_some() {
                break;
            }
            assert!(total < buf.len(), "oversized client handshake");
        }
        let req = &buf[..total];
        let path = if find(req, b"GET /ws HTTP/1.1").is_some() {
            "/ws"
        } else if find(req, b"GET /edge HTTP/1.1").is_some() {
            "/edge"
        } else {
            panic!("unexpected request line");
        };
        assert!(find(req, b"Host: localhost").is_some(), "Host header");
        let key = extract_client_key(req);
        stream.write_all(&build_101_reply(&key)).expect("write 101");
        stream.flush().expect("flush 101");

        // The class's subscribe set arrives right after the upgrade.
        let want = if path == "/ws" { 1 } else { FUT_SUBS.len() };
        let mut frames = Vec::new();
        while frames.len() < want {
            frames.push(read_client_frame(&mut stream));
        }
        let push = if path == "/ws" {
            server_frame(0x82, &spot_book_push())
        } else {
            server_frame(0x81, FUT_DEPTH)
        };
        stream.write_all(&push).expect("write push");
        stream.flush().expect("flush push");
        seen.lock().unwrap().push((path.to_string(), frames));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    let _ = sock.shutdown(Shutdown::Both);
}

fn keepalive() -> core_net::Keepalive {
    core_net::Keepalive::new(core_net::KeepaliveCfg {
        ping_interval_ns: u64::MAX / 4,
        idle_timeout_ns: u64::MAX / 2,
    })
}

#[test]
fn mexc_tls_loopback_spot_pb_and_futures_json_through_run_multi() {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let done = Arc::new(AtomicBool::new(false));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));

    let server = {
        let done = done.clone();
        let seen = seen.clone();
        thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..2 {
                let (sock, _peer) = listener.accept().expect("accept");
                let (cfg, done, seen) = (server_cfg.clone(), done.clone(), seen.clone());
                handlers.push(thread::spawn(move || serve_one(sock, cfg, done, seen)));
            }
            for h in handlers {
                h.join().expect("handler thread");
            }
        })
    };

    let mut spot = MexcSymbolTable::new();
    spot.insert(b"BTCUSDT", SYM_SPOT).unwrap();
    let mut perp = MexcSymbolTable::new();
    perp.insert(b"BTC_USDT", SYM_PERP).unwrap();
    let mut conns = vec![
        MexcConn::new(
            Driver::new(0x5107, MexcClass::Spot, spot),
            b"localhost",
            MexcClass::Spot.ws_path(),
            keepalive(),
            core_net::Backoff::new(1_000_000, 10_000_000, 1),
        ),
        MexcConn::new(
            Driver::new(0xF07, MexcClass::Futures, perp),
            b"localhost",
            MexcClass::Futures.ws_path(),
            keepalive(),
            core_net::Backoff::new(1_000_000, 10_000_000, 2),
        ),
    ];

    let (mut prod, mut cons) = Ring::<Tick, TICK_RING_CAP>::new().split();
    let (mut etx, _erx) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let mut poll = mio::Poll::new().expect("mio poll");
    let mut events = mio::Events::with_capacity(16);
    let stop = StopFlag::new(false);
    let status = core_metrics::IngressStatus::new();
    let server_name: ServerName<'static> = ServerName::try_from("localhost")
        .expect("server name")
        .to_owned();

    let (res, ticks) = thread::scope(|s| {
        // Watcher: pop ticks until both classes delivered (or a bounded
        // deadline), then stop the loop.
        let watcher = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut ticks: Vec<Tick> = Vec::new();
            while ticks.len() < 2 && Instant::now() < deadline {
                match cons.try_pop() {
                    Some(t) => ticks.push(t),
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
            stop.store(true, Ordering::Relaxed);
            ticks
        });
        let res = run_multi(
            &mut conns,
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut poll,
            &mut events,
            &stop,
            &status,
            &mut NullCapture,
            |_i| TlsTransport::connect(addr, server_name.clone(), client_cfg.clone()).ok(),
        );
        (res, watcher.join().expect("watcher"))
    });
    done.store(true, Ordering::Release);
    server.join().expect("server thread");

    assert_eq!(res, RunResult::Stopped);
    assert_eq!(ticks.len(), 2, "one tick per class within 10 s");

    // The subscribe bytes, per class.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (path, frames) in seen.iter() {
        if path == "/ws" {
            assert_eq!(frames.len(), 1, "spot: ONE SUBSCRIPTION frame");
            assert_eq!(frames[0].0, 0x1, "text");
            assert_eq!(frames[0].1, SPOT_SUB);
        } else {
            let got: Vec<&[u8]> = frames.iter().map(|f| f.1.as_slice()).collect();
            assert_eq!(got, FUT_SUBS.to_vec(), "futures: one frame per (symbol, channel)");
            assert!(frames.iter().all(|f| f.0 == 0x1));
        }
    }

    // The ticks, per class.
    let spot = ticks.iter().find(|t| t.sym == SYM_SPOT).expect("spot tick");
    assert_eq!(spot.venue, VenueId::Mexc as u8);
    assert_eq!(spot.bid_px.raw(), 80_535_880_000);
    assert_eq!(spot.ask_px.raw(), 80_535_890_000);
    assert_eq!(spot.ask_qty.raw(), 333_363);
    assert_eq!(spot.venue_seq, (81_721_676_217u64 & 0xFFFF_FFFF) as u32);
    assert_eq!(spot.venue_time_ms, 1_789_897_517_479);
    let perp = ticks.iter().find(|t| t.sym == SYM_PERP).expect("futures tick");
    assert_eq!(perp.bid_px.raw(), 80_468_600_000);
    assert_eq!(perp.ask_px.raw(), 80_468_700_000);
    assert_eq!(perp.bid_qty.raw(), 31_288_000_000);
    assert_eq!(perp.venue_time_ms, 1_789_897_581_009);

    assert_eq!(status.state(), core_metrics::IngressState::Up);
    assert_eq!(status.parse_errors_total(), 0);
    assert!(status.ticks_total() >= 2);
    assert_eq!(conns[0].drv.sub_count(), 1, "spot bookTicker confirmed by data");
    assert_eq!(conns[1].drv.sub_count(), 1, "futures depth.full confirmed by data");
}
