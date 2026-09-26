// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test: the Hypercall ingress against a real `127.0.0.1`
//! TLS WebSocket server with a self-signed cert, driven through
//! [`run`] with a real `TlsTransport`. The server replays the GOLDEN
//! frames captured live on 2026-09-25, then closes the socket the way
//! the venue's slow-consumer law does (1008 + the reason JSON the D3
//! probe captured). The client must: send ClockSync + ONE Subscribe per
//! channel (the universe in ONE frame), turn the quotes into ticks,
//! count the close by cause, request a REST snapshot, reconnect,
//! re-subscribe identically, and keep ticking. No network beyond
//! loopback.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_net::{expected_accept, TlsTransport};
use core_ring::Ring;
use core_types::{ChannelEvent, NullCapture, OptSummary, Tick, EVENT_RING_SIZE, OPT_RING_SIZE};
use ingress_hypercall::counters::get;
use ingress_hypercall::run_loop::{
    run, Driver, HcConn, Lanes, RunResult, StopFlag, HANDOFF_RING_CAP, TICK_RING_CAP,
};
use ingress_hypercall::{HcCloseCause, HcCounters, HcSymbolTable, HcUnderlyings};

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

const Q1: &[u8] = include_bytes!("fixtures/quote_one_provider.json");
const Q2X: &[u8] = include_bytes!("fixtures/quote_two_providers_crossed.json");
const IDX: &[u8] = include_bytes!("fixtures/index_update.json");
const CLOSE_REASON: &[u8] = include_bytes!("fixtures/close_reason_message_limit.json");
const ACK: &[u8] = br#"{"type":"Subscribed","channel":"indicative_market_data"}"#;
/// The one-provider quote moved (a new bid): session two's tick.
const Q1_MOVED: &[u8] = br#"{"type":"IndicativeMarketData","instrument":"ETH-20260927-2675-P","best_bid":"7.7","best_ask":"15.2405","indicative_bid_size":"63.816073","indicative_ask_size":"63.816073","num_providers":1,"rfq_provider_quotes":[],"published_at":1790373480234,"timestamp":1790373480000}"#;

const ETH_P: u32 = (9 << 24) | 512;
const MU_C: u32 = (9 << 24) | 513;

const WANT_SUBS: [&[u8]; 4] = [
    br#"{"type":"Subscribe","channel":"index_prices"}"#,
    br#"{"type":"Subscribe","channel":"trades"}"#,
    br#"{"type":"Subscribe","channel":"market_updates"}"#,
    br#"{"type":"Subscribe","channel":"indicative_market_data","symbols":["ETH-20260927-2675-P","MU-20260928-1090-C"]}"#,
];

struct LoopbackCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> LoopbackCert {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("key DER");
    LoopbackCert { cert_der, key_der }
}

fn server_config(cert: &LoopbackCert) -> Arc<ServerConfig> {
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
            .expect("server config"),
    )
}

fn client_config(cert: &LoopbackCert) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("anchor");
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Unmasked server→client frame.
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

/// Every session's client frames (ClockSync + the subscribes).
type Seen = Arc<Mutex<Vec<Vec<(u8, Vec<u8>)>>>>;

/// Serve one session: upgrade, read the 5 client frames, send `script`,
/// then either close 1008 with the slow-consumer reason (`slow`) or hold
/// until the test is done.
fn serve(mut sock: TcpStream, cfg: Arc<ServerConfig>, script: Vec<Vec<u8>>, slow: bool, done: Arc<AtomicBool>, seen: Seen) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    let mut conn = ServerConnection::new(cfg).expect("server conn");
    {
        let mut stream = Stream::new(&mut conn, &mut sock);
        let mut buf = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = stream.read(&mut buf[total..]).expect("server read");
            assert!(n > 0, "client closed before the handshake");
            total += n;
            if find(&buf[..total], b"\r\n\r\n").is_some() {
                break;
            }
        }
        let req = &buf[..total];
        assert!(find(req, b"GET /ws HTTP/1.1").is_some(), "request line");
        assert!(find(req, b"Host: localhost").is_some(), "Host header");
        let k = find(req, b"Sec-WebSocket-Key: ").expect("key") + 19;
        let mut key = [0u8; 24];
        key.copy_from_slice(&req[k..k + 24]);
        let accept = expected_accept(&key);
        let mut reply = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
        reply.extend_from_slice(&accept);
        reply.extend_from_slice(b"\r\n\r\n");
        stream.write_all(&reply).expect("101");
        stream.flush().expect("flush");
        let mut frames = Vec::new();
        while frames.len() < 5 {
            frames.push(read_client_frame(&mut stream));
        }
        seen.lock().unwrap().push(frames);
        for f in &script {
            stream.write_all(f).expect("script");
        }
        stream.flush().expect("flush script");
        if slow {
            let mut body = 1008u16.to_be_bytes().to_vec();
            body.extend_from_slice(CLOSE_REASON);
            stream.write_all(&server_frame(0x88, &body)).expect("close");
            stream.flush().expect("flush close");
            // Like the venue: hold until the client drops the socket, so
            // no unread byte turns the close into a TCP RST that would
            // discard the frames still in flight.
            let mut sink = [0u8; 1024];
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                match stream.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    if !slow {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
    let _ = sock.shutdown(Shutdown::Both);
}

#[test]
fn golden_frames_ticks_a_slow_consumer_close_and_the_single_frame_resubscribe() {
    let cert = make_cert();
    let scfg = server_config(&cert);
    let ccfg = client_config(&cert);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let done = Arc::new(AtomicBool::new(false));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));

    let server = {
        let (done, seen) = (done.clone(), seen.clone());
        thread::spawn(move || {
            // Session 1: ack, two golden quotes, the index — then 1008.
            let (s1, _) = listener.accept().expect("accept 1");
            let script1 = vec![
                server_frame(0x81, ACK),
                server_frame(0x81, Q1),
                server_frame(0x81, Q2X),
                server_frame(0x81, IDX),
            ];
            serve(s1, scfg.clone(), script1, true, done.clone(), seen.clone());
            // Session 2: the moved quote.
            let (s2, _) = listener.accept().expect("accept 2");
            serve(s2, scfg, vec![server_frame(0x81, ACK), server_frame(0x81, Q1_MOVED)], false, done, seen);
        })
    };

    let mut symbols = HcSymbolTable::new();
    symbols.insert(b"ETH-20260927-2675-P", ETH_P).unwrap();
    symbols.insert(b"MU-20260928-1090-C", MU_C).unwrap();
    let mut conn = HcConn::new(
        Driver::new(0xC0FFEE, symbols, HcUnderlyings::new()),
        b"localhost",
        core_net::Backoff::new(1_000_000, 10_000_000, 1),
    );
    let (mut tp, mut tc) = Ring::<Tick, TICK_RING_CAP>::new().split();
    let (mut ep, _ec) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let (mut op, _oc) = Ring::<OptSummary, OPT_RING_SIZE>::new().split();
    let (_hp, mut hc) = Ring::<OptSummary, HANDOFF_RING_CAP>::new().split();
    let mut poll = mio::Poll::new().expect("poll");
    let mut events = mio::Events::with_capacity(16);
    let stop = StopFlag::new(false);
    let status = core_metrics::IngressStatus::new();
    let counters = HcCounters::new();
    let server_name: ServerName<'static> = ServerName::try_from("localhost").expect("name").to_owned();

    let (res, ticks) = thread::scope(|s| {
        let watcher = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut ticks: Vec<Tick> = Vec::new();
            while ticks.len() < 3 && Instant::now() < deadline {
                match tc.try_pop_ref().as_deref().copied() {
                    Some(t) => ticks.push(t),
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
            stop.store(true, Ordering::Relaxed);
            ticks
        });
        let mut lanes = Lanes {
            ticks: &mut tp,
            events: &mut ep,
            event_mask: 0,
            opts: &mut op,
        };
        let res = run(
            &mut conn,
            &mut lanes,
            &mut hc,
            &mut poll,
            &mut events,
            &stop,
            &status,
            &counters,
            &mut NullCapture,
            || TlsTransport::connect(addr, server_name.clone(), ccfg.clone()).ok(),
        );
        (res, watcher.join().expect("watcher"))
    });
    done.store(true, Ordering::Release);
    server.join().expect("server");

    assert_eq!(res, RunResult::Stopped);
    assert_eq!(ticks.len(), 3, "two in session 1, one after the reconnect");
    assert_eq!(ticks[0].sym, ETH_P);
    assert_eq!((ticks[0].bid_px.raw(), ticks[0].ask_px.raw()), (7_679_000, 15_240_500));
    assert_eq!(ticks[1].sym, MU_C);
    assert!(ticks[1].bid_px.raw() > ticks[1].ask_px.raw(), "the crossed quote, as published");
    assert_eq!((ticks[2].sym, ticks[2].bid_px.raw()), (ETH_P, 7_700_000));

    // Both sessions sent ClockSync + the SAME one-frame-per-channel set.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for frames in seen.iter() {
        assert!(frames[0].1.starts_with(br#"{"type":"ClockSync","nonce":""#));
        let subs: Vec<&[u8]> = frames[1..].iter().map(|f| f.1.as_slice()).collect();
        assert_eq!(subs, WANT_SUBS.to_vec());
    }

    // The close was counted by cause and asked the poller for a snapshot.
    assert_eq!(get(&counters.ws.closes[HcCloseCause::MessageLimit as usize]), 1);
    assert_eq!(counters.snapshot_req.load(Ordering::Acquire), 1);
    assert_eq!(get(&counters.ws.subscribes), 2);
    assert_eq!(get(&counters.ws.crossed_quotes), 1);
    assert_eq!(status.reconnects_total(), 1);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.state(), core_metrics::IngressState::Up);
}
