// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

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
//! | T4     | ingress-ai (UDS command listener; only with `AI_INGRESS_HMAC_KEY`) |
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
use core_io::{SlotCapture, SlotKind};
use core_metrics::{GaugeId, IngressState, IngressStatus, MetricsRegistry};
use core_net::{Backoff, Keepalive, KeepaliveCfg, TlsTransport};
use core_ring::{Consumer, Producer, Ring};
use core_time::now_ns;
use core_types::{
    make_symbol_id, AiCmd, Capture, ChannelEvent, DepthTopK, Fill, NsTs, Order, RuleTableSlot,
    OptSummary, Signal, SymbolId, Tick, VenueId, AI_RING_SIZE, DEPTH_RING_SIZE,
    EVENT_LANE_ASSET_CTX, EVENT_LANE_FUNDING, OPT_RING_SIZE,
    EVENT_RING_SIZE, RULE_TABLE_RING_SLOTS,
};
use engine::{
    Engine, ENGINE_FILLS_FILE, ENGINE_ORDERS_FILE, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES,
    SIGNAL_RING_SIZE, TICK_RING_SIZE,
};
use ingress_ai::{AiCmdCapture, AiIngressCfg, RulesetSidePath};
// Re-exported (lib.rs) so the binary reaches the AI status slot type
// through `cli::` like every other paper-mode surface.
pub use ingress_ai::AiIngressStatus;
use rustls_pki_types::ServerName;
use strategy_latency_arb::LatencyArb;

use ingress_binance::run_loop as bwl;
use ingress_bybit::run_loop as ywl;
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
fn spawn_or_die(
    builder: thread::Builder,
    name: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> JoinHandle<()> {
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
/// WS9: Bybit wants a `{"op":"ping"}` at least every 20 s; the probe
/// goes out at 15 s and anything quieter than 30 s is a dead session.
const BYBIT_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 15_000_000_000,
    idle_timeout_ns: 30_000_000_000,
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
    /// One fill ring per execution lane (`engine::fill_lane_of`).
    /// Live dispatchers gain producers in Phase 8j; until then the
    /// engine's dispatcher fill pump (D3) is the only fill source.
    pub fill: [Arc<Ring<Fill, FILL_RING_SIZE>>; NUM_FILL_LANES],
    /// AI command ring (Phase 8f §4.3). Producer half goes to the
    /// `ingress-ai` thread when `AI_INGRESS_HMAC_KEY` is configured;
    /// otherwise it is dropped and the engine's AI lane reads empty
    /// forever (the unspawned-venue shape, §3.3).
    pub ai: Arc<Ring<AiCmd, AI_RING_SIZE>>,
    /// Ruleset-table handoff ring (Phase 8g §6, D1a): SPSC, one
    /// validated table per Stage at operator cadence. Producer half
    /// rides with the AI lane into `spawn_ai`; the consumer half
    /// PARKS in the bin's boot plumbing until item 7 wires the
    /// engine's pre-AI-drain pop. Unspawned shape mirrors `ai`: no
    /// `AI_INGRESS_HMAC_KEY` ⇒ producer dropped, ring reads empty
    /// forever.
    pub ruleset_tables: Arc<Ring<RuleTableSlot, RULE_TABLE_RING_SLOTS>>,
    /// WS10-A: one venue-event ring per tick lane (same indexing).
    /// Producers ride into the four funding-capable venue spawns
    /// (bn/okx/deribit/bybit); PM and RPC lanes never see a producer
    /// push (§3.3 unspawned shape — the engine drains them empty).
    pub event: [Arc<Ring<ChannelEvent, EVENT_RING_SIZE>>; engine::NUM_EVENT_LANES],
    /// WS10-B: one depth ring per depth lane (`engine::depth_lane_of`
    /// order: 0 = OKX, 1 = Deribit). Producers ride into the two
    /// depth-capable spawns; without a depth subscription the lane
    /// reads empty forever (§3.3).
    pub depth: [Arc<Ring<DepthTopK, DEPTH_RING_SIZE>>; engine::NUM_DEPTH_LANES],
    /// VM2 V2: one options-summary ring per opt lane
    /// (`engine::opt_lane_of` order: 0 = OKX, 1 = Deribit,
    /// 2 = Binance eapi). Producers ride into the three
    /// options-capable spawns; without an options subscription the
    /// lane reads empty forever (§3.3).
    pub opt: [Arc<Ring<OptSummary, OPT_RING_SIZE>>; engine::NUM_OPT_LANES],
}

impl Rings {
    /// Allocate all rings. Single call; never used on hot path.
    pub fn new() -> Self {
        Self {
            tick: [
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
            ],
            rpc_signal: Ring::new(),
            fill: [Ring::new(), Ring::new(), Ring::new(), Ring::new()],
            ai: Ring::new(),
            ruleset_tables: Ring::new(),
            event: [
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
                Ring::new(),
            ],
            depth: [Ring::new(), Ring::new()],
            opt: [Ring::new(), Ring::new(), Ring::new()],
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
/// [`Observability`] (reader).
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
    /// WS9: Bybit WSS thread (spot + linear conns, one thread).
    /// Stays Down when the `[bybit]` section is empty.
    pub bybit: Arc<IngressStatus>,
    /// Polygon RPC WSS thread.
    pub rpc: Arc<IngressStatus>,
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
            bybit: Arc::new(IngressStatus::new()),
            rpc: Arc::new(IngressStatus::new()),
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
    asset_ids: Vec<Vec<u8>>,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "pm", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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

            // M1 multi-market: one connection, one subscribe frame
            // listing every configured token id (the driver keeps the
            // table across reconnects).
            let id_refs: Vec<&[u8]> = asset_ids.iter().map(|v| v.as_slice()).collect();
            let mut driver = pwl::Driver::new_multi(now_ns(), &id_refs);
            drop(id_refs);
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
                let ticks_before = status.ticks_total();

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
                capture.mirror_now();
                if matches!(res, pwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // T1(b): only moved MARKET DATA (or a rate-limited
                // idle trip) restarts the schedule — see
                // `should_reset_backoff` (D8 restored).
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, pwl::RunResult::IdleTimeout),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
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
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    mut opt_tx: Producer<OptSummary, OPT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "bn", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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
                let ticks_before = status.ticks_total();

                let res = bwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut event_tx,
                    EVENT_LANE_FUNDING,
                    &mut opt_tx,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "binance: run-loop returned");
                capture.mirror_now();
                if matches!(res, bwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // T1(b): see `should_reset_backoff` (D8 restored).
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, bwl::RunResult::IdleTimeout),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
            status.set_state(IngressState::Down);
        },
    ))
}

/// One resolved Binance connection spec for [`spawn_binance_multi`]:
/// host + path + pinned sym. Spot and USDS-M futures slots mix
/// freely — each slot carries its own host (M1 design).
pub struct BinanceConnSpec {
    /// WS host (spot: `BINANCE_WS_HOST`; USDS-M: `BINANCE_FUT_WS_HOST`;
    /// eapi options: `BINANCE_EAPI_WS_HOST`).
    pub host: String,
    /// Stream path (`/ws/<symbol>@bookTicker`, or the M2.4 eapi
    /// combined `/stream?streams=…`).
    pub path: String,
    /// Pinned SymbolId (the M1 allocation law; 0 sentinel on an eapi
    /// slot — its syms live in the lane table).
    pub sym: core_types::SymbolId,
    /// M2.4: present ⇒ this slot is the eapi combined options stream
    /// — the boot-built symbol table + the configured underlyings
    /// (stream-lowercased inside the lane).
    pub eapi: Option<(ingress_binance::eapi::EapiSymbolTable, Vec<String>)>,
    /// WS5: true ⇒ this slot is a `/ws/<sym>@markPrice` stream (the
    /// capture-only mark/index/funding lane; `sym` pinned like
    /// bookTicker). Mutually exclusive with `eapi`.
    pub mark_price: bool,
}

/// Spawn the M1 multi-symbol Binance ingress thread: N single-stream
/// connections (ONE per instrument — the parser stays byte-frozen),
/// ONE thread, ONE producer (single-writer law), one `"bn"` capture.
/// `ingress_binance::run_multi` owns the in-thread reconnect pacing
/// (one dial per poll iteration, jittered per-slot backoff). See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_binance_multi(
    specs: Vec<BinanceConnSpec>,
    tls_config: RustlsConfig,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    mut opt_tx: Producer<OptSummary, OPT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "bn", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "bn", VenueId::Binance.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name(format!("ingress-binance-x{}", specs.len())),
        "ingress-binance",
        move || {
            log_pin_outcome("binance", core_id);
            // Resolve every endpoint + server name up front; failure is
            // fatal for the venue thread (the single-connection
            // wrapper's bad-server-name posture, applied per slot).
            let mut eps: Vec<WssEndpoint> = Vec::with_capacity(specs.len());
            let mut names: Vec<ServerName> = Vec::with_capacity(specs.len());
            for spec in &specs {
                let ep = match WssEndpoint::resolve(&spec.host, 443, &spec.path) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(error = ?e, host = %spec.host, "binance: DNS failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                let name = match TlsTransport::server_name_from_host(&ep.host) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = ?e, "binance: bad server name");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                eps.push(ep);
                names.push(name);
            }
            let (mut poll, mut events, _token) = match new_poll() {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "binance: mio init failed");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let mut conns: Vec<bwl::MultiConn<TlsTransport>> = Vec::with_capacity(specs.len());
            for (i, spec) in specs.into_iter().enumerate() {
                // M2.4: an eapi spec builds the combined-stream lane
                // driver; WS5: a markPrice spec builds the mark lane;
                // bookTicker slots stay byte-identical.
                let drv = match spec.eapi {
                    Some((table, ulys)) => {
                        let uly_refs: Vec<&[u8]> = ulys.iter().map(|s| s.as_bytes()).collect();
                        bwl::Driver::new_eapi(
                            now_ns().wrapping_add(i as u64),
                            ingress_binance::eapi::EapiLane::new(table, &uly_refs),
                        )
                    }
                    None if spec.mark_price => {
                        bwl::Driver::new_mark_price(now_ns().wrapping_add(i as u64), spec.sym)
                    }
                    None => bwl::Driver::new(now_ns().wrapping_add(i as u64), spec.sym),
                };
                conns.push(bwl::MultiConn::new(
                    drv,
                    eps[i].host.as_bytes(),
                    eps[i].path.as_bytes(),
                    Keepalive::new(BN_KEEPALIVE),
                    Backoff::default_for_ingress(core_id as u64 + 1 + i as u64),
                ));
            }
            status.set_state(IngressState::Connecting);
            let res = bwl::run_multi(
                &mut conns,
                &mut producer,
                &mut event_tx,
                EVENT_LANE_FUNDING,
                &mut opt_tx,
                &mut poll,
                &mut events,
                &SHUTDOWN,
                &status,
                &mut capture,
                |i| match connect_tls(&eps[i], &names[i], &tls_config) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::warn!(error = ?e, host = %eps[i].host, slot = i, "binance: connect failed");
                        None
                    }
                },
            );
            tracing::info!(?res, "binance: multi run-loop returned");
            capture.mirror_now();
            status.set_state(IngressState::Down);
        },
    ))
}

/// WS9: one resolved Bybit connection spec for [`spawn_bybit`] —
/// a class (spot / linear) with its own symbol table.
pub struct BybitConnSpec {
    /// Stream path (`/v5/public/spot` or `/v5/public/linear`).
    pub path: String,
    /// This connection's `SYMBOL → SymbolId` table.
    pub table: ingress_bybit::BybitSymbolTable,
    /// True on the linear conn: subscribe `tickers.<SYM>` too.
    pub want_tickers: bool,
}

/// WS9: spawn the Bybit ingress thread — N single-class connections
/// (spot + linear) on ONE thread, ONE producer (single-writer law),
/// one `"bybit"` capture. `ingress_bybit::run_multi` owns the
/// in-thread reconnect pacing + the WS2 establishment budget. See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
#[allow(clippy::too_many_arguments)]
pub fn spawn_bybit(
    host: String,
    specs: Vec<BybitConnSpec>,
    tls_config: RustlsConfig,
    stale_after_ms: u32,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "bybit", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "bybit", VenueId::Bybit.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name(format!("ingress-bybit-x{}", specs.len())),
        "ingress-bybit",
        move || {
            log_pin_outcome("bybit", core_id);
            let mut eps: Vec<WssEndpoint> = Vec::with_capacity(specs.len());
            let mut names: Vec<ServerName> = Vec::with_capacity(specs.len());
            for spec in &specs {
                let ep = match WssEndpoint::resolve(&host, 443, &spec.path) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(error = ?e, host = %host, "bybit: DNS failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                let name = match TlsTransport::server_name_from_host(&ep.host) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = ?e, "bybit: bad server name");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                eps.push(ep);
                names.push(name);
            }
            let (mut poll, mut events, _token) = match new_poll() {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "bybit: mio init failed");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let mut conns: Vec<ywl::BybitConn<TlsTransport>> = Vec::with_capacity(specs.len());
            for (i, spec) in specs.into_iter().enumerate() {
                let mut drv = ywl::Driver::new(
                    now_ns().wrapping_add(i as u64),
                    spec.table,
                    spec.want_tickers,
                );
                // VT2: one estimator per CONNECTION, same threshold.
                drv.set_stale_after_ms(stale_after_ms);
                conns.push(ywl::BybitConn::new(
                    drv,
                    eps[i].host.as_bytes(),
                    eps[i].path.as_bytes(),
                    Keepalive::new(BYBIT_KEEPALIVE),
                    Backoff::default_for_ingress(core_id as u64 + 1 + i as u64),
                ));
            }
            status.set_state(IngressState::Connecting);
            let res = ywl::run_multi(
                &mut conns,
                &mut producer,
                &mut event_tx,
                EVENT_LANE_FUNDING,
                &mut poll,
                &mut events,
                &SHUTDOWN,
                &status,
                &mut capture,
                |i| match connect_tls(&eps[i], &names[i], &tls_config) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::warn!(error = ?e, host = %eps[i].host, slot = i, "bybit: connect failed");
                        None
                    }
                },
            );
            // T1(a): name any recorded session error on the exit line.
            let err = status.take_last_err();
            tracing::info!(
                ?res,
                err_site = core_metrics::err_site_name(err.site),
                io_kind = core_metrics::io_kind_name(err.io_kind),
                venue_code = err.venue_code as i32,
                "bybit: multi run-loop returned"
            );
            capture.mirror_now();
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
/// or more than [`ingress_okx::OKX_STATIC_MAX`] instruments — boot
/// refuses to start rather than run with a venue map that doesn't
/// match the operator's intent. (M2.2: discovered options rows join
/// the table AFTER this via [`extend_okx_table_with_options`].)
pub fn build_okx_symbol_table(
    spec: &str,
    discovery: &ingress_okx::discovery::OkxDiscovery,
) -> Result<ingress_okx::OkxSymbolTable, &'static str> {
    let mut table = ingress_okx::OkxSymbolTable::new();
    // Raw-spec dedupe list, independent of what actually gets
    // inserted (a MISSING item must still trip the duplicate check).
    let mut seen: [&str; ingress_okx::OKX_STATIC_MAX] = [""; ingress_okx::OKX_STATIC_MAX];
    for (n_seen, item) in spec.split(',').enumerate() {
        let inst_id = item.trim();
        if inst_id.is_empty() {
            return Err("okx: empty instId in --okx-symbols");
        }
        if seen[..n_seen].contains(&inst_id) {
            return Err("okx: duplicate instId in --okx-symbols");
        }
        if n_seen >= ingress_okx::OKX_STATIC_MAX {
            return Err("okx: --okx-symbols exceeds OKX_STATIC_MAX instruments");
        }
        seen[n_seen] = inst_id;

        // Ordinals are 1-based, consumed per spec item (missing rows
        // still burn theirs) — lockstep with the dedupe count.
        let sym = make_symbol_id(VenueId::Okx, (n_seen + 1) as u32);
        let Some(row) = discovery.find(inst_id.as_bytes()).filter(|r| r.live) else {
            // MISSING — already logged by boot_discovery's coverage
            // pass; the table just doesn't carry a row for it.
            continue;
        };
        match table.insert(inst_id.as_bytes(), sym, row.inst_type) {
            Ok(()) => {}
            Err(ingress_okx::SymbolTableErr::Full) => {
                // Unreachable: n_seen caps at OKX_STATIC_MAX < table
                // capacity — kept as a defensive arm.
                return Err("okx: --okx-symbols exceeds OKX_STATIC_MAX instruments");
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

/// M2.2: append the discovered capped options chain to an OKX symbol
/// table (after every static insert; `bbo-tbt`-only rows — the
/// `OkxInstType::Option` tag drives the channel gating). `pairs`
/// comes from `boot_discovery::Outcome::okx_options` — already
/// deterministic-ordered and ordinal-allocated. Fails fast on
/// duplicates and on the options-block cap.
pub fn extend_okx_table_with_options(
    table: &mut ingress_okx::OkxSymbolTable,
    pairs: &[(String, core_types::SymbolId)],
) -> Result<(), &'static str> {
    // The OKX table has no static/options partition field (the
    // instType tag IS the discriminator) — derive the current
    // options count so the cap holds across calls.
    let mut n_options: usize = 0;
    let mut i = 0;
    while let Some((_, _, it)) = table.get(i) {
        if it == ingress_okx::OkxInstType::Option {
            n_options += 1;
        }
        i += 1;
    }
    for (inst_id, sym) in pairs {
        if table.lookup(inst_id.as_bytes()).is_some() {
            return Err("okx: duplicate instId in discovered options chain");
        }
        if n_options >= ingress_okx::OKX_OPT_MAX {
            return Err("okx: options chain exceeds OKX_OPT_MAX — shrink \
                 options_underlyings/options_expiries/options_strikes");
        }
        match table.insert(inst_id.as_bytes(), *sym, ingress_okx::OkxInstType::Option) {
            Ok(()) => n_options += 1,
            Err(ingress_okx::SymbolTableErr::Full) => {
                return Err(
                    "okx: options chain exceeds the symbol-table capacity — shrink \
                     options_underlyings/options_expiries/options_strikes",
                );
            }
            Err(ingress_okx::SymbolTableErr::TooLong) => {
                return Err("okx: discovered option instId exceeds OKX_INST_ID_MAX");
            }
            Err(ingress_okx::SymbolTableErr::Empty) => {
                return Err("okx: empty discovered option instId");
            }
        }
    }
    Ok(())
}

/// Spawn the OKX v5 public-WS ingress thread (Phase 8b). One thread
/// covers every configured instrument — the driver batches all
/// `(channel × instId)` pairs into a single subscribe op (§4.1).
/// `depth_enabled` adds the 400-level `books` channel per
/// instrument (`--okx-depth`; capture + integrity only, §4.5). See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
/// M2.3: `opt_families` carries the configured option underlyings
/// (`[okx] options_underlyings`) for the family-keyed `opt-summary`
/// subscription — empty = no options analytics lane.
#[allow(clippy::too_many_arguments)]
pub fn spawn_okx(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    symbols: ingress_okx::OkxSymbolTable,
    depth_enabled: bool,
    opt_families: Vec<String>,
    stale_after_ms: u32,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    mut depth_tx: Producer<DepthTopK, DEPTH_RING_SIZE>,
    mut opt_tx: Producer<OptSummary, OPT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "okx", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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

            // Boot-time (pre-loop) allocation: family byte refs for
            // the driver's fixed-capacity family table.
            let fam_refs: Vec<&[u8]> = opt_families.iter().map(|s| s.as_bytes()).collect();
            let mut driver = owl::Driver::new(now_ns(), symbols, depth_enabled, &fam_refs);
            // VT2: venue default or the operator's `--stale-after-ms okx:<ms>`.
            driver.set_stale_after_ms(stale_after_ms);
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
                let ticks_before = status.ticks_total();

                let res = owl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut event_tx,
                    EVENT_LANE_FUNDING,
                    &mut depth_tx,
                    &mut opt_tx,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                // T1(a): name the failure on the very line the
                // operator greps (outage 2026-08-27 §5.5 — six days
                // of `res=Error` with zero diagnostic payload).
                let err = status.take_last_err();
                tracing::info!(
                    ?res,
                    err_site = core_metrics::err_site_name(err.site),
                    io_kind = core_metrics::io_kind_name(err.io_kind),
                    venue_code = err.venue_code as i32,
                    "okx: run-loop returned"
                );
                capture.mirror_now();
                if matches!(res, owl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // T1(b): see `should_reset_backoff` (D8 restored).
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, owl::RunResult::IdleTimeout),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
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
/// [`ingress_deribit::DERIBIT_STATIC_MAX`] instruments — boot
/// refuses to start rather than run with a venue map that doesn't
/// match the operator's intent. (M2.1: discovered options rows join
/// the table AFTER this via `insert_option` — the bin's
/// options-chain arm.)
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
        match table.insert(
            instrument.as_bytes(),
            make_symbol_id(VenueId::Deribit, ordinal),
        ) {
            Ok(()) => {}
            Err(ingress_deribit::SymbolTableErr::Full) => {
                return Err("deribit: --deribit-symbols exceeds DERIBIT_STATIC_MAX instruments");
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
            Err(ingress_deribit::SymbolTableErr::StaticAfterOptions)
            | Err(ingress_deribit::SymbolTableErr::OptionAfterCombos) => {
                // This builder only performs static inserts, before
                // any option/combo insert — unreachable by
                // construction.
                debug_assert!(false, "static-only builder saw a build-order error");
                return Err("deribit: internal symbol-table build-order violation");
            }
        }
    }
    Ok(table)
}

/// M2.1: append the discovered capped options chain to a Deribit
/// symbol table (after every static insert; quote-only subscription
/// rows). `pairs` comes from `boot_discovery::Outcome::deribit_options`
/// — already deterministic-ordered and ordinal-allocated. Fails fast
/// on duplicates (a chain listing an instrument twice is a venue
/// contract violation) and on the options-block cap.
pub fn extend_deribit_table_with_options(
    table: &mut ingress_deribit::DeribitSymbolTable,
    pairs: &[(String, core_types::SymbolId)],
) -> Result<(), &'static str> {
    for (name, sym) in pairs {
        if table.lookup(name.as_bytes()).is_some() {
            return Err("deribit: duplicate instrument in discovered options chain");
        }
        match table.insert_option(name.as_bytes(), *sym) {
            Ok(()) => {}
            Err(ingress_deribit::SymbolTableErr::Full) => {
                return Err("deribit: options chain exceeds DERIBIT_OPT_MAX — shrink \
                     options_underlyings/options_expiries/options_strikes");
            }
            Err(ingress_deribit::SymbolTableErr::TooLong) => {
                return Err("deribit: discovered option instrument exceeds DERIBIT_INSTR_MAX");
            }
            Err(ingress_deribit::SymbolTableErr::Empty) => {
                return Err("deribit: empty discovered option instrument");
            }
            Err(ingress_deribit::SymbolTableErr::HasDot) => {
                return Err("deribit: discovered option instrument must not contain '.'");
            }
            Err(ingress_deribit::SymbolTableErr::StaticAfterOptions) => {
                debug_assert!(false, "insert_option never reports StaticAfterOptions");
                return Err("deribit: internal symbol-table build-order violation");
            }
            Err(ingress_deribit::SymbolTableErr::OptionAfterCombos) => {
                // The bin inserts options BEFORE combos (WS6 partition
                // law) — unreachable in that order.
                debug_assert!(false, "options inserted after combos");
                return Err("deribit: internal symbol-table build-order violation");
            }
        }
    }
    Ok(())
}

/// WS6: append the configured option COMBOS to a Deribit symbol
/// table — AFTER every static and option insert (partition law).
/// Quote-only rows; combos share the venue's 64-row option-block
/// capacity, so a full discovered chain plus a long combo list is a
/// boot error naming the knobs to shrink.
pub fn extend_deribit_table_with_combos(
    table: &mut ingress_deribit::DeribitSymbolTable,
    combos: &[(String, core_types::SymbolId)],
) -> Result<(), &'static str> {
    for (name, sym) in combos {
        if table.lookup(name.as_bytes()).is_some() {
            return Err("deribit: combo duplicates a configured/discovered instrument");
        }
        match table.insert_combo(name.as_bytes(), *sym) {
            Ok(()) => {}
            Err(ingress_deribit::SymbolTableErr::Full) => {
                return Err(
                    "deribit: options + combos exceed the 64-row tail block — shrink \
                     options_underlyings/options_expiries/options_strikes or the combo list",
                );
            }
            Err(ingress_deribit::SymbolTableErr::TooLong) => {
                return Err("deribit: combo instrument exceeds DERIBIT_INSTR_MAX");
            }
            Err(ingress_deribit::SymbolTableErr::Empty) => {
                return Err("deribit: empty combo instrument");
            }
            Err(ingress_deribit::SymbolTableErr::HasDot) => {
                return Err("deribit: combo instrument must not contain '.'");
            }
            Err(ingress_deribit::SymbolTableErr::StaticAfterOptions)
            | Err(ingress_deribit::SymbolTableErr::OptionAfterCombos) => {
                debug_assert!(false, "insert_combo never reports build-order errors");
                return Err("deribit: internal symbol-table build-order violation");
            }
        }
    }
    Ok(())
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
    dvol_indices: Vec<String>,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    mut depth_tx: Producer<DepthTopK, DEPTH_RING_SIZE>,
    mut opt_tx: Producer<OptSummary, OPT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "deribit", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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

            // WS6: DVOL index subscriptions (empty = none — the
            // pre-WS6 shape).
            let dvol_refs: Vec<&[u8]> = dvol_indices.iter().map(|s| s.as_bytes()).collect();
            let mut driver =
                dwl::Driver::new_with_dvol(now_ns(), symbols, depth_enabled, &dvol_refs);
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
                let ticks_before = status.ticks_total();

                let res = dwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut event_tx,
                    EVENT_LANE_FUNDING,
                    &mut depth_tx,
                    &mut opt_tx,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                // T1(a): name the failure on the very line the
                // operator greps (outage 2026-08-27 §5.5). For the
                // subscribe-missing site, venue_code = COUNT of
                // missing channels (u128 masks don't fit a gauge).
                let err = status.take_last_err();
                tracing::info!(
                    ?res,
                    err_site = core_metrics::err_site_name(err.site),
                    io_kind = core_metrics::io_kind_name(err.io_kind),
                    venue_code = err.venue_code as i32,
                    "deribit: run-loop returned"
                );
                capture.mirror_now();
                if matches!(res, dwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // T1(b): see `should_reset_backoff` (D8 restored).
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, dwl::RunResult::IdleTimeout),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
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
pub fn build_hl_coin_table(spec: &str) -> Result<ingress_hyperliquid::HlCoinTable, &'static str> {
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
        match table.insert(
            coin.as_bytes(),
            make_symbol_id(VenueId::Hyperliquid, ordinal),
        ) {
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
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "hl", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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
                let ticks_before = status.ticks_total();

                let res = hwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut event_tx,
                    // VM2 V2: HL funding rides AssetCtx — the lane
                    // mask carries both bits (feature-engine law).
                    EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                tracing::info!(?res, "hyperliquid: run-loop returned");
                capture.mirror_now();
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
                // T1(b): see `should_reset_backoff` (D8 restored).
                // A staleness trip is budget-limited like an idle
                // timeout — both count as venue-quiet trips.
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, hwl::RunResult::IdleTimeout | hwl::RunResult::Stale),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
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
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "rpc", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
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
                let ticks_before = status.ticks_total();

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
                capture.mirror_now();
                if matches!(res, rwl::RunResult::Stopped) {
                    status.set_state(IngressState::Down);
                    return;
                }
                // T1(b): see `should_reset_backoff` (D8 restored).
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    matches!(res, rwl::RunResult::IdleTimeout),
                ) {
                    backoff.reset();
                }
                status.inc_reconnects();
                status.set_state(IngressState::Backoff);
                sleep_backoff(&mut backoff);
            }
            capture.mirror_now();
            status.set_state(IngressState::Down);
        },
    ))
}

/// Open the Phase-8f engine-thread fills capture
/// (`<run_dir>/engine-fills.pmlr`, `SlotKind::Fill`). Boot-only; the
/// bin hands the result to [`Observability::with_fills_capture`] and
/// the engine loop takes ownership from there.
pub fn open_fills_capture(run_dir: &Path, epoch_ns: u64) -> io::Result<SlotCapture<Fill>> {
    SlotCapture::open(run_dir.join(ENGINE_FILLS_FILE), SlotKind::Fill, epoch_ns)
}

/// Open the M4.1 engine-thread order-intent capture
/// (`<run_dir>/engine-orders.pmlr`, `SlotKind::Order`). Boot-only; the
/// bin hands the result to [`Observability::with_orders_capture`] and
/// the engine loop takes ownership from there.
pub fn open_orders_capture(run_dir: &Path, epoch_ns: u64) -> io::Result<SlotCapture<Order>> {
    SlotCapture::open(run_dir.join(ENGINE_ORDERS_FILE), SlotKind::Order, epoch_ns)
}

/// Parse `AI_INGRESS_HMAC_KEY` (64 hex chars) into the 32-byte HMAC
/// key (Phase 8f §4.1). The error strings deliberately carry **no key
/// material** — this value must never reach a log. Returns `Err` on
/// wrong length or a non-hex nibble; absence is the *caller's*
/// decision (unset ⇒ ingress-ai not spawned, see the bin wiring).
pub fn parse_ai_hmac_key(hex: &str) -> Result<[u8; 32], &'static str> {
    let b = hex.trim().as_bytes();
    if b.len() != 64 {
        return Err("AI_INGRESS_HMAC_KEY must be exactly 64 hex chars");
    }
    let nibble = |c: u8| -> Result<u8, &'static str> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err("AI_INGRESS_HMAC_KEY contains a non-hex character"),
        }
    };
    let mut key = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        key[i] = (nibble(b[i * 2])? << 4) | nibble(b[i * 2 + 1])?;
        i += 1;
    }
    Ok(key)
}

/// Mirror the AI ingress thread's capture health into its two registry
/// gauges (`engine_ingress_ai_capture_{io_errors,records}`). Same
/// constraint as [`mirror_capture_metrics`]: the capture is owned by
/// the spawned thread, so the thread itself mirrors — after every
/// `run` return and once before exit. Between those points the pair
/// is stale by design (venue-wrapper parity); live AI health comes
/// from the centrally mirrored [`AiIngressStatus`] counters.
fn mirror_ai_capture_metrics(metrics: &CaptureMetrics, capture: &AiCmdCapture) {
    if let Some((reg, ids)) = metrics.as_ref() {
        reg.gauge(ids.io_errors).set(capture.io_errors() as i64);
        reg.gauge(ids.records).set(capture.records() as i64);
    }
}

/// Build the §4.3 boot-universe snapshot for the ruleset validator:
/// every SymbolId the boot wired into a venue ingress — the PM/BN
/// pair flags plus each discovery-gated venue table — **sorted
/// strict-ascending and deduped** (binary-searched per §4.2 rule-6
/// check; `RulesetSidePath::new` debug-asserts the ordering).
///
/// Universe membership is a boot-time fact: a symbol that later
/// loses its feed still validates — the row just never triggers
/// (§4.3, mirroring how every other consumer treats SymbolMap).
/// Called ONCE in the bin, after 8e discovery gates the venue
/// tables and before any thread spawns; boot-time allocation.
pub fn build_ai_universe(
    polymarket_syms: &[SymbolId],
    binance_syms: &[SymbolId],
    okx: Option<&ingress_okx::OkxSymbolTable>,
    deribit: Option<&ingress_deribit::DeribitSymbolTable>,
    hl: Option<&ingress_hyperliquid::HlCoinTable>,
) -> Arc<[u32]> {
    let mut v: Vec<u32> = Vec::with_capacity(
        polymarket_syms.len()
            + binance_syms.len()
            + okx.map_or(0, |t| t.len())
            + deribit.map_or(0, |t| t.len())
            + hl.map_or(0, |t| t.len()),
    );
    v.extend_from_slice(polymarket_syms);
    v.extend_from_slice(binance_syms);
    if let Some(t) = okx {
        let mut i = 0usize;
        while let Some((_, sym, _)) = t.get(i) {
            v.push(sym);
            i += 1;
        }
    }
    if let Some(t) = deribit {
        let mut i = 0usize;
        while let Some((_, sym)) = t.get(i) {
            v.push(sym);
            i += 1;
        }
    }
    if let Some(t) = hl {
        let mut i = 0usize;
        while let Some((_, sym)) = t.get(i) {
            v.push(sym);
            i += 1;
        }
    }
    v.sort_unstable();
    v.dedup();
    Arc::from(v)
}

/// Spawn the AI-command ingress thread (Phase 8f §4). Opens the
/// `ai-cmds.pmlr` capture **before** spawning — capture-open failure
/// is a fatal boot error, matching every venue wrapper. The thread
/// binds `sock_path`, serves the single `claude-worker` client, and
/// rebinds after transport-fatal errors until shutdown.
///
/// **Core pinning:** core 4 per the §9 core map (freed by the 8f
/// item-16 RSS removal).
///
/// `key` is moved into the thread and never logged.
///
/// 8g item 4: `table_producer` is the push half of the §6 ruleset
/// table-handoff ring and `universe` the §4.3 boot snapshot from
/// [`build_ai_universe`] — both feed the [`RulesetSidePath`].
#[allow(clippy::too_many_arguments)] // one parameter per boot-wired resource
pub fn spawn_ai(
    sock_path: PathBuf,
    ruleset_dir: PathBuf,
    key: [u8; 32],
    producer: Producer<AiCmd, AI_RING_SIZE>,
    table_producer: Producer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
    universe: Arc<[u32]>,
    descriptors: Arc<ingress_ai::DescriptorTable>,
    status: Arc<AiIngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    capture_metrics: CaptureMetrics,
) -> io::Result<JoinHandle<()>> {
    let mut capture = AiCmdCapture::open(run_dir, epoch_ns)?;
    let builder = thread::Builder::new().name("ingress-ai".to_string());
    Ok(spawn_or_die(builder, "ingress-ai", move || {
        log_pin_outcome("ai", core_id);
        let cfg = AiIngressCfg { sock_path };
        let mut producer = producer;
        // Ruleset side-path (§4.4 step 8): Stage/Commit kinds run the
        // full 8g §4.2 validator against `AI_RULESET_DIR/
        // <hash128-hex>.json` (rule 1 full-SHA-256 recompute, rules
        // 2–8 byte scan into the preallocated scratch table) and are
        // recorded as staged/committed state + the
        // `engine_ai_ruleset_*_total` counters. Control-plane only —
        // the frame pump stays allocation-free.
        //
        // 8g item 4: the §4.3 boot-universe snapshot is the REAL
        // sorted discovery-derived set (`build_ai_universe`, built in
        // the bin before threads spawn), and a validated Stage hands
        // its table to the engine through the §6 ring — `try_push` of
        // the scratch (documented 16 KiB copy #1, operator cadence);
        // push-full ⇒ reject, counted (`table_push_fail`). The
        // consumer half parks in the bin until item 7 wires the
        // engine drain.
        let mut side_path =
            RulesetSidePath::new(
                ruleset_dir,
                Arc::clone(&status),
                universe,
                descriptors,
                table_producer,
            );
        let mut seam = |c: &AiCmd| side_path.on_cmd(c);
        while !shutdown_requested() {
            match ingress_ai::run(
                &cfg,
                &key,
                &mut producer,
                &mut capture,
                &status,
                &mut seam,
                &SHUTDOWN,
            ) {
                // Stop flag flipped — the while condition exits.
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(error = ?e, "ingress-ai: run loop error; rebinding");
                    mirror_ai_capture_metrics(&capture_metrics, &capture);
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
        mirror_ai_capture_metrics(&capture_metrics, &capture);
        tracing::info!("ingress-ai: thread exiting");
    }))
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
    /// WS9: tap config for the Bybit ingress.
    pub bybit: TapCfg,
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
    let enabled_cfg = TapCfg {
        mode: tap_mode,
        budget_bytes,
    };

    let mut cfg = RawTapConfig {
        pm: TapCfg::off(),
        bn: TapCfg::off(),
        okx: TapCfg::off(),
        rpc: TapCfg::off(),
        deribit: TapCfg::off(),
        hl: TapCfg::off(),
        bybit: TapCfg::off(),
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
        cfg.bybit = enabled_cfg;
        return Ok(cfg);
    }

    let mut seen: [&str; 7] = [""; 7];
    for (n_seen, item) in spec.split(',').enumerate() {
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
        match label {
            "pm" => cfg.pm = enabled_cfg,
            "bn" => cfg.bn = enabled_cfg,
            "okx" => cfg.okx = enabled_cfg,
            "rpc" => cfg.rpc = enabled_cfg,
            "deribit" => cfg.deribit = enabled_cfg,
            "hl" => cfg.hl = enabled_cfg,
            "bybit" => cfg.bybit = enabled_cfg,
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
    /// WS10-A: venue events observed (all event lanes combined).
    pub venue_events: u64,
    /// WS10-B: depth snapshots observed (both depth lanes combined).
    pub depth_snaps: u64,
    /// VM2 V2: options records observed (all opt lanes combined).
    pub opt_records: u64,
}

impl DrainCounters {
    /// Add another reading.
    #[inline]
    pub fn add(&mut self, other: &Self) {
        self.polymarket_ticks += other.polymarket_ticks;
        self.binance_ticks += other.binance_ticks;
        self.other_venue_ticks += other.other_venue_ticks;
        self.rpc_signals += other.rpc_signals;
        self.venue_events += other.venue_events;
        self.depth_snaps += other.depth_snaps;
        self.opt_records += other.opt_records;
    }
}

/// Consumer-side handles passed to the drain loop / engine. Created
/// from `Ring::split()`; the producer ends went to ingress threads.
pub struct Consumers {
    /// Tick-lane consumers, indexed by `VenueId as usize` (§3.3).
    pub tick_lanes: [Consumer<Tick, TICK_RING_SIZE>; NUM_TICK_LANES],
    /// WS10-A: venue-event lane consumers, tick-lane indexing. Lanes
    /// without a producing venue read empty forever (§3.3).
    pub event_lanes: [Consumer<ChannelEvent, EVENT_RING_SIZE>; engine::NUM_EVENT_LANES],
    /// WS10-B: depth-lane consumers (`engine::depth_lane_of` order).
    pub depth_lanes: [Consumer<DepthTopK, DEPTH_RING_SIZE>; engine::NUM_DEPTH_LANES],
    /// VM2 V2: options-summary lane consumers (`engine::opt_lane_of`
    /// order).
    pub opt_lanes: [Consumer<OptSummary, OPT_RING_SIZE>; engine::NUM_OPT_LANES],
    /// RPC signal consumer.
    pub rpc_signal: Consumer<Signal, SIGNAL_RING_SIZE>,
    /// Fill-lane consumers (`engine::fill_lane_of` order). Producers
    /// arrive with the venue dispatchers in Phase 8j; paper-mode
    /// fills flow through the engine's dispatcher pump (D3).
    pub fill_lanes: [Consumer<Fill, FILL_RING_SIZE>; NUM_FILL_LANES],
    /// AI command lane consumer (Phase 8f). Reads empty forever when
    /// `ingress-ai` is not spawned (producer dropped).
    pub ai_cmds: Consumer<AiCmd, AI_RING_SIZE>,
    /// Shared AI-ingress status slot. Rides in `Consumers` because it
    /// must reach `Engine::new` alongside the AI lane — the engine
    /// drain site is the designated writer of its `expired_total`
    /// field; the metrics mirror reads the rest through
    /// `Engine::ai_status()`.
    pub ai_status: Arc<AiIngressStatus>,
    /// Ruleset table-handoff lane (8g §6, item 7). The engine pops it
    /// immediately before the AI-cmd drain each iteration and hands
    /// slots to `Strategy::on_ruleset_table` (→ the set's vm member,
    /// documented copy #2). Reads empty forever when `ingress-ai` is
    /// not spawned (producer dropped — §3.3 unspawned shape); on
    /// non-set strategy paths the pops land on the trait's default
    /// no-op, mirroring how `on_ai` behaves on bare strategies.
    pub ruleset_tables: Consumer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
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
        // WS10-A: keep the event lanes drained in capture-only mode
        // too — the events are already in capture; an undrained lane
        // would fill within minutes and pollute event_ring_drops.
        let mut lane = 0;
        while lane < engine::NUM_EVENT_LANES {
            for _ in 0..DRAIN_BATCH {
                if cons.event_lanes[lane].try_pop().is_some() {
                    period.venue_events += 1;
                } else {
                    break;
                }
            }
            lane += 1;
        }
        // WS10-B: same for the depth lanes (snapshots are already in
        // capture).
        let mut lane = 0;
        while lane < engine::NUM_DEPTH_LANES {
            for _ in 0..DRAIN_BATCH {
                if cons.depth_lanes[lane].try_pop().is_some() {
                    period.depth_snaps += 1;
                } else {
                    break;
                }
            }
            lane += 1;
        }
        // VM2 V2: same for the opt lanes (records are already in
        // capture).
        let mut lane = 0;
        while lane < engine::NUM_OPT_LANES {
            for _ in 0..DRAIN_BATCH {
                if cons.opt_lanes[lane].try_pop().is_some() {
                    period.opt_records += 1;
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

        let now = now_ns();
        if now >= next_report {
            total.add(&period);
            tracing::info!(
                pm_ticks = period.polymarket_ticks,
                bn_ticks = period.binance_ticks,
                other_ticks = period.other_venue_ticks,
                rpc_sigs = period.rpc_signals,
                venue_events = period.venue_events,
                depth_snaps = period.depth_snaps,
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

/// Configure a latency-arb instance from [`EngineConfig`] (threshold,
/// qty, cooldown, pairs). Shared by the standalone path and the
/// Phase-8f [`engine_loop_set_full`] builder — do not duplicate.
fn configure_latency_arb<const N: usize>(
    strat: &mut LatencyArb<N>,
    cfg: &EngineConfig,
) -> Result<(), &'static str> {
    strat.set_threshold(cfg.threshold_1e6);
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);
    for p in &cfg.pairs {
        if let Err(e) = strat.add_pair(p.polymarket, p.binance) {
            tracing::error!(error = ?e, pm = p.polymarket, bn = p.binance, "add_pair failed");
            return Err("engine_loop: add_pair rejected");
        }
    }
    Ok(())
}

/// Load + configure an EV instance (artifact table, params, symbol
/// registration). Pairs are interpreted as Polymarket symbols only —
/// the Binance leg is ignored. Shared by the standalone path and the
/// Phase-8f set builder.
fn configure_ev<const N: usize>(
    strat: &mut strategy_ev::EvStrategy<N>,
    cfg: &EngineConfig,
    artifact_path: &std::path::Path,
) -> Result<(), &'static str> {
    let (table, skipped) = match research_artifacts::ArtifactTable::<N>::load_ndjson(artifact_path)
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = ?e, path = %artifact_path.display(), "ev: load_ndjson failed");
            return Err("engine_loop_ev: artifact load failed");
        }
    };
    tracing::info!(
        loaded = table.len(),
        skipped,
        path = %artifact_path.display(),
        "ev: loaded artifact table"
    );
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
            return Err("engine_loop_ev: register rejected");
        }
    }
    Ok(())
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
    let mut strat: strategy_ev::EvStrategy<STRATEGY_SLOTS> = strategy_ev::EvStrategy::new();
    if let Err(reason) = configure_ev(&mut strat, &cfg, artifact_path) {
        return EngineLoopResult::Failed(reason);
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
    if let Err(reason) = configure_cross_arb(&mut strat, &cfg, groups) {
        return EngineLoopResult::Failed(reason);
    }
    run_engine_loop(cons, disp, strat, obs)
}

/// Configure a cross-arb instance (params + groups). Shared by the
/// standalone path and the Phase-8f set builder.
fn configure_cross_arb<const N: usize, const M: usize>(
    strat: &mut strategy_cross_arb::CrossArb<N, M>,
    cfg: &EngineConfig,
    groups: &[&[core_types::SymbolId]],
) -> Result<(), &'static str> {
    strat.set_threshold(cfg.threshold_1e6);
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);
    for g in groups {
        if let Err(e) = strat.register_group(g) {
            tracing::error!(error = ?e, "cross-arb: register_group failed");
            return Err("engine_loop_cross_arb: register rejected");
        }
    }
    tracing::info!(groups = groups.len(), "cross-arb: registered groups");
    Ok(())
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
    let mut strat: strategy_rule_tree::RuleTree<8> = strategy_rule_tree::RuleTree::new();
    if let Err(reason) = configure_rule_tree(&mut strat, &cfg, rules_path, sym_for_rule) {
        return EngineLoopResult::Failed(reason);
    }
    run_engine_loop(cons, disp, strat, obs)
}

/// Load + configure a rule-tree instance (rules file, params, symbol
/// mapping). Shared by the standalone path and the Phase-8f set
/// builder.
fn configure_rule_tree<const N: usize>(
    strat: &mut strategy_rule_tree::RuleTree<N>,
    cfg: &EngineConfig,
    rules_path: &std::path::Path,
    sym_for_rule: &[(core_types::SymbolId, [u8; 16], u8)],
) -> Result<(), &'static str> {
    let (rules, skipped) = match research_artifacts::RulesTable::<N>::load_json(rules_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = ?e, path = %rules_path.display(), "rule-tree: load failed");
            return Err("engine_loop_rule_tree: rules load failed");
        }
    };
    tracing::info!(
        loaded = rules.len(),
        skipped,
        path = %rules_path.display(),
        "rule-tree: loaded rules"
    );
    if rules.is_empty() {
        return Err("engine_loop_rule_tree: rules file is empty");
    }
    if sym_for_rule.is_empty() {
        return Err("engine_loop_rule_tree: no symbol mapping provided");
    }
    strat.set_qty(core_types::Qty::from_raw(cfg.qty_1e6));
    strat.set_cooldown_ns(cfg.cooldown_ns);

    for (mapping_idx, r) in rules.slice().iter().enumerate() {
        if mapping_idx >= sym_for_rule.len() {
            break;
        }
        let (sym, kw, kw_len) = sym_for_rule[mapping_idx];
        if let Err(e) = strat.add_rule(*r, sym, &kw[..kw_len as usize]) {
            tracing::error!(error = ?e, "rule-tree: add_rule failed");
            return Err("engine_loop_rule_tree: add_rule rejected");
        }
    }
    Ok(())
}

/// Phase 8f item 7: run the composed [`strategy_set::StrategySet`].
/// The initial mask enables exactly the members whose configuration
/// was provided — latency-arb always (pairs are mandatory), ev with
/// `--artifacts-path`, cross-arb with `--groups`, rule-tree with
/// `--rules-path`, **ai-exec and vm unconditionally** (neither has
/// boot config: ai-exec's universe arrives over UDS at runtime and
/// its `on_start` validates parameters only; vm boots inert until a
/// ruleset table is staged + committed — 8g §7.3, normal, not an
/// error). `requested_mask` (from
/// [`strategy_set::mask_for_name`]) is intersected with that
/// configured mask, so `--strategy all` means "all built members the
/// given flags can boot" — every enabled member still validates
/// fail-fast in `on_start`. An AI `EnableStrategy` may later switch
/// on a member that booted unconfigured; it stays inert (registers
/// nothing, so it never fires) — documented in `strategy-set`.
#[allow(clippy::too_many_arguments)]
pub fn engine_loop_set_full<D: OrderDispatch>(
    cons: Consumers,
    cfg: EngineConfig,
    disp: D,
    obs: Observability,
    requested_mask: u8,
    ev_artifacts: Option<&std::path::Path>,
    cross_groups: &[&[core_types::SymbolId]],
    rules: Option<(&std::path::Path, &[(core_types::SymbolId, [u8; 16], u8)])>,
) -> EngineLoopResult {
    if cfg.pairs.is_empty() {
        return EngineLoopResult::Failed("engine_loop: no symbol pairs configured");
    }
    let mut configured =
        strategy_set::BIT_LATENCY_ARB | strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM;
    if ev_artifacts.is_some() {
        configured |= strategy_set::BIT_EV;
    }
    if !cross_groups.is_empty() {
        configured |= strategy_set::BIT_CROSS_ARB;
    }
    if rules.is_some() {
        configured |= strategy_set::BIT_RULE_TREE;
    }
    let mask = requested_mask & configured;
    if mask == 0 {
        return EngineLoopResult::Failed("engine_loop_set: no requested member is configured");
    }

    let mut set = strategy_set::StrategySet::new(mask);
    if let Err(reason) = configure_latency_arb(set.latency_arb_mut(), &cfg) {
        return EngineLoopResult::Failed(reason);
    }
    if let Some(path) = ev_artifacts {
        if let Err(reason) = configure_ev(set.ev_mut(), &cfg, path) {
            return EngineLoopResult::Failed(reason);
        }
    }
    if !cross_groups.is_empty() {
        if let Err(reason) = configure_cross_arb(set.cross_arb_mut(), &cfg, cross_groups) {
            return EngineLoopResult::Failed(reason);
        }
    }
    if let Some((rules_path, sym_for_rule)) = rules {
        if let Err(reason) =
            configure_rule_tree(set.rule_tree_mut(), &cfg, rules_path, sym_for_rule)
        {
            return EngineLoopResult::Failed(reason);
        }
    }
    tracing::info!(
        mask,
        latency_arb = mask & strategy_set::BIT_LATENCY_ARB != 0,
        ev = mask & strategy_set::BIT_EV != 0,
        cross_arb = mask & strategy_set::BIT_CROSS_ARB != 0,
        rule_tree = mask & strategy_set::BIT_RULE_TREE != 0,
        ai_exec = mask & strategy_set::BIT_AI_EXEC != 0,
        vm = mask & strategy_set::BIT_VM != 0,
        "strategy-set: composed"
    );
    run_engine_loop(cons, disp, set, obs)
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
            let strategy_set = reg
                .register_gauge("engine_strategy_set_active")
                .map_err(|_| "register engine_strategy_set_active")?;
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
            let ingress_bybit_state = reg
                .register_gauge("engine_ingress_bybit_state")
                .map_err(|_| "register engine_ingress_bybit_state")?;
            let ingress_rpc_state = reg
                .register_gauge("engine_ingress_rpc_state")
                .map_err(|_| "register engine_ingress_rpc_state")?;
            // T1(c) (outage 2026-08-27 §5.5): per-venue last-TICK age
            // in seconds. `*_state` lies on a 1 Hz-churning lane (a
            // sampler nearly always catches it mid-cycle at Up) and
            // `last_activity` advances on the venue's own rejection
            // bytes — only "when did MARKET DATA last arrive" names a
            // dead lane. -1 = no tick since boot. Order matches the
            // derivation loop: pm, bn, okx, deribit, hl, bybit, rpc.
            let ingress_last_tick_age: [core_metrics::GaugeId; 7] = [
                reg.register_gauge("engine_ingress_polymarket_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_polymarket_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_binance_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_binance_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_okx_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_okx_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_deribit_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_deribit_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_hyperliquid_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_hyperliquid_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_bybit_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_bybit_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_rpc_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_rpc_last_tick_age_seconds")?,
            ];
            // T1(c) / F12: age of the newest launchd restart-lane
            // slot stamp — the restart lane failing silently for 28 h
            // is what let the Aug-28 midnight turn lapse. -1 = no
            // stamps readable.
            let restart_stamp_age = reg
                .register_gauge("engine_restart_stamp_age_seconds")
                .map_err(|_| "register engine_restart_stamp_age_seconds")?;
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
                let name =
                    std::str::from_utf8(&name_buf[..n]).map_err(|_| "tick_age_ns_b name utf8")?;
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
            let ingress_bybit = register_ingress_counters(&mut reg, "bybit")?;
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
            let capture_bybit = register_capture_gauges(&mut reg, "bybit")?;
            let capture_rpc = register_capture_gauges(&mut reg, "rpc")?;

            // §6.1 boot-discovery coverage gauges — PM/OKX/Deribit/HL
            // + Binance since M1 (exchangeInfo audit); RPC alone has
            // no REST discovery (boot_discovery module docs).
            let coverage_pm = register_coverage_gauge(&mut reg, "pm")?;
            let coverage_okx = register_coverage_gauge(&mut reg, "okx")?;
            let coverage_deribit = register_coverage_gauge(&mut reg, "deribit")?;
            let coverage_hyperliquid = register_coverage_gauge(&mut reg, "hl")?;
            let coverage_binance = register_coverage_gauge(&mut reg, "bn")?;
            let coverage_bybit = register_coverage_gauge(&mut reg, "bybit")?;
            // M2.1/M2.2: how many capped-chain option instruments
            // this boot selected + subscribed (0 = options lane off).
            let deribit_options_selected = reg
                .register_gauge("engine_ingress_deribit_options_selected")
                .map_err(|_| "register engine_ingress_deribit_options_selected")?;
            let okx_options_selected = reg
                .register_gauge("engine_ingress_okx_options_selected")
                .map_err(|_| "register engine_ingress_okx_options_selected")?;
            let binance_options_selected = reg
                .register_gauge("engine_ingress_binance_options_selected")
                .map_err(|_| "register engine_ingress_binance_options_selected")?;

            // Phase-8f AI family: §4.4 counters + heartbeat-age gauge
            // (mirrored centrally from the shared status slot), the
            // AI thread's capture pair (mirrored from inside the
            // spawn wrapper, venue pattern), and the engine-thread
            // fills-capture pair (mirrored centrally).
            let ingress_ai = register_ai_counters(&mut reg)?;
            let capture_ai = register_capture_gauges(&mut reg, "ai")?;
            // Phase-8g §9: the enable-mask gauge + the vm family
            // (mirrored centrally via the StrategyCounters defaults).
            let strategy_enabled_mask = reg
                .register_gauge("engine_strategy_enabled_mask")
                .map_err(|_| "register engine_strategy_enabled_mask")?;
            let vm = register_vm_metrics(&mut reg)?;
            let fills_capture = {
                let io_errors = reg
                    .register_gauge("engine_fills_capture_io_errors")
                    .map_err(|_| "register engine_fills_capture_io_errors")?;
                let records = reg
                    .register_gauge("engine_fills_capture_records")
                    .map_err(|_| "register engine_fills_capture_records")?;
                CaptureGaugeIds { io_errors, records }
            };
            let orders_capture = {
                let io_errors = reg
                    .register_gauge("engine_orders_capture_io_errors")
                    .map_err(|_| "register engine_orders_capture_io_errors")?;
                let records = reg
                    .register_gauge("engine_orders_capture_records")
                    .map_err(|_| "register engine_orders_capture_records")?;
                CaptureGaugeIds { io_errors, records }
            };

            out.metrics = Some(Arc::new(reg));
            out.counter_ids = Some(EngineCounters {
                ticks,
                signals,
                orders_emitted,
                orders_dropped,
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
                strategy_set,
                ingress_polymarket_state,
                ingress_binance_state,
                ingress_okx_state,
                ingress_deribit_state,
                ingress_hyperliquid_state,
                ingress_bybit_state,
                ingress_rpc_state,
                ingress_last_tick_age,
                restart_stamp_age,
                max_tick_age_ns,
                tick_age_ns_per_bucket,
                ingress_polymarket,
                ingress_binance,
                ingress_okx,
                ingress_deribit,
                ingress_hyperliquid,
                ingress_bybit,
                ingress_rpc,
                capture_pm,
                capture_bn,
                capture_okx,
                capture_deribit,
                capture_hyperliquid,
                capture_bybit,
                capture_rpc,
                coverage_pm,
                coverage_okx,
                coverage_deribit,
                coverage_hyperliquid,
                coverage_binance,
                coverage_bybit,
                deribit_options_selected,
                okx_options_selected,
                binance_options_selected,
                ingress_ai,
                capture_ai,
                fills_capture,
                orders_capture,
                strategy_enabled_mask,
                vm,
            });
        }
        if enable_tui {
            out.snapshot = Some(Arc::new(tui::SnapshotCell::new()));
        }
        Ok(out)
    }
}

/// VT2: per-venue staleness thresholds (ms), indexed by the `VenueId`
/// byte — the venue defaults (`VenueId::default_stale_after_ms`,
/// docs/venue-time-capture-plan.md §2 doctrine 4) overridden by
/// repeatable `--stale-after-ms <venue>:<ms>` specs (labels as the
/// harness flags: `pm`/`bn`/`okx`/`deribit`/`hl`/`bybit`). A zero
/// disables the judgement for that venue (nothing is ever stale).
pub fn parse_stale_after_ms(specs: &[String]) -> Result<[u32; 7], String> {
    let mut table = [0u32; 7];
    let mut i = 0;
    while i < table.len() {
        table[i] = VenueId::from_u8(i as u8).map_or(0, VenueId::default_stale_after_ms);
        i += 1;
    }
    for spec in specs {
        let (label, ms) = spec
            .split_once(':')
            .ok_or_else(|| format!("bad --stale-after-ms {spec:?}: want <venue>:<ms>"))?;
        let venue = crate::backtest::model_venue(label)
            .ok_or_else(|| format!("bad --stale-after-ms {spec:?}: unknown venue {label:?}"))?;
        let ms: u32 = ms
            .parse()
            .map_err(|_| format!("bad --stale-after-ms {spec:?}: unparseable ms"))?;
        table[venue] = ms;
    }
    Ok(table)
}

/// Register the per-ingress §6.4 counters for one ingress. Boot-only.
fn register_ingress_counters(
    reg: &mut core_metrics::MetricsRegistry,
    venue: &str,
) -> Result<IngressCounterIds, &'static str> {
    let mut one = |metric: &str| -> Result<core_metrics::CounterId, &'static str> {
        let name = format!("engine_ingress_{venue}_{metric}_total");
        reg.register_counter(&name)
            .map_err(|_| "register ingress counter")
    };
    let ids = IngressCounterIds {
        msgs: one("msgs")?,
        bytes: one("bytes")?,
        parse_errors: one("parse_errors")?,
        gaps: one("gaps")?,
        resubscribes: one("resubscribes")?,
        reconnects: one("reconnects")?,
        ring_drops: one("ring_drops")?,
        ticks: one("ticks")?,
        sub_drops: one("sub_drops")?,
        event_ring_drops: one("event_ring_drops")?,
        depth_ring_drops: one("depth_ring_drops")?,
        stale_ticks: one("stale_ticks")?,
        feed_delay_ema_ms: reg
            .register_gauge(&format!("engine_ingress_{venue}_feed_delay_ema_ms"))
            .map_err(|_| "register ingress gauge")?,
    };
    Ok(ids)
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
    /// Phase-8f engine-thread fills capture (`engine-fills.pmlr`),
    /// opened by the bin inside the per-run capture directory and
    /// **taken** by the engine loop at boot (`Option::take` — the
    /// engine thread owns it from then on). `None` in tests and in
    /// tools that replay rather than run.
    pub fills_capture: Option<SlotCapture<Fill>>,
    /// M4.1: the engine-thread order-intent capture, same lifecycle
    /// as `fills_capture` (bin opens, engine loop takes ownership).
    /// `None` in tests and in tools that replay rather than run.
    pub orders_capture: Option<SlotCapture<Order>>,
}

impl Observability {
    /// Attach a [`LatencyDump`] config. Boot-only; called from the
    /// cli after [`Observability::build`]. Returns `self` so it can
    /// be chained.
    pub fn with_latency_dump(mut self, dump: Option<LatencyDump>) -> Self {
        self.latency_dump = dump;
        self
    }

    /// Attach the engine-thread fills capture (Phase 8f item 6).
    /// Boot-only; called from the bin after the per-run capture
    /// directory exists.
    pub fn with_fills_capture(mut self, cap: SlotCapture<Fill>) -> Self {
        self.fills_capture = Some(cap);
        self
    }

    /// Attach the engine-thread order-intent capture (M4.1).
    /// Boot-only; called from the bin after the per-run capture
    /// directory exists.
    pub fn with_orders_capture(mut self, cap: SlotCapture<Order>) -> Self {
        self.orders_capture = Some(cap);
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
    /// Active-strategy indicator — the Phase-8f composed set.
    pub strategy_set: core_metrics::GaugeId,
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
    /// WS9: per-ingress state gauge, Bybit v5 public WS.
    pub ingress_bybit_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Polygon JSON-RPC.
    pub ingress_rpc_state: core_metrics::GaugeId,
    /// T1(c): per-venue last-tick-age gauges in seconds
    /// (`engine_ingress_<venue>_last_tick_age_seconds`; -1 = no tick
    /// since boot). Order: pm, bn, okx, deribit, hl, bybit, rpc.
    pub ingress_last_tick_age: [core_metrics::GaugeId; 7],
    /// T1(c)/F12: newest restart-lane slot-stamp age in seconds
    /// (`engine_restart_stamp_age_seconds`; -1 = unreadable).
    pub restart_stamp_age: core_metrics::GaugeId,
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
    /// WS9: §6.4 loss-accounting counters, Bybit thread.
    pub ingress_bybit: IngressCounterIds,
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
    /// WS9: §6.5 capture-health gauges, Bybit thread.
    pub capture_bybit: CaptureGaugeIds,
    /// §6.5 capture-health gauges, RPC thread.
    pub capture_rpc: CaptureGaugeIds,
    /// §6.1 boot-discovery coverage gauge, Polymarket (always runs).
    pub coverage_pm: GaugeId,
    /// §6.1 boot-discovery coverage gauge, OKX (0 when unconfigured).
    pub coverage_okx: GaugeId,
    /// §6.1 boot-discovery coverage gauge, Deribit (0 when
    /// unconfigured).
    pub coverage_deribit: GaugeId,
    /// M2.1: selected capped-chain option instrument count
    /// (`engine_ingress_deribit_options_selected`; 0 = lane off).
    pub deribit_options_selected: GaugeId,
    /// M2.2: same for OKX (`engine_ingress_okx_options_selected`).
    pub okx_options_selected: GaugeId,
    /// M2.4: same for the Binance eapi lane
    /// (`engine_ingress_binance_options_selected`).
    pub binance_options_selected: GaugeId,
    /// §6.1 boot-discovery coverage gauge, Hyperliquid (0 when
    /// unconfigured).
    pub coverage_hyperliquid: GaugeId,
    /// M1 boot-discovery coverage gauge, Binance exchangeInfo audit
    /// (0 when skipped — legacy flag boots).
    pub coverage_binance: GaugeId,
    /// WS9: boot-discovery coverage gauge, Bybit instruments-info
    /// audit (0 when the `[bybit]` section is empty).
    pub coverage_bybit: GaugeId,
    /// Phase-8f AI ingress family (`engine_ingress_ai_*` + the engine
    /// drain-site counter + heartbeat-age gauge).
    pub ingress_ai: AiIngressCounterIds,
    /// Phase-8f capture-health gauges, AI ingress thread
    /// (`engine_ingress_ai_capture_{io_errors,records}`).
    pub capture_ai: CaptureGaugeIds,
    /// Phase-8f capture-health gauges, engine-thread fills capture
    /// (`engine_fills_capture_{io_errors,records}`). Mirrored
    /// centrally from the engine loop (unlike the per-thread venue
    /// pairs — the engine owns this capture).
    pub fills_capture: CaptureGaugeIds,
    /// M4.1 capture-health gauges, engine-thread order-intent capture
    /// (`engine_orders_capture_{io_errors,records}`). Mirrored
    /// centrally from the engine loop like the fills pair.
    pub orders_capture: CaptureGaugeIds,
    /// Phase-8g §9: the set's live enable mask
    /// (`engine_strategy_enabled_mask` — the G0 demo finding: the
    /// flip was only inferable from order-flow deltas). Read via the
    /// `StrategyCounters` default route; 0 on bare-strategy boots.
    pub strategy_enabled_mask: GaugeId,
    /// Phase-8g §9 vm-member family (`engine_vm_*`), mirrored
    /// centrally on the 5 s cadence.
    pub vm: VmMetricIds,
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
    /// Parsed market-data rows (T1(b): control frames excluded —
    /// `engine_ingress_<venue>_ticks_total`).
    pub ticks: core_metrics::CounterId,
    /// WS2: non-fatal subscribe drops
    /// (`engine_ingress_<venue>_sub_drops_total`).
    pub sub_drops: core_metrics::CounterId,
    /// WS10-A: venue-event lane pushes refused by a full ring
    /// (`engine_ingress_<venue>_event_ring_drops_total`).
    pub event_ring_drops: core_metrics::CounterId,
    /// WS10-B: depth-lane pushes refused by a full ring
    /// (`engine_ingress_<venue>_depth_ring_drops_total`).
    pub depth_ring_drops: core_metrics::CounterId,
    /// VT2: ticks the ingress judged stale
    /// (`engine_ingress_<venue>_stale_ticks_total`).
    pub stale_ticks: core_metrics::CounterId,
    /// VT2 gauge: the connection's smoothed feed delay
    /// (`engine_ingress_<venue>_feed_delay_ema_ms`).
    pub feed_delay_ema_ms: GaugeId,
}

/// Registry handles for the Phase-8f AI ingress family
/// (`engine_ingress_ai_*` — design §4.4) plus the engine drain-site
/// defense-in-depth counter. Counters are mirrored centrally from the
/// shared [`AiIngressStatus`] slot as deltas (unlike the venue §6.4
/// counters there is no per-thread snapshot problem — the slot is
/// `Arc`-shared); the heartbeat gauge is derived at mirror time.
#[derive(Copy, Clone, Debug)]
pub struct AiIngressCounterIds {
    /// `engine_ingress_ai_cmds_total`.
    pub cmds: core_metrics::CounterId,
    /// `engine_ingress_ai_hmac_fail_total`.
    pub hmac_fail: core_metrics::CounterId,
    /// `engine_ingress_ai_protocol_err_total`.
    pub protocol_err: core_metrics::CounterId,
    /// `engine_ingress_ai_malformed_total`.
    pub malformed: core_metrics::CounterId,
    /// `engine_ingress_ai_seq_gap_total`.
    pub seq_gap: core_metrics::CounterId,
    /// `engine_ingress_ai_seq_regress_total`.
    pub seq_regress: core_metrics::CounterId,
    /// `engine_ingress_ai_ring_drops_total`.
    pub ring_drops: core_metrics::CounterId,
    /// `engine_ingress_ai_expired_total` (writer: engine drain site).
    pub expired: core_metrics::CounterId,
    /// `engine_ingress_ai_rejected_conns_total`.
    pub rejected_conns: core_metrics::CounterId,
    /// `engine_ai_drain_malformed_total` — the engine drain-site shape
    /// re-check (defense in depth; distinct from the ingress-side
    /// `malformed_total`).
    pub drain_malformed: core_metrics::CounterId,
    /// `engine_ai_enable_refused_total` — `EnableStrategy` commands
    /// the strategy set refused (halted, or reserved/unknown slot).
    /// Mirrored generically via
    /// `StrategyCounters::ai_enable_refused` (0 for plain
    /// strategies).
    pub enable_refused: core_metrics::CounterId,
    /// `engine_ai_ruleset_staged_total` — item-14 side path: Stage
    /// frames whose artifact resolved and hash-verified.
    pub ruleset_staged: core_metrics::CounterId,
    /// `engine_ai_ruleset_committed_total` — Commits accepted for the
    /// currently staged hash (the 8f "state flag" observable).
    pub ruleset_committed: core_metrics::CounterId,
    /// `engine_ai_ruleset_rejected_total` — Stage/Commit refusals
    /// (artifact missing/unreadable, hash mismatch, unstaged commit).
    pub ruleset_rejected: core_metrics::CounterId,
    /// `engine_ai_table_push_fail_total` — 8g §9: Stages that passed
    /// the §4.2 validator but were REJECTED at the table-ring
    /// `try_push` (§5 push-full; isolates the cause inside
    /// `ruleset_rejected`). Unreachable at operator cadence against a
    /// running engine since item 7 — it counts engine-down staging.
    pub table_push_fail: core_metrics::CounterId,
    /// `engine_ingress_ai_last_heartbeat_age_ns` gauge. Derived as
    /// `now - last_heartbeat_ns` at mirror time; **-1 is the sentinel
    /// for "no heartbeat ever accepted"** (`last_heartbeat_ns == 0`) —
    /// a literal 0 would read as "heartbeat this instant", which is
    /// the opposite of the truth at boot.
    pub last_heartbeat_age_ns: GaugeId,
}

/// Last-mirrored cumulative [`AiIngressStatus`] values + the engine
/// drain-site counter — same delta bookkeeping as
/// [`IngressCountersSnapshot`].
#[derive(Copy, Clone, Debug, Default)]
struct AiCountersSnapshot {
    cmds: u64,
    hmac_fail: u64,
    protocol_err: u64,
    malformed: u64,
    seq_gap: u64,
    seq_regress: u64,
    ring_drops: u64,
    expired: u64,
    rejected_conns: u64,
    drain_malformed: u64,
    enable_refused: u64,
    ruleset_staged: u64,
    ruleset_committed: u64,
    ruleset_rejected: u64,
    table_push_fail: u64,
}

/// Mirror the AI status slot (+ the engine drain-site counter) into
/// registry counters as deltas, and derive the heartbeat-age gauge.
/// 5 s cadence — cold path.
fn mirror_ai_counters(
    reg: &core_metrics::MetricsRegistry,
    ids: &AiIngressCounterIds,
    st: &AiIngressStatus,
    engine_drain_malformed: u64,
    strategy_enable_refused: u64,
    now: u64,
    last: &mut AiCountersSnapshot,
) {
    let cur = AiCountersSnapshot {
        cmds: st.cmds(),
        hmac_fail: st.hmac_fail(),
        protocol_err: st.protocol_err(),
        malformed: st.malformed(),
        seq_gap: st.seq_gap(),
        seq_regress: st.seq_regress(),
        ring_drops: st.ring_drops(),
        expired: st.expired(),
        rejected_conns: st.rejected_conns(),
        drain_malformed: engine_drain_malformed,
        enable_refused: strategy_enable_refused,
        ruleset_staged: st.ruleset_staged(),
        ruleset_committed: st.ruleset_committed(),
        ruleset_rejected: st.ruleset_rejected(),
        table_push_fail: st.table_push_fail(),
    };
    reg.counter(ids.cmds)
        .inc(cur.cmds.saturating_sub(last.cmds));
    reg.counter(ids.hmac_fail)
        .inc(cur.hmac_fail.saturating_sub(last.hmac_fail));
    reg.counter(ids.protocol_err)
        .inc(cur.protocol_err.saturating_sub(last.protocol_err));
    reg.counter(ids.malformed)
        .inc(cur.malformed.saturating_sub(last.malformed));
    reg.counter(ids.seq_gap)
        .inc(cur.seq_gap.saturating_sub(last.seq_gap));
    reg.counter(ids.seq_regress)
        .inc(cur.seq_regress.saturating_sub(last.seq_regress));
    reg.counter(ids.ring_drops)
        .inc(cur.ring_drops.saturating_sub(last.ring_drops));
    reg.counter(ids.expired)
        .inc(cur.expired.saturating_sub(last.expired));
    reg.counter(ids.rejected_conns)
        .inc(cur.rejected_conns.saturating_sub(last.rejected_conns));
    reg.counter(ids.drain_malformed)
        .inc(cur.drain_malformed.saturating_sub(last.drain_malformed));
    reg.counter(ids.enable_refused)
        .inc(cur.enable_refused.saturating_sub(last.enable_refused));
    reg.counter(ids.ruleset_staged)
        .inc(cur.ruleset_staged.saturating_sub(last.ruleset_staged));
    reg.counter(ids.ruleset_committed)
        .inc(cur.ruleset_committed.saturating_sub(last.ruleset_committed));
    reg.counter(ids.ruleset_rejected)
        .inc(cur.ruleset_rejected.saturating_sub(last.ruleset_rejected));
    reg.counter(ids.table_push_fail)
        .inc(cur.table_push_fail.saturating_sub(last.table_push_fail));
    // Heartbeat age: -1 sentinel for "never" (see the field docs).
    let hb = st.last_heartbeat_ns();
    reg.gauge(ids.last_heartbeat_age_ns).set(if hb == 0 {
        -1
    } else {
        now.saturating_sub(hb) as i64
    });
    *last = cur;
}

/// Register the Phase-8f AI ingress metric family. Boot-only.
fn register_ai_counters(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<AiIngressCounterIds, &'static str> {
    let last_heartbeat_age_ns = reg
        .register_gauge("engine_ingress_ai_last_heartbeat_age_ns")
        .map_err(|_| "register engine_ingress_ai_last_heartbeat_age_ns")?;
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register ai counter")
    };
    Ok(AiIngressCounterIds {
        cmds: one("engine_ingress_ai_cmds_total")?,
        hmac_fail: one("engine_ingress_ai_hmac_fail_total")?,
        protocol_err: one("engine_ingress_ai_protocol_err_total")?,
        malformed: one("engine_ingress_ai_malformed_total")?,
        seq_gap: one("engine_ingress_ai_seq_gap_total")?,
        seq_regress: one("engine_ingress_ai_seq_regress_total")?,
        ring_drops: one("engine_ingress_ai_ring_drops_total")?,
        expired: one("engine_ingress_ai_expired_total")?,
        rejected_conns: one("engine_ingress_ai_rejected_conns_total")?,
        drain_malformed: one("engine_ai_drain_malformed_total")?,
        enable_refused: one("engine_ai_enable_refused_total")?,
        ruleset_staged: one("engine_ai_ruleset_staged_total")?,
        ruleset_committed: one("engine_ai_ruleset_committed_total")?,
        ruleset_rejected: one("engine_ai_ruleset_rejected_total")?,
        table_push_fail: one("engine_ai_table_push_fail_total")?,
        last_heartbeat_age_ns,
    })
}

// ---------------------------------------------------------------
// Phase 8g §9 — set/vm observability (5 s mirror, cold path)
// ---------------------------------------------------------------

/// Registry handles for the 8g §9 vm-member family (`engine_vm_*`).
/// Values cross the generic engine boundary via the
/// `StrategyCounters` default accessors (the `ai_enable_refused`
/// route — no set-specific plumbing in the loop); bare-strategy
/// boots mirror an all-zero family from the trait defaults.
#[derive(Copy, Clone, Debug)]
pub struct VmMetricIds {
    /// `engine_vm_rows_active` — active-table `len` (0 = inert).
    pub rows_active: GaugeId,
    /// `engine_vm_table_epoch` — active-table `epoch` (0 = none
    /// ever).
    pub table_epoch: GaugeId,
    /// `engine_vm_fires_total` — rows fired (pre-clamp).
    pub fires: core_metrics::CounterId,
    /// `engine_vm_orders_emitted_total` — via StrategyCounters
    /// kind="vm" (the vm member's own count, not the set aggregate).
    pub orders_emitted: core_metrics::CounterId,
    /// `engine_vm_orders_dropped_total` — kind="vm" value.
    pub orders_dropped: core_metrics::CounterId,
    /// `engine_vm_commit_dropped_total` — in-stream Commit with
    /// no/mismatched staged table (§6).
    pub commit_dropped: core_metrics::CounterId,
}

/// Last-mirrored cumulative vm-member counter values — same delta
/// bookkeeping as [`AiCountersSnapshot`] (registry counters get
/// monotonic deltas; the sources are cumulative strategy counters).
#[derive(Copy, Clone, Debug, Default)]
struct VmCountersSnapshot {
    fires: u64,
    orders_emitted: u64,
    orders_dropped: u64,
    commit_dropped: u64,
}

/// Mirror the §9 vm family: gauges as sets, counters as monotonic
/// deltas. Generic over the strategy — the trait defaults make this
/// a zero-mirror on bare-strategy boots. 5 s cadence — cold path.
fn mirror_vm_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &VmMetricIds,
    strat: &S,
    last: &mut VmCountersSnapshot,
) {
    reg.gauge(ids.rows_active)
        .set(strat.vm_rows_active() as i64);
    reg.gauge(ids.table_epoch)
        .set(strat.vm_table_epoch() as i64);
    let cur = VmCountersSnapshot {
        fires: strat.vm_fires(),
        orders_emitted: strat.vm_orders_emitted(),
        orders_dropped: strat.vm_orders_dropped(),
        commit_dropped: strat.vm_commit_dropped(),
    };
    reg.counter(ids.fires)
        .inc(cur.fires.saturating_sub(last.fires));
    reg.counter(ids.orders_emitted)
        .inc(cur.orders_emitted.saturating_sub(last.orders_emitted));
    reg.counter(ids.orders_dropped)
        .inc(cur.orders_dropped.saturating_sub(last.orders_dropped));
    reg.counter(ids.commit_dropped)
        .inc(cur.commit_dropped.saturating_sub(last.commit_dropped));
    *last = cur;
}

/// Register the §9 vm-member family. Boot-only.
fn register_vm_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<VmMetricIds, &'static str> {
    let rows_active = reg
        .register_gauge("engine_vm_rows_active")
        .map_err(|_| "register engine_vm_rows_active")?;
    let table_epoch = reg
        .register_gauge("engine_vm_table_epoch")
        .map_err(|_| "register engine_vm_table_epoch")?;
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register vm counter")
    };
    Ok(VmMetricIds {
        fires: one("engine_vm_fires_total")?,
        orders_emitted: one("engine_vm_orders_emitted_total")?,
        orders_dropped: one("engine_vm_orders_dropped_total")?,
        commit_dropped: one("engine_vm_commit_dropped_total")?,
        rows_active,
        table_epoch,
    })
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
    /// Mirrors the sum of `ticks_written()`, `events_written()`,
    /// `signals_written()`, `opt_summaries_written()`,
    /// `depths_written()` and `tap_records()` — total records staged
    /// since the capture was opened (monotonic snapshot, not a
    /// delta; opt/depth joined the sum at WS10-B).
    pub records: GaugeId,
}

/// Registry handle + gauge ids for one ingress thread's §6.5 capture
/// metrics. `None` when `--metrics` (and `--tui`, which implies it)
/// are both off, in which case the spawn wrapper skips the gauge
/// writes entirely.
pub type CaptureMetrics = Option<(Arc<MetricsRegistry>, CaptureGaugeIds)>;

/// §6.5 capture wrapped with its own gauge mirror on the 1 s flush
/// cadence (G1 remediation item 3, 2026-08-15). The first 6 h soak
/// showed `capture_records` gauges frozen at their last run-loop-exit
/// value (venues that never cycled reported 0 against growing pmlr
/// files) because mirroring only happened in the spawn wrappers after
/// `run(...)` returned. The run loops already call
/// `Capture::maybe_flush` once per poll; this wrapper piggybacks the
/// mirror onto that hook, rate-limited to the same 1 s the inner
/// flush uses, so gauges advance within ~2 s in steady state.
///
/// Zero-alloc on the hot path: the added work is one branch per poll
/// and, at most once a second, eight relaxed atomic stores
/// ([`MetricsRegistry`] gauges are preallocated slots). Monomorphized
/// like every other `Capture` impl — no `dyn`.
pub struct GaugedCapture {
    inner: PmlrCapture,
    metrics: CaptureMetrics,
    last_pub_ns: u64,
}

impl GaugedCapture {
    /// Wrap an opened capture with its (optional) gauge handles.
    pub fn new(inner: PmlrCapture, metrics: CaptureMetrics) -> Self {
        Self {
            inner,
            metrics,
            last_pub_ns: 0,
        }
    }

    /// Mirror immediately — the spawn wrappers call this after every
    /// `run(...)` return and once more before the thread exits, so
    /// final values land even if the last second went unmirrored.
    pub fn mirror_now(&self) {
        mirror_capture_metrics(&self.metrics, &self.inner);
    }

    /// Boot-time delegate of [`PmlrCapture::set_tap_venue_byte`] (the
    /// spawn wrappers stamp the tap header right after open).
    pub fn set_tap_venue_byte(
        &mut self,
        dir: &Path,
        venue_label: &str,
        venue: u8,
    ) -> io::Result<()> {
        self.inner.set_tap_venue_byte(dir, venue_label, venue)
    }
}

impl Capture for GaugedCapture {
    #[inline(always)]
    fn tick(&mut self, t: &Tick) {
        self.inner.tick(t);
    }

    #[inline(always)]
    fn event(&mut self, e: &ChannelEvent) {
        self.inner.event(e);
    }

    #[inline(always)]
    fn signal(&mut self, s: &Signal) {
        self.inner.signal(s);
    }

    #[inline(always)]
    fn opt_summary(&mut self, o: &core_types::OptSummary) {
        // M2.3: forward explicitly — the trait's default body is a
        // no-op and would silently swallow the channel.
        self.inner.opt_summary(o);
    }

    #[inline(always)]
    fn depth(&mut self, d: &DepthTopK) {
        // WS10-B: same trap as opt_summary — the WS13 live smoke
        // caught this wrapper swallowing every depth snapshot via
        // the trait's default no-op (2026-08-29: zero <venue>-depth
        // records while Book events flowed on both venues).
        self.inner.depth(d);
    }

    #[inline(always)]
    fn raw_frame(&mut self, ts_ns: NsTs, payload: &[u8]) {
        self.inner.raw_frame(ts_ns, payload);
    }

    #[inline(always)]
    fn parse_reject(&mut self, ts_ns: NsTs, payload: &[u8]) {
        self.inner.parse_reject(ts_ns, payload);
    }

    #[inline(always)]
    fn maybe_flush(&mut self, now_ns: NsTs) {
        self.inner.maybe_flush(now_ns);
        if now_ns.wrapping_sub(self.last_pub_ns) >= 1_000_000_000 {
            self.last_pub_ns = now_ns;
            mirror_capture_metrics(&self.metrics, &self.inner);
        }
    }
}

/// Mirror one ingress thread's §6.5 capture health into its two
/// registry gauges. Called from [`GaugedCapture`]: on the 1 s
/// `maybe_flush` cadence in steady state plus immediately after every
/// `run(...)` return / before thread exit (`mirror_now`) — see
/// [`CaptureMetrics`] docs for why this can't be done centrally.
fn mirror_capture_metrics(metrics: &CaptureMetrics, capture: &PmlrCapture) {
    if let Some((reg, ids)) = metrics.as_ref() {
        reg.gauge(ids.io_errors).set(capture.io_errors() as i64);
        let records = capture.ticks_written()
            + capture.events_written()
            + capture.signals_written()
            + capture.opt_summaries_written()
            + capture.depths_written()
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
/// for one boot-discovery venue (`pm`/`okx`/`deribit`/`hl`, plus
/// `bn` since the M1 exchangeInfo audit — RPC alone has no REST
/// discovery, see `boot_discovery` module docs). Boot-only.
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
    ticks: u64,
    sub_drops: u64,
    event_ring_drops: u64,
    depth_ring_drops: u64,
    stale_ticks: u64,
}

/// Mirror one ingress status slot into its registry counters as
/// deltas since the previous publish tick (the VT2 delay gauge is a
/// last-value copy). 5 s cadence — cold path.
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
        ticks: st.ticks_total(),
        sub_drops: st.sub_drops_total(),
        event_ring_drops: st.event_ring_drops_total(),
        depth_ring_drops: st.depth_ring_drops_total(),
        stale_ticks: st.stale_ticks_total(),
    };
    reg.counter(ids.msgs)
        .inc(cur.msgs.saturating_sub(last.msgs));
    reg.counter(ids.bytes)
        .inc(cur.bytes.saturating_sub(last.bytes));
    reg.counter(ids.parse_errors)
        .inc(cur.parse_errors.saturating_sub(last.parse_errors));
    reg.counter(ids.gaps)
        .inc(cur.gaps.saturating_sub(last.gaps));
    reg.counter(ids.resubscribes)
        .inc(cur.resubscribes.saturating_sub(last.resubscribes));
    reg.counter(ids.reconnects)
        .inc(cur.reconnects.saturating_sub(last.reconnects));
    reg.counter(ids.ring_drops)
        .inc(cur.ring_drops.saturating_sub(last.ring_drops));
    reg.counter(ids.ticks)
        .inc(cur.ticks.saturating_sub(last.ticks));
    reg.counter(ids.sub_drops)
        .inc(cur.sub_drops.saturating_sub(last.sub_drops));
    reg.counter(ids.event_ring_drops)
        .inc(cur.event_ring_drops.saturating_sub(last.event_ring_drops));
    reg.counter(ids.depth_ring_drops)
        .inc(cur.depth_ring_drops.saturating_sub(last.depth_ring_drops));
    reg.counter(ids.stale_ticks)
        .inc(cur.stale_ticks.saturating_sub(last.stale_ticks));
    reg.gauge(ids.feed_delay_ema_ms)
        .set(st.feed_delay_ema_ms() as i64);
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

fn run_engine_loop<S, D>(cons: Consumers, disp: D, strat: S, obs: Observability) -> EngineLoopResult
where
    S: strategy_core::Strategy,
    D: OrderDispatch,
{
    // Phase 8a lane engine: five tick lanes + one signal lane + four
    // fill lanes. The signal lane is bound to the RPC ring — the D2
    // disposition for Stage 1 (per §3.3).
    // Fills flow from the dispatcher pump (D3) until 8j wires the
    // per-venue fill-lane producers.
    let Consumers {
        tick_lanes,
        event_lanes,
        depth_lanes,
        opt_lanes,
        rpc_signal,
        fill_lanes,
        ai_cmds,
        ai_status,
        ruleset_tables,
    } = cons;
    let mut obs = obs;
    let mut eng = Engine::new(
        strat,
        disp,
        tick_lanes,
        event_lanes,
        depth_lanes,
        opt_lanes,
        rpc_signal,
        fill_lanes,
        ai_cmds,
        ai_status,
        ruleset_tables,
    );
    // Phase 8f: the fills capture is opened by the bin (per-run
    // capture directory) and rides in via Observability; the engine
    // thread owns it from here.
    if let Some(cap) = obs.fills_capture.take() {
        eng.set_fill_capture(cap);
    }
    // M4.1: the order-intent capture rides the same handoff.
    if let Some(cap) = obs.orders_capture.take() {
        eng.set_order_capture(cap);
    }
    if let Err(e) = eng.start() {
        tracing::error!(error = ?e, "engine on_start failed");
        return EngineLoopResult::Failed("engine_loop: on_start failed");
    }

    let mut next_report = now_ns() + REPORT_PERIOD_NS;
    let mut last_ticks = 0u64;
    let mut last_signals = 0u64;
    let mut last_orders = 0u64;
    // Last-mirrored snapshots for the §6.4 ingress counters
    // (pm, bn, okx, rpc, deribit, hyperliquid) so registry counters
    // get monotonic deltas. Append-only: existing indices are
    // load-bearing, new venues go at the end.
    let mut ingress_last = [IngressCountersSnapshot::default(); 7];
    // T1(c): last-tick-age derivation state per venue —
    // (ticks_total last seen, wall ns when it last advanced);
    // wall ns 0 = never ticked. Order pairs with
    // `ids.ingress_last_tick_age`: pm, bn, okx, deribit, hl, bybit,
    // rpc (NOT the ingress_last order — that array predates this and
    // its indices are load-bearing).
    let mut tick_age_track = [(0u64, 0u64); 7];
    // Phase-8f AI-family delta snapshot (same bookkeeping).
    let mut ai_last = AiCountersSnapshot::default();
    // Phase-8g §9 vm-family delta snapshot (same bookkeeping).
    let mut vm_last = VmCountersSnapshot::default();
    // Periodic HdrHistogram dump cadence. `next_dump_ns` is only
    // consulted when `obs.latency_dump.is_some()`.
    let mut next_dump_ns: u64 = match obs.latency_dump.as_ref() {
        Some(d) => now_ns().saturating_add(d.interval_ns),
        None => u64::MAX,
    };

    while !shutdown_requested() {
        eng.tick(DRAIN_BATCH);

        let now = now_ns();
        if now >= next_report {
            // Phase 8f: bound fills-capture staging staleness to one
            // report period even when no further fills arrive.
            // Independent of the metrics gate — capture durability is
            // not an observability option.
            eng.maybe_flush_fill_capture(now);
            eng.maybe_flush_order_capture(now);

            let ticks = eng.ticks_dispatched;
            let signals = eng.signals_dispatched;
            let orders = strategy_core::StrategyCounters::orders_emitted(eng.strategy());
            let dropped = strategy_core::StrategyCounters::orders_dropped(eng.strategy());
            tracing::info!(
                pm_bn_ticks = ticks - last_ticks,
                rpc_sigs = signals - last_signals,
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
                reg.gauge(ids.strategy_set)
                    .set(if kind == "set" { 1 } else { 0 });

                // Phase-8g §9: live enable mask (the G0 demo
                // finding) + the vm family. Both ride the
                // StrategyCounters-default route (UFCS — the set
                // overrides, bare strategies mirror zeros).
                reg.gauge(ids.strategy_enabled_mask)
                    .set(strategy_core::StrategyCounters::enabled_mask(eng.strategy()) as i64);
                mirror_vm_metrics(reg, &ids.vm, eng.strategy(), &mut vm_last);

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
                    reg.gauge(ids.ingress_bybit_state)
                        .set(ing.bybit.state() as i64);
                    reg.gauge(ids.ingress_rpc_state).set(ing.rpc.state() as i64);
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
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_bybit,
                        &ing.bybit,
                        &mut ingress_last[6],
                    );

                    // T1(c): derive per-venue last-tick age. A lane
                    // that stops moving data goes visibly stale here
                    // even while its ~1 Hz reconnect churn keeps the
                    // state gauge reading Up (outage 2026-08-27
                    // §5.5). -1 = no tick since boot.
                    let venue_ticks = [
                        ing.polymarket.ticks_total(),
                        ing.binance.ticks_total(),
                        ing.okx.ticks_total(),
                        ing.deribit.ticks_total(),
                        ing.hyperliquid.ticks_total(),
                        ing.bybit.ticks_total(),
                        ing.rpc.ticks_total(),
                    ];
                    let mut i = 0;
                    while i < 7 {
                        let (seen, _) = tick_age_track[i];
                        if venue_ticks[i] > seen {
                            tick_age_track[i] = (venue_ticks[i], now);
                        }
                        let (_, wall) = tick_age_track[i];
                        let age_s: i64 = if wall == 0 {
                            -1
                        } else {
                            (now.saturating_sub(wall) / 1_000_000_000) as i64
                        };
                        reg.gauge(ids.ingress_last_tick_age[i]).set(age_s);
                        i += 1;
                    }
                }

                // T1(c)/F12: restart-lane liveness — the newest slot
                // stamp's age (cold fs read on the 5 s cadence).
                reg.gauge(ids.restart_stamp_age)
                    .set(restart_stamp_age_secs());

                // Phase-8f AI family: §4.4 counter deltas from the
                // shared status slot (incl. the engine-written
                // `expired`), the drain-site re-check counter, and
                // the derived heartbeat-age gauge (-1 = never).
                mirror_ai_counters(
                    reg,
                    &ids.ingress_ai,
                    eng.ai_status(),
                    eng.ai_drain_malformed,
                    strategy_core::StrategyCounters::ai_enable_refused(eng.strategy()),
                    now,
                    &mut ai_last,
                );
                // Engine-thread fills-capture pair — mirrored
                // centrally (the engine owns this capture, unlike the
                // per-thread venue sinks).
                reg.gauge(ids.fills_capture.io_errors)
                    .set(eng.fill_capture_io_errors() as i64);
                reg.gauge(ids.fills_capture.records)
                    .set(eng.fill_capture_records() as i64);
                // M4.1 order-intent capture pair — same central mirror.
                reg.gauge(ids.orders_capture.io_errors)
                    .set(eng.order_capture_io_errors() as i64);
                reg.gauge(ids.orders_capture.records)
                    .set(eng.order_capture_records() as i64);

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
                // bit3 = rss (retired 8f — reserved, always 0),
                // bit4 = okx, bit5 = deribit, bit6 = hl (8e —
                // appended), bit7 = bybit (WS9 — the u8's last bit;
                // existing bits never renumber); bit set iff the
                // thread is Up.
                state.ingest_health = match obs.ingress.as_ref() {
                    Some(ing) => {
                        (u8::from(ing.polymarket.state() == IngressState::Up))
                            | (u8::from(ing.binance.state() == IngressState::Up) << 1)
                            | (u8::from(ing.rpc.state() == IngressState::Up) << 2)
                            | (u8::from(ing.okx.state() == IngressState::Up) << 4)
                            | (u8::from(ing.deribit.state() == IngressState::Up) << 5)
                            | (u8::from(ing.hyperliquid.state() == IngressState::Up) << 6)
                            | (u8::from(ing.bybit.state() == IngressState::Up) << 7)
                    }
                    None => 0,
                };
                cell.publish(state);
            }

            last_ticks = ticks;
            last_signals = signals;
            last_orders = orders;
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

/// T1(b) — the D8 intent, restored (outage 2026-08-27 §5.3): the
/// reconnect schedule resets only when the session actually MOVED
/// MARKET DATA (`ticks_total` advanced), or ended in a venue-quiet
/// idle/staleness trip (inherently rate-limited by the keepalive /
/// staleness budget, so it cannot hammer). A session that only
/// received its own subscribe rejection — the exact post-settlement
/// failure that reconnected at ~1 Hz for 16 h/day — keeps
/// escalating. One definition for all six venue loops.
#[inline]
fn should_reset_backoff(ticks_after: u64, ticks_before: u64, venue_quiet_trip: bool) -> bool {
    ticks_after > ticks_before || venue_quiet_trip
}

/// T1(c) (outage 2026-08-27 finding F12): age in seconds of the
/// NEWEST launchd restart-lane slot stamp
/// (`~/multivenue/state/last-restart-utc-*`), or -1 when the dir /
/// stamps are unreadable or absent. A healthy lane rewrites a stamp
/// at every UTC slot; an age far beyond the slot spacing means the
/// minutely job is dead (the 2026-08-27→28 failure ran silent for
/// 28 h with zero signals). Cold path — 5 s publish cadence;
/// allocation + syscalls are sanctioned here like the rest of the
/// publish block.
fn restart_stamp_age_secs() -> i64 {
    let Ok(home) = std::env::var("HOME") else {
        return -1;
    };
    let dir = std::path::Path::new(&home).join("multivenue/state");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return -1;
    };
    let mut newest: Option<std::time::SystemTime> = None;
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("last-restart-utc-") {
            continue;
        }
        if let Ok(md) = ent.metadata() {
            if let Ok(m) = md.modified() {
                if newest.is_none_or(|n| m > n) {
                    newest = Some(m);
                }
            }
        }
    }
    let Some(m) = newest else {
        return -1;
    };
    match std::time::SystemTime::now().duration_since(m) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
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

    use core_config::universe::{OptionsPolicy, BN_OPT_ORDINAL_BASE, OPT_ORDINAL_BASE};
    use core_config::Config;
    use core_types::{make_symbol_id, SymbolId, VenueId};
    use ingress_binance::discovery::BnDiscovery;
    use ingress_deribit::discovery::{parse_index_price, select_capped_chain, DeribitDiscovery};
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
        /// per instrument. M2.2: ALSO carries the appended options
        /// rows, and is `Some` when the options policy alone enables
        /// the venue (options-only `[okx]` is a valid universe).
        pub okx_table: Option<ingress_okx::OkxSymbolTable>,
        /// M2.2: the selected OKX capped options chain — `(instId,
        /// sym)` pairs in the deterministic allocation order (base
        /// [`OPT_ORDINAL_BASE`]). Already inside `okx_table`; carried
        /// separately for the gauge + logging. Empty when the policy
        /// is disabled.
        pub okx_options: Vec<(String, SymbolId)>,
        /// Deribit coverage; `None` when `--deribit-symbols` is unset.
        /// Deribit's symbol table is still built by
        /// [`super::build_deribit_symbol_table`] exactly as before —
        /// unlike OKX it doesn't need discovery data to construct.
        pub deribit: Option<VenueCoverage>,
        /// M2.1: the selected capped options chain — `(instrument,
        /// sym)` pairs in the DETERMINISTIC allocation order
        /// (underlyings in config order; per underlying: expiry asc →
        /// strike asc → call before put; ordinals from
        /// [`OPT_ORDINAL_BASE`]). Empty when the policy is disabled.
        /// The bin appends these to the deribit symbol table via
        /// `insert_option` (quote-only subscription).
        pub deribit_options: Vec<(String, SymbolId)>,
        /// Hyperliquid coverage; `None` when `--hl-coins` is unset.
        /// Hyperliquid's coin table is still built by
        /// [`super::build_hl_coin_table`] exactly as before.
        pub hl: Option<VenueCoverage>,
        /// Binance coverage (M1 exchangeInfo audit); `None` when the
        /// caller skipped it (legacy flag boots keep their historical
        /// zero-REST Binance behavior — config boots audit).
        pub bn: Option<VenueCoverage>,
        /// M2.4: the selected Binance eapi options chain — `(symbol,
        /// sym, uly_idx)` in deterministic allocation order (base
        /// [`BN_OPT_ORDINAL_BASE`]). The bin builds the eapi lane
        /// table from these. Empty when the policy is disabled.
        pub bn_options: Vec<(String, SymbolId, u8)>,
        /// WS9: Bybit coverage (instruments-info audit, spot + linear
        /// pages); `None` when the `[bybit]` section is empty.
        pub bybit: Option<VenueCoverage>,
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

    /// `None` ⇒ `symbol_upper` is a TRADING Binance symbol in the
    /// ingested exchangeInfo table. `Some(reason)` ⇒ MISSING.
    pub fn bn_missing_reason(d: &BnDiscovery, symbol_upper: &[u8]) -> Option<MissingReason> {
        match d.find(symbol_upper) {
            None => Some("not_found"),
            Some(row) if !row.trading => Some("not_trading"),
            Some(_) => None,
        }
    }

    /// Uppercase a configured (lowercase) stream symbol into a stack
    /// buffer for exchangeInfo lookup. Returns the buffer + length.
    fn upper_symbol(s: &str) -> ([u8; 32], usize) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(32);
        let mut out = [0u8; 32];
        for i in 0..n {
            out[i] = bytes[i].to_ascii_uppercase();
        }
        (out, n)
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
        core_net::boot_http::https_get(
            tls,
            host,
            port,
            path,
            USER_AGENT,
            buf,
            MAX_BODY,
            FETCH_TIMEOUT,
        )
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
        asset_ids: &[String],
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.polymarket_gamma_host, 443)?;
        // M1 multi-market: one Gamma query per configured id — the
        // single-id query shape + parse path proven live in 8e/H6a,
        // looped. 150 ms spacing mirrors the OKX page pacing.
        let mut matched = 0u32;
        let mut universe = 0u32;
        for (i, asset_id) in asset_ids.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let path = format!("/markets?clob_token_ids={asset_id}");
            let range = get(tls, host, port, &path, buf).map_err(|e| {
                tracing::error!(venue = "pm", asset_id, error = ?e, "discovery: fetch failed");
                "pm: discovery fetch failed"
            })?;
            let mut d = PmDiscovery::new();
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "pm", asset_id, error = ?e, "discovery: parse failed");
                "pm: discovery parse failed"
            })?;

            match pm_missing_reason(&d, asset_id.as_bytes()) {
                None => {
                    matched += 1;
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
            universe += d.universe_total();
        }
        let configured = asset_ids.len() as u32;
        tracing::info!(
            venue = "pm",
            configured,
            matched,
            universe,
            "discovery: coverage"
        );
        Ok(VenueCoverage {
            configured,
            matched,
            universe,
        })
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

        let configured: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
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
        tracing::info!(
            venue = "okx",
            configured = configured.len(),
            matched,
            universe,
            "discovery: coverage"
        );

        let table = super::build_okx_symbol_table(spec, &d)?;
        Ok((
            VenueCoverage {
                configured: configured.len() as u32,
                matched,
                universe,
            },
            table,
        ))
    }

    /// M2.2: fetch + select the capped OKX options chain — the
    /// Deribit `run_deribit_options` law on the v5 surface. Per
    /// configured underlying (`uly`, e.g. `"BTC-USD"`): ONE
    /// `index-tickers` fetch (the ATM reference — the uly IS the
    /// index instId) + ONE `instType=OPTION&uly=` page into a FRESH
    /// table, then `select_capped_chain`. Ordinals allocated HERE in
    /// selection order from [`OPT_ORDINAL_BASE`]. Fetch/parse
    /// failures FATAL; an EMPTY per-underlying selection is MISSING
    /// semantics (reason `no_chain`). 150 ms pacing matches the OKX
    /// page pacing.
    fn run_okx_options(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        policy: &OptionsPolicy,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<Vec<(String, SymbolId)>, &'static str> {
        let (host, port) = split_host_port(&cfg.okx_rest_host, 443)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut out: Vec<(String, SymbolId)> = Vec::new();
        let mut k = 0u32;
        for uly in &policy.underlyings {
            std::thread::sleep(Duration::from_millis(150));
            let idx_path = format!("/api/v5/market/index-tickers?instId={uly}");
            let range = get(tls, host, port, &idx_path, buf).map_err(|e| {
                tracing::error!(venue = "okx", underlying = %uly, error = ?e, "discovery: index-price fetch failed");
                "okx: options index-price fetch failed"
            })?;
            let index_px_1e9 =
                ingress_okx::discovery::parse_index_price(&buf[range]).map_err(|e| {
                    tracing::error!(venue = "okx", underlying = %uly, error = ?e, "discovery: index-price parse failed");
                    "okx: options index-price parse failed"
                })?;

            std::thread::sleep(Duration::from_millis(150));
            let path = format!("/api/v5/public/instruments?instType=OPTION&uly={uly}");
            let range = get(tls, host, port, &path, buf).map_err(|e| {
                tracing::error!(venue = "okx", underlying = %uly, error = ?e, "discovery: options fetch failed");
                "okx: options discovery fetch failed"
            })?;
            let mut d = OkxDiscovery::new();
            d.ingest_options_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "okx", underlying = %uly, error = ?e, "discovery: options parse failed");
                "okx: options discovery parse failed"
            })?;

            let sel = ingress_okx::discovery::select_capped_chain(
                d.rows(),
                index_px_1e9,
                policy.expiries,
                policy.strikes,
                now_ms,
            );
            if sel.is_empty() {
                *any_missing = true;
                tracing::error!(
                    venue = "okx",
                    underlying = %uly,
                    reason = "no_chain",
                    chain_total = d.universe_total(),
                    "discovery: options underlying selected no instruments"
                );
            }
            for row in &sel {
                let inst_id = core::str::from_utf8(row.inst_id())
                    .map_err(|_| "okx: non-utf8 option instId")?;
                let sym = make_symbol_id(VenueId::Okx, OPT_ORDINAL_BASE + k + 1);
                k += 1;
                out.push((inst_id.to_string(), sym));
            }
            tracing::info!(
                venue = "okx",
                underlying = %uly,
                index_px_1e9,
                expiries = policy.expiries,
                strikes = policy.strikes,
                chain_total = d.universe_total(),
                chain_live = d.universe_live(),
                selected = sel.len(),
                "discovery: options chain"
            );
        }
        if out.len() > ingress_okx::OKX_OPT_MAX {
            tracing::error!(
                venue = "okx",
                selected = out.len(),
                cap = ingress_okx::OKX_OPT_MAX,
                "discovery: selected options chain exceeds the per-connection cap"
            );
            return Err("okx: selected options chain exceeds OKX_OPT_MAX — shrink \
                 options_underlyings/options_expiries/options_strikes");
        }
        Ok(out)
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

        let configured: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        // WS6: a configured SPOT instrument (name-shape law: no `-`,
        // e.g. `BTC_USDC`) lives on the `kind=spot` pages — fetch
        // them ONLY when the config asks for spot (3 paced requests).
        if configured.iter().any(|s| !s.contains('-')) {
            for ccy in ["BTC", "ETH", "USDC"] {
                std::thread::sleep(Duration::from_millis(1050));
                let path = format!("/api/v2/public/get_instruments?currency={ccy}&kind=spot");
                let range = get(tls, host, port, &path, buf).map_err(|e| {
                    tracing::error!(venue = "deribit", currency = %ccy, page = "spot", error = ?e, "discovery: fetch failed");
                    "deribit: discovery fetch failed"
                })?;
                d.ingest_spot_body(&buf[range]).map_err(|e| {
                    tracing::error!(venue = "deribit", currency = %ccy, page = "spot", error = ?e, "discovery: parse failed");
                    "deribit: discovery parse failed"
                })?;
            }
        }
        let mut matched = 0u32;
        let mut dated = 0u32;
        for instr in &configured {
            match deribit_missing_reason(&d, instr.as_bytes()) {
                None => {
                    matched += 1;
                    // WS3 (gaps §1): `settlement_period` was parsed
                    // since 8e and never used. A configured DATED
                    // future is named at boot — its ticker carries no
                    // funding (the run loop's `has_funding` gate is
                    // the wire-level twin of this split). WS6: spot
                    // rows (no `-` in the name) are their own class,
                    // not dated futures.
                    if let Some(row) = d.find(instr.as_bytes()) {
                        if !row.perpetual && instr.contains('-') {
                            dated += 1;
                            tracing::info!(
                                venue = "deribit",
                                symbol = *instr,
                                "discovery: configured instrument is a dated future (no funding on its ticker)"
                            );
                        }
                    }
                }
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
        tracing::info!(
            venue = "deribit",
            configured = configured.len(),
            matched,
            dated,
            universe,
            "discovery: coverage"
        );
        Ok(VenueCoverage {
            configured: configured.len() as u32,
            matched,
            universe,
        })
    }

    /// M2.1: fetch + select the capped Deribit options chain
    /// (docs/m2-progress.md design entry). Per configured underlying:
    /// ONE `get_index_price` fetch (the ATM reference) + ONE
    /// `kind=option` `get_instruments` page into a FRESH table, then
    /// [`select_capped_chain`] (nearest-E expiries × K nearest-ATM
    /// strikes, calls+puts). Ordinals are allocated HERE, in selection
    /// order, from [`OPT_ORDINAL_BASE`] — disjoint from every
    /// file-order ordinal by construction. Fetch/parse failures are
    /// FATAL (index price included — no silent options-less boot); an
    /// underlying whose selection comes back EMPTY is MISSING
    /// semantics (`any_missing`, reason `no_chain`) — paper warns,
    /// live refuses, exactly like a missing configured symbol.
    fn run_deribit_options(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        policy: &OptionsPolicy,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<Vec<(String, SymbolId)>, &'static str> {
        let (host, port) = split_host_port(&cfg.deribit_rest_host, 443)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut out: Vec<(String, SymbolId)> = Vec::new();
        let mut k = 0u32;
        for ccy in &policy.underlyings {
            // Venue rate-limits public/get_* to 1 req/s — pace before
            // EVERY options-lane fetch (the futures lane may have just
            // finished its own paced sequence).
            std::thread::sleep(Duration::from_millis(1050));
            let idx_name = ingress_deribit::discovery::index_name(ccy);
            let idx_path = format!("/api/v2/public/get_index_price?index_name={idx_name}");
            let range = get(tls, host, port, &idx_path, buf).map_err(|e| {
                tracing::error!(venue = "deribit", underlying = %ccy, error = ?e, "discovery: index-price fetch failed");
                "deribit: options index-price fetch failed"
            })?;
            let index_px_1e9 = parse_index_price(&buf[range]).map_err(|e| {
                tracing::error!(venue = "deribit", underlying = %ccy, error = ?e, "discovery: index-price parse failed");
                "deribit: options index-price parse failed"
            })?;

            std::thread::sleep(Duration::from_millis(1050));
            let path = format!("/api/v2/public/get_instruments?currency={ccy}&kind=option");
            let range = get(tls, host, port, &path, buf).map_err(|e| {
                tracing::error!(venue = "deribit", underlying = %ccy, error = ?e, "discovery: options fetch failed");
                "deribit: options discovery fetch failed"
            })?;
            let mut d = DeribitDiscovery::new();
            d.ingest_options_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "deribit", underlying = %ccy, error = ?e, "discovery: options parse failed");
                "deribit: options discovery parse failed"
            })?;

            let sel = select_capped_chain(
                d.rows(),
                index_px_1e9,
                policy.expiries,
                policy.strikes,
                now_ms,
            );
            if sel.is_empty() {
                *any_missing = true;
                tracing::error!(
                    venue = "deribit",
                    underlying = %ccy,
                    reason = "no_chain",
                    chain_total = d.universe_total(),
                    "discovery: options underlying selected no instruments"
                );
            }
            for row in &sel {
                let name = core::str::from_utf8(row.instrument_name())
                    .map_err(|_| "deribit: non-utf8 option instrument name")?;
                let sym = make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + k + 1);
                k += 1;
                out.push((name.to_string(), sym));
            }
            tracing::info!(
                venue = "deribit",
                underlying = %ccy,
                index_px_1e9,
                expiries = policy.expiries,
                strikes = policy.strikes,
                chain_total = d.universe_total(),
                chain_live = d.universe_live(),
                selected = sel.len(),
                "discovery: options chain"
            );
        }
        if out.len() > ingress_deribit::DERIBIT_OPT_MAX {
            tracing::error!(
                venue = "deribit",
                selected = out.len(),
                cap = ingress_deribit::DERIBIT_OPT_MAX,
                "discovery: selected options chain exceeds the per-connection cap"
            );
            return Err(
                "deribit: selected options chain exceeds DERIBIT_OPT_MAX — shrink \
                 options_underlyings/options_expiries/options_strikes",
            );
        }
        Ok(out)
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

        let configured: Vec<&str> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let mut matched = 0u32;
        for coin in &configured {
            match d.resolve(coin.as_bytes()) {
                Some(info) => {
                    matched += 1;
                    // WS8 (gaps §2.5 tick/lot): the audit row now
                    // names the venue's size/price granularity for
                    // perps (lot step = 10^-szDecimals; price tick =
                    // ≤ max_price_decimals decimals, ≤5 sig figs).
                    tracing::debug!(
                        venue = "hl",
                        coin = *coin,
                        asset_id = info.asset_id,
                        kind = ?info.kind,
                        sz_decimals = info.sz_decimals,
                        max_price_decimals = info.max_price_decimals().unwrap_or(0),
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
        tracing::info!(
            venue = "hl",
            configured = configured.len(),
            matched,
            universe,
            "discovery: coverage"
        );
        Ok(VenueCoverage {
            configured: configured.len() as u32,
            matched,
            universe,
        })
    }

    fn run_bn(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spot: &[String],
        usdm: &[String],
        dated: &[String],
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let mut d = BnDiscovery::new();
        let mut matched = 0u32;

        // Spot: one `?symbol=` probe per configured symbol. The venue
        // 400s unknown symbols — mapped to MISSING (not fatal), every
        // other transport/parse failure stays fatal. 150 ms spacing
        // mirrors the OKX page pacing.
        let (spot_host, spot_port) = split_host_port(&cfg.binance_rest_host, 443)?;
        for (i, sym) in spot.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let (up, up_len) = upper_symbol(sym);
            let upper = core::str::from_utf8(&up[..up_len]).unwrap_or("");
            let path = format!("/api/v3/exchangeInfo?symbol={upper}");
            match get(tls, spot_host, spot_port, &path, buf) {
                Ok(range) => {
                    d.ingest_body(&buf[range]).map_err(|e| {
                        tracing::error!(venue = "bn", symbol = sym.as_str(), error = ?e, "discovery: parse failed");
                        "bn: discovery parse failed"
                    })?;
                    match bn_missing_reason(&d, &up[..up_len]) {
                        None => matched += 1,
                        Some(reason) => {
                            *any_missing = true;
                            tracing::error!(
                                venue = "bn",
                                symbol = sym.as_str(),
                                reason,
                                "discovery: configured symbol missing from venue universe"
                            );
                        }
                    }
                }
                Err(core_net::boot_http::BootHttpErr::Status(400)) => {
                    *any_missing = true;
                    tracing::error!(
                        venue = "bn",
                        symbol = sym.as_str(),
                        reason = "not_found",
                        "discovery: configured symbol missing from venue universe (HTTP 400)"
                    );
                }
                Err(e) => {
                    tracing::error!(venue = "bn", symbol = sym.as_str(), error = ?e, "discovery: fetch failed");
                    return Err("bn: discovery fetch failed");
                }
            }
        }

        // USDS-M: one full exchangeInfo page, membership-checked
        // (perps AND — WS5 — the dated delivery class).
        if !usdm.is_empty() || !dated.is_empty() {
            let (fut_host, fut_port) = split_host_port(&cfg.binance_fut_rest_host, 443)?;
            let range = get(tls, fut_host, fut_port, "/fapi/v1/exchangeInfo", buf).map_err(|e| {
                tracing::error!(venue = "bn", page = "fapi", error = ?e, "discovery: fetch failed");
                "bn: discovery fetch failed"
            })?;
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "bn", page = "fapi", error = ?e, "discovery: parse failed");
                "bn: discovery parse failed"
            })?;
            for sym in usdm {
                let (up, up_len) = upper_symbol(sym);
                match bn_missing_reason(&d, &up[..up_len]) {
                    None => matched += 1,
                    Some(reason) => {
                        *any_missing = true;
                        tracing::error!(
                            venue = "bn",
                            symbol = sym.as_str(),
                            market = "usdm",
                            reason,
                            "discovery: configured symbol missing from venue universe"
                        );
                    }
                }
            }
            // WS5: a `usdm_dated` entry must exist AND be a dated
            // contract class — a perpetual misfiled here would ride
            // the dated ordinal block and lie to every offline
            // consumer about its class.
            for sym in dated {
                let (up, up_len) = upper_symbol(sym);
                match bn_missing_reason(&d, &up[..up_len]) {
                    None => {
                        let is_dated = d
                            .find(&up[..up_len])
                            .is_some_and(|row| row.contract_type.is_dated());
                        if is_dated {
                            matched += 1;
                        } else {
                            *any_missing = true;
                            tracing::error!(
                                venue = "bn",
                                symbol = sym.as_str(),
                                market = "usdm_dated",
                                reason = "not_dated",
                                "discovery: configured symbol is not a dated contract"
                            );
                        }
                    }
                    Some(reason) => {
                        *any_missing = true;
                        tracing::error!(
                            venue = "bn",
                            symbol = sym.as_str(),
                            market = "usdm_dated",
                            reason,
                            "discovery: configured symbol missing from venue universe"
                        );
                    }
                }
            }
        }

        let configured = (spot.len() + usdm.len() + dated.len()) as u32;
        let universe = d.universe_trading();
        tracing::info!(
            venue = "bn",
            configured,
            matched,
            universe,
            "discovery: coverage"
        );
        Ok(VenueCoverage {
            configured,
            matched,
            universe,
        })
    }

    /// M2.4: fetch + select the capped Binance eapi options chain —
    /// the Deribit/OKX law on the eapi surface. ONE `exchangeInfo`
    /// page carries EVERY underlying (the selection filters per
    /// family); one paced `index` fetch per configured underlying is
    /// the ATM reference. Ordinals allocated HERE in selection order
    /// from [`BN_OPT_ORDINAL_BASE`] (the venue's 512-block belongs to
    /// usdm). Fetch/parse failures FATAL; an EMPTY per-underlying
    /// selection is MISSING semantics (reason `no_chain`). Returns
    /// `(symbol, sym, uly_idx)` — the lane table needs the underlying
    /// index for its per-family index-price cache.
    fn run_bn_options(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        policy: &OptionsPolicy,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<Vec<(String, SymbolId, u8)>, &'static str> {
        let (host, port) = split_host_port(&cfg.binance_eapi_rest_host, 443)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let range = get(tls, host, port, "/eapi/v1/exchangeInfo", buf).map_err(|e| {
            tracing::error!(venue = "bn", error = ?e, "discovery: eapi exchangeInfo fetch failed");
            "bn: eapi exchangeInfo fetch failed"
        })?;
        let mut d = ingress_binance::eapi::EapiDiscovery::new();
        d.ingest_exchange_info(&buf[range]).map_err(|e| {
            tracing::error!(venue = "bn", error = ?e, "discovery: eapi exchangeInfo parse failed");
            "bn: eapi exchangeInfo parse failed"
        })?;

        let mut out: Vec<(String, SymbolId, u8)> = Vec::new();
        let mut k = 0u32;
        for (uly_idx, uly) in policy.underlyings.iter().enumerate() {
            std::thread::sleep(Duration::from_millis(150));
            let idx_path = format!("/eapi/v1/index?underlying={uly}");
            let range = get(tls, host, port, &idx_path, buf).map_err(|e| {
                tracing::error!(venue = "bn", underlying = %uly, error = ?e, "discovery: eapi index fetch failed");
                "bn: eapi index fetch failed"
            })?;
            let index_px_1e9 =
                ingress_binance::eapi::parse_index_price(&buf[range]).map_err(|e| {
                    tracing::error!(venue = "bn", underlying = %uly, error = ?e, "discovery: eapi index parse failed");
                    "bn: eapi index parse failed"
                })?;

            let sel = ingress_binance::eapi::select_capped_chain(
                d.rows(),
                uly.as_bytes(),
                index_px_1e9,
                policy.expiries,
                policy.strikes,
                now_ms,
            );
            if sel.is_empty() {
                *any_missing = true;
                tracing::error!(
                    venue = "bn",
                    underlying = %uly,
                    reason = "no_chain",
                    chain_total = d.universe_total(),
                    "discovery: options underlying selected no instruments"
                );
            }
            for row in &sel {
                let symbol = core::str::from_utf8(row.symbol())
                    .map_err(|_| "bn: non-utf8 eapi option symbol")?;
                let sym = make_symbol_id(VenueId::Binance, BN_OPT_ORDINAL_BASE + k + 1);
                k += 1;
                out.push((symbol.to_string(), sym, uly_idx as u8));
            }
            tracing::info!(
                venue = "bn",
                underlying = %uly,
                index_px_1e9,
                expiries = policy.expiries,
                strikes = policy.strikes,
                chain_total = d.universe_total(),
                selected = sel.len(),
                "discovery: options chain"
            );
        }
        if out.len() > ingress_binance::eapi::EAPI_OPT_MAX {
            tracing::error!(
                venue = "bn",
                selected = out.len(),
                cap = ingress_binance::eapi::EAPI_OPT_MAX,
                "discovery: selected options chain exceeds the per-connection cap"
            );
            return Err("bn: selected options chain exceeds EAPI_OPT_MAX — shrink \
                 options_underlyings/options_expiries/options_strikes");
        }
        Ok(out)
    }

    /// Run the full boot discovery pass: OKX (if `okx_spec` is
    /// configured), Deribit (if `deribit_spec` is configured),
    /// Hyperliquid (if `hl_spec` is configured), Binance (M1 — if the
    /// caller passes the spot/usdm lists; legacy flag boots pass
    /// `None` and keep their historical zero-REST Binance behavior),
    /// then Polymarket (always). One reused `Vec<u8>` buffer carries
    /// every fetch's response body. Any fetch/parse failure is FATAL —
    /// returned as `Err` for the caller to log + exit non-zero; a
    /// MISSING symbol is not itself an `Err` here (see
    /// [`Outcome::any_missing`] — the caller decides paper-vs-live).
    #[allow(clippy::too_many_arguments)]
    pub fn run_all(
        cfg: &Config,
        tls_config: &Arc<rustls::ClientConfig>,
        okx_spec: Option<&str>,
        okx_options_policy: &OptionsPolicy,
        deribit_spec: Option<&str>,
        deribit_options_policy: &OptionsPolicy,
        hl_spec: Option<&str>,
        binance: Option<(&[String], &[String], &[String])>,
        bn_options_policy: &OptionsPolicy,
        bybit: Option<(&[String], &[String])>,
        polymarket_asset_ids: &[String],
    ) -> Result<Outcome, &'static str> {
        let mut buf: Vec<u8> = Vec::new();
        let mut any_missing = false;

        let (okx, mut okx_table) = match okx_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => {
                let (cov, table) = run_okx(cfg, tls_config, spec, &mut buf, &mut any_missing)?;
                (Some(cov), Some(table))
            }
            // M2.2: an options-only [okx] section still boots the
            // venue — empty static table, chain appended below.
            None if okx_options_policy.enabled() => {
                (None, Some(ingress_okx::OkxSymbolTable::new()))
            }
            None => (None, None),
        };

        // M2.2: the capped OKX options chain (config-file policy).
        let okx_options = if okx_options_policy.enabled() {
            let pairs = run_okx_options(
                cfg,
                tls_config,
                okx_options_policy,
                &mut buf,
                &mut any_missing,
            )?;
            let table = okx_table
                .as_mut()
                .expect("policy-on arm always has a table");
            super::extend_okx_table_with_options(table, &pairs)?;
            pairs
        } else {
            Vec::new()
        };

        let deribit = match deribit_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => Some(run_deribit(
                cfg,
                tls_config,
                spec,
                &mut buf,
                &mut any_missing,
            )?),
            None => None,
        };

        // M2.1: the capped options chain (config-file policy; legacy
        // boots carry a disabled default and skip this entirely).
        let deribit_options = if deribit_options_policy.enabled() {
            run_deribit_options(
                cfg,
                tls_config,
                deribit_options_policy,
                &mut buf,
                &mut any_missing,
            )?
        } else {
            Vec::new()
        };

        let hl = match hl_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => Some(run_hl(cfg, tls_config, spec, &mut buf, &mut any_missing)?),
            None => None,
        };

        let bn = match binance {
            Some((spot, usdm, dated))
                if !spot.is_empty() || !usdm.is_empty() || !dated.is_empty() =>
            {
                Some(run_bn(
                    cfg,
                    tls_config,
                    spot,
                    usdm,
                    dated,
                    &mut buf,
                    &mut any_missing,
                )?)
            }
            _ => None,
        };

        // M2.4: the eapi capped options chain — its own surface,
        // independent of the spot/usdm audit arm.
        let bn_options = if bn_options_policy.enabled() {
            run_bn_options(
                cfg,
                tls_config,
                bn_options_policy,
                &mut buf,
                &mut any_missing,
            )?
        } else {
            Vec::new()
        };

        // WS9: the Bybit instruments-info audit (spot + linear pages,
        // only the configured categories are fetched).
        let bybit_cov = match bybit {
            Some((spot, linear)) if !spot.is_empty() || !linear.is_empty() => Some(run_bybit(
                cfg,
                tls_config,
                spot,
                linear,
                &mut buf,
                &mut any_missing,
            )?),
            _ => None,
        };

        let pm = run_pm(
            cfg,
            tls_config,
            polymarket_asset_ids,
            &mut buf,
            &mut any_missing,
        )?;

        Ok(Outcome {
            any_missing,
            pm,
            okx,
            okx_table,
            okx_options,
            deribit,
            deribit_options,
            hl,
            bn,
            bn_options,
            bybit: bybit_cov,
        })
    }

    /// WS9: the Bybit boot audit — one PAGED `instruments-info` walk
    /// per configured category (spot / linear), membership +
    /// liveness checked per configured symbol; tick/lot metadata
    /// rides the rows (the WS4 parity line). 150 ms page pacing.
    fn run_bybit(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spot: &[String],
        linear: &[String],
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.bybit_rest_host, 443)?;
        let mut matched = 0u32;
        let mut universe = 0u32;
        for (category, symbols) in [("spot", spot), ("linear", linear)] {
            if symbols.is_empty() {
                continue;
            }
            // Per-category table: spot and linear share symbol TEXT
            // but are different instruments.
            let mut d = ingress_bybit::discovery::BybitDiscovery::new();
            let mut cursor: Option<String> = None;
            loop {
                let path = match &cursor {
                    None => format!("/v5/market/instruments-info?category={category}&limit=1000"),
                    Some(c) => format!(
                        "/v5/market/instruments-info?category={category}&limit=1000&cursor={c}"
                    ),
                };
                let range = get(tls, host, port, &path, buf).map_err(|e| {
                    tracing::error!(venue = "bybit", category, error = ?e, "discovery: fetch failed");
                    "bybit: discovery fetch failed"
                })?;
                d.ingest_body(&buf[range.clone()]).map_err(|e| {
                    tracing::error!(venue = "bybit", category, error = ?e, "discovery: parse failed");
                    "bybit: discovery parse failed"
                })?;
                match ingress_bybit::discovery::next_page_cursor(&buf[range]) {
                    Some(c) => {
                        cursor = Some(
                            core::str::from_utf8(c)
                                .map_err(|_| "bybit: non-utf8 page cursor")?
                                .to_string(),
                        );
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    None => break,
                }
            }
            universe += d.universe_trading();
            for symbol in symbols {
                match d.find(symbol.as_bytes()) {
                    Some(row) if row.trading => {
                        matched += 1;
                        tracing::debug!(
                            venue = "bybit",
                            category,
                            symbol = symbol.as_str(),
                            tick_size_1e9 = row.tick_size_1e9,
                            lot_step_1e9 = row.lot_step_1e9,
                            "discovery: bybit instrument resolved"
                        );
                    }
                    Some(_) => {
                        *any_missing = true;
                        tracing::error!(
                            venue = "bybit",
                            category,
                            symbol = symbol.as_str(),
                            reason = "not_trading",
                            "discovery: configured symbol missing from venue universe"
                        );
                    }
                    None => {
                        *any_missing = true;
                        tracing::error!(
                            venue = "bybit",
                            category,
                            symbol = symbol.as_str(),
                            reason = "not_found",
                            "discovery: configured symbol missing from venue universe"
                        );
                    }
                }
            }
        }
        let configured = (spot.len() + linear.len()) as u32;
        tracing::info!(
            venue = "bybit",
            configured,
            matched,
            universe,
            "discovery: coverage"
        );
        Ok(VenueCoverage {
            configured,
            matched,
            universe,
        })
    }

    // -----------------------------------------------------------
    // Tests — pure decision logic only, no network (see module docs).
    // -----------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        fn bn_fixture() -> BnDiscovery {
            let mut d = BnDiscovery::new();
            d.ingest_body(
                br#"{"symbols":[
                  {"symbol":"BTCUSDT","status":"TRADING"},
                  {"symbol":"OLDUSDT","status":"BREAK"}
                ]}"#,
            )
            .unwrap();
            d
        }

        #[test]
        fn bn_missing_reason_covers_found_not_trading_and_absent() {
            let d = bn_fixture();
            assert_eq!(bn_missing_reason(&d, b"BTCUSDT"), None);
            assert_eq!(bn_missing_reason(&d, b"OLDUSDT"), Some("not_trading"));
            assert_eq!(bn_missing_reason(&d, b"NOPEUSDT"), Some("not_found"));
        }

        #[test]
        fn upper_symbol_uppercases_into_stack_buffer() {
            let (buf, n) = upper_symbol("btcusdt");
            assert_eq!(&buf[..n], b"BTCUSDT");
            let (buf2, n2) = upper_symbol("btcusdt_260327");
            assert_eq!(&buf2[..n2], b"BTCUSDT_260327");
        }

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
            assert_eq!(
                pm_missing_reason(&d, b"00000000000000000000"),
                Some("not_found")
            );
        }
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// T1(b) (outage 2026-08-27 §5.3): the predicate that replaces
    /// the msgs-based reset. Happy path: data moved ⇒ reset. Failure
    /// mode: a rejection-only session (msgs moved, ticks did not)
    /// must keep escalating; a venue-quiet idle/staleness trip is
    /// rate-limited by construction and may reset.
    #[test]
    fn backoff_resets_only_on_moved_data_or_quiet_trip() {
        // Data moved ⇒ reset regardless of result class.
        assert!(should_reset_backoff(10, 3, false));
        // Rejection-only session: ticks unchanged ⇒ keep escalating.
        assert!(!should_reset_backoff(3, 3, false));
        // The exact outage shape: rejection received every cycle,
        // never a tick — first cycle from zero included.
        assert!(!should_reset_backoff(0, 0, false));
        // Venue-quiet idle/staleness trip ⇒ reset (budget-limited).
        assert!(should_reset_backoff(3, 3, true));
    }

    /// T1(c): stamp-age helper degrades to -1, never panics, when
    /// the stamp dir is missing (fresh hosts, CI).
    #[test]
    fn restart_stamp_age_handles_missing_dir() {
        // The helper reads $HOME/multivenue/state — on a host where
        // that does not exist it must return -1; where it does, any
        // value >= -1 is legal. Either way: no panic.
        assert!(restart_stamp_age_secs() >= -1);
    }

    fn temp_capture_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gauged_capture_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// G1 remediation item 3: the records gauge must advance on the
    /// 1 s `maybe_flush` cadence WITHOUT a run-loop exit — the first
    /// 6 h soak showed it frozen at run-loop-exit values.
    #[test]
    fn gauged_capture_publishes_on_flush_cadence() {
        let dir = temp_capture_dir("cadence");
        let mut reg = MetricsRegistry::new();
        let ids = register_capture_gauges(&mut reg, "deribit").unwrap();
        let reg = Arc::new(reg);
        let inner = PmlrCapture::open(&dir, "deribit", 7, TapCfg::off()).unwrap();
        let mut cap = GaugedCapture::new(inner, Some((reg.clone(), ids)));

        let t0 = 10_000_000_000u64;
        cap.tick(&Tick::new(
            t0,
            VenueId::Deribit,
            make_symbol_id(VenueId::Deribit, 1),
            1,
            core_types::Price::from_raw(1_000_000),
            core_types::Qty::from_raw(1_000_000),
            core_types::Price::from_raw(1_001_000),
            core_types::Qty::from_raw(1_000_000),
        ));
        // Inside the first second: mirrored once at the first poll
        // (last_pub_ns starts at 0 → t0 - 0 ≥ 1 s), then quiet.
        cap.maybe_flush(t0);
        assert_eq!(reg.gauge(ids.records).get(), 1, "first poll mirrors");
        cap.tick(&Tick::new(
            t0 + 1,
            VenueId::Deribit,
            make_symbol_id(VenueId::Deribit, 1),
            2,
            core_types::Price::from_raw(1_000_000),
            core_types::Qty::from_raw(1_000_000),
            core_types::Price::from_raw(1_001_000),
            core_types::Qty::from_raw(1_000_000),
        ));
        cap.maybe_flush(t0 + 500_000_000);
        assert_eq!(reg.gauge(ids.records).get(), 1, "rate-limited inside 1 s");
        // Past the interval: the new record shows without any
        // run-loop exit / mirror_now.
        cap.maybe_flush(t0 + 1_000_000_000);
        assert_eq!(
            reg.gauge(ids.records).get(),
            2,
            "advances on the 1 s cadence"
        );
        assert_eq!(reg.gauge(ids.io_errors).get(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Metrics off (`CaptureMetrics = None`): every hook must stay a
    /// pure delegate — no panics, capture still records.
    #[test]
    fn gauged_capture_without_metrics_is_a_pure_delegate() {
        let dir = temp_capture_dir("nometrics");
        let inner = PmlrCapture::open(&dir, "deribit", 7, TapCfg::off()).unwrap();
        let mut cap = GaugedCapture::new(inner, None);
        cap.tick(&Tick::new(
            5,
            VenueId::Deribit,
            make_symbol_id(VenueId::Deribit, 1),
            1,
            core_types::Price::from_raw(1_000_000),
            core_types::Qty::from_raw(1_000_000),
            core_types::Price::from_raw(1_001_000),
            core_types::Qty::from_raw(1_000_000),
        ));
        cap.maybe_flush(2_000_000_000);
        cap.mirror_now();
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        let event_lanes = {
            let mut it = rings.event.iter().map(|r| r.clone().split().1);
            [
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
            ]
        };
        let depth_lanes = {
            let mut it = rings.depth.iter().map(|r| r.clone().split().1);
            [it.next().unwrap(), it.next().unwrap()]
        };
        let opt_lanes = {
            let mut it = rings.opt.iter().map(|r| r.clone().split().1);
            [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]
        };
        Consumers {
            tick_lanes,
            event_lanes,
            depth_lanes,
            opt_lanes,
            rpc_signal: rings.rpc_signal.clone().split().1,
            fill_lanes,
            ai_cmds: rings.ai.clone().split().1,
            ai_status: Arc::new(AiIngressStatus::new()),
            ruleset_tables: rings.ruleset_tables.clone().split().1,
        }
    }

    /// WS13 live catch (2026-08-29): the wrapper swallowed every
    /// depth snapshot through the trait's default no-op while Book
    /// events flowed on both depth venues. Pin EVERY per-record hook
    /// forwarding to the inner capture so the next added channel
    /// cannot repeat this silently.
    #[test]
    fn gauged_capture_forwards_every_record_hook() {
        let dir =
            std::env::temp_dir().join(format!("gauged_fwd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut g = GaugedCapture::new(
            PmlrCapture::open(&dir, "okx", 1, core_io::TapCfg::off()).unwrap(),
            None,
        );
        let t = Tick::new(
            1,
            VenueId::Okx,
            7,
            1,
            core_types::Price::from_raw(1),
            core_types::Qty::from_raw(1),
            core_types::Price::from_raw(2),
            core_types::Qty::from_raw(1),
        );
        Capture::tick(&mut g, &t);
        Capture::event(
            &mut g,
            &ChannelEvent::new(1, VenueId::Okx, core_types::ChannelId::Funding, 7, 0, 0, 1, 0),
        );
        Capture::depth(&mut g, &core_types::DepthTopK::EMPTY);
        assert_eq!(g.inner.ticks_written(), 1);
        assert_eq!(g.inner.events_written(), 1);
        assert_eq!(g.inner.depths_written(), 1, "the WS13 live catch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rings_allocate_and_split() {
        let rings = Rings::new();
        let cons = split_all_consumers(&rings);
        assert_eq!(cons.tick_lanes.len(), NUM_TICK_LANES);
        assert_eq!(cons.fill_lanes.len(), NUM_FILL_LANES);
        // 8g item 4: the ruleset-table handoff ring splits like every
        // other ring and round-trips a slot.
        let (mut tp, mut tc) = rings.ruleset_tables.clone().split();
        assert_eq!(rings.ruleset_tables.capacity(), RULE_TABLE_RING_SLOTS);
        assert!(tp.try_push(core_types::RuleTableV2::EMPTY).is_ok());
        assert!(tc.try_pop().is_some());
        assert!(tc.try_pop().is_none());
    }

    /// 8g §4.3: the boot-universe snapshot is the PM/BN pair plus
    /// every discovery-gated venue-table id, sorted strict-ascending
    /// and deduped — the shape `RulesetSidePath::new` debug-asserts.
    #[test]
    fn ai_universe_is_sorted_deduped_union_of_boot_tables() {
        let d =
            okx_discovery_fixture(&[("BTC-USDT", "SPOT", true), ("ETH-USD-SWAP", "SWAP", true)]);
        let okx = build_okx_symbol_table("BTC-USDT,ETH-USD-SWAP", &d).unwrap();
        let deribit = build_deribit_symbol_table("BTC-PERPETUAL").unwrap();
        let hl = build_hl_coin_table("BTC,ETH").unwrap();

        let u = build_ai_universe(&[42], &[7], Some(&okx), Some(&deribit), Some(&hl));
        let expect: Vec<u32> = {
            let mut v = vec![
                42,
                7,
                make_symbol_id(VenueId::Okx, 1),
                make_symbol_id(VenueId::Okx, 2),
                make_symbol_id(VenueId::Deribit, 1),
                make_symbol_id(VenueId::Hyperliquid, 1),
                make_symbol_id(VenueId::Hyperliquid, 2),
            ];
            v.sort_unstable();
            v
        };
        assert_eq!(&u[..], &expect[..]);
        // Strict-ascending (sorted AND deduped) — the side-path
        // debug_assert's exact invariant.
        let mut i = 1usize;
        while i < u.len() {
            assert!(u[i - 1] < u[i], "strict ascending at {i}");
            i += 1;
        }
    }

    /// Failure-shape coverage: duplicate ids across sources collapse
    /// (dedup), and absent venues leave exactly the PM/BN pair.
    #[test]
    fn ai_universe_dedups_and_handles_absent_venues() {
        // PM and BN misconfigured to the same id: one survivor.
        let u = build_ai_universe(&[7], &[7], None, None, None);
        assert_eq!(&u[..], &[7]);

        // No optional venues: exactly the sorted pair.
        let u = build_ai_universe(&[42], &[7], None, None, None);
        assert_eq!(&u[..], &[7, 42]);

        // M1 multi-market: every PM token + every BN sym flows in.
        let u = build_ai_universe(&[42, 2, 3], &[7, 16_777_218], None, None, None);
        assert_eq!(&u[..], &[2, 3, 7, 42, 16_777_218]);
    }

    #[test]
    fn ai_hmac_key_parses_64_hex_chars() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let key = parse_ai_hmac_key(hex).unwrap();
        assert_eq!(key[0], 0x00);
        assert_eq!(key[1], 0x01);
        assert_eq!(key[31], 0x1f);
        // Mixed case + surrounding whitespace are tolerated.
        let key2 = parse_ai_hmac_key(
            " 000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F\n",
        )
        .unwrap();
        assert_eq!(key, key2);
    }

    #[test]
    fn ai_hmac_key_rejects_bad_length_and_bad_nibble() {
        assert!(parse_ai_hmac_key("").is_err());
        assert!(parse_ai_hmac_key("abcd").is_err(), "too short");
        let long = "00".repeat(33);
        assert!(parse_ai_hmac_key(&long).is_err(), "too long");
        let bad = "0g0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        assert!(parse_ai_hmac_key(bad).is_err(), "non-hex nibble");
    }

    #[test]
    fn drain_counters_add() {
        let mut a = DrainCounters {
            polymarket_ticks: 1,
            binance_ticks: 2,
            other_venue_ticks: 0,
            rpc_signals: 3,
            venue_events: 4,
            depth_snaps: 5,
            opt_records: 6,
        };
        let b = DrainCounters {
            polymarket_ticks: 10,
            binance_ticks: 20,
            other_venue_ticks: 5,
            rpc_signals: 30,
            venue_events: 40,
            depth_snaps: 50,
            opt_records: 60,
        };
        a.add(&b);
        assert_eq!(a.polymarket_ticks, 11);
        assert_eq!(a.binance_ticks, 22);
        assert_eq!(a.other_venue_ticks, 5);
        assert_eq!(a.rpc_signals, 33);
        assert_eq!(a.venue_events, 44);
        assert_eq!(a.depth_snaps, 55);
        assert_eq!(a.opt_records, 66);
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
        d.ingest_body(body.as_bytes())
            .expect("fixture body must parse");
        d
    }

    /// Happy path: `--okx-symbols` items get 1-based, flag-ordered
    /// ordinals under the Okx venue byte, with whitespace trimmed,
    /// and each row's `OkxInstType` comes from discovery.
    #[test]
    fn okx_symbol_table_allocates_flag_ordered_ids() {
        let d =
            okx_discovery_fixture(&[("BTC-USDT", "SPOT", true), ("ETH-USD-SWAP", "SWAP", true)]);
        let t = build_okx_symbol_table("BTC-USDT, ETH-USD-SWAP", &d).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.lookup(b"BTC-USDT"), Some(make_symbol_id(VenueId::Okx, 1)));
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
        assert_eq!(t.lookup(b"BTC-USDT"), Some(make_symbol_id(VenueId::Okx, 1)));
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
        // OKX_STATIC_MAX + 1 distinct instruments ⇒ Full.
        let mut spec = String::new();
        let mut rows: Vec<(String, &str, bool)> = Vec::new();
        for i in 0..=ingress_okx::OKX_STATIC_MAX {
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
            Some("okx: --okx-symbols exceeds OKX_STATIC_MAX instruments")
        );
        // Exactly OKX_STATIC_MAX is still fine.
        let max_spec = spec.rsplit_once(',').unwrap().0;
        assert_eq!(
            build_okx_symbol_table(max_spec, &full).unwrap().len(),
            ingress_okx::OKX_STATIC_MAX
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
    /// `DERIBIT_STATIC_MAX` instruments, and a dotted instrument
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
        // DERIBIT_STATIC_MAX + 1 distinct instruments ⇒ Full.
        let mut spec = String::new();
        for i in 0..=ingress_deribit::DERIBIT_STATIC_MAX {
            if i > 0 {
                spec.push(',');
            }
            spec.push_str(&format!("S{i}-PERPETUAL"));
        }
        assert_eq!(
            build_deribit_symbol_table(&spec).err(),
            Some("deribit: --deribit-symbols exceeds DERIBIT_STATIC_MAX instruments")
        );
        // Exactly DERIBIT_STATIC_MAX is still fine.
        let max_spec = spec.rsplit_once(',').unwrap().0;
        assert_eq!(
            build_deribit_symbol_table(max_spec).unwrap().len(),
            ingress_deribit::DERIBIT_STATIC_MAX
        );
        // A dotted instrument would corrupt channel-name parsing.
        assert_eq!(
            build_deribit_symbol_table("BTC.PERPETUAL").err(),
            Some("deribit: instrument in --deribit-symbols must not contain '.'")
        );
    }

    /// M2.1: the discovered options chain appends to the table after
    /// the static block, under the OPT_ORDINAL_BASE id law; dupes and
    /// the options-block cap refuse boot.
    #[test]
    fn deribit_table_extends_with_options_chain() {
        use core_config::universe::OPT_ORDINAL_BASE;
        let mut t = build_deribit_symbol_table("BTC-PERPETUAL").unwrap();
        let pairs = vec![
            (
                "BTC-27MAR26-100000-C".to_string(),
                make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 1),
            ),
            (
                "BTC-27MAR26-100000-P".to_string(),
                make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 2),
            ),
        ];
        extend_deribit_table_with_options(&mut t, &pairs).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t.static_len(), 1);
        assert_eq!(t.n_options(), 2);
        assert_eq!(
            t.lookup(b"BTC-27MAR26-100000-C"),
            Some(make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 1))
        );
        // Disjointness law: options ids can never collide with the
        // static block (ordinals 1.. ≤ 500 < 512).
        assert_ne!(
            t.lookup(b"BTC-27MAR26-100000-C"),
            t.lookup(b"BTC-PERPETUAL")
        );
        // Duplicate chain instrument refuses boot.
        let dup = vec![pairs[0].clone()];
        assert_eq!(
            extend_deribit_table_with_options(&mut t, &dup).err(),
            Some("deribit: duplicate instrument in discovered options chain")
        );
        // Options-block cap refuses boot with the actionable message.
        let mut big: Vec<(String, core_types::SymbolId)> = Vec::new();
        for i in 0..ingress_deribit::DERIBIT_OPT_MAX {
            big.push((
                format!("X{i}-C"),
                make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 100 + i as u32),
            ));
        }
        let e = extend_deribit_table_with_options(&mut t, &big)
            .err()
            .unwrap();
        assert!(e.contains("DERIBIT_OPT_MAX"), "{e}");
    }

    /// M2.2: the OKX table extension mirrors the deribit law — rows
    /// carry `OkxInstType::Option` (bbo-tbt-only gating), ordinals in
    /// the base-512 block, dupes + options cap fail fast.
    #[test]
    fn okx_table_extends_with_options_chain() {
        use core_config::universe::OPT_ORDINAL_BASE;
        let mut d = ingress_okx::discovery::OkxDiscovery::new();
        d.ingest_body(br#"{"code":"0","data":[{"instId":"BTC-USDT","instType":"SPOT","state":"live","tickSz":"0.1","lotSz":"0.01","ctVal":""}],"msg":""}"#)
            .unwrap();
        let mut t = build_okx_symbol_table("BTC-USDT", &d).unwrap();
        let pairs = vec![
            (
                "BTC-USD-260327-100000-C".to_string(),
                make_symbol_id(VenueId::Okx, OPT_ORDINAL_BASE + 1),
            ),
            (
                "BTC-USD-260327-100000-P".to_string(),
                make_symbol_id(VenueId::Okx, OPT_ORDINAL_BASE + 2),
            ),
        ];
        extend_okx_table_with_options(&mut t, &pairs).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(
            t.lookup(b"BTC-USD-260327-100000-C"),
            Some(make_symbol_id(VenueId::Okx, OPT_ORDINAL_BASE + 1))
        );
        // Duplicate chain instId refuses boot.
        let dup = vec![pairs[0].clone()];
        assert_eq!(
            extend_okx_table_with_options(&mut t, &dup).err(),
            Some("okx: duplicate instId in discovered options chain")
        );
        // Options cap refuses boot with the actionable message.
        let mut big: Vec<(String, core_types::SymbolId)> = Vec::new();
        for i in 0..ingress_okx::OKX_OPT_MAX {
            big.push((
                format!("X{i}-C"),
                make_symbol_id(VenueId::Okx, OPT_ORDINAL_BASE + 100 + i as u32),
            ));
        }
        let e = extend_okx_table_with_options(&mut t, &big).err().unwrap();
        assert!(e.contains("OKX_OPT_MAX"), "{e}");
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

    /// VT2: no `--stale-after-ms` ⇒ the measured per-venue defaults,
    /// `Ai` (no market data) 0.
    #[test]
    fn stale_after_ms_defaults_to_the_venue_table() {
        let t = parse_stale_after_ms(&[]).unwrap();
        assert_eq!(t[VenueId::Polymarket as usize], 1_000);
        assert_eq!(t[VenueId::Binance as usize], 1_000);
        assert_eq!(t[VenueId::Okx as usize], 400);
        assert_eq!(t[VenueId::Deribit as usize], 600);
        assert_eq!(t[VenueId::Hyperliquid as usize], 700);
        assert_eq!(t[VenueId::Ai as usize], 0);
        assert_eq!(t[VenueId::Bybit as usize], 500);
    }

    /// VT2: overrides replace only the named venue; the last spec for
    /// a venue wins; `0` is a legal "measure only" value.
    #[test]
    fn stale_after_ms_overrides_named_venues_only() {
        let specs = ["okx:250".to_owned(), "bn:0".to_owned(), "okx:300".to_owned()];
        let t = parse_stale_after_ms(&specs).unwrap();
        assert_eq!(t[VenueId::Okx as usize], 300);
        assert_eq!(t[VenueId::Binance as usize], 0);
        assert_eq!(t[VenueId::Bybit as usize], 500, "untouched venue keeps its default");
    }

    /// VT2: malformed specs refuse the boot with a named reason.
    #[test]
    fn stale_after_ms_rejects_bad_specs() {
        for bad in ["okx", "mars:400", "okx:fast", "okx:-1"] {
            let err = parse_stale_after_ms(&[bad.to_owned()]).unwrap_err();
            assert!(err.contains("--stale-after-ms"), "{err}");
        }
    }

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

    /// `rss` is a retired ingress label (8f item 16) that was never
    /// capture-bearing (§6.5) — it must stay rejected like any other
    /// unknown label, not silently ignored.
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

    /// More than seven comma-separated labels trips the defensive
    /// capacity guard — there are only seven known venues (WS9 added
    /// bybit), so this branch is a pure defense-in-depth backstop
    /// reached here by listing all seven plus an eighth item.
    #[test]
    fn raw_tap_flags_rejects_more_labels_than_known_venues() {
        assert_eq!(
            parse_raw_tap_flags(Some("pm,bn,okx,rpc,deribit,hl,bybit,pm2"), "rejects", 64).err(),
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

    // ------------- Phase 8g §9 — set/vm observability -------------

    /// Production-like clock for synthetic vm drives (the G3 lesson:
    /// cooldown first-window semantics need `now` ≥ horizon, which
    /// wallclock ns trivially satisfies).
    const VM_T0: NsTs = 100_000_000_000_000_000;
    const VM_HASH: [u8; 16] = [0x5A; 16];
    const VM_PM: SymbolId = 11;
    const VM_BN: SymbolId = 22;

    struct VmMirrorCtx {
        now: NsTs,
    }

    impl strategy_core::Ctx for VmMirrorCtx {
        fn submit(&mut self, _order: core_types::Order) -> Result<(), strategy_core::SubmitErr> {
            Ok(())
        }
        fn now_ns(&self) -> NsTs {
            self.now
        }
    }

    fn vm_mirror_table() -> Box<core_types::RuleTableV2> {
        let mut t = Box::new(core_types::RuleTableV2::EMPTY);
        t.rows[0] = core_types::RuleRowV2::from_v1(&core_types::RuleRow::new(
            VM_PM,
            VM_BN,
            20,
            0,
            0,
            1_000_000,
            core_types::fnv1a_64(b"g6-mirror"),
            core_types::RuleRow::TRIGGER_CROSS_DEVIATION,
            core_types::RuleRow::SIDE_BOTH,
            0,
        ));
        t.len = 1;
        t.epoch = 3;
        t.hash128 = VM_HASH;
        t
    }

    fn vm_mirror_tick(venue: VenueId, sym: SymbolId, bid_1e6: i64, ask_1e6: i64) -> Tick {
        Tick::new(
            VM_T0,
            venue,
            sym,
            1,
            core_types::Price::from_raw(bid_1e6),
            core_types::Qty::from_raw(1_000_000),
            core_types::Price::from_raw(ask_1e6),
            core_types::Qty::from_raw(1_000_000),
        )
    }

    /// §9 happy path: gauges mirror as sets and counters as monotonic
    /// deltas through the StrategyCounters route, against a real
    /// `StrategySet` driven stage → commit → fire at `VM_T0`.
    #[test]
    fn vm_metrics_mirror_deltas_and_gauges() {
        let mut reg = MetricsRegistry::new();
        let mask_id = reg.register_gauge("engine_strategy_enabled_mask").unwrap();
        let ids = register_vm_metrics(&mut reg).unwrap();
        let mut last = VmCountersSnapshot::default();

        let mut s = strategy_set::StrategySet::new(strategy_set::BIT_VM);
        let mut c = VmMirrorCtx { now: VM_T0 };
        strategy_core::Strategy::on_start(&mut s, &mut c).unwrap();

        // Inert boot mirrors zeros (and the mask gauge reads the live
        // bit through UFCS — the G0 demo observable).
        reg.gauge(mask_id)
            .set(strategy_core::StrategyCounters::enabled_mask(&s) as i64);
        mirror_vm_metrics(&reg, &ids, &s, &mut last);
        assert_eq!(reg.gauge(mask_id).get(), i64::from(strategy_set::BIT_VM));
        assert_eq!(reg.gauge(ids.rows_active).get(), 0);
        assert_eq!(reg.gauge(ids.table_epoch).get(), 0);
        assert_eq!(reg.counter(ids.fires).get(), 0);

        // Mismatched Commit first (nothing staged) → commit_dropped.
        let bad = core_types::AiCmd::new(
            VM_T0,
            1,
            core_types::SYMBOL_ID_NONE,
            0,
            0,
            0,
            core_types::AiCmdKind::RulesetCommit,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_VM,
            core_types::AI_SIDE_NONE,
            0,
            0,
        );
        strategy_core::Strategy::on_ai(&mut s, &bad, &mut c);

        // Stage → Commit → diverged books → fire.
        s.vm_mut().receive_table_v2(&vm_mirror_table());
        let commit = core_types::AiCmd::new(
            VM_T0,
            2,
            core_types::SYMBOL_ID_NONE,
            i64::from_le_bytes(VM_HASH[..8].try_into().unwrap()),
            i64::from_le_bytes(VM_HASH[8..].try_into().unwrap()),
            0,
            core_types::AiCmdKind::RulesetCommit,
            VenueId::Ai,
            core_types::STRATEGY_SLOT_VM,
            core_types::AI_SIDE_NONE,
            0,
            0,
        );
        strategy_core::Strategy::on_ai(&mut s, &commit, &mut c);
        strategy_core::Strategy::on_tick(
            &mut s,
            &vm_mirror_tick(VenueId::Binance, VM_BN, 490_000, 510_000),
            &mut c,
        );
        strategy_core::Strategy::on_tick(
            &mut s,
            &vm_mirror_tick(VenueId::Polymarket, VM_PM, 390_000, 410_000),
            &mut c,
        );

        mirror_vm_metrics(&reg, &ids, &s, &mut last);
        assert_eq!(reg.gauge(ids.rows_active).get(), 1, "gauge is a set");
        assert_eq!(reg.gauge(ids.table_epoch).get(), 3, "fixture epoch");
        assert_eq!(reg.counter(ids.fires).get(), 1);
        assert_eq!(reg.counter(ids.orders_emitted).get(), 1);
        assert_eq!(reg.counter(ids.orders_dropped).get(), 0);
        assert_eq!(reg.counter(ids.commit_dropped).get(), 1);

        // Steady state: a third mirror with no strategy motion adds
        // zero deltas — the counters stay put (monotonic, no double
        // counting of cumulative sources).
        mirror_vm_metrics(&reg, &ids, &s, &mut last);
        assert_eq!(reg.counter(ids.fires).get(), 1);
        assert_eq!(reg.counter(ids.orders_emitted).get(), 1);
        assert_eq!(reg.counter(ids.commit_dropped).get(), 1);
    }

    /// §9 failure modes: bare strategies mirror an all-zero family
    /// (trait defaults), and a source regression (fresh strategy
    /// against a stale snapshot — the restart shape) saturates to a
    /// zero delta instead of underflowing.
    #[test]
    fn vm_metrics_mirror_bare_default_and_saturation() {
        struct Bare;
        impl strategy_core::StrategyCounters for Bare {}

        let mut reg = MetricsRegistry::new();
        let ids = register_vm_metrics(&mut reg).unwrap();

        let mut last = VmCountersSnapshot::default();
        mirror_vm_metrics(&reg, &ids, &Bare, &mut last);
        assert_eq!(reg.gauge(ids.rows_active).get(), 0);
        assert_eq!(reg.gauge(ids.table_epoch).get(), 0);
        assert_eq!(reg.counter(ids.fires).get(), 0);
        assert_eq!(reg.counter(ids.orders_emitted).get(), 0);
        assert_eq!(reg.counter(ids.orders_dropped).get(), 0);
        assert_eq!(reg.counter(ids.commit_dropped).get(), 0);
        assert_eq!(
            strategy_core::StrategyCounters::enabled_mask(&Bare),
            0,
            "bare boots read mask 0"
        );

        // Stale snapshot ahead of the (zero) sources: saturating_sub
        // yields 0-deltas, counters unmoved.
        let mut stale = VmCountersSnapshot {
            fires: 100,
            orders_emitted: 100,
            orders_dropped: 100,
            commit_dropped: 100,
        };
        mirror_vm_metrics(&reg, &ids, &Bare, &mut stale);
        assert_eq!(reg.counter(ids.fires).get(), 0, "regression saturates");
        assert_eq!(stale.fires, 0, "snapshot re-bases to the source");
    }

    /// Boot-surface pin: `Observability::build(true, _)` registers
    /// every §9 row under its verbatim design name (plus the AI-side
    /// `table_push_fail`), and the registry encodes them.
    #[test]
    fn observability_build_registers_section9_rows() {
        let obs = Observability::build(true, false).unwrap();
        let reg = obs.metrics.as_ref().unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let n = reg.encode_prometheus(&mut buf).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        for name in [
            "engine_strategy_enabled_mask",
            "engine_vm_rows_active",
            "engine_vm_table_epoch",
            "engine_vm_fires_total",
            "engine_vm_orders_emitted_total",
            "engine_vm_orders_dropped_total",
            "engine_vm_commit_dropped_total",
            "engine_ai_table_push_fail_total",
        ] {
            assert!(text.contains(name), "missing §9 row {name}");
        }
    }
}
