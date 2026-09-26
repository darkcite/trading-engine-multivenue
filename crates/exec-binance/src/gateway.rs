// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The gateway thread (plan §3.3).** The single writer of every Binance
//! socket and every Binance table.
//!
//! ```text
//! loop {
//!   1. latency path first: every queued command rendered into ONE burst
//!      on the order socket, then ONE flush (the E7 lesson)
//!   2. sockets: order answers (ACK / refusal) · user events (fills, states,
//!      margin) · REST answers (listenKey, dead-man, recon, the day)
//!   3. coarse timers (every ~1 ms): TTL cancels · the pulse (100 ms) · the
//!      dead-man heartbeat · the clock (60 s) · recon + margin · the
//!      listenKey keep-alive (30 min) · in-doubt queries · reconnects ·
//!      the 23 h rotation
//! }
//! ```
//!
//! **Boot** ([`BnGateway::boot`]) runs the same loop on the BOOT thread
//! until the gateway is ready: the clock measured, the BX-19 assertions
//! held (one-way mode, single-asset margin, a key that trades futures and
//! cannot withdraw, the account mode), the listenKey created, the order
//! session logged on, the user stream open, the first reconciliation run
//! with every orphan swept (BX-9) and the day's spend read (obligation 8).
//! An assertion that fails refuses the boot. Only then is the gateway
//! moved onto its own thread ([`BnGateway::run`]).
//!
//! **Ordering (BX3 obligation 1).** Fills and events leave in the order
//! they were produced: an order's fills are on lane 4 before the event
//! that retires it. If lane 4 or the event ring is full, both are HELD in
//! one ordered queue and re-offered first thing each pass — a fill is
//! never dropped and a retirement never overtakes a fill.
//!
//! **In doubt (BX-11).** A request is in flight from the flush onward. A
//! place whose answer never comes (a timeout, a dead socket) is IN DOUBT:
//! booked as resting, resolved by `order.status`, never resent. So is every
//! open order when the user stream reopens (Binance does not replay what
//! it sent while the stream was down) or the order session logs on again,
//! and every working order an open-order listing should have named and
//! did not. Every `ORDER_TRADE_UPDATE` of ours books whatever its
//! cumulative quantity says is still unbooked (trades lost while the
//! stream was down, S1). A fill booked without its trade — from a status
//! answer or a cumulative quantity, at the implied average — marks its
//! order: the stream's trades for it then book only past its cumulative
//! quantity.
//!
//! **Cancels are owed, never dropped.** A cancel the session cannot take
//! now, whose answer was lost, or that the venue refused is OWED and
//! retried from the TTL wheel. A `-2011` (no such open order) puts the
//! order in doubt and keeps the cancel owed until the venue shows the
//! order working (its place was still in flight, S3).
//!
//! **The dead-man (O-BX13).** `countdownCancelAll` is renewed for every
//! row with a maker of ours resting, and only while the order session can
//! cancel — a dead-man renewed by a gateway that cannot cancel would
//! outlive the makers it guards. A WS API request unanswered for
//! [`WS_REQ_TIMEOUT_NS`] drops the order session, and a session silent for
//! a heartbeat while a maker rests is probed (a margin re-read), so a
//! half-open socket stops the dead-man too (S2). The sweep falls back to
//! REST (`DELETE allOpenOrders`) while the session is down, and from its
//! second round on (S4).
//!
//! **Quiet (BX-12).** After a budget or lock answer the gateway sends
//! NOTHING for [`QUIET_NS`] — or until the ban the venue names ends, up to
//! [`QUIET_MAX_NS`] (S6): no order, cancel, heartbeat, REST call or
//! reconnect (the dead-man then cancels the makers).
//!
//! Built for **USDⓈ-M × classic** (the only maker product, BX-17 after
//! §13.5); the other products and modes arrive with BX7–BX10.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clob_dispatcher::{
    RETIRED_CANCELED_MEMBER, RETIRED_CANCELED_TTL, RETIRED_CANCELED_VENUE, RETIRED_EXPIRED,
    RETIRED_FILLED, RETIRED_REJECTED,
};
use core_fill::ORDER_KIND_MAKER;
use core_net::{Backoff, ConnCfg, HttpsConn, Progress, ReqIds, WsCfg, WsConn, WsNext, WsProgress};
use core_ring::{Consumer, Producer};
use core_types::{ExecRecord, Fill, Price, Qty, Side, FILL_FLAG_SETTLEMENT, FILL_ORIGIN_VENUE};
use mio::{Events, Poll, Token};

use crate::cid::{CidClass, CidPrefix};
use crate::clock::VenueClock;
use crate::cmd::*;
use crate::config::BnConfig;
use crate::gov::{classify, CodeClass, ACCOUNT_REREAD_MS, QTR_ICR_NS};
use crate::inst::{WireTable, INST_LIVE, INST_NO_DEADMAN, INST_OWNED, PRODUCT_USDM, ROW_NONE};
use crate::journal::{
    JournalTx, J_ACK, J_ANCHOR, J_ENDED, J_FILL, J_FILL_LOST, J_FILL_UNSEEN, J_MARGIN, J_RECON, J_REJECT, J_SWEEP, J_VENUE,
};
use crate::margin::um_ratio_1e6;
use crate::mode::{judge_key_futures, judge_mode, judge_one_way, judge_single_asset, AccountMode, AssertErr};
use crate::oot::{
    EndedRing, Oot, OotErr, TidRing, OF_CANCEL_SENT, OF_FILLED_ANY, OF_LISTED, OF_MODIFY_SENT, OF_ON_WHEEL, OF_STATUS_BOOKED,
    OF_TTL, OOT_CAP, OWE_MEMBER, OWE_TTL, OWE_UNKNOWN, ST_IN_DOUBT, ST_LIVE, ST_SENT,
};
use crate::recon::{day_increasing_1e6, judge_positions, scan_um_account, Leg, PosRow, Verdict, TRADES_PAGE};
use crate::rest::*;
use crate::ttl::TtlWheel;
use crate::userstream::*;
use crate::wsapi::*;

const TOKEN_WS: Token = Token(0);
const TOKEN_US: Token = Token(1);
const TOKEN_REST: Token = Token(2);
const TOKEN_SAPI: Token = Token(3);

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
/// WS API requests in flight at once.
pub const WS_IDS: usize = 256;
/// An unanswered WS API request is judged after this — and the order
/// session with it: a request unanswered this long means the session is
/// dead whatever its socket says (S2).
pub const WS_REQ_TIMEOUT_NS: u64 = 5 * S;
/// The pulse.
pub const STATUS_EVERY_NS: u64 = 100 * MS;
/// The clock round.
pub const CLOCK_EVERY_NS: u64 = 60 * S;
/// The 23 h rotation (connections live at most 24 h).
pub const ROTATE_AFTER_NS: u64 = 23 * 3_600 * S;
/// An in-doubt order is queried this often.
pub const IN_DOUBT_EVERY_NS: u64 = S;
/// Rounds of cancels a sweep tries before it reports `Stranded`.
pub const SWEEP_ROUNDS: u8 = 3;
/// Events and fills held while a ring is full.
pub const HELD_CAP: usize = 2_048;
/// Commands drained per pass.
pub const CMD_BURST: usize = 64;
/// A non-order request's id marker in [`WsReq::ix`].
const IX_NONE: u16 = u16::MAX;
/// Local refusal codes (never a venue's: Binance codes are negative, these
/// positive and above any HTTP status).
/// The open-order table is full.
pub const CODE_TABLE_FULL: i32 = 9_001;
/// The id is open already.
pub const CODE_DUPLICATE: i32 = 9_002;
/// The order session is not ready (not logged on, clock not measured).
pub const CODE_NOT_READY: i32 = 9_003;
/// The request could not be rendered into the send window.
pub const CODE_RENDER: i32 = 9_004;
/// Every WS API request id is in flight.
pub const CODE_IDS_FULL: i32 = 9_005;
/// The order a cancel or modify names is not open.
pub const CODE_UNKNOWN_ORDER: i32 = 9_006;
/// A modify is already in flight for the order.
pub const CODE_MODIFY_BUSY: i32 = 9_007;
/// The gateway is quiet after a budget or lock answer (plan §3.9, BX-12).
pub const CODE_QUIET: i32 = 9_008;
/// How long the gateway sends nothing after a budget or lock answer (the
/// production [`GwKnobs::quiet_ns`]): no order, no cancel, no heartbeat
/// (the dead-man then cancels the makers), no REST, no reconnect.
/// Binance's 429 asks for a pause; traffic through it earns a 418 ban on
/// the IP the worker shares.
pub const QUIET_NS: u64 = 60 * S;
/// A working order placed this long before an open-order list was asked
/// for, and absent from it, ended unseen: in doubt (BX-11).
pub const LISTING_MARGIN_NS: u64 = 2 * S;
/// An owed cancel the session could not take is retried this soon (the
/// TTL wheel is the retry queue; nothing is sent while not ready).
pub const CANCEL_RETRY_NS: u64 = 10 * MS;
/// A cancel the venue refused (other than `-2011`) is retried after this.
pub const CANCEL_REFUSED_RETRY_NS: u64 = S;
/// A ban end the venue names is honoured this far ahead at most (Binance
/// bans last 2 minutes to 3 days, S6).
pub const QUIET_MAX_NS: u64 = 3 * 86_400 * S;
/// The least time between account reads a margin re-read may start (every
/// fill pushes an `ACCOUNT_UPDATE`, S5).
const ACCOUNT_REREAD_NS: u64 = ACCOUNT_REREAD_MS * MS;
/// A fill's venue time `T` is trusted this far behind the gateway's venue
/// clock (a late event)…
const FILL_TS_BEHIND_MS: u64 = 86_400_000;
/// …and this far ahead of it (clock error); otherwise the clock's own.
const FILL_TS_AHEAD_MS: u64 = 60_000;
/// The sweep's confirmation reads back off: round `r` (from 0) of a sweep
/// after `k` stranded in a row waits `S << min(r + k, SWEEP_BACKOFF_MAX)`.
const SWEEP_BACKOFF_MAX: u8 = 5;
/// The tallies go out in groups of three, one group per [`EVT_TALLY`].
const TALLY_GROUPS: u8 = (crate::arm::GW_TALLIES / 3) as u8;
const _: () = assert!(crate::arm::GW_TALLIES % 3 == 0);

/// The gateway's boot-fixed knobs.
#[derive(Clone, Debug)]
pub struct GwKnobs {
    /// The boot epoch (low 32 bits of unix seconds).
    pub epoch: u32,
    /// The one live Binance slot (BX-7 as built).
    pub owner_slot: u8,
    /// The account is shared (O-BX2a).
    pub shared: bool,
    /// The account mode `exec.toml` asserts (BX-19).
    pub mode: AccountMode,
    /// `recv_window_ms`.
    pub recv_window_ms: u64,
    /// `stp_mode`.
    pub stp_mode: String,
    /// `countdown_ms`.
    pub countdown_ms: u64,
    /// `heartbeat_ms`.
    pub heartbeat_ms: u64,
    /// `recon_every_ms`.
    pub recon_every_ms: u64,
    /// Busy-poll (1) or sleep up to 1 ms in `poll` (0).
    pub spin: bool,
    /// The session anchor already persisted (E7; 0 = none yet). A first
    /// anchor is journaled ([`J_ANCHOR`]) and persisted by the journal's
    /// writer thread.
    pub pnl_anchor_1e6: i64,
    /// How long `boot` may take.
    pub boot_timeout_ns: u64,
    /// How long the gateway stays quiet after a budget or lock answer:
    /// [`QUIET_NS`] (the boot passes the constant; a test, less).
    pub quiet_ns: u64,
    /// The journal writer's "the anchor could not be stored" flag
    /// ([`crate::journal::AnchorFile::unsaved_flag`]; F2), read by the
    /// pulse into `EVT_F_ANCHOR_UNSAVED`.
    pub anchor_unsaved: Option<Arc<AtomicBool>>,
}

/// Why the boot was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BootErr {
    /// A transport could not be built (DNS, a bad endpoint).
    Transport,
    /// The boot vectors failed.
    SelfTest(crate::selftest::SelfTestErr),
    /// A BX-19 assertion did not hold.
    Assert(AssertErr),
    /// The venue locked the account or the IP during the boot.
    VenueLock(i32),
    /// The venue answered a budget code (429, `-1003`, `-1015`) during the
    /// boot: the gateway went quiet and the boot stops.
    Budget(i32),
    /// Not ready within the boot timeout (the phase it stalled in).
    Timeout(u8),
}

/// Where the gateway is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// Measuring the clock.
    Clock = 0,
    /// The BX-19 assertions.
    Assert = 1,
    /// Creating the listenKey.
    ListenKey = 2,
    /// Opening and logging on the sockets.
    Connect = 3,
    /// The first reconciliation and the orphan sweep.
    Recon = 4,
    /// The day's spend.
    Day = 5,
    /// Serving.
    Ready = 6,
    /// Leaving (after a shutdown sweep).
    Exiting = 7,
}

/// A REST job.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
enum JobKind {
    Time = 0,
    Dual = 1,
    MultiAssets = 2,
    Restrictions = 3,
    Portfolio = 4,
    ListenNew = 5,
    ListenKeep = 6,
    Countdown = 7,
    OpenOrders = 8,
    UserTrades = 9,
    CancelAll = 10,
    /// The dead-man's proving request: `countdownCancelAll` with
    /// `countdownTime=0`, which sets no timer (N1).
    Probe = 11,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Job {
    kind: JobKind,
    row: u16,
    sent_ns: u64,
}

/// REST jobs queued at once. Each row queues at most one countdown, one
/// day read and one REST cancel-all at a time, so the queue can hold every
/// job there can be — a full queue is counted ([`crate::arm::GW_JOBS_DROPPED`])
/// and cannot occur.
const JOB_CAP: usize = 1_024;
const _: () = assert!(JOB_CAP >= 3 * crate::inst::INST_MAX + 16);

/// A small FIFO (the REST jobs; the held output).
struct Fifo<T: Copy, const N: usize> {
    buf: Box<[Option<T>; N]>,
    head: usize,
    tail: usize,
}

impl<T: Copy, const N: usize> Fifo<T, N> {
    fn new() -> Self {
        Self {
            buf: Box::new([None; N]),
            head: 0,
            tail: 0,
        }
    }
    #[inline(always)]
    fn len(&self) -> usize {
        self.tail - self.head
    }
    #[inline(always)]
    fn push(&mut self, v: T) -> bool {
        if self.len() == N {
            return false;
        }
        self.buf[self.tail % N] = Some(v);
        self.tail += 1;
        true
    }
    #[inline(always)]
    fn front(&self) -> Option<T> {
        if self.len() == 0 {
            None
        } else {
            self.buf[self.head % N]
        }
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<T> {
        let v = self.front()?;
        self.head += 1;
        Some(v)
    }
    #[inline(always)]
    fn front_ref(&self) -> Option<&T> {
        if self.len() == 0 {
            None
        } else {
            self.buf[self.head % N].as_ref()
        }
    }
    #[inline(always)]
    fn advance(&mut self) {
        if self.len() > 0 {
            self.head += 1;
        }
    }
}

/// One held output, in production order.
#[derive(Copy, Clone)]
enum Out {
    Fill(Fill),
    Evt(BnEvt),
}

/// The recon cycle in progress.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ReconStep {
    Idle,
    Account,
    Orders,
}

/// The sweep in progress.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Sweep {
    active: bool,
    round: u8,
    confirm_at_ns: u64,
    cancels: i64,
    shutdown: bool,
}

/// Per-row state on the gateway.
#[derive(Copy, Clone, Debug, Default)]
struct RowGw {
    booked_1e6: i64,
    last_px_1e6: i64,
    cd_ok_ns: u64,
    cd_next_ns: u64,
    /// When the row's first resting maker was placed (its dead-man's age
    /// until the first countdown answer).
    cd_since_ns: u64,
    day_spent_1e6: i64,
    /// Orders of ours open on the row / of which makers (O-BX13: the
    /// countdown runs only while a maker rests).
    open: u16,
    makers: u16,
    day_read: bool,
    /// The row's last countdown was refused or lost.
    cd_bad: bool,
    /// A job of this kind is queued or in flight for the row (at most one).
    cd_queued: bool,
    day_queued: bool,
    cancel_all_queued: bool,
}

/// **The gateway** (module docs). `FILL_N` is the engine's fill-lane size.
pub struct BnGateway<const FILL_N: usize> {
    cmd: Consumer<BnCmd, CMD_RING>,
    evt: Producer<BnEvt, EVT_RING>,
    fills: Producer<Fill, FILL_N>,
    journal: JournalTx,
    poll: Poll,
    events: Events,
    ws: WsConn,
    us: WsConn,
    rest: HttpsConn,
    sapi: Option<HttpsConn>,
    ids: ReqIds<WsReq, WS_IDS>,
    signer: signer_ed25519::Ed25519Signer,
    api_key: String,
    st: WsStatic,
    prefix: CidPrefix,
    wire: WireTable,
    oot: Oot,
    ttl: TtlWheel,
    tids: TidRing<256>,
    ended: EndedRing,
    clock: VenueClock,
    user: UserStream,
    knobs: GwKnobs,
    phase: Phase,
    logged_on: bool,
    ws_since_ns: u64,
    ws_down_ns: u64,
    us_down_ns: u64,
    ws_backoff: Backoff,
    us_backoff: Backoff,
    ws_retry_ns: u64,
    us_retry_ns: u64,
    rows: Vec<RowGw>,
    legs: Vec<Leg>,
    jobs: Fifo<Job, JOB_CAP>,
    job: Option<Job>,
    sapi_job: Option<Job>,
    held: Fifo<Out, HELD_CAP>,
    recon: ReconStep,
    recon_next_ns: u64,
    /// The recon in flight reads the open orders alone (the sweep's
    /// confirmation while the order session is down).
    recon_listing_only: bool,
    /// The recon in flight reads the account alone (a margin re-read, S5;
    /// the order session's probe, S2).
    recon_account_only: bool,
    /// When the last account read went out: a margin re-read starts no
    /// sooner than [`ACCOUNT_REREAD_NS`] after it.
    account_read_ns: u64,
    account_next_ns: u64,
    pos: Box<[PosRow; 256]>,
    orders: Box<[OrderRow; 1_024]>,
    trades: Box<[TradeRow; TRADES_PAGE]>,
    verdict: Verdict,
    equity_1e6: i64,
    ratio_1e6: i64,
    anchor_1e6: i64,
    status_next_ns: u64,
    clock_next_ns: u64,
    clock_pending: u8,
    doubt_next_ns: u64,
    tally_next_ns: u64,
    timers_next_ns: u64,
    tally_group: u8,
    asserts_left: u8,
    day_left: u16,
    day_sent: bool,
    sweep: Sweep,
    boot_err: Option<BootErr>,
    t: [u64; crate::arm::GW_TALLIES],
    /// Nothing is sent before this (a budget or lock answer).
    quiet_until_ns: u64,
    /// An output the rings and the held queue could not take (sticky).
    lost: bool,
    /// A countdown answered once this process: the endpoint works for
    /// this key. No maker is admitted before (`EVT_F_DEADMAN_OK`).
    cd_proven: bool,
    /// The row the proving countdown is sent for (a maker-capable live
    /// row, the next one after a refusal; `ROW_NONE` if there is none).
    cd_probe_row: u16,
    /// The probe is queued or in flight; the next not before this (N1).
    probe_queued: bool,
    probe_next_ns: u64,
    /// The E7 anchor's journal record was dropped: it is offered again
    /// until the ring takes it (S8).
    anchor_pending: bool,
    /// When the order session last delivered a frame (S2).
    ws_rx_ns: u64,
    /// Makers of ours resting, every row.
    makers: u32,
    /// Sweeps that ended stranded in a row (their reads back off).
    sweep_streak: u8,
    /// The last sweep: orders left, cancels sent, `EVT_F_SWEEP_*`.
    last_sweep: (i64, i64, u8),
    /// The last reconciliation's verdict.
    last_verdict: Verdict,
}

impl<const FILL_N: usize> BnGateway<FILL_N> {
    /// **Boot-only construction**: resolve every host (blocking DNS, here
    /// and never again), build the transports and the ONE signer (after the
    /// signer's own self-test and the boot vectors).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &BnConfig,
        tls: Arc<rustls::ClientConfig>,
        knobs: GwKnobs,
        wire: WireTable,
        cmd: Consumer<BnCmd, CMD_RING>,
        evt: Producer<BnEvt, EVT_RING>,
        fills: Producer<Fill, FILL_N>,
        journal: JournalTx,
    ) -> Result<Self, BootErr> {
        crate::selftest::run().map_err(BootErr::SelfTest)?;
        let signer = cfg.signer().map_err(|_| BootErr::SelfTest(crate::selftest::SelfTestErr::Signer))?;
        let h = cfg.hosts();
        let port = cfg.port();
        let wscfg = WsCfg {
            rx_cap: 256 * 1_024,
            tx_cap: 64 * 1_024,
            establish_ns: 10 * S,
            idle_ns: 240 * S,
        };
        let ws = WsConn::new(&h.fut_wsapi, port, &h.fut_wsapi_path, tls.clone(), wscfg, TOKEN_WS, 0x6273_7773 ^ knobs.epoch as u64)
            .map_err(|_| BootErr::Transport)?;
        // The user stream's path is the listenKey's, set once it exists.
        let us = WsConn::new(&h.fut_user, port, "/ws/pending", tls.clone(), wscfg, TOKEN_US, 0x6273_7573 ^ knobs.epoch as u64)
            .map_err(|_| BootErr::Transport)?;
        let key_header = [(KEY_HEADER, cfg.api_key())];
        let conncfg = ConnCfg {
            win_cap: 4 * 1_024,
            resp_cap: 1_024 * 1_024,
            req_timeout_ns: 5 * S,
        };
        let rest = HttpsConn::new(&h.fut_rest, port, tls.clone(), &fapi_specs(&key_header), conncfg, TOKEN_REST)
            .map_err(|_| BootErr::Transport)?;
        let sapi = HttpsConn::new(&h.sapi, port, tls, &sapi_specs(&key_header), conncfg, TOKEN_SAPI)
            .map_err(|_| BootErr::Transport)?;
        let poll = Poll::new().map_err(|_| BootErr::Transport)?;
        let n = wire.len();
        let now = core_time::now_ns();
        let mut legs = vec![Leg::default(); n];
        let mut cd_probe_row = ROW_NONE;
        let mut i = 0;
        while i < n {
            let w = wire.row(i as u16);
            legs[i].owned = w.flags & INST_OWNED != 0;
            // The first cycle counts at once (recon.rs).
            legs[i].differed = true;
            if cd_probe_row == ROW_NONE && w.flags & INST_LIVE != 0 && w.flags & INST_NO_DEADMAN == 0 {
                cd_probe_row = i as u16;
            }
            i += 1;
        }
        let st = WsStatic::new(&knobs.stp_mode, knobs.recv_window_ms);
        Ok(Self {
            cmd,
            evt,
            fills,
            journal,
            poll,
            events: Events::with_capacity(64),
            ws,
            us,
            rest,
            sapi: Some(sapi),
            ids: ReqIds::new(),
            signer,
            api_key: String::from(cfg.api_key()),
            st,
            prefix: CidPrefix::new(knobs.epoch),
            wire,
            oot: Oot::new(),
            ttl: TtlWheel::new(now),
            tids: TidRing::new(),
            ended: EndedRing::new(),
            clock: VenueClock::new(core_time::WallAnchor::now()),
            user: UserStream::new(),
            anchor_1e6: knobs.pnl_anchor_1e6,
            knobs,
            phase: Phase::Clock,
            logged_on: false,
            ws_since_ns: 0,
            ws_down_ns: now,
            us_down_ns: now,
            ws_backoff: Backoff::new(250 * MS, 30 * S, 7),
            us_backoff: Backoff::new(250 * MS, 30 * S, 11),
            ws_retry_ns: 0,
            us_retry_ns: 0,
            rows: vec![RowGw::default(); n],
            legs,
            jobs: Fifo::new(),
            job: None,
            sapi_job: None,
            held: Fifo::new(),
            recon: ReconStep::Idle,
            recon_next_ns: 0,
            recon_listing_only: false,
            recon_account_only: false,
            account_read_ns: 0,
            account_next_ns: 0,
            pos: Box::new([PosRow::default(); 256]),
            orders: Box::new([OrderRow::default(); 1_024]),
            trades: Box::new([TradeRow::default(); TRADES_PAGE]),
            verdict: Verdict::default(),
            equity_1e6: 0,
            ratio_1e6: 0,
            status_next_ns: 0,
            clock_next_ns: 0,
            clock_pending: 0,
            doubt_next_ns: 0,
            tally_next_ns: 0,
            timers_next_ns: 0,
            tally_group: 0,
            asserts_left: 0,
            day_left: 0,
            day_sent: false,
            sweep: Sweep {
                active: false,
                round: 0,
                confirm_at_ns: 0,
                cancels: 0,
                shutdown: false,
            },
            boot_err: None,
            t: [0; crate::arm::GW_TALLIES],
            quiet_until_ns: 0,
            lost: false,
            cd_proven: false,
            cd_probe_row,
            probe_queued: false,
            probe_next_ns: 0,
            anchor_pending: false,
            ws_rx_ns: 0,
            makers: 0,
            sweep_streak: 0,
            last_sweep: (0, 0, 0),
            last_verdict: Verdict::default(),
        })
    }

    /// The phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The last sweep: orders left, cancels sent, `EVT_F_SWEEP_*` (cold).
    #[must_use]
    pub const fn last_sweep(&self) -> (i64, i64, u8) {
        self.last_sweep
    }

    /// The last reconciliation's verdict (cold: the boot tell).
    #[must_use]
    pub const fn last_verdict(&self) -> Verdict {
        self.last_verdict
    }

    /// Refuse the boot with the FIRST reason seen (a lock is not
    /// overwritten by the assertion its lock made unreadable).
    fn refuse_boot(&mut self, e: BootErr) {
        if self.boot_err.is_none() {
            self.boot_err = Some(e);
        }
    }

    /// **The boot** (module docs), on the calling thread.
    pub fn boot(&mut self) -> Result<(), BootErr> {
        let start = core_time::now_ns();
        self.clock_pending = crate::clock::SAMPLES_PER_ROUND;
        self.queue_job(JobKind::Time, 0);
        while self.phase != Phase::Ready {
            self.step();
            if let Some(e) = self.boot_err {
                return Err(e);
            }
            if core_time::now_ns().saturating_sub(start) > self.knobs.boot_timeout_ns {
                return Err(BootErr::Timeout(self.phase as u8));
            }
        }
        // The boot-only connection goes.
        self.sapi = None;
        Ok(())
    }

    /// **Serve** until the shutdown sweep completes — or `grace_ns` after
    /// `stop` is first seen, when nothing asked for one (a boot aborted
    /// after the gateway started). The engine's own drain asks for the
    /// sweep once `stop` is set (the process's SIGINT flag), so the
    /// grace is the time the sweep has to take the makers off the venue.
    pub fn run(&mut self, stop: &AtomicBool, grace_ns: u64) {
        let mut stop_seen_ns = 0u64;
        while self.phase != Phase::Exiting {
            self.step();
            if stop.load(Ordering::Relaxed) {
                let now = core_time::now_ns();
                if stop_seen_ns == 0 {
                    stop_seen_ns = now;
                }
                if now.saturating_sub(stop_seen_ns) >= grace_ns {
                    break;
                }
            }
        }
        self.drain_held();
    }

    /// One pass of the loop (module docs).
    pub fn step(&mut self) {
        let now = core_time::now_ns();
        self.drain_held();
        // 1. The latency path: every queued command, one burst, one flush.
        if self.phase == Phase::Ready {
            let mut n = 0;
            while n < CMD_BURST {
                let Some(g) = self.cmd.try_pop_ref() else {
                    break;
                };
                // COPY: one command (64 B) off the ring — the slot is
                // released before `on_cmd` borrows the whole gateway;
                // handling it in place would hold `self.cmd` across it.
                let c: BnCmd = *g;
                drop(g);
                self.on_cmd(&c, now);
                n += 1;
            }
        }
        self.flush_ws();
        // 2. Sockets.
        let timeout = if self.knobs.spin {
            Some(std::time::Duration::ZERO)
        } else {
            Some(std::time::Duration::from_millis(1))
        };
        if self.poll.poll(&mut self.events, timeout).is_ok() {
            self.dispatch_events(now);
        }
        // 3. Timers.
        self.timers(core_time::now_ns());
        self.flush_ws();
        self.drain_held();
    }

    // ---------------------------------------------------------------------
    // Output: fills and events in production order (obligation 1)
    // ---------------------------------------------------------------------

    fn drain_held(&mut self) {
        loop {
            let ok = match self.held.front_ref() {
                None => return,
                Some(Out::Fill(f)) => self.fills.try_push_ref(f),
                Some(Out::Evt(e)) => self.evt.try_push_ref(e),
            };
            if !ok {
                return;
            }
            self.held.advance();
        }
    }

    /// A fill for lane 4, behind anything held (obligation 1). `false`:
    /// lost — the lane and the held queue are both full, nothing
    /// downstream is draining; the pulse raises unbounded drift.
    #[inline]
    fn out_fill(&mut self, f: &Fill) -> bool {
        if self.held.len() == 0 && self.fills.try_push_ref(f) {
            return true;
        }
        self.t[crate::arm::GW_FILLS_DEFERRED] += 1;
        // COPY: one Fill (64 B) into the held queue — the lane is full, or
        // something is held ahead of it, and no retirement may overtake it
        // (obligation 1) — dropping it was rejected: a position the engine
        // would never hear about.
        if self.held.push(Out::Fill(*f)) {
            return true;
        }
        self.t[crate::arm::GW_FILLS_LOST] += 1;
        self.lost = true;
        false
    }

    /// An event for the arm, behind anything held.
    #[inline]
    fn emit(&mut self, e: &BnEvt) {
        if self.held.len() == 0 && self.evt.try_push_ref(e) {
            return;
        }
        // The periodic reports are not held: the next carries fresher news.
        if e.kind == EVT_STATUS || e.kind == EVT_TALLY {
            return;
        }
        // COPY: one event (64 B) into the held queue — the ring is full, or
        // something is held ahead of it, and a retirement may not overtake
        // its fill (obligation 1) — dropping it was rejected: the arm would
        // never learn the order ended.
        if !self.held.push(Out::Evt(*e)) {
            self.lost = true;
        }
    }

    /// Journal one record. `false`: the ring was full and it was dropped
    /// (counted; the E7 anchor is offered again, S8).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn note(&mut self, kind: u8, now: u64, client_oid: u64, venue_oid: u64, a: i64, b: i64, c: i64, code: i32, row: u16, slot: u8) -> bool {
        let r = ExecRecord {
            ts_ns: now,
            wall_ms: self.clock.venue_ms(now),
            client_oid,
            venue_oid,
            a,
            b,
            c,
            code,
            row,
            kind,
            slot,
        };
        self.journal.note(&r)
    }

    // ---------------------------------------------------------------------
    // Commands
    // ---------------------------------------------------------------------

    /// The order session can take a request now: logged on, the clock
    /// measured, and not quiet after a budget or lock answer.
    #[inline]
    fn ready_to_send(&self, now: u64) -> bool {
        self.logged_on && self.ws.is_open() && self.clock.measured() && now >= self.quiet_until_ns
    }

    /// Why a verb cannot be sent now.
    #[inline]
    fn not_ready_code(&self, now: u64) -> i32 {
        if now < self.quiet_until_ns {
            CODE_QUIET
        } else {
            CODE_NOT_READY
        }
    }

    fn on_cmd(&mut self, c: &BnCmd, now: u64) {
        match c.verb {
            VERB_PLACE => self.on_place(c, now),
            VERB_CANCEL => self.on_cancel(c, now),
            VERB_MODIFY => self.on_modify(c, now),
            VERB_CANCEL_ALL => self.start_sweep(now, false),
            VERB_SHUTDOWN => self.start_sweep(now, true),
            _ => {}
        }
    }

    /// Refuse a place locally: the arm hears a REJECT with a local code.
    fn refuse_place(&mut self, c: &BnCmd, code: i32, now: u64) {
        let mut e = BnEvt::order(EVT_REJECT, now, c.client_oid, c.row, c.slot);
        e.code = code;
        e.why = RETIRED_REJECTED;
        e.side = c.side;
        e.product = PRODUCT_USDM;
        e.flags = (c.kind != ORDER_KIND_MAKER) as u8 * EVT_F_IOC;
        self.emit(&e);
        self.note(J_REJECT, now, c.client_oid, 0, 0, 0, 0, code, c.row, c.slot);
    }

    fn on_place(&mut self, c: &BnCmd, now: u64) {
        if (c.row as usize) >= self.wire.len() {
            return self.refuse_place(c, CODE_NOT_READY, now);
        }
        if !self.ready_to_send(now) {
            let code = self.not_ready_code(now);
            return self.refuse_place(c, code, now);
        }
        // S7: an id the ring remembers ending is not placed again — a late
        // answer or event of the ended order would reach the new one
        // ("unique per slot per boot" is a member's arming law).
        if self.ended.holds(c.slot, c.client_oid) {
            return self.refuse_place(c, CODE_DUPLICATE, now);
        }
        let ix = match self.oot.insert(c.slot, c.client_oid, c.row, c.side, c.kind, c.px_1e6, c.qty_1e6, now) {
            Ok(ix) => ix,
            Err(OotErr::Full) => return self.refuse_place(c, CODE_TABLE_FULL, now),
            Err(OotErr::Duplicate) => return self.refuse_place(c, CODE_DUPLICATE, now),
        };
        let gen = self.oot.aux(ix).gen;
        let Some(id) = self.ids.issue(WsReq::on(REQ_PLACE, ix, gen, c.client_oid), now) else {
            self.oot.remove(ix);
            return self.refuse_place(c, CODE_IDS_FULL, now);
        };
        let maker = c.kind == ORDER_KIND_MAKER;
        let w = self.wire.row(c.row);
        let o = Place {
            symbol: w.symbol(),
            side: c.side,
            maker,
            qty_1e6: c.qty_1e6,
            qty_dec: w.qty_dec,
            px_1e6: c.px_1e6,
            px_dec: w.px_dec,
            cid: Part::Cid(&self.prefix, c.slot, c.client_oid),
            ts_ms: self.clock.venue_ms(now),
        };
        if queue_place(&mut self.ws, id, &self.st, &o).is_err() {
            let _ = self.ids.answer(id);
            self.oot.remove(ix);
            return self.refuse_place(c, CODE_RENDER, now);
        }
        // The row's open orders: a resting maker is what the dead-man
        // guards (O-BX13) — the row's first asks for it this pass.
        let r = &mut self.rows[c.row as usize];
        r.open += 1;
        if maker {
            if r.makers == 0 {
                r.cd_since_ns = now;
                r.cd_next_ns = now;
            }
            r.makers += 1;
            self.makers += 1;
        }
        if c.ttl_deadline_ns != 0 {
            self.ttl.insert(ix, c.ttl_deadline_ns);
            self.oot.get_mut(ix).flags |= OF_ON_WHEEL;
        }
    }

    fn on_cancel(&mut self, c: &BnCmd, now: u64) {
        let Some(ix) = self.member_order(c.slot, c.client_oid) else {
            let mut e = BnEvt::order(EVT_CANCEL_FAILED, now, c.client_oid, c.row, c.slot);
            e.code = CODE_UNKNOWN_ORDER;
            return self.emit(&e);
        };
        self.cancel_or_owe(ix, now, OWE_MEMBER);
    }

    /// The open order a member command names: by its current id, else by
    /// the id of a modify the venue refused (the member was told `Ok` and
    /// knows the order by that id; it rests under its previous one).
    #[inline]
    fn member_order(&self, slot: u8, oid: u64) -> Option<u16> {
        match self.oot.by_current(slot, oid) {
            Some(ix) => Some(ix),
            None => self.oot.by_aborted(slot, oid),
        }
    }

    /// Cancel the open order in slot `ix` now, or owe the cancel (`owe`:
    /// whose it is, `OWE_*`) — retried from the TTL wheel until the
    /// session takes it. A cancel in flight is not repeated.
    fn cancel_or_owe(&mut self, ix: u16, now: u64, owe: u8) {
        if self.oot.get(ix).flags & OF_CANCEL_SENT != 0 {
            return;
        }
        if !self.send_cancel(ix, now, owe == OWE_TTL) {
            self.owe_cancel(ix, owe, now + CANCEL_RETRY_NS);
        }
    }

    /// Owe the cancel of `ix` (`OWE_*`), retried at `at_ns` from the TTL
    /// wheel — or sooner, if the wheel holds it for an earlier deadline.
    fn owe_cancel(&mut self, ix: u16, owe: u8, at_ns: u64) {
        // Counted when the order becomes owed, not on every retry.
        self.t[crate::arm::GW_CANCELS_OWED] += (self.oot.get(ix).owe & (OWE_MEMBER | OWE_TTL) == 0) as u64;
        let m = self.oot.get_mut(ix);
        m.owe |= owe;
        let on = m.flags & OF_ON_WHEEL != 0;
        m.flags |= OF_ON_WHEEL;
        if on {
            if self.ttl.deadline_of(ix) <= at_ns {
                return;
            }
            self.ttl.remove(ix);
        }
        self.ttl.insert(ix, at_ns);
    }

    /// Send the cancel of the open order in slot `ix` (a member's, the
    /// TTL's or a sweep's) now. `false`: not sent — one is in flight, or
    /// the session cannot take it now.
    fn send_cancel(&mut self, ix: u16, now: u64, ttl: bool) -> bool {
        let (flags, slot, oid, row) = {
            let e = self.oot.get(ix);
            (e.flags, e.slot, e.cid_oid, e.row)
        };
        if flags & OF_CANCEL_SENT != 0 || !self.ready_to_send(now) {
            return false;
        }
        let gen = self.oot.aux(ix).gen;
        let Some(id) = self.ids.issue(WsReq::on(REQ_CANCEL, ix, gen, oid), now) else {
            return false;
        };
        let sym = self.wire.row(row).symbol();
        let cid = Part::Cid(&self.prefix, slot, oid);
        if queue_cancel(&mut self.ws, id, &self.st, sym, cid, self.clock.venue_ms(now)).is_err() {
            let _ = self.ids.answer(id);
            return false;
        }
        self.oot.get_mut(ix).flags |= OF_CANCEL_SENT | (ttl as u8 * OF_TTL);
        true
    }

    fn modify_failed(&mut self, c: &BnCmd, code: i32, now: u64) {
        let mut e = BnEvt::order(EVT_MODIFY_FAILED, now, c.client_oid, c.row, c.slot);
        e.a = c.prev_client_oid as i64;
        e.code = code;
        self.emit(&e);
    }

    fn on_modify(&mut self, c: &BnCmd, now: u64) {
        let Some(ix) = self.member_order(c.slot, c.prev_client_oid) else {
            return self.modify_failed(c, CODE_UNKNOWN_ORDER, now);
        };
        let (flags, slot, oid, row, side) = {
            let e = self.oot.get(ix);
            (e.flags, e.slot, e.cid_oid, e.row, e.side)
        };
        if flags & (OF_MODIFY_SENT | OF_CANCEL_SENT) != 0 {
            return self.modify_failed(c, CODE_MODIFY_BUSY, now);
        }
        if !self.ready_to_send(now) {
            let code = self.not_ready_code(now);
            return self.modify_failed(c, code, now);
        }
        if !self.oot.add_pending(ix, c.client_oid, c.px_1e6, c.qty_1e6, now) {
            return self.modify_failed(c, CODE_DUPLICATE, now);
        }
        let gen = self.oot.aux(ix).gen;
        let Some(id) = self.ids.issue(WsReq::on(REQ_MODIFY, ix, gen, oid), now) else {
            self.oot.abort_rename(ix);
            return self.modify_failed(c, CODE_IDS_FULL, now);
        };
        let w = self.wire.row(row);
        let o = Place {
            symbol: w.symbol(),
            side,
            maker: true,
            qty_1e6: c.qty_1e6,
            qty_dec: w.qty_dec,
            px_1e6: c.px_1e6,
            px_dec: w.px_dec,
            cid: Part::Cid(&self.prefix, slot, oid),
            ts_ms: self.clock.venue_ms(now),
        };
        if queue_modify(&mut self.ws, id, &self.st, &o).is_err() {
            let _ = self.ids.answer(id);
            self.oot.abort_rename(ix);
            self.modify_failed(c, CODE_RENDER, now);
        }
    }

    // ---------------------------------------------------------------------
    // The sweep (plan §3.11 switch 1; O-BX8, O-BX9: never a flatten)
    // ---------------------------------------------------------------------

    fn start_sweep(&mut self, now: u64, shutdown: bool) {
        self.sweep = Sweep {
            active: true,
            round: 0,
            confirm_at_ns: 0,
            cancels: 0,
            shutdown,
        };
        self.sweep_round(now);
    }

    fn sweep_round(&mut self, now: u64) {
        // Every maker of ours over the order session (an IoC ends on its
        // own).
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) && self.oot.get(ix).kind == ORDER_KIND_MAKER && self.send_cancel(ix, now, false) {
                self.sweep.cancels += 1;
            }
            ix += 1;
        }
        // B1 / S4: neither an order session that cannot cancel nor WS
        // cancels that keep failing strand the makers — while the session is
        // down, from the second round on, and from the first after a sweep
        // stranded, each row with a maker of ours resting is cancelled over
        // REST (unless the gateway is quiet: the dead-man then does it).
        if !self.ready_to_send(now) || self.sweep.round >= 1 || self.sweep_streak > 0 {
            let mut r = 0;
            while r < self.rows.len() {
                if self.rows[r].makers > 0 && self.rest_cancel_all(r as u16, now) {
                    self.sweep.cancels += 1;
                }
                r += 1;
            }
        }
        // Confirm by the venue's own list once the cancels had time: later
        // each round, and later still after sweeps that stranded (each
        // read is an `openOrders` of weight 40).
        let k = self.sweep.round.saturating_add(self.sweep_streak).min(SWEEP_BACKOFF_MAX);
        self.sweep.round += 1;
        self.sweep.confirm_at_ns = now + (S << k);
    }

    /// Queue the REST cancel of every open order on `row` (the sweep's
    /// fallback, B1; the venue cancels the orders of every client on the
    /// symbol — a dedicated account's are ours, BX6 refuses a shared one).
    /// `false`: quiet, one is queued already, or the queue is full.
    fn rest_cancel_all(&mut self, row: u16, now: u64) -> bool {
        let r = row as usize;
        if now < self.quiet_until_ns || self.rows[r].cancel_all_queued || !self.queue_job(JobKind::CancelAll, row) {
            return false;
        }
        self.rows[r].cancel_all_queued = true;
        true
    }

    /// The confirmation's open orders were read: done, another round, or
    /// stranded.
    fn sweep_confirm(&mut self, ours_left: u32, now: u64) {
        let in_flight = self.oot.len() as u32;
        if ours_left == 0 && in_flight == 0 {
            self.sweep_streak = 0;
            self.finish_sweep(0, EVT_F_SWEEP_DONE, now);
        } else if self.sweep.round >= SWEEP_ROUNDS {
            self.sweep_streak = self.sweep_streak.saturating_add(1);
            self.finish_sweep(ours_left.max(in_flight) as i64, EVT_F_SWEEP_STRANDED, now);
        } else {
            self.sweep_round(now);
        }
    }

    fn finish_sweep(&mut self, left: i64, flag: u8, now: u64) {
        let mut e = BnEvt::new(EVT_SWEEP, now);
        e.a = left;
        e.b = self.sweep.cancels;
        e.flags = flag;
        self.t[crate::arm::GW_SWEEP_LEFT] = left as u64;
        self.last_sweep = (left, self.sweep.cancels, flag);
        self.emit(&e);
        self.note(J_SWEEP, now, 0, 0, left, self.sweep.cancels, 0, 0, 0, 0);
        let shutdown = self.sweep.shutdown;
        self.sweep.active = false;
        if shutdown {
            self.phase = Phase::Exiting;
        }
    }

    // ---------------------------------------------------------------------
    // Sockets
    // ---------------------------------------------------------------------

    fn flush_ws(&mut self) {
        if self.ws.wants_flush() && self.ws.flush(self.poll.registry()).is_err() {
            self.ws_failed(core_time::now_ns());
        }
        if self.us.wants_flush() && self.us.flush(self.poll.registry()).is_err() {
            self.us_failed(core_time::now_ns());
        }
    }

    fn dispatch_events(&mut self, now: u64) {
        // The batch is moved out for the dispatch and back after it: a
        // zero-capacity `Events` owns no buffer, so this allocates nothing.
        let events = core::mem::replace(&mut self.events, Events::with_capacity(0));
        for ev in events.iter() {
            match ev.token() {
                TOKEN_WS => {
                    let p = self.ws.on_event(ev, self.poll.registry(), now);
                    self.on_ws_progress(p, now);
                }
                TOKEN_US => {
                    let p = self.us.on_event(ev, self.poll.registry(), now);
                    self.on_us_progress(p, now);
                }
                TOKEN_REST => {
                    let p = self.rest.on_event(ev, self.poll.registry());
                    self.on_rest_progress(p, now, false);
                }
                TOKEN_SAPI => {
                    if let Some(s) = self.sapi.as_mut() {
                        let p = s.on_event(ev, self.poll.registry());
                        self.on_rest_progress(p, now, true);
                    }
                }
                _ => {}
            }
        }
        self.events = events;
    }

    fn on_ws_progress(&mut self, p: WsProgress, now: u64) {
        match p {
            WsProgress::Waiting => {}
            WsProgress::Opened => {
                if now < self.quiet_until_ns {
                    // N3: a dial that completes inside a quiet period sends
                    // nothing — dropped, and dialled again after it.
                    self.ws.close();
                    self.ws_retry_ns = self.quiet_until_ns;
                    return;
                }
                self.ws_backoff.reset();
                self.ws_since_ns = now;
                self.ws_rx_ns = now;
                self.t[crate::arm::GW_RECONNECTS] += (self.ws.dials() > 1) as u64;
                // session.logon: once per connection, the one signer.
                if let Some(id) = self.ids.issue(WsReq::on(REQ_LOGON, IX_NONE, 0, 0), now) {
                    let key = self.api_key.as_bytes();
                    if queue_logon(&mut self.ws, id, key, self.clock.venue_ms(now), &self.signer).is_err() {
                        let _ = self.ids.answer(id);
                    }
                }
                self.drain_ws(now);
            }
            WsProgress::Readable => self.drain_ws(now),
            WsProgress::Failed(_) => self.ws_failed(now),
        }
    }

    fn drain_ws(&mut self, now: u64) {
        loop {
            let span = match self.ws.next_frame() {
                WsNext::Idle => return,
                WsNext::Text(s) | WsNext::Binary(s) => s,
                WsNext::Failed(_) => return self.ws_failed(now),
            };
            self.ws_rx_ns = now;
            let mut a = WsAnswer::default();
            let scanned = scan_answer(self.ws.payload(span), &mut a);
            match scanned {
                Ok(()) => self.on_answer(&a, span, now),
                Err(e) => self.scan_failed(SCAN_WSAPI, e, now),
            }
        }
    }

    fn on_us_progress(&mut self, p: WsProgress, now: u64) {
        match p {
            WsProgress::Waiting => {}
            WsProgress::Opened => {
                self.us_backoff.reset();
                self.t[crate::arm::GW_RECONNECTS] += (self.us.dials() > 1) as u64;
                if self.phase == Phase::Ready {
                    // What the venue sent while the stream was down is gone
                    // (Binance does not replay): every open order is in
                    // doubt, and the books are checked now (B2).
                    self.doubt_all();
                    self.recon_next_ns = now;
                }
                self.drain_us(now);
            }
            WsProgress::Readable => self.drain_us(now),
            WsProgress::Failed(_) => self.us_failed(now),
        }
    }

    fn drain_us(&mut self, now: u64) {
        loop {
            let span = match self.us.next_frame() {
                WsNext::Idle => return,
                WsNext::Text(s) | WsNext::Binary(s) => s,
                WsNext::Failed(_) => return self.us_failed(now),
            };
            self.user.last_rx_ns = now;
            let mut ev = UserEvent::default();
            let frame = self.us.payload(span);
            match scan_user_event(frame, &mut ev) {
                Ok(()) => {
                    // Resolve the row and the id while the frame is borrowed.
                    let row = if ev.kind == UE_ORDER || ev.kind == UE_TRADE_LITE {
                        self.wire.find(PRODUCT_USDM, ev.symbol.get(frame))
                    } else {
                        ROW_NONE
                    };
                    let class = self.prefix.classify(ev.cid.get(frame));
                    self.on_user_event(&ev, row, class, now);
                }
                Err(e) => self.scan_failed(SCAN_USER, e, now),
            }
        }
    }

    fn scan_failed(&mut self, which: i32, _e: ScanErr, now: u64) {
        let mut e = BnEvt::new(EVT_SCAN_FAIL, now);
        e.code = which;
        self.emit(&e);
    }

    /// The order socket died: every request in flight is in doubt (BX-11).
    fn ws_failed(&mut self, now: u64) {
        if self.ws.is_open() || !self.ws.is_down() {
            self.ws.close();
        }
        if self.logged_on || self.ws_down_ns == 0 {
            self.ws_down_ns = now;
        }
        self.logged_on = false;
        self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
        self.ws_retry_ns = now + self.ws_backoff.next_delay_ns();
        while let Some(p) = self.ids.take_expired(now, 0) {
            self.on_request_lost(p.kind, now);
        }
    }

    fn us_failed(&mut self, now: u64) {
        if !self.us.is_down() {
            self.us.close();
        }
        if self.us_down_ns == 0 {
            self.us_down_ns = now;
        }
        self.t[crate::arm::GW_CONNECT_FAILURES] += 1;
        self.us_retry_ns = now + self.us_backoff.next_delay_ns();
    }

    /// The request was sent for the order still in slot `r.ix`: the same
    /// placement id AND the same generation (S7: a member that reuses an id
    /// could otherwise meet its previous order's late answers).
    #[inline]
    fn for_open(&self, r: &WsReq) -> bool {
        r.ix != IX_NONE && self.oot.is_open(r.ix) && self.oot.get(r.ix).cid_oid == r.oid && self.oot.aux(r.ix).gen == r.gen
    }

    /// A request whose answer will never come.
    fn on_request_lost(&mut self, r: WsReq, now: u64) {
        if r.verb == REQ_ACCOUNT {
            // The recon waits on it: without this it would wait forever.
            if self.recon == ReconStep::Account {
                self.recon_failed();
            }
            return;
        }
        if !self.for_open(&r) {
            return;
        }
        match r.verb {
            REQ_PLACE | REQ_MODIFY => self.doubt(r.ix),
            REQ_CANCEL => {
                // It may or may not have landed: owed again (a second
                // cancel of a gone order is `-2011`, which resolves it).
                let m = self.oot.get_mut(r.ix);
                let owe = if m.flags & OF_TTL != 0 { OWE_TTL } else { OWE_MEMBER };
                m.flags &= !(OF_CANCEL_SENT | OF_TTL);
                self.owe_cancel(r.ix, owe, now + CANCEL_RETRY_NS);
            }
            // A lost status query: the next in-doubt round asks again.
            _ => {}
        }
    }

    /// Put the open order in slot `ix` in doubt (BX-11).
    #[inline]
    fn doubt(&mut self, ix: u16) {
        if self.oot.get(ix).state != ST_IN_DOUBT {
            self.oot.set_in_doubt(ix);
            self.t[crate::arm::GW_IN_DOUBT_RAISED] += 1;
        }
    }

    /// Every open order in doubt (the user stream reopened; the order
    /// session logged on again): `order.status` resolves each.
    fn doubt_all(&mut self) {
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) {
                self.doubt(ix);
            }
            ix += 1;
        }
    }

    // ---------------------------------------------------------------------
    // WS API answers
    // ---------------------------------------------------------------------

    fn on_answer(&mut self, a: &WsAnswer, span: core_net::PayloadSpan, now: u64) {
        if a.shutdown {
            // serverShutdown: move to a new connection now.
            return self.ws_failed(now);
        }
        let Some(p) = self.ids.answer(a.id) else {
            return;
        };
        let r = p.kind;
        if a.status >= 400 || a.code != 0 {
            self.venue_code(a.code, a.status, a.retry_after_ms, now);
        }
        match r.verb {
            REQ_LOGON => {
                if a.status == 200 {
                    let again = self.phase == Phase::Ready;
                    self.logged_on = true;
                    self.ws_down_ns = 0;
                    if again {
                        // A new session: what the last one left unanswered
                        // is resolved by status (BX-11).
                        self.doubt_all();
                    }
                } else {
                    self.ws_failed(now);
                }
            }
            REQ_PLACE if self.for_open(&r) => self.place_answered(r.ix, a, span, now),
            REQ_CANCEL if self.for_open(&r) && a.status != 200 => self.cancel_refused(r.ix, a.code, now),
            REQ_MODIFY if self.for_open(&r) => self.modify_answered(r.ix, a, now),
            REQ_STATUS if self.for_open(&r) => self.status_answered(r.ix, a, now),
            REQ_ACCOUNT => self.account_answered(a, span, now),
            _ => {}
        }
    }

    /// Nothing is sent before `now + quiet_ns` — or before the end the
    /// venue names (`until_venue_ms`, venue ms; 0 = none), whichever is
    /// later, up to [`QUIET_MAX_NS`] (S6: a ban of minutes or days hit every
    /// `quiet_ns` would only lengthen it, on the IP the worker shares).
    fn go_quiet(&mut self, now: u64, until_venue_ms: u64) {
        let venue_now_ms = self.clock.venue_ms(now);
        let named_ns = until_venue_ms.saturating_sub(venue_now_ms).saturating_mul(MS).min(QUIET_MAX_NS);
        let until = now + self.knobs.quiet_ns.max(named_ns);
        if until > self.quiet_until_ns {
            self.t[crate::arm::GW_QUIET] += (now >= self.quiet_until_ns) as u64;
            self.quiet_until_ns = until;
        }
    }

    /// A venue refusal's side effects beyond the order (§3.9 venue codes).
    /// `until_ms`: the end of the pause the venue names (a WS error's
    /// `retryAfter`, a 418's "banned until"; 0 = none).
    fn venue_code(&mut self, code: i32, http: u16, until_ms: u64, now: u64) {
        match classify(code, http) {
            CodeClass::Budget => {
                self.go_quiet(now, until_ms);
                let mut e = BnEvt::new(EVT_BUDGET, now);
                e.code = if code != 0 { code } else { http as i32 };
                e.a = self.quiet_until_ns as i64;
                self.emit(&e);
                self.note(J_VENUE, now, 0, 0, 0, 0, 0, e.code, 0, 0);
                if self.phase != Phase::Ready {
                    self.refuse_boot(BootErr::Budget(e.code));
                }
            }
            CodeClass::Lock => {
                self.go_quiet(now, until_ms);
                let mut e = BnEvt::new(EVT_LOCK, now);
                e.code = if code != 0 { code } else { http as i32 };
                self.emit(&e);
                self.note(J_VENUE, now, 0, 0, 0, 0, 0, e.code, 0, 0);
                if self.phase != Phase::Ready {
                    self.refuse_boot(BootErr::VenueLock(e.code));
                }
            }
            CodeClass::Clock => {
                self.clock.invalidate();
                self.start_clock_round(now);
            }
            CodeClass::Reject | CodeClass::Unknown | CodeClass::WouldTake | CodeClass::RowFatal => {}
        }
    }

    fn place_answered(&mut self, ix: u16, a: &WsAnswer, span: core_net::PayloadSpan, now: u64) {
        let (slot, oid, cur, row) = {
            let e = self.oot.get(ix);
            (e.slot, e.cid_oid, e.current_oid, e.row)
        };
        if a.status == 200 {
            // An ACK names the order: the venue's id, and ours echoed. A
            // 200 that does not is not an answer we know (BX-15): in
            // doubt, resolved by `order.status`.
            let echo = self.prefix.classify(a.cid.get(self.ws.payload(span)));
            if a.venue_oid == 0 || echo != (CidClass::Ours { slot, client_oid: oid }) {
                return self.doubt(ix);
            }
            self.oot.get_mut(ix).venue_oid = a.venue_oid;
            self.oot.set_live(ix);
            self.emit(&BnEvt::order(EVT_ACK, now, cur, row, slot));
            self.note(J_ACK, now, cur, a.venue_oid, 0, 0, 0, 0, row, slot);
            // F1: the ACK clears a doubt a `-2011` raised (its cancel beat the
            // place to the venue): the parked cancel goes now.
            return self.live_again(ix, now);
        }
        if classify(a.code, a.status) == CodeClass::Unknown {
            // 5xx / -1007: the execution status is UNKNOWN (BX-11).
            return self.doubt(ix);
        }
        self.end_order(ix, EVT_REJECT, RETIRED_REJECTED, a.code, now);
    }

    /// The venue refused a cancel of ours.
    fn cancel_refused(&mut self, ix: u16, code: i32, now: u64) {
        let m = self.oot.get_mut(ix);
        let owe = if m.flags & OF_TTL != 0 { OWE_TTL } else { OWE_MEMBER };
        m.flags &= !(OF_CANCEL_SENT | OF_TTL);
        if code == -2011 {
            // The venue holds no such open order: it ended and its end is
            // late or lost — or our place is still in flight (the venue does
            // not promise to take a session's requests in order). In doubt,
            // resolved by `order.status` (BX-11); the cancel stays owed and
            // goes again once the venue shows the order working (S3).
            self.t[crate::arm::GW_CANCELS_OWED] += (self.oot.get(ix).owe & (OWE_MEMBER | OWE_TTL) == 0) as u64;
            self.oot.get_mut(ix).owe |= owe | OWE_UNKNOWN;
            return self.doubt(ix);
        }
        let (cur, row, slot) = {
            let e = self.oot.get(ix);
            (e.current_oid, e.row, e.slot)
        };
        let mut x = BnEvt::order(EVT_CANCEL_FAILED, now, cur, row, slot);
        x.code = code;
        self.emit(&x);
        // Still owed: the member was told `Ok`, and a TTL is a promise.
        self.owe_cancel(ix, owe, now + CANCEL_REFUSED_RETRY_NS);
    }

    /// The venue shows the order in slot `ix` working (a status, a stream
    /// update): a cancel still owed on it — refused as unknown (`-2011`)
    /// while its place was in flight — goes now (S3).
    #[inline]
    fn live_again(&mut self, ix: u16, now: u64) {
        let (owe, flags) = {
            let e = self.oot.get(ix);
            (e.owe, e.flags)
        };
        if owe & OWE_UNKNOWN == 0 || flags & OF_CANCEL_SENT != 0 {
            return;
        }
        let whose = if owe & OWE_MEMBER != 0 { OWE_MEMBER } else { OWE_TTL };
        if flags & OF_ON_WHEEL != 0 {
            self.ttl.remove(ix);
        }
        let m = self.oot.get_mut(ix);
        m.owe = 0;
        m.flags &= !OF_ON_WHEEL;
        self.cancel_or_owe(ix, now, whose);
    }

    fn modify_answered(&mut self, ix: u16, a: &WsAnswer, now: u64) {
        let unknown = if a.status == 200 {
            a.order_status == 0
        } else {
            classify(a.code, a.status) == CodeClass::Unknown
        };
        if unknown {
            // The answer does not say what happened (5xx, -1007, or a 200
            // with no status): in doubt — the status query confirms or
            // aborts the rename (BX-11).
            return self.doubt(ix);
        }
        let gone = matches!(a.order_status, S_CANCELED | S_EXPIRED | S_EXPIRED_IN_MATCH);
        if a.status == 200 && !gone {
            return self.modify_confirmed(ix, now);
        }
        // Refused — or the venue turned the modify into a cancel, whose
        // terminal event retires the order under its unchanged id.
        self.modify_aborted(ix, a.code, now);
    }

    /// The venue confirmed the modify in flight on `ix` — by its answer, by
    /// the stream's `AMENDMENT`, or by a status showing the new terms,
    /// whichever comes first: the member's id becomes the new one (the
    /// router renames on `EVT_MODIFIED`).
    fn modify_confirmed(&mut self, ix: u16, now: u64) {
        let (pending, current, row, slot, side) = {
            let e = self.oot.get(ix);
            (e.pending_oid, e.current_oid, e.row, e.slot, e.side)
        };
        if pending == 0 {
            return;
        }
        let (qty, px) = {
            let a = self.oot.aux(ix);
            (a.pending_qty_1e6, a.pending_px_1e6)
        };
        self.oot.confirm_rename(ix);
        let mut x = BnEvt::order(EVT_MODIFIED, now, pending, row, slot);
        x.a = current as i64;
        x.b = qty;
        x.c = px;
        x.side = side;
        self.emit(&x);
    }

    /// The modify in flight on `ix` did not happen (`code`: the venue's,
    /// 0 when a status says so).
    fn modify_aborted(&mut self, ix: u16, code: i32, now: u64) {
        let (pending, current, row, slot) = {
            let e = self.oot.get(ix);
            (e.pending_oid, e.current_oid, e.row, e.slot)
        };
        if pending == 0 {
            return;
        }
        self.oot.abort_rename(ix);
        let mut x = BnEvt::order(EVT_MODIFY_FAILED, now, pending, row, slot);
        x.a = current as i64;
        x.code = code;
        self.emit(&x);
    }

    /// An in-doubt order's status (BX-11).
    fn status_answered(&mut self, ix: u16, a: &WsAnswer, now: u64) {
        let age = now.saturating_sub(self.oot.aux(ix).placed_ns);
        if a.status != 200 {
            // -2013: no such order. Final only once no copy can still arrive
            // (the recvWindow has passed with margin).
            if a.code == -2013 && age > self.knobs.recv_window_ms * MS + 5 * S {
                self.end_order(ix, EVT_REJECT, RETIRED_REJECTED, a.code, now);
            }
            return;
        }
        // A modify in flight: the new terms confirm it; the old ones are
        // final only once the venue can no longer execute it (its
        // recvWindow has passed with margin) — until then the order stays
        // in doubt and is asked again.
        if self.oot.get(ix).pending_oid != 0 {
            let (qty, px, sent_ns) = {
                let x = self.oot.aux(ix);
                (x.pending_qty_1e6, x.pending_px_1e6, x.modify_sent_ns)
            };
            if a.qty_1e6 == qty && a.px_1e6 == px {
                self.modify_confirmed(ix, now);
            } else if now.saturating_sub(sent_ns) > self.knobs.recv_window_ms * MS + 5 * S {
                self.modify_aborted(ix, 0, now);
            }
        }
        // Fills the stream never delivered are booked from the status at
        // the implied average (N4), flagged `J_FILL_UNSEEN`; the stream's
        // trades for the order then book only past its cumulative quantity
        // (OF_STATUS_BOOKED).
        let (filled, row, slot, side, cur, own_px) = {
            let e = self.oot.get(ix);
            (e.filled_1e6, e.row, e.slot, e.side, e.current_oid, e.px_1e6)
        };
        if a.executed_1e6 > filled {
            let missing = a.executed_1e6 - filled;
            // The implied average; with no average, the order's price as
            // the venue states it, else as we sent it (never unbooked).
            let px = if a.avg_px_1e6 > 0 {
                implied_px_1e6(a.executed_1e6, a.avg_px_1e6, self.oot.aux(ix).booked_quote_1e12, missing)
            } else if a.px_1e6 > 0 {
                a.px_1e6
            } else {
                own_px
            };
            let ts = self.fill_ts_ns(0, now);
            if self.book_fill(Some(ix), None, row, slot, side, px, missing, 0, J_FILL_UNSEEN, false, ts, now, cur) {
                self.oot.get_mut(ix).flags |= OF_STATUS_BOOKED;
            }
        }
        match a.order_status {
            S_NEW | S_PARTIALLY_FILLED => {
                if self.oot.get(ix).flags & OF_MODIFY_SENT == 0 {
                    self.oot.set_live(ix);
                }
                self.live_again(ix, now);
            }
            S_FILLED => self.end_order(ix, EVT_RETIRED, RETIRED_FILLED, 0, now),
            S_CANCELED => {
                let why = self.cancel_why(ix);
                self.end_order(ix, EVT_RETIRED, why, 0, now)
            }
            S_EXPIRED => self.end_order(ix, EVT_RETIRED, RETIRED_EXPIRED, 0, now),
            S_EXPIRED_IN_MATCH => self.end_order(ix, EVT_RETIRED, RETIRED_CANCELED_VENUE, 0, now),
            _ => {}
        }
    }

    fn cancel_why(&self, ix: u16) -> u8 {
        let f = self.oot.get(ix).flags;
        if f & OF_CANCEL_SENT == 0 {
            RETIRED_CANCELED_VENUE
        } else if f & OF_TTL != 0 {
            RETIRED_CANCELED_TTL
        } else {
            RETIRED_CANCELED_MEMBER
        }
    }

    /// End an order: its fills are already out; now the event that retires
    /// it, then it leaves every table.
    fn end_order(&mut self, ix: u16, kind: u8, why: u8, code: i32, now: u64) {
        let (cur, cid, venue_oid, row, slot, side, okind, flags, filled, qty) = {
            let e = self.oot.get(ix);
            (e.current_oid, e.cid_oid, e.venue_oid, e.row, e.slot, e.side, e.kind, e.flags, e.filled_1e6, e.qty_1e6)
        };
        let (placed_ns, quote) = {
            let a = self.oot.aux(ix);
            (a.placed_ns, a.booked_quote_1e12)
        };
        let mut x = BnEvt::order(kind, now, cur, row, slot);
        x.why = why;
        x.code = code;
        x.side = side;
        x.product = PRODUCT_USDM;
        let icr = why == RETIRED_CANCELED_MEMBER && now.saturating_sub(placed_ns) < QTR_ICR_NS;
        x.flags = ((flags & OF_FILLED_ANY != 0) as u8 * EVT_F_HAD_FILL)
            | ((okind != ORDER_KIND_MAKER) as u8 * EVT_F_IOC)
            | (icr as u8 * EVT_F_ICR);
        self.emit(&x);
        let jk = if kind == EVT_REJECT { J_REJECT } else { J_ENDED };
        self.note(jk, now, cur, venue_oid, filled, qty, 0, if kind == EVT_REJECT { code } else { why as i32 }, row, slot);
        if flags & OF_ON_WHEEL != 0 {
            self.ttl.remove(ix);
        }
        self.ended.record(slot, cid, now, filled, quote, flags & OF_STATUS_BOOKED != 0);
        let r = &mut self.rows[row as usize];
        debug_assert!(r.open > 0, "an order ended on a row with none open");
        r.open = r.open.saturating_sub(1);
        if okind == ORDER_KIND_MAKER {
            r.makers = r.makers.saturating_sub(1);
            self.makers = self.makers.saturating_sub(1);
        }
        self.oot.remove(ix);
    }

    // ---------------------------------------------------------------------
    // The user stream: fills and states
    // ---------------------------------------------------------------------

    fn on_user_event(&mut self, ev: &UserEvent, row: u16, class: CidClass, now: u64) {
        match ev.kind {
            UE_ORDER | UE_TRADE_LITE => self.on_order_event(ev, row, class, now),
            UE_MARGIN_CALL => {
                let mut e = BnEvt::new(EVT_MARGIN, now);
                e.product = PRODUCT_USDM;
                e.a = self.ratio_1e6;
                e.b = self.equity_1e6;
                e.c = self.anchor_1e6;
                e.flags = EVT_F_MARGIN_CALL;
                self.emit(&e);
                self.note(J_MARGIN, now, 0, 0, e.a, e.b, 1, 0, 0, 0);
                // Read the figures now, not at the next cycle.
                self.account_next_ns = now;
            }
            UE_ACCOUNT => {
                // A margin re-read — the account alone, no sooner than
                // ACCOUNT_REREAD_NS after the last read (every fill pushes
                // one, S5).
                if self.account_next_ns == 0 {
                    self.account_next_ns = now.max(self.account_read_ns + ACCOUNT_REREAD_NS);
                }
            }
            UE_KEY_EXPIRED => {
                self.user.expire();
                self.us.close();
                if self.us_down_ns == 0 {
                    self.us_down_ns = now;
                }
                let _ = self.queue_job(JobKind::ListenNew, 0);
            }
            _ => {}
        }
    }

    fn on_order_event(&mut self, ev: &UserEvent, row: u16, class: CidClass, now: u64) {
        let trade = ev.exec == X_TRADE || ev.exec == X_CALCULATED;
        if row == ROW_NONE {
            if trade {
                self.t[crate::arm::GW_FILLS_UNOWNED] += 1;
            }
            return;
        }
        let owner = self.knobs.owner_slot;
        // The order: in the table, or remembered ending (a late trade).
        let (ix, ended, slot, order_id, settle, ours) = match class {
            CidClass::Ours { slot, client_oid } => match self.oot.by_placement(slot, client_oid) {
                // S7: an event naming another venue order under this id is
                // the previous order of a reused id, not this one.
                Some(ix) if self.same_venue_order(ix, ev.venue_oid) => (Some(ix), None, slot, self.oot.get(ix).current_oid, false, true),
                _ => (None, self.ended.find(slot, client_oid), slot, client_oid, false, true),
            },
            // No member id: a venue order id or an earlier boot's id could
            // collide with a live one.
            CidClass::Orphan { .. } | CidClass::Liquidation | CidClass::Adl => (None, None, owner, 0, false, false),
            CidClass::Settlement => (None, None, owner, 0, true, false),
            CidClass::Foreign => {
                // BX-8: what the engine did not place is a reconciliation
                // fact (drift on a dedicated account), never booked as the
                // engine's.
                if trade {
                    self.t[crate::arm::GW_FILLS_FOREIGN] += 1;
                }
                return;
            }
        };
        self.book_event(ev, ix, ended, row, slot, order_id, settle, ours, now);
        if ev.kind != UE_ORDER {
            return;
        }
        let Some(ix) = ix else { return };
        if ev.exec == X_AMENDMENT {
            // The venue's own word on a modify: the new terms confirm it.
            let (qty, px) = {
                let a = self.oot.aux(ix);
                (a.pending_qty_1e6, a.pending_px_1e6)
            };
            if ev.qty_1e6 == qty && ev.px_1e6 == px {
                self.modify_confirmed(ix, now);
            }
        }
        match ev.status {
            S_NEW | S_PARTIALLY_FILLED => {
                // Working — which resolves a doubt, unless a modify is still
                // undetermined (the status query settles it).
                let (state, flags) = {
                    let e = self.oot.get(ix);
                    (e.state, e.flags)
                };
                if state == ST_SENT || (state == ST_IN_DOUBT && flags & OF_MODIFY_SENT == 0) {
                    self.oot.set_live(ix);
                }
                if self.oot.get(ix).venue_oid == 0 {
                    self.oot.get_mut(ix).venue_oid = ev.venue_oid;
                }
                self.live_again(ix, now);
            }
            S_FILLED => self.end_order(ix, EVT_RETIRED, RETIRED_FILLED, 0, now),
            S_CANCELED => {
                let why = self.cancel_why(ix);
                self.end_order(ix, EVT_RETIRED, why, 0, now);
            }
            S_EXPIRED => self.end_order(ix, EVT_RETIRED, RETIRED_EXPIRED, 0, now),
            S_EXPIRED_IN_MATCH => self.end_order(ix, EVT_RETIRED, RETIRED_CANCELED_VENUE, 0, now),
            _ => {}
        }
    }

    /// The event names the venue order in slot `ix` — or names none, or the
    /// slot's order has no venue id yet (S7).
    #[inline]
    fn same_venue_order(&self, ix: u16, venue_oid: u64) -> bool {
        let v = self.oot.get(ix).venue_oid;
        venue_oid == 0 || v == 0 || v == venue_oid
    }

    /// **Book a stream event's fills** (BX-2, BX-11, S1). A trade books its
    /// own quantity at its own price once — trade ids dedupe its
    /// `TRADE_LITE` and `ORDER_TRADE_UPDATE` reports. An update of an order
    /// of ours the gateway knows (open, or remembered ending) — ANY update,
    /// not only a trade — then books whatever its cumulative `z` says is
    /// still unbooked (trades the stream lost while down, whose updates
    /// never came) at the implied average, flagged `J_FILL_UNSEEN`. From
    /// then on the order's `TRADE_LITE`s, which carry no `z`, wait for their
    /// updates (`OF_STATUS_BOOKED`).
    #[allow(clippy::too_many_arguments)]
    fn book_event(&mut self, ev: &UserEvent, ix: Option<u16>, ended: Option<usize>, row: u16, slot: u8, order_id: u64, settle: bool, ours: bool, now: u64) {
        let update = ev.kind == UE_ORDER;
        let known = ix.is_some() || ended.is_some();
        let (unseen, mut booked) = match (ix, ended) {
            (Some(i), _) => {
                let e = self.oot.get(i);
                (e.flags & OF_STATUS_BOOKED != 0, e.filled_1e6)
            }
            (None, Some(k)) => (self.ended.status_booked(k), self.ended.filled_1e6(k)),
            (None, None) => (false, 0),
        };
        let trade = (ev.exec == X_TRADE || ev.exec == X_CALCULATED) && ev.last_qty_1e6 > 0;
        if trade && (update || !unseen) {
            let seen = self.tids.seen_or_record(row, ev.trade_id);
            // An update books its trade only up to its `z` (a later trade's
            // lite report may be booked already: the rest waits for `z`).
            let q = if seen {
                0
            } else if update && known {
                ev.last_qty_1e6.min(ev.cum_qty_1e6.saturating_sub(booked)).max(0)
            } else {
                ev.last_qty_1e6
            };
            if q > 0 {
                self.t[crate::arm::GW_FILLS_UNRESOLVED] += (ours && !known) as u64;
                let ts = self.fill_ts_ns(ev.trade_ms, now);
                let code = (ev.maker != 0) as i32;
                if self.book_fill(ix, ended, row, slot, ev.side, ev.last_px_1e6, q, ev.commission_1e6, code, settle, ts, now, order_id) {
                    booked = booked.saturating_add(q);
                }
            }
        }
        if !(update && known) {
            return;
        }
        let excess = ev.cum_qty_1e6.saturating_sub(booked);
        if excess <= 0 {
            return;
        }
        // S1: trades the stream lost while it was down — the update's `z`
        // books them now (their times unknown: the clock's).
        let quote = match (ix, ended) {
            (Some(i), _) => self.oot.aux(i).booked_quote_1e12,
            (None, Some(k)) => self.ended.quote_1e12(k),
            (None, None) => 0,
        };
        let px = if ev.avg_px_1e6 > 0 {
            implied_px_1e6(ev.cum_qty_1e6, ev.avg_px_1e6, quote, excess)
        } else if ev.last_px_1e6 > 0 {
            ev.last_px_1e6
        } else {
            ev.px_1e6
        };
        let ts = self.fill_ts_ns(0, now);
        if self.book_fill(ix, ended, row, slot, ev.side, px, excess, 0, J_FILL_UNSEEN, settle, ts, now, order_id) {
            match (ix, ended) {
                (Some(i), _) => self.oot.get_mut(i).flags |= OF_STATUS_BOOKED,
                (None, Some(k)) => self.ended.set_status_booked(k),
                (None, None) => {}
            }
        }
    }

    /// A fill's timestamp ([`fill_wall_ns`]) at `now`.
    #[inline]
    fn fill_ts_ns(&self, venue_ms: u64, now: u64) -> u64 {
        fill_wall_ns(venue_ms, self.clock.venue_ms(now))
    }

    /// **Book one fill** stamped `ts_ns` (wall): lane 4 first, the
    /// position, the order's booked quantity and quote (in the table, or
    /// remembered ending), the journal (`code`: [`J_FILL`]'s bits), and the
    /// position event when the row's sign changes. `false`: the fill was
    /// LOST (lane 4 and the held queue are both full) and nothing was
    /// booked.
    #[allow(clippy::too_many_arguments)]
    fn book_fill(
        &mut self,
        ix: Option<u16>,
        ended: Option<usize>,
        row: u16,
        slot: u8,
        side: u8,
        px: i64,
        qty: i64,
        commission: i64,
        code: i32,
        settle: bool,
        ts_ns: u64,
        now: u64,
        order_id: u64,
    ) -> bool {
        let sym = self.wire.row(row).sym;
        let s = if side == Side::Ask as u8 { Side::Ask } else { Side::Bid };
        let mut f = Fill::new(ts_ns, sym, s, Price(px), Qty(qty), order_id).with_attribution(slot, FILL_ORIGIN_VENUE);
        if settle {
            f = f.with_flags(FILL_FLAG_SETTLEMENT);
        }
        let signed = if s == Side::Bid { qty } else { -qty };
        if !self.out_fill(&f) {
            // Nothing downstream drains lane 4. Nothing is booked — the
            // engine never saw it, so the venue's position must read as
            // drift — and the pulse raises unbounded drift (EVT_F_LOST).
            self.note(J_FILL, now, order_id, 0, px, signed, commission, code | J_FILL_LOST, row, slot);
            return false;
        }
        self.t[crate::arm::GW_FILLS_BOOKED] += 1;
        let quote = qty as i128 * px as i128;
        if let Some(ix) = ix {
            let m = self.oot.get_mut(ix);
            m.filled_1e6 = m.filled_1e6.saturating_add(qty);
            m.flags |= OF_FILLED_ANY;
            let a = self.oot.aux_mut(ix);
            a.booked_quote_1e12 = a.booked_quote_1e12.saturating_add(quote);
        } else if let Some(k) = ended {
            self.ended.add_filled(k, qty, quote);
        }
        let r = &mut self.rows[row as usize];
        let before = r.booked_1e6;
        r.booked_1e6 = if settle {
            0
        } else if s == Side::Bid {
            before.saturating_add(qty)
        } else {
            before.saturating_sub(qty)
        };
        if px > 0 {
            r.last_px_1e6 = px;
        }
        let after = r.booked_1e6;
        self.note(J_FILL, now, order_id, 0, px, signed, commission, code, row, slot);
        if before.signum() != after.signum() {
            let mut e = BnEvt::new(EVT_POSITION, now);
            e.row = row;
            e.a = after;
            self.emit(&e);
        }
        true
    }

    // ---------------------------------------------------------------------
    // REST
    // ---------------------------------------------------------------------

    /// Queue a REST job. `false`: the queue is full — counted, and never
    /// silent to the caller (each row queues at most one job of each kind,
    /// so it cannot happen: [`JOB_CAP`]).
    fn queue_job(&mut self, kind: JobKind, row: u16) -> bool {
        if self.jobs.push(Job { kind, row, sent_ns: 0 }) {
            return true;
        }
        self.t[crate::arm::GW_JOBS_DROPPED] += 1;
        false
    }

    /// A job left the queue for good (answered, failed or dropped): its
    /// row may queue another of its kind.
    fn job_done(&mut self, j: &Job) {
        self.probe_queued &= j.kind != JobKind::Probe;
        if let Some(r) = self.rows.get_mut(j.row as usize) {
            match j.kind {
                JobKind::Countdown => r.cd_queued = false,
                JobKind::UserTrades => r.day_queued = false,
                JobKind::CancelAll => r.cancel_all_queued = false,
                _ => {}
            }
        }
    }

    fn is_sapi(k: JobKind) -> bool {
        matches!(k, JobKind::Restrictions | JobKind::Portfolio)
    }

    /// Start the next REST job if its connection is free (between poll
    /// batches, BX5) — nothing while quiet.
    fn start_jobs(&mut self, now: u64) {
        if now < self.quiet_until_ns {
            return;
        }
        if self.sapi_job.is_none() {
            if let Some(j) = self.jobs.front() {
                if Self::is_sapi(j.kind) {
                    self.jobs.pop();
                    self.start_rest(j, now, true);
                }
            }
        }
        if self.job.is_none() {
            if let Some(j) = self.jobs.front() {
                if !Self::is_sapi(j.kind) {
                    self.jobs.pop();
                    self.start_rest(j, now, false);
                }
            }
        }
    }

    fn start_rest(&mut self, mut j: Job, now: u64, sapi: bool) {
        if j.kind == JobKind::Countdown && !self.ready_to_send(now) {
            // N2: a renewal queued while the session could cancel is not
            // sent once it cannot (B1).
            return self.job_done(&j);
        }
        let ts = self.clock.venue_ms(now);
        let rw = self.knobs.recv_window_ms;
        let (t, signed) = match j.kind {
            JobKind::Time => (T_TIME, false),
            JobKind::Dual => (T_DUAL, true),
            JobKind::MultiAssets => (T_MULTI_ASSETS, true),
            JobKind::Restrictions => (S_API_RESTRICTIONS, true),
            JobKind::Portfolio => (S_PORTFOLIO, true),
            JobKind::ListenNew => (T_LISTEN_KEY_NEW, false),
            JobKind::ListenKeep => (T_LISTEN_KEY_KEEP, false),
            JobKind::Countdown => (T_COUNTDOWN, true),
            JobKind::OpenOrders => (T_OPEN_ORDERS, true),
            JobKind::UserTrades => (T_USER_TRADES, true),
            JobKind::CancelAll => (T_CANCEL_ALL, true),
            JobKind::Probe => (T_COUNTDOWN, true),
        };
        let per_row = matches!(j.kind, JobKind::Countdown | JobKind::UserTrades | JobKind::CancelAll | JobKind::Probe);
        if per_row && (j.row as usize) >= self.wire.len() {
            debug_assert!(false, "a per-row job for no row");
            return self.rest_failed(j, now);
        }
        let day_start = ts / 86_400_000 * 86_400_000;
        let cd = self.knobs.countdown_ms;
        let conn = if sapi {
            match self.sapi.as_mut() {
                Some(c) => c,
                // The boot-only connection is gone: a boot job after the
                // boot has nowhere to go.
                None => return self.job_done(&j),
            }
        } else {
            &mut self.rest
        };
        let win = conn.window_mut(t);
        if win.is_empty() && signed {
            // A dial is pending: the window comes back next pass (the job
            // was just popped, so there is room for it).
            let _ = self.jobs.push(j);
            return;
        }
        let mut w = QueryWriter::new(win);
        if per_row {
            let s = self.wire.row(j.row).symbol();
            w.put(b"symbol=").put(s);
            match j.kind {
                JobKind::Countdown => {
                    w.put(b"&countdownTime=").uint(cd);
                }
                JobKind::Probe => {
                    w.put(b"&countdownTime=0");
                }
                JobKind::UserTrades => {
                    w.put(b"&startTime=").uint(day_start).put(b"&limit=").uint(TRADES_PAGE as u64);
                }
                _ => {}
            }
            w.put(b"&");
        }
        let len = if signed {
            w.put(b"recvWindow=").uint(rw).put(b"&timestamp=").uint(ts);
            w.sign(&self.signer)
        } else {
            w.rendered()
        };
        let Ok(len) = len else {
            return self.rest_failed(j, now);
        };
        j.sent_ns = now;
        match conn.start(t, len, self.poll.registry(), now) {
            Ok(()) => {
                if sapi {
                    self.sapi_job = Some(j);
                } else {
                    self.job = Some(j);
                }
            }
            Err(_) => {
                // Busy or the dial failed: offer it again next pass (just
                // popped: there is room).
                let _ = self.jobs.push(j);
            }
        }
    }

    /// The answer body of the last REST request on its connection.
    #[inline]
    fn body(&self, sapi: bool, b0: usize, b1: usize) -> &[u8] {
        let conn = match (sapi, self.sapi.as_ref()) {
            (true, Some(c)) => c,
            _ => &self.rest,
        };
        conn.resp().get(b0..b1).unwrap_or(&[])
    }

    fn on_rest_progress(&mut self, p: Progress, now: u64, sapi: bool) {
        let (status, span) = match p {
            Progress::Waiting => return,
            Progress::Done { status, body } => (status, body),
            Progress::Failed(_) => {
                let j = if sapi { self.sapi_job.take() } else { self.job.take() };
                if let Some(j) = j {
                    self.rest_failed(j, now);
                }
                return;
            }
        };
        let j = if sapi { self.sapi_job.take() } else { self.job.take() };
        let Some(j) = j else { return };
        if status != 200 {
            let (code, until) = {
                let b = self.body(sapi, span.start, span.end);
                // A 418 names its ban's end in its message (S6).
                (scan_error(b), if status == 418 || status == 429 { scan_ban_until(b) } else { 0 })
            };
            self.venue_code(code, status, until, now);
        }
        self.rest_answered(j, status, span.start, span.end, now, sapi);
    }

    /// A REST request that got no answer: re-offered where the boot or a
    /// verdict waits on it; the timers re-issue the rest.
    fn rest_failed(&mut self, j: Job, now: u64) {
        self.job_done(&j);
        match j.kind {
            JobKind::OpenOrders => self.recon_failed(),
            JobKind::Time => {
                self.clock_pending = self.clock_pending.saturating_sub(1);
                if self.clock_pending == 0 && !self.clock.measured() {
                    self.start_clock_round(now);
                } else if self.clock_pending > 0 {
                    let _ = self.queue_job(JobKind::Time, 0);
                }
            }
            JobKind::UserTrades => {
                self.day_left = self.day_left.saturating_sub(1);
                if self.day_left == 0 {
                    self.day_done(now);
                }
            }
            JobKind::Countdown => {
                if let Some(r) = self.rows.get_mut(j.row as usize) {
                    r.cd_bad = true;
                }
            }
            // The sweep's next round asks again while a maker rests; the
            // next probe goes at its time.
            JobKind::CancelAll | JobKind::Probe => {}
            JobKind::Dual
            | JobKind::MultiAssets
            | JobKind::Restrictions
            | JobKind::Portfolio
            | JobKind::ListenNew
            | JobKind::ListenKeep => {
                let _ = self.queue_job(j.kind, j.row);
            }
        }
    }

    fn rest_answered(&mut self, j: Job, status: u16, b0: usize, b1: usize, now: u64, sapi: bool) {
        self.job_done(&j);
        match j.kind {
            JobKind::Time => {
                let t = scan_server_time(self.body(sapi, b0, b1));
                if let (200, Ok(ms)) = (status, t) {
                    self.clock.sample(j.sent_ns, now, ms);
                }
                self.clock_pending = self.clock_pending.saturating_sub(1);
                if self.clock_pending > 0 {
                    let _ = self.queue_job(JobKind::Time, 0);
                } else if self.phase == Phase::Clock {
                    if self.clock.measured() {
                        self.phase = Phase::Assert;
                        self.asserts_left = 4;
                        let _ = self.queue_job(JobKind::Dual, 0);
                        let _ = self.queue_job(JobKind::MultiAssets, 0);
                        let _ = self.queue_job(JobKind::Restrictions, 0);
                        let _ = self.queue_job(JobKind::Portfolio, 0);
                    } else {
                        self.start_clock_round(now);
                    }
                }
            }
            JobKind::Dual | JobKind::MultiAssets | JobKind::Restrictions | JobKind::Portfolio => {
                let b = self.body(sapi, b0, b1);
                let r = match j.kind {
                    JobKind::Dual if status == 200 => judge_one_way(b),
                    JobKind::MultiAssets if status == 200 => judge_single_asset(b),
                    JobKind::Restrictions if status == 200 => judge_key_futures(b),
                    JobKind::Portfolio => judge_mode(status, b, self.knobs.mode),
                    _ => Err(AssertErr::Unreadable),
                };
                if let Err(e) = r {
                    self.refuse_boot(BootErr::Assert(e));
                    return;
                }
                self.asserts_left = self.asserts_left.saturating_sub(1);
                if self.asserts_left == 0 && self.phase == Phase::Assert {
                    self.phase = Phase::ListenKey;
                    let _ = self.queue_job(JobKind::ListenNew, 0);
                }
            }
            JobKind::ListenNew => self.listen_key_answered(status, b0, b1, now),
            JobKind::ListenKeep => {
                if status == 200 {
                    self.user.kept_ns = now;
                } else {
                    // The key is gone: a new one, a new socket.
                    self.user.expire();
                    let _ = self.queue_job(JobKind::ListenNew, 0);
                }
            }
            JobKind::Countdown => {
                let ok = status == 200 && scan_countdown(self.body(sapi, b0, b1)).is_ok();
                if let Some(r) = self.rows.get_mut(j.row as usize) {
                    if ok {
                        r.cd_ok_ns = now;
                    }
                    r.cd_bad = !ok;
                }
                self.cd_proven |= ok;
            }
            JobKind::Probe => {
                if status == 200 && scan_countdown(self.body(sapi, b0, b1)).is_ok() {
                    self.cd_proven = true;
                } else {
                    // N1: a row refused for good (`-4411`) must not block
                    // every maker — the next probe asks the next row.
                    self.cd_probe_row = self.next_probe_row(self.cd_probe_row);
                }
            }
            JobKind::OpenOrders => self.orders_answered(status, b0, b1, j.sent_ns, now),
            JobKind::UserTrades => self.trades_answered(j.row, status, b0, b1, now),
            // The orders' ends arrive on the user stream, or the next
            // listing puts them in doubt.
            JobKind::CancelAll => {}
        }
    }

    /// A listenKey was created: the stream (re)connects on it. A key that
    /// does not fit or is not `[A-Za-z0-9]` is refused whole and asked for
    /// again, never truncated.
    fn listen_key_answered(&mut self, status: u16, b0: usize, b1: usize, now: u64) {
        let adopted = {
            let b = self.rest.resp().get(b0..b1).unwrap_or(&[]);
            match (status, scan_listen_key(b)) {
                // Adopted straight from the answer (the stream keeps its
                // own copy: the next request overwrites the answer).
                (200, Ok(ks)) => self.user.set_key(ks.get(b), now),
                _ => None,
            }
        };
        let Some(changed) = adopted else {
            let _ = self.queue_job(JobKind::ListenNew, 0);
            return;
        };
        if changed || self.us.is_down() {
            let key = self.user.key();
            let n = 4 + key.len();
            let mut path = [0u8; 4 + LISTEN_KEY_MAX];
            // COPY: `/ws/` and the key (≤ 100 B) into the path the socket
            // upgrades to — cold, once per key; the socket keeps its own —
            // rejected: a parts-taking `set_path` (an API for one call).
            path[..4].copy_from_slice(b"/ws/");
            path[4..n].copy_from_slice(key);
            if let Ok(p) = core::str::from_utf8(&path[..n]) {
                if !self.us.is_down() {
                    self.us.close();
                }
                let _ = self.us.set_path(p);
            }
            self.us_retry_ns = 0;
        }
        if self.phase == Phase::ListenKey {
            self.phase = Phase::Connect;
        }
    }

    // ---------------------------------------------------------------------
    // Reconciliation, margin, the day
    // ---------------------------------------------------------------------

    /// Start a reconciliation — the account read, then the open orders and
    /// the verdict — or, `account_only`, a margin re-read (S5). `false`:
    /// not started (one in flight, the session cannot send, no request id).
    fn start_recon(&mut self, now: u64, account_only: bool) -> bool {
        if self.recon != ReconStep::Idle || !self.ready_to_send(now) {
            return false;
        }
        let Some(id) = self.ids.issue(WsReq::on(REQ_ACCOUNT, IX_NONE, 0, 0), now) else {
            return false;
        };
        if queue_account(&mut self.ws, id, &self.st, self.clock.venue_ms(now)).is_err() {
            let _ = self.ids.answer(id);
            return false;
        }
        self.recon = ReconStep::Account;
        self.recon_account_only = account_only;
        self.account_read_ns = now;
        // A full cycle's account read answers a re-read asked for (N-e).
        if !account_only {
            self.account_next_ns = 0;
        }
        true
    }

    /// The cycle in flight failed: the next one starts on schedule.
    fn recon_failed(&mut self) {
        self.t[crate::arm::GW_RECON_FAILED] += 1;
        self.recon = ReconStep::Idle;
        self.recon_listing_only = false;
        self.recon_account_only = false;
    }

    fn account_answered(&mut self, a: &WsAnswer, span: core_net::PayloadSpan, now: u64) {
        if a.status != 200 {
            return self.recon_failed();
        }
        // The result lies inside the frame the WS session still holds.
        let snap = {
            let frame = self.ws.payload(span);
            scan_um_account(frame, a.result, &mut self.pos[..])
        };
        let Ok(snap) = snap else {
            self.scan_failed(SCAN_ACCOUNT, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let account_only = self.recon_account_only;
        // Positions onto the legs (a full reconciliation's; the unseen legs
        // are counted afresh each time).
        let mut i = 0;
        while i < self.legs.len() && !account_only {
            self.legs[i].venue_1e6 = 0;
            self.legs[i].booked_1e6 = self.rows[i].booked_1e6;
            self.legs[i].px_1e6 = self.rows[i].last_px_1e6;
            i += 1;
        }
        self.verdict.unseen_legs = 0;
        let mut k = 0;
        while k < snap.n_pos && !account_only {
            let p = &self.pos[k];
            let row = self.wire.find(PRODUCT_USDM, p.symbol.get(self.ws.payload(span)));
            if row != ROW_NONE {
                let l = &mut self.legs[row as usize];
                l.venue_1e6 = p.amt_1e6;
                if p.amt_1e6 != 0 {
                    l.px_1e6 = ((p.notional_1e6.saturating_abs() as i128 * 1_000_000) / p.amt_1e6.saturating_abs() as i128) as i64;
                }
            } else if p.amt_1e6 != 0 && !self.knobs.shared {
                // A dedicated account holding what the boot did not bind.
                self.verdict.unseen_legs += 1;
            }
            k += 1;
        }
        // Margin (BX-20) and the session equity (E7).
        let ratio = um_ratio_1e6(snap.maint_1e6, snap.margin_balance_1e6);
        self.ratio_1e6 = ratio;
        self.equity_1e6 = snap.margin_balance_1e6;
        let mut e = BnEvt::new(EVT_MARGIN, now);
        e.product = PRODUCT_USDM;
        e.a = ratio;
        e.b = self.equity_1e6;
        e.c = self.anchor_1e6;
        self.emit(&e);
        self.note(J_MARGIN, now, 0, 0, ratio, self.equity_1e6, 0, 0, 0, 0);
        if account_only {
            self.recon = ReconStep::Idle;
            self.recon_account_only = false;
            return;
        }
        self.recon = ReconStep::Orders;
        if !self.queue_job(JobKind::OpenOrders, 0) {
            self.recon_failed();
        }
    }

    /// The venue's open orders, asked for at `sent_ns`: ours (listed),
    /// ghosts and orphans (cancelled), foreign; then the working orders
    /// the listing should have named and did not (in doubt), the sweep's
    /// confirmation and — unless only the listing was read — the verdict.
    fn orders_answered(&mut self, status: u16, b0: usize, b1: usize, sent_ns: u64, now: u64) {
        let listing_only = self.recon_listing_only;
        if status != 200 {
            return self.recon_failed();
        }
        let n = {
            let body = self.rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_orders(body, &mut self.orders[..])
        };
        let Ok(n) = n else {
            self.scan_failed(SCAN_REST, ScanErr::Malformed, now);
            return self.recon_failed();
        };
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) {
                self.oot.get_mut(ix).flags &= !OF_LISTED;
            }
            ix += 1;
        }
        let able = self.ready_to_send(now);
        let mut v = Verdict {
            shared: self.knobs.shared,
            unseen_legs: self.verdict.unseen_legs,
            ..Verdict::default()
        };
        let mut ours_open = 0u32;
        let mut i = 0;
        while i < n {
            let o = &self.orders[i];
            i += 1;
            let body = self.rest.resp().get(b0..b1).unwrap_or(&[]);
            let cid = o.cid.get(body);
            let row = self.wire.find(PRODUCT_USDM, o.symbol.get(body));
            let stray = match self.prefix.classify(cid) {
                CidClass::Ours { slot, client_oid } => match self.oot.by_placement(slot, client_oid) {
                    Some(k) => {
                        self.oot.get_mut(k).flags |= OF_LISTED;
                        ours_open += 1;
                        false
                    }
                    // Open at the venue, ended here before the list was
                    // asked for: a ghost (BX-9).
                    None if !self.ended.ended_since(slot, client_oid, sent_ns) => {
                        ours_open += 1;
                        v.ghosts += 1;
                        true
                    }
                    // The list predates the end (EndedRing): stale.
                    None => false,
                },
                CidClass::Orphan { .. } => {
                    ours_open += 1;
                    v.orphans += 1;
                    true
                }
                _ => {
                    let owned = row != ROW_NONE && self.wire.row(row).flags & INST_OWNED != 0;
                    if owned || (row == ROW_NONE && !self.knobs.shared) {
                        v.foreign += 1;
                    }
                    false
                }
            };
            if !stray || row == ROW_NONE {
                continue;
            }
            if able {
                // Cancelled by the venue's own id, rendered straight from
                // the answer into the frame.
                if let Some(id) = self.ids.issue(WsReq::on(REQ_CANCEL, IX_NONE, 0, 0), now) {
                    let sym = self.wire.row(row).symbol();
                    if queue_cancel(&mut self.ws, id, &self.st, sym, Part::Lit(cid), self.clock.venue_ms(now)).is_err() {
                        let _ = self.ids.answer(id);
                    }
                }
            } else {
                let _ = self.rest_cancel_all(row, now);
            }
        }
        // Working orders the listing should have named and did not: their
        // end was lost (BX-11) — in doubt, resolved by `order.status`. And
        // the backstop of F1: a listed order whose cancel `-2011` parked is
        // working — the cancel goes now.
        let before = sent_ns.saturating_sub(LISTING_MARGIN_NS);
        let mut ix = 0u16;
        while (ix as usize) < OOT_CAP {
            if self.oot.is_open(ix) {
                let (state, flags, owe) = {
                    let e = self.oot.get(ix);
                    (e.state, e.flags, e.owe)
                };
                if state == ST_LIVE && flags & OF_LISTED == 0 && self.oot.aux(ix).placed_ns < before {
                    self.doubt(ix);
                } else if flags & OF_LISTED != 0 && owe & OWE_UNKNOWN != 0 {
                    self.live_again(ix, now);
                }
            }
            ix += 1;
        }
        self.recon = ReconStep::Idle;
        self.recon_listing_only = false;
        if self.sweep.active && now >= self.sweep.confirm_at_ns {
            self.sweep_confirm(ours_open, now);
        }
        if listing_only {
            return;
        }
        self.verdict = Verdict::default();
        judge_positions(&mut self.legs, &mut v);
        let ok = v.reconciled();
        self.last_verdict = v;
        self.t[crate::arm::GW_RECON_DRIFT_LEGS] = v.drift_legs as u64;
        self.t[crate::arm::GW_RECON_UNSEEN_LEGS] = v.unseen_legs as u64;
        if ok {
            self.t[crate::arm::GW_RECON_OK] += 1;
            self.set_anchor_once(now);
        }
        let mut e = BnEvt::new(EVT_RECON, now);
        e.a = v.drift_reported();
        e.b = v.unseen_legs as i64;
        e.c = v.foreign as i64;
        e.flags = ok as u8 * EVT_F_RECONCILED;
        self.emit(&e);
        self.note(J_RECON, now, 0, 0, e.a, e.b, e.c, ok as i32, 0, 0);
        if self.phase == Phase::Recon {
            if v.orphans == 0 && v.ghosts == 0 {
                self.phase = Phase::Day;
                self.start_day(now);
            } else {
                // Swept; the next cycle confirms.
                self.recon_next_ns = now + S;
            }
        }
        if self.phase == Phase::Ready && !self.day_sent && self.day_left == 0 {
            // An unread day retries once per cycle (S7-L1).
            self.start_day(now);
        }
    }

    /// E7: the session anchor is the first reconciled equity. The journal's
    /// writer thread persists it ([`J_ANCHOR`]) so a restart keeps "the
    /// session" — the gateway thread never writes a file.
    fn set_anchor_once(&mut self, now: u64) {
        if self.anchor_1e6 != 0 || self.equity_1e6 <= 0 {
            return;
        }
        self.anchor_1e6 = self.equity_1e6;
        // S8: a dropped anchor would let the next boot re-anchor at its own
        // equity and forget every loss before it — offered until it lands.
        self.anchor_pending = !self.note(J_ANCHOR, now, 0, 0, self.anchor_1e6, 0, 0, 0, 0, 0);
        let mut e = BnEvt::new(EVT_MARGIN, now);
        e.product = PRODUCT_USDM;
        e.a = self.ratio_1e6;
        e.b = self.equity_1e6;
        e.c = self.anchor_1e6;
        self.emit(&e);
    }

    fn start_day(&mut self, now: u64) {
        let mut left = 0u16;
        let mut i = 0;
        while i < self.rows.len() {
            let w = self.wire.row(i as u16);
            let want = w.flags & INST_OWNED != 0 && w.flags & INST_LIVE != 0 && !self.rows[i].day_read;
            if want && !self.rows[i].day_queued && self.queue_job(JobKind::UserTrades, i as u16) {
                self.rows[i].day_queued = true;
                left += 1;
            }
            i += 1;
        }
        self.day_left = left;
        if left == 0 {
            self.day_done(now);
        }
    }

    fn trades_answered(&mut self, row: u16, status: u16, b0: usize, b1: usize, now: u64) {
        let n = if status == 200 {
            let body = self.rest.resp().get(b0..b1).unwrap_or(&[]);
            scan_trades(body, &mut self.trades[..]).ok()
        } else {
            None
        };
        let r = row as usize;
        match n {
            // A full page may be cut short: refused, never summed (S7-L1).
            Some(n) if n < TRADES_PAGE && r < self.rows.len() => {
                // UM rows are linear; the inverse law (COIN-M) arrives with
                // BX7, which binds COIN-M rows and their contract size.
                debug_assert!(self.wire.row(row).flags & crate::inst::INST_INVERSE == 0);
                let cur = self.legs[r].venue_1e6;
                self.rows[r].day_spent_1e6 = day_increasing_1e6(&mut self.trades[..n], cur, false, 0);
                self.rows[r].day_read = true;
            }
            _ => {}
        }
        self.day_left = self.day_left.saturating_sub(1);
        if self.day_left == 0 {
            self.day_done(now);
        }
    }

    fn day_done(&mut self, now: u64) {
        let mut all = true;
        let mut sum = 0i64;
        let mut i = 0;
        while i < self.rows.len() {
            let w = self.wire.row(i as u16);
            if w.flags & INST_OWNED != 0 && w.flags & INST_LIVE != 0 {
                all &= self.rows[i].day_read;
                sum = sum.saturating_add(self.rows[i].day_spent_1e6);
            }
            i += 1;
        }
        if all && !self.day_sent {
            let mut e = BnEvt::new(EVT_DAY, now);
            e.slot = self.knobs.owner_slot;
            e.a = (self.clock.venue_ms(now) / 86_400_000) as i64;
            e.b = sum;
            self.emit(&e);
            self.day_sent = true;
        }
        if self.phase == Phase::Day {
            // Unread rows retry at recon cadence; the slot stays unseeded
            // until they are read (the arm reports `reconciled` only then).
            self.phase = Phase::Ready;
        }
    }

    // ---------------------------------------------------------------------
    // Timers
    // ---------------------------------------------------------------------

    fn start_clock_round(&mut self, now: u64) {
        if self.clock_pending == 0 {
            self.clock_pending = crate::clock::SAMPLES_PER_ROUND;
            let _ = self.queue_job(JobKind::Time, 0);
        }
        self.clock_next_ns = now + CLOCK_EVERY_NS;
    }

    fn timers(&mut self, now: u64) {
        if now < self.timers_next_ns {
            return;
        }
        self.timers_next_ns = now + MS;
        // WS API request timeouts (S2): a request unanswered this long means
        // the order session is dead whatever its socket says (a half-open
        // path: no FIN, no RST) — every request in flight is in doubt, the
        // socket is dropped and dialled again.
        if let Some(p) = self.ids.take_expired(now, WS_REQ_TIMEOUT_NS) {
            self.on_request_lost(p.kind, now);
            self.ws_failed(now);
        }
        // REST timeouts are the transport's.
        let p = self.rest.on_tick(now);
        self.on_rest_progress(p, now, false);
        if let Some(s) = self.sapi.as_mut() {
            let p = s.on_tick(now);
            self.on_rest_progress(p, now, true);
        }
        // Sockets: ticks and reconnects (none while quiet).
        let p = self.ws.on_tick(now);
        self.on_ws_progress(p, now);
        let p = self.us.on_tick(now);
        self.on_us_progress(p, now);
        let connecting = matches!(self.phase, Phase::Connect | Phase::Recon | Phase::Day | Phase::Ready) && now >= self.quiet_until_ns;
        if connecting && self.ws.is_down() && now >= self.ws_retry_ns && self.ws.connect(self.poll.registry(), now).is_err() {
            self.ws_retry_ns = now + self.ws_backoff.next_delay_ns();
        }
        if connecting
            && self.us.is_down()
            && self.user.phase == UsPhase::Keyed
            && now >= self.us_retry_ns
            && self.us.connect(self.poll.registry(), now).is_err()
        {
            self.us_retry_ns = now + self.us_backoff.next_delay_ns();
        }
        if self.phase == Phase::Connect && self.logged_on && self.us.is_open() {
            self.phase = Phase::Recon;
            self.us_down_ns = 0;
            self.recon_next_ns = now;
        }
        if self.us.is_open() {
            self.us_down_ns = 0;
        }
        // TTL cancels (BX-13) and owed cancels.
        while let Some(ix) = self.ttl.pop_expired(now) {
            if !self.oot.is_open(ix) {
                continue;
            }
            let m = self.oot.get_mut(ix);
            m.flags &= !OF_ON_WHEEL;
            // Whose: a member's owed cancel, else the TTL's.
            let owe = if m.owe & OWE_MEMBER != 0 { OWE_MEMBER } else { OWE_TTL };
            if m.flags & OF_CANCEL_SENT != 0 {
                m.owe = 0;
                continue;
            }
            if self.send_cancel(ix, now, owe == OWE_TTL) {
                self.oot.get_mut(ix).owe = 0;
            } else {
                // Not sendable now: still owed, and the rest wait for a pass.
                self.owe_cancel(ix, owe, now + CANCEL_RETRY_NS);
                break;
            }
        }
        if self.phase == Phase::Ready || self.phase == Phase::Exiting {
            self.ready_timers(now);
        }
        // The cadence moves only when a cycle started (N7: a "now" asked
        // for while one is in flight is kept, not lost).
        if matches!(self.phase, Phase::Recon | Phase::Day | Phase::Ready) && now >= self.recon_next_ns && self.start_recon(now, false) {
            self.recon_next_ns = now + self.knobs.recon_every_ms * MS;
        }
        self.start_jobs(now);
    }

    fn ready_timers(&mut self, now: u64) {
        if now >= self.status_next_ns {
            self.status_next_ns = now + STATUS_EVERY_NS;
            self.pulse(now);
        }
        if now >= self.clock_next_ns {
            self.start_clock_round(now);
        }
        if self.user.keepalive_due(now) {
            self.user.kept_ns = now;
            let _ = self.queue_job(JobKind::ListenKeep, 0);
        }
        // The dead-man (O-BX13): every row with a maker of ours resting,
        // every heartbeat, ONLY while the order session can cancel (B1).
        let hb = self.knobs.heartbeat_ms * MS;
        if self.ready_to_send(now) {
            let mut i = 0;
            while i < self.rows.len() {
                let r = &self.rows[i];
                if r.makers > 0 && !r.cd_queued && now >= r.cd_next_ns && self.queue_job(JobKind::Countdown, i as u16) {
                    let r = &mut self.rows[i];
                    r.cd_queued = true;
                    r.cd_next_ns = now + hb;
                }
                i += 1;
            }
            // N1: until one countdown has answered, the proving request —
            // `countdownTime=0`, which sets no timer — on a row with no
            // maker of ours; a refused row hands over to the next.
            let p = self.cd_probe_row;
            if !self.cd_proven && !self.probe_queued && now >= self.probe_next_ns && p != ROW_NONE && self.rows[p as usize].makers == 0 && self.queue_job(JobKind::Probe, p) {
                self.probe_queued = true;
                self.probe_next_ns = now + hb;
            }
        }
        // In-doubt orders: one status query each, once a second.
        if now >= self.doubt_next_ns && self.oot.in_doubt() > 0 && self.ready_to_send(now) {
            self.doubt_next_ns = now + IN_DOUBT_EVERY_NS;
            let mut ix = 0u16;
            while (ix as usize) < OOT_CAP {
                if self.oot.is_open(ix) && self.oot.get(ix).state == ST_IN_DOUBT {
                    let (slot, oid, row) = {
                        let e = self.oot.get(ix);
                        (e.slot, e.cid_oid, e.row)
                    };
                    let gen = self.oot.aux(ix).gen;
                    let Some(id) = self.ids.issue(WsReq::on(REQ_STATUS, ix, gen, oid), now) else {
                        break;
                    };
                    let sym = self.wire.row(row).symbol();
                    let cid = Part::Cid(&self.prefix, slot, oid);
                    if queue_status(&mut self.ws, id, &self.st, sym, cid, self.clock.venue_ms(now)).is_err() {
                        let _ = self.ids.answer(id);
                        break;
                    }
                }
                ix += 1;
            }
        }
        // S2: a half-open order session answers nothing and says nothing.
        // While a maker of ours rests, a session silent for a heartbeat is
        // asked for the margin: its timeout drops the session, which stops
        // the dead-man (B1).
        if self.account_next_ns == 0 && self.makers > 0 && now.saturating_sub(self.ws_rx_ns) >= hb {
            self.account_next_ns = now.max(self.account_read_ns + ACCOUNT_REREAD_NS);
        }
        // A margin re-read (an ACCOUNT_UPDATE, a MARGIN_CALL, the probe
        // above): the account alone (S5).
        // (A full cycle due now reads the account itself: none extra, N-e.)
        if self.account_next_ns != 0 && now >= self.account_next_ns && now < self.recon_next_ns && self.start_recon(now, true) {
            self.account_next_ns = 0;
        }
        // S8: the E7 anchor, offered again until the journal ring takes it
        // (a retry is not another drop: N-a).
        if self.anchor_pending {
            let r = ExecRecord {
                ts_ns: now,
                wall_ms: self.clock.venue_ms(now),
                a: self.anchor_1e6,
                kind: J_ANCHOR,
                ..ExecRecord::default()
            };
            self.anchor_pending = !self.journal.offer(&r);
        }
        // The sweep's confirmation read: the whole cycle while the order
        // session is up; the REST listing alone while it is not (B1).
        if self.sweep.active && now >= self.sweep.confirm_at_ns && self.recon == ReconStep::Idle {
            if self.ready_to_send(now) {
                self.recon_next_ns = now;
            } else if now >= self.quiet_until_ns && self.queue_job(JobKind::OpenOrders, 0) {
                self.recon = ReconStep::Orders;
                self.recon_listing_only = true;
            }
        }
        // The 23 h rotation, at a quiet moment.
        if self.ws.is_open()
            && now.saturating_sub(self.ws_since_ns) >= ROTATE_AFTER_NS
            && self.ids.in_flight() == 0
            && self.cmd.is_empty()
        {
            self.ws.close();
            self.ws_failed(now);
            self.ws_retry_ns = now;
        }
        // Tallies, one group of three every quarter second.
        if now >= self.tally_next_ns {
            self.tally_next_ns = now + S / 4;
            self.t[crate::arm::GW_JOURNAL_DROPPED] = self.journal.dropped();
            let g = self.tally_group as usize;
            let mut e = BnEvt::new(EVT_TALLY, now);
            e.why = self.tally_group;
            e.a = self.t[g * 3] as i64;
            e.b = self.t[g * 3 + 1] as i64;
            e.c = self.t[g * 3 + 2] as i64;
            self.emit(&e);
            self.tally_group = (self.tally_group + 1) % TALLY_GROUPS;
        }
    }

    /// The maker-capable live row after `from` (wrapping), or `from` when
    /// there is no other (N1).
    fn next_probe_row(&self, from: u16) -> u16 {
        let n = self.wire.len();
        let mut k = 1;
        while k < n {
            let i = ((from as usize).wrapping_add(k)) % n;
            let w = self.wire.row(i as u16);
            if w.flags & INST_LIVE != 0 && w.flags & INST_NO_DEADMAN == 0 {
                return i as u16;
            }
            k += 1;
        }
        from
    }

    /// Every row with a maker of ours resting is covered by a countdown:
    /// one answered since its first maker and within two heartbeats, or —
    /// until that first answer — a first maker younger than two heartbeats
    /// (its countdown went out the same pass).
    fn deadman_covers(&self, now: u64) -> bool {
        let limit = 2 * self.knobs.heartbeat_ms * MS;
        let mut ok = self.cd_proven;
        let mut i = 0;
        while i < self.rows.len() {
            let r = &self.rows[i];
            if r.makers > 0 {
                let since = if r.cd_ok_ns != 0 && r.cd_ok_ns >= r.cd_since_ns { r.cd_ok_ns } else { r.cd_since_ns };
                ok &= !r.cd_bad && now.saturating_sub(since) < limit;
            }
            i += 1;
        }
        ok
    }

    fn pulse(&mut self, now: u64) {
        let ws_gap = if self.ws_down_ns == 0 { 0 } else { now.saturating_sub(self.ws_down_ns) };
        let us_gap = if self.us_down_ns == 0 { 0 } else { now.saturating_sub(self.us_down_ns) };
        let mut e = BnEvt::new(EVT_STATUS, now);
        e.a = ws_gap.max(us_gap) as i64;
        e.b = self.clock.offset_ms();
        e.c = self.oot.in_doubt() as i64;
        // DEADMAN_OK: a maker placed now is covered — the session can
        // cancel (B1) and every resting maker's countdown is current.
        let deadman = self.ready_to_send(now) && self.deadman_covers(now);
        // F2: the E7 anchor is not persisted yet — the ring has not taken
        // it, or the writer could not store it (a restart would re-anchor).
        let unsaved = self.anchor_pending || self.knobs.anchor_unsaved.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));
        e.flags = ((self.logged_on && self.ws.is_open()) as u8 * EVT_F_ORDER_UP)
            | (self.us.is_open() as u8 * EVT_F_USER_UP)
            | (deadman as u8 * EVT_F_DEADMAN_OK)
            | (self.clock.measured() as u8 * EVT_F_CLOCK_OK)
            | (self.lost as u8 * EVT_F_LOST)
            | (unsaved as u8 * EVT_F_ANCHOR_UNSAVED);
        self.emit(&e);
    }
}

/// **A fill's timestamp is WALL time**, ns — the ledger's day reads it so
/// (E6: one day epoch): the venue's `T` (`venue_ms`) when it is plausible
/// against the gateway's venue clock (`local_ms`: at most a day behind, a
/// minute ahead), else the clock's own; never the monotonic clock (B3).
#[inline]
#[must_use]
pub const fn fill_wall_ns(venue_ms: u64, local_ms: u64) -> u64 {
    let plausible = venue_ms != 0
        && venue_ms <= local_ms.saturating_add(FILL_TS_AHEAD_MS)
        && venue_ms.saturating_add(FILL_TS_BEHIND_MS) >= local_ms;
    (if plausible { venue_ms } else { local_ms }).saturating_mul(MS)
}

/// **The price of a quantity booked without its trades** (S1, N4): what
/// the venue's average `avg_1e6` over the cumulative `cum_1e6` leaves after
/// the quote already booked (`booked_quote_1e12`, Σ quantity × price) —
/// `(cum × avg − booked) / excess` — when it lies within a tenth of the
/// average; else the average itself (a coarse average makes the
/// difference noise). Cold: only a lost trade reaches it.
#[must_use]
pub fn implied_px_1e6(cum_1e6: i64, avg_1e6: i64, booked_quote_1e12: i128, excess_1e6: i64) -> i64 {
    if excess_1e6 <= 0 || avg_1e6 <= 0 {
        return avg_1e6;
    }
    let rest = (cum_1e6 as i128 * avg_1e6 as i128).saturating_sub(booked_quote_1e12);
    let px = rest / excess_1e6 as i128;
    let band = (avg_1e6 / 10) as i128;
    if px > 0 && (px - avg_1e6 as i128).abs() <= band {
        px.min(i64::MAX as i128) as i64
    } else {
        avg_1e6
    }
}

/// [`EVT_SCAN_FAIL`] `code`: a WS API answer.
pub const SCAN_WSAPI: i32 = 1;
/// A user-data event.
pub const SCAN_USER: i32 = 2;
/// A REST answer.
pub const SCAN_REST: i32 = 3;
/// The account snapshot.
pub const SCAN_ACCOUNT: i32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    /// B3: a fill carries wall time — the venue's `T` inside the window,
    /// the clock's own outside it; a monotonic reading never gets through
    /// (it is decades "behind"). Break-and-watch: stamping `now` (the
    /// monotonic clock) fails the first assertion.
    #[test]
    fn a_fill_is_stamped_on_the_wall_clock() {
        let local = 1_790_000_000_000u64;
        assert_eq!(fill_wall_ns(local - 1_500, local), (local - 1_500) * MS, "a late event keeps its T");
        assert_eq!(fill_wall_ns(local + 59_000, local), (local + 59_000) * MS);
        assert_eq!(fill_wall_ns(0, local), local * MS, "no T: the clock");
        assert_eq!(fill_wall_ns(local + 61_000, local), local * MS, "too far ahead");
        assert_eq!(fill_wall_ns(local - 86_400_001, local), local * MS, "more than a day behind");
        assert_eq!(fill_wall_ns(12_345_678, local), local * MS, "a monotonic-looking T");
        assert_eq!(fill_wall_ns(u64::MAX, local), local * MS, "never overflows");
    }

    /// S1 / N4: a quantity booked without its trades is priced at what the
    /// average leaves after the booked quote. Break-and-watch: pricing at
    /// the average fails the first assertion.
    #[test]
    fn a_lost_trade_is_priced_at_the_implied_average() {
        // 0.001 @ 60 000 lost, 0.002 @ 60 300 booked: average 60 200.
        let booked = 2_000i128 * 60_300_000_000;
        assert_eq!(implied_px_1e6(3_000, 60_200_000_000, booked, 1_000), 60_000_000_000);
        assert_eq!(implied_px_1e6(3_000, 60_200_000_000, 0, 3_000), 60_200_000_000, "nothing booked: the average");
        assert_eq!(implied_px_1e6(3_000, 60_200_000_000, booked * 3, 1_000), 60_200_000_000, "implausible: the average");
        assert_eq!(implied_px_1e6(3_000, 0, booked, 1_000), 0, "no average: the caller's fallback");
        assert_eq!(implied_px_1e6(i64::MAX, i64::MAX, i128::MIN, 1), i64::MAX, "never overflows");
        assert_eq!(implied_px_1e6(2, i64::MAX, i64::MAX as i128, 1), i64::MAX, "never wraps");
    }
}
