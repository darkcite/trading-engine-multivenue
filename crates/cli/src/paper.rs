//! Paper-mode orchestration. Wires the four ingress run-loops into
//! dedicated threads, pins each to its own core, owns the lock-free
//! SPSC rings, and runs a drain-and-count consumer on the main
//! thread that emits a tick/signal summary every 5 s.
//!
//! There is deliberately **no** strategy / dispatcher / signer
//! wiring here. Paper mode exists to validate that the four ingress
//! pipelines stay green under live network conditions before we
//! attach a strategy. That work is Phase 2.
//!
//! ## Thread topology
//!
//! | Thread | Role |
//! |--------|------|
//! | main   | drain-and-count consumers + 5 s log timer + reverse-order join |
//! | T1     | ingress-polymarket (CLOB WSS) |
//! | T2     | ingress-binance (bookTicker WSS) |
//! | T3     | ingress-rpc (Polygon JSON-RPC WSS) |
//! | T4     | ingress-rss (HTTPS polling) |
//!
//! Cores 0..=4 are pinned (Linux only). On non-Linux we log a
//! single warning and let the OS scheduler do as it pleases.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clob_dispatcher::{OrderDispatch, PaperDispatcher};
// LiveDispatcher is re-exported through cli::paper so the binary
// doesn't have to depend on clob-dispatcher directly.
pub use clob_dispatcher::{LiveDispatcher, LiveDispatcherErr};
use core_net::TlsTransport;
use core_ring::{Consumer, Producer, Ring};
use core_time::now_ns;
use core_types::{Fill, Signal, SymbolId, Tick};
use engine::{Engine, FILL_RING_SIZE, SIGNAL_RING_SIZE};
use rustls_pki_types::ServerName;
use strategy_latency_arb::LatencyArb;

use ingress_binance::run_loop as bwl;
use ingress_polymarket::run_loop as pwl;
use ingress_rpc::run_loop as rwl;

use crate::pinning::pin_current_thread_to_core;
use crate::sigint::{shutdown_requested, SHUTDOWN};

/// Spawn a thread and abort the cli boot with a useful diagnostic
/// if the OS refused. Common refusal modes on Linux:
///
///   * `EAGAIN` / `ENOMEM` — process thread limit reached (raise
///     `RLIMIT_NPROC`, check `/proc/sys/kernel/threads-max`)
///   * `EPERM`             — capabilities / cgroup PIDs limit
///
/// On macOS the error message is less specific but the same
/// general guidance applies.
fn spawn_or_die(builder: thread::Builder, name: &'static str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    match builder.spawn(f) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                thread = name,
                error = ?e,
                "thread spawn failed — check RLIMIT_NPROC / threads-max"
            );
            panic!("spawn ingress thread {name} failed: {e}");
        }
    }
}

// rustls is re-exported through core_net.
type RustlsConfig = std::sync::Arc<rustls::ClientConfig>;

/// Cadence at which the main-thread drain loop logs ring counters.
const REPORT_PERIOD_NS: u64 = 5_000_000_000;

/// Sleep between reconnect attempts when the transport fails. Keeps
/// failure storms from hammering the upstream during outages.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Maximum number of items the main thread drains per ring per
/// iteration. Bounded so a backed-up ring can't starve the others.
const DRAIN_BATCH: usize = 256;

// ---------------------------------------------------------------
// Endpoint config — boot-time strings; never touched on hot path
// ---------------------------------------------------------------

/// Endpoint config for a single WSS ingress. All strings are owned
/// + boxed because they have to outlive the ingress thread.
#[derive(Debug, Clone)]
pub struct WssEndpoint {
    /// Hostname (for SNI + the `Host:` header).
    pub host: String,
    /// HTTP/WS request-line path (e.g. `/ws/btcusdt@bookTicker`).
    pub path: String,
    /// Resolved socket address. We resolve once at boot — no DNS in
    /// the hot path.
    pub addr: SocketAddr,
}

impl WssEndpoint {
    /// Resolve `host:port` and stamp `path`. Returns an error if DNS
    /// returned no records or the host string was malformed.
    pub fn resolve(host: &str, port: u16, path: &str) -> io::Result<Self> {
        let mut iter = (host, port).to_socket_addrs()?;
        let addr = iter
            .next()
            .ok_or_else(|| io::Error::other(format!("dns: no records for {host}")))?;
        Ok(Self {
            host: host.to_string(),
            path: path.to_string(),
            addr,
        })
    }
}

// ---------------------------------------------------------------
// Ring + Producer/Consumer alias bundles
// ---------------------------------------------------------------

/// Every ring the cli owns at boot. Sizes are pulled from the
/// ingress + engine crates so the cli doesn't restate them; the
/// `const` equality below is structural — the compiler enforces
/// that the engine and ingress agree.
const _: () = {
    assert!(pwl::DEFAULT_TICK_RING_CAP == engine::PM_TICK_RING_SIZE);
    assert!(bwl::DEFAULT_TICK_RING_CAP == engine::BN_TICK_RING_SIZE);
    assert!(rwl::DEFAULT_SIGNAL_RING_CAP == SIGNAL_RING_SIZE);
};

/// All preallocated rings the engine + cli touch.
pub struct Rings {
    /// Tick ring for the Polymarket CLOB.
    pub polymarket_tick: Arc<Ring<Tick, { pwl::DEFAULT_TICK_RING_CAP }>>,
    /// Tick ring for Binance bookTicker.
    pub binance_tick: Arc<Ring<Tick, { bwl::DEFAULT_TICK_RING_CAP }>>,
    /// Signal ring for Polygon newHeads — feeds the engine.
    pub rpc_signal: Arc<Ring<Signal, SIGNAL_RING_SIZE>>,
    /// Signal ring for RSS news items — drained at cli level (the
    /// strategy doesn't react to RSS in Phase 2).
    pub rss_signal: Arc<Ring<Signal, 1024>>,
    /// Fill ring — the paper dispatcher never produces fills in
    /// Phase 2 but the engine still consumes one so the structural
    /// shape matches the live-trading path.
    pub fill: Arc<Ring<Fill, FILL_RING_SIZE>>,
}

impl Rings {
    /// Allocate all rings. Single call; never used on hot path.
    pub fn new() -> Self {
        Self {
            polymarket_tick: Ring::new(),
            binance_tick: Ring::new(),
            rpc_signal: Ring::new(),
            rss_signal: Ring::new(),
            fill: Ring::new(),
        }
    }
}

impl Default for Rings {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Thread bootstrappers
// ---------------------------------------------------------------

/// Spawn the Polymarket CLOB ingress thread. `producer` is the SPSC
/// producer half of the tick ring; the consumer half stays on the
/// main thread. Returns a [`JoinHandle`] the caller will join in
/// reverse boot order during shutdown.
pub fn spawn_polymarket(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    symbol_map: pwl::SymbolMap,
    mut producer: Producer<Tick, { pwl::DEFAULT_TICK_RING_CAP }>,
    core_id: usize,
) -> JoinHandle<()> {
    spawn_or_die(
        thread::Builder::new().name("ingress-polymarket".into()),
        "ingress-polymarket",
        move || {
            log_pin_outcome("polymarket", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "polymarket: bad server name");
                    return;
                }
            };

            let mut driver = pwl::Driver::new(now_ns());
            while !shutdown_requested() {
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "polymarket: connect failed");
                        backoff();
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "polymarket: mio init failed");
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());

                let res = pwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &symbol_map,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                );
                tracing::info!(?res, "polymarket: run-loop returned");
                if matches!(res, pwl::RunResult::Stopped) {
                    return;
                }
                backoff();
            }
        },
    )
}

/// Spawn the Binance bookTicker ingress thread. One thread per
/// symbol — caller spawns N of these if they want multi-symbol
/// coverage.
pub fn spawn_binance(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    sym: core_types::SymbolId,
    mut producer: Producer<Tick, { bwl::DEFAULT_TICK_RING_CAP }>,
    core_id: usize,
) -> JoinHandle<()> {
    spawn_or_die(
        thread::Builder::new().name(format!("ingress-binance-{sym}")),
        "ingress-binance",
        move || {
            log_pin_outcome("binance", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "binance: bad server name");
                    return;
                }
            };

            let mut driver = bwl::Driver::new(now_ns(), sym);
            while !shutdown_requested() {
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "binance: connect failed");
                        backoff();
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "binance: mio init failed");
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());

                let res = bwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                );
                tracing::info!(?res, "binance: run-loop returned");
                if matches!(res, bwl::RunResult::Stopped) {
                    return;
                }
                backoff();
            }
        },
    )
}

/// Spawn the Polygon JSON-RPC ingress thread.
pub fn spawn_rpc(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    mut producer: Producer<Signal, { rwl::DEFAULT_SIGNAL_RING_CAP }>,
    core_id: usize,
) -> JoinHandle<()> {
    spawn_or_die(
        thread::Builder::new().name("ingress-rpc".into()),
        "ingress-rpc",
        move || {
            log_pin_outcome("rpc", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "rpc: bad server name");
                    return;
                }
            };

            let mut driver = rwl::Driver::new(now_ns());
            while !shutdown_requested() {
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "rpc: connect failed");
                        backoff();
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "rpc: mio init failed");
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());

                let res = rwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                );
                tracing::info!(?res, "rpc: run-loop returned");
                if matches!(res, rwl::RunResult::Stopped) {
                    return;
                }
                backoff();
            }
        },
    )
}

/// One parsed `RSS_FEEDS` entry. URL is pre-split into host/path
/// at boot so the RSS thread never needs to re-tokenise.
#[derive(Debug, Clone)]
pub struct RssFeed {
    /// DNS name. Owned so the spawned thread outlives the cli.
    pub host: String,
    /// HTTP request path (e.g. `/rss`).
    pub path: String,
    /// Poll interval between fetches (nanoseconds).
    pub poll_interval_ns: u64,
}

impl RssFeed {
    /// Parse an `https://host/path` URL into `RssFeed`. The poller is
    /// HTTPS-only; non-`https` schemes are rejected.
    pub fn parse(url: &str, poll_interval_ns: u64) -> Result<Self, &'static str> {
        let trimmed = url.trim();
        let rest = trimmed
            .strip_prefix("https://")
            .ok_or("rss feed URL must be https://")?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if host.is_empty() {
            return Err("rss feed URL has empty host");
        }
        Ok(Self {
            host: host.to_string(),
            path: path.to_string(),
            poll_interval_ns,
        })
    }
}

/// Spawn the RSS ingress thread. Polls each `feeds` entry on its
/// own schedule and pushes `Signal`s onto `producer`.
///
/// Unlike the WSS ingresses each fetch opens a fresh TLS
/// connection — the RSS protocol is request/response so there's
/// no long-lived socket to hold. The connect-cost per fetch is
/// fine because polls happen at minutes-scale, not microseconds.
pub fn spawn_rss(
    feeds: Vec<RssFeed>,
    tls_config: RustlsConfig,
    mut producer: Producer<Signal, 1024>,
    core_id: usize,
) -> JoinHandle<()> {
    spawn_or_die(
        thread::Builder::new().name("ingress-rss".into()),
        "ingress-rss",
        move || {
            log_pin_outcome("rss", core_id);
            if feeds.is_empty() {
                tracing::info!("rss: no feeds configured — thread exiting");
                return;
            }
            // Build FeedCfg slice once. `feeds` is owned by this
            // closure so the &[u8] borrows live the whole thread.
            let cfgs: Vec<ingress_rss::poller::FeedCfg<'_>> = feeds
                .iter()
                .map(|f| {
                    ingress_rss::poller::FeedCfg::new(
                        f.host.as_bytes(),
                        f.path.as_bytes(),
                        f.poll_interval_ns,
                    )
                })
                .collect();
            let mut schedules: Vec<ingress_rss::poller::FeedSchedule> =
                cfgs.iter()
                    .map(|_| ingress_rss::poller::FeedSchedule::immediate())
                    .collect();
            let mut drv = ingress_rss::poller::FetchDriver::new();
            let mut seen: ingress_rss::SeenRing<256> = ingress_rss::SeenRing::new();

            // Per-fetch TLS connect closure. DNS resolution happens
            // every fetch — acceptable at minutes cadence.
            let tls_cfg = tls_config.clone();
            let connect = move |cfg: &ingress_rss::poller::FeedCfg<'_>| -> std::io::Result<TlsTransport> {
                let host = match std::str::from_utf8(cfg.host) {
                    Ok(s) => s,
                    Err(_) => {
                        return Err(std::io::Error::other("rss: feed host not valid UTF-8"));
                    }
                };
                let addr = match (host, 443u16).to_socket_addrs() {
                    Ok(mut it) => match it.next() {
                        Some(a) => a,
                        None => {
                            return Err(std::io::Error::other(format!(
                                "rss: dns: no records for {host}"
                            )));
                        }
                    },
                    Err(e) => return Err(e),
                };
                let server_name = TlsTransport::server_name_from_host(host)
                    .map_err(|_| std::io::Error::other("rss: bad server name"))?;
                TlsTransport::connect(addr, server_name, tls_cfg.clone())
            };

            if let Err(e) = ingress_rss::poller::run::<TlsTransport, _, 256, 1024>(
                connect,
                &cfgs,
                &mut schedules,
                &mut drv,
                &mut seen,
                &mut producer,
                &SHUTDOWN,
            ) {
                tracing::error!(error = ?e, "rss: poller::run returned error");
            }
            tracing::info!("rss: ingress thread exiting");
        },
    )
}

// ---------------------------------------------------------------
// Drain-and-count consumer (main thread)
// ---------------------------------------------------------------

/// Per-ring observed counters reset every 5 s.
#[derive(Default, Debug, Clone, Copy)]
pub struct DrainCounters {
    /// Polymarket ticks observed.
    pub polymarket_ticks: u64,
    /// Binance ticks observed.
    pub binance_ticks: u64,
    /// RPC signals observed.
    pub rpc_signals: u64,
    /// RSS signals observed.
    pub rss_signals: u64,
}

impl DrainCounters {
    /// Add another reading.
    #[inline]
    pub fn add(&mut self, other: &Self) {
        self.polymarket_ticks += other.polymarket_ticks;
        self.binance_ticks += other.binance_ticks;
        self.rpc_signals += other.rpc_signals;
        self.rss_signals += other.rss_signals;
    }
}

/// Consumer-side handles passed to the drain loop. Created from
/// `Ring::split()`; the producer ends went to ingress threads.
pub struct Consumers {
    /// Polymarket tick consumer.
    pub polymarket_tick: Consumer<Tick, { pwl::DEFAULT_TICK_RING_CAP }>,
    /// Binance tick consumer.
    pub binance_tick: Consumer<Tick, { bwl::DEFAULT_TICK_RING_CAP }>,
    /// RPC signal consumer.
    pub rpc_signal: Consumer<Signal, SIGNAL_RING_SIZE>,
    /// RSS signal consumer (cli-level drain).
    pub rss_signal: Consumer<Signal, 1024>,
}

/// Drain-and-count loop. Runs on the main thread until
/// [`SHUTDOWN`](crate::sigint::SHUTDOWN) is raised. Emits one
/// `info!` line every [`REPORT_PERIOD_NS`] with cumulative counters.
///
/// Returns the final counter snapshot when the loop exits.
pub fn drain_and_count_loop(mut cons: Consumers) -> DrainCounters {
    let mut total = DrainCounters::default();
    let mut period = DrainCounters::default();
    let mut next_report = now_ns() + REPORT_PERIOD_NS;

    while !shutdown_requested() {
        // Drain each ring in fixed-size batches; bounded so one ring
        // can't monopolise the main thread.
        for _ in 0..DRAIN_BATCH {
            if cons.polymarket_tick.try_pop().is_some() {
                period.polymarket_ticks += 1;
            } else {
                break;
            }
        }
        for _ in 0..DRAIN_BATCH {
            if cons.binance_tick.try_pop().is_some() {
                period.binance_ticks += 1;
            } else {
                break;
            }
        }
        for _ in 0..DRAIN_BATCH {
            if cons.rpc_signal.try_pop().is_some() {
                period.rpc_signals += 1;
            } else {
                break;
            }
        }
        for _ in 0..DRAIN_BATCH {
            if cons.rss_signal.try_pop().is_some() {
                period.rss_signals += 1;
            } else {
                break;
            }
        }

        let now = now_ns();
        if now >= next_report {
            total.add(&period);
            tracing::info!(
                pm_ticks = period.polymarket_ticks,
                bn_ticks = period.binance_ticks,
                rpc_sigs = period.rpc_signals,
                rss_sigs = period.rss_signals,
                "5s ring summary"
            );
            period = DrainCounters::default();
            next_report = now + REPORT_PERIOD_NS;
        }

        // 1 ms park is plenty for paper mode — we're measuring, not
        // trading. Production drain pipeline (Phase 2) doesn't park.
        thread::sleep(Duration::from_millis(1));
    }

    total.add(&period);
    total
}

// ---------------------------------------------------------------
// Engine loop — real strategy wired to the dispatcher
// ---------------------------------------------------------------

/// Slot capacity for the Phase 2 strategy table. Holds at most `N`
/// symbol pairs (one Polymarket book + one Binance reference per
/// pair). `8` is plenty for v1; bump and recompile when we widen
/// coverage.
pub const STRATEGY_SLOTS: usize = 8;

/// A symbol pair to register with [`LatencyArb`] at boot.
#[derive(Copy, Clone, Debug)]
pub struct StrategyPair {
    /// Polymarket SymbolId (must match the run-loop's SymbolMap).
    pub polymarket: SymbolId,
    /// Binance SymbolId (must match the Binance ingress driver).
    pub binance: SymbolId,
}

/// Boot config for the engine loop. Read from CLI args or a config
/// file in the cli binary.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Symbol pairs to register. At least one is required.
    pub pairs: Vec<StrategyPair>,
    /// Trigger threshold in 1e6 fixed-point units.
    pub threshold_1e6: i64,
    /// Per-order quantity in 1e6 fixed-point units.
    pub qty_1e6: i64,
    /// Cooldown between emits per market (ns).
    pub cooldown_ns: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            pairs: Vec::new(),
            threshold_1e6: strategy_latency_arb::DEFAULT_THRESHOLD_1E6,
            qty_1e6: strategy_latency_arb::DEFAULT_QTY.raw(),
            cooldown_ns: strategy_latency_arb::DEFAULT_COOLDOWN_NS,
        }
    }
}

/// Run the real engine loop with a [`PaperDispatcher`]. Default
/// `--paper` entry point — builds a `LatencyArb` strategy.
pub fn engine_loop(cons: Consumers, cfg: EngineConfig) -> EngineLoopResult {
    let disp = PaperDispatcher::new();
    engine_loop_with(cons, cfg, disp)
}

/// Run the EV strategy (Strategy A) over the paper dispatcher.
///
/// `artifact_path` points at a `claude-worker`-emitted NDJSON file
/// (one tag per line). Boot fails fast if the file can't be loaded
/// or no symbol pairs are configured.
pub fn engine_loop_ev_paper(
    cons: Consumers,
    cfg: EngineConfig,
    artifact_path: &std::path::Path,
) -> EngineLoopResult {
    let disp = PaperDispatcher::new();
    engine_loop_ev_full(cons, cfg, disp, Observability::default(), artifact_path)
}

/// EV strategy with observability and a caller-chosen dispatcher.
/// Pairs in [`EngineConfig`] are interpreted as Polymarket symbols
/// only; the Binance leg is ignored.
pub fn engine_loop_ev_full<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
    obs: Observability,
    artifact_path: &std::path::Path,
) -> EngineLoopResult {
    if cfg.pairs.is_empty() {
        return EngineLoopResult::Failed("engine_loop: no symbol pairs configured");
    }
    let (table, skipped) =
        match research_artifacts::ArtifactTable::<STRATEGY_SLOTS>::load_ndjson(artifact_path) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = ?e, path = %artifact_path.display(), "ev: load_ndjson failed");
                return EngineLoopResult::Failed("engine_loop_ev: artifact load failed");
            }
        };
    tracing::info!(
        loaded = table.len(),
        skipped,
        path = %artifact_path.display(),
        "ev: loaded artifact table"
    );

    let mut strat: strategy_ev::EvStrategy<STRATEGY_SLOTS> = strategy_ev::EvStrategy::new();
    strat.set_threshold(cfg.threshold_1e6);
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);
    // Move the loaded table in by swap.
    *strat.table_mut() = table;
    // Register each Polymarket symbol. The asset-id key is the
    // SymbolId encoded as decimal ASCII for v1 — matches what the
    // claude-worker artifacts will use when SymbolId is the
    // canonical key. Phase 5.1 introduces a richer mapping.
    for p in &cfg.pairs {
        let mut buf = [0u8; 32];
        let n = format_u64_into(&mut buf, p.polymarket as u64);
        if let Err(e) = strat.register(p.polymarket, n) {
            tracing::error!(error = ?e, sym = p.polymarket, "ev: register failed");
            return EngineLoopResult::Failed("engine_loop_ev: register rejected");
        }
    }

    run_engine_loop(cons, disp, strat, obs)
}

/// Run Strategy C (cross-market arbitrage). `groups` is a flat
/// `&[&[SymbolId]]`, one group per outer slice. Up to 8 groups of
/// up to 8 members each.
pub fn engine_loop_cross_arb_full<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
    obs: Observability,
    groups: &[&[core_types::SymbolId]],
) -> EngineLoopResult {
    if groups.is_empty() {
        return EngineLoopResult::Failed("engine_loop_cross_arb: no groups configured");
    }
    let mut strat: strategy_cross_arb::CrossArb<8, 8> = strategy_cross_arb::CrossArb::new();
    strat.set_threshold(cfg.threshold_1e6);
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);
    for g in groups {
        if let Err(e) = strat.register_group(g) {
            tracing::error!(error = ?e, "cross-arb: register_group failed");
            return EngineLoopResult::Failed("engine_loop_cross_arb: register rejected");
        }
    }
    tracing::info!(groups = groups.len(), "cross-arb: registered groups");
    run_engine_loop(cons, disp, strat, obs)
}

/// Run Strategy D (rule-tree). `rules_path` points at a JSON-
/// array file as emitted by `claude-worker/rule_parser.py`. Each
/// rule's first 16 ASCII bytes of `trigger` are used as the
/// keyword; the cli passes a `(sym_for_rule)` table to map each
/// rule name to a Polymarket SymbolId.
pub fn engine_loop_rule_tree_full<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
    obs: Observability,
    rules_path: &std::path::Path,
    sym_for_rule: &[(core_types::SymbolId, [u8; 16], u8)],
) -> EngineLoopResult {
    let (rules, skipped) = match research_artifacts::RulesTable::<8>::load_json(rules_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = ?e, path = %rules_path.display(), "rule-tree: load failed");
            return EngineLoopResult::Failed("engine_loop_rule_tree: rules load failed");
        }
    };
    tracing::info!(
        loaded = rules.len(),
        skipped,
        path = %rules_path.display(),
        "rule-tree: loaded rules"
    );
    if rules.is_empty() {
        return EngineLoopResult::Failed("engine_loop_rule_tree: rules file is empty");
    }
    if sym_for_rule.is_empty() {
        return EngineLoopResult::Failed("engine_loop_rule_tree: no symbol mapping provided");
    }
    let mut strat: strategy_rule_tree::RuleTree<8> = strategy_rule_tree::RuleTree::new();
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);

    for (mapping_idx, r) in rules.slice().iter().enumerate() {
        if mapping_idx >= sym_for_rule.len() {
            break;
        }
        let (sym, kw, kw_len) = sym_for_rule[mapping_idx];
        if let Err(e) = strat.add_rule(*r, sym, &kw[..kw_len as usize]) {
            tracing::error!(error = ?e, "rule-tree: add_rule failed");
            return EngineLoopResult::Failed("engine_loop_rule_tree: add_rule rejected");
        }
    }

    run_engine_loop(cons, disp, strat, obs)
}

fn format_u64_into(buf: &mut [u8; 32], mut v: u64) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

impl Observability {
    /// Build the registry + snapshot cell. Both are `None` by
    /// default; flip `enable_metrics` / `enable_tui` to populate.
    /// Boot-only; allocates once.
    pub fn build(enable_metrics: bool, enable_tui: bool) -> Result<Self, &'static str> {
        let mut out = Observability::default();
        if enable_metrics {
            let mut reg = core_metrics::MetricsRegistry::new();
            let ticks = reg
                .register_counter("engine_ticks_total")
                .map_err(|_| "register engine_ticks_total")?;
            let signals = reg
                .register_counter("engine_signals_total")
                .map_err(|_| "register engine_signals_total")?;
            let orders_emitted = reg
                .register_counter("engine_orders_emitted_total")
                .map_err(|_| "register engine_orders_emitted_total")?;
            let orders_dropped = reg
                .register_counter("engine_orders_dropped_total")
                .map_err(|_| "register engine_orders_dropped_total")?;
            let rss_signals = reg
                .register_counter("engine_rss_signals_total")
                .map_err(|_| "register engine_rss_signals_total")?;
            let ingest_p50_ns = reg
                .register_gauge("engine_latency_ingest_p50_ns")
                .map_err(|_| "register engine_latency_ingest_p50_ns")?;
            let ingest_p99_ns = reg
                .register_gauge("engine_latency_ingest_p99_ns")
                .map_err(|_| "register engine_latency_ingest_p99_ns")?;
            let decide_p50_ns = reg
                .register_gauge("engine_latency_decide_p50_ns")
                .map_err(|_| "register engine_latency_decide_p50_ns")?;
            let decide_p99_ns = reg
                .register_gauge("engine_latency_decide_p99_ns")
                .map_err(|_| "register engine_latency_decide_p99_ns")?;
            let ack_p50_ns = reg
                .register_gauge("engine_latency_ack_p50_ns")
                .map_err(|_| "register engine_latency_ack_p50_ns")?;
            let ack_p99_ns = reg
                .register_gauge("engine_latency_ack_p99_ns")
                .map_err(|_| "register engine_latency_ack_p99_ns")?;
            let strategy_latency_arb = reg
                .register_gauge("engine_strategy_latency_arb_active")
                .map_err(|_| "register engine_strategy_latency_arb_active")?;
            let strategy_ev = reg
                .register_gauge("engine_strategy_ev_active")
                .map_err(|_| "register engine_strategy_ev_active")?;
            let strategy_cross_arb = reg
                .register_gauge("engine_strategy_cross_arb_active")
                .map_err(|_| "register engine_strategy_cross_arb_active")?;
            let strategy_rule_tree = reg
                .register_gauge("engine_strategy_rule_tree_active")
                .map_err(|_| "register engine_strategy_rule_tree_active")?;
            let ingress_polymarket_state = reg
                .register_gauge("engine_ingress_polymarket_state")
                .map_err(|_| "register engine_ingress_polymarket_state")?;
            let ingress_binance_state = reg
                .register_gauge("engine_ingress_binance_state")
                .map_err(|_| "register engine_ingress_binance_state")?;
            let ingress_rpc_state = reg
                .register_gauge("engine_ingress_rpc_state")
                .map_err(|_| "register engine_ingress_rpc_state")?;
            let ingress_rss_state = reg
                .register_gauge("engine_ingress_rss_state")
                .map_err(|_| "register engine_ingress_rss_state")?;
            let max_tick_age_ns = reg
                .register_gauge("engine_max_tick_age_ns")
                .map_err(|_| "register engine_max_tick_age_ns")?;
            // Per-bucket tick-age gauges. Names follow the
            // `engine_tick_age_ns_b<NN>` pattern — Prometheus-
            // compatible identifiers (no labels in v1; the
            // registry's name table is fixed-size byte arrays, no
            // label support yet).
            let mut tick_age_ns_per_bucket =
                [core_metrics::GaugeId::default(); engine::SYM_BUCKETS];
            let mut name_buf = [0u8; 64];
            for (b, slot) in tick_age_ns_per_bucket.iter_mut().enumerate() {
                // Format `engine_tick_age_ns_bNN` into a stack
                // buffer; no heap allocation. Two-digit zero pad
                // keeps Prometheus label-order stable when listed.
                let prefix = b"engine_tick_age_ns_b";
                name_buf[..prefix.len()].copy_from_slice(prefix);
                let tens = (b / 10) as u8;
                let ones = (b % 10) as u8;
                name_buf[prefix.len()] = b'0' + tens;
                name_buf[prefix.len() + 1] = b'0' + ones;
                let n = prefix.len() + 2;
                let name = std::str::from_utf8(&name_buf[..n])
                    .map_err(|_| "tick_age_ns_b name utf8")?;
                *slot = reg
                    .register_gauge(name)
                    .map_err(|_| "register engine_tick_age_ns_bNN")?;
            }
            out.metrics = Some(Arc::new(reg));
            out.counter_ids = Some(EngineCounters {
                ticks,
                signals,
                orders_emitted,
                orders_dropped,
                rss_signals,
                ingest_p50_ns,
                ingest_p99_ns,
                decide_p50_ns,
                decide_p99_ns,
                ack_p50_ns,
                ack_p99_ns,
                strategy_latency_arb,
                strategy_ev,
                strategy_cross_arb,
                strategy_rule_tree,
                ingress_polymarket_state,
                ingress_binance_state,
                ingress_rpc_state,
                ingress_rss_state,
                max_tick_age_ns,
                tick_age_ns_per_bucket,
            });
        }
        if enable_tui {
            out.snapshot = Some(Arc::new(tui::SnapshotCell::new()));
        }
        Ok(out)
    }
}

/// Periodic HdrHistogram dump config. When wired into
/// [`Observability`], the engine loop writes the three
/// `LatencyTracker` histograms (ingest/decide/ack) to a fresh file
/// inside `dir` every `interval_ns`. Disabled when `interval_ns` is 0.
///
/// File naming: `latency_<unix_ns>.hgrm`. Directory is created on
/// first dump if missing. Each dump is a fresh file so an operator
/// gets a trend across the run rather than a single overwritten file.
#[derive(Debug, Clone)]
pub struct LatencyDump {
    /// Destination directory. Created on first dump if missing.
    pub dir: PathBuf,
    /// Dump cadence in nanoseconds. Zero disables dumping.
    pub interval_ns: u64,
}

impl LatencyDump {
    /// Build a [`LatencyDump`] from a directory + period-in-seconds.
    /// Returns `None` when `seconds` is 0 (caller treats as disabled).
    pub fn from_secs(dir: PathBuf, seconds: u64) -> Option<Self> {
        if seconds == 0 {
            return None;
        }
        Some(Self {
            dir,
            interval_ns: seconds.saturating_mul(1_000_000_000),
        })
    }
}

/// Optional observability surfaces wired around the engine loop.
/// Build once at boot via [`Observability::build`] and hand the
/// owned `Arc`s into [`engine_loop_full`] — the loop publishes
/// counters + dashboard snapshots into them.
#[derive(Default)]
pub struct Observability {
    /// Live metrics registry (counters + gauges). `None` if the
    /// `/metrics` server is disabled.
    pub metrics: Option<Arc<core_metrics::MetricsRegistry>>,
    /// Counter handles the engine loop bumps inline.
    pub counter_ids: Option<EngineCounters>,
    /// Snapshot cell the TUI thread reads. `None` if `--tui` is
    /// disabled.
    pub snapshot: Option<Arc<tui::SnapshotCell>>,
    /// Periodic HdrHistogram dump config. `None` disables dumping.
    pub latency_dump: Option<LatencyDump>,
}

impl Observability {
    /// Attach a [`LatencyDump`] config. Boot-only; called from the
    /// cli after [`Observability::build`]. Returns `self` so it can
    /// be chained.
    pub fn with_latency_dump(mut self, dump: Option<LatencyDump>) -> Self {
        self.latency_dump = dump;
        self
    }
}

/// CounterId + GaugeId handles for the engine-loop hot path. Built
/// once at boot inside `Observability::build`.
#[derive(Copy, Clone, Debug)]
pub struct EngineCounters {
    /// Total PM + BN ticks dispatched.
    pub ticks: core_metrics::CounterId,
    /// Total RPC signals dispatched.
    pub signals: core_metrics::CounterId,
    /// Orders emitted via `ctx.submit`.
    pub orders_emitted: core_metrics::CounterId,
    /// Orders the dispatcher rejected.
    pub orders_dropped: core_metrics::CounterId,
    /// RSS items drained at the cli level.
    pub rss_signals: core_metrics::CounterId,
    /// p50 ingest→strategy latency (ns).
    pub ingest_p50_ns: core_metrics::GaugeId,
    /// p99 ingest→strategy latency (ns).
    pub ingest_p99_ns: core_metrics::GaugeId,
    /// p50 strategy→submit latency (ns).
    pub decide_p50_ns: core_metrics::GaugeId,
    /// p99 strategy→submit latency (ns).
    pub decide_p99_ns: core_metrics::GaugeId,
    /// p50 submit→ack latency (ns).
    pub ack_p50_ns: core_metrics::GaugeId,
    /// p99 submit→ack latency (ns).
    pub ack_p99_ns: core_metrics::GaugeId,
    /// Active-strategy indicator — latency-arb (B).
    pub strategy_latency_arb: core_metrics::GaugeId,
    /// Active-strategy indicator — ev (A).
    pub strategy_ev: core_metrics::GaugeId,
    /// Active-strategy indicator — cross-arb (C).
    pub strategy_cross_arb: core_metrics::GaugeId,
    /// Active-strategy indicator — rule-tree (D).
    pub strategy_rule_tree: core_metrics::GaugeId,
    /// Per-ingress state gauge: Polymarket WSS.
    pub ingress_polymarket_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Binance bookTicker.
    pub ingress_binance_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Polygon JSON-RPC.
    pub ingress_rpc_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: RSS poller.
    pub ingress_rss_state: core_metrics::GaugeId,
    /// Maximum tick age across every observed symbol (ns).
    /// Spikes here surface a silenced market.
    pub max_tick_age_ns: core_metrics::GaugeId,
    /// Per-bucket tick-age gauges. Bucket index = `sym & (SYM_BUCKETS-1)`.
    /// Operators can pinpoint which exact bucket went silent
    /// instead of only seeing the across-buckets max.
    pub tick_age_ns_per_bucket: [core_metrics::GaugeId; engine::SYM_BUCKETS],
}

/// Generic engine loop: pass in any `OrderDispatch`. The `--live`
/// path constructs a [`LiveDispatcher`] and forwards to this fn.
pub fn engine_loop_with<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
) -> EngineLoopResult {
    engine_loop_full(cons, cfg, disp, Observability::default())
}

/// Full engine loop with observability plumbed in. Used by the
/// `--metrics`/`--tui` paths.
pub fn engine_loop_full<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
    obs: Observability,
) -> EngineLoopResult {
    if cfg.pairs.is_empty() {
        return EngineLoopResult::Failed("engine_loop: no symbol pairs configured");
    }

    // Build the strategy.
    let mut strat: LatencyArb<STRATEGY_SLOTS> = LatencyArb::new();
    strat.set_threshold(cfg.threshold_1e6);
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);
    for p in &cfg.pairs {
        if let Err(e) = strat.add_pair(p.polymarket, p.binance) {
            tracing::error!(error = ?e, pm = p.polymarket, bn = p.binance, "add_pair failed");
            return EngineLoopResult::Failed("engine_loop: add_pair rejected");
        }
    }

    run_engine_loop(cons, disp, strat, obs)
}

fn run_engine_loop<S, D>(
    cons: Consumers,
    disp: D,
    strat: S,
    obs: Observability,
) -> EngineLoopResult
where
    S: strategy_core::Strategy,
    D: OrderDispatch,
{
    let mut cons = cons;
    // The engine consumes Polymarket ticks, Binance ticks, RPC
    // signals, and a fill ring. Fills are emitted only by the live
    // dispatcher; we still need the consumer end so the type
    // shape matches.
    let fill_ring: Arc<Ring<Fill, FILL_RING_SIZE>> = Ring::new();
    let (_fill_prod, fill_cons) = fill_ring.split();

    let mut eng = Engine::new(
        strat,
        disp,
        cons.polymarket_tick,
        cons.binance_tick,
        cons.rpc_signal,
        fill_cons,
    );
    if let Err(e) = eng.start() {
        tracing::error!(error = ?e, "engine on_start failed");
        return EngineLoopResult::Failed("engine_loop: on_start failed");
    }

    let mut next_report = now_ns() + REPORT_PERIOD_NS;
    let mut rss_seen_period: u64 = 0;
    let mut rss_seen_total: u64 = 0;
    let mut last_ticks = 0u64;
    let mut last_signals = 0u64;
    let mut last_orders = 0u64;
    // Periodic HdrHistogram dump cadence. `next_dump_ns` is only
    // consulted when `obs.latency_dump.is_some()`.
    let mut next_dump_ns: u64 = match obs.latency_dump.as_ref() {
        Some(d) => now_ns().saturating_add(d.interval_ns),
        None => u64::MAX,
    };

    while !shutdown_requested() {
        eng.tick(DRAIN_BATCH);

        // RSS drain — cli-level only.
        for _ in 0..DRAIN_BATCH {
            if cons.rss_signal.try_pop().is_some() {
                rss_seen_period += 1;
            } else {
                break;
            }
        }

        let now = now_ns();
        if now >= next_report {
            let ticks = eng.ticks_dispatched;
            let signals = eng.signals_dispatched;
            let orders = strategy_core::StrategyCounters::orders_emitted(eng.strategy());
            let dropped = strategy_core::StrategyCounters::orders_dropped(eng.strategy());
            tracing::info!(
                pm_bn_ticks = ticks - last_ticks,
                rpc_sigs = signals - last_signals,
                rss_sigs = rss_seen_period,
                orders = orders - last_orders,
                dropped,
                iter = eng.iterations,
                "5s engine summary"
            );

            // Publish counter deltas + gauge snapshots into the
            // metrics registry, if one is wired up.
            if let (Some(reg), Some(ids)) = (obs.metrics.as_ref(), obs.counter_ids.as_ref()) {
                reg.counter(ids.ticks).inc(ticks - last_ticks);
                reg.counter(ids.signals).inc(signals - last_signals);
                reg.counter(ids.orders_emitted).inc(orders - last_orders);
                let total_dropped = dropped;
                let _ = total_dropped;
                reg.counter(ids.rss_signals).inc(rss_seen_period);
                // Latency gauges are full snapshots, not deltas.
                reg.gauge(ids.ingest_p50_ns).set(eng.ingest_p50_ns() as i64);
                reg.gauge(ids.ingest_p99_ns).set(eng.ingest_p99_ns() as i64);
                reg.gauge(ids.decide_p50_ns).set(eng.decide_p50_ns() as i64);
                reg.gauge(ids.decide_p99_ns).set(eng.decide_p99_ns() as i64);
                reg.gauge(ids.ack_p50_ns).set(eng.ack_p50_ns() as i64);
                reg.gauge(ids.ack_p99_ns).set(eng.ack_p99_ns() as i64);

                // Active-strategy gauges — flip exactly one to 1.
                let kind = strategy_core::StrategyCounters::strategy_kind(eng.strategy());
                reg.gauge(ids.strategy_latency_arb)
                    .set(if kind == "latency-arb" { 1 } else { 0 });
                reg.gauge(ids.strategy_ev)
                    .set(if kind == "ev" { 1 } else { 0 });
                reg.gauge(ids.strategy_cross_arb)
                    .set(if kind == "cross-arb" { 1 } else { 0 });
                reg.gauge(ids.strategy_rule_tree)
                    .set(if kind == "rule-tree" { 1 } else { 0 });

                // Per-ingress connection state — v1 heuristic:
                // assume up if iterations is advancing (consumer
                // is being driven). Phase 7 wires real per-thread
                // status. RSS is "up" once we've drained at least
                // one period of items.
                let up = i64::from(eng.iterations > 0);
                reg.gauge(ids.ingress_polymarket_state).set(up);
                reg.gauge(ids.ingress_binance_state).set(up);
                reg.gauge(ids.ingress_rpc_state).set(up);
                reg.gauge(ids.ingress_rss_state)
                    .set(i64::from(rss_seen_total > 0 || rss_seen_period > 0));

                // Max tick age — surfaces silenced markets.
                reg.gauge(ids.max_tick_age_ns)
                    .set(eng.max_tick_age_ns(now) as i64);
                // Per-bucket tick-age gauges. Iterate only
                // populated buckets — unpopulated buckets stay at
                // 0, which is the correct semantic ("we've never
                // seen this bucket, so it has no age").
                let mut mask = eng.populated_sym_mask();
                while mask != 0 {
                    let b = mask.trailing_zeros() as usize;
                    mask &= mask - 1;
                    reg.gauge(ids.tick_age_ns_per_bucket[b])
                        .set(eng.tick_age_ns_bucket(b, now) as i64);
                }
            }

            // Periodic HdrHistogram dump. Inline (not a separate
            // thread) so we don't have to share `Engine` across
            // threads. Allocation here is fine — we're off the hot
            // path; the 5s publish tick already does I/O.
            if let Some(dump) = obs.latency_dump.as_ref() {
                if now >= next_dump_ns {
                    if let Err(e) = dump_latency_histograms(dump, &eng, now) {
                        tracing::warn!(error = ?e, "latency dump failed");
                    }
                    next_dump_ns = now.saturating_add(dump.interval_ns);
                }
            }

            // Publish a dashboard snapshot if `--tui` wired one up.
            // Per-symbol top-of-book is strategy-specific; v1
            // leaves `recent_tob` empty and lets the dashboard
            // show ring counts + last order summary. Phase 5.1
            // will expose a generic `Strategy::book_snapshot()`
            // accessor.
            if let Some(cell) = obs.snapshot.as_ref() {
                let mut state = tui::DashboardState::empty();
                state.iterations = eng.iterations;
                state.ticks_dispatched = ticks;
                state.signals_dispatched = signals;
                state.orders_emitted = orders;
                state.orders_dropped = dropped;
                state.fills_seen = eng.fills_dispatched;
                // Wire engine-side LatencyTracker percentiles into
                // the dashboard. ix 0 = ingest→strategy,
                // 1 = strategy→submit, 2 = submit→ack.
                state.p50_ns[0] = eng.ingest_p50_ns();
                state.p99_ns[0] = eng.ingest_p99_ns();
                state.p50_ns[1] = eng.decide_p50_ns();
                state.p99_ns[1] = eng.decide_p99_ns();
                state.p50_ns[2] = eng.ack_p50_ns();
                state.p99_ns[2] = eng.ack_p99_ns();
                // Ingest health is best-effort for now: all four
                // threads assumed up if iterations is advancing.
                state.ingest_health = if eng.iterations > 0 { 0b1111 } else { 0 };
                cell.publish(state);
            }

            last_ticks = ticks;
            last_signals = signals;
            last_orders = orders;
            rss_seen_total = rss_seen_total.wrapping_add(rss_seen_period);
            rss_seen_period = 0;
            next_report = now + REPORT_PERIOD_NS;
        }

        // Phase 7-prep: `thread::sleep(1ms)` here was coupling
        // engine reactivity to the Linux scheduler quantum
        // (effective floor ~4-10 ms on non-RT kernels). The cli
        // pins this thread to its own core (see `pin_current_thread_to_core(0)`
        // in main) so a tight `yield_now` is cheap — it hands the
        // CPU to anything else that wants it, then the scheduler
        // returns to us promptly. Net effect: tick-to-decide
        // latency tracks the ingress producer cadence, not the
        // kernel HZ.
        std::thread::yield_now();
    }

    eng.stop();
    let total = EngineLoopResult::Done(EngineLoopStats {
        iterations: eng.iterations,
        ticks_dispatched: eng.ticks_dispatched,
        signals_dispatched: eng.signals_dispatched,
        rss_signals_drained: rss_seen_total.wrapping_add(rss_seen_period),
        orders_emitted: strategy_core::StrategyCounters::orders_emitted(eng.strategy()),
        orders_dropped: strategy_core::StrategyCounters::orders_dropped(eng.strategy()),
        dispatcher_accepted: eng.dispatcher().stats().accepted,
    });
    total
}

/// Cumulative engine-loop counters returned on exit.
#[derive(Debug, Clone, Copy, Default)]
pub struct EngineLoopStats {
    /// Number of `engine.tick()` calls.
    pub iterations: u64,
    /// Combined PM + BN ticks the engine dispatched.
    pub ticks_dispatched: u64,
    /// RPC signals the engine dispatched.
    pub signals_dispatched: u64,
    /// RSS signals drained at the cli level.
    pub rss_signals_drained: u64,
    /// Orders the strategy emitted via `ctx.submit`.
    pub orders_emitted: u64,
    /// Orders the dispatcher rejected (ring-full).
    pub orders_dropped: u64,
    /// Orders the dispatcher accepted (paper: always == emitted).
    pub dispatcher_accepted: u64,
}

/// Outcome of [`engine_loop`].
#[derive(Debug)]
pub enum EngineLoopResult {
    /// Clean shutdown via SIGINT; carries cumulative stats.
    Done(EngineLoopStats),
    /// Boot rejected for a static reason — caller should exit
    /// non-zero.
    Failed(&'static str),
}

/// Write all three engine LatencyTracker histograms to a fresh
/// file in `dump.dir`. File name is `latency_<unix_ns>.hgrm`.
/// Caller is the 5s publish tick — off the hot path; allocation
/// and blocking I/O are acceptable here.
fn dump_latency_histograms<S, D>(
    dump: &LatencyDump,
    eng: &Engine<S, D>,
    now_ns_stamp: u64,
) -> io::Result<()>
where
    S: strategy_core::Strategy,
    D: OrderDispatch,
{
    std::fs::create_dir_all(&dump.dir)?;
    let mut path = dump.dir.clone();
    path.push(format!("latency_{now_ns_stamp}.hgrm"));
    let mut file = std::fs::File::create(&path)?;
    eng.write_latency_hgrm(&mut file)?;
    tracing::info!(path = %path.display(), "wrote latency histogram dump");
    Ok(())
}

// ---------------------------------------------------------------
// Shutdown helpers
// ---------------------------------------------------------------

/// Join `handles` in reverse boot order. Errors are logged, never
/// propagated — we're already shutting down.
pub fn join_reverse(handles: Vec<JoinHandle<()>>) {
    for h in handles.into_iter().rev() {
        let name = h.thread().name().unwrap_or("<unnamed>").to_string();
        if let Err(e) = h.join() {
            tracing::error!(thread = %name, error = ?e, "thread join panicked");
        } else {
            tracing::info!(thread = %name, "thread joined");
        }
    }
}

/// Force shutdown — used by tests and the second-press SIGINT path.
pub fn signal_shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

// ---------------------------------------------------------------
// Internals
// ---------------------------------------------------------------

fn connect_tls(
    ep: &WssEndpoint,
    server_name: &ServerName<'static>,
    tls_config: &RustlsConfig,
) -> io::Result<TlsTransport> {
    TlsTransport::connect(ep.addr, server_name.clone(), tls_config.clone())
}

fn new_poll() -> io::Result<(mio::Poll, mio::Events, mio::Token)> {
    let poll = mio::Poll::new()?;
    let events = mio::Events::with_capacity(64);
    Ok((poll, events, mio::Token(0)))
}

fn backoff() {
    thread::sleep(RECONNECT_BACKOFF);
}

fn log_pin_outcome(thread_label: &str, core_id: usize) {
    match pin_current_thread_to_core(core_id) {
        Ok(()) => tracing::info!(thread = thread_label, core = core_id, "thread pinned"),
        Err(crate::pinning::PinError::Unsupported) => {
            tracing::warn!(
                thread = thread_label,
                "thread pinning unsupported on this OS; continuing unpinned"
            )
        }
        Err(crate::pinning::PinError::Syscall(e)) => tracing::warn!(
            thread = thread_label,
            core = core_id,
            error = ?e,
            "sched_setaffinity failed; continuing unpinned"
        ),
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `Rings::new()` must succeed without panicking. The rings are
    /// large (~MB scale) so this also exercises the
    /// `Box::new_uninit()` + `addr_of_mut!` invariant from
    /// `core-ring`.
    #[test]
    fn rings_allocate_and_split() {
        let rings = Rings::new();
        let (_pt_p, _pt_c) = rings.polymarket_tick.split();
        let (_bt_p, _bt_c) = rings.binance_tick.split();
        let (_rpc_p, _rpc_c) = rings.rpc_signal.split();
        let (_rss_p, _rss_c) = rings.rss_signal.split();
    }

    #[test]
    fn drain_counters_add() {
        let mut a = DrainCounters {
            polymarket_ticks: 1,
            binance_ticks: 2,
            rpc_signals: 3,
            rss_signals: 4,
        };
        let b = DrainCounters {
            polymarket_ticks: 10,
            binance_ticks: 20,
            rpc_signals: 30,
            rss_signals: 40,
        };
        a.add(&b);
        assert_eq!(a.polymarket_ticks, 11);
        assert_eq!(a.binance_ticks, 22);
        assert_eq!(a.rpc_signals, 33);
        assert_eq!(a.rss_signals, 44);
    }

    /// Drain loop must exit promptly when `SHUTDOWN` is set, and
    /// return whatever it had drained before the flag flipped.
    #[test]
    fn drain_loop_exits_when_shutdown_set() {
        // Reset state — other tests in this binary may have flipped it.
        SHUTDOWN.store(false, Ordering::Release);

        let rings = Rings::new();
        let (_p1, c1) = rings.polymarket_tick.split();
        let (_p2, c2) = rings.binance_tick.split();
        let (_p3, c3) = rings.rpc_signal.split();
        let (_p4, c4) = rings.rss_signal.split();
        let cons = Consumers {
            polymarket_tick: c1,
            binance_tick: c2,
            rpc_signal: c3,
            rss_signal: c4,
        };

        // Flip shutdown from a sibling thread after a brief delay,
        // then assert the drain loop returns.
        let stop_handle = thread::spawn(|| {
            thread::sleep(Duration::from_millis(50));
            signal_shutdown();
        });
        let counters = drain_and_count_loop(cons);
        stop_handle.join().unwrap();
        // Empty rings → all zeros.
        assert_eq!(counters.polymarket_ticks, 0);
        assert_eq!(counters.binance_ticks, 0);
        assert_eq!(counters.rpc_signals, 0);
        assert_eq!(counters.rss_signals, 0);

        // Reset for downstream tests.
        SHUTDOWN.store(false, Ordering::Release);
    }

    #[test]
    fn join_reverse_handles_empty_vec() {
        join_reverse(Vec::new());
    }
}
