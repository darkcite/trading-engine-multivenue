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
    symbols.insert(b"BTC-USDT", sym).unwrap();
    let mut driver = owl::Driver::new(0x0C0Cu64, symbols, true);
    owl::note_transport_ready(&mut driver, core_net::Status::Ready);
    // Health telemetry sink — relaxed atomics only; built outside
    // the measurement window.
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { owl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Send the client GET handshake.
    owl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    owl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
        owl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
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
    let test_req: &[u8] = br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
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
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { dwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Send the client GET handshake.
    dwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    dwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    dwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    const TEST_REQ: &[u8] = br#"{"jsonrpc":"2.0","method":"heartbeat","params":{"type":"test_request"}}"#;
    const BOOK_SNAP: &[u8] = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"timestamp":1554373962454,"instrument_name":"BTC-PERPETUAL","change_id":1000,"bids":[["new",5042.34,30.0]],"asks":[["new",5042.64,40.0]],"type":"snapshot"}}}"#;

    let mut stream: Vec<u8> = Vec::with_capacity(400 * 1024);
    push_text_frame(&mut stream, BOOK_SNAP);
    let mut change_id: i64 = 1_000; // BOOK_SNAP's change_id — chain root
    let mut trade_seq: i64 = 50_000;
    let mut test_reqs: u64 = 0;
    for c in 0..CYCLES {
        push_text_frame(&mut stream, QUOTE);
        let trade = format!(
            r#"{{"jsonrpc":"2.0","method":"subscription","params":{{"channel":"trades.BTC-PERPETUAL.100ms","data":[{{"trade_seq":{},"trade_id":"9","timestamp":1000,"price":8950.0,"direction":"buy","amount":10.0}}]}}}}"#,
            trade_seq
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
        dwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
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
    assert_eq!(bytes, 0, "deribit run-loop bytes should be zero: saw {bytes}");
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
    assert_eq!(
        allocs, 0,
        "hl parsers allocated {allocs} times ({bytes} B)"
    );
    assert_eq!(bytes, 0, "hl parser bytes should be zero: saw {bytes}");
}

/// Drive the Hyperliquid ingress run-loop through 1 000+
/// pre-injected steady-state frames (all 9 subscriptionResponse
/// acks — verification + staleness arming happen **inside** the
/// window via `session_health` — then cycles of bbo / l2Book /
/// trades across a perp and a HIP-4 `#<enc>` coin, plus WS protocol
/// Pings whose pong replies render inside the window) via a
/// `TestTransport`. Steady state is reached over the real handshake
/// + per-sub subscribe path; every `drive_one` call must allocate
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
    let status = core_metrics::IngressStatus::new();

    let ring: std::sync::Arc<Ring<Tick, { hwl::TICK_RING_CAP }>> = Ring::new();
    let (mut prod, mut cons) = ring.split();

    // Send the client GET handshake.
    hwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
    hwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
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
        hwl::drive_one(&mut transport, &mut driver, b"h", b"/", &mut prod, &status).unwrap();
        drives += 1;
        assert!(drives <= 4_096, "scripted stream failed to drain");
    }
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
}
