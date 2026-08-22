//! # multivenue-engine — main entrypoint
//!
//! Thin dispatch shim. All orchestration logic lives in the `cli`
//! library so that it can be unit-tested without spawning a real
//! binary.
//!
//! Subcommands:
//! * `run --paper` — spawn the ingress threads (Polymarket, Binance,
//!   OKX when `--okx-symbols` is set, Deribit when
//!   `--deribit-symbols` is set, Hyperliquid when `--hl-coins` is
//!   set, Polygon RPC), boot the
//!   real `Engine` with the latency-arb strategy + paper dispatcher,
//!   drain consumers on the main thread until SIGINT.
//! * `print-config` — load `.env` + env and print the resolved
//!   (non-secret) config.

use std::path::PathBuf;
use std::process::ExitCode;

use std::sync::atomic::AtomicBool;

use clap::Parser;
use cli::{
    engine_loop_cross_arb_full, engine_loop_ev_full, engine_loop_full,
    engine_loop_rule_tree_full, engine_loop_set_full, install_sigint_handler, join_reverse,
    spawn_binance, spawn_deribit, spawn_hyperliquid, spawn_okx, spawn_polymarket, spawn_rpc,
    Consumers, EngineConfig, EngineLoopResult, LatencyDump, LiveDispatcher, Observability, Rings,
    StrategyPair, WssEndpoint, SHUTDOWN,
};
use core_config::{Config, Secrets};
use core_net::TlsTransport;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(name = "multivenue-engine", version)]
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
    /// Offline §6.5 capture audit: per-symbol rates, cadence-band
    /// checks, integrity re-derivations and the venue×channel coverage
    /// matrix over one capture run directory.
    AuditReplay(AuditReplayArgs),
    /// Offline 8h backtest harness (docs/phase-8h-design.md §3–§5):
    /// deterministic replay of PMLR capture through the real
    /// strategy-vm against a candidate ruleset. schema-1 JSON on
    /// stdout (the frozen claude-worker contract), human summary on
    /// stderr; exit 0 only when a trustworthy report was printed.
    /// H1 slice: hold-model accounting — the §4 fill model lands in H2.
    Backtest(BacktestArgs),
    /// Offline M3 capture catalog (mvp-plan §4-M3): walks a replay
    /// root (or one `run-<epoch_ns>` dir) and reports per-run wall
    /// spans, per-venue tick coverage, UTC-day continuity (gap map,
    /// gap-free-day streaks), run-dir sizes, the backtest view
    /// (harness §3.1 acceptance + §4.5 day arithmetic) and the
    /// monitor view (§8.3 trailing-window coverage). JSON on stdout,
    /// human summary on stderr; an EMPTY root is a valid zero-run
    /// report (init-if-empty visibility).
    CaptureCatalog(CaptureCatalogArgs),
}

#[derive(Debug, Parser)]
struct AuditReplayArgs {
    /// Capture run directory (`<MULTIVENUE_LOG_DIR>/run-<ns>`).
    #[arg(long)]
    dir: PathBuf,
}

#[derive(Debug, Parser)]
struct CaptureCatalogArgs {
    /// Replay root (`MULTIVENUE_LOG_DIR`) or one `run-<epoch_ns>`
    /// directory — the same §3.1 resolution as `backtest
    /// --replay-dir`.
    #[arg(long)]
    dir: PathBuf,
    /// Max dark ns a UTC day may carry and still count gap-free
    /// (default 300 s — the daily-restart drain allowance).
    #[arg(long, default_value_t = cli::capture_catalog::DEFAULT_GAP_TOLERANCE_NS)]
    gap_tolerance_ns: u64,
}

#[derive(Debug, Parser)]
struct BacktestArgs {
    /// Candidate ruleset JSON artifact (8g §4.1 grammar).
    #[arg(long)]
    ruleset: PathBuf,
    /// Capture source: a single `run-<epoch_ns>` directory or a log
    /// root (`MULTIVENUE_LOG_DIR`) containing `run-*` children.
    #[arg(long)]
    replay_dir: PathBuf,
    /// IS/OOS split `N/M`: integers, `N + M == 100`, both >= 10 — or
    /// the carved all-OOS monitor form `0/100` (design §3.4). Echoed
    /// verbatim into the schema-1 report.
    #[arg(long)]
    split: String,
    /// §4.3 fee override `<venue>:<maker_bps>:<taker_bps>`,
    /// repeatable (venues: pm|bn|okx|deribit|hl). Defaults all 0/0.
    /// Parsed but UNUSED by the H1 hold model (consumed from H2).
    #[arg(long)]
    fee_bps: Vec<String>,
    /// §4.4 global latency-penalty override in ns (default per-venue:
    /// pm 200 ms, bn/okx/deribit 100 ms, hl 600 ms). Parsed but
    /// UNUSED by the H1 hold model (consumed from H2).
    #[arg(long)]
    latency_ns: Option<u64>,
    /// §4.4 per-venue latency override `<venue>:<ns>`, repeatable;
    /// wins over `--latency-ns`. Parsed but UNUSED by the H1 hold
    /// model (consumed from H2).
    #[arg(long)]
    latency_ns_venue: Vec<String>,
    /// §5 rich-detail sidecar path (per-symbol/IS metrics). Declared
    /// now; the sidecar is written starting H2.
    #[arg(long)]
    emit_detail: Option<PathBuf>,
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
    /// Universe config file (M1; TOML subset — see
    /// `universe.toml.example`). Explicit path must exist. Absent:
    /// `~/multivenue/universe.toml` is used IF present, else the
    /// legacy flag-driven boot. Per-venue flags override the file.
    #[arg(long)]
    universe: Option<PathBuf>,
    /// Binance spot symbol (lowercase, no separator), e.g. `btcusdt`.
    /// Legacy default `btcusdt`; with a universe config this flag
    /// OVERRIDES the config's spot list with the one symbol.
    #[arg(long)]
    binance_symbol: Option<String>,
    /// Internal SymbolId for the Binance spot symbol (legacy anchor 7
    /// when unset). With a universe config, requires
    /// `--binance-symbol`.
    #[arg(long)]
    binance_sym_id: Option<u32>,
    /// Internal SymbolId for the first Polymarket market (legacy
    /// anchor 42 when unset). With a universe config, requires
    /// `--polymarket-asset-id`.
    #[arg(long)]
    polymarket_sym_id: Option<u32>,
    /// Comma-separated OKX instIds (e.g. `BTC-USDT,ETH-USD-SWAP`).
    /// The i-th entry (0-based) is allocated SymbolId
    /// `make_symbol_id(Okx, i+1)` — flag order is id order. Empty /
    /// absent = the OKX ingress thread is not started.
    #[arg(long)]
    okx_symbols: Option<String>,
    /// Also subscribe the OKX 400-level `books` channel per
    /// instrument (capture + integrity only, §4.5). Default OFF —
    /// `bbo-tbt` alone feeds the tick lane.
    #[arg(long, default_value_t = false)]
    okx_depth: bool,
    /// Comma-separated Deribit instruments (e.g.
    /// `BTC-PERPETUAL,ETH-PERPETUAL`). The i-th entry (0-based) is
    /// allocated SymbolId `make_symbol_id(Deribit, i+1)` — flag
    /// order is id order. Empty / absent = the Deribit ingress
    /// thread is not started.
    #[arg(long)]
    deribit_symbols: Option<String>,
    /// Also subscribe the Deribit change_id-chained
    /// `book.{instr}.100ms` channel per instrument (capture +
    /// integrity only, §4.5). Default OFF — `quote` alone feeds
    /// the tick lane.
    #[arg(long, default_value_t = false)]
    deribit_depth: bool,
    /// Comma-separated Hyperliquid coins (e.g. `BTC,ETH,#330`;
    /// HIP-4 `#<enc>` outcome coins and spot `@<idx>` pairs are
    /// ordinary items). The i-th entry (0-based) is allocated
    /// SymbolId `make_symbol_id(Hyperliquid, i+1)` — flag order is
    /// id order. Empty / absent = the Hyperliquid ingress thread is
    /// not started. There is no depth flag: `l2Book` is always
    /// subscribed — it feeds the §6.2 staleness monitor.
    #[arg(long)]
    hl_coins: Option<String>,
    /// Polymarket CLOB asset id (token id) — the decimal string from
    /// the market's `clobTokenIds`. REQUIRED in legacy mode (no
    /// universe config): without it the PM symbol map is empty and
    /// every Polymarket frame fails lookup (defect D1 — zero PM
    /// ticks); boot refuses to start rather than run venue-blind.
    /// With a universe config, this flag OVERRIDES the config's
    /// market list with the one market.
    #[arg(long)]
    polymarket_asset_id: Option<String>,
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
    /// `ai-exec` (Phase 8f item 8) runs the AI-driven fair-value/
    /// intent strategy alone via the set path (no boot symbol
    /// config — the AI publishes the universe over UDS); paper-only
    /// until 8i. `vm` (Phase 8g) runs the ruleset-VM strategy alone
    /// via the set path (no boot config — it boots inert and trades
    /// only after a ruleset table is staged + committed over UDS,
    /// design §7.3); paper-only until 8i. `all` (Phase 8f) runs the
    /// composed StrategySet: every built member whose config flags
    /// are present (ai-exec and vm need none and are always
    /// included), AI-toggleable at runtime; paper-only until 8i.
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
    /// `<MULTIVENUE_LOG_DIR>/latency`. Only consulted when
    /// `--latency-dump-secs` is non-zero.
    #[arg(long)]
    latency_dump_dir: Option<PathBuf>,
    /// Comma-separated venue labels (`pm,bn,okx,rpc,deribit,hl`) or
    /// the literal `all` — enables the §6.5 bounded raw-payload tap
    /// for those ingress threads. Default: none (tap off everywhere).
    #[arg(long)]
    raw_tap: Option<String>,
    /// Raw-tap recording mode: `rejects` (only parser-rejected
    /// payloads) or `all` (every inbound payload, rejects included).
    /// Only meaningful when `--raw-tap` names at least one venue.
    #[arg(long, default_value = "rejects")]
    raw_tap_mode: String,
    /// Raw-tap file budget per venue, in MiB. Once exhausted, further
    /// tap records are dropped and counted
    /// (`PmlrCapture::tap_dropped`) rather than growing the file.
    #[arg(long, default_value_t = 64u64)]
    raw_tap_budget_mb: u64,
}

#[derive(Debug, Parser)]
struct ConfigArgs {
    /// Path to a .env file; defaults to `./.env` via dotenvy.
    #[arg(long)]
    env_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    // Parse BEFORE installing tracing: the backtest arm must route
    // every log line to stderr — its stdout is the schema-1 JSON the
    // worker `json.loads`es, and one stray fmt-layer line (default
    // writer: stdout) would corrupt the frozen contract. The other
    // arms keep their historical stdout logging unchanged.
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => {
            init_tracing();
            run(args)
        }
        Cmd::PrintConfig(args) => {
            init_tracing();
            print_config(args)
        }
        Cmd::AuditReplay(args) => {
            init_tracing();
            audit_replay(args)
        }
        Cmd::Backtest(args) => {
            init_tracing_stderr();
            backtest(args)
        }
        Cmd::CaptureCatalog(args) => {
            // stderr tracing for the same reason as the backtest arm:
            // stdout carries the catalog JSON and nothing else.
            init_tracing_stderr();
            capture_catalog(args)
        }
    }
}

/// M3 catalog arm: JSON on stdout + summary on stderr, exit 0 iff a
/// report was produced (an empty root IS a report); any failure
/// prints its reason to stderr only and exits nonzero.
fn capture_catalog(args: CaptureCatalogArgs) -> ExitCode {
    let cfg = cli::capture_catalog::CatalogConfig {
        dir: args.dir,
        gap_tolerance_ns: args.gap_tolerance_ns,
    };
    match cli::capture_catalog::run_catalog(&cfg) {
        Ok(out) => {
            eprint!("{}", out.summary);
            println!("{}", out.json);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("capture-catalog: {e}");
            ExitCode::from(1)
        }
    }
}

/// The §5 exit-code contract: schema-1 on stdout + summary on stderr
/// and exit 0 iff the report is trustworthy; ANY failure prints its
/// reason to stderr only and exits nonzero (the worker maps every
/// nonzero to `BacktestError` — "harness output untrusted").
fn backtest(args: BacktestArgs) -> ExitCode {
    let cfg = cli::backtest::BacktestConfig {
        ruleset: args.ruleset,
        replay_dir: args.replay_dir,
        split: args.split,
        fee_bps: args.fee_bps,
        latency_ns: args.latency_ns,
        latency_ns_venue: args.latency_ns_venue,
        emit_detail: args.emit_detail,
    };
    match cli::backtest::run(&cfg) {
        Ok(out) => {
            eprint!("{}", out.summary);
            println!("{}", out.schema1);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("backtest: {e}");
            ExitCode::from(1)
        }
    }
}

fn audit_replay(args: AuditReplayArgs) -> ExitCode {
    match cli::audit_replay::run_audit(&args.dir) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = ?e, dir = %args.dir.display(), "audit-replay failed");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Backtest-arm tracing: identical filter, writer pinned to stderr so
/// stdout carries schema-1 bytes and nothing else.
fn init_tracing_stderr() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
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

    // -- M1 universe resolution (config file + flag overrides) --
    // Pure precedence law in cli::universe_boot; any failure is a
    // fatal boot error BEFORE side effects (no capture dir, no
    // discovery, no threads).
    let config_src = match cli::universe_boot::read_universe_source(args.universe.as_deref()) {
        Ok(v) => v,
        Err(reason) => {
            error!(reason, "universe config read failed");
            return ExitCode::from(1);
        }
    };
    let boot = match cli::universe_boot::resolve_boot_universe(&cli::universe_boot::UniverseFlags {
        config_src: config_src.as_deref(),
        pm_asset_id: args.polymarket_asset_id.as_deref(),
        pm_sym_id: args.polymarket_sym_id,
        bn_symbol: args.binance_symbol.as_deref(),
        bn_sym_id: args.binance_sym_id,
        okx_symbols: args.okx_symbols.as_deref(),
        deribit_symbols: args.deribit_symbols.as_deref(),
        hl_coins: args.hl_coins.as_deref(),
        okx_depth: args.okx_depth,
        deribit_depth: args.deribit_depth,
    }) {
        Ok(b) => b,
        Err(reason) => {
            error!(reason, "universe resolution failed");
            return ExitCode::from(1);
        }
    };
    info!(
        from_config = boot.from_config,
        pm_tokens = boot.allocated.pm_tokens.len(),
        bn_spot = boot.allocated.bn_spot.len(),
        bn_usdm = boot.allocated.bn_usdm.len(),
        pairs = boot.allocated.pairs.len(),
        "universe resolved"
    );
    let pm_ids: Vec<String> = boot
        .allocated
        .pm_tokens
        .iter()
        .map(|t| t.token_id.clone())
        .collect();

    // -- Raw-tap flags (Phase 8e §6.5; fail-fast on a bad spec) --
    let raw_tap_cfg = match cli::parse_raw_tap_flags(
        args.raw_tap.as_deref(),
        &args.raw_tap_mode,
        args.raw_tap_budget_mb,
    ) {
        Ok(c) => c,
        Err(reason) => {
            error!(reason, "bad --raw-tap flags");
            return ExitCode::from(1);
        }
    };

    // -- Capture run directory (Phase 8e §6.5) --
    // Every spawned ingress's PmlrCapture files land under here; the
    // directory is created now so the first `PmlrCapture::open` below
    // never races its own `create_dir_all`.
    let (run_dir, epoch_ns) = match cli::new_capture_run_dir(&cfg.log_dir) {
        Ok(v) => v,
        Err(e) => {
            error!(error = ?e, dir = %cfg.log_dir, "capture: run directory create failed");
            return ExitCode::from(1);
        }
    };
    info!(dir = %run_dir.display(), "capture: run directory");

    // -- Phase-8e boot REST discovery (plan §6.1) --
    // Runs BEFORE any ingress thread spawns: validates every
    // configured symbol against the venue's live universe and (OKX
    // only) builds the discovery-gated symbol table. Any fetch/parse
    // failure is a fatal boot error in both paper and live mode — a
    // venue whose REST is down now would fail its WS subscribe anyway.
    // M1: config-driven boots get the Binance exchangeInfo audit;
    // legacy flag boots keep their historical zero-REST BN behavior.
    let bn_spot_names: Vec<String> = boot
        .allocated
        .bn_spot
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bn_usdm_names: Vec<String> = boot
        .allocated
        .bn_usdm
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bn_discovery_arg: Option<(&[String], &[String])> = if boot.from_config {
        Some((&bn_spot_names, &bn_usdm_names))
    } else {
        None
    };
    // M2.1: an explicit --deribit-symbols override replaced the whole
    // [deribit] section — an enabled config options policy was dropped
    // with it (M1a override law). Say so loudly.
    if boot.deribit_options_dropped {
        warn!(
            "--deribit-symbols override active — the universe config's deribit options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    if boot.okx_options_dropped {
        warn!(
            "--okx-symbols override active — the universe config's okx options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    if boot.bn_options_dropped {
        warn!(
            "--binance-symbol override active — the universe config's binance options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    let discovery = match cli::boot_discovery::run_all(
        &cfg,
        &tls_config,
        boot.okx_spec.as_deref(),
        &boot.okx_options,
        boot.deribit_spec.as_deref(),
        &boot.deribit_options,
        boot.hl_spec.as_deref(),
        bn_discovery_arg,
        &boot.bn_options,
        &pm_ids,
    ) {
        Ok(o) => o,
        Err(reason) => {
            error!(reason, "boot discovery failed");
            return ExitCode::from(1);
        }
    };
    if discovery.any_missing {
        if args.live {
            error!(
                "discovery: configured symbol(s) missing from venue universe — refusing to start live"
            );
            return ExitCode::from(1);
        }
        warn!(
            "discovery: configured symbol(s) missing from venue universe — continuing in paper mode"
        );
    }

    // -- Resolve endpoints --
    // Path fixed 2026-08-14 (8d live test): the real-time host serves the
    // market channel at `/ws/market`; `/ws/` returns HTTP 404.
    let pm_ep = match WssEndpoint::resolve(&cfg.polymarket_clob_host, 443, "/ws/market") {
        Ok(e) => e,
        Err(e) => {
            error!(error = ?e, "polymarket DNS failed");
            return ExitCode::from(1);
        }
    };
    // (Binance endpoint resolution happens at spawn below — the M1
    // multi lane resolves per slot inside its thread.)

    // -- OKX boot config (Phase 8b; venue is opt-in) --
    // Host (+ optional :port) is now `cfg.okx_ws_host` (Phase 8e §9 —
    // closes the 8b deferral); the WS path stays a cli const. The
    // symbol table comes straight from `discovery.okx_table` — it's
    // already discovery-gated (built above).
    const OKX_WS_PATH: &str = "/ws/v5/public";
    let okx_boot = match discovery.okx_table {
        Some(symbols) => {
            let (okx_host, okx_port) = match cli::split_host_port(&cfg.okx_ws_host, 8443) {
                Ok(v) => v,
                Err(reason) => {
                    error!(reason, host = %cfg.okx_ws_host, "bad okx_ws_host");
                    return ExitCode::from(1);
                }
            };
            let okx_ep = match WssEndpoint::resolve(okx_host, okx_port, OKX_WS_PATH) {
                Ok(e) => e,
                Err(e) => {
                    error!(error = ?e, "okx DNS failed");
                    return ExitCode::from(1);
                }
            };
            Some((symbols, okx_ep))
        }
        None => None,
    };

    // -- Deribit boot config (Phase 8c; venue is opt-in) --
    // Host is now `cfg.deribit_ws_host` (closes the 8c deferral); WS
    // path stays a cli const. Symbol table building is unaffected by
    // discovery (unlike OKX it needs no per-instrument `instType`).
    const DERIBIT_WS_PATH: &str = "/ws/api/v2";
    // M2.1: the venue boots when EITHER static instruments are
    // configured OR the discovered options chain is non-empty (an
    // options-only [deribit] section is a valid universe).
    let deribit_spec_trim = boot
        .deribit_spec
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let deribit_boot = if deribit_spec_trim.is_some() || !discovery.deribit_options.is_empty() {
        let mut symbols = match deribit_spec_trim {
            Some(spec) => match cli::build_deribit_symbol_table(spec) {
                Ok(t) => t,
                Err(reason) => {
                    error!(reason, spec, "bad --deribit-symbols");
                    return ExitCode::from(1);
                }
            },
            None => ingress_deribit::DeribitSymbolTable::new(),
        };
        // Discovered capped chain appends AFTER the static block
        // (quote-only rows; ordinals already allocated in discovery).
        if let Err(reason) =
            cli::extend_deribit_table_with_options(&mut symbols, &discovery.deribit_options)
        {
            error!(
                reason,
                selected = discovery.deribit_options.len(),
                "deribit options chain table build failed"
            );
            return ExitCode::from(1);
        }
        let (deribit_host, deribit_port) = match cli::split_host_port(&cfg.deribit_ws_host, 443)
        {
            Ok(v) => v,
            Err(reason) => {
                error!(reason, host = %cfg.deribit_ws_host, "bad deribit_ws_host");
                return ExitCode::from(1);
            }
        };
        let deribit_ep = match WssEndpoint::resolve(deribit_host, deribit_port, DERIBIT_WS_PATH)
        {
            Ok(e) => e,
            Err(e) => {
                error!(error = ?e, "deribit DNS failed");
                return ExitCode::from(1);
            }
        };
        Some((symbols, deribit_ep))
    } else {
        None
    };

    // -- Hyperliquid boot config (Phase 8d; venue is opt-in) --
    // Host is now `cfg.hyperliquid_ws_host` (closes the 8d deferral);
    // WS path stays a cli const. Coin table building is unaffected by
    // discovery (unlike OKX it needs no per-coin metadata).
    const HL_WS_PATH: &str = "/ws";
    let hl_boot = match boot.hl_spec.as_deref().map(str::trim) {
        Some(spec) if !spec.is_empty() => {
            let coins = match cli::build_hl_coin_table(spec) {
                Ok(t) => t,
                Err(reason) => {
                    error!(reason, spec, "bad --hl-coins");
                    return ExitCode::from(1);
                }
            };
            let (hl_host, hl_port) = match cli::split_host_port(&cfg.hyperliquid_ws_host, 443) {
                Ok(v) => v,
                Err(reason) => {
                    error!(reason, host = %cfg.hyperliquid_ws_host, "bad hyperliquid_ws_host");
                    return ExitCode::from(1);
                }
            };
            let hl_ep = match WssEndpoint::resolve(hl_host, hl_port, HL_WS_PATH) {
                Ok(e) => e,
                Err(e) => {
                    error!(error = ?e, "hyperliquid DNS failed");
                    return ExitCode::from(1);
                }
            };
            Some((coins, hl_ep))
        }
        _ => None,
    };

    // -- 8g §4.3: boot-universe snapshot for the ruleset validator --
    // Built ONCE here — after 8e discovery gated the venue tables,
    // before any ingress thread spawns (the tables move into their
    // spawn calls below). Sorted strict-ascending, deduped; feeds
    // `spawn_ai` → `RulesetSidePath` (§4.2 rule-6 membership checks).
    // M1: spawn-aligned — every PM token + every Binance instrument
    // (spot + USDS-M) this boot wires.
    let pm_syms: Vec<core_types::SymbolId> =
        boot.allocated.pm_tokens.iter().map(|t| t.sym).collect();
    let bn_syms: Vec<core_types::SymbolId> = boot
        .allocated
        .bn_spot
        .iter()
        .chain(boot.allocated.bn_usdm.iter())
        .map(|i| i.sym)
        .collect();
    let ai_universe = cli::build_ai_universe(
        &pm_syms,
        &bn_syms,
        okx_boot.as_ref().map(|(t, _)| t),
        deribit_boot.as_ref().map(|(t, _)| t),
        hl_boot.as_ref().map(|(t, _)| t),
    );
    info!(symbols = ai_universe.len(), "ai: ruleset boot-universe snapshot built");

    // -- Allocate rings + split into producer/consumer halves --
    //
    // Phase 8a lane layout: rings.tick is indexed by VenueId
    // (0=PM, 1=BN, 2=OKX, 3=Deribit, 4=HL). Lane 2 gains its
    // producer below when `--okx-symbols` is set (Phase 8b),
    // lane 3 when `--deribit-symbols` is set (Phase 8c) and
    // lane 4 when `--hl-coins` is set (Phase 8d); an unspawned
    // venue's producer is deliberately dropped, leaving a
    // permanently-empty ring the engine drains for two atomic
    // loads per iteration (§3.3). Fill-lane producers arrive with
    // the venue dispatchers in 8j; paper fills flow through the
    // engine's dispatcher pump (D3).
    let rings = Rings::new();
    let (pm_prod, pm_lane_cons) = rings.tick[0].clone().split();
    let (bn_prod, bn_lane_cons) = rings.tick[1].clone().split();
    let (okx_prod, okx_lane_cons) = rings.tick[2].clone().split();
    let (deribit_prod, deribit_lane_cons) = rings.tick[3].clone().split();
    let (hl_prod, hl_lane_cons) = rings.tick[4].clone().split();
    let (rpc_prod, rpc_cons) = rings.rpc_signal.clone().split();
    let fill_lane_cons = {
        let (_f0p, f0) = rings.fill[0].clone().split();
        let (_f1p, f1) = rings.fill[1].clone().split();
        let (_f2p, f2) = rings.fill[2].clone().split();
        let (_f3p, f3) = rings.fill[3].clone().split();
        [f0, f1, f2, f3]
    };
    // AI command lane (Phase 8f). The producer half feeds the
    // `ingress-ai` thread (spawned below, gated on
    // AI_INGRESS_HMAC_KEY); when the key is absent the producer is
    // dropped and the engine's AI lane reads permanently empty — the
    // §3.3 unspawned shape.
    let (ai_prod, ai_lane_cons) = rings.ai.clone().split();
    let ai_status = std::sync::Arc::new(cli::AiIngressStatus::new());
    // Ruleset-table handoff ring (Phase 8g §6, item 7): the producer
    // half rides with the AI lane into `spawn_ai`; the consumer half
    // rides in `Consumers` to the engine, which pops it immediately
    // before the AI-cmd drain each iteration and hands slots to the
    // strategy's `on_ruleset_table` hook (→ the set's vm member —
    // documented copy #2). Key-unset boots drop the producer and the
    // lane reads empty forever (§3.3 unspawned shape).
    let (ruleset_table_prod, ruleset_table_cons) = rings.ruleset_tables.clone().split();

    // -- Per-ingress status slots (D7) --
    let statuses = std::sync::Arc::new(cli::IngressStatusSet::new());
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // -- Build observability surfaces --
    // Built here (before any ingress thread spawns) rather than after
    // — Part B.4's two §6.5 capture gauges are set from inside each
    // spawn wrapper thread itself, so the registry + gauge ids must
    // already exist at spawn time.
    let enable_metrics = args.metrics || args.tui;
    let enable_tui = args.tui;
    let obs = match Observability::build(enable_metrics, enable_tui) {
        Ok(o) => o.with_ingress_statuses(statuses.clone()),
        Err(reason) => {
            error!(reason, "observability build failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    // Resolve latency-dump destination. Defaults to
    // `<cfg.log_dir>/latency` so an operator who already set
    // `MULTIVENUE_LOG_DIR` doesn't need a second flag.
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

    // §6.1 boot-discovery coverage gauges — static boot-time counts,
    // set once (not mirrored on a cadence like the §6.4 counters).
    if let (Some(reg), Some(ids)) = (obs.metrics.as_ref(), obs.counter_ids.as_ref()) {
        reg.gauge(ids.coverage_pm).set(discovery.pm.configured as i64);
        reg.gauge(ids.coverage_okx)
            .set(discovery.okx.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_deribit)
            .set(discovery.deribit.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_hyperliquid)
            .set(discovery.hl.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_binance)
            .set(discovery.bn.map(|c| c.configured).unwrap_or(0) as i64);
        // M2.1/M2.2/M2.4: capped options chain sizes this boot
        // (0 = lane off).
        reg.gauge(ids.deribit_options_selected)
            .set(discovery.deribit_options.len() as i64);
        reg.gauge(ids.okx_options_selected)
            .set(discovery.okx_options.len() as i64);
        reg.gauge(ids.binance_options_selected)
            .set(discovery.bn_options.len() as i64);
    }

    // Per-venue (registry, gauge-ids) pair for the §6.5 capture
    // metrics — `None` end-to-end when `--metrics`/`--tui` are off.
    let capture_metrics_for = |ids: Option<cli::CaptureGaugeIds>| -> cli::CaptureMetrics {
        match (obs.metrics.as_ref(), ids) {
            (Some(reg), Some(ids)) => Some((reg.clone(), ids)),
            _ => None,
        }
    };

    // -- Spawn ingress threads --

    // D1 fix + M1 multi-market: the PM symbol map carries EVERY
    // configured token id → sym pair. Venue-wide REST discovery
    // (above) validated each against the live venue.
    let pm_map = ingress_polymarket::run_loop::SymbolMap::from_pairs(
        boot.allocated
            .pm_tokens
            .iter()
            .map(|t| (t.token_id.clone().into_bytes(), t.sym)),
    );
    info!(
        markets = boot.allocated.pm_tokens.len(),
        first_sym = boot.allocated.pm_tokens[0].sym,
        "polymarket: symbol map configured"
    );
    let pm_id_bytes: Vec<Vec<u8>> = boot
        .allocated
        .pm_tokens
        .iter()
        .map(|t| t.token_id.clone().into_bytes())
        .collect();
    let pm_handle = match spawn_polymarket(
        pm_ep,
        tls_config.clone(),
        pm_map,
        pm_id_bytes,
        pm_prod,
        statuses.polymarket.clone(),
        1,
        &run_dir,
        epoch_ns,
        raw_tap_cfg.pm,
        capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_pm)),
    ) {
        Ok(h) => h,
        Err(e) => {
            error!(error = ?e, "polymarket: capture open failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    handles.push(pm_handle);

    // Binance: the legacy single-stream lane for one-symbol boots
    // (byte-identical pre-M1, soak-proven), the M1 multi-connection
    // lane whenever the universe wires more than one BN instrument —
    // or (M2.4) whenever the eapi options chain is on: the eapi slot
    // is a MultiConn slot, keeping the venue single-writer.
    let bn_total = boot.allocated.bn_spot.len() + boot.allocated.bn_usdm.len();
    let bn_eapi_on = !discovery.bn_options.is_empty();
    let bn_handle = if bn_total > 1 || bn_eapi_on {
        let mut specs: Vec<cli::BinanceConnSpec> =
            Vec::with_capacity(bn_total + usize::from(bn_eapi_on));
        for inst in &boot.allocated.bn_spot {
            specs.push(cli::BinanceConnSpec {
                host: cfg.binance_ws_host.clone(),
                path: format!("/ws/{}@bookTicker", inst.name),
                sym: inst.sym,
                eapi: None,
            });
        }
        for inst in &boot.allocated.bn_usdm {
            specs.push(cli::BinanceConnSpec {
                host: cfg.binance_fut_ws_host.clone(),
                path: format!("/ws/{}@bookTicker", inst.name),
                sym: inst.sym,
                eapi: None,
            });
        }
        if bn_eapi_on {
            // M2.4: one combined-stream slot carries every selected
            // option ticker + one index stream per underlying (all
            // stream names lowercased; no subscribe frames — the
            // house direct-URL pattern).
            let mut table = ingress_binance::eapi::EapiSymbolTable::new();
            let mut streams = String::new();
            for (symbol, sym, uly_idx) in &discovery.bn_options {
                if let Err(e) = table.insert(symbol.as_bytes(), *sym, *uly_idx) {
                    error!(?e, symbol = %symbol, "binance: eapi table build failed");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
                if !streams.is_empty() {
                    streams.push('/');
                }
                streams.push_str(&symbol.to_ascii_lowercase());
                streams.push_str("@ticker");
            }
            for uly in &boot.bn_options.underlyings {
                streams.push('/');
                streams.push_str(&uly.to_ascii_lowercase());
                streams.push_str("@index");
            }
            specs.push(cli::BinanceConnSpec {
                host: cfg.binance_eapi_ws_host.clone(),
                // The documented eapi combined base (legacy docs +
                // the live nbstream ALB). TEMPORARILY UNREACHABLE
                // from this network as of 2026-08-22 (404/403 on
                // every candidate route while eapi REST serves —
                // forensics in docs/m2-progress.md); the slot retries
                // harmlessly and BINANCE_EAPI_WS_HOST is the
                // override once an endpoint is confirmed.
                path: format!("/eoptions/stream?streams={streams}"),
                sym: 0,
                eapi: Some((table, boot.bn_options.underlyings.clone())),
            });
        }
        if bn_eapi_on {
            // Loud endpoint provenance — the options WS base has
            // churned (nbstream/fstream/vstream history) and can be
            // geo-gated; the operator override is the escape hatch.
            info!(
                host = %cfg.binance_eapi_ws_host,
                streams = discovery.bn_options.len() + boot.bn_options.underlyings.len(),
                "binance: eapi options combined-stream slot (override via BINANCE_EAPI_WS_HOST)"
            );
        }
        info!(
            conns = specs.len(),
            spot = boot.allocated.bn_spot.len(),
            usdm = boot.allocated.bn_usdm.len(),
            eapi_options = discovery.bn_options.len(),
            "binance: M1 multi-connection lane"
        );
        match cli::spawn_binance_multi(
            specs,
            tls_config.clone(),
            bn_prod,
            statuses.binance.clone(),
            2,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.bn,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_bn)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "binance: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
    } else {
        let bn_path = format!("/ws/{}@bookTicker", boot.allocated.bn_spot[0].name);
        let bn_ep = match WssEndpoint::resolve(&cfg.binance_ws_host, 443, &bn_path) {
            Ok(e) => e,
            Err(e) => {
                error!(error = ?e, "binance DNS failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        match spawn_binance(
            bn_ep,
            tls_config.clone(),
            boot.allocated.bn_spot[0].sym,
            bn_prod,
            statuses.binance.clone(),
            2,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.bn,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_bn)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "binance: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
    };
    handles.push(bn_handle);

    // OKX rides core 5 per the §9 core map (rpc keeps 3, ai takes 4).
    if let Some((okx_symbols, okx_ep)) = okx_boot {
        info!(
            instruments = okx_symbols.len(),
            depth = boot.okx_depth,
            "okx: starting ingress thread"
        );
        let okx_handle = match spawn_okx(
            okx_ep,
            tls_config.clone(),
            okx_symbols,
            boot.okx_depth,
            // M2.3: family-keyed opt-summary subscription args.
            boot.okx_options.underlyings.clone(),
            okx_prod,
            statuses.okx.clone(),
            5,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.okx,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_okx)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "okx: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(okx_handle);
    } else {
        info!("--okx-symbols empty / unset; OKX ingress thread not started");
        // Drop the producer side so the lane stays a permanently-
        // empty ring (the unspawned-venue shape, §3.3).
        drop(okx_prod);
    }

    // Deribit rides core 6 per the §9 core map.
    if let Some((deribit_symbols, deribit_ep)) = deribit_boot {
        info!(
            instruments = deribit_symbols.len(),
            depth = boot.deribit_depth,
            "deribit: starting ingress thread"
        );
        let deribit_handle = match spawn_deribit(
            deribit_ep,
            tls_config.clone(),
            deribit_symbols,
            boot.deribit_depth,
            deribit_prod,
            statuses.deribit.clone(),
            6,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.deribit,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_deribit)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "deribit: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(deribit_handle);
    } else {
        info!("--deribit-symbols empty / unset; Deribit ingress thread not started");
        // Drop the producer side so the lane stays a permanently-
        // empty ring (the unspawned-venue shape, §3.3).
        drop(deribit_prod);
    }

    // Hyperliquid rides core 7 per the §9 core map.
    if let Some((hl_coins, hl_ep)) = hl_boot {
        info!(coins = hl_coins.len(), "hyperliquid: starting ingress thread");
        let hl_handle = match spawn_hyperliquid(
            hl_ep,
            tls_config.clone(),
            hl_coins,
            hl_prod,
            statuses.hyperliquid.clone(),
            7,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.hl,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_hyperliquid)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "hyperliquid: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(hl_handle);
    } else {
        info!("--hl-coins empty / unset; Hyperliquid ingress thread not started");
        // Drop the producer side so the lane stays a permanently-
        // empty ring (the unspawned-venue shape, §3.3).
        drop(hl_prod);
    }

    if let Some(polygon_path) = args.polygon_path {
        match WssEndpoint::resolve(&cfg.alchemy_host, 443, &polygon_path) {
            Ok(rpc_ep) => {
                match spawn_rpc(
                    rpc_ep,
                    tls_config.clone(),
                    rpc_prod,
                    statuses.rpc.clone(),
                    3,
                    &run_dir,
                    epoch_ns,
                    raw_tap_cfg.rpc,
                    capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_rpc)),
                ) {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        error!(error = ?e, "rpc: capture open failed");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            }
            Err(e) => {
                error!(error = ?e, "RPC DNS failed; skipping rpc ingress");
            }
        }
    } else {
        warn!("--polygon-path not provided; RPC ingress thread not started");
    }

    // -- AI-command ingress (Phase 8f; opt-in via AI_INGRESS_HMAC_KEY
    // in .env) --
    // Key semantics: ABSENT/empty ⇒ thread not started (back-compat
    // with pre-8f .env files); PRESENT but unparseable ⇒ fatal boot
    // error — a typo'd key must never silently disable the AI lane.
    // The parsed key is moved into the thread and never logged.
    match std::env::var("AI_INGRESS_HMAC_KEY") {
        Ok(hex) if !hex.trim().is_empty() => match cli::parse_ai_hmac_key(&hex) {
            Ok(key) => {
                info!(
                    sock = %cfg.ai_ingress_sock,
                    ruleset_dir = %cfg.ai_ruleset_dir,
                    "ingress-ai: starting thread"
                );
                let ai_handle = match cli::spawn_ai(
                    PathBuf::from(&cfg.ai_ingress_sock),
                    PathBuf::from(&cfg.ai_ruleset_dir),
                    key,
                    ai_prod,
                    ruleset_table_prod,
                    ai_universe,
                    ai_status.clone(),
                    4,
                    &run_dir,
                    epoch_ns,
                    capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_ai)),
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        error!(error = ?e, "ingress-ai: capture open failed");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                };
                handles.push(ai_handle);
            }
            Err(reason) => {
                // `reason` is a static description; no key material.
                error!(reason, "AI_INGRESS_HMAC_KEY present but invalid — refusing to boot");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        },
        _ => {
            info!("AI_INGRESS_HMAC_KEY unset; ingress-ai thread not started");
            drop(ai_prod);
            // Same unspawned shape for the table ring: no producer,
            // the parked consumer reads empty forever.
            drop(ruleset_table_prod);
        }
    }

    // -- Engine-thread fills capture (Phase 8f item 6) --
    // engine-fills.pmlr in the per-run capture directory: the
    // positions/P&L feed for the research loop. Open failure is a
    // fatal boot error (§6.5 stance: capture is the product).
    let obs = match cli::open_fills_capture(&run_dir, epoch_ns) {
        Ok(cap) => obs.with_fills_capture(cap),
        Err(e) => {
            error!(error = ?e, dir = %run_dir.display(), "fills capture open failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };

    // -- Main thread: real engine loop until SIGINT --
    let cons = Consumers {
        tick_lanes: [
            pm_lane_cons,
            bn_lane_cons,
            okx_lane_cons,
            deribit_lane_cons,
            hl_lane_cons,
        ],
        rpc_signal: rpc_cons,
        fill_lanes: fill_lane_cons,
        ai_cmds: ai_lane_cons,
        ai_status,
        ruleset_tables: ruleset_table_cons,
    };
    let engine_cfg = EngineConfig {
        // M1: pairs from the resolved universe ([pairs] map or the
        // default first-PM × first-BN-spot).
        pairs: boot
            .allocated
            .pairs
            .iter()
            .map(|&(pm, bn)| StrategyPair {
                polymarket: pm,
                binance: bn,
            })
            .collect(),
        threshold_1e6: args.threshold_1e6,
        qty_1e6: args.qty_1e6,
        cooldown_ns: args.cooldown_ns,
    };

    // Observability (`obs`) + latency-dump destination were already
    // built above, before the ingress spawns (Part B.4 needs the
    // registry at spawn time).

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
                    // Non-fatal serve events land here so they carry
                    // the standard tracing timestamp (G1 remediation
                    // item 2 — the old in-crate eprintln had neither
                    // timestamp nor level). WARN, not ERROR: scrape
                    // clients retry; the soak "no ERROR" grep must not
                    // trip on a benign scrape hiccup.
                    let on_event = |ev: core_metrics::MetricsServeEvent<'_>| match ev {
                        core_metrics::MetricsServeEvent::ConnError(e) => {
                            warn!(error = %e, "metrics: connection error")
                        }
                        core_metrics::MetricsServeEvent::AcceptError(e) => {
                            warn!(error = %e, "metrics: accept error")
                        }
                    };
                    if let Err(e) = core_metrics::serve_metrics(bind, reg, stop_ref, on_event) {
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
            let mapping = vec![(boot.allocated.pm_tokens[0].sym, kw, n as u8)];
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
        (name @ ("all" | "ai-exec" | "vm"), _live) => {
            // Phase 8f item 7: the composed StrategySet. `all` means
            // "every built member the given flags can boot" —
            // latency-arb from the mandatory pair flags, ev/cross-arb/
            // rule-tree only when their config flags are present,
            // ai-exec and vm unconditionally (neither has boot
            // config; items 8 / 8g-6) (members without config boot
            // inert; see engine_loop_set_full docs). `ai-exec` (item
            // 8b) and `vm` (8g item 6) are single-bit sets per §7
            // "single name = single bit" — no standalone path exists
            // for either; vm boots inert until a ruleset table is
            // staged + committed (8g §7.3 — normal, not an error).
            // PAPER-only until the 8i RiskGate lands — the set has no
            // live arm.
            let requested =
                strategy_set::mask_for_name(name).expect("matched names are valid mask names");
            let ev_path = args.artifacts_path.clone();
            let owned_groups: Vec<Vec<core_types::SymbolId>> = match args.groups.as_deref() {
                Some(spec) => spec
                    .split(';')
                    .map(|grp| {
                        grp.split(',')
                            .filter_map(|s| s.trim().parse::<u32>().ok())
                            .collect()
                    })
                    .filter(|v: &Vec<u32>| !v.is_empty())
                    .collect(),
                None => Vec::new(),
            };
            let groups_ref: Vec<&[core_types::SymbolId]> =
                owned_groups.iter().map(|v| v.as_slice()).collect();
            // Rule mapping: same v1 shape as the standalone rule-tree
            // arm (every rule → --polymarket-sym-id, "halving" kw).
            let mut kw = [0u8; 16];
            let n = b"halving".len().min(16);
            kw[..n].copy_from_slice(&b"halving"[..n]);
            let mapping = vec![(boot.allocated.pm_tokens[0].sym, kw, n as u8)];
            let rules = args
                .rules_path
                .as_deref()
                .map(|rp| (rp, mapping.as_slice()));
            info!("running strategy-set PAPER — no orders will be submitted");
            engine_loop_set_full(
                cons,
                engine_cfg,
                clob_dispatcher::PaperDispatcher::new(),
                obs,
                requested,
                ev_path.as_deref(),
                &groups_ref,
                rules,
            )
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
