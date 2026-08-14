//! Integration test: RSS poller against a real 127.0.0.1 HTTP/1.1
//! server.
//!
//! Boots a plain TCP listener on an ephemeral port, scripts a single
//! 200 OK + Content-Length response carrying a two-item RSS body,
//! drives the [`FetchDriver`] state machine against the real socket
//! via [`PlainTcpTransport`], and asserts that the body parses into
//! two unique signals — proving that the run-loop, the HTTP/1.1
//! codec, and the SPSC ring all hand off correctly on a real socket.
//!
//! This is the highest-leverage integration check we have. The
//! corresponding TLS-loopback tests for Polymarket / Binance / RPC
//! follow the same pattern but add a rustls server + rcgen self-
//! signed cert; those are tracked separately.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use core_net::PlainTcpTransport;
use core_ring::Ring;
use core_types::Signal;
use ingress_rss::poller::{drive_one_fetch, FeedCfg, FetchDriver, FetchState};
use ingress_rss::{parse_body_into_signals, SeenRing};

/// Body we'll send back from the loopback server. Two unique RSS
/// items → two signals.
const RSS_BODY: &[u8] = br#"<rss><channel>
<item><title>first</title><link>https://news.example/1</link></item>
<item><title>second</title><link>https://news.example/2</link></item>
</channel></rss>"#;

/// Drives a single fetch against a real socket. Returns the parsed
/// body bytes.
fn fetch_loopback_body() -> Vec<u8> {
    // 1. Bind the loopback server on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let body = RSS_BODY.to_vec();
    let body_len = body.len();

    // 2. Spawn the server thread.
    let server = thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept");
        // Read the client's GET request — we ignore its contents,
        // we just need to know the request came through.
        let mut scratch = [0u8; 4096];
        let mut total = 0;
        // Read until the blank line (\r\n\r\n).
        loop {
            let n = sock.read(&mut scratch[total..]).expect("server read");
            if n == 0 {
                break;
            }
            total += n;
            if scratch[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if total == scratch.len() {
                panic!("oversized request");
            }
        }
        // Write 200 OK with Content-Length framing.
        let resp_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\nContent-Type: application/rss+xml\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(resp_head.as_bytes()).expect("server write head");
        sock.write_all(&body).expect("server write body");
        let _ = sock.shutdown(Shutdown::Both);
    });

    // 3. Drive the client.
    let transport = PlainTcpTransport::connect(addr).expect("client connect");
    let mut transport = transport;
    let feed = FeedCfg::new(b"127.0.0.1", b"/feed", 60_000_000_000);

    let mut drv = FetchDriver::new();

    // Drive until Done — bounded iteration count keeps a hanging
    // server from wedging the test.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !matches!(drv.state(), FetchState::Done | FetchState::Error) {
        if std::time::Instant::now() > deadline {
            panic!("timed out driving FetchDriver to Done/Error");
        }
        let _ = drive_one_fetch(&mut transport, &mut drv, &feed);
        thread::sleep(Duration::from_millis(1));
    }

    server.join().expect("server thread");
    assert_eq!(
        drv.state(),
        FetchState::Done,
        "expected Done, got {:?}",
        drv.state()
    );
    assert_eq!(drv.status(), 200);
    drv.body().to_vec()
}

#[test]
fn rss_http1_loopback_yields_two_signals() {
    let body = fetch_loopback_body();
    // The body we read back must match the script byte-for-byte.
    assert_eq!(body.as_slice(), RSS_BODY);

    // Now feed it through the same parser the live poller uses.
    let feed = FeedCfg::new(b"127.0.0.1", b"/feed", 60_000_000_000);
    let mut seen: SeenRing<64> = SeenRing::new();
    let ring: Arc<Ring<Signal, 128>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    let n = parse_body_into_signals(&body, &feed, &mut seen, &mut prod);
    assert_eq!(n, 2, "two unique items in the canned body");

    let mut got = 0;
    while cons.try_pop().is_some() {
        got += 1;
    }
    assert_eq!(got, 2, "two signals must reach the ring");
}

#[test]
fn rss_http1_loopback_dedupes_on_repeat() {
    let body = fetch_loopback_body();
    let feed = FeedCfg::new(b"127.0.0.1", b"/feed", 60_000_000_000);
    let mut seen: SeenRing<64> = SeenRing::new();
    let ring: Arc<Ring<Signal, 128>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    let first = parse_body_into_signals(&body, &feed, &mut seen, &mut prod);
    let second = parse_body_into_signals(&body, &feed, &mut seen, &mut prod);
    assert_eq!(first, 2);
    assert_eq!(second, 0, "second pass must dedupe via SeenRing");

    let mut got = 0;
    while cons.try_pop().is_some() {
        got += 1;
    }
    assert_eq!(got, 2);
}
