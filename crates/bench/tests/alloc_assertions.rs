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
    ws_mask_from_counter, ws_read_frame, ws_unmask_in_place, ws_write_text_frame, TestTransport,
    WsReadResult,
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
    let row: &[u8] = br#"{"asset_id":"0xabc","price":"0.518","size":"642.77","side":"BUY","hash":"d0c1","best_bid":"0.518","best_ask":"0.520"}"#;
    let g = AllocGuard::new();
    for _ in 0..1_000u32 {
        let mut t = core_types::Tick::ZERO;
        assert!(ingress_polymarket::parse_book_update(buf, 1, 0, &mut t));
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
/// times through the preallocated tx/rx buffers. The full core-net
/// codec path must not allocate.
#[test]
fn ws_frame_roundtrip_is_zero_alloc() {
    // Preallocated tx/rx buffers — single allocation each, outside the
    // measurement window.
    let mut tx = [0u8; 256];
    let mut rx = [0u8; 256];
    let payload: &[u8] = b"{\"u\":12345,\"s\":\"BTCUSDT\"}";

    let g = AllocGuard::new();

    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        let mask = ws_mask_from_counter(i);
        let n = ws_write_text_frame(&mut tx, payload, mask).unwrap();
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
    let (mut etx, _erx) = event_ring.split();
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
    let mut odriver = bwl::Driver::new_eapi(0xBA08u64, table);
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
    use clob_dispatcher::PaperDispatcher;
    use engine::{
        Engine, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES, SIGNAL_RING_SIZE, TICK_RING_SIZE,
    };
    use strategy_core::{Ctx, Strategy, StrategyCounters, StrategyError, SubmitErr};

    // The lane arrays below are written out for the lane geometry
    // (eight tick lanes since HC1 added Hypercall at lane 7, after
    // MX2's MEXC at lane 6 and WS9's Bybit at lane 5; four opt lanes
    // since HC1; four fill lanes); break the build loudly if that
    // drifts.
    const _: () = assert!(NUM_TICK_LANES == 8 && engine::NUM_OPT_LANES == 4 && NUM_FILL_LANES == 4);

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
    // + four fill lanes. Only
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

    let mut eng = Engine::new(
        NoopStrat,
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4, t5, t6, t7],
        [e0, e1, e2, e3, e4, e5, e6, e7],
        [d0, d1],
        [o0, o1, o2, o3],
        sc,
        [f0, f1, f2, f3],
        ai_c,
        std::sync::Arc::new(AiIngressStatus::new()),
        tbl_c,
    );
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
        eng.tick(1);
        acc = acc.wrapping_add(eng.ingest_p50_ns());
    }
    std::hint::black_box(acc);
    assert_eq!(eng.events_dispatched, 10_000, "event lane drained");
    assert_eq!(eng.depths_dispatched, 10_000, "depth lane drained");

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "engine.tick() with latency record allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
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
        let spec = ingress_hyperliquid::discovery::parse_outcome_spec(id, desc);
        acc = acc.wrapping_add(spec.strike_1e6 ^ spec.expiry_ns as i64);
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
    const CYCLES: usize = 199; // 9 acks + 5 × 199 = 1 004 frames
    const BBO_BTC: &[u8] = br#"{"channel":"bbo","data":{"coin":"BTC","time":1708622398623,"bbo":[{"px":"64437.0","sz":"1.4491","n":2},{"px":"64438.0","sz":"0.541","n":3}]}}"#;
    const L2_BTC: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1677700000000,"levels":[[{"px":"19900.0","sz":"1.0","n":1},{"px":"19899.0","sz":"2.5","n":2}],[{"px":"20100.0","sz":"1.0","n":1}]]}}"#;
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

    let mut stream: Vec<u8> = Vec::with_capacity(256 * 1024);
    for ack in ACKS {
        push_text_frame(&mut stream, ack);
    }
    for _ in 0..CYCLES {
        push_text_frame(&mut stream, BBO_BTC);
        push_text_frame(&mut stream, L2_BTC);
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
    // `bbo` one-sided. `L2_BTC` still yields no tick — a perp's touch
    // comes from bbo alone, so no perp number moved.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop_ref().as_deref().copied() {
        acc = acc.wrapping_add(t.bid_px.raw());
        popped += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // Steady state consumed the whole script: one tick per bbo frame
    // (both coins — HIP-4 flows the same path) plus the HIP-4
    // `l2Book` touch (BIN15 O8), every ack verified, every frame
    // counted, no losses, no staleness trips.
    assert_eq!(popped, 3 * CYCLES);
    assert!(driver.is_verified());
    // 9 acks + per cycle: bbo(1) + l2Book(1) + trades rows(2) +
    // bbo(1) + l2Book(1) = 6. WS Pings are activity, not messages.
    assert_eq!(status.msgs_total(), (9 + 6 * CYCLES) as u64);
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

/// ICDP I3 gate (40): the slot-6 strategy's whole tick path — foreign
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
/// path — a FULL `EngineSnapshot` (256 vm rows, 64 + 64 recents, every
/// text field at capacity) published into the seqlock, read back into
/// the server thread's scratch and encoded as JSON into a 256 KiB
/// response buffer, 1 000 times — allocates nothing. Truncation is a
/// test failure (the encoder refuses, never truncates).
#[test]
fn state_snapshot_publish_read_encode_is_zero_alloc() {
    use core_types::{Fill, Order, Price, Qty, Side, VenueId, RULE_TABLE_ROWS};
    use engine_snapshot::{
        encode_state_json, EngineSnapshot, SnapshotCell, RECENT_FILLS, RECENT_ORDERS,
        RUN_DIR_MAX,
    };
    use strategy_core::VmRowView;

    // Boot-time construction (allocation sanctioned): the cell, the
    // engine-side scratch, the server-side scratch, the response buf.
    let cell = SnapshotCell::new(EngineSnapshot::empty());
    let mut scratch = Box::new(EngineSnapshot::empty());
    scratch.boot.set_git_sha(&[b'f'; 48]);
    scratch.boot.set_run_dir(&[b'r'; RUN_DIR_MAX]);
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

/// Gate 52 (BIN15 O4b, spec §6.4): the bin15 member's live callbacks at
/// full width — eight families over four underlyings, 100 rolls, 10 000
/// `Mark` events with their per-minute forecast refresh, both binary
/// legs ticking, both arms armed, the 1 s timer sweeping pendings, and
/// every order it emits coming straight back as a fill.
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
/// for the arms' candidate list, a `format!` in a counter's name, a
/// `Box` for a pending — would be invisible at the call site and fatal
/// at 4 marks a second per coin.
#[test]
fn bin15_member_roll_tick_reprice_take_is_zero_alloc() {
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
        last: Option<Order>,
        now: u64,
    }
    impl Ctx for SinkCtx {
        fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
            self.n += 1;
            self.last = Some(order);
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
    // A daily family's floors are an hour, which no gate window reaches;
    // shrink them so one tape exercises both kinds' arms.
    params.tau_min_take_ns = [60_000_000_000, 60_000_000_000];
    params.tau_min_quote_ns = [120_000_000_000, 120_000_000_000];
    params.maker_enabled = 1;
    params.null_arm = 1;
    params.cap_instance_usd_1e6 = 1_000_000_000;
    params.cap_day_usd_1e6 = 1_000_000_000_000;
    // BIN15 S5: the coverage entry ON, with its persistence gate and its
    // elapsed ceiling, so the run, the counterfactual, a price refusal and
    // (in the S3 phase below) the ceiling's close are all under the guard.
    // The legs tick every 10 s and a failing Yes book lands at 5 s, so
    // each 15 m instance enters on its third passing snapshot, 30 s in.
    params.entry_usd_1e6 = 50_000_000;
    params.entry_persist_polls = 3;
    params.entry_elapsed_max_ns = 240_000_000_000;

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

    let mut ctx = SinkCtx {
        n: 0,
        last: None,
        now: at(0),
    };
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
        // a resident ask at 0.40 is always takeable and the arms run
        // every time rather than only when the walk happens to cross.
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
            // Both legs tick every tenth second; both must be
            // actionable before either arm fires.
            if k % 10 == 0 {
                m.on_tick(&leg_tick(yes_sym(fam) + 1, ts, 390_000, 600_000), &mut ctx);
                m.on_tick(&leg_tick(yes_sym(fam), ts, 300_000, 400_000), &mut ctx);
                // Every order comes straight back as a full fill, so
                // `book_fill` — which moves the position and clears the
                // pending — is inside the guard too.
                if let Some(o) = ctx.last.take() {
                    let fill =
                        core_types::Fill::new(ts, o.sym, o.side, o.px, o.qty, o.client_oid);
                    m.on_fill(&fill, &mut ctx);
                }
                m.on_timer(ts, &mut ctx);
            }
            // BIN15 S5: once per instance a Yes ask the entry's bound
            // refuses — a failing snapshot — so the gate's price refusal
            // and a broken run are under the guard too.
            if k == 5 {
                m.on_tick(&leg_tick(yes_sym(fam), ts, 980_000, 990_000), &mut ctx);
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
            // BIN15 S5: this instance is bound 830 s into its life, past
            // the ceiling, so its first coverage reprice CLOSES it.
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
    assert!(counters.takes_submitted > 0, "the gate must measure real takes");
    assert!(counters.takes_filled > 0, "and real fills");
    assert!(counters.quotes_submitted > 0, "and Arm B");
    assert!(counters.skipped_entry_persist > 0, "and the S5 persistence gate");
    assert!(counters.skipped_entry_price > 0, "and its price refusal");
    assert_eq!(counters.skipped_entry_elapsed, 1, "and the S5 ceiling's close, once");
    // BIN15 S5b: both counterfactuals were stamped under the guard. The last
    // roll's instance (family 3 — the S3 phase rebinds family 0 only) passed
    // both tests on its first Yes snapshot, at its own bind second.
    let last = ROLLS - 1;
    assert_ne!(last % BIN15_MAX_FAMILIES, 0, "the S3 phase rebinds family 0");
    let f_last = *m.family(last % BIN15_MAX_FAMILIES).expect("the last roll's family");
    let t_last = at(1 + last as u64 * MARKS_PER_ROLL);
    assert_eq!(
        (f_last.entry_first_ok_ts, f_last.entry_ctl_ok_ts),
        (t_last, t_last),
        "the artifact's first fire and the control"
    );
    assert!(ctx.n > 0, "the gate must measure real submits");
    assert_eq!(
        allocs, 0,
        "strategy-bin15 callbacks allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "strategy-bin15 hot bytes should be zero: saw {bytes}");
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
/// `HlHttp::post` cannot be driven here without a server, and the TLS
/// loopback test covers its behaviour. What CAN be pinned — and what
/// actually runs per order — is the pair either side of the socket:
/// building the signed JSON body, and scanning the venue's answer.
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
        encode_cancel, encode_order, encode_reserve_weight, CancelWire, OrderWire, Tif, MAX_ACTION,
    };
    use exec_hyperliquid::request::{cancel_json, envelope, order_json, reserve_weight_json};
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
    let mut bought = [0i64; 8];
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
/// * the halt EDGE itself: latch, `exec.HALT` write, and the
///   venue-wide cancel request;
/// * `cancel_all_state` polling through `Working` to `Clear`, which
///   is where `clear_resting` walks the table;
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

/// **HC2 gate 73a — the generic HTTP/1.1 head writer is 0 B/op.** Every
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

/// **HC2 gate 73b — `HttpsReq`'s keep-alive cycle, as allocations per
/// request: exactly gate 72's two.** `HttpsReq` renders its head per
/// request (any method, any target) flush against the body already in
/// place and writes ONE contiguous slice ONCE, over the same connection
/// engine as `HttpsPost` (`core_net::https_conn`). rustls' buffered API
/// seals one record out and decrypts one in; a regression in our render
/// or in the shared engine shows as a third. Same child-process server
/// as gate 72 (the counting allocator is process-global).
#[test]
fn https_req_keep_alive_cycle_allocates_only_rustls_record_buffers() {
    const REQS: u64 = 500;
    const RUSTLS_ALLOCS_PER_REQ: u64 = 2;
    let dir = std::env::temp_dir().join(format!("mv-gate73-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("gate 73 dir");
    let mut child = std::process::Command::new(std::env::current_exe().expect("gate 73 exe"))
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
        .expect("gate 73 child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let port: u16 = loop {
        if let Ok(s) = std::fs::read_to_string(dir.join("port")) {
            break s.trim().parse().expect("gate 73 port");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate 73: the node never came up"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let der = std::fs::read(dir.join("cert.der")).expect("gate 73 cert");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .expect("gate 73 anchor");
    let cfg = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut h = core_net::HttpsReq::new("localhost", port, cfg, 1024, 8192, 16 * 1024)
        .expect("gate 73 client");
    let n = exec_hyperevm::rpc::write_chain_id(h.body_mut(), 1).expect("gate 73 body");
    let extra: [core_net::Header<'_>; 1] = [(b"X-Gate", b"73")];
    let mut i = 0;
    while i < 50 {
        // The handshake, the session tickets and rustls' queues growing
        // to their working size: the cold part.
        let (status, _) = h
            .request(core_net::Method::Post, b"/evm", &extra, n)
            .expect("gate 73 warm-up");
        assert_eq!(status, 200);
        i += 1;
    }

    let g = AllocGuard::new();
    let mut acc = 0u64;
    let mut k = 0u64;
    while k < REQS {
        let (status, r) = h
            .request(core_net::Method::Post, b"/evm", &extra, n)
            .expect("gate 73 request");
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
// HC3: Hypercall ingress hot-path assertions (gates 74, 75)
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

/// **HC3 gate 74 — every Hypercall parser is 0 B/op.** Classify over
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
        let meta = hc::parse_indicative(HC_Q2X, &mut q).expect("gate 74 quote");
        acc = acc.wrapping_add(q.bid_px_1e6 - q.ask_px_1e6 + i64::from(meta.num_providers));
        let (row, sym) = table.lookup(hc::span_bytes(HC_Q2X, q.instrument)).expect("gate 74 lookup");
        acc = acc.wrapping_add(row as i64 + i64::from(sym));
        let mut ps = [hc::HcProvider::default(); hc::HC_MAX_PROVIDERS];
        let (read, present) = hc::walk_providers(HC_Q2X, q.providers, &mut ps).expect("gate 74 providers");
        acc = acc.wrapping_add(ps[1].ask_px_1e6 + i64::from(read) + i64::from(present));
        acc = acc.wrapping_add(hc::provider_quote_seq(1, true, meta.num_providers, ps[1].wallet_lo32) as i64);
        let mut q1 = hc::HcQuote::ZERO;
        assert_eq!(hc::parse_indicative(HC_Q1, &mut q1).map(|m| m.sides), Some(hc::SIDE_BID | hc::SIDE_ASK));
        let mut q0 = hc::HcQuote::ZERO;
        assert_eq!(hc::parse_indicative(HC_Q0, &mut q0).map(|m| m.sides), Some(0));
        // The index frame, every entry in place.
        let mut xs = [hc::HcIndexEntry::default(); hc::HC_MAX_UNDERLYINGS];
        let (n, all, ts) = hc::parse_index_update(HC_IDX, &mut xs).expect("gate 74 index");
        acc = acc.wrapping_add(xs[3].price_1e6 + i64::from(n) + i64::from(all) + ts as i64);
        // Trades, listings, the clock, errors, the close reason.
        let t = hc::parse_trade(HC_TRADE).expect("gate 74 trade");
        acc = acc.wrapping_add(t.px_1e6 + t.signed_qty_1e6);
        let (action, name, ts) = hc::parse_market_update(HC_MU_EXPIRED).expect("gate 74 listing");
        acc = acc.wrapping_add(
            i64::from(action == hc::HcListingAction::Expired) + hc::span_bytes(HC_MU_EXPIRED, name).len() as i64 + ts as i64,
        );
        let (nonce, server_at) = hc::parse_clock_synced(HC_CLOCK).expect("gate 74 clock");
        acc = acc.wrapping_add((nonce ^ server_at) as i64);
        acc = acc.wrapping_add(hc::parse_error(error_frame).map_or(-1, |s| i64::from(s.1 - s.0)));
        acc = acc.wrapping_add(hc::parse_close_reason(HC_CLOSE_ML) as i64);
        // The outbound side: the universe subscribe as parts, a nonce.
        let n = hc::subscribe_parts(hc::CH_INDICATIVE, Some(&table), &mut parts).expect("gate 74 parts");
        acc = acc.wrapping_add(n as i64 + parts[n - 1].len() as i64);
        let mut digits = [0u8; 20];
        let d = hc::fmt_u64(1_790_373_478_201 + i, &mut digits);
        acc = acc.wrapping_add(hc::clock_sync_parts(d)[1].len() as i64);
        // The poller's scan: REST rows → `OptSummary`.
        let rows = hc::rest::parse_summary_rows(HC_SUMMARY, |r| {
            acc = acc.wrapping_add(hc::rest::to_opt_summary(i, (9 << 24) | 515, r).mark_iv_1e9);
        })
        .expect("gate 74 summary");
        acc = acc.wrapping_add(rows as i64);
        i += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hypercall parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hypercall parser bytes should be zero: saw {bytes}");
}

/// **HC3 gate 75 — the Hypercall run loop in steady state is 0 B/op.**
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

/// **HC7 gate 76 — the settlement replicator is 0 B/op.** A 30-minute
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

/// **HAR H1 gate 77 — the long-tenor engine is 0 B/op.** Sixty UTC days
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
