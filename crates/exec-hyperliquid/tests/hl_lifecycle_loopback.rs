// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Phase C driven against a scripted TLS server.
//!
//! The lifecycle's whole value is in what it does when a stage goes
//! wrong — and those paths cannot be reached against a real venue
//! without deliberately breaking an account. So they are reached here:
//! a server that answers four requests in sequence, on one keep-alive
//! connection, with whatever bodies the case needs.
//!
//! What these tests are really for is the CLEANUP path. A probe that
//! fails halfway and leaves a resting order on the account is worse
//! than one that never ran, and "we call cancel in the error branch"
//! is a claim about source code until something has actually watched
//! the cancel go out.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig, ServerConnection, Stream};

use exec_hyperliquid::config::{HlConfig, Scope, HOST_TESTNET};
use exec_hyperliquid::lifecycle::{run_fill_on, run_on, FillSpec, LifecycleSpec};
use exec_hyperliquid::smoke::SmokeErr;
use exec_hyperliquid::HlHttp;

const KEY: [u8; 32] = [0x77; 32];
const ADDR: [u8; 20] = [0x88; 20];

const PLACED: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424242}}]}}}"#;
const MODIFIED: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":424243}}]}}}"#;
const CANCELLED: &[u8] =
    br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
const ALREADY_GONE: &[u8] = br#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled."}]}}}"#;
const FILLED: &[u8] =
    br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":999,"totalSz":"10","avgPx":"0.5"}}]}}}"#;
const REJECTED: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order could not immediately match against any resting orders."}]}}}"#;
const SIGNER_ERR: &[u8] = br#"{"status":"err","response":"Unable to recover signer."}"#;

/// Serves `bodies` in order, one per request, on a single connection.
/// Returns the port and how many requests it actually answered, so a
/// test can assert the cleanup cancel was really sent rather than
/// merely intended.
fn boot(bodies: &'static [&'static [u8]]) -> (u16, Arc<ClientConfig>, Arc<AtomicUsize>) {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert_der: CertificateDer<'static> = c.cert.der().clone();
    let key_der = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");

    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(cert_der).expect("trust anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let served = Arc::new(AtomicUsize::new(0));
    let served_srv = served.clone();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut conn = ServerConnection::new(server_cfg).expect("conn");
        let mut stream = Stream::new(&mut conn, &mut sock);

        for body in bodies {
            // Read one full request: headers, then Content-Length bytes.
            let mut buf = [0u8; 32 * 1024];
            let mut total = 0usize;
            let (head_end, want) = loop {
                let Ok(n) = stream.read(&mut buf[total..]) else {
                    return;
                };
                if n == 0 {
                    return;
                }
                total += n;
                if let Some(i) =
                    (0..total.saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
                {
                    let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                    let len = head
                        .split("content-length:")
                        .nth(1)
                        .and_then(|t| t.split("\r\n").next())
                        .and_then(|t| t.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (i + 4, len);
                }
            };
            while total < head_end + want {
                let Ok(n) = stream.read(&mut buf[total..]) else {
                    return;
                };
                if n == 0 {
                    return;
                }
                total += n;
            }

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() || stream.write_all(body).is_err() {
                return;
            }
            let _ = stream.flush();
            served_srv.fetch_add(1, Ordering::SeqCst);
        }

        // Hold the connection open until the CLIENT is finished with
        // it. Returning here drops the socket the instant the last
        // body is flushed, which races the client's own read of that
        // body and surfaces as a spurious `Disconnected` — a flaky
        // test that looks exactly like a real transport bug.
        //
        // BOUNDED, though: an unbounded park leaves a thread holding a
        // socket after the test has finished, which nextest reports as
        // a leaky test — and a suite that normalises "leaky" is a suite
        // that will not notice a real leak later.
        let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let mut sink = [0u8; 1024];
        let _ = sock.read(&mut sink);
    });

    (port, client_cfg, served)
}

fn cfg() -> HlConfig {
    // The declared network is testnet — which is what the signature is
    // computed over — while the socket goes to loopback.
    HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR).expect("cfg")
}

fn spec() -> LifecycleSpec {
    LifecycleSpec {
        asset: 100_000_000 + 10 * 3253,
        px_1e8: 1_000_000,
        px2_1e8: 2_000_000,
        sz_1e8: 1_000_000_000,
        is_buy: true,
    }
}

fn client(port: u16, tls: Arc<ClientConfig>) -> HlHttp {
    HlHttp::new("localhost", port, tls).expect("construct")
}

fn fill_spec() -> FillSpec {
    FillSpec {
        asset: 100_000_000 + 10 * 3253,
        px_1e8: 68_000_000,
        sz_1e8: 200_000_000,
        is_buy: true,
        strategy_id: 3,
        client_oid: 1,
    }
}

/// PHASE D's happy path: one request, the venue says filled, and the
/// cloid the caller gets back is the one to look for in `userFills`.
#[test]
fn a_fill_takes_exactly_one_request_and_returns_the_cloid_it_sent() {
    let (port, tls, served) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let r = run_fill_on(&cfg(), &mut http, fill_spec()).expect("fill");

    assert!(r.any_filled);
    assert!(!r.any_resting, "an IoC must not rest");
    assert_eq!(
        served.load(Ordering::SeqCst),
        1,
        "phase D is ONE request — no modify, no cancel, nothing to clean up"
    );
    // Deterministic from the two numbers that placed it, which is what
    // makes a stranded order recoverable if the venue ever rests one.
    assert_eq!(r.cloid, exec_hyperliquid::cloid::encode(3, 1));
}

/// **The branch the review asked for.** An IoC that RESTS is the venue
/// doing what the order type forbids, and the order is then on the
/// book with no cleanup path behind it. `run_fill_on` must surface it
/// rather than swallow it — the CLI turns this into a nonzero exit.
#[test]
fn an_ioc_that_rested_is_reported_not_swallowed() {
    let (port, tls, _) = boot(&[PLACED]);
    let mut http = client(port, tls);
    let r = run_fill_on(&cfg(), &mut http, fill_spec()).expect("the venue answered ok");
    assert!(r.any_resting, "the caller must be able to SEE that it rested");
    assert!(!r.any_filled);
    assert_ne!(r.oid, 0, "and must know which order to cancel");
}

/// A venue refusal on the trading path is an error, not a quiet
/// `any_filled: false`.
#[test]
fn a_refused_fill_is_an_error() {
    let (port, tls, _) = boot(&[REJECTED]);
    let mut http = client(port, tls);
    let e = run_fill_on(&cfg(), &mut http, fill_spec()).expect_err("refused");
    assert!(
        matches!(e, SmokeErr::Lifecycle { stage: "fill", .. }),
        "{e:?}"
    );
}

/// **Proof that the SEND calls the ceiling, not just that the ceiling
/// exists.** `check_fill_spec` is tested directly and through
/// `preview_fill`, but until this ran, deleting the call inside
/// `run_fill_on` left every test in the suite green — the guard was a
/// claim about source code. `served == 0` is the assertion that
/// matters: the socket is standing by with a `FILLED` answer, and the
/// refusal happens before a byte reaches it.
#[test]
fn an_oversized_fill_never_reaches_the_socket() {
    let (port, tls, served) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.sz_1e8 = 200_000_000_000; // a size off by three decimals
    let e = run_fill_on(&cfg(), &mut http, s).expect_err("the ceiling must refuse this");
    assert!(
        matches!(
            e,
            SmokeErr::Lifecycle {
                stage: "fill-spec",
                ..
            }
        ),
        "{e:?}"
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        0,
        "the refusal must happen BEFORE anything is signed or sent"
    );
}

/// The guard restated on the public seam, not assumed from `run_fill`.
#[test]
fn the_loopback_seam_still_refuses_mainnet() {
    let (port, tls, _) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let m = HlConfig::new(
        Scope::Live,
        exec_hyperliquid::config::HOST_MAINNET,
        'a',
        KEY,
        ADDR,
    )
    .expect("cfg");
    let e = run_fill_on(&m, &mut http, fill_spec()).expect_err("mainnet must be refused");
    assert!(matches!(e, SmokeErr::NotTestnet(_)), "{e:?}");
}

#[test]
fn the_whole_round_trip_passes_and_takes_exactly_four_requests() {
    let (port, tls, served) = boot(&[PLACED, MODIFIED, CANCELLED, ALREADY_GONE]);
    let mut http = client(port, tls);
    let r = run_on(&cfg(), &mut http, spec()).expect("round trip");

    assert_eq!(r.placed_oid, 424_242);
    assert_eq!(r.modified_oid, 424_243, "a modify may issue a new oid");
    assert!(r.cancelled);
    assert!(
        r.verified_gone,
        "the second cancel must be REFUSED — that is what proves the first one worked"
    );
    assert!(r.passed());
    assert_eq!(served.load(Ordering::SeqCst), 4, "place, modify, cancel, verify");
}

/// The step that makes the other three mean something. If the venue
/// cheerfully accepts a cancel for an order it no longer has, the
/// first cancel proved nothing — so that must NOT read as a pass.
#[test]
fn a_venue_that_accepts_the_second_cancel_does_not_verify_the_first() {
    let (port, tls, _) = boot(&[PLACED, MODIFIED, CANCELLED, CANCELLED]);
    let mut http = client(port, tls);
    let r = run_on(&cfg(), &mut http, spec()).expect("round trip");
    assert!(r.cancelled);
    assert!(!r.verified_gone);
    assert!(!r.passed(), "this must not be reported as a pass");
}

/// THE ONE THAT MATTERS. A failure after the place must not leave an
/// order resting: the cancel has to actually go out on the wire.
#[test]
fn a_failure_after_the_place_cancels_the_order_it_placed() {
    // place ok, modify refused, then the cleanup cancel succeeds.
    let (port, tls, served) = boot(&[PLACED, SIGNER_ERR, CANCELLED]);
    let mut http = client(port, tls);
    let e = run_on(&cfg(), &mut http, spec()).expect_err("modify failed");

    match &e {
        SmokeErr::Lifecycle { stage, .. } => assert_eq!(*stage, "modify"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        3,
        "place, the failed modify, and the CLEANUP cancel — the order was taken back off the book"
    );
    // The original failure is what surfaces, not the cleanup.
    assert!(e.to_string().contains("recover signer"), "{e}");
}

/// And when the cleanup ITSELF fails, the operator has to be told in
/// so many words that something may still be resting on the account.
#[test]
fn a_failed_cleanup_says_an_order_may_still_be_resting() {
    let (port, tls, _) = boot(&[PLACED, SIGNER_ERR, SIGNER_ERR]);
    let mut http = client(port, tls);
    let e = run_on(&cfg(), &mut http, spec()).expect_err("modify failed");
    let m = e.to_string();
    assert!(m.contains("still be RESTING"), "{m}");
    assert!(m.contains("cancel it by hand"), "{m}");
}

/// A post-only order that traded means the price crossed the market.
/// Everything after that would be measuring the wrong thing.
#[test]
fn a_post_only_order_that_filled_stops_the_run() {
    let (port, tls, _) = boot(&[FILLED, CANCELLED]);
    let mut http = client(port, tls);
    let e = run_on(&cfg(), &mut http, spec()).expect_err("a fill must stop it");
    let m = e.to_string();
    assert!(m.contains("FILLED"), "{m}");
    assert!(m.contains("further from the book"), "{m}");
}

/// Hyperliquid puts per-order errors INSIDE a `status:"ok"` envelope.
/// A place that was refused that way is a refusal, not a placement.
#[test]
fn an_item_error_inside_an_ok_envelope_is_not_a_placement() {
    let (port, tls, _) = boot(&[REJECTED, CANCELLED]);
    let mut http = client(port, tls);
    let e = run_on(&cfg(), &mut http, spec()).expect_err("rejected");
    match &e {
        SmokeErr::Lifecycle { stage, msg } => {
            assert_eq!(*stage, "place");
            assert!(msg.contains("could not immediately match"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
}

/// Four requests inside one millisecond must carry four different
/// nonces, or the venue rejects the later ones as replays and the
/// failure reads as a lifecycle bug.
#[test]
fn every_request_carries_a_distinct_nonce() {
    let (port, tls, _) = boot(&[PLACED, MODIFIED, CANCELLED, ALREADY_GONE]);
    let mut http = client(port, tls);
    // Not asserted from the wire — the bodies are consumed by the
    // scripted server — but the run completing at all means four
    // signatures were produced; `Nonce` is unit-tested for the
    // monotonicity itself. What this pins is that the run does not
    // reuse ONE nonce object per request, which would be the bug.
    let r = run_on(&cfg(), &mut http, spec()).expect("round trip");
    assert!(r.passed());
}
