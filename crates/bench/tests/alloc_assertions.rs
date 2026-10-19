// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Allocation-assertion harness.
//!
//! Installs `core_alloc::CountingAllocator` as the global allocator for
//! this test binary and asserts that the hot paths (ring push/pop,
//! parser scan, book apply) allocate ZERO bytes per operation.
//!
//! Run:
//!     cargo test -p bench --test alloc_assertions --release -- --test-threads=1
//!
//! `--release` matters: we want this check to run against the codegen
//! path that ships to production. `--test-threads=1` is REQUIRED —
//! the counting allocator is process-global, and the default parallel
//! test runner would cross-pollute each test's `AllocGuard` delta with
//! allocations from sibling tests. The `make alloc-assert` target
//! already passes this flag.

use core_alloc::{AllocGuard, CountingAllocator};

// Install the counting allocator. Only this test binary is affected.
#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator::new();

use core_net::{
    ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_text_frame,
    ws_write_text_frame_rendered, TestTransport, WsReadResult,
};
use core_parse::scan_price_1e6;
use core_ring::Ring;
use core_types::{Price, Qty, SymbolId, Tick, VenueId};
use ingress_ai::{admit_frame, pack_frame, AiCmdCapture, AiIngressStatus, FrameVerdict, SeqPolicy};
use ingress_binance::parse_book_ticker;
use ingress_polymarket::run_loop::{
    drive_one, note_transport_ready, Driver, State, SymbolMap, DEFAULT_TICK_RING_CAP,
};
use ingress_rpc::{
    eth_block_number_request_parts, parse_block_number_result, parse_new_head_notification,
};

/// Scan 10_000 prices through the byte-scanner parser — must not allocate.
#[test]
fn price_scanner_is_zero_alloc() {
    let buf = b"0.518000";
    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        let (v, _end) = scan_price_1e6(buf, 0).unwrap();
        acc = acc.wrapping_add(v);
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "price scanner allocated {allocs} times ({bytes} B)"
    );
}

/// Parsing a sample Polymarket book frame and `price_change` row 1000x
/// must not allocate.
#[test]
fn book_parser_is_zero_alloc() {
    let buf: &[u8] = br#"[{"market":"0x60c2","asset_id":"0xabc","timestamp":"1713000000000","hash":"deadbeef","bids":[{"price":"0.517","size":"200.0"},{"price":"0.518","size":"100.0"}],"asks":[{"price":"0.521","size":"150.0"},{"price":"0.520","size":"50.0"}],"event_type":"book"}]"#;
    // The single-object, `", "`-separated re-snapshot (live 2026-10-15;
    // a copy of ingress-polymarket's `LIVE_BOOK_SPACED` test fixture).
    let spaced: &[u8] = br#"{"market":"0x99b5ab8514be117c2a544cfc01186da0157453f1db21ae635c11d6a6afef22ee", "asset_id":"88378104575386233188962226569320382272592669666454108287035040791872565531906", "bids":[{"price":"0.01", "size":"14741.19"}, {"price":"0.16", "size":"190"}, {"price":"0.17", "size":"10"}], "asks":[{"price":"0.99", "size":"107748.47"}, {"price":"0.2", "size":"5465"}, {"price":"0.19", "size":"1861.19"}], "hash":"b383d532df1450e8b0a702c5c42fef0aff0b461b", "timestamp":"1792068514889", "event_type":"book"}"#;
    let row: &[u8] = br#"{"asset_id":"0xabc","price":"0.518","size":"642.77","side":"BUY","hash":"d0c1","best_bid":"0.518","best_ask":"0.520"}"#;
    let g = AllocGuard::new();
    for _ in 0..1_000u32 {
        let mut t = core_types::Tick::ZERO;
        assert!(ingress_polymarket::parse_book_update(buf, 1, 0, &mut t));
        std::hint::black_box(t);
        let mut t = core_types::Tick::ZERO;
        assert!(ingress_polymarket::parse_book_update(spaced, 1, 0, &mut t));
        std::hint::black_box(t);
        let mut t = core_types::Tick::ZERO;
        assert!(ingress_polymarket::parse_price_change_row(row, 1, 0, 1_713_000_000_123, &mut t));
        std::hint::black_box(t);
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "book parser allocated {allocs} times ({bytes} B)"
    );
}

/// Sanity check: the guard itself behaves.
#[test]
fn guard_reports_zero_when_nothing_allocates() {
    let g = AllocGuard::new();
    // Pure arithmetic loop — nothing should allocate.
    let mut x: u64 = 0;
    for i in 0..10_000u64 {
        x = x.wrapping_add(i);
    }
    std::hint::black_box(x);
    let (allocs, _bytes, _) = g.delta();
    assert_eq!(allocs, 0);
}

// ---------------------------------------------------------------
// Phase 1a hot-path allocation assertions
// ---------------------------------------------------------------

/// Round-trip a WebSocket text frame (write → read → unmask) 10_000
/// times through the preallocated tx/rx buffers, the header-first render
/// beside the plain writer. The full core-net codec path must not
/// allocate.
#[test]
fn ws_frame_roundtrip_is_zero_alloc() {
    // Preallocated tx/rx buffers — single allocation each, outside the
    // measurement window.
    let mut tx = [0u8; 256];
    let mut rendered = [0u8; 256];
    let mut rx = [0u8; 256];
    let payload: &[u8] = b"{\"u\":12345,\"s\":\"BTCUSDT\"}";

    let g = AllocGuard::new();

    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        let mask = ws_mask_from_counter(i);
        let n = ws_write_text_frame(&mut tx, payload, mask).unwrap();
        let m = ws_write_text_frame_rendered(&mut rendered, mask, |p| p.put(payload)).unwrap();
        assert!(rendered[..m] == tx[..n], "the rendered frame differs");
        // Copy the written bytes into rx so the read path operates on
        // its own mutable buffer (unmask is in-place).
        rx[..n].copy_from_slice(&tx[..n]);
        match ws_read_frame(&rx[..n]) {
            WsReadResult::Frame { header, payload } => {
                let start = payload.start;
                let end = payload.end;
                if header.masked {
                    ws_unmask_in_place(&mut rx[start..end], header.mask);
                }
                acc = acc.wrapping_add(end as u64 - start as u64);
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "ws_frame roundtrip allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "ws_frame roundtrip bytes should be zero: saw {bytes}"
    );
}

/// Parse a Binance `@bookTicker` frame and a USDⓈ-M `@markPrice` frame
/// 10_000x each, in place into one reused frame apiece (BX0) — must be
/// zero-alloc.
#[test]
fn binance_book_ticker_is_zero_alloc() {
    let buf: &[u8] =
        br#"{"u":400900217,"s":"BTCUSDT","b":"65000.01","B":"1.234","a":"65000.55","A":"0.987"}"#;
    let mark: &[u8] = br#"{"e":"markPriceUpdate","E":1790161527002,"s":"BTCUSDT","p":"85840.40234633","ap":"85840.40234633","P":"85863.42568007","i":"85882.44043478","r":"0.00005016","T":1790179200000,"st":1}"#;
    let sym: SymbolId = 7;
    let mut t = ingress_binance::BookTickerFrame::ZERO;
    let mut m = ingress_binance::BnMarkPriceFrame::ZERO;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        assert!(parse_book_ticker(buf, sym, &mut t));
        acc = acc.wrapping_add(t.bid_px_1e6);
        assert!(ingress_binance::parse_mark_price(mark, sym, &mut m));
        acc = acc.wrapping_add(m.funding_rate_1e9);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "parse_book_ticker / parse_mark_price allocated {allocs} times ({bytes} B)"
    );
}

/// The in-place ring API: 10 000 `Tick`s copied once into their slot
/// and read there through the `Popped` guard. Must not allocate.
#[test]
fn ring_push_ref_pop_ref_is_zero_alloc() {
    let (mut prod, mut cons) = Ring::<Tick, 1024>::new().split();
    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for i in 0..10_000u32 {
        let t = Tick::new(
            0,
            VenueId::Polymarket,
            1,
            i + 1,
            Price::from_raw(500_000),
            Qty::from_raw(100),
            Price::from_raw(510_000),
            Qty::from_raw(50),
        );
        assert!(prod.try_push_ref(&t));
        let slot = cons.try_pop_ref().expect("the tick just pushed");
        acc = acc
            .wrapping_add(slot.bid_px.raw())
            .wrapping_add(i64::from(slot.venue_seq));
    }
    std::hint::black_box(acc);
    let (allocs, bytes, deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "ring push_ref/pop_ref allocated {allocs} times ({bytes} B, {deallocs} deallocs)"
    );
}

/// The same for the 192 B `DepthTopK` lane: one copy into the slot, the
/// snapshot read in place. Must not allocate.
#[test]
fn depth_ring_push_ref_pop_ref_is_zero_alloc() {
    let (mut prod, mut cons) =
        Ring::<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>::new().split();
    let mut snap = core_types::DepthTopK::EMPTY;
    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        snap.ts_ns = i;
        assert!(prod.try_push_ref(&snap));
        let slot = cons.try_pop_ref().expect("the snapshot just pushed");
        acc = acc.wrapping_add(slot.ts_ns).wrapping_add(u64::from(slot.k));
    }
    std::hint::black_box(acc);
    let (allocs, bytes, deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "depth ring push_ref/pop_ref allocated {allocs} times ({bytes} B, {deallocs} deallocs)"
    );
}

/// Exercise the Polygon RPC codec hot paths — request serialize,
/// response parse, notification parse — for 10_000 iterations each.
/// Must be zero-alloc.
#[test]
fn rpc_block_number_is_zero_alloc() {
    // Preallocated request buffer.
    let mut req = [0u8; 128];
    let resp: &[u8] = br#"{"jsonrpc":"2.0","id":42,"result":"0x10e6c0c"}"#;
    let notif: &[u8] = br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xcafe","result":{"number":"0x1234","timestamp":"0x5faa","gasUsed":"0x7a1200"}}}"#;

    let g = AllocGuard::new();

    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        let mut digits = [0u8; 20];
        let parts = eth_block_number_request_parts(i, &mut digits);
        let n = core_net::ws_write_binary_frame_parts(&mut req, &parts, [1, 2, 3, 4]).unwrap();
        acc = acc.wrapping_add(n as u64);
        let (id, block) = parse_block_number_result(resp).unwrap();
        acc = acc.wrapping_add(id).wrapping_add(block);
        let mut head = ingress_rpc::NewHead::ZERO;
        assert!(parse_new_head_notification(notif, &mut head));
        acc = acc.wrapping_add(head.number);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "rpc codecs allocated {allocs} times ({bytes} B)");
}

// ---------------------------------------------------------------
// Phase 1b: run-loop steady-state zero-alloc assertion
// ---------------------------------------------------------------

/// Drive the Polymarket ingress run-loop through 1 000 steady-state
/// frames via a `TestTransport`. The only non-zero-alloc work happens
/// at construction; every `drive_one` call must allocate zero bytes.
#[test]
fn polymarket_run_loop_steady_state_is_zero_alloc() {
    // ---- boot (NOT measured) ----
    let mut transport = TestTransport::with_capacity(128 * 1024);

    let mut driver = Driver::new(0xDEAD_BEEFu64, b"1234567890");
    note_transport_ready(&mut driver, core_net::Status::Ready);
    // Ingress health telemetry sink (Phase 8a). Its bumps are relaxed
    // atomics and allocate nothing, but construct it outside the
    // measurement window on principle: setup is never measured.
    let status = core_metrics::IngressStatus::new();
    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("pm_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "pm",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();
    // Jump the driver straight to Steady via a round-trip through the
    // handshake so we exercise the production path during boot, not
    // the measurement window.
    drive_one(
        &mut transport,
        &mut driver,
        b"host",
        b"/",
        &mut placeholder_producer(),
        &SymbolMap::from_pairs(std::iter::empty()),
        &status,
        &mut capture,
    )
    .unwrap();
    // Drain the client's GET request so the test transport's outbound
    // buffer is empty.
    let mut scratch = [0u8; 4096];
    let _ = transport.drain_outgoing(&mut scratch);
    // Inject a canned `101 Switching Protocols` reply and advance.
    let key = core_net::sec_websocket_key_from_seed(0xDEAD_BEEFu64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    let symbol_map = SymbolMap::from_pairs(std::iter::once((b"0xABC".to_vec(), 42u32)));
    let ring: std::sync::Arc<Ring<Tick, DEFAULT_TICK_RING_CAP>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    drive_one(
        &mut transport,
        &mut driver,
        b"host",
        b"/",
        &mut prod,
        &symbol_map,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), State::Steady);

    // Preloaded unmasked Text frame containing a Polymarket book
    // update in the live wire shape (2026-08-14) — 16-bit extended
    // length, real book events exceed the 125 B short form.
    let payload: &[u8] = br#"[{"market":"0x60c2","asset_id":"0xABC","timestamp":"1713000000000","hash":"h","bids":[{"price":"0.518","size":"100"}],"asks":[{"price":"0.520","size":"50"}],"event_type":"book"}]"#;
    assert!(payload.len() <= u16::MAX as usize);
    let mut frame = [0u8; 256];
    frame[0] = 0x81;
    frame[1] = 126;
    frame[2..4].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    frame[4..4 + payload.len()].copy_from_slice(payload);
    let frame_len = 4 + payload.len();

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut acc: i64 = 0;
    for _ in 0..1_000u32 {
        // Feeding one frame per iteration — the test transport's
        // `append` copies bytes into its preallocated ring buffer and
        // never reallocates.
        let written = transport.inject_incoming(&frame[..frame_len]);
        assert_eq!(written, frame_len);

        drive_one(
            &mut transport,
            &mut driver,
            b"host",
            b"/",
            &mut prod,
            &symbol_map,
            &status,
            &mut capture,
        )
        .unwrap();

        // Drain the tick so the ring doesn't fill.
        let t = *cons.try_pop_ref().expect("tick should be produced");
        acc = acc.wrapping_add(t.bid_px.raw());
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "polymarket run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "polymarket run-loop bytes should be zero: saw {bytes}"
    );
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.tap_dropped(), 0);
    assert!(capture.ticks_written() > 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// Helper: build a short-lived `Producer` for scratch tests that don't
/// care about the ring. Boxed so the lifetime matches the caller's
/// frame.
fn placeholder_producer() -> core_ring::Producer<Tick, DEFAULT_TICK_RING_CAP> {
    let ring: std::sync::Arc<Ring<Tick, DEFAULT_TICK_RING_CAP>> = Ring::new();
    let (prod, _cons) = ring.split();
    prod
}

// ---------------------------------------------------------------
// Phase 1c: run-loop steady-state zero-alloc assertions (3 new)
// ---------------------------------------------------------------

/// BX0-F2: the live Binance options push (fstream `/market`, K6
/// 2026-09-23) — three of its 752 elements, verbatim: the push's first
/// row and the ATM pair.
const BN_LIVE_MARK_ARRAY: &[u8] = br#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-261225-92000-C","mp":"4696.169","E":1790161477975,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"4670.000","ao":"4760.000","bq":"3.52","aq":"3.52","b":"0.38545907","a":"0.39075673","hl":"8450.000","ll":"940.000","vo":"0.387","rf":"0.0529","d":"0.42618971","t":"-35.49083602","g":"0.00002332","v":"169.85468453"},{"s":"BTC-260925-86000-P","mp":"905.351","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"905.000","ao":"920.000","bq":"4.43","aq":"12.00","b":"0.34885705","a":"0.35497367","hl":"1625.000","ll":"185.000","vo":"0.349","rf":"0.0558","d":"-0.51276574","t":"-230.5222729","g":"0.00018424","v":"24.52223767"},{"s":"BTC-260925-86000-C","mp":"809.784","E":1790161477974,"e":"markPrice","i":"85879.82826087","P":"0.000","bo":"800.000","ao":"810.000","bq":"5.08","aq":"1.10","b":"0.34500957","a":"0.34908772","hl":"1455.000","ll":"165.000","vo":"0.349","rf":"0.0558","d":"0.48723426","t":"-227.33432684","g":"0.00018682","v":"24.52223767"}]}"#;

/// The server's `101` reply to a client handshake seeded with `seed`
/// (boot side of the Binance run-loop gate, outside any window).
fn bn_upgrade_reply(seed: u64, resp: &mut [u8; 256]) -> usize {
    let key = core_net::sec_websocket_key_from_seed(seed);
    let accept = core_net::expected_accept(&key);
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    n
}

/// Drive the Binance ingress run-loop through 1 000 steady-state
/// frames via a `TestTransport` — bookTicker slot first, then (BX0-F2)
/// the options slot through 1 000 live mark-array pushes. The only
/// non-zero-alloc work happens at construction; every `drive_one`
/// call must allocate zero bytes.
#[test]
fn binance_run_loop_steady_state_is_zero_alloc() {
    use ingress_binance::run_loop as bwl;

    // ---- boot (NOT measured) ----
    let mut transport = TestTransport::with_capacity(128 * 1024);

    let sym: SymbolId = 7;
    let mut driver = bwl::Driver::new(0xBA07u64, sym);
    bwl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    // VM2 V2: hoisted throwaway opt lane — created OUTSIDE the
    // AllocGuard window (Ring::new allocates).
    let (mut otx, mut orx) =
        Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { bwl::DEFAULT_TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    // WS10-A: event lane built boot-side; the measured pushes/
    // drops below must be 0 B/op like everything else.
    let event_ring: std::sync::Arc<
        Ring<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>,
    > = Ring::new();
    let (mut etx, mut erx) = event_ring.split();
    let depth_ring: std::sync::Arc<Ring<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>> =
        Ring::new();
    let (_dtx, _drx) = depth_ring.split();

    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("bn_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "bn",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    // Send the client GET handshake.
    bwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut scratch = [0u8; 4096];
    let _ = transport.drain_outgoing(&mut scratch);

    // Inject the 101 reply matching the seed.
    let mut resp = [0u8; 256];
    let n = bn_upgrade_reply(0xBA07u64, &mut resp);
    transport.inject_incoming(&resp[..n]);
    bwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), bwl::State::Steady);

    // Canned unmasked Text bookTicker frame.
    let payload: &[u8] =
        br#"{"u":400900217,"s":"BTCUSDT","b":"65000.01","B":"1.234","a":"65000.55","A":"0.987"}"#;
    assert!(payload.len() <= 125);
    let mut frame = [0u8; 128];
    frame[0] = 0x81;
    frame[1] = payload.len() as u8;
    frame[2..2 + payload.len()].copy_from_slice(payload);
    let frame_len = 2 + payload.len();

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut acc: i64 = 0;
    for _ in 0..1_000u32 {
        let written = transport.inject_incoming(&frame[..frame_len]);
        assert_eq!(written, frame_len);
        bwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            &status,
            &mut capture,
        )
        .unwrap();
        let t = *cons.try_pop_ref().expect("tick should be produced");
        acc = acc.wrapping_add(t.bid_px.raw());
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "binance run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "binance run-loop bytes should be zero: saw {bytes}"
    );

    // ---- BX0-F2: the options slot (boot NOT measured) ----
    let mut table = ingress_binance::eapi::EapiSymbolTable::new();
    table.insert(b"BTC-260925-86000-C", (1 << 24) | 1025).unwrap();
    table.insert(b"BTC-260925-86000-P", (1 << 24) | 1026).unwrap();
    let mut odriver = bwl::Driver::new_eapi(
        0xBA08u64,
        table,
        std::sync::Arc::new(core_metrics::IngressStatus::new()),
    );
    bwl::note_transport_ready(&mut odriver, core_net::Status::Ready);
    let mut otransport = TestTransport::with_capacity(128 * 1024);
    for step in 0..2 {
        if step == 1 {
            let _ = otransport.drain_outgoing(&mut scratch);
            let n = bn_upgrade_reply(0xBA08u64, &mut resp);
            otransport.inject_incoming(&resp[..n]);
        }
        bwl::drive_one(
            &mut otransport,
            &mut odriver,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            &status,
            &mut capture,
        )
        .unwrap();
    }
    assert_eq!(odriver.state(), bwl::State::Steady);
    // The live push as one unmasked Text frame (16-bit length form).
    let mut mframe = [0u8; 2048];
    let plen = BN_LIVE_MARK_ARRAY.len();
    assert!(plen > 125 && plen + 4 <= mframe.len());
    mframe[0] = 0x81;
    mframe[1] = 126;
    mframe[2..4].copy_from_slice(&(plen as u16).to_be_bytes());
    mframe[4..4 + plen].copy_from_slice(BN_LIVE_MARK_ARRAY);
    let mlen = 4 + plen;

    // ---- measurement window: split, walk, look up, parse, publish ----
    let g2 = AllocGuard::new();
    let mut rows = 0u64;
    let mut summaries = 0u64;
    for _ in 0..1_000u32 {
        let written = otransport.inject_incoming(&mframe[..mlen]);
        assert_eq!(written, mlen);
        bwl::drive_one(
            &mut otransport,
            &mut odriver,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut otx,
            &status,
            &mut capture,
        )
        .unwrap();
        while cons.try_pop_ref().is_some() {
            rows += 1;
        }
        while orx.try_pop_ref().is_some() {
            summaries += 1;
        }
    }
    core_types::Capture::maybe_flush(&mut capture, 2 * core_io::CAPTURE_FLUSH_INTERVAL_NS + 2);
    let (allocs, bytes, _deallocs) = g2.delta();
    assert_eq!(
        allocs, 0,
        "binance options slot allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "binance options slot bytes should be zero: saw {bytes}");
    // Premises: both selected rows came out of every push, and the
    // unselected one never did.
    assert_eq!(rows, 2_000, "a tick per selected row per push");
    assert_eq!(summaries, 2_000, "a summary per selected row per push");

    // ---- BX6 (O-BX29): the mark slot with the Mark bit ARMED (boot NOT
    // measured) — the armed engine's mask: every mark is captured, then
    // flagged in place and pushed onto the venue-event lane ----
    let armed = core_types::EVENT_LANE_FUNDING | core_types::event_lane_bit(core_types::ChannelId::Mark);
    let mut mdriver = bwl::Driver::new_mark_price(0xBA09u64, sym);
    bwl::note_transport_ready(&mut mdriver, core_net::Status::Ready);
    let mut mtransport = TestTransport::with_capacity(128 * 1024);
    for step in 0..2 {
        if step == 1 {
            let _ = mtransport.drain_outgoing(&mut scratch);
            let n = bn_upgrade_reply(0xBA09u64, &mut resp);
            mtransport.inject_incoming(&resp[..n]);
        }
        bwl::drive_one(&mut mtransport, &mut mdriver, b"h", b"/", &mut prod, &mut etx, armed, &mut otx, &status, &mut capture)
            .unwrap();
    }
    assert_eq!(mdriver.state(), bwl::State::Steady);
    let mpayload: &[u8] = br#"{"e":"markPriceUpdate","E":1790000000000,"s":"BTCUSDT","p":"65000.10","i":"64998.20","P":"64999.00","r":"0.00010000","T":1790006400000}"#;
    assert!(mpayload.len() > 125);
    let mut mark_frame = [0u8; 512];
    mark_frame[0] = 0x81;
    mark_frame[1] = 126;
    mark_frame[2..4].copy_from_slice(&(mpayload.len() as u16).to_be_bytes());
    mark_frame[4..4 + mpayload.len()].copy_from_slice(mpayload);
    let mark_len = 4 + mpayload.len();
    let g3 = AllocGuard::new();
    let (mut marks, mut fundings) = (0u64, 0u64);
    for _ in 0..1_000u32 {
        assert_eq!(mtransport.inject_incoming(&mark_frame[..mark_len]), mark_len);
        bwl::drive_one(&mut mtransport, &mut mdriver, b"h", b"/", &mut prod, &mut etx, armed, &mut otx, &status, &mut capture)
            .unwrap();
        while let Some(ev) = erx.try_pop_ref() {
            if ev.channel == core_types::ChannelId::Mark as u8 {
                assert_eq!(ev.flags, core_types::EVENT_FLAG_EXEC_ONLY);
                marks += 1;
            } else {
                fundings += 1;
            }
        }
    }
    let (allocs, bytes, _deallocs) = g3.delta();
    assert_eq!(allocs, 0, "the armed Binance mark lane allocated {allocs} times ({bytes} B)");
    assert_eq!((marks, fundings), (1_000, 1_000), "every mark flagged onto the lane, then its funding");

    assert_eq!(status.parse_errors_total(), 0);
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.tap_dropped(), 0);
    assert!(capture.ticks_written() > 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// Drive the RPC ingress run-loop through 1 000 steady-state newHeads
/// notifications. Polling is suppressed so the allocation check is over
/// pure notification dispatch.
#[test]
fn rpc_run_loop_steady_state_is_zero_alloc() {
    use ingress_rpc::run_loop as rwl;

    let mut transport = TestTransport::with_capacity(128 * 1024);
    let mut driver = rwl::Driver::new(0xCAFEu64);
    rwl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<core_types::Signal, { rwl::DEFAULT_SIGNAL_RING_CAP }>> =
        Ring::new();
    let (mut prod, mut cons) = ring.split();

    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("rpc_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "rpc",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    rwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut scratch = [0u8; 4096];
    let _ = transport.drain_outgoing(&mut scratch);

    let key = core_net::sec_websocket_key_from_seed(0xCAFEu64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    rwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), rwl::State::Steady);
    // Drain subscribe request so the tx buffer stays cursor=0.
    let _ = transport.drain_outgoing(&mut scratch);
    // Drain the subscribe-tracking pending signal from the ring.
    let _ = cons.try_pop_ref().as_deref().copied();

    // Canned newHeads notification frame — use medium length (<65k) because
    // the JSON is >125 B.
    let payload: &[u8] = br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xab","result":{"number":"0x1234","timestamp":"0x5faa","gasUsed":"0x7a1200","hash":"0xdeadbeef"}}}"#;
    let mut frame = [0u8; 512];
    let frame_len;
    if payload.len() <= 125 {
        frame[0] = 0x81;
        frame[1] = payload.len() as u8;
        frame[2..2 + payload.len()].copy_from_slice(payload);
        frame_len = 2 + payload.len();
    } else {
        frame[0] = 0x81;
        frame[1] = 126;
        let len_be = (payload.len() as u16).to_be_bytes();
        frame[2] = len_be[0];
        frame[3] = len_be[1];
        frame[4..4 + payload.len()].copy_from_slice(payload);
        frame_len = 4 + payload.len();
    }

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut acc: u64 = 0;
    for _ in 0..1_000u32 {
        let written = transport.inject_incoming(&frame[..frame_len]);
        assert_eq!(written, frame_len);
        rwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut capture,
        )
        .unwrap();
        // Drain the Signal so the ring doesn't fill.
        if let Some(s) = cons.try_pop_ref().as_deref().copied() {
            acc = acc.wrapping_add(u64::from_le_bytes(s.payload[0..8].try_into().unwrap()));
        }
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "rpc run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "rpc run-loop bytes should be zero: saw {bytes}");
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.tap_dropped(), 0);
    assert!(capture.signals_written() > 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

// ---------------------------------------------------------------
// Phase 2: book-builder + strategy hot-path assertions
// ---------------------------------------------------------------

/// Drive `MultiBook<8>::apply` 10_000x over four cycling symbols —
/// must be zero-alloc after `track`.
#[test]
fn multi_book_apply_is_zero_alloc() {
    use book_builder::MultiBook;

    let mut mb: MultiBook<8> = MultiBook::empty();
    mb.track(10).unwrap();
    mb.track(20).unwrap();
    mb.track(30).unwrap();
    mb.track(40).unwrap();

    // Preallocated tick scratch — bumped each iter via `venue_seq`.
    let tick = |sym: SymbolId, seq: u32| -> Tick {
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            seq,
            Price::from_raw(500_000),
            Qty::from_raw(100),
            Price::from_raw(510_000),
            Qty::from_raw(50),
        )
    };

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for i in 0..10_000u32 {
        let sym = match i % 4 {
            0 => 10,
            1 => 20,
            2 => 30,
            _ => 40,
        };
        let t = tick(sym, i + 1);
        mb.apply(&t);
        acc = acc.wrapping_add(mb.snapshot(sym).unwrap().bid_px.raw());
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "MultiBook::apply allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// Drive `LatencyArb<8>::on_tick` 10_000x with a cooldown-suppressed
/// stream so the hot path runs the full compare-and-emit pipeline
/// repeatedly without actually firing. Zero-alloc.
#[test]
fn latency_arb_on_tick_is_zero_alloc() {
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_latency_arb::LatencyArb;

    struct Noop;
    impl Ctx for Noop {
        fn submit(&mut self, _order: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            1_000_000
        }
    }

    let mut strat: LatencyArb<8> = LatencyArb::new();
    strat.add_pair(100, 200).unwrap();
    strat.set_threshold(20_000);
    strat.set_qty(Qty::from_raw(1_000_000));
    // Set cooldown longer than the simulated clock so we measure
    // the no-emit branch (the emit branch isn't hot — it allocates
    // an Order on the stack and forwards through ctx, both of which
    // are checked separately).
    strat.set_cooldown_ns(u64::MAX);

    let mut ctx = Noop;
    // Prime the Binance reference mid.
    let bn_tick = Tick::new(
        0,
        VenueId::Binance,
        200,
        1,
        Price::from_raw(499_000),
        Qty::from_raw(10),
        Price::from_raw(501_000),
        Qty::from_raw(10),
    );
    strat.on_tick(&bn_tick, &mut ctx);
    // Prime the PM book.
    let pm_tick = Tick::new(
        0,
        VenueId::Polymarket,
        100,
        1,
        Price::from_raw(599_000),
        Qty::from_raw(10),
        Price::from_raw(601_000),
        Qty::from_raw(10),
    );
    strat.on_tick(&pm_tick, &mut ctx);

    let g = AllocGuard::new();
    for i in 0..10_000u32 {
        // Alternate PM and BN ticks to exercise both branches of
        // the on_tick dispatcher. Bump venue_seq to avoid stale
        // drops.
        let t = if i % 2 == 0 {
            Tick::new(
                0,
                VenueId::Polymarket,
                100,
                2 + i,
                Price::from_raw(599_000),
                Qty::from_raw(10),
                Price::from_raw(601_000),
                Qty::from_raw(10),
            )
        } else {
            Tick::new(
                0,
                VenueId::Binance,
                200,
                2 + i,
                Price::from_raw(499_000),
                Qty::from_raw(10),
                Price::from_raw(501_000),
                Qty::from_raw(10),
            )
        };
        strat.on_tick(&t, &mut ctx);
    }
    std::hint::black_box(strat.pm_ticks_seen + strat.bn_ticks_seen);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "LatencyArb::on_tick allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

// ---------------------------------------------------------------
// Phase 3: signer + JSON encoder hot-path assertions
// ---------------------------------------------------------------

/// Encode a Polymarket-shaped POST body 10_000x into the same
/// preallocated buffer — must be zero-alloc.
#[test]
fn live_dispatcher_encode_is_zero_alloc() {
    use clob_dispatcher::{encode_signed_order, json_encoder::ORDER_TYPE_GTC};
    use signer_eip712::OrderToSign;

    let order = OrderToSign::new(
        42,
        [0xAAu8; 20],
        [0xAAu8; 20],
        [0u8; 20],
        [0x7au8; 32],
        10_000_000,
        5_000_000,
        0,
        0,
        0,
        0,
        0,
    );
    let sig = [0x12u8; 65];
    let owner = [0xAAu8; 20];
    let mut buf = [0u8; 4096];

    // Prime: a single encode to warm any one-time setup.
    let _ = encode_signed_order(&mut buf, &order, &sig, &owner, ORDER_TYPE_GTC).unwrap();

    let g = AllocGuard::new();
    let mut acc: usize = 0;
    for _ in 0..10_000u32 {
        let n = encode_signed_order(&mut buf, &order, &sig, &owner, ORDER_TYPE_GTC).unwrap();
        acc = acc.wrapping_add(n);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "encode_signed_order allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// Sign a canned order 100x and assert the per-call allocation
/// budget stays within the documented limit.
///
/// The `secp256k1` crate's `sign_ecdsa_recoverable` allocates a
/// single ~208 B context per call inside `libsecp256k1`. That cost
/// is acceptable at our submit rate (≤ a few orders / sec under the
/// strategy cooldown). Phase 3 documents this budget rather than
/// chase it; if/when secp256k1 grows a no-alloc API we tighten the
/// bound.
///
/// The non-secp256k1 layers — typehash caches (`OnceLock`), the
/// EIP-712 byte-pack into a 416-byte stack buffer — are themselves
/// zero-alloc.
#[test]
fn signer_sign_order_per_call_budget_holds() {
    use signer_eip712::{sign_order, OrderToSign};

    let mut key = [0u8; 32];
    key[31] = 1;
    let order = OrderToSign::new(
        42,
        [0xAAu8; 20],
        [0xAAu8; 20],
        [0u8; 20],
        [0x7au8; 32],
        10_000_000,
        5_000_000,
        0,
        0,
        0,
        0,
        0,
    );

    // Prime: warm the OnceLock typehashes + the secp256k1 context.
    let _ = sign_order(&order, &key).unwrap();

    const ITERS: u64 = 100;
    /// Per-call alloc budget. libsecp256k1's `sign_ecdsa_recoverable`
    /// reserves ~208 B per signature; we allow 1 alloc + ≤ 256 B
    /// per call.
    const PER_CALL_ALLOCS: u64 = 1;
    const PER_CALL_BYTES: u64 = 256;

    let g = AllocGuard::new();
    let mut acc: u8 = 0;
    for _ in 0..ITERS {
        let sig = sign_order(&order, &key).unwrap();
        acc = acc.wrapping_add(sig[64]);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        allocs <= ITERS * PER_CALL_ALLOCS,
        "sign_order alloc count {allocs} > budget {} for {ITERS} calls",
        ITERS * PER_CALL_ALLOCS
    );
    assert!(
        bytes <= ITERS * PER_CALL_BYTES,
        "sign_order byte count {bytes} > budget {} for {ITERS} calls",
        ITERS * PER_CALL_BYTES
    );
}

// ---------------------------------------------------------------
// Phase 4: metrics + latency hot-path assertions (the RG6 /state
// snapshot gate 43 sits at the end of the file)
// ---------------------------------------------------------------

/// 10 000 counter increments through a registered counter must be
/// zero-alloc. The registry was sized at boot; `inc` is just a
/// relaxed atomic add.
#[test]
fn metrics_counter_inc_is_zero_alloc() {
    use core_metrics::MetricsRegistry;
    let mut reg = MetricsRegistry::new();
    let id = reg.register_counter("engine_ticks_total").unwrap();
    let c = reg.counter(id);

    let g = AllocGuard::new();
    for _ in 0..10_000u32 {
        c.inc(1);
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "counter.inc allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// 10 000 latency samples through `LatencyTracker<24>` — must be
/// zero-alloc. Per-sample work is one bit-scan + three atomic
/// updates.
#[test]
fn latency_record_is_zero_alloc() {
    use core_latency::LatencyTracker;
    let t: LatencyTracker<24> = LatencyTracker::new();

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        // Mix of sample sizes to exercise different bucket rows.
        let ns = 100u64.wrapping_add(i * 7);
        t.record(ns);
        acc = acc.wrapping_add(ns);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "LatencyTracker::record allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

// ---------------------------------------------------------------
// Phase 5: Strategy A (EvStrategy) hot-path assertion
// ---------------------------------------------------------------

/// Drive `EvStrategy<8>::on_tick` 10 000x with a cooldown-
/// suppressed stream so the lookup + decision pipeline runs
/// repeatedly without emitting. Zero-alloc.
#[test]
fn ev_strategy_on_tick_is_zero_alloc() {
    use research_artifacts::{Family, Impact};
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_ev::EvStrategy;

    struct Noop;
    impl Ctx for Noop {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            1_000_000
        }
    }

    const PM: SymbolId = 42;
    let mut s: EvStrategy<8> = EvStrategy::new();
    s.register(PM, b"0xabc").unwrap();
    s.table_mut()
        .insert(b"0xabc", 500_000, Family::Crypto, Impact::High)
        .unwrap();
    // Cooldown longer than the fake clock so we exercise the no-
    // emit branch every iteration.
    s.set_cooldown_ns(u64::MAX);
    s.set_threshold(20_000);

    let mut ctx = Noop;
    // Prime by feeding one tick.
    let prime = Tick::new(
        0,
        VenueId::Polymarket,
        PM,
        1,
        Price::from_raw(690_000),
        Qty::from_raw(10),
        Price::from_raw(710_000),
        Qty::from_raw(10),
    );
    s.on_tick(&prime, &mut ctx);

    let g = AllocGuard::new();
    for i in 0..10_000u32 {
        let t = Tick::new(
            0,
            VenueId::Polymarket,
            PM,
            2 + i,
            Price::from_raw(690_000),
            Qty::from_raw(10),
            Price::from_raw(710_000),
            Qty::from_raw(10),
        );
        s.on_tick(&t, &mut ctx);
    }
    std::hint::black_box(s.pm_ticks_seen);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "EvStrategy::on_tick allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

// ---------------------------------------------------------------
// Phase 6: Strategy C + Strategy D hot-path assertions
// ---------------------------------------------------------------

/// Drive `CrossArb<4, 3>::on_tick` 10 000x cycling over three
/// group members. Cooldown is `u64::MAX` so the emit branch never
/// fires; we measure the lookup + sum + threshold-compare pipeline.
#[test]
fn cross_arb_on_tick_is_zero_alloc() {
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_cross_arb::CrossArb;

    struct Noop;
    impl Ctx for Noop {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            1_000_000
        }
    }

    let mut s: CrossArb<4, 3> = CrossArb::new();
    s.set_threshold(20_000);
    s.set_qty(Qty::from_raw(1_000_000));
    s.set_cooldown_ns(u64::MAX); // never fire — measure no-op path
    s.register_group(&[10, 11, 12]).unwrap();

    let mut ctx = Noop;
    // Prime each member with one tick so subsequent applies hit
    // the "stale seq" branch (still zero-alloc).
    for sym in [10, 11, 12] {
        s.on_tick(
            &Tick::new(
                0,
                VenueId::Polymarket,
                sym,
                1,
                Price::from_raw(333_330),
                Qty::from_raw(10),
                Price::from_raw(333_336),
                Qty::from_raw(10),
            ),
            &mut ctx,
        );
    }

    let g = AllocGuard::new();
    for i in 0..10_000u32 {
        let sym = match i % 3 {
            0 => 10,
            1 => 11,
            _ => 12,
        };
        let t = Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            2 + i,
            Price::from_raw(333_330),
            Qty::from_raw(10),
            Price::from_raw(333_336),
            Qty::from_raw(10),
        );
        s.on_tick(&t, &mut ctx);
    }
    std::hint::black_box(s.pm_ticks_seen);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "CrossArb::on_tick allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// Drive `RuleTree<4>::on_signal` 10 000x against a registered
/// rule. Cooldown is `u64::MAX` so we measure the keyword-scan +
/// book-snapshot + compare pipeline without emit.
#[test]
fn rule_tree_on_signal_is_zero_alloc() {
    use core_types::{LatencyClass, Signal, SignalSource};
    use research_artifacts::RulesTable;
    use std::io::Write;
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_rule_tree::RuleTree;

    struct Noop;
    impl Ctx for Noop {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            1_000_000
        }
    }

    // Boot-time: write a single rule + load through the public
    // loader.
    let dir = std::env::temp_dir();
    let p = dir.join(format!("ra_strat_d_alloc_{}.json", std::process::id()));
    {
        let mut f = std::fs::File::create(&p).unwrap();
        write!(
            f,
            r#"[{{"name":"r","family":"crypto","trigger":"t","edge_bps":20,"horizon_ms":1000,"max_risk_usd":50}}]"#
        )
        .unwrap();
    }
    let (table, _) = RulesTable::<4>::load_json(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    let r = *table.slice().first().unwrap();

    let mut s: RuleTree<4> = RuleTree::new();
    s.set_floor_edge_bps(10);
    s.set_qty(Qty::from_raw(1_000_000));
    s.set_cooldown_ns(u64::MAX);
    s.add_rule(r, 42, b"halving").unwrap();

    let mut ctx = Noop;
    // Prime the book.
    s.on_tick(
        &Tick::new(
            0,
            VenueId::Polymarket,
            42,
            1,
            Price::from_raw(290_000),
            Qty::from_raw(10),
            Price::from_raw(310_000),
            Qty::from_raw(10),
        ),
        &mut ctx,
    );

    let mut payload = [0u8; 40];
    payload[..15].copy_from_slice(b"halving inbound");
    let sig = Signal::new(0, 42, LatencyClass::Warm, SignalSource::Rpc as u8, payload);

    let g = AllocGuard::new();
    for _ in 0..10_000u32 {
        s.on_signal(&sig, &mut ctx);
    }
    std::hint::black_box(s.signals_seen);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "RuleTree::on_signal allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

// ---------------------------------------------------------------
// Audit-fix: engine latency tracker on the hot path
// ---------------------------------------------------------------

/// Engine.tick() with per-stage LatencyTracker recording must stay
/// zero-alloc. Boots a real Engine + PaperDispatcher + Counter
/// strategy, pre-fills the PM tick ring, drains it 10 000 times
/// while latency is sampled.
#[test]
fn engine_tick_with_latency_record_is_zero_alloc() {
    use clob_dispatcher::{OrderDispatch, PaperDispatcher};
    use engine::{
        Engine, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES, SIGNAL_RING_SIZE, TICK_RING_SIZE,
    };
    use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

    // The lane arrays below are written out for the lane geometry
    // (eight tick lanes since HC1 added Hypercall at lane 7, after
    // MX2's MEXC at lane 6 and WS9's Bybit at lane 5; four opt lanes
    // since HC1; five fill lanes since BX3 gave Binance lane 4); break
    // the build loudly if that drifts.
    const _: () = assert!(NUM_TICK_LANES == 10 && engine::NUM_OPT_LANES == 4 && NUM_FILL_LANES == 5);

    struct NoopStrat;
    impl StrategyCounters for NoopStrat {}
    impl Strategy for NoopStrat {
        fn on_start<C: Ctx>(&mut self, _c: &mut C) -> Result<(), StrategyError> {
            Ok(())
        }
        fn on_tick<C: Ctx>(&mut self, _t: &core_types::Tick, _c: &mut C) {}
        fn on_signal<C: Ctx>(&mut self, _s: &core_types::Signal, _c: &mut C) {}
        fn on_fill<C: Ctx>(&mut self, _f: &core_types::Fill, _c: &mut C) {}
        fn on_timer<C: Ctx>(&mut self, _n: core_time::NsTs, _c: &mut C) {}
        fn timer_period_ns(&self) -> u64 {
            u64::MAX
        }
        fn on_stop<C: Ctx>(&mut self, _c: &mut C) {}
    }
    // `SubmitErr` is imported above so the trait bound resolves;
    // unused inside this test fixture.
    let _ = std::marker::PhantomData::<SubmitErr>;

    // Lane arrays: eight tick lanes (Polymarket, Binance, OKX,
    // Deribit, Hyperliquid, Bybit — WS9, MEXC — MX2, Hypercall — HC1)
    // + five fill lanes. Only
    // lane 0 (Polymarket) gets a live producer here; the unused
    // producer halves stay alive until end of scope, and their lanes
    // simply read empty every iteration.
    let (mut pm_p, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t1p, t1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t2p, t2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t3p, t3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t4p, t4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t5p, t5) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t6p, t6) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t7p, t7) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // PM15: lane 8, the rolling families.
    let (_t8p, t8) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // PMJ J1b: lane 9, the 5-minute families.
    let (_t9p, t9) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // WS10-A: eight venue-event lanes ride in every engine. Lane 2
    // (OKX) gets a live producer — the measured window below pushes
    // one funding ChannelEvent per iteration and the engine drains
    // it through `on_venue_event`, proving lane push + drain are
    // 0 B/op; the other seven read empty (two atomic loads each).
    let (mut ev2_p, e2) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e0p, e0) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e1p, e1) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e3p, e3) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e4p, e4) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e5p, e5) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e6p, e6) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e7p, e7) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e8p, e8) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let (_e9p, e9) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    // WS10-B: two depth lanes; lane 0 (OKX) live — the measured
    // window pushes one DepthTopK per iteration and the engine
    // drains it through `on_depth` (192 B Copy slot, 0 B/op).
    let (mut d0_p, d0) =
        Ring::<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>::new().split();
    let (_d1p, d1) = Ring::<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>::new().split();
    // VM2 V2: opt lanes (producer-dropped — empty-lane steady cost is
    // part of the measured window, the §3.3 shape).
    let (_o0p, o0) = Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let (_o1p, o1) = Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let (_o2p, o2) = Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let (_o3p, o3) = Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let (_sp, sc) = Ring::<core_types::Signal, SIGNAL_RING_SIZE>::new().split();
    let (_f0p, f0) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f1p, f1) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f2p, f2) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f3p, f3) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f4p, f4) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    // Phase 8f: the AI lane rides in every engine; producer-dropped
    // here so it reads empty (two atomic loads per iteration inside
    // the measured window — part of the real tick cost).
    let (_aip, ai_c) = Ring::<core_types::AiCmd, { core_types::AI_RING_SIZE }>::new().split();
    // Phase 8g item 7: the ruleset table lane rides in every engine
    // too; producer-dropped so its pre-AI-drain pop reads empty (one
    // acquire load per iteration inside the measured window — the §6
    // steady-state cost of the lane; the loaded pop→receive_table
    // path is gate 35's seam).
    let (_tblp, tbl_c) =
        Ring::<core_types::RuleTableSlot, { core_types::RULE_TABLE_RING_SLOTS }>::new().split();
    // XMM XH1: the trade and order-event lanes, both live — one print
    // and one order event per iteration ride the same 0 B/op assertion.
    let (mut tr_p, tr_c) =
        Ring::<core_types::TradePrint, { core_types::TRADE_RING_SIZE }>::new().split();
    let (mut oe_p, oe_c) =
        Ring::<core_types::OrderEvent, { core_types::ORDER_EVENT_RING_SIZE }>::new().split();

    let mut eng = Engine::new(
        NoopStrat,
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4, t5, t6, t7, t8, t9],
        [e0, e1, e2, e3, e4, e5, e6, e7, e8, e9],
        [d0, d1],
        [o0, o1, o2, o3],
        sc,
        [f0, f1, f2, f3, f4],
        ai_c,
        std::sync::Arc::new(AiIngressStatus::new()),
        tbl_c,
    );
    eng.set_trade_lane(tr_c);
    eng.set_order_event_lane(oe_c);
    // XMM XH5d: the order-event capture rides both drains — the lane's
    // events and the paper arm's below are staged inside the window.
    let ev_dir = std::env::temp_dir().join(format!("xh5d_alloc_events_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ev_dir);
    std::fs::create_dir_all(&ev_dir).unwrap();
    eng.set_event_capture(
        core_io::SlotCapture::open(
            ev_dir.join(engine::ENGINE_EVENTS_FILE),
            core_io::SlotKind::OrderEvent,
            1,
        )
        .unwrap(),
    );
    // XMM XH2: the paper arm's queue law on a tracked Hyperliquid perp —
    // each iteration a post-only bid lands on the print that goes
    // through it, so the dispatcher's order-event pump runs its `true`
    // arm (RESTING, FILLED) and the fill pump its queue fill, inside the
    // measured window.
    let hl_sym = core_types::make_symbol_id(VenueId::Hyperliquid, 1);
    eng.dispatcher_mut().track_queue_sym(hl_sym);
    eng.start().unwrap();

    // Prime + drain a few ticks outside the measurement window.
    for i in 0..16u32 {
        assert!(pm_p.try_push_ref(&Tick::new(
            i as u64,
            VenueId::Polymarket,
            1,
            i + 1,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        )));
    }
    eng.tick(64);

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    for i in 0..10_000u32 {
        // Push one tick + one funding event, drain both (WS10-A: the
        // event-lane push + `on_venue_event` drain ride the same
        // 0 B/op assertion).
        assert!(pm_p.try_push_ref(&Tick::new(
            (i as u64) * 1000,
            VenueId::Polymarket,
            1,
            i + 100,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        )));
        assert!(ev2_p.try_push_ref(&core_types::ChannelEvent::new(
            (i as u64) * 1000,
            VenueId::Okx,
            core_types::ChannelId::Funding,
            1,
            0,
            0,
            125,
            0,
        )));
        assert!(d0_p.try_push_ref(&core_types::DepthTopK::EMPTY));
        let mut bid = core_types::Order::new(
            0,
            VenueId::Hyperliquid,
            hl_sym,
            core_types::Side::Bid,
            0,
            Price::from_raw(100_000_000),
            Qty::from_raw(1_000_000),
            u64::from(i) + 1,
        )
        .with_post_only();
        bid.strategy_id = 6;
        eng.dispatcher_mut().submit(&bid).unwrap();
        assert!(tr_p.try_push_ref(&core_types::TradePrint::new(
            (i as u64) * 1000,
            VenueId::Hyperliquid,
            hl_sym,
            u64::from(i),
            0,
            99_900_000,
            1_000_000,
            core_types::TRADE_AGGRESSOR_SELL,
        )));
        assert!(oe_p.try_push_ref(&core_types::OrderEvent::new(
            (i as u64) * 1000,
            VenueId::Hyperliquid,
            1,
            u64::from(i),
            6,
            core_types::ORDER_EVENT_RESTING,
            core_types::ORDER_EVENT_REASON_NONE,
            0,
        )));
        // Budget 2: one record per lane, and the paper arm's two events.
        eng.tick(2);
        acc = acc.wrapping_add(eng.ingest_p50_ns());
    }
    std::hint::black_box(acc);
    assert_eq!(eng.events_dispatched, 10_000, "event lane drained");
    assert_eq!(eng.depths_dispatched, 10_000, "depth lane drained");
    assert_eq!(eng.trades_dispatched, 10_000, "trade lane drained");
    assert_eq!(
        eng.order_events_dispatched, 30_000,
        "the lane's 10 000, and the paper arm's RESTING + FILLED per bid"
    );
    assert_eq!(eng.fills_dispatched, 10_000, "one queue fill per bid");

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "engine.tick() with latency record allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
    assert_eq!(
        eng.event_capture_records(),
        eng.order_events_dispatched,
        "every drained order event staged, once"
    );
    assert_eq!(eng.event_capture_io_errors(), 0);
    drop(eng);
    let _ = std::fs::remove_dir_all(&ev_dir);
}

/// QueuedDispatcher worker drain — engine pushes 1000 orders into
/// the SPSC ring, worker drains them into a PaperDispatcher. The
/// hot path (try_pop_ref → inner.submit → atomic stats mirror) must
/// be zero-alloc. Boot allocations (Ring, Arc&lt;DispatchStatsAtomic&gt;)
/// happen before the guard.
#[test]
fn queued_dispatcher_worker_drain_is_zero_alloc() {
    use clob_dispatcher::{OrderDispatch, PaperDispatcher, QueuedDispatcher};
    use core_types::{Order, Side};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (mut queued, worker) = QueuedDispatcher::new(PaperDispatcher::new());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    // Pre-warm: route one order so the worker thread runs at
    // least one cycle before we open the AllocGuard.
    let o = Order::new(
        0,
        VenueId::Polymarket,
        1,
        Side::Bid,
        0,
        Price::from_raw(500_000),
        Qty::from_raw(1_000_000),
        1,
    );
    queued.submit(&o).expect("warmup push");
    let h = std::thread::spawn(move || worker.run(&stop_w));
    // Spin until stats reflect the warmup, then start counting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while queued.stats().accepted == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_micros(50));
    }

    let g = AllocGuard::new();
    for i in 0..1000u64 {
        let o = Order::new(
            0,
            VenueId::Polymarket,
            1,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            i + 2,
        );
        let _ = queued.submit(&o);
    }
    // Wait for the worker to fully drain so the stats reflect the
    // post-warmup orders too. The wait happens inside the guard,
    // but the operations inside it (load + sleep on a literal
    // Duration) don't allocate.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while queued.stats().accepted < 1001 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    let (allocs, bytes, _) = g.delta();

    stop.store(true, Ordering::Release);
    h.join().expect("worker join");

    assert_eq!(
        allocs, 0,
        "queued dispatcher worker drain allocated {allocs} times ({bytes} B)"
    );
}

// ---------------------------------------------------------------
// Phase 8b: OKX ingress hot-path assertions
// ---------------------------------------------------------------

/// Run classify + all five OKX channel parsers over fixed realistic
/// samples (mirrors the ingress-okx unit-test corpus) for 10_000
/// iterations each — must be zero-alloc.
#[test]
fn okx_parsers_are_zero_alloc() {
    let bbo: &[u8] = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","55154","0","2"]],"bids":[["111.05","57745","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
    let trade: &[u8] = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"130639474","px":"42219.9","sz":"0.12060306","side":"buy","ts":"1630048897897","count":"3","seqId":123456}]}"#;
    let mark: &[u8] = br#"{"arg":{"channel":"mark-price","instId":"BTC-USD-SWAP"},"data":[{"instType":"SWAP","instId":"BTC-USD-SWAP","markPx":"42310.6","ts":"1630049455539"}]}"#;
    let funding: &[u8] = br#"{"arg":{"channel":"funding-rate","instId":"BTC-USD-SWAP"},"data":[{"fundingRate":"0.0000593","fundingTime":"1630051200000","instId":"BTC-USD-SWAP","instType":"SWAP","ts":"1630048897897"}]}"#;
    let book: &[u8] = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["8476.98","415","0","13"]],"bids":[["8476.97","256","0","12"]],"ts":"1597026383085","checksum":0,"prevSeqId":-1,"seqId":123456}]}"#;
    // Venue-namespaced symbol (venue byte 2 = Okx, ordinal 1).
    let sym: SymbolId = (2 << 24) | 1;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        std::hint::black_box(ingress_okx::classify(bbo));
        std::hint::black_box(ingress_okx::classify(trade));
        std::hint::black_box(ingress_okx::classify(mark));
        std::hint::black_box(ingress_okx::classify(funding));
        std::hint::black_box(ingress_okx::classify(book));
        let mut b = ingress_okx::OkxBboFrame::ZERO;
        assert!(ingress_okx::parse_bbo(bbo, sym, &mut b));
        acc = acc.wrapping_add(b.bid_px_1e6);
        let mut t = ingress_okx::OkxTradeFrame::ZERO;
        assert!(ingress_okx::parse_trade(trade, sym, &mut t));
        acc = acc.wrapping_add(t.px_1e6);
        let mut m = ingress_okx::OkxMarkPriceFrame::ZERO;
        assert!(ingress_okx::parse_mark_price(mark, sym, &mut m));
        acc = acc.wrapping_add(m.mark_px_1e6);
        let mut f = ingress_okx::OkxFundingFrame::ZERO;
        assert!(ingress_okx::parse_funding_rate(funding, sym, &mut f));
        acc = acc.wrapping_add(f.funding_rate_1e9);
        let mut h = ingress_okx::OkxBookFrame::ZERO;
        assert!(ingress_okx::parse_book_header(book, sym, &mut h));
        acc = acc.wrapping_add(h.seq_id);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "okx parsers allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "okx parser bytes should be zero: saw {bytes}");
}

/// Drive the OKX ingress run-loop through 1 000 pre-injected
/// steady-state frames (bbo-tbt / trades / correctly chained books)
/// via a `TestTransport`. Steady state is reached over the real
/// handshake path; the entire scripted stream is injected before the
/// guard; every `drive_one` call must allocate zero bytes.
#[test]
fn okx_run_loop_steady_state_is_zero_alloc() {
    use ingress_okx::run_loop as owl;

    // ---- boot (NOT measured) ----
    // Capacity must hold the whole scripted stream at once (~190 KiB
    // for 1 000 frames): the transport's buffers are fixed-size and
    // nothing drains them until the measurement loop runs.
    let mut transport = TestTransport::with_capacity(256 * 1024);

    // Venue-namespaced symbol (venue byte 2 = Okx, ordinal 1).
    let sym: SymbolId = (2 << 24) | 1;
    let mut symbols = ingress_okx::OkxSymbolTable::new();
    symbols
        .insert(b"BTC-USDT", sym, ingress_okx::OkxInstType::Spot)
        .unwrap();
    let mut driver = owl::Driver::new(0x0C0Cu64, symbols, true, &[]);
    owl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    // VM2 V2: hoisted throwaway opt lane — created OUTSIDE the
    // AllocGuard window (Ring::new allocates).
    let (mut otx, _orx) =
        Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { owl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    // WS10-A: event lane built boot-side; the measured pushes/
    // drops below must be 0 B/op like everything else.
    let event_ring: std::sync::Arc<
        Ring<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>,
    > = Ring::new();
    let (mut etx, _erx) = event_ring.split();
    let depth_ring: std::sync::Arc<Ring<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>> =
        Ring::new();
    let (mut dtx, _drx) = depth_ring.split();

    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("okx_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "okx",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    // Send the client GET handshake.
    owl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut dtx,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut scratch = [0u8; 4096];
    let _ = transport.drain_outgoing(&mut scratch);

    // Inject the 101 reply matching the seed.
    let key = core_net::sec_websocket_key_from_seed(0x0C0Cu64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    owl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut dtx,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), owl::State::Steady);
    // Drain the batched subscribe op so the tx buffer stays empty.
    let _ = transport.drain_outgoing(&mut scratch);

    // Pre-build + pre-inject the full scripted stream: one books
    // snapshot, then cycles of [bbo-tbt, trades, books update]. The
    // updates chain correctly (prevSeqId == prior seqId) so the §6.2
    // resync path stays cold — no resubscribes fire inside the
    // measurement window. Trades reuse one seqId (equal ids are
    // legal). `inject_incoming` may compact/copy; all of it happens
    // here, before the guard.
    const CYCLES: usize = 333; // 1 snapshot + 3 × 333 = 1 000 frames
    const BBO: &[u8] = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT"},"data":[{"asks":[["111.06","55154","0","2"]],"bids":[["111.05","57745","0","2"]],"ts":"1670324386802","seqId":363996337}]}"#;
    const TRADE: &[u8] = br#"{"arg":{"channel":"trades","instId":"BTC-USDT"},"data":[{"instId":"BTC-USDT","tradeId":"130639474","px":"42219.9","sz":"0.12060306","side":"buy","ts":"1630048897897","count":"3","seqId":123456}]}"#;
    const BOOK_SNAP: &[u8] = br#"{"arg":{"channel":"books","instId":"BTC-USDT"},"action":"snapshot","data":[{"asks":[["8476.98","415","0","13"]],"bids":[["8476.97","256","0","12"]],"ts":"1597026383085","checksum":0,"prevSeqId":-1,"seqId":123456}]}"#;

    /// Unmasked (server→client) WS text frame appended to `stream`.
    fn push_text_frame(stream: &mut Vec<u8>, body: &[u8]) {
        stream.push(0x81);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            assert!(body.len() <= u16::MAX as usize);
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }

    let mut stream: Vec<u8> = Vec::with_capacity(220 * 1024);
    push_text_frame(&mut stream, BOOK_SNAP);
    let mut seq: i64 = 123_456; // BOOK_SNAP's seqId — chain root
    for _ in 0..CYCLES {
        push_text_frame(&mut stream, BBO);
        push_text_frame(&mut stream, TRADE);
        let upd = format!(
            r#"{{"arg":{{"channel":"books","instId":"BTC-USDT"}},"action":"update","data":[{{"asks":[["8476.98","415","0","13"]],"bids":[],"ts":"1597026383217","checksum":0,"prevSeqId":{},"seqId":{}}}]}}"#,
            seq,
            seq + 1
        );
        push_text_frame(&mut stream, upd.as_bytes());
        seq += 1;
    }
    let injected = transport.inject_incoming(&stream);
    assert_eq!(
        injected,
        stream.len(),
        "transport capacity must hold the full scripted stream"
    );

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut drives = 0u32;
    while transport.incoming_len() > 0 {
        owl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut dtx,
            &mut otx,
            &status,
            &mut capture,
        )
        .unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    // Drain the bbo ticks — one per cycle. try_pop_ref is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(t.bid_px.raw());
        popped += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // Steady state consumed the whole script: one tick per bbo frame,
    // every frame counted, no losses, no chain breaks, no resyncs.
    assert_eq!(popped, CYCLES);
    assert_eq!(status.msgs_total(), (1 + 3 * CYCLES) as u64);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.gaps_total(), 0);
    assert_eq!(status.resubscribes_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(
        allocs, 0,
        "okx run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "okx run-loop bytes should be zero: saw {bytes}");

    // Capture accounting: one tick per bbo, one event per trade row +
    // one per books frame (snapshot + updates), one tap record per
    // data payload, no I/O errors, nothing dropped.
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.ticks_written(), CYCLES as u64);
    assert_eq!(capture.events_written(), (2 * CYCLES + 1) as u64);
    assert_eq!(capture.tap_records(), (1 + 3 * CYCLES) as u64);
    assert_eq!(capture.tap_dropped(), 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

// ---------------------------------------------------------------
// Phase 8c: Deribit ingress hot-path assertions
// ---------------------------------------------------------------

/// Run classify + all four Deribit channel parsers (+ instrument
/// extraction) over fixed realistic samples (mirrors the
/// ingress-deribit unit-test corpus) for 10_000 iterations each —
/// must be zero-alloc.
#[test]
fn deribit_parsers_are_zero_alloc() {
    let quote: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
    let ticker: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"timestamp":1550652954406,"open_interest":18918470,"min_price":3943.21,"max_price":3982.84,"mark_price":3940.06,"index_price":3931.73,"current_funding":0.00042}}}"#;
    let trade_row: &[u8] = br#""trade_seq":30289442,"trade_id":"48079269","timestamp":1590484512188,"tick_direction":2,"price":8950.0,"mark_price":8948.9,"instrument_name":"BTC-PERPETUAL","index_price":8955.88,"direction":"sell","amount":10.0}"#;
    let book: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1554373962454,"instrument_name":"BTC-PERPETUAL","change_id":297217105,"bids":[["new",5042.34,30.0],["new",5041.94,20.0]],"asks":[["new",5042.64,40.0]],"type":"snapshot"}}}"#;
    let test_req: &[u8] =
        br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
    // Venue-namespaced symbol (venue byte 3 = Deribit, ordinal 1).
    let sym: SymbolId = (3 << 24) | 1;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        std::hint::black_box(ingress_deribit::classify(quote));
        std::hint::black_box(ingress_deribit::classify(ticker));
        std::hint::black_box(ingress_deribit::classify(book));
        std::hint::black_box(ingress_deribit::classify(test_req));
        std::hint::black_box(ingress_deribit::extract_instrument(
            quote,
            ingress_deribit::DeribitChannel::Quote,
        ));
        let mut q = ingress_deribit::DeribitQuoteFrame::ZERO;
        assert!(ingress_deribit::parse_quote(quote, sym, &mut q));
        acc = acc.wrapping_add(q.bid_px_1e6);
        let mut k = ingress_deribit::DeribitTickerFrame::ZERO;
        assert!(ingress_deribit::parse_ticker(ticker, sym, &mut k));
        acc = acc.wrapping_add(k.mark_px_1e6);
        let mut t = ingress_deribit::DeribitTradeFrame::ZERO;
        assert!(ingress_deribit::parse_trade(trade_row, sym, &mut t));
        acc = acc.wrapping_add(t.px_1e6);
        let mut b = ingress_deribit::DeribitBookFrame::ZERO;
        assert!(ingress_deribit::parse_book_header(book, sym, &mut b));
        acc = acc.wrapping_add(b.change_id);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "deribit parsers allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "deribit parser bytes should be zero: saw {bytes}");
}

/// Drive the Deribit ingress run-loop through 1 000+ pre-injected
/// steady-state frames (quotes / trade_seq-chained trades /
/// change_id-chained books / a sprinkling of heartbeat test_requests
/// whose `public/test` answers are rendered inside the window) via a
/// `TestTransport`. Steady state is reached over the real handshake +
/// set_heartbeat + batched-subscribe + subscribe-result path; the
/// M2.3 options-analytics parsers (Deribit option `ticker`, OKX
/// `opt-summary` row, and since BX0-F2 the Binance `optionMarkPrice`
/// array walk) + the `OptSummary` record construction —
/// live-shaped payloads for 10_000 iterations each, zero-alloc (the
/// hot ingress threads run these per push).
#[test]
fn option_analytics_parsers_are_zero_alloc() {
    let deribit_opt: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-27MAR26-100000-C.100ms","data":{"timestamp":1774000000123,"instrument_name":"BTC-27MAR26-100000-C","state":"open","mark_price":0.0523,"mark_iv":65.43,"greeks":{"delta":0.512,"gamma":1.234e-5,"vega":152.3,"theta":-85.3,"rho":12.1},"open_interest":1234.5,"index_price":77216.94,"underlying_price":77300.12}}}"#;
    let okx_row: &[u8] = br#"{"instType":"OPTION","instId":"BTC-USD-260327-100000-C","uly":"BTC-USD","deltaBS":"0.512","gammaBS":"1.234e-5","thetaBS":"-85.3","vegaBS":"152.3","markVol":"0.6543","fwdPx":"77300.12","ts":"1774598400123"}"#;
    let sym: SymbolId = (3 << 24) | 513;
    // BX0-F2: the options lane's boot table (built outside the window).
    let mut bn_table = ingress_binance::eapi::EapiSymbolTable::new();
    bn_table.insert(b"BTC-260925-86000-C", (1 << 24) | 1025).unwrap();
    bn_table.insert(b"BTC-260925-86000-P", (1 << 24) | 1026).unwrap();

    let mut bn_frame = ingress_binance::eapi::EapiMarkFrame::ZERO;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    let mut bn_rows = 0u32;
    for _ in 0..10_000u32 {
        let mut f = ingress_deribit::DeribitOptTickerFrame::ZERO;
        assert!(ingress_deribit::parse_option_ticker(deribit_opt, &mut f));
        acc = acc.wrapping_add(f.mark_iv_1e9);
        let o = core_types::OptSummary::new(
            1,
            core_types::VenueId::Deribit,
            sym,
            core_types::OPT_SUMMARY_FLAG_MARK_PX | core_types::OPT_SUMMARY_FLAG_OI,
            f.mark_px_1e9,
            f.mark_iv_1e9,
            f.underlying_px_1e9,
            f.open_interest_1e6,
            f.delta_1e9,
            f.gamma_1e9,
            f.vega_1e6,
            f.theta_1e6,
        );
        acc = acc.wrapping_add(std::hint::black_box(&o).mark_px_1e9);
        let r = ingress_okx::parse_opt_summary_row(okx_row).unwrap();
        acc = acc.wrapping_add(r.fwd_px_1e9);
        std::hint::black_box(ingress_okx::extract_inst_family(okx_row));
        // BX0-F2: the options push — envelope split, array walk,
        // symbol lookup, element parse (the whole per-push path).
        let (_, tail) = ingress_binance::eapi::split_combined(BN_LIVE_MARK_ARRAY).unwrap();
        let mut cur = ingress_binance::eapi::EapiArrayCursor::new(tail).unwrap();
        while let ingress_binance::eapi::ArrayStep::Elem(e) = cur.next_elem() {
            let s = ingress_binance::eapi::eapi_elem_symbol(e).unwrap();
            if bn_table.lookup(s).is_some()
                && ingress_binance::eapi::parse_eapi_mark(e, &mut bn_frame)
            {
                acc = acc
                    .wrapping_add(bn_frame.mark_px_1e9)
                    .wrapping_add(bn_frame.index_px_1e9);
                bn_rows += 1;
            }
        }
    }
    std::hint::black_box(acc);
    assert_eq!(bn_rows, 20_000, "both selected rows of every push parsed");

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "option analytics parsers allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "option analytics parser bytes should be zero: saw {bytes}"
    );
}

/// entire scripted stream is injected before the guard; every
/// `drive_one` call must allocate zero bytes.
#[test]
fn deribit_run_loop_steady_state_is_zero_alloc() {
    use ingress_deribit::run_loop as dwl;

    // ---- boot (NOT measured) ----
    // Capacity must hold the whole scripted stream at once (~330 KiB
    // for 1 000 frames): the transport's buffers are fixed-size and
    // nothing drains them until the measurement loop runs.
    let mut transport = TestTransport::with_capacity(512 * 1024);

    // Venue-namespaced symbol (venue byte 3 = Deribit, ordinal 1).
    let sym: SymbolId = (3 << 24) | 1;
    let mut symbols = ingress_deribit::DeribitSymbolTable::new();
    symbols.insert(b"BTC-PERPETUAL", sym).unwrap();
    let mut driver = dwl::Driver::new(0x0D0Du64, symbols, true);
    dwl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    // VM2 V2: hoisted throwaway opt lane — created OUTSIDE the
    // AllocGuard window (Ring::new allocates).
    let (mut otx, _orx) =
        Ring::<core_types::OptSummary, { core_types::OPT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { dwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    // WS10-A: event lane built boot-side; the measured pushes/
    // drops below must be 0 B/op like everything else.
    let event_ring: std::sync::Arc<
        Ring<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>,
    > = Ring::new();
    let (mut etx, _erx) = event_ring.split();
    let depth_ring: std::sync::Arc<Ring<core_types::DepthTopK, { core_types::DEPTH_RING_SIZE }>> =
        Ring::new();
    let (mut dtx, _drx) = depth_ring.split();

    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("deribit_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "deribit",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    // Send the client GET handshake.
    dwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut dtx,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut scratch = [0u8; 8192];
    let _ = transport.drain_outgoing(&mut scratch);

    // Inject the 101 reply matching the seed.
    let key = core_net::sec_websocket_key_from_seed(0x0D0Du64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    dwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut dtx,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), dwl::State::Steady);
    // Drain the set_heartbeat + batched subscribe calls (ids 1, 2).
    let _ = transport.drain_outgoing(&mut scratch);

    /// Unmasked (server→client) WS text frame appended to `stream`.
    fn push_text_frame(stream: &mut Vec<u8>, body: &[u8]) {
        stream.push(0x81);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            assert!(body.len() <= u16::MAX as usize);
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }

    // Retire the session-start calls: set_heartbeat "ok" + the
    // subscribe result echoing every configured channel (depth on).
    let mut boot: Vec<u8> = Vec::with_capacity(1024);
    push_text_frame(&mut boot, br#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#);
    push_text_frame(
        &mut boot,
        br#"{"jsonrpc":"2.0","id":2,"result":["quote.BTC-PERPETUAL","ticker.BTC-PERPETUAL.100ms","trades.BTC-PERPETUAL.100ms","book.BTC-PERPETUAL.100ms"]}"#,
    );
    transport.inject_incoming(&boot);
    dwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut etx,
        core_types::EVENT_LANE_FUNDING,
        &mut dtx,
        &mut otx,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.pending_count(), 0, "session-start calls retired");
    assert_eq!(driver.sub_count(), 4, "all channels confirmed");
    let boot_msgs = status.msgs_total();

    // Pre-build + pre-inject the full scripted stream: one book
    // snapshot, then cycles of [quote, trades, book change], plus a
    // heartbeat test_request every 100 cycles (its `public/test`
    // answer renders + flushes INSIDE the measurement window — the
    // heartbeat path must be zero-alloc too). Books chain
    // (prev_change_id == prior change_id) and trades increment
    // trade_seq by exactly 1, so the §6.2 resync path stays cold.
    const CYCLES: usize = 333; // 1 snapshot + 3 × 333 + test_requests
    const QUOTE: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"quote.BTC-PERPETUAL","data":{"timestamp":1550658624149,"instrument_name":"BTC-PERPETUAL","best_bid_price":3914.97,"best_bid_amount":40.0,"best_ask_price":3996.61,"best_ask_amount":50.0}}}"#;
    const TEST_REQ: &[u8] =
        br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
    const BOOK_SNAP: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1554373962454,"instrument_name":"BTC-PERPETUAL","change_id":1000,"bids":[["new",5042.34,30.0]],"asks":[["new",5042.64,40.0]],"type":"snapshot"}}}"#;

    let mut stream: Vec<u8> = Vec::with_capacity(400 * 1024);
    push_text_frame(&mut stream, BOOK_SNAP);
    let mut change_id: i64 = 1_000; // BOOK_SNAP's change_id — chain root
    let mut trade_seq: i64 = 50_000;
    let mut test_reqs: u64 = 0;
    for c in 0..CYCLES {
        push_text_frame(&mut stream, QUOTE);
        let trade = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription","params":{{"channel":"trades.BTC-PERPETUAL.100ms","data":[{{"trade_seq":{trade_seq},"trade_id":"9","timestamp":1000,"price":8950.0,"direction":"buy","amount":10.0}}]}}}}"#
        );
        push_text_frame(&mut stream, trade.as_bytes());
        trade_seq += 1;
        let upd = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription","params":{{"channel":"book.BTC-PERPETUAL.100ms","data":{{"timestamp":2000,"instrument_name":"BTC-PERPETUAL","change_id":{},"prev_change_id":{},"bids":[["change",5042.34,31.0]],"asks":[],"type":"change"}}}}}}"#,
            change_id + 1,
            change_id
        );
        push_text_frame(&mut stream, upd.as_bytes());
        change_id += 1;
        if c % 100 == 99 {
            push_text_frame(&mut stream, TEST_REQ);
            test_reqs += 1;
        }
    }
    let injected = transport.inject_incoming(&stream);
    assert_eq!(
        injected,
        stream.len(),
        "transport capacity must hold the full scripted stream"
    );

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut drives = 0u32;
    while transport.incoming_len() > 0 {
        dwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &mut etx,
            core_types::EVENT_LANE_FUNDING,
            &mut dtx,
            &mut otx,
            &status,
            &mut capture,
        )
        .unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    // Drain the quote ticks — one per cycle. try_pop_ref is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(t.bid_px.raw());
        popped += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // Steady state consumed the whole script: one tick per quote,
    // every frame counted, no losses, no chain breaks, no resyncs,
    // and every test_request answered (in-flight `public/test` calls
    // occupy exactly `test_reqs` pending slots).
    assert_eq!(popped, CYCLES);
    assert_eq!(
        status.msgs_total() - boot_msgs,
        (1 + 3 * CYCLES) as u64 + test_reqs
    );
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.gaps_total(), 0);
    assert_eq!(status.resubscribes_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(driver.pending_count(), test_reqs as usize);
    assert_eq!(
        allocs, 0,
        "deribit run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "deribit run-loop bytes should be zero: saw {bytes}"
    );
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.tap_dropped(), 0);
    assert!(capture.ticks_written() > 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

// ---------------------------------------------------------------
// Phase 8d: Hyperliquid ingress hot-path assertions
// ---------------------------------------------------------------

/// Run classify + every Hyperliquid channel parser (+ coin
/// extraction and the subscriptionResponse echo parser) over fixed
/// realistic samples (mirrors the ingress-hyperliquid unit-test
/// corpus, HIP-4 `#<enc>` coin included) for 10_000 iterations each
/// — must be zero-alloc.
#[test]
fn hl_parsers_are_zero_alloc() {
    let bbo: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    let l2book: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"19900.0","sz":"1.0","n":1},{"px":"19899.0","sz":"2.5","n":2}],[{"px":"20100.0","sz":"1.0","n":1}]]}}"#;
    let trade: &[u8] = br#"{"coin":"BTC","side":"B","px":"19900.5","sz":"0.5","hash":"0xabc","time":1677700000000,"tid":118906512037719}"#;
    let ctx: &[u8] = br#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"funding":"0.0000125","markPx":"14.3161","openInterest":"688.11","oraclePx":"14.32"}}}"#;
    let mids: &[u8] = br#"{"channel":"allMids","data":{"mids":{"BTC":"29792.0","ETH":"1891.4"}}}"#;
    let outcome: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"kind":"outcomeCreated","coin":"#330","time":1723600000000}]}"##;
    let subresp: &[u8] = br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}}"##;
    // Venue-namespaced symbol (venue byte 4 = Hyperliquid, ordinal 1).
    let sym: SymbolId = (4 << 24) | 1;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        std::hint::black_box(ingress_hyperliquid::classify(bbo));
        std::hint::black_box(ingress_hyperliquid::classify(l2book));
        std::hint::black_box(ingress_hyperliquid::classify(ctx));
        std::hint::black_box(ingress_hyperliquid::classify(mids));
        std::hint::black_box(ingress_hyperliquid::classify(outcome));
        std::hint::black_box(ingress_hyperliquid::classify(subresp));
        std::hint::black_box(ingress_hyperliquid::extract_coin(bbo));
        let mut b = ingress_hyperliquid::HlBboFrame::ZERO;
        assert!(ingress_hyperliquid::parse_bbo(bbo, sym, &mut b));
        acc = acc.wrapping_add(b.bid_px_1e6);
        let mut l = ingress_hyperliquid::HlL2BookFrame::ZERO;
        assert!(ingress_hyperliquid::parse_l2book_header(l2book, sym, &mut l));
        acc = acc.wrapping_add(l.best_bid_px_1e6 + l.n_bids as i64);
        let mut d = core_types::DepthTopK::EMPTY;
        let mut h = ingress_hyperliquid::HlL2BookFrame::ZERO;
        assert!(ingress_hyperliquid::parse_l2book_depth(l2book, sym, 1, &mut d, &mut h));
        acc = acc.wrapping_add(d.bids[1].px_1e6 + d.asks[0].qty_1e6 + h.n_asks as i64);
        let mut t = ingress_hyperliquid::HlTradeFrame::ZERO;
        assert!(ingress_hyperliquid::parse_trade(trade, sym, &mut t));
        acc = acc.wrapping_add(t.px_1e6);
        let mut c = ingress_hyperliquid::HlAssetCtxFrame::ZERO;
        assert!(ingress_hyperliquid::parse_active_asset_ctx(ctx, sym, &mut c));
        acc = acc.wrapping_add(c.funding_1e9);
        let m = ingress_hyperliquid::parse_all_mids(mids).unwrap();
        acc = acc.wrapping_add(m as i64);
        let mut o = ingress_hyperliquid::HlOutcomeMetaFrame::ZERO;
        assert!(ingress_hyperliquid::parse_outcome_meta(outcome, &mut o));
        acc = acc.wrapping_add(o.enc as i64);
        std::hint::black_box(ingress_hyperliquid::parse_sub_response(subresp));
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hl parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hl parser bytes should be zero: saw {bytes}");
}

/// BIN15 O2: the family ROLL through the driver's real message path
/// — 64 rolls, zero allocations.
///
/// A roll rebinds two coin-table rows, queues six unsubscribes and
/// six subscribes, re-baselines two staleness stamps and emits an
/// `InstrumentRoll` event, all on the ingress thread inside the
/// `outcomeMetaUpdates` arm. Every one of those is a fixed-capacity
/// write; this gate is what keeps it that way.
///
/// 64 rolls rather than the plan's 1 000: the whole scripted stream
/// is injected before the guard opens (`inject_incoming` may copy, so
/// it cannot be measured), and one `drive_one` may then consume every
/// frame at once — 64 rolls is what `TX_BUF_SIZE` holds in a single
/// drain. The law under test is per-roll allocation, which 64
/// exercises exactly as 1 000 would.
#[test]
fn hl_family_roll_is_zero_alloc() {
    use ingress_hyperliquid::family::{rolling_sym, HlFamilyTable, HlRollStatus};
    use ingress_hyperliquid::run_loop as hwl;

    // ---- boot (NOT measured) ----
    let mut transport = TestTransport::with_capacity(512 * 1024);
    let sym_btc: SymbolId = (4 << 24) | 1;
    let mut coins = ingress_hyperliquid::HlCoinTable::new();
    coins.insert(b"BTC", sym_btc).unwrap();
    let yes = coins.reserve(rolling_sym(0, 0)).unwrap();
    let no = coins.reserve(rolling_sym(0, 1)).unwrap();
    let mut families = HlFamilyTable::new();
    let (kind, und, period) = HlFamilyTable::parse_key(b"out:BTC:15m").unwrap();
    families
        .push(
            kind,
            und,
            period,
            [yes as u8, no as u8],
            [rolling_sym(0, 0), rolling_sym(0, 1)],
        )
        .unwrap();

    // 2026-09-12T06:30:00Z, and an anchor 30 s before it: both
    // scripted instances (06:30 and 06:45) sit inside the family's
    // one-period window, deterministically, whatever this machine's
    // wall clock says.
    const EXPIRY_2649_NS: u64 = 1_789_194_600_000_000_000;
    let wall = core_time::WallAnchor::new(core_time::now_ns(), EXPIRY_2649_NS - 30_000_000_000);
    let roll_status = std::sync::Arc::new(HlRollStatus::new());

    let mut driver = hwl::Driver::new(0x0B15u64, coins, u64::MAX / 4, u64::MAX / 4);
    driver.set_families(families, roll_status.clone(), wall);
    hwl::note_transport_ready(&mut driver, core_net::Status::Ready);

    let (mut hl_etx, _herx) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();
    let ring: std::sync::Arc<Ring<Tick, { hwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, _cons) = ring.split();
    let lane = core_types::EVENT_LANE_FUNDING
        | core_types::event_lane_bit(core_types::ChannelId::InstrumentRoll);

    // Handshake.
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut hl_etx,
        lane,
        &status,
        &mut core_types::NullCapture,
    )
    .unwrap();
    let mut scratch = [0u8; 16384];
    let _ = transport.drain_outgoing(&mut scratch);
    let key = core_net::sec_websocket_key_from_seed(0x0B15u64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut hl_etx,
        lane,
        &status,
        &mut core_types::NullCapture,
    )
    .unwrap();
    assert_eq!(driver.state(), hwl::State::Steady);
    let _ = transport.drain_outgoing(&mut scratch);

    // The two live created shapes, alternating: each one changes the
    // family's live instance, so each is a real roll.
    const CREATED_2649: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeCreated":{"outcome":2649,"name":"template:binaryPrice","description":"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630","sideSpecs":[{"name":"template:Yes"},{"name":"template:No"}],"quoteToken":"USDC","venue":"out","deployerFeeScale":"1.0"}}]}"##;
    const CREATED_2650: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeCreated":{"outcome":2650,"name":"template:binaryPrice","description":"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77201|time:20260912-0645","sideSpecs":[{"name":"template:Yes"},{"name":"template:No"}],"quoteToken":"USDC","venue":"out","deployerFeeScale":"1.0"}}]}"##;
    const ROLLS: usize = 64;

    fn push_text_frame(stream: &mut Vec<u8>, body: &[u8]) {
        stream.push(0x81);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            assert!(body.len() <= u16::MAX as usize);
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }
    let mut stream: Vec<u8> = Vec::with_capacity(128 * 1024);
    for i in 0..ROLLS {
        push_text_frame(
            &mut stream,
            if i % 2 == 0 {
                CREATED_2650
            } else {
                CREATED_2649
            },
        );
    }
    let injected = transport.inject_incoming(&stream);
    assert_eq!(injected, stream.len(), "transport must hold the script");

    // ---- measurement window ----
    let g = AllocGuard::new();
    let mut drives = 0u32;
    let mut out_scratch = [0u8; 16384];
    while transport.incoming_len() > 0 {
        hwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &mut hl_etx,
            lane,
            &status,
            &mut core_types::NullCapture,
        )
        .unwrap();
        // Keep the wire moving: the rolls' own frames leave through
        // here, and draining is stack-scratch only.
        let _ = transport.drain_outgoing(&mut out_scratch);
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
    // The non-fatal family ack pass runs inside the window too.
    hwl::roll_health(
        &mut driver,
        &status,
        &mut core_types::NullCapture,
        core_time::now_ns(),
    )
    .unwrap();

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        roll_status.rolls_total(),
        ROLLS as u64,
        "every scripted push must have rolled"
    );
    assert_eq!(roll_status.rolls_ignored_unmatched(), 0);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(allocs, 0, "hl family roll allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hl family roll bytes should be zero: saw {bytes}");
}

/// BIN15 O1: the LIVE `outcomeMetaUpdates` shapes through the
/// lifecycle parser, the zero-copy description accessor and the
/// description grammar — 10 000 iterations, zero allocations. The
/// grammar parser runs on the ingress thread the moment a roll lands,
/// so "boot-only module" is not a licence for it to allocate.
#[test]
fn hl_outcome_meta_parsers_are_zero_alloc() {
    let created: &[u8] = br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeCreated":{"outcome":2649,"name":"template:binaryPrice","description":"perp:BTC|priceDescription:BTC-USDC perp mark|seconds:60|threshold:77177|time:20260912-0630","sideSpecs":[{"name":"template:Yes"},{"name":"template:No"}],"quoteToken":"USDC","venue":"out","deployerFeeScale":"1.0"}}]}"##;
    let settled: &[u8] =
        br##"{"channel":"outcomeMetaUpdates","data":[{"outcomeSettled":2638}]}"##;
    let native: &[u8] =
        b"class:priceBinary|underlying:ETH|expiry:20260913-0600|targetPrice:2510.5|period:1d";

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        let mut c = ingress_hyperliquid::HlOutcomeMetaFrame::ZERO;
        assert!(ingress_hyperliquid::parse_outcome_meta(created, &mut c));
        acc = acc.wrapping_add(c.enc as i64);
        let mut s = ingress_hyperliquid::HlOutcomeMetaFrame::ZERO;
        assert!(ingress_hyperliquid::parse_outcome_meta(settled, &mut s));
        acc = acc.wrapping_add(s.enc as i64);
        let (id, desc) = ingress_hyperliquid::outcome_meta_description(created).unwrap();
        acc = acc.wrapping_add(id as i64 + desc.len() as i64);
        let mut spec = ingress_hyperliquid::discovery::parse_outcome_spec(id, desc);
        spec.set_venue(ingress_hyperliquid::outcome_meta_venue(created));
        acc = acc.wrapping_add(
            spec.strike_1e6 ^ spec.expiry_ns as i64 ^ spec.venue_bytes().len() as i64,
        );
        let n = ingress_hyperliquid::discovery::parse_outcome_spec(1, native);
        acc = acc.wrapping_add(n.strike_1e6 + n.period_s as i64);
        std::hint::black_box(ingress_hyperliquid::discovery::parse_hl_time_ns(
            b"20260912-0630",
        ));
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hl outcome parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hl outcome parser bytes should be zero: saw {bytes}");
}

/// Drive the Hyperliquid ingress run-loop through 1 000+
/// pre-injected steady-state frames (all 9 subscriptionResponse
/// acks — verification + staleness arming happen **inside** the
/// window via `session_health` — then cycles of bbo / l2Book /
/// trades across a perp and a HIP-4 `#<enc>` coin, plus WS protocol
/// Pings whose pong replies render inside the window) via a
/// `TestTransport`. Steady state is reached over the real handshake
/// and per-sub subscribe path; every `drive_one` call must allocate
/// zero bytes.
#[test]
fn hl_run_loop_steady_state_is_zero_alloc() {
    use ingress_hyperliquid::run_loop as hwl;

    // ---- boot (NOT measured) ----
    // Capacity must hold the whole scripted stream at once (~170 KiB
    // for 1 000+ frames): the transport's buffers are fixed-size and
    // nothing drains them until the measurement loop runs.
    let mut transport = TestTransport::with_capacity(512 * 1024);

    // Venue-namespaced symbols (venue byte 4 = Hyperliquid).
    let sym_btc: SymbolId = (4 << 24) | 1;
    let sym_hip4: SymbolId = (4 << 24) | 2;
    let mut coins = ingress_hyperliquid::HlCoinTable::new();
    coins.insert(b"BTC", sym_btc).unwrap();
    coins.insert(b"#330", sym_hip4).unwrap();
    // Generous budgets: neither the ack deadline nor staleness may
    // trip inside the measurement window.
    let mut driver = hwl::Driver::new(0x0D0Du64, coins, u64::MAX / 4, u64::MAX / 4);
    hwl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // XMM XH1: the trade lane, attached — every `trades` row in the
    // measured window is captured AND pushed as a print.
    let (trade_tx, mut trade_rx) =
        Ring::<core_types::TradePrint, { core_types::TRADE_RING_SIZE }>::new().split();
    driver.set_trade_lane(trade_tx);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    // VM2 V2: hoisted throwaway HL event lane (same rationale).
    let (mut hl_etx, _herx) =
        Ring::<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { hwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // §6.5 capture: REAL PmlrCapture with the raw tap in `All` mode —
    // the measured window below proves the entire capture path (tick +
    // event appends, tap records, staging flushes) is 0 B/op. Files go
    // to a temp dir created here (boot side, outside the guard).
    let cap_dir = std::env::temp_dir().join(format!("hl_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "hl",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    // Send the client GET handshake.
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut hl_etx,
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut scratch = [0u8; 16384];
    let _ = transport.drain_outgoing(&mut scratch);

    // Inject the 101 reply matching the seed.
    let key = core_net::sec_websocket_key_from_seed(0x0D0Du64);
    let accept = core_net::expected_accept(&key);
    let mut resp = [0u8; 256];
    let mut n = 0;
    for src in [
        &b"HTTP/1.1 101 Switching Protocols\r\n"[..],
        &b"Upgrade: websocket\r\n"[..],
        &b"Connection: Upgrade\r\n"[..],
        &b"Sec-WebSocket-Accept: "[..],
        &accept[..],
        &b"\r\n\r\n"[..],
    ] {
        resp[n..n + src.len()].copy_from_slice(src);
        n += src.len();
    }
    transport.inject_incoming(&resp[..n]);
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &mut hl_etx,
        core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
        &status,
        &mut capture,
    )
    .unwrap();
    assert_eq!(driver.state(), hwl::State::Steady);
    // Drain the 9 per-subscription subscribe frames so the tx buffer
    // stays empty.
    let _ = transport.drain_outgoing(&mut scratch);

    // Pre-build + pre-inject the full scripted stream: all 9 acks
    // (BTC: bbo/l2Book/trades/activeAssetCtx; #330: bbo/l2Book/
    // trades; global: allMids/outcomeMetaUpdates), then cycles of
    // [bbo BTC, l2Book BTC, trades BTC ×2 rows, bbo #330,
    // l2Book #330], then a few WS protocol Pings. Stateless venue —
    // no chains to maintain; staleness budgets are generous (boot).
    // `inject_incoming` may compact/copy; all of it happens here,
    // before the guard.
    const CYCLES: usize = 199; // 9 acks + 3 unsubs + 5 × 199 = 1 007 frames
    const BBO_BTC: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    // XMM XH1: two DEEP perp books (7 levels a side, past `DEPTH_K`),
    // alternated so every cycle's perp `l2Book` is a CHANGED top-K: the
    // perp depth path — walk, skip past K, change gate, capture — runs
    // its commit branch inside the measured window, every cycle.
    const L2_BTC_A: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"64437.0","sz":"1.0","n":1},{"px":"64436.0","sz":"2.0","n":2},{"px":"64435.0","sz":"3.0","n":3},{"px":"64434.0","sz":"4.0","n":4},{"px":"64433.0","sz":"5.0","n":5},{"px":"64432.0","sz":"6.0","n":6},{"px":"64431.0","sz":"7.0","n":7}],[{"px":"64438.0","sz":"1.5","n":1},{"px":"64439.0","sz":"2.5","n":2},{"px":"64440.0","sz":"3.5","n":3},{"px":"64441.0","sz":"4.5","n":4},{"px":"64442.0","sz":"5.5","n":5},{"px":"64443.0","sz":"6.5","n":6},{"px":"64444.0","sz":"7.5","n":7}]]}}"#;
    const L2_BTC_B: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000001,"levels":[[{"px":"64437.0","sz":"1.5","n":1},{"px":"64436.0","sz":"2.5","n":2},{"px":"64435.0","sz":"3.5","n":3},{"px":"64434.0","sz":"4.5","n":4},{"px":"64433.0","sz":"5.5","n":5},{"px":"64432.0","sz":"6.5","n":6},{"px":"64431.0","sz":"7.5","n":7}],[{"px":"64438.0","sz":"1.5","n":1},{"px":"64439.0","sz":"2.5","n":2},{"px":"64440.0","sz":"3.5","n":3},{"px":"64441.0","sz":"4.5","n":4},{"px":"64442.0","sz":"5.5","n":5},{"px":"64443.0","sz":"6.5","n":6},{"px":"64444.0","sz":"7.5","n":7}]]}}"#;
    const TRADES_BTC: &[u8] = br#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"1.0","sz":"1.0","hash":"0x1","time":1000,"tid":1},{"coin":"BTC","side":"A","px":"1.1","sz":"2.0","hash":"0x2","time":1001,"tid":2}]}"#;
    const BBO_HIP4: &[u8] = br##"{"channel":"bbo","data":{"coin":"#330","time":1723600000001,"bbo":[{"px":"0.4","sz":"100.0","n":1},{"px":"0.6","sz":"50.0","n":1}]}}"##;
    const L2_HIP4: &[u8] = br##"{"channel":"l2Book","data":{"coin":"#330","time":1723600000002,"levels":[[{"px":"0.4","sz":"100.0","n":1}],[{"px":"0.6","sz":"50.0","n":1}]]}}"##;
    const ACKS: [&[u8]; 9] = [
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"BTC"}}}"#,
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}}"#,
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}}"#,
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"activeAssetCtx","coin":"BTC"}}}"#,
        br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"#330"}}}"##,
        br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"#330"}}}"##,
        br##"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"#330"}}}"##,
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"allMids"}}}"#,
        br#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"outcomeMetaUpdates"}}}"#,
    ];

    /// Unmasked (server→client) WS text frame appended to `stream`.
    fn push_text_frame(stream: &mut Vec<u8>, body: &[u8]) {
        stream.push(0x81);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            assert!(body.len() <= u16::MAX as usize);
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }

    // HK1: a roll's unsubscribe echoes — answers, counted as messages
    // (`HlRollStatus::unsub_acks`), never as parse errors — ride the
    // measured window too.
    const UNSUBS: [&[u8]; 3] = [
        br##"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"bbo","coin":"#330"}}}"##,
        br##"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"l2Book","coin":"#330"}}}"##,
        br##"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"trades","coin":"#330"}}}"##,
    ];

    // HK37: a fresh leg's empty `trades` snapshot — venue data,
    // counted as a message (`HlRollStatus::trades_empty`), never as a
    // parse error — rides the measured window too.
    const TRADES_EMPTY: &[u8] = br#"{"channel":"trades","data":[]}"#;

    let mut stream: Vec<u8> = Vec::with_capacity(256 * 1024);
    for ack in ACKS {
        push_text_frame(&mut stream, ack);
    }
    for unsub in UNSUBS {
        push_text_frame(&mut stream, unsub);
    }
    push_text_frame(&mut stream, TRADES_EMPTY);
    for c in 0..CYCLES {
        push_text_frame(&mut stream, BBO_BTC);
        push_text_frame(&mut stream, if c % 2 == 0 { L2_BTC_A } else { L2_BTC_B });
        push_text_frame(&mut stream, TRADES_BTC);
        push_text_frame(&mut stream, BBO_HIP4);
        push_text_frame(&mut stream, L2_HIP4);
    }
    // Three server-side WS protocol Pings — the pong replies render
    // into the tx buffer inside the measurement window.
    const N_PINGS: usize = 3;
    for _ in 0..N_PINGS {
        stream.extend_from_slice(&[0x89, 0x02, b'h', b'l']);
    }
    let injected = transport.inject_incoming(&stream);
    assert_eq!(
        injected,
        stream.len(),
        "transport capacity must hold the full scripted stream"
    );

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut drives = 0u32;
    while transport.incoming_len() > 0 {
        hwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &mut hl_etx,
            core_types::EVENT_LANE_FUNDING | core_types::EVENT_LANE_ASSET_CTX,
            &status,
            &mut capture,
        )
        .unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
    // Flush-path inside the window too: staged capture bytes hit disk
    // via plain write_all (no alloc).
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    // Ack verification + staleness arming — the run()-loop health
    // check — happens inside the window too.
    assert_eq!(
        ingress_hyperliquid::run_loop::session_health(&mut driver, &status, core_time::now_ns()),
        None
    );
    // Drain our pong replies out of the transport (stack scratch).
    let mut out_scratch = [0u8; 4096];
    let _ = transport.drain_outgoing(&mut out_scratch);
    // Drain the ticks — THREE per cycle. try_pop_ref is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    //
    // BIN15 O8: two bbo frames plus the HIP-4 coin's `l2Book`, which
    // now carries that leg's touch because the venue publishes its
    // `bbo` one-sided. The BTC books still yield no tick — a perp's
    // touch comes from bbo alone, so no perp number moved.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(t.bid_px.raw());
        popped += 1;
    }
    // XMM XH1: the prints, TWO per cycle (the two BTC `trades` rows).
    let mut prints: usize = 0;
    while let Some(p) = trade_rx.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(p.qty_1e6);
        prints += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // Steady state consumed the whole script: one tick per bbo frame
    // (both coins — HIP-4 flows the same path) plus the HIP-4
    // `l2Book` touch (BIN15 O8), every ack verified, every frame
    // counted, no losses, no staleness trips.
    assert_eq!(popped, 3 * CYCLES);
    assert_eq!(prints, 2 * CYCLES);
    assert_eq!(status.trade_ring_drops_total(), 0);
    assert!(driver.is_verified());
    // 9 acks + 3 unsubscribe echoes (HK1) + 1 empty trades snapshot
    // (HK37) + per cycle: bbo(1) + l2Book(1) + trades rows(2) + bbo(1)
    // + l2Book(1) = 6. WS Pings are activity, not messages.
    assert_eq!(
        status.msgs_total(),
        (9 + UNSUBS.len() + 1 + 6 * CYCLES) as u64
    );
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.gaps_total(), 0);
    assert_eq!(status.resubscribes_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(
        allocs, 0,
        "hl run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl run-loop bytes should be zero: saw {bytes}");
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.tap_dropped(), 0);
    assert!(capture.ticks_written() > 0);
    // XMM XH1: one depth row per cycle for the alternating BTC books,
    // plus the HIP-4 leg's first (and only changed) snapshot.
    assert_eq!(capture.depths_written(), CYCLES as u64 + 1);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// 8f item 5 (design §11 alloc gate): the full AI-ingress frame path —
/// pack (client side of the loopback), then accept → HMAC verify →
/// shape check → seq policy → ts rewrite → capture → try_push_ref — must
/// allocate ZERO bytes per frame after boot. 10 000 frames; consumer
/// pops in lockstep so the ring never saturates (`ring_drops` stays 0
/// and the push path is exercised end-to-end).
#[test]
fn ai_ingress_admit_frame_is_zero_alloc() {
    use core_types::{AiCmd, AiCmdKind, AI_SIDE_NONE, STRATEGY_SLOT_NONE, SYMBOL_ID_NONE};

    // Boot (allocation allowed): ring, capture sink, status slot.
    let ring: std::sync::Arc<Ring<AiCmd, { core_types::AI_RING_SIZE }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    let cap_dir =
        std::env::temp_dir().join(format!("stage2_alloc_ai_ingress_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = AiCmdCapture::open(&cap_dir, 1).unwrap();
    let status = AiIngressStatus::new();
    let mut seq = SeqPolicy::new();
    let key = [0x77u8; 32];
    let mut frame = [0u8; ingress_ai::FRAME_LEN];
    let mut seam_hits = 0u64;
    let mut seam = |_c: &AiCmd| seam_hits += 1;

    const CYCLES: u32 = 10_000;
    let mut acc: u64 = 0;

    let g = AllocGuard::new();
    let mut i = 1u32;
    while i <= CYCLES {
        let cmd = AiCmd::new(
            u64::from(i), // worker ts — rewritten on accept
            i,
            SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        );
        pack_frame(&key, &cmd, &mut frame);
        let v = admit_frame(
            &frame,
            &key,
            &mut seq,
            &mut prod,
            &mut capture,
            &status,
            &mut seam,
            u64::from(i) + 1_000_000,
        );
        assert!(matches!(v, FrameVerdict::Accepted));
        let popped = *cons.try_pop_ref().unwrap();
        acc = acc.wrapping_add(popped.ts_ns);
        i += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(status.cmds(), u64::from(CYCLES));
    assert_eq!(status.hmac_fail(), 0);
    assert_eq!(status.protocol_err(), 0);
    assert_eq!(status.malformed(), 0);
    assert_eq!(status.seq_gap(), 0);
    assert_eq!(status.seq_regress(), 0);
    assert_eq!(status.ring_drops(), 0);
    assert_eq!(seam_hits, 0);
    assert_eq!(capture.records(), u64::from(CYCLES));
    assert!(!capture.is_disabled());
    assert_eq!(
        allocs, 0,
        "ai ingress frame path allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "ai ingress frame path bytes should be zero: saw {bytes}"
    );
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// Phase 8f item 7: StrategySet fan-out steady state — mask-gated
/// member dispatch (ticks through the slot-0 member — the hyparb stub
/// since HYPARB H0, latency-arb before it — AI heartbeat fan-out, an
/// Enable/Disable round trip of slot 0) must allocate
/// nothing after boot. 8g item 6: the vm member joins the set it
/// measures — a committed one-row table fires + re-arms on every PM
/// tick through the set's fan-out, and a per-cycle `RulesetCommit`
/// with nothing staged exercises the commit-dropped path (same gate,
/// baseline stays 36).
#[test]
fn strategy_set_fanout_is_zero_alloc() {
    use core_types::{
        fnv1a_64, AiCmd, AiCmdKind, Order, RuleRow, RuleTableV2, AI_SIDE_NONE, STRATEGY_SLOT_NONE,
        STRATEGY_SLOT_VM, SYMBOL_ID_NONE,
    };
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_set::{StrategySet, BIT_HYPARB, BIT_VM, SLOT_HYPARB};

    struct CountCtx {
        submitted: u64,
        now: u64,
    }
    impl Ctx for CountCtx {
        fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    // Boot (allocation allowed): slot 0 (hyparb — configured since H4
    // validates in `on_start`: one observe-only pool, one perp coin)
    // rides the fan-out, and a one-row vm table is committed on the
    // (PM=11, BN=22) pair. Clock is production-like (G3 lesson: fresh
    // cooldown stamps arm only once `now ≥ horizon_ns`).
    let mut set = StrategySet::new(BIT_HYPARB | BIT_VM);
    {
        let mut hp = strategy_hyparb::HyparbParams::EMPTY;
        hp.coins[0] = strategy_hyparb::CoinParams {
            perp_sym: core_types::make_symbol_id(VenueId::Hyperliquid, 5),
            spot_sym: SYMBOL_ID_NONE,
            lot_1e6: 10_000,
            min_notional_usd_1e6: 10_000_000,
        };
        hp.n_coins = 1;
        hp.pools[0] = strategy_hyparb::PoolParams {
            sym: core_types::make_symbol_id(VenueId::HyperEvm, 1),
            coin0: 0,
            coin1: strategy_hyparb::COIN_USD,
            trade: false,
            max_notional_usd_1e6: 1_000_000,
        };
        hp.n_pools = 1;
        hp.lag_ns = 1;
        hp.basis_window_ns = 1;
        hp.max_order_usd_1e6 = 1;
        hp.cap_day_usd_1e6 = 1;
        hp.inventory_cap_usd_1e6 = 1;
        set.hyparb_mut()
            .configure(hp, core_time::WallAnchor::new(0, 0))
            .expect("gate hyparb params");
    }
    let mut ctx = CountCtx {
        submitted: 0,
        now: 100_000_000_000_000_000,
    };
    set.on_start(&mut ctx).unwrap();

    let vm_hash: [u8; 16] = [0xAB; 16];
    let mut table = Box::new(RuleTableV2::EMPTY);
    table.rows[0] = core_types::RuleRowV2::from_v1(&RuleRow::new(
        11,
        22,
        20,
        0,
        0,
        1_000_000,
        fnv1a_64(b"g4-gate"),
        RuleRow::TRIGGER_CROSS_DEVIATION,
        RuleRow::SIDE_BOTH,
        0,
    ));
    table.len = 1;
    table.epoch = 1;
    table.hash128 = vm_hash;
    set.vm_mut().receive_table_v2(&table);
    let commit = {
        let px = i64::from_le_bytes(vm_hash[..8].try_into().expect("8 bytes"));
        let qty = i64::from_le_bytes(vm_hash[8..].try_into().expect("8 bytes"));
        AiCmd::new(
            1,
            4,
            SYMBOL_ID_NONE,
            px,
            qty,
            0,
            AiCmdKind::RulesetCommit,
            VenueId::Ai,
            STRATEGY_SLOT_VM,
            AI_SIDE_NONE,
            0,
            0,
        )
    };
    set.on_ai(&commit, &mut ctx);
    assert_eq!(set.vm().commits_applied, 1, "boot flip applied");

    let bn = Tick::new(
        0,
        VenueId::Binance,
        22,
        1,
        Price::from_raw(490_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(510_000),
        Qty::from_raw(1_000_000),
    );
    let pm = Tick::new(
        0,
        VenueId::Polymarket,
        11,
        1,
        Price::from_raw(390_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(410_000),
        Qty::from_raw(1_000_000),
    );
    let hb = AiCmd::new(
        1,
        1,
        SYMBOL_ID_NONE,
        0,
        0,
        0,
        AiCmdKind::Heartbeat,
        VenueId::Ai,
        STRATEGY_SLOT_NONE,
        AI_SIDE_NONE,
        0,
        0,
    );
    let disable = AiCmd::new(
        1,
        2,
        SYMBOL_ID_NONE,
        0,
        0,
        0,
        AiCmdKind::DisableStrategy,
        VenueId::Ai,
        SLOT_HYPARB,
        AI_SIDE_NONE,
        0,
        0,
    );
    let enable = AiCmd::new(
        1,
        3,
        SYMBOL_ID_NONE,
        0,
        0,
        0,
        AiCmdKind::EnableStrategy,
        VenueId::Ai,
        SLOT_HYPARB,
        AI_SIDE_NONE,
        0,
        0,
    );

    const CYCLES: u32 = 10_000;
    let g = AllocGuard::new();
    let mut i = 0u32;
    while i < CYCLES {
        set.on_tick(&bn, &mut ctx);
        set.on_tick(&pm, &mut ctx);
        set.on_ai(&hb, &mut ctx);
        set.on_ai(&disable, &mut ctx);
        set.on_ai(&enable, &mut ctx);
        // Nothing staged after the boot flip: every in-loop Commit
        // exercises the vm commit-dropped path through the fan-out.
        set.on_ai(&commit, &mut ctx);
        i += 1;
    }
    std::hint::black_box(ctx.submitted);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        ctx.submitted >= u64::from(CYCLES),
        "the vm row must fire every cycle"
    );
    assert_eq!(set.enabled_mask(), BIT_HYPARB | BIT_VM);
    assert_eq!(set.enable_refused_total(), 0);
    assert_eq!(set.vm().commits_applied, 1, "no further flip in-loop");
    assert_eq!(set.vm().commits_dropped, u64::from(CYCLES));
    assert!(set.vm().orders_emitted >= u64::from(CYCLES));
    assert_eq!(
        allocs, 0,
        "strategy-set fan-out allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "strategy-set fan-out bytes should be zero: saw {bytes}"
    );
}

/// **XMM gate 73 (XH1)** — the set's new paths after boot: the trade
/// fan-out, the order-event router (a slot-6 event routed, a slot-7 one
/// counted unrouted) and the per-slot timer gate (hyparb's 1 s period
/// firing on every other call; xmm's 100 ms safety timer, XH2, on every
/// call with no feed to judge) — allocate nothing.
#[test]
fn xmm_slot_trade_order_event_and_slot_timers_are_zero_alloc() {
    use core_types::{Order, OrderEvent, TradePrint, SYMBOL_ID_NONE};
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_set::{StrategySet, BIT_HYPARB, BIT_XMM, SLOT_XMM};

    struct CountCtx {
        submitted: u64,
        now: u64,
    }
    impl Ctx for CountCtx {
        fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    // Boot (allocation allowed): hyparb as in the fan-out gate, so a
    // member with a real (1 s) timer rides the set; xmm on one perp.
    let mut set = StrategySet::new(BIT_HYPARB | BIT_XMM);
    {
        let mut hp = strategy_hyparb::HyparbParams::EMPTY;
        hp.coins[0] = strategy_hyparb::CoinParams {
            perp_sym: core_types::make_symbol_id(VenueId::Hyperliquid, 5),
            spot_sym: SYMBOL_ID_NONE,
            lot_1e6: 10_000,
            min_notional_usd_1e6: 10_000_000,
        };
        hp.n_coins = 1;
        hp.pools[0] = strategy_hyparb::PoolParams {
            sym: core_types::make_symbol_id(VenueId::HyperEvm, 1),
            coin0: 0,
            coin1: strategy_hyparb::COIN_USD,
            trade: false,
            max_notional_usd_1e6: 1_000_000,
        };
        hp.n_pools = 1;
        hp.lag_ns = 1;
        hp.basis_window_ns = 1;
        hp.max_order_usd_1e6 = 1;
        hp.cap_day_usd_1e6 = 1;
        hp.inventory_cap_usd_1e6 = 1;
        set.hyparb_mut()
            .configure(hp, core_time::WallAnchor::new(0, 0))
            .expect("gate hyparb params");
        let mut xp = strategy_xmm::XmmParams::EMPTY;
        xp.perps[0] = strategy_xmm::XmmPerp {
            hl_sym: core_types::make_symbol_id(VenueId::Hyperliquid, 5),
            lead_sym: core_types::make_symbol_id(VenueId::Binance, 9),
            lot_1e6: 10_000,
        };
        xp.n_perps = 1;
        xp.maker_enabled = 1;
        xp.theta_bps_1e6 = 500_000;
        xp.gate_window_ms = 500;
        xp.lifetime_ms = 30_000;
        xp.lead_stale_ms = 300;
        xp.follower_stale_ms = 2_000;
        xp.rtt_pull_ms = 1_500;
        xp.requote_min_ms = 250;
        xp.clip_usd_1e6 = 15_000_000;
        xp.inv_cap_usd_1e6 = 150_000_000;
        xp.gross_inv_cap_usd_1e6 = 400_000_000;
        xp.resting_cap_usd_1e6 = 300_000_000;
        set.xmm_mut().configure(&xp).expect("gate xmm params");
    }
    let t0: u64 = 100_000_000_000_000_000;
    let mut ctx = CountCtx {
        submitted: 0,
        now: t0,
    };
    set.on_start(&mut ctx).unwrap();

    let print = TradePrint::new(
        t0,
        VenueId::Hyperliquid,
        core_types::make_symbol_id(VenueId::Hyperliquid, 5),
        1,
        0,
        100_000_000_000,
        1_000_000,
        core_types::TRADE_AGGRESSOR_BUY,
    );
    let routed = OrderEvent::new(
        t0,
        VenueId::Hyperliquid,
        core_types::make_symbol_id(VenueId::Hyperliquid, 5),
        7,
        SLOT_XMM,
        core_types::ORDER_EVENT_CANCELED,
        core_types::ORDER_EVENT_REASON_EXPIRED,
        0,
    );
    let mut unrouted = routed;
    unrouted.strategy_id = 7;

    const CYCLES: u64 = 10_000;
    // Half a second a cycle: hyparb's 1 s period is due every other call.
    const STEP_NS: u64 = 500_000_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        ctx.now = t0 + (i + 1) * STEP_NS;
        set.on_trade(&print, &mut ctx);
        set.on_order_event(&routed, &mut ctx);
        set.on_order_event(&unrouted, &mut ctx);
        set.on_timer(ctx.now, &mut ctx);
        i += 1;
    }
    std::hint::black_box(ctx.submitted);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(set.enabled_mask(), BIT_HYPARB | BIT_XMM);
    assert_eq!(set.order_events_unrouted(), CYCLES, "slot 7 is nobody's");
    assert_eq!(ctx.submitted, 0, "no feed reached xmm; hyparb observes only");
    assert_eq!(
        allocs, 0,
        "xmm slot paths allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "xmm slot paths bytes should be zero: saw {bytes}");
}

/// **XMM gate 74 (XH2)** — the queue law after boot: placements landing
/// against the book, prints consuming the queue ahead and then filling,
/// a modify, a requested cancel and the event drain — allocate nothing.
#[test]
fn xmm_queue_law_place_land_fill_modify_cancel_are_zero_alloc() {
    use core_fill::{QueueBook, QueuePlace, Touch, QUEUE_NEVER};
    use core_types::Side;

    const U: i64 = 1_000_000;
    const CYCLES: u64 = 10_000;
    let sym = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let book = Touch {
        bid_1e6: 100 * U,
        ask_1e6: 101 * U,
        bid_qty_1e6: 2 * U,
        ask_qty_1e6: 2 * U,
    };
    // Boot (allocation allowed).
    let mut q = QueueBook::new();
    q.track(sym).expect("gate 74 track");
    q.on_book(sym, book, 0);

    let mut kinds = 0u64;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        let t = (i + 1) * 1_000;
        let oid = i * 4;
        let bid = QueuePlace {
            client_oid: oid + 1,
            sym,
            side: Side::Bid,
            slot: 6,
            px_1e6: 100 * U,
            qty_1e6: U,
            ready_ns: t,
            expiry_ns: QUEUE_NEVER,
        };
        let ask = QueuePlace {
            client_oid: oid + 2,
            side: Side::Ask,
            px_1e6: 101 * U,
            expiry_ns: t + 500,
            ..bid
        };
        q.place(&bid).expect("gate 74 bid");
        q.place(&ask).expect("gate 74 ask");
        q.on_book(sym, book, t); // both land, 2 shown ahead of each
        q.on_print(sym, 100 * U, 3 * U, true, t + 100); // 2 ahead, then our 1: FILLED
        let requote = QueuePlace {
            client_oid: oid + 3,
            px_1e6: 102 * U,
            ready_ns: t + 200,
            expiry_ns: QUEUE_NEVER,
            ..ask
        };
        q.modify(oid + 2, &requote).expect("gate 74 modify");
        q.on_book(sym, book, t + 200); // the new ask lands, the old one is replaced
        q.cancel(oid + 3, 6, t + 300).expect("gate 74 cancel");
        q.on_print(sym, 101 * U, U, false, t + 300); // the cancel lands ahead of the print
        while let Some(e) = q.try_next_event() {
            kinds += u64::from(e.kind);
        }
        i += 1;
    }
    std::hint::black_box(kinds);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(q.is_empty(), "every cycle ends flat");
    assert_eq!(q.counters.filled, CYCLES);
    assert_eq!(q.counters.canceled, 2 * CYCLES);
    assert_eq!(q.counters.out_overflow, 0);
    assert_eq!(allocs, 0, "queue law allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "queue law bytes should be zero: saw {bytes}");
}

/// **XMM gate 75 (XH2)** — the member's hot paths after `on_start`:
/// leader and follower updates (placements at the touch, the gate's ring
/// lookups), the LEAD cancel, order events, fills and the safety timer
/// — allocate nothing. (The gate's history rings are allocated once, in
/// `configure`.)
#[test]
fn xmm_member_place_lead_cancel_events_fills_are_zero_alloc() {
    use core_types::{CancelReq, Fill, Order, OrderEvent, Side, ORDER_EVENT_CANCELED,
        ORDER_EVENT_FILLED, ORDER_EVENT_RESTING};
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// Remembers the last two client ids it was handed, in fixed slots.
    struct OidCtx {
        now: u64,
        oids: [u64; 2],
        n: usize,
        cancels: u64,
    }
    impl Ctx for OidCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            self.oids[self.n & 1] = o.client_oid;
            self.n += 1;
            Ok(())
        }
        fn cancel(&mut self, _r: CancelReq) -> Result<(), SubmitErr> {
            self.cancels += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let bn = core_types::make_symbol_id(VenueId::Binance, 9);
    // Boot (allocation allowed).
    let mut xp = strategy_xmm::XmmParams::EMPTY;
    xp.perps[0] = strategy_xmm::XmmPerp {
        hl_sym: hl,
        lead_sym: bn,
        lot_1e6: 10_000,
    };
    xp.n_perps = 1;
    xp.maker_enabled = 1;
    xp.theta_bps_1e6 = 500_000;
    // A 1 ms window: the gate's ring lookups run every placement, and
    // the flat leader a step earlier leaves both sides open.
    xp.gate_window_ms = 1;
    xp.lifetime_ms = 30_000;
    xp.lead_stale_ms = 300;
    xp.follower_stale_ms = 2_000;
    xp.rtt_pull_ms = 1_500;
    xp.requote_min_ms = 250;
    xp.clip_usd_1e6 = 15_000_000;
    xp.inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.gross_inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.resting_cap_usd_1e6 = 1_000_000_000_000;
    let mut m = strategy_xmm::XmmStrategy::new();
    m.configure(&xp).expect("gate 75 params");
    let t0: u64 = 1_000_000_000_000;
    let mut ctx = OidCtx {
        now: t0,
        oids: [0; 2],
        n: 0,
        cancels: 0,
    };
    m.on_start(&mut ctx).expect("gate 75 start");
    let book = |sym: u32, bid: i64, ask: i64| {
        Tick::new(
            0,
            if sym == hl { VenueId::Hyperliquid } else { VenueId::Binance },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    };
    let lead_flat = book(bn, 99_990_000, 100_010_000);
    let lead_down = book(bn, 99_980_000, 100_000_000);
    let fol = book(hl, 99_990_000, 100_010_000);
    let ev = |oid: u64, kind: u8| OrderEvent::new(0, VenueId::Hyperliquid, hl, oid, 6, kind, 0, 0);

    const CYCLES: u64 = 10_000;
    const STEP: u64 = 10_000_000;
    let mut seen = strategy_core::XmmCounters::default();
    let mut rows = [strategy_core::XmmPerpView::default(); strategy_xmm::XMM_MAX_PERPS];
    let mut viewed = 0u64;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        ctx.now = t0 + i * 8 * STEP;
        m.on_tick(&lead_flat, &mut ctx);
        ctx.now += STEP;
        m.on_tick(&fol, &mut ctx); // both sides placed at the touch
        let (bid, ask) = (ctx.oids[0], ctx.oids[1]);
        m.on_order_event(&ev(bid, ORDER_EVENT_RESTING), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_RESTING), &mut ctx);
        ctx.now += STEP;
        m.on_tick(&lead_down, &mut ctx); // −1 bp: the LEAD rule pulls the bid
        let f = Fill::new(ctx.now, hl, Side::Ask, Price::from_raw(100_010_000), Qty::from_raw(140_000), ask);
        m.on_fill(&f, &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_FILLED), &mut ctx);
        m.on_order_event(&ev(bid, ORDER_EVENT_CANCELED), &mut ctx);
        m.on_timer(ctx.now, &mut ctx);
        // XMM XH3: what the 1 s `/state` publish and the 5 s metrics
        // mirror read from the member.
        strategy_core::StrategyCounters::xmm_counters(&m, &mut seen);
        viewed = viewed.wrapping_add(u64::from(strategy_core::StrategyCounters::xmm_perps_view(
            &m, &mut rows,
        )));
        i += 1;
    }
    std::hint::black_box(ctx.cancels);
    std::hint::black_box((seen, rows, viewed));

    let (allocs, bytes, _deallocs) = g.delta();
    let c = m.counters();
    assert_eq!(c.placed, 2 * CYCLES, "both sides every cycle");
    assert_eq!(c.lead_cancels, CYCLES, "the bid pulled every cycle");
    assert_eq!((c.filled, c.canceled, c.stuck), (CYCLES, CYCLES, 0));
    assert_eq!((seen, viewed), (c, CYCLES), "the accessors read the member");
    assert_eq!(allocs, 0, "xmm member allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "xmm member bytes should be zero: saw {bytes}");
}

/// **XMM gate 75b (XH2)** — the member's other hot branches: a requote
/// by MODIFY (the touch moved past `requote_min`), the replaced order's
/// end, a stale-flagged leader tick that pulls both sides, and the
/// safety timer's pass — allocate nothing.
#[test]
fn xmm_member_requote_replaced_and_stale_pull_are_zero_alloc() {
    use core_types::{CancelReq, Order, OrderEvent, ORDER_EVENT_CANCELED, ORDER_EVENT_RESTING};
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// Remembers the last ids it was handed in fixed slots: placements
    /// by side, the latest modify's new id.
    struct ModCtx {
        now: u64,
        placed: [u64; 2],
        n: usize,
        modified: u64,
        cancels: u64,
    }
    impl Ctx for ModCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            self.placed[self.n & 1] = o.client_oid;
            self.n += 1;
            Ok(())
        }
        fn cancel(&mut self, _r: CancelReq) -> Result<(), SubmitErr> {
            self.cancels += 1;
            Ok(())
        }
        fn modify(&mut self, _prev: u64, o: Order) -> Result<(), SubmitErr> {
            self.modified = o.client_oid;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let bn = core_types::make_symbol_id(VenueId::Binance, 9);
    let mut xp = strategy_xmm::XmmParams::EMPTY;
    xp.perps[0] = strategy_xmm::XmmPerp {
        hl_sym: hl,
        lead_sym: bn,
        lot_1e6: 10_000,
    };
    xp.n_perps = 1;
    xp.maker_enabled = 1;
    xp.theta_bps_1e6 = 500_000;
    xp.gate_window_ms = 1;
    xp.lifetime_ms = 30_000;
    xp.lead_stale_ms = 300;
    xp.follower_stale_ms = 2_000;
    xp.rtt_pull_ms = 1_500;
    xp.requote_min_ms = 250;
    xp.clip_usd_1e6 = 15_000_000;
    xp.inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.gross_inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.resting_cap_usd_1e6 = 1_000_000_000_000;
    let mut m = strategy_xmm::XmmStrategy::new();
    m.configure(&xp).expect("gate 75b params");
    let t0: u64 = 1_000_000_000_000;
    let mut ctx = ModCtx {
        now: t0,
        placed: [0; 2],
        n: 0,
        modified: 0,
        cancels: 0,
    };
    m.on_start(&mut ctx).expect("gate 75b start");
    let book = |sym: u32, bid: i64, ask: i64, stale: bool| {
        let mut t = Tick::new(
            0,
            if sym == hl { VenueId::Hyperliquid } else { VenueId::Binance },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= core_types::TICK_FLAG_STALE;
        }
        t
    };
    let lead = book(bn, 99_990_000, 100_010_000, false);
    let lead_stale = book(bn, 99_990_000, 100_010_000, true);
    let fol = book(hl, 99_990_000, 100_010_000, false);
    let fol_up = book(hl, 99_995_000, 100_010_000, false);
    let ev = |oid: u64, kind: u8, reason: u8| {
        OrderEvent::new(0, VenueId::Hyperliquid, hl, oid, 6, kind, reason, 0)
    };

    const CYCLES: u64 = 10_000;
    const MS: u64 = 1_000_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        ctx.now = t0 + i * 1_000 * MS;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol, &mut ctx); // both sides placed
        let (bid, ask) = (ctx.placed[0], ctx.placed[1]);
        m.on_order_event(&ev(bid, ORDER_EVENT_RESTING, 0), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_RESTING, 0), &mut ctx);
        ctx.now += 300 * MS;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol_up, &mut ctx); // the bid touch rose: MODIFY
        let new = ctx.modified;
        m.on_order_event(&ev(bid, ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_REPLACED), &mut ctx);
        m.on_order_event(&ev(new, ORDER_EVENT_RESTING, 0), &mut ctx);
        ctx.now += 10 * MS;
        m.on_tick(&lead_stale, &mut ctx); // a stale leader pulls both
        m.on_timer(ctx.now, &mut ctx);
        m.on_order_event(&ev(new, ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED), &mut ctx);
        i += 1;
    }
    std::hint::black_box(ctx.cancels);

    let (allocs, bytes, _deallocs) = g.delta();
    let c = m.counters();
    assert_eq!(c.modifies, CYCLES, "one requote by modify per cycle");
    assert_eq!(c.pull_cancels, 2 * CYCLES, "the stale leader pulled both sides");
    assert_eq!((c.stuck, c.unmatched), (0, 0));
    assert_eq!(allocs, 0, "xmm member branches allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "xmm member branches bytes should be zero: saw {bytes}");
}

/// **XMM gate 75e (XH5d-2)** — the A/B arms: the block's clock (the
/// follower's venue time) alternating A and B every cycle; under `ab_mode`
/// 1 a bid decided in arm B pulled on half θ (a −0.3 bps leader move that
/// arm A's bid rides out); under `ab_mode` 3 both quotes decided in arm B
/// taking their size-only modify, a lot down then back, each a floor after
/// the last change, then no more — and, every third arm-B cycle, the bid's
/// touch moving after its lot down (its requote ends the step), another
/// the bid's lot down refused (the step ends); under `ab_mode` 2 (XH5d-3b)
/// the bids decided in arm B, at a level formed that cycle, bidding 1 %
/// until the boot's cap — allocate nothing.
#[test]
fn xmm_member_ab_arms_are_zero_alloc() {
    use core_types::{
        CancelReq, Order, OrderEvent, Side, ORDER_EVENT_CANCELED,
        ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_REPLACED, ORDER_EVENT_RESTING,
    };
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    /// The live id and price by side (a placement's, then each modify's);
    /// the modifies sorted — the same price (size-only) or a new one (a
    /// requote); one refusal when asked; the places bidding ALO priority —
    /// arm A's, arm B's bids, arm B's asks.
    struct AbCtx {
        now: u64,
        live: [u64; 2],
        px: [i64; 2],
        size_only: u64,
        repriced: u64,
        refuse: bool,
        prio: [u64; 3],
    }
    impl Ctx for AbCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            let s = usize::from(o.side == Side::Ask);
            if o.alo_priority_1e8 != 0 {
                // `ab_arm` 1 is arm A.
                self.prio[if o.ab_arm == 1 { 0 } else { 1 + s }] += 1;
            }
            self.live[s] = o.client_oid;
            self.px[s] = o.px.raw();
            Ok(())
        }
        fn cancel(&mut self, _r: CancelReq) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn modify(&mut self, _prev: u64, o: Order) -> Result<(), SubmitErr> {
            if self.refuse {
                self.refuse = false;
                return Err(SubmitErr::RingFull);
            }
            let s = usize::from(o.side == Side::Ask);
            if o.px.raw() == self.px[s] {
                self.size_only += 1;
            } else {
                self.repriced += 1;
            }
            self.live[s] = o.client_oid;
            self.px[s] = o.px.raw();
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let bn = core_types::make_symbol_id(VenueId::Binance, 9);
    let mut xp = strategy_xmm::XmmParams::EMPTY;
    xp.perps[0] = strategy_xmm::XmmPerp {
        hl_sym: hl,
        lead_sym: bn,
        lot_1e6: 10_000,
    };
    xp.n_perps = 1;
    xp.maker_enabled = 1;
    xp.theta_bps_1e6 = 500_000;
    xp.gate_window_ms = 1;
    xp.lifetime_ms = 30_000;
    xp.lead_stale_ms = 300;
    xp.follower_stale_ms = 2_000;
    xp.rtt_pull_ms = 1_500;
    xp.requote_min_ms = 250;
    xp.clip_usd_1e6 = 15_000_000;
    xp.inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.gross_inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.resting_cap_usd_1e6 = 1_000_000_000_000;
    xp.ab_mode = 1;
    let mut theta = strategy_xmm::XmmStrategy::new();
    theta.configure(&xp).expect("gate 75e θ params");
    xp.ab_mode = 3;
    let mut size = strategy_xmm::XmmStrategy::new();
    size.configure(&xp).expect("gate 75e size params");
    xp.ab_mode = 2;
    xp.alo_priority_1e8 = 1_000_000;
    let mut prio = strategy_xmm::XmmStrategy::new();
    prio.configure(&xp).expect("gate 75e priority params");
    let t0: u64 = 1_000_000_000_000;
    let mut tc = AbCtx {
        now: t0,
        live: [0; 2],
        px: [0; 2],
        size_only: 0,
        repriced: 0,
        refuse: false,
        prio: [0; 3],
    };
    let mut sc = AbCtx {
        now: t0,
        live: [0; 2],
        px: [0; 2],
        size_only: 0,
        repriced: 0,
        refuse: false,
        prio: [0; 3],
    };
    let mut pc = AbCtx {
        now: t0,
        live: [0; 2],
        px: [0; 2],
        size_only: 0,
        repriced: 0,
        refuse: false,
        prio: [0; 3],
    };
    theta.on_start(&mut tc).expect("gate 75e θ start");
    size.on_start(&mut sc).expect("gate 75e size start");
    prio.on_start(&mut pc).expect("gate 75e priority start");
    let book = |sym: u32, bid: i64, ask: i64, stale: bool| {
        let mut t = Tick::new(
            0,
            if sym == hl {
                VenueId::Hyperliquid
            } else {
                VenueId::Binance
            },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= core_types::TICK_FLAG_STALE;
        }
        t
    };
    let lead = book(bn, 99_990_000, 100_010_000, false);
    // The leader's bid + ask from 200 000 000 to 199 994 000: −0.3 bps.
    let lead_dn = book(bn, 99_987_000, 100_007_000, false);
    let lead_stale = book(bn, 99_990_000, 100_010_000, true);
    let fol = book(hl, 99_990_000, 100_010_000, false);
    // The follower's bid touch half a tick up: the bid requotes.
    let fol_up = book(hl, 99_995_000, 100_010_000, false);
    let ev = |oid: u64, kind: u8, reason: u8| {
        OrderEvent::new(0, VenueId::Hyperliquid, hl, oid, 6, kind, reason, 0)
    };
    let pulled = ORDER_EVENT_REASON_CANCEL_REQUESTED;

    // A multiple of 6: the three arm-B variants run equally often.
    const CYCLES: u64 = 12_000;
    const MS: u64 = 1_000_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        // Cycle i is in block i: even A, odd B.
        let mut f = fol;
        f.venue_time_ms = i * strategy_xmm::XMM_AB_BLOCK_MS + 1;
        let mut fu = fol_up;
        fu.venue_time_ms = f.venue_time_ms;
        // `ab_mode` 1: both sides placed in the block's arm; the leader
        // falls 0.3 bps (arm B's bid pulls, arm A's rests) and comes back;
        // a stale leader tick pulls what rests.
        tc.now = t0 + i * 1_000 * MS;
        theta.on_tick(&lead, &mut tc);
        theta.on_tick(&f, &mut tc);
        let placed = tc.live;
        theta.on_order_event(&ev(placed[0], ORDER_EVENT_RESTING, 0), &mut tc);
        theta.on_order_event(&ev(placed[1], ORDER_EVENT_RESTING, 0), &mut tc);
        tc.now += 10 * MS;
        theta.on_tick(&lead_dn, &mut tc);
        tc.now += 10 * MS;
        theta.on_tick(&lead, &mut tc);
        theta.on_tick(&lead_stale, &mut tc);
        theta.on_order_event(&ev(placed[0], ORDER_EVENT_CANCELED, pulled), &mut tc);
        theta.on_order_event(&ev(placed[1], ORDER_EVENT_CANCELED, pulled), &mut tc);
        // `ab_mode` 3: both sides placed; three steps 300 ms apart (past
        // the 250 ms floor), the leader fresh at each. In arm B: a lot
        // down, then back, then nothing — or (i ≡ 3 mod 6) the bid's touch
        // up after its lot down, its requote ending the step; or (i ≡ 5)
        // the bid's lot down refused. A stale leader tick pulls both.
        sc.now = t0 + i * 1_000 * MS;
        size.on_tick(&lead, &mut sc);
        size.on_tick(&f, &mut sc);
        let mut live = sc.live;
        size.on_order_event(&ev(live[0], ORDER_EVENT_RESTING, 0), &mut sc);
        size.on_order_event(&ev(live[1], ORDER_EVENT_RESTING, 0), &mut sc);
        sc.refuse = i % 6 == 5;
        let mut step = 0;
        while step < 3 {
            sc.now += 300 * MS;
            size.on_tick(&lead, &mut sc);
            size.on_tick(if i % 6 == 3 && step > 0 { &fu } else { &f }, &mut sc);
            let mut s = 0usize;
            while s < 2 {
                if sc.live[s] != live[s] {
                    let replaced = ORDER_EVENT_REASON_REPLACED;
                    size.on_order_event(&ev(live[s], ORDER_EVENT_CANCELED, replaced), &mut sc);
                    size.on_order_event(&ev(sc.live[s], ORDER_EVENT_RESTING, 0), &mut sc);
                    live[s] = sc.live[s];
                }
                s += 1;
            }
            step += 1;
        }
        size.on_tick(&lead_stale, &mut sc);
        size.on_order_event(&ev(live[0], ORDER_EVENT_CANCELED, pulled), &mut sc);
        size.on_order_event(&ev(live[1], ORDER_EVENT_CANCELED, pulled), &mut sc);
        // `ab_mode` 2: the follower's bid improves half a tick into each
        // arm-B cycle (its level forms there; arm A's retreat and the ask
        // form none); both sides placed, then pulled.
        let pf = if i % 2 == 0 { f } else { fu };
        pc.now = t0 + i * 1_000 * MS;
        prio.on_tick(&lead, &mut pc);
        prio.on_tick(&pf, &mut pc);
        let placed = pc.live;
        prio.on_order_event(&ev(placed[0], ORDER_EVENT_RESTING, 0), &mut pc);
        prio.on_order_event(&ev(placed[1], ORDER_EVENT_RESTING, 0), &mut pc);
        prio.on_tick(&lead_stale, &mut pc);
        prio.on_order_event(&ev(placed[0], ORDER_EVENT_CANCELED, pulled), &mut pc);
        prio.on_order_event(&ev(placed[1], ORDER_EVENT_CANCELED, pulled), &mut pc);
        i += 1;
    }

    let (allocs, bytes, _deallocs) = g.delta();
    let (t, s) = (theta.counters(), size.counters());
    assert_eq!(t.placed, 2 * CYCLES, "θ: both sides every cycle");
    assert_eq!(
        t.lead_cancels,
        CYCLES / 2,
        "θ: arm B's bid pulled on half θ, arm A's rested"
    );
    assert_eq!(
        t.pull_cancels,
        3 * CYCLES / 2,
        "θ: the stale pull took what rested"
    );
    assert_eq!(s.placed, 2 * CYCLES, "size: both sides every cycle");
    // Per arm-B cycle: 4 size-only (down and back, both sides); 3 and a
    // requote (the bid's back replaced by its requote); 2 and a refusal
    // (the bid's down refused, the ask's down and back).
    let b = CYCLES / 6;
    assert_eq!(
        (sc.size_only, sc.repriced, s.ctx_refused),
        (b * (4 + 3 + 2), b, b),
        "size: the size-only modifies, the requotes ending a step, the refusals"
    );
    assert_eq!(s.modifies, sc.size_only + sc.repriced);
    assert_eq!(
        StrategyCounters::orders_emitted(&size),
        s.placed + s.modifies
    );
    assert_eq!(s.pull_cancels, 2 * CYCLES, "size: the stale pull took both");
    assert_eq!((tc.size_only, tc.repriced), (0, 0), "θ: no modify");
    // `ab_mode` 2: arm B's bids bid 1 % until the boot's cap; the asks
    // (never moved) and arm A never; the cap stops them well before the
    // last arm-B cycle.
    let pr = prio.counters();
    assert_eq!(pc.prio, [0, pr.priority_places, 0]);
    // Each arm-B bid at 99.995 is 0.15 (the $15 clip in lots): 1 % of it
    // is ⌈149 992.5⌉ = 149 993 USD ×1e6; $25 holds 166 of them — then the
    // cap refuses the remaining arm-B cycles, inside the window.
    assert_eq!(
        (pr.priority_places, pr.priority_fee_usd_1e6),
        (166, 166 * 149_993),
        "the cap stopped them"
    );
    assert!(pr.priority_fee_usd_1e6 <= u64::from(strategy_xmm::XMM_PRIORITY_MAX_USD_1E6));
    assert_eq!(pr.placed, 2 * CYCLES);
    assert_eq!((pr.stuck, pr.unmatched), (0, 0));
    assert_eq!((t.stuck, t.unmatched, s.stuck, s.unmatched), (0, 0, 0, 0));
    assert_eq!(
        allocs, 0,
        "the xmm A/B arms allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "the xmm A/B arms bytes should be zero: saw {bytes}"
    );
}

/// **XMM gate 75d (XH4f)** — the member's health branches (D8): a
/// blocked live path pulling both sides and holding, answers slower than
/// `rtt_pull_ms` pulling, a throttled arm pulling on the timer's pass, and
/// a busy arm's requote deferred then sent — allocate nothing.
#[test]
fn xmm_member_health_pulls_and_busy_requotes_are_zero_alloc() {
    use core_types::{
        CancelReq, ExecHealth, Order, OrderEvent, EXEC_BLOCKED_BUDGET, EXEC_CONGESTION_BUSY,
        EXEC_CONGESTION_THROTTLED, ORDER_EVENT_CANCELED, ORDER_EVENT_RESTING,
    };
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// Answers the health word the script sets; remembers the last ids.
    struct HealthCtx {
        now: u64,
        placed: [u64; 2],
        n: usize,
        modified: u64,
        cancels: u64,
        health: ExecHealth,
    }
    impl Ctx for HealthCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            self.placed[self.n & 1] = o.client_oid;
            self.n += 1;
            Ok(())
        }
        fn cancel(&mut self, _r: CancelReq) -> Result<(), SubmitErr> {
            self.cancels += 1;
            Ok(())
        }
        fn modify(&mut self, _prev: u64, o: Order) -> Result<(), SubmitErr> {
            self.modified = o.client_oid;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
        fn exec_health(&self) -> ExecHealth {
            self.health
        }
    }

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let bn = core_types::make_symbol_id(VenueId::Binance, 9);
    let mut xp = strategy_xmm::XmmParams::EMPTY;
    xp.perps[0] = strategy_xmm::XmmPerp {
        hl_sym: hl,
        lead_sym: bn,
        lot_1e6: 10_000,
    };
    xp.n_perps = 1;
    xp.maker_enabled = 1;
    xp.theta_bps_1e6 = 500_000;
    xp.gate_window_ms = 1;
    xp.lifetime_ms = 30_000;
    xp.lead_stale_ms = 300;
    xp.follower_stale_ms = 2_000;
    xp.rtt_pull_ms = 1_500;
    xp.requote_min_ms = 250;
    xp.clip_usd_1e6 = 15_000_000;
    xp.inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.gross_inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.resting_cap_usd_1e6 = 1_000_000_000_000;
    let mut m = strategy_xmm::XmmStrategy::new();
    m.configure(&xp).expect("gate 75d params");
    let t0: u64 = 1_000_000_000_000;
    let mut ctx = HealthCtx {
        now: t0,
        placed: [0; 2],
        n: 0,
        modified: 0,
        cancels: 0,
        health: ExecHealth::HEALTHY,
    };
    m.on_start(&mut ctx).expect("gate 75d start");
    let book = |sym: u32, bid: i64, ask: i64| {
        Tick::new(
            0,
            if sym == hl {
                VenueId::Hyperliquid
            } else {
                VenueId::Binance
            },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    };
    let lead = book(bn, 99_990_000, 100_010_000);
    let fol = book(hl, 99_990_000, 100_010_000);
    let fol_up = book(hl, 99_995_000, 100_010_000);
    let ev = |oid: u64, kind: u8, reason: u8| {
        OrderEvent::new(0, VenueId::Hyperliquid, hl, oid, 6, kind, reason, 0)
    };
    let done = core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED;
    let blocked = ExecHealth::new(EXEC_BLOCKED_BUDGET, 0, 0);
    let slow = ExecHealth::new(0, 0, 1_600);
    let throttled = ExecHealth::new(0, EXEC_CONGESTION_THROTTLED, 0);
    let busy = ExecHealth::new(0, EXEC_CONGESTION_BUSY, 0);

    const CYCLES: u64 = 10_000;
    const MS: u64 = 1_000_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        ctx.now = t0 + i * 10_000 * MS;
        ctx.health = ExecHealth::HEALTHY;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol, &mut ctx); // both sides placed
        let (bid, ask) = (ctx.placed[0], ctx.placed[1]);
        m.on_order_event(&ev(bid, ORDER_EVENT_RESTING, 0), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_RESTING, 0), &mut ctx);
        // Busy: the bid touch rose 300 ms on — deferred; 1 050 ms on — sent.
        ctx.health = busy;
        ctx.now += 300 * MS;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol_up, &mut ctx);
        ctx.now += 750 * MS;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol_up, &mut ctx); // MODIFY
        let new = ctx.modified;
        m.on_order_event(
            &ev(
                bid,
                ORDER_EVENT_CANCELED,
                core_types::ORDER_EVENT_REASON_REPLACED,
            ),
            &mut ctx,
        );
        m.on_order_event(&ev(new, ORDER_EVENT_RESTING, 0), &mut ctx);
        // The cycle's pull: blocked, slow or throttled (the timer's pass).
        ctx.now += 10 * MS;
        match i % 3 {
            0 => {
                ctx.health = blocked;
                m.on_tick(&fol_up, &mut ctx);
            }
            1 => {
                ctx.health = slow;
                m.on_tick(&fol_up, &mut ctx);
            }
            _ => {
                ctx.health = throttled;
                m.on_timer(ctx.now, &mut ctx);
            }
        }
        m.on_order_event(&ev(new, ORDER_EVENT_CANCELED, done), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_CANCELED, done), &mut ctx);
        i += 1;
    }
    std::hint::black_box(ctx.cancels);

    let (allocs, bytes, _deallocs) = g.delta();
    let c = m.counters();
    assert_eq!((c.modifies, c.busy_deferred), (CYCLES, CYCLES));
    assert_eq!(
        c.blocked_cancels + c.rtt_cancels + c.congestion_cancels,
        2 * CYCLES,
        "each cycle's pull took both sides"
    );
    assert!(c.blocked_cancels > 0 && c.rtt_cancels > 0 && c.congestion_cancels > 0);
    assert_eq!((c.stuck, c.unmatched, c.pull_cancels), (0, 0, 0));
    assert_eq!(
        allocs, 0,
        "xmm health branches allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "xmm health branches bytes should be zero: saw {bytes}"
    );
}

/// **XMM gate 75c (XH2)** — the member's fail-safe branches: a refused
/// replacement handing the side back to its predecessor (and the blind
/// cancel that sends it), the member's own TTL cancel (LAW E-8), the
/// stuck watchdog's release and blind cancels, and the gate failing
/// closed on a leader burst that overwrote its history — allocate
/// nothing.
#[test]
fn xmm_member_refused_replacement_ttl_stuck_and_gate_overflow_are_zero_alloc() {
    use core_types::{CancelReq, Order, OrderEvent, ORDER_EVENT_CANCELED, ORDER_EVENT_REJECTED,
        ORDER_EVENT_RESTING};
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// Remembers the last ids it was handed in fixed slots: placements
    /// by side, the latest modify's new id.
    struct ModCtx {
        now: u64,
        placed: [u64; 2],
        n: usize,
        modified: u64,
        cancels: u64,
    }
    impl Ctx for ModCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            self.placed[self.n & 1] = o.client_oid;
            self.n += 1;
            Ok(())
        }
        fn cancel(&mut self, _r: CancelReq) -> Result<(), SubmitErr> {
            self.cancels += 1;
            Ok(())
        }
        fn modify(&mut self, _prev: u64, o: Order) -> Result<(), SubmitErr> {
            self.modified = o.client_oid;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let bn = core_types::make_symbol_id(VenueId::Binance, 9);
    let mut xp = strategy_xmm::XmmParams::EMPTY;
    xp.perps[0] = strategy_xmm::XmmPerp {
        hl_sym: hl,
        lead_sym: bn,
        lot_1e6: 10_000,
    };
    xp.n_perps = 1;
    xp.maker_enabled = 1;
    xp.theta_bps_1e6 = 500_000;
    xp.gate_window_ms = 1;
    // A 1 s TTL, and feeds that stay fresh for 2 s: the timer's TTL and
    // watchdog branches run, its stale pull never does.
    xp.lifetime_ms = 1_000;
    xp.lead_stale_ms = 2_000;
    xp.follower_stale_ms = 2_000;
    xp.rtt_pull_ms = 1_500;
    xp.requote_min_ms = 250;
    xp.clip_usd_1e6 = 15_000_000;
    xp.inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.gross_inv_cap_usd_1e6 = 1_000_000_000_000;
    xp.resting_cap_usd_1e6 = 1_000_000_000_000;
    let mut m = strategy_xmm::XmmStrategy::new();
    m.configure(&xp).expect("gate 75c params");
    let t0: u64 = 1_000_000_000_000;
    let mut ctx = ModCtx {
        now: t0,
        placed: [0; 2],
        n: 0,
        modified: 0,
        cancels: 0,
    };
    m.on_start(&mut ctx).expect("gate 75c start");
    let book = |sym: u32, bid: i64, ask: i64| {
        Tick::new(
            0,
            if sym == hl { VenueId::Hyperliquid } else { VenueId::Binance },
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    };
    let lead = book(bn, 99_990_000, 100_010_000);
    let fol = book(hl, 99_990_000, 100_010_000);
    let fol_up = book(hl, 99_995_000, 100_010_000);
    let ev = |oid: u64, kind: u8, reason: u8| {
        OrderEvent::new(0, VenueId::Hyperliquid, hl, oid, 6, kind, reason, 0)
    };

    const CYCLES: u64 = 10_000;
    const MS: u64 = 1_000_000;
    const SEC: u64 = 1_000 * MS;
    // Every 625th cycle (16 in all) a leader burst one past the gate's
    // 8 192-sample ring lands inside one 1 ms gate window.
    const BURST_EVERY: u64 = 625;
    const BURST: u64 = 8_193;
    let g = AllocGuard::new();
    let mut bursts = 0u64;
    let mut i = 0u64;
    while i < CYCLES {
        let t = t0 + i * 20 * SEC;
        ctx.now = t;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol, &mut ctx); // both sides placed
        let (bid, ask) = (ctx.placed[0], ctx.placed[1]);
        m.on_order_event(&ev(bid, ORDER_EVENT_RESTING, 0), &mut ctx);
        m.on_order_event(&ev(ask, ORDER_EVENT_RESTING, 0), &mut ctx);
        ctx.now = t + 300 * MS;
        m.on_tick(&lead, &mut ctx);
        m.on_tick(&fol_up, &mut ctx); // the bid requoted by MODIFY ...
        let new = ctx.modified;
        // ... whose replacement is refused: the predecessor is the
        // side's order again, and is cancelled blind.
        m.on_order_event(&ev(new, ORDER_EVENT_REJECTED, core_types::ORDER_EVENT_REASON_OTHER), &mut ctx);
        m.on_order_event(&ev(bid, ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_CANCEL_REQUESTED), &mut ctx);
        ctx.now = t + 1_200 * MS;
        m.on_timer(ctx.now, &mut ctx); // the ask's TTL: the member cancels it
        m.on_order_event(&ev(ask, ORDER_EVENT_CANCELED, core_types::ORDER_EVENT_REASON_EXPIRED), &mut ctx);
        // Placed again, and no event ever comes: the member's TTL cancel
        // at +1 s, the watchdog's release (and blind cancels) 10 s later.
        let mut sec = 2u64;
        while sec <= 14 {
            ctx.now = t + sec * SEC;
            m.on_tick(&lead, &mut ctx);
            m.on_tick(&fol, &mut ctx);
            m.on_timer(ctx.now, &mut ctx);
            sec += 1;
        }
        if i % BURST_EVERY == 0 {
            let tb = t + 15 * SEC;
            let mut j = 0u64;
            while j < BURST {
                ctx.now = tb + j * 100;
                m.on_tick(&lead, &mut ctx);
                j += 1;
            }
            ctx.now = tb + BURST * 100;
            m.on_tick(&fol, &mut ctx); // both sides: history lost, gated
            bursts += 1;
        }
        i += 1;
    }
    std::hint::black_box(ctx.cancels);

    let (allocs, bytes, _deallocs) = g.delta();
    let c = m.counters();
    assert_eq!(c.placed, 4 * CYCLES, "two placements per side per cycle");
    assert_eq!((c.modifies, c.rejected_other), (CYCLES, CYCLES), "one refused replacement per cycle");
    assert_eq!(c.requote_cancels, CYCLES, "the predecessor cancelled after its refused replacement");
    assert_eq!(c.expiry_cancels, 3 * CYCLES, "the member's own TTL cancels");
    assert_eq!(c.stuck, 2 * CYCLES, "the watchdog released both sides");
    assert_eq!(c.gate_overflow, 2 * bursts, "the gate failed closed on both sides");
    assert_eq!((c.pull_cancels, c.unmatched, c.ctx_refused), (0, 0, 0));
    assert_eq!(allocs, 0, "xmm fail-safe branches allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "xmm fail-safe branches bytes should be zero: saw {bytes}");
}

/// **XMM gate 76 (XH2)** — the paper dispatcher's queue path after boot:
/// post-only submits (one crossing → `BAD_ALO_PX`), landing on a book, a
/// partial fill and FILLED from prints, a modify, a refused modify of a
/// vanished order, a cancel landed by a stale book, a replacement
/// REJECTED on landing because its predecessor filled in flight, a modify
/// refused by a full table, and both pumps — allocate nothing.
#[test]
fn paper_dispatcher_queue_path_is_zero_alloc() {
    use clob_dispatcher::{OrderDispatch, PaperDispatcher};
    use core_types::{CancelReq, ModifyReq, Order, OrderEvent, Side, TradePrint};

    let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 5);
    let order = |side: Side, px: i64, oid: u64, ts: u64| {
        let mut o = Order::new(
            ts,
            VenueId::Hyperliquid,
            hl,
            side,
            0,
            Price::from_raw(px),
            Qty::from_raw(2_000_000),
            oid,
        )
        .with_post_only();
        o.strategy_id = 6;
        o
    };
    let book = |bid: i64, ask: i64, stale: bool| {
        let mut t = Tick::new(
            0,
            VenueId::Hyperliquid,
            hl,
            0,
            Price::from_raw(bid),
            Qty::from_raw(0),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        );
        if stale {
            t.flags |= core_types::TICK_FLAG_STALE;
        }
        t
    };
    let print = |px: i64, qty: i64| {
        TradePrint::new(0, VenueId::Hyperliquid, hl, 1, 0, px, qty, core_types::TRADE_AGGRESSOR_SELL)
    };
    // Boot (allocation allowed).
    let mut d = PaperDispatcher::new();
    d.track_queue_sym(hl);
    d.observe_tick(&book(100_000_000, 101_000_000, false), 0);
    const DELTA: u64 = 340_000_000;
    const CYCLES: u64 = 10_000;
    const TABLE: u64 = core_fill::QUEUE_MAX_ORDERS as u64;
    let mut ev = OrderEvent::ZERO;
    let mut kinds = 0u64;
    let mut stale_rejects = 0u64;
    let mut full = 0u64;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        let t = (i + 1) * 16 * DELTA;
        let oid = i * 64;
        d.submit(&order(Side::Bid, 100_000_000, oid + 1, t)).unwrap();
        d.submit(&order(Side::Bid, 101_000_000, oid + 2, t)).unwrap(); // crosses the ask
        d.submit(&order(Side::Ask, 102_000_000, oid + 3, t)).unwrap();
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + DELTA);
        d.observe_trade(&print(100_000_000, 1_000_000), t + DELTA + 1); // partial
        d.observe_trade(&print(100_000_000, 5_000_000), t + DELTA + 2); // the rest: FILLED
        let requote = order(Side::Ask, 101_500_000, oid + 4, t + 2 * DELTA);
        d.modify(&ModifyReq::new(oid + 3, requote)).unwrap();
        let _ = d.modify(&ModifyReq::new(oid + 1, order(Side::Bid, 99_000_000, oid + 5, t + 2 * DELTA)));
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + 3 * DELTA);
        d.cancel(&CancelReq::of(&requote, t + 3 * DELTA)).unwrap();
        d.observe_tick(&book(100_000_000, 101_000_000, true), t + 4 * DELTA + 1);
        // A modify whose predecessor fills while it flies: the
        // replacement is REJECTED on landing (fail-closed).
        d.submit(&order(Side::Bid, 100_000_000, oid + 6, t + 5 * DELTA)).unwrap();
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + 6 * DELTA);
        let late = order(Side::Bid, 99_500_000, oid + 7, t + 6 * DELTA + 1);
        d.modify(&ModifyReq::new(oid + 6, late)).unwrap();
        d.observe_trade(&print(100_000_000, 5_000_000), t + 6 * DELTA + 2);
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + 7 * DELTA + 2);
        // A full table: a modify needs a free row, so it is refused whole.
        let mut k = 0u64;
        while k < TABLE {
            d.submit(&order(Side::Bid, 90_000_000, oid + 8 + k, t + 8 * DELTA)).unwrap();
            k += 1;
        }
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + 9 * DELTA);
        let refused = order(Side::Bid, 91_000_000, oid + 8 + TABLE, t + 9 * DELTA + 1);
        if d.modify(&ModifyReq::new(oid + 8, refused)).is_err() {
            full += 1;
        }
        k = 0;
        while k < TABLE {
            let o = order(Side::Bid, 90_000_000, oid + 8 + k, t + 9 * DELTA + 2);
            d.cancel(&CancelReq::of(&o, t + 9 * DELTA + 2)).unwrap();
            k += 1;
        }
        d.observe_tick(&book(100_000_000, 101_000_000, false), t + 11 * DELTA);
        while let Some(f) = d.try_next_fill() {
            kinds = kinds.wrapping_add(f.qty.raw() as u64);
        }
        while d.try_next_order_event(&mut ev) {
            kinds = kinds.wrapping_add(u64::from(ev.kind));
            if ev.kind == core_types::ORDER_EVENT_REJECTED && ev.reason == core_types::ORDER_EVENT_REASON_OTHER {
                stale_rejects += 1;
            }
        }
        i += 1;
    }
    std::hint::black_box(kinds);

    let (allocs, bytes, _deallocs) = g.delta();
    let c = d.matcher_counters();
    assert_eq!(c.queue_rejected_alo, CYCLES, "one crossing bid per cycle");
    assert_eq!(c.queue_fills, 3 * CYCLES, "a partial, the rest, and the in-flight predecessor");
    assert_eq!(
        c.queue_canceled,
        (2 + TABLE) * CYCLES,
        "the replaced ask, its cancelled requote, and the full table"
    );
    assert_eq!(stale_rejects, CYCLES, "the in-flight replacement, rejected on landing");
    assert_eq!(full, CYCLES, "the full table's modify, refused");
    assert_eq!(c.no_such_order, CYCLES, "the vanished bid's modify, refused");
    assert_eq!((c.out_overflow, c.order_events_overflow), (0, 0));
    assert_eq!(d.open_paper_orders(), 0, "every cycle ends flat");
    assert_eq!(allocs, 0, "paper queue path allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "paper queue path bytes should be zero: saw {bytes}");
}

/// Phase 8f item 6: the engine-thread fills capture
/// (`SlotCapture<Fill>` → engine-fills.pmlr) must stage + flush with
/// zero allocations after boot — it sits on the engine thread's fill
/// dispatch path.
#[test]
fn engine_fills_capture_append_is_zero_alloc() {
    use core_io::{SlotCapture, SlotKind};
    use core_types::{Fill, Side};

    let cap_dir =
        std::env::temp_dir().join(format!("stage2_alloc_fills_capture_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    std::fs::create_dir_all(&cap_dir).unwrap();
    let path = cap_dir.join(engine::ENGINE_FILLS_FILE);
    let mut capture: SlotCapture<Fill> = SlotCapture::open(&path, SlotKind::Fill, 1).unwrap();

    const CYCLES: u64 = 10_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        let f = Fill::new(
            i,
            7,
            Side::Bid,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            i,
        );
        capture.append(&f);
        // Exercise the periodic-drain branch inside the window too —
        // interval elapsed on every call (last_flush starts at 0 and
        // the interval is < the synthetic clock we feed).
        capture.maybe_flush(i.wrapping_mul(10_000_000_000));
        i += 1;
    }

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(capture.records(), CYCLES);
    assert_eq!(capture.io_errors(), 0);
    assert!(!capture.is_disabled());
    assert_eq!(
        allocs, 0,
        "fills capture path allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "fills capture path bytes should be zero: saw {bytes}"
    );
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// M4.1 (M-a + M-c): the order-intent capture (`SlotCapture<Order>` →
/// engine-orders.pmlr) and the strategy-set `StampCtx` attribution
/// stamp must both run zero-alloc — they sit on the engine thread's
/// submit path.
#[test]
fn engine_orders_capture_and_stamp_are_zero_alloc() {
    use core_io::{SlotCapture, SlotKind};
    use core_types::{Order, Side, VenueId};
    use strategy_core::{Ctx, SubmitErr};
    use strategy_set::{StampCtx, SLOT_VM};

    struct SinkCtx {
        submitted: u64,
    }
    impl Ctx for SinkCtx {
        #[inline(always)]
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            // The stamp must land BEFORE the sink sees the order.
            assert_eq!(order.strategy_id, SLOT_VM);
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            0
        }
    }

    let cap_dir = std::env::temp_dir().join(format!(
        "stage2_alloc_orders_capture_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&cap_dir);
    std::fs::create_dir_all(&cap_dir).unwrap();
    let path = cap_dir.join(engine::ENGINE_ORDERS_FILE);
    let mut capture: SlotCapture<Order> = SlotCapture::open(&path, SlotKind::Order, 1).unwrap();
    let mut sink = SinkCtx { submitted: 0 };

    const CYCLES: u64 = 10_000;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < CYCLES {
        let o = Order::new(
            i,
            VenueId::Polymarket,
            42,
            Side::Bid,
            0,
            Price::from_raw(410_000),
            Qty::from_raw(1_000_000),
            i,
        );
        let mut stamped = StampCtx::new(&mut sink, SLOT_VM);
        Ctx::submit(&mut stamped, o).unwrap();
        capture.append(&o);
        capture.maybe_flush(i.wrapping_mul(10_000_000_000));
        i += 1;
    }

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(sink.submitted, CYCLES);
    assert_eq!(capture.records(), CYCLES);
    assert_eq!(capture.io_errors(), 0);
    assert!(!capture.is_disabled());
    assert_eq!(
        allocs, 0,
        "orders capture/stamp path allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "orders capture/stamp bytes should be zero: saw {bytes}"
    );
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// Phase 8f item 8: `strategy-ai-exec`'s tick path (fair-table probe,
/// lazy-tracked book apply, deviation quote) must be zero-alloc in
/// steady state — it runs inside `Engine::tick()` for every market
/// tick when the member is enabled.
#[test]
fn ai_exec_on_tick_is_zero_alloc() {
    use core_types::{make_symbol_id, AiCmd, AiCmdKind, Order, AI_SIDE_NONE, STRATEGY_SLOT_NONE};
    use strategy_ai_exec::AiExec;
    use strategy_core::{Ctx, Strategy, SubmitErr};

    struct CountCtx {
        submitted: u64,
        now: u64,
    }
    impl Ctx for CountCtx {
        fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    const T0: u64 = 1_000_000_000_000;
    let pm = make_symbol_id(VenueId::Polymarket, 7);
    let other = make_symbol_id(VenueId::Binance, 9);

    // Boot (allocation allowed): fair entry + first tick claims the
    // lazy book slot.
    let mut s: AiExec<64> = AiExec::new();
    s.set_cooldown_ns(0);
    let mut ctx = CountCtx {
        submitted: 0,
        now: T0,
    };
    s.on_start(&mut ctx).unwrap();
    let fair = AiCmd::new(
        T0,
        1,
        pm,
        500_000,
        0,
        3_600_000_000_000,
        AiCmdKind::SetFairValue,
        VenueId::Ai,
        STRATEGY_SLOT_NONE,
        AI_SIDE_NONE,
        0,
        0,
    );
    s.on_ai(&fair, &mut ctx);
    let quote_tick = Tick::new(
        0,
        VenueId::Polymarket,
        pm,
        1,
        Price::from_raw(690_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(710_000),
        Qty::from_raw(1_000_000),
    );
    let ignored_tick = Tick::new(
        0,
        VenueId::Binance,
        other,
        1,
        Price::from_raw(490_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(510_000),
        Qty::from_raw(1_000_000),
    );
    s.on_tick(&quote_tick, &mut ctx); // lazy track happens here

    const CYCLES: u32 = 10_000;
    let g = AllocGuard::new();
    let mut i = 0u32;
    while i < CYCLES {
        ctx.now += 1;
        s.on_tick(&quote_tick, &mut ctx); // quote path
        s.on_tick(&ignored_tick, &mut ctx); // no-fair-entry path
        i += 1;
    }
    std::hint::black_box(ctx.submitted);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        ctx.submitted >= u64::from(CYCLES),
        "deviation quote must fire every cycle"
    );
    assert_eq!(
        allocs, 0,
        "ai-exec on_tick allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "ai-exec on_tick bytes should be zero: saw {bytes}"
    );
}

/// Phase 8f item 8: `strategy-ai-exec`'s AI-lane path (fair/bias
/// upserts, heartbeat liveness, intent honor AND stale-refusal incl.
/// the silence sweep) must be zero-alloc — it runs inside the
/// engine's budgeted AI drain.
#[test]
fn ai_exec_on_ai_is_zero_alloc() {
    use core_types::{
        make_symbol_id, AiCmd, AiCmdKind, Order, Side, AI_CMD_FLAG_EXPIRE_ON_SILENCE, AI_SIDE_NONE,
        STRATEGY_SLOT_AI_EXEC, STRATEGY_SLOT_NONE,
    };
    use strategy_ai_exec::AiExec;
    use strategy_core::{Ctx, Strategy, SubmitErr};

    struct CountCtx {
        submitted: u64,
        now: u64,
    }
    impl Ctx for CountCtx {
        fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
            self.submitted += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    fn fair(ts: u64, sym: core_types::SymbolId, eos: u16) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            sym,
            500_000,
            0,
            3_600_000_000_000,
            AiCmdKind::SetFairValue,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            eos,
        )
    }

    const T0: u64 = 1_000_000_000_000;
    // Past the 15 s staleness window each cycle — exercises the
    // sweep + intent-refusal branches without any wall time.
    const GAP: u64 = strategy_ai_exec::AI_STALENESS_NS + 1_000;
    let pm = make_symbol_id(VenueId::Polymarket, 7);

    let mut s: AiExec<64> = AiExec::new();
    let mut ctx = CountCtx {
        submitted: 0,
        now: T0,
    };
    s.on_start(&mut ctx).unwrap();
    s.on_ai(&fair(T0, pm, 0), &mut ctx); // boot upsert

    const CYCLES: u32 = 10_000;
    let g = AllocGuard::new();
    let mut ts = T0;
    let mut i = 0u32;
    while i < CYCLES {
        // Silence window closes: this intent is REFUSED (stale) and
        // the expire_on_silence sweep runs.
        ts += GAP;
        let refused = AiCmd::new(
            ts,
            1,
            pm,
            430_000,
            2_000_000,
            1_000_000_000,
            AiCmdKind::OrderIntent,
            VenueId::Polymarket,
            STRATEGY_SLOT_AI_EXEC,
            Side::Bid as u8,
            0,
            0,
        );
        s.on_ai(&refused, &mut ctx);
        // Live sequence: heartbeat, upserts (one flagged for the next
        // sweep), honored intent.
        ts += 1;
        let hb = AiCmd::new(
            ts,
            1,
            core_types::SYMBOL_ID_NONE,
            0,
            0,
            0,
            AiCmdKind::Heartbeat,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        );
        s.on_ai(&hb, &mut ctx);
        ts += 1;
        s.on_ai(&fair(ts, pm, 1), &mut ctx);
        ts += 1;
        // Flag carried on the bias too — upserts are last-writer-wins
        // for the entry policy, and the next cycle's sweep must find
        // the entry flagged.
        let bias = AiCmd::new(
            ts,
            1,
            pm,
            -10_000,
            0,
            3_600_000_000_000,
            AiCmdKind::SetBias,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            AI_CMD_FLAG_EXPIRE_ON_SILENCE,
        );
        s.on_ai(&bias, &mut ctx);
        ts += 1;
        let honored = AiCmd::new(
            ts,
            1,
            pm,
            430_000,
            2_000_000,
            1_000_000_000,
            AiCmdKind::OrderIntent,
            VenueId::Polymarket,
            STRATEGY_SLOT_AI_EXEC,
            Side::Bid as u8,
            0,
            0,
        );
        s.on_ai(&honored, &mut ctx);
        i += 1;
    }
    std::hint::black_box(ctx.submitted);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(s.intents_refused_stale, u64::from(CYCLES));
    assert_eq!(s.intents_honored, u64::from(CYCLES));
    assert_eq!(ctx.submitted, u64::from(CYCLES));
    assert!(s.silence_expired >= 1, "sweep must have run");
    assert_eq!(
        allocs, 0,
        "ai-exec on_ai allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "ai-exec on_ai bytes should be zero: saw {bytes}");
}

/// Gate 34 (8g §10): the §4.2 ruleset validator seam — a max-size
/// (256-row) VALID ruleset AND a battery of per-rule rejects, scanned
/// over `&[u8]` into a prewarmed scratch table, must be 0 B/op. The
/// `fs::read` that produces the bytes in production is the documented
/// operator-cadence copy #0 and sits OUTSIDE this seam (fixtures are
/// built before the guard).
#[test]
fn ruleset_validator_is_zero_alloc() {
    fn hash128_of(bytes: &[u8]) -> [u8; 16] {
        let digest = core_crypto::sha256(bytes);
        let mut h = [0u8; 16];
        h.copy_from_slice(&digest[..16]);
        h
    }

    // Sorted universe: 256 action syms (3, 6, ..768) + reference 1000.
    let mut universe: Vec<u32> = (1..=256u32).map(|i| i * 3).collect();
    universe.push(1_000);

    // Max-size valid ruleset: 255 v1 cross_deviation rows + one v2
    // grammar row (VM2 V4 — the gate covers BOTH arms, descriptor
    // resolution + capability checks included), distinct syms and
    // names; $3.90/row keeps every budget green.
    let mut json = String::from(r#"{"rows":["#);
    for i in 0..255u32 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            r#"{{"name":"r{i:03}","family":"crypto","trigger":{{"type":"cross_deviation","ref":1000}},"sym":{},"side":"both","edge_bps":80,"horizon_ms":1500,"max_risk_usd":3.9}}"#,
            (i + 1) * 3
        ));
    }
    json.push_str(
        r#",{"name":"v2row","instrument":"okx:BTC-USDT-SWAP","ref":"binance-usdm:btcusdt","feature":"apr24","combine":"diff","enter":0.20,"exit":0.0,"confirm_feature":"apr72","confirm":0.30,"confirm_abs":true,"confirm_pair":true,"group":3,"min_hold_s":600,"max_hold_s":864000,"horizon_ms":60000,"max_risk_usd":3.9}"#,
    );
    json.push_str("]}");
    let valid_bytes = json.into_bytes();
    let valid_hash = hash128_of(&valid_bytes);

    // Reject battery — one reachable fixture per §4.2 rule. Rule 1
    // reuses the valid bytes under a wrong hash.
    let wrong_hash = [0xEEu8; 16];
    let row = |name: &str, trig: &str, sym: &str, risk: &str| {
        format!(
            r#"{{"name":"{name}","family":"crypto","trigger":{trig},"sym":{sym},"side":"bid","edge_bps":80,"horizon_ms":1500,"max_risk_usd":{risk}}}"#
        )
    };
    let cd = r#"{"type":"cross_deviation","ref":1000}"#;
    let reject_bodies: Vec<Vec<u8>> = vec![
        // Rule 2: unknown row key.
        format!(
            r#"{{"rows":[{}]}}"#,
            row("j2", cd, "3", "3.9").replace(r#""side""#, r#""bogus":1,"side""#)
        )
        .into_bytes(),
        // Rule 3: exponent.
        format!(r#"{{"rows":[{}]}}"#, row("j3", cd, "3", "5e1")).into_bytes(),
        // Rule 4: empty rows.
        br#"{"rows":[]}"#.to_vec(),
        // Rule 5: duplicate name (levels differ so rule 8 stays out).
        format!(
            r#"{{"rows":[{},{}]}}"#,
            row("dup", r#"{"type":"level_breach","level":0.01}"#, "3", "3.9"),
            row("dup", r#"{"type":"level_breach","level":0.02}"#, "3", "3.9"),
        )
        .into_bytes(),
        // Rule 6: sym outside the universe.
        format!(r#"{{"rows":[{}]}}"#, row("j6", cd, "4", "3.9")).into_bytes(),
        // Rule 7: per-row cap breach (operator ruling 2026-08-29,
        // $50k tier: the cap is $10,000/row now).
        format!(r#"{{"rows":[{}]}}"#, row("j7", cd, "3", "10000.01")).into_bytes(),
        // Rule 8: exact duplicate row.
        format!(
            r#"{{"rows":[{},{}]}}"#,
            row("j8a", cd, "3", "3.9"),
            row("j8b", cd, "3", "3.9")
        )
        .into_bytes(),
    ];
    let rejects: Vec<(Vec<u8>, [u8; 16])> = reject_bodies
        .into_iter()
        .map(|b| {
            let h = hash128_of(&b);
            (b, h)
        })
        .collect();

    let mut scratch = Box::new(core_types::RuleTableV2::EMPTY);
    // VM2 V4: the v2 row resolves through here — the resolve path
    // (binary search) sits inside the measured window.
    let descs = ingress_ai::DescriptorTable::from_entries(vec![
        (
            "okx:BTC-USDT-SWAP".to_owned(),
            2_000,
            ingress_ai::CAP_PRICE | ingress_ai::CAP_FUNDING | ingress_ai::CAP_DEPTH,
        ),
        (
            "binance-usdm:btcusdt".to_owned(),
            2_001,
            ingress_ai::CAP_PRICE | ingress_ai::CAP_FUNDING,
        ),
    ]);
    // Prewarm + prove the fixtures behave before measuring.
    ingress_ai::validate_ruleset(&valid_bytes, &valid_hash, &universe, &descs, &mut scratch)
        .expect("max-size ruleset must validate");
    assert_eq!(scratch.len, 256);
    assert!(ingress_ai::validate_ruleset(
        &valid_bytes,
        &wrong_hash,
        &universe,
        &descs,
        &mut scratch
    )
    .is_err());
    for (b, h) in &rejects {
        assert!(ingress_ai::validate_ruleset(b, h, &universe, &descs, &mut scratch).is_err());
    }

    let g = AllocGuard::new();
    for _ in 0..50u32 {
        let ok = ingress_ai::validate_ruleset(
            &valid_bytes,
            &valid_hash,
            &universe,
            &descs,
            &mut scratch,
        );
        std::hint::black_box(ok.is_ok());
        std::hint::black_box(&scratch.len);
        // Rule 1 reject on the same bytes.
        let r1 = ingress_ai::validate_ruleset(
            &valid_bytes,
            &wrong_hash,
            &universe,
            &descs,
            &mut scratch,
        );
        std::hint::black_box(r1.is_err());
        // Rules 2–8 rejects.
        let mut k = 0usize;
        while k < rejects.len() {
            let (b, h) = &rejects[k];
            let r = ingress_ai::validate_ruleset(b, h, &universe, &descs, &mut scratch);
            std::hint::black_box(r.is_err());
            k += 1;
        }
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "ruleset validator allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "ruleset validator bytes should be zero: saw {bytes}"
    );
}

/// Gate 35 (8g §10): the §6 table-handoff seam — ring push (documented
/// copy #1, scratch → slot), pop + member receive (copy #2, slot →
/// staged buffer via `VmStrategy::receive_table`), the §5 push-full
/// reject path AND the Commit flip (index swap, no copy) must be
/// 0 B/op: the copies move 16 KiB + 64 of bytes, never the heap. The
/// Commit-flip third joined with the vm member (§12 item 5, this
/// gate's original parenthetical); ring + vm construction is
/// boot-time and sits outside the guard.
#[test]
fn ruleset_table_handoff_is_zero_alloc() {
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_vm::VmStrategy;

    struct Noop;
    impl Ctx for Noop {
        fn submit(&mut self, _order: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            1_000_000
        }
    }

    // Full-size table so every copy moves the entire body (contents
    // are immaterial to the seam).
    let mut table = Box::new(core_types::RuleTableV2::EMPTY);
    table.len = core_types::RULE_TABLE_ROWS as u32;
    table.hash128 = [0xA5; 16];
    let ring: std::sync::Arc<
        Ring<core_types::RuleTableSlot, { core_types::RULE_TABLE_RING_SLOTS }>,
    > = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // The flip consumer + the in-stream Commit (px/qty = the [0xA5;16]
    // identity halves, the shared `ruleset_hash128` pairing).
    let mut vm: Box<VmStrategy> = Box::new(VmStrategy::new());
    let mut ctx = Noop;
    let commit = core_types::AiCmd::new(
        1,
        1,
        core_types::SYMBOL_ID_NONE,
        i64::from_le_bytes([0xA5; 8]),
        i64::from_le_bytes([0xA5; 8]),
        0,
        core_types::AiCmdKind::RulesetCommit,
        VenueId::Ai,
        core_types::STRATEGY_SLOT_VM,
        core_types::AI_SIDE_NONE,
        0,
        0,
    );

    // Prewarm: one full round trip incl. receive + flip before the
    // measurement window.
    table.epoch = 0;
    assert!(prod.try_push_ref(&table));
    vm.receive_table_v2(&cons.try_pop_ref().expect("prewarm pop"));
    vm.on_ai(&commit, &mut ctx);
    assert_eq!(vm.commits_applied, 1, "prewarm flip must land");

    let mut ok_pushes = 0u32;
    let mut full_rejects = 0u32;
    let mut pops = 0u32;
    let g = AllocGuard::new();
    let mut i = 0u32;
    while i < 50 {
        // Two stages fill the RULE_TABLE_RING_SLOTS = 2 ring …
        table.epoch = 2 * i + 1;
        ok_pushes += u32::from(prod.try_push_ref(&table));
        table.epoch = 2 * i + 2;
        ok_pushes += u32::from(prod.try_push_ref(&table));
        // … the third is the §5 push-full reject path.
        full_rejects += u32::from(!prod.try_push_ref(&table));
        while let Some(t) = cons.try_pop_ref() {
            std::hint::black_box(t.epoch);
            // Copy #2: popped slot → member staged buffer. The second
            // pop of the pair overwrites the first — the engine-side
            // restage-supersedes mirror, measured too.
            vm.receive_table_v2(&t);
            pops += 1;
        }
        // Commit flip third (§10): hash match ⇒ index swap, no copy.
        vm.on_ai(&commit, &mut ctx);
        i += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(ok_pushes, 100);
    assert_eq!(full_rejects, 50, "cap-2 ring must reject the third stage");
    assert_eq!(pops, 100);
    assert_eq!(vm.commits_applied, 51, "every in-window Commit must flip");
    assert_eq!(vm.commits_dropped, 0);
    assert_eq!(vm.active_epoch(), 100, "last flip exposes the last pop");
    assert_eq!(
        allocs, 0,
        "ruleset table handoff allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "ruleset table handoff bytes should be zero: saw {bytes}"
    );
}

/// Gate 36 (8g §10): `VmStrategy::on_tick` steady state — a full-table
/// (256-row) tick storm including fires, cooldown re-arms and clamped
/// submits into a placeholder order ring — must be 0 B/op. The storm
/// covers every hot branch: level_breach + cross_deviation fires, the
/// policy re-clamp (a hand-built over-cap row), the qty-floor
/// clamp-to-zero path, ref-leg book refreshes, the irrelevant-sym
/// relevance-scan miss, and sleeping rows. Construction + book
/// tracking are boot-time (prewarmed) and sit outside the guard.
#[test]
fn vm_on_tick_steady_state_is_zero_alloc() {
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_vm::VmStrategy;

    /// Placeholder order ring behind the ctx: submits land in a real
    /// SPSC ring, drained by the test loop like the engine would.
    struct RingCtx {
        prod: core_ring::Producer<core_types::Order, 1024>,
        now: u64,
    }
    impl Ctx for RingCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            if self.prod.try_push_ref(&order) {
                Ok(())
            } else {
                Err(SubmitErr::RingFull)
            }
        }
        fn now_ns(&self) -> core_time::NsTs {
            self.now
        }
    }

    const REF_SYM: u32 = 1_000;
    const IRRELEVANT_SYM: u32 = 5_000;

    // 256 rows over 256 distinct action syms (venue byte 0 =
    // Polymarket — raw-id style, like the §4.3 universe's raw ids):
    //   syms 1..=128   level_breach Bid @ 0.50, $1 cap, 10 ms horizon
    //   syms 129..=256 cross_deviation both vs REF_SYM, 80 bps, $1
    // Special rows for branch coverage (validator-illegal, hand-built
    // on purpose — the emit-time layer must stand alone):
    //   sym 1  carries a $500 cap ⇒ policy-clamps to $100
    //   sym 128 is Ask-side with a 1-micro-$ cap and ticks at a $2
    //   mid ⇒ fires (bid ≥ level) but qty floors to zero
    let mut table = Box::new(core_types::RuleTableV2::EMPTY);
    for k in 0..128u32 {
        let risk = if k == 0 {
            500_000_000 // policy re-clamp branch
        } else if k == 127 {
            1 // qty-floor clamp-to-zero branch
        } else {
            1_000_000
        };
        let side = if k == 127 { 1 } else { 0 }; // Ask / Bid
        table.rows[k as usize] = core_types::RuleRowV2::from_v1(&core_types::RuleRow::new(
            k + 1,
            core_types::SYMBOL_ID_NONE,
            0,
            10,
            500_000,
            risk,
            k as u64,
            core_types::RuleRow::TRIGGER_LEVEL_BREACH,
            side,
            0,
        ));
    }
    for k in 0..128u32 {
        table.rows[(128 + k) as usize] = core_types::RuleRowV2::from_v1(&core_types::RuleRow::new(
            129 + k,
            REF_SYM,
            80,
            10,
            0,
            1_000_000,
            (128 + k) as u64,
            core_types::RuleRow::TRIGGER_CROSS_DEVIATION,
            core_types::RuleRow::SIDE_BOTH,
            0,
        ));
    }
    table.len = core_types::RULE_TABLE_ROWS as u32;
    table.epoch = 1;
    table.hash128 = [0x36; 16];

    let ring: std::sync::Arc<Ring<core_types::Order, 1024>> = Ring::new();
    let (prod, mut cons) = ring.split();
    let mut vm: Box<VmStrategy> = Box::new(VmStrategy::new());
    let mut ctx = RingCtx {
        prod,
        now: 1_000_000_000,
    };
    vm.on_start(&mut ctx).unwrap();
    vm.receive_table_v2(&table);
    let commit = core_types::AiCmd::new(
        1,
        1,
        core_types::SYMBOL_ID_NONE,
        i64::from_le_bytes([0x36; 8]),
        i64::from_le_bytes([0x36; 8]),
        0,
        core_types::AiCmdKind::RulesetCommit,
        VenueId::Ai,
        core_types::STRATEGY_SLOT_VM,
        core_types::AI_SIDE_NONE,
        0,
        0,
    );
    vm.on_ai(&commit, &mut ctx);
    assert_eq!(vm.rows_active(), 256);

    // One storm iteration = one tick. Phase cycle: 256 action syms,
    // then the ref leg, then an irrelevant sym (relevance-scan miss).
    fn storm_tick(i: u32) -> Tick {
        let phase = i % 258;
        let (sym, bid, ask) = if phase < 128 {
            // ask 0.49 ≤ level 0.50 ⇒ Bid fire. Sym 128 (Ask row):
            // bid $1.99 ≥ level ⇒ fires, but the $2 mid floors the
            // 1-micro-cap qty to zero — the clamp-to-zero branch.
            if phase == 127 {
                (phase + 1, 1_990_000, 2_010_000)
            } else {
                (phase + 1, 470_000, 490_000)
            }
        } else if phase < 256 {
            // mid 0.70 vs ref mid 0.50 ⇒ 4_000 bps ≥ 80 ⇒ Ask fire.
            (phase + 1, 690_000, 710_000)
        } else if phase == 256 {
            (REF_SYM, 490_000, 510_000)
        } else {
            (IRRELEVANT_SYM, 400_000, 420_000)
        };
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            i + 1, // globally increasing ⇒ per-sym increasing
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    // Prewarm: one full cycle tracks every book slot and exercises
    // every branch once before the measurement window.
    let mut i = 0u32;
    while i < 258 {
        vm.on_tick(&storm_tick(i), &mut ctx);
        ctx.now += 1_000_000; // 1 ms per tick ⇒ 10 ms horizons re-arm
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let warm_fires = vm.fires;
    let warm_emitted = vm.orders_emitted;
    assert!(
        warm_fires > 0 && warm_emitted > 0,
        "prewarm must exercise the emit path"
    );

    let g = AllocGuard::new();
    while i < 258 + 10_000 {
        vm.on_tick(&storm_tick(i), &mut ctx);
        ctx.now += 1_000_000;
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert!(vm.fires > warm_fires, "storm must keep firing");
    assert!(vm.orders_emitted > warm_emitted, "storm must keep emitting");
    assert!(
        vm.fires > vm.orders_emitted + vm.orders_dropped,
        "the clamp-to-zero row must fire without emitting"
    );
    assert_eq!(vm.feats.sym_slots_exhausted, 0);
    assert_eq!(
        allocs, 0,
        "vm on_tick steady state allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "vm on_tick bytes should be zero: saw {bytes}");
}

#[test]
fn vm_feature_engine_paths_are_zero_alloc() {
    // VM2 V2 gate (39): every feature-engine ingest + read path is
    // 0 B/op after boot — wall-live tick minute-sampling, the
    // per-venue funding print laws (advance + hourly sample), seed
    // dedup, depth derivation, opt-summary latch, and every FeatId
    // read (lazy rolling recompute + APR recompute included).
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_vm::VmStrategy;

    struct SinkCtx {
        now: u64,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, _o: core_types::Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> core_time::NsTs {
            self.now
        }
    }

    const MONO0: u64 = 100_000_000_000_000_000;
    const WALL0: u64 = 1_787_961_600_000; // a UTC midnight, ms

    let okx_sym = core_types::make_symbol_id(VenueId::Okx, 11);
    let dbt_sym = core_types::make_symbol_id(VenueId::Deribit, 12);
    let hl_sym = core_types::make_symbol_id(VenueId::Hyperliquid, 13);
    let bn_sym = core_types::make_symbol_id(VenueId::Binance, 14);

    let mut vm: Box<VmStrategy> = Box::new(VmStrategy::new());
    let mut ctx = SinkCtx { now: MONO0 };
    vm.on_start(&mut ctx).unwrap();

    // Boot-time roll bindings (table-commit-time in production).
    assert!(vm.feats.bind_roll(okx_sym, 10));
    assert!(vm.feats.bind_roll(okx_sym, 60));

    let mono_at = |wall_ms: u64| MONO0 + (wall_ms - WALL0) * 1_000_000;

    let mk_tick = |sym: u32, px: i64, seq: u32, ts: u64| {
        Tick::new(
            ts,
            VenueId::Okx,
            sym,
            seq,
            Price::from_raw(px - 5_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(px + 5_000),
            Qty::from_raw(1_000_000),
        )
    };
    let funding_ev = |sym: u32, wall: u64, rate: i64, next_ms: i64, venue: VenueId| {
        core_types::ChannelEvent::new(
            mono_at(wall),
            venue,
            core_types::ChannelId::Funding,
            sym,
            0,
            wall,
            rate,
            next_ms,
        )
    };
    let ctx_ev = |sym: u32, rate: i64| {
        core_types::ChannelEvent::new(
            0,
            VenueId::Hyperliquid,
            core_types::ChannelId::AssetCtx,
            sym,
            0,
            0,
            rate,
            5,
        )
    };
    let mk_depth = |sym: u32, ts: u64| {
        let mut bids = [core_types::DepthLevel::EMPTY; core_types::DEPTH_K];
        let mut asks = [core_types::DepthLevel::EMPTY; core_types::DEPTH_K];
        bids[0] = core_types::DepthLevel {
            px_1e6: 100_000_000,
            qty_1e6: 3_000_000,
        };
        asks[0] = core_types::DepthLevel {
            px_1e6: 100_500_000,
            qty_1e6: 1_000_000,
        };
        core_types::DepthTopK::new(ts, VenueId::Okx, sym, 0, bids, asks)
    };
    let mk_opt = |sym: u32, ts: u64| {
        core_types::OptSummary::new(
            ts,
            VenueId::Deribit,
            sym,
            core_types::OPT_SUMMARY_FLAG_MARK_PX,
            41_500_000,
            700_000_000,
            65_000_000_000_000,
            0,
            -400_000_000,
            2,
            3,
            -5,
        )
    };
    let seed_cmd = |sym: u32, ts_ms: i64, rate: i64| {
        core_types::AiCmd::new(
            1,
            1,
            sym,
            rate,
            ts_ms,
            0,
            core_types::AiCmdKind::FundingSeed,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_VM,
            core_types::AI_SIDE_NONE,
            0,
            0,
        )
    };

    // Prewarm: teach the wall, claim every slot, exercise every
    // branch (block claims allocate NOTHING — pools are inside the
    // boot Box — but the first pass exists to mirror the storm).
    let all_feats = [
        core_types::FeatId::Mid,
        core_types::FeatId::Bid,
        core_types::FeatId::Ask,
        core_types::FeatId::RollMean,
        core_types::FeatId::RollEma,
        core_types::FeatId::RollMin,
        core_types::FeatId::RollMax,
        core_types::FeatId::RollStd,
        core_types::FeatId::Apr24,
        core_types::FeatId::Apr72,
        core_types::FeatId::MarkPx,
        core_types::FeatId::MarkIv,
        core_types::FeatId::DepthImb,
        core_types::FeatId::DepthSpreadBps,
        core_types::FeatId::DepthNearNotional,
        core_types::FeatId::ClockToFunding,
        core_types::FeatId::ClockUtcSod,
    ];
    let mut pass = 0u64;
    let mut run_storm = |vm: &mut Box<VmStrategy>, ctx: &mut SinkCtx, iters: u64| {
        let mut k = 0u64;
        while k < iters {
            let wall = WALL0 + pass * 60_000; // one minute per pass
            let now = mono_at(wall);
            ctx.now = now;
            // Ticks (minute sampling on okx_sym's two bound rings).
            let mut t = mk_tick(okx_sym, 100_000_000 + pass as i64, pass as u32 + 1, now);
            t.ts_ns = now;
            vm.on_tick(&t, ctx);
            // OKX advance law: next-funding steps forward every 3
            // passes ⇒ settled prints keep recording.
            vm.on_venue_event(
                &funding_ev(
                    okx_sym,
                    wall,
                    100_000_000 + pass as i64,
                    (WALL0 + ((pass / 3) + 1) * 8 * 3_600_000) as i64,
                    VenueId::Okx,
                ),
                ctx,
            );
            // Deribit hourly sample (v1 = funding_8h).
            vm.on_venue_event(
                &funding_ev(dbt_sym, wall, 7_000_000, 16_000_000, VenueId::Deribit),
                ctx,
            );
            // HL ctx sample (wall-hour law).
            vm.on_venue_event(&ctx_ev(hl_sym, 12_500), ctx);
            // Seeds: alternating fresh/duplicate (dedup scan path).
            let seed_ts = (WALL0 as i64) - 8 * 3_600_000 * ((pass as i64 % 4) + 1);
            vm.on_ai(&seed_cmd(bn_sym, seed_ts, 50_000_000), ctx);
            // Depth + opt.
            vm.on_depth(&mk_depth(okx_sym, now), ctx);
            vm.on_opt_summary(&mk_opt(dbt_sym, now), ctx);
            // Reads: every feature on its natural sym.
            let mut f = 0;
            while f < all_feats.len() {
                let feat = all_feats[f];
                let sym = if feat.requires_opt_summary() {
                    dbt_sym
                } else if feat == core_types::FeatId::Apr24 || feat == core_types::FeatId::Apr72 {
                    bn_sym
                } else {
                    okx_sym
                };
                let win = if feat.requires_window() {
                    if f % 2 == 0 {
                        10
                    } else {
                        60
                    }
                } else {
                    0
                };
                std::hint::black_box(vm.feats.read(feat, sym, win, now));
                f += 1;
            }
            pass += 1;
            k += 1;
        }
    };

    run_storm(&mut vm, &mut ctx, 200);
    assert!(vm.feats.prints_recorded > 0, "prewarm recorded prints");
    assert!(vm.feats.seeds_deduped > 0, "prewarm hit the dedup path");
    assert_eq!(vm.feats.sym_slots_exhausted, 0);

    let g = AllocGuard::new();
    run_storm(&mut vm, &mut ctx, 2_000);
    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        vm.feats.prints_recorded >= 200,
        "storm kept recording prints"
    );
    assert_eq!(
        allocs, 0,
        "feature-engine steady state allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "feature-engine bytes should be zero: saw {bytes}");
}

/// ICDP I3 gate (40): the icdp strategy's whole tick path (slot 6 until
/// XMM XH1 unlinked it; the crate and its offline arm stay) — foreign
/// syms, in-bar feature updates, stale ticks, the decision (features +
/// composite + IoC entry), the bar roll (IoC exit), the 256-tick sweep
/// — is 0 B/op after `configure`. Eight instruments (the D4 v1 count),
/// 15 s bars, δ 25 %, with a threshold every bar clears.
#[test]
fn icdp_on_tick_decision_and_roll_are_zero_alloc() {
    use core_time::WallAnchor;
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_icdp::{IcdpParams, IcdpStrategy, IcdpSymParams, ICDP_NF, SCALE_1E9};

    struct RingCtx {
        prod: core_ring::Producer<core_types::Order, 1024>,
    }
    impl Ctx for RingCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            if self.prod.try_push_ref(&order) {
                Ok(())
            } else {
                Err(SubmitErr::RingFull)
            }
        }
        fn now_ns(&self) -> core_time::NsTs {
            0
        }
    }

    const MS: u64 = 1_000_000;
    const TF: u64 = 15_000 * MS;
    const N: usize = 8;
    let mut params = IcdpParams::EMPTY;
    params.tf_ns = TF;
    params.delta_ns = 3_750 * MS;
    params.n = N;
    let mut k = 0usize;
    while k < N {
        params.syms[k] = IcdpSymParams {
            sym: core_types::make_symbol_id(VenueId::Okx, k as u32 + 1),
            mu: [0; ICDP_NF],
            inv_sd: [SCALE_1E9; ICDP_NF],
            w: [SCALE_1E9, 0, 0, 0, 0],
            b: 0,
            thr: SCALE_1E9 / 10, // 0.1 bps: fires on the +2 bps script below
            notional_1e6: 1_000_000_000,
            spread_cap_1e9: 5 * SCALE_1E9,
            entry_slip_1e9: SCALE_1E9,
            exit_slip_1e9: SCALE_1E9,
        };
        k += 1;
    }
    let ring: std::sync::Arc<Ring<core_types::Order, 1024>> = Ring::new();
    let (prod, mut cons) = ring.split();
    let mut s: Box<IcdpStrategy> = Box::new(IcdpStrategy::new());
    let anchor = WallAnchor::new(1_000_000_000_000, 1_788_400_000_000_000_000);
    s.configure(anchor, &params).unwrap();
    let mut ctx = RingCtx { prod };
    s.on_start(&mut ctx).unwrap();
    let foreign = core_types::make_symbol_id(VenueId::Deribit, 77);

    // One bar per 8 syms = 40 ticks: 4 in-bar quotes per sym (one of
    // them stale every third bar), the decision tick at δ with a +2 bps
    // move, then 8 foreign ticks. Bars roll on the next bar's first tick.
    fn script_tick(i: u32, t0: u64, foreign: u32) -> Tick {
        let bar = i / 40;
        let j = i % 40;
        let open = t0 + bar as u64 * TF;
        let (sym_i, phase) = (j % 8, j / 8);
        let sym = core_types::make_symbol_id(VenueId::Okx, sym_i + 1);
        // The bar opens on the previous bar's last quote, so the
        // decision quote must MOVE bar to bar: ±4 bps alternating.
        let dec_bid = if bar % 2 == 0 {
            100_040_000
        } else {
            99_960_000
        };
        let (sym, ts, bid, flags) = match phase {
            0 => (sym, open + 10 * MS, 100_000_000, 0),
            1 => (
                sym,
                open + 1_000 * MS,
                100_001_000,
                if bar % 3 == 2 {
                    core_types::TICK_FLAG_STALE
                } else {
                    0
                },
            ),
            2 => (sym, open + 2_000 * MS, 100_002_000, 0),
            3 => (sym, open + 3_750 * MS, dec_bid, 0),
            _ => (foreign, open + 5_000 * MS, 1_000_000, 0),
        };
        Tick::new_stamped(
            ts,
            VenueId::Okx,
            sym,
            i + 1,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000 + (i % 7) as i64 * 100_000),
            Price::from_raw(bid + 10_000),
            Qty::from_raw(1_000_000 + (i % 5) as i64 * 100_000),
            0,
            flags,
        )
    }
    let t0 = s.clock().open_mono(s.clock().bar_id(anchor.mono_ns) + 1);
    // Prewarm: 3 bars (every branch: fresh, stale, decision, roll, sweep).
    let mut i = 0u32;
    while i < 120 {
        s.on_tick(&script_tick(i, t0, foreign), &mut ctx);
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let warm = s.counters().intents;
    assert!(warm > 0, "prewarm must enter");
    let g = AllocGuard::new();
    while i < 120 + 40 * 300 {
        s.on_tick(&script_tick(i, t0, foreign), &mut ctx);
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    let k = s.counters();
    assert!(k.intents > warm && k.exits > 0 && k.skipped_stale_dec > 0 && k.rolls > 0);
    assert_eq!(
        allocs, 0,
        "icdp on_tick allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "icdp on_tick bytes should be zero: saw {bytes}");
}

/// RG3 gate 42 (`docs/regime-and-dashboard-plan.md` §7): the vm's row
/// regime gate — a full 256-row table of LABELLED rows (bull / bear /
/// `rel:` variants over one signal, hard and soft off) under a tick
/// storm interleaved with view changes (`set_regime_view` re-judging
/// every row, REL probes included) — allocates nothing after the
/// commit. Covers the blocked-entry branch, the open-entry emit, the
/// hard-exit flatten of position rows and the re-judge itself.
#[test]
fn vm_regime_gate_and_view_rejudge_are_zero_alloc() {
    use core_regime::RegimeView;
    use core_types::regime::{
        RegimeLabelBuilder, FUND_POS, LEVEL_NORMAL, REL_LAGGING, REL_LEADING, SHAPE_TREND,
        SOURCE_MEASURED, STRETCH_NEUTRAL, TREND_BEAR, TREND_BULL, VOL_NORMAL,
    };
    use core_types::{
        CombineOp, FeatId, RegimeTerm, RegimeWord, RuleRow, RuleRowV2, FEAT_NONE, GROUP_NONE,
        REGIME_OFF_HARD, REGIME_OFF_SOFT, ROW_FLAG_POSITION,
    };
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_vm::VmStrategy;

    struct RingCtx {
        prod: core_ring::Producer<core_types::Order, 4096>,
        now: u64,
    }
    impl Ctx for RingCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            if self.prod.try_push_ref(&order) {
                Ok(())
            } else {
                Err(SubmitErr::RingFull)
            }
        }
        fn now_ns(&self) -> core_time::NsTs {
            self.now
        }
    }

    const REF_SYM: u32 = 1_000;
    let term = |strs: &[&str]| -> RegimeTerm {
        let mut b = RegimeLabelBuilder::new();
        for s in strs {
            b.add(s.as_bytes()).expect("term");
        }
        b.finish()
    };
    let bull = term(&["trend:bull"]);
    let bear = term(&["trend:bear"]);
    let lag = term(&["rel:lagging"]);

    // 256 rows: syms 1..=128 carry refire level-breach rows labelled
    // bull / bear / rel-lagging (round robin); syms 129..=256 carry
    // POSITION cross-deviation rows vs REF_SYM labelled bull (hard) /
    // bear (soft) — the flatten path runs when the view turns.
    let mut table = Box::new(core_types::RuleTableV2::EMPTY);
    for k in 0..128u32 {
        let base = RuleRowV2::from_v1(&RuleRow::new(
            k + 1,
            core_types::SYMBOL_ID_NONE,
            0,
            10,
            500_000,
            1_000_000,
            k as u64,
            RuleRow::TRIGGER_LEVEL_BREACH,
            0,
            0,
        ));
        let t = match k % 3 {
            0 => bull,
            1 => bear,
            _ => lag,
        };
        table.rows[k as usize] = base.with_regime(t, REGIME_OFF_SOFT);
    }
    for k in 0..128u32 {
        let row = RuleRowV2::new(
            ROW_FLAG_POSITION,
            RuleRow::SIDE_BOTH,
            GROUP_NONE,
            FeatId::Mid,
            FeatId::Mid,
            FEAT_NONE,
            CombineOp::DiffBps,
            129 + k,
            REF_SYM,
            0,
            0,
            0,
            core_types::CMP_ENTRY_ABS,
            400_000_000_000,
            100_000_000_000,
            0,
            0,
            10,
            0,
            1_000_000,
            (128 + k) as u64,
            0,
            0,
        );
        let (t, off) = if k % 2 == 0 {
            (bull, REGIME_OFF_HARD)
        } else {
            (bear, REGIME_OFF_SOFT)
        };
        table.rows[(128 + k) as usize] = row.with_regime(t, off);
    }
    table.len = core_types::RULE_TABLE_ROWS as u32;
    table.epoch = 1;
    table.hash128 = [0x42; 16];

    let ring: std::sync::Arc<Ring<core_types::Order, 4096>> = Ring::new();
    let (prod, mut cons) = ring.split();
    let mut vm: Box<VmStrategy> = Box::new(VmStrategy::new());
    let mut ctx = RingCtx {
        prod,
        now: 1_000_000_000,
    };
    vm.on_start(&mut ctx).unwrap();
    vm.receive_table_v2(&table);
    let commit = core_types::AiCmd::new(
        1,
        1,
        core_types::SYMBOL_ID_NONE,
        i64::from_le_bytes([0x42; 8]),
        i64::from_le_bytes([0x42; 8]),
        0,
        core_types::AiCmdKind::RulesetCommit,
        VenueId::Ai,
        core_types::STRATEGY_SLOT_VM,
        core_types::AI_SIDE_NONE,
        0,
        0,
    );
    vm.on_ai(&commit, &mut ctx);
    assert_eq!(vm.rows_active(), 256);

    // Two views: bull with every sym LAGGING, bear with every sym
    // LEADING — the storm alternates them so both variants open and
    // close, and the REL probe walks all 32 slots.
    let word = |trend: u8| {
        RegimeWord::from_values(
            trend,
            SHAPE_TREND,
            VOL_NORMAL,
            FUND_POS,
            LEVEL_NORMAL,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        )
    };
    let view = |trend: u8, rel: u8| {
        let mut v = RegimeView::UNKNOWN;
        v.configured = 1;
        v.effective[0] = word(trend);
        v.effective[1] = word(trend);
        v.n_syms = 32;
        let mut s = 0usize;
        while s < 32 {
            // slot 0 = the "ref" (never REL-judged); slots 1..32 = syms
            // 1..=31 — the rest of the table's syms are non-members
            // (REL unknown ⇒ their `rel:` rows stay closed: fail-closed).
            v.syms[s] = if s == 0 { REF_SYM } else { s as u32 };
            v.rel[0][s] = rel;
            v.rel[1][s] = rel;
            s += 1;
        }
        v
    };
    let views = [view(TREND_BULL, REL_LAGGING), view(TREND_BEAR, REL_LEADING)];

    fn storm_tick(i: u32) -> Tick {
        let phase = i % 258;
        let (sym, bid, ask) = if phase < 128 {
            (phase + 1, 470_000, 490_000) // ask ≤ 0.50 ⇒ Bid fire
        } else if phase < 256 {
            (phase + 1, 690_000, 710_000) // +4000 bps vs ref ⇒ enter
        } else if phase == 256 {
            (REF_SYM, 490_000, 510_000)
        } else {
            (5_000, 400_000, 420_000) // irrelevant sym
        };
        Tick::new(
            0,
            VenueId::Polymarket,
            sym,
            i + 1,
            Price::from_raw(bid),
            Qty::from_raw(1_000_000),
            Price::from_raw(ask),
            Qty::from_raw(1_000_000),
        )
    }

    // Prewarm: two full cycles under both views.
    let mut i = 0u32;
    while i < 2 * 258 {
        if i % 258 == 0 {
            vm.set_regime_view(&views[((i / 258) % 2) as usize]);
        }
        vm.on_tick(&storm_tick(i), &mut ctx);
        ctx.now += 1_000_000;
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let warm_blocked = vm.regime_blocked;
    let warm_hard = vm.regime_hard_exits;
    let warm_emitted = vm.orders_emitted;
    assert!(
        warm_blocked > 0 && warm_emitted > 0,
        "prewarm must gate and emit"
    );

    let g = AllocGuard::new();
    while i < 2 * 258 + 20 * 258 {
        if i % 258 == 0 {
            // A view change every cycle: re-judge all 256 rows.
            vm.set_regime_view(&views[((i / 258) % 2) as usize]);
        }
        vm.on_tick(&storm_tick(i), &mut ctx);
        ctx.now += 1_000_000;
        while let Some(o) = cons.try_pop_ref().as_deref().copied() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        vm.regime_blocked > warm_blocked,
        "the storm keeps blocking closed rows"
    );
    assert!(
        vm.regime_hard_exits > warm_hard,
        "the view flips keep flattening hard rows"
    );
    assert!(
        vm.orders_emitted > warm_emitted,
        "open variants keep emitting"
    );
    assert_eq!(
        allocs, 0,
        "vm regime gate / re-judge allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "vm regime gate bytes should be zero: saw {bytes}");
}

/// RG1 gate 41 (`docs/regime-and-dashboard-plan.md` §7): the regime
/// evaluator's hot path — `on_tick` for members and non-members, the
/// 1 s timer including several minute rolls (ring write + the full
/// judge pass for both profiles), a declaration and the effective
/// refresh — allocates nothing after `new_boxed` + `configure` + `seed`.
#[test]
fn regime_on_tick_and_minute_roll_are_zero_alloc() {
    use core_regime::{
        ProfileParams, RegimeParams, RegimeState, SeedRow, MINUTE_NS, REGIME_MAX_MEMBERS,
    };
    use core_time::WallAnchor;
    use core_types::RegimeWord;

    const N_MEMBERS: usize = REGIME_MAX_MEMBERS;
    let btc = core_types::make_symbol_id(VenueId::Binance, 900);
    let mut members = [core_types::SYMBOL_ID_NONE; REGIME_MAX_MEMBERS];
    let mut i = 0usize;
    while i < N_MEMBERS {
        members[i] = core_types::make_symbol_id(VenueId::Binance, 901 + i as u32);
        i += 1;
    }
    let foreign = core_types::make_symbol_id(VenueId::Okx, 7);
    let mut fast = ProfileParams::FAST_DEFAULT;
    fast.rv_p30_bps_1e9 = 10_000_000_000;
    fast.rv_p70_bps_1e9 = 100_000_000_000;
    let params = RegimeParams::new(
        btc,
        btc,
        members,
        N_MEMBERS as u8,
        3,
        [fast, ProfileParams::SLOW_DEFAULT],
    );
    // Boot (may allocate): the box, the map, a 300-minute seed.
    const T0: u64 = 1_000_000_000_000;
    let anchor = WallAnchor::new(T0, 1_800_000_000 * 1_000_000_000);
    let mut s = RegimeState::new_boxed();
    s.configure(&params, anchor, T0).expect("params valid");
    let m0 = s.minute();
    let mut rows = Vec::with_capacity(300 * (N_MEMBERS + 1));
    let mut k = 0i64;
    while k < 300 {
        let m = m0 - 300 + k;
        rows.push(SeedRow::new(btc, m, 100_000_000 + k * 20_000));
        let mut j = 0usize;
        while j < N_MEMBERS {
            rows.push(SeedRow::new(
                members[j],
                m,
                50_000_000 + k * 10_000 + j as i64,
            ));
            j += 1;
        }
        k += 1;
    }
    assert_eq!(s.seed(&rows) as usize, rows.len());
    s.on_funding(25_000, 1_700_000_000_000);

    let tick = |sym: SymbolId, ts: u64, mid: i64| {
        Tick::new(
            ts,
            VenueId::Binance,
            sym,
            1,
            Price(mid - 500),
            Qty(1_000_000),
            Price(mid + 500),
            Qty(1_000_000),
        )
    };
    // Prewarm one live minute so every branch has run once.
    let mut ts = T0;
    let mut minute = 0u64;
    while minute < 1 {
        let mut n = 0usize;
        while n < 60 {
            s.on_tick(&tick(btc, ts, 106_000_000 + n as i64));
            s.on_tick(&tick(members[n % N_MEMBERS], ts, 53_000_000 + n as i64));
            s.on_tick(&tick(foreign, ts, 5_000_000));
            ts += 1_000_000_000;
            n += 1;
        }
        std::hint::black_box(s.on_timer(ts + 1_000_000));
        minute += 1;
    }

    let g = AllocGuard::new();
    while minute < 6 {
        let mut n = 0usize;
        while n < 60 {
            s.on_tick(&tick(
                btc,
                ts,
                106_000_000 + (minute as i64) * 20_000 + n as i64,
            ));
            s.on_tick(&tick(members[n % N_MEMBERS], ts, 53_000_000 + n as i64));
            s.on_tick(&tick(foreign, ts, 5_000_000));
            std::hint::black_box(s.on_timer(ts));
            ts += 1_000_000_000;
            n += 1;
        }
        if minute == 3 {
            s.set_declared(0, RegimeWord(1u64 << 2), ts, 5 * MINUTE_NS);
        }
        std::hint::black_box(s.on_timer(ts + 1_000_000));
        std::hint::black_box(s.effective(0));
        std::hint::black_box(s.rel_of(1, members[3]));
        minute += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        s.minutes_judged() >= 6,
        "rolls happened: {}",
        s.minutes_judged()
    );
    assert_eq!(
        allocs, 0,
        "regime hot path allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "regime hot path bytes should be zero: saw {bytes}"
    );
}

/// RG6 gate 43 (`docs/regime-and-dashboard-plan.md` §7): the `/state`
/// path — a FULL `EngineSnapshot` (256 vm rows, 64 + 64 recents, the
/// eight xmm perp rows since XMM XH3, the twelve `har` rows since HAR
/// H3.7, every text field at capacity)
/// published into the seqlock, read back into
/// the server thread's scratch and encoded as JSON into a 256 KiB
/// response buffer, 1 000 times — allocates nothing. Truncation is a
/// test failure (the encoder refuses, never truncates).
#[test]
fn state_snapshot_publish_read_encode_is_zero_alloc() {
    use core_types::{Fill, Order, Price, Qty, Side, VenueId, RULE_TABLE_ROWS};
    use engine_snapshot::{
        encode_state_json, EngineSnapshot, SnapshotCell, RECENT_FILLS, RECENT_ORDERS,
        RUN_DIR_MAX, SNAPSHOT_XMM_PERPS,
    };
    use strategy_core::{
        HarCounters, HarSeriesView, VmRowView, XmmPerpView, HAR_VIEW_NAME_MAX, HAR_VIEW_SERIES,
        HAR_VIEW_TENORS, HAR_VIEW_WEEKDAYS,
    };

    // Boot-time construction (allocation sanctioned): the cell, the
    // engine-side scratch, the server-side scratch, the response buf.
    let cell = SnapshotCell::new(EngineSnapshot::empty());
    let mut scratch = Box::new(EngineSnapshot::empty());
    scratch.boot.set_git_sha(&[b'f'; 48]);
    scratch.boot.set_run_dir(&[b'r'; RUN_DIR_MAX]);
    // XMM XH3: the xmm block at its widest — every coin byte escapes,
    // all eight rows at full width, both `age_ms` arms (one feed never
    // heard).
    scratch.boot.xmm_hash = [0xFF; 32];
    scratch.boot.set_xmm_coins(&[b'"'; 48]);
    scratch.xmm.n_perps = SNAPSHOT_XMM_PERPS as u32;
    for (i, r) in scratch.xmm.perps.iter_mut().enumerate() {
        *r = XmmPerpView {
            pos_1e6: i64::MIN,
            touch_bid_1e6: i64::MIN,
            touch_ask_1e6: i64::MIN,
            bid_px_1e6: i64::MIN,
            ask_px_1e6: i64::MIN,
            lead_rx_ns: if i == 0 { 0 } else { 1 },
            fol_rx_ns: 1,
            hl_sym: u32::MAX,
            lead_sym: u32::MAX,
            bid_state: u8::MAX,
            ask_state: u8::MAX,
            stale_flags: u8::MAX,
            _pad: [0; 5],
        };
    }
    // HAR H3.7: the `har` block at its widest — all twelve rows, every
    // name byte a control byte (six out per byte), every number at its
    // widest, and a wall clock that makes each `day_age_s` a real age
    // (eleven digits) rather than the `-1` of a series that never closed.
    scratch.wall_ns = u64::MAX;
    scratch.har.hash = [0xFF; 32];
    scratch.har.n = HAR_VIEW_SERIES as u32;
    scratch.har.dropped = u32::MAX;
    scratch.har.counters = HarCounters {
        minutes_rolled: u64::MAX,
        closes: u64::MAX,
        day_closes: u64::MAX,
        held: u64::MAX,
        forced: u64::MAX,
        day_close_ns_max: u64::MAX,
        day_close_ns_last: u64::MAX,
        epoch: u64::MAX,
    };
    for r in scratch.har.series.iter_mut() {
        *r = HarSeriesView {
            last_min_ms: u64::MAX,
            newest_day_ms: 1_000_000_000_000,
            gaps: u64::MAX,
            epoch: u64::MAX,
            feed: u32::MAX,
            open_minutes: u32::MAX,
            raw_1e6: [i32::MIN; HAR_VIEW_TENORS],
            fit_1e6: [i32::MIN; HAR_VIEW_TENORS],
            weekday_1e6: [i32::MIN; HAR_VIEW_WEEKDAYS],
            weekday_n: [u8::MAX; HAR_VIEW_WEEKDAYS],
            pairs: [u8::MAX; HAR_VIEW_TENORS],
            name: [0x01; HAR_VIEW_NAME_MAX],
            name_len: HAR_VIEW_NAME_MAX as u8,
            warm: u8::MAX,
            days: u8::MAX,
            empty_days: u8::MAX,
            fitted: u16::MAX,
            fit_beats_raw: u16::MAX,
        };
    }
    scratch.set_strategy_kind(b"set");
    scratch.vm.rows_active = RULE_TABLE_ROWS as u32;
    for (i, r) in scratch.vm.rows.iter_mut().enumerate() {
        *r = VmRowView::new(
            u64::MAX - i as u64,
            1_500_000,
            1,
            2_000_000,
            i as u32,
            u32::MAX,
            1,
            0,
            1,
            1,
            0,
            0,
            1,
        );
    }
    let o = Order::new(
        1,
        VenueId::Okx,
        7,
        Side::Bid,
        0,
        Price::from_raw(1_500_000),
        Qty::from_raw(2_000_000),
        u64::MAX,
    );
    for _ in 0..RECENT_ORDERS {
        scratch.recent_orders.push(o);
    }
    let f = Fill::new(1, 7, Side::Ask, Price::from_raw(1), Qty::from_raw(2), u64::MAX);
    for _ in 0..RECENT_FILLS {
        scratch.recent_fills.push(f);
    }
    // BX11 (the alloc review's AL-3): every venue arm present — the
    // `exec.arms` members and their commas — the Binance block's words at
    // their widest.
    scratch.exec.arms_binance.present = 1;
    scratch.exec.arms_hyperliquid_gw.present = 1;
    scratch.exec.arms_binance_gw = engine_snapshot::BinanceGwSnapshot {
        present: 1,
        slot: u8::MAX,
        account_mode: u8::MAX,
        sweep: u8::MAX,
        refused: [u32::MAX; 21],
        halt_reason: u64::MAX,
        in_doubt: u64::MAX,
        events_other: u64::MAX,
        heartbeat_age_ms: i64::MAX as u64,
        post_rtt_p99_ns: u64::MAX,
        clock_offset_ms: i64::MIN,
        margin_usdm_1e6: i64::MIN,
        margin_coinm_1e6: i64::MIN,
        margin_options_1e6: i64::MIN,
        opt_weight_used: u64::MAX,
        // BX13: spot's pool, spot's day and the held cycles, at their widest.
        spot_weight_used: u64::MAX,
        spot_weight_max: u64::MAX,
        spot_day_unfilled: u64::MAX,
        spot_day_cap: u64::MAX,
        spot_sell_cap: u64::MAX,
        spot_day_seeded: u64::MAX,
        recon_held: u64::MAX,
        ..engine_snapshot::BinanceGwSnapshot::default()
    };
    let mut server_scratch = Box::new(EngineSnapshot::empty());
    let mut resp = vec![0u8; 256 * 1024];

    let g = AllocGuard::new();
    let mut acc: usize = 0;
    for i in 0..1_000u64 {
        scratch.seq = i;
        scratch.mono_ns = i * 1_000_000_000;
        cell.publish(&scratch);
        cell.read_into(&mut server_scratch);
        let n = encode_state_json(&server_scratch, &mut resp).expect("full body fits 256 KiB");
        acc = acc.wrapping_add(n);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc > 0);
    let n = encode_state_json(&server_scratch, &mut resp).expect("fits");
    let body = core::str::from_utf8(&resp[..n]).expect("utf-8");
    assert_eq!(body.matches("\"touch_bid_1e6\":").count(), SNAPSHOT_XMM_PERPS);
    assert!(body.contains("\"lead_age_ms\":-1"), "the never-heard arm ran");
    assert_eq!(body.matches("\"weekday_n\":").count(), HAR_VIEW_SERIES, "every har row encoded");
    assert_eq!(
        body.matches("\\u0001").count(),
        HAR_VIEW_SERIES * HAR_VIEW_NAME_MAX,
        "every har name byte escaped"
    );
    assert!(body.contains("\"day_age_s\":17446657673"), "a real age, not -1");
    for arm in ["\"binance\":{", "\"binance_gw\":{", "\"hyperliquid_gw\":{"] {
        assert!(body.contains(arm), "gate 43: {arm} encoded");
    }
    assert_eq!(
        allocs, 0,
        "/state publish+read+encode allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "/state path bytes should be zero: saw {bytes}");
}

/// VRP V1 gate: the option registry's HOT lookups.
///
/// `get` / `is_option` run inside `on_opt_summary`, which fires on every
/// option ticker push (measured: ~480 k Deribit records per 2 h window
/// across a 64-instrument chain), so they are on the hot path by volume
/// even though each call is trivial. Population is boot-only and stays
/// OUTSIDE the guard, exactly as the icdp gate keeps configure+prewarm
/// outside its own.
///
/// The measurement deliberately includes MISSES as well as hits — the
/// perp hedge leg, the static instruments, and another venue's
/// identically-ordinalled symbols all reach `is_option` in the member's
/// real callback, and the miss path must not allocate either.
#[test]
fn opt_registry_lookups_are_zero_alloc() {
    use opt_registry::{OptInstrument, OptRegistry, RIGHT_CALL, RIGHT_PUT};

    // The live shape (VRP V0(d), 2026-09-09): a Deribit block of 64
    // options at ordinals 513..=576 over a perp at ordinal 1.
    const OPT_BASE: u32 = 512;
    const N: u32 = 64;
    let perp = core_types::make_symbol_id(VenueId::Deribit, 1);

    // Boot: may allocate (it does not, but that is not what is measured).
    let mut reg = Box::new(OptRegistry::new());
    let mut syms = Vec::with_capacity(N as usize);
    for k in 0..N {
        let sym = core_types::make_symbol_id(VenueId::Deribit, OPT_BASE + 1 + k);
        syms.push(sym);
        reg.insert(OptInstrument::new(
            sym,
            perp,
            VenueId::Deribit as u8,
            1_789_027_200_000_000_000,
            (78_000 + 500 * (k as i64 / 2)) * 1_000_000,
            if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
            1_000_000_000,
        ))
        .expect("boot insert");
    }
    assert_eq!(reg.len(), N as usize);

    // Misses the member really sees: the hedge leg, a static, an
    // out-of-block ordinal, and another venue at the same ordinal.
    let misses = [
        perp,
        core_types::make_symbol_id(VenueId::Deribit, 9),
        core_types::make_symbol_id(VenueId::Deribit, OPT_BASE + 1 + N),
        core_types::make_symbol_id(VenueId::Okx, OPT_BASE + 1),
    ];

    let g = AllocGuard::new();
    let mut hits: u64 = 0;
    let mut strike_acc: i64 = 0;
    let mut miss_acc: u64 = 0;
    for _ in 0..1_000u32 {
        let mut i = 0usize;
        while i < syms.len() {
            let sym = syms[i];
            if let Some(row) = reg.get(sym) {
                strike_acc = strike_acc.wrapping_add(row.strike_1e6);
                hits += 1;
            }
            if reg.is_option(sym) {
                hits += 1;
            }
            i += 1;
        }
        let mut j = 0usize;
        while j < misses.len() {
            if reg.get(misses[j]).is_none() {
                miss_acc += 1;
            }
            if !reg.is_option(misses[j]) {
                miss_acc += 1;
            }
            j += 1;
        }
    }
    std::hint::black_box((hits, strike_acc, miss_acc));

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(hits, 2 * 1_000 * N as u64);
    assert_eq!(miss_acc, 2 * 1_000 * misses.len() as u64);
    assert_eq!(
        allocs, 0,
        "opt-registry lookups allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "opt-registry lookup bytes should be zero: saw {bytes}");
}

/// VRP V4: the forecast's two hot entries — the per-minute fold and the
/// per-expiry bound query — allocate nothing.
///
/// `on_minute_close` runs once a minute for the life of the process and
/// `bounds` runs at every entry decision; both live inside the engine's
/// single-threaded loop, so an allocation in either is a page fault and
/// a lock the strategy loop cannot afford. The engine is boxed because
/// its inline rings are ~16 KiB — construction may allocate (it does,
/// once, for the box), which is exactly why the guard opens after it.
#[test]
fn vol_engine_minute_and_bounds_are_zero_alloc() {
    const TAU_8H: u64 = 28_800_000_000_000;
    const THETA: i64 = 100_000_000;

    let mut e = Box::new(core_vol::VolEngine::new());
    // Boot: warm the ring past a full wrap, then seed a fit. Neither is
    // measured — the seed replay is a boot path (V5).
    let mut px = 79_000_000_000i64;
    let mut s = 20_260_910i64;
    let mut i = 0usize;
    while i < 2_000 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        e.on_minute_close(px);
        i += 1;
    }
    let x0 = e.x_1e9(TAU_8H).expect("warm ring");
    let mut k = 0usize;
    while k < core_vol::MIN_PAIRS {
        e.seed_pair(x0 + k as i64 * 31_000_000, x0 + k as i64 * 26_000_000);
        k += 1;
    }
    assert!(e.fit().is_some(), "the gate must measure a FITTED engine");

    let g = AllocGuard::new();
    let mut lo_acc: i64 = 0;
    let mut hi_acc: i64 = 0;
    let mut n = 0usize;
    while n < 5_000 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        e.on_minute_close(px);
        // The decision path: bounds, then the two i64 compares the
        // member actually makes against a quoted IV.
        if let Some((lo, hi)) = e.bounds(TAU_8H, THETA) {
            lo_acc = lo_acc.wrapping_add(lo);
            hi_acc = hi_acc.wrapping_add(hi);
        }
        n += 1;
    }
    // And the kill tell, which the member reads every decision.
    let q = e.qlike_counters();
    std::hint::black_box((lo_acc, hi_acc, q));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(lo_acc != 0 && hi_acc != 0, "the gate must measure real work");
    assert_eq!(
        allocs, 0,
        "core-vol minute/bounds allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "core-vol hot bytes should be zero: saw {bytes}");
}

/// VRP V6: the member's two live callbacks allocate nothing.
///
/// `on_tick` runs on every underlying quote for the life of the process
/// and `on_opt_summary` on every option ticker frame of the selected
/// chain; both sit inside the engine's single-threaded loop. The member
/// is boxed because its inline forecast rings and option table are ~24
/// KiB — `configure` may allocate, which is exactly why the guard opens
/// after it.
/// X1 gate: the engine-side paper matcher.
///
/// It runs on EVERY tick of every sym the engine receives — the hottest
/// place a new allocation could hide — and it holds two fixed arrays and
/// nothing else. Drives all four verdict kinds, the TTL sweep, both cap
/// refusals and the unroutable path.
#[test]
fn paper_matcher_submit_observe_pump_is_zero_alloc() {
    use clob_dispatcher::PaperMatcher;
    use core_types::{make_symbol_id, Order, Price, Qty, Side, Tick};

    let sym = make_symbol_id(VenueId::Deribit, 1);
    let mut m = Box::new(PaperMatcher::new());

    let mk_order = |kind: u8, side: Side, px: i64, qty: i64, oid: u64, ttl: u64, ts: u64| {
        let mut o = Order::new(
            ts,
            VenueId::Deribit,
            sym,
            side,
            kind,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.ttl_ns = ttl;
        o.strategy_id = 1;
        o
    };
    let mk_tick = |bid: i64, ask: i64, qty: i64| {
        Tick::new(
            0,
            VenueId::Deribit,
            sym,
            0,
            Price::from_raw(bid),
            Qty::from_raw(qty),
            Price::from_raw(ask),
            Qty::from_raw(qty),
        )
    };

    let g = AllocGuard::new();
    let mut oid = 1u64;
    let mut i = 0usize;
    let mut fills = 0u64;
    while i < 10_000 {
        let ts = 1_000_000_000 * i as u64;
        // Four kinds of order, so every verdict arm is exercised: a
        // marketable IoC, a doomed mid-priced IoC (the F7 shape), a
        // maker that rests, and one with a TTL that will expire.
        m.submit(&mk_order(1, Side::Bid, 101_000_000, 600_000, oid, 0, ts), ts);
        oid += 1;
        m.submit(&mk_order(1, Side::Bid, 100_000_000, 600_000, oid, 0, ts), ts);
        oid += 1;
        m.submit(&mk_order(0, Side::Ask, 200_000_000, 600_000, oid, 0, ts), ts);
        oid += 1;
        m.submit(&mk_order(1, Side::Ask, 1_000_000, 600_000, oid, 1_000, ts), ts);
        oid += 1;
        // Every fifth pass, overrun both caps and the unroutable path.
        if i % 5 == 0 {
            let mut k = 0usize;
            while k < 12 {
                m.submit(&mk_order(0, Side::Ask, 500_000_000, 100_000, oid, 0, ts), ts);
                oid += 1;
                k += 1;
            }
            m.submit(&mk_order(2, Side::Bid, 1_000_000, 1_000, oid, 0, ts), ts);
            oid += 1;
            m.submit(&mk_order(1, Side::Bid, 0, 1_000, oid, 0, ts), ts);
            oid += 1;
        }
        // Judge them, twice — the second pass sweeps the TTL.
        m.observe_tick(&mk_tick(99_000_000, 101_000_000, 1_000_000), ts + 300_000_000);
        m.observe_tick(&mk_tick(99_000_000, 100_000_000, 1_000_000), ts + 900_000_000);
        while m.try_next_fill().is_some() {
            fills += 1;
        }
        i += 1;
    }
    let c = m.counters;
    std::hint::black_box((fills, c));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(c.fills > 0, "the gate must measure real fills");
    assert!(c.ioc_canceled > 0, "and real cancels (the F7 path)");
    assert!(c.ttl_expired > 0, "and the TTL sweep");
    assert!(c.rejected_open_cap > 0, "and a cap refusal");
    assert!(c.unroutable > 0, "and an unmodellable order");
    assert_eq!(c.out_overflow, 0, "the out ring must never overflow");
    assert_eq!(
        allocs, 0,
        "paper matcher allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "paper matcher hot bytes should be zero: saw {bytes}");
}

#[test]
fn vrp_member_tick_and_opt_summary_are_zero_alloc() {
    use core_types::{make_symbol_id, OptSummary, Price, Qty, Tick, OPT_SUMMARY_FLAG_MARK_PX};
    use opt_registry::{OptInstrument, RIGHT_CALL, RIGHT_PUT};
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    const MONO0: u64 = 3_191_000_000_000_000;
    const EXPIRY: u64 = 1_789_027_200_000_000_000;
    const WALL0: u64 = EXPIRY - 172_800_000_000_000;
    const MINUTE_NS: u64 = 60_000_000_000;

    struct SinkCtx {
        n: u64,
        now: u64,
        /// X1: the last order submitted, so the gate can feed its fill
        /// straight back — `on_fill` is a hot path now.
        last: Option<core_types::Order>,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            self.n += 1;
            self.last = Some(order);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let perp = make_symbol_id(VenueId::Deribit, 1);
    let mut reg = opt_registry::OptRegistry::new();
    let mut k = 0u32;
    while k < 16 {
        reg.insert(OptInstrument::new(
            make_symbol_id(VenueId::Deribit, 513 + k),
            perp,
            VenueId::Deribit as u8,
            EXPIRY,
            (77_000 + 250 * k as i64) * 1_000_000,
            if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
            1_000_000_000,
        ))
        .expect("boot insert");
        k += 1;
    }

    let mut m = Box::new(strategy_vrp::VrpStrategy::new());
    m.configure(
        strategy_vrp::VrpParams::default(),
        reg,
        perp,
        perp,
        core_time::WallAnchor::new(MONO0, WALL0),
        [0u8; 32],
    )
    .expect("configure");

    let mono_of = |wall: u64| MONO0.wrapping_add(wall.wrapping_sub(WALL0));
    let mk_tick = |wall: u64, px: i64| {
        Tick::new(
            mono_of(wall),
            VenueId::Deribit,
            perp,
            0,
            Price::from_raw(px - 500_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(px + 500_000),
            Qty::from_raw(1_000_000),
        )
    };
    let mk_opt = |wall: u64, sym: u32, iv: i64| {
        OptSummary::new(
            mono_of(wall),
            VenueId::Deribit,
            sym,
            OPT_SUMMARY_FLAG_MARK_PX,
            3_800_000,
            iv,
            79_000_000_000_000,
            0,
            500_000_000,
            1,
            1,
            -1,
        )
    };

    let mut ctx = SinkCtx { n: 0, now: MONO0, last: None };
    // Boot: warm the ring and seed a fit. Not measured.
    let mut wall = WALL0;
    let mut px = 79_000_000_000i64;
    let mut s = 20_260_910i64;
    let mut i = 0usize;
    while i < 1_442 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        m.on_tick(&mk_tick(wall, px), &mut ctx);
        wall += MINUTE_NS;
        i += 1;
    }
    let mut j = 0i64;
    while j < 60 {
        m.seed_pair(24_000_000_000 + j * 11_000_000, 24_100_000_000 + j * 9_000_000);
        j += 1;
    }

    // Measured: the two live callbacks, driven through a full campaign
    // (selection, the entry decision, hedges, the E−ε unwind) so the
    // gate covers the branches that submit, not only the ones that skip.
    let g = AllocGuard::new();
    let start = EXPIRY - 28_800_000_000_000 - 600_000_000_000;
    let mut w = start;
    let mut n = 0usize;
    while n < 4_000 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        ctx.now = mono_of(w);
        m.on_opt_summary(&mk_opt(w, make_symbol_id(VenueId::Deribit, 513 + 8), 5_000_000_000), &mut ctx);
        m.on_tick(&mk_tick(w, px), &mut ctx);
        // X1: the position path. Every order the member just emitted
        // comes straight back as a full fill, so `on_fill` — which now
        // moves the book and can submit the hedge — is inside the
        // guard too.
        if let Some(o) = ctx.last.take() {
            let f = core_types::Fill::new(
                mono_of(w),
                o.sym,
                o.side,
                o.px,
                o.qty,
                o.client_oid,
            )
            .with_attribution(1, core_types::FILL_ORIGIN_PAPER);
            m.on_fill(&f, &mut ctx);
        }
        w += 7_500_000_000; // 7.5 s: 4000 steps span the whole 8 h hold
        n += 1;
    }
    let counters = m.vrp_counters();
    std::hint::black_box((ctx.n, counters));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(counters.decisions > 0, "the gate must measure a real decision");
    assert!(ctx.n > 0, "the gate must measure real submits");
    assert!(counters.fills > 0, "and real FILLS — X1's whole path");
    assert_eq!(
        allocs, 0,
        "strategy-vrp callbacks allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "strategy-vrp hot bytes should be zero: saw {bytes}");
}

/// Gate 46b (VRP P3.1, R1/R2): the same member under the EXECUTION
/// modes — a resting option entry with the cross fallback, a resting
/// hedge with its unconditional cross, and the option QUOTE lane
/// feeding `on_tick` on every step.
///
/// The quote lane is the part that matters here: it is a new branch on
/// the hottest callback the member has, it runs a `checked_mul` and two
/// `coin_to_usd_1e6` conversions per tick, and `opt_cost_1e6` /
/// `theta_eff_1e9` reach `fx::ln_1e9` on the fallback path. None of
/// that may allocate.
#[test]
fn vrp_member_execution_modes_are_zero_alloc() {
    use core_types::{make_symbol_id, OptSummary, Price, Qty, Tick, OPT_SUMMARY_FLAG_MARK_PX};
    use opt_registry::{OptInstrument, RIGHT_CALL, RIGHT_PUT};
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    const MONO0: u64 = 3_191_000_000_000_000;
    const EXPIRY: u64 = 1_789_027_200_000_000_000;
    const WALL0: u64 = EXPIRY - 172_800_000_000_000;
    const MINUTE_NS: u64 = 60_000_000_000;

    struct SinkCtx {
        n: u64,
        now: u64,
        last: Option<core_types::Order>,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            self.n += 1;
            self.last = Some(order);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let perp = make_symbol_id(VenueId::Deribit, 1);
    let opt = make_symbol_id(VenueId::Deribit, 513 + 8);
    let mut reg = opt_registry::OptRegistry::new();
    let mut k = 0u32;
    while k < 16 {
        reg.insert(OptInstrument::new(
            make_symbol_id(VenueId::Deribit, 513 + k),
            perp,
            VenueId::Deribit as u8,
            EXPIRY,
            (77_000 + 250 * k as i64) * 1_000_000,
            if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
            1_000_000_000,
        ))
        .expect("boot insert");
        k += 1;
    }

    let mut m = Box::new(strategy_vrp::VrpStrategy::new());
    m.configure(
        strategy_vrp::VrpParams {
            entry_mode: strategy_vrp::ENTRY_MODE_MAKER,
            entry_fallback: strategy_vrp::ENTRY_FALLBACK_CROSS,
            hedge_mode: strategy_vrp::HEDGE_MODE_MAKER,
            ..strategy_vrp::VrpParams::default()
        },
        reg,
        perp,
        perp,
        core_time::WallAnchor::new(MONO0, WALL0),
        [0u8; 32],
    )
    .expect("configure");

    let mono_of = |wall: u64| MONO0.wrapping_add(wall.wrapping_sub(WALL0));
    let mk_tick = |wall: u64, px: i64| {
        Tick::new(
            mono_of(wall),
            VenueId::Deribit,
            perp,
            0,
            Price::from_raw(px - 500_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(px + 500_000),
            Qty::from_raw(1_000_000),
        )
    };
    // The option's own quote lane: COIN on the wire, as the venue sends
    // it. 0.0036 / 0.0040 BTC around a 0.0038 mark.
    let mk_opt_tick = |wall: u64| {
        Tick::new(
            mono_of(wall),
            VenueId::Deribit,
            opt,
            0,
            Price::from_raw(3_600),
            Qty::from_raw(1_000_000),
            Price::from_raw(4_000),
            Qty::from_raw(1_000_000),
        )
    };
    let mk_opt = |wall: u64, iv: i64| {
        OptSummary::new(
            mono_of(wall),
            VenueId::Deribit,
            opt,
            OPT_SUMMARY_FLAG_MARK_PX,
            3_800_000,
            iv,
            79_000_000_000_000,
            0,
            500_000_000,
            1,
            1,
            -1,
        )
    };

    let mut ctx = SinkCtx { n: 0, now: MONO0, last: None };
    let mut wall = WALL0;
    let mut px = 79_000_000_000i64;
    let mut s = 20_260_913i64;
    let mut i = 0usize;
    while i < 1_442 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        m.on_tick(&mk_tick(wall, px), &mut ctx);
        wall += MINUTE_NS;
        i += 1;
    }
    let mut j = 0i64;
    while j < 60 {
        m.seed_pair(24_000_000_000 + j * 11_000_000, 24_100_000_000 + j * 9_000_000);
        j += 1;
    }

    let g = AllocGuard::new();
    let start = EXPIRY - 28_800_000_000_000 - 600_000_000_000;
    let mut w = start;
    let mut n = 0usize;
    while n < 4_000 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        ctx.now = mono_of(w);
        m.on_opt_summary(&mk_opt(w, 5_000_000_000), &mut ctx);
        // The new branch: the selected option's own quote, converted.
        m.on_tick(&mk_opt_tick(w), &mut ctx);
        m.on_tick(&mk_tick(w, px), &mut ctx);
        // Every OTHER order fills, so the sweep, the cross fallback and
        // the maker handover all run inside the guard.
        if n % 2 == 0 {
            if let Some(o) = ctx.last.take() {
                let f = core_types::Fill::new(
                    mono_of(w),
                    o.sym,
                    o.side,
                    o.px,
                    o.qty,
                    o.client_oid,
                )
                .with_attribution(1, core_types::FILL_ORIGIN_PAPER);
                m.on_fill(&f, &mut ctx);
            }
        } else {
            ctx.last = None;
        }
        w += 7_500_000_000;
        n += 1;
    }
    let counters = m.vrp_counters();
    std::hint::black_box((ctx.n, counters));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(counters.decisions > 0, "a real decision");
    assert!(
        counters.entry_maker_submitted > 0,
        "the gate must measure a RESTING entry: {counters:?}"
    );
    assert!(ctx.n > 0, "real submits");
    assert_eq!(
        allocs, 0,
        "strategy-vrp execution modes allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "execution-mode hot bytes should be zero: saw {bytes}");
}

/// Gate 46 (XSD-2, statarb doc 08 §3.9): the xsd member's live callbacks
/// at full capacity — 128 targets × 3 partners over 130 syms with a
/// 720-hour window — through 24 hourly rolls (each `O(pairs × window)`),
/// ~6,000 ticks, entries, exits and a regime flip. The boot box, the
/// table and the seed are not measured.
#[test]
fn xsd_member_tick_roll_and_regime_are_zero_alloc() {
    use core_types::{make_symbol_id, Price, Qty, Tick, RegimeWord, REGIME_OFF_HARD};
    use strategy_core::{Ctx, RegimeGate, Strategy, StrategyCounters, SubmitErr};
    use strategy_xsd::{XsdParams, XsdStrategy, XsdTable, XsdTableRow, HOUR_NS, XSD_MAX_TARGETS};

    const MONO0: u64 = 3_191_000_000_000_000;
    const WALL0: u64 = 1_789_171_200_000_000_000; // 2026-09-12 00:00:00Z
    const HOUR0: i64 = (WALL0 / HOUR_NS) as i64;
    const N_SYMS: usize = XSD_MAX_TARGETS + 2;
    const WINDOW: u32 = 720;

    struct SinkCtx {
        n: u64,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, _order: core_types::Order) -> Result<(), SubmitErr> {
            self.n += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            0
        }
    }

    let sym_of = |i: usize| make_symbol_id(VenueId::Binance, 512 + i as u32);
    let mut table = XsdTable::EMPTY;
    let mut t = 0usize;
    while t < XSD_MAX_TARGETS {
        let mut k = 0usize;
        while k < 3 {
            table.rows[table.n] = XsdTableRow {
                target: sym_of(t),
                partner: sym_of((t + 1 + k) % N_SYMS),
                beta_1e9: 1_000_000_000,
            };
            table.n += 1;
            k += 1;
        }
        t += 1;
    }
    table.hash = [1; 32];
    let params = XsdParams {
        z_window_h: WINDOW,
        z_enter_1e9: 3_000_000_000,
        z_exit_1e9: 0,
        z_stop_1e9: 5_000_000_000,
        consensus: 1,
        grid_n: 3,
        grid_step_1e9: 500_000_000,
        max_hold_h: 240,
        cooldown_h: 1,
        ttl_ns: 300_000_000_000,
        position_usd_1e6: 1_000_000_000,
        max_positions: 82,
        max_gross_usd_1e6: 100_000_000_000,
        direction: 1,
        slip_1e9: 0,
        hash: [2; 32],
    };
    let mut m = XsdStrategy::new();
    m.configure(core_time::WallAnchor::new(MONO0, WALL0), &params, &table)
        .expect("configure");

    // Boot seed: 720 hours of a noisy walk per sym (not measured).
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut px = [100_000_000i64; N_SYMS];
    let mut h = HOUR0 - WINDOW as i64;
    while h < HOUR0 {
        let mut i = 0usize;
        while i < N_SYMS {
            px[i] = (px[i] + (next() % 100_001) as i64 - 50_000).max(50_000_000);
            assert!(m.seed_close(sym_of(i), h, px[i]));
            i += 1;
        }
        h += 1;
    }

    let mono_at = |hour: i64, off: u64| MONO0 + (hour - HOUR0) as u64 * HOUR_NS + off;
    let mk_tick = |i: usize, ts: u64, p: i64| {
        Tick::new(
            ts,
            VenueId::Binance,
            sym_of(i),
            0,
            Price::from_raw(p - 1_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(p + 1_000),
            Qty::from_raw(1_000_000),
        )
    };
    let mut ctx = SinkCtx { n: 0 };
    m.on_timer(mono_at(HOUR0, 1), &mut ctx);

    // Measured: 24 live hours. Every fourth target dislocates +20 % for
    // two hours from hour 2 (entries, then stops or adds), snaps back
    // (revert exits); a hard-closed gate at hour 12 flattens whatever is
    // still open, reopened at hour 13.
    let g = AllocGuard::new();
    let mut hour = HOUR0;
    while hour < HOUR0 + 24 {
        let rel = hour - HOUR0;
        let mut i = 0usize;
        while i < N_SYMS {
            let mut p = (px[i] + (next() % 100_001) as i64 - 50_000).max(50_000_000);
            if i % 4 == 0 && (2..4).contains(&rel) {
                p = p * 120 / 100;
            }
            m.on_tick(&mk_tick(i, mono_at(hour, 1_000_000_000 + i as u64), p), &mut ctx);
            m.on_tick(&mk_tick(i, mono_at(hour, 3_599_000_000_000 + i as u64), p), &mut ctx);
            px[i] = p;
            i += 1;
        }
        if rel == 12 {
            let hard = RegimeGate::new([RegimeWord::UNKNOWN; 4], false, REGIME_OFF_HARD);
            m.on_regime(hard, &mut ctx);
        }
        if rel == 13 {
            m.on_regime(RegimeGate::OPEN_UNKNOWN, &mut ctx);
        }
        hour += 1;
        m.on_timer(mono_at(hour, 1), &mut ctx);
    }
    let counters = m.xsd_counters();
    std::hint::black_box((ctx.n, counters));

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(counters.rolls, 24, "every boundary rolled");
    assert!(counters.entries > 0, "the gate must measure real entries");
    let exits = counters.exits_revert + counters.exits_stop + counters.exits_maxhold + counters.exits_regime;
    assert!(exits > 0, "the gate must measure real exits");
    assert!(ctx.n > 0, "the gate must measure real submits");
    assert_eq!(
        allocs, 0,
        "strategy-xsd callbacks allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "strategy-xsd hot bytes should be zero: saw {bytes}");
}

/// Gate 49 (VRP P4): the member's DECISION paths at full width — the
/// regime view pushed on a minute cadence (R3), the selection window's
/// implied-vol ring and its insertion-sorted median (R6), the venue's
/// 30-minute delivery TWAP (R5) and the cost term (R7) — over a whole
/// campaign, selection through settlement. The warm-up and the boot box
/// are not measured.
///
/// The median is the one that had to be gated: it copies 64 `i64`s to
/// the stack and sorts them in place, once per campaign, and an
/// implementation that reached for a `Vec` would read identically at
/// the call site.
#[test]
fn vrp_member_p4_decision_paths_are_zero_alloc() {
    use core_types::{make_symbol_id, OptSummary, Price, Qty, Tick, OPT_SUMMARY_FLAG_MARK_PX};
    use opt_registry::{OptInstrument, RIGHT_CALL, RIGHT_PUT};
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    const MONO0: u64 = 3_191_000_000_000_000;
    const EXPIRY: u64 = 1_789_027_200_000_000_000;
    const WALL0: u64 = EXPIRY - 172_800_000_000_000;
    const MINUTE_NS: u64 = 60_000_000_000;

    struct SinkCtx {
        n: u64,
        now: u64,
        last: Option<core_types::Order>,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, order: core_types::Order) -> Result<(), SubmitErr> {
            self.n += 1;
            self.last = Some(order);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let perp = make_symbol_id(VenueId::Deribit, 1);
    let opt = make_symbol_id(VenueId::Deribit, 513 + 8);
    let mut reg = opt_registry::OptRegistry::new();
    let mut k = 0u32;
    while k < 16 {
        reg.insert(OptInstrument::new(
            make_symbol_id(VenueId::Deribit, 513 + k),
            perp,
            VenueId::Deribit as u8,
            EXPIRY,
            (77_000 + 250 * k as i64) * 1_000_000,
            if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
            1_000_000_000,
        ))
        .expect("boot insert");
        k += 1;
    }

    // R3: the §3.3 train-only fast-profile intercepts, so the offset in
    // force at the decision is non-zero and the arm is scored on it.
    let mut regime_off = [[0i64; 3]; 2];
    regime_off[0][core_types::regime::VOL_LOW as usize] = 20_000_000;
    regime_off[0][core_types::regime::VOL_HIGH as usize] = -99_000_000;
    regime_off[1][core_types::regime::VOL_LOW as usize] = 77_000_000;
    regime_off[1][core_types::regime::VOL_HIGH as usize] = -165_000_000;

    let mut m = Box::new(strategy_vrp::VrpStrategy::new());
    m.configure(
        strategy_vrp::VrpParams {
            regime_off_1e9: regime_off,
            ..strategy_vrp::VrpParams::default()
        },
        reg,
        perp,
        perp,
        core_time::WallAnchor::new(MONO0, WALL0),
        [0u8; 32],
    )
    .expect("configure");

    // The view the set pushes on every minute roll: `vol:high` fast,
    // `vol:low` slow — both profiles speaking, so both table rows are
    // read and the offsets ADD.
    let mut view = core_regime::RegimeView::UNKNOWN;
    view.configured = 1;
    view.effective[0] =
        core_types::RegimeWord::EMPTY.with_dim(core_types::regime::DIM_VOL, core_types::regime::VOL_HIGH);
    view.effective[1] =
        core_types::RegimeWord::EMPTY.with_dim(core_types::regime::DIM_VOL, core_types::regime::VOL_LOW);

    let mono_of = |wall: u64| MONO0.wrapping_add(wall.wrapping_sub(WALL0));
    let mk_tick = |wall: u64, px: i64| {
        Tick::new(
            mono_of(wall),
            VenueId::Deribit,
            perp,
            0,
            Price::from_raw(px - 500_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(px + 500_000),
            Qty::from_raw(1_000_000),
        )
    };
    let mk_opt = |wall: u64, iv: i64, under: i64| {
        OptSummary::new(
            mono_of(wall),
            VenueId::Deribit,
            opt,
            OPT_SUMMARY_FLAG_MARK_PX,
            3_800_000,
            iv,
            under,
            0,
            500_000_000,
            1,
            1,
            -1,
        )
    };

    let mut ctx = SinkCtx { n: 0, now: MONO0, last: None };
    let mut wall = WALL0;
    let mut px = 79_000_000_000i64;
    let mut s = 20_260_914i64;
    let mut i = 0usize;
    while i < 1_442 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        m.on_tick(&mk_tick(wall, px), &mut ctx);
        wall += MINUTE_NS;
        i += 1;
    }
    let mut j = 0i64;
    while j < 60 {
        m.seed_pair(24_000_000_000 + j * 11_000_000, 24_100_000_000 + j * 9_000_000);
        j += 1;
    }

    let g = AllocGuard::new();
    // Selection window open through settlement: 8 h 10 m at 15 s a step.
    let start = EXPIRY - 28_800_000_000_000 - 600_000_000_000;
    let mut w = start;
    let mut n = 0usize;
    while n < 2_100 {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        px = (px + ((s as u64 >> 32) % 40_000_000) as i64 - 20_000_000).max(1_000_000_000);
        ctx.now = mono_of(w);
        // R3: the set pushes the view on the minute roll, never per
        // tick — four steps apart at 15 s is exactly that cadence.
        if n % 4 == 0 {
            m.set_regime_view(&view);
        }
        // R5/R6: the summary feeds the implied-vol ring and, in the
        // last half hour, the delivery TWAP.
        m.on_opt_summary(&mk_opt(w, 5_000_000_000, px * 1_000), &mut ctx);
        m.on_tick(&mk_tick(w, px), &mut ctx);
        if n % 2 == 0 {
            if let Some(o) = ctx.last.take() {
                let f = core_types::Fill::new(
                    mono_of(w),
                    o.sym,
                    o.side,
                    o.px,
                    o.qty,
                    o.client_oid,
                )
                .with_attribution(1, core_types::FILL_ORIGIN_PAPER);
                m.on_fill(&f, &mut ctx);
            }
        } else {
            ctx.last = None;
        }
        w += 15_000_000_000;
        n += 1;
    }
    let counters = m.vrp_counters();
    let off = m.last_regime_offset_1e9();
    std::hint::black_box((ctx.n, counters, off));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(counters.decisions > 0, "a real decision: {counters:?}");
    assert_eq!(
        counters.iv_median_fallback, 0,
        "R6: the median must have SORTED, not fallen back: {counters:?}"
    );
    assert_eq!(
        off,
        -99_000_000 + 77_000_000,
        "R3: both profiles' intercepts were in force at the decision"
    );
    assert!(ctx.n > 0, "real submits");
    assert_eq!(
        allocs, 0,
        "strategy-vrp P4 decision paths allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "P4 decision-path hot bytes should be zero: saw {bytes}");
}

/// Gate 52 (BIN15 O4b, spec §6.4; narrowed at H90 G3): the bin15
/// member's live callbacks at full width — eight families over four
/// underlyings, 100 rolls, 10 000 `Mark` events with their per-minute
/// forecast refresh, both binary legs ticking and the 1 s timer — with
/// the late law OFF, so what runs is the S-law pricer, the member's
/// logged counterfactual, which never trades (the arms it once fed were
/// retired at H90 G3; gate 52b measures the late law's takes).
///
/// The boot box is not measured: the lookup tables are 17 KiB on the
/// heap by design (`Box<Bin15Luts>`), the two `VolEngine`s per
/// underlying are boxed, and the seeds replay 1 441 returns and 60
/// pairs per underlying per tenor. All of that happens before the
/// guard, which is the point — after `configure` the member must never
/// allocate again.
///
/// What this gate actually protects: the re-price runs on EVERY mark of
/// EVERY underlying (one mark moves every binary struck against it), so
/// a member with eight families on four coins re-prices twenty thousand
/// times over this tape. An allocation anywhere on that path — a `Vec`
/// of candidates, a `format!` in a counter's name — would be invisible
/// at the call site and fatal at 4 marks a second per coin.
#[test]
fn bin15_member_roll_tick_reprice_is_zero_alloc() {
    use core_types::{make_symbol_id, ChannelEvent, ChannelId, Order};
    use strategy_bin15::{
        price, Bin15Params, Bin15Strategy, FAMILY_NATIVE_DAILY, FAMILY_OUT_15M,
        BIN15_MAX_FAMILIES, BIN15_MAX_UNDERLYINGS,
    };
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    const MONO0: u64 = 4_100_000_000_000_000;
    const WALL0: u64 = 1_789_171_200_000_000_000; // 2026-09-12 00:00:00Z
    /// Marks per instance, one a second.
    const MARKS_PER_ROLL: u64 = 100;
    const ROLLS: usize = 100;

    struct SinkCtx {
        n: u64,
        now: u64,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, _order: Order) -> Result<(), SubmitErr> {
            self.n += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    let under_sym = |u: usize| make_symbol_id(VenueId::Hyperliquid, 3 + u as u32);
    let yes_sym = |f: usize| make_symbol_id(VenueId::Hyperliquid, 400 + 2 * f as u32);

    // Eight families over four underlyings: the 15 m four and the
    // native-daily four, so both tenors of every coin are live at once
    // — which is ruling O-Q7's shape and the reason a `MarkState`
    // carries two forecast engines.
    let mut params = Bin15Params::default();
    let mut f = 0usize;
    while f < BIN15_MAX_FAMILIES {
        params.sym_yes[f] = yes_sym(f);
        params.sym_no[f] = yes_sym(f) + 1;
        params.family_underlying[f] = (f % BIN15_MAX_UNDERLYINGS) as u8;
        params.family_kind[f] = if f < 4 {
            FAMILY_OUT_15M
        } else {
            FAMILY_NATIVE_DAILY
        };
        f += 1;
    }
    params.n_families = BIN15_MAX_FAMILIES;
    let mut u = 0usize;
    while u < BIN15_MAX_UNDERLYINGS {
        params.underlying_sym[u] = under_sym(u);
        u += 1;
    }
    params.n_underlyings = BIN15_MAX_UNDERLYINGS;
    // The late law stays OFF (the default): every order would be its, and
    // gate 52b measures it.
    params.cap_instance_usd_1e6 = 1_000_000_000;
    params.cap_day_usd_1e6 = 1_000_000_000_000;

    // A real CDF shape, boxed before the guard.
    let mut luts = Box::new(price::Bin15Luts::identity());
    let mut i = 0usize;
    while i < price::PHI_POINTS {
        luts.phi[i] = (500_000 + (i as i64 * 499_950) / (price::PHI_POINTS as i64 - 1)) as u32;
        i += 1;
    }
    let mut m = Bin15Strategy::new();
    m.configure(params, luts, core_time::WallAnchor::new(MONO0, WALL0))
        .expect("configure");

    // Boot seeds (not measured): 1 441 stamped returns fill the HAR
    // window and 60 pairs on the identity line fit the forecast, per
    // underlying and per tenor.
    let mut u = 0usize;
    while u < BIN15_MAX_UNDERLYINGS {
        let mut rets: Vec<(u64, i64)> = Vec::with_capacity(1_441);
        let mut k = 0u64;
        while k < 1_441 {
            let r = if k % 2 == 0 { 5_164_000_000 } else { -5_164_000_000 };
            rets.push((WALL0 / 1_000_000 - (1_441 - k) * 60_000, r));
            k += 1;
        }
        m.seed_returns(u, &rets);
        let mut pairs: Vec<(u64, i64, i64)> = Vec::with_capacity(60);
        let mut k = 0i64;
        while k < 60 {
            let x = 23_000_000_000 + k * 100_000_000;
            pairs.push((WALL0 / 1_000_000 - (60 - k as u64) * 900_000, x, x));
            k += 1;
        }
        m.seed_pairs(u, FAMILY_OUT_15M, &pairs);
        m.seed_pairs(u, FAMILY_NATIVE_DAILY, &pairs);
        u += 1;
    }

    let at = |secs: u64| MONO0 + secs * 1_000_000_000;
    let mark_ev = |u: usize, px_1e6: i64, ts: u64| {
        ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Mark,
            under_sym(u),
            0,
            0,
            px_1e6,
            0,
        )
    };
    let roll_ev = |fam: usize, outcome: u32, strike_1e6: i64, expiry_wall: u64, settled: bool| {
        let seq = u64::from(outcome)
            | (60u64 << 32)
            | ((fam as u64) << 48)
            | (u64::from(settled) << 56);
        ChannelEvent::new(
            at(0),
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            yes_sym(fam),
            seq,
            0,
            strike_1e6,
            expiry_wall as i64,
        )
    };
    let leg_tick = |sym: SymbolId, ts: u64, bid_1e6: i64, ask_1e6: i64| {
        Tick::new(
            ts,
            VenueId::Hyperliquid,
            sym,
            0,
            Price::from_raw(bid_1e6),
            Qty::from_raw(100_000_000),
            Price::from_raw(ask_1e6),
            Qty::from_raw(100_000_000),
        )
    };

    let mut ctx = SinkCtx { n: 0, now: at(0) };
    // One mark per underlying before the guard, so the first measured
    // mark is a minute ROLL and not a cold first sighting.
    let mut u = 0usize;
    while u < BIN15_MAX_UNDERLYINGS {
        m.on_venue_event(&mark_ev(u, 79_000_000_000, at(0)), &mut ctx);
        u += 1;
    }

    // Measured.
    let g = AllocGuard::new();
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut mark_px = [79_000_000_000i64; BIN15_MAX_UNDERLYINGS];
    let mut marks_seen = 0u64;
    let mut roll = 0usize;
    while roll < ROLLS {
        let fam = roll % BIN15_MAX_FAMILIES;
        let t0 = 1 + roll as u64 * MARKS_PER_ROLL;
        let outcome = 2_600 + roll as u32;
        ctx.now = at(t0);
        // The outgoing instance settles, then the successor binds — the
        // venue's own order inside one push burst.
        if roll >= BIN15_MAX_FAMILIES {
            m.on_venue_event(
                &roll_ev(fam, 2_600 + (roll - BIN15_MAX_FAMILIES) as u32, 0, 0, true),
                &mut ctx,
            );
        }
        // A strike 10 % below the mark: the Yes leg is a certainty, so
        // the pricer's φ table is read at its far end on every reprice.
        m.on_venue_event(
            &roll_ev(
                fam,
                outcome,
                71_000_000_000,
                WALL0 + (t0 + 900) * 1_000_000_000,
                false,
            ),
            &mut ctx,
        );
        let mut k = 0u64;
        while k < MARKS_PER_ROLL {
            let ts = at(t0 + k);
            ctx.now = ts;
            let ui = (roll + k as usize) % BIN15_MAX_UNDERLYINGS;
            mark_px[ui] = (mark_px[ui] + (next() % 20_000_001) as i64 - 10_000_000)
                .max(60_000_000_000);
            m.on_venue_event(&mark_ev(ui, mark_px[ui], ts), &mut ctx);
            marks_seen += 1;
            // Both legs tick every tenth second (a touch change re-prices),
            // and the 1 s timer sweeps.
            if k % 10 == 0 {
                m.on_tick(&leg_tick(yes_sym(fam) + 1, ts, 390_000, 600_000), &mut ctx);
                m.on_tick(&leg_tick(yes_sym(fam), ts, 300_000, 400_000), &mut ctx);
                m.on_timer(ts, &mut ctx);
            }
            k += 1;
        }
        roll += 1;
    }
    // BIN15 S3: the last minute of an instance, under the same guard —
    // `fold_twap` on every mark, `fair_value_twap`'s running-TWAP branch,
    // the cubic horizon. Every instance above settles 100 s before its
    // window opens, so without this phase none of it would be measured.
    // Family 0 settles its last instance (roll 96) and binds one whose
    // window `[T − 60 s, T]` opens 10 s in; underlying 0 marks every
    // second until the tail holds.
    let t0 = 1 + ROLLS as u64 * MARKS_PER_ROLL;
    ctx.now = at(t0);
    m.on_venue_event(&roll_ev(0, 2_600 + 96, 0, 0, true), &mut ctx);
    m.on_venue_event(
        &roll_ev(0, 9_000, 79_000_000_000, WALL0 + (t0 + 70) * 1_000_000_000, false),
        &mut ctx,
    );
    let mut priced_inside = 0u64;
    let mut k = 0u64;
    while k < 62 {
        let ts = at(t0 + k);
        ctx.now = ts;
        let px = 79_000_000_000 + (next() % 20_000_001) as i64 - 10_000_000;
        m.on_venue_event(&mark_ev(0, px, ts), &mut ctx);
        if k == 0 {
            m.on_tick(&leg_tick(yes_sym(0) + 1, ts, 390_000, 600_000), &mut ctx);
            m.on_tick(&leg_tick(yes_sym(0), ts, 300_000, 400_000), &mut ctx);
        }
        if let Some(f0) = m.family(0) {
            priced_inside += u64::from(
                f0.p_ts_ns == ts && f0.twap_last_ts > 0 && f0.last_tau_ns < 20_000_000_000,
            );
        }
        k += 1;
    }
    let counters = m.bin15_counters();
    std::hint::black_box((ctx.n, counters));

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(marks_seen, ROLLS as u64 * MARKS_PER_ROLL, "10 000 marks");
    assert!(
        priced_inside > 40,
        "the running-TWAP branch ran under the guard: {priced_inside} in-window reprices"
    );
    assert_eq!(counters.rolls, ROLLS as u64 + 1, "every roll bound an instance (+ S3's)");
    assert!(counters.rolls_settled > 0, "and the predecessors settled");
    assert!(
        counters.reprices > 10_000,
        "one mark re-prices every family on its underlying: saw {}",
        counters.reprices
    );
    assert_eq!(ctx.n, 0, "late off: the S-law pricer is a counterfactual and never trades");
    assert_eq!(counters.takes_submitted, 0, "and counted none");
    assert_eq!(
        allocs, 0,
        "strategy-bin15 callbacks allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "strategy-bin15 hot bytes should be zero: saw {bytes}");
}

/// Gate 52b (BIN15 H90, vault doc 33 §L1–L2): the bin15 member's
/// LATE-WINDOW entry under gate 52's law — after `configure` the member
/// never allocates — at the law's own width: the three 15 m families
/// (BTC, ETH, HYPE) on their own underlyings, a `Mark` AND a perp BBO per
/// underlying every second for an hour, 90 instances of two minutes, and
/// every late order answered (a full fill, or — once in four instances —
/// the gateway's refusal, which re-arms the entry).
///
/// What runs under the guard: the 1-s grid on every mark and every perp
/// tick (`advance_to`, the r² ring and its running sum, the basis ring),
/// `late_fair` on every reprice inside the `[2, 30)` s window (the
/// integer square root, the Φ lookup), the S5 poll's run on the favoured
/// leg — Yes instances, No instances, and at-the-money ones whose books
/// the price test refuses — both counterfactuals (P1b's and the
/// production p̂'s, which the seeded S-law pricer stamps at τ ≥ 10 s), the
/// σ-cold holds of the first 300 s, the take, its fill or refusal, and the
/// settle that resets the family's `LateState`.
///
/// The late law is the member's only trading law since H90 G3, so every
/// order on the tape is its. Not measured: `configure`, the two boxed
/// Φ tables (`Box<Bin15Luts>`, `Box<LateLut>`: 16 KiB each, built once at
/// boot) and the HAR seeds.
#[test]
fn bin15_late_window_entry_is_zero_alloc() {
    use core_types::{make_symbol_id, ChannelEvent, ChannelId, Order};
    use strategy_bin15::{late, price, Bin15Params, Bin15Strategy, FAMILY_OUT_15M};
    use strategy_core::{Ctx, Strategy, StrategyCounters, SubmitErr};

    const MONO0: u64 = 4_200_000_000_000_000;
    const WALL0: u64 = 1_789_171_200_000_000_000; // 2026-09-12 00:00:00Z
    const FAMS: usize = 3;
    /// One instance's life, bind to expiry, in seconds.
    const LIFE: u64 = 120;
    /// The tape's length in seconds.
    const SECS: u64 = 3_600;
    /// The instances' starts, staggered per family.
    const STAGGER: u64 = 10;
    /// BTC, ETH and HYPE's marks ×1e6 at the start of the tape.
    const BASE: [i64; FAMS] = [79_000_000_000, 2_500_000_000, 40_000_000];

    struct SinkCtx {
        n: u64,
        n_yes: u64,
        last: Option<Order>,
        now: u64,
        yes: [SymbolId; FAMS],
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            self.n += 1;
            let mut f = 0usize;
            while f < FAMS {
                self.n_yes += u64::from(order.sym == self.yes[f]);
                f += 1;
            }
            self.last = Some(order);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    /// Answer the order the last callback emitted, if any: the gateway's
    /// refusal when its family is marked to refuse one, else a full fill.
    /// Returns `1` for a refusal.
    fn answer(m: &mut Bin15Strategy, ctx: &mut SinkCtx, refuse_next: &mut [u8; FAMS]) -> u64 {
        let Some(o) = ctx.last.take() else {
            return 0;
        };
        let mut f = 0usize;
        while f < FAMS && o.sym != ctx.yes[f] && o.sym != ctx.yes[f] + 1 {
            f += 1;
        }
        assert!(f < FAMS, "a late order names one of the three families' legs");
        if refuse_next[f] == 1 {
            refuse_next[f] = 0;
            let mut ev = core_types::OrderEvent::ZERO;
            ev.client_oid = o.client_oid;
            ev.kind = core_types::ORDER_EVENT_REJECTED;
            m.on_order_event(&ev, ctx);
            return 1;
        }
        let fill = core_types::Fill::new(ctx.now, o.sym, o.side, o.px, o.qty, o.client_oid);
        m.on_fill(&fill, ctx);
        0
    }

    let under_sym = |u: usize| make_symbol_id(VenueId::Hyperliquid, 3 + u as u32);
    let yes_sym = |f: usize| make_symbol_id(VenueId::Hyperliquid, 400 + 2 * f as u32);

    let mut params = Bin15Params::default();
    let mut f = 0usize;
    while f < FAMS {
        params.sym_yes[f] = yes_sym(f);
        params.sym_no[f] = yes_sym(f) + 1;
        params.family_underlying[f] = f as u8;
        params.family_kind[f] = FAMILY_OUT_15M;
        params.underlying_sym[f] = under_sym(f);
        f += 1;
    }
    params.n_families = FAMS;
    params.n_underlyings = FAMS;
    params.cap_instance_usd_1e6 = 1_000_000_000;
    params.cap_day_usd_1e6 = 1_000_000_000_000;
    // The ruled law (O-H90-5..7): [2, 30) s, p ≥ 0.90, 2 c, two polls, $100.
    params.late.enabled = 1;

    // Real CDF shapes, boxed before the guard: the S-law's ramp (gate 52's)
    // and the late table rising from 0.5 at z = 0 to certainty at z = 4.
    let mut luts = Box::new(price::Bin15Luts::identity());
    let mut i = 0usize;
    while i < price::PHI_POINTS {
        luts.phi[i] = (500_000 + (i as i64 * 499_950) / (price::PHI_POINTS as i64 - 1)) as u32;
        i += 1;
    }
    let mut late_lut = Box::new(late::LateLut::flat());
    let mut i = 0usize;
    while i < late::LATE_PHI_POINTS {
        let z = i as i64 * late::LATE_PHI_STEP_1E6;
        late_lut.phi[i] = (500_000 + z.min(4_000_000) / 8) as u32;
        i += 1;
    }
    let mut m = Bin15Strategy::new();
    m.configure(params, luts, core_time::WallAnchor::new(MONO0, WALL0))
        .expect("configure");
    m.configure_late(late_lut);

    // Boot seeds (not measured), gate 52's: the HAR window and the line,
    // so the S-law pricer stamps p̂ and the production counterfactual runs.
    let mut u = 0usize;
    while u < FAMS {
        let mut rets: Vec<(u64, i64)> = Vec::with_capacity(1_441);
        let mut k = 0u64;
        while k < 1_441 {
            let r = if k % 2 == 0 { 5_164_000_000 } else { -5_164_000_000 };
            rets.push((WALL0 / 1_000_000 - (1_441 - k) * 60_000, r));
            k += 1;
        }
        m.seed_returns(u, &rets);
        let mut pairs: Vec<(u64, i64, i64)> = Vec::with_capacity(60);
        let mut k = 0i64;
        while k < 60 {
            let x = 23_000_000_000 + k * 100_000_000;
            pairs.push((WALL0 / 1_000_000 - (60 - k as u64) * 900_000, x, x));
            k += 1;
        }
        m.seed_pairs(u, FAMILY_OUT_15M, &pairs);
        u += 1;
    }

    let at = |secs: u64| MONO0 + secs * 1_000_000_000;
    let mark_ev = |u: usize, px_1e6: i64, ts: u64| {
        ChannelEvent::new(ts, VenueId::Hyperliquid, ChannelId::Mark, under_sym(u), 0, 0, px_1e6, 0)
    };
    let roll_ev = |fam: usize, outcome: u32, strike_1e6: i64, expiry_wall: u64, settled: bool, ts: u64| {
        let seq = u64::from(outcome) | (60u64 << 32) | ((fam as u64) << 48) | (u64::from(settled) << 56);
        ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            yes_sym(fam),
            seq,
            0,
            strike_1e6,
            expiry_wall as i64,
        )
    };
    let book = |sym: SymbolId, ts: u64, bid_1e6: i64, ask_1e6: i64| {
        Tick::new(
            ts,
            VenueId::Hyperliquid,
            sym,
            0,
            Price::from_raw(bid_1e6),
            Qty::from_raw(100_000_000),
            Price::from_raw(ask_1e6),
            Qty::from_raw(100_000_000),
        )
    };

    let mut ctx = SinkCtx {
        n: 0,
        n_yes: 0,
        last: None,
        now: at(0),
        yes: [yes_sym(0), yes_sym(1), yes_sym(2)],
    };

    // Measured.
    let g = AllocGuard::new();
    let mut s_rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        s_rng ^= s_rng << 13;
        s_rng ^= s_rng >> 7;
        s_rng ^= s_rng << 17;
        s_rng
    };
    let mut mark = BASE;
    let mut inst = [0u32; FAMS];
    let mut bound_at = [0u64; FAMS];
    // 0 = Yes decided, 1 = No decided, 2 = at the money behind 0.99 asks.
    let mut shape = [0u8; FAMS];
    let mut refuse_next = [0u8; FAMS];
    let mut refused = 0u64;
    let mut polls = 0u64;
    let mut s = 1u64;
    while s <= SECS {
        // Rolls: the outgoing instance settles, then the successor binds.
        let mut f = 0usize;
        while f < FAMS {
            if (s + f as u64 * STAGGER) % LIFE == 0 {
                let ts = at(s);
                ctx.now = ts;
                let k = inst[f];
                if k > 0 {
                    m.on_venue_event(&roll_ev(f, 5_000 + 1_000 * f as u32 + k - 1, 0, 0, true, ts), &mut ctx);
                }
                shape[f] = (k % 4) as u8 % 3;
                let strike = match shape[f] {
                    0 => mark[f] / 10 * 9,
                    1 => mark[f] / 10 * 11,
                    _ => mark[f],
                };
                m.on_venue_event(
                    &roll_ev(f, 5_000 + 1_000 * f as u32 + k, strike, WALL0 + (s + LIFE) * 1_000_000_000, false, ts),
                    &mut ctx,
                );
                refuse_next[f] = u8::from(k % 4 == 3);
                bound_at[f] = s;
                inst[f] = k + 1;
            }
            f += 1;
        }
        // Each underlying: its mark at +0.2 s, its perp BBO at +0.3 s.
        let mut u = 0usize;
        while u < FAMS {
            let step = BASE[u] / 8_000;
            mark[u] += (next() % (2 * step as u64 + 1)) as i64 - step;
            let ts = at(s) + 200_000_000;
            ctx.now = ts;
            m.on_venue_event(&mark_ev(u, mark[u], ts), &mut ctx);
            refused += answer(&mut m, &mut ctx, &mut refuse_next);
            let half = BASE[u] / 160_000;
            let ts = at(s) + 300_000_000;
            ctx.now = ts;
            m.on_tick(&book(under_sym(u), ts, mark[u] - half, mark[u] + half), &mut ctx);
            refused += answer(&mut m, &mut ctx, &mut refuse_next);
            u += 1;
        }
        // The legs' polls, every 2 s through the window (τ from 29.6 s
        // down to 1.6 s — the last one under the 2 s floor).
        let mut f = 0usize;
        while f < FAMS {
            let e = s.wrapping_sub(bound_at[f]);
            if inst[f] > 0 && (90..=118).contains(&e) && e % 2 == 0 {
                let ts = at(s) + 400_000_000;
                ctx.now = ts;
                let (yb, ya, nb, na) = match shape[f] {
                    0 => (800_000, 850_000, 150_000, 200_000),
                    1 => (150_000, 200_000, 800_000, 850_000),
                    _ => (10_000, 990_000, 10_000, 990_000),
                };
                m.on_tick(&book(yes_sym(f), ts, yb, ya), &mut ctx);
                refused += answer(&mut m, &mut ctx, &mut refuse_next);
                m.on_tick(&book(yes_sym(f) + 1, ts, nb, na), &mut ctx);
                refused += answer(&mut m, &mut ctx, &mut refuse_next);
                polls += 1;
            }
            f += 1;
        }
        if s % 10 == 0 {
            let ts = at(s) + 500_000_000;
            ctx.now = ts;
            m.on_timer(ts, &mut ctx);
            refused += answer(&mut m, &mut ctx, &mut refuse_next);
        }
        s += 1;
    }
    let counters = m.bin15_counters();
    std::hint::black_box((ctx.n, counters));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(polls > 1_000, "the legs polled through every window: {polls}");
    assert!(counters.late_reprices > 2_000, "the late pricer ran: {counters:?}");
    assert!(counters.late_skipped_stale > 0, "the σ-cold first windows held");
    assert!(counters.late_skipped_persist > 0, "one poll is not two");
    assert!(counters.late_skipped_price > 0, "the 0.99 asks were refused");
    assert!(counters.late_submitted > 30, "real late entries: {counters:?}");
    assert!(refused > 0, "and gateway refusals that re-armed the entry");
    assert_eq!(
        counters.late_filled + refused,
        counters.late_submitted,
        "every late order was answered once"
    );
    assert_eq!(ctx.n, counters.late_submitted, "every order on the tape was the late law's");
    assert_eq!(counters.takes_submitted, counters.late_submitted, "every take is a late take");
    assert!(
        ctx.n_yes > 0 && ctx.n_yes < ctx.n,
        "Yes AND No entries: {} of {}",
        ctx.n_yes,
        ctx.n
    );
    assert_eq!(allocs, 0, "the bin15 late entry allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "the bin15 late entry's hot bytes should be zero: saw {bytes}");
}

/// E1 gate 53 — the execution router's steady state allocates nothing.
///
/// `RoutedDispatcher::submit` is on the engine's hot path: every order
/// every member emits passes through it. It must cost one masked byte
/// load from the route table, one branch and one delegated call — and
/// zero bytes.
///
/// The loop below exercises all four routing outcomes in the same
/// measurement window, because the refusal paths are the ones that
/// could plausibly allocate (they build no message today, and this is
/// what keeps it that way):
///
/// * a PAPER slot -> the paper matcher;
/// * a LIVE slot on its own venue -> the live arm;
/// * a LIVE slot on a venue it has no route to -> refused
///   (`NoLiveRoute`, LAW E-1 — never a paper fallback);
/// * an OFF slot -> refused (`SlotDisabled`);
/// * plus `observe_tick`, `try_next_fill`, `matcher_counters` and
///   `open_paper_orders`, which every 5 s metrics tick calls.
///
/// `stats()` is deliberately OUTSIDE the guard: `DispatchStats::merged`
/// is a cold-path field-wise sum called once per 5 s tick, not per
/// order, and holding it to the hot-path budget would be measuring the
/// wrong thing.
#[test]
fn routed_dispatch_steady_state() {
    use clob_dispatcher::{DispatchError, OrderDispatch, PaperDispatcher};
    use core_types::{Price, Qty, Side, Tick, VenueId, STRATEGY_ID_NONE};
    use exec_router::{ExecMode, ExecRoute, HaltLimits, NullLiveDispatcher, RoutedDispatcher, SlotCaps};

    const SYM: core_types::SymbolId = 42;

    // Boot-time construction — outside the measurement window, as the
    // engine's own boot is.
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            3,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            // The shipped template's numbers. E6's clamps refuse a
            // zero cap, and a gate whose submits were all refused
            // would measure the refusal path, not the dispatch path.
            SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
            HaltLimits::none(),
        )
        .expect("boot: slot 3 live");
    route
        .set_slot(6, ExecMode::Off, &[], SlotCaps::none(), HaltLimits::none())
        .expect("boot: slot 6 off");

    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        NullLiveDispatcher::new(),
        core_time::WallAnchor::now(),
    );
    d.mark_ledger_seeded();

    // Warm the paper matcher's open table so `observe_tick` has real
    // work to do inside the guard rather than walking an empty list.
    let mut warm = 0u64;
    while warm < 16 {
        let mut o = core_types::Order::new(
            1_000 + warm,
            VenueId::Polymarket,
            SYM,
            Side::Bid,
            0,
            Price::from_raw(400_000),
            Qty::from_raw(1_000_000),
            warm,
        );
        o.strategy_id = 0;
        let _ = d.submit(&o);
        warm += 1;
    }

    let g = AllocGuard::new();

    let mut paper = 0u64;
    let mut live = 0u64;
    let mut no_route = 0u64;
    let mut off = 0u64;
    let mut fills = 0u64;
    let mut open_acc = 0u64;

    let mut i = 0u64;
    while i < 10_000 {
        // Cycle the four outcomes deterministically.
        let (slot, venue) = match i & 3 {
            0 => (0u8, VenueId::Polymarket),          // paper
            1 => (3u8, VenueId::Hyperliquid),         // live -> null arm
            2 => (3u8, VenueId::Binance),             // live, no route
            _ => (6u8, VenueId::Hyperliquid),         // off
        };
        let mut o = core_types::Order::new(
            2_000 + i,
            venue,
            SYM,
            if i & 1 == 0 { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(400_000 + (i as i64 % 1_000)),
            Qty::from_raw(1_000_000),
            1_000 + i,
        );
        o.strategy_id = slot;
        match d.submit(&o) {
            Ok(()) => paper += 1,
            Err(DispatchError::NoLiveRoute) => {
                // Both the "wrong venue" case and the null live arm
                // land here in E1 — the arm refuses everything.
                if venue == VenueId::Binance {
                    no_route += 1;
                } else {
                    live += 1;
                }
            }
            Err(DispatchError::SlotDisabled) => off += 1,
            Err(e) => panic!("unexpected dispatch error {e:?}"),
        }

        // An un-stamped order every 16th pass: the fail-closed path.
        if i % 16 == 0 {
            let mut u = o;
            u.strategy_id = STRATEGY_ID_NONE;
            let _ = d.submit(&u);
        }

        // The tick the paper matcher judges its open table against.
        let t = Tick::new(
            2_000 + i,
            VenueId::Polymarket,
            SYM,
            i as u32,
            Price::from_raw(399_000),
            Qty::from_raw(1_000_000),
            Price::from_raw(401_000),
            Qty::from_raw(1_000_000),
        );
        d.observe_tick(&t, 2_000 + i);
        while let Some(f) = d.try_next_fill() {
            fills += 1;
            std::hint::black_box(f.order_id);
        }
        open_acc = open_acc.wrapping_add(d.open_paper_orders() as u64);
        std::hint::black_box(d.matcher_counters().intake);
        std::hint::black_box(d.exec_counters().live_submits);
        i += 1;
    }
    std::hint::black_box((paper, live, no_route, off, fills, open_acc));

    let (allocs, bytes, _deallocs) = g.delta();

    // Every routing outcome must actually have been exercised, or the
    // zero below would be measuring a path that never ran.
    assert!(paper > 0, "the paper arm must have taken orders");
    assert!(live > 0, "the live arm must have been reached");
    assert!(no_route > 0, "LAW E-1's refusal path must have fired");
    assert!(off > 0, "the off-slot refusal must have fired");

    assert_eq!(
        allocs, 0,
        "RoutedDispatcher steady state allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "RoutedDispatcher hot bytes should be zero: saw {bytes}"
    );
}

/// **XMM XH4h S3 — the agent's shared nonce stream draws at 0 B/op.**
/// Every signer of one agent address (the gateway's thread, the blocking
/// arms) draws through a [`exec_hyperliquid::NonceSource`]; the shared
/// variant is one compare-and-swap on an `Arc`'d cache line made at boot.
/// The registry and the `Arc` are boot work, outside the window.
#[test]
fn hl_shared_nonce_draws_are_zero_alloc() {
    use exec_hyperliquid::{NonceSource, NonceStreams};

    let mut streams = NonceStreams::new();
    let agent = [0xE3; 20];
    let mut gateway = NonceSource::own();
    let mut arm = NonceSource::own();
    gateway.share(streams.stream(&agent).expect("a stream"));
    arm.share(streams.stream(&agent).expect("the same stream"));
    let mut own = NonceSource::own();

    let g = AllocGuard::new();
    let mut acc = 0u64;
    for i in 0..10_000u64 {
        let ms = 1_790_000_000_000 + i / 3;
        acc = acc.wrapping_add(gateway.next(ms));
        acc = acc.wrapping_add(arm.next(ms));
        acc = acc.wrapping_add(own.next(ms));
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "nonce draws allocated {allocs} times ({bytes} B)"
    );
    assert!(gateway.last() == arm.last() && gateway.is_shared());
}

/// E2 gate 54 — encoding AND signing a Hyperliquid action allocates
/// nothing.
///
/// This is the live order path: every order the member emits is built
/// into a stack buffer, msgpack-encoded, keccak-hashed with its nonce
/// and vault tail, wrapped in the `Agent` EIP-712 envelope and signed.
/// All of it runs on the dispatcher worker thread, per order.
///
/// The signing key is parsed ONCE outside the guard, exactly as the
/// dispatcher parses it once at boot — `secp256k1`'s context is cached
/// behind a `OnceLock`, so the first signature of the process warms it
/// and every later one is allocation-free. The warm-up call below is
/// what makes that explicit rather than accidental.
#[test]
fn hl_action_encode_sign() {
    use exec_hyperliquid::action::{
        encode_batch_modify, encode_cancel, encode_cancel_by_cloid, encode_order,
        CancelByCloidWire, CancelWire, ModifyWire, OrderWire, Tif, MAX_ACTION,
    };
    use exec_hyperliquid::sign::{sign_action, Network, Vault};
    use exec_hyperliquid::Nonce;

    const KEY: [u8; 32] = [0x11; 32];
    const HIP4_YES: u32 = 100_032_530;
    const HIP4_NO: u32 = 100_032_531;

    // Boot-time work, outside the measurement window — as it is live.
    let sk = signer_eip712::parse_secret_key(&KEY).expect("key");
    let mut buf = [0u8; MAX_ACTION];
    let mut nonce = Nonce::new();

    // Warm the cached secp256k1 signing context and the cached EIP-712
    // domain separator. Both are `OnceLock`s initialised on first use;
    // measuring that first use would be measuring boot, not the hot
    // path.
    {
        let n = encode_order(
            &mut buf,
            &[OrderWire::new(HIP4_YES, true, 50_000_000, 100_000_000, Tif::Ioc)],
            b"na",
        )
        .expect("warm encode");
        let _ = sign_action(&sk, &buf[..n], 1, Vault::None, None, Network::Mainnet)
            .expect("warm sign");
    }

    let cloid = [
        0x4d, 0x56, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2a,
    ];

    let g = AllocGuard::new();

    let mut sigs: u64 = 0;
    let mut bytes: u64 = 0;
    let mut i = 0u64;
    while i < 2_000 {
        let now_ms = 1_789_000_000_000 + i;

        // Arm A / coverage: an IoC take with a client id.
        let n = encode_order(
            &mut buf,
            &[
                OrderWire::new(HIP4_YES, true, 45_670_000 + (i as i64 % 100), 100_000_000, Tif::Ioc)
                    .with_cloid(cloid),
            ],
            b"na",
        )
        .expect("encode order");
        let sig = sign_action(
            &sk,
            &buf[..n],
            nonce.next(now_ms),
            Vault::None,
            None,
            Network::Mainnet,
        )
        .expect("sign order");
        bytes = bytes.wrapping_add(n as u64);
        sigs = sigs.wrapping_add(u64::from(sig[64]));

        // Arm B: a post-only maker quote, then the requote that LAW E-7
        // says must be a MODIFY rather than a cancel plus a place.
        let n = encode_order(
            &mut buf,
            &[OrderWire::new(HIP4_NO, false, 60_000_000, 100_000_000, Tif::Alo).with_cloid(cloid)],
            b"na",
        )
        .expect("encode quote");
        let sig = sign_action(
            &sk,
            &buf[..n],
            nonce.next(now_ms),
            Vault::None,
            None,
            Network::Mainnet,
        )
        .expect("sign quote");
        bytes = bytes.wrapping_add(n as u64);
        sigs = sigs.wrapping_add(u64::from(sig[64]));

        let n = encode_batch_modify(
            &mut buf,
            &[ModifyWire {
                order: OrderWire::new(HIP4_NO, false, 60_100_000, 100_000_000, Tif::Alo)
                    .with_cloid(cloid),
                oid: 0,
                oid_cloid: cloid,
                oid_is_cloid: true,
            }],
        )
        .expect("encode modify");
        let sig = sign_action(
            &sk,
            &buf[..n],
            nonce.next(now_ms),
            Vault::None,
            None,
            Network::Testnet,
        )
        .expect("sign modify");
        bytes = bytes.wrapping_add(n as u64);
        sigs = sigs.wrapping_add(u64::from(sig[64]));

        // The roll's cancel-all, both forms, and the vault + expiry
        // tails so no branch of the hash builder escapes the guard.
        if i % 8 == 0 {
            let n = encode_cancel(
                &mut buf,
                &[
                    CancelWire { asset: HIP4_YES, oid: i },
                    CancelWire { asset: HIP4_NO, oid: i + 1 },
                ],
            )
            .expect("encode cancel");
            let sig = sign_action(
                &sk,
                &buf[..n],
                nonce.next(now_ms),
                Vault::Address([9u8; 20]),
                Some(now_ms + 60_000),
                Network::Mainnet,
            )
            .expect("sign cancel");
            bytes = bytes.wrapping_add(n as u64);
            sigs = sigs.wrapping_add(u64::from(sig[64]));

            let n = encode_cancel_by_cloid(
                &mut buf,
                &[CancelByCloidWire { asset: HIP4_YES, cloid }],
            )
            .expect("encode cancel-by-cloid");
            let sig = sign_action(
                &sk,
                &buf[..n],
                nonce.next(now_ms),
                Vault::None,
                Some(now_ms + 60_000),
                Network::Mainnet,
            )
            .expect("sign cancel-by-cloid");
            bytes = bytes.wrapping_add(n as u64);
            sigs = sigs.wrapping_add(u64::from(sig[64]));
        }
        i += 1;
    }
    std::hint::black_box((sigs, bytes, nonce.last()));

    let (allocs, alloc_bytes, _deallocs) = g.delta();
    assert!(bytes > 0, "the gate must have encoded something");
    assert!(sigs > 0, "and signed something");
    assert_eq!(
        allocs, 0,
        "hl action encode+sign allocated {allocs} times ({alloc_bytes} B)"
    );
    assert_eq!(
        alloc_bytes, 0,
        "hl action encode+sign bytes should be zero: saw {alloc_bytes}"
    );
}

/// Gate 58 (E3): the Hyperliquid exchange arm's per-order cycle.
///
/// (Numbered 58 at the E7 review: the E3 session labelled this gate 54
/// — a duplicate of E2's — and E4 then numbered its own 55–57 on top,
/// so 58 was the label the sequence 53..62 was missing. The E3/E4
/// session logs keep their original numbers; they are history.)
///
/// The socket between them — `HlHttp::post` — is gate 101's since XMM
/// HD2 (rustls' two allocations per post, against gate 72's
/// child-process node), and the TLS loopback test covers its behaviour.
/// What this gate pins — and what actually runs per order on either side
/// of the socket — is building the signed JSON body, and scanning the
/// venue's answer.
///
/// Both are supposed to be pure index arithmetic over caller-owned
/// buffers. `scan` in particular returns `Span` OFFSETS rather than
/// owned bytes precisely so that reading a rejection message costs
/// nothing; this assertion is what stops a later "just return a
/// String, it's only the error path" from landing unnoticed — the
/// error path is the one that runs when the venue is having a bad day
/// and we are sending the most orders.
#[test]
fn hl_exchange_encode_and_scan_are_zero_alloc() {
    use exec_hyperliquid::action::{
        encode_cancel, encode_noop, encode_order, encode_reserve_weight, encode_schedule_cancel,
        encode_update_leverage, CancelWire, OrderWire, Tif, MAX_ACTION,
    };
    use exec_hyperliquid::request::{
        cancel_json, envelope, noop_json, order_json, reserve_weight_json, schedule_cancel_json,
        update_leverage_json,
    };
    use exec_hyperliquid::response::{scan, HlResponse};

    const HIP4_YES: u32 = 100_000_000 + 10 * 3253;

    let orders = [OrderWire::new(HIP4_YES, true, 48_000_000, 2_500_000_000, Tif::Alo)];
    let cancels = [CancelWire {
        asset: HIP4_YES,
        oid: 987_654_321,
    }];
    let sig = [0x12u8; 65];

    // The three answers the venue actually sends, including the two
    // that look like successes and are not.
    let ack: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":77216390}}]}}}"#;
    let item_err: &[u8] = br#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order must have minimum value of $10."}]}}}"#;
    let top_err: &[u8] = br#"{"status":"err","response":"Unable to recover signer."}"#;
    // S7-L1: the request-weight top-up's answer.
    let default_ok: &[u8] = br#"{"status":"ok","response":{"type":"default"}}"#;

    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mut body = [0u8; exec_hyperliquid::MAX_REQ_BODY];

    // Prime every path once — one-time setup must not be counted.
    let n = encode_order(&mut mp, &orders, b"na").unwrap();
    let m = order_json(&mut aj, &orders, b"na").unwrap();
    let _ = envelope(&mut body, &aj[..m], 1, &sig, None, None).unwrap();
    std::hint::black_box(n);
    let _ = scan(ack).unwrap();

    let g = AllocGuard::new();
    let mut acc: usize = 0;
    for i in 0..10_000u32 {
        let n = encode_order(&mut mp, &orders, b"na").unwrap();
        let m = order_json(&mut aj, &orders, b"na").unwrap();
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, None).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);

        let n = encode_cancel(&mut mp, &cancels).unwrap();
        let m = cancel_json(&mut aj, &cancels).unwrap();
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, None).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);

        // S7-L1: the request-weight top-up, the same three steps.
        let n = encode_reserve_weight(&mut mp, 5_000).unwrap();
        let m = reserve_weight_json(&mut aj, 5_000).unwrap();
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, None).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);

        // XH4c: the perp arm's control actions, the same three steps —
        // the dead man's scheduleCancel in both shapes, with its
        // `expiresAfter`.
        let n = encode_update_leverage(&mut mp, 1, true, 3).unwrap();
        let m = update_leverage_json(&mut aj, 1, true, 3).unwrap();
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, None).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);
        let time = if i % 2 == 0 {
            Some(1_789_000_005_000)
        } else {
            None
        };
        let n = encode_schedule_cancel(&mut mp, time).unwrap();
        let m = schedule_cancel_json(&mut aj, time).unwrap();
        let expires = Some(1_789_000_000_000 + u64::from(i));
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, expires).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);
        let n = encode_noop(&mut mp).unwrap();
        let m = noop_json(&mut aj).unwrap();
        let e = envelope(&mut body, &aj[..m], i as u64, &sig, None, None).unwrap();
        acc = acc.wrapping_add(n).wrapping_add(m).wrapping_add(e);

        // Scanning is per-order too, and the error paths most of all.
        for bytes in [ack, item_err, top_err, default_ok] {
            match scan(bytes) {
                Ok(HlResponse::Ok(ok)) => {
                    acc = acc.wrapping_add(ok.statuses as usize);
                    acc = acc.wrapping_add(ok.first_error.of(bytes).len());
                }
                Ok(HlResponse::Err { msg }) => acc = acc.wrapping_add(msg.of(bytes).len()),
                Err(_) => acc = acc.wrapping_add(1),
            }
        }
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "hl exchange encode/scan allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl exchange encode/scan bytes should be zero: saw {bytes}");
}

/// Gate 55 (E4): the live fill lane's per-fill work.
///
/// Everything here runs once per venue fill, on the dispatcher worker
/// thread that also owns the exchange socket. A `String` on this path
/// would allocate on every print the venue sends, and the moments that
/// produce the most prints are the moments the engine can least afford
/// it — a sweeping IoC answers with several.
///
/// `scan_user_fills` returning `Span` offsets rather than owned coin
/// names is what makes that possible, and this is the assertion that
/// stops a later "just return a String, it's only the coin name".
#[test]
fn hl_user_fill_lane_is_zero_alloc() {
    use exec_hyperliquid::cloid::{decode as decode_cloid, encode as encode_cloid};
    use exec_hyperliquid::recon::{scan_spot_state, SpotBalance};
    use exec_hyperliquid::userws::{owner_of, scan_user_fills, to_fill, TidRing, UserFill};
    use exec_hyperliquid::AddressBudget;

    // `#<enc>` is the FILL namespace; the STATE body below keeps
    // `+<enc>`, which is the BALANCE namespace. Both measured.
    const FILLS: &[u8] = br##"{"channel":"userFills","data":{"isSnapshot":false,"user":"0xabc","fills":[{"coin":"#32530","px":"0.47","sz":"25","side":"B","time":1757942400000,"oid":77216390,"tid":9001,"fee":"0.0123","cloid":"0x4d560300000000000000000012345678"},{"coin":"#32540","px":"0.53","sz":"25","side":"A","time":1757942400001,"oid":77216391,"tid":9002,"fee":"0.0"},{"coin":"#32540","px":"1.0","sz":"25","side":"A","time":1757942400002,"oid":77216392,"tid":9003,"dir":"Settlement","fee":"0.0"}]}}"##;
    const STATE: &[u8] = br#"{"balances":[{"coin":"USDC","token":0,"total":"1234.56","hold":"12.00"},{"coin":"+3253","token":107,"total":"10.00000001","hold":"0.0"}]}"#;

    let mut fills = [UserFill::default(); 8];
    let mut bal = [SpotBalance::default(); 8];
    let mut ring: TidRing<{ exec_hyperliquid::userws::SNAPSHOT_RING }> = TidRing::new();
    let mut budget = AddressBudget::restored([0xAB; 20], 100, 0, 0);

    // Prime every path once.
    let _ = scan_user_fills(FILLS, &mut fills).unwrap();
    let _ = scan_spot_state(STATE, &mut bal).unwrap();
    let _ = decode_cloid(&encode_cloid(3, 1));

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for i in 0..10_000u64 {
        let (n, _snap) = scan_user_fills(FILLS, &mut fills).unwrap();
        for f in &fills[..n] {
            // Attribution, dedupe, conversion, budget — the whole
            // per-fill cycle.
            let _ = owner_of(f);
            let fresh = ring.admit(f.tid ^ i);
            if let Ok(r) = to_fill(f, 7, i) {
                let fill = r.fill();
                acc = acc.wrapping_add(fill.qty.raw()).wrapping_add(fill.strategy_id as i64);
                acc = acc.wrapping_add(r.for_lane().is_some() as i64);
            }
            if fresh {
                budget.on_venue_fill(f.notional_usdc_1e6());
            }
            acc = acc.wrapping_add(f.coin.of(FILLS).len() as i64);
        }
        budget.on_action_sent(1);
        acc = acc.wrapping_add(budget.remaining());

        let n = scan_spot_state(STATE, &mut bal).unwrap();
        for b in &bal[..n] {
            acc = acc.wrapping_add(b.free_1e8());
            acc = acc.wrapping_add(b.coin.of(STATE).len() as i64);
        }

        let c = encode_cloid((i & 7) as u8, i);
        if let exec_hyperliquid::cloid::Owner::Ours { strategy_id, .. } = decode_cloid(&c) {
            acc = acc.wrapping_add(strategy_id as i64);
        }
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "hl user-fill lane allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl user-fill lane bytes should be zero: saw {bytes}");
}

/// E4 gate 56 — routing a user-fill frame through the LIVE ARM's own
/// router allocates nothing.
///
/// Gate 55 measures the fill lane's pieces. This measures
/// [`HlExchange::route_frame`], the function the socket's pump
/// actually calls — the distinction is not pedantic. The audit that
/// prompted this gate found the production path staging every frame
/// into a heap `Vec` before routing it, on the thread that also signs
/// and submits, while gate 55 sat green the whole time: it never
/// touched `HlExchange`. A gate that measures a lookalike is a gate
/// that reports on code nobody runs.
///
/// The frame is SNAPSHOT-SIZED (400 rows) because the reconnect
/// snapshot is where a per-frame allocation hurts most and where the
/// scratch buffer is under real pressure.
#[test]
fn hl_exchange_route_frame_is_zero_alloc() {
    use core_ring::Ring;
    use core_types::Fill;
    use exec_hyperliquid::asset::AssetTable;
    use exec_hyperliquid::exchange::{HlExchange, HlExecCounters};
    use exec_hyperliquid::userws::{TidRing, UserFill, SNAPSHOT_RING};
    use exec_hyperliquid::AddressBudget;

    // Built ONCE, outside the guard — 400 rows of venue JSON.
    // `base_tid` shifts the tid range so two frames can be disjoint.
    fn venue_frame(base_tid: u64) -> Vec<u8> {
        let mut f =
            String::from(r#"{"channel":"userFills","data":{"isSnapshot":false,"fills":["#);
        for i in 0..400u64 {
            if i > 0 {
                f.push(',');
            }
            f.push_str(&format!(
                // OUR cloid (magic 'M','V', slot 3) — without it the
                // rows route as foreign and never reach the lane, so
                // the measured region would stop at a counter.
                r##"{{"coin":"#32530","px":"0.47","sz":"25","side":"B","time":1757942400000,"oid":{},"tid":{},"fee":"0.01","cloid":"0x4d560300000000000000000012345678"}}"##,
                base_tid + i,
                base_tid + i
            ));
        }
        // Two SETTLEMENT rows — the venue's own, cloid-less, at 1.0
        // for the winner and 0.0 for the loser. They take the
        // attribute-by-symbol branch, which is otherwise unmeasured.
        // Their tids ride `base_tid` too, or the priming frame and the
        // measured frame would share them and the dedupe ring would
        // eat the second pair — correctly, which is how the first
        // version of this fixture came out two short.
        for k in 0..2u64 {
            f.push_str(&format!(
                r##",{{"coin":"#32530","px":"{px}","sz":"25","side":"A","time":1757942400000,"oid":{t},"tid":{t},"dir":"Settlement","fee":"0.0"}}"##,
                px = if k == 0 { "1.0" } else { "0.0" },
                t = base_tid + 900_000 + k,
            ));
        }
        f.push_str("]}}");
        f.into_bytes()
    }
    let frame = venue_frame(1);
    let prime = venue_frame(1_000_000);

    // The coin IS bound, so the measured region runs the whole book
    // path — `to_fill`, the cloid attribution, `try_push_ref` into the
    // lane and `on_venue_fill` against the budget. Until the symbol
    // binding existed this was unreachable and the gate could only
    // measure as far as the unresolved counter.
    let mut assets = AssetTable::new();
    assets
        .bind(7, AssetTable::asset_id(3253, 0).expect("in range"), 1, b"#32530")
        .expect("bind");
    // An owner, so the SETTLEMENT rows below take the attribute-by-
    // symbol branch rather than stopping at `fills_unowned`. Without
    // this the branch is in the fixture and not in the measurement.
    assets.note_owner(7, 3);

    // Every buffer preallocated, exactly as `HlExchange::new` does it.
    let mut scratch: Vec<UserFill> = vec![UserFill::default(); SNAPSHOT_RING];
    let mut seen: TidRing<SNAPSHOT_RING> = TidRing::new();
    let mut budget = AddressBudget::restored([0xAB; 20], 1_000_000, 0, 0);
    let (mut fills, _c) = Ring::<Fill, 1024>::new().split();
    let mut counters = HlExecCounters::default();

    // Prime with a DIFFERENT frame: the first pass through any code
    // is the one allowed to be cold, but priming with `frame` itself
    // would fill `seen` with its tids and every measured pass would
    // then short-circuit at the dedupe — a gate covering one branch
    // while its comment claimed two.
    //
    // Priming with disjoint tids instead means measured iteration 0
    // takes the FRESH path (scan, admit, budget credit, per-row
    // routing) and 1..200 take the dedupe path. Both are measured.
    // What is NOT reachable from here is `to_fill`/`try_push_ref`: today
    // `resolve_sym` is a `None` stub, so no row can reach the lane.
    // Gate 55 measures those functions directly.
    let _ = HlExchange::<1024>::route_frame(
        &prime,
        1,
        &mut assets,
        &mut scratch,
        &mut seen,
        &mut budget,
        &mut fills,
        &mut counters,
    );

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for i in 0..200u64 {
        acc = acc.wrapping_add(HlExchange::<1024>::route_frame(
            &frame,
            i,
            &mut assets,
            &mut scratch,
            &mut seen,
            &mut budget,
            &mut fills,
            &mut counters,
        ) as i64);
        // A frame from another channel takes the early-return branch.
        acc = acc.wrapping_add(HlExchange::<1024>::route_frame(
            br#"{"channel":"orderUpdates","data":[]}"#,
            i,
            &mut assets,
            &mut scratch,
            &mut seen,
            &mut budget,
            &mut fills,
            &mut counters,
        ) as i64);
    }
    std::hint::black_box(acc);
    std::hint::black_box(&counters);

    let (allocs, bytes, _deallocs) = g.delta();

    // The comment above claims the measured region covers BOTH the
    // fresh-tid path and the dedupe path, and that the rows reach the
    // LANE rather than stopping at a counter. Pin both: 400 rows book
    // on the priming frame and 400 on measured iteration 0, and every
    // later iteration must short-circuit at the dedupe and add
    // nothing. A gate's own coverage claim is worth exactly as much as
    // the assertion that holds it.
    assert_eq!(
        counters.fills_booked, 804,
        "expected 400 primed + 400 fresh rows, plus 2 settlements each, BOOKED and then \
         pure dedupe; saw {}",
        counters.fills_booked
    );
    assert_eq!(
        counters.fills_settlement, 4,
        "the settlement branch must have been INSIDE the measured region too"
    );
    assert_eq!(counters.fills_unowned, 0, "the leg has an owner");
    assert_eq!(
        counters.fills_unresolved, 0,
        "the coin is bound; nothing should have failed to resolve"
    );
    assert_eq!(counters.fills_dropped, 0, "the lane is 1024 and took 800");

    assert_eq!(
        allocs, 0,
        "hl exchange route_frame allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl exchange route_frame bytes should be zero: saw {bytes}");
}

/// E4 gate 57 — the roll hook allocates nothing, on EITHER path.
///
/// `OrderDispatch::on_venue_event` is called for **every** venue event
/// on every lane, from the engine tick loop — funding and mark flow
/// continuously, and only a handful of events a day are rolls. So the
/// early-return path is the hot one and the binding path is the rare
/// one, and both are measured here: a table that allocated while
/// binding would do it at every quarter-hour roll, on the thread that
/// also signs and submits.
#[test]
fn hl_exchange_roll_hook_is_zero_alloc() {
    use core_ring::Ring;
    use core_types::{ChannelEvent, ChannelId, Fill, VenueId};
    use exec_hyperliquid::config::{HlConfig, Scope};
    use exec_hyperliquid::exchange::HlExchange;
    use clob_dispatcher::OrderDispatch;

    fn roll(outcome: u32, settled: bool, sym: u32) -> ChannelEvent {
        let seq = u64::from(outcome) | (60u64 << 32) | ((settled as u64) << 56);
        ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym,
            seq,
            0,
            1_000_000,
            2_000_000_000,
        )
    }
    // The event the hook sees thousands of times for every roll.
    let funding = ChannelEvent::new(
        1,
        VenueId::Hyperliquid,
        ChannelId::Funding,
        7,
        0,
        0,
        125,
        0,
    );

    let cfg = HlConfig::new(
        Scope::Testnet,
        exec_hyperliquid::config::HOST_TESTNET,
        'b',
        [0x55; 32],
        [0x66; 20],
    )
    .expect("cfg");
    let (p, _c) = Ring::<Fill, 64>::new().split();
    let mut x = HlExchange::<64>::new(
        &cfg,
        core_net::TlsTransport::default_client_config(),
        p,
        std::env::temp_dir().join(format!("mv-gate57-{}.state", std::process::id())),
        100,
    )
    .expect("build");

    // Prime both paths once; the first pass through any code is the
    // one allowed to be cold.
    x.on_venue_event(&roll(19_418, false, 4096));
    x.on_venue_event(&funding);

    let g = AllocGuard::new();
    for i in 0..2_000u32 {
        // The hot path: not a roll, not ours — returns immediately.
        x.on_venue_event(&funding);
        // The rare path: a real roll, binding both legs. Rebinding the
        // SAME symbols in place is what a quarter-hour roll does.
        x.on_venue_event(&roll(19_418 + (i & 7), false, 4096));
        x.on_venue_event(&roll(19_418 + (i & 7), true, 4096));
    }
    std::hint::black_box(x.counters());

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "hl exchange roll hook allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl exchange roll hook bytes should be zero: saw {bytes}");
    assert!(
        x.counters().rolls_bound >= 2_000,
        "the binding path must actually have run: {}",
        x.counters().rolls_bound
    );
    assert_eq!(x.counters().rolls_refused, 0);
}

/// **E5 gate 60 — the per-REQUOTE path through the exchange arm.**
///
/// §7.1's exit gate asks for a modify entry at 0 B/op.
/// `hl_action_encode_sign` already pins the raw encoders; this one
/// goes through `HlExchange`'s OWN `modify` and `cancel_by_cloid`,
/// which is what actually runs when Arm B reprices — asset lookup,
/// the scale guards, the cloid builder, the wire structs, msgpack and
/// JSON — plus `seal`, the nonce/signature/envelope half of
/// `send_action`.
///
/// **It cannot cross the socket**, and neither could gate 59, which
/// split `compare` out for exactly this reason and said why: a path
/// that has only ever run behind an HTTPS round trip is a claim about
/// source code. So the two halves either side of the socket are
/// driven directly:
///
/// * `modify` runs with the budget's floor at `u64::MAX`, so
///   `send_action` refuses at its barrier BEFORE any network work —
///   the encode half runs, the post does not;
/// * `stage_modify` + `seal` then run the SAME encode half again and
///   sign the bytes it produced, in the arm's own boot buffers. Since
///   the E7 zero-copy pass the request body is rendered in place
///   (`envelope_open` / action JSON / `envelope_close`), so there is
///   no caller-side body buffer to hand in — and nothing here is a
///   lookalike of the path that trades.
///
/// Between them that is every instruction a requote executes on the
/// engine thread.
///
/// **`cancel_by_cloid` is deliberately NOT driven here.** It spends
/// `Spend::Cancel`, and `may_cancel()` is unconditionally true —
/// a halted engine must be able to flatten — so no budget setting can
/// bar it, and driving it would have this gate open two thousand
/// sockets to the live testnet. Its encode half is pinned by
/// `hl_action_encode_sign`, which drives `encode_cancel_by_cloid`
/// through `sign_action` directly. The assertion below that the
/// barrier really held is what keeps this gate honest about it.
#[test]
fn hl_exchange_requote_path_is_zero_alloc() {
    use clob_dispatcher::OrderDispatch;
    use core_ring::Ring;
    use core_types::{ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, VenueId};
    use exec_hyperliquid::config::{HlConfig, Scope};
    use exec_hyperliquid::exchange::HlExchange;

    const OUTCOME: u32 = 19_418;
    const SYM: u32 = 4096;

    fn roll(outcome: u32, sym: u32) -> ChannelEvent {
        let seq = u64::from(outcome) | (60u64 << 32);
        ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym,
            seq,
            0,
            1_000_000,
            2_000_000_000,
        )
    }

    fn quote(px: i64, oid_seq: u64) -> Order {
        // The instance rides the low 32 bits (`OID_INSTANCE_MASK`), so
        // the asset lookup can refuse an order naming a retired one —
        // LAW E-4. A requote's id must carry the LIVE instance.
        let client_oid = (oid_seq << 32) | u64::from(OUTCOME);
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            SYM,
            Side::Bid,
            0, // ORDER_KIND_MAKER
            Price::from_raw(px),
            Qty::from_raw(25_000_000),
            client_oid,
        );
        o.strategy_id = 3;
        o
    }

    let cfg = HlConfig::new(
        Scope::Testnet,
        exec_hyperliquid::config::HOST_TESTNET,
        'b',
        [0x57; 32],
        [0x58; 20],
    )
    .expect("cfg");
    let (p, _c) = Ring::<Fill, 64>::new().split();
    let mut x = HlExchange::<64>::new(
        &cfg,
        core_net::TlsTransport::default_client_config(),
        p,
        std::env::temp_dir().join(format!("mv-gate60-{}.state", std::process::id())),
        // The floor at the ceiling: every SUBMIT-spending verb is
        // refused at the barrier, so nothing reaches a socket. The
        // encode half still runs, which is the half being measured.
        u64::MAX,
    )
    .expect("build");
    // XH4h S3: as the engine boots it — the agent's shared nonce stream.
    x.share_nonce(
        exec_hyperliquid::NonceStreams::new()
            .stream(&[0x58; 20])
            .expect("a stream"),
    );
    x.on_venue_event(&roll(OUTCOME, SYM));

    // Prime: the first pass through the signing context and the
    // EIP-712 domain separator is boot, not the hot path.
    let _ = x.modify(&ModifyReq::new(1 << 32 | u64::from(OUTCOME), quote(470_000, 1)));
    {
        let (mp_n, end) = x
            .stage_modify(1 << 32 | u64::from(OUTCOME), &quote(470_000, 1))
            .expect("warm stage");
        let _ = x.seal(mp_n, end).expect("warm seal");
    }

    let g = AllocGuard::new();
    let mut sealed: u64 = 0;
    let mut i = 1u64;
    while i <= 2_000 {
        let prev = (i << 32) | u64::from(OUTCOME);
        let q = quote(470_000 + (i as i64 % 50) * 100, i + 1);
        // LAW E-7: the requote itself, through the TRAIT verb the
        // router calls (BX0-F3) — refused at the budget barrier, after
        // the encode half.
        let _ = x.modify(&ModifyReq::new(prev, q));
        // And the half `send_action` does after the barrier, before
        // the post: sign the bytes the encode half just rendered.
        let (mp_n, end) = x.stage_modify(prev, &q).expect("stage");
        let n = x.seal(mp_n, end).expect("seal");
        sealed = sealed.wrapping_add(n as u64);
        i += 1;
    }
    std::hint::black_box((sealed, x.counters()));

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "hl exchange requote path allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl exchange requote path bytes: saw {bytes}");
    assert!(sealed > 0, "the envelope half must actually have run");
    // And the verbs must really have reached their encode, not bounced
    // off the asset table — a gate over a lookup failure would measure
    // an early return and call it a requote.
    assert_eq!(
        x.counters().rolls_bound, 1,
        "the leg must be bound, or `modify` refuses before it encodes"
    );
    assert_eq!(
        x.counters().refused_stale, 0,
        "and the instance must be the live one"
    );
    // **THE assertion that keeps this gate off the network.** The
    // barrier is what stops `modify` before the socket, and it is a
    // property of `Spend::Submit` meeting a floor — not of anything
    // this test can see directly. If a future change routed the
    // requote through `Spend::Cancel`, which `may_cancel()` permits
    // unconditionally, this gate would quietly start opening two
    // thousand sockets to the live testnet. It would still pass on
    // allocations. So the refusal is counted, not assumed.
    assert_eq!(
        x.counters().modifies_sent,
        0,
        "nothing may reach the venue from an allocation gate"
    );
    assert!(
        x.counters().refused_local >= 2_000,
        "the budget barrier must have refused every one: {}",
        x.counters().refused_local
    );
}

/// E4 gate 59 — the reconciler's COMPARISON allocates nothing.
///
/// `reconcile()` itself needs a socket, so what is measured here is the
/// half that runs after the answer arrives: the scan into a
/// preallocated sheet and the per-leg walk. Split out as
/// `HlExchange::compare` precisely so it could be measured — a
/// comparison that has only ever run behind an HTTPS round trip is a
/// claim about source code, which is the failure this lane has already
/// made three times.
#[test]
fn hl_exchange_reconcile_compare_is_zero_alloc() {
    use exec_hyperliquid::asset::AssetTable;
    use exec_hyperliquid::exchange::HlExchange;
    use exec_hyperliquid::recon::{scan_spot_state, SpotBalance, MAX_SPOT_BALANCES};

    // The venue's own shape, BALANCE namespace, with the fourteen-row
    // padding a one-coin account really came back with.
    let mut sheet = String::from(r#"{"balances":[{"coin":"USDC","token":0,"total":"997.64","hold":"0.0"}"#);
    for i in 0..8u32 {
        // S7-L1: with the cost basis the at-cost account view reads.
        sheet.push_str(&format!(
            r#",{{"coin":"+{}","total":"2.0","hold":"0.0","entryNtl":"1.36"}}"#,
            194_180 + i
        ));
    }
    sheet.push_str("]}");
    let sheet = sheet.into_bytes();

    let mut assets = AssetTable::new();
    for f in 0..4u32 {
        let outcome = 19_418 + f;
        for side in 0u8..2 {
            let mut coin = [0u8; exec_hyperliquid::asset::COIN_MAX];
            let n = AssetTable::outcome_coin(outcome, side, &mut coin).expect("in range");
            assets
                .bind(
                    4096 + 2 * f + u32::from(side),
                    AssetTable::asset_id(outcome, side).expect("in range"),
                    u64::from(outcome),
                    &coin[..n],
                )
                .expect("bind");
            assets.book_qty(4096 + 2 * f + u32::from(side), 1_000_000);
        }
    }

    let mut bal = vec![SpotBalance::default(); MAX_SPOT_BALANCES];
    // Prime: the first pass through any code is the one allowed to be
    // cold.
    let n = scan_spot_state(&sheet, &mut bal).expect("scans");
    let _ = HlExchange::<64>::compare(&assets, &bal[..n], &sheet);

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..2_000u32 {
        let n = scan_spot_state(&sheet, &mut bal).expect("scans");
        let (legs, worst) = HlExchange::<64>::compare(&assets, &bal[..n], &sheet);
        acc = acc.wrapping_add(legs as i64).wrapping_add(worst);
        // S7-L1: the session bound's account view, at cost.
        let v = exec_hyperliquid::recon::account_view(&bal[..n], &sheet);
        acc = acc.wrapping_add(v.equity_at_cost_1e6());
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "hl reconcile compare allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl reconcile compare bytes should be zero: saw {bytes}");
    let n = scan_spot_state(&sheet, &mut bal).expect("scans");
    let v = exec_hyperliquid::recon::account_view(&bal[..n], &sheet);
    assert_eq!((v.legs, v.held_cost_1e6), (8, 8 * 1_360_000), "the cost basis was read");

    // S7-L1: the day-spend read's render and scan, over a page one row
    // short of the venue's limit — built outside the window.
    use exec_hyperliquid::dayspend::{fills_since_request, scan_day_bought, MAX_DAY_REQ, VENUE_PAGE_MAX};
    const DAY0_MS: u64 = 1_790_208_000_000;
    let mut page = String::from("[");
    for i in 0..(VENUE_PAGE_MAX - 1) {
        if i > 0 {
            page.push(',');
        }
        page.push_str(&format!(
            r##"{{"coin":"#194180","px":"0.5","sz":"4.0","side":"{}","time":{},"oid":{i},"tid":{i},"cloid":"0x4d560300000000000000000000000065","fee":"0.0"}}"##,
            if i % 2 == 0 { "B" } else { "A" },
            DAY0_MS + i as u64,
        ));
    }
    page.push(']');
    let page = page.into_bytes();
    let mut bought = [0i64; core_config::exec::EXEC_SLOTS];
    let mut req = [0u8; MAX_DAY_REQ];
    let _ = scan_day_bought(&page, DAY0_MS, &mut bought).expect("prime");

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    let mut rows = 0usize;
    for k in 0..20u64 {
        let n = fills_since_request(&mut req, &[0xAB; 20], DAY0_MS + k).expect("renders");
        rows = scan_day_bought(&page, DAY0_MS, &mut bought).expect("a complete page");
        acc = acc.wrapping_add(n as i64).wrapping_add(bought[3]);
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hl day-spend read allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hl day-spend read bytes should be zero: saw {bytes}");
    assert_eq!(rows, VENUE_PAGE_MAX - 1);
    assert_eq!(bought[3], 1_000 * 2_000_000, "a thousand $2 buys by slot 3");
    // And the comparison must actually have found the drift, or the
    // guard measured a walk that returned early.
    let n = scan_spot_state(&sheet, &mut bal).expect("scans");
    let (legs, worst) = HlExchange::<64>::compare(&assets, &bal[..n], &sheet);
    assert_eq!(legs, 8, "every leg disagrees: booked 1.0, venue says 2.0");
    assert_eq!(worst, 1_000_000);
}

/// **E6 gate 61 — the venue-fill exposure ledger, end to end.**
///
/// E6 commit 2 put a memory behind the risk gate: an exposure ledger,
/// a day-turnover counter and a resting-order table, fed by
/// `OrderDispatch::on_fill_booked` and by the `InstrumentRoll` events
/// the router already forwards. **All three run on the engine
/// thread** — `on_fill_booked` is called from the engine's fill-lane
/// drain, once per venue fill, before the member sees it — so every
/// one of them is a hot path and none may allocate.
///
/// What this drives inside the guard:
///
/// * `on_fill_booked` on a BOUND leg (the position moves, the
///   turnover moves, a resting order is decremented);
/// * `on_fill_booked` on an UNBOUND leg (the whole table is walked
///   and nothing matches — the worst case for the scan);
/// * `submit` under all four clamps, including refusals, so the two
///   ledger walks the gate does per order are measured;
/// * `cancel` and `modify`, which search the resting table;
/// * `on_venue_event` with created AND settled rolls, which bind,
///   clear an instance and drop that instance's resting orders;
/// * `slot_exposure_1e6` / `slot_day_turnover_1e6` / `slot_resting`,
///   which `/state` reads.
///
/// Every outcome is counted and asserted non-zero at the end. A gate
/// whose refusal path never fired would be asserting zero allocations
/// over a branch that never ran — which is how a zero-allocation
/// claim becomes agreement over an empty set.
#[test]
fn routed_ledger_steady_state() {
    use clob_dispatcher::{DispatchError, OrderDispatch, PaperDispatcher};
    use core_types::{
        CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, VenueId,
        FILL_ORIGIN_PAPER, FILL_ORIGIN_VENUE,
    };
    use exec_router::{ExecMode, ExecRoute, HaltLimits, RoutedDispatcher, SlotCaps};

    const SLOT: u8 = 3;
    /// Hyperliquid namespace. Yes legs are even ordinals, No odd.
    const fn sym(ord: u32) -> core_types::SymbolId {
        (4u32 << 24) | ord
    }
    /// 2026-09-19T00:00:01Z.
    const T0: u64 = 1_789_776_001_000_000_000;

    /// A live arm that ACCEPTS, so the ledger's lifecycle hooks are
    /// reached. `NullLiveDispatcher` refuses everything, which would
    /// leave the resting table empty and the fill matcher measuring
    /// nothing.
    struct Yes;
    impl OrderDispatch for Yes {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
    }

    let roll = |family: usize, outcome: u32, sym_yes: core_types::SymbolId, settled: bool| {
        ChannelEvent::new(
            T0,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym_yes,
            core_types::pack_roll_seq(outcome, 60, family, settled),
            0,
            0,
            0,
        )
    };

    // Boot-time construction — outside the window, as the engine's is.
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            SLOT as usize,
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            // Tight enough that the refusal paths really fire, loose
            // enough that the accepting paths do too.
            SlotCaps::new(100_000_000, 40_000_000, 200_000_000, 32),
            HaltLimits::none(),
        )
        .expect("boot: slot 3 live");
    // The identity anchor: this gate stamps orders and fills from one
    // `T0`, so mapping it to itself keeps both in one day epoch. The
    // conversion itself is held by `exec_router`'s own tests.
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        Yes,
        core_time::WallAnchor::new(T0, T0),
    );
    // What E6 commit 3's reconciler will do at boot: without it every
    // live PLACE is refused and the gate would be measuring the
    // interlock rather than the ledger.
    d.mark_ledger_seeded();

    // Fill the binding table, so every scan walks a FULL table rather
    // than bailing on the first free row.
    let mut o = 0u32;
    while o < exec_router::LEDGER_ROWS as u32 {
        d.on_venue_event(&roll(o as usize, 1_000 + o, sym(100 + o * 2), false));
        o += 1;
    }

    let g = AllocGuard::new();

    let mut placed = 0u64;
    let mut refused = 0u64;
    let mut cancelled = 0u64;
    let mut modified = 0u64;
    let mut booked = 0u64;
    let mut acc = 0i64;

    let mut i = 0u64;
    while i < 10_000 {
        let k = (i % exec_router::LEDGER_ROWS as u64) as u32;
        let outcome = 1_000 + k;
        let yes = sym(100 + k * 2);
        let no = yes + 1;
        let oid = 1_000_000 + i;

        // ---- a place ------------------------------------------------
        let mut ord = Order::new(
            T0 + i,
            VenueId::Hyperliquid,
            if i & 1 == 0 { yes } else { no },
            if i & 2 == 0 { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(400_000 + (i as i64 % 1_000)),
            Qty::from_raw(1_000_000),
            oid,
        );
        ord.strategy_id = SLOT;
        match d.submit(&ord) {
            Ok(()) => placed += 1,
            Err(DispatchError::RiskRefused) => refused += 1,
            Err(e) => panic!("unexpected dispatch error {e:?}"),
        }

        // ---- a venue fill on a BOUND leg ----------------------------
        let f = Fill::new(
            T0 + i,
            ord.sym,
            ord.side,
            Price::from_raw(400_000),
            Qty::from_raw(500_000),
            oid,
        )
        .with_attribution(SLOT, FILL_ORIGIN_VENUE);
        d.on_fill_booked(&f);
        booked += 1;

        // ---- a fill on an UNBOUND leg: the full-table scan ----------
        if i % 8 == 0 {
            let stray = Fill::new(
                T0 + i,
                sym(900_000 + (i as u32 & 7)),
                Side::Bid,
                Price::from_raw(400_000),
                Qty::from_raw(100_000),
                oid,
            )
            .with_attribution(SLOT, FILL_ORIGIN_VENUE);
            d.on_fill_booked(&stray);
        }

        // ---- a PAPER fill: the filter the ledger makes in one place -
        if i % 16 == 0 {
            let p = f.with_attribution(SLOT, FILL_ORIGIN_PAPER);
            d.on_fill_booked(&p);
        }

        // ---- a modify, then a cancel --------------------------------
        if i % 4 == 0 {
            let mut rep = ord;
            rep.client_oid = oid + 500_000_000;
            rep.px = Price::from_raw(410_000);
            if d.modify(&ModifyReq::new(oid, rep)).is_ok() {
                modified += 1;
            }
            if d.cancel(&CancelReq::of(&rep, T0 + i)).is_ok() {
                cancelled += 1;
            }
        }

        // ---- a roll: bind, settle, and drop what was resting --------
        if i % 64 == 0 {
            // A settle, then the successor's create — a NEW outcome id
            // on the same family, which is what a real roll carries.
            d.on_venue_event(&roll(k as usize, outcome, yes, true));
            d.on_venue_event(&roll(k as usize, outcome + 500_000, yes, false));
        }

        // ---- what `/state` reads ------------------------------------
        acc = acc
            .wrapping_add(d.ledger().slot_exposure_1e6(SLOT as usize))
            .wrapping_add(d.ledger().slot_day_turnover_1e6(SLOT as usize))
            .wrapping_add(i64::from(d.ledger().slot_resting(SLOT as usize)));
        i += 1;
    }
    std::hint::black_box((placed, refused, cancelled, modified, booked, acc));

    let (allocs, bytes, _deallocs) = g.delta();

    // ---- the premises, asserted rather than assumed -----------------
    let c = d.ledger().counters();
    assert!(placed > 0, "no order was ever accepted");
    assert!(refused > 0, "the risk gate never refused — the clamps never ran");
    assert!(cancelled > 0, "the cancel path never ran");
    assert!(modified > 0, "the modify path never ran");
    assert!(c.fills_booked > 0, "no venue fill was booked");
    assert!(c.fills_unbound > 0, "the full-table miss scan never ran");
    assert!(c.binds > 0, "no roll ever bound");
    assert!(c.instances_cleared > 0, "no instance was ever retired");

    assert_eq!(
        allocs, 0,
        "the venue-fill ledger allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "ledger hot bytes should be zero: saw {bytes}");
}

/// **E6 gate 62 — the halt machine's idle poll.**
///
/// E6 commit 3 gave the router a hook that runs on the ENGINE THREAD
/// every 2 ms whether or not anything is trading: `on_idle` reads the
/// arm's `halt_signal`, evaluates five triggers per live slot,
/// latches, and drives the cancel-all request/confirm cycle. Commit 4
/// added a file poll to the same hook.
///
/// None of that appeared in gates 53 or 61 — neither drives
/// `on_idle` — so the busiest path this lane added was the one path
/// with no zero-allocation gate at all. That gap is what this closes.
///
/// What runs inside the window:
///
/// * `on_idle` with a HEALTHY signal, the steady state — 500 times,
///   which is one second of real pacing and therefore includes the
///   halt file's `stat` and the one read it permits;
/// * the seeding edge, where `reconciled` first turns the restart
///   interlock off;
/// * `trigger_for` against every threshold on a live slot;
/// * the halt EDGE — latch, `exec.HALT` write and the slot's cancel
///   request — runs BETWEEN the windows, unmeasured: `write_atomic`
///   builds a path, once per incident (gate 100 measures the edge's
///   per-slot request, with no halt file);
/// * `cancel_slot_state` polling through `Working` to `Clear` (the second
///   window), which is where `clear_resting_slot` walks the table — this
///   arm answers the per-slot verbs through the trait's defaults;
/// * `exec_counters`, which `/state` and `/metrics` both read and
///   which now copies a per-slot halt array.
///
/// The halt file is deliberately CONFIGURED: `write_atomic` allocates
/// (a `PathBuf` for the `.tmp` sibling), and it runs on the engine
/// thread on the halt edge. That allocation is real, it is bounded to
/// one per halt incident rather than per poll, and this gate measures
/// the steady state separately from the edge so the distinction is
/// enforced rather than asserted in a comment.
#[test]
fn routed_halt_idle_steady_state() {
    use clob_dispatcher::{
        CancelAllState, DispatchError, HaltSignal, OrderDispatch, PaperDispatcher,
    };
    use core_types::{CancelReq, Fill, ModifyReq, Order};
    use exec_router::{ExecMode, ExecRoute, HaltLimits, HaltReason, RoutedDispatcher, SlotCaps};

    const SLOT: u8 = 3;

    /// An arm whose signal the gate drives, and whose cancel-all
    /// sweeps for a few polls before confirming — the real shape.
    struct Arm {
        polls: u64,
        sweeping: u32,
        cancels: u64,
    }
    /// The poll on which the arm starts reporting a reject streak.
    /// The arm trips ITSELF rather than being poked, because
    /// `live_mut` is test-only and widening it for a benchmark would
    /// put a mutable handle on the live arm into the public API.
    const TRIP_AT: u64 = 500;
    impl OrderDispatch for Arm {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
        fn halt_signal(&self) -> HaltSignal {
            // Healthy until TRIP_AT, then a reject streak at the
            // threshold. Reconciled throughout, so the seeding edge
            // happens on the first poll.
            // `>` not `>=`: the arm's `on_idle` runs BEFORE the
            // router reads this, so `polls == TRIP_AT` is still the
            // last healthy poll of the steady-state window.
            let streak = if self.polls > TRIP_AT { 5 } else { 0 };
            HaltSignal::new(1_000_000, 0, streak, 0, false, true, 1_000_000)
        }
        fn cancel_all(&mut self) -> Result<(), DispatchError> {
            self.cancels += 1;
            self.sweeping = 3;
            Ok(())
        }
        fn cancel_all_state(&self) -> CancelAllState {
            if self.sweeping > 0 {
                CancelAllState::Working
            } else {
                CancelAllState::Clear
            }
        }
        fn on_idle(&mut self) -> bool {
            self.polls += 1;
            self.sweeping = self.sweeping.saturating_sub(1);
            false
        }
        /// S7-L1: the venue's day spend for slot 3 — and the NEXT day's
        /// from poll 250, so the adoption's roll runs inside the window.
        fn venue_day_bought(&self, slot: usize) -> Option<(u64, i64)> {
            if slot != SLOT as usize {
                return None;
            }
            let day = DAY0 + u64::from(self.polls > 250);
            Some((day, 1_000_000 + 1_000_000 * i64::from(self.polls > 250)))
        }
    }
    /// The day the gate's anchor sits in.
    const DAY0: u64 = 1_789_776_001_000_000_000 / 86_400_000_000_000;

    // Boot-time construction — outside the window, as the engine's is.
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            SLOT as usize,
            ExecMode::Live,
            &[core_types::VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000),
        )
        .expect("boot: slot 3 live");
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        Arm {
            polls: 0,
            sweeping: 0,
            cancels: 0,
        },
        core_time::WallAnchor::new(1_789_776_001_000_000_000, 1_789_776_001_000_000_000),
    );
    let dir = std::env::temp_dir().join(format!("mv-gate62-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("gate 62 tmp dir");
    let halt_file = dir.join("exec.HALT");
    // **The file EXISTS for the whole window**, and says nothing is
    // halted. That is the steady state an operator actually leaves
    // behind — and the case where the poll does real work rather
    // than failing at the first `stat`. Measuring with no file would
    // measure the early return.
    std::fs::write(&halt_file, "# nothing halted\n").expect("gate 62 halt file");
    d.set_halt_path(halt_file.clone());
    // The boot read-back, outside the window: it is a boot step.
    let _ = d.adopt_halt_file();
    // **Now CHANGE it**, so the one poll the cadence lets through
    // inside the window takes the READ branch and not the
    // mtime-unchanged early return.
    //
    // Without this the gate measured the early return and called it
    // the read — and passed while `read_to_string` (as it then was)
    // allocated a `String` on the engine thread. A gate whose subject
    // never runs reports the same `ok` as a gate that holds.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&halt_file, "# still nothing halted\n").expect("gate 62 touch");

    // ---- the steady state -------------------------------------------
    let g = AllocGuard::new();
    let mut idles = 0u64;
    let mut i = 0u64;
    while i < TRIP_AT {
        std::hint::black_box(d.on_idle());
        idles += 1;
        std::hint::black_box(d.exec_counters().refused_halted);
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    std::hint::black_box(idles);

    assert!(d.ledger().is_seeded(), "the seeding edge must have run");
    assert_eq!(
        d.ledger().slot_day_turnover_1e6(SLOT as usize),
        2_000_000,
        "S7-L1: the venue's day spend was adopted, and the next day's after the roll"
    );
    assert!(d.ledger().counters().day_rollovers >= 1, "and the roll ran in the window");
    assert_eq!(
        d.halt_file_polls(),
        1,
        "the cadence must have let exactly one stat through — an upper \
         bound here would let a poll that never ran pass"
    );
    assert_eq!(
        d.halt_file_reads(),
        1,
        "and that poll must have READ, or this window is measuring the \
         mtime early return rather than the read it claims"
    );
    assert_eq!(
        d.halt_file_inert(),
        1,
        "the file it read halts nothing, by construction"
    );
    assert!(
        !d.halt().any_halted(),
        "a healthy signal must not halt — this gate would then be \
         measuring the edge rather than the steady state"
    );
    assert_eq!(
        allocs, 0,
        "the halt machine's idle poll allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "idle-poll hot bytes should be zero: saw {bytes}");

    // ---- the halt EDGE, measured on its own --------------------------
    //
    // Not folded into the window above: the edge writes `exec.HALT`,
    // and `core_io::write_atomic` builds a `.tmp` sibling path. That
    // allocation is deliberate and is bounded to one per incident —
    // what must never happen is it recurring on every poll. The arm
    // has reached TRIP_AT, so the next poll IS the edge.
    d.on_idle();
    assert!(d.halt().is_halted(SLOT as usize), "the edge must have fired");
    assert_eq!(d.halt().reason(SLOT as usize), HaltReason::RejectStreak);

    let g = AllocGuard::new();
    let mut j = 0usize;
    while j < 500 {
        std::hint::black_box(d.on_idle());
        j += 1;
    }
    let (allocs, bytes, _) = g.delta();
    assert!(
        !d.halt().cancel_outstanding(),
        "the cancel must have confirmed inside the window"
    );
    assert_eq!(
        allocs, 0,
        "a HALTED slot's idle poll allocated {allocs} times ({bytes} B) — \
         the edge is once per incident, the poll is every 2 ms"
    );
    assert_eq!(bytes, 0, "halted idle-poll bytes should be zero: saw {bytes}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **XMM gate 100 (XH5a) — the per-slot halt path is 0 B/op.** R-XH4-1
/// made the router's halt cancel per slot: every idle poll,
/// `try_cancel_slots` runs each halted slot's request/confirm cycle on
/// its own, and a slot's `Clear` releases that slot's rows alone
/// (`Ledger::clear_resting_slot`). Three windows:
///
/// * the router over an arm that answers PER SLOT — slot 3 trips its own
///   reject streak while slot 4, on the same arm, never does. Slot 3's
///   first request is refused (`Retry`), its first sweep strands
///   (`Stranded`, asked again) and the third clears, releasing slot 3's
///   resting row while slot 4's stays. No halt file is configured, so
///   gate 62's one allocation on the edge (`write_atomic`'s path) is not
///   in it;
/// * the blocking arm's own request and confirmation
///   (`HlExchange::cancel_slot` / `cancel_slot_state`) with more live legs
///   than its sweep table holds: the leg walk, the inserts, every later
///   slot OR'd in, the legs left out stranded (`strand`), the per-slot
///   scan;
/// * the sweep's selection over a venue answer (`recon::scan_open_orders`
///   and `recon::ours_on_leg` with a one-slot mask), which runs on every
///   idle moment while a sweep is pending.
#[test]
fn xmm_per_slot_halt_path_is_zero_alloc() {
    use clob_dispatcher::{
        CancelAllState, DispatchError, HaltSignal, OrderDispatch, PaperDispatcher,
    };
    use core_types::{CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Side};
    use exec_hyperliquid::config::{HlConfig, Scope};
    use exec_hyperliquid::exchange::HlExchange;
    use exec_hyperliquid::recon::{ours_on_leg, scan_open_orders, OpenOrder, MAX_OPEN_ORDERS};
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, HaltReason, InstrumentSpec, RoutedDispatcher, SlotCaps,
        LAW_LINEAR,
    };

    fn roll(outcome: u32, sym: u32) -> ChannelEvent {
        ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym,
            u64::from(outcome) | (60u64 << 32),
            0,
            1_000_000,
            2_000_000_000,
        )
    }

    /// The poll after which slot 3's reject streak sits at its threshold.
    const TRIP_AT: u64 = 250;

    /// An arm whose slots sweep on their own: slot 3 trips ITSELF (as
    /// gate 62's arm does); a slot's sweep drains in three polls; the next
    /// `fail[s]` requests of a slot are refused and its next `strand[s]`
    /// sweeps give a leg up.
    struct Arm {
        polls: u64,
        sweeping: [u32; 8],
        asked: [u32; 8],
        fail: [u32; 8],
        strand: [u32; 8],
        stranded: [bool; 8],
    }
    impl OrderDispatch for Arm {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
        fn halt_signal(&self) -> HaltSignal {
            HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000)
        }
        fn halt_signal_for(&self, slot: u8) -> HaltSignal {
            let streak = 5 * u32::from(slot == 3 && self.polls > TRIP_AT);
            HaltSignal::new(1_000_000, 0, streak, 0, false, true, 1_000_000)
        }
        fn cancel_slot(&mut self, slot: u8) -> Result<(), DispatchError> {
            let s = usize::from(slot);
            self.asked[s] += 1;
            if self.fail[s] > 0 {
                self.fail[s] -= 1;
                return Err(DispatchError::QueueFull);
            }
            self.stranded[s] = false;
            self.sweeping[s] = 3;
            Ok(())
        }
        fn cancel_slot_state(&self, slot: u8) -> CancelAllState {
            let s = usize::from(slot);
            if self.sweeping[s] > 0 {
                CancelAllState::Working
            } else if self.stranded[s] {
                CancelAllState::Stranded
            } else {
                CancelAllState::Clear
            }
        }
        fn on_idle(&mut self) -> bool {
            self.polls += 1;
            let mut s = 0usize;
            while s < 8 {
                if self.sweeping[s] == 1 && self.strand[s] > 0 {
                    // This sweep gives a leg up: the venue still holds one.
                    self.strand[s] -= 1;
                    self.stranded[s] = true;
                }
                self.sweeping[s] = self.sweeping[s].saturating_sub(1);
                s += 1;
            }
            false
        }
    }

    // Boot-time construction — outside the windows, as the engine's is.
    let mut route = ExecRoute::all_paper();
    let mut slot = 3usize;
    while slot <= 4 {
        route
            .set_slot(
                slot,
                ExecMode::Live,
                &[core_types::VenueId::Hyperliquid.to_u8()],
                SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
                HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000),
            )
            .expect("boot: slots 3 and 4 live");
        slot += 1;
    }
    let mut fail = [0u32; 8];
    let mut strand = [0u32; 8];
    fail[3] = 1;
    strand[3] = 1;
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        Arm {
            polls: 0,
            sweeping: [0; 8],
            asked: [0; 8],
            fail,
            strand,
            stranded: [false; 8],
        },
        core_time::WallAnchor::new(1_789_776_001_000_000_000, 1_789_776_001_000_000_000),
    );
    let sym = core_types::make_symbol_id(VenueId::Hyperliquid, 950);
    d.bind_instrument(&InstrumentSpec::new(sym, LAW_LINEAR, 0, 0, 0))
        .expect("gate 100 row");
    // The seeding edge: a boot step, outside the window.
    d.on_idle();
    // One resting order each for slots 3 and 4, so slot 3's `Clear`
    // releases a real row and slot 4's is there to survive it.
    let mut s = 3u8;
    while s <= 4 {
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            0,
            Price(1_000_000),
            Qty(1_000_000),
            u64::from(s),
        );
        o.strategy_id = s;
        d.submit(&o).expect("gate 100: a resting order");
        s += 1;
    }
    assert_eq!(
        (d.ledger().slot_resting(3), d.ledger().slot_resting(4)),
        (1, 1)
    );

    // ---- the router's per-slot cycle ----------------------------------
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < 2 * TRIP_AT {
        std::hint::black_box(d.on_idle());
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    assert!(
        d.halt().is_halted(3),
        "slot 3's edge must have fired in the window"
    );
    assert_eq!(d.halt().reason(3), HaltReason::RejectStreak);
    assert!(
        !d.halt().is_halted(4),
        "slot 4 never tripped its own numbers"
    );
    assert!(
        !d.halt().cancel_outstanding(),
        "slot 3's cancel must have confirmed"
    );
    assert_eq!(
        d.live().asked,
        [0, 0, 0, 3, 0, 0, 0, 0],
        "slot 3: refused, stranded, cleared; nobody else asked"
    );
    assert_eq!(d.halt().cancel_all_failures, 1, "the Retry branch ran");
    assert_eq!(d.halt().cancel_all_stranded, 1, "the Stranded branch ran");
    assert_eq!(
        (d.ledger().slot_resting(3), d.ledger().slot_resting(4)),
        (0, 1),
        "slot 3's row released on its Clear; slot 4's stays"
    );
    assert_eq!(
        allocs, 0,
        "the per-slot halt poll allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "per-slot halt-poll bytes should be zero: saw {bytes}"
    );

    // ---- the blocking arm's per-slot request and confirmation ---------
    let cfg = HlConfig::new(Scope::Testnet, "127.0.0.1", 'b', [0x55; 32], [0x66; 20]).expect("cfg");
    let (p, _c) = Ring::<Fill, 64>::new().split();
    let mut x = HlExchange::<64>::new(
        &cfg,
        core_net::TlsTransport::default_client_config(),
        p,
        std::env::temp_dir().join(format!("mv-gate100-{}.state", std::process::id())),
        100,
    )
    .expect("build");
    // Nine instances: 18 live legs, two more than the sweep table holds,
    // bound outside the window.
    let mut k = 0u32;
    while k < 9 {
        x.on_venue_event(&roll(19_418 + k, 4096 + 2 * k));
        k += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut j = 0u32;
    while j < 2_000 {
        let s = (j & 7) as u8;
        acc ^= u64::from(x.cancel_slot(s).is_ok());
        acc ^= u64::from(x.cancel_slot_state(s) == CancelAllState::Working);
        j += 1;
    }
    let (allocs, bytes, _) = g.delta();
    std::hint::black_box(acc);
    let c = *x.counters();
    assert_eq!(c.cancel_all_runs, 2_000, "every request ran");
    assert_eq!(
        c.cancel_all_unqueued, 4_000,
        "two legs left out of every request, counted"
    );
    assert_eq!(c.sweep_left, 0, "a retry is not a stranding");
    assert_eq!(
        x.cancel_slot_state(5),
        CancelAllState::Working,
        "every slot OR'd into the pending legs"
    );
    assert_eq!(
        allocs, 0,
        "HlExchange::cancel_slot allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "per-slot request bytes should be zero: saw {bytes}"
    );

    // ---- the sweep's selection over a venue answer --------------------
    // Six of ours on one leg, slots 3 and 4 in turn — built outside the
    // window, scanned and selected inside it.
    let mut body = String::from("[");
    let mut r = 0u64;
    while r < 6 {
        let c = exec_hyperliquid::cloid::encode(3 + (r % 2) as u8, 100 + r);
        let mut h = [0u8; 34];
        let n = exec_hyperliquid::cloid::to_hex(&c, &mut h);
        if r > 0 {
            body.push(',');
        }
        body.push_str(&format!(
            r##"{{"coin":"#195641","oid":{},"cloid":"{}"}}"##,
            11 + r,
            core::str::from_utf8(&h[..n]).expect("ascii")
        ));
        r += 1;
    }
    body.push(']');
    let body = body.into_bytes();
    let mut rows = vec![OpenOrder::default(); MAX_OPEN_ORDERS];
    let mut oids = vec![0u64; MAX_OPEN_ORDERS];

    let g = AllocGuard::new();
    let mut picked = 0usize;
    let mut j = 0u32;
    while j < 2_000 {
        let n = scan_open_orders(&body, &mut rows).expect("the venue's answer scans");
        picked += ours_on_leg(&rows[..n], &body, b"#195641", 1 << 3, &mut oids);
        j += 1;
    }
    let (allocs, bytes, _) = g.delta();
    assert_eq!(picked, 3 * 2_000, "slot 3's three of the six, every pass");
    assert_eq!(
        allocs, 0,
        "the sweep's selection allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "sweep-selection bytes should be zero: saw {bytes}"
    );
}

// ---------------------------------------------------------------
// MX8: MEXC ingress hot-path assertions (spot protobuf + futures JSON)
// ---------------------------------------------------------------

/// Test-only protobuf encoder for the MEXC fixtures (boot side — every
/// call happens outside the measured windows).
fn mexc_pb_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn mexc_pb_len(out: &mut Vec<u8>, field_no: u32, payload: &[u8]) {
    mexc_pb_varint(out, ((field_no as u64) << 3) | 2);
    mexc_pb_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

fn mexc_pb_u64(out: &mut Vec<u8>, field_no: u32, v: u64) {
    mexc_pb_varint(out, (field_no as u64) << 3);
    mexc_pb_varint(out, v);
}

/// The plan §1.1 spot bookTicker push for BTCUSDT at `bid_qty`.
fn mexc_spot_book(bid_qty: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    mexc_pb_len(&mut body, 1, b"80535.88");
    mexc_pb_len(&mut body, 2, bid_qty);
    mexc_pb_len(&mut body, 3, b"80535.89");
    mexc_pb_len(&mut body, 4, b"0.33336356");
    mexc_pb_len(&mut body, 5, b"81721676217");
    let mut book = Vec::new();
    mexc_pb_len(&mut book, 1, b"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT");
    mexc_pb_len(&mut book, 3, b"BTCUSDT");
    mexc_pb_u64(&mut book, 6, 1_789_897_517_479);
    mexc_pb_len(&mut book, 315, &body);
    book
}

/// The plan §1.1 spot pushes for BTCUSDT: (bookTicker, deals × 2).
fn mexc_spot_pushes() -> (Vec<u8>, Vec<u8>) {
    let book = mexc_spot_book(b"0.380497");

    let mut items = Vec::new();
    for (px, qty, side, id) in [
        (&b"80535.88"[..], &b"0.01362099"[..], 2u64, &b"730292425431437318X0_730292425431437319X0"[..]),
        (&b"80535.89"[..], &b"0.0012345"[..], 1u64, &b"730292425431437320X0_730292425431437320X0"[..]),
    ] {
        let mut it = Vec::new();
        mexc_pb_len(&mut it, 1, px);
        mexc_pb_len(&mut it, 2, qty);
        mexc_pb_u64(&mut it, 3, side);
        mexc_pb_u64(&mut it, 4, 1_789_897_518_262);
        mexc_pb_len(&mut it, 5, id);
        mexc_pb_len(&mut items, 1, &it);
    }
    mexc_pb_len(&mut items, 2, b"spot@public.aggre.deals.v3.api.pb@10ms");
    let mut deals = Vec::new();
    mexc_pb_len(&mut deals, 1, b"spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT");
    mexc_pb_len(&mut deals, 3, b"BTCUSDT");
    mexc_pb_u64(&mut deals, 6, 1_789_897_518_300);
    mexc_pb_len(&mut deals, 314, &items);
    (book, deals)
}

const MEXC_FUT_DEPTH: &[u8] = br#"{"symbol":"BTC_USDT","data":{"cts":1789897581009,"asks":[[80468.7,3446,2],[80469.1,1239,1]],"bids":[[80468.6,31288,7]],"version":41925002140},"channel":"push.depth.full","ts":1789897581013}"#;
/// [`MEXC_FUT_DEPTH`] with the best-bid size moved (a BBO change).
const MEXC_FUT_DEPTH_ALT: &[u8] = br#"{"symbol":"BTC_USDT","data":{"cts":1789897581009,"asks":[[80468.7,3446,2],[80469.1,1239,1]],"bids":[[80468.6,31289,7]],"version":41925002140},"channel":"push.depth.full","ts":1789897581013}"#;
const MEXC_FUT_DEAL: &[u8] = br#"{"symbol":"BTC_USDT","data":[{"p":80489,"v":11,"T":1,"O":3,"M":1,"t":1789897547210,"i":"16270106116","cts":"1789897547210"},{"p":80488.5,"v":2,"T":2,"O":3,"M":2,"t":1789897547211,"i":"16270106117"}],"channel":"push.deal","ts":1789897547215}"#;
const MEXC_FUT_TICKER: &[u8] = br#"{"symbol":"BTC_USDT","data":{"symbol":"BTC_USDT","lastPrice":80489,"indexPrice":80490.1,"fairPrice":80489.5,"fundingRate":0.0001,"holdVol":92415393,"timestamp":1789897545754},"channel":"push.ticker","ts":1789897545760}"#;

/// Classify + every MEXC parser of both classes (the spot wrapper walk,
/// both bodies, the deals walk, the ack + its failed-param walker; the
/// futures depth.full / deal walk / ticker + the extract helpers + the
/// funding clock) over the plan's measured shapes, 10 000 iterations —
/// must be zero-alloc.
#[test]
fn mexc_parsers_are_zero_alloc() {
    let (book, deals) = mexc_spot_pushes();
    let ack: &[u8] = "{\"id\":0,\"code\":0,\"msg\":\"Subscribed successful! [spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT]. Not Subscribed successfully! [spot@public.increase.depth.v3.api.pb@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@ETHUSDT].  Reason\u{ff1a} Blocked! \"}".as_bytes();

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for i in 0..10_000u64 {
        std::hint::black_box(ingress_mexc::classify_spot(&book, true));
        std::hint::black_box(ingress_mexc::classify_spot(ack, false));
        std::hint::black_box(ingress_mexc::classify_futures(MEXC_FUT_DEPTH));
        std::hint::black_box(ingress_mexc::classify_futures(MEXC_FUT_DEAL));
        std::hint::black_box(ingress_mexc::classify_futures(MEXC_FUT_TICKER));
        let w = ingress_mexc::parse_spot_wrapper(&book).unwrap();
        acc = acc.wrapping_add(w.symbol(&book).len() as i64);
        let mut b = ingress_mexc::spot::MexcBookTicker::ZERO;
        assert!(ingress_mexc::parse_book_ticker_body(w.body(&book), &mut b));
        acc = acc.wrapping_add(b.bid_px_1e6);
        let w = ingress_mexc::parse_spot_wrapper(&deals).unwrap();
        let mut dw = ingress_mexc::MexcDealsWalk::new(w.body(&deals));
        while let Some(item) = dw.next_item() {
            let mut deal = ingress_mexc::MexcDeal::ZERO;
            assert!(ingress_mexc::parse_deal_item(item, &mut deal));
            acc = acc.wrapping_add(deal.signed_qty_1e6());
        }
        let mut a = ingress_mexc::spot::MexcSpotAck::ZERO;
        assert!(ingress_mexc::parse_sub_ack(ack, &mut a));
        let mut p = a.failed_params(ack);
        while let Some(param) = p.next_param() {
            acc = acc.wrapping_add(ingress_mexc::extract_param_symbol(param).map_or(0, |s| s.len() as i64));
            acc = acc.wrapping_add(ingress_mexc::extract_param_channel(param).map_or(-1, |c| c.discriminant()));
        }
        acc = acc.wrapping_add(ingress_mexc::extract_fut_symbol(MEXC_FUT_DEPTH).unwrap().len() as i64);
        acc = acc.wrapping_add(ingress_mexc::extract_fut_ts_ms(MEXC_FUT_DEAL) as i64);
        let mut d = ingress_mexc::futures::MexcDepthFrame::ZERO;
        assert!(ingress_mexc::parse_depth_full(MEXC_FUT_DEPTH, &mut d));
        acc = acc.wrapping_add(d.bid_px_1e6);
        let mut fw = ingress_mexc::MexcFutDealsWalk::new(MEXC_FUT_DEAL);
        while let Some(item) = fw.next_item() {
            let mut deal = ingress_mexc::MexcDeal::ZERO;
            assert!(ingress_mexc::parse_fut_deal_item(item, &mut deal));
            acc = acc.wrapping_add(deal.px_1e6);
        }
        let mut t = ingress_mexc::futures::MexcTickerFrame::ZERO;
        assert!(ingress_mexc::parse_ticker(MEXC_FUT_TICKER, &mut t));
        acc = acc.wrapping_add(t.funding_rate_1e9);
        acc = acc.wrapping_add(ingress_mexc::funding_next_settle_ms(
            1_789_920_000_000,
            8 * ingress_mexc::MS_PER_HOUR,
            1_789_920_000_000 + i,
        ) as i64);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "mexc parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "mexc parser bytes should be zero: saw {bytes}");
}

/// Drive BOTH MEXC connection classes through their real handshakes,
/// then through a pre-injected steady-state stream (spot: the per-param
/// ack + CYCLES × [PB bookTicker, its unchanged republication, PB
/// deals]; futures: CYCLES × [depth.full, its unchanged republication,
/// deal, ticker] with the Funding event on the lane) with a REAL
/// `PmlrCapture` (raw tap `All`). The BBO alternates between two
/// touches every cycle, so BOTH the emit path (a change) and the
/// dedupe path (the republication) are measured. Every `drive_one`
/// must allocate zero bytes.
#[test]
fn mexc_run_loop_steady_state_is_zero_alloc() {
    use ingress_mexc::run_loop as mwl;
    use ingress_mexc::{MexcClass, MexcSymbolTable};

    // ---- boot (NOT measured) ----
    const CYCLES: usize = 300;
    let sym_spot: SymbolId = (7 << 24) | 1;
    let sym_perp: SymbolId = (7 << 24) | 513;
    let mut st = MexcSymbolTable::new();
    st.insert(b"BTCUSDT", sym_spot).unwrap();
    let mut ft = MexcSymbolTable::new();
    ft.insert(b"BTC_USDT", sym_perp).unwrap();
    let mut spot = mwl::Driver::new(0x5107, MexcClass::Spot, st);
    let mut fut = mwl::Driver::new(0x0F07, MexcClass::Futures, ft);
    assert!(fut.set_funding_seed(sym_perp, 1_789_920_000_000, 8));
    // The fixtures carry FIXED venue stamps: disable the stale judgement
    // so a slow (debug) run cannot flip a verdict mid-stream and add a
    // tick (a flipped verdict is a tick by the dedupe law).
    spot.set_stale_after_ms(0);
    fut.set_stale_after_ms(0);
    let mut ts = TestTransport::with_capacity(512 * 1024);
    let mut tf = TestTransport::with_capacity(512 * 1024);

    let status = core_metrics::IngressStatus::new();
    let ring: std::sync::Arc<Ring<Tick, { mwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();
    let event_ring: std::sync::Arc<
        Ring<core_types::ChannelEvent, { core_types::EVENT_RING_SIZE }>,
    > = Ring::new();
    let (mut etx, mut erx) = event_ring.split();

    let cap_dir = std::env::temp_dir().join(format!("mexc_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "mexc",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    // Real handshake per class (GET → 101 → subscribe set queued).
    for (t, d, seed, path) in [
        (&mut ts, &mut spot, 0x5107u64, &b"/ws"[..]),
        (&mut tf, &mut fut, 0x0F07u64, &b"/edge"[..]),
    ] {
        mwl::note_transport_ready(d, core_net::Status::Ready);
        mwl::drive_one(t, d, b"h", path, &mut prod, &mut etx, core_types::EVENT_LANE_FUNDING, &status, &mut capture).unwrap();
        let mut scratch = [0u8; 8192];
        let _ = t.drain_outgoing(&mut scratch);
        let accept = core_net::expected_accept(&core_net::sec_websocket_key_from_seed(seed));
        let mut resp = Vec::new();
        resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
        resp.extend_from_slice(&accept);
        resp.extend_from_slice(b"\r\n\r\n");
        t.inject_incoming(&resp);
        mwl::drive_one(t, d, b"h", path, &mut prod, &mut etx, core_types::EVENT_LANE_FUNDING, &status, &mut capture).unwrap();
        assert_eq!(d.state(), mwl::State::Steady);
        let _ = t.drain_outgoing(&mut scratch); // the subscribe set
    }

    /// Unmasked server→client frame (`first` = 0x81 text / 0x82 binary).
    fn push_frame(stream: &mut Vec<u8>, first: u8, body: &[u8]) {
        stream.push(first);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }

    let (book, deals) = mexc_spot_pushes();
    let book_alt = mexc_spot_book(b"0.380498");
    let mut spot_stream = Vec::with_capacity(256 * 1024);
    push_frame(
        &mut spot_stream,
        0x81,
        br#"{"id":0,"code":0,"msg":"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT,spot@public.aggre.deals.v3.api.pb@10ms@BTCUSDT"}"#,
    );
    let mut fut_stream = Vec::with_capacity(512 * 1024);
    for cycle in 0..CYCLES {
        let (b, depth) = if cycle % 2 == 0 {
            (&book, MEXC_FUT_DEPTH)
        } else {
            (&book_alt, MEXC_FUT_DEPTH_ALT)
        };
        push_frame(&mut spot_stream, 0x82, b);
        push_frame(&mut spot_stream, 0x82, b); // republication: no tick
        push_frame(&mut spot_stream, 0x82, &deals);
        push_frame(&mut fut_stream, 0x81, depth);
        push_frame(&mut fut_stream, 0x81, depth); // republication: no tick
        push_frame(&mut fut_stream, 0x81, MEXC_FUT_DEAL);
        push_frame(&mut fut_stream, 0x81, MEXC_FUT_TICKER);
    }
    assert_eq!(ts.inject_incoming(&spot_stream), spot_stream.len());
    assert_eq!(tf.inject_incoming(&fut_stream), fut_stream.len());

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut drives = 0u32;
    for (t, d) in [(&mut ts, &mut spot), (&mut tf, &mut fut)] {
        while t.incoming_len() > 0 {
            mwl::drive_one(t, d, b"h", b"/", &mut prod, &mut etx, core_types::EVENT_LANE_FUNDING, &status, &mut capture).unwrap();
            drives += 1;
            assert!(drives <= 4_096, "scripted stream failed to drain");
        }
    }
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    let mut acc: i64 = 0;
    let mut ticks = 0usize;
    while let Some(t) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(t.bid_px.raw());
        ticks += 1;
    }
    let mut funding = 0usize;
    while let Some(e) = erx.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(e.v1);
        funding += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // One tick per BBO CHANGE of either class (never per republication),
    // one lane event per ticker, every frame counted, nothing lost, no
    // regressions.
    assert_eq!(ticks, 2 * CYCLES);
    assert_eq!(funding, CYCLES);
    assert_eq!(status.msgs_total(), (1 + CYCLES * 4 + CYCLES * 5) as u64);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(status.event_ring_drops_total(), 0);
    assert_eq!(status.seq_regressions_total(), 0);
    assert_eq!(status.sub_drops_total(), 0);
    assert_eq!(spot.sub_count(), 2);
    assert_eq!(fut.sub_count(), 3);
    assert_eq!(allocs, 0, "mexc run-loop allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "mexc run-loop bytes should be zero: saw {bytes}");

    // Capture accounting: a tick per BBO, an event per print (2 + 2 per
    // cycle) + three per ticker (Mark, Funding, OI), a tap record per
    // data payload (the ack + 7 per cycle), no I/O errors.
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.ticks_written(), (2 * CYCLES) as u64);
    assert_eq!(capture.events_written(), (7 * CYCLES) as u64);
    assert_eq!(capture.tap_records(), (1 + 7 * CYCLES) as u64);
    assert_eq!(capture.tap_dropped(), 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// **HYPARB H1 gate 63 — the AMM walk and the arb solve.**
///
/// `solve_arb` runs on every pool update the member re-evaluates (one
/// per on-chain swap, ~1/s across the universe) and walks the pool's
/// real tick map through the contract's 256-bit arithmetic; the paper
/// matcher's `swap_exact_in_range` judges every AMM order. Both live on
/// the engine thread. Everything here is `Copy` PODs, fixed arrays and
/// a caller-owned map, so the whole path must be 0 B/op. The map is
/// boxed at boot (32 KiB) — that one allocation is outside the guard.
#[test]
fn amm_walk_and_arb_solve_are_zero_alloc() {
    use core_amm::{
        price_1e18_from_sqrt, solve_arb, sqrt_at_tick, swap_exact, swap_exact_in_range, tick_at_sqrt, ArbParams,
        ArbSide, PoolMeta, PoolState, SwapSpec, TickMap, TickNode,
    };
    const SPACING: i32 = 10;
    let mut nodes = [TickNode::ZERO; 64];
    let mut k = 0usize;
    while k < 32 {
        // Nested positions around -230,540: lower edges add, upper edges remove.
        let w = (k as i32 + 1) * 40 * SPACING;
        nodes[31 - k] = TickNode::new(-230_540 - w, 1_000_000_000_000_000);
        nodes[32 + k] = TickNode::new(-230_540 + w, -1_000_000_000_000_000);
        k += 1;
    }
    let mut map = Box::new(TickMap::<1024>::EMPTY);
    map.load(&nodes, -250_000, -210_000, SPACING).expect("gate 63 map");
    let (lo, hi) = sqrt_at_tick(-230_543);
    let state = PoolState::new(lo, hi, -230_543, 32_000_000_000_000_000);
    let mut meta = PoolMeta::ZERO;
    meta.tick_spacing = SPACING;
    meta.fee_pips = 500;
    meta.dec0 = 18;
    meta.dec1 = 6;
    let mid = price_1e18_from_sqrt(lo, hi, 18, 6);

    let g = AllocGuard::new();
    let mut traded = 0u64;
    let mut acc: u128 = 0;
    let mut n = 0u64;
    while n < 5_000 {
        // Hedge bid swings ±150 bps around the pool mid.
        let bps = (n % 301) as u128;
        let bid = mid - mid * 150 / 10_000 + mid * bps / 10_000;
        let q = solve_arb(&state, &meta, &map, &ArbParams {
            eff_bid_1e18: bid,
            eff_ask_1e18: bid + bid / 5_000,
            px0_usd_1e6: (mid / 1_000_000_000_000) as i64,
            max_notional_usd_1e6: 20_000_000_000,
            gas_usd_1e6: 10_000,
        });
        if q.side != ArbSide::None {
            traded += 1;
            acc = acc.wrapping_add(q.token0_raw);
        }
        let (tl, th) = sqrt_at_tick(-230_543 + (n % 400) as i32 - 200);
        let spec = SwapSpec {
            amount: 1_000_000_000_000_000_000,
            limit_lo: tl,
            limit_hi: th,
            fee_pips: 500,
            zero_for_one: n % 400 < 200,
            exact_in: n % 2 == 0,
        };
        let r = swap_exact(&state, &meta, &map, &spec);
        let m = swap_exact_in_range(&state, &meta, &spec);
        acc = acc.wrapping_add(r.amount_out).wrapping_add(m.amount_out);
        acc = acc.wrapping_add(tick_at_sqrt(r.after.sqrt_price_lo, r.after.sqrt_price_hi) as u128);
        n += 1;
    }
    std::hint::black_box((traded, acc));

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(traded > 0 && acc != 0, "the gate must measure real work");
    assert_eq!(allocs, 0, "core-amm walk/solve allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "core-amm hot bytes should be zero: saw {bytes}");
}

/// **HYPARB H7 gate 64 — the EVM sign + hash + hex-render path.**
///
/// Every testnet send (and, after a later ruling, every mainnet one)
/// goes digest → secp256k1 sign → tx hash → hex render into the request
/// body. The digest and the hash are `keccak256_parts` over stack
/// encodings and the BORROWED calldata; the render writes into the
/// caller's buffer. The whole path must be 0 B/op — the body buffer is
/// allocated once at boot, outside the guard.
#[test]
fn evm_sign_hash_and_render_are_zero_alloc() {
    use signer_evm::{tx_encode_signed_hex, tx_hash, tx_sign, Eip1559Tx};
    let sk = signer_eip712::parse_secret_key(&[0x42; 32]).expect("gate 64 key");
    let calldata = [0xa5u8; 228]; // a V3 swap's worth of calldata
    let mut body = vec![0u8; 4096];
    // Boot: the first signature builds signer-eip712's process-wide
    // secp256k1 context (one 208 B allocation, `OnceLock`). The arm does
    // this at boot too; the guard measures the steady state after it.
    let warm = Eip1559Tx { chain_id: 998, nonce: 0, max_priority_fee_per_gas: 0, max_fee_per_gas: 0, gas_limit: 21_000, to: [0; 20], value: 0, data: &[] };
    tx_sign(&warm, &sk).expect("gate 64 warm-up");

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut n = 0u64;
    while n < 2_000 {
        let tx = Eip1559Tx {
            chain_id: 998,
            nonce: n,
            max_priority_fee_per_gas: 1_000_000_000 + n as u128,
            max_fee_per_gas: 3_000_000_000,
            gas_limit: 350_000,
            to: [0x55; 20],
            value: 0,
            data: &calldata,
        };
        let sig = tx_sign(&tx, &sk).expect("gate 64 sign");
        let h = tx_hash(&tx, &sig).expect("gate 64 hash");
        let w = tx_encode_signed_hex(&tx, &sig, &mut body).expect("gate 64 render");
        acc = acc.wrapping_add(w as u64).wrapping_add(h[0] as u64);
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(allocs, 0, "signer-evm sign/hash/render allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "signer-evm hot bytes should be zero: saw {bytes}");
}

/// **HYPARB gate 65 — the Algebra walk, tick-map maintenance, and the
/// pool-event payload codec.**
///
/// Every on-chain swap reaches the member as two 40-byte payloads the
/// ingress encodes and the member decodes; every `Mint`/`Burn` mutates a
/// pool's tick map in place; an Algebra pool is walked in its own loop.
/// All of it runs on the ingress or the engine thread per event, so all
/// of it must be 0 B/op. The map is boxed at boot, outside the guard.
#[test]
fn amm_algebra_walk_map_mutation_and_payload_are_zero_alloc() {
    use core_amm::payload::{decode, encode_liquidity, encode_state, encode_swap, PoolEvent};
    use core_amm::{
        sqrt_at_tick, swap_exact, PoolMeta, PoolState, SwapSpec, TickMap, AMM_KIND_ALGEBRA,
    };
    let mut map = Box::new(TickMap::<1024>::EMPTY);
    map.load(&[], -250_000, -210_000, 1).expect("gate 65 map");
    let mut state = PoolState::new(0, 0, -230_543, 0);
    let (lo, hi) = sqrt_at_tick(-230_543);
    state.sqrt_price_lo = lo;
    state.sqrt_price_hi = hi;
    let mut meta = PoolMeta::ZERO;
    meta.kind = AMM_KIND_ALGEBRA;
    meta.tick_spacing = 10;
    meta.fee_pips = 500;
    // Seed nested positions through the mutation path itself.
    let mut k = 0i32;
    while k < 32 {
        let w = (k + 1) * 400;
        map.apply_position(-230_540 - w, -230_540 + w, 1_000_000_000_000_000)
            .expect("gate 65 seed");
        state
            .apply_position(-230_540 - w, -230_540 + w, 1_000_000_000_000_000)
            .expect("gate 65 seed");
        k += 1;
    }

    let g = AllocGuard::new();
    let mut acc: u128 = 0;
    let mut n = 0u64;
    while n < 5_000 {
        // A mint and its burn: an insert and a remove on the fixed array.
        let t = -230_000 + (n % 97) as i32 * 10;
        map.apply_position(t - 50, t + 50, 7_777)
            .expect("gate 65 mint");
        map.apply_position(t - 50, t + 50, -7_777)
            .expect("gate 65 burn");
        let (tl, th) = sqrt_at_tick(-230_543 + (n % 4_000) as i32 - 2_000);
        let spec = SwapSpec {
            amount: 1_000_000_000_000_000_000,
            limit_lo: tl,
            limit_hi: th,
            fee_pips: 500,
            zero_for_one: n % 4_000 < 2_000,
            exact_in: n % 2 == 0,
        };
        let r = swap_exact(&state, &meta, &map, &spec);
        // Ingress → member, both payloads of the swap and a Mint.
        let s = encode_swap(46_650_000 + n, r.amount_in as i128, -(r.amount_out as i128))
            .expect("gate 65 swap");
        let p = encode_state(
            r.after.tick,
            r.after.sqrt_price_lo,
            r.after.sqrt_price_hi,
            r.after.liquidity,
            false,
        )
        .expect("gate 65 state");
        let l = encode_liquidity(46_650_000 + n, n % 3 == 0, t - 50, t + 50, 7_777)
            .expect("gate 65 liq");
        if let Some(PoolEvent::Swap { amount0, .. }) = decode(&s) {
            acc = acc.wrapping_add(amount0 as u128);
        }
        if let Some(PoolEvent::State {
            liquidity, tick, ..
        }) = decode(&p)
        {
            acc = acc.wrapping_add(liquidity).wrapping_add(tick as u128);
        }
        if let Some(PoolEvent::Liquidity { amount, .. }) = decode(&l) {
            acc = acc.wrapping_add(amount);
        }
        acc = acc.wrapping_add(r.amount_out);
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(
        acc != 0 && map.len() == 64,
        "the gate must measure real work"
    );
    assert_eq!(
        allocs, 0,
        "core-amm algebra/map/payload allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "core-amm algebra/map/payload hot bytes should be zero: saw {bytes}"
    );
}
/// **HYPARB H3 gate 66 — the HyperEVM ingress after the handshake.**
///
/// Everything the session does once upgraded, inside the guard: both
/// subscriptions (the `logs` frame renders every pool address and topic),
/// the pinned pool snapshot (reads issued, replies scanned, the archive
/// probe, the snapshot emitted onto the ring), then 1 000 live `Swap`
/// pushes decoded into `SWAP` + `STATE` payloads and pushed. The reply
/// frames are rendered before the guard; the driver was sized at boot.
#[test]
fn hyperevm_session_snapshot_and_live_swaps_are_zero_alloc() {
    use ingress_hyperevm::{run_loop as hwl, PoolEntry, PoolFamily, PoolTable};

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x81u8];
        if body.len() <= 125 {
            out.push(body.len() as u8);
        } else {
            out.push(126);
            out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(body);
        out
    }
    fn word_i(v: i64) -> String {
        if v < 0 {
            format!("{}{:016x}", "f".repeat(48), v as u64)
        } else {
            format!("{:064x}", v as u64)
        }
    }
    fn reply(id: u64, words: &str) -> Vec<u8> {
        frame(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x{words}"}}"#).as_bytes())
    }

    const B: u64 = 46_650_000;
    let addr = [0x30u8; 20];
    let pools = PoolTable::new(&[PoolEntry {
        address: addr,
        sym: 900,
        family: PoolFamily::Algebra,
        dec0: 18,
        dec1: 6,
    }])
    .expect("gate 66 pools");
    let mut transport = TestTransport::with_capacity(1 << 20);
    let mut driver = hwl::Driver::new(0xBEEF, pools, 4_000);
    hwl::note_transport_ready(&mut driver, core_net::Status::Ready);
    let status = core_metrics::IngressStatus::new();
    let ring: std::sync::Arc<Ring<core_types::Signal, { hwl::DEFAULT_POOL_RING_CAP }>> =
        Ring::new();
    let (mut prod, mut cons) = ring.split();
    let mut capture = core_types::NullCapture;

    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let mut sink = vec![0u8; 1 << 20];
    let _ = transport.drain_outgoing(&mut sink);
    let accept = core_net::expected_accept(&core_net::sec_websocket_key_from_seed(0xBEEF));
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&accept);
    resp.extend_from_slice(b"\r\n\r\n");

    // Everything the node will say, rendered now. Ids are the driver's:
    // 1 newHeads, 2 logs, 3.. the Algebra header reads in issue order
    // (globalState, liquidity, tickSpacing, prev, next, token0, token1,
    // probe), then the two `decimals()` reads (HYPARB H3b: 18 / 6, the
    // configured values) and the two list reads (both markers: an empty
    // book).
    let (sqrt, _) = core_amm::sqrt_at_tick(-297_448);
    let gs = |s: u128| {
        format!(
            "{:064x}{}{:064x}{}",
            s,
            word_i(-297_448),
            500u64,
            "0".repeat(192)
        )
    };
    let marker = format!(
        "{}{}{}{}{}",
        "0".repeat(128),
        word_i(-887_272),
        word_i(887_272),
        "0".repeat(64),
        "0".repeat(64)
    );
    let mut session = Vec::new();
    session.extend_from_slice(&resp);
    let mut setup = Vec::new();
    setup.extend_from_slice(&frame(
        br#"{"jsonrpc":"2.0","id":1,"result":"0x9cef478923ff08bf67fde6c64013158d"}"#,
    ));
    setup.extend_from_slice(&frame(
        br#"{"jsonrpc":"2.0","id":2,"result":"0x1111478923ff08bf67fde6c640131500"}"#,
    ));
    setup.extend_from_slice(&frame(format!(r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"0x9cef478923ff08bf67fde6c64013158d","result":{{"number":"0x{B:x}","timestamp":"0x68d2a1f3","baseFeePerGas":"0x5f5e100"}}}}}}"#).as_bytes()));
    let headers = [
        reply(3, &gs(sqrt)),
        reply(4, &format!("{:064x}", 77_000u64)),
        reply(5, &word_i(1)),
        reply(6, &word_i(-887_272)),
        reply(7, &word_i(887_272)),
        reply(8, &format!("{}{}", "0".repeat(24), "11".repeat(20))),
        reply(9, &format!("{}{}", "0".repeat(24), "22".repeat(20))),
        reply(10, &gs(sqrt + 1)),
    ];
    let decimals = [
        reply(11, &format!("{:064x}", 18u64)),
        reply(12, &format!("{:064x}", 6u64)),
    ];
    let links = [reply(13, &marker), reply(14, &marker)];
    let a: String = addr.iter().map(|b| format!("{b:02x}")).collect();
    let swap = frame(format!(
        r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"0x1111478923ff08bf67fde6c640131500","result":{{"address":"0x{a}","topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67","0x{z}","0x{z}"],"data":"0x{d0}{d1}{d2:064x}{d3:064x}{d4}","blockNumber":"0x{b:x}","logIndex":"0x1","removed":false}}}}}}"#,
        z = "0".repeat(64), d0 = word_i(12_345), d1 = word_i(-6_789), d2 = sqrt, d3 = 77_000u64, d4 = word_i(-297_448), b = B + 1
    ).as_bytes());

    transport.inject_incoming(&session);

    // ---- measurement window ----
    let g = AllocGuard::new();

    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let _ = transport.drain_outgoing(&mut sink);
    transport.inject_incoming(&setup);
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let _ = transport.drain_outgoing(&mut sink);
    let mut k = 0;
    while k < headers.len() {
        transport.inject_incoming(&headers[k]);
        k += 1;
    }
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let _ = transport.drain_outgoing(&mut sink);
    transport.inject_incoming(&decimals[0]);
    transport.inject_incoming(&decimals[1]);
    transport.inject_incoming(&links[0]);
    transport.inject_incoming(&links[1]);
    hwl::drive_one(
        &mut transport,
        &mut driver,
        b"h",
        b"/",
        &mut prod,
        &status,
        &mut capture,
    )
    .unwrap();
    let live = driver.phase() == hwl::Phase::Live;
    let mut acc: u64 = 0;
    while let Some(s) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(s.payload[0] as u64);
    }
    let mut n = 0u32;
    while n < 1_000 {
        transport.inject_incoming(&swap);
        hwl::drive_one(
            &mut transport,
            &mut driver,
            b"h",
            b"/",
            &mut prod,
            &status,
            &mut capture,
        )
        .unwrap();
        while let Some(s) = cons.try_pop_ref().as_deref().copied() {
            acc = acc.wrapping_add(s.payload[0] as u64);
        }
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(live, "the session must reach Live inside the window");
    assert_eq!(
        acc,
        0x27 + 0x11 + 1000 * (0x02 + 0x01),
        "one snapshot (Algebra SNAPSHOT + snapshot STATE), then SWAP + STATE per push"
    );
    assert_eq!(driver.snapshot_counters().pools_ok, 1);
    assert_eq!(
        allocs, 0,
        "hyperevm session allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hyperevm hot bytes should be zero: saw {bytes}");
}

/// **HYPARB gate 67 — the AMM fill law on the engine's paper matcher.**
///
/// `PaperDispatcher::observe_amm` in steady state: pool events rebuild
/// the book (swap + state with the fee observation, a position change),
/// a new AMM order per cycle is submitted, and every HEAD judges it —
/// a fill (the walk, the impact carried, the fill pushed) or a cancel —
/// and the fill is pumped. Boot (the first snapshot) is outside the
/// window. 0 B/op.
#[test]
fn amm_paper_matcher_observe_judge_and_fill_are_zero_alloc() {
    use clob_dispatcher::{OrderDispatch, PaperDispatcher};
    use core_amm::payload::{
        encode_head, encode_liquidity, encode_snapshot, encode_state, encode_swap, FAMILY_V3,
    };
    use core_amm::{price_1e18_from_sqrt, sqrt_at_tick, PoolMeta, PoolState, SwapSpec};
    use core_types::{make_symbol_id, Order, Side, SYMBOL_ID_NONE};

    const S: u64 = 1_000_000_000;
    let pool = make_symbol_id(VenueId::HyperEvm, 1);
    let tick = -230_543;
    let liq: u128 = 50_000_000_000_000_000_000;
    let (lo, hi) = sqrt_at_tick(tick);
    let mid = (price_1e18_from_sqrt(lo, hi, 18, 6) / 1_000_000_000_000) as i64;

    // Boot (allocation allowed): the snapshot, and the payloads the
    // loop replays — a real 0.3 %-paying swap and its state, a mint.
    let mut d = PaperDispatcher::new();
    d.observe_amm(
        pool,
        &encode_snapshot(10, FAMILY_V3, -240_000, -220_000, 0, 500, 10, 18, 6)
            .expect("gate 67 snap"),
        0,
    );
    let reset = encode_state(tick, lo, hi, liq, true).expect("gate 67 state");
    d.observe_amm(pool, &reset, 0);
    let mut meta = PoolMeta::ZERO;
    meta.fee_pips = 3_000;
    meta.tick_spacing = 10;
    let r = core_amm::swap_exact_in_range(
        &PoolState::new(lo, hi, tick, liq),
        &meta,
        &SwapSpec {
            amount: 1_000_000_000_000_000_000,
            limit_lo: core_amm::MIN_SQRT_LO + 1,
            limit_hi: 0,
            fee_pips: 3_000,
            zero_for_one: true,
            exact_in: true,
        },
    );
    let swap = encode_swap(11, r.amount_in as i128, -(r.amount_out as i128)).expect("gate 67 swap");
    let a = r.after;
    let post = encode_state(a.tick, a.sqrt_price_lo, a.sqrt_price_hi, a.liquidity, false)
        .expect("gate 67 post");
    let mint = encode_liquidity(11, false, -230_600, -230_500, 7).expect("gate 67 mint");

    const CYCLES: u64 = 10_000;
    let g = AllocGuard::new();
    let mut fills = 0u64;
    let mut n = 0u64;
    while n < CYCLES {
        let now = (n + 1) * 2 * S;
        // Chain activity: a snapshot-state reset (so the pool is live
        // again whatever our last impact did), a swap + its state, a mint.
        d.observe_amm(pool, &reset, now);
        d.observe_amm(pool, &swap, now);
        d.observe_amm(pool, &post, now);
        d.observe_amm(pool, &mint, now);
        // One order: a sell that fills on even cycles, a buy limited
        // to half the mid (cannot fill) on odd ones.
        let (side, px) = if n % 2 == 0 {
            (Side::Ask, mid * 99 / 100)
        } else {
            (Side::Bid, mid / 2)
        };
        let mut o = Order::new(
            now,
            VenueId::HyperEvm,
            pool,
            side,
            core_fill::ORDER_KIND_AMM_SWAP,
            Price::from_raw(px),
            Qty::from_raw(1_000_000),
            n + 1,
        );
        o.strategy_id = 0;
        d.submit(&o).expect("paper submit");
        let head = encode_head(12 + n, now + S, 1).expect("gate 67 head");
        d.observe_amm(SYMBOL_ID_NONE, &head, now + S);
        while let Some(f) = d.try_next_fill() {
            fills += 1;
            std::hint::black_box(f);
        }
        n += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    let c = d.matcher_counters();
    assert_eq!(
        fills,
        CYCLES / 2,
        "every sell fills, no buy at half the mid does"
    );
    assert_eq!(c.amm_fills, CYCLES / 2);
    assert_eq!(c.amm_canceled, CYCLES / 2);
    assert!(d.open_orders() == 0, "judged once, gone either way");
    assert_eq!(
        allocs, 0,
        "AMM paper matcher allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "AMM paper matcher hot bytes should be zero: saw {bytes}"
    );
}

/// **HYPARB H4 gate 68 — the slot-0 member in steady state.**
///
/// `HyparbStrategy` driven the way the engine drives it: every cycle the
/// chain resets the pool (a snapshot `STATE` — our carried impact
/// erased), a swap + its state and a mint arrive, a head, a funding
/// event, a perp BBO 1 % off the pool; the member decides (the real
/// `solve_arb` walk over its tick map), submits the AMM swap, carries
/// its impact, is filled, sends the hedge IoC, is filled on the hedge,
/// and its timer runs. Boot (params, the first snapshot and map load) is
/// outside the window. 0 B/op on `on_tick` / `on_signal` / `on_fill` /
/// `on_timer` / `on_venue_event`.
#[test]
fn hyparb_member_decision_hedge_and_timer_are_zero_alloc() {
    use core_amm::payload::{
        encode_head, encode_liquidity, encode_snapshot, encode_state, encode_swap, encode_tick,
        FAMILY_V3,
    };
    use core_amm::{price_1e18_from_sqrt, sqrt_at_tick};
    use core_types::{
        make_symbol_id, ChannelEvent, ChannelId, Fill, LatencyClass, Order, Signal, SignalSource,
        SYMBOL_ID_NONE,
    };
    use strategy_core::{Ctx, Strategy, SubmitErr};
    use strategy_hyparb::{CoinParams, HyparbParams, HyparbStrategy, PoolParams, COIN_USD};

    /// Keeps the last AMM order and the last hedge order — no storage
    /// that grows.
    struct LastCtx {
        amm: Option<Order>,
        hedge: Option<Order>,
        now: u64,
    }
    impl Ctx for LastCtx {
        fn submit(&mut self, o: Order) -> Result<(), SubmitErr> {
            if o.venue == VenueId::HyperEvm as u8 {
                self.amm = Some(o);
            } else {
                self.hedge = Some(o);
            }
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    const S: u64 = 1_000_000_000;
    const T0: u64 = 1_000 * S;
    let pool = make_symbol_id(VenueId::HyperEvm, 1);
    let perp = make_symbol_id(VenueId::Hyperliquid, 5);
    let tick = -230_543;
    let liq: u128 = 50_000_000_000_000_000_000;
    let (lo, hi) = sqrt_at_tick(tick);
    let mid = (price_1e18_from_sqrt(lo, hi, 18, 6) / 1_000_000_000_000) as i64;
    let sig = |sym, payload| {
        Signal::new(
            T0,
            sym,
            LatencyClass::Warm,
            SignalSource::HyperEvm as u8,
            payload,
        )
    };

    // Boot (allocation allowed).
    let mut p = HyparbParams::EMPTY;
    p.coins[0] = CoinParams {
        perp_sym: perp,
        spot_sym: SYMBOL_ID_NONE,
        lot_1e6: 10_000,
        min_notional_usd_1e6: 10_000_000,
    };
    p.n_coins = 1;
    p.pools[0] = PoolParams {
        sym: pool,
        coin0: 0,
        coin1: COIN_USD,
        trade: true,
        max_notional_usd_1e6: 1_000_000_000,
    };
    p.n_pools = 1;
    p.lag_ns = S / 2;
    p.basis_window_ns = 60 * S;
    p.depth_cap_enabled = true;
    p.gas_p50_usd_1e6 = 10_000;
    p.gas_p99_usd_1e6 = 3_910_000;
    p.max_order_usd_1e6 = 1_000_000_000;
    p.cap_day_usd_1e6 = 1_000_000_000_000_000;
    p.min_net_bps_1e6 = 5_000_000;
    p.inventory_cap_usd_1e6 = 1_000_000_000_000;
    p.perp_taker_bps_1e6 = 4_500_000;
    p.spot_taker_bps_1e6 = 7_000_000;
    p.funding_window_ns = 3_600 * S;
    p.cooldown_ns = S;
    let mut m = HyparbStrategy::new();
    m.configure(p, core_time::WallAnchor::new(0, 1_789_192_800 * S))
        .expect("gate 68 params");
    let mut c = LastCtx {
        amm: None,
        hedge: None,
        now: T0,
    };
    m.on_start(&mut c).expect("gate 68 start");
    m.on_signal(
        &sig(
            pool,
            encode_snapshot(10, FAMILY_V3, -240_000, -220_000, 2, 500, 10, 18, 6)
                .expect("gate 68 snap"),
        ),
        &mut c,
    );
    m.on_signal(
        &sig(
            pool,
            encode_tick(-240_000, liq as i128, liq).expect("gate 68 t"),
        ),
        &mut c,
    );
    m.on_signal(
        &sig(
            pool,
            encode_tick(-220_000, -(liq as i128), liq).expect("gate 68 t"),
        ),
        &mut c,
    );
    let reset = encode_state(tick, lo, hi, liq, true).expect("gate 68 state");
    m.on_signal(&sig(pool, reset), &mut c);
    assert_eq!(m.counters().maps_loaded, 1);
    let swap = encode_swap(11, 1_000_000_000_000, -97_000).expect("gate 68 swap");
    let post = encode_state(tick, lo, hi, liq, false).expect("gate 68 post");
    let mint = encode_liquidity(11, false, -230_600, -230_500, 7).expect("gate 68 mint");

    const CYCLES: u64 = 10_000;
    let g = AllocGuard::new();
    let mut n = 0u64;
    while n < CYCLES {
        let now = T0 + (n + 1) * 4 * S;
        c.now = now;
        m.on_signal(&sig(pool, reset), &mut c);
        m.on_signal(&sig(pool, swap), &mut c);
        m.on_signal(&sig(pool, post), &mut c);
        m.on_signal(&sig(pool, mint), &mut c);
        m.on_signal(
            &sig(
                SYMBOL_ID_NONE,
                encode_head(12 + n, now / S, 1).expect("head"),
            ),
            &mut c,
        );
        m.on_venue_event(
            &ChannelEvent::new(
                now,
                VenueId::Hyperliquid,
                ChannelId::AssetCtx,
                perp,
                0,
                0,
                12_500,
                0,
            ),
            &mut c,
        );
        // A perp 1 % above the pool, its size alternating so every tick
        // is a change: the member buys the pool.
        let px = mid * 101 / 100;
        let q = 100_000_000 + (n % 2) as i64;
        m.on_tick(
            &Tick::new(
                now,
                VenueId::Hyperliquid,
                perp,
                1,
                Price::from_raw(px - 500),
                Qty::from_raw(q),
                Price::from_raw(px + 500),
                Qty::from_raw(q),
            ),
            &mut c,
        );
        if let Some(o) = c.amm.take() {
            m.on_fill(
                &Fill::new(now, pool, o.side, o.px, o.qty, o.client_oid),
                &mut c,
            );
        }
        if let Some(h) = c.hedge.take() {
            m.on_fill(
                &Fill::new(now, perp, h.side, h.px, h.qty, h.client_oid),
                &mut c,
            );
        }
        m.on_timer(now + 2 * S, &mut c);
        n += 1;
    }
    let (allocs, bytes, _deallocs) = g.delta();
    let k = m.counters();
    assert!(k.arbs_submitted >= CYCLES, "one arb per cycle: {k:?}");
    assert_eq!(k.amm_fills, k.arbs_submitted);
    assert_eq!(k.hedge_fills, k.hedges_submitted);
    assert!(k.hedges_submitted >= CYCLES);
    assert_eq!(k.maps_refused, 0);
    assert_eq!(
        allocs, 0,
        "hyparb member allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "hyparb member hot bytes should be zero: saw {bytes}"
    );
}

/// **HYPARB gate 71 (H7c) — the EVM write path either side of the
/// socket.**
///
/// Per testnet send the arm thread encodes the executor calldata into a
/// stack array, bids gas, claims a nonce, signs, hashes, renders the
/// `eth_sendRawTransaction` body in place, and scans the node's answer;
/// per poll it renders the receipt request and scans the receipt into
/// caller storage (its logs walked, not searched); per block it scans
/// the fee history. The transaction is encoded ONCE (`PreparedTx`) and
/// signed, hashed and rendered from that encoding. `HttpsPost::post`
/// itself is gate 72 (its server in a child process). All of this must
/// be 0 B/op — the body buffer is allocated once at boot, outside the
/// guard.
#[test]
fn evm_arm_encode_bid_nonce_render_and_scan_are_zero_alloc() {
    use exec_hyperevm::calldata::{encode_swap, SwapCall, SWAP_CALLDATA_LEN};
    use exec_hyperevm::gas::{bid, SWAP_GAS_LIMIT};
    use exec_hyperevm::nonce::NonceTable;
    use exec_hyperevm::rpc::{
        classify_send_refusal, scan_hash, scan_next_base_fee, scan_receipt, write_receipt,
        write_send_raw,
    };
    use signer_evm::{tx_sign, Eip1559Tx, PreparedTx};

    let sk = signer_eip712::parse_secret_key(&[0x42; 32]).expect("gate 71 key");
    let mut rc = exec_hyperevm::rpc::Receipt::ZERO;
    let mut body = vec![0u8; exec_hyperevm::arm::MAX_BODY];
    let receipt = br#"{"jsonrpc":"2.0","id":7,"result":{"type":"0x2","status":"0x1","logs":[{"address":"0xd3303d83422e93b840cceed9d5671f2427fae726","topics":["0xe5451a8402e365c27dd14a967e40e72c37559b904cb33b29b9c2ee04a10d3d94"],"data":"0x01","blockNumber":"0x3e05ee7","transactionHash":"0xa0f288ad8674b31c431269cdfa13cc2db0a448d751e46c4cd4f729730f9a8cf7","logIndex":"0x0","removed":false}],"transactionHash":"0xa0f288ad8674b31c431269cdfa13cc2db0a448d751e46c4cd4f729730f9a8cf7","transactionIndex":"0x0","blockNumber":"0x3e05ee7","gasUsed":"0x70a5","effectiveGasPrice":"0x5f5e100","from":"0xeec1f3fcca6b05a7c9f05521f8dd9080e5edac14","to":"0xd3303d83422e93b840cceed9d5671f2427fae726","contractAddress":null}}"#;
    let fees = br#"{"jsonrpc":"2.0","id":8,"result":{"baseFeePerGas":["0x5f5e100","0x54f2d51"],"gasUsedRatio":[0.08],"oldestBlock":"0x3e05ed3"}}"#;
    let refused =
        br#"{"jsonrpc":"2.0","id":9,"error":{"code":-32000,"message":"transaction underpriced"}}"#;
    let answer = br#"{"jsonrpc":"2.0","id":9,"result":"0xa0f288ad8674b31c431269cdfa13cc2db0a448d751e46c4cd4f729730f9a8cf7"}"#;
    // Boot: the first signature builds signer-eip712's process-wide
    // secp256k1 context (gate 64's note).
    let warm = Eip1559Tx {
        chain_id: 998,
        nonce: 0,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: 0,
        gas_limit: 21_000,
        to: [0; 20],
        value: 0,
        data: &[],
    };
    tx_sign(&warm, &sk).expect("gate 71 warm-up");
    let mut nonces = NonceTable::new(3);
    let mut w = 0;
    while w < 3 {
        nonces.sync(w, 10 * w as u64, 10 * w as u64);
        w += 1;
    }

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut n = 0u64;
    while n < 2_000 {
        let call = SwapCall {
            amount_specified: 1_000_000 + n as i128,
            sqrt_limit_lo: 4_295_128_740 + n as u128,
            min_out: n as u128,
            sqrt_limit_hi: 0,
            pool: [0x77; 20],
            zero_for_one: n & 1 == 0,
        };
        let mut cd = [0u8; SWAP_CALLDATA_LEN];
        encode_swap(&call, &mut cd);
        let b = bid(
            1_000_000 + n as i64,
            100_000_000,
            SWAP_GAS_LIMIT,
            40_000_000,
            3_910_000,
        )
        .expect("gate 71 bid");
        let wallet = nonces.pick().expect("gate 71 wallet");
        let nonce = nonces.take(wallet).expect("gate 71 nonce");
        let tx = Eip1559Tx {
            chain_id: 998,
            nonce,
            max_priority_fee_per_gas: b.max_priority_fee_per_gas,
            max_fee_per_gas: b.max_fee_per_gas,
            gas_limit: SWAP_GAS_LIMIT,
            to: [0xe7; 20],
            value: 0,
            data: &cd,
        };
        let prepared = PreparedTx::call(&tx);
        let sig = prepared.sign(&sk).expect("gate 71 sign");
        let st = prepared.signed(&sig).expect("gate 71 attach");
        let h = st.hash();
        let k = write_send_raw(&mut body, 9, &st).expect("gate 71 render");
        let got = scan_hash(answer, 9).expect("gate 71 answer");
        if let Err(exec_hyperevm::rpc::ScanErr::Rpc(e)) = scan_hash(refused, 9) {
            let why =
                classify_send_refusal(&refused[e.message_start as usize..e.message_end as usize]);
            acc = acc.wrapping_add(why as u64);
        }
        let r = write_receipt(&mut body, 7, &h).expect("gate 71 receipt req");
        assert!(
            scan_receipt(receipt, 7, &mut rc).expect("gate 71 receipt"),
            "mined"
        );
        let fee = scan_next_base_fee(fees, 8).expect("gate 71 fees");
        nonces.mined(wallet);
        acc = acc
            .wrapping_add(k as u64 + r as u64)
            .wrapping_add(got[0] as u64 + rc.gas_used + fee as u64);
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(nonces.next(0), 667, "round-robin over the three wallets");
    assert_eq!(
        allocs, 0,
        "exec-hyperevm send/scan allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "exec-hyperevm hot bytes should be zero: saw {bytes}"
    );
}

/// Gate 72's server half: runs ONLY in the child process gate 72 spawns
/// (it is `#[ignore]`d, and a no-op without `GATE72_NODE_DIR`). Boots
/// the rustls `testnode`, writes its port and certificate for the
/// parent, and serves until the parent kills it (or 120 s pass).
#[test]
#[ignore = "gate 72's child process; never run on its own"]
fn gate72_node_helper() {
    let Some(dir) = std::env::var_os("GATE72_NODE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let certs = exec_hyperevm::testnode::certs();
    let n = exec_hyperevm::testnode::boot_with(
        exec_hyperevm::testnode::Node {
            chain_id: 998,
            ..exec_hyperevm::testnode::Node::default()
        },
        &certs,
    );
    std::fs::write(dir.join("cert.der"), certs.cert_der()).expect("gate 72 cert");
    // The port last: its presence says the rest is written.
    std::fs::write(dir.join("port.tmp"), n.port.to_string()).expect("gate 72 port");
    std::fs::rename(dir.join("port.tmp"), dir.join("port")).expect("gate 72 port");
    std::thread::sleep(std::time::Duration::from_secs(120));
}

/// **HYPARB gate 72 (H9) — the write arm's HTTPS keep-alive cycle, as
/// allocations per request.**
///
/// Not 0 B/op, and pinned anyway: rustls 0.23's BUFFERED API allocates
/// one `Vec` per TLS record it seals (each `write` call) and one per
/// application-data record it decrypts — measured 2026-09-23. What this
/// gate owns is everything ELSE: `HttpsPost` renders the request as ONE
/// contiguous slice written ONCE (one record), reads in place, and the
/// transport's `WouldBlock` is `io::Error::from(kind)` (it was
/// `Error::new(kind, "…")`: three allocations on EVERY drain loop's last
/// read, in every TLS ingress thread). Measured before H9: 6
/// allocations/post; after: exactly 2 — rustls' residue. A regression in
/// our code shows as a third. Removing the residue needs rustls'
/// unbuffered API (`UnbufferedClientConnection`) — a core-net transport
/// decision, recorded, not taken here.
///
/// The server runs in a CHILD process (this binary's
/// [`gate72_node_helper`]): the counting allocator is process-global, so
/// a server thread here would count its own allocations.
#[test]
fn https_post_keep_alive_cycle_allocates_only_rustls_record_buffers() {
    const POSTS: u64 = 500;
    const RUSTLS_ALLOCS_PER_POST: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate72-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 72 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 72 exe"))
        .args([
            "gate72_node_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE72_NODE_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 72 child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 72 port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 72: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 72 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 72 anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut h = core_net::HttpsPost::new("localhost", port, "/evm", cfg, 8192, 16 * 1024)
        .expect("gate 72 client");
    let n = exec_hyperevm::rpc::write_chain_id(h.body_mut(), 1).expect("gate 72 body");
    let mut i = 0;
    while i < 50 {
        // The handshake, the session tickets and rustls' queues growing
        // to their working size: the cold part.
        let (status, _) = h.post(n).expect("gate 72 warm-up");
        assert_eq!(status, 200);
        i += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut k = 0u64;
    while k < POSTS {
        let (status, r) = h.post(n).expect("gate 72 post");
        acc = acc.wrapping_add(u64::from(status) + (r.end - r.start) as u64);
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(h.is_connected(), "one keep-alive connection throughout");
    assert_eq!(h.dials(), 1, "no redial inside the measurement");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_POST * POSTS,
        "HttpsPost cycle: {allocs} allocations ({bytes} B) over {POSTS} posts — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_POST}/post (one sealed record \
         out, one decrypted record in); anything above is ours"
    );
}

/// **XMM gate 101 (HD2) — the Hyperliquid arm's HTTPS cycle, as
/// allocations per post: exactly gate 72's two.** Since HD2 `HlHttp` is
/// a face over `HttpsPost` (templates `POST /exchange` and `POST /info`):
/// the order renders in place in the `/exchange` window and goes out as
/// ONE slice in ONE write — one sealed record while it is ≤ 16 KiB —
/// where the pre-HD2 client wrote its head and its body as two (two
/// sealed records: 3 per post by gate 72's one-record-per-write rule,
/// never gated).
/// rustls' buffered API seals one record out and decrypts one in; a
/// regression in our code shows as a third. The bodies alternate a
/// two-digit and a three-digit `Content-Length`, so the head moves on
/// every post, inside the measurement. `/info` is the same engine path
/// on template 1 (crate-private; its bytes are pinned in core-net).
/// Served by gate 72's child-process node (the counting allocator is
/// process-global), which answers by JSON-RPC method, whatever the path.
#[test]
fn hl_http_keep_alive_cycle_allocates_only_rustls_record_buffers() {
    const POSTS: u64 = 500;
    const RUSTLS_ALLOCS_PER_POST: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate101-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 101 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 101 exe"))
        .args([
            "gate72_node_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE72_NODE_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 101 child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 101 port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 101: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 101 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 101 anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut h = exec_hyperliquid::HlHttp::new("localhost", port, cfg).expect("gate 101 client");
    let mut k = 0u64;
    while k < 50 {
        // The handshake, the session tickets and rustls' queues growing
        // to their working size: the cold part.
        assert_eq!(gate101_post(&mut h, k).0, 200, "gate 101 warm-up");
        k += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    while k < 50 + POSTS {
        let (status, len) = gate101_post(&mut h, k);
        assert_eq!(status, 200, "gate 101 post {k}");
        acc = acc.wrapping_add(u64::from(status) + len as u64);
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(h.is_connected(), "one keep-alive connection throughout");
    assert_eq!(h.dials(), 1, "no redial inside the measurement");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_POST * POSTS,
        "HlHttp cycle: {allocs} allocations ({bytes} B) over {POSTS} posts — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_POST}/post (one sealed record \
         out, one decrypted record in); anything above is ours"
    );
}

/// Gate 101's request `k`: an `eth_chainId` (a two-digit length) when
/// `k` is even, an `eth_getTransactionCount` (three digits) when odd —
/// rendered in place in the `/exchange` window, then posted. The status
/// and the answer body's length.
fn gate101_post(h: &mut exec_hyperliquid::HlHttp, k: u64) -> (u16, usize) {
    use exec_hyperevm::rpc::{write_chain_id, write_tx_count, BlockTag};
    let n = if k & 1 == 0 {
        write_chain_id(h.body_mut(), k)
    } else {
        write_tx_count(h.body_mut(), k, &[0x11; 20], BlockTag::Latest)
    }
    .expect("gate 101 body");
    let (status, r) = h.post(n).expect("gate 101 post");
    (status, r.end - r.start)
}

/// **HC2 gate 77a — the generic HTTP/1.1 head writer is 0 B/op.** Every
/// Hypercall REST head — a `GET` with a query, a signed `DELETE` with
/// two extra headers — is sized and rendered into a caller-owned buffer
/// without touching the heap.
#[test]
fn http1_request_head_writer_is_zero_alloc() {
    use core_net::{request_head_len, write_request_head, Header, Method, ReqHead};
    let extra: [Header<'_>; 2] = [
        (b"X-Hypercall-Expires-At-Ms", b"1790371200000"),
        (b"X-Hypercall-Signature", b"0x1b2c3d4e5f"),
    ];
    let heads = [
        ReqHead {
            method: Method::Get,
            host: b"api.hypercall.xyz",
            target: b"/options-summary?currency=BTC&include_rfq_provider_quotes=true",
            user_agent: core_net::REQ_USER_AGENT,
            content_type: None,
            extra: &[],
            keep_alive: true,
        },
        ReqHead {
            method: Method::Delete,
            host: b"api.hypercall.xyz",
            target: b"/order",
            user_agent: core_net::REQ_USER_AGENT,
            content_type: Some(b"application/json"),
            extra: &extra,
            keep_alive: true,
        },
    ];
    let mut buf = [0u8; 512];
    // Warm-up outside the window.
    let _ = write_request_head(&mut buf, &heads[0], 0);
    let g = AllocGuard::new();
    let mut acc = 0usize;
    let mut i = 0usize;
    while i < 10_000 {
        let h = &heads[i & 1];
        let body_len = (i & 1) * 57;
        let n = request_head_len(h, body_len).expect("sized");
        let w = write_request_head(&mut buf, h, body_len).expect("rendered");
        acc = acc.wrapping_add(n ^ w ^ usize::from(buf[w - 1]));
        i += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    assert_eq!(
        allocs, 0,
        "http1 head writer allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// **HC2 gate 77b — `HttpsReq`'s keep-alive cycle, as allocations per
/// request: exactly gate 72's two.** `HttpsReq` renders its head per
/// request (any method, any target) flush against the body already in
/// place and writes ONE contiguous slice ONCE, over the same connection
/// engine as `HttpsPost` (`core_net::https_conn`; since XMM HD1 through
/// `HttpsPost::raw` → `HttpsConn::start_raw` — before it this pin measured
/// the deleted `KeepAlive` copy, at the same 2). rustls' buffered API
/// seals one record out and decrypts one in; a regression in our render
/// or in the shared engine shows as a third. Same child-process server
/// as gate 72 (the counting allocator is process-global).
#[test]
fn https_req_keep_alive_cycle_allocates_only_rustls_record_buffers() {
    const REQS: u64 = 500;
    const RUSTLS_ALLOCS_PER_REQ: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate73-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 77b dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 77b exe"))
        .args([
            "gate72_node_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE72_NODE_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 77b child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 77b port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 77b: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 77b cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 77b anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut h = core_net::HttpsReq::new("localhost", port, cfg, 1024, 8192, 16 * 1024)
        .expect("gate 77b client");
    let n = exec_hyperevm::rpc::write_chain_id(h.body_mut(), 1).expect("gate 77b body");
    let extra: [core_net::Header<'_>; 1] = [(b"X-Gate", b"73")];
    let mut i = 0;
    while i < 50 {
        // The handshake, the session tickets and rustls' queues growing
        // to their working size: the cold part.
        let (status, _) = h
            .request(core_net::Method::Post, b"/evm", &extra, n)
            .expect("gate 77b warm-up");
        assert_eq!(status, 200);
        i += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut k = 0u64;
    while k < REQS {
        let (status, r) = h
            .request(core_net::Method::Post, b"/evm", &extra, n)
            .expect("gate 77b request");
        acc = acc.wrapping_add(u64::from(status) + (r.end - r.start) as u64);
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(h.is_connected(), "one keep-alive connection throughout");
    assert_eq!(h.dials(), 1, "no redial inside the measurement");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_REQ * REQS,
        "HttpsReq cycle: {allocs} allocations ({bytes} B) over {REQS} requests — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_REQ}/request (one sealed record \
         out, one decrypted record in); anything above is ours"
    );
}

// ---------------------------------------------------------------
// HC3: Hypercall ingress hot-path assertions (gates 78, 79)
// ---------------------------------------------------------------

/// The golden frames, captured live from the Mac on 2026-09-25 (HC0).
const HC_Q1: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/quote_one_provider.json");
const HC_Q2X: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/quote_two_providers_crossed.json");
const HC_Q0: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/quote_empty.json");
const HC_IDX: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/index_update.json");
const HC_TRADE: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/trade_docs_example.json");
const HC_MU_EXPIRED: &[u8] =
    include_bytes!("../../ingress-hypercall/tests/fixtures/market_update_expired_schema.json");
const HC_CLOCK: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/clock_synced.json");
const HC_SUBSCRIBED: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/subscribed.json");
const HC_CLOSE_ML: &[u8] =
    include_bytes!("../../ingress-hypercall/tests/fixtures/close_reason_message_limit.json");
const HC_SUMMARY: &[u8] = include_bytes!("../../ingress-hypercall/tests/fixtures/options_summary_trimmed.json");

/// The gates' universe: the golden frames' instruments, and two of the
/// index frame's twelve underlyings (boot side, outside every window).
fn hc_tables() -> (ingress_hypercall::HcSymbolTable, ingress_hypercall::HcUnderlyings) {
    let mut t = ingress_hypercall::HcSymbolTable::new();
    t.insert(b"ETH-20260927-2675-P", (9 << 24) | 512).unwrap();
    t.insert(b"MU-20260928-1090-C", (9 << 24) | 513).unwrap();
    t.insert(b"BTC-20261002-100000-C", (9 << 24) | 514).unwrap();
    t.insert(b"AAPL-20260926-300-C", (9 << 24) | 515).unwrap();
    let mut u = ingress_hypercall::HcUnderlyings::new();
    u.insert(b"AAPL", (9 << 24) | 1).unwrap();
    u.insert(b"BTC", (9 << 24) | 2).unwrap();
    (t, u)
}

/// `src` with every `from` replaced by the same-length `to` — a second
/// touch of a golden quote (boot side).
fn hc_swap_all(src: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len(), "a same-length swap keeps the frame shape");
    let mut out = src.to_vec();
    let mut i = 0;
    let mut hits = 0;
    while i + from.len() <= out.len() {
        if &out[i..i + from.len()] == from {
            out[i..i + from.len()].copy_from_slice(to);
            i += from.len();
            hits += 1;
        } else {
            i += 1;
        }
    }
    assert!(hits > 0, "the swap must change the frame");
    out
}

/// **HC3 gate 78 — every Hypercall parser is 0 B/op.** Classify over
/// every message kind; the indicative quote (one provider, two providers
/// CROSSED, empty) with its symbol-table lookup and the provider walk;
/// the 12-underlying index frame; a trade; a listing update; ClockSynced;
/// a venue error; the 1008 close reason; the outbound side (the universe
/// subscribe as parts, the ClockSync nonce render); and the REST
/// poller's summary scan into `OptSummary` rows — 10 000 iterations.
#[test]
fn hypercall_parsers_are_zero_alloc() {
    use ingress_hypercall as hc;
    let (table, _) = hc_tables();
    let error_frame: &[u8] = br#"{"type":"Error","message":"unknown channel"}"#;
    let kinds: [&[u8]; 9] = [
        HC_Q1,
        HC_Q2X,
        HC_Q0,
        HC_IDX,
        HC_TRADE,
        HC_MU_EXPIRED,
        HC_CLOCK,
        HC_SUBSCRIBED,
        error_frame,
    ];
    let mut parts: [&[u8]; hc::SUBSCRIBE_PARTS_MAX] = [&[]; hc::SUBSCRIBE_PARTS_MAX];

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    let mut i = 0u64;
    while i < 10_000 {
        let mut k = 0;
        while k < kinds.len() {
            std::hint::black_box(hc::classify(kinds[k]));
            k += 1;
        }
        // The quote path: in place, the lookup, the provider walk.
        let mut q = hc::HcQuote::ZERO;
        let meta = hc::parse_indicative(HC_Q2X, &mut q).expect("gate 78 quote");
        acc = acc.wrapping_add(q.bid_px_1e6 - q.ask_px_1e6 + i64::from(meta.num_providers));
        let (row, sym) = table.lookup(hc::span_bytes(HC_Q2X, q.instrument)).expect("gate 78 lookup");
        acc = acc.wrapping_add(row as i64 + i64::from(sym));
        let mut ps = [hc::HcProvider::default(); hc::HC_MAX_PROVIDERS];
        let (read, present) = hc::walk_providers(HC_Q2X, q.providers, &mut ps).expect("gate 78 providers");
        acc = acc.wrapping_add(ps[1].ask_px_1e6 + i64::from(read) + i64::from(present));
        acc = acc.wrapping_add(hc::provider_quote_seq(1, true, meta.num_providers, ps[1].wallet_lo32) as i64);
        let mut q1 = hc::HcQuote::ZERO;
        assert_eq!(hc::parse_indicative(HC_Q1, &mut q1).map(|m| m.sides), Some(hc::SIDE_BID | hc::SIDE_ASK));
        let mut q0 = hc::HcQuote::ZERO;
        assert_eq!(hc::parse_indicative(HC_Q0, &mut q0).map(|m| m.sides), Some(0));
        // The index frame, every entry in place.
        let mut xs = [hc::HcIndexEntry::default(); hc::HC_MAX_UNDERLYINGS];
        let (n, all, ts) = hc::parse_index_update(HC_IDX, &mut xs).expect("gate 78 index");
        acc = acc.wrapping_add(xs[3].price_1e6 + i64::from(n) + i64::from(all) + ts as i64);
        // Trades, listings, the clock, errors, the close reason.
        let t = hc::parse_trade(HC_TRADE).expect("gate 78 trade");
        acc = acc.wrapping_add(t.px_1e6 + t.signed_qty_1e6);
        let (action, name, ts) = hc::parse_market_update(HC_MU_EXPIRED).expect("gate 78 listing");
        acc = acc.wrapping_add(
            i64::from(action == hc::HcListingAction::Expired) + hc::span_bytes(HC_MU_EXPIRED, name).len() as i64 + ts as i64,
        );
        let (nonce, server_at) = hc::parse_clock_synced(HC_CLOCK).expect("gate 78 clock");
        acc = acc.wrapping_add((nonce ^ server_at) as i64);
        acc = acc.wrapping_add(hc::parse_error(error_frame).map_or(-1, |s| i64::from(s.1 - s.0)));
        acc = acc.wrapping_add(hc::parse_close_reason(HC_CLOSE_ML) as i64);
        // The outbound side: the universe subscribe as parts, a nonce.
        let n = hc::subscribe_parts(hc::CH_INDICATIVE, Some(&table), &mut parts).expect("gate 78 parts");
        acc = acc.wrapping_add(n as i64 + parts[n - 1].len() as i64);
        let mut digits = [0u8; 20];
        let d = hc::fmt_u64(1_790_373_478_201 + i, &mut digits);
        acc = acc.wrapping_add(hc::clock_sync_parts(d)[1].len() as i64);
        // The poller's scan: REST rows → `OptSummary`.
        let rows = hc::rest::parse_summary_rows(HC_SUMMARY, |r| {
            acc = acc.wrapping_add(hc::rest::to_opt_summary(i, (9 << 24) | 515, r).mark_iv_1e9);
        })
        .expect("gate 78 summary");
        acc = acc.wrapping_add(rows as i64);
        i += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hypercall parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hypercall parser bytes should be zero: saw {bytes}");
}

/// **HC3 gate 79 — the Hypercall run loop in steady state is 0 B/op.**
/// The real handshake (GET → 101 → ClockSync + the four subscribes, the
/// indicative one naming the universe in ONE frame) and the four acks
/// are boot; then CYCLES rounds of the measured wire mix are injected,
/// driven and drained the way the engine drains its lanes: a
/// one-provider quote alternating between two touches (the emit path)
/// plus its unchanged republication (the dedupe path), a CROSSED
/// two-provider quote alternating too (4 `ProviderQuote` events), the
/// 12-underlying index frame (2 configured → 2 `Mark`s), a trade, a
/// `ClockSynced` and a server Ping (the Pong goes rx → tx), and one
/// poller row through the SPSC handoff onto opt lane 3 — with a REAL
/// `PmlrCapture` (raw tap `All`).
#[test]
fn hypercall_run_loop_steady_state_is_zero_alloc() {
    use core_types::{event_lane_bit, ChannelEvent, ChannelId, OptSummary, EVENT_RING_SIZE, OPT_RING_SIZE};
    use ingress_hypercall::counters::get;
    use ingress_hypercall::run_loop as hwl;

    // ---- boot (NOT measured) ----
    const CYCLES: usize = 300;
    const SEED: u64 = 0x4C09;
    const HOST: &[u8] = b"api.hypercall.xyz";
    let (table, unds) = hc_tables();
    let mut drv = hwl::Driver::new(SEED, table, unds);
    // The golden frames carry FIXED venue stamps: disable the stale
    // judgement so a slow (debug) run cannot flip a verdict mid-stream
    // and add a tick (a flipped verdict is a tick by the dedupe law).
    drv.set_stale_after_ms(0);
    let mut t = TestTransport::with_capacity(256 * 1024);
    let status = core_metrics::IngressStatus::new();
    let counters = ingress_hypercall::HcCounters::new();
    let (mut tick_tx, mut tick_rx) = Ring::<Tick, { hwl::TICK_RING_CAP }>::new().split();
    let (mut ev_tx, mut ev_rx) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let (mut opt_tx, mut opt_rx) = Ring::<OptSummary, OPT_RING_SIZE>::new().split();
    let (mut hand_tx, mut hand_rx) = Ring::<OptSummary, { hwl::HANDOFF_RING_CAP }>::new().split();
    let mut lanes = hwl::Lanes {
        ticks: &mut tick_tx,
        events: &mut ev_tx,
        event_mask: event_lane_bit(ChannelId::ProviderQuote)
            | event_lane_bit(ChannelId::Mark)
            | event_lane_bit(ChannelId::Trade),
        opts: &mut opt_tx,
    };
    let cap_dir = std::env::temp_dir().join(format!("hypercall_bench_cap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cap_dir);
    let mut capture = core_io::PmlrCapture::open(
        &cap_dir,
        "hypercall",
        0,
        core_io::TapCfg {
            mode: core_io::TapMode::All,
            budget_bytes: 8 * 1024 * 1024,
        },
    )
    .unwrap();

    /// Unmasked server→client frame (`first` = 0x81 text / 0x89 ping).
    fn push_frame(stream: &mut Vec<u8>, first: u8, body: &[u8]) {
        stream.push(first);
        if body.len() <= 125 {
            stream.push(body.len() as u8);
        } else {
            stream.push(126);
            stream.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        stream.extend_from_slice(body);
    }

    // The real handshake: GET → 101 → ClockSync + four subscribes.
    hwl::note_transport_ready(&mut drv, core_net::Status::Ready);
    hwl::drive_one(&mut t, &mut drv, HOST, &mut lanes, &status, &counters, &mut capture).unwrap();
    let mut scratch = [0u8; 8192];
    let _ = t.drain_outgoing(&mut scratch);
    let accept = core_net::expected_accept(&core_net::sec_websocket_key_from_seed(SEED));
    let mut resp = Vec::new();
    resp.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ");
    resp.extend_from_slice(&accept);
    resp.extend_from_slice(b"\r\n\r\n");
    t.inject_incoming(&resp);
    hwl::drive_one(&mut t, &mut drv, HOST, &mut lanes, &status, &counters, &mut capture).unwrap();
    assert_eq!(drv.state(), hwl::State::Steady);
    let _ = t.drain_outgoing(&mut scratch); // the ClockSync + the subscribe set
    let mut acks = Vec::new();
    for ch in ["index_prices", "trades", "market_updates", "indicative_market_data"] {
        push_frame(&mut acks, 0x81, format!("{{\"type\":\"Subscribed\",\"channel\":\"{ch}\"}}").as_bytes());
    }
    t.inject_incoming(&acks);
    hwl::drive_one(&mut t, &mut drv, HOST, &mut lanes, &status, &counters, &mut capture).unwrap();
    assert_eq!(drv.sub_count(), 4, "every channel acked");

    // Two touches per quote; a cycle's stream alternates between them.
    let q1_alt = hc_swap_all(HC_Q1, b"\"7.679\"", b"\"7.681\"");
    let q2x_alt = hc_swap_all(HC_Q2X, b"\"18.6445\"", b"\"18.6451\"");
    let mut streams = [Vec::with_capacity(4096), Vec::with_capacity(4096)];
    for (s, q1, q2x) in [(0usize, HC_Q1, HC_Q2X), (1, &q1_alt[..], &q2x_alt[..])] {
        push_frame(&mut streams[s], 0x81, q1);
        push_frame(&mut streams[s], 0x81, q1); // republication: no tick
        push_frame(&mut streams[s], 0x81, q2x);
        push_frame(&mut streams[s], 0x81, HC_IDX);
        push_frame(&mut streams[s], 0x81, HC_TRADE);
        push_frame(&mut streams[s], 0x81, HC_CLOCK);
        push_frame(&mut streams[s], 0x89, b"PING");
    }
    let poller_row = OptSummary::new(1, VenueId::Hypercall, (9 << 24) | 515, 0, 1, 2, 3, 4, 5, 6, 7, 8);
    let mut pong = [0u8; 64];

    // ---- measurement window ----
    let g = AllocGuard::new();

    let mut acc: i64 = 0;
    let (mut ticks, mut events, mut opts, mut handed) = (0usize, 0usize, 0usize, 0usize);
    let mut cycle = 0usize;
    while cycle < CYCLES {
        let s = &streams[cycle & 1];
        assert_eq!(t.inject_incoming(s), s.len());
        let mut drives = 0u32;
        while t.incoming_len() > 0 {
            hwl::drive_one(&mut t, &mut drv, HOST, &mut lanes, &status, &counters, &mut capture).unwrap();
            drives += 1;
            assert!(drives <= 64, "scripted stream failed to drain");
        }
        assert_eq!(t.drain_outgoing(&mut pong), 2 + 4 + 4, "one masked Pong echoing PING");
        // The poller's round, through the handoff (the poller's own
        // thread in production).
        assert!(hand_tx.try_push_ref(&poller_row));
        handed += hwl::drain_handoff(&mut hand_rx, &mut lanes, &status, &mut capture);
        // The engine's side: every lane read in place.
        while let Some(x) = tick_rx.try_pop_ref() {
            acc = acc.wrapping_add(x.bid_px.raw() - x.ask_px.raw());
            ticks += 1;
        }
        while let Some(e) = ev_rx.try_pop_ref() {
            acc = acc.wrapping_add(e.v0);
            events += 1;
        }
        while let Some(o) = opt_rx.try_pop_ref() {
            acc = acc.wrapping_add(o.mark_iv_1e9);
            opts += 1;
        }
        cycle += 1;
    }
    core_types::Capture::maybe_flush(&mut capture, core_io::CAPTURE_FLUSH_INTERVAL_NS + 1);
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // One tick per BBO CHANGE of either quote (never per republication);
    // per cycle 4 ProviderQuote + 2 Mark + 1 Trade on the lane; one opt
    // row; every text frame counted; nothing lost.
    assert_eq!(ticks, 2 * CYCLES);
    assert_eq!(events, 7 * CYCLES);
    assert_eq!((handed, opts), (CYCLES, CYCLES));
    assert_eq!(status.msgs_total(), (4 + 6 * CYCLES) as u64);
    assert_eq!(status.parse_errors_total(), 0);
    assert_eq!(status.ring_drops_total(), 0);
    assert_eq!(status.event_ring_drops_total(), 0);
    assert_eq!(status.opt_ring_drops_total(), 0);
    assert_eq!(drv.sub_count(), 5, "four acks + the first quote");
    assert_eq!(get(&counters.ws.crossed_quotes), CYCLES as u64);
    assert_eq!(get(&counters.ws.provider_quotes), (4 * CYCLES) as u64);
    assert_eq!(get(&counters.ws.clock_syncs), CYCLES as u64);
    assert_eq!(allocs, 0, "hypercall run-loop allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hypercall run-loop bytes should be zero: saw {bytes}");

    // Capture accounting: a tick per BBO change, every event, one opt
    // row per round, a tap record per text payload (the 4 acks + 6 per
    // cycle), no I/O errors.
    assert!(!capture.is_disabled());
    assert_eq!(capture.io_errors(), 0);
    assert_eq!(capture.ticks_written(), (2 * CYCLES) as u64);
    assert_eq!(capture.events_written(), (7 * CYCLES) as u64);
    assert_eq!(capture.opt_summaries_written(), CYCLES as u64);
    assert_eq!(capture.tap_records(), (4 + 6 * CYCLES) as u64);
    assert_eq!(capture.tap_dropped(), 0);
    drop(capture);
    let _ = std::fs::remove_dir_all(&cap_dir);
}

/// **HC7 gate 80 — the settlement replicator is 0 B/op.** A 30-minute
/// window of oracle prints (one every 700 ms, a trending walk with
/// spikes, starting before the window so a price carries in), the grid
/// closed at `T`, and the median-of-means under BOTH bucket orders —
/// then the window reset and refilled for the next expiry, 20 times.
/// The window and the scratch are boot-boxed (the only allocations).
#[test]
fn settlement_window_and_median_of_means_are_zero_alloc() {
    use core_settle::{BucketOrder, SettleWindow, GRID_POINTS, SETTLE_WINDOW_MS};
    const T0: u64 = 1_790_366_400_000;
    let mut w = Box::new(SettleWindow::new(T0));
    let mut scratch = Box::new([0i64; GRID_POINTS]);

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    let mut e = 0u64;
    while e < 20 {
        let t_end = T0 + e * 86_400_000;
        w.reset(t_end);
        let mut ts = t_end - SETTLE_WINDOW_MS - 5_000;
        let mut px: i64 = 224_000_000;
        let mut i = 0u64;
        while ts <= t_end {
            px += ((i * 7_919) % 95_001) as i64 - 40_000;
            let spike = if i % 211 == 7 { 3_000_000 } else { 0 };
            if w.push(ts, px + spike).is_err() {
                acc = acc.wrapping_add(1);
            }
            ts += 700;
            i += 1;
        }
        acc = acc.wrapping_add(w.points() as i64);
        acc = acc.wrapping_add(w.settle_1e6(BucketOrder::Sorted, &mut scratch).unwrap_or(0));
        acc = acc.wrapping_add(w.settle_1e6(BucketOrder::Time, &mut scratch).unwrap_or(0));
        e += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    assert_eq!(w.points(), GRID_POINTS, "a full grid every expiry");
    assert_eq!(allocs, 0, "settlement replicator allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0);
}

/// The long-tenor gate's price walk: a slow 40-day vol regime so the
/// fold's regressor varies and the fits exist (test-only, no model).
fn long_vol_px(s: &mut u64, px: &mut i64, day: u64) -> i64 {
    *s = s
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let phase = day % 40;
    let level = if phase < 20 { phase } else { 40 - phase };
    let amp = 2_000_000 * (4 + level) as i64;
    *px = (*px + ((*s >> 32) % (2 * amp as u64 + 1)) as i64 - amp).max(1_000_000_000);
    *px
}

/// **HAR H1 gate 81 — the long-tenor engine is 0 B/op.** Sixty UTC days
/// of minute closes (86 400) through a WARM engine whose short tenors
/// are fitted: every minute's return and add, every day close's settle +
/// pair + refit + arm over the whole 1–40 d grid (the 1 d pair ring and
/// the QLIKE ring wrap), and every tenor's forecasts, fit and QLIKE tell
/// read once an hour. The engine is boot-boxed and
/// warmed by 100 unmeasured days (the only allocation is the box).
#[test]
fn long_vol_is_zero_alloc() {
    use core_vol::{LongForecast, LongVolEngine, DAY_MS, DAY_NS, LONG_TAU_DAYS_MAX};
    const DAY0: u64 = 1_767_225_600_000; // 2026-01-01 00:00Z
    let mut e = Box::new(LongVolEngine::new());
    let mut s: u64 = 20_260_926;
    let mut px: i64 = 79_000_000_000;
    let mut day = 0u64;
    while day < 100 {
        let mut m = 0u64;
        while m < 1440 {
            e.on_minute_close_at(long_vol_px(&mut s, &mut px, day), DAY0 + day * DAY_MS + m * 60_000);
            m += 1;
        }
        day += 1;
    }
    assert!(e.is_warm());
    assert!(e.fit(DAY_NS).is_some(), "the gate must measure a FITTED engine");

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    while day < 160 {
        let mut m = 0u64;
        while m < 1440 {
            e.on_minute_close_at(long_vol_px(&mut s, &mut px, day), DAY0 + day * DAY_MS + m * 60_000);
            if m % 60 == 0 {
                let mut d = 1u64;
                while d <= LONG_TAU_DAYS_MAX as u64 {
                    let t = d * DAY_NS;
                    acc = acc.wrapping_add(e.sigma_ann_1e9(t, LongForecast::Raw).unwrap_or(0));
                    acc = acc.wrapping_add(e.sigma_ann_1e9(t, LongForecast::Fit).unwrap_or(0));
                    acc = acc.wrapping_add(e.qlike_counters(t).fit_mean_1e9);
                    acc = acc.wrapping_add(e.n_pairs(t) as i64);
                    d += 1;
                }
            }
            m += 1;
        }
        day += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(e.n_pairs(DAY_NS), core_vol::PAIR_RING_LONG, "the 1 d ring wrapped under the guard");
    assert_eq!(allocs, 0, "long-tenor engine allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "long-tenor engine bytes should be zero: saw {bytes}");
}

/// **HAR H3.3 gate 82 — the long-tenor SET is 0 B/op.** Twelve series
/// (the Hypercall underlyings) configured and warmed by 35 unmeasured days
/// (the boxes are the only allocation — boot). Under the guard, two more
/// UTC days of the engine loop's traffic: every minute a fresh quote of
/// all twelve feeds among stale, one-sided and stranger ticks, the 1 s
/// poll's rolls, and at each UTC boundary the STAGGERED day close — one
/// series per poll, eleven held and released — with every forecast, the
/// weekday profile and the counters read once an hour.
///
/// **HAR H3.7, the second half — the set as the engine holds it.** The
/// same traffic through a `StrategySet` with the twelve series and the
/// state writer's outbox installed (twelve boxed mailboxes — boot): its
/// own 1 s HAR timer, the `/state.har` rows rebuilt at each close, every
/// series' state copied whole into its mailbox at its close and taken
/// back (the writer thread's side, here on one thread), and once an hour
/// `har_series_view` + `har_counters` + `har_gauges` — the report
/// block's `/state.har` and `engine_har_*` reads.
#[test]
fn long_vol_set_is_zero_alloc() {
    use core_time::WallAnchor;
    use core_types::{make_symbol_id, Order, TICK_FLAG_STALE};
    use core_vol::{
        LongForecast, LongSeries, LongStateSnap, LongVolSet, DAY_NS, LONG_SET_MAX,
        LONG_TAU_DAYS_MAX,
    };
    use strategy_core::{Ctx, HarSeriesView, Strategy, StrategyCounters, SubmitErr, HAR_VIEW_SERIES};
    use strategy_set::{StrategySet, BIT_AI_EXEC};
    const MIN_NS: u64 = 60_000_000_000;
    const SEC_NS: u64 = 1_000_000_000;
    const NAMES: [&[u8]; LONG_SET_MAX] = [
        b"SP500", b"SPCX", b"MU", b"NVDA", b"MSFT", b"META", b"AAPL", b"BABA", b"SNDK", b"BOT",
        b"BTC", b"ETH",
    ];
    fn quote(ts: u64, sym: SymbolId, px: i64, flags: u8) -> Tick {
        let mut t = Tick::new(
            ts,
            VenueId::Binance,
            sym,
            0,
            Price::from_raw(px),
            Qty::from_raw(1),
            Price::from_raw(px + 2),
            Qty::from_raw(1),
        );
        t.flags = flags;
        t
    }
    let feeds: [SymbolId; LONG_SET_MAX] =
        core::array::from_fn(|i| make_symbol_id(VenueId::Binance, 300 + i as u32));
    let stranger = make_symbol_id(VenueId::Binance, 999);
    let series: [LongSeries<'_>; LONG_SET_MAX] =
        core::array::from_fn(|i| LongSeries { name: NAMES[i], feed: feeds[i] });
    let mut set = Box::new(LongVolSet::new());
    set.configure(&series, WallAnchor::new(0, 1_767_225_600_000_000_000), 0)
        .expect("gate 82 configure");
    let mut s: u64 = 20_260_926;
    let mut px = [79_000_000_000i64; LONG_SET_MAX];
    let mut run = |set: &mut LongVolSet, from_day: u64, to_day: u64, acc: &mut i64| {
        let mut m = from_day * 1440;
        while m < to_day * 1440 {
            let t0 = m * MIN_NS;
            let mut p = 0u64;
            while p < 13 {
                set.on_timer(t0 + p * SEC_NS);
                p += 1;
            }
            let mut i = 0usize;
            while i < LONG_SET_MAX {
                let v = long_vol_px(&mut s, &mut px[i], m / 1440 + i as u64);
                set.on_tick(&quote(t0 + 20 * SEC_NS, feeds[i], v, 0));
                set.on_tick(&quote(t0 + 21 * SEC_NS, feeds[i], 1, TICK_FLAG_STALE));
                set.on_tick(&quote(t0 + 22 * SEC_NS, feeds[i], 0, 0));
                i += 1;
            }
            set.on_tick(&quote(t0 + 23 * SEC_NS, stranger, 5, 0));
            if m % 60 == 0 {
                let mut k = 0usize;
                while k < set.len() {
                    let e = set.engine(k).expect("gate 82 engine");
                    let mut d = 1u64;
                    while d <= LONG_TAU_DAYS_MAX as u64 {
                        let t = d * DAY_NS;
                        *acc = acc.wrapping_add(e.sigma_ann_1e9(t, LongForecast::Raw).unwrap_or(0));
                        *acc = acc.wrapping_add(e.sigma_ann_1e9(t, LongForecast::Fit).unwrap_or(0));
                        *acc = acc.wrapping_add(e.qlike_counters(t).fit_mean_1e9);
                        d += 1;
                    }
                    let (prof, n) = set.profile(k).expect("gate 82 profile");
                    *acc = acc.wrapping_add(prof[0]).wrapping_add(n[6] as i64);
                    k += 1;
                }
                *acc = acc.wrapping_add(set.counters().day_closes as i64);
            }
            m += 1;
        }
    };
    let mut acc: i64 = 0;
    run(&mut set, 0, 35, &mut acc);
    assert!(set.engine(0).expect("gate 82").is_warm(), "the gate must measure WARM engines");
    let before = set.counters();

    let g = AllocGuard::new();
    run(&mut set, 35, 37, &mut acc);
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    let after = set.counters();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(after.day_closes - before.day_closes, 2 * LONG_SET_MAX as u64, "two staggered boundaries");
    assert_eq!(after.held - before.held, 2 * (LONG_SET_MAX as u64 - 1), "the stagger held");
    assert_eq!(allocs, 0, "long-tenor set allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "long-tenor set bytes should be zero: saw {bytes}");

    // ---- H3.7: the second half, through the `StrategySet` ----
    struct NowCtx(u64);
    impl Ctx for NowCtx {
        fn submit(&mut self, _o: Order) -> Result<(), SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.0
        }
    }
    const WALL0_MS: u64 = 1_767_225_600_000;
    let mut ctx = NowCtx(0);
    let mut sset = Box::new(StrategySet::new(BIT_AI_EXEC));
    sset.on_start(&mut ctx).expect("gate 82 on_start");
    sset.har_mut()
        .configure(&series, WallAnchor::new(0, WALL0_MS * 1_000_000), 0)
        .expect("gate 82 set configure");
    let mut tx = Vec::with_capacity(LONG_SET_MAX);
    let mut rx = Vec::with_capacity(LONG_SET_MAX);
    let mut k = 0usize;
    while k < LONG_SET_MAX {
        let (t, r) = core_ring::Mailbox::new(Box::new(LongStateSnap::new())).split();
        tx.push(t);
        rx.push(r);
        k += 1;
    }
    sset.install_har_outbox(tx);
    let mut rows = [HarSeriesView::default(); HAR_VIEW_SERIES];
    let (mut s2, mut px2) = (20_260_927u64, [79_000_000_000i64; LONG_SET_MAX]);
    let mut run_set = |sset: &mut StrategySet,
                       rx: &mut [core_ring::MailboxRx<LongStateSnap>],
                       from_day: u64,
                       to_day: u64,
                       acc: &mut i64,
                       handed: &mut u64| {
        let mut m = from_day * 1440;
        while m < to_day * 1440 {
            let t0 = m * MIN_NS;
            let mut p = 0u64;
            while p < 13 {
                sset.on_timer(t0 + p * SEC_NS, &mut ctx);
                p += 1;
            }
            let mut i = 0usize;
            while i < LONG_SET_MAX {
                let v = long_vol_px(&mut s2, &mut px2[i], m / 1440 + i as u64);
                sset.on_tick(&quote(t0 + 20 * SEC_NS, feeds[i], v, 0), &mut ctx);
                sset.on_tick(&quote(t0 + 21 * SEC_NS, feeds[i], 1, TICK_FLAG_STALE), &mut ctx);
                sset.on_tick(&quote(t0 + 22 * SEC_NS, feeds[i], 0, 0), &mut ctx);
                i += 1;
            }
            sset.on_tick(&quote(t0 + 23 * SEC_NS, stranger, 5, 0), &mut ctx);
            // The writer's side: take every handed state, read it, free it.
            let mut j = 0usize;
            while j < rx.len() {
                if let Some(snap) = rx[j].try_take() {
                    *acc = acc.wrapping_add(snap.epoch as i64);
                    *acc = acc.wrapping_add(snap.engine.last_min_ts_ms() as i64);
                    *handed += 1;
                }
                j += 1;
            }
            if m % 60 == 0 {
                let n = StrategyCounters::har_series_view(&*sset, &mut rows);
                let c = StrategyCounters::har_counters(&*sset);
                let g = strategy_core::har_gauges(&rows, n, &c, WALL0_MS + m * 60_000);
                *acc = acc
                    .wrapping_add(g.warm)
                    .wrapping_add(g.day_age_max_s)
                    .wrapping_add(rows[LONG_SET_MAX - 1].fit_1e6[0] as i64);
            }
            m += 1;
        }
    };
    let mut handed = 0u64;
    run_set(&mut sset, &mut rx, 0, 35, &mut acc, &mut handed);
    assert!(sset.har().engine(0).expect("gate 82 set").is_warm(), "WARM engines, again");
    assert_eq!(handed, 34 * LONG_SET_MAX as u64, "every close of the warm-up handed");
    let before = sset.har().counters();

    let g = AllocGuard::new();
    let mut handed = 0u64;
    run_set(&mut sset, &mut rx, 35, 37, &mut acc, &mut handed);
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    let after = sset.har().counters();
    assert_eq!(after.day_closes - before.day_closes, 2 * LONG_SET_MAX as u64, "two staggered boundaries");
    assert_eq!(handed, 2 * LONG_SET_MAX as u64, "every close handed through its mailbox");
    let n = StrategyCounters::har_series_view(&*sset, &mut rows);
    let gauges = strategy_core::har_gauges(&rows, n, &StrategyCounters::har_counters(&*sset), WALL0_MS);
    assert_eq!((gauges.configured, gauges.warm), (LONG_SET_MAX as i64, LONG_SET_MAX as i64));
    assert_eq!(allocs, 0, "the set's HAR path allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "the set's HAR path bytes should be zero: saw {bytes}");
}

/// **HC8 gate 83 — a Hypercall signature is 0 B/op.** The exec arm's
/// per-order work: each struct hash over the request body's OWN spans
/// (D7 — the view borrows the rendered body), the digest under the
/// boot-cached domain separator, and the secp256k1 signature — for the
/// live `PlaceOrder`, a `ReplaceOrder`, a `CancelOrderByClientId` and an
/// `AcceptRFQQuote` (a sign-extended `int256`). The body, the key and the
/// separator are boot; the first signature builds the process-wide
/// secp256k1 context outside the guard, as the arm does at boot.
#[test]
fn hypercall_sign_with_key_is_zero_alloc() {
    use signer_eip712::hypercall as hc;
    // The value of `"key":"<value>"` in `body` (boot-time lookup).
    fn span<'a>(body: &'a [u8], key: &[u8]) -> &'a [u8] {
        let mut i = 0usize;
        while i + key.len() + 4 <= body.len() {
            if body[i] == b'"'
                && &body[i + 1..i + 1 + key.len()] == key
                && &body[i + 1 + key.len()..i + 4 + key.len()] == b"\":\""
            {
                let from = i + 4 + key.len();
                let mut to = from;
                while body[to] != b'"' {
                    to += 1;
                }
                return &body[from..to];
            }
            i += 1;
        }
        panic!("gate 83: no {key:?} in the body");
    }
    let sk = signer_eip712::parse_secret_key(&[0x42; 32]).expect("gate 83 key");
    let ds = hc::hc_domain_separator(hc::HC_CHAIN_ID_MAINNET);
    let wallet = [0x5a; 20];
    let body: Vec<u8> = br#"{"symbol":"BOT-20260925-2.5-C","side":"Buy","size":"12.345678","price":"0.0005","tif":"ioc","route":"best_execution","client_id":"0x4843000300000000000000000000002a","order_id":"123456789"}"#.to_vec();
    let (symbol, side, size, price) =
        (span(&body, b"symbol"), span(&body, b"side"), span(&body, b"size"), span(&body, b"price"));
    let (tif, route, cloid, order_id) =
        (span(&body, b"tif"), span(&body, b"route"), span(&body, b"client_id"), span(&body, b"order_id"));
    let (rfq, quote) = ([0x11u8; 32], [0x22u8; 32]);
    let warm = hc::revoke_all_agents_struct_hash(0);
    hc::sign_hc_with_key(&sk, &ds, &warm).expect("gate 83 warm-up");

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut n = 0u64;
    while n < 1_000 {
        let place = hc::HcPlaceView {
            wallet: &wallet,
            symbol,
            side,
            size,
            price,
            tif,
            route,
            client_id: cloid,
            nonce: 1_790_400_000_000_000 + n,
        };
        let s1 = hc::sign_place_order_with_key(&sk, &ds, &place).expect("gate 83 place");
        let replace = hc::HcReplaceView {
            wallet: &wallet,
            order_id,
            symbol,
            side,
            size,
            price,
            tif,
            client_id: cloid,
            nonce: n,
        };
        let s2 = hc::sign_hc_with_key(&sk, &ds, &hc::replace_order_struct_hash(&replace))
            .expect("gate 83 replace");
        let s3 = hc::sign_hc_with_key(&sk, &ds, &hc::cancel_order_by_client_id_struct_hash(&wallet, cloid, n))
            .expect("gate 83 cancel");
        let s4 = hc::sign_hc_with_key(
            &sk,
            &ds,
            &hc::accept_rfq_quote_struct_hash(&rfq, &quote, -(n as i128) - 1, &wallet, n),
        )
        .expect("gate 83 rfq");
        acc = acc
            .wrapping_add(u64::from(s1[0]))
            .wrapping_add(u64::from(s2[1]))
            .wrapping_add(u64::from(s3[2]))
            .wrapping_add(u64::from(s4[64]));
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(allocs, 0, "Hypercall signing allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "Hypercall signing bytes should be zero: saw {bytes}");
}

/// **HC9 gate 84 — the Hypercall arm's per-order work is 0 B/op.** What
/// one place, one replace and one cancel cost on the engine thread
/// before and after the round trip: each body rendered IN PLACE into
/// the (boot-allocated) body window and signed over its own spans (D7),
/// the fail-closed answer scans, the rate governor, and the private
/// socket's reader over a `Fill` and an `OrderUpdate` with the fill-id
/// dedupe — plus one reconcile page of each kind. The TLS round trip
/// itself is gate 77b's (`HttpsReq`, exactly rustls' 2 per request).
#[test]
fn hypercall_arm_render_sign_and_scan_is_zero_alloc() {
    use exec_hypercall::{budget, cloid, events, recon, render, response};
    use signer_eip712::hypercall as hc;
    let sk = signer_eip712::parse_secret_key(&[0x42; 32]).expect("gate 84 key");
    let ds = hc::hc_domain_separator(hc::HC_CHAIN_ID_MAINNET);
    let wallet = [0x5a; 20];
    let mut body = vec![0u8; 2048];
    let mut gov = budget::HcBudget::new();
    let mut ids = events::FillIds::new();
    const ACK: &[u8] = br#"{"timestamp":1,"info":{"symbol":"BTC-20261002-100000-C","price":"12.5","size":"0.2","side":"Buy","tif":"ioc","is_perp":false,"order_id":42},"status":"PARTIALLY_FILLED","filled_size":"0.1","wallet_address":"0x5a","order_id":42,"reason":null}"#;
    const CXL: &[u8] = br#"{"success":true,"data":{"status":"CANCELED","filled_size":"0.1","order_id":42},"error":null}"#;
    const FILL: &[u8] = br#"{"type":"Fill","order_id":42,"fill_id":7,"symbol":"BTC-20261002-100000-C","side":"buy","price":"12.5","size":"0.1","timestamp":1767225600000,"wallet_address":"0x5a","fee":"0.01","trade_id":9,"is_taker":true}"#;
    const UPD: &[u8] = br#"{"type":"OrderUpdate","order_id":42,"status":"CANCELED","filled_size":"0.1","timestamp":2,"reason":null}"#;
    const ORDERS: &[u8] = br#"{"success":true,"data":[{"order_id":1,"symbol":"BTC-20261002-100000-C","side":"buy","size":"0.2","filled_size":"0","client_id":"48430107000000000000002a00000000","status":"open"}],"pagination":{"limit":100,"offset":0,"count":1}}"#;
    const PORT: &[u8] = br#"{"success":true,"data":{"positions":[{"symbol":"BTC-20261002-100000-C","amount":"-0.1","entry_price":"12.5"}],"available_balance":"4.5"},"error":null}"#;
    // Warm: the process-wide secp256k1 context, as the arm at boot.
    hc::sign_hc_with_key(&sk, &ds, &hc::revoke_all_agents_struct_hash(0)).expect("gate 84 warm-up");

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut n = 0u64;
    while n < 500 {
        let cid = cloid::encode(7, n);
        let p = render::Place {
            wallet: &wallet,
            symbol: b"BTC-20261002-100000-C",
            buy: n % 2 == 0,
            px_1e6: 12_500_000 + n as i64,
            qty_1e6: 200_000,
            tif: render::Tif::Ioc,
            route: render::Route::BestExecution,
            client_id: &cid,
            nonce: 1_790_400_000_000_000 + n,
        };
        let k1 = render::place(&mut body, &p, &sk, &ds).expect("gate 84 place");
        let r = render::Replace {
            wallet: &wallet,
            order_id: 42 + n,
            symbol: b"BTC-20261002-100000-C",
            buy: true,
            px_1e6: 12_000_000,
            qty_1e6: 100_000,
            tif: render::Tif::Gtc,
            client_id: &cid,
            nonce: 2 * n + 1,
        };
        let k2 = render::replace(&mut body, &r, &sk, &ds).expect("gate 84 replace");
        let k3 = render::cancel_cloid(&mut body, &wallet, &cid, 3 * n + 2, &sk, &ds).expect("gate 84 cancel");
        let a = response::scan_place(200, ACK).expect("gate 84 ack");
        let c = response::scan_cancel(200, CXL).expect("gate 84 cancel ack");
        assert!(gov.place(1_000 + n * 2_000).is_ok(), "gate 84 governor place");
        assert!(gov.cancel(1_000 + n * 2_000).is_ok(), "gate 84 governor cancel");
        if let events::Msg::Fill(f) = events::parse(FILL) {
            acc = acc.wrapping_add(u64::from(ids.first_time(f.fill_id + n))).wrapping_add(f.qty_1e6 as u64);
        }
        if let events::Msg::Update(u) = events::parse(UPD) {
            acc = acc.wrapping_add(u.filled_1e6 as u64);
        }
        let mut open = 0u64;
        assert_eq!(recon::scan_orders(200, ORDERS, |o| open += o.size_1e6 as u64), Ok(1));
        assert!(recon::scan_portfolio(200, PORT, |_, amt, _| open = open.wrapping_add(amt as u64)).is_ok());
        acc = acc
            .wrapping_add((k1 + k2 + k3) as u64)
            .wrapping_add(a.order_id + c.order_id)
            .wrapping_add(u64::from(body[k3 - 3]))
            .wrapping_add(open);
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(allocs, 0, "the Hypercall arm's per-order work allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "the Hypercall arm's per-order work bytes should be zero: saw {bytes}");
}

/// **HC11 gate 85 — the slot-7 member and the paper held-quote law are
/// 0 B/op.** Two underlyings, a near-ATM chain, σ̂ pushed, a calendar by
/// mailbox; the member's orders go into a `PaperMatcher` (Hypercall IoCs
/// under the held-quote law, the hedges under the strict-cross IoC law)
/// and its fills and order events come back — 2 400 one-second engine
/// steps: the quote swings rich and cheap (sales, buy-backs, the caps),
/// the hedge follows outside its band, a position injected at boot runs
/// through the final window's TWAP to its median-of-means settlement, the
/// calendar is replaced and the HAR view re-pushed mid-run — and (HC11b)
/// every change to the book is handed to the writer's mailbox, grid and
/// all, and taken back as the writer thread would. Everything is boxed at
/// `configure` and at the outbox's install (the only allocations).
#[test]
fn hcv_member_and_the_held_quote_law_are_zero_alloc() {
    use clob_dispatcher::PaperMatcher;
    use core_types::{make_symbol_id, ChannelEvent, ChannelId, Order, OrderEvent, Side};
    use strategy_core::{Ctx, HarSeriesView, Strategy, SubmitErr, HAR_VIEW_TENORS};
    use strategy_hcv::{HcvEvents, HcvOpt, HcvParams, HcvStrategy, HcvUnd};

    struct PaperCtx<'a> {
        m: &'a mut PaperMatcher,
        now: u64,
    }
    impl Ctx for PaperCtx<'_> {
        fn submit(&mut self, mut order: Order) -> Result<(), SubmitErr> {
            order.strategy_id = 7;
            self.m.submit(&order, self.now);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    const T0_MS: u64 = 1_790_424_000_000;
    const MONO0: u64 = 1_000_000_000_000;
    const DAY: u64 = 86_400_000;
    const STEPS: u64 = 2_400;
    let ns = |ms: u64| MONO0 + ms * 1_000_000;
    let sp = make_symbol_id(VenueId::Hyperliquid, 40);
    let btc = make_symbol_id(VenueId::Hyperliquid, 1);
    let opt = |k: u32| make_symbol_id(VenueId::Hypercall, 513 + k);
    let s_sp: i64 = 6_600_000_000;
    let s_btc: i64 = 110_000_000_000;
    let mut options = Vec::new();
    let mut k = 0u32;
    while k < 6 {
        let strike = s_sp - 100_000_000 + i64::from(k / 2) * 100_000_000;
        options.push(HcvOpt { sym: opt(k), und: 0, call: k % 2 == 0, strike_1e6: strike, exp_ms: T0_MS + 7 * DAY });
        k += 1;
    }
    options.push(HcvOpt { sym: opt(6), und: 1, call: true, strike_1e6: s_btc, exp_ms: T0_MS + 7 * DAY });
    // Injected at boot: expires 15 minutes in — the final window and the
    // settlement run inside the measured loop.
    options.push(HcvOpt { sym: opt(7), und: 0, call: true, strike_1e6: s_sp, exp_ms: T0_MS + 15 * 60_000 });
    let p = HcvParams {
        und: vec![HcvUnd::new(b"SP500", sp, 2), HcvUnd::new(b"BTC", btc, 5)],
        options,
        theta_vol_1e6: 50_000,
        atm_band_bps: 500,
        tenor_min_d: 1,
        tenor_max_d: 40,
        clip_usd_1e6: 5_000_000,
        vega_cap_usd_1e6: 20_000_000,
        premium_cap_usd_1e6: 50_000_000,
        day_loss_usd_1e6: 1_000_000_000,
        tail_loss_usd_1e6: 1_000_000_000,
        opt_size_step_1e6: 1_000,
        hedge_band_1e6: 100_000,
        hedge_min_usd_1e6: 10_000_000,
        hedge_slip_bps: 10,
        quote_stale_ms: 10_000,
        oracle_stale_ms: 10_000,
        unwind_min: 30,
        settle_delay_ms: 60_000,
        settle_order: strategy_hcv::BucketOrder::Sorted,
        event_law: true,
        events_stale_ms: 7_200_000,
        kill: false,
        timer_ms: 1_000,
        anchor: core_time::WallAnchor::new(MONO0, T0_MS * 1_000_000),
    };
    // Boot (allocation allowed).
    let mut s = HcvStrategy::new();
    s.configure(&p).expect("gate 85 configure");
    let mut rows = [HarSeriesView::default(); 2];
    rows[0].name[..5].copy_from_slice(b"SP500");
    rows[0].name_len = 5;
    rows[1].name[..3].copy_from_slice(b"BTC");
    rows[1].name_len = 3;
    rows[0].warm = 1;
    rows[1].warm = 1;
    rows[0].raw_1e6 = [150_000; HAR_VIEW_TENORS];
    rows[1].raw_1e6 = [450_000; HAR_VIEW_TENORS];
    s.set_har_view(&rows);
    let (mut tx, rx) = core_ring::Mailbox::new(Box::new(HcvEvents::new())).split();
    s.install_events(rx);
    let (book_tx, mut book_rx) = core_ring::Mailbox::new(strategy_hcv::HcvStateSnap::new_boxed()).split();
    s.install_state_outbox(book_tx);
    let mut books = 0u64;
    let calendar = |gen_ms: u64| {
        let mut c = HcvEvents::new();
        c.generated_ms = gen_ms;
        c.from_ms = gen_ms - 30 * DAY;
        c.until_ms = gen_ms + 60 * DAY;
        c.push(gen_ms + 20 * DAY, 0b01);
        c
    };
    {
        let mut f = tx.try_fill().expect("gate 85 mailbox");
        *f = calendar(T0_MS - 60_000);
        f.commit();
    }
    let mut m = Box::new(PaperMatcher::new());
    {
        let mut c = PaperCtx { m: &mut m, now: ns(0) };
        s.on_fill(&core_types::Fill::new(ns(0), opt(7), Side::Bid, Price::from_raw(30_000_000), Qty::from_raw(1_000_000), 999), &mut c);
    }
    // Premiums at ~22 % (rich) and ~9 % (cheap) of a 7-day ATM SP500
    // option; the BTC call at ~70 % / ~30 %.
    let px_sp = |rich: bool| if rich { (80_000_000i64, 84_000_000i64) } else { (30_000_000i64, 33_000_000i64) };
    let px_btc = |rich: bool| if rich { (4_250_000_000i64, 4_400_000_000i64) } else { (1_700_000_000i64, 1_850_000_000i64) };

    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < STEPS {
        let now = ns(i * 1_000);
        let rich = (i / 40) % 2 == 0;
        let drift = ((i * 7_919) % 2_001) as i64 * 100_000 - 100_000_000;
        let (spx, bpx) = (s_sp + drift, s_btc + drift * 16);
        let hl_sp = Tick::new_stamped(now, VenueId::Hyperliquid, sp, 0, Price::from_raw(spx - 500_000), Qty::from_raw(100_000_000), Price::from_raw(spx + 500_000), Qty::from_raw(100_000_000), 0, 0);
        let hl_btc = Tick::new_stamped(now, VenueId::Hyperliquid, btc, 0, Price::from_raw(bpx - 1_000_000), Qty::from_raw(10_000_000), Price::from_raw(bpx + 1_000_000), Qty::from_raw(10_000_000), 0, 0);
        m.observe_tick(&hl_sp, now);
        m.observe_tick(&hl_btc, now);
        {
            let mut c = PaperCtx { m: &mut m, now };
            s.on_venue_event(&ChannelEvent::new(now, VenueId::Hyperliquid, ChannelId::Mark, sp, 0, 0, spx, spx), &mut c);
            s.on_venue_event(&ChannelEvent::new(now, VenueId::Hyperliquid, ChannelId::Mark, btc, 0, 0, bpx, bpx), &mut c);
            s.on_tick(&hl_sp, &mut c);
            s.on_tick(&hl_btc, &mut c);
        }
        let mut k = 0u32;
        while k < 7 {
            let (b, a) = if k == 6 { px_btc(rich) } else { px_sp(rich) };
            let q = Tick::new_stamped(now, VenueId::Hypercall, opt(k), 0, Price::from_raw(b), Qty::from_raw(1_000_000), Price::from_raw(a), Qty::from_raw(1_000_000), 0, 0);
            m.observe_tick(&q, now);
            let mut c = PaperCtx { m: &mut m, now };
            s.on_tick(&q, &mut c);
            k += 1;
        }
        {
            let mut c = PaperCtx { m: &mut m, now };
            s.on_timer(now, &mut c);
        }
        while let Some(f) = m.try_next_fill() {
            let mut c = PaperCtx { m: &mut m, now };
            s.on_fill(&f, &mut c);
        }
        let mut ev = OrderEvent::ZERO;
        while m.try_next_order_event(&mut ev) {
            let mut c = PaperCtx { m: &mut m, now };
            s.on_order_event(&ev, &mut c);
        }
        // The writer's end: take the handed book, hand the slot back.
        if let Some(b) = book_rx.try_take() {
            books += u64::from(b.epoch != 0 && b.n_pos as usize <= strategy_hcv::state::HCV_SNAP_POSITIONS);
        }
        if i % 600 == 300 {
            if let Some(mut f) = tx.try_fill() {
                *f = calendar(T0_MS + i * 1_000);
                f.commit();
            }
            s.set_har_view(&rows);
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let k = s.counters();
    assert!(k.sells > 0 && k.buys > 0, "both sides traded: {k:?}");
    assert!(k.option_fills > 0 && k.hedges > 0 && k.hedge_fills > 0, "fills and hedges: {k:?}");
    assert!(k.unwind_slices > 0 && k.settlements >= 1, "the final window and the settlement ran: {k:?}");
    assert!(k.calendars >= 3 && k.skip_caps > 0, "calendars replaced, caps met: {k:?}");
    assert!(books > 30, "the book was handed at its fills, bookings and window minutes: {books}");
    assert_eq!(allocs, 0, "slot 7 over the held-quote law allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0);
}

/// **BX4 gate 86 — the Ed25519 sign + base64 render (`signer-ed25519`).**
///
/// Binance's WS API logon and every REST order request (options,
/// Portfolio Margin, Binance Stocks) carry one Ed25519 signature, written
/// as base64 (WS API) or percent-encoded base64 (REST) straight into the
/// request's wire buffer. `ring` signs into its own return value on the
/// stack; the render writes into the caller's buffer. The whole path must
/// be 0 B/op. Boot is pinned too, the way gate 72 pins `HttpsPost`: a
/// signer costs exactly one allocation (its page), and the self-test one
/// per known answer — so a `ring` bump that starts allocating in
/// `from_seed_unchecked`, or a keypair moved into a `Box`, fails here.
///
/// PMJ J2: the raw signature into a Solana transaction's slot
/// (`sign_raw`) and its verification (`verify`) ride the same loop, and
/// the self-test's raw and verify checks the same boot count.
#[test]
fn ed25519_sign_and_render_are_zero_alloc() {
    use signer_ed25519::{Ed25519Signer, RFC8032, SIG_B64_LEN, SIG_B64_PCT_MAX, SIG_LEN};
    let g = AllocGuard::new();
    let signer = Ed25519Signer::from_seed(&[0x42; 32]).expect("gate 86 key");
    let (boot_allocs, boot_bytes, _) = g.delta();
    assert_eq!(boot_allocs, 1, "from_seed: exactly one page ({boot_bytes} B)");
    let g = AllocGuard::new();
    signer_ed25519::self_test().expect("gate 86 self-test");
    let (st_allocs, _, _) = g.delta();
    assert_eq!(st_allocs as usize, RFC8032.len(), "self_test: one page per known answer");
    // A REST order query's worth of payload.
    let mut payload = [0x61u8; 200];
    let mut b64 = [0u8; SIG_B64_LEN];
    let mut pct = [0u8; SIG_B64_PCT_MAX];
    let mut raw = [0u8; SIG_LEN];
    let mut public = [0u8; 32];
    public.copy_from_slice(signer.public_key());
    signer.sign_b64(&payload, &mut b64); // warm-up
    signer.sign_raw(&payload, &mut raw);
    assert!(signer_ed25519::verify(&public, &payload, &raw), "gate 86: warm-up verifies");

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut verified = 0u64;
    let mut n = 0u64;
    while n < 2_000 {
        payload[..8].copy_from_slice(&n.to_le_bytes());
        acc = acc.wrapping_add(signer.sign_b64(&payload, &mut b64) as u64);
        acc = acc.wrapping_add(signer.sign_b64_pct(&payload, &mut pct) as u64);
        acc = acc.wrapping_add(u64::from(b64[0]) + u64::from(pct[0]));
        signer.sign_raw(&payload, &mut raw);
        verified += u64::from(signer_ed25519::verify(&public, &payload, &raw));
        n += 1;
    }
    std::hint::black_box(acc);
    assert_eq!(verified, 2_000, "gate 86: every raw signature verifies");

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(allocs, 0, "signer-ed25519 sign/render allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "signer-ed25519 hot bytes should be zero: saw {bytes}");
}

/// **BX5 gate 87 — the exec gateway's transports, their socket-free
/// half: request staging, the answer judge, the WebSocket framer and the
/// request ids.**
///
/// An exec gateway (BX6) stages every REST request in place
/// (`ReqWire::stage`: body form — the length digits, the head moved on a
/// digit-count change; query form — the header tail after the query),
/// judges each answer where it was read (`parse_answer`, a chunked body
/// decoded in place; since XMM HD2 interim `1xx` heads skipped, a `204`
/// ended at its head and a `close` on any head retiring), and runs its
/// WebSocket API sessions through
/// `WsFramer` (data frames handed out in place, pings echoed into the
/// send window, requests masked straight into it), its answers matched
/// to requests by the kit's in-flight table (`exec_gateway::Pending`, its
/// skip-busy `claim_next` — BX7; BX5 had `core_net::ReqIds`). All of it
/// must be 0 B/op; the windows and tables are allocated at boot, outside
/// the guard. The socket half is gate 87b.
#[test]
fn bx5_staging_answer_judge_ws_framing_and_ids_are_zero_alloc() {
    use core_net::https_conn::parse_answer;
    use core_net::{Method, Params, ReqSpec, ReqWire, WsFramer, WsNext};

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum Kind {
        Place = 0,
        Free = 255,
    }

    const H: &[(&str, &str)] = &[("X-MBX-APIKEY", "gate87key")];
    let specs = [
        ReqSpec {
            method: Method::Post,
            path: "/eapi/v1/order",
            params: Params::Body("application/x-www-form-urlencoded"),
            headers: H,
        },
        ReqSpec {
            method: Method::Get,
            path: "/eapi/v1/openOrders",
            params: Params::Query,
            headers: H,
        },
    ];
    let mut wire = ReqWire::new("eapi.binance.com", &specs, 2048).expect("gate 87 wire");
    let mut framer = WsFramer::new(64 * 1024, 64 * 1024, 7);
    let mut ids: exec_gateway::Pending<Kind, 64> = exec_gateway::Pending::new(Kind::Free);
    let resp: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 28\r\n\r\n{\"orderId\":1,\"status\":\"NEW\"}";
    let chunked: &[u8] =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n12}\r\n0\r\n\r\n";
    let interim: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\n\
        Connection: close\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n";
    // What a WS API socket delivers per request: the answer, a server
    // ping, a pong, and a user-data event (server frames are unmasked).
    let mut inbound = [0u8; 256];
    let mut len = 0usize;
    let parts: [(u8, &[u8]); 4] = [
        (0x81, br#"{"id":"1","status":200,"result":{"orderId":1}}"#),
        (0x89, b"ping"),
        (0x8A, b"pong"),
        (0x81, br#"{"subscriptionId":0,"event":{"e":"executionReport"}}"#),
    ];
    let mut k = 0;
    while k < parts.len() {
        inbound[len] = parts[k].0;
        inbound[len + 1] = parts[k].1.len() as u8;
        inbound[len + 2..len + 2 + parts[k].1.len()].copy_from_slice(parts[k].1);
        len += 2 + parts[k].1.len();
        k += 1;
    }
    let inbound = &inbound[..len];
    let mut rbuf = [0u8; 512];
    // Held unanswered through the whole run: every 64th id lands on its
    // slot and is skipped — the busy path.
    let hold = {
        let (id, k) = ids.claim_next(u64::MAX).expect("gate 87 hold");
        *k = Kind::Place;
        id
    };

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    let mut n = 0usize;
    while n < 2_000 {
        // REST: bodies of 1–4 digits (the head moves), queries of any
        // length (the tail follows), each staged in place.
        let blen = [9usize, 99, 180, 1500][n % 4];
        wire.window_mut(0)[..8].copy_from_slice(&(n as u64).to_le_bytes());
        let s = wire.stage(0, blen).expect("gate 87 body");
        acc = acc.wrapping_add(wire.bytes(s).len() as u64);
        let s = wire.stage(1, n % 64).expect("gate 87 query");
        acc = acc.wrapping_add(wire.bytes(s).len() as u64);
        // The answers, judged where they lie.
        rbuf[..resp.len()].copy_from_slice(resp);
        let a = parse_answer(&mut rbuf[..resp.len()], false)
            .expect("gate 87 answer")
            .expect("whole");
        acc = acc.wrapping_add(u64::from(a.status) + (a.body.end - a.body.start) as u64);
        rbuf[..chunked.len()].copy_from_slice(chunked);
        let a = parse_answer(&mut rbuf[..chunked.len()], false)
            .expect("gate 87 chunked")
            .expect("whole");
        acc = acc.wrapping_add((a.body.end - a.body.start) as u64);
        rbuf[..interim.len()].copy_from_slice(interim);
        let a = parse_answer(&mut rbuf[..interim.len()], false)
            .expect("gate 87 interim")
            .expect("whole");
        acc = acc.wrapping_add(u64::from(a.status) + u64::from(a.retire));
        // …and the refusals: a short answer waits, a short answer after a
        // close is a lost connection.
        rbuf[..resp.len()].copy_from_slice(resp);
        assert_eq!(parse_answer(&mut rbuf[..resp.len() - 3], false), Ok(None));
        assert_eq!(
            parse_answer(&mut rbuf[..resp.len() - 3], true),
            Err(core_net::PostErrKind::Disconnected)
        );
        // WS API: a request out, its answer and the traffic around it in.
        let id = {
            let (id, k) = ids.claim_next(n as u64).expect("gate 87 id");
            *k = Kind::Place;
            id
        };
        let lost = ids.claim_next(n as u64).expect("gate 87 id").0;
        framer
            .queue_text(&[b"{\"id\":\"1\",\"method\":\"order.place\",\"params\":{", b"}}"])
            .expect("gate 87 queue");
        let free = framer.rx_free_mut();
        free[..inbound.len()].copy_from_slice(inbound);
        framer.rx_advance(inbound.len());
        loop {
            match framer.next_frame() {
                WsNext::Text(s) | WsNext::Binary(s) => {
                    acc = acc.wrapping_add(framer.payload(s).len() as u64);
                }
                WsNext::Idle => break,
                WsNext::Failed(e) => panic!("gate 87 framer: {e}"),
            }
        }
        // The answer finds its record in place; the unanswered one is
        // retired at its deadline (the gateway's timer queue).
        acc = acc.wrapping_add(u64::from(ids.get(id) == Some(&Kind::Place)) + ids.sent_ns(id).unwrap_or(0));
        assert!(ids.release(id));
        assert!(ids.release(lost), "gate 87 expiry");
        let out = framer.tx_pending().len();
        acc = acc.wrapping_add(out as u64);
        framer.tx_consume(out);
        n += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(ids.get(hold), Some(&Kind::Place));
    assert!(ids.release(hold));
    assert_eq!(ids.live(), 0, "every request was answered or expired");
    assert_eq!(allocs, 0, "BX5 staging/judge/framing/ids allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "BX5 hot bytes should be zero: saw {bytes}");
}

/// One gate 87b request on the owner's poll: start, then events and
/// ticks until the answer is whole.
fn gate87_cycle(
    conn: &mut core_net::HttpsConn,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    epoch: std::time::Instant,
    n: usize,
) -> u16 {
    use core_net::Progress;
    let now = epoch.elapsed().as_nanos() as u64;
    conn.start(0, n, poll.registry(), now).expect("gate 87 start");
    // The deadline check on a request in flight, every cycle.
    assert_eq!(conn.on_tick(now), Progress::Waiting);
    loop {
        poll.poll(events, Some(std::time::Duration::from_millis(5)))
            .expect("gate 87 poll");
        for ev in events.iter() {
            match conn.on_event(ev, poll.registry()) {
                Progress::Waiting => {}
                Progress::Done { status, .. } => return status,
                Progress::Failed(e) => panic!("gate 87: {e}"),
            }
        }
        match conn.on_tick(epoch.elapsed().as_nanos() as u64) {
            Progress::Waiting => {}
            Progress::Done { status, .. } => return status,
            Progress::Failed(e) => panic!("gate 87: {e}"),
        }
    }
}

/// **BX5 gate 87b — the non-blocking HTTPS cycle, as allocations per
/// request.**
///
/// `HttpsConn` is `HttpsPost`'s protocol split for an owner's poll (the
/// exec gateway's): `start` stages and writes, `on_event` reads and
/// judges, `on_tick` keeps the deadline. Same law as gate 72: rustls'
/// buffered API allocates one record buffer per sealed write and one per
/// decrypted record, so a request costs exactly 2 allocations, and
/// anything above that is ours. Served by gate 72's child-process node
/// (the counting allocator is process-global).
///
/// "Exactly 2" is THIS fixture's number: one request ≤ 16 KiB (one
/// record out), one answer in one record read whole (one record in).
/// Across a WAN, with no regression of ours, rustls adds one allocation
/// per further inbound record, a grow-and-shrink of its deframer buffer
/// whenever a record is split across reads, and three on a burst of
/// more than 16 KiB of plaintext. BX6's budget is not 2 per request.
#[test]
fn https_conn_nonblocking_cycle_allocates_only_rustls_record_buffers() {
    use core_net::{ConnCfg, HttpsConn, Method, Params, ReqSpec};
    const REQS: u64 = 500;
    const RUSTLS_ALLOCS_PER_REQ: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate87-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 87 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 87 exe"))
        .args([
            "gate72_node_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE72_NODE_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 87 child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 87 port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 87: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 87 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 87 anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let spec = [ReqSpec {
        method: Method::Post,
        path: "/evm",
        params: Params::Body("application/json"),
        headers: &[],
    }];
    let c = ConnCfg {
        win_cap: 8192,
        resp_cap: 16 * 1024,
        req_timeout_ns: 5_000_000_000,
    };
    let mut conn =
        HttpsConn::new("localhost", port, cfg, &spec, c, mio::Token(5)).expect("gate 87 conn");
    let mut poll = mio::Poll::new().expect("gate 87 poll");
    let mut events = mio::Events::with_capacity(8);
    let epoch = std::time::Instant::now();
    let n = exec_hyperevm::rpc::write_chain_id(conn.window_mut(0), 1).expect("gate 87 body");
    let mut i = 0;
    while i < 50 {
        // The handshake, the session tickets and rustls' queues growing
        // to their working size: the cold part.
        assert_eq!(gate87_cycle(&mut conn, &mut poll, &mut events, epoch, n), 200);
        i += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut k = 0u64;
    while k < REQS {
        let status = gate87_cycle(&mut conn, &mut poll, &mut events, epoch, n);
        acc = acc.wrapping_add(u64::from(status) + conn.resp().len() as u64);
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(conn.is_connected(), "one keep-alive connection throughout");
    assert_eq!(conn.dials(), 1, "no redial inside the measurement");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_REQ * REQS,
        "HttpsConn cycle: {allocs} allocations ({bytes} B) over {REQS} requests — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_REQ}/request (one sealed record \
         out, one decrypted record in); anything above is ours"
    );
}

/// A server frame (unmasked), FIN set.
fn gate87_server_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0x80 | op];
    if payload.len() < 126 {
        v.push(payload.len() as u8);
    } else {
        v.push(126);
        v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

/// Gate 87c's server half: runs ONLY in the child process gate 87c
/// spawns (`#[ignore]`d; a no-op without `GATE87_WS_DIR`). A rustls
/// WebSocket node: it upgrades one client, then answers every client
/// text frame with ONE server frame in ONE write — every 8th answer
/// carrying a ping in the same write — until the parent kills it.
#[test]
#[ignore = "gate 87c's child process; never run on its own"]
fn gate87_ws_node_helper() {
    use std::io::{Read, Write};
    let Some(dir) = std::env::var_os("GATE87_WS_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let c = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    let cert = c.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key");
    let cfg = std::sync::Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("gate 87c server cfg"),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("gate 87c bind");
    let port = listener.local_addr().expect("gate 87c addr").port();
    std::fs::write(dir.join("cert.der"), cert.as_ref()).expect("gate 87c cert");
    // The port last: its presence says the rest is written.
    std::fs::write(dir.join("port.tmp"), port.to_string()).expect("gate 87c port");
    std::fs::rename(dir.join("port.tmp"), dir.join("port")).expect("gate 87c port");
    let (sock, _) = listener.accept().expect("gate 87c accept");
    sock.set_nodelay(true).ok();
    let mut tls = rustls::StreamOwned::new(
        rustls::ServerConnection::new(cfg).expect("gate 87c conn"),
        sock,
    );
    let mut buf: Vec<u8> = Vec::new();
    let mut b = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = tls.read(&mut b).expect("gate 87c upgrade");
        assert!(n > 0, "gate 87c: no upgrade");
        buf.extend_from_slice(&b[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    buf.drain(..head_end);
    let key: [u8; 24] = head
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .expect("gate 87c key")
        .as_bytes()
        .try_into()
        .expect("gate 87c key length");
    let accept = core_net::expected_accept(&key);
    let ok = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        String::from_utf8_lossy(&accept)
    );
    tls.write_all(ok.as_bytes()).expect("gate 87c 101");
    tls.flush().expect("gate 87c flush");
    let mut answered = 0u64;
    loop {
        // One client frame: FIN, masked, 7- or 16-bit length.
        let (op, payload) = loop {
            if buf.len() >= 2 {
                let (len, at) = match buf[1] & 0x7F {
                    126 if buf.len() >= 4 => (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4),
                    126 => (usize::MAX, 0),
                    n => (usize::from(n), 2),
                };
                if len != usize::MAX && buf.len() >= at + 4 + len {
                    let mask = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
                    let p: Vec<u8> = (0..len).map(|i| buf[at + 4 + i] ^ mask[i & 3]).collect();
                    let op = buf[0] & 0x0F;
                    buf.drain(..at + 4 + len);
                    break (op, p);
                }
            }
            match tls.read(&mut b) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&b[..n]),
            }
        };
        if op != 0x1 {
            continue; // the pongs ride in with the next request
        }
        answered += 1;
        let mut out = gate87_server_frame(0x1, &payload);
        if answered % 8 == 0 {
            out.extend(gate87_server_frame(0x9, b"k"));
        }
        if tls.write_all(&out).is_err() || tls.flush().is_err() {
            return;
        }
    }
}

/// One gate 87c round on the owner's poll: queue a request, flush once
/// (with any pong the last round queued), turn the loop until its echo
/// is drained. Returns the echo's length.
fn gate87c_round(
    conn: &mut core_net::WsConn,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    epoch: std::time::Instant,
    req: &[&[u8]],
) -> usize {
    use core_net::{WsNext, WsProgress};
    conn.queue_text(req).expect("gate 87c queue");
    conn.flush(poll.registry()).expect("gate 87c flush");
    loop {
        poll.poll(events, Some(std::time::Duration::from_millis(5)))
            .expect("gate 87c poll");
        let now = epoch.elapsed().as_nanos() as u64;
        for ev in events.iter() {
            if let WsProgress::Failed(e) = conn.on_event(ev, poll.registry(), now) {
                panic!("gate 87c: {e}");
            }
        }
        let mut got = 0usize;
        loop {
            match conn.next_frame() {
                WsNext::Text(s) | WsNext::Binary(s) => got += conn.payload(s).len(),
                WsNext::Idle => break,
                WsNext::Failed(e) => panic!("gate 87c: {e}"),
            }
        }
        if let WsProgress::Failed(e) = conn.on_tick(now) {
            panic!("gate 87c: {e}");
        }
        if got > 0 {
            return got;
        }
    }
}

/// **BX5 gate 87c — the WebSocket session's socket half, as allocations
/// per round.**
///
/// `WsConn` is what BX6's WS API session sends `session.logon` and
/// `order.place` through. A round is: `queue_text` (masked into the send
/// window), `flush` (the whole window in ONE write, with the pong the
/// last round's ping queued), `on_event` (pump, read to `WouldBlock`),
/// `next_frame` (the echo handed out in place; every 8th round a ping
/// answered into the send window), `on_tick` (the idle law). rustls'
/// buffered API allocates one record buffer per sealed write and one per
/// decrypted record, and the node answers each round in ONE write, so a
/// round costs exactly 2 — anything above is ours. The node runs in a
/// CHILD process ([`gate87_ws_node_helper`]): the counting allocator is
/// process-global. Gate 87b's caveat holds: 2 is this fixture's number.
#[test]
fn ws_conn_session_round_allocates_only_rustls_record_buffers() {
    use core_net::{WsCfg, WsConn};
    const ROUNDS: u64 = 400;
    const RUSTLS_ALLOCS_PER_ROUND: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate87c-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 87c dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 87c exe"))
        .args([
            "gate87_ws_node_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE87_WS_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 87c child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 87c port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 87c: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 87c cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 87c anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let c = WsCfg {
        rx_cap: 64 * 1024,
        tx_cap: 64 * 1024,
        establish_ns: 10_000_000_000,
        idle_ns: 60_000_000_000,
    };
    let mut conn = WsConn::new("localhost", port, "/ws-api/v3", cfg, c, mio::Token(6), 0x74C)
        .expect("gate 87c conn");
    let mut poll = mio::Poll::new().expect("gate 87c poll");
    let mut events = mio::Events::with_capacity(8);
    let epoch = std::time::Instant::now();
    conn.connect(poll.registry(), 0).expect("gate 87c connect");
    while !conn.is_open() {
        poll.poll(&mut events, Some(std::time::Duration::from_millis(5)))
            .expect("gate 87c poll");
        let now = epoch.elapsed().as_nanos() as u64;
        for ev in events.iter() {
            if let core_net::WsProgress::Failed(e) = conn.on_event(ev, poll.registry(), now) {
                panic!("gate 87c open: {e}");
            }
        }
        assert!(epoch.elapsed() < std::time::Duration::from_secs(10), "gate 87c: never opened");
    }
    let req: [&[u8]; 3] = [
        br#"{"id":"7","method":"order.place","params":{"symbol":"BTCUSDT","side":"BUY","#,
        br#""type":"LIMIT","timeInForce":"IOC","quantity":"0.001","price":"60000","#,
        br#""signature":"Ws+5m/CMnpkko0uBFxGTZ2+fjqqBXsUjRiaz173fPhXTkhoDBYNZ6wcYNeWItdrGn1pvG7vkwx2fhmJdAZ3KDQ=="}}"#,
    ];
    let mut i = 0;
    while i < 50 {
        // The session tickets and rustls' queues growing to their working
        // size: the cold part.
        assert!(gate87c_round(&mut conn, &mut poll, &mut events, epoch, &req) > 0);
        i += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut k = 0u64;
    while k < ROUNDS {
        acc = acc.wrapping_add(gate87c_round(&mut conn, &mut poll, &mut events, epoch, &req) as u64);
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(conn.is_open(), "one session throughout");
    assert_eq!(conn.dials(), 1, "no redial inside the measurement");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_ROUND * ROUNDS,
        "WsConn round: {allocs} allocations ({bytes} B) over {ROUNDS} rounds — rustls' buffered \
         API accounts for exactly {RUSTLS_ALLOCS_PER_ROUND}/round (one sealed record out, one \
         decrypted record in); anything above is ours"
    );
}

/// **BX3 gate 88 — the router's retirement drain (F9).**
///
/// An arm reports every accepted order that ended without a (further)
/// fill as a `Retired`; the router drains at most `RETIRED_DRAIN_MAX`
/// of them per `on_idle`, releases each one's resting row and counts it
/// by `why`. It runs on the engine thread every idle moment, so it must
/// be allocation-free in the steady state — including the poll where
/// the bound fires, which is every poll here: the arm queues a burst of
/// 100 a poll from a preallocated ring, every `why` code and two
/// undefined ones in turn.
#[test]
fn routed_retired_drain_steady_state() {
    use clob_dispatcher::{DispatchError, HaltSignal, OrderDispatch, PaperDispatcher, Retired};
    use core_types::{CancelReq, Fill, ModifyReq, Order};
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, RoutedDispatcher, SlotCaps, RETIRED_DRAIN_MAX,
    };

    const SLOT: u8 = 3;
    const RING: usize = 256;
    const BURST: usize = 100;
    const POLLS: u64 = 1_000;

    /// A fixed ring of retirements, refilled by a burst each idle.
    struct Arm {
        ring: [Retired; RING],
        head: usize,
        len: usize,
        polls: u64,
    }
    impl OrderDispatch for Arm {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
        fn halt_signal(&self) -> HaltSignal {
            HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000)
        }
        fn try_next_retired(&mut self) -> Option<Retired> {
            if self.len == 0 {
                return None;
            }
            let r = self.ring[self.head];
            self.head = (self.head + 1) % RING;
            self.len -= 1;
            Some(r)
        }
        fn on_idle(&mut self) -> bool {
            self.polls += 1;
            let mut k = 0usize;
            while k < BURST && self.len < RING {
                let tail = (self.head + self.len) % RING;
                let n = self.polls * BURST as u64 + k as u64;
                self.ring[tail] = Retired::new(n, SLOT, (n % 8) as u8);
                self.len += 1;
                k += 1;
            }
            false
        }
    }

    // Boot-time construction — outside the window.
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            SLOT as usize,
            ExecMode::Live,
            &[core_types::VenueId::Hyperliquid.to_u8()],
            SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64),
            HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000),
        )
        .expect("boot: slot 3 live");
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        Arm {
            ring: [Retired::default(); RING],
            head: 0,
            len: 0,
            polls: 0,
        },
        core_time::WallAnchor::new(1_789_776_001_000_000_000, 1_789_776_001_000_000_000),
    );
    // One poll outside the window: the seeding edge.
    d.on_idle();

    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < POLLS {
        std::hint::black_box(d.on_idle());
        std::hint::black_box(d.retired().total());
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    // The bound fired on every poll: the arm always held more.
    assert_eq!(d.retired().total(), (POLLS + 1) * RETIRED_DRAIN_MAX as u64);
    let mut w = 0usize;
    while w < 6 {
        assert!(d.retired().by_why[w] > 0, "why {w} never drained");
        w += 1;
    }
    assert!(
        d.retired().by_why[clob_dispatcher::RETIRED_WHY_UNKNOWN] > 0,
        "the undefined codes were counted"
    );
    assert_eq!(
        allocs, 0,
        "gate 88: the retirement drain allocated {allocs} times ({bytes} B) over {POLLS} polls"
    );
}

/// **BX3 gate 89 — two live arms split by venue, under the HYPARB split.**
///
/// The engine's BX3 shape: `SlotSplit<VenueSplit<Hl, Bn>, HyparbLive>`
/// behind the router, with the legacy anchor aliased to Binance. Inside
/// the window every verb reaches each arm by its route venue (Hyperliquid
/// ids, native Binance ids and the aliased anchor), the idle poll merges
/// the signal of a slot both venue arms trade, and ticks, venue events
/// and option summaries fan out to all three arms. Zero allocations.
#[test]
fn venuesplit_route_steady_state() {
    use clob_dispatcher::{
        DispatchError, HaltSignal, OrderDispatch, PaperDispatcher, RouteAliases,
    };
    use core_types::{
        CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, OptSummary, Order, Price, Qty, Side,
        Tick, VenueId,
    };
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, InstrumentSpec, RoutedDispatcher, SlotCaps, SlotSplit,
        VenueSplit, LAW_LINEAR, LAW_SPOT,
    };

    const T0: u64 = 1_789_776_001_000_000_000;
    const BN: u8 = VenueId::Binance as u8;
    const HL: u8 = VenueId::Hyperliquid as u8;
    const EVM: u8 = VenueId::HyperEvm as u8;

    /// Accepts every verb and counts what reached it.
    #[derive(Default)]
    struct Count {
        verbs: u64,
        idles: u64,
        events: u64,
        ticks: u64,
        opts: u64,
    }
    impl OrderDispatch for Count {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            self.verbs += 1;
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            self.verbs += 1;
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            self.verbs += 1;
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
        fn observe_tick(&mut self, _t: &Tick, _now: core_types::NsTs) {
            self.ticks += 1;
        }
        fn on_venue_event(&mut self, _e: &ChannelEvent) {
            self.events += 1;
        }
        fn on_opt_summary(&mut self, _s: &OptSummary) {
            self.opts += 1;
        }
        fn on_idle(&mut self) -> bool {
            self.idles += 1;
            false
        }
        fn halt_signal(&self) -> HaltSignal {
            HaltSignal::new(1_000_000, 0, 0, 0, false, true, 1_000_000)
        }
    }

    // Boot-time construction — outside the window.
    let caps = SlotCaps::new(100_000_000, i64::MAX, i64::MAX, 64);
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(3, ExecMode::Live, &[HL], caps, HaltLimits::none())
        .expect("slot 3");
    route
        .set_slot(2, ExecMode::Live, &[BN], caps, HaltLimits::none())
        .expect("slot 2");
    route
        .set_slot(5, ExecMode::Live, &[HL, BN], caps, HaltLimits::none())
        .expect("slot 5");
    route
        .set_slot(0, ExecMode::Live, &[EVM, HL], caps, HaltLimits::none())
        .expect("slot 0");
    let aliases = RouteAliases::NONE.with(7, BN).expect("alias");
    let venue = VenueSplit::new(BN, Count::default(), Count::default(), &route);
    let live = SlotSplit::new(0, venue, Count::default());
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        live,
        core_time::WallAnchor::new(T0, T0),
    );
    d.set_route_aliases(aliases);
    let lin = core_types::make_symbol_id(VenueId::Binance, 600);
    d.bind_instrument(&InstrumentSpec::new(7, LAW_SPOT, 0, 0, 0))
        .expect("anchor row");
    d.bind_instrument(&InstrumentSpec::new(lin, LAW_LINEAR, 0, 0, 0))
        .expect("linear row");
    d.on_idle(); // the seeding edge

    let mk = |slot: u8, venue: u8, sym: core_types::SymbolId, oid: u64| {
        let mut o = Order::new(
            T0,
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            0,
            Price::from_raw(1_000_000),
            Qty::from_raw(1_000_000),
            oid,
        );
        o.venue = venue;
        o.strategy_id = slot;
        o
    };
    let tick = Tick::new(
        T0,
        VenueId::Binance,
        lin,
        1,
        Price::from_raw(1_000_000),
        Qty::from_raw(1_000_000),
        Price::from_raw(1_001_000),
        Qty::from_raw(1_000_000),
    );
    let ev = ChannelEvent::new(T0, VenueId::Binance, ChannelId::Funding, lin, 1, 0, 1, 1);
    let opt = OptSummary::new(T0, VenueId::Binance, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0);

    let g = AllocGuard::new();
    let mut refused = 0u64;
    let mut i = 0u64;
    while i < 10_000 {
        let oid = 1_000_000 + i * 4;
        let (slot, venue, sym) = match i % 5 {
            0 => (3u8, HL, 4096 + (i as u32 & 63)),
            1 => (2u8, BN, lin),
            2 => (5u8, 0u8, 7u32),
            3 => (5u8, HL, 4200 + (i as u32 & 63)),
            _ => (0u8, EVM, core_types::make_symbol_id(VenueId::HyperEvm, 1)),
        };
        let o = mk(slot, venue, sym, oid);
        if d.submit(&o).is_err() {
            refused += 1;
        }
        let mut rep = o;
        rep.client_oid = oid + 1;
        if d.modify(&ModifyReq::new(oid, rep)).is_err() {
            refused += 1;
        }
        if d.cancel(&CancelReq::of(&rep, T0)).is_err() {
            refused += 1;
        }
        d.observe_tick(&tick, T0);
        if i % 8 == 0 {
            d.on_idle();
        }
        if i % 16 == 0 {
            d.on_venue_event(&ev);
        }
        if i % 32 == 0 {
            d.on_opt_summary(&opt);
        }
        std::hint::black_box(d.exec_counters().live_submits);
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    let s = d.live();
    let (hl, bn, evm) = (s.a().a(), s.a().b(), s.b());
    assert_eq!(refused, 0, "every verb was routed and accepted");
    assert!(
        hl.verbs > 0 && bn.verbs > 0 && evm.verbs > 0,
        "a venue arm saw no verb"
    );
    assert_eq!(hl.verbs, 3 * 4_000, "Hyperliquid: slots 3 and 5");
    assert_eq!(
        bn.verbs,
        3 * 4_000,
        "Binance: slot 2 native and the slot-5 anchor"
    );
    assert_eq!(evm.verbs, 3 * 2_000, "HYPARB: slot 0");
    assert!(hl.idles > 0 && bn.idles > 0 && evm.idles > 0);
    assert!(hl.events > 0 && bn.events > 0 && evm.events > 0);
    assert!(hl.ticks > 0 && bn.ticks > 0 && evm.ticks > 0);
    assert!(hl.opts > 0 && bn.opts > 0 && evm.opts > 0);
    assert_eq!(
        allocs, 0,
        "gate 89: the venue split allocated {allocs} times ({bytes} B)"
    );
}

/// Boot a router with one live Binance slot and a full instrument table:
/// 64 rows of each law (spot, linear, inverse, option — half the options
/// writable calls, half non-writable puts). Shared by gates 90 and 91.
fn bx3_instrument_router<L: clob_dispatcher::OrderDispatch>(
    live: L,
    caps: exec_router::SlotCaps,
) -> exec_router::RoutedDispatcher<clob_dispatcher::PaperDispatcher, L> {
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, InstrumentSpec, RoutedDispatcher, INST_CALL,
        INST_WRITABLE, LAW_INVERSE, LAW_LINEAR, LAW_OPTION, LAW_SPOT, LEDGER_INSTRUMENTS,
    };
    const T0: u64 = 1_789_776_001_000_000_000;
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            3,
            ExecMode::Live,
            &[core_types::VenueId::Binance.to_u8()],
            caps,
            HaltLimits::none(),
        )
        .expect("slot 3 on Binance");
    let mut d = RoutedDispatcher::new(
        route,
        clob_dispatcher::PaperDispatcher::new(),
        live,
        core_time::WallAnchor::new(T0, T0),
    );
    d.mark_ledger_seeded();
    let mut k = 0u32;
    while k < LEDGER_INSTRUMENTS as u32 {
        let sym = core_types::make_symbol_id(core_types::VenueId::Binance, 5_000 + k);
        let spec = match k % 4 {
            0 => InstrumentSpec::new(sym, LAW_SPOT, 0, 0, 0),
            1 => InstrumentSpec::new(sym, LAW_LINEAR, 0, 0, 0),
            2 => InstrumentSpec::new(sym, LAW_INVERSE, 0, 100_000_000, 0),
            _ if k % 8 == 3 => InstrumentSpec::new(
                sym,
                LAW_OPTION,
                INST_WRITABLE | INST_CALL,
                1_000_000,
                100_000_000_000,
            ),
            _ => InstrumentSpec::new(sym, LAW_OPTION, 0, 1_000_000_000, 84_000),
        };
        d.bind_instrument(&spec).expect("bind");
        k += 1;
    }
    d
}

/// **BX3 gate 90 — the ledger's instrument rows on the order and fill
/// paths.** A full table (256 rows, every law): orders of every law
/// through the probe and the four clamps — the law refusals (`Short`,
/// `Unpriced`) and every clamp fire inside the window — venue fills that
/// open, grow, reduce and cross positions, settlements, and a modify and
/// a cancel per order. Zero allocations.
#[test]
fn ledger_instrument_rows_steady_state() {
    use clob_dispatcher::{DispatchError, OrderDispatch};
    use core_types::{
        CancelReq, Fill, ModifyReq, OptSummary, Order, Price, Qty, Side, VenueId,
        FILL_FLAG_SETTLEMENT, FILL_ORIGIN_VENUE,
    };
    use exec_router::{SlotCaps, LEDGER_INSTRUMENTS};

    const T0: u64 = 1_789_776_001_000_000_000;
    const E6: i64 = 1_000_000;

    struct Yes;
    impl OrderDispatch for Yes {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
    }

    // Tight enough that every clamp fires; loose enough that most pass.
    let caps = SlotCaps::new(50_000 * E6, 2_000_000 * E6, 50_000_000 * E6, 64);
    let mut d = bx3_instrument_router(Yes, caps);
    // Half the writable calls get an index; the rest stay unpriced.
    let mut k = 0u32;
    while k < LEDGER_INSTRUMENTS as u32 {
        if k % 16 == 3 {
            let sym = core_types::make_symbol_id(VenueId::Binance, 5_000 + k);
            let s = OptSummary::new(
                T0,
                VenueId::Binance,
                sym,
                core_types::OPT_SUMMARY_FLAG_MARK_PX,
                400 * E6 * 1_000,
                0,
                100_000 * E6 * 1_000,
                0,
                0,
                0,
                0,
                0,
            );
            d.on_opt_summary(&s);
        }
        k += 1;
    }

    let g = AllocGuard::new();
    let mut placed = 0u64;
    let mut refused = 0u64;
    let mut i = 0u64;
    while i < 20_000 {
        let k = (i % LEDGER_INSTRUMENTS as u64) as u32;
        let sym = core_types::make_symbol_id(VenueId::Binance, 5_000 + k);
        let buy = (i / 3) % 3 != 0;
        let qty = (1 + (i % 7) as i64) * E6;
        let px = if i % 97 == 0 {
            0
        } else {
            (100 + (i % 900) as i64) * E6
        };
        let oid = 10_000_000 + i * 2;
        let mut o = Order::new(
            T0 + i,
            VenueId::Binance,
            sym,
            if buy { Side::Bid } else { Side::Ask },
            0,
            Price::from_raw(px),
            Qty::from_raw(qty),
            oid,
        );
        o.strategy_id = 3;
        match d.submit(&o) {
            Ok(()) => placed += 1,
            Err(DispatchError::RiskRefused) => refused += 1,
            Err(e) => panic!("unexpected dispatch error {e:?}"),
        }
        let mut rep = o;
        rep.client_oid = oid + 1;
        let _ = d.modify(&ModifyReq::new(oid, rep));
        let _ = d.cancel(&CancelReq::of(&rep, T0 + i));
        // A venue fill on the row — some opening, some crossing — and
        // now and then a settlement.
        let mut f = Fill::new(
            T0 + i,
            sym,
            if buy { Side::Bid } else { Side::Ask },
            Price::from_raw(if px > 0 { px } else { 100 * E6 }),
            Qty::from_raw(qty),
            oid,
        )
        .with_attribution(3, FILL_ORIGIN_VENUE);
        if i % 211 == 0 {
            f = f.with_flags(FILL_FLAG_SETTLEMENT);
        }
        d.on_fill_booked(&f);
        std::hint::black_box(d.ledger().slot_exposure_1e6(3));
        std::hint::black_box(d.ledger().slot_day_turnover_1e6(3));
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    let c = d.counters();
    assert!(placed > 0, "no order was ever accepted");
    assert!(refused > 0);
    assert!(c.refused_short > 0, "the Short law never fired");
    assert!(c.refused_unpriced > 0, "the Unpriced law never fired");
    assert!(c.refused_max_order > 0, "clamp 1 never fired");
    assert!(c.refused_cap_instance > 0, "clamp 3 never fired");
    assert!(d.ledger().counters().fills_booked > 0);
    assert_eq!(
        allocs, 0,
        "gate 90: the instrument rows allocated {allocs} times ({bytes} B)"
    );
}

/// **BX3 gate 91 — the ledger's price feed.** Venue `Mark` events on
/// bound and unbound ids, interleaved with the rolls the family rows
/// bind from and with option summaries — with and without a mark, on
/// bound options and on another venue's ids. Each mark re-prices its
/// row for every slot and keeps the aggregate exact. Zero allocations.
#[test]
fn ledger_price_feed_steady_state() {
    use clob_dispatcher::{DispatchError, OrderDispatch};
    use core_types::{
        CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, OptSummary, Order, Price, Qty, Side,
        VenueId, FILL_ORIGIN_VENUE,
    };
    use exec_router::{SlotCaps, LEDGER_INSTRUMENTS};

    const T0: u64 = 1_789_776_001_000_000_000;
    const E6: i64 = 1_000_000;

    struct Yes;
    impl OrderDispatch for Yes {
        fn submit(&mut self, _o: &Order) -> Result<(), DispatchError> {
            Ok(())
        }
        fn cancel(&mut self, _r: &CancelReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn modify(&mut self, _r: &ModifyReq) -> Result<(), DispatchError> {
            Ok(())
        }
        fn try_next_fill(&mut self) -> Option<Fill> {
            None
        }
        fn stats(&self) -> clob_dispatcher::DispatchStats {
            clob_dispatcher::DispatchStats::default()
        }
    }

    let caps = SlotCaps::new(i64::MAX, i64::MAX, i64::MAX, 64);
    let mut d = bx3_instrument_router(Yes, caps);
    // A position on every row, so every re-price moves the aggregate.
    let mut k = 0u32;
    while k < LEDGER_INSTRUMENTS as u32 {
        let sym = core_types::make_symbol_id(VenueId::Binance, 5_000 + k);
        let f = Fill::new(
            T0,
            sym,
            Side::Bid,
            Price::from_raw(100 * E6),
            Qty::from_raw(E6),
            1,
        )
        .with_attribution(3, FILL_ORIGIN_VENUE);
        d.on_fill_booked(&f);
        k += 1;
    }
    let before = d.ledger().instrument_exposure_1e6(3);

    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < 50_000 {
        let k = (i % (LEDGER_INSTRUMENTS as u64 + 16)) as u32;
        let sym = core_types::make_symbol_id(VenueId::Binance, 5_000 + k);
        match i % 4 {
            0 | 1 => {
                let m = ChannelEvent::new(
                    T0 + i,
                    VenueId::Binance,
                    ChannelId::Mark,
                    sym,
                    i,
                    0,
                    (100 + (i % 50) as i64) * E6,
                    (100 + (i % 51) as i64) * E6,
                );
                d.on_venue_event(&m);
            }
            2 => {
                let s = OptSummary::new(
                    T0 + i,
                    if i % 8 == 2 {
                        VenueId::Deribit
                    } else {
                        VenueId::Binance
                    },
                    sym,
                    if i % 3 == 0 {
                        0
                    } else {
                        core_types::OPT_SUMMARY_FLAG_MARK_PX
                    },
                    (300 + (i % 70) as i64) * E6 * 1_000,
                    0,
                    (90_000 + (i % 20_000) as i64) * E6 * 1_000,
                    0,
                    0,
                    0,
                    0,
                    0,
                );
                d.on_opt_summary(&s);
            }
            _ => {
                let roll = ChannelEvent::new(
                    T0 + i,
                    VenueId::Hyperliquid,
                    ChannelId::InstrumentRoll,
                    (4u32 << 24) | (100 + (i as u32 & 15) * 2),
                    core_types::pack_roll_seq(
                        1_000 + (i as u32 & 15),
                        60,
                        (i & 15) as usize,
                        false,
                    ),
                    0,
                    0,
                    0,
                );
                d.on_venue_event(&roll);
            }
        }
        std::hint::black_box(d.ledger().instrument_exposure_1e6(3));
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert_ne!(
        d.ledger().instrument_exposure_1e6(3),
        before,
        "the marks never re-priced a row"
    );
    assert_eq!(
        allocs, 0,
        "gate 91: the price feed allocated {allocs} times ({bytes} B)"
    );
}

/// BX6 gates 92–93: two USDⓈ-M rows as discovery reports them.
const BX6_FAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.10","maxPrice":"4529764","tickSize":"0.10"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"1000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"100"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]},{"symbol":"ETHUSDT","pair":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"300000","tickSize":"0.01"},{"filterType":"LOT_SIZE","stepSize":"0.001","maxQty":"10000","minQty":"0.001"},{"filterType":"MAX_NUM_ORDERS","limit":200},{"filterType":"MIN_NOTIONAL","notional":"20"},{"filterType":"PERCENT_PRICE","multiplierUp":"1.0500","multiplierDown":"0.9500","multiplierDecimal":"4"}]}]}"#;

/// **BX6 gate 92 — the Binance arm on the engine thread, in the engine's
/// own shape**: `RoutedDispatcher<Paper, SlotSplit<VenueSplit<NullLive,
/// BnArm>, Option<_>>>` (BX11: BIN15's slot beside it, as the engine
/// composes it) with the anchor aliased to Binance and both rows bound in
/// the ledger.
///
/// Every verb passes the router's risk gate, the split and the arm (the
/// alias-first row, the BX-5 quantization, the order budget, QTR,
/// breadth, the per-row cap) onto the command ring; the router's idle
/// hook drains the gateway's events through the arm and takes its
/// confirm-later retirements and renames into the ledger, sweeps the
/// stale marks and reads the arm's counters. Inside the window: IoC and
/// maker places on both sides of the aliased anchor and a native row —
/// accepted, and once the venue's order budget is spent, refused —
/// cancels, modifies, EVERY event kind the gateway sends (each pulse
/// reading the published tallies — BX7, the kit's stats block), the marks
/// (one of them stale: the sweep expires it), `/state`'s counters and
/// (BX11) the arm's own report — its counters, the pulse's words, the
/// governor's windows, the margin book, the round trips' quantiles over
/// trackers that hold samples, one walk a report — and,
/// last, a venue lock that halts the slot and the sweep — the stats word,
/// never a command (XG-1) — that clears it. BX8: a spot row beside them
/// (BTCUSDT's spot book, its own engine id and ledger row): IoC BUYs every
/// fourth turn on spot's own ORDERS pool, each ended as the gateway ends
/// it (FILLED with `EVT_F_HAD_FILL`; EXPIRED without), and a spot maker now
/// and then. Pinned here: 0 B; exactly 100 spot IoCs accepted in the 10 s
/// window, at least one refused, no spot maker accepted (IoC-only: no
/// dead-man). Exercised, not asserted here (no accessor): the refusals'
/// reason, the futures' pool left alone and the unfilled count's decrement
/// — `arm::tests::a_spot_row_is_ioc_only_on_its_own_pool` and
/// `gov::tests::the_spot_pool_mirrors_the_unfilled_count` pin those. A
/// break-and-watch (an 8-byte `Vec` in the arm's spot pool and in its fill
/// decrement) failed it at 970: 937 IoCs at the pool, 33 fills' ends.
///
/// BX9 (the alloc review): two option rows beside them (eapi's BTC call,
/// writable, and put, not — the arm's table; both `LAW_OPTION` rows of the
/// ledger, priced by option summaries every 32nd turn and stamped fresh),
/// the put held long by the gateway's `EVT_POSITION`. Every fourth turn an
/// option IoC in turn: a call BUY, a call SELL (the first answered
/// `-6057`: the arm unwrites the underlying, every later one refused
/// `NotWritable` before the pool), a put SELL (leaving REDUCE-ONLY: the
/// flag on the popped command), a put BUY — on options' own ORDERS pool,
/// whose 10 s window takes 30. Pinned: 0 B; 30 options IoCs accepted, the
/// rest refused (`NotWritable` and the pool's `Orders`, both counted in the
/// report), every put SELL popped reduce-only, the one `-6057`. A
/// break-and-watch (an 8-byte `Vec` before the arm's writing law) failed
/// it at 1 000: every options attempt.
///
/// BX10: two Binance Stocks rows beside them (the scripted venue's AAPL,
/// `BUY_SELL`, and SPY, SELL-only — the arm's table; both `LAW_SPOT` rows
/// of the ledger). Every fourth turn an equity order in turn: an AAPL BUY
/// — on equity's own pool, whose minute takes 200, each popped under the
/// arm's TTL (an equity order rests, DAY), acknowledged and cancelled by
/// its member (equity's cancel passes the arm despite `NO_DEADMAN`); every
/// seventh refused by the venue `486405` (a six-digit code: the reject
/// streak's, `is_venue_code`) — and a SPY BUY, refused `NotLive` (the
/// venue takes no BUY there) before the pool. Pinned: 0 B; 200 equity
/// orders accepted, every one popped with its deadline and cancelled, the
/// rest refused (the pool's `Orders`, SPY's `NotLive`, both counted in the
/// report). A break-and-watch (an 8-byte `Vec` before the arm's equity
/// law) failed it at 1 000: every equity attempt.
///
/// BX13 (R2-2): the arm admits no spot order before the venue's day count
/// is read: the gate gives `EVT_SPOT_DAY` before the window (its spot IoCs
/// would otherwise all be `NotReady`) and again every 64th turn inside it
/// (`SpotOrders::seed`, the caps). Pinned: 0 B; the report's day seeded,
/// its caps 200 000 at `orders_frac` 1.0. A spot SELL needs a holding the
/// ledger never books here (no fill reaches lane 4): the reserve's branch
/// is `arm::tests::a_spot_sell_keeps_its_reserve_past_the_days_share`'s.
#[test]
fn binance_arm_steady_state() {
    use clob_dispatcher::{
        CancelAllState, OrderDispatch, PaperDispatcher, RouteAliases, RETIRED_CANCELED_MEMBER,
        RETIRED_EXPIRED, RETIRED_FILLED,
    };
    use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
    use core_types::{CancelReq, ChannelEvent, ChannelId, ModifyReq, Order, OptSummary, Side, OPT_SUMMARY_FLAG_MARK_PX, ORDER_FLAG_REDUCE_ONLY};
    use exec_binance::arm::{ArmKnobs, BnArm, Refusal};
    use exec_binance::cmd::{
        duplex, BnCmd, BnEvt, EVT_ACK, EVT_BUDGET, EVT_CANCEL_FAILED, EVT_DAY, EVT_F_CLOCK_OK,
        EVT_F_DEADMAN_OK, EVT_F_HAD_FILL, EVT_F_IOC, EVT_F_ORDER_UP, EVT_F_RECONCILED, EVT_F_SWEEP_DONE,
        EVT_F_USER_UP, EVT_LOCK, EVT_MARGIN, EVT_MODIFIED, EVT_MODIFY_FAILED, EVT_POSITION,
        EVT_RECON, EVT_REJECT, EVT_RETIRED, EVT_RING, EVT_SCAN_FAIL, EVT_SPOT_DAY, EVT_STATUS, EVT_SWEEP,
        VERB_CANCEL, VERB_MODIFY, VERB_PLACE,
    };
    use exec_gateway::stats::SWEEP_CLEAR;
    use exec_gateway::EvtTx;
    use exec_binance::equity::scan_eq_symbol;
    use exec_binance::inst::{BindSpec, EqBindSpec, InstTable, OptBindSpec, PRODUCT_EQUITY, PRODUCT_OPTIONS, PRODUCT_SPOT, PRODUCT_USDM};
    use exec_binance::margin::{MarginBook, SLOTS};
    use exec_binance::testvenue::{EQ_INFO, OPT_CALL, OPT_PUT};
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, InstrumentSpec, NullLiveDispatcher, RoutedDispatcher,
        SlotCaps, SlotSplit, VenueSplit, INST_CALL, INST_WRITABLE, LAW_LINEAR, LAW_OPTION, LAW_SPOT,
    };

    const SLOT: u8 = 2;
    const ALIAS: SymbolId = 7;
    const N: u64 = 4_000;

    /// The gateway's emit: only with room (the kit halts on a full ring).
    fn give(tx: &mut EvtTx<BnEvt, EVT_RING>, e: &BnEvt) -> bool {
        tx.room() > 0 && tx.emit(e)
    }

    // Boot-time construction — outside the window.
    let mut disc = ingress_binance::discovery::BnDiscovery::new();
    disc.ingest_body(BX6_FAPI).expect("gate 92 discovery");
    let (mut t, mut w) = InstTable::new(ALIAS);
    let eth = core_types::make_symbol_id(VenueId::Binance, 514);
    let rows = [(ALIAS, &b"BTCUSDT"[..]), (eth, &b"ETHUSDT"[..])];
    for (sym, name) in rows {
        let row = disc.find(name).expect("gate 92 row");
        t.bind(&mut w, &BindSpec { sym, product: PRODUCT_USDM, row, owned: true, maker_ok: true })
            .expect("gate 92 bind");
    }
    // BX8: BTCUSDT's spot book beside them, under its own engine id —
    // IoC-only (spot has no dead-man).
    let spot = core_types::make_symbol_id(VenueId::Binance, 2);
    let mut spot_disc = ingress_binance::discovery::BnDiscovery::new();
    spot_disc.ingest_body(exec_binance::testvenue::SPOT).expect("gate 92 spot discovery");
    let spot_row = t
        .bind(
            &mut w,
            &BindSpec { sym: spot, product: PRODUCT_SPOT, row: spot_disc.find(b"BTCUSDT").expect("gate 92 spot row"), owned: true, maker_ok: false },
        )
        .expect("gate 92 spot bind");
    // BX9: eapi's BTC call (writable) and put (not), a month ahead.
    let mut eapi = ingress_binance::eapi::EapiDiscovery::new();
    eapi.ingest_exchange_info(exec_binance::testvenue::EAPI).expect("gate 92 eapi discovery");
    let ahead = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_millis() as i64 + 30 * 86_400_000;
    let (call, put) = (core_types::make_symbol_id(VenueId::Binance, 1_025), core_types::make_symbol_id(VenueId::Binance, 1_026));
    let mut opt_rows = [0u16; 2];
    for (k, (sym, name)) in [(call, OPT_CALL), (put, OPT_PUT)].into_iter().enumerate() {
        let mut row = *eapi.rows().iter().find(|r| r.symbol() == name.as_bytes()).expect("gate 92 option row");
        row.expiry_ms = ahead;
        opt_rows[k] = t.bind_option(&mut w, &OptBindSpec { sym, row: &row, owned: true, writable: k == 0 }).expect("gate 92 option bind");
    }
    let (call_row, put_row) = (opt_rows[0], opt_rows[1]);
    // BX10: AAPL (both sides) and SPY (SELL-only), Binance Stocks.
    let (aapl, spy) = (core_types::make_symbol_id(VenueId::Binance, 4_097), core_types::make_symbol_id(VenueId::Binance, 4_098));
    let mut eq_rows = [0u16; 2];
    for (k, (sym, name)) in [(aapl, &b"AAPL"[..]), (spy, &b"SPY"[..])].into_iter().enumerate() {
        let row = scan_eq_symbol(EQ_INFO, name).expect("gate 92 equity rules");
        eq_rows[k] = t.bind_equity(&mut w, &EqBindSpec { sym, row: &row, owned: true, bstock: b"" }).expect("gate 92 equity bind");
    }
    let (eng, mut gw) = duplex();
    let mut lim = [0i64; SLOTS];
    lim[SLOT as usize] = 600_000;
    let mut prod = [0u8; SLOTS];
    prod[SLOT as usize] = (1 << PRODUCT_USDM) | (1 << PRODUCT_SPOT) | (1 << PRODUCT_OPTIONS) | (1 << PRODUCT_EQUITY);
    let arm = BnArm::new(
        eng,
        t,
        ArmKnobs {
            orders_frac_1e6: 1_000_000,
            qtr_frac_1e6: 1_000_000,
            owner_slot: SLOT,
            max_symbols: 20,
            min_maker_ttl_ns: 5_000_000_000,
            margin: MarginBook::new(lim, prod),
            anchor: core_time::WallAnchor::now(),
            mode: exec_binance::mode::AccountMode::Classic,
            demo: false,
            equity_ttl_ns: 30_000_000_000,
        },
    );
    let bn = VenueId::Binance.to_u8();
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            SLOT as usize,
            ExecMode::Live,
            &[bn],
            SlotCaps::new(i64::MAX, i64::MAX, i64::MAX, 1_024),
            HaltLimits::none(),
        )
        .expect("gate 92 route");
    let split = VenueSplit::new(bn, NullLiveDispatcher::new(), arm, &route);
    // BX11 (the alloc review's AL-4): BIN15's slot beside the split, an
    // `Option` arm (here none) — the report crosses every forwarder the
    // engine's does.
    let split = SlotSplit::new(3, split, Option::<NullLiveDispatcher>::None);
    let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), split, core_time::WallAnchor::now());
    d.set_route_aliases(RouteAliases::NONE.with(ALIAS, bn).expect("gate 92 alias"));
    for sym in [ALIAS, eth] {
        d.bind_instrument(&InstrumentSpec::new(sym, LAW_LINEAR, 0, 0, 0)).expect("gate 92 ledger row");
    }
    d.bind_instrument(&InstrumentSpec::new(spot, LAW_SPOT, 0, 0, 0)).expect("gate 92 spot ledger row");
    // Both options writable in the ledger: the arm's law is the guard here.
    d.bind_instrument(&InstrumentSpec::new(call, LAW_OPTION, INST_WRITABLE | INST_CALL, 1_000_000, 85_000_000_000)).expect("gate 92 call row");
    d.bind_instrument(&InstrumentSpec::new(put, LAW_OPTION, INST_WRITABLE, 1_000_000, 90_000_000_000)).expect("gate 92 put row");
    for sym in [aapl, spy] {
        d.bind_instrument(&InstrumentSpec::new(sym, LAW_SPOT, 0, 0, 0)).expect("gate 92 equity ledger row");
    }
    let summary = |now: u64, sym: SymbolId, mark_1e6: i64| {
        OptSummary::new(now, VenueId::Binance, sym, OPT_SUMMARY_FLAG_MARK_PX, mark_1e6 * 1_000, 0, 65_000_000_000_000, 0, 0, 0, 0, 0)
    };
    const CALL_MARK: i64 = 500_000_000;
    const PUT_MARK: i64 = 25_100_000_000;
    let healthy = |now: u64| {
        let mut s = BnEvt::new(EVT_STATUS, now);
        s.flags = EVT_F_ORDER_UP | EVT_F_USER_UP | EVT_F_CLOCK_OK | EVT_F_DEADMAN_OK;
        s
    };
    let t0 = core_time::now_ns();
    let mut r = BnEvt::new(EVT_RECON, t0);
    r.flags = EVT_F_RECONCILED;
    let mut day = BnEvt::new(EVT_DAY, t0);
    day.slot = SLOT;
    day.a = (t0 / 86_400_000_000_000) as i64;
    // The put held long (the gateway's booked position).
    let mut held = BnEvt::new(EVT_POSITION, t0);
    held.row = put_row;
    held.a = 10_000;
    // BX13 (R2-2): spot's day as the venue counts it — no spot order before
    // — for the UTC day it was asked on.
    let today = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_millis() / 86_400_000) as i64;
    // The venue's limit — 150 000 at the boot, 160 000 in the window (not
    // the 200 000 the pool assumes before a read): the caps are the last
    // read's (the alloc review's rounds 2 and 3).
    let spot_day = |now: u64, limit: i64| {
        let mut e = BnEvt::new(EVT_SPOT_DAY, now);
        e.a = 0;
        e.b = limit;
        e.c = today;
        e
    };
    assert!(
        give(&mut gw.evt, &healthy(t0))
            && give(&mut gw.evt, &r)
            && give(&mut gw.evt, &day)
            && give(&mut gw.evt, &held)
            && give(&mut gw.evt, &spot_day(t0, 150_000))
    );
    d.on_idle();
    d.on_idle();
    let mut boot = clob_dispatcher::BinanceArmReport::default();
    assert!(d.binance_arm_report(t0, &mut boot), "gate 92: the arm's report");
    assert_eq!((boot.spot_day_cap, boot.spot_sell_cap), (150_000, 150_000), "gate 92: the boot's read seeded the caps");
    assert!(d.ledger().is_slot_seeded(SLOT as usize), "gate 92: the slot is seeded from the arm's day");
    // A mark already stale when the window opens: the router's next sweep
    // (once a second) expires it inside the window.
    std::thread::sleep(std::time::Duration::from_millis(1_050));
    let old = core_time::now_ns().saturating_sub(exec_router::ledger::MARK_STALE_NS + 1_000_000_000);
    d.on_venue_event(&ChannelEvent::new(old, VenueId::Binance, ChannelId::Mark, eth, 0, 0, 3_000_000_000, 3_000_000_000));
    let expired_before = d.ledger().counters().marks_expired;

    // The venue's side keeps what a real one knows: a refused modify's new
    // id names the order still resting under its previous one.
    let mut aborted = [(0u64, 0u64); 16];
    let mut aborted_at = 0usize;

    let mut bn_report = clob_dispatcher::BinanceArmReport::default();
    // The trackers hold samples: the report's quantile walks are real
    // walks inside the window (one a report, round robin).
    for ns in [900_000u64, 2_000_000, 40_000_000] {
        gw.stats.post_rtt.record(ns);
        gw.stats.cancel_rtt.record(ns);
        gw.stats.fill_venue_to_local.record(ns);
        gw.stats.rest_rtt.record(ns);
        gw.stats.push_to_write.record(ns / 1_000);
    }
    let g = AllocGuard::new();
    // The sweep is due: the stale mark stops pricing its row.
    d.on_idle();
    let (mut ok, mut refused, mut polls) = (0u64, 0u64, 0u64);
    let (mut spot_ok, mut spot_refused, mut spot_makers) = (0u64, 0u64, 0u64);
    // BX13: the day's reads given inside the window.
    let mut days_given = 0u32;
    let (mut opt_ok, mut opt_refused, mut put_sells, mut reduce_only, mut unwritten) = (0u64, 0u64, 0u64, 0u64, false);
    let (mut eq_ok, mut eq_refused, mut eq_popped, mut eq_ttl, mut eq_cancels, mut eq_rejected) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut eq_six = 0u64;
    d.on_opt_summary(&summary(core_time::now_ns(), call, CALL_MARK));
    d.on_opt_summary(&summary(core_time::now_ns(), put, PUT_MARK));
    let mut i = 0u64;
    while i < N {
        let now = core_time::now_ns();
        if i % 32 == 0 {
            let _ = give(&mut gw.evt, &healthy(now));
            for sym in [ALIAS, eth] {
                let px = if sym == ALIAS { 65_000_000_000 } else { 3_000_000_000 };
                d.on_venue_event(&ChannelEvent::new(now, VenueId::Binance, ChannelId::Mark, sym, i, 0, px, px + 10_000));
            }
            d.on_opt_summary(&summary(now, call, CALL_MARK));
            d.on_opt_summary(&summary(now, put, PUT_MARK));
        }
        // BX9: an option IoC every fourth turn, in turn — a call BUY, a
        // call SELL, a put SELL (reduce-only), a put BUY.
        if i % 4 == 1 {
            let (sym, side, px) = match (i / 4) % 4 {
                0 => (call, Side::Bid, CALL_MARK),
                1 => (call, Side::Ask, CALL_MARK),
                2 => (put, Side::Ask, PUT_MARK),
                _ => (put, Side::Bid, PUT_MARK),
            };
            let mut o = Order::new(now, VenueId::Binance, sym, side, ORDER_KIND_IOC, Price::from_raw(px), Qty::from_raw(10_000), 30_000 + i);
            o.strategy_id = SLOT;
            if d.submit(&o).is_ok() {
                opt_ok += 1;
            } else {
                opt_refused += 1;
            }
        }
        // BX10: an equity order every fourth turn, in turn — an AAPL BUY, a
        // SPY BUY (SELL-only there).
        if i % 4 == 2 {
            let (sym, px) = if (i / 4) % 2 == 0 { (aapl, 336_100_000) } else { (spy, 650_000_000) };
            let mut o = Order::new(now, VenueId::Binance, sym, Side::Bid, ORDER_KIND_IOC, Price::from_raw(px), Qty::from_raw(30_000), 40_000 + i);
            o.strategy_id = SLOT;
            if d.submit(&o).is_ok() {
                eq_ok += 1;
                // It rests (DAY): the member cancels it — equity's cancel
                // passes the arm despite its row's NO_DEADMAN.
                if d.cancel(&CancelReq::of(&o, now)).is_ok() {
                    eq_cancels += 1;
                }
            } else {
                eq_refused += 1;
            }
        }
        // The gateway's cold reports, one kind in turn.
        if i % 64 == 16 {
            let mut e = match (i / 64) % 6 {
                0 => BnEvt::new(EVT_MARGIN, now),
                1 => BnEvt::new(EVT_BUDGET, now),
                2 => BnEvt::new(EVT_SWEEP, now),
                3 => BnEvt::new(EVT_POSITION, now),
                4 => BnEvt::new(EVT_RECON, now),
                _ => BnEvt::new(EVT_DAY, now),
            };
            e.product = PRODUCT_USDM;
            e.slot = SLOT;
            e.why = (i / 64 % 5) as u8;
            e.a = if e.kind == EVT_MARGIN { 100_000 } else { (i & 7) as i64 };
            e.b = 1_000_000_000;
            e.c = 1_000_000_000;
            e.flags = if e.kind == EVT_RECON { EVT_F_RECONCILED } else { 0 };
            if e.kind == EVT_BUDGET {
                // Already past: an observation, not a floor.
                e.a = 1;
            }
            if e.kind == EVT_DAY {
                e.a = (now / 86_400_000_000_000) as i64;
            }
            let _ = give(&mut gw.evt, &e);
        }
        // BX13 (R2-2): the day's read again now and then (a new venue day's
        // first cycle): the arm's seed, in the window.
        if i % 64 == 48 {
            days_given += u32::from(give(&mut gw.evt, &spot_day(now, 160_000)));
        }
        let btc = i & 4 == 0;
        let buy = i & 1 == 0;
        let maker = i & 2 == 0;
        // Off the tick and off the step: the quantization has work to do.
        let (sym, px, qty) = if btc {
            (ALIAS, 65_000_000_000 + (i % 100) as i64 * 100_000 + 37, 2_345)
        } else {
            (eth, 3_000_000_000 + (i % 100) as i64 * 10_000 + 3, 12_345)
        };
        let side = if buy { Side::Bid } else { Side::Ask };
        let kind = if maker { ORDER_KIND_MAKER } else { ORDER_KIND_IOC };
        let mut o = Order::new(now, VenueId::Binance, sym, side, kind, Price::from_raw(px), Qty::from_raw(qty), 10_000 + i);
        o.strategy_id = SLOT;
        o.ttl_ns = if maker { 6_000_000_000 } else { 0 };
        if d.submit(&o).is_ok() {
            ok += 1;
            if maker && i % 8 < 4 {
                let _ = d.cancel(&CancelReq::of(&o, now));
            } else if maker {
                let mut repl = o;
                repl.client_oid = 1_000_000 + i;
                repl.qty = Qty::from_raw(qty + 1_000);
                let _ = d.modify(&ModifyReq::new(o.client_oid, repl));
                let _ = d.cancel(&CancelReq::of(&repl, now));
            }
        } else {
            refused += 1;
        }
        // BX8: the spot row — an IoC BUY every fourth turn (a spot sell is
        // bounded by the holding), on spot's own pool; now and then a
        // maker, refused (IoC-only).
        if i % 4 == 3 {
            let maker = i % 64 == 3;
            let kind = if maker { ORDER_KIND_MAKER } else { ORDER_KIND_IOC };
            let px = Price::from_raw(65_000_000_000 + (i % 100) as i64 * 10_000 + 7);
            let mut o = Order::new(now, VenueId::Binance, spot, Side::Bid, kind, px, Qty::from_raw(2_345), 20_000 + i);
            o.strategy_id = SLOT;
            o.ttl_ns = if maker { 6_000_000_000 } else { 0 };
            match (d.submit(&o).is_ok(), maker) {
                (true, false) => spot_ok += 1,
                (false, false) => spot_refused += 1,
                (placed, true) => spot_makers += u64::from(placed),
            }
        }
        // The gateway's side: every command answered in order — now and
        // then refused (a reject, a failed cancel, a failed modify).
        while let Some(gc) = gw.cmd.pop() {
            let c: BnCmd = *gc;
            drop(gc);
            if c.verb == VERB_PLACE && c.row == put_row && c.side == Side::Ask as u8 {
                put_sells += 1;
                reduce_only += u64::from(c.flags & ORDER_FLAG_REDUCE_ONLY != 0);
            }
            if c.verb == VERB_PLACE && (c.row == eq_rows[0] || c.row == eq_rows[1]) {
                eq_popped += 1;
                eq_ttl += u64::from(c.ttl_deadline_ns > c.pushed_ns && c.pushed_ns != 0);
            }
            let equity = c.row == eq_rows[0] || c.row == eq_rows[1];
            let e = match c.verb {
                // BX10: an equity order rests (DAY): acknowledged, its
                // member's cancel retires it; every seventh refused by the
                // venue with a positive six-digit code (the streak's).
                VERB_PLACE if equity && eq_popped % 7 == 3 => {
                    eq_rejected = c.client_oid;
                    eq_six += 1;
                    let mut x = BnEvt::order(EVT_REJECT, now, c.client_oid, c.row, c.slot);
                    x.code = 486_405;
                    x
                }
                VERB_PLACE if equity => BnEvt::order(EVT_ACK, now, c.client_oid, c.row, c.slot),
                // The refused order's cancel finds nothing to end.
                VERB_CANCEL if equity && c.client_oid == eq_rejected => BnEvt::new(0, now),
                // BX9: the first call SELL — not eligible to write (`-6057`).
                VERB_PLACE if c.row == call_row && c.side == Side::Ask as u8 && !unwritten => {
                    unwritten = true;
                    let mut x = BnEvt::order(EVT_REJECT, now, c.client_oid, c.row, c.slot);
                    x.code = -6057;
                    x
                }
                // An IoC refused (a maker's refusal would meet its cancel).
                VERB_PLACE if i % 97 == 1 && c.kind != ORDER_KIND_MAKER => {
                    let mut x = BnEvt::order(EVT_REJECT, now, c.client_oid, c.row, c.slot);
                    x.code = -2019;
                    x
                }
                VERB_PLACE if c.kind == ORDER_KIND_MAKER => BnEvt::order(EVT_ACK, now, c.client_oid, c.row, c.slot),
                VERB_PLACE => {
                    let _ = give(&mut gw.evt, &BnEvt::order(EVT_ACK, now, c.client_oid, c.row, c.slot));
                    let mut x = BnEvt::order(EVT_RETIRED, now, c.client_oid, c.row, c.slot);
                    x.why = if i % 3 == 0 { RETIRED_FILLED } else { RETIRED_EXPIRED };
                    // A spot fill's end says it filled (BX8: the order
                    // leaves the day's unfilled count).
                    let had_fill = c.row == spot_row && x.why == RETIRED_FILLED;
                    x.flags = EVT_F_IOC | (u8::from(had_fill) * EVT_F_HAD_FILL);
                    x
                }
                VERB_MODIFY if i % 13 == 0 => {
                    aborted[aborted_at & 15] = (c.client_oid, c.prev_client_oid);
                    aborted_at += 1;
                    let mut x = BnEvt::order(EVT_MODIFY_FAILED, now, c.client_oid, c.row, c.slot);
                    x.a = c.prev_client_oid as i64;
                    x.code = -1000;
                    x
                }
                VERB_MODIFY => {
                    let mut x = BnEvt::order(EVT_MODIFIED, now, c.client_oid, c.row, c.slot);
                    x.a = c.prev_client_oid as i64;
                    x.b = c.qty_1e6;
                    x.c = c.px_1e6;
                    x.side = c.side;
                    x
                }
                VERB_CANCEL => {
                    if i % 11 == 0 {
                        let mut f = BnEvt::order(EVT_CANCEL_FAILED, now, c.client_oid, c.row, c.slot);
                        f.code = -1000;
                        let _ = give(&mut gw.evt, &f);
                    }
                    // The order ends under the id it rests by.
                    let mut oid = c.client_oid;
                    let mut k = 0;
                    while k < aborted.len() {
                        if aborted[k].0 == oid && oid != 0 {
                            oid = aborted[k].1;
                            aborted[k] = (0, 0);
                            break;
                        }
                        k += 1;
                    }
                    let mut x = BnEvt::order(EVT_RETIRED, now, oid, c.row, c.slot);
                    x.why = RETIRED_CANCELED_MEMBER;
                    x
                }
                _ => BnEvt::new(0, now),
            };
            let _ = give(&mut gw.evt, &e);
        }
        polls += d.on_idle() as u64;
        if i % 256 == 0 {
            std::hint::black_box(d.exec_counters());
            assert!(d.binance_arm_report(core_time::now_ns(), &mut bn_report), "gate 92: the arm's report");
            std::hint::black_box(&bn_report);
        }
        i += 1;
    }
    // Last: a venue lock and a frame that did not scan halt the slot; the
    // router asks for the sweep and the gateway confirms it clear.
    let now = core_time::now_ns();
    let _ = give(&mut gw.evt, &BnEvt::new(EVT_LOCK, now));
    let _ = give(&mut gw.evt, &BnEvt::new(EVT_SCAN_FAIL, now));
    let mut swept = 0u32;
    let mut seen_req = gw.stats.sweep_requested();
    let mut k = 0;
    while k < 8 {
        d.on_idle();
        while let Some(gc) = gw.cmd.pop() {
            drop(gc);
        }
        // The sweep is the word (XG-1): the gateway answers it in the word,
        // then reports it.
        let req = gw.stats.sweep_requested();
        if req != seen_req {
            seen_req = req;
            swept += 1;
            gw.stats.set_sweep(req, SWEEP_CLEAR);
            let mut s = BnEvt::new(EVT_SWEEP, core_time::now_ns());
            s.flags = EVT_F_SWEEP_DONE;
            let _ = give(&mut gw.evt, &s);
        }
        k += 1;
    }
    let state = d.live().cancel_all_state();
    let (allocs, bytes, _) = g.delta();
    assert!(d.binance_arm_report(core_time::now_ns(), &mut bn_report), "gate 92: the arm's report");

    assert!(ok > 100 && refused > 0, "both halves of the gate ran: {ok} accepted, {refused} refused");
    assert_eq!(
        (spot_ok, spot_makers),
        (100, 0),
        "gate 92: spot's own pool took its 10 s window's 100 IoCs ({spot_refused} refused there); no spot maker"
    );
    assert!(spot_refused > 0, "gate 92: spot's pool was spent");
    assert_eq!(
        (bn_report.spot_day_seeded, bn_report.spot_day_cap, bn_report.spot_sell_cap),
        (1, 160_000, 160_000),
        "gate 92: spot's day seeded from the venue's reads (BX13), its caps the window's read's limit at orders_frac 1.0"
    );
    assert!(days_given > 0, "gate 92: the day's read given inside the window");
    assert_eq!(opt_ok, 30, "gate 92: options' own pool took its 10 s window's 30 IoCs ({opt_refused} refused)");
    assert!(
        unwritten && bn_report.refused[Refusal::NotWritable as usize] > 0 && bn_report.refused[Refusal::Orders as usize] > 0,
        "gate 92: the -6057 unwrote the call ({:?})",
        bn_report.refused
    );
    assert!(put_sells > 0 && reduce_only == put_sells, "gate 92: every put SELL left reduce-only ({reduce_only} of {put_sells})");
    assert_eq!(
        (eq_ok, eq_popped, eq_ttl, eq_cancels),
        (200, 200, 200, 200),
        "gate 92: equity's own pool took its minute's 200 orders ({eq_refused} refused), each popped under the arm's TTL and cancelled"
    );
    // The alloc review's round-2 I2: the six-digit refusal's branch ran —
    // every seventh equity order from the third — and the arm counted each
    // as a venue reject.
    assert_eq!(eq_six, 29, "gate 92: the venue's six-digit refusals");
    assert!(
        bn_report.refused[Refusal::NotLive as usize] > 0 && eq_refused + eq_ok == N / 4,
        "gate 92: SPY's BUYs refused NotLive ({:?})",
        bn_report.refused
    );
    assert!(polls > 100, "the idle hook drained events: {polls}");
    let c = d.exec_counters();
    assert!(c.arm_bn.submitted > 100 && c.arm_bn.rejected >= eq_six, "{:?}", c.arm_bn);
    assert!(d.ledger().counters().marks_expired > expired_before, "the stale mark was swept");
    assert_eq!((swept, state), (1, CancelAllState::Clear), "the lock halted the slot and the sweep cleared it");
    assert_eq!(allocs, 0, "gate 92: the Binance arm allocated {allocs} times ({bytes} B)");
}

/// **BX6 gate 93 — the Binance gateway's order path either side of the
/// socket.** Everything the gateway thread does per order that is not
/// the transport (gate 87c measures `WsConn` itself): the client id
/// classified; `order.place`, `order.cancel`, `order.modify`,
/// `order.status` and `v2/account.status` rendered — their numbers and
/// client ids IN the frame — and masked by core-net's own writer into a
/// send window; the answers and user events scanned in place and their
/// rows found; the open-order table (insert, rename, remove), the
/// in-flight table and the deadline queue (BX7: the kit's `Pending` and
/// `TimerQueue` — a request's id and deadline, an order's TTL), the
/// trade-id and ended rings and the clock (BX7b: its `serverTime` answer
/// read too); the dead-man's
/// signed query rendered and signed in its window, its answer and a
/// refusal scanned; and — at their own cadence — the reconciliation's
/// scans and judges. BX7: a position update's not-cross positions walked
/// in one pass (`NotCross`), each looked up; an `ACCOUNT_CONFIG_UPDATE`
/// carrying a leverage and the multi-assets mode read. BX11: a trade's two
/// reports seen by the trade-id ring — `TRADE_LITE` books it, its update
/// owes the commission — and the update's commission (×1e9, its asset
/// `N`) classified against the row, the trade's notional taken, and its
/// `J_FEE` noted into the journal's ring (drained as the writer would).
/// Zero allocations.
#[test]
fn binance_gateway_render_and_scan_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::clock::VenueClock;
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
    use exec_binance::journal::{
        fee_asset, fee_code, fee_notional_1e9, JournalTx, FEE_ASSET_SETTLE, JOURNAL_RING, J_ANCHOR, J_FEE, TRADE_BY_MEMBER,
    };
    use exec_binance::oot::{EndedRing, Oot, TidRing};
    use exec_binance::recon::{day_increasing_1e6, judge_positions, scan_um_account, Leg, PosRow, Verdict};
    use exec_binance::rest::{
        scan_countdown, scan_error, scan_orders, scan_server_time, scan_trades, OrderRow, QueryWriter, TradeRow,
    };
    use exec_binance::userstream::{scan_user_event, UserEvent, UE_OTHER};
    use exec_binance::wsapi::{
        queue_account, queue_cancel, queue_modify, queue_place, queue_status, scan_answer, FrameSink,
        Part, Place, WsAnswer, WsStatic,
    };

    const N: u64 = 20_000;
    const PLACED: &[u8] = br#"{"id":17,"status":200,"result":{"orderId":8001,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.0","avgPrice":"0.00","origQty":"0.002","executedQty":"0","cumQuote":"0","timeInForce":"GTX","type":"LIMIT","side":"BUY","updateTime":1790000000000},"rateLimits":[{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":300,"count":1}]}"#;
    const REFUSED: &[u8] = br#"{"id":18,"status":400,"error":{"code":-5022,"msg":"Due to the order could not be executed as maker, the Post Only order will be rejected."}}"#;
    const UPDATE: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000123,"T":1790000000120,"o":{"s":"BTCUSDT","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.002","p":"65000.0","ap":"64999.9","sp":"0","x":"TRADE","X":"FILLED","i":8001,"l":"0.002","z":"0.002","L":"64999.9","N":"USDT","n":"0.06499990","T":1790000000120,"t":9001,"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_MAKER","pm":"NONE","gtd":0}}"#;
    const LITE: &[u8] = br#"{"e":"TRADE_LITE","E":1790000000121,"T":1790000000120,"s":"BTCUSDT","q":"0.002","p":"65000.0","m":false,"c":"mv6512ab0f2000000000000006500000","S":"BUY","L":"64999.9","l":"0.002","t":9001,"i":8001}"#;
    const OPEN: &[u8] = br#"[{"avgPrice":"0.00","clientOrderId":"mv6512ab0f2000000000000006500000","cumQuote":"0","executedQty":"0","orderId":8001,"origQty":"0.002","price":"60000.0","side":"BUY","status":"NEW","symbol":"BTCUSDT","timeInForce":"GTX","type":"LIMIT","updateTime":1790000000000},{"avgPrice":"0.00","clientOrderId":"web_stranger_1","cumQuote":"0","executedQty":"0","orderId":7002,"origQty":"0.001","price":"90000.0","side":"SELL","status":"NEW","symbol":"BTCUSDT","timeInForce":"GTC","type":"LIMIT","updateTime":1790000000000}]"#;
    const TRADES: &[u8] = br#"[{"buyer":true,"commission":"0.032","commissionAsset":"USDT","id":501,"maker":false,"orderId":4001,"price":"64000.0","qty":"0.001","quoteQty":"64.0","realizedPnl":"0","side":"BUY","positionSide":"BOTH","symbol":"BTCUSDT","time":1790000000001},{"buyer":false,"commission":"0.032","commissionAsset":"USDT","id":502,"maker":false,"orderId":4002,"price":"64100.0","qty":"0.001","quoteQty":"64.1","realizedPnl":"0.1","side":"SELL","positionSide":"BOTH","symbol":"BTCUSDT","time":1790000000002}]"#;
    const ACCOUNT: &[u8] = br#"{"id":19,"status":200,"result":{"totalInitialMargin":"1.00","totalMaintMargin":"0.52","totalWalletBalance":"1000.00","totalMarginBalance":"1000.00","availableBalance":"999.00","assets":[{"asset":"USDT","walletBalance":"1000.00"}],"positions":[{"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"0.003","unrealizedProfit":"0.00","notional":"195.00","updateTime":1790000000000}]}}"#;

    const COUNTDOWN: &[u8] = br#"{"symbol":"BTCUSDT","countdownTime":"30000"}"#;
    // BX7b: the venue clock's answer (the cycle's clock read; one whole
    // object, the key once).
    const TIME: &[u8] = br#"{"serverTime":1790000000000}"#;
    // BX7 (carried): an account reconfigured — a leverage and the
    // multi-assets mode, in one frame.
    const CONFIG_UPDATE: &[u8] = br#"{"e":"ACCOUNT_CONFIG_UPDATE","E":1790000000125,"T":1790000000121,"ac":{"s":"BTCUSDT","l":25},"ai":{"j":true}}"#;
    // BX7 (BX-19): a position update naming two isolated positions.
    const ACCOUNT_UPDATE: &[u8] = br#"{"e":"ACCOUNT_UPDATE","E":1790000000124,"T":1790000000120,"a":{"m":"ORDER","B":[{"a":"USDT","wb":"1000.00","cw":"1000.00","bc":"0"}],"P":[{"s":"ETHUSDT","pa":"1","ep":"3000","cr":"0","up":"0","mt":"isolated","iw":"10","ps":"BOTH"},{"s":"BTCUSDT","pa":"0.002","ep":"65000","cr":"0","up":"0","mt":"isolated","iw":"10","ps":"BOTH"}]}}"#;
    const REST_REFUSED: &[u8] = br#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#;
    // BX11 (the alloc review's AL-1): events of no kind the arm acts on —
    // a strategy's update, and a kind newer than this build — scanned and
    // counted.
    const STRATEGY: &[u8] = br#"{"e":"STRATEGY_UPDATE","T":1790000000126,"E":1790000000126,"su":{"si":8,"st":"GRID","ss":"NEW","s":"BTCUSDT","ut":1790000000126}}"#;
    const NEWER: &[u8] = br#"{"e":"AN_EVENT_OF_2027","E":1790000000127}"#;

    /// The send window, as `WsConn`'s: one preallocated buffer, each frame
    /// written and masked by core-net's own in-place writer.
    struct Window {
        buf: [u8; 4_096],
        n: usize,
        mask: u64,
    }
    impl FrameSink for Window {
        fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), core_net::WsErr> {
            self.mask = self.mask.wrapping_add(1);
            let m = core_net::ws_mask_from_counter(self.mask);
            self.n = core_net::ws_write_text_frame_with(&mut self.buf, parts, m).map_err(|_| core_net::WsErr::Overflow)?;
            Ok(())
        }
    }

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 5_000);
    let signer = signer_ed25519::Ed25519Signer::from_seed(&[0x5a; 32]).expect("gate 93 signer");
    let mut disc = ingress_binance::discovery::BnDiscovery::new();
    disc.ingest_body(BX6_FAPI).expect("gate 93 discovery");
    let (mut table, mut wire) = InstTable::new(7);
    for (sym, name) in [(7, &b"BTCUSDT"[..]), (core_types::make_symbol_id(VenueId::Binance, 514), &b"ETHUSDT"[..])] {
        let row = disc.find(name).expect("gate 93 row");
        table
            .bind(&mut wire, &BindSpec { sym, product: PRODUCT_USDM, row, owned: true, maker_ok: true })
            .expect("gate 93 bind");
    }
    let mut win = Window { buf: [0; 4_096], n: 0, mask: 0x6512 };
    let mut rest_win = [0u8; 1_024];
    let mut oot = Oot::new();
    let mut tq: exec_gateway::TimerQueue<{ exec_binance::gateway::TIMER_CAP }> = exec_gateway::TimerQueue::new();
    let mut pend: exec_gateway::Pending<u64, { exec_binance::gateway::WS_IDS }> = exec_gateway::Pending::new(0);
    let mut due = exec_gateway::Expired { deadline_ns: 0, key: 0, kind: 0 };
    let mut tids = TidRing::<256>::new();
    let (j_p, mut j_c) = Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
    let mut journal = JournalTx::new(j_p);
    // BX11 (the alloc review's AL-2): S8 — a journal two records deep, so
    // the E7 anchor's note is refused, offered again and lands under the
    // guard.
    let (s8_p, mut s8_c) = Ring::<core_types::ExecRecord, 2>::new().split();
    let mut s8 = JournalTx::<2>::new(s8_p);
    let filler = core_types::ExecRecord { kind: J_FEE, ..core_types::ExecRecord::default() };
    let anchor = core_types::ExecRecord { kind: J_ANCHOR, ..core_types::ExecRecord::default() };
    let mut ended = EndedRing::new();
    let mut clock = VenueClock::new(core_time::WallAnchor::now());
    let mut orders = [OrderRow::default(); 16];
    let mut trades = [TradeRow::default(); 16];
    let mut pos = [PosRow::default(); 8];
    let mut legs = [Leg::default(); 2];
    legs[0].owned = true;
    let mut a = WsAnswer::default();
    let mut ev = UserEvent::default();

    let g = AllocGuard::new();
    let (mut rendered, mut expired, mut fees) = (0usize, 0u64, 0u64);
    let (mut others, mut anchored) = (0u64, 0u64);
    let mut i = 0u64;
    while i < N {
        let now = core_time::now_ns();
        let ts = 1_790_000_000_000 + i;
        let side = (i & 1) as u8;
        // The client ids the venue reports, classified.
        std::hint::black_box(prefix.classify(b"mv6512ab0f2000000000000006500000"));
        std::hint::black_box(prefix.classify(b"web_stranger_1"));
        // The order session's requests, the client id rendered in place.
        let px = 65_000_000_000 + (i % 100) as i64 * 100_000;
        let cid = Part::Cid(&prefix, 2, 0x65 + i);
        let mut o = Place {
            symbol: wire.row(0).symbol(),
            side,
            maker: i & 2 == 0,
            qty_1e6: 2_000,
            qty_dec: 3,
            px_1e6: px,
            px_dec: 1,
            cid,
            ts_ms: ts,
            reduce_only: i & 4 == 0,
        };
        queue_place(&mut win, 17 + i, &st, &o).expect("gate 93 place");
        rendered += win.n;
        queue_cancel(&mut win, 18 + i, &st, b"BTCUSDT", cid, ts).expect("gate 93 cancel");
        o.qty_1e6 = 3_000;
        queue_modify(&mut win, 19 + i, &st, &o).expect("gate 93 modify");
        queue_status(&mut win, 20 + i, &st, b"BTCUSDT", cid, ts).expect("gate 93 status");
        // A ghost's cancel, by the venue's own id.
        queue_cancel(&mut win, 21 + i, &st, b"BTCUSDT", Part::Lit(b"mv65110000200000000000000004d00000"), ts)
            .expect("gate 93 ghost cancel");
        queue_account(&mut win, 22 + i, &st, exec_binance::inst::PRODUCT_USDM, ts).expect("gate 93 account");
        rendered += win.n;
        // Their answers and the user stream.
        scan_answer(PLACED, &mut a).expect("gate 93 answer");
        std::hint::black_box(a.venue_oid);
        scan_answer(REFUSED, &mut a).expect("gate 93 refusal");
        std::hint::black_box(a.code);
        // A trade's two reports: TRADE_LITE books it, the update owes its
        // commission (BX11) — noted, then drained as the writer would.
        let tid = 9_000 + (i & 511);
        scan_user_event(LITE, &mut ev).expect("gate 93 lite");
        assert!(!tids.see(0, tid, false).again, "gate 93: a fresh trade");
        scan_user_event(UPDATE, &mut ev).expect("gate 93 update");
        std::hint::black_box(prefix.classify(ev.cid.get(UPDATE)));
        let row = wire.find(PRODUCT_USDM, ev.symbol.get(UPDATE));
        let w = wire.row(row);
        let asset = fee_asset(w, ev.fee_asset.get(UPDATE));
        if tids.see(row, tid, true).fee_due {
            let fee = core_types::ExecRecord {
                ts_ns: now,
                wall_ms: ts,
                client_oid: 0x65 + i,
                venue_oid: ev.venue_oid,
                a: ev.last_px_1e6,
                b: fee_notional_1e9(w, ev.last_px_1e6, ev.last_qty_1e6),
                c: ev.commission_1e9,
                code: fee_code(ev.maker != 0, asset, TRADE_BY_MEMBER, w),
                row,
                kind: J_FEE,
                slot: 2,
            };
            assert!(journal.note(&fee), "gate 93: the journal's ring has room");
            fees += u64::from(asset == FEE_ASSET_SETTLE);
        }
        while let Some(r) = j_c.try_pop_ref() {
            std::hint::black_box((r.b, r.c));
        }
        // BX7 (BX-19): a position update's margin types — every isolated
        // position looked up, in place, in one pass.
        scan_user_event(ACCOUNT_UPDATE, &mut ev).expect("gate 93 account update");
        let mut walk = exec_binance::userstream::NotCross::new(ACCOUNT_UPDATE, &ev);
        let mut named = 0u8;
        while let Some(s) = walk.next() {
            std::hint::black_box(wire.find(PRODUCT_USDM, s.get(ACCOUNT_UPDATE)));
            named += 1;
        }
        assert_eq!(named, ev.isolated, "gate 93: the walk names what the scan counted");
        scan_user_event(CONFIG_UPDATE, &mut ev).expect("gate 93 config update");
        std::hint::black_box((ev.config, wire.find(PRODUCT_USDM, ev.symbol.get(CONFIG_UPDATE))));
        // BX11: a kind the arm does not act on scans and is counted, as the
        // gateway's tally counts it.
        for f in [STRATEGY, NEWER] {
            scan_user_event(f, &mut ev).expect("gate 93 another kind");
            others += u64::from(ev.kind == UE_OTHER);
        }
        // BX11: S8 — the anchor on a full ring: refused and counted once,
        // offered again (not counted), landing once the writer made room.
        if i % 16 == 0 {
            let mut k = 0;
            while k < 8 && s8.offer(&filler) {
                k += 1;
            }
            assert!(!s8.note(&anchor) && !s8.offer(&anchor), "gate 93: the anchor waits on a full ring");
            std::hint::black_box(s8_c.try_pop_ref().is_some());
            anchored += u64::from(s8.offer(&anchor));
            while s8_c.try_pop_ref().is_some() {}
        }
        // The open-order table, the request's id and deadline, the TTL.
        let ix = oot
            .insert(2, 1_000 + i, 0, side, (i & 1) as u8, px, 2_000, now)
            .expect("gate 93 insert");
        let (id, rec) = pend.claim_next(now).expect("gate 93 id");
        *rec = u64::from(ix);
        let deadline = tq.arm(now + 5_000_000_000, id, 2).expect("gate 93 deadline");
        let ttl = tq.arm(now + 5_000_000_000, u64::from(ix), 1).expect("gate 93 ttl");
        assert!(oot.add_pending(ix, 2_000_000 + i, px, 3_000, now));
        oot.confirm_rename(ix);
        std::hint::black_box(oot.by_current(2, 2_000_000 + i));
        assert_eq!(pend.get(id), Some(&u64::from(ix)));
        assert!(tq.cancel(deadline) && pend.release(id), "the answer");
        assert!(tq.cancel(ttl));
        if i % 16 == 0 {
            tq.arm(now, u64::from(ix), 1).expect("gate 93 ttl due");
            while tq.pop_due(now + 8_000_000, &mut due) {
                expired += u64::from(due.key == u64::from(ix));
            }
        }
        // S7: the place path's question, then the end and a late trade.
        std::hint::black_box(ended.holds(2, 1_000 + i));
        ended.record(2, 1_000 + i, now, 2_000, 2_000 * px as i128, i & 8 == 0);
        if let Some(k) = ended.find(2, 1_000 + i) {
            ended.add_filled(k, 1, px as i128);
            ended.set_status_booked(k);
            std::hint::black_box((ended.filled_1e6(k), ended.status_booked(k)));
            // S1: a quantity booked without its trades, at the implied average.
            std::hint::black_box(exec_binance::gateway::implied_px_1e6(2_002, px, ended.quote_1e12(k), 1));
        }
        oot.remove(ix);
        // The clock: its answer read (BX7b: whole, the key once), then
        // sampled.
        std::hint::black_box(scan_server_time(TIME).expect("gate 93 time"));
        std::hint::black_box(clock.sample(now, now + 40_000, 1_790_000_000_000 + i));
        std::hint::black_box(clock.venue_ms(now));
        // The dead-man's signed query, in its window.
        let mut q = QueryWriter::new(&mut rest_win);
        q.put(b"symbol=").put(b"BTCUSDT").put(b"&countdownTime=").uint(30_000);
        q.put(b"&recvWindow=").uint(5_000).put(b"&timestamp=").uint(ts);
        rendered += q.sign(&signer).expect("gate 93 sign");
        std::hint::black_box(scan_countdown(COUNTDOWN).is_ok());
        std::hint::black_box(scan_error(REST_REFUSED));
        // The reconciliation, at its own cadence.
        if i % 64 == 0 {
            let n = scan_orders(OPEN, &mut orders).expect("gate 93 open orders");
            std::hint::black_box(prefix.classify(orders[n - 1].cid.get(OPEN)));
            let n = scan_trades(TRADES, &mut trades).expect("gate 93 trades");
            std::hint::black_box(day_increasing_1e6(&mut trades[..n], 0, false, 0));
            scan_answer(ACCOUNT, &mut a).expect("gate 93 account answer");
            let snap = scan_um_account(ACCOUNT, a.result, &mut pos).expect("gate 93 account");
            legs[0].venue_1e6 = pos[0].amt_1e6;
            legs[0].booked_1e6 = snap.n_pos as i64 * 3_000;
            let mut v = Verdict::default();
            judge_positions(&mut legs, &mut v);
            std::hint::black_box(v.reconciled());
            std::hint::black_box(ended.ended_since(2, 1_000 + i, now));
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(rendered > 0 && expired == N.div_ceil(16), "{rendered} B rendered, {expired} expired");
    assert!(oot.is_empty() && pend.live() == 0 && tq.is_empty());
    assert_eq!(fees, N, "gate 93: every trade's commission noted once, in the row's quote");
    assert_eq!(others, 2 * N, "gate 93: every event of another kind scanned as one");
    assert_eq!((anchored, s8.dropped()), (N.div_ceil(16), N.div_ceil(16)), "gate 93: each anchor refused once, then landed");
    std::hint::black_box(&table);
    assert_eq!(allocs, 0, "gate 93: the gateway's render and scan allocated {allocs} times ({bytes} B)");
}

/// Gate 94's venue half: runs ONLY in the child process gate 94 (and gate
/// 108) spawns (it is `#[ignore]`d, and a no-op without
/// `GATE94_VENUE_DIR`): the scripted Binance (`exec_binance::testvenue`) —
/// with `GATE94_REST=1` holding the account's HMAC secret, for gate 94b's
/// REST route; with `GATE94_OPT=1` (BX9, gate 110) no options
/// `ACCOUNT_UPDATE`; with `GATE94_SPOT=1` (BX8, gate 108) its spot account
/// holding 1 BNB and paying every trade's commission in it (the burn: the
/// BTC a round buys is sold back whole, so no balance drifts and no SELL
/// is refused), and an IoC of exactly 0.003 filling half
/// (`spot_halve_qty_1e6`: the partial window's) — writing its port and
/// certificate for the parent. Then it serves until killed, and switches
/// the spot sessions to a write a frame (`spot_frame_per_write`) once the
/// parent leaves a `spot-frames` file, acknowledging with `spot-frames-on`
/// (gate 108's per-frame window: this process's venue is out of the
/// parent's reach otherwise).
#[test]
#[ignore = "gate 94's child process; never run on its own"]
fn gate94_venue_helper() {
    use exec_binance::testvenue::{Venue, HMAC_SECRET};
    use std::time::{Duration, Instant};
    let Some(dir) = std::env::var_os("GATE94_VENUE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let v = Venue::start();
    if std::env::var_os("GATE94_REST").is_some_and(|x| x == "1") {
        v.with(|s| s.hmac = Some(HMAC_SECRET.to_vec()));
    }
    if std::env::var_os("GATE94_OPT").is_some_and(|x| x == "1") {
        // BX9, gate 110: no options `ACCOUNT_UPDATE` (its paced margin
        // re-read would land in a window at its own time).
        v.with(|s| s.opt_account_events = false);
    }
    if std::env::var_os("GATE94_EQ").is_some_and(|x| x == "1") {
        // BX10, gate 112: every equity place at or over 300 USD fills
        // whole at its limit; one under it rests.
        v.with(|s| s.eq_fill_from_1e6 = Some(300_000_000));
    }
    if std::env::var_os("GATE94_SPOT").is_some_and(|x| x == "1") {
        v.with(|s| {
            s.spot_bal_1e8.insert(String::from("BNB"), 100_000_000);
            s.spot_bnb_fee_1e8 = Some(1_000);
            s.spot_halve_qty_1e6 = 3_000;
        });
    }
    std::fs::write(dir.join("cert.der"), &v.cert_der).expect("gate 94 cert");
    // The port last: its presence says the rest is written.
    std::fs::write(dir.join("port.tmp"), v.port.to_string()).expect("gate 94 port");
    std::fs::rename(dir.join("port.tmp"), dir.join("port")).expect("gate 94 port");
    let asked = dir.join("spot-frames");
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(120) {
        if asked.exists() {
            v.with(|s| s.spot_frame_per_write = true);
            std::fs::remove_file(&asked).expect("gate 108 ask");
            std::fs::write(dir.join("spot-frames-on"), b"1").expect("gate 108 ack");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// **BX12 gate 94 — the Binance gateway's whole step, over a real rustls
/// loopback, allocates only rustls' record buffers** (the live-socket
/// gate: an arming precondition, runbook §8).
///
/// The REAL gateway (`BnGateway::step`, booted as the engine boots it)
/// against the scripted Binance
/// (`exec_binance::testvenue`) in a CHILD process — the counting
/// allocator is process-global (gate 72's harness, gate 96b's shape) —
/// the engine end driven raw (the arm on the engine thread is gate 92's).
/// The knob timers — the dead-man's heartbeat and countdown, the
/// reconciliation cadence, the session's rotation — are set past the
/// gate's run; the 100 ms pulse runs inside every window (0 B, pinned with
/// it); the clock's 60 s round is a constant the gate's seconds-long run
/// stays clear of (asserted before each window). One maker rests
/// throughout, so no round is a first maker that arms the dead-man: the
/// first maker's countdown, the clock's first round and the countdown
/// probe all land in the warm-up, and the rounds' window asserts that no
/// REST job (a clock round, a countdown, a cycle) was answered in it.
/// Every round: a post-only maker and a
/// crossing IoC — selling, then buying, round by round: short, then flat,
/// so the booked position changes sign every round (`EVT_POSITION`,
/// counted) — pushed together:
/// one `order.place` each, written in ONE record; the venue's two answers
/// and five user-stream frames (the maker's NEW; the IoC's NEW, its
/// `TRADE_LITE`, the update's TRADE and that update again), the IoC booked
/// once from the lite (lane 4, `J_FILL`), its commission journaled from
/// the update (`J_FEE`, BX11), retired FILLED; then the maker's cancel,
/// its answer and the stream's CANCELED. Pinned like gate 96b: rustls 0.23's buffered API allocates
/// one `Vec` per record it seals and one per application-data record it
/// opens — exactly [`G94_WS_ROUND`] a round (two sealed, nine opened)
/// while every inbound burst stays under its 4 KiB receive window (the
/// largest here, about 2.3 KB); its decrypted-record queue grows once per
/// connection, which the stalled first round forces (on 94b, one request
/// in flight, the warm-up's rounds grow it). Everything else — the command
/// burst, the desk, the deadlines, the render (on the WS API the frames
/// ride the session's `session.logon`, no signature an order; 94b signs
/// each request with HMAC), the frame, the scans, the dedupe ring, the
/// fee, the journal, lane 4, the events — is 0 B: a regression shows as
/// one more. Then, measured alone (gate 94 only): a maker
/// with a 50 ms TTL and the gateway's own TTL cancel (the deadline queue,
/// rendered and written in the step's second flush) — [`G94_TTL`], no
/// REST job answered; and
/// the halt's sweep (the stats word, never a command): the resting maker
/// cancelled, then — the order route being up — the whole reconciliation
/// cycle confirms it (`v2/account.status` over the WS API, the REST
/// listing), until the word reads clear — [`G94_SWEEP`], one REST job
/// answered: the cycle's own cost on the socket, four records. A
/// break-and-watch (an 8-byte `Vec` in `on_place`) failed the rounds at 13
/// and the TTL at 7.
#[test]
fn binance_gateway_round_trip_allocates_only_rustls_record_buffers() {
    gate94(false);
}

/// **BX12 gate 94b — the REST order route (HMAC), the same rounds.** Every
/// place and cancel is its own signed, body-form request on the route's
/// one keep-alive connection (one in flight): a record sealed and its
/// answer opened each, beside the same six user-stream frames —
/// [`G94_REST_ROUND`] a round; the HMAC padded in the request's window
/// (gate 104's render and signature, here on the socket).
#[test]
fn binance_rest_route_round_trip_allocates_only_rustls_record_buffers() {
    gate94(true);
}

/// Gate 94's WS API round: two places in one record and the cancel sealed;
/// two place answers, five frames, the cancel's answer and CANCELED opened.
const G94_WS_ROUND: u64 = 11;
/// Gate 94b's REST round: three requests sealed and answered; six frames.
const G94_REST_ROUND: u64 = 12;
/// The TTL window: the place and the TTL cancel sealed; their answers, the
/// NEW and the CANCELED opened.
const G94_TTL: u64 = 6;
/// The sweep window: the resting maker's cancel sealed, its answer and
/// CANCELED opened; the confirmation — the whole reconciliation cycle, the
/// order route being up — `v2/account.status` and the REST listing sealed,
/// their answers opened.
const G94_SWEEP: u64 = 7;

/// What the engine end saw (gate 94).
#[derive(Clone, Copy, Default)]
struct Seen94 {
    /// The ACK of the order the round watches.
    watched_acked: bool,
    /// The order the round watches.
    watch: u64,
    filled: u64,
    canceled_member: u64,
    canceled_ttl: u64,
    retired_other: u64,
    rejects: u64,
    sweeps_done: u64,
    /// `EVT_POSITION`: the booked position changed sign.
    positions: u64,
    fills: u64,
    j_fill: u64,
    j_fee: u64,
}

/// Gate 94 on the WS API route (`rest` false, Ed25519) or the REST order
/// route (HMAC).
fn gate94(rest: bool) {
    use clob_dispatcher::{RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_FILLED};
    use core_config::SecretKeyBytes;
    use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
    use core_ring::{Consumer, Ring};
    use core_types::{ExecRecord, Fill, Side};
    use exec_binance::cmd::{
        duplex, BnCmd, BnEngineEnd, EVT_ACK, EVT_F_SWEEP_DONE, EVT_POSITION, EVT_REJECT, EVT_RETIRED, EVT_SWEEP,
    };
    use exec_binance::config::{BnConfig, HmacSigner, Scope};
    use exec_binance::gateway::{BnGateway, GwKnobs, Phase};
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_USDM};
    use exec_binance::journal::{JournalTx, JOURNAL_RING, J_FEE, J_FILL};
    use exec_binance::mode::AccountMode;
    use exec_binance::testvenue::{EPOCH, FAPI, HMAC_SECRET, KEY, SEED, SLOT};
    use exec_gateway::stats::SWEEP_CLEAR;
    use ingress_binance::discovery::BnDiscovery;
    use std::time::{Duration, Instant};

    const FILL_N: usize = 1_024;
    const ROUNDS: u64 = 300;
    const WARM: u64 = 50;
    /// Every timer the gate does not measure, set past its run: no
    /// reconciliation cycle, no dead-man renewal, no half-open probe.
    const LONG_MS: u64 = 3_600_000;
    /// BTCUSDT on the venue's grid (tick 0.10, step 0.001), ×1e6: the
    /// resting maker, the round's maker and the TTL maker below the mark
    /// (65 000) inside the band; the IoCs across it, bought and sold; 0.002
    /// each (126 USDT and more: over the 100 USDT minimum).
    const PX_REST: i64 = 63_000_000_000;
    const PX_MAKER: i64 = 64_000_000_000;
    const PX_TTL: i64 = 63_500_000_000;
    const PX_IOC_BUY: i64 = 65_100_000_000;
    const PX_IOC_SELL: i64 = 64_900_000_000;
    /// The clock's round comes 60 s after the first Ready pass (a
    /// constant): every window must start well before it.
    const CLOCK_CLEAR: Duration = Duration::from_secs(50);
    const QTY: i64 = 2_000;

    type Gw = BnGateway<FILL_N>;

    /// One step, then everything the engine end, lane 4 and the journal
    /// hold, tallied in place.
    fn pump(
        gw: &mut Gw,
        eng: &mut BnEngineEnd,
        lane: &mut Consumer<Fill, FILL_N>,
        journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
        s: &mut Seen94,
    ) {
        gw.step();
        while let Some(g) = eng.evt.pop() {
            let (kind, oid, why, flags) = (g.kind, g.client_oid, g.why, g.flags);
            drop(g);
            match kind {
                EVT_ACK => s.watched_acked |= oid == s.watch,
                EVT_RETIRED => match why {
                    RETIRED_FILLED => s.filled += 1,
                    RETIRED_CANCELED_MEMBER => s.canceled_member += 1,
                    RETIRED_CANCELED_TTL => s.canceled_ttl += 1,
                    _ => s.retired_other += 1,
                },
                EVT_REJECT => s.rejects += 1,
                EVT_SWEEP => s.sweeps_done += u64::from(flags & EVT_F_SWEEP_DONE != 0),
                EVT_POSITION => s.positions += 1,
                _ => {}
            }
        }
        while let Some(g) = lane.try_pop_ref() {
            drop(g);
            s.fills += 1;
        }
        while let Some(g) = journal.try_pop_ref() {
            s.j_fill += u64::from(g.kind == J_FILL);
            s.j_fee += u64::from(g.kind == J_FEE);
            drop(g);
        }
    }

    // ---- the venue, in its own process ----
    let dir = std::env::temp_dir().join(format!("mv-gate94-{}-{}", std::process::id(), u8::from(rest)));
    std::fs::create_dir_all(&dir).expect("gate 94 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 94 exe"))
        .args(["gate94_venue_helper", "--exact", "--ignored", "--test-threads=1"])
        .env("GATE94_VENUE_DIR", &dir)
        .env("GATE94_REST", if rest { "1" } else { "0" })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 94 child");
    let deadline = Instant::now() + Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 94 port");
        }
        assert!(Instant::now() < deadline, "gate 94: the venue never came up");
        std::thread::sleep(Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 94 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der)).expect("gate 94 anchor");
    let tls = std::sync::Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());

    // ---- the gateway, booted as the engine boots it ----
    let cfg = if rest {
        let h = HmacSigner::from_secret(HMAC_SECRET).expect("gate 94 secret");
        BnConfig::loopback_hmac(Scope::Live, String::from(KEY), h, port).expect("gate 94 config")
    } else {
        let seed = SecretKeyBytes::new_locked(SEED).expect("gate 94 seed");
        BnConfig::loopback(Scope::Live, String::from(KEY), seed, port).expect("gate 94 config")
    };
    let mut disc = BnDiscovery::new();
    disc.ingest_body(FAPI).expect("gate 94 discovery");
    let (mut table, mut wire) = InstTable::new(core_config::universe::LEGACY_BN_ANCHOR_SYM);
    let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
    table
        .bind(
            &mut wire,
            &BindSpec { sym: core_config::universe::LEGACY_BN_ANCHOR_SYM, product: PRODUCT_USDM, row, owned: true, maker_ok: true },
        )
        .expect("gate 94 bind");
    let (mut eng, gw_end) = duplex();
    let (fill_tx, mut lane) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, mut journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: AccountMode::Classic,
        recv_window_ms: 5_000,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 2 * LONG_MS,
        heartbeat_ms: LONG_MS,
        recon_every_ms: LONG_MS,
        spin: false,
        rotate_after_ns: LONG_MS * 1_000_000,
        rest_orders: rest,
        pnl_anchor_1e6: 0,
        boot_timeout_ns: 20_000_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: 0,
        equity: None,
    };
    let mut gw = Gw::new(&cfg, tls, knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx)).expect("gate 94 gateway");
    gw.boot().expect("gate 94 boot");
    assert_eq!(gw.phase(), Phase::Ready);
    let ready_at = Instant::now();
    let mut s = Seen94::default();
    let place = |oid: u64, px: i64, ttl_ns: u64, kind: u8, side: Side| {
        BnCmd::place(oid, px, QTY, ttl_ns, 0, kind, side, SLOT, 0).at(core_time::now_ns())
    };
    // The order verbs' answers, on either route (the REST jobs' are
    // `rest_rtt`'s).
    let answers = |eng: &BnEngineEnd| eng.stats.io()[1];

    // The maker that rests throughout: the row's first, so its countdown
    // (and the clock's first round, and the countdown probe) are the
    // warm-up's.
    s.watch = 900_001;
    assert!(eng.cmd.push(&place(900_001, PX_REST, 0, ORDER_KIND_MAKER, Side::Bid)));
    let t = Instant::now();
    while !s.watched_acked {
        assert!(t.elapsed() < Duration::from_secs(10), "gate 94: the resting maker was never acknowledged");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    // Until no REST answer for 300 ms: the clock's first round, the
    // countdown the maker armed, the probe.
    let (mut seen, mut quiet) = (eng.stats.rest_rtt.count(), Instant::now());
    while quiet.elapsed() < Duration::from_millis(300) {
        assert!(t.elapsed() < Duration::from_secs(10), "gate 94: the boot's REST never settled");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        if eng.stats.rest_rtt.count() != seen {
            (seen, quiet) = (eng.stats.rest_rtt.count(), Instant::now());
        }
    }

    // One round: a maker and an IoC in one burst; then the maker's cancel.
    let mut oid = 1_000_000u64;
    let mut round = |gw: &mut Gw,
                     eng: &mut BnEngineEnd,
                     lane: &mut Consumer<Fill, FILL_N>,
                     journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
                     s: &mut Seen94,
                     stall: bool| {
        oid += 2;
        let (maker, ioc) = (oid, oid + 1);
        let (a0, f0, l0, c0, fee0) = (answers(eng), s.filled, s.fills, s.canceled_member, s.j_fee);
        let sent0 = eng.stats.io()[0];
        s.watch = maker;
        s.watched_acked = false;
        // The IoC sells, then buys, round by round (AL-6): short, then flat
        // — the position changes sign every round.
        let (side, px_ioc) = if (oid / 2) % 2 == 0 { (Side::Bid, PX_IOC_BUY) } else { (Side::Ask, PX_IOC_SELL) };
        assert!(eng.cmd.push(&place(maker, PX_MAKER, 0, ORDER_KIND_MAKER, Side::Bid)));
        assert!(eng.cmd.push(&place(ioc, px_ioc, 0, ORDER_KIND_IOC, side)));
        let t = Instant::now();
        if stall {
            // Write both places, then let every answer and frame arrive
            // before the next read: rustls' record queues reach their
            // high-water marks here, in the warm-up.
            while eng.stats.io()[0] < sent0 + 2 {
                assert!(t.elapsed() < Duration::from_secs(5), "gate 94: the places were not written");
                pump(gw, eng, lane, journal, s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        while !(answers(eng) >= a0 + 2 && s.watched_acked && s.filled > f0 && s.fills > l0 && s.j_fee > fee0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 94: a round's places stalled");
            pump(gw, eng, lane, journal, s);
        }
        let a1 = answers(eng);
        assert!(eng.cmd.push(&BnCmd::cancel(maker, 0, SLOT).at(core_time::now_ns())));
        while !(answers(eng) > a1 && s.canceled_member > c0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 94: a round's cancel stalled");
            pump(gw, eng, lane, journal, s);
        }
    };
    let mut w = 0u64;
    while w < WARM {
        round(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, w == 0);
        w += 1;
    }

    // ---- the rounds ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 94: the run neared the clock's 60 s round");
    let warm = s;
    let (rest0, reconnects0) = (eng.stats.rest_rtt.count(), eng.stats.io()[5]);
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        round(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, false);
        r += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let rest_in_window = eng.stats.rest_rtt.count() - rest0;
    let reconnects = eng.stats.io()[5] - reconnects0;
    let rounds = s;

    // ---- the TTL, measured alone ----
    let (ttl_allocs, ttl_bytes, ttl_seen) = if rest {
        (0, 0, (0, 0, 0, 0, 0, 0))
    } else {
        assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 94: the run neared the clock's 60 s round");
        let (t0, a0) = (s.canceled_ttl, answers(&eng));
        let (r0, c0, j0, f0, p0) = (eng.stats.rest_rtt.count(), eng.stats.io()[5], s.rejects, s.fills, s.positions);
        s.watch = 950_001;
        s.watched_acked = false;
        let g = AllocGuard::new();
        assert!(eng.cmd.push(&place(950_001, PX_TTL, core_time::now_ns() + 50_000_000, ORDER_KIND_MAKER, Side::Bid)));
        let t = Instant::now();
        // Its place's answer, the TTL cancel's answer and its CANCELED.
        while !(s.watched_acked && s.canceled_ttl > t0 && answers(&eng) >= a0 + 2) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 94: the TTL never fired");
            pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        }
        let (a, b, _) = g.delta();
        // (REST jobs, order-session re-opens, TTL cancels, rejects, fills,
        // position events) in the window.
        let seen = (
            eng.stats.rest_rtt.count() - r0,
            eng.stats.io()[5] - c0,
            s.canceled_ttl - t0,
            s.rejects - j0,
            s.fills - f0,
            s.positions - p0,
        );
        (a, b, seen)
    };

    // ---- the halt's sweep, measured alone ----
    let (sweep_allocs, sweep_bytes, sweep_seen) = if rest {
        (0, 0, (0, 0, 0, 0))
    } else {
        assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 94: the run neared the clock's 60 s round");
        let (d0, m0, o0, a0) = (s.sweeps_done, s.canceled_member, s.retired_other, answers(&eng));
        let r0 = eng.stats.rest_rtt.count();
        let g = AllocGuard::new();
        let swept = eng.stats.request_sweep();
        let t = Instant::now();
        // Clear, its event, the resting maker cancelled and its cancel answered.
        while !(eng.stats.sweep() == (swept, SWEEP_CLEAR) && s.sweeps_done > d0 && s.canceled_member > m0 && answers(&eng) > a0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 94: the sweep never cleared");
            pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        }
        let (a, b, _) = g.delta();
        // (REST jobs, members cancelled, others retired, sweeps done). The
        // sweep's own cancel retires as a member's (`cancel_why` has no
        // sweep bucket): a dedicated reason would change this pin.
        let seen = (eng.stats.rest_rtt.count() - r0, s.canceled_member - m0, s.retired_other - o0, s.sweeps_done - d0);
        (a, b, seen)
    };
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();

    let route = if rest { "94b" } else { "94" };
    let per_round = if rest { G94_REST_ROUND } else { G94_WS_ROUND };
    assert_eq!(
        (rounds.filled - warm.filled, rounds.fills - warm.fills, rounds.j_fill - warm.j_fill, rounds.j_fee - warm.j_fee),
        (ROUNDS, ROUNDS, ROUNDS, ROUNDS),
        "gate {route}: every round's IoC filled, booked once on lane 4, journaled once with its fee"
    );
    assert_eq!(rounds.canceled_member - warm.canceled_member, ROUNDS, "gate {route}: every round's maker cancelled");
    assert_eq!((rounds.rejects, rounds.retired_other), (0, 0), "gate {route}: nothing refused, nothing else retired");
    assert_eq!(rounds.positions - warm.positions, ROUNDS, "gate {route}: the position changed sign every round");
    assert_eq!(reconnects, 0, "gate {route}: no order-session re-open in the window");
    assert_eq!(rest_in_window, 0, "gate {route}: no REST job (a clock round, a countdown, a cycle) in the window");
    if !rest {
        assert_eq!(
            ttl_seen,
            (0, 0, 1, 0, 0, 0),
            "gate 94: the TTL window — no REST job, no order-session re-open, one TTL cancel, nothing refused, filled or moved"
        );
        assert_eq!(sweep_seen, (1, 1, 0, 1), "gate 94: the sweep window — the cycle's one listing, the maker cancelled, nothing else retired, one sweep");
    }
    assert_eq!(
        (allocs, ttl_allocs, sweep_allocs),
        (per_round * ROUNDS, if rest { 0 } else { G94_TTL }, if rest { 0 } else { G94_SWEEP }),
        "gate {route}: the rounds, the TTL and the sweep allocated {allocs} ({bytes} B), {ttl_allocs} ({ttl_bytes} B) and {sweep_allocs} ({sweep_bytes} B) — rustls' records only"
    );
}

/// **XMM XH4a gate 95 — the order-gateway kit is 0 B/op.** Built once
/// (both rings, the stats block, the in-flight table, the deadline queue,
/// the tx buffer — boot). Under the guard:
///
/// * **A — 10 000 turns of a gateway loop at load:** the engine pushes
///   three 64 B commands a turn; the gateway pops them into one post
///   claimed in place, arms a 30 ms TTL per command, frames a 3 000 B
///   payload in place (the 16-bit length form of a signed post) and arms
///   the post's 5 s timeout; each post's reply comes 200 turns later — so
///   200 posts stay in flight — read back through `sent_ns` / `get_mut`
///   / `get`, one event per item, the timeout cancelled; the TTLs fire as
///   they come due; a make-before-break swap every 1 000 turns; the wait
///   policy, every latency tracker and the gauges every turn; the engine
///   drains its events in place.
/// * **B — every refusal and cold path:** the command ring pushed until
///   it refuses; the deadline queue armed until `TimerFull`, then
///   cleared; the in-flight table walked with `id_at` and emptied; a
///   connection death and its re-dial; a replacement whose drain outlives
///   its budget and is retired; a quiet stretch in which the wait policy
///   parks under the next deadline; and (release builds, where it is not
///   a panic) a side duplex's event ring overflowed: counted and halted.
#[test]
fn exec_gateway_kit_is_zero_alloc() {
    use core_net::{ws_frame_in_place, Backoff, WsOpcode, WS_CLIENT_HDR_MAX};
    use exec_gateway::stats::{HALT_EVENT_RING_FULL, HALT_NONE};
    use exec_gateway::{
        duplex, Expired, Idle, Mbb, MbbAction, Pending, TimerFull, TimerId, TimerQueue,
    };

    #[derive(Copy, Clone)]
    #[repr(C, align(64))]
    struct Msg {
        oid: u64,
        t_ns: u64,
        _pad: [u8; 48],
    }
    #[derive(Copy, Clone)]
    struct Post {
        n: usize,
        oids: [u64; 39],
    }
    const MS: u64 = 1_000_000;
    const TIMEOUT: u8 = 1;
    const TTL: u8 = 2;
    const IN_FLIGHT: u64 = 200;
    let msg = |oid: u64, t_ns: u64| Msg {
        oid,
        t_ns,
        _pad: [0; 48],
    };

    let (mut eng, mut gw) = duplex::<Msg, Msg, 1024, 4096>();
    let (_side_eng, mut side_gw) = duplex::<Msg, Msg, 2, 8>();
    let mut pending: Pending<Post, 256> = Pending::new(Post {
        n: 0,
        oids: [0; 39],
    });
    let mut timers: TimerQueue<1024> = TimerQueue::new();
    let mut tids: [Option<TimerId>; 256] = [None; 256];
    let mut mbb = Mbb::new(
        Backoff::new(500 * MS, 8_000 * MS, 7),
        30_000 * MS,
        10_000 * MS,
        5_000 * MS,
    );
    let mut idle = Idle::default();
    let mut tx = vec![0u8; WS_CLIENT_HDR_MAX + 4_096];
    let body = [b'x'; 3_000];
    let stats = std::sync::Arc::clone(&gw.stats);
    if let MbbAction::Dial(s) = mbb.poll(0) {
        mbb.on_live(s, 0);
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut oid = 1u64;
    let mut post_id = 1u64;
    let mut expired = Expired {
        deadline_ns: 0,
        key: 0,
        kind: 0,
    };

    // ---- A: steady traffic, 200 posts in flight --------------------
    let mut turn = 0u64;
    while turn < 10_000 {
        let now = 1_000 * MS + turn * MS;
        let mut k = 0;
        while k < 3 {
            assert!(eng.cmd.push(&msg(oid, now)));
            oid += 1;
            k += 1;
        }
        let post = pending.claim(post_id, now).expect("gate 95 claim");
        post.n = 0;
        let mut worked = false;
        while let Some(c) = gw.cmd.pop() {
            post.oids[post.n] = c.oid;
            post.n += 1;
            stats.push_to_write.record(now.saturating_sub(c.t_ns) + 1);
            assert!(
                timers.arm(now + 30 * MS, c.oid, TTL).is_ok(),
                "gate 95: TTL queue full"
            );
            worked = true;
        }
        tx[WS_CLIENT_HDR_MAX..WS_CLIENT_HDR_MAX + body.len()].copy_from_slice(&body);
        let start = ws_frame_in_place(
            &mut tx,
            WS_CLIENT_HDR_MAX,
            WsOpcode::Text,
            body.len(),
            [1, 2, 3, 4],
        )
        .expect("gate 95 frame");
        acc = acc.wrapping_add(u64::from(tx[start]));
        stats.note_action_sent();
        tids[(post_id & 255) as usize] = Some(
            timers
                .arm(now + 5_000 * MS, post_id, TIMEOUT)
                .expect("gate 95 timeout"),
        );
        if post_id > IN_FLIGHT {
            let old = post_id - IN_FLIGHT;
            let sent = pending.sent_ns(old).expect("gate 95: in flight");
            if let Some(p) = pending.get_mut(old) {
                p.n = p.n.min(39);
            }
            let n = pending.get(old).map_or(0, |p| p.n);
            let mut i = 0;
            while i < n {
                let o = pending.get(old).map_or(0, |p| p.oids[i]);
                assert!(gw.evt.emit(&msg(o, now)));
                i += 1;
            }
            assert!(pending.release(old));
            let t = tids[(old & 255) as usize]
                .take()
                .expect("gate 95: its timeout");
            assert!(timers.cancel(t));
            stats.note_reply();
            stats.post_rtt.record(now - sent);
        }
        stats.cancel_rtt.record(200 * MS + turn);
        stats.fill_venue_to_local.record(120 * MS + turn);
        stats.rest_rtt.record(900 * MS + turn);
        post_id += 1;
        while timers.pop_due(now, &mut expired) {
            acc = acc.wrapping_add(expired.key ^ u64::from(expired.kind));
        }
        if turn % 1_000 == 999 {
            mbb.request_replace();
            if let MbbAction::Dial(s) = mbb.poll(now) {
                mbb.on_live(s, now);
                mbb.on_drained(s ^ 1, now);
            }
        }
        mbb.note_healthy(now);
        stats.set_gauges(pending.live() as u64, mbb.active().is_some());
        acc = acc.wrapping_add(idle.timeout_ns(worked, now, timers.next_deadline()));
        acc =
            acc.wrapping_add(gw.evt.room() as u64 + eng.cmd.queued() as u64 + gw.cmd.len() as u64);
        while let Some(ev) = eng.evt.pop() {
            acc = acc.wrapping_add(ev.oid);
        }
        turn += 1;
    }
    let live_after_a = pending.live();

    // ---- B: every refusal and cold path ----------------------------
    let t0 = 20_000 * MS;
    let mut refused = false;
    while !refused {
        refused = !eng.cmd.push(&msg(oid, t0));
        oid += 1;
    }
    while let Some(c) = gw.cmd.pop() {
        acc = acc.wrapping_add(c.oid);
    }
    while timers.arm(u64::MAX - 1, 7, TTL).is_ok() {
        acc = acc.wrapping_add(1);
    }
    assert_eq!(
        timers.arm(u64::MAX - 1, 7, TTL),
        Err(TimerFull),
        "full, and it says so"
    );
    timers.clear();
    let mut s = 0usize;
    while s < pending.capacity() {
        if let Some(id) = pending.id_at(s) {
            assert!(pending.release(id));
        }
        s += 1;
    }
    let a = mbb.active().expect("gate 95: a live connection");
    mbb.on_down(a, t0);
    match mbb.poll(t0 + 10_000 * MS) {
        MbbAction::Dial(s) => mbb.on_live(s, t0 + 10_000 * MS),
        other => panic!("gate 95: no re-dial ({other:?})"),
    }
    mbb.request_replace();
    let old = mbb.active().expect("gate 95: re-dialled");
    match mbb.poll(t0 + 11_000 * MS) {
        MbbAction::Dial(s) => mbb.on_live(s, t0 + 11_000 * MS),
        other => panic!("gate 95: no replacement dial ({other:?})"),
    }
    let retire = mbb.poll(t0 + 16_000 * MS);
    if retire == MbbAction::Retire(old) {
        mbb.on_down(old, t0 + 16_000 * MS);
    }
    let mut parked = 0u64;
    let mut k = 0;
    while k < 2_000 {
        if idle.timeout_ns(false, t0, Some(t0 + 50_000)) > 0 {
            parked += 1;
        }
        k += 1;
    }
    let side_halt = if cfg!(debug_assertions) {
        HALT_EVENT_RING_FULL // debug: a lost event is a panic (exec-gateway's own test pins both)
    } else {
        let mut k = 0u64;
        while k < 8 {
            assert!(side_gw.evt.emit(&msg(k, t0)));
            k += 1;
        }
        assert!(!side_gw.evt.emit(&msg(9, t0)));
        side_gw.stats.halt_reason()
    };

    // Every read before the measurement closes.
    let q50 = stats.post_rtt.quantile_ns(0.5);
    let ring = stats.gateway_ring();
    let cmds = stats.cmds();
    let io = stats.io();
    let timers_left = timers.len();
    let live_left = pending.live();
    let main_halt = stats.halt_reason();
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();

    assert!(acc != 0, "the gate must measure real work");
    assert_eq!(
        live_after_a, IN_FLIGHT as usize,
        "200 posts in flight through phase A"
    );
    assert_eq!(mbb.swaps(), 11, "ten swaps in A, one in B");
    assert_eq!(
        retire,
        MbbAction::Retire(old),
        "the drain outlived its budget"
    );
    assert_eq!(
        cmds,
        (30_000 + 1_024, 1),
        "the ring took 1 024 and refused the next"
    );
    assert_eq!(
        ring,
        (30_000 + 1_024, 0),
        "every command popped, no event lost"
    );
    assert_eq!(io[0], 10_000, "one action a turn");
    assert_eq!(
        io[1],
        10_000 - IN_FLIGHT,
        "a reply for every post older than 200 turns"
    );
    assert!(q50 >= 200 * MS, "post RTT = 200 turns of 1 ms");
    assert_eq!((timers_left, live_left), (0, 0));
    assert!(parked > 0, "a quiet gateway parks");
    assert_eq!(side_halt, HALT_EVENT_RING_FULL);
    assert_eq!(main_halt, HALT_NONE);
    assert_eq!(
        allocs, 0,
        "the gateway kit allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(
        bytes, 0,
        "the gateway kit bytes should be zero: saw {bytes}"
    );
}

/// **XMM gate 99 (XH4f) — slot 6 through the router is 0 B/op.** The
/// engine's shape for slot 6 (`RoutedDispatcher<PaperDispatcher,
/// SlotSplit<NullLiveDispatcher, HlGatewayArm>>`) over a scripted gateway end
/// in-process, driven as the engine drives it. Every round: a quote (bids and
/// asks by turn) placed, answered resting (the ledger's row); a modify asking
/// twice its size (the asked hold) — landed every other round (the router's
/// rename) or refused crossing (the hold dropped with the replacement's
/// retirement); every third round a fill, taken and booked back (a position);
/// a per-perp cap and a resting cap refusing (`CapPerp`, `MaxResting`); the
/// live order cancelled and released on its retirement; the member-facing
/// health read; the members' order events taken; every 500 rounds the
/// operator's report (the engine loop takes one a second), its latency
/// trackers holding samples. The arm sets its perp's leverage at boot, as in
/// production. Every 2 000 rounds a stale mark, which the router's sweep
/// expires: growth is refused while a position has no mark (XH2-C) until a
/// fresh one arrives. The window runs until each path has run in it.
#[test]
fn xmm_slot_six_through_the_router_is_zero_alloc() {
    use clob_dispatcher::{OrderDispatch, PaperDispatcher};
    use core_fill::ORDER_KIND_MAKER;
    use core_types::{
        CancelReq, ChannelEvent, ChannelId, ModifyReq, Order, Side, ORDER_EVENT_REASON_BAD_ALO_PX,
        ORDER_EVENT_REASON_CANCEL_REQUESTED,
    };
    use exec_gateway::stats::SWEEP_CLEAR;
    use exec_hyperliquid::arm::{ArmCfg, HlGatewayArm, PerpBind, ARM_PERPS};
    use exec_hyperliquid::cmd::{
        HlCmd, CMD_CANCEL, CMD_LEVERAGE, CMD_MODIFY, CMD_PLACE, CMD_READ_DAY, CMD_READ_LIMIT,
        CMD_READ_ORDERS, CMD_READ_STATE, CMD_SCHEDULE_CANCEL,
    };
    use exec_hyperliquid::deadman::DeadManCfg;
    use exec_hyperliquid::event::{
        ExecEvent, EVF_BUY, EVF_CMD_SHIFT, EVF_POST_ITEM, EVF_RESTING, EV_ACK, EV_CANCELED,
        EV_FILL, EV_READ_DONE, EV_REJECTED, EV_SNAPSHOT, READ_DAY, READ_LIMIT, READ_ORDERS,
        READ_STATE,
    };
    use exec_hyperliquid::gateway::HlGatewayEnd;
    use exec_hyperliquid::hl_duplex;
    use exec_hyperliquid::rules::PerpRules;
    use exec_router::{
        ExecMode, ExecRoute, HaltLimits, InstrumentSpec, NullLiveDispatcher, PerpLimits,
        RoutedDispatcher, SlotCaps, SlotSplit, LAW_LINEAR,
    };

    const SLOT: u8 = 6;
    const ASSET: u32 = 3;
    const E6: i64 = 1_000_000;
    const E8: i64 = 100_000_000;
    const ROUNDS: u64 = 20_000;

    /// The first command of `kind` in the ring; the arm's others (the dead
    /// man's, the reads') are left unanswered — in doubt, as a slow venue
    /// leaves them.
    fn take(end: &mut HlGatewayEnd, kind: u8) -> HlCmd {
        loop {
            match end.cmd.pop() {
                Some(c) if c.kind == kind => return *c,
                Some(_) => {}
                None => panic!("gate 99: the arm sent no command {kind}"),
            }
        }
    }
    /// The first commands of `a` and of `b`, in whichever order the arm
    /// pushed them.
    fn take_two(end: &mut HlGatewayEnd, a: u8, b: u8) -> (HlCmd, HlCmd) {
        let (mut x, mut y) = (None, None);
        while x.is_none() || y.is_none() {
            match end.cmd.pop() {
                Some(c) if c.kind == a && x.is_none() => x = Some(*c),
                Some(c) if c.kind == b && y.is_none() => y = Some(*c),
                Some(_) => {}
                None => panic!("gate 99: the arm sent no command {a} or {b}"),
            }
        }
        (x.expect("a"), y.expect("b"))
    }
    fn item(c: &HlCmd, kind: u8, reason: u8, flags: u8) -> ExecEvent {
        let bits = if c.kind <= CMD_CANCEL { c.kind } else { 0 };
        ExecEvent {
            client_oid: c.client_oid,
            asset: c.asset,
            kind,
            reason,
            slot: c.slot,
            flags: EVF_POST_ITEM | (bits << EVF_CMD_SHIFT) | flags,
            ..ExecEvent::default()
        }
    }
    fn read_end(c: &HlCmd, px_1e8: i64, aux: u64, venue_ms: u64) -> ExecEvent {
        let read = match c.kind {
            CMD_READ_ORDERS => READ_ORDERS,
            CMD_READ_LIMIT => READ_LIMIT,
            CMD_READ_DAY => READ_DAY,
            _ => READ_STATE,
        };
        ExecEvent {
            client_oid: c.client_oid,
            venue_ms,
            px_1e8,
            aux,
            kind: EV_READ_DONE,
            reason: read,
            slot: c.slot,
            ..ExecEvent::default()
        }
    }
    fn quote(cid: u64, sym: u32, side: Side, qty_1e6: i64) -> Order {
        let mut o = Order::new(
            core_time::now_ns(),
            core_types::VenueId::Hyperliquid,
            sym,
            side,
            ORDER_KIND_MAKER,
            core_types::Price::from_raw(100 * E6),
            core_types::Qty::from_raw(qty_1e6),
            cid,
        )
        .with_post_only();
        o.strategy_id = SLOT;
        o
    }
    fn mark(sym: u32, ts_ns: u64) -> ChannelEvent {
        ChannelEvent::new(
            ts_ns,
            core_types::VenueId::Hyperliquid,
            ChannelId::Mark,
            sym,
            0,
            0,
            100 * E6,
            100 * E6,
        )
    }

    let sym = core_types::make_symbol_id(core_types::VenueId::Hyperliquid, 950);
    let bind = PerpBind {
        sym,
        asset: ASSET,
        rules: PerpRules::new(4).expect("gate 99 rules"),
    };
    let cfg = ArmCfg {
        slot: SLOT,
        perps: [bind; ARM_PERPS],
        n_perps: 1,
        expires_after_ms: 5_000,
        post_timeout_ms: 5_000,
        skew_ms: 2_000,
        recon_ms: 60_000,
        join_ms: 1_000,
        gone_ms: 3_000,
        shutdown_ms: 300,
        own_account: false,
        anchor_1e6: 0,
        deadman: DeadManCfg::DEFAULT,
        budget_floor: 2_000,
        limit_ms: 600_000,
        leverage: 3,
        outcomes: false,
    };
    let (eng, mut end) = hl_duplex();
    let arm = HlGatewayArm::new(&cfg, eng).expect("gate 99 arm");
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            usize::from(SLOT),
            ExecMode::Live,
            &[core_types::VenueId::Hyperliquid as u8],
            // Order $1 000, gross $10 000, day unbounded, 16 orders.
            SlotCaps::new(1_000 * E6, 10_000 * E6, i64::MAX, 16),
            HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000),
        )
        .expect("gate 99 slot");
    // Per perp $150, resting $100.
    route
        .set_perp_limits(usize::from(SLOT), PerpLimits::new(150 * E6, 100 * E6))
        .expect("gate 99 perp pair");
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        // As the engine composes it (XMM XH4i-5): slot 3's absent gateway
        // arm innermost, its split naming no slot.
        SlotSplit::new(
            SLOT,
            SlotSplit::new(
                exec_router::NO_SLOT,
                NullLiveDispatcher::new(),
                None::<HlGatewayArm>,
            ),
            arm,
        ),
        core_time::WallAnchor::now(),
    );
    // As the engine binds it (XMM XH5a): slot 6's row is slot 6's.
    d.bind_instrument(&InstrumentSpec::new(sym, LAW_LINEAR, 0, 0, 0).owned_by(SLOT))
        .expect("gate 99 row");
    d.mark_ledger_seeded();

    // The boot, through the router's idle, as in the engine.
    end.stats.set_gauges(0, true);
    end.stats.beat(core_time::now_ns());
    d.on_idle();
    let clear = take(&mut end, CMD_SCHEDULE_CANCEL);
    assert!(end.evt.emit(&item(&clear, EV_ACK, 0, 0)));
    assert!(end.evt.emit(&ExecEvent {
        kind: EV_SNAPSHOT,
        ..ExecEvent::default()
    }));
    d.on_idle();
    end.stats
        .set_sweep(end.stats.sweep_requested(), SWEEP_CLEAR);
    d.on_idle();
    let c = take(&mut end, CMD_READ_STATE);
    assert!(end.evt.emit(&read_end(&c, 1_000 * E8, 0, 1)));
    d.on_idle();
    // The sheet read the perp flat: its leverage goes beside the orders read.
    let (c, lev) = take_two(&mut end, CMD_READ_ORDERS, CMD_LEVERAGE);
    assert!(end.evt.emit(&read_end(&c, 0, 0, 0)));
    assert!(end.evt.emit(&item(&lev, EV_ACK, 0, 0)));
    d.on_idle();
    // The budget: $10 M traded, 100 used — room for every round's requests.
    let c = take(&mut end, CMD_READ_LIMIT);
    assert!(end.evt.emit(&read_end(&c, 10_000_000 * E8, 100, 0)));
    d.on_idle();
    let c = take(&mut end, CMD_READ_DAY);
    assert!(end.evt.emit(&read_end(&c, 0, 0, c.time_ms)));
    d.on_idle();
    d.on_venue_event(&mark(sym, core_time::now_ns()));

    let mut fills = 0u64;
    let mut events = 0u64;
    let mut order_ev = core_types::OrderEvent::ZERO;
    let mut pos = 0i64;
    let mut marks_needed = 0u64;
    let mut refused = 0u64;
    let mut health = 0u64;
    let mut report = clob_dispatcher::GatewayArmReport::default();
    let mut reports = 0u64;
    let expired0 = d.exec_counters().ledger_marks_expired;
    let risk0 = d.exec_counters().refused_risk;
    // A post-only crossing (BAD_ALO_PX) retires its replacement EXPIRED.
    let crossed0 = d.exec_counters().retired[usize::from(clob_dispatcher::RETIRED_EXPIRED)];
    let (mut crossed, mut landed) = (0u64, 0u64);
    // The report's quantiles walk trackers that hold samples, as a live
    // gateway's do.
    let mut k = 1u64;
    while k <= 64 {
        end.stats.post_rtt.record(k * 1_000_000);
        end.stats.cancel_rtt.record(k * 1_000_000);
        end.stats.push_to_write.record(k * 1_000);
        end.stats.fill_venue_to_local.record(k * 2_000_000);
        end.stats.rest_rtt.record(k * 3_000_000);
        k += 1;
    }
    let window = std::time::Instant::now();
    let g = AllocGuard::new();
    let mut r = 0u64;
    loop {
        let now = core_time::now_ns();
        end.stats.beat(now);
        let side = if r % 2 == 0 { Side::Bid } else { Side::Ask };
        let first = 4 * r + 1;
        // A quote placed and resting.
        if d.submit(&quote(first, sym, side, E6 / 10)).is_err() {
            // XH2-C: a position no mark prices — a fresh mark, again.
            marks_needed += 1;
            d.on_venue_event(&mark(sym, now));
            let again = d.submit(&quote(first, sym, side, E6 / 10));
            assert!(
                again.is_ok(),
                "gate 99 round {r}: {again:?} resting {} rows {} pos {pos} marks {marks_needed} {:?}",
                d.ledger().resting_usd_1e6(usize::from(SLOT)),
                d.ledger().slot_resting(usize::from(SLOT)),
                d.counters()
            );
        }
        let c = take(&mut end, CMD_PLACE);
        assert!(end.evt.emit(&item(&c, EV_ACK, 0, EVF_RESTING)));
        d.on_idle();
        let placed = d.ledger().resting_usd_1e6(usize::from(SLOT));
        // A modify asking twice the size: landed, or refused crossing.
        let mut live = first;
        let repl = quote(first + 1, sym, side, E6 / 5);
        if d.modify(&ModifyReq::new(first, repl)).is_ok() {
            let m = take(&mut end, CMD_MODIFY);
            if r % 2 == 0 {
                assert!(end.evt.emit(&item(&m, EV_ACK, 0, EVF_RESTING)));
                live = first + 1;
                landed += 1;
            } else {
                assert!(end
                    .evt
                    .emit(&item(&m, EV_REJECTED, ORDER_EVENT_REASON_BAD_ALO_PX, 0)));
                crossed += 1;
            }
            d.on_idle();
            d.on_idle();
            // The rename landed the asked size ($10 more); the refusal's
            // retirement dropped the hold.
            let want = if live == first {
                placed
            } else {
                placed + 10 * E6
            };
            assert_eq!(
                d.ledger().resting_usd_1e6(usize::from(SLOT)),
                want,
                "gate 99 round {r}: the rename, or the hold's drop"
            );
        }
        // Every third round a fill on it, booked back as the engine does.
        if r % 3 == 0 {
            assert!(end.evt.emit(&ExecEvent {
                client_oid: live,
                venue_ms: 1_790_000_000_000 + r,
                px_1e8: 100 * E8,
                sz_1e8: E8 / 20,
                asset: ASSET,
                kind: EV_FILL,
                slot: SLOT,
                flags: if side == Side::Bid { EVF_BUY } else { 0 },
                ..ExecEvent::default()
            }));
            pos += if side == Side::Bid { E6 / 20 } else { -E6 / 20 };
            d.on_idle();
            while let Some(f) = d.try_next_fill() {
                d.on_fill_booked(&f);
                fills += 1;
            }
        }
        // The perp's caps, on the side the position grows: $160 over the
        // per-perp $150; $99 over the resting $100 beside what rests (≥ $5).
        let grow = if pos >= 0 { Side::Bid } else { Side::Ask };
        assert!(
            d.submit(&quote(first + 2, sym, grow, 16 * E6 / 10))
                .is_err(),
            "gate 99: the per-perp cap let $160 through"
        );
        assert!(
            d.submit(&quote(first + 3, sym, grow, 99 * E6 / 100))
                .is_err(),
            "gate 99: the resting cap let $99 through"
        );
        refused += 2;
        // The member reads its health.
        health += u64::from(d.exec_health(SLOT, now).blocked == 0);
        // XH4f (D11): the operator's report, through the whole composition.
        if r % 500 == 0 {
            assert!(d.gateway_arm_report(now, &mut report));
            reports += 1;
        }
        // The live order cancelled and released on its retirement.
        assert!(d
            .cancel(&CancelReq::of(&quote(live, sym, side, E6), now))
            .is_ok());
        let x = take(&mut end, CMD_CANCEL);
        assert!(end.evt.emit(&item(&x, EV_ACK, 0, 0)));
        assert!(end.evt.emit(&ExecEvent {
            client_oid: live,
            asset: ASSET,
            kind: EV_CANCELED,
            reason: ORDER_EVENT_REASON_CANCEL_REQUESTED,
            slot: SLOT,
            ..ExecEvent::default()
        }));
        d.on_idle();
        d.on_idle();
        // The members' order events, taken as the engine takes them.
        while d.try_next_order_event(&mut order_ev) {
            events += 1;
        }
        // Every 2 000 rounds a stale mark: the router's sweep expires it.
        if r % 2_000 == 1_999 {
            d.on_venue_event(&mark(sym, now.saturating_sub(31_000_000_000)));
        }
        r += 1;
        // Until each path has run: the rounds, and two XH2-C cycles (the
        // router sweeps its marks once a second).
        if r >= ROUNDS && marks_needed > 1 {
            break;
        }
        assert!(
            window.elapsed() < std::time::Duration::from_secs(20),
            "gate 99: no mark expired twice in 20 s ({marks_needed})"
        );
    }
    let (allocs, bytes, _) = g.delta();
    let e = d.exec_counters();
    let rc = *d.counters();
    assert_eq!(
        d.ledger().slot_resting(usize::from(SLOT)),
        0,
        "every row released"
    );
    assert!(fills > 0 && refused >= 2 * r && health > 0 && events > 2 * r);
    assert!(
        e.refused_risk - risk0 >= 2 * r,
        "the router refused every cap probe"
    );
    assert!(
        crossed > 0
            && e.retired[usize::from(clob_dispatcher::RETIRED_EXPIRED)] - crossed0 == crossed,
        "every crossing modify's replacement retired in the window"
    );
    assert!(landed > 0, "renames drained in the window");
    assert!(reports > 0 && report.present == 1 && report.refused_cap_perp > 0);
    assert!(report.post_rtt_p99_ns > 0 && report.leverage_dark == 0);
    assert!(rc.refused_cap_perp > 0 && rc.refused_max_resting > 0);
    assert!(rc.refused_unpriced >= marks_needed && e.ledger_marks_expired > expired0);
    assert_eq!(
        allocs, 0,
        "slot 6 through the router allocated {allocs} times ({bytes} B) over {r} rounds"
    );
}

/// **XMM gate 96a (XH4b) — the HL gateway's ENGINE half is 0 B/op.**
/// XG-1: an arm's submit is one `HlCmd` ring write and its answers are
/// `ExecEvent` ring reads — no syscall, no allocation, never blocking.
/// 10 000 round trips through the production duplex (`hl_duplex`, the
/// 1 024 / 4 096 rings), each command read in place on the gateway side
/// and each event read in place on the engine side.
#[test]
fn hl_gateway_engine_half_is_zero_alloc() {
    use exec_hyperliquid::event::{ExecEvent, EV_ACK};
    use exec_hyperliquid::{hl_duplex, HlCmd, Tif};

    let (mut eng, mut gw) = hl_duplex();
    let cmd = HlCmd::place(6, 1, 1, true, 250_000_000_000, 10_000_000, Tif::Alo);
    let ev = ExecEvent {
        kind: EV_ACK,
        ..ExecEvent::default()
    };
    let mut acc = 0u64;
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < 10_000 {
        assert!(eng.cmd.push(&cmd.at(i)));
        match gw.cmd.pop() {
            Some(c) => acc ^= c.ts_ns,
            None => panic!("gate 96a: the command did not arrive"),
        }
        assert!(gw.evt.emit(&ExecEvent { aux: i, ..ev }));
        match eng.evt.pop() {
            Some(e) => acc ^= e.aux,
            None => panic!("gate 96a: the event did not arrive"),
        }
        i += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    assert_eq!(
        allocs, 0,
        "the HL gateway's engine half allocated {allocs} times ({bytes} B)"
    );
}

/// **XMM gate 102 (XH4i-1) — an outcome map names a leg in 0 B/op.** The
/// gateway thread resolves the coin of every fill, order-update,
/// open-order and position row, and every sweep hit, through its
/// `CoinMap`; on a map built `with_outcomes` (BIN15's, slot 3) a `#<enc>`
/// coin is parsed in place into its asset id. 10 000 lookups over a perp
/// name, two legs, every way out of the parse — a bare `#`, a name over
/// 20 bytes, a leading zero, a non-digit, a digit overflow, a base
/// overflow — and the balance spelling `+<enc>`, which never enters it.
/// Gate 96b's turn runs a perps-only map (slot 6's); its outcome-map
/// variant comes with the slot-3 composition (XH4i-5).
#[test]
fn hl_gateway_outcome_coin_map_is_zero_alloc() {
    use exec_hyperliquid::asset::ASSET_BASE;
    use exec_hyperliquid::event::ASSET_UNKNOWN;
    use exec_hyperliquid::CoinMap;

    let mut m = CoinMap::with_outcomes();
    assert!(m.insert(b"ETH", 1));
    let coins: [(&[u8], u32); 10] = [
        (b"ETH", 1),
        (b"#1941", ASSET_BASE + 1941),
        (b"#154150", ASSET_BASE + 154_150),
        (b"#", ASSET_UNKNOWN),
        (b"#123456789012345678901", ASSET_UNKNOWN),
        (b"#01941", ASSET_UNKNOWN),
        (b"#19a1", ASSET_UNKNOWN),
        (b"+1941", ASSET_UNKNOWN),
        (b"#42949672950", ASSET_UNKNOWN),
        (b"#4194967296", ASSET_UNKNOWN),
    ];
    let mut acc = 0u64;
    let g = AllocGuard::new();
    let mut i = 0usize;
    while i < 10_000 {
        let (coin, want) = coins[i % coins.len()];
        let a = m.asset(std::hint::black_box(coin));
        assert!(a == want, "gate 102: a coin resolved to the wrong id");
        acc = acc.wrapping_add(u64::from(a));
        i += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    assert_eq!(
        allocs, 0,
        "the outcome coin map allocated {allocs} times ({bytes} B)"
    );
}

/// **XMM gate 103 (XH4i-3b-2a) — the gateway arm's outcome mode is 0 B/op.**
/// An outcome arm (slot 3, BIN15's HIP-4 legs: no perp, no leverage) booted
/// through the duplex by a scripted venue that answers every command at
/// once. Then, round after round on the real clock: the family rolls (the
/// next instance binds both legs, the last one's retire — LAW E-8's cancel
/// of the MAKER still resting there, and the orders read at once); a fill
/// on the retired generation for that MAKER (its row held) and one for the
/// last round's IoC (its row gone); a MAKER on the new YES leg and an IoC on
/// NO (BIN15's client ids, no flag), answered resting (the owner noted); one
/// refusal of each kind (a stale instance, an unbound leg, an off-grid
/// price); a modify of the MAKER and a cancel of the IoC; then every other
/// branch of `on_leg_fill` — ours on the live generation, a mismatched
/// asset, `ASSET_UNKNOWN`, a refused fill, an owned settlement, an unowned
/// one (a family no order of ours trades) and another slot's row. Every
/// out-queue drained as the engine does. The scripted venue rests the IoC so
/// it can be cancelled, and settles a holding as a sale; no row closes
/// unseen (`gone_ms` 60 s) and the budget never nears its floor. The warm-up
/// runs until every path has run; the window must run each at least 99 % of
/// its scripted count — at this pace the arm's own gates refuse an
/// occasional round's verbs (about 10 in 40 000, measured), so exact counts
/// are not the arm's to promise.
#[test]
fn hl_gateway_outcome_arm_is_zero_alloc() {
    use clob_dispatcher::OrderDispatch;
    use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
    use core_types::{CancelReq, ChannelEvent, ChannelId, ModifyReq, Order, OrderEvent, Side};
    use exec_gateway::stats::SWEEP_CLEAR;
    use exec_hyperliquid::anchor::AnchorSlot;
    use exec_hyperliquid::arm::{ArmCfg, HlGatewayArm, PerpBind, ARM_PERPS};
    use exec_hyperliquid::asset::AssetTable;
    use exec_hyperliquid::cmd::{
        CMD_CANCEL, CMD_MODIFY, CMD_PLACE, CMD_READ_DAY, CMD_READ_LIMIT, CMD_READ_ORDERS,
        CMD_READ_SPOT, CMD_READ_STATE,
    };
    use exec_hyperliquid::deadman::DeadManCfg;
    use exec_hyperliquid::event::{
        ExecEvent, ASSET_UNKNOWN, EVF_BUY, EVF_CMD_SHIFT, EVF_POST_ITEM, EVF_RESTING, EV_ACK,
        EV_BALANCE, EV_FILL, EV_READ_DONE, EV_SNAPSHOT, READ_DAY, READ_LIMIT, READ_ORDERS,
        READ_SPOT, READ_STATE, REASON_SETTLEMENT, SLOT_FOREIGN, SPOT_LEGS_READ,
    };
    use exec_hyperliquid::gateway::HlGatewayEnd;
    use exec_hyperliquid::hl_duplex;
    use exec_hyperliquid::rules::PerpRules;

    const SLOT: u8 = 3;
    const ROUNDS: u64 = 40_000;
    /// Each path's runs a round, in `paths()`'s order: one roll; one E-8
    /// cancel (the last round's MAKER); one refusal of each kind; one owned
    /// and one unowned settlement; two fills on the retired generation (row
    /// held, row gone); one mismatch; one refused fill; four fills booked
    /// (the two old, the live one, the settlement); two unresolved (the
    /// mismatch, `ASSET_UNKNOWN`); one foreign row; one modify; one cancel;
    /// XH4i-5: one repeat of the live roll, left alone (A-S7-83).
    const PER_ROUND: [u64; 16] = [1, 1, 1, 1, 1, 1, 1, 2, 1, 1, 4, 2, 1, 1, 1, 1];
    /// BIN15's first family: its YES leg (NO is the next ordinal).
    const LEG_YES: u32 = ((VenueId::Hyperliquid as u32) << 24) | 4_096;
    /// A family no order of ours trades: its settlements are unowned.
    const LEG_OTHER: u32 = LEG_YES + 2;
    const INST_OTHER: u32 = 900;
    /// The traded family's first instance; one more each round.
    const INST_BASE: u32 = 1_000;

    /// The scripted venue: every command answered at once — orders resting,
    /// cancels done, reads with what the boot needs (a $1 000 account, $500
    /// USDC, 100 of 1 010 000 requests used — $1M traded, so the budget never
    /// nears its floor — no day's fills, nothing resting), the dead man's
    /// posts acknowledged.
    fn answer(gw: &mut HlGatewayEnd, n: &mut u64) {
        gw.stats.beat(core_time::now_ns());
        while let Some(p) = gw.cmd.pop() {
            let c = *p;
            *n += 1;
            let bits = if c.kind <= CMD_CANCEL { c.kind } else { 0 };
            let item = ExecEvent {
                client_oid: c.client_oid,
                asset: c.asset,
                slot: c.slot,
                flags: EVF_POST_ITEM | (bits << EVF_CMD_SHIFT),
                ..ExecEvent::default()
            };
            let end = ExecEvent {
                client_oid: c.client_oid,
                slot: c.slot,
                kind: EV_READ_DONE,
                ..ExecEvent::default()
            };
            let e = match c.kind {
                CMD_PLACE | CMD_MODIFY => ExecEvent {
                    kind: EV_ACK,
                    flags: item.flags | EVF_RESTING,
                    oid: *n,
                    ..item
                },
                CMD_READ_STATE => ExecEvent {
                    reason: READ_STATE,
                    px_1e8: 100_000_000_000,
                    venue_ms: *n,
                    ..end
                },
                CMD_READ_SPOT => {
                    // XH4i-5: one leg row a read — the untraded family's live
                    // YES leg, a contract the ledger never booked (a drift) —
                    // and the end counting it: `on_balance` and the legs'
                    // comparison run every read.
                    let leg = ExecEvent {
                        client_oid: c.client_oid,
                        slot: c.slot,
                        kind: EV_BALANCE,
                        asset: AssetTable::asset_id(INST_OTHER, 0).expect("gate 103: an id"),
                        sz_1e8: 100_000_000,
                        px_1e8: 30_000_000,
                        ..ExecEvent::default()
                    };
                    assert!(gw.evt.emit(&leg), "gate 103: the event ring is full");
                    ExecEvent {
                        reason: READ_SPOT,
                        px_1e8: 50_000_000_000,
                        sz_1e8: 1,
                        asset: SPOT_LEGS_READ,
                        oid: 1,
                        ..end
                    }
                }
                CMD_READ_ORDERS => ExecEvent {
                    reason: READ_ORDERS,
                    ..end
                },
                CMD_READ_LIMIT => ExecEvent {
                    reason: READ_LIMIT,
                    aux: 100,
                    px_1e8: 100_000_000_000_000,
                    ..end
                },
                CMD_READ_DAY => ExecEvent {
                    reason: READ_DAY,
                    ..end
                },
                // A cancel done; the dead man's posts (no command bits).
                _ => ExecEvent {
                    kind: EV_ACK,
                    ..item
                },
            };
            assert!(gw.evt.emit(&e), "gate 103: the event ring is full");
        }
    }

    /// A roll of the family whose YES leg is `sym` (`pack_roll_seq`'s
    /// layout: a created instance).
    fn roll(sym: u32, outcome: u32) -> ChannelEvent {
        ChannelEvent::new(
            1,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym,
            u64::from(outcome) | (60u64 << 32),
            0,
            1_000_000,
            2_000_000_000,
        )
    }

    /// BIN15's client id: the null arm's bit 63, a 14-bit sequence, the
    /// instance in the low 32 bits.
    fn cid(seq: u64, inst: u32) -> u64 {
        (1u64 << 63) | ((seq & 0x3FFF) << 49) | u64::from(inst)
    }

    /// BIN15's order (no flag): four contracts.
    fn leg(seq: u64, inst: u32, sym: u32, kind: u8, px_1e6: i64) -> Order {
        let mut o = Order::new(
            0,
            VenueId::Hyperliquid,
            sym,
            Side::Bid,
            kind,
            Price(px_1e6),
            Qty(4_000_000),
            cid(seq, inst),
        );
        o.strategy_id = SLOT;
        o
    }

    /// A `userFills` row as an outcome map emits it: a buy of one contract
    /// (`cid` 0: the venue's settlement row, which names no slot — a sale of
    /// the holding).
    fn fill(cid: u64, slot: u8, asset: u32, px_1e8: i64, tid: u64) -> ExecEvent {
        ExecEvent {
            client_oid: cid,
            oid: tid,
            venue_ms: tid,
            px_1e8,
            sz_1e8: 100_000_000,
            aux: tid,
            asset,
            kind: EV_FILL,
            reason: if cid == 0 { REASON_SETTLEMENT } else { 0 },
            slot,
            flags: if cid == 0 { 0 } else { EVF_BUY },
            ..ExecEvent::default()
        }
    }

    fn drain(arm: &mut HlGatewayArm, acc: &mut u64) {
        while let Some(f) = arm.try_next_fill() {
            *acc ^= f.order_id;
        }
        let mut e = OrderEvent::ZERO;
        while arm.try_next_order_event(&mut e) {
            *acc ^= e.client_oid;
        }
        while let Some(r) = arm.try_next_retired() {
            *acc ^= r.client_oid;
        }
        while let Some(n) = arm.try_next_renamed() {
            *acc ^= n.client_oid;
        }
    }

    /// The paths each window must run again.
    fn paths(arm: &HlGatewayArm, modifies: u64, cancels: u64) -> [u64; 16] {
        let o = arm.outcome_counters().expect("gate 103: an outcome arm");
        let c = arm.counters();
        [
            o.rolls_bound,
            o.e8_cancels,
            o.refused_stale,
            o.refused_unbound,
            o.refused_rules,
            o.settlements,
            o.settlements_unowned,
            o.fills_old_gen,
            o.fills_mismatched,
            o.fills_refused,
            c.fills,
            c.fills_unresolved,
            c.fills_foreign,
            modifies,
            cancels,
            o.rolls_repeated,
        ]
    }

    /// One round on instance `INST_BASE + r` (the last round's retires).
    fn round(
        arm: &mut HlGatewayArm,
        gw: &mut HlGatewayEnd,
        r: u64,
        n: &mut u64,
        acc: &mut u64,
        done: &mut [u64; 2],
    ) {
        let k = INST_BASE + r as u32;
        let seq = r * 4;
        // XH4i-5: the outcome arm's report as `/state` reads it (1 Hz):
        // every thousandth round.
        if r % 1_000 == 0 {
            let mut rep = clob_dispatcher::GatewayArmReport::default();
            *acc ^= u64::from(arm.gateway_arm_report(core_time::now_ns(), &mut rep))
                ^ rep.outcome_rolls_bound;
        }
        let yes = AssetTable::asset_id(k, 0).expect("gate 103: an id");
        let no = AssetTable::asset_id(k, 1).expect("gate 103: an id");
        let old_yes = AssetTable::asset_id(k - 1, 0).expect("gate 103: an id");
        let old_no = AssetTable::asset_id(k - 1, 1).expect("gate 103: an id");
        // The family rolls: k binds, k − 1 retires — the last round's MAKER
        // (modified, resting) is cancelled and the orders are read.
        arm.on_venue_event(&roll(LEG_YES, k));
        // A-S7-83: the same roll again — left alone.
        arm.on_venue_event(&roll(LEG_YES, k));
        // The retired generation: that MAKER (its row held while its cancel
        // is out) and the last round's IoC (its row gone).
        let t = r * 16;
        assert!(gw.evt.emit(&fill(
            cid(seq.wrapping_sub(2), k - 1),
            SLOT,
            old_yes,
            31_000_000,
            t
        )));
        assert!(gw.evt.emit(&fill(
            cid(seq.wrapping_sub(3), k - 1),
            SLOT,
            old_no,
            31_000_000,
            t + 1
        )));
        // The new instance: a MAKER on YES and an IoC on NO; one refusal of
        // each kind — a stale instance, an unbound leg, an off-grid price.
        let maker = leg(seq, k, LEG_YES, ORDER_KIND_MAKER, 300_000);
        let ioc = leg(seq + 1, k, LEG_YES + 1, ORDER_KIND_IOC, 600_000);
        *acc ^= u64::from(arm.submit(&maker).is_ok());
        *acc ^= u64::from(arm.submit(&ioc).is_ok()) << 1;
        *acc ^= u64::from(
            arm.submit(&leg(seq + 3, k - 1, LEG_YES, ORDER_KIND_MAKER, 300_000))
                .is_err(),
        ) << 2;
        *acc ^= u64::from(
            arm.submit(&leg(seq + 3, k, LEG_YES + 4, ORDER_KIND_MAKER, 300_000))
                .is_err(),
        ) << 3;
        *acc ^= u64::from(
            arm.submit(&leg(seq + 3, k, LEG_YES, ORDER_KIND_MAKER, 300_055))
                .is_err(),
        ) << 4;
        answer(gw, n);
        let _ = arm.on_idle();
        drain(arm, acc);
        // A modify of the MAKER (its leg kept) and a cancel of the IoC.
        let next = leg(seq + 2, k, LEG_YES, ORDER_KIND_MAKER, 310_000);
        done[0] += u64::from(arm.modify(&ModifyReq::new(maker.client_oid, next)).is_ok());
        let mut x = CancelReq::new(0, VenueId::Hyperliquid, LEG_YES + 1, ioc.client_oid);
        x.strategy_id = SLOT;
        done[1] += u64::from(arm.cancel(&x).is_ok());
        answer(gw, n);
        let _ = arm.on_idle();
        // Every other branch of `on_leg_fill`.
        let other = AssetTable::asset_id(INST_OTHER, 0).expect("gate 103: an id");
        let rows = [
            // Ours on the live generation: a quarter of the MAKER.
            fill(next.client_oid, SLOT, yes, 31_000_000, t + 2),
            // Ours naming another asset than its row's.
            fill(next.client_oid, SLOT, no, 31_000_000, t + 3),
            // Ours with no row, on no leg the gateway could name.
            fill(cid(seq + 3, k), SLOT, ASSET_UNKNOWN, 31_000_000, t + 4),
            // A refused one: a negative price.
            fill(cid(seq + 3, k), SLOT, yes, -1, t + 5),
            // The venue's settlements: on our leg (owned), on a family no
            // order of ours trades (unowned).
            fill(0, SLOT_FOREIGN, yes, 100_000_000, t + 6),
            fill(0, SLOT_FOREIGN, other, 100_000_000, t + 7),
            // Another slot's row.
            fill(1, 5, yes, 31_000_000, t + 8),
        ];
        let mut i = 0usize;
        while i < rows.len() {
            assert!(gw.evt.emit(&rows[i]), "gate 103: the event ring is full");
            i += 1;
        }
        let _ = arm.on_idle();
        answer(gw, n);
        let _ = arm.on_idle();
        drain(arm, acc);
    }

    // ---- boot (NOT measured) ----
    let bind = PerpBind {
        sym: 42,
        asset: 3,
        rules: PerpRules::new(4).expect("rules"),
    };
    let cfg = ArmCfg {
        slot: SLOT,
        perps: [bind; ARM_PERPS],
        n_perps: 0,
        expires_after_ms: 1,
        post_timeout_ms: 5_000,
        skew_ms: 0,
        recon_ms: 1,
        join_ms: 1,
        gone_ms: 60_000,
        shutdown_ms: 1_000,
        own_account: true,
        anchor_1e6: 0,
        deadman: DeadManCfg::DEFAULT,
        budget_floor: 2_000,
        limit_ms: 2,
        leverage: 0,
        outcomes: true,
    };
    let (eng, mut gw) = hl_duplex();
    let mut arm = HlGatewayArm::new(&cfg, eng).expect("gate 103: the outcome arm");
    let keep = std::sync::Arc::new(AnchorSlot::new());
    arm.keep_anchor_in(std::sync::Arc::clone(&keep));
    gw.stats.set_gauges(0, true);
    let mut n = 0u64;
    let _ = arm.on_idle();
    answer(&mut gw, &mut n);
    assert!(gw.evt.emit(&ExecEvent {
        kind: EV_SNAPSHOT,
        ..ExecEvent::default()
    }));
    let _ = arm.on_idle();
    gw.stats.set_sweep(gw.stats.sweep_requested(), SWEEP_CLEAR);
    let mut spins = 0u32;
    while !arm.ready() {
        let _ = arm.on_idle();
        answer(&mut gw, &mut n);
        spins += 1;
        assert!(spins < 1_000_000, "gate 103: the arm never booted");
    }
    arm.on_venue_event(&roll(LEG_OTHER, INST_OTHER));

    // ---- warm-up: every path runs ----
    let mut acc = 0u64;
    let mut done = [0u64; 2];
    let mut r = 0u64;
    while r < 2_000 || paths(&arm, done[0], done[1]).contains(&0) {
        round(&mut arm, &mut gw, r, &mut n, &mut acc, &mut done);
        r += 1;
        assert!(
            r < 200_000,
            "gate 103: a path never ran: {:?}",
            paths(&arm, done[0], done[1])
        );
    }

    // ---- the window (measured) ----
    let before = paths(&arm, done[0], done[1]);
    let legs_before = arm.outcome_counters().expect("gate 103").leg_reads;
    let g = AllocGuard::new();
    let end = r + ROUNDS;
    while r < end {
        round(&mut arm, &mut gw, r, &mut n, &mut acc, &mut done);
        r += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    let after = paths(&arm, done[0], done[1]);
    let mut k = 0usize;
    while k < after.len() {
        assert!(
            after[k] - before[k] >= PER_ROUND[k] * ROUNDS / 100 * 99,
            "gate 103: path {k} fell short of its script in the window: {before:?} → {after:?}"
        );
        k += 1;
    }
    assert!(
        arm.outcome_counters().expect("gate 103").leg_reads > legs_before,
        "gate 103: the spot reads' legs were compared in the window"
    );
    assert_eq!(
        allocs, 0,
        "the outcome gateway arm allocated {allocs} times ({bytes} B)"
    );
}

/// **XMM gate 97 (XH4c) — the dead-man switch and the perp rules are
/// 0 B/op.** Both run on the engine thread from XH4f, the switch on
/// every arm tick: `DeadMan::poll/on_event/unsent` over a scripted 55 h
/// (the boot clear, the probe, pushes every 5 s, clears, a full ring, a
/// local refusal, answers in doubt, the rate limit's back-off, a stall
/// through a deadline, the venue's refusal, two 00:00 UTC rolls), and
/// `PerpRules::{tick_1e8, round_px, round_sz_down, check}` over a
/// geometric price sweep at every `szDecimals`.
#[test]
fn hl_deadman_and_perp_rules_are_zero_alloc() {
    use exec_hyperliquid::deadman::{DeadMan, DeadManCfg};
    use exec_hyperliquid::event::{
        ExecEvent, EVF_POST_ITEM, EV_ACK, EV_REJECTED, EV_UNANSWERED, REASON_LOCAL,
        REASON_RATE_LIMIT, REASON_TRANSPORT,
    };
    use exec_hyperliquid::rules::{PerpRules, Round};
    use exec_hyperliquid::HlCmd;

    // One verdict per post: a cycle of acceptances, a local refusal and a
    // verdict in doubt; every ninth post a full ring instead; the rate
    // limit once (its minute's back-off lets a deadline pass: a trigger)
    // and the venue's refusal of an arm once (its arms back off an answer).
    const CYCLE: [(u8, u8); 8] = [
        (EV_ACK, 0),
        (EV_ACK, 0),
        (EV_ACK, 0),
        (EV_REJECTED, REASON_LOCAL),
        (EV_ACK, 0),
        (EV_UNANSWERED, REASON_TRANSPORT),
        (EV_ACK, 0),
        (EV_ACK, 0),
    ];
    // 2026-09-26 00:00:00 UTC.
    const DAY0_MS: u64 = 1_790_380_800_000;

    let mut dm = DeadMan::new(DeadManCfg::DEFAULT, 6, 1 << 40).expect("the default timing");
    let mut scratch = HlCmd::noop(0, 0);
    let mut rules = [PerpRules::new(0).expect("0"); 7];
    let mut sz = 1u8;
    while sz <= 6 {
        rules[usize::from(sz)] = PerpRules::new(sz).expect("sz");
        sz += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut posts = 0u32;
    let mut tick = 0u64;
    while tick < 200_000 {
        let now = DAY0_MS + tick * 1_000;
        // Day 1, ticks 100 000 .. 100 030: the engine stalls through a
        // deadline (day 0 is parked from post 400 on).
        let stalled = (100_000..100_030).contains(&tick);
        let want = tick % 97 != 0;
        if !stalled && dm.poll(now, tick, want, &mut scratch) {
            posts += 1;
            if posts % 9 == 0 {
                dm.unsent(now);
            } else {
                let (kind, reason) = match posts {
                    300 => (EV_REJECTED, REASON_RATE_LIMIT),
                    400 => (EV_REJECTED, 0),
                    _ => CYCLE[(posts % 8) as usize],
                };
                let ev = ExecEvent {
                    kind,
                    reason,
                    slot: scratch.slot,
                    client_oid: scratch.client_oid,
                    flags: EVF_POST_ITEM,
                    ..ExecEvent::default()
                };
                acc ^= u64::from(dm.on_event(&ev, now + 30));
            }
        }
        acc ^= dm.deadline_ms();
        tick += 1;
    }

    let mut k = 0usize;
    while k < rules.len() {
        let r = rules[k];
        let mut px = 7i64;
        while px < 500_000_000_000_000 {
            acc ^= r.tick_1e8(px) as u64;
            if let Some(p) = r.round_px(px, Round::Down) {
                acc ^= p as u64;
            }
            if let Some(p) = r.round_px(px, Round::Up) {
                acc ^= p as u64;
            }
            acc ^= r.round_sz_down(px) as u64;
            acc ^= u64::from(r.check(px, r.lot_1e8() * 1_000).is_ok());
            px = px * 3 + 1;
        }
        k += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    let c = *dm.counters();
    assert!(
        c.accepted > 1_000 && c.triggers >= 1 && c.refused == 1 && c.unsent > 0,
        "the script reached the paths it names: {c:?}"
    );
    assert!(c.in_doubt > 0 && c.rate_limited == 1 && c.clears > 0);
    assert_eq!(
        allocs, 0,
        "the dead-man switch / perp rules allocated {allocs} times ({bytes} B)"
    );
}

/// **XMM gate 98 (XH4d) — the gateway arm is 0 B/op.** The engine's half
/// of the Hyperliquid gateway arm (`HlGatewayArm`, an `OrderDispatch`)
/// against a scripted gateway end in-process (no socket: the gateway's
/// own turn, reads included, is gate 96b's). Every round: a place (bids
/// and asks), a modify of the last round's order on its own side, cancels,
/// the cancel-all word every thousand; the script answers resting (the
/// stream's `open` row first, every fourth), fills (the stream's `filled`
/// first, the fill and its fee a round later — every seventh never: the
/// join window), post-only crosses (the reply and the stream's row, every
/// other one the row first), in-doubt verdicts (settled by a cancel after
/// their 1 ms expiry), cancels that succeed (with the stream's `canceled`
/// row, every other one first) or find nothing (gone, read again, then
/// closed), sheets (every fifth failing; flat until the arm boots — a
/// position holds the boot — then one position), orders reads that list
/// the last sixteen accepted orders (listings, revivals, races) and every
/// seventh one order the arm never placed (cancelled), a stream gap every
/// 97 commands and another slot's resting row every 89, the gateway's
/// heartbeat every answer; XH4e: the budget read every 2 ms (every ninth
/// failing, every thirteenth answering a figure under the floor — growth
/// refused until the next read lifts it; every place, modify and cancel
/// charged, the sweep's cancels by the gateway's word), every
/// seventeenth place or modify refused `THROTTLED` (taken back from the
/// budget), and, after the boot — its own window — the day's spend (a
/// page that moves, then one that adds nothing past its start); XH5b-1:
/// the spot sheet after each sheet ($500 USDC; every sixth failing, every
/// seventh listing no USDC — both refused, the arm's `spot_failed`);
/// XH5b-2: the boot's anchor handed to a real keeper that stores it just
/// before the window, whose first pass reads it kept, and in every round of
/// it that keeper's pass (done) and one nothing is handed to; XMM XH5c-2:
/// top-ups armed (1 000 requests a purchase, no day's max) — they start
/// with the anchor kept, in the window — every seventh budget read inside
/// the band (never while a purchase awaits its confirming read), decided,
/// its state stored by a REAL top-up keeper turned every round (the line
/// in place, a temp file, an fsync, a rename), the purchase built in place
/// and answered accepted (the next reads answer the full headroom, which
/// shows it; its price charged once a sheet issued after it and its spot
/// half land) or, every third, refused `THROTTLED` (never left: the day's
/// charge returned; decided again, its state the one stored — kept with no
/// write) — the window runs until each has run — then, measured alone, a
/// venue refusal stops them and the stop is stored; the halt signals and
/// the cancel-all state; every out-queue drained as the engine does.
/// XMM XH5d: the `open` rows carry the venue's time, and every fourth
/// place or modify's comes after the answer (its RESTING repeated with it);
/// the ALO priority armed, every third order bids it (a place's fee
/// tallied; a modify's bid dropped).
/// Timers run on the real clock (1 ms windows): the warm-up runs until
/// every path has run, and the window must run each again.
#[test]
fn hl_gateway_arm_is_zero_alloc() {
    use clob_dispatcher::{CancelAllState, OrderDispatch};
    use core_fill::ORDER_KIND_MAKER;
    use core_types::{
        CancelReq, ModifyReq, Order, OrderEvent, Side, ORDER_EVENT_CANCELED,
        ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_OTHER, ORDER_EVENT_REASON_REPLACED,
        ORDER_EVENT_RESTING,
    };
    use exec_gateway::stats::SWEEP_CLEAR;
    use exec_hyperliquid::anchor::{AnchorKeeper, AnchorSlot, Kept};
    use exec_hyperliquid::arm::{ArmCfg, ArmCounters, HlGatewayArm, PerpBind, ARM_PERPS};
    use exec_hyperliquid::budget::TopupGovernor;
    use exec_hyperliquid::cmd::{
        CMD_CANCEL, CMD_MODIFY, CMD_PLACE, CMD_READ_DAY, CMD_READ_LIMIT, CMD_READ_ORDERS,
        CMD_READ_SPOT, CMD_READ_STATE, CMD_RESERVE,
    };
    use exec_hyperliquid::deadman::DeadManCfg;
    use exec_hyperliquid::event::{
        ExecEvent, EVF_BUY, EVF_CMD_SHIFT, EVF_CROSSED, EVF_POST_ITEM, EVF_RESTING, EV_ACK,
        EV_CANCELED, EV_FILL, EV_FILLED, EV_FILL_META, EV_GAP, EV_OPEN_ORDER, EV_POSITION,
        EV_READ_DONE, EV_READ_FAILED, EV_REJECTED, EV_RESTING, EV_SNAPSHOT, EV_UNANSWERED,
        READ_DAY, READ_LIMIT, READ_ORDERS, READ_SPOT, READ_STATE, REASON_NOT_FOUND,
        REASON_THROTTLED,
    };
    use exec_hyperliquid::gateway::HlGatewayEnd;
    use exec_hyperliquid::hl_duplex;
    use exec_hyperliquid::rules::PerpRules;
    use exec_hyperliquid::topup::{Topped, TopupKeeper, TopupSlot};

    const SLOT: u8 = 6;
    const SYM: u32 = 42;
    const ASSET: u32 = 3;
    const ROUNDS: u64 = 40_000;
    const RECENT: usize = 16;

    /// The scripted venue's memory: every buffer fixed.
    struct Script {
        n: u64,
        /// Sheets and orders reads answered, each counted apart (every
        /// fifth sheet fails, every seventh orders read lists a stranger
        /// — whatever the arm's interleaving of the two).
        sheets: u64,
        orders: u64,
        /// Orders answered resting lately (the orders read lists them).
        recent: [u64; RECENT],
        at: usize,
        /// A fill held for the next answer (after its `filled` row).
        held: Option<ExecEvent>,
        replaced: u64,
        modifies: u64,
        /// The sheets are flat until the arm boots (a position in a bound
        /// perp holds the boot: the member boots flat); then they carry one.
        flat: bool,
        /// XH4e: budget reads and day pages answered.
        limits: u64,
        days: u64,
        /// XH5b-1: spot sheets answered (every sixth failing, every seventh
        /// listing no USDC: the arm counts both `spot_failed`).
        spots: u64,
        /// XMM XH5c-2: the weight bought (the reads' surplus), the
        /// purchases answered, the reads kept out of the band and off the
        /// floor while a purchase awaits its confirming read, and whether
        /// the venue refuses the next one (the stop's window).
        surplus: u64,
        reserves: u64,
        calm: u8,
        refuse: bool,
        /// XMM XH5d: RESTINGs stamped with the `open` row's time — the row
        /// before the answer landing the order, and after it, the repeat.
        stamped: u64,
        repeated: u64,
    }

    /// The gateway's end: every command answered by the script.
    fn answer(gw: &mut HlGatewayEnd, s: &mut Script) {
        // The gateway's turn beats: the arm's stream is alive.
        gw.stats.beat(core_time::now_ns());
        if let Some(f) = s.held.take() {
            assert!(gw.evt.emit(&f));
            assert!(gw.evt.emit(&ExecEvent {
                kind: EV_FILL_META,
                px_1e8: -10,
                flags: EVF_CROSSED,
                ..f
            }));
        }
        while let Some(p) = gw.cmd.pop() {
            let c = *p;
            s.n += 1;
            let n = s.n;
            let bits = if c.kind <= CMD_CANCEL { c.kind } else { 0 };
            let item = ExecEvent {
                client_oid: c.client_oid,
                asset: c.asset,
                slot: c.slot,
                flags: EVF_POST_ITEM | (bits << EVF_CMD_SHIFT),
                ..ExecEvent::default()
            };
            let row = ExecEvent {
                client_oid: c.client_oid,
                asset: c.asset,
                slot: c.slot,
                ..ExecEvent::default()
            };
            let end = ExecEvent {
                client_oid: c.client_oid,
                slot: c.slot,
                kind: EV_READ_DONE,
                ..ExecEvent::default()
            };
            match c.kind {
                // XH4e: refused unsent over the rate shares — never left,
                // no row; the arm takes its request back.
                CMD_PLACE | CMD_MODIFY if n % 17 == 0 => assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_REJECTED,
                    reason: REASON_THROTTLED,
                    ..item
                })),
                CMD_PLACE | CMD_MODIFY if n % 11 == 0 => {
                    let reply = ExecEvent {
                        kind: EV_REJECTED,
                        reason: ORDER_EVENT_REASON_BAD_ALO_PX,
                        ..item
                    };
                    let stream = ExecEvent {
                        kind: EV_REJECTED,
                        reason: ORDER_EVENT_REASON_BAD_ALO_PX,
                        ..row
                    };
                    // Every other one the stream's row beats the reply.
                    let (a, b) = if n % 22 == 0 {
                        (stream, reply)
                    } else {
                        (reply, stream)
                    };
                    assert!(gw.evt.emit(&a));
                    assert!(gw.evt.emit(&b));
                }
                CMD_PLACE | CMD_MODIFY if n % 13 == 0 => assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_UNANSWERED,
                    ..item
                })),
                CMD_PLACE | CMD_MODIFY => {
                    // XMM XH5d: the `open` row carries the venue's time —
                    // every fourth before the answer (it lands the order),
                    // every fourth after it (the answer's RESTING repeated).
                    let open = ExecEvent {
                        kind: EV_RESTING,
                        venue_ms: n,
                        ..row
                    };
                    if n % 4 == 0 {
                        assert!(gw.evt.emit(&open));
                    }
                    assert!(gw.evt.emit(&ExecEvent {
                        kind: EV_ACK,
                        flags: item.flags | EVF_RESTING,
                        oid: n,
                        ..item
                    }));
                    if n % 4 == 2 {
                        assert!(gw.evt.emit(&open));
                    }
                    s.recent[s.at % RECENT] = c.client_oid;
                    s.at += 1;
                    if n % 5 == 0 {
                        assert!(gw.evt.emit(&ExecEvent {
                            kind: EV_FILLED,
                            ..row
                        }));
                        if n % 7 != 0 {
                            s.held = Some(ExecEvent {
                                kind: EV_FILL,
                                px_1e8: c.px_1e8,
                                sz_1e8: c.sz_1e8,
                                venue_ms: n,
                                flags: if c.is_buy != 0 { EVF_BUY } else { 0 },
                                ..row
                            });
                        }
                    }
                }
                CMD_CANCEL if n % 3 == 0 => assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_REJECTED,
                    reason: REASON_NOT_FOUND,
                    ..item
                })),
                CMD_CANCEL => {
                    let ack = ExecEvent {
                        kind: EV_ACK,
                        ..item
                    };
                    let canceled = ExecEvent {
                        kind: EV_CANCELED,
                        reason: ORDER_EVENT_REASON_OTHER,
                        ..row
                    };
                    // Every other one the stream's row beats the answer.
                    let (a, b) = if n % 2 == 0 {
                        (canceled, ack)
                    } else {
                        (ack, canceled)
                    };
                    assert!(gw.evt.emit(&a));
                    assert!(gw.evt.emit(&b));
                }
                CMD_READ_STATE => {
                    s.sheets += 1;
                    if s.sheets % 5 == 0 {
                        assert!(gw.evt.emit(&ExecEvent {
                            kind: EV_READ_FAILED,
                            reason: READ_STATE,
                            ..end
                        }));
                    } else {
                        if !s.flat {
                            assert!(gw.evt.emit(&ExecEvent {
                                kind: EV_POSITION,
                                asset: ASSET,
                                sz_1e8: 50_000_000,
                                aux: 5_000_000_000,
                                ..row
                            }));
                        }
                        assert!(gw.evt.emit(&ExecEvent {
                            reason: READ_STATE,
                            px_1e8: 100_000_000_000,
                            venue_ms: n,
                            ..end
                        }));
                    }
                }
                CMD_READ_ORDERS => {
                    s.orders += 1;
                    let mut k = 0usize;
                    while k < RECENT {
                        if s.recent[k] != 0 {
                            assert!(gw.evt.emit(&ExecEvent {
                                kind: EV_OPEN_ORDER,
                                client_oid: s.recent[k],
                                aux: c.client_oid,
                                asset: ASSET,
                                slot: SLOT,
                                ..ExecEvent::default()
                            }));
                        }
                        k += 1;
                    }
                    if s.orders % 7 == 0 {
                        // An order the arm never placed.
                        assert!(gw.evt.emit(&ExecEvent {
                            kind: EV_OPEN_ORDER,
                            client_oid: (1 << 40) + n,
                            aux: c.client_oid,
                            asset: ASSET,
                            slot: SLOT,
                            ..ExecEvent::default()
                        }));
                    }
                    assert!(gw.evt.emit(&ExecEvent {
                        reason: READ_ORDERS,
                        ..end
                    }));
                }
                CMD_READ_LIMIT => {
                    s.limits += 1;
                    if s.limits % 9 == 0 {
                        assert!(gw.evt.emit(&ExecEvent {
                            kind: EV_READ_FAILED,
                            reason: READ_LIMIT,
                            ..end
                        }));
                    } else {
                        // 100 requests used of a $1M address's 1 010 000 —
                        // every 13th read 1 009 000: the floor (2 000)
                        // breached, and lifted by the next read. XMM
                        // XH5c-2: every seventh read 2 500 left — inside the
                        // band (2 000, 3 000]. The weight bought rides the
                        // surplus (the grant) and `used` follows it, so each
                        // read states the headroom it names; the band and
                        // the floor never while a purchase awaits its
                        // confirming read (the full headroom shows it).
                        let calm = s.calm > 0;
                        s.calm = s.calm.saturating_sub(1);
                        let left = if !calm && s.limits % 13 == 0 {
                            1_000
                        } else if !calm && s.limits % 7 == 0 {
                            2_500
                        } else {
                            1_009_900
                        };
                        assert!(gw.evt.emit(&ExecEvent {
                            reason: READ_LIMIT,
                            aux: 1_010_000 + s.surplus - left,
                            px_1e8: 100_000_000_000_000,
                            sz_1e8: s.surplus as i64,
                            ..end
                        }));
                    }
                }
                CMD_READ_DAY => {
                    s.days += 1;
                    // A full page that moves, then one that adds nothing
                    // past its start: two pages.
                    assert!(gw.evt.emit(&ExecEvent {
                        reason: READ_DAY,
                        aux: if s.days == 1 { 2_000 } else { 3 },
                        px_1e8: 1_000,
                        venue_ms: if s.days == 1 {
                            c.time_ms + 1
                        } else {
                            c.time_ms
                        },
                        ..end
                    }));
                }
                // XH5b-1: the worth's spot half, $500 USDC — every sixth
                // failing, every seventh listing no USDC.
                CMD_READ_SPOT => {
                    s.spots += 1;
                    let failed = s.spots % 6 == 0;
                    let listed = s.spots % 7 != 0;
                    assert!(gw.evt.emit(&ExecEvent {
                        kind: if failed { EV_READ_FAILED } else { EV_READ_DONE },
                        reason: READ_SPOT,
                        px_1e8: if listed { 50_000_000_000 } else { 0 },
                        sz_1e8: i64::from(listed),
                        ..end
                    }));
                }
                // XMM XH5c-2: a weight purchase — every third refused
                // unsent (throttled: the day's charge returned), else
                // accepted: its weight in the surplus from here on, the
                // next two reads calm; in the stop's window, refused by the
                // venue.
                CMD_RESERVE => {
                    s.reserves += 1;
                    let (kind, reason) = if s.refuse {
                        (EV_REJECTED, ORDER_EVENT_REASON_OTHER)
                    } else if s.reserves % 3 == 0 {
                        (EV_REJECTED, REASON_THROTTLED)
                    } else {
                        s.surplus += u64::from(c.arg);
                        s.calm = 2;
                        (EV_ACK, 0)
                    };
                    assert!(gw.evt.emit(&ExecEvent {
                        kind,
                        reason,
                        ..item
                    }));
                }
                // The dead man's posts.
                _ => assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_ACK,
                    ..item
                })),
            }
            if n % 97 == 0 {
                assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_GAP,
                    ..ExecEvent::default()
                }));
            }
            if n % 89 == 0 {
                // Another slot's order rests on the account.
                assert!(gw.evt.emit(&ExecEvent {
                    kind: EV_RESTING,
                    client_oid: n,
                    asset: ASSET,
                    slot: 3,
                    ..ExecEvent::default()
                }));
            }
        }
    }

    fn drain(arm: &mut HlGatewayArm, s: &mut Script, acc: &mut u64) {
        while let Some(f) = arm.try_next_fill() {
            *acc ^= f.order_id;
        }
        let mut ev = OrderEvent::ZERO;
        while arm.try_next_order_event(&mut ev) {
            s.replaced += u64::from(
                ev.kind == ORDER_EVENT_CANCELED && ev.reason == ORDER_EVENT_REASON_REPLACED,
            );
            // XMM XH5d: the script's `open` rows carry n — before the
            // answer when n ≡ 0 (mod 4), after it when n ≡ 2.
            let stamped = ev.kind == ORDER_EVENT_RESTING && ev.venue_time_ms != 0;
            s.stamped += u64::from(stamped && ev.venue_time_ms % 4 == 0);
            s.repeated += u64::from(stamped && ev.venue_time_ms % 4 == 2);
            *acc ^= ev.client_oid ^ u64::from(ev.kind);
        }
        while let Some(r) = arm.try_next_retired() {
            *acc ^= r.client_oid ^ u64::from(r.why);
        }
        // XH4f: a landed modify's rename (the queued-arm contract).
        while let Some(n) = arm.try_next_renamed() {
            *acc ^= n.client_oid ^ n.prev_client_oid;
        }
    }

    fn quote(cid: u64, side: Side) -> Order {
        let mut o = Order::new(
            0,
            core_types::VenueId::Hyperliquid,
            SYM,
            side,
            ORDER_KIND_MAKER,
            core_types::Price(100_000_000 + (cid % 7) as i64 * 10_000),
            core_types::Qty(500_000),
            cid,
        )
        .with_post_only()
        // XMM XH5d: every third bids ALO priority (1 bp).
        .with_alo_priority(if cid % 3 == 0 { 10_000 } else { 0 });
        o.strategy_id = SLOT;
        o
    }

    fn side_of(r: u64) -> Side {
        if r % 2 == 0 {
            Side::Bid
        } else {
            Side::Ask
        }
    }

    fn cancel_req(cid: u64) -> CancelReq {
        let mut q = CancelReq::new(0, core_types::VenueId::Hyperliquid, SYM, cid);
        q.strategy_id = SLOT;
        q
    }

    fn round(arm: &mut HlGatewayArm, gw: &mut HlGatewayEnd, r: u64, s: &mut Script, acc: &mut u64) {
        let cid = 10 * r + 10;
        *acc ^= u64::from(arm.submit(&quote(cid, side_of(r))).is_ok());
        if r % 3 == 1 {
            // The last round's order, replaced on its own side.
            let prev = 10 * (r - 1) + 10;
            if arm
                .modify(&ModifyReq::new(prev, quote(cid + 1, side_of(r - 1))))
                .is_ok()
            {
                s.modifies += 1;
            }
        }
        if r % 3 == 2 {
            *acc ^= u64::from(arm.cancel(&cancel_req(10 * (r - 1) + 11)).is_ok());
            *acc ^= u64::from(arm.cancel(&cancel_req(10 * (r - 2) + 10)).is_ok());
        }
        if r % 1_000 == 999 {
            *acc ^= u64::from(arm.cancel_all().is_ok());
            // The sweep's cancels, as the gateway notes them: the arm
            // charges them to the address.
            gw.stats.note_sweep_cancels(3);
            gw.stats.set_sweep(gw.stats.sweep_requested(), SWEEP_CLEAR);
        }
        answer(gw, s);
        let _ = arm.on_idle();
        drain(arm, s, acc);
        let sig = arm.halt_signal();
        *acc ^= sig.ws_gap_ns ^ u64::from(sig.alo_reject_permille);
        *acc ^= arm.halt_signal_for(0).ws_gap_ns;
        *acc ^= u64::from(arm.cancel_all_state() == CancelAllState::Clear);
    }

    /// The paths the window must run again, each counted.
    fn paths(c: &ArmCounters, s: &Script) -> [u64; 19] {
        [
            c.fills,
            c.alo_rejects,
            c.doubt_settled,
            c.reads,
            c.reads_failed,
            c.closed_unseen,
            c.join_timeouts,
            c.unknown_orders,
            c.fees_1e8.unsigned_abs(),
            s.replaced,
            s.modifies,
            c.limit_reads,
            c.throttled,
            c.refunded,
            c.refused_budget,
            c.spot_reads,
            c.spot_failed,
            s.stamped,
            s.repeated,
        ]
    }

    // ---- boot (NOT measured) ----
    let bind = PerpBind {
        sym: SYM,
        asset: ASSET,
        rules: PerpRules::new(4).expect("rules"),
    };
    let cfg = ArmCfg {
        slot: SLOT,
        perps: [bind; ARM_PERPS],
        n_perps: 1,
        expires_after_ms: 1,
        post_timeout_ms: 5_000,
        skew_ms: 0,
        recon_ms: 1,
        join_ms: 1,
        gone_ms: 1,
        shutdown_ms: 1_000,
        own_account: true,
        anchor_1e6: 0,
        deadman: DeadManCfg::DEFAULT,
        budget_floor: 2_000,
        limit_ms: 2,
        leverage: 0,
        outcomes: false,
    };
    let (eng, mut gw) = hl_duplex();
    let mut arm = HlGatewayArm::new(&cfg, eng).expect("the arm");
    // XH5b-2: the anchor's hand-over — a real `AnchorKeeper` stores it just
    // before the window, whose first pass reads it kept.
    let keep = std::sync::Arc::new(AnchorSlot::new());
    arm.keep_anchor_in(std::sync::Arc::clone(&keep));
    // XMM XH5c-2: top-ups armed — 1 000 requests a purchase, no day's max —
    // their state handed to a real `TopupKeeper` (built before the window).
    let topup_slot = std::sync::Arc::new(TopupSlot::new());
    arm.set_topup(
        TopupGovernor::restored(0, 0, 0).armed(1_000, u64::MAX),
        Some(std::sync::Arc::clone(&topup_slot)),
    )
    .expect("gate 98: top-ups on the slot's own account");
    // XMM XH5d: the ALO priority armed — its fee tallied on every place
    // that bids one, in the window.
    arm.set_priority_cap(u32::MAX);
    gw.stats.set_gauges(0, true);
    let mut s = Script {
        n: 0,
        sheets: 0,
        orders: 0,
        recent: [0; RECENT],
        at: 0,
        held: None,
        replaced: 0,
        modifies: 0,
        flat: true,
        limits: 0,
        days: 0,
        spots: 0,
        surplus: 0,
        reserves: 0,
        calm: 0,
        refuse: false,
        stamped: 0,
        repeated: 0,
    };
    let mut acc = 0u64;
    let _ = arm.on_idle();
    answer(&mut gw, &mut s);
    assert!(gw.evt.emit(&ExecEvent {
        kind: EV_SNAPSHOT,
        ..ExecEvent::default()
    }));
    let _ = arm.on_idle();
    gw.stats.set_sweep(gw.stats.sweep_requested(), SWEEP_CLEAR);
    let mut spins = 0u32;
    while !arm.ready() {
        let _ = arm.on_idle();
        answer(&mut gw, &mut s);
        spins += 1;
        assert!(spins < 1_000_000, "gate 98: the arm never booted");
    }
    assert!(!arm.boot_position(), "gate 98: the seeding sheet was flat");
    s.flat = false;
    // XH4e: the day's spend, read once a boot — a full page, then the next
    // a read period later. The arm is ready, so this is after init: its
    // own measured window.
    assert_eq!(
        arm.counters().day_pages,
        0,
        "gate 98: no day page answered before its window"
    );
    let g = AllocGuard::new();
    while arm.venue_day_bought(usize::from(SLOT)).is_none() {
        let _ = arm.on_idle();
        answer(&mut gw, &mut s);
        spins += 1;
        assert!(spins < 1_000_000, "gate 98: the day was never read");
    }
    let (day_allocs, day_bytes, _) = g.delta();
    assert_eq!(s.days, 2, "gate 98: a page that moved asks for the next");
    assert_eq!(
        arm.counters().day_pages,
        2,
        "gate 98: both pages in the window"
    );
    assert_eq!(
        day_allocs, 0,
        "the HL gateway arm's day read allocated {day_allocs} times ({day_bytes} B)"
    );

    // Warm-up (NOT measured): until every path has run.
    let mut r = 0u64;
    while r < 2_000 || paths(arm.counters(), &s).contains(&0) {
        round(&mut arm, &mut gw, r, &mut s, &mut acc);
        r += 1;
        assert!(
            r < 2_000_000,
            "gate 98: a path never ran: {:?}",
            paths(arm.counters(), &s)
        );
    }

    // ---- measurement window ----
    // XH5b-2: the slot's keeper stores the anchor the boot handed over —
    // once, before the window; in the window its pass runs every round
    // (done), beside a keeper nothing is handed to (nothing yet).
    let dir = std::env::temp_dir().join(format!("gate98-anchor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 98: a temp dir");
    let mut keeper = AnchorKeeper::new(
        std::sync::Arc::clone(&keep),
        dir.join("xmm-pnl-anchor-testnet.state"),
        [0x66; 20],
        std::sync::Arc::clone(&gw.stats),
    );
    let mut idle_keeper = AnchorKeeper::new(
        std::sync::Arc::new(AnchorSlot::new()),
        dir.join("never.state"),
        [0x66; 20],
        std::sync::Arc::clone(&gw.stats),
    );
    assert!(
        matches!(keeper.turn(), Kept::Stored(_)),
        "gate 98: the boot's worth was handed over and stored"
    );
    // XMM XH5c-2: the top-up keeper, the anchor's stamp handed to it as the
    // gateway's loop does; nothing handed over before the anchor is kept.
    let mut topup_keeper = TopupKeeper::new(
        std::sync::Arc::clone(&topup_slot),
        dir.join("xmm-topup-testnet.state"),
        [0x66; 20],
        0,
    )
    .expect("gate 98: the top-up keeper");
    topup_keeper.anchored_at(keeper.set_unix_s());
    assert_eq!(
        topup_keeper.turn(),
        Topped::Idle,
        "gate 98: no top-up before the anchor is kept"
    );
    let before = paths(arm.counters(), &s);
    let g = AllocGuard::new();
    let end = r + ROUNDS;
    // XMM XH5c-2: and on until a purchase was seen, one refused unsent, and
    // a decision's state was kept with no write.
    let mut unchanged = 0u64;
    while r < end
        || arm.counters().topup_ok == 0
        || arm.counters().topup_unsent == 0
        || unchanged == 0
    {
        round(&mut arm, &mut gw, r, &mut s, &mut acc);
        assert!(keeper.turn() == Kept::Idle && idle_keeper.turn() == Kept::Idle);
        match topup_keeper.turn() {
            Topped::Failed(_) => panic!("gate 98: the top-up state could not be stored"),
            Topped::Unchanged => unchanged += 1,
            Topped::Idle | Topped::Stored(_) => {}
        }
        r += 1;
        assert!(r < end + 2_000_000, "gate 98: the top-ups never ran");
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = g.delta();
    let after = paths(arm.counters(), &s);
    let mut k = 0usize;
    while k < after.len() {
        assert!(
            after[k] > before[k],
            "gate 98: path {k} did not run in the window: {before:?} → {after:?}"
        );
        k += 1;
    }
    assert_eq!(
        allocs, 0,
        "the HL gateway arm allocated {allocs} times ({bytes} B)"
    );
    let c = arm.counters();
    assert!(
        c.topup_ok > 0 && c.topup_unsent > 0 && c.topup_failed == 0 && s.reserves > 0,
        "gate 98: top-ups seen and refused unsent, never stopped: {} seen, {} unsent, {} \
         stopped",
        c.topup_ok,
        c.topup_unsent,
        c.topup_failed
    );
    eprintln!(
        "gate 98: {} rounds; top-ups {} seen, {} refused unsent, {} purchases answered, {} \
         decisions kept with no write",
        r - (end - ROUNDS),
        c.topup_ok,
        c.topup_unsent,
        s.reserves,
        unchanged
    );
    let mut rep = clob_dispatcher::GatewayArmReport::default();
    assert!(arm.gateway_arm_report(0, &mut rep));
    assert_eq!(
        rep.anchor_kept, 1,
        "gate 98: the window read the anchor kept"
    );
    assert!(
        rep.topup_weight == 1_000 && rep.topup_cost_usd_1e6 > 0,
        "gate 98: the spend of the purchases seen"
    );

    // XMM XH5c-2, measured alone: a purchase the venue refuses stops the
    // top-ups, and the stop — the day spent — is stored.
    s.refuse = true;
    let mut stored = None;
    let g = AllocGuard::new();
    while arm.counters().topup_failed == 0 || stored.is_none() {
        round(&mut arm, &mut gw, r, &mut s, &mut acc);
        if let Topped::Stored(st) = topup_keeper.turn() {
            if arm.counters().topup_failed > 0 {
                stored = Some(st);
            }
        }
        r += 1;
        assert!(r < end + 4_000_000, "gate 98: the top-ups never stopped");
    }
    std::hint::black_box(acc);
    let (stop_allocs, stop_bytes, _) = g.delta();
    assert_eq!(
        stored.map(|st| (st.used, st.cost_1e6 > 0)),
        Some((u64::MAX, true)),
        "gate 98: the stop stored as the day spent, with the session's spend"
    );
    assert_eq!(
        stop_allocs, 0,
        "the HL gateway arm's top-up stop allocated {stop_allocs} times ({stop_bytes} B)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Gate 96b's venue half: runs ONLY in the child process gate 96b spawns
/// (it is `#[ignore]`d, and a no-op without `GATE96_VENUE_DIR`). A
/// scripted Hyperliquid (`exec_hyperliquid::testvenue`, which sends the
/// empty `userFills` snapshot on subscribe) that answers the ALO post
/// with one resting status and one post-only cross, then pushes one
/// `orderUpdates` row and one `userFills` row carrying our cloids — each
/// its own TLS record — answers the modify with a resting status and the
/// cancel with `success`, and the two reads (XH4d) with the account's
/// open orders (one ours, one a stranger's) and its perp sheet (one
/// position); it writes its port and certificate for the parent. XH4i-2:
/// with `GATE96_OUTCOMES=1` (the outcome variant) the orders, the fills
/// and the day's fills are on the HIP-4 leg `#1941` instead of ETH, and
/// the spot sheet also holds that leg as `+1941` — beside a settled husk
/// (`+1942`) and a name that does not parse (`+0195`).
#[test]
#[ignore = "gate 96b's child process; never run on its own"]
fn gate96_venue_helper() {
    use exec_hyperliquid::cloid::{encode, to_hex};
    use exec_hyperliquid::testvenue::{Answer, Venue};
    let Some(dir) = std::env::var_os("GATE96_VENUE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let outcomes = std::env::var_os("GATE96_OUTCOMES").is_some_and(|v| v == "1");
    let coin = if outcomes { "#1941" } else { "ETH" };
    let leg = if outcomes {
        r#",{"coin":"+1941","token":2,"total":"25.0","hold":"0.0","entryNtl":"10.25"},{"coin":"+1942","token":3,"total":"0.0","hold":"0.0"},{"coin":"+0195","token":4,"total":"3.0","hold":"0.0","entryNtl":"1.2"}"#
    } else {
        ""
    };
    let v = Venue::start();
    let push = v.pusher();
    let mut k = 0u64;
    v.policy(move |s| {
        k += 1;
        if s.body.contains("frontendOpenOrders") {
            let mut hex = [0u8; 34];
            to_hex(&encode(6, k), &mut hex);
            let c = String::from_utf8_lossy(&hex).to_string();
            return Answer::Info(format!(
                r#"{{"type":"frontendOpenOrders","data":[{{"coin":"{coin}","limitPx":"2500.0","oid":{k},"origSz":"0.01","side":"B","sz":"0.01","timestamp":1,"isTrigger":false,"cloid":"{c}"}},{{"coin":"{coin}","limitPx":"2600.0","oid":7,"origSz":"0.01","side":"A","sz":"0.01","timestamp":1,"isTrigger":false}}]}}"#
            ));
        }
        // XH5b-1: the spot sheet (XH4i-2: and the leg, in the outcome
        // variant).
        if s.body.contains("spotClearinghouseState") {
            return Answer::Info(format!(
                r#"{{"type":"spotClearinghouseState","data":{{"balances":[{{"coin":"USDC","token":0,"total":"250.25","hold":"0.0","entryNtl":"0.0"}},{{"coin":"HYPE","token":150,"total":"1.5","hold":"0.0","entryNtl":"40.0"}}{leg}]}}}}"#
            ));
        }
        if s.body.contains("userRateLimit") {
            return Answer::Info(
                r#"{"type":"userRateLimit","data":{"cumVlm":"1500.5","nRequestsUsed":2890,"nRequestsCap":11500,"nRequestsSurplus":7}}"#
                    .to_string(),
            );
        }
        if s.body.contains("userFillsByTime") {
            let mut hex = [0u8; 34];
            to_hex(&encode(6, k), &mut hex);
            let c = String::from_utf8_lossy(&hex).to_string();
            return Answer::Info(format!(
                r#"{{"type":"userFillsByTime","data":[{{"coin":"{coin}","px":"2500","sz":"0.01","side":"B","time":1700000000001,"dir":"Open Long","oid":{k},"crossed":false,"fee":"0.0001","tid":{k},"cloid":"{c}"}},{{"coin":"{coin}","px":"2500","sz":"0.01","side":"A","time":1700000000002,"dir":"Close Long","oid":{k},"crossed":true,"fee":"0.0001","tid":{k},"cloid":"{c}"}}]}}"#
            ));
        }
        if s.body.contains("clearinghouseState") {
            return Answer::Info(
                r#"{"type":"clearinghouseState","data":{"marginSummary":{"accountValue":"1000.5","totalNtlPos":"25.0","totalRawUsd":"975.5","totalMarginUsed":"2.5"},"crossMarginSummary":{"accountValue":"1000.5","totalNtlPos":"25.0","totalRawUsd":"975.5","totalMarginUsed":"2.5"},"crossMaintenanceMarginUsed":"0.5","withdrawable":"998.0","assetPositions":[{"type":"oneWay","position":{"coin":"ETH","entryPx":"2500.0","szi":"0.01","positionValue":"25.0","unrealizedPnl":"0.0"}}],"time":1790000000000}}"#
                    .to_string(),
            );
        }
        if s.body.contains("cancelByCloid") {
            return Answer::Action(
                r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#
                    .to_string(),
            );
        }
        // XMM XH5c-2: a weight purchase — a control action's answer.
        if s.body.contains("reserveRequestWeight") {
            return Answer::Action(r#"{"status":"ok","response":{"type":"default"}}"#.to_string());
        }
        if s.body.contains("batchModify") {
            return Answer::Action(format!(
                r#"{{"status":"ok","response":{{"type":"order","data":{{"statuses":[{{"resting":{{"oid":{k}}}}}]}}}}}}"#
            ));
        }
        let mut hex = [0u8; 34];
        to_hex(&encode(6, k), &mut hex);
        let c = String::from_utf8_lossy(&hex).to_string();
        push.push_all(&format!(
            r#"{{"channel":"orderUpdates","data":[{{"order":{{"coin":"{coin}","side":"B","limitPx":"2500","sz":"0.01","oid":{k},"timestamp":1,"origSz":"0.01","cloid":"{c}"}},"status":"open","statusTimestamp":{k}}}]}}"#
        ));
        push.push_all(&format!(
            r#"{{"channel":"userFills","data":{{"user":"0x","fills":[{{"coin":"{coin}","px":"2500","sz":"0.01","side":"B","time":1700000000000,"dir":"Open Long","oid":{k},"crossed":false,"fee":"-0.0001","tid":{k},"cloid":"{c}"}}]}}}}"#
        ));
        Answer::Action(format!(
            r#"{{"status":"ok","response":{{"type":"order","data":{{"statuses":[{{"resting":{{"oid":{k}}}}},{{"error":"Post only order would have immediately matched, bbo was 1@2. asset=1"}}]}}}}}}"#
        ))
    });
    std::fs::write(dir.join("cert.der"), &v.cert_der).expect("gate 96 cert");
    // The port last: its presence says the rest is written.
    std::fs::write(dir.join("port.tmp"), v.port.to_string()).expect("gate 96 port");
    std::fs::rename(dir.join("port.tmp"), dir.join("port")).expect("gate 96 port");
    // HK4 (gate 114): once the parent writes `refuse_ws` into the dir, every
    // WebSocket upgrade is refused — the parent's posts go REST.
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(120) {
        if dir.join("refuse_ws").exists() {
            v.refuse_ws(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **XMM gate 96b (XH4b) — the HL gateway's whole turn, over a real
/// rustls loopback, allocates only rustls' record buffers.**
///
/// The REAL gateway (`HlGateway::turn`) against a scripted venue in a
/// CHILD process (the counting allocator is process-global — gate 72's
/// harness). Every round pushes two ALO places (XMM XH5d: both bidding
/// priority 1 bp — one batch, its `{"p": N}` grouping sealed and rendered
/// in place), an ALO modify and a
/// cancel, which the gateway coalesces into THREE posts (the ALO batch,
/// the batchModify, the cancelByCloid) — each encoded, signed, rendered
/// and framed in place — and two reads (XH4d: the open orders, the
/// sheet), each posted alone, unsigned; the venue answers them (one item
/// resting, one a post-only cross; the modify resting; the cancel
/// `success`; the reads' rows) and pushes an `orderUpdates` row and a
/// `userFills` row carrying our cloids; the gateway maps every status
/// onto its item, decodes the cloids, scans both streams and both reads
/// in place, dedupes and emits — 3 ACKs, a BAD_ALO_PX rejection,
/// RESTING, FILL, FILL_META, two open orders, a position and two read
/// ends. Pinned like gate 72: rustls 0.23's buffered API allocates one
/// `Vec` per record it seals (five posts) and one per application-data
/// record it opens (five answers, two pushes) — exactly 12 per round
/// while every inbound burst stays under
/// rustls' 4 KiB receive window (a larger burst grows and shrinks that
/// window: more). rustls' decrypted-record queue grows ONCE per
/// connection, the first time seven records wait in it together: the
/// warm-up forces that (a round whose answers are all in before the
/// gateway reads), so it never lands in the window by chance. Everything
/// else in the round — admission, pending, deadlines,
/// coalescing, msgpack, the signature, the JSON, the frame, the scans,
/// the dedupe rings, the events — is 0 B: a regression in the gateway
/// shows as a thirteenth. After the rounds, measured alone, XH4e's two
/// reads: the address budget (`userRateLimit`) and the day's fills
/// (`userFillsByTime`, its rows summed in place — no row table), and
/// XH5b-1's spot sheet (`spotClearinghouseState`, scanned into a fixed
/// table), each posted alone, unsigned, and answered by one end — exactly
/// 6 allocations, rustls' again (three records sealed, three opened); no sweep
/// has been asked yet, so none can land in that window. XH4f (D9), measured
/// alone: two ALO places carrying a 50 ms TTL (one post: one rests, one
/// crosses; the helper pushes an `orderUpdates` row and a fill), then at the
/// next beat past it the gateway's own TTL cancel of the resting one —
/// untimed, its success read `EXPIRED` — with the RTT window published at
/// every beat inside the window (pinned by `hl_gateway_loopback`): exactly
/// 6, rustls' (two sealed, four opened). XMM XH5c-2, measured alone: one
/// weight purchase (`reserveRequestWeight`, a control action — encoded,
/// signed, rendered and framed in place, posted alone), its `default`
/// answer read back as the arm's acknowledgement: exactly 2, rustls' (one
/// sealed, one opened). Then, measured
/// alone, one round of the cancel-all word's sweep (XH4d, XG-1): its own
/// read (answered with one order of ours and a stranger's, scanned in
/// place, emitting nothing), the cancel of ours by cloid, the cancel's
/// answer back to the engine — exactly 4 again. The residue goes with the
/// rustls record-layer pass (TL4/TL5b).
#[test]
fn hl_gateway_round_trip_allocates_only_rustls_record_buffers() {
    gate96b(false);
}

/// **XMM gate 96b, the outcome variant (XH4i-2).** The same turn, the same
/// pins, on a gateway whose coin map names HIP-4 outcome legs
/// (`CoinMap::with_outcomes`, BIN15's) against a venue whose orders,
/// fills and day's fills are on the leg `#1941` and whose spot sheet holds
/// it as `+1941` (beside a husk and a name that does not parse): every
/// `#1941` row resolved in place to its id (five events a round), the spot
/// read's two `EV_BALANCE` rows and its marked end, and the sweep's cancel
/// naming the leg — the gateway's outcome paths (XH4i-1, -2) at rustls'
/// exact residue, nothing of their own.
#[test]
fn hl_gateway_outcome_round_trip_allocates_only_rustls_record_buffers() {
    gate96b(true);
}

/// Gate 96b's harness, shared with gate 114 (HK4): the scripted venue in a
/// CHILD process (`gate96_venue_helper`; a `refuse_ws` file written into
/// the dir makes it refuse every WebSocket upgrade from then on), and the
/// REAL gateway booted against it as the engine boots it — the agent's
/// shared nonce stream included (XH4h S3). The caller kills the child and
/// removes the dir.
fn gate96_boot(
    tag: &str,
    outcomes: bool,
) -> (
    std::process::Child,
    std::path::PathBuf,
    exec_hyperliquid::gateway::HlGateway,
    exec_hyperliquid::gateway::HlEngineEnd,
) {
    use exec_gateway::Idle;
    use exec_hyperliquid::gateway::{GatewayCfg, HlGateway};
    use exec_hyperliquid::{hl_duplex, CoinMap, Network};

    let dir = std::env::temp_dir().join(format!(
        "mv-{tag}-{}-{}",
        std::process::id(),
        u8::from(outcomes)
    ));
    std::fs::create_dir_all(&dir).expect("gate 96 dir");
    let child = std::process::Command::new(std::env::current_exe().expect("gate 96 exe"))
        .args([
            "gate96_venue_helper",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GATE96_VENUE_DIR", &dir)
        .env("GATE96_OUTCOMES", if outcomes { "1" } else { "0" })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 96 child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 96 port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 96: the venue never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 96 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 96 anchor");
    let tls = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let (eng, gw_end) = hl_duplex();
    let mut coins = if outcomes {
        CoinMap::with_outcomes()
    } else {
        CoinMap::new()
    };
    assert!(coins.insert(b"ETH", 1));
    let cfg = GatewayCfg {
        host: "localhost".to_string(),
        port,
        tls,
        network: Network::Testnet,
        master: [0xA5; 20],
        slots: 1 << 6,
        post_timeout_ns: 5_000_000_000,
        idle: Idle::new(0, 200_000),
        // The gate measures allocations: its rounds must never throttle.
        ip_weight_per_min: u32::MAX,
        ws_msgs_per_min: u32::MAX,
    };
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("gate 96 key");
    let mut gw = HlGateway::new(cfg, gw_end, sk, coins).expect("gate 96 gateway");
    // XH4h S3: as the engine boots it — the agent's shared nonce stream.
    gw.share_nonce(
        exec_hyperliquid::NonceStreams::new()
            .stream(&[0x58; 20])
            .expect("a stream"),
    );
    (child, dir, gw, eng)
}

/// Gate 96b, on a perps-only map (`outcomes` false, slot 6's) or one that
/// names outcome legs.
fn gate96b(outcomes: bool) {
    use exec_hyperliquid::event::{
        ExecEvent, EV_ACK, EV_BALANCE, EV_FILL, EV_FILL_META, EV_OPEN_ORDER, EV_POSITION,
        EV_READ_DONE, EV_REJECTED, EV_RESTING, EV_SNAPSHOT, READ_DAY, READ_LIMIT, READ_SPOT,
    };
    use exec_hyperliquid::gateway::{HlEngineEnd, HlGateway};
    use exec_hyperliquid::{HlCmd, Tif};

    /// XH4i: the outcome variant's leg, `#1941` / `+1941`.
    const LEG: u32 = exec_hyperliquid::asset::ASSET_BASE + 1941;
    const ROUNDS: u64 = 300;
    /// Signed posts a round (the reads are posted, unsigned, uncounted).
    const POSTS_PER_ROUND: u64 = 3;
    const EVENTS_PER_ROUND: u64 = 12;
    const RUSTLS_ALLOCS_PER_ROUND: u64 = 12;
    /// One sweep round: the read and the cancel sealed, their answers
    /// opened.
    const RUSTLS_ALLOCS_PER_SWEEP_ROUND: u64 = 4;
    /// XH4e: the budget and day reads, and XH5b-1's spot sheet — each
    /// sealed, each answer opened.
    const RUSTLS_ALLOCS_PER_ACCOUNT_READS: u64 = 6;
    /// XH4f: the places' post and the TTL cancel sealed; the places'
    /// answer, the two pushes and the cancel's answer opened.
    const RUSTLS_ALLOCS_PER_TTL: u64 = 6;
    /// XMM XH5c-2: the purchase sealed, its answer opened.
    const RUSTLS_ALLOCS_PER_RESERVE: u64 = 2;
    /// XMM XH5c-2: the purchase's client id (the arm's range: bit 63 set,
    /// bit 62 clear, bit 60).
    const RESERVE_OID: u64 = (1 << 63) | (1 << 60) | 1;
    let (mut child, dir, mut gw, mut eng) = gate96_boot("gate96", outcomes);
    let mut kinds = [0u64; 256];
    let t0 = std::time::Instant::now();
    while !gw.ws_live() || kinds[usize::from(EV_SNAPSHOT)] == 0 {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "gate 96: the socket never came up with its snapshot"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            kinds[usize::from(e.kind)] += 1;
        }
    }

    // One round: two ALO places, a modify and a cancel of orders from no
    // open batch — three posts; their answers and the two pushes back.
    // `named` counts the events naming the outcome variant's leg (XH4i).
    let mut oid = 0u64;
    let mut named = 0u64;
    let mut round = |gw: &mut HlGateway,
                     eng: &mut HlEngineEnd,
                     kinds: &mut [u64; 256],
                     named: &mut u64,
                     stall: bool| {
        oid += 3;
        let sent = eng.stats.io()[0];
        let cmds = [
            HlCmd::place(6, oid, 1, true, 250_000_000_000, 1_000_000, Tif::Alo)
                .with_priority(10_000),
            HlCmd::place(6, oid + 1, 1, false, 251_000_000_000, 1_000_000, Tif::Alo)
                .with_priority(10_000),
            HlCmd::place(6, oid + 2, 1, true, 249_000_000_000, 1_000_000, Tif::Alo)
                .replacing(oid + 1_000_000),
            HlCmd::cancel(6, oid + 2_000_000, 1),
            HlCmd::read_orders(6, oid + 3_000_000),
            HlCmd::read_state(6, oid + 4_000_000),
        ];
        let mut j = 0usize;
        while j < cmds.len() {
            assert!(eng.cmd.push(&cmds[j]));
            j += 1;
        }
        let mut got = 0u64;
        let t = std::time::Instant::now();
        if stall {
            // Write the posts, then let every answer and push arrive
            // before the next read: rustls decrypts all seven
            // records at once and its record queue reaches its
            // high-water mark here, in the warm-up.
            while eng.stats.io()[0] < sent + POSTS_PER_ROUND {
                assert!(
                    t.elapsed() < std::time::Duration::from_secs(5),
                    "gate 96: posts not written"
                );
                gw.turn();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        while got < EVENTS_PER_ROUND {
            assert!(
                t.elapsed() < std::time::Duration::from_secs(5),
                "gate 96: a round stalled at {got}/{EVENTS_PER_ROUND} events"
            );
            gw.turn();
            while let Some(e) = eng.evt.pop() {
                let ev: &ExecEvent = &e;
                kinds[usize::from(ev.kind)] += 1;
                *named += u64::from(ev.asset == LEG);
                got += 1;
            }
        }
    };
    let mut w = 0;
    while w < 50 {
        // The handshake's tail, session tickets and rustls' queues growing
        // to their working size: the cold part (the first round stalls).
        round(&mut gw, &mut eng, &mut kinds, &mut named, w == 0);
        w += 1;
    }
    let warm = kinds;
    let named_warm = named;
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        round(&mut gw, &mut eng, &mut kinds, &mut named, false);
        r += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let window = kinds;
    let named = named - named_warm;
    let posts = eng.stats.io()[0];

    // XH4e, measured alone — before any sweep is asked, so no sweep round
    // can land in the window: the budget read, the day read and XH5b-1's
    // spot sheet, each posted alone, unsigned; their answers scanned in
    // place into one end each.
    let sweeps0 = gw.counters().sweep_rounds;
    let mut ends = [(0u8, 0u64, 0i64); 3];
    let mut ne = 0usize;
    // XH4i-2: the spot read's rows (the leg, the unparsed one), the count
    // its end says, and its mark.
    let (mut legs, mut unknown, mut legs_said, mut mark) = (0u64, 0u64, 0u64, 0u32);
    let g = AllocGuard::new();
    assert!(eng.cmd.push(&HlCmd::read_limit(6, 9_000_001)));
    assert!(eng.cmd.push(&HlCmd::read_day(
        6,
        9_000_002,
        1_700_000_000_000,
        1_700_000_600_000
    )));
    assert!(eng.cmd.push(&HlCmd::read_spot(6, 9_000_003)));
    let t = std::time::Instant::now();
    while ne < 3 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "gate 96: the budget, day and spot reads never answered"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            if e.kind == EV_READ_DONE && ne < 3 {
                ends[ne] = (e.reason, e.aux, e.px_1e8);
                ne += 1;
                if e.reason == READ_SPOT {
                    legs_said = e.oid;
                    mark = e.asset;
                }
            }
            unknown += u64::from(
                e.kind == EV_BALANCE && e.asset == exec_hyperliquid::event::ASSET_UNKNOWN,
            );
            legs += u64::from(e.kind == EV_BALANCE && e.asset == LEG && e.sz_1e8 == 2_500_000_000);
            kinds[usize::from(e.kind)] += 1;
        }
    }
    let (read_allocs, read_bytes, _) = g.delta();
    assert_eq!(
        gw.counters().sweep_rounds,
        sweeps0,
        "gate 96: no sweep round in the reads' window"
    );

    // XH4f (D9), measured alone — still before any sweep is asked: two ALO
    // places with a 50 ms TTL — the crossing one refused well inside it —
    // the resting one's TTL cancel at the beat after it.
    let (ttl_posts0, ttl0) = (eng.stats.io()[0], gw.counters().ttl_cancels);
    let mut ttl_places = [
        HlCmd::place(6, 8_000_001, 1, true, 250_000_000_000, 1_000_000, Tif::Alo),
        HlCmd::place(6, 8_000_002, 1, false, 251_000_000_000, 1_000_000, Tif::Alo),
    ];
    ttl_places[0].time_ms = 50;
    ttl_places[1].time_ms = 50;
    let mut expired = 0u64;
    let g = AllocGuard::new();
    assert!(eng.cmd.push(&ttl_places[0]));
    assert!(eng.cmd.push(&ttl_places[1]));
    let t = std::time::Instant::now();
    while expired == 0 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "gate 96: the TTL never fired"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            if e.kind == EV_ACK && e.reason == core_types::ORDER_EVENT_REASON_EXPIRED {
                expired += 1;
            }
            kinds[usize::from(e.kind)] += 1;
        }
    }
    let (ttl_allocs, ttl_bytes, _) = g.delta();
    let ttl = (
        gw.counters().ttl_cancels - ttl0,
        eng.stats.io()[0] - ttl_posts0,
        gw.counters().sweep_rounds - sweeps0,
    );

    // XMM XH5c-2, measured alone — still before any sweep is asked: one
    // weight purchase, posted alone and signed; its answer the arm's ACK.
    let reserve_posts0 = eng.stats.io()[0];
    let mut bought = 0u64;
    let g = AllocGuard::new();
    assert!(eng.cmd.push(&HlCmd::reserve(6, RESERVE_OID, 10_000)));
    let t = std::time::Instant::now();
    while bought == 0 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "gate 96: the weight purchase never answered"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            if e.kind == EV_ACK && e.client_oid == RESERVE_OID {
                bought += 1;
            }
            kinds[usize::from(e.kind)] += 1;
        }
    }
    let (reserve_allocs, reserve_bytes, _) = g.delta();
    let reserve = (
        eng.stats.io()[0] - reserve_posts0,
        gw.counters().sweep_rounds - sweeps0,
    );
    let posts_before_sweep = eng.stats.io()[0];

    // The cancel-all word, measured alone: one sweep round — the read,
    // the cancel of ours, its answer — then the sweep settles (1 s).
    let (rounds0, cancels0) = (gw.counters().sweep_rounds, gw.counters().sweep_cancels);
    let acks = kinds[usize::from(EV_ACK)];
    let mut swept = 0u32;
    let g = AllocGuard::new();
    eng.stats.request_sweep();
    let t = std::time::Instant::now();
    while kinds[usize::from(EV_ACK)] == acks {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "gate 96: the sweep never cancelled ours"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            if e.kind == EV_ACK {
                // The cancel's item carries the asset the sweep named.
                swept = e.asset;
            }
            kinds[usize::from(e.kind)] += 1;
        }
    }
    let (sweep_allocs, sweep_bytes, _) = g.delta();
    let sweep = (
        gw.counters().sweep_rounds - rounds0,
        gw.counters().sweep_cancels - cancels0,
        eng.stats.io()[0] - posts_before_sweep,
    );

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    let n = |k: u8| window[usize::from(k)] - warm[usize::from(k)];
    assert_eq!(
        n(EV_ACK),
        ROUNDS * 3,
        "the resting place, the modify and the cancel"
    );
    assert_eq!(n(EV_REJECTED), ROUNDS, "the post-only cross, item by item");
    assert_eq!(n(EV_RESTING), ROUNDS, "every orderUpdates row routed");
    assert_eq!(
        (n(EV_FILL), n(EV_FILL_META)),
        (ROUNDS, ROUNDS),
        "every fill routed with its fee"
    );
    assert_eq!(
        (n(EV_OPEN_ORDER), n(EV_POSITION), n(EV_READ_DONE)),
        (2 * ROUNDS, ROUNDS, 2 * ROUNDS),
        "both reads scanned in place, every row and every end"
    );
    assert_eq!(
        posts,
        POSTS_PER_ROUND * (50 + ROUNDS),
        "three posts a round, no REST, no retry"
    );
    assert_eq!(gw.swaps(), 0, "one connection throughout");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_ROUND * ROUNDS,
        "HL gateway round: {allocs} allocations ({bytes} B) over {ROUNDS} rounds — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_ROUND}/round (five sealed \
         records out, seven opened in); anything above is the gateway's"
    );
    assert_eq!(
        sweep,
        (1, 1, 1),
        "one sweep round: its read, one cancel of ours (the one signed post)"
    );
    assert_eq!(
        sweep_allocs, RUSTLS_ALLOCS_PER_SWEEP_ROUND,
        "HL gateway sweep round: {sweep_allocs} allocations ({sweep_bytes} B) — rustls' \
         buffered API accounts for exactly {RUSTLS_ALLOCS_PER_SWEEP_ROUND} (two sealed, two \
         opened); anything above is the gateway's"
    );
    assert_eq!(
        ends,
        [
            (READ_LIMIT, 2_890, 150_050_000_000),
            (READ_DAY, 2, 5_000_000_000),
            (READ_SPOT, 2 + 3 * u64::from(outcomes), 25_025_000_000)
        ],
        "the budget's figures; the day's two rows of ours, $50; the spot sheet's USDC, \
         $250.25 over two rows (five with the outcome variant's)"
    );
    // XH4i: the outcome paths ran in the windows — or, on slot 6's map,
    // never.
    let o = u64::from(outcomes);
    assert_eq!(
        named,
        5 * ROUNDS * o,
        "a round's fill, its fee, the orderUpdates row and both open orders name the leg"
    );
    assert_eq!(
        (legs, unknown, legs_said, mark),
        (
            o,
            o,
            2 * o,
            if outcomes {
                exec_hyperliquid::event::SPOT_LEGS_READ
            } else {
                0
            }
        ),
        "the spot read's two held rows (the husk none), its end's count, its mark"
    );
    assert_eq!(
        swept,
        if outcomes { LEG } else { 1 },
        "the sweep's cancel names the leg off the venue's `#1941`"
    );
    assert_eq!(
        read_allocs, RUSTLS_ALLOCS_PER_ACCOUNT_READS,
        "HL gateway budget, day and spot reads: {read_allocs} allocations ({read_bytes} B) \
         — rustls' buffered API accounts for exactly {RUSTLS_ALLOCS_PER_ACCOUNT_READS} \
         (three sealed, three opened); anything above is the gateway's"
    );
    assert_eq!(
        ttl,
        (1, 2, 0),
        "one TTL cancel: the places' post and the cancel's, no sweep"
    );
    assert_eq!(
        ttl_allocs, RUSTLS_ALLOCS_PER_TTL,
        "HL gateway TTL: {ttl_allocs} allocations ({ttl_bytes} B) — rustls' buffered API \
         accounts for exactly {RUSTLS_ALLOCS_PER_TTL} (two sealed, four opened); anything \
         above is the gateway's"
    );
    assert_eq!(
        reserve,
        (1, 0),
        "one weight purchase, posted alone; no sweep"
    );
    assert_eq!(
        reserve_allocs, RUSTLS_ALLOCS_PER_RESERVE,
        "HL gateway weight purchase: {reserve_allocs} allocations ({reserve_bytes} B) — \
         rustls' buffered API accounts for exactly {RUSTLS_ALLOCS_PER_RESERVE} (one sealed, \
         one opened); anything above is the gateway's"
    );
}

/// **HK4 gate 114 — the HL gateway's REST path allocates only rustls'
/// record buffers.** Gate 96b's harness: the gateway goes live on the
/// WebSocket (its fills snapshot in — before it, growth is refused), then
/// the venue refuses every upgrade and the socket dies under a post's
/// write (the test seam `kill_ws_after_next_send`, `testvenue` only): that
/// post is re-sent by REST, and from then on every post goes REST — sealed
/// straight into the REST client's `/exchange` window (XMM HD2: no copy),
/// posted on its one keep-alive connection, the answer mapped onto the
/// items and emitted. Every round: gate 96b's two ALO places (one bid
/// resting, one post-only cross), pushed together — counted by what the
/// gateway posts, not assumed batched. Pinned like gate 101 (`HlHttp`, 2 a
/// post): rustls seals one record and opens one — exactly 2 a REST post;
/// everything the gateway does around them is 0 B, and a regression there
/// shows as one more. The gateway keeps redialling the refused socket on
/// its backoff (a dial builds its TLS session: cold, and it allocates), so
/// a window is judged only when no connection is between its dial and
/// Ready as it opens (`HlGateway::dialing`) and no dial started inside it
/// (`dials` unchanged); any other window is measured again.
#[test]
fn hl_gateway_rest_path_allocates_only_rustls_record_buffers() {
    use exec_hyperliquid::event::{EV_ACK, EV_REJECTED, EV_SNAPSHOT};
    use exec_hyperliquid::gateway::{HlEngineEnd, HlGateway};
    use exec_hyperliquid::{HlCmd, Tif};

    const ROUNDS: u64 = 20;
    /// One record sealed, one opened, per REST post.
    const RUSTLS_ALLOCS_PER_REST_POST: u64 = 2;
    /// `io()`'s REST-fallback slot.
    const IO_REST_POSTS: usize = 4;
    let (mut child, dir, mut gw, mut eng) = gate96_boot("gate114", false);
    let t0 = std::time::Instant::now();
    let mut snapshot = false;
    while !gw.ws_live() || !snapshot {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "gate 114: the socket never came up with its snapshot"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            snapshot |= e.kind == EV_SNAPSHOT;
        }
    }
    // From now on the venue refuses every upgrade; the live socket dies
    // under the next post's write.
    std::fs::write(dir.join("refuse_ws"), b"1").expect("gate 114 flag");
    std::thread::sleep(std::time::Duration::from_millis(50));
    gw.kill_ws_after_next_send();
    let mut oid = 0u64;
    let mut answered = 0u64;
    let mut round = |gw: &mut HlGateway, eng: &mut HlEngineEnd, answered: &mut u64| {
        oid += 2;
        let want = *answered + 2;
        assert!(eng.cmd.push(&HlCmd::place(
            6,
            oid,
            1,
            true,
            250_000_000_000,
            1_000_000,
            Tif::Alo
        )));
        assert!(eng.cmd.push(&HlCmd::place(
            6,
            oid + 1,
            1,
            false,
            251_000_000_000,
            1_000_000,
            Tif::Alo
        )));
        let t = std::time::Instant::now();
        while *answered < want {
            assert!(
                t.elapsed() < std::time::Duration::from_secs(5),
                "gate 114: a round stalled at {answered}/{want}"
            );
            gw.turn();
            while let Some(e) = eng.evt.pop() {
                *answered += u64::from(e.kind == EV_ACK || e.kind == EV_REJECTED);
            }
        }
    };
    // The killed post (re-sent by REST), then the warm-up: the REST
    // connection's dial and handshake, session tickets, rustls' queues at
    // their working size.
    let mut w = 0;
    while w < 20 {
        round(&mut gw, &mut eng, &mut answered);
        w += 1;
    }
    assert!(!gw.ws_live(), "gate 114: no socket may come back");
    assert!(eng.stats.io()[IO_REST_POSTS] >= 20, "gate 114: the warm-up went REST");
    let mut tries = 0u32;
    let (allocs, bytes, posts) = loop {
        tries += 1;
        assert!(tries <= 50, "gate 114: no dial-free window in 50 tries");
        if gw.dialing() {
            // A dial is still out: let it end (refused) before measuring.
            gw.turn();
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        let dials = gw.counters().dials;
        let posts0 = eng.stats.io()[IO_REST_POSTS];
        let g = AllocGuard::new();
        let mut r = 0u64;
        while r < ROUNDS {
            round(&mut gw, &mut eng, &mut answered);
            r += 1;
        }
        let (a, b, _) = g.delta();
        if gw.counters().dials == dials {
            break (a, b, eng.stats.io()[IO_REST_POSTS] - posts0);
        }
    };
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(posts >= ROUNDS, "gate 114: every round posted ({posts} REST posts)");
    assert_eq!(
        allocs,
        RUSTLS_ALLOCS_PER_REST_POST * posts,
        "HL gateway REST path: {allocs} allocations ({bytes} B) over {posts} REST posts — \
         rustls' buffered API accounts for exactly {RUSTLS_ALLOCS_PER_REST_POST} a post (one \
         sealed, one opened); anything above is the gateway's"
    );
}

/// **BX7 gate 104 — the REST order route and HMAC are 0 B/op.** Everything
/// the gateway does per request on `order_transport = "rest"` that is not
/// the transport (gate 87b measures `HttpsConn`): the place, the modify,
/// the cancel and the query by client id, a stray's cancel by the venue's
/// order id and the account read rendered in place — the client id and
/// the numbers IN the window — then signed in place by HMAC-SHA256 (R22)
/// and, for a share, by Ed25519; the answers read as the WS API's (a 200's
/// order, a refusal's code, a 5xx with none). Built once outside the
/// guard: the windows, the signers.
#[test]
fn binance_rest_route_render_sign_and_read_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::config::{HmacSigner, Signer};
    use exec_binance::rest::{render_account, render_by_cid, render_by_order_id, render_modify, render_place, QueryWriter};
    use exec_binance::wsapi::{scan_rest_answer, Part, Place, WsAnswer, WsStatic};

    const N: u64 = 20_000;
    const PLACED: &[u8] = br#"{"orderId":8001,"symbol":"BTCUSDT","status":"NEW","clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.0","avgPrice":"0.00","origQty":"0.002","executedQty":"0","cumQuote":"0","timeInForce":"GTX","type":"LIMIT","side":"BUY","updateTime":1790000000000}"#;
    const REFUSED: &[u8] = br#"{"code":-2019,"msg":"Margin is insufficient."}"#;

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 1_000);
    let hmac = Signer::Hmac(
        HmacSigner::from_secret(*b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j").expect("gate 104 secret"),
    );
    let ed = Signer::Ed25519(signer_ed25519::Ed25519Signer::from_seed(&[0x5a; 32]).expect("gate 104 signer"));
    let mut win = [0u8; 1_024];
    let mut a = WsAnswer::default();

    let g = AllocGuard::new();
    let mut total = 0usize;
    let mut i = 0u64;
    while i < N {
        let ts = 1_790_000_000_000 + i;
        let cid = Part::Cid(&prefix, 2, 0x65 + i);
        let o = Place {
            symbol: b"BTCUSDT",
            side: (i & 1) as u8,
            maker: i & 2 == 0,
            reduce_only: i & 4 == 0,
            qty_1e6: 2_000,
            qty_dec: 3,
            px_1e6: 65_000_000_000 + (i % 100) as i64 * 100_000,
            px_dec: 1,
            cid,
            ts_ms: ts,
        };
        let signer = if i % 8 == 0 { &ed } else { &hmac };
        let mut w = QueryWriter::new(&mut win);
        render_place(&mut w, &st, &o);
        total += w.sign_with(signer).expect("gate 104 place");
        let mut w = QueryWriter::new(&mut win);
        render_modify(&mut w, &st, &o);
        total += w.sign_with(&hmac).expect("gate 104 modify");
        let mut w = QueryWriter::new(&mut win);
        render_by_cid(&mut w, &st, b"BTCUSDT", cid, ts);
        total += w.sign_with(&hmac).expect("gate 104 cancel");
        let mut w = QueryWriter::new(&mut win);
        render_by_order_id(&mut w, &st, b"BTCUSDT", 7_000 + i, ts);
        total += w.sign_with(&hmac).expect("gate 104 stray");
        if i % 64 == 0 {
            let mut w = QueryWriter::new(&mut win);
            render_account(&mut w, &st, ts);
            total += w.sign_with(&hmac).expect("gate 104 account");
        }
        scan_rest_answer(200, PLACED, &mut a).expect("gate 104 answer");
        std::hint::black_box(prefix.classify(a.cid.get(PLACED)));
        scan_rest_answer(400, REFUSED, &mut a).expect("gate 104 refusal");
        std::hint::black_box(a.code);
        scan_rest_answer(503, b"", &mut a).expect("gate 104 5xx");
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(total > 0, "the gate rendered nothing");
    assert_eq!(allocs, 0, "gate 104: the REST route allocated {allocs} times ({bytes} B)");
}

/// **BX7 gate 105 — the COIN-M lane is 0 B/op.** What the gateway does per
/// order and per reconciliation on its COIN-M lane that the USDⓈ-M gates
/// do not already measure: `order.place` for an inverse perpetual (whole
/// contracts, `reduceOnly`) and the lane's own `account.status` rendered
/// and masked into a send window; the answer and the dstream's
/// `ORDER_TRADE_UPDATE` (the margin asset's fee) scanned in place and their
/// row found by product — BX11: the fee (×1e9, in the coin) classified as
/// the row's margin coin and the trade's notional taken in the coin
/// (contracts × face ÷ price), `J_FEE`'s fields; the COIN-M account read
/// (per-asset margins, the
/// riskiest asset's ratio — `scan_cm_account`), its position onto an
/// inverse leg judged at the contract's face, dapi's open orders and the
/// day's trades summed by the inverse law (contracts × face); the REST
/// route's place rendered and HMAC-signed for `/dapi/v1/order`. Built
/// once outside the guard: the table, the windows, the signer.
#[test]
fn binance_coinm_lane_render_and_scan_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::config::{HmacSigner, Signer};
    use exec_binance::inst::{BindSpec, InstTable, INST_INVERSE, PRODUCT_COINM};
    use exec_binance::recon::{day_increasing_1e6, judge_positions, scan_cm_account, Leg, PosRow, Verdict};
    use exec_binance::rest::{render_place, scan_orders, scan_trades, OrderRow, QueryWriter, TradeRow};
    use exec_binance::userstream::{scan_user_event, UserEvent};
    use exec_binance::wsapi::{queue_account, queue_place, scan_answer, FrameSink, Part, Place, WsAnswer, WsStatic};

    const N: u64 = 20_000;
    const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","contractStatus":"TRADING","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]}]}"#;
    const PLACED: &[u8] = br#"{"id":17,"status":200,"result":{"orderId":9001,"symbol":"BTCUSD_PERP","pair":"BTCUSD","status":"NEW","clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.0","avgPrice":"0.0","origQty":"3","executedQty":"0","cumBase":"0","timeInForce":"IOC","type":"LIMIT","side":"BUY","updateTime":1790000000000}}"#;
    const UPDATE: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000123,"T":1790000000120,"i":"SfsR","o":{"s":"BTCUSD_PERP","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"3","p":"65000.0","ap":"64999.9","sp":"0","x":"TRADE","X":"FILLED","i":9001,"l":"3","z":"3","L":"64999.9","ma":"BTC","N":"BTC","n":"0.00000230","T":1790000000120,"t":9101,"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0}}"#;
    const ACCOUNT: &[u8] = br#"{"id":19,"status":200,"result":{"feeTier":0,"canTrade":true,"assets":[{"asset":"BTC","walletBalance":"0.00600000","marginBalance":"0.00600000","maintMargin":"0.00012000","initialMargin":"0.00024000"},{"asset":"ETH","walletBalance":"0","marginBalance":"0","maintMargin":"0","initialMargin":"0"}],"positions":[{"symbol":"BTCUSD_PERP","positionSide":"BOTH","positionAmt":"3","maintMargin":"0.00012000","notionalValue":"0.00461538","leverage":"20","isolated":false,"updateTime":1790000000000}]}}"#;
    const OPEN: &[u8] = br#"[{"avgPrice":"0.0","clientOrderId":"mv6511000020000000000000004e00000","cumBase":"0","executedQty":"0","orderId":7003,"origQty":"5","price":"60000.0","side":"BUY","status":"NEW","symbol":"BTCUSD_PERP","pair":"BTCUSD","timeInForce":"GTX","type":"LIMIT","updateTime":1790000000000}]"#;
    const TRADES: &[u8] = br#"[{"symbol":"BTCUSD_PERP","id":601,"orderId":5001,"pair":"BTCUSD","side":"BUY","price":"64000.0","qty":"3","realizedPnl":"0","marginAsset":"BTC","baseQty":"0.00468750","commission":"0.00000234","commissionAsset":"BTC","time":1790000000001,"positionSide":"BOTH","buyer":true,"maker":false},{"symbol":"BTCUSD_PERP","id":602,"orderId":5002,"pair":"BTCUSD","side":"SELL","price":"64100.0","qty":"3","realizedPnl":"0","marginAsset":"BTC","baseQty":"0.00468018","commission":"0.00000234","commissionAsset":"BTC","time":1790000000002,"positionSide":"BOTH","buyer":false,"maker":false}]"#;

    /// The send window, as gate 93's (`WsConn`'s writer).
    struct Window {
        buf: [u8; 4_096],
        n: usize,
        mask: u64,
    }
    impl FrameSink for Window {
        fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), core_net::WsErr> {
            self.mask = self.mask.wrapping_add(1);
            let m = core_net::ws_mask_from_counter(self.mask);
            self.n = core_net::ws_write_text_frame_with(&mut self.buf, parts, m).map_err(|_| core_net::WsErr::Overflow)?;
            Ok(())
        }
    }

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 5_000);
    let hmac = Signer::Hmac(
        HmacSigner::from_secret(*b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j").expect("gate 105 secret"),
    );
    let mut disc = ingress_binance::discovery::BnDiscovery::new();
    disc.ingest_body(DAPI).expect("gate 105 discovery");
    let (mut table, mut wire) = InstTable::new(7);
    let sym = core_types::make_symbol_id(VenueId::Binance, 3_073);
    let row = disc.find(b"BTCUSD_PERP").expect("gate 105 row");
    table
        .bind(&mut wire, &BindSpec { sym, product: PRODUCT_COINM, row, owned: true, maker_ok: false })
        .expect("gate 105 bind");
    let w = *wire.row(0);
    assert!(w.flags & INST_INVERSE != 0 && w.unit_1e6 == 100_000_000, "an inverse row at its face");
    let mut win = Window { buf: [0; 4_096], n: 0, mask: 0x6513 };
    let mut rest_win = [0u8; 1_024];
    let mut orders = [OrderRow::default(); 8];
    let mut trades = [TradeRow::default(); 8];
    let mut pos = [PosRow::default(); 8];
    let mut legs = [Leg::default(); 1];
    legs[0].owned = true;
    let mut a = WsAnswer::default();
    let mut ev = UserEvent::default();

    let g = AllocGuard::new();
    let mut total = 0usize;
    let mut i = 0u64;
    while i < N {
        let ts = 1_790_000_000_000 + i;
        let o = Place {
            symbol: w.symbol(),
            side: (i & 1) as u8,
            maker: false,
            reduce_only: i & 4 == 0,
            qty_1e6: 3_000_000,
            qty_dec: w.qty_dec,
            px_1e6: 65_000_000_000 + (i % 100) as i64 * 100_000,
            px_dec: w.px_dec,
            cid: Part::Cid(&prefix, 2, 0x65 + i),
            ts_ms: ts,
        };
        queue_place(&mut win, 17 + i, &st, &o).expect("gate 105 place");
        total += win.n;
        scan_answer(PLACED, &mut a).expect("gate 105 answer");
        std::hint::black_box(a.venue_oid);
        scan_user_event(UPDATE, &mut ev).expect("gate 105 update");
        std::hint::black_box(prefix.classify(ev.cid.get(UPDATE)));
        let r = wire.find(PRODUCT_COINM, ev.symbol.get(UPDATE));
        let asset = exec_binance::journal::fee_asset(wire.row(r), ev.fee_asset.get(UPDATE));
        assert_eq!(asset, exec_binance::journal::FEE_ASSET_SETTLE, "gate 105: BTC is the row's coin");
        std::hint::black_box((ev.qty_1e6, ev.commission_1e9, exec_binance::journal::fee_notional_1e9(&w, ev.last_px_1e6, ev.last_qty_1e6)));
        if i % 8 == 0 {
            // The REST route's place, for `/dapi/v1/order`.
            let mut q = QueryWriter::new(&mut rest_win);
            render_place(&mut q, &st, &o);
            total += q.sign_with(&hmac).expect("gate 105 rest place");
        }
        // The lane's reconciliation, at its own cadence.
        if i % 64 == 0 {
            queue_account(&mut win, 19 + i, &st, PRODUCT_COINM, ts).expect("gate 105 account");
            total += win.n;
            scan_answer(ACCOUNT, &mut a).expect("gate 105 account answer");
            let snap = scan_cm_account(ACCOUNT, a.result, &mut pos).expect("gate 105 account");
            std::hint::black_box(snap.ratio_1e6);
            legs[0].venue_1e6 = pos[0].amt_1e6;
            legs[0].booked_1e6 = 3_000_000;
            legs[0].px_1e6 = w.unit_1e6;
            let mut v = Verdict::default();
            judge_positions(&mut legs, &mut v);
            std::hint::black_box(v.reconciled());
            let n = scan_orders(OPEN, &mut orders).expect("gate 105 open orders");
            std::hint::black_box(prefix.classify(orders[n - 1].cid.get(OPEN)));
            let n = scan_trades(TRADES, &mut trades).expect("gate 105 trades");
            std::hint::black_box(day_increasing_1e6(&mut trades[..n], 0, true, w.unit_1e6));
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(total > 0, "the gate rendered nothing");
    assert_eq!(allocs, 0, "gate 105: the COIN-M lane allocated {allocs} times ({bytes} B)");
}

/// **BX7 gate 106 — Portfolio Margin is 0 B/op.** What the gateway does per
/// order and per reconciliation in `account_mode = "pm"` that the classic
/// gates do not already measure: papi's USDⓈ-M and COIN-M places rendered
/// in place (COIN-M's without an STP mode) and HMAC-signed; the one
/// `/pm/ws/` stream's `ORDER_TRADE_UPDATE` scanned with its business unit
/// (`fs`) and its row found by that product; `riskLevelChange` scanned;
/// the product's positions alone (`scan_positions`) and the unified
/// account (`scan_pm_account`) read. Built once outside the guard: the
/// windows, the signer, the table.
#[test]
fn binance_portfolio_margin_render_and_scan_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::config::{HmacSigner, Signer};
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_COINM, PRODUCT_USDM};
    use exec_binance::recon::{scan_pm_account, scan_positions, PosRow};
    use exec_binance::rest::{render_place, QueryWriter};
    use exec_binance::userstream::{scan_user_event, Span, UserEvent, UE_RISK};
    use exec_binance::wsapi::{Part, Place, WsStatic};

    const N: u64 = 20_000;
    const DAPI: &[u8] = br#"{"symbols":[{"symbol":"BTCUSD_PERP","pair":"BTCUSD","contractType":"PERPETUAL","contractStatus":"TRADING","contractSize":100,"filters":[{"filterType":"PRICE_FILTER","tickSize":"0.1"},{"filterType":"LOT_SIZE","stepSize":"1","minQty":"1","maxQty":"1000000"}]}]}"#;
    const OTU: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","fs":"CM","E":1790000000123,"T":1790000000120,"i":"","o":{"s":"BTCUSD_PERP","c":"mv6512ab0f2000000000000006500000","S":"SELL","o":"LIMIT","f":"IOC","q":"4","p":"65000.0","ap":"65000.1","sp":"0","x":"TRADE","X":"FILLED","i":9001,"l":"4","z":"4","L":"65000.1","N":"BTC","n":"0.00000307","T":1790000000120,"t":9101,"b":"0","a":"0","m":false,"R":false,"ps":"BOTH","rp":"0","V":"EXPIRE_TAKER","gtd":0}}"#;
    const RISK: &[u8] = br#"{"e":"riskLevelChange","E":1790000000000,"u":"1.05","s":"REDUCE_ONLY","eq":"1000.0","ae":"1000.0","m":"952.0"}"#;
    const ACCOUNT: &[u8] = br#"{"uniMMR":"50.00000000","accountEquity":"1000.00000000","actualEquity":"1000.00000000","accountInitialMargin":"40.00000000","accountMaintMargin":"20.00000000","accountStatus":"NORMAL","virtualMaxWithdrawAmount":"960.00000000","totalAvailableBalance":"","totalMarginOpenLoss":"","updateTime":1790000000000}"#;
    const POSITIONS: &[u8] = br#"{"assets":[{"asset":"USDT","crossWalletBalance":"1000","crossUnPnl":"0","maintMargin":"0.52","initialMargin":"1","updateTime":1}],"positions":[{"symbol":"BTCUSDT","initialMargin":"1","maintMargin":"0.52","unrealizedProfit":"0","positionSide":"BOTH","positionAmt":"0.002","notional":"130.00","updateTime":1}]}"#;

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st_um = WsStatic::new("EXPIRE_MAKER", 5_000);
    let st_cm = st_um.without_stp();
    let hmac = Signer::Hmac(
        HmacSigner::from_secret(*b"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j").expect("gate 106 secret"),
    );
    let mut disc = ingress_binance::discovery::BnDiscovery::new();
    disc.ingest_body(BX6_FAPI).expect("gate 106 fapi");
    let mut dapi = ingress_binance::discovery::BnDiscovery::new();
    dapi.ingest_body(DAPI).expect("gate 106 dapi");
    let (mut table, mut wire) = InstTable::new(7);
    let row = disc.find(b"BTCUSDT").expect("gate 106 um row");
    table
        .bind(&mut wire, &BindSpec { sym: 7, product: PRODUCT_USDM, row, owned: true, maker_ok: false })
        .expect("gate 106 bind um");
    let row = dapi.find(b"BTCUSD_PERP").expect("gate 106 cm row");
    let cm_sym = core_types::make_symbol_id(VenueId::Binance, 3_073);
    table
        .bind(&mut wire, &BindSpec { sym: cm_sym, product: PRODUCT_COINM, row, owned: true, maker_ok: false })
        .expect("gate 106 bind cm");
    let (um, cm) = (*wire.row(0), *wire.row(1));
    let mut rest_win = [0u8; 1_024];
    let mut pos = [PosRow::default(); 8];
    let mut ev = UserEvent::default();

    let g = AllocGuard::new();
    let mut total = 0usize;
    let mut i = 0u64;
    while i < N {
        let ts = 1_790_000_000_000 + i;
        let cm_side = (i & 1) == 0;
        let (w, st) = if cm_side { (&cm, &st_cm) } else { (&um, &st_um) };
        let o = Place {
            symbol: w.symbol(),
            side: (i & 1) as u8,
            maker: false,
            reduce_only: i & 4 == 0,
            qty_1e6: if cm_side { 4_000_000 } else { 2_000 },
            qty_dec: w.qty_dec,
            px_1e6: 65_000_000_000 + (i % 100) as i64 * 100_000,
            px_dec: w.px_dec,
            cid: Part::Cid(&prefix, 2, 0x65 + i),
            ts_ms: ts,
        };
        let mut q = QueryWriter::new(&mut rest_win);
        render_place(&mut q, st, &o);
        total += q.sign_with(&hmac).expect("gate 106 place");
        scan_user_event(OTU, &mut ev).expect("gate 106 update");
        std::hint::black_box(prefix.classify(ev.cid.get(OTU)));
        std::hint::black_box(wire.find(ev.fs, ev.symbol.get(OTU)));
        if i % 64 == 0 {
            scan_user_event(RISK, &mut ev).expect("gate 106 risk");
            assert_eq!(ev.kind, UE_RISK);
            let n = scan_positions(POSITIONS, Span { at: 0, len: POSITIONS.len() as u32 }, &mut pos).expect("gate 106 positions");
            std::hint::black_box(pos[n - 1].amt_1e6);
            let snap = scan_pm_account(ACCOUNT).expect("gate 106 account");
            std::hint::black_box(exec_binance::margin::um_ratio_1e6(snap.maint_1e6, snap.equity_1e6));
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(total > 0, "the gate rendered nothing");
    assert_eq!(allocs, 0, "gate 106: Portfolio Margin allocated {allocs} times ({bytes} B)");
}

/// **BX7 gate 107 — the DNS re-resolve's gateway side is 0 B/op.** What the
/// gateway's timer pass does every millisecond for the cold resolver
/// (`dns_tick`): every answer read (none here) and each host's streak of
/// failed establishments asked again where due — driven below
/// `RERESOLVE_AFTER` (and back at 0), so the cold thread never resolves:
/// its `getaddrinfo` allocates by design, on its own thread, and the guard
/// counts the whole process. Built once outside the guard: the resolver
/// and its thread — `spawn` returns once the thread serves, so its
/// start-up (std's bookkeeping, which allocates on macOS) is outside too.
#[test]
fn binance_dns_tick_is_zero_alloc() {
    use exec_binance::dns::{Resolver, DNS_HOSTS, RERESOLVE_AFTER};

    const N: u32 = 20_000;
    let mut names = [""; DNS_HOSTS];
    names[0] = "localhost";
    names[1] = "ws-fapi.binance.com";
    let mut r = Resolver::spawn(names, 2, 443).expect("gate 107 resolver");
    let mut streak = [0u32; DNS_HOSTS];

    let g = AllocGuard::new();
    let mut i = 0u32;
    while i < N {
        streak[0] = i % RERESOLVE_AFTER;
        streak[1] = (i / 7) % RERESOLVE_AFTER;
        while let Some(a) = r.answer() {
            std::hint::black_box(a);
        }
        r.ask_where_due(&streak);
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert_eq!(r.resolved(), 0, "gate 107: nothing was due");
    assert_eq!(allocs, 0, "gate 107: the DNS tick allocated {allocs} times ({bytes} B)");
}

/// Gate 108's round: two IoCs, each one `order.place` sealed and ONE
/// record opened — its ACK and the subscription's three events for it
/// (NEW, the TRADE, the balances), which the venue writes together.
const G108_ROUND: u64 = 4;
/// Gate 108's partial: the place sealed; its ACK, NEW, the half's TRADE,
/// the EXPIRED and the balances opened as one record.
const G108_PARTIAL: u64 = 2;
/// Gate 108's round with the venue writing a frame a write: each IoC's
/// place sealed, and its ACK, NEW, TRADE and balances opened as FOUR
/// records — one Vec a frame.
const G108_FRAMES_ROUND: u64 = 10;
/// Gate 108's reconciliation cycle (the sweep's confirmation): spot's
/// `account.status` sealed on the order session and its answer opened; the
/// REST listing sealed and its answer opened.
const G108_RECON: u64 = 4;

/// What the engine end saw (gate 108).
#[derive(Clone, Copy, Default)]
struct Seen108 {
    acks: u64,
    filled: u64,
    expired: u64,
    retired_other: u64,
    rejects: u64,
    locks: u64,
    scan_fails: u64,
    /// `EVT_POSITION`: the booked balance went flat → long, or back.
    positions: u64,
    fills: u64,
    /// Lane 4's quantities ×1e6: bought, sold.
    bought: i64,
    sold: i64,
    j_fill: u64,
    j_fee: u64,
    /// `J_FEE`s of a spot row whose commission is BNB (the venue's burn).
    j_fee_bnb: u64,
    /// `EVT_STATUS`: the pulse, which publishes the tallies first.
    pulses: u64,
    /// `EVT_SWEEP` reporting the sweep done.
    sweeps_done: u64,
    /// `EVT_RECON`: the cycles judged, and those reconciled.
    recons: u64,
    reconciled: u64,
    /// BX13 (R2-2): `EVT_SPOT_DAY` — the venue's day count read.
    spot_days: u64,
}

/// **BX8 gate 108 — the spot lane's whole step, over a real rustls
/// loopback, allocates only rustls' record buffers** (gate 94's harness,
/// spot's lane).
///
/// The REAL gateway (`BnGateway::step`, booted as the engine boots it)
/// SPOT-ONLY — BTCUSDT bound under the alias as `PRODUCT_SPOT`, IoC-only
/// (spot has no dead-man) — against the scripted Binance's spot routes
/// (`/ws-api/v3`, `/api/v3`) in gate 94's CHILD process (`GATE94_SPOT=1`:
/// the account pays every commission in BNB, so the BTC a round buys is
/// sold back whole — the balances stay level, no SELL is refused for
/// funds). The engine end driven raw; the knob timers (the
/// reconciliation, the session's rotation) set past the run, the clock's
/// 60 s round asserted clear before each window, the boot's REST settled
/// (no answer for 300 ms) before the warm-up. Every round: an IoC BUY of
/// 0.002 at 65 000.00, then — once it ended — an IoC SELL of 0.002 at
/// 64 999.98, each filled whole one tick better (64 999.99). Each IoC is
/// ONE `order.place` on the order session (one record sealed, about 345 B
/// on the wire) and ONE record opened: the venue writes the place's ACK
/// and the three events the subscription carries for it —
/// `executionReport` NEW, its TRADE (FILLED, the commission 0.00001 BNB)
/// and `outboundAccountPosition` — in one write, framed 356 + 531 + 556 +
/// 248 B ≈ 1.7 KB, under rustls' 4 KiB receive window. Each frame is told
/// from an answer and its event read in place (`scan_answer`, then
/// `scan_spot_event` on the `event` span), its row found by product; the
/// trade booked once (lane 4, `J_FILL`; the booked balance flat → long →
/// flat, an `EVT_POSITION` each way), its commission journaled once
/// (`J_FEE`, BNB on a spot row), the IoC retired FILLED, the balances
/// counted, never booked. Pinned like gate 94: rustls 0.23's buffered API
/// allocates one `Vec` per record it seals and one per application-data
/// record it opens — exactly [`G108_ROUND`] a round (two sealed, two
/// opened); the decrypted-record queue's growth is forced into the
/// warm-up by its stalled first IoC. Everything else — the command, the
/// desk, the render (the frame rides the session's `session.logon`), the
/// scans, the dedupe ring, the fee, the journal, lane 4, the events — is
/// 0 B: a regression shows as one more. Then, each measured alone:
///
/// - **the partial** — an IoC BUY of exactly 0.003, which the venue fills
///   half (0.0015, `spot_halve_qty_1e6`) and EXPIREs: its ACK, NEW, TRADE
///   (PARTIALLY_FILLED), EXPIRED and the balances in one record, framed
///   356 + 531 + 563 + 578 + 248 B ≈ 2.3 KB — [`G108_PARTIAL`]; the order's
///   `z` is what its TRADE booked, so nothing is booked without its trade
///   and no order's-trades read is queued;
/// - **a write a frame** — the venue (`spot_frame_per_write`, asked of the
///   child by a file) writes the ACK and each event in its own write, its
///   own record, as the venue may: the same rounds open four records an
///   IoC — [`G108_FRAMES_ROUND`] a round (two sealed, eight opened): the
///   cost is a record's, never the frame's. An IoC ends at its TRADE while
///   its balances' record may still be on the wire, to queue in rustls
///   beside the next IoC's four (five at once: the decrypted-record queue,
///   a `VecDeque`, grows past four — one allocation more, on four runs in
///   six before this warm-up): its own warm-up opens with a BUY and a SELL
///   written together, eight records answered in one read, so that growth
///   is the warm-up's; and the window opens and closes on every balances'
///   event read (the pulse's `GW_EVENTS_OTHER`), each record inside its
///   own IoC's window;
/// - **a reconciliation cycle** — the halt's sweep (the stats word): no
///   spot order rests, so nothing is cancelled and the confirmation is the
///   whole cycle — spot's `account.status` on the order session (about
///   145 B sealed; its answer, the balances and the permissions, about
///   820 B opened), then the REST listing (about 325 B sealed; `[]` under
///   its head, about 100 B opened): [`G108_RECON`], one REST job, about
///   1.4 KB measured; the
///   sweep reported done and the cycle reconciled (`EVT_RECON`, the
///   verdict: no drift, nothing unseen, no ghost, orphan or stranger), the
///   BTC the partial left explained.
///
/// Each window's tallies (`arm::GW_*`, read as the arm reads them: the
/// words a pulse publishes) move as its events say: one
/// `GW_EVENTS_OTHER` an IoC (its `outboundAccountPosition`), its fills
/// booked (`GW_FILLS_BOOKED`: two a round, one the partial), and the
/// cycle's `GW_RECON_OK` by one with no drift or unseen leg; foreign,
/// unowned, deferred, unresolved and lost fills, dropped jobs, doubts
/// raised and journal records dropped stay 0 throughout. Each IoC window
/// answers no REST job and opens no session again; no frame fails its
/// scan.
///
/// NOT pinned here: the boot (the logon, the subscription, the day's
/// trades, `ticker/price`); BX13's day read (`account.rateLimits.orders`:
/// asked, answered and carried to the arm at each venue UTC day's first
/// cycle — the boot's here, outside every window; its render and scan are
/// gate 109's, and the request's post, its weight and the event's emit
/// are the cycle's own, pinned here); the order's-trades read of a fill booked
/// without its report (REST, as the listing; its scan is gate 109's); a
/// commission charged in the base asset (a BUY without the burn) — the
/// same path but for the explained balance's subtraction, classified in
/// gate 109; the in-doubt `order.status`, the session's rotation and the
/// subscription's end (`eventStreamTerminated`: the session dropped and
/// dialled again); and bursts over the 4 KiB window (several IoCs'
/// reports in one read). The bytes, unpinned, match the arithmetic: 4 076
/// B a round (two records of about 345 B sealed, two of about 1.7 KB
/// opened), 2 621 B the partial. A break-and-watch (an 8-byte `Vec` in
/// `spot_event`: three events an IoC, four in the partial; one in
/// `spot_account_answered`) failed the rounds at 3 000, the partial at 6, a
/// frame a write at 4 800 and the cycle at 5.
#[test]
fn binance_spot_lane_round_trip_allocates_only_rustls_record_buffers() {
    use clob_dispatcher::{RETIRED_EXPIRED, RETIRED_FILLED};
    use core_config::SecretKeyBytes;
    use core_fill::ORDER_KIND_IOC;
    use core_ring::{Consumer, Ring};
    use core_types::{ExecRecord, Fill, Side};
    use exec_binance::arm::{
        GW_EVENTS_OTHER, GW_FILLS_BOOKED, GW_FILLS_DEFERRED, GW_FILLS_FOREIGN, GW_FILLS_LOST, GW_FILLS_UNOWNED,
        GW_FILLS_UNRESOLVED, GW_IN_DOUBT_RAISED, GW_JOBS_DROPPED, GW_JOURNAL_DROPPED, GW_RECON_DRIFT_LEGS, GW_RECON_FAILED,
        GW_RECON_HELD, GW_RECON_OK, GW_RECON_UNSEEN_LEGS, GW_TALLIES,
    };
    use exec_binance::cmd::{
        duplex, BnCmd, BnEngineEnd, EVT_ACK, EVT_F_RECONCILED, EVT_F_SWEEP_DONE, EVT_LOCK, EVT_POSITION, EVT_RECON, EVT_REJECT,
        EVT_RETIRED, EVT_SCAN_FAIL, EVT_SPOT_DAY, EVT_STATUS, EVT_SWEEP,
    };
    use exec_binance::config::{BnConfig, Scope};
    use exec_binance::gateway::{BnGateway, GwKnobs, Phase};
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_SPOT};
    use exec_binance::journal::{fee_fields, JournalTx, FEE_ASSET_BNB, JOURNAL_RING, J_FEE, J_FILL};
    use exec_binance::mode::AccountMode;
    use exec_binance::testvenue::{EPOCH, KEY, SEED, SLOT, SPOT};
    use exec_gateway::stats::SWEEP_CLEAR;
    use ingress_binance::discovery::BnDiscovery;
    use std::time::{Duration, Instant};

    const FILL_N: usize = 1_024;
    const ROUNDS: u64 = 300;
    const WARM: u64 = 50;
    /// The per-frame window's own warm-up, once the venue writes a frame a
    /// write.
    const WARM_FRAMES: u64 = 20;
    /// Every timer the gate does not measure, set past its run: no
    /// reconciliation cycle, no rotation.
    const LONG_MS: u64 = 3_600_000;
    /// BTCUSDT on spot's grid (tick 0.01, step 0.00001), ×1e6: the BUY's
    /// limit and the SELL's, each filled one tick better (64 999.99) —
    /// 0.002 each (130 USDT: over the 5 USDT notional); the partial's
    /// 0.003, which the venue fills half.
    const PX_BUY: i64 = 65_000_000_000;
    const PX_SELL: i64 = 64_999_980_000;
    const QTY: i64 = 2_000;
    const QTY_HALVED: i64 = 3_000;
    /// The clock's round comes 60 s after the first Ready pass (a
    /// constant): every window must start well before it.
    const CLOCK_CLEAR: Duration = Duration::from_secs(50);

    type Gw = BnGateway<FILL_N>;

    /// One step, then everything the engine end, lane 4 and the journal
    /// hold, tallied in place.
    fn pump(
        gw: &mut Gw,
        eng: &mut BnEngineEnd,
        lane: &mut Consumer<Fill, FILL_N>,
        journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
        s: &mut Seen108,
    ) {
        gw.step();
        while let Some(g) = eng.evt.pop() {
            let (kind, why, flags) = (g.kind, g.why, g.flags);
            drop(g);
            match kind {
                EVT_ACK => s.acks += 1,
                EVT_RETIRED => match why {
                    RETIRED_FILLED => s.filled += 1,
                    RETIRED_EXPIRED => s.expired += 1,
                    _ => s.retired_other += 1,
                },
                EVT_REJECT => s.rejects += 1,
                EVT_LOCK => s.locks += 1,
                EVT_SCAN_FAIL => s.scan_fails += 1,
                EVT_POSITION => s.positions += 1,
                EVT_STATUS => s.pulses += 1,
                EVT_SWEEP => s.sweeps_done += u64::from(flags & EVT_F_SWEEP_DONE != 0),
                EVT_RECON => {
                    s.recons += 1;
                    s.reconciled += u64::from(flags & EVT_F_RECONCILED != 0);
                }
                EVT_SPOT_DAY => s.spot_days += 1,
                _ => {}
            }
        }
        while let Some(g) = lane.try_pop_ref() {
            if matches!(g.side, Side::Bid) {
                s.bought += g.qty.0;
            } else {
                s.sold += g.qty.0;
            }
            drop(g);
            s.fills += 1;
        }
        while let Some(g) = journal.try_pop_ref() {
            s.j_fill += u64::from(g.kind == J_FILL);
            if g.kind == J_FEE {
                let (_, asset, _, product, _, _) = fee_fields(g.code);
                s.j_fee += 1;
                s.j_fee_bnb += u64::from(asset == FEE_ASSET_BNB && product == PRODUCT_SPOT);
            }
            drop(g);
        }
    }

    /// Pumped until the tallies a pulse published count `n` events of no
    /// kind the arm acts on (`GW_EVENTS_OTHER`: the balances' events) — the
    /// pulses run inside every window, 0 B.
    fn others_read(
        gw: &mut Gw,
        eng: &mut BnEngineEnd,
        lane: &mut Consumer<Fill, FILL_N>,
        journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
        s: &mut Seen108,
        n: u64,
    ) {
        let t = Instant::now();
        while eng.stats.published(GW_EVENTS_OTHER) < n {
            assert!(t.elapsed() < Duration::from_secs(2), "gate 108: the balances' events never all read");
            pump(gw, eng, lane, journal, s);
        }
    }

    /// The gateway's tallies (`arm::GW_*`) as of a pulse after everything
    /// so far — the words each pulse publishes before its `EVT_STATUS`,
    /// read as the arm reads them. Outside the windows.
    fn tallies(
        gw: &mut Gw,
        eng: &mut BnEngineEnd,
        lane: &mut Consumer<Fill, FILL_N>,
        journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
        s: &mut Seen108,
    ) -> [u64; GW_TALLIES] {
        let (p0, t) = (s.pulses, Instant::now());
        while s.pulses == p0 {
            assert!(t.elapsed() < Duration::from_secs(2), "gate 108: no pulse");
            pump(gw, eng, lane, journal, s);
        }
        let mut w = [0u64; GW_TALLIES];
        let mut i = 0;
        while i < GW_TALLIES {
            w[i] = eng.stats.published(i);
            i += 1;
        }
        w
    }

    // ---- the venue, in its own process (gate 94's helper, spot's knobs) ----
    let dir = std::env::temp_dir().join(format!("mv-gate108-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 108 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 108 exe"))
        .args(["gate94_venue_helper", "--exact", "--ignored", "--test-threads=1"])
        .env("GATE94_VENUE_DIR", &dir)
        .env("GATE94_REST", "0")
        .env("GATE94_SPOT", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 108 child");
    let deadline = Instant::now() + Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 108 port");
        }
        assert!(Instant::now() < deadline, "gate 108: the venue never came up");
        std::thread::sleep(Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 108 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der)).expect("gate 108 anchor");
    let tls = std::sync::Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());

    // ---- the gateway, booted spot-only as the engine boots it ----
    let seed = SecretKeyBytes::new_locked(SEED).expect("gate 108 seed");
    let cfg = BnConfig::loopback(Scope::Live, String::from(KEY), seed, port).expect("gate 108 config");
    let mut disc = BnDiscovery::new();
    disc.ingest_body(SPOT).expect("gate 108 discovery");
    let (mut table, mut wire) = InstTable::new(core_config::universe::LEGACY_BN_ANCHOR_SYM);
    let row = disc.find(b"BTCUSDT").expect("BTCUSDT");
    table
        .bind(
            &mut wire,
            &BindSpec { sym: core_config::universe::LEGACY_BN_ANCHOR_SYM, product: PRODUCT_SPOT, row, owned: true, maker_ok: false },
        )
        .expect("gate 108 bind");
    let (mut eng, gw_end) = duplex();
    let (fill_tx, mut lane) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, mut journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: AccountMode::Classic,
        recv_window_ms: 5_000,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 2 * LONG_MS,
        heartbeat_ms: LONG_MS,
        recon_every_ms: LONG_MS,
        spin: false,
        rotate_after_ns: LONG_MS * 1_000_000,
        rest_orders: false,
        pnl_anchor_1e6: 0,
        boot_timeout_ns: 20_000_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: 0,
        equity: None,
    };
    let mut gw = Gw::new(&cfg, tls, knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx)).expect("gate 108 gateway");
    gw.boot().expect("gate 108 boot");
    assert_eq!(gw.phase(), Phase::Ready);
    let ready_at = Instant::now();
    let mut s = Seen108::default();
    // The order verbs' answers (the REST jobs' are `rest_rtt`'s).
    let answers = |eng: &BnEngineEnd| eng.stats.io()[1];
    // (REST jobs answered, order-session re-opens) so far.
    let io = |eng: &BnEngineEnd| (eng.stats.rest_rtt.count(), eng.stats.io()[5]);

    // Until no REST answer for 300 ms: whatever the boot left queued.
    let t = Instant::now();
    let (mut seen, mut quiet) = (eng.stats.rest_rtt.count(), Instant::now());
    while quiet.elapsed() < Duration::from_millis(300) {
        assert!(t.elapsed() < Duration::from_secs(10), "gate 108: the boot's REST never settled");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        if eng.stats.rest_rtt.count() != seen {
            (seen, quiet) = (eng.stats.rest_rtt.count(), Instant::now());
        }
    }

    // One IoC — (side, limit, quantity) — pushed, then pumped until it
    // ended: answered, acknowledged, booked on lane 4, its commission
    // journaled, retired (FILLED; the partial EXPIRED).
    let mut oid = 1_000_000u64;
    let mut ioc = |gw: &mut Gw,
                   eng: &mut BnEngineEnd,
                   lane: &mut Consumer<Fill, FILL_N>,
                   journal: &mut Consumer<ExecRecord, JOURNAL_RING>,
                   s: &mut Seen108,
                   (side, px, qty): (Side, i64, i64),
                   stall: bool| {
        oid += 1;
        let (a0, k0, e0, l0, fee0) = (answers(eng), s.acks, s.filled + s.expired, s.fills, s.j_fee);
        let sent0 = eng.stats.io()[0];
        assert!(eng.cmd.push(&BnCmd::place(oid, px, qty, 0, 0, ORDER_KIND_IOC, side, SLOT, 0).at(core_time::now_ns())));
        let t = Instant::now();
        if stall {
            // Write the place, then let its whole answer arrive before the
            // next read: rustls' record queues reach their high-water
            // marks here, in the warm-up.
            while eng.stats.io()[0] <= sent0 {
                assert!(t.elapsed() < Duration::from_secs(5), "gate 108: the place was not written");
                pump(gw, eng, lane, journal, s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        while !(answers(eng) > a0 && s.acks > k0 && s.filled + s.expired > e0 && s.fills > l0 && s.j_fee > fee0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 108: an IoC stalled");
            pump(gw, eng, lane, journal, s);
        }
    };
    let (buy, sell, halved) = ((Side::Bid, PX_BUY, QTY), (Side::Ask, PX_SELL, QTY), (Side::Bid, PX_BUY, QTY_HALVED));
    let mut w = 0u64;
    while w < WARM {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, buy, w == 0);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, sell, false);
        w += 1;
    }

    // ---- the rounds ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 108: the run neared the clock's 60 s round");
    let t_warm = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    // BX13 (R2-2): the venue's day count was read once, by the boot's first
    // cycle — outside every window.
    assert_eq!(s.spot_days, 1, "gate 108: spot's day count read once, before the windows");
    let (warm, io0) = (s, io(&eng));
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, buy, false);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, sell, false);
        r += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let (rounds, io1) = (s, io(&eng));
    let t_rounds = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);

    // ---- the partial, measured alone ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 108: the run neared the clock's 60 s round");
    let io2 = io(&eng);
    let g = AllocGuard::new();
    ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, halved, false);
    let (partial_allocs, partial_bytes, _) = g.delta();
    let (partial, io3) = (s, io(&eng));
    let t_partial = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);

    // ---- a write a frame: the ACK and each event in its own record ----
    std::fs::write(dir.join("spot-frames"), b"1").expect("gate 108 ask");
    let t = Instant::now();
    while !dir.join("spot-frames-on").exists() {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 108: the venue never wrote a frame a write");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    // An IoC ends at its TRADE; its balances' event, now a record of its
    // own, may still be on the wire — and then queues in rustls beside
    // the next IoC's four. The warm-up's first step is a BUY and a SELL
    // written together and answered at once (eight records in one read):
    // the decrypted-record queue's high-water mark is the warm-up's.
    let (k0, l0, fee0, sent0) = (s.acks, s.fills, s.j_fee, eng.stats.io()[0]);
    let now = core_time::now_ns();
    assert!(eng.cmd.push(&BnCmd::place(2_000_001, PX_BUY, QTY, 0, 0, ORDER_KIND_IOC, Side::Bid, SLOT, 0).at(now)));
    assert!(eng.cmd.push(&BnCmd::place(2_000_002, PX_SELL, QTY, 0, 0, ORDER_KIND_IOC, Side::Ask, SLOT, 0).at(now)));
    let t = Instant::now();
    while eng.stats.io()[0] < sent0 + 2 {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 108: the pair was not written");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    std::thread::sleep(Duration::from_millis(20));
    while !(s.acks >= k0 + 2 && s.fills >= l0 + 2 && s.j_fee >= fee0 + 2) {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 108: the pair stalled");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    let mut w = 0u64;
    while w < WARM_FRAMES {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, buy, false);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, sell, false);
        w += 1;
    }
    // Every warm-up IoC's balances read: none falls inside the window.
    others_read(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, t_partial[GW_EVENTS_OTHER] + 2 + 2 * WARM_FRAMES);
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 108: the run neared the clock's 60 s round");
    let t_frames0 = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    let (frames0, io4) = (s, io(&eng));
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, buy, false);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, sell, false);
        r += 1;
    }
    // ...and every window IoC's, the last one's too: all inside it.
    others_read(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, t_frames0[GW_EVENTS_OTHER] + 2 * ROUNDS);
    let (frame_allocs, frame_bytes, _) = g.delta();
    let (frames, io5) = (s, io(&eng));
    let t_frames = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);

    // ---- a reconciliation cycle, measured alone: the sweep's confirmation ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 108: the run neared the clock's 60 s round");
    let (before, io6) = (s, io(&eng));
    let g = AllocGuard::new();
    let swept = eng.stats.request_sweep();
    let t = Instant::now();
    while !(eng.stats.sweep() == (swept, SWEEP_CLEAR) && s.sweeps_done > before.sweeps_done && s.recons > before.recons) {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 108: the sweep's cycle never ran");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    let (recon_allocs, recon_bytes, _) = g.delta();
    let (recon, io7) = (s, io(&eng));
    let t_recon = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    let verdict = gw.last_verdict();
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();

    // ---- the IoC windows' events ----
    let both = 2 * ROUNDS;
    // The booked balance changes sign each IoC of the rounds (flat ↔ long);
    // over the partial's 0.0015 it stays long throughout.
    for (name, a, b, signs) in [("rounds", warm, rounds, both), ("a frame a write", frames0, frames, 0)] {
        assert_eq!(
            (b.acks - a.acks, b.filled - a.filled, b.fills - a.fills, b.j_fill - a.j_fill),
            (both, both, both, both),
            "gate 108, {name}: every IoC acknowledged, filled whole, booked once on lane 4 and journaled once"
        );
        assert_eq!(
            (b.j_fee - a.j_fee, b.j_fee_bnb - a.j_fee_bnb, b.expired - a.expired),
            (both, both, 0),
            "gate 108, {name}: every trade's commission journaled once, in BNB, on a spot row; nothing expired"
        );
        assert_eq!(
            (b.bought - a.bought, b.sold - a.sold, b.positions - a.positions),
            (ROUNDS as i64 * QTY, ROUNDS as i64 * QTY, signs),
            "gate 108, {name}: every round bought 0.002 and sold it back"
        );
    }
    assert_eq!(
        (partial.acks - rounds.acks, partial.expired - rounds.expired, partial.filled - rounds.filled, partial.fills - rounds.fills),
        (1, 1, 0, 1),
        "gate 108: the partial acknowledged, one trade booked, the rest expired"
    );
    assert_eq!(
        (partial.bought - rounds.bought, partial.j_fill - rounds.j_fill, partial.j_fee_bnb - rounds.j_fee_bnb, partial.positions - rounds.positions),
        (QTY_HALVED / 2, 1, 1, 1),
        "gate 108: the partial's half booked and journaled with its commission, the balance long"
    );
    // ---- the cycle's ----
    assert_eq!(
        (recon.sweeps_done - before.sweeps_done, recon.recons - before.recons, recon.reconciled - before.reconciled),
        (1, 1, 1),
        "gate 108: the sweep done, one cycle judged, reconciled"
    );
    assert_eq!(
        (recon.fills, recon.acks, recon.j_fee),
        (before.fills, before.acks, before.j_fee),
        "gate 108: the cycle placed and booked nothing"
    );
    assert_eq!(
        (verdict.drift_legs, verdict.unseen_legs, verdict.ghosts, verdict.orphans, verdict.foreign),
        (0, 0, 0, 0, 0),
        "gate 108: the verdict — the BTC the partial left explained, nothing of anyone else's"
    );
    assert_eq!(
        (recon.rejects, recon.retired_other, recon.scan_fails, recon.locks),
        (0, 0, 0, 0),
        "gate 108: nothing refused, nothing else retired, no frame failed its scan, no lock"
    );
    // ---- the tallies (`arm::GW_*`) ----
    let moved = |a: &[u64; GW_TALLIES], b: &[u64; GW_TALLIES]| {
        (b[GW_EVENTS_OTHER] - a[GW_EVENTS_OTHER], b[GW_FILLS_BOOKED] - a[GW_FILLS_BOOKED], b[GW_RECON_OK] - a[GW_RECON_OK])
    };
    assert_eq!(moved(&t_warm, &t_rounds), (both, both, 0), "gate 108: the rounds — a balances' event and a fill an IoC");
    assert_eq!(moved(&t_rounds, &t_partial), (1, 1, 0), "gate 108: the partial — one each");
    assert_eq!(moved(&t_frames0, &t_frames), (both, both, 0), "gate 108: a frame a write — a balances' event and a fill an IoC");
    assert_eq!(moved(&t_frames, &t_recon), (0, 0, 1), "gate 108: the cycle — reconciled once, nothing booked or counted");
    assert_eq!(
        (t_recon[GW_RECON_FAILED] - t_frames[GW_RECON_FAILED], t_recon[GW_RECON_DRIFT_LEGS], t_recon[GW_RECON_UNSEEN_LEGS]),
        (0, 0, 0),
        "gate 108: the cycle — not failed, no drift leg, no unseen leg"
    );
    let zero = [
        GW_FILLS_FOREIGN,
        GW_FILLS_UNOWNED,
        GW_FILLS_DEFERRED,
        GW_FILLS_UNRESOLVED,
        GW_FILLS_LOST,
        GW_JOBS_DROPPED,
        GW_IN_DOUBT_RAISED,
        GW_JOURNAL_DROPPED,
        GW_RECON_HELD,
    ]
    .map(|k| t_recon[k]);
    assert_eq!(zero, [0; 9], "gate 108: no foreign, unowned, deferred, unresolved or lost fill; no job, doubt or record dropped; no held cycle");
    // BX13: the same venue day — the sweep's cycle asked nothing more (a run
    // across 00:00Z on the venue's clock asks again there: rerun it).
    assert_eq!(s.spot_days, 1, "gate 108: spot's day count read once in the run");
    // ---- the sockets ----
    let d = |a: (u64, u64), b: (u64, u64)| (b.0 - a.0, b.1 - a.1);
    assert_eq!(
        (d(io0, io1), d(io2, io3), d(io4, io5), d(io6, io7)),
        ((0, 0), (0, 0), (0, 0), (1, 0)),
        "gate 108: (REST jobs, order-session re-opens) — none in the IoC windows; the cycle's one listing"
    );
    assert_eq!(
        (allocs, partial_allocs, frame_allocs, recon_allocs),
        (G108_ROUND * ROUNDS, G108_PARTIAL, G108_FRAMES_ROUND * ROUNDS, G108_RECON),
        "gate 108: the rounds, the partial, a frame a write and the cycle allocated {allocs} ({bytes} B), {partial_allocs} ({partial_bytes} B), {frame_allocs} ({frame_bytes} B) and {recon_allocs} ({recon_bytes} B) — rustls' records only"
    );
}

/// **BX8 gate 109 — spot's render and scans are 0 B/op** (gate 93's shape,
/// spot's frames; in-process, no socket). What the gateway does per spot
/// order and per spot reconciliation that the futures gates do not
/// already measure: `order.place` for a spot row (no `reduceOnly`; the
/// step's 5 decimals, the tick's 2), the in-doubt `order.status`,
/// `userDataStream.subscribe` and spot's `account.status`
/// (`omitZeroBalances`) rendered and masked into a send window; the
/// subscription's answer read for the id it names (`scan_subscribed`: a
/// 200 naming none is no subscription; an 8-byte `Vec` in it failed the
/// gate at 313, once a cadence); the ACK
/// (spot's names no status) and an `order.status` answer (no `avgPrice`:
/// the average is `cummulativeQuoteQty / executedQty`) scanned; the
/// subscription's frames — `{"subscriptionId":0,"event":{…}}` — told from
/// an answer (`scan_answer`: `user`, the `event` span) and the event read
/// in place (`scan_spot_event`): an `executionReport` NEW; its TRADE with
/// the commission in each asset the venue charges — BNB (the burn), the
/// base (a BUY's: the armed row's, paid out of its explained balance, the
/// row found by asset name as `spot_row_of` finds it), the quote (a
/// SELL's) — each classified against the row (`fee_asset`), seen once by
/// the trade-id ring (a second report books nothing and owes nothing),
/// its notional taken and its `J_FEE` noted into the journal's ring
/// (drained as the writer would); an IoC's EXPIRED whose `z` no trade
/// booked (S1's quantity and implied price); `outboundAccountPosition`
/// (counted); `eventStreamTerminated` (the session's end). At their own
/// cadence: spot's `account.status` result — the armed base's balance
/// ×1e8, `canTrade`, the permissions (`scan_spot_account`) — the bStock
/// check (`judge_permission`) and the balance judged as a leg (×1e8
/// floored to ×1e6 against the explained); spot's `openOrders`; spot's
/// `myTrades` (`isBuyer`, the commission and its asset) summed as the
/// day's buys (`day_buys_1e6`) and read as the order's-trades answer
/// (its order's trade's fee owed once, another order's not paid); and
/// `ticker/price` (the adoption's price); BX13 (R2-2):
/// `account.rateLimits.orders` rendered and its answer's one DAY entry
/// read (`scan_day_orders`). NOT in-process: the booking —
/// `spot_event` → `on_user_event` → `book_event` → `book_fill` (lane 4,
/// the explained balance, the position event) — is the gateway's
/// private path, reachable only through `BnGateway::step` reading a
/// socket; gate 108 measures it there. Zero allocations.
#[test]
fn binance_spot_lane_render_and_scan_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::inst::{BindSpec, InstTable, PRODUCT_SPOT, ROW_NONE};
    use exec_binance::journal::{
        fee_asset, fee_code, fee_notional_1e9, JournalTx, FEE_ASSET_BNB, FEE_ASSET_OTHER, FEE_ASSET_SETTLE, JOURNAL_RING, J_FEE,
        TRADE_BY_MEMBER,
    };
    use exec_binance::mode::judge_permission;
    use exec_binance::oot::TidRing;
    use exec_binance::recon::{day_buys_1e6, judge_positions, scan_spot_account, Leg, Verdict};
    use exec_binance::rest::{scan_orders, scan_ticker_price, scan_trades, OrderRow, TradeRow};
    use exec_binance::userstream::{
        scan_spot_event, UserEvent, S_EXPIRED, S_FILLED, S_NEW, UE_ORDER, UE_OTHER, UE_STREAM_END, X_EXPIRED, X_NEW, X_TRADE,
    };
    use exec_binance::wsapi::{
        queue_account, queue_place, queue_rate_limits, queue_status, queue_subscribe, scan_answer, scan_day_orders, scan_subscribed,
        FrameSink, Part, Place, WsAnswer, WsStatic,
    };
    use ingress_binance::discovery::BnPermissions;

    const N: u64 = 20_000;
    // Spot's WS API answers: the place's ACK (`newOrderRespType=ACK`), the
    // in-doubt query's order.
    const PLACED: &[u8] = br#"{"id":17,"status":200,"result":{"symbol":"BTCUSDT","orderId":8001,"orderListId":-1,"clientOrderId":"mv6512ab0f2000000000000006500000","transactTime":1790000000000},"rateLimits":[{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":100,"count":1}]}"#;
    const STATUS: &[u8] = br#"{"id":18,"status":200,"result":{"symbol":"BTCUSDT","orderId":8001,"orderListId":-1,"clientOrderId":"mv6512ab0f2000000000000006500000","price":"65000.00000000","origQty":"0.00200000","executedQty":"0.00200000","cummulativeQuoteQty":"129.99998000","status":"FILLED","timeInForce":"IOC","type":"LIMIT","side":"BUY","stopPrice":"0.00000000","icebergQty":"0.00000000","time":1790000000000,"updateTime":1790000000000,"isWorking":true,"workingTime":1790000000000,"origQuoteOrderQty":"0.00000000","selfTradePreventionMode":"EXPIRE_MAKER"}}"#;
    // The subscription's events on the same session.
    const NEW: &[u8] = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1790000000123,"s":"BTCUSDT","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.00200000","p":"65000.00000000","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"NEW","X":"NEW","r":"NONE","i":8001,"l":"0.00000000","z":"0.00000000","L":"0.00000000","n":"0","N":null,"T":1790000000120,"t":-1,"I":1790000000121,"w":true,"m":false,"M":false,"O":1790000000120,"Z":"0.00000000","Y":"0.00000000","Q":"0.00000000","W":1790000000120,"V":"EXPIRE_MAKER"}}"#;
    const TRADE_BNB: &[u8] = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1790000000124,"s":"BTCUSDT","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.00200000","p":"65000.00000000","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"TRADE","X":"FILLED","r":"NONE","i":8001,"l":"0.00200000","z":"0.00200000","L":"64999.99000000","n":"0.00001000","N":"BNB","T":1790000000121,"t":9001,"I":1790000000122,"w":false,"m":false,"M":true,"O":1790000000120,"Z":"129.99998000","Y":"129.99998000","Q":"0.00000000","W":1790000000120,"V":"EXPIRE_MAKER"}}"#;
    const TRADE_BASE: &[u8] = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1790000000124,"s":"BTCUSDT","c":"mv6512ab0f2000000000000006600000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.00200000","p":"65000.00000000","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"TRADE","X":"FILLED","r":"NONE","i":8002,"l":"0.00200000","z":"0.00200000","L":"64999.99000000","n":"0.00000200","N":"BTC","T":1790000000121,"t":9002,"I":1790000000122,"w":false,"m":false,"M":true,"O":1790000000120,"Z":"129.99998000","Y":"129.99998000","Q":"0.00000000","W":1790000000120,"V":"EXPIRE_MAKER"}}"#;
    const TRADE_QUOTE: &[u8] = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1790000000124,"s":"BTCUSDT","c":"mv6512ab0f2000000000000006700000","S":"SELL","o":"LIMIT","f":"IOC","q":"0.00200000","p":"64999.98000000","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"TRADE","X":"FILLED","r":"NONE","i":8003,"l":"0.00200000","z":"0.00200000","L":"64999.99000000","n":"0.12999998","N":"USDT","T":1790000000121,"t":9003,"I":1790000000122,"w":false,"m":false,"M":true,"O":1790000000120,"Z":"129.99998000","Y":"129.99998000","Q":"0.00000000","W":1790000000120,"V":"EXPIRE_MAKER"}}"#;
    const EXPIRED: &[u8] = br#"{"subscriptionId":0,"event":{"e":"executionReport","E":1790000000125,"s":"BTCUSDT","c":"mv6512ab0f2000000000000006800000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.00300000","p":"65000.00000000","P":"0.00000000","F":"0.00000000","g":-1,"C":"","x":"EXPIRED","X":"EXPIRED","r":"NONE","i":8004,"l":"0.00000000","z":"0.00150000","L":"0.00000000","n":"0","N":null,"T":1790000000122,"t":-1,"I":1790000000123,"w":false,"m":false,"M":false,"O":1790000000120,"Z":"97.49998500","Y":"0.00000000","Q":"0.00000000","W":1790000000120,"V":"EXPIRE_MAKER","eR":"UNFILLED_IOC_QUANTITY_EXPIRED"}}"#;
    const POSITION: &[u8] = br#"{"subscriptionId":0,"event":{"e":"outboundAccountPosition","E":1790000000126,"u":1790000000126,"B":[{"a":"BTC","f":"0.00199800","l":"0.00000000"},{"a":"USDT","f":"9870.00002000","l":"0.00000000"},{"a":"BNB","f":"0.99999000","l":"0.00000000"}]}}"#;
    const ENDED: &[u8] = br#"{"subscriptionId":0,"event":{"e":"eventStreamTerminated","E":1790000000127}}"#;
    // The subscription's answer: live only on the id it names.
    const SUBSCRIBED: &[u8] = br#"{"id":21,"status":200,"result":{"subscriptionId":0},"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","interval":"MINUTE","intervalNum":1,"limit":6000,"count":4}]}"#;
    // BX13 (R2-2): `account.rateLimits.orders`' answer, the docs' shape.
    const RATE_LIMITS: &[u8] = br#"{"id":23,"status":200,"result":[{"rateLimitType":"ORDERS","interval":"SECOND","intervalNum":10,"limit":100,"count":0},{"rateLimitType":"ORDERS","interval":"DAY","intervalNum":1,"limit":200000,"count":1234}],"rateLimits":[{"rateLimitType":"REQUEST_WEIGHT","interval":"MINUTE","intervalNum":1,"limit":6000,"count":40}]}"#;
    // The reconciliation's reads: the account over the session, then REST.
    const ACCOUNT: &[u8] = br#"{"id":19,"status":200,"result":{"makerCommission":10,"takerCommission":10,"buyerCommission":0,"sellerCommission":0,"commissionRates":{"maker":"0.00100000","taker":"0.00100000","buyer":"0.00000000","seller":"0.00000000"},"canTrade":true,"canWithdraw":false,"canDeposit":true,"brokered":false,"requireSelfTradePrevention":false,"preventSor":false,"updateTime":1790000000000,"accountType":"SPOT","balances":[{"asset":"BNB","free":"0.99999000","locked":"0.00000000"},{"asset":"BTC","free":"0.00199800","locked":"0.00000000"},{"asset":"USDT","free":"9870.00002000","locked":"0.00000000"}],"permissions":["SPOT"],"uid":354937868}}"#;
    const OPEN: &[u8] = br#"[{"symbol":"BTCUSDT","orderId":7002,"orderListId":-1,"clientOrderId":"web_stranger_1","price":"90000.00000000","origQty":"0.00100000","executedQty":"0.00000000","cummulativeQuoteQty":"0.00000000","status":"NEW","timeInForce":"GTC","type":"LIMIT","side":"SELL","stopPrice":"0.00000000","icebergQty":"0.00000000","time":1790000000000,"updateTime":1790000000000,"isWorking":true,"workingTime":1790000000000,"origQuoteOrderQty":"0.00000000","selfTradePreventionMode":"EXPIRE_MAKER"}]"#;
    const TRADES: &[u8] = br#"[{"symbol":"BTCUSDT","id":501,"orderId":4001,"orderListId":-1,"price":"64000.00000000","qty":"0.00100000","quoteQty":"64.00000000","commission":"0.00000100","commissionAsset":"BTC","time":1790000000001,"isBuyer":true,"isMaker":false,"isBestMatch":true},{"symbol":"BTCUSDT","id":502,"orderId":4002,"orderListId":-1,"price":"64100.00000000","qty":"0.00100000","quoteQty":"64.10000000","commission":"0.06410000","commissionAsset":"USDT","time":1790000000002,"isBuyer":false,"isMaker":false,"isBestMatch":true}]"#;
    const TICKER: &[u8] = br#"{"symbol":"BTCUSDT","price":"65000.01000000"}"#;

    /// The send window, as gate 93's (`WsConn`'s writer).
    struct Window {
        buf: [u8; 4_096],
        n: usize,
        mask: u64,
    }
    impl FrameSink for Window {
        fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), core_net::WsErr> {
            self.mask = self.mask.wrapping_add(1);
            let m = core_net::ws_mask_from_counter(self.mask);
            self.n = core_net::ws_write_text_frame_with(&mut self.buf, parts, m).map_err(|_| core_net::WsErr::Overflow)?;
            Ok(())
        }
    }

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 5_000);
    let mut disc = ingress_binance::discovery::BnDiscovery::new();
    disc.ingest_body(exec_binance::testvenue::SPOT).expect("gate 109 discovery");
    let (mut table, mut wire) = InstTable::new(7);
    let row = disc.find(b"BTCUSDT").expect("gate 109 row");
    let r = table
        .bind(&mut wire, &BindSpec { sym: 7, product: PRODUCT_SPOT, row, owned: true, maker_ok: false })
        .expect("gate 109 bind");
    let w = *wire.row(r);
    assert_eq!((w.px_dec, w.qty_dec), (2, 5), "spot's BTCUSDT: tick 0.01, step 0.00001");
    // The armed base assets, as the gateway holds them (`spot_assets`).
    let assets = [wire.spot(r).base];
    assert_eq!(assets[0].get(), b"BTC");
    // The armed row whose base asset is `asset` (the gateway's
    // `spot_row_of`, over its one armed row).
    let spot_row_of = |asset: &[u8]| if assets[0].get() == asset { r } else { ROW_NONE };
    let mut win = Window { buf: [0; 4_096], n: 0, mask: 0x6514 };
    let mut tids = TidRing::<256>::new();
    let (j_p, mut j_c) = Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
    let mut journal = JournalTx::new(j_p);
    let mut bal = [0i64; 1];
    let mut perm = BnPermissions::EMPTY;
    let mut orders = [OrderRow::default(); 8];
    let mut trades = [TradeRow::default(); 8];
    let mut legs = [Leg::default(); 1];
    legs[0].owned = true;
    let mut a = WsAnswer::default();
    let mut ev = UserEvent::default();

    let g = AllocGuard::new();
    let (mut rendered, mut fees, mut paid, mut again) = (0usize, [0u64; 4], 0u64, 0u64);
    let (mut others, mut ends, mut order_fees, mut not_paid) = (0u64, 0u64, 0u64, 0u64);
    let mut days = 0u64;
    // The explained balance ×1e8 (the gateway's `spot_1e8`): what the
    // base-asset BUY booked, less its commission.
    let mut explained_1e8 = 0i64;
    let mut i = 0u64;
    while i < N {
        let now = core_time::now_ns();
        let ts = 1_790_000_000_000 + i;
        let cid = Part::Cid(&prefix, 2, 0x65 + i);
        // The order session's requests: spot's place (no `reduceOnly`),
        // the in-doubt query.
        let o = Place {
            symbol: w.symbol(),
            side: (i & 1) as u8,
            maker: false,
            reduce_only: false,
            qty_1e6: 2_000,
            qty_dec: w.qty_dec,
            px_1e6: 65_000_000_000 + (i % 100) as i64 * 10_000,
            px_dec: w.px_dec,
            cid,
            ts_ms: ts,
        };
        queue_place(&mut win, 17 + i, &st, &o).expect("gate 109 place");
        rendered += win.n;
        queue_status(&mut win, 18 + i, &st, w.symbol(), cid, ts).expect("gate 109 status");
        rendered += win.n;
        // Their answers.
        scan_answer(PLACED, &mut a).expect("gate 109 ack");
        assert!(!a.user && a.venue_oid == 8_001, "gate 109: the ACK names the order");
        std::hint::black_box(prefix.classify(a.cid.get(PLACED)));
        scan_answer(STATUS, &mut a).expect("gate 109 status answer");
        assert_eq!((a.order_status, a.avg_px_1e6), (S_FILLED, 64_999_990_000), "gate 109: spot's average, quote over quantity");
        // The subscription's frames, each told from an answer and its
        // event read in place.
        let mut k = 0u64;
        for f in [NEW, TRADE_BNB, TRADE_BASE, TRADE_QUOTE, EXPIRED, POSITION] {
            scan_answer(f, &mut a).expect("gate 109 frame");
            assert!(a.user, "gate 109: a subscription's frame");
            scan_spot_event(f, a.event.at as usize, &mut ev).expect("gate 109 event");
            if ev.kind != UE_ORDER {
                others += u64::from(ev.kind == UE_OTHER);
                continue;
            }
            std::hint::black_box(prefix.classify(ev.cid.get(f)));
            let row = wire.find(PRODUCT_SPOT, ev.symbol.get(f));
            assert_eq!(row, r, "gate 109: the event's row, by product");
            let fa = ev.fee_asset.get(f);
            let (asset, fee_row) = (fee_asset(wire.row(row), fa), spot_row_of(fa));
            match ev.exec {
                X_NEW => assert_eq!(ev.status, S_NEW),
                X_TRADE => {
                    assert_eq!((ev.status, ev.last_qty_1e6, ev.last_px_1e6), (S_FILLED, 2_000, 64_999_990_000));
                    // A fresh trade each pass (ids reused only after the
                    // ring has turned over).
                    let tid = ev.trade_id + 10 * (i % 512);
                    let seen = tids.see(row, tid, true);
                    assert!(!seen.again && seen.fee_due, "gate 109: a fresh trade, its commission owed");
                    // Booked: its quantity, up to the order's `z`.
                    let q = ev.last_qty_1e6.min(ev.cum_qty_1e6).max(0);
                    let wr = wire.row(row);
                    let fee = core_types::ExecRecord {
                        ts_ns: now,
                        wall_ms: ts,
                        client_oid: 0x65 + i,
                        venue_oid: ev.venue_oid,
                        a: ev.last_px_1e6,
                        b: fee_notional_1e9(wr, ev.last_px_1e6, ev.last_qty_1e6),
                        c: ev.commission_1e9,
                        code: fee_code(ev.maker != 0, asset, TRADE_BY_MEMBER, wr),
                        row,
                        kind: J_FEE,
                        slot: 2,
                    };
                    assert!(journal.note(&fee), "gate 109: the journal's ring has room");
                    fees[asset as usize] += 1;
                    if fee_row != ROW_NONE {
                        // Charged in the armed base: out of its explained
                        // balance (×1e9 → ×1e8).
                        explained_1e8 = q * 100 - ev.commission_1e9 / 10;
                        paid += 1;
                    }
                    // The same trade reported again: booked and owed once.
                    let dup = tids.see(row, tid, true);
                    again += u64::from(dup.again && !dup.fee_due);
                    k += 1;
                }
                X_EXPIRED => {
                    // S1: the order's `z`, none of it booked here (its
                    // trade's report lost) — the excess at the implied
                    // average; the gateway then asks for its trades (REST).
                    assert_eq!((ev.status, ev.cum_qty_1e6, ev.avg_px_1e6), (S_EXPIRED, 1_500, 64_999_990_000));
                    let excess = ev.cum_qty_1e6;
                    std::hint::black_box(exec_binance::gateway::implied_px_1e6(ev.cum_qty_1e6, ev.avg_px_1e6, 0, excess));
                }
                _ => unreachable!("gate 109: no other execution"),
            }
        }
        assert_eq!(k, 3, "gate 109: three trades a pass");
        while let Some(x) = j_c.try_pop_ref() {
            std::hint::black_box((x.b, x.c));
        }
        // The reconciliation and the session's subscription, at their own
        // cadence.
        if i % 64 == 0 {
            queue_subscribe(&mut win, 21 + i).expect("gate 109 subscribe");
            rendered += win.n;
            scan_answer(SUBSCRIBED, &mut a).expect("gate 109 subscribed");
            assert!(scan_subscribed(SUBSCRIBED, a.result), "gate 109: the subscription names its id");
            queue_account(&mut win, 22 + i, &st, PRODUCT_SPOT, ts).expect("gate 109 account");
            rendered += win.n;
            // BX13 (R2-2): spot's day count — asked, and its one DAY entry
            // read from the answer in place.
            queue_rate_limits(&mut win, 23 + i, &st, ts).expect("gate 109 rate limits");
            rendered += win.n;
            scan_answer(RATE_LIMITS, &mut a).expect("gate 109 rate limits answer");
            days += u64::from(scan_day_orders(RATE_LIMITS, a.result) == Some((1_234, 200_000)));
            scan_answer(ENDED, &mut a).expect("gate 109 end");
            scan_spot_event(ENDED, a.event.at as usize, &mut ev).expect("gate 109 end event");
            ends += u64::from(a.user && ev.kind == UE_STREAM_END);
            scan_answer(ACCOUNT, &mut a).expect("gate 109 account answer");
            let snap = scan_spot_account(ACCOUNT, a.result, &assets, &mut bal, &mut perm).expect("gate 109 account");
            assert!(snap.can_trade && judge_permission(&wire.spot(r).perm, &perm).is_ok(), "gate 109: the account may trade the row");
            legs[0].venue_1e6 = bal[0].div_euclid(100);
            legs[0].booked_1e6 = explained_1e8.div_euclid(100);
            let mut v = Verdict::default();
            judge_positions(&mut legs, &mut v);
            assert_eq!((v.drift_legs, v.unseen_legs), (0, 0), "gate 109: the balance explained");
            let n = scan_orders(OPEN, &mut orders).expect("gate 109 open orders");
            std::hint::black_box(prefix.classify(orders[n - 1].cid.get(OPEN)));
            let n = scan_trades(TRADES, &mut trades).expect("gate 109 trades");
            assert_eq!(day_buys_1e6(&trades[..n]), 64_000_000, "gate 109: the day's buys");
            // The same answer as order 4001's trades (a fill booked without
            // its report): its trade's commission owed once, another
            // order's not paid.
            let mut t = 0;
            while t < n {
                let x = trades[t];
                t += 1;
                let fa = x.fee_asset().get(TRADES);
                let ours = x.venue_oid == 4_001 && x.symbol.get(TRADES) == w.symbol();
                let (asset, fee_row) = (fee_asset(&w, fa), spot_row_of(fa));
                if !ours || !tids.see(r, x.trade_id * 100_000 + i, true).fee_due {
                    not_paid += 1;
                    continue;
                }
                std::hint::black_box((fee_code(x.maker != 0, asset, TRADE_BY_MEMBER, &w), fee_notional_1e9(&w, x.px_1e6, x.qty_1e6)));
                order_fees += u64::from(fee_row == r);
            }
            assert_eq!(scan_ticker_price(TICKER, b"BTCUSDT"), Ok(65_000_010_000), "gate 109: the adoption's price");
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(rendered > 0, "the gate rendered nothing");
    assert_eq!(
        (fees[FEE_ASSET_BNB as usize], fees[FEE_ASSET_OTHER as usize], fees[FEE_ASSET_SETTLE as usize]),
        (N, N, N),
        "gate 109: every trade's commission noted once — in BNB, in the base, in the quote"
    );
    assert_eq!((paid, again), (N, 3 * N), "gate 109: the base's commission paid from its balance; a second report owes nothing");
    assert_eq!(others, N, "gate 109: the balances counted, never booked");
    assert_eq!(
        (ends, order_fees, not_paid),
        (N.div_ceil(64), N.div_ceil(64), N.div_ceil(64)),
        "gate 109: the subscription's end read; the order's trade paid, another order's not"
    );
    assert_eq!(days, N.div_ceil(64), "gate 109: spot's day count read, every cadence (BX13)");
    std::hint::black_box(&table);
    assert_eq!(allocs, 0, "gate 109: spot's render and scans allocated {allocs} times ({bytes} B)");
}

/// Gate 110's round: two IoCs over the options lane's REST route — each
/// one `POST /eapi/v1/order` sealed and its answer opened, and the
/// stream's two frames for it (NEW, the TRADE that fills it), each its own
/// record (the venue writes a frame a write).
const G110_ROUND: u64 = 8;
/// Gate 110's reconciliation cycle (the sweep's confirmation): the options
/// account on the route, the positions and the listing on the jobs — each
/// request sealed, its answer opened.
const G110_RECON: u64 = 6;

/// What the engine end saw (gate 110).
#[derive(Clone, Copy, Default)]
struct Seen110 {
    acks: u64,
    filled: u64,
    retired_other: u64,
    rejects: u64,
    locks: u64,
    scan_fails: u64,
    positions: u64,
    margins: u64,
    fills: u64,
    bought: i64,
    sold: i64,
    j_fill: u64,
    j_fee: u64,
    /// `J_FEE`s on an option row, in its settle asset, a charge (> 0).
    j_fee_opt: u64,
    pulses: u64,
    sweeps_done: u64,
    recons: u64,
    reconciled: u64,
}

/// **BX9 gate 110 — the options lane's whole step, over a real rustls
/// loopback, allocates only rustls' record buffers** (gate 108's harness,
/// options' lane).
///
/// The REAL gateway (`BnGateway::step`, booted as the engine boots it)
/// OPTIONS-ONLY — the call and the put of the scripted venue's eapi
/// (`exec_binance::testvenue::EAPI`) bound IoC-only, the call writable —
/// against the venue in gate 94's CHILD process (`GATE94_OPT=1`: a trade
/// pushes no `ACCOUNT_UPDATE`, whose paced margin re-read would land in a
/// window at its own time). The arm key Ed25519, the options key its own
/// HMAC pair (Q-1): every eapi request under its header, HMAC-signed in
/// place. Every round: an IoC BUY of 0.01 call at 500, then — once it
/// ended — an IoC SELL of 0.01 at 500 (a long closed: the booked position
/// long, then flat — an `EVT_POSITION` each way); each filled whole at its
/// limit. Each IoC is ONE `POST /eapi/v1/order` on the lane's REST route
/// (one record sealed, about 530 B) and its answer (one record opened,
/// about 520 B), and the `/private/ws/` stream's two frames — the NEW and
/// the TRADE (`FILLED`), each its own record: the trade booked once (lane
/// 4, `J_FILL`), its commission — NEGATIVE on eapi's stream — journaled
/// once as a charge in the settle asset (`J_FEE`), the IoC retired FILLED
/// (its ACK when the route's answer beats the stream's end). Pinned like
/// gates 94b and 108: rustls 0.23's buffered API allocates one `Vec` per
/// record it seals and one per application-data record it opens —
/// [`G110_ROUND`] a round. Then, measured alone, **a reconciliation
/// cycle** — the halt's sweep: no order rests, so nothing is cancelled and
/// the confirmation is the whole cycle — `marginAccount` on the route,
/// `position` and `openOrders` on the jobs: [`G110_RECON`]; the sweep
/// reported done, the cycle reconciled — the PUT held long through it
/// (bought before the window: the position read values an open leg at its
/// mark and judges it matched; the alloc review), the call flat — the
/// options margin sampled (`EVT_MARGIN`).
///
/// NOT pinned here (the alloc review's list): the boot (the clock on
/// eapi, the keys' permissions, the listenKey, the day a row at a time);
/// the listenKey's keep-alive (periodic, after the boot; the generic job
/// render, as the futures lanes' — unpinned there too);
/// the paced margin re-read an `ACCOUNT_UPDATE` asks for (the same route
/// request as the cycle's first); the exercise records and a settlement
/// fill (REST, as the listing; its scan, price and agreement are gate
/// 111's — not `exercise_answered`'s check of a page naming a row twice,
/// span compares in place); an unfilled IoC's stream `EXPIRED` and a `RISK_LEVEL_CHANGE`
/// (scanned at 0 B in gate 111; a lock is cold, once); a place on an
/// expired row (`CODE_EXPIRED`: refused before a byte is rendered); an
/// in-doubt order's `GET /eapi/v1/order` (rendered, signed and its answer
/// scanned in gate 111; on a socket only after a lost answer); bursts over
/// the 4 KiB window. A break-and-watch (an 8-byte `Vec` in the options commission's
/// normalization — two order events an IoC — and one in
/// `opt_account_answered`) failed the rounds at 3 600 and the cycle at 7.
#[test]
fn binance_options_lane_round_trip_allocates_only_rustls_record_buffers() {
    use clob_dispatcher::RETIRED_FILLED;
    use core_config::SecretKeyBytes;
    use core_fill::ORDER_KIND_IOC;
    use core_ring::{Consumer, Ring};
    use core_types::{ExecRecord, Fill, Side};
    use exec_binance::arm::{
        GW_FILLS_BOOKED, GW_FILLS_DEFERRED, GW_FILLS_FOREIGN, GW_FILLS_LOST, GW_FILLS_UNOWNED, GW_FILLS_UNRESOLVED, GW_IN_DOUBT_RAISED,
        GW_JOBS_DROPPED, GW_JOURNAL_DROPPED, GW_RECON_DRIFT_LEGS, GW_RECON_FAILED, GW_RECON_OK, GW_RECON_UNSEEN_LEGS, GW_TALLIES,
    };
    use exec_binance::cmd::{
        duplex, BnCmd, BnEngineEnd, EVT_ACK, EVT_F_RECONCILED, EVT_F_SWEEP_DONE, EVT_LOCK, EVT_MARGIN, EVT_POSITION, EVT_RECON,
        EVT_REJECT, EVT_RETIRED, EVT_SCAN_FAIL, EVT_STATUS, EVT_SWEEP,
    };
    use exec_binance::config::{BnConfig, HmacSigner, Scope};
    use exec_binance::gateway::{BnGateway, GwKnobs, Phase};
    use exec_binance::inst::{InstTable, OptBindSpec, PRODUCT_OPTIONS};
    use exec_binance::journal::{fee_fields, JournalTx, FEE_ASSET_SETTLE, JOURNAL_RING, J_FEE, J_FILL};
    use exec_binance::mode::AccountMode;
    use exec_binance::testvenue::{EAPI, EPOCH, KEY, OPT_CALL, OPT_HMAC_SECRET, OPT_KEY, OPT_PUT, SEED, SLOT};
    use exec_gateway::stats::SWEEP_CLEAR;
    use std::time::{Duration, Instant};

    const FILL_N: usize = 1_024;
    const ROUNDS: u64 = 300;
    const WARM: u64 = 50;
    const LONG_MS: u64 = 3_600_000;
    const PX: i64 = 500_000_000;
    const QTY: i64 = 10_000;
    const CLOCK_CLEAR: Duration = Duration::from_secs(50);

    type Gw = BnGateway<FILL_N>;

    fn pump(gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen110) {
        gw.step();
        while let Some(g) = eng.evt.pop() {
            let (kind, why, flags) = (g.kind, g.why, g.flags);
            drop(g);
            match kind {
                EVT_ACK => s.acks += 1,
                EVT_RETIRED if why == RETIRED_FILLED => s.filled += 1,
                EVT_RETIRED => s.retired_other += 1,
                EVT_REJECT => s.rejects += 1,
                EVT_LOCK => s.locks += 1,
                EVT_SCAN_FAIL => s.scan_fails += 1,
                EVT_POSITION => s.positions += 1,
                EVT_MARGIN => s.margins += 1,
                EVT_STATUS => s.pulses += 1,
                EVT_SWEEP => s.sweeps_done += u64::from(flags & EVT_F_SWEEP_DONE != 0),
                EVT_RECON => {
                    s.recons += 1;
                    s.reconciled += u64::from(flags & EVT_F_RECONCILED != 0);
                }
                _ => {}
            }
        }
        while let Some(g) = lane.try_pop_ref() {
            if matches!(g.side, Side::Bid) {
                s.bought += g.qty.0;
            } else {
                s.sold += g.qty.0;
            }
            drop(g);
            s.fills += 1;
        }
        while let Some(g) = journal.try_pop_ref() {
            s.j_fill += u64::from(g.kind == J_FILL);
            if g.kind == J_FEE {
                let (_, asset, _, product, _, _) = fee_fields(g.code);
                s.j_fee += 1;
                s.j_fee_opt += u64::from(asset == FEE_ASSET_SETTLE && product == PRODUCT_OPTIONS && g.c > 0);
            }
            drop(g);
        }
    }

    fn tallies(gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen110) -> [u64; GW_TALLIES] {
        let (p0, t) = (s.pulses, Instant::now());
        while s.pulses == p0 {
            assert!(t.elapsed() < Duration::from_secs(2), "gate 110: no pulse");
            pump(gw, eng, lane, journal, s);
        }
        let mut w = [0u64; GW_TALLIES];
        let mut i = 0;
        while i < GW_TALLIES {
            w[i] = eng.stats.published(i);
            i += 1;
        }
        w
    }

    // ---- the venue, in its own process (gate 94's helper, options' knob) ----
    let dir = std::env::temp_dir().join(format!("mv-gate110-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 110 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 110 exe"))
        .args(["gate94_venue_helper", "--exact", "--ignored", "--test-threads=1"])
        .env("GATE94_VENUE_DIR", &dir)
        .env("GATE94_REST", "0")
        .env("GATE94_OPT", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 110 child");
    let deadline = Instant::now() + Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 110 port");
        }
        assert!(Instant::now() < deadline, "gate 110: the venue never came up");
        std::thread::sleep(Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 110 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der)).expect("gate 110 anchor");
    let tls = std::sync::Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());

    // ---- the gateway, booted options-only as the engine boots it ----
    let seed = SecretKeyBytes::new_locked(SEED).expect("gate 110 seed");
    let h = HmacSigner::from_secret(OPT_HMAC_SECRET).expect("gate 110 secret");
    let cfg = BnConfig::loopback(Scope::Live, String::from(KEY), seed, port)
        .and_then(|c| c.with_options_hmac(String::from(OPT_KEY), h))
        .expect("gate 110 config");
    let mut d = ingress_binance::eapi::EapiDiscovery::new();
    d.ingest_exchange_info(EAPI).expect("gate 110 discovery");
    let ahead = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_millis() as i64 + 30 * 86_400_000;
    let (mut table, mut wire) = InstTable::new(core_config::universe::LEGACY_BN_ANCHOR_SYM);
    for (k, sym) in [OPT_CALL, OPT_PUT].into_iter().enumerate() {
        let mut row = *d.rows().iter().find(|r| r.symbol() == sym.as_bytes()).expect("gate 110 row");
        row.expiry_ms = ahead;
        let id = core_types::make_symbol_id(VenueId::Binance, 1_025 + k as u32);
        table.bind_option(&mut wire, &OptBindSpec { sym: id, row: &row, owned: true, writable: k == 0 }).expect("gate 110 bind");
    }
    let (mut eng, gw_end) = duplex();
    let (fill_tx, mut lane) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, mut journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: AccountMode::Classic,
        recv_window_ms: 5_000,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 2 * LONG_MS,
        heartbeat_ms: LONG_MS,
        recon_every_ms: LONG_MS,
        spin: false,
        rotate_after_ns: LONG_MS * 1_000_000,
        rest_orders: false,
        pnl_anchor_1e6: 0,
        boot_timeout_ns: 20_000_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: 0,
        equity: None,
    };
    let mut gw = Gw::new(&cfg, tls, knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx)).expect("gate 110 gateway");
    gw.boot().expect("gate 110 boot");
    assert_eq!(gw.phase(), Phase::Ready);
    let ready_at = Instant::now();
    let mut s = Seen110::default();
    let answers = |eng: &BnEngineEnd| eng.stats.io()[1];
    let rest_jobs = |eng: &BnEngineEnd| eng.stats.rest_rtt.count();

    // Until no REST answer for 300 ms: whatever the boot left queued.
    let t = Instant::now();
    let (mut seen, mut quiet) = (rest_jobs(&eng), Instant::now());
    while quiet.elapsed() < Duration::from_millis(300) {
        assert!(t.elapsed() < Duration::from_secs(10), "gate 110: the boot's REST never settled");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        if rest_jobs(&eng) != seen {
            (seen, quiet) = (rest_jobs(&eng), Instant::now());
        }
    }

    // One IoC on the call: pushed, then pumped until it ended — the route
    // answered, booked on lane 4, its commission journaled, retired.
    let mut oid = 1_000_000u64;
    let mut ioc = |gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen110, row: u16, side: Side| {
        oid += 1;
        let (a0, e0, l0, fee0) = (answers(eng), s.filled, s.fills, s.j_fee);
        assert!(eng.cmd.push(&BnCmd::place(oid, PX, QTY, 0, row, ORDER_KIND_IOC, side, SLOT, 0).at(core_time::now_ns())));
        let t = Instant::now();
        while !(answers(eng) > a0 && s.filled > e0 && s.fills > l0 && s.j_fee > fee0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 110: an IoC stalled");
            pump(gw, eng, lane, journal, s);
        }
    };
    let mut w = 0u64;
    while w < WARM {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, 0, Side::Bid);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, 0, Side::Ask);
        w += 1;
    }

    // ---- the rounds ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 110: the run neared the clock's 60 s round");
    let t_warm = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    let (warm, jobs0) = (s, rest_jobs(&eng));
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, 0, Side::Bid);
        ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, 0, Side::Ask);
        r += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let (rounds, jobs1) = (s, rest_jobs(&eng));
    let t_rounds = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);

    // ---- a reconciliation cycle, measured alone: the sweep's confirmation ----
    // The put bought first and held through it: an open leg in the read.
    ioc(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, 1, Side::Bid);
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 110: the run neared the clock's 60 s round");
    let (before, jobs2) = (s, rest_jobs(&eng));
    let g = AllocGuard::new();
    let swept = eng.stats.request_sweep();
    let t = Instant::now();
    while !(eng.stats.sweep() == (swept, SWEEP_CLEAR) && s.sweeps_done > before.sweeps_done && s.recons > before.recons) {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 110: the sweep's cycle never ran");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    let (recon_allocs, recon_bytes, _) = g.delta();
    let (recon, jobs3) = (s, rest_jobs(&eng));
    let t_recon = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    let verdict = gw.last_verdict();
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();

    let both = 2 * ROUNDS;
    assert_eq!(
        (rounds.filled - warm.filled, rounds.fills - warm.fills, rounds.j_fill - warm.j_fill, rounds.j_fee_opt - warm.j_fee_opt),
        (both, both, both, both),
        "gate 110: every IoC filled whole, booked once on lane 4 and journaled once, its commission a charge in the settle asset"
    );
    assert_eq!(
        (rounds.bought - warm.bought, rounds.sold - warm.sold, rounds.positions - warm.positions),
        (ROUNDS as i64 * QTY, ROUNDS as i64 * QTY, both),
        "gate 110: every round bought 0.01 and sold it back — the position long, then flat"
    );
    assert!(rounds.acks - warm.acks <= both, "gate 110: an ACK at most an IoC (the stream's end may beat it)");
    assert_eq!(
        (recon.sweeps_done - before.sweeps_done, recon.recons - before.recons, recon.reconciled - before.reconciled, recon.margins - before.margins),
        (1, 1, 1, 1),
        "gate 110: the sweep done, one cycle judged and reconciled, the options margin sampled"
    );
    assert_eq!(before.bought - rounds.bought, QTY, "gate 110: the put held long through the cycle");
    assert_eq!(
        (verdict.drift_legs, verdict.unseen_legs, verdict.ghosts, verdict.orphans, verdict.foreign),
        (0, 0, 0, 0, 0),
        "gate 110: the verdict"
    );
    assert_eq!(
        (recon.rejects, recon.retired_other, recon.scan_fails, recon.locks),
        (0, 0, 0, 0),
        "gate 110: nothing refused, nothing else retired, no frame failed its scan, no lock"
    );
    assert_eq!(
        (t_rounds[GW_FILLS_BOOKED] - t_warm[GW_FILLS_BOOKED], t_recon[GW_RECON_OK] - t_rounds[GW_RECON_OK]),
        (both, 1),
        "gate 110: a fill an IoC; the cycle reconciled once"
    );
    assert_eq!(
        (t_recon[GW_RECON_FAILED] - t_rounds[GW_RECON_FAILED], t_recon[GW_RECON_DRIFT_LEGS], t_recon[GW_RECON_UNSEEN_LEGS]),
        (0, 0, 0),
        "gate 110: the cycle — not failed, no drift leg, no unseen leg"
    );
    let zero = [
        GW_FILLS_FOREIGN,
        GW_FILLS_UNOWNED,
        GW_FILLS_DEFERRED,
        GW_FILLS_UNRESOLVED,
        GW_FILLS_LOST,
        GW_JOBS_DROPPED,
        GW_IN_DOUBT_RAISED,
        GW_JOURNAL_DROPPED,
    ]
    .map(|k| t_recon[k]);
    assert_eq!(zero, [0; 8], "gate 110: no foreign, unowned, deferred, unresolved or lost fill; no job, doubt or record dropped");
    assert_eq!((jobs1 - jobs0, jobs3 - jobs2), (0, 2), "gate 110: no REST job in the rounds; the cycle's positions and listing");
    assert_eq!(
        (allocs, recon_allocs),
        (G110_ROUND * ROUNDS, G110_RECON),
        "gate 110: the rounds and the cycle allocated {allocs} ({bytes} B) and {recon_allocs} ({recon_bytes} B) — rustls' records only"
    );
}

/// **BX9 gate 111 — options' renders and scans are 0 B/op** (gate 109's
/// shape, eapi's frames; in-process, no socket). What the gateway does per
/// option order and per options cycle that the other gates do not already
/// measure: `POST /eapi/v1/order` (`clientOrderId`, LIMIT + IOC, the
/// reduce-only of the writing law), the in-doubt `GET /eapi/v1/order` and
/// `marginAccount` rendered and HMAC-signed in place (Q-1); the place's ACK
/// and the query's answer (`quantity`, never `origQty`) scanned; the
/// stream's `ORDER_TRADE_UPDATE` NEW, TRADE (a NEGATIVE commission, taken
/// as a charge) and EXPIRED, the options `ACCOUNT_UPDATE` (`B`/`P`, no `a`)
/// and `RISK_LEVEL_CHANGE` read in place, each order event's row found by
/// product, its trade seen once by the dedupe ring, its commission
/// classified (USDT: the settle asset) and noted into the journal's ring.
/// At their own cadence: the options account (`scan_opt_margin`: the
/// ratio, the equity, `canTrade`, `reduceOnly`), the positions
/// (`scan_opt_positions`: side-signed, mark-valued) judged as legs — one
/// SETTLING — the open orders (`ACCEPTED`), the day's trades (eapi's
/// `userTrades`: `quantity`, `side`), the exercise records priced at their
/// intrinsic value (`settle_px_1e6`) and judged against the booked leg
/// (`settlement_agrees`, R4), the index (`scan_opt_index`). And the
/// router's ledger on an option row (Q-2): a summary's mark judged sane
/// (`option_mark_sane`) and stamped fresh, a long's and a short's probe,
/// the staleness sweep — and, every 64th turn, the sweep past the stale
/// age (`INST_STALE` raised) and both probes refused `Unpriced` (opening
/// on a stale row), the next summary freshening it. Zero allocations. A
/// break-and-watch (an 8-byte `Vec` in `render_opt_place` and one in
/// `scan_opt_index`) failed it at 20 313 (20 000 places, 313 cycles).
#[test]
fn binance_options_lane_render_and_scan_steady_state() {
    use exec_binance::cid::CidPrefix;
    use exec_binance::config::{HmacSigner, Signer};
    use exec_binance::gateway::settle_px_1e6;
    use exec_binance::inst::{InstTable, OptBindSpec, PRODUCT_OPTIONS};
    use exec_binance::journal::{fee_asset, fee_code, fee_notional_1e9, JournalTx, FEE_ASSET_SETTLE, JOURNAL_RING, J_FEE, TRADE_BY_MEMBER};
    use exec_binance::margin::um_ratio_1e6;
    use exec_binance::oot::TidRing;
    use exec_binance::recon::{
        day_increasing_1e6, judge_positions, scan_exercise, scan_opt_margin, scan_opt_positions, settlement_agrees, ExerciseRow, Leg, PosRow, Verdict,
    };
    use exec_binance::rest::{
        render_account, render_opt_by_cid, render_opt_place, scan_opt_index, scan_orders, scan_trades, OrderRow, QueryWriter, TradeRow,
    };
    use exec_binance::userstream::{scan_user_event, UserEvent, RISK_NORMAL, S_NEW, UE_ACCOUNT, UE_ORDER, UE_RISK, X_EXPIRED, X_NEW, X_TRADE};
    use exec_binance::wsapi::{scan_rest_answer, Part, Place, WsAnswer, WsStatic};
    use exec_router::ledger::MARK_STALE_NS;
    use exec_router::{InstRefusal, InstrumentSpec, Ledger, INST_CALL, INST_WRITABLE, LAW_OPTION};

    const N: u64 = 20_000;
    const PLACED: &[u8] = br#"{"orderId":8001,"symbol":"BTC-261225-85000-C","price":"500","quantity":"0.01","executedQty":"0","fee":"0","side":"BUY","type":"LIMIT","timeInForce":"IOC","reduceOnly":false,"postOnly":false,"createTime":1790000000000,"updateTime":1790000000000,"status":"NEW","avgPrice":"0","clientOrderId":"mv6512ab0f2000000000000006500000","priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT","mmp":false}"#;
    const STATUS: &[u8] = br#"{"orderId":8001,"symbol":"BTC-261225-85000-C","price":"500","quantity":"0.01","executedQty":"0.01","fee":"0.0015","side":"BUY","type":"LIMIT","timeInForce":"IOC","reduceOnly":false,"postOnly":false,"createTime":1790000000000,"updateTime":1790000000001,"status":"FILLED","avgPrice":"500","clientOrderId":"mv6512ab0f2000000000000006500000","priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT","mmp":false}"#;
    const NEW: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000123,"T":1790000000123,"o":{"s":"BTC-261225-85000-C","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.01","p":"500.000","ap":"0.000","sp":"0","x":"NEW","X":"NEW","i":8001,"l":"0.00","z":"0.00","L":"0.000","N":"USDT","n":"0","T":1790000000123,"t":0,"b":"0","a":"0","m":false,"R":false,"ps":"BOTH","rp":"0","er":"0","V":"EXPIRE_MAKER"}}"#;
    const TRADE: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000124,"T":1790000000124,"o":{"s":"BTC-261225-85000-C","c":"mv6512ab0f2000000000000006500000","S":"BUY","o":"LIMIT","f":"IOC","q":"0.01","p":"500.000","ap":"500.000","sp":"0","x":"TRADE","X":"FILLED","i":8001,"l":"0.01","z":"0.01","L":"500.000","N":"USDT","n":"-0.001500","T":1790000000124,"t":9001,"b":"0","a":"0","m":false,"R":false,"ps":"BOTH","rp":"0","er":"0","V":"EXPIRE_MAKER"}}"#;
    const EXPIRED: &[u8] = br#"{"e":"ORDER_TRADE_UPDATE","E":1790000000125,"T":1790000000125,"o":{"s":"BTC-261225-90000-P","c":"mv6512ab0f2000000000000006600000","S":"SELL","o":"LIMIT","f":"IOC","q":"0.01","p":"25100.000","ap":"0.000","sp":"0","x":"EXPIRED","X":"EXPIRED","i":8002,"l":"0.00","z":"0.00","L":"0.000","N":"USDT","n":"0","T":1790000000125,"t":0,"b":"0","a":"0","m":false,"R":false,"ps":"BOTH","rp":"0","er":"0","V":"EXPIRE_MAKER"}}"#;
    const ACCOUNT: &[u8] = br#"{"e":"ACCOUNT_UPDATE","E":1790000000126,"B":[{"b":"2000.000000","m":"2000.000000","u":"0","U":0,"M":"10.000000","i":"0","a":"USDT"}],"P":[{"s":"BTC-261225-85000-C","c":"0.01","r":"0.01","p":"500.000","a":"0"}],"uid":1}"#;
    const RISK: &[u8] = br#"{"e":"RISK_LEVEL_CHANGE","E":1790000000127,"s":"NORMAL","mb":"1534.11","mm":"254.78"}"#;
    const MARGIN: &[u8] = br#"{"asset":[{"asset":"USDT","marginBalance":"2000.000000","equity":"2000.000000","available":"2000.000000","initialMargin":"0","maintMargin":"10.000000","unrealizedPNL":"0","adjustedEquity":"2000.000000"}],"time":1790000000000,"canTrade":true,"canDeposit":true,"canWithdraw":true,"reduceOnly":false,"tradeGroupId":-1}"#;
    const POSITIONS: &[u8] = br#"[{"entryPrice":"500.000","symbol":"BTC-261225-85000-C","side":"LONG","quantity":"0.01","reducibleQty":"0.01","markValue":"5","ror":"0","unrealizedPNL":"0","markPrice":"500.000","strikePrice":"85000","positionCost":"5","expiryDate":1798185600000,"priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT"},{"entryPrice":"25100.000","symbol":"BTC-261225-90000-P","side":"SHORT","quantity":"-0.01","reducibleQty":"0.01","markValue":"-251","ror":"0","unrealizedPNL":"0","markPrice":"25100.000","strikePrice":"90000","positionCost":"251","expiryDate":1798185600000,"priceScale":3,"quantityScale":4,"optionSide":"PUT","quoteAsset":"USDT"}]"#;
    const OPEN: &[u8] = br#"[{"orderId":7101,"symbol":"BTC-261225-85000-C","price":"400","quantity":"0.01","executedQty":"0","fee":"0","side":"BUY","type":"LIMIT","timeInForce":"GTC","reduceOnly":false,"postOnly":false,"createTime":1790000000000,"updateTime":1790000000000,"status":"ACCEPTED","avgPrice":"0","clientOrderId":"web_stranger_9","priceScale":3,"quantityScale":4,"optionSide":"CALL","quoteAsset":"USDT","mmp":false}]"#;
    const TRADES: &[u8] = br#"[{"id":4611875134427365377,"tradeId":239,"orderId":4611875134427365376,"symbol":"BTC-261225-85000-C","price":"500","quantity":"0.01","fee":"-0.0015","realizedProfit":"0","side":"BUY","type":"LIMIT","volatility":"0.5","liquidity":"TAKER","quoteAsset":"USDT","time":1790000000001,"priceScale":3,"quantityScale":4,"optionSide":"CALL"}]"#;
    const EXERCISE: &[u8] = br#"[{"id":"1125899906842624000","currency":"USDT","symbol":"BTC-261225-85000-C","exercisePrice":"86000.000","markPrice":"86000.000","quantity":"0.01","amount":"10.000","fee":"0.015","createDate":1790000000000,"priceScale":3,"quantityScale":4,"optionSide":"CALL","positionSide":"LONG","quoteAsset":"USDT"}]"#;
    const INDEX: &[u8] = br#"{"time":1790000000000,"indexPrice":"65000.000"}"#;

    // Boot-time construction — outside the window.
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 5_000);
    let signer = Signer::Hmac(HmacSigner::from_secret(exec_binance::testvenue::OPT_HMAC_SECRET).expect("gate 111 secret"));
    let mut d = ingress_binance::eapi::EapiDiscovery::new();
    d.ingest_exchange_info(exec_binance::testvenue::EAPI).expect("gate 111 discovery");
    let (mut table, mut wire) = InstTable::new(7);
    let mut rows = [0u16; 2];
    for (k, sym) in [&b"BTC-261225-85000-C"[..], &b"BTC-261225-90000-P"[..]].into_iter().enumerate() {
        let row = d.rows().iter().find(|r| r.symbol() == sym).expect("gate 111 row");
        rows[k] = table
            .bind_option(&mut wire, &OptBindSpec { sym: core_types::make_symbol_id(VenueId::Binance, 1_025 + k as u32), row, owned: true, writable: true })
            .expect("gate 111 bind");
    }
    let (call, put) = (rows[0], rows[1]);
    let w = *wire.row(call);
    let o_call = *wire.opt(call);
    let mut win = [0u8; 2_048];
    let mut tids = TidRing::<256>::new();
    let (j_p, mut j_c) = core_ring::Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
    let mut journal = JournalTx::new(j_p);
    let mut pos = [PosRow::default(); 8];
    let mut orders = [OrderRow::default(); 8];
    let mut trades = [TradeRow::default(); 8];
    let mut ex = [ExerciseRow::default(); 8];
    let mut legs = [Leg::default(); 2];
    legs[0].owned = true;
    legs[1].owned = true;
    let mut a = WsAnswer::default();
    let mut ev = UserEvent::default();
    // The router's ledger, an option row bound (Q-2).
    let (t0, sym_call) = (1_000_000_000u64, core_types::make_symbol_id(VenueId::Binance, 1_025));
    let mut ledger = Box::new(Ledger::new(core_time::WallAnchor::new(t0, t0)));
    ledger.mark_seeded();
    ledger.bind_instrument(&InstrumentSpec::new(sym_call, LAW_OPTION, INST_WRITABLE | INST_CALL, 1_000_000, 85_000_000_000)).expect("gate 111 ledger row");

    let g = AllocGuard::new();
    let (mut rendered, mut fees, mut others, mut risks, mut settled, mut probes, mut stale) = (0usize, 0u64, 0u64, 0u64, 0i64, 0u64, 0u64);
    let mut i = 0u64;
    while i < N {
        let now = core_time::now_ns();
        let ts = 1_790_000_000_000 + i;
        let cid = Part::Cid(&prefix, 2, 0x65 + i);
        // The route's requests, HMAC-signed in place.
        let o = Place {
            symbol: w.symbol(),
            side: (i & 1) as u8,
            maker: false,
            reduce_only: i & 2 != 0,
            qty_1e6: 10_000,
            qty_dec: w.qty_dec,
            px_1e6: 500_000_000 + (i % 100) as i64 * 5_000_000,
            px_dec: w.px_dec,
            cid,
            ts_ms: ts,
        };
        let mut q = QueryWriter::new(&mut win);
        render_opt_place(&mut q, &st, &o);
        rendered += q.sign_with(&signer).expect("gate 111 place");
        let mut q = QueryWriter::new(&mut win);
        render_opt_by_cid(&mut q, &st, w.symbol(), cid, ts);
        rendered += q.sign_with(&signer).expect("gate 111 query");
        // Their answers.
        scan_rest_answer(200, PLACED, &mut a).expect("gate 111 ack");
        assert_eq!((a.venue_oid, a.order_status), (8_001, S_NEW), "gate 111: the ACK names the order");
        std::hint::black_box(prefix.classify(a.cid.get(PLACED)));
        scan_rest_answer(200, STATUS, &mut a).expect("gate 111 status");
        assert_eq!((a.executed_1e6, a.qty_1e6), (10_000, 10_000), "gate 111: eapi's `quantity`");
        // The stream's frames, read in place.
        for f in [NEW, TRADE, EXPIRED, ACCOUNT, RISK] {
            scan_user_event(f, &mut ev).expect("gate 111 event");
            match ev.kind {
                UE_ORDER => {}
                UE_ACCOUNT => {
                    others += 1;
                    continue;
                }
                UE_RISK => {
                    risks += u64::from(ev.risk == RISK_NORMAL);
                    continue;
                }
                _ => unreachable!("gate 111: no other event"),
            }
            std::hint::black_box(prefix.classify(ev.cid.get(f)));
            let row = wire.find(PRODUCT_OPTIONS, ev.symbol.get(f));
            assert!(row == call || row == put, "gate 111: the event's row, by product");
            match ev.exec {
                X_NEW | X_EXPIRED => {}
                X_TRADE => {
                    let tid = ev.trade_id + 10 * (i % 512);
                    let seen = tids.see(row, tid, true);
                    assert!(!seen.again && seen.fee_due, "gate 111: a fresh trade, its commission owed");
                    // The gateway's normalization: a charge whatever its sign.
                    let commission = ev.commission_1e9.saturating_abs();
                    assert_eq!(commission, 1_500_000, "gate 111: 0.0015 USDT ×1e9");
                    let wr = wire.row(row);
                    let asset = fee_asset(wr, ev.fee_asset.get(f));
                    assert_eq!(asset, FEE_ASSET_SETTLE, "gate 111: USDT, an option's settle asset");
                    let fee = core_types::ExecRecord {
                        ts_ns: now,
                        wall_ms: ts,
                        client_oid: 0x65 + i,
                        venue_oid: ev.venue_oid,
                        a: ev.last_px_1e6,
                        b: fee_notional_1e9(wr, ev.last_px_1e6, ev.last_qty_1e6),
                        c: commission,
                        code: fee_code(ev.maker != 0, asset, TRADE_BY_MEMBER, wr),
                        row,
                        kind: J_FEE,
                        slot: 2,
                    };
                    assert!(journal.note(&fee), "gate 111: the journal's ring has room");
                    fees += 1;
                }
                _ => unreachable!("gate 111: no other execution"),
            }
        }
        while let Some(x) = j_c.try_pop_ref() {
            std::hint::black_box((x.b, x.c));
        }
        // The router's ledger: a fresh, sane mark; a long's and a short's
        // probe; the staleness sweep.
        let mark = 500_000_000 + (i % 7) as i64 * 5_000_000;
        let s = core_types::OptSummary::new(t0 + i, VenueId::Binance, sym_call, core_types::OPT_SUMMARY_FLAG_MARK_PX, mark * 1_000, 0, 65_000_000_000_000, 0, 0, 0, 0, 0);
        ledger.on_opt_summary(&s);
        for buy in [true, false] {
            let p = ledger.probe_instrument(sym_call, 2, 10_000, buy, mark, None).expect("gate 111 probe");
            probes += u64::from(p.refusal == InstRefusal::None);
        }
        ledger.expire_marks(t0 + i);
        // Now and then the feed falls silent past the stale age: opening
        // refused either way until the next summary.
        if i % 64 == 32 {
            ledger.expire_marks(t0 + i + MARK_STALE_NS + 1);
            for buy in [true, false] {
                let p = ledger.probe_instrument(sym_call, 2, 10_000, buy, mark, None).expect("gate 111 stale probe");
                stale += u64::from(p.refusal == InstRefusal::Unpriced);
            }
        }
        // The cycle's reads, at their own cadence.
        if i % 64 == 0 {
            let mut q = QueryWriter::new(&mut win);
            render_account(&mut q, &st, ts);
            rendered += q.sign_with(&signer).expect("gate 111 account");
            scan_rest_answer(200, MARGIN, &mut a).expect("gate 111 margin answer");
            let m = scan_opt_margin(MARGIN, a.result).expect("gate 111 margin");
            assert!(m.can_trade && !m.reduce_only && um_ratio_1e6(m.maint_1e6, m.adj_equity_1e6) == 5_000, "gate 111: the ratio, 0.5 %");
            let n = scan_opt_positions(POSITIONS, &mut pos).expect("gate 111 positions");
            assert_eq!((n, pos[1].amt_1e6), (2, -10_000), "gate 111: the short side-signed");
            legs[0].venue_1e6 = pos[0].amt_1e6;
            legs[0].booked_1e6 = 10_000;
            legs[1].venue_1e6 = 0;
            legs[1].booked_1e6 = -10_000;
            legs[1].settling = true;
            let mut v = Verdict::default();
            judge_positions(&mut legs, &mut v);
            assert!(v.reconciled(), "gate 111: one leg matched, one settling");
            let n = scan_orders(OPEN, &mut orders).expect("gate 111 open orders");
            std::hint::black_box(prefix.classify(orders[n - 1].cid.get(OPEN)));
            let n = scan_trades(TRADES, &mut trades).expect("gate 111 trades");
            assert_eq!(day_increasing_1e6(&mut trades[..n], 10_000, false, 1_000_000), 5_000_000, "gate 111: the day's 5 USD");
            let n = scan_exercise(EXERCISE, &mut ex).expect("gate 111 exercise");
            let px = settle_px_1e6(&o_call, w.unit_1e6, ex[n - 1].exercise_px_1e6);
            assert!(settlement_agrees(&ex[n - 1], 10_000, px), "gate 111: the record agrees with the booked long");
            settled += px;
            assert_eq!(scan_opt_index(INDEX), Ok(65_000_000_000), "gate 111: the index");
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();

    assert!(rendered > 0, "the gate rendered nothing");
    assert_eq!((fees, others, risks, probes), (N, N, N, 2 * N), "gate 111: a fee a trade; the account and the risk counted; every probe priced");
    assert_eq!(settled, N.div_ceil(64) as i64 * 1_000_000_000, "gate 111: 1 000 a contract at 86 000 over 85 000");
    assert_eq!(stale, 2 * ((N + 31) / 64), "gate 111: every probe on the stale row refused Unpriced");
    std::hint::black_box((&table, &ledger));
    assert_eq!(allocs, 0, "gate 111: options' render and scans allocated {allocs} times ({bytes} B)");
}

/// Gate 112's round: a BUY and the SELL of it over the equity lane — each
/// one `POST /sapi/v1/equity/order/place` sealed on the route and its
/// answer opened; the stream's two reports (`accepted`, `filled`), each its
/// own record (the venue writes a frame a write); and the order's detail
/// (`GET …/order/detail` by its UUID on the jobs connection) sealed and
/// its answer opened.
const G112_ROUND: u64 = 12;
/// Gate 112's sweep (the halt's): the resting order's cancel by its UUID
/// and its answer on the route, the stream's `canceled`, the order's detail
/// and its answer; then the confirmation — the reconciliation cycle: the
/// session's executions (one page of `trade/history`, nine executions) and
/// `open-orders`, each on the jobs connection. Each request sealed, each
/// answer and frame opened.
const G112_RECON: u64 = 9;
/// Gate 112's TTL round: a resting DAY order the gateway's TTL cancels —
/// the place and its answer, the stream's `accepted`, the cancel by the
/// UUID and its answer, the stream's `canceled`, the order's detail and
/// its answer: each request sealed, each answer and frame opened.
const G112_TTL: u64 = 8;

/// What the engine end saw (gate 112).
#[derive(Clone, Copy, Default)]
struct Seen112 {
    acks: u64,
    filled: u64,
    retired_other: u64,
    rejects: u64,
    locks: u64,
    scan_fails: u64,
    positions: u64,
    fills: u64,
    bought: i64,
    sold: i64,
    j_fill: u64,
    j_ack: u64,
    j_accepted: u64,
    j_fee: u64,
    /// `J_FEE`s on an equity row in its settle asset (USD), a charge at
    /// the $0.35 floor, flagged [`exec_binance::journal::FEE_FLAG_EQ_FLOOR`].
    j_fee_floor: u64,
    pulses: u64,
    sweeps_done: u64,
    recons: u64,
    reconciled: u64,
    /// Orders retired by the gateway's TTL, and by a member's cancel.
    ttl: u64,
    member: u64,
}

/// **BX10 gate 112 — the equity lane's whole step, over a real rustls
/// loopback, allocates only rustls' record buffers** (gate 110's harness,
/// Binance Stocks' lane).
///
/// The REAL gateway (`BnGateway::step`, booted as the engine boots it)
/// EQUITY-ONLY — AAPL of the scripted venue's `exchangeInfo`
/// (`exec_binance::testvenue::EQ_INFO`) bound and owned, the D5 keys `RTH`,
/// no tokenize, USDC — against the venue in gate 94's CHILD process
/// (`GATE94_EQ=1`: a place at or above 300 USD fills whole at its limit as
/// it is placed; one under it rests).
/// The arm key Ed25519; every SAPI request signed in place. Every round: a
/// BUY of 0.03 AAPL at 336.10 (LIMIT DAY under the gateway's TTL), then —
/// once it ended — the SELL of it: the booked position long, then flat (an
/// `EVT_POSITION` each way). Each order is ONE `POST …/order/place` on the
/// lane's REST route and its answer (`J_ACK`); the stream's `accepted` and
/// `filled` reports, each its own record — the report the TRIGGER: the
/// order's detail read once by its UUID on the jobs connection (whichever
/// came first, the report or the answer: one read) — its execution booked
/// once (lane 4, `J_FILL`), the broker's `ACCEPTED` journaled once
/// (`J_ACCEPTED`), its fee — the $0.35 floor on $10.08 — journaled once,
/// flagged (`J_FEE`), the order retired FILLED. Pinned like gates 94b, 108
/// and 110: rustls 0.23's buffered API allocates one `Vec` per record it
/// seals and one per application-data record it opens — [`G112_ROUND`] a
/// round. Then **TTL rounds** (the alloc review: Stocks has no IoC — a DAY
/// limit that does not cross ends only so): a BUY under the venue's
/// filling price rests, its 20 ms TTL falls due, the gateway cancels it by
/// its UUID, the venue's `S` arms the cancel's re-check, the stream's
/// `canceled` asks the detail, the order retired `CANCELED_TTL` (its
/// re-check disarmed): [`G112_TTL`] a round. Measured alone, BEFORE the
/// rounds, **the halt's sweep** with one order of ours resting: cancelled
/// by its UUID (`CANCELED_MEMBER`), then the confirmation — a
/// reconciliation cycle over the session's executions (four rounds and one
/// more BUY: nine, one page) and the open orders: [`G112_RECON`]; the
/// sweep reported done, the cycle reconciled — AAPL held long
/// through it, every execution since the boot's first cycle matching what
/// the arm booked — then the held lot sold. Before the
/// rounds because the cycle reads EVERY execution since the boot: a page
/// over rustls' 4 KiB read buffer (a full one is 23 KiB) grows it in steps
/// that depend on how the bytes arrive, then shrinks it — rustls'
/// allocations, not pinnable to an exact count (a full page's scan is
/// gate 113's, at 0 B).
///
/// NOT pinned here (each allocation-free by inspection, built of what the
/// gates pin): the boot (the clock, the key's permissions and the account
/// mode on SAPI, the listenKey, the day a ticker at a time, the first
/// cycle and its stray judgement); the listenKey's keep-alive (periodic,
/// the generic job render); the in-doubt detail by our id (rendered,
/// signed and its answer scanned in gate 113; on a socket only after a
/// lost answer); the account's `cancel-all` (a sweep's second round on a
/// dedicated account); the gateway's refusals — a place answered `F`, a
/// coded non-200, an unreadable one, `486218` — and a cancel answered
/// `F`, unreadably or naming another UUID; a cancel's re-check firing on
/// an order still working; the `CANCELED_VENUE`, `EXPIRED` and `REJECTED`
/// ends; a stray or foreign row in a listing; the over-fill, page and
/// total guards; an early report's kept `ACCEPTED` (taken only when the
/// stream beats the place's answer); bursts over the 4 KiB window. A break-and-watch (an 8-byte
/// `Vec` in the gateway's detail reading and one in its page reading)
/// failed the rounds at 4 200 and the cycle at 5.
#[test]
fn binance_equity_lane_round_trip_allocates_only_rustls_record_buffers() {
    use clob_dispatcher::{RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_FILLED};
    use core_config::SecretKeyBytes;
    use core_fill::ORDER_KIND_IOC;
    use core_ring::{Consumer, Ring};
    use core_types::{ExecRecord, Fill, Side};
    use exec_binance::arm::{
        GW_FILLS_BOOKED, GW_FILLS_DEFERRED, GW_FILLS_FOREIGN, GW_FILLS_LOST, GW_FILLS_UNOWNED, GW_FILLS_UNRESOLVED, GW_IN_DOUBT_RAISED,
        GW_JOBS_DROPPED, GW_JOURNAL_DROPPED, GW_RECON_DRIFT_LEGS, GW_RECON_FAILED, GW_RECON_OK, GW_RECON_UNSEEN_LEGS, GW_TALLIES,
    };
    use exec_binance::cmd::{
        duplex, BnCmd, BnEngineEnd, EVT_ACK, EVT_F_RECONCILED, EVT_F_SWEEP_DONE, EVT_LOCK, EVT_POSITION, EVT_RECON, EVT_REJECT, EVT_RETIRED,
        EVT_SCAN_FAIL, EVT_STATUS, EVT_SWEEP,
    };
    use exec_binance::config::{BnConfig, Scope};
    use exec_binance::equity::{scan_eq_symbol, EqKnobs, EqSession};
    use exec_binance::gateway::{BnGateway, GwKnobs, Phase};
    use exec_binance::inst::{EqBindSpec, InstTable, PRODUCT_EQUITY};
    use exec_binance::journal::{
        fee_fields, JournalTx, EQ_FEE_MIN_USD_1E6, FEE_ASSET_SETTLE, FEE_FLAG_EQ_FLOOR, JOURNAL_RING, J_ACCEPTED, J_ACK, J_FEE, J_FILL,
    };
    use exec_binance::mode::AccountMode;
    use exec_binance::testvenue::{EPOCH, EQ_INFO, KEY, SEED, SLOT};
    use exec_gateway::stats::SWEEP_CLEAR;
    use std::time::{Duration, Instant};

    const FILL_N: usize = 1_024;
    const ROUNDS: u64 = 300;
    const WARM: u64 = 50;
    /// The rounds before the cycle: with the held BUY, nine executions —
    /// one page under rustls' 4 KiB read buffer.
    const RECON_WARM: u64 = 4;
    const LONG_MS: u64 = 3_600_000;
    const PX: i64 = 336_100_000;
    /// Under the scripted venue's filling price (300 USD): it rests.
    const REST_PX: i64 = 200_000_000;
    const TTL_ROUNDS: u64 = 50;
    const QTY: i64 = 30_000;
    const CLOCK_CLEAR: Duration = Duration::from_secs(50);

    type Gw = BnGateway<FILL_N>;

    fn pump(gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen112) {
        gw.step();
        while let Some(g) = eng.evt.pop() {
            let (kind, why, flags) = (g.kind, g.why, g.flags);
            drop(g);
            match kind {
                EVT_ACK => s.acks += 1,
                EVT_RETIRED if why == RETIRED_FILLED => s.filled += 1,
                EVT_RETIRED if why == RETIRED_CANCELED_TTL => s.ttl += 1,
                EVT_RETIRED if why == RETIRED_CANCELED_MEMBER => s.member += 1,
                EVT_RETIRED => s.retired_other += 1,
                EVT_REJECT => s.rejects += 1,
                EVT_LOCK => s.locks += 1,
                EVT_SCAN_FAIL => s.scan_fails += 1,
                EVT_POSITION => s.positions += 1,
                EVT_STATUS => s.pulses += 1,
                EVT_SWEEP => s.sweeps_done += u64::from(flags & EVT_F_SWEEP_DONE != 0),
                EVT_RECON => {
                    s.recons += 1;
                    s.reconciled += u64::from(flags & EVT_F_RECONCILED != 0);
                }
                _ => {}
            }
        }
        while let Some(g) = lane.try_pop_ref() {
            if matches!(g.side, Side::Bid) {
                s.bought += g.qty.0;
            } else {
                s.sold += g.qty.0;
            }
            drop(g);
            s.fills += 1;
        }
        while let Some(g) = journal.try_pop_ref() {
            s.j_fill += u64::from(g.kind == J_FILL);
            s.j_ack += u64::from(g.kind == J_ACK);
            s.j_accepted += u64::from(g.kind == J_ACCEPTED);
            if g.kind == J_FEE {
                let (_, asset, _, product, _, _) = fee_fields(g.code);
                s.j_fee += 1;
                s.j_fee_floor += u64::from(
                    asset == FEE_ASSET_SETTLE && product == PRODUCT_EQUITY && g.code & FEE_FLAG_EQ_FLOOR != 0 && g.c == EQ_FEE_MIN_USD_1E6 * 1_000,
                );
            }
            drop(g);
        }
    }

    fn tallies(gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen112) -> [u64; GW_TALLIES] {
        let (p0, t) = (s.pulses, Instant::now());
        while s.pulses == p0 {
            assert!(t.elapsed() < Duration::from_secs(2), "gate 112: no pulse");
            pump(gw, eng, lane, journal, s);
        }
        let mut w = [0u64; GW_TALLIES];
        let mut i = 0;
        while i < GW_TALLIES {
            w[i] = eng.stats.published(i);
            i += 1;
        }
        w
    }

    // ---- the venue, in its own process (gate 94's helper, equity's knob) ----
    let dir = std::env::temp_dir().join(format!("mv-gate112-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 112 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 112 exe"))
        .args(["gate94_venue_helper", "--exact", "--ignored", "--test-threads=1"])
        .env("GATE94_VENUE_DIR", &dir)
        .env("GATE94_REST", "0")
        .env("GATE94_EQ", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("gate 112 child");
    let deadline = Instant::now() + Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 112 port");
        }
        assert!(Instant::now() < deadline, "gate 112: the venue never came up");
        std::thread::sleep(Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 112 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der)).expect("gate 112 anchor");
    let tls = std::sync::Arc::new(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());

    // ---- the gateway, booted equity-only as the engine boots it ----
    let seed = SecretKeyBytes::new_locked(SEED).expect("gate 112 seed");
    let cfg = BnConfig::loopback(Scope::Live, String::from(KEY), seed, port).expect("gate 112 config");
    let (mut table, mut wire) = InstTable::new(core_config::universe::LEGACY_BN_ANCHOR_SYM);
    let aapl = scan_eq_symbol(EQ_INFO, b"AAPL").expect("gate 112 rules");
    let sym = core_types::make_symbol_id(VenueId::Binance, 4_097);
    table.bind_equity(&mut wire, &EqBindSpec { sym, row: &aapl, owned: true, bstock: b"" }).expect("gate 112 bind");
    let (mut eng, gw_end) = duplex();
    let (fill_tx, mut lane) = Ring::<Fill, FILL_N>::new().split();
    let (j_tx, mut journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
    let knobs = GwKnobs {
        epoch: EPOCH,
        owner_slot: SLOT,
        shared: false,
        mode: AccountMode::Classic,
        recv_window_ms: 5_000,
        stp_mode: String::from("EXPIRE_MAKER"),
        countdown_ms: 2 * LONG_MS,
        heartbeat_ms: LONG_MS,
        recon_every_ms: LONG_MS,
        spin: false,
        rotate_after_ns: LONG_MS * 1_000_000,
        rest_orders: false,
        pnl_anchor_1e6: 0,
        boot_timeout_ns: 20_000_000_000,
        quiet_ns: 60_000_000_000,
        anchor_unsaved: None,
        max_leverage: 20,
        adopt_cap_usd_1e6: 0,
        equity: EqKnobs::new(EqSession::Rth, false, "USDC"),
    };
    let mut gw = Gw::new(&cfg, tls, knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx)).expect("gate 112 gateway");
    gw.boot().expect("gate 112 boot");
    assert_eq!(gw.phase(), Phase::Ready);
    let ready_at = Instant::now();
    let mut s = Seen112::default();
    let rest_jobs = |eng: &BnEngineEnd| eng.stats.rest_rtt.count();

    // Until no REST answer for 300 ms: whatever the boot left queued.
    let t = Instant::now();
    let (mut seen, mut quiet) = (rest_jobs(&eng), Instant::now());
    while quiet.elapsed() < Duration::from_millis(300) {
        assert!(t.elapsed() < Duration::from_secs(10), "gate 112: the boot's REST never settled");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
        if rest_jobs(&eng) != seen {
            (seen, quiet) = (rest_jobs(&eng), Instant::now());
        }
    }

    // One order: pushed under its TTL, then pumped until it ended — its
    // receipt, its execution booked on lane 4, its fee journaled, retired.
    // The fills' ids, and (from 2 000 000) the resting orders'.
    let mut oid = 2_000_000u64;
    let mut next = 1_000_000u64;
    let mut order = |gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen112, side: Side| {
        next += 1;
        let (a0, e0, l0, fee0) = (s.acks, s.filled, s.fills, s.j_fee);
        let now = core_time::now_ns();
        assert!(eng.cmd.push(&BnCmd::place(next, PX, QTY, now + LONG_MS * 1_000_000, 0, ORDER_KIND_IOC, side, SLOT, 0).at(now)));
        let t = Instant::now();
        while !(s.acks > a0 && s.filled > e0 && s.fills > l0 && s.j_fee > fee0) {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 112: an order stalled");
            pump(gw, eng, lane, journal, s);
        }
    };
    // ---- a reconciliation cycle, measured alone: the sweep's confirmation ----
    // Four rounds and one more BUY, held through it: an open leg.
    let mut w = 0u64;
    while w < RECON_WARM {
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Bid);
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Ask);
        w += 1;
    }
    order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Bid);
    // One order of ours resting as the sweep begins: the sweep cancels it
    // by its UUID; the confirmation cycle runs once it has ended (the
    // listing then empty — a non-empty listing's found row is gate 113's
    // scan).
    oid += 1;
    let (a0, acc0) = (s.acks, s.j_accepted);
    let now = core_time::now_ns();
    assert!(eng.cmd.push(&BnCmd::place(oid, REST_PX, QTY, now + LONG_MS * 1_000_000, 0, ORDER_KIND_IOC, Side::Bid, SLOT, 0).at(now)));
    let t = Instant::now();
    while !(s.acks > a0 && s.j_accepted > acc0) {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 112: the resting order was never accepted");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    let (before, jobs2) = (s, rest_jobs(&eng));
    let g = AllocGuard::new();
    let swept = eng.stats.request_sweep();
    let t = Instant::now();
    while !(eng.stats.sweep() == (swept, SWEEP_CLEAR) && s.sweeps_done > before.sweeps_done && s.recons > before.recons && s.member > before.member) {
        assert!(t.elapsed() < Duration::from_secs(5), "gate 112: the sweep's cycle never ran");
        pump(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    }
    let (recon_allocs, recon_bytes, _) = g.delta();
    let (recon, jobs3) = (s, rest_jobs(&eng));
    let verdict = gw.last_verdict();
    // The held lot sold: flat again.
    order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Ask);
    let mut w = 0u64;
    while w < WARM {
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Bid);
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Ask);
        w += 1;
    }

    // ---- the rounds ----
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 112: the run neared the clock's 60 s round");
    let t_warm = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    let (warm, jobs0) = (s, rest_jobs(&eng));
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < ROUNDS {
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Bid);
        order(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, Side::Ask);
        r += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let (rounds, jobs1) = (s, rest_jobs(&eng));

    // ---- the TTL rounds: a resting order the gateway's TTL cancels ----
    let ttl_round = |gw: &mut Gw, eng: &mut BnEngineEnd, lane: &mut Consumer<Fill, FILL_N>, journal: &mut Consumer<ExecRecord, JOURNAL_RING>, s: &mut Seen112, oid: u64| {
        let t0 = s.ttl;
        let now = core_time::now_ns();
        assert!(eng.cmd.push(&BnCmd::place(oid, REST_PX, QTY, now + 20_000_000, 0, ORDER_KIND_IOC, Side::Bid, SLOT, 0).at(now)));
        let t = Instant::now();
        while s.ttl == t0 {
            assert!(t.elapsed() < Duration::from_secs(5), "gate 112: a TTL round stalled");
            pump(gw, eng, lane, journal, s);
        }
    };
    let mut w = 0u64;
    while w < 5 {
        oid += 1;
        ttl_round(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, oid);
        w += 1;
    }
    let (ttl_warm, jobs4) = (s, rest_jobs(&eng));
    let g = AllocGuard::new();
    let mut r = 0u64;
    while r < TTL_ROUNDS {
        oid += 1;
        ttl_round(&mut gw, &mut eng, &mut lane, &mut journal, &mut s, oid);
        r += 1;
    }
    let (ttl_allocs, ttl_bytes, _) = g.delta();
    let (ttls, jobs5) = (s, rest_jobs(&eng));
    let t_rounds = tallies(&mut gw, &mut eng, &mut lane, &mut journal, &mut s);
    assert!(ready_at.elapsed() < CLOCK_CLEAR, "gate 112: the run neared the clock's 60 s round");
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    std::hint::black_box(&table);

    let both = 2 * ROUNDS;
    assert_eq!(
        (rounds.filled - warm.filled, rounds.fills - warm.fills, rounds.j_fill - warm.j_fill),
        (both, both, both),
        "gate 112: every order filled whole, booked once on lane 4 and journaled once"
    );
    assert_eq!(
        (rounds.acks - warm.acks, rounds.j_ack - warm.j_ack, rounds.j_accepted - warm.j_accepted),
        (both, both, both),
        "gate 112: a receipt an order (`EVT_ACK`, `J_ACK`); the broker's ACCEPTED journaled once an order"
    );
    assert_eq!(
        (rounds.j_fee - warm.j_fee, rounds.j_fee_floor - warm.j_fee_floor),
        (both, both),
        "gate 112: a fee an order — USD, the $0.35 floor, flagged"
    );
    assert_eq!(
        (rounds.bought - warm.bought, rounds.sold - warm.sold, rounds.positions - warm.positions),
        (ROUNDS as i64 * QTY, ROUNDS as i64 * QTY, both),
        "gate 112: every round bought 0.03 and sold it back — the position long, then flat"
    );
    assert_eq!(
        (recon.sweeps_done - before.sweeps_done, recon.recons - before.recons, recon.reconciled - before.reconciled, recon.member - before.member),
        (1, 1, 1, 1),
        "gate 112: the sweep done — the resting order cancelled by its UUID — one cycle judged and reconciled"
    );
    assert_eq!(
        (ttls.ttl - ttl_warm.ttl, ttls.acks - ttl_warm.acks, ttls.j_accepted - ttl_warm.j_accepted, ttls.fills - ttl_warm.fills),
        (TTL_ROUNDS, TTL_ROUNDS, TTL_ROUNDS, 0),
        "gate 112: every TTL round's order receipted, ACCEPTED once, cancelled by its TTL, nothing filled"
    );
    assert_eq!(before.bought - before.sold, QTY, "gate 112: AAPL held long through the cycle");
    assert_eq!(
        (verdict.drift_legs, verdict.unseen_legs, verdict.ghosts, verdict.orphans, verdict.foreign),
        (0, 0, 0, 0, 0),
        "gate 112: the verdict"
    );
    assert_eq!(
        (recon.rejects, recon.retired_other, recon.scan_fails, recon.locks),
        (0, 0, 0, 0),
        "gate 112: nothing refused, nothing else retired, no frame failed its scan, no lock"
    );
    assert_eq!((ttls.rejects, ttls.retired_other, ttls.scan_fails, ttls.locks), (0, 0, 0, 0), "gate 112: the TTL rounds — nothing else");
    assert_eq!(
        (t_rounds[GW_FILLS_BOOKED] - t_warm[GW_FILLS_BOOKED], t_rounds[GW_RECON_OK]),
        (both, 2),
        "gate 112: a fill an order; the boot's cycle and the sweep's reconciled"
    );
    assert_eq!(
        (t_rounds[GW_RECON_FAILED], t_rounds[GW_RECON_DRIFT_LEGS], t_rounds[GW_RECON_UNSEEN_LEGS]),
        (0, 0, 0),
        "gate 112: no cycle failed, no drift leg, no unseen leg"
    );
    let zero = [
        GW_FILLS_FOREIGN,
        GW_FILLS_UNOWNED,
        GW_FILLS_DEFERRED,
        GW_FILLS_UNRESOLVED,
        GW_FILLS_LOST,
        GW_JOBS_DROPPED,
        GW_IN_DOUBT_RAISED,
        GW_JOURNAL_DROPPED,
    ]
    .map(|k| t_rounds[k]);
    assert_eq!(zero, [0; 8], "gate 112: no foreign, unowned, deferred, unresolved or lost fill; no job, doubt or record dropped");
    assert_eq!(
        (jobs1 - jobs0, jobs3 - jobs2, jobs5 - jobs4),
        (both, 3, TTL_ROUNDS),
        "gate 112: a detail an order in the rounds; the sweep's detail, the cycle's page and its listing; a detail a TTL round"
    );
    assert_eq!(
        (allocs, recon_allocs, ttl_allocs),
        (G112_ROUND * ROUNDS, G112_RECON, G112_TTL * TTL_ROUNDS),
        "gate 112: the rounds, the sweep and the TTL rounds allocated {allocs} ({bytes} B), {recon_allocs} ({recon_bytes} B) and {ttl_allocs} ({ttl_bytes} B) — rustls' records only"
    );
}

/// **BX10 gate 113 — Binance Stocks' renders and scans are 0 B/op** (gate
/// 111's shape, the equity lane's frames; in-process, no socket). What the
/// gateway does per equity order and per equity cycle that gate 112 does
/// not already measure on a socket: `POST …/order/place` (LIMIT DAY, the D5
/// statics, our id), the cancel and the detail by the venue's UUID, the
/// in-doubt detail by our id and a page of `trade/history` — the cycle's
/// window, or a ticker's day — rendered and Ed25519-signed in place; the place's receipt read (our id classified,
/// the UUID held whole — `EqOrderId::of` — and keyed, `eq_key`), the
/// cancel's; the stream's `accepted` and `filled` reports read in place
/// (their JSON numbers exactly, the ticker found by product); the order's
/// detail with its executions — each seen once by the dedupe ring under
/// its execution id's key, booked no further than `filledQty` — its fee
/// classified (USD: the settle asset), flagged under $350 and noted into
/// the journal's ring; the venue's codes classified (`486207` row-fatal,
/// `486410` and `486449` venue codes). At their own cadence: the open
/// orders (no client id) and a FULL page of the session's executions (100
/// rows, 23 KiB — the page gate 112's cycle keeps under rustls' read
/// buffer), and the account's `cancel-all` answer. Zero allocations. A
/// break-and-watch (an 8-byte `Vec` in `render_eq_place` and one in
/// `scan_eq_trades`) failed it at 20 313 (20 000 places, 313 pages).
#[test]
fn binance_equity_lane_render_and_scan_steady_state() {
    use exec_binance::cid::{CidClass, CidPrefix};
    use exec_binance::config::Signer;
    use exec_binance::equity::{
        eq_key, eq_status_of, eq_terminal, exec_key, scan_eq_ack, scan_eq_cancel, scan_eq_cancel_all, scan_eq_detail, scan_eq_open, scan_eq_symbol,
        scan_eq_trades, scan_order_report, EqAck, EqDetail, EqExec, EqKnobs, EqOpenRow, EqOrderId, EqReport, EqSession, EQ_EV_ORDER, EQ_S_ACCEPTED,
        EQ_S_FILLED,
    };
    use exec_binance::gov::{classify, is_venue_code, CodeClass, EQ_DISCLAIMER, EQ_DUPLICATE_ID, EQ_NOT_TRADABLE};
    use exec_binance::inst::{EqBindSpec, InstTable, PRODUCT_EQUITY};
    use exec_binance::journal::{
        fee_asset, fee_code, JournalTx, EQ_FEE_FLOOR_NOTIONAL_1E6, FEE_ASSET_SETTLE, FEE_FLAG_EQ_FLOOR, JOURNAL_RING, J_FEE, TRADE_BY_MEMBER,
    };
    use exec_binance::oot::TidRing;
    use exec_binance::rest::{render_eq_by_id, render_eq_detail_cid, render_eq_place, render_eq_trades, QueryWriter, EQ_PAGE};
    use exec_binance::testvenue::EQ_INFO;
    use exec_binance::wsapi::{Part, Place, WsStatic};

    const N: u64 = 20000;
    const ID: &str = "8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f";
    const ACK: &[u8] = br#"{"status":"S","orderId":"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f","clientOrderId":"mv6512ab0f2000000000000006500000"}"#;
    const CANCEL: &[u8] = br#"{"orderId":"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f","status":"S"}"#;
    const CANCEL_ALL: &[u8] = br#"{"success":true}"#;
    const ACCEPTED: &[u8] = br#"{"e":"orderReport","E":1791700000000,"x":"ORDER_UPDATE","i":"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f","ai":1,"b":"EQ_AAPL","q":"USDC","S":"buy","o":"limit","p":336.1,"Q":0.03,"N":null,"fq":0,"FN":0,"tc":0,"Z":0,"n":0,"s":"accepted","T":1791700000000,"U":1791700000000}"#;
    const FILLED: &[u8] = br#"{"e":"orderReport","E":1791700000400,"x":"ORDER_TERMINAL","i":"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f","ai":1,"b":"EQ_AAPL","q":"USDC","S":"buy","o":"limit","p":336.1,"Q":0.03,"N":null,"fq":0.03,"FN":10.083,"tc":2,"Z":1.0083e1,"n":0.35,"s":"filled","T":1791700000000,"U":1791700000400}"#;
    const DETAIL: &[u8] = br#"{"orderId":"8b1f3a52-7c0e-4d55-9a8e-1f2b3c4d5e6f","clientOrderId":"mv6512ab0f2000000000000006500000","symbol":"AAPL","quote":"USDC","side":"BUY","orderType":"LIMIT","limitPrice":"336.10","avgFilledPrice":"336.10","qty":"0.03","notional":null,"filledQty":"0.030000000","filledTotal":"10.083","fee":"0.35","session":"RTH","status":"FILLED","createdAt":1791700000000,"updatedAt":1791700000400,"trades":[{"executionId":"x000002","executionAt":1791700000400,"price":"336.10","qty":"0.010000000"},{"executionId":"x000001","executionAt":1791700000300,"price":"336.10","qty":"0.020000000"}]}"#;
    const OPEN: &[u8] = br#"[{"orderId":"0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d","symbol":"AAPL","quote":"USDC","side":"BUY","orderType":"LIMIT","limitPrice":"300.00","qty":"0.03","filledQty":"0","status":"ACCEPTED","createdAt":1791700000000}]"#;

    // Boot-time construction — outside the window.
    let mut page = String::from(r#"{"page":1,"size":100,"total":241,"rows":["#);
    let mut k = 0;
    while k < EQ_PAGE {
        if k > 0 {
            page.push(',');
        }
        page.push_str(&format!(
            r#"{{"executionId":"x{k:06}","orderId":"{ID}","symbol":"AAPL","quote":"USDC","side":"{}","orderType":"LIMIT","price":"336.10","qty":"0.030000000","total":"10.083000","executionAt":{},"updatedAt":{}}}"#,
            if k % 2 == 0 { "BUY" } else { "SELL" },
            1_791_700_000_000u64 + k as u64,
            1_791_700_000_000u64 + k as u64
        ));
        k += 1;
    }
    page.push_str("]}");
    let page = page.into_bytes();
    assert!(page.len() > 20_000, "gate 113: a full page ({} B)", page.len());
    let prefix = CidPrefix::new(0x6512_ab0f);
    let st = WsStatic::new("EXPIRE_MAKER", 5_000);
    let signer = Signer::Ed25519(signer_ed25519::Ed25519Signer::from_seed(&[0x5a; 32]).expect("gate 113 signer"));
    let eq = EqKnobs::new(EqSession::Rth, false, "USDC").expect("gate 113 D5 keys");
    let (mut table, mut wire) = InstTable::new(7);
    let rules = scan_eq_symbol(EQ_INFO, b"AAPL").expect("gate 113 rules");
    let row = table
        .bind_equity(&mut wire, &EqBindSpec { sym: core_types::make_symbol_id(VenueId::Binance, 4_097), row: &rules, owned: true, bstock: b"" })
        .expect("gate 113 bind");
    let w = *wire.row(row);
    let mut win = [0u8; 2_048];
    let mut tids = TidRing::<256>::new();
    let (j_p, mut j_c) = core_ring::Ring::<core_types::ExecRecord, JOURNAL_RING>::new().split();
    let mut journal = JournalTx::new(j_p);
    let mut x = [EqExec::default(); 256];
    let mut rows = [EqExec::default(); EQ_PAGE];
    let mut open = [EqOpenRow::default(); 16];
    let (mut a, mut d, mut rep) = (EqAck::default(), EqDetail::default(), EqReport::default());

    let g = AllocGuard::new();
    let (mut rendered, mut fees, mut booked, mut pages, mut lists) = (0usize, 0u64, 0i64, 0u64, 0u64);
    let mut i = 0u64;
    while i < N {
        let now = core_time::now_ns();
        let ts = 1_791_700_000_000 + i;
        let cid = Part::Cid(&prefix, 2, 0x65 + i);
        // The route's requests and the jobs', Ed25519-signed in place.
        let o = Place {
            symbol: w.symbol(),
            side: (i & 1) as u8,
            maker: false,
            reduce_only: false,
            qty_1e6: 30_000,
            qty_dec: w.qty_dec,
            px_1e6: 336_100_000 + (i % 100) as i64 * 10_000,
            px_dec: w.px_dec,
            cid,
            ts_ms: ts,
        };
        let mut q = QueryWriter::new(&mut win);
        render_eq_place(&mut q, &st, &eq, &o);
        rendered += q.sign_with(&signer).expect("gate 113 place");
        let mut q = QueryWriter::new(&mut win);
        render_eq_by_id(&mut q, &st, ID.as_bytes(), ts);
        rendered += q.sign_with(&signer).expect("gate 113 by id");
        let mut q = QueryWriter::new(&mut win);
        render_eq_detail_cid(&mut q, &st, cid, ts);
        rendered += q.sign_with(&signer).expect("gate 113 by our id");
        let mut q = QueryWriter::new(&mut win);
        // The cycle's window, and (every other turn) a ticker's day.
        let day: &[u8] = if i & 1 == 0 { b"" } else { w.symbol() };
        render_eq_trades(&mut q, &st, day, ts - 3_600_000, ts + 60_000, 1 + i % 20, ts);
        rendered += q.sign_with(&signer).expect("gate 113 page");
        // The receipt: our id, the UUID held whole and keyed.
        scan_eq_ack(ACK, &mut a).expect("gate 113 receipt");
        assert!(a.ok && matches!(prefix.classify(a.cid.get(ACK)), CidClass::Ours { .. }), "gate 113: the receipt echoes our id");
        let id = EqOrderId::of(a.id.get(ACK)).expect("gate 113 UUID");
        let key = eq_key(id.get());
        // The stream's reports, read in place.
        for f in [ACCEPTED, FILLED] {
            scan_order_report(f, &mut rep).expect("gate 113 report");
            assert!(rep.kind == EQ_EV_ORDER && rep.id.get(f) == id.get(), "gate 113: the report names the order");
            assert_eq!(wire.find(PRODUCT_EQUITY, rep.ticker.get(f)), row, "gate 113: the ticker's row, by product");
            std::hint::black_box((rep.status == EQ_S_ACCEPTED, eq_terminal(rep.status), rep.filled_1e6, key));
        }
        assert_eq!((rep.filled_1e6, rep.filled_notional_1e6), (30_000, 10_083_000), "gate 113: the stream's numbers, exactly");
        // The detail: each execution once, no further than `filledQty`.
        let n = scan_eq_detail(DETAIL, &mut d, &mut x).expect("gate 113 detail");
        assert!(n == 2 && d.status == EQ_S_FILLED && eq_status_of(b"filled") == EQ_S_FILLED && d.id.get(DETAIL) == id.get());
        let mut filled = 0i64;
        let mut k = n;
        while k > 0 {
            k -= 1;
            let seen = tids.see(row, exec_key(x[k].exec.get(DETAIL)) ^ (i << 24), false);
            if !seen.again && filled + x[k].qty_1e6 <= d.filled_1e6 {
                filled += x[k].qty_1e6;
            }
        }
        booked += filled;
        // Its fee: USD, the settle asset; the floor under $350.
        let asset = fee_asset(&w, b"USD");
        assert_eq!(asset, FEE_ASSET_SETTLE, "gate 113: USD, an equity's settle asset");
        let mut code = fee_code(false, asset, TRADE_BY_MEMBER, &w);
        if d.filled_total_1e6 < EQ_FEE_FLOOR_NOTIONAL_1E6 {
            code |= FEE_FLAG_EQ_FLOOR;
        }
        let fee = core_types::ExecRecord {
            ts_ns: now,
            wall_ms: ts,
            client_oid: 0x65 + i,
            venue_oid: key,
            a: d.avg_1e6,
            b: d.filled_total_1e6 * 1_000,
            c: d.fee_1e9,
            code,
            row,
            kind: J_FEE,
            slot: 2,
        };
        assert!(journal.note(&fee), "gate 113: the journal's ring has room");
        fees += u64::from(code & FEE_FLAG_EQ_FLOOR != 0);
        while let Some(r) = j_c.try_pop_ref() {
            std::hint::black_box((r.b, r.c));
        }
        // The cancel's answer; the venue's codes.
        let (cid_span, ok) = scan_eq_cancel(CANCEL).expect("gate 113 cancel");
        assert!(ok && cid_span.get(CANCEL) == id.get());
        assert!(
            classify(EQ_NOT_TRADABLE, 400) == CodeClass::RowFatal && is_venue_code(EQ_DISCLAIMER) && is_venue_code(EQ_DUPLICATE_ID),
            "gate 113: the venue's codes"
        );
        // At their own cadence: the listing, a full page, the cancel-all.
        if i % 64 == 0 {
            lists += scan_eq_open(OPEN, &mut open).expect("gate 113 open orders") as u64;
            let p = scan_eq_trades(&page, &mut rows).expect("gate 113 page");
            assert_eq!((p.total, p.n), (241, EQ_PAGE), "gate 113: a full page of the window's 241");
            pages += 1;
            assert_eq!(scan_eq_cancel_all(CANCEL_ALL), Ok(true));
        }
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    std::hint::black_box((&table, rendered));
    assert_eq!((fees, booked), (N, N as i64 * 30_000), "gate 113: a floored fee and 0.03 booked an order");
    assert_eq!((pages, lists), (N.div_ceil(64), N.div_ceil(64)), "gate 113: the page and the listing at their cadence");
    assert_eq!(allocs, 0, "gate 113: equity's render and scans allocated {allocs} times ({bytes} B)");
}

/// PM15 gate 114: the rolling 15m ingress driver's frame paths and slot 9's
/// member, in steady state — `book` snapshots, `price_change` events (the
/// ladder, the venue touch, the change-gated tick), Chainlink reports
/// (marks), the roll clock; and the member's report grid, nowcast, pricer,
/// persistence, take, fill and settlement. Zero bytes per operation.
#[test]
fn pm15_ingress_driver_and_member_are_zero_alloc() {
    use core_ring::Ring;
    use core_twaplock::late::LateLut;
    use core_types::pm_roll::{
        pm_instance_id, pm_mark_sym, pm_pair_of_window, pm_roll_family, pm_rolling_sym,
        pm_window_end_ns, pm_window_of_open, PM_LEG_DOWN, PM_LEG_UP, PM_SETTLED_UP, PM_TWAP_S,
    };
    use core_types::{ChannelEvent, ChannelId, Fill, NullCapture, Order, Price, Qty, Side, Tick, VenueId};
    use ingress_polymarket::gamma15::PmWindowSpec;
    use ingress_polymarket::pm15_loop::{Pm15Counters, Pm15Driver, Pm15Out, Pm15Wires, PM15_EVENT_MASK};
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// A wire that counts what it would send.
    struct CountWire(usize);
    impl Pm15Wires for CountWire {
        fn is_open(&self, _k: usize) -> bool {
            true
        }
        fn queue(&mut self, _k: usize, parts: &[&[u8]]) -> bool {
            let mut i = 0usize;
            while i < parts.len() {
                self.0 += parts[i].len();
                i += 1;
            }
            true
        }
    }
    /// A ctx that counts orders and keeps the last one.
    struct CountCtx {
        now: u64,
        n: u64,
        last: Option<Order>,
    }
    impl Ctx for CountCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            self.n += 1;
            self.last = Some(order);
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    const S: u64 = 1_000_000_000;
    const OPEN: u64 = 1_792_216_800;
    let w = pm_window_of_open(OPEN).unwrap();
    let up = b"74913418352376030655968323928940542465856871160803692353977404305259528321778";
    let down = b"108416957532714595532891587985594213692071846013834323276407586325741402897137";

    // ---- the ingress driver (boot: tables, rings, frames) ----
    let mut spec = PmWindowSpec::EMPTY;
    spec.window = w;
    spec.tick_1e6 = 10_000;
    spec.min_size_1e6 = 5_000_000;
    spec.up.b[..up.len()].copy_from_slice(up);
    spec.up.len = up.len() as u8;
    spec.down.b[..down.len()].copy_from_slice(down);
    spec.down.len = down.len() as u8;
    let (mut tick_tx, mut tick_rx) = Ring::<Tick, { ingress_polymarket::DEFAULT_TICK_RING_CAP }>::new().split();
    let (mut ev_tx, mut ev_rx) = Ring::<ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();
    let counters = Pm15Counters::new();
    let mut cap = NullCapture;
    let mut wire = CountWire(0);
    let mut d = Pm15Driver::new(&[b"btc"], 5_000);
    let wall0 = (OPEN + 100) * S;
    {
        let mut out = Pm15Out { tick: &mut tick_tx, event: &mut ev_tx, event_mask: PM15_EVENT_MASK, status: &status, counters: &counters, capture: &mut cap };
        d.offer(&spec, wall0, wall0, &mut out, &mut wire);
    }
    let us = std::str::from_utf8(up).unwrap();
    let ds = std::str::from_utf8(down).unwrap();
    let book = format!(
        "[{{\"market\":\"0x70\",\"asset_id\":\"{us}\",\"timestamp\":\"1792216900000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.40\",\"size\":\"10\"}},{{\"price\":\"0.50\",\"size\":\"10\"}}],\"asks\":[{{\"price\":\"0.60\",\"size\":\"7\"}},{{\"price\":\"0.52\",\"size\":\"7\"}}],\"event_type\":\"book\"}}, {{\"market\":\"0x70\",\"asset_id\":\"{ds}\",\"timestamp\":\"1792216900000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.48\",\"size\":\"7\"}}],\"asks\":[{{\"price\":\"0.50\",\"size\":\"10\"}}],\"event_type\":\"book\"}}]"
    );
    let mut changes: Vec<String> = Vec::with_capacity(64);
    let mut k = 0u32;
    while k < 64 {
        let sz = 10 + k % 7;
        changes.push(format!(
            "{{\"market\":\"0x70\", \"price_changes\":[{{\"asset_id\":\"{us}\", \"price\":\"0.50\", \"size\":\"{sz}\", \"side\":\"BUY\", \"hash\":\"a\", \"best_bid\":\"0.5\", \"best_ask\":\"0.52\"}}, {{\"asset_id\":\"{ds}\", \"price\":\"0.50\", \"size\":\"{sz}\", \"side\":\"SELL\", \"hash\":\"b\", \"best_bid\":\"0.48\", \"best_ask\":\"0.5\"}}], \"timestamp\":\"{}\", \"event_type\":\"price_change\"}}",
            1_792_216_900_000u64 + u64::from(k)
        ));
        k += 1;
    }
    let mut marks: Vec<String> = Vec::with_capacity(64);
    let mut k = 0u64;
    while k < 64 {
        marks.push(format!(
            "{{\"connection_id\":\"x\",\"payload\":{{\"full_accuracy_value\":\"{}\",\"symbol\":\"btc/usd\",\"timestamp\":{},\"value\":1.0}},\"timestamp\":1,\"topic\":\"crypto_prices_chainlink\",\"type\":\"update\"}}",
            81_000_000_000_000_000_000_000u128 + u128::from(k) * 1_000_000_000_000_000,
            (OPEN + 200 + k) * 1_000
        ));
        k += 1;
    }

    let g = AllocGuard::new();
    let mut i = 0usize;
    while i < 2_000 {
        let now = wall0 + i as u64 * 1_000_000;
        let mut out = Pm15Out { tick: &mut tick_tx, event: &mut ev_tx, event_mask: PM15_EVENT_MASK, status: &status, counters: &counters, capture: &mut cap };
        if i % 256 == 0 {
            d.on_market_text(0, book.as_bytes(), now, now, &mut out);
        }
        d.on_market_text(0, changes[i % 64].as_bytes(), now, now, &mut out);
        if i % 32 == 0 {
            d.on_rtds_text(marks[(i / 32) % 64].as_bytes(), now, &mut out);
            d.on_clock(now, now, &mut out, &mut wire);
        }
        d.perform(now, &mut out, &mut wire);
        while tick_rx.try_pop_ref().is_some() {}
        while ev_rx.try_pop_ref().is_some() {}
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    assert!(counters.of(ingress_polymarket::pm15_loop::Pm15Counter::Marks) > 0, "gate 114: marks flowed");
    assert!(status.ticks_total() > 0, "gate 114: ticks flowed");
    assert_eq!(allocs, 0, "gate 114: the PM15 ingress driver allocated {allocs} times ({bytes} B)");

    // ---- slot 9's member (boot: configure, the grids) ----
    let mut lut = Box::new(LateLut::flat());
    let mut z = 0usize;
    while z < core_twaplock::late::LATE_PHI_POINTS {
        lut.phi[z] = (500_000 + ((z as i64 * 2_000).min(4_000_000) / 8)) as u32;
        z += 1;
    }
    let perp = core_types::make_symbol_id(VenueId::Hyperliquid, 1);
    let mut perp_sym = [core_types::SYMBOL_ID_NONE; strategy_pm15::PM15_MAX_FAMILIES];
    perp_sym[0] = perp;
    let p = strategy_pm15::Pm15Params { n_families: 1, perp_sym, ..strategy_pm15::Pm15Params::default() };
    let mut m = strategy_pm15::Pm15Strategy::new();
    m.configure(p, lut, core_time::WallAnchor::new(0, 0)).expect("gate 114: configure");
    let roll = |wc: u32, settled: bool, v0: i64| {
        let pair = pm_pair_of_window(wc);
        let inst = pm_instance_id(wc, 0).unwrap();
        ChannelEvent::new(0, VenueId::Polymarket, ChannelId::InstrumentRoll, pm_rolling_sym(0, pair, PM_LEG_UP), core_types::pack_roll_seq(inst, PM_TWAP_S, pm_roll_family(0, pair), settled), 0, v0, pm_window_end_ns(wc) as i64)
    };
    let law = |wc: u32| {
        let inst = pm_instance_id(wc, 0).unwrap();
        ChannelEvent::new(0, VenueId::Polymarket, ChannelId::OutcomeMeta, pm_rolling_sym(0, pm_pair_of_window(wc), PM_LEG_UP), u64::from(inst), 0, 10_000, 5_000_000)
    };
    let mut c = CountCtx { now: 0, n: 0, last: None };
    let base: i64 = 81_000_000_000;
    let g = AllocGuard::new();
    let mut cycle = 0u32;
    // Four windows two apart (one slot pair), each with the 20 minutes of
    // tape before it (the overlap is older than the newest report: ignored).
    while cycle < 4 {
        let wc = w + 2 * cycle;
        let open = OPEN + u64::from(cycle) * 1_800;
        let end = open + 900;
        let pair = pm_pair_of_window(wc);
        let mut s = open - 1_200;
        while s < end - 2 {
            if s == open {
                c.now = open * S;
                m.on_venue_event(&roll(wc, false, 0), &mut c);
                m.on_venue_event(&law(wc), &mut c);
            }
            let px = if s >= open { base + 400_000_000 } else { base } + if s % 2 == 0 { 2_000_000 } else { -2_000_000 };
            c.now = s * S - 200_000_000;
            m.on_tick(&Tick::new(c.now, VenueId::Hyperliquid, perp, 1, Price::from_raw(px - 500_000), Qty::from_raw(1), Price::from_raw(px + 500_000), Qty::from_raw(1)), &mut c);
            c.now = s * S + 1_300_000_000;
            m.on_venue_event(&ChannelEvent::new(c.now, VenueId::Polymarket, ChannelId::Mark, pm_mark_sym(0), s, s * 1_000, px, px * 1_000_000), &mut c);
            m.on_tick(&Tick::new(c.now, VenueId::Polymarket, pm_rolling_sym(0, pair, PM_LEG_UP), 1, Price::from_raw(920_000), Qty::from_raw(500_000_000), Price::from_raw(930_000), Qty::from_raw(500_000_000)), &mut c);
            m.on_tick(&Tick::new(c.now, VenueId::Polymarket, pm_rolling_sym(0, pair, PM_LEG_DOWN), 1, Price::from_raw(70_000), Qty::from_raw(500_000_000), Price::from_raw(80_000), Qty::from_raw(500_000_000)), &mut c);
            if let Some(o) = c.last.take() {
                m.on_fill(&Fill::new(c.now, o.sym, Side::Bid, Price::from_raw(930_000), o.qty, o.client_oid), &mut c);
            }
            m.on_timer(c.now, &mut c);
            s += 1;
        }
        m.on_venue_event(&roll(wc, true, PM_SETTLED_UP), &mut c);
        cycle += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let ct = m.counters();
    assert!(c.n >= 4 && ct.settled_won == 4, "gate 114: the member took and settled every cycle ({} orders, {:?})", c.n, ct);
    assert_eq!(allocs, 0, "gate 114: slot 9's member allocated {allocs} times ({bytes} B)");
    std::hint::black_box((wire.0, ct));
}

/// PM15-6a gate 115: the slot-9 arm's order path without a socket — the
/// FAK amounts, the struct hash and signature (EOA and the Deposit
/// Wallet's wrapper), the per-order tail, the L2 MAC over the request's
/// spans, the answer's read and a user-channel frame's scan. Zero bytes
/// per order. (PM15-6b: the head is `HttpsReq`'s, gate 77b's.)
#[test]
fn pm15_exec_order_path_is_zero_alloc() {
    use exec_polymarket::amounts::fak_buy;
    use exec_polymarket::l2::L2Secret;
    use exec_polymarket::render::{render_order_tail, render_token_prefix, render_ts, PREFIX_MAX, TAIL_MAX};
    use exec_polymarket::response::{read_order_answer, Verdict};
    use exec_polymarket::sign::{sign_order, SIG_MAX};
    use exec_polymarket::userws::{scan_user_frame, UserEvt, KIND_TRADE};
    use signer_eip712::pmv2::{
        eip55, exchange_separator, u256_from_decimal, OrderV2, CHAIN_POLYGON, EXCHANGE_V2, SIDE_BUY, SIG_EOA,
        SIG_POLY_1271,
    };

    const N: u64 = 256;
    let key = [0x42u8; 32];
    let sk = signer_eip712::parse_secret_key(&key).unwrap();
    let me = signer_eip712::address_from_private_key(&key).unwrap();
    let me_text = eip55(&me);
    let tok_dec: &[u8] = b"74913418352376030655968323928940542465856871160803692353977404305259528321778";
    let tok = u256_from_decimal(tok_dec).unwrap();
    let sep = exchange_separator(b"2", CHAIN_POLYGON, EXCHANGE_V2);
    let secret = L2Secret::from_base64(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
    let api_key: &[u8] = b"00000000-0000-0000-0000-000000000000";
    let mut prefix = [0u8; PREFIX_MAX];
    let mut tail = [0u8; TAIL_MAX];
    let mut sig = [0u8; SIG_MAX];
    let mut ts = [0u8; 20];
    let mut evts = [UserEvt::BLANK; 4];
    let p = render_token_prefix(&me_text, &me_text, tok_dec, SIG_EOA, &mut prefix);
    let answer: &[u8] = br#"{"success":true,"errorMsg":"","orderID":"0x72e7378745dcfb7f03ad3bb1663fc00a91332015d6000399d131536361deaf9a","status":"matched","makingAmount":"2.5","takingAmount":"4.5454","tradeIDs":["t1"]}"#;
    let frame: &[u8] = br#"{"event_type":"trade","type":"TRADE","id":"28c4d2eb-bbea-40e7-a9f0-b2fdb56b2c2e","taker_order_id":"0x72e7378745dcfb7f03ad3bb1663fc00a91332015d6000399d131536361deaf9a","asset_id":"74913418352376030655968323928940542465856871160803692353977404305259528321778","side":"BUY","size":"4.5454","price":"0.55","status":"MATCHED","trader_side":"TAKER","maker_orders":[{"order_id":"0xab","price":"0.45","side":"BUY"}],"timestamp":"1792217690300"}"#;

    let mut bytes_out = 0usize;
    let mut accepted = 0u64;
    let mut fills = 0u64;
    // Warm-up: the first signature builds the process's secp256k1 signing
    // context (a boot cost, 208 B once) — taken before the count.
    let warm = OrderV2 {
        salt: 1,
        maker: me,
        signer: me,
        token_id: tok,
        maker_amount: 1,
        taker_amount: 1,
        timestamp_ms: 1,
        metadata: [0; 32],
        builder: [0; 32],
        side: SIDE_BUY,
        signature_type: SIG_EOA,
        _pad: [0; 6],
    };
    sign_order(&sk, &sep, CHAIN_POLYGON, &warm, &mut sig).unwrap();
    let g = AllocGuard::new();
    let mut i = 0u64;
    while i < N {
        let a = fak_buy(250_000_000 - i * 10_000, 950_000, 10_000).unwrap();
        let sig_type = if i % 2 == 0 { SIG_EOA } else { SIG_POLY_1271 };
        let o = OrderV2 {
            salt: 479_249_096_354 + i,
            maker: me,
            signer: me,
            token_id: tok,
            maker_amount: a.maker,
            taker_amount: a.taker,
            timestamp_ms: 1_792_217_690_123 + i,
            metadata: [0; 32],
            builder: [0; 32],
            side: SIDE_BUY,
            signature_type: sig_type,
            _pad: [0; 6],
        };
        let s = sign_order(&sk, &sep, CHAIN_POLYGON, &o, &mut sig).unwrap();
        let t = render_order_tail(o.salt, &a, o.timestamp_ms, &sig[..s.sig_len], api_key, &mut tail);
        let tn = render_ts(1_792_217_690 + i, &mut ts);
        let mac = secret.sign(&[&ts[..tn], b"POST", b"/order", &prefix[..p], &tail[..t]]);
        bytes_out += mac.len() + t;
        if read_order_answer(200, answer).verdict == Verdict::Accepted {
            accepted += 1;
        }
        let r = scan_user_frame(frame, &mut evts).unwrap();
        if r.events == 1 && evts[0].kind == KIND_TRADE {
            fills += 1;
        }
        i += 1;
    }
    let (allocs, nbytes, _) = g.delta();
    std::hint::black_box((bytes_out, &tail));
    assert_eq!((accepted, fills), (N, N), "gate 115: every answer accepted and every frame a fill");
    assert_eq!(allocs, 0, "gate 115: the Polymarket order path allocated {allocs} times ({nbytes} B)");
}

/// PM15-6b gate 130: the slot-9 arm's two halves on one thread, at 0 B per
/// order. pm-gw end to end on a fixed CLOB rig — the window from the spec
/// ring, the amounts, the signature, the body in place, the MAC, SENT, the
/// request and its answer (the venue's zero-fill FAK refusal) — and the
/// arm admitting, draining SENT and REFUSED and retiring; then the arm
/// alone against a scripted duplex: SENT, ACK, a channel trade booked
/// once, a repeat, the retirement.
#[test]
fn pm15_exec_arm_and_gateway_are_zero_alloc() {
    use clob_dispatcher::OrderDispatch;
    use core_types::pm_roll::{pm_instance_id, pm_pair_of_window, pm_rolling_sym, pm_window_of_open};
    use core_types::{ChannelEvent, ChannelId, Order, Price, Qty, Side, VenueId};
    use exec_polymarket::account::PmAccount;
    use exec_polymarket::arm::{PmArm, PmArmCfg};
    use exec_polymarket::clob::PmClob;
    use exec_polymarket::cmd::{PmCmd, PmEvt, EVT_ACK, EVT_SENT, PM_CMD_RING, PM_EVT_RING, PM_USER_RING};
    use exec_polymarket::gw::{PmGateway, BODY_MIN};
    use exec_polymarket::userws::TS_MATCHED;
    use ingress_polymarket::gamma15::{PmTokenId, PmWindowSpec};
    use ingress_polymarket::pm15_loop::PM15_SPEC_RING;

    struct FixedClob {
        body: Box<[u8; BODY_MIN]>,
        resp: &'static [u8],
        posts: u64,
    }
    impl PmClob for FixedClob {
        fn body_mut(&mut self) -> &mut [u8] {
            &mut self.body[..]
        }
        fn request(
            &mut self,
            _m: core_net::Method,
            _t: &[u8],
            extra: &[core_net::Header<'_>],
            body_len: usize,
        ) -> Result<(u16, core::ops::Range<usize>), core_net::PostErr> {
            self.posts += (extra.len() == 5 && body_len > 0) as u64;
            Ok((400, 0..self.resp.len()))
        }
        fn resp(&self) -> &[u8] {
            self.resp
        }
    }

    const N: u64 = 64;
    let key = [0x42u8; 32];
    let me = signer_eip712::address_from_private_key(&key).unwrap();
    let acct = |k: &[u8; 32]| {
        PmAccount::new(
            0,
            me,
            None,
            b"00000000-0000-0000-0000-000000000000",
            b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            b"pass",
            signer_eip712::parse_secret_key(k).unwrap(),
        )
        .unwrap()
    };
    let w = pm_window_of_open(1_792_216_800).unwrap();
    let inst = pm_instance_id(w, 0).unwrap();
    let pair = pm_pair_of_window(w);
    let up = pm_rolling_sym(0, pair, 0);
    let mut spec = PmWindowSpec::EMPTY;
    spec.window = w;
    spec.tick_1e6 = 10_000;
    spec.min_size_1e6 = 5_000_000;
    let tok = |d: &[u8]| {
        let mut t = PmTokenId::NONE;
        t.b[..d.len()].copy_from_slice(d);
        t.len = d.len() as u8;
        t
    };
    spec.up = tok(b"74913418352376030655968323928940542465856871160803692353977404305259528321778");
    spec.down = tok(b"1234");
    spec.condition_id_len = 66;
    spec.condition_id.copy_from_slice(b"0x53efa9a4a5bddfd12a4d154653e337cedf2ec6667842dca165036ac21f6765b7");
    let law = ChannelEvent::new(0, VenueId::Polymarket, ChannelId::OutcomeMeta, up, u64::from(inst), 0, 10_000, 5_000_000);
    let order = |i: u64| {
        let mut o = Order::new(
            1,
            VenueId::Polymarket,
            up,
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(930_000),
            Qty::from_raw(268_810_000),
            (i << 49) | u64::from(inst),
        );
        o.strategy_id = 9;
        o
    };

    // Half one: the arm and pm-gw on one thread.
    let (eng, end) = exec_gateway::duplex::<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>();
    let (mut specs_tx, specs_rx) = core_ring::Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let (_user_tx, user_rx) = core_ring::Ring::<PmEvt, PM_USER_RING>::new().split();
    let clob = FixedClob {
        body: exec_gateway::boxed_array(0),
        resp: br#"{"error":"no orders found to match with FAK order. FAK orders are partially filled or killed if no match is found."}"#,
        posts: 0,
    };
    let mut gw = PmGateway::new(acct(&key), clob, end, specs_rx, 7);
    let mut arm = PmArm::new(PmArmCfg { slot: 9, live_families: 1 }, eng, user_rx);
    arm.on_venue_event(&law);
    assert!(specs_tx.try_push_ref(&spec));
    gw.step(1, 1_792_217_690_000_000_000);
    // Warm-up: the first signature builds the secp256k1 context (boot).
    arm.submit(&order(0)).unwrap();
    gw.step(2, 1_792_217_690_000_000_000);
    arm.on_idle();
    arm.on_idle();
    let _ = arm.try_next_retired();
    let g = AllocGuard::new();
    let mut retired = 0u64;
    let mut i = 1u64;
    while i <= N {
        arm.submit(&order(i)).unwrap();
        gw.step(2 + i, 1_792_217_690_000_000_000 + i * 1_000_000);
        arm.on_idle();
        arm.on_idle();
        while arm.try_next_retired().is_some() {
            retired += 1;
        }
        i += 1;
    }
    let (allocs, nbytes, _) = g.delta();
    assert_eq!(retired, N, "gate 130: every order refused and retired");
    assert_eq!(gw.counters().refused, N + 1);
    assert_eq!(allocs, 0, "gate 130: the arm and pm-gw allocated {allocs} times ({nbytes} B)");

    // Half two: the arm against a scripted duplex and user ring.
    let (eng, mut end) = exec_gateway::duplex::<PmCmd, PmEvt, PM_CMD_RING, PM_EVT_RING>();
    let (mut user_tx, user_rx) = core_ring::Ring::<PmEvt, PM_USER_RING>::new().split();
    let mut arm = PmArm::new(PmArmCfg { slot: 9, live_families: 1 }, eng, user_rx);
    arm.on_venue_event(&law);
    let g = AllocGuard::new();
    let mut fills = 0u64;
    let mut retired = 0u64;
    let mut i = 1u64;
    while i <= N {
        let o = order(i);
        arm.submit(&o).unwrap();
        while end.cmd.pop().is_some() {}
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&i.to_le_bytes());
        let mut e = PmEvt::about(EVT_SENT, o.client_oid);
        e.order_id = id;
        assert!(end.evt.emit(&e));
        let mut e = PmEvt::about(EVT_ACK, o.client_oid);
        e.order_id = id;
        e.b = 1;
        assert!(end.evt.emit(&e));
        let t = PmEvt::fill(id, 1_000 + i, 930_000, 268_810_000, TS_MATCHED);
        assert!(user_tx.try_push_ref(&t));
        assert!(user_tx.try_push_ref(&t));
        while arm.try_next_fill().is_some() {
            fills += 1;
        }
        while arm.try_next_retired().is_some() {
            retired += 1;
        }
        i += 1;
    }
    let (allocs, nbytes, _) = g.delta();
    assert_eq!(
        (fills, retired),
        (N, N),
        "gate 130: each trade booked once, each order retired"
    );
    assert_eq!(
        allocs, 0,
        "gate 130: the arm allocated {allocs} times ({nbytes} B)"
    );
}

/// Gate 131 (PM15-6d): the relayer redemption — the wallet's nonce read,
/// the batch (the boot's approval, then eight markets) hashed, signed and
/// rendered in the client's window, the submit, the poll to its
/// confirmation — at 0 B per batch.
#[test]
fn pm15_relayer_redemption_is_zero_alloc() {
    use exec_polymarket::account::PmAccount;
    use exec_polymarket::clob::PmClob;
    use exec_polymarket::redeem::{
        Redeemer, RelayerKey, POLL_GAP_NS, REDEEM_AFTER_NS, REDEEM_BATCH, RELAYER_BODY,
    };

    struct FixedRelayer {
        body: Box<[u8; RELAYER_BODY]>,
        resp: &'static [u8],
        submits: u64,
    }
    impl PmClob for FixedRelayer {
        fn body_mut(&mut self) -> &mut [u8] {
            &mut self.body[..]
        }
        fn request(
            &mut self,
            m: core_net::Method,
            t: &[u8],
            _extra: &[core_net::Header<'_>],
            body_len: usize,
        ) -> Result<(u16, core::ops::Range<usize>), core_net::PostErr> {
            self.resp = if t.starts_with(b"/v1/account/transactions/params") {
                br#"{"address":"0x","nonce":"31"}"#
            } else if m == core_net::Method::Post {
                self.submits += u64::from(body_len > 4_000);
                br#"{"transactionID":"0190b317-a1d3-7bec-9b91-eeb6dcd3a620","state":"STATE_NEW"}"#
            } else {
                br#"{"transaction_id":"x","state":"STATE_CONFIRMED"}"#
            };
            Ok((200, 0..self.resp.len()))
        }
        fn resp(&self) -> &[u8] {
            self.resp
        }
    }

    const N: u64 = 16;
    let key = [0x42u8; 32];
    let me = signer_eip712::address_from_private_key(&key).unwrap();
    let acct = PmAccount::new(
        3,
        me,
        Some([0x11; 20]),
        b"00000000-0000-0000-0000-000000000000",
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        b"pass",
        signer_eip712::parse_secret_key(&key).unwrap(),
    )
    .unwrap();
    let mut rly = FixedRelayer {
        body: exec_gateway::boxed_array(0),
        resp: b"",
        submits: 0,
    };
    let mut r = Redeemer::new(RelayerKey::new(b"relayer-key").unwrap(), [0x11; 20], 137);
    let mut now = 0u64;
    let mut g = None;
    let mut c = 0u64;
    while c <= N {
        if c == 1 {
            // The warm-up batch (the approval; the secp256k1 context) is boot.
            g = Some(AllocGuard::new());
        }
        let mut k = 0usize;
        while k < REDEEM_BATCH {
            let mut cond = [0u8; 32];
            cond[0] = c as u8;
            cond[1] = k as u8;
            r.ask(&cond, now);
            k += 1;
        }
        now += REDEEM_AFTER_NS;
        while r.waiting() > 0 {
            if r.due(now) {
                r.step(&mut rly, &acct, now, 1_792_225_360);
            } else {
                now += POLL_GAP_NS;
            }
        }
        c += 1;
    }
    let (allocs, nbytes, _) = g.unwrap().delta();
    let k = r.counters();
    assert_eq!(
        (k.batches_done, k.redeemed),
        (N + 1, (N + 1) * REDEEM_BATCH as u64)
    );
    assert_eq!(rly.submits, N + 1, "gate 131: every batch is a full one");
    assert_eq!(
        allocs, 0,
        "gate 131: the redeemer allocated {allocs} times ({nbytes} B)"
    );
}

/// Gate 132 (PM15-7, dormant): PolyBolt's frames — the auth and subscribe
/// renders and the parse of a price and a TWAP report — at 0 B.
#[test]
fn pm15_polybolt_frames_are_zero_alloc() {
    use ingress_polymarket::polybolt::{
        parse_polybolt, render_auth, render_subscribe, PbFrame, PbReport, POLYBOLT_TWAP_S,
    };
    const PRICE: &[u8] = br#"{"v":1,"channel":"price.crypto","seq":5,"payload":{"timestamp":"2026-09-08T16:49:37Z","value":"78794.15450441","symbol":"btcusd"}}"#;
    const TWAP: &[u8] = br#"{"v":1,"channel":"price.crypto.twap","seq":6,"payload":{"symbol":"ethusd","window_seconds":60,"full_accuracy_value":"4123456789012345678901","timestamp":1792238400250}}"#;
    let mut buf = [0u8; 1024];
    let mut r = PbReport::default();
    let g = AllocGuard::new();
    let mut n = 0u64;
    let mut i = 0u64;
    while i < 64 {
        n += render_auth(&mut buf, b"key", b"c2VjcmV0", b"pass").unwrap() as u64;
        n += render_subscribe(&mut buf, &[b"btc", b"eth"], POLYBOLT_TWAP_S).unwrap() as u64;
        n += u64::from(parse_polybolt(PRICE, &mut r) == PbFrame::Price);
        n += u64::from(parse_polybolt(TWAP, &mut r) == PbFrame::Twap);
        i += 1;
    }
    let (allocs, nbytes, _) = g.delta();
    assert!(n > 0 && r.px.px_1e6 == 4_123_456_789);
    assert_eq!(
        allocs, 0,
        "gate 132: PolyBolt's frames allocated {allocs} times ({nbytes} B)"
    );
}

/// PMJ J1b gate 133: the 5-minute pipeline's ingress driver
/// (`Pm15Driver::new_for(PmPeriod::M5, ..)`) in steady state — `book`
/// snapshots, `price_change` events (the ladder, the venue touch, the
/// change-gated tick and the change-gated top-of-book), Chainlink reports
/// on the 5 m mark slot, the roll clock across a window's end and its
/// retirement; R-PMJ-S15: under the deployed depth policy (one record per
/// leg per second) with a market `PONG` every 5 s, so the spacing gate and
/// the sweep run too; PMJ J7: on the deployed sockets (one per family), the
/// window's shard dropping and reopening every 50 s — the withdrawal, the
/// ladders' restart and the resubscribe. Zero bytes per operation.
#[test]
fn pm5_ingress_driver_is_zero_alloc() {
    use core_ring::Ring;
    use core_types::pm_roll::{pm5_window_of_open, PmPeriod};
    use core_types::{ChannelEvent, NullCapture, Tick};
    use ingress_polymarket::gamma15::PmWindowSpec;
    use ingress_polymarket::pm15_loop::{
        Pm15Counters, Pm15Driver, Pm15Out, Pm15Wires, PM15_EVENT_MASK, PM15_MARKET_SOCKETS_MAX,
    };

    struct CountWire(usize);
    impl Pm15Wires for CountWire {
        fn is_open(&self, _k: usize) -> bool {
            true
        }
        fn queue(&mut self, _k: usize, parts: &[&[u8]]) -> bool {
            let mut i = 0usize;
            while i < parts.len() {
                self.0 += parts[i].len();
                i += 1;
            }
            true
        }
    }

    const S: u64 = 1_000_000_000;
    const OPEN5: u64 = 1_792_308_000;
    let w = pm5_window_of_open(OPEN5).unwrap();
    let up = b"74913418352376030655968323928940542465856871160803692353977404305259528321778";
    let down = b"108416957532714595532891587985594213692071846013834323276407586325741402897137";
    let mut spec = PmWindowSpec::EMPTY;
    spec.window = w;
    spec.tick_1e6 = 10_000;
    spec.min_size_1e6 = 5_000_000;
    spec.market_id = 5_768_386;
    spec.up.b[..up.len()].copy_from_slice(up);
    spec.up.len = up.len() as u8;
    spec.down.b[..down.len()].copy_from_slice(down);
    spec.down.len = down.len() as u8;
    let (mut tick_tx, mut tick_rx) = Ring::<Tick, { ingress_polymarket::DEFAULT_TICK_RING_CAP }>::new().split();
    let (mut ev_tx, mut ev_rx) = Ring::<ChannelEvent, { core_types::EVENT_RING_SIZE }>::new().split();
    let status = core_metrics::IngressStatus::new();
    let counters = Pm15Counters::new();
    let mut cap = NullCapture;
    let mut wire = CountWire(0);
    // R-PMJ-S15: the deployed depth policy (one record per leg per second,
    // the PONG sweep catching up) — the same 0 B.
    let mut d = Pm15Driver::new_for(PmPeriod::M5, &[b"btc"], 5_000)
        .with_depth_policy(true, 1_000)
        .with_market_sockets(PM15_MARKET_SOCKETS_MAX);
    let shard = d.shard_of(0);
    let wall0 = (OPEN5 + 10) * S;
    {
        let mut out = Pm15Out { tick: &mut tick_tx, event: &mut ev_tx, event_mask: PM15_EVENT_MASK, status: &status, counters: &counters, capture: &mut cap };
        d.offer(&spec, wall0, wall0, &mut out, &mut wire);
    }
    let us = std::str::from_utf8(up).unwrap();
    let ds = std::str::from_utf8(down).unwrap();
    let book = format!(
        "[{{\"market\":\"0x70\",\"asset_id\":\"{us}\",\"timestamp\":\"1792308010000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.40\",\"size\":\"10\"}},{{\"price\":\"0.50\",\"size\":\"10\"}}],\"asks\":[{{\"price\":\"0.60\",\"size\":\"7\"}},{{\"price\":\"0.52\",\"size\":\"7\"}}],\"event_type\":\"book\"}}, {{\"market\":\"0x70\",\"asset_id\":\"{ds}\",\"timestamp\":\"1792308010000\",\"hash\":\"h\",\"bids\":[{{\"price\":\"0.48\",\"size\":\"7\"}}],\"asks\":[{{\"price\":\"0.50\",\"size\":\"10\"}}],\"event_type\":\"book\"}}]"
    );
    let mut changes: Vec<String> = Vec::with_capacity(64);
    let mut k = 0u32;
    while k < 64 {
        let sz = 10 + k % 7;
        changes.push(format!(
            "{{\"market\":\"0x70\", \"price_changes\":[{{\"asset_id\":\"{us}\", \"price\":\"0.49\", \"size\":\"{sz}\", \"side\":\"BUY\", \"hash\":\"a\", \"best_bid\":\"0.5\", \"best_ask\":\"0.52\"}}, {{\"asset_id\":\"{ds}\", \"price\":\"0.51\", \"size\":\"{sz}\", \"side\":\"SELL\", \"hash\":\"b\", \"best_bid\":\"0.48\", \"best_ask\":\"0.5\"}}], \"timestamp\":\"{}\", \"event_type\":\"price_change\"}}",
            1_792_308_010_000u64 + u64::from(k)
        ));
        k += 1;
    }
    let mut marks: Vec<String> = Vec::with_capacity(64);
    let mut k = 0u64;
    while k < 64 {
        marks.push(format!(
            "{{\"connection_id\":\"x\",\"payload\":{{\"full_accuracy_value\":\"{}\",\"symbol\":\"btc/usd\",\"timestamp\":{},\"value\":1.0}},\"timestamp\":1,\"topic\":\"crypto_prices_chainlink\",\"type\":\"update\"}}",
            81_000_000_000_000_000_000_000u128 + u128::from(k) * 1_000_000_000_000_000,
            (OPEN5 + 20 + k) * 1_000
        ));
        k += 1;
    }
    let g = AllocGuard::new();
    let mut i = 0usize;
    while i < 2_000 {
        // 2 000 steps of 0.5 s: across the window's end (300 s) and its
        // retirement 540 s later.
        let now = wall0 + i as u64 * 500_000_000;
        let mut out = Pm15Out { tick: &mut tick_tx, event: &mut ev_tx, event_mask: PM15_EVENT_MASK, status: &status, counters: &counters, capture: &mut cap };
        if i % 256 == 0 {
            d.on_market_text(shard, book.as_bytes(), now, now, &mut out);
        }
        d.on_market_text(shard, changes[i % 64].as_bytes(), now, now, &mut out);
        if i % 32 == 0 {
            d.on_rtds_text(marks[(i / 32) % 64].as_bytes(), now, &mut out);
        }
        if i % 10 == 0 {
            d.on_market_text(shard, b"PONG", now, now, &mut out);
        }
        // PMJ J7: the window's shard drops and reopens.
        if i % 100 == 99 {
            d.on_market_down(shard, now, &mut out);
            assert!(d.on_market_open(shard, &mut wire));
        }
        d.on_clock(now, now, &mut out, &mut wire);
        d.perform(now, &mut out, &mut wire);
        while tick_rx.try_pop_ref().is_some() {}
        while ev_rx.try_pop_ref().is_some() {}
        i += 1;
    }
    let (allocs, bytes, _) = g.delta();
    assert!(counters.of(ingress_polymarket::pm15_loop::Pm15Counter::Marks) > 0, "gate 133: marks flowed");
    assert!(status.ticks_total() > 0, "gate 133: ticks flowed");
    assert_eq!(
        counters.of(ingress_polymarket::pm15_loop::Pm15Counter::RollsUnknown),
        1,
        "gate 133: the window ended and retired inside the measured run"
    );
    assert!(
        counters.of(ingress_polymarket::pm15_loop::Pm15Counter::TouchesWithdrawn) > 0,
        "gate 133: a shard's loss withdrew its touches"
    );
    assert_eq!(allocs, 0, "gate 133: the 5 m ingress driver allocated {allocs} times ({bytes} B)");
}

/// PMJ J3 gate 134: the slot-11 arm's per-order work that is not I/O, at
/// 0 B — the build answer scanned in place, the transaction base64-decoded
/// into its fixed buffer, parsed, GUARDED, signed into its own slot, the
/// execute body rendered (the signed transaction encoded straight into it,
/// the context carried back), and the status and execute answers scanned.
#[test]
fn jupiter_guard_sign_and_scanners_are_zero_alloc() {
    use exec_jupiter::json::{scan_build, scan_execute, scan_status, BuildAnswer, ExecuteAnswer, StatusAnswer};
    use exec_jupiter::render::{execute_body, order_body, status_path, Outcome, EXECUTE_BODY_MAX, ORDER_BODY_MAX, PATH_MAX};
    use exec_jupiter::tx::{guard, SolTx, TxGuard, TOKEN_PROGRAM, TX_MAX};

    // Boot: the wallet (RFC 8032 TEST 1), a v0 order transaction it pays
    // for with one TransferChecked of the deposit, its build answer.
    let v = &signer_ed25519::RFC8032[0];
    let signer = signer_ed25519::Ed25519Signer::from_seed(&v.seed).expect("gate 134 key");
    let jup = [0x66u8; 32];
    let mut msg = vec![0x80u8, 1, 0, 3, 6];
    msg.extend_from_slice(&v.public);
    msg.extend_from_slice(&[0x33; 32]);
    msg.extend_from_slice(&[0x44; 32]);
    msg.extend_from_slice(&[0x55; 32]);
    msg.extend_from_slice(&jup);
    msg.extend_from_slice(&TOKEN_PROGRAM);
    msg.extend_from_slice(&[0xBB; 32]);
    msg.push(2);
    msg.extend_from_slice(&[4, 4, 0, 1, 2, 3, 8, 1, 2, 3, 4, 5, 6, 7, 8]);
    let mut data = vec![12u8];
    data.extend_from_slice(&5_000_000u64.to_le_bytes());
    data.push(6);
    msg.extend_from_slice(&[5, 4, 1, 3, 2, 0, data.len() as u8]);
    msg.extend_from_slice(&data);
    msg.push(0);
    let mut unsigned = vec![1u8];
    unsigned.extend_from_slice(&[0u8; 64]);
    unsigned.extend_from_slice(&msg);
    let mut b64 = vec![0u8; core_crypto::base64_encoded_len(unsigned.len())];
    let n = core_crypto::base64_encode(&unsigned, &mut b64);
    let answer = format!(
        r#"{{"transaction":"{}","txMeta":{{"blockhash":"x","lastValidBlockHeight":9}},"externalOrderId":null,"requiredSigners":["FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z"],"execution":{{"endpoint":"/execute","context":{{"type":"create_order","orderPubkey":"Ord1111"}}}},"order":{{"orderPubkey":"Ord1111","positionPubkey":"Pos2222","maxBuyPriceUsd":"960000","estimatedTotalFeeUsd":"35000","orderCostUsd":"5000000","contractsMicro":"5263157"}}}}"#,
        std::str::from_utf8(&b64[..n]).unwrap()
    );
    let status = br#"{"orderPubkey":"Ord1111","status":"filled","latestEventType":"filled","latestSignature":"s","externalOrderId":"","orderId":"o","history":[]}"#;
    let exec = br#"{"status":"Success","signature":"5h6xBEauJ3PK6SWCZ1PGjBvj8vDdWG3KpwATGy1ARAXFSDwt8GFXM7W5Ncn16wmqokgpiKRLuS83KUxyZyv2sUYv","error":null}"#;
    let mut g = TxGuard::refuse_all(v.public);
    assert!(g.allow_program(jup) && g.allow_program(TOKEN_PROGRAM));
    g.deposit_account = [0x33; 32];
    g.deposit_mint = [0x55; 32];
    let mut txbuf = [0u8; TX_MAX];
    let mut body = vec![0u8; EXECUTE_BODY_MAX];
    let mut obody = [0u8; ORDER_BODY_MAX];
    let mut path = [0u8; PATH_MAX];
    let mut acc = 0u64;

    let guard_ = AllocGuard::new();
    let mut i = 0u64;
    while i < 2_000 {
        let on = order_body(&mut obody, b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z", 5_798_888 + i, Outcome::Up, 5_000_000, b"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").expect("order body");
        let mut a = BuildAnswer::default();
        scan_build(answer.as_bytes(), &mut a).expect("build answer");
        let tn = core_crypto::base64_decode(a.transaction.of(answer.as_bytes()), &mut txbuf).expect("b64");
        let t = SolTx::parse(&txbuf[..tn]).expect("tx");
        let p = guard(&txbuf[..tn], &t, &g, 5_000_000).expect("guard passes");
        t.sign_into(&mut txbuf[..tn], p.signer_idx, &signer).expect("sign");
        let en = execute_body(&mut body, &txbuf[..tn], a.context.of(answer.as_bytes()), i).expect("execute body");
        let pn = status_path(&mut path, a.order_pubkey.of(answer.as_bytes())).expect("path");
        let mut e = ExecuteAnswer::default();
        scan_execute(exec, &mut e).expect("execute answer");
        let mut s = StatusAnswer::default();
        scan_status(status, &mut s).expect("status answer");
        acc = acc.wrapping_add((on + en + pn) as u64 + p.token_debit + u64::from(txbuf[1]));
        i += 1;
    }
    std::hint::black_box(acc);
    let (allocs, bytes, _) = guard_.delta();
    assert!(acc != 0, "gate 134: real work");
    assert_eq!(allocs, 0, "gate 134: the Jupiter arm's per-order path allocated {allocs} times ({bytes} B)");
}


/// PMJ J3 gate 135: slot 11's arm and jup-gw in their steady state, at 0 B
/// — an order admitted by the arm (`OrderDispatch::submit`), its intent
/// across the duplex, built, guarded, signed, executed, polled and
/// reconciled by jup-gw against a scripted Jupiter answering from fixed
/// buffers, its fill and its retirement back on the engine thread. The
/// arm keeps a done row 120 s, so the run is 3 warm orders and 12 measured.
#[test]
fn jupiter_gateway_and_arm_steady_state_is_zero_alloc() {
    use clob_dispatcher::OrderDispatch;
    use core_net::{Header, Method, PostErr};
    use core_types::pm_roll::{pm5_instance_id, pm5_pair_of_window, pm5_rolling_sym, pm5_window_end_ns};
    use core_types::{ChannelEvent, ChannelId, Order, Side};
    use exec_jupiter::arm::{JupArm, JupArmCfg};
    use exec_jupiter::client::JupClient;
    use exec_jupiter::cmd::{JupEvent, JupIntent, JUP_CMD_RING, JUP_EVT_RING};
    use exec_jupiter::gw::{JupGateway, JupGwCfg, BODY_MIN, MIN_LEAD_NS_DEFAULT, POLL_EVERY_NS};
    use exec_jupiter::tx::{TxGuard, TOKEN_PROGRAM};
    use ingress_polymarket::gamma15::PmWindowSpec;
    use ingress_polymarket::pm15_loop::PM15_SPEC_RING;

    struct FixedJup {
        body: Vec<u8>,
        resp: Vec<u8>,
        build: Vec<u8>,
        exec: Vec<u8>,
        status: Vec<u8>,
        page: Vec<u8>,
    }
    impl JupClient for FixedJup {
        fn body_mut(&mut self) -> &mut [u8] {
            &mut self.body
        }
        fn request(
            &mut self,
            _m: Method,
            target: &[u8],
            _x: &[Header<'_>],
            _n: usize,
        ) -> Result<(u16, core::ops::Range<usize>), PostErr> {
            let src: &[u8] = if target == b"/prediction/v1/orders" {
                &self.build
            } else if target == b"/prediction/v1/execute" {
                &self.exec
            } else if target.starts_with(b"/prediction/v1/orders/status/") {
                &self.status
            } else {
                &self.page
            };
            self.resp[..src.len()].copy_from_slice(src);
            Ok((200, 0..src.len()))
        }
        fn resp(&self) -> &[u8] {
            &self.resp
        }
    }

    // Boot: the wallet (RFC 8032 TEST 1), gate 134's v0 order transaction,
    // its build answer, the execute / status / recon answers.
    let v = &signer_ed25519::RFC8032[0];
    let signer = signer_ed25519::Ed25519Signer::from_seed(&v.seed).expect("gate 135 key");
    let jup = [0x66u8; 32];
    let mut msg = vec![0x80u8, 1, 0, 3, 6];
    msg.extend_from_slice(&v.public);
    msg.extend_from_slice(&[0x33; 32]);
    msg.extend_from_slice(&[0x44; 32]);
    msg.extend_from_slice(&[0x55; 32]);
    msg.extend_from_slice(&jup);
    msg.extend_from_slice(&TOKEN_PROGRAM);
    msg.extend_from_slice(&[0xBB; 32]);
    msg.push(2);
    msg.extend_from_slice(&[4, 4, 0, 1, 2, 3, 8, 1, 2, 3, 4, 5, 6, 7, 8]);
    let mut data = vec![12u8];
    data.extend_from_slice(&5_000_000u64.to_le_bytes());
    data.push(6);
    msg.extend_from_slice(&[5, 4, 1, 3, 2, 0, data.len() as u8]);
    msg.extend_from_slice(&data);
    msg.push(0);
    let mut unsigned = vec![1u8];
    unsigned.extend_from_slice(&[0u8; 64]);
    unsigned.extend_from_slice(&msg);
    let b58 = |k: &[u8; 32]| {
        let mut o = [0u8; 44];
        let n = core_crypto::base58_encode(k, &mut o).unwrap();
        String::from_utf8(o[..n].to_vec()).unwrap()
    };
    let mut b64 = vec![0u8; core_crypto::base64_encoded_len(unsigned.len())];
    let n = core_crypto::base64_encode(&unsigned, &mut b64);
    let (wallet, order, position) = (b58(&v.public), b58(&[0x0A; 32]), b58(&[0x0B; 32]));
    let build = format!(
        r#"{{"transaction":"{}","txMeta":{{"blockhash":"x","lastValidBlockHeight":9}},"requiredSigners":["{wallet}"],"execution":{{"endpoint":"/execute","context":{{"type":"create_order","orderPubkey":"{order}"}}}},"order":{{"orderPubkey":"{order}","positionPubkey":"{position}","marketId":"POLY-5798888-1","isYes":true,"isBuy":true,"userPubkey":"{wallet}","maxBuyPriceUsd":"950000","estimatedTotalFeeUsd":"35000","orderCostUsd":"5000000","contractsMicro":"5263158"}}}}"#,
        std::str::from_utf8(&b64[..n]).unwrap()
    );
    let status = format!(r#"{{"orderPubkey":"{order}","status":"filled","latestEventType":"filled","history":[]}}"#);
    let page = format!(
        r#"{{"data":[{{"pubkey":"{order}","status":"filled","filledContracts":"5.263158","avgFillPriceUsd":"950000","settled":false}}],"pagination":{{"hasNext":false}}}}"#
    );
    let client = FixedJup {
        body: vec![0u8; BODY_MIN],
        resp: vec![0u8; 8_192],
        build: build.into_bytes(),
        exec: br#"{"status":"Success","signature":"5h6x","error":null}"#.to_vec(),
        status: status.into_bytes(),
        page: page.into_bytes(),
    };
    let mut g = TxGuard::refuse_all(v.public);
    assert!(g.allow_program(jup) && g.allow_program(TOKEN_PROGRAM));
    g.deposit_account = [0x33; 32];
    g.deposit_mint = [0x55; 32];
    let (eng, gwe) = exec_gateway::duplex::<JupIntent, JupEvent, JUP_CMD_RING, JUP_EVT_RING>();
    let (mut specs, specs_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let cfg = JupGwCfg {
        wallet: v.public,
        usdc_mint: [0x55; 32],
        guard: g,
        max_order_micro: 50_000_000,
        min_lead_ns: MIN_LEAD_NS_DEFAULT,
    };
    let mut gw = JupGateway::new(cfg, client, gwe, specs_rx, signer, b"gate-135-key").expect("jup-gw");
    gw.set_idle_schedule(u64::MAX, u64::MAX);
    let window = 5_974_360u32;
    let inst = pm5_instance_id(window, 0).unwrap();
    let mut spec = PmWindowSpec::EMPTY;
    spec.window = window;
    spec.market_id = 5_798_888;
    assert!(specs.try_push_ref(&spec));
    let mut arm = JupArm::new(
        JupArmCfg {
            slot: 11,
            live_families: 1,
            max_order_micro: 50_000_000,
        },
        eng,
    )
    .expect("the arm");
    let leg = pm5_rolling_sym(0, pm5_pair_of_window(window), 1);
    arm.on_venue_event(&ChannelEvent::new(0, VenueId::Polymarket, ChannelId::OutcomeMeta, leg, u64::from(inst), 0, 10_000, 5_000_000));
    let wall = pm5_window_end_ns(window) - 10_000_000_000;
    let mut now = 1_000_000_000u64;
    let mut fills = 0u64;
    let mut retired = 0u64;
    let mut cycle = |seq: u64, now: &mut u64, fills: &mut u64, retired: &mut u64| {
        let mut o = Order::new(
            1,
            VenueId::Jupiter,
            leg,
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(950_000),
            Qty::from_raw(5_263_158),
            (seq << 40) | (11 << 32) | u64::from(inst),
        );
        o.strategy_id = 11;
        arm.submit(&o).expect("admitted");
        gw.step(*now, wall);
        gw.step(*now + POLL_EVERY_NS, wall);
        gw.step(*now + POLL_EVERY_NS + 1, wall);
        *now += 2 * POLL_EVERY_NS;
        while let Some(f) = arm.try_next_fill() {
            *fills += f.qty.raw() as u64;
        }
        while arm.try_next_retired().is_some() {
            *retired += 1;
        }
        let _ = arm.on_idle();
        while arm.try_next_retired().is_some() {
            *retired += 1;
        }
    };
    let mut seq = 1u64;
    while seq <= 3 {
        cycle(seq, &mut now, &mut fills, &mut retired);
        seq += 1;
    }
    let guard_ = AllocGuard::new();
    while seq <= 15 {
        cycle(seq, &mut now, &mut fills, &mut retired);
        seq += 1;
    }
    let (allocs, bytes, _) = guard_.delta();
    let c = gw.counters();
    assert_eq!((c.landed, c.fills, c.done, c.refused), (15, 15, 15, 0), "gate 135: every order landed, filled, done");
    assert_eq!(fills, 15 * 5_263_158, "gate 135: every fill booked");
    assert_eq!(retired, 15, "gate 135: every order retired");
    assert_eq!(allocs, 0, "gate 135: jup-gw and the arm allocated {allocs} times ({bytes} B)");
}

/// PMJ J4 gate 136: slot 11's member on the 5-minute pool over the paper
/// matcher's 5-minute held-quote branch, in steady state — the report grid,
/// nowcast, pricer, persistence, the price floor, the $5 deposit sizing, the
/// take, the IoC held for the keeper's delay and judged at the quote in
/// force, its fill and the settlement. Zero bytes per operation.
#[test]
fn pmj_member_steady_state_is_zero_alloc() {
    use clob_dispatcher::PaperMatcher;
    use core_twaplock::late::LateLut;
    use core_types::pm_roll::{
        pm5_instance_id, pm5_mark_sym, pm5_pair_of_window, pm5_roll_family, pm5_rolling_sym,
        pm5_window_end_ns, pm5_window_of_open, PM_LEG_DOWN, PM_LEG_UP, PM_SETTLED_UP, PM_TWAP_S,
    };
    use core_types::{ChannelEvent, ChannelId, Order, OrderEvent, Price, Qty, Tick, VenueId};
    use strategy_core::{Ctx, Strategy, SubmitErr};

    /// The paper side: every order to the matcher, as slot 11's.
    struct PaperCtx<'a> {
        pm: &'a mut PaperMatcher,
        now: u64,
        n: &'a mut u64,
    }
    impl Ctx for PaperCtx<'_> {
        fn submit(&mut self, mut order: Order) -> Result<(), SubmitErr> {
            order.strategy_id = core_types::STRATEGY_SLOT_PMJ;
            self.pm.submit(&order, self.now);
            *self.n += 1;
            Ok(())
        }
        fn now_ns(&self) -> u64 {
            self.now
        }
    }

    const S: u64 = 1_000_000_000;
    const OPEN: u64 = 1_792_216_800;
    let w = pm5_window_of_open(OPEN).unwrap();

    // ---- boot: the member (configure, the grids) and the matcher ----
    let mut lut = Box::new(LateLut::flat());
    let mut z = 0usize;
    while z < core_twaplock::late::LATE_PHI_POINTS {
        lut.phi[z] = (500_000 + ((z as i64 * 2_000).min(4_000_000) / 8)) as u32;
        z += 1;
    }
    let perp = core_types::make_symbol_id(VenueId::Hyperliquid, 1);
    let mut perp_sym = [core_types::SYMBOL_ID_NONE; strategy_pmj::PMJ_MAX_FAMILIES];
    perp_sym[0] = perp;
    let p = strategy_pmj::PmjParams { n_families: 1, perp_sym, ..strategy_pmj::PmjParams::default() };
    let mut m = strategy_pmj::PmjStrategy::new();
    m.configure(p, lut, core_time::WallAnchor::new(0, 0)).expect("gate 136: configure");
    let mut pm = Box::new(PaperMatcher::new());
    pm.set_pm5_held_delay_ns(core_fill::held::PMJ_PAPER_DELAY_NS_DEFAULT);
    let roll = |wc: u32, settled: bool, v0: i64| {
        let pair = pm5_pair_of_window(wc);
        let inst = pm5_instance_id(wc, 0).unwrap();
        ChannelEvent::new(0, VenueId::Polymarket, ChannelId::InstrumentRoll, pm5_rolling_sym(0, pair, PM_LEG_UP), core_types::pack_roll_seq(inst, PM_TWAP_S, pm5_roll_family(0, pair), settled), 0, v0, pm5_window_end_ns(wc) as i64)
    };
    let law = |wc: u32, leg: u32| {
        let inst = pm5_instance_id(wc, 0).unwrap();
        ChannelEvent::new(0, VenueId::Polymarket, ChannelId::OutcomeMeta, pm5_rolling_sym(0, pm5_pair_of_window(wc), leg), u64::from(inst), 0, 10_000, 5_000_000)
    };
    let mut n = 0u64;
    let (mut fills, mut events) = (0u64, 0u64);
    let base: i64 = 81_000_000_000;
    let g = AllocGuard::new();
    let mut cycle = 0u32;
    // Four 5-minute windows six apart (one slot pair, 30 minutes between
    // opens), each with the 20 minutes of tape before it; the favourite's ask
    // (0.93) clears the 0.90 floor and holds through the keeper's delay.
    while cycle < 4 {
        let wc = w + 6 * cycle;
        let open = OPEN + u64::from(cycle) * 1_800;
        let end = open + 300;
        let pair = pm5_pair_of_window(wc);
        let mut s = open - 1_200;
        while s < end - 2 {
            if s == open {
                let mut c = PaperCtx { pm: &mut pm, now: open * S, n: &mut n };
                m.on_venue_event(&roll(wc, false, 0), &mut c);
                m.on_venue_event(&law(wc, PM_LEG_UP), &mut c);
                m.on_venue_event(&law(wc, PM_LEG_DOWN), &mut c);
            }
            let px = if s >= open { base + 400_000_000 } else { base } + if s % 2 == 0 { 2_000_000 } else { -2_000_000 };
            let t_perp = s * S - 200_000_000;
            {
                let mut c = PaperCtx { pm: &mut pm, now: t_perp, n: &mut n };
                m.on_tick(&Tick::new(t_perp, VenueId::Hyperliquid, perp, 1, Price::from_raw(px - 500_000), Qty::from_raw(1), Price::from_raw(px + 500_000), Qty::from_raw(1)), &mut c);
            }
            let now = s * S + 1_300_000_000;
            let up = Tick::new(now, VenueId::Polymarket, pm5_rolling_sym(0, pair, PM_LEG_UP), 1, Price::from_raw(920_000), Qty::from_raw(500_000_000), Price::from_raw(930_000), Qty::from_raw(500_000_000));
            let down = Tick::new(now, VenueId::Polymarket, pm5_rolling_sym(0, pair, PM_LEG_DOWN), 1, Price::from_raw(70_000), Qty::from_raw(500_000_000), Price::from_raw(80_000), Qty::from_raw(500_000_000));
            pm.observe_tick(&up, now);
            pm.observe_tick(&down, now);
            {
                let mut c = PaperCtx { pm: &mut pm, now, n: &mut n };
                m.on_venue_event(&ChannelEvent::new(now, VenueId::Polymarket, ChannelId::Mark, pm5_mark_sym(0), s, s * 1_000, px, px * 1_000_000), &mut c);
                m.on_tick(&up, &mut c);
                m.on_tick(&down, &mut c);
                m.on_timer(now, &mut c);
            }
            while let Some(f) = pm.try_next_fill() {
                fills += 1;
                let mut c = PaperCtx { pm: &mut pm, now, n: &mut n };
                m.on_fill(&f, &mut c);
            }
            let mut ev = OrderEvent::ZERO;
            while pm.try_next_order_event(&mut ev) {
                events += 1;
                let mut c = PaperCtx { pm: &mut pm, now, n: &mut n };
                m.on_order_event(&ev, &mut c);
            }
            s += 1;
        }
        let mut c = PaperCtx { pm: &mut pm, now: end * S, n: &mut n };
        m.on_venue_event(&roll(wc, true, PM_SETTLED_UP), &mut c);
        cycle += 1;
    }
    let (allocs, bytes, _) = g.delta();
    let ct = m.counters();
    assert!(n >= 4 && fills >= 4 && ct.settled_won == 4, "gate 136: the member took, the held law filled and the member settled every cycle ({n} orders, {fills} fills, {events} events, {ct:?})");
    assert_eq!(pm.held_pending(), 0, "gate 136: every held IoC judged");
    assert_eq!(allocs, 0, "gate 136: slot 11's member and the 5-minute held law allocated {allocs} times ({bytes} B)");
    std::hint::black_box(ct);
}
