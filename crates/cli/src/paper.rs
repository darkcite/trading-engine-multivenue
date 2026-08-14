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
//! | T5     | ingress-okx (v5 public WSS; only with `--okx-symbols`) |
//! | T6     | ingress-deribit (JSON-RPC WSS; only with `--deribit-symbols`) |
//! | T7     | ingress-hyperliquid (public WSS; only with `--hl-coins`) |
//!
//! Cores 0..=7 are pinned (Linux only) per the §9 core map. On
//! non-Linux we log a single warning and let the OS scheduler do as
//! it pleases.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clob_dispatcher::{OrderDispatch, PaperDispatcher};
// LiveDispatcher is re-exported through cli::paper so the binary
// doesn't have to depend on clob-dispatcher directly.
pub use clob_dispatcher::{LiveDispatcher, LiveDispatcherErr};
use core_io::{PmlrCapture, TapCfg, TapMode};
use core_metrics::{GaugeId, IngressState, IngressStatus, MetricsRegistry};
use core_net::{Backoff, Keepalive, KeepaliveCfg, TlsTransport};
use core_ring::{Consumer, Producer, Ring};
use core_time::now_ns;
use core_types::{make_symbol_id, Fill, Signal, SymbolId, Tick, VenueId};
use engine::{Engine, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES, SIGNAL_RING_SIZE, TICK_RING_SIZE};
use rustls_pki_types::ServerName;
use strategy_latency_arb::LatencyArb;

use ingress_binance::run_loop as bwl;
use ingress_deribit::run_loop as dwl;
use ingress_hyperliquid::run_loop as hwl;
use ingress_okx::run_loop as owl;
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

/// Keepalive policy per WSS ingress (D5/D6). Ping intervals sit
/// well under each venue's idle cutoff; idle timeouts catch
/// half-open TCP that `Ok(0)` never surfaces. The RPC feed's probe
/// is its own 2 s `eth_blockNumber` poll — keepalive only supplies
/// the reconnect deadline there.
const PM_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 10_000_000_000,
    idle_timeout_ns: 30_000_000_000,
};
/// Binance server-pings every ~20 s; our proactive ping is a cheap
/// second line of defense.
const BN_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 15_000_000_000,
    idle_timeout_ns: 45_000_000_000,
};
/// OKX cuts connections that stay silent for 30 s (plan §4.1); the
/// venue-literal `ping` text frame goes out at 25 s and anything
/// quieter than 40 s is a dead session.
const OKX_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 25_000_000_000,
    idle_timeout_ns: 40_000_000_000,
};
/// Deribit has no WS-level ping: the run loop arms
/// `public/set_heartbeat {"interval":15}` and answers venue
/// `test_request`s with `public/test`; the venue closes the socket
/// on an unanswered test_request, so the idle budget is ~2× the
/// 15 s heartbeat interval. `SendPing` fires a proactive
/// `public/test` probe at 20 s of silence.
const DERIBIT_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 20_000_000_000,
    idle_timeout_ns: 30_000_000_000,
};
/// Hyperliquid cuts connections that stay silent for 60 s (§4.3);
/// the venue-specific `{"method":"ping"}` text frame goes out at
/// 50 s and anything quieter than 60 s is a dead session.
const HL_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 50_000_000_000,
    idle_timeout_ns: 60_000_000_000,
};
/// Polygon RPC: newHeads every ~2 s + our own 2 s poll → anything
/// quieter than 30 s is a dead session.
const RPC_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 10_000_000_000,
    idle_timeout_ns: 30_000_000_000,
};

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

/// Split a `core_config::Config` host field that may carry an
/// embedded `:port` (e.g. `okx_ws_host` defaults to
/// `"ws.okx.com:8443"`) into `(host, port)`. Hosts with no `:` use
/// `default_port` (8e §9 — REST hosts never carry a port; the OKX WS
/// host does because the venue's public WS runs on a non-443 port).
pub fn split_host_port(host_cfg: &str, default_port: u16) -> Result<(&str, u16), &'static str> {
    match host_cfg.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| "config: bad :port in host string")?;
            if h.is_empty() {
                return Err("config: empty host before :port");
            }
            Ok((h, port))
        }
        None => Ok((host_cfg, default_port)),
    }
}

/// Build `<log_dir>/run-<epoch_ns>` (epoch_ns = wall-clock ns at
/// boot) and create it. This is the Phase-8e §6.5 capture run
/// directory — every spawned ingress's `PmlrCapture` files land here.
/// Boot-only; the caller logs the resulting directory.
pub fn new_capture_run_dir(log_dir: &str) -> io::Result<(PathBuf, u64)> {
    let epoch_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut dir = PathBuf::from(log_dir);
    dir.push(format!("run-{epoch_ns}"));
    std::fs::create_dir_all(&dir)?;
    Ok((dir, epoch_ns))
}

// ---------------------------------------------------------------
// Ring + Producer/Consumer alias bundles
// ---------------------------------------------------------------

/// Every ring the cli owns at boot. Sizes are pulled from the
/// ingress + engine crates so the cli doesn't restate them; the
/// `const` equality below is structural — the compiler enforces
/// that the engine and ingress agree on the single Phase-8a
/// standardized tick capacity (§3.3).
const _: () = {
    assert!(pwl::DEFAULT_TICK_RING_CAP == TICK_RING_SIZE);
    assert!(bwl::DEFAULT_TICK_RING_CAP == TICK_RING_SIZE);
    assert!(owl::TICK_RING_CAP == TICK_RING_SIZE);
    assert!(dwl::TICK_RING_CAP == TICK_RING_SIZE);
    assert!(hwl::TICK_RING_CAP == TICK_RING_SIZE);
    assert!(rwl::DEFAULT_SIGNAL_RING_CAP == SIGNAL_RING_SIZE);
};

/// All preallocated rings the engine + cli touch.
pub struct Rings {
    /// One tick ring per venue lane, indexed by `VenueId as usize`
    /// (0 = Polymarket, 1 = Binance, 2 = OKX, 3 = Deribit,
    /// 4 = Hyperliquid). Lanes without a spawned ingress simply
    /// never see a producer push — the engine drains them empty.
    pub tick: [Arc<Ring<Tick, TICK_RING_SIZE>>; NUM_TICK_LANES],
    /// Signal ring for Polygon newHeads — feeds the engine.
    pub rpc_signal: Arc<Ring<Signal, SIGNAL_RING_SIZE>>,
    /// Signal ring for RSS news items — drained at cli level.
    /// Stays in place untouched until Stage 2 (RSS retirement, §8.1).
    pub rss_signal: Arc<Ring<Signal, 1024>>,
    /// One fill ring per execution lane (`engine::fill_lane_of`).
    /// Live dispatchers gain producers in Phase 8j; until then the
    /// engine's dispatcher fill pump (D3) is the only fill source.
    pub fill: [Arc<Ring<Fill, FILL_RING_SIZE>>; NUM_FILL_LANES],
}

impl Rings {
    /// Allocate all rings. Single call; never used on hot path.
    pub fn new() -> Self {
        Self {
            tick: [Ring::new(), Ring::new(), Ring::new(), Ring::new(), Ring::new()],
            rpc_signal: Ring::new(),
            rss_signal: Ring::new(),
            fill: [Ring::new(), Ring::new(), Ring::new(), Ring::new()],
        }
    }
}

impl Default for Rings {
    fn default() -> Self {
        Self::new()
    }
}

/// One [`IngressStatus`] slot per ingress thread (D7). Allocated at
/// boot, cloned into the spawn wrappers (writers) and into
/// [`Observability`] (reader). RSS gets a slot for uniformity even
/// though its thread only reports coarse Up/Down.
pub struct IngressStatusSet {
    /// Polymarket WSS thread.
    pub polymarket: Arc<IngressStatus>,
    /// Binance WSS thread.
    pub binance: Arc<IngressStatus>,
    /// OKX WSS thread (Phase 8b). Stays Down when `--okx-symbols`
    /// is empty and the thread is never spawned.
    pub okx: Arc<IngressStatus>,
    /// Deribit WSS thread (Phase 8c). Stays Down when
    /// `--deribit-symbols` is empty and the thread is never spawned.
    pub deribit: Arc<IngressStatus>,
    /// Hyperliquid WSS thread (Phase 8d). Stays Down when
    /// `--hl-coins` is empty and the thread is never spawned.
    pub hyperliquid: Arc<IngressStatus>,
    /// Polygon RPC WSS thread.
    pub rpc: Arc<IngressStatus>,
    /// RSS poller thread.
    pub rss: Arc<IngressStatus>,
}

impl IngressStatusSet {
    /// Allocate all seven slots (boot only).
    pub fn new() -> Self {
        Self {
            polymarket: Arc::new(IngressStatus::new()),
            binance: Arc::new(IngressStatus::new()),
            okx: Arc::new(IngressStatus::new()),
            deribit: Arc::new(IngressStatus::new()),
            hyperliquid: Arc::new(IngressStatus::new()),
            rpc: Arc::new(IngressStatus::new()),
            rss: Arc::new(IngressStatus::new()),
        }
    }
}

impl Default for IngressStatusSet {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Thread bootstrappers
// ---------------------------------------------------------------

/// Spawn the Polymarket CLOB ingress thread. `producer` is the SPSC
/// producer half of the tick ring; the consumer half stays on the
/// main thread. Opens this venue's `PmlrCapture` (label `"pm"`)
/// **before** spawning — capture-open failure is a fatal boot error
/// (§6.5: capture is the Stage-1 product), so the caller sees it via
/// the returned `Err` rather than a panic deep inside the thread.
/// Returns a [`JoinHandle`] the caller will join in reverse boot
/// order during shutdown.
#[allow(clippy::too_many_arguments)]
pub fn spawn_polymarket(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    symbol_map: pwl::SymbolMap,
    asset_id: Vec<u8>,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = PmlrCapture::open(run_dir, "pm", epoch_ns, tap_cfg)?;
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "pm", VenueId::Polymarket.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-polymarket".into()),
        "ingress-polymarket",
        move || {
            log_pin_outcome("polymarket", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "polymarket: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = pwl::Driver::new(now_ns(), &asset_id);
            let mut keepalive = Keepalive::new(PM_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "polymarket: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "polymarket: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

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
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "polymarket: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, pwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // A session that moved data restarts the schedule;
                // a flapping endpoint keeps escalating (D8).
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
}

/// Spawn the Binance bookTicker ingress thread. One thread per
/// symbol — caller spawns N of these if they want multi-symbol
/// coverage. See [`spawn_polymarket`] for the capture-open /
/// fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_binance(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    sym: core_types::SymbolId,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = PmlrCapture::open(run_dir, "bn", epoch_ns, tap_cfg)?;
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "bn", VenueId::Binance.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name(format!("ingress-binance-{sym}")),
        "ingress-binance",
        move || {
            log_pin_outcome("binance", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "binance: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = bwl::Driver::new(now_ns(), sym);
            let mut keepalive = Keepalive::new(BN_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "binance: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "binance: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

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
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "binance: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, bwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
}

/// Build the boot-time OKX `instId → SymbolId` table from the
/// comma-separated `--okx-symbols` value, gated on `discovery` (the
/// Phase-8e REST instrument table — see `boot_discovery::run_okx`).
/// The i-th instrument (0-based) is allocated
/// `make_symbol_id(VenueId::Okx, i + 1)` — ordinals follow flag
/// order, 1-based so ordinal 0 never aliases an unconfigured id
/// (§3.1), and ordinal allocation does NOT depend on whether the
/// venue actually has the instrument: it's computed before the
/// discovery lookup so it stays stable across venue universe churn.
///
/// A configured `instId` the venue doesn't currently list live is
/// silently **omitted** from the returned table (not a hard boot
/// error) — the §6.1 coverage pass in `boot_discovery` already logged
/// it as MISSING and decided whether that's fatal (live) or a warning
/// (paper). Every *present* row's [`ingress_okx::OkxInstType`] comes
/// from the discovered `instType` — the old instId-suffix hack is
/// gone, so `OkxSymbolTable::insert` cannot succeed without it.
///
/// Still fails fast on an empty item, a duplicate `instId` (checked
/// against the raw configured list, independent of venue liveness),
/// or more than [`ingress_okx::OKX_MAX_SYMBOLS`] instruments — boot
/// refuses to start rather than run with a venue map that doesn't
/// match the operator's intent.
pub fn build_okx_symbol_table(
    spec: &str,
    discovery: &ingress_okx::discovery::OkxDiscovery,
) -> Result<ingress_okx::OkxSymbolTable, &'static str> {
    let mut table = ingress_okx::OkxSymbolTable::new();
    // Raw-spec dedupe list, independent of what actually gets
    // inserted (a MISSING item must still trip the duplicate check).
    let mut seen: [&str; ingress_okx::OKX_MAX_SYMBOLS] = [""; ingress_okx::OKX_MAX_SYMBOLS];
    let mut n_seen: usize = 0;
    let mut ordinal: u32 = 0;
    for item in spec.split(',') {
        let inst_id = item.trim();
        if inst_id.is_empty() {
            return Err("okx: empty instId in --okx-symbols");
        }
        if seen[..n_seen].contains(&inst_id) {
            return Err("okx: duplicate instId in --okx-symbols");
        }
        if n_seen >= ingress_okx::OKX_MAX_SYMBOLS {
            return Err("okx: --okx-symbols exceeds OKX_MAX_SYMBOLS instruments");
        }
        seen[n_seen] = inst_id;
        n_seen += 1;

        ordinal += 1;
        let sym = make_symbol_id(VenueId::Okx, ordinal);
        let Some(row) = discovery.find(inst_id.as_bytes()).filter(|r| r.live) else {
            // MISSING — already logged by boot_discovery's coverage
            // pass; the table just doesn't carry a row for it.
            continue;
        };
        match table.insert(inst_id.as_bytes(), sym, row.inst_type) {
            Ok(()) => {}
            Err(ingress_okx::SymbolTableErr::Full) => {
                return Err("okx: --okx-symbols exceeds OKX_MAX_SYMBOLS instruments");
            }
            Err(ingress_okx::SymbolTableErr::TooLong) => {
                return Err("okx: instId in --okx-symbols exceeds OKX_INST_ID_MAX bytes");
            }
            Err(ingress_okx::SymbolTableErr::Empty) => {
                return Err("okx: empty instId in --okx-symbols");
            }
        }
    }
    Ok(table)
}

/// Spawn the OKX v5 public-WS ingress thread (Phase 8b). One thread
/// covers every configured instrument — the driver batches all
/// `(channel × instId)` pairs into a single subscribe op (§4.1).
/// `depth_enabled` adds the 400-level `books` channel per
/// instrument (`--okx-depth`; capture + integrity only, §4.5). See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_okx(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    symbols: ingress_okx::OkxSymbolTable,
    depth_enabled: bool,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = PmlrCapture::open(run_dir, "okx", epoch_ns, tap_cfg)?;
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "okx", VenueId::Okx.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-okx".into()),
        "ingress-okx",
        move || {
            log_pin_outcome("okx", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "okx: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = owl::Driver::new(now_ns(), symbols, depth_enabled);
            let mut keepalive = Keepalive::new(OKX_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "okx: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "okx: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

                let res = owl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "okx: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, owl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
}

/// Build the boot-time Deribit `instrument_name → SymbolId` table
/// from the comma-separated `--deribit-symbols` value. The i-th
/// instrument (0-based) is allocated
/// `make_symbol_id(VenueId::Deribit, i + 1)` — ordinals follow flag
/// order, 1-based so ordinal 0 never aliases an unconfigured id
/// (§3.1; venue REST discovery replaces this manual allocation in
/// the Phase-8e boot coverage audit).
///
/// Fails fast on an empty item, a duplicate instrument, an over-long
/// instrument, an instrument containing `.` (would corrupt channel-
/// name parsing), or more than
/// [`ingress_deribit::DERIBIT_MAX_SYMBOLS`] instruments — boot
/// refuses to start rather than run with a venue map that doesn't
/// match the operator's intent.
pub fn build_deribit_symbol_table(
    spec: &str,
) -> Result<ingress_deribit::DeribitSymbolTable, &'static str> {
    let mut table = ingress_deribit::DeribitSymbolTable::new();
    let mut ordinal: u32 = 0;
    for item in spec.split(',') {
        let instrument = item.trim();
        if instrument.is_empty() {
            return Err("deribit: empty instrument in --deribit-symbols");
        }
        if table.lookup(instrument.as_bytes()).is_some() {
            return Err("deribit: duplicate instrument in --deribit-symbols");
        }
        ordinal += 1;
        match table.insert(instrument.as_bytes(), make_symbol_id(VenueId::Deribit, ordinal)) {
            Ok(()) => {}
            Err(ingress_deribit::SymbolTableErr::Full) => {
                return Err("deribit: --deribit-symbols exceeds DERIBIT_MAX_SYMBOLS instruments");
            }
            Err(ingress_deribit::SymbolTableErr::TooLong) => {
                return Err(
                    "deribit: instrument in --deribit-symbols exceeds DERIBIT_INSTR_MAX bytes",
                );
            }
            Err(ingress_deribit::SymbolTableErr::Empty) => {
                return Err("deribit: empty instrument in --deribit-symbols");
            }
            Err(ingress_deribit::SymbolTableErr::HasDot) => {
                return Err("deribit: instrument in --deribit-symbols must not contain '.'");
            }
        }
    }
    Ok(table)
}

/// Spawn the Deribit JSON-RPC/WS ingress thread (Phase 8c). One
/// thread covers every configured instrument — the driver batches
/// all `(channel × instrument)` pairs into a single subscribe call
/// (§4.2 credit budget). `depth_enabled` adds the change_id-chained
/// `book.{instr}.100ms` channel per instrument (`--deribit-depth`;
/// capture + integrity only, §4.5). See [`spawn_polymarket`] for the
/// capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_deribit(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    symbols: ingress_deribit::DeribitSymbolTable,
    depth_enabled: bool,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = PmlrCapture::open(run_dir, "deribit", epoch_ns, tap_cfg)?;
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "deribit", VenueId::Deribit.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-deribit".into()),
        "ingress-deribit",
        move || {
            log_pin_outcome("deribit", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "deribit: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = dwl::Driver::new(now_ns(), symbols, depth_enabled);
            let mut keepalive = Keepalive::new(DERIBIT_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "deribit: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "deribit: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

                let res = dwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "deribit: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, dwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
}

/// Build the boot-time Hyperliquid `coin → SymbolId` table from
/// the comma-separated `--hl-coins` value. The i-th coin (0-based)
/// is allocated `make_symbol_id(VenueId::Hyperliquid, i + 1)` —
/// ordinals follow flag order, 1-based so ordinal 0 never aliases
/// an unconfigured id (§3.1; venue REST discovery replaces this
/// manual allocation in the Phase-8e boot coverage audit). HIP-4
/// `#<enc>` outcome coins and spot `@<idx>` pairs are ordinary
/// items — no special syntax.
///
/// Fails fast on an empty item, a duplicate coin, an over-long
/// coin, or more than [`ingress_hyperliquid::HL_MAX_COINS`] coins —
/// boot refuses to start rather than run with a venue map that
/// doesn't match the operator's intent.
pub fn build_hl_coin_table(
    spec: &str,
) -> Result<ingress_hyperliquid::HlCoinTable, &'static str> {
    let mut table = ingress_hyperliquid::HlCoinTable::new();
    let mut ordinal: u32 = 0;
    for item in spec.split(',') {
        let coin = item.trim();
        if coin.is_empty() {
            return Err("hl: empty coin in --hl-coins");
        }
        if table.lookup(coin.as_bytes()).is_some() {
            return Err("hl: duplicate coin in --hl-coins");
        }
        ordinal += 1;
        match table.insert(coin.as_bytes(), make_symbol_id(VenueId::Hyperliquid, ordinal)) {
            Ok(()) => {}
            Err(ingress_hyperliquid::CoinTableErr::Full) => {
                return Err("hl: --hl-coins exceeds HL_MAX_COINS coins");
            }
            Err(ingress_hyperliquid::CoinTableErr::TooLong) => {
                return Err("hl: coin in --hl-coins exceeds HL_COIN_MAX bytes");
            }
            Err(ingress_hyperliquid::CoinTableErr::Empty) => {
                return Err("hl: empty coin in --hl-coins");
            }
        }
    }
    Ok(table)
}

/// Spawn the Hyperliquid public-WS ingress thread (Phase 8d). One
/// thread covers every configured coin — the driver queues one
/// subscribe frame per `(channel × coin)` pair (no batch form,
/// §4.3). There is no depth flag: `l2Book` is always subscribed —
/// it feeds the §6.2 per-coin staleness monitor. See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_hyperliquid(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    coins: ingress_hyperliquid::HlCoinTable,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = PmlrCapture::open(run_dir, "hl", epoch_ns, tap_cfg)?;
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "hl", VenueId::Hyperliquid.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-hyperliquid".into()),
        "ingress-hyperliquid",
        move || {
            log_pin_outcome("hyperliquid", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "hyperliquid: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = hwl::Driver::new(
                now_ns(),
                coins,
                ingress_hyperliquid::HL_STALENESS_BUDGET_NS,
                hwl::HL_SUB_ACK_BUDGET_NS,
            );
            let mut keepalive = Keepalive::new(HL_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "hyperliquid: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "hyperliquid: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

                let res = hwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "hyperliquid: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, hwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // A staleness trip reconnects exactly like
                // IdleTimeout (backoff below) — the next snapshot
                // recovers all state by construction. `gaps_total`
                // was already incremented inside the run loop; no
                // double count here.
                if matches!(res, hwl::RunResult::Stale) {
                    tracing::warn!("hl: staleness trip — reconnecting for fresh snapshots");
                }
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
}

/// Spawn the Polygon JSON-RPC ingress thread. See [`spawn_polymarket`]
/// for the capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_rpc(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    mut producer: Producer<Signal, { rwl::DEFAULT_SIGNAL_RING_CAP }>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    // No `set_tap_venue_byte` call here: RPC (Polygon newHeads) has no
    // `core_types::VenueId` — it's a `Signal`/`SignalSource::Rpc`
    // source, not a market-data venue (`VenueId`'s six variants are
    // PM/BN/OKX/Deribit/HL/Ai — Ai is the distinct, not-yet-spawned
    // claude-worker command feed). The tap header's venue byte stays
    // the `0xFF` "unknown" sentinel; `rpc-raw.tap`'s filename already
    // self-identifies for the offline tooling.
    let mut capture = PmlrCapture::open(run_dir, "rpc", epoch_ns, tap_cfg)?;
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-rpc".into()),
        "ingress-rpc",
        move || {
            log_pin_outcome("rpc", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "rpc: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };

            let mut driver = rwl::Driver::new(now_ns());
            let mut keepalive = Keepalive::new(RPC_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "rpc: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "rpc: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let msgs_before = status.msgs_total();

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
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "rpc: run-loop returned");
                mirror_capture_metrics(&capture_metrics, &capture);
                if matches!(res, rwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                if status.msgs_total() > msgs_before {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            mirror_capture_metrics(&capture_metrics, &capture);
            status.set_state(IngressState::Down);
        },
    ))
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
    status: Arc<IngressStatus>,
    core_id: usize,
) -> JoinHandle<()> {
    spawn_or_die(
        thread::Builder::new().name("ingress-rss".into()),
        "ingress-rss",
        move || {
            log_pin_outcome("rss", core_id);
            if feeds.is_empty() {
                tracing::info!("rss: no feeds configured — thread exiting");
                status.set_state(IngressState::Down);
                return;
            }
            // Coarse status only: the poller owns its own schedule
            // and connection lifecycle (request/response at minutes
            // cadence), so Up-while-running / Down-on-exit is the
            // honest granularity. RSS retires entirely in Stage 2.
            status.set_state(IngressState::Up);
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
            status.set_state(IngressState::Down);
            tracing::info!("rss: ingress thread exiting");
        },
    )
}

// ---------------------------------------------------------------
// Raw-tap flag parsing (--raw-tap / --raw-tap-mode / --raw-tap-budget-mb)
// ---------------------------------------------------------------

/// Per-venue [`TapCfg`], indexed by the same short capture-venue
/// labels `PmlrCapture::open` uses (`pm`/`bn`/`okx`/`rpc`/`deribit`/
/// `hl`). Built by [`parse_raw_tap_flags`] from the `run` command's
/// `--raw-tap*` flags.
#[derive(Copy, Clone, Debug)]
pub struct RawTapConfig {
    /// Tap config for the Polymarket ingress.
    pub pm: TapCfg,
    /// Tap config for the Binance ingress.
    pub bn: TapCfg,
    /// Tap config for the OKX ingress.
    pub okx: TapCfg,
    /// Tap config for the Polygon RPC ingress.
    pub rpc: TapCfg,
    /// Tap config for the Deribit ingress.
    pub deribit: TapCfg,
    /// Tap config for the Hyperliquid ingress.
    pub hl: TapCfg,
}

/// Parse `--raw-tap <CSV|all>` + `--raw-tap-mode <rejects|all>` +
/// `--raw-tap-budget-mb <u64>` into a [`RawTapConfig`]. `raw_tap`
/// absent/empty ⇒ every venue gets [`TapCfg::off`] (default: none).
/// `raw_tap` equal (after trim) to the literal `all` enables every
/// venue; otherwise it's a comma-separated list of venue labels
/// (`pm`/`bn`/`okx`/`rpc`/`deribit`/`hl`), trimmed, non-empty, no
/// duplicates. Every enabled venue shares the same `mode` +
/// `budget_mb` (×1 MiB → `TapCfg::budget_bytes`). Unknown venue
/// labels and a bad `--raw-tap-mode` value both fail fast at parse —
/// boot refuses to start with a raw-tap flag it can't honor.
pub fn parse_raw_tap_flags(
    raw_tap: Option<&str>,
    mode: &str,
    budget_mb: u64,
) -> Result<RawTapConfig, &'static str> {
    let tap_mode = match mode {
        "rejects" => TapMode::Rejects,
        "all" => TapMode::All,
        _ => return Err("--raw-tap-mode must be 'rejects' or 'all'"),
    };
    let budget_bytes = budget_mb.saturating_mul(1024 * 1024);
    let enabled_cfg = TapCfg { mode: tap_mode, budget_bytes };

    let mut cfg = RawTapConfig {
        pm: TapCfg::off(),
        bn: TapCfg::off(),
        okx: TapCfg::off(),
        rpc: TapCfg::off(),
        deribit: TapCfg::off(),
        hl: TapCfg::off(),
    };

    let spec = match raw_tap.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return Ok(cfg),
    };

    if spec == "all" {
        cfg.pm = enabled_cfg;
        cfg.bn = enabled_cfg;
        cfg.okx = enabled_cfg;
        cfg.rpc = enabled_cfg;
        cfg.deribit = enabled_cfg;
        cfg.hl = enabled_cfg;
        return Ok(cfg);
    }

    let mut seen: [&str; 6] = [""; 6];
    let mut n_seen = 0usize;
    for item in spec.split(',') {
        let label = item.trim();
        if label.is_empty() {
            return Err("--raw-tap: empty venue label");
        }
        if seen[..n_seen].contains(&label) {
            return Err("--raw-tap: duplicate venue label");
        }
        if n_seen >= seen.len() {
            return Err("--raw-tap: more venue labels than known venues");
        }
        seen[n_seen] = label;
        n_seen += 1;
        match label {
            "pm" => cfg.pm = enabled_cfg,
            "bn" => cfg.bn = enabled_cfg,
            "okx" => cfg.okx = enabled_cfg,
            "rpc" => cfg.rpc = enabled_cfg,
            "deribit" => cfg.deribit = enabled_cfg,
            "hl" => cfg.hl = enabled_cfg,
            _ => return Err("--raw-tap: unknown venue label"),
        }
    }
    Ok(cfg)
}

// ---------------------------------------------------------------
// Drain-and-count consumer (main thread)
// ---------------------------------------------------------------

/// Per-ring observed counters reset every 5 s.
#[derive(Default, Debug, Clone, Copy)]
pub struct DrainCounters {
    /// Polymarket ticks observed (lane 0).
    pub polymarket_ticks: u64,
    /// Binance ticks observed (lane 1).
    pub binance_ticks: u64,
    /// Ticks observed on the OKX/Deribit/Hyperliquid lanes (2..5).
    /// Zero until those ingresses exist (Phases 8b–8d).
    pub other_venue_ticks: u64,
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
        self.other_venue_ticks += other.other_venue_ticks;
        self.rpc_signals += other.rpc_signals;
        self.rss_signals += other.rss_signals;
    }
}

/// Consumer-side handles passed to the drain loop / engine. Created
/// from `Ring::split()`; the producer ends went to ingress threads.
pub struct Consumers {
    /// Tick-lane consumers, indexed by `VenueId as usize` (§3.3).
    pub tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
    /// RPC signal consumer.
    pub rpc_signal: Consumer<Signal, SIGNAL_RING_SIZE>,
    /// RSS signal consumer (cli-level drain; Stage-2 retirement).
    pub rss_signal: Consumer<Signal, 1024>,
    /// Fill-lane consumers (`engine::fill_lane_of` order). Producers
    /// arrive with the venue dispatchers in Phase 8j; paper-mode
    /// fills flow through the engine's dispatcher pump (D3).
    pub fill_lanes: [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
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
        // Drain each lane in fixed-size batches; bounded so one ring
        // can't monopolise the main thread. Lane order = VenueId.
        let mut lane = 0;
        while lane < NUM_TICK_LANES {
            for _ in 0..DRAIN_BATCH {
                if cons.tick_lanes[lane].try_pop().is_some() {
                    match lane {
                        0 => period.polymarket_ticks += 1,
                        1 => period.binance_ticks += 1,
                        _ => period.other_venue_ticks += 1,
                    }
                } else {
                    break;
                }
            }
            lane += 1;
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
                other_ticks = period.other_venue_ticks,
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
            let ingress_okx_state = reg
                .register_gauge("engine_ingress_okx_state")
                .map_err(|_| "register engine_ingress_okx_state")?;
            let ingress_deribit_state = reg
                .register_gauge("engine_ingress_deribit_state")
                .map_err(|_| "register engine_ingress_deribit_state")?;
            let ingress_hyperliquid_state = reg
                .register_gauge("engine_ingress_hyperliquid_state")
                .map_err(|_| "register engine_ingress_hyperliquid_state")?;
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
            // §6.4 loss-accounting counters, one set per WSS
            // ingress (D4). Boot-only; format! is fine here.
            let ingress_polymarket = register_ingress_counters(&mut reg, "polymarket")?;
            let ingress_binance = register_ingress_counters(&mut reg, "binance")?;
            let ingress_okx = register_ingress_counters(&mut reg, "okx")?;
            let ingress_deribit = register_ingress_counters(&mut reg, "deribit")?;
            let ingress_hyperliquid = register_ingress_counters(&mut reg, "hyperliquid")?;
            let ingress_rpc = register_ingress_counters(&mut reg, "rpc")?;

            // §6.5 capture-health gauges, one pair per spawnable
            // ingress thread (short capture-venue labels — see
            // `register_capture_gauges` docs). Registered unconditionally
            // (matches the `register_ingress_counters` convention above)
            // so the registry surface is stable regardless of which
            // optional venues get spawned; unspawned venues simply never
            // get their gauges set past the zero default.
            let capture_pm = register_capture_gauges(&mut reg, "pm")?;
            let capture_bn = register_capture_gauges(&mut reg, "bn")?;
            let capture_okx = register_capture_gauges(&mut reg, "okx")?;
            let capture_deribit = register_capture_gauges(&mut reg, "deribit")?;
            let capture_hyperliquid = register_capture_gauges(&mut reg, "hl")?;
            let capture_rpc = register_capture_gauges(&mut reg, "rpc")?;

            // §6.1 boot-discovery coverage gauges — PM/OKX/Deribit/HL
            // only (BN + RPC have no REST discovery, boot_discovery
            // module docs).
            let coverage_pm = register_coverage_gauge(&mut reg, "pm")?;
            let coverage_okx = register_coverage_gauge(&mut reg, "okx")?;
            let coverage_deribit = register_coverage_gauge(&mut reg, "deribit")?;
            let coverage_hyperliquid = register_coverage_gauge(&mut reg, "hl")?;

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
                ingress_okx_state,
                ingress_deribit_state,
                ingress_hyperliquid_state,
                ingress_rpc_state,
                ingress_rss_state,
                max_tick_age_ns,
                tick_age_ns_per_bucket,
                ingress_polymarket,
                ingress_binance,
                ingress_okx,
                ingress_deribit,
                ingress_hyperliquid,
                ingress_rpc,
                capture_pm,
                capture_bn,
                capture_okx,
                capture_deribit,
                capture_hyperliquid,
                capture_rpc,
                coverage_pm,
                coverage_okx,
                coverage_deribit,
                coverage_hyperliquid,
            });
        }
        if enable_tui {
            out.snapshot = Some(Arc::new(tui::SnapshotCell::new()));
        }
        Ok(out)
    }
}

/// Register the seven §6.4 counters for one ingress. Boot-only.
fn register_ingress_counters(
    reg: &mut core_metrics::MetricsRegistry,
    venue: &str,
) -> Result<IngressCounterIds, &'static str> {
    let mut one = |metric: &str| -> Result<core_metrics::CounterId, &'static str> {
        let name = format!("engine_ingress_{venue}_{metric}_total");
        reg.register_counter(&name)
            .map_err(|_| "register ingress counter")
    };
    Ok(IngressCounterIds {
        msgs: one("msgs")?,
        bytes: one("bytes")?,
        parse_errors: one("parse_errors")?,
        gaps: one("gaps")?,
        resubscribes: one("resubscribes")?,
        reconnects: one("reconnects")?,
        ring_drops: one("ring_drops")?,
    })
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
    /// Per-ingress status slots (D7). `None` only in tests that
    /// exercise the loop without spawned ingresses.
    pub ingress: Option<Arc<IngressStatusSet>>,
}

impl Observability {
    /// Attach a [`LatencyDump`] config. Boot-only; called from the
    /// cli after [`Observability::build`]. Returns `self` so it can
    /// be chained.
    pub fn with_latency_dump(mut self, dump: Option<LatencyDump>) -> Self {
        self.latency_dump = dump;
        self
    }

    /// Attach the per-ingress status slots (reader side). Boot-only.
    pub fn with_ingress_statuses(mut self, set: Arc<IngressStatusSet>) -> Self {
        self.ingress = Some(set);
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
    /// Per-ingress state gauge: OKX v5 public WS.
    pub ingress_okx_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Deribit JSON-RPC WS.
    pub ingress_deribit_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Hyperliquid public WS.
    pub ingress_hyperliquid_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Polygon JSON-RPC.
    pub ingress_rpc_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: RSS poller.
    pub ingress_rss_state: core_metrics::GaugeId,
    /// Maximum tick age across every observed symbol (ns).
    /// Spikes here surface a silenced market.
    pub max_tick_age_ns: core_metrics::GaugeId,
    /// Per-bucket tick-age gauges. Bucket index =
    /// `symbol_bucket_mix(sym) & (SYM_BUCKETS-1)` (§3.1).
    /// Operators can pinpoint which exact bucket went silent
    /// instead of only seeing the across-buckets max.
    pub tick_age_ns_per_bucket: [core_metrics::GaugeId; engine::SYM_BUCKETS],
    /// §6.4 loss-accounting counters, Polymarket thread.
    pub ingress_polymarket: IngressCounterIds,
    /// §6.4 loss-accounting counters, Binance thread.
    pub ingress_binance: IngressCounterIds,
    /// §6.4 loss-accounting counters, OKX thread.
    pub ingress_okx: IngressCounterIds,
    /// §6.4 loss-accounting counters, Deribit thread.
    pub ingress_deribit: IngressCounterIds,
    /// §6.4 loss-accounting counters, Hyperliquid thread.
    pub ingress_hyperliquid: IngressCounterIds,
    /// §6.4 loss-accounting counters, RPC thread.
    pub ingress_rpc: IngressCounterIds,
    /// §6.5 capture-health gauges, Polymarket thread.
    pub capture_pm: CaptureGaugeIds,
    /// §6.5 capture-health gauges, Binance thread.
    pub capture_bn: CaptureGaugeIds,
    /// §6.5 capture-health gauges, OKX thread.
    pub capture_okx: CaptureGaugeIds,
    /// §6.5 capture-health gauges, Deribit thread.
    pub capture_deribit: CaptureGaugeIds,
    /// §6.5 capture-health gauges, Hyperliquid thread.
    pub capture_hyperliquid: CaptureGaugeIds,
    /// §6.5 capture-health gauges, RPC thread.
    pub capture_rpc: CaptureGaugeIds,
    /// §6.1 boot-discovery coverage gauge, Polymarket (always runs).
    pub coverage_pm: GaugeId,
    /// §6.1 boot-discovery coverage gauge, OKX (0 when unconfigured).
    pub coverage_okx: GaugeId,
    /// §6.1 boot-discovery coverage gauge, Deribit (0 when
    /// unconfigured).
    pub coverage_deribit: GaugeId,
    /// §6.1 boot-discovery coverage gauge, Hyperliquid (0 when
    /// unconfigured).
    pub coverage_hyperliquid: GaugeId,
}

/// Registry counter handles for one ingress thread's §6.4 loss
/// accounting (D4: `ring_drops` is the headline).
#[derive(Copy, Clone, Debug)]
pub struct IngressCounterIds {
    /// Parsed messages.
    pub msgs: core_metrics::CounterId,
    /// Payload bytes received.
    pub bytes: core_metrics::CounterId,
    /// Parser rejections.
    pub parse_errors: core_metrics::CounterId,
    /// Sequence gaps.
    pub gaps: core_metrics::CounterId,
    /// Integrity-driven resubscribes.
    pub resubscribes: core_metrics::CounterId,
    /// Transport reconnects.
    pub reconnects: core_metrics::CounterId,
    /// Ring `try_push` failures (D4).
    pub ring_drops: core_metrics::CounterId,
}

// ---------------------------------------------------------------
// §6.5 capture metrics — set from inside the spawn wrapper thread
// ---------------------------------------------------------------
//
// `PmlrCapture` is moved into each ingress thread's closure (Part B)
// and never shared — unlike `IngressStatus`, there is no cross-thread
// handle the central engine loop could read from to mirror these
// gauges centrally (see `mirror_ingress_counters`). Each spawn
// wrapper therefore mirrors its own capture health directly via the
// registry handle it's handed at spawn time.

/// Registry gauge handles for one ingress thread's §6.5 capture
/// health: `PmlrCapture::io_errors()` and the summed record count
/// (`ticks_written + events_written + signals_written + tap_records`).
#[derive(Copy, Clone, Debug)]
pub struct CaptureGaugeIds {
    /// Mirrors `PmlrCapture::io_errors()`. Nonzero ⇒ capture
    /// sticky-disabled itself (module docs, core-io) — should be
    /// treated as a soak-verdict red flag even though the market-data
    /// session itself is unaffected.
    pub io_errors: GaugeId,
    /// Mirrors `ticks_written() + events_written() + signals_written()
    /// + tap_records()` — total records staged since the capture was
    /// opened (monotonic snapshot, not a delta).
    pub records: GaugeId,
}

/// Registry handle + gauge ids for one ingress thread's §6.5 capture
/// metrics. `None` when `--metrics` (and `--tui`, which implies it)
/// are both off, in which case the spawn wrapper skips the gauge
/// writes entirely.
pub type CaptureMetrics = Option<(Arc<MetricsRegistry>, CaptureGaugeIds)>;

/// Mirror one ingress thread's §6.5 capture health into its two
/// registry gauges. Called from inside the spawn wrapper thread
/// itself, right after every `run(...)` return and once more before
/// the thread exits — see [`CaptureMetrics`] docs for why this can't
/// be done centrally.
fn mirror_capture_metrics(metrics: &CaptureMetrics, capture: &PmlrCapture) {
    if let Some((reg, ids)) = metrics.as_ref() {
        reg.gauge(ids.io_errors).set(capture.io_errors() as i64);
        let records = capture.ticks_written()
            + capture.events_written()
            + capture.signals_written()
            + capture.tap_records();
        reg.gauge(ids.records).set(records as i64);
    }
}

/// Register the [`CaptureGaugeIds`] pair for one venue. Boot-only.
/// Uses the short capture venue label (`pm`/`bn`/`okx`/`rpc`/
/// `deribit`/`hl` — [`PmlrCapture::open`]'s `venue_label`) rather than
/// the long form `register_ingress_counters` uses, so a gauge name
/// and its capture files always agree on the venue string.
fn register_capture_gauges(
    reg: &mut MetricsRegistry,
    venue_label: &str,
) -> Result<CaptureGaugeIds, &'static str> {
    let io_errors = reg
        .register_gauge(&format!("engine_ingress_{venue_label}_capture_io_errors"))
        .map_err(|_| "register capture io_errors gauge")?;
    let records = reg
        .register_gauge(&format!("engine_ingress_{venue_label}_capture_records"))
        .map_err(|_| "register capture records gauge")?;
    Ok(CaptureGaugeIds { io_errors, records })
}

/// Register the `engine_ingress_<venue>_coverage_configured` gauge
/// for one Phase-8e boot-discovery venue (`pm`/`okx`/`deribit`/`hl` —
/// BN and RPC have no REST discovery, see `boot_discovery` module
/// docs). Boot-only.
fn register_coverage_gauge(
    reg: &mut MetricsRegistry,
    venue_label: &str,
) -> Result<GaugeId, &'static str> {
    reg.register_gauge(&format!("engine_ingress_{venue_label}_coverage_configured"))
        .map_err(|_| "register coverage_configured gauge")
}

/// Last-mirrored cumulative values for one ingress — the registry
/// wants monotonic increments, the status slot exposes cumulative
/// totals; the delta lives here.
#[derive(Copy, Clone, Debug, Default)]
struct IngressCountersSnapshot {
    msgs: u64,
    bytes: u64,
    parse_errors: u64,
    gaps: u64,
    resubscribes: u64,
    reconnects: u64,
    ring_drops: u64,
}

/// Mirror one ingress status slot into its registry counters as
/// deltas since the previous publish tick. 5 s cadence — cold path.
fn mirror_ingress_counters(
    reg: &core_metrics::MetricsRegistry,
    ids: &IngressCounterIds,
    st: &IngressStatus,
    last: &mut IngressCountersSnapshot,
) {
    let cur = IngressCountersSnapshot {
        msgs: st.msgs_total(),
        bytes: st.bytes_total(),
        parse_errors: st.parse_errors_total(),
        gaps: st.gaps_total(),
        resubscribes: st.resubscribes_total(),
        reconnects: st.reconnects_total(),
        ring_drops: st.ring_drops_total(),
    };
    reg.counter(ids.msgs).inc(cur.msgs.saturating_sub(last.msgs));
    reg.counter(ids.bytes).inc(cur.bytes.saturating_sub(last.bytes));
    reg.counter(ids.parse_errors)
        .inc(cur.parse_errors.saturating_sub(last.parse_errors));
    reg.counter(ids.gaps).inc(cur.gaps.saturating_sub(last.gaps));
    reg.counter(ids.resubscribes)
        .inc(cur.resubscribes.saturating_sub(last.resubscribes));
    reg.counter(ids.reconnects)
        .inc(cur.reconnects.saturating_sub(last.reconnects));
    reg.counter(ids.ring_drops)
        .inc(cur.ring_drops.saturating_sub(last.ring_drops));
    *last = cur;
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
    // Phase 8a lane engine: five tick lanes + one signal lane + four
    // fill lanes. The signal lane is bound to the RPC ring — the D2
    // disposition for Stage 1 (per §3.3; RSS retires in Stage 2 §8).
    // Fills flow from the dispatcher pump (D3) until 8j wires the
    // per-venue fill-lane producers.
    let Consumers {
        tick_lanes,
        rpc_signal,
        mut rss_signal,
        fill_lanes,
    } = cons;
    let mut eng = Engine::new(strat, disp, tick_lanes, rpc_signal, fill_lanes);
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
    // Last-mirrored snapshots for the §6.4 ingress counters
    // (pm, bn, okx, rpc, deribit, hyperliquid) so registry counters
    // get monotonic deltas. Append-only: existing indices are
    // load-bearing, new venues go at the end.
    let mut ingress_last = [IngressCountersSnapshot::default(); 6];
    // Periodic HdrHistogram dump cadence. `next_dump_ns` is only
    // consulted when `obs.latency_dump.is_some()`.
    let mut next_dump_ns: u64 = match obs.latency_dump.as_ref() {
        Some(d) => now_ns().saturating_add(d.interval_ns),
        None => u64::MAX,
    };

    while !shutdown_requested() {
        eng.tick(DRAIN_BATCH);

        // RSS drain — cli-level only (Stage-2 retirement, §8.1).
        for _ in 0..DRAIN_BATCH {
            if rss_signal.try_pop().is_some() {
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

                // Per-ingress connection state — real per-thread
                // status slots (D7 fix). Gauge value = IngressState:
                // 0=Down, 1=Connecting, 2=Up, 3=Backoff.
                if let Some(ing) = obs.ingress.as_ref() {
                    reg.gauge(ids.ingress_polymarket_state)
                        .set(ing.polymarket.state() as i64);
                    reg.gauge(ids.ingress_binance_state)
                        .set(ing.binance.state() as i64);
                    reg.gauge(ids.ingress_okx_state).set(ing.okx.state() as i64);
                    reg.gauge(ids.ingress_deribit_state)
                        .set(ing.deribit.state() as i64);
                    reg.gauge(ids.ingress_hyperliquid_state)
                        .set(ing.hyperliquid.state() as i64);
                    reg.gauge(ids.ingress_rpc_state).set(ing.rpc.state() as i64);
                    reg.gauge(ids.ingress_rss_state).set(ing.rss.state() as i64);
                    // §6.4 loss accounting: mirror the per-thread
                    // cumulative counters into the registry as
                    // monotonic deltas (D4: ring_drops included).
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_polymarket,
                        &ing.polymarket,
                        &mut ingress_last[0],
                    );
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_binance,
                        &ing.binance,
                        &mut ingress_last[1],
                    );
                    mirror_ingress_counters(reg, &ids.ingress_okx, &ing.okx, &mut ingress_last[2]);
                    mirror_ingress_counters(reg, &ids.ingress_rpc, &ing.rpc, &mut ingress_last[3]);
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_deribit,
                        &ing.deribit,
                        &mut ingress_last[4],
                    );
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_hyperliquid,
                        &ing.hyperliquid,
                        &mut ingress_last[5],
                    );
                }

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
                // Ingest health from the real status slots (D7):
                // bit0 = polymarket, bit1 = binance, bit2 = rpc,
                // bit3 = rss, bit4 = okx, bit5 = deribit, bit6 = hl
                // (8e — appended; existing bits never renumber); bit
                // set iff the thread is Up.
                state.ingest_health = match obs.ingress.as_ref() {
                    Some(ing) => {
                        (u8::from(ing.polymarket.state() == IngressState::Up))
                            | (u8::from(ing.binance.state() == IngressState::Up) << 1)
                            | (u8::from(ing.rpc.state() == IngressState::Up) << 2)
                            | (u8::from(ing.rss.state() == IngressState::Up) << 3)
                            | (u8::from(ing.okx.state() == IngressState::Up) << 4)
                            | (u8::from(ing.deribit.state() == IngressState::Up) << 5)
                            | (u8::from(ing.hyperliquid.state() == IngressState::Up) << 6)
                    }
                    None => 0,
                };
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

/// Sleep for the next capped-exponential delay (D8). The schedule
/// lives in the caller's per-thread [`Backoff`]; a healthy session
/// resets it (see the spawn loops).
fn sleep_backoff(b: &mut Backoff) {
    let delay = Duration::from_nanos(b.next_delay_ns());
    tracing::debug!(?delay, attempt = b.attempt(), "reconnect backoff");
    thread::sleep(delay);
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
// Phase-8e boot REST discovery (plan §6.1)
// ---------------------------------------------------------------

/// Boot-only venue REST discovery: validates every `--okx-symbols` /
/// `--deribit-symbols` / `--hl-coins` / `--polymarket-asset-id` entry
/// against the venue's live instrument universe *before* any ingress
/// thread spawns, and (OKX only) builds the discovery-gated
/// [`ingress_okx::OkxSymbolTable`] `build_okx_symbol_table` now
/// requires.
///
/// BN + RPC deliberately have no discovery here: Binance discovery is
/// out of Phase-8 scope (plan §6.1), and Polygon RPC has no
/// instrument universe to validate against (it streams block headers,
/// not a tradable-instrument list).
///
/// Network calls (`run_all` and its per-venue helpers) are not unit
/// tested — they need a live socket. The MISSING-detection decision
/// logic each of them drives (`okx_missing_reason` /
/// `deribit_missing_reason` / `hl_missing_reason` / `pm_missing_reason`)
/// is pure and fully covered by `mod tests` below using tiny inline
/// fixtures fed through the same `ingest_*` parsers the network path
/// uses.
pub mod boot_discovery {
    use std::ops::Range;
    use std::sync::Arc;
    use std::time::Duration;

    use core_config::Config;
    use ingress_deribit::discovery::DeribitDiscovery;
    use ingress_hyperliquid::discovery::HlDiscovery;
    use ingress_okx::discovery::OkxDiscovery;
    use ingress_polymarket::discovery::PmDiscovery;

    use super::split_host_port;

    /// UA string for every boot-discovery fetch.
    const USER_AGENT: &[u8] = b"multivenue-engine/8e";
    /// Shared body cap — OKX's SPOT page alone is ~1.45 MB live.
    const MAX_BODY: usize = 8 * 1024 * 1024;
    /// Per-fetch deadline (connect + TLS + request + full response).
    const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

    /// Why a configured symbol failed venue validation. Always a
    /// short machine-grep-able token, logged as the `reason` field.
    pub type MissingReason = &'static str;

    /// Configured / matched / venue-universe counts for one venue's
    /// coverage line + `..._coverage_configured` gauge.
    #[derive(Copy, Clone, Debug, Default)]
    pub struct VenueCoverage {
        /// Symbols the operator configured for this venue.
        pub configured: u32,
        /// Of those, how many resolved live on the venue.
        pub matched: u32,
        /// Venue universe size (`universe_live()` for okx/deribit,
        /// `universe_total()` for hl/pm — see each venue's bullet in
        /// the phase-8e plan §6.1).
        pub universe: u32,
    }

    /// Everything [`run_all`] produces, consumed by the cli's `run()`
    /// before any ingress thread spawns.
    pub struct Outcome {
        /// True if ANY configured symbol, across every venue this
        /// pass touched, failed to validate. The caller's fail-fast
        /// decision (fatal in `--live`, a warning in `--paper`) is a
        /// single global check on this flag — not per-venue.
        pub any_missing: bool,
        /// Polymarket coverage (discovery always runs — the asset id
        /// is a required flag).
        pub pm: VenueCoverage,
        /// OKX coverage; `None` when `--okx-symbols` is unset.
        pub okx: Option<VenueCoverage>,
        /// The discovery-gated OKX symbol table, built here because
        /// `build_okx_symbol_table` needs the discovered `instType`
        /// per instrument. `Some` iff `okx` is `Some`.
        pub okx_table: Option<ingress_okx::OkxSymbolTable>,
        /// Deribit coverage; `None` when `--deribit-symbols` is unset.
        /// Deribit's symbol table is still built by
        /// [`super::build_deribit_symbol_table`] exactly as before —
        /// unlike OKX it doesn't need discovery data to construct.
        pub deribit: Option<VenueCoverage>,
        /// Hyperliquid coverage; `None` when `--hl-coins` is unset.
        /// Hyperliquid's coin table is still built by
        /// [`super::build_hl_coin_table`] exactly as before.
        pub hl: Option<VenueCoverage>,
    }

    // -----------------------------------------------------------
    // Pure decision logic — unit tested below, no network involved.
    // -----------------------------------------------------------

    /// `None` ⇒ `inst_id` is live on OKX. `Some(reason)` ⇒ MISSING.
    pub fn okx_missing_reason(d: &OkxDiscovery, inst_id: &[u8]) -> Option<MissingReason> {
        match d.find(inst_id) {
            None => Some("not_found"),
            Some(row) if !row.live => Some("not_live"),
            Some(_) => None,
        }
    }

    /// `None` ⇒ `instrument` is live on Deribit. `Some(reason)` ⇒
    /// MISSING.
    pub fn deribit_missing_reason(
        d: &DeribitDiscovery,
        instrument: &[u8],
    ) -> Option<MissingReason> {
        match d.find(instrument) {
            None => Some("not_found"),
            Some(row) if !row.live => Some("not_live"),
            Some(_) => None,
        }
    }

    /// `None` ⇒ `coin` resolves on Hyperliquid. `Some(reason)` ⇒
    /// MISSING. Hyperliquid's `resolve` has no separate liveness flag
    /// (module docs) — found is live.
    pub fn hl_missing_reason(d: &HlDiscovery, coin: &[u8]) -> Option<MissingReason> {
        match d.resolve(coin) {
            None => Some("not_found"),
            Some(_) => None,
        }
    }

    /// `None` ⇒ `token` (the CLOB asset id) is a tradable market on
    /// Polymarket. `Some(reason)` ⇒ MISSING, naming which flag failed
    /// (plan §6.1: "log which flag failed").
    pub fn pm_missing_reason(d: &PmDiscovery, token: &[u8]) -> Option<MissingReason> {
        match d.find_by_token(token) {
            None => Some("not_found"),
            Some(row) if row.closed => Some("closed"),
            Some(row) if !row.active => Some("not_active"),
            Some(row) if !row.accepting_orders => Some("not_accepting_orders"),
            Some(row) if !row.enable_order_book => Some("no_order_book"),
            Some(_) => None,
        }
    }

    // -----------------------------------------------------------
    // Network fetch helpers
    // -----------------------------------------------------------

    fn get(
        tls: &Arc<rustls::ClientConfig>,
        host: &str,
        port: u16,
        path: &str,
        buf: &mut Vec<u8>,
    ) -> Result<Range<usize>, core_net::boot_http::BootHttpErr> {
        core_net::boot_http::https_get(tls, host, port, path, USER_AGENT, buf, MAX_BODY, FETCH_TIMEOUT)
    }

    fn post(
        tls: &Arc<rustls::ClientConfig>,
        host: &str,
        port: u16,
        path: &str,
        body: &[u8],
        buf: &mut Vec<u8>,
    ) -> Result<Range<usize>, core_net::boot_http::BootHttpErr> {
        core_net::boot_http::https_post(
            tls,
            host,
            port,
            path,
            USER_AGENT,
            b"application/json",
            body,
            buf,
            MAX_BODY,
            FETCH_TIMEOUT,
        )
    }

    // -----------------------------------------------------------
    // Per-venue orchestration — network + logging; not unit tested.
    // -----------------------------------------------------------

    fn run_pm(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        asset_id: &str,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.polymarket_gamma_host, 443)?;
        let path = format!("/markets?clob_token_ids={asset_id}");
        let range = get(tls, host, port, &path, buf).map_err(|e| {
            tracing::error!(venue = "pm", error = ?e, "discovery: fetch failed");
            "pm: discovery fetch failed"
        })?;
        let mut d = PmDiscovery::new();
        d.ingest_body(&buf[range]).map_err(|e| {
            tracing::error!(venue = "pm", error = ?e, "discovery: parse failed");
            "pm: discovery parse failed"
        })?;

        let mut matched = 0u32;
        match pm_missing_reason(&d, asset_id.as_bytes()) {
            None => {
                matched = 1;
                // pm_missing_reason == None guarantees find_by_token hits.
                if let Some(row) = d.find_by_token(asset_id.as_bytes()) {
                    let sibling = d
                        .sibling_of(asset_id.as_bytes())
                        .map(|s| String::from_utf8_lossy(s).into_owned());
                    tracing::info!(
                        venue = "pm",
                        asset_id,
                        sibling,
                        neg_risk = row.neg_risk,
                        tick_1e9 = row.order_price_min_tick_1e9,
                        min_size_1e6 = row.order_min_size_1e6,
                        "discovery: pm market resolved"
                    );
                }
            }
            Some(reason) => {
                *any_missing = true;
                tracing::error!(
                    venue = "pm",
                    symbol = asset_id,
                    reason,
                    "discovery: configured symbol missing from venue universe"
                );
            }
        }
        let universe = d.universe_total();
        tracing::info!(venue = "pm", configured = 1, matched, universe, "discovery: coverage");
        Ok(VenueCoverage { configured: 1, matched, universe })
    }

    fn run_okx(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spec: &str,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<(VenueCoverage, ingress_okx::OkxSymbolTable), &'static str> {
        let (host, port) = split_host_port(&cfg.okx_rest_host, 443)?;
        let mut d = OkxDiscovery::new();
        for (i, page) in ["SPOT", "SWAP", "FUTURES"].iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let path = format!("/api/v5/public/instruments?instType={page}");
            let range = get(tls, host, port, &path, buf).map_err(|e| {
                tracing::error!(venue = "okx", page = %page, error = ?e, "discovery: fetch failed");
                "okx: discovery fetch failed"
            })?;
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "okx", page = %page, error = ?e, "discovery: parse failed");
                "okx: discovery parse failed"
            })?;
        }

        let configured: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let mut matched = 0u32;
        for inst in &configured {
            match okx_missing_reason(&d, inst.as_bytes()) {
                None => matched += 1,
                Some(reason) => {
                    *any_missing = true;
                    tracing::error!(
                        venue = "okx",
                        symbol = *inst,
                        reason,
                        "discovery: configured symbol missing from venue universe"
                    );
                }
            }
        }
        let universe = d.universe_live();
        tracing::info!(venue = "okx", configured = configured.len(), matched, universe, "discovery: coverage");

        let table = super::build_okx_symbol_table(spec, &d)?;
        Ok((VenueCoverage { configured: configured.len() as u32, matched, universe }, table))
    }

    fn run_deribit(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spec: &str,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.deribit_rest_host, 443)?;
        let mut d = DeribitDiscovery::new();
        for (i, ccy) in ["BTC", "ETH", "USDC"].iter().enumerate() {
            if i > 0 {
                // Venue rate-limits public/get_instruments to 1 req/s.
                std::thread::sleep(Duration::from_millis(1050));
            }
            let path = format!("/api/v2/public/get_instruments?currency={ccy}&kind=future");
            let range = get(tls, host, port, &path, buf).map_err(|e| {
                tracing::error!(venue = "deribit", currency = %ccy, error = ?e, "discovery: fetch failed");
                "deribit: discovery fetch failed"
            })?;
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "deribit", currency = %ccy, error = ?e, "discovery: parse failed");
                "deribit: discovery parse failed"
            })?;
        }

        let configured: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let mut matched = 0u32;
        for instr in &configured {
            match deribit_missing_reason(&d, instr.as_bytes()) {
                None => matched += 1,
                Some(reason) => {
                    *any_missing = true;
                    tracing::error!(
                        venue = "deribit",
                        symbol = *instr,
                        reason,
                        "discovery: configured symbol missing from venue universe"
                    );
                }
            }
        }
        let universe = d.universe_live();
        tracing::info!(venue = "deribit", configured = configured.len(), matched, universe, "discovery: coverage");
        Ok(VenueCoverage { configured: configured.len() as u32, matched, universe })
    }

    fn run_hl(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spec: &str,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.hyperliquid_api_host, 443)?;
        let mut d = HlDiscovery::new();

        let requests: [(&str, &[u8]); 4] = [
            ("meta", br#"{"type":"meta"}"#),
            ("spotMeta", br#"{"type":"spotMeta"}"#),
            ("perpDexs", br#"{"type":"perpDexs"}"#),
            ("outcomeMeta", br#"{"type":"outcomeMeta"}"#),
        ];
        for (i, (label, body)) in requests.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            let range = post(tls, host, port, "/info", body, buf).map_err(|e| {
                tracing::error!(venue = "hl", request = %label, error = ?e, "discovery: fetch failed");
                "hl: discovery fetch failed"
            })?;
            let parsed = match *label {
                "meta" => d.ingest_meta(&buf[range]),
                "spotMeta" => d.ingest_spot_meta(&buf[range]),
                "perpDexs" => d.ingest_perp_dexs(&buf[range]),
                "outcomeMeta" => d.ingest_outcome_meta(&buf[range]),
                _ => unreachable!("requests array is a fixed literal"),
            };
            parsed.map_err(|e| {
                tracing::error!(venue = "hl", request = %label, error = ?e, "discovery: parse failed");
                "hl: discovery parse failed"
            })?;
        }

        let configured: Vec<&str> = spec.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let mut matched = 0u32;
        for coin in &configured {
            match d.resolve(coin.as_bytes()) {
                Some(info) => {
                    matched += 1;
                    tracing::debug!(
                        venue = "hl",
                        coin = *coin,
                        asset_id = info.asset_id,
                        kind = ?info.kind,
                        "discovery: hl asset resolved"
                    );
                }
                None => {
                    *any_missing = true;
                    tracing::error!(
                        venue = "hl",
                        symbol = *coin,
                        reason = "not_found",
                        "discovery: configured symbol missing from venue universe"
                    );
                }
            }
        }
        let universe = d.universe_total();
        tracing::info!(venue = "hl", configured = configured.len(), matched, universe, "discovery: coverage");
        Ok(VenueCoverage { configured: configured.len() as u32, matched, universe })
    }

    /// Run the full Phase-8e boot discovery pass: OKX (if
    /// `okx_spec` is configured), Deribit (if `deribit_spec` is
    /// configured), Hyperliquid (if `hl_spec` is configured), then
    /// Polymarket (always). One reused `Vec<u8>` buffer carries every
    /// fetch's response body. Any fetch/parse failure is FATAL —
    /// returned as `Err` for the caller to log + exit non-zero; a
    /// MISSING symbol is not itself an `Err` here (see
    /// [`Outcome::any_missing`] — the caller decides paper-vs-live).
    pub fn run_all(
        cfg: &Config,
        tls_config: &Arc<rustls::ClientConfig>,
        okx_spec: Option<&str>,
        deribit_spec: Option<&str>,
        hl_spec: Option<&str>,
        polymarket_asset_id: &str,
    ) -> Result<Outcome, &'static str> {
        let mut buf: Vec<u8> = Vec::new();
        let mut any_missing = false;

        let (okx, okx_table) = match okx_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => {
                let (cov, table) = run_okx(cfg, tls_config, spec, &mut buf, &mut any_missing)?;
                (Some(cov), Some(table))
            }
            None => (None, None),
        };

        let deribit = match deribit_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => Some(run_deribit(cfg, tls_config, spec, &mut buf, &mut any_missing)?),
            None => None,
        };

        let hl = match hl_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => Some(run_hl(cfg, tls_config, spec, &mut buf, &mut any_missing)?),
            None => None,
        };

        let pm = run_pm(cfg, tls_config, polymarket_asset_id, &mut buf, &mut any_missing)?;

        Ok(Outcome { any_missing, pm, okx, okx_table, deribit, hl })
    }

    // -----------------------------------------------------------
    // Tests — pure decision logic only, no network (see module docs).
    // -----------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        fn okx_fixture() -> OkxDiscovery {
            let mut d = OkxDiscovery::new();
            d.ingest_body(
                br#"{"code":"0","data":[
                  {"instId":"BTC-USDT","instType":"SPOT","state":"live","tickSz":"0.1","lotSz":"0.01","ctVal":""},
                  {"instId":"DEAD-USDT","instType":"SPOT","state":"suspend","tickSz":"0.1","lotSz":"0.01","ctVal":""}
                ],"msg":""}"#,
            )
            .unwrap();
            d
        }

        #[test]
        fn okx_missing_reason_covers_found_not_live_and_absent() {
            let d = okx_fixture();
            assert_eq!(okx_missing_reason(&d, b"BTC-USDT"), None);
            assert_eq!(okx_missing_reason(&d, b"DEAD-USDT"), Some("not_live"));
            assert_eq!(okx_missing_reason(&d, b"NOPE-USDT"), Some("not_found"));
        }

        fn deribit_fixture() -> DeribitDiscovery {
            let mut d = DeribitDiscovery::new();
            d.ingest_body(
                br#"{"jsonrpc":"2.0","result":[
                  {"instrument_name":"BTC-PERPETUAL","kind":"future","is_active":true,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0},
                  {"instrument_name":"DEAD-PERPETUAL","kind":"future","is_active":false,"state":"open","settlement_period":"perpetual","tick_size":0.5,"contract_size":10.0,"min_trade_amount":10.0}
                ],"usIn":1,"usOut":2,"usDiff":1,"testnet":false}"#,
            )
            .unwrap();
            d
        }

        #[test]
        fn deribit_missing_reason_covers_found_not_live_and_absent() {
            let d = deribit_fixture();
            assert_eq!(deribit_missing_reason(&d, b"BTC-PERPETUAL"), None);
            assert_eq!(
                deribit_missing_reason(&d, b"DEAD-PERPETUAL"),
                Some("not_live")
            );
            assert_eq!(
                deribit_missing_reason(&d, b"NOPE-PERPETUAL"),
                Some("not_found")
            );
        }

        fn hl_fixture() -> HlDiscovery {
            let mut d = HlDiscovery::new();
            d.ingest_meta(br#"{"universe":[{"name":"BTC","szDecimals":5}]}"#)
                .unwrap();
            d
        }

        #[test]
        fn hl_missing_reason_covers_found_and_absent() {
            let d = hl_fixture();
            assert_eq!(hl_missing_reason(&d, b"BTC"), None);
            assert_eq!(hl_missing_reason(&d, b"NOPE"), Some("not_found"));
        }

        fn pm_fixture() -> PmDiscovery {
            let mut d = PmDiscovery::new();
            d.ingest_body(
                br#"[{"clobTokenIds":"[\"11111111112222222222\"]","conditionId":"0xab","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":true},
                     {"clobTokenIds":"[\"33333333334444444444\"]","conditionId":"0xcd","active":false,"closed":false,"acceptingOrders":true,"enableOrderBook":true},
                     {"clobTokenIds":"[\"55555555556666666666\"]","conditionId":"0xef","active":true,"closed":true,"acceptingOrders":true,"enableOrderBook":true},
                     {"clobTokenIds":"[\"77777777778888888888\"]","conditionId":"0x12","active":true,"closed":false,"acceptingOrders":false,"enableOrderBook":true},
                     {"clobTokenIds":"[\"99999999990000000000\"]","conditionId":"0x34","active":true,"closed":false,"acceptingOrders":true,"enableOrderBook":false}]"#,
            )
            .unwrap();
            d
        }

        #[test]
        fn pm_missing_reason_covers_every_gating_flag_and_absent() {
            let d = pm_fixture();
            assert_eq!(pm_missing_reason(&d, b"11111111112222222222"), None);
            assert_eq!(
                pm_missing_reason(&d, b"33333333334444444444"),
                Some("not_active")
            );
            assert_eq!(
                pm_missing_reason(&d, b"55555555556666666666"),
                Some("closed")
            );
            assert_eq!(
                pm_missing_reason(&d, b"77777777778888888888"),
                Some("not_accepting_orders")
            );
            assert_eq!(
                pm_missing_reason(&d, b"99999999990000000000"),
                Some("no_order_book")
            );
            assert_eq!(pm_missing_reason(&d, b"00000000000000000000"), Some("not_found"));
        }
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
    /// Split every ring in `Rings` into its producer/consumer
    /// halves, dropping the producers (the "unspawned venue"
    /// shape). Returns engine-ready `Consumers`.
    fn split_all_consumers(rings: &Rings) -> Consumers {
        let tick_lanes = {
            let mut it = rings.tick.iter().map(|r| r.clone().split().1);
            [
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
            ]
        };
        let fill_lanes = {
            let mut it = rings.fill.iter().map(|r| r.clone().split().1);
            [
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
            ]
        };
        Consumers {
            tick_lanes,
            rpc_signal: rings.rpc_signal.clone().split().1,
            rss_signal: rings.rss_signal.clone().split().1,
            fill_lanes,
        }
    }

    #[test]
    fn rings_allocate_and_split() {
        let rings = Rings::new();
        let cons = split_all_consumers(&rings);
        assert_eq!(cons.tick_lanes.len(), NUM_TICK_LANES);
        assert_eq!(cons.fill_lanes.len(), NUM_FILL_LANES);
    }

    #[test]
    fn drain_counters_add() {
        let mut a = DrainCounters {
            polymarket_ticks: 1,
            binance_ticks: 2,
            other_venue_ticks: 0,
            rpc_signals: 3,
            rss_signals: 4,
        };
        let b = DrainCounters {
            polymarket_ticks: 10,
            binance_ticks: 20,
            other_venue_ticks: 5,
            rpc_signals: 30,
            rss_signals: 40,
        };
        a.add(&b);
        assert_eq!(a.polymarket_ticks, 11);
        assert_eq!(a.binance_ticks, 22);
        assert_eq!(a.other_venue_ticks, 5);
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
        let cons = split_all_consumers(&rings);

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

    /// Build a tiny [`ingress_okx::discovery::OkxDiscovery`] fixture
    /// from `(instId, instType, live)` rows — mirrors the fixture
    /// shape used by `ingress-okx/src/discovery.rs`'s own tests, kept
    /// inline here so `build_okx_symbol_table` tests never touch the
    /// network.
    fn okx_discovery_fixture(rows: &[(&str, &str, bool)]) -> ingress_okx::discovery::OkxDiscovery {
        let mut body = String::from(r#"{"code":"0","data":["#);
        for (i, (inst_id, inst_type, live)) in rows.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            let state = if *live { "live" } else { "suspend" };
            body.push_str(&format!(
                r#"{{"instId":"{inst_id}","instType":"{inst_type}","state":"{state}","tickSz":"0.1","lotSz":"0.01","ctVal":"0.01"}}"#
            ));
        }
        body.push_str(r#"],"msg":""}"#);
        let mut d = ingress_okx::discovery::OkxDiscovery::new();
        d.ingest_body(body.as_bytes()).expect("fixture body must parse");
        d
    }

    /// Happy path: `--okx-symbols` items get 1-based, flag-ordered
    /// ordinals under the Okx venue byte, with whitespace trimmed,
    /// and each row's `OkxInstType` comes from discovery.
    #[test]
    fn okx_symbol_table_allocates_flag_ordered_ids() {
        let d = okx_discovery_fixture(&[
            ("BTC-USDT", "SPOT", true),
            ("ETH-USD-SWAP", "SWAP", true),
        ]);
        let t = build_okx_symbol_table("BTC-USDT, ETH-USD-SWAP", &d).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(
            t.lookup(b"BTC-USDT"),
            Some(make_symbol_id(VenueId::Okx, 1))
        );
        assert_eq!(
            t.lookup(b"ETH-USD-SWAP"),
            Some(make_symbol_id(VenueId::Okx, 2))
        );
        assert_eq!(t.lookup(b"XRP-USDT"), None);
        assert_eq!(
            t.get(0).map(|(_, _, ty)| ty),
            Some(ingress_okx::OkxInstType::Spot)
        );
        assert_eq!(
            t.get(1).map(|(_, _, ty)| ty),
            Some(ingress_okx::OkxInstType::Swap)
        );
    }

    /// A configured instrument the venue doesn't list live (either
    /// absent entirely or `state != "live"`) is skipped rather than
    /// failing the whole boot — the coverage pass upstream already
    /// logged it as MISSING. Ordinal allocation still advances past
    /// it (flag order stays stable regardless of venue availability).
    #[test]
    fn okx_symbol_table_skips_symbols_missing_from_discovery() {
        let d = okx_discovery_fixture(&[
            ("BTC-USDT", "SPOT", true),
            ("DEAD-USDT", "SPOT", false), // not live
            // NOPE-USDT is entirely absent from the fixture.
        ]);
        let t = build_okx_symbol_table("BTC-USDT,DEAD-USDT,NOPE-USDT", &d).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            t.lookup(b"BTC-USDT"),
            Some(make_symbol_id(VenueId::Okx, 1))
        );
        assert_eq!(t.lookup(b"DEAD-USDT"), None);
        assert_eq!(t.lookup(b"NOPE-USDT"), None);
    }

    /// Failure modes: empty item, duplicate instId, and more than
    /// `OKX_MAX_SYMBOLS` instruments all refuse boot — independent of
    /// what discovery knows about (an empty fixture is enough).
    #[test]
    fn okx_symbol_table_rejects_bad_specs() {
        // Trailing comma ⇒ empty item.
        let empty = okx_discovery_fixture(&[]);
        assert_eq!(
            build_okx_symbol_table("BTC-USDT,", &empty).err(),
            Some("okx: empty instId in --okx-symbols")
        );
        // Duplicate instId (whitespace doesn't disguise it) — flagged
        // even though neither is in the (empty) discovery fixture.
        assert_eq!(
            build_okx_symbol_table("BTC-USDT,ETH-USDT, BTC-USDT", &empty).err(),
            Some("okx: duplicate instId in --okx-symbols")
        );
        // OKX_MAX_SYMBOLS + 1 distinct instruments ⇒ Full.
        let mut spec = String::new();
        let mut rows: Vec<(String, &str, bool)> = Vec::new();
        for i in 0..=ingress_okx::OKX_MAX_SYMBOLS {
            if i > 0 {
                spec.push(',');
            }
            let inst = format!("S{i}-USDT");
            spec.push_str(&inst);
            rows.push((inst, "SPOT", true));
        }
        let rows_ref: Vec<(&str, &str, bool)> =
            rows.iter().map(|(a, b, c)| (a.as_str(), *b, *c)).collect();
        let full = okx_discovery_fixture(&rows_ref);
        assert_eq!(
            build_okx_symbol_table(&spec, &full).err(),
            Some("okx: --okx-symbols exceeds OKX_MAX_SYMBOLS instruments")
        );
        // Exactly OKX_MAX_SYMBOLS is still fine.
        let max_spec = spec.rsplit_once(',').unwrap().0;
        assert_eq!(
            build_okx_symbol_table(max_spec, &full).unwrap().len(),
            ingress_okx::OKX_MAX_SYMBOLS
        );
    }

    /// Happy path: `--deribit-symbols` items get 1-based,
    /// flag-ordered ordinals under the Deribit venue byte, with
    /// whitespace trimmed.
    #[test]
    fn deribit_symbol_table_allocates_flag_ordered_ids() {
        let t = build_deribit_symbol_table("BTC-PERPETUAL, ETH-PERPETUAL").unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(
            t.lookup(b"BTC-PERPETUAL"),
            Some(make_symbol_id(VenueId::Deribit, 1))
        );
        assert_eq!(
            t.lookup(b"ETH-PERPETUAL"),
            Some(make_symbol_id(VenueId::Deribit, 2))
        );
        assert_eq!(t.lookup(b"SOL-PERPETUAL"), None);
    }

    /// Failure modes: empty item, duplicate instrument, more than
    /// `DERIBIT_MAX_SYMBOLS` instruments, and a dotted instrument
    /// all refuse boot.
    #[test]
    fn deribit_symbol_table_rejects_bad_specs() {
        // Trailing comma ⇒ empty item.
        assert_eq!(
            build_deribit_symbol_table("BTC-PERPETUAL,").err(),
            Some("deribit: empty instrument in --deribit-symbols")
        );
        // Duplicate instrument (whitespace doesn't disguise it).
        assert_eq!(
            build_deribit_symbol_table("BTC-PERPETUAL,ETH-PERPETUAL, BTC-PERPETUAL").err(),
            Some("deribit: duplicate instrument in --deribit-symbols")
        );
        // DERIBIT_MAX_SYMBOLS + 1 distinct instruments ⇒ Full.
        let mut spec = String::new();
        for i in 0..=ingress_deribit::DERIBIT_MAX_SYMBOLS {
            if i > 0 {
                spec.push(',');
            }
            spec.push_str(&format!("S{i}-PERPETUAL"));
        }
        assert_eq!(
            build_deribit_symbol_table(&spec).err(),
            Some("deribit: --deribit-symbols exceeds DERIBIT_MAX_SYMBOLS instruments")
        );
        // Exactly DERIBIT_MAX_SYMBOLS is still fine.
        let max_spec = spec.rsplit_once(',').unwrap().0;
        assert_eq!(
            build_deribit_symbol_table(max_spec).unwrap().len(),
            ingress_deribit::DERIBIT_MAX_SYMBOLS
        );
        // A dotted instrument would corrupt channel-name parsing.
        assert_eq!(
            build_deribit_symbol_table("BTC.PERPETUAL").err(),
            Some("deribit: instrument in --deribit-symbols must not contain '.'")
        );
    }

    /// Happy path: `--hl-coins` items get 1-based, flag-ordered
    /// ordinals under the Hyperliquid venue byte, with whitespace
    /// trimmed. A HIP-4 `#<enc>` outcome coin is an ordinary item.
    #[test]
    fn hl_coin_table_allocates_flag_ordered_ids() {
        let t = build_hl_coin_table("BTC, ETH,#330").unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(
            t.lookup(b"BTC"),
            Some(make_symbol_id(VenueId::Hyperliquid, 1))
        );
        assert_eq!(
            t.lookup(b"ETH"),
            Some(make_symbol_id(VenueId::Hyperliquid, 2))
        );
        assert_eq!(
            t.lookup(b"#330"),
            Some(make_symbol_id(VenueId::Hyperliquid, 3))
        );
        assert_eq!(t.lookup(b"SOL"), None);
    }

    /// Failure modes: empty item, duplicate coin, an over-long
    /// coin, and more than `HL_MAX_COINS` coins all refuse boot.
    #[test]
    fn hl_coin_table_rejects_bad_specs() {
        // Trailing comma ⇒ empty item.
        assert_eq!(
            build_hl_coin_table("BTC,").err(),
            Some("hl: empty coin in --hl-coins")
        );
        // Duplicate coin (whitespace doesn't disguise it).
        assert_eq!(
            build_hl_coin_table("BTC,ETH, BTC").err(),
            Some("hl: duplicate coin in --hl-coins")
        );
        // A coin longer than HL_COIN_MAX bytes ⇒ TooLong.
        let long = "C".repeat(ingress_hyperliquid::HL_COIN_MAX + 1);
        assert_eq!(
            build_hl_coin_table(&long).err(),
            Some("hl: coin in --hl-coins exceeds HL_COIN_MAX bytes")
        );
        // HL_MAX_COINS + 1 distinct coins ⇒ Full.
        let mut spec = String::new();
        for i in 0..=ingress_hyperliquid::HL_MAX_COINS {
            if i > 0 {
                spec.push(',');
            }
            spec.push_str(&format!("C{i}"));
        }
        assert_eq!(
            build_hl_coin_table(&spec).err(),
            Some("hl: --hl-coins exceeds HL_MAX_COINS coins")
        );
        // Exactly HL_MAX_COINS is still fine.
        let max_spec = spec.rsplit_once(',').unwrap().0;
        assert_eq!(
            build_hl_coin_table(max_spec).unwrap().len(),
            ingress_hyperliquid::HL_MAX_COINS
        );
    }

    // -----------------------------------------------------------
    // parse_raw_tap_flags (Part B.3 — --raw-tap / --raw-tap-mode /
    // --raw-tap-budget-mb)
    // -----------------------------------------------------------

    /// No `--raw-tap` ⇒ every venue's tap stays off, regardless of
    /// the (clap-default) `--raw-tap-mode` / `--raw-tap-budget-mb`.
    #[test]
    fn raw_tap_flags_default_to_off() {
        let cfg = parse_raw_tap_flags(None, "rejects", 64).unwrap();
        for c in [cfg.pm, cfg.bn, cfg.okx, cfg.rpc, cfg.deribit, cfg.hl] {
            assert_eq!(c.mode, TapMode::Off);
            assert_eq!(c.budget_bytes, 0);
        }
    }

    /// An empty (or all-whitespace) `--raw-tap` value behaves exactly
    /// like it being absent.
    #[test]
    fn raw_tap_flags_empty_string_is_off() {
        let cfg = parse_raw_tap_flags(Some("   "), "rejects", 64).unwrap();
        assert_eq!(cfg.okx.mode, TapMode::Off);
        assert_eq!(cfg.okx.budget_bytes, 0);
    }

    /// `--raw-tap all` enables every venue with the shared mode +
    /// budget.
    #[test]
    fn raw_tap_flags_all_enables_every_venue() {
        let cfg = parse_raw_tap_flags(Some("all"), "all", 8).unwrap();
        let want_bytes = 8 * 1024 * 1024;
        for c in [cfg.pm, cfg.bn, cfg.okx, cfg.rpc, cfg.deribit, cfg.hl] {
            assert_eq!(c.mode, TapMode::All);
            assert_eq!(c.budget_bytes, want_bytes);
        }
    }

    /// A CSV subset enables only the named venues (whitespace
    /// trimmed); unnamed venues stay off.
    #[test]
    fn raw_tap_flags_csv_subset_enables_only_named_venues() {
        let cfg = parse_raw_tap_flags(Some(" pm, okx "), "rejects", 32).unwrap();
        let want_bytes = 32 * 1024 * 1024;
        assert_eq!(cfg.pm.mode, TapMode::Rejects);
        assert_eq!(cfg.pm.budget_bytes, want_bytes);
        assert_eq!(cfg.okx.mode, TapMode::Rejects);
        assert_eq!(cfg.okx.budget_bytes, want_bytes);
        for c in [cfg.bn, cfg.rpc, cfg.deribit, cfg.hl] {
            assert_eq!(c.mode, TapMode::Off);
            assert_eq!(c.budget_bytes, 0);
        }
    }

    /// Every known capture-venue label is accepted in one CSV.
    #[test]
    fn raw_tap_flags_every_known_venue_label_accepted() {
        let cfg = parse_raw_tap_flags(Some("pm,bn,okx,rpc,deribit,hl"), "all", 1).unwrap();
        for c in [cfg.pm, cfg.bn, cfg.okx, cfg.rpc, cfg.deribit, cfg.hl] {
            assert_eq!(c.mode, TapMode::All);
        }
    }

    /// `rss` is not a capture-bearing venue label (the RSS ingress
    /// owns no `PmlrCapture`, §6.5) — it must be rejected like any
    /// other unknown label, not silently ignored.
    #[test]
    fn raw_tap_flags_rejects_rss_label() {
        assert_eq!(
            parse_raw_tap_flags(Some("rss"), "rejects", 64).err(),
            Some("--raw-tap: unknown venue label")
        );
    }

    /// Failure modes: unknown label, duplicate label, an empty item
    /// (trailing comma), and a bad `--raw-tap-mode` value all refuse
    /// to build a config.
    #[test]
    fn raw_tap_flags_rejects_bad_specs() {
        assert_eq!(
            parse_raw_tap_flags(Some("bogus"), "rejects", 64).err(),
            Some("--raw-tap: unknown venue label")
        );
        assert_eq!(
            parse_raw_tap_flags(Some("pm,pm"), "rejects", 64).err(),
            Some("--raw-tap: duplicate venue label")
        );
        assert_eq!(
            parse_raw_tap_flags(Some("pm,"), "rejects", 64).err(),
            Some("--raw-tap: empty venue label")
        );
        assert_eq!(
            parse_raw_tap_flags(Some("pm"), "loud", 64).err(),
            Some("--raw-tap-mode must be 'rejects' or 'all'")
        );
    }

    /// More than six comma-separated labels trips the defensive
    /// capacity guard — there are only six known venues, so this
    /// branch is a pure defense-in-depth backstop reached here by
    /// listing all six plus a seventh item.
    #[test]
    fn raw_tap_flags_rejects_more_labels_than_known_venues() {
        assert_eq!(
            parse_raw_tap_flags(Some("pm,bn,okx,rpc,deribit,hl,pm2"), "rejects", 64).err(),
            Some("--raw-tap: more venue labels than known venues")
        );
    }

    /// `--raw-tap-budget-mb` converts MiB → bytes and saturates
    /// rather than overflowing on an absurd operator-supplied value.
    #[test]
    fn raw_tap_flags_budget_mb_converts_and_saturates() {
        let cfg = parse_raw_tap_flags(Some("hl"), "all", 2).unwrap();
        assert_eq!(cfg.hl.budget_bytes, 2 * 1024 * 1024);

        let cfg = parse_raw_tap_flags(Some("hl"), "all", u64::MAX).unwrap();
        assert_eq!(cfg.hl.budget_bytes, u64::MAX);
    }
}
