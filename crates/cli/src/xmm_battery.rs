// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **XMM XH4g — the testnet battery** (`exec-smoke --hl-perp --lifecycle`):
//! slot 6's gateway arm and its gateway, composed in-process with the
//! engine's own composition pieces (`crate::xmm_live`'s binding, the arm's
//! configuration, the gateway's thread) and driven through the arm's
//! `OrderDispatch` surface as the engine loop drives it — the verbs, the
//! fill, order-event, rename and retirement queues, the idle — against
//! Hyperliquid TESTNET, signing as the E3 agent. Where it differs from the
//! engine's boot: `expiresAfter` 10 s, the minimum budget floor, no
//! pinning, no router, no mark lane. No arming switch (D1): the exec-smoke
//! wrapper is the only way in, and the binary hands this module nothing but
//! the testnet configuration.
//!
//! COPY-DOCTRINE: an operator tool run by hand against testnet, never
//! reachable from the engine loop: it allocates and copies freely (the
//! report, the tap's rows, the reads' answers).
//!
//! **No router — what stands in for it.** The router's caps and halts
//! (`CapPerp`, `MaxResting`, `ReconDrift`, `AloRejectRate`, `BudgetFloor`,
//! the gateway halt) are not on this path. Instead: the stated notional
//! bounds every order; every quote but the TTL phase's and the stall's
//! carries the gateway's 30 s TTL; growth — every place, modify and top-up,
//! behind one gate (`Run::may_grow`) — stops at the first drift, unknown
//! order of the slot or gateway halt word ([`EXIT_RECON`]), read before
//! every order and after the last; the account must hold no order of slot 6
//! at the start (`--sweep-first`: the arm's boot cancels them on the run's
//! perps); a tap row not of slot 6, or a tap that missed rows, fails the run
//! ([`EXIT_ACCOUNT`]); the end cancels everything resting, closes every
//! position the fills booked and reads the account flat ([`EXIT_LEFTOVER`]).
//! A SIGINT/SIGTERM stops growth at once and fails the run; its end is the
//! cancel-all and the flat read, inside the gateway thread's grace (no
//! closes; the worst case is asserted at compile time).
//!
//! **The run.**
//! 0. *The account* (`/info`, reads only): the perp sheet, the spot
//!    balances, the open orders (whose slot), the address budget, the mids.
//!    A perp with no mid here, or one holding a position (the arm would hold
//!    its boot), is dropped from the run; none left, short collateral, a
//!    short budget or an order of slot 6 resting (without `--sweep-first`)
//!    stop it.
//! 1. *The boot*: the arm's own — the dead man's clear, the first
//!    `userFills` snapshot, the slot's sweep, the sheet, the orders, the
//!    budget, the leverage of every bound perp — until `ready()`.
//! 2. *Cycles*, one perp each in turn, the side alternating: an ALO place
//!    (the open orders read at once: can a read lag an acknowledged order?);
//!    a modify that moves the price; one that changes only the size; a
//!    cancel through the verb; then, on a fresh quote, a modify that would
//!    cross (refused `BAD_ALO_PX` — does the venue keep the original?); a
//!    crossing ALO place (`BAD_ALO_PX`); an order with a short TTL, which the
//!    gateway cancels itself (`EXPIRED`); every `sweep_every` cycles two
//!    quotes and the cancel-all word.
//! 3. *Fills*: an IoC that crosses through the gateway's taker class, then a
//!    reduce-only IoC that closes what the fills booked (D2: no second
//!    address, so no maker fill), each priced off a mid read just before it
//!    and each leg waiting for a reconciliation that agrees — a sheet AND an
//!    orders read answered a post timeout after the leg's acknowledgement
//!    (so issued after it), both agreeing with the book.
//! 4. *The stall* (opt-in, `--stall`): an order with no TTL, then no pump
//!    past the dead man's deadline — the venue's `scheduleCancel` must
//!    cancel it (`SCHEDULED_CANCEL`): the venue half of XH-1, end to end.
//! 5. *The end*: the cancel-all word read clear, then every position the
//!    fills booked closed (a passive fill's included), the arm's shutdown,
//!    the gateway released, the account read flat in every bound perp with
//!    no order of slot 6 — or [`EXIT_LEFTOVER`], the loudest.
//!
//! **Prices (D5).** From the venue's `allMids`, at the stated distances: a
//! resting quote `rest_bps` behind the mid, a crossing one `cross_bps`
//! through it. A post-only order cannot TAKE (a cross is refused), but a
//! resting quote can be filled passively: quotes are at most 2 × `usd` and
//! live at most 30 s while the process runs (the stall's has no TTL), and a
//! passive fill is closed at the end like the IoCs'. An IoC is bounded by the stated notional at its own limit price
//! ([`MAX_USD`]) — a buy fills at or under it; a sell can fill above its
//! limit, so its size, not its notional, is what is bounded.
//!
//! **The tap (D6).** A second, read-only user stream for the master — it
//! signs nothing — records every `orderUpdates` and `userFills` frame with
//! its local receipt time: the measurements the amendments ask of XH4g
//! (a modified order's original row, the venue's status words, the
//! stream's lag behind the arm's events, the venue's clock) are read off it
//! after the run. Lags are against the battery's drain, which pauses during
//! its own blocking `/info` reads.
//!
//! Exit codes: [`EXIT_ACCOUNT`] … [`EXIT_STALL`], pinned by a test; the
//! report names every phase's verdict and whether the bar was met.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clob_dispatcher::{
    CancelAllState, GatewayArmReport, OrderDispatch, Renamed, Retired, RETIRED_CANCELED_MEMBER,
    RETIRED_CANCELED_TTL,
};
use core_fill::{ORDER_KIND_IOC, ORDER_KIND_MAKER};
use core_net::HttpsPost;
use core_time::now_ns;
use core_types::{
    CancelReq, Fill, ModifyReq, Order, OrderEvent, Price, Qty, Side, VenueId,
    EXEC_CONGESTION_THROTTLED, ORDER_EVENT_CANCELED, ORDER_EVENT_FILLED,
    ORDER_EVENT_REASON_BAD_ALO_PX, ORDER_EVENT_REASON_CANCEL_REQUESTED, ORDER_EVENT_REASON_EXPIRED,
    ORDER_EVENT_REASON_REPLACED, ORDER_EVENT_REASON_SCHEDULED_CANCEL, ORDER_EVENT_REJECTED,
    ORDER_EVENT_RESTING,
};
use exec_hyperliquid::arm::{HlGatewayArm, MIN_BUDGET_FLOOR};
use exec_hyperliquid::cloid::{self, Owner};
use exec_hyperliquid::event::{EV_CANCELED, EV_RESTING};
use exec_hyperliquid::gateway::{hl_duplex, GatewayCfg, HlGateway};
use exec_hyperliquid::orderupd::{scan_order_updates, OrderUpdate};
use exec_hyperliquid::recon::{
    fills_by_time_request, open_orders_request, perp_state_request, scan_open_orders,
    scan_perp_state, scan_spot_state, spot_state_request, OpenOrder, PerpPosition, SpotBalance,
    MAX_OPEN_ORDERS, MAX_SPOT_BALANCES,
};
use exec_hyperliquid::rules::{PerpRules, Round};
use exec_hyperliquid::userws::{scan_user_fills, UserFill};
use exec_hyperliquid::{Network, UserWs};
use rustls::ClientConfig;
use tracing::{error, info, warn};

use crate::exec_boot::XMM_SLOT;
use crate::xmm_live::{
    arm_cfg, bind_perps, spawn_gateway, GatewayStop, PerpAsk, PerpBinding, META_REQ,
};

/// The slot the battery drives (the arm's cloids carry it, E-9).
const SLOT: u8 = XMM_SLOT as u8;
/// The engine symbols the battery gives its perps (the arm's own table; no
/// universe is read).
const SYM_BASE: u32 = 6_000_000;
/// `expiresAfter` on every place and modify, ms: an order in doubt settles
/// within it and the skew.
const EXPIRES_MS: u16 = 10_000;
/// The longest notional one order may carry, USD — a typo limit, as
/// `exec-smoke --fill`'s (the IoCs are sized at their own limit price).
pub const MAX_USD: i64 = 100;
/// The smallest stated notional: the venue's $10 minimum and its rounding.
pub const MIN_USD: i64 = 12;
/// The largest stated notional: an IoC sized at the lower of its open and
/// close limits must stay under [`MAX_USD`] at the higher (a 10 % cross
/// each way is a 1.22 ratio).
pub const MAX_STATED_USD: i64 = 80;
/// `xmm.toml.example`'s `rtt_pull_ms`: the battery reports how long the
/// arm's RTT word — what the member pulls on (XH4f D8) — stood at or above
/// it.
pub const RTT_PULL_MS: u32 = 1_500;
/// The collateral the run needs at least, USD ×1e8 (two quotes, a
/// size-doubled modify and an open IoC at 3×, with room): the perp account
/// value plus the free spot USDC — measured 2026-10-03 07:47Z, a perp ALO
/// rests on the E3 account with a $0 perp value and spot USDC (exec-smoke
/// phase C on BTC); the IoCs say whether spot USDC margins a position.
const MIN_ACCOUNT_1E8: i64 = 50 * E8;
/// The address requests the run needs left at least: the floor, every
/// cycle's verbs, the dead man's re-arms, room.
const MIN_BUDGET_LEFT: i64 = 2_000;
/// How far the price modify moves the quote further from the mid, bps.
const MOVE_BPS: i64 = 50;
/// The order lifetime the gateway enforces on every quote but the TTL
/// phase's and the stall's (a battery bug strands nothing past it).
const SAFETY_TTL_MS: u64 = 30_000;
/// How long the boot may take.
const BOOT_WAIT: Duration = Duration::from_secs(120);
/// How long one verb's answer may take: a post timeout, the doubt's
/// settlement (`expiresAfter` and the skew), room.
const STEP_WAIT: Duration = Duration::from_secs(25);
/// The arm's shutdown and the gateway's join.
const JOIN_WAIT: Duration = Duration::from_secs(20);
/// The end's cancel-all wait after a SIGINT/SIGTERM: short — the arm's
/// shutdown repeats the cancel-all.
const STOP_WAIT_MS: u64 = 2_000;
const STOP_WAIT: Duration = Duration::from_millis(STOP_WAIT_MS);
// The worst case from the signal: a read in flight (`core_net`'s
// deadline), the wait and the arm's shutdown fit the gateway thread's grace
// (it stops turning then); with the gateway's join (a wait again) and the
// flat read's two reads after them, the drain alarm.
const _: () = {
    let read_ms = core_net::https_post::REQ_DEADLINE.as_millis();
    let to_shutdown = read_ms + STOP_WAIT_MS as u128 + crate::xmm_live::SHUTDOWN_MS as u128;
    assert!(to_shutdown * 1_000_000 < crate::xmm_live::GATEWAY_GRACE_NS as u128);
    assert!(
        to_shutdown + STOP_WAIT_MS as u128 + 2 * read_ms
            < crate::sigint::DRAIN_DEADLINE_S as u128 * 1_000
    );
};
/// Tries to close a position (each a fresh mid and a wider cross).
const CLOSE_TRIES: u32 = 4;
const E8: i64 = 100_000_000;
const NS_PER_MS: u64 = 1_000_000;

/// The account was not the battery's to run on: no asked perp listed and
/// flat, short collateral, a short address budget, an order of slot 6
/// resting at the start (without `--sweep-first`), the account unreadable —
/// or, at the end, a tap that could not watch it throughout, or a tap row
/// not of slot 6.
pub const EXIT_ACCOUNT: u8 = 30;
/// The arm never became ready (or its boot was held by a position).
pub const EXIT_BOOT: u8 = 31;
/// A place, modify or cancel did not end as the vocabulary says.
pub const EXIT_CYCLE: u8 = 32;
/// A crossing ALO did not read `BAD_ALO_PX`.
pub const EXIT_BAD_ALO: u8 = 33;
/// A TTL did not end `EXPIRED` and `RETIRED_CANCELED_TTL`.
pub const EXIT_TTL: u8 = 34;
/// A sweep did not read clear.
pub const EXIT_SWEEP: u8 = 35;
/// Fewer clean reconciliations than the bar, or a halt: a drift, an
/// unknown order of the slot or the gateway's halt word.
pub const EXIT_RECON: u8 = 36;
/// Fewer fills than the bar.
pub const EXIT_FILLS: u8 = 37;
/// The run left a position or an order of the slot on the account — the
/// loudest: the next boot would be held.
pub const EXIT_LEFTOVER: u8 = 38;
/// The dead man did not cancel the stalled order.
pub const EXIT_STALL: u8 = 39;

/// The bar (the session-3 brief §4): cycles, fills, clean reconciliations.
pub const BAR_CYCLES: u32 = 20;
/// Fills.
pub const BAR_FILLS: u32 = 5;
/// Clean reconciliations.
pub const BAR_RECONS: u32 = 3;

/// What the operator states.
#[derive(Clone, Debug)]
pub struct BatterySpec {
    /// The perps (`BTC`, …), 1 ..= 8.
    pub coins: Vec<String>,
    /// Lifecycle cycles.
    pub cycles: u32,
    /// IoC round trips (an open and its reduce-only close).
    pub fill_pairs: u32,
    /// Each order's notional, USD ([`MIN_USD`] ..= [`MAX_STATED_USD`]).
    pub usd: i64,
    /// A resting quote's distance behind the mid, bps (100 ..= 2 000).
    pub rest_bps: i64,
    /// A crossing order's distance through the mid, bps (50 ..= 1 000).
    pub cross_bps: i64,
    /// The TTL phase's lifetime, ms (1 000 ..= 10 000).
    pub ttl_ms: u64,
    /// A sweep every this many cycles (≥ 1).
    pub sweep_every: u32,
    /// The arm's read period, ms (≥ 1 000; the composition's is
    /// `xmm_live::RECON_MS`).
    pub recon_ms: u64,
    /// Run the stall (the dead man's firing; spends one of the day's ten
    /// triggers).
    pub stall: bool,
    /// Orders of slot 6 resting at the start are a run that did not end:
    /// let the arm's boot sweep cancel them — on the run's perps only (one
    /// on another coin holds the boot, and the flat read then finds it:
    /// exit 38) — without it, they refuse the run (they could be another
    /// slot-6 signer's).
    pub sweep_first: bool,
}

impl BatterySpec {
    /// Whether the statement holds together.
    ///
    /// # Errors
    /// The first term out of its bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.coins.is_empty() || self.coins.len() > 8 {
            return Err(format!("{} coins; 1 ..= 8", self.coins.len()));
        }
        if self.cycles == 0 || self.sweep_every == 0 {
            return Err(String::from("cycles and sweep_every must be ≥ 1"));
        }
        if !(MIN_USD..=MAX_STATED_USD).contains(&self.usd) {
            return Err(format!(
                "usd {} outside {MIN_USD} ..= {MAX_STATED_USD}",
                self.usd
            ));
        }
        if !(100..=2_000).contains(&self.rest_bps) || !(50..=1_000).contains(&self.cross_bps) {
            return Err(format!(
                "rest_bps {} (100 ..= 2 000) / cross_bps {} (50 ..= 1 000)",
                self.rest_bps, self.cross_bps
            ));
        }
        if !(1_000..=10_000).contains(&self.ttl_ms) || self.recon_ms < 1_000 {
            return Err(format!(
                "ttl_ms {} (1 000 ..= 10 000) / recon_ms {} (≥ 1 000)",
                self.ttl_ms, self.recon_ms
            ));
        }
        Ok(())
    }
}

/// One phase's verdicts.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Passed.
    pub ok: u32,
    /// Failed.
    pub bad: u32,
}

impl Tally {
    fn note(&mut self, ok: bool) {
        if ok {
            self.ok += 1;
        } else {
            self.bad += 1;
        }
    }
}

/// p50 / p90 / max of a sample, and its size.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Spread {
    /// Samples.
    pub n: usize,
    /// The smallest.
    pub min: i64,
    /// The median.
    pub p50: i64,
    /// The 90th percentile.
    pub p90: i64,
    /// The largest.
    pub max: i64,
}

impl Spread {
    fn of(v: &mut [i64]) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        let at = |q: usize| v[(v.len() - 1) * q / 100];
        Self {
            n: v.len(),
            min: v[0],
            p50: at(50),
            p90: at(90),
            max: v[v.len() - 1],
        }
    }
}

/// What the venue said, measured (the amendments' XH4g lists).
#[derive(Clone, Debug, Default)]
pub struct Measured {
    /// Landed modifies whose original got an `orderUpdates` `canceled` row
    /// on the tap, of those checked.
    pub modify_original_canceled: (u32, u32),
    /// Refused modifies whose original the venue ended (a row on the tap,
    /// or the arm's `CANCELED`) before the battery's own cancel, of those
    /// refused.
    pub refused_modify_dropped_original: (u32, u32),
    /// Open-orders reads right after an acknowledged place that did not
    /// list it, of those read.
    pub read_lagged_ack: (u32, u32),
    /// How long after the acknowledgement those reads were answered, ms.
    pub read_after_ack_ms: Spread,
    /// The tap's `open` row against the arm's `RESTING`, ms (positive: the
    /// row came later).
    pub open_row_lag_ms: Spread,
    /// The tap's `canceled` row against the arm's `CANCELED`, ms.
    pub cancel_row_lag_ms: Spread,
    /// The tap's fill row against the arm's fill, ms.
    pub fill_row_lag_ms: Spread,
    /// Local receipt wall time minus the venue's stamp, ms — the clock
    /// offset plus the one-way delay.
    pub skew_ms: Spread,
    /// The first sheet answered after a fill disagreed (the stream behind
    /// the sheet), of the fills reconciled.
    pub first_sheet_disagreed: (u32, u32),
    /// `userFillsByTime` from 00:00Z: rows, bytes, and whether the rows run
    /// oldest first.
    pub day_page: (u64, u64, bool),
    /// `userFillsByTime` over all time: rows, bytes, oldest first.
    pub all_page: (u64, u64, bool),
    /// The largest IP-weight and WebSocket-message window sums seen, and
    /// the worst congestion word.
    pub rate_peaks: (u64, u64, u64),
    /// Tap rows recorded (updates, fills) and whether the tap failed.
    pub tap_rows: (u64, u64, bool),
    /// Rows on the tap NOT of slot 6 while the battery ran — another
    /// signer of the shared account, or another slot's order filled
    /// (XH4f's obligation: none).
    pub foreign_rows: (u64, u64),
    /// The arm's end of a refused modify's original, as `kind/reason`
    /// (`ORDER_EVENT_*`), counted.
    pub dropped_original_reasons: Vec<(String, u32)>,
    /// Every `orderUpdates` status word the venue sent, counted — the
    /// vocabulary `orderupd::status_of` reads.
    pub status_words: Vec<(String, u32)>,
    /// The arm's RTT word (`ExecHealth::rtt_ms`, the member's pull): its
    /// largest value, when (ms after the arm was ready), the REST-fallback
    /// posts counted by then, and how long it stood at or above
    /// [`RTT_PULL_MS`], ms.
    pub rtt_word: (u32, u64, u64, u64),
}

/// Count `word` in `v`.
fn note_word(v: &mut Vec<(String, u32)>, word: String) {
    match v.iter_mut().find(|(w, _)| *w == word) {
        Some((_, n)) => *n += 1,
        None => v.push((word, 1)),
    }
}

/// The battery's report.
#[derive(Clone, Debug, Default)]
pub struct BatteryReport {
    /// The perps run.
    pub coins: Vec<String>,
    /// Perps dropped at the account check (a position on them).
    pub dropped: Vec<String>,
    /// The account at the check: perp value USD ×1e8, spot USDC ×1e8,
    /// budget requests left, other slots' or strangers' orders.
    pub account: (i64, i64, i64, u32),
    /// The boot, ms (0 = never ready).
    pub boot_ms: u64,
    /// Cycles run whole.
    pub cycles: Tally,
    /// The modify that would cross: refused as `BAD_ALO_PX`.
    pub modify_refused: Tally,
    /// The crossing ALO place.
    pub bad_alo: Tally,
    /// The TTL phase.
    pub ttl: Tally,
    /// The sweeps.
    pub sweeps: Tally,
    /// Clean reconciliations (and those that were not).
    pub recons: Tally,
    /// IoC legs (filled or not).
    pub iocs: Tally,
    /// Fills booked.
    pub fills: u32,
    /// Fees, USD ×1e8 (the arm's sum).
    pub fees_1e8: i64,
    /// The stall: `None` = not run (and why), `Some(ok)`.
    pub stall: Option<bool>,
    /// Why the stall did not run.
    pub stall_skipped: &'static str,
    /// The arm's numbers at the end (before its shutdown).
    pub arm: GatewayArmReport,
    /// The measurements.
    pub measured: Measured,
    /// What the run left on the account: positions, orders of the slot.
    pub leftover: (u32, u32),
    /// Failures, in order.
    pub failures: Vec<String>,
    /// The exit code: 0, or the first failing phase's.
    pub exit: u8,
    /// The XH4g bar: ≥ [`BAR_CYCLES`] whole cycles, ≥ [`BAR_FILLS`] fills,
    /// ≥ [`BAR_RECONS`] clean reconciliations — and nothing failed.
    pub bar_met: bool,
    /// The arm's drift high water at the end, USD ×1e6 (0 = never drifted).
    pub drift_hw_1e6: i64,
}

impl BatteryReport {
    fn fail(&mut self, code: u8, msg: String) {
        error!(code, "xh4g: {msg}");
        self.failures.push(msg);
        if self.exit == 0 || code == EXIT_LEFTOVER {
            self.exit = code;
        }
    }

    /// The bar met and nothing failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.exit == 0
    }

    /// One line of JSON (stdout, the exec-smoke law).
    #[must_use]
    pub fn to_json(&self) -> String {
        let m = &self.measured;
        let mut s = String::with_capacity(1_024);
        let _ = write!(
            s,
            "{{\"exit\":{},\"bar_met\":{},\"drift_hw_1e6\":{},\"halt_reason\":{},\
             \"coins\":\"{}\",\"dropped\":\"{}\",\"boot_ms\":{},\
             \"cycles_ok\":{},\"cycles_bad\":{},\"modify_refused_ok\":{},\"bad_alo_ok\":{},\
             \"ttl_ok\":{},\"sweeps_ok\":{},\"recons_ok\":{},\"recons_bad\":{},\"iocs_ok\":{},\
             \"fills\":{},\"fees_1e8\":{},\"stall\":\"{}\",\"leftover_positions\":{},\
             \"leftover_orders\":{},\"post_rtt_p50_ns\":{},\"post_rtt_p99_ns\":{},\
             \"post_rtt_max_ns\":{},\"cancel_rtt_p99_ns\":{},\"fill_lag_p99_ns\":{},\
             \"dm_accepted\":{},\"dm_refused\":{},\"dm_triggers\":{},\"leverage_dark\":{},\
             \"alo_rejects\":{},\"rejects\":{},\"unknown_orders\":{},\"read_lagged_ack\":\"{}/{}\",\
             \"modify_original_canceled\":\"{}/{}\",\"refused_modify_dropped\":\"{}/{}\",\
             \"skew_p50_ms\":{},\"foreign_rows\":\"{}/{}\",\"rtt_word_max_ms\":{},\
             \"rtt_word_above_pull_ms\":{},\"failures\":{}}}",
            self.exit,
            self.bar_met,
            self.drift_hw_1e6,
            self.arm.halt_reason,
            self.coins.join(","),
            self.dropped.join(","),
            self.boot_ms,
            self.cycles.ok,
            self.cycles.bad,
            self.modify_refused.ok,
            self.bad_alo.ok,
            self.ttl.ok,
            self.sweeps.ok,
            self.recons.ok,
            self.recons.bad,
            self.iocs.ok,
            self.fills,
            self.fees_1e8,
            match self.stall {
                None => "skipped",
                Some(true) => "fired",
                Some(false) => "FAILED",
            },
            self.leftover.0,
            self.leftover.1,
            self.arm.post_rtt_p50_ns,
            self.arm.post_rtt_p99_ns,
            self.arm.post_rtt_max_ns,
            self.arm.cancel_rtt_p99_ns,
            self.arm.fill_lag_p99_ns,
            self.arm.dm_accepted,
            self.arm.dm_refused,
            self.arm.dm_triggers,
            self.arm.leverage_dark,
            self.arm.alo_rejects,
            self.arm.rejects,
            self.arm.unknown_orders,
            m.read_lagged_ack.0,
            m.read_lagged_ack.1,
            m.modify_original_canceled.0,
            m.modify_original_canceled.1,
            m.refused_modify_dropped_original.0,
            m.refused_modify_dropped_original.1,
            m.skew_ms.p50,
            m.foreign_rows.0,
            m.foreign_rows.1,
            m.rtt_word.0,
            m.rtt_word.3,
            self.failures.len(),
        );
        s
    }
}

/// `/info`, over one keep-alive connection.
struct Info {
    http: HttpsPost,
    master: [u8; 20],
}

/// An `/info` request's room.
const INFO_REQ: usize = 512;
/// An `/info` answer's room: a full `userFillsByTime` page is ≈ 0.8 MB.
const INFO_RESP: usize = 4 * 1024 * 1024;

impl Info {
    fn new(
        host: &str,
        port: u16,
        tls: Arc<ClientConfig>,
        master: [u8; 20],
    ) -> Result<Self, String> {
        let http = HttpsPost::new(host, port, "/info", tls, INFO_REQ, INFO_RESP)
            .map_err(|e| format!("{host}/info: {e}"))?;
        Ok(Self { http, master })
    }

    /// Post `body`, hand the answer to `f` in place.
    fn read<T>(&mut self, body: &[u8], f: impl FnOnce(&[u8]) -> T) -> Result<T, String> {
        let b = self.http.body_mut();
        if body.len() > b.len() {
            return Err(String::from("/info: request too large"));
        }
        b[..body.len()].copy_from_slice(body);
        let (status, r) = self
            .http
            .post(body.len())
            .map_err(|e| format!("/info: {e}"))?;
        if status != 200 {
            return Err(format!("/info: http status {status}"));
        }
        Ok(f(&self.http.resp()[r]))
    }

    /// A per-user read rendered by `render` into a scratch buffer.
    fn user<T>(
        &mut self,
        render: fn(&mut [u8], &[u8; 20]) -> Result<usize, exec_hyperliquid::ScanErr>,
        f: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, String> {
        let mut buf = [0u8; INFO_REQ];
        let n = render(&mut buf, &self.master).map_err(|e| format!("render: {e:?}"))?;
        self.read(&buf[..n], f)
    }

    /// The perp sheet: account value ×1e8 and the positions by coin.
    fn sheet(&mut self) -> Result<(i64, Vec<(String, i64)>), String> {
        self.user(perp_state_request, |b| {
            let mut pos = vec![PerpPosition::default(); 256];
            let (value, n) = scan_perp_state(b, &mut pos).map_err(|e| format!("sheet: {e:?}"))?;
            let mut out = Vec::with_capacity(n);
            for p in &pos[..n] {
                out.push((String::from_utf8_lossy(p.coin.of(b)).to_string(), p.szi_1e8));
            }
            Ok((value, out))
        })?
    }

    /// The open orders: `(slot 6's client ids, other slots' or strangers'
    /// count)`.
    fn orders(&mut self) -> Result<(Vec<u64>, u32), String> {
        self.user(open_orders_request, |b| {
            let mut rows = vec![OpenOrder::default(); MAX_OPEN_ORDERS];
            let n = scan_open_orders(b, &mut rows).map_err(|e| format!("orders: {e:?}"))?;
            let mut ours = Vec::new();
            let mut others = 0u32;
            for r in &rows[..n] {
                match r.cloid.map(|c| cloid::decode(&c)) {
                    Some(Owner::Ours {
                        strategy_id,
                        client_oid,
                    }) if strategy_id == SLOT => ours.push(client_oid),
                    _ => others += 1,
                }
            }
            Ok((ours, others))
        })?
    }
}

/// A decimal string (`"60123.5"`) as ×1e8.
fn dec_1e8(s: &[u8]) -> Option<i64> {
    let mut int: i64 = 0;
    let mut frac: i64 = 0;
    let mut scale: i64 = E8;
    let mut dot = false;
    let mut any = false;
    for &c in s {
        match c {
            b'0'..=b'9' if !dot => {
                int = int.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
                any = true;
            }
            b'0'..=b'9' => {
                if scale > 1 {
                    scale /= 10;
                    frac += i64::from(c - b'0') * scale;
                }
                any = true;
            }
            b'.' if !dot => dot = true,
            _ => return None,
        }
    }
    if !any {
        return None;
    }
    int.checked_mul(E8)?.checked_add(frac)
}

/// `"<coin>":"<decimal>"` out of an `allMids` answer, ×1e8.
fn mid_of(body: &[u8], coin: &str) -> Option<i64> {
    let key = format!("\"{coin}\":\"");
    let k = key.as_bytes();
    let at = body.windows(k.len()).position(|w| w == k)? + k.len();
    let end = body[at..].iter().position(|&c| c == b'"')? + at;
    dec_1e8(&body[at..end])
}

/// Wall time, ms (the venue's clock: `expiresAfter`, the dead man's
/// deadline, the fills' stamps).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `needle` somewhere in `hay`.
fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// One recorded tap frame.
struct TapRow {
    at_ns: u64,
    wall_ms: u64,
    fills: bool,
    body: Vec<u8>,
}

/// The read-only user stream (D6).
struct Tap {
    rows: Arc<Mutex<Vec<TapRow>>>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Tap {
    fn start(host: &str, port: u16, tls: Arc<ClientConfig>, master: [u8; 20]) -> Self {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (r2, s2, f2, host) = (
            Arc::clone(&rows),
            Arc::clone(&stop),
            Arc::clone(&failed),
            host.to_string(),
        );
        let handle = std::thread::Builder::new()
            .name(String::from("xh4g-tap"))
            .spawn(move || {
                let mut tries = 0u32;
                while !s2.load(Ordering::Acquire) && tries < 5 {
                    tries += 1;
                    let mut ws = match UserWs::new(&host, port, Arc::clone(&tls), &master) {
                        Ok(w) => w,
                        Err(e) => {
                            warn!("xh4g tap: {e}");
                            std::thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    };
                    if let Err(e) = ws.connect() {
                        warn!("xh4g tap: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    while !s2.load(Ordering::Acquire) {
                        let r = ws.pump(Duration::from_millis(100), |p| {
                            let fills = has(p, br#""channel":"userFills""#);
                            let upd = has(p, br#""channel":"orderUpdates""#);
                            if fills || upd {
                                let row = TapRow {
                                    at_ns: now_ns(),
                                    wall_ms: now_ms(),
                                    fills,
                                    body: p.to_vec(),
                                };
                                if let Ok(mut v) = r2.lock() {
                                    v.push(row);
                                }
                            }
                        });
                        if let Err(e) = r {
                            // A gap: rows in it go unseen, so the check
                            // is incomplete — the run fails (it reconnects
                            // to keep measuring).
                            if !s2.load(Ordering::Acquire) {
                                f2.store(true, Ordering::Release);
                            }
                            warn!("xh4g tap: {e} — a gap in the tap");
                            break;
                        }
                    }
                }
                if !s2.load(Ordering::Acquire) {
                    f2.store(true, Ordering::Release);
                }
            })
            .ok();
        Self {
            rows,
            stop,
            failed,
            handle,
        }
    }

    fn finish(&mut self) -> Vec<TapRow> {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.rows
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

/// A modify the battery sent: `(sent, prev, new, landed)`.
type ModifyRec = (u64, u64, u64, bool);

/// The arm, driven as the engine drives it, and what it said.
struct Drive {
    arm: HlGatewayArm,
    events: Vec<(u64, OrderEvent)>,
    fills: Vec<(u64, Fill)>,
    renamed: Vec<(u64, Renamed)>,
    retired: Vec<(u64, Retired)>,
    report: GatewayArmReport,
    next_report_ns: u64,
    peaks: (u64, u64, u64),
    /// The RTT word: `(max ms, at ns, REST posts by then, ns at or above
    /// the pull)` — see [`Measured::rtt_word`].
    rtt: (u32, u64, u64, u64),
    /// The last turn, ns (0 = before the arm was ready: not measured).
    last_ns: u64,
    /// When the arm was ready, ns.
    ready_ns: u64,
    /// Waits end at a SIGINT/SIGTERM (the end's own waits do not).
    honor_stop: bool,
}

impl Drive {
    /// One engine iteration: the fills (the pump runs inside), the order
    /// events, the renames, the retirements, the idle; the arm's report
    /// once a second.
    fn turn(&mut self) {
        while let Some(f) = self.arm.try_next_fill() {
            self.fills.push((now_ns(), f));
        }
        let mut ev = OrderEvent::ZERO;
        while self.arm.try_next_order_event(&mut ev) {
            self.events.push((now_ns(), ev));
        }
        while let Some(r) = self.arm.try_next_renamed() {
            self.renamed.push((now_ns(), r));
        }
        while let Some(r) = self.arm.try_next_retired() {
            self.retired.push((now_ns(), r));
        }
        let _ = self.arm.on_idle();
        let now = now_ns();
        if self.last_ns != 0 {
            let w = self.arm.exec_health(SLOT, now).rtt_ms;
            if w >= RTT_PULL_MS {
                self.rtt.3 += now.saturating_sub(self.last_ns);
            }
            if w > self.rtt.0 {
                self.rtt = (
                    w,
                    now.saturating_sub(self.ready_ns),
                    self.report.rest_fallback_posts,
                    self.rtt.3,
                );
            }
            self.last_ns = now;
        }
        if now >= self.next_report_ns {
            self.next_report_ns = now + 1_000 * NS_PER_MS;
            if self.arm.gateway_arm_report(now, &mut self.report) {
                self.peaks.0 = self.peaks.0.max(self.report.rate_ip_sum);
                self.peaks.1 = self.peaks.1.max(self.report.rate_ws_sum);
                self.peaks.2 = self.peaks.2.max(self.report.congestion);
            }
        }
    }

    /// Turn until `done` or `wait` passes — or, outside the end, a
    /// SIGINT/SIGTERM.
    fn until(&mut self, wait: Duration, mut done: impl FnMut(&Self) -> bool) -> bool {
        let end = Instant::now() + wait;
        loop {
            self.turn();
            if done(self) {
                return true;
            }
            if Instant::now() >= end || (self.honor_stop && crate::sigint::shutdown_requested()) {
                return false;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Start measuring the RTT word: the arm is ready.
    fn mark_ready(&mut self) {
        let now = now_ns();
        self.ready_ns = now;
        self.last_ns = now;
    }

    /// The position the fills booked on `sym`, signed ×1e8 (buys positive)
    /// — every fill of the slot's orders, a passive one's included.
    fn position(&self, sym: u32) -> i64 {
        let mut pos = 0i64;
        for (_, f) in &self.fills {
            if f.sym == sym {
                let q = f.qty.0 * 100;
                pos += if f.side == Side::Bid { q } else { -q };
            }
        }
        pos
    }

    /// The first event of `kind` about `oid`.
    fn event(&self, oid: u64, kind: u8) -> Option<(u64, OrderEvent)> {
        self.events
            .iter()
            .find(|(_, e)| e.client_oid == oid && e.kind == kind)
            .copied()
    }

    /// The end of `oid`: its first `REJECTED`, `CANCELED` or `FILLED`.
    fn end(&self, oid: u64) -> Option<(u64, OrderEvent)> {
        self.events
            .iter()
            .find(|(_, e)| {
                e.client_oid == oid
                    && matches!(
                        e.kind,
                        ORDER_EVENT_REJECTED | ORDER_EVENT_CANCELED | ORDER_EVENT_FILLED
                    )
            })
            .copied()
    }

    /// `oid` rests or ended.
    fn settled(&self, oid: u64) -> bool {
        self.event(oid, ORDER_EVENT_RESTING).is_some() || self.end(oid).is_some()
    }

    /// The fills of `oid`: signed size ×1e8 (buys positive) and the count.
    fn filled(&self, oid: u64) -> (i64, u32) {
        let mut sz = 0i64;
        let mut n = 0u32;
        for (_, f) in &self.fills {
            if f.order_id == oid {
                let q = f.qty.0 * 100;
                sz += if f.side == Side::Bid { q } else { -q };
                n += 1;
            }
        }
        (sz, n)
    }
}

/// One bound perp, as the battery prices it.
#[derive(Copy, Clone)]
struct Perp {
    sym: u32,
    rules: PerpRules,
}

/// The battery's state.
struct Run<'a> {
    spec: &'a BatterySpec,
    info: Info,
    drive: Drive,
    perps: Vec<Perp>,
    coins: Vec<String>,
    next_oid: u64,
    modifies: Vec<ModifyRec>,
    /// How long after each acknowledgement its read was answered, ms.
    read_ack_ms: Vec<i64>,
    rep: BatteryReport,
    post_timeout_ns: u64,
    /// A halt was seen: growth stopped.
    halt_noted: bool,
}

impl Run<'_> {
    fn oid(&mut self) -> u64 {
        self.next_oid += 1;
        self.next_oid
    }

    /// A SIGINT/SIGTERM asked the run to end.
    fn stopping(&self) -> bool {
        crate::sigint::shutdown_requested()
    }

    /// Growth stops at the first drift, unknown order of the slot or
    /// gateway halt word — the router's `ReconDrift` and gateway halt,
    /// which this path does not have. The first one fails the run.
    fn halted(&mut self) -> bool {
        if self.halt_noted {
            return true;
        }
        let s = self.drive.arm.halt_signal();
        let unknown = self.drive.arm.counters().unknown_orders;
        if s.recon_drift_usd_1e6 != 0 || unknown != 0 || s.gateway_halted != 0 {
            self.halt_noted = true;
            self.rep.fail(
                EXIT_RECON,
                format!(
                    "HALT — drift {} unknown orders {unknown} gateway halted {}: growth stops",
                    s.recon_drift_usd_1e6, s.gateway_halted
                ),
            );
            return true;
        }
        false
    }

    /// Growth may go on: no stop asked, no halt seen. Every place and
    /// modify passes here; cancels never do.
    fn may_grow(&mut self) -> bool {
        !self.stopping() && !self.halted()
    }

    /// Submit `o` if growth may go on. `false`: gated or refused locally.
    fn submit(&mut self, o: &Order) -> bool {
        if !self.may_grow() {
            return false;
        }
        match self.drive.arm.submit(o) {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    oid = o.client_oid,
                    ?e,
                    "xh4g: the place was refused locally"
                );
                false
            }
        }
    }

    /// Perp `q`'s mid, read now.
    fn mid(&mut self, q: usize) -> Option<i64> {
        match self.mids() {
            Ok(m) => m.get(q).copied(),
            Err(e) => {
                warn!("xh4g: {e}");
                None
            }
        }
    }

    /// Wait until the gateway's rates no longer throttle growth (XH4e: at
    /// 80 % of a share a place is refused unsent) — at most [`STEP_WAIT`].
    fn unthrottled(&mut self) -> bool {
        self.drive.until(STEP_WAIT, |d| {
            d.arm.congestion() < u64::from(EXEC_CONGESTION_THROTTLED)
        })
    }

    fn mids(&mut self) -> Result<Vec<i64>, String> {
        let coins = self.coins.clone();
        self.info.read(br#"{"type":"allMids"}"#, |b| {
            let mut out = Vec::with_capacity(coins.len());
            for c in &coins {
                out.push(mid_of(b, c).ok_or_else(|| format!("allMids: no mid for {c}"))?);
            }
            Ok(out)
        })?
    }

    /// `mid` moved `bps` up (or down), rounded the order's way.
    fn px(&self, p: usize, mid: i64, bps: i64, up: bool) -> Option<i64> {
        let k = if up { 10_000 + bps } else { 10_000 - bps };
        let raw = i64::try_from(i128::from(mid) * i128::from(k) / 10_000).ok()?;
        self.perps[p]
            .rules
            .round_px(raw, if up { Round::Up } else { Round::Down })
    }

    /// The smallest size whose notional at `px` is the stated one and
    /// passes the venue's rules.
    fn size(&self, p: usize, px: i64) -> Option<i64> {
        let rules = self.perps[p].rules;
        let lot = rules.lot_1e8();
        let want = i128::from(self.spec.usd) * i128::from(E8) * i128::from(E8);
        let raw = i64::try_from(want / i128::from(px.max(1))).ok()?;
        let mut sz = raw / lot * lot;
        if sz < raw {
            sz += lot;
        }
        let mut k = 0;
        while rules.check(px, sz).is_err() && k < 1_000 {
            sz += lot;
            k += 1;
        }
        rules.check(px, sz).ok().map(|()| sz)
    }

    fn order(
        &self,
        p: usize,
        oid: u64,
        side: Side,
        kind: u8,
        px: i64,
        sz: i64,
        ttl_ms: u64,
    ) -> Order {
        let mut o = Order::new(
            now_ns(),
            VenueId::Hyperliquid,
            self.perps[p].sym,
            side,
            kind,
            Price(px / 100),
            Qty(sz / 100),
            oid,
        )
        .with_ttl_ns(ttl_ms * NS_PER_MS);
        o.strategy_id = SLOT;
        if kind == ORDER_KIND_MAKER {
            o = o.with_post_only();
        }
        o
    }

    /// Place an ALO and wait for it to rest. `Some(acknowledged at)`.
    fn rest(&mut self, p: usize, oid: u64, side: Side, px: i64, sz: i64, ttl: u64) -> Option<u64> {
        let o = self.order(p, oid, side, ORDER_KIND_MAKER, px, sz, ttl);
        if !self.submit(&o) {
            return None;
        }
        if !self.drive.until(STEP_WAIT, |d| d.settled(oid)) {
            return None;
        }
        self.drive.event(oid, ORDER_EVENT_RESTING).map(|(t, _)| t)
    }

    /// Cancel `oid` and wait for its end (a cleanup: an order the arm no
    /// longer holds has ended).
    fn cancel(&mut self, p: usize, oid: u64) -> bool {
        match self.cancel_verb(p, oid) {
            Some(ended) => ended,
            None => self.drive.end(oid).is_some(),
        }
    }

    /// The cancel verb on `oid`, then its end: `None` when the arm refused
    /// the verb, else whether the order ended.
    fn cancel_verb(&mut self, p: usize, oid: u64) -> Option<bool> {
        let mut req = CancelReq::new(now_ns(), VenueId::Hyperliquid, self.perps[p].sym, oid);
        req.strategy_id = SLOT;
        self.drive.arm.cancel(&req).ok()?;
        Some(self.drive.until(STEP_WAIT, |d| d.end(oid).is_some()))
    }

    /// Modify `prev` into `new`; `Some(landed)` once settled.
    fn modify(&mut self, p: usize, prev: u64, side: Side, px: i64, sz: i64) -> Option<(u64, bool)> {
        let new = self.oid();
        let o = self.order(p, new, side, ORDER_KIND_MAKER, px, sz, SAFETY_TTL_MS);
        let sent = now_ns();
        if !self.may_grow() {
            return None;
        }
        if let Err(e) = self.drive.arm.modify(&ModifyReq::new(prev, o)) {
            warn!(prev, new, ?e, "xh4g: the modify was refused locally");
            return None;
        }
        let ok = self.drive.until(STEP_WAIT, |d| {
            let landed = d.event(new, ORDER_EVENT_RESTING).is_some()
                && d.events.iter().any(|(_, e)| {
                    e.client_oid == prev
                        && e.kind == ORDER_EVENT_CANCELED
                        && e.reason == ORDER_EVENT_REASON_REPLACED
                });
            landed || d.end(new).is_some()
        });
        if !ok {
            return None;
        }
        let landed = self.drive.end(new).is_none();
        self.modifies.push((sent, prev, new, landed));
        Some((new, landed))
    }

    /// A reconciliation that agrees: a sheet and an orders read answered a
    /// post timeout after `after` (so issued after it), both agreeing, no
    /// drift, no unknown order. Returns `(clean, first sheet disagreed)`.
    fn recon(&mut self, after: u64, why: &str) -> (bool, bool) {
        let need = after + self.post_timeout_ns;
        let wait = Duration::from_millis(3 * self.spec.recon_ms) + Duration::from_secs(15);
        let mut first_bad = None;
        let ok = self.drive.until(wait, |d| {
            let r = d.arm.recon_view();
            if first_bad.is_none() && r.sheet_ns >= need {
                first_bad = Some(!r.state_ok);
            }
            r.sheet_ns >= need && r.orders_ns >= need && r.state_ok && r.orders_ok
        });
        let r = self.drive.arm.recon_view();
        let s = self.drive.arm.halt_signal();
        let clean = ok && r.drift_hw_1e6 == 0 && s.reconciled == 1 && s.recon_drift_usd_1e6 == 0;
        self.rep.recons.note(clean);
        if clean {
            info!(
                why,
                "xh4g: reconciliation CLEAN (sheet and orders agree with the book)"
            );
        } else {
            self.rep.fail(
                EXIT_RECON,
                format!(
                    "reconciliation after {why}: {r:?}, reconciled {} drift {}",
                    s.reconciled, s.recon_drift_usd_1e6
                ),
            );
        }
        (clean, first_bad.unwrap_or(false))
    }
}

/// Run the battery. The configuration must be testnet's; the key is the
/// agent's. Never panics on the venue's account: every outcome is in the
/// report.
#[must_use]
pub fn run(spec: &BatterySpec, gcfg: GatewayCfg, sk: signer_eip712::SecretKey) -> BatteryReport {
    let mut rep = BatteryReport::default();
    if let Err(e) = spec.validate() {
        rep.fail(EXIT_ACCOUNT, format!("the statement: {e}"));
        return rep;
    }
    if gcfg.network != Network::Testnet
        || (gcfg.host != exec_hyperliquid::config::HOST_TESTNET && gcfg.host != "localhost")
    {
        rep.fail(
            EXIT_ACCOUNT,
            String::from("the battery runs on TESTNET only"),
        );
        return rep;
    }
    let (host, port, tls, master) = (
        gcfg.host.clone(),
        gcfg.port,
        Arc::clone(&gcfg.tls),
        gcfg.master,
    );
    let post_timeout_ns = gcfg.post_timeout_ns;
    let mut info = match Info::new(&host, port, Arc::clone(&tls), master) {
        Ok(i) => i,
        Err(e) => {
            rep.fail(EXIT_ACCOUNT, e);
            return rep;
        }
    };

    // --- 0. The account.
    let coins = match account(spec, &mut info, &mut rep) {
        Ok(c) => c,
        Err(e) => {
            rep.fail(EXIT_ACCOUNT, e);
            return rep;
        }
    };
    rep.coins.clone_from(&coins);

    // --- 1. The boot.
    let asks: Vec<PerpAsk<'_>> = coins
        .iter()
        .enumerate()
        .map(|(i, c)| PerpAsk {
            coin: c,
            sym: SYM_BASE + i as u32,
            lot_1e6: None,
        })
        .collect();
    let mut cfg = arm_cfg(SLOT, EXPIRES_MS, spec.recon_ms, MIN_BUDGET_FLOOR);
    let mut bound = PerpBinding::new(asks.len());
    match info.read(META_REQ, |b| bind_perps(b, &asks, &mut cfg, &mut bound)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) | Err(e) => {
            rep.fail(EXIT_BOOT, e);
            return rep;
        }
    }
    let perps: Vec<Perp> = (0..cfg.n_perps)
        .map(|i| Perp {
            sym: cfg.perps[i].sym,
            rules: cfg.perps[i].rules,
        })
        .collect();
    let (eng, gw_end) = hl_duplex();
    let arm = match HlGatewayArm::new(&cfg, eng) {
        Ok(a) => a,
        Err(e) => {
            rep.fail(EXIT_BOOT, format!("the arm: {e:?}"));
            return rep;
        }
    };
    let gw = match HlGateway::new(gcfg, gw_end, sk, bound.coins) {
        Ok(g) => g,
        Err(e) => {
            rep.fail(EXIT_BOOT, format!("the gateway: {e:?}"));
            return rep;
        }
    };
    let mut tap = Tap::start(&host, port, Arc::clone(&tls), master);
    let (handle, stop) = match spawn_gateway(gw, "hl-gw-battery", None, None, None) {
        Ok(h) => h,
        Err(e) => {
            rep.fail(EXIT_BOOT, e);
            let _ = tap.finish();
            return rep;
        }
    };
    let mut run = Run {
        spec,
        info,
        drive: Drive {
            arm,
            events: Vec::with_capacity(4_096),
            fills: Vec::with_capacity(256),
            renamed: Vec::with_capacity(256),
            retired: Vec::with_capacity(1_024),
            report: GatewayArmReport::default(),
            next_report_ns: 0,
            peaks: (0, 0, 0),
            rtt: (0, 0, 0, 0),
            last_ns: 0,
            ready_ns: 0,
            honor_stop: true,
        },
        perps,
        coins,
        // Fresh client ids each run (the boot sweep cancels a leftover).
        next_oid: now_ms() << 8,
        modifies: Vec::new(),
        read_ack_ms: Vec::new(),
        rep,
        post_timeout_ns,
        halt_noted: false,
    };
    let t0 = Instant::now();
    let ready = run
        .drive
        .until(BOOT_WAIT, |d| d.arm.ready() || d.arm.boot_position());
    if run.drive.arm.boot_position() {
        run.rep.fail(
            EXIT_BOOT,
            String::from("the boot is HELD: the seeding sheet holds a position in a bound perp"),
        );
    } else if !ready {
        let mut r = GatewayArmReport::default();
        let _ = run.drive.arm.gateway_arm_report(now_ns(), &mut r);
        run.rep.fail(
            EXIT_BOOT,
            format!(
                "never ready in {BOOT_WAIT:?}: ws_up {} dm_accepted {} dm_refused {} limit_reads {} \
                 reads {} reads_failed {} leverage_dark {}",
                r.ws_up, r.dm_accepted, r.dm_refused, r.limit_reads, r.reads, r.reads_failed,
                r.leverage_dark
            ),
        );
    } else {
        run.rep.boot_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        run.drive.mark_ready();
        info!(ms = run.rep.boot_ms, coins = %run.coins.join(","), "xh4g: the arm is READY");
        // The day's spend: `reconciled` waits for it.
        let _ = run
            .drive
            .until(STEP_WAIT, |d| d.arm.halt_signal().reconciled == 1);
        cycles(&mut run);
        if spec.stall {
            stall(&mut run);
        } else {
            run.rep.stall_skipped = "not asked (--stall)";
        }
    }

    // --- 5. The end.
    finish(&mut run, &mut tap, handle, &stop);
    run.rep
}

/// Phase 0: read the account and choose the perps.
fn account(
    spec: &BatterySpec,
    info: &mut Info,
    rep: &mut BatteryReport,
) -> Result<Vec<String>, String> {
    let (value, positions) = info.sheet()?;
    let usdc = info.user(spot_state_request, |b| {
        let mut bal = vec![SpotBalance::default(); MAX_SPOT_BALANCES];
        let n = scan_spot_state(b, &mut bal).map_err(|e| format!("spot: {e:?}"))?;
        Ok::<i64, String>(
            bal[..n]
                .iter()
                .find(|x| x.coin.of(b) == b"USDC")
                .map_or(0, SpotBalance::free_1e8),
        )
    })??;
    let (ours, others) = info.orders()?;
    let (used, vlm_1e6, _) = info
        .user(exec_hyperliquid::budget::rate_limit_request, |b| {
            exec_hyperliquid::budget::scan_rate_limit(b)
        })?
        .ok_or_else(|| String::from("userRateLimit: unreadable"))?;
    let cap = i64::try_from(exec_hyperliquid::INITIAL_BUFFER).unwrap_or(0) + vlm_1e6 / 1_000_000;
    let left = cap - i64::try_from(used).unwrap_or(i64::MAX);
    rep.account = (value, usdc, left, others);
    info!(
        perp_value_usd = value / E8,
        spot_usdc = usdc / E8,
        budget_left = left,
        slot6_orders = ours.len(),
        other_orders = others,
        positions = positions.len(),
        "xh4g: the account"
    );
    let mids = info.read(br#"{"type":"allMids"}"#, <[u8]>::to_vec)?;
    let mut coins = Vec::with_capacity(spec.coins.len());
    for c in &spec.coins {
        if mid_of(&mids, c).is_none() {
            warn!(coin = %c, "xh4g: no mid on this venue (not listed here) — dropped from the run");
            rep.dropped.push(c.clone());
            continue;
        }
        match positions.iter().find(|(k, _)| k == c) {
            Some((_, szi)) if *szi != 0 => {
                warn!(coin = %c, szi_1e8 = szi, "xh4g: a position — dropped from the run");
                rep.dropped.push(c.clone());
            }
            _ => coins.push(c.clone()),
        }
    }
    if coins.is_empty() {
        return Err(String::from(
            "no asked perp is listed here and flat on the account (a position would hold the \
             arm's boot)",
        ));
    }
    if value.saturating_add(usdc) < MIN_ACCOUNT_1E8 {
        return Err(format!(
            "perp account value ${} + spot USDC ${} < ${}: a usdClassTransfer spot→perp (the \
             master's signature) or the faucet",
            value / E8,
            usdc / E8,
            MIN_ACCOUNT_1E8 / E8
        ));
    }
    if !ours.is_empty() && !spec.sweep_first {
        return Err(format!(
            "{} order(s) of slot 6 rest on the account (client ids {:?}…): another slot-6 \
             signer, or a run that did not end — `--sweep-first` lets the arm's boot cancel \
             them (on the run's perps)",
            ours.len(),
            &ours[..ours.len().min(4)]
        ));
    }
    if !ours.is_empty() {
        warn!(
            n = ours.len(),
            "xh4g: slot-6 orders rest — the arm's boot cancels them (--sweep-first)"
        );
    }
    if left < MIN_BUDGET_LEFT {
        return Err(format!(
            "the address budget has {left} requests left (< {MIN_BUDGET_LEFT})"
        ));
    }
    if others > 0 {
        warn!(
            others,
            "xh4g: other slots' or strangers' orders rest on the account — the dead man stays \
             unarmed (scheduleCancel is account-wide)"
        );
    }
    Ok(coins)
}

/// Phases 2 and 3.
fn cycles(run: &mut Run<'_>) {
    let n = run.perps.len();
    let total = run.spec.cycles;
    let pairs = run.spec.fill_pairs;
    // The fill round trips, spread over the cycles.
    let every = (total / pairs.max(1)).max(1);
    let mut pairs_done = 0u32;
    let mut k = 0u32;
    while k < total && run.rep.exit != EXIT_LEFTOVER && !run.stopping() && !run.halted() {
        let p = (k as usize) % n;
        let side = if k % 2 == 0 { Side::Bid } else { Side::Ask };
        let mids = match run.mids() {
            Ok(m) => m,
            Err(e) => {
                run.rep.fail(EXIT_CYCLE, format!("cycle {k}: {e}"));
                k += 1;
                continue;
            }
        };
        let _ = run.unthrottled();
        let ok = one_cycle(run, k, p, side, mids[p]);
        run.rep.cycles.note(ok);
        if k % run.spec.sweep_every == run.spec.sweep_every - 1 {
            sweep(run, k, &mids);
        }
        if pairs_done < pairs && k % every == every - 1 && run.may_grow() {
            // The perps in turn, one round trip each; one that does not
            // fill (no liquidity within the cross) passes to the next.
            let mut tried = 0usize;
            while tried < n && run.may_grow() {
                let q = (pairs_done as usize + tried) % n;
                if round_trip(run, k, q, pairs_done % 2 == 0) {
                    pairs_done += 1;
                    break;
                }
                tried += 1;
            }
        }
        k += 1;
    }
    // The remaining round trips, if the cycles ended first.
    let mut extra = 0u32;
    while pairs_done < pairs
        && extra < 4 * pairs
        && run.rep.exit != EXIT_LEFTOVER
        && !run.stopping()
        && !run.halted()
    {
        let q = (extra as usize) % n;
        if round_trip(run, total + extra, q, pairs_done % 2 == 0) {
            pairs_done += 1;
        }
        extra += 1;
    }
    // A halt in the last turn (the loops test it only before each).
    let _ = run.halted();
}

/// One lifecycle cycle on perp `p`. `true` when the place, both modifies
/// and the cancel ended as the vocabulary says.
fn one_cycle(run: &mut Run<'_>, k: u32, p: usize, side: Side, mid: i64) -> bool {
    let buy = side == Side::Bid;
    let (Some(px1), Some(px2), Some(cross)) = (
        run.px(p, mid, run.spec.rest_bps, !buy),
        run.px(p, mid, run.spec.rest_bps + MOVE_BPS, !buy),
        run.px(p, mid, run.spec.cross_bps, buy),
    ) else {
        run.rep
            .fail(EXIT_CYCLE, format!("cycle {k}: no price from mid {mid}"));
        return false;
    };
    let Some(sz) = run.size(p, px2.min(px1)) else {
        run.rep
            .fail(EXIT_CYCLE, format!("cycle {k}: no size at {px1}"));
        return false;
    };

    // The place, and a read of the open orders at once (XH4d: can a read
    // lag an acknowledged order?).
    let a = run.oid();
    let Some(acked) = run.rest(p, a, side, px1, sz, SAFETY_TTL_MS) else {
        run.rep.fail(
            EXIT_CYCLE,
            format!("cycle {k}: place {a} never rested: {:?}", run.drive.end(a)),
        );
        return false;
    };
    match run.info.orders() {
        Ok((ours, _)) => {
            let m = &mut run.rep.measured;
            m.read_lagged_ack.1 += 1;
            if !ours.contains(&a) {
                m.read_lagged_ack.0 += 1;
            }
            let ms = (now_ns().saturating_sub(acked) / NS_PER_MS) as i64;
            run.read_ack_ms.push(ms);
        }
        Err(e) => warn!("xh4g: the read after the ack: {e}"),
    }

    // The price modify, then the size-only one.
    let b = match run.modify(p, a, side, px2, sz) {
        Some((b, true)) => b,
        other => {
            run.rep.fail(
                EXIT_CYCLE,
                format!("cycle {k}: price modify of {a}: {other:?}"),
            );
            let _ = run.cancel(p, a);
            return false;
        }
    };
    let c = match run.modify(p, b, side, px2, 2 * sz) {
        Some((c, true)) => c,
        other => {
            run.rep.fail(
                EXIT_CYCLE,
                format!("cycle {k}: size modify of {b}: {other:?}"),
            );
            let _ = run.cancel(p, b);
            return false;
        }
    };

    // The cancel, through the verb: `CANCELED(CANCEL_REQUESTED)`, retired
    // `RETIRED_CANCELED_MEMBER`.
    let cancelled = run.cancel_verb(p, c) == Some(true)
        && run.drive.end(c).is_some_and(|(_, e)| {
            e.kind == ORDER_EVENT_CANCELED && e.reason == ORDER_EVENT_REASON_CANCEL_REQUESTED
        })
        && run.drive.until(STEP_WAIT, |d| {
            d.retired
                .iter()
                .any(|(_, r)| r.client_oid == c && r.why == RETIRED_CANCELED_MEMBER)
        });
    if !cancelled {
        run.rep.fail(
            EXIT_CYCLE,
            format!("cycle {k}: the cancel of {c}: {:?}", run.drive.end(c)),
        );
    }

    if !run.may_grow() {
        return cancelled;
    }
    refused_modify(run, k, p, side, px1, cross, sz);

    // A crossing ALO place: BAD_ALO_PX (XH-7: information, not a fault).
    if !run.may_grow() {
        return cancelled;
    }
    let e = run.oid();
    let o = run.order(p, e, side, ORDER_KIND_MAKER, cross, sz, SAFETY_TTL_MS);
    let bad = run.submit(&o)
        && run.drive.until(STEP_WAIT, |d| d.settled(e))
        && run.drive.end(e).is_some_and(|(_, ev)| {
            ev.kind == ORDER_EVENT_REJECTED && ev.reason == ORDER_EVENT_REASON_BAD_ALO_PX
        });
    run.rep.bad_alo.note(bad);
    if !bad {
        run.rep.fail(
            EXIT_BAD_ALO,
            format!("cycle {k}: crossing ALO {e}: {:?}", run.drive.end(e)),
        );
        if run.drive.end(e).is_none() {
            let _ = run.cancel(p, e);
        }
    }

    // The TTL: the gateway cancels it itself; the end reads EXPIRED.
    if !run.may_grow() {
        return cancelled;
    }
    let f = run.oid();
    let ttl_ok = run.rest(p, f, side, px1, sz, run.spec.ttl_ms).is_some()
        && run
            .drive
            .until(Duration::from_millis(run.spec.ttl_ms) + STEP_WAIT, |d| {
                d.end(f).is_some() && d.retired.iter().any(|(_, r)| r.client_oid == f)
            })
        && run.drive.end(f).is_some_and(|(_, ev)| {
            ev.kind == ORDER_EVENT_CANCELED && ev.reason == ORDER_EVENT_REASON_EXPIRED
        })
        && run
            .drive
            .retired
            .iter()
            .any(|(_, r)| r.client_oid == f && r.why == RETIRED_CANCELED_TTL);
    run.rep.ttl.note(ttl_ok);
    if !ttl_ok {
        run.rep.fail(
            EXIT_TTL,
            format!("cycle {k}: TTL order {f}: {:?}", run.drive.end(f)),
        );
        if run.drive.end(f).is_none() {
            let _ = run.cancel(p, f);
        }
    }
    info!(cycle = k, coin = %run.coins[p], ?side, cancelled, bad, ttl_ok, "xh4g: cycle");
    cancelled
}

/// On a fresh quote, a modify that would cross: refused `BAD_ALO_PX`. Does
/// the venue keep the original (XH4d)? The arm says within the gone window;
/// an original still standing is cancelled.
fn refused_modify(run: &mut Run<'_>, k: u32, p: usize, side: Side, px: i64, cross: i64, sz: i64) {
    if !run.may_grow() {
        return;
    }
    let d = run.oid();
    if run.rest(p, d, side, px, sz, SAFETY_TTL_MS).is_none() {
        run.rep.modify_refused.note(false);
        run.rep.fail(
            EXIT_CYCLE,
            format!("cycle {k}: the quote {d} for the crossing modify never rested"),
        );
        return;
    }
    match run.modify(p, d, side, cross, sz) {
        Some((e, false)) => {
            let refused = run
                .drive
                .end(e)
                .is_some_and(|(_, x)| x.reason == ORDER_EVENT_REASON_BAD_ALO_PX);
            run.rep.modify_refused.note(refused);
            if !refused {
                run.rep.fail(
                    EXIT_CYCLE,
                    format!(
                        "cycle {k}: the crossing modify {e} ended {:?}",
                        run.drive.end(e)
                    ),
                );
            }
            let _ = run
                .drive
                .until(Duration::from_secs(3), |dr| dr.end(d).is_some());
            let end = run.drive.end(d);
            let m = &mut run.rep.measured;
            m.refused_modify_dropped_original.1 += 1;
            if let Some((_, x)) = end {
                m.refused_modify_dropped_original.0 += 1;
                note_word(
                    &mut m.dropped_original_reasons,
                    format!("{}/{}", x.kind, x.reason),
                );
            }
        }
        Some((e, true)) => {
            run.rep.modify_refused.note(false);
            run.rep.fail(
                EXIT_CYCLE,
                format!("cycle {k}: the crossing modify {e} RESTED"),
            );
            let _ = run.cancel(p, e);
        }
        None => {
            run.rep.modify_refused.note(false);
            run.rep.fail(
                EXIT_CYCLE,
                format!("cycle {k}: the crossing modify of {d} never settled"),
            );
        }
    }
    if run.drive.end(d).is_none() && !run.cancel(p, d) {
        run.rep
            .fail(EXIT_CYCLE, format!("cycle {k}: cancel of {d} never ended"));
    }
}

/// The cancel-all word over two quotes (one each side).
fn sweep(run: &mut Run<'_>, k: u32, mids: &[i64]) {
    if !run.may_grow() {
        return;
    }
    let p = (k as usize) % run.perps.len();
    let mid = mids[p];
    let (Some(bid), Some(ask)) = (
        run.px(p, mid, run.spec.rest_bps, false),
        run.px(p, mid, run.spec.rest_bps, true),
    ) else {
        run.rep.sweeps.note(false);
        return;
    };
    let Some(sz) = run.size(p, bid) else {
        run.rep.sweeps.note(false);
        return;
    };
    let (g, h) = (run.oid(), run.oid());
    let rested = run.rest(p, g, Side::Bid, bid, sz, SAFETY_TTL_MS).is_some()
        & run.rest(p, h, Side::Ask, ask, sz, SAFETY_TTL_MS).is_some();
    let _ = run.drive.arm.cancel_all();
    let clear = run.drive.until(STEP_WAIT + STEP_WAIT, |d| {
        d.arm.cancel_all_state() == CancelAllState::Clear
            && d.end(g).is_some()
            && d.end(h).is_some()
    });
    let ok = rested && clear;
    run.rep.sweeps.note(ok);
    if ok {
        info!(cycle = k, "xh4g: sweep CLEAR");
    } else {
        run.rep.fail(
            EXIT_SWEEP,
            format!(
                "cycle {k}: sweep: rested {rested}, state {:?}, ends {:?} {:?}",
                run.drive.arm.cancel_all_state(),
                run.drive.end(g),
                run.drive.end(h)
            ),
        );
    }
}

/// One IoC round trip on perp `q`: open (`long` or short) off a mid read
/// now, reconcile, close what the fills booked, reconcile. `true` when it
/// filled (a close that fails is the run's [`EXIT_LEFTOVER`]).
fn round_trip(run: &mut Run<'_>, k: u32, q: usize, long: bool) -> bool {
    if !run.may_grow() {
        return false;
    }
    let Some(mid) = run.mid(q) else {
        return false;
    };
    let (Some(open_px), Some(close_px)) = (
        run.px(q, mid, run.spec.cross_bps, long),
        run.px(q, mid, run.spec.cross_bps, !long),
    ) else {
        return false;
    };
    // Sized at the lower of the two limits, so the close passes $10 too.
    let Some(sz) = run.size(q, open_px.min(close_px)) else {
        return false;
    };
    if i128::from(sz) * i128::from(open_px.max(close_px)) / i128::from(E8)
        > i128::from(MAX_USD) * i128::from(E8)
    {
        run.rep.fail(
            EXIT_FILLS,
            format!("round trip {k}: {sz} would exceed ${MAX_USD}"),
        );
        return false;
    }
    let side = if long { Side::Bid } else { Side::Ask };
    let _ = run.unthrottled();
    let i = run.oid();
    let o = run.order(q, i, side, ORDER_KIND_IOC, open_px, sz, 0);
    if !run.submit(&o) {
        return false;
    }
    let ended = run.drive.until(STEP_WAIT, |d| d.end(i).is_some());
    // Fills of a partial IoC trail its answer by the join window.
    let _ = run.drive.until(Duration::from_secs(2), |_| false);
    let (got, nf) = run.drive.filled(i);
    run.rep.iocs.note(ended && nf > 0);
    if nf == 0 {
        warn!(round = k, coin = %run.coins[q], end = ?run.drive.end(i), "xh4g: the IoC did not fill");
        return false;
    }
    info!(round = k, coin = %run.coins[q], fills = nf, sz_1e8 = got, "xh4g: opened");
    let at = run.drive.end(i).map_or_else(now_ns, |(t, _)| t);
    let (_, lagged) = run.recon(at, "the open");
    note_first_sheet(run, lagged);
    if !close(run, "a round trip", q) {
        return true;
    }
    info!(round = k, coin = %run.coins[q], "xh4g: closed");
    let (_, lagged) = run.recon(now_ns(), "the close");
    note_first_sheet(run, lagged);
    true
}

/// Close perp `q`'s position as the fills booked it: reduce-only IoCs, each
/// off a mid read just before it, the cross widening each try. A position
/// under the venue's $10 at the close's price — which the arm refuses
/// locally (LAW XH-4) — is topped up first through the same taker class,
/// never on the last try and never under a halt (a top-up grows). A
/// reduce-only close only shrinks: it goes out under a drift or an unknown
/// order too (the gateway's own halt word refuses every place, a close
/// included), never under a stop (the end is short). `true` when flat; a
/// position left is the run's [`EXIT_LEFTOVER`].
fn close(run: &mut Run<'_>, why: &str, q: usize) -> bool {
    let sym = run.perps[q].sym;
    let mut tries = 0u32;
    while run.drive.position(sym) != 0 && tries < CLOSE_TRIES && !run.stopping() {
        tries += 1;
        let pos = run.drive.position(sym);
        let long_now = pos > 0;
        let Some(mid) = run.mid(q) else {
            continue;
        };
        // A signal during the read: nothing more goes out.
        if run.stopping() {
            break;
        }
        let bps = run.spec.cross_bps * i64::from(tries.min(3));
        let (Some(px), Some(up_px)) = (
            run.px(q, mid, bps, !long_now),
            run.px(q, mid, bps, long_now),
        ) else {
            break;
        };
        let held = pos.abs();
        let _ = run.unthrottled();
        let j = run.oid();
        let sent = if run.perps[q].rules.check(px, held).is_err() {
            if tries == CLOSE_TRIES || !run.may_grow() {
                break;
            }
            let Some(up) = run.size(q, up_px) else {
                break;
            };
            warn!(why, held, "xh4g: below $10 to close — topping up first");
            let side = if long_now { Side::Bid } else { Side::Ask };
            let o = run.order(q, j, side, ORDER_KIND_IOC, up_px, up, 0);
            run.submit(&o)
        } else {
            let side = if long_now { Side::Ask } else { Side::Bid };
            let o = run
                .order(q, j, side, ORDER_KIND_IOC, px, held, 0)
                .with_reduce_only();
            run.drive.arm.submit(&o).is_ok()
        };
        if !sent {
            warn!(why, "xh4g: the close was refused locally");
            continue;
        }
        let _ = run.drive.until(STEP_WAIT, |d| d.end(j).is_some());
        let _ = run.drive.until(Duration::from_secs(2), |_| false);
        let (_, nf) = run.drive.filled(j);
        run.rep.iocs.note(nf > 0);
    }
    let left = run.drive.position(sym);
    if left != 0 {
        run.rep.fail(
            EXIT_LEFTOVER,
            format!("{why}: {} still holds {left} ×1e-8", run.coins[q]),
        );
        return false;
    }
    true
}

fn note_first_sheet(run: &mut Run<'_>, lagged: bool) {
    let m = &mut run.rep.measured;
    m.first_sheet_disagreed.1 += 1;
    m.first_sheet_disagreed.0 += u32::from(lagged);
}

/// Phase 4: no pump past the dead man's deadline; the venue cancels.
fn stall(run: &mut Run<'_>) {
    if run.rep.exit == EXIT_LEFTOVER || run.halted() || run.stopping() {
        run.rep.stall_skipped = "the run already failed loudly, halted or was stopped";
        return;
    }
    if run.drive.arm.deadman().counters().accepted == 0 {
        run.rep.stall_skipped = "the dead man was never accepted (not eligible, or unarmed)";
        return;
    }
    if run.rep.account.3 > 0 {
        run.rep.stall_skipped = "other orders rest on the shared account";
        return;
    }
    let Ok(mids) = run.mids() else {
        run.rep.stall_skipped = "no mid";
        return;
    };
    let Some(px) = run.px(0, mids[0], run.spec.rest_bps, false) else {
        run.rep.stall_skipped = "no price";
        return;
    };
    let Some(sz) = run.size(0, px) else {
        run.rep.stall_skipped = "no size";
        return;
    };
    let s = run.oid();
    if run.rest(0, s, Side::Bid, px, sz, 0).is_none() {
        run.rep.stall_skipped = "the order did not rest";
        return;
    }
    // Let the switch take the order into its schedule, then stall.
    let _ = run.drive.until(Duration::from_secs(6), |_| false);
    let deadline = run.drive.arm.deadman().deadline_ms();
    if deadline == 0 {
        run.rep.stall_skipped = "no deadline held";
        let _ = run.cancel(0, s);
        return;
    }
    let wake = deadline + 5_000;
    info!(
        deadline,
        "xh4g: STALL — no pump until the dead man's deadline has passed"
    );
    while now_ms() < wake && now_ms() < deadline + 40_000 {
        if run.stopping() {
            // The end's cancel-all clears the order.
            run.rep.stall_skipped = "a signal ended the stall";
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let fired = run.drive.until(STEP_WAIT, |d| d.end(s).is_some())
        && run
            .drive
            .end(s)
            .is_some_and(|(_, e)| e.reason == ORDER_EVENT_REASON_SCHEDULED_CANCEL);
    if !fired && run.stopping() {
        run.rep.stall_skipped = "a signal ended the stall";
        return;
    }
    run.rep.stall = Some(fired);
    if fired {
        info!("xh4g: the dead man FIRED — the venue cancelled the stalled order");
    } else {
        run.rep.fail(
            EXIT_STALL,
            format!("the stalled order {s}: {:?}", run.drive.end(s)),
        );
        if run.drive.end(s).is_none() {
            let _ = run.cancel(0, s);
        }
    }
}

/// Phase 5: clear, close, read flat, shut the arm down, release the
/// gateway, read the tap. Under a signal the gateway thread turns 18 s more
/// at most, so the end is short: the cancel-all and the flat read — no
/// closes, no reconciliation, no pages (a position open at the signal is
/// left: [`EXIT_LEFTOVER`]).
fn finish(run: &mut Run<'_>, tap: &mut Tap, handle: JoinHandle<()>, stop: &GatewayStop) {
    if run.drive.arm.ready() {
        // Nothing resting first: a quote filled during the closes would
        // leave a position they never saw.
        let _ = run.drive.arm.cancel_all();
        let cleared = |d: &Drive| d.arm.cancel_all_state() == CancelAllState::Clear;
        // The long wait, cut by a signal; then the stop's short one.
        let mut clear = run.drive.until(STEP_WAIT + STEP_WAIT, cleared);
        if !clear && run.stopping() {
            run.drive.honor_stop = false;
            clear = run.drive.until(STOP_WAIT, cleared);
            run.drive.honor_stop = true;
        }
        if !clear {
            run.rep.fail(
                EXIT_SWEEP,
                String::from("the final cancel-all never read clear"),
            );
        }
        // Every position the fills booked — a passive fill's included.
        let mut q = 0usize;
        while q < run.perps.len() && !run.stopping() {
            if run.drive.position(run.perps[q].sym) != 0 {
                let _ = close(run, "the end", q);
            }
            q += 1;
        }
        if !run.stopping() && run.rep.recons.ok < BAR_RECONS {
            let _ = run.recon(now_ns(), "the end");
        }
        // A halt in the last turns, the end's reconciliation included.
        let _ = run.halted();
    }
    if run.stopping() {
        // Incomplete, whatever it reached: never a pass.
        run.rep.fail(
            EXIT_CYCLE,
            String::from(
                "STOPPED by a signal — the end was short (no closes, no reconciliation, no \
                 pages)",
            ),
        );
    }
    run.rep.fills = u32::try_from(run.drive.fills.len()).unwrap_or(u32::MAX);
    run.rep.measured.rate_peaks = run.drive.peaks;
    let (w, at, rest, above) = run.drive.rtt;
    run.rep.measured.rtt_word = (w, at / NS_PER_MS, rest, above / NS_PER_MS);
    // The arm's shutdown, then its numbers — the shutdown's counters in.
    run.drive.arm.on_shutdown();
    let _ = run.drive.arm.gateway_arm_report(now_ns(), &mut run.rep.arm);
    run.rep.fees_1e8 = run.rep.arm.fees_1e8;
    // The drift high water over the whole run, the shutdown's included (it
    // only rises).
    run.rep.drift_hw_1e6 = run.drive.arm.recon_view().drift_hw_1e6;
    if run.rep.drift_hw_1e6 != 0 {
        run.rep.fail(
            EXIT_RECON,
            format!(
                "the drift high water at the end: {} ×1e-6 USD",
                run.rep.drift_hw_1e6
            ),
        );
    }
    stop.release();
    // Under a stop the join is short: the flat read below uses the
    // battery's own connection, not the gateway's.
    let join = if run.stopping() { STOP_WAIT } else { JOIN_WAIT };
    let t = Instant::now();
    while !handle.is_finished() && t.elapsed() < join {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        warn!("xh4g: the gateway thread did not stop in {join:?}");
    }

    // The account, read again: flat in every bound perp, nothing of ours.
    match (run.info.sheet(), run.info.orders()) {
        (Ok((_, positions)), Ok((ours, _))) => {
            let held = positions
                .iter()
                .filter(|(c, szi)| *szi != 0 && run.coins.contains(c))
                .count() as u32;
            run.rep.leftover = (held, u32::try_from(ours.len()).unwrap_or(u32::MAX));
            if held > 0 || !ours.is_empty() {
                run.rep.fail(
                    EXIT_LEFTOVER,
                    format!(
                        "LEFT ON THE ACCOUNT: {held} position(s), {} order(s) of slot 6",
                        ours.len()
                    ),
                );
            }
        }
        (a, b) => run.rep.fail(
            EXIT_LEFTOVER,
            format!("the final account read: {:?} {:?}", a.err(), b.err()),
        ),
    }

    // The day's pages (XH4e's list): their size, their order, their bytes
    // — not under a stop (the alarm's 30 s).
    if !run.stopping() {
        let day0 = now_ms() / 86_400_000 * 86_400_000;
        run.rep.measured.day_page = page(&mut run.info, day0);
        run.rep.measured.all_page = page(&mut run.info, 0);
    }

    let started = tap.handle.is_some();
    let rows = tap.finish();
    measure(run, &rows, !started || tap.failed.load(Ordering::Acquire));
    bars(run);
}

/// One `userFillsByTime` page from `start_ms` to now: `(rows, bytes, oldest
/// first)`.
fn page(info: &mut Info, start_ms: u64) -> (u64, u64, bool) {
    let mut buf = [0u8; INFO_REQ];
    let Ok(n) = fills_by_time_request(&mut buf, &info.master, start_ms, now_ms()) else {
        return (0, 0, false);
    };
    info.read(&buf[..n], |b| {
        let mut times = Vec::new();
        let k = b"\"time\":";
        let mut i = 0usize;
        while let Some(at) = b[i..].windows(k.len()).position(|w| w == k) {
            let s = i + at + k.len();
            let e = b[s..]
                .iter()
                .position(|c| !c.is_ascii_digit())
                .map_or(b.len(), |x| s + x);
            if let Ok(t) = std::str::from_utf8(&b[s..e]).unwrap_or("").parse::<u64>() {
                times.push(t);
            }
            i = e.max(s + 1).min(b.len());
            if i >= b.len() {
                break;
            }
        }
        let oldest_first = times.windows(2).all(|w| w[0] <= w[1]);
        (times.len() as u64, b.len() as u64, oldest_first)
    })
    .unwrap_or((0, 0, false))
}

/// The tap's measurements.
fn measure(run: &mut Run<'_>, rows: &[TapRow], failed: bool) {
    let mut updates: Vec<(u64, OrderUpdate, u64)> = Vec::new();
    let mut fills: Vec<(u64, UserFill)> = Vec::new();
    let mut skew = Vec::new();
    let mut words = Vec::new();
    let mut nu = 0u64;
    let mut nf = 0u64;
    let mut buf_u = vec![OrderUpdate::default(); 256];
    let mut buf_f = vec![UserFill::default(); 2_048];
    for r in rows {
        if r.fills {
            nf += 1;
            if let Ok((n, snap)) = scan_user_fills(&r.body, &mut buf_f) {
                if snap {
                    continue;
                }
                for f in &buf_f[..n] {
                    skew.push(r.wall_ms as i64 - f.time_ms as i64);
                    fills.push((r.at_ns, *f));
                }
            }
        } else {
            nu += 1;
            let k = br#""status":""#;
            let mut i = 0usize;
            while let Some(at) = r.body[i..].windows(k.len()).position(|w| w == k) {
                let s = i + at + k.len();
                let Some(e) = r.body[s..].iter().position(|&c| c == b'"') else {
                    break;
                };
                words.push(String::from_utf8_lossy(&r.body[s..s + e]).to_string());
                i = s + e;
            }
            if let Ok(n) = scan_order_updates(&r.body, &mut buf_u) {
                for u in &buf_u[..n] {
                    skew.push(r.wall_ms as i64 - u.status_ms as i64);
                    updates.push((r.at_ns, *u, r.wall_ms));
                }
            }
        }
    }
    let ours = |c: &Option<[u8; 16]>| match c.map(|x| cloid::decode(&x)) {
        Some(Owner::Ours {
            strategy_id,
            client_oid,
        }) if strategy_id == SLOT => Some(client_oid),
        _ => None,
    };
    let m = &mut run.rep.measured;
    m.tap_rows = (nu, nf, failed);
    for w in words {
        note_word(&mut m.status_words, w);
    }
    m.foreign_rows = (
        updates
            .iter()
            .filter(|(_, u, _)| ours(&u.cloid).is_none())
            .count() as u64,
        fills
            .iter()
            .filter(|(_, f)| ours(&f.cloid).is_none())
            .count() as u64,
    );
    let foreign = m.foreign_rows;
    m.skew_ms = Spread::of(&mut skew);
    // A landed modify's original: a `canceled` row on the tap?
    for &(sent, prev, _, landed) in &run.modifies {
        if !landed {
            continue;
        }
        m.modify_original_canceled.1 += 1;
        if updates
            .iter()
            .any(|(at, u, _)| *at >= sent && u.kind == EV_CANCELED && ours(&u.cloid) == Some(prev))
        {
            m.modify_original_canceled.0 += 1;
        }
    }
    // The rows' lag behind the arm's events.
    let mut open = Vec::new();
    let mut canceled = Vec::new();
    for (t, e) in &run.drive.events {
        let want = match e.kind {
            ORDER_EVENT_RESTING => EV_RESTING,
            ORDER_EVENT_CANCELED => EV_CANCELED,
            _ => continue,
        };
        if let Some((at, _, _)) = updates
            .iter()
            .find(|(_, u, _)| u.kind == want && ours(&u.cloid) == Some(e.client_oid))
        {
            let lag = (*at as i64 - *t as i64) / NS_PER_MS as i64;
            if want == EV_RESTING {
                open.push(lag);
            } else {
                canceled.push(lag);
            }
        }
    }
    m.open_row_lag_ms = Spread::of(&mut open);
    m.cancel_row_lag_ms = Spread::of(&mut canceled);
    let mut fl = Vec::new();
    for (t, f) in &run.drive.fills {
        if let Some((at, _)) = fills
            .iter()
            .find(|(_, u)| ours(&u.cloid) == Some(f.order_id) && u.sz_1e8 == f.qty.0 * 100)
        {
            fl.push((*at as i64 - *t as i64) / NS_PER_MS as i64);
        }
    }
    m.fill_row_lag_ms = Spread::of(&mut fl);
    m.read_after_ack_ms = Spread::of(&mut run.read_ack_ms);
    if failed {
        run.rep.fail(
            EXIT_ACCOUNT,
            String::from(
                "the tap did not watch the account the whole run (it never connected, or its \
                 stream broke): rows of another signer would go unseen",
            ),
        );
    }
    if foreign != (0, 0) {
        run.rep.fail(
            EXIT_ACCOUNT,
            format!(
                "rows NOT of slot 6 on the shared account while the battery ran: {} update(s), \
                 {} fill(s) — another signer, or another slot's order filled",
                foreign.0, foreign.1
            ),
        );
    }
}

/// The bar: cycles, fills, clean reconciliations.
fn bars(run: &mut Run<'_>) {
    let r = &mut run.rep;
    if r.boot_ms == 0 {
        return;
    }
    if r.cycles.ok < run.spec.cycles.min(BAR_CYCLES) {
        let msg = format!("cycles {}/{} whole", r.cycles.ok, run.spec.cycles);
        r.fail(EXIT_CYCLE, msg);
    }
    if r.fills < BAR_FILLS.min(2 * run.spec.fill_pairs) {
        let msg = format!(
            "fills {} < {}",
            r.fills,
            BAR_FILLS.min(2 * run.spec.fill_pairs)
        );
        r.fail(EXIT_FILLS, msg);
    }
    if r.recons.ok < BAR_RECONS {
        let msg = format!("clean reconciliations {} < {BAR_RECONS}", r.recons.ok);
        r.fail(EXIT_RECON, msg);
    }
    // The bar itself, whatever the run's own counts asked (a short run
    // passes its own counts, never the bar).
    r.bar_met = r.exit == 0
        && r.cycles.ok >= BAR_CYCLES
        && r.fills >= BAR_FILLS
        && r.recons.ok >= BAR_RECONS;
}

/// The human report, one line per fact (stderr).
pub fn log_report(r: &BatteryReport) {
    let m = &r.measured;
    info!(
        exit = r.exit,
        bar_met = r.bar_met,
        coins = %r.coins.join(","),
        dropped = %r.dropped.join(","),
        boot_ms = r.boot_ms,
        drift_hw_1e6 = r.drift_hw_1e6,
        halt_reason = r.arm.halt_reason,
        "xh4g REPORT"
    );
    info!(
        cycles = ?r.cycles, modify_refused = ?r.modify_refused, bad_alo = ?r.bad_alo,
        ttl = ?r.ttl, sweeps = ?r.sweeps, recons = ?r.recons, iocs = ?r.iocs,
        fills = r.fills, fees_1e8 = r.fees_1e8, stall = ?r.stall, stall_skipped = r.stall_skipped,
        leftover = ?r.leftover,
        "xh4g phases"
    );
    let a = &r.arm;
    info!(
        post_rtt_p50_ms = a.post_rtt_p50_ns / NS_PER_MS,
        post_rtt_p99_ms = a.post_rtt_p99_ns / NS_PER_MS,
        post_rtt_max_ms = a.post_rtt_max_ns / NS_PER_MS,
        push_to_write_p99_us = a.push_to_write_p99_ns / 1_000,
        cancel_rtt_p99_ms = a.cancel_rtt_p99_ns / NS_PER_MS,
        fill_lag_p99_ms = a.fill_lag_p99_ns / NS_PER_MS,
        rtt_worst_ms = a.rtt_worst_ns / NS_PER_MS,
        "xh4g latencies"
    );
    info!(
        dm_arms = a.dm_arms,
        dm_clears = a.dm_clears,
        dm_accepted = a.dm_accepted,
        dm_refused = a.dm_refused,
        dm_rate_limited = a.dm_rate_limited,
        dm_in_doubt = a.dm_in_doubt,
        dm_triggers = a.dm_triggers,
        dm_availability = a.dm_availability,
        leverage_dark = a.leverage_dark,
        budget_remaining = a.budget_remaining,
        limit_reads = a.limit_reads,
        day_pages = a.day_pages,
        day_unreadable = a.day_unreadable,
        "xh4g dead man, leverage, budget, day"
    );
    info!(
        reads = a.reads,
        reads_failed = a.reads_failed,
        alo_rejects = a.alo_rejects,
        rejects = a.rejects,
        stray = a.stray,
        unknown_orders = a.unknown_orders,
        closed_unseen = a.closed_unseen,
        doubt_settled = a.doubt_settled,
        join_timeouts = a.join_timeouts,
        throttled = a.throttled,
        ttl_cancels = a.gw_ttl_cancels,
        post_timeouts = a.post_timeouts,
        rest_fallback = a.rest_fallback_posts,
        reconnects = a.reconnects,
        ws_errors = a.gw_ws_errors,
        "xh4g arm"
    );
    info!(
        modify_original_canceled = ?m.modify_original_canceled,
        refused_modify_dropped_original = ?m.refused_modify_dropped_original,
        read_lagged_ack = ?m.read_lagged_ack, read_after_ack_ms = ?m.read_after_ack_ms,
        open_row_lag_ms = ?m.open_row_lag_ms, cancel_row_lag_ms = ?m.cancel_row_lag_ms,
        fill_row_lag_ms = ?m.fill_row_lag_ms, skew_ms = ?m.skew_ms,
        first_sheet_disagreed = ?m.first_sheet_disagreed,
        day_page = ?m.day_page, all_page = ?m.all_page,
        rate_peaks_ip_ws_congestion = ?m.rate_peaks, tap_rows = ?m.tap_rows,
        foreign_rows = ?m.foreign_rows,
        dropped_original_reasons = ?m.dropped_original_reasons,
        status_words = ?m.status_words,
        rtt_word_max_at_rest_above_ms = ?m.rtt_word,
        "xh4g measured"
    );
    let mut i = 0usize;
    while i < r.failures.len() {
        error!(n = i + 1, "xh4g failure: {}", r.failures[i]);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit codes are the operator's contract: pinned.
    #[test]
    fn the_exit_codes_are_pinned() {
        assert_eq!(
            [
                EXIT_ACCOUNT,
                EXIT_BOOT,
                EXIT_CYCLE,
                EXIT_BAD_ALO,
                EXIT_TTL,
                EXIT_SWEEP,
                EXIT_RECON,
                EXIT_FILLS,
                EXIT_LEFTOVER,
                EXIT_STALL
            ],
            [30, 31, 32, 33, 34, 35, 36, 37, 38, 39]
        );
        assert_eq!((BAR_CYCLES, BAR_FILLS, BAR_RECONS), (20, 5, 3));
    }

    fn spec() -> BatterySpec {
        BatterySpec {
            coins: vec![String::from("BTC")],
            cycles: 20,
            fill_pairs: 3,
            usd: 15,
            rest_bps: 500,
            cross_bps: 300,
            ttl_ms: 2_000,
            sweep_every: 5,
            recon_ms: 10_000,
            stall: false,
            sweep_first: false,
        }
    }

    #[test]
    fn the_statement_is_bounded() {
        assert_eq!(spec().validate(), Ok(()));
        let bad = [
            BatterySpec {
                coins: Vec::new(),
                ..spec()
            },
            BatterySpec {
                cycles: 0,
                ..spec()
            },
            BatterySpec {
                usd: MAX_STATED_USD + 1,
                ..spec()
            },
            BatterySpec {
                usd: MIN_USD - 1,
                ..spec()
            },
            BatterySpec {
                rest_bps: 50,
                ..spec()
            },
            BatterySpec {
                cross_bps: 2_000,
                ..spec()
            },
            BatterySpec {
                ttl_ms: 500,
                ..spec()
            },
            BatterySpec {
                recon_ms: 999,
                ..spec()
            },
            BatterySpec {
                sweep_every: 0,
                ..spec()
            },
        ];
        let mut i = 0usize;
        while i < bad.len() {
            assert!(bad[i].validate().is_err(), "case {i}");
            i += 1;
        }
    }

    #[test]
    fn mids_and_decimals_scan_and_refuse() {
        let b = br#"{"@1":"1.0","BTC":"60123.5","kBTC":"7","ETH":"3000","SOL":"x"}"#;
        assert_eq!(mid_of(b, "BTC"), Some(6_012_350_000_000));
        assert_eq!(mid_of(b, "ETH"), Some(300_000_000_000));
        assert_eq!(mid_of(b, "SOL"), None, "not a number");
        assert_eq!(mid_of(b, "XRP"), None, "absent");
        assert_eq!(
            dec_1e8(b"0.123456789"),
            Some(12_345_678),
            "past 8 decimals: cut"
        );
        assert_eq!(dec_1e8(b""), None);
        assert_eq!(dec_1e8(b"1.2.3"), None);
    }

    #[test]
    fn spreads_take_their_quantiles() {
        let mut v = vec![5, 1, 4, 2, 3];
        let s = Spread::of(&mut v);
        assert_eq!((s.n, s.min, s.p50, s.p90, s.max), (5, 1, 3, 4, 5));
        assert_eq!(Spread::of(&mut []), Spread::default());
    }

    #[test]
    fn a_leftover_outranks_every_other_failure() {
        let mut r = BatteryReport::default();
        r.fail(EXIT_TTL, String::from("ttl"));
        r.fail(EXIT_RECON, String::from("recon"));
        assert_eq!(r.exit, EXIT_TTL, "the first failure names the run");
        r.fail(EXIT_LEFTOVER, String::from("left"));
        assert_eq!(r.exit, EXIT_LEFTOVER, "a leftover is the loudest");
        assert!(!r.passed());
        assert!(r.to_json().starts_with("{\"exit\":38,"));
    }
}
