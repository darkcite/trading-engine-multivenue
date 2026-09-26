// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Paper-mode orchestration. Wires the four ingress run-loops into
//! dedicated threads, pins each to its own core, owns the lock-free
//! SPSC rings, and runs the engine loop on the main thread, which
//! emits a tick/signal summary every 5 s.
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
//! | main   | the engine loop (drains every ring) + 5 s log timer + reverse-order join |
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
use core_net::{should_reset_backoff, Backoff, Keepalive, KeepaliveCfg, TlsTransport};
use core_ring::{Consumer, Producer, Ring};
use core_time::now_ns;
use core_types::{
    make_symbol_id, AiCmd, Capture, ChannelEvent, DepthTopK, Fill, NsTs, OptSummary, Order,
    RuleTableSlot, Signal, SymbolId, Tick, TradePrint, VenueId, AI_RING_SIZE, DEPTH_RING_SIZE,
    EVENT_LANE_ASSET_CTX, EVENT_LANE_FUNDING, EVENT_RING_SIZE, OPT_RING_SIZE,
    RULE_TABLE_RING_SLOTS, TRADE_RING_SIZE,
};
use engine::{
    Engine, ENGINE_FILLS_FILE, ENGINE_ORDERS_FILE, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES,
    SIGNAL_RING_SIZE, TICK_RING_SIZE,
};
use engine_snapshot::{BootInfo, EngineSnapshot, SnapshotCell, SNAPSHOT_VENUES};
use ingress_ai::{AiCmdCapture, AiIngressCfg, RulesetSidePath};
// Re-exported (lib.rs) so the binary reaches the AI status slot type
// through `cli::` like every other paper-mode surface.
pub use ingress_ai::AiIngressStatus;
use rustls_pki_types::ServerName;

use ingress_binance::run_loop as bwl;
use ingress_bybit::run_loop as ywl;
use ingress_deribit::run_loop as dwl;
use ingress_hyperevm::run_loop as hel;
use ingress_hypercall::run_loop as hcl;
use ingress_hyperliquid::run_loop as hwl;
use ingress_mexc::run_loop as mxl;
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

/// Cadence at which the main-thread engine loop logs its counters.
const REPORT_PERIOD_NS: u64 = 5_000_000_000;

/// RG6: cadence of the `/state` snapshot publish (plan §6.1 — one
/// ≈ 24 KB POD copy into the seqlock per period, off the tick path).
const SNAPSHOT_PERIOD_NS: u64 = 1_000_000_000;

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
/// MX6: MEXC spot reaps a connection idle 60 s with a live
/// subscription and futures after ~1 min without a ping (plan §1.1
/// / §1.2, venue-documented). Both classes get their class-literal
/// probe (`{"method":"PING"}` / `{"method":"ping"}`) at 15 s; the
/// answer is inbound activity, so anything quieter than 40 s is a
/// dead session.
const MEXC_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 15_000_000_000,
    idle_timeout_ns: 40_000_000_000,
};
/// Polygon RPC: newHeads every ~2 s + our own 2 s poll → anything
/// quieter than 30 s is a dead session.
const RPC_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 10_000_000_000,
    idle_timeout_ns: 30_000_000_000,
};

/// HYPARB H3b: HyperEVM — a head every ~1 s plus our own polls; a
/// session quiet for 30 s is dead (the RPC law, one block faster).
const HYPEREVM_KEEPALIVE: KeepaliveCfg = KeepaliveCfg {
    ping_interval_ns: 10_000_000_000,
    idle_timeout_ns: 30_000_000_000,
};

// The engine's pool lane and the ingress's default ring are one size.
const _: () = assert!(engine::POOL_RING_SIZE == hel::DEFAULT_POOL_RING_CAP);

/// Maximum number of items the main thread drains per ring per
/// iteration. Bounded so a backed-up ring can't starve the others.
const DRAIN_BATCH: usize = 256;

/// **E6 — the longest the dispatcher may go without an idle moment.**
///
/// The loop gives the dispatcher its idle moment after a tick that
/// drained nothing, which is the right default: the venue work is
/// blocking and the market data is not.
///
/// But "when the rings are empty" ALONE would starve it. `tick`
/// drains up to [`DRAIN_BATCH`] per ring per lane, and a busy market
/// across six venues can keep every tick non-empty indefinitely — so
/// the one path that pumps the venue's user-event socket, runs the
/// reconciler and persists the budget would stop running exactly when
/// there is most trading to reconcile. A ceiling on the gap costs one
/// clock read per iteration and removes that failure entirely.
///
/// 2 ms: far below the reconciler's own 60 s cadence and the WS
/// backoff, so in practice this changes nothing but the worst case.
const DISPATCHER_IDLE_MAX_GAP_NS: u64 = 2_000_000;

/// Should the dispatcher get its idle moment this iteration?
///
/// Extracted from the loop so the starvation property is TESTABLE.
/// Inline, the condition sits inside a two-thousand-line function
/// that no test constructs, and "the socket still gets pumped in a
/// busy market" would be a claim in a comment rather than a fact —
/// which is the defect shape this lane keeps finding.
#[inline]
#[must_use]
pub const fn should_drive_idle(drained: usize, now_ns: u64, last_idle_ns: u64) -> bool {
    drained == 0 || now_ns.saturating_sub(last_idle_ns) >= DISPATCHER_IDLE_MAX_GAP_NS
}

/// **Paces the dispatcher's idle moment, and owns the stamping.**
///
/// `should_drive_idle` alone was not enough to hold the property. The
/// first cut stamped `last_idle_ns` with the clock read taken BEFORE
/// the blocking call, which collapses the ceiling in exactly the case
/// it exists for — one HTTPS round trip always exceeds 2 ms, so the
/// next iteration's clock is already past the gap and the gate fires
/// again immediately. Break-and-watch reinstated that bug and NOT ONE
/// TEST FAILED: the test modelled the stamping itself, so it was
/// asserting its own simulation rather than the loop.
///
/// Taking the clock and the call as arguments is what fixes that. The
/// loop hands it the real ones; a test hands it a fake clock that
/// advances by the call's cost, and the assertion is then about
/// behaviour rather than arithmetic.
#[derive(Debug, Default)]
pub struct IdlePacer {
    /// When the last idle moment FINISHED. `0` = never, so the first
    /// iteration drives it.
    last_ns: u64,
}

impl IdlePacer {
    /// A pacer that has never driven.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self { last_ns: 0 }
    }

    /// Drive the dispatcher if it is due, and return the current
    /// clock — **re-read after the call when one happened**.
    ///
    /// The returned value is what the caller must use for everything
    /// downstream. A stale `now` would have the 1 s `/state` publish
    /// and the 5 s report compute staleness against a reading from
    /// before the stall, under-reporting it by exactly the stall's
    /// duration and hiding the pause that caused it.
    #[inline]
    pub fn drive_if_due<C, F>(&mut self, drained: usize, now_ns: u64, mut clock: C, mut drive: F) -> u64
    where
        C: FnMut() -> u64,
        F: FnMut(),
    {
        if !should_drive_idle(drained, now_ns, self.last_ns) {
            return now_ns;
        }
        drive();
        let finished = clock();
        self.last_ns = finished;
        finished
    }
}

#[cfg(test)]
mod dispatcher_idle_tests {
    use super::{should_drive_idle, DISPATCHER_IDLE_MAX_GAP_NS};

    /// The pacer must hand back the POST-call clock, or every cadence
    /// downstream measures staleness from before the stall and
    /// under-reports it by exactly the stall's length.
    #[test]
    fn the_pacer_returns_the_clock_from_after_the_call() {
        use std::cell::Cell;
        let clock = Cell::new(1_000u64);
        let mut pacer = super::IdlePacer::new();
        let after = pacer.drive_if_due(
            0,
            clock.get(),
            || clock.get(),
            || clock.set(clock.get() + 50_000_000),
        );
        assert_eq!(after, 50_001_000, "the caller was handed a stale clock");
    }

    #[test]
    fn the_pacer_does_not_read_the_clock_when_it_does_not_drive() {
        use std::cell::Cell;
        let reads = Cell::new(0usize);
        let mut pacer = super::IdlePacer::new();
        // Drive once so `last_ns` is set, then a busy iteration a
        // microsecond later must not be due.
        let _ = pacer.drive_if_due(0, 1_000, || 1_000, || {});
        let n = pacer.drive_if_due(
            256,
            1_001,
            || {
                reads.set(reads.get() + 1);
                1_001
            },
            || panic!("drove when it was not due"),
        );
        assert_eq!(n, 1_001, "a non-driving call must pass the clock through");
        assert_eq!(reads.get(), 0, "and must not pay for a clock read");
    }

    /// The default: the venue work blocks, so market data must not
    /// queue behind it.
    #[test]
    fn a_quiet_tick_drives_the_dispatcher() {
        assert!(should_drive_idle(0, 1_000, 1_000));
    }

    /// **The starvation property.** `tick` drains up to `DRAIN_BATCH`
    /// per ring per lane, and a busy market across six venues can keep
    /// every tick non-empty indefinitely. Gated on emptiness ALONE,
    /// the only path that pumps the venue's user-event socket, runs
    /// the reconciler and persists the budget would stop running
    /// exactly when there is most trading to reconcile.
    /// One simulated second of the REAL loop, driven through
    /// [`IdlePacer`] with a fake clock that advances by `call_ns`
    /// whenever the dispatcher is driven.
    ///
    /// The point is that the pacer under test is the one the loop
    /// uses, stamping included. An earlier version of this helper
    /// re-implemented the stamping inline, so it asserted its own
    /// simulation — and a break-and-watch run that reinstated the
    /// pre-call stamp failed nothing at all.
    fn simulate_second(drained: usize, call_ns: u64) -> (usize, usize) {
        use std::cell::Cell;
        const ITER_NS: u64 = 1_000; // 1 us of tick work an iteration
        const START: u64 = 1_000;

        let clock = Cell::new(START);
        let mut pacer = super::IdlePacer::new();
        let mut drove = 0usize;
        let mut iters = 0usize;

        while clock.get() - START < 1_000_000_000 {
            clock.set(clock.get() + ITER_NS);
            iters += 1;
            let now = clock.get();
            let after = pacer.drive_if_due(
                drained,
                now,
                || clock.get(),
                || {
                    // The blocking call, as the wall clock sees it.
                    clock.set(clock.get() + call_ns);
                    drove += 1;
                },
            );
            assert!(after >= now, "the pacer handed back a clock from the past");
        }
        (drove, iters)
    }

    #[test]
    fn a_permanently_busy_market_still_gets_an_idle_moment() {
        let (drove, _) = simulate_second(256, 0);
        assert!(
            drove > 0,
            "a busy market never let the dispatcher near its socket"
        );
        assert!(
            (400..=600).contains(&drove),
            "drove {drove} times in a second"
        );
    }

    /// **The degeneration the first cut shipped.**
    ///
    /// The loop stamped `last_idle_ns` with a clock read taken BEFORE
    /// the blocking call. One HTTPS round trip always exceeds 2 ms,
    /// so the next iteration's clock was already past the gap and the
    /// gate fired again immediately — in exactly the regime the
    /// ceiling exists for, it meant "every iteration", and the engine
    /// thread sat inside `on_idle` continuously.
    ///
    /// Stamping from the POST-call clock is what bounds it, and this
    /// asserts the bound rather than the arithmetic: with a 50 ms
    /// call, one second admits about twenty of them and no more.
    /// **The degeneration the first cut shipped, measured the only
    /// way that can see it.**
    ///
    /// Counting DRIVES PER SECOND cannot: when the blocking call
    /// dominates the clock, "fires every iteration" and "fires every
    /// 2 ms" both give about twenty a second, and a break-and-watch
    /// run that reinstated the pre-call stamp passed this test
    /// happily at 20.
    ///
    /// What actually differs is how much TICK WORK the engine got to
    /// do between venue calls. Correct: ~2 ms of it, which at 1 µs an
    /// iteration is ~2000 iterations. Broken: one.
    #[test]
    fn a_blocking_call_leaves_the_engine_time_to_work_between_calls() {
        let (drove, iters) = simulate_second(256, 50_000_000); // 50 ms a call
        assert!(drove > 0, "never drove at all");
        let per_drive = iters / drove;
        assert!(
            per_drive >= 1_000,
            "only {per_drive} iterations of engine work between venue calls \
             ({drove} calls over {iters} iterations) — the ceiling collapsed \
             and the loop is a synchronous HTTP loop with the engine attached"
        );
    }

    #[test]
    fn a_very_slow_call_runs_at_its_own_pace_and_not_faster() {
        // A 5 s call — `http::REQ_DEADLINE`, the worst one round trip
        // can cost — cannot be entered twice inside one second.
        assert!(simulate_second(256, 5_000_000_000).0 <= 1);
    }

    #[test]
    fn the_gap_ceiling_is_a_ceiling_not_a_period() {
        assert!(!should_drive_idle(
            1,
            1_000 + DISPATCHER_IDLE_MAX_GAP_NS - 1,
            1_000
        ));
        // `>=`, because a ceiling that must be EXCEEDED is a ceiling
        // one iteration higher than it says.
        assert!(should_drive_idle(1, 1_000 + DISPATCHER_IDLE_MAX_GAP_NS, 1_000));
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_starve_it_forever() {
        // `saturating_sub` floors at 0, so a backward step reads as
        // "no time has passed" rather than as an enormous gap — which
        // would drive it every iteration. The quiet-tick path still
        // runs, so nothing is stuck.
        assert!(!should_drive_idle(1, 500, 1_000));
        assert!(should_drive_idle(0, 500, 1_000));
    }

    #[test]
    fn the_first_iteration_drives_it() {
        // `last_idle_ns` starts at 0, so even a busy first tick gets
        // the dispatcher onto its socket immediately rather than after
        // the first quiet moment.
        assert!(should_drive_idle(256, 1_789_776_001_000_000_000, 0));
    }
}

// ---------------------------------------------------------------
// Endpoint config — boot-time strings; never touched on hot path
// ---------------------------------------------------------------

/// Endpoint config for a single WSS ingress. All strings are owned
/// + boxed because they have to outlive the ingress thread.
///
/// It also carries the Hyperliquid `/info` endpoint (`path` =
/// `/info`) that ingress re-reads between sessions — resolved at boot
/// like the WS host, so the Hyperliquid ingress thread never runs a
/// DNS lookup.
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
    assert!(ywl::DEFAULT_TICK_RING_CAP == TICK_RING_SIZE);
    assert!(mxl::TICK_RING_CAP == TICK_RING_SIZE);
    assert!(hcl::TICK_RING_CAP == TICK_RING_SIZE);
    assert!(rwl::DEFAULT_SIGNAL_RING_CAP == SIGNAL_RING_SIZE);
};

/// All preallocated rings the engine + cli touch.
pub struct Rings {
    /// One tick ring per venue lane, indexed by `engine::tick_lane_of`
    /// (0 = Polymarket, 1 = Binance, 2 = OKX, 3 = Deribit,
    /// 4 = Hyperliquid, 5 = Bybit, 6 = MEXC, 7 = Hypercall). Lanes without a spawned
    /// ingress simply never see a producer push — the engine drains
    /// them empty.
    pub tick: [Arc<Ring<Tick, TICK_RING_SIZE>>; NUM_TICK_LANES],
    /// Signal ring for Polygon newHeads — feeds the engine.
    pub rpc_signal: Arc<Ring<Signal, SIGNAL_RING_SIZE>>,
    /// HYPARB H3b: the HyperEVM pool-event ring — feeds the engine's
    /// pool lane (`Engine::set_pool_lane`). Permanently empty when the
    /// ingress is not spawned.
    pub hyperevm_signal: Arc<Ring<Signal, { engine::POOL_RING_SIZE }>>,
    /// XMM XH1: the trade-print ring — the Hyperliquid ingress's
    /// `trades` rows, feeding the engine's trade lane
    /// (`Engine::set_trade_lane`). Permanently empty when the ingress is
    /// not spawned.
    pub trade: Arc<Ring<TradePrint, TRADE_RING_SIZE>>,
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
    /// 2 = Binance eapi, 3 = Hypercall REST summary — HC1). Producers
    /// ride into the options-capable spawns; without an options
    /// subscription the lane reads empty forever (§3.3).
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
                Ring::new(),
                Ring::new(),
            ],
            rpc_signal: Ring::new(),
            hyperevm_signal: Ring::new(),
            trade: Ring::new(),
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
                Ring::new(),
                Ring::new(),
            ],
            depth: [Ring::new(), Ring::new()],
            opt: [Ring::new(), Ring::new(), Ring::new(), Ring::new()],
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
    /// MX2: MEXC WSS thread (spot PB + futures JSON conns, one thread).
    /// Never spawned before MX6 — stays Down (the unspawned-venue
    /// shape), and stays Down after it when `[mexc]` is empty.
    pub mexc: Arc<IngressStatus>,
    /// HYPARB H3b: the HyperEVM pool-event thread (newHeads + pool
    /// logs + in-session snapshots). Down unless `--hyperevm-path` and
    /// `[hyperevm] pools` are both given.
    pub hyperevm: Arc<IngressStatus>,
    /// HC5: the Hypercall WSS thread (one public socket; data-only,
    /// O-HC1). Down unless `[hypercall]` selected a chain at boot.
    pub hypercall: Arc<IngressStatus>,
    /// HC5: the Hypercall venue counters (WS + REST poller). Venue-
    /// specific, so they ride beside the generic slot, like `hl_roll`.
    pub hc: Arc<ingress_hypercall::HcCounters>,
    /// BIN15 O2: the Hyperliquid ROLL counters. Venue-specific, so
    /// they could not live in the size-locked generic
    /// [`IngressStatus`] slot; they ride here so the metrics
    /// publisher reaches them the same way.
    pub hl_roll: Arc<ingress_hyperliquid::family::HlRollStatus>,
}

impl IngressStatusSet {
    /// Allocate all ten slots + the HL roll and Hypercall counters (boot
    /// only).
    pub fn new() -> Self {
        Self {
            polymarket: Arc::new(IngressStatus::new()),
            binance: Arc::new(IngressStatus::new()),
            okx: Arc::new(IngressStatus::new()),
            deribit: Arc::new(IngressStatus::new()),
            hyperliquid: Arc::new(IngressStatus::new()),
            bybit: Arc::new(IngressStatus::new()),
            rpc: Arc::new(IngressStatus::new()),
            mexc: Arc::new(IngressStatus::new()),
            hyperevm: Arc::new(IngressStatus::new()),
            hypercall: Arc::new(IngressStatus::new()),
            hc: Arc::new(ingress_hypercall::HcCounters::new()),
            hl_roll: Arc::new(ingress_hyperliquid::family::HlRollStatus::new()),
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
    stale_after_ms: u32,
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
            // VT2: venue default or the operator's `--stale-after-ms pm:<ms>`.
            driver.set_stale_after_ms(stale_after_ms);
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
                let session_start_ns = now_ns();

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
                    now_ns().saturating_sub(session_start_ns),
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
    stale_after_ms: u32,
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

            // VT2: the legacy single-connection lane is the spot anchor
            // (`/ws/<symbol>@bookTicker`) — it carries the aggTrade
            // sentinel too.
            let mut driver = match spot_stream_symbol(&ep.path) {
                Some(_) => bwl::Driver::new_spot_sentinel(now_ns(), sym),
                None => bwl::Driver::new(now_ns(), sym),
            };
            // venue default or the operator's `--stale-after-ms bn:<ms>`.
            driver.set_stale_after_ms(stale_after_ms);
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
                let session_start_ns = now_ns();

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
                    now_ns().saturating_sub(session_start_ns),
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
    /// Stream path (`/ws/<symbol>@bookTicker`, `/market/ws/<symbol>@markPrice`
    /// — [`bn_usdm_specs`] — or the options combined path
    /// [`bn_options_path`]).
    pub path: String,
    /// Pinned SymbolId (the M1 allocation law; 0 sentinel on an eapi
    /// slot — its syms live in the lane table).
    pub sym: core_types::SymbolId,
    /// M2.4 / BX0-F2: present ⇒ this slot is the options combined
    /// stream, carrying the boot-built table of the selected chain.
    pub eapi: Option<ingress_binance::eapi::EapiSymbolTable>,
    /// WS5: true ⇒ this slot is a `/market/ws/<sym>@markPrice` stream
    /// (the capture-only mark/index/funding lane; `sym` pinned like
    /// bookTicker). Mutually exclusive with `eapi`.
    pub mark_price: bool,
    /// VT2: true ⇒ a SPOT bookTicker slot that also subscribes
    /// `<sym>@aggTrade` on the same socket as its staleness sentinel
    /// (spot bookTicker carries no venue stamp). The stream symbol is
    /// derived from `path` (`/ws/<symbol>@bookTicker`). Only meaningful
    /// with `eapi == None && !mark_price`.
    pub spot_sentinel: bool,
}

/// VT2: the lowercase stream symbol of a `/ws/<symbol>@bookTicker`
/// path (`None` for any other shape) — the sentinel's `<symbol>@aggTrade`.
pub fn spot_stream_symbol(path: &str) -> Option<&str> {
    path.strip_prefix("/ws/")?.strip_suffix("@bookTicker")
}

/// BX0-F1: the USDⓈ-M `bookTicker` stream prefix — fstream's legacy
/// `/ws/` form, which still carries the `/public` streams (measured
/// 2026-09-23: 2 428 frames in 3 s). It moves to `/public/ws/` on a
/// measurement (K7), never on a guess.
pub const BN_USDM_BOOK_TICKER_PREFIX: &str = "/ws/";

/// BX0-F1: the USDⓈ-M `markPrice` stream prefix — fstream's ROUTED
/// `/market` path. The legacy `/ws/<sym>@markPrice` URL stopped
/// carrying `/market` streams on 2026-04-23, and it fails SILENTLY:
/// the upgrade still answers 101 and the socket then carries nothing
/// (measured 2026-09-23: 0 frames in 7 s against one per 3 s on
/// `/market/ws/`). No handshake or error tells the two apart — only
/// a frame count does, which is why the live smoke asserts one.
pub const BN_USDM_MARK_PRICE_PREFIX: &str = "/market/ws/";

/// BX0-F1: the two USDⓈ-M slots one instrument gets on `host`
/// (`BINANCE_FUT_WS_HOST`): `bookTicker` (the tick lane) and
/// `markPrice` (the capture-only mark / index / funding lane). One
/// builder for the engine boot and the live smoke, so the path the
/// smoke proves is the path the engine dials. Boot-only.
#[must_use]
pub fn bn_usdm_specs(host: &str, name: &str, sym: core_types::SymbolId) -> [BinanceConnSpec; 2] {
    [
        BinanceConnSpec {
            host: host.to_owned(),
            path: format!("{BN_USDM_BOOK_TICKER_PREFIX}{name}@bookTicker"),
            sym,
            eapi: None,
            mark_price: false,
            // USDS-M bookTicker stamps itself (T/E) — no sentinel.
            spot_sentinel: false,
        },
        BinanceConnSpec {
            host: host.to_owned(),
            path: format!("{BN_USDM_MARK_PRICE_PREFIX}{name}@markPrice"),
            sym,
            eapi: None,
            mark_price: true,
            spot_sentinel: false,
        },
    ]
}

/// BX0-F2: the options lane's combined path on fstream's routed
/// `/market` path — one `<underlying>@optionMarkPrice` stream per
/// configured underlying (lowercase). Each push is ONE array holding
/// every listed option on that underlying (mark, IV, greeks, best
/// bid/ask and the index), about once a second; the lane keeps the
/// boot-selected chain and skips the rest by table lookup
/// (`ingress_binance::eapi`). The retired nbstream `/eoptions/…`
/// `@ticker`/`@index` streams answer HTTP 404. Boot-only.
#[must_use]
pub fn bn_options_path(underlyings: &[String]) -> String {
    const HEAD: &str = "/market/stream?streams=";
    const STREAM: &str = "@optionMarkPrice";
    let mut p = String::with_capacity(HEAD.len() + underlyings.len() * (16 + STREAM.len() + 1));
    p.push_str(HEAD);
    for (i, uly) in underlyings.iter().enumerate() {
        if i > 0 {
            p.push('/');
        }
        p.push_str(&uly.to_ascii_lowercase());
        p.push_str(STREAM);
    }
    p
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
    stale_after_ms: u32,
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
            let mut conns: Vec<bwl::MultiConn<'_, TlsTransport>> = Vec::with_capacity(specs.len());
            for (i, spec) in specs.into_iter().enumerate() {
                // M2.4/BX0-F2: an options spec builds the mark-array
                // lane driver; WS5: a markPrice spec builds the mark
                // lane; bookTicker slots stay byte-identical.
                let drv = match spec.eapi {
                    Some(table) => bwl::Driver::new_eapi(now_ns().wrapping_add(i as u64), table),
                    None if spec.mark_price => {
                        bwl::Driver::new_mark_price(now_ns().wrapping_add(i as u64), spec.sym)
                    }
                    None => {
                        let seed = now_ns().wrapping_add(i as u64);
                        // VT2: a spot slot carries the aggTrade sentinel
                        // (`<symbol>@aggTrade` from the path); USDS-M
                        // stamps its bookTicker directly.
                        let mut d = match (spec.spot_sentinel, spot_stream_symbol(&spec.path)) {
                            (true, Some(_)) => bwl::Driver::new_spot_sentinel(seed, spec.sym),
                            _ => bwl::Driver::new(seed, spec.sym),
                        };
                        // one estimator per CONNECTION, same threshold.
                        d.set_stale_after_ms(stale_after_ms);
                        d
                    }
                };
                // COPY: the slot (its Driver inline, ≈ 2.9 KB) moves into
                // the Vec once at boot — see `MultiConn::new`.
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
            let mut conns: Vec<ywl::BybitConn<'_, TlsTransport>> = Vec::with_capacity(specs.len());
            for (i, spec) in specs.into_iter().enumerate() {
                let mut drv = ywl::Driver::new(
                    now_ns().wrapping_add(i as u64),
                    spec.table,
                    spec.want_tickers,
                );
                // VT2: one estimator per CONNECTION, same threshold.
                drv.set_stale_after_ms(stale_after_ms);
                // COPY: the slot (its Driver inline, 5 488 B) moves into the
                // Vec once at boot — see `BybitConn::new`.
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

/// MX6: one resolved MEXC connection spec for [`spawn_mexc`] — one
/// class (spot protobuf / futures JSON) on one socket, with its own
/// symbol table chunked to the class's measured per-socket cap
/// ([`ingress_mexc::MexcClass::symbols_per_conn`], plan §4 D1).
pub struct MexcConnSpec {
    /// Connection class — selects parser, subscribe, ping, acks.
    pub class: ingress_mexc::MexcClass,
    /// WS host (`MEXC_WS_HOST` for spot, `MEXC_FUT_WS_HOST` for
    /// futures).
    pub host: String,
    /// WS path (`/ws` spot, `/edge` futures).
    pub path: String,
    /// This connection's `SYMBOL → SymbolId` table.
    pub table: ingress_mexc::MexcSymbolTable,
    /// Futures only (ruling Q-MX3): `(sym, nextSettleTime ms,
    /// collectCycle h)` from the boot REST `funding_rate/{SYM}` calls;
    /// empty on spot.
    pub funding_seeds: Vec<(SymbolId, u64, u32)>,
}

/// MX6: spawn the MEXC ingress thread — N single-class connections
/// (spot PB + futures JSON, on two different hosts) on ONE thread, ONE
/// tick producer, ONE event producer (single-writer law), one
/// `"mexc"` capture. `ingress_mexc::run_multi` owns the in-thread
/// reconnect pacing + the WS2 establishment budget; the bybit shape
/// otherwise (see [`spawn_bybit`]).
#[allow(clippy::too_many_arguments)]
pub fn spawn_mexc(
    specs: Vec<MexcConnSpec>,
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
        PmlrCapture::open(run_dir, "mexc", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "mexc", VenueId::Mexc.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name(format!("ingress-mexc-x{}", specs.len())),
        "ingress-mexc",
        move || {
            log_pin_outcome("mexc", core_id);
            let mut eps: Vec<WssEndpoint> = Vec::with_capacity(specs.len());
            let mut names: Vec<ServerName> = Vec::with_capacity(specs.len());
            for spec in &specs {
                let ep = match WssEndpoint::resolve(&spec.host, 443, &spec.path) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(error = ?e, host = %spec.host, "mexc: DNS failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                let name = match TlsTransport::server_name_from_host(&ep.host) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::error!(error = ?e, "mexc: bad server name");
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
                    tracing::error!(error = ?e, "mexc: mio init failed");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let mut conns: Vec<mxl::MexcConn<'_, TlsTransport>> = Vec::with_capacity(specs.len());
            for (i, spec) in specs.into_iter().enumerate() {
                let mut drv =
                    mxl::Driver::new(now_ns().wrapping_add(i as u64), spec.class, spec.table);
                // VT2: one estimator per CONNECTION, same threshold.
                drv.set_stale_after_ms(stale_after_ms);
                for &(sym, next_ms, cycle_h) in &spec.funding_seeds {
                    // The bin builds seeds only for syms it put in this
                    // futures table — a refusal is a wiring defect.
                    if !drv.set_funding_seed(sym, next_ms, cycle_h) {
                        tracing::error!(sym, slot = i, "mexc: funding seed refused by its driver");
                        status.set_state(IngressState::Down);
                        return;
                    }
                }
                // COPY: the slot (its Driver inline, 3 008 B) moves into the
                // Vec once at boot — see `MexcConn::new`.
                conns.push(mxl::MexcConn::new(
                    drv,
                    eps[i].host.as_bytes(),
                    eps[i].path.as_bytes(),
                    Keepalive::new(MEXC_KEEPALIVE),
                    Backoff::default_for_ingress(core_id as u64 + 1 + i as u64),
                ));
            }
            status.set_state(IngressState::Connecting);
            let res = mxl::run_multi(
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
                        tracing::warn!(error = ?e, host = %eps[i].host, slot = i, "mexc: connect failed");
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
                "mexc: multi run-loop returned"
            );
            capture.mirror_now();
            status.set_state(IngressState::Down);
        },
    ))
}

/// HC5: everything [`spawn_hypercall`] moves onto the Hypercall threads,
/// built by the bin from the HC4 discovery outcome.
pub struct HypercallSpec {
    /// WS host (`HYPERCALL_WS_HOST`); the path is `/ws`.
    pub ws_host: String,
    /// REST host (`HYPERCALL_REST_HOST`) — the `/options-summary` poller.
    pub rest_host: String,
    /// The universe: every selected option, `name → sym` (its clone
    /// goes to the poller).
    pub symbols: ingress_hypercall::HcSymbolTable,
    /// Underlying → its `hypercall-idx:<U>` sym (the index `Mark`s).
    pub underlyings: ingress_hypercall::HcUnderlyings,
    /// The underlyings the poller refreshes, in config order.
    pub summary_underlyings: Vec<String>,
    /// Each underlying's refresh period (`[hypercall] summary_every_s`).
    pub summary_every_s: u32,
    /// VT2 staleness threshold (venue default or `--stale-after-ms`).
    pub stale_after_ms: u32,
}

/// HC5: spawn the Hypercall ingress (data-only, ruling O-HC1) — TWO
/// threads over ONE capture: `ingress-hypercall` owns the public
/// socket, the tick / event / opt producers and the `"hypercall"`
/// capture; `hypercall-poller` runs the REST `/options-summary` cycle
/// (a request may block up to its deadline — never on the socket's
/// thread) and hands its rows to the ingress thread over an SPSC ring,
/// so every file and every lane keeps ONE writer (plan §3 HC3).
///
/// The poller resolves its host on its own thread: a DNS failure there
/// ends the poller (an error line, `polls_err` flat) and leaves the
/// quote stream running.
#[allow(clippy::too_many_arguments)]
pub fn spawn_hypercall(
    spec: HypercallSpec,
    tls_config: RustlsConfig,
    mut ticks: Producer<Tick, TICK_RING_SIZE>,
    mut events: Producer<ChannelEvent, EVENT_RING_SIZE>,
    mut opts: Producer<OptSummary, OPT_RING_SIZE>,
    status: Arc<IngressStatus>,
    counters: Arc<ingress_hypercall::HcCounters>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
) -> io::Result<[JoinHandle<()>; 2]> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "hypercall", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "hypercall", VenueId::Hypercall.to_u8())?;
    }
    let (handoff_tx, mut handoff_rx) =
        Ring::<OptSummary, { hcl::HANDOFF_RING_CAP }>::new().split();
    // Boot-time copy of the universe for the poller (its own thread).
    let poller_symbols = spec.symbols.clone();
    let poller_counters = counters.clone();
    let rest_host = spec.rest_host;
    let summary_underlyings = spec.summary_underlyings;
    let every_s = spec.summary_every_s;
    let poller_tls = tls_config.clone();
    let poller = spawn_or_die(
        thread::Builder::new().name("hypercall-poller".into()),
        "hypercall-poller",
        move || {
            let http = match core_net::HttpsReq::new(
                &rest_host,
                443,
                poller_tls,
                ingress_hypercall::rest::REST_HEAD_CAP,
                0,
                ingress_hypercall::rest::REST_RESP_CAP,
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(error = ?e, host = %rest_host, "hypercall: poller client failed — no summary rows this run");
                    return;
                }
            };
            let names: Vec<&[u8]> = summary_underlyings.iter().map(|u| u.as_bytes()).collect();
            let Some(mut p) = ingress_hypercall::rest::Poller::new(
                http,
                poller_symbols,
                &names,
                every_s,
                handoff_tx,
                poller_counters,
            ) else {
                tracing::error!(underlyings = names.len(), every_s, "hypercall: poller refused its targets");
                return;
            };
            tracing::info!(host = %rest_host, underlyings = names.len(), every_s, "hypercall: poller running");
            ingress_hypercall::rest::run_poller(&mut p, &SHUTDOWN);
            tracing::info!("hypercall: poller returned");
        },
    );
    let ingress = spawn_or_die(
        thread::Builder::new().name("ingress-hypercall".into()),
        "ingress-hypercall",
        move || {
            log_pin_outcome("hypercall", core_id);
            let ep = match WssEndpoint::resolve(&spec.ws_host, 443, "/ws") {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = ?e, host = %spec.ws_host, "hypercall: DNS failed");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "hypercall: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let (mut poll, mut mio_events, _token) = match new_poll() {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "hypercall: mio init failed");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let mut drv = hcl::Driver::new(now_ns(), spec.symbols, spec.underlyings);
            drv.set_stale_after_ms(spec.stale_after_ms);
            let mut conn = hcl::HcConn::new(
                drv,
                ep.host.as_bytes(),
                Backoff::default_for_ingress(core_id as u64 + 1),
            );
            let mut lanes = hcl::Lanes {
                ticks: &mut ticks,
                events: &mut events,
                // The v1 venue-event lane law: only funding flows —
                // Hypercall has none, so its events are capture-only.
                event_mask: EVENT_LANE_FUNDING,
                opts: &mut opts,
            };
            status.set_state(IngressState::Connecting);
            let res = hcl::run(
                &mut conn,
                &mut lanes,
                &mut handoff_rx,
                &mut poll,
                &mut mio_events,
                &SHUTDOWN,
                &status,
                &counters,
                &mut capture,
                || match connect_tls(&ep, &name, &tls_config) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::warn!(error = ?e, host = %ep.host, "hypercall: connect failed");
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
                "hypercall: run-loop returned"
            );
            capture.mirror_now();
            status.set_state(IngressState::Down);
        },
    );
    Ok([ingress, poller])
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
                let session_start_ns = now_ns();

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
                    now_ns().saturating_sub(session_start_ns),
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

/// P5.2: one selected Deribit option as DISCOVERY saw it — the
/// instrument name, the sym this boot allocated for it, and the venue's
/// own numeric terms: `strike` ×1e9, `expiration_timestamp` in ms, and
/// [`opt_registry::RIGHT_CALL`] / [`opt_registry::RIGHT_PUT`].
///
/// The terms ride along because the boot already parsed them out of the
/// venue's REST JSON; without them `vrp_boot::build_registry` has to
/// parse the instrument NAME back apart to recover the same three
/// numbers, which is a second law for one fact. Offline surfaces (the
/// harness, `audit-pnl`) still parse the name, because a capture's
/// options manifest carries names and nothing else.
pub type DiscoveredOption = (String, core_types::SymbolId, i64, i64, u8);

/// M2.1: append the discovered capped options chain to a Deribit
/// symbol table (after every static insert; quote-only subscription
/// rows). `pairs` comes from `boot_discovery::Outcome::deribit_options`
/// — already deterministic-ordered and ordinal-allocated. Fails fast
/// on duplicates (a chain listing an instrument twice is a venue
/// contract violation) and on the options-block cap.
pub fn extend_deribit_table_with_options(
    table: &mut ingress_deribit::DeribitSymbolTable,
    pairs: &[DiscoveredOption],
) -> Result<(), &'static str> {
    for (name, sym, ..) in pairs {
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
            // VT2: venue default or the operator's `--stale-after-ms deribit:<ms>`.
            driver.set_stale_after_ms(stale_after_ms);
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
                let session_start_ns = now_ns();

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
                    now_ns().saturating_sub(session_start_ns),
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
            // BIN15 O2 added the variant; `insert` never returns it
            // (it names a row, and insert appends one).
            Err(ingress_hyperliquid::CoinTableErr::NoSuchRow) => {
                return Err("hl: coin table row missing (unreachable from insert)");
            }
        }
    }
    Ok(table)
}

/// BIN15 O2: reserve two coin-table slots per ROLLING family and
/// build the family table.
///
/// Reserved rows come AFTER every configured coin, so the configured
/// coins keep the ordinals they always had and a family's presence
/// changes no existing sym. The slot syms come from the rolling pool
/// (`family::rolling_sym`), which is the same law
/// `core_config::universe` used to write the manifest rows — the two
/// must agree or an offline consumer reads the wrong instrument.
///
/// Fails fast on a bad key, on more families than
/// [`ingress_hyperliquid::family::HL_MAX_FAMILIES`], and on a coin
/// table that cannot hold `coins + 2 × families`.
pub fn build_hl_families(
    rolling: &[String],
    coins: &mut ingress_hyperliquid::HlCoinTable,
) -> Result<ingress_hyperliquid::family::HlFamilyTable, String> {
    use ingress_hyperliquid::family::{rolling_sym, HlFamilyTable, HL_MAX_FAMILIES};
    let mut families = HlFamilyTable::new();
    if rolling.is_empty() {
        return Ok(families);
    }
    if rolling.len() > HL_MAX_FAMILIES {
        return Err(format!(
            "hl: {} rolling families exceeds HL_MAX_FAMILIES ({HL_MAX_FAMILIES})",
            rolling.len()
        ));
    }
    let need = coins.len() + 2 * rolling.len();
    if need > ingress_hyperliquid::HL_MAX_COINS {
        return Err(format!(
            "hl: {} coins + 2 x {} rolling families = {need} rows exceeds HL_MAX_COINS ({})",
            coins.len(),
            rolling.len(),
            ingress_hyperliquid::HL_MAX_COINS
        ));
    }
    for (f, key) in rolling.iter().enumerate() {
        let (kind, underlying, period_s) = HlFamilyTable::parse_key(key.as_bytes())
            .ok_or_else(|| format!("hl: bad rolling family `{key}`"))?;
        let yes = coins
            .reserve(rolling_sym(f, 0))
            .map_err(|e| format!("hl: reserving `{key}` [yes]: {e:?}"))?;
        let no = coins
            .reserve(rolling_sym(f, 1))
            .map_err(|e| format!("hl: reserving `{key}` [no]: {e:?}"))?;
        families
            .push(
                kind,
                underlying,
                period_s,
                [yes as u8, no as u8],
                [rolling_sym(f, 0), rolling_sym(f, 1)],
            )
            .map_err(|e| format!("hl: registering `{key}`: {e:?}"))?;
    }
    Ok(families)
}

/// Spawn the Hyperliquid public-WS ingress thread (Phase 8d). One
/// thread covers every configured coin — the driver queues one
/// subscribe frame per `(channel × coin)` pair (no batch form,
/// §4.3). There is no depth flag: `l2Book` is always subscribed —
/// it feeds the §6.2 per-coin staleness monitor. See
/// [`spawn_polymarket`] for the capture-open / fail-fast contract.
///
/// `info_ep` is the `/info` endpoint (`Config::hyperliquid_api_host`,
/// resolved at boot like `ep`): a reconnect that retired a rolling
/// family re-discovers its successor there
/// ([`boot_discovery::fetch_hl_outcome_specs`]).
#[allow(clippy::too_many_arguments)]
pub fn spawn_hyperliquid(
    ep: WssEndpoint,
    info_ep: WssEndpoint,
    tls_config: RustlsConfig,
    coins: ingress_hyperliquid::HlCoinTable,
    families: ingress_hyperliquid::family::HlFamilyTable,
    roll_status: Arc<ingress_hyperliquid::family::HlRollStatus>,
    wall_anchor: core_time::WallAnchor,
    stale_after_ms: u32,
    mut producer: Producer<Tick, TICK_RING_SIZE>,
    mut event_tx: Producer<ChannelEvent, EVENT_RING_SIZE>,
    trade_tx: Producer<TradePrint, TRADE_RING_SIZE>,
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
            // VT2: venue default or the operator's `--stale-after-ms hl:<ms>`.
            driver.set_stale_after_ms(stale_after_ms);
            // BIN15 O2: rolling families. An empty table leaves every
            // path in the driver exactly as it was before BIN15.
            let has_families = !families.is_empty();
            driver.set_families(families, roll_status, wall_anchor);
            // XMM XH1: every parsed `trades` row also reaches the
            // engine's trade lane (the driver outlives reconnects).
            driver.set_trade_lane(trade_tx);
            // VM2 V2: HL funding rides AssetCtx — the lane mask carries
            // both bits (feature-engine law). BIN15 O2: with rolling
            // families configured the lane also carries the roll and the
            // MARK — a member cannot price a HIP-4 binary without the
            // strike's reference, and cannot know which instance its
            // slot holds without the roll.
            let event_mask = if has_families {
                EVENT_LANE_FUNDING
                    | EVENT_LANE_ASSET_CTX
                    | core_types::event_lane_bit(core_types::ChannelId::InstrumentRoll)
                    | core_types::event_lane_bit(core_types::ChannelId::Mark)
            } else {
                EVENT_LANE_FUNDING | EVENT_LANE_ASSET_CTX
            };
            let mut keepalive = Keepalive::new(HL_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            let mut last_rediscovery_ns: Option<u64> = None;
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                // Reconnect hygiene (2026-09-26): a family on a settled
                // or expired instance is retired here — never
                // re-subscribed (the venue drops the socket on it) — and
                // its successor re-discovered over `/info`, because
                // `outcomeMetaUpdates` replays nothing on subscribe.
                // Before the socket opens, so the `/info` round trip
                // never idles a fresh TLS session; at most once a
                // minute, so a family whose deployer stopped creating
                // instances cannot turn every reconnect into a fetch.
                let retired = driver.reset_for_reconnect(now_ns());
                if retired > 0 {
                    tracing::warn!(
                        retired,
                        "hyperliquid: families on a settled/expired instance retired before resubscribing"
                    );
                }
                let now = now_ns();
                let due = match last_rediscovery_ns {
                    Some(at) => now.saturating_sub(at) >= HL_REDISCOVERY_MIN_INTERVAL_NS,
                    None => true,
                };
                if driver.families_awaiting() > 0 && due && !shutdown_requested() {
                    last_rediscovery_ns = Some(now);
                    let bound = rediscover_hl_families(
                        &mut driver,
                        &info_ep,
                        &tls_config,
                        &mut event_tx,
                        event_mask,
                        &status,
                        &mut capture,
                    );
                    // The roll records must reach disk even if the
                    // connect that follows fails.
                    if bound > 0 {
                        capture.mirror_now();
                    }
                }
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
                let ticks_before = status.ticks_total();
                let session_start_ns = now_ns();

                let res = hwl::run(
                    &mut transport,
                    &mut driver,
                    ep.host.as_bytes(),
                    ep.path.as_bytes(),
                    &mut producer,
                    &mut event_tx,
                    event_mask,
                    &mut poll,
                    &mut events,
                    token,
                    &SHUTDOWN,
                    &status,
                    &mut keepalive,
                    &mut capture,
                );
                let session_ns = now_ns().saturating_sub(session_start_ns);
                // T1(a): name the end on the line the operator greps —
                // `res=Disconnected` alone hid a 1.2 s reconnect loop
                // for hours (2026-09-26).
                let err = status.take_last_err();
                let (acks, acks_expected) = driver.ack_progress();
                tracing::info!(
                    ?res,
                    err_site = core_metrics::err_site_name(err.site),
                    io_kind = core_metrics::io_kind_name(err.io_kind),
                    venue_code = err.venue_code as i32,
                    lived_ms = session_ns / 1_000_000,
                    acks,
                    acks_expected,
                    "hyperliquid: run-loop returned"
                );
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
                    session_ns,
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

/// Minimum spacing of the between-session `/info` re-read
/// ([`rediscover_hl_families`]): a family whose deployer stopped
/// creating instances stays `awaiting` for good, and a connect-failure
/// storm reconnects every ≤ 8 s — neither may turn into a fetch per
/// attempt.
const HL_REDISCOVERY_MIN_INTERVAL_NS: u64 = 60_000_000_000;

/// Re-discover the live instance of every rolling family a reconnect
/// retired (2026-09-26): one `/info {"type":"outcomeMeta"}` fetch —
/// the boot's own request and parser — then
/// [`hwl::Driver::rebind_dormant`], which binds without touching the
/// wire and announces each adoption as a roll. Returns how many
/// families were bound (the caller flushes their roll events).
///
/// Cold path: between sessions, only while a family awaits a
/// successor, throttled by the caller. A failed fetch leaves the
/// family awaiting; the venue's next `outcomeCreated` adopts it on a
/// healthy session, and a later reconnect asks again.
///
/// One race remains, by construction: a successor created between
/// this snapshot and the new session's `outcomeMetaUpdates` ack is
/// missed until the family's NEXT instance — at most one 15-minute
/// instance (a daily's successor is listed a day ahead).
fn rediscover_hl_families<C: core_types::Capture>(
    driver: &mut hwl::Driver,
    info_ep: &WssEndpoint,
    tls: &RustlsConfig,
    event_tx: &mut Producer<ChannelEvent, EVENT_RING_SIZE>,
    event_mask: u16,
    status: &IngressStatus,
    capture: &mut C,
) -> usize {
    let specs = match boot_discovery::fetch_hl_outcome_specs(tls, info_ep) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = e,
                awaiting = driver.families_awaiting(),
                "hyperliquid: successor re-discovery failed — retired families wait for the venue's next outcomeCreated"
            );
            return 0;
        }
    };
    let bound = driver.rebind_dormant(&specs, event_tx, event_mask, status, capture);
    tracing::info!(
        bound,
        awaiting = driver.families_awaiting(),
        candidates = specs.len(),
        "hyperliquid: successors re-discovered at reconnect"
    );
    if bound > 0 {
        log_hl_families(driver.families());
    }
    bound
}

/// One line per rolling family — its instance, or that it has none.
/// The boot binding and every reconnect re-discovery report through
/// this, so both read the same in the log.
pub fn log_hl_families(families: &ingress_hyperliquid::family::HlFamilyTable) {
    let mut f = 0usize;
    while f < families.len() {
        let Some(row) = families.get(f) else {
            break;
        };
        let underlying = core::str::from_utf8(row.underlying_bytes()).unwrap_or("?");
        if row.dormant {
            tracing::info!(
                family = f,
                underlying,
                period_s = row.period_s,
                sym_yes = row.sym[0],
                sym_no = row.sym[1],
                "hyperliquid: family dormant (no live instance)"
            );
        } else {
            tracing::info!(
                family = f,
                underlying,
                period_s = row.period_s,
                sym_yes = row.sym[0],
                sym_no = row.sym[1],
                live = row.live.outcome,
                strike_1e6 = row.live.strike_1e6,
                expiry_ns = row.live.expiry_ns,
                twap_s = row.live.twap_s,
                "hyperliquid: family live"
            );
        }
        f += 1;
    }
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
                let session_start_ns = now_ns();

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
                    now_ns().saturating_sub(session_start_ns),
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

// HYPARB H3b: one pool bound, one decimals bound, everywhere.
const _: () = assert!(
    core_config::universe::HYPEREVM_POOLS_MAX == ingress_hyperevm::HYPEREVM_MAX_POOLS
        && ingress_hyperevm::HYPEREVM_MAX_POOLS == core_fill::AMM_MAX_POOLS
);
const _: () =
    assert!(core_config::universe::HYPEREVM_DECIMALS_MAX == core_amm::payload::MAX_TOKEN_DECIMALS);

/// HYPARB H3b: the ingress's pool table from the resolved universe —
/// `[hyperevm] pools` in file order, each at its allocated symbol.
/// Boot-only (allocates).
pub fn hyperevm_pool_table(
    alloc: &core_config::universe::AllocatedUniverse,
) -> Result<ingress_hyperevm::PoolTable, ingress_hyperevm::PoolTableErr> {
    use core_config::universe::HyperEvmFamily;
    let mut entries = Vec::with_capacity(alloc.hyperevm_pools.len());
    let mut i = 0usize;
    while i < alloc.hyperevm_pools.len() {
        let p = alloc.hyperevm_pools[i];
        entries.push(ingress_hyperevm::PoolEntry {
            address: p.address,
            sym: alloc.hyperevm[i].sym,
            family: match p.family {
                HyperEvmFamily::V3 => ingress_hyperevm::PoolFamily::UniswapV3,
                HyperEvmFamily::Slipstream => ingress_hyperevm::PoolFamily::Slipstream,
                HyperEvmFamily::Algebra => ingress_hyperevm::PoolFamily::Algebra,
            },
            dec0: p.dec0,
            dec1: p.dec1,
        });
        i += 1;
    }
    ingress_hyperevm::PoolTable::new(&entries)
}

/// HYPARB H3b: spawn the HyperEVM pool-event thread — `spawn_rpc`'s
/// shape, plus a tap venue byte (this source HAS a `VenueId`), its own
/// pool table and snapshot radius, and one extra exit: an endpoint that
/// fails the archive probe (O-H4) is not reconnected to. Per O-H15 that
/// disables the pool member (no pool is ever judgeable), never the
/// engine.
#[allow(clippy::too_many_arguments)]
pub fn spawn_hyperevm(
    ep: WssEndpoint,
    tls_config: RustlsConfig,
    mut producer: Producer<Signal, { engine::POOL_RING_SIZE }>,
    status: Arc<IngressStatus>,
    core_id: usize,
    run_dir: &Path,
    epoch_ns: u64,
    tap_cfg: TapCfg,
    capture_metrics: CaptureMetrics,
    pools: ingress_hyperevm::PoolTable,
    radius: i32,
) -> io::Result<JoinHandle<()>> {
    let mut capture = GaugedCapture::new(
        PmlrCapture::open(run_dir, "hyperevm", epoch_ns, tap_cfg)?,
        capture_metrics,
    );
    if tap_cfg.mode != TapMode::Off {
        capture.set_tap_venue_byte(run_dir, "hyperevm", VenueId::HyperEvm.to_u8())?;
    }
    Ok(spawn_or_die(
        thread::Builder::new().name("ingress-hyperevm".into()),
        "ingress-hyperevm",
        move || {
            log_pin_outcome("hyperevm", core_id);
            let server_name = match TlsTransport::server_name_from_host(&ep.host) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = ?e, "hyperevm: bad server name");
                    status.set_state(IngressState::Down);
                    return;
                }
            };
            let mut driver = hel::Driver::new(now_ns(), pools, radius);
            let mut keepalive = Keepalive::new(HYPEREVM_KEEPALIVE);
            let mut backoff = Backoff::default_for_ingress(core_id as u64 + 1);
            while !shutdown_requested() {
                status.set_state(IngressState::Connecting);
                let mut transport = match connect_tls(&ep, &server_name, &tls_config) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "hyperevm: connect failed");
                        status.set_state(IngressState::Backoff);
                        sleep_backoff(&mut backoff);
                        continue;
                    }
                };
                let (mut poll, mut events, token) = match new_poll() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = ?e, "hyperevm: mio init failed");
                        status.set_state(IngressState::Down);
                        return;
                    }
                };
                driver.reset_for_reconnect(now_ns());
                let ticks_before = status.ticks_total();
                let session_start_ns = now_ns();
                let res = hel::run(
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
                tracing::info!(?res, "hyperevm: run-loop returned");
                capture.mirror_now();
                match res {
                    hel::RunResult::Stopped => {
                        status.set_state(IngressState::Down);
                        return;
                    }
                    hel::RunResult::ArchiveDishonest => {
                        tracing::error!(
                            host = %ep.host,
                            "hyperevm: the endpoint answers historical eth_call with LATEST \
                             state (O-H4 archive probe) — ingress stopped; the pool member \
                             stays dark (O-H15), the engine runs on"
                        );
                        status.set_state(IngressState::Down);
                        return;
                    }
                    _ => {}
                }
                if should_reset_backoff(
                    status.ticks_total(),
                    ticks_before,
                    now_ns().saturating_sub(session_start_ns),
                    matches!(res, hel::RunResult::IdleTimeout),
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
/// pair flags plus each discovery-gated venue table, plus (MX6,
/// operator ruling Q-MX6) every MEXC instrument the boot allocated
/// (spot + perp; Bybit stays out by its own WS9 precedent), plus
/// (ruling O-HC17) every Hypercall option the boot selected — its
/// `hypercall-idx:<U>` index syms stay out (capture-only `Mark`s, caps
/// 0: nothing could read them) — **sorted strict-ascending and
/// deduped** (binary-searched per §4.2 rule-6 check;
/// `RulesetSidePath::new` debug-asserts the ordering). Data-only venues
/// (MEXC, Hypercall) enter as signal or reference legs: an order on one
/// stays unroutable in the paper matcher and the harness.
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
    mexc_syms: &[SymbolId],
    hypercall_option_syms: &[SymbolId],
) -> Arc<[u32]> {
    let mut v: Vec<u32> = Vec::with_capacity(
        polymarket_syms.len()
            + binance_syms.len()
            + okx.map_or(0, |t| t.len())
            + deribit.map_or(0, |t| t.len())
            + hl.map_or(0, |t| t.len())
            + mexc_syms.len()
            + hypercall_option_syms.len(),
    );
    v.extend_from_slice(polymarket_syms);
    v.extend_from_slice(binance_syms);
    v.extend_from_slice(mexc_syms);
    v.extend_from_slice(hypercall_option_syms);
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
        // its table to the engine through the §6 ring — `try_push_ref` of
        // the scratch (documented 32 KiB copy #1, operator cadence);
        // push-full ⇒ reject, counted (`table_push_fail`). The
        // consumer half parks in the bin until item 7 wires the
        // engine drain.
        let mut side_path = RulesetSidePath::new(
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
    /// MX6: tap config for the MEXC ingress (spot PB frames are
    /// tapped as the raw BINARY payload bytes).
    pub mexc: TapCfg,
    /// HYPARB H3b: tap config for the HyperEVM ingress.
    pub hyperevm: TapCfg,
    /// HC5: tap config for the Hypercall ingress (every WS text
    /// payload; the REST poller's bodies are not tapped).
    pub hypercall: TapCfg,
}

/// Parse `--raw-tap <CSV|all>` + `--raw-tap-mode <rejects|all>` +
/// `--raw-tap-budget-mb <u64>` into a [`RawTapConfig`]. `raw_tap`
/// absent/empty ⇒ every venue gets [`TapCfg::off`] (default: none).
/// `raw_tap` equal (after trim) to the literal `all` enables every
/// venue; otherwise it's a comma-separated list of venue labels
/// (`pm`/`bn`/`okx`/`rpc`/`deribit`/`hl`/`bybit`/`mexc`/`hyperevm`/`hypercall`), trimmed, non-empty, no
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
        mexc: TapCfg::off(),
        hyperevm: TapCfg::off(),
        hypercall: TapCfg::off(),
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
        cfg.mexc = enabled_cfg;
        cfg.hyperevm = enabled_cfg;
        cfg.hypercall = enabled_cfg;
        return Ok(cfg);
    }

    let mut seen: [&str; 10] = [""; 10];
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
            "mexc" => cfg.mexc = enabled_cfg,
            "hyperevm" => cfg.hyperevm = enabled_cfg,
            "hypercall" => cfg.hypercall = enabled_cfg,
            _ => return Err("--raw-tap: unknown venue label"),
        }
    }
    Ok(cfg)
}

// ---------------------------------------------------------------
// Consumer ends (the engine drains every ring)
// ---------------------------------------------------------------

/// Consumer-side handles passed to the engine. Created
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
    /// HYPARB H3b: HyperEVM pool-event consumer (the engine's pool lane).
    pub hyperevm_signal: Consumer<Signal, { engine::POOL_RING_SIZE }>,
    /// XMM XH1: trade-print consumer (the engine's trade lane). Reads
    /// empty forever when the Hyperliquid ingress is not spawned.
    pub trades: Consumer<TradePrint, TRADE_RING_SIZE>,
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
    /// immediately before the AI-cmd drain each iteration and lends
    /// each slot in place to `Strategy::on_ruleset_table` (→ the set's
    /// vm member, whose copy into its staged buffer is documented copy
    /// #2). Reads empty forever when `ingress-ai` is
    /// not spawned (producer dropped — §3.3 unspawned shape); on
    /// non-set strategy paths the pops land on the trait's default
    /// no-op, mirroring how `on_ai` behaves on bare strategies.
    pub ruleset_tables: Consumer<RuleTableSlot, RULE_TABLE_RING_SLOTS>,
}

// ---------------------------------------------------------------
// Engine loop — real strategy wired to the dispatcher
// ---------------------------------------------------------------

/// Slot capacity for the standalone strategy tables (ev, rule-tree).
/// Holds at most `N` symbol pairs (one Polymarket book + one Binance
/// reference per pair). `8` is plenty for v1; bump and recompile when
/// we widen coverage.
pub const STRATEGY_SLOTS: usize = 8;

/// A symbol pair registered with the standalone members at boot.
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

/// Default trigger threshold (1e6 fixed-point) — the value the
/// unlinked latency-arb member exported (HYPARB H0, O-H1: its numbers
/// stay, its crate leaves the cli graph).
const DEFAULT_THRESHOLD_1E6: i64 = 20_000;
/// Default per-order quantity (1e6 fixed-point) — 10 units.
const DEFAULT_QTY_1E6: i64 = 10_000_000;
/// Default per-market cooldown between emits (ns) — 250 ms.
const DEFAULT_COOLDOWN_NS: u64 = 250_000_000;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            pairs: Vec::new(),
            threshold_1e6: DEFAULT_THRESHOLD_1E6,
            qty_1e6: DEFAULT_QTY_1E6,
            cooldown_ns: DEFAULT_COOLDOWN_NS,
        }
    }
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
/// was provided — vrp when `vrp.toml` resolves, xsd / bin15 / xmm
/// when their artifacts resolve, slot 0 when `hyparb.toml` resolves
/// (HYPARB H5 — the member is configured only by its own boot
/// artifact; unconfigured it refuses `on_start`, so it never boots
/// inert under a healthy-looking name),
/// **ai-exec and vm unconditionally** (neither has
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
    mut disp: D,
    obs: Observability,
    requested_mask: u8,
    vrp: Option<&crate::vrp_boot::VrpBoot>,
    xsd: Option<&crate::xsd_boot::XsdBoot>,
    bin15: Option<&crate::bin15_boot::Bin15Boot>,
    xmm: Option<&crate::xmm_boot::XmmBoot>,
    regime: Option<&RegimeBoot>,
    hyparb: Option<&crate::hyparb_boot::HyparbBoot>,
    har: Option<&crate::har_boot::HarBoot>,
) -> EngineLoopResult {
    let mut configured = strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM;
    if hyparb.is_some() {
        configured |= strategy_set::BIT_HYPARB;
    }
    if vrp.is_some() {
        configured |= strategy_set::BIT_VRP;
    }
    if xsd.is_some() {
        configured |= strategy_set::BIT_XSD;
    }
    if bin15.is_some() {
        configured |= strategy_set::BIT_BIN15;
    }
    if xmm.is_some() {
        configured |= strategy_set::BIT_XMM;
    }
    let mask = requested_mask & configured;
    if mask == 0 {
        return EngineLoopResult::Failed("engine_loop_set: no requested member is configured");
    }
    // RG6: the masks the `/state` `boot` section reports.
    let mut obs = obs;
    obs.boot.requested_mask = requested_mask;
    obs.boot.configured_mask = configured;
    if let Some(boot) = xmm {
        // XMM XH3: the artifact's identity on `/state` (`boot.xmm_hash`)
        // and the names of its perp rows, in row order (boot-only).
        obs.boot.xmm_hash = boot.hash;
        obs.boot.set_xmm_coins(boot.coins.join(",").as_bytes());
    }

    let mut set = strategy_set::StrategySet::new(mask);
    if let Some(boot) = vrp {
        // VRP V7: the wall anchor is taken HERE, once, right before the
        // engine loop starts — the member maps every tick's monotonic
        // stamp to wall time through it, and every expiry instant it
        // compares against is wall.
        let anchor = core_time::WallAnchor::now();
        if let Err(e) = set.vrp_mut().configure(
            boot.params,
            boot.registry.clone(),
            boot.underlying_sym,
            boot.hedge_sym,
            anchor,
            boot.hash,
        ) {
            tracing::error!(error = ?e, "vrp: artifact refused");
            return EngineLoopResult::Failed("vrp: artifact refused by the strategy");
        }
        // F2: the MERGED pairs — the worker's seed unioned with the
        // engine's own `P` rows by expiry, state winning. `restore_state`
        // below counts the file's `P` rows and pushes none of them; the
        // two unconditional pushes put 38 expiries into the OLS twice at
        // every boot (`seed_pairs=90 … pairs=90 … total_pairs=128`).
        for (ts_ms, x, y) in &boot.pairs {
            set.vrp_mut().seed_pair_at(*ts_ms, *x, *y);
        }
        // W2/W3: the HAR's rolling window, reconciled at boot from the
        // worker's candle cut and the engine's own last state. Replayed
        // BEFORE the first live minute close, because the ring's
        // eviction arm assumes chronological order.
        //
        // Without this the member starts every boot with `minutes = 0`
        // against a 24 h warm-up, and the restart lane fires five times
        // a UTC day with a longest gap of 7 h 35 m — so the forecast
        // never exists and the member never trades. That is what the
        // first live campaign did on 2026-09-11.
        let seeded_minutes = set.vrp_mut().seed_returns(&boot.window);
        // V8a: the engine's OWN history on top of the worker's
        // bootstrap — the pairs it formed itself, the QLIKE window kill
        // criterion 3 is measured over, and any campaign that was open
        // when the process went down. A state file the engine cannot
        // read exactly is a position nobody is tracking, so a malformed
        // one refuses the boot.
        let restored = match boot.state.as_deref() {
            Some(text) => match set.vrp_mut().restore_state(text) {
                Ok(r) => r,
                Err(reason) => {
                    tracing::error!(
                        reason,
                        path = %boot.state_path.display(),
                        "vrp: state refused"
                    );
                    return EngineLoopResult::Failed("vrp: state file refused by the strategy");
                }
            },
            None => strategy_vrp::VrpRestored::default(),
        };
        tracing::info!(
            hash = %format_hex32(&boot.hash),
            chain_rows = boot.registry.len(),
            chain_rows_refused = boot.rows_refused,
            // P5.2: 0 on a live boot — the venue's own strike and
            // expiry travel with the sym. Non-zero means discovery
            // dropped fields it used to carry and the chain was rebuilt
            // by parsing instrument names.
            chain_rows_from_name = boot.rows_from_name,
            seed_pairs = set.vrp().n_pairs(),
            tau_ns = boot.params.tau_ns,
            theta_1e9 = boot.params.theta_1e9,
            "vrp: artifact configured"
        );
        // R1/R2: the EXECUTION modes, printed on every boot. A member
        // that rests its entry behaves visibly differently from one
        // that crosses, and an operator reading a campaign back has to
        // be able to tell which one produced it.
        tracing::info!(
            entry_mode = match boot.params.entry_mode {
                strategy_vrp::ENTRY_MODE_MAKER => "maker",
                _ => "ioc",
            },
            entry_patience_ns = boot.params.entry_patience_ns,
            entry_fallback = match boot.params.entry_fallback {
                strategy_vrp::ENTRY_FALLBACK_CROSS => "cross",
                _ => "abandon",
            },
            hedge_mode = match boot.params.hedge_mode {
                strategy_vrp::HEDGE_MODE_MAKER => "maker",
                _ => "taker",
            },
            hedge_patience_ns = boot.params.hedge_patience_ns,
            band_qty_1e6 = boot.params.band_qty_1e6,
            opt_fee_index_bps = boot.params.opt_fee_index_bps,
            opt_fee_prem_bps = boot.params.opt_fee_prem_bps,
            "vrp: execution modes"
        );
        // R3: the four effective log-vol intercepts. An all-zero table
        // is bit-identical to no table at all, so the tell is the only
        // way to tell a loaded correction from a missing one.
        tracing::info!(
            regime_fast_vol_low_1e9 =
                boot.params.regime_off_1e9[0][core_types::regime::VOL_LOW as usize],
            regime_fast_vol_high_1e9 =
                boot.params.regime_off_1e9[0][core_types::regime::VOL_HIGH as usize],
            regime_slow_vol_low_1e9 =
                boot.params.regime_off_1e9[1][core_types::regime::VOL_LOW as usize],
            regime_slow_vol_high_1e9 =
                boot.params.regime_off_1e9[1][core_types::regime::VOL_HIGH as usize],
            "vrp: regime intercepts"
        );
        tracing::info!(
            seed_pairs = boot.pairs_from_seed + boot.pairs_from_state,
            pairs = boot.pairs.len(),
            pairs_from_seed = boot.pairs_from_seed,
            pairs_from_state = boot.pairs_from_state,
            decisive = boot.pairs.len() >= core_vol::MIN_PAIRS,
            path = %boot.seed_path.display(),
            "vrp: seed applied"
        );
        tracing::info!(
            pairs = restored.pairs,
            qlike = restored.qlike,
            campaign = restored.campaign,
            campaign_resolved = restored.campaign_resolved,
            killed = restored.killed,
            returns = restored.returns,
            prev_px_1e6 = restored.prev_px_1e6,
            gaps = set.vrp().vol_gaps(),
            total_pairs = set.vrp().n_pairs(),
            path = %boot.state_path.display(),
            "vrp: state restored"
        );
        // W2: the warm-up state, on EVERY boot. The member spent its
        // whole first live day cold and said nothing — `no_bounds` was
        // the only tell, and it is indistinguishable from every other
        // cause.
        if set.vrp().vol_is_warm() {
            tracing::info!(
                minutes = set.vrp().vol_minutes(),
                seeded = seeded_minutes,
                from_seed = boot.window_from_seed,
                from_state = boot.window_from_state,
                last_min_ts_ms = set.vrp().vol_last_min_ts_ms(),
                "vrp: forecast WARM"
            );
        } else {
            tracing::warn!(
                minutes = set.vrp().vol_minutes(),
                need = core_vol::HAR_WARM_MINUTES,
                short_by = set.vrp().vol_short_by(),
                seeded = seeded_minutes,
                from_seed = boot.window_from_seed,
                from_state = boot.window_from_state,
                "vrp: forecast COLD — every decision refuses with no_bounds until the \
                 window fills. Re-cut the seed if this does not clear."
            );
        }
        if restored.campaign && !restored.campaign_resolved {
            tracing::warn!(
                "vrp: the restored campaign's contract is no longer in the chain — the \
                 settle rung will close it out at the first index"
            );
        }
        obs.vrp_state_path = Some(boot.state_path.clone());
        if restored.killed {
            tracing::warn!(
                "vrp: kill criterion 3 was ARMED before this restart — the member will not \
                 enter. Investigate before clearing the state file."
            );
        }
    }
    if let Some(boot) = bin15 {
        // BIN15 O4b. The wall anchor is taken HERE, once, right before
        // the engine loop starts — the minute grid is UTC-aligned from
        // this instant on, and replay rebuilds the same anchor from the
        // harness rebase (the ICDP I2 precedent, and the same reason O2
        // threads a `WallAnchor` into the HL driver: the venue's expiry
        // is an EPOCH instant and `now_ns()` is not).
        let anchor = core_time::WallAnchor::now();
        // The LUT box moves into the member, so it is cloned out of the
        // boot struct here — 17 KiB, once, at boot.
        let luts = Box::new((*boot.luts).clone());
        if let Err(e) = set.bin15_mut().configure(boot.params, luts, anchor) {
            tracing::error!(error = %e, "bin15: configure failed");
            return EngineLoopResult::Failed("engine_loop_set: bin15 configure rejected");
        }
        let mut u = 0usize;
        while u < boot.seeds.len() {
            let seed = &boot.seeds[u];
            // The minute window is a property of the PRICE SERIES, so
            // both forecast engines replay it. Replayed before any
            // pair, because the ring's eviction arm assumes
            // chronological order.
            set.bin15_mut().seed_returns(u, &seed.returns);
            // The pairs are PER TENOR. Pushing one cloud into both —
            // which this did until O4b — fits the daily line on the 15 m
            // regressor, and both being log-vols of the same series is
            // exactly why nothing downstream would notice.
            set.bin15_mut()
                .seed_pairs(u, strategy_bin15::FAMILY_OUT_15M, &seed.pairs_15m);
            set.bin15_mut()
                .seed_pairs(u, strategy_bin15::FAMILY_NATIVE_DAILY, &seed.pairs_daily);
            u += 1;
        }
        let dormant = set.bin15_mut().counters().families_dormant as usize;
        tracing::info!("{}", crate::bin15_boot::render_boot_tell(boot, dormant));
    }
    if let Some(boot) = hyparb {
        // HYPARB H5. The wall anchor is taken HERE, once — the member's
        // day cap is UTC-aligned from this instant on (the icdp law).
        // The params are cloned out of the boot struct (a few KiB, once).
        let anchor = core_time::WallAnchor::now();
        if let Err(e) = set.hyparb_mut().configure(boot.params.clone(), anchor) {
            tracing::error!(error = %e, "hyparb: configure failed");
            return EngineLoopResult::Failed("engine_loop_set: hyparb configure rejected");
        }
        tracing::info!("{}", crate::hyparb_boot::render_boot_tell(boot));
    }
    if let Some(boot) = xmm {
        // XMM XH1: the member validates and stores its artifact. It
        // quotes on the paper queue law since XH2 (a live slot 6 refuses
        // the boot until XH4).
        if let Err(e) = set.xmm_mut().configure(&boot.params) {
            tracing::error!(error = %e, "xmm: artifact refused");
            return EngineLoopResult::Failed("xmm: artifact refused by the strategy");
        }
        // XMM XH2: the queue law tracks the member's perps from boot, so
        // the first post-only order on each meets a known book.
        let mut i = 0usize;
        while i < boot.params.n_perps as usize {
            disp.track_queue_sym(boot.params.perps[i].hl_sym);
            i += 1;
        }
        tracing::info!("{}", crate::xmm_boot::render_boot_tell(boot));
    }
    if let Some(boot) = xsd {
        // XSD-3: the wall anchor is taken HERE, once — the member's hour
        // grid is UTC-aligned from this instant on (the icdp law). The
        // seed fills buckets strictly before the boot hour (the bundle
        // dropped anything else); the state restores under the same
        // table hash or flattens under a changed one.
        let anchor = core_time::WallAnchor::now();
        if let Err(e) = set.xsd_mut().configure(anchor, &boot.params, &boot.table) {
            tracing::error!(error = %e, "xsd: artifact refused");
            return EngineLoopResult::Failed("xsd: artifact refused by the strategy");
        }
        let mut i = 0usize;
        while i < boot.seed.len() {
            let (sym, hour, close) = boot.seed[i];
            set.xsd_mut().seed_close(sym, hour, close);
            i += 1;
        }
        let mut restored = 0usize;
        let mut r = 0usize;
        while r < boot.restore.len() {
            if let Err(reason) = set.xsd_mut().restore_position(&boot.restore[r], boot.restore_flatten) {
                tracing::error!(reason, "xsd: state file refused by the strategy");
                return EngineLoopResult::Failed("xsd: state file refused by the strategy");
            }
            restored += 1;
            r += 1;
        }
        let x = set.xsd();
        let c = *x.counters();
        tracing::info!(
            hash = %format_hex32(x.params_hash()),
            table_hash = %format_hex32(x.table_hash()),
            targets = x.targets(),
            pairs = x.pairs(),
            syms = x.syms(),
            rows_dropped = boot.rows_dropped,
            seed_rows = c.seed_rows,
            seed_dropped = c.seed_dropped + boot.seed_dropped as u64,
            z_window_h = boot.params.z_window_h,
            grid_n = boot.params.grid_n,
            anchor_wall_ns = anchor.wall_ns,
            params = %boot.params_path.display(),
            table = %boot.table_path.display(),
            seed = %boot.seed_path.display(),
            "xsd: artifact configured"
        );
        if !boot.state_present {
            tracing::info!(path = %boot.state_path.display(), "xsd: no state — first boot under this table");
        } else if boot.restore_flatten {
            tracing::warn!(
                positions_to_flatten = restored,
                path = %boot.state_path.display(),
                "xsd: state discarded (table hash changed) — every listed position exits at its first fresh tick"
            );
        } else {
            tracing::info!(positions = restored, path = %boot.state_path.display(), "xsd: state restored");
        }
        obs.xsd_state = Some(XsdStateSink {
            path: boot.state_path.clone(),
            table_hash: boot.table.hash,
            descriptors: boot.descriptors.clone(),
        });
    }
    // HAR H3.4: the long-tenor HAR series — configured, restored and told
    // here, beside the regime detector (same seat, same 1 s poll). No
    // member reads them (plan law L4); a refusal turns the service off and
    // never the boot (`har_boot`'s failure isolation). The anchor is taken
    // here, once, like every other member's minute grid.
    if let Some(hb) = har {
        let anchor = core_time::WallAnchor::now();
        if crate::har_boot::install(&mut set, hb, anchor, core_time::now_ns()) {
            obs.har_state_paths = hb.series.iter().map(|s| s.state_path.clone()).collect();
            // H3.5: the file's identity and what it lost, for `/state.har`.
            obs.har_hash = hb.hash;
            obs.har_dropped = hb.dropped.len().min(u32::MAX as usize) as u32;
            // H3.7: the state leaves the engine thread — one copy into a
            // mailbox at each day close, the render and the fsync on the
            // writer thread. A spawn failure keeps the write on the loop
            // (logged, never a refusal — the boot's failure isolation).
            let (tx, rx) = crate::har_writer::outbox(hb.series.len());
            let names = hb.series.iter().map(|s| s.name.clone()).collect();
            match crate::har_writer::HarWriter::spawn(rx, names, obs.har_state_paths.clone()) {
                Ok(w) => {
                    set.install_har_outbox(tx);
                    obs.har_writer = Some(w);
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "har: the state writer thread did not spawn — the engine loop writes the state"
                ),
            }
        }
    }
    // RG2 (plan §4.2–§4.3): the regime detector — configure, apply the
    // `[labels.*]` overrides, seed, and print the boot tells. An
    // absent artifact leaves it unconfigured: every word UNKNOWN,
    // every unconstrained member open (today's behaviour).
    match regime {
        Some(rb) => {
            let anchor = core_time::WallAnchor::now();
            let now = core_time::now_ns();
            if let Err(e) = set.configure_regime(&rb.params, anchor, now) {
                tracing::error!(error = ?e, "regime: artifact refused by the detector");
                return EngineLoopResult::Failed("regime: artifact refused by the detector");
            }
            for (slot, label) in &rb.labels {
                if set.set_regime_label(*slot, *label) {
                    tracing::info!(
                        slot,
                        terms = label.n,
                        off = label.off,
                        "regime: label override applied"
                    );
                } else {
                    tracing::warn!(
                        slot,
                        "regime: label override refused (slot cannot be relabelled)"
                    );
                }
            }
            // RG8 (operator ruling 2026-09-05): with `[labels] require = 1`
            // an enabled signal-carrying coded member must carry a label
            // — an ANY member would trade in every regime and the gate
            // would be a no-op for it. Fail-fast, before the seed.
            if let Some(slot) = unlabelled_required_slot(&set, mask, rb.require_labels) {
                tracing::error!(
                    slot,
                    member = engine_snapshot::SLOT_NAMES[slot as usize],
                    "regime: [labels] require = 1 but this ENABLED member carries no label (ANY) — \
                     add [labels.<member>] to regime.toml or drop it from the mask"
                );
                return EngineLoopResult::Failed(
                    "regime: an enabled coded member has no label ([labels] require = 1)",
                );
            }
            let applied = set.seed_regime(&rb.seed, now);
            let c = strategy_core::StrategyCounters::regime_counters(&set);
            tracing::info!(
                hash = %format_hex32(&rb.hash),
                members = rb.params.n_members,
                confirm_min = rb.params.confirm_min,
                require_labels = rb.require_labels,
                seed_rows = applied,
                seed_file_rows = rb.seed.len(),
                fast = %format_hex16(c.effective[0].0),
                slow = %format_hex16(c.effective[1].0),
                gates = ?c.gates,
                "regime: artifact configured"
            );
            if rb.seed.is_empty() {
                tracing::info!("regime: seed absent — profiles warm live");
            }
        }
        None => tracing::info!(
            "regime: no artifact — detector unconfigured (words UNKNOWN, gates open)"
        ),
    }
    tracing::info!(
        mask,
        hyparb = mask & strategy_set::BIT_HYPARB != 0,
        vrp = mask & strategy_set::BIT_VRP != 0,
        xsd = mask & strategy_set::BIT_XSD != 0,
        bin15 = mask & strategy_set::BIT_BIN15 != 0,
        ai_exec = mask & strategy_set::BIT_AI_EXEC != 0,
        vm = mask & strategy_set::BIT_VM != 0,
        xmm = mask & strategy_set::BIT_XMM != 0,
        "strategy-set: composed"
    );
    run_engine_loop(cons, disp, set, obs)
}

/// Lower-hex render of a 32-byte hash (boot log; cold path).
fn format_hex32(h: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 16-hex-digit render of a regime word (boot log; cold path).
fn format_hex16(w: u64) -> String {
    format!("{w:016x}")
}

/// RG2: everything the set needs to boot its regime detector — built
/// by the bin from `regime.toml` + the seed file with every descriptor
/// resolved against the boot universe (the icdp precedent). Boot-only;
/// allocation is fine.
pub struct RegimeBoot {
    /// Resolved detector parameters.
    pub params: core_regime::RegimeParams,
    /// `[labels.<member>]` overrides as `(slot, set)`.
    pub labels: Vec<(u8, core_types::RegimeLabelSet)>,
    /// Resolved seed rows (empty = seed absent).
    pub seed: Vec<core_regime::SeedRow>,
    /// SHA-256 of the artifact bytes (boot log / `/state`).
    pub hash: [u8; 32],
    /// RG8: `[labels] require = 1` — refuse to boot an enabled
    /// signal-carrying coded member whose label is ANY.
    pub require_labels: bool,
}

/// RG8: the coded members the `[labels] require` law covers — the ones
/// that carry their OWN signal (slots 0–3 + xmm, slot 6). `ai-exec` (slot 4) is
/// exempt: it carries the worker's intent lanes, which gate themselves
/// through `regime_allows()` (their `REGIME_LABEL`); the vm (slot 5) is
/// held to the law upstream — every row of a staged table must be
/// labelled (the worker's RG8 gates), never at boot.
const REQUIRE_LABEL_SLOTS: [u8; 5] = [
    strategy_set::SLOT_HYPARB,
    strategy_set::SLOT_VRP,
    strategy_set::SLOT_XSD,
    strategy_set::SLOT_BIN15,
    strategy_set::SLOT_XMM,
];

/// RG8: the first enabled, label-required slot whose label set is ANY —
/// `None` when the law holds (or is off).
fn unlabelled_required_slot(set: &strategy_set::StrategySet, mask: u8, require: bool) -> Option<u8> {
    if !require {
        return None;
    }
    let mut i = 0usize;
    while i < REQUIRE_LABEL_SLOTS.len() {
        let slot = REQUIRE_LABEL_SLOTS[i];
        if mask & (1u8 << slot) != 0 && set.regime_label_of(slot).n == 0 {
            return Some(slot);
        }
        i += 1;
    }
    None
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
    /// Build the registry + the `/state` snapshot cell (both `None`
    /// unless `enable_metrics`). Boot-only; allocates once.
    /// `exec_modes` — E1: the per-slot `ExecMode` bytes when an
    /// `exec.toml` is in force, `None` when there is no `--exec`.
    /// `None` registers NOTHING, which is what keeps `/metrics`
    /// byte-identical to a pre-E1 binary's on an unconfigured boot.
    pub fn build(
        enable_metrics: bool,
        exec_modes: Option<[u8; clob_dispatcher::EXEC_COUNTER_SLOTS]>,
    ) -> Result<Self, &'static str> {
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
            let strategy_hyparb = reg
                .register_gauge("engine_strategy_hyparb_active")
                .map_err(|_| "register engine_strategy_hyparb_active")?;
            let strategy_vrp = reg
                .register_gauge("engine_strategy_vrp_active")
                .map_err(|_| "register engine_strategy_vrp_active")?;
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
            // BIN15 O2: the rolling families' own tells. `rolls_total`
            // should step once per family per period; `ignored` counts
            // the other deployers' markets on the shared lifecycle
            // channel and is expected to be large.
            let ingress_hl_rolls = reg
                .register_gauge("engine_ingress_hyperliquid_rolls_total")
                .map_err(|_| "register engine_ingress_hyperliquid_rolls_total")?;
            let ingress_hl_rolls_ignored = reg
                .register_gauge("engine_ingress_hyperliquid_rolls_ignored_unmatched_total")
                .map_err(|_| "register engine_ingress_hyperliquid_rolls_ignored_unmatched_total")?;
            let ingress_hl_family_ack_timeouts = reg
                .register_gauge("engine_ingress_hyperliquid_family_ack_timeouts_total")
                .map_err(|_| "register engine_ingress_hyperliquid_family_ack_timeouts_total")?;
            let ingress_hl_families_dormant = reg
                .register_gauge("engine_ingress_hyperliquid_families_dormant")
                .map_err(|_| "register engine_ingress_hyperliquid_families_dormant")?;
            // BIN15 O8: outcome legs' one-sided `bbo` pushes, dropped by
            // policy — their own count, out of `parse_errors_total`.
            let ingress_hl_outcome_bbo_one_sided = reg
                .register_gauge("engine_ingress_hyperliquid_outcome_bbo_one_sided_total")
                .map_err(|_| "register engine_ingress_hyperliquid_outcome_bbo_one_sided_total")?;
            let ingress_bybit_state = reg
                .register_gauge("engine_ingress_bybit_state")
                .map_err(|_| "register engine_ingress_bybit_state")?;
            let ingress_rpc_state = reg
                .register_gauge("engine_ingress_rpc_state")
                .map_err(|_| "register engine_ingress_rpc_state")?;
            let ingress_mexc_state = reg
                .register_gauge("engine_ingress_mexc_state")
                .map_err(|_| "register engine_ingress_mexc_state")?;
            let ingress_hyperevm_state = reg
                .register_gauge("engine_ingress_hyperevm_state")
                .map_err(|_| "register engine_ingress_hyperevm_state")?;
            let ingress_hypercall_state = reg
                .register_gauge("engine_ingress_hypercall_state")
                .map_err(|_| "register engine_ingress_hypercall_state")?;
            // T1(c) (outage 2026-08-27 §5.5): per-venue last-TICK age
            // in seconds. `*_state` lies on a 1 Hz-churning lane (a
            // sampler nearly always catches it mid-cycle at Up) and
            // `last_activity` advances on the venue's own rejection
            // bytes — only "when did MARKET DATA last arrive" names a
            // dead lane. -1 = no tick since boot. Order matches the
            // derivation loop: pm, bn, okx, deribit, hl, bybit, rpc,
            // mexc. The mexc gauge follows the unspawned-venue
            // convention (okx/deribit/hl/bybit with an empty section) —
            // registered, and -1 until the MEXC ingress ever ticks.
            let ingress_last_tick_age: [core_metrics::GaugeId; SNAPSHOT_VENUES] = [
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
                reg.register_gauge("engine_ingress_mexc_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_mexc_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_hyperevm_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_hyperevm_last_tick_age_seconds")?,
                reg.register_gauge("engine_ingress_hypercall_last_tick_age_seconds")
                    .map_err(|_| "register engine_ingress_hypercall_last_tick_age_seconds")?,
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
            let ingress_mexc = register_ingress_counters(&mut reg, "mexc")?;
            let ingress_hyperevm = register_ingress_counters(&mut reg, "hyperevm")?;
            let ingress_hypercall = register_ingress_counters(&mut reg, "hypercall")?;

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
            let capture_mexc = register_capture_gauges(&mut reg, "mexc")?;
            let capture_hyperevm = register_capture_gauges(&mut reg, "hyperevm")?;
            let capture_hypercall = register_capture_gauges(&mut reg, "hypercall")?;

            // §6.1 boot-discovery coverage gauges — PM/OKX/Deribit/HL
            // + Binance since M1 (exchangeInfo audit); RPC alone has
            // no REST discovery (boot_discovery module docs).
            let coverage_pm = register_coverage_gauge(&mut reg, "pm")?;
            let coverage_okx = register_coverage_gauge(&mut reg, "okx")?;
            let coverage_deribit = register_coverage_gauge(&mut reg, "deribit")?;
            let coverage_hyperliquid = register_coverage_gauge(&mut reg, "hl")?;
            let coverage_binance = register_coverage_gauge(&mut reg, "bn")?;
            let coverage_bybit = register_coverage_gauge(&mut reg, "bybit")?;
            let coverage_mexc = register_coverage_gauge(&mut reg, "mexc")?;
            let coverage_hypercall = register_coverage_gauge(&mut reg, "hypercall")?;
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
            let hypercall_options_selected = reg
                .register_gauge("engine_ingress_hypercall_options_selected")
                .map_err(|_| "register engine_ingress_hypercall_options_selected")?;
            // HC5: the venue's own family (the slow-consumer law, quote
            // shapes, the poller) — gauges mirrored from its atomics.
            let hypercall = register_hypercall_metrics(&mut reg)?;

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
            let xmm = register_xmm_metrics(&mut reg)?;
            let vrp = register_vrp_metrics(&mut reg)?;
            let xsd = register_xsd_metrics(&mut reg)?;
            let bin15 = register_bin15_metrics(&mut reg)?;
            let hyparb = register_hyparb_metrics(&mut reg)?;
            let hyparb_evm = register_hyparb_evm_metrics(&mut reg)?;
            let regime = register_regime_metrics(&mut reg)?;
            let har = register_har_metrics(&mut reg)?;
            let paper_matcher = register_paper_matcher_metrics(&mut reg)?;
            // E1: only when a router is actually in force. A boot with
            // no `--exec` reports `configured == 0` here and registers
            // NOTHING, which is what keeps `/metrics` byte-identical.
            let exec = match exec_modes.as_ref() {
                None => None,
                Some(m) => Some(register_exec_metrics(&mut reg, m)?),
            };
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
                strategy_hyparb,
                strategy_vrp,
                strategy_rule_tree,
                strategy_set,
                ingress_polymarket_state,
                ingress_binance_state,
                ingress_okx_state,
                ingress_deribit_state,
                ingress_hyperliquid_state,
                ingress_hl_rolls,
                ingress_hl_rolls_ignored,
                ingress_hl_family_ack_timeouts,
                ingress_hl_families_dormant,
                ingress_hl_outcome_bbo_one_sided,
                ingress_bybit_state,
                ingress_rpc_state,
                ingress_mexc_state,
                ingress_hyperevm_state,
                ingress_hypercall_state,
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
                ingress_mexc,
                ingress_hyperevm,
                ingress_hypercall,
                capture_pm,
                capture_bn,
                capture_okx,
                capture_deribit,
                capture_hyperliquid,
                capture_bybit,
                capture_rpc,
                capture_mexc,
                capture_hyperevm,
                capture_hypercall,
                coverage_pm,
                coverage_okx,
                coverage_deribit,
                coverage_hyperliquid,
                coverage_binance,
                coverage_bybit,
                coverage_mexc,
                coverage_hypercall,
                deribit_options_selected,
                okx_options_selected,
                binance_options_selected,
                hypercall_options_selected,
                hypercall,
                ingress_ai,
                capture_ai,
                fills_capture,
                orders_capture,
                strategy_enabled_mask,
                vm,
                xmm,
                vrp,
                xsd,
                bin15,
                hyparb,
                hyparb_evm,
                regime,
                har,
                paper_matcher,
                exec,
            });
        }
        if enable_metrics {
            // RG6: the `/state` snapshot cell — `/metrics` and the TUI
            // both read it (`--tui` implies metrics in the bin); one
            // boot-only allocation, ≈ 24 KB.
            out.state = Some(Arc::new(SnapshotCell::new(EngineSnapshot::empty())));
        }
        Ok(out)
    }
}

/// VT2: per-venue staleness thresholds (ms), indexed by the `VenueId`
/// byte — the venue defaults (`VenueId::default_stale_after_ms`,
/// docs/venue-time-capture-plan.md §2 doctrine 4) overridden by
/// repeatable `--stale-after-ms <venue>:<ms>` specs (labels as the
/// harness flags: `pm`/`bn`/`okx`/`deribit`/`hl`/`bybit`/`mexc`/
/// `hyperevm`/`hypercall`). A zero disables the judgement for that
/// venue (nothing is ever stale).
pub fn parse_stale_after_ms(specs: &[String]) -> Result<[u32; core_types::VENUE_COUNT], String> {
    let mut table = VenueId::stale_after_ms_defaults();
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
        trade_ring_drops: one("trade_ring_drops")?,
        stale_ticks: one("stale_ticks")?,
        seq_regressions: one("seq_regressions")?,
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
/// owned `Arc`s into [`engine_loop_set_full`] — the loop publishes
/// counters + dashboard snapshots into them.
#[derive(Default)]
pub struct Observability {
    /// Live metrics registry (counters + gauges). `None` if the
    /// `/metrics` server is disabled.
    pub metrics: Option<Arc<core_metrics::MetricsRegistry>>,
    /// Counter handles the engine loop bumps inline.
    pub counter_ids: Option<EngineCounters>,
    /// RG6: the `/state` snapshot cell — published every
    /// [`SNAPSHOT_PERIOD_NS`] by the engine loop, read by the
    /// `metrics-http` thread and the TUI. `None` when `/metrics` is
    /// disabled.
    pub state: Option<Arc<SnapshotCell<EngineSnapshot>>>,
    /// RG6: boot identity copied into every snapshot (the bin fills
    /// the process/run facts via [`Observability::with_boot_info`],
    /// the set builder stamps the masks).
    pub boot: BootInfo,
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
    /// VRP V8a: where the VRP member's persisted state is written.
    /// `None` = no VRP member is configured, and the engine writes
    /// nothing. Set by the set builder from the boot bundle.
    pub vrp_state_path: Option<std::path::PathBuf>,
    /// XSD-3: where and how the xsd member's positions are persisted.
    /// `None` = no xsd member is configured. Set by the set builder.
    pub xsd_state: Option<XsdStateSink>,
    /// HAR H3.4: `state-<NAME>.tsv` per long-tenor series, in the set's
    /// order. Empty = no HAR service, and the engine writes nothing. Set
    /// by the set builder from the boot bundle.
    pub har_state_paths: Vec<std::path::PathBuf>,
    /// HAR H3.7: the thread that renders and writes those files — each
    /// series' state handed to it at its day close through the set's
    /// outbox. `None` (no HAR service, or its spawn failed) = the engine
    /// loop writes them itself (the pre-H3.7 path). **Taken** by the
    /// engine loop, stopped and joined before its shutdown write.
    pub har_writer: Option<crate::har_writer::HarWriter>,
    /// HAR H3.5: SHA-256 of the `har.toml` the set was configured from
    /// (all-zero: no HAR service) — `/state.har.hash`.
    pub har_hash: [u8; 32],
    /// HAR H3.5: series the file named that the boot dropped (a feed the
    /// boot universe does not carry) — `/state.har.dropped`.
    pub har_dropped: u32,
    /// HYPARB H8: the testnet write path's tap (`mode = "testnet"`
    /// only) — **taken** by the engine loop, drained once per report
    /// period (O-H12: each paper AMM decision is shadowed on chain 998).
    pub hyparb_shadow: Option<crate::evm_testnet::ShadowTap>,
    /// HYPARB H9: `mode = "testnet"` booted with the shadow DARK
    /// (`evm_testnet::ShadowBootErr::Dark`) — published as
    /// `engine_hyparb_evm_dark`.
    pub hyparb_shadow_dark: bool,
    /// HYPARB L5: slot 0's LIVE arm's status — the same
    /// `engine_hyparb_evm_*` family the shadow publishes, now from the
    /// mainnet swap path. `None` unless slot 0 is armed live.
    pub hyparb_live_status: Option<std::sync::Arc<crate::hyparb_live::LiveShared>>,
}

/// XSD-3: the state writer's identity — the path, the table hash the
/// file is stamped with, and the descriptor of every target (the file
/// speaks descriptors, never `SymbolId`s).
#[derive(Clone, Debug)]
pub struct XsdStateSink {
    /// `~/multivenue/xsd-state.tsv` by default.
    pub path: std::path::PathBuf,
    /// The booted table's hash.
    pub table_hash: [u8; 32],
    /// `(target sym, descriptor)` for every table target.
    pub descriptors: Vec<(SymbolId, String)>,
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

    /// RG6: attach the boot identity the `/state` snapshot carries.
    /// Boot-only; the bin fills it once the run directory exists.
    pub fn with_boot_info(mut self, boot: BootInfo) -> Self {
        self.boot = boot;
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
    /// Active-strategy indicator — hyparb (slot 0; was latency-arb
    /// before HYPARB H0).
    pub strategy_hyparb: core_metrics::GaugeId,
    /// Active-strategy indicator — ev (A).
    pub strategy_vrp: core_metrics::GaugeId,
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
    /// BIN15 O2: `engine_ingress_hyperliquid_rolls_total`.
    pub ingress_hl_rolls: core_metrics::GaugeId,
    /// BIN15 O2: `engine_ingress_hyperliquid_rolls_ignored_unmatched_total`.
    pub ingress_hl_rolls_ignored: core_metrics::GaugeId,
    /// BIN15 O2: `engine_ingress_hyperliquid_family_ack_timeouts_total`.
    pub ingress_hl_family_ack_timeouts: core_metrics::GaugeId,
    /// BIN15 O2: `engine_ingress_hyperliquid_families_dormant` (gauge).
    pub ingress_hl_families_dormant: core_metrics::GaugeId,
    /// BIN15 O8: `engine_ingress_hyperliquid_outcome_bbo_one_sided_total`.
    pub ingress_hl_outcome_bbo_one_sided: core_metrics::GaugeId,
    /// WS9: per-ingress state gauge, Bybit v5 public WS.
    pub ingress_bybit_state: core_metrics::GaugeId,
    /// Per-ingress state gauge: Polygon JSON-RPC.
    pub ingress_rpc_state: core_metrics::GaugeId,
    /// MX6: per-ingress state gauge, MEXC (spot PB + futures JSON).
    pub ingress_mexc_state: core_metrics::GaugeId,
    /// HYPARB H3b: per-ingress state gauge, HyperEVM pool events.
    pub ingress_hyperevm_state: core_metrics::GaugeId,
    /// HC5: per-ingress state gauge, Hypercall public WS.
    pub ingress_hypercall_state: core_metrics::GaugeId,
    /// T1(c): per-venue last-tick-age gauges in seconds
    /// (`engine_ingress_<venue>_last_tick_age_seconds`; -1 = no tick
    /// since boot). Order: pm, bn, okx, deribit, hl, bybit, rpc, mexc,
    /// hyperevm, hypercall (the `SNAPSHOT_VENUES` / `ingress_lanes`
    /// order).
    pub ingress_last_tick_age: [core_metrics::GaugeId; SNAPSHOT_VENUES],
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
    /// MX6: §6.4 loss-accounting counters, MEXC thread.
    pub ingress_mexc: IngressCounterIds,
    /// HYPARB H3b: §6.4 counters, HyperEVM pool events.
    pub ingress_hyperevm: IngressCounterIds,
    /// HC5: §6.4 loss-accounting counters, Hypercall thread.
    pub ingress_hypercall: IngressCounterIds,
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
    /// MX6: §6.5 capture-health gauges, MEXC thread.
    pub capture_mexc: CaptureGaugeIds,
    /// HYPARB H3b: capture-health gauges, HyperEVM.
    pub capture_hyperevm: CaptureGaugeIds,
    /// HC5: capture-health gauges, Hypercall (its REST rows included —
    /// the ingress thread writes them).
    pub capture_hypercall: CaptureGaugeIds,
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
    /// HC5: same for Hypercall (`engine_ingress_hypercall_options_selected`).
    pub hypercall_options_selected: GaugeId,
    /// HC5: the Hypercall venue family ([`register_hypercall_metrics`]).
    pub hypercall: HcMetricIds,
    /// §6.1 boot-discovery coverage gauge, Hyperliquid (0 when
    /// unconfigured).
    pub coverage_hyperliquid: GaugeId,
    /// M1 boot-discovery coverage gauge, Binance exchangeInfo audit
    /// (0 when skipped — legacy flag boots).
    pub coverage_binance: GaugeId,
    /// WS9: boot-discovery coverage gauge, Bybit instruments-info
    /// audit (0 when the `[bybit]` section is empty).
    pub coverage_bybit: GaugeId,
    /// MX6: boot-discovery coverage gauge, MEXC exchangeInfo +
    /// contract/detail audit (0 when the `[mexc]` section is empty).
    pub coverage_mexc: GaugeId,
    /// HC5: boot-discovery coverage gauge, Hypercall `/markets`
    /// (configured underlyings; 0 when `[hypercall]` is off).
    pub coverage_hypercall: GaugeId,
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
    /// XMM XH3: the `engine_xmm_*` family (slot 6; it replaced the
    /// retired `engine_icdp_*_total` family).
    pub xmm: XmmMetricIds,
    /// VRP V7: the `engine_vrp_*` family (slot 1).
    pub vrp: VrpMetricIds,
    /// XSD-3: the `engine_xsd_*` family (slot 2).
    pub xsd: XsdMetricIds,
    /// BIN15 O4b: the `engine_bin15_*` family (slot 3).
    pub bin15: Bin15MetricIds,
    /// HYPARB H6: the `engine_hyparb_*` family (slot 0).
    pub hyparb: HyparbMetricIds,
    /// HYPARB H8: the `engine_hyparb_evm_*` family (the testnet shadow).
    pub hyparb_evm: HyparbEvmMetricIds,
    /// RG2: the `engine_regime_*` family.
    pub regime: RegimeMetricIds,
    /// HAR H3.5: the `engine_har_*` gauges.
    pub har: HarMetricIds,
    /// X1: the `engine_paper_matcher_*` family + the set's
    /// `engine_set_fills_unrouted_total`.
    pub paper_matcher: PaperMatcherMetricIds,
    /// E1: the `engine_exec_*` family. `None` on a boot with NO
    /// `--exec`, and that is load-bearing: nothing is registered, so
    /// `/metrics` is byte-identical to a pre-E1 binary's. This is the
    /// metric half of the E1 acceptance gate.
    pub exec: Option<ExecMetricIds>,
}

/// E1: the execution router's metric family. Boot-only.
///
/// `core-metrics` registers FIXED names — there is no label mechanism
/// (`MAX_COUNTERS = 512` since E7 — 256 before —, `MAX_GAUGES = 384`, `NAME_MAX = 63`), so the
/// per-slot names are generated as whole strings at boot, one
/// `register_counter` call each, and never formatted again. The
/// per-slot family is registered for LIVE slots ONLY: an all-paper
/// artifact costs five names, not forty.
#[derive(Copy, Clone, Debug)]
pub struct ExecMetricIds {
    /// `engine_exec_configured` (gauge) — 1 while a route table is in
    /// force. The one-glance "is this engine routing?" answer.
    pub configured: GaugeId,
    /// `engine_exec_live_submits_total`
    pub live_submits: core_metrics::CounterId,
    /// `engine_exec_paper_submits_total`
    pub paper_submits: core_metrics::CounterId,
    /// `engine_exec_refused_off_total`
    pub refused_off: core_metrics::CounterId,
    /// `engine_exec_refused_risk_total` (E6) — requests the RISK GATE
    /// refused, ALL six reasons summed (max_order, cap_instance,
    /// cap_day, open_orders, unseeded, halted). Only the max_order
    /// share means "the member's ledger and the operator's number
    /// disagreed"; the rest are the clamp working. An alert belongs on
    /// the router's `refused_max_order` (in `/state`), not here.
    pub refused_risk: core_metrics::CounterId,
    /// `engine_exec_cancel_on_off_total` (E7) — cancels that reached
    /// the live arm from an `Off` slot. Not a refusal; an `off` slot
    /// that is cancelling is an `off` slot that still had orders at
    /// the venue.
    pub cancel_on_off: core_metrics::CounterId,
    /// `engine_exec_refused_no_route_total` — **the LAW E-1 counter.**
    /// A live slot's order that named a venue with no route. Must stay
    /// 0; anything else is a routing bug, and the order was refused
    /// rather than quietly modelled.
    pub refused_no_route: core_metrics::CounterId,
    /// `engine_exec_refused_halted_total` (E6 c4) — requests refused
    /// because their slot is HALTED. The one refusal reason an
    /// operator must not have to infer from a total.
    pub refused_halted: core_metrics::CounterId,
    /// `engine_exec_refused_unseeded_total` (E6 c4) — refused because
    /// the ledger has never been reconciled. Expected non-zero for a
    /// few seconds after a boot and zero after; still climbing means
    /// the arm never reached the venue.
    pub refused_unseeded: core_metrics::CounterId,
    /// `engine_exec_halts_total` (E6 c4) — halt EDGES. **The alarm.**
    pub halts: core_metrics::CounterId,
    /// `engine_exec_cancel_all_failures_total` (E6 c4) — cancel-all
    /// requests the arm would not accept.
    pub cancel_all_failures: core_metrics::CounterId,
    /// `engine_exec_cancel_all_stranded_total` (E6 c4) — polls on
    /// which the arm reported it had given up with the venue
    /// unconfirmed. **The stranded-quote number** (LAW E-8).
    pub cancel_all_stranded: core_metrics::CounterId,
    /// `engine_exec_seeded` (gauge, E6 c4) — 1 once the ledger has
    /// been reconciled. A boot stuck at 0 is a boot not trading, and
    /// nothing else says so in one glance.
    pub seeded: GaugeId,
    /// **E7 — the ledger's alarms**, `engine_exec_ledger_*_total`. All
    /// six should stay 0: `fills_unbound` = the risk gate stopped
    /// seeing a real position; `sells_below_zero` = the router and the
    /// venue disagree; `resting_full` = `max_open_orders` ratcheting
    /// toward permanent refusal; `resting_ambiguous` = open-order
    /// tracking broke; `binds_refused` / `settles_unmatched` = a roll
    /// the ledger could not take.
    pub ledger: [core_metrics::CounterId; 6],
    /// **E7 — the live arm**, `engine_exec_hl_*`. Counters in the
    /// order of [`LIVE_ARM_COUNTER_NAMES`]; `budget_remaining` is the
    /// one gauge (`engine_exec_hl_budget_remaining`).
    pub arm: [core_metrics::CounterId; LIVE_ARM_COUNTER_NAMES.len()],
    /// `engine_exec_hl_budget_remaining` (gauge) — the address request
    /// budget's headroom. At or below `request_budget_floor` the arm
    /// refuses every submit and the halt machine latches
    /// `budget-floor`.
    pub budget_remaining: GaugeId,
    /// `engine_exec_hl_pnl_anchor_usd_1e6` (gauge) — E7 session bound:
    /// the spot-USDC anchor, 0 until the first flat reconciliation.
    pub pnl_anchor: GaugeId,
    /// `engine_exec_hl_session_pnl_usd_1e6` (gauge, signed) — E7
    /// session bound: spot USDC minus the anchor at the last
    /// reconciliation; what `halt_on_gain_usd_1e6` /
    /// `halt_on_loss_usd_1e6` latch `pnl-gain` / `pnl-loss` on.
    pub session_pnl: GaugeId,
    /// Index = slot. `Some` only for LIVE slots — a paper or off slot
    /// costs no metric names at all (plan §3.5).
    ///
    /// Fixed array rather than a `Vec` so [`MetricIds`] stays `Copy`,
    /// which the engine loop relies on.
    pub slots: [Option<ExecSlotMetricIds>; clob_dispatcher::EXEC_COUNTER_SLOTS],
}

/// E1: one live slot's metric handles.
///
/// Names: `engine_exec_slot<N>_mode`,
/// `engine_exec_slot<N>_live_submits_total`,
/// `engine_exec_slot<N>_refused_total`.
///
/// DEFERRED to the phase that can actually move them, rather than
/// registered here reading a permanent zero: `_live_acks_total` /
/// `_rej_venue_total` (E3, the HTTP arm), `_rej_gate_total` /
/// `_halted` / `_recon_drift_usd_1e6` (E6, the risk gate),
/// `engine_exec_hl_budget_remaining` / `engine_exec_hl_nonce_last`
/// (E4, the governor). Registering a name that can only read zero
/// spends scarce registry room and tells an operator nothing.
#[derive(Copy, Clone, Debug)]
pub struct ExecSlotMetricIds {
    /// `engine_exec_slot<N>_mode` — 0 paper / 1 live / 2 off.
    pub mode: GaugeId,
    /// `engine_exec_slot<N>_live_submits_total`
    pub live_submits: core_metrics::CounterId,
    /// `engine_exec_slot<N>_refused_total`
    pub refused: core_metrics::CounterId,
    /// `engine_exec_slot<N>_halted` (E6 c4) — `HaltReason as u8`;
    /// 0 running, 1 reject-streak, 2 budget-floor, 3 recon-drift,
    /// 4 ws-gap, 5 asset-refusals, 6 operator. A gauge rather than a
    /// counter because an operator's question is "is it halted NOW,
    /// and why", not "how many times".
    pub halted: GaugeId,
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
    /// Ring `try_push_ref` failures (D4).
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
    /// XMM XH1: trade-lane pushes refused by a full ring
    /// (`engine_ingress_<venue>_trade_ring_drops_total`; the print is
    /// still captured).
    pub trade_ring_drops: core_metrics::CounterId,
    /// VT2: ticks the ingress judged stale
    /// (`engine_ingress_<venue>_stale_ticks_total`).
    pub stale_ticks: core_metrics::CounterId,
    /// MX6 (ruling Q-MX1): venue sequence values seen BELOW the last
    /// full-width value of the same symbol × stream
    /// (`engine_ingress_<venue>_seq_regressions_total`) — the check a
    /// venue whose streams skip versions by design gets instead of the
    /// §6.2 chain law. 0 on every venue that never increments it.
    pub seq_regressions: core_metrics::CounterId,
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
    /// `try_push_ref` (§5 push-full; isolates the cause inside
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
    /// `engine_vm_regime_blocked_total` (RG3) — entry evaluations
    /// refused by a closed row regime gate.
    pub regime_blocked: core_metrics::CounterId,
    /// `engine_vm_regime_hard_exits_total` (RG3) — positions
    /// flattened by a HARD-closed row gate.
    pub regime_hard_exits: core_metrics::CounterId,
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
    regime_blocked: u64,
    regime_hard_exits: u64,
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
        regime_blocked: strat.vm_regime_blocked(),
        regime_hard_exits: strat.vm_regime_hard_exits(),
    };
    reg.counter(ids.fires)
        .inc(cur.fires.saturating_sub(last.fires));
    reg.counter(ids.orders_emitted)
        .inc(cur.orders_emitted.saturating_sub(last.orders_emitted));
    reg.counter(ids.orders_dropped)
        .inc(cur.orders_dropped.saturating_sub(last.orders_dropped));
    reg.counter(ids.commit_dropped)
        .inc(cur.commit_dropped.saturating_sub(last.commit_dropped));
    reg.counter(ids.regime_blocked)
        .inc(cur.regime_blocked.saturating_sub(last.regime_blocked));
    reg.counter(ids.regime_hard_exits)
        .inc(cur.regime_hard_exits.saturating_sub(last.regime_hard_exits));
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
        regime_blocked: one("engine_vm_regime_blocked_total")?,
        regime_hard_exits: one("engine_vm_regime_hard_exits_total")?,
        rows_active,
        table_epoch,
    })
}

// ---------------------------------------------------------------
// VRP V7 — slot-1 observability (5 s mirror, cold path)
// ---------------------------------------------------------------

/// Registry handles for the VRP family (`engine_vrp_*`), mirrored like
/// the ICDP family through the `StrategyCounters` default accessor
/// `vrp_counters` (zeros on every boot without a configured slot-1
/// member).
///
/// The two QLIKE rows are GAUGES, not counters: they are trailing means,
/// not accumulations, and `qlike_har_beats_iv` reading 0 on a full
/// window is **kill criterion 3** — E1 has stopped holding and the
/// member has no edge left to harvest. That is the one number on this
/// dashboard an operator is expected to act on.
#[derive(Copy, Clone, Debug)]
pub struct VrpMetricIds {
    /// `engine_vrp_decisions_total`
    pub decisions: core_metrics::CounterId,
    /// `engine_vrp_decisions_late_total`
    pub decisions_late: core_metrics::CounterId,
    /// `engine_vrp_entries_total`
    pub entries: core_metrics::CounterId,
    /// `engine_vrp_hedges_total`
    pub hedges: core_metrics::CounterId,
    /// `engine_vrp_exits_total`
    pub exits: core_metrics::CounterId,
    /// `engine_vrp_holds_total`
    pub holds: core_metrics::CounterId,
    /// `engine_vrp_holds_side_total` (Q4 — the band opened an arm that
    /// `sides` policy refuses)
    pub holds_side: core_metrics::CounterId,
    /// `engine_vrp_select_scans_total` (F31 — chain scans by the
    /// selection law; bounded by the selection window)
    pub select_scans: core_metrics::CounterId,
    /// `engine_vrp_entries_submitted_total` (X1 — intents; `entries`
    /// counts the ones that FILLED, and the gap is F7)
    pub entries_submitted: core_metrics::CounterId,
    /// `engine_vrp_entries_unfilled_total` (X1)
    pub entries_unfilled: core_metrics::CounterId,
    /// `engine_vrp_hedge_unfilled_total` (X1)
    pub hedge_unfilled: core_metrics::CounterId,
    /// `engine_vrp_hedge_abandoned_total` (X1). **Non-zero is an
    /// operator alert** — the book is off its delta target and nothing
    /// is chasing it (`docs/risk-policy.md`).
    pub hedge_abandoned: core_metrics::CounterId,
    /// `engine_vrp_fills_total` (X1 — modelled fills consumed)
    pub fills: core_metrics::CounterId,
    /// `engine_vrp_fills_ignored_total` (X1 — matched no leg in flight)
    pub fills_ignored: core_metrics::CounterId,
    /// `engine_vrp_entry_maker_submitted_total` (R1 — entries posted as
    /// a RESTING order; `entries_submitted` minus this crossed)
    pub entry_maker_submitted: core_metrics::CounterId,
    /// `engine_vrp_entry_crossed_total` (R1 — the fallback fired)
    pub entry_crossed: core_metrics::CounterId,
    /// `engine_vrp_entry_cost_refused_total` (R1 — the fallback REFUSED:
    /// crossing would not have cleared the cost gate)
    pub entry_cost_refused: core_metrics::CounterId,
    /// `engine_vrp_hedge_crossed_total` (R2 — a maker hedge that had to
    /// cross; the share of the maker saving that is not real)
    pub hedge_crossed: core_metrics::CounterId,
    /// `engine_vrp_settle_index_fallback_total` (R5 — a settlement
    /// priced off the LAST print because the 30-minute delivery window
    /// carried under 10 min of samples)
    pub settle_index_fallback: core_metrics::CounterId,
    /// `engine_vrp_iv_median_fallback_total` (R6 — a decision taken on
    /// the LAST quoted implied vol for want of a median)
    pub iv_median_fallback: core_metrics::CounterId,
    /// `engine_vrp_holds_cost_total` (R7 — a HOLD that θ ALONE would
    /// have traded: the fee load, measured)
    pub holds_cost: core_metrics::CounterId,
    /// `engine_vrp_last_settle_value_1e6` (X1 gauge — the cash the last
    /// settlement booked; no order is emitted for it)
    pub last_settle_value_1e6: core_metrics::GaugeId,
    /// `engine_vrp_regime_offset_1e6` (R3 gauge — the regime's log-vol
    /// intercept in force at the last decision; 0 = no table, no
    /// detector, or `vol:normal`)
    pub regime_offset_1e6: core_metrics::GaugeId,
    /// `engine_vrp_no_bounds_total`
    pub no_bounds: core_metrics::CounterId,
    /// `engine_vrp_stale_skips_total` (F30 — DECISIONS lost to a stale
    /// mark; non-zero at a decision instant is a campaign lost)
    pub stale_skips: core_metrics::CounterId,
    /// `engine_vrp_records_ignored_total` (F30 — option records that
    /// carried nothing usable; routine)
    pub records_ignored: core_metrics::CounterId,
    /// `engine_vrp_no_selection_total`
    pub no_selection: core_metrics::CounterId,
    /// `engine_vrp_regime_blocked_total`
    pub regime_blocked: core_metrics::CounterId,
    /// `engine_vrp_regime_exits_total`
    pub regime_exits: core_metrics::CounterId,
    /// `engine_vrp_settlements_total`
    pub settlements: core_metrics::CounterId,
    /// `engine_vrp_caps_rejected_total`
    pub caps_rejected: core_metrics::CounterId,
    /// `engine_vrp_settled_itm_total` (VX)
    pub settled_itm: core_metrics::CounterId,
    /// `engine_vrp_settled_otm_total` (VX — an OTM expiry is free, so
    /// this counts positions that ended with no fill at all)
    pub settled_otm: core_metrics::CounterId,
    /// `engine_vrp_settled_unpriced_total` (V8a). **Non-zero is a
    /// reconciliation item, not a routine counter:** an in-the-money
    /// expiry whose contract had already rolled off the chain, so the
    /// value could not be recorded.
    pub settled_unpriced: core_metrics::CounterId,
    /// `engine_vrp_killed` (gauge; kill criterion 3 has HALTED the
    /// member — sticky until a restart)
    pub killed: core_metrics::GaugeId,
    /// `engine_vrp_qlike_iv_1e6` (gauge)
    pub qlike_iv_1e6: core_metrics::GaugeId,
    /// `engine_vrp_qlike_har_1e6` (gauge)
    pub qlike_har_1e6: core_metrics::GaugeId,
    /// `engine_vrp_qlike_har_beats_iv` (gauge; kill criterion 3)
    pub qlike_har_beats_iv: core_metrics::GaugeId,
}

/// Register the VRP family. Boot-only.
fn register_vrp_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<VrpMetricIds, &'static str> {
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register vrp counter")
    };
    let decisions = one("engine_vrp_decisions_total")?;
    let decisions_late = one("engine_vrp_decisions_late_total")?;
    let entries = one("engine_vrp_entries_total")?;
    let hedges = one("engine_vrp_hedges_total")?;
    let exits = one("engine_vrp_exits_total")?;
    let holds = one("engine_vrp_holds_total")?;
    let holds_side = one("engine_vrp_holds_side_total")?;
    let select_scans = one("engine_vrp_select_scans_total")?;
    let entries_submitted = one("engine_vrp_entries_submitted_total")?;
    let entries_unfilled = one("engine_vrp_entries_unfilled_total")?;
    let hedge_unfilled = one("engine_vrp_hedge_unfilled_total")?;
    let hedge_abandoned = one("engine_vrp_hedge_abandoned_total")?;
    let fills = one("engine_vrp_fills_total")?;
    let fills_ignored = one("engine_vrp_fills_ignored_total")?;
    let entry_maker_submitted = one("engine_vrp_entry_maker_submitted_total")?;
    let entry_crossed = one("engine_vrp_entry_crossed_total")?;
    let entry_cost_refused = one("engine_vrp_entry_cost_refused_total")?;
    let hedge_crossed = one("engine_vrp_hedge_crossed_total")?;
    let settle_index_fallback = one("engine_vrp_settle_index_fallback_total")?;
    let iv_median_fallback = one("engine_vrp_iv_median_fallback_total")?;
    let holds_cost = one("engine_vrp_holds_cost_total")?;
    let no_bounds = one("engine_vrp_no_bounds_total")?;
    let stale_skips = one("engine_vrp_stale_skips_total")?;
    let records_ignored = one("engine_vrp_records_ignored_total")?;
    let no_selection = one("engine_vrp_no_selection_total")?;
    let regime_blocked = one("engine_vrp_regime_blocked_total")?;
    let regime_exits = one("engine_vrp_regime_exits_total")?;
    let settlements = one("engine_vrp_settlements_total")?;
    let caps_rejected = one("engine_vrp_caps_rejected_total")?;
    let settled_itm = one("engine_vrp_settled_itm_total")?;
    let settled_otm = one("engine_vrp_settled_otm_total")?;
    let settled_unpriced = one("engine_vrp_settled_unpriced_total")?;
    let mut g = |name: &str| -> Result<core_metrics::GaugeId, &'static str> {
        reg.register_gauge(name).map_err(|_| "register vrp gauge")
    };
    Ok(VrpMetricIds {
        decisions,
        decisions_late,
        entries,
        hedges,
        exits,
        holds,
        holds_side,
        select_scans,
        entries_submitted,
        entries_unfilled,
        hedge_unfilled,
        hedge_abandoned,
        fills,
        fills_ignored,
        entry_maker_submitted,
        entry_crossed,
        entry_cost_refused,
        hedge_crossed,
        settle_index_fallback,
        iv_median_fallback,
        holds_cost,
        no_bounds,
        stale_skips,
        records_ignored,
        no_selection,
        regime_blocked,
        regime_exits,
        settlements,
        caps_rejected,
        settled_itm,
        settled_otm,
        settled_unpriced,
        killed: g("engine_vrp_killed")?,
        qlike_iv_1e6: g("engine_vrp_qlike_iv_1e6")?,
        qlike_har_1e6: g("engine_vrp_qlike_har_1e6")?,
        qlike_har_beats_iv: g("engine_vrp_qlike_har_beats_iv")?,
        last_settle_value_1e6: g("engine_vrp_last_settle_value_1e6")?,
        regime_offset_1e6: g("engine_vrp_regime_offset_1e6")?,
    })
}

/// X1: the paper matcher's family. Boot-only.
#[derive(Copy, Clone, Debug)]
pub struct PaperMatcherMetricIds {
    /// `engine_paper_matcher_intake_total`
    pub intake: core_metrics::CounterId,
    /// `engine_paper_matcher_fills_total`
    pub fills: core_metrics::CounterId,
    /// `engine_paper_matcher_ioc_canceled_total` — **the F7 counter**.
    /// A mid-priced IoC on a real spread lives here, and the VRP
    /// member's option entry did, twice, while the member believed it
    /// held the position.
    pub ioc_canceled: core_metrics::CounterId,
    /// `engine_paper_matcher_ttl_expired_total`
    pub ttl_expired: core_metrics::CounterId,
    /// `engine_paper_matcher_rejected_open_cap_total`
    pub rejected_open_cap: core_metrics::CounterId,
    /// `engine_paper_matcher_unroutable_total`
    pub unroutable: core_metrics::CounterId,
    /// `engine_paper_matcher_out_overflow_total` — must stay 0.
    pub out_overflow: core_metrics::CounterId,
    /// `engine_paper_matcher_open_orders` (gauge)
    pub open_orders: core_metrics::GaugeId,
    /// `engine_set_fills_unrouted_total` — fills stamped for a slot
    /// that is not enabled, or not built.
    pub fills_unrouted: core_metrics::CounterId,
    /// `engine_paper_matcher_cancels_total` (E5) — resting orders the
    /// matcher took back.
    pub cancels: core_metrics::CounterId,
    /// `engine_paper_matcher_modifies_total` (E5) — resting orders
    /// repriced in place.
    pub modifies: core_metrics::CounterId,
    /// `engine_paper_matcher_no_such_order_total` (E5) — lifecycle
    /// verbs that lost a race to a fill or a TTL. **Expected to be
    /// non-zero**; it counts races, not errors.
    pub no_such_order: core_metrics::CounterId,
    /// `engine_paper_matcher_identity_mismatch_total` (E5) — lifecycle
    /// verbs that described a different order than the one resting
    /// under that client id. **Must stay 0**: nothing in normal
    /// operation changes an order's identity, so a non-zero value
    /// names a caller that built the wrong request.
    pub identity_mismatch: core_metrics::CounterId,
    /// `engine_paper_matcher_ambiguous_order_total` (E5) — lifecycle
    /// verbs naming an id more than one of that slot's resting orders
    /// answers to. **Must stay 0**: it means a member reused a client
    /// id while the first order was still resting.
    pub ambiguous_order: core_metrics::CounterId,
    /// `engine_lifecycle_cancels_ok_total` (E5). Cancels ARE on the
    /// tape since E5 commit 4a: every lifecycle verb is captured as an
    /// `Order` row with `Order.verb` set (`docs/migration.md`), so a
    /// non-zero count here is a normal event, not a replay hazard.
    /// Same for `modifies_ok`.
    pub lifecycle_cancels_ok: core_metrics::CounterId,
    /// `engine_lifecycle_cancels_err_total` (E5).
    pub lifecycle_cancels_err: core_metrics::CounterId,
    /// `engine_lifecycle_modifies_ok_total` (E5) — see
    /// `lifecycle_cancels_ok`.
    pub lifecycle_modifies_ok: core_metrics::CounterId,
    /// `engine_lifecycle_modifies_err_total` (E5).
    pub lifecycle_modifies_err: core_metrics::CounterId,
    /// HYPARB H6: `engine_paper_matcher_amm_{fills,canceled,partial,
    /// not_live}_total` — the AMM fill law's verdicts (H2), in that order.
    pub amm: [core_metrics::CounterId; 4],
    /// XMM XH2: `engine_paper_matcher_queue_{placed,rested,rejected_alo,
    /// canceled,fills}_total` — the queue law's own tally, in that order —
    /// and `engine_paper_matcher_order_events_overflow_total` (must stay 0).
    pub queue: [core_metrics::CounterId; 6],
    /// XMM XH2: `engine_set_order_events_unrouted_total` — order events
    /// that reached the set for a slot not enabled or not built, or
    /// attributed to none. Counted, never fanned out.
    pub order_events_unrouted: core_metrics::CounterId,
}

/// Register the paper-matcher family. Boot-only.
fn register_paper_matcher_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<PaperMatcherMetricIds, &'static str> {
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register paper matcher counter")
    };
    let intake = one("engine_paper_matcher_intake_total")?;
    let fills = one("engine_paper_matcher_fills_total")?;
    let ioc_canceled = one("engine_paper_matcher_ioc_canceled_total")?;
    let ttl_expired = one("engine_paper_matcher_ttl_expired_total")?;
    let rejected_open_cap = one("engine_paper_matcher_rejected_open_cap_total")?;
    let unroutable = one("engine_paper_matcher_unroutable_total")?;
    let out_overflow = one("engine_paper_matcher_out_overflow_total")?;
    let fills_unrouted = one("engine_set_fills_unrouted_total")?;
    let cancels = one("engine_paper_matcher_cancels_total")?;
    let modifies = one("engine_paper_matcher_modifies_total")?;
    let no_such_order = one("engine_paper_matcher_no_such_order_total")?;
    let identity_mismatch = one("engine_paper_matcher_identity_mismatch_total")?;
    let ambiguous_order = one("engine_paper_matcher_ambiguous_order_total")?;
    let lifecycle_cancels_ok = one("engine_lifecycle_cancels_ok_total")?;
    let lifecycle_cancels_err = one("engine_lifecycle_cancels_err_total")?;
    let lifecycle_modifies_ok = one("engine_lifecycle_modifies_ok_total")?;
    let lifecycle_modifies_err = one("engine_lifecycle_modifies_err_total")?;
    let amm = [
        one("engine_paper_matcher_amm_fills_total")?,
        one("engine_paper_matcher_amm_canceled_total")?,
        one("engine_paper_matcher_amm_partial_total")?,
        one("engine_paper_matcher_amm_not_live_total")?,
    ];
    let queue = [
        one("engine_paper_matcher_queue_placed_total")?,
        one("engine_paper_matcher_queue_rested_total")?,
        one("engine_paper_matcher_queue_rejected_alo_total")?,
        one("engine_paper_matcher_queue_canceled_total")?,
        one("engine_paper_matcher_queue_fills_total")?,
        one("engine_paper_matcher_order_events_overflow_total")?,
    ];
    let order_events_unrouted = one("engine_set_order_events_unrouted_total")?;
    Ok(PaperMatcherMetricIds {
        amm,
        queue,
        order_events_unrouted,
        intake,
        fills,
        ioc_canceled,
        ttl_expired,
        rejected_open_cap,
        unroutable,
        out_overflow,
        fills_unrouted,
        cancels,
        modifies,
        no_such_order,
        identity_mismatch,
        ambiguous_order,
        lifecycle_cancels_ok,
        lifecycle_cancels_err,
        lifecycle_modifies_ok,
        lifecycle_modifies_err,
        open_orders: reg
            .register_gauge("engine_paper_matcher_open_orders")
            .map_err(|_| "register paper matcher gauge")?,
    })
}

/// E1: register the `engine_exec_*` family. Boot-only, and only when a
/// route table is in force.
///
/// Per-slot names are built here as fixed strings — `core-metrics` has
/// no label mechanism, so `engine_exec_slot3_mode` is a whole name
/// registered once, not `engine_exec_slot_mode{slot="3"}`. Only LIVE
/// slots get a per-slot family (plan §3.5): today that is at most one.
/// The ledger rows of the exec family, in `ExecMetricIds::ledger`
/// order.
const LEDGER_COUNTER_NAMES: [&str; 6] = [
    "engine_exec_ledger_fills_unbound_total",
    "engine_exec_ledger_sells_below_zero_total",
    "engine_exec_ledger_binds_refused_total",
    "engine_exec_ledger_resting_full_total",
    "engine_exec_ledger_resting_ambiguous_total",
    "engine_exec_ledger_settles_unmatched_total",
];

/// The live-arm rows of the exec family, in `ExecMetricIds::arm`
/// order — one per `clob_dispatcher::LiveArmCounters` counter field,
/// mirrored by [`live_arm_counter_values`], which is what pins the two
/// together.
const LIVE_ARM_COUNTER_NAMES: [&str; 30] = [
    "engine_exec_hl_submitted_total",
    "engine_exec_hl_rejected_total",
    "engine_exec_hl_ioc_missed_total",
    "engine_exec_hl_refused_local_total",
    "engine_exec_hl_refused_stale_total",
    "engine_exec_hl_sent_unanswered_total",
    "engine_exec_hl_fills_booked_total",
    "engine_exec_hl_fills_unresolved_total",
    "engine_exec_hl_fills_foreign_total",
    "engine_exec_hl_fills_dropped_total",
    "engine_exec_hl_fills_refused_total",
    "engine_exec_hl_fills_scan_failed_total",
    "engine_exec_hl_fills_unowned_total",
    "engine_exec_hl_recon_ok_total",
    "engine_exec_hl_recon_failed_total",
    "engine_exec_hl_recon_drift_legs_total",
    "engine_exec_hl_recon_unseen_legs_total",
    "engine_exec_hl_sweep_left_total",
    "engine_exec_hl_sweep_stalled_total",
    "engine_exec_hl_cancel_all_unqueued_total",
    "engine_exec_hl_ws_reconnects_total",
    "engine_exec_hl_ws_connect_failures_total",
    "engine_exec_hl_rolls_bound_total",
    "engine_exec_hl_rolls_refused_total",
    "engine_exec_hl_owner_contested_total",
    "engine_exec_hl_sweep_all_cancelled_total",
    "engine_exec_hl_topup_ok_total",
    "engine_exec_hl_topup_failed_total",
    "engine_exec_hl_day_sync_ok_total",
    "engine_exec_hl_day_sync_failed_total",
];

/// `LiveArmCounters`' counter fields in [`LIVE_ARM_COUNTER_NAMES`]
/// order. `recon_drift_legs` and `recon_unseen_legs` are LEVELS at
/// the last reconciliation, mirrored as monotonic counters like the
/// bin15 dormant-families level is — a rising series means the
/// comparisons keep disagreeing, and `/state` carries the level.
// COPY: [u64; 30] (240 B) returned by value — cold, the 5 s /metrics
// mirror; the struct's fields are visited once in the metric name order
// and the array is what the registry's delta loop indexes — rejected:
// an out-param, for 240 B twice every 5 s (cur and last).
// `sweep_all_left` is not mirrored: it is a level whose "unreadable"
// sentinel is `u64::MAX`, which a delta counter would read as a jump;
// the drain logs it.
#[inline]
fn live_arm_counter_values(a: &clob_dispatcher::LiveArmCounters) -> [u64; 30] {
    [
        a.submitted,
        a.rejected,
        a.ioc_missed,
        a.refused_local,
        a.refused_stale,
        a.sent_unanswered,
        a.fills_booked,
        a.fills_unresolved,
        a.fills_foreign,
        a.fills_dropped,
        a.fills_refused,
        a.fills_scan_failed,
        a.fills_unowned,
        a.recon_ok,
        a.recon_failed,
        a.recon_drift_legs,
        a.recon_unseen_legs,
        a.sweep_left,
        a.sweep_stalled,
        a.cancel_all_unqueued,
        a.ws_reconnects,
        a.ws_connect_failures,
        a.rolls_bound,
        a.rolls_refused,
        a.owner_contested,
        a.sweep_all_cancelled,
        a.topup_ok,
        a.topup_failed,
        a.day_sync_ok,
        a.day_sync_failed,
    ]
}

fn register_exec_metrics(
    reg: &mut core_metrics::MetricsRegistry,
    modes: &[u8; clob_dispatcher::EXEC_COUNTER_SLOTS],
) -> Result<ExecMetricIds, &'static str> {
    let configured = reg
        .register_gauge("engine_exec_configured")
        .map_err(|_| "register engine_exec_configured")?;
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name).map_err(|_| "register exec counter")
    };
    let live_submits = one("engine_exec_live_submits_total")?;
    let paper_submits = one("engine_exec_paper_submits_total")?;
    let refused_off = one("engine_exec_refused_off_total")?;
    let refused_no_route = one("engine_exec_refused_no_route_total")?;
    let refused_risk = one("engine_exec_refused_risk_total")?;
    let refused_halted = one("engine_exec_refused_halted_total")?;
    let refused_unseeded = one("engine_exec_refused_unseeded_total")?;
    let halts = one("engine_exec_halts_total")?;
    let cancel_all_failures = one("engine_exec_cancel_all_failures_total")?;
    let cancel_all_stranded = one("engine_exec_cancel_all_stranded_total")?;
    let cancel_on_off = one("engine_exec_cancel_on_off_total")?;
    let mut ledger = [core_metrics::CounterId::default(); 6];
    for (i, name) in LEDGER_COUNTER_NAMES.iter().enumerate() {
        ledger[i] = one(name)?;
    }
    let mut arm = [core_metrics::CounterId::default(); LIVE_ARM_COUNTER_NAMES.len()];
    for (i, name) in LIVE_ARM_COUNTER_NAMES.iter().enumerate() {
        arm[i] = one(name)?;
    }
    let seeded = reg
        .register_gauge("engine_exec_seeded")
        .map_err(|_| "register engine_exec_seeded")?;
    let budget_remaining = reg
        .register_gauge("engine_exec_hl_budget_remaining")
        .map_err(|_| "register engine_exec_hl_budget_remaining")?;
    let pnl_anchor = reg
        .register_gauge("engine_exec_hl_pnl_anchor_usd_1e6")
        .map_err(|_| "register engine_exec_hl_pnl_anchor_usd_1e6")?;
    let session_pnl = reg
        .register_gauge("engine_exec_hl_session_pnl_usd_1e6")
        .map_err(|_| "register engine_exec_hl_session_pnl_usd_1e6")?;

    let mut slots: [Option<ExecSlotMetricIds>; clob_dispatcher::EXEC_COUNTER_SLOTS] =
        [None; clob_dispatcher::EXEC_COUNTER_SLOTS];
    for (slot, mode) in modes.iter().enumerate() {
        // 1 == ExecMode::Live. Paper and Off slots cost no names.
        if *mode != 1 {
            continue;
        }
        slots[slot] = Some(ExecSlotMetricIds {
            mode: reg
                .register_gauge(&format!("engine_exec_slot{slot}_mode"))
                .map_err(|_| "register exec slot gauge")?,
            live_submits: reg
                .register_counter(&format!("engine_exec_slot{slot}_live_submits_total"))
                .map_err(|_| "register exec slot counter")?,
            refused: reg
                .register_counter(&format!("engine_exec_slot{slot}_refused_total"))
                .map_err(|_| "register exec slot counter")?,
            halted: reg
                .register_gauge(&format!("engine_exec_slot{slot}_halted"))
                .map_err(|_| "register exec slot gauge")?,
        });
    }
    Ok(ExecMetricIds {
        configured,
        live_submits,
        paper_submits,
        refused_off,
        refused_no_route,
        refused_risk,
        refused_halted,
        refused_unseeded,
        halts,
        cancel_all_failures,
        cancel_all_stranded,
        cancel_on_off,
        ledger,
        arm,
        seeded,
        budget_remaining,
        pnl_anchor,
        session_pnl,
        slots,
    })
}

/// E1: mirror the router's counters as monotonic deltas, same shape and
/// cadence as every other family here.
fn mirror_exec_metrics(
    reg: &core_metrics::MetricsRegistry,
    ids: &ExecMetricIds,
    cur: clob_dispatcher::ExecCounters,
    last: &mut clob_dispatcher::ExecCounters,
) {
    reg.gauge(ids.configured).set(i64::from(cur.configured));
    reg.counter(ids.live_submits)
        .inc(cur.live_submits.saturating_sub(last.live_submits));
    reg.counter(ids.paper_submits)
        .inc(cur.paper_submits.saturating_sub(last.paper_submits));
    reg.counter(ids.refused_off)
        .inc(cur.refused_off.saturating_sub(last.refused_off));
    reg.counter(ids.refused_no_route)
        .inc(cur.refused_no_route.saturating_sub(last.refused_no_route));
    reg.counter(ids.refused_risk)
        .inc(cur.refused_risk.saturating_sub(last.refused_risk));
    reg.counter(ids.refused_halted)
        .inc(cur.refused_halted.saturating_sub(last.refused_halted));
    reg.counter(ids.refused_unseeded)
        .inc(cur.refused_unseeded.saturating_sub(last.refused_unseeded));
    reg.counter(ids.halts)
        .inc(cur.halts.saturating_sub(last.halts));
    reg.counter(ids.cancel_all_failures)
        .inc(cur.cancel_all_failures.saturating_sub(last.cancel_all_failures));
    reg.counter(ids.cancel_all_stranded)
        .inc(cur.cancel_all_stranded.saturating_sub(last.cancel_all_stranded));
    reg.counter(ids.cancel_on_off)
        .inc(cur.cancel_on_off.saturating_sub(last.cancel_on_off));
    let led_cur = [
        cur.ledger_fills_unbound,
        cur.ledger_sells_below_zero,
        cur.ledger_binds_refused,
        cur.ledger_resting_full,
        cur.ledger_resting_ambiguous,
        cur.ledger_settles_unmatched,
    ];
    let led_last = [
        last.ledger_fills_unbound,
        last.ledger_sells_below_zero,
        last.ledger_binds_refused,
        last.ledger_resting_full,
        last.ledger_resting_ambiguous,
        last.ledger_settles_unmatched,
    ];
    for i in 0..6 {
        reg.counter(ids.ledger[i]).inc(led_cur[i].saturating_sub(led_last[i]));
    }
    let arm_cur = live_arm_counter_values(&cur.arm);
    let arm_last = live_arm_counter_values(&last.arm);
    for i in 0..LIVE_ARM_COUNTER_NAMES.len() {
        reg.counter(ids.arm[i]).inc(arm_cur[i].saturating_sub(arm_last[i]));
    }
    reg.gauge(ids.budget_remaining).set(cur.arm.budget_remaining);
    reg.gauge(ids.pnl_anchor).set(cur.arm.pnl_anchor_usd_1e6);
    reg.gauge(ids.session_pnl).set(cur.arm.session_pnl_usd_1e6);
    reg.gauge(ids.seeded).set(i64::from(cur.seeded));
    for (s, slot) in ids.slots.iter().enumerate() {
        let Some(slot) = slot else { continue };
        reg.gauge(slot.mode).set(i64::from(cur.modes[s]));
        reg.gauge(slot.halted).set(i64::from(cur.halted[s]));
        reg.counter(slot.live_submits).inc(
            cur.live_submits_by_slot[s].saturating_sub(last.live_submits_by_slot[s]),
        );
        reg.counter(slot.refused)
            .inc(cur.refused_by_slot[s].saturating_sub(last.refused_by_slot[s]));
    }
    *last = cur;
}

/// X1: mirror the matcher's counters as monotonic deltas.
fn mirror_paper_matcher_metrics(
    reg: &core_metrics::MetricsRegistry,
    ids: &PaperMatcherMetricIds,
    cur: clob_dispatcher::MatcherCounters,
    open_orders: usize,
    unrouted: [u64; 2],
    lifecycle: engine::LifecycleCounters,
    last: &mut clob_dispatcher::MatcherCounters,
    last_unrouted: &mut [u64; 2],
    last_lifecycle: &mut engine::LifecycleCounters,
) {
    // `unrouted` = [fills, order events] the set could not route.
    let fills_unrouted = unrouted[0];
    reg.counter(ids.intake)
        .inc(cur.intake.saturating_sub(last.intake));
    reg.counter(ids.fills)
        .inc(cur.fills.saturating_sub(last.fills));
    reg.counter(ids.ioc_canceled)
        .inc(cur.ioc_canceled.saturating_sub(last.ioc_canceled));
    reg.counter(ids.ttl_expired)
        .inc(cur.ttl_expired.saturating_sub(last.ttl_expired));
    reg.counter(ids.rejected_open_cap)
        .inc(cur.rejected_open_cap.saturating_sub(last.rejected_open_cap));
    reg.counter(ids.unroutable)
        .inc(cur.unroutable.saturating_sub(last.unroutable));
    reg.counter(ids.out_overflow)
        .inc(cur.out_overflow.saturating_sub(last.out_overflow));
    reg.counter(ids.fills_unrouted)
        .inc(fills_unrouted.saturating_sub(last_unrouted[0]));
    reg.counter(ids.order_events_unrouted)
        .inc(unrouted[1].saturating_sub(last_unrouted[1]));
    reg.counter(ids.cancels)
        .inc(cur.cancels.saturating_sub(last.cancels));
    reg.counter(ids.modifies)
        .inc(cur.modifies.saturating_sub(last.modifies));
    reg.counter(ids.no_such_order)
        .inc(cur.no_such_order.saturating_sub(last.no_such_order));
    reg.counter(ids.identity_mismatch)
        .inc(cur.identity_mismatch.saturating_sub(last.identity_mismatch));
    reg.counter(ids.ambiguous_order)
        .inc(cur.ambiguous_order.saturating_sub(last.ambiguous_order));
    // HYPARB H6: the AMM fill law's verdicts.
    reg.counter(ids.amm[0])
        .inc(cur.amm_fills.saturating_sub(last.amm_fills));
    reg.counter(ids.amm[1])
        .inc(cur.amm_canceled.saturating_sub(last.amm_canceled));
    reg.counter(ids.amm[2])
        .inc(cur.amm_partial.saturating_sub(last.amm_partial));
    reg.counter(ids.amm[3])
        .inc(cur.amm_not_live.saturating_sub(last.amm_not_live));
    // XMM XH2: the queue law's verdicts.
    reg.counter(ids.queue[0])
        .inc(cur.queue_placed.saturating_sub(last.queue_placed));
    reg.counter(ids.queue[1])
        .inc(cur.queue_rested.saturating_sub(last.queue_rested));
    reg.counter(ids.queue[2])
        .inc(cur.queue_rejected_alo.saturating_sub(last.queue_rejected_alo));
    reg.counter(ids.queue[3])
        .inc(cur.queue_canceled.saturating_sub(last.queue_canceled));
    reg.counter(ids.queue[4])
        .inc(cur.queue_fills.saturating_sub(last.queue_fills));
    reg.counter(ids.queue[5])
        .inc(cur.order_events_overflow.saturating_sub(last.order_events_overflow));
    // E5: the engine-level tally, which counts verbs on BOTH arms —
    // the matcher family above sees only the paper one, so a live
    // slot's cancels would otherwise be invisible here.
    reg.counter(ids.lifecycle_cancels_ok)
        .inc(lifecycle.cancels_ok.saturating_sub(last_lifecycle.cancels_ok));
    reg.counter(ids.lifecycle_cancels_err)
        .inc(lifecycle
            .cancels_err
            .saturating_sub(last_lifecycle.cancels_err));
    reg.counter(ids.lifecycle_modifies_ok)
        .inc(lifecycle
            .modifies_ok
            .saturating_sub(last_lifecycle.modifies_ok));
    reg.counter(ids.lifecycle_modifies_err)
        .inc(lifecycle
            .modifies_err
            .saturating_sub(last_lifecycle.modifies_err));
    reg.gauge(ids.open_orders).set(open_orders as i64);
    *last = cur;
    *last_unrouted = unrouted;
    *last_lifecycle = lifecycle;
}

/// VRP V8a: rewrite `vrp-state.tsv` when, and only when, the member's
/// state epoch moved.
///
/// Same 5 s cadence as the metrics mirror, and for the same reason: it
/// is the cold path the engine already visits. A quiet engine writes
/// nothing at all — the epoch is the member's own answer to "did
/// anything that outlives this process change", so there is no polling
/// of contents and no guessing.
///
/// A failed write is LOGGED, never fatal. The alternative is taking a
/// running engine down over a full disk while it holds a position, which
/// is strictly worse than losing the ability to restore one.
fn write_vrp_state_if_changed<S: strategy_core::StrategyCounters>(
    path: Option<&std::path::Path>,
    strat: &S,
    last_epoch: &mut u64,
    buf: &mut String,
    last_warn_ns: &mut u64,
    now: u64,
) {
    let Some(path) = path else { return };
    let epoch = strategy_core::StrategyCounters::vrp_state_epoch(strat);
    if epoch == *last_epoch {
        return;
    }
    if !strategy_core::StrategyCounters::render_vrp_state(strat, buf) {
        return;
    }
    match crate::vrp_boot::write_state(path, buf) {
        Ok(()) => *last_epoch = epoch,
        Err(reason) => warn_state_write("vrp", &reason, last_warn_ns, now),
    }
}

/// F18: a failing state write repeats every 5 s for as long as the
/// cause lasts — a full disk lasts hours. One line a minute per writer
/// says the same thing and leaves the log readable.
pub(crate) fn warn_state_write(kind: &str, reason: &str, last_warn_ns: &mut u64, now: u64) {
    const STATE_WARN_PERIOD_NS: u64 = 60_000_000_000;
    if *last_warn_ns != 0 && now.saturating_sub(*last_warn_ns) < STATE_WARN_PERIOD_NS {
        return;
    }
    *last_warn_ns = now.max(1);
    tracing::warn!(kind, reason, "state write failed — will retry");
}

/// Mirror the VRP family as monotonic deltas of the cumulative strategy
/// counters, plus the three QLIKE gauges as levels. 5 s cadence — cold
/// path.
fn mirror_vrp_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &VrpMetricIds,
    strat: &S,
    last: &mut strategy_core::VrpCounters,
) {
    let cur = strat.vrp_counters();
    reg.counter(ids.decisions)
        .inc(cur.decisions.saturating_sub(last.decisions));
    reg.counter(ids.decisions_late)
        .inc(cur.decisions_late.saturating_sub(last.decisions_late));
    reg.counter(ids.entries)
        .inc(cur.entries.saturating_sub(last.entries));
    reg.counter(ids.hedges)
        .inc(cur.hedges.saturating_sub(last.hedges));
    reg.counter(ids.exits)
        .inc(cur.exits.saturating_sub(last.exits));
    reg.counter(ids.holds)
        .inc(cur.holds.saturating_sub(last.holds));
    reg.counter(ids.holds_side)
        .inc(cur.holds_side.saturating_sub(last.holds_side));
    reg.counter(ids.select_scans)
        .inc(cur.select_scans.saturating_sub(last.select_scans));
    reg.counter(ids.entries_submitted)
        .inc(cur.entries_submitted.saturating_sub(last.entries_submitted));
    reg.counter(ids.entries_unfilled)
        .inc(cur.entries_unfilled.saturating_sub(last.entries_unfilled));
    reg.counter(ids.hedge_unfilled)
        .inc(cur.hedge_unfilled.saturating_sub(last.hedge_unfilled));
    reg.counter(ids.hedge_abandoned)
        .inc(cur.hedge_abandoned.saturating_sub(last.hedge_abandoned));
    reg.counter(ids.fills).inc(cur.fills.saturating_sub(last.fills));
    reg.counter(ids.fills_ignored)
        .inc(cur.fills_ignored.saturating_sub(last.fills_ignored));
    reg.counter(ids.entry_maker_submitted)
        .inc(cur.entry_maker_submitted.saturating_sub(last.entry_maker_submitted));
    reg.counter(ids.entry_crossed)
        .inc(cur.entry_crossed.saturating_sub(last.entry_crossed));
    reg.counter(ids.entry_cost_refused)
        .inc(cur.entry_cost_refused.saturating_sub(last.entry_cost_refused));
    reg.counter(ids.hedge_crossed)
        .inc(cur.hedge_crossed.saturating_sub(last.hedge_crossed));
    reg.counter(ids.settle_index_fallback)
        .inc(cur.settle_index_fallback.saturating_sub(last.settle_index_fallback));
    reg.counter(ids.iv_median_fallback)
        .inc(cur.iv_median_fallback.saturating_sub(last.iv_median_fallback));
    reg.counter(ids.holds_cost)
        .inc(cur.holds_cost.saturating_sub(last.holds_cost));
    reg.counter(ids.no_bounds)
        .inc(cur.no_bounds.saturating_sub(last.no_bounds));
    reg.counter(ids.stale_skips)
        .inc(cur.stale_skips.saturating_sub(last.stale_skips));
    reg.counter(ids.records_ignored)
        .inc(cur.records_ignored.saturating_sub(last.records_ignored));
    reg.counter(ids.no_selection)
        .inc(cur.no_selection.saturating_sub(last.no_selection));
    reg.counter(ids.regime_blocked)
        .inc(cur.regime_blocked.saturating_sub(last.regime_blocked));
    reg.counter(ids.regime_exits)
        .inc(cur.regime_exits.saturating_sub(last.regime_exits));
    reg.counter(ids.settlements)
        .inc(cur.settlements.saturating_sub(last.settlements));
    reg.counter(ids.caps_rejected)
        .inc(cur.caps_rejected.saturating_sub(last.caps_rejected));
    reg.counter(ids.settled_itm)
        .inc(cur.settled_itm.saturating_sub(last.settled_itm));
    reg.counter(ids.settled_otm)
        .inc(cur.settled_otm.saturating_sub(last.settled_otm));
    reg.counter(ids.settled_unpriced)
        .inc(cur.settled_unpriced.saturating_sub(last.settled_unpriced));
    reg.gauge(ids.killed).set(cur.killed as i64);
    reg.gauge(ids.qlike_iv_1e6).set(cur.qlike_iv_1e6);
    reg.gauge(ids.qlike_har_1e6).set(cur.qlike_har_1e6);
    reg.gauge(ids.qlike_har_beats_iv)
        .set(cur.qlike_har_beats_iv as i64);
    reg.gauge(ids.last_settle_value_1e6)
        .set(strategy_core::StrategyCounters::vrp_last_settle_value_1e6(strat));
    reg.gauge(ids.regime_offset_1e6)
        .set(strategy_core::StrategyCounters::vrp_regime_offset_1e6(strat));
    *last = cur;
}

// ---------------------------------------------------------------
// XMM XH3 — slot-6 observability (5 s mirror, cold path)
// ---------------------------------------------------------------

/// XMM XH3: perps carried by the per-perp gauges (the first N
/// configured; `/state` carries all eight).
pub const XMM_METRIC_PERPS: usize = 4;

/// The counter rows of the xmm family, in [`xmm_counter_values`] order
/// — which is what pins the two together.
const XMM_COUNTER_NAMES: [&str; 17] = [
    "engine_xmm_placed_total",
    "engine_xmm_modifies_total",
    "engine_xmm_lead_cancels_total",
    "engine_xmm_requote_cancels_total",
    "engine_xmm_pull_cancels_total",
    "engine_xmm_expiry_cancels_total",
    "engine_xmm_gated_total",
    "engine_xmm_gate_overflow_total",
    "engine_xmm_capped_total",
    "engine_xmm_rejected_alo_total",
    "engine_xmm_rejected_other_total",
    "engine_xmm_canceled_total",
    "engine_xmm_filled_total",
    "engine_xmm_fills_total",
    "engine_xmm_unmatched_total",
    "engine_xmm_ctx_refused_total",
    "engine_xmm_stuck_total",
];

/// `XmmCounters`' fields in [`XMM_COUNTER_NAMES`] order.
// COPY: [u64; 17] (136 B) returned by value — cold, the 5 s /metrics
// mirror; the fields are visited once in the name order — a borrowed
// view would need the struct to be an array, which the POD is not.
fn xmm_counter_values(c: &strategy_core::XmmCounters) -> [u64; 17] {
    [
        c.placed,
        c.modifies,
        c.lead_cancels,
        c.requote_cancels,
        c.pull_cancels,
        c.expiry_cancels,
        c.gated,
        c.gate_overflow,
        c.capped,
        c.rejected_alo,
        c.rejected_other,
        c.canceled,
        c.filled,
        c.fills,
        c.unmatched,
        c.ctx_refused,
        c.stuck,
    ]
}

/// XMM XH3: the `engine_xmm_*` family (slot 6) — what the member did
/// (the XH3 bar reads `stuck`, the pulls by reason and the requote
/// rate from here), how many perps it quotes, and each perp's position.
#[derive(Copy, Clone, Debug)]
pub struct XmmMetricIds {
    /// The counters, in [`XMM_COUNTER_NAMES`] order.
    pub counters: [core_metrics::CounterId; 17],
    /// `engine_xmm_perps` — perps configured (0 = the member is off).
    pub perps: core_metrics::GaugeId,
    /// `engine_xmm_p{k}_pos_1e6` — the signed position on the k-th
    /// configured perp, base × 1e6.
    pub pos: [core_metrics::GaugeId; XMM_METRIC_PERPS],
}

/// Register the xmm family. Boot-only.
fn register_xmm_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<XmmMetricIds, &'static str> {
    let mut counters = [core_metrics::CounterId::default(); 17];
    let mut i = 0usize;
    while i < XMM_COUNTER_NAMES.len() {
        counters[i] = reg
            .register_counter(XMM_COUNTER_NAMES[i])
            .map_err(|_| "register xmm counter")?;
        i += 1;
    }
    let perps = reg
        .register_gauge("engine_xmm_perps")
        .map_err(|_| "register xmm gauge")?;
    let mut pos = [core_metrics::GaugeId::default(); XMM_METRIC_PERPS];
    let mut k = 0usize;
    while k < XMM_METRIC_PERPS {
        pos[k] = reg
            .register_gauge(&format!("engine_xmm_p{k}_pos_1e6"))
            .map_err(|_| "register xmm gauge")?;
        k += 1;
    }
    Ok(XmmMetricIds {
        counters,
        perps,
        pos,
    })
}

/// Mirror the xmm family: the counters as monotonic deltas of the
/// member's cumulative ones, the gauges as levels. 5 s cadence — cold
/// path (the perp rows are read into a stack buffer, never allocated).
fn mirror_xmm_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &XmmMetricIds,
    strat: &S,
    last: &mut strategy_core::XmmCounters,
) {
    // COPY: the 136 B `XmmCounters` read into `cur`, then kept as the
    // next delta's baseline (`*last = cur`) — 5 s cadence, cold; a delta
    // needs the previous value, so one copy survives either way.
    let mut cur = strategy_core::XmmCounters::default();
    strat.xmm_counters(&mut cur);
    let c = xmm_counter_values(&cur);
    let l = xmm_counter_values(last);
    let mut i = 0usize;
    while i < c.len() {
        reg.counter(ids.counters[i]).inc(c[i].saturating_sub(l[i]));
        i += 1;
    }
    // COPY: 136 B — the next delta's baseline (see above).
    *last = cur;
    let mut rows = [strategy_core::XmmPerpView::default(); XMM_METRIC_PERPS];
    let n = strat.xmm_perps_view(&mut rows);
    reg.gauge(ids.perps).set(i64::from(n));
    let mut k = 0usize;
    while k < XMM_METRIC_PERPS {
        reg.gauge(ids.pos[k]).set(if (k as u32) < n { rows[k].pos_1e6 } else { 0 });
        k += 1;
    }
}

/// HYPARB H6: coins carried by the per-coin gauges (the first N configured;
/// `/state` carries all eight).
pub const HYPARB_METRIC_COINS: usize = 4;
/// HYPARB H6: pools carried by the per-pool gauges (the first N
/// configured; `/state` carries 64, the capture every pool).
pub const HYPARB_METRIC_POOLS: usize = 4;

/// The counter rows of the hyparb family, in [`hyparb_counter_values`]
/// order — which is what pins the two together.
const HYPARB_COUNTER_NAMES: [&str; 27] = [
    "engine_hyparb_pool_events_total",
    "engine_hyparb_pool_refused_total",
    "engine_hyparb_maps_loaded_total",
    "engine_hyparb_maps_refused_total",
    "engine_hyparb_evaluations_total",
    "engine_hyparb_arbs_submitted_total",
    "engine_hyparb_side_buy_total",
    "engine_hyparb_side_sell_total",
    "engine_hyparb_skipped_below_min_total",
    "engine_hyparb_skipped_not_live_total",
    "engine_hyparb_skipped_no_hedge_total",
    "engine_hyparb_skipped_inflight_total",
    "engine_hyparb_skipped_cooldown_total",
    "engine_hyparb_skipped_halted_total",
    "engine_hyparb_size_capped_total",
    "engine_hyparb_amm_fills_total",
    "engine_hyparb_hedges_submitted_total",
    "engine_hyparb_hedge_venue_perp_total",
    "engine_hyparb_hedge_venue_spot_total",
    "engine_hyparb_hedge_fills_total",
    "engine_hyparb_hedges_missed_total",
    "engine_hyparb_flattens_submitted_total",
    "engine_hyparb_inventory_breaches_total",
    "engine_hyparb_orders_dropped_total",
    "engine_hyparb_gas_charged_usd_1e6_total",
    "engine_hyparb_pnl_predicted_usd_1e6_total",
    "engine_hyparb_amm_notional_usd_1e6_total",
];

/// `HyparbCounters`' cumulative fields in [`HYPARB_COUNTER_NAMES`] order.
/// The three money sums are non-negative by construction (gas and
/// notional are charged, the prediction only ever adds a positive
/// quote), so they mirror as counters; the two LEVELS
/// (`funding_earned_usd_1e6`, signed, and `halted`) are gauges.
// COPY: [u64; 27] (216 B) returned by value — cold, the 5 s /metrics
// mirror; the fields are visited once in the name order — a borrowed
// view would need the struct to be an array, which the POD is not.
fn hyparb_counter_values(c: &strategy_core::HyparbCounters) -> [u64; 27] {
    [
        c.pool_events,
        c.pool_refused,
        c.maps_loaded,
        c.maps_refused,
        c.evaluations,
        c.arbs_submitted,
        c.arbs_buy,
        c.arbs_sell,
        c.skipped_below_min,
        c.skipped_not_live,
        c.skipped_no_hedge,
        c.skipped_inflight,
        c.skipped_cooldown,
        c.skipped_halted,
        c.size_capped,
        c.amm_fills,
        c.hedges_submitted,
        c.hedges_perp,
        c.hedges_spot,
        c.hedge_fills,
        c.hedges_missed,
        c.flattens_submitted,
        c.inventory_breaches,
        c.orders_dropped,
        c.gas_charged_usd_1e6.max(0) as u64,
        c.pnl_predicted_usd_1e6.max(0) as u64,
        c.amm_notional_usd_1e6.max(0) as u64,
    ]
}

/// Per-coin gauge suffixes, in [`mirror_hyparb_metrics`]' write order.
const HYPARB_COIN_GAUGES: [&str; 5] = [
    "perp_depth_usd_1e6",
    "spot_depth_usd_1e6",
    "perp_cost_bps_1e6",
    "spot_cost_bps_1e6",
    "inventory_1e6",
];
/// Per-pool gauge suffixes, in [`mirror_hyparb_metrics`]' write order.
const HYPARB_POOL_GAUGES: [&str; 3] = ["basis_bps_1e6", "pnl_predicted_usd_1e6", "live"];

/// HYPARB H6: the `engine_hyparb_*` family (slot 0) — plan §10's six
/// disputed quantities as series: the depth the hedge books showed and
/// how often a cap cut the size (#1), the basis per pool and the side
/// balance (#2), the prediction per pool (#4), and each venue's hedge
/// count, quoted cost and the funding the perp earned (#6).
#[derive(Copy, Clone, Debug)]
pub struct HyparbMetricIds {
    /// The counters, in [`HYPARB_COUNTER_NAMES`] order.
    pub counters: [core_metrics::CounterId; 27],
    /// `engine_hyparb_funding_earned_usd_1e6` (signed level).
    pub funding_earned: core_metrics::GaugeId,
    /// `engine_hyparb_halted` (0/1).
    pub halted: core_metrics::GaugeId,
    /// `engine_hyparb_pnl_session_usd_1e6` — the member's marked session
    /// P&L (signed): a level, paper's evidence for gate G1.
    pub pnl_session: core_metrics::GaugeId,
    /// `engine_hyparb_pools_live` — pools judgeable right now.
    pub pools_live: core_metrics::GaugeId,
    /// `engine_hyparb_c<k>_<suffix>` for the first
    /// [`HYPARB_METRIC_COINS`] coins.
    pub coins: [[core_metrics::GaugeId; 5]; HYPARB_METRIC_COINS],
    /// `engine_hyparb_p<k>_<suffix>` for the first
    /// [`HYPARB_METRIC_POOLS`] pools.
    pub pools: [[core_metrics::GaugeId; 3]; HYPARB_METRIC_POOLS],
}

/// HYPARB H8: the testnet write path's shadow — the tap's and the
/// `evm-shadow` thread's counters and levels (`cli::evm_testnet`).
#[derive(Copy, Clone, Debug)]
pub struct HyparbEvmMetricIds {
    /// In [`crate::evm_testnet::SHADOW_COUNTER_NAMES`] order.
    pub counters: [core_metrics::CounterId; crate::evm_testnet::SHADOW_COUNTER_NAMES.len()],
    /// In [`crate::evm_testnet::SHADOW_GAUGE_NAMES`] order.
    pub gauges: [core_metrics::GaugeId; crate::evm_testnet::SHADOW_GAUGE_NAMES.len()],
    /// `engine_hyparb_evm_dark`: 1 when `mode = "testnet"` booted with the
    /// shadow DARK (H9: the reason is the boot's ERROR line) — the one
    /// level that exists without a shadow.
    pub dark: core_metrics::GaugeId,
}

/// Register the shadow family: 18 counters, 5 gauges (the shadow's 4 +
/// `dark`). UNCONDITIONAL — a paper boot exposes the rows at zero.
fn register_hyparb_evm_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<HyparbEvmMetricIds, &'static str> {
    use crate::evm_testnet::{SHADOW_COUNTER_NAMES, SHADOW_GAUGE_NAMES};
    let mut counters = [core_metrics::CounterId::default(); SHADOW_COUNTER_NAMES.len()];
    let mut i = 0usize;
    while i < SHADOW_COUNTER_NAMES.len() {
        counters[i] = reg
            .register_counter(SHADOW_COUNTER_NAMES[i])
            .map_err(|_| "register hyparb evm counter")?;
        i += 1;
    }
    let mut gauges = [core_metrics::GaugeId::default(); SHADOW_GAUGE_NAMES.len()];
    let mut g = 0usize;
    while g < SHADOW_GAUGE_NAMES.len() {
        gauges[g] = reg
            .register_gauge(SHADOW_GAUGE_NAMES[g])
            .map_err(|_| "register hyparb evm gauge")?;
        g += 1;
    }
    let dark = reg
        .register_gauge("engine_hyparb_evm_dark")
        .map_err(|_| "register hyparb evm dark gauge")?;
    Ok(HyparbEvmMetricIds {
        counters,
        gauges,
        dark,
    })
}

/// Mirror the shadow's status (counters as deltas, gauges as levels) and
/// the dark flag. No shadow ⇒ only `dark` moves.
fn mirror_hyparb_evm_metrics(
    reg: &core_metrics::MetricsRegistry,
    ids: &HyparbEvmMetricIds,
    status: Option<&crate::evm_testnet::ShadowStatus>,
    dark: bool,
    last: &mut [u64; crate::evm_testnet::SHADOW_COUNTER_NAMES.len()],
) {
    reg.gauge(ids.dark).set(i64::from(dark));
    let Some(st) = status else { return };
    let mut i = 0usize;
    while i < last.len() {
        let now = st.counter(i);
        reg.counter(ids.counters[i])
            .inc(now.saturating_sub(last[i]));
        last[i] = now;
        i += 1;
    }
    let mut g = 0usize;
    while g < ids.gauges.len() {
        reg.gauge(ids.gauges[g])
            .set(i64::try_from(st.gauge(g)).unwrap_or(i64::MAX));
        g += 1;
    }
}

/// Register the hyparb family: 27 counters, 4 + 4×5 + 4×3 = 36 gauges.
/// UNCONDITIONAL like every family — a mask without slot 0 exposes the
/// rows at zero, which is how an operator tells "off" from "broken".
fn register_hyparb_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<HyparbMetricIds, &'static str> {
    let mut counters = [core_metrics::CounterId::default(); 27];
    let mut i = 0usize;
    while i < HYPARB_COUNTER_NAMES.len() {
        counters[i] = reg
            .register_counter(HYPARB_COUNTER_NAMES[i])
            .map_err(|_| "register hyparb counter")?;
        i += 1;
    }
    let mut gauge = |name: &str| -> Result<core_metrics::GaugeId, &'static str> {
        reg.register_gauge(name)
            .map_err(|_| "register hyparb gauge")
    };
    let funding_earned = gauge("engine_hyparb_funding_earned_usd_1e6")?;
    let halted = gauge("engine_hyparb_halted")?;
    let pnl_session = gauge("engine_hyparb_pnl_session_usd_1e6")?;
    let pools_live = gauge("engine_hyparb_pools_live")?;
    let mut coins = [[core_metrics::GaugeId::default(); 5]; HYPARB_METRIC_COINS];
    let mut k = 0usize;
    while k < HYPARB_METRIC_COINS {
        let mut g = 0usize;
        while g < HYPARB_COIN_GAUGES.len() {
            coins[k][g] = gauge(&format!("engine_hyparb_c{k}_{}", HYPARB_COIN_GAUGES[g]))?;
            g += 1;
        }
        k += 1;
    }
    let mut pools = [[core_metrics::GaugeId::default(); 3]; HYPARB_METRIC_POOLS];
    let mut k = 0usize;
    while k < HYPARB_METRIC_POOLS {
        let mut g = 0usize;
        while g < HYPARB_POOL_GAUGES.len() {
            pools[k][g] = gauge(&format!("engine_hyparb_p{k}_{}", HYPARB_POOL_GAUGES[g]))?;
            g += 1;
        }
        k += 1;
    }
    Ok(HyparbMetricIds {
        counters,
        funding_earned,
        halted,
        pnl_session,
        pools_live,
        coins,
        pools,
    })
}

/// Mirror the hyparb family: counters as deltas of the cumulative
/// member counters, levels as sets. 5 s cadence — cold path.
fn mirror_hyparb_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &HyparbMetricIds,
    strat: &S,
    last: &mut strategy_core::HyparbCounters,
) {
    let cur = strat.hyparb_counters();
    let now = hyparb_counter_values(&cur);
    let was = hyparb_counter_values(last);
    let mut i = 0usize;
    while i < now.len() {
        reg.counter(ids.counters[i])
            .inc(now[i].saturating_sub(was[i]));
        i += 1;
    }
    *last = cur;
    reg.gauge(ids.funding_earned)
        .set(cur.funding_earned_usd_1e6);
    reg.gauge(ids.halted).set(cur.halted as i64);
    reg.gauge(ids.pnl_session).set(cur.pnl_session_usd_1e6);
    // Levels: a coin or pool the member does not configure keeps its row
    // at zero rather than vanishing.
    let mut coins = [strategy_core::HyparbCoinView::default(); HYPARB_METRIC_COINS];
    strat.hyparb_coins_view(&mut coins);
    let mut k = 0usize;
    while k < HYPARB_METRIC_COINS {
        let c = &coins[k];
        let v = [
            c.perp_depth_usd_1e6,
            c.spot_depth_usd_1e6,
            c.perp_cost_bps_1e6,
            c.spot_cost_bps_1e6,
            c.inventory_1e6,
        ];
        let mut g = 0usize;
        while g < v.len() {
            reg.gauge(ids.coins[k][g]).set(v[g]);
            g += 1;
        }
        k += 1;
    }
    // Every pool's liveness (the view is read in full once), the first
    // N pools' rows.
    let mut pools = [strategy_core::HyparbPoolView::default(); strategy_hyparb::HYPARB_MAX_POOLS];
    let n = (strat.hyparb_pools_view(&mut pools) as usize).min(pools.len());
    let mut live = 0i64;
    let mut p = 0usize;
    while p < n {
        live += i64::from(pools[p].live);
        p += 1;
    }
    reg.gauge(ids.pools_live).set(live);
    let mut k = 0usize;
    while k < HYPARB_METRIC_POOLS {
        let r = &pools[k];
        reg.gauge(ids.pools[k][0]).set(r.basis_bps_1e6);
        reg.gauge(ids.pools[k][1]).set(r.pnl_predicted_usd_1e6);
        reg.gauge(ids.pools[k][2]).set(i64::from(r.live));
        k += 1;
    }
}

/// BIN15 O4b: the `engine_bin15_*` family (slot 3). Counters mirror the
/// member's cumulative [`strategy_core::Bin15Counters`] as deltas; the
/// per-family rows are gauges, because every one of them is a LEVEL —
/// what a slot holds right now, not how often it changed.
#[derive(Copy, Clone, Debug)]
pub struct Bin15MetricIds {
    /// `engine_bin15_reprices_total`
    pub reprices: core_metrics::CounterId,
    /// `engine_bin15_rolls_total`
    pub rolls: core_metrics::CounterId,
    /// `engine_bin15_rolls_settled_total`
    pub rolls_settled: core_metrics::CounterId,
    /// `engine_bin15_spec_overrides_total`
    pub spec_overrides: core_metrics::CounterId,
    /// `engine_bin15_spec_refused_total`
    pub spec_refused: core_metrics::CounterId,
    /// `engine_bin15_takes_submitted_total`
    pub takes_submitted: core_metrics::CounterId,
    /// `engine_bin15_takes_filled_total`
    pub takes_filled: core_metrics::CounterId,
    /// `engine_bin15_takes_unfilled_total`
    pub takes_unfilled: core_metrics::CounterId,
    /// `engine_bin15_quotes_submitted_total`
    pub quotes_submitted: core_metrics::CounterId,
    /// `engine_bin15_quotes_filled_total`
    pub quotes_filled: core_metrics::CounterId,
    /// `engine_bin15_quotes_expired_total`
    pub quotes_expired: core_metrics::CounterId,
    /// `engine_bin15_quotes_modified_total` (E5, LAW E-7) — Arm B
    /// quotes repriced in place instead of waiting for a TTL.
    pub quotes_modified: core_metrics::CounterId,
    /// `engine_bin15_quotes_modify_refused_total` (E5). `NoSuchOrder`
    /// is the common cause and is a race the venue won, not a defect.
    pub quotes_modify_refused: core_metrics::CounterId,
    /// `engine_bin15_quotes_cancelled_total` (E5, LAW E-8) — lapsed
    /// quotes the member took back, because the venue has no
    /// server-side TTL on a Gtc/Alo order.
    pub quotes_cancelled: core_metrics::CounterId,
    /// `engine_bin15_quotes_cancel_refused_total` (E5). **This one
    /// matters**: the member clears its book at its own deadline
    /// either way, so a non-zero value is a quote the member has
    /// stopped tracking and the venue may still hold.
    pub quotes_cancel_refused: core_metrics::CounterId,
    /// `engine_bin15_quotes_raced_total` (E5) — fills booked against
    /// a quote's PREVIOUS client id. Before the one-generation memory
    /// these landed in `unknown_fills` and moved no position.
    pub quotes_raced: core_metrics::CounterId,
    /// `engine_bin15_skipped_partial_total` (E5) — reprices held
    /// because the resting quote is partially filled.
    pub skipped_partial: core_metrics::CounterId,
    /// `engine_bin15_closes_submitted_total`
    pub closes_submitted: core_metrics::CounterId,
    /// `engine_bin15_skipped_tau_total`
    pub skipped_tau: core_metrics::CounterId,
    /// `engine_bin15_skipped_tail_total`
    pub skipped_tail: core_metrics::CounterId,
    /// `engine_bin15_skipped_stale_total`
    pub skipped_stale: core_metrics::CounterId,
    /// `engine_bin15_skipped_mark_stale_total`
    pub skipped_mark_stale: core_metrics::CounterId,
    /// `engine_bin15_skipped_book_total`
    pub skipped_book: core_metrics::CounterId,
    /// `engine_bin15_skipped_inventory_total`
    pub skipped_inventory: core_metrics::CounterId,
    /// `engine_bin15_skipped_cap_total`
    pub skipped_cap: core_metrics::CounterId,
    /// `engine_bin15_skipped_grid_total`
    pub skipped_grid: core_metrics::CounterId,
    /// `engine_bin15_skipped_entry_price_total`
    pub skipped_entry_price: core_metrics::CounterId,
    /// `engine_bin15_skipped_entry_persist_total` (BIN15 S5)
    pub skipped_entry_persist: core_metrics::CounterId,
    /// `engine_bin15_skipped_entry_elapsed_total` (BIN15 S5)
    pub skipped_entry_elapsed: core_metrics::CounterId,
    /// `engine_bin15_families_dormant_total`
    pub families_dormant: core_metrics::CounterId,
    /// `engine_bin15_fills_total`
    pub fills: core_metrics::CounterId,
    /// `engine_bin15_unknown_fills_total`
    pub unknown_fills: core_metrics::CounterId,
    /// `engine_bin15_settlement_fills_total`
    ///
    /// Registered alongside `unknown_fills` deliberately: settlements
    /// used to be counted there, so leaving this one unpublished would
    /// move them from a visible series to a field only a unit test can
    /// see — a regression dressed as a fix.
    pub settlement_fills: core_metrics::CounterId,
    /// Per family: `p_hat_1e6`, `pos_yes_1e6`, `pos_no_1e6`,
    /// `live_outcome`, in that order.
    pub families: [[core_metrics::GaugeId; BIN15_FAMILY_GAUGES]; BIN15_METRIC_FAMILIES],
}

/// Families the `engine_bin15_f<n>_*` rows cover. Mirrors
/// `strategy_core::BIN15_VIEW_FAMILIES` and ruling O-Q7's eight.
pub const BIN15_METRIC_FAMILIES: usize = strategy_core::BIN15_VIEW_FAMILIES;

/// Gauges per family. BIN15 O6 raised this from 4: the first four
/// say WHAT the member believes, the six added say WHY, so a
/// pinned `p̂` is explainable from a scrape instead of a replay.
pub const BIN15_FAMILY_GAUGES: usize = 10;

/// The per-family gauge names, written out rather than formatted.
///
/// `register_gauge` copies the name into a fixed buffer, so a built
/// string would work — and would also mean the only list of what this
/// engine exposes lived in a loop. An operator greps `/metrics` names;
/// these are the names.
const BIN15_GAUGE_NAMES: [[&str; BIN15_FAMILY_GAUGES]; BIN15_METRIC_FAMILIES] = [
    [
        "engine_bin15_f0_p_hat_1e6",
        "engine_bin15_f0_pos_yes_1e6",
        "engine_bin15_f0_pos_no_1e6",
        "engine_bin15_f0_live_outcome",
        "engine_bin15_f0_p_raw_1e6",
        "engine_bin15_f0_strike_1e6",
        "engine_bin15_f0_mark_1e6",
        "engine_bin15_f0_d_1e6",
        "engine_bin15_f0_den_1e9",
        "engine_bin15_f0_tau_s",
    ],
    [
        "engine_bin15_f1_p_hat_1e6",
        "engine_bin15_f1_pos_yes_1e6",
        "engine_bin15_f1_pos_no_1e6",
        "engine_bin15_f1_live_outcome",
        "engine_bin15_f1_p_raw_1e6",
        "engine_bin15_f1_strike_1e6",
        "engine_bin15_f1_mark_1e6",
        "engine_bin15_f1_d_1e6",
        "engine_bin15_f1_den_1e9",
        "engine_bin15_f1_tau_s",
    ],
    [
        "engine_bin15_f2_p_hat_1e6",
        "engine_bin15_f2_pos_yes_1e6",
        "engine_bin15_f2_pos_no_1e6",
        "engine_bin15_f2_live_outcome",
        "engine_bin15_f2_p_raw_1e6",
        "engine_bin15_f2_strike_1e6",
        "engine_bin15_f2_mark_1e6",
        "engine_bin15_f2_d_1e6",
        "engine_bin15_f2_den_1e9",
        "engine_bin15_f2_tau_s",
    ],
    [
        "engine_bin15_f3_p_hat_1e6",
        "engine_bin15_f3_pos_yes_1e6",
        "engine_bin15_f3_pos_no_1e6",
        "engine_bin15_f3_live_outcome",
        "engine_bin15_f3_p_raw_1e6",
        "engine_bin15_f3_strike_1e6",
        "engine_bin15_f3_mark_1e6",
        "engine_bin15_f3_d_1e6",
        "engine_bin15_f3_den_1e9",
        "engine_bin15_f3_tau_s",
    ],
    [
        "engine_bin15_f4_p_hat_1e6",
        "engine_bin15_f4_pos_yes_1e6",
        "engine_bin15_f4_pos_no_1e6",
        "engine_bin15_f4_live_outcome",
        "engine_bin15_f4_p_raw_1e6",
        "engine_bin15_f4_strike_1e6",
        "engine_bin15_f4_mark_1e6",
        "engine_bin15_f4_d_1e6",
        "engine_bin15_f4_den_1e9",
        "engine_bin15_f4_tau_s",
    ],
    [
        "engine_bin15_f5_p_hat_1e6",
        "engine_bin15_f5_pos_yes_1e6",
        "engine_bin15_f5_pos_no_1e6",
        "engine_bin15_f5_live_outcome",
        "engine_bin15_f5_p_raw_1e6",
        "engine_bin15_f5_strike_1e6",
        "engine_bin15_f5_mark_1e6",
        "engine_bin15_f5_d_1e6",
        "engine_bin15_f5_den_1e9",
        "engine_bin15_f5_tau_s",
    ],
    [
        "engine_bin15_f6_p_hat_1e6",
        "engine_bin15_f6_pos_yes_1e6",
        "engine_bin15_f6_pos_no_1e6",
        "engine_bin15_f6_live_outcome",
        "engine_bin15_f6_p_raw_1e6",
        "engine_bin15_f6_strike_1e6",
        "engine_bin15_f6_mark_1e6",
        "engine_bin15_f6_d_1e6",
        "engine_bin15_f6_den_1e9",
        "engine_bin15_f6_tau_s",
    ],
    [
        "engine_bin15_f7_p_hat_1e6",
        "engine_bin15_f7_pos_yes_1e6",
        "engine_bin15_f7_pos_no_1e6",
        "engine_bin15_f7_live_outcome",
        "engine_bin15_f7_p_raw_1e6",
        "engine_bin15_f7_strike_1e6",
        "engine_bin15_f7_mark_1e6",
        "engine_bin15_f7_d_1e6",
        "engine_bin15_f7_den_1e9",
        "engine_bin15_f7_tau_s",
    ],
];

/// Register the BIN15 family. Boot-only.
///
/// 22 counters and 32 gauges. Registration is UNCONDITIONAL like every
/// other family's — a mask that excludes slot 3 still exposes the rows
/// at zero, which is what lets an operator tell "off" from "broken".
fn register_bin15_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<Bin15MetricIds, &'static str> {
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register bin15 counter")
    };
    let reprices = one("engine_bin15_reprices_total")?;
    let rolls = one("engine_bin15_rolls_total")?;
    let rolls_settled = one("engine_bin15_rolls_settled_total")?;
    let spec_overrides = one("engine_bin15_spec_overrides_total")?;
    let spec_refused = one("engine_bin15_spec_refused_total")?;
    let takes_submitted = one("engine_bin15_takes_submitted_total")?;
    let takes_filled = one("engine_bin15_takes_filled_total")?;
    let takes_unfilled = one("engine_bin15_takes_unfilled_total")?;
    let quotes_submitted = one("engine_bin15_quotes_submitted_total")?;
    let quotes_filled = one("engine_bin15_quotes_filled_total")?;
    let quotes_expired = one("engine_bin15_quotes_expired_total")?;
    let quotes_modified = one("engine_bin15_quotes_modified_total")?;
    let quotes_modify_refused = one("engine_bin15_quotes_modify_refused_total")?;
    let quotes_cancelled = one("engine_bin15_quotes_cancelled_total")?;
    let quotes_cancel_refused = one("engine_bin15_quotes_cancel_refused_total")?;
    let quotes_raced = one("engine_bin15_quotes_raced_total")?;
    let skipped_partial = one("engine_bin15_skipped_partial_total")?;
    let closes_submitted = one("engine_bin15_closes_submitted_total")?;
    let skipped_tau = one("engine_bin15_skipped_tau_total")?;
    let skipped_tail = one("engine_bin15_skipped_tail_total")?;
    let skipped_stale = one("engine_bin15_skipped_stale_total")?;
    let skipped_mark_stale = one("engine_bin15_skipped_mark_stale_total")?;
    let skipped_book = one("engine_bin15_skipped_book_total")?;
    let skipped_inventory = one("engine_bin15_skipped_inventory_total")?;
    let skipped_cap = one("engine_bin15_skipped_cap_total")?;
    let skipped_grid = one("engine_bin15_skipped_grid_total")?;
    let skipped_entry_price = one("engine_bin15_skipped_entry_price_total")?;
    let skipped_entry_persist = one("engine_bin15_skipped_entry_persist_total")?;
    let skipped_entry_elapsed = one("engine_bin15_skipped_entry_elapsed_total")?;
    let families_dormant = one("engine_bin15_families_dormant_total")?;
    let fills = one("engine_bin15_fills_total")?;
    let unknown_fills = one("engine_bin15_unknown_fills_total")?;
    let settlement_fills = one("engine_bin15_settlement_fills_total")?;
    let mut families = [[core_metrics::GaugeId::default(); BIN15_FAMILY_GAUGES];
        BIN15_METRIC_FAMILIES];
    let mut f = 0usize;
    while f < BIN15_METRIC_FAMILIES {
        let mut g = 0usize;
        while g < BIN15_FAMILY_GAUGES {
            families[f][g] = reg
                .register_gauge(BIN15_GAUGE_NAMES[f][g])
                .map_err(|_| "register bin15 gauge")?;
            g += 1;
        }
        f += 1;
    }
    Ok(Bin15MetricIds {
        reprices,
        rolls,
        rolls_settled,
        spec_overrides,
        spec_refused,
        takes_submitted,
        takes_filled,
        takes_unfilled,
        quotes_submitted,
        quotes_filled,
        quotes_expired,
        quotes_modified,
        quotes_modify_refused,
        quotes_cancelled,
        quotes_cancel_refused,
        quotes_raced,
        skipped_partial,
        closes_submitted,
        skipped_tau,
        skipped_tail,
        skipped_stale,
        skipped_mark_stale,
        skipped_book,
        skipped_inventory,
        skipped_cap,
        skipped_grid,
        skipped_entry_price,
        skipped_entry_persist,
        skipped_entry_elapsed,
        families_dormant,
        fills,
        unknown_fills,
        settlement_fills,
        families,
    })
}

/// Mirror the BIN15 family as monotonic deltas of the cumulative
/// strategy counters, plus the per-family levels. 5 s cadence — cold
/// path.
fn mirror_bin15_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &Bin15MetricIds,
    strat: &S,
    last: &mut strategy_core::Bin15Counters,
) {
    let cur = strat.bin15_counters();
    reg.counter(ids.reprices)
        .inc(cur.reprices.saturating_sub(last.reprices));
    reg.counter(ids.rolls)
        .inc(cur.rolls.saturating_sub(last.rolls));
    reg.counter(ids.rolls_settled)
        .inc(cur.rolls_settled.saturating_sub(last.rolls_settled));
    reg.counter(ids.spec_overrides)
        .inc(cur.spec_overrides.saturating_sub(last.spec_overrides));
    reg.counter(ids.spec_refused)
        .inc(cur.spec_refused.saturating_sub(last.spec_refused));
    reg.counter(ids.takes_submitted)
        .inc(cur.takes_submitted.saturating_sub(last.takes_submitted));
    reg.counter(ids.takes_filled)
        .inc(cur.takes_filled.saturating_sub(last.takes_filled));
    reg.counter(ids.takes_unfilled)
        .inc(cur.takes_unfilled.saturating_sub(last.takes_unfilled));
    reg.counter(ids.quotes_submitted)
        .inc(cur.quotes_submitted.saturating_sub(last.quotes_submitted));
    reg.counter(ids.quotes_filled)
        .inc(cur.quotes_filled.saturating_sub(last.quotes_filled));
    reg.counter(ids.quotes_expired)
        .inc(cur.quotes_expired.saturating_sub(last.quotes_expired));
    reg.counter(ids.quotes_modified)
        .inc(cur.quotes_modified.saturating_sub(last.quotes_modified));
    reg.counter(ids.quotes_modify_refused)
        .inc(cur
            .quotes_modify_refused
            .saturating_sub(last.quotes_modify_refused));
    reg.counter(ids.quotes_cancelled)
        .inc(cur.quotes_cancelled.saturating_sub(last.quotes_cancelled));
    reg.counter(ids.quotes_cancel_refused)
        .inc(cur
            .quotes_cancel_refused
            .saturating_sub(last.quotes_cancel_refused));
    reg.counter(ids.quotes_raced)
        .inc(cur.quotes_raced.saturating_sub(last.quotes_raced));
    reg.counter(ids.skipped_partial)
        .inc(cur.skipped_partial.saturating_sub(last.skipped_partial));
    reg.counter(ids.closes_submitted)
        .inc(cur.closes_submitted.saturating_sub(last.closes_submitted));
    reg.counter(ids.skipped_tau)
        .inc(cur.skipped_tau.saturating_sub(last.skipped_tau));
    reg.counter(ids.skipped_tail)
        .inc(cur.skipped_tail.saturating_sub(last.skipped_tail));
    reg.counter(ids.skipped_stale)
        .inc(cur.skipped_stale.saturating_sub(last.skipped_stale));
    reg.counter(ids.skipped_mark_stale)
        .inc(cur.skipped_mark_stale.saturating_sub(last.skipped_mark_stale));
    reg.counter(ids.skipped_book)
        .inc(cur.skipped_book.saturating_sub(last.skipped_book));
    reg.counter(ids.skipped_inventory)
        .inc(cur.skipped_inventory.saturating_sub(last.skipped_inventory));
    reg.counter(ids.skipped_cap)
        .inc(cur.skipped_cap.saturating_sub(last.skipped_cap));
    reg.counter(ids.skipped_grid)
        .inc(cur.skipped_grid.saturating_sub(last.skipped_grid));
    reg.counter(ids.skipped_entry_price)
        .inc(cur.skipped_entry_price.saturating_sub(last.skipped_entry_price));
    reg.counter(ids.skipped_entry_persist)
        .inc(cur.skipped_entry_persist.saturating_sub(last.skipped_entry_persist));
    reg.counter(ids.skipped_entry_elapsed)
        .inc(cur.skipped_entry_elapsed.saturating_sub(last.skipped_entry_elapsed));
    reg.counter(ids.families_dormant)
        .inc(cur.families_dormant.saturating_sub(last.families_dormant));
    reg.counter(ids.fills)
        .inc(cur.fills.saturating_sub(last.fills));
    reg.counter(ids.unknown_fills)
        .inc(cur.unknown_fills.saturating_sub(last.unknown_fills));
    reg.counter(ids.settlement_fills)
        .inc(cur.settlement_fills.saturating_sub(last.settlement_fills));
    *last = cur;
    // The levels. A family the member does not configure keeps its row
    // at zero rather than disappearing: a missing series reads as a
    // scrape problem, a zero reads as a dormant slot.
    let mut view = [strategy_core::Bin15FamilyView::default(); BIN15_METRIC_FAMILIES];
    let n = strat.bin15_families_view(&mut view) as usize;
    let mut f = 0usize;
    while f < BIN15_METRIC_FAMILIES {
        let v = if f < n {
            view[f]
        } else {
            strategy_core::Bin15FamilyView::default()
        };
        reg.gauge(ids.families[f][0]).set(v.p_hat_1e6);
        reg.gauge(ids.families[f][1]).set(v.pos_yes_1e6);
        reg.gauge(ids.families[f][2]).set(v.pos_no_1e6);
        reg.gauge(ids.families[f][3]).set(i64::from(v.live_outcome));
        // BIN15 O6: the inputs. `d_1e6` and `den_1e9` are the pair that
        // separates "the move was real" from "the forecast was
        // overconfident"; `mark`/`strike`/`tau_s` let the reader
        // recompute `d` by hand and check the member's arithmetic —
        // outside the settlement window (BIN15 S3: inside it `d` also
        // carries the running average, which is not exported).
        reg.gauge(ids.families[f][4]).set(v.p_raw_1e6);
        reg.gauge(ids.families[f][5]).set(v.strike_1e6);
        reg.gauge(ids.families[f][6]).set(v.mark_1e6);
        reg.gauge(ids.families[f][7]).set(v.d_1e6);
        reg.gauge(ids.families[f][8]).set(v.den_1e9);
        reg.gauge(ids.families[f][9]).set(i64::from(v.tau_s));
        f += 1;
    }
}

/// XSD-3: the `engine_xsd_*` family (slot 2). Counters mirror the
/// member's cumulative [`strategy_core::XsdCounters`] as deltas; the two
/// levels (`pairs_warm`, `positions`) are gauges.
#[derive(Copy, Clone, Debug)]
pub struct XsdMetricIds {
    /// `engine_xsd_rolls_total`
    pub rolls: core_metrics::CounterId,
    /// `engine_xsd_decisions_total`
    pub decisions: core_metrics::CounterId,
    /// `engine_xsd_entries_decided_total`
    pub entries_decided: core_metrics::CounterId,
    /// `engine_xsd_adds_decided_total`
    pub adds_decided: core_metrics::CounterId,
    /// `engine_xsd_entries_total`
    pub entries: core_metrics::CounterId,
    /// `engine_xsd_adds_total`
    pub adds: core_metrics::CounterId,
    /// `engine_xsd_exits_revert_total`
    pub exits_revert: core_metrics::CounterId,
    /// `engine_xsd_exits_stop_total`
    pub exits_stop: core_metrics::CounterId,
    /// `engine_xsd_exits_maxhold_total`
    pub exits_maxhold: core_metrics::CounterId,
    /// `engine_xsd_exits_rotation_total`
    pub exits_rotation: core_metrics::CounterId,
    /// `engine_xsd_exits_regime_total`
    pub exits_regime: core_metrics::CounterId,
    /// `engine_xsd_intents_carried_total`
    pub intents_carried: core_metrics::CounterId,
    /// `engine_xsd_entries_cancelled_total`
    pub entries_cancelled: core_metrics::CounterId,
    /// `engine_xsd_caps_rejected_total`
    pub caps_rejected: core_metrics::CounterId,
    /// `engine_xsd_holds_absent_total`
    pub holds_absent: core_metrics::CounterId,
    /// `engine_xsd_regime_blocked_total`
    pub regime_blocked: core_metrics::CounterId,
    /// `engine_xsd_seed_rows_total`
    pub seed_rows: core_metrics::CounterId,
    /// `engine_xsd_seed_dropped_total`
    pub seed_dropped: core_metrics::CounterId,
    /// `engine_xsd_pairs_warm` (level)
    pub pairs_warm: core_metrics::GaugeId,
    /// `engine_xsd_positions` (level: entered targets)
    pub positions: core_metrics::GaugeId,
}

/// Register the XSD family. Boot-only.
fn register_xsd_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<XsdMetricIds, &'static str> {
    let mut one = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name).map_err(|_| "register xsd counter")
    };
    let rolls = one("engine_xsd_rolls_total")?;
    let decisions = one("engine_xsd_decisions_total")?;
    let entries_decided = one("engine_xsd_entries_decided_total")?;
    let adds_decided = one("engine_xsd_adds_decided_total")?;
    let entries = one("engine_xsd_entries_total")?;
    let adds = one("engine_xsd_adds_total")?;
    let exits_revert = one("engine_xsd_exits_revert_total")?;
    let exits_stop = one("engine_xsd_exits_stop_total")?;
    let exits_maxhold = one("engine_xsd_exits_maxhold_total")?;
    let exits_rotation = one("engine_xsd_exits_rotation_total")?;
    let exits_regime = one("engine_xsd_exits_regime_total")?;
    let intents_carried = one("engine_xsd_intents_carried_total")?;
    let entries_cancelled = one("engine_xsd_entries_cancelled_total")?;
    let caps_rejected = one("engine_xsd_caps_rejected_total")?;
    let holds_absent = one("engine_xsd_holds_absent_total")?;
    let regime_blocked = one("engine_xsd_regime_blocked_total")?;
    let seed_rows = one("engine_xsd_seed_rows_total")?;
    let seed_dropped = one("engine_xsd_seed_dropped_total")?;
    let pairs_warm = reg
        .register_gauge("engine_xsd_pairs_warm")
        .map_err(|_| "register engine_xsd_pairs_warm")?;
    let positions = reg
        .register_gauge("engine_xsd_positions")
        .map_err(|_| "register engine_xsd_positions")?;
    Ok(XsdMetricIds {
        rolls,
        decisions,
        entries_decided,
        adds_decided,
        entries,
        adds,
        exits_revert,
        exits_stop,
        exits_maxhold,
        exits_rotation,
        exits_regime,
        intents_carried,
        entries_cancelled,
        caps_rejected,
        holds_absent,
        regime_blocked,
        seed_rows,
        seed_dropped,
        pairs_warm,
        positions,
    })
}

/// Mirror the XSD family as monotonic deltas of the cumulative strategy
/// counters, plus the two levels. 5 s cadence — cold path.
fn mirror_xsd_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &XsdMetricIds,
    strat: &S,
    last: &mut strategy_core::XsdCounters,
) {
    let cur = strat.xsd_counters();
    reg.counter(ids.rolls).inc(cur.rolls.saturating_sub(last.rolls));
    reg.counter(ids.decisions)
        .inc(cur.decisions.saturating_sub(last.decisions));
    reg.counter(ids.entries_decided)
        .inc(cur.entries_decided.saturating_sub(last.entries_decided));
    reg.counter(ids.adds_decided)
        .inc(cur.adds_decided.saturating_sub(last.adds_decided));
    reg.counter(ids.entries).inc(cur.entries.saturating_sub(last.entries));
    reg.counter(ids.adds).inc(cur.adds.saturating_sub(last.adds));
    reg.counter(ids.exits_revert)
        .inc(cur.exits_revert.saturating_sub(last.exits_revert));
    reg.counter(ids.exits_stop)
        .inc(cur.exits_stop.saturating_sub(last.exits_stop));
    reg.counter(ids.exits_maxhold)
        .inc(cur.exits_maxhold.saturating_sub(last.exits_maxhold));
    reg.counter(ids.exits_rotation)
        .inc(cur.exits_rotation.saturating_sub(last.exits_rotation));
    reg.counter(ids.exits_regime)
        .inc(cur.exits_regime.saturating_sub(last.exits_regime));
    reg.counter(ids.intents_carried)
        .inc(cur.intents_carried.saturating_sub(last.intents_carried));
    reg.counter(ids.entries_cancelled)
        .inc(cur.entries_cancelled.saturating_sub(last.entries_cancelled));
    reg.counter(ids.caps_rejected)
        .inc(cur.caps_rejected.saturating_sub(last.caps_rejected));
    reg.counter(ids.holds_absent)
        .inc(cur.holds_absent.saturating_sub(last.holds_absent));
    reg.counter(ids.regime_blocked)
        .inc(cur.regime_blocked.saturating_sub(last.regime_blocked));
    reg.counter(ids.seed_rows)
        .inc(cur.seed_rows.saturating_sub(last.seed_rows));
    reg.counter(ids.seed_dropped)
        .inc(cur.seed_dropped.saturating_sub(last.seed_dropped));
    reg.gauge(ids.pairs_warm).set(cur.pairs_warm as i64);
    // Entered targets = the view's count (a level, cheap: one pass over
    // ≤ 128 machines, no rendering).
    let mut none: [strategy_core::XsdPositionView; 0] = [];
    reg.gauge(ids.positions)
        .set(strat.xsd_positions_view(&mut none) as i64);
    *last = cur;
}

/// XSD-3: rewrite `xsd-state.tsv` when the member's persisted-state
/// epoch moved (the `write_vrp_state_if_changed` law: a quiet engine
/// writes nothing; a failed write is logged, never fatal).
fn write_xsd_state_if_changed<S: strategy_core::StrategyCounters>(
    sink: Option<&XsdStateSink>,
    strat: &S,
    last_epoch: &mut u64,
    buf: &mut String,
    views: &mut [strategy_core::XsdPositionView],
    last_warn_ns: &mut u64,
    now: u64,
) {
    let Some(sink) = sink else { return };
    let epoch = strategy_core::StrategyCounters::xsd_state_epoch(strat);
    if epoch == *last_epoch {
        return;
    }
    let n = strategy_core::StrategyCounters::xsd_positions_view(strat, views) as usize;
    let n = n.min(views.len());
    let descriptor_of = |sym: SymbolId| -> Option<&str> {
        let mut i = 0usize;
        while i < sink.descriptors.len() {
            if sink.descriptors[i].0 == sym {
                return Some(sink.descriptors[i].1.as_str());
            }
            i += 1;
        }
        None
    };
    crate::xsd_boot::render_state(&sink.table_hash, &descriptor_of, &views[..n], buf);
    match crate::xsd_boot::write_state(&sink.path, buf) {
        Ok(()) => *last_epoch = epoch,
        Err(reason) => warn_state_write("xsd", &reason, last_warn_ns, now),
    }
}

/// HAR H3.4: rewrite each long-tenor series' `state-<NAME>.tsv` whose
/// epoch moved — one file per series, so a day close writes one engine's
/// rows (~350 KiB for a fitted series), never all twelve. `force` (the
/// shutdown drain) writes every series: its open day moves the state
/// without moving the epoch. A failed write is logged, never fatal.
fn write_har_state<S: strategy_core::StrategyCounters>(
    paths: &[std::path::PathBuf],
    strat: &S,
    written: &mut [u64; core_vol::LONG_SET_MAX],
    buf: &mut String,
    last_warn_ns: &mut u64,
    now: u64,
    force: bool,
) {
    let n = strategy_core::StrategyCounters::har_series(strat).min(paths.len());
    let mut i = 0usize;
    while i < n && i < core_vol::LONG_SET_MAX {
        let epoch = strategy_core::StrategyCounters::har_series_epoch(strat, i);
        if (force || epoch != written[i])
            && strategy_core::StrategyCounters::render_har_series(strat, i, buf)
        {
            match crate::state_file::write_atomic(&paths[i], buf) {
                Ok(()) => written[i] = epoch,
                Err(reason) => warn_state_write("har", &reason, last_warn_ns, now),
            }
        }
        i += 1;
    }
}

// ---------------------------------------------------------------
// RG2: the `engine_regime_*` family (plan §4.9)
// ---------------------------------------------------------------

/// Registry handles of the regime family: per-profile word gauges +
/// raw inputs, per-dimension flip counters, disagree counters, the
/// per-slot gate gauges and the boot/plane counters.
#[derive(Copy, Clone, Debug)]
pub struct RegimeMetricIds {
    /// `engine_regime_configured` (0/1).
    pub configured: GaugeId,
    /// `engine_regime_{fast,slow}_{measured,declared,effective}` — the
    /// word as an i64 gauge (index = profile).
    pub words: [[GaugeId; 3]; 2],
    /// `engine_regime_{fast,slow}_source` (0 measured / 1 declared / 2 unknown).
    pub source: [GaugeId; 2],
    /// `engine_regime_{fast,slow}_declared_age_ns` (−1 = none).
    pub declared_age: [GaugeId; 2],
    /// `engine_regime_{fast,slow}_raw_{ret_bps,er,rv_bps,stretch}` ×1e9.
    pub raw: [[GaugeId; 4]; 2],
    /// `engine_regime_{fast,slow}_flips_{trend,shape,vol,fund,level,stretch}_total`.
    pub flips: [[core_metrics::CounterId; 6]; 2],
    /// `engine_regime_{fast,slow}_disagree_total`.
    pub disagree: [core_metrics::CounterId; 2],
    /// `engine_strategy_regime_gate_{0..7}` (0 open / 1 soft-closed / 2 hard-closed).
    pub gates: [GaugeId; 8],
    /// `engine_regime_minutes_judged_total`.
    pub minutes_judged: core_metrics::CounterId,
    /// `engine_regime_seed_rows` (gauge).
    pub seed_rows: GaugeId,
    /// `engine_regime_declared_total`.
    pub declared_total: core_metrics::CounterId,
    /// `engine_regime_gate_changes_total`.
    pub gate_changes: core_metrics::CounterId,
}

const REGIME_PROFILE_NAMES: [&str; 2] = ["fast", "slow"];
const REGIME_WORD_NAMES: [&str; 3] = ["measured", "declared", "effective"];
const REGIME_RAW_NAMES: [&str; 4] = ["ret_bps", "er", "rv_bps", "stretch"];
const REGIME_DIM_NAMES: [&str; 6] = ["trend", "shape", "vol", "fund", "level", "stretch"];

/// Register the regime family (≈ 50 names). Boot-only.
fn register_regime_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<RegimeMetricIds, &'static str> {
    let mut gauge = |name: &str| -> Result<GaugeId, &'static str> {
        reg.register_gauge(name)
            .map_err(|_| "register regime gauge")
    };
    let configured = gauge("engine_regime_configured")?;
    let mut words = [[GaugeId::default(); 3]; 2];
    let mut source = [GaugeId::default(); 2];
    let mut declared_age = [GaugeId::default(); 2];
    let mut raw = [[GaugeId::default(); 4]; 2];
    let mut gates = [GaugeId::default(); 8];
    for (p, pname) in REGIME_PROFILE_NAMES.iter().enumerate() {
        for (w, wname) in REGIME_WORD_NAMES.iter().enumerate() {
            words[p][w] = gauge(&format!("engine_regime_{pname}_{wname}"))?;
        }
        source[p] = gauge(&format!("engine_regime_{pname}_source"))?;
        declared_age[p] = gauge(&format!("engine_regime_{pname}_declared_age_ns"))?;
        for (r, rname) in REGIME_RAW_NAMES.iter().enumerate() {
            raw[p][r] = gauge(&format!("engine_regime_{pname}_raw_{rname}"))?;
        }
    }
    for (slot, g) in gates.iter_mut().enumerate() {
        *g = gauge(&format!("engine_strategy_regime_gate_{slot}"))?;
    }
    let seed_rows = gauge("engine_regime_seed_rows")?;
    let mut counter = |name: &str| -> Result<core_metrics::CounterId, &'static str> {
        reg.register_counter(name)
            .map_err(|_| "register regime counter")
    };
    let mut flips = [[core_metrics::CounterId::default(); 6]; 2];
    let mut disagree = [core_metrics::CounterId::default(); 2];
    for (p, pname) in REGIME_PROFILE_NAMES.iter().enumerate() {
        for (d, dname) in REGIME_DIM_NAMES.iter().enumerate() {
            flips[p][d] = counter(&format!("engine_regime_{pname}_flips_{dname}_total"))?;
        }
        disagree[p] = counter(&format!("engine_regime_{pname}_disagree_total"))?;
    }
    Ok(RegimeMetricIds {
        configured,
        words,
        source,
        declared_age,
        raw,
        flips,
        disagree,
        gates,
        minutes_judged: counter("engine_regime_minutes_judged_total")?,
        seed_rows,
        declared_total: counter("engine_regime_declared_total")?,
        gate_changes: counter("engine_regime_gate_changes_total")?,
    })
}

/// Mirror the regime family: gauges set, counters as monotonic deltas
/// of the cumulative values. 5 s cadence — cold path.
fn mirror_regime_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &RegimeMetricIds,
    strat: &S,
    last: &mut strategy_core::RegimeCounters,
    now: NsTs,
) {
    let cur = strat.regime_counters();
    reg.gauge(ids.configured).set(i64::from(cur.configured));
    for p in 0..2usize {
        reg.gauge(ids.words[p][0]).set(cur.measured[p].0 as i64);
        reg.gauge(ids.words[p][1]).set(cur.declared[p].0 as i64);
        reg.gauge(ids.words[p][2]).set(cur.effective[p].0 as i64);
        let src = cur.effective[p].source();
        reg.gauge(ids.source[p]).set(if src & 0b001 != 0 {
            0
        } else if src & 0b010 != 0 {
            1
        } else {
            2
        });
        let age = if cur.declared_ttl_ns[p] == 0 {
            -1
        } else {
            now.wrapping_sub(cur.declared_ts_ns[p]).min(i64::MAX as u64) as i64
        };
        reg.gauge(ids.declared_age[p]).set(age);
        for r in 0..4usize {
            let present = cur.raw_present[p] & (1u8 << r) != 0;
            reg.gauge(ids.raw[p][r])
                .set(if present { cur.raw[p][r] } else { 0 });
        }
        for d in 0..6usize {
            reg.counter(ids.flips[p][d])
                .inc(cur.flips[p][d].saturating_sub(last.flips[p][d]));
        }
        reg.counter(ids.disagree[p])
            .inc(cur.disagree[p].saturating_sub(last.disagree[p]));
    }
    for slot in 0..8usize {
        reg.gauge(ids.gates[slot]).set(i64::from(cur.gates[slot]));
    }
    reg.counter(ids.minutes_judged)
        .inc(cur.minutes_judged.saturating_sub(last.minutes_judged));
    reg.gauge(ids.seed_rows)
        .set(cur.seed_rows.min(i64::MAX as u64) as i64);
    reg.counter(ids.declared_total)
        .inc(cur.declared_total.saturating_sub(last.declared_total));
    reg.counter(ids.gate_changes)
        .inc(cur.gate_changes.saturating_sub(last.gate_changes));
    *last = cur;
}

// ---------------------------------------------------------------
// HAR H3.5: the `engine_har_*` gauges
// ---------------------------------------------------------------

/// Registry handles of the long-tenor HAR gauges. Always registered (the
/// regime family's precedent): a boot without `har.toml` reports
/// `engine_har_series_configured 0` and nothing else moves.
#[derive(Copy, Clone, Debug)]
pub struct HarMetricIds {
    /// `engine_har_series_configured` — series the set runs.
    pub configured: GaugeId,
    /// `engine_har_series_warm` — series whose fold forecasts.
    pub warm: GaugeId,
    /// `engine_har_day_age_max_s` — the stalest series: whole seconds since
    /// its newest closed day ENDED (−1 = no series has closed a day). Past
    /// 26 h (93 600 s) a series is not recalibrating.
    pub day_age_max_s: GaugeId,
    /// `engine_har_day_close_ns_max` — the costliest UTC day close since
    /// boot (the engine's close law alone; the stagger pays one a poll).
    pub day_close_ns_max: GaugeId,
}

/// Register the HAR gauges. Boot-only.
fn register_har_metrics(
    reg: &mut core_metrics::MetricsRegistry,
) -> Result<HarMetricIds, &'static str> {
    let mut gauge = |name: &str| -> Result<GaugeId, &'static str> {
        reg.register_gauge(name).map_err(|_| "register har gauge")
    };
    Ok(HarMetricIds {
        configured: gauge("engine_har_series_configured")?,
        warm: gauge("engine_har_series_warm")?,
        day_age_max_s: gauge("engine_har_day_age_max_s")?,
        day_close_ns_max: gauge("engine_har_day_close_ns_max")?,
    })
}

/// Mirror the HAR gauges from the set's rows. 5 s cadence — cold path;
/// the rows land in the caller's boot-allocated scratch.
fn mirror_har_metrics<S: strategy_core::StrategyCounters>(
    reg: &core_metrics::MetricsRegistry,
    ids: &HarMetricIds,
    strat: &S,
    rows: &mut [strategy_core::HarSeriesView],
    wall_ms: u64,
) {
    let n = strat.har_series_view(rows);
    // H3.7: one law, `strategy_core::har_gauges` (alloc gate 82 runs it).
    let g = strategy_core::har_gauges(rows, n, &strat.har_counters(), wall_ms);
    reg.gauge(ids.configured).set(g.configured);
    reg.gauge(ids.warm).set(g.warm);
    reg.gauge(ids.day_age_max_s).set(g.day_age_max_s);
    reg.gauge(ids.day_close_ns_max).set(g.day_close_ns_max);
}

// ---------------------------------------------------------------
// RG6 `/state` snapshot — filled once per second by the engine loop
// ---------------------------------------------------------------

/// Build the boot identity of the `/state` snapshot: pid, the wall
/// anchor of this instant, the running binary's link time (pitfall
/// 18's staleness tell), the git commit `build.rs` recorded
/// (`MULTIVENUE_GIT_SHA`; "unknown" without git at build time), the
/// capture run and the `--strategy` name. The masks and the regime
/// hash are stamped later by the set builder / the bin. Boot-only.
pub fn boot_info(run_dir: &Path, run_epoch_ns: u64, strategy: &str, paper: bool) -> BootInfo {
    let anchor = core_time::WallAnchor::now();
    let mut b = BootInfo::EMPTY;
    b.boot_mono_ns = anchor.mono_ns;
    b.boot_wall_ns = anchor.wall_ns;
    b.binary_mtime_ns = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    b.run_epoch_ns = run_epoch_ns;
    b.pid = std::process::id();
    b.paper = u8::from(paper);
    b.set_git_sha(option_env!("MULTIVENUE_GIT_SHA").unwrap_or("unknown").as_bytes());
    b.set_strategy_name(strategy.as_bytes());
    b.set_run_dir(run_dir.as_os_str().as_encoded_bytes());
    b
}

/// The `GET /state` body writer the metrics server calls (one per
/// server thread): copy the latest snapshot out of the seqlock into a
/// boot-boxed scratch and encode it into the response buffer. Zero
/// allocation after construction.
pub fn state_writer(
    cell: Arc<SnapshotCell<EngineSnapshot>>,
) -> impl FnMut(&mut [u8]) -> Result<usize, core_metrics::EncodeErr> {
    let mut scratch = Box::new(EngineSnapshot::empty());
    move |dst: &mut [u8]| {
        cell.read_into(&mut scratch);
        engine_snapshot::encode_state_json(&scratch, dst)
            .map_err(|_| core_metrics::EncodeErr::BufferTooSmall)
    }
}

/// The ingress status slots in the T1(c) / `VENUE_NAMES` order:
/// pm, bn, okx, deribit, hl, bybit, rpc, mexc (MX2, appended),
/// hyperevm (HYPARB H3b, appended), hypercall (HC5, appended).
#[inline]
fn ingress_lanes(ing: &IngressStatusSet) -> [&IngressStatus; SNAPSHOT_VENUES] {
    [
        &ing.polymarket,
        &ing.binance,
        &ing.okx,
        &ing.deribit,
        &ing.hyperliquid,
        &ing.bybit,
        &ing.rpc,
        &ing.mexc,
        &ing.hyperevm,
        &ing.hypercall,
    ]
}

/// T1(c) last-tick tracking: `(ticks_total last seen, engine ns when
/// it last advanced; 0 = never)` per lane. 1 s cadence — cold path.
fn update_tick_age(
    track: &mut [(u64, u64); SNAPSHOT_VENUES],
    ing: &IngressStatusSet,
    now: NsTs,
) {
    let lanes = ingress_lanes(ing);
    let mut i = 0usize;
    while i < SNAPSHOT_VENUES {
        let ticks = lanes[i].ticks_total();
        if ticks > track[i].0 {
            track[i] = (ticks, now);
        }
        i += 1;
    }
}

/// Fill `out` from every source the loop can reach — the strategy
/// (through the `StrategyCounters` UFCS route, set-agnostic), the
/// engine's counters/percentiles/captures/recent rings, the AI status
/// slot and the ingress slots. 1 s cadence, off the tick path, zero
/// allocation (every field is a plain store or a POD copy; the alloc
/// gate in `crates/bench` pins the publish + encode).
fn fill_snapshot<S, D>(
    out: &mut EngineSnapshot,
    eng: &Engine<S, D>,
    obs: &Observability,
    tick_age: &[(u64, u64); SNAPSHOT_VENUES],
    now: NsTs,
    seq: u64,
) where
    S: strategy_core::Strategy,
    D: OrderDispatch,
{
    use strategy_core::StrategyCounters as Sc;
    let strat = eng.strategy();

    out.seq = seq;
    out.mono_ns = now;
    out.wall_ns = obs
        .boot
        .boot_wall_ns
        .wrapping_add(now.wrapping_sub(obs.boot.boot_mono_ns));
    // `out.boot` was stored once when the scratch was boxed: the boot
    // identity never changes, so it is not re-copied every second.
    out.set_strategy_kind(Sc::strategy_kind(strat).as_bytes());
    out.halted = u8::from(Sc::is_halted(strat));
    out.enabled_mask = Sc::enabled_mask(strat) as u8;

    let k = &mut out.counters;
    k.iterations = eng.iterations;
    k.ticks = eng.ticks_dispatched;
    k.signals = eng.signals_dispatched;
    k.fills = eng.fills_dispatched;
    k.events = eng.events_dispatched;
    k.depths = eng.depths_dispatched;
    k.opts = eng.opts_dispatched;
    k.orders_emitted = Sc::orders_emitted(strat);
    k.orders_dropped = Sc::orders_dropped(strat);
    k.ai_dispatched = eng.ai_dispatched;
    k.ai_drain_malformed = eng.ai_drain_malformed;

    out.latency.p50_ns = [eng.ingest_p50_ns(), eng.decide_p50_ns(), eng.ack_p50_ns()];
    out.latency.p99_ns = [eng.ingest_p99_ns(), eng.decide_p99_ns(), eng.ack_p99_ns()];

    out.regime = Sc::regime_counters(strat);
    out.regime_rel = Sc::regime_rel_view(strat);
    let mut slot = 0u8;
    while (slot as usize) < out.slots.len() {
        out.slots[slot as usize] = Sc::slot_counters(strat, slot);
        slot += 1;
    }

    let v = &mut out.vm;
    v.active_hash = Sc::vm_active_hash128(strat);
    v.staged_hash = Sc::vm_staged_hash128(strat);
    v.rows_active = Sc::vm_rows_view(strat, &mut v.rows);
    v.epoch = Sc::vm_table_epoch(strat) as u32;
    v.fires = Sc::vm_fires(strat);
    v.orders_emitted = Sc::vm_orders_emitted(strat);
    v.orders_dropped = Sc::vm_orders_dropped(strat);
    v.commit_dropped = Sc::vm_commit_dropped(strat);
    v.regime_blocked = Sc::vm_regime_blocked(strat);
    v.regime_hard_exits = Sc::vm_regime_hard_exits(strat);

    // XMM XH3: slot 6 — counters and perp rows from ONE publish instant
    // (a quote and the touch it was placed against can never disagree).
    Sc::xmm_counters(strat, &mut out.xmm.counters);
    out.xmm.n_perps = Sc::xmm_perps_view(strat, &mut out.xmm.perps);

    // P6: slot 1. Both halves come from the same publish instant, so a
    // strike and the position held against it can never disagree.
    out.vrp.counters = Sc::vrp_counters(strat);
    out.vrp.view = Sc::vrp_snapshot_view(strat);

    // HYPARB H6: slot 0 — counters, pool rows and coin rows from ONE
    // publish instant (the rows are copied into the snapshot's own
    // arrays; a configured count beyond them is still reported).
    out.hyparb.counters = Sc::hyparb_counters(strat);
    out.hyparb.n_pools = Sc::hyparb_pools_view(strat, &mut out.hyparb.pools);
    out.hyparb.n_coins = Sc::hyparb_coins_view(strat, &mut out.hyparb.coins);

    // HAR H3.5: the long-tenor series — the set's cached rows (rebuilt
    // at each series' day close) with the minute fields read live.
    out.har.hash = obs.har_hash;
    out.har.dropped = obs.har_dropped;
    out.har.counters = Sc::har_counters(strat);
    out.har.n = Sc::har_series_view(strat, &mut out.har.series);

    // E6 c4: the router's kill switches. Read through the trait, from
    // the same publish instant as everything else, so a halted slot
    // and the refusals it produced can never disagree.
    let ec = clob_dispatcher::OrderDispatch::exec_counters(eng.dispatcher());
    let ex = &mut out.exec;
    ex.configured = ec.configured;
    ex.seeded = ec.seeded;
    ex.adopted = ec.halt_file_adopted;
    ex.file_present = ec.halt_file_present;
    ex.halts = ec.halts;
    ex.refused_halted = ec.refused_halted;
    ex.refused_unseeded = ec.refused_unseeded;
    ex.cancel_all_failures = ec.cancel_all_failures;
    ex.cancel_all_stranded = ec.cancel_all_stranded;
    ex.halted = ec.halted;
    // E7: the ledger's alarms and the live arm's numbers reach the
    // surface an operator actually reads.
    ex.ledger_fills_unbound = ec.ledger_fills_unbound;
    ex.ledger_sells_below_zero = ec.ledger_sells_below_zero;
    ex.ledger_resting_full = ec.ledger_resting_full;
    ex.ledger_resting_ambiguous = ec.ledger_resting_ambiguous;
    ex.arm_fills_booked = ec.arm.fills_booked;
    ex.arm_fills_dropped = ec.arm.fills_dropped;
    ex.arm_fills_unresolved = ec.arm.fills_unresolved;
    ex.arm_sent_unanswered = ec.arm.sent_unanswered;
    ex.arm_recon_ok = ec.arm.recon_ok;
    ex.arm_recon_failed = ec.arm.recon_failed;
    ex.arm_recon_drift_legs = ec.arm.recon_drift_legs;
    ex.arm_recon_unseen_legs = ec.arm.recon_unseen_legs;
    ex.arm_sweep_left = ec.arm.sweep_left;
    ex.arm_budget_remaining = ec.arm.budget_remaining;
    ex.arm_pnl_anchor_usd_1e6 = ec.arm.pnl_anchor_usd_1e6;
    ex.arm_session_pnl_usd_1e6 = ec.arm.session_pnl_usd_1e6;

    let st = eng.ai_status();
    let a = &mut out.ai;
    a.cmds = st.cmds();
    a.hmac_fail = st.hmac_fail();
    a.protocol_err = st.protocol_err();
    a.malformed = st.malformed();
    a.seq_gap = st.seq_gap();
    a.seq_regress = st.seq_regress();
    a.ring_drops = st.ring_drops();
    a.expired = st.expired();
    a.rejected_conns = st.rejected_conns();
    a.drain_malformed = eng.ai_drain_malformed;
    a.enable_refused = Sc::ai_enable_refused(strat);
    a.ruleset_staged = st.ruleset_staged();
    a.ruleset_committed = st.ruleset_committed();
    a.ruleset_rejected = st.ruleset_rejected();
    a.table_push_fail = st.table_push_fail();
    a.last_heartbeat_ns = st.last_heartbeat_ns();

    if let Some(ing) = obs.ingress.as_ref() {
        let lanes = ingress_lanes(ing);
        let mut i = 0usize;
        while i < SNAPSHOT_VENUES {
            let g = &mut out.ingress[i];
            let l = lanes[i];
            g.last_tick_ns = tick_age[i].1;
            g.ticks = l.ticks_total();
            g.msgs = l.msgs_total();
            g.reconnects = l.reconnects_total();
            g.ring_drops = l.ring_drops_total();
            g.stale_ticks = l.stale_ticks_total();
            g.parse_errors = l.parse_errors_total();
            g.gaps = l.gaps_total();
            g.sub_drops = l.sub_drops_total();
            g.feed_delay_ema_ms = l.feed_delay_ema_ms();
            g.state = l.state() as u8;
            i += 1;
        }
    }

    out.capture.fills_records = eng.fill_capture_records();
    out.capture.fills_io_errors = eng.fill_capture_io_errors();
    out.capture.orders_records = eng.order_capture_records();
    out.capture.orders_io_errors = eng.order_capture_io_errors();

    out.recent_orders = *eng.recent_orders();
    out.recent_fills = *eng.recent_fills();
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

/// HC5: the Hypercall venue family, in [`HC_METRIC_NAMES`] order —
/// every value is a GAUGE mirrored from the venue's own atomics
/// (`ingress_hypercall::HcCounters`; the `_total` ones are monotonic by
/// construction, the BIN15 roll-gauge precedent), so the mirror is a
/// plain store per value, no delta bookkeeping.
#[derive(Copy, Clone, Debug)]
pub struct HcMetricIds {
    /// One gauge per [`HC_METRIC_NAMES`] entry.
    pub gauges: [GaugeId; HC_METRICS],
}

/// Size of the Hypercall family.
pub const HC_METRICS: usize = 31;

/// The Hypercall family's names — the ORDER is [`hc_metric_values`]'
/// (pinned by a test). Closes by cause follow `HcCloseCause::ALL`.
pub const HC_METRIC_NAMES: [&str; HC_METRICS] = [
    "engine_ingress_hypercall_closes_message_limit_total",
    "engine_ingress_hypercall_closes_byte_limit_total",
    "engine_ingress_hypercall_closes_queue_age_total",
    "engine_ingress_hypercall_closes_write_timeout_total",
    "engine_ingress_hypercall_closes_slow_other_total",
    "engine_ingress_hypercall_closes_other_total",
    "engine_ingress_hypercall_subscribes_total",
    "engine_ingress_hypercall_one_sided_quotes_total",
    "engine_ingress_hypercall_empty_quotes_total",
    "engine_ingress_hypercall_crossed_quotes_total",
    "engine_ingress_hypercall_provider_quotes_total",
    "engine_ingress_hypercall_clock_syncs_total",
    "engine_ingress_hypercall_listings_created_total",
    "engine_ingress_hypercall_listings_expired_total",
    "engine_ingress_hypercall_listings_deleted_total",
    "engine_ingress_hypercall_listings_other_total",
    "engine_ingress_hypercall_foreign_trades_total",
    "engine_ingress_hypercall_venue_errors_total",
    "engine_ingress_hypercall_quote_publish_lag_ms",
    "engine_ingress_hypercall_quoted_instruments",
    "engine_ingress_hypercall_providers_max",
    "engine_ingress_hypercall_index_age_ms",
    "engine_ingress_hypercall_clock_rtt_ms",
    "engine_ingress_hypercall_snapshot_requests_total",
    "engine_ingress_hypercall_rest_polls_ok_total",
    "engine_ingress_hypercall_rest_polls_err_total",
    "engine_ingress_hypercall_rest_opt_rows_total",
    "engine_ingress_hypercall_rest_foreign_rows_total",
    "engine_ingress_hypercall_rest_snapshots_total",
    "engine_ingress_hypercall_rest_handoff_drops_total",
    "engine_ingress_hypercall_rest_last_round_ms",
];

/// Register the Hypercall family (boot-only).
fn register_hypercall_metrics(reg: &mut MetricsRegistry) -> Result<HcMetricIds, &'static str> {
    let mut gauges = [GaugeId::default(); HC_METRICS];
    let mut i = 0usize;
    while i < HC_METRICS {
        gauges[i] = reg
            .register_gauge(HC_METRIC_NAMES[i])
            .map_err(|_| "register hypercall gauge")?;
        i += 1;
    }
    Ok(HcMetricIds { gauges })
}

/// The Hypercall family's current values, in [`HC_METRIC_NAMES`] order.
/// Relaxed loads — statistics, never a synchronization point.
#[must_use]
pub fn hc_metric_values(c: &ingress_hypercall::HcCounters) -> [u64; HC_METRICS] {
    use ingress_hypercall::counters::get;
    let (w, r) = (&c.ws, &c.rest);
    [
        get(&w.closes[0]),
        get(&w.closes[1]),
        get(&w.closes[2]),
        get(&w.closes[3]),
        get(&w.closes[4]),
        get(&w.closes[5]),
        get(&w.subscribes),
        get(&w.one_sided_quotes),
        get(&w.empty_quotes),
        get(&w.crossed_quotes),
        get(&w.provider_quotes),
        get(&w.clock_syncs),
        get(&w.listings[0]),
        get(&w.listings[1]),
        get(&w.listings[2]),
        get(&w.listings[3]),
        get(&w.foreign_trades),
        get(&w.venue_errors),
        get(&w.quote_publish_lag_ms),
        get(&w.quoted_instruments),
        get(&w.providers_max),
        get(&w.index_age_ms),
        get(&w.clock_rtt_ms),
        get(&c.snapshot_req),
        get(&r.polls_ok),
        get(&r.polls_err),
        get(&r.opt_rows),
        get(&r.foreign_rows),
        get(&r.snapshots),
        get(&r.handoff_drops),
        get(&r.last_round_ms),
    ]
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
    trade_ring_drops: u64,
    stale_ticks: u64,
    seq_regressions: u64,
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
        trade_ring_drops: st.trade_ring_drops_total(),
        stale_ticks: st.stale_ticks_total(),
        seq_regressions: st.seq_regressions_total(),
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
    reg.counter(ids.trade_ring_drops)
        .inc(cur.trade_ring_drops.saturating_sub(last.trade_ring_drops));
    reg.counter(ids.stale_ticks)
        .inc(cur.stale_ticks.saturating_sub(last.stale_ticks));
    reg.counter(ids.seq_regressions)
        .inc(cur.seq_regressions.saturating_sub(last.seq_regressions));
    reg.gauge(ids.feed_delay_ema_ms)
        .set(st.feed_delay_ema_ms() as i64);
    *last = cur;
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
        hyperevm_signal,
        trades,
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
    // HYPARB H3b: the pool-event lane (empty forever when the HyperEVM
    // ingress is not spawned).
    eng.set_pool_lane(hyperevm_signal);
    // XMM XH1: the trade lane (empty forever when the Hyperliquid
    // ingress is not spawned).
    eng.set_trade_lane(trades);
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
    // RG6: the 1 s `/state` publish gate + the boot-boxed scratch the
    // loop fills before each seqlock copy (one allocation, here).
    let mut next_state = now_ns() + SNAPSHOT_PERIOD_NS;
    let mut state_scratch: Option<Box<EngineSnapshot>> = obs.state.as_ref().map(|_| {
        let mut s = Box::new(EngineSnapshot::empty());
        // The boot identity (416 B since XMM XH3) is fixed for the
        // process: stored here once, never re-copied per publish.
        s.boot = obs.boot;
        s
    });
    let mut state_seq = 0u64;
    let mut last_ticks = 0u64;
    let mut last_signals = 0u64;
    let mut last_orders = 0u64;
    // Last-mirrored snapshots for the §6.4 ingress counters
    // (pm, bn, okx, rpc, deribit, hyperliquid, bybit, mexc, hyperevm,
    // hypercall) so registry counters get monotonic deltas. Append-only:
    // existing indices are load-bearing, new venues go at the end.
    let mut ingress_last = [IngressCountersSnapshot::default(); 10];
    // T1(c): last-tick-age derivation state per venue —
    // (ticks_total last seen, wall ns when it last advanced);
    // wall ns 0 = never ticked. Order pairs with
    // `ids.ingress_last_tick_age`: pm, bn, okx, deribit, hl, bybit,
    // rpc, mexc (NOT the ingress_last order — that array predates this
    // and its indices are load-bearing).
    let mut tick_age_track = [(0u64, 0u64); SNAPSHOT_VENUES];
    // Phase-8f AI-family delta snapshot (same bookkeeping).
    let mut ai_last = AiCountersSnapshot::default();
    // Phase-8g §9 vm-family delta snapshot (same bookkeeping).
    let mut vm_last = VmCountersSnapshot::default();
    // XMM XH3 slot-6 family delta snapshot (same bookkeeping).
    let mut xmm_last = strategy_core::XmmCounters::default();
    let mut vrp_last = strategy_core::VrpCounters::default();
    // VRP V8a: the persisted-state writer. The epoch starts at whatever
    // the member came up with, so a boot that changed nothing rewrites
    // nothing — the first write is a real state change, not a restart.
    let vrp_state_path = obs.vrp_state_path.clone();
    let mut vrp_state_epoch =
        strategy_core::StrategyCounters::vrp_state_epoch(eng.strategy());
    let mut vrp_state_buf = String::new();
    // XSD-3: the same epoch-gated writer for slot 2 (positions rendered
    // with descriptors through the boot sink; the view buffer is
    // allocated once here and reused).
    let mut xsd_last = strategy_core::XsdCounters::default();
    // BIN15 O4b: slot 3's counter baseline. No state writer — a binary
    // instance dies at its own expiry, so there is nothing an epoch
    // could carry across a restart that the next roll does not rebind.
    let mut bin15_last = strategy_core::Bin15Counters::default();
    let mut hyparb_last = strategy_core::HyparbCounters::default();
    // HYPARB H8: the testnet shadow's tap, owned by this thread from here.
    let mut hyparb_shadow = obs.hyparb_shadow.take();
    let hyparb_shadow_dark = obs.hyparb_shadow_dark;
    let hyparb_live_status = obs.hyparb_live_status.take();
    let mut hyparb_evm_last = [0u64; crate::evm_testnet::SHADOW_COUNTER_NAMES.len()];
    let xsd_sink = obs.xsd_state.clone();
    let mut xsd_state_epoch = strategy_core::StrategyCounters::xsd_state_epoch(eng.strategy());
    let mut xsd_state_buf = String::new();
    let mut xsd_views =
        vec![strategy_core::XsdPositionView::default(); strategy_xsd::XSD_MAX_TARGETS];
    let mut vrp_state_warn_ns: u64 = 0;
    let mut xsd_state_warn_ns: u64 = 0;
    // HAR H3.4: the long-tenor writer's per-series epochs start at the
    // restore's (a boot that changed nothing rewrites nothing); the buffer
    // is reused for every series.
    let har_state_paths = std::mem::take(&mut obs.har_state_paths);
    // H3.7: with the writer thread running, the loop only hands the states
    // (the set's outbox, in `on_timer`) and writes none of them until the
    // shutdown's forced write.
    let har_writer = obs.har_writer.take();
    let har_on_loop = har_writer.is_none();
    let mut har_state_epochs = [0u64; core_vol::LONG_SET_MAX];
    {
        let n = strategy_core::StrategyCounters::har_series(eng.strategy());
        let mut i = 0usize;
        while i < n && i < core_vol::LONG_SET_MAX {
            har_state_epochs[i] = strategy_core::StrategyCounters::har_series_epoch(eng.strategy(), i);
            i += 1;
        }
    }
    let mut har_state_buf = String::new();
    let mut har_state_warn_ns: u64 = 0;
    // X1: the paper matcher's delta snapshot.
    let mut matcher_last = clob_dispatcher::MatcherCounters::default();
    let mut lifecycle_last = engine::LifecycleCounters::default();
    let mut fills_unrouted_last: [u64; 2] = [0; 2];
    // E1: the router's previous snapshot, for the monotonic deltas.
    let mut exec_last = clob_dispatcher::ExecCounters::default();
    // F18/F21: ONE call site for every member's persisted state, so a
    // third member cannot be added to one of the two places and not the
    // other. A macro rather than a closure because it borrows `eng`
    // alongside code that also borrows `eng` mutably; it expands to the
    // same two epoch-gated calls either way.
    macro_rules! flush_member_state {
        () => {{
            write_vrp_state_if_changed(
                vrp_state_path.as_deref(),
                eng.strategy(),
                &mut vrp_state_epoch,
                &mut vrp_state_buf,
                &mut vrp_state_warn_ns,
                now_ns(),
            );
            write_xsd_state_if_changed(
                xsd_sink.as_ref(),
                eng.strategy(),
                &mut xsd_state_epoch,
                &mut xsd_state_buf,
                &mut xsd_views,
                &mut xsd_state_warn_ns,
                now_ns(),
            );
            if har_on_loop {
                write_har_state(
                    &har_state_paths,
                    eng.strategy(),
                    &mut har_state_epochs,
                    &mut har_state_buf,
                    &mut har_state_warn_ns,
                    now_ns(),
                    false,
                );
            }
        }};
    }
    let mut regime_last = strategy_core::RegimeCounters::default();
    // HAR H3.5: the gauges' row scratch (boot-allocated, reused).
    let mut har_rows = [strategy_core::HarSeriesView::default(); strategy_core::HAR_VIEW_SERIES];
    // Periodic HdrHistogram dump cadence. `next_dump_ns` is only
    // consulted when `obs.latency_dump.is_some()`.
    let mut next_dump_ns: u64 = match obs.latency_dump.as_ref() {
        Some(d) => now_ns().saturating_add(d.interval_ns),
        None => u64::MAX,
    };

    // E6: paces the dispatcher's idle moment. See `IdlePacer`.
    let mut idle_pacer = IdlePacer::new();

    while !shutdown_requested() {
        let drained = eng.tick(DRAIN_BATCH);

        let mut now = now_ns();

        // E6 commit 3a — THE LIVE ARM'S ONLY THREAD.
        //
        // `RoutedDispatcher` is handed straight to this loop on the
        // `--exec` path, with no `DispatcherWorker` behind it, so
        // until now nothing called `on_idle` on the one path that can
        // arm Hyperliquid: the user-event socket never pumped, the
        // reconciler never ran, the budget never persisted, and E6's
        // halt triggers had no source of truth.
        //
        // Driven when the tick drained NOTHING — the venue work blocks
        // and market data must not queue behind it — or when the gap
        // has reached `DISPATCHER_IDLE_MAX_GAP_NS`, because "only when
        // idle" starves it in exactly the busy market that most needs
        // reconciling.
        //
        // A PAPER boot reaches this too, and its dispatcher's
        // `on_idle` is the trait's default: a `false` return and
        // nothing else. No syscall, no branch an operator can see.
        // The pacer owns the "is it due" test, the call, and the
        // stamping — all three, because the stamping is where the
        // first cut went wrong and a test that only covered the test
        // could not see it. `now` is replaced by the post-call
        // reading, so every cadence below measures from a clock taken
        // AFTER any stall rather than before it.
        now = idle_pacer.drive_if_due(drained, now, now_ns, || {
            eng.drive_dispatcher_idle();
        });
        if now >= next_state {
            // T1(c): per-venue last-tick stamps, refreshed on the 1 s
            // cadence (the 5 s block below reads them for the age
            // gauges; the snapshot reads them for `last_tick_age_s`).
            if let Some(ing) = obs.ingress.as_ref() {
                update_tick_age(&mut tick_age_track, ing, now);
            }
            // RG6: fill the scratch from every source reachable here
            // and publish it into the seqlock — one POD copy.
            if let (Some(cell), Some(scratch)) = (obs.state.as_ref(), state_scratch.as_mut()) {
                state_seq = state_seq.wrapping_add(1);
                fill_snapshot(scratch, &eng, &obs, &tick_age_track, now, state_seq);
                cell.publish(scratch);
            }
            next_state = now + SNAPSHOT_PERIOD_NS;
        }
        if now >= next_report {
            // Phase 8f: bound fills-capture staging staleness to one
            // report period even when no further fills arrive.
            // Independent of the metrics gate — capture durability is
            // not an observability option.
            eng.maybe_flush_fill_capture(now);
            eng.maybe_flush_order_capture(now);
            // F21: and the members' own persisted state, for the same
            // reason. It used to sit inside `if let (Some(reg),
            // Some(ids)) = (obs.metrics, obs.counter_ids)`, which made
            // POSITION PERSISTENCE depend on `--metrics` — a flag that
            // is on by default and is therefore exactly the kind of
            // coupling nobody notices until it is off. Capture flushes
            // were hoisted out of that gate deliberately; these belong
            // beside them.
            flush_member_state!();
            // HYPARB H8: hand the period's AMM decisions to the testnet
            // shadow (a ring push each; nothing blocks, nothing is sent
            // from this thread).
            if let Some(tap) = hyparb_shadow.as_mut() {
                tap.drain(eng.strategy());
            }

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
                let live_mask = strategy_core::StrategyCounters::enabled_mask(eng.strategy());
                reg.gauge(ids.strategy_hyparb).set(i64::from(
                    kind == "hyparb" || live_mask & u64::from(strategy_set::BIT_HYPARB) != 0,
                ));
                // F29: the live engine runs the SET, so `kind` is
                // "set" and this gauge read 0 for the whole life of the
                // member — an alert on "is the VRP member running"
                // could never fire. It now answers the question it
                // names: the bare strategy IS vrp, or the set has slot
                // 1 ENABLED right now (a runtime `DisableStrategy`
                // drops it back to 0, which is the point).
                reg.gauge(ids.strategy_vrp).set(i64::from(
                    kind == "vrp" || live_mask & u64::from(strategy_set::BIT_VRP) != 0,
                ));
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
                mirror_xmm_metrics(reg, &ids.xmm, eng.strategy(), &mut xmm_last);
                mirror_vrp_metrics(reg, &ids.vrp, eng.strategy(), &mut vrp_last);
                mirror_xsd_metrics(reg, &ids.xsd, eng.strategy(), &mut xsd_last);
                mirror_bin15_metrics(reg, &ids.bin15, eng.strategy(), &mut bin15_last);
                mirror_hyparb_metrics(reg, &ids.hyparb, eng.strategy(), &mut hyparb_last);
                mirror_hyparb_evm_metrics(
                    reg,
                    &ids.hyparb_evm,
                    match hyparb_live_status.as_deref() {
                        Some(l) => Some(&l.status),
                        None => hyparb_shadow
                            .as_ref()
                            .map(crate::evm_testnet::ShadowTap::status),
                    },
                    hyparb_shadow_dark,
                    &mut hyparb_evm_last,
                );
                // X1: what the paper matcher did. `ioc_canceled` is the
                // F7 counter — a mid-priced IoC on a real spread never
                // fills, and the member used to call that a position.
                mirror_paper_matcher_metrics(
                    reg,
                    &ids.paper_matcher,
                    clob_dispatcher::OrderDispatch::matcher_counters(eng.dispatcher()),
                    clob_dispatcher::OrderDispatch::open_paper_orders(eng.dispatcher()),
                    [
                        strategy_core::StrategyCounters::fills_unrouted(eng.strategy()),
                        strategy_core::StrategyCounters::order_events_unrouted(eng.strategy()),
                    ],
                    eng.lifecycle_counters(),
                    &mut matcher_last,
                    &mut fills_unrouted_last,
                    &mut lifecycle_last,
                );
                mirror_regime_metrics(reg, &ids.regime, eng.strategy(), &mut regime_last, now);
                // HAR H3.5: day ages are wall time (UTC days).
                mirror_har_metrics(
                    reg,
                    &ids.har,
                    eng.strategy(),
                    &mut har_rows,
                    obs.boot
                        .boot_wall_ns
                        .wrapping_add(now.wrapping_sub(obs.boot.boot_mono_ns))
                        / 1_000_000,
                );
                // E1: the router's own counters. Absent family = no
                // `--exec` = nothing to mirror, and no branch cost that
                // a pre-E1 boot did not already pay.
                if let Some(ex) = ids.exec.as_ref() {
                    mirror_exec_metrics(
                        reg,
                        ex,
                        clob_dispatcher::OrderDispatch::exec_counters(eng.dispatcher()),
                        &mut exec_last,
                    );
                }

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
                    reg.gauge(ids.ingress_hl_rolls)
                        .set(ing.hl_roll.rolls_total() as i64);
                    reg.gauge(ids.ingress_hl_rolls_ignored)
                        .set(ing.hl_roll.rolls_ignored_unmatched() as i64);
                    reg.gauge(ids.ingress_hl_family_ack_timeouts)
                        .set(ing.hl_roll.family_ack_timeouts() as i64);
                    reg.gauge(ids.ingress_hl_families_dormant)
                        .set(ing.hl_roll.families_dormant() as i64);
                    reg.gauge(ids.ingress_hl_outcome_bbo_one_sided)
                        .set(ing.hl_roll.outcome_bbo_one_sided() as i64);
                    reg.gauge(ids.ingress_hyperliquid_state)
                        .set(ing.hyperliquid.state() as i64);
                    reg.gauge(ids.ingress_bybit_state)
                        .set(ing.bybit.state() as i64);
                    reg.gauge(ids.ingress_rpc_state).set(ing.rpc.state() as i64);
                    reg.gauge(ids.ingress_mexc_state)
                        .set(ing.mexc.state() as i64);
                    reg.gauge(ids.ingress_hyperevm_state)
                        .set(ing.hyperevm.state() as i64);
                    reg.gauge(ids.ingress_hypercall_state)
                        .set(ing.hypercall.state() as i64);
                    // HC5: the venue family — plain stores (see
                    // `HcMetricIds`).
                    let hc = hc_metric_values(&ing.hc);
                    let mut k = 0usize;
                    while k < HC_METRICS {
                        reg.gauge(ids.hypercall.gauges[k]).set(hc[k] as i64);
                        k += 1;
                    }
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
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_mexc,
                        &ing.mexc,
                        &mut ingress_last[7],
                    );
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_hyperevm,
                        &ing.hyperevm,
                        &mut ingress_last[8],
                    );
                    mirror_ingress_counters(
                        reg,
                        &ids.ingress_hypercall,
                        &ing.hypercall,
                        &mut ingress_last[9],
                    );

                    // T1(c): per-venue last-tick age from the stamps
                    // the 1 s gate keeps. A lane that stops moving
                    // data goes visibly stale here even while its
                    // ~1 Hz reconnect churn keeps the state gauge
                    // reading Up (outage 2026-08-27 §5.5). -1 = no
                    // tick since boot.
                    let mut i = 0;
                    while i < SNAPSHOT_VENUES {
                        let (_, stamp) = tick_age_track[i];
                        let age_s: i64 = if stamp == 0 {
                            -1
                        } else {
                            (now.saturating_sub(stamp) / 1_000_000_000) as i64
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

    // F18: THE DRAIN LAW. The restart lane SIGTERMs five times a UTC
    // day and the state writer ran only on the 5 s report cadence, so
    // up to five seconds of campaign state was lost at every one of
    // them. The 00:10Z slot sits ON the edge of the VRP decision band
    // `[E−τ, E−τ+selection]`: an entry at 00:09:57 with the drain at
    // 00:10:00 was never written, the reboot restored `entry_done = 0`,
    // and the campaign came back as an orphaned option and hedge that
    // nothing would re-hedge or settle.
    //
    // Unconditional, and a no-op on an unchanged epoch.
    flush_member_state!();
    eng.stop();
    // H3.7: the state writer stops and is joined FIRST — a write in flight
    // and the forced write below must never share a path's temp file. A
    // state it still held is superseded by that write.
    if let Some(w) = har_writer {
        w.shutdown();
    }
    // HAR H3.4: after `on_stop` delivered any minute the day-close stagger
    // still held, every series' state is written UNCONDITIONALLY — its
    // open day moves the state without moving the epoch.
    write_har_state(
        &har_state_paths,
        eng.strategy(),
        &mut har_state_epochs,
        &mut har_state_buf,
        &mut har_state_warn_ns,
        now_ns(),
        true,
    );
    // S7-L1: what the live arm's shutdown sweep did — every resting
    // order of ours taken off the venue, or how many it could not
    // confirm (`u64::MAX`: the open orders could not be read).
    let ec = eng.dispatcher().exec_counters();
    if ec.configured != 0 {
        tracing::info!(
            cancelled_total = ec.arm.sweep_all_cancelled,
            left = ec.arm.sweep_all_left,
            "exec: shutdown sweep of resting orders done"
        );
    }
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

/// Outcome of an engine loop ([`engine_loop_set_full`] and the
/// standalone ev / rule-tree arms).
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

/// How long the process will wait for its threads to join before it
/// kills itself.
///
/// Generous — a healthy ingress thread notices `SHUTDOWN` within one
/// mio cycle, so this only ever elapses when a thread is genuinely
/// stuck. It is a backstop, not a timeout anybody should hit.
pub const JOIN_GRACE: Duration = Duration::from_secs(20);

/// Exit code when [`JOIN_GRACE`] elapses and the process force-exits.
/// Distinct from the ordinary boot-abort `1` so an operator reading
/// `launchctl list` can tell "refused to boot" from "would not die".
pub const EXIT_JOIN_TIMEOUT: i32 = 75;

/// Join `handles` in reverse boot order. Errors are logged, never
/// propagated — we're already shutting down.
///
/// ## Why this signals shutdown first
///
/// Every ingress run-loop polls [`SHUTDOWN`] and returns when it is
/// set. The NORMAL shutdown path sets it (SIGINT, or the engine loop
/// returning) before it gets here — but the ~30 **boot-abort** sites do
/// not: they hit a refusal, call this, and return an exit code. Joining
/// threads that were never asked to stop blocks forever.
///
/// Observed live 2026-09-15: a Deribit outage left the options chain
/// empty, the VRP member correctly refused the boot, and the process
/// then sat at 15–21 % CPU for **seven minutes** wedged right here.
/// Because it never exited, launchd KeepAlive could not relaunch it, so
/// the engine stayed down long after the venue had recovered — a
/// transient outage turned into an engine that needed a human. Worse,
/// `/state` was already serving (the metrics thread starts before the
/// engine loop), so a monitor saw a live-looking engine reporting a
/// zeroed boot block.
///
/// Signalling here rather than at each of the 30 call sites is
/// deliberate: it is one place that cannot be forgotten by the 31st.
/// It is idempotent, and on the normal path the flag is already set.
///
/// ## Why there is a watchdog
///
/// A thread that ignores the flag — blocked in a syscall with no
/// timeout — would still hang the join. **The process MUST reach exit**,
/// because a supervisor can only restart a process that dies. After
/// [`JOIN_GRACE`] the watchdog force-exits with [`EXIT_JOIN_TIMEOUT`].
/// Fail-fast beats a graceful wait that never ends.
pub fn join_reverse(handles: Vec<JoinHandle<()>>) {
    // Nothing spawned, nothing to stop, and no reason to touch a
    // process-wide flag: return before either step below.
    if handles.is_empty() {
        return;
    }

    // (1) Tell the threads to stop. On a boot abort nothing else has.
    signal_shutdown();

    // (2) The backstop.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done_w = done.clone();
        let n = handles.len();
        // Detached on purpose: it must outlive nothing and be joined by
        // nobody, or it becomes the hang it exists to prevent.
        let _ = thread::Builder::new()
            .name("join-watchdog".into())
            .spawn(move || {
                let deadline = std::time::Instant::now() + JOIN_GRACE;
                while std::time::Instant::now() < deadline {
                    if done_w.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if !done_w.load(Ordering::Acquire) {
                    // `eprintln!` rather than `tracing`: the subscriber
                    // may itself be waiting on a thread we are about to
                    // kill, and this line is the only evidence of why
                    // the process died.
                    eprintln!(
                        "join-watchdog: {n} thread(s) did not stop within {}s after SHUTDOWN \
                         — force-exiting with {EXIT_JOIN_TIMEOUT} so the supervisor can \
                         relaunch. This is a BUG: some thread is not polling the flag.",
                        JOIN_GRACE.as_secs()
                    );
                    std::process::exit(EXIT_JOIN_TIMEOUT);
                }
            });
    }

    for h in handles.into_iter().rev() {
        let name = h.thread().name().unwrap_or("<unnamed>").to_string();
        if let Err(e) = h.join() {
            tracing::error!(thread = %name, error = ?e, "thread join panicked");
        } else {
            tracing::info!(thread = %name, "thread joined");
        }
    }
    // (3) Disarm.
    done.store(true, Ordering::Release);
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
/// resets it (`core_net::should_reset_backoff`, see the spawn loops).
fn sleep_backoff(b: &mut Backoff) {
    let delay = Duration::from_nanos(b.next_delay_ns());
    tracing::debug!(?delay, attempt = b.attempt(), "reconnect backoff");
    thread::sleep(delay);
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
/// One exception runs after boot: [`fetch_hl_outcome_specs`](boot_discovery::fetch_hl_outcome_specs),
/// the Hyperliquid ingress thread's between-session re-read of the
/// live HIP-4 outcomes (2026-09-26 reconnect-loop fix) — cold,
/// throttled, to an address resolved at boot, on a short deadline.
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
        pub deribit_options: Vec<super::DiscoveredOption>,
        /// Hyperliquid coverage; `None` when `--hl-coins` is unset.
        /// Hyperliquid's coin table is still built by
        /// [`super::build_hl_coin_table`] exactly as before.
        pub hl: Option<VenueCoverage>,
        /// BIN15 O2: HL outcome rows whose description parsed to a
        /// KNOWN grammar. The rolling families bind their live
        /// instance out of this at boot, so a family is subscribed at
        /// the first `Steady` rather than after up to a whole period
        /// of waiting for the next lifecycle push. Empty when the
        /// venue is off or no row parsed.
        pub hl_outcome_specs: Vec<ingress_hyperliquid::discovery::HlOutcomeSpec>,
        /// Binance coverage (M1 exchangeInfo audit); `None` when the
        /// caller skipped it (legacy flag boots keep their historical
        /// zero-REST Binance behavior — config boots audit).
        pub bn: Option<VenueCoverage>,
        /// M2.4: the selected Binance eapi options chain — `(symbol,
        /// sym)` in deterministic allocation order (base
        /// [`BN_OPT_ORDINAL_BASE`]), the OKX shape. The bin builds the
        /// options lane table from these. Empty when the policy is
        /// disabled.
        pub bn_options: Vec<(String, SymbolId)>,
        /// WS9: Bybit coverage (instruments-info audit, spot + linear
        /// pages); `None` when the `[bybit]` section is empty.
        pub bybit: Option<VenueCoverage>,
        /// MX6: MEXC coverage (spot `exchangeInfo` + futures
        /// `contract/detail` audit); `None` when `[mexc]` is empty.
        pub mexc: Option<VenueCoverage>,
        /// MX6 (ruling Q-MX3): one boot REST `funding_rate/{SYM}` seed
        /// per LIVE configured perp, in `[mexc] perp` order — the
        /// Funding events' `v1` clock. Empty when no perp is live.
        pub mexc_funding: Vec<(String, ingress_mexc::discovery::MexcFundingSeed)>,
        /// HC4: Hypercall coverage — `configured` underlyings,
        /// `matched` = those that selected a chain, `universe` = the
        /// candidate rows `/markets` listed for them. `None` when
        /// `[hypercall]` is off.
        pub hypercall: Option<VenueCoverage>,
        /// HC4: the selected Hypercall capped chain (O-HC2), in the
        /// DETERMINISTIC allocation order (underlyings in config order;
        /// per underlying: expiry asc → strike asc → call before put;
        /// ordinals from [`OPT_ORDINAL_BASE`]) with the venue's own
        /// terms. Empty when the lane is off.
        pub hypercall_options: Vec<super::DiscoveredOption>,
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
    ) -> Result<Vec<super::DiscoveredOption>, &'static str> {
        let (host, port) = split_host_port(&cfg.deribit_rest_host, 443)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut out: Vec<super::DiscoveredOption> = Vec::new();
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
                // P5.2: the venue's OWN numeric terms travel with the
                // name. They were parsed out of this row's REST JSON a
                // moment ago and thrown away, and the VRP boot then
                // re-derived them by parsing the name back apart — two
                // laws for one fact, one of them a string parser.
                out.push((
                    name.to_string(),
                    sym,
                    row.strike_1e9,
                    row.expiration_ts_ms,
                    if row.is_call {
                        opt_registry::RIGHT_CALL
                    } else {
                        opt_registry::RIGHT_PUT
                    },
                ));
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

    /// The `/info` body that lists every live HIP-4 outcome.
    const HL_OUTCOME_META_REQ: &[u8] = br#"{"type":"outcomeMeta"}"#;

    /// Deadline for the between-session re-read: the ingress thread
    /// waits on it with the lane dark, so it is kept far below the
    /// boot's [`FETCH_TIMEOUT`].
    const REDISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

    /// The live HIP-4 outcome specs, re-read from
    /// `/info {"type":"outcomeMeta"}` with the boot's own request and
    /// parser.
    ///
    /// Reconnect-loop fix (2026-09-26): how the Hyperliquid ingress
    /// re-discovers the successor of a rolling family it retired at a
    /// reconnect — `outcomeMetaUpdates` replays nothing on subscribe,
    /// so an `outcomeCreated` a dead session missed never comes back,
    /// and without this a daily family would stay dark until the next
    /// day's instance. Cold path: the ingress thread calls it between
    /// sessions, only while a family awaits a successor, at most once a
    /// minute — to `ep`'s boot-resolved address, so no DNS lookup runs
    /// and every socket step is armed with what remains of
    /// [`REDISCOVERY_TIMEOUT`].
    pub fn fetch_hl_outcome_specs(
        tls: &Arc<rustls::ClientConfig>,
        ep: &super::WssEndpoint,
    ) -> Result<Vec<ingress_hyperliquid::discovery::HlOutcomeSpec>, &'static str> {
        let mut buf = Vec::new();
        let range = core_net::boot_http::https_post_at(
            tls,
            ep.addr,
            &ep.host,
            &ep.path,
            USER_AGENT,
            b"application/json",
            HL_OUTCOME_META_REQ,
            &mut buf,
            MAX_BODY,
            REDISCOVERY_TIMEOUT,
        )
        .map_err(|e| {
            tracing::warn!(venue = "hl", request = "outcomeMeta", error = ?e, "re-discovery: fetch failed");
            "hl: outcomeMeta fetch failed"
        })?;
        let mut d = HlDiscovery::new();
        d.ingest_outcome_meta(&buf[range]).map_err(|e| {
            tracing::warn!(venue = "hl", request = "outcomeMeta", error = ?e, "re-discovery: parse failed");
            "hl: outcomeMeta parse failed"
        })?;
        Ok(d.outcome_specs())
    }

    #[allow(clippy::type_complexity)]
    fn run_hl(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spec: &str,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
        out_specs: &mut Vec<ingress_hyperliquid::discovery::HlOutcomeSpec>,
    ) -> Result<VenueCoverage, &'static str> {
        let (host, port) = split_host_port(&cfg.hyperliquid_api_host, 443)?;
        let mut d = HlDiscovery::new();

        let requests: [(&str, &[u8]); 4] = [
            ("meta", br#"{"type":"meta"}"#),
            ("spotMeta", br#"{"type":"spotMeta"}"#),
            ("perpDexs", br#"{"type":"perpDexs"}"#),
            ("outcomeMeta", HL_OUTCOME_META_REQ),
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

        // BIN15 O2: hand the parsed outcome economics back before the
        // discovery table is dropped — the rolling families' boot
        // binding reads this instead of waiting a whole period for the
        // venue's next lifecycle push.
        *out_specs = d.outcome_specs();

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
    /// `(symbol, sym)` — the lane needs nothing per underlying since
    /// BX0-F2: every element of the `<uly>@optionMarkPrice` array
    /// carries its own index price.
    fn run_bn_options(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        policy: &OptionsPolicy,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<Vec<(String, SymbolId)>, &'static str> {
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

        let mut out: Vec<(String, SymbolId)> = Vec::new();
        let mut k = 0u32;
        for uly in &policy.underlyings {
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
                out.push((symbol.to_string(), sym));
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
        mexc: Option<(&[String], &[String])>,
        hypercall_policy: &OptionsPolicy,
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

        let mut hl_outcome_specs = Vec::new();
        let hl = match hl_spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) => Some(run_hl(
                cfg,
                tls_config,
                spec,
                &mut buf,
                &mut any_missing,
                &mut hl_outcome_specs,
            )?),
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

        // MX6: the MEXC audit (spot exchangeInfo + futures
        // contract/detail, only the configured classes) + the Q-MX3
        // funding seeds for every live configured perp.
        let (mexc_cov, mexc_funding) = match mexc {
            Some((spot, perp)) if !spot.is_empty() || !perp.is_empty() => {
                let (cov, seeds) =
                    run_mexc(cfg, tls_config, spot, perp, &mut buf, &mut any_missing)?;
                (Some(cov), seeds)
            }
            _ => (None, Vec::new()),
        };

        // HC4: ONE /markets pass + the O-HC2 capped chain.
        let (hypercall, hypercall_options) = if hypercall_policy.enabled() {
            let (cov, opts) =
                run_hypercall(cfg, tls_config, hypercall_policy, &mut buf, &mut any_missing)?;
            (Some(cov), opts)
        } else {
            (None, Vec::new())
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
            hl_outcome_specs,
            bn,
            bn_options,
            bybit: bybit_cov,
            mexc: mexc_cov,
            mexc_funding,
            hypercall,
            hypercall_options,
        })
    }

    /// HC4: the Hypercall boot discovery. ONE `GET /markets` — every
    /// listed instrument, ≈ 4.3 MB (927 ms from the Mac, 2026-09-25) —
    /// scanned in one forward pass (`ingress_hypercall::discovery`),
    /// then the O-HC2 capped chain per configured underlying: the
    /// nearest `expiries` series OUTSIDE the provider's pre-expiry
    /// quoting blackout × the `strikes` nearest the venue's index.
    ///
    /// A configured underlying the venue does not list is FATAL (a typo
    /// must never boot a silently smaller universe — the Deribit-combo
    /// precedent); one that is listed but selects nothing (every series
    /// inside the blackout) marks the boot `any_missing`. Options take
    /// ordinals from [`OPT_ORDINAL_BASE`] in selection order; the
    /// indices below them are the config file's.
    fn run_hypercall(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        policy: &OptionsPolicy,
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<(VenueCoverage, Vec<super::DiscoveredOption>), &'static str> {
        use ingress_hypercall::discovery::{
            parse_markets, select_universe, DiscoveryErr, DEFAULT_BLACKOUT_MS, MARKETS_MAX_BODY,
            MARKETS_PATH,
        };
        let (host, port) = split_host_port(&cfg.hypercall_rest_host, 443)?;
        let range = core_net::boot_http::https_get(
            tls,
            host,
            port,
            MARKETS_PATH,
            USER_AGENT,
            buf,
            MARKETS_MAX_BODY,
            FETCH_TIMEOUT,
        )
        .map_err(|e| {
            tracing::error!(venue = "hypercall", error = ?e, "discovery: /markets fetch failed");
            "hypercall: /markets fetch failed"
        })?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let unds: Vec<&[u8]> = policy.underlyings.iter().map(|u| u.as_bytes()).collect();
        let m = parse_markets(&buf[range], &unds, now_ms, DEFAULT_BLACKOUT_MS).map_err(|e| {
            let underlying = match e {
                DiscoveryErr::NotListed(i) => policy.underlyings.get(i).map_or("?", String::as_str),
                _ => "",
            };
            tracing::error!(venue = "hypercall", error = %e, underlying, "discovery: /markets refused");
            match e {
                DiscoveryErr::NotListed(_) => {
                    "hypercall: a configured underlying is not listed — fix [hypercall] underlyings"
                }
                DiscoveryErr::TooManyUnderlyings => "hypercall: too many underlyings configured",
                DiscoveryErr::Malformed => "hypercall: /markets body is not a markets answer",
            }
        })?;
        let sel = select_universe(&m, unds.len(), policy.expiries, policy.strikes);
        let mut per_und = vec![0u32; unds.len()];
        let mut out: Vec<super::DiscoveredOption> = Vec::with_capacity(sel.len());
        for (k, row) in sel.iter().enumerate() {
            let name = core::str::from_utf8(row.name())
                .map_err(|_| "hypercall: non-utf8 option instrument name")?;
            let sym = make_symbol_id(VenueId::Hypercall, OPT_ORDINAL_BASE + k as u32 + 1);
            if let Some(n) = per_und.get_mut(row.underlying as usize) {
                *n += 1;
            }
            out.push((
                name.to_string(),
                sym,
                row.strike_1e9,
                row.exp_ms,
                if row.call {
                    opt_registry::RIGHT_CALL
                } else {
                    opt_registry::RIGHT_PUT
                },
            ));
        }
        let mut matched = 0u32;
        for (i, u) in policy.underlyings.iter().enumerate() {
            let selected = per_und[i];
            if selected == 0 {
                *any_missing = true;
                tracing::error!(
                    venue = "hypercall",
                    underlying = %u,
                    reason = "no_chain",
                    "discovery: options underlying selected no instruments"
                );
            } else {
                matched += 1;
            }
            tracing::info!(
                venue = "hypercall",
                underlying = %u,
                index_px_1e9 = m.index_1e9[i],
                expiries = policy.expiries,
                strikes = policy.strikes,
                selected,
                "discovery: options chain"
            );
        }
        if out.len() > ingress_hypercall::HC_MAX_INSTRUMENTS {
            tracing::error!(
                venue = "hypercall",
                selected = out.len(),
                cap = ingress_hypercall::HC_MAX_INSTRUMENTS,
                "discovery: selected chain exceeds the one-frame subscribe cap"
            );
            return Err("hypercall: selected chain exceeds HC_MAX_INSTRUMENTS — shrink \
                 [hypercall] underlyings/expiries/strikes");
        }
        tracing::info!(
            venue = "hypercall",
            candidates = m.rows.len(),
            refused = m.refused,
            selected = out.len(),
            "discovery: hypercall universe"
        );
        Ok((
            VenueCoverage {
                configured: unds.len() as u32,
                matched,
                universe: m.rows.len() as u32,
            },
            out,
        ))
    }

    /// MX6: the MEXC boot audit. Spot: ONE `GET /api/v3/exchangeInfo`
    /// (the whole ~1.6 MB list — boot-only buffer, the Binance
    /// exchangeInfo precedent; a row is live when `status == "1"` and
    /// `permissions` carries `SPOT`). Futures: ONE `GET
    /// /api/v1/contract/detail` (live = `state == 0` and `apiAllowed`),
    /// then one `GET /api/v1/contract/funding_rate/{SYM}` per LIVE
    /// configured perp, paced 110 ms (the venue's 20 req / 2 s limit),
    /// whose `nextSettleTime` + `collectCycle` seed the Funding `v1`
    /// clock (ruling Q-MX3). xStocks and TradFi perps are ordinary
    /// rows — no equity branch (plan §1.3). A discovery fetch/parse
    /// failure is fatal (the discovery contract); a SEED failure is not
    /// (operator ruling 2026-09-23 — that perp boots with Funding
    /// v1 = 0); a missing symbol flags `any_missing` and gets no seed.
    fn run_mexc(
        cfg: &Config,
        tls: &Arc<rustls::ClientConfig>,
        spot: &[String],
        perp: &[String],
        buf: &mut Vec<u8>,
        any_missing: &mut bool,
    ) -> Result<(VenueCoverage, Vec<(String, ingress_mexc::discovery::MexcFundingSeed)>), &'static str>
    {
        use ingress_mexc::discovery as mxd;
        let mut matched = 0u32;
        let mut universe = 0u32;
        let mut seeds = Vec::with_capacity(perp.len());

        if !spot.is_empty() {
            let (host, port) = split_host_port(&cfg.mexc_rest_host, 443)?;
            let range = get(tls, host, port, mxd::SPOT_EXCHANGE_INFO_PATH, buf).map_err(|e| {
                tracing::error!(venue = "mexc", class = "spot", error = ?e, "discovery: fetch failed");
                "mexc: spot discovery fetch failed"
            })?;
            let mut d = mxd::MexcSpotDiscovery::new();
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "mexc", class = "spot", error = ?e, "discovery: parse failed");
                "mexc: spot discovery parse failed"
            })?;
            universe += d.universe_trading();
            for symbol in spot {
                let reason = match d.find(symbol.as_bytes()) {
                    Some(row) if row.trading => {
                        matched += 1;
                        tracing::debug!(
                            venue = "mexc",
                            class = "spot",
                            symbol = symbol.as_str(),
                            tick_size_1e9 = row.tick_size_1e9,
                            lot_step_1e9 = row.lot_step_1e9,
                            maker_fee_1e9 = row.maker_fee_1e9,
                            taker_fee_1e9 = row.taker_fee_1e9,
                            "discovery: mexc instrument resolved"
                        );
                        continue;
                    }
                    Some(_) => "not_trading",
                    None => "not_found",
                };
                *any_missing = true;
                tracing::error!(
                    venue = "mexc",
                    class = "spot",
                    symbol = symbol.as_str(),
                    reason,
                    "discovery: configured symbol missing from venue universe"
                );
            }
        }

        if !perp.is_empty() {
            let (host, port) = split_host_port(&cfg.mexc_fut_rest_host, 443)?;
            let range = get(tls, host, port, mxd::FUT_CONTRACT_DETAIL_PATH, buf).map_err(|e| {
                tracing::error!(venue = "mexc", class = "perp", error = ?e, "discovery: fetch failed");
                "mexc: perp discovery fetch failed"
            })?;
            let mut d = mxd::MexcPerpDiscovery::new();
            d.ingest_body(&buf[range]).map_err(|e| {
                tracing::error!(venue = "mexc", class = "perp", error = ?e, "discovery: parse failed");
                "mexc: perp discovery parse failed"
            })?;
            universe += d.universe_trading();
            for symbol in perp {
                let reason = match d.find(symbol.as_bytes()) {
                    Some(row) if row.trading => {
                        matched += 1;
                        tracing::debug!(
                            venue = "mexc",
                            class = "perp",
                            symbol = symbol.as_str(),
                            contract_size_1e9 = row.contract_size_1e9,
                            price_unit_1e9 = row.price_unit_1e9,
                            vol_unit_1e9 = row.vol_unit_1e9,
                            maker_fee_1e9 = row.maker_fee_1e9,
                            taker_fee_1e9 = row.taker_fee_1e9,
                            "discovery: mexc instrument resolved"
                        );
                        None
                    }
                    Some(_) => Some("not_trading"),
                    None => Some("not_found"),
                };
                if let Some(reason) = reason {
                    *any_missing = true;
                    tracing::error!(
                        venue = "mexc",
                        class = "perp",
                        symbol = symbol.as_str(),
                        reason,
                        "discovery: configured symbol missing from venue universe"
                    );
                    continue;
                }
                std::thread::sleep(Duration::from_millis(110));
                // Operator ruling 2026-09-23: a funding SEED is an
                // enrichment, not the venue's instrument truth — a
                // failed fetch/parse never refuses the (all-venue)
                // boot. The perp boots unseeded (Funding v1 = 0 until
                // the next restart); rates still flow and the worker's
                // funding history lane is the authority.
                let path = format!("{}{}", mxd::FUT_FUNDING_RATE_PATH, symbol);
                let range = match get(tls, host, port, &path, buf) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(venue = "mexc", symbol = symbol.as_str(), error = ?e, "discovery: funding seed fetch failed — booting unseeded (v1 = 0)");
                        continue;
                    }
                };
                let seed = match mxd::parse_funding_rate(&buf[range]) {
                    Ok(seed) => seed,
                    Err(e) => {
                        tracing::error!(venue = "mexc", symbol = symbol.as_str(), error = ?e, "discovery: funding seed parse failed — booting unseeded (v1 = 0)");
                        continue;
                    }
                };
                tracing::debug!(
                    venue = "mexc",
                    symbol = symbol.as_str(),
                    next_settle_ms = seed.next_settle_ms,
                    collect_cycle_h = seed.collect_cycle_h,
                    rate_1e9 = seed.rate_1e9,
                    "discovery: mexc funding seed"
                );
                seeds.push((symbol.clone(), seed));
            }
        }

        let configured = (spot.len() + perp.len()) as u32;
        tracing::info!(
            venue = "mexc",
            configured,
            matched,
            universe,
            seeds = seeds.len(),
            "discovery: coverage"
        );
        Ok((
            VenueCoverage {
                configured,
                matched,
                universe,
            },
            seeds,
        ))
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

    /// BX0-F1: the markPrice slot dials fstream's ROUTED `/market`
    /// path — the legacy `/ws/` form answers 101 and then stays silent
    /// — while bookTicker keeps the legacy path that still delivers.
    /// One builder feeds the boot and the live smoke, so this pins
    /// both.
    #[test]
    fn usdm_specs_route_mark_price_via_market_and_keep_book_ticker_legacy() {
        let [book, mark] = bn_usdm_specs("fstream.binance.com", "btcusdt_260925", 7);
        assert_eq!(book.host, "fstream.binance.com");
        assert_eq!(book.path, "/ws/btcusdt_260925@bookTicker");
        assert!(!book.mark_price && !book.spot_sentinel && book.eapi.is_none());
        assert_eq!(book.sym, 7);
        assert_eq!(mark.host, "fstream.binance.com");
        assert_eq!(mark.path, "/market/ws/btcusdt_260925@markPrice");
        assert!(mark.mark_price && !mark.spot_sentinel && mark.eapi.is_none());
        assert_eq!(mark.sym, 7);
        // The spot sentinel reads its symbol off the LEGACY bookTicker
        // shape; the USDⓈ-M book path must keep matching it.
        assert_eq!(spot_stream_symbol(&book.path), Some("btcusdt_260925"));
    }

    /// BX0-F2: one `<uly>@optionMarkPrice` stream per underlying,
    /// lowercased, on the routed `/market` combined path — never the
    /// retired `/eoptions/` form or the per-option `@ticker`/`@index`
    /// streams (HTTP 404 since the 2025-12 options migration).
    #[test]
    fn options_path_is_the_routed_mark_array_stream() {
        assert_eq!(
            bn_options_path(&["BTCUSDT".to_string(), "ETHUSDT".to_string()]),
            "/market/stream?streams=btcusdt@optionMarkPrice/ethusdt@optionMarkPrice"
        );
        assert_eq!(
            bn_options_path(&["BTCUSDT".to_string()]),
            "/market/stream?streams=btcusdt@optionMarkPrice"
        );
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
        // Built over each lane array's own length (HC1: the eighth tick
        // lane and fourth opt lane are not an edit here).
        let tick_lanes = core::array::from_fn(|i| rings.tick[i].clone().split().1);
        let fill_lanes = core::array::from_fn(|i| rings.fill[i].clone().split().1);
        let event_lanes = core::array::from_fn(|i| rings.event[i].clone().split().1);
        let depth_lanes = core::array::from_fn(|i| rings.depth[i].clone().split().1);
        let opt_lanes = core::array::from_fn(|i| rings.opt[i].clone().split().1);
        Consumers {
            tick_lanes,
            event_lanes,
            depth_lanes,
            opt_lanes,
            rpc_signal: rings.rpc_signal.clone().split().1,
            hyperevm_signal: rings.hyperevm_signal.clone().split().1,
            trades: rings.trade.clone().split().1,
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
        let dir = std::env::temp_dir().join(format!("gauged_fwd_{}", std::process::id()));
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
            &ChannelEvent::new(
                1,
                VenueId::Okx,
                core_types::ChannelId::Funding,
                7,
                0,
                0,
                1,
                0,
            ),
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
        // 8g item 4: the ruleset-table handoff ring is sized as planned
        // and its type round-trips a slot. `split_all_consumers` already
        // took this ring's handles — a ring splits exactly once — so the
        // round trip runs on a fresh ring of the same type.
        assert_eq!(rings.ruleset_tables.capacity(), RULE_TABLE_RING_SLOTS);
        let (mut tp, mut tc) = Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
        assert!(tp.try_push_ref(&core_types::RuleTableV2::EMPTY));
        assert!(tc.try_pop_ref().is_some());
        assert!(tc.try_pop_ref().is_none());
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

        let mexc = [make_symbol_id(VenueId::Mexc, 1), make_symbol_id(VenueId::Mexc, 513)];
        let hc = [make_symbol_id(VenueId::Hypercall, 513), make_symbol_id(VenueId::Hypercall, 514)];
        let u = build_ai_universe(&[42], &[7], Some(&okx), Some(&deribit), Some(&hl), &mexc, &hc);
        let expect: Vec<u32> = {
            let mut v = vec![
                42,
                7,
                make_symbol_id(VenueId::Mexc, 1),
                make_symbol_id(VenueId::Mexc, 513),
                make_symbol_id(VenueId::Hypercall, 513),
                make_symbol_id(VenueId::Hypercall, 514),
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
        let u = build_ai_universe(&[7], &[7], None, None, None, &[], &[]);
        assert_eq!(&u[..], &[7]);

        // No optional venues: exactly the sorted pair.
        let u = build_ai_universe(&[42], &[7], None, None, None, &[], &[]);
        assert_eq!(&u[..], &[7, 42]);

        // M1 multi-market: every PM token + every BN sym flows in.
        let u = build_ai_universe(&[42, 2, 3], &[7, 16_777_218], None, None, None, &[], &[]);
        assert_eq!(&u[..], &[2, 3, 7, 42, 16_777_218]);

        // O-HC17: the Hypercall options sort among the rest, a duplicate
        // collapses; the caller passes no index sym.
        let o = make_symbol_id(VenueId::Hypercall, 600);
        let u = build_ai_universe(&[42], &[7], None, None, None, &[], &[o, o]);
        assert_eq!(&u[..], &[7, 42, o]);
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

    /// O4b: the family's SIZE, and the headroom it leaves.
    ///
    /// The registry is a fixed array (`MAX_COUNTERS` 256, `MAX_GAUGES`
    /// 384) and registration is unconditional, so every family a lane
    /// adds is paid for at every boot whatever the mask. Measured on the
    /// live engine at the O4b battery: 198 counters before this family,
    /// 220 after — **36 counters of headroom left**. The next member
    /// that needs more than that has to raise `MAX_COUNTERS` rather than
    /// discover `RegErr::Full` at a live boot, which is a refused
    /// registration and therefore a refused boot.
    ///
    /// BIN15 O6 took the per-family levels from 4 to 10, so this block
    /// is 80 gauges. Measured on the live engine the same day: 200
    /// gauges in use before the change, 248 after — **136 of headroom
    /// left**, and the counter side did not move.
    ///
    /// BIN15 P0 (F4) added `skipped_mark_stale` and P3 (F6) added
    /// `skipped_entry_price`: 22 → **24 counters**, so 222 in use and
    /// **34 of headroom left**. The gauge side did not move.
    ///
    /// E4 added `settlement_fills`: 24 → **25 counters**, so 223 in use
    /// against `MAX_COUNTERS = 256` and **33 of headroom left**. The
    /// gauge side did not move. It is registered rather than left as a
    /// bare field on purpose — settlements used to be counted in
    /// `unknown_fills`, which IS published, so an unpublished
    /// replacement would have moved them from a visible series to a
    /// field only a unit test can see.
    ///
    /// E5 commit 4b added the Arm B lifecycle block —
    /// `quotes_modified`, `quotes_modify_refused`, `quotes_cancelled`,
    /// `quotes_cancel_refused`, `quotes_raced`, `skipped_partial`:
    /// 25 → **31 counters**. All six are published for the same
    /// reason `settlement_fills` was: `quotes_cancel_refused` in
    /// particular names a quote the member has stopped tracking and
    /// the venue may still hold, which is not a number to leave where
    /// only a unit test can see it.
    ///
    /// The E5 lifecycle block in the paper-matcher family
    /// (commit 3, 4 counters) and the engine block (4 more) also
    /// landed since the last count, so the registry-wide headroom
    /// note above is stale by more than this family's six. The
    /// registry's own `MAX_COUNTERS` assertion is what actually
    /// guards it; `Observability::build` panicking in every boot test
    /// is what would catch an overflow.
    ///
    /// E7 (2026-09-19): the exec family grew its ledger and live-arm
    /// rows (≈ +30 counters) and `MAX_COUNTERS` went 256 → 512 rather
    /// than land `RegErr::Full` on the first live boot. The
    /// `the_exec_family_size_is_pinned` test in `exec_boot` counts
    /// that family; the registry-wide number is re-measured on the
    /// host at every ramp step.
    ///
    /// BIN15 S5 (2026-09-24) added `skipped_entry_persist` and
    /// `skipped_entry_elapsed`: 31 → **33 counters** (pinned by
    /// `the_bin15_family_is_33_counters_and_80_gauges`); the gauge side
    /// did not move.
    ///
    /// HYPARB H6: the family's size is pinned — `RegErr::Full` is a
    /// refused boot, and the registry is shared by every family.
    /// HC5: the Hypercall family is 31 gauges, every name distinct and
    /// within `NAME_MAX`, and the value order IS the name order (the
    /// mirror is positional). A worst-case boot — every exec slot live —
    /// still fits the fixed registry with the new venue's ~50 rows.
    #[test]
    fn the_hypercall_family_is_31_gauges_in_value_order() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let (c0, g0) = (reg.counters_len(), reg.gauges_len());
        let ids = register_hypercall_metrics(&mut reg).expect("register hypercall");
        assert_eq!(reg.counters_len() - c0, 0, "gauges only");
        assert_eq!(reg.gauges_len() - g0, HC_METRICS);
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for n in HC_METRIC_NAMES {
            assert!(n.starts_with("engine_ingress_hypercall_"), "{n}");
            assert!(n.len() <= core_metrics::NAME_MAX, "{n} is {} bytes", n.len());
            assert!(seen.insert(n), "duplicate {n}");
        }
        // Positional: poke one distinct value into each source and read
        // it back at its name's index.
        let c = ingress_hypercall::HcCounters::new();
        let mut k = 0u64;
        for x in c.ws.closes.iter() {
            k += 1;
            x.store(k, Ordering::Relaxed);
        }
        c.ws.subscribes.store(7, Ordering::Relaxed);
        c.ws.crossed_quotes.store(10, Ordering::Relaxed);
        c.ws.listings[1].store(14, Ordering::Relaxed);
        c.ws.clock_rtt_ms.store(23, Ordering::Relaxed);
        c.snapshot_req.store(24, Ordering::Relaxed);
        c.rest.handoff_drops.store(30, Ordering::Relaxed);
        c.rest.last_round_ms.store(31, Ordering::Relaxed);
        let v = hc_metric_values(&c);
        assert_eq!(&v[..6], &[1, 2, 3, 4, 5, 6], "closes in HcCloseCause::ALL order");
        let at = |name: &str| HC_METRIC_NAMES.iter().position(|n| *n == name).expect(name);
        for (i, cause) in ingress_hypercall::HcCloseCause::ALL.iter().enumerate() {
            assert_eq!(at(&format!("engine_ingress_hypercall_closes_{}_total", cause.label())), i);
        }
        assert_eq!(v[at("engine_ingress_hypercall_subscribes_total")], 7);
        assert_eq!(v[at("engine_ingress_hypercall_crossed_quotes_total")], 10);
        assert_eq!(v[at("engine_ingress_hypercall_listings_expired_total")], 14);
        assert_eq!(v[at("engine_ingress_hypercall_clock_rtt_ms")], 23);
        assert_eq!(v[at("engine_ingress_hypercall_snapshot_requests_total")], 24);
        assert_eq!(v[at("engine_ingress_hypercall_rest_handoff_drops_total")], 30);
        assert_eq!(v[at("engine_ingress_hypercall_rest_last_round_ms")], 31);
        assert_eq!(ids.gauges.len(), HC_METRICS);
        // The whole registry, worst case, still fits.
        let obs = Observability::build(true, Some([1u8; clob_dispatcher::EXEC_COUNTER_SLOTS]))
            .expect("every family fits the fixed registry");
        let reg = obs.metrics.as_ref().unwrap();
        assert!(reg.counters_len() <= core_metrics::MAX_COUNTERS);
        assert!(reg.gauges_len() <= core_metrics::MAX_GAUGES);
        let mut buf = vec![0u8; 256 * 1024];
        let n = reg.encode_prometheus(&mut buf).expect("/metrics fits its buffer");
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.contains("engine_ingress_hypercall_state"));
        assert!(text.contains("engine_ingress_hypercall_last_tick_age_seconds"));
        assert!(text.contains("engine_ingress_hypercall_msgs_total"));
        assert!(text.contains("engine_ingress_hypercall_capture_records"));
        assert!(text.contains("engine_ingress_hypercall_coverage_configured"));
        assert!(text.contains("engine_ingress_hypercall_options_selected"));
    }

    /// HAR H3.5: four gauges, always registered (a boot without
    /// `har.toml` reports `configured 0`); the mirror counts the warm
    /// series and reports the STALEST newest-day age (−1: none closed).
    #[test]
    fn the_har_family_is_4_gauges_and_mirrors_the_stalest_day() {
        struct Fake(Vec<strategy_core::HarSeriesView>, u64);
        impl strategy_core::StrategyCounters for Fake {
            fn har_counters(&self) -> strategy_core::HarCounters {
                strategy_core::HarCounters {
                    day_close_ns_max: self.1,
                    ..strategy_core::HarCounters::default()
                }
            }
            fn har_series_view(&self, out: &mut [strategy_core::HarSeriesView]) -> u32 {
                let m = self.0.len().min(out.len());
                out[..m].copy_from_slice(&self.0[..m]);
                self.0.len() as u32
            }
        }
        let mut reg = core_metrics::MetricsRegistry::new();
        let (c0, g0) = (reg.counters_len(), reg.gauges_len());
        let ids = register_har_metrics(&mut reg).expect("register har");
        assert_eq!(reg.counters_len() - c0, 0, "gauges only");
        assert_eq!(reg.gauges_len() - g0, 4);
        assert!(register_har_metrics(&mut reg).is_err(), "names are unique");
        let mut rows = [strategy_core::HarSeriesView::default(); strategy_core::HAR_VIEW_SERIES];
        mirror_har_metrics(&reg, &ids, &Fake(Vec::new(), 0), &mut rows, 0);
        assert_eq!(reg.gauge(ids.configured).get(), 0);
        assert_eq!(reg.gauge(ids.warm).get(), 0);
        assert_eq!(reg.gauge(ids.day_age_max_s).get(), -1, "no series: never");
        // 2026-09-26T09:00Z: one warm series whose newest closed day is
        // 09-25 (ended 9 h ago), one cold series stuck on 09-24 (33 h).
        let wall_ms: u64 = 1_790_413_200_000;
        let current = strategy_core::HarSeriesView {
            warm: 1,
            newest_day_ms: 1_790_294_400_000,
            ..strategy_core::HarSeriesView::default()
        };
        let stuck = strategy_core::HarSeriesView {
            newest_day_ms: 1_790_294_400_000 - core_vol::DAY_MS,
            ..strategy_core::HarSeriesView::default()
        };
        let never = strategy_core::HarSeriesView::default();
        mirror_har_metrics(&reg, &ids, &Fake(vec![current, stuck, never], 70_000), &mut rows, wall_ms);
        assert_eq!(reg.gauge(ids.configured).get(), 3);
        assert_eq!(reg.gauge(ids.warm).get(), 1);
        assert_eq!(reg.gauge(ids.day_age_max_s).get(), 33 * 3_600, "the stalest wins");
        assert_eq!(reg.gauge(ids.day_close_ns_max).get(), 70_000);
    }

    #[test]
    fn the_hyparb_family_is_27_counters_and_36_gauges() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let before_c = reg.counters_len();
        let before_g = reg.gauges_len();
        let ids = register_hyparb_metrics(&mut reg).expect("register hyparb");
        assert_eq!(reg.counters_len() - before_c, 27, "the counter block");
        assert_eq!(
            reg.gauges_len() - before_g,
            36,
            "4 + 4 coins x 5 + 4 pools x 3 (the session P&L level since go-live)"
        );
        // A second registration collides on every name — nothing reused
        // a name silently.
        assert!(register_hyparb_metrics(&mut reg).is_err());
        reg.gauge(ids.coins[3][4]).set(-5);
        assert_eq!(reg.gauge(ids.coins[3][4]).get(), -5);
        assert_eq!(reg.gauge(ids.pools[0][0]).get(), 0);
        // Every counter name is distinct and a counter.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for n in HYPARB_COUNTER_NAMES {
            assert!(
                n.starts_with("engine_hyparb_") && n.ends_with("_total"),
                "{n}"
            );
            assert!(seen.insert(n), "duplicate {n}");
        }
    }

    /// XMM XH3: the slot-6 family — 17 counters (the member's, in name
    /// order) and 1 + 4 gauges; every name distinct, none reused.
    #[test]
    fn the_xmm_family_is_17_counters_and_5_gauges() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let before_c = reg.counters_len();
        let before_g = reg.gauges_len();
        let ids = register_xmm_metrics(&mut reg).expect("register xmm");
        assert_eq!(reg.counters_len() - before_c, 17, "the counter block");
        assert_eq!(reg.gauges_len() - before_g, 1 + XMM_METRIC_PERPS, "perps + positions");
        assert!(register_xmm_metrics(&mut reg).is_err(), "a second registration collides");
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for n in XMM_COUNTER_NAMES {
            assert!(n.starts_with("engine_xmm_") && n.ends_with("_total"), "{n}");
            assert!(seen.insert(n), "duplicate {n}");
        }
        reg.gauge(ids.pos[3]).set(-7);
        assert_eq!(reg.gauge(ids.pos[3]).get(), -7);
    }

    /// XMM XH3: the mirror publishes DELTAS of the member's cumulative
    /// counters and LEVELS for the gauges; perps the member does not
    /// quote read zero; an unconfigured member publishes zeros.
    #[test]
    fn the_xmm_mirror_publishes_deltas_and_levels() {
        struct Fake {
            c: strategy_core::XmmCounters,
            rows: [strategy_core::XmmPerpView; 2],
            n: u32,
        }
        impl strategy_core::StrategyCounters for Fake {
            fn orders_emitted(&self) -> u64 {
                0
            }
            fn orders_dropped(&self) -> u64 {
                0
            }
            fn strategy_kind(&self) -> &'static str {
                "fake"
            }
            fn xmm_counters(&self, out: &mut strategy_core::XmmCounters) {
                *out = self.c;
            }
            fn xmm_perps_view(&self, out: &mut [strategy_core::XmmPerpView]) -> u32 {
                let m = out.len().min(self.rows.len());
                out[..m].copy_from_slice(&self.rows[..m]);
                self.n
            }
        }
        let mut reg = core_metrics::MetricsRegistry::new();
        let ids = register_xmm_metrics(&mut reg).expect("register");
        let mut last = strategy_core::XmmCounters::default();
        let mut f = Fake {
            c: strategy_core::XmmCounters::default(),
            rows: [strategy_core::XmmPerpView::default(); 2],
            n: 2,
        };
        f.c.placed = 9;
        f.c.pull_cancels = 4;
        f.c.stuck = 1;
        f.rows[0].pos_1e6 = 150_000;
        f.rows[1].pos_1e6 = -20_000;
        mirror_xmm_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.counters[0]).get(), 9, "placed");
        assert_eq!(reg.counter(ids.counters[4]).get(), 4, "pull_cancels");
        assert_eq!(reg.counter(ids.counters[16]).get(), 1, "stuck");
        assert_eq!(reg.gauge(ids.perps).get(), 2);
        assert_eq!((reg.gauge(ids.pos[0]).get(), reg.gauge(ids.pos[1]).get()), (150_000, -20_000));
        assert_eq!(reg.gauge(ids.pos[2]).get(), 0, "a perp not quoted reads zero");
        // The same cumulative counters again add nothing; a move adds
        // exactly the move.
        mirror_xmm_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.counters[0]).get(), 9);
        f.c.placed = 12;
        mirror_xmm_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.counters[0]).get(), 12);
        // An unconfigured member: the gauges fall to zero.
        f.n = 0;
        mirror_xmm_metrics(&reg, &ids, &f, &mut last);
        assert_eq!((reg.gauge(ids.perps).get(), reg.gauge(ids.pos[0]).get()), (0, 0));
    }

    /// HYPARB H8: the shadow family's size is pinned too, and its mirror
    /// publishes deltas and levels — and nothing at all with no shadow.
    #[test]
    fn the_hyparb_evm_family_is_18_counters_and_5_gauges_and_mirrors_deltas() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let (c0, g0) = (reg.counters_len(), reg.gauges_len());
        let ids = register_hyparb_evm_metrics(&mut reg).expect("register");
        assert_eq!(reg.counters_len() - c0, 18);
        assert_eq!(reg.gauges_len() - g0, 5);
        assert!(
            register_hyparb_evm_metrics(&mut reg).is_err(),
            "names are unique"
        );
        let mut last = [0u64; crate::evm_testnet::SHADOW_COUNTER_NAMES.len()];
        mirror_hyparb_evm_metrics(&reg, &ids, None, false, &mut last);
        assert_eq!(
            reg.counter(ids.counters[0]).get(),
            0,
            "no shadow: nothing moves"
        );
        assert_eq!(reg.gauge(ids.dark).get(), 0);
        mirror_hyparb_evm_metrics(&reg, &ids, None, true, &mut last);
        assert_eq!(reg.gauge(ids.dark).get(), 1, "a dark shadow is a level");
        use std::sync::atomic::{AtomicU64, Ordering};
        let st = crate::evm_testnet::ShadowStatus {
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            gauges: std::array::from_fn(|_| AtomicU64::new(0)),
        };
        st.counters[5].store(3, Ordering::Relaxed);
        st.gauges[3].store(65_000_000, Ordering::Relaxed);
        mirror_hyparb_evm_metrics(&reg, &ids, Some(&st), false, &mut last);
        st.counters[5].store(5, Ordering::Relaxed);
        mirror_hyparb_evm_metrics(&reg, &ids, Some(&st), false, &mut last);
        assert_eq!(reg.counter(ids.counters[5]).get(), 5, "3 then +2");
        assert_eq!(reg.gauge(ids.gauges[3]).get(), 65_000_000);
    }

    /// The hyparb mirror publishes counter DELTAS in name order (the
    /// value fn and the names are pinned together here), levels as sets,
    /// and holds unconfigured rows at zero.
    #[test]
    fn the_hyparb_mirror_publishes_deltas_levels_and_pool_liveness() {
        struct Fake {
            c: strategy_core::HyparbCounters,
            pool: strategy_core::HyparbPoolView,
            coin: strategy_core::HyparbCoinView,
        }
        impl strategy_core::StrategyCounters for Fake {
            fn orders_emitted(&self) -> u64 {
                0
            }
            fn orders_dropped(&self) -> u64 {
                0
            }
            fn strategy_kind(&self) -> &'static str {
                "fake"
            }
            fn hyparb_counters(&self) -> strategy_core::HyparbCounters {
                self.c
            }
            fn hyparb_pools_view(&self, out: &mut [strategy_core::HyparbPoolView]) -> u32 {
                out[0] = self.pool;
                1
            }
            fn hyparb_coins_view(&self, out: &mut [strategy_core::HyparbCoinView]) -> u32 {
                out[0] = self.coin;
                1
            }
        }
        let mut reg = core_metrics::MetricsRegistry::new();
        let ids = register_hyparb_metrics(&mut reg).expect("register");
        let mut last = strategy_core::HyparbCounters::default();
        let mut f = Fake {
            c: strategy_core::HyparbCounters::default(),
            pool: strategy_core::HyparbPoolView::new(1, 1, 1, 0, 500, 97, -1_000, 3, 42),
            coin: strategy_core::HyparbCoinView::default(),
        };
        f.c.arbs_buy = 3;
        f.c.arbs_sell = 1;
        f.c.gas_charged_usd_1e6 = 30_000;
        f.c.funding_earned_usd_1e6 = -12;
        f.c.halted = 1;
        f.c.pnl_session_usd_1e6 = -20_000_000;
        f.coin.perp_depth_usd_1e6 = 900_000_000;
        mirror_hyparb_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.counters[6]).get(), 3, "side_buy");
        assert_eq!(reg.counter(ids.counters[7]).get(), 1, "side_sell");
        assert_eq!(reg.counter(ids.counters[24]).get(), 30_000, "gas");
        assert_eq!(reg.gauge(ids.funding_earned).get(), -12);
        assert_eq!(reg.gauge(ids.halted).get(), 1);
        assert_eq!(reg.gauge(ids.pnl_session).get(), -20_000_000);
        assert_eq!(reg.gauge(ids.pools_live).get(), 1);
        assert_eq!(reg.gauge(ids.pools[0][0]).get(), -1_000);
        assert_eq!(reg.gauge(ids.pools[0][1]).get(), 42);
        assert_eq!(reg.gauge(ids.pools[1][2]).get(), 0, "unconfigured pool row");
        assert_eq!(reg.gauge(ids.coins[0][0]).get(), 900_000_000);
        // The second pass publishes only the delta.
        f.c.arbs_buy = 5;
        mirror_hyparb_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.counters[6]).get(), 5);
        assert_eq!(reg.counter(ids.counters[7]).get(), 1);
        // The value order IS the name order.
        assert_eq!(HYPARB_COUNTER_NAMES[6], "engine_hyparb_side_buy_total");
        assert_eq!(
            HYPARB_COUNTER_NAMES[24],
            "engine_hyparb_gas_charged_usd_1e6_total"
        );
    }

    #[test]
    fn the_bin15_family_is_33_counters_and_80_gauges() {
        let mut reg = core_metrics::MetricsRegistry::new();
        let before_c = reg.counters_len();
        let before_g = reg.gauges_len();
        let ids = register_bin15_metrics(&mut reg).expect("register bin15");
        assert_eq!(reg.counters_len() - before_c, 33, "the counter block");
        assert_eq!(reg.gauges_len() - before_g, 80, "8 families x 10 levels");
        assert!(
            reg.gauges_len() <= core_metrics::MAX_GAUGES,
            "the block must fit the fixed registry"
        );
        // Every per-family gauge is a DISTINCT name: a copy-paste that
        // reused one would silently publish two families as one.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut f = 0usize;
        while f < BIN15_METRIC_FAMILIES {
            let mut g = 0usize;
            while g < BIN15_FAMILY_GAUGES {
                assert!(
                    seen.insert(BIN15_GAUGE_NAMES[f][g]),
                    "duplicate gauge name {}",
                    BIN15_GAUGE_NAMES[f][g]
                );
                g += 1;
            }
            f += 1;
        }
        assert_eq!(seen.len(), 80);
        // And the ids are usable: a level written is a level read back.
        reg.gauge(ids.families[7][3]).set(2650);
        assert_eq!(reg.gauge(ids.families[7][3]).get(), 2650);
        assert_eq!(reg.gauge(ids.families[0][3]).get(), 0);
    }

    /// The mirror is a DELTA of cumulative counters and a SET of
    /// levels, and a dormant family keeps its row at zero rather than
    /// vanishing — a missing series reads as a scrape problem.
    #[test]
    fn the_bin15_mirror_publishes_deltas_and_holds_dormant_rows_at_zero() {
        struct Fake {
            c: strategy_core::Bin15Counters,
            view: [strategy_core::Bin15FamilyView; 2],
        }
        impl strategy_core::StrategyCounters for Fake {
            fn orders_emitted(&self) -> u64 {
                0
            }
            fn orders_dropped(&self) -> u64 {
                0
            }
            fn strategy_kind(&self) -> &'static str {
                "fake"
            }
            fn bin15_counters(&self) -> strategy_core::Bin15Counters {
                self.c
            }
            fn bin15_families_view(&self, out: &mut [strategy_core::Bin15FamilyView]) -> u32 {
                out[..2].copy_from_slice(&self.view);
                2
            }
        }
        let mut reg = core_metrics::MetricsRegistry::new();
        let ids = register_bin15_metrics(&mut reg).expect("register");
        let mut last = strategy_core::Bin15Counters::default();
        let mut f = Fake {
            c: strategy_core::Bin15Counters::default(),
            view: [strategy_core::Bin15FamilyView::default(); 2],
        };
        f.c.reprices = 7;
        f.c.takes_filled = 2;
        f.view[0] = strategy_core::Bin15FamilyView {
            live_outcome: 2650,
            tau_s: 640,
            p_hat_1e6: 894_000,
            pos_yes_1e6: 100_000_000,
            pos_no_1e6: 0,
            p_raw_1e6: 857_000,
            strike_1e6: 77_131_000_000,
            mark_1e6: 77_170_500_000,
            d_1e6: 1_240_000,
            den_1e9: 318_000,
        };
        mirror_bin15_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.reprices).get(), 7);
        assert_eq!(reg.counter(ids.takes_filled).get(), 2);
        assert_eq!(reg.gauge(ids.families[0][0]).get(), 894_000);
        assert_eq!(reg.gauge(ids.families[0][3]).get(), 2650);
        // BIN15 O6: the six that explain the fair value. `p_raw` under
        // `p_hat` is the recal table sharpening the forecast, which is
        // exactly the effect the live gauges exist to show.
        assert_eq!(reg.gauge(ids.families[0][4]).get(), 857_000);
        assert_eq!(reg.gauge(ids.families[0][5]).get(), 77_131_000_000);
        assert_eq!(reg.gauge(ids.families[0][6]).get(), 77_170_500_000);
        assert_eq!(reg.gauge(ids.families[0][7]).get(), 1_240_000);
        assert_eq!(reg.gauge(ids.families[0][8]).get(), 318_000);
        assert_eq!(reg.gauge(ids.families[0][9]).get(), 640);
        // Families the member does not configure stay at zero.
        assert_eq!(reg.gauge(ids.families[5][3]).get(), 0);
        assert_eq!(reg.gauge(ids.families[5][8]).get(), 0, "no denominator, no reading");
        // A second mirror with the SAME cumulative counters adds
        // nothing: the block publishes deltas, not the totals.
        mirror_bin15_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.reprices).get(), 7, "a delta, not a total");
        f.c.reprices = 10;
        mirror_bin15_metrics(&reg, &ids, &f, &mut last);
        assert_eq!(reg.counter(ids.reprices).get(), 10);
    }

    /// The 2026-09-15 hang, as a test.
    ///
    /// A boot abort joins ingress threads that nobody asked to stop.
    /// Every real ingress loop polls `SHUTDOWN`, so this spawns a
    /// thread with exactly that shape: it spins until the flag is set.
    /// Before the fix, `join_reverse` blocked on it forever and the
    /// process never reached exit — which is why launchd could not
    /// relaunch the engine after Deribit recovered.
    ///
    /// **TERMINATION IS THE ASSERTION.** The threads below exit only
    /// when `SHUTDOWN` is set, and nothing but `join_reverse` sets it —
    /// so if this test returns at all, the fix works. A regression
    /// HANGS rather than fails, which the runner reports as a timeout:
    /// the honest signal for "the process would not die".
    ///
    /// Deliberately NOT asserted: the value of `SHUTDOWN` afterwards.
    /// It is a process-wide static that several tests in this binary
    /// flip, so reading it back would make this test order-dependent
    /// and tell us nothing the threads' own exit has not already
    /// proved.
    #[test]
    fn join_reverse_stops_threads_that_were_never_told_to_stop() {
        use std::sync::atomic::AtomicUsize;

        // Establish the precondition this test needs, whatever any
        // sibling left behind: nobody has signalled shutdown yet.
        crate::SHUTDOWN.store(false, Ordering::Release);

        static SPINS: AtomicUsize = AtomicUsize::new(0);
        SPINS.store(0, Ordering::Release);

        let mut handles = Vec::new();
        for i in 0..3 {
            handles.push(
                thread::Builder::new()
                    .name(format!("fake-ingress-{i}"))
                    .spawn(|| {
                        // An ingress run-loop in miniature.
                        while !crate::shutdown_requested() {
                            SPINS.fetch_add(1, Ordering::Relaxed);
                            thread::sleep(Duration::from_millis(1));
                        }
                    })
                    .expect("spawn"),
            );
        }
        // Let them actually get going, so the join has something to
        // wait for rather than racing a thread that never started.
        while SPINS.load(Ordering::Acquire) < 3 {
            thread::sleep(Duration::from_millis(1));
        }

        // Nobody has signalled shutdown. Before the fix this never
        // returned.
        // Nobody has signalled shutdown. Before the fix, this call
        // never returned.
        join_reverse(handles);

        // Reaching this line is the whole proof. The count is checked
        // only to be sure the threads really ran, so that a future
        // edit cannot make this pass by spawning nothing.
        assert!(
            SPINS.load(Ordering::Acquire) >= 3,
            "the fake ingress threads never ran, so nothing was proved"
        );
    }

    /// The empty case must NOT touch the process-wide flag — a boot
    /// that spawned nothing has nothing to stop, and mutating a global
    /// on the way past would be a surprise.
    #[test]
    fn join_reverse_with_no_handles_leaves_the_shutdown_flag_alone() {
        crate::SHUTDOWN.store(false, Ordering::Release);
        join_reverse(Vec::new());
        assert!(
            !crate::shutdown_requested(),
            "a boot that spawned nothing has nothing to stop"
        );
        crate::SHUTDOWN.store(false, Ordering::Release);
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
                100_000_000_000_000i64,
                1_774_598_400_000i64,
                opt_registry::RIGHT_CALL,
            ),
            (
                "BTC-27MAR26-100000-P".to_string(),
                make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 2),
                100_000_000_000_000i64,
                1_774_598_400_000i64,
                opt_registry::RIGHT_PUT,
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
        let mut big: Vec<DiscoveredOption> = Vec::new();
        for i in 0..ingress_deribit::DERIBIT_OPT_MAX {
            big.push((
                format!("X{i}-C"),
                make_symbol_id(VenueId::Deribit, OPT_ORDINAL_BASE + 100 + i as u32),
                100_000_000_000_000i64,
                1_774_598_400_000i64,
                opt_registry::RIGHT_CALL,
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
        assert_eq!(t[VenueId::Mexc as usize], 400, "MX9 measured p99, rounded up");
    }

    /// VT2: overrides replace only the named venue; the last spec for
    /// a venue wins; `0` is a legal "measure only" value.
    #[test]
    fn stale_after_ms_overrides_named_venues_only() {
        let specs = [
            "okx:250".to_owned(),
            "bn:0".to_owned(),
            "okx:300".to_owned(),
            "mexc:400".to_owned(),
        ];
        let t = parse_stale_after_ms(&specs).unwrap();
        assert_eq!(t[VenueId::Okx as usize], 300);
        assert_eq!(t[VenueId::Binance as usize], 0);
        // MX2: the D8 lever — a too-coarse MEXC default is overridden
        // here, not by a per-class table.
        assert_eq!(t[VenueId::Mexc as usize], 400);
        assert_eq!(
            t[VenueId::Bybit as usize],
            500,
            "untouched venue keeps its default"
        );
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
        for c in [
            cfg.pm,
            cfg.bn,
            cfg.okx,
            cfg.rpc,
            cfg.deribit,
            cfg.hl,
            cfg.bybit,
            cfg.mexc,
            cfg.hyperevm,
            cfg.hypercall,
        ] {
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
        for c in [
            cfg.bn,
            cfg.rpc,
            cfg.deribit,
            cfg.hl,
            cfg.bybit,
            cfg.mexc,
            cfg.hyperevm,
            cfg.hypercall,
        ] {
            assert_eq!(c.mode, TapMode::Off);
            assert_eq!(c.budget_bytes, 0);
        }
    }

    /// Every known capture-venue label is accepted in one CSV.
    #[test]
    fn raw_tap_flags_every_known_venue_label_accepted() {
        let cfg = parse_raw_tap_flags(
            Some("pm,bn,okx,rpc,deribit,hl,bybit,mexc,hyperevm,hypercall"),
            "all",
            1,
        )
        .unwrap();
        for c in [
            cfg.pm,
            cfg.bn,
            cfg.okx,
            cfg.rpc,
            cfg.deribit,
            cfg.hl,
            cfg.bybit,
            cfg.mexc,
            cfg.hyperevm,
            cfg.hypercall,
        ] {
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

    /// More than ten comma-separated labels trips the defensive
    /// capacity guard — there are only ten capture labels (WS9 added
    /// bybit, MX6 mexc, HYPARB H3b hyperevm, HC5 hypercall), so this
    /// branch is a pure defense-in-depth backstop reached here by
    /// listing all ten plus an eleventh item.
    #[test]
    fn raw_tap_flags_rejects_more_labels_than_known_venues() {
        assert_eq!(
            parse_raw_tap_flags(
                Some("pm,bn,okx,rpc,deribit,hl,bybit,mexc,hyperevm,hypercall,pm2"),
                "rejects",
                64
            )
            .err(),
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
            regime_blocked: 100,
            regime_hard_exits: 100,
        };
        mirror_vm_metrics(&reg, &ids, &Bare, &mut stale);
        assert_eq!(reg.counter(ids.fires).get(), 0, "regression saturates");
        assert_eq!(stale.fires, 0, "snapshot re-bases to the source");
    }

    /// RG8: `[labels] require = 1` refuses an ENABLED signal-carrying coded
    /// member whose label is ANY; ai-exec and the vm are exempt (their
    /// labels live elsewhere); labelled members pass; `require = 0` is
    /// advisory.
    #[test]
    fn require_labels_refuses_only_enabled_unlabelled_signal_members() {
        use core_types::regime::RegimeLabelBuilder;
        let mut set = strategy_set::StrategySet::new(strategy_set::BUILT_MASK);
        // Off: never a refusal.
        assert_eq!(unlabelled_required_slot(&set, strategy_set::BUILT_MASK, false), None);
        // On, every coded member ANY: the first enabled required slot names the refusal.
        assert_eq!(
            unlabelled_required_slot(&set, strategy_set::BUILT_MASK, true),
            Some(strategy_set::SLOT_HYPARB)
        );
        // The AI-only mask (ai-exec + vm) is exempt — nothing to label at boot.
        let ai = strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM;
        assert_eq!(unlabelled_required_slot(&set, ai, true), None);
        let mut b = RegimeLabelBuilder::new();
        b.add(b"fast:shape:trend").unwrap();
        let label = core_types::RegimeLabelSet::from_terms(&[b.finish()], core_types::REGIME_OFF_SOFT)
            .unwrap();
        // ai+bin15: bin15 is a signal member ⇒ refused until labelled.
        let ai_bin15 = ai | strategy_set::BIT_BIN15;
        assert_eq!(
            unlabelled_required_slot(&set, ai_bin15, true),
            Some(strategy_set::SLOT_BIN15)
        );
        assert!(set.set_regime_label(strategy_set::SLOT_BIN15, label));
        assert_eq!(unlabelled_required_slot(&set, ai_bin15, true), None);
        // XMM XH3: ai+xmm — xmm carries its own signal (the Binance lead),
        // so with the law on it is refused until labelled; its label is
        // carried (never consulted — the HORIZON law) and satisfies it.
        let ai_xmm = ai | strategy_set::BIT_XMM;
        assert_eq!(
            unlabelled_required_slot(&set, ai_xmm, true),
            Some(strategy_set::SLOT_XMM)
        );
        assert!(set.set_regime_label(strategy_set::SLOT_XMM, label));
        assert_eq!(unlabelled_required_slot(&set, ai_xmm, true), None);
    }

    /// Boot-surface pin: `Observability::build(true)` registers
    /// every §9 row under its verbatim design name (plus the AI-side
    /// `table_push_fail`), and the registry encodes them.
    #[test]
    fn observability_build_registers_section9_rows() {
        let obs = Observability::build(true, None).unwrap();
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
            "engine_vm_regime_blocked_total",
            "engine_vm_regime_hard_exits_total",
            "engine_ai_table_push_fail_total",
        ] {
            assert!(text.contains(name), "missing §9 row {name}");
        }
    }

    /// E1, the metric half of the acceptance gate: with NO `--exec`
    /// the registry must not grow a single name. `/metrics` from a
    /// post-E1 binary on an unconfigured boot is byte-identical to a
    /// pre-E1 binary's.
    #[test]
    fn no_exec_artifact_registers_no_exec_metrics() {
        let obs = Observability::build(true, None).unwrap();
        let reg = obs.metrics.as_ref().unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let n = reg.encode_prometheus(&mut buf).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            !text.contains("engine_exec_"),
            "an unconfigured boot must register NO engine_exec_* name"
        );
    }

    /// With an artifact present, the global family registers — and the
    /// per-slot family registers for LIVE slots ONLY (plan §3.5).
    #[test]
    fn an_exec_artifact_registers_the_global_family_and_only_live_slots() {
        // slot 3 live, slot 6 off, everything else paper.
        let mut modes = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        modes[3] = 1;
        modes[6] = 2;
        let obs = Observability::build(true, Some(modes)).unwrap();
        let reg = obs.metrics.as_ref().unwrap();
        let mut buf = vec![0u8; 256 * 1024];
        let n = reg.encode_prometheus(&mut buf).unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        for name in [
            "engine_exec_configured",
            "engine_exec_live_submits_total",
            "engine_exec_paper_submits_total",
            "engine_exec_refused_off_total",
            "engine_exec_refused_no_route_total",
            "engine_exec_refused_risk_total",
            // E6 commit 4 — the halt family.
            "engine_exec_refused_halted_total",
            "engine_exec_refused_unseeded_total",
            "engine_exec_halts_total",
            "engine_exec_cancel_all_failures_total",
            "engine_exec_cancel_all_stranded_total",
            "engine_exec_seeded",
            // E7 session bound — the two gauges the operator watches.
            "engine_exec_hl_pnl_anchor_usd_1e6",
            "engine_exec_hl_session_pnl_usd_1e6",
            "engine_exec_slot3_mode",
            "engine_exec_slot3_live_submits_total",
            "engine_exec_slot3_refused_total",
            "engine_exec_slot3_halted",
        ] {
            assert!(text.contains(name), "missing exec row {name}");
        }
        // Paper and OFF slots cost no names.
        for absent in [
            "engine_exec_slot0_mode",
            "engine_exec_slot1_mode",
            "engine_exec_slot6_mode",
            "engine_exec_slot6_refused_total",
            "engine_exec_slot6_halted",
        ] {
            assert!(!text.contains(absent), "must NOT register {absent}");
        }
    }

    /// Every E1 metric name fits `core_metrics::NAME_MAX`.
    #[test]
    fn every_exec_metric_name_fits_the_registry() {
        for slot in 0..clob_dispatcher::EXEC_COUNTER_SLOTS {
            for n in [
                format!("engine_exec_slot{slot}_mode"),
                format!("engine_exec_slot{slot}_live_submits_total"),
                format!("engine_exec_slot{slot}_refused_total"),
                format!("engine_exec_slot{slot}_halted"),
            ] {
                assert!(n.len() <= core_metrics::NAME_MAX, "{n} is {} bytes", n.len());
            }
        }
        for n in [
            "engine_exec_configured",
            "engine_exec_live_submits_total",
            "engine_exec_paper_submits_total",
            "engine_exec_refused_off_total",
            "engine_exec_refused_no_route_total",
            "engine_exec_refused_risk_total",
            "engine_exec_refused_halted_total",
            "engine_exec_refused_unseeded_total",
            "engine_exec_halts_total",
            "engine_exec_cancel_all_failures_total",
            "engine_exec_cancel_all_stranded_total",
            "engine_exec_seeded",
            "engine_exec_hl_pnl_anchor_usd_1e6",
            "engine_exec_hl_session_pnl_usd_1e6",
        ] {
            assert!(n.len() <= core_metrics::NAME_MAX);
        }
    }
}
