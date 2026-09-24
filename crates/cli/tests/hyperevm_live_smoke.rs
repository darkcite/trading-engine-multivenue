// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HYPARB H3b live smoke — the HyperEVM pool-event ingress against the
//! REAL chain, WITHOUT the engine. The launchd engine is the ONE engine
//! and is never stopped for this (the MX9 ruling's shape): nothing here
//! binds 9191 or `ai.sock`. What runs is the production path end to end
//! — [`cli::spawn_hyperevm`] (subscriptions, the in-session snapshot at a
//! pinned block, the O-H4 archive probe, the on-chain `decimals()` check,
//! hold-and-flush, live events, the PMLR capture and raw tap) — with this
//! test draining the pool ring into a `core_fill::AmmBook`, exactly as the
//! engine's paper matcher does.
//!
//! `#[ignore]` — live network. Run it explicitly, in a SEPARATE target
//! dir so the release binary the launchd wrapper execs is untouched:
//!
//! ```text
//! CARGO_TARGET_DIR=/tmp/hyparb-smoke-target cargo test --release -p cli \
//!   --test hyperevm_live_smoke -- --ignored --nocapture
//! ```
//!
//! Env: `HYPEREVM_WS_HOST` (default `rpc.purroofgroup.com`, O-H15),
//! `HYPEREVM_SMOKE_PATH` (default `/`), `HYPEREVM_SMOKE_POOLS` (comma-
//! separated `[hyperevm] pools` entries; default the WHYPE/USDC 0.05 %
//! Uniswap-ABI pool), `HYPEREVM_SMOKE_SECS` (window, default 60, capped
//! at 900) and `HYPEREVM_SMOKE_DIR` (capture root, default the OS temp
//! dir). Offline test code: allocation is fine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_io::{TapCfg, TapMode};
use core_metrics::IngressStatus;
use core_net::TlsTransport;
use core_ring::Ring;
use core_types::{make_symbol_id, VenueId};

/// WHYPE (18) / USDC (6), 0.05 %, Uniswap V3 ABI — the pool the
/// executor's fork test swaps against.
const DEFAULT_POOLS: &str = "0x6c9a33e3b592c0d65b3ba59355d5be0d38259285:v3:18:6";

#[test]
#[ignore = "live network: the HyperEVM chain over the archive endpoint"]
fn hyperevm_live_smoke() {
    // The ingress thread's own reports (connect failures, each run-loop
    // verdict) — the first live run reconnected silently without them.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let host =
        std::env::var("HYPEREVM_WS_HOST").unwrap_or_else(|_| "rpc.purroofgroup.com".to_owned());
    let path = std::env::var("HYPEREVM_SMOKE_PATH").unwrap_or_else(|_| "/".to_owned());
    let pools_spec =
        std::env::var("HYPEREVM_SMOKE_POOLS").unwrap_or_else(|_| DEFAULT_POOLS.to_owned());
    let secs: u64 = std::env::var("HYPEREVM_SMOKE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
        .min(900);
    let root = std::env::var("HYPEREVM_SMOKE_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("hyperevm-smoke")
            .to_string_lossy()
            .into_owned()
    });

    // The universe grammar, so the smoke speaks exactly what the boot does.
    let mut entries = Vec::new();
    for (i, e) in pools_spec.split(',').map(str::trim).enumerate() {
        let p = core_config::universe::parse_hyperevm_pool(e)
            .unwrap_or_else(|| panic!("bad pool entry `{e}`"));
        entries.push(ingress_hyperevm::PoolEntry {
            address: p.address,
            sym: make_symbol_id(VenueId::HyperEvm, i as u32 + 1),
            family: match p.family {
                core_config::universe::HyperEvmFamily::V3 => {
                    ingress_hyperevm::PoolFamily::UniswapV3
                }
                core_config::universe::HyperEvmFamily::Slipstream => {
                    ingress_hyperevm::PoolFamily::Slipstream
                }
                core_config::universe::HyperEvmFamily::Algebra => {
                    ingress_hyperevm::PoolFamily::Algebra
                }
            },
            dec0: p.dec0,
            dec1: p.dec1,
        });
    }
    let n = entries.len();
    let table = ingress_hyperevm::PoolTable::new(&entries).expect("pool table");

    let tls = TlsTransport::default_client_config();
    let ep = cli::WssEndpoint::resolve(&host, 443, &path).expect("resolve");
    let (run_dir, epoch_ns) = cli::new_capture_run_dir(&root).expect("run dir");
    let (prod, mut cons) = Ring::<core_types::Signal, { engine::POOL_RING_SIZE }>::new().split();
    let status = Arc::new(IngressStatus::new());
    let handle = cli::spawn_hyperevm(
        ep,
        tls,
        prod,
        status.clone(),
        10,
        &run_dir,
        epoch_ns,
        TapCfg {
            mode: TapMode::All,
            budget_bytes: 256 * 1024 * 1024,
        },
        None,
        table,
        ingress_hyperevm::run_loop::DEFAULT_SNAPSHOT_RADIUS,
    )
    .expect("spawn_hyperevm");

    // ---- drain into the book the paper matcher keeps ----
    let mut book = core_fill::AmmBook::new();
    let mut heads = 0u64;
    let mut first_live: Vec<Option<Duration>> = vec![None; n];
    let mut signals = 0u64;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let mut idle = true;
        while let Some(s) = cons.try_pop_ref().as_deref().copied() {
            idle = false;
            signals += 1;
            assert_eq!(s.source, core_types::SignalSource::HyperEvm as u8);
            match book.observe(s.sym, &s.payload) {
                core_fill::AmmObs::Head { .. } => heads += 1,
                core_fill::AmmObs::Pool { index } if index < n && book.is_live(index) => {
                    if first_live[index].is_none() {
                        first_live[index] = Some(t0.elapsed());
                    }
                }
                _ => {}
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    cli::signal_shutdown();
    handle.join().expect("ingress thread joins");

    // ---- report ----
    println!(
        "\nwindow {secs} s · host {host}{path} · run dir {}",
        run_dir.display()
    );
    println!(
        "status: msgs={} ticks={} parse_errors={} reconnects={} ring_drops={} · signals={signals} heads={heads}",
        status.msgs_total(),
        status.ticks_total(),
        status.parse_errors_total(),
        status.reconnects_total(),
        status.ring_drops_total(),
    );
    let c = book.counters;
    println!(
        "book: events={} refused={} snapshots={} fees_observed={} stale_marks={}",
        c.events, c.refused, c.snapshots, c.fees_observed, c.stale_marks
    );
    let mut live = 0usize;
    for (i, e) in entries.iter().enumerate() {
        let addr: String = e.address.iter().map(|b| format!("{b:02x}")).collect();
        println!(
            "pool 0x{addr}: live={} first_live={:?} mid_1e6={:?} judged_fee_pips={:?}",
            book.is_live(i),
            first_live[i],
            book.mid_1e6(i),
            book.judged_fee(i),
        );
        if first_live[i].is_some() {
            live += 1;
        }
    }
    assert!(heads > 0, "no head arrived in {secs} s");
    assert_eq!(c.refused, 0, "the book refused a payload the ingress wrote");
    assert_eq!(
        live, n,
        "every pool must complete a snapshot (archive probe + decimals)"
    );
}
