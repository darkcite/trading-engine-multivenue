// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX2 live smoke — the COIN-M market-data lane (O-BX3), against the
//! REAL venue, WITHOUT the engine:
//!
//! - **discovery**: the whole `GET /dapi/v1/exchangeInfo` page through
//!   the production parser (`ingress_binance::discovery`) — COIN-M rows
//!   say `contractStatus`, carry `contractSize`, and keep their venue
//!   rules (F11);
//! - **the feeds**: `bookTicker` on `/ws/` and `markPrice` on
//!   `/market/ws/` on dstream, the USDⓈ-M path law K10 measured there,
//!   for two perpetuals and the front BTCUSD delivery contract.
//!
//! The launchd engine is the ONE engine and is never stopped for this:
//! nothing here binds 9191 or `ai.sock`. What runs is the production
//! path end to end — the connection specs from the SAME builder the
//! engine boot calls (`cli::bn_coinm_specs`), then
//! [`cli::spawn_binance_multi`] (the same run loop, parsers and PMLR
//! capture the engine spawns) for a bounded window. The test drains the
//! tick and venue-event rings itself, then reads the run's
//! `bn-events.pmlr` back for the capture-only `Mark` events.
//!
//! `#[ignore]` — live network. Run it explicitly, in a SEPARATE target
//! dir so the release binary the launchd wrapper execs is untouched:
//!
//! ```text
//! CARGO_TARGET_DIR=/tmp/bx2-target cargo test --release -p cli \
//!   --test binance_coinm_live_smoke -- --ignored --nocapture
//! ```
//!
//! Env: `BN_SMOKE_SECS` (window, default 30, capped at 900) and
//! `BN_SMOKE_DIR` (capture root, default the OS temp dir; the run dir
//! is printed at the end). Offline test code: allocation is fine.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_config::universe::{BN_COINM_DATED_ORDINAL_BASE, BN_COINM_ORDINAL_BASE};
use core_io::{PmlrReader, TapCfg, TapMode};
use core_metrics::IngressStatus;
use core_net::TlsTransport;
use core_ring::Ring;
use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, OptSummary, SymbolId, Tick, VenueId,
    EVENT_RING_SIZE, OPT_RING_SIZE,
};
use engine::TICK_RING_SIZE;
use ingress_binance::discovery::{BnContractType, BnDiscovery, BnStatus, BnUnderlying};

/// COIN-M perpetuals under test (stream-lowercase) and the USD face of
/// one contract on each.
const PERPS: [(&str, i64); 2] = [("btcusd_perp", 100), ("ethusd_perp", 10)];
/// The dstream host the engine's default `BINANCE_COINM_WS_HOST` names.
const HOST: &str = "dstream.binance.com";

#[test]
#[ignore = "live network — the BX2 COIN-M market-data smoke; run explicitly (module docs)"]
fn binance_coinm_live_smoke() {
    let secs: u64 = std::env::var("BN_SMOKE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(10, 900);
    let root = std::env::var("BN_SMOKE_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let tls = TlsTransport::default_client_config();
    let mut buf = Vec::new();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // ---- discovery: the whole dapi page, the production parser ----
    let r = core_net::boot_http::https_get(
        &tls,
        "dapi.binance.com",
        443,
        "/dapi/v1/exchangeInfo",
        b"multivenue-engine/bx2-smoke",
        &mut buf,
        16 * 1024 * 1024,
        Duration::from_secs(20),
    )
    .expect("GET dapi exchangeInfo");
    let mut d = BnDiscovery::new();
    let n = d.ingest_body(&buf[r]).expect("dapi exchangeInfo parses");
    println!("dapi: {n} rows, {} trading", d.universe_trading());
    let mut failures = Vec::new();
    for row in d.rows() {
        let name = String::from_utf8_lossy(row.symbol()).into_owned();
        if !row.is_inverse() || row.contract_size <= 0 {
            failures.push(format!("{name}: no contractSize — not read as inverse"));
        }
        if row.underlying != BnUnderlying::Coin {
            failures.push(format!("{name}: underlying {:?}", row.underlying));
        }
        let f = row.filters;
        if f.tick_size_1e9 <= 0 || f.lot_step_1e9 <= 0 || f.min_qty_1e9 <= 0 {
            failures.push(format!("{name}: tick/step/minQty missing ({f:?})"));
        }
        if f.max_qty_1e9 < f.min_qty_1e9 || f.max_num_orders == 0 {
            failures.push(format!("{name}: maxQty / order cap missing ({f:?})"));
        }
        if f.bid_up_1e9 <= 1_000_000_000 || f.bid_down_1e9 >= 1_000_000_000 {
            failures.push(format!("{name}: percent-price band missing ({f:?})"));
        }
        if row.price_precision == u8::MAX || row.qty_precision == u8::MAX {
            failures.push(format!("{name}: precisions missing"));
        }
    }

    let mut insts: Vec<(String, SymbolId, bool)> = Vec::new();
    for (j, (p, face)) in PERPS.iter().enumerate() {
        let row = d
            .find(p.to_ascii_uppercase().as_bytes())
            .unwrap_or_else(|| panic!("{p} not listed"));
        assert_eq!(row.status, BnStatus::Trading, "{p}");
        assert_eq!(row.contract_type, BnContractType::Perpetual, "{p}");
        assert_eq!(row.contract_size, *face, "{p}: contract face");
        let sym = make_symbol_id(VenueId::Binance, BN_COINM_ORDINAL_BASE + j as u32 + 1);
        insts.push((p.to_string(), sym, false));
    }
    // The front BTCUSD delivery contract the venue lists as TRADING.
    let front = d
        .rows()
        .iter()
        .filter(|r| {
            r.symbol().starts_with(b"BTCUSD_")
                && r.trading
                && r.contract_type.is_dated()
                && r.delivery_ms > now_ms
        })
        .min_by_key(|r| r.delivery_ms)
        .expect("no TRADING dated BTCUSD contract listed");
    let dated = String::from_utf8_lossy(front.symbol()).to_ascii_lowercase();
    insts.push((
        dated.clone(),
        make_symbol_id(VenueId::Binance, BN_COINM_DATED_ORDINAL_BASE + 1),
        true,
    ));
    println!("coinm: {:?}", insts.iter().map(|i| &i.0).collect::<Vec<_>>());

    // ---- the production specs and spawn ----
    let mut specs = Vec::new();
    for (name, sym, _) in &insts {
        let [book, mark] = cli::bn_coinm_specs(HOST, name, *sym);
        specs.push(book);
        specs.push(mark);
    }
    let (run_dir, epoch_ns) = cli::new_capture_run_dir(&root).expect("run dir");
    let (tick_prod, mut tick_cons) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (ev_prod, mut ev_cons) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let (opt_prod, _opt_cons) = Ring::<OptSummary, OPT_RING_SIZE>::new().split();
    let status = Arc::new(IngressStatus::new());
    let handle = cli::spawn_binance_multi(
        specs,
        tls.clone(),
        VenueId::Binance.default_stale_after_ms(),
        tick_prod,
        ev_prod,
        opt_prod,
        status.clone(),
        9,
        &run_dir,
        epoch_ns,
        TapCfg {
            mode: TapMode::Rejects,
            budget_bytes: 64 * 1024 * 1024,
        },
        None,
    )
    .expect("spawn_binance_multi");

    // ---- drain for the window ----
    let mut ticks = HashMap::<SymbolId, (u64, Option<Tick>)>::new();
    let mut funding = HashMap::<SymbolId, (u64, i64, i64)>::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut idle = true;
        while let Some(t) = tick_cons.try_pop_ref().as_deref().copied() {
            idle = false;
            let e = ticks.entry(t.sym).or_default();
            e.0 += 1;
            e.1 = Some(t);
        }
        while let Some(e) = ev_cons.try_pop_ref().as_deref().copied() {
            idle = false;
            if e.channel == ChannelId::Funding as u8 {
                let f = funding.entry(e.sym).or_default();
                f.0 += 1;
                f.1 = e.v0;
                f.2 = e.v1;
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    cli::signal_shutdown();
    handle.join().expect("ingress thread joins");

    // ---- the capture-only Mark events, read back from the run ----
    let events =
        PmlrReader::<ChannelEvent>::open(run_dir.join("bn-events.pmlr")).expect("bn-events.pmlr");
    let mut marks = HashMap::<SymbolId, (u64, i64, i64)>::new();
    for e in events.records() {
        if e.channel == ChannelId::Mark as u8 {
            let m = marks.entry(e.sym).or_default();
            m.0 += 1;
            m.1 = e.v0;
            m.2 = e.v1;
        }
    }

    // ---- report + verdict ----
    println!("\nwindow {secs} s · run dir {}", run_dir.display());
    println!(
        "status: msgs={} ticks={} parse_errors={} reconnects={} ring_drops={} event_ring_drops={}",
        status.msgs_total(),
        status.ticks_total(),
        status.parse_errors_total(),
        status.reconnects_total(),
        status.ring_drops_total(),
        status.event_ring_drops_total(),
    );
    // A 3 s cadence, less a start-up and a shutdown push of slack.
    let mark_floor = (secs / 3).saturating_sub(2).max(1);
    for (name, sym, is_dated) in &insts {
        let (n_ticks, last) = ticks.get(sym).copied().unwrap_or_default();
        let m = marks.get(sym).copied().unwrap_or_default();
        let f = funding.get(sym).copied().unwrap_or_default();
        println!(
            "  {name:<16} book_ticks={n_ticks:<6} last={last:?} marks={:<4} mark_1e6={:<14} index_1e6={:<14} funding(n={}, rate_1e9={}, next_ms={})",
            m.0, m.1, m.2, f.0, f.1, f.2
        );
        if n_ticks == 0 {
            failures.push(format!("{name}: no book ticks on dstream /ws/"));
        }
        if m.0 < mark_floor {
            failures.push(format!("{name}: {} Mark events, floor {mark_floor} (1 per 3 s)", m.0));
        }
        if m.1 <= 0 || m.2 <= 0 {
            failures.push(format!("{name}: mark/index not positive"));
        }
        if *is_dated {
            if f.0 != 0 {
                failures.push(format!("{name}: a DATED contract put {} Funding events on the lane", f.0));
            }
        } else if f.0 == 0 || f.2 <= now_ms {
            failures.push(format!("{name}: no Funding event with a future settlement on the lane"));
        }
    }
    if status.parse_errors_total() != 0 {
        failures.push(format!("{} parse errors", status.parse_errors_total()));
    }
    if status.reconnects_total() != 0 {
        failures.push(format!("{} reconnects", status.reconnects_total()));
    }
    assert!(failures.is_empty(), "live smoke failures: {failures:#?}");
}
