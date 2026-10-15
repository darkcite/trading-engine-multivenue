// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HC5 live smoke — the Hypercall ingress against the REAL venue,
//! WITHOUT the engine. The launchd engine is the ONE engine and is never
//! stopped for this (rulings MX9-EXEC / O-HC6): nothing here binds 9191
//! or `ai.sock`. What runs is the production path end to end — the HC4
//! boot discovery (`GET /markets` through `ingress_hypercall::discovery`
//! and the capped-chain law), then [`cli::spawn_hypercall`] (the same
//! run loop, parsers, REST poller, handoff ring, PMLR capture and raw
//! tap the engine spawns) for a bounded window, with this test draining
//! the tick and opt rings itself and reading the capture back after.
//!
//! `#[ignore]` — live network. Run it explicitly, in a SEPARATE target
//! dir so the release binary the launchd wrapper execs is untouched:
//!
//! ```text
//! CARGO_TARGET_DIR=/tmp/hc5-target cargo test --release -p cli \
//!   --test hypercall_live_smoke -- --ignored --nocapture
//! ```
//!
//! Env: `HC_SMOKE_SECS` (window, default 90, capped at 900 — the ≤ 2 h
//! law with margin), `HC_SMOKE_DIR` (capture root, default the OS temp
//! dir), `HC_SMOKE_UNDERLYINGS` (default `BTC,ETH,SP500,NVDA`, the HC0
//! probe's), `HC_SMOKE_E` / `HC_SMOKE_K` (default 1 × 4), and
//! `HC_SMOKE_CA_BUNDLE` — a PEM root bundle that REPLACES the compiled-in
//! Mozilla roots, for a host whose egress re-signs TLS (a sandbox; never
//! the engine, which only ever trusts the compiled-in set). The poller
//! runs at the 60 s floor so every underlying is polled inside a 90 s
//! window. The run dir keeps `hypercall-{ticks,events,opt-summary}.pmlr`
//! and the full raw tap. Offline test code: allocation is fine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_config::universe::{HC_SUMMARY_EVERY_S_MIN, OPT_ORDINAL_BASE};
use core_io::{PmlrReader, TapCfg, TapMode};
use core_metrics::IngressStatus;
use core_net::TlsTransport;
use core_ring::Ring;
use core_types::{
    make_symbol_id, ChannelEvent, ChannelId, OptSummary, Tick, VenueId, EVENT_RING_SIZE,
    OPT_RING_SIZE,
};
use engine::TICK_RING_SIZE;
use ingress_hypercall::counters::get;
use ingress_hypercall::discovery as hcd;

/// The compiled-in roots, or `HC_SMOKE_CA_BUNDLE`'s (module docs).
fn tls_config() -> Arc<rustls::ClientConfig> {
    use rustls_pki_types::pem::PemObject;
    let Ok(path) = std::env::var("HC_SMOKE_CA_BUNDLE") else {
        return TlsTransport::default_client_config();
    };
    let pem = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    for der in rustls_pki_types::CertificateDer::pem_slice_iter(&pem) {
        roots.add(der.expect("a PEM certificate")).expect("a usable trust anchor");
    }
    println!("tls: {} roots from {path}", roots.len());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "live network — the HC5 Hypercall smoke; run explicitly (module docs)"]
fn hypercall_live_smoke() {
    let secs = u64::from(env_u32("HC_SMOKE_SECS", 90).min(900));
    let root = std::env::var("HC_SMOKE_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let unds: Vec<String> = std::env::var("HC_SMOKE_UNDERLYINGS")
        .unwrap_or_else(|_| "BTC,ETH,SP500,NVDA".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let (e, k) = (env_u32("HC_SMOKE_E", 1), env_u32("HC_SMOKE_K", 4));
    let tls = tls_config();

    // ---- boot discovery (the run_hypercall contract) ----
    let mut buf = Vec::new();
    let t = Instant::now();
    let r = core_net::boot_http::https_get(
        &tls,
        ingress_hypercall::HC_REST_HOST,
        443,
        hcd::MARKETS_PATH,
        b"multivenue-engine/hc5-smoke",
        &mut buf,
        hcd::MARKETS_MAX_BODY,
        Duration::from_secs(30),
    )
    .expect("GET /markets");
    println!("discovery: /markets {} B in {} ms", r.len(), t.elapsed().as_millis());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let names: Vec<&[u8]> = unds.iter().map(|u| u.as_bytes()).collect();
    let m = hcd::parse_markets(&buf[r], &names, now_ms, hcd::DEFAULT_BLACKOUT_MS)
        .expect("/markets parses and lists every smoke underlying");
    let sel = hcd::select_universe(&m, names.len(), e, k);
    println!(
        "discovery: {} candidates, {} refused, {} selected (E{e} × K{k} × 2 × {})",
        m.rows.len(),
        m.refused,
        sel.len(),
        names.len()
    );
    assert!(!sel.is_empty(), "the capped chain selected nothing");
    let mut symbols = ingress_hypercall::HcSymbolTable::new();
    let mut sel_names = Vec::with_capacity(sel.len());
    for (i, row) in sel.iter().enumerate() {
        let sym = make_symbol_id(VenueId::Hypercall, OPT_ORDINAL_BASE + i as u32 + 1);
        symbols.insert(row.name(), sym).expect("universe table");
        sel_names.push(String::from_utf8_lossy(row.name()).into_owned());
    }
    let mut underlyings = ingress_hypercall::HcUnderlyings::new();
    for (i, u) in unds.iter().enumerate() {
        underlyings
            .insert(u.as_bytes(), make_symbol_id(VenueId::Hypercall, i as u32 + 1))
            .expect("index table");
    }

    // ---- the production spawn ----
    let (run_dir, epoch_ns) = cli::new_capture_run_dir(&root).expect("run dir");
    let (tick_prod, mut tick_cons) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (ev_prod, mut ev_cons) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();
    let (opt_prod, mut opt_cons) = Ring::<OptSummary, OPT_RING_SIZE>::new().split();
    let status = Arc::new(IngressStatus::new());
    let counters = Arc::new(ingress_hypercall::HcCounters::new());
    let spec = cli::HypercallSpec {
        ws_host: String::from_utf8_lossy(ingress_hypercall::HC_WS_HOST).into_owned(),
        rest_host: ingress_hypercall::HC_REST_HOST.to_string(),
        symbols,
        underlyings,
        summary_underlyings: unds.clone(),
        summary_every_s: HC_SUMMARY_EVERY_S_MIN,
        stale_after_ms: VenueId::Hypercall.default_stale_after_ms(),
    };
    let handles = cli::spawn_hypercall(
        spec,
        tls.clone(),
        tick_prod,
        ev_prod,
        opt_prod,
        status.clone(),
        counters.clone(),
        11,
        &run_dir,
        epoch_ns,
        TapCfg {
            mode: TapMode::All,
            budget_bytes: 256 * 1024 * 1024,
        },
        None,
    )
    .expect("spawn_hypercall");

    // ---- drain for the window ----
    let n = sel.len();
    let idx_of = |sym: u32| ((sym & 0x00FF_FFFF) - OPT_ORDINAL_BASE - 1) as usize;
    let mut ticks = vec![0u64; n];
    let (mut one_sided, mut crossed, mut stale) = (0u64, 0u64, 0u64);
    let mut opt_rows = vec![0u64; n];
    let mut lane_events = 0u64;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut idle = true;
        while let Some(t) = tick_cons.try_pop_ref() {
            idle = false;
            assert_eq!(t.venue, VenueId::Hypercall as u8);
            ticks[idx_of(t.sym)] += 1;
            let (b, a) = (t.bid_px.raw(), t.ask_px.raw());
            if b == 0 || a == 0 {
                one_sided += 1;
            } else if b > a {
                crossed += 1;
            }
            if t.is_stale() {
                stale += 1;
            }
        }
        while let Some(o) = opt_cons.try_pop_ref() {
            idle = false;
            assert_eq!(o.venue, VenueId::Hypercall as u8);
            opt_rows[idx_of(o.sym)] += 1;
        }
        while ev_cons.try_pop_ref().is_some() {
            lane_events += 1;
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    cli::signal_shutdown();
    for h in handles {
        h.join().expect("hypercall thread joins");
    }

    // ---- the capture, read back ----
    let events: PmlrReader<ChannelEvent> =
        PmlrReader::open(run_dir.join("hypercall-events.pmlr")).expect("events file");
    let (mut marks, mut trades, mut providers) = (vec![0u64; unds.len()], 0u64, 0u64);
    for ev in events.records() {
        match ChannelId::from_u8(ev.channel) {
            Some(ChannelId::Mark) => marks[((ev.sym & 0x00FF_FFFF) - 1) as usize] += 1,
            Some(ChannelId::Trade) => trades += 1,
            Some(ChannelId::ProviderQuote) => providers += 1,
            other => panic!("unexpected channel {other:?} in the Hypercall capture"),
        }
    }
    let captured_ticks = PmlrReader::<Tick>::open(run_dir.join("hypercall-ticks.pmlr"))
        .expect("ticks file")
        .len();
    let captured_opts = PmlrReader::<OptSummary>::open(run_dir.join("hypercall-opt-summary.pmlr"))
        .expect("opt-summary file")
        .len();

    // ---- report ----
    let c = &counters;
    println!("\nwindow {secs} s · run dir {}", run_dir.display());
    println!(
        "status: msgs={} ticks={} parse_errors={} reconnects={} stale={} ring_drops={} opt_ring_drops={} feed_delay_ema_ms={}",
        status.msgs_total(),
        status.ticks_total(),
        status.parse_errors_total(),
        status.reconnects_total(),
        status.stale_ticks_total(),
        status.ring_drops_total(),
        status.opt_ring_drops_total(),
        status.feed_delay_ema_ms()
    );
    println!(
        "venue: subscribes={} closes={:?} one_sided={} empty={} crossed={} provider_quotes={} providers_max={} clock_syncs={} clock_rtt_ms={} index_age_ms={} publish_lag_ms={} quoted_instruments={}",
        get(&c.ws.subscribes),
        c.ws.closes.iter().map(get).collect::<Vec<_>>(),
        get(&c.ws.one_sided_quotes),
        get(&c.ws.empty_quotes),
        get(&c.ws.crossed_quotes),
        get(&c.ws.provider_quotes),
        get(&c.ws.providers_max),
        get(&c.ws.clock_syncs),
        get(&c.ws.clock_rtt_ms),
        get(&c.ws.index_age_ms),
        get(&c.ws.quote_publish_lag_ms),
        get(&c.ws.quoted_instruments),
    );
    println!(
        "poller: ok={} err={} rows={} foreign_rows={} handoff_drops={} last_round_ms={}",
        get(&c.rest.polls_ok),
        get(&c.rest.polls_err),
        get(&c.rest.opt_rows),
        get(&c.rest.foreign_rows),
        get(&c.rest.handoff_drops),
        get(&c.rest.last_round_ms),
    );
    println!(
        "ring: one-sided {one_sided}, crossed {crossed}, stale {stale}; lane events {lane_events} (funding-only lane: 0 expected)"
    );
    println!(
        "capture: ticks {captured_ticks}, opt rows {captured_opts}, marks {marks:?}, trades {trades}, provider sides {providers}"
    );
    for (i, name) in sel_names.iter().enumerate() {
        println!("  {name:<28} ticks={:<5} opt_rows={}", ticks[i], opt_rows[i]);
    }

    let mut failures = Vec::new();
    if ticks.iter().all(|&t| t == 0) {
        failures.push("no tick on any selected instrument".to_string());
    }
    if status.parse_errors_total() != 0 {
        failures.push(format!("{} parse errors", status.parse_errors_total()));
    }
    // Server pings every 20 s are answered and the client ClockSync runs
    // on its own 20 s clock: a window past 60 s must see no reconnect.
    if status.reconnects_total() != 0 {
        failures.push(format!("{} reconnects", status.reconnects_total()));
    }
    if get(&c.ws.subscribes) != 1 {
        failures.push(format!("{} subscribe sets (one expected)", get(&c.ws.subscribes)));
    }
    if get(&c.ws.clock_syncs) == 0 {
        failures.push("no ClockSynced answer".to_string());
    }
    if marks.contains(&0) {
        failures.push(format!("an underlying without an index Mark: {marks:?}"));
    }
    if secs >= 60 && get(&c.rest.polls_ok) < names.len() as u64 {
        failures.push(format!(
            "{} of {} underlyings polled OK",
            get(&c.rest.polls_ok),
            names.len()
        ));
    }
    if secs >= 60 && opt_rows.iter().sum::<u64>() == 0 {
        failures.push("no OptSummary row reached the opt lane".to_string());
    }
    if lane_events != 0 {
        failures.push(format!("{lane_events} events on the funding-only lane"));
    }
    // `ticks_total` counts market-data records (the house law: Deribit
    // counts its tickers and summaries too) — here the BBO ticks plus the
    // index Marks, every one of which the capture must hold.
    let marks_total: u64 = marks.iter().sum();
    if captured_ticks as u64 + marks_total != status.ticks_total() {
        failures.push(format!(
            "capture holds {captured_ticks} ticks + {marks_total} marks, the status counted {}",
            status.ticks_total()
        ));
    }
    assert!(failures.is_empty(), "live smoke failures: {failures:#?}");
}
