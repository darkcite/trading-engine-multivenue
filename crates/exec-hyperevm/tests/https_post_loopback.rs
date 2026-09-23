// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `core_net::HttpsPost` — the write arm's keep-alive client — against
//! the rustls `testnode` (HYPARB H9; `HttpsPost`'s only user is this
//! crate, so its loopback lives here).
//!
//! | test | asserts |
//! |---|---|
//! | keep-alive | many requests, ONE connection |
//! | digit counts | bodies of 1–4 length digits, in any order, all arrive intact on one connection |
//! | idle close | the server hangs up after answering: the next request dials fresh and SUCCEEDS — never a false "may have left the host" |
//! | announced close | `Connection: close` retires the connection with the answer |
//! | chunked | one chunk (read where it lies) and many chunks (compacted) both scan |
//! | overflow | a body over the window is refused before any byte is written |
//!
//! Offline-path doctrine: this test allocates freely.

use core_net::{HttpsPost, PostErr, PostErrKind};
use exec_hyperevm::rpc::{scan_quantity, write_chain_id};
use exec_hyperevm::testnode::{boot, Node};

fn node() -> Node {
    Node {
        chain_id: 998,
        ..Node::default()
    }
}

fn client(port: u16, cfg: std::sync::Arc<rustls::ClientConfig>, body_cap: usize) -> HttpsPost {
    HttpsPost::new("localhost", port, "/evm", cfg, body_cap, 16 * 1024).expect("client")
}

/// `eth_chainId` with `pad` spaces inside the object (a body of any
/// length the server still parses).
fn ask_chain(h: &mut HttpsPost, id: u64, pad: usize) -> Result<u128, PostErr> {
    let n = write_chain_id(h.body_mut(), id).expect("fits");
    // Shift the rendered request right by `pad` and fill with spaces
    // after the opening brace.
    let b = h.body_mut();
    b.copy_within(1..n, 1 + pad);
    let mut i = 1;
    while i <= pad {
        b[i] = b' ';
        i += 1;
    }
    let (status, r) = h.post(n + pad)?;
    assert_eq!(status, 200);
    Ok(scan_quantity(&h.resp()[r], id).expect("scan"))
}

#[test]
fn keep_alive_serves_many_requests_on_one_connection() {
    let n = boot(node());
    let mut h = client(n.port, n.client_cfg.clone(), 4096);
    let mut id = 1;
    while id <= 20 {
        assert_eq!(ask_chain(&mut h, id, 0).expect("round trip"), 998);
        id += 1;
    }
    assert_eq!(n.state.lock().unwrap().conns, 1);
    assert_eq!(h.dials(), 1);
    assert!(h.is_connected());
}

#[test]
fn bodies_of_every_digit_count_arrive_intact_on_one_connection() {
    let n = boot(node());
    let mut h = client(n.port, n.client_cfg.clone(), 4096);
    // write_chain_id renders 59 B: pads reach 2, 3 and 4 digits and back.
    let pads = [0usize, 60, 3000, 41, 0, 2000, 900];
    let mut i = 0;
    while i < pads.len() {
        assert_eq!(
            ask_chain(&mut h, 100 + i as u64, pads[i]).expect("round trip"),
            998,
            "pad {}",
            pads[i]
        );
        i += 1;
    }
    assert_eq!(n.state.lock().unwrap().conns, 1);
}

#[test]
fn an_idle_close_is_noticed_before_the_next_request_is_written() {
    let mut nd = node();
    nd.close_after = 1;
    nd.close_delay_ms = 50;
    let n = boot(nd);
    let mut h = client(n.port, n.client_cfg.clone(), 4096);
    let mut id = 1;
    while id <= 4 {
        // The FIN arrives while the client is idle; the reuse probe must
        // retire the connection so this request dials fresh and never
        // reports `left_host` for bytes a dead socket swallowed.
        std::thread::sleep(std::time::Duration::from_millis(250));
        match ask_chain(&mut h, id, 0) {
            Ok(v) => assert_eq!(v, 998),
            Err(e) => panic!("request {id}: {e} — a stale keep-alive was reused"),
        }
        id += 1;
    }
    assert_eq!(
        n.state.lock().unwrap().conns,
        4,
        "one connection per answer"
    );
    assert_eq!(h.dials(), 4, "the dial counter shows the endpoint's closes");
}

#[test]
fn an_announced_close_retires_the_connection_with_the_answer() {
    let mut nd = node();
    nd.close_after = 1;
    nd.announce_close = true;
    nd.close_delay_ms = 300;
    let n = boot(nd);
    let mut h = client(n.port, n.client_cfg.clone(), 4096);
    assert_eq!(ask_chain(&mut h, 1, 0).expect("first"), 998);
    assert!(!h.is_connected(), "`Connection: close` retires it at once");
    assert!(!h.resp().is_empty(), "the answer survives the retirement");
    // Immediately — the server has not closed yet (300 ms): the client
    // must not have reused the doomed connection.
    assert_eq!(ask_chain(&mut h, 2, 0).expect("second"), 998);
    assert_eq!(n.state.lock().unwrap().conns, 2);
}

#[test]
fn chunked_answers_scan_in_one_chunk_and_in_many() {
    let mut chunks = 1u8;
    while chunks <= 5 {
        let mut nd = node();
        nd.chunked = chunks;
        let n = boot(nd);
        let mut h = client(n.port, n.client_cfg.clone(), 4096);
        assert_eq!(
            ask_chain(&mut h, 7, 0).expect("chunked"),
            998,
            "{chunks} chunks"
        );
        assert_eq!(
            ask_chain(&mut h, 8, 500).expect("chunked"),
            998,
            "{chunks} chunks"
        );
        chunks += 2;
    }
}

#[test]
fn a_body_over_the_window_is_refused_before_any_byte_is_written() {
    let n = boot(node());
    let mut h = client(n.port, n.client_cfg.clone(), 64);
    assert_eq!(
        h.post(65).err(),
        Some(PostErr {
            err: PostErrKind::Overflow,
            left_host: false
        })
    );
    assert_eq!(
        n.state.lock().unwrap().conns,
        0,
        "no connection was even opened"
    );
}
