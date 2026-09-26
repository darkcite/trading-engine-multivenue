// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `HcExchange` against a real rustls server on `127.0.0.1` — the house
//! standard for every network path. One scripted venue serves the REST
//! surface (`/portfolio`, `/orders`, `POST /order`, `DELETE
//! /order_cloid`) and the private socket (`/ws`: `Authenticate` →
//! `Authenticated`, two subscribes, then `Fill` / `OrderUpdate` frames
//! as the order's life unfolds).
//!
//! | test | asserts |
//! |---|---|
//! | lifecycle | reconcile seeds → the socket authenticates as the OWNER → a place is signed over its own body (the server recovers the signer from the bytes it received) → the fill comes from the socket alone, attributed to the slot and the member's id → the cancel by client id → the unfilled rest retires → the next reconcile agrees |
//! | refusals | `REJECTED` at HTTP 200 is a refusal with its reason and a reject streak; an unreadable 200 is `JsonMalformed`; a 401 is `SignerRejected` — none is an acceptance, none books anything |

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clob_dispatcher::{DispatchError, OrderDispatch};
use core_config::SecretKeyBytes;
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_net::{expected_accept, ws_read_frame, ws_unmask_in_place, WsReadResult};
use core_types::{CancelReq, Order, Price, Qty, Side, VenueId};
use exec_hypercall::{json, HcExchange, HcExecConfig, HcInstruments};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};
use signer_eip712::hypercall as hc;

const KEY: [u8; 32] = [0x42; 32];
const WALLET: [u8; 20] = [0xab; 20];
const SYM: u32 = 0x0900_0201;
const NAME: &[u8] = b"BTC-20261002-100000-C";

#[derive(Default)]
struct Venue {
    /// What `POST /order` answers (status, body).
    post_reply: (u16, Vec<u8>),
    /// What `DELETE /order_cloid` answers, when not the CANCELED default.
    delete_reply: Option<(u16, Vec<u8>)>,
    /// The bodies received on writes, in order.
    writes: Vec<(String, Vec<u8>)>,
    /// The socket should send the fill (set by the POST).
    placed: bool,
    /// The socket sent the fill (the portfolio then holds 0.1).
    filled: bool,
    /// The cancel came (the socket sends CANCELED).
    cancelled: bool,
    /// The wallet the socket authenticated.
    authed_wallet: String,
}

fn make_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");
    (c.cert.der().clone(), key)
}

fn server_frame(payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0x81u8];
    let n = payload.len();
    if n < 126 {
        v.push(n as u8);
    } else {
        v.push(126);
        v.extend_from_slice(&(n as u16).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP request: `(head, body)`; `None` when the peer is gone.
fn read_request<S: Read>(s: &mut S, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i + 4]).to_string();
            let cl = head
                .lines()
                .find_map(|l| {
                    let l = l.to_ascii_lowercase();
                    l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < i + 4 + cl {
                let mut tmp = [0u8; 4096];
                let n = s.read(&mut tmp).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[i + 4..i + 4 + cl].to_vec();
            buf.drain(..i + 4 + cl);
            return Some((head, body));
        }
        let mut tmp = [0u8; 4096];
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn reply<S: Write>(s: &mut S, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

/// Read client frames until `n` text payloads arrived.
fn client_texts<S: Read>(s: &mut S, buf: &mut Vec<u8>, n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while out.len() < n {
        while let WsReadResult::Frame { header: h, payload: span } = ws_read_frame(buf) {
            let total = h.header_len as usize + h.payload_len as usize;
            if total > buf.len() {
                break;
            }
            assert!(h.masked, "client frames are masked");
            let mut p = buf[span.start..span.end].to_vec();
            ws_unmask_in_place(&mut p, h.mask);
            out.push(p);
            buf.drain(..total);
        }
        if out.len() >= n {
            break;
        }
        let mut tmp = [0u8; 4096];
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return out,
            Ok(k) => buf.extend_from_slice(&tmp[..k]),
        }
    }
    out
}

fn serve_ws<S: Read + Write>(s: &mut S, head: &str, mut buf: Vec<u8>, v: &Mutex<Venue>, done: &AtomicBool) {
    let key = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|k| k.trim().as_bytes().to_vec())
        .expect("key");
    let mut key24 = [0u8; 24];
    key24.copy_from_slice(&key);
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&expected_accept(&key24));
    resp.extend_from_slice(b"\r\n\r\n");
    let _ = s.write_all(&resp);
    let _ = s.flush();
    let auth = client_texts(s, &mut buf, 1);
    let a = String::from_utf8_lossy(&auth[0]).to_string();
    assert!(a.starts_with(r#"{"type":"Authenticate","wallet":"0x"#), "{a}");
    v.lock().unwrap().authed_wallet = a;
    let _ = s.write_all(&server_frame(br#"{"type":"Authenticated","wallet":"0x"}"#));
    let _ = s.flush();
    let subs = client_texts(s, &mut buf, 2);
    assert_eq!(subs[0], br#"{"type":"Subscribe","channel":"fills"}"#);
    assert_eq!(subs[1], br#"{"type":"Subscribe","channel":"order_updates"}"#);
    let mut ok = server_frame(br#"{"type":"Subscribed","channel":"fills"}"#);
    ok.extend_from_slice(&server_frame(br#"{"type":"Subscribed","channel":"order_updates"}"#));
    let _ = s.write_all(&ok);
    let _ = s.flush();
    let mut sent_fill = false;
    let mut sent_cancel = false;
    while !done.load(Ordering::Acquire) {
        let (placed, cancelled) = {
            let g = v.lock().unwrap();
            (g.placed, g.cancelled)
        };
        if placed && !sent_fill {
            let mut w = server_frame(br#"{"type":"Fill","order_id":42,"fill_id":1,"symbol":"BTC-20261002-100000-C","side":"buy","price":"12.5","size":"0.1","timestamp":1767225600000,"wallet_address":"0xab","fee":"0","trade_id":7,"is_taker":true,"instrument_type":"option"}"#);
            // The replay a reconnect would bring — booked once.
            w.extend_from_slice(&w.clone());
            w.extend_from_slice(&server_frame(br#"{"type":"OrderUpdate","order_id":42,"status":"PARTIALLY_FILLED","filled_size":"0.1","timestamp":1,"reason":null,"wallet_address":"0xab","instrument_type":"option"}"#));
            let _ = s.write_all(&w);
            let _ = s.flush();
            sent_fill = true;
            v.lock().unwrap().filled = true;
        }
        if cancelled && !sent_cancel {
            let _ = s.write_all(&server_frame(br#"{"type":"OrderUpdate","order_id":42,"status":"CANCELED","filled_size":"0.1","timestamp":2,"reason":null,"wallet_address":"0xab","instrument_type":"option"}"#));
            let _ = s.flush();
            sent_cancel = true;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn serve_conn(sock: std::net::TcpStream, cfg: Arc<ServerConfig>, v: Arc<Mutex<Venue>>, done: Arc<AtomicBool>) {
    let mut sock = sock;
    let mut conn = ServerConnection::new(cfg).expect("conn");
    let mut s = Stream::new(&mut conn, &mut sock);
    let mut buf = Vec::new();
    while let Some((head, body)) = read_request(&mut s, &mut buf) {
        let line = head.lines().next().unwrap_or("").to_string();
        if line.starts_with("GET /ws ") {
            serve_ws(&mut s, &head, std::mem::take(&mut buf), &v, &done);
            return;
        }
        if line.starts_with("GET /portfolio?wallet=0xab") {
            let pos = if v.lock().unwrap().filled {
                r#"[{"symbol":"BTC-20261002-100000-C","amount":"0.1","entry_price":"12.5","margin_posted":"0","realized_pnl":"0","unrealized_pnl":"0","updated_at":"x","wallet_address":"0xab"}]"#
            } else {
                "[]"
            };
            let b = format!(r#"{{"success":true,"data":{{"wallet_address":"0xab","positions":{pos},"total_margin_used":"0","available_balance":"5","portfolio_snapshot_timestamp_ms":1,"margin_mode":"standard"}},"error":null}}"#);
            reply(&mut s, 200, b.as_bytes());
        } else if line.starts_with("GET /orders?wallet=0xab") && line.contains("status=open") {
            reply(&mut s, 200, br#"{"success":true,"data":[],"pagination":{"limit":100,"offset":0,"count":0}}"#);
        } else if line.starts_with("POST /order ") {
            let (st, b) = {
                let mut g = v.lock().unwrap();
                g.writes.push(("POST /order".into(), body));
                g.placed = true;
                g.post_reply.clone()
            };
            reply(&mut s, st, &b);
        } else if line.starts_with("DELETE /order_cloid ") {
            let custom = {
                let mut g = v.lock().unwrap();
                g.writes.push(("DELETE /order_cloid".into(), body));
                let c = g.delete_reply.clone();
                if c.is_none() {
                    g.cancelled = true;
                }
                c
            };
            match custom {
                Some((st, b)) => reply(&mut s, st, &b),
                None => reply(&mut s, 200, br#"{"success":true,"data":{"timestamp":2,"info":{"symbol":"BTC-20261002-100000-C","price":"12.5","size":"0.2","side":"Buy","tif":"gtc","is_perp":false},"status":"CANCELED","filled_size":"0.1","wallet_address":"0xab","order_id":42},"error":null}"#),
            }
        } else {
            reply(&mut s, 404, br#"{"code":"NOT_FOUND","message":"no route"}"#);
        }
    }
}

fn boot(post_reply: (u16, Vec<u8>)) -> (u16, Arc<ClientConfig>, Arc<Mutex<Venue>>, Arc<AtomicBool>) {
    let (cert, key) = make_cert();
    let scfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert).expect("anchor");
    let ccfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let v = Arc::new(Mutex::new(Venue {
        post_reply,
        ..Venue::default()
    }));
    let done = Arc::new(AtomicBool::new(false));
    let (v2, d2) = (v.clone(), done.clone());
    thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            let (c, v3, d3) = (scfg.clone(), v2.clone(), d2.clone());
            thread::spawn(move || serve_conn(sock, c, v3, d3));
        }
    });
    (port, ccfg, v, done)
}

fn arm(port: u16, tls: Arc<ClientConfig>) -> HcExchange {
    let cfg = HcExecConfig::new("localhost", "localhost", WALLET, SecretKeyBytes::new_locked(KEY).unwrap()).unwrap();
    let mut t = HcInstruments::new();
    t.insert(SYM, NAME).unwrap();
    HcExchange::new(&cfg, tls, t, 7, port).expect("arm")
}

fn order(oid: u64) -> Order {
    let mut o = Order::new(
        core_time::now_ns(),
        VenueId::Hypercall,
        SYM,
        Side::Bid,
        ORDER_KIND_IOC,
        Price::from_raw(12_500_000),
        Qty::from_raw(200_000),
        oid,
    );
    o.strategy_id = 7;
    o
}

fn hexb(s: &[u8]) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(std::str::from_utf8(&s[2 * i..2 * i + 2]).unwrap(), 16).unwrap())
        .collect()
}

/// Recover the address that signed `digest` with the body's `signature`.
fn signer_of(body: &[u8], digest: &[u8; 32]) -> [u8; 20] {
    let sig = hexb(&json::field(body, b"signature").unwrap().bytes(body)[2..]);
    let secp = secp256k1::Secp256k1::new();
    let rid = secp256k1::ecdsa::RecoveryId::from_i32(i32::from(sig[64]) - 27).unwrap();
    let rs = secp256k1::ecdsa::RecoverableSignature::from_compact(&sig[..64], rid).unwrap();
    let msg = secp256k1::Message::from_digest_slice(digest).unwrap();
    let pk = secp.recover_ecdsa(&msg, &rs).unwrap();
    let h = signer_eip712::keccak256(&pk.serialize_uncompressed()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    a
}

fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

const OPEN: &[u8] = br#"{"timestamp":1,"info":{"symbol":"BTC-20261002-100000-C","price":"12.5","size":"0.2","side":"Buy","tif":"gtc","is_perp":false,"order_id":42},"status":"OPEN","filled_size":"0","wallet_address":"0xab","order_id":42,"reason":null}"#;

#[test]
fn lifecycle_place_fill_cancel_reconcile() {
    let (port, tls, v, done) = boot((200, OPEN.to_vec()));
    let mut a = arm(port, tls);

    let r = a.reconcile().expect("seed");
    assert!(r.agreed && r.positions == 0 && r.open_orders == 0);
    until("the private socket", 10, || {
        a.pump();
        a.ws_connected()
    });
    assert!(v.lock().unwrap().authed_wallet.contains(&"ab".repeat(20)), "the OWNER authenticates");

    // The engine's maker is post-only and the venue has none: refused
    // before anything is sent.
    let mut maker = order(899);
    maker.kind = ORDER_KIND_MAKER;
    assert_eq!(a.submit(&maker), Err(DispatchError::NoLiveRoute));
    assert!(v.lock().unwrap().writes.is_empty());
    // The verbs' resting order: gtc + book_only (the dust smoke's path).
    a.place_resting(&order(900)).expect("accepted");
    assert_eq!(a.live_orders(), 1);
    assert!(a.try_next_fill().is_none(), "the ACK books nothing (E-5)");
    let mut fill = None;
    until("the fill", 10, || {
        a.pump();
        fill = a.try_next_fill();
        fill.is_some()
    });
    let f = fill.unwrap();
    assert_eq!((f.sym, f.strategy_id, f.order_id, f.side), (SYM, 7, 900, Side::Bid));
    assert_eq!((f.px.raw(), f.qty.raw()), (12_500_000, 100_000));
    assert_eq!(f.origin, core_types::FILL_ORIGIN_VENUE);
    // The replayed copy is not booked twice.
    for _ in 0..20 {
        a.pump();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(a.try_next_fill().is_none());
    assert_eq!(a.counters().fills_dup, 1);
    assert_eq!(a.position(SYM), 100_000);

    let mut req = CancelReq::new(core_time::now_ns(), VenueId::Hypercall, SYM, 900);
    req.strategy_id = 7;
    a.cancel(&req).expect("cancel accepted");
    assert_eq!(a.live_orders(), 0);
    assert_eq!(a.try_next_retired(), Some((900, 7)), "the unfilled rest leaves the resting count");

    let r = a.reconcile().expect("reconcile");
    assert!(r.agreed, "{r:?}");
    assert_eq!(a.halt_signal().reconciled, 1);
    assert_eq!(a.halt_signal().reject_streak, 0);

    // D7 end to end: the venue recovers the signer from the bytes it got.
    let signer = signer_eip712::address_from_private_key(&KEY).unwrap();
    let dom = hc::hc_domain_separator(hc::HC_CHAIN_ID_MAINNET);
    let g = v.lock().unwrap();
    let (_, place) = &g.writes[0];
    let s = |k: &[u8]| json::field(place, k).unwrap().bytes(place).to_vec();
    let nonce = json::field(place, b"nonce").unwrap().as_u64(place).unwrap();
    let h = hc::place_order_struct_hash(&hc::HcPlaceView {
        wallet: &WALLET,
        symbol: &s(b"symbol"),
        side: &s(b"side"),
        size: &s(b"size"),
        price: &s(b"price"),
        tif: &s(b"tif"),
        route: &s(b"route"),
        client_id: &s(b"client_id"),
        nonce,
    });
    assert_eq!(signer_of(place, &hc::hc_eip712_digest(&dom, &h)), signer);
    assert_eq!(s(b"route"), b"book_only");
    assert_eq!(s(b"tif"), b"gtc");
    assert_eq!(s(b"size"), b"0.2");
    assert_eq!(exec_hypercall::cloid::decode(&s(b"client_id")), Some((7, 900)));
    let (_, cancel) = &g.writes[1];
    let cn = json::field(cancel, b"nonce").unwrap().as_u64(cancel).unwrap();
    let cid = json::field(cancel, b"client_id").unwrap().bytes(cancel).to_vec();
    let h = hc::cancel_order_by_client_id_struct_hash(&WALLET, &cid, cn);
    assert_eq!(signer_of(cancel, &hc::hc_eip712_digest(&dom, &h)), signer);
    assert!(cn > nonce, "nonces never repeat");
    done.store(true, Ordering::Release);
}

#[test]
fn refusals_are_never_acceptances() {
    let cases: [(u16, &[u8], DispatchError); 3] = [
        (
            200,
            br#"{"timestamp":1,"info":{},"status":"REJECTED","filled_size":"0","wallet_address":"0xab","reason":"price below tick"}"#,
            DispatchError::Http(200),
        ),
        (200, b"<html>maintenance</html>", DispatchError::JsonMalformed),
        (401, br#"{"code":"BAD_SIGNATURE","message":"signature mismatch"}"#, DispatchError::SignerRejected),
    ];
    for (st, body, want) in cases {
        let (port, tls, _v, done) = boot((st, body.to_vec()));
        let mut a = arm(port, tls);
        let e = a.submit(&order(1)).unwrap_err();
        assert_eq!(e, want, "{}", String::from_utf8_lossy(body));
        {
            let g = _v.lock().unwrap();
            let (_, b) = &g.writes[0];
            let tif = json::field(b, b"tif").unwrap().bytes(b).to_vec();
            let route = json::field(b, b"route").unwrap().bytes(b).to_vec();
            assert_eq!((tif.as_slice(), route.as_slice()), (&b"ioc"[..], &b"best_execution"[..]));
        }
        assert_eq!(a.live_orders(), 0);
        assert!(a.try_next_fill().is_none());
        assert_eq!(a.halt_signal().reject_streak, 1);
        if st == 200 && body.starts_with(b"{") {
            assert_eq!(a.last_reason(), b"price below tick");
        }
        if st == 401 {
            assert_eq!(a.counters().auth_refused, 1);
            assert_eq!(a.last_reason(), b"signature mismatch");
        }
        done.store(true, Ordering::Release);
    }
}

/// A cancel the venue refused for any reason but "not found" leaves the
/// order possibly RESTING: the row stays live, cancel-all reads Working
/// — never "gone" (the fail-open the review caught).
#[test]
fn a_refused_cancel_keeps_the_order_working() {
    let (port, tls, v, done) = boot((200, OPEN.to_vec()));
    v.lock().unwrap().delete_reply = Some((400, br#"{"code":"NONCE_USED","message":"nonce already used"}"#.to_vec()));
    let mut a = arm(port, tls);
    a.reconcile().expect("seed");
    a.place_resting(&order(5)).expect("accepted");
    assert_eq!(a.live_orders(), 1);
    let mut req = CancelReq::new(core_time::now_ns(), VenueId::Hypercall, SYM, 5);
    req.strategy_id = 7;
    assert_eq!(a.cancel(&req), Err(DispatchError::Http(400)));
    assert_eq!(a.live_orders(), 1, "still working");
    assert!(a.cancel_all().is_err());
    assert_eq!(a.cancel_all_state(), clob_dispatcher::CancelAllState::Working);
    assert_eq!(a.last_reason(), b"nonce already used");
    // "not found" IS gone.
    v.lock().unwrap().delete_reply = Some((200, br#"{"success":false,"data":null,"error":"order not found"}"#.to_vec()));
    assert!(a.cancel_all().is_ok());
    assert_eq!(a.cancel_all_state(), clob_dispatcher::CancelAllState::Clear);
    done.store(true, Ordering::Release);
}
