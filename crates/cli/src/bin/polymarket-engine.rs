//! # polymarket-engine — main entrypoint
//!
//! Thin dispatch shim. All orchestration logic lives in the `cli`
//! library so that it can be unit-tested without spawning a real
//! binary.
//!
//! Subcommands:
//! * `run --paper` — spawn all four ingress threads (Polymarket,
//!   Binance, Polygon RPC, RSS), boot the real `Engine` with the
//!   latency-arb strategy + paper dispatcher, drain consumers on
//!   the main thread until SIGINT.
//! * `print-config` — load `.env` + env and print the resolved
//!   (non-secret) config.

use std::path::PathBuf;
use std::process::ExitCode;

use std::sync::atomic::AtomicBool;

use clap::Parser;
use cli::{
    engine_loop_cross_arb_full, engine_loop_ev_full, engine_loop_full,
    engine_loop_rule_tree_full, install_sigint_handler, join_reverse, spawn_binance,
    spawn_polymarket, spawn_rpc, Consumers, EngineConfig, EngineLoopResult, LatencyDump,
    LiveDispatcher, Observability, Rings, StrategyPair, WssEndpoint, SHUTDOWN,
};
use core_config::{Config, Secrets};
use core_net::TlsTransport;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(name = "polymarket-engine", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Parser)]
// The CLI dispatch enum is parsed once at boot. The size delta
// between `Run` and `PrintConfig` is irrelevant — the value lives
// on the stack of `main` for the whole process. Boxing would
// fight clap's Parser derive for no runtime benefit.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Launch the engine.
    Run(RunArgs),
    /// Print resolved configuration and exit — smoke-tests the `.env` loader.
    PrintConfig(ConfigArgs),
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Path to a .env file; defaults to `./.env` via dotenvy.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Paper mode — do not actually submit orders. Default ON.
    /// Mutually exclusive with `--live`.
    #[arg(long, default_value_t = true, conflicts_with = "live")]
    paper: bool,
    /// Live mode — sign + POST orders to Polymarket's CLOB. Requires
    /// a valid `.env` with `POLYMARKET_EIP712_KEY`. Default OFF.
    #[arg(long, default_value_t = false)]
    live: bool,
    /// Binance symbol string (lowercase, no separator), e.g. `btcusdt`.
    #[arg(long, default_value = "btcusdt")]
    binance_symbol: String,
    /// Internal SymbolId for the Binance pair. Compact u32 used inside
    /// the engine; must match the run-loop driver argument.
    #[arg(long, default_value_t = 7u32)]
    binance_sym_id: u32,
    /// Internal SymbolId for the Polymarket market paired with the
    /// Binance symbol above. The Polymarket SymbolMap must resolve the
    /// market's asset_id to this same id.
    #[arg(long, default_value_t = 42u32)]
    polymarket_sym_id: u32,
    /// Trigger threshold in 1e6 fixed-point units (e.g. `20000` is
    /// $0.02).
    #[arg(long, default_value_t = 20_000i64)]
    threshold_1e6: i64,
    /// Order quantity in 1e6 fixed-point units.
    #[arg(long, default_value_t = 10_000_000i64)]
    qty_1e6: i64,
    /// Cooldown between emits per market, in nanoseconds.
    #[arg(long, default_value_t = 250_000_000u64)]
    cooldown_ns: u64,
    /// Polygon RPC path (e.g. `/v2/<KEY>`). If absent, the RPC ingress
    /// thread is not started.
    #[arg(long)]
    polygon_path: Option<String>,
    /// Bind `127.0.0.1:9191` and expose `/metrics` in Prometheus
    /// text format. Default ON.
    #[arg(long, default_value_t = true)]
    metrics: bool,
    /// Render a live ratatui dashboard instead of the per-5s
    /// tracing log line. Implies `--metrics`.
    #[arg(long, default_value_t = false)]
    tui: bool,
    /// Strategy selector. `latency-arb` (default) uses Binance →
    /// Polymarket cross-venue arbitrage. `ev` uses Strategy A:
    /// model-vs-market mispricing against claude-worker artifacts.
    #[arg(long, default_value = "latency-arb")]
    strategy: String,
    /// Path to claude-worker NDJSON tag artifacts. Required when
    /// `--strategy ev`.
    #[arg(long)]
    artifacts_path: Option<PathBuf>,
    /// Comma-separated group spec for `--strategy cross-arb`,
    /// e.g. `"10,11,12;20,21"`. Each `;`-delimited slice is one
    /// MarketGroup of comma-delimited SymbolIds.
    #[arg(long)]
    groups: Option<String>,
    /// Path to claude-worker rule JSON. Required for
    /// `--strategy rule-tree`.
    #[arg(long)]
    rules_path: Option<PathBuf>,
    /// Cadence in seconds for periodic HdrHistogram dumps. `0`
    /// disables dumping (default). When >0, the engine writes the
    /// three latency histograms (ingest→strategy, strategy→submit,
    /// submit→ack) into `--latency-dump-dir` every N seconds.
    #[arg(long, default_value_t = 0u64)]
    latency_dump_secs: u64,
    /// Destination directory for HdrHistogram dumps. Defaults to
    /// `<POLYMARKET_LOG_DIR>/latency`. Only consulted when
    /// `--latency-dump-secs` is non-zero.
    #[arg(long)]
    latency_dump_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct ConfigArgs {
    /// Path to a .env file; defaults to `./.env` via dotenvy.
    #[arg(long)]
    env_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => run(args),
        Cmd::PrintConfig(args) => print_config(args),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn print_config(args: ConfigArgs) -> ExitCode {
    let cfg = match Config::load(args.env_file.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            error!(error = ?e, "config load failed");
            return ExitCode::from(1);
        }
    };
    info!(?cfg, "resolved non-secret config");
    match Secrets::load() {
        Ok(_) => info!("secrets loaded (redacted)"),
        Err(e) => info!(error=?e, "secrets not present — that's fine for PrintConfig"),
    }
    ExitCode::SUCCESS
}

/// Boot a [`LiveDispatcher`] from the loaded config + secrets.
/// Returns a static error message on any boot-time failure so the
/// caller can surface a clean `EngineLoopResult::Failed`.
fn boot_live_dispatcher(
    cfg: &Config,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<LiveDispatcher, &'static str> {
    // Surface the actual ConfigError variant instead of a one-size
    // "missing key?" string. Each branch logs the precise cause so
    // an operator can fix the right thing.
    let secrets = match Secrets::load() {
        Ok(s) => s,
        Err(core_config::ConfigError::Missing(k)) => {
            error!(key = k, "Secrets::load: required env var missing");
            return Err("Secrets::load: required env var missing");
        }
        Err(core_config::ConfigError::Invalid(k)) => {
            error!(
                key = k,
                "Secrets::load: env var present but not parseable (hex length / nibble)"
            );
            return Err("Secrets::load: env var unparseable");
        }
        Err(core_config::ConfigError::Mlock(errno)) => {
            error!(
                errno,
                "Secrets::load: mlock failed — raise RLIMIT_MEMLOCK or run with CAP_IPC_LOCK"
            );
            return Err("Secrets::load: mlock failed");
        }
        Err(core_config::ConfigError::DotenvMissing(path)) => {
            error!(path, "Secrets::load: .env file not found / unreadable");
            return Err("Secrets::load: .env file not found");
        }
    };
    let mut key = [0u8; 32];
    key.copy_from_slice(secrets.signing_key());
    let port = 443u16;
    LiveDispatcher::connect(
        &cfg.polymarket_clob_host,
        "/order",
        port,
        key,
        tls_config,
    )
    .map_err(|e| {
        error!(error = ?e, "LiveDispatcher::connect failed");
        "LiveDispatcher::connect failed (DNS / cert / key)"
    })
}

/// Boot a [`LiveDispatcher`] wrapped in a [`QueuedDispatcher`].
/// Spawns the worker thread; returns the producer-side handle and
/// the worker's `JoinHandle` so the caller can keep it alive
/// until shutdown.
///
/// **Why queue?** `LiveDispatcher::submit` blocks for one TCP+TLS
/// POST round-trip (~50–100 ms over WAN). The queued path lets
/// the engine fire-and-forget — submits become a single SPSC
/// ring push (~ns) instead of a network round-trip.
fn boot_queued_live(
    cfg: &Config,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<(clob_dispatcher::QueuedDispatcher, std::thread::JoinHandle<()>), &'static str> {
    let live = boot_live_dispatcher(cfg, tls_config)?;
    let (queued, worker) = clob_dispatcher::QueuedDispatcher::new(live);
    let stop_ref: &'static AtomicBool = &SHUTDOWN;
    let handle = std::thread::Builder::new()
        .name("clob-dispatcher".into())
        .spawn(move || {
            info!("clob-dispatcher worker thread up");
            worker.run(stop_ref);
            info!("clob-dispatcher worker thread exiting");
        })
        .map_err(|_| "spawn clob-dispatcher worker failed")?;
    Ok((queued, handle))
}

fn run(args: RunArgs) -> ExitCode {
    let cfg = match Config::load(args.env_file.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            error!(error = ?e, "config load failed");
            return ExitCode::from(1);
        }
    };

    info!(
        paper = args.paper,
        polymarket = %cfg.polymarket_clob_host,
        binance = %cfg.binance_ws_host,
        alchemy = %cfg.alchemy_host,
        "starting engine"
    );

    if let Err(e) = install_sigint_handler() {
        error!(error = ?e, "SIGINT handler install failed");
        return ExitCode::from(1);
    }

    let tls_config = TlsTransport::default_client_config();

    // -- Resolve endpoints --
    let pm_ep = match WssEndpoint::resolve(&cfg.polymarket_clob_host, 443, "/ws/") {
        Ok(e) => e,
        Err(e) => {
            error!(error = ?e, "polymarket DNS failed");
            return ExitCode::from(1);
        }
    };
    let bn_path = format!("/ws/{}@bookTicker", args.binance_symbol);
    let bn_ep = match WssEndpoint::resolve(&cfg.binance_ws_host, 443, &bn_path) {
        Ok(e) => e,
        Err(e) => {
            error!(error = ?e, "binance DNS failed");
            return ExitCode::from(1);
        }
    };

    // -- Allocate rings + split into producer/consumer halves --
    let rings = Rings::new();
    let (pm_prod, pm_cons) = rings.polymarket_tick.split();
    let (bn_prod, bn_cons) = rings.binance_tick.split();
    let (rpc_prod, rpc_cons) = rings.rpc_signal.split();
    let (rss_prod, rss_cons) = rings.rss_signal.split();

    // -- Spawn ingress threads --
    let mut handles = Vec::new();

    let pm_handle = spawn_polymarket(
        pm_ep,
        tls_config.clone(),
        ingress_polymarket::run_loop::SymbolMap::from_pairs(std::iter::empty()),
        pm_prod,
        1,
    );
    handles.push(pm_handle);

    let bn_handle = spawn_binance(bn_ep, tls_config.clone(), args.binance_sym_id, bn_prod, 2);
    handles.push(bn_handle);

    if let Some(polygon_path) = args.polygon_path {
        match WssEndpoint::resolve(&cfg.alchemy_host, 443, &polygon_path) {
            Ok(rpc_ep) => {
                let rpc_handle = spawn_rpc(rpc_ep, tls_config.clone(), rpc_prod, 3);
                handles.push(rpc_handle);
            }
            Err(e) => {
                error!(error = ?e, "RPC DNS failed; skipping rpc ingress");
            }
        }
    } else {
        warn!("--polygon-path not provided; RPC ingress thread not started");
    }

    // -- RSS ingress (optional; opt-in via RSS_FEEDS in .env) --
    // Default poll interval: 5 min — RSS feeds explicitly Slow-class
    // signals; tighter polling would burn the publisher's free tier
    // and yield no statistical advantage.
    const RSS_POLL_NS: u64 = 5 * 60 * 1_000_000_000;
    let mut rss_feeds: Vec<cli::RssFeed> = Vec::new();
    for url in cfg.rss_feeds() {
        match cli::RssFeed::parse(url, RSS_POLL_NS) {
            Ok(f) => rss_feeds.push(f),
            Err(reason) => {
                warn!(url, reason, "rss: rejecting feed URL");
            }
        }
    }
    if rss_feeds.is_empty() {
        info!("RSS_FEEDS empty / unset; RSS ingress thread not started");
        // Drop the producer side so the engine's consumer doesn't
        // park forever waiting on a ring nothing pushes to.
        drop(rss_prod);
    } else {
        info!(feeds = rss_feeds.len(), "rss: starting ingress thread");
        let rss_handle = cli::spawn_rss(rss_feeds, tls_config.clone(), rss_prod, 4);
        handles.push(rss_handle);
    }

    // -- Main thread: real engine loop until SIGINT --
    let cons = Consumers {
        polymarket_tick: pm_cons,
        binance_tick: bn_cons,
        rpc_signal: rpc_cons,
        rss_signal: rss_cons,
    };
    let engine_cfg = EngineConfig {
        pairs: vec![StrategyPair {
            polymarket: args.polymarket_sym_id,
            binance: args.binance_sym_id,
        }],
        threshold_1e6: args.threshold_1e6,
        qty_1e6: args.qty_1e6,
        cooldown_ns: args.cooldown_ns,
    };

    // -- Build observability surfaces --
    let enable_metrics = args.metrics || args.tui;
    let enable_tui = args.tui;
    let obs = match Observability::build(enable_metrics, enable_tui) {
        Ok(o) => o,
        Err(reason) => {
            error!(reason, "observability build failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    // Resolve latency-dump destination. Defaults to
    // `<cfg.log_dir>/latency` so an operator who already set
    // `POLYMARKET_LOG_DIR` doesn't need a second flag.
    let latency_dump_dir = args
        .latency_dump_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}/latency", cfg.log_dir)));
    let obs = obs.with_latency_dump(LatencyDump::from_secs(
        latency_dump_dir.clone(),
        args.latency_dump_secs,
    ));
    if args.latency_dump_secs > 0 {
        info!(
            secs = args.latency_dump_secs,
            dir = %latency_dump_dir.display(),
            "latency dump enabled"
        );
    }

    // Boot the /metrics HTTP server if requested. Owns its own
    // thread; observes the same SHUTDOWN flag.
    let mut obs_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    if let Some(reg) = obs.metrics.clone() {
        let bind: std::net::SocketAddr = cfg
            .metrics_bind
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:9191".parse().unwrap());
        let stop_ref: &'static AtomicBool = &SHUTDOWN;
        obs_handles.push(
            std::thread::Builder::new()
                .name("metrics-http".into())
                .spawn(move || {
                    info!(%bind, "metrics: HTTP server starting");
                    if let Err(e) = core_metrics::serve_metrics(bind, reg, stop_ref) {
                        error!(error = ?e, "metrics: serve_metrics returned error");
                    }
                })
                .expect("spawn metrics thread"),
        );
    }

    // Boot the TUI render thread if requested.
    if let Some(cell) = obs.snapshot.clone() {
        let stop_ref: &'static AtomicBool = &SHUTDOWN;
        obs_handles.push(
            std::thread::Builder::new()
                .name("tui-render".into())
                .spawn(move || {
                    if let Err(e) = tui::run_dashboard(&cell, stop_ref) {
                        error!(error = ?e, "tui: run_dashboard returned error");
                    }
                })
                .expect("spawn tui thread"),
        );
    }

    let strategy_choice = args.strategy.as_str();
    let result = match (strategy_choice, args.live) {
        ("latency-arb", true) => match boot_queued_live(&cfg, tls_config.clone()) {
            Ok((queued, worker_handle)) => {
                info!("running latency-arb LIVE — orders queued to dispatcher thread");
                obs_handles.push(worker_handle);
                engine_loop_full(cons, engine_cfg, queued, obs)
            }
            Err(reason) => EngineLoopResult::Failed(reason),
        },
        ("latency-arb", false) => {
            info!("running latency-arb PAPER — no orders will be submitted");
            engine_loop_full(cons, engine_cfg, clob_dispatcher::PaperDispatcher::new(), obs)
        }
        ("cross-arb", _live) => {
            let spec = match args.groups.as_deref() {
                Some(s) => s,
                None => {
                    error!("--strategy cross-arb requires --groups <SPEC>");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            // Parse "10,11,12;20,21" into owned Vec<Vec<SymbolId>>.
            let owned: Vec<Vec<core_types::SymbolId>> = spec
                .split(';')
                .map(|grp| {
                    grp.split(',')
                        .filter_map(|s| s.trim().parse::<u32>().ok())
                        .collect()
                })
                .filter(|v: &Vec<u32>| !v.is_empty())
                .collect();
            let groups_ref: Vec<&[core_types::SymbolId]> =
                owned.iter().map(|v| v.as_slice()).collect();
            info!(groups = groups_ref.len(), "cross-arb: parsed groups");
            info!("running cross-arb PAPER — no orders will be submitted");
            engine_loop_cross_arb_full(
                cons,
                engine_cfg,
                clob_dispatcher::PaperDispatcher::new(),
                obs,
                &groups_ref,
            )
        }
        ("rule-tree", _live) => {
            let rp = match args.rules_path.as_deref() {
                Some(p) => p,
                None => {
                    error!("--strategy rule-tree requires --rules-path <JSON>");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            // v1: map every loaded rule to `--polymarket-sym-id`
            // with the rule name's first 16 bytes as the keyword.
            let mut kw = [0u8; 16];
            let n = b"halving".len().min(16);
            kw[..n].copy_from_slice(&b"halving"[..n]);
            let mapping = vec![(args.polymarket_sym_id, kw, n as u8)];
            info!("running rule-tree PAPER — no orders will be submitted");
            engine_loop_rule_tree_full(
                cons,
                engine_cfg,
                clob_dispatcher::PaperDispatcher::new(),
                obs,
                rp,
                &mapping,
            )
        }
        ("ev", live_flag) => {
            let path = match args.artifacts_path.as_deref() {
                Some(p) => p,
                None => {
                    error!("--strategy ev requires --artifacts-path <NDJSON>");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            if live_flag {
                match boot_queued_live(&cfg, tls_config.clone()) {
                    Ok((queued, worker_handle)) => {
                        info!("running ev LIVE — orders queued to dispatcher thread");
                        obs_handles.push(worker_handle);
                        engine_loop_ev_full(cons, engine_cfg, queued, obs, path)
                    }
                    Err(reason) => EngineLoopResult::Failed(reason),
                }
            } else {
                info!("running ev PAPER — no orders will be submitted");
                engine_loop_ev_full(
                    cons,
                    engine_cfg,
                    clob_dispatcher::PaperDispatcher::new(),
                    obs,
                    path,
                )
            }
        }
        (other, _) => {
            error!(strategy = other, "unknown --strategy value");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    let exit_code = match result {
        EngineLoopResult::Done(stats) => {
            info!(?stats, "engine loop exited cleanly");
            ExitCode::SUCCESS
        }
        EngineLoopResult::Failed(reason) => {
            error!(reason, "engine loop failed at boot");
            ExitCode::from(1)
        }
    };

    // -- Reverse-order join (ingress threads first, then obs) --
    join_reverse(handles);
    for h in obs_handles.into_iter().rev() {
        let name = h.thread().name().unwrap_or("<obs>").to_string();
        if let Err(e) = h.join() {
            tracing::error!(thread = %name, error = ?e, "obs thread join panicked");
        }
    }
    info!("clean shutdown");
    exit_code
}
