// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Integration test (PM15-9b): `Pm15Discovery` against a real `127.0.0.1`
//! keep-alive TLS server standing in for BOTH Gamma (`/events?slug=`) and
//! the CLOB (`/clob-markets/<conditionId>`) — the rcgen harness of the
//! crate's other loopback.
//!
//! | test | asserts |
//! |---|---|
//! | tick | the CLOB's `mts` and `mos` override Gamma's tick and minimum; a Gamma-only discovery never asks the CLOB |
//! | refusals | a CLOB breach (an unparsable 200 included) refuses the window by cause with `out` untouched; a CLOB 404 is a fetch failure |
//! | change | a law change mid-session refuses the family's next windows (no spec handed over), counted `venue_law_changed` once per change |
//! | garbage | an unparsable 200 after a pass is a refusal AND a change |
//! | tee | a refused window reaches neither the ingress nor slot 9's tee; an admitted one reaches both |
//! | 5 m | the 5-minute pipeline reads and obeys the CLOB the same way |
//!
//! Offline-path doctrine: this test allocates freely.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use core_net::HttpsReq;
use core_ring::Ring;
use core_types::pm_roll::{pm_window_of_open, PmPeriod};
use core_ring::Consumer;
use ingress_polymarket::gamma15::{PmLawErr, PmWindowSpec};
use ingress_polymarket::pm15_discovery::{FetchErr, Pm15Discovery};
use ingress_polymarket::pm15_loop::{Pm15Counter, Pm15Counters, PM15_SPEC_RING};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// A 15-minute window's open (the gamma15 tests' live window).
const OPEN: u64 = 1_792_217_700;
/// A 5-minute window's open (the gamma15 tests' 5 m window).
const OPEN5: u64 = 1_792_308_000;
const UP: &str = "34093056663108462497544643362722002462217972423461710020380621393557500264758";
const DOWN: &str = "88467165377337597854389991157504062307411366248534291856206167172694971782672";

/// What the CLOB answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clob {
    /// The live 2026-10-29 record.
    Live,
    /// The live record on the 0.001 grid with a 2.5-share minimum.
    FineTick,
    /// A 200 whose body is no record (a CDN error page).
    Garbage,
    /// No crypto taker hold.
    NoHold,
    /// Half the fee.
    HalfFee,
    /// Another market's condition id.
    OtherMarket,
    /// 404.
    NotFound,
}

struct Venue {
    clob: Mutex<Clob>,
    seen: Mutex<Vec<String>>,
}

struct Server {
    port: u16,
    client_cfg: Arc<ClientConfig>,
    venue: Arc<Venue>,
}

impl Server {
    fn set(&self, mode: Clob) {
        *self.venue.clob.lock().unwrap() = mode;
    }

    fn seen(&self) -> Vec<String> {
        self.venue.seen.lock().unwrap().clone()
    }
}

/// The condition id the fake Gamma gives window `open`'s market.
fn cid_of(open: u64) -> String {
    format!("0x{open:064x}")
}

/// Gamma's one-event answer for btc's `dur` (`15m` / `5m`) window `open`
/// (the gamma15 tests' live answer, trimmed, under its own slug, duration
/// and condition id).
fn gamma_body(dur: &str, open: u64) -> String {
    let cid = cid_of(open);
    format!(
        r#"[{{"id":"1198359","slug":"btc-updown-{dur}-{open}","active":true,"closed":false,"negRisk":false,"enableOrderBook":true,"markets":[{{"id":"5768487","conditionId":"{cid}","slug":"btc-updown-{dur}-{open}","outcomes":"[\"Up\", \"Down\"]","active":true,"closed":false,"enableOrderBook":true,"orderPriceMinTickSize":0.01,"orderMinSize":5,"clobTokenIds":"[\"{UP}\", \"{DOWN}\"]","acceptingOrders":true,"negRisk":false,"feesEnabled":true,"feeSchedule":{{"exponent":1,"rate":0.07,"takerOnly":true,"rebateRate":0.2}},"cryptoMarketConfig":{{"id":"btc-{dur}-twap-60","asset":"btc","duration":"{dur}","twapEnabled":true,"twapLookbackSeconds":60}},"version":"v1"}}],"version":"v1"}}]"#
    )
}

/// The CLOB's record for `cid` under `mode`.
fn clob_body(cid: &str, mode: Clob) -> String {
    let c = if mode == Clob::OtherMarket {
        cid_of(1)
    } else {
        cid.to_string()
    };
    let itode = if mode == Clob::NoHold { "false" } else { "true" };
    let rate = if mode == Clob::HalfFee { "0.035" } else { "0.07" };
    let mts = if mode == Clob::FineTick { "0.001" } else { "0.01" };
    let mos = if mode == Clob::FineTick { "2.5" } else { "5" };
    format!(
        r#"{{"r":{{"mi":50,"ma":4.5,"moas":30}},"t":[{{"t":"{UP}","o":"Up"}},{{"t":"{DOWN}","o":"Down"}}],"c":"{c}","mos":{mos},"mts":{mts},"mbf":1000,"tbf":1000,"ao":true,"aot":"2026-10-28T09:52:49Z","itode":{itode},"fd":{{"r":{rate},"e":1,"to":true}},"v":"v1"}}"#
    )
}

fn answer(v: &Venue, target: &str) -> (u16, String) {
    for dur in ["15m", "5m"] {
        let head = format!("/events?slug=btc-updown-{dur}-");
        if let Some(open) = target.strip_prefix(head.as_str()) {
            return match open.parse::<u64>() {
                Ok(o) => (200, gamma_body(dur, o)),
                Err(_) => (200, String::from("[]")),
            };
        }
    }
    if let Some(cid) = target.strip_prefix("/clob-markets/") {
        let mode = *v.clob.lock().unwrap();
        if mode == Clob::NotFound {
            return (404, String::from(r#"{"error":"market not found"}"#));
        }
        if mode == Clob::Garbage {
            return (200, String::from("<html><body>502 Bad Gateway</body></html>"));
        }
        return (200, clob_body(cid, mode));
    }
    (404, String::from("{}"))
}

fn serve() -> Server {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der: CertificateDer<'static> = cert.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).expect("key");
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let venue = Arc::new(Venue {
        clob: Mutex::new(Clob::Live),
        seen: Mutex::new(Vec::new()),
    });
    let v = Arc::clone(&venue);
    thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            // One thread per connection: the Gamma and the CLOB clients
            // each keep theirs alive.
            let (cfg, v) = (Arc::clone(&server_cfg), Arc::clone(&v));
            thread::spawn(move || handle(sock, &cfg, &v));
        }
    });
    Server {
        port,
        client_cfg,
        venue,
    }
}

/// Serve one keep-alive connection of bodiless GETs until the client
/// leaves.
fn handle(mut sock: std::net::TcpStream, cfg: &Arc<ServerConfig>, v: &Venue) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut conn = ServerConnection::new(Arc::clone(cfg)).expect("conn");
    let mut tls = Stream::new(&mut conn, &mut sock);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let he = loop {
            if let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break he;
            }
            match tls.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..he]).to_string();
        buf.drain(..he + 4);
        let target = head
            .split("\r\n")
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .unwrap_or("")
            .to_string();
        let (status, body) = answer(v, &target);
        v.seen.lock().unwrap().push(target);
        let reason = if status == 200 { "OK" } else { "Not Found" };
        let out = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        if tls.write_all(out.as_bytes()).is_err() || tls.flush().is_err() {
            return;
        }
    }
}

fn client(s: &Server) -> HttpsReq {
    HttpsReq::new("localhost", s.port, Arc::clone(&s.client_cfg), 512, 0, 64 * 1024)
        .expect("client")
}

/// A discovery of `period` for btc reading the CLOB on the loopback.
fn discovery(s: &Server, period: PmPeriod) -> Pm15Discovery {
    let mut d = Pm15Discovery::with_client_for(period, client(s), &[b"btc"]);
    d.set_clob(client(s));
    d
}

/// Wall time 1 s into window `w + k` of `period`, ns.
fn wall(period: PmPeriod, w: u32, k: u32) -> u64 {
    (period.window_open_s(w + k) + 1) * 1_000_000_000
}

/// Every spec on `rx`, as `(window, tick_1e6, min_size_1e6)`.
fn drain(rx: &mut Consumer<PmWindowSpec, PM15_SPEC_RING>) -> Vec<(u32, i64, i64)> {
    let mut got = Vec::new();
    while let Some(p) = rx.try_pop_ref() {
        got.push((p.window, p.tick_1e6, p.min_size_1e6));
    }
    got
}

#[test]
fn the_clob_tick_overrides_gammas_and_a_discovery_without_a_clob_never_asks_it() {
    let s = serve();
    let w = pm_window_of_open(OPEN).expect("grid");
    s.set(Clob::FineTick);
    // A test discovery: Gamma's law alone, Gamma's tick.
    let mut d = Pm15Discovery::with_client_for(PmPeriod::M15, client(&s), &[b"btc"]);
    let mut out = PmWindowSpec::EMPTY;
    d.fetch(0, w, &mut out).expect("Gamma's law holds");
    assert_eq!((out.tick_1e6, out.min_size_1e6), (10_000, 5_000_000), "Gamma's");
    assert!(
        s.seen().iter().all(|t| !t.starts_with("/clob-markets/")),
        "no CLOB client, no CLOB request"
    );
    // With the CLOB: its record is read by the condition id, and its tick
    // (0.001) and minimum (2.5) replace Gamma's (0.01, 5).
    d.set_clob(client(&s));
    let mut out = PmWindowSpec::EMPTY;
    d.fetch(0, w, &mut out).expect("both laws hold");
    assert_eq!(out.tick_1e6, 1_000, "the CLOB's tick overrides Gamma's");
    assert_eq!(out.min_size_1e6, 2_500_000, "the CLOB's minimum overrides Gamma's");
    assert_eq!(out.condition_id(), cid_of(OPEN).as_bytes());
    assert_eq!((out.family, out.window), (0, w));
    assert_eq!(
        s.seen().last().map(String::as_str),
        Some(format!("/clob-markets/{}", cid_of(OPEN)).as_str())
    );
    s.set(Clob::Live);
    d.fetch(0, w, &mut out).expect("both laws hold");
    assert_eq!((out.tick_1e6, out.min_size_1e6), (10_000, 5_000_000));
}

#[test]
fn a_clob_breach_refuses_the_window_by_cause_and_leaves_out_untouched() {
    let s = serve();
    let w = pm_window_of_open(OPEN).expect("grid");
    let mut d = Pm15Discovery::with_client_for(PmPeriod::M15, client(&s), &[b"btc"]);
    d.set_clob(client(&s));
    let cases = [
        (Clob::NoHold, Err(FetchErr::Law(PmLawErr::ClobHold))),
        (Clob::HalfFee, Err(FetchErr::Law(PmLawErr::ClobFee))),
        (Clob::OtherMarket, Err(FetchErr::Law(PmLawErr::ClobShape))),
        (Clob::Garbage, Err(FetchErr::Law(PmLawErr::ClobShape))),
        (Clob::NotFound, Err(FetchErr::Fetch)),
        (Clob::Live, Ok(())),
    ];
    for (mode, want) in cases {
        s.set(mode);
        let mut out = PmWindowSpec::EMPTY;
        assert_eq!(d.fetch(0, w, &mut out), want, "{mode:?}");
        if want.is_err() {
            assert_eq!(out, PmWindowSpec::EMPTY, "{mode:?}: untouched");
        } else {
            assert_eq!(out.window, w);
        }
    }
}

#[test]
fn a_law_change_mid_session_stops_the_family_and_is_counted_once() {
    let s = serve();
    let w = pm_window_of_open(OPEN).expect("grid");
    let mut d = Pm15Discovery::with_client_for(PmPeriod::M15, client(&s), &[b"btc"]);
    d.set_clob(client(&s));
    let (mut tx, mut rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let c = Pm15Counters::new();
    let stop = AtomicBool::new(false);
    // Wall time 1 s into window `w + k`.
    let wall = |k: u32| (PmPeriod::M15.window_open_s(w + k) + 1) * 1_000_000_000;
    let n = |k: Pm15Counter| c.of(k);

    // Boot: the current window and the next, both laws holding.
    d.pass(wall(0), &mut tx, &c, &stop);
    assert_eq!(n(Pm15Counter::DiscoveryOk), 2);
    // The venue halves its fee: the next window is refused — the family
    // is dormant for it — and the change is counted.
    s.set(Clob::HalfFee);
    d.pass(wall(1), &mut tx, &c, &stop);
    assert_eq!(n(Pm15Counter::DiscoveryOk), 2, "no spec handed over");
    assert_eq!(n(Pm15Counter::DiscoveryLawRefused), 1);
    assert_eq!(n(Pm15Counter::VenueLawChanged), 1);
    assert_eq!(c.last_law_refusal(), Some((0, PmLawErr::ClobFee as u8)));
    // Still changed a window later: refused again, the same change.
    d.pass(wall(2), &mut tx, &c, &stop);
    assert_eq!(n(Pm15Counter::DiscoveryLawRefused), 2);
    assert_eq!(n(Pm15Counter::VenueLawChanged), 1);
    // The law holds again: the family's next window is admitted.
    s.set(Clob::Live);
    d.pass(wall(3), &mut tx, &c, &stop);
    assert_eq!(n(Pm15Counter::DiscoveryOk), 3);
    // Then the hold goes: a second change.
    s.set(Clob::NoHold);
    d.pass(wall(4), &mut tx, &c, &stop);
    assert_eq!(n(Pm15Counter::VenueLawChanged), 2);
    assert_eq!(c.last_law_refusal(), Some((0, PmLawErr::ClobHold as u8)));
    // Exactly the admitted windows reached the ingress, on the CLOB's tick.
    let mut got = Vec::new();
    while let Some(p) = rx.try_pop_ref() {
        got.push((p.window, p.tick_1e6));
    }
    assert_eq!(got, vec![(w, 10_000), (w + 1, 10_000), (w + 4, 10_000)]);
}

#[test]
fn an_unparsable_200_after_a_pass_is_a_refusal_and_a_change() {
    let s = serve();
    let p = PmPeriod::M15;
    let w = pm_window_of_open(OPEN).expect("grid");
    let mut d = discovery(&s, p);
    let (mut tx, mut rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let c = Pm15Counters::new();
    let stop = AtomicBool::new(false);
    d.pass(wall(p, w, 0), &mut tx, &c, &stop);
    assert_eq!(c.of(Pm15Counter::DiscoveryOk), 2);
    // A CDN page where the record should be: the next window is refused
    // for its shape, and — after a pass — that is a change.
    s.set(Clob::Garbage);
    d.pass(wall(p, w, 1), &mut tx, &c, &stop);
    assert_eq!(c.of(Pm15Counter::DiscoveryLawRefused), 1);
    assert_eq!(c.of(Pm15Counter::DiscoveryFetchErrors), 0, "a 200 is no transport failure");
    assert_eq!(c.of(Pm15Counter::VenueLawChanged), 1);
    assert_eq!(c.last_law_refusal(), Some((0, PmLawErr::ClobShape as u8)));
    assert_eq!(c.last_law_change(), Some((0, PmLawErr::ClobShape as u8)));
    assert_eq!(drain(&mut rx), vec![(w, 10_000, 5_000_000), (w + 1, 10_000, 5_000_000)]);
}

#[test]
fn a_refused_window_reaches_neither_the_ingress_nor_the_tee() {
    let s = serve();
    let p = PmPeriod::M15;
    let w = pm_window_of_open(OPEN).expect("grid");
    let mut d = discovery(&s, p);
    let (tee_tx, mut tee_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    d.set_tee(tee_tx);
    let (mut tx, mut rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let c = Pm15Counters::new();
    let stop = AtomicBool::new(false);
    // No hold from the boot: both windows refused, neither handed over —
    // and no change (the family never passed).
    s.set(Clob::NoHold);
    d.pass(wall(p, w, 0), &mut tx, &c, &stop);
    assert_eq!(c.of(Pm15Counter::DiscoveryOk), 0);
    assert_eq!(c.of(Pm15Counter::DiscoveryLawRefused), 2);
    assert_eq!(c.of(Pm15Counter::VenueLawChanged), 0);
    assert!(drain(&mut rx).is_empty(), "nothing to the ingress");
    assert!(drain(&mut tee_rx).is_empty(), "nothing to slot 9's gateway");
    // The law holds: the next window reaches both (the refused one waits
    // out its retry).
    s.set(Clob::Live);
    d.pass(wall(p, w, 1), &mut tx, &c, &stop);
    assert_eq!(drain(&mut rx), vec![(w + 2, 10_000, 5_000_000)]);
    assert_eq!(drain(&mut tee_rx), vec![(w + 2, 10_000, 5_000_000)]);
    assert_eq!(c.of(Pm15Counter::DiscoveryTeeFull), 0);
}

#[test]
fn the_five_minute_pipeline_reads_and_obeys_the_clob() {
    let s = serve();
    let p = PmPeriod::M5;
    let w = p.window_of_open(OPEN5).expect("grid");
    let mut d = discovery(&s, p);
    // One window: the 5 m slug on Gamma, then the CLOB's own record.
    s.set(Clob::FineTick);
    let mut out = PmWindowSpec::EMPTY;
    d.fetch(0, w, &mut out).expect("both 5 m laws hold");
    assert_eq!((out.window, out.tick_1e6, out.min_size_1e6), (w, 1_000, 2_500_000));
    let seen = s.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], format!("/events?slug=btc-updown-5m-{OPEN5}"));
    assert_eq!(seen[1], format!("/clob-markets/{}", cid_of(OPEN5)));
    s.set(Clob::HalfFee);
    let mut out = PmWindowSpec::EMPTY;
    assert_eq!(d.fetch(0, w, &mut out), Err(FetchErr::Law(PmLawErr::ClobFee)));
    assert_eq!(out, PmWindowSpec::EMPTY);
    // Passes: the current window and the two ahead, then a change.
    let (mut tx, mut rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let c = Pm15Counters::new();
    let stop = AtomicBool::new(false);
    s.set(Clob::Live);
    d.pass(wall(p, w, 0), &mut tx, &c, &stop);
    assert_eq!(c.of(Pm15Counter::DiscoveryOk), 3);
    s.set(Clob::NoHold);
    d.pass(wall(p, w, 1), &mut tx, &c, &stop);
    assert_eq!(c.of(Pm15Counter::VenueLawChanged), 1);
    assert_eq!(c.last_law_change(), Some((0, PmLawErr::ClobHold as u8)));
    let got = drain(&mut rx);
    assert_eq!(
        got.iter().map(|g| g.0).collect::<Vec<_>>(),
        vec![w, w + 1, w + 2],
        "w + 3 was refused"
    );
}
