// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `UserWs` — the socket that carries the FILLS — against a real
//! rustls WebSocket server on `127.0.0.1`.
//!
//! Until the E7 review this was the one socket in the arm with no
//! loopback behind it: three unit tests covered a bad hostname, the
//! master-hex string and the `Display` impls, and the handshake, the
//! first-frame-in-the-101 compaction, the straddle guard, the
//! unread-tail compaction, the Ping echo, the Close, the keepalive and
//! the oversize refusal had only ever run against the venue — "a path
//! that has only ever run behind a round trip is a claim about source
//! code" (`exchange.rs`, gate 59). This is the loopback.
//!
//! | script | asserts |
//! |---|---|
//! | stream | 101 + first frame in ONE segment; a frame split across two writes; two frames in one write; a Ping mid-stream is answered with the SAME payload; a Close is `Disconnected` |
//! | subscribes | the two subscribe frames the client sends are masked, well-formed and name the master |
//! | keepalive | a silent server draws `{"method":"ping"}` after the ping interval and `Disconnected` after the idle timeout |
//! | refused upgrade | a 403 is `Upgrade`, not a hang |
//! | oversize | a frame that cannot fit the receive buffer is `BadFrame`, not an unbounded read |
//!
//! Offline path: this file MAY allocate (server side, scripts).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_net::{
    expected_accept, ws_read_frame, ws_unmask_in_place, KeepaliveCfg, WsOpcode, WsReadResult,
};
use exec_hyperliquid::userws_conn::{UserWs, WsErr, MAX_WS_BUF};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

const MASTER: [u8; 20] = [0xA5; 20];

/// A client frame the server read, unmasked.
#[derive(Debug, Clone)]
struct ClientFrame {
    opcode: WsOpcode,
    payload: Vec<u8>,
}

/// What the scripted server does after the upgrade.
#[derive(Copy, Clone)]
enum Script {
    /// The stream script (see the module table). Reads the client's
    /// two subscribes first, reports every client frame on the
    /// channel.
    Stream,
    /// Refuse the upgrade with a 403.
    Refuse,
    /// Send one frame whose declared payload exceeds the client's
    /// receive buffer, then keep feeding zeros.
    Oversize,
    /// Upgrade, read the subscribes, then say nothing and report
    /// every client frame (the keepalive ping) on the channel.
    Silent,
}

struct Cert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> Cert {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der = c.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");
    Cert { cert_der, key_der }
}

/// Server-side: pull complete client frames (masked) out of `buf`,
/// unmask them and push them to `out`. Returns `(bytes consumed,
/// frames drained)`.
fn drain_client_frames(
    buf: &mut [u8],
    len: usize,
    out: &mpsc::Sender<ClientFrame>,
) -> (usize, usize) {
    let mut consumed = 0usize;
    let mut frames = 0usize;
    loop {
        let (header, span) = match ws_read_frame(&buf[consumed..len]) {
            WsReadResult::Frame { header, payload } => (header, payload),
            _ => return (consumed, frames),
        };
        let total = header.header_len as usize + header.payload_len as usize;
        if consumed + total > len {
            return (consumed, frames);
        }
        assert!(header.masked, "client frames MUST be masked (RFC 6455 §5.1)");
        let start = consumed + span.start;
        let end = consumed + span.end;
        ws_unmask_in_place(&mut buf[start..end], header.mask);
        let _ = out.send(ClientFrame {
            opcode: header.opcode,
            payload: buf[start..end].to_vec(),
        });
        consumed += total;
        frames += 1;
    }
}

/// Read until `n` client frames have been drained (or the peer is
/// gone). Whatever was already buffered counts first.
fn read_client_frames<S: Read>(
    stream: &mut S,
    buf: &mut [u8],
    len: &mut usize,
    n: usize,
    out: &mpsc::Sender<ClientFrame>,
) {
    let mut got = 0usize;
    loop {
        let before = *len;
        let (consumed, frames) = drain_client_frames(buf, before, out);
        if consumed > 0 {
            buf.copy_within(consumed..before, 0);
            *len -= consumed;
        }
        got += frames;
        if got >= n {
            return;
        }
        let Ok(k) = stream.read(&mut buf[*len..]) else {
            return;
        };
        if k == 0 {
            return;
        }
        *len += k;
    }
}

/// Unmasked server frame: FIN + opcode + 7/16/64-bit length, no mask.
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 10);
    v.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        v.push(n as u8);
    } else if n < 65_536 {
        v.push(126);
        v.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        v.push(127);
        v.extend_from_slice(&(n as u64).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

fn boot(script: Script) -> (u16, Arc<ClientConfig>, mpsc::Receiver<ClientFrame>) {
    let cert = make_cert();
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert_der.clone()).expect("trust anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut conn = ServerConnection::new(server_cfg).expect("conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        // ---- the client's upgrade request ----------------------------
        let mut buf = vec![0u8; 64 * 1024];
        let mut len = 0usize;
        let header_end = loop {
            let Ok(n) = stream.read(&mut buf[len..]) else {
                return;
            };
            if n == 0 {
                return;
            }
            len += n;
            if let Some(i) = (0..len.saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
        assert!(head.starts_with("GET /ws HTTP/1.1\r\n"), "request line: {head:?}");
        let key_line = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
            .expect("Sec-WebSocket-Key header");
        let key = key_line.split(':').nth(1).unwrap().trim().as_bytes();
        let mut key24 = [0u8; 24];
        key24.copy_from_slice(key);
        let accept = expected_accept(&key24);

        if let Script::Refuse = script {
            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(200));
            return;
        }

        let mut resp = Vec::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");

        match script {
            Script::Refuse => unreachable!(),
            Script::Oversize => {
                let _ = stream.write_all(&resp);
                // A text frame claiming MAX_WS_BUF + 1 bytes: the client
                // must refuse once its buffer is full, never grow it.
                let mut hdr = Vec::new();
                hdr.push(0x81);
                hdr.push(127);
                hdr.extend_from_slice(&((MAX_WS_BUF as u64) + 1).to_be_bytes());
                let _ = stream.write_all(&hdr);
                let zeros = vec![0u8; 64 * 1024];
                let mut sent = 0usize;
                while sent <= MAX_WS_BUF + 1 {
                    if stream.write_all(&zeros).is_err() {
                        break;
                    }
                    sent += zeros.len();
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(200));
            }
            Script::Stream => {
                // 101 AND the first frame in the SAME write — the shape
                // the venue produces when it packs the snapshot into
                // the upgrade's segment.
                resp.extend_from_slice(&server_frame(0x1, br#"{"channel":"subscriptionResponse","n":1}"#));
                let _ = stream.write_all(&resp);
                let _ = stream.flush();
                // The two subscribes; the client sends them right after
                // the 101, so they may already be in flight.
                let tail = buf[header_end..len].to_vec();
                buf[..tail.len()].copy_from_slice(&tail);
                len = tail.len();
                read_client_frames(&mut stream, &mut buf, &mut len, 2, &tx);
                // Second message split across TWO writes with a gap.
                let f2 = server_frame(0x1, br#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0x","fills":[]}}"#);
                let cut = f2.len() / 2;
                let _ = stream.write_all(&f2[..cut]);
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(30));
                let _ = stream.write_all(&f2[cut..]);
                let _ = stream.flush();
                // Two messages in ONE write, then a Ping with a payload,
                // in the same write too.
                let mut two = server_frame(0x1, br#"{"channel":"orderUpdates","data":[]}"#);
                two.extend_from_slice(&server_frame(0x1, br#"{"channel":"pong"}"#));
                two.extend_from_slice(&server_frame(0x9, b"hl-ping-7"));
                let _ = stream.write_all(&two);
                let _ = stream.flush();
                // The pong the client owes us.
                read_client_frames(&mut stream, &mut buf, &mut len, 1, &tx);
                // And a Close.
                let _ = stream.write_all(&server_frame(0x8, &[0x03, 0xE8]));
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(200));
            }
            Script::Silent => {
                let _ = stream.write_all(&resp);
                let _ = stream.flush();
                let tail = buf[header_end..len].to_vec();
                buf[..tail.len()].copy_from_slice(&tail);
                len = tail.len();
                // Two subscribes, then whatever the keepalive sends —
                // keep reading until the client hangs up.
                loop {
                    let Ok(k) = stream.read(&mut buf[len..]) else {
                        return;
                    };
                    if k == 0 {
                        return;
                    }
                    len += k;
                    let before = len;
                    let (consumed, _) = drain_client_frames(&mut buf, before, &tx);
                    if consumed > 0 {
                        buf.copy_within(consumed..before, 0);
                        len -= consumed;
                    }
                }
            }
        }
    });

    (port, client_cfg, rx)
}

fn client(port: u16, cfg: Arc<ClientConfig>) -> UserWs {
    UserWs::new("localhost", port, cfg, &MASTER).expect("client")
}

/// Pump until `want` payloads were delivered or `for_` elapsed.
fn pump_until(ws: &mut UserWs, want: usize, for_: Duration, sink: &mut Vec<Vec<u8>>) -> Result<(), WsErr> {
    let until = Instant::now() + for_;
    while sink.len() < want && Instant::now() < until {
        ws.pump(Duration::from_millis(5), |p| sink.push(p.to_vec()))?;
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

#[test]
fn the_stream_script_round_trips_every_frame_shape() {
    let (port, cfg, from_server) = boot(Script::Stream);
    let mut ws = client(port, cfg);
    ws.connect().expect("connect + upgrade + subscribes");
    assert!(ws.is_connected());

    // The server saw two masked, well-formed subscribes naming the
    // master, in order.
    let s1 = from_server.recv_timeout(Duration::from_secs(5)).expect("subscribe 1");
    let s2 = from_server.recv_timeout(Duration::from_secs(5)).expect("subscribe 2");
    for (s, ch) in [(&s1, "userFills"), (&s2, "orderUpdates")] {
        assert_eq!(s.opcode, WsOpcode::Text);
        let text = std::str::from_utf8(&s.payload).expect("utf8");
        assert_eq!(
            text,
            format!(
                "{{\"method\":\"subscribe\",\"subscription\":{{\"type\":\"{ch}\",\"user\":\"{}\"}}}}",
                ws.master_hex()
            )
        );
    }

    // Four TEXT payloads, in order: the one packed into the 101's
    // segment, the split one, and the two that arrived together.
    let mut got: Vec<Vec<u8>> = Vec::new();
    pump_until(&mut ws, 4, Duration::from_secs(5), &mut got).expect("pump");
    assert_eq!(got.len(), 4, "delivered: {got:?}");
    assert_eq!(got[0], br#"{"channel":"subscriptionResponse","n":1}"#.to_vec());
    assert_eq!(
        got[1],
        br#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0x","fills":[]}}"#.to_vec()
    );
    assert_eq!(got[2], br#"{"channel":"orderUpdates","data":[]}"#.to_vec());
    assert_eq!(got[3], br#"{"channel":"pong"}"#.to_vec());

    // The Ping that rode in with them was answered with its payload.
    let pong = from_server.recv_timeout(Duration::from_secs(5)).expect("pong");
    assert_eq!(pong.opcode, WsOpcode::Pong);
    assert_eq!(pong.payload, b"hl-ping-7".to_vec());

    // The Close is a disconnect, and the socket is gone.
    let mut rest: Vec<Vec<u8>> = Vec::new();
    let r = pump_until(&mut ws, 1, Duration::from_secs(5), &mut rest);
    assert_eq!(r, Err(WsErr::Disconnected), "a Close must surface as Disconnected");
    assert!(rest.is_empty(), "nothing after the Close: {rest:?}");
    assert!(!ws.is_connected());
}

#[test]
fn a_silent_server_draws_a_ping_then_a_reconnect() {
    let (port, cfg, from_server) = boot(Script::Silent);
    let mut ws = client(port, cfg);
    // The 50 s / 75 s law in milliseconds.
    ws.set_keepalive(KeepaliveCfg {
        ping_interval_ns: 120_000_000,
        idle_timeout_ns: 400_000_000,
    });
    ws.connect().expect("connect");
    let _s1 = from_server.recv_timeout(Duration::from_secs(5)).expect("subscribe 1");
    let _s2 = from_server.recv_timeout(Duration::from_secs(5)).expect("subscribe 2");

    // Pump through the interval: nothing arrives, the ping goes out.
    let t0 = Instant::now();
    let mut sink: Vec<Vec<u8>> = Vec::new();
    let mut outcome = Ok(());
    let mut pumps = 0u32;
    while t0.elapsed() < Duration::from_secs(3) {
        match ws.pump(Duration::from_millis(2), |p| sink.push(p.to_vec())) {
            Ok(_) => {}
            Err(e) => {
                outcome = Err(e);
                break;
            }
        }
        pumps += 1;
        thread::sleep(Duration::from_millis(5));
    }
    let elapsed = t0.elapsed();
    assert_eq!(outcome, Err(WsErr::Disconnected), "an unanswered ping is a dead session");
    assert!(!ws.is_connected());
    assert!(
        elapsed >= Duration::from_millis(390) && elapsed < Duration::from_millis(1500),
        "the idle timeout, not the 3 s budget, ended it: {elapsed:?} after {pumps} pumps"
    );
    let ping = from_server.recv_timeout(Duration::from_secs(2)).expect("the keepalive ping");
    assert_eq!(ping.opcode, WsOpcode::Text);
    assert_eq!(ping.payload, b"{\"method\":\"ping\"}".to_vec());
    assert!(sink.is_empty(), "a silent server delivers nothing: {sink:?}");
    // And exactly ONE ping: the interval anchors on the ping itself,
    // so a second one would need another 120 ms of silence, which the
    // 400 ms idle cut-off leaves room for — twice at most.
    let mut extra = 0usize;
    while from_server.try_recv().is_ok() {
        extra += 1;
    }
    assert!(extra <= 3, "ping storm: {} pings", 1 + extra);
}

#[test]
fn a_refused_upgrade_is_an_upgrade_error_not_a_hang() {
    let (port, cfg, _rx) = boot(Script::Refuse);
    let mut ws = client(port, cfg);
    let t0 = Instant::now();
    let r = ws.connect();
    assert_eq!(r, Err(WsErr::Upgrade));
    assert!(t0.elapsed() < Duration::from_secs(5), "bounded: {:?}", t0.elapsed());
    assert!(!ws.is_connected());
}

#[test]
fn a_frame_larger_than_the_buffer_is_refused_not_grown() {
    let (port, cfg, _rx) = boot(Script::Oversize);
    let mut ws = client(port, cfg);
    ws.connect().expect("connect");
    let delivered = AtomicUsize::new(0);
    let until = Instant::now() + Duration::from_secs(10);
    let mut outcome = Ok(0usize);
    while Instant::now() < until {
        match ws.pump(Duration::from_millis(5), |_| {
            delivered.fetch_add(1, Ordering::Relaxed);
        }) {
            Ok(_) => thread::sleep(Duration::from_millis(1)),
            Err(e) => {
                outcome = Err(e);
                break;
            }
        }
    }
    assert_eq!(outcome, Err(WsErr::BadFrame), "the buffer is the bound");
    assert_eq!(delivered.load(Ordering::Relaxed), 0, "nothing of it may be delivered");
    assert!(!ws.is_connected());
}
