// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX7b — the Binance Demo battery** (`exec-smoke --binance <um|cm|um,cm>
//! --lifecycle`; the build plan `claude/bx7b-build-plan-2026-10-08.md`
//! §3.4): the engine's arm, router and gateway — `RoutedDispatcher` →
//! `BnArm` → the command ring → `BnGateway` on its own thread → fill lane 4
//! and the event ring → the router's ledger — on Binance **Demo**'s hosts
//! with the Demo keys, driven through `OrderDispatch` as the engine loop
//! drives it (the idle hook, then the fill lane).
//!
//! COPY-DOCTRINE: an operator tool run by hand against Binance Demo, never
//! reachable from the engine loop: it allocates and copies freely (the
//! report, the tap's frames, the journal's records, the reads' answers).
//!
//! **Where it departs from the engine's composition** (`bn_live`): no split
//! layers (no `SlotSplit`, no `VenueSplit`: the arm sits behind the router
//! directly), no alias check, no journal writer thread (the journal ring is
//! drained here — every venue answer the gateway journals is the battery's
//! evidence), no shutdown grace, no thread pinning; the Demo scope
//! (`Scope::Demo`: classic by construction, no SAPI reads — gateway
//! "Demo"); no `exec.toml` (the knobs below), slot [`SLOT`]; no anchor file
//! and no session bound (the battery is not a session); a halt latched in
//! memory only (no `exec.HALT`).
//!
//! **The router is here.** Its four clamps bound every order
//! (`max_order_usd`, `cap_instance_usd`, `cap_day_usd`, `max_open_orders`)
//! and its kill switches are `exec.toml.example`'s ([`HALT_LIMITS`]); the
//! arm quantizes, governs and bands each order against the mark the
//! battery hands it (`premiumIndex`, as the mark lane would; the touch read
//! inside the PERCENT_PRICE band around it). Growth — every place and
//! modify — stops at the first failure, a signal, a halt, the arm's halt
//! word, any reconciliation drift, or a full record ([`Battery::may_grow`]);
//! a reduce-only close stops only at a signal or a halt.
//!
//! **The run.**
//! 0. *The account* (cold reads, `bn_demo::read_account` and
//!    `bn_demo::open_orders` — the gateway's scanners): flat on every
//!    symbol (a dedicated account: the boot adopts positions on bound rows
//!    and leaves the slot unreconciled over any other), no order but the
//!    arm's own (`cid::classify` reads it ours or an orphan the boot
//!    sweeps), and what BX-19 asserts: one-way, single-asset (USDⓈ-M), each
//!    symbol CROSSED at a leverage within 20 — else [`EXIT_ACCOUNT`].
//! 1. *The boot*: the gateway's own (the clock, BX-19, the listenKey, the
//!    WS API logon or the REST route, the first reconciliation with its
//!    orphan sweep, the day); then the router seeds the slot.
//! 2. *Maker cycles* (USDⓈ-M; the side alternates): a post-only place
//!    (listed under our id), a modify of the price, one of the size (each
//!    listed), a cancel through the verb (`CANCELED_MEMBER`); once, a
//!    post-only order that would cross (refused, never a fill) and a maker
//!    with a short TTL (the gateway's `CANCELED_TTL`).
//! 3. *Fills* (each product): an IoC through the touch, filled and booked
//!    on lane 4, the ledger's position moved; then a reduce-only IoC
//!    closing it — each leg proven by a reconciliation journaled after its
//!    end that agrees. COIN-M first refuses a maker locally (O-BX3a).
//! 4. *The halt's sweep* (USDⓈ-M, last — a halted slot refuses every order
//!    after it): two quotes, then an operator halt of the slot; the router
//!    asks the arm to cancel the slot: `Working`, then `Clear`, the venue
//!    listing neither.
//! 5. *The end* — always, after a failure or a signal too: the positions
//!    the ledger holds closed (not after a signal, not once halted), the
//!    shutdown sweep, and the account read flat on the run's symbols with
//!    no order of this run's epoch — else [`EXIT_LEFTOVER`], the loudest.
//!
//! **The bar** is the plan's, absolute ([`bar_short`]): a clean run short
//! of it exits with the short phase's code, never 0.
//!
//! **Signals.** Before the boot nothing of the battery's is on the account:
//! a signal ends it there. After it, every wait and every growth reads the
//! signal (a cancel still goes, once), and the end fits the drain alarm
//! (`sigint::DRAIN_DEADLINE_S`, asserted below): the request in flight, the
//! arm's shutdown spin, the gateway's [`SIGNAL_GW_WAIT`], its stop, the
//! tap's join, and each lane's two flat reads, the lanes in parallel; K2
//! and the tap's lines are skipped. Outside the bound: core-net's blocking
//! DNS lookup after a third failed dial. A second signal kills at once; the
//! USDⓈ-M dead-man then cancels the makers within 30 s.
//!
//! **The tap (K2).** A second, read-only socket on the USDⓈ-M user stream
//! (the account's one listenKey; it signs nothing) records each frame with
//! its local receipt time, on its own thread, within [`TAP_FRAMES_MAX`] and
//! [`TAP_BYTES_MAX`]. Per IoC the report orders the ACK (the gateway's
//! `J_ACK`) against `TRADE_LITE` and the `ORDER_TRADE_UPDATE` trade:
//! evidence across two sockets, not proof.
//!
//! Exit codes: `bn_demo::EXIT_*` (50–58), pinned there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clob_dispatcher::{
    BinanceArmReport, CancelAllState, DispatchError, OrderDispatch, PaperDispatcher, RETIRED_CANCELED_MEMBER,
    RETIRED_CANCELED_TTL, RETIRED_EXPIRED, RETIRED_FILLED, RETIRED_REJECTED,
};
use core_config::universe::Instrument;
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_ring::{Consumer, Ring};
use core_types::{
    make_symbol_id, CancelReq, ChannelEvent, ChannelId, ExecRecord, Fill, ModifyReq, Order, Price, Qty, Side, SymbolId,
    VenueId, ORDER_FLAG_POST_ONLY, ORDER_FLAG_REDUCE_ONLY, SYMBOL_ID_NONE,
};
use exec_binance::arm::{
    ArmKnobs, BnArm, Refusal, GW_CONNECT_FAILURES, GW_FILLS_LOST, GW_JOURNAL_DROPPED, GW_RECONNECTS, GW_RECON_FAILED,
    GW_RECON_OK, SHUTDOWN_WAIT_NS,
};
use exec_binance::cid::{CidPrefix, CID_LEN};
use exec_binance::cmd::{duplex, EVT_F_SWEEP_DONE};
use exec_binance::config::{BnConfig, Signer};
use exec_binance::gateway::{BnGateway, GwKnobs, Phase, QUIET_NS, ROTATE_AFTER_NS};
use exec_binance::inst::{BnInstCold, BnInstHot, PRODUCT_COINM, PRODUCT_USDM};
use exec_binance::journal::{JournalTx, JOURNAL_RING, J_ACK, J_ENDED, J_FEE, J_FILL, J_RECON, J_REJECT};
use exec_binance::margin::{MarginBook, SLOTS};
use exec_binance::mode::{judge_one_way, judge_single_asset, judge_symbol_config, AccountMode};
use exec_binance::recon::MARGIN_CROSS;
use exec_binance::rest::{
    render_account, render_by_cid, scan_listen_key, R_ACCOUNT, R_CANCEL, R_STATUS, T_DUAL, T_LISTEN_KEY_NEW,
    T_MULTI_ASSETS, T_OPEN_ORDERS, T_SYMBOL_CONFIG,
};
use exec_binance::userstream::{scan_user_event, UserEvent, S_CANCELED, UE_ORDER, UE_TRADE_LITE, X_TRADE};
use exec_binance::wsapi::{Part, WsStatic};
use exec_router::{ExecMode, ExecRoute, HaltLimits, HaltReason, RoutedDispatcher, SlotCaps};
use ingress_binance::discovery::BnDiscovery;
use rustls::ClientConfig;
use tracing::{error, info, warn};

use crate::bn_demo::{
    account_of, demo_config, discover, is_arms, open_orders, order_state, orders_of, product_word, px_off, qty_for,
    read_account, wall_ms, Line, Public, Rest, Ws, EXIT_ACCOUNT, EXIT_BOOT, EXIT_CYCLE, EXIT_DRILL, EXIT_FILLS,
    EXIT_LEFTOVER, EXIT_POST_ONLY, EXIT_RECON, EXIT_SWEEP, EXIT_TTL, ORD0, RECV_WINDOW_MS, STP_MODE,
};
use crate::bn_live::{bind_table, BindSet};
use crate::bn_ops::{flatten, OpsSpec, Reach, DEFAULT_LEVERAGE_CAP};

/// The slot the battery's orders carry (the arm's ids name it, E-9): the
/// slot the loopback rig uses; Demo is the battery's alone.
pub const SLOT: u8 = 2;
/// The engine's fill-lane size (lane 4 as the engine builds it).
const FILL_N: usize = engine::FILL_RING_SIZE;
/// The highest initial leverage BX-19 accepts on Demo (the engine's
/// default `max_leverage`).
const MAX_LEVERAGE: u8 = 20;
/// The router's clamps on the battery's slot, USD ×1e6: one order, the net
/// exposure, the day's buying (the venue's day, adopted at boot, includes
/// earlier runs'), and the orders working.
const CAP_ORDER_USD_1E6: i64 = 600_000_000;
const CAP_INSTANCE_USD_1E6: i64 = 2_000_000_000;
const CAP_DAY_USD_1E6: i64 = 100_000_000_000;
const MAX_OPEN_ORDERS: u32 = 16;
/// The request's notional ceiling, USD (`--bn-usd`): a typo limit. A row's
/// minimum notional can raise an order past it; the router's
/// `CAP_ORDER_USD_1E6` clamp (600 USD) is the order's real ceiling.
pub const MAX_USD: i64 = 200;
/// The router's kill switches on the slot, `exec.toml.example`'s: 5 venue
/// refusals in a row, 5 USD of reconciliation drift, 30 s of a silent
/// order route, 3 local asset refusals in a row, 300 s without an agreeing
/// reconciliation. The battery's own gate is stricter on drift: any stops
/// growth ([`Battery::may_grow`]).
const HALT_LIMITS: HaltLimits = HaltLimits::new(5, 5_000_000, 30_000, 3, 300_000);
/// The arm's shortest maker TTL — BX6 (h)'s floor, as the engine's.
const MIN_MAKER_TTL_NS: u64 = 5_000_000_000;
/// The TTL phase's bounds, ms (the floor is the arm's).
const TTL_MIN_MS: u64 = MIN_MAKER_TTL_NS / 1_000_000;
const TTL_MAX_MS: u64 = 20_000;
/// Every quote but the TTL phase's carries this gateway TTL: a battery bug
/// strands nothing past it.
const SAFETY_TTL_NS: u64 = 30_000_000_000;
/// A modify's move, bps (further behind the touch).
const MOVE_BPS: i64 = 30;
/// The reconciliation cadence the battery asks of the gateway (the
/// `recon_every_ms` floor `exec.toml` allows).
const RECON_EVERY_MS: u64 = 10_000;
/// How long a verb's answer may take, and a reconciliation (two cycles and
/// room).
const STEP_WAIT: Duration = Duration::from_secs(15);
const RECON_WAIT: Duration = Duration::from_secs(35);
/// The boot's budget (the gateway's own; it reads no signal), and the
/// slot's seeding after it.
const BOOT_TIMEOUT_NS: u64 = 20_000_000_000;
const SEED_WAIT: Duration = Duration::from_secs(30);
/// The end's wait for the gateway thread to leave after the shutdown
/// sweep; after a signal, the drain's share of it; then, stopped, its exit.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
const SIGNAL_GW_WAIT: Duration = Duration::from_secs(5);
const GW_STOP_WAIT: Duration = Duration::from_secs(1);
/// The flat reads' `recvWindow`, ms: both of a lane's reads are signed
/// before the first is sent (the signer stays on the main thread), so the
/// second may wait out the first's whole deadline — reads, which the venue
/// allows up to 60 s.
const FLAT_RECV_WINDOW_MS: u64 = 11_000;
/// The tap's read slice, and the end's wait for its thread.
const TAP_SLICE: Duration = Duration::from_millis(100);
const TAP_JOIN: Duration = Duration::from_secs(1);
/// What the tap keeps at most; past either, a frame is counted, not kept.
const TAP_FRAMES_MAX: usize = 50_000;
const TAP_BYTES_MAX: usize = 64 * 1_024 * 1_024;
/// The most journal records and fills the battery keeps; past either, the
/// evidence is incomplete and growth stops (a failure, never a pass).
const RECS_MAX: usize = 65_536;
const FILLS_MAX: usize = 16_384;
/// Tries to close a position (each through a fresh touch, wider).
const CLOSE_TRIES: u32 = 3;
/// The venue's REST listing can trail its WS API answer: a listing check
/// reads up to this many times, [`LISTING_GAP`] apart.
const LISTING_READS: u32 = 10;
const LISTING_GAP: Duration = Duration::from_millis(300);
/// The plan's bar (BX7b §3.4): USDⓈ-M's maker cycles, fills and agreeing
/// reconciliations; COIN-M's fills and reconciliations.
const BAR_CYCLES: u32 = 10;
const BAR_FILLS_UM: u32 = 6;
const BAR_RECONS_UM: u32 = 3;
const BAR_FILLS_CM: u32 = 4;
const BAR_RECONS_CM: u32 = 2;
/// What a step that stopped for a signal says.
const STOPPED: &str = "stopped by a signal";

// The drain bound (module docs, Signals): from a signal, the request in
// flight, the arm's shutdown spin, the gateway's share and its stop, the
// tap's join, and the flat reads (each lane's two, the lanes in parallel)
// end inside the drain alarm; so does a boot the signal lands on.
const _: () = {
    let req = core_net::https_post::REQ_DEADLINE.as_millis();
    let arm = (SHUTDOWN_WAIT_NS / 1_000_000) as u128;
    let end = req + arm + SIGNAL_GW_WAIT.as_millis() + GW_STOP_WAIT.as_millis() + TAP_JOIN.as_millis() + 2 * req;
    let drain = crate::sigint::DRAIN_DEADLINE_S as u128 * 1_000;
    assert!(end < drain);
    assert!((BOOT_TIMEOUT_NS / 1_000_000) as u128 + req < drain);
    assert!(SIGNAL_GW_WAIT.as_millis() <= SHUTDOWN_WAIT.as_millis());
    assert!(TAP_SLICE.as_millis() < TAP_JOIN.as_millis());
    assert!(FLAT_RECV_WINDOW_MS as u128 >= 2 * req + 1_000 && FLAT_RECV_WINDOW_MS <= 60_000);
};

/// What the operator asked for (`exec-smoke --binance …`).
#[derive(Clone, Debug)]
pub struct BatterySpec {
    /// USDⓈ-M armed.
    pub um: bool,
    /// COIN-M armed (IoC-only, O-BX3a).
    pub cm: bool,
    /// The REST order route (`order_transport = "rest"`) instead of the WS
    /// API.
    pub rest: bool,
    /// The HMAC key (REST only, R22).
    pub hmac: bool,
    /// The USDⓈ-M symbol.
    pub um_symbol: String,
    /// The COIN-M symbol.
    pub cm_symbol: String,
    /// Maker cycles (USDⓈ-M); the bar is 10.
    pub cycles: u32,
    /// IoC round trips per product; the bar is 3 (USDⓈ-M) and 2 (COIN-M).
    pub fill_pairs: u32,
    /// Each order's requested notional, USD (raised to the row's minimum;
    /// at most [`MAX_USD`]).
    pub usd: i64,
    /// A resting quote's distance behind the touch, bps.
    pub rest_bps: i64,
    /// An IoC's distance through the touch, bps.
    pub cross_bps: i64,
    /// The TTL phase's lifetime, ms (at least the arm's 5 s floor).
    pub ttl_ms: u64,
    /// The order session's planned replacement after this many seconds
    /// (make-before-break on the WS API); 0 = the production 23 h.
    pub rotate_s: u64,
    /// BX11: a drill instead of the phases (`--bn-drill`).
    pub drill: Option<Drill>,
}

/// **BX11 — the battery's drills** (`--bn-drill`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Drill {
    /// The gateway dies under resting makers: the venue's dead-man must
    /// cancel them (the BX13 R0 bar "the dead-man fired", on Demo).
    Gap,
    /// The operator's flatten on a real position (O-BX9): the arm opens one
    /// with an IoC through the touch, its gateway stops, and
    /// `bn_ops::flatten` must close it — its verdict a fresh read.
    Flatten,
}

/// The gateway's dead-man countdown on Demo (its heartbeat renews it).
const COUNTDOWN_MS: u64 = 30_000;
/// The gateway's heartbeat on Demo: the countdown's renewal cadence.
const HEARTBEAT_MS: u64 = 10_000;
/// How long past the countdown the gap drill waits for the venue.
const DRILL_MARGIN: Duration = Duration::from_secs(10);

impl BatterySpec {
    /// The spec's own refusals, before anything is read.
    ///
    /// # Errors
    /// What the operator must change.
    pub fn check(&self) -> Result<(), String> {
        if !self.um && !self.cm {
            return Err(String::from("--binance names no product (um, cm or um,cm)"));
        }
        if self.hmac && !self.rest {
            return Err(String::from("--hmac needs --rest: HMAC signs REST only (the WS API logon takes Ed25519)"));
        }
        if !(1..=MAX_USD).contains(&self.usd) {
            return Err(format!("--bn-usd must be 1..={MAX_USD}"));
        }
        if !(50..=450).contains(&self.rest_bps) {
            return Err(String::from("--bn-rest-bps must be 50..=450 (inside the venue's 5 % band)"));
        }
        if !(1..=300).contains(&self.cross_bps) {
            return Err(String::from("--bn-cross-bps must be 1..=300"));
        }
        if !(TTL_MIN_MS..=TTL_MAX_MS).contains(&self.ttl_ms) {
            return Err(format!("--bn-ttl-ms must be {TTL_MIN_MS}..={TTL_MAX_MS} (the arm's shortest maker TTL is 5 s)"));
        }
        if self.drill == Some(Drill::Gap) && (!self.um || self.cm || self.rest) {
            return Err(String::from(
                "--bn-drill gap is USDⓈ-M's on the WS API (`--binance um`): the dead-man rests makers there only",
            ));
        }
        if self.drill == Some(Drill::Flatten) && self.um == self.cm {
            return Err(String::from("--bn-drill flatten takes one product (`--binance um` or `--binance cm`)"));
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------
// The report
// -------------------------------------------------------------------------

/// The run's verdicts: the first failure's code (a leftover overrides it),
/// what the bar counts, and what the END line reports besides.
#[derive(Default)]
struct Report {
    code: u8,
    why: String,
    left: Vec<String>,
    cycles: u32,
    fills: [u32; 3],
    recons: [u32; 3],
    sweeps: u32,
    ttl: u32,
    post_only: u32,
    cm_maker_refused: bool,
    k2: Vec<String>,
    /// The gateway's tallies at the end (the arm's mirror): stream and
    /// session reconnects — the rotation's evidence — connect failures,
    /// reconciliations ok and failed, journal records and fills lost.
    reconnects: u64,
    connect_failures: u64,
    recon_ok: u64,
    recon_failed: u64,
    journal_dropped: u64,
    fills_lost: u64,
    /// The tap's frames kept and dropped; the battery's records and fills
    /// dropped past their caps.
    tap_frames: u64,
    tap_dropped: u64,
    recs_dropped: u64,
    fills_dropped: u64,
    /// BX11: the gap drill's verdict — the venue's dead-man cancelled the
    /// makers this long after the gateway died (ms).
    drill_fired_ms: Option<u64>,
    /// BX11: the run's `J_FILL` and `J_FEE` records (one fee per trade).
    fills_journaled: u64,
    /// See [`Self::fills_journaled`].
    fees_journaled: u64,
}

impl Report {
    fn fail(&mut self, code: u8, why: String) {
        error!(code, "binance-battery: {why}");
        if self.code == 0 {
            self.code = code;
            self.why = why;
        }
    }

    fn exit(&self) -> u8 {
        if self.left.is_empty() {
            self.code
        } else {
            EXIT_LEFTOVER
        }
    }
}

/// **The plan's bar, absolute** (BX7b §3.4) — whatever the spec asked: the
/// first short count's phase code and why, `None` when met. USDⓈ-M: 10
/// maker cycles, a post-only refusal, a TTL cancel, 6 fills, 3 agreeing
/// reconciliations, a halt's sweep. COIN-M: the maker refused, 4 fills, 2
/// reconciliations.
fn bar_short(spec: &BatterySpec, rep: &Report) -> Option<(u8, String)> {
    // A drill is its own bar: its verdict is the run's code.
    if spec.drill.is_some() {
        return None;
    }
    let (um, cm) = (PRODUCT_USDM as usize, PRODUCT_COINM as usize);
    let need: [(bool, u32, u32, u8, &str); 9] = [
        (spec.um, rep.cycles, BAR_CYCLES, EXIT_CYCLE, "maker cycles"),
        (spec.um, rep.post_only, 1, EXIT_POST_ONLY, "post-only refusals"),
        (spec.um, rep.ttl, 1, EXIT_TTL, "TTL cancels"),
        (spec.um, rep.fills[um], BAR_FILLS_UM, EXIT_FILLS, "USDⓈ-M fills"),
        (spec.um, rep.recons[um], BAR_RECONS_UM, EXIT_RECON, "USDⓈ-M agreeing reconciliations"),
        (spec.um, rep.sweeps, 1, EXIT_SWEEP, "halt sweeps"),
        (spec.cm, u32::from(rep.cm_maker_refused), 1, EXIT_FILLS, "COIN-M maker refusals"),
        (spec.cm, rep.fills[cm], BAR_FILLS_CM, EXIT_FILLS, "COIN-M fills"),
        (spec.cm, rep.recons[cm], BAR_RECONS_CM, EXIT_RECON, "COIN-M agreeing reconciliations"),
    ];
    let mut i = 0;
    while i < need.len() {
        let (on, have, want, code, what) = need[i];
        if on && have < want {
            return Some((code, format!("short of the bar: {have} {what} of {want}")));
        }
        i += 1;
    }
    None
}

/// One JSON line on stdout: `{"phase":…, …}` (the exec-smoke law).
fn phase(name: &str) -> Line {
    Line::new("phase", name)
}

/// A count for a report line.
fn count(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A failure: the code and why.
type Fail = (u8, String);

// -------------------------------------------------------------------------
// The tap
// -------------------------------------------------------------------------

/// What the tap kept: each frame with its receipt time, within
/// [`TAP_FRAMES_MAX`] and [`TAP_BYTES_MAX`]; past either, counted only.
#[derive(Default)]
struct TapLog {
    frames: Vec<(u64, Vec<u8>)>,
    bytes: usize,
    dropped: u64,
}

impl TapLog {
    fn keep(&mut self, t: u64, frame: Vec<u8>) {
        if self.frames.len() < TAP_FRAMES_MAX && self.bytes + frame.len() <= TAP_BYTES_MAX {
            self.bytes += frame.len();
            self.frames.push((t, frame));
        } else {
            self.dropped += 1;
        }
    }
}

/// The read-only user-stream socket (K2), on its own thread.
struct Tap {
    log: Arc<Mutex<TapLog>>,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Option<String>>,
}

impl Tap {
    fn spawn(host: &str, path: String, tls: Arc<ClientConfig>) -> Result<Self, String> {
        let mut ws = Ws::open(host, &path, tls, Duration::from_secs(10)).map_err(|e| format!("the tap: {e}"))?;
        let log = Arc::new(Mutex::new(TapLog { frames: Vec::with_capacity(4_096), ..TapLog::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let (kept, s) = (Arc::clone(&log), Arc::clone(&stop));
        let handle = std::thread::Builder::new()
            .name(String::from("bn-tap"))
            .spawn(move || {
                while !s.load(Ordering::Acquire) {
                    match ws.next(TAP_SLICE) {
                        Ok(Some((t, f))) => {
                            if let Ok(mut v) = kept.lock() {
                                v.keep(t, f);
                            }
                        }
                        Ok(None) => {}
                        Err(e) => return Some(e),
                    }
                }
                None
            })
            .map_err(|e| format!("the tap's thread: {e}"))?;
        Ok(Self { log, stop, handle })
    }

    /// Stop it — its thread waited for at most [`TAP_JOIN`], else left —
    /// and take what it kept; why it ended early, if it did.
    fn finish(self) -> (TapLog, Option<String>) {
        self.stop.store(true, Ordering::Release);
        let t0 = Instant::now();
        while !self.handle.is_finished() && t0.elapsed() < TAP_JOIN {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ended = if self.handle.is_finished() {
            self.handle.join().unwrap_or_else(|_| Some(String::from("the tap's thread panicked")))
        } else {
            Some(String::from("the tap's thread did not stop in time (left)"))
        };
        let log = self.log.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
        (log, ended)
    }
}

// -------------------------------------------------------------------------
// The battery
// -------------------------------------------------------------------------

/// One armed product's half.
struct LaneB {
    product: u8,
    symbol: String,
    sym: SymbolId,
    rest: Rest,
    public: Public,
    /// The arm's row (tick, step, minimums, decimals), read once bound.
    h: BnInstHot,
    /// Its PERCENT_PRICE band (a touch is judged against it).
    c: BnInstCold,
}

/// The gateway thread's end: its phase and its last sweep's flag.
type GwEnd = (Phase, u8);

struct Battery {
    spec: BatterySpec,
    /// The key's configuration: held, never read again — the seed lives as
    /// long as anything signs (`bn_live`'s law).
    _cfg: BnConfig,
    signer: Signer,
    lanes: Vec<LaneB>,
    d: RoutedDispatcher<PaperDispatcher, BnArm>,
    lane4: Consumer<Fill, FILL_N>,
    journal: Consumer<ExecRecord, JOURNAL_RING>,
    gw: Option<JoinHandle<GwEnd>>,
    gw_stop: Arc<AtomicBool>,
    prefix: CidPrefix,
    next_oid: u64,
    fills: Vec<Fill>,
    recs: Vec<ExecRecord>,
    fills_dropped: u64,
    recs_dropped: u64,
    tap: Option<Tap>,
    /// The UM IoCs' client ids, for K2.
    k2_oids: Vec<u64>,
    /// BX11: a drill stopped the gateway — no shutdown sweep to wait for,
    /// and no order through it.
    gw_killed: bool,
    /// The TLS configuration (BX11: the flatten drill's own client).
    tls: Arc<ClientConfig>,
}

/// **Run the battery** (module docs): the exit code.
pub fn run(spec: &BatterySpec, tls: Arc<ClientConfig>) -> u8 {
    let mut rep = Report::default();
    if let Err(e) = spec.check() {
        error!("binance-battery: {e}");
        return 1;
    }
    match Battery::prepare(spec, tls) {
        Ok(mut b) => {
            if let Err((code, why)) = b.phases(&mut rep) {
                let why = if crate::sigint::shutdown_requested() { format!("{STOPPED} ({why})") } else { why };
                rep.fail(code, why);
            }
            b.end(&mut rep);
        }
        Err((code, why)) => rep.fail(code, why),
    }
    summary(spec, &mut rep)
}

fn summary(spec: &BatterySpec, rep: &mut Report) -> u8 {
    if rep.code == 0 {
        if let Some((code, why)) = bar_short(spec, rep) {
            rep.fail(code, why);
        }
    }
    let code = rep.exit();
    phase("END")
        .n("exit", i64::from(code))
        .b("bar_met", code == 0)
        .s("route", if spec.rest { "rest" } else { "ws" })
        .s("signing", if spec.hmac { "hmac" } else { "ed25519" })
        .n("cycles", i64::from(rep.cycles))
        .n("fills_usdm", i64::from(rep.fills[PRODUCT_USDM as usize]))
        .n("fills_coinm", i64::from(rep.fills[PRODUCT_COINM as usize]))
        .n("recons_usdm", i64::from(rep.recons[PRODUCT_USDM as usize]))
        .n("recons_coinm", i64::from(rep.recons[PRODUCT_COINM as usize]))
        .n("sweeps", i64::from(rep.sweeps))
        .n("ttl", i64::from(rep.ttl))
        .n("post_only_refused", i64::from(rep.post_only))
        .b("coinm_maker_refused", rep.cm_maker_refused)
        .n("reconnects", count(rep.reconnects))
        .n("connect_failures", count(rep.connect_failures))
        .n("gw_recon_ok", count(rep.recon_ok))
        .n("gw_recon_failed", count(rep.recon_failed))
        .n("journal_dropped", count(rep.journal_dropped))
        .n("fills_lost", count(rep.fills_lost))
        .n("tap_frames", count(rep.tap_frames))
        .n("tap_dropped", count(rep.tap_dropped))
        .n("recs_dropped", count(rep.recs_dropped))
        .n("fills_dropped", count(rep.fills_dropped))
        .n("fills_journaled", count(rep.fills_journaled))
        .n("fees_journaled", count(rep.fees_journaled))
        .n("drill_fired_ms", rep.drill_fired_ms.map_or(-1, |ms| ms as i64))
        .s("why", &rep.why)
        .s("left", &rep.left.join(" | "))
        .emit();
    for k in &rep.k2 {
        info!("binance-battery K2: {k}");
    }
    if code == EXIT_LEFTOVER {
        error!(left = ?rep.left, "binance-battery: SOMETHING WAS LEFT ON THE DEMO ACCOUNT");
    } else if code != 0 {
        error!(code, why = %rep.why, "binance-battery: FAILED");
    } else {
        info!("binance-battery: PASSED — the plan's bar is met; nothing left on the Demo account");
    }
    code
}

impl Battery {
    // ---------------------------------------------------------------------
    // 0–1. The account, the composition, the boot
    // ---------------------------------------------------------------------

    fn prepare(spec: &BatterySpec, tls: Arc<ClientConfig>) -> Result<Self, Fail> {
        let acct = |e: String| (EXIT_ACCOUNT, e);
        let stopped = || {
            if crate::sigint::shutdown_requested() {
                Err((EXIT_BOOT, format!("{STOPPED} before the boot — nothing was placed")))
            } else {
                Ok(())
            }
        };
        let cfg = demo_config(spec.hmac).map_err(acct)?;
        let signer = cfg.signer().map_err(|e| acct(format!("the signer: {e:?}")))?;
        let mut sets = Vec::with_capacity(2);
        let mut reads: Vec<(u8, String, SymbolId, Rest, Public)> = Vec::with_capacity(2);
        let mut discs: Vec<(u8, String, BnDiscovery, SymbolId)> = Vec::with_capacity(2);
        for (on, product, symbol, ord) in [(spec.um, PRODUCT_USDM, &spec.um_symbol, 513u32), (spec.cm, PRODUCT_COINM, &spec.cm_symbol, 3_073)] {
            if !on {
                continue;
            }
            stopped()?;
            let d = discover(&cfg, product, &tls).map_err(acct)?;
            discs.push((product, symbol.to_ascii_uppercase(), d, make_symbol_id(VenueId::Binance, ord)));
        }
        for (product, symbol, d, sym) in &discs {
            stopped()?;
            let row = *d.find(symbol.as_bytes()).ok_or_else(|| acct(format!("Demo lists no {symbol}")))?;
            let mut rest = Rest::new(&cfg, *product, tls.clone()).map_err(acct)?;
            let (offset, rtt) = rest.clock().map_err(acct)?;
            phase("clock").s("host", rest.host()).n("offset_ms", offset).n("rtt_ms", rtt as i64).emit();
            let public = Public::new(&cfg, *product, tls.clone()).map_err(acct)?;
            account_check(&mut rest, &signer, *product, symbol).map_err(acct)?;
            sets.push((*product, vec![Instrument { sym: *sym, name: symbol.clone(), descriptor: format!("binance-{}:{}", product_word(*product), symbol.to_ascii_lowercase()) }], vec![(symbol.clone(), row)]));
            reads.push((*product, symbol.clone(), *sym, rest, public));
        }
        // The arm's table, bound as `bn_live` binds it (classic: USDT-margined
        // USDⓈ-M rows only; COIN-M IoC-only).
        let binds: Vec<BindSet<'_>> = sets
            .iter()
            .map(|(product, insts, rows)| BindSet { product: *product, insts, rows, owned: &[] })
            .collect();
        let (table, wire, specs) = bind_table(&binds, SYMBOL_ID_NONE, AccountMode::Classic, false).map_err(|e| (EXIT_ACCOUNT, e))?;
        let mut lanes: Vec<LaneB> = reads
            .into_iter()
            .map(|(product, symbol, sym, rest, public)| {
                let row = table.row_of(sym);
                LaneB { product, h: *table.hot(row), c: *table.cold(row), symbol, sym, rest, public }
            })
            .collect();

        // The tap first: the account's listenKey (the gateway's POST returns
        // the same one).
        stopped()?;
        let tap = if spec.um {
            let lk = lanes[0].rest.plain(T_LISTEN_KEY_NEW, b"").map_err(acct)?;
            let key = scan_listen_key(&lk.body).map(|s| String::from_utf8_lossy(s.get(&lk.body)).into_owned()).map_err(|e| acct(format!("the tap's listenKey: {e:?}")))?;
            match Tap::spawn(&cfg.hosts().fut_user, format!("/ws/{key}"), tls.clone()) {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!("binance-battery: {e} — K2 is not measured this run");
                    None
                }
            }
        } else {
            None
        };

        // The gateway, booted here.
        let epoch = (wall_ms() / 1_000) as u32;
        let (eng_end, gw_end) = duplex();
        let (fill_tx, lane4) = Ring::<Fill, FILL_N>::new().split();
        let (j_tx, journal) = Ring::<ExecRecord, JOURNAL_RING>::new().split();
        let knobs = GwKnobs {
            epoch,
            owner_slot: SLOT,
            shared: false,
            mode: AccountMode::Classic,
            recv_window_ms: RECV_WINDOW_MS,
            stp_mode: String::from(STP_MODE),
            countdown_ms: COUNTDOWN_MS,
            heartbeat_ms: HEARTBEAT_MS,
            recon_every_ms: RECON_EVERY_MS,
            spin: false,
            rotate_after_ns: if spec.rotate_s == 0 { ROTATE_AFTER_NS } else { spec.rotate_s * 1_000_000_000 },
            rest_orders: spec.rest,
            pnl_anchor_1e6: 0,
            boot_timeout_ns: BOOT_TIMEOUT_NS,
            quiet_ns: QUIET_NS,
            anchor_unsaved: None,
            max_leverage: MAX_LEVERAGE,
            adopt_cap_usd_1e6: CAP_INSTANCE_USD_1E6,
        };
        let boot = |e: String| (EXIT_BOOT, e);
        let mut gw = BnGateway::<FILL_N>::new(&cfg, tls.clone(), knobs, wire, gw_end, fill_tx, JournalTx::new(j_tx))
            .map_err(|e| boot(format!("the gateway could not be built: {e:?}")))?;
        stopped()?;
        let t0 = Instant::now();
        gw.boot().map_err(|e| boot(format!("the boot was refused: {e:?}")))?;
        // A signal during the boot: nothing is placed yet — the gateway
        // closes as it drops.
        stopped()?;
        let (adopted, _) = gw.adopted();
        let unseen = gw.last_verdict().unseen_legs;
        phase("boot")
            .n("ms", t0.elapsed().as_millis() as i64)
            .n("epoch", i64::from(epoch))
            .s("route", if spec.rest { "rest" } else { "ws" })
            .n("adopted", i64::from(adopted))
            .n("unseen_legs", i64::from(unseen))
            .emit();
        if adopted > 0 || unseen > 0 {
            return Err(boot(format!("the boot found venue positions ({adopted} adopted, {unseen} unseen) on a flat-checked account")));
        }
        let mut lim = [0i64; SLOTS];
        lim[SLOT as usize] = 600_000;
        let mut prods = [0u8; SLOTS];
        prods[SLOT as usize] = (u8::from(spec.um) << PRODUCT_USDM) | (u8::from(spec.cm) << PRODUCT_COINM);
        let arm = BnArm::new(
            eng_end,
            table,
            ArmKnobs {
                orders_frac_1e6: 800_000,
                qtr_frac_1e6: 800_000,
                owner_slot: SLOT,
                max_symbols: 8,
                min_maker_ttl_ns: MIN_MAKER_TTL_NS,
                margin: MarginBook::new(lim, prods),
                anchor: core_time::WallAnchor::now(),
                mode: exec_binance::mode::AccountMode::Classic,
                demo: true,
            },
        );
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                SLOT as usize,
                ExecMode::Live,
                &[VenueId::Binance.to_u8()],
                SlotCaps::new(CAP_ORDER_USD_1E6, CAP_INSTANCE_USD_1E6, CAP_DAY_USD_1E6, MAX_OPEN_ORDERS),
                HALT_LIMITS,
            )
            .map_err(|e| boot(format!("the route: {e:?}")))?;
        let mut d = RoutedDispatcher::new(route, PaperDispatcher::new(), arm, core_time::WallAnchor::now());
        for s in &specs {
            d.bind_instrument(s).map_err(|e| boot(format!("the ledger row: {e:?}")))?;
        }
        let gw_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&gw_stop);
        let handle = std::thread::Builder::new()
            .name(String::from("bn-gateway"))
            .spawn(move || {
                gw.run(&stop, 0);
                (gw.phase(), gw.last_sweep().2)
            })
            .map_err(|e| boot(format!("the gateway thread: {e}")))?;
        Ok(Self {
            spec: spec.clone(),
            _cfg: cfg,
            signer,
            lanes,
            d,
            lane4,
            journal,
            gw: Some(handle),
            gw_stop,
            prefix: CidPrefix::new(epoch),
            next_oid: 0,
            fills: Vec::with_capacity(256),
            recs: Vec::with_capacity(4_096),
            fills_dropped: 0,
            recs_dropped: 0,
            tap,
            k2_oids: Vec::with_capacity(16),
            gw_killed: false,
            tls,
        })
    }

    // ---------------------------------------------------------------------
    // The engine's turn
    // ---------------------------------------------------------------------

    /// One engine iteration in the engine's own order: the idle hook, then
    /// lane 4; then the journal (the battery's evidence, kept within its
    /// caps — past them, counted, and growth stops).
    fn turn(&mut self) {
        self.d.on_idle();
        while let Some(g) = self.lane4.try_pop_ref() {
            self.d.on_fill_booked(&g);
            if self.fills.len() < FILLS_MAX {
                // COPY: the fill out of its ring slot (64 B) — cold: the
                // battery's evidence outlives the slot (borrowing it
                // rejected: the slot is the ring's again at the next pop).
                self.fills.push(*g);
            } else {
                self.fills_dropped += 1;
            }
        }
        while let Some(g) = self.journal.try_pop_ref() {
            if self.recs.len() < RECS_MAX {
                // COPY: the record out of its ring slot (64 B) — cold, as
                // the fill's.
                self.recs.push(*g);
            } else {
                self.recs_dropped += 1;
            }
        }
    }

    /// Turn until `cond` holds (`true`) or `within` passes, a signal lands
    /// (`false`).
    fn until(&mut self, within: Duration, cond: impl Fn(&Self) -> bool) -> bool {
        let t0 = Instant::now();
        loop {
            self.turn();
            if cond(self) {
                return true;
            }
            if t0.elapsed() >= within || crate::sigint::shutdown_requested() {
                return false;
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }

    /// Turn for `ms`, or until a signal lands.
    fn idle_for(&mut self, ms: u64) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(ms) && !crate::sigint::shutdown_requested() {
            self.turn();
            std::thread::sleep(Duration::from_micros(250));
        }
    }

    /// The gate before a reduce-only close: no signal, no halt on the slot.
    /// (Growth's other stops do not keep a position open: reduce-only never
    /// adds to it, and a halted slot's sweep cancels.)
    fn may_reduce(&self) -> Result<(), String> {
        if crate::sigint::shutdown_requested() {
            return Err(String::from(STOPPED));
        }
        if self.d.halt().is_halted(SLOT as usize) {
            return Err(format!("the slot halted: {:?}", self.d.halt().reason(SLOT as usize)));
        }
        Ok(())
    }

    /// **The one gate before every place and modify**: [`Battery::may_reduce`]'s,
    /// nothing the arm's halt word says, no drift in the latest
    /// reconciliation (the router halts at 5 USD; the battery grows only on
    /// an agreeing one), and its record not full.
    fn may_grow(&self) -> Result<(), String> {
        self.may_reduce()?;
        let s = self.d.live().halt_signal();
        if s.gateway_halted != 0 || s.venue_lock != 0 || s.margin_risk != 0 || s.budget_floor_breached != 0 {
            return Err(format!("the arm's halt word: {s:?}"));
        }
        if s.recon_drift_usd_1e6 != 0 {
            return Err(format!("the latest reconciliation drifted {} USD ×1e6 — no growth until the venue and the ledger agree", s.recon_drift_usd_1e6));
        }
        if self.recs_dropped > 0 || self.fills_dropped > 0 {
            return Err(format!("the battery's record is full ({} records, {} fills dropped)", self.recs_dropped, self.fills_dropped));
        }
        Ok(())
    }

    /// The journal's first record of `kind` for client id `oid`.
    fn rec(&self, kind: u8, oid: u64) -> Option<&ExecRecord> {
        self.recs.iter().find(|r| r.kind == kind && r.client_oid == oid)
    }

    /// `oid`'s end: its `J_ENDED` (the `RETIRED_*` why) or `J_REJECT` (the
    /// code).
    fn ended(&self, oid: u64) -> Option<(u8, i32)> {
        self.end_rec(oid).map(|r| (r.kind, r.code))
    }

    fn end_rec(&self, oid: u64) -> Option<&ExecRecord> {
        self.recs.iter().find(|r| (r.kind == J_ENDED || r.kind == J_REJECT) && r.client_oid == oid)
    }

    /// Lane 4's fills for `oid`: (count, signed quantity ×1e6).
    fn filled(&self, oid: u64) -> (u32, i64) {
        let mut n = 0;
        let mut q = 0i64;
        for f in self.fills.iter().filter(|f| f.order_id == oid) {
            n += 1;
            q += if f.side == Side::Bid { f.qty.raw() } else { -f.qty.raw() };
        }
        (n, q)
    }

    fn pos(&self, sym: SymbolId) -> i64 {
        self.d.ledger().instrument_position_1e6(SLOT as usize, sym).unwrap_or(0)
    }

    /// Our id on the venue for `oid` (this boot's prefix, the slot).
    fn cid(&self, oid: u64) -> String {
        let mut out = [0u8; CID_LEN];
        self.prefix.render(SLOT, oid, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[allow(clippy::too_many_arguments)]
    fn order(&mut self, l: usize, side: Side, kind: u8, px_1e6: i64, qty_1e6: i64, flags: u8, ttl_ns: u64) -> Order {
        self.next_oid += 1;
        let mut o = Order::new(
            core_time::now_ns(),
            VenueId::Binance,
            self.lanes[l].sym,
            side,
            kind,
            Price::from_raw(px_1e6),
            Qty::from_raw(qty_1e6),
            self.next_oid,
        );
        o.strategy_id = SLOT;
        o.flags = flags;
        o.ttl_ns = ttl_ns;
        o
    }

    /// The touch, read inside the band around the mark (`Public::priced`);
    /// the mark handed to the dispatcher as the mark lane would (the arm
    /// bands against it; the ledger prices with it).
    fn priced(&mut self, l: usize) -> Result<(i64, i64), String> {
        let lane = &mut self.lanes[l];
        let (t, mark, index) = lane.public.priced(lane.symbol.as_bytes(), &lane.c)?;
        let ev = ChannelEvent::new(core_time::now_ns(), VenueId::Binance, ChannelId::Mark, lane.sym, 0, wall_ms(), mark, index);
        self.d.on_venue_event(&ev);
        Ok((t.bid_1e6, t.ask_1e6))
    }

    /// Submit through the growth gate (module docs: [`Battery::verb`]).
    fn submit(&mut self, o: &Order) -> Result<(), String> {
        self.may_grow()?;
        self.verb(false, |d| d.submit(o))
    }

    /// **One verb as the engine sends it**: after a turn — the arm judges
    /// readiness on the gateway's pulse, which only the idle hook delivers,
    /// and the battery's blocking reads pause it — retrying while the arm
    /// holds it back for a transient reason (`NotReady`: the pulse or a
    /// route; `DeadmanDown`: a maker before the dead-man is proven). After
    /// a signal no verb goes — but a cancel (`exit`: a member getting out)
    /// goes once, unretried.
    fn verb(&mut self, exit: bool, mut send: impl FnMut(&mut RoutedDispatcher<PaperDispatcher, BnArm>) -> Result<(), DispatchError>) -> Result<(), String> {
        let t0 = Instant::now();
        loop {
            let stopped = crate::sigint::shutdown_requested();
            if stopped && !exit {
                return Err(String::from(STOPPED));
            }
            self.turn();
            let c = *self.d.live().counters();
            match send(&mut self.d) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let now = *self.d.live().counters();
                    let transient = now.refused[Refusal::DeadmanDown as usize] > c.refused[Refusal::DeadmanDown as usize]
                        || now.refused[Refusal::NotReady as usize] > c.refused[Refusal::NotReady as usize];
                    if !transient || stopped || t0.elapsed() > STEP_WAIT {
                        return Err(format!("refused: {e:?} (the arm's refusals {:?})", refusal_delta(&c.refused, &now.refused)));
                    }
                }
            }
            self.idle_for(100);
        }
    }

    /// Wait for `oid`'s ACK; a refusal or a silence is the error.
    fn acked(&mut self, oid: u64) -> Result<u64, String> {
        if !self.until(STEP_WAIT, |b| b.rec(J_ACK, oid).is_some() || b.ended(oid).is_some()) {
            return Err(format!("order {oid}: no answer within {STEP_WAIT:?}"));
        }
        if let Some(r) = self.rec(J_ACK, oid) {
            return Ok(r.ts_ns);
        }
        Err(format!("order {oid}: refused {:?}", self.ended(oid)))
    }

    /// Wait for `oid` to end; its (kind, code-or-why).
    fn end_of(&mut self, oid: u64, within: Duration) -> Result<(u8, i32), String> {
        if !self.until(within, |b| b.ended(oid).is_some()) {
            return Err(format!("order {oid}: no end within {within:?}"));
        }
        self.ended(oid).ok_or_else(|| String::from("unreachable: an end vanished"))
    }

    /// **A reconciliation that agrees, journaled after `after_ns`**
    /// (reconciled, no drift) — the leg's proof.
    fn reconciled_after(&mut self, after_ns: u64, product: u8, rep: &mut Report) -> Result<(), String> {
        let ok = |r: &ExecRecord| r.kind == J_RECON && r.ts_ns > after_ns && r.code == 1 && r.a == 0;
        if !self.until(RECON_WAIT, |b| b.recs.iter().any(ok)) {
            let last = self.recs.iter().rev().find(|r| r.kind == J_RECON && r.ts_ns > after_ns);
            return Err(format!(
                "no reconciliation agreed within {RECON_WAIT:?} (last after the leg: {})",
                last.map_or_else(|| String::from("none"), |r| format!("reconciled {} drift {} unseen {} foreign {}", r.code, r.a, r.b, r.c))
            ));
        }
        rep.recons[product as usize] += 1;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // 2–4. The phases
    // ---------------------------------------------------------------------

    fn phases(&mut self, rep: &mut Report) -> Result<(), Fail> {
        // The router seeds the slot from the arm's first reconciled report;
        // until then every order is refused. Here, not in the boot: the
        // gateway thread runs, so a failure must go through the end.
        if !self.until(SEED_WAIT, |b| b.d.ledger().is_slot_seeded(SLOT as usize)) {
            return Err((EXIT_BOOT, String::from("the router never seeded the slot (no reconciled report from the arm)")));
        }
        match self.spec.drill {
            Some(Drill::Gap) => return self.gap_drill(rep),
            Some(Drill::Flatten) => return self.flatten_drill(rep),
            None => {}
        }
        if self.spec.um {
            let l = self.lane(PRODUCT_USDM);
            for i in 0..self.spec.cycles {
                self.maker_cycle(l, i).map_err(|e| (EXIT_CYCLE, format!("cycle {i}: {e}")))?;
                rep.cycles += 1;
            }
            self.post_only(l).map_err(|e| (EXIT_POST_ONLY, e))?;
            rep.post_only += 1;
            self.ttl(l).map_err(|e| (EXIT_TTL, e))?;
            rep.ttl += 1;
        }
        if self.spec.cm {
            let l = self.lane(PRODUCT_COINM);
            self.cm_maker_refused(l).map_err(|e| (EXIT_FILLS, e))?;
            rep.cm_maker_refused = true;
        }
        for product in [PRODUCT_USDM, PRODUCT_COINM] {
            let on = if product == PRODUCT_USDM { self.spec.um } else { self.spec.cm };
            if !on {
                continue;
            }
            let l = self.lane(product);
            for k in 0..self.spec.fill_pairs {
                self.round_trip(l, k, rep)?;
            }
        }
        if self.spec.um {
            let l = self.lane(PRODUCT_USDM);
            self.halt_sweep(l).map_err(|e| (EXIT_SWEEP, e))?;
            rep.sweeps += 1;
        }
        Ok(())
    }

    /// **BX11 drill — the gateway dies under resting makers** (plan §5
    /// BX11): two post-only makers rest behind the touch under the
    /// dead-man (the gateway's 30 s countdown, renewed every heartbeat);
    /// once the arm's report says the dead-man covers them, the gateway
    /// thread is stopped WITHOUT its sweep; the account's open orders are
    /// read every second until neither is listed — within the countdown
    /// and 10 s, or the drill fails and cancels them by id (the end then
    /// reads the account flat).
    fn gap_drill(&mut self, rep: &mut Report) -> Result<(), Fail> {
        // Before the kill a failure is the cycle's: the dead-man was never
        // put to the test (and the end's sweep cancels the makers).
        let cycle = |e: String| (EXIT_CYCLE, e);
        let l = self.lane(PRODUCT_USDM);
        let h = self.lanes[l].h;
        let (bid, ask) = self.priced(l).map_err(cycle)?;
        let mut cids = Vec::with_capacity(2);
        for (side, from, down) in [(Side::Bid, bid, true), (Side::Ask, ask, false)] {
            let px = px_off(&h, from, self.spec.rest_bps, down).map_err(cycle)?;
            let qty = qty_for(&h, px, self.spec.usd * 1_000_000).map_err(cycle)?;
            let o = self.order(l, side, ORDER_KIND_MAKER, px, qty, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
            self.submit(&o).map_err(cycle)?;
            self.acked(o.client_oid).map_err(cycle)?;
            let cid = self.cid(o.client_oid);
            self.listed(l, &cid, Some((px, qty)), "the drill's maker resting").map_err(cycle)?;
            cids.push(cid);
        }
        let covered = |b: &Self| {
            let mut r = BinanceArmReport::default();
            b.d.binance_arm_report(core_time::now_ns(), &mut r) && r.deadman_ok == 1
        };
        if !self.until(STEP_WAIT, covered) {
            return Err(cycle(String::from("the dead-man never covered the makers")));
        }
        let phase_end = self.kill_gateway().map_err(cycle)?;
        let died = Instant::now();
        phase("drill").s("drill", "gap").s("gateway", &format!("{phase_end:?}")).s("makers", &cids.join(" ")).emit();
        // The venue's countdown, renewed no more, fires. A read that fails
        // is read again until the deadline: the verdict is the venue's book.
        let drill = |e: String| (EXIT_DRILL, e);
        let deadline = Duration::from_millis(COUNTDOWN_MS) + DRILL_MARGIN;
        let mut left = cids.len();
        loop {
            // After a signal nothing more is read: the makers are cancelled
            // by id below (the drain bound).
            let signalled = crate::sigint::shutdown_requested();
            if !signalled {
                let lane = &mut self.lanes[l];
                match open_orders(&mut lane.rest, &self.signer, Some(lane.symbol.as_bytes())) {
                    Ok(rows) => left = cids.iter().filter(|c| rows.iter().any(|r| r.cid == **c)).count(),
                    Err(e) => warn!("binance-battery: the drill's listing: {e} — read again"),
                }
            }
            if left == 0 && !signalled {
                // Unlisted is cancelled — or filled, which proves no
                // dead-man: each maker's own status says which (a read that
                // fails, or names none, is read again until the deadline).
                let mut words = Vec::with_capacity(cids.len());
                for c in &cids {
                    let lane = &mut self.lanes[l];
                    let sym = lane.symbol.clone();
                    let a = lane.rest.signed(ORD0 + R_STATUS, &self.signer, false, |w, st, ts| {
                        render_by_cid(w, st, sym.as_bytes(), Part::Lit(c.as_bytes()), ts);
                    });
                    words.push((c.clone(), a.as_ref().map_or((0, 0), order_state)));
                }
                let ms = died.elapsed().as_millis() as u64;
                if words.iter().all(|(_, (s, _))| *s != 0) {
                    // The countdown's own cancel reads CANCELED — and no
                    // sooner than the countdown less two heartbeats (a
                    // covered dead-man's last renewal can be two old).
                    let cancelled = words.iter().all(|(_, (s, _))| *s == S_CANCELED);
                    let in_time = ms + 2 * HEARTBEAT_MS >= COUNTDOWN_MS;
                    phase("drill")
                        .s("drill", "gap")
                        .b("fired", cancelled && in_time)
                        .n("after_ms", ms as i64)
                        .s("statuses", &format!("{words:?}"))
                        .emit();
                    if !cancelled {
                        return Err(drill(format!("the makers left the book, but not cancelled ({words:?}): the dead-man is unproven")));
                    }
                    if !in_time {
                        return Err(drill(format!("the makers were cancelled after {ms} ms — sooner than the countdown: not the dead-man")));
                    }
                    rep.drill_fired_ms = Some(ms);
                    return Ok(());
                }
                warn!("binance-battery: the drill's status read named none ({words:?}) — read again");
            }
            if died.elapsed() > deadline || signalled {
                let why = if left == 0 {
                    String::from("the makers left the book, but their statuses could not be read")
                } else {
                    format!("the dead-man did not cancel {left} maker(s) within {} s", deadline.as_secs())
                };
                for c in &cids {
                    let lane = &mut self.lanes[l];
                    let sym = lane.symbol.clone();
                    let a = lane.rest.signed(ORD0 + R_CANCEL, &self.signer, false, |w, st, ts| {
                        render_by_cid(w, st, sym.as_bytes(), Part::Lit(c.as_bytes()), ts);
                    });
                    warn!("binance-battery: the drill's cancel of {c}: {}", a.map_or_else(|e| e, |a| a.text()));
                }
                return Err(drill(why));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// A drill's stop: the gateway thread ends WITHOUT its sweep (the engine
    /// asked for none); from here on the end neither sweeps nor orders
    /// through it (`gw_killed`).
    fn kill_gateway(&mut self) -> Result<Phase, String> {
        self.gw_stop.store(true, Ordering::Release);
        // From here the end neither sweeps nor orders through it — stopped
        // in time or not.
        self.gw_killed = true;
        let t0 = Instant::now();
        while self.gw.as_ref().is_some_and(|h| !h.is_finished()) && t0.elapsed() < GW_STOP_WAIT {
            std::thread::sleep(Duration::from_millis(5));
        }
        // A thread still running keeps its handle: the end waits for it.
        if !self.gw.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(String::from("the gateway thread did not stop"));
        }
        let handle = self.gw.take().ok_or_else(|| String::from("no gateway thread"))?;
        Ok(handle.join().unwrap_or((Phase::Clock, 0)).0)
    }

    /// **BX11 drill — the flatten on a real position** (O-BX9, plan §3.11
    /// kill switch 5): the arm opens a position with one IoC through the
    /// touch, reconciled; its gateway stops (no order of the arm's rests —
    /// an IoC never does); then the operator's tool, `bn_ops::flatten` on
    /// Demo, closes it with its own reduce-only IoC and judges by a fresh
    /// read. Its exit code is the drill's; the end reads the account flat.
    fn flatten_drill(&mut self, rep: &mut Report) -> Result<(), Fail> {
        let product = if self.spec.um { PRODUCT_USDM } else { PRODUCT_COINM };
        let l = self.lane(product);
        let open = self.ioc(l, true, None).map_err(|e| (EXIT_FILLS, format!("the drill's opening IoC: {e}")))?;
        rep.fills[product as usize] += open.1;
        self.reconciled_after(open.2, product, rep).map_err(|e| (EXIT_RECON, e))?;
        let opened = self.pos(self.lanes[l].sym);
        let gateway = self.kill_gateway().map_err(|e| (EXIT_CYCLE, e))?;
        phase("drill").s("drill", "flatten").s("gateway", &format!("{gateway:?}")).n("opened", opened).emit();
        let spec = OpsSpec {
            reach: Reach::Demo,
            confirm: false,
            hmac: self.spec.hmac,
            product,
            symbols: vec![self.lanes[l].symbol.clone()],
            leverage: None,
            cap: DEFAULT_LEVERAGE_CAP,
            sign_tradfi: false,
            reassert: false,
            cancel_first: false,
            close_bps: 20,
        };
        if crate::sigint::shutdown_requested() {
            return Err((EXIT_FILLS, String::from("a signal before the flatten: the position is left to the flat read")));
        }
        let code = flatten(&spec, self.tls.clone());
        phase("drill").s("drill", "flatten").n("flatten_exit", i64::from(code)).emit();
        if code != 0 {
            return Err((code, format!("the flatten answered {code} on a {opened} ×1e6 position")));
        }
        Ok(())
    }

    fn lane(&self, product: u8) -> usize {
        self.lanes.iter().position(|l| l.product == product).unwrap_or(0)
    }

    /// One maker cycle (module docs, 2).
    fn maker_cycle(&mut self, l: usize, i: u32) -> Result<(), String> {
        let buy = i % 2 == 0;
        let (side, other_down) = if buy { (Side::Bid, true) } else { (Side::Ask, false) };
        let h = self.lanes[l].h;
        let (bid, ask) = self.priced(l)?;
        let px = if buy { px_off(&h, bid, self.spec.rest_bps, true)? } else { px_off(&h, ask, self.spec.rest_bps, false)? };
        let qty = qty_for(&h, px, self.spec.usd * 1_000_000)?;
        let o = self.order(l, side, ORDER_KIND_MAKER, px, qty, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
        self.submit(&o)?;
        self.acked(o.client_oid)?;
        let cid = self.cid(o.client_oid);
        let r0 = self.listed(l, &cid, Some((px, qty)), "the acknowledged order under our id")?;
        // A modify that moves the price (further behind the touch); the
        // venue keeps our id.
        let px2 = px_off(&h, px, MOVE_BPS, other_down)?;
        let m1 = self.modify(l, &o, px2, qty)?;
        let r1 = self.listed(l, &cid, Some((px2, qty)), "the price modify's terms")?;
        // A modify of the size alone.
        let qty2 = qty + i64::from(h.step_1e6);
        let m2 = self.modify(l, &m1, px2, qty2)?;
        let r2 = self.listed(l, &cid, Some((px2, qty2)), "the size modify's terms")?;
        // The cancel through the verb.
        let mut c = CancelReq::new(core_time::now_ns(), VenueId::Binance, self.lanes[l].sym, m2.client_oid);
        c.strategy_id = SLOT;
        self.verb(true, |d| d.cancel(&c)).map_err(|e| format!("the cancel: {e}"))?;
        match self.end_of(m2.client_oid, STEP_WAIT)? {
            (J_ENDED, why) if why == i32::from(RETIRED_CANCELED_MEMBER) => {}
            other => return Err(format!("the cancel ended the order otherwise: {other:?}")),
        }
        let r3 = self.listed(l, &cid, None, "the cancelled order gone")?;
        phase("cycle")
            .n("i", i64::from(i))
            .s("side", if buy { "buy" } else { "sell" })
            .n("px", px)
            .n("px2", px2)
            .n("qty", qty)
            .n("qty2", qty2)
            .s("listing_reads", &format!("{r0}/{r1}/{r2}/{r3}"))
            .emit();
        Ok(())
    }

    /// **The venue's listing agrees**: `cid` listed at `terms` (price,
    /// quantity) — or, with `None`, not listed — within [`LISTING_READS`]
    /// reads (its REST listing can trail the WS API's answer); a listing
    /// naming an id twice is refused (`bn_demo::open_orders`). The reads it
    /// took.
    fn listed(&mut self, l: usize, cid: &str, terms: Option<(i64, i64)>, what: &str) -> Result<u32, String> {
        let mut seen = None;
        for k in 1..=LISTING_READS {
            if crate::sigint::shutdown_requested() {
                return Err(String::from(STOPPED));
            }
            let lane = &mut self.lanes[l];
            let rows = open_orders(&mut lane.rest, &self.signer, Some(lane.symbol.as_bytes()))?;
            let row = rows.iter().find(|r| r.cid == cid).map(|r| (r.px_1e6, r.qty_1e6));
            if row == terms {
                return Ok(k);
            }
            seen = row;
            self.idle_for(LISTING_GAP.as_millis() as u64);
        }
        Err(format!("the venue's listing never showed {what}: wanted {terms:?}, listed {seen:?}"))
    }

    /// Modify `prev` to `px` × `qty` under a fresh member id; wait for the
    /// venue's confirmation (the arm's `modified`).
    fn modify(&mut self, l: usize, prev: &Order, px: i64, qty: i64) -> Result<Order, String> {
        self.may_grow()?;
        let mut m = self.order(l, prev.side, ORDER_KIND_MAKER, px, qty, prev.flags, SAFETY_TTL_NS);
        m.ts_ns = core_time::now_ns();
        let before = *self.d.live().counters();
        let req = ModifyReq::new(prev.client_oid, m);
        self.verb(false, |d| d.modify(&req)).map_err(|e| format!("the modify: {e}"))?;
        let done = self.until(STEP_WAIT, |b| {
            let c = b.d.live().counters();
            c.modified > before.modified || c.modifies_refused > before.modifies_refused
        });
        let after = *self.d.live().counters();
        if !done || after.modified == before.modified {
            return Err(format!("the modify was not confirmed (refused {})", after.modifies_refused - before.modifies_refused));
        }
        Ok(m)
    }

    /// **The halt's sweep** (module docs, 4): two quotes, then an operator
    /// halt of the slot — the router's cancel of the slot, `Working` →
    /// `Clear`, the venue listing neither. The last growth of the run.
    fn halt_sweep(&mut self, l: usize) -> Result<(), String> {
        let h = self.lanes[l].h;
        let (bid, ask) = self.priced(l)?;
        let (pb, pa) = (px_off(&h, bid, self.spec.rest_bps, true)?, px_off(&h, ask, self.spec.rest_bps, false)?);
        let (qb, qa) = (qty_for(&h, pb, self.spec.usd * 1_000_000)?, qty_for(&h, pa, self.spec.usd * 1_000_000)?);
        let b = self.order(l, Side::Bid, ORDER_KIND_MAKER, pb, qb, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
        let a = self.order(l, Side::Ask, ORDER_KIND_MAKER, pa, qa, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
        self.submit(&b)?;
        self.submit(&a)?;
        self.acked(b.client_oid)?;
        self.acked(a.client_oid)?;
        if !self.d.halt_slot(SLOT as usize, HaltReason::Operator) {
            return Err(String::from("the operator halt did not latch"));
        }
        let ok = self.until(STEP_WAIT, |x| {
            x.d.live().cancel_slot_state(SLOT) == CancelAllState::Clear && x.ended(b.client_oid).is_some() && x.ended(a.client_oid).is_some()
        });
        if !ok {
            return Err(format!(
                "the halt's sweep did not read clear ({:?}; ended {:?} / {:?})",
                self.d.live().cancel_slot_state(SLOT),
                self.ended(b.client_oid),
                self.ended(a.client_oid)
            ));
        }
        for oid in [b.client_oid, a.client_oid] {
            match self.ended(oid) {
                Some((J_ENDED, _)) => {}
                other => return Err(format!("a swept order ended otherwise: {other:?}")),
            }
        }
        let (cb, ca) = (self.cid(b.client_oid), self.cid(a.client_oid));
        self.listed(l, &cb, None, "the swept bid gone")?;
        self.listed(l, &ca, None, "the swept ask gone")?;
        phase("halt_sweep").n("cleared", 2).s("halt", &format!("{:?}", self.d.halt().reason(SLOT as usize))).emit();
        Ok(())
    }

    /// A post-only order that would cross: refused (never a fill).
    fn post_only(&mut self, l: usize) -> Result<(), String> {
        let h = self.lanes[l].h;
        let (_, ask) = self.priced(l)?;
        let px = px_off(&h, ask, self.spec.cross_bps, false)?;
        let o = self.order(l, Side::Bid, ORDER_KIND_MAKER, px, qty_for(&h, px, self.spec.usd * 1_000_000)?, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
        self.submit(&o)?;
        let (kind, code) = self.end_of(o.client_oid, STEP_WAIT)?;
        let (n, _) = self.filled(o.client_oid);
        if n > 0 {
            return Err(String::from("a post-only order TRADED"));
        }
        let how = match (kind, code) {
            (J_REJECT, c) => format!("refused {c}"),
            (_, w) if w == i32::from(RETIRED_EXPIRED) => String::from("expired"),
            (_, w) if w == i32::from(RETIRED_REJECTED) => String::from("rejected"),
            (k, w) => return Err(format!("the crossing post-only order ended otherwise: {k} {w}")),
        };
        phase("post_only").n("px", px).s("ended", &how).emit();
        Ok(())
    }

    /// A maker with a short TTL: the gateway's own cancel (`CANCELED_TTL`).
    fn ttl(&mut self, l: usize) -> Result<(), String> {
        let h = self.lanes[l].h;
        let (bid, _) = self.priced(l)?;
        let px = px_off(&h, bid, self.spec.rest_bps, true)?;
        let ttl_ns = self.spec.ttl_ms * 1_000_000;
        let o = self.order(l, Side::Bid, ORDER_KIND_MAKER, px, qty_for(&h, px, self.spec.usd * 1_000_000)?, ORDER_FLAG_POST_ONLY, ttl_ns);
        self.submit(&o)?;
        self.acked(o.client_oid)?;
        match self.end_of(o.client_oid, Duration::from_nanos(ttl_ns) + STEP_WAIT)? {
            (J_ENDED, why) if why == i32::from(RETIRED_CANCELED_TTL) => {}
            other => return Err(format!("the TTL order ended otherwise: {other:?}")),
        }
        phase("ttl").n("ttl_ms", self.spec.ttl_ms as i64).emit();
        Ok(())
    }

    /// COIN-M is IoC-only (O-BX3a): a maker is refused by the arm itself —
    /// asked through the growth gate, as every other order.
    fn cm_maker_refused(&mut self, l: usize) -> Result<(), String> {
        let h = self.lanes[l].h;
        let (bid, _) = self.priced(l)?;
        let px = px_off(&h, bid, self.spec.rest_bps, true)?;
        let o = self.order(l, Side::Bid, ORDER_KIND_MAKER, px, qty_for(&h, px, self.spec.usd * 1_000_000)?, ORDER_FLAG_POST_ONLY, SAFETY_TTL_NS);
        self.may_grow()?;
        self.turn();
        let before = self.d.live().counters().refused[Refusal::NoDeadman as usize];
        match self.d.submit(&o) {
            Err(_) if self.d.live().counters().refused[Refusal::NoDeadman as usize] > before => {
                phase("coinm_maker").s("refused", "no_deadman").emit();
                Ok(())
            }
            other => Err(format!("a COIN-M maker was not refused for its dead-man: {other:?}")),
        }
    }

    /// One IoC round trip (module docs, 3).
    fn round_trip(&mut self, l: usize, k: u32, rep: &mut Report) -> Result<(), Fail> {
        let product = self.lanes[l].product;
        let fills = |e: String| (EXIT_FILLS, format!("{} round trip {k}: {e}", product_word(product)));
        let buy = k % 2 == 0;
        let open = self.ioc(l, buy, None).map_err(fills)?;
        if product == PRODUCT_USDM {
            self.k2_oids.push(open.0);
        }
        rep.fills[product as usize] += open.1;
        self.reconciled_after(open.2, product, rep).map_err(|e| (EXIT_RECON, e))?;
        let pos = self.pos(self.lanes[l].sym);
        if pos == 0 {
            return Err(fills(String::from("the fill moved no position in the ledger")));
        }
        let close = self.ioc(l, pos < 0, Some(pos.abs())).map_err(fills)?;
        rep.fills[product as usize] += close.1;
        self.reconciled_after(close.2, product, rep).map_err(|e| (EXIT_RECON, e))?;
        let left = self.pos(self.lanes[l].sym);
        phase("round_trip").s("product", product_word(product)).n("k", i64::from(k)).n("opened", pos).n("left", left).emit();
        if left != 0 {
            return Err(fills(format!("the reduce-only close left {left}")));
        }
        Ok(())
    }

    /// An IoC through the touch — `reduce` the size of a reduce-only close —
    /// waited to its end: (its id, fills, its end's time). It must fill.
    /// Its END is the leg's answer, not its ACK: the stream can end an IoC
    /// before the WS API's answer lands (on Demo the user stream and the WS
    /// API are different hosts), and an answer for an order already ended
    /// is applied to nothing — so journaled as nothing. An opening IoC goes
    /// through the growth gate; a close, through the reduce gate.
    fn ioc(&mut self, l: usize, buy: bool, reduce: Option<i64>) -> Result<(u64, u32, u64), String> {
        let h = self.lanes[l].h;
        let (bid, ask) = self.priced(l)?;
        let px = if buy { px_off(&h, ask, self.spec.cross_bps, false)? } else { px_off(&h, bid, self.spec.cross_bps, true)? };
        let (qty, flags) = match reduce {
            Some(q) => (q, ORDER_FLAG_REDUCE_ONLY),
            None => (qty_for(&h, px, self.spec.usd * 1_000_000)?, 0),
        };
        let o = self.order(l, if buy { Side::Bid } else { Side::Ask }, ORDER_KIND_IOC, px, qty, flags, 0);
        if reduce.is_some() {
            self.may_reduce()?;
        } else {
            self.may_grow()?;
        }
        self.verb(false, |d| d.submit(&o))?;
        let end = self.end_of(o.client_oid, STEP_WAIT)?;
        if end.0 == J_REJECT {
            return Err(format!("the IoC was refused {}", end.1));
        }
        let end_ns = self.end_rec(o.client_oid).map_or(0, |r| r.ts_ns);
        // The fills reach lane 4 before the retirement (obligation 1).
        let (n, q) = self.filled(o.client_oid);
        if n == 0 || q == 0 {
            return Err(format!("the IoC through the touch did not fill (ended {end:?})"));
        }
        if end != (J_ENDED, i32::from(RETIRED_FILLED)) && end != (J_ENDED, i32::from(RETIRED_EXPIRED)) {
            return Err(format!("the IoC ended otherwise: {end:?}"));
        }
        let journaled = self.recs.iter().filter(|r| r.kind == J_FILL && r.client_oid == o.client_oid).count();
        phase("ioc")
            .s("product", product_word(self.lanes[l].product))
            .s("side", if buy { "buy" } else { "sell" })
            .b("reduce_only", reduce.is_some())
            .n("px", px)
            .n("qty", qty)
            .n("filled", q)
            .n("fills", i64::from(n))
            .n("journaled", journaled as i64)
            .b("ack_journaled", self.rec(J_ACK, o.client_oid).is_some())
            .emit();
        Ok((o.client_oid, n, end_ns))
    }

    // ---------------------------------------------------------------------
    // 5. The end
    // ---------------------------------------------------------------------

    fn end(mut self, rep: &mut Report) {
        // Every position the ledger holds, closed (reduce-only, wider each
        // try) while the slot can still order — not after a signal (a
        // member's exit is not the signal's to make), not once halted (a
        // halted slot refuses every order; its sweep cancels), and not
        // through a gateway a drill stopped (the flat read judges).
        let mut l = 0;
        while l < self.lanes.len() {
            let mut tries = 0;
            while !self.gw_killed && tries < CLOSE_TRIES && self.pos(self.lanes[l].sym) != 0 && self.may_reduce().is_ok() {
                tries += 1;
                let pos = self.pos(self.lanes[l].sym);
                self.spec.cross_bps = (self.spec.cross_bps * 2).min(300);
                if let Err(e) = self.ioc(l, pos < 0, Some(pos.abs())) {
                    warn!("binance-battery: the end's close: {e}");
                }
            }
            l += 1;
        }
        // The shutdown sweep (the arm spins for it at most
        // `arm::SHUTDOWN_WAIT_NS`); the gateway thread leaves — waited for
        // `SHUTDOWN_WAIT`, or after a signal its drain share; then stopped.
        // Not after the gap drill: it stopped the gateway on purpose.
        if !self.gw_killed {
            self.d.on_shutdown();
        }
        let t0 = Instant::now();
        while self.gw.as_ref().is_some_and(|h| !h.is_finished()) {
            let budget = if crate::sigint::shutdown_requested() { SIGNAL_GW_WAIT } else { SHUTDOWN_WAIT };
            if t0.elapsed() >= budget {
                break;
            }
            self.turn();
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.gw.as_ref().is_some_and(|h| !h.is_finished()) {
            self.gw_stop.store(true, Ordering::Release);
            warn!("binance-battery: the shutdown sweep did not end the gateway in time — stopped");
            let t1 = Instant::now();
            while self.gw.as_ref().is_some_and(|h| !h.is_finished()) && t1.elapsed() < GW_STOP_WAIT {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let (gw_phase, flag) = match self.gw.take() {
            Some(h) if h.is_finished() => h.join().unwrap_or((Phase::Clock, 0)),
            Some(_) => {
                warn!("binance-battery: the gateway thread did not stop — left");
                (Phase::Clock, 0)
            }
            None => (Phase::Clock, 0),
        };
        let gateway = if self.gw_killed { String::from("stopped by the drill") } else { format!("{gw_phase:?}") };
        phase("shutdown").s("gateway", &gateway).b("sweep_done", flag & EVT_F_SWEEP_DONE != 0).emit();
        // A drill stopped the gateway: what the arm can no longer close or
        // cancel — a maker that filled before the dead-man fired, a
        // position the flatten drill left — the operator's own tool does,
        // on Demo (the arm's orders by id first; a flat account is one
        // read). The flat read then judges.
        if self.gw_killed && !crate::sigint::shutdown_requested() {
            for i in 0..self.lanes.len() {
                let spec = OpsSpec {
                    reach: Reach::Demo,
                    confirm: false,
                    hmac: self.spec.hmac,
                    product: self.lanes[i].product,
                    symbols: vec![self.lanes[i].symbol.clone()],
                    leverage: None,
                    cap: DEFAULT_LEVERAGE_CAP,
                    sign_tradfi: false,
                    reassert: false,
                    cancel_first: true,
                    close_bps: 20,
                };
                let code = flatten(&spec, self.tls.clone());
                phase("end_flatten").s("product", product_word(self.lanes[i].product)).n("exit", i64::from(code)).emit();
            }
        }
        if gw_phase != Phase::Exiting && !self.gw_killed {
            rep.fail(EXIT_SWEEP, format!("the shutdown sweep did not complete (the gateway ended {gw_phase:?})"));
        }
        self.turn();
        let c = *self.d.live().counters();
        rep.reconnects = c.gw[GW_RECONNECTS];
        rep.connect_failures = c.gw[GW_CONNECT_FAILURES];
        rep.recon_ok = c.gw[GW_RECON_OK];
        rep.recon_failed = c.gw[GW_RECON_FAILED];
        rep.journal_dropped = c.gw[GW_JOURNAL_DROPPED];
        rep.fills_lost = c.gw[GW_FILLS_LOST];
        rep.recs_dropped = self.recs_dropped;
        rep.fills_dropped = self.fills_dropped;
        // BX11 (BX-14): every trade's commission journaled once — its
        // `J_FEE` beside its fill (a fill booked without its trade has none).
        rep.fees_journaled = self.recs.iter().filter(|r| r.kind == J_FEE).count() as u64;
        rep.fills_journaled = self.recs.iter().filter(|r| r.kind == J_FILL).count() as u64;
        let log = self.tap.take().map(|tap| {
            let (log, ended) = tap.finish();
            if let Some(e) = ended {
                warn!("binance-battery: the tap ended early: {e}");
            }
            log
        });
        // The verdict first; the evidence after it, and not after a signal
        // (up to 50 000 frames read and printed: outside the drain bound).
        self.flat_read(rep);
        if let Some(log) = log {
            rep.tap_frames = log.frames.len() as u64;
            rep.tap_dropped = log.dropped;
            if crate::sigint::shutdown_requested() {
                rep.k2 = vec![String::from("K2 and the tap's lines are skipped after a signal")];
            } else {
                rep.k2 = k2(&log.frames, &self.k2_oids, &self.recs, &self.prefix);
                tap_lines(&log.frames);
            }
        }
    }

    /// **The account read flat** on the run's symbols, with no order of
    /// this run's epoch anywhere — else a leftover. Each lane's two reads
    /// are signed here (the one signer stays on this thread) and sent from
    /// the lane's own thread, the lanes in parallel (the drain bound).
    fn flat_read(&mut self, rep: &mut Report) {
        let mut ours = [0u8; CID_LEN];
        self.prefix.render(SLOT, 0, &mut ours);
        // `mv` + the epoch's 8 hex: every id this boot made.
        let epoch_prefix = String::from_utf8_lossy(&ours[..10]).into_owned();
        let mut signed = Vec::with_capacity(self.lanes.len());
        for lane in &mut self.lanes {
            signed.push(sign_flat(lane, &self.signer));
        }
        let (lanes, prefix) = (&mut self.lanes, epoch_prefix.as_str());
        let found: Vec<Vec<String>> = std::thread::scope(|sc| {
            let mut handles = Vec::with_capacity(lanes.len());
            for (lane, s) in lanes.iter_mut().zip(signed) {
                handles.push(sc.spawn(move || flat_lane(lane, s, prefix)));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| vec![String::from("a flat read's thread panicked")]))
                .collect()
        });
        for v in found {
            rep.left.extend(v);
        }
        phase("flat_read").s("left", &rep.left.join(" | ")).emit();
    }
}

/// The flat read's two requests on `lane` — the account's open orders, its
/// positions — signed into their templates' windows under
/// [`FLAT_RECV_WINDOW_MS`] (the second is sent only after the first is
/// answered): their lengths.
fn sign_flat(lane: &mut LaneB, signer: &Signer) -> Result<(usize, usize), String> {
    let long = WsStatic::new(STP_MODE, FLAT_RECV_WINDOW_MS);
    let orders = lane.rest.sign(T_OPEN_ORDERS, signer, false, |w, _, ts| render_account(w, &long, ts))?;
    let account = lane.rest.sign(ORD0 + R_ACCOUNT, signer, false, |w, _, ts| render_account(w, &long, ts))?;
    Ok((orders, account))
}

/// One lane's flat read, its requests signed ([`sign_flat`]): no order of
/// this run's epoch on the product's account, the lane's symbol flat —
/// what is left, or could not be read.
fn flat_lane(lane: &mut LaneB, signed: Result<(usize, usize), String>, epoch_prefix: &str) -> Vec<String> {
    let mut left = Vec::new();
    let pw = product_word(lane.product);
    let (orders, account) = match signed {
        Ok(n) => n,
        Err(e) => {
            left.push(format!("{pw}: the flat read could not be signed ({e})"));
            return left;
        }
    };
    match lane.rest.send(T_OPEN_ORDERS, orders).and_then(|a| orders_of(&a)) {
        Ok(v) => {
            for o in v.iter().filter(|o| o.cid.starts_with(epoch_prefix)) {
                left.push(format!("{pw}: order {} on {}", o.cid, o.symbol));
            }
        }
        Err(e) => left.push(format!("{pw}: the open orders could not be read ({e})")),
    }
    let position = lane.rest.send(ORD0 + R_ACCOUNT, account).and_then(|a| account_of(lane.product, a));
    match position.and_then(|a| a.position(lane.product, lane.symbol.as_bytes())) {
        Ok(0) => {}
        Ok(p) => left.push(format!("{}: a position of {p}", lane.symbol)),
        Err(e) => left.push(format!("{}: the position could not be read ({e})", lane.symbol)),
    }
    left
}

/// The arm's refusals that moved between two readings, by name.
fn refusal_delta(before: &[u64], after: &[u64]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (i, (b, a)) in before.iter().zip(after).enumerate() {
        if a > b {
            out.push(exec_binance::arm::REFUSAL_WORDS[i]);
        }
    }
    out
}

// -------------------------------------------------------------------------
// The account check
// -------------------------------------------------------------------------

/// **The account check** (module docs, 0) on one product's REST client.
fn account_check(rest: &mut Rest, signer: &Signer, product: u8, symbol: &str) -> Result<(), String> {
    let pw = product_word(product);
    let a = rest.signed(T_DUAL, signer, false, render_account)?;
    if a.status != 200 {
        return Err(format!("{pw}: the position mode could not be read ({})", a.text()));
    }
    judge_one_way(&a.body).map_err(|e| format!("{pw}: BX-19 one-way: {e:?} — the account must be in one-way mode"))?;
    let acct = read_account(rest, signer, product)?;
    let open = acct.open();
    if !open.is_empty() {
        return Err(format!("{pw}: the account holds positions ({}) — a dedicated account must be flat", open.join(" ")));
    }
    // The symbol's row named once (COIN-M lists every symbol: its row must
    // be there), and COIN-M's BX-19 settings on it.
    if let Some(p) = acct.row(product, symbol.as_bytes())? {
        if product == PRODUCT_COINM {
            if p.margin != MARGIN_CROSS {
                return Err(format!("{pw}: BX-19: {symbol} is not cross-margined"));
            }
            if p.leverage == 0 || p.leverage > u16::from(MAX_LEVERAGE) {
                return Err(format!("{pw}: BX-19: {symbol}'s leverage {} is not within {MAX_LEVERAGE}", p.leverage));
            }
        }
    }
    if product == PRODUCT_USDM {
        let a = rest.signed(T_MULTI_ASSETS, signer, false, render_account)?;
        judge_single_asset(&a.body).map_err(|e| format!("{pw}: BX-19 single-asset: {e:?}"))?;
        let a = rest.signed(T_SYMBOL_CONFIG, signer, false, |w, st, ts| {
            w.put(b"symbol=").put(symbol.as_bytes()).put(b"&");
            render_account(w, st, ts);
        })?;
        judge_symbol_config(&a.body, symbol.as_bytes(), MAX_LEVERAGE).map_err(|e| format!("{pw}: BX-19 {symbol}: {e:?}"))?;
    }
    // No order but the arm's: an id `cid::classify` reads as ours or as an
    // orphan of an earlier boot (the boot sweeps those). Any other — a
    // malformed `mv…` id included — would rest unseen all run.
    let foreign: Vec<String> = open_orders(rest, signer, None)?
        .into_iter()
        .filter(|o| !is_arms(&o.cid))
        .map(|o| format!("{}:{}", o.symbol, o.cid))
        .collect();
    if !foreign.is_empty() {
        return Err(format!("{pw}: orders not the arm's rest on the account ({}) — a dedicated account holds none", foreign.join(" ")));
    }
    phase("account").s("product", pw).b("flat", true).n("margin_balance_usd", acct.um.map_or(-1, |(b, _)| b / 1_000_000)).emit();
    Ok(())
}

// -------------------------------------------------------------------------
// K2 and the tap's record
// -------------------------------------------------------------------------

/// Every frame the tap kept, one line each (`ms` from the first): the
/// event, its execution type and status, our id's member part, the
/// quantities and prices — the run's venue-side record.
fn tap_lines(frames: &[(u64, Vec<u8>)]) {
    let t0 = frames.first().map_or(0, |f| f.0);
    for (t, f) in frames {
        let mut ev = UserEvent::default();
        let ok = scan_user_event(f, &mut ev).is_ok();
        let cid = ev.cid.get(f);
        phase("tap")
            .n("ms", (t.saturating_sub(t0) / 1_000_000) as i64)
            .b("scanned", ok)
            .n("kind", i64::from(ev.kind))
            .n("x", i64::from(ev.exec))
            .n("X", i64::from(ev.status))
            .s("cid_oid", &String::from_utf8_lossy(cid.get(11..27).unwrap_or_default()))
            .n("q", ev.qty_1e6)
            .n("p", ev.px_1e6)
            .n("l", ev.last_qty_1e6)
            .n("z", ev.cum_qty_1e6)
            .n("E", ev.event_ms as i64)
            .emit();
    }
}

/// **K2** per IoC, off the tap (one socket, so its two events compare
/// cleanly) and the gateway's journal: the first `TRADE_LITE` against the
/// first `ORDER_TRADE_UPDATE` trade for its id — the receipt lead (µs) and
/// the venue's own `E` lead (ms) — and whether the WS API's ACK was
/// journaled at all (an ACK landing after the stream ended the order is
/// applied to nothing), and if it was, against the `TRADE_LITE` (µs,
/// across two sockets: evidence, not proof).
fn k2(frames: &[(u64, Vec<u8>)], oids: &[u64], recs: &[ExecRecord], prefix: &CidPrefix) -> Vec<String> {
    let mut out = Vec::with_capacity(oids.len() + 1);
    let (mut lite_first, mut acks, mut heard) = (0u32, 0u32, 0u32);
    for &oid in oids {
        let mut cid = [0u8; CID_LEN];
        prefix.render(SLOT, oid, &mut cid);
        let ack = recs.iter().find(|r| r.kind == J_ACK && r.client_oid == oid).map(|r| r.ts_ns);
        let (mut lite, mut otu): (Option<(u64, u64)>, Option<(u64, u64)>) = (None, None);
        for (t, f) in frames {
            let mut ev = UserEvent::default();
            if scan_user_event(f, &mut ev).is_err() || ev.cid.get(f) != cid {
                continue;
            }
            if ev.kind == UE_TRADE_LITE && lite.is_none() {
                lite = Some((*t, ev.event_ms));
            }
            if ev.kind == UE_ORDER && ev.exec == X_TRADE && otu.is_none() {
                otu = Some((*t, ev.event_ms));
            }
        }
        acks += u32::from(ack.is_some());
        let lead = match (lite, otu) {
            (Some(l), Some(o)) => {
                heard += 1;
                lite_first += u32::from(l.0 <= o.0);
                format!("TRADE_LITE leads ORDER_TRADE_UPDATE by {} µs on receipt, {} ms by E", (o.0 as i128 - l.0 as i128) / 1_000, o.1 as i128 - l.1 as i128)
            }
            _ => format!("the tap heard TRADE_LITE {} and the trade update {}", lite.is_some(), otu.is_some()),
        };
        let ack_word = match (ack, lite) {
            (Some(a), Some(l)) => format!("ACK journaled, {} µs after the TRADE_LITE's receipt", (a as i128 - l.0 as i128) / 1_000),
            (Some(_), None) => String::from("ACK journaled"),
            (None, _) => String::from("no ACK journaled (the stream ended the order first)"),
        };
        out.push(format!("oid {oid}: {lead}; {ack_word}"));
    }
    out.push(format!(
        "{} IoCs: both events heard for {heard}, TRADE_LITE first in {lite_first}; ACK journaled for {acks}; the tap kept {} frames",
        oids.len(),
        frames.len()
    ));
    phase("k2").s("summary", out.last().map_or("", String::as_str)).emit();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BatterySpec {
        BatterySpec {
            um: true,
            cm: true,
            rest: false,
            hmac: false,
            um_symbol: String::from("BTCUSDT"),
            cm_symbol: String::from("BNBUSD_PERP"),
            cycles: 10,
            fill_pairs: 3,
            usd: 60,
            rest_bps: 300,
            cross_bps: 50,
            ttl_ms: 5_000,
            rotate_s: 0,
            drill: None,
        }
    }

    #[test]
    fn the_spec_refuses_what_the_venue_or_the_law_would() {
        assert!(spec().check().is_ok());
        // BX11: the gap drill is USDⓈ-M's on the WS API.
        let gap = BatterySpec { cm: false, drill: Some(Drill::Gap), ..spec() };
        assert!(gap.check().is_ok());
        assert!(BatterySpec { cm: true, ..gap.clone() }.check().is_err(), "COIN-M rests no maker");
        assert!(BatterySpec { rest: true, ..gap.clone() }.check().is_err(), "the WS API's");
        // A drill is its own bar: the plan's counts do not apply.
        assert_eq!(bar_short(&gap, &Report::default()), None);
        // BX11: the flatten drill takes one product, either, on either route.
        let flat = BatterySpec { drill: Some(Drill::Flatten), ..spec() };
        assert!(flat.check().is_err(), "two products");
        assert!(BatterySpec { cm: false, ..flat.clone() }.check().is_ok());
        assert!(BatterySpec { um: false, rest: true, ..flat.clone() }.check().is_ok());
        assert_eq!(bar_short(&flat, &Report::default()), None);
        assert!(BatterySpec { um: false, cm: false, ..spec() }.check().is_err(), "no product");
        assert!(BatterySpec { hmac: true, ..spec() }.check().is_err(), "HMAC on the WS API");
        assert!(BatterySpec { hmac: true, rest: true, ..spec() }.check().is_ok());
        assert!(BatterySpec { usd: MAX_USD + 1, ..spec() }.check().is_err(), "past the notional ceiling");
        assert!(BatterySpec { rest_bps: 500, ..spec() }.check().is_err(), "outside the 5 % band");
        assert!(BatterySpec { ttl_ms: 4_999, ..spec() }.check().is_err(), "under the arm's 5 s maker TTL floor");
        assert!(BatterySpec { ttl_ms: 20_001, ..spec() }.check().is_err());
    }

    /// A report as a full run on both products leaves it.
    fn full() -> Report {
        Report {
            cycles: 10,
            fills: [0, 6, 4],
            recons: [0, 6, 4],
            sweeps: 1,
            ttl: 1,
            post_only: 1,
            cm_maker_refused: true,
            ..Report::default()
        }
    }

    /// The risk review (BX7b round 1, R1-5): the bar is the plan's,
    /// absolute — a run asked for less is short of it, with the short
    /// phase's code, never a pass.
    #[test]
    fn the_bar_is_the_plans_whatever_the_spec_asked() {
        assert_eq!((PRODUCT_USDM, PRODUCT_COINM), (1, 2), "the report's arrays are indexed by product");
        assert_eq!(bar_short(&spec(), &full()), None);
        let short = |f: fn(&mut Report)| {
            let mut r = full();
            f(&mut r);
            bar_short(&spec(), &r).map(|(code, _)| code)
        };
        assert_eq!(short(|r| r.cycles = 9), Some(EXIT_CYCLE));
        assert_eq!(short(|r| r.post_only = 0), Some(EXIT_POST_ONLY));
        assert_eq!(short(|r| r.ttl = 0), Some(EXIT_TTL));
        assert_eq!(short(|r| r.fills[1] = 5), Some(EXIT_FILLS));
        assert_eq!(short(|r| r.recons[1] = 2), Some(EXIT_RECON));
        assert_eq!(short(|r| r.sweeps = 0), Some(EXIT_SWEEP));
        assert_eq!(short(|r| r.cm_maker_refused = false), Some(EXIT_FILLS));
        assert_eq!(short(|r| r.fills[2] = 3), Some(EXIT_FILLS));
        assert_eq!(short(|r| r.recons[2] = 1), Some(EXIT_RECON));
        // A product not armed is not judged.
        let mut r = full();
        r.fills[2] = 0;
        assert_eq!(bar_short(&BatterySpec { cm: false, ..spec() }, &r), None);
        // `--bn-cycles 0 --bn-fill-pairs 0`, run clean: short, exit non-zero.
        let mut r = Report::default();
        assert_eq!(summary(&BatterySpec { cycles: 0, fill_pairs: 0, ..spec() }, &mut r), EXIT_CYCLE);
        assert!(r.why.starts_with("short of the bar"), "{}", r.why);
    }

    /// The parser review (BX7b round 1, 4): the tap keeps frames within its
    /// caps and counts the rest.
    #[test]
    fn the_tap_keeps_within_its_caps_and_counts_the_rest() {
        let mut log = TapLog::default();
        log.keep(1, vec![0; TAP_BYTES_MAX - 10]);
        log.keep(2, vec![0; 11]);
        log.keep(3, vec![0; 10]);
        assert_eq!((log.frames.len(), log.bytes, log.dropped), (2, TAP_BYTES_MAX, 1));
        let mut log = TapLog::default();
        let mut i = 0;
        while i <= TAP_FRAMES_MAX {
            log.keep(i as u64, Vec::new());
            i += 1;
        }
        assert_eq!((log.frames.len(), log.dropped), (TAP_FRAMES_MAX, 1));
    }

    /// The risk review (BX7b round 1, R1-3): the account check admits only
    /// ids the arm owns or sweeps — a malformed `mv…` id is foreign.
    #[test]
    fn only_the_arms_ids_or_its_orphans_pass_the_account_check() {
        let mut id = [0u8; CID_LEN];
        CidPrefix::new(0x6512_ab0f).render(SLOT, 7, &mut id);
        let orphan = String::from_utf8_lossy(&id).into_owned();
        assert!(is_arms(&orphan), "an earlier boot's: the boot sweeps it");
        let mut bad = orphan.clone();
        bad.truncate(CID_LEN - 1);
        assert!(!is_arms(&bad), "a malformed mv… id");
        assert!(!is_arms("bxprobe1791488561x01"));
        assert!(!is_arms("web_8f3b"));
    }

    #[test]
    fn the_k2_reader_orders_the_ack_against_the_streams() {
        let prefix = CidPrefix::new(0x6512_ab0f);
        let mut cid = [0u8; CID_LEN];
        prefix.render(SLOT, 5, &mut cid);
        let cid = String::from_utf8_lossy(&cid).into_owned();
        let lite = format!(
            r#"{{"e":"TRADE_LITE","E":1,"T":1,"s":"BTCUSDT","q":"0.001","p":"0","m":false,"c":"{cid}","S":"BUY","L":"65000","l":"0.001","t":9,"i":77}}"#
        );
        let otu = format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":2,"T":2,"o":{{"s":"BTCUSDT","c":"{cid}","S":"BUY","o":"LIMIT","f":"IOC","q":"0.001","p":"65000","ap":"65000","sp":"0","x":"TRADE","X":"FILLED","i":77,"l":"0.001","z":"0.001","L":"65000","n":"0.01","N":"USDT","T":2,"t":9,"b":"0","a":"0","m":false,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"0","pP":false,"si":0,"ss":0,"V":"EXPIRE_MAKER","pm":"NONE","gtd":0}}}}"#
        );
        let frames = vec![(1_000_000u64, lite.into_bytes()), (3_000_000, otu.into_bytes())];
        let mut ack = ExecRecord {
            ts_ns: 2_000_000,
            wall_ms: 0,
            client_oid: 5,
            venue_oid: 77,
            a: 0,
            b: 0,
            c: 0,
            code: 0,
            row: 0,
            kind: J_ACK,
            slot: SLOT,
        };
        let out = k2(&frames, &[5], &[ack], &prefix);
        assert!(out[0].contains("TRADE_LITE leads ORDER_TRADE_UPDATE by 2000 µs on receipt, 1 ms by E"), "{}", out[0]);
        assert!(out[0].contains("ACK journaled, 1000 µs after the TRADE_LITE's receipt"), "{}", out[0]);
        assert!(out[1].contains("both events heard for 1, TRADE_LITE first in 1; ACK journaled for 1"), "{}", out[1]);
        ack.client_oid = 6;
        let out = k2(&frames, &[5], &[ack], &prefix);
        assert!(out[0].contains("no ACK journaled (the stream ended the order first)"), "{}", out[0]);
    }

    #[test]
    fn a_report_keeps_the_first_failure_and_a_leftover_overrides_it() {
        let mut r = Report::default();
        r.fail(EXIT_CYCLE, String::from("first"));
        r.fail(EXIT_RECON, String::from("second"));
        assert_eq!((r.exit(), r.why.as_str()), (EXIT_CYCLE, "first"));
        r.left.push(String::from("an order"));
        assert_eq!(r.exit(), EXIT_LEFTOVER);
    }
}
