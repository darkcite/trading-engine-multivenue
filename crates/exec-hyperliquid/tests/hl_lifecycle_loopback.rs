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
use exec_hyperliquid::lifecycle::{
    run_fill_on, run_on, run_requote_on, run_sweep_on, FillSpec, LifecycleSpec, SweepSpec,
};
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
        repeat: 1,
    }
}

/// PHASE D's happy path: one request, the venue says filled, and the
/// cloid the caller gets back is the one to look for in `userFills`.
#[test]
fn a_fill_takes_exactly_one_request_and_returns_the_cloid_it_sent() {
    let (port, tls, served) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let run = run_fill_on(&cfg(), &mut http, fill_spec()).expect("fill");
    let r = run.report;

    assert!(run.stopped.is_none(), "the batch finished");
    assert_eq!(r.filled, 1);
    assert_eq!(r.sent, 1);
    assert_eq!(r.attempted, 1);
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

/// A BATCH is N sequential orders, each its own signature, nonce and
/// ACK. Not one batched action: LAW E-5 says the response is the ACK,
/// and an ACK covering twenty orders tells you nothing about which one
/// the venue took.
#[test]
fn a_batch_places_one_request_per_order() {
    let (port, tls, served) = boot(&[FILLED, FILLED, FILLED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.repeat = 3;
    let run = run_fill_on(&cfg(), &mut http, s).expect("batch");
    let r = run.report;

    assert!(run.stopped.is_none());
    assert_eq!(r.attempted, 3);
    assert_eq!(r.sent, 3);
    assert_eq!(r.filled, 3);
    assert_eq!(served.load(Ordering::SeqCst), 3, "one request per order");
    // The LAST cloid is reported; the others are derivable from the
    // first client id, which is what makes a stranded order
    // recoverable.
    assert_eq!(r.cloid, exec_hyperliquid::cloid::encode(3, 3));
}

/// **The first refusal ENDS the batch — and the batch still reports
/// what it did.** The earlier shape accumulated into a local and
/// returned it only on the happy path, so the order that REALLY TRADED
/// before the refusal came back as an error string and nothing else.
/// This scripts exactly that: order 1 fills, order 2 is refused.
#[test]
fn a_refusal_stops_the_batch_but_never_hides_what_already_happened() {
    let (port, tls, served) = boot(&[FILLED, REJECTED, FILLED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.repeat = 3;
    let run = run_fill_on(&cfg(), &mut http, s).expect("a stopped batch is not an Err");

    let e = run.stopped.as_ref().expect("the refusal must surface");
    assert!(
        matches!(e, SmokeErr::Lifecycle { stage: "fill", .. }),
        "{e:?}"
    );
    // The point of the test: the completed fill is STILL REPORTED.
    assert_eq!(run.report.filled, 1, "order 1 really traded");
    assert_eq!(run.report.sent, 1, "and exactly one was ACKED");
    assert_eq!(run.report.attempted, 2, "two requests left the process");
    assert_eq!(run.report.cloid, exec_hyperliquid::cloid::encode(3, 1));
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "the third order must never have been sent"
    );
}

/// **The case the batch invented.** A venue that RESTS an IoC has done
/// what the order type forbids, leaving an order on the book with no
/// cleanup behind it — and if a LATER order is then refused, the whole
/// report used to be discarded with the error. `any_resting` must
/// survive the stop, or the operator is never told to go cancel it.
#[test]
fn a_rest_earlier_in_the_batch_survives_a_refusal_later() {
    let (port, tls, served) = boot(&[FILLED, PLACED, REJECTED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.repeat = 3;
    let run = run_fill_on(&cfg(), &mut http, s).expect("a stopped batch is not an Err");

    assert!(
        run.stopped.is_some(),
        "the third order's refusal ended the batch"
    );
    assert!(
        run.report.any_resting,
        "THE ASSERTION: an IoC is on the book and the caller must hear about it"
    );
    assert!(
        !run.report.in_doubt,
        "every request was answered — the rest is KNOWN, not doubted"
    );
    assert_eq!(run.report.filled, 1);
    assert_eq!(run.report.sent, 2, "the rest was ACKED too");
    assert_eq!(run.report.attempted, 3);
    assert_eq!(served.load(Ordering::SeqCst), 3);
}

/// **The branch the review asked for.** An IoC that RESTS is the venue
/// doing what the order type forbids, and the order is then on the
/// book with no cleanup path behind it. `run_fill_on` must surface it
/// rather than swallow it — the CLI turns this into a nonzero exit.
#[test]
fn an_ioc_that_rested_is_reported_not_swallowed() {
    let (port, tls, _) = boot(&[PLACED]);
    let mut http = client(port, tls);
    let r = run_fill_on(&cfg(), &mut http, fill_spec())
        .expect("the venue answered ok")
        .report;
    assert!(r.any_resting, "the caller must be able to SEE that it rested");
    assert_eq!(r.filled, 0);
    assert_ne!(r.oid, 0, "and must know which order to cancel");
}

/// A venue refusal on the trading path is surfaced, not a quiet
/// `filled: 0`. It rides in `stopped` rather than `Err` because `Err`
/// now means the stricter thing: **nothing left the process**. The
/// report proves that distinction — one request was attempted, none
/// ACKED.
#[test]
fn a_refused_fill_is_reported_as_a_stop_not_a_silent_zero() {
    let (port, tls, _) = boot(&[REJECTED]);
    let mut http = client(port, tls);
    let run = run_fill_on(&cfg(), &mut http, fill_spec()).expect("the venue answered");
    let e = run.stopped.as_ref().expect("refused");
    assert!(
        matches!(e, SmokeErr::Lifecycle { stage: "fill", .. }),
        "{e:?}"
    );
    assert_eq!(run.report.attempted, 1);
    assert_eq!(run.report.sent, 0, "a refusal is not an ACK");
    assert_eq!(run.report.filled, 0);
    // THE ASSERTION. A non-crossing price is refused exactly like
    // this, and it is the most common outcome of the whole command. An
    // operator must NOT be sent hunting for a phantom order on it: the
    // venue answered, and the answer was that it placed nothing. The
    // CLI's "an order may be ON THE BOOK" alarm reads this bit, and an
    // alarm that fires on the routine case is one nobody reads by the
    // twentieth run.
    assert!(
        !run.report.in_doubt,
        "the venue ANSWERED — a refusal is not doubt"
    );
}

/// The other half of that bit. A transport failure means the request
/// left and we never heard back — the venue may well have placed the
/// order. That IS doubt, and the alarm must fire.
///
/// The fixture: one scripted body, two orders. The second request goes
/// out, the server is out of answers and drops the socket.
#[test]
fn a_send_with_no_answer_is_the_case_the_alarm_exists_for() {
    let (port, tls, served) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.repeat = 2;
    let run = run_fill_on(&cfg(), &mut http, s).expect("a stopped batch is not an Err");

    assert!(
        matches!(run.stopped, Some(SmokeErr::Http(_))),
        "{:?}",
        run.stopped
    );
    assert!(
        run.report.in_doubt,
        "no answer came back — order 2 may be on the venue"
    );
    assert_eq!(run.report.attempted, 2, "and the range must cover it");
    assert_eq!(run.report.sent, 1);
    assert_eq!(run.report.filled, 1, "order 1 still traded");
    assert_eq!(served.load(Ordering::SeqCst), 1);
}

/// `Err` means NOTHING WAS SENT, and that has to be observable rather
/// than documented: the socket is standing by with an answer and never
/// gets a byte.
#[test]
fn an_err_from_the_fill_path_means_nothing_left_the_process() {
    let (port, tls, served) = boot(&[FILLED]);
    let mut http = client(port, tls);
    let mut s = fill_spec();
    s.client_oid = u64::MAX;
    s.repeat = 2; // the second id would wrap to 0
    let e = run_fill_on(&cfg(), &mut http, s).expect_err("the wrap must be refused");
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
    assert_eq!(served.load(Ordering::SeqCst), 0);
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


// ---- Phase F: the requote -------------------------------------------

/// **The shape the whole probe rests on.** A requote passes only when
/// the OLD cloid is gone AND the NEW one was really there. Four
/// requests: place, modify, and two cancels that ARE the assertion.
#[test]
fn a_requote_passes_only_when_the_order_actually_moved() {
    let (port, tls, served) = boot(&[PLACED, MODIFIED, ALREADY_GONE, CANCELLED]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("requote");

    assert!(r.passed());
    assert!(r.old_cancel_refused, "cancelling the old id was refused");
    assert!(r.new_cancel_succeeded, "cancelling the new id succeeded");
    assert_eq!(r.placed_oid, 424242);
    assert_eq!(served.load(Ordering::SeqCst), 4, "place, modify, verify, cleanup");
}

/// **A modify that left BOTH orders resting is a leak, not a requote.**
/// The old id still cancels, so `old_cloid_gone` is false — and a
/// verdict reading only `new_cloid_lived` would have called this a
/// pass while two quotes sat on the book under one intent.
#[test]
fn a_modify_that_left_both_orders_resting_is_not_a_requote() {
    let (port, tls, served) = boot(&[PLACED, MODIFIED, CANCELLED, CANCELLED]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("the venue answered");

    assert!(!r.old_cancel_refused, "the old id was still cancellable");
    assert!(r.new_cancel_succeeded);
    assert!(!r.passed());
    // And BOTH were still swept: the cancels are the cleanup, so the
    // failing path does not strand the very order it just found.
    assert_eq!(served.load(Ordering::SeqCst), 4);
}

/// **A modify that killed the old order and created nothing** is the
/// other half of the asymmetry. The old id is gone — which on its own
/// looks exactly like success — but nothing answers to the new one.
#[test]
fn a_modify_that_created_nothing_is_not_a_requote_either() {
    let (port, tls, _) = boot(&[PLACED, MODIFIED, ALREADY_GONE, ALREADY_GONE]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("the venue answered");

    assert!(r.old_cancel_refused, "and this alone would have read as a pass");
    assert!(!r.new_cancel_succeeded);
    assert!(!r.passed());
}

/// A post-only requote that TRADES is measuring the market, not the
/// venue's modify semantics — and it costs balance to learn nothing.
/// The original must be taken back off the book before returning.
#[test]
fn a_requote_that_fills_sweeps_both_ids_rather_than_only_the_old_one() {
    // place, the filling modify, then BOTH cancels.
    let (port, tls, served) = boot(&[PLACED, FILLED, ALREADY_GONE, CANCELLED]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("a stop is not an Err");

    assert!(
        matches!(r.stopped, Some(SmokeErr::Lifecycle { stage: "modify", .. })),
        "{:?}",
        r.stopped
    );
    assert!(!r.passed());
    // THE POINT: which id survives a failed modify is exactly what is
    // unknown — the answer may have been lost after the venue acted —
    // so both are swept. The old shape cancelled only the old one.
    assert_eq!(served.load(Ordering::SeqCst), 4, "place, modify, and BOTH cancels");
    assert!(!r.has_unswept(), "both cancels were answered");
}

/// **The regression the review found.** A transport failure on the
/// LAST cancel used to return `Err` through a `?`, which left the NEW
/// id — the one this probe's own hypothesis says is resting — unswept
/// and, because the cloids come from a timestamp nobody typed,
/// unnameable. The report must come back carrying both ids and saying
/// which one went unanswered.
#[test]
fn a_cancel_that_never_answers_is_reported_with_the_id_to_go_and_find() {
    // Three bodies for four requests: the last cancel gets no answer.
    let (port, tls, served) = boot(&[PLACED, MODIFIED, ALREADY_GONE]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("a stop is not an Err");

    assert!(r.old_cancel_refused, "the old id was answered, and refused");
    assert!(!r.unswept_old);
    assert!(r.unswept_new, "the new id's cancel never came back");
    assert!(r.has_unswept());
    assert!(!r.passed(), "an unswept id is not a pass");
    assert!(matches!(r.stopped, Some(SmokeErr::Http(_))), "{:?}", r.stopped);
    // And the id is IN the report, which is the whole recovery path.
    assert_eq!(r.new_cloid.len(), 16);
    assert_ne!(r.new_cloid, r.old_cloid, "two distinct ids, both named");
    assert_eq!(served.load(Ordering::SeqCst), 3);
}

/// The mainnet guard, on this seam too — checked rather than assumed
/// from the fact that the three probes look alike.
/// **The worst case, and the clause nothing else pins.** The socket
/// dies before either cancel is answered, so TWO post-only orders may
/// be on the book — and the only way an operator finds them is the two
/// hex ids in this report, because they are a marker plus a millisecond
/// nobody typed.
///
/// Also the only witness for `!unswept_old` in `passed()`: every other
/// case answers the third request.
#[test]
fn both_ids_go_unswept_when_the_socket_dies_before_either_cancel() {
    let (port, tls, served) = boot(&[PLACED, MODIFIED]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("a stop is not an Err");

    assert!(r.unswept_old, "the old id's cancel never came back");
    assert!(r.unswept_new, "and the new one was still ATTEMPTED, then did not either");
    assert!(r.has_unswept());
    assert!(!r.passed());
    assert!(!r.old_cancel_refused, "no answer is not evidence the old id is gone");
    assert!(!r.new_cancel_succeeded);
    // Both ids are named, which is the entire recovery path.
    assert_ne!(r.old_cloid, r.new_cloid);
    assert_eq!(r.old_cloid[0], 0xE3, "a probe id, not one of ours");
    assert_eq!(served.load(Ordering::SeqCst), 2);
}

/// A place whose answer never comes back cannot tell whether the order
/// is resting. It holds the cloid, so the sweep still runs — walking
/// away would leave a post-only order under an id nothing prints.
#[test]
fn a_place_with_no_answer_still_sweeps_rather_than_returning_err() {
    let (port, tls, served) = boot(&[]);
    let mut http = client(port, tls);
    let r = run_requote_on(&cfg(), &mut http, spec()).expect("a stop is not an Err");

    assert!(r.stopped.is_some(), "the place failed");
    assert_eq!(r.placed_oid, 0, "no oid was ever echoed");
    assert!(r.has_unswept());
    assert!(!r.passed());
    assert_ne!(r.old_cloid, r.new_cloid, "and both ids are still named");
    assert_eq!(served.load(Ordering::SeqCst), 0);
}

#[test]
fn a_requote_refuses_mainnet() {
    let (port, tls, served) = boot(&[PLACED]);
    let mut http = client(port, tls);
    let m = HlConfig::new(
        Scope::Live,
        exec_hyperliquid::config::HOST_MAINNET,
        'a',
        KEY,
        ADDR,
    )
    .expect("cfg");
    let e = run_requote_on(&m, &mut http, spec()).expect_err("mainnet must be refused");
    assert!(matches!(e, SmokeErr::NotTestnet(_)), "{e:?}");
    assert_eq!(served.load(Ordering::SeqCst), 0);
}


// ---- Phase G: the roll sweep ----------------------------------------

fn sweep_spec() -> SweepSpec {
    SweepSpec {
        asset: 100_000_000 + 10 * 3253,
        px_1e8: 30_000_000,
        sz_1e8: 400_000_000,
        is_buy: true,
        strategy_id: 3,
        client_oid: 1,
    }
}

/// The open-orders answer the venue gives while our order rests.
/// `oid` 424242 is what `PLACED` echoes.
const OPEN_OURS: &[u8] = br##"[{"coin":"#32530","oid":424242,"limitPx":"0.30",
  "cloid":"0x4d560300000000000000000000000001","isTrigger":false}]"##;
/// The same order, but the venue omitted the cloid — which is what
/// plain `openOrders` does, and why the sweep asks for the frontend
/// variant.
const OPEN_NO_CLOID: &[u8] = br##"[{"coin":"#32530","oid":424242,"limitPx":"0.30"}]"##;
const OPEN_EMPTY: &[u8] = b"[]";

/// **The whole sweep, end to end.** Place, enumerate (listed, with the
/// cloid, and the ARM'S OWN selection picks it), cancel by oid,
/// enumerate again (gone).
#[test]
fn the_sweep_lists_selects_cancels_and_confirms() {
    let (port, tls, served) = boot(&[PLACED, OPEN_OURS, CANCELLED, OPEN_EMPTY]);
    let mut http = client(port, tls);
    let r = run_sweep_on(&cfg(), &mut http, sweep_spec()).expect("sweep");

    assert!(r.listed, "the venue named it, with its cloid");
    assert!(r.selected, "and recon::ours_on_leg picked it out");
    assert!(r.cancelled);
    assert!(r.gone_after, "a SECOND enumerate is what proves it");
    assert!(!r.unswept);
    assert!(r.passed());
    // The leg's name came from the VENUE's row, never derived from the
    // asset id — that derivation is the guess LAW E-4 refuses.
    assert_eq!(&r.coin[..r.coin_len as usize], b"#32530");
    assert_eq!(served.load(Ordering::SeqCst), 4);
}

/// **Why the sweep asks for `frontendOpenOrders`.** Plain `openOrders`
/// omits the cloid, and without it the arm cannot tell our order from a
/// stranger's — so it would sweep nothing, silently, on every roll.
#[test]
fn an_answer_without_cloids_selects_nothing_rather_than_guessing() {
    let (port, tls, _) = boot(&[PLACED, OPEN_NO_CLOID, OPEN_EMPTY]);
    let mut http = client(port, tls);
    let r = run_sweep_on(&cfg(), &mut http, sweep_spec()).expect("the venue answered");

    assert!(!r.listed, "no cloid means we cannot claim it");
    assert!(!r.selected);
    assert!(!r.passed());
}

/// **ACKED and GONE are different claims**, which is the entire reason
/// the probe enumerates a second time. A venue that accepts a cancel
/// and leaves the order resting would otherwise read as a pass.
#[test]
fn a_cancel_that_was_acked_but_left_the_order_resting_is_not_a_pass() {
    let (port, tls, _) = boot(&[PLACED, OPEN_OURS, CANCELLED, OPEN_OURS, ALREADY_GONE]);
    let mut http = client(port, tls);
    let r = run_sweep_on(&cfg(), &mut http, sweep_spec()).expect("the venue answered");

    assert!(r.listed && r.selected && r.cancelled);
    assert!(!r.gone_after, "it is STILL LISTED");
    assert!(!r.passed(), "an acked cancel is not evidence");
}

/// The mainnet guard on this seam too.
#[test]
fn a_sweep_refuses_mainnet() {
    let (port, tls, served) = boot(&[PLACED]);
    let mut http = client(port, tls);
    let m = HlConfig::new(
        Scope::Live,
        exec_hyperliquid::config::HOST_MAINNET,
        'a',
        KEY,
        ADDR,
    )
    .expect("cfg");
    let e = run_sweep_on(&m, &mut http, sweep_spec()).expect_err("mainnet must be refused");
    assert!(matches!(e, SmokeErr::NotTestnet(_)), "{e:?}");
    assert_eq!(served.load(Ordering::SeqCst), 0);
}
