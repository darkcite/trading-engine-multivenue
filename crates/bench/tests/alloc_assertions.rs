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
    parse_block_number_result, parse_new_head_notification, write_request_eth_block_number,
};

/// Push/pop a Tick through the SPSC ring 10_000 times — must not
/// allocate on any iteration after the initial ring construction.
#[test]
fn ring_push_pop_is_zero_alloc() {
    let ring: std::sync::Arc<Ring<Tick, 1024>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Prime the measurement window: ignore any boot-time allocs.
    let g = AllocGuard::new();

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
        prod.try_push(t).unwrap();
        let popped = cons.try_pop().unwrap();
        std::hint::black_box(popped);
    }

    let (allocs, bytes, deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "ring push/pop allocated {allocs} times ({bytes} B, {deallocs} deallocs)"
    );
    assert_eq!(bytes, 0, "ring push/pop bytes should be zero: saw {bytes}");
}

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

/// Parsing a sample Polymarket book frame 1000x must not allocate.
#[test]
fn book_parser_is_zero_alloc() {
    let buf: &[u8] = br#"[{"market":"0x60c2","asset_id":"0xabc","timestamp":"1713000000000","hash":"deadbeef","bids":[{"price":"0.517","size":"200.0"},{"price":"0.518","size":"100.0"}],"asks":[{"price":"0.521","size":"150.0"},{"price":"0.520","size":"50.0"}],"event_type":"book"}]"#;
    let g = AllocGuard::new();
    for _ in 0..1_000u32 {
        let t = ingress_polymarket::parse_book_update(buf, 1, 0);
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

/// Parse a Binance `@bookTicker` frame 10_000x — must be zero-alloc.
#[test]
fn binance_book_ticker_is_zero_alloc() {
    let buf: &[u8] =
        br#"{"u":400900217,"s":"BTCUSDT","b":"65000.01","B":"1.234","a":"65000.55","A":"0.987"}"#;
    let sym: SymbolId = 7;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        let t = parse_book_ticker(buf, sym).unwrap();
        acc = acc.wrapping_add(t.bid_px_1e6);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "parse_book_ticker allocated {allocs} times ({bytes} B)"
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
        let n = write_request_eth_block_number(&mut req, i).unwrap();
        acc = acc.wrapping_add(n as u64);
        let (id, block) = parse_block_number_result(resp).unwrap();
        acc = acc.wrapping_add(id).wrapping_add(block);
        let head = parse_new_head_notification(notif).unwrap();
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
        let t = cons.try_pop().expect("tick should be produced");
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

/// Drive the Binance ingress run-loop through 1 000 steady-state
/// frames via a `TestTransport`. The only non-zero-alloc work happens
/// at construction; every `drive_one` call must allocate zero bytes.
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
    let (mut otx, _orx) =
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
    let key = core_net::sec_websocket_key_from_seed(0xBA07u64);
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
        let t = cons.try_pop().expect("tick should be produced");
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
    let _ = cons.try_pop();

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
        if let Some(s) = cons.try_pop() {
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
    // (six tick lanes since WS9 added Bybit at lane 5; four fill
    // lanes); break the build loudly if that drifts.
    const _: () = assert!(NUM_TICK_LANES == 6 && NUM_FILL_LANES == 4);

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

    // Lane arrays: six tick lanes (Polymarket, Binance, OKX,
    // Deribit, Hyperliquid, Bybit — WS9) + four fill lanes. Only
    // lane 0 (Polymarket) gets a live producer here; the unused
    // producer halves stay alive until end of scope, and their lanes
    // simply read empty every iteration.
    let (mut pm_p, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t1p, t1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t2p, t2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t3p, t3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t4p, t4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t5p, t5) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    // WS10-A: six venue-event lanes ride in every engine. Lane 2
    // (OKX) gets a live producer — the measured window below pushes
    // one funding ChannelEvent per iteration and the engine drains
    // it through `on_venue_event`, proving lane push + drain are
    // 0 B/op; the other five read empty (two atomic loads each).
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
        [t0, t1, t2, t3, t4, t5],
        [e0, e1, e2, e3, e4, e5],
        [d0, d1],
        [o0, o1, o2],
        sc,
        [f0, f1, f2, f3],
        ai_c,
        std::sync::Arc::new(AiIngressStatus::new()),
        tbl_c,
    );
    eng.start().unwrap();

    // Prime + drain a few ticks outside the measurement window.
    for i in 0..16u32 {
        pm_p.try_push(Tick::new(
            i as u64,
            VenueId::Polymarket,
            1,
            i + 1,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        ))
        .unwrap();
    }
    eng.tick(64);

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    for i in 0..10_000u32 {
        // Push one tick + one funding event, drain both (WS10-A: the
        // event-lane push + `on_venue_event` drain ride the same
        // 0 B/op assertion).
        pm_p.try_push(Tick::new(
            (i as u64) * 1000,
            VenueId::Polymarket,
            1,
            i + 100,
            Price::from_raw(0),
            Qty::from_raw(0),
            Price::from_raw(0),
            Qty::from_raw(0),
        ))
        .unwrap();
        ev2_p
            .try_push(core_types::ChannelEvent::new(
                (i as u64) * 1000,
                VenueId::Okx,
                core_types::ChannelId::Funding,
                1,
                0,
                0,
                125,
                0,
            ))
            .unwrap();
        d0_p.try_push(core_types::DepthTopK::EMPTY).unwrap();
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
/// hot path (try_pop → inner.submit → atomic stats mirror) must
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
        let b = ingress_okx::parse_bbo(bbo, sym).unwrap();
        acc = acc.wrapping_add(b.bid_px_1e6);
        let t = ingress_okx::parse_trade(trade, sym).unwrap();
        acc = acc.wrapping_add(t.px_1e6);
        let m = ingress_okx::parse_mark_price(mark, sym).unwrap();
        acc = acc.wrapping_add(m.mark_px_1e6);
        let f = ingress_okx::parse_funding_rate(funding, sym).unwrap();
        acc = acc.wrapping_add(f.funding_rate_1e9);
        let h = ingress_okx::parse_book_header(book, sym).unwrap();
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
    // Drain the bbo ticks — one per cycle. try_pop is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop() {
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
        let q = ingress_deribit::parse_quote(quote, sym).unwrap();
        acc = acc.wrapping_add(q.bid_px_1e6);
        let k = ingress_deribit::parse_ticker(ticker, sym).unwrap();
        acc = acc.wrapping_add(k.mark_px_1e6);
        let t = ingress_deribit::parse_trade(trade_row, sym).unwrap();
        acc = acc.wrapping_add(t.px_1e6);
        let b = ingress_deribit::parse_book_header(book, sym).unwrap();
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
/// `opt-summary` row) + the `OptSummary` record construction —
/// live-shaped payloads for 10_000 iterations each, zero-alloc (the
/// hot ingress threads run these per push).
#[test]
fn option_analytics_parsers_are_zero_alloc() {
    let deribit_opt: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-27MAR26-100000-C.100ms","data":{"timestamp":1774000000123,"instrument_name":"BTC-27MAR26-100000-C","state":"open","mark_price":0.0523,"mark_iv":65.43,"greeks":{"delta":0.512,"gamma":1.234e-5,"vega":152.3,"theta":-85.3,"rho":12.1},"open_interest":1234.5,"index_price":77216.94,"underlying_price":77300.12}}}"#;
    let okx_row: &[u8] = br#"{"instType":"OPTION","instId":"BTC-USD-260327-100000-C","uly":"BTC-USD","deltaBS":"0.512","gammaBS":"1.234e-5","thetaBS":"-85.3","vegaBS":"152.3","markVol":"0.6543","fwdPx":"77300.12","ts":"1774598400123"}"#;
    let bn_combined: &[u8] = br#"{"stream":"btc-260327-100000-c@ticker","data":{"s":"BTC-260327-100000-C","bo":"2040.5","ao":"2060.1","bq":"1.25","aq":"0.75","d":"0.512","t":"-85.3","g":"0.0000123","v":"152.3","vo":"0.6543","mp":"2051.2"}}"#;
    let bn_index: &[u8] =
        br#"{"stream":"btcusdt@index","data":{"e":"index","s":"BTCUSDT","p":"77000.5"}}"#;
    let sym: SymbolId = (3 << 24) | 513;

    let g = AllocGuard::new();
    let mut acc: i64 = 0;
    for _ in 0..10_000u32 {
        let f = ingress_deribit::parse_option_ticker(deribit_opt).unwrap();
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
        // M2.4: the eapi combined splitter + ticker/index parsers.
        let (_, tail) = ingress_binance::eapi::split_combined(bn_combined).unwrap();
        let e = ingress_binance::eapi::parse_eapi_ticker(tail).unwrap();
        acc = acc.wrapping_add(e.mark_px_1e9);
        let (_, itail) = ingress_binance::eapi::split_combined(bn_index).unwrap();
        acc = acc.wrapping_add(ingress_binance::eapi::parse_eapi_index(itail).unwrap());
    }
    std::hint::black_box(acc);

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
    // Drain the quote ticks — one per cycle. try_pop is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop() {
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
        let b = ingress_hyperliquid::parse_bbo(bbo, sym).unwrap();
        acc = acc.wrapping_add(b.bid_px_1e6);
        let l = ingress_hyperliquid::parse_l2book_header(l2book, sym).unwrap();
        acc = acc.wrapping_add(l.best_bid_px_1e6 + l.n_bids as i64);
        let t = ingress_hyperliquid::parse_trade(trade, sym).unwrap();
        acc = acc.wrapping_add(t.px_1e6);
        let c = ingress_hyperliquid::parse_active_asset_ctx(ctx, sym).unwrap();
        acc = acc.wrapping_add(c.funding_1e9);
        let m = ingress_hyperliquid::parse_all_mids(mids).unwrap();
        acc = acc.wrapping_add(m as i64);
        let o = ingress_hyperliquid::parse_outcome_meta(outcome).unwrap();
        acc = acc.wrapping_add(o.enc as i64);
        std::hint::black_box(ingress_hyperliquid::parse_sub_response(subresp));
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "hl parsers allocated {allocs} times ({bytes} B)");
    assert_eq!(bytes, 0, "hl parser bytes should be zero: saw {bytes}");
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
    // Drain the bbo ticks — two per cycle. try_pop is zero-alloc
    // (asserted by the ring test above), so popping inside the guard
    // keeps the window honest.
    let mut acc: i64 = 0;
    let mut popped: usize = 0;
    while let Some(t) = cons.try_pop() {
        acc = acc.wrapping_add(t.bid_px.raw());
        popped += 1;
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    // Steady state consumed the whole script: one tick per bbo frame
    // (both coins — HIP-4 flows the same path), every ack verified,
    // every frame counted, no losses, no staleness trips.
    assert_eq!(popped, 2 * CYCLES);
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
/// shape check → seq policy → ts rewrite → capture → try_push — must
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
        let popped = cons.try_pop().unwrap();
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
/// member dispatch (ticks through a configured latency-arb member,
/// AI heartbeat fan-out, an Enable/Disable round trip) must allocate
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
    use strategy_set::{StrategySet, BIT_LATENCY_ARB, BIT_VM, SLOT_LATENCY_ARB};

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

    // Boot (allocation allowed): configure the latency-arb member
    // and commit a one-row vm table on the same (PM=11, BN=22) pair.
    // Clock is production-like (G3 lesson: fresh cooldown stamps arm
    // only once `now ≥ horizon_ns`).
    let mut set = StrategySet::new(BIT_LATENCY_ARB | BIT_VM);
    set.latency_arb_mut().add_pair(11, 22).unwrap();
    set.latency_arb_mut().set_cooldown_ns(0);
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
        SLOT_LATENCY_ARB,
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
        SLOT_LATENCY_ARB,
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
        ctx.submitted >= 2 * u64::from(CYCLES),
        "latency-arb and the vm row must both fire every cycle"
    );
    assert_eq!(set.enabled_mask(), BIT_LATENCY_ARB | BIT_VM);
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
    assert!(prod.try_push(*table).is_ok());
    let warm = cons.try_pop().expect("prewarm pop");
    vm.receive_table_v2(&warm);
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
        ok_pushes += u32::from(prod.try_push(*table).is_ok());
        table.epoch = 2 * i + 2;
        ok_pushes += u32::from(prod.try_push(*table).is_ok());
        // … the third is the §5 push-full reject path.
        full_rejects += u32::from(prod.try_push(*table).is_err());
        while let Some(t) = cons.try_pop() {
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
            self.prod.try_push(order).map_err(|_| SubmitErr::RingFull)
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
        while let Some(o) = cons.try_pop() {
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
        while let Some(o) = cons.try_pop() {
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
            self.prod.try_push(order).map_err(|_| SubmitErr::RingFull)
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
        while let Some(o) = cons.try_pop() {
            std::hint::black_box(o.client_oid);
        }
        i += 1;
    }
    let warm = s.counters().intents;
    assert!(warm > 0, "prewarm must enter");
    let g = AllocGuard::new();
    while i < 120 + 40 * 300 {
        s.on_tick(&script_tick(i, t0, foreign), &mut ctx);
        while let Some(o) = cons.try_pop() {
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
            self.prod.try_push(order).map_err(|_| SubmitErr::RingFull)
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
        while let Some(o) = cons.try_pop() {
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
        while let Some(o) = cons.try_pop() {
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
