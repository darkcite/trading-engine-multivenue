// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! BX0 live smoke — the two Binance market-data lanes BX0 re-lit,
//! against the REAL venue, WITHOUT the engine:
//!
//! - **F1**: the USDⓈ-M `markPrice` lane on fstream's routed
//!   `/market/ws/` path (the legacy `/ws/` URL upgrades and then stays
//!   silent), on three perpetuals and the front dated future;
//! - **F2**: the options lane's `<uly>@optionMarkPrice` array on
//!   fstream's routed `/market/stream` path (the retired nbstream
//!   `/eoptions/` base answers 404).
//!
//! The launchd engine is the ONE engine and is never stopped for this:
//! nothing here binds 9191 or `ai.sock`. What runs is the production
//! path end to end — the eapi boot discovery and capped selection
//! (`ingress_binance::eapi`), the connection specs from the SAME
//! builders the engine boot calls (`cli::bn_usdm_specs`,
//! `cli::bn_options_path`), then [`cli::spawn_binance_multi`] (the same
//! run loop, parsers and PMLR capture the engine spawns) for a bounded
//! window. The test drains the tick, venue-event and options rings
//! itself, then reads the run's `bn-events.pmlr` back for the
//! capture-only `Mark` events.
//!
//! `#[ignore]` — live network. Run it explicitly, in a SEPARATE target
//! dir so the release binary the launchd wrapper execs is untouched:
//!
//! ```text
//! CARGO_TARGET_DIR=/tmp/bx0-target cargo test --release -p cli \
//!   --test binance_md_live_smoke -- --ignored --nocapture
//! ```
//!
//! Env: `BN_SMOKE_SECS` (window, default 30, capped at 900) and
//! `BN_SMOKE_DIR` (capture root, default the OS temp dir; the run dir
//! is printed at the end). Offline test code: allocation is fine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_io::{PmlrReader, TapCfg, TapMode};
use core_metrics::IngressStatus;
use core_net::TlsTransport;
use core_ring::Ring;
use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, OptSummary, SymbolId, Tick, VenueId,
    EVENT_RING_SIZE, OPT_RING_SIZE,
};
use engine::TICK_RING_SIZE;
use ingress_binance::discovery::BnDiscovery;
use ingress_binance::eapi::{self, EapiDiscovery, EapiSymbolTable};

/// USDⓈ-M perpetuals under test (stream-lowercase).
const PERPS: [&str; 3] = ["btcusdt", "ethusdt", "solusdt"];
/// Option underlyings under test, and the chain each contributes
/// (expiries × strikes × C/P).
const ULYS: [&str; 2] = ["BTCUSDT", "ETHUSDT"];
const EXPIRIES: u32 = 1;
const STRIKES: u32 = 4;

fn get(tls: &Arc<rustls::ClientConfig>, host: &str, path: &str, buf: &mut Vec<u8>) -> std::ops::Range<usize> {
    core_net::boot_http::https_get(
        tls,
        host,
        443,
        path,
        b"multivenue-engine/bx0-smoke",
        buf,
        16 * 1024 * 1024,
        Duration::from_secs(20),
    )
    .unwrap_or_else(|e| panic!("GET https://{host}{path}: {e:?}"))
}

/// The front dated BTCUSDT contract the venue lists as TRADING —
/// candidate names scraped from the body, the verdict from the
/// production parser.
fn front_dated(body: &[u8], d: &BnDiscovery) -> String {
    let text = String::from_utf8_lossy(body);
    let mut names: Vec<String> = text
        .match_indices("\"symbol\":\"BTCUSDT_")
        .filter_map(|(i, m)| {
            let rest = &text[i + m.len() - "BTCUSDT_".len()..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .find(|n| {
            d.find(n.as_bytes())
                .is_some_and(|r| r.trading && r.contract_type.is_dated())
        })
        .expect("no TRADING dated BTCUSDT contract listed")
}

#[test]
#[ignore = "live network — the BX0 Binance market-data smoke; run explicitly (module docs)"]
fn binance_md_live_smoke() {
    let secs: u64 = std::env::var("BN_SMOKE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(10, 900);
    let root = std::env::var("BN_SMOKE_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let tls = TlsTransport::default_client_config();
    let mut buf = Vec::new();

    // ---- USDⓈ-M: the three perps + the front dated contract ----
    let r = get(&tls, "fapi.binance.com", "/fapi/v1/exchangeInfo", &mut buf);
    let mut fut = BnDiscovery::new();
    fut.ingest_body(&buf[r.clone()]).expect("fapi exchangeInfo parses");
    let dated = front_dated(&buf[r], &fut);
    let mut usdm: Vec<(String, SymbolId)> = Vec::new();
    for (j, p) in PERPS.iter().enumerate() {
        let row = fut.find(p.to_ascii_uppercase().as_bytes()).unwrap_or_else(|| panic!("{p} not listed"));
        assert!(row.trading && !row.contract_type.is_dated(), "{p} is not a live perpetual");
        usdm.push((p.to_string(), make_symbol_id(VenueId::Binance, 600 + j as u32)));
    }
    usdm.push((dated.to_ascii_lowercase(), make_symbol_id(VenueId::Binance, 610)));
    println!("usdm: {:?} (dated = {dated})", usdm.iter().map(|u| &u.0).collect::<Vec<_>>());

    // ---- options: the capped chain, selected exactly as the boot does ----
    let r = get(&tls, "eapi.binance.com", "/eapi/v1/exchangeInfo", &mut buf);
    let mut od = EapiDiscovery::new();
    od.ingest_exchange_info(&buf[r]).expect("eapi exchangeInfo parses");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut table = EapiSymbolTable::new();
    let mut options: Vec<(String, SymbolId)> = Vec::new();
    for uly in ULYS {
        std::thread::sleep(Duration::from_millis(150));
        let r = get(&tls, "eapi.binance.com", &format!("/eapi/v1/index?underlying={uly}"), &mut buf);
        let idx = eapi::parse_index_price(&buf[r]).expect("index parses");
        let sel = eapi::select_capped_chain(od.rows(), uly.as_bytes(), idx, EXPIRIES, STRIKES, now_ms);
        assert!(!sel.is_empty(), "{uly}: empty chain");
        for row in &sel {
            let sym = make_symbol_id(VenueId::Binance, 1_025 + options.len() as u32);
            table.insert(row.symbol(), sym).expect("table insert");
            options.push((String::from_utf8(row.symbol().to_vec()).unwrap(), sym));
        }
    }
    println!("options: {} selected ({:?} …)", options.len(), &options[..2]);

    // ---- the production specs and spawn ----
    let mut specs = Vec::new();
    for (name, sym) in &usdm {
        let [book, mark] = cli::bn_usdm_specs("fstream.binance.com", name, *sym);
        specs.push(book);
        specs.push(mark);
    }
    let ulys: Vec<String> = ULYS.iter().map(|u| u.to_string()).collect();
    let opt_path = cli::bn_options_path(&ulys);
    println!("options path: wss://fstream.binance.com{opt_path}");
    specs.push(cli::BinanceConnSpec {
        host: "fstream.binance.com".to_string(),
        path: opt_path,
        sym: 0,
        eapi: Some(table),
        mark_price: false,
        spot_sentinel: false,
    });
    let (run_dir, epoch_ns) = cli::new_capture_run_dir(&root).expect("run dir");
    let (tick_prod, mut tick_cons) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (ev_prod, mut ev_cons) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let (opt_prod, mut opt_cons) = Ring::<OptSummary, OPT_RING_SIZE>::new().split();
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
    let mut ticks = std::collections::HashMap::<SymbolId, u64>::new();
    let mut funding = std::collections::HashMap::<SymbolId, (u64, i64, i64)>::new();
    let mut summaries = std::collections::HashMap::<SymbolId, (u64, OptSummary)>::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut idle = true;
        while let Some(t) = tick_cons.try_pop() {
            idle = false;
            *ticks.entry(t.sym).or_default() += 1;
        }
        while let Some(e) = ev_cons.try_pop() {
            idle = false;
            if e.channel == ChannelId::Funding as u8 {
                let f = funding.entry(e.sym).or_default();
                f.0 += 1;
                f.1 = e.v0;
                f.2 = e.v1;
            }
        }
        while let Some(o) = opt_cons.try_pop() {
            idle = false;
            let s = summaries.entry(o.sym).or_insert((0, o));
            s.0 += 1;
            s.1 = o;
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    cli::signal_shutdown();
    handle.join().expect("ingress thread joins");

    // ---- the capture-only Mark events, read back from the run ----
    let events = PmlrReader::<ChannelEvent>::open(run_dir.join("bn-events.pmlr")).expect("bn-events.pmlr");
    let mut marks = std::collections::HashMap::<SymbolId, (u64, i64, i64)>::new();
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
        "status: msgs={} ticks={} parse_errors={} reconnects={} ring_drops={} event_ring_drops={} opt_ring_drops={}",
        status.msgs_total(),
        status.ticks_total(),
        status.parse_errors_total(),
        status.reconnects_total(),
        status.ring_drops_total(),
        status.event_ring_drops_total(),
        status.opt_ring_drops_total(),
    );
    let mut failures = Vec::new();
    // A 3 s cadence, less a start-up and a shutdown push of slack.
    let mark_floor = (secs / 3).saturating_sub(2).max(1);
    for (k, (name, sym)) in usdm.iter().enumerate() {
        let is_dated = k == usdm.len() - 1;
        let m = marks.get(sym).copied().unwrap_or_default();
        let f = funding.get(sym).copied().unwrap_or_default();
        println!(
            "  {name:<16} book_ticks={:<6} marks={:<4} mark_1e6={:<14} index_1e6={:<14} funding(n={}, rate_1e9={}, next_ms={})",
            ticks.get(sym).copied().unwrap_or(0),
            m.0,
            m.1,
            m.2,
            f.0,
            f.1,
            f.2
        );
        if m.0 < mark_floor {
            failures.push(format!("{name}: {} Mark events, floor {mark_floor} (1 per 3 s)", m.0));
        }
        if m.1 <= 0 || m.2 <= 0 {
            failures.push(format!("{name}: mark/index not positive"));
        }
        if is_dated {
            if f.0 != 0 {
                failures.push(format!("{name}: a DATED contract put {} Funding events on the lane", f.0));
            }
        } else if f.0 == 0 || f.2 <= now_ms {
            failures.push(format!("{name}: no Funding event with a future settlement on the lane"));
        }
    }
    // One push per underlying per ~1 s carries every selected row.
    let opt_floor = (secs / 2).max(1);
    for (name, sym) in &options {
        match summaries.get(sym) {
            Some((n, o)) => {
                println!(
                    "  {name:<22} summaries={n:<4} mark_1e9={:<16} iv_1e9={:<11} index_1e9={:<18} delta_1e9={:<11} book_ticks={}",
                    o.mark_px_1e9,
                    o.mark_iv_1e9,
                    o.underlying_px_1e9,
                    o.delta_1e9,
                    ticks.get(sym).copied().unwrap_or(0)
                );
                if *n < opt_floor {
                    failures.push(format!("{name}: {n} summaries, floor {opt_floor}"));
                }
                if o.mark_px_1e9 <= 0 || o.underlying_px_1e9 <= 0 {
                    failures.push(format!("{name}: mark/index not positive"));
                }
                if o.flags != core_types::OPT_SUMMARY_FLAG_MARK_PX {
                    failures.push(format!("{name}: flags {:#x}", o.flags));
                }
            }
            None => failures.push(format!("{name}: no OptSummary at all")),
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
