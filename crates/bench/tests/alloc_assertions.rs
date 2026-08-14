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
use ingress_binance::parse_book_ticker;
use ingress_polymarket::run_loop::{
    drive_one, note_transport_ready, Driver, State, SymbolMap, DEFAULT_TICK_RING_CAP,
};
use ingress_rpc::{
    parse_block_number_result, parse_new_head_notification, write_request_eth_block_number,
};
use ingress_rss::{feed_items, fnv1a_64, SeenRing};

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
    assert_eq!(allocs, 0, "price scanner allocated {allocs} times ({bytes} B)");
}

/// Parsing a sample Polymarket book frame 1000x must not allocate.
#[test]
fn book_parser_is_zero_alloc() {
    let buf: &[u8] = br#"{"event_type":"book","asset_id":"0xabc","timestamp":"1713000000000","hash":"deadbeef","bids":[["0.518","100.0"]],"asks":[["0.520","50.0"]]}"#;
    let g = AllocGuard::new();
    for _ in 0..1_000u32 {
        let t = ingress_polymarket::parse_book_update(buf, 1, 0);
        std::hint::black_box(t);
    }
    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(allocs, 0, "book parser allocated {allocs} times ({bytes} B)");
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
    assert_eq!(bytes, 0, "ws_frame roundtrip bytes should be zero: saw {bytes}");
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
    assert_eq!(
        allocs, 0,
        "rpc codecs allocated {allocs} times ({bytes} B)"
    );
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

    let mut driver = Driver::new(0xDEAD_BEEFu64);
    note_transport_ready(&mut driver, core_net::Status::Ready);
    // Ingress health telemetry sink (Phase 8a). Its bumps are relaxed
    // atomics and allocate nothing, but construct it outside the
    // measurement window on principle: setup is never measured.
    let status = core_metrics::IngressStatus::new();
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
    )
    .unwrap();
    assert_eq!(driver.state(), State::Steady);

    // Preloaded unmasked Text frame containing a Polymarket book update.
    // Header: FIN | opcode=Text(0x1) → 0x81, len=<123 (fits short).
    let payload: &[u8] = br#"{"event_type":"book","asset_id":"0xABC","timestamp":"1713000000000","bids":[["0.518","100"]],"asks":[["0.520","50"]]}"#;
    assert!(payload.len() <= 125);
    let mut frame = [0u8; 256];
    frame[0] = 0x81;
    frame[1] = payload.len() as u8;
    frame[2..2 + payload.len()].copy_from_slice(payload);
    let frame_len = 2 + payload.len();

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
        )
        .unwrap();

        // Drain the tick so the ring doesn't fill.
        let t = cons.try_pop().expect("tick should be produced");
        acc = acc.wrapping_add(t.bid_px.raw());
    }
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
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { bwl::DEFAULT_TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Send the client GET handshake.
    bwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    bwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
        bwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
        let t = cons.try_pop().expect("tick should be produced");
        acc = acc.wrapping_add(t.bid_px.raw());
    }
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

    rwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    rwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
        rwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
        // Drain the Signal so the ring doesn't fill.
        if let Some(s) = cons.try_pop() {
            acc = acc.wrapping_add(u64::from_le_bytes(s.payload[0..8].try_into().unwrap()));
        }
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "rpc run-loop allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "rpc run-loop bytes should be zero: saw {bytes}");
}

/// Parse an RSS body through `parse_body_into_signals` 1 000 times —
/// must be zero-alloc after the SeenRing is primed.
#[test]
fn rss_poller_body_parse_is_zero_alloc() {
    use ingress_rss::poller as rsp;

    const BODY: &[u8] = br#"<rss><channel>
<item><title>one</title><link>https://a.example/1</link></item>
<item><title>two</title><link>https://a.example/2</link></item>
</channel></rss>"#;

    let feed = rsp::FeedCfg::new(b"a.example", b"/feed", 60_000_000_000);
    let mut seen: SeenRing<64> = SeenRing::new();
    let ring: std::sync::Arc<Ring<core_types::Signal, 128>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Prime the seen-ring so every subsequent loop iteration dedupes
    // rather than emits — keeps the measurement focused on parse cost.
    let _ = rsp::parse_body_into_signals(BODY, &feed, &mut seen, &mut prod);
    while cons.try_pop().is_some() {}

    let g = AllocGuard::new();
    let mut acc: usize = 0;
    for _ in 0..1_000u32 {
        let emitted = rsp::parse_body_into_signals(BODY, &feed, &mut seen, &mut prod);
        acc = acc.wrapping_add(emitted);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "rss poller parse_body_into_signals allocated {allocs} times ({bytes} B)"
    );
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
// Phase 4: metrics + latency + tui hot-path assertions
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

/// 10 000 snapshot publish + read round-trips through SnapshotCell
/// must be zero-alloc. The cell is a single-writer seqlock
/// (version counter + POD slot) — no OS lock object exists at all.
/// History: the v1 `Mutex<DashboardState>` cell failed this
/// assertion on macOS, where std's pthread-backed `Mutex` lazily
/// heap-allocates its 64-byte `pthread_mutex_t` on first lock
/// (Linux's futex `Mutex` never allocates, so only the Mac gate
/// caught it). The seqlock is allocation-free by construction on
/// every platform.
#[test]
fn dashboard_snapshot_read_is_zero_alloc() {
    use tui::{DashboardState, SnapshotCell};
    let cell = SnapshotCell::new();
    let mut state = DashboardState::empty();

    let g = AllocGuard::new();
    let mut acc: u64 = 0;
    for i in 0..10_000u64 {
        state.iterations = i;
        cell.publish(state);
        let got = cell.read();
        acc = acc.wrapping_add(got.iterations);
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "SnapshotCell publish+read allocated {allocs} times ({bytes} B)"
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

    // The lane arrays below are written out for the Phase 8a
    // geometry (five tick lanes in VenueId order, four fill lanes);
    // break the build loudly if that drifts.
    const _: () = assert!(NUM_TICK_LANES == 5 && NUM_FILL_LANES == 4);

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

    // Phase 8a lane arrays: five tick lanes (VenueId order:
    // Polymarket, Binance, OKX, Deribit, Hyperliquid) + four fill
    // lanes. Only lane 0 (Polymarket) gets a live producer here;
    // the unused producer halves stay alive until end of scope, and
    // their lanes simply read empty every iteration.
    let (mut pm_p, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t1p, t1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t2p, t2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t3p, t3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t4p, t4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_sp, sc) = Ring::<core_types::Signal, SIGNAL_RING_SIZE>::new().split();
    let (_f0p, f0) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f1p, f1) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f2p, f2) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f3p, f3) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();

    let mut eng = Engine::new(
        NoopStrat,
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4],
        sc,
        [f0, f1, f2, f3],
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
        // Push one tick + drain one.
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
        eng.tick(1);
        acc = acc.wrapping_add(eng.ingest_p50_ns());
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "engine.tick() with latency record allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0);
}

/// Exercise the RSS iterator + FNV-1a + SeenRing dedupe for 1000
/// iterations over a two-item feed. Must be zero-alloc.
#[test]
fn rss_items_and_fnv_is_zero_alloc() {
    let buf: &[u8] = br#"<rss><channel>
<item><title>one</title><link>https://a.example/1</link></item>
<item><title>two</title><link>https://a.example/2</link></item>
</channel></rss>"#;

    let mut seen: SeenRing<64> = SeenRing::new();

    let g = AllocGuard::new();

    let mut acc: u64 = 0;
    for _ in 0..1_000u32 {
        // Iterator is stack-allocated; holds only (&buf, pos).
        // `for` over an iterator monomorphises to the same tight loop as
        // `while let Some(..) = iter.next()` and doesn't allocate.
        for item in feed_items(buf) {
            let h = fnv1a_64(item.link);
            let _inserted = seen.insert(h);
            acc = acc.wrapping_add(h);
        }
    }
    std::hint::black_box(acc);

    let (allocs, bytes, _deallocs) = g.delta();
    assert_eq!(
        allocs, 0,
        "rss + fnv + SeenRing allocated {allocs} times ({bytes} B)"
    );
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
