// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **T2T T0-C — the order path's stage harness** (plan
//! `t2t-plan-2026-10-25`, T0 item 3). Real code, stage by stage, on this
//! machine, warm and cold:
//!
//! * `parse/<channel>` — the Hyperliquid ingress's classify and parse of
//!   recorded public-WS payloads (`T2T_FRAMES`: a `T2TF` file in the vault,
//!   written by `research/t2t/record_frames.py`; the stage is skipped when
//!   unset). The payload is already decrypted and deframed: the live
//!   `engine_latency_parse_*` gauges carry the read turn whole.
//! * `sign` — the gateway's signature: `sign_action` on an encoded order
//!   action (keccak connection id, EIP-712 agent digest, RFC 6979 ECDSA).
//! * `sign/prenonce` — the same through T1-c's precomputed-nonce signer
//!   (`sign_action_prenonce`), its pool refilled between triggers.
//! * `route` — the exec router's live submit of a bin15 take (every clamp,
//!   a full ledger table) into an accepting arm.
//! * `gateway/inline` — the real HL gateway against the scripted loopback
//!   venue (`exec_hyperliquid::testvenue`), turned on this thread with a
//!   zero park: an IoC place pushed → the turn that wrote its frame
//!   returns (admit, msgpack, sign, JSON, WS frame, rustls, `write(2)`).
//! * `gateway/spinning`, `gateway/parked` — the same gateway on its own
//!   thread under the production idle policy (1 000 non-blocking polls
//!   after activity, then a 100 µs park): push → this thread sees the
//!   gateway's sent counter move, right after the socket took the frame
//!   (plus one cross-core load). One place every 2 ms finds it still
//!   spinning; one every 25 ms finds it parked, as a live order does (one
//!   an hour), so the wake-up is in. Each also reports the gateway's own
//!   `push_to_write` (bucket bounds) as a cross-check.
//! * `gateway/hot`, `gateway/hot+qos` — the parked case under a hot window
//!   that never closes (T1-a): one place every 25 ms, the gateway kept
//!   awake; then with T1-b's user-interactive QoS on its thread, as
//!   production's `spawn_gateway` sets it. The pool is not refilled inside
//!   a window (T1-c), so each gateway takes [`HOT_PER_BOOT`] orders (its
//!   warm-up's included) and a fresh one, its pool full as each live
//!   window opens, takes the next: [`HOT_BOOTS`] gateways a row.
//! * `gateway/parked`, `gateway/hot` **cold** — the caches evicted before
//!   each take's 25 ms gap, the gap then left to run: in the hot window the
//!   keep-hot dry run (T1-f) re-warms the order path inside it; parked,
//!   nothing does. A live take lands on that state.
//! * `wake/*` — T1-b: a thread woken by one loopback byte, blocked in
//!   `poll` (the HL ingress today) or spinning, with and without macOS's
//!   user-interactive QoS: the write's stamp → the poll's return.
//!
//! **Cold** evicts the caches before each trigger (a 64 MiB sweep, one
//! store per line): the order path's state as it is about once an hour.
//! Samples are exact (`now_ns` pairs, ~14 ns of clock each side); the
//! table prints n, p50, p99, p99.9 and max in ns. `T2T_OUT=<path>` also
//! writes the rows as JSON (results belong in the vault, never git).
//!
//! Run: `T2T_FRAMES=<file> cargo bench -p bench --bench t2t`;
//! `T2T_ONLY=<group>` (parse, sign, route, gateway, wake) runs one group.
//!
//! COPY-DOCTRINE: an offline bench; it allocates and copies outside the
//! timed spans.

use std::hint::black_box;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clob_dispatcher::{DispatchError, OrderDispatch, PaperDispatcher};
use core_time::now_ns;
use core_types::{
    CancelReq, ChannelEvent, ChannelId, Fill, ModifyReq, Order, Price, Qty, Side, VenueId,
};
use exec_gateway::{HotWindow, Idle};
use exec_hyperliquid::gateway::{pw, HlEngineEnd};
use exec_hyperliquid::testvenue::{Answer, Venue};
use exec_hyperliquid::{
    hl_duplex, CoinMap, GatewayCfg, HlCmd, HlGateway, Network, NonceStreams, Tif, Vault,
};

/// Warm triggers per stage.
const N_WARM: usize = 10_000;
/// Cold triggers per stage (each pays a full eviction).
const N_COLD: usize = 2_000;
/// Spinning triggers (2 ms apart: ~10 s).
const N_SPINNING: usize = 5_000;
/// Parked triggers (25 ms apart: ~50 s).
const N_PARKED: usize = 2_000;
/// Hot triggers per gateway: its 20 warm-up orders and these stay inside
/// the signer's 64-entry pool, as a window's few takes do.
const HOT_PER_BOOT: usize = 40;
/// Gateways per hot row (25 ms apart: ~38 s).
const HOT_BOOTS: usize = 25;
/// Parked triggers in the cold mode (an eviction and 25 ms apart: ~35 s).
const N_PARKED_COLD: usize = 1_000;
/// The eviction sweep: past the M4 Pro's L2 and system cache.
const EVICT_BYTES: usize = 64 << 20;
/// BIN15's slot.
const SLOT: u8 = 3;
/// A HIP-4 leg's asset id (`ASSET_BASE` + outcome 1941).
const LEG: u32 = exec_hyperliquid::asset::ASSET_BASE + 1941;
/// A late take: 250 contracts at 0.95 (1e8).
const PX_1E8: i64 = 95_000_000;
const SZ_1E8: i64 = 25_000_000_000;
/// The venue's answer to a late IoC that found the ask gone.
const IOC_MISS: &str = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Order could not immediately match against any resting orders. asset=101941"}]}}}"#;

/// One stage's samples (ns).
struct Row {
    stage: &'static str,
    mode: &'static str,
    ns: Vec<u64>,
}

impl Row {
    fn new(stage: &'static str, mode: &'static str, cap: usize) -> Self {
        Self {
            stage,
            mode,
            ns: Vec::with_capacity(cap),
        }
    }
}

/// Exact quantile of sorted samples (nearest rank).
fn q(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[i.clamp(1, sorted.len()) - 1]
}

/// One store per cache line over `buf`: evicts L1, L2 and the system cache.
fn evict(buf: &mut [u8]) {
    let mut i = 0usize;
    while i < buf.len() {
        buf[i] = buf[i].wrapping_add(1);
        i += 64;
    }
    black_box(&*buf);
}

// -----------------------------------------------------------------
// parse
// -----------------------------------------------------------------

/// The recorded payloads, in arrival order.
fn load_frames(path: &str) -> Vec<Vec<u8>> {
    let b = std::fs::read(path).expect("T2T_FRAMES: read");
    assert!(
        b.len() >= 8 && &b[..4] == b"T2TF",
        "T2T_FRAMES: not a T2TF file"
    );
    let mut out = Vec::new();
    let mut off = 8usize;
    while off + 12 <= b.len() {
        let n = u32::from_le_bytes(b[off + 8..off + 12].try_into().expect("len")) as usize;
        off += 12;
        assert!(off + n <= b.len(), "T2T_FRAMES: truncated");
        out.push(b[off..off + n].to_vec());
        off += n;
    }
    out
}

/// The channel index a payload parses under (`usize::MAX` = not a data push).
fn channel_of(p: &[u8]) -> usize {
    use ingress_hyperliquid::{classify, HlChannel, HlMsgKind};
    match classify(p) {
        HlMsgKind::Data(HlChannel::Bbo) => 0,
        HlMsgKind::Data(HlChannel::L2Book) => 1,
        HlMsgKind::Data(HlChannel::Trades) => 2,
        HlMsgKind::Data(HlChannel::ActiveAssetCtx) => 3,
        HlMsgKind::Data(HlChannel::AllMids) => 4,
        _ => usize::MAX,
    }
}

const CHANNELS: [&str; 5] = [
    "parse/bbo",
    "parse/l2Book",
    "parse/trades",
    "parse/activeAssetCtx",
    "parse/allMids",
];

/// What the ingress does with one data push after the WS deframe: classify,
/// the coin, the channel's parse (`run_loop`'s calls; trades split per row).
#[inline(never)]
fn parse_one(p: &[u8], depth: &mut core_types::DepthTopK) -> u64 {
    use ingress_hyperliquid::{
        classify, extract_coin, parse_active_asset_ctx, parse_all_mids, parse_bbo_any,
        parse_l2book_depth, parse_trade, HlAssetCtxFrame, HlBboFrame, HlChannel, HlL2BookFrame,
        HlMsgKind, HlTradeFrame,
    };
    const SYM: u32 = (4 << 24) | 7;
    let mut acc = 0u64;
    match classify(p) {
        HlMsgKind::Data(HlChannel::Bbo) => {
            acc += extract_coin(p).map_or(0, <[u8]>::len) as u64;
            let mut f = HlBboFrame::ZERO;
            acc += u64::from(parse_bbo_any(p, SYM, &mut f)) + f.ts_ns;
        }
        HlMsgKind::Data(HlChannel::L2Book) => {
            acc += extract_coin(p).map_or(0, <[u8]>::len) as u64;
            let mut h = HlL2BookFrame::ZERO;
            acc += u64::from(parse_l2book_depth(p, SYM, 1, depth, &mut h));
        }
        HlMsgKind::Data(HlChannel::Trades) => {
            const MARKER: &[u8] = b"\"coin\":\"";
            let mut at = 0usize;
            while let Some(off) = memchr::memmem::find(&p[at..], MARKER) {
                let start = at + off;
                let next = memchr::memmem::find(&p[start + MARKER.len()..], MARKER)
                    .map(|o| start + MARKER.len() + o);
                let end = next.unwrap_or(p.len());
                let mut t = HlTradeFrame::ZERO;
                acc += u64::from(parse_trade(&p[start..end], SYM, &mut t));
                at = end;
                if next.is_none() {
                    break;
                }
            }
        }
        HlMsgKind::Data(HlChannel::ActiveAssetCtx) => {
            let mut f = HlAssetCtxFrame::ZERO;
            acc += u64::from(parse_active_asset_ctx(p, SYM, &mut f));
        }
        HlMsgKind::Data(HlChannel::AllMids) => {
            acc += u64::from(parse_all_mids(p).unwrap_or(0));
        }
        _ => {}
    }
    acc
}

fn stage_parse(rows: &mut Vec<Row>, frames: &[Vec<u8>], evict_buf: &mut [u8]) {
    let mut depth = core_types::DepthTopK::EMPTY;
    let mut warm: Vec<Row> = Vec::new();
    let mut cold: Vec<Row> = Vec::new();
    let mut c = 0usize;
    while c < CHANNELS.len() {
        warm.push(Row::new(CHANNELS[c], "warm", N_WARM * 4));
        cold.push(Row::new(CHANNELS[c], "cold", N_COLD));
        c += 1;
    }
    let mut kind = Vec::with_capacity(frames.len());
    let mut i = 0usize;
    while i < frames.len() {
        kind.push(channel_of(&frames[i]));
        i += 1;
    }
    // Warm: the tape, back to back, until every channel has its samples.
    let mut pass = 0usize;
    while pass < 64 && warm[0].ns.len() < N_WARM {
        let mut i = 0usize;
        while i < frames.len() {
            let k = kind[i];
            if k != usize::MAX {
                let t = now_ns();
                black_box(parse_one(black_box(&frames[i]), &mut depth));
                warm[k].ns.push(now_ns() - t);
            }
            i += 1;
        }
        pass += 1;
    }
    // Cold: one frame per eviction, cycling the tape.
    let mut i = 0usize;
    let mut done = 0usize;
    while done < N_COLD * 2 && i < frames.len() * 64 {
        let f = i % frames.len();
        let k = kind[f];
        if k != usize::MAX && cold[k].ns.len() < N_COLD {
            evict(evict_buf);
            let t = now_ns();
            black_box(parse_one(black_box(&frames[f]), &mut depth));
            cold[k].ns.push(now_ns() - t);
            done += 1;
        }
        i += 1;
    }
    rows.append(&mut warm);
    rows.append(&mut cold);
}

// -----------------------------------------------------------------
// sign
// -----------------------------------------------------------------

fn stage_sign(rows: &mut Vec<Row>, evict_buf: &mut [u8]) {
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("sign: key");
    // An encoded one-order action is ~110 B of msgpack; the hash cost is
    // length-bound (one keccak block) and the ECDSA cost is constant.
    let mut action = [0u8; 112];
    let mut i = 0usize;
    while i < action.len() {
        action[i] = (i as u8).wrapping_mul(31).wrapping_add(0x80);
        i += 1;
    }
    let mut warm = Row::new("sign", "warm", N_WARM);
    let mut cold = Row::new("sign", "cold", N_COLD);
    let mut n = 0u64;
    while (n as usize) < N_WARM {
        let t = now_ns();
        let s = exec_hyperliquid::sign_action(
            &sk,
            black_box(&action),
            1_761_000_000_000 + n,
            Vault::None,
            None,
            Network::Testnet,
        );
        warm.ns.push(now_ns() - t);
        black_box(s.expect("sign"));
        n += 1;
    }
    while cold.ns.len() < N_COLD {
        evict(evict_buf);
        let t = now_ns();
        let s = exec_hyperliquid::sign_action(
            &sk,
            black_box(&action),
            1_761_000_000_000 + n,
            Vault::None,
            None,
            Network::Testnet,
        );
        cold.ns.push(now_ns() - t);
        black_box(s.expect("sign"));
        n += 1;
    }
    rows.push(warm);
    rows.push(cold);
}

/// T1-c: the same action through the precomputed-nonce signer — the pool
/// refilled between triggers, untimed, as the gateway's idle turns do.
fn stage_sign_prenonce(rows: &mut Vec<Row>, evict_buf: &mut [u8]) {
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("sign: key");
    let mut signer = signer_eip712::prenonce::PrenonceSigner::new(&sk).expect("sign: pool");
    let mut action = [0u8; 112];
    let mut i = 0usize;
    while i < action.len() {
        action[i] = (i as u8).wrapping_mul(31).wrapping_add(0x80);
        i += 1;
    }
    let mut warm = Row::new("sign/prenonce", "warm", N_WARM);
    let mut cold = Row::new("sign/prenonce", "cold", N_COLD);
    let mut n = 0u64;
    while (n as usize) < N_WARM + N_COLD {
        let is_cold = (n as usize) >= N_WARM;
        if is_cold {
            evict(evict_buf);
        }
        let t = now_ns();
        let s = exec_hyperliquid::sign::sign_action_prenonce(
            &mut signer,
            black_box(&action),
            1_761_000_000_000 + n,
            Vault::None,
            None,
            Network::Testnet,
        );
        let dt = now_ns() - t;
        black_box(s.expect("sign"));
        if is_cold {
            cold.ns.push(dt);
        } else {
            warm.ns.push(dt);
        }
        while signer.refill_one() {}
        n += 1;
    }
    assert_eq!(
        signer.counters().fallback,
        0,
        "sign/prenonce: every signature from the pool"
    );
    rows.push(warm);
    rows.push(cold);
}

// -----------------------------------------------------------------
// route
// -----------------------------------------------------------------

/// A live arm that takes everything (the gateway arm's ring push is the
/// `ring/*` benches').
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

fn stage_route(rows: &mut Vec<Row>, evict_buf: &mut [u8]) {
    use exec_router::{ExecMode, ExecRoute, HaltLimits, RoutedDispatcher, SlotCaps, LEDGER_ROWS};
    /// 2026-09-19T00:00:01Z: the router's wall anchor maps it to itself.
    const T0: u64 = 1_789_776_001_000_000_000;
    const fn sym(ord: u32) -> u32 {
        (4u32 << 24) | ord
    }
    let mut route = ExecRoute::all_paper();
    route
        .set_slot(
            usize::from(SLOT),
            ExecMode::Live,
            &[VenueId::Hyperliquid.to_u8()],
            // The standalone's frame: $250 an order and an instance.
            SlotCaps::new(250_000_000, 250_000_000, 100_000_000_000, 32),
            HaltLimits::none(),
        )
        .expect("route: slot 3 live");
    let mut d = RoutedDispatcher::new(
        route,
        PaperDispatcher::new(),
        Yes,
        core_time::WallAnchor::new(T0, T0),
    );
    d.mark_ledger_seeded();
    // A full binding table, so every scan walks every row.
    let mut o = 0u32;
    while o < LEDGER_ROWS as u32 {
        d.on_venue_event(&ChannelEvent::new(
            T0,
            VenueId::Hyperliquid,
            ChannelId::InstrumentRoll,
            sym(100 + o * 2),
            core_types::pack_roll_seq(1_000 + o, 60, o as usize, false),
            0,
            0,
            0,
        ));
        o += 1;
    }
    let mut warm = Row::new("route", "warm", N_WARM);
    let mut cold = Row::new("route", "cold", N_COLD);
    let mut i = 0u64;
    let mut refused = 0u64;
    while (i as usize) < N_WARM + N_COLD {
        let k = (i % LEDGER_ROWS as u64) as u32;
        let mut ord = Order::new(
            T0 + i,
            VenueId::Hyperliquid,
            sym(100 + k * 2),
            Side::Bid,
            core_fill::ORDER_KIND_IOC,
            Price::from_raw(950_000),
            Qty::from_raw(250_000_000),
            1_000_000 + i,
        );
        ord.strategy_id = SLOT;
        let is_cold = (i as usize) >= N_WARM;
        if is_cold {
            evict(evict_buf);
        }
        let t = now_ns();
        let r = d.submit(black_box(&ord));
        let dt = now_ns() - t;
        if r.is_ok() {
            if is_cold {
                cold.ns.push(dt);
            } else {
                warm.ns.push(dt);
            }
            // The venue's IoC miss, as the arm reports it: the order ends.
            let _ = d.cancel(&CancelReq::of(&ord, T0 + i));
        } else {
            refused += 1;
        }
        i += 1;
    }
    assert!(
        refused == 0,
        "route: {refused} takes refused — the bench measured the refusal path"
    );
    rows.push(warm);
    rows.push(cold);
}

// -----------------------------------------------------------------
// gateway
// -----------------------------------------------------------------

/// The real gateway as the engine boots it (the agent's shared nonce
/// stream), against `venue`, under `idle`.
fn boot_gateway(venue: &Venue, idle: Idle) -> (HlGateway, HlEngineEnd) {
    let (mut eng, gw_end) = hl_duplex();
    let cfg = GatewayCfg {
        host: "localhost".to_string(),
        port: venue.port,
        tls: Arc::clone(&venue.tls),
        network: Network::Testnet,
        master: [0xA5; 20],
        slots: 1 << SLOT,
        post_timeout_ns: 5_000_000_000,
        idle,
        ip_weight_per_min: u32::MAX,
        ws_msgs_per_min: u32::MAX,
    };
    let sk = signer_eip712::parse_secret_key(&[0x11; 32]).expect("gateway: key");
    let mut gw = HlGateway::new(cfg, gw_end, sk, CoinMap::with_outcomes()).expect("gateway");
    gw.share_nonce(
        NonceStreams::new()
            .stream(&[0x58; 20])
            .expect("gateway: nonce stream"),
    );
    let t0 = Instant::now();
    let mut snap = false;
    while !gw.ws_live() || !snap {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "gateway: the socket never came up"
        );
        gw.turn();
        while let Some(e) = eng.evt.pop() {
            snap |= e.kind == exec_hyperliquid::event::EV_SNAPSHOT;
        }
    }
    (gw, eng)
}

/// One take through an inline gateway: push → the turn that wrote it
/// returns; then its answer is drained (untimed).
fn fire_inline(gw: &mut HlGateway, eng: &mut HlEngineEnd, oid: u64) -> u64 {
    let io = eng.stats.io();
    let t = now_ns();
    let cmd = HlCmd::place(SLOT, oid, LEG, true, PX_1E8, SZ_1E8, Tif::Ioc).at(t);
    assert!(eng.cmd.push(&cmd), "gateway: the command ring is full");
    while eng.stats.io()[0] == io[0] {
        gw.turn();
    }
    let dt = now_ns() - t;
    let t0 = Instant::now();
    while eng.stats.io()[1] == io[1] {
        assert!(t0.elapsed() < Duration::from_secs(5), "gateway: no answer");
        gw.turn();
    }
    while eng.evt.pop().is_some() {}
    dt
}

fn stage_gateway(rows: &mut Vec<Row>, evict_buf: &mut [u8]) {
    let venue = Venue::start();
    venue.policy(|_| Answer::Action(IOC_MISS.to_string()));

    // Inline: a zero park, turned here.
    let (mut gw, mut eng) = boot_gateway(&venue, Idle::new(0, 0));
    let mut oid = 1u64;
    let mut w = 0;
    while w < 200 {
        black_box(fire_inline(&mut gw, &mut eng, oid));
        oid += 1;
        w += 1;
    }
    let mut warm = Row::new("gateway/inline", "warm", N_WARM);
    while warm.ns.len() < N_WARM {
        warm.ns.push(fire_inline(&mut gw, &mut eng, oid));
        oid += 1;
    }
    let mut cold = Row::new("gateway/inline", "cold", N_COLD);
    while cold.ns.len() < N_COLD {
        evict(evict_buf);
        cold.ns.push(fire_inline(&mut gw, &mut eng, oid));
        oid += 1;
    }
    rows.push(warm);
    rows.push(cold);
    let p2w = &eng.stats.push_to_write;
    println!(
        "gateway/inline cross-check, the gateway's own push_to_write (warm-up, warm and cold): n={} p50<={} p99<={} max={} ns",
        p2w.count(),
        p2w.quantile_ns(0.50),
        p2w.quantile_ns(0.99),
        p2w.max_ns()
    );
    drop(gw);

    let prod = Idle::default();
    let load = |gap_ms, n, idle, qos, cold| Load {
        gap_ms,
        n,
        idle,
        qos,
        cold,
    };
    let spinning = load(2, N_SPINNING, prod, false, false);
    rows.push(threaded(&venue, "gateway/spinning", spinning, evict_buf, &mut oid));
    let parked = load(25, N_PARKED, prod, false, false);
    rows.push(threaded(&venue, "gateway/parked", parked, evict_buf, &mut oid));
    let parked_cold = load(25, N_PARKED_COLD, prod, false, true);
    rows.push(threaded(&venue, "gateway/parked", parked_cold, evict_buf, &mut oid));
    // T1-a: the production policy under a window that is always hot; then
    // with T1-b's QoS on the gateway thread, as `spawn_gateway` sets it;
    // then cold, T1-f's keep-hot against an eviction.
    let hot = prod.with_hot(HotWindow::new(1_000, 1_000, 0).expect("always hot"));
    let hot_rows = [
        ("gateway/hot", false, false),
        ("gateway/hot+qos", true, false),
        ("gateway/hot", false, true),
    ];
    let mut h = 0;
    while h < hot_rows.len() {
        let (name, qos, cold) = hot_rows[h];
        let mode = if cold { "cold" } else { "warm" };
        let mut row = Row::new(name, mode, HOT_PER_BOOT * HOT_BOOTS);
        let mut b = 0;
        while b < HOT_BOOTS {
            let l = load(25, HOT_PER_BOOT, hot, qos, cold);
            row.ns.extend(threaded(&venue, name, l, evict_buf, &mut oid).ns);
            b += 1;
        }
        rows.push(row);
        h += 1;
    }
}

/// One threaded row's load: a take every `gap_ms` to a gateway under
/// `idle`, `n` of them measured after 20 warm-up takes.
#[derive(Clone, Copy)]
struct Load {
    gap_ms: u64,
    n: usize,
    idle: Idle,
    /// T1-b's QoS on the gateway thread.
    qos: bool,
    /// The caches evicted before each take's gap.
    cold: bool,
}

/// A fresh gateway on its own thread under `load.idle`; one take every
/// `load.gap_ms`, push → its sent counter moves.
fn threaded(
    venue: &Venue,
    stage: &'static str,
    load: Load,
    evict_buf: &mut [u8],
    oid: &mut u64,
) -> Row {
    let (mut gw, mut eng) = boot_gateway(venue, load.idle);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let qos = load.qos;
    let th = std::thread::spawn(move || {
        if qos {
            prefer_p_cores();
        }
        gw.run(&stop2);
    });
    let n_trig = load.n;
    let mode = if load.cold { "cold" } else { "warm" };
    let mut row = Row::new(stage, mode, n_trig);
    let mut n = 0usize;
    while n < n_trig + 20 {
        if load.cold {
            evict(evict_buf);
        }
        std::thread::sleep(Duration::from_millis(load.gap_ms));
        let io = eng.stats.io();
        let t = now_ns();
        let cmd = HlCmd::place(SLOT, *oid, LEG, true, PX_1E8, SZ_1E8, Tif::Ioc).at(t);
        assert!(eng.cmd.push(&cmd), "gateway: the command ring is full");
        while eng.stats.io()[0] == io[0] {
            std::hint::spin_loop();
        }
        let dt = now_ns() - t;
        if n >= 20 {
            row.ns.push(dt);
        }
        let t0 = Instant::now();
        while eng.stats.io()[1] == io[1] {
            assert!(t0.elapsed() < Duration::from_secs(5), "gateway: no answer");
            std::hint::spin_loop();
        }
        while eng.evt.pop().is_some() {}
        *oid += 1;
        n += 1;
    }
    stop.store(true, Ordering::Relaxed);
    th.join().expect("gateway thread");
    let p2w = &eng.stats.push_to_write;
    println!(
        "{stage} {mode} cross-check, the gateway's own push_to_write: n={} p50<={} p99<={} p999<={} max={} ns; signed pooled/fallback at its last heartbeat {}/{}",
        p2w.count(),
        p2w.quantile_ns(0.50),
        p2w.quantile_ns(0.99),
        p2w.quantile_ns(0.999),
        p2w.max_ns(),
        eng.stats.published(pw::SIGNS_POOLED),
        eng.stats.published(pw::SIGNS_FALLBACK)
    );
    row
}

// -----------------------------------------------------------------
// wake (T1-b)
// -----------------------------------------------------------------

/// Wake-up triggers per variant (3 ms apart).
const N_WAKE: usize = 1_000;

/// The macOS QoS class that keeps a thread on the performance cores.
#[cfg(target_os = "macos")]
fn prefer_p_cores() {
    // SAFETY: sets the calling thread's own QoS class; no memory is passed.
    let _ = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
}

#[cfg(not(target_os = "macos"))]
fn prefer_p_cores() {}

/// **T1-b: what the HL ingress pays to be woken.** A loopback TCP pair:
/// this thread stamps and writes one byte every 3 ms; a reader thread waits
/// on mio — blocked in `poll` with the ingress's 50 ms timeout (today), or
/// spinning on a zero timeout (T1-b) — and stamps the poll's return. The
/// row is that stamp minus the write's (the loopback send included).
fn stage_wake(rows: &mut Vec<Row>, stage: &'static str, spin: bool, qos: bool) {
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicU64;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("wake: bind");
    let addr = listener.local_addr().expect("wake: addr");
    let mut tx = std::net::TcpStream::connect(addr).expect("wake: connect");
    tx.set_nodelay(true).expect("wake: nodelay");
    let (rx, _) = listener.accept().expect("wake: accept");
    rx.set_nonblocking(true).expect("wake: nonblocking");
    let woke = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (woke2, stop2) = (Arc::clone(&woke), Arc::clone(&stop));
    let th = std::thread::spawn(move || {
        if qos {
            prefer_p_cores();
        }
        let mut poll = mio::Poll::new().expect("wake: poll");
        let mut events = mio::Events::with_capacity(4);
        let mut rx = mio::net::TcpStream::from_std(rx);
        poll.registry()
            .register(&mut rx, mio::Token(0), mio::Interest::READABLE)
            .expect("wake: register");
        let timeout = if spin {
            Duration::ZERO
        } else {
            Duration::from_millis(50)
        };
        let mut buf = [0u8; 64];
        while !stop2.load(Ordering::Relaxed) {
            if poll.poll(&mut events, Some(timeout)).is_err() {
                return;
            }
            if events.is_empty() {
                continue;
            }
            let t1 = now_ns();
            while let Ok(n) = rx.read(&mut buf) {
                if n == 0 {
                    return;
                }
            }
            woke2.store(t1, Ordering::Release);
        }
    });
    std::thread::sleep(Duration::from_millis(20));
    let mut row = Row::new(stage, "warm", N_WAKE);
    let mut n = 0usize;
    while n < N_WAKE + 20 {
        std::thread::sleep(Duration::from_millis(3));
        woke.store(0, Ordering::Release);
        let t0 = now_ns();
        tx.write_all(&[1u8]).expect("wake: write");
        let t0w = Instant::now();
        let mut t1 = woke.load(Ordering::Acquire);
        while t1 == 0 {
            assert!(t0w.elapsed() < Duration::from_secs(1), "wake: no wake");
            std::hint::spin_loop();
            t1 = woke.load(Ordering::Acquire);
        }
        if n >= 20 {
            row.ns.push(t1.saturating_sub(t0));
        }
        n += 1;
    }
    stop.store(true, Ordering::Relaxed);
    drop(tx);
    th.join().expect("wake thread");
    rows.push(row);
}

// -----------------------------------------------------------------
// report
// -----------------------------------------------------------------

fn main() {
    let mut evict_buf = vec![0u8; EVICT_BYTES];
    let mut rows: Vec<Row> = Vec::new();
    // `T2T_ONLY=<group>` runs one group: parse, sign, route, gateway, wake.
    let only = std::env::var("T2T_ONLY").ok();
    let on = |group: &str| only.as_deref().is_none_or(|o| o == group);
    if on("parse") {
        match std::env::var("T2T_FRAMES") {
            Ok(path) => {
                let frames = load_frames(&path);
                println!("parse: {} recorded payloads from {path}", frames.len());
                stage_parse(&mut rows, &frames, &mut evict_buf);
            }
            Err(_) => println!("parse: skipped (T2T_FRAMES unset)"),
        }
    }
    if on("sign") {
        stage_sign(&mut rows, &mut evict_buf);
        stage_sign_prenonce(&mut rows, &mut evict_buf);
    }
    if on("route") {
        stage_route(&mut rows, &mut evict_buf);
    }
    if on("gateway") {
        stage_gateway(&mut rows, &mut evict_buf);
    }
    if on("wake") {
        stage_wake(&mut rows, "wake/blocked", false, false);
        stage_wake(&mut rows, "wake/blocked+qos", false, true);
        stage_wake(&mut rows, "wake/spin", true, false);
        stage_wake(&mut rows, "wake/spin+qos", true, true);
    }

    println!(
        "{:<22} {:<5} {:>7} {:>9} {:>9} {:>9} {:>10}",
        "stage", "mode", "n", "p50_ns", "p99_ns", "p999_ns", "max_ns"
    );
    let mut json = String::from("{\"bench\":\"t2t\",\"rows\":[");
    let mut i = 0usize;
    while i < rows.len() {
        let r = &mut rows[i];
        r.ns.sort_unstable();
        let (p50, p99, p999, max) = (
            q(&r.ns, 0.50),
            q(&r.ns, 0.99),
            q(&r.ns, 0.999),
            q(&r.ns, 1.0),
        );
        println!(
            "{:<22} {:<5} {:>7} {:>9} {:>9} {:>9} {:>10}",
            r.stage,
            r.mode,
            r.ns.len(),
            p50,
            p99,
            p999,
            max
        );
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"stage\":\"{}\",\"mode\":\"{}\",\"n\":{},\"p50_ns\":{p50},\"p99_ns\":{p99},\"p999_ns\":{p999},\"max_ns\":{max}}}",
            r.stage,
            r.mode,
            r.ns.len()
        ));
        i += 1;
    }
    json.push_str("]}\n");
    if let Ok(out) = std::env::var("T2T_OUT") {
        let mut f = std::fs::File::create(&out).expect("T2T_OUT: create");
        f.write_all(json.as_bytes()).expect("T2T_OUT: write");
        println!("rows written to {out}");
    }
}
