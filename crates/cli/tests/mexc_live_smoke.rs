// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! MX9 live smoke — the MEXC ingress against the REAL venue, WITHOUT
//! the engine. The launchd engine is the ONE engine and is never
//! stopped for this (operator ruling 2026-09-23): nothing here binds
//! 9191 or `ai.sock`. What runs is the production path end to end —
//! the boot REST discovery + Q-MX3 funding seeds through the
//! `ingress_mexc::discovery` parsers, then [`cli::spawn_mexc`] (the
//! same run loop, parsers, PMLR capture and raw tap the engine
//! spawns) on the Q-MX5 universe for a bounded window, with this test
//! draining the tick + venue-event rings itself.
//!
//! `#[ignore]` — live network. Run it explicitly, in a SEPARATE target
//! dir so the release binary the launchd wrapper execs is untouched:
//!
//! ```text
//! CARGO_TARGET_DIR=/tmp/mx9-target cargo test --release -p cli \
//!   --test mexc_live_smoke -- --ignored --nocapture
//! ```
//!
//! Env: `MEXC_SMOKE_SECS` (window, default 90, capped at 900) and
//! `MEXC_SMOKE_DIR` (capture root, default the OS temp dir). The run
//! dir keeps `mexc-ticks.pmlr`, `mexc-events.pmlr` and the full raw
//! tap for byte-level inspection (`multivenue-engine audit-replay`),
//! and is printed at the end. Offline test code: allocation is fine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_io::{TapCfg, TapMode};
use core_metrics::IngressStatus;
use core_net::TlsTransport;
use core_ring::Ring;
use core_types::{make_symbol_id, ChannelEvent, ChannelId, Tick, VenueId, EVENT_RING_SIZE};
use engine::TICK_RING_SIZE;
use ingress_mexc::discovery as mxd;
use ingress_mexc::{MexcClass, MexcSymbolTable};

/// The operator's Q-MX5 smoke universe: every class — crypto spot,
/// xStocks, crypto perps, TradFi perps (metals, energy, FX, index,
/// single-name equity).
const SPOT: [&str; 5] = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "AAPLXUSDT", "SPYXUSDT"];
const PERP: [&str; 7] = [
    "BTC_USDT",
    "ETH_USDT",
    "XAU_USDT",
    "USOIL_USDT",
    "EUR_USDT",
    "SPY_USDT",
    "AAPLSTOCK_USDT",
];

// The MX4 cadence exit (`depth.full` p99 < 1 s) was judged on the raw
// PUSH cadence before ticks became BBO changes (2026-09-23: BTC_USDT
// p50 269 ms / p99 764 ms; quiet perps push on change, 4–5 s apart).
// The ring now carries BBO changes only, so the gaps printed below are
// change-to-change — informational, not the push cadence.

fn get(
    tls: &Arc<rustls::ClientConfig>,
    host: &str,
    path: &str,
    buf: &mut Vec<u8>,
) -> std::ops::Range<usize> {
    core_net::boot_http::https_get(
        tls,
        host,
        443,
        path,
        b"multivenue-engine/mx9-smoke",
        buf,
        8 * 1024 * 1024,
        Duration::from_secs(15),
    )
    .unwrap_or_else(|e| panic!("GET https://{host}{path}: {e:?}"))
}

fn pctl(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

#[test]
#[ignore = "live network — the MX9 MEXC smoke; run explicitly (module docs)"]
fn mexc_live_smoke() {
    let secs: u64 = std::env::var("MEXC_SMOKE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90)
        .min(900);
    let root = std::env::var("MEXC_SMOKE_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let tls = TlsTransport::default_client_config();
    let mut buf = Vec::new();

    // ---- boot discovery (the run_mexc contract) ----
    let r = get(&tls, mxd::SPOT_REST_HOST, mxd::SPOT_EXCHANGE_INFO_PATH, &mut buf);
    let mut spot_d = mxd::MexcSpotDiscovery::new();
    spot_d.ingest_body(&buf[r]).expect("exchangeInfo parses");
    let r = get(&tls, mxd::FUT_REST_HOST, mxd::FUT_CONTRACT_DETAIL_PATH, &mut buf);
    let mut perp_d = mxd::MexcPerpDiscovery::new();
    perp_d.ingest_body(&buf[r]).expect("contract/detail parses");
    println!(
        "discovery: spot universe {} ({} live) · perp universe {} ({} live)",
        spot_d.universe_total(),
        spot_d.universe_trading(),
        perp_d.universe_total(),
        perp_d.universe_trading()
    );
    for s in SPOT {
        let row = spot_d.find(s.as_bytes()).unwrap_or_else(|| panic!("spot {s} not listed"));
        println!(
            "  spot {s:<16} live={} tick={} lot={} maker={} taker={} (×1e9)",
            row.trading, row.tick_size_1e9, row.lot_step_1e9, row.maker_fee_1e9, row.taker_fee_1e9
        );
        assert!(row.trading, "spot {s} not live");
    }
    let mut seeds = Vec::new();
    for (j, s) in PERP.iter().enumerate() {
        let row = perp_d.find(s.as_bytes()).unwrap_or_else(|| panic!("perp {s} not listed"));
        println!(
            "  perp {s:<16} live={} ctSize={} priceUnit={} volUnit={} maker={} taker={} (×1e9)",
            row.trading,
            row.contract_size_1e9,
            row.price_unit_1e9,
            row.vol_unit_1e9,
            row.maker_fee_1e9,
            row.taker_fee_1e9
        );
        assert!(row.trading, "perp {s} not live (state/apiAllowed)");
        std::thread::sleep(Duration::from_millis(110));
        let path = format!("{}{}", mxd::FUT_FUNDING_RATE_PATH, s);
        let r = get(&tls, mxd::FUT_REST_HOST, &path, &mut buf);
        let seed = mxd::parse_funding_rate(&buf[r]).expect("funding_rate parses");
        println!(
            "       funding next_settle_ms={} cycle_h={} rate_1e9={}",
            seed.next_settle_ms, seed.collect_cycle_h, seed.rate_1e9
        );
        assert!(seed.next_settle_ms > 0 && seed.collect_cycle_h > 0);
        seeds.push((
            make_symbol_id(VenueId::Mexc, core_config::universe::MEXC_PERP_ORDINAL_BASE + j as u32 + 1),
            seed.next_settle_ms,
            seed.collect_cycle_h,
        ));
    }

    // ---- the production spawn (bin's chunking law) ----
    let mut spot_t = MexcSymbolTable::new();
    for (i, s) in SPOT.iter().enumerate() {
        spot_t.insert(s.as_bytes(), make_symbol_id(VenueId::Mexc, i as u32 + 1)).unwrap();
    }
    let mut perp_t = MexcSymbolTable::new();
    for (j, s) in PERP.iter().enumerate() {
        perp_t
            .insert(
                s.as_bytes(),
                make_symbol_id(VenueId::Mexc, core_config::universe::MEXC_PERP_ORDINAL_BASE + j as u32 + 1),
            )
            .unwrap();
    }
    let path_of = |c: MexcClass| std::str::from_utf8(c.ws_path()).unwrap().to_string();
    let host_of = |c: MexcClass| std::str::from_utf8(c.default_ws_host()).unwrap().to_string();
    let specs = vec![
        cli::MexcConnSpec {
            class: MexcClass::Spot,
            host: host_of(MexcClass::Spot),
            path: path_of(MexcClass::Spot),
            table: spot_t,
            funding_seeds: Vec::new(),
        },
        cli::MexcConnSpec {
            class: MexcClass::Futures,
            host: host_of(MexcClass::Futures),
            path: path_of(MexcClass::Futures),
            table: perp_t,
            funding_seeds: seeds,
        },
    ];
    let (run_dir, epoch_ns) = cli::new_capture_run_dir(&root).expect("run dir");
    let (tick_prod, mut tick_cons) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (ev_prod, mut ev_cons) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let status = Arc::new(IngressStatus::new());
    let handle = cli::spawn_mexc(
        specs,
        tls.clone(),
        VenueId::Mexc.default_stale_after_ms(),
        tick_prod,
        ev_prod,
        status.clone(),
        9,
        &run_dir,
        epoch_ns,
        TapCfg {
            mode: TapMode::All,
            budget_bytes: 256 * 1024 * 1024,
        },
        None,
    )
    .expect("spawn_mexc");

    // ---- drain for the window ----
    let n_sym = SPOT.len() + PERP.len();
    let idx_of = |sym: u32| -> usize {
        let ord = sym & 0x00FF_FFFF;
        if ord as usize <= SPOT.len() {
            ord as usize - 1
        } else {
            SPOT.len() + (ord - core_config::universe::MEXC_PERP_ORDINAL_BASE - 1) as usize
        }
    };
    let mut ticks = vec![0u64; n_sym];
    let mut last_ts = vec![0u64; n_sym];
    let mut gaps: Vec<Vec<u64>> = vec![Vec::new(); n_sym];
    let mut crossed = vec![0u64; n_sym];
    let mut stale = vec![0u64; n_sym];
    let mut last_px = vec![(0i64, 0i64); n_sym];
    let mut funding = vec![(0u64, 0i64, 0i64); n_sym];
    let mut delay_ms: Vec<i64> = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut idle = true;
        while let Some(t) = tick_cons.try_pop_ref().as_deref().copied() {
            idle = false;
            assert_eq!(t.venue, VenueId::Mexc as u8);
            let k = idx_of(t.sym);
            ticks[k] += 1;
            if last_ts[k] != 0 {
                gaps[k].push(t.ts_ns.saturating_sub(last_ts[k]));
            }
            last_ts[k] = t.ts_ns;
            if t.bid_px.raw() >= t.ask_px.raw() {
                crossed[k] += 1;
            }
            if t.is_stale() {
                stale[k] += 1;
            }
            last_px[k] = (t.bid_px.raw(), t.ask_px.raw());
            if t.venue_time_ms != 0 {
                let wall_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                delay_ms.push(wall_ms - t.venue_time_ms as i64);
            }
        }
        while let Some(e) = ev_cons.try_pop_ref().as_deref().copied() {
            idle = false;
            if e.channel == ChannelId::Funding as u8 {
                let k = idx_of(e.sym);
                funding[k].0 += 1;
                funding[k].1 = e.v0;
                funding[k].2 = e.v1;
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    cli::signal_shutdown();
    handle.join().expect("ingress thread joins");

    // ---- report ----
    println!("\nwindow {secs} s · run dir {}", run_dir.display());
    println!(
        "status: msgs={} ticks={} parse_errors={} sub_drops={} seq_regressions={} reconnects={} stale={} ring_drops={} event_ring_drops={} feed_delay_ema_ms={}",
        status.msgs_total(),
        status.ticks_total(),
        status.parse_errors_total(),
        status.sub_drops_total(),
        status.seq_regressions_total(),
        status.reconnects_total(),
        status.stale_ticks_total(),
        status.ring_drops_total(),
        status.event_ring_drops_total(),
        status.feed_delay_ema_ms()
    );
    delay_ms.sort_unstable();
    let d: Vec<u64> = delay_ms.iter().map(|&v| v.max(0) as u64).collect();
    println!(
        "venue_time→wall (uncorrected for clock offset) ms: p50={} p90={} p99={} max={} (n={})",
        pctl(&d, 0.5),
        pctl(&d, 0.9),
        pctl(&d, 0.99),
        d.last().copied().unwrap_or(0),
        d.len()
    );
    let mut failures = Vec::new();
    for k in 0..n_sym {
        let name = if k < SPOT.len() { SPOT[k] } else { PERP[k - SPOT.len()] };
        gaps[k].sort_unstable();
        let (p50, p99, max) = (
            pctl(&gaps[k], 0.5) / 1_000_000,
            pctl(&gaps[k], 0.99) / 1_000_000,
            gaps[k].last().copied().unwrap_or(0) / 1_000_000,
        );
        println!(
            "  {name:<16} ticks={:<6} change_gap_ms p50={p50:<5} p99={p99:<6} max={max:<6} crossed={} stale={} ({:.2}%) last bid/ask={}/{} funding(n={}, rate_1e9={}, v1={})",
            ticks[k], crossed[k], stale[k], 100.0 * stale[k] as f64 / ticks[k].max(1) as f64, last_px[k].0, last_px[k].1, funding[k].0, funding[k].1, funding[k].2
        );
        if ticks[k] == 0 {
            failures.push(format!("{name}: no tick"));
        }
        if crossed[k] != 0 {
            failures.push(format!("{name}: {} crossed ticks", crossed[k]));
        }
        if k >= SPOT.len() && (funding[k].0 == 0 || funding[k].2 == 0) {
            failures.push(format!("{name}: no Funding event with a v1 on the event lane"));
        }
    }
    if status.parse_errors_total() != 0 {
        failures.push(format!("{} parse errors", status.parse_errors_total()));
    }
    if status.sub_drops_total() != 0 {
        failures.push(format!("{} subscribe drops", status.sub_drops_total()));
    }
    // The client-heartbeat law (futures closes a socket without a client
    // ping for 60 s, however busy): a window past 60 s must see none.
    if status.reconnects_total() != 0 {
        failures.push(format!("{} reconnects", status.reconnects_total()));
    }
    assert!(failures.is_empty(), "live smoke failures: {failures:#?}");
}
