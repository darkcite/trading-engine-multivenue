// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! §5.4 regression (2026-08-29): `TlsTransport` must survive a burst
//! larger than rustls' internal received-plaintext buffer.
//!
//! rustls 0.23 hard-caps buffered received plaintext at 16 KiB
//! (`DEFAULT_RECEIVED_PLAINTEXT_LIMIT`, `common_state.rs`) and its
//! `read_tls()` signals BACKPRESSURE — not failure — with an
//! `io::ErrorKind::Other` error ("received plaintext buffer full",
//! `conn.rs`). The pre-fix `drive_tls` looped `read_tls` without
//! draining plaintext between iterations and treated every error as
//! fatal, so ANY poll wake with >16 KiB of decryptable ciphertext
//! queued killed the session: `err_site=pump io_kind=other
//! venue_code=0`, ~1.4 s cycle, 508 200 log lines over Aug 25–29.
//! OKX is the venue whose normal bursts qualify (`books` 400-level
//! snapshots ≈ 25 KiB/frame, `opt-summary` family pushes ≈ 600 KiB,
//! post-subscribe burst = MBs); Deribit's "churny sessions" were the
//! same bug at lower rate.
//!
//! Offline test doctrine: this is not a hot path — test-side
//! allocation is fine. The transport under test still allocates
//! nothing after construction.

use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_net::TlsTransport;
use core_net::{Status, Transport};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// One deterministic pseudo-random byte per offset — content check
/// without holding a reference copy.
#[inline]
fn pattern_byte(i: usize) -> u8 {
    ((i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as u8
}

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

/// Drive one client session against a server that writes `total`
/// patterned bytes in bursts of `burst` per `write_all` call, with
/// `pause` between bursts. Returns the bytes the client received
/// before EOF/deadline; panics on any transport error — the exact
/// failure this test exists to pin.
fn run_session(total: usize, burst: usize, pause: Duration) -> Vec<u8> {
    let cert = make_cert();
    let server_cfg = build_server_config(&cert);
    let client_cfg = build_client_config(&cert);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = thread::spawn(move || {
        let (mut tcp, _) = listener.accept().expect("accept");
        tcp.set_nodelay(true).ok();
        let mut conn = ServerConnection::new(server_cfg).expect("server conn");
        let mut tls = Stream::new(&mut conn, &mut tcp);
        let mut sent = 0usize;
        let mut chunk = vec![0u8; burst];
        while sent < total {
            let n = burst.min(total - sent);
            let mut i = 0;
            while i < n {
                chunk[i] = pattern_byte(sent + i);
                i += 1;
            }
            tls.write_all(&chunk[..n]).expect("server write");
            tls.flush().expect("server flush");
            sent += n;
            if !pause.is_zero() {
                thread::sleep(pause);
            }
        }
        // Give the client time to drain before FIN so a raced close
        // never truncates the burst on a slow CI host.
        thread::sleep(Duration::from_millis(600));
        tcp.shutdown(std::net::Shutdown::Both).ok();
    });

    let server_name = ServerName::try_from("localhost").expect("name");
    let mut transport =
        TlsTransport::connect(addr, server_name, client_cfg).expect("client connect");
    let mut poll = mio::Poll::new().expect("poll");
    let mut events = mio::Events::with_capacity(64);
    let token = mio::Token(0);
    transport
        .register(poll.registry(), token)
        .expect("register");
    let mut last_interest = transport.interest();

    let mut got: Vec<u8> = Vec::with_capacity(total);
    let mut scratch = [0u8; 65536];
    let mut closed = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !closed && got.len() < total && Instant::now() < deadline {
        poll.poll(&mut events, Some(Duration::from_millis(50)))
            .expect("poll");
        for ev in events.iter() {
            if ev.token() != token {
                continue;
            }
            // The §5.4 pin: the pump must NEVER error on a burst —
            // pre-fix this panicked here with kind=Other ("received
            // plaintext buffer full") once >16 KiB was queued.
            let status = transport.pump(ev).expect("pump must not error on a burst");
            if status == Status::Closed {
                closed = true;
            }
        }
        // fill_rx shape: drain plaintext until WouldBlock. Post-fix
        // read() also pulls queued ciphertext through the TLS state
        // machine, so the whole burst lands in this loop.
        loop {
            match transport.read(&mut scratch) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => got.extend_from_slice(&scratch[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read must not error on a burst: {e:?}"),
            }
        }
        let interest = transport.interest();
        if interest != last_interest {
            transport
                .reregister(poll.registry(), token)
                .expect("reregister");
            last_interest = interest;
        }
    }

    server.join().expect("server thread");
    got
}

/// The §5.4 reproduction: one 256 KiB burst written in a single
/// server-side `write_all` — 16× the rustls received-plaintext cap.
/// The session must survive and deliver every byte, in order.
#[test]
fn burst_larger_than_rustls_plaintext_cap_survives() {
    let total = 256 * 1024;
    let got = run_session(total, total, Duration::ZERO);
    assert_eq!(got.len(), total, "client must drain the full burst");
    let mut i = 0;
    while i < got.len() {
        assert_eq!(got[i], pattern_byte(i), "byte {i} corrupted");
        i += 1;
    }
}

/// Fast-path guard: a small trickle (well under the cap, spaced
/// writes) behaves exactly as before the fix — same bytes, no error.
#[test]
fn small_trickle_still_flows() {
    let total = 3 * 1024;
    let got = run_session(total, 1024, Duration::from_millis(20));
    assert_eq!(got.len(), total);
    let mut i = 0;
    while i < got.len() {
        assert_eq!(got[i], pattern_byte(i), "byte {i} corrupted");
        i += 1;
    }
}
